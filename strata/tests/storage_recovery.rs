//! Opening after damage: a newest version that fails a check, a WAL damaged
//! before its last commits, and files a failed commit left behind in the
//! directory layout.

mod storage_util;

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use storage_util::*;
use strata::error::Error;
use strata::storage::manifest::Manifest;
use strata::storage::pager::{read_extent, read_metas, Extent, PAGE};
use strata::storage::{Database, DbOptions, Layout, MergePolicy};
use strata::Value;

fn keys(r: impl Iterator<Item = u64>) -> BTreeSet<String> {
    r.map(key).collect()
}

fn insert(db: &Database, src: &MapSource, ids: impl Iterator<Item = u64>) {
    let mut tx = db.begin().unwrap();
    for i in ids {
        let d = doc(i, 0);
        src.put(&d);
        tx.insert(d).unwrap();
    }
    tx.commit().unwrap();
}

fn delete(db: &Database, ids: impl Iterator<Item = u64>) {
    let mut tx = db.begin().unwrap();
    for i in ids {
        assert!(tx.delete(&Value::Str(key(i))).unwrap());
    }
    tx.commit().unwrap();
}

fn wal_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push("-wal");
    s.into()
}

fn damaged_copy_of(wal: &Path) -> PathBuf {
    let mut s = wal.as_os_str().to_owned();
    s.push(".damaged");
    s.into()
}

/// Copies the database and its WAL to `to`, and overwrites the start of
/// the newest version's manifest there; returns that version's txn, the
/// older slot's page count and the copy's length in pages, or `None` when
/// the file has one valid meta slot.
fn copy_with_damaged_manifest(path: &Path, to: &Path) -> Option<(u64, u64, u64)> {
    std::fs::copy(path, to).unwrap();
    if wal_of(path).exists() {
        std::fs::copy(wal_of(path), wal_of(to)).unwrap();
    }
    let f = OpenOptions::new().read(true).write(true).open(to).unwrap();
    let metas = read_metas(&f).unwrap();
    let [(newest, _), (older, _)] = metas[..] else { return None };
    f.write_all_at(&[0xAB; 64], newest.manifest.page * PAGE).unwrap();
    Some((newest.txn, older.page_count, f.metadata().unwrap().len() / PAGE))
}

// A newest version whose manifest failed its checksum made the database
// unopenable, although the older meta slot still named an intact version
// (with `sync` off, a power loss can leave exactly that). Opening now falls
// back to it, and says so.
#[test]
fn a_damaged_newest_version_opens_the_one_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let src = Arc::new(MapSource::default());
    {
        let db = Database::create(&path, schema(), opts(&src)).unwrap();
        let docs: Vec<_> = (0..300).map(|i| doc(i, 0)).collect();
        docs.iter().for_each(|d| src.put(d));
        db.bulk_load(docs).unwrap();
        delete(&db, 0..100);
        db.flush().unwrap();
        insert(&db, &src, 1000..1001);
        db.flush().unwrap();
    }
    let copy = dir.path().join("copy.strata");
    let (txn, _, _) = copy_with_damaged_manifest(&path, &copy).unwrap();
    {
        let db = Database::open(&copy, opts(&src)).unwrap();
        let why = db.damaged().expect("says what was skipped");
        assert!(why.contains(&format!("version {txn} skipped")), "{why}");
        // The flush before the last: the insert it flushed is lost with it.
        assert_keys(&db.snapshot(), &keys(100..300));
        db.check().unwrap();
        insert(&db, &src, 2000..2001);
        db.flush().unwrap();
        db.check().unwrap();
    }
    // The next commit replaced the damaged version.
    let db = Database::open(&copy, opts(&src)).unwrap();
    assert!(db.damaged().is_none());
    assert_keys(&db.snapshot(), &keys((100..300).chain(2000..2001)));
    db.check().unwrap();
}

