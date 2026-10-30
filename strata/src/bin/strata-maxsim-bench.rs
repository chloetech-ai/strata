//! `maxsim(emb, vectors)` against brute force, by the number of query
//! vectors.
//!
//! Loads `n` unit vectors drawn around `clusters` centres that share a
//! common direction (as learned embeddings do) into a database per index
//! kind, then runs `ORDER BY maxsim(emb, :qs) DESC LIMIT 10` for each of
//! `histories` lists of query vectors, each list's first Q vectors for every
//! Q, on one thread. Measures latency, recall@10 against the exact answer
//! (the best of each row's dot products with the Q vectors, over every row)
//! and the cost of each added query vector: the slope of mean latency over
//! Q. `similarity(emb, :q)` with the first vector is the baseline.
//!
//! The data is synthetic; how close the approximate searches come depends on
//! the embeddings, and the cost per added vector is one more search.
//!
//! Writes `results/vector/maxsim-<n>x<dim>.json` and prints a table.

use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::metrics::{mean, percentile, recall};
use strata::query::exec::ExecOptions;
use strata::query::{bind, Params};
use strata::segment::build::BuildOptions;
use strata::storage::DbOptions;
use strata::vector::exact::exact_top_k;
use strata::vector::kernels;
use strata::{Field, FieldType, Schema, Value, VectorIndexKind};

#[derive(Parser)]
#[command(about = "maxsim against brute force, by the number of query vectors")]
struct Args {
    #[arg(long, default_value_t = 200_000)]
    n: usize,
    #[arg(long, default_value_t = 384)]
    dim: usize,
    #[arg(long, default_value_t = 1000)]
    clusters: usize,
    /// Lists of query vectors (a user's history each).
    #[arg(long, default_value_t = 100)]
    histories: usize,
    /// Numbers of query vectors, comma-separated.
    #[arg(long, default_value = "1,2,4,8,16,32,64")]
    qs: String,
    /// Index kinds, comma-separated.
    #[arg(long, default_value = "hnsw,flat")]
    kinds: String,
    /// Threads for building and ground truth (queries run on one).
    #[arg(long, default_value_t = 4)]
    threads: usize,
    #[arg(long, default_value = "results/vector")]
    out: String,
    /// Where the databases are built (removed at the end).
    #[arg(long)]
    dir: Option<std::path::PathBuf>,
}

