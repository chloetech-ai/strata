//! Binary vector codes (a bit per dimension) and RaBitQ codes (1, 2 or 4
//! bits of the rotated vectors, and a factor a row) against SQ8 and exact
//! search: sizes, how closely the codes order rows, recall of every index
//! and filter strategy after float32 rescoring, exact scores, missing
//! vectors, merges, the schema setting, SQL and the build option.

use std::sync::Arc;

use rand::{Rng, SeedableRng};
use strata::db::Db;
use strata::query::Params;
use strata::segment::build::{build_segment, BuildOptions};
use strata::segment::{DocFilter, DocSet, SegmentFile};
use strata::storage::DbOptions;
use strata::storage::merge::MergePolicy;
use strata::vector::codes::{oversample, BINARY_RERANK};
use strata::vector::rabitq;
use strata::vector::{Hit, SearchParams, Strategy, VectorReader};
use strata::{Field, FieldType, Schema, Value, VectorCodes, VectorIndexKind};

/// The sections of field `emb`, the table's only field (id 0).
const EMB: &str = "f0";

fn gaussian(rng: &mut impl Rng) -> f32 {
    let (u, v): (f64, f64) = (rng.gen_range(1e-12..1.0), rng.gen());
    ((-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()) as f32
}

/// `n` unit vectors around `clusters` centres that share a common
/// direction, as learned embeddings do (so the codes must be centred).
fn embeddings(n: usize, dim: usize, clusters: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let common: Vec<f32> = (0..dim).map(|_| 1.5 * gaussian(&mut rng)).collect();
    let mut setup = rand::rngs::StdRng::seed_from_u64(7);
    let centres: Vec<Vec<f32>> = (0..clusters).map(|_| (0..dim).map(|j| common[j] + gaussian(&mut setup)).collect()).collect();
    (0..n)
        .map(|_| {
            let c = &centres[rng.gen_range(0..clusters)];
            let mut v: Vec<f32> = c.iter().map(|x| x + 1.5 * gaussian(&mut rng)).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter_mut().for_each(|x| *x /= norm);
            v
        })
        .collect()
}

fn segment(vectors: &[Option<Vec<f32>>], dim: usize, codes: VectorCodes, opts: &BuildOptions) -> SegmentFile {
    let field = Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Flat }).not_stored().codes(codes);
    let schema = Schema::new("t", vec![field], None).unwrap();
    let docs: Vec<Vec<Value>> = vectors.iter().map(|v| vec![v.as_ref().map_or(Value::Null, |v| Value::Vector(Arc::from(v.as_slice())))]).collect();
    SegmentFile::from_vec(build_segment(&schema, &docs, opts).unwrap()).unwrap()
}

fn naive(vectors: &[Option<Vec<f32>>], q: &[f32], k: usize, filter: Option<&dyn DocFilter>) -> Vec<u32> {
    let mut all: Vec<(f32, u32)> = vectors
        .iter()
        .enumerate()
        .filter_map(|(d, v)| v.as_ref().map(|v| (d as u32, v)))
        .filter(|(d, _)| filter.is_none_or(|f| f.contains(*d)))
        .map(|(d, v)| (v.iter().zip(q).map(|(a, b)| a * b).sum::<f32>(), d))
        .collect();
    all.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    all.into_iter().take(k).map(|x| x.1).collect()
}

fn ids(h: &[Hit]) -> Vec<u32> {
    h.iter().map(|h| h.doc).collect()
}

fn opts(kind: &str) -> BuildOptions {
    BuildOptions::default().set("vector.index", kind).set("vector.ivf.nlist", 32).set("vector.hnsw.ef_construction", 100)
}

