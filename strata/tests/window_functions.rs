//! Window functions (`f(..) OVER (PARTITION BY .. ORDER BY .. frame)`,
//! `WINDOW`, `QUALIFY`) against SQLite's, on a random table with NULLs and
//! ties: ranking, NTILE, LAG and LEAD, value functions and aggregates over
//! ROWS and RANGE frames, over rows, groups, joins, `WITH` results and
//! derived tables, under DISTINCT, ORDER BY and LIMIT. Each query runs on
//! Strata's operators and, with the feature `datafusion`, on DataFusion.
//! SQLite has no QUALIFY, so its query filters a derived table instead.
//!
//! The orders here break their ties (or ask what ties do not change), as
//! SQLite does not say in what order tied rows come; Strata keeps the
//! input's order ([`a_window_s_ties_keep_the_rows_order`]).

#![cfg(feature = "sqlite")]

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rusqlite::Connection;
use strata::db::{Db, SqlResult};
use strata::query::exec::{Engine, ExecOptions};
use strata::query::ir::{self, Plan};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

const ROWS: i64 = 300;

/// Strata's table and SQLite's, with the same rows: `g` a KEYWORD of four
/// values, `h` an INT of four, `x` an INT of many ties, `y` a FLOAT, each
/// NULL in about one row of eight.
fn tables(dir: &std::path::Path) -> (Db, Connection) {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.join("w.strata"), seed, DbOptions { sync: false, segment_max_docs: 70, ..DbOptions::default() }).unwrap();
    db.sql("CREATE TABLE t (id INT PRIMARY KEY, g KEYWORD, h INT, x INT, y FLOAT)", &Params::new()).unwrap();
    let lite = Connection::open_in_memory().unwrap();
    lite.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, g TEXT, h INTEGER, x INTEGER, y REAL)").unwrap();
    let mut rng = StdRng::seed_from_u64(11);
    let mut rows = Vec::new();
    for id in 0..ROWS {
        let mut maybe = |v: Value| if rng.gen_range(0..8) == 0 { Value::Null } else { v };
        let g = maybe(Value::Str(["a", "b", "c", "d"][(id as usize * 7 + 3) % 4].into()));
        let h = maybe(Value::Int((id * 5) % 4));
        let x = maybe(Value::Int((id * 37 % 41) - 20));
        let y = maybe(Value::Float(((id * 53) % 97) as f64 / 8.0));
        rows.push(vec![Value::Int(id), g, h, x, y]);
    }
    for r in &rows {
        let p = |v: &Value| match v {
            Value::Null => rusqlite::types::Value::Null,
            Value::Int(i) => rusqlite::types::Value::Integer(*i),
            Value::Float(f) => rusqlite::types::Value::Real(*f),
            Value::Str(s) => rusqlite::types::Value::Text(s.clone()),
            v => panic!("{v:?}"),
        };
        lite.execute("INSERT INTO t VALUES (?1, ?2, ?3, ?4, ?5)", rusqlite::params_from_iter(r.iter().map(p))).unwrap();
    }
    // Some rows in segments, some in the memtable, one updated, one gone.
    let (old, new) = rows.split_at(250);
    db.load_into("t", old.to_vec()).unwrap();
    for r in new {
        let v = |x: &Value| match x {
            Value::Null => "NULL".to_string(),
            Value::Str(s) => format!("'{s}'"),
            Value::Float(f) => format!("{f:?}"),
            x => format!("{x:?}").trim_start_matches("Int(").trim_end_matches(')').to_string(),
        };
        db.sql(&format!("INSERT INTO t (id, g, h, x, y) VALUES ({})", r.iter().map(v).collect::<Vec<_>>().join(", ")), &Params::new()).unwrap();
    }
    for s in ["UPDATE t SET x = 7, y = 2.5 WHERE id = 17", "DELETE FROM t WHERE id = 42"] {
        db.sql(s, &Params::new()).unwrap();
        lite.execute(s, []).unwrap();
    }
    (db, lite)
}

