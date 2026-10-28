//! The merge scheduler: merges running at once (calls on several threads,
//! the merger threads) take different segments; a merged image is written
//! before the writer lock is taken, into space reserved for it that
//! commits, `check` and `vacuum` leave alone and that a merge given up or a
//! crash gives back; writes go on meanwhile; the memory budget cuts merges;
//! segments of a merge a crash interrupted are merged in smaller groups,
//! or left alone when no group of them fits.

mod storage_util;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use storage_util::*;
use strata::db::Db;
use strata::segment::build::build_threads;
use strata::storage::merge::{merge_memory, MergeEntry, MergeRecord, SegmentInfo};
use strata::storage::{DbOptions, Layout, MergePolicy, MergerOptions};
use strata::Value;

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

fn options(layout: Layout) -> DbOptions {
    // Only the tests flush.
    DbOptions { memtable_max_docs: 1_000_000, segment_max_docs: 1_000_000, sync: false, layout, ..DbOptions::default() }
}

fn path_for(dir: &Path, layout: Layout) -> PathBuf {
    dir.join(if layout == Layout::Directory { "db.dir" } else { "db.strata" })
}

fn record_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        return path.join("MERGING");
    }
    let mut s = path.as_os_str().to_owned();
    s.push("-merging");
    s.into()
}

/// `n` segments of `per` documents each.
fn segments(path: &Path, layout: Layout, n: u64, per: u64) -> (Db, Rows) {
    let db = Db::create(path, schema(), options(layout)).unwrap();
    let mut model = Rows::new();
    for s in 0..n {
        let mut tx = db.storage().begin().unwrap();
        for i in s * per..(s + 1) * per {
            tx.insert(doc(i, i as i64)).unwrap();
            model.insert(key(i), i as i64);
        }
        tx.commit().unwrap();
        db.storage().flush().unwrap();
    }
    assert_eq!(segment_ids(&db).len() as u64, n);
    (db, model)
}

fn segment_ids(db: &Db) -> Vec<u64> {
    db.storage().file_stats().segments.iter().map(|s| s.id).collect()
}

/// Upserts, deletes and sets on keys below `keys`, applied to `model`;
/// fails on `Busy`.
fn write(db: &Db, rng: &mut StdRng, model: &mut Rows, n: usize, keys: u64) {
    for _ in 0..n {
        let mut tx = db.storage().begin().unwrap();
        for _ in 0..rng.gen_range(1..6) {
            let i = rng.gen_range(0..keys);
            let (k, v) = (key(i), rng.gen_range(0..1000));
            let kv = Value::Str(k.clone());
            match rng.gen_range(0..4) {
                0 | 1 => {
                    tx.upsert(doc(i, v)).unwrap();
                    model.insert(k, v);
                }
                2 => assert_eq!(tx.delete(&kv).unwrap(), model.remove(&k).is_some(), "delete {k}"),
                _ => {
                    assert_eq!(tx.set(&kv, "n", Value::Int(v)).unwrap(), model.contains_key(&k), "set {k}");
                    if let Some(x) = model.get_mut(&k) {
                        *x = v;
                    }
                }
            }
        }
        tx.commit().unwrap();
    }
}

/// Copies the database's files as a crash at this moment would leave them.
fn copy_files(path: &Path, to: &Path) {
    let with = |p: &Path, suffix: &str| {
        let mut s = p.as_os_str().to_owned();
        s.push(suffix);
        PathBuf::from(s)
    };
    if path.is_dir() {
        std::fs::create_dir(to).unwrap();
        for e in std::fs::read_dir(path).unwrap() {
            let e = e.unwrap();
            if e.file_name() != "LOCK" {
                std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
            }
        }
        return;
    }
    std::fs::copy(path, to).unwrap();
    for suffix in ["-wal", "-merging"] {
        if with(path, suffix).exists() {
            std::fs::copy(with(path, suffix), with(to, suffix)).unwrap();
        }
    }
}

const ALL_TIER_0: MergePolicy = MergePolicy { merge_factor: 2, floor_docs: 100_000, max_merged_docs: 2_000_000, max_deleted_ratio: 0.3 };

