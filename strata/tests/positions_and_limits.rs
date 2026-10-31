//! Column positions, traversal limits, literals and scores that read as
//! written. Review bugs of 30 October BUG-7 (`DISTINCT ON (3)` of two
//! columns), BUG-9 (the literal i64::MIN), BUG-11 (REACHABLE cut at
//! `max_nodes`), BUG-12 (`max_hops => 0` read as 1), BUG-14 (`score()`
//! over a WITH result without ORDER BY), and of 31 October BUG-N4 (`dot`
//! in a ranking expression) and BUG-N6 (a correlated subquery keyed on a
//! multi-valued outer column).

use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

fn open(path: &std::path::Path) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    Db::create(path, seed, DbOptions { sync: false, segment_max_docs: 3, ..DbOptions::default() }).unwrap()
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    sql_with(db, s, &Params::new())
}

fn sql_with(db: &Db, s: &str, p: &Params) -> Vec<Vec<Value>> {
    match db.sql(s, p).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

fn error(db: &Db, s: &str) -> String {
    match db.sql(s, &Params::new()) {
        Ok(_) => panic!("{s}: no error"),
        Err(e) => e.to_string(),
    }
}

fn ints(rows: &[Vec<Value>], col: usize) -> Vec<i64> {
    rows.iter().map(|r| r[col].as_i64().unwrap()).collect()
}

/// Review BUG-7: `DISTINCT ON (3)` and `ORDER BY 3` of two columns read 3
/// as a constant, so every row was one group; a number is a position.
#[test]
fn a_number_is_a_position() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("t.strata"));
    sql(&db, "CREATE TABLE t (id INT PRIMARY KEY, k KEYWORD)");
    sql(&db, "INSERT INTO t (id, k) VALUES (1, 'a'), (2, 'b'), (3, 'a'), (4, NULL)");
    for q in [
        "SELECT DISTINCT ON (3) k, id FROM t ORDER BY 1 LIMIT 10",
        "SELECT DISTINCT ON (1) k, id FROM t ORDER BY 3 LIMIT 10",
        "SELECT DISTINCT ON (0) k, id FROM t ORDER BY 1 LIMIT 10",
        "SELECT DISTINCT ON (k) k, id FROM t ORDER BY 1.5 LIMIT 10",
        "SELECT k, id FROM (SELECT * FROM t) d ORDER BY 3 LIMIT 10",
        "SELECT id FROM t ORDER BY 2 LIMIT 10",
    ] {
        let e = error(&db, q);
        assert!(e.contains("no such column"), "{q}: {e}");
    }
    let firsts = sql(&db, "SELECT DISTINCT ON (1) k, id FROM t ORDER BY 1, 2 LIMIT 10");
    assert_eq!(ints(&firsts, 1), vec![1, 2, 4]);
    let all = sql(&db, "SELECT DISTINCT ON (2) k, id FROM t ORDER BY 2 DESC LIMIT 10");
    assert_eq!(ints(&all, 1), vec![4, 3, 2, 1]);
}

