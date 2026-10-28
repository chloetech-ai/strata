//! Requests that used to panic or abort the process (the engineering
//! review's reproduced bugs 8, 10, 11 and 12): each must now answer or
//! fail with an error, never take the process down.

use std::sync::Arc;

use strata::cluster;
use strata::db::Db;
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Int),
            Field::new("body", FieldType::Text { positions: true }),
            Field::new("kind", FieldType::Keyword),
            Field::new("rating", FieldType::Float),
            Field::new("emb", FieldType::Vector { dim: 2, index: VectorIndexKind::Hnsw { m: 16, ef_construction: 100 } }),
        ],
        Some("id"),
    )
    .unwrap()
}

fn row(id: i64, body: &str, rating: Option<f64>, emb: [f32; 2]) -> Vec<Value> {
    vec![Value::Int(id), body.into(), "x".into(), rating.map_or(Value::Null, Value::Float), Value::Vector(emb.to_vec().into())]
}

fn db_with(rows: Vec<Vec<Value>>) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("t.strata"), schema(), DbOptions::default()).unwrap();
    db.load(rows).unwrap();
    (dir, db)
}

fn sample() -> (tempfile::TempDir, Db) {
    db_with(vec![
        row(1, "apple", None, [1.0, 0.0]),
        row(2, "apple pie", Some(1.0), [0.0, 1.0]),
        row(3, "banana", Some(3.0), [-1.0, 0.0]),
        row(4, "apple apple tart", Some(2.0), [0.6, 0.8]),
    ])
}

fn ids(r: &ResultSet) -> Vec<i64> {
    r.rows.iter().map(|row| row[0].as_i64().unwrap()).collect()
}

/// Runs `f` on a thread with a tokio worker's 2 MB stack.
fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new().stack_size(2 << 20).spawn(f).unwrap().join().expect("no stack overflow")
}

// Bug 10: a NaN reached the store through the Rust API, then ORDER BY
// panicked on it.
#[test]
fn non_finite_floats_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("t.strata"), schema(), DbOptions::default()).unwrap();
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let r = db.load(vec![row(1, "a", Some(x), [1.0, 0.0])]);
        assert!(matches!(r, Err(strata::Error::Schema(_))), "{x}: {r:?}");
        let mut t = db.storage().begin().unwrap();
        assert!(t.insert(row(1, "a", Some(x), [1.0, 0.0])).is_err(), "{x}");
    }
    let r = db.load(vec![row(1, "a", Some(1.0), [f32::NAN, 0.0])]);
    assert!(matches!(r, Err(strata::Error::Schema(_))), "NaN in a vector: {r:?}");

    db.load(vec![row(1, "a", Some(1.0), [1.0, 0.0])]).unwrap();
    let mut t = db.storage().begin().unwrap();
    assert!(t.set(&Value::Int(1), "rating", Value::Float(f64::NAN)).is_err());
    drop(t);
    // An in-place sum that overflows is refused, not stored as infinity.
    db.sql("UPDATE t SET rating = 1e308 WHERE id = 1", &Params::new()).unwrap();
    assert!(db.sql("UPDATE t SET rating = rating + 1e308 WHERE id = 1", &Params::new()).is_err());
    let r = db.query("SELECT id, rating FROM t ORDER BY rating DESC", &Params::new()).unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1), Value::Float(1e308)]]);
    // A query vector must be finite too.
    let p = Params::new().set("q", Value::Vector(vec![f32::NAN, 1.0].into()));
    assert!(db.query("SELECT id FROM t ORDER BY similarity(emb, :q) DESC", &p).is_err());
}

// Bug 8: a weighted sum with a NULL column normalised to NaN and panicked.
#[test]
fn null_inputs_of_a_weighted_sum_rank_as_their_worst() {
    let (_d, db) = sample();
    let r = db.query("SELECT id, score() FROM t ORDER BY 0.7 * bm25(body, 'apple') + 0.3 * rating DESC LIMIT 10", &Params::new()).unwrap();
    assert!(r.scores.iter().all(|s| s.is_finite()), "{:?}", r.scores);
    // Row 1 has the top BM25 of the short texts but no rating: it scores
    // as the lowest rating, 0 after min-max, so row 4 (more apples, a
    // middling rating) and row 1 stay above row 2 only by their BM25.
    assert_eq!(ids(&r)[0], 4, "{:?}", r.rows);
    assert!(ids(&r).contains(&1));
    // A negative weight: the row without a value gains nothing from it.
    let r = db.query("SELECT id, score() FROM t ORDER BY bm25(body, 'apple') - 0.1 * rating DESC LIMIT 10", &Params::new()).unwrap();
    assert!(r.scores.iter().all(|s| s.is_finite()), "{:?}", r.scores);
    let worst_of_rated = r.scores[ids(&r).iter().position(|&i| i == 3).unwrap()];
    assert!(worst_of_rated.is_finite());
    // Raw scores: a NULL counts as the worst rating, never as +∞.
    let r = db.query("SELECT id, score() FROM t ORDER BY bm25(body, 'apple', norm => 'none') - 0.1 * rating DESC LIMIT 10", &Params::new()).unwrap();
    assert!(r.scores.iter().all(|s| s.is_finite() && *s < 1e6), "{:?}", r.scores);
}

