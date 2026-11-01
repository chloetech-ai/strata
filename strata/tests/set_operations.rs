//! Set operations (`UNION`, `INTERSECT`, `EXCEPT`, each with `ALL`)
//! against SQLite's, on two random tables with NULLs and repeated values:
//! of columns, groups, windows and literals, nested, in parentheses, in
//! derived tables, `WITH` results and subqueries, under ORDER BY and LIMIT.
//! Each query runs on Strata's operators and, with the feature
//! `datafusion`, on DataFusion.
//!
//! SQLite has no `INTERSECT ALL` and `EXCEPT ALL`, gives every operator one
//! precedence and takes no parentheses around a SELECT, so those queries
//! are written for it another way, or checked against [`expected`], which
//! takes the rows of each side from SQLite. [`expected`] also gives the
//! order Strata promises when the query has no ORDER BY.

#![cfg(feature = "sqlite")]

use std::collections::HashMap;

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

/// Strata's tables and SQLite's, with the same rows. `t` (300 rows): `g` a
/// KEYWORD of four values, `h` an INT of four, `x` an INT of many repeats,
/// `y` a FLOAT. `u` (120 rows): `g` of six values (two not in `t`), `x` an
/// INT, `z` a FLOAT never whole. Each value is NULL in about one row of
/// eight.
fn tables(dir: &std::path::Path) -> (Db, Connection) {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.join("s.strata"), seed, DbOptions { sync: false, segment_max_docs: 70, ..DbOptions::default() }).unwrap();
    db.sql("CREATE TABLE t (id INT PRIMARY KEY, g KEYWORD, h INT, x INT, y FLOAT)", &Params::new()).unwrap();
    db.sql("CREATE TABLE u (id INT PRIMARY KEY, g KEYWORD, x INT, z FLOAT)", &Params::new()).unwrap();
    let lite = Connection::open_in_memory().unwrap();
    lite.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, g TEXT, h INTEGER, x INTEGER, y REAL); CREATE TABLE u (id INTEGER PRIMARY KEY, g TEXT, x INTEGER, z REAL)").unwrap();
    let mut rng = StdRng::seed_from_u64(23);
    let mut maybe = |v: Value| if rng.gen_range(0..8) == 0 { Value::Null } else { v };
    let t: Vec<Vec<Value>> = (0..300i64)
        .map(|id| {
            let g = maybe(Value::Str(["a", "b", "c", "d"][(id as usize * 7 + 3) % 4].into()));
            let h = maybe(Value::Int((id * 5) % 4));
            let x = maybe(Value::Int((id * 37 % 41) - 20));
            let y = maybe(Value::Float(((id * 53) % 97) as f64 / 8.0));
            vec![Value::Int(id), g, h, x, y]
        })
        .collect();
    let u: Vec<Vec<Value>> = (0..120i64)
        .map(|id| {
            let g = maybe(Value::Str(["a", "c", "e", "b", "f", "a"][(id as usize * 5 + 1) % 6].into()));
            let x = maybe(Value::Int((id * 13 % 23) - 11));
            let z = maybe(Value::Float(((id * 29) % 31) as f64 - 15.5));
            vec![Value::Int(id), g, x, z]
        })
        .collect();
    let lit = |v: &Value| match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Int(i) => rusqlite::types::Value::Integer(*i),
        Value::Float(f) => rusqlite::types::Value::Real(*f),
        Value::Str(s) => rusqlite::types::Value::Text(s.clone()),
        v => panic!("{v:?}"),
    };
    for (table, rows) in [("t", &t), ("u", &u)] {
        let marks = vec!["?"; rows[0].len()].join(", ");
        for r in rows {
            lite.execute(&format!("INSERT INTO {table} VALUES ({marks})"), rusqlite::params_from_iter(r.iter().map(lit))).unwrap();
        }
        // Some rows in segments, some in the memtable.
        let (old, new) = rows.split_at(rows.len() * 5 / 6);
        db.load_into(table, old.to_vec()).unwrap();
        for r in new {
            let v = |x: &Value| match x {
                Value::Null => "NULL".to_string(),
                Value::Str(s) => format!("'{s}'"),
                Value::Float(f) => format!("{f:?}"),
                Value::Int(i) => i.to_string(),
                x => panic!("{x:?}"),
            };
            db.sql(&format!("INSERT INTO {table} VALUES ({})", r.iter().map(v).collect::<Vec<_>>().join(", ")), &Params::new()).unwrap();
        }
    }
    // One row updated, one gone.
    for s in ["UPDATE t SET x = 7, g = 'e' WHERE id = 17", "DELETE FROM u WHERE id = 42"] {
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

fn same_rows(want: &[Vec<Value>], got: &[Vec<Value>]) -> bool {
    want.len() == got.len() && want.iter().zip(got).all(|(w, g)| w.len() == g.len() && w.iter().zip(g).all(|(a, b)| same_value(a, b)))
}

fn engines() -> Vec<Engine> {
    let mut out = vec![Engine::Operators];
    if cfg!(feature = "datafusion") {
        out.push(Engine::DataFusion);
    }
    out
}

/// Strata's query, and SQLite's when it differs.
const QUERIES: &[(&str, Option<&str>)] = &[
    ("SELECT g FROM t UNION SELECT g FROM u ORDER BY g NULLS FIRST", None),
    ("SELECT g, x FROM t UNION ALL SELECT g, x FROM u ORDER BY g NULLS FIRST, x NULLS FIRST", None),
    ("SELECT x FROM t INTERSECT SELECT x FROM u ORDER BY x NULLS FIRST", None),
    ("SELECT g, h FROM t EXCEPT SELECT g, x FROM u ORDER BY g NULLS FIRST, h NULLS FIRST", None),
    ("SELECT g FROM u EXCEPT SELECT g FROM t ORDER BY g DESC NULLS LAST", None),
    ("SELECT h FROM t UNION SELECT h FROM t ORDER BY h NULLS FIRST", None),
    ("SELECT x, g FROM u INTERSECT SELECT x, g FROM u WHERE id < 60 ORDER BY x NULLS FIRST, g NULLS FIRST", None),
    // INTERSECT before UNION and EXCEPT; left to right otherwise.
    (
        "SELECT x FROM t WHERE h = 1 UNION SELECT x FROM u WHERE g = 'a' INTERSECT SELECT x FROM t WHERE h = 2 ORDER BY x NULLS FIRST",
        Some("SELECT x FROM t WHERE h = 1 UNION SELECT * FROM (SELECT x FROM u WHERE g = 'a' INTERSECT SELECT x FROM t WHERE h = 2) ORDER BY x NULLS FIRST"),
    ),
    ("SELECT x FROM t WHERE h = 1 EXCEPT SELECT x FROM u UNION SELECT x FROM u WHERE g = 'f' ORDER BY x NULLS FIRST", None),
    (
        "SELECT g FROM t WHERE h = 0 UNION ALL (SELECT g FROM u EXCEPT SELECT g FROM t WHERE h = 3) ORDER BY g NULLS FIRST",
        Some("SELECT g FROM t WHERE h = 0 UNION ALL SELECT * FROM (SELECT g FROM u EXCEPT SELECT g FROM t WHERE h = 3) ORDER BY g NULLS FIRST"),
    ),
    // ORDER BY and LIMIT of the whole, and of a side in parentheses.
    ("SELECT id, x FROM t WHERE x > 15 UNION ALL SELECT id, x FROM u WHERE x < -8 ORDER BY x DESC, id LIMIT 7", None),
    ("SELECT id, x FROM t WHERE x > 15 UNION ALL SELECT id, x FROM u WHERE x < -8 ORDER BY x, id LIMIT 5 OFFSET 3", None),
    (
        "(SELECT id FROM t ORDER BY id LIMIT 3) UNION ALL (SELECT id FROM u ORDER BY id DESC LIMIT 2) ORDER BY id",
        Some("SELECT * FROM (SELECT id FROM t ORDER BY id LIMIT 3) UNION ALL SELECT * FROM (SELECT id FROM u ORDER BY id DESC LIMIT 2) ORDER BY id"),
    ),
    // Sides of groups, windows, computed columns and literals.
    ("SELECT g, COUNT(*) AS n FROM t GROUP BY g UNION ALL SELECT g, COUNT(*) FROM u GROUP BY g ORDER BY g NULLS FIRST, n", None),
    ("SELECT id, ROW_NUMBER() OVER (ORDER BY id DESC) AS r FROM t WHERE id < 6 UNION ALL SELECT id, x FROM u WHERE id < 6 ORDER BY id, r NULLS FIRST", None),
    ("SELECT x * 2 AS v FROM t WHERE h = 1 UNION SELECT x + 1 FROM u ORDER BY v NULLS FIRST", None),
    ("SELECT g, NULL AS x FROM u WHERE id < 30 UNION ALL SELECT g, x FROM t WHERE id < 20 ORDER BY g NULLS FIRST, x NULLS FIRST", None),
    ("SELECT 'one' AS k, COUNT(*) AS n FROM t UNION ALL SELECT 'two', COUNT(*) FROM u ORDER BY k", None),
    // An INT column with a FLOAT one.
    ("SELECT x FROM t UNION SELECT z FROM u ORDER BY x NULLS FIRST", None),
    // Inside derived tables, WITH results and subqueries.
    ("SELECT g, COUNT(*) AS n, SUM(x) AS s FROM (SELECT g, x FROM t UNION ALL SELECT g, x FROM u) d GROUP BY g ORDER BY g NULLS FIRST", None),
    ("WITH s AS (SELECT x FROM t EXCEPT SELECT x FROM u) SELECT COUNT(*), MIN(x), MAX(x) FROM s", None),
    ("WITH a AS (SELECT g, x FROM t WHERE h = 2), b AS (SELECT g, x FROM u) SELECT g, x FROM a INTERSECT SELECT g, x FROM b ORDER BY g NULLS FIRST, x NULLS FIRST", None),
    ("SELECT id FROM t WHERE x IN (SELECT x FROM u WHERE g = 'b' UNION SELECT h FROM t WHERE id < 10) ORDER BY id", None),
    ("SELECT d.g, d.x, u.id FROM (SELECT g, x FROM t WHERE id < 40 INTERSECT SELECT g, x FROM u) d JOIN u ON u.x = d.x ORDER BY u.id, d.g NULLS FIRST", None),
    ("SELECT x, COUNT(*) AS n FROM (SELECT x FROM t UNION ALL SELECT x FROM t WHERE h = 1 UNION ALL SELECT x FROM u) d WHERE x IS NOT NULL GROUP BY x HAVING COUNT(*) > 9 ORDER BY x", None),
];

#[test]
fn set_operations_answer_as_sqlite_does() {
    let dir = tempfile::tempdir().unwrap();
    let (db, lite) = tables(dir.path());
    let mut fell_back = Vec::new();
    for &(sql, theirs) in QUERIES {
        let want = lite_rows(&lite, theirs.unwrap_or(sql));
        assert!(!want.is_empty(), "{sql}: no rows to compare");
        for engine in engines() {
            let opts = ExecOptions { engine, ..db.exec.clone() };
            let got = db.query_with(sql, &Params::new(), &opts).unwrap_or_else(|e| panic!("{sql} ({engine:?}): {e}"));
            if engine == Engine::DataFusion {
                if let Some(why) = got.stats.plan.iter().find(|l| l.starts_with("operators (not datafusion")) {
                    fell_back.push(format!("{sql}: {why}"));
                }
            }
            assert!(same_rows(&want, &got.rows), "{sql} ({engine:?}):\nsqlite {want:?}\nstrata {:?}", got.rows);
        }
    }
    // DataFusion runs them all but the one whose sides' types differ, which
    // the operators answer.
    let expected = ["SELECT x FROM t UNION SELECT z FROM u"];
    assert!(fell_back.iter().all(|f| expected.iter().any(|e| f.contains(e))), "{fell_back:#?}");
    assert_eq!(fell_back.len(), if cfg!(feature = "datafusion") { expected.len() } else { 0 }, "{fell_back:#?}");
}

/// What a set operation gives, from its sides' rows in their order: each
/// row of `left` (of `left` and `right` for UNION) is kept or not by how
/// many rows equal to it came before it on its side and how many the other
/// side has.
fn expected(op: &str, left: &[Vec<Value>], right: &[Vec<Value>]) -> Vec<Vec<Value>> {
    let key = |r: &Vec<Value>| format!("{r:?}");
    let count = |rows: &[Vec<Value>]| {
        let mut c: HashMap<String, usize> = HashMap::new();
        rows.iter().for_each(|r| *c.entry(key(r)).or_default() += 1);
        c
    };
    let (ours, theirs) = (count(left), count(right));
    let mut nth: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::new();
    let rows: Vec<&Vec<Value>> = match op {
        "UNION" | "UNION ALL" => left.iter().chain(right).collect(),
        _ => left.iter().collect(),
    };
    for r in rows {
        let k = key(r);
        let n = nth.entry(k.clone()).or_default();
        *n += 1;
        let t = theirs.get(&k).copied().unwrap_or(0);
        let keep = match op {
            "UNION ALL" => true,
            "UNION" => *n == 1,
            "INTERSECT" => *n == 1 && t > 0,
            "INTERSECT ALL" => *n <= t,
            "EXCEPT" => *n == 1 && t == 0,
            "EXCEPT ALL" => *n + t <= ours[&k],
            op => panic!("{op}"),
        };
        if keep {
            out.push(r.clone());
        }
    }
    out
}

/// Without an ORDER BY, a set operation's rows come in its sides' order
/// ([`ir::Node::SetOp`]), and the ALL forms SQLite lacks keep as many of
/// each row as the sides' counts say.
#[test]
fn set_operations_keep_their_sides_order() {
    let dir = tempfile::tempdir().unwrap();
    let (db, lite) = tables(dir.path());
    let sides = [
        ("SELECT g, h FROM t", "SELECT g, x FROM u"),
        ("SELECT x FROM t", "SELECT x FROM u"),
        ("SELECT h FROM t", "SELECT h FROM t WHERE id < 100"),
        ("SELECT g FROM t", "SELECT g FROM u"),
        ("SELECT g, x FROM u WHERE id < 70", "SELECT g, x FROM t"),
        ("SELECT x, h FROM t WHERE h = 3", "SELECT x, h FROM t WHERE x > 0"),
    ];
    let sorted = |mut rows: Vec<Vec<Value>>| {
        rows.sort_by_key(|r| format!("{r:?}"));
        rows
    };
    let mut tried = 0;
    for engine in engines() {
        let opts = ExecOptions { engine, ..db.exec.clone() };
        let rows = |sql: &str| db.query_with(sql, &Params::new(), &opts).unwrap_or_else(|e| panic!("{sql} ({engine:?}): {e}")).rows;
        for (a, b) in sides {
            // Each side's rows, in the order the side gives them: SQLite's
            // rows, in an order of Strata's.
            let (left, right) = (rows(a), rows(b));
            assert_eq!(sorted(left.clone()), sorted(lite_rows(&lite, a)), "{a}");
            assert_eq!(sorted(right.clone()), sorted(lite_rows(&lite, b)), "{b}");
            for op in ["UNION", "UNION ALL", "INTERSECT", "INTERSECT ALL", "EXCEPT", "EXCEPT ALL"] {
                let sql = format!("{a} {op} {b}");
                let want = expected(op, &left, &right);
                tried += !want.is_empty() as usize;
                let got = rows(&sql);
                assert!(same_rows(&want, &got), "{sql} ({engine:?}):\nwant {want:?}\ngot  {got:?}");
            }
        }
    }
    let tried = tried / engines().len();
    assert!(tried > 30, "{tried} of 36 have rows");
}

#[test]
fn set_operations_out_of_shape_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = tables(dir.path());
    for (sql, says) in [
        ("SELECT g, x FROM t UNION SELECT g FROM u", "UNION: the query before it selects 2 columns and the one after 1"),
        ("SELECT g FROM t INTERSECT SELECT g, x FROM u", "both sides of a INTERSECT select as many"),
        ("SELECT g FROM t UNION SELECT g FROM u ORDER BY nope", "nope"),
        ("SELECT g FROM t UNION SELECT g FROM u FETCH FIRST 1 ROWS ONLY", "FETCH"),
        ("SELECT g FROM t UNION SELECT g FROM u FOR UPDATE", "FOR UPDATE"),
        ("SELECT g FROM t UNION SELECT nope FROM u", "nope"),
        ("SELECT g FROM t UNION SELECT g FROM missing", "no such table: missing"),
    ] {
        match db.sql(sql, &Params::new()) {
            Err(e) => assert!(e.to_string().contains(says), "{sql}: {e}"),
            Ok(_) => panic!("{sql}: answered"),
        }
    }
}

