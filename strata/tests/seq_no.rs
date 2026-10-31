//! Sequence numbers, terms and row versions. A database that numbers its
//! writes (`Database::start_seq_no`) gives every write transaction the
//! next number under its term, logs it in a numbered WAL (`STRWAL02`), and
//! goes on counting across flushes, background flushes, crashes,
//! checkpoints, merges and reopening; a new term (`Database::begin_term`)
//! numbers the writes after it, and the manifest keeps where each term
//! began. Each row keeps the number of the write that last changed it
//! (`Database::seq_of`) through all of those too. Starting is one commit
//! that makes the database format 3 with feature `seq_no`, which older
//! versions refuse. A database that does not number its writes is written
//! as before.

mod storage_util;

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use storage_util::*;
use strata::db::Db;
use strata::query::ir::{self, Plan};
use strata::query::Params;
use strata::storage::pager::{Meta, PAGE};
use strata::storage::{wal, CheckpointOptions, DbOptions, Layout, MergePolicy, SeqNo};
use strata::Value;

type Rows = BTreeMap<String, i64>;
/// Each live key's sequence number.
type Seqs = BTreeMap<String, u64>;

fn options(layout: Layout) -> DbOptions {
    // Commits never fill the memtable: only the test flushes.
    DbOptions { memtable_max_docs: 1_000_000, memtable_chunk_docs: 64, segment_max_docs: 100, sync: false, layout, ..DbOptions::default() }
}

fn path_for(dir: &Path, name: &str, layout: Layout) -> PathBuf {
    dir.join(if layout == Layout::Directory { format!("{name}.dir") } else { format!("{name}.strata") })
}

fn wal_for(path: &Path) -> PathBuf {
    if path.is_dir() {
        return path.join("wal");
    }
    let mut s = path.as_os_str().to_owned();
    s.push("-wal");
    s.into()
}

fn magic(path: &Path) -> String {
    String::from_utf8(std::fs::read(wal_for(path)).unwrap()[..8].to_vec()).unwrap()
}

/// The format of the newest committed version on disk.
fn on_disk(path: &Path) -> u32 {
    if path.is_dir() {
        let cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
        return cur.get("format").map_or(1, |f| f.as_u64().unwrap() as u32);
    }
    let f = File::open(path).unwrap();
    (0..2u64)
        .filter_map(|slot| {
            let mut page = vec![0u8; PAGE as usize];
            f.read_exact_at(&mut page, slot * PAGE).unwrap();
            Meta::decode(&page).unwrap()
        })
        .max_by_key(|m| m.txn)
        .unwrap()
        .format
}

fn rows(db: &Db) -> Rows {
    db.documents(&db.snapshot()).map(|d| d.unwrap()).map(|d| (d[0].as_str().unwrap().to_string(), d[1].as_i64().unwrap())).collect()
}

fn upsert(db: &Db, model: &mut Rows, i: u64, v: i64) -> Option<u64> {
    let mut tx = db.storage().begin().unwrap();
    tx.upsert(doc(i, v)).unwrap();
    model.insert(key(i), v);
    tx.commit_seq().unwrap()
}

fn seq(term: u64, seq: u64) -> Option<SeqNo> {
    Some(SeqNo { term, seq })
}

