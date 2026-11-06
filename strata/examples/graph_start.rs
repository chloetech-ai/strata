//! A merge's vector graph built whole, started from its largest input's
//! graph (Lucene 9.9), and started from it with the other inputs' graphs
//! joined (Lucene 10.2): build time and recall@10 against an exact scan.
//!
//! ```text
//! graph_start DIM SEGMENTS DOCS_PER_SEGMENT [--partition] [KEY=VALUE ...]
//! graph_start beir:NAME SEGMENTS DOCS_PER_SEGMENT [--partition] [KEY=VALUE ...]
//! ```
//!
//! `--partition` gives each row a random rating from 0 to 5 and partitions
//! the field by it at 1, 2, 3 and 4, as the engine comparison builds
//! Strata's vectors: a merge then also builds a graph per partition, and
//! recall is that of the five partition graphs searched together.
//!
//! With a number, the vectors are unit vectors around 100 random centres;
//! with `beir:NAME`, the first documents of a collection prepared under
//! `STRATA_DATA` (`doc_vecs.f32`, `query_vecs.f32`, `meta.json`), and as
//! queries its queries and the 500 documents after those indexed (a BEIR
//! collection can have as few as 50 queries). `KEY=VALUE` pairs are build options, such as
//! `vector.threads=4` or `vector.hnsw.build=f32`. The inputs are equal in
//! size, as the tiered policy merges them.
use std::io::Read;
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

/// The first `n` rows of a file of float32 rows of `dim` values.
fn rows(path: &std::path::Path, dim: usize, n: usize) -> Vec<Vec<f32>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).unwrap().take((n * dim * 4) as u64).read_to_end(&mut bytes).unwrap();
    bytes.chunks_exact(dim * 4).map(|r| r.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()).collect()
}

fn recall(seg: &SegmentFile, field: &str, queries: &[Vec<f32>], ef: usize) -> f64 {
    let r = VectorReader::open(seg, field).unwrap();
    let parts: Vec<usize> = (0..r.partitions().map_or(0, |p| p.parts.len())).collect();
    let params = SearchParams { strategy: Strategy::Hnsw, ef, ..SearchParams::default() };
    let mut found = 0;
    for q in queries {
        let exact: Vec<u32> = r.exact(q, 10, None).iter().map(|h| h.doc).collect();
        let got = if parts.is_empty() { r.search(q, 10, &params, None) } else { r.search_parts(q, 10, &params, &parts, None) };
        found += got.hits.iter().filter(|h| exact.contains(&h.doc)).count();
    }
    found as f64 / (10 * queries.len()) as f64
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (segs, per): (usize, usize) = (a[2].parse().unwrap(), a[3].parse().unwrap());
    let partition = a[4..].iter().any(|x| x == "--partition");
    let mut opts = BuildOptions::default();
    for kv in a[4..].iter().filter(|x| *x != "--partition") {
        let (k, v) = kv.split_once('=').expect("KEY=VALUE");
        opts = opts.set(k, v);
    }
    let (dim, vectors, queries) = match a[1].strip_prefix("beir:") {
        Some(name) => {
            let dir = strata::data::data_dir().join(name);
            let meta: serde_json::Value = serde_json::from_reader(std::fs::File::open(dir.join("meta.json")).unwrap()).unwrap();
            let dim = meta["dim"].as_u64().unwrap() as usize;
            let mut vectors = rows(&dir.join("doc_vecs.f32"), dim, segs * per + 500);
            assert_eq!(vectors.len(), segs * per + 500, "the collection has fewer documents");
            let mut queries = rows(&dir.join("query_vecs.f32"), dim, 500);
            queries.extend(vectors.split_off(segs * per));
            (dim, vectors, queries)
        }
        None => {
            let dim: usize = a[1].parse().unwrap();
            let mut rng = rand::rngs::StdRng::seed_from_u64(1);
            let centres: Vec<Vec<f32>> = (0..100).map(|_| (0..dim).map(|_| gaussian(&mut rng)).collect()).collect();
            let vectors = clustered(segs * per, &mut rng, &centres);
            (dim, vectors, clustered(200, &mut rng, &centres))
        }
    };
    let mut emb = Field::new("emb", FieldType::Vector { dim, index: DEFAULT_HNSW });
    let mut rng = rand::rngs::StdRng::seed_from_u64(2);
    let (schema, docs): (Schema, Vec<Vec<Value>>) = if partition {
        emb.partition = Some(strata::schema::VectorPartition { by: "rating".into(), bounds: vec![1.0, 2.0, 3.0, 4.0] });
        let fields = vec![Field::new("id", FieldType::Int), Field::new("rating", FieldType::Float), emb];
        let docs = vectors.iter().enumerate().map(|(i, v)| vec![Value::Int(i as i64), Value::Float(rng.gen_range(0.0..5.0)), Value::Vector(Arc::from(v.as_slice()))]).collect();
        (Schema::new("t", fields, Some("id")).unwrap(), docs)
    } else {
        let docs = vectors.iter().enumerate().map(|(i, v)| vec![Value::Int(i as i64), Value::Vector(Arc::from(v.as_slice()))]).collect();
        (Schema::new("t", vec![Field::new("id", FieldType::Int), emb], Some("id")).unwrap(), docs)
    };
    let (emb_id, key) = (schema.field_index("emb").unwrap(), schema.key_of("emb").unwrap());
    let t = Instant::now();
    let inputs: Vec<SegmentFile> = (0..segs).map(|s| SegmentFile::from_vec(build_segment(&schema, &docs[s * per..(s + 1) * per], &opts).unwrap()).unwrap()).collect();
    let inputs_s = t.elapsed().as_secs_f64() / segs as f64;
    let starts: Vec<GraphStart> = inputs.iter().enumerate().map(|(s, seg)| GraphStart { field: emb_id, segment: seg.clone(), base: (s * per) as u32 }).collect();
    let timed = |starts: &[GraphStart]| {
        let t = Instant::now();
        let seg = SegmentFile::from_vec(build_segment_from(&schema, &docs, &opts, starts).unwrap()).unwrap();
        (t.elapsed().as_secs_f64(), recall(&seg, &key, &queries, 64), recall(&seg, &key, &queries, 100))
    };
    println!("{dim}-dim, {segs} x {per} ({} queries){}, an input {inputs_s:.2} s", queries.len(), if partition { ", partitioned by rating" } else { "" });
    for (name, s) in [("whole", &starts[..0]), ("largest", &starts[..1]), ("joined", &starts[..])] {
        let (secs, r64, r100) = timed(s);
        println!("  {name:8} {secs:7.2} s, recall@10 ef 64 {r64:.3}, ef 100 {r100:.3}");
    }
}
