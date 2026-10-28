//! A merge's vector graph built whole against started from its largest
//! input's graph: build time and recall@10 against an exact scan, on unit
//! vectors around 100 random centres.
//!
//! ```text
//! graph_start DIM SEGMENTS DOCS_PER_SEGMENT [KEY=VALUE ...]
//! ```
//!
//! `KEY=VALUE` pairs are build options, such as `vector.threads=4`. The
//! inputs are equal in size, as the tiered policy merges them.
use std::sync::Arc;
use std::time::Instant;

use rand::{Rng, SeedableRng};
use strata::schema::DEFAULT_HNSW;
use strata::segment::build::{build_segment, build_segment_from, BuildOptions, GraphStart};
use strata::segment::SegmentFile;
use strata::vector::{SearchParams, Strategy, VectorReader};
use strata::{Field, FieldType, Schema, Value};

fn gaussian(rng: &mut impl Rng) -> f32 {
    let (u, v): (f64, f64) = (rng.gen_range(1e-12..1.0), rng.gen());
    ((-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()) as f32
}

fn clustered(n: usize, rng: &mut impl Rng, centres: &[Vec<f32>]) -> Vec<Vec<f32>> {
    (0..n)
        .map(|_| {
            let c = &centres[rng.gen_range(0..centres.len())];
            let mut v: Vec<f32> = c.iter().map(|x| x + 0.6 * gaussian(rng)).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter_mut().for_each(|x| *x /= norm);
            v
        })
        .collect()
}

fn recall(seg: &SegmentFile, queries: &[Vec<f32>], ef: usize) -> f64 {
    let r = VectorReader::open(seg, "emb").unwrap();
    let params = SearchParams { strategy: Strategy::Hnsw, ef, ..SearchParams::default() };
    let mut found = 0;
    for q in queries {
        let exact: Vec<u32> = r.exact(q, 10, None).iter().map(|h| h.doc).collect();
        found += r.search(q, 10, &params, None).hits.iter().filter(|h| exact.contains(&h.doc)).count();
    }
    found as f64 / (10 * queries.len()) as f64
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (dim, segs, per): (usize, usize, usize) = (a[1].parse().unwrap(), a[2].parse().unwrap(), a[3].parse().unwrap());
    let mut opts = BuildOptions::default();
    for kv in &a[4..] {
        let (k, v) = kv.split_once('=').expect("KEY=VALUE");
        opts = opts.set(k, v);
    }
    let mut rng = rand::rngs::StdRng::seed_from_u64(1);
    let centres: Vec<Vec<f32>> = (0..100).map(|_| (0..dim).map(|_| gaussian(&mut rng)).collect()).collect();
    let vectors = clustered(segs * per, &mut rng, &centres);
    let queries = clustered(200, &mut rng, &centres);
    let schema = Schema::new("t", vec![Field::new("id", FieldType::Int), Field::new("emb", FieldType::Vector { dim, index: DEFAULT_HNSW })], Some("id")).unwrap();
    let docs: Vec<Vec<Value>> = vectors.iter().enumerate().map(|(i, v)| vec![Value::Int(i as i64), Value::Vector(Arc::from(v.as_slice()))]).collect();
    let first = SegmentFile::from_vec(build_segment(&schema, &docs[..per], &opts).unwrap()).unwrap();
    let t = Instant::now();
    let whole = SegmentFile::from_vec(build_segment(&schema, &docs, &opts).unwrap()).unwrap();
    let whole_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let start = [GraphStart { field: 1, segment: first, base: 0 }];
    let started = SegmentFile::from_vec(build_segment_from(&schema, &docs, &opts, &start).unwrap()).unwrap();
    let started_s = t.elapsed().as_secs_f64();
    println!(
        "{dim}-dim, {segs} x {per}: whole {whole_s:.2} s, recall@10 ef 64 {:.3}, ef 100 {:.3}; started {started_s:.2} s, recall@10 ef 64 {:.3}, ef 100 {:.3}",
        recall(&whole, &queries, 64),
        recall(&whole, &queries, 100),
        recall(&started, &queries, 64),
        recall(&started, &queries, 100)
    );
}
