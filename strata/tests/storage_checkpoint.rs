//! Checkpoints ([`Database::checkpoint`]): a consistent copy of an open
//! database, taken while it is written and merged, in either layout, with
//! unchanged segments hard-linked from the previous checkpoint.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use strata::db::Db;
use strata::query::Params;
use strata::storage::{CheckpointOptions, DbOptions, Layout, MergePolicy};
use strata::{Document, Field, FieldType, Schema, Value};

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![Field::new("id", FieldType::Keyword), Field::new("n", FieldType::Int), Field::new("body", FieldType::Text { positions: false })],
        Some("id"),
    )
    .unwrap()
}

fn side() -> Schema {
    Schema::new("u", vec![Field::new("id", FieldType::Keyword), Field::new("x", FieldType::Float)], Some("id")).unwrap()
}

fn key(i: u64) -> String {
    format!("k{i:06}")
}

fn doc(i: u64) -> Document {
    vec![Value::Str(key(i)), Value::Int(i as i64), Value::Str(format!("document {i} about {}", ["oak", "elm", "ash"][i as usize % 3]))]
}

fn opts(layout: Layout) -> DbOptions {
    DbOptions { layout, memtable_max_docs: 60, segment_max_docs: 50, sync: false, ..DbOptions::default() }
}

fn path_for(dir: &Path, name: &str, layout: Layout) -> std::path::PathBuf {
    dir.join(if layout == Layout::Directory { format!("{name}.dir") } else { format!("{name}.strata") })
}

/// Every row of every table, by table and key.
fn rows(db: &Db) -> BTreeMap<String, BTreeMap<String, Document>> {
    let snap = db.snapshot();
    let mut out = BTreeMap::new();
    for s in db.catalog() {
        let t = snap.table(&s.table).unwrap();
        let rows = db.documents(&t).map(|d| d.unwrap()).map(|d| (d[0].as_str().unwrap().to_string(), d)).collect();
        out.insert(s.table.clone(), rows);
    }
    out
}

fn segment_ids(db: &Db) -> Vec<u64> {
    db.snapshot().segments().iter().filter(|s| !s.in_memory).map(|s| s.id).collect()
}

/// A database with two tables, deletes, values set in place and committed
/// transactions still in the memtable.
fn populated(path: &Path, layout: Layout) -> Db {
    let db = Db::create(path, schema(), opts(layout)).unwrap();
    db.create_table(side()).unwrap();
    db.load((0..300).map(doc)).unwrap();
    let mut t = db.storage().begin().unwrap();
    for i in (0..300).step_by(7) {
        t.delete(&Value::Str(key(i))).unwrap();
    }
    for i in (1..300).step_by(11) {
        t.set(&Value::Str(key(i)), "n", Value::Int(-(i as i64))).unwrap();
    }
    let mut u = t.table("u").unwrap();
    for i in 0..40 {
        u.upsert(vec![Value::Str(key(i)), Value::Float(i as f64 / 2.0)]).unwrap();
    }
    t.commit().unwrap();
    db.storage().flush().unwrap();
    // Committed, but only in the WAL and the memtable.
    let mut t = db.storage().begin().unwrap();
    for i in 300..330 {
        t.insert(doc(i)).unwrap();
    }
    t.delete(&Value::Str(key(2))).unwrap();
    t.commit().unwrap();
    db
}

#[test]
fn a_checkpoint_holds_every_committed_row_in_either_layout() {
    for from in [Layout::SingleFile, Layout::Directory] {
        for to in [Layout::SingleFile, Layout::Directory] {
            let tmp = tempfile::tempdir().unwrap();
            let db = populated(&path_for(tmp.path(), "db", from), from);
            let want = rows(&db);
            let dest = path_for(tmp.path(), "copy", to);
            let stats = db.storage().checkpoint(&dest, &CheckpointOptions { layout: to, link_from: None }).unwrap();
            let ids = segment_ids(&db);
            let at = db.snapshot();
            assert_eq!(stats.tables, 2);
            assert_eq!(stats.live_docs, want.values().map(|t| t.len() as u64).sum::<u64>());
            assert_eq!(stats.txn, db.snapshot().txn());

            // The source goes on as before.
            let mut t = db.storage().begin().unwrap();
            t.insert(doc(1000)).unwrap();
            t.commit().unwrap();

            let copy = Db::open(&dest, opts(to)).unwrap();
            copy.storage().check().unwrap();
            assert_eq!(rows(&copy), want, "{from:?} to {to:?}");
            assert_eq!(copy.snapshot().txn(), stats.txn, "the copy's version is numbered as the source's");
            assert_eq!(segment_ids(&copy), ids, "segment ids kept");
            let p = Params::new().set("q", Value::Str("elm".into()));
            let sql = "SELECT id FROM t WHERE MATCH(body, :q) AND n >= 0 ORDER BY n DESC LIMIT 20";
            let (a, b) = (db.query_at(&at, sql, &p, &Default::default()).unwrap(), copy.query_at(&copy.snapshot(), sql, &p, &Default::default()).unwrap());
            assert_eq!(a.keys, b.keys);
            // Only live data: no WAL to replay, and no free pages but the
            // few the copy's own last commits freed (manifests, free lists).
            let fs = copy.storage().file_stats();
            assert_eq!((fs.memtable_docs, fs.pending_deletes), (0, 0));
            if to == Layout::SingleFile {
                assert!(fs.free_pages == 0 && fs.pending_pages <= 8, "{fs:?}");
            }
            drop(copy);
            let leftovers: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.contains("checkpoint")).collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }
    }
}

