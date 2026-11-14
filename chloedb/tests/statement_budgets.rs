//! Statement budgets ([`chloedb::query::budget`]): a statement is held to
//! its budget's `nodes`, `depth` and `list` once its stored functions are
//! written out, to `plan_ms` as it plans and to `rows_read` as it runs,
//! through every entry point that plans one (audit 4, X-B3 and X-B12).

use chloedb::db::Db;
use chloedb::query::budget::Budget;
use chloedb::query::exec::ExecOptions;
use chloedb::query::Params;
use chloedb::storage::DbOptions;
use chloedb::{Error, Field, FieldType, Schema, Value};

const ROWS: usize = 5_000;

fn open(dir: &std::path::Path) -> Db {
    let schema = Schema::new("t1", vec![Field::new("id", FieldType::Int), Field::new("n", FieldType::Int), Field::new("tags", FieldType::Keyword).multi()], Some("id")).unwrap();
    let db = Db::create(&dir.join("t.chloedb"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load((0..ROWS).map(|i| vec![Value::Int(i as i64), Value::Int((i % 7) as i64), Value::List(vec![Value::Str("x".into()), Value::Str(format!("t{}", i % 10))])])).unwrap();
    db.sql("CREATE FUNCTION wide(x INT) RETURNS INT AS :x + :x + :x + :x + :x + :x + :x + :x", &Params::new()).unwrap();
    db.sql("CREATE FUNCTION wider(x INT) RETURNS INT AS wide(:x) + wide(:x) + wide(:x) + wide(:x)", &Params::new()).unwrap();
    db
}

fn with(db: &Db, budget: Budget) -> ExecOptions {
    ExecOptions { budget, ..db.exec.clone() }
}

fn refused(r: chloedb::Result<impl std::fmt::Debug>, says: &str) {
    match r {
        Err(Error::Query(m)) | Err(Error::Timeout(m)) if m.contains(says) => {}
        r => panic!("expected a refusal naming {says}: {r:?}"),
    }
}

/// `nodes` and `depth` count the statement written out: a call of a few
/// bytes is refused for what its functions write, not for its text.
#[test]
fn a_budget_holds_the_statement_written_out() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let sql = "SELECT wider(n) AS v FROM t1 WHERE id = 3";
    // No budget: it answers 3 × 32.
    assert_eq!(db.query(sql, &Params::new()).unwrap().rows[0][0], Value::Int(96));
    // Its text holds a dozen expressions; written out, more than forty.
    let small = Budget { nodes: Some(30), ..Budget::default() };
    assert!(db.query_with("SELECT n FROM t1 WHERE id = 3", &Params::new(), &with(&db, small)).is_ok());
    refused(db.query_with(sql, &Params::new(), &with(&db, small)), "written out");
    refused(db.query_with(sql, &Params::new(), &with(&db, small)), "(nodes;");
    refused(db.query_with(sql, &Params::new(), &with(&db, Budget { depth: Some(6), ..Budget::default() })), "(depth;");
    assert!(db.query_with(sql, &Params::new(), &with(&db, Budget { nodes: Some(500), depth: Some(100), ..Budget::default() })).is_ok());
    // The same through Db::sql, Db::explain and EXPLAIN.
    refused(db.sql_with(sql, &Params::new(), &with(&db, small)), "(nodes;");
    refused(db.explain_with(sql, &Params::new(), &with(&db, small)), "(nodes;");
    refused(db.sql_with(&format!("EXPLAIN {sql}"), &Params::new(), &with(&db, small)), "(nodes;");
    assert!(db.explain(sql, &Params::new()).is_ok());
}

/// `list` bounds a list in the statement and a list parameter alike;
/// a write is held to its shape as written.
#[test]
fn a_budget_holds_lists_and_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let b = Budget { list: Some(100), ..Budget::default() };
    let ids = |n: usize| Value::List((0..n as i64).map(Value::Int).collect());
    let tags = |n: usize| Value::List((0..n).map(|i| Value::Str(format!("t{i}"))).collect());
    let q = |n| Params::new().set("ids", ids(n));
    assert_eq!(db.query_with("SELECT COUNT(*) FROM t1 WHERE id IN (:ids)", &q(100), &with(&db, b)).unwrap().rows[0][0], Value::Int(100));
    refused(db.query_with("SELECT COUNT(*) FROM t1 WHERE id IN (:ids)", &q(101), &with(&db, b)), "list parameter holds 101");
    refused(db.query_with("SELECT COUNT(*) FROM t1 WHERE tags @> :t", &Params::new().set("t", tags(500)), &with(&db, b)), "(list;");
    let inline = format!("SELECT COUNT(*) FROM t1 WHERE id IN ({})", (0..150).map(|i| i.to_string()).collect::<Vec<_>>().join(", "));
    refused(db.query_with(&inline, &Params::new(), &with(&db, b)), "holds 150 values");
    // Writes: an UPDATE by a long list, through Db::sql and prepare_write.
    refused(db.sql_with("UPDATE t1 SET n = 1 WHERE id IN (:ids)", &q(200), &with(&db, b)), "(list;");
    refused(db.prepare_write_with("DELETE FROM t1 WHERE id IN (:ids)", &q(200), &with(&db, b)).map(|w| w.is_some()), "(list;");
    assert!(db.prepare_write_with("DELETE FROM t1 WHERE id IN (:ids)", &q(20), &with(&db, b)).unwrap().is_some());
    assert_eq!(db.query("SELECT COUNT(*) FROM t1", &Params::new()).unwrap().rows[0][0], Value::Int(ROWS as i64), "nothing written");
}