/// A plan with a set operation names the feature, so that a reader without
/// them refuses it by name, and goes through JSON and EXPLAIN whole.
#[test]
fn a_set_operation_s_plan_names_its_feature_and_explains() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = tables(dir.path());
    let sql = "SELECT g FROM t WHERE h = 1 UNION SELECT g FROM u EXCEPT ALL SELECT g FROM t WHERE h = 2 ORDER BY g LIMIT 3";
    let plan = db.explain(sql, &Params::new()).unwrap();
    assert!(plan.features.iter().any(|f| f == ir::SET_OPS), "{:?}", plan.features);
    assert_eq!(Plan::from_json(&plan.to_json().unwrap()).unwrap(), plan);
    let features: Vec<&str> = plan.features.iter().map(String::as_str).collect();
    let older: Vec<&str> = ir::FEATURES.iter().copied().filter(|f| *f != ir::SET_OPS).collect();
    let e = Plan::readable_by(plan.version as u64, &features, &older).err().expect("refused").to_string();
    assert!(e.contains("\"set_ops\""), "{e}");
    // One inside a subquery needs the feature too.
    let inner = db.explain("SELECT id FROM t WHERE x IN (SELECT x FROM u UNION ALL SELECT h FROM t)", &Params::new()).unwrap();
    assert!(inner.features.iter().any(|f| f == ir::SET_OPS), "{:?}", inner.features);
    let SqlResult::Rows(r) = db.sql(&format!("EXPLAIN {sql}"), &Params::new()).unwrap() else { panic!("rows") };
    let text: Vec<String> = r.rows.into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    let text = text.join("\n");
    for part in ["Except all", "  Union", "Limit"] {
        assert!(text.contains(part), "EXPLAIN lacks {part}:\n{text}");
    }
}