/// Copies the database's files as a crash at this moment would leave them.
fn copy_files(path: &Path, to: &Path) {
    if path.is_dir() {
        std::fs::create_dir(to).unwrap();
        for e in std::fs::read_dir(path).unwrap() {
            let e = e.unwrap();
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
        return;
    }
    std::fs::copy(path, to).unwrap();
    std::fs::copy(wal_for(path), wal_for(to)).unwrap();
    for (t, p) in wal::sealed_files(&wal_for(path)).unwrap() {
        std::fs::copy(p, wal::sealed_path(&wal_for(to), t)).unwrap();
    }
}

#[test]
fn writes_are_numbered_once_started() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "db", layout);
        let mut model = Rows::new();
        let db = Db::create(&path, schema(), options(layout)).unwrap();
        // Not numbered: written as before.
        assert_eq!(upsert(&db, &mut model, 1, 1), None);
        assert_eq!(db.storage().seq_no(), None);
        assert_eq!((magic(&path).as_str(), on_disk(&path)), ("STRWAL01", 3));
        assert!(db.storage().begin_term(2).unwrap_err().to_string().contains("does not number its writes"));

        db.storage().start_seq_no().unwrap();
        assert_eq!(db.storage().seq_no(), seq(1, 0));
        assert_eq!((magic(&path).as_str(), on_disk(&path)), ("STRWAL02", 3));
        // The write before is in a segment now, not in the log.
        assert_eq!(db.storage().file_stats().memtable_docs, 0);
        for i in 1..=5 {
            assert_eq!(upsert(&db, &mut model, i + 1, i as i64), Some(i));
        }
        // A transaction that writes nothing takes no number; deletes and
        // values set in place do.
        assert_eq!(db.storage().begin().unwrap().commit_seq().unwrap(), None);
        let mut tx = db.storage().begin().unwrap();
        assert!(tx.delete(&Value::Str(key(1))).unwrap());
        model.remove(&key(1));
        assert_eq!(tx.commit_seq().unwrap(), Some(6));
        let mut tx = db.storage().begin().unwrap();
        assert!(tx.set(&Value::Str(key(2)), "n", Value::Int(40)).unwrap());
        model.insert(key(2), 40);
        assert_eq!(tx.commit_seq().unwrap(), Some(7));
        // Starting again changes nothing.
        db.storage().start_seq_no().unwrap();
        assert_eq!(db.storage().seq_no(), seq(1, 7));

        // Reopened: the numbers come back from the log.
        drop(db);
        let db = Db::open(&path, options(layout)).unwrap();
        assert_eq!((db.storage().seq_no(), rows(&db)), (seq(1, 7), model.clone()));
        assert_eq!(upsert(&db, &mut model, 9, 9), Some(8));
        // Flushed (the log is emptied) and reopened: from the manifest.
        db.storage().flush().unwrap();
        drop(db);
        let db = Db::open(&path, options(layout)).unwrap();
        assert_eq!((db.storage().seq_no(), rows(&db)), (seq(1, 8), model.clone()));
        assert_eq!(magic(&path), "STRWAL02");

        // A new term numbers the writes after it, from where they were.
        assert!(db.storage().begin_term(1).unwrap_err().to_string().contains("term 1 is not after the database's term, 1"));
        db.storage().begin_term(3).unwrap();
        assert_eq!(db.storage().seq_no(), seq(3, 8));
        assert_eq!(upsert(&db, &mut model, 10, 10), Some(9));
        drop(db);
        let db = Db::open(&path, options(layout)).unwrap();
        assert_eq!(db.storage().seq_no(), seq(3, 9));
        // The manifest keeps where each term began.
        assert_eq!(db.storage().numbering().unwrap().terms, vec![(1, 1), (3, 9)]);
        // A merge commits a version too, and keeps the count.
        for i in 0..3 {
            upsert(&db, &mut model, 20 + i, 0);
            db.storage().flush().unwrap();
        }
        let merged = db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
        assert!(merged.docs_merged > 0);
        drop(db);
        let db = Db::open(&path, options(layout)).unwrap();
        assert_eq!((db.storage().seq_no(), rows(&db)), (seq(3, 12), model.clone()));
        assert_eq!(db.storage().numbering().unwrap().terms, vec![(1, 1), (3, 9)]);
        db.storage().check().unwrap();
    }
}

/// A snapshot says the number of the last transaction it holds, and keeps
/// it as later writes, flushes and merges go on, published by the writer
/// or built by the reader.
#[test]
fn a_snapshot_says_the_last_transaction_it_holds() {
    for publish_snapshots in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "db", Layout::SingleFile);
        let opts = || DbOptions { publish_snapshots, ..options(Layout::SingleFile) };
        let mut model = Rows::new();
        let db = Db::create(&path, schema(), opts()).unwrap();
        upsert(&db, &mut model, 0, 0);
        assert_eq!(db.snapshot().seq_no(), None, "not numbered");
        db.storage().start_seq_no().unwrap();
        assert_eq!(db.snapshot().seq_no(), Some(0));
        for i in 1..=3 {
            upsert(&db, &mut model, i, i as i64);
            assert_eq!(db.snapshot().seq_no(), Some(i));
        }
        let old = db.snapshot();
        let at_3 = model.clone();
        for i in 4..=6 {
            upsert(&db, &mut model, i % 3, i as i64);
        }
        db.storage().flush().unwrap();
        upsert(&db, &mut model, 9, 9);
        db.storage().flush().unwrap();
        db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
        // Values set in place and deletes are transactions too.
        let mut tx = db.storage().begin().unwrap();
        tx.set(&Value::Str(key(1)), "n", Value::Int(41)).unwrap();
        model.insert(key(1), 41);
        tx.commit_seq().unwrap();
        let mut tx = db.storage().begin().unwrap();
        assert!(tx.delete(&Value::Str(key(2))).unwrap());
        model.remove(&key(2));
        assert_eq!(tx.commit_seq().unwrap(), Some(9));
        assert_eq!((db.snapshot().seq_no(), rows(&db)), (Some(9), model.clone()));
        let in_old: Rows = db.documents(&old).map(|d| d.unwrap()).map(|d| (d[0].as_str().unwrap().to_string(), d[1].as_i64().unwrap())).collect();
        assert_eq!((old.seq_no(), in_old), (Some(3), at_3), "an older snapshot keeps its number and rows");
        drop((old, db));
        let db = Db::open(&path, opts()).unwrap();
        assert_eq!(db.snapshot().seq_no(), Some(9));
    }
}

