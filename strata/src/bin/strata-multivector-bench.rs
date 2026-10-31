//! Page retrieval with multi-vector fields: ViDoRe pages as ColPali-style
//! patch vectors, ranked by `maxsim`, against BM25 over the pages' OCR text.
//!
//! Reads what `strata/scripts/vidore_encode.py` writes for one dataset:
//! `pages.f32` and `queries.f32` (little-endian float32 vectors),
//! `pages.off` and `queries.off` (little-endian u64 offsets, in vectors, of
//! each page's and each query's first vector, and the total), `pages.jsonl`
//! (`{"id", "text"}`: the page's name and OCR text), `queries.jsonl`
//! (`{"query", "page"}`: the question and the page that answers it) and
//! `meta.json` (`{"dataset", "model", "dim"}`).
//!
//! Loads the pages into a database per layout (the default: binary codes
//! searched, SQ8 codes to score; `keep_f32`; pooled at write by a cosine
//! threshold or by Ward's linkage with a pool factor), then for each
//! query on one thread: `maxsim(patches, :q)` at several candidate caps and
//! exactly, `bm25(text, :q)` over the OCR text, and their `rrf`. Reports
//! NDCG@5 (one relevant page per query), latency, and the bytes per page of
//! the field's sections. The model's own float32 scores over every page are
//! the reference.
//!
//! Writes `results/vector/multivector-<dataset>.json` and prints a table.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::metrics::{mean, percentile};
use strata::query::exec::ExecOptions;
use strata::query::{bind, Params};
use strata::segment::build::BuildOptions;
use strata::storage::DbOptions;
use strata::vector::kernels;
use strata::{Field, FieldType, Schema, Value, VectorIndexKind};

#[derive(Parser)]
#[command(about = "maxsim over ViDoRe pages against BM25 over their OCR text")]
struct Args {
    /// The encoder's output directory for one dataset.
    #[arg(long)]
    data: PathBuf,
    /// Layouts: sq8 (the default), f32 (keep_f32), pool-<cosine> (pooled
    /// by a cosine threshold), ward-<factor> (pooled by Ward's linkage with a
    /// pool factor).
    #[arg(long, default_value = "sq8,f32,pool-0.95,pool-0.9,ward-2,ward-3")]
    layouts: String,
    /// Candidate caps, comma-separated.
    #[arg(long, default_value = "10,25,50,100,200,400")]
    caps: String,
    /// The layouts every page is also scored exactly on (`all`, or a
    /// comma-separated list): a second a question at 1,000 vectors a page.
    #[arg(long, default_value = "all")]
    exact: String,
    #[arg(long, default_value_t = 4)]
    threads: usize,
    #[arg(long, default_value = "results/vector")]
    out: String,
    /// Where the databases are built (removed at the end).
    #[arg(long)]
    dir: Option<PathBuf>,
}

fn floats(path: &Path) -> Vec<f32> {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()
}

fn offsets(path: &Path) -> Vec<usize> {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap()) as usize).collect()
}

fn jsonl(path: &Path) -> Vec<Json> {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())).lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect()
}

/// Each item's vectors, by its offsets into `all`.
fn split(all: &[f32], off: &[usize], dim: usize) -> Vec<Vec<Arc<[f32]>>> {
    off.windows(2).map(|w| (w[0]..w[1]).map(|i| Arc::from(&all[i * dim..(i + 1) * dim])).collect()).collect()
}

/// NDCG@5 with one relevant page: 1/log2(rank + 1) when it is in the top 5.
fn ndcg5(ranked: &[String], page: &str) -> f64 {
    ranked.iter().take(5).position(|p| p == page).map_or(0.0, |i| 1.0 / ((i + 2) as f64).log2())
}