/// Review BUG-9: `-9223372036854775808` parsed its number as a FLOAT
/// before the minus, so i64::MIN could not be written anywhere.
#[test]
fn the_smallest_integer_is_a_literal() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("t.strata"));
    const MIN: &str = "-9223372036854775808";
    sql(&db, &format!("CREATE TABLE t (id INT PRIMARY KEY, n INT, xs INT[], d INT DEFAULT {MIN})"));
    sql(&db, &format!("INSERT INTO t (id, n, xs) VALUES ({MIN}, {MIN}, ARRAY[{MIN}, 1]), (1, 0, NULL)"));
    let min = Value::Int(i64::MIN);
    let rows = sql(&db, &format!("SELECT id, n, xs, d FROM t WHERE id = {MIN}"));
    assert_eq!(rows, vec![vec![min.clone(), min.clone(), Value::List(vec![min.clone(), Value::Int(1)]), min.clone()]]);
    sql(&db, &format!("UPDATE t SET n = {MIN} WHERE id = 1"));
    assert_eq!(ints(&sql(&db, &format!("SELECT id FROM t WHERE n = {MIN} ORDER BY id")), 0), vec![i64::MIN, 1]);
    assert_eq!(ints(&sql(&db, &format!("SELECT id FROM (SELECT * FROM t) s WHERE n = {MIN} ORDER BY id")), 0), vec![i64::MIN, 1]);
    assert_eq!(sql(&db, &format!("SELECT id, n + 0 FROM t WHERE id = {MIN}")), vec![vec![min.clone(), min.clone()]]);
    sql(&db, &format!("DELETE FROM t WHERE id = {MIN}"));
    assert_eq!(ints(&sql(&db, "SELECT id FROM t"), 0), vec![1]);
    // One past it is still a FLOAT, which no INT holds.
    assert!(error(&db, "INSERT INTO t (id, n) VALUES (2, -9223372036854775809)").contains("Int"));
}

