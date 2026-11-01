//! SQ8 against binary and RaBitQ vector codes.
//!
//! Builds one segment per code kind and index kind over `n` unit vectors
//! drawn around `clusters` centres that share a common direction (as learned
//! embeddings do), or over a prepared BEIR collection's embeddings
//! (`--dataset`, from [`strata::data::data_dir`]), then measures per
//! configuration: build time, bytes of codes (what a search must keep in
//! memory) and of float32 rows (read only for the rescored candidates), and
//! recall@10 against exact search with single-thread latency, at several
//! `rerank` depths. Each code kind hands
//! [`strata::vector::codes::oversample`] times its depth to the float32
//! rescoring (`pool`); `rescored` is the rows it reads, fewer over RaBitQ
//! codes, whose error bounds rule some out.
//!
//! Synthetic data is kind to compressed codes: some embedding models lose
//! more to them than others, so check recall on the real vectors.
//!
//! Writes `results/vector/codes-<n>x<dim>.json` (or
//! `codes-<dataset>.json`; `--tag` adds `-<tag>`) and prints a table.

use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as Json};
use strata::metrics::{mean, percentile, recall};
use strata::segment::build::{build_segment, BuildOptions};
use strata::segment::SegmentFile;
use strata::vector::exact::exact_top_k;
use strata::vector::{kernels, SearchParams, VectorReader};
use strata::{Field, FieldType, Schema, Value, VectorCodes, VectorIndexKind};

#[derive(Parser)]
#[command(about = "SQ8 against binary vector codes on synthetic embeddings")]
struct Args {
    #[arg(long, default_value_t = 500_000)]
    n: usize,
    #[arg(long, default_value_t = 768)]
    dim: usize,
    #[arg(long, default_value_t = 1000)]
    clusters: usize,
    #[arg(long, default_value_t = 200)]
    queries: usize,
    /// Index kinds, comma-separated.
    #[arg(long, default_value = "flat,ivf")]
    kinds: String,
    /// Re-rank depths, comma-separated (binary and RaBitQ codes rescore
    /// `codes::oversample` times as many).
    #[arg(long, default_value = "10,20,40")]
    rerank: String,
    #[arg(long, default_value_t = 32)]
    nprobe: usize,
    #[arg(long, default_value_t = 64)]
    ef: usize,
    /// Threads for building and ground truth (searches run on one).
    #[arg(long, default_value_t = 4)]
    threads: usize,
    /// Code kinds, comma-separated.
    #[arg(long, default_value = "sq8,binary,rabitq1,rabitq2,rabitq4")]
    codes: String,
    /// A prepared BEIR collection's embeddings instead of synthetic ones
    /// (its first `n` documents and `queries` queries).
    #[arg(long)]
    dataset: Option<String>,
    #[arg(long)]
    tag: Option<String>,
    #[arg(long, default_value = "results/vector")]
    out: String,
}

/// A prepared collection's first `n` document and `queries` query vectors.
fn collection(name: &str, n: usize, queries: usize) -> (Vec<Arc<[f32]>>, Vec<Arc<[f32]>>, usize) {
    let dir = strata::data::data_dir().join(name);
    let meta: Json = serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
    let dim = meta["dim"].as_u64().unwrap() as usize;
    let read = |file: &str, max: usize| -> Vec<Arc<[f32]>> {
        let bytes = std::fs::read(dir.join(file)).unwrap();
        bytes.chunks_exact(dim * 4).take(max).map(|r| r.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect::<Vec<f32>>().into()).collect()
    };
    (read("doc_vecs.f32", n), read("query_vecs.f32", queries), dim)
}

