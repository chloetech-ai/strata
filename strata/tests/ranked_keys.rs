//! `ranked(:keys)`, a ranking computed outside Strata given as primary
//! keys: alone it gives the listed rows that WHERE keeps in list order, as
//! an input of `rrf` it fuses like any ranking, in an expression a row not
//! listed has NULL, the same rows split over shards rank alike, and a plan
//! that uses it needs the `ranked` feature.

use std::collections::HashMap;
use std::sync::Arc;

use strata::cluster;
use strata::data::{self, col};
use strata::db::{Db, SqlResult};
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::ir::{Plan, FEATURES, RANKED};
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

const N: usize = 1200;
const DIM: usize = 16;
const SEG: usize = 200;

fn open(dir: &std::path::Path, name: &str, docs: &[strata::Document]) -> Db {
    let opts = DbOptions { segment_max_docs: SEG, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), data::beir_schema(DIM, VectorIndexKind::Flat), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db
}

fn key(i: usize) -> String {
    format!("d{i:07}")
}

/// A ranking of 150 keys from elsewhere: rows in a scrambled order, a key
/// no row has, and a key listed twice.
fn ranking() -> Vec<String> {
    let mut keys: Vec<String> = (0..150).map(|i| key((i * 37 + 11) % N)).collect();
    keys.insert(20, "d9999999".into());
    keys.insert(40, keys[3].clone());
    keys
}

fn keys_param(keys: &[String]) -> Value {
    Value::List(keys.iter().map(|k| Value::Str(k.clone())).collect())
}

/// The coordinator's rounds over `shards`.
fn sharded(shards: &[Db], sql: &str, params: &Params) -> ResultSet {
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
    let select = bind(sql, snaps[0].schema(), params).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
    let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() };
    let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &select, &opts).unwrap_or_else(|e| panic!("{sql}: {e}"))).collect();
    cluster::merge(&select, answers).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn close(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

/// The same scores, in order; keys in the same order, except that rows
/// tied on score may swap within their tie.
fn same(sql: &str, whole: &ResultSet, split: &ResultSet) {
    assert_eq!(whole.keys.len(), split.keys.len(), "{sql}: row counts");
    for (i, (a, b)) in whole.scores.iter().zip(&split.scores).enumerate() {
        assert!(close(*a, *b), "{sql}: row {i}: {a} vs {b}");
    }
    let mut i = 0;
    while i < whole.keys.len() {
        let mut j = i + 1;
        while j < whole.keys.len() && close(whole.scores[i], whole.scores[j]) {
            j += 1;
        }
        if j < whole.keys.len() {
            let mut a: Vec<String> = whole.keys[i..j].iter().map(|k| format!("{k:?}")).collect();
            let mut b: Vec<String> = split.keys[i..j].iter().map(|k| format!("{k:?}")).collect();
            a.sort();
            b.sort();
            assert_eq!(a, b, "{sql}: rows {i}..{j}");
        }
        i = j;
    }
}

fn keys_of(r: &ResultSet) -> Vec<String> {
    r.keys.iter().map(|k| k.as_str().unwrap().to_string()).collect()
}

#[test]
fn listed_rows_rank_in_list_order() {
    let docs = data::synthetic(N, DIM, 7);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "r.strata", &docs);
    // A deleted row is not ranked, wherever it is listed.
    db.sql(&format!("DELETE FROM docs WHERE id = '{}'", key(11)), &Params::new()).unwrap();
    let keys = ranking();
    let params = Params::new().set("keys", keys_param(&keys));
    let year: HashMap<String, i64> = docs.iter().map(|d| (d[col::ID].as_str().unwrap().to_string(), d[col::YEAR].as_i64().unwrap())).collect();

    // Each live key once, where it is first listed, with its position.
    let mut want: Vec<(String, f64)> = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if year.contains_key(k) && *k != key(11) && !want.iter().any(|(w, _)| w == k) {
            want.push((k.clone(), -((i + 1) as f64)));
        }
    }
    let r = db.query("SELECT id, score() FROM docs ORDER BY ranked(:keys) DESC LIMIT 1000", &params).unwrap();
    assert_eq!(keys_of(&r), want.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>());
    assert_eq!(r.scores, want.iter().map(|(_, s)| *s).collect::<Vec<_>>());
    assert!(r.stats.plan.iter().any(|l| l.starts_with("ranked: ")), "{:?}", r.stats.plan);

    // WHERE keeps some of them, in the same order; LIMIT cuts the list.
    let r = db.query("SELECT id FROM docs WHERE year >= 2010 ORDER BY ranked(:keys) DESC LIMIT 25", &params).unwrap();
    let kept: Vec<String> = want.iter().map(|(k, _)| k.clone()).filter(|k| year[k] >= 2010).take(25).collect();
    assert_eq!(keys_of(&r), kept);

    // A tuple or an array of keys, and one key, read alike.
    let sql = format!("SELECT id FROM docs ORDER BY ranked(('{}', '{}', '{}')) DESC LIMIT 10", key(5), key(3), key(9));
    assert_eq!(keys_of(&db.query(&sql, &Params::new()).unwrap()), [key(5), key(3), key(9)]);
    let sql = format!("SELECT id FROM docs ORDER BY ranked(ARRAY['{}', '{}']) DESC LIMIT 10", key(8), key(2));
    assert_eq!(keys_of(&db.query(&sql, &Params::new()).unwrap()), [key(8), key(2)]);
    let one = Params::new().set("k", Value::Str(key(4)));
    assert_eq!(keys_of(&db.query("SELECT id FROM docs ORDER BY ranked(:k) DESC LIMIT 10", &one).unwrap()), [key(4)]);

    for (sql, wants) in [
        ("SELECT id FROM docs ORDER BY ranked() DESC LIMIT 5", "takes one argument"),
        ("SELECT id FROM docs ORDER BY ranked(:keys, :keys) DESC LIMIT 5", "takes one argument"),
    ] {
        let e = db.query(sql, &params).unwrap_err().to_string();
        assert!(e.contains(wants), "{sql}: {e}");
    }
}