fn graph(dir: &std::path::Path) -> Db {
    let schema = Schema::new("g", vec![Field::new("id", FieldType::Int), Field::new("next", FieldType::Edges)], Some("id")).unwrap();
    let db = Db::create(&dir.join("g.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    // 0 -> 1, 0 -> 2, 1 -> 3, 2 -> 4, 3 -> 5
    sql(&db, "INSERT INTO g (id, next) VALUES (0, '[1, 2]'), (1, '[3]'), (2, '[4]'), (3, '[5]'), (4, NULL), (5, NULL)");
    db
}

/// Review BUG-11: REACHABLE at `max_nodes` counted the seeds and returned
/// a part of the rows that depended on the order of the edges; it now
/// counts the rows it reaches and refuses to cut them.
#[test]
fn reachable_does_not_cut_at_max_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let db = graph(dir.path());
    let q = |hops: u32, max: u32| format!("SELECT id FROM g WHERE REACHABLE(next, 0, hops => {hops}, max_nodes => {max}) ORDER BY id LIMIT 10");
    // Five rows within 3 hops of 0, two within 1.
    assert_eq!(ints(&sql(&db, &q(3, 5)), 0), vec![1, 2, 3, 4, 5]);
    assert_eq!(ints(&sql(&db, &q(1, 2)), 0), vec![1, 2]);
    for (hops, max) in [(3, 4), (3, 1), (1, 1)] {
        let e = error(&db, &q(hops, max));
        assert!(e.contains("max_nodes"), "{e}");
    }
    // PATH out of nodes before it finds the path says so, rather than
    // answering that there is none.
    let e = error(&db, "SELECT id FROM g WHERE PATH(next, 0, 5, max_nodes => 1) LIMIT 10");
    assert!(e.contains("max_nodes"), "{e}");
    assert_eq!(ints(&sql(&db, "SELECT id FROM g WHERE PATH(next, 0, 5, max_nodes => 3) ORDER BY id LIMIT 10"), 0), vec![0, 1, 3, 5]);
}

/// Review BUG-12: `max_hops => 0` was read as 1, and `k => 0` as 1; a
/// path of no hops joins a seed that is a target, and a limit of 0 is
/// refused.
#[test]
fn zero_hops_and_zero_limits() {
    let dir = tempfile::tempdir().unwrap();
    let db = graph(dir.path());
    assert_eq!(ints(&sql(&db, "SELECT id FROM g WHERE PATH(next, 0, 0, max_hops => 0) LIMIT 10"), 0), vec![0]);
    assert!(sql(&db, "SELECT id FROM g WHERE PATH(next, 0, 1, max_hops => 0) LIMIT 10").is_empty());
    assert_eq!(ints(&sql(&db, "SELECT id FROM g WHERE PATH(next, 0, 1, max_hops => 1) ORDER BY id LIMIT 10"), 0), vec![0, 1]);
    for q in [
        "SELECT id FROM g WHERE REACHABLE(next, 0, hops => 2, k => 0) LIMIT 10",
        "SELECT id FROM g WHERE REACHABLE(next, 0, hops => 2, max_nodes => 0) LIMIT 10",
        "SELECT id FROM g WHERE PATH(next, 0, 3, max_nodes => 0) LIMIT 10",
        "SELECT id FROM g WHERE REACHABLE(next, 0, max_nodes => 2.5) LIMIT 10",
    ] {
        let e = error(&db, q);
        assert!(e.contains("from 1"), "{q}: {e}");
    }
}

fn docs(dir: &std::path::Path) -> Db {
    let db = open(&dir.join("d.strata"));
    sql(&db, "CREATE TABLE d (id INT PRIMARY KEY, body TEXT, n INT, emb VECTOR(2) OPTIONS(index = 'flat'))");
    sql(
        &db,
        "INSERT INTO d (id, body, n, emb) VALUES (1, 'apple pie', 3, ARRAY[1.0, 0.0]), (2, 'apple apple cherry', 1, ARRAY[0.0, 1.0]), \
         (3, 'pear tart', 2, ARRAY[0.6, 0.8]), (4, 'apple crumble with apple', 5, NULL), (5, 'cherry', 4, ARRAY[0.8, 0.6])",
    );
    db
}

/// Review BUG-14: `score()` over a WITH result read with no ORDER BY was
/// 0.0; it is the score the WITH result ranked its rows by, in its order.
#[test]
fn score_over_a_with_result_is_its_ranking() {
    let dir = tempfile::tempdir().unwrap();
    let db = docs(dir.path());
    let with = "WITH top AS (SELECT * FROM d ORDER BY bm25(body, 'apple') DESC LIMIT 3)";
    let ranked = sql(&db, &format!("{with} SELECT id, score() FROM top ORDER BY bm25(body, 'apple') DESC"));
    assert_eq!(ranked.len(), 3);
    assert!(ranked.iter().all(|r| r[1].as_f64().unwrap() > 0.0), "{ranked:?}");
    assert_eq!(sql(&db, &format!("{with} SELECT id, score() FROM top")), ranked);
    assert_eq!(sql(&db, &format!("{with} SELECT id, score() FROM top LIMIT 2")), ranked[..2].to_vec());
    let plain = sql(&db, "SELECT id, score() FROM d ORDER BY bm25(body, 'apple') DESC LIMIT 3");
    assert_eq!(ranked, plain);
    // Ranked by a column, score() is the column, as over the table.
    let by_n = sql(&db, "WITH top AS (SELECT * FROM d ORDER BY n DESC LIMIT 2) SELECT id, score() FROM top");
    assert_eq!(by_n, vec![vec![Value::Int(4), Value::Float(5.0)], vec![Value::Int(5), Value::Float(4.0)]]);
}

/// Review BUG-N4: the documented `bm25(..) + 2 * coalesce(dot(emb, ..),
/// 0)` failed with "unknown score function dot"; in a ranking, `dot` is
/// the field's dot product with the vector, NULL without one.
#[test]
fn dot_ranks_as_a_number() {
    let dir = tempfile::tempdir().unwrap();
    let db = docs(dir.path());
    let mut p = Params::new();
    p.0.insert("v".into(), Value::Vector(vec![1.0f32, 0.0].into()));
    let bm25: std::collections::HashMap<i64, f64> =
        sql(&db, "SELECT id, score() FROM d ORDER BY bm25(body, 'apple') DESC LIMIT 10").iter().map(|r| (r[0].as_i64().unwrap(), r[1].as_f64().unwrap())).collect();
    let dot = |id: i64| match id {
        1 => 1.0,
        3 => 0.6,
        5 => 0.8,
        _ => 0.0,
    };
    let mut want: Vec<(i64, f64)> = (1..=5).map(|id| (id, bm25.get(&id).copied().unwrap_or(0.0) + 2.0 * dot(id))).collect();
    want.sort_by(|a, b| b.1.total_cmp(&a.1));
    for q in [
        "SELECT id, score() FROM d ORDER BY bm25(body, 'apple') + 2 * coalesce(dot(emb, :v), 0) DESC LIMIT 10",
        "SELECT id, score() FROM d ORDER BY bm25(body, 'apple') + 2 * coalesce(dot(:v, emb), 0) DESC LIMIT 10",
        "WITH top AS (SELECT * FROM d ORDER BY bm25(body, 'apple') DESC LIMIT 10) SELECT id, score() FROM top ORDER BY bm25(body, 'apple') + 2 * coalesce(dot(emb, :v), 0) DESC LIMIT 10",
    ] {
        let got: Vec<(i64, f64)> = sql_with(&db, q, &p).iter().map(|r| (r[0].as_i64().unwrap(), r[1].as_f64().unwrap())).collect();
        let want: Vec<(i64, f64)> = want.iter().copied().filter(|(id, _)| !q.starts_with("WITH") || bm25.contains_key(id)).collect();
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), want.iter().map(|w| w.0).collect::<Vec<_>>(), "{q}");
        for (g, w) in got.iter().zip(&want) {
            assert!((g.1 - w.1).abs() < 1e-5, "{q}: {got:?} vs {want:?}");
        }
    }
    let e = db.sql("SELECT id FROM d ORDER BY dot(emb) DESC LIMIT 3", &p).unwrap_err().to_string();
    assert!(e.contains("dot(") && e.contains("two"), "{e}");
}