/// Mean recall@10 of `p` over `queries`.
fn recall(r: &VectorReader, vecs: &[Option<Vec<f32>>], queries: &[Vec<f32>], p: &SearchParams, filter: Option<&DocSet>) -> f64 {
    let mut found = 0;
    let mut total = 0;
    for q in queries {
        let truth = naive(vecs, q, 10, filter.map(|f| f as &dyn DocFilter));
        let out = r.search(q, 10, p, filter.map(|f| f as &dyn DocFilter));
        if let Some(f) = filter {
            assert!(out.hits.iter().all(|h| f.contains(h.doc)), "a non-member came back");
        }
        found += out.hits.iter().filter(|h| truth.contains(&h.doc)).count();
        total += truth.len();
    }
    found as f64 / total as f64
}

#[test]
fn binary_codes_are_a_32nd_of_the_vectors_and_rescore_exactly() {
    let (dim, n) = (384, 3000);
    let vecs: Vec<Option<Vec<f32>>> = embeddings(n, dim, 40, 1).into_iter().map(Some).collect();
    let queries = embeddings(40, dim, 40, 2);
    let bin = VectorReader::open(&segment(&vecs, dim, VectorCodes::Binary, &opts("flat")), EMB).unwrap();
    let sq8 = VectorReader::open(&segment(&vecs, dim, VectorCodes::Sq8, &opts("flat")), EMB).unwrap();
    assert_eq!(bin.info().codes, VectorCodes::Binary);
    assert_eq!(bin.code_bytes(), n * dim / 8);
    assert_eq!(sq8.code_bytes(), n * dim);
    assert!(bin.is_zero_copy() && bin.sq8().is_none() && sq8.sq8().is_some());

    let flat = SearchParams { strategy: Strategy::Flat, ..Default::default() };
    let b = recall(&bin, &vecs, &queries, &flat, None);
    let s = recall(&sq8, &vecs, &queries, &flat, None);
    assert!(b >= 0.97, "binary recall@10 {b} (sq8 {s})");
    for q in &queries[..5] {
        let out = bin.search(q, 10, &flat, None);
        // Binary codes rescore BINARY_RERANK × rerank candidates.
        assert_eq!(out.stats.candidates, BINARY_RERANK * flat.rerank);
        assert_eq!(out.stats.f32_distances, BINARY_RERANK * flat.rerank);
        for h in &out.hits {
            assert_eq!(h.score, bin.similarity(h.doc, q), "scores are exact float32");
        }
        // Rescoring every document is exact search.
        let all = bin.search(q, 10, &SearchParams { strategy: Strategy::Flat, rerank: n, ..Default::default() }, None);
        assert_eq!(ids(&all.hits), naive(&vecs, q, 10, None));
    }
}

#[test]
fn every_index_and_filter_strategy_searches_binary_codes() {
    let (dim, n) = (256, 6000);
    let mut vecs: Vec<Option<Vec<f32>>> = embeddings(n, dim, 48, 3).into_iter().map(Some).collect();
    // Some documents have no vector: never returned.
    for d in (0..n).step_by(97) {
        vecs[d] = None;
    }
    let queries = embeddings(30, dim, 48, 4);
    let mut rng = rand::rngs::StdRng::seed_from_u64(5);
    let filters: Vec<DocSet> = [0.01, 0.2].iter().map(|&p| DocSet::from_sorted((0..n as u32).filter(|_| rng.gen_bool(p)).collect(), n as u32)).collect();
    for kind in ["flat", "ivf", "hnsw"] {
        let r = VectorReader::open(&segment(&vecs, dim, VectorCodes::Binary, &opts(kind)), EMB).unwrap();
        assert_eq!(r.index_kind(), kind);
        let p = SearchParams { nprobe: 8, ef: 64, ..Default::default() };
        let got = recall(&r, &vecs, &queries, &p, None);
        assert!(got >= 0.95, "{kind}: recall@10 {got}");
        for q in &queries {
            assert!(r.search(q, 10, &p, None).hits.iter().all(|h| vecs[h.doc as usize].is_some()), "{kind}: a document without a vector");
        }
        for f in &filters {
            for strategy in [Strategy::Auto, Strategy::PreFilter, Strategy::PostFilter] {
                let got = recall(&r, &vecs, &queries, &SearchParams { strategy, ..p }, Some(f));
                assert!(got >= 0.9, "{kind} {strategy:?} with {} members: recall@10 {got}", f.len());
            }
        }
    }
}