#[test]
fn merges_on_several_threads_run_at_once_on_different_segments() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), layout);
        let (db, model) = segments(&path, layout, 16, 40);
        // Each thread's first merge waits, image written, until the other's
        // is there too: they run at once.
        let met = Arc::new((Mutex::new(0usize), Condvar::new()));
        std::thread::scope(|s| {
            for _ in 0..2 {
                let (db, met) = (&db, met.clone());
                s.spawn(move || {
                    let mut first = true;
                    db.storage()
                        .merge_with(&ALL_TIER_0, || {
                            if std::mem::take(&mut first) {
                                let (n, cv) = &*met;
                                let mut n = n.lock().unwrap();
                                *n += 1;
                                cv.notify_all();
                                let (n, _) = cv.wait_timeout_while(n, Duration::from_secs(20), |n| *n < 2).unwrap();
                                assert_eq!(*n, 2, "the other merge did not run at the same time");
                            }
                            true
                        })
                        .unwrap();
                });
            }
        });
        // Whatever one call left to the other.
        db.storage().merge(&ALL_TIER_0).unwrap();
        assert_eq!(segment_ids(&db).len(), 1, "{layout:?}");
        assert_eq!(rows(&db), model);
        db.storage().check().unwrap();
        assert!(!record_path(&path).exists(), "the record of running merges stayed");
        assert_eq!(db.storage().merge_status().running, 0);
    }
}

#[test]
fn merges_racing_on_many_threads_take_only_segments_still_there() {
    // A merge that installs its segment and gives up its claim between
    // another one's snapshot and its claim must not leave that one its
    // inputs, gone by then ("merge input vanished").
    for round in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), Layout::SingleFile);
        let (db, model) = segments(&path, Layout::SingleFile, 32, 10);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| db.storage().merge(&ALL_TIER_0).unwrap());
            }
        });
        db.storage().merge(&ALL_TIER_0).unwrap();
        assert_eq!(segment_ids(&db).len(), 1, "round {round}");
        assert_eq!(rows(&db), model);
    }
}

#[test]
fn writes_go_on_while_a_merged_image_is_written() {
    for (seed, layout) in [(1, Layout::SingleFile), (2, Layout::Directory)] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), layout);
        let (mut db, mut model) = segments(&path, layout, 8, 50);
        let mut rng = StdRng::seed_from_u64(seed);
        let mut calls = 0;
        db.storage()
            .merge_with(&ALL_TIER_0, || {
                calls += 1;
                // Writes do not wait for the merge (they fail on Busy), and
                // a flush commits a version while the image is not
                // installed: the space reserved for it stays out of the
                // freelist, and `check` and `vacuum` leave it alone.
                write(&db, &mut rng, &mut model, 20, 400);
                if calls % 2 == 0 {
                    db.storage().flush().unwrap();
                }
                db.storage().check().unwrap();
                db.storage().vacuum().unwrap();
                db.storage().check().unwrap();
                true
            })
            .unwrap();
        assert!(calls >= 7, "{calls} merges");
        assert_eq!(rows(&db), model, "{layout:?}");
        db.storage().check().unwrap();
        drop(db);
        db = Db::open(&path, options(layout)).unwrap();
        assert_eq!(rows(&db), model, "{layout:?}, reopened");
        db.storage().check().unwrap();
    }
}

#[test]
fn a_merge_given_up_gives_its_space_back() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), layout);
        let (mut db, model) = segments(&path, layout, 4, 50);
        let before = segment_ids(&db);
        db.storage().merge_with(&ALL_TIER_0, || false).unwrap();
        assert_eq!(segment_ids(&db), before, "{layout:?}: a merge given up changed the segments");
        // No page or file is left that nothing names.
        db.storage().check().unwrap();
        db.storage().flush().unwrap();
        db.storage().check().unwrap();
        assert_eq!(db.storage().vacuum().unwrap(), 0, "{layout:?}: vacuum found something to remove");
        db.storage().merge(&ALL_TIER_0).unwrap();
        assert_eq!(segment_ids(&db).len(), 1);
        drop(db);
        db = Db::open(&path, options(layout)).unwrap();
        assert_eq!(rows(&db), model);
        db.storage().check().unwrap();
    }
}

