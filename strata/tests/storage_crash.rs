//! Crash recovery: a child process (this test binary, re-executed) runs
//! random transactions, flushes, merges and bulk loads, and is killed with
//! SIGKILL at a random moment. The reopened database must hold exactly the
//! state after some prefix of the child's transactions that includes every
//! transaction whose commit returned.
//!
//! The child logs each transaction to a side file before committing it
//! (`B`) and after the commit returns (`C`); file writes survive SIGKILL,
//! so the log brackets the recovered state.
//!
//! The same runs on a table with `cluster_by` and `retain`, whose flushes
//! and merges reorder rows and drop expired ones: a tenth of the keys get
//! a time a day past the retention, and may be there or not.
//!
//! A child that only creates databases, one after another, is killed too:
//! each path must then hold a whole database or nothing.

mod storage_util;

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use rand::{Rng, SeedableRng};
use storage_util::*;
use strata::schema::{Field, Schema};
use strata::storage::{Database, DbOptions, MergePolicy};
use strata::value::Document;
use strata::Value;

const CHILD_ENV: &str = "STRATA_CRASH_CHILD";
const CREATE_ENV: &str = "STRATA_CREATE_CHILD";

fn id_of(k: &str) -> u64 {
    k[1..].parse().unwrap()
}

const DAY: i64 = 86_400_000_000;

/// [`schema`] with a time, its rows ordered by `n` and kept for a day.
fn clustered() -> Schema {
    let s = schema();
    let mut fields = s.fields.clone();
    fields.push(Field::timestamp("ts"));
    Schema::new("t", fields, Some("id")).unwrap().with_cluster_by("n").unwrap().with_retain("ts", 86_400).unwrap()
}

/// Keys whose rows in the clustered table are past its retention.
fn expires(i: u64) -> bool {
    i.is_multiple_of(10)
}

/// [`opts`], whose merges rebuild rows from the doc store stand-in, or
/// for the clustered table from the segments' stored rows: the stand-in
/// gives a key's latest row, not the one a segment holds, and the order a
/// merge writes is checked.
fn source(clustered: bool, src: &Arc<MapSource>) -> DbOptions {
    match clustered {
        true => DbOptions { doc_source: Some(Arc::new(strata::db::StoredSource)), ..opts(src) },
        false => opts(src),
    }
}

/// [`doc`], with a time when the table is the clustered one (as stored,
/// an integer).
fn doc_of(clustered: bool, i: u64, v: i64) -> Document {
    let mut d = doc(i, v);
    if clustered {
        let now = strata::timestamp::now();
        d.push(Value::Int(if expires(i) { now - 2 * DAY } else { now }));
    }
    d
}

/// The child: runs until killed. Not a real test unless `CHILD_ENV` is set.
#[test]
fn crash_child() {
    let Ok(spec) = std::env::var(CHILD_ENV) else { return };
    let parts: Vec<&str> = spec.split('|').collect();
    let (db_path, log_path) = (Path::new(parts[0]), Path::new(parts[1]));
    let seed: u64 = parts[2].parse().unwrap();
    let clustered = parts[3] == "clustered";
    let doc = |i, v| doc_of(clustered, i, v);
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let src = Arc::new(MapSource::default());
    let o = DbOptions {
        sync: seed.is_multiple_of(2),
        memtable_max_docs: rng.gen_range(20..300),
        memtable_chunk_docs: rng.gen_range(1..200),
        segment_max_docs: rng.gen_range(50..400),
        ..source(clustered, &src)
    };
    let db = match clustered && !db_path.exists() {
        true => Database::create(db_path, self::clustered(), o).unwrap(),
        false => open_or_create(db_path, o),
    };
    // Keys whose rows expire are written with upserts only: a flush or a
    // merge may drop them at any time.
    let mut live: BTreeSet<u64> = live_keys(&db.snapshot()).keys().map(|k| id_of(k)).filter(|&i| !(clustered && expires(i))).collect();
    for &i in &live {
        src.put(&doc(i, 0));
    }
    let mut log = OpenOptions::new().create(true).append(true).open(log_path).unwrap();
    let policy = MergePolicy { merge_factor: 3, floor_docs: 50, max_deleted_ratio: 0.3, ..MergePolicy::default() };
    let mut fresh = 1_000_000 * (seed + 1);
    for n in 1u64.. {
        match rng.gen_range(0..100) {
            0..=74 => {
                let mut ops = Vec::new();
                let mut tx = db.begin().unwrap();
                for _ in 0..rng.gen_range(1..30) {
                    let i = rng.gen_range(0..1500u64);
                    let d = doc(i, n as i64);
                    if clustered && expires(i) {
                        src.put(&d);
                        tx.upsert(d).unwrap();
                        ops.push(format!("u{i}"));
                    } else if live.contains(&i) && rng.gen_bool(0.5) {
                        assert!(tx.delete(&Value::Str(key(i))).unwrap());
                        live.remove(&i);
                        ops.push(format!("d{i}"));
                    } else if live.contains(&i) || rng.gen_bool(0.5) {
                        src.put(&d);
                        tx.upsert(d).unwrap();
                        live.insert(i);
                        ops.push(format!("u{i}"));
                    } else {
                        src.put(&d);
                        tx.insert(d).unwrap();
                        live.insert(i);
                        ops.push(format!("i{i}"));
                    }
                }
                note(&mut log, format!("B {n} {}\n", ops.join(",")));
                tx.commit().unwrap();
                note(&mut log, format!("C {n}\n"));
            }
            75..=84 => db.flush().unwrap(),
            85..=91 => {
                db.merge(&policy).unwrap();
            }
            92..=94 => {
                db.vacuum().unwrap();
            }
            _ => {
                let ids: Vec<u64> = (fresh..fresh + rng.gen_range(1..30)).collect();
                fresh += 1000;
                let docs: Vec<_> = ids.iter().map(|&i| doc(i, 0)).collect();
                docs.iter().for_each(|d| src.put(d));
                let ops: Vec<String> = ids.iter().map(|i| format!("i{i}")).collect();
                note(&mut log, format!("B {n} {}\n", ops.join(",")));
                db.bulk_load(docs).unwrap();
                live.extend(ids.into_iter().filter(|&i| !(clustered && expires(i))));
                note(&mut log, format!("C {n}\n"));
            }
        }
    }
}

