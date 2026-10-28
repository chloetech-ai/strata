//! Power losses, full disks and failing syncs, simulated by `storage::io`
//! (feature `fault-injection`): a workload of transactions, flushes (also
//! in the background, with a transaction committing while one builds),
//! merges, vacuums and bulk loads runs with the `n`-th change to its files
//! failing, for every `n`, and the database is reopened from what the
//! failure left.
//!
//! * Power loss: the `n`-th call never happens, and the files are what a
//!   power loss at that moment could leave (unsynced writes kept or lost
//!   sector by sector). With `sync`, the reopened database holds every
//!   transaction acknowledged before the loss, and at most the one in
//!   flight beyond them; without it, the transactions since the last
//!   flush (or bulk load, which flushes first) may be lost, newest first.
//! * Failed calls: the `n`-th call fails (`ENOSPC`, or `EIO`; a failed
//!   sync loses the unsynced writes, as Linux does), once or from then
//!   on. The workload goes on; operations that fail must have changed
//!   nothing, except the one that left the database refusing writes
//!   ("poisoned"), which may or may not have reached the disk.
#![cfg(feature = "fault-injection")]

mod storage_util;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use storage_util::*;
use strata::storage::io::{self, Fault};
use strata::storage::{wal, Database, DbOptions, Layout, MergePolicy};
use strata::Value;

type Keys = BTreeSet<u64>;

fn wal_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push("-wal");
    s.into()
}

fn id_of(k: &str) -> u64 {
    k[1..].parse().unwrap()
}

fn keys_of(db: &Database) -> Keys {
    let got = live_keys(&db.snapshot());
    assert!(got.values().all(|&c| c == 1), "a key is live twice");
    got.keys().map(|k| id_of(k)).collect()
}

/// What a workload did: the keys after each operation that succeeded
/// (`states[0]` before any), and for each that failed, the keys had it
/// succeeded, with whether it left the database poisoned.
#[derive(Default)]
struct Run {
    states: Vec<Keys>,
    failed: Vec<(usize, Keys, bool)>,
    /// Without `sync`, the states a power loss cannot go back past, by
    /// index into `states`: after each flush, and before each bulk load
    /// (which flushes first, but whose own version may be lost).
    flushes: Vec<usize>,
    /// Without `stop`: whether a flush after the last operation succeeded,
    /// and whether the database was poisoned then.
    last_flush: Option<(bool, bool)>,
}

fn options(src: &Arc<MapSource>, sync: bool) -> DbOptions {
    DbOptions { sync, memtable_max_docs: 60, segment_max_docs: 150, ..opts(src) }
}

/// Runs `ops` seeded operations on a new database at `path` (in the
/// directory layout when it ends in `.dir`); with `stop`,
/// stops at the first that fails. Recording starts after the database is
/// created (its creation is synced).
fn workload(path: &Path, src: &Arc<MapSource>, sync: bool, seed: u64, ops: usize, plan: Option<Fault>, stop: bool) -> Run {
    let layout = if path.extension().is_some_and(|e| e == "dir") { Layout::Directory } else { Layout::SingleFile };
    let db = Database::create(path, schema(), DbOptions { layout, ..options(src, sync) }).unwrap();
    io::arm(seed);
    if let Some(f) = plan {
        io::fail_at(f);
    }
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let policy = MergePolicy { merge_factor: 3, floor_docs: 20, max_deleted_ratio: 0.3, ..MergePolicy::default() };
    let mut live = Keys::new();
    let mut run = Run { states: vec![live.clone()], ..Run::default() };
    let mut fresh = 10_000;
    for n in 0..ops {
        let mut next = live.clone();
        // How far back a power loss may go without `sync`: to this op's
        // state (0), the one before it (1), or further (None).
        let (res, floor) = match rng.gen_range(0..100) {
            0..=69 => (transaction(&db, &mut rng, src, &mut next, n), None),
            70..=74 => (db.flush(), Some(0)),
            75..=79 => {
                // The transaction stands on its own: it commits or fails
                // whatever the flush around it does.
                let (mut inner, mut with) = (None, live.clone());
                let res = db.background_flush_with(|| inner = Some(transaction(&db, &mut rng, src, &mut with, n)));
                match inner {
                    Some(Ok(())) => {
                        live = with;
                        run.states.push(live.clone());
                    }
                    Some(Err(_)) => {
                        run.failed.push((run.states.len() - 1, with, db.is_poisoned()));
                        if stop {
                            break;
                        }
                    }
                    None => {}
                }
                next = live.clone();
                (res, Some(0))
            }
            80..=87 => (db.merge(&policy).map(|_| ()), None),
            88..=91 => (db.vacuum().map(|_| ()), None),
            _ => {
                let ids: Vec<u64> = (fresh..fresh + rng.gen_range(1..40)).collect();
                fresh += 1000;
                let docs: Vec<_> = ids.iter().map(|&i| doc(i, 0)).collect();
                docs.iter().for_each(|d| src.put(d));
                next.extend(ids);
                (db.bulk_load(docs), Some(1))
            }
        };
        match res {
            Ok(()) => {
                live = next;
                run.states.push(live.clone());
                if let Some(back) = floor {
                    run.flushes.push(run.states.len() - 1 - back);
                }
            }
            Err(_) => {
                run.failed.push((run.states.len() - 1, next, db.is_poisoned()));
                if stop {
                    break;
                }
            }
        }
    }
    if !stop {
        run.last_flush = Some((db.flush().is_ok(), db.is_poisoned()));
    }
    run
}

