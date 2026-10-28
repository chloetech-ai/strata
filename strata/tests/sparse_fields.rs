//! SPARSE fields (learned term weights): top-k by the dot product equal to
//! brute force over the quantised weights for every top-k algorithm, with
//! filters, over several segments and merged shards; in fusions and
//! products; SQL writes; and the errors.

use std::collections::HashMap;
use std::sync::Arc;

use rand::{Rng, SeedableRng};
use strata::cluster;
use strata::db::{Db, SqlResult};
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::storage::DbOptions;
use strata::text::sparse::{quantise, to_value, SCALE};
use strata::{Field, FieldType, Schema, Value};

const N: usize = 3000;

fn schema() -> Schema {
    Schema::new(
        "docs",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("year", FieldType::Int),
            Field::new("body", FieldType::Text { positions: false }),
            Field::new("splade", FieldType::Sparse),
        ],
        Some("id"),
    )
    .unwrap()
}

fn term(i: usize) -> String {
    format!("t{i}")
}

/// Documents with 10 to 60 terms each from a skewed vocabulary of 1,500,
/// weights up to 3.
fn docs(seed: u64) -> (Vec<Vec<Value>>, Vec<HashMap<String, f32>>) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut out = Vec::new();
    let mut weights = Vec::new();
    for i in 0..N {
        let mut w: HashMap<String, f32> = HashMap::new();
        for _ in 0..rng.gen_range(10..60) {
            let t = (rng.gen::<f64>().powi(3) * 1500.0) as usize;
            w.insert(term(t), rng.gen_range(0.0f32..3.0));
        }
        let pairs: Vec<(String, f32)> = w.iter().map(|(t, w)| (t.clone(), *w)).collect();
        let body = pairs.iter().take(5).map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(" ");
        out.push(vec![Value::Str(format!("d{i:05}")), Value::Int(2000 + (i % 20) as i64), Value::Str(body), to_value(&pairs)]);
        weights.push(w);
    }
    (out, weights)
}

fn open(dir: &std::path::Path, name: &str, docs: &[Vec<Value>]) -> Db {
    let db = Db::create(&dir.join(name), schema(), DbOptions { segment_max_docs: 700, ..DbOptions::default() }).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db
}

fn query(rng: &mut impl Rng) -> Vec<(String, f32)> {
    (0..rng.gen_range(3..25)).map(|_| (term((rng.gen::<f64>().powi(2) * 1500.0) as usize), rng.gen_range(0.05f32..2.0))).collect()
}

fn json(q: &[(String, f32)]) -> Value {
    // Repeated query terms add up.
    let mut m: HashMap<&str, f32> = HashMap::new();
    for (t, w) in q {
        *m.entry(t).or_default() += w;
    }
    let pairs: Vec<(String, f32)> = m.into_iter().map(|(t, w)| (t.to_string(), w)).collect();
    to_value(&pairs)
}

/// Brute-force scores over the quantised weights, best first (ties by key).
fn brute(weights: &[HashMap<String, f32>], q: &[(String, f32)], keep: impl Fn(usize) -> bool) -> Vec<(String, f64)> {
    let mut m: HashMap<&str, f32> = HashMap::new();
    for (t, w) in q {
        *m.entry(t).or_default() += w;
    }
    let mut all: Vec<(String, f64)> = weights
        .iter()
        .enumerate()
        .filter(|(i, _)| keep(*i))
        .map(|(i, d)| {
            let s: f64 = m.iter().map(|(t, qw)| d.get(*t).map_or(0.0, |w| *qw as f64 * (quantise(*w) as f64 / SCALE as f64))).sum();
            (format!("d{i:05}"), s)
        })
        .filter(|(_, s)| *s > 0.0)
        .collect();
    all.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.cmp(&b.0)));
    all
}

fn keys(r: &ResultSet) -> Vec<String> {
    r.keys.iter().map(|k| k.as_str().unwrap().to_string()).collect()
}

/// `got` holds the best `k` of `want` up to ties at the boundary, each
/// with its exact score.
fn check(got: &ResultSet, want: &[(String, f64)], k: usize, what: &str) {
    let want_score: HashMap<&str, f64> = want.iter().map(|(d, s)| (d.as_str(), *s)).collect();
    assert_eq!(got.keys.len(), k.min(want.len()), "{what}");
    for (key, s) in keys(got).iter().zip(&got.scores) {
        let w = want_score.get(key.as_str()).copied().unwrap_or(0.0);
        assert!((s - w).abs() < 1e-4 * w.max(1.0), "{what}: {key} scored {s}, brute force {w}");
    }
    if let (Some(last), Some(cut)) = (got.scores.last(), want.get(k.min(want.len()).saturating_sub(1))) {
        assert!((last - cut.1).abs() < 1e-4 * cut.1.max(1.0), "{what}: k-th score {last} vs {}", cut.1);
    }
}