#[test]
fn a_crash_while_a_merged_image_waits_leaves_the_database_whole() {
    for (seed, layout) in [(3, Layout::SingleFile), (4, Layout::Directory)] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), layout);
        let (db, mut model) = segments(&path, layout, 2, 50);
        let mut rng = StdRng::seed_from_u64(seed);
        let crashed = path.with_extension("crashed");
        let mut at_crash = None;
        db.storage()
            .merge_with(&ALL_TIER_0, || {
                if at_crash.is_none() {
                    // A version committed after the reservation: on disk,
                    // its pages are free.
                    write(&db, &mut rng, &mut model, 10, 200);
                    db.storage().flush().unwrap();
                    copy_files(&path, &crashed);
                    at_crash = Some(model.clone());
                }
                true
            })
            .unwrap();
        let at_crash = at_crash.unwrap();
        drop(db);
        let db = Db::open(&crashed, options(layout)).unwrap();
        assert_eq!(rows(&db), at_crash, "{layout:?}");
        db.storage().check().unwrap();
        // The merge was running: the record says it was interrupted.
        assert_eq!(db.storage().merge_status().interrupted, 1, "{layout:?}");
        let db = Db::open(&path, options(layout)).unwrap();
        assert_eq!(rows(&db), model);
        db.storage().check().unwrap();
        assert_eq!(db.storage().merge_status().interrupted, 0);
    }
}

fn infos(db: &Db, ids: &[u64]) -> Vec<SegmentInfo> {
    db.storage()
        .file_stats()
        .segments
        .iter()
        .filter(|s| ids.contains(&s.id))
        .map(|s| SegmentInfo { id: s.id, n_docs: s.n_docs, n_live: s.n_docs - s.n_deleted, bytes: s.bytes })
        .collect()
}

/// The merge memory the database estimates for segments `ids`.
fn estimate(db: &Db, ids: &[u64]) -> u64 {
    merge_memory(&infos(db, ids), build_threads(&options(Layout::SingleFile).build).unwrap())
}

#[test]
fn segments_of_an_interrupted_merge_merge_in_smaller_groups() {
    let policy = MergePolicy { merge_factor: 4, ..ALL_TIER_0 };
    let dir = tempfile::tempdir().unwrap();
    let path = path_for(dir.path(), Layout::SingleFile);
    let (db, model) = segments(&path, Layout::SingleFile, 4, 50);
    let ids = segment_ids(&db);
    let memory = estimate(&db, &ids);
    drop(db);
    // The process died merging all four: the next merges of them are
    // estimated at under half that, so two at a time.
    MergeRecord { running: vec![MergeEntry { segments: ids.clone(), memory, times: 0 }], interrupted: vec![] }.store(&record_path(&path)).unwrap();
    let db = Db::open(&path, options(Layout::SingleFile)).unwrap();
    assert_eq!(db.storage().merge_status().interrupted, 1);
    let stats = db.storage().merge(&ALL_TIER_0).unwrap();
    assert_eq!((stats.merges, stats.merges_limited), (3, 0), "pairs fit under half: {stats:?}");
    assert_eq!(segment_ids(&db).len(), 1);
    assert_eq!(rows(&db), model);
    // Their segments are gone: so is the record.
    assert_eq!(db.storage().merge_status().interrupted, 0);
    assert!(!record_path(&path).exists());

    let dir = tempfile::tempdir().unwrap();
    let path = path_for(dir.path(), Layout::SingleFile);
    let (db, model) = segments(&path, Layout::SingleFile, 4, 50);
    let ids = segment_ids(&db);
    let memory = estimate(&db, &ids);
    drop(db);
    MergeRecord { running: vec![MergeEntry { segments: ids.clone(), memory, times: 0 }], interrupted: vec![] }.store(&record_path(&path)).unwrap();
    let db = Db::open(&path, options(Layout::SingleFile)).unwrap();
    // The four are cut to two; then the tier holds three, short of four.
    let stats = db.storage().merge(&policy).unwrap();
    assert_eq!((stats.merges, stats.merges_limited), (1, 1), "{stats:?}");
    assert_eq!(segment_ids(&db).len(), 3);
    assert_eq!(rows(&db), model);
    db.storage().check().unwrap();
}