// Bug 8 over shards: the coordinator fuses the same way.
#[test]
fn null_inputs_fuse_alike_over_shards() {
    let rows: Vec<Vec<Value>> = (0..200)
        .map(|i| {
            let body = format!("apple {} {}", ["pie", "tart", "juice", "core"][i % 4], "x ".repeat(i % 7));
            row(i as i64, &body, (i % 3 != 0).then_some((i % 11) as f64), [(i as f32).cos(), (i as f32).sin()])
        })
        .collect();
    let (_d0, whole) = db_with(rows.clone());
    let (_d1, a) = db_with(rows[..100].to_vec());
    let (_d2, b) = db_with(rows[100..].to_vec());
    let params = Params::new().set("q", Value::Vector(vec![1.0, 0.0].into()));
    for sql in [
        "SELECT id, score() FROM t ORDER BY 0.7 * bm25(body, 'apple tart') + 0.3 * rating DESC LIMIT 20",
        "SELECT id, score() FROM t ORDER BY bm25(body, 'apple tart', norm => 'none') - 0.5 * rating DESC LIMIT 20",
        "SELECT id, score() FROM t ORDER BY similarity(emb, :q) * rating DESC LIMIT 20",
    ] {
        let want = whole.query(sql, &params).unwrap();
        let snaps = [a.snapshot(), b.snapshot()];
        let select = bind(sql, snaps[0].schema(), &params).unwrap();
        let stats = Arc::new(TermStats::sum(&[a.term_stats(&snaps[0], &select).unwrap(), b.term_stats(&snaps[1], &select).unwrap()]));
        let opts = ExecOptions { stats: Some(stats), shard: true, ..ExecOptions::default() };
        let parts = vec![a.execute(&snaps[0], &select, &opts).unwrap(), b.execute(&snaps[1], &select, &opts).unwrap()];
        let got = cluster::merge(&select, parts).unwrap();
        assert!(want.scores.iter().all(|s| s.is_finite()), "{sql}");
        assert_eq!(want.scores.len(), got.scores.len(), "{sql}");
        for (x, y) in want.scores.iter().zip(&got.scores) {
            assert!((x - y).abs() < 1e-6, "{sql}: {:?} vs {:?}", want.scores, got.scores);
        }
    }
}

// A product of a negative similarity and another factor inverted the order.
#[test]
fn negative_product_factors_count_as_zero() {
    let (_d, db) = sample();
    let p = Params::new().set("q", Value::Vector(vec![-1.0, 0.0].into()));
    let r = db.query("SELECT id, score() FROM t ORDER BY similarity(emb, :q) * rating DESC LIMIT 10", &p).unwrap();
    // Row 3 points along the query (similarity 1, rating 3); the others
    // point away from it and score 0.
    assert_eq!(ids(&r)[0], 3);
    assert!(r.scores[1..].iter().all(|&s| s == 0.0), "{:?}", r.scores);
}

// Bug 11: 100,000 nested parentheses overflowed a 2 MB stack.
#[test]
fn deep_match_queries_are_refused() {
    let (_d, db) = sample();
    let db = Arc::new(db);
    for q in [format!("{}apple{}", "(".repeat(100_000), ")".repeat(100_000)), format!("{}apple", "NOT ".repeat(100_000)), format!("{}apple", "-".repeat(100_000))] {
        let db = db.clone();
        let r = on_small_stack(move || db.query("SELECT id FROM t WHERE MATCH(body, :q)", &Params::new().set("q", Value::Str(q))).map(|r| r.rows.len()));
        assert!(matches!(&r, Err(strata::Error::Query(m)) if m.contains("nests")), "{r:?}");
    }
    // Ordinary nesting still works.
    let r = db.query("SELECT id FROM t WHERE MATCH(body, '((apple AND (pie OR tart)))')", &Params::new()).unwrap();
    assert_eq!(r.rows.len(), 2);
}

