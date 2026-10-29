//! A collection split into shards answers like the unsplit one: every
//! query runs on one database holding all documents, and on two shards that
//! hold the same segments, queried with their summed statistics in shard
//! mode and merged by `cluster::merge`, in one round or as query then
//! fetch. Keys must match in order and scores to 1e-6 (rows tied on score
//! may swap).

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

/// The rounds again, as query then fetch: the shards rank the keys, and
/// those that hold the page's rows fetch their columns.
fn fetched(shards: &[&Db], sql: &str, params: &Params) -> ResultSet {
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
    let schema = snaps[0].schema();
    let select = bind(sql, schema, params).unwrap();
    assert!(cluster::fetches(&select, schema), "{sql}");
    let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
    let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() };
    let ids = cluster::ids(&select, schema).unwrap();
    let answers: Vec<ResultSet> = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &ids, &opts).unwrap()).collect();
    // Each answer is the key alone.
    assert!(answers.iter().all(|a| a.rows.iter().zip(&a.keys).all(|(r, k)| r == std::slice::from_ref(k))), "{sql}");
    let owner: std::collections::HashMap<String, usize> = answers.iter().enumerate().flat_map(|(i, a)| a.keys.iter().map(move |k| (format!("{k:?}"), i))).collect();
    let ranked = cluster::merge(&ids, answers).unwrap();
    let mut keys = vec![Vec::new(); shards.len()];
    for k in &ranked.keys {
        keys[owner[&format!("{k:?}")]].push(k.clone());
    }
    let fetched = shards.iter().zip(&snaps).zip(keys).map(|((d, s), k)| d.execute(s, &cluster::fetch(&select, schema, k).unwrap(), &opts).unwrap()).collect();
    let (rs, lost) = cluster::fill(&select, ranked, fetched).unwrap();
    assert_eq!(lost, 0, "{sql}");
    rs
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
        "SELECT category, COUNT(*), AVG(rating), MIN(year), MAX(year), COUNT(DISTINCT topic) FROM docs GROUP BY category ORDER BY AVG(rating) DESC LIMIT 6".into(),
        "SELECT category, topic, COUNT(*) FROM docs WHERE year >= 2000 GROUP BY category, topic HAVING COUNT(*) > 3 ORDER BY 3 DESC, 1 LIMIT 10".into(),
        "SELECT year, SUM(rating) FROM docs GROUP BY year ORDER BY year DESC LIMIT 4 OFFSET 1".into(),
        "SELECT COUNT(*), SUM(year), MIN(rating), COUNT(DISTINCT category) FROM docs WHERE rating > 2".into(),
        "SELECT topic FROM docs GROUP BY topic LIMIT 5".into(),
        format!("SELECT COUNT(*) FROM docs WHERE MATCH(body, '{}')", words[0]),
    ];
    for sql in &queries {
        let want = whole.query(sql, &params).unwrap();
        let got = sharded(&[&a, &b], sql, &params);
        same(sql, &want, &got);
        eprintln!("ok ({} rows): {sql}", want.rows.len());
    }
    // Query then fetch, for queries that select stored columns.
    let wide = [
        "SELECT id, title, year, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 10",
        "SELECT title, id, category FROM docs WHERE year >= 2005 ORDER BY bm25(body, :t) DESC LIMIT 7 OFFSET 4",
        "SELECT score(), text FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 10",
        "SELECT id, title, score() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 10",
        "SELECT id, rating, title FROM docs WHERE category = 'c03' ORDER BY rating DESC, id LIMIT 12 OFFSET 3",
        "SELECT year, topic FROM docs WHERE MATCH(body, :t) ORDER BY year, id LIMIT 25",
    ];
    for sql in wide {
        let want = whole.query(sql, &params).unwrap();
        assert!(!want.rows.is_empty(), "{sql}");
        same(sql, &want, &fetched(&[&a, &b], sql, &params));
    }
    // Groups, counts, the key and score alone, and every row come in one
    // round.
    for sql in [
        "SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 10",
        "SELECT COUNT(*) FROM docs",
        "SELECT topic, COUNT(*) FROM docs GROUP BY topic",
        "SELECT id, title FROM docs WHERE year >= 2005",
    ] {
        assert!(!cluster::fetches(&bind(sql, a.schema(), &params).unwrap(), a.schema()), "{sql}");
    }
    // A row that changed between the phases is left out, and counted.
    let sql = wide[0];
    let select = bind(sql, a.schema(), &params).unwrap();
    let ids = cluster::ids(&select, a.schema()).unwrap();
    let ranked = a.execute(&a.snapshot(), &ids, &ExecOptions { shard: true, ..ExecOptions::default() }).unwrap();
    let mut keys = ranked.keys.clone();
    let gone = keys.remove(0);
    let one = a.execute(&a.snapshot(), &cluster::fetch(&select, a.schema(), keys).unwrap(), &ExecOptions { shard: true, ..ExecOptions::default() }).unwrap();
    let (rs, lost) = cluster::fill(&select, cluster::merge(&ids, vec![ranked]).unwrap(), vec![one]).unwrap();
    assert_eq!((lost, rs.rows.len()), (1, 9));
    assert!(!rs.keys.contains(&gone));

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
fn tied_scores_cut_and_rank_alike() {
    // 40 distinct texts over 3,000 rows: BM25 ties in runs of about 75,
    // so the candidate cut (depth) and RRF's ranks fall inside ties. One
    // database and the shards must cut and rank them alike (ties by key).
    let mut docs = data::synthetic(N, DIM, 11);
    let texts: Vec<Vec<Value>> = docs[..40].iter().map(|d| d[data::col::TITLE..=data::col::BODY].to_vec()).collect();
    for (i, d) in docs.iter_mut().enumerate() {
        d.splice(data::col::TITLE..=data::col::BODY, texts[i % texts.len()].iter().cloned());
    }
    // Loaded in descending key order, so a row's place in its segment
    // does not follow its key.
    docs.reverse();
    let dir = tempfile::tempdir().unwrap();
    let whole = open(dir.path(), "whole.strata", &docs);
    let a = open(dir.path(), "a.strata", &docs[..N / 2]);
    let b = open(dir.path(), "b.strata", &docs[N / 2..]);
    let word = docs[0][data::col::TITLE].as_str().unwrap().split(' ').next().unwrap().to_string();
    let params = Params::new().set("q", docs[7][data::col::EMB].clone()).set("t", Value::Str(word));
    let queries = [
        "SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 10",
        "SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 5 OFFSET 7",
        "SELECT id, score() FROM docs WHERE year >= 2005 ORDER BY bm25(body, :t) DESC LIMIT 10",
        "SELECT id, score() FROM docs ORDER BY decay(year, 2020, 5) DESC LIMIT 10",
        "SELECT id, score() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 10",
        "SELECT id, score() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q), depth => 7) DESC LIMIT 10",
        "SELECT id, score() FROM docs ORDER BY bm25(body, :t, depth => 7) * decay(year, 2020, 5) DESC LIMIT 10",
        "SELECT id, score() FROM docs ORDER BY 0.5 * bm25(body, :t, depth => 7) + 0.5 * similarity(emb, :q) DESC LIMIT 10",
    ];
    for sql in queries {
        let want = whole.query(sql, &params).unwrap();
        let got = sharded(&[&a, &b], sql, &params);
        assert!(!want.rows.is_empty(), "{sql}");
        let ids = |r: &ResultSet| r.rows.iter().map(|row| row[0].clone()).collect::<Vec<_>>();
        assert_eq!(ids(&want), ids(&got), "{sql}: {:?} vs {:?}", want.rows, got.rows);
        same(sql, &want, &got);
    }
}