/// Checkpoints taken while one thread commits a key at a time and another
/// merges: each holds exactly the keys of one moment, a prefix of the
/// writer's, and passes its checks.
#[test]
fn checkpoints_during_writes_and_merges_are_consistent() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::create(&path_for(tmp.path(), "db", layout), schema(), opts(layout)).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (db, stop) = (db.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    loop {
                        match db.storage().begin() {
                            Ok(mut t) => {
                                t.insert(doc(i)).unwrap();
                                t.commit().unwrap();
                                break;
                            }
                            Err(strata::Error::Busy(_)) => std::thread::yield_now(),
                            Err(e) => panic!("{e}"),
                        }
                    }
                    i += 1;
                }
                i
            })
        };
        let merger = {
            let (db, stop) = (db.clone(), stop.clone());
            std::thread::spawn(move || {
                let policy = MergePolicy { merge_factor: 3, floor_docs: 30, ..MergePolicy::default() };
                while !stop.load(Ordering::Relaxed) {
                    db.storage().merge(&policy).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            })
        };
        let mut seen = Vec::new();
        for c in 0..8 {
            std::thread::sleep(std::time::Duration::from_millis(30));
            let dest = path_for(tmp.path(), &format!("copy{c}"), layout);
            db.storage().checkpoint(&dest, &CheckpointOptions { layout, link_from: None }).unwrap();
            let copy = Db::open(&dest, opts(layout)).unwrap();
            copy.storage().check().unwrap();
            let keys: Vec<String> = rows(&copy)["t"].keys().cloned().collect();
            let n = keys.len() as u64;
            assert_eq!(keys, (0..n).map(key).collect::<Vec<_>>(), "checkpoint {c} is a prefix");
            seen.push(n);
        }
        stop.store(true, Ordering::Relaxed);
        let written = writer.join().unwrap();
        merger.join().unwrap();
        assert!(seen.windows(2).all(|w| w[0] <= w[1]) && seen[0] > 0 && *seen.last().unwrap() <= written, "{seen:?} of {written}");
    }
}

/// A checkpoint in the directory layout hard-links the segments an earlier
/// one holds, and stands on its own once the earlier one is deleted.
#[test]
fn incremental_checkpoints_link_unchanged_segments() {
    let tmp = tempfile::tempdir().unwrap();
    let db = populated(&tmp.path().join("db.strata"), Layout::SingleFile);
    let o = |link_from| CheckpointOptions { layout: Layout::Directory, link_from };
    let first = tmp.path().join("first.dir");
    let a = db.storage().checkpoint(&first, &o(None)).unwrap();
    assert_eq!(a.segments_linked, 0);
    db.load((1000..1100).map(doc)).unwrap();
    let second = tmp.path().join("second.dir");
    let b = db.storage().checkpoint(&second, &o(Some(first.clone()))).unwrap();
    assert_eq!(b.segments_linked, a.segments, "every segment of the first is unchanged");
    assert!(b.segments > a.segments && b.bytes_written < a.bytes_written + 200_000, "{a:?} {b:?}");
    // Segment files of both tables, by name, with their inodes.
    let segs = |d: &Path| -> BTreeMap<String, u64> {
        std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_name().to_string_lossy().starts_with("seg-"))
            .map(|e| (e.file_name().into_string().unwrap(), e.metadata().unwrap().ino()))
            .collect()
    };
    let (fa, fb) = (segs(&first), segs(&second));
    assert_eq!(fa.len(), a.segments);
    assert!(fa.iter().all(|(name, ino)| fb.get(name) == Some(ino)), "{fa:?} {fb:?}");
    let want = rows(&db);
    std::fs::remove_dir_all(&first).unwrap();
    let copy = Db::open(&second, opts(Layout::Directory)).unwrap();
    copy.storage().check().unwrap();
    assert_eq!(rows(&copy), want);

    // A merge replaces segments: they are written, not linked.
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1000, ..MergePolicy::default() }).unwrap();
    let third = tmp.path().join("third.dir");
    let c = db.storage().checkpoint(&third, &o(Some(second.clone()))).unwrap();
    assert!(c.segments_linked < c.segments, "{c:?}");
    assert_eq!(rows(&Db::open(&third, opts(Layout::Directory)).unwrap()), rows(&db));
}

