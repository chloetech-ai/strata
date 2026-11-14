//! Statements that nest deeply (long chains of `UNION`, `||`, `+`, a
//! `GROUP BY` of a long sum) either answer or are refused with a query
//! error, on a thread with a tokio worker's 2 MiB of stack: none of them
//! overflows it, which aborts the process. Each shape runs at the depth
//! a statement plans and runs at on the thread that asks for it
//! (`INLINE_DEPTH`), at the depth the parser takes ([`MAX_EXPR_DEPTH`],
//! [`SET_OP_LEVELS`]), which runs on a thread of its own, and just past
//! it, on the operators and on DataFusion.

use chloedb::db::Db;
use chloedb::query::compound;
use chloedb::query::exec::{Engine, ExecOptions};
use chloedb::query::parse::{nests_deep, parse, MAX_EXPR_DEPTH, SET_OP_LEVELS};
use chloedb::query::Params;
use chloedb::storage::DbOptions;
use chloedb::{Error, Field, FieldType, Schema, Value};

/// A tokio worker's stack, and a blocking thread's. A build without
/// optimisation has frames several times larger.
const STACK: usize = if cfg!(debug_assertions) { 16 << 20 } else { 2 << 20 };

fn open(dir: &std::path::Path) -> Db {
    let schema = Schema::new("t1", vec![Field::new("id", FieldType::Int), Field::new("n", FieldType::Int), Field::new("a", FieldType::Text { positions: false })], Some("id")).unwrap();
    let db = Db::create(&dir.join("t.chloedb"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    let rows: Vec<String> = (0..50).map(|i| format!("({i}, {}, 'row {i}')", i % 7)).collect();
    db.sql(&format!("INSERT INTO t1 (id, n, a) VALUES {}", rows.join(", ")), &Params::new()).unwrap();
    db
}

/// The queries joined by set operators that the parser takes at most.
fn most_joined() -> usize {
    (MAX_EXPR_DEPTH - 1) / SET_OP_LEVELS + 1
}

/// Each shape, by the number of terms (or queries) it is written with.
#[allow(clippy::type_complexity)]
fn shapes() -> Vec<(&'static str, Box<dyn Fn(usize) -> String>, usize)> {
    let chain = |sep: &'static str, term: &'static str| move |n: usize| vec![term; n].join(sep);
    let terms = MAX_EXPR_DEPTH - 10;
    vec![
        ("UNION ALL", Box::new(move |n| chain(" UNION ALL ", "SELECT 1")(n)), most_joined()),
        ("UNION", Box::new(move |n| chain(" UNION ", "SELECT id FROM t1 WHERE id = 1")(n)), most_joined()),
        ("INTERSECT", Box::new(move |n| chain(" INTERSECT ", "SELECT 1")(n)), most_joined()),
        ("EXCEPT", Box::new(move |n| chain(" EXCEPT ", "SELECT n FROM t1")(n)), most_joined()),
        ("IN (UNION ALL)", Box::new(move |n| format!("SELECT id FROM t1 WHERE id IN ({})", chain(" UNION ALL ", "SELECT 1")(n))), most_joined()),
        ("||", Box::new(move |n| format!("SELECT {} FROM t1 LIMIT 1", chain(" || ", "'a'")(n))), terms),
        ("|| of a column", Box::new(move |n| format!("SELECT {} FROM t1 LIMIT 1", chain(" || ", "a")(n))), terms),
        ("+", Box::new(move |n| format!("SELECT {} FROM t1 LIMIT 1", chain(" + ", "1")(n))), terms),
        ("GROUP BY a sum", Box::new(move |n| format!("SELECT {} AS k, COUNT(*) FROM t1 GROUP BY k", chain(" + ", "n")(n))), terms),
        ("ORDER BY a sum", Box::new(move |n| format!("SELECT id FROM t1 ORDER BY {} DESC LIMIT 1", chain(" + ", "n")(n))), terms),
        ("WHERE a sum", Box::new(move |n| format!("SELECT id FROM t1 WHERE {} >= 0", chain(" + ", "n")(n))), terms),
    ]
}

fn engines() -> Vec<Engine> {
    let mut e = vec![Engine::Operators];
    if cfg!(feature = "datafusion") {
        e.push(Engine::Auto);
    }
    e
}

#[test]
fn deep_statements_answer_or_are_refused_within_a_workers_stack() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open(dir.path()));
    for (name, sql, at) in shapes() {
        // The longest of the shape planned and run on the thread that asks
        // (on 2 MiB), then one near the longest the parser takes (on a
        // thread of QUERY_STACK), then the shortest it refuses.
        let inline = (1..at).rev().find(|&n| !nests_deep(&sql(n))).expect("a statement short enough");
        let past = (at..).find(|&n| parse(&sql(n)).is_err()).expect("a statement too deep");
        for (n, ok) in [(inline, true), (at, true), (past, false)] {
            let text = sql(n);
            for engine in engines() {
                // Which statement it was, should one abort the process.
                eprintln!("{name} of {n} on {engine:?}");
                let db = db.clone();
                let text = text.clone();
                let r = std::thread::Builder::new()
                    .stack_size(STACK)
                    .spawn(move || {
                        let opts = ExecOptions { engine, ..db.exec.clone() };
                        db.query_with(&text, &Params::new(), &opts).map(|rs| rs.rows.len())
                    })
                    .unwrap()
                    .join()
                    .unwrap();
                match (ok, &r) {
                    (true, Ok(_)) => {}
                    (false, Err(Error::Query(m))) if m.contains("nests more than") => {}
                    _ => panic!("{name} of {n} on {engine:?}: {r:?}"),
                }
            }
        }
    }
}