fn lite_rows(lite: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut st = lite.prepare(sql).unwrap_or_else(|e| panic!("sqlite: {sql}: {e}"));
    let n = st.column_count();
    st.query_map([], |r| {
        (0..n)
            .map(|i| {
                Ok(match r.get::<_, rusqlite::types::Value>(i)? {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(i) => Value::Int(i),
                    rusqlite::types::Value::Real(f) => Value::Float(f),
                    rusqlite::types::Value::Text(s) => Value::Str(s),
                    rusqlite::types::Value::Blob(_) => panic!("a blob"),
                })
            })
            .collect()
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0),
        _ => a == b,
    }
}

fn engines() -> Vec<Engine> {
    let mut out = vec![Engine::Operators];
    if cfg!(feature = "datafusion") {
        out.push(Engine::DataFusion);
    }
    out
}

/// Strata's query, and SQLite's when it differs (QUALIFY).
const QUERIES: &[(&str, Option<&str>)] = &[
    // Ranking.
    ("SELECT id, ROW_NUMBER() OVER (ORDER BY x NULLS LAST, id) FROM t ORDER BY id", None),
    ("SELECT id, ROW_NUMBER() OVER (PARTITION BY g ORDER BY y DESC NULLS FIRST, id) FROM t ORDER BY id", None),
    ("SELECT id, RANK() OVER (ORDER BY x NULLS LAST), DENSE_RANK() OVER (ORDER BY x NULLS LAST), RANK() OVER (ORDER BY x DESC NULLS FIRST) FROM t ORDER BY id", None),
    (
        "SELECT id, RANK() OVER (PARTITION BY g, h ORDER BY x DESC NULLS LAST, y NULLS FIRST), PERCENT_RANK() OVER (PARTITION BY g ORDER BY x NULLS FIRST), CUME_DIST() OVER (PARTITION BY h ORDER BY x NULLS LAST), DENSE_RANK() OVER (PARTITION BY h ORDER BY g NULLS LAST) FROM t ORDER BY id",
        None,
    ),
    ("SELECT id, RANK() OVER (), PERCENT_RANK() OVER (PARTITION BY g), CUME_DIST() OVER () FROM t ORDER BY id", None),
    ("SELECT id, NTILE(4) OVER (ORDER BY id), NTILE(7) OVER (PARTITION BY g ORDER BY x NULLS LAST, id), NTILE(1000) OVER (PARTITION BY h ORDER BY id), NTILE(3) OVER (PARTITION BY x ORDER BY id) FROM t ORDER BY id", None),
    // LAG and LEAD.
    (
        "SELECT id, LAG(x) OVER (ORDER BY id), LEAD(x, 2) OVER (PARTITION BY g ORDER BY id), LAG(y, 3, -1.5) OVER (PARTITION BY h ORDER BY id), LEAD(g, 1, 'none') OVER (ORDER BY id DESC), LAG(x, 0) OVER (ORDER BY id), LEAD(y, 400) OVER (ORDER BY id) FROM t ORDER BY id",
        None,
    ),
    ("SELECT id, LAG(x, 1, h) OVER (PARTITION BY g ORDER BY id), LEAD(x * 2, 1, -id) OVER (ORDER BY y NULLS FIRST, id) FROM t ORDER BY id", None),
    // Aggregates over the whole partition, and running (to the last peer).
    (
        "SELECT id, COUNT(*) OVER (PARTITION BY g), COUNT(x) OVER (PARTITION BY g), SUM(x) OVER (PARTITION BY g), AVG(y) OVER (PARTITION BY h), MIN(x) OVER (), MAX(y) OVER (PARTITION BY g, h), AVG(x) OVER (PARTITION BY h) FROM t ORDER BY id",
        None,
    ),
    (
        "SELECT id, SUM(x) OVER (ORDER BY x NULLS LAST), COUNT(*) OVER (PARTITION BY g ORDER BY h NULLS FIRST), AVG(y) OVER (PARTITION BY g ORDER BY x DESC NULLS LAST), MIN(y) OVER (ORDER BY h NULLS LAST), MAX(x) OVER (PARTITION BY h ORDER BY y NULLS FIRST), SUM(y) OVER (ORDER BY g NULLS FIRST, h NULLS LAST) FROM t ORDER BY id",
        None,
    ),
    // ROWS frames.
    (
        "SELECT id, SUM(x) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW), AVG(y) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING), COUNT(x) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING), MIN(x) OVER (PARTITION BY h ORDER BY id ROWS BETWEEN 3 FOLLOWING AND 5 FOLLOWING), MAX(y) OVER (ORDER BY id ROWS BETWEEN 4 PRECEDING AND 2 PRECEDING), SUM(y) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING) FROM t ORDER BY id",
        None,
    ),
    ("SELECT id, COUNT(*) OVER (PARTITION BY g ORDER BY x NULLS LAST, id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING), SUM(x) OVER (ORDER BY id ROWS BETWEEN 0 PRECEDING AND 0 FOLLOWING), AVG(x) OVER (ORDER BY id DESC ROWS 3 PRECEDING) FROM t ORDER BY id", None),
    // RANGE frames: from the row's value of the one key, NULLs their peers.
    (
        "SELECT id, SUM(x) OVER (ORDER BY x NULLS LAST RANGE BETWEEN 2 PRECEDING AND 2 FOLLOWING), COUNT(*) OVER (PARTITION BY g ORDER BY x DESC NULLS FIRST RANGE BETWEEN 1 PRECEDING AND CURRENT ROW), AVG(x) OVER (ORDER BY y NULLS LAST RANGE BETWEEN 0.5 PRECEDING AND 0.25 FOLLOWING), MAX(x) OVER (ORDER BY x NULLS FIRST RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING), MIN(y) OVER (ORDER BY h NULLS LAST RANGE BETWEEN UNBOUNDED PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        None,
    ),
    ("SELECT id, COUNT(y) OVER (ORDER BY y DESC NULLS LAST RANGE BETWEEN 1 PRECEDING AND 3 FOLLOWING), SUM(h) OVER (PARTITION BY g ORDER BY x RANGE BETWEEN 5 FOLLOWING AND 9 FOLLOWING), MIN(id) OVER (ORDER BY x NULLS LAST RANGE 3 PRECEDING) FROM t ORDER BY id", None),
    // Value functions.
    (
        "SELECT id, FIRST_VALUE(x) OVER (PARTITION BY g ORDER BY id), LAST_VALUE(x) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING), NTH_VALUE(y, 3) OVER (PARTITION BY h ORDER BY id), FIRST_VALUE(y) OVER (ORDER BY id ROWS BETWEEN 2 FOLLOWING AND 4 FOLLOWING), LAST_VALUE(g) OVER (ORDER BY id ROWS BETWEEN 3 PRECEDING AND 1 PRECEDING), NTH_VALUE(x, 2) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        None,
    ),
    (
        "SELECT id, FIRST_VALUE(x) OVER (PARTITION BY g ORDER BY x DESC NULLS LAST), LAST_VALUE(x) OVER (ORDER BY x NULLS LAST), LAST_VALUE(h) OVER (PARTITION BY g ORDER BY h NULLS FIRST), FIRST_VALUE(y) OVER (PARTITION BY h ORDER BY y NULLS FIRST RANGE BETWEEN 1 FOLLOWING AND 2 FOLLOWING), NTH_VALUE(g, 1) OVER (PARTITION BY g) FROM t ORDER BY id",
        None,
    ),
    // Named windows.
    (
        "SELECT id, ROW_NUMBER() OVER w, SUM(x) OVER (w ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), RANK() OVER v, COUNT(*) OVER u FROM t WINDOW w AS (PARTITION BY g ORDER BY id), v AS (ORDER BY h NULLS LAST), u AS (PARTITION BY h) ORDER BY id",
        None,
    ),
    ("SELECT id, LAG(x) OVER (u ORDER BY id), MAX(y) OVER u FROM t WINDOW u AS (PARTITION BY h) ORDER BY id", None),
    // Expressions of windows, and windows of expressions.
    ("SELECT id, x - LAG(x) OVER (ORDER BY id), ROW_NUMBER() OVER (ORDER BY id) * 10 + 1, CASE WHEN RANK() OVER (PARTITION BY g ORDER BY x DESC NULLS LAST) <= 3 THEN 'top' ELSE 'rest' END FROM t ORDER BY id", None),
    ("SELECT id, SUM(x * 2 + h) OVER (PARTITION BY id % 3 ORDER BY id), ROW_NUMBER() OVER (PARTITION BY COALESCE(g, 'z') ORDER BY -id), MAX(LOWER(g)) OVER (PARTITION BY h) FROM t ORDER BY id", None),
    // QUALIFY.
    (
        "SELECT id, g, x FROM t QUALIFY ROW_NUMBER() OVER (PARTITION BY g ORDER BY x DESC NULLS LAST, id) <= 2 ORDER BY id",
        Some("SELECT id, g, x FROM (SELECT id, g, x, ROW_NUMBER() OVER (PARTITION BY g ORDER BY x DESC NULLS LAST, id) AS q FROM t) WHERE q <= 2 ORDER BY id"),
    ),
    ("SELECT id, RANK() OVER (ORDER BY x DESC NULLS LAST) AS r FROM t QUALIFY r <= 10 ORDER BY id", Some("SELECT id, r FROM (SELECT id, RANK() OVER (ORDER BY x DESC NULLS LAST) AS r FROM t) WHERE r <= 10 ORDER BY id")),
    ("SELECT id, x FROM t WHERE y > 3 QUALIFY x > AVG(x) OVER (PARTITION BY g) ORDER BY id", Some("SELECT id, x FROM (SELECT id, x, AVG(x) OVER (PARTITION BY g) AS a FROM t WHERE y > 3) WHERE x > a ORDER BY id")),
    // An order by windows, and cuts after them.
    ("SELECT id, x FROM t ORDER BY ROW_NUMBER() OVER (ORDER BY x NULLS LAST, id DESC) LIMIT 20", None),
    ("SELECT id, SUM(y) OVER (PARTITION BY g) AS s FROM t ORDER BY s DESC NULLS LAST, id LIMIT 15 OFFSET 3", None),
    ("SELECT id, COUNT(*) OVER () FROM t ORDER BY id LIMIT 5", None),
    ("SELECT id, SUM(x) OVER (ORDER BY id) FROM t ORDER BY x DESC NULLS LAST, id LIMIT 10", None),
    ("SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM t WHERE x > 5 ORDER BY id", None),
    ("SELECT id, ROW_NUMBER() OVER () FROM t WHERE x > 1000", None),
    // Over groups: the groups' values and aggregates.
    ("SELECT g, COUNT(*) AS n, RANK() OVER (ORDER BY COUNT(*) DESC), SUM(SUM(x)) OVER (), ROW_NUMBER() OVER (ORDER BY g NULLS FIRST) FROM t GROUP BY g ORDER BY g NULLS FIRST", None),
    (
        "SELECT g, h, COUNT(*) AS n, DENSE_RANK() OVER (PARTITION BY g ORDER BY COUNT(*) DESC) AS r FROM t GROUP BY g, h HAVING COUNT(*) > 3 QUALIFY r = 1 ORDER BY g NULLS FIRST, h NULLS FIRST",
        Some("SELECT g, h, n, r FROM (SELECT g, h, COUNT(*) AS n, DENSE_RANK() OVER (PARTITION BY g ORDER BY COUNT(*) DESC) AS r FROM t GROUP BY g, h HAVING COUNT(*) > 3) WHERE r = 1 ORDER BY g NULLS FIRST, h NULLS FIRST"),
    ),
    ("SELECT h, AVG(y), LAG(AVG(y)) OVER (ORDER BY h NULLS LAST), MAX(x) - MIN(MAX(x)) OVER () FROM t GROUP BY h ORDER BY h NULLS LAST", None),
    // Over joins, WITH results and derived tables, under DISTINCT.
    ("SELECT a.id, b.g, ROW_NUMBER() OVER (PARTITION BY b.g ORDER BY a.id), SUM(a.x) OVER (PARTITION BY a.h) FROM t a JOIN t b ON b.id = a.h ORDER BY a.id", None),
    ("WITH r AS (SELECT id, g, ROW_NUMBER() OVER (PARTITION BY g ORDER BY id DESC) AS rn FROM t) SELECT g, id FROM r WHERE rn = 1 ORDER BY g NULLS FIRST", None),
    ("WITH r AS (SELECT id, g, x FROM t WHERE x > 0) SELECT id, RANK() OVER (PARTITION BY g ORDER BY x NULLS LAST) FROM r ORDER BY id", None),
    ("SELECT id, rn FROM (SELECT id, ROW_NUMBER() OVER (ORDER BY y NULLS FIRST, id) AS rn FROM t WHERE x > 0) d WHERE rn % 5 = 0 ORDER BY id", None),
    ("SELECT DISTINCT g, COUNT(*) OVER (PARTITION BY g) FROM t ORDER BY g NULLS FIRST", None),
    // A frame does not change the functions that read none.
    ("SELECT id, ROW_NUMBER() OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), LAG(x) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND CURRENT ROW) FROM t ORDER BY id", None),
];

