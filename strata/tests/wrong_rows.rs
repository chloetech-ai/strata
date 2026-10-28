//! Queries that returned the wrong rows (the engineering review's
//! reproduced bugs 2, 5 and 6): integers compared through doubles, NULLs
//! read as values, and ORDER BY dropping rows without a value.

use std::sync::Arc;

use strata::cluster;
use strata::db::{Db, SqlResult};
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

const BIG: i64 = 1 << 53; // 2^53: the first integer after it a double cannot hold

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Int),
            Field::new("n", FieldType::Int),
            Field::new("tags", FieldType::Int).multi(),
            Field::new("kind", FieldType::Keyword),
            Field::new("rating", FieldType::Float),
            Field::new("body", FieldType::Text { positions: false }),
        ],
        Some("id"),
    )
    .unwrap()
}

fn row(id: i64, n: Option<i64>, tags: &[i64], kind: Option<&str>, rating: Option<f64>) -> Vec<Value> {
    vec![
        Value::Int(id),
        n.map_or(Value::Null, Value::Int),
        if tags.is_empty() { Value::Null } else { Value::List(tags.iter().map(|&t| Value::Int(t)).collect()) },
        kind.map_or(Value::Null, |k| k.into()),
        rating.map_or(Value::Null, Value::Float),
        "apple".into(),
    ]
}

fn db_with(rows: Vec<Vec<Value>>) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("t.strata"), schema(), DbOptions::default()).unwrap();
    db.load(rows).unwrap();
    (dir, db)
}

/// Rows whose ids, `n` and tags sit on both sides of 2^53, and the i64
/// limits; rows 5 and 6 have NULLs.
fn sample() -> (tempfile::TempDir, Db) {
    db_with(vec![
        row(BIG, Some(BIG), &[BIG], Some("a"), Some(1.0)),
        row(BIG + 1, Some(BIG + 1), &[BIG + 1], Some("b"), Some(2.0)),
        row(i64::MAX, Some(i64::MAX), &[i64::MAX, 1], Some("a"), Some(3.0)),
        row(i64::MIN, Some(i64::MIN), &[i64::MIN], Some("b"), Some(4.0)),
        row(5, None, &[], None, None),
        row(6, Some(6), &[6], None, None),
    ])
}

fn ids(r: &ResultSet) -> Vec<i64> {
    let mut v: Vec<i64> = r.rows.iter().map(|row| row[0].as_i64().unwrap()).collect();
    v.sort_unstable();
    v
}

fn ordered(r: &ResultSet) -> Vec<i64> {
    r.rows.iter().map(|row| row[0].as_i64().unwrap()).collect()
}

