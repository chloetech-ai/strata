//! SQLite and Tantivy on SciFact (skipped if the collection is not
//! prepared): exact filters, plausible BM25, exact vectors, cold runs.
#![cfg(feature = "baselines")]

use std::collections::HashSet;

use strata::baselines::{Sqlite, Tantivy, TantivyOptions};
use strata::data::{self, col};
use strata::workload::run::{run_kind, QueryOutput, RunOptions, System};
use strata::workload::{filter_matches, generate, Kind, WorkloadConfig, TOP_K};

/// Fraction of queries on which `f` holds for both systems' outputs.
fn agreement(a: &[(usize, QueryOutput)], b: &[(usize, QueryOutput)], f: impl Fn(&QueryOutput, &QueryOutput) -> bool) -> f64 {
    a.iter().zip(b).filter(|((_, x), (_, y))| f(x, y)).count() as f64 / a.len().max(1) as f64
}

#[test]
fn scifact_baselines_agree() {
    if !data::data_dir().join("scifact/meta.json").exists() {
        eprintln!("SciFact not prepared; skipping");
        return;
    }
    let coll = data::load("scifact", None).unwrap();
    let w = generate(&coll, &WorkloadConfig { max_boolean: 100, per_target: 5, ..WorkloadConfig::default() });
    let dir = tempfile::tempdir().unwrap();
    let (mut sqlite, built) = Sqlite::build(&dir.path().join("scifact.sqlite"), &coll).unwrap();
    assert!(built.bytes as usize > coll.docs.len() * coll.dim * 4, "vectors missing from {built:?}");
    let (mut tantivy, built) = Tantivy::build(&dir.path().join("scifact.tantivy"), &coll, &TantivyOptions::default()).unwrap();
    assert!(built.bytes > 0);
    assert_eq!(tantivy.segments(), 1);

    // Filters: both systems return the exact count and the same top-10 by
    // rating, ties by id, as a scan of the documents does.
    for i in w.of_kind(Kind::Filter) {
        let q = &w.queries[i];
        let mut expect: Vec<(f64, &str)> = coll
            .docs
            .iter()
            .filter(|d| filter_matches(&q.filter, d))
            .map(|d| (d[col::RATING].as_f64().unwrap(), d[col::ID].as_str().unwrap()))
            .collect();
        expect.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
        let expect: Vec<String> = expect.iter().take(TOP_K).map(|(_, id)| id.to_string()).collect();
        for sys in [&mut sqlite as &mut dyn System, &mut tantivy] {
            let out = sys.run(q, &coll).unwrap();
            assert_eq!(out.count, q.matches, "{}: {}", sys.name(), q.sql);
            assert_eq!(out.ids, expect, "{}: {}", sys.name(), q.sql);
        }
    }

    // BM25: plausible quality on both; overlap is reported, not asserted.
    let opts = RunOptions { reps: 1, cold_sample: 0, ..RunOptions::default() };
    let s = run_kind(&mut sqlite, &w, &coll, Kind::Bm25, &opts).unwrap();
    let t = run_kind(&mut tantivy, &w, &coll, Kind::Bm25, &opts).unwrap();
    let overlap = agreement(&s.outputs, &t.outputs, |x, y| x.ids.iter().filter(|id| y.ids.contains(id)).count() >= TOP_K / 2);
    let (ns, nt) = (s.report.ndcg10.unwrap(), t.report.ndcg10.unwrap());
    eprintln!("BM25 nDCG@10: sqlite {ns:.4}, tantivy {nt:.4}; queries sharing at least half their top-10: {overlap:.3}");
    assert!(ns > 0.6 && nt > 0.6, "implausible BM25 quality: sqlite {ns}, tantivy {nt}");

    // Booleans and facets: the stemmers differ (Porter vs Snowball), so
    // counts may differ; most agree.
    for kind in [Kind::And2, Kind::And3, Kind::Or3, Kind::Facet] {
        let s = run_kind(&mut sqlite, &w, &coll, kind, &opts).unwrap();
        let t = run_kind(&mut tantivy, &w, &coll, kind, &opts).unwrap();
        let same = agreement(&s.outputs, &t.outputs, |x, y| x.count == y.count && x.facets == y.facets);
        eprintln!("{}: same result on {same:.3} of {} queries", kind.name(), s.outputs.len());
        assert!(same > 0.7, "{}: systems agree on only {same}", kind.name());
    }

    // Vectors: SQLite's brute force is exact.
    let s = run_kind(&mut sqlite, &w, &coll, Kind::Vector, &RunOptions { max_per_kind: Some(10), ..opts.clone() }).unwrap();
    for (i, out) in &s.outputs {
        let qv = &coll.queries[w.queries[*i].query.unwrap()].vector;
        let mut scored: Vec<(f32, &str)> = coll
            .docs
            .iter()
            .map(|d| (d[col::EMB].as_vector().unwrap().iter().zip(qv.iter()).map(|(a, b)| a * b).sum(), d[col::ID].as_str().unwrap()))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        let truth: HashSet<&str> = scored.iter().take(TOP_K).map(|(_, id)| *id).collect();
        let found = out.ids.iter().filter(|id| truth.contains(id.as_str())).count();
        assert!(found >= TOP_K - 1, "exact search found {found} of the top {TOP_K}");
    }
    assert!(s.report.ndcg10.unwrap() > 0.6);
    let vector_query = &w.queries[w.of_kind(Kind::Vector)[0]];
    assert!(!tantivy.supports(Kind::Vector) && tantivy.run(vector_query, &coll).is_err());

    // Cold runs reopen the systems and still answer correctly.
    let cold = RunOptions { reps: 1, cold_sample: 3, max_per_kind: Some(10), ..RunOptions::default() };
    for sys in [&mut sqlite as &mut dyn System, &mut tantivy] {
        let r = run_kind(sys, &w, &coll, Kind::FilteredBm25, &cold).unwrap();
        let c = r.report.cold.unwrap();
        assert_eq!(c.n, 3);
        eprintln!("{} cold filtered_bm25: p50 {:.3} ms, {:?} pages", sys.name(), c.p50_ms, r.report.cold_pages);
    }
}