/// A question as plain words, read the same with or without query operators.
fn words(q: &str) -> String {
    q.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The prefix of the `patches` field's section names.
fn section_prefix(db: &Db) -> String {
    format!("vec.{}.", db.schema().key_of("patches").expect("patches"))
}

fn main() {
    let a = Args::parse();
    let meta: Json = serde_json::from_str(&std::fs::read_to_string(a.data.join("meta.json")).unwrap()).unwrap();
    let dim = meta["dim"].as_u64().unwrap() as usize;
    let dataset = meta["dataset"].as_str().unwrap().to_string();
    let pages = split(&floats(&a.data.join("pages.f32")), &offsets(&a.data.join("pages.off")), dim);
    let queries = split(&floats(&a.data.join("queries.f32")), &offsets(&a.data.join("queries.off")), dim);
    let page_meta = jsonl(&a.data.join("pages.jsonl"));
    let query_meta = jsonl(&a.data.join("queries.jsonl"));
    assert_eq!(pages.len(), page_meta.len());
    assert_eq!(queries.len(), query_meta.len());
    let ids: Vec<String> = page_meta.iter().map(|p| p["id"].as_str().unwrap().to_string()).collect();
    let relevant: Vec<String> = query_meta.iter().map(|q| q["page"].as_str().unwrap().to_string()).collect();
    let n_vectors: usize = pages.iter().map(|p| p.len()).sum();
    eprintln!("{dataset}: {} pages ({:.0} vectors a page), {} queries ({:.1} vectors a query), dim {dim}", pages.len(), n_vectors as f64 / pages.len() as f64, queries.len(), queries.iter().map(|q| q.len()).sum::<usize>() as f64 / queries.len() as f64);

    // The model's own scores: float32 maxsim over every page.
    let maxsim = |q: &[Arc<[f32]>], p: &[Arc<[f32]>]| -> f32 { q.iter().map(|x| p.iter().map(|v| kernels::dot_f32(v, x)).fold(f32::NEG_INFINITY, f32::max)).sum() };
    let t = Instant::now();
    let reference: Vec<f64> = queries
        .iter()
        .zip(&relevant)
        .map(|(q, rel)| {
            let mut s: Vec<(f32, usize)> = pages.iter().enumerate().filter(|(_, p)| !p.is_empty()).map(|(i, p)| (maxsim(q, p), i)).collect();
            s.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)));
            ndcg5(&s.iter().take(5).map(|&(_, i)| ids[i].clone()).collect::<Vec<_>>(), rel)
        })
        .collect();
    let reference = mean(&reference);
    eprintln!("float32 brute force: NDCG@5 {reference:.4} in {:.1} s", t.elapsed().as_secs_f64());

    let dir = a.dir.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("strata-multivector-bench-{}", std::process::id())));
    std::fs::create_dir_all(&dir).unwrap();
    let caps: Vec<usize> = a.caps.split(',').map(|x| x.parse().unwrap()).collect();
    let mut runs: Vec<Json> = Vec::new();
    let mut layouts: Vec<Json> = Vec::new();
    println!("{:<10} {:<22} {:>7} {:>8} {:>8} {:>8}", "layout", "ranking", "NDCG@5", "p50 ms", "mean ms", "p99 ms");
    println!("{:<10} {:<22} {:>7.4}", "float32", "maxsim, brute force", reference);
    for layout in a.layouts.split(',') {
        let mut patches = Field::new("patches", FieldType::Vector { dim, index: VectorIndexKind::Hnsw { m: 16, ef_construction: 200 } }).multi();
        match layout {
            "sq8" => {}
            "f32" => patches = patches.keep_f32(),
            l if l.starts_with("pool-") => patches = patches.pooled(l["pool-".len()..].parse().unwrap()),
            l if l.starts_with("ward-") => patches = patches.pooled_by(l["ward-".len()..].parse().unwrap()),
            l => panic!("layout {l}"),
        }
        let schema = Schema::new("pages", vec![Field::new("id", FieldType::Keyword), Field::new("text", FieldType::Text { positions: true }), patches], Some("id")).unwrap();
        let opts = DbOptions { build: BuildOptions::default().set("vector.threads", a.threads), sync: false, ..DbOptions::default() };
        let path = dir.join(format!("{layout}.strata"));
        let t = Instant::now();
        let db = Db::create(&path, schema, opts).unwrap();
        db.load(pages.iter().zip(&page_meta).map(|(p, m)| {
            let vs = if p.is_empty() { Value::Null } else { Value::List(p.iter().map(|v| Value::Vector(v.clone())).collect()) };
            vec![m["id"].clone().as_str().unwrap().into(), Value::Str(m["text"].as_str().unwrap_or("").to_string()), vs]
        }))
        .unwrap();
        let load_s = t.elapsed().as_secs_f64();
        let snap = db.snapshot();
        // Sections are named by the field's id (`vec.f3.codes`).
        let prefix = section_prefix(&db);
        let mut sections: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for s in snap.segments() {
            for (name, bytes) in s.file.section_sizes() {
                if let Some(part) = name.strip_prefix(prefix.as_str()) {
                    *sections.entry(part.to_string()).or_default() += bytes;
                }
            }
        }
        let total: usize = sections.values().sum();
        let stored: usize = snap.segments().iter().map(|s| s.file.section_sizes().iter().filter(|(n, _)| n.starts_with(prefix.as_str()) && n.ends_with(".owner")).map(|(_, b)| b / 4).sum::<usize>()).sum();
        let per_page = total as f64 / pages.len() as f64;
        eprintln!("{layout}: {:.0} vectors a page kept, {:.0} bytes a page ({:?}), load {load_s:.1} s", stored as f64 / pages.len() as f64, per_page, sections);
        layouts.push(json!({"layout": layout, "vectors_per_page": stored as f64 / pages.len() as f64, "bytes_per_page": per_page, "sections_bytes": sections, "load_s": load_s}));

        let schema = snap.schema().clone();
        let exec = ExecOptions { threads: 1, ..ExecOptions::default() };
        let mut cases: Vec<(String, String)> = caps.iter().map(|c| (format!("maxsim, {c} candidates"), format!("maxsim(patches, :qs, candidates => {c})"))).collect();
        if a.exact == "all" || a.exact.split(',').any(|l| l == layout) {
            cases.push(("maxsim, exact".into(), "maxsim(patches, :qs, depth => 100000000, candidates => 100000000)".into()));
        }
        if layout == "sq8" {
            cases.push(("bm25 over OCR text".into(), "bm25(text, :t)".into()));
            cases.push(("rrf(bm25, maxsim)".into(), "rrf(bm25(text, :t), maxsim(patches, :qs))".into()));
        }
        for (name, score) in cases {
            let sql = format!("SELECT id FROM pages ORDER BY {score} DESC LIMIT 5");
            let selects: Vec<_> = queries
                .iter()
                .zip(&query_meta)
                .map(|(q, m)| {
                    let p = Params::new().set("qs", Value::List(q.iter().map(|v| Value::Vector(v.clone())).collect())).set("t", Value::Str(words(m["query"].as_str().unwrap())));
                    bind(&sql, &schema, &p).unwrap()
                })
                .collect();
            for s in selects.iter().take(20) {
                std::hint::black_box(db.execute(&snap, s, &exec).unwrap());
            }
            let mut ms = Vec::with_capacity(selects.len());
            let mut ndcg = Vec::with_capacity(selects.len());
            for (s, rel) in selects.iter().zip(&relevant) {
                let t = Instant::now();
                let out = db.execute(&snap, s, &exec).unwrap();
                ms.push(t.elapsed().as_secs_f64() * 1e3);
                ndcg.push(ndcg5(&out.keys.iter().map(|k| k.as_str().unwrap().to_string()).collect::<Vec<_>>(), rel));
            }
            let (n, p50, avg, p99) = (mean(&ndcg), percentile(&ms, 50.0), mean(&ms), percentile(&ms, 99.0));
            println!("{layout:<10} {name:<22} {n:>7.4} {p50:>8.3} {avg:>8.3} {p99:>8.3}");
            runs.push(json!({"layout": layout, "ranking": name, "ndcg_at_5": n, "p50_ms": p50, "mean_ms": avg, "p99_ms": p99}));
        }
        drop((snap, db));
        let _ = std::fs::remove_file(&path).or_else(|_| std::fs::remove_dir_all(&path));
    }
    let _ = std::fs::remove_dir_all(&dir);
    let machine = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let report = json!({
        "data": {"dataset": dataset, "model": meta["model"], "dim": dim, "pages": pages.len(), "queries": queries.len(), "vectors_per_page": n_vectors as f64 / pages.len() as f64,
                 "float32_bytes_per_page": (n_vectors * dim * 4) as f64 / pages.len() as f64},
        "params": {"limit": 5, "index": "hnsw m 16, ef_construction 200", "threads_build": a.threads, "threads_query": 1},
        "machine": {"cpu": machine, "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), "sq8_kernel": kernels::sq8_kernel()},
        "reference_ndcg_at_5": reference,
        "layouts": layouts,
        "runs": runs,
    });
    std::fs::create_dir_all(&a.out).unwrap();
    let path = format!("{}/multivector-{}.json", a.out, dataset.rsplit('/').next().unwrap());
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    eprintln!("wrote {path}");
}