/// The log of a crash holds the numbers of the transactions it holds,
/// sealed by a background flush or not; reopened, the database goes on
/// from the last.
#[test]
fn numbers_survive_crashes_and_background_flushes() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "db", layout);
        let mut model = Rows::new();
        let db = Db::create(&path, schema(), options(layout)).unwrap();
        db.storage().start_seq_no().unwrap();
        for i in 0..10 {
            upsert(&db, &mut model, i, i as i64);
        }
        let mut at_crash = Rows::new();
        let crash = path_for(dir.path(), "crash", layout);
        db.storage()
            .background_flush_with(|| {
                for i in 10..15 {
                    upsert(&db, &mut model, i, i as i64);
                }
                assert_eq!(wal::sealed_files(&wal_for(&path)).unwrap().len(), 1);
                copy_files(&path, &crash);
                at_crash = model.clone();
            })
            .unwrap();
        assert_eq!(db.storage().seq_no(), seq(1, 15));
        let after = path_for(dir.path(), "after", layout);
        copy_files(&path, &after);
        drop(db);

        // Before the install: the sealed log and the open one.
        let crashed = Db::open(&crash, options(layout)).unwrap();
        assert_eq!((crashed.storage().seq_no(), rows(&crashed)), (seq(1, 15), at_crash.clone()));
        assert_eq!(upsert(&crashed, &mut at_crash, 30, 30), Some(16));
        crashed.storage().check().unwrap();
        // After it: the version and the open log.
        let reopened = Db::open(&after, options(layout)).unwrap();
        assert_eq!((reopened.storage().seq_no(), rows(&reopened)), (seq(1, 15), model.clone()));
    }
}

/// A crash right after the commit that starts numbering, before the log was
/// started again numbered, leaves an empty log of the old kind: opening
/// starts it numbered. One holding transactions is refused.
#[test]
fn a_crash_while_starting_leaves_an_empty_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_for(dir.path(), "db", Layout::SingleFile);
    let mut model = Rows::new();
    let db = Db::create(&path, schema(), options(Layout::SingleFile)).unwrap();
    upsert(&db, &mut model, 1, 1);
    let unnumbered = std::fs::read(wal_for(&path)).unwrap();
    db.storage().flush().unwrap();
    let empty = std::fs::read(wal_for(&path)).unwrap();
    db.storage().start_seq_no().unwrap();
    drop(db);

    std::fs::write(wal_for(&path), &empty).unwrap();
    let db = Db::open(&path, options(Layout::SingleFile)).unwrap();
    assert_eq!((db.storage().seq_no(), rows(&db)), (seq(1, 0), model.clone()));
    assert_eq!(magic(&path), "STRWAL02");
    assert_eq!(upsert(&db, &mut model, 2, 2), Some(1));
    drop(db);

    std::fs::write(wal_for(&path), &unnumbered).unwrap();
    let e = Db::open(&path, options(Layout::SingleFile)).err().unwrap().to_string();
    assert!(e.contains("holds transactions without sequence numbers, and the database numbers its writes"), "{e}");
}