/// Review BUG-N6: a correlated subquery that compared with a
/// multi-valued column of the outer row found nothing for every row; it
/// compares with its values, as a filter on the field does.
#[test]
fn a_subquery_keyed_on_a_multi_valued_outer_column() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("t.strata"));
    sql(&db, "CREATE TABLE t (id INT PRIMARY KEY, tags KEYWORD[], sizes INT[])");
    sql(&db, "INSERT INTO t (id, tags, sizes) VALUES (1, ARRAY['x', 'y'], ARRAY[1, 5]), (2, NULL, NULL), (3, ARRAY['z'], ARRAY[9]), (4, ARRAY['y'], ARRAY[2])");
    sql(&db, "CREATE TABLE b (id INT PRIMARY KEY, k KEYWORD, n INT)");
    sql(&db, "INSERT INTO b (id, k, n) VALUES (1, 'x', 1), (2, 'y', 2), (3, 'y', 3), (4, 'q', 4), (5, NULL, NULL)");
    let counts = |q: &str| -> Vec<(i64, f64)> { sql(&db, q).iter().map(|r| (r[0].as_i64().unwrap(), r[1].as_f64().unwrap())).collect() };
    assert_eq!(counts("SELECT id, (SELECT COUNT(*) FROM b WHERE b.k = t.tags) FROM t ORDER BY id"), vec![(1, 3.0), (2, 0.0), (3, 0.0), (4, 2.0)]);
    // None of the values: rows of b with a k other than all of them.
    assert_eq!(counts("SELECT id, (SELECT COUNT(*) FROM b WHERE b.k != t.tags) FROM t ORDER BY id"), vec![(1, 1.0), (2, 0.0), (3, 4.0), (4, 2.0)]);
    // Below one of the values.
    assert_eq!(counts("SELECT id, (SELECT COUNT(*) FROM b WHERE b.n < t.sizes) FROM t ORDER BY id"), vec![(1, 4.0), (2, 0.0), (3, 4.0), (4, 1.0)]);
    assert_eq!(counts("SELECT id, (SELECT COUNT(*) FROM b WHERE t.sizes > b.n) FROM t ORDER BY id"), vec![(1, 4.0), (2, 0.0), (3, 4.0), (4, 1.0)]);
    // As a ranking.
    assert_eq!(ints(&sql(&db, "SELECT id FROM t ORDER BY (SELECT COUNT(*) FROM b WHERE b.k = t.tags) DESC, id LIMIT 2"), 0), vec![1, 4]);
}
