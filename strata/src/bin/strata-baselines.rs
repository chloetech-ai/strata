//! `strata-baselines`: builds the SQLite and Tantivy baselines for BEIR
//! collections, runs the shared workloads on them, and writes the results.
//!
//! ```text
//! strata-baselines --collections scifact,fiqa,trec-covid
//! strata-baselines --collections quora --limit 100000 --cold-sample 50
//! strata-baselines --collections quora --workload-only
//! ```
//!
//! Per collection (`<tag>` is the name, plus `_<limit>` with `--limit`):
//!
//! * `results/workloads/<tag>.json`: the workload, generated if missing.
//! * `results/baselines/<system>_<tag>.json`: build time and sizes, latency
//!   per query kind warm and cold, pages read cold, nDCG@10 of the judged
//!   kinds, configuration and (SQLite) query plans.
//! * `results/baselines/truth_<tag>.json`: SQLite's exact vector and hybrid
//!   results per query, the reference for Strata's approximate search.
//! * `results/baselines/compare_<tag>.json`: agreement of the systems'
//!   results per kind.
//!
//! Databases go to `--db-dir` and are deleted after the run unless `--keep`.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use clap::Parser;
use serde_json::{json, Value as Json};
use strata::baselines::{Sqlite, Tantivy, TantivyOptions};
use strata::data;
use strata::workload::run::{run_kind, BuildReport, KindReport, QueryOutput, RunOptions, System};
use strata::workload::{generate, Kind, Workload, WorkloadConfig};
use strata::{Error, Result};

#[derive(Parser)]
#[command(about = "Run the shared workloads on the SQLite and Tantivy baselines")]
struct Args {
    /// Collections under the data directory, comma separated.
    #[arg(long, value_delimiter = ',', default_value = "scifact")]
    collections: Vec<String>,
    /// Systems: sqlite, tantivy.
    #[arg(long, value_delimiter = ',', default_value = "sqlite,tantivy")]
    systems: Vec<String>,
    /// Use only the first N documents of each collection.
    #[arg(long)]
    limit: Option<usize>,
    /// Query kinds to run (default: all).
    #[arg(long, value_delimiter = ',')]
    kinds: Vec<String>,
    /// Most timed warm repetitions per query (latency is their median).
    #[arg(long, default_value_t = 3)]
    reps: usize,
    /// Seconds of timed warm passes per kind (fewer repetitions if needed).
    #[arg(long, default_value_t = 60.0)]
    warm_budget: f64,
    /// Most cold runs per kind; 0 disables cold runs.
    #[arg(long, default_value_t = 100)]
    cold_sample: usize,
    /// Seconds of cold runs per kind (at least 5 run).
    #[arg(long, default_value_t = 60.0)]
    cold_budget: f64,
    /// Run at most this many queries per kind (an evenly spaced sample).
    #[arg(long)]
    max_per_kind: Option<usize>,
    /// Run at most this many queries of each kind that uses vectors.
    #[arg(long)]
    max_vector: Option<usize>,
    /// Where databases are built.
    #[arg(long, default_value = "/home/user/data/baselines")]
    db_dir: PathBuf,
    /// Results root (default: the crate's `results/`).
    #[arg(long)]
    results_dir: Option<PathBuf>,
    /// Reuse databases left by an earlier `--keep` run instead of building.
    #[arg(long)]
    reuse: bool,
    /// Keep the databases after the run.
    #[arg(long)]
    keep: bool,
    /// Regenerate workloads even if their files exist.
    #[arg(long)]
    regen_workload: bool,
    /// Only generate the workloads.
    #[arg(long)]
    workload_only: bool,
    /// Tantivy indexing threads.
    #[arg(long, default_value_t = 1)]
    tantivy_threads: usize,
    /// Tantivy indexing memory budget, MiB.
    #[arg(long, default_value_t = 512)]
    tantivy_memory_mb: usize,
}

/// A built (or reopened) system and what is known about it.
struct Built {
    sys: Box<dyn System>,
    version: String,
    config: Json,
    build: Option<BuildReport>,
    extra: Json,
    /// Database files or directories, deleted after the run.
    paths: Vec<PathBuf>,
}

fn main() {
    let args = Args::parse();
    for name in &args.collections {
        if let Err(e) = run_collection(&args, name) {
            eprintln!("{name}: {e}");
            std::process::exit(1);
        }
    }
}

fn results_root(args: &Args) -> PathBuf {
    args.results_dir.clone().unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/results")))
}

