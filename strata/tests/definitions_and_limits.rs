//! Definitions and options checked when written, and values compared as
//! their columns hold them. Review bugs of 30 October BUG-15 (a rank
//! profile on an unknown column, a column named "", CURRENT_TIMESTAMP in
//! a write, `k = 1` on a KEYWORD, ORDER BY a KEYWORD) and of 31 October
//! BUG-N10 (`AT_LEAST('0%')`, `max_rows => 0`, `min_rows` above
//! `max_rows`, `MAX k PER` a TEXT field).

use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

fn db(path: &std::path::Path) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&path.join("t.strata"), seed, DbOptions { sync: false, segment_max_docs: 2, ..DbOptions::default() }).unwrap();
    sql(&db, "CREATE TABLE t (id INT PRIMARY KEY, n INT, k KEYWORD, s TEXT, tags KEYWORD[])");
    sql(&db, "INSERT INTO t (id, n, k, s, tags) VALUES (1, 3, 'b', 'apple pie', ARRAY['x']), (2, NULL, NULL, 'apple cherry tart', NULL), (3, 2, 'a', 'vaccine trial', ARRAY['x', 'y']), (4, 4, 'b', 'cherry jam', NULL), (5, 1, '1', 'mrna vaccine', NULL)");
    db
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
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

fn ids(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter().map(|r| r[0].as_i64().unwrap()).collect()
}

/// Review BUG-15: `CREATE RANK PROFILE` took a ranking by a column the
/// table does not have, and failed only when a query ranked by it.
#[test]
fn a_rank_profile_names_the_tables_columns() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    for (q, want) in [
        ("CREATE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY nosuch DESC", "no such column nosuch in t"),
        ("CREATE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY bm25(body, :q) DESC", "no such column body in t"),
        ("CREATE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY bm25(s, :q) * (1 + :w * nosuch) DESC WITH (w => 0.5)", "no such column nosuch"),
        ("CREATE RANK PROFILE p ON t AS WITH top AS (SELECT * FROM t ORDER BY bm25(s, :q) DESC LIMIT 10) SELECT * FROM top ORDER BY t.nosuch DESC", "no such column nosuch"),
        ("CREATE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY k DESC", "ranks by k, which is not a number"),
        ("CREATE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY tags DESC", "ranks by tags, which is not a number"),
    ] {
        let e = error(&db, q);
        assert!(e.contains(want), "{q}: {e}");
    }
    // A profile of the table's columns is kept, and ranks.
    sql(&db, "CREATE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY bm25(s, :q) * (1 + :w * n) DESC WITH (w => 0.5)");
    sql(&db, "CREATE RANK PROFILE recent ON t AS SELECT * FROM t ORDER BY n");
    assert!(!sql(&db, "SELECT id FROM t ORDER BY rank_profile('p', q => 'apple') LIMIT 10").is_empty());
    assert_eq!(ids(&sql(&db, "SELECT id FROM t WHERE n IS NOT NULL ORDER BY rank_profile('recent') LIMIT 2")), vec![5, 3]);
}

/// Review BUG-15: a column named "" was created.
#[test]
fn a_column_has_a_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    for q in ["CREATE TABLE e (id INT PRIMARY KEY, \"\" INT)", "CREATE TABLE e (\"\" INT PRIMARY KEY, x INT)", "CREATE TABLE e (id INT PRIMARY KEY, \" \" KEYWORD)"] {
        let e = error(&db, q);
        assert!(e.contains("empty name"), "{q}: {e}");
    }
    assert!(!sql(&db, "SELECT COUNT(*) FROM t").is_empty());
}

