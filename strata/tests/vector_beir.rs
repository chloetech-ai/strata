//! ANN recall on real embeddings (SciFact, 5k documents, 300 queries):
//! IVF and HNSW at moderate parameters against exact search, and filtered
//! strategies against the exact filtered top-10.

use std::sync::Arc;

use rand::{Rng, SeedableRng};
use strata::metrics::{mean, recall};
use strata::segment::build::{build_segment, BuildOptions};
use strata::segment::{DocSet, SegmentFile};
use strata::vector::{SearchParams, Strategy, VectorReader};
use strata::{data, Field, FieldType, Schema, Value, VectorIndexKind};

struct Fixture {
    docs: Vec<Arc<[f32]>>,
    queries: Vec<Arc<[f32]>>,
}

fn fixture() -> Option<Fixture> {
    if !data::data_dir().join("scifact/meta.json").exists() {
        eprintln!("SciFact not prepared; skipping");
        return None;
    }
    let c = data::load("scifact", None).unwrap();
    let docs = c.docs.iter().map(|d| match &d[data::col::EMB] {
        Value::Vector(v) => v.clone(),
        _ => unreachable!(),
    });
    Some(Fixture { docs: docs.collect(), queries: c.queries.iter().map(|q| q.vector.clone()).collect() })
}

fn build(f: &Fixture, opts: BuildOptions) -> VectorReader {
    let schema = Schema::new("t", vec![Field::new("emb", FieldType::Vector { dim: 1024, index: VectorIndexKind::Flat }).not_stored()], None).unwrap();
    let docs: Vec<Vec<Value>> = f.docs.iter().map(|v| vec![Value::Vector(v.clone())]).collect();
    let seg = SegmentFile::from_vec(build_segment(&schema, &docs, &opts).unwrap()).unwrap();
    VectorReader::open(&seg, "emb").unwrap()
}

/// Mean recall@10 of `strategy` against exact search (optionally filtered).
fn recall_at_10(r: &VectorReader, f: &Fixture, p: &SearchParams, filter: Option<&DocSet>) -> f64 {
    let rs: Vec<f64> = f
        .queries
        .iter()
        .map(|q| {
            let truth: Vec<u32> = r.exact(q, 10, filter.map(|x| x as _)).iter().map(|h| h.doc).collect();
            let got: Vec<u32> = r.search(q, 10, p, filter.map(|x| x as _)).hits.iter().map(|h| h.doc).collect();
            if let Some(fl) = filter {
                assert!(got.iter().all(|&d| fl.contains(d)));
            }
            recall(&got, &truth)
        })
        .collect();
    mean(&rs)
}

#[test]
fn ann_recall_on_scifact() {
    let Some(f) = fixture() else { return };
    let flat = build(&f, BuildOptions::default().set("vector.index", "flat"));
    let r_flat = recall_at_10(&flat, &f, &SearchParams { strategy: Strategy::Flat, ..Default::default() }, None);
    eprintln!("flat (SQ8 + rerank 40) recall@10 {r_flat:.4}");
    assert!(r_flat >= 0.99);

    let ivf = build(&f, BuildOptions::default().set("vector.index", "ivf").set("vector.threads", 2));
    let r_ivf = recall_at_10(&ivf, &f, &SearchParams { strategy: Strategy::Ivf, nprobe: 32, ..Default::default() }, None);
    eprintln!("ivf nlist {:?} nprobe 32 recall@10 {r_ivf:.4}", ivf.info().nlist);
    assert!(r_ivf >= 0.9);

    let hnsw = build(&f, BuildOptions::default().set("vector.index", "hnsw").set("vector.threads", 2));
    let r_hnsw = recall_at_10(&hnsw, &f, &SearchParams { strategy: Strategy::Hnsw, ef: 64, ..Default::default() }, None);
    eprintln!("hnsw ef 64 recall@10 {r_hnsw:.4}");
    assert!(r_hnsw >= 0.9);

    // Filtered: pre-filter is exact up to SQ8 ties; the in-index strategies
    // keep high recall at a moderate selectivity.
    let mut rng = rand::rngs::StdRng::seed_from_u64(1);
    let n = f.docs.len() as u32;
    let filter = DocSet::from_sorted((0..n).filter(|_| rng.gen_bool(0.1)).collect(), n);
    for (r, s, min) in [(&flat, Strategy::PreFilter, 0.99), (&ivf, Strategy::Ivf, 0.9), (&hnsw, Strategy::Hnsw, 0.9), (&hnsw, Strategy::PostFilter, 0.85), (&ivf, Strategy::Auto, 0.9)] {
        let rec = recall_at_10(r, &f, &SearchParams { strategy: s, nprobe: 32, ..Default::default() }, Some(&filter));
        eprintln!("{} {s:?} 10% filter recall@10 {rec:.4}", r.index_kind());
        assert!(rec >= min, "{} {s:?}: {rec}", r.index_kind());
    }
}