fn write_json(path: &Path, value: &Json) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let text = serde_json::to_string_pretty(value).map_err(strata::error::corrupt)?;
    std::fs::write(path, text + "\n")?;
    Ok(())
}

/// Peak resident set size of this process, MiB.
fn peak_rss_mb() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    u.ru_maxrss as f64 / 1024.0
}

/// The build report kept next to a database, so `--reuse` can report it.
fn report_path(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.build.json", db.display()))
}

fn save_report(db: &Path, r: &BuildReport) -> Result<()> {
    write_json(&report_path(db), &json!(r))
}

fn load_report(db: &Path) -> Option<BuildReport> {
    serde_json::from_slice(&std::fs::read(report_path(db)).ok()?).ok()
}

fn kinds(args: &Args) -> Result<Vec<Kind>> {
    if args.kinds.is_empty() {
        return Ok(Kind::ALL.to_vec());
    }
    args.kinds.iter().map(|k| Kind::parse(k).ok_or_else(|| Error::Query(format!("unknown kind {k}")))).collect()
}

fn workload(args: &Args, coll: &data::Collection, tag: &str) -> Result<Workload> {
    let path = results_root(args).join("workloads").join(format!("{tag}.json"));
    if path.exists() && !args.regen_workload {
        let w = Workload::load(&path)?;
        if w.n_docs != coll.docs.len() {
            return Err(Error::Query(format!("{} is for {} documents, not {}", path.display(), w.n_docs, coll.docs.len())));
        }
        return Ok(w);
    }
    let t = Instant::now();
    let w = generate(coll, &WorkloadConfig::default());
    w.save(&path)?;
    let counts: Vec<String> = Kind::ALL.iter().map(|&k| format!("{} {}", k.name(), w.of_kind(k).len())).collect();
    eprintln!("   workload {} ({:.1}s): {}", path.display(), t.elapsed().as_secs_f64(), counts.join(", "));
    Ok(w)
}

fn build_sqlite(args: &Args, coll: &data::Collection, w: &Workload, tag: &str) -> Result<Built> {
    let path = args.db_dir.join(format!("{tag}.sqlite"));
    let (sys, build) = if args.reuse && path.exists() {
        (Sqlite::open(&path)?, load_report(&path))
    } else {
        let (s, r) = Sqlite::build(&path, coll)?;
        save_report(&path, &r)?;
        (s, Some(r))
    };
    // One plan per (kind, filter shape): the first such query.
    let mut plans = BTreeMap::new();
    for q in &w.queries {
        let key = format!("{}{}", q.kind.name(), q.shape.map_or(String::new(), |s| format!("/{}", s.name())));
        if let Entry::Vacant(e) = plans.entry(key) {
            e.insert(sys.explain(q, coll)?.into_iter().map(|(sql, plan)| json!({"sql": sql, "plan": plan})).collect::<Vec<Json>>());
        }
    }
    let options = sys.compile_options()?;
    let config = json!({
        "schema": "docs(id TEXT PRIMARY KEY, title, text, year INTEGER, category TEXT, rating REAL, topic TEXT, emb BLOB float32 LE) + indexes on year, category, rating, topic",
        "fts": "fts5(body, content='', contentless_delete=1, tokenize='porter unicode61'), optimized to one segment",
        "bm25": "FTS5 bm25(), k1 = 1.2, b = 0.75; OR of quoted query terms (workload::query_terms: stopwords removed)",
        "vectors": "brute force: ORDER BY vdot(emb, ?) DESC LIMIT k, vdot a Rust scalar function",
        "hybrid": "application-side RRF (k = 60) of BM25 and vector top-100",
        "pragmas": "page_size 4096, cache_size -2000 (2000 KiB), mmap_size 0, journal_mode WAL, synchronous FULL (defaults except WAL)",
        "load": "one transaction: inserts, CREATE INDEX, FTS5 optimize; then ANALYZE, wal_checkpoint(TRUNCATE)",
        "fts5_enabled": options.iter().any(|o| o == "ENABLE_FTS5"),
    });
    Ok(Built {
        sys: Box::new(sys),
        version: Sqlite::version(),
        config,
        build,
        extra: json!({ "plans": plans }),
        paths: vec![path.clone(), PathBuf::from(format!("{}-wal", path.display())), PathBuf::from(format!("{}-shm", path.display())), report_path(&path)],
    })
}