#[test]
fn a_query_joining_many_queries_answers_their_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let n = most_joined();
    let sql = (0..n).map(|i| format!("SELECT id FROM t1 WHERE id = {}", i % 50)).collect::<Vec<_>>().join(" UNION ALL ");
    for engine in engines() {
        let rs = db.query_with(&sql, &Params::new(), &ExecOptions { engine, ..db.exec.clone() }).unwrap();
        assert_eq!(rs.rows.len(), n, "{engine:?}");
    }
    // INTERSECT binds tighter than UNION, also along a chain.
    let rs = db.query("SELECT id FROM t1 WHERE id < 3 UNION SELECT id FROM t1 WHERE id = 10 INTERSECT SELECT id FROM t1 WHERE id > 5 EXCEPT SELECT 1", &Params::new()).unwrap();
    let mut ids: Vec<String> = rs.rows.iter().map(|r| format!("{:?}", r[0])).collect();
    ids.sort();
    assert_eq!(ids.len(), 3, "{ids:?}");
}

#[test]
fn planning_stops_at_the_deadline_and_when_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let schema = db.schema();
    let catalog = [&*schema];
    let ctes: Vec<String> = std::iter::once("c0 AS (SELECT id FROM t1)".to_string()).chain((1..400).map(|i| format!("c{i} AS (SELECT id FROM c{})", i - 1))).collect();
    let sql = format!("WITH {} SELECT COUNT(*) FROM c399", ctes.join(", "));
    let plan = |opts: &ExecOptions| compound::planning_under(opts, || compound::plan_read(&sql, &catalog, &Params::new(), &|_, _| None));
    assert!(plan(&ExecOptions::default()).is_ok());
    let past = ExecOptions { deadline: Some(std::time::Instant::now()), ..ExecOptions::default() };
    assert!(matches!(plan(&past), Err(Error::Timeout(_))), "past its deadline");
    let cancelled = ExecOptions { cancel: Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true))), ..ExecOptions::default() };
    assert!(matches!(plan(&cancelled), Err(Error::Timeout(_))), "cancelled");
    // Outside planning_under, no limit holds.
    assert!(plan(&ExecOptions::default()).is_ok());
    // A query of the database plans under its own deadline.
    match db.query_with(&sql, &Params::new(), &ExecOptions { deadline: Some(std::time::Instant::now()), ..db.exec.clone() }) {
        Err(Error::Timeout(m)) => assert!(m.contains("planned"), "{m}"),
        r => panic!("{:?}", r.map(|rs| rs.rows.len())),
    }
}

/// `f` on a thread with a tokio worker's stack ([`STACK`]).
fn on_a_workers_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new().stack_size(STACK).spawn(f).unwrap().join().unwrap()
}