fn gaussian(rng: &mut impl Rng) -> f32 {
    let (u, v): (f64, f64) = (rng.gen_range(1e-12..1.0), rng.gen());
    ((-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()) as f32
}

/// Unit vectors around shared-direction centres (centres fixed by `dim`
/// and `clusters`; `seed` picks the points).
fn embeddings(n: usize, dim: usize, clusters: usize, seed: u64) -> Vec<Arc<[f32]>> {
    let mut setup = rand::rngs::StdRng::seed_from_u64(7);
    let common: Vec<f32> = (0..dim).map(|_| 1.5 * gaussian(&mut setup)).collect();
    let centres: Vec<Vec<f32>> = (0..clusters).map(|_| (0..dim).map(|j| common[j] + gaussian(&mut setup)).collect()).collect();
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    (0..n)
        .map(|_| {
            let c = &centres[rng.gen_range(0..clusters)];
            let mut v: Vec<f32> = c.iter().map(|x| x + 1.5 * gaussian(&mut rng)).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter_mut().for_each(|x| *x /= norm);
            Arc::from(v)
        })
        .collect()
}

fn main() {
    let a = Args::parse();
    let t = Instant::now();
    let (docs, queries, dim) = match &a.dataset {
        Some(name) => collection(name, a.n, a.queries),
        None => (embeddings(a.n, a.dim, a.clusters, 1), embeddings(a.queries, a.dim, a.clusters, 2), a.dim),
    };
    let n = docs.len();
    eprintln!("data: {} × {dim} and {} queries in {:.1} s", n, queries.len(), t.elapsed().as_secs_f64());
    let t = Instant::now();
    let rows: Vec<&[f32]> = docs.iter().map(|d| &d[..]).collect();
    let qs: Vec<&[f32]> = queries.iter().map(|q| &q[..]).collect();
    let truth: Vec<Vec<u32>> = exact_top_k(&rows, &qs, 10, None, a.threads).into_iter().map(|h| h.into_iter().map(|h| h.doc).collect()).collect();
    eprintln!("exact top-10 in {:.1} s", t.elapsed().as_secs_f64());
    let values: Vec<Vec<Value>> = docs.iter().map(|d| vec![Value::Vector(d.clone())]).collect();
    drop(rows);

    let mut runs: Vec<Json> = Vec::new();
    println!("{:<7} {:<6} {:>6} {:>6} {:>8} {:>9} {:>9} {:>7} {:>9} {:>8} {:>8}", "codes", "index", "rerank", "pool", "rescored", "code MB", "f32 MB", "build s", "recall@10", "p50 ms", "p99 ms");
    let kinds: Vec<VectorCodes> = a.codes.split(',').map(|c| VectorCodes::parse(c).unwrap_or_else(|| panic!("unknown codes {c}"))).collect();
    for kind in a.kinds.split(',') {
        for &codes in &kinds {
            let field = Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Flat }).not_stored().codes(codes);
            let schema = Schema::new("t", vec![field], None).unwrap();
            let opts = BuildOptions::default().set("vector.index", kind).set("vector.threads", a.threads);
            let t = Instant::now();
            let seg = SegmentFile::from_vec(build_segment(&schema, &values, &opts).unwrap()).unwrap();
            let build_s = t.elapsed().as_secs_f64();
            let r = VectorReader::open(&seg, &schema.key_of("emb").unwrap()).unwrap();
            let code_mb = r.code_bytes() as f64 / 1e6;
            let f32_mb = (n * r.info().stride_f32 * 4) as f64 / 1e6;
            for depth in a.rerank.split(',').map(|x| x.parse::<usize>().unwrap()) {
                let p = SearchParams { rerank: depth, nprobe: a.nprobe, ef: a.ef, ..Default::default() };
                // One warm-up pass, then the timed one.
                for q in &qs {
                    std::hint::black_box(r.search(q, 10, &p, None));
                }
                let mut ms = Vec::with_capacity(qs.len());
                let mut rec = Vec::with_capacity(qs.len());
                let (mut candidates, mut rescored) = (0usize, 0usize);
                for (q, want) in qs.iter().zip(&truth) {
                    let t = Instant::now();
                    let out = r.search(q, 10, &p, None);
                    ms.push(t.elapsed().as_secs_f64() * 1e3);
                    candidates += out.stats.candidates;
                    rescored += out.stats.f32_distances;
                    rec.push(recall(&out.hits.iter().map(|h| h.doc).collect::<Vec<_>>(), want));
                }
                let (rc, p50, p99) = (mean(&rec), percentile(&ms, 50.0), percentile(&ms, 99.0));
                let (pool, rescored) = (candidates as f64 / qs.len() as f64, rescored as f64 / qs.len() as f64);
                println!("{:<7} {:<6} {:>6} {:>6.0} {:>8.1} {:>9.1} {:>9.1} {:>7.1} {:>9.3} {:>8.3} {:>8.3}", codes.as_str(), kind, depth, pool, rescored, code_mb, f32_mb, build_s, rc, p50, p99);
                runs.push(json!({
                    "codes": codes.as_str(), "index": kind, "rerank": depth, "candidates": pool, "rescored": rescored,
                    "code_bytes": r.code_bytes(), "f32_bytes": n * r.info().stride_f32 * 4, "build_s": build_s,
                    "recall_at_10": rc, "p50_ms": p50, "p99_ms": p99, "mean_ms": mean(&ms),
                }));
            }
        }
    }
    let machine = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let report = json!({
        "data": match &a.dataset {
            Some(name) => json!({"dataset": name, "n": n, "dim": dim, "queries": queries.len(), "synthetic": false}),
            None => json!({"n": n, "dim": dim, "clusters": a.clusters, "queries": queries.len(), "synthetic": true}),
        },
        "params": {"nprobe": a.nprobe, "ef": a.ef, "threads_build": a.threads, "threads_search": 1},
        "machine": {"cpu": machine, "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), "sq8_kernel": kernels::sq8_kernel()},
        "runs": runs,
    });
    std::fs::create_dir_all(&a.out).unwrap();
    let base = match &a.dataset {
        Some(name) => format!("codes-{name}"),
        None => format!("codes-{n}x{dim}"),
    };
    let path = format!("{}/{base}{}.json", a.out, a.tag.as_ref().map(|t| format!("-{t}")).unwrap_or_default());
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    eprintln!("wrote {path}");
}