// A newest version whose pages never reached the disk can name extents
// past the end of the file. Reading them failed with an I/O error, which
// opening did not count as damage, so it did not fall back.
#[test]
fn a_newest_version_past_the_end_of_the_file_opens_the_one_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let copy = dir.path().join("copy.strata");
    let src = Arc::new(MapSource::default());
    let db = Database::create(&path, schema(), opts(&src)).unwrap();
    let mut cut = 0;
    for k in 1..7u64 {
        insert(&db, &src, (k - 1) * 100..k * 100);
        db.flush().unwrap();
        std::fs::copy(&path, &copy).unwrap();
        std::fs::copy(wal_of(&path), wal_of(&copy)).unwrap();
        let f = OpenOptions::new().read(true).write(true).open(&copy).unwrap();
        let [(newest, _), (older, _)] = read_metas(&f).unwrap()[..] else { continue };
        let at = newest.manifest.page.min(newest.freelist.page);
        if at < older.page_count {
            continue;
        }
        f.set_len(at * PAGE).unwrap();
        drop(f);
        let o = Database::open(&copy, opts(&src)).unwrap();
        assert!(o.damaged().unwrap().contains("past the end of the file"), "{:?}", o.damaged());
        // The flush before: the WAL was reset after the newest one.
        assert_keys(&o.snapshot(), &keys(0..(k - 1) * 100));
        o.check().unwrap();
        cut += 1;
    }
    assert!(cut > 0);
}

// A commit cuts free pages off the end of the file, which can leave it
// shorter than the version before it says. After every commit of a
// workload of inserts, deletes, flushes, merges and a vacuum, a copy whose
// newest version is damaged must open at the version before it and pass
// `check`.
#[test]
fn every_older_version_stays_openable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let copy = dir.path().join("copy.strata");
    let src = Arc::new(MapSource::default());
    let db = Database::create(&path, schema(), opts(&src)).unwrap();
    let policy = MergePolicy { merge_factor: 10, floor_docs: 50, max_deleted_ratio: 0.5, ..MergePolicy::default() };
    let mut live = BTreeSet::new();
    let mut states = vec![live.clone()];
    let (mut checked, mut extended) = (0, 0);
    let ops = (0..4u64)
        .map(|k| ('i', k * 200..k * 200 + 200))
        .chain([('d', 600..800), ('m', 0..0), ('d', 0..1), ('d', 1..2), ('d', 2..3), ('d', 200..400), ('m', 0..0), ('v', 0..0)])
        .chain((3..6).map(|i| ('d', i..i + 1)))
        .chain([('i', 2000..2300), ('d', 2000..2300), ('m', 0..0), ('d', 6..7), ('d', 7..8)]);
    for (step, (op, ids)) in ops.enumerate() {
        match op {
            'i' => {
                insert(&db, &src, ids.clone());
                live.extend(ids);
                db.flush().unwrap();
            }
            'd' => {
                delete(&db, ids.clone());
                ids.for_each(|i| {
                    live.remove(&i);
                });
                db.flush().unwrap();
            }
            'm' => {
                db.merge(&policy).unwrap();
            }
            _ => {
                db.vacuum().unwrap();
            }
        }
        states.push(live.clone());
        let Some((_, older_pages, file_pages)) = copy_with_damaged_manifest(&path, &copy) else { continue };
        extended += (file_pages < older_pages) as u32;
        let db = Database::open(&copy, opts(&src)).unwrap_or_else(|e| panic!("step {step}: {e}"));
        assert!(db.damaged().is_some(), "step {step}");
        db.check().unwrap_or_else(|e| panic!("step {step}: {e}"));
        let got: BTreeSet<u64> = live_keys(&db.snapshot()).keys().map(|k| k[1..].parse().unwrap()).collect();
        assert!(states.contains(&got), "step {step}: opened a state no commit made");
        checked += 1;
    }
    assert_eq!(checked, 20);
    // Some commit cut the file below the older version's length.
    assert!(extended > 0);
}

