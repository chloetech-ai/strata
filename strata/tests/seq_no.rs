//! Sequence numbers and terms. A database that numbers its writes
//! (`Database::start_seq_no`) gives every write transaction the next
//! number under its term, logs it in a numbered WAL (`STRWAL02`), and goes
//! on counting across flushes, background flushes, crashes, checkpoints,
//! merges and reopening; a new term (`Database::begin_term`) numbers the
//! writes after it. Starting is one commit that makes the database format
//! 3 with feature `seq_no`, which older versions refuse. A database that
//! does not number its writes is written as before.

mod storage_util;

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use storage_util::*;
use strata::db::Db;
use strata::storage::pager::{Meta, PAGE};
use strata::storage::{wal, CheckpointOptions, DbOptions, Layout, MergePolicy, SeqNo};
use strata::Value;

type Rows = BTreeMap<String, i64>;

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
        assert_eq!((magic(&path).as_str(), on_disk(&path)), ("STRWAL01", 1));
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
        db.storage().check().unwrap();
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
        assert_eq!(on_disk(&copy_path), 3);
        assert_eq!(upsert(&copy, &mut model, 7, 7), Some(5));
        assert_eq!(magic(&copy_path), "STRWAL02");
    }
}
