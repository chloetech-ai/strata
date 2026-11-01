//! Every query's profile: the timed steps of its execution
//! (`ExecStats::steps`), a tree of operators and the segments each
//! searched, on which thread, with CPU time under `ExecOptions::profile`.

use strata::data;
use strata::db::Db;
use strata::query::exec::ExecOptions;
use strata::query::steps::Step;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

/// The steps directly inside the step at `at`.
fn children(steps: &[Step], at: usize) -> Vec<usize> {
    let d = steps[at].depth;
    steps[at + 1..].iter().enumerate().take_while(|(_, s)| s.depth > d).filter(|(_, s)| s.depth == d + 1).map(|(i, _)| at + 1 + i).collect()
}

/// Every step lies within its parent's time, and depths step down one at
/// a time.
fn well_formed(steps: &[Step]) {
    assert!(!steps.is_empty());
    assert_eq!(steps[0].depth, 0);
    for w in steps.windows(2) {
        assert!(w[1].depth <= w[0].depth + 1, "{steps:#?}");
    }
    for i in 0..steps.len() {
        for c in children(steps, i) {
            let (p, s) = (&steps[i], &steps[c]);
            assert!(s.start_us + 0.3 >= p.start_us && s.start_us + s.us <= p.start_us + p.us + 0.3, "{s:?} is not inside {p:?}");
        }
    }
}

#[test]
fn a_query_s_steps_are_its_operators_and_segments() {
    let docs = data::synthetic(4000, 16, 5);
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { segment_max_docs: 250, ..DbOptions::default() };
    let db = Db::create(&dir.path().join("t.strata"), data::beir_schema(16, VectorIndexKind::Hnsw { m: 12, ef_construction: 60 }), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    let segments = db.snapshot().segments().iter().filter(|s| s.n_live > 0).count();
    assert!(segments >= 16);
    let title = docs[3][data::col::TITLE].as_str().unwrap().to_string();
    let params = Params::new().set("t", Value::Str(title)).set("q", docs[3][data::col::EMB].clone());

    let sql = "SELECT id, score() FROM docs WHERE year >= 1990 ORDER BY bm25(body, :t) DESC LIMIT 10";
    let profiled = ExecOptions { threads: 4, profile: true, ..ExecOptions::default() };
    let rs = db.query_with(sql, &params, &profiled).unwrap();
    let steps = &rs.stats.steps;
    well_formed(steps);
    let rank = steps.iter().position(|s| s.op == "rank").expect("a rank step");
    assert_eq!(steps[rank].depth, 0);
    assert!(steps[rank].detail.contains("bm25") && steps[rank].detail.contains("top 10"), "{:?}", steps[rank]);
    assert_eq!(steps[rank].rows, Some(10));
    let bm25 = children(steps, rank).into_iter().find(|&i| steps[i].op == "bm25").expect("a bm25 step under rank");
    assert!(steps[bm25].detail.starts_with("body: "), "{:?}", steps[bm25]);
    let segs: Vec<&Step> = children(steps, bm25).into_iter().map(|i| &steps[i]).filter(|s| s.op == "segment").collect();
    assert_eq!(segs.len(), segments, "a step per segment searched");
    let mut ords: Vec<u32> = segs.iter().map(|s| s.seg.unwrap()).collect();
    ords.dedup();
    assert_eq!(ords.len(), segments);
    assert!(segs.iter().all(|s| s.thread < 4 && s.cpu_us.is_some()));
    // A filter materialised in a segment is a step inside the segment's.
    let materialised = rs.stats.plan.iter().filter(|l| l.contains("filter materialised")).count();
    assert_eq!(steps.iter().filter(|s| s.op == "filter" && s.seg.is_some() && s.rows.is_some()).count(), materialised);
    assert!(steps.iter().any(|s| s.op == "project" && s.depth == 0 && s.rows == Some(10)));
    // CPU time counts every thread's: the rank step's covers its segments'.
    let cpu = |s: &Step| s.cpu_us.unwrap();
    let in_segments: f64 = segs.iter().map(|s| cpu(s)).sum();
    assert!(cpu(&steps[rank]) + 50.0 >= in_segments * 0.9, "rank {} µs of CPU, its segments {in_segments}", cpu(&steps[rank]));

    // Without `profile`, the same steps without CPU time.
    let plain = db.query_with(sql, &params, &ExecOptions::default()).unwrap();
    well_formed(&plain.stats.steps);
    assert!(plain.stats.steps.iter().all(|s| s.cpu_us.is_none() && s.thread == 0));
    let ops = |steps: &[Step]| steps.iter().map(|s| (s.op.clone(), s.depth)).collect::<Vec<_>>();
    assert_eq!(ops(&plain.stats.steps), ops(&db.query_with(sql, &params, &ExecOptions { profile: true, ..ExecOptions::default() }).unwrap().stats.steps));

    // A fused score: each input's top rows, the candidates, their exact
    // scores.
    let hybrid = "SELECT id FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 5";
    let steps = db.query_with(hybrid, &params, &ExecOptions::default()).unwrap().stats.steps;
    well_formed(&steps);
    for op in ["rank", "bm25", "vector", "candidates"] {
        assert!(steps.iter().any(|s| s.op == op), "no {op} step in {steps:#?}");
    }

    // A count and a facet.
    let steps = db.query_with("SELECT COUNT(*) FROM docs WHERE year >= 2000", &params, &ExecOptions::default()).unwrap().stats.steps;
    assert!(steps.iter().any(|s| s.op == "count" && s.rows.is_some()), "{steps:#?}");
    let steps = db.query_with("SELECT year, COUNT(*) FROM docs GROUP BY year", &params, &ExecOptions::default()).unwrap().stats.steps;
    assert!(steps.iter().any(|s| (s.op == "facet" || s.op == "aggregate") && s.rows.is_some_and(|n| n > 1)), "{steps:#?}");

    // A compound query: its operators, each leaf's executor steps inside
    // the leaf's. Strata's operators are steps around the leaves;
    // DataFusion's (STRATA_ENGINE=datafusion) are inside a `datafusion`
    // step after them. With the feature `datafusion`, DataFusion runs the
    // leaves first, and leaves so few rows to the operators, which reuse
    // them.
    let compound = "SELECT DISTINCT year FROM (SELECT year FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50) AS t";
    let rs = db.query_with(compound, &params, &ExecOptions::default()).unwrap();
    let steps = &rs.stats.steps;
    well_formed(steps);
    let on_datafusion = steps.iter().any(|s| s.op == "datafusion");
    let leaf = steps.iter().position(|s| s.op == "leaf").unwrap_or_else(|| panic!("no leaf step in {steps:#?}"));
    assert!(steps[leaf].detail.ends_with("docs"), "{:?}", steps[leaf]);
    assert!(children(steps, leaf).iter().any(|&i| steps[i].op == "rank"), "{steps:#?}");
    // DISTINCT runs as GROUP BY its columns.
    let distinct = |s: &Step| s.op == "aggregate" && if on_datafusion { s.detail.contains("gby=") && s.depth > 0 } else { s.detail == "1 key, 1 aggregate" };
    assert!(steps.iter().any(distinct), "{steps:#?}");
}
