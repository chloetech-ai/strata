//! NULL in conditions and groups, as SQL reads it, over a table, over its
//! derived rows and over shards. Review bugs of 30 October BUG-3 (the NULL
//! group of a facet and of DISTINCT), BUG-4 (`NOT IN` a list with a NULL),
//! BUG-6 (a scalar subquery without a row) and BUG-16 (DISTINCT through
//! the shard merge), and of 31 October BUG-N7 (`NOT LIKE '%'`) and BUG-N9
//! (an IN list naming the outer row's column).

use std::sync::Arc;

use strata::cluster;
use strata::db::{Db, SqlResult};
use strata::query::exec::ExecOptions;
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

/// Table t (id INT PRIMARY KEY, n INT, k KEYWORD, tags KEYWORD[]), the
/// database's first, which [`Db::execute`] answers for the shard rounds.
fn db(path: &std::path::Path) -> Db {
    let fields = vec![
        Field::new("id", FieldType::Int),
        Field::new("n", FieldType::Int),
        Field::new("k", FieldType::Keyword),
        Field::new("tags", FieldType::Keyword).multi(),
    ];
    let t = Schema::new("t", fields, Some("id")).unwrap();
    Db::create(path, t, DbOptions { sync: false, segment_max_docs: 3, ..DbOptions::default() }).unwrap()
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

fn ids(db: &Db, q: &str) -> Vec<i64> {
    let mut v: Vec<i64> = sql(db, q).iter().map(|r| r[0].as_i64().unwrap()).collect();
    v.sort();
    v
}

/// Rows 1..=8: n and k with NULLs, tags with and without values.
fn fill(db: &Db) {
    sql(
        db,
        "INSERT INTO t (id, n, k, tags) VALUES (1, 1, 'a', ARRAY['x']), (2, 2, NULL, NULL), (3, NULL, 'b', ARRAY['x', 'y']), (4, 4, 'a', NULL), \
         (5, NULL, NULL, ARRAY['y']), (6, 1, 'c', NULL), (7, 7, 'b', ARRAY['z']), (8, 8, NULL, ARRAY['x'])",
    );
}

fn derived(q: &str) -> String {
    q.replacen("FROM t", "FROM (SELECT * FROM t) d", 1)
}

/// Review BUG-3: `GROUP BY k` with only COUNT, and `SELECT DISTINCT k`,
/// took the facet path, which counted values and dropped the rows without
/// one: their NULL group is back, as with any other aggregate.
#[test]
fn groups_keep_the_null_group() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("t.strata"));
    fill(&db);
    let mut groups = sql(&db, "SELECT k, COUNT(*) FROM t GROUP BY k");
    groups.sort_by_key(|g| format!("{g:?}"));
    let want = vec![
        vec![Value::Null, Value::Int(3)],
        vec![Value::Str("a".into()), Value::Int(2)],
        vec![Value::Str("b".into()), Value::Int(2)],
        vec![Value::Str("c".into()), Value::Int(1)],
    ];
    assert_eq!(groups, want);
    // The general path, with another aggregate, agrees.
    let mut general: Vec<Vec<Value>> = sql(&db, "SELECT k, COUNT(*), SUM(id) FROM t GROUP BY k").into_iter().map(|mut r| {
        r.pop();
        r
    }).collect();
    general.sort_by_key(|g| format!("{g:?}"));
    assert_eq!(general, want);
    // Counts add up to the rows, also under a filter.
    let total: i64 = sql(&db, "SELECT k, COUNT(*) FROM t WHERE n > 1 GROUP BY k").iter().map(|r| r[1].as_i64().unwrap()).sum();
    assert_eq!(total, sql(&db, "SELECT COUNT(*) FROM t WHERE n > 1")[0][0].as_i64().unwrap());
    // A multi-valued field: a group per value, and NULL for rows without.
    let mut tags = sql(&db, "SELECT tags, COUNT(*) FROM t GROUP BY tags");
    tags.sort_by_key(|g| format!("{g:?}"));
    assert_eq!(tags[0], vec![Value::Null, Value::Int(3)]);
    assert_eq!(tags.len(), 4);
    // Ordered by its value, the NULL group comes last either way, as rows
    // without a value do, over the table and over derived rows.
    for q in ["SELECT k, COUNT(*) FROM t GROUP BY k ORDER BY k", "SELECT k, COUNT(*), SUM(n) FROM t GROUP BY k ORDER BY k"] {
        for (dir, keys) in [("", ["a", "b", "c"]), (" DESC", ["c", "b", "a"])] {
            let q = format!("{q}{dir}");
            for q in [q.clone(), derived(&q)] {
                let got: Vec<Value> = sql(&db, &q).into_iter().map(|r| r[0].clone()).collect();
                let mut want: Vec<Value> = keys.iter().map(|k| Value::Str(k.to_string())).collect();
                want.push(Value::Null);
                assert_eq!(got, want, "{q}");
            }
        }
    }
    // And a group whose aggregate is NULL (AVG of no values) last.
    let q = "SELECT k, AVG(n) FROM t WHERE id IN (2, 3, 4, 5, 6) GROUP BY k ORDER BY AVG(n)";
    let got: Vec<Value> = sql(&db, q).into_iter().map(|r| r[1].clone()).collect();
    assert_eq!(got, vec![Value::Float(1.0), Value::Float(2.0), Value::Float(4.0), Value::Null], "{q}");
    let mut distinct = sql(&db, "SELECT DISTINCT k FROM t");
    distinct.sort_by_key(|g| format!("{g:?}"));
    assert_eq!(distinct, vec![vec![Value::Null], vec![Value::Str("a".into())], vec![Value::Str("b".into())], vec![Value::Str("c".into())]]);
}

