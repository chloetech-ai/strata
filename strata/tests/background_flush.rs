//! Background flushes (`Database::background_flush` and the flusher
//! thread): commits made while a flush builds its segments (inserts,
//! upserts, deletes and values set in place, also on the documents being
//! flushed) keep their effect, in the new segments and in the memtable,
//! and come back after reopening: from the new WAL file, and from the
//! sealed one when a crash came before the install. A flush of everything
//! in between makes the background one a no-op. The flusher thread keeps
//! the memtable bounded under a stream of commits. Damage in a sealed log
//! fails the open, or with `wal_drop_damaged` drops what follows it.

mod storage_util;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use storage_util::*;
use strata::db::Db;
use strata::storage::{wal, DbOptions, Layout};
use strata::{Error, Value};

type Rows = BTreeMap<String, i64>;

fn rows(db: &Db) -> Rows {
    let mut out = Rows::new();
    for d in db.documents(&db.snapshot()) {
        let d = d.unwrap();
        let k = d[0].as_str().unwrap().to_string();
        assert!(out.insert(k.clone(), d[1].as_i64().unwrap()).is_none(), "{k} is live twice");
    }
    out
}

fn options(layout: Layout, publish: bool) -> DbOptions {
    // Commits never fill the memtable: only the test flushes.
    DbOptions { memtable_max_docs: 1_000_000, memtable_chunk_docs: 64, segment_max_docs: 100, sync: false, publish_snapshots: publish, layout, ..DbOptions::default() }
}

fn path_for(dir: &Path, layout: Layout) -> PathBuf {
    dir.join(if layout == Layout::Directory { "db.dir" } else { "db.strata" })
}

fn wal_for(path: &Path) -> PathBuf {
    if path.is_dir() {
        return path.join("wal");
    }
    let mut s = path.as_os_str().to_owned();
    s.push("-wal");
    s.into()
}

fn sealed(path: &Path) -> Vec<u64> {
    wal::sealed_files(&wal_for(path)).unwrap().into_iter().map(|(t, _)| t).collect()
}

/// Retries while another writer (the flusher) holds the lock.
fn retry<T>(mut f: impl FnMut() -> strata::Result<T>) -> T {
    loop {
        match f() {
            Err(Error::Busy(_)) => std::thread::yield_now(),
            r => return r.unwrap(),
        }
    }
}

/// Commits `n` random transactions on keys below `keys`, applying them to
/// `model`.
fn write(db: &Db, rng: &mut StdRng, model: &mut Rows, n: usize, keys: u64) {
    for _ in 0..n {
        let mut local = model.clone();
        let mut tx = retry(|| db.storage().begin());
        for _ in 0..rng.gen_range(1..8) {
            let i = rng.gen_range(0..keys);
            let (k, v) = (key(i), rng.gen_range(0..1000));
            let kv = Value::Str(k.clone());
            match rng.gen_range(0..5) {
                0 | 1 => {
                    tx.upsert(doc(i, v)).unwrap();
                    local.insert(k, v);
                }
                2 => assert_eq!(tx.delete(&kv).unwrap(), local.remove(&k).is_some(), "delete {k}"),
                _ => {
                    assert_eq!(tx.set(&kv, "n", Value::Int(v)).unwrap(), local.contains_key(&k), "set {k}");
                    if let Some(x) = local.get_mut(&k) {
                        *x = v;
                    }
                }
            }
        }
        tx.commit().unwrap();
        *model = local;
    }
}

/// Copies the database's files as a crash at this moment would leave them
/// (nothing is being written).
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
fn writes_made_while_a_flush_builds_keep_their_effect() {
    for (seed, layout, publish) in [(1, Layout::SingleFile, false), (2, Layout::SingleFile, true), (3, Layout::Directory, false), (4, Layout::Directory, true)] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), layout);
        let mut rng = StdRng::seed_from_u64(seed);
        let mut model = Rows::new();
        let mut db = Db::create(&path, schema(), options(layout, publish)).unwrap();
        for round in 0..6 {
            let what = format!("seed {seed}, round {round}");
            write(&db, &mut rng, &mut model, 120, 400);
            let before = db.storage().file_stats().memtable_docs;
            db.storage().background_flush_with(|| write(&db, &mut rng, &mut model, 40, 400)).unwrap();
            assert_eq!(rows(&db), model, "{what}");
            let stats = db.storage().file_stats();
            assert!(!stats.segments.is_empty(), "{what}: nothing was flushed");
            assert!(stats.memtable_docs < before, "{what}: {} documents in the memtable, {before} before", stats.memtable_docs);
            assert!(sealed(&path).is_empty(), "{what}: the sealed log stayed");
            assert!(db.storage().flush_error().is_none());
            db.storage().check().unwrap();
            if round % 2 == 1 {
                // What the memtable kept comes back from the new log.
                drop(db);
                db = Db::open(&path, options(layout, publish)).unwrap();
                assert_eq!(rows(&db), model, "{what}, reopened");
            }
        }
        // A flush of everything meanwhile: the background one does nothing.
        write(&db, &mut rng, &mut model, 50, 400);
        let segments = db.storage().file_stats().segments.len();
        db.storage()
            .background_flush_with(|| {
                write(&db, &mut rng, &mut model, 10, 400);
                db.storage().flush().unwrap();
                write(&db, &mut rng, &mut model, 10, 400);
            })
            .unwrap();
        assert_eq!(rows(&db), model);
        assert!(db.storage().file_stats().segments.len() > segments);
        assert!(sealed(&path).is_empty());
        db.storage().check().unwrap();
        drop(db);
        let db = Db::open(&path, options(layout, publish)).unwrap();
        assert_eq!(rows(&db), model, "seed {seed}, reopened");
        db.storage().check().unwrap();
    }
}

