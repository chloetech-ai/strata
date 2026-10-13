//! End to end: SciFact loaded into a Strata file through `Db`, queried with
//! the shared workload's SQL, and checked against direct computations.

use std::collections::HashMap;

use strata::data::{self, col};
use strata::db::Db;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::workload::{self, Kind, WorkloadConfig};
use strata::VectorIndexKind;

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn ids(rs: &strata::query::exec::ResultSet) -> Vec<String> {
    rs.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

#[test]
fn scifact_end_to_end() {
    if !data::data_dir().join("scifact/meta.json").exists() {
        eprintln!("SciFact not prepared; skipping");
        return;
    }
    let coll = data::load("scifact", None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scifact.strata");
    // Several segments (2,000 documents each) so that merging across segments is exercised.
    let opts = DbOptions { segment_max_docs: 2000, ..DbOptions::default() };
    let db = Db::create(&path, data::beir_schema(coll.dim, VectorIndexKind::Hnsw { m: 16, ef_construction: 100 }), opts).unwrap();
    db.load(coll.docs.iter().cloned()).unwrap();
    assert!(db.snapshot().segments().len() >= 3);

    let w = workload::generate(&coll, &WorkloadConfig::default());
    let of = |k: Kind| -> Vec<&workload::WorkloadQuery> { w.of_kind(k).into_iter().map(|i| &w.queries[i]).collect() };
    for k in [Kind::Filter, Kind::Vector, Kind::FilteredVector, Kind::Bm25, Kind::And2, Kind::Facet, Kind::Hybrid] {
        eprintln!("{k:?}: {} queries", of(k).len());
        assert!(!of(k).is_empty(), "no {k:?} queries");
    }
    let params = |q: Option<usize>| match q {
        Some(i) => Params::new().set("q", Value::Vector(coll.queries[i].vector.clone())),
        None => Params::new(),
    };

    // Structured filters: exact counts and the top-10 by rating, ties by id.
    for q in of(Kind::Filter) {
        let n = db.query(q.count_sql.as_ref().unwrap(), &Params::new()).unwrap();
        assert_eq!(n.rows[0][0], Value::Int(q.matches.unwrap() as i64), "{}", q.sql);
        let got = ids(&db.query(&q.sql, &Params::new()).unwrap());
        let mut want: Vec<(f64, &str)> = coll
            .docs
            .iter()
            .filter(|d| q.filter.iter().all(|p| p.matches(d)))
            .map(|d| (d[col::RATING].as_f64().unwrap(), d[col::ID].as_str().unwrap()))
            .collect();
        want.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(a.1.cmp(b.1)));
        let want: Vec<String> = want.into_iter().take(10).map(|x| x.1.to_string()).collect();
        assert_eq!(got, want, "{}", q.sql);
    }

    // Exact vector search (pre-filter strategy) against brute force, with and without filters.
    for q in of(Kind::Vector).into_iter().chain(of(Kind::FilteredVector)).take(60) {
        let qi = q.query.unwrap();
        let sql = q.sql.replace("similarity(emb, :q)", "similarity(emb, :q, strategy => 'prefilter', rerank => 200)");
        let got = ids(&db.query(&sql, &params(Some(qi))).unwrap());
        let mut want: Vec<(f32, &str)> = coll
            .docs
            .iter()
            .filter(|d| q.filter.iter().all(|p| p.matches(d)))
            .map(|d| (dot(d[col::EMB].as_vector().unwrap(), &coll.queries[qi].vector), d[col::ID].as_str().unwrap()))
            .collect();
        want.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        let want: Vec<String> = want.into_iter().take(10).map(|x| x.1.to_string()).collect();
        assert_eq!(got.len(), want.len(), "{sql}");
        let overlap = got.iter().filter(|x| want.contains(x)).count();
        assert!(overlap >= want.len().saturating_sub(1), "{sql}: {got:?} vs {want:?}");
    }

    // Planner-chosen vector plans keep high recall.
    let mut recall = Vec::new();
    for q in of(Kind::FilteredVector).into_iter().take(40) {
        let qi = q.query.unwrap();
        let auto = ids(&db.query(&q.sql, &params(Some(qi))).unwrap());
        let sql = q.sql.replace("similarity(emb, :q)", "similarity(emb, :q, strategy => 'prefilter', rerank => 200)");
        let exact = ids(&db.query(&sql, &params(Some(qi))).unwrap());
        recall.push(strata::metrics::recall(&auto, &exact));
    }
    let mean = strata::metrics::mean(&recall);
    assert!(mean > 0.9, "planner recall {mean}");

    // BM25: nDCG@10 matches the text index's own measurement (0.682).
    let mut ndcg = Vec::new();
    for q in of(Kind::Bm25) {
        let qi = q.query.unwrap();
        let got = ids(&db.query(&q.sql, &Params::new()).unwrap());
        ndcg.push(strata::metrics::ndcg_at_k(&got, coll.qrels.get(&coll.queries[qi].id).unwrap_or(&HashMap::new()), 10));
    }
    let ndcg = strata::metrics::mean(&ndcg);
    assert!((ndcg - 0.682).abs() < 0.01, "BM25 nDCG@10 {ndcg}");

    // Boolean counts and facets agree with the analyzer applied to every document.
    let analyzed: Vec<std::collections::HashSet<String>> =
        coll.docs.iter().map(|d| strata::text::analyze(d[col::BODY].as_str().unwrap()).into_iter().collect()).collect();
    for q in of(Kind::And2).into_iter().take(40).chain(of(Kind::Facet).into_iter().take(40)) {
        let terms: Vec<String> = q.words.iter().flat_map(|x| strata::text::analyze(x)).collect();
        let matching: Vec<usize> = (0..coll.docs.len()).filter(|&i| terms.iter().all(|t| analyzed[i].contains(t))).collect();
        if q.kind == Kind::Facet {
            let rs = db.query(&q.sql, &Params::new()).unwrap();
            let mut counts: HashMap<&str, i64> = HashMap::new();
            for &i in &matching {
                *counts.entry(coll.docs[i][col::CATEGORY].as_str().unwrap()).or_insert(0) += 1;
            }
            let mut want: Vec<(&str, i64)> = counts.into_iter().collect();
            want.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            want.truncate(5);
            let got: Vec<(String, i64)> = rs.rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap())).collect();
            let want: Vec<(String, i64)> = want.into_iter().map(|(a, b)| (a.to_string(), b)).collect();
            assert_eq!(got, want, "{}", q.sql);
        } else {
            let n = db.query(q.count_sql.as_ref().unwrap(), &Params::new()).unwrap();
            assert_eq!(n.rows[0][0], Value::Int(matching.len() as i64), "{}", q.sql);
        }
    }

    // Hybrid: RRF of the two rankings computed by hand from Strata's own lists.
    for q in of(Kind::Hybrid).into_iter().take(20) {
        let qi = q.query.unwrap();
        let got = db.query(&q.sql, &params(Some(qi))).unwrap();
        let text = coll.queries[qi].text.replace('\'', "''");
        let bm = ids(&db.query(&format!("SELECT id FROM docs ORDER BY bm25(body, '{text}') DESC LIMIT 100"), &Params::new()).unwrap());
        let ve = ids(&db.query("SELECT id FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 100", &params(Some(qi))).unwrap());
        let mut fused: HashMap<&str, f64> = HashMap::new();
        for list in [&bm, &ve] {
            for (r, id) in list.iter().enumerate() {
                *fused.entry(id).or_insert(0.0) += 1.0 / (60.0 + r as f64 + 1.0);
            }
        }
        let mut want: Vec<(&str, f64)> = fused.into_iter().collect();
        want.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(b.0)));
        let want: Vec<String> = want.into_iter().take(10).map(|x| x.0.to_string()).collect();
        assert_eq!(ids(&got), want, "{}", q.sql);
    }

    // Deletes are visible to queries; weighted fusion runs.
    let victim = coll.docs[0][col::ID].clone();
    let before = db.query("SELECT COUNT(*) FROM docs", &Params::new()).unwrap().rows[0][0].clone();
    let mut txn = db.storage().begin().unwrap();
    assert!(txn.delete(&victim).unwrap());
    txn.commit().unwrap();
    let after = db.query("SELECT COUNT(*) FROM docs", &Params::new()).unwrap().rows[0][0].clone();
    assert_eq!(before.as_i64().unwrap() - 1, after.as_i64().unwrap());
    let rs = db
        .query(
            "SELECT id, score() FROM docs WHERE year >= 2000 ORDER BY 0.3 * bm25(body, 'cancer treatment') + 0.7 * similarity(emb, :q) DESC LIMIT 5",
            &params(Some(0)),
        )
        .unwrap();
    assert_eq!(rs.rows.len(), 5);
    assert!(rs.rows.iter().all(|r| r[0] != victim));
}