/// `rows_read` stops a scan as it reads past the budget, every part of
/// the statement counting into one; a lookup by key reads its rows only.
#[test]
fn a_budget_holds_the_rows_a_statement_reads() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let b = |n| with(&db, Budget { rows_read: Some(n), ..Budget::default() });
    // A scan of every row, by a column the indexes do not answer alone.
    let scan = "SELECT COUNT(*) FROM t1 WHERE n + 1 > 0";
    assert_eq!(db.query_with(scan, &Params::new(), &b(ROWS as u64 * 3)).unwrap().rows[0][0], Value::Int(ROWS as i64));
    refused(db.query_with(scan, &Params::new(), &b(1_000)), "(rows_read;");
    refused(db.query_with("SELECT id, n FROM t1 WHERE n + 1 > 0 ORDER BY id LIMIT 3000", &Params::new(), &b(2_000)), "read more than 2000 rows");
    // A join reads both sides into one count.
    let join = "SELECT COUNT(*) FROM t1 a JOIN (SELECT id FROM t1 WHERE n = 1) b ON a.id = b.id";
    refused(db.query_with(join, &Params::new(), &b(2_000)), "(rows_read;");
    // A few rows by key.
    assert_eq!(db.query_with("SELECT n FROM t1 WHERE id IN (1, 2, 3)", &Params::new(), &b(10)).unwrap().rows.len(), 3);
    // No budget, no count.
    assert!(db.query(join, &Params::new()).is_ok());
}

/// `plan_ms` stops a plan that takes longer, with a timeout that names
/// it, and leaves a quick one be.
#[test]
fn a_budget_holds_the_time_a_plan_takes() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let ctes: Vec<String> = std::iter::once("c0 AS (SELECT id FROM t1)".to_string()).chain((1..2000).map(|i| format!("c{i} AS (SELECT id FROM c{})", i - 1))).collect();
    let slow = format!("WITH {} SELECT COUNT(*) FROM c1999", ctes.join(", "));
    let b = with(&db, Budget { plan_ms: Some(1), ..Budget::default() });
    match db.query_with(&slow, &Params::new(), &b) {
        Err(Error::Timeout(m)) => assert!(m.contains("plan_ms"), "{m}"),
        r => panic!("{:?}", r.map(|rs| rs.rows.len())),
    }
    assert!(db.query_with("SELECT COUNT(*) FROM t1", &Params::new(), &with(&db, Budget { plan_ms: Some(1_000), ..Budget::default() })).is_ok());
}

/// A list of 100,000 values plans under its deadline: the binder checks it
/// a thousand values apart (X-B3), and the plan of `EXPLAIN` and of a
/// write stops there too (X-B12).
#[test]
fn long_lists_stop_at_the_deadline_as_they_bind() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let tags = Value::List((0..100_000).map(|i| Value::Str(format!("t{i}"))).collect());
    let p = Params::new().set("t", tags);
    let past = ExecOptions { deadline: Some(std::time::Instant::now()), ..db.exec.clone() };
    for r in [
        db.query_with("SELECT COUNT(*) FROM t1 WHERE tags @> :t", &p, &past).map(drop),
        db.explain_with("SELECT COUNT(*) FROM t1 WHERE tags @> :t", &p, &past).map(drop),
        db.prepare_write_with("DELETE FROM t1 WHERE tags @> :t", &p, &past).map(drop),
    ] {
        assert!(matches!(r, Err(Error::Timeout(_))), "{r:?}");
    }
}