/// RRF over BM25 and a ranking by key scores each row Σ 1 / (k + its
/// place) over the inputs' top `depth` rows that WHERE keeps (a page of
/// `depth` rows, which does not raise it).
#[test]
fn rrf_fuses_a_ranking_given_by_key() {
    let docs = data::synthetic(N, DIM, 9);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "f.strata", &docs);
    let keys = ranking();
    let title = docs[42][col::TITLE].as_str().unwrap().to_string();
    let text: String = title.split(' ').take(3).collect::<Vec<_>>().join(" ");
    let params = Params::new().set("keys", keys_param(&keys)).set("t", Value::Str(text));
    let (k, depth) = (10.0, 40);
    for filter in ["", "WHERE year >= 2005"] {
        let bm25 = db.query(&format!("SELECT id FROM docs {filter} ORDER BY bm25(body, :t) DESC, id LIMIT {depth}"), &params).unwrap();
        let listed = db.query(&format!("SELECT id FROM docs {filter} ORDER BY ranked(:keys) DESC LIMIT {depth}"), &params).unwrap();
        let mut want: HashMap<String, f64> = HashMap::new();
        for list in [&bm25, &listed] {
            for (i, key) in keys_of(list).into_iter().enumerate() {
                *want.entry(key).or_default() += 1.0 / (k + (i + 1) as f64);
            }
        }
        let sql = format!("SELECT id, score() FROM docs {filter} ORDER BY rrf(bm25(body, :t), ranked(:keys), k => {k}, depth => {depth}) DESC LIMIT {depth}");
        let r = db.query(&sql, &params).unwrap();
        let mut best: Vec<f64> = want.values().copied().collect();
        best.sort_by(|a, b| b.total_cmp(a));
        best.truncate(depth);
        assert_eq!(r.keys.len(), best.len(), "{sql}");
        for ((key, score), b) in keys_of(&r).iter().zip(&r.scores).zip(&best) {
            assert!(close(want[key], *score) && close(*b, *score), "{sql}: {key}: {} vs {score}", want[key]);
        }
    }

    // In an expression, a row the list leaves out has NULL.
    let sql = "SELECT id, score() FROM docs ORDER BY minmax(bm25(body, :t)) + coalesce(ranked(:keys), -1000) DESC LIMIT 1000";
    let r = db.query(sql, &params).unwrap();
    let listed: Vec<String> = keys_of(&r).into_iter().zip(&r.scores).filter(|(_, s)| **s > -999.0).map(|(k, _)| k).collect();
    assert!(!listed.is_empty() && listed.len() < r.keys.len(), "{sql}: {} of {}", listed.len(), r.keys.len());
    assert!(listed.iter().all(|k| keys.contains(k)), "{sql}");
    assert!(keys_of(&r).iter().zip(&r.scores).filter(|(_, s)| **s <= -999.0).all(|(k, _)| !keys.contains(k)), "{sql}");
}