/// Appends a line with one `write` (a kill cannot split it).
fn note(log: &mut std::fs::File, line: String) {
    log.write_all(line.as_bytes()).unwrap();
}

/// A parsed log line: (is a `B` line, transaction number, operations).
type LogLine = (bool, u64, Vec<(char, u64)>);

/// A log line: `B n ops` or `C n`; `None` for a line torn by the kill.
fn parse_line(line: &str) -> Option<LogLine> {
    let mut it = line.splitn(3, ' ');
    let (tag, n) = (it.next()?, it.next()?.parse().ok()?);
    match (tag, it.next()) {
        ("B", Some(ops)) => {
            let ops = ops.split(',').map(|o| Some((o.chars().next()?, o.get(1..)?.parse().ok()?))).collect::<Option<_>>()?;
            Some((true, n, ops))
        }
        ("C", None) => Some((false, n, Vec::new())),
        _ => None,
    }
}

/// States the database may recover to: after each transaction from the
/// last acknowledged one to the last attempted one.
fn candidates(start: &BTreeSet<u64>, log: &str) -> Vec<BTreeSet<u64>> {
    let mut acked = 0u64;
    let mut txns: Vec<(u64, Vec<(char, u64)>)> = Vec::new();
    for (begin, n, ops) in log.lines().filter_map(parse_line) {
        if begin {
            txns.push((n, ops));
        } else {
            acked = n;
        }
    }
    let mut state = start.clone();
    let mut out = Vec::new();
    if txns.first().is_none_or(|(n, _)| *n > acked) {
        out.push(state.clone());
    }
    for (n, ops) in txns {
        for (op, i) in ops {
            match op {
                'd' => state.remove(&i),
                _ => state.insert(i),
            };
        }
        if n >= acked {
            out.push(state.clone());
        }
    }
    out
}

#[test]
fn killed_writer_recovers_a_committed_prefix() {
    killed_writer_recovers("db.strata", false);
}

#[test]
fn killed_writer_recovers_a_committed_prefix_in_directory_layout() {
    killed_writer_recovers("db.dir", false);
}

#[test]
fn killed_writer_of_a_clustered_table_recovers_a_committed_prefix() {
    killed_writer_recovers("db.strata", true);
}