#[test]
fn build_option_overrides_the_schema() {
    let dim = 64;
    let vecs: Vec<Option<Vec<f32>>> = embeddings(500, dim, 8, 6).into_iter().map(Some).collect();
    let r = VectorReader::open(&segment(&vecs, dim, VectorCodes::Sq8, &opts("flat").set("vector.codes", "binary")), EMB).unwrap();
    assert_eq!(r.info().codes, VectorCodes::Binary);
    let r = VectorReader::open(&segment(&vecs, dim, VectorCodes::Binary, &opts("hnsw").set("vector.codes", "sq8")), EMB).unwrap();
    assert_eq!(r.info().codes, VectorCodes::Sq8);
    let schema = Schema::new("t", vec![Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Flat })], None).unwrap();
    let docs = vec![vec![Value::Vector(Arc::from(vecs[0].as_deref().unwrap()))]];
    let e = build_segment(&schema, &docs, &opts("flat").set("vector.codes", "int3")).err().unwrap();
    assert!(e.to_string().contains("unknown vector codes"), "{e}");
}

#[test]
fn a_table_with_binary_codes_answers_sql_like_brute_force() {
    let dim = 128;
    let n = 4000;
    let vecs = embeddings(n, dim, 30, 8);
    let schema = Schema::new(
        "docs",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("year", FieldType::Int),
            Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Hnsw { m: 16, ef_construction: 100 } }).codes(VectorCodes::Binary),
        ],
        Some("id"),
    )
    .unwrap();
    let json = serde_json::to_string(&schema).unwrap();
    assert!(json.contains(r#""codes":"binary""#), "{json}");
    let back: Schema = serde_json::from_str(&json).unwrap();
    assert_eq!(back, schema);
    let plain = Schema::new("t", vec![Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Flat })], None).unwrap();
    assert!(!serde_json::to_string(&plain).unwrap().contains("codes"), "SQ8 is the default and is not written");

    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("b.strata"), schema, DbOptions { segment_max_docs: 1500, ..DbOptions::default() }).unwrap();
    let docs: Vec<Vec<Value>> = vecs.iter().enumerate().map(|(i, v)| vec![Value::Str(format!("d{i:05}")), Value::Int(2000 + (i % 25) as i64), Value::Vector(Arc::from(v.as_slice()))]).collect();
    db.load(docs).unwrap();
    let queries = embeddings(20, dim, 30, 9);
    let (mut found, mut total) = (0, 0);
    for q in &queries {
        let p = Params::new().set("q", Value::Vector(Arc::from(q.as_slice())));
        let r = db.query("SELECT id, score() FROM docs WHERE year >= 2010 ORDER BY similarity(emb, :q) DESC LIMIT 10", &p).unwrap();
        let vs: Vec<Option<Vec<f32>>> = vecs.iter().enumerate().map(|(i, v)| (2000 + i % 25 >= 2010).then(|| v.clone())).collect();
        let truth: Vec<String> = naive(&vs, q, 10, None).into_iter().map(|d| format!("d{d:05}")).collect();
        let got: Vec<String> = r.keys.iter().map(|k| k.as_str().unwrap().to_string()).collect();
        found += got.iter().filter(|g| truth.contains(g)).count();
        total += truth.len();
        // Scores are the exact dot products.
        for (k, s) in got.iter().zip(&r.scores) {
            let i: usize = k[1..].parse().unwrap();
            let exact: f32 = vecs[i].iter().zip(q).map(|(a, b)| a * b).sum();
            assert!((*s - exact as f64).abs() < 1e-5, "{k}: {s} vs {exact}");
        }
    }
    assert!(found as f64 >= 0.95 * total as f64, "SQL recall@10 {found}/{total}");

    let bad = Schema::new("t", vec![Field::new("year", FieldType::Int).codes(VectorCodes::Binary)], None);
    assert!(bad.err().unwrap().to_string().contains("not a VECTOR"));
}