#[test]
fn window_functions_answer_as_sqlite_does() {
    let dir = tempfile::tempdir().unwrap();
    let (db, lite) = tables(dir.path());
    let mut fell_back = Vec::new();
    for &(sql, theirs) in QUERIES {
        let want = lite_rows(&lite, theirs.unwrap_or(sql));
        for engine in engines() {
            let opts = ExecOptions { engine, ..db.exec.clone() };
            let got = db.query_with(sql, &Params::new(), &opts).unwrap_or_else(|e| panic!("{sql} ({engine:?}): {e}"));
            if engine == Engine::DataFusion {
                if let Some(why) = got.stats.plan.iter().find(|l| l.starts_with("operators (not datafusion")) {
                    fell_back.push(format!("{sql}: {why}"));
                }
            }
            assert_eq!(want.len(), got.rows.len(), "{sql} ({engine:?}): rows");
            for (i, (w, g)) in want.iter().zip(&got.rows).enumerate() {
                assert!(w.len() == g.len() && w.iter().zip(g).all(|(a, b)| same_value(a, b)), "{sql} ({engine:?}): row {i}: sqlite {w:?}, strata {g:?}");
            }
        }
    }
    // DataFusion runs them all but what its lowering leaves to the
    // operators: a lag's default of the row, and a value function over a
    // RANGE frame of offsets.
    let expected = ["LAG(x, 1, h)", "FIRST_VALUE(y) OVER (PARTITION BY h ORDER BY y NULLS FIRST RANGE BETWEEN 1 FOLLOWING"];
    assert!(fell_back.iter().all(|f| expected.iter().any(|e| f.contains(e))), "{fell_back:#?}");
    assert_eq!(fell_back.len(), if cfg!(feature = "datafusion") { expected.len() } else { 0 }, "{fell_back:#?}");
}