/// Review BUG-4: `k NOT IN (SELECT k FROM b)` returned rows although b
/// holds a NULL, and a literal `n NOT IN (1, NULL)` was an error; both hold
/// for no row, and `IN` with a NULL is TRUE where a value matches.
#[test]
fn in_lists_with_a_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("t.strata"));
    fill(&db);
    sql(&db, "CREATE TABLE b (id INT PRIMARY KEY, k KEYWORD, n INT)");
    sql(&db, "INSERT INTO b (id, k, n) VALUES (1, 'a', 1), (2, NULL, NULL), (3, 'c', 7), (4, 'x', NULL)");
    for (q, want) in [
        ("SELECT id FROM t WHERE k NOT IN (SELECT k FROM b) LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE k IN (SELECT k FROM b) LIMIT 100", vec![1, 4, 6]),
        ("SELECT id FROM t WHERE NOT (k NOT IN (SELECT k FROM b)) LIMIT 100", vec![1, 4, 6]),
        ("SELECT id FROM t WHERE k NOT IN (SELECT k FROM b WHERE k IS NOT NULL) LIMIT 100", vec![3, 7]),
        ("SELECT id FROM t WHERE n NOT IN (SELECT n FROM b) OR id = 2 LIMIT 100", vec![2]),
        ("SELECT id FROM t WHERE n NOT IN (1, NULL) LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE n IN (1, NULL) LIMIT 100", vec![1, 6]),
        ("SELECT id FROM t WHERE NOT (n NOT IN (1, NULL)) LIMIT 100", vec![1, 6]),
        ("SELECT id FROM t WHERE n IN (NULL) LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE n NOT IN (NULL) OR k = 'c' LIMIT 100", vec![6]),
        ("SELECT id FROM t WHERE k NOT IN ('a', NULL) OR n = 2 LIMIT 100", vec![2]),
        // A multi-valued field's membership is never UNKNOWN: one of its
        // values is in the list, or none is (rows without tags included).
        ("SELECT id FROM t WHERE tags IN ('y', NULL) LIMIT 100", vec![3, 5]),
        ("SELECT id FROM t WHERE tags NOT IN ('y', NULL) LIMIT 100", vec![1, 2, 4, 6, 7, 8]),
        ("SELECT id FROM t WHERE tags NOT IN (NULL) LIMIT 100", vec![1, 2, 3, 4, 5, 6, 7, 8]),
        ("SELECT id FROM t WHERE tags IN (SELECT k FROM b) LIMIT 100", vec![1, 3, 8]),
        ("SELECT id FROM t WHERE tags NOT IN (SELECT k FROM b) LIMIT 100", vec![2, 4, 5, 6, 7]),
    ] {
        assert_eq!(ids(&db, q), want, "{q}");
        assert_eq!(ids(&db, &derived(q)), want, "{}", derived(q));
    }
}