fn gaussian(rng: &mut impl Rng) -> f32 {
    let (u, v): (f64, f64) = (rng.gen_range(1e-12..1.0), rng.gen());
    ((-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()) as f32
}

/// Unit vectors around shared-direction centres (centres fixed by `dim`
/// and `clusters`; `seed` picks the points), as in `strata-codes-bench`.
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

/// The slope of `y` over `x` by least squares.
fn slope(x: &[f64], y: &[f64]) -> f64 {
    let (mx, my) = (mean(x), mean(y));
    let num: f64 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
    let den: f64 = x.iter().map(|a| (a - mx) * (a - mx)).sum();
    num / den
}

fn main() {
    let a = Args::parse();
    let qs: Vec<usize> = a.qs.split(',').map(|x| x.parse().unwrap()).collect();
    let most = *qs.iter().max().unwrap();
    let t = Instant::now();
    let docs = embeddings(a.n, a.dim, a.clusters, 1);
    let hist = embeddings(a.histories * most, a.dim, a.clusters, 2);
    eprintln!("data: {} × {} in {:.1} s", a.n, a.dim, t.elapsed().as_secs_f64());

    // The exact answer for the first Q vectors of a history: the best rows
    // of the union of each vector's exact top 10 (a row of the exact top 10
    // is in the top 10 of the vector it is closest to), by their best dot
    // product.
    let t = Instant::now();
    let rows: Vec<&[f32]> = docs.iter().map(|d| &d[..]).collect();
    let queries: Vec<&[f32]> = hist.iter().map(|q| &q[..]).collect();
    let tops: Vec<Vec<u32>> = exact_top_k(&rows, &queries, 10, None, a.threads).into_iter().map(|h| h.into_iter().map(|h| h.doc).collect()).collect();
    let truth = |h: usize, q: usize| -> Vec<u32> {
        let vs = &hist[h * most..h * most + q];
        let mut cands: Vec<u32> = tops[h * most..h * most + q].iter().flatten().copied().collect();
        cands.sort_unstable();
        cands.dedup();
        let mut scored: Vec<(f32, u32)> = cands.iter().map(|&d| (vs.iter().map(|v| kernels::dot_f32(&docs[d as usize], v)).fold(f32::NEG_INFINITY, f32::max), d)).collect();
        scored.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)));
        scored.into_iter().take(10).map(|(_, d)| d).collect()
    };
    eprintln!("exact top-10 of {} vectors in {:.1} s", queries.len(), t.elapsed().as_secs_f64());
    drop(rows);

    let dir = a.dir.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("strata-maxsim-bench-{}", std::process::id())));
    std::fs::create_dir_all(&dir).unwrap();
    let mut runs: Vec<Json> = Vec::new();
    let mut fits = serde_json::Map::new();
    println!("{:<6} {:<10} {:>4} {:>9} {:>8} {:>8} {:>8} {:>9}", "index", "score", "Q", "recall@10", "p50 ms", "mean ms", "p99 ms", "scored");
    for kind in a.kinds.split(',') {
        let index = match kind {
            "hnsw" => VectorIndexKind::Hnsw { m: 16, ef_construction: 200 },
            "flat" => VectorIndexKind::Flat,
            k => panic!("index kind {k}"),
        };
        let schema = Schema::new("docs", vec![Field::new("id", FieldType::Keyword), Field::new("emb", FieldType::Vector { dim: a.dim, index }).not_stored()], Some("id")).unwrap();
        let opts = DbOptions { build: BuildOptions::default().set("vector.threads", a.threads), sync: false, ..DbOptions::default() };
        let t = Instant::now();
        let db = Db::create(&dir.join(format!("{kind}.strata")), schema, opts).unwrap();
        db.load(docs.iter().enumerate().map(|(i, d)| vec![Value::Str(format!("d{i}")), Value::Vector(d.clone())])).unwrap();
        let load_s = t.elapsed().as_secs_f64();
        let snap = db.snapshot();
        let schema = snap.schema().clone();
        let exec = ExecOptions { threads: 1, ..ExecOptions::default() };
        let id = |k: &Value| k.as_str().unwrap()[1..].parse::<u32>().unwrap();
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        let mut cases: Vec<(&str, usize)> = vec![("similarity", 1)];
        cases.extend(qs.iter().map(|&q| ("maxsim", q)));
        for (score, q) in cases {
            let sql = match score {
                "similarity" => "SELECT id FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 10",
                _ => "SELECT id FROM docs ORDER BY maxsim(emb, :qs) DESC LIMIT 10",
            };
            let selects: Vec<_> = (0..a.histories)
                .map(|h| {
                    let vs = &hist[h * most..h * most + q];
                    let params = Params::new().set("q", Value::Vector(vs[0].clone())).set("qs", Value::List(vs.iter().map(|v| Value::Vector(v.clone())).collect()));
                    bind(sql, &schema, &params).unwrap()
                })
                .collect();
            // One warm-up pass, then the timed one.
            for s in &selects {
                std::hint::black_box(db.execute(&snap, s, &exec).unwrap());
            }
            let mut ms = Vec::with_capacity(a.histories);
            let mut rec = Vec::with_capacity(a.histories);
            let mut scored = 0u64;
            for (h, s) in selects.iter().enumerate() {
                let t = Instant::now();
                let out = db.execute(&snap, s, &exec).unwrap();
                ms.push(t.elapsed().as_secs_f64() * 1e3);
                scored += out.stats.scored;
                rec.push(recall(&out.keys.iter().map(id).collect::<Vec<_>>(), &truth(h, q)));
            }
            let (rc, p50, avg, p99) = (mean(&rec), percentile(&ms, 50.0), mean(&ms), percentile(&ms, 99.0));
            let scored = scored as f64 / a.histories as f64;
            println!("{kind:<6} {score:<10} {q:>4} {rc:>9.3} {p50:>8.3} {avg:>8.3} {p99:>8.3} {scored:>9.0}");
            if score == "maxsim" {
                xs.push(q as f64);
                ys.push(avg);
            }
            runs.push(json!({"index": kind, "score": score, "query_vectors": q, "recall_at_10": rc, "p50_ms": p50, "mean_ms": avg, "p99_ms": p99, "scored": scored}));
        }
        let per = slope(&xs, &ys);
        println!("{kind}: {per:.3} ms per added query vector (load {load_s:.1} s)");
        fits.insert(kind.to_string(), json!({"ms_per_added_vector": per, "load_s": load_s}));
        drop((snap, db));
        let path = dir.join(format!("{kind}.strata"));
        let _ = std::fs::remove_file(&path).or_else(|_| std::fs::remove_dir_all(&path));
    }
    let _ = std::fs::remove_dir_all(&dir);
    let machine = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let report = json!({
        "data": {"n": a.n, "dim": a.dim, "clusters": a.clusters, "histories": a.histories, "synthetic": true},
        "params": {"limit": 10, "threads_build": a.threads, "threads_query": 1},
        "machine": {"cpu": machine, "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), "sq8_kernel": kernels::sq8_kernel()},
        "per_added_vector": fits,
        "runs": runs,
    });
    std::fs::create_dir_all(&a.out).unwrap();
    let path = format!("{}/maxsim-{}x{}.json", a.out, a.n, a.dim);
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    eprintln!("wrote {path}");
}
