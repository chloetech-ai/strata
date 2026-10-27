//! A collection split into shards answers like the unsplit one: every
//! query runs on one database holding all documents, and on two shards that
//! hold the same segments, queried with their summed statistics in shard
//! mode and merged by `cluster::merge`. Keys must match in order and scores
//! to 1e-6 (rows tied on score may swap).

use std::sync::Arc;

use strata::cluster;
use strata::data;
use strata::db::Db;
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

const N: usize = 3000;
const DIM: usize = 16;
const SEG: usize = 500;

fn open(dir: &std::path::Path, name: &str, docs: &[strata::Document]) -> Db {
    let opts = DbOptions { segment_max_docs: SEG, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), data::beir_schema(DIM, VectorIndexKind::Flat), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db
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

fn same(sql: &str, whole: &ResultSet, split: &ResultSet) {
    assert_eq!(whole.rows.len(), split.rows.len(), "{sql}: row counts");
    assert_eq!(whole.columns, split.columns, "{sql}");
    let mut i = 0;
    while i < whole.rows.len() {
        // Rows tied on score (within 1e-9) may come in any order.
        let mut j = i + 1;
        while j < whole.rows.len() && (whole.scores.get(j).zip(whole.scores.get(i))).is_some_and(|(a, b)| (a - b).abs() < 1e-9) {
            j += 1;
        }
        let mut a: Vec<String> = whole.rows[i..j].iter().map(|r| format!("{r:?}")).collect();
        let mut b: Vec<String> = split.rows[i..j].iter().map(|r| format!("{r:?}")).collect();
        a.sort();
        b.sort();
        if a != b {
            // Allow score columns to differ in the last digits.
            for (x, y) in whole.rows[i..j].iter().zip(&split.rows[i..j]) {
                for (u, v) in x.iter().zip(y) {
                    match (u, v) {
                        (Value::Float(p), Value::Float(q)) => assert!((p - q).abs() < 1e-6, "{sql}: {p} vs {q}"),
                        _ => assert_eq!(u, v, "{sql}: row {i}"),
                    }
                }
            }
        }
        i = j;
    }
}

#[test]
fn split_collection_answers_like_the_whole() {
    let docs = data::synthetic(N, DIM, 7);
    let dir = tempfile::tempdir().unwrap();
    let whole = open(dir.path(), "whole.strata", &docs);
    let a = open(dir.path(), "a.strata", &docs[..N / 2]);
    let b = open(dir.path(), "b.strata", &docs[N / 2..]);
    assert_eq!(whole.snapshot().segments().len(), a.snapshot().segments().len() + b.snapshot().segments().len());

    // A query vector and words that occur in the corpus.
    let q = docs[42][data::col::EMB].clone();
    let title = docs[42][data::col::TITLE].as_str().unwrap().to_string();
    let words: Vec<&str> = title.split(' ').take(3).collect();
    let text = words.join(" ");
    let params = Params::new().set("q", q).set("t", Value::Str(text.clone()));
    let queries = [
        "SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 10".to_string(),
        "SELECT id, score() FROM docs WHERE year >= 2005 AND category IN ('c00', 'c01', 'c02') ORDER BY bm25(body, :t) DESC LIMIT 20".into(),
        "SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 5 OFFSET 7".into(),
        "SELECT id, score() FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 10".into(),
        "SELECT id, score() FROM docs WHERE rating > 2.5 ORDER BY similarity(emb, :q) DESC LIMIT 10".into(),
        "SELECT id, score() FROM docs ORDER BY 0.3 * bm25(body, :t) + 0.7 * similarity(emb, :q) DESC LIMIT 10".into(),
        "SELECT id, score() FROM docs WHERE year < 2015 ORDER BY 0.5 * bm25(body, :t, depth => 40) + 0.5 * similarity(emb, :q) DESC LIMIT 15".into(),
        "SELECT id, score() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 10".into(),
        "SELECT id, rating FROM docs WHERE category = 'c03' ORDER BY rating DESC, id LIMIT 12 OFFSET 3".into(),
        "SELECT id, year FROM docs WHERE MATCH(body, :t) ORDER BY year, id LIMIT 25".into(),
        "SELECT category, COUNT(*) FROM docs WHERE year >= 2000 GROUP BY category ORDER BY COUNT(*) DESC LIMIT 8".into(),
        "SELECT topic, COUNT(*) FROM docs GROUP BY topic ORDER BY topic LIMIT 5 OFFSET 2".into(),
        "SELECT COUNT(*) FROM docs WHERE MATCH(body, :t) AND rating < 4".into(),
        format!("SELECT COUNT(*) FROM docs WHERE MATCH(body, '{}')", words[0]),
    ];
    for sql in &queries {
        let want = whole.query(sql, &params).unwrap();
        let got = sharded(&[&a, &b], sql, &params);
        same(sql, &want, &got);
        eprintln!("ok ({} rows): {sql}", want.rows.len());
    }

    // Without the summed statistics, a shard's BM25 scores differ.
    let sql = &queries[0];
    let select = bind(sql, a.schema(), &params).unwrap();
    let local = a.execute(&a.snapshot(), &select, &ExecOptions { shard: true, ..ExecOptions::default() }).unwrap();
    let global = {
        let stats = TermStats::sum(&[a.term_stats(&a.snapshot(), &select).unwrap(), b.term_stats(&b.snapshot(), &select).unwrap()]);
        a.execute(&a.snapshot(), &select, &ExecOptions { stats: Some(Arc::new(stats)), shard: true, ..ExecOptions::default() }).unwrap()
    };
    assert_ne!(local.scores, global.scores);
}

#[test]
fn shard_mode_refuses_graph_queries() {
    let docs = data::synthetic(200, DIM, 3);
    let dir = tempfile::tempdir().unwrap();
    let a = open(dir.path(), "a.strata", &docs);
    // Vector neighbours count as edges, and they cross shards.
    let sql = "SELECT id FROM docs WHERE REACHABLE(emb, 'd0000001', hops => 2) LIMIT 10";
    let select = bind(sql, a.schema(), &Params::new()).unwrap();
    assert!(a.execute(&a.snapshot(), &select, &ExecOptions::default()).is_ok());
    let r = a.execute(&a.snapshot(), &select, &ExecOptions { shard: true, ..ExecOptions::default() });
    assert!(matches!(r, Err(strata::Error::Query(_))), "{r:?}");
}

#[test]
fn deadline_stops_a_query() {
    let docs = data::synthetic(1000, DIM, 5);
    let dir = tempfile::tempdir().unwrap();
    let a = open(dir.path(), "a.strata", &docs);
    let past = std::time::Instant::now() - std::time::Duration::from_millis(1);
    let opts = ExecOptions { deadline: Some(past), ..ExecOptions::default() };
    let r = a.query_with("SELECT id FROM docs ORDER BY bm25(body, 'kalo') DESC LIMIT 3", &Params::new(), &opts);
    assert!(matches!(r, Err(strata::Error::Timeout(_))), "{r:?}");
}