// Deep SQL expressions (a long OR chain parses into a deep tree).
#[test]
fn deep_sql_expressions_are_refused() {
    let (_d, db) = sample();
    let db = Arc::new(db);
    for sql in [
        format!("SELECT id FROM t WHERE {}", vec!["kind = 'x'"; 50_000].join(" OR ")),
        format!("SELECT id FROM t WHERE {}kind = 'x'", "NOT ".repeat(50_000)),
        format!("SELECT id FROM t WHERE {}kind = 'x'{}", "(".repeat(50_000), ")".repeat(50_000)),
        format!("SELECT id FROM t ORDER BY {} DESC", vec!["rating"; 50_000].join(" + ")),
    ] {
        let db = db.clone();
        let r = on_small_stack(move || db.query(&sql, &Params::new()).map(|r| r.rows.len()));
        assert!(r.is_err(), "{r:?}");
    }
    // Just under the limit, a statement binds, runs and drops on a small
    // stack.
    for sql in [
        format!("SELECT id FROM t WHERE {}", vec!["kind = 'x'"; 495].join(" OR ")),
        format!("SELECT id FROM t WHERE {}", vec!["NOT NOT kind = 'x'"; 240].join(" AND ")),
        format!("SELECT id FROM t ORDER BY {} DESC LIMIT 2", vec!["rating"; 995].join(" + ")),
        format!("SELECT id FROM t ORDER BY {} DESC LIMIT 2", vec!["2 * rating"; 495].join(" - ")),
    ] {
        let db = db.clone();
        let head = sql[..60].to_string();
        let r = on_small_stack(move || db.query(&sql, &Params::new()).map(|r| r.rows.len()));
        assert!(r.is_ok(), "{head}: {r:?}");
    }
    // A long but flat IN list is fine.
    let list = (0..20_000).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
    let r = db.query(&format!("SELECT id FROM t WHERE id IN ({list})"), &Params::new()).unwrap();
    assert_eq!(r.rows.len(), 4);
}

// Bug 12: LIMIT 1e12 sized a heap of 8 TB and aborted the process.
#[test]
fn huge_limits_and_depths_answer_with_what_there_is() {
    let (_d, db) = sample();
    let p = Params::new().set("q", Value::Vector(vec![1.0, 0.0].into()));
    for (sql, rows) in [
        ("SELECT id FROM t ORDER BY bm25(body, 'apple') DESC LIMIT 1000000000000", 3),
        ("SELECT id FROM t ORDER BY bm25(body, 'apple') DESC LIMIT 18446744073709551615 OFFSET 1", 2),
        ("SELECT id FROM t ORDER BY rating DESC LIMIT 18446744073709551615 OFFSET 1", 2),
        ("SELECT id FROM t LIMIT 18446744073709551615 OFFSET 3", 1),
        ("SELECT id FROM t ORDER BY similarity(emb, :q) DESC LIMIT 1000000000000", 4),
        ("SELECT id FROM t ORDER BY similarity(emb, :q, ef => 1e15, rerank => 1e15) DESC LIMIT 2", 2),
        ("SELECT id FROM t ORDER BY rrf(bm25(body, 'apple'), similarity(emb, :q), depth => 1e15) DESC LIMIT 1000000000000", 4),
        ("SELECT id FROM t ORDER BY 0.5 * bm25(body, 'apple', depth => 1e15) + 0.5 * similarity(emb, :q) DESC LIMIT 1000000000000", 4),
        ("SELECT id FROM t ORDER BY bm25(body, 'apple', depth => 1e15) * similarity(emb, :q) DESC LIMIT 1000000000000", 4),
        ("SELECT id FROM t ORDER BY decay(rating, 2, 1) DESC LIMIT 1000000000000", 4),
    ] {
        let r = db.query(sql, &p).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(r.rows.len(), rows, "{sql}");
    }
}

// An error message quoting a long non-ASCII value was cut inside a
// character and panicked.
#[test]
fn error_messages_cut_at_character_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("t.strata"), schema(), DbOptions::default()).unwrap();
    let mut bad = row(1, "a", Some(1.0), [1.0, 0.0]);
    bad[3] = Value::Str("é".repeat(100));
    let r = db.load(vec![bad]);
    assert!(matches!(r, Err(strata::Error::Schema(_))), "{r:?}");
}