/// A transaction of upserts and deletes of keys below 300, applied to
/// `next` as it goes.
fn transaction(db: &Database, rng: &mut StdRng, src: &MapSource, next: &mut Keys, n: usize) -> strata::Result<()> {
    let mut tx = db.begin()?;
    for _ in 0..rng.gen_range(1..12) {
        let i = rng.gen_range(0..300u64);
        if next.contains(&i) && rng.gen_bool(0.4) {
            tx.delete(&Value::Str(key(i)))?;
            next.remove(&i);
        } else {
            let d = doc(i, n as i64);
            src.put(&d);
            tx.upsert(d)?;
            next.insert(i);
        }
    }
    tx.commit()
}

/// The calls a workload makes.
fn calls_of(name: &str, sync: bool, seed: u64, ops: usize) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let src = Arc::new(MapSource::default());
    workload(&dir.path().join(name), &src, sync, seed, ops, None, true);
    let n = io::calls();
    io::crash();
    n
}

/// The failure points to try: all of them, or (`STRATA_FAULT_STRIDE`)
/// every so many.
fn points(calls: u64) -> impl Iterator<Item = u64> {
    let stride = std::env::var("STRATA_FAULT_STRIDE").map_or(1, |v| v.parse().unwrap());
    (1..=calls).step_by(stride)
}

fn power_loss(sync: bool, seed: u64, ops: usize) {
    let calls = calls_of("db.strata", sync, seed, ops);
    assert!(calls > 100, "{calls} calls");
    for at in points(calls) {
        let dir = tempfile::tempdir().unwrap();
        let (path, copy) = (dir.path().join("db.strata"), dir.path().join("copy.strata"));
        let src = Arc::new(MapSource::default());
        let fault = Fault { at, errno: libc::EIO, sticky: true, crash: true };
        let run = workload(&path, &src, sync, seed, ops, Some(fault), true);
        let mut crash = io::crash();
        // The states the database may come back in.
        let last = run.states.len() - 1;
        let lost = if sync { last } else { run.flushes.last().copied().unwrap_or(0) };
        let mut allowed: Vec<&Keys> = run.states[lost..].iter().collect();
        allowed.extend(run.failed.iter().map(|(_, k, _)| k));
        for image in 0..3 {
            crash.copy(&path, &copy).unwrap();
            crash.copy(&wal_of(&path), &wal_of(&copy)).unwrap();
            for (_, p) in wal::sealed_files(&wal_of(&copy)).unwrap() {
                std::fs::remove_file(p).unwrap();
            }
            for (t, p) in wal::sealed_files(&wal_of(&path)).unwrap() {
                crash.copy(&p, &wal::sealed_path(&wal_of(&copy), t)).unwrap();
            }
            let what = format!("sync {sync}, power lost at call {at} of {calls}, image {image}");
            let db = Database::open(&copy, options(&src, sync)).unwrap_or_else(|e| panic!("{what}: {e}"));
            db.check().unwrap_or_else(|e| panic!("{what}: {e}"));
            let got = keys_of(&db);
            assert!(allowed.contains(&&got), "{what}: {} keys, not a state the workload may come back in ({})", got.len(), nearest(&run, &got));
            drop(db);
            let _ = std::fs::remove_file(strata_damaged(&copy));
        }
    }
}