/// Rows that tie in a window's order keep the order they came in: the
/// table's (its keys') without an ORDER BY of the query, and a window of no
/// order sees the partition in that order too.
#[test]
fn a_window_s_ties_keep_the_rows_order() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = tables(dir.path());
    for engine in engines() {
        let opts = ExecOptions { engine, ..db.exec.clone() };
        let rows = |sql: &str| db.query_with(sql, &Params::new(), &opts).unwrap_or_else(|e| panic!("{sql}: {e}")).rows;
        let ordered = rows("SELECT id, ROW_NUMBER() OVER (PARTITION BY h ORDER BY id) FROM t ORDER BY id");
        for sql in ["SELECT id, ROW_NUMBER() OVER (PARTITION BY h) FROM t ORDER BY id", "SELECT id, ROW_NUMBER() OVER (PARTITION BY h ORDER BY h) FROM t ORDER BY id"] {
            assert_eq!(rows(sql), ordered, "{sql} ({engine:?})");
        }
    }
}

#[test]
fn window_functions_out_of_place_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = tables(dir.path());
    for (sql, says) in [
        ("SELECT id FROM t WHERE ROW_NUMBER() OVER (ORDER BY id) = 1", "not part of WHERE"),
        ("SELECT g, COUNT(*) FROM t GROUP BY g HAVING RANK() OVER (ORDER BY g) = 1", "not part of WHERE, ON, GROUP BY or HAVING"),
        ("SELECT id, SUM(ROW_NUMBER() OVER (ORDER BY id)) OVER () FROM t", "holds another"),
        ("SELECT id, ROW_NUMBER() OVER w FROM t", "no window named w"),
        ("SELECT id, ROW_NUMBER() OVER (w PARTITION BY h) FROM t WINDOW w AS (ORDER BY id)", "keeps its partition"),
        ("SELECT id, SUM(x) OVER (w ROWS 1 PRECEDING) FROM t WINDOW w AS (ORDER BY id ROWS 2 PRECEDING)", "has a frame"),
        ("SELECT id, SUM(x) OVER (ORDER BY id GROUPS 1 PRECEDING) FROM t", "GROUPS"),
        ("SELECT id, SUM(x) OVER (ORDER BY id, h RANGE 1 PRECEDING) FROM t", "one ORDER BY key"),
        ("SELECT id, SUM(x) OVER (ORDER BY id ROWS 1.5 PRECEDING) FROM t", "whole number of rows"),
        ("SELECT id, SUM(x) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND 1 PRECEDING) FROM t", "starts no later than it ends"),
        ("SELECT id, SUM(x) OVER (ORDER BY id ROWS BETWEEN x PRECEDING AND CURRENT ROW) FROM t", "a frame's offset is a number"),
        ("SELECT id, NTILE(0) OVER (ORDER BY id) FROM t", "NTILE takes a whole number of at least 1"),
        ("SELECT id, NTILE(x) OVER (ORDER BY id) FROM t", "NTILE takes a whole number"),
        ("SELECT id, NTH_VALUE(x, 0) OVER (ORDER BY id) FROM t", "NTH_VALUE takes a whole number of at least 1"),
        ("SELECT id, LAG(x, -1) OVER (ORDER BY id) FROM t", "LAG takes a whole number of at least 0"),
        ("SELECT id, COUNT(DISTINCT x) OVER () FROM t", "DISTINCT"),
        ("SELECT id, LOWER(g) OVER () FROM t", "not a window function"),
        ("SELECT id, RANK(x) OVER (ORDER BY id) FROM t", "of no arguments"),
        ("SELECT x, ROW_NUMBER() OVER (ORDER BY g) FROM t GROUP BY g", "must be in GROUP BY"),
    ] {
        match db.sql(sql, &Params::new()) {
            Err(e) => assert!(e.to_string().contains(says), "{sql}: {e}"),
            Ok(_) => panic!("{sql}: answered"),
        }
    }
}

