//! `decay(field, origin, scale, ...)` and products of scores
//! (`bm25(..) * decay(..)`): the functions' values, rankings equal to a
//! brute-force computation, the same answers from shards merged by
//! `cluster::merge`, and the errors.

use std::collections::HashMap;
use std::sync::Arc;

use strata::cluster;
use strata::data::{self, col};
use strata::db::Db;
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::{bind, Decay, DecayShape, Params};
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

const N: usize = 3000;
const DIM: usize = 16;

fn open(dir: &std::path::Path, name: &str, docs: &[strata::Document]) -> Db {
    let opts = DbOptions { segment_max_docs: 500, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), data::beir_schema(DIM, VectorIndexKind::Flat), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db
}

fn ids(r: &ResultSet) -> Vec<String> {
    r.keys.iter().map(|k| k.as_str().unwrap().to_string()).collect()
}

#[test]
fn decay_functions_have_their_shapes() {
    for shape in [DecayShape::Exp, DecayShape::Gauss, DecayShape::Linear] {
        let d = Decay { origin: 100.0, scale: 10.0, offset: 2.0, decay: 0.5, shape };
        assert_eq!(d.eval(100.0), 1.0, "{shape:?} at the origin");
        assert_eq!(d.eval(98.0), 1.0, "{shape:?} inside the offset");
        assert!((d.eval(112.0) - 0.5).abs() < 1e-12, "{shape:?} at offset + scale: {}", d.eval(112.0));
        assert!((d.eval(88.0) - 0.5).abs() < 1e-12, "{shape:?} is symmetric");
        assert!(d.eval(120.0) < d.eval(110.0), "{shape:?} falls");
    }
    let lin = Decay { origin: 0.0, scale: 10.0, offset: 0.0, decay: 0.5, shape: DecayShape::Linear };
    assert_eq!(lin.eval(20.0), 0.0, "linear reaches 0 at scale / (1 - decay)");
    assert_eq!(lin.eval(50.0), 0.0);
}

#[test]
fn decay_and_products_rank_like_brute_force() {
    let docs = data::synthetic(N, DIM, 11);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "d.strata", &docs);
    let year: HashMap<String, Option<i64>> = docs.iter().map(|d| (d[col::ID].as_str().unwrap().to_string(), d[col::YEAR].as_i64())).collect();
    let f = Decay { origin: 2012.0, scale: 4.0, offset: 1.0, decay: 0.3, shape: DecayShape::Gauss };
    let decay_of = |id: &str| year[id].map_or(0.0, |y| f.eval(y as f64));

    // ORDER BY decay alone: every row of the filter, by decay, ties by key.
    let r = db.query("SELECT id, score() FROM docs WHERE category IN ('c00', 'c01', 'c02') ORDER BY decay(year, 2012, 4, offset => 1, decay => 0.3, shape => 'gauss') DESC LIMIT 25", &Params::new()).unwrap();
    let mut want: Vec<(String, f64)> = docs
        .iter()
        .filter(|d| matches!(d[col::CATEGORY].as_str(), Some("c00" | "c01" | "c02")))
        .map(|d| {
            let id = d[col::ID].as_str().unwrap().to_string();
            let s = decay_of(&id);
            (id, s)
        })
        .collect();
    want.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.cmp(&b.0)));
    want.truncate(25);
    assert_eq!(ids(&r), want.iter().map(|w| w.0.clone()).collect::<Vec<_>>());
    for (got, (_, w)) in r.scores.iter().zip(&want) {
        assert!((got - w).abs() < 1e-6, "{got} vs {w}");
    }

    // bm25 * decay: BM25's top 100 rescored by the product.
    let text = docs[7][col::TITLE].as_str().unwrap().split(' ').take(3).collect::<Vec<_>>().join(" ");
    let p = Params::new().set("t", Value::Str(text));
    let top = db.query("SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 100", &p).unwrap();
    let mut want: Vec<(String, f64)> =
        ids(&top).into_iter().zip(&top.scores).map(|(id, s)| (id.clone(), (*s as f32 as f64) * (decay_of(&id) as f32 as f64))).collect();
    want.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.cmp(&b.0)));
    want.truncate(10);
    let r = db.query("SELECT id, score() FROM docs ORDER BY bm25(body, :t) * decay(year, 2012, 4, offset => 1, decay => 0.3, shape => 'gauss') DESC LIMIT 10", &p).unwrap();
    assert_eq!(ids(&r), want.iter().map(|w| w.0.clone()).collect::<Vec<_>>());
    for (got, (_, w)) in r.scores.iter().zip(&want) {
        assert!((got - w).abs() < 1e-4 * w.abs().max(1.0), "{got} vs {w}");
    }
    assert!(r.stats.plan.iter().any(|l| l.starts_with("fused bm25(body) * decay(year): ")), "{:?}", r.stats.plan);

    // A product of a column and a decay (no relevance factor): every
    // factor's top rows are candidates.
    let r = db.query("SELECT id, rating, year FROM docs ORDER BY rating * decay(year, 2020, 2) DESC LIMIT 5", &Params::new()).unwrap();
    assert_eq!(r.rows.len(), 5);
    let s: Vec<f64> = r.rows.iter().map(|row| row[1].as_f64().unwrap() * f64::from(Decay { origin: 2020.0, scale: 2.0, offset: 0.0, decay: 0.5, shape: DecayShape::Exp }.eval(row[2].as_i64().unwrap() as f64) as f32)).collect();
    assert!(s.windows(2).all(|w| w[0] >= w[1] - 1e-6), "{s:?}");

    // In a weighted sum and in RRF.
    let r = db.query("SELECT id FROM docs ORDER BY 0.7 * bm25(body, :t) + 0.3 * decay(year, 2012, 4) DESC LIMIT 10", &p).unwrap();
    assert_eq!(r.rows.len(), 10);
    let r = db.query("SELECT id FROM docs ORDER BY rrf(bm25(body, :t), decay(year, now(), 3650)) DESC LIMIT 10", &p).unwrap();
    assert_eq!(r.rows.len(), 10);
}