/// A stored function's call is written out after the statement's text
/// was checked, so a call of a few bytes could plan as deep as its bodies
/// nest together (audit 4, X-B1): it is checked written out, and refused
/// past the depth a statement's text may have, on a worker's stack.
#[test]
fn stored_functions_written_out_are_held_to_the_depth_of_a_statement() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open(dir.path()));
    let terms = |n: usize| vec!["1"; n].join(" + ");
    // Each body as deep as the parser takes, and each calling the one before.
    db.sql(&format!("CREATE FUNCTION f1(x INT) RETURNS INT AS :x + {}", terms(MAX_EXPR_DEPTH - 20)), &Params::new()).unwrap();
    for k in 2..=16 {
        db.sql(&format!("CREATE FUNCTION f{k}(x INT) RETURNS INT AS f{}(:x) + {}", k - 1, terms(MAX_EXPR_DEPTH - 20)), &Params::new()).unwrap();
    }
    // Calls nested in the arguments, the text a few levels deep; and a
    // body that writes out four copies of the one below it, 4^16 of f1.
    db.sql("CREATE FUNCTION g1(x INT) RETURNS INT AS :x + :x + :x + :x", &Params::new()).unwrap();
    for k in 2..=16 {
        db.sql(&format!("CREATE FUNCTION g{k}(x INT) RETURNS INT AS g{p}(:x) + g{p}(:x) + g{p}(:x) + g{p}(:x)", p = k - 1), &Params::new()).unwrap();
    }
    // A short statement that calls one is planned with the stack for it.
    assert!(nests_deep("SELECT f1(n) FROM t1"));
    assert!(nests_deep("SELECT * FROM t(1)"));
    assert!(!nests_deep("SELECT COUNT(*) FROM t1 WHERE id IN (1, 2)"));
    assert!(!nests_deep("INSERT INTO t1 (id, n, a) VALUES (1, 2, 'x')"));
    assert!(!nests_deep("SELECT id FROM t1 WHERE CAST(n AS TEXT) = '1' AND EXISTS (SELECT 1)"));
    let cases: Vec<(String, Option<&str>)> = vec![
        ("SELECT f1(n) FROM t1 LIMIT 1".into(), None),
        ("SELECT g4(n) FROM t1 LIMIT 1".into(), None),
        ("SELECT f2(n) FROM t1 LIMIT 1".into(), Some("nests more than")),
        ("SELECT f16(n) FROM t1 LIMIT 1".into(), Some("nests more than")),
        ("SELECT id FROM t1 WHERE f16(n) > 0".into(), Some("nests more than")),
        ("SELECT f1(f1(f1(n))) FROM t1 LIMIT 1".into(), Some("nests more than")),
        ("SELECT g16(n) FROM t1 LIMIT 1".into(), Some("expressions")),
    ];
    for (sql, refused) in cases {
        for engine in engines() {
            eprintln!("{sql} on {engine:?}");
            let (db, text) = (db.clone(), sql.clone());
            let r = on_a_workers_stack(move || db.query_with(&text, &Params::new(), &ExecOptions { engine, ..db.exec.clone() }).map(|rs| rs.rows.len()));
            match (refused, &r) {
                (None, Ok(n)) => assert!(*n >= 1, "{sql}"),
                (Some(why), Err(Error::Query(m))) if m.contains(why) && m.contains("written out") => {}
                _ => panic!("{sql} on {engine:?}: {r:?}"),
            }
        }
    }
    // The answer of one that may be written out.
    let rs = db.query("SELECT f1(n) AS v FROM t1 WHERE id = 3", &Params::new()).unwrap();
    assert_eq!(rs.rows[0][0], Value::Int(3 + (MAX_EXPR_DEPTH - 20) as i64));
}

/// A table of `n` rows, each with tags `x` and `y`.
fn tagged(dir: &std::path::Path, n: usize) -> Db {
    let schema = Schema::new("docs", vec![Field::new("id", FieldType::Int), Field::new("tags", FieldType::Keyword).multi()], Some("id")).unwrap();
    let db = Db::create(&dir.join("d.chloedb"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load((0..n).map(|i| vec![Value::Int(i as i64), Value::List(vec![Value::Str("x".into()), Value::Str("y".into())])])).unwrap();
    db
}

/// A condition over rows in memory with many parts is one node of the
/// plan (an `IN` per value of `tags @> :list`, the conditions of a long
/// WHERE), which DataFusion must not get as a chain as deep as it is long
/// (audit 4, X-B2): over more rows than the operators keep for themselves,
/// on a worker's stack, it answers.
#[test]
fn wide_conditions_over_rows_in_memory_answer_within_a_workers_stack() {
    let dir = tempfile::tempdir().unwrap();
    // More rows than the operators take for themselves under Auto (4,096).
    let db = std::sync::Arc::new(tagged(dir.path(), 5_000));
    let in_memory = "SELECT id FROM (SELECT id, tags FROM docs LIMIT 100000) d";
    let many = |n: usize| Value::List((0..n).map(|i| Value::Str(["x", "y"][i % 2].into())).collect());
    // A WHERE of 1,020 conditions in groups of 30, which the planner joins
    // into one AND.
    let group = format!("({})", vec!["d.id + e.id >= 0"; 30].join(" AND "));
    let wide_where = format!("SELECT d.id FROM docs d JOIN docs e ON d.id = e.id WHERE {}", vec![group.as_str(); 34].join(" AND "));
    let cases: Vec<(String, Params, usize)> = vec![
        (format!("{in_memory} WHERE tags @> :tags"), Params::new().set("tags", many(2_000)), 5_000),
        (format!("{in_memory} WHERE tags @> :tags"), Params::new().set("tags", many(900)), 5_000),
        (format!("{in_memory} WHERE tags @> ARRAY[{}]", vec!["'x'"; 2_000].join(", ")), Params::new(), 5_000),
        (format!("{in_memory} WHERE tags @> :tags"), Params::new().set("tags", Value::List(vec![Value::Str("x".into()), Value::Str("z".into())])), 0),
        (wide_where, Params::new(), 5_000),
    ];
    for (sql, params, want) in cases {
        for engine in engines() {
            eprintln!("{} on {engine:?}", &sql[..sql.len().min(120)]);
            let (db, text, p) = (db.clone(), sql.clone(), params.clone());
            let t = std::time::Instant::now();
            let r = on_a_workers_stack(move || db.query_with(&text, &p, &ExecOptions { engine, max_rows: Some(100_000), ..db.exec.clone() }).map(|rs| rs.rows.len()));
            eprintln!("  {:?}", t.elapsed());
            assert_eq!(r.as_ref().ok(), Some(&want), "{} on {engine:?}: {r:?}", &sql[..sql.len().min(120)]);
        }
    }
}
