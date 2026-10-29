//! Queries search their segments on threads borrowed from one pool the
//! process shares, and start none of their own, alone or many at once.

use strata::data;
use strata::db::Db;
use strata::query::exec::ExecOptions;
use strata::query::pool::Pool;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

/// How many threads the process has started so far: `ThreadId`s are
/// handed out in order, one to each thread as it starts (this one's own
/// included).
fn threads_started() -> u64 {
    let id = std::thread::spawn(|| std::thread::current().id()).join().unwrap();
    format!("{id:?}").trim_start_matches("ThreadId(").trim_end_matches(')').parse().unwrap()
}

#[test]
fn queries_borrow_the_pools_threads() {
    let docs = data::synthetic(4000, 16, 5);
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { segment_max_docs: 250, ..DbOptions::default() };
    let db = Db::create(&dir.path().join("t.strata"), data::beir_schema(16, VectorIndexKind::Hnsw { m: 12, ef_construction: 60 }), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    assert!(db.snapshot().segments().len() >= 16);
    let title = docs[3][data::col::TITLE].as_str().unwrap().to_string();
    let params = Params::new().set("q", docs[3][data::col::EMB].clone()).set("t", Value::Str(title));
    let queries = [
        "SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 10",
        "SELECT id, score() FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 10",
        "SELECT id, score() FROM docs WHERE year >= 2000 ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 10",
    ];
    let alone: Vec<Vec<Vec<Value>>> = queries.iter().map(|sql| db.query_with(sql, &params, &ExecOptions::default()).unwrap().rows).collect();
    let four = ExecOptions { threads: 4, ..ExecOptions::default() };
    // The pool starts with the first query that asks for threads.
    db.query_with(queries[0], &params, &four).unwrap();
    let before = threads_started();
    for _ in 0..50 {
        for (sql, rows) in queries.iter().zip(&alone) {
            assert_eq!(&db.query_with(sql, &params, &four).unwrap().rows, rows, "{sql}");
        }
    }
    // Only the counting thread itself started: these 150 queries on four
    // threads used to start 800 (the hybrid one searches twice).
    assert_eq!(threads_started() - before, 1);
    let plan = db.query_with(queries[0], &params, &four).unwrap().stats.plan;
    assert!(plan.iter().any(|l| l.ends_with("segments on up to 4 threads")), "{plan:?}");
    // Eight callers at once on a pool of fewer threads get the same
    // answers, and leave no helper waiting.
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..20 {
                    for (sql, rows) in queries.iter().zip(&alone) {
                        let opts = ExecOptions { threads: 8, ..ExecOptions::default() };
                        assert_eq!(&db.query_with(sql, &params, &opts).unwrap().rows, rows, "{sql}");
                    }
                }
            });
        }
    });
    assert_eq!(Pool::global().queued(), 0);
}
