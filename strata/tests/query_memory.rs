//! The memory breaker: a query whose groups, candidates, rows, graph walk
//! or join need more than the budget has left fails with `Error::Memory`,
//! and every query gives back what it held, failed or not.

use std::sync::Arc;

use strata::db::Db;
use strata::error::Error;
use strata::graph::{edges_value, Edge};
use strata::query::exec::ExecOptions;
use strata::query::memory::MemoryBudget;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::{Field, FieldType, Schema};

const N: i64 = 200_000;

/// `N` rows: an integer key, `a` with a value per row, `b`, `next` (an
/// edge to the following row), and an 8 KB `note` on the first 1,000.
fn db(dir: &std::path::Path) -> Db {
    let schema = Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Int),
            Field::new("a", FieldType::Int),
            Field::new("b", FieldType::Int),
            Field::new("next", FieldType::Edges).not_stored(),
            Field::new("note", FieldType::Text { positions: false }).stored_only(),
        ],
        Some("id"),
    )
    .unwrap();
    let db = Db::create(&dir.join("m.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load((0..N).map(|i| {
        let note = if i < 1000 { Value::Str(format!("{i:08} ").repeat(900)) } else { Value::Null };
        vec![Value::Int(i), Value::Int(i * 7919 % N), Value::Int(i % 1000), edges_value(&[Edge::to(Value::Int(i + 1))]), note]
    }))
    .unwrap();
    db
}

/// Each needs about 6 MiB or more, counted.
const HEAVY: [&str; 7] = [
    // Groups.
    "SELECT a, COUNT(*) FROM t GROUP BY a LIMIT 5",
    // COUNT(DISTINCT)'s values.
    "SELECT COUNT(DISTINCT a) FROM t",
    // Candidates ordered by a column, as many as LIMIT plus OFFSET.
    "SELECT id FROM t WHERE b >= 0 ORDER BY b DESC LIMIT 10 OFFSET 150000",
    // Rows returned.
    "SELECT id, note FROM t WHERE id < 1000 LIMIT 1000",
    // A graph walk's nodes.
    "SELECT COUNT(*) FROM t WHERE REACHABLE(next, 0, hops => 1000000, max_nodes => 1000000)",
    // A compound query's steps and join.
    "SELECT COUNT(*) FROM t x JOIN t y ON x.a = y.id",
    "SELECT id FROM t WHERE a IN (SELECT id FROM t WHERE b > 10) LIMIT 5",
];

fn run(db: &Db, sql: &str, opts: &ExecOptions) -> Result<usize, Error> {
    db.query_with(sql, &Params::new(), opts).map(|r| r.rows.len())
}

#[test]
fn queries_past_the_budget_fail_and_give_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let small = MemoryBudget::new(4 << 20);
    let large = MemoryBudget::new(1 << 30);
    for (i, sql) in HEAVY.iter().enumerate() {
        let free = run(&db, sql, &ExecOptions::default()).unwrap_or_else(|e| panic!("{sql}: {e}"));
        // Under a large budget, the same answer, and nothing held after.
        let opts = ExecOptions { memory: Some(large.clone()), ..ExecOptions::default() };
        assert_eq!(run(&db, sql, &opts).unwrap(), free, "{sql}");
        assert_eq!(large.used(), 0, "{sql}");
        // Under a small one, refused, and nothing held after either.
        let opts = ExecOptions { memory: Some(small.clone()), ..ExecOptions::default() };
        match run(&db, sql, &opts) {
            Err(Error::Memory(m)) => assert!(m.contains("4 MiB"), "{sql}: {m}"),
            r => panic!("{sql}: {r:?}"),
        }
        assert_eq!(small.used(), 0, "{sql}");
        assert_eq!(small.trips(), i as u64 + 1, "{sql}");
    }
    assert_eq!(large.trips(), 0);
    // Light queries fit the small budget.
    let opts = ExecOptions { memory: Some(small.clone()), ..ExecOptions::default() };
    for sql in [
        "SELECT COUNT(*) FROM t",
        "SELECT id, note FROM t WHERE id < 100 LIMIT 100",
        "SELECT b, COUNT(*) FROM t GROUP BY b LIMIT 5",
        "SELECT id FROM t ORDER BY id LIMIT 10",
        "SELECT id FROM t ORDER BY b DESC LIMIT 10",
        "SELECT id FROM t ORDER BY a LIMIT 10 OFFSET 1000",
        "SELECT COUNT(*) FROM t WHERE REACHABLE(next, 0, hops => 1000)",
    ] {
        run(&db, sql, &opts).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(small.used(), 0, "{sql}");
    }
}

#[test]
fn queries_running_at_once_share_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(db(dir.path()));
    // Enough for one grouping at a time, not for eight.
    let budget = MemoryBudget::new(48 << 20);
    let sql = "SELECT a, COUNT(*) FROM t GROUP BY a LIMIT 5";
    let opts = ExecOptions { memory: Some(budget.clone()), ..ExecOptions::default() };
    run(&db, sql, &opts).unwrap();
    let outcomes: Vec<Result<usize, Error>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..8).map(|_| s.spawn(|| run(&db, sql, &opts))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let refused = outcomes.iter().filter(|r| matches!(r, Err(Error::Memory(_)))).count();
    assert!(refused > 0 && outcomes.iter().all(|r| matches!(r, Ok(5) | Err(Error::Memory(_)))), "{outcomes:?}");
    assert_eq!(budget.used(), 0);
}

/// An ordered query with a LIMIT keeps about twice the LIMIT of candidates
/// as it goes, not every match, with the same answer.
#[test]
fn ordered_candidates_are_cut_to_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let ids = |sql: &str| -> Vec<i64> { db.query(sql, &Params::new()).unwrap().rows.iter().map(|r| r[0].as_i64().unwrap()).collect() };
    // `b` has 1,000 values of 200 rows each; `a` one value per row.
    for order in ["b DESC", "b", "a", "a DESC"] {
        let all = ids(&format!("SELECT id FROM t ORDER BY {order}"));
        assert_eq!(all.len(), N as usize);
        for (limit, offset) in [(1, 0), (10, 0), (10, 190), (4097, 0), (9000, 0), (100, 20_000)] {
            let page = ids(&format!("SELECT id FROM t ORDER BY {order} LIMIT {limit} OFFSET {offset}"));
            assert_eq!(page, all[offset..offset + limit], "ORDER BY {order} LIMIT {limit} OFFSET {offset}");
        }
    }
    // With ties broken by key, too.
    let all = ids("SELECT id FROM t ORDER BY b DESC, id DESC");
    assert_eq!(ids("SELECT id FROM t ORDER BY b DESC, id DESC LIMIT 5000"), all[..5000]);
}