// Bug 9 and SCALE-14: a fused ranking's depth is raised to LIMIT + OFFSET
// on the shards and in the merge, and a ranked query without LIMIT answers
// 100 rows after its OFFSET on one database as through a coordinator.
#[test]
fn pages_past_the_depth_split_alike() {
    let docs = data::synthetic(N, DIM, 7);
    let dir = tempfile::tempdir().unwrap();
    let whole = open(dir.path(), "whole.strata", &docs);
    let a = open(dir.path(), "a.strata", &docs[..N / 2]);
    let b = open(dir.path(), "b.strata", &docs[N / 2..]);
    let title = docs[42][data::col::TITLE].as_str().unwrap().to_string();
    let text = title.split(' ').take(3).collect::<Vec<_>>().join(" ");
    let params = Params::new().set("q", docs[42][data::col::EMB].clone()).set("t", Value::Str(text));
    let queries = [
        ("SELECT id, score() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q), depth => 7) DESC LIMIT 10 OFFSET 20", 10),
        ("SELECT id, score() FROM docs ORDER BY 0.5 * bm25(body, :t, depth => 5) + 0.5 * similarity(emb, :q) DESC LIMIT 30", 30),
        ("SELECT id, score() FROM docs ORDER BY bm25(body, :t, depth => 7) * decay(year, 2020, 5) DESC LIMIT 10 OFFSET 15", 10),
        ("SELECT id, score() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC OFFSET 10", 100),
        ("SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC OFFSET 30", 100),
        ("SELECT id, score() FROM docs ORDER BY similarity(emb, :q) DESC OFFSET 50", 100),
    ];
    for (sql, n) in queries {
        let want = whole.query(sql, &params).unwrap();
        assert_eq!(want.rows.len(), n, "{sql}");
        same(sql, &want, &sharded(&[&a, &b], sql, &params));
    }
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

// SCALE-2/OPS-3: a query whose caller went away kept running to the end.
// Its cancellation flag now stops it where the deadline would.
#[test]
fn a_cancelled_query_stops() {
    let docs = data::synthetic(1000, DIM, 5);
    let dir = tempfile::tempdir().unwrap();
    let a = open(dir.path(), "a.strata", &docs);
    let sql = "SELECT id FROM docs ORDER BY bm25(body, 'kalo') DESC LIMIT 3";
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let opts = ExecOptions { cancel: Some(flag.clone()), ..ExecOptions::default() };
    assert!(a.query_with(sql, &Params::new(), &opts).is_ok());
    flag.store(true, std::sync::atomic::Ordering::Relaxed);
    let r = a.query_with(sql, &Params::new(), &opts);
    assert!(matches!(&r, Err(strata::Error::Timeout(m)) if m.contains("cancelled")), "{r:?}");
}