// In the directory layout a commit names its new files after ids and a txn
// that no committed version has used; a failed commit left them behind and
// every later commit failed on them (EEXIST) until a reopen. They are now
// replaced.
#[test]
fn files_a_failed_commit_left_do_not_fail_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.dir");
    let src = Arc::new(MapSource::default());
    let o = DbOptions { layout: Layout::Directory, ..opts(&src) };
    let db = Database::create(&path, schema(), o.clone()).unwrap();
    insert(&db, &src, 0..50);
    db.flush().unwrap();
    insert(&db, &src, 50..100);
    delete(&db, 0..10);
    // What a flush that failed partway leaves: the files it would write.
    let mut planted = Vec::new();
    for name in (0..20).map(strata::storage::dir::segment_name).chain((0..20).map(strata::storage::dir::manifest_name)) {
        let f = path.join(name);
        if !f.exists() {
            std::fs::write(&f, b"left by a failed commit").unwrap();
            planted.push(f);
        }
    }
    db.flush().unwrap();
    insert(&db, &src, 100..110);
    db.flush().unwrap();
    assert_keys(&db.snapshot(), &keys(10..110));
    drop(db);
    // Opening removes the planted files no version names.
    let db = Database::open(&path, o).unwrap();
    assert_keys(&db.snapshot(), &keys(10..110));
    db.check().unwrap();
}

/// A database with ten one-document transactions in its WAL (none
/// flushed), and the WAL's length after each.
fn wal_with_ten_commits(path: &Path, src: &Arc<MapSource>) -> Vec<u64> {
    let db = Database::create(path, schema(), DbOptions { sync: true, ..opts(src) }).unwrap();
    (0..10)
        .map(|i| {
            insert(&db, src, i..i + 1);
            std::fs::metadata(wal_of(path)).unwrap().len()
        })
        .collect()
}

fn corrupt_byte(path: &Path, at: u64) {
    let f = OpenOptions::new().read(true).write(true).open(path).unwrap();
    let mut b = [0u8];
    f.read_exact_at(&mut b, at).unwrap();
    f.write_all_at(&[b[0] ^ 0xFF], at).unwrap();
}

// A record damaged in the middle of the WAL cut it there, and the commits
// after it, durable and acknowledged, were dropped without a trace. Now
// opening fails, unless asked to drop them, and a copy of the log is kept.
#[test]
fn a_wal_damaged_mid_log_fails_to_open_unless_asked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let src = Arc::new(MapSource::default());
    let lens = wal_with_ten_commits(&path, &src);
    let wal = wal_of(&path);
    // Inside the third transaction.
    corrupt_byte(&wal, lens[1] + 12);
    let before = std::fs::read(&wal).unwrap();
    let sync = DbOptions { sync: true, ..opts(&src) };
    match Database::open(&path, sync.clone()) {
        Err(Error::Corrupt(m)) => assert!(m.contains("8 committed transactions follow the damage"), "{m}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("opened a WAL damaged mid-log"),
    }
    // Nothing was cut or copied.
    assert_eq!(std::fs::read(&wal).unwrap(), before);
    assert!(!damaged_copy_of(&wal).exists());

    let db = Database::open(&path, DbOptions { wal_drop_damaged: true, ..sync }).unwrap();
    assert!(db.damaged().unwrap().contains("8 committed transactions"));
    assert_keys(&db.snapshot(), &keys(0..2));
    assert_eq!(std::fs::read(damaged_copy_of(&wal)).unwrap(), before);
    // Later commits follow the kept prefix.
    insert(&db, &src, 20..21);
    drop(db);
    let db = Database::open(&path, opts(&src)).unwrap();
    assert!(db.damaged().is_none());
    assert_keys(&db.snapshot(), &keys((0..2).chain(20..21)));
}

// Without `sync` a power loss can tear any appends not yet written back,
// so the same damage drops the commits after it (as that mode allows), but
// keeps a copy and says so.
#[test]
fn without_sync_a_damaged_wal_drops_the_commits_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let src = Arc::new(MapSource::default());
    let lens = wal_with_ten_commits(&path, &src);
    let wal = wal_of(&path);
    corrupt_byte(&wal, lens[1] + 12);
    let db = Database::open(&path, opts(&src)).unwrap();
    assert!(db.damaged().unwrap().contains(&damaged_copy_of(&wal).display().to_string()));
    assert_keys(&db.snapshot(), &keys(0..2));
    assert!(damaged_copy_of(&wal).exists());
}