#[test]
fn a_failed_checkpoint_leaves_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let db = populated(&tmp.path().join("db.strata"), Layout::SingleFile);
    let dest = tmp.path().join("copy.strata");
    std::fs::write(&dest, b"in the way").unwrap();
    assert!(db.storage().checkpoint(&dest, &CheckpointOptions::default()).is_err(), "dest exists");
    assert_eq!(std::fs::read(&dest).unwrap(), b"in the way");
    let other = tmp.path().join("other.strata");
    let r = db.storage().checkpoint(&other, &CheckpointOptions { layout: Layout::SingleFile, link_from: Some(tmp.path().join("nothing")) });
    assert!(r.is_err(), "links need the directory layout");
    let r = db.storage().checkpoint(&tmp.path().join("x.dir"), &CheckpointOptions { layout: Layout::Directory, link_from: Some(tmp.path().join("nothing")) });
    assert!(r.is_err(), "no earlier checkpoint there");
    let names: Vec<String> = std::fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().all(|n| !n.contains("checkpoint") && n != "other.strata" && n != "x.dir"), "{names:?}");
    // The database is still whole and writable.
    db.storage().checkpoint(&other, &CheckpointOptions::default()).unwrap();
    let mut t = db.storage().begin().unwrap();
    t.insert(doc(5000)).unwrap();
    t.commit().unwrap();
}

/// A checkpoint is a plain database: restoring is opening it (or moving it
/// into place while the server is stopped), and it can be checkpointed.
#[test]
fn a_checkpoint_restores_and_checkpoints_again() {
    let tmp = tempfile::tempdir().unwrap();
    let db = populated(&tmp.path().join("db.strata"), Layout::Directory);
    let want = rows(&db);
    let dest = tmp.path().join("backup.strata");
    db.storage().checkpoint(&dest, &CheckpointOptions::default()).unwrap();
    drop(db);
    let restored = Db::open(&dest, opts(Layout::SingleFile)).unwrap();
    assert_eq!(rows(&restored), want);
    let mut t = restored.storage().begin().unwrap();
    t.insert(doc(9000)).unwrap();
    t.commit().unwrap();
    let again = tmp.path().join("again.dir");
    restored.storage().checkpoint(&again, &CheckpointOptions { layout: Layout::Directory, link_from: None }).unwrap();
    let mut want = want;
    want.get_mut("t").unwrap().insert(key(9000), doc(9000));
    assert_eq!(rows(&Db::open(&again, opts(Layout::Directory)).unwrap()), want);
}

/// Directory checkpoints synced one after another into one place, the way
/// `aws s3 sync` uploads them: files whose names are already there are not
/// copied again, `CURRENT` goes last. A name in two checkpoints holds the
/// same bytes in both, and the place opens as the latest one.
#[test]
fn directory_checkpoints_sync_into_one_place() {
    let tmp = tempfile::tempdir().unwrap();
    let db = populated(&tmp.path().join("db.strata"), Layout::SingleFile);
    let place = tmp.path().join("place");
    std::fs::create_dir(&place).unwrap();
    let mut previous: Option<std::path::PathBuf> = None;
    for round in 0..4u64 {
        match round {
            0 => {}
            1 => {
                db.load((1000..1100).map(doc)).unwrap();
            }
            2 => {
                // Deletes and in-place values change existing segments'
                // delete and update sets.
                let mut t = db.storage().begin().unwrap();
                for i in (3..1100).step_by(13) {
                    t.delete(&Value::Str(key(i))).unwrap();
                }
                for i in (5..300).step_by(17) {
                    t.set(&Value::Str(key(i)), "n", Value::Int(7)).unwrap();
                }
                t.commit().unwrap();
            }
            _ => {
                db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1000, ..MergePolicy::default() }).unwrap();
            }
        }
        let dest = tmp.path().join(format!("ck{round}"));
        let o = CheckpointOptions { layout: Layout::Directory, link_from: previous.clone() };
        db.storage().checkpoint(&dest, &o).unwrap();
        let mut copied = 0;
        for e in std::fs::read_dir(&dest).unwrap() {
            let name = e.unwrap().file_name().into_string().unwrap();
            if ["CURRENT", "LOCK", "wal"].contains(&name.as_str()) {
                continue;
            }
            let there = place.join(&name);
            if there.exists() {
                assert_eq!(std::fs::read(&there).unwrap(), std::fs::read(dest.join(&name)).unwrap(), "{name} differs between checkpoints");
            } else {
                std::fs::copy(dest.join(&name), &there).unwrap();
                copied += 1;
            }
        }
        assert!(copied > 0);
        std::fs::copy(dest.join("CURRENT"), place.join("CURRENT")).unwrap();
        // A restore: a copy of the place, opened.
        let restored = tmp.path().join(format!("restored{round}"));
        std::fs::create_dir(&restored).unwrap();
        for e in std::fs::read_dir(&place).unwrap() {
            let e = e.unwrap();
            std::fs::copy(e.path(), restored.join(e.file_name())).unwrap();
        }
        let copy = Db::open(&restored, opts(Layout::Directory)).unwrap();
        copy.storage().check().unwrap();
        assert_eq!(rows(&copy), rows(&db), "round {round}");
        previous = Some(dest);
    }
}