#[test]
fn sparse_top_k_equals_brute_force() {
    let (d, w) = docs(1);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "s.strata", &d);
    let mut rng = rand::rngs::StdRng::seed_from_u64(2);
    for i in 0..30 {
        let q = query(&mut rng);
        let p = Params::new().set("q", json(&q));
        for algo in ["exhaustive", "maxscore", "bmw"] {
            let sql = format!("SELECT id, score() FROM docs ORDER BY similarity(splade, :q, algo => '{algo}') DESC LIMIT 10");
            let r = db.query(&sql, &p).unwrap();
            check(&r, &brute(&w, &q, |_| true), 10, &format!("query {i} {algo}"));
            assert!(r.stats.plan.iter().any(|l| l.contains("sparse")), "{:?}", r.stats.plan);
            let sql = format!("SELECT id, score() FROM docs WHERE year >= 2015 ORDER BY similarity(splade, :q, algo => '{algo}') DESC LIMIT 7");
            let r = db.query(&sql, &p).unwrap();
            check(&r, &brute(&w, &q, |j| 2000 + j % 20 >= 2015), 7, &format!("query {i} {algo} filtered"));
        }
    }
    // A query with no known term finds nothing.
    let r = db.query("SELECT id FROM docs ORDER BY similarity(splade, :q) DESC LIMIT 5", &Params::new().set("q", Value::Str(r#"{"zzz": 1}"#.into()))).unwrap();
    assert!(r.rows.is_empty());
}

/// The coordinator's two rounds over `shards`.
fn sharded(shards: &[&Db], sql: &str, params: &Params) -> ResultSet {
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
    let select = bind(sql, snaps[0].schema(), params).unwrap();
    let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
    let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() };
    let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &select, &opts).unwrap()).collect();
    cluster::merge(&select, answers).unwrap()
}

#[test]
fn shards_fusions_and_products_with_sparse_scores() {
    let (d, _) = docs(3);
    let dir = tempfile::tempdir().unwrap();
    let whole = open(dir.path(), "whole.strata", &d);
    let a = open(dir.path(), "a.strata", &d[..N / 2]);
    let b = open(dir.path(), "b.strata", &d[N / 2..]);
    let mut rng = rand::rngs::StdRng::seed_from_u64(4);
    for _ in 0..5 {
        let q = query(&mut rng);
        let text = q.iter().take(3).map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(" ");
        let p = Params::new().set("q", json(&q)).set("t", Value::Str(text));
        for sql in [
            "SELECT id, score() FROM docs ORDER BY similarity(splade, :q) DESC LIMIT 10",
            "SELECT id, score() FROM docs WHERE year < 2010 ORDER BY similarity(splade, :q) DESC LIMIT 10 OFFSET 3",
            "SELECT id, score() FROM docs ORDER BY rrf(bm25(body, :t), similarity(splade, :q)) DESC LIMIT 10",
            "SELECT id, score() FROM docs ORDER BY 0.5 * bm25(body, :t) + 0.5 * similarity(splade, :q) DESC LIMIT 10",
            "SELECT id, score() FROM docs ORDER BY similarity(splade, :q) * decay(year, 2019, 5) DESC LIMIT 10",
        ] {
            let want = whole.query(sql, &p).unwrap();
            let got = sharded(&[&a, &b], sql, &p);
            assert_eq!(keys(&want), keys(&got), "{sql}");
            for (x, y) in want.scores.iter().zip(&got.scores) {
                assert!((x - y).abs() < 1e-5 * x.abs().max(1.0), "{sql}: {x} vs {y}");
            }
            assert!(!want.rows.is_empty(), "{sql}");
        }
    }
}

#[test]
fn sparse_values_through_sql_writes_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "w.strata", &docs(5).0[..100]);
    let n = db.sql("INSERT INTO docs (id, year, body, splade) VALUES ('new', 2024, 'fresh', '{\"rare\": 2.5, \"t1\": 0.1}')", &Params::new()).unwrap();
    assert!(matches!(n, SqlResult::Written(1)), "{n:?}");
    let r = db.query("SELECT id, splade, score() FROM docs ORDER BY similarity(splade, :q) DESC LIMIT 1", &Params::new().set("q", Value::Str(r#"{"rare": 2}"#.into()))).unwrap();
    assert_eq!(keys(&r), vec!["new"]);
    assert!((r.scores[0] - 5.0).abs() < 1e-6, "{}", r.scores[0]);
    let back = strata::text::sparse::parse(&r.rows[0][1]).unwrap();
    assert_eq!(back, vec![("rare".to_string(), 2.5), ("t1".to_string(), 0.1)]);

    for (sql, msg) in [
        ("SELECT id FROM docs ORDER BY distance(splade, :q) ASC", "not distance"),
        ("SELECT id FROM docs ORDER BY similarity(splade, '[1, 2]') DESC", "JSON object"),
        ("SELECT id FROM docs ORDER BY similarity(splade, '{\"a\": -1}') DESC", "from 0"),
        ("SELECT id FROM docs ORDER BY similarity(year, '{\"a\": 1}') DESC", "VECTOR or SPARSE"),
        ("INSERT INTO docs (id, splade) VALUES ('bad', 'a:1')", "JSON object"),
    ] {
        let p = Params::new().set("q", Value::Str(r#"{"a": 1}"#.into()));
        let e = if sql.starts_with("INSERT") { db.sql(sql, &p).err() } else { db.query(sql, &p).err() };
        let e = e.unwrap_or_else(|| panic!("{sql} was accepted")).to_string();
        assert!(e.contains(msg), "{sql}: {e}");
    }
    let bad = vec![Value::Str("x".into()), Value::Int(1), Value::Null, Value::Str(r#"{"a": "b"}"#.into())];
    assert!(db.load([bad]).is_err());
}