const RABITQ: [VectorCodes; 3] = [VectorCodes::Rabitq1, VectorCodes::Rabitq2, VectorCodes::Rabitq4];

/// The share of the exact top 10 among the `depth` rows the codes score
/// highest, over `queries`: how well the codes alone order rows.
fn code_recall(r: &VectorReader, vecs: &[Option<Vec<f32>>], queries: &[Vec<f32>], depth: usize) -> f64 {
    let stride = r.info().stride_u8;
    let mut found = 0;
    for q in queries {
        let qc = r.quantiser().prepare(q, stride);
        let mut by: Vec<(i32, u32)> = (0..r.n_docs()).map(|d| (qc.dot(r.codes(d)), d)).collect();
        by.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let top: Vec<u32> = by[..depth].iter().map(|x| x.1).collect();
        found += naive(vecs, q, 10, None).iter().filter(|d| top.contains(d)).count();
    }
    found as f64 / (10 * queries.len()) as f64
}

#[test]
fn rabitq_codes_order_rows_closer_than_binary_codes() {
    let (dim, n) = (384, 3000);
    let vecs: Vec<Option<Vec<f32>>> = embeddings(n, dim, 40, 1).into_iter().map(Some).collect();
    let queries = embeddings(40, dim, 40, 2);
    let open = |codes| VectorReader::open(&segment(&vecs, dim, codes, &opts("flat")), EMB).unwrap();
    let bin = open(VectorCodes::Binary);
    let mut got = vec![code_recall(&bin, &vecs, &queries, 20)];
    for (codes, bits) in RABITQ.into_iter().zip([1, 2, 4]) {
        let r = open(codes);
        assert_eq!(r.info().codes, codes);
        assert_eq!(r.info().stride_u8, bits * dim / 8 + 8, "bit planes and two floats a row");
        assert_eq!(r.code_bytes(), n * rabitq::stride(dim, bits));
        assert!(r.is_zero_copy() && r.sq8().is_none());
        // An estimate is a dot product, close to the exact one.
        let q = &queries[0];
        let qc = r.quantiser().prepare(q, r.info().stride_u8);
        let err: f32 = (0..100).map(|d| (qc.score(qc.dot(r.codes(d))) - r.similarity(d, q)).abs()).sum::<f32>() / 100.0;
        assert!(err < [0.0, 0.03, 0.015, 0.0, 0.005][bits], "{codes:?}: mean error {err}");
        got.push(code_recall(&r, &vecs, &queries, 20));
    }
    // Of the exact top 10, the top 20 by code hold more with RaBitQ's one
    // bit than with the sign bits around the mean, and more with each step
    // of bits.
    assert!(got.windows(2).all(|w| w[0] < w[1]), "binary, rabitq1, rabitq2, rabitq4: {got:?}");
    assert!(got[3] >= 0.97, "{got:?}");
}