fn build_tantivy(args: &Args, coll: &data::Collection, tag: &str) -> Result<Built> {
    let dir = args.db_dir.join(format!("{tag}.tantivy"));
    let opts = TantivyOptions { threads: args.tantivy_threads, memory_budget: args.tantivy_memory_mb << 20 };
    let report = PathBuf::from(format!("{}.build.json", dir.display()));
    let (sys, build) = if args.reuse && dir.exists() {
        (Tantivy::open(&dir)?, serde_json::from_slice(&std::fs::read(&report).unwrap_or_default()).ok())
    } else {
        let (s, r) = Tantivy::build(&dir, coll, &opts)?;
        write_json(&report, &json!(r))?;
        (s, Some(r))
    };
    let config = json!({
        "schema": "id STRING|STORED|FAST, title STORED, text STORED, body TEXT en_stem WithFreqsAndPositions, year i64 INDEXED|FAST, rating f64 INDEXED|FAST, category STRING|FAST, topic STRING|FAST, seq u64 FAST",
        "bm25": "Tantivy BM25, k1 = 1.2, b = 0.75; BooleanQuery of Should TermQuery (Block-WAND)",
        "filters": "Must RangeQuery (fast-field scan) / TermQuery / TermSetQuery; ConstScoreQuery(0) under BM25",
        "facets": "collector counting category ordinals in the fast column",
        "indexing": format!("{} thread(s), {} MiB budget, NoMergePolicy, then one explicit merge", opts.threads, args.tantivy_memory_mb),
        "doc_store": "LZ4, 16 KiB blocks (defaults)",
        "reader": "ReloadPolicy::Manual, single-threaded search",
    });
    let segments = sys.segments();
    Ok(Built {
        sys: Box::new(sys),
        version: Tantivy::version(),
        config,
        build,
        extra: json!({ "segments": segments }),
        paths: vec![dir, report],
    })
}

fn summary_line(r: &KindReport) -> String {
    let cold = r.cold.as_ref().map_or("-".into(), |c| format!("{:.3}", c.p50_ms));
    let pages = r.cold_pages.as_ref().map_or("-".into(), |p| format!("{:.0}", p.p50));
    let ndcg = r.ndcg10.map_or("-".into(), |x| format!("{x:.4}"));
    format!(
        "   {:<16} n={:<5} x{} warm mean {:>9.3} p50 {:>9.3} p99 {:>9.3} ms | cold n={:<3} p50 {:>9} ms, pages p50 {:>6} | nDCG@10 {}",
        r.kind.name(),
        r.queries,
        r.reps,
        r.warm.mean_ms,
        r.warm.p50_ms,
        r.warm.p99_ms,
        r.cold.as_ref().map_or(0, |c| c.n),
        cold,
        pages,
        ndcg
    )
}

/// Agreement of two systems' outputs on the queries both ran, with up to
/// ten differing counts as `[workload index, count a, count b]`.
fn compare(a: &[(usize, QueryOutput)], b: &[(usize, QueryOutput)]) -> Json {
    let b: BTreeMap<usize, &QueryOutput> = b.iter().map(|(i, o)| (*i, o)).collect();
    let (mut n, mut same_ids, mut overlap, mut counts, mut same_counts, mut facets, mut same_facets) = (0, 0, 0.0, 0, 0, 0, 0);
    let mut differing = Vec::new();
    for (i, x) in a {
        let Some(y) = b.get(i) else { continue };
        n += 1;
        same_ids += (x.ids == y.ids) as usize;
        let common = x.ids.iter().filter(|id| y.ids.contains(id)).count();
        let size = x.ids.len().max(y.ids.len());
        overlap += if size == 0 { 1.0 } else { common as f64 / size as f64 };
        if let (Some(c), Some(d)) = (x.count, y.count) {
            counts += 1;
            same_counts += (c == d) as usize;
            if c != d && differing.len() < 10 {
                differing.push(json!([i, c, d]));
            }
        }
        if let (Some(f), Some(g)) = (&x.facets, &y.facets) {
            facets += 1;
            same_facets += (f == g) as usize;
        }
    }
    let frac = |k: usize, of: usize| if of == 0 { Json::Null } else { json!(k as f64 / of as f64) };
    json!({
        "queries": n,
        "same_ids": frac(same_ids, n),
        "mean_overlap": if n == 0 { Json::Null } else { json!(overlap / n as f64) },
        "same_count": frac(same_counts, counts),
        "same_facets": frac(same_facets, facets),
        "differing_counts": differing,
    })
}