// A record with an intact checksum but a value its field cannot hold (an
// INT stored as a FLOAT here) was replayed into the memtable, and a later
// snapshot panicked building it. Now it counts as damage like a failed
// checksum.
#[test]
fn a_wal_record_that_does_not_fit_the_schema_is_damage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let src = Arc::new(MapSource::default());
    let lens = wal_with_ten_commits(&path, &src);
    let wal = wal_of(&path);
    // The third transaction's insert: the tag of `n` (after the key) goes
    // from INT to FLOAT, and the record's checksum is fixed.
    let mut b = std::fs::read(&wal).unwrap();
    let rec = lens[1] as usize;
    let tag = rec + b[rec..].windows(7).position(|w| w == key(2).as_bytes()).unwrap() + 7;
    assert_eq!(b[tag], 1);
    b[tag] = 2;
    let len = u32::from_le_bytes(b[rec..rec + 4].try_into().unwrap()) as usize;
    let crc = crc32fast::hash(&b[rec + 8..rec + 8 + len]);
    b[rec + 4..rec + 8].copy_from_slice(&crc.to_le_bytes());
    std::fs::write(&wal, &b).unwrap();

    let sync = DbOptions { sync: true, ..opts(&src) };
    match Database::open(&path, sync.clone()) {
        Err(Error::Corrupt(m)) => assert!(m.contains("8 committed transactions follow the damage"), "{m}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("replayed a document that does not fit the schema"),
    }
    let db = Database::open(&path, DbOptions { wal_drop_damaged: true, ..sync }).unwrap();
    assert_keys(&db.snapshot(), &keys(0..2));
}

// A crash during the last append tears the tail: cut off quietly, or, when
// the commit record of that last (unacknowledged) transaction survived the
// tear, cut off after a copy is kept.
#[test]
fn a_torn_wal_tail_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let src = Arc::new(MapSource::default());
    let lens = wal_with_ten_commits(&path, &src);
    let wal = wal_of(&path);
    let sync = DbOptions { sync: true, ..opts(&src) };

    OpenOptions::new().write(true).open(&wal).unwrap().set_len(lens[9] - 5).unwrap();
    let db = Database::open(&path, sync.clone()).unwrap();
    assert!(db.damaged().is_none());
    assert_keys(&db.snapshot(), &keys(0..9));
    assert!(!damaged_copy_of(&wal).exists());
    drop(db);

    // The last transaction's operation damaged, its commit record intact.
    let lens: Vec<u64> = lens[..9].to_vec();
    let db = Database::open(&path, sync.clone()).unwrap();
    insert(&db, &src, 9..10);
    let end = std::fs::metadata(&wal).unwrap().len();
    drop(db);
    assert!(end > lens[8]);
    corrupt_byte(&wal, lens[8] + 12);
    let db = Database::open(&path, sync).unwrap();
    assert!(db.damaged().unwrap().contains("1 committed transactions"));
    assert_keys(&db.snapshot(), &keys(0..9));
    assert!(damaged_copy_of(&wal).exists());
}

/// Where each segment of the newest version lies: `(file, offset, len)` of
/// its image and of its delete set, if any, for either layout.
#[allow(clippy::type_complexity)]
fn segment_bytes(path: &Path) -> Vec<((PathBuf, u64, u64), Option<(PathBuf, u64, u64)>)> {
    let manifest = if path.is_dir() {
        strata::storage::dir::read_current(path).unwrap().1
    } else {
        let f = std::fs::File::open(path).unwrap();
        let (meta, _) = read_metas(&f).unwrap()[0];
        Manifest::decode(&read_extent(&f, meta.manifest, meta.manifest_crc).unwrap()).unwrap()
    };
    let at = |e: Option<Extent>, file: &Option<String>| match (e, file) {
        (_, Some(f)) => Some((path.join(f), 0, std::fs::metadata(path.join(f)).unwrap().len())),
        (Some(e), None) => Some((path.to_path_buf(), e.page * PAGE, e.len)),
        _ => None,
    };
    manifest.segments().map(|s| (at(Some(s.extent), &s.file).unwrap(), at(s.deletes, &s.deletes_file))).collect()
}

