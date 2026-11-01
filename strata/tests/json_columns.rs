//! Paths of JSON fields read outside WHERE: `GROUP BY attrs.color`,
//! aggregates of `attrs.price`, `ORDER BY attrs.price` and paths selected
//! for a page of rows. The executor reads them from the field's terms
//! (each row's one value, and the object of a row the terms do not give
//! exactly: an object, an array, a long string, `5.0`, an integer beyond
//! 2^53), and its answer is the one the operators give reading every
//! row's object: the same groups with the same aggregates, the same rows
//! in the same order. Checked on rows in the memtable, across flushed
//! segments, after a merge, after deletes and updates, and after reopening.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as Json};
use strata::db::{Db, SqlResult};
use strata::query::compound::plan_read_by_rows;
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::value::Value;

fn seed() -> Schema {
    Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap()
}

fn opts() -> DbOptions {
    DbOptions { sync: false, layout: Layout::SingleFile, segment_max_docs: 150, ..DbOptions::default() }
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

/// A value of each kind the terms cannot give exactly, and many they can.
fn object(rng: &mut StdRng, i: usize) -> Option<Json> {
    if rng.gen_ratio(1, 12) {
        return None;
    }
    let pick = |rng: &mut StdRng, xs: &[Json]| xs[rng.gen_range(0..xs.len())].clone();
    let long = json!(format!("long{}", "y".repeat(260 + i % 3)));
    let mut m = serde_json::Map::new();
    let mut put = |k: &str, p: (u32, u32), v: Json, rng: &mut StdRng| {
        if rng.gen_ratio(p.0, p.1) {
            m.insert(k.to_string(), v);
        }
    };
    let c = pick(rng, &[json!("red"), json!("blue"), json!("green"), json!("red"), json!("n/a"), long.clone()]);
    put("c", (3, 4), c, rng);
    let p = pick(
        rng,
        &[json!(1), json!(5), json!(5.0), json!(-3), json!(2.25), json!(-0.5), json!(9007199254740993u64), json!(-9007199254740993i64), json!(1e300), json!(12345), json!("n/a"), json!(true), json!(null)],
    );
    put("p", (2, 3), p, rng);
    let b = pick(rng, &[json!(true), json!(false), json!(true), json!(1), json!(0)]);
    put("b", (1, 2), b, rng);
    let l = pick(rng, &[json!([1, 2]), json!([2, 2]), json!(["x", "y", "x"]), json!([]), json!([[1]]), json!([[]]), json!([1, "a"]), json!([true, 1]), json!([3]), json!(["x"])]);
    put("l", (1, 3), l, rng);
    let o = match rng.gen_range(0..4) {
        0 => json!({}),
        1 => json!({ "w": pick(rng, &[json!(1), json!(4), json!(10.5), json!("wide")]) }),
        _ => json!({ "w": pick(rng, &[json!(1), json!(4), json!(40)]), "h": pick(rng, &[json!(2), json!(7.25)]) }),
    };
    put("o", (1, 2), o, rng);
    let mixed = pick(rng, &[json!("s"), json!(7), json!(false), json!({ "k": 1 }), json!([5, 6]), json!(7.0), long]);
    put("m", (1, 3), mixed, rng);
    put("z", (1, 40), json!(rng.gen_range(0..3)), rng);
    let items = match rng.gen_range(0..3) {
        0 => json!([{ "k": "x" }, { "k": "y", "v": 3 }]),
        1 => json!([{ "k": "x" }, { "k": "x" }]),
        _ => json!([{ "v": 1 }]),
    };
    put("items", (1, 3), items, rng);
    Some(Json::Object(m))
}

/// Inserts rows `from..to`, flushing every `flush` rows (0: never).
fn fill(db: &Db, rng: &mut StdRng, from: usize, to: usize, flush: usize) {
    for i in from..to {
        let a = object(rng, i).map_or("NULL".to_string(), |j| format!("'{}'", j.to_string().replace('\'', "''")));
        sql(db, &format!("INSERT INTO t (id, n, attrs) VALUES ('r{i:04}', {}, {a})", i % 7));
        if flush > 0 && (i + 1) % flush == 0 {
            db.storage().flush().unwrap();
        }
    }
}

/// The paths the queries read, as SQL names them.
const PATHS: [&str; 11] = ["attrs.c", "attrs.p", "attrs.b", "attrs.l", "attrs.o", "attrs.o.w", "attrs.m", "attrs.z", "attrs.items.k", "attrs.absent", "json_extract(attrs, 'p', 'number')"];

/// The queries checked, and whether each runs as one leaf (the Sort a
/// GROUP BY without an ORDER BY of its own gets, by count then each
/// column, stays an operator over the leaf's groups).
fn queries() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for p in PATHS {
        // Counts of a path's values (from its postings), and of the rows a
        // filter keeps.
        out.push((format!("SELECT {p}, COUNT(*) FROM t GROUP BY {p}"), false));
        out.push((format!("SELECT {p}, COUNT(*) FROM t WHERE attrs.c = 'red' OR n < 2 GROUP BY {p} ORDER BY COUNT(*) DESC, {p}"), true));
        // Aggregates of a path.
        out.push((format!("SELECT COUNT(*), COUNT({p}), MIN({p}), MAX({p}), SUM({p}), AVG({p}), COUNT(DISTINCT {p}) FROM t"), false));
        out.push((format!("SELECT COUNT({p}), MIN({p}), MAX({p}), SUM({p}) FROM t WHERE n >= 3"), false));
        // Grouped by a path and a field, aggregates of another path.
        out.push((format!("SELECT n, {p}, COUNT(*), SUM(attrs.p), MIN(attrs.o.w), MAX(attrs.c) FROM t GROUP BY n, {p}"), false));
        out.push((format!("SELECT {p}, attrs.b, COUNT(*), AVG(attrs.o.h) FROM t GROUP BY {p}, attrs.b ORDER BY COUNT(*)"), false));
        out.push((format!("SELECT {p}, COUNT(*) FROM t GROUP BY {p} HAVING COUNT(*) > 20 ORDER BY COUNT(*) DESC, {p}"), true));
        // Rows by a path, ties by key, the path among the columns or not.
        out.push((format!("SELECT id, {p} FROM t ORDER BY {p}, id LIMIT 40"), true));
        out.push((format!("SELECT id, {p}, attrs.o FROM t WHERE n <> 4 ORDER BY {p} DESC, id LIMIT 25 OFFSET 7"), true));
        out.push((format!("SELECT id, n FROM t WHERE attrs.c IS NOT NULL ORDER BY {p}, id LIMIT 30"), false));
        out.push((format!("SELECT id, {p} FROM t ORDER BY {p} DESC NULLS FIRST, id LIMIT 30"), true));
        out.push((format!("SELECT id, {p} FROM t ORDER BY {p} LIMIT 50"), true));
        out.push((format!("SELECT id, {p} FROM t WHERE n = 1 ORDER BY {p} DESC, id"), true));
    }
    out
}