fn run_collection(args: &Args, name: &str) -> Result<()> {
    let tag = args.limit.map_or(name.to_string(), |l| format!("{name}_{l}"));
    eprintln!("== {tag}");
    let t = Instant::now();
    let mut coll = data::load(name, args.limit)?;
    eprintln!("   loaded {} documents, {} queries ({:.1}s)", coll.docs.len(), coll.queries.len(), t.elapsed().as_secs_f64());
    let w = workload(args, &coll, &tag)?;
    if args.workload_only {
        return Ok(());
    }
    let kinds = kinds(args)?;
    std::fs::create_dir_all(&args.db_dir)?;
    let mut built = Vec::new();
    for s in &args.systems {
        let t = Instant::now();
        let b = match s.as_str() {
            "sqlite" => build_sqlite(args, &coll, &w, &tag)?,
            "tantivy" => build_tantivy(args, &coll, &tag)?,
            other => return Err(Error::Query(format!("unknown system {other}"))),
        };
        if let Some(r) = &b.build {
            eprintln!("   {s}: built in {:.1}s, {:.1} MiB ({:.1}s with reopen)", r.seconds, r.bytes as f64 / (1 << 20) as f64, t.elapsed().as_secs_f64());
        }
        built.push(b);
    }
    // Queries need only the query texts and vectors.
    coll.docs = Vec::new();

    let opts = RunOptions {
        reps: args.reps,
        warm_budget_secs: args.warm_budget,
        cold_sample: args.cold_sample,
        cold_budget_secs: args.cold_budget,
        max_per_kind: args.max_per_kind,
        max_vector: args.max_vector,
    };
    let out_dir = results_root(args).join("baselines");
    let mut outputs: Vec<BTreeMap<Kind, Vec<(usize, QueryOutput)>>> = Vec::new();
    for b in &mut built {
        let sys_name = b.sys.name().to_string();
        eprintln!("   {sys_name}: running");
        let mut reports = Vec::new();
        let mut outs = BTreeMap::new();
        let mut unsupported = Vec::new();
        for &k in &kinds {
            if !b.sys.supports(k) {
                unsupported.push(k.name());
                continue;
            }
            let run = run_kind(b.sys.as_mut(), &w, &coll, k, &opts)?;
            eprintln!("{}", summary_line(&run.report));
            reports.push(run.report);
            outs.insert(k, run.outputs);
        }
        let result = json!({
            "system": sys_name,
            "version": b.version,
            "collection": name,
            "limit": args.limit,
            "n_docs": w.n_docs,
            "workload": format!("results/workloads/{tag}.json"),
            "config": b.config,
            "run": {
                "options": opts,
                "unix_time": SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
                "peak_rss_mib": peak_rss_mb(),
            },
            "build": b.build,
            "kinds": reports,
            "unsupported": unsupported,
            "extra": b.extra,
        });
        write_json(&out_dir.join(format!("{sys_name}_{tag}.json")), &result)?;
        if sys_name == "sqlite" {
            let truth: BTreeMap<&str, Vec<(usize, &Vec<String>)>> = outs
                .iter()
                .filter(|(k, _)| k.uses_vector())
                .map(|(k, o)| (k.name(), o.iter().map(|(i, x)| (*i, &x.ids)).collect()))
                .collect();
            let doc = json!({
                "collection": name,
                "limit": args.limit,
                "workload": format!("results/workloads/{tag}.json"),
                "source": "SQLite exact search (vdot brute force); hybrid = RRF of FTS5 BM25 and exact vector top-100",
                "kinds": truth,
            });
            std::fs::write(out_dir.join(format!("truth_{tag}.json")), serde_json::to_string(&doc).map_err(strata::error::corrupt)? + "\n")?;
        }
        outputs.push(outs);
    }

    if built.len() == 2 {
        let mut by_kind = BTreeMap::new();
        for k in &kinds {
            if let (Some(a), Some(b)) = (outputs[0].get(k), outputs[1].get(k)) {
                let c = compare(a, b);
                eprintln!("   agreement {:<14} {c}", k.name());
                by_kind.insert(k.name(), c);
            }
        }
        let doc = json!({"collection": name, "limit": args.limit, "systems": [built[0].sys.name(), built[1].sys.name()], "kinds": by_kind});
        write_json(&out_dir.join(format!("compare_{tag}.json")), &doc)?;
    }

    if !args.keep {
        for b in built {
            drop(b.sys);
            for p in b.paths {
                if p.is_dir() {
                    std::fs::remove_dir_all(&p)?;
                } else if p.exists() {
                    std::fs::remove_file(&p)?;
                }
            }
        }
    }
    Ok(())
}