#[test]
fn rabitq_codes_rescore_exactly() {
    let (dim, n) = (200, 2000);
    let vecs: Vec<Option<Vec<f32>>> = embeddings(n, dim, 30, 11).into_iter().map(Some).collect();
    let queries = embeddings(30, dim, 30, 12);
    let flat = SearchParams { strategy: Strategy::Flat, ..Default::default() };
    for codes in RABITQ {
        let mut r = VectorReader::open(&segment(&vecs, dim, codes, &opts("flat")), EMB).unwrap();
        let got = recall(&r, &vecs, &queries, &flat, None);
        assert!(got >= 0.97, "{codes:?}: recall@10 {got}");
        // Reading ahead only the rows that can still pass changes nothing.
        for q in &queries[..5] {
            let plain = r.search(q, 10, &flat, None);
            r.set_io_hints(true);
            let hinted = r.search(q, 10, &flat, None);
            r.set_io_hints(false);
            assert_eq!(plain.hits, hinted.hits, "{codes:?}");
            assert_eq!(plain.stats, hinted.stats, "{codes:?}");
        }
        // The bounds pass over most of the pool unread.
        let (mut pool, mut read) = (0, 0);
        for q in &queries {
            let s = r.search(q, 10, &flat, None).stats;
            assert_eq!(s.candidates, s.f32_distances + s.pruned);
            (pool, read) = (pool + s.candidates, read + s.f32_distances);
        }
        assert!(read * 2 < pool, "{codes:?}: {read} of {pool} candidates rescored");
        for q in &queries[..5] {
            let out = r.search(q, 10, &flat, None);
            assert_eq!(out.stats.candidates, oversample(codes) * flat.rerank);
            for h in &out.hits {
                assert_eq!(h.score, r.similarity(h.doc, q), "scores are exact float32");
            }
            let all = r.search(q, 10, &SearchParams { strategy: Strategy::Flat, rerank: n, ..Default::default() }, None);
            assert_eq!(ids(&all.hits), naive(&vecs, q, 10, None));
        }
    }
}

#[test]
fn every_index_and_filter_strategy_searches_rabitq_codes() {
    let (dim, n) = (256, 6000);
    let mut vecs: Vec<Option<Vec<f32>>> = embeddings(n, dim, 48, 3).into_iter().map(Some).collect();
    for d in (0..n).step_by(97) {
        vecs[d] = None;
    }
    let queries = embeddings(30, dim, 48, 4);
    let mut rng = rand::rngs::StdRng::seed_from_u64(5);
    let filters: Vec<DocSet> = [0.01, 0.2].iter().map(|&p| DocSet::from_sorted((0..n as u32).filter(|_| rng.gen_bool(p)).collect(), n as u32)).collect();
    for codes in [VectorCodes::Rabitq1, VectorCodes::Rabitq4] {
        for kind in ["flat", "ivf", "hnsw"] {
            let r = VectorReader::open(&segment(&vecs, dim, codes, &opts(kind)), EMB).unwrap();
            assert_eq!(r.index_kind(), kind);
            let p = SearchParams { nprobe: 8, ef: 64, ..Default::default() };
            let got = recall(&r, &vecs, &queries, &p, None);
            assert!(got >= 0.95, "{codes:?} {kind}: recall@10 {got}");
            for q in &queries {
                assert!(r.search(q, 10, &p, None).hits.iter().all(|h| vecs[h.doc as usize].is_some()), "{kind}: a document without a vector");
            }
            for f in &filters {
                for strategy in [Strategy::Auto, Strategy::PreFilter, Strategy::PostFilter] {
                    let got = recall(&r, &vecs, &queries, &SearchParams { strategy, ..p }, Some(f));
                    assert!(got >= 0.9, "{codes:?} {kind} {strategy:?} with {} members: recall@10 {got}", f.len());
                }
            }
        }
    }
}