/// Review BUG-15: `ON CONFLICT ... DO UPDATE SET at = CURRENT_TIMESTAMP`
/// failed (`unknown function current_timestamp`), and so did the time as
/// an INSERT's value and in `UPDATE ... SET`; a column's DEFAULT took it.
#[test]
fn the_time_of_the_statement_is_a_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    sql(&db, "CREATE TABLE c (id INT PRIMARY KEY, at TIMESTAMP, n INT)");
    let before = strata::timestamp::now();
    sql(&db, "INSERT INTO c (id, at) VALUES (1, CURRENT_TIMESTAMP), (2, now()), (3, LOCALTIMESTAMP), (4, '2020-01-01')");
    sql(&db, "INSERT INTO c (id, at) VALUES (4, '2021-01-01') ON CONFLICT (id) DO UPDATE SET at = CURRENT_TIMESTAMP");
    sql(&db, "INSERT INTO c (id, at) VALUES (5, '2020-01-01')");
    sql(&db, "UPDATE c SET at = now() WHERE id = 5");
    let after = strata::timestamp::now();
    let rows = sql(&db, "SELECT id, at FROM c ORDER BY id LIMIT 10");
    assert_eq!(rows.len(), 5);
    for r in &rows {
        let Value::Timestamp(at) = r[1] else { panic!("{r:?}") };
        assert!((before..=after).contains(&at), "{r:?} not within {before}..{after}");
    }
    // One statement, one time.
    assert_eq!(rows[0][1], rows[1][1]);
    assert_eq!(rows[0][1], rows[2][1]);
    // A condition reads it too.
    assert_eq!(ids(&sql(&db, "SELECT id FROM c WHERE at <= CURRENT_TIMESTAMP ORDER BY id LIMIT 10")), vec![1, 2, 3, 4, 5]);
    // It is a time, not a number.
    assert!(error(&db, "INSERT INTO c (id, n) VALUES (6, CURRENT_TIMESTAMP)").contains("does not fit"));
}

/// Review BUG-15: `WHERE k = 1` on a KEYWORD compared with the text '1',
/// while over derived rows it is an error; a number is not a KEYWORD's
/// value in a condition, as text is not an INT's. A write still stores
/// it as text.
#[test]
fn a_keyword_is_compared_with_text() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    for q in [
        "SELECT id FROM t WHERE k = 1 LIMIT 10",
        "SELECT id FROM t WHERE k != 1 LIMIT 10",
        "SELECT id FROM t WHERE k IN ('a', 1) LIMIT 10",
        "SELECT id FROM t WHERE k NOT IN (1.5) LIMIT 10",
        "SELECT id FROM t WHERE tags = 1 LIMIT 10",
        "SELECT id FROM (SELECT * FROM t) d WHERE k = 1 LIMIT 10",
    ] {
        let e = error(&db, q);
        assert!(e.contains("KEYWORD") || e.contains("cannot compare"), "{q}: {e}");
    }
    assert_eq!(ids(&sql(&db, "SELECT id FROM t WHERE k = '1' LIMIT 10")), vec![5]);
    let p = Params::new().set("k", Value::Int(1));
    assert!(db.sql("SELECT id FROM t WHERE k = :k LIMIT 10", &p).unwrap_err().to_string().contains("KEYWORD"));
    sql(&db, "INSERT INTO t (id, k) VALUES (6, 7)");
    assert_eq!(ids(&sql(&db, "SELECT id FROM t WHERE k = '7' LIMIT 10")), vec![6]);
}

/// Review BUG-15: ORDER BY a KEYWORD was refused ("ORDER BY a column
/// needs a numeric field"); it sorts by the text, NULL last either way,
/// as over derived rows.
#[test]
fn rows_order_by_a_keyword() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let both = |q: &str| {
        let table = ids(&sql(&db, &q.replace("{t}", "t")));
        let derived = ids(&sql(&db, &q.replace("{t}", "(SELECT * FROM t) d")));
        assert_eq!(table, derived, "{q}");
        table
    };
    assert_eq!(both("SELECT id, k FROM {t} ORDER BY k, id LIMIT 10"), vec![5, 3, 1, 4, 2]);
    assert_eq!(both("SELECT id, k FROM {t} ORDER BY k DESC, id LIMIT 10"), vec![1, 4, 3, 5, 2]);
    assert_eq!(both("SELECT id FROM {t} ORDER BY k DESC, id LIMIT 2"), vec![1, 4]);
    assert_eq!(both("SELECT id FROM {t} ORDER BY k, id LIMIT 2 OFFSET 1"), vec![3, 1]);
    assert_eq!(ids(&sql(&db, "SELECT id FROM t WHERE MATCH(s, 'apple OR cherry') ORDER BY k, id LIMIT 10")), vec![1, 4, 2]);
    assert_eq!(both("SELECT id FROM {t} ORDER BY s LIMIT 10"), vec![2, 1, 4, 5, 3]);
    // A score or a number first, then the key, as before.
    assert_eq!(ids(&sql(&db, "SELECT id FROM t ORDER BY n DESC, id LIMIT 3")), vec![4, 1, 3]);
    assert!(error(&db, "SELECT id FROM t ORDER BY tags LIMIT 10").contains("multi-valued"));
}