fn q(db: &Db, sql: &str) -> Vec<i64> {
    ids(&db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")))
}

// Bug 2: `WHERE id = 9007199254740993` matched (and DELETE deleted) the row
// with id 2^53, because both sides went through a double.
#[test]
fn integers_compare_exactly() {
    let (_d, db) = sample();
    let (b, b1) = (BIG, BIG + 1);
    for (sql, want) in [
        (format!("SELECT id FROM t WHERE id = {b1}"), vec![b1]),
        (format!("SELECT id FROM t WHERE n = {b1}"), vec![b1]),
        (format!("SELECT id FROM t WHERE n = {b}"), vec![b]),
        (format!("SELECT id FROM t WHERE n > {b}"), vec![b1, i64::MAX]),
        (format!("SELECT id FROM t WHERE n >= {b1}"), vec![b1, i64::MAX]),
        (format!("SELECT id FROM t WHERE n < {b1}"), vec![i64::MIN, 6, b]),
        (format!("SELECT id FROM t WHERE n <= {b}"), vec![i64::MIN, 6, b]),
        (format!("SELECT id FROM t WHERE n BETWEEN {b1} AND {b1}"), vec![b1]),
        (format!("SELECT id FROM t WHERE n IN ({b1}, 7)"), vec![b1]),
        (format!("SELECT id FROM t WHERE tags = {b1}"), vec![b1]),
        (format!("SELECT id FROM t WHERE tags > {b}"), vec![b1, i64::MAX]),
        (format!("SELECT id FROM t WHERE tags IN ({b1})"), vec![b1]),
        // The i64 limits and bounds past them.
        ("SELECT id FROM t WHERE n = 9223372036854775807".into(), vec![i64::MAX]),
        ("SELECT id FROM t WHERE n > 9223372036854775806".into(), vec![i64::MAX]),
        ("SELECT id FROM t WHERE n > 9223372036854775807".into(), vec![]),
        ("SELECT id FROM t WHERE n >= 9223372036854775808".into(), vec![]),
        ("SELECT id FROM t WHERE n < 1e30".into(), vec![i64::MIN, 6, b, b1, i64::MAX]),
        ("SELECT id FROM t WHERE n = -9223372036854775808".into(), vec![i64::MIN]),
        ("SELECT id FROM t WHERE n < -9223372036854775807".into(), vec![i64::MIN]),
        ("SELECT id FROM t WHERE n < -9223372036854775808".into(), vec![]),
        ("SELECT id FROM t WHERE tags >= 9223372036854775807".into(), vec![i64::MAX]),
        ("SELECT id FROM t WHERE tags <= -9223372036854775808".into(), vec![i64::MIN]),
        // Fractions bound integers by rounding inward.
        ("SELECT id FROM t WHERE n = 5.5".into(), vec![]),
        ("SELECT id FROM t WHERE n > 5.5 AND n < 6.5".into(), vec![6]),
        ("SELECT id FROM t WHERE n BETWEEN 5.5 AND 6.5".into(), vec![6]),
    ] {
        assert_eq!(q(&db, &sql), want, "{sql}");
    }
    let count = db.query(&format!("SELECT COUNT(*) FROM t WHERE n = {b1}"), &Params::new()).unwrap();
    assert_eq!(count.rows[0][0], Value::Int(1));

    // Sums of integers are exact, and beyond 64 bits become a FLOAT
    // rather than wrapping.
    let sum = |sql: &str| db.query(sql, &Params::new()).unwrap().rows[0][0].clone();
    assert_eq!(sum(&format!("SELECT SUM(n) FROM t WHERE id IN ({b}, {b1})")), Value::Int(2 * b + 1));
    assert_eq!(sum("SELECT SUM(n) FROM t WHERE n > 0"), Value::Float(i64::MAX as f64 + (2 * b + 1 + 6) as f64));

    // Parameters too, a single one and a list (`IN (:ids)`).
    let p = Params::new().set("x", Value::Int(b1)).set("xs", Value::List(vec![Value::Int(b1), Value::Int(i64::MAX)]));
    assert_eq!(ids(&db.query("SELECT id FROM t WHERE id = :x", &p).unwrap()), vec![b1]);
    assert_eq!(ids(&db.query("SELECT id FROM t WHERE n IN (:xs)", &p).unwrap()), vec![b1, i64::MAX]);

    // DELETE removes 2^53 + 1 and nothing else.
    let n = db.sql(&format!("DELETE FROM t WHERE id = {b1}"), &Params::new()).unwrap();
    assert!(matches!(n, SqlResult::Written(1)));
    assert_eq!(q(&db, "SELECT id FROM t"), vec![i64::MIN, 5, 6, b, i64::MAX]);
}

// Values written in place are compared exactly too, and so are
// increments.
#[test]
fn in_place_integers_compare_exactly() {
    let (_d, db) = sample();
    let p = Params::new();
    db.sql(&format!("UPDATE t SET n = {} WHERE id = 6", BIG + 1), &p).unwrap();
    assert_eq!(q(&db, &format!("SELECT id FROM t WHERE n = {}", BIG + 1)), vec![6, BIG + 1]);
    assert_eq!(q(&db, &format!("SELECT id FROM t WHERE n IN ({})", BIG + 1)), vec![6, BIG + 1]);
    db.sql("UPDATE t SET n = n + 9007199254740993 WHERE id = 5", &p).unwrap();
    db.sql("UPDATE t SET n = n - 2 WHERE id = 6", &p).unwrap();
    let r = db.query("SELECT id, n FROM t WHERE id IN (5, 6)", &p).unwrap();
    let got: Vec<(i64, Value)> = r.rows.iter().map(|row| (row[0].as_i64().unwrap(), row[1].clone())).collect();
    assert!(got.contains(&(5, Value::Int(BIG + 1))) && got.contains(&(6, Value::Int(BIG - 1))), "{got:?}");
    assert!(db.sql("UPDATE t SET n = n + 1.5 WHERE id = 5", &p).is_err());
    assert!(db.sql("UPDATE t SET n = n + 1 WHERE id = 9223372036854775807", &p).is_err(), "i64::MAX + 1 overflows");
}

// Bug 5: `rating != 2` returned the rows without a rating. A comparison
// with NULL is UNKNOWN, and so is its negation.
#[test]
fn nulls_are_unknown_in_comparisons() {
    let (_d, db) = sample();
    let (b, b1, max, min) = (BIG, BIG + 1, i64::MAX, i64::MIN);
    for (sql, want) in [
        ("SELECT id FROM t WHERE rating != 2", vec![min, b, max]),
        ("SELECT id FROM t WHERE NOT rating > 2", vec![b, b1]),
        ("SELECT id FROM t WHERE NOT (rating > 2 OR kind = 'a')", vec![b1]),
        ("SELECT id FROM t WHERE NOT (rating > 2 AND kind = 'a')", vec![min, b, b1]),
        ("SELECT id FROM t WHERE NOT NOT rating = 1", vec![b]),
        ("SELECT id FROM t WHERE rating NOT IN (1, 2)", vec![min, max]),
        ("SELECT id FROM t WHERE rating NOT BETWEEN 1.5 AND 3.5", vec![min, b]),
        ("SELECT id FROM t WHERE kind != 'a'", vec![min, b1]),
        ("SELECT id FROM t WHERE n != 6", vec![min, b, b1, max]),
        // A multi-valued field: NOT holds when no value matches, so for a
        // row without values too.
        ("SELECT id FROM t WHERE NOT tags = 1", vec![min, 5, 6, b, b1]),
        ("SELECT id FROM t WHERE rating IS NULL", vec![5, 6]),
        ("SELECT id FROM t WHERE rating IS NOT NULL", vec![min, b, b1, max]),
        ("SELECT id FROM t WHERE NOT rating IS NULL", vec![min, b, b1, max]),
        ("SELECT id FROM t WHERE kind IS NULL", vec![5, 6]),
        ("SELECT id FROM t WHERE n IS NULL", vec![5]),
        ("SELECT id FROM t WHERE tags IS NULL", vec![5]),
        ("SELECT id FROM t WHERE rating IS NULL OR rating > 3", vec![min, 5, 6]),
        ("SELECT id FROM t WHERE NOT (rating IS NULL OR rating > 3)", vec![b, b1, max]),
        // MATCH is never UNKNOWN.
        ("SELECT id FROM t WHERE NOT MATCH(body, 'pear')", vec![min, 5, 6, b, b1, max]),
    ] {
        assert_eq!(q(&db, sql), want, "{sql}");
        let count = db.query(&sql.replace("SELECT id", "SELECT COUNT(*)"), &Params::new()).unwrap();
        assert_eq!(count.rows[0][0], Value::Int(want.len() as i64), "{sql}");
    }
    let r = db.query("SELECT id FROM t WHERE rating = NULL", &Params::new());
    assert!(matches!(&r, Err(e) if e.to_string().contains("IS NULL")), "{r:?}");

    // In-place NULLs count as missing too.
    db.sql("UPDATE t SET rating = NULL WHERE id = 9223372036854775807", &Params::new()).unwrap();
    assert_eq!(q(&db, "SELECT id FROM t WHERE rating != 2"), vec![min, b]);
    assert_eq!(q(&db, "SELECT id FROM t WHERE rating IS NULL"), vec![5, 6, max]);
}

// Joins filter rows in memory with the same three-valued logic.
#[test]
fn joins_treat_nulls_as_unknown() {
    let (_d, db) = sample();
    let got = q(&db, "SELECT a.id FROM t a JOIN t b ON a.id = b.id WHERE NOT (a.rating < b.rating)");
    assert_eq!(got, vec![i64::MIN, BIG, BIG + 1, i64::MAX]);
    let got = q(&db, "SELECT a.id FROM t a JOIN t b ON a.id = b.id WHERE a.rating IS NULL OR NOT (a.rating < b.rating)");
    assert_eq!(got, vec![i64::MIN, 5, 6, BIG, BIG + 1, i64::MAX]);
    let got = q(&db, &format!("SELECT a.id FROM t a JOIN t b ON a.id = b.id WHERE a.n = {}", BIG + 1));
    assert_eq!(got, vec![BIG + 1]);
}

// Bug 6: ORDER BY a column dropped the rows without a value. They come
// last in either direction, or first with NULLS FIRST.
#[test]
fn order_by_keeps_rows_without_a_value() {
    let (_d, db) = sample();
    let (b, b1, max, min) = (BIG, BIG + 1, i64::MAX, i64::MIN);
    let p = Params::new();
    for (sql, want) in [
        ("SELECT id FROM t ORDER BY rating DESC, id", vec![min, max, b1, b, 5, 6]),
        ("SELECT id FROM t ORDER BY rating, id", vec![b, b1, max, min, 5, 6]),
        ("SELECT id FROM t ORDER BY rating NULLS LAST, id", vec![b, b1, max, min, 5, 6]),
        ("SELECT id FROM t ORDER BY rating DESC NULLS FIRST, id", vec![5, 6, min, max, b1, b]),
        ("SELECT id FROM t ORDER BY rating ASC NULLS FIRST, id DESC", vec![6, 5, b, b1, max, min]),
        ("SELECT id FROM t ORDER BY n DESC", vec![max, b1, b, 6, min, 5]),
        ("SELECT id FROM t ORDER BY rating DESC, id LIMIT 2 OFFSET 3", vec![b, 5]),
        ("SELECT id FROM t WHERE id > 0 ORDER BY rating DESC NULLS FIRST, id LIMIT 3", vec![5, 6, max]),
    ] {
        let r = db.query(sql, &p).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(ordered(&r), want, "{sql}");
    }
    let r = db.query("SELECT id, score() FROM t ORDER BY rating DESC, id", &p).unwrap();
    assert_eq!(r.rows[0][1], Value::Float(4.0));
    assert_eq!(r.rows[5][1], Value::Null, "a row without a rating has no score");
    let r = db.query("SELECT id FROM t ORDER BY bm25(body, 'apple') DESC NULLS FIRST", &p);
    assert!(r.is_err(), "NULLS FIRST only orders columns");
}

// Shards place the rows without a value as one database does.
#[test]
fn order_by_nulls_alike_over_shards() {
    let rows: Vec<Vec<Value>> = (0..60).map(|i| row(i, (i % 4 != 0).then_some(i % 9), &[], Some("a"), (i % 3 != 0).then_some((i % 7) as f64))).collect();
    let (_d0, whole) = db_with(rows.clone());
    let (_d1, a) = db_with(rows[..30].to_vec());
    let (_d2, b) = db_with(rows[30..].to_vec());
    let params = Params::new();
    for sql in [
        "SELECT id, score() FROM t ORDER BY rating DESC, id LIMIT 50",
        "SELECT id, score() FROM t ORDER BY rating, id DESC LIMIT 50",
        "SELECT id, score() FROM t ORDER BY n DESC NULLS FIRST, id LIMIT 30",
        "SELECT id, score() FROM t ORDER BY n NULLS FIRST, id",
    ] {
        let want = whole.query(sql, &params).unwrap();
        let snaps = [a.snapshot(), b.snapshot()];
        let select = bind(sql, snaps[0].schema(), &params).unwrap();
        let stats = Arc::new(TermStats::sum(&[a.term_stats(&snaps[0], &select).unwrap(), b.term_stats(&snaps[1], &select).unwrap()]));
        let opts = ExecOptions { stats: Some(stats), shard: true, ..ExecOptions::default() };
        let parts = vec![a.execute(&snaps[0], &select, &opts).unwrap(), b.execute(&snaps[1], &select, &opts).unwrap()];
        let got = cluster::merge(&select, parts).unwrap();
        assert_eq!(ordered(&want), ordered(&got), "{sql}");
        assert_eq!(want.rows, got.rows, "{sql}");
    }
}

// A sum of large integers over shards is as exact as on one database.
#[test]
fn sums_are_exact_over_shards() {
    let rows: Vec<Vec<Value>> = (0..40).map(|i| row(i, Some(BIG + i), &[], Some(["a", "b"][i as usize % 2]), Some(i as f64))).collect();
    let (_d0, whole) = db_with(rows.clone());
    let (_d1, a) = db_with(rows[..20].to_vec());
    let (_d2, b) = db_with(rows[20..].to_vec());
    let params = Params::new();
    for sql in ["SELECT SUM(n) FROM t", "SELECT kind, SUM(n), SUM(rating) FROM t GROUP BY kind ORDER BY kind"] {
        let want = whole.query(sql, &params).unwrap();
        let snaps = [a.snapshot(), b.snapshot()];
        let select = bind(sql, snaps[0].schema(), &params).unwrap();
        let opts = ExecOptions { shard: true, ..ExecOptions::default() };
        let parts = vec![a.execute(&snaps[0], &select, &opts).unwrap(), b.execute(&snaps[1], &select, &opts).unwrap()];
        let got = cluster::merge(&select, parts).unwrap();
        assert_eq!(want.rows, got.rows, "{sql}");
    }
    let total = whole.query("SELECT SUM(n) FROM t", &params).unwrap();
    assert_eq!(total.rows[0][0], Value::Int(40 * BIG + (0..40).sum::<i64>()));
}
