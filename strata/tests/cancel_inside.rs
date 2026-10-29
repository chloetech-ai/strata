//! A query stops soon after its deadline passes or it is cancelled, even
//! inside one segment: the aggregation, ordering, graph walk and join loops
//! check every so many rows, not only between segments.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use strata::db::Db;
use strata::error::Error;
use strata::graph::{edges_value, Edge};
use strata::query::exec::ExecOptions;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::{Field, FieldType, Schema};

/// One segment of `n` rows: an integer key, `a` (1,000 values), `b` (500
/// values), `c`, and `next`, an edge to the following row.
fn db(dir: &std::path::Path, n: i64) -> Db {
    let schema = Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Int),
            Field::new("a", FieldType::Int),
            Field::new("b", FieldType::Int),
            Field::new("c", FieldType::Int),
            Field::new("next", FieldType::Edges).not_stored(),
        ],
        Some("id"),
    )
    .unwrap();
    let opts = DbOptions { segment_max_docs: 1 << 30, sync: false, ..DbOptions::default() };
    let db = Db::create(&dir.join("c.strata"), schema, opts).unwrap();
    db.load((0..n).map(|i| {
        let next = edges_value(&[Edge::to(Value::Int(i + 1))]);
        vec![Value::Int(i), Value::Int(i * 7919 % 1000), Value::Int(i * 104_729 % 500), Value::Int(i % 3), next]
    }))
    .unwrap();
    assert_eq!(db.snapshot().segments().len(), 1, "the test needs every row in one segment");
    db
}

/// `sql` stops with a timeout, well before it would have finished, when
/// its deadline passes a tenth of the way in, and when another thread
/// cancels it then.
fn stops_midway(db: &Db, sql: &str) {
    let run = |opts: &ExecOptions| {
        let t0 = Instant::now();
        let r = db.query_with(sql, &Params::new(), opts);
        (r.map(|r| r.rows.len()), t0.elapsed())
    };
    // The faster of two full runs (the first may still be reading pages).
    let full = (0..2)
        .map(|_| {
            let (r, took) = run(&ExecOptions::default());
            r.unwrap();
            took
        })
        .min()
        .unwrap();
    assert!(full >= Duration::from_millis(25), "{sql} took {full:?}: too short to stop in the middle");
    let (r, took) = run(&ExecOptions { deadline: Some(Instant::now() + full / 10), ..ExecOptions::default() });
    assert!(matches!(r, Err(Error::Timeout(ref m)) if m.contains("deadline")), "{sql}: {r:?}");
    assert!(took < full / 2, "{sql}: stopped after {took:?} of {full:?}");
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(full / 10);
        flag.store(true, Ordering::Relaxed);
    });
    let (r, took) = run(&ExecOptions { cancel: Some(cancel), ..ExecOptions::default() });
    canceller.join().unwrap();
    assert!(matches!(r, Err(Error::Timeout(ref m)) if m.contains("cancelled")), "{sql}: {r:?}");
    assert!(took < full / 2, "{sql}: stopped after {took:?} of {full:?}");
}

#[test]
fn aggregation_and_ordering_stop_inside_a_segment() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path(), 3_000_000);
    // Grouped document by document, 500,000 groups.
    let grouped = "SELECT a, b, COUNT(*), SUM(c) FROM t GROUP BY a, b";
    stops_midway(&db, grouped);
    // Ordered by a column over every row.
    let ordered = "SELECT id FROM t WHERE c >= 0 ORDER BY b DESC LIMIT 10";
    stops_midway(&db, ordered);
}

#[test]
fn graph_walks_stop_inside_a_segment() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path(), 1_000_000);
    // A chain: each hop reaches one more row.
    let walk = "SELECT COUNT(*) FROM t WHERE REACHABLE(next, 0, hops => 1000000, max_nodes => 1000000)";
    stops_midway(&db, walk);
}

#[test]
fn joins_stop_inside_a_segment() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path(), 2_400);
    // Every row meets every row of its `c` (800 each), and none passes
    // the rest of the join condition.
    let join = "SELECT x.id, y.id FROM t x JOIN t y ON x.c = y.c AND x.a < y.a - 2000";
    stops_midway(&db, join);
}