/// Review BUG-N10: a count of 0 (`AT_LEAST(0, ...)`, `'0%'`, `min => 0`,
/// `)@0`) kept every row, `max_rows => 0` was taken, and so was a
/// `min_rows` above `max_rows`, which no level can meet.
#[test]
fn counts_and_guards_are_one_or_more() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    for (q, want) in [
        ("SELECT id FROM t WHERE AT_LEAST(0, MATCH(s, 'apple'), MATCH(s, 'cherry')) LIMIT 10", "above 0"),
        ("SELECT id FROM t WHERE AT_LEAST('0%', MATCH(s, 'apple'), MATCH(s, 'cherry')) LIMIT 10", "above 0"),
        ("SELECT id FROM (SELECT * FROM t) d WHERE AT_LEAST('0%', d.n > 1, d.n > 2) LIMIT 10", "above 0"),
        ("SELECT id FROM t WHERE MATCH(s, 'apple cherry', min => 0) LIMIT 10", "above 0"),
        ("SELECT id FROM t WHERE MATCH(s, 'apple cherry', min => '0%') LIMIT 10", "above 0"),
        ("SELECT id FROM t WHERE MATCH(s, '(apple cherry)@0') LIMIT 10", "above 0"),
        ("SELECT id FROM t WHERE MATCH(s, '(apple cherry)@0%') LIMIT 10", "above 0"),
        ("SELECT id FROM t ORDER BY bm25(s, 'apple cherry', min => 0) DESC LIMIT 10", "above 0"),
        ("SELECT id FROM t WHERE AT_LEAST(1, MATCH(s, 'apple'), MATCH(s, 'cherry'), max_rows => 0) LIMIT 10", "1 or more"),
        ("SELECT id FROM t WHERE AT_LEAST(1, MATCH(s, 'apple'), MATCH(s, 'cherry'), min_rows => 0) LIMIT 10", "1 or more"),
        ("SELECT id FROM t ORDER BY bm25(s, 'apple cherry', min => 1, max_rows => 0) DESC LIMIT 10", "1 or more"),
        ("SELECT id FROM t WHERE AT_LEAST(1, MATCH(s, 'apple'), MATCH(s, 'cherry'), min_rows => 5, max_rows => 2) LIMIT 10", "above max_rows"),
        ("SELECT id FROM t WHERE MATCH(s, 'apple cherry', min => 1, min_rows => 5, max_rows => 2) LIMIT 10", "above max_rows"),
    ] {
        let e = error(&db, q);
        assert!(e.contains(want), "{q}: {e}");
    }
    // The smallest counts still read: 1, 1% of the items, and equal bounds.
    assert_eq!(ids(&sql(&db, "SELECT id FROM t WHERE AT_LEAST(1, MATCH(s, 'apple'), MATCH(s, 'cherry')) ORDER BY id LIMIT 10")), vec![1, 2, 4]);
    assert_eq!(ids(&sql(&db, "SELECT id FROM t WHERE MATCH(s, '(apple cherry)@1%') ORDER BY id LIMIT 10")), vec![1, 2, 4]);
    assert_eq!(ids(&sql(&db, "SELECT id FROM t WHERE AT_LEAST(1, MATCH(s, 'apple'), MATCH(s, 'cherry'), min_rows => 2, max_rows => 2) ORDER BY id LIMIT 10")), vec![2]);
}

/// Review BUG-N10: `MAX k PER` a TEXT field was taken, grouping rows by
/// their whole text, which rows rarely share.
#[test]
fn max_per_caps_a_keyword_or_a_number() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    assert!(error(&db, "SELECT id FROM t ORDER BY id MAX 1 PER s LIMIT 10").contains("is TEXT"));
    assert_eq!(ids(&sql(&db, "SELECT id FROM t ORDER BY id MAX 1 PER k LIMIT 10")), vec![1, 2, 3, 5]);
}