#[test]
fn a_table_with_rabitq_codes_answers_sql_through_merges() {
    let dim = 96;
    let n = 3000;
    let vecs = embeddings(n, dim, 30, 13);
    let schema = Schema::new(
        "docs",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("year", FieldType::Int),
            Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Hnsw { m: 16, ef_construction: 100 } }).codes(VectorCodes::Rabitq2),
        ],
        Some("id"),
    )
    .unwrap();
    let json = serde_json::to_string(&schema).unwrap();
    assert!(json.contains(r#""codes":"rabitq2""#), "{json}");
    assert_eq!(serde_json::from_str::<Schema>(&json).unwrap(), schema);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rq.strata");
    let opts = || DbOptions { layout: strata::storage::Layout::Directory, segment_max_docs: 800, sync: false, ..DbOptions::default() };
    let db = Db::create(&path, schema, opts()).unwrap();
    let docs: Vec<Vec<Value>> = vecs.iter().enumerate().map(|(i, v)| vec![Value::Str(format!("d{i:05}")), Value::Int(2000 + (i % 25) as i64), Value::Vector(Arc::from(v.as_slice()))]).collect();
    db.load(docs).unwrap();
    let queries = embeddings(20, dim, 30, 14);
    let check = |db: &Db| {
        let (mut found, mut total) = (0, 0);
        for q in &queries {
            let p = Params::new().set("q", Value::Vector(Arc::from(q.as_slice())));
            let r = db.query("SELECT id, score() FROM docs WHERE year >= 2010 ORDER BY similarity(emb, :q) DESC LIMIT 10", &p).unwrap();
            let vs: Vec<Option<Vec<f32>>> = vecs.iter().enumerate().map(|(i, v)| (2000 + i % 25 >= 2010).then(|| v.clone())).collect();
            let truth: Vec<String> = naive(&vs, q, 10, None).into_iter().map(|d| format!("d{d:05}")).collect();
            let got: Vec<String> = r.keys.iter().map(|k| k.as_str().unwrap().to_string()).collect();
            found += got.iter().filter(|g| truth.contains(g)).count();
            total += truth.len();
            for (k, s) in got.iter().zip(&r.scores) {
                let i: usize = k[1..].parse().unwrap();
                let exact: f32 = vecs[i].iter().zip(q).map(|(a, b)| a * b).sum();
                assert!((*s - exact as f64).abs() < 1e-5, "{k}: {s} vs {exact}");
            }
        }
        assert!(found as f64 >= 0.95 * total as f64, "SQL recall@10 {found}/{total}");
    };
    check(&db);
    let segs = db.snapshot().segments().len();
    assert!(segs > 1, "several segments");
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
    assert!(db.snapshot().segments().len() < segs, "merged");
    check(&db);
    // The manifest names the feature, so a Strata without RaBitQ codes
    // refuses the database by name; it opens again here.
    let cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join(cur["manifest"].as_str().unwrap())).unwrap()).unwrap();
    assert!(manifest["features"]["required"].as_array().unwrap().iter().any(|f| f == "rabitq"), "{}", manifest["features"]);
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    check(&db);
}

#[test]
fn a_multi_vector_field_searches_rabitq_codes() {
    // Rows of a few vectors; the codes find candidates, SQ8 codes score
    // them (the float32 vectors are not kept), as with binary codes.
    let dim = 32;
    let mut rng = rand::rngs::StdRng::seed_from_u64(15);
    let rows: Vec<Vec<Vec<f32>>> = (0..600).map(|_| embeddings(rng.gen_range(1..6), dim, 20, rng.gen())).collect();
    let field = Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Flat }).multi().codes(VectorCodes::Rabitq1);
    let schema = Schema::new("t", vec![field], None).unwrap();
    let docs: Vec<Vec<Value>> = rows.iter().map(|vs| vec![Value::List(vs.iter().map(|v| Value::Vector(Arc::from(v.as_slice()))).collect())]).collect();
    let seg = SegmentFile::from_vec(build_segment(&schema, &docs, &opts("hnsw")).unwrap()).unwrap();
    let r = VectorReader::open(&seg, EMB).unwrap();
    assert_eq!(r.info().codes, VectorCodes::Rabitq1);
    assert!(!r.has_f32() && r.rows().is_some());
    let q = &rows[7][0];
    let out = r.search(q, 5, &SearchParams::default(), None);
    let best = out.hits[0];
    assert_eq!(r.rows().unwrap().owner(best.doc), Some(7), "the query's own vector is found");
    assert!((best.score - 1.0).abs() < 0.02, "an SQ8 score {}", best.score);
}