/// A checkpoint of a numbered database numbers its writes on from the
/// source's; one of a database that does not stays as it was.
#[test]
fn checkpoints_keep_the_count() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "db", Layout::SingleFile);
        let mut model = Rows::new();
        let db = Db::create(&path, schema(), options(Layout::SingleFile)).unwrap();
        upsert(&db, &mut model, 1, 1);
        let plain = path_for(dir.path(), "plain", layout);
        db.storage().checkpoint(&plain, &CheckpointOptions { layout, ..CheckpointOptions::default() }).unwrap();
        db.storage().start_seq_no().unwrap();
        db.storage().begin_term(4).unwrap();
        for i in 2..6 {
            upsert(&db, &mut model, i, i as i64);
        }
        let copy = path_for(dir.path(), "copy", layout);
        db.storage().checkpoint(&copy, &CheckpointOptions { layout, ..CheckpointOptions::default() }).unwrap();
        assert_eq!(upsert(&db, &mut model.clone(), 9, 9), Some(5));

        let plain = Db::open(&plain, options(layout)).unwrap();
        assert_eq!(plain.storage().seq_no(), None);
        let copy_path = copy;
        let copy = Db::open(&copy_path, options(layout)).unwrap();
        assert_eq!((copy.storage().seq_no(), rows(&copy)), (seq(4, 4), model.clone()));
        assert_eq!(copy.storage().numbering().unwrap().terms, vec![(1, 1), (4, 1)]);
        assert_eq!(on_disk(&copy_path), 3);
        assert_eq!(upsert(&copy, &mut model, 7, 7), Some(5));
        assert_eq!(magic(&copy_path), "STRWAL02");
    }
}

/// Commits `n` random transactions on keys below `keys` (upserts, deletes,
/// values set in place and added to), applying them to `rows` and `seqs`;
/// in between, a transaction's `seq_of` gives the committed number of a
/// row it has not written, and `None` for one it has.
fn churn(db: &Db, rng: &mut StdRng, rows: &mut Rows, seqs: &mut Seqs, n: usize, keys: u64) {
    for _ in 0..n {
        let mut tx = db.storage().begin().unwrap();
        let mut r = rows.clone();
        let mut touched: Vec<String> = Vec::new();
        for _ in 0..rng.gen_range(1..5) {
            let i = rng.gen_range(0..keys);
            let (k, v) = (key(i), rng.gen_range(0..1000));
            let kv = Value::Str(k.clone());
            let wrote = match rng.gen_range(0..6) {
                0 | 1 => {
                    tx.upsert(doc(i, v)).unwrap();
                    r.insert(k.clone(), v);
                    true
                }
                2 => tx.delete(&kv).unwrap() && r.remove(&k).is_some(),
                3 => tx.set(&kv, "n", Value::Int(v)).unwrap() && r.insert(k.clone(), v).is_some(),
                4 => tx.add(&kv, "n", &Value::Int(1)).unwrap() && r.get_mut(&k).map(|x| *x += 1).is_some(),
                _ => {
                    let want = if touched.contains(&k) { None } else { seqs.get(&k).copied() };
                    assert_eq!(tx.seq_of(&kv).unwrap(), want, "{k} in a transaction");
                    false
                }
            };
            if wrote {
                touched.push(k);
            }
        }
        let seq = tx.commit_seq().unwrap();
        assert_eq!(seq.is_some(), !touched.is_empty());
        for k in touched {
            match r.contains_key(&k) {
                true => seqs.insert(k, seq.unwrap()),
                false => seqs.remove(&k),
            };
        }
        *rows = r;
    }
}

/// Each row's `seq_no()`, by key.
fn select_seqs(db: &Db) -> Seqs {
    let r = db.query("SELECT id, seq_no() FROM t LIMIT 100000", &Params::new()).unwrap();
    r.rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap() as u64)).collect()
}

/// Each live row's number as `Snapshot::row_versions` lists them, once
/// each.
fn row_versions(db: &Db) -> Seqs {
    let mut out = Seqs::new();
    db.snapshot()
        .row_versions(|k, seq| {
            let k = k.expect("a keyed table").as_str().unwrap().to_string();
            assert!(out.insert(k.clone(), seq).is_none(), "{k} listed twice");
        })
        .unwrap();
    out
}

/// The rows and every key's number are as the model has them, by
/// `seq_of`, by `seq_no()` in a SELECT and by `row_versions`.
fn check_seqs(db: &Db, want: &Rows, seqs: &Seqs, keys: u64, what: &str) {
    assert_eq!(&rows(db), want, "{what}");
    for i in 0..keys {
        let k = key(i);
        assert_eq!(db.storage().seq_of(Some("t"), &Value::Str(k.clone())).unwrap(), seqs.get(&k).copied(), "{what}: {k}");
    }
    assert_eq!(&select_seqs(db), seqs, "{what}: seq_no()");
    assert_eq!(&row_versions(db), seqs, "{what}: row_versions");
}