/// A plan with windows names the feature, so that a reader without them
/// refuses it by name, and goes through JSON and EXPLAIN whole.
#[test]
fn a_window_s_plan_names_its_feature_and_explains() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = tables(dir.path());
    let sql = "SELECT id, RANK() OVER (PARTITION BY g ORDER BY x DESC), SUM(y) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND 1 FOLLOWING), LAG(x, 2, 0) OVER (ORDER BY id) FROM t QUALIFY NTILE(3) OVER (ORDER BY id) = 1";
    let plan = db.explain(sql, &Params::new()).unwrap();
    assert!(plan.features.iter().any(|f| f == ir::WINDOW), "{:?}", plan.features);
    assert_eq!(Plan::from_json(&plan.to_json().unwrap()).unwrap(), plan);
    let features: Vec<&str> = plan.features.iter().map(String::as_str).collect();
    let older: Vec<&str> = ir::FEATURES.iter().copied().filter(|f| *f != ir::WINDOW).collect();
    let e = Plan::readable_by(plan.version as u64, &features, &older).err().expect("refused").to_string();
    assert!(e.contains("\"window\""), "{e}");
    // A window inside a subquery needs the feature too.
    let inner = db.explain("SELECT id FROM t WHERE id IN (SELECT id FROM (SELECT id, ROW_NUMBER() OVER (ORDER BY x) AS r FROM t) d WHERE r < 4)", &Params::new()).unwrap();
    assert!(inner.features.iter().any(|f| f == ir::WINDOW), "{:?}", inner.features);
    let SqlResult::Rows(r) = db.sql(&format!("EXPLAIN {sql}"), &Params::new()).unwrap() else { panic!("rows") };
    let text: Vec<String> = r.rows.into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    let text = text.join("\n");
    for part in [
        "Window: RANK() OVER (PARTITION BY g ORDER BY x DESC)",
        "SUM(y) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND 1 FOLLOWING)",
        "LAG(x, 2, 0) OVER (ORDER BY id)",
        "NTILE(3) OVER (ORDER BY id)",
    ] {
        assert!(text.contains(part), "EXPLAIN lacks {part}:\n{text}");
    }
}