fn killed_writer_recovers(name: &str, clustered: bool) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join(name);
    let exe = std::env::current_exe().unwrap();
    let mut rng = rand::rngs::StdRng::seed_from_u64(99);
    let mut state: BTreeSet<u64> = BTreeSet::new();
    let src = Arc::new(MapSource::default());
    let iterations = std::env::var("STRATA_CRASH_ITERS").map_or(20, |v| v.parse().unwrap());
    for iter in 0..iterations {
        let log_path = dir.path().join(format!("log{iter}"));
        let spec = format!("{}|{}|{iter}|{}", db_path.display(), log_path.display(), if clustered { "clustered" } else { "plain" });
        let mut child = Command::new(&exe)
            .args(["crash_child", "--exact", "--test-threads=1"])
            .env(CHILD_ENV, spec)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(rng.gen_range(40..400)));
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited on its own: {status}");
        }
        child.kill().unwrap();
        child.wait().unwrap();

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if !db_path.exists() {
            // Killed before its database was created: nothing committed.
            assert!(iter == 0 && !log.contains("C "), "iteration {iter}: the database is gone");
            continue;
        }
        // Keys that expire may be there or not.
        let kept = |s: &BTreeSet<u64>| s.iter().copied().filter(|&i| !(clustered && expires(i))).collect::<BTreeSet<u64>>();
        let cands: Vec<BTreeSet<u64>> = candidates(&state, &log).iter().map(kept).collect();
        let db = Database::open(&db_path, source(clustered, &src)).unwrap();
        let snap = db.snapshot();
        let got = live_keys(&snap);
        assert!(got.values().all(|&c| c == 1), "iteration {iter}: a key is live twice");
        let got: BTreeSet<u64> = got.keys().map(|k| id_of(k)).collect();
        if clustered {
            // Every segment on disk holds its rows in order of n.
            for v in snap.segments().iter().filter(|v| !v.in_memory) {
                let n = strata::forward::NumericColumn::open(&v.file, "n").unwrap();
                let ns: Vec<Option<i64>> = (0..v.file.n_docs() as u32).map(|d| n.get_i64(d)).collect();
                assert!(ns.is_sorted(), "iteration {iter}: segment {} is not in order of n", v.id);
            }
        }
        let pos = cands.iter().position(|c| *c == kept(&got));
        let txns = log.lines().filter(|l| l.starts_with("C ")).count();
        assert!(pos.is_some(), "iteration {iter}: recovered {} keys, not a committed prefix ({} candidates)", got.len(), cands.len());
        assert_eq!(snap.n_live_docs(), got.len() as u64);
        drop(snap);
        db.check().unwrap();
        eprintln!("iteration {iter}: {txns} commits acknowledged, recovered {} keys", got.len());
        state = kept(&got);
    }
}

/// A database's name in the layout `dir` names, as [`open_or_create`]
/// reads it.
fn db_name(n: u64, dir: bool) -> String {
    format!("db-{n}.{}", if dir { "dir" } else { "strata" })
}

/// The creating child: creates `db-0`, `db-1`, ... in a folder until it is
/// killed. Not a real test unless `CREATE_ENV` is set.
#[test]
fn create_child() {
    let Ok(spec) = std::env::var(CREATE_ENV) else { return };
    let (folder, dir) = spec.split_once('|').unwrap();
    let layout = if dir == "dir" { strata::storage::Layout::Directory } else { strata::storage::Layout::SingleFile };
    let src = Arc::new(MapSource::default());
    for n in 0.. {
        let path = Path::new(folder).join(db_name(n, layout == strata::storage::Layout::Directory));
        drop(Database::create(&path, schema(), DbOptions { layout, sync: true, ..opts(&src) }).unwrap());
    }
}

/// A create a kill interrupts leaves a database or nothing at its path:
/// every path there opens, and the one it was creating can be created.
#[test]
fn killed_create_leaves_a_database_or_nothing() {
    for dir in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let src = Arc::new(MapSource::default());
        for iter in 0..20 {
            let folder = tmp.path().join(format!("iter{iter}"));
            std::fs::create_dir(&folder).unwrap();
            let spec = format!("{}|{}", folder.display(), if dir { "dir" } else { "file" });
            let mut child = Command::new(&exe).args(["create_child", "--exact", "--test-threads=1"]).env(CREATE_ENV, spec).stdout(Stdio::null()).spawn().unwrap();
            std::thread::sleep(Duration::from_millis(rng.gen_range(2..40)));
            child.kill().unwrap();
            child.wait().unwrap();
            let mut n = 0;
            while folder.join(db_name(n, dir)).exists() {
                let db = Database::open(&folder.join(db_name(n, dir)), opts(&src)).unwrap_or_else(|e| panic!("iteration {iter}: {} does not open: {e}", db_name(n, dir)));
                assert_eq!(db.snapshot().n_live_docs(), 0);
                db.check().unwrap();
                n += 1;
            }
            assert!(!folder.join(db_name(n + 1, dir)).exists(), "iteration {iter}: {} is there without {}", db_name(n + 1, dir), db_name(n, dir));
            let layout = if dir { strata::storage::Layout::Directory } else { strata::storage::Layout::SingleFile };
            let db = Database::create(&folder.join(db_name(n, dir)), schema(), DbOptions { layout, ..opts(&src) }).unwrap_or_else(|e| panic!("iteration {iter}: {} cannot be created: {e}", db_name(n, dir)));
            db.check().unwrap();
        }
    }
}

#[test]
fn candidate_states_from_log() {
    let start: BTreeSet<u64> = [1, 2].into();
    let log = "B 1 i3,d1\nC 1\nB 2 d2\nC 2\nB 3 u9\n";
    let c = candidates(&start, log);
    assert_eq!(c, vec![[3].into(), [3, 9].into()]);
    assert_eq!(candidates(&start, "B 1 d1\n"), vec![start.clone(), [2].into()]);
    assert_eq!(candidates(&start, "B 1 d1\nB 2 x"), vec![start.clone(), [2].into()]);
}