/// Rows keep the number of the write that last changed them, through the
/// memtable, flushes, background flushes and merges with writes during
/// them, reopening and checkpoints, against a model; rows from before
/// numbering and bulk loaded ones have number 0.
#[test]
fn rows_keep_the_number_of_their_last_write() {
    for (seed, layout) in [(1, Layout::SingleFile), (2, Layout::Directory), (3, Layout::SingleFile), (4, Layout::Directory)] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), "db", layout);
        // Small memtable chunks: a query builds segments of the full ones
        // once, and of the last one again after each write.
        let opts = DbOptions { segment_max_docs: 40, memtable_chunk_docs: 8, ..options(layout) };
        let mut rng = StdRng::seed_from_u64(seed);
        let (mut rows, mut seqs, keys) = (Rows::new(), Seqs::new(), 300);
        let mut db = Db::create(&path, schema(), opts.clone()).unwrap();
        assert!(db.storage().seq_of(None, &Value::Str(key(0))).unwrap_err().to_string().contains("does not number its writes"));
        assert!(db.snapshot().row_versions(|_, _| ()).unwrap_err().to_string().contains("does not number its writes"));
        // Before numbering: number 0.
        let mut tx = db.storage().begin().unwrap();
        for i in 0..20 {
            tx.upsert(doc(i, 5)).unwrap();
            rows.insert(key(i), 5);
            seqs.insert(key(i), 0);
        }
        tx.commit().unwrap();
        db.storage().start_seq_no().unwrap();
        // Bulk loaded: number 0 too.
        db.storage().bulk_load((20..30).map(|i| doc(i, 6))).unwrap();
        for i in 20..30 {
            rows.insert(key(i), 6);
            seqs.insert(key(i), 0);
        }
        check_seqs(&db, &rows, &seqs, keys, "start");
        let copies = dir.path().join("copies");
        std::fs::create_dir(&copies).unwrap();
        for round in 0..24 {
            churn(&db, &mut rng, &mut rows, &mut seqs, 15, keys);
            let what = format!("seed {seed}, round {round}");
            check_seqs(&db, &rows, &seqs, keys, &format!("{what}, written"));
            match rng.gen_range(0..6) {
                0 => db.storage().flush().unwrap(),
                1 => db.storage().background_flush_with(|| churn(&db, &mut rng, &mut rows, &mut seqs, 10, keys)).unwrap(),
                2 => {
                    db.storage().flush().unwrap();
                    let policy = MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() };
                    db.storage()
                        .merge_with(&policy, || {
                            churn(&db, &mut rng, &mut rows, &mut seqs, 5, keys);
                            true
                        })
                        .unwrap();
                }
                3 => {
                    drop(db);
                    db = Db::open(&path, opts.clone()).unwrap();
                }
                4 => {
                    let to = path_for(&copies, &format!("c{round}"), layout);
                    db.storage().checkpoint(&to, &CheckpointOptions { layout, ..CheckpointOptions::default() }).unwrap();
                    let copy = Db::open(&to, opts.clone()).unwrap();
                    check_seqs(&copy, &rows, &seqs, keys, &format!("{what}, checkpoint"));
                }
                _ => {}
            }
            check_seqs(&db, &rows, &seqs, keys, &what);
        }
        db.storage().check().unwrap();
        let segments = db.storage().file_stats().segments.len();
        assert!(segments > 2, "seed {seed}: {segments} segments");
    }
}

/// Rows of an attached segment have number 0, like loaded ones, also when
/// the segment was exported from a database that numbered them: its
/// numbers could equal those of later writes here.
#[test]
fn attached_rows_have_number_0() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let dir = tempfile::tempdir().unwrap();
        let (mut model, mut other) = (Rows::new(), Rows::new());
        let from = Db::create(&path_for(dir.path(), "from", layout), schema(), options(layout)).unwrap();
        from.storage().start_seq_no().unwrap();
        for i in 0..5 {
            upsert(&from, &mut other, i, i as i64);
        }
        from.storage().flush().unwrap();
        assert_eq!(from.storage().seq_of(None, &Value::Str(key(4))).unwrap(), Some(5));
        let exported = dir.path().join("seg");
        from.storage().export_segment(from.snapshot().segments()[0].id, &exported).unwrap();

        let db = Db::create(&path_for(dir.path(), "db", layout), schema(), options(layout)).unwrap();
        db.storage().start_seq_no().unwrap();
        assert_eq!(upsert(&db, &mut model, 10, 10), Some(1));
        let ids = db.storage().attach_segments(&[exported]).unwrap();
        model.extend(other);
        assert_eq!(rows(&db), model);
        for i in 0..5 {
            assert_eq!(db.storage().seq_of(None, &Value::Str(key(i))).unwrap(), Some(0), "{}", key(i));
        }
        assert_eq!(db.storage().seq_of(None, &Value::Str(key(10))).unwrap(), Some(1));
        let attached: Vec<_> = db.snapshot().segments().iter().filter(|s| ids.contains(&s.id)).map(|s| s.file.has("seq")).collect();
        assert_eq!(attached, [false]);
        let want: Seqs = (0..5).map(|i| (key(i), 0)).chain([(key(10), 1)]).collect();
        assert_eq!(row_versions(&db), want);
        db.storage().check().unwrap();
    }
}