/// Which state of the run `got` is closest to, for a failure message.
fn nearest(run: &Run, got: &Keys) -> String {
    let (i, d) = run.states.iter().enumerate().map(|(i, s)| (i, s.symmetric_difference(got).count())).min_by_key(|&(_, d)| d).unwrap();
    format!("{d} keys from the state after operation {i} of {}; flushes at {:?}", run.states.len() - 1, run.flushes)
}

fn strata_damaged(copy: &Path) -> PathBuf {
    let mut s = wal_of(copy).into_os_string();
    s.push(".damaged");
    s.into()
}

/// The workloads' seeds: `STRATA_FAULT_SEEDS` more than the default.
fn seeds(first: u64, default: u64) -> impl Iterator<Item = u64> {
    let n = std::env::var("STRATA_FAULT_SEEDS").map_or(default, |v| v.parse().unwrap());
    (0..n).map(move |i| first + 100 * i)
}

fn ops() -> usize {
    std::env::var("STRATA_FAULT_OPS").map_or(80, |v| v.parse().unwrap())
}

#[test]
fn power_loss_with_sync_keeps_every_acknowledged_commit() {
    seeds(1, 2).for_each(|s| power_loss(true, s, ops()));
}

#[test]
fn power_loss_without_sync_loses_only_the_newest_commits() {
    seeds(2, 2).for_each(|s| power_loss(false, s, ops()));
}

fn failing_calls(name: &str, errno: i32, sticky: bool, seed: u64, ops: usize) {
    let calls = calls_of(name, true, seed, ops);
    for at in points(calls) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let src = Arc::new(MapSource::default());
        let fault = Fault { at, errno, sticky, crash: false };
        let run = workload(&path, &src, true, seed, ops, Some(fault), false);
        io::crash();
        let what = format!("{name}, errno {errno}, sticky {sticky}, call {at} of {calls}");
        let db = Database::open(&path, options(&src, true)).unwrap_or_else(|e| panic!("{what}: {e}"));
        db.check().unwrap_or_else(|e| panic!("{what}: {e}"));
        let got = keys_of(&db);
        let model = run.states.last().unwrap();
        let poisoned: Vec<&Keys> = run.failed.iter().filter(|f| f.2).map(|f| &f.1).take(1).collect();
        // A failure that passed leaves the database able to write again.
        if let (false, Some((flushed, false))) = (sticky, run.last_flush) {
            assert!(flushed, "{what}: a flush after the failure still fails");
        }
        assert!(&got == model || poisoned.contains(&&got), "{what}: {} keys, expected {} ({} failed operations)", got.len(), model.len(), run.failed.len());
    }
}

#[test]
fn a_failed_call_changes_nothing_but_its_operation() {
    seeds(3, 2).for_each(|s| failing_calls("db.strata", libc::EIO, false, s, ops()));
}

#[test]
fn a_full_disk_keeps_what_was_committed() {
    seeds(4, 2).for_each(|s| failing_calls("db.strata", libc::ENOSPC, true, s, ops()));
}

// A commit that failed partway left files that failed every later commit
// in this layout (EEXIST), until a reopen.
#[test]
fn in_the_directory_layout_a_failed_call_changes_nothing_but_its_operation() {
    seeds(5, 1).for_each(|s| failing_calls("db.dir", libc::EIO, false, s, ops()));
}

#[test]
fn in_the_directory_layout_a_full_disk_keeps_what_was_committed() {
    seeds(6, 1).for_each(|s| failing_calls("db.dir", libc::ENOSPC, true, s, ops()));
}