#[test]
fn shards_rank_by_keys_like_one_database() {
    let docs = data::synthetic(N, DIM, 13);
    let dir = tempfile::tempdir().unwrap();
    let whole = open(dir.path(), "whole.strata", &docs);
    let splits: Vec<Vec<Db>> = [1, 2, 4]
        .iter()
        .map(|&k| (0..k).map(|i| open(dir.path(), &format!("s{k}-{i}.strata"), &docs[i * N / k..(i + 1) * N / k])).collect())
        .collect();
    let title = docs[42][col::TITLE].as_str().unwrap().to_string();
    let text: String = title.split(' ').take(3).collect::<Vec<_>>().join(" ");
    let params = Params::new().set("keys", keys_param(&ranking())).set("t", Value::Str(text)).set("q", docs[42][col::EMB].clone());
    let shapes = [
        "ranked(:keys)",
        "rrf(bm25(body, :t), ranked(:keys))",
        "rrf(similarity(emb, :q), ranked(:keys), k => 10, depth => 40)",
        "0.5 * minmax(bm25(body, :t)) + 0.5 * minmax(ranked(:keys))",
        "ranked(:keys) + 0.01 * coalesce(rating, 0)",
        "similarity(emb, :q) + coalesce(ranked(:keys), -1000)",
    ];
    for s in shapes {
        for filter in ["", "WHERE year >= 2005"] {
            let sql = format!("SELECT id, score() FROM docs {filter} ORDER BY {s} DESC LIMIT 30");
            let want = whole.query(&sql, &params).unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert!(!want.keys.is_empty(), "{sql}");
            for shards in &splits {
                same(&format!("{sql} on {} shards", shards.len()), &want, &sharded(shards, &sql, &params));
            }
        }
    }
}

/// A plan that ranks by keys needs the `ranked` feature, so a reader from
/// before it refuses the plan by name; its JSON gives the same rows.
#[test]
fn a_plan_ranked_by_keys_needs_its_feature() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "p.strata", &data::synthetic(200, DIM, 5));
    let params = Params::new().set("keys", keys_param(&[key(7), key(3), key(150)]));
    let old: Vec<&str> = FEATURES.iter().copied().filter(|f| *f != RANKED).collect();
    for sql in [
        "SELECT id FROM docs ORDER BY ranked(:keys) DESC LIMIT 5",
        "SELECT id FROM docs ORDER BY rrf(bm25(body, 'x'), ranked(:keys)) DESC LIMIT 5",
        "SELECT id FROM docs WHERE id IN (SELECT id FROM docs ORDER BY ranked(:keys) DESC LIMIT 2)",
    ] {
        let plan = db.explain(sql, &params).unwrap();
        let json = plan.to_json().unwrap();
        let back = Plan::from_json(&json).unwrap();
        let features: Vec<&str> = back.features.iter().map(String::as_str).collect();
        assert!(features.contains(&RANKED), "{sql}: {features:?}");
        let e = Plan::readable_by(back.version as u64, &features, &old).unwrap_err().to_string();
        assert!(e.contains("ranked"), "{sql}: {e}");
        let SqlResult::Rows(ran) = db.run(&back).unwrap() else { panic!("{sql}: a read") };
        assert_eq!(ran.rows, db.query(sql, &params).unwrap().rows, "{sql}");
        let lines = back.explain(&[db.snapshot().schema()]).join("\n");
        assert!(lines.contains(&format!("ranked(ARRAY['{}', '{}', '{}'])", key(7), key(3), key(150))), "{sql}:\n{lines}");
    }
    assert_eq!(db.query("SELECT id FROM docs ORDER BY ranked(:keys) DESC LIMIT 5", &params).unwrap().keys, [Value::Str(key(7)), Value::Str(key(3)), Value::Str(key(150))]);
}