/// `row_versions` lists the live rows of the table a snapshot looks at,
/// also of a table without a primary key, before and after a flush.
#[test]
fn row_versions_list_each_table_s_live_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&path_for(dir.path(), "db", Layout::SingleFile), schema(), options(Layout::SingleFile)).unwrap();
    db.storage().start_seq_no().unwrap();
    let fields = vec![strata::Field::new("line", strata::FieldType::Text { positions: false })];
    db.create_table(strata::Schema::new("log", fields, None).unwrap()).unwrap();
    let mut model = Rows::new();
    for i in 0..3 {
        upsert(&db, &mut model, i, 0);
        let mut tx = db.storage().begin().unwrap();
        tx.table("log").unwrap().insert(vec![Value::Str(format!("line {i}"))]).unwrap();
        tx.commit_seq().unwrap();
    }
    let mut tx = db.storage().begin().unwrap();
    assert!(tx.delete(&Value::Str(key(1))).unwrap());
    tx.commit_seq().unwrap();
    for flushed in [false, true] {
        let keyless = |s: &strata::storage::Snapshot| {
            let mut out = Vec::new();
            s.row_versions(|k, seq| out.push((k.cloned(), seq))).unwrap();
            out.sort_by_key(|x| x.1);
            out
        };
        assert_eq!(row_versions(&db), Seqs::from([(key(0), 1), (key(2), 5)]), "flushed {flushed}");
        assert_eq!(keyless(&db.snapshot().table("log").unwrap()), [(None, 2), (None, 4), (None, 6)], "flushed {flushed}");
        db.storage().flush().unwrap();
    }
}

/// `seq_no()` is a column of a SELECT on one table of a numbered database:
/// its plan needs feature `seq_no`, and it cannot be aggregated, ordered
/// by, or used in a compound query.
#[test]
fn seq_no_is_a_column_of_numbered_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&path_for(dir.path(), "db", Layout::SingleFile), schema(), options(Layout::SingleFile)).unwrap();
    let mut model = Rows::new();
    upsert(&db, &mut model, 1, 1);
    let sql = "SELECT id, seq_no() FROM t WHERE n >= 0 LIMIT 10";
    let e = db.query(sql, &Params::new()).unwrap_err().to_string();
    assert!(e.contains("the database does not number its writes"), "{e}");

    db.storage().start_seq_no().unwrap();
    for i in 2..5 {
        upsert(&db, &mut model, i, i as i64);
    }
    let r = db.query(sql, &Params::new()).unwrap();
    assert_eq!(r.columns, ["id", "seq_no()"]);
    let got: Vec<(String, i64)> = r.rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap())).collect();
    assert_eq!(got, [(key(1), 0), (key(2), 1), (key(3), 2), (key(4), 3)]);

    // The plan needs the feature, and runs the same from JSON.
    let plan = db.explain(sql, &Params::new()).unwrap();
    assert_eq!(plan.features, [ir::SEQ_NO]);
    assert!(db.explain("SELECT id FROM t LIMIT 10", &Params::new()).unwrap().features.is_empty());
    let read = Plan::from_json(&plan.to_json().unwrap()).unwrap();
    assert_eq!(read, plan);
    assert_eq!(db.run_at(&db.snapshot(), &read, &Default::default()).unwrap().rows, r.rows);
    assert!(ir::FEATURES.contains(&"seq_no"));

    for (sql, says) in [
        ("SELECT COUNT(*), seq_no() FROM t", "cannot be aggregated"),
        ("SELECT id, seq_no() FROM t ORDER BY 2", "ORDER BY seq_no()"),
        ("SELECT id, seq_no() FROM t WHERE id IN (SELECT id FROM t WHERE n > 1)", "not of a compound query"),
    ] {
        let e = db.query(sql, &Params::new()).unwrap_err().to_string();
        assert!(e.contains(says), "{sql}: {e}");
    }
}