/// The coordinator's two rounds over `shards`.
fn sharded(shards: &[&Db], sql: &str, params: &Params) -> ResultSet {
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
    let select = bind(sql, snaps[0].schema(), params).unwrap();
    let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
    let stats = Arc::new(TermStats::sum(&parts));
    let opts = ExecOptions { stats: Some(stats), shard: true, ..ExecOptions::default() };
    let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &select, &opts).unwrap()).collect();
    cluster::merge(&select, answers).unwrap()
}

#[test]
fn shards_merge_decay_and_products_like_one_database() {
    let docs = data::synthetic(N, DIM, 5);
    let dir = tempfile::tempdir().unwrap();
    let whole = open(dir.path(), "whole.strata", &docs);
    let a = open(dir.path(), "a.strata", &docs[..N / 3]);
    let b = open(dir.path(), "b.strata", &docs[N / 3..]);
    let text = docs[3][col::TITLE].as_str().unwrap().split(' ').take(3).collect::<Vec<_>>().join(" ");
    let p = Params::new().set("t", Value::Str(text)).set("q", docs[9][col::EMB].clone());
    for sql in [
        "SELECT id, score() FROM docs ORDER BY decay(year, 2010, 5) DESC LIMIT 15",
        "SELECT id, score() FROM docs WHERE rating > 2 ORDER BY bm25(body, :t) * decay(year, 2010, 5, shape => 'linear') DESC LIMIT 12",
        "SELECT id, score() FROM docs ORDER BY similarity(emb, :q) * decay(rating, 5, 1) DESC LIMIT 10",
        "SELECT id, score() FROM docs ORDER BY bm25(body, :t, depth => 40) * similarity(emb, :q) * decay(year, 2000, 10) DESC LIMIT 10 OFFSET 2",
        "SELECT id, score() FROM docs ORDER BY 0.6 * bm25(body, :t) + 0.4 * decay(year, 2015, 3) DESC LIMIT 10",
    ] {
        let want = whole.query(sql, &p).unwrap();
        let got = sharded(&[&a, &b], sql, &p);
        assert_eq!(ids(&want), ids(&got), "{sql}");
        for (x, y) in want.scores.iter().zip(&got.scores) {
            assert!((x - y).abs() < 1e-6 * x.abs().max(1.0), "{sql}: {x} vs {y}");
        }
    }
}

#[test]
fn bad_decays_and_products_are_refused() {
    let docs = data::synthetic(50, DIM, 2);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "e.strata", &docs);
    for (sql, msg) in [
        ("SELECT id FROM docs ORDER BY decay(category, 1, 1) DESC", "numeric"),
        ("SELECT id FROM docs ORDER BY decay(year, 2000, 0) DESC", "scale > 0"),
        ("SELECT id FROM docs ORDER BY decay(year, 2000, 5, decay => 1.5) DESC", "0 < decay < 1"),
        ("SELECT id FROM docs ORDER BY decay(year, 2000, 5, shape => 'cubic') DESC", "shape"),
        ("SELECT id FROM docs ORDER BY decay(year, 'soon', 5) DESC", "origin"),
        ("SELECT id FROM docs ORDER BY decay(year, 2000) DESC", "decay(field, origin, scale"),
        ("SELECT id FROM docs ORDER BY bm25(body, 'x') * 2 * decay(year, 2000, 5) DESC", "constant"),
        ("SELECT id FROM docs ORDER BY rrf(bm25(body, 'x'), year) * decay(year, 2000, 5) DESC", "not sums or fusions"),
    ] {
        let e = db.query(sql, &Params::new()).err().unwrap_or_else(|| panic!("{sql} was accepted")).to_string();
        assert!(e.contains(msg), "{sql}: {e}");
    }
}