/// Rows as a multiset, for groups (whose order the query leaves open
/// between values that compare equal, as 5 and 5.0 do).
fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut out: Vec<String> = rows.drain(..).map(|r| format!("{r:?}")).collect();
    out.sort();
    out
}

/// Every query's answer, against the plan that reads each row's object.
fn check(db: &Db, stage: &str) {
    let catalog = db.catalog();
    let tables: Vec<&Schema> = catalog.iter().map(|t| &**t).collect();
    let (mut leaves, mut objects_only) = (0, 0);
    for (q, leaf) in queries() {
        let got = db.query(&q, &Params::new()).unwrap_or_else(|e| panic!("{stage}: {q}: {e}"));
        let plan = db.explain(&q, &Params::new()).unwrap();
        assert!(plan.features.iter().any(|f| f == "json_columns"), "{stage}: {q}: the executor reads the path: {:?}", plan.features);
        if leaf {
            assert!(plan.root.is_leaf(), "{stage}: {q}: one leaf: {}", serde_json::to_string(&plan.root).unwrap());
            leaves += 1;
        }
        let by_rows = plan_read_by_rows(&q, &tables, &Params::new()).unwrap();
        assert!(!by_rows.features.iter().any(|f| f == "json_columns"), "{q}");
        let SqlResult::Rows(want) = db.run(&by_rows).unwrap_or_else(|e| panic!("{stage}: {q}, by rows: {e}")) else { panic!("rows") };
        assert_eq!(got.columns, want.columns, "{stage}: {q}");
        if q.contains("GROUP BY") {
            assert_eq!(sorted(got.rows), sorted(want.rows), "{stage}: {q}");
        } else {
            assert_eq!(got.rows, want.rows, "{stage}: {q}");
        }
        if got.stats.plan.iter().any(|l| l.contains("read from their objects")) {
            objects_only += 1;
        }
    }
    assert!(leaves > 60, "{leaves}");
    assert!(objects_only > 0, "some rows' values are read from their objects");
}

#[test]
fn paths_read_from_terms_answer_as_rows_read_one_by_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.strata");
    let mut rng = StdRng::seed_from_u64(5);
    {
        let db = Db::create(&path, seed(), opts()).unwrap();
        sql(&db, "CREATE TABLE t (id KEYWORD PRIMARY KEY, n INT, attrs JSON)");
        // In the memtable.
        fill(&db, &mut rng, 0, 120, 0);
        check(&db, "memtable");
        // Across flushed segments and the memtable.
        fill(&db, &mut rng, 120, 900, 170);
        check(&db, "segments");
        db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
        check(&db, "merged");
        // Deleted and rewritten rows.
        sql(&db, "DELETE FROM t WHERE n = 6 OR attrs.c = 'green'");
        sql(&db, "UPDATE t SET attrs = '{\"c\": \"red\", \"p\": 5.0, \"l\": [2, 2]}' WHERE n = 5");
        check(&db, "deletes and updates");
    }
    let db = Db::open(&path, opts()).unwrap();
    check(&db, "reopened");
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy::default()).unwrap();
    check(&db, "reopened and merged");
}

#[test]
fn a_path_of_a_bool_groups_as_booleans() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("b.strata"), seed(), opts()).unwrap();
    sql(&db, "CREATE TABLE t (id KEYWORD PRIMARY KEY, n INT, attrs JSON)");
    for (i, v) in ["true", "false", "true", "1", "null"].iter().enumerate() {
        sql(&db, &format!("INSERT INTO t (id, n, attrs) VALUES ('r{i}', 0, '{{\"ok\": {v}}}')"));
    }
    db.storage().flush().unwrap();
    let got = sorted(sql(&db, "SELECT attrs.ok, COUNT(*) FROM t GROUP BY attrs.ok"));
    let want = sorted(vec![
        vec![Value::Bool(true), Value::Int(2)],
        vec![Value::Bool(false), Value::Int(1)],
        vec![Value::Int(1), Value::Int(1)],
        vec![Value::Null, Value::Int(1)],
    ]);
    assert_eq!(got, want);
}