#[test]
fn a_crash_before_the_install_replays_the_sealed_log() {
    for (seed, layout) in [(5, Layout::SingleFile), (6, Layout::Directory)] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), layout);
        let copy = dir.path().join(if layout == Layout::Directory { "copy.dir" } else { "copy.strata" });
        let left = dir.path().join("left");
        let mut rng = StdRng::seed_from_u64(seed);
        let mut model = Rows::new();
        let db = Db::create(&path, schema(), options(layout, false)).unwrap();
        write(&db, &mut rng, &mut model, 100, 300);
        db.storage().flush().unwrap();
        write(&db, &mut rng, &mut model, 100, 300);
        let mut at_crash = Rows::new();
        let mut sealed_at = Vec::new();
        db.storage()
            .background_flush_with(|| {
                write(&db, &mut rng, &mut model, 30, 300);
                // A crash now: the sealed log and the new one hold everything.
                copy_files(&path, &copy);
                at_crash = model.clone();
                sealed_at = wal::sealed_files(&wal_for(&path)).unwrap();
                assert_eq!(sealed_at.len(), 1);
                std::fs::copy(&sealed_at[0].1, &left).unwrap();
            })
            .unwrap();
        assert_eq!(rows(&db), model);
        drop(db);

        let crashed = Db::open(&copy, options(layout, false)).unwrap();
        assert_eq!(rows(&crashed), at_crash, "seed {seed}: reopened after a crash before the install");
        assert_eq!(sealed(&copy), vec![sealed_at[0].0], "the sealed log stays until a flush installs it");
        crashed.storage().check().unwrap();
        crashed.storage().background_flush().unwrap();
        assert!(sealed(&copy).is_empty());
        assert_eq!(rows(&crashed), at_crash);
        drop(crashed);
        let crashed = Db::open(&copy, options(layout, false)).unwrap();
        assert_eq!(rows(&crashed), at_crash);
        drop(crashed);

        // A crash after the install, before the sealed log was deleted: it
        // is skipped (the version holds it) and deleted.
        std::fs::copy(&left, &sealed_at[0].1).unwrap();
        let db = Db::open(&path, options(layout, false)).unwrap();
        assert_eq!(rows(&db), model, "seed {seed}: reopened with an installed sealed log left");
        assert!(sealed(&path).is_empty());
        db.storage().check().unwrap();
    }
}

#[test]
fn damage_in_a_sealed_log_fails_the_open_unless_asked_to_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let copy = dir.path().join("copy.strata");
    let mut rng = StdRng::seed_from_u64(7);
    let mut model = Rows::new();
    let db = Db::create(&path, schema(), options(Layout::SingleFile, false)).unwrap();
    write(&db, &mut rng, &mut model, 200, 300);
    db.storage().background_flush_with(|| write(&db, &mut rng, &mut model, 20, 300)).unwrap_or(());
    db.storage()
        .background_flush_with(|| {
            write(&db, &mut rng, &mut model, 20, 300);
            copy_files(&path, &copy);
        })
        .unwrap();
    drop(db);
    let (t, p) = wal::sealed_files(&wal_for(&copy)).unwrap().pop().unwrap();
    let mut bytes = std::fs::read(&p).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x5a;
    std::fs::write(&p, &bytes).unwrap();

    let err = Db::open(&copy, options(Layout::SingleFile, false)).err().expect("a damaged sealed log fails the open");
    assert!(err.to_string().contains(&p.display().to_string()), "{err}");
    let db = Db::open(&copy, DbOptions { wal_drop_damaged: true, ..options(Layout::SingleFile, false) }).unwrap();
    let what = db.storage().damaged().expect("says what it dropped").to_string();
    assert!(what.contains(&p.display().to_string()), "{what}");
    assert!(wal::damaged_path(&p).exists() && wal::damaged_path(&wal_for(&copy)).exists());
    let kept = rows(&db);
    db.storage().check().unwrap();
    // Writes go on, and the next open needs no permission to drop.
    let mut after = kept.clone();
    write(&db, &mut rng, &mut after, 20, 300);
    drop(db);
    let db = Db::open(&copy, options(Layout::SingleFile, false)).unwrap();
    assert_eq!(rows(&db), after);
    assert_eq!(sealed(&copy), vec![t]);
}

#[test]
fn the_flusher_keeps_the_memtable_bounded_under_a_stream_of_commits() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), layout);
        let o = DbOptions { memtable_max_docs: 300, memtable_chunk_docs: 64, segment_max_docs: 1000, sync: false, publish_snapshots: true, layout, ..DbOptions::default() };
        let mut rng = StdRng::seed_from_u64(8);
        let mut model = Rows::new();
        let db = Db::create(&path, schema(), o.clone()).unwrap();
        db.spawn_flusher();
        let mut most = 0;
        for _ in 0..60 {
            write(&db, &mut rng, &mut model, 50, 5000);
            most = most.max(db.storage().file_stats().memtable_docs);
        }
        // Twice the limit and one transaction more at most.
        assert!(most <= 2 * 300 + 8, "{most} documents in the memtable");
        let deadline = Instant::now() + Duration::from_secs(20);
        while db.storage().file_stats().memtable_docs > 300 {
            assert!(Instant::now() < deadline, "the flusher did not catch up");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(db.storage().flush_error().is_none());
        assert_eq!(rows(&db), model);
        db.storage().check().unwrap();
        drop(db);
        // The flusher ended with the database: it opens again at once.
        let db = Db::open(&path, o).unwrap();
        assert_eq!(rows(&db), model);
        db.storage().check().unwrap();
    }
}