// A merge rebuilds its inputs from their documents and gives the merged
// segment a fresh checksum, so a segment damaged on disk became a valid
// one and `check` stopped seeing the damage. A merge now checks its
// inputs first.
#[test]
fn a_merge_refuses_a_damaged_segment() {
    for name in ["db.strata", "db.dir"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let src = Arc::new(MapSource::default());
        {
            let db = open_or_create(&path, opts(&src));
            insert(&db, &src, 0..300);
            db.flush().unwrap();
            insert(&db, &src, 300..600);
            db.flush().unwrap();
        }
        let (file, off, len) = segment_bytes(&path)[0].0.clone();
        corrupt_byte(&file, off + len / 2);
        let db = Database::open(&path, opts(&src)).unwrap();
        let policy = MergePolicy { merge_factor: 2, ..MergePolicy::default() };
        match db.merge(&policy) {
            Err(Error::Corrupt(m)) => assert!(m.contains("fails its checksum"), "{name}: {m}"),
            other => panic!("{name}: merged a damaged segment: {other:?}"),
        }
        assert!(db.check().is_err(), "{name}: the damage is still there to see");
        assert_eq!(db.snapshot().segments().len(), 2, "{name}");
    }
}

// A delete set was read without its checksum, so damage to it brought
// deleted rows back, or deleted others, and the next delete wrote it out
// again under a fresh checksum. Opening now checks it (it is small).
#[test]
fn a_damaged_delete_set_fails_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let src = Arc::new(MapSource::default());
    {
        let db = Database::create(&path, schema(), opts(&src)).unwrap();
        insert(&db, &src, 0..300);
        db.flush().unwrap();
        delete(&db, 0..100);
        db.flush().unwrap();
        // A second version naming the same delete set, so that both meta
        // slots do.
        insert(&db, &src, 1000..1001);
        db.flush().unwrap();
    }
    let (file, off, _) = segment_bytes(&path).into_iter().find_map(|(_, d)| d).expect("a delete set");
    corrupt_byte(&file, off);
    match Database::open(&path, opts(&src)) {
        Err(Error::Corrupt(m)) => assert!(m.contains("delete set") && m.contains("fails its checksum"), "{m}"),
        Err(e) => panic!("{e}"),
        Ok(db) => panic!("opened with keys {:?}", live_keys(&db.snapshot()).len()),
    }
}

// With `verify_checksums`, opening reads every segment against its
// checksum: a version naming a damaged one is damaged, and opening falls
// back to the one before it. Without it, opening trusts the segment (its
// text lists are checked when first used).
#[test]
fn verify_checksums_skips_a_version_with_a_damaged_segment() {
    for name in ["db.strata", "db.dir"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let src = Arc::new(MapSource::default());
        {
            let db = open_or_create(&path, opts(&src));
            insert(&db, &src, 0..300);
            db.flush().unwrap();
            insert(&db, &src, 300..600);
            db.flush().unwrap();
        }
        let segs = segment_bytes(&path);
        let (file, off, len) = segs[1].0.clone();
        corrupt_byte(&file, off + len / 2);
        // Opening without the option trusts the segment.
        assert!(Database::open(&path, opts(&src)).unwrap().damaged().is_none(), "{name}");
        let verify = verify_opts(&src);
        if name.ends_with(".dir") {
            // One version on disk: nothing to fall back to.
            match Database::open(&path, verify) {
                Err(Error::Corrupt(m)) => assert!(m.contains("fails its checksum"), "{m}"),
                other => panic!("{name}: {:?}", other.map(|_| ())),
            }
        } else {
            let db = Database::open(&path, verify).unwrap();
            let why = db.damaged().expect("says what was skipped");
            assert!(why.contains("fails its checksum"), "{why}");
            assert_keys(&db.snapshot(), &keys(0..300));
        }
        // Damage that every version shares fails to open.
        corrupt_byte(&segs[0].0 .0, segs[0].0 .1 + segs[0].0 .2 / 2);
        assert!(matches!(Database::open(&path, verify_opts(&src)), Err(Error::Corrupt(_))), "{name}");
    }
}

fn verify_opts(src: &Arc<MapSource>) -> DbOptions {
    DbOptions { verify_checksums: true, ..opts(src) }
}