/// Review BUG-6: a scalar subquery that finds no row failed the query ("a
/// comparison with NULL is never true"); its NULL is UNKNOWN, so neither
/// the comparison nor its negation holds.
#[test]
fn a_scalar_subquery_without_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("t.strata"));
    fill(&db);
    for (q, want) in [
        ("SELECT id FROM t WHERE n > (SELECT n FROM t WHERE id = 99) LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE NOT (n > (SELECT n FROM t WHERE id = 99)) LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE n > (SELECT n FROM t WHERE id = 99) OR k = 'c' LIMIT 100", vec![6]),
        ("SELECT id FROM t WHERE n = (SELECT n FROM t WHERE id = 3) LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE n BETWEEN (SELECT n FROM t WHERE id = 99) AND 4 LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE n > (SELECT MAX(n) FROM t WHERE id > 99) LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE n >= (SELECT n FROM t WHERE id = 7) LIMIT 100", vec![7, 8]),
    ] {
        assert_eq!(ids(&db, q), want, "{q}");
        assert_eq!(ids(&db, &derived(q)), want, "{}", derived(q));
    }
    // A NULL the query writes is still an error that points to IS NULL.
    let e = db.sql("SELECT id FROM t WHERE n > NULL LIMIT 10", &Params::new()).unwrap_err().to_string();
    assert!(e.contains("NULL"), "{e}");
}

/// Review BUG-N7: `NOT k LIKE '%'` and `NOT PREFIX(k, '')` kept the rows
/// without k over a table (the empty prefix was `IS NOT NULL`), and not
/// over derived rows; `NULL LIKE '%'` is UNKNOWN, so neither keeps them.
/// A multi-valued field's NOT holds when no value matches, so rows without
/// tags pass `NOT PREFIX(tags, '')`, as `tags != 'x'` does.
#[test]
fn the_empty_prefix_and_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("t.strata"));
    fill(&db);
    for (q, want) in [
        ("SELECT id FROM t WHERE NOT k LIKE '%' LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE k NOT LIKE '%' LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE NOT PREFIX(k, '') LIMIT 100", vec![]),
        ("SELECT id FROM t WHERE k LIKE '%' LIMIT 100", vec![1, 3, 4, 6, 7]),
        ("SELECT id FROM t WHERE PREFIX(k, '') LIMIT 100", vec![1, 3, 4, 6, 7]),
        // UNKNOWN AND FALSE is FALSE (row 2), UNKNOWN AND TRUE is not (8).
        ("SELECT id FROM t WHERE NOT (k LIKE '%' AND n > 3) LIMIT 100", vec![1, 2, 6]),
        ("SELECT id FROM t WHERE NOT k LIKE 'a%' LIMIT 100", vec![3, 6, 7]),
    ] {
        assert_eq!(ids(&db, q), want, "{q}");
        assert_eq!(ids(&db, &derived(q)), want, "{}", derived(q));
    }
    for (q, want) in [
        ("SELECT id FROM t WHERE NOT PREFIX(tags, '') LIMIT 100", vec![2, 4, 6]),
        ("SELECT id FROM t WHERE PREFIX(tags, '') LIMIT 100", vec![1, 3, 5, 7, 8]),
        ("SELECT id FROM t WHERE NOT tags LIKE 'x%' LIMIT 100", vec![2, 4, 5, 6, 7]),
        ("SELECT id FROM t WHERE tags LIKE 'y%' LIMIT 100", vec![3, 5]),
    ] {
        assert_eq!(ids(&db, q), want, "{q}");
        assert_eq!(ids(&db, &derived(q)), want, "{}", derived(q));
    }
}