#[test]
fn segments_no_group_of_which_fits_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_for(dir.path(), Layout::Directory);
    let (db, model) = segments(&path, Layout::Directory, 6, 50);
    let ids = segment_ids(&db);
    drop(db);
    // Interrupted twice, estimated at a few bytes: nothing fits.
    let r = MergeRecord { running: vec![], interrupted: vec![MergeEntry { segments: ids[..2].to_vec(), memory: 8, times: 2 }] };
    r.store(&record_path(&path)).unwrap();
    let db = Db::open(&path, options(Layout::Directory)).unwrap();
    let stats = db.storage().merge(&ALL_TIER_0).unwrap();
    let left = segment_ids(&db);
    assert!(stats.merges >= 3, "{stats:?}");
    assert!(left.contains(&ids[0]) && left.contains(&ids[1]), "{left:?}");
    assert_eq!(left.len(), 3, "the other four become one: {left:?}");
    assert_eq!(rows(&db), model);
    assert_eq!(db.storage().merge_status().interrupted, 1);
    // Kept across a reopen.
    drop(db);
    let db = Db::open(&path, options(Layout::Directory)).unwrap();
    db.storage().merge(&ALL_TIER_0).unwrap();
    assert!(segment_ids(&db).contains(&ids[0]));
    assert_eq!(MergeRecord::load(&record_path(&path)), r);
}

/// Waits until `done` holds, for up to a minute.
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !done() {
        assert!(t0.elapsed() < Duration::from_secs(60), "waited a minute for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_merger_cuts_merges_to_its_memory_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = path_for(dir.path(), Layout::SingleFile);
    let (db, model) = segments(&path, Layout::SingleFile, 8, 50);
    let ids = segment_ids(&db);
    let three = estimate(&db, &ids[..3]);
    let db = Arc::new(db);
    let policy = MergePolicy { merge_factor: 8, ..ALL_TIER_0 };
    db.spawn_merger(MergerOptions { policy, threads: 1, memory_budget: Some(three + three / 10), write_bytes_per_sec: None });
    // Three of the eight merge: then the tier holds six, short of eight.
    wait_for("a merge", || segment_ids(&db).len() == 6 && db.storage().merge_status().running == 0);
    let status = db.storage().merge_status();
    assert_eq!((status.merges, status.merges_limited), (1, 1), "{status:?}");
    let sizes: Vec<u32> = db.storage().file_stats().segments.iter().map(|s| s.n_docs).collect();
    assert_eq!(sizes.iter().max(), Some(&150), "{sizes:?}");
    assert_eq!(rows(&db), model);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(segment_ids(&db).len(), 6);
}

#[test]
fn the_merger_keeps_up_with_the_flusher() {
    for layout in [Layout::SingleFile, Layout::Directory] {
        let dir = tempfile::tempdir().unwrap();
        let path = path_for(dir.path(), layout);
        let o = DbOptions { memtable_max_docs: 100, segment_max_docs: 100, ..options(layout) };
        let db = Arc::new(Db::create(&path, schema(), o).unwrap());
        db.spawn_flusher();
        let policy = MergePolicy { merge_factor: 4, floor_docs: 100, ..MergePolicy::default() };
        db.spawn_merger(MergerOptions { policy, threads: 2, memory_budget: None, write_bytes_per_sec: Some(64 << 20) });
        let mut rng = StdRng::seed_from_u64(7);
        let mut model = Rows::new();
        for _ in 0..300 {
            // The flusher may hold the lock to seal or install: retry.
            loop {
                let mut local = model.clone();
                let r = (|| {
                    let mut tx = db.storage().begin()?;
                    for _ in 0..10 {
                        let i = rng.gen_range(0..2000);
                        let v = rng.gen_range(0..1000);
                        tx.upsert(doc(i, v))?;
                        local.insert(key(i), v);
                    }
                    tx.commit()
                })();
                match r {
                    Ok(()) => {
                        model = local;
                        break;
                    }
                    Err(strata::Error::Busy(_)) => std::thread::yield_now(),
                    Err(e) => panic!("{e}"),
                }
            }
        }
        // Tiers of 100, 400, 1,600 documents: few segments once merges
        // catch up.
        wait_for("merges to catch up", || {
            let s = db.storage().merge_status();
            s.running == 0 && segment_ids(&db).len() <= 12
        });
        let status = db.storage().merge_status();
        assert!(status.merges > 0 && status.error.is_none(), "{status:?}");
        assert_eq!(rows(&db), model, "{layout:?}");
        db.storage().check().unwrap();
    }
}