/// Review BUG-N9: an IN list naming the outer row's column failed with the
/// planner's internal name (`expected a literal or parameter, got
/// __outer(2)`); it compares with the outer row's value, and NULL there is
/// UNKNOWN.
#[test]
fn an_in_list_with_the_outer_rows_column() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("t.strata"));
    fill(&db);
    sql(&db, "CREATE TABLE b (id INT PRIMARY KEY, k KEYWORD)");
    sql(&db, "INSERT INTO b (id, k) VALUES (1, 'a'), (2, 'a'), (3, 'q'), (4, 'b'), (5, NULL)");
    let rows = sql(&db, "SELECT id, (SELECT COUNT(*) FROM b WHERE b.k IN (t.k, 'q')) AS c FROM t ORDER BY id LIMIT 100");
    let got: Vec<(i64, f64)> = rows.iter().map(|r| (r[0].as_i64().unwrap(), r[1].as_f64().unwrap())).collect();
    // a: 2 + q: 1; b: 1 + 1; c: 0 + 1; NULL: only q.
    assert_eq!(got, vec![(1, 3.0), (2, 1.0), (3, 2.0), (4, 3.0), (5, 1.0), (6, 1.0), (7, 2.0), (8, 1.0)]);
    let rows = sql(&db, "SELECT id, (SELECT COUNT(*) FROM b WHERE b.k NOT IN (t.k, 'q')) AS c FROM t ORDER BY id LIMIT 100");
    let got: Vec<(i64, f64)> = rows.iter().map(|r| (r[0].as_i64().unwrap(), r[1].as_f64().unwrap())).collect();
    // b's NULL row is never counted; a NULL outer k counts nothing.
    assert_eq!(got, vec![(1, 1.0), (2, 0.0), (3, 2.0), (4, 1.0), (5, 0.0), (6, 3.0), (7, 2.0), (8, 0.0)]);
}

/// Review BUG-16: `SELECT DISTINCT` failed through the shard merge ("SELECT
/// DISTINCT ... is not supported"); it binds as GROUP BY, so shards answer
/// with groups the coordinator merges, NULL group included.
#[test]
fn distinct_over_shards() {
    let dir = tempfile::tempdir().unwrap();
    let whole = db(&dir.path().join("w.strata"));
    fill(&whole);
    let rows = sql(&whole, "SELECT id, n, k, tags FROM t ORDER BY id LIMIT 100");
    let shards: Vec<Db> = (0..2).map(|i| db(&dir.path().join(format!("s{i}.strata")))).collect();
    for (i, r) in rows.iter().enumerate() {
        let lit = |v: &Value| match v {
            Value::Null => "NULL".to_string(),
            Value::Int(x) => x.to_string(),
            Value::Str(s) => format!("'{s}'"),
            Value::List(xs) => format!("ARRAY[{}]", xs.iter().map(|x| format!("'{}'", x.as_str().unwrap())).collect::<Vec<_>>().join(", ")),
            v => panic!("{v:?}"),
        };
        sql(&shards[i % 2], &format!("INSERT INTO t (id, n, k, tags) VALUES ({}, {}, {}, {})", lit(&r[0]), lit(&r[1]), lit(&r[2]), lit(&r[3])));
    }
    for q in ["SELECT DISTINCT k FROM t", "SELECT DISTINCT k, n FROM t WHERE n > 1", "SELECT DISTINCT tags FROM t", "SELECT k, COUNT(*) FROM t GROUP BY k"] {
        let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
        let select = bind(q, snaps[0].table("t").unwrap().schema(), &Params::new()).unwrap();
        let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
        let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() };
        let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &select, &opts).unwrap()).collect();
        let mut merged = cluster::merge(&select, answers).unwrap().rows;
        let mut want = sql(&whole, q);
        merged.sort_by_key(|r| format!("{r:?}"));
        want.sort_by_key(|r| format!("{r:?}"));
        assert_eq!(merged, want, "{q}");
        assert!(want.iter().any(|r| r[0] == Value::Null), "{q}: {want:?}");
    }
}
