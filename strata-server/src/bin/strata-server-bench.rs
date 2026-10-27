//! End-to-end benchmark of Strata and strata-server on a BEIR collection
//! with real embeddings: how compact the database is, what 1-bit vector
//! codes cost in recall against 8-bit ones on real vectors, and how fast the
//! server answers each kind of query over HTTP, with one client and under
//! concurrent clients, from one database and from a coordinator over shards.
//!
//! Laptop run (the collection prepared by `strata/scripts/prepare_beir.py`):
//!
//! ```text
//! cd strata-server
//! STRATA_DATA=/path/to/beir caffeinate -i cargo run --release --bin strata-server-bench -- \
//!     --collection quora --work /path/outside/the/repo
//! ```
//!
//! Steps, all in this one process:
//!
//! 1. Builds the collection three ways in `--work` (one fully merged
//!    segment each, HNSW): 8-bit codes, 1-bit codes, and split into
//!    `--shards` databases by primary key. Records build time, file size
//!    and bytes by component. `--reuse` reopens databases a previous run
//!    left.
//! 2. Vector search through SQL on one thread: recall@10 against an exact
//!    float32 search, and latency, for 8-bit codes (HNSW at ef 64 and 320,
//!    and the exact scan of the codes) and 1-bit codes (HNSW and scan).
//! 3. Serves the 8-bit database over HTTP and times each query kind for
//!    `--seconds` at each `--concurrency`: client latency, the server's own
//!    time (`took_ms`) and queries per second, then the vector and hybrid
//!    kinds again from the 1-bit database. Clients run in the same
//!    process, so they share the machine's cores with the server.
//! 4. Writes over HTTP to a copy of the 8-bit database: single in-place
//!    updates, SQL updates by key, and batches of inserts (deleted again
//!    afterwards).
//! 5. The same reads through a coordinator over the shards (gRPC on
//!    localhost), and how often its answers equal one database's: the same
//!    keys in order, the same scores (rows tied on score may swap), and for
//!    kinds that search vectors, recall@10 of each against an exact scan.
//!
//! Writes `--out/<collection>.json` (default `results/bench/`) naming the
//! machine, and prints the tables in Markdown.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{json, Value as Json};
use strata::data::{self, col};
use strata::db::Db;
use strata::metrics::{mean, percentile, recall};
use strata::query::Params;
use strata::segment::build::BuildOptions;
use strata::storage::DbOptions;
use strata::vector::exact::exact_top_k;
use strata::vector::VectorReader;
use strata::{Document, Value, VectorCodes, VectorIndexKind};
use strata_server::coord::Coordinator;
use strata_server::server;
use strata_server::service::{shard_of, AgentConfig, Backend, Service};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(about = "End-to-end benchmark of Strata and strata-server on a BEIR collection")]
struct Args {
    /// A collection under $STRATA_DATA.
    #[arg(long, default_value = "quora")]
    collection: String,
    /// Where the databases are built (several GB for Quora).
    #[arg(long)]
    work: PathBuf,
    #[arg(long, default_value = "results/bench")]
    out: PathBuf,
    /// Keep only the first N documents (for a quick check).
    #[arg(long)]
    limit: Option<usize>,
    /// Queries taken from the collection's query set.
    #[arg(long, default_value_t = 300)]
    queries: usize,
    /// Threads for building vector indexes and the exact search.
    #[arg(long, default_value_t = 4)]
    threads: usize,
    /// Concurrent HTTP clients, comma-separated.
    #[arg(long, value_delimiter = ',', default_value = "1,4,16")]
    concurrency: Vec<usize>,
    /// Seconds per query kind and concurrency.
    #[arg(long, default_value_t = 8.0)]
    seconds: f64,
    /// Shards for the coordinator run (0 skips it).
    #[arg(long, default_value_t = 2)]
    shards: usize,
    /// Reopen databases a previous run built in --work.
    #[arg(long)]
    reuse: bool,
}

fn wal_path(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", p.display()))
}

fn serve_opts() -> DbOptions {
    server::db_options(false)
}

struct Built {
    name: String,
    db: Arc<Db>,
    build_s: f64,
    file_bytes: u64,
    docs: usize,
    components: BTreeMap<String, u64>,
    code_bytes: u64,
}

/// Builds `docs` into one segment at `path`, or reopens it with `reuse`.
fn build(args: &Args, name: &str, path: &Path, codes: VectorCodes, dim: usize, docs: Vec<Document>) -> strata::Result<Built> {
    let report = PathBuf::from(format!("{}.build.json", path.display()));
    let n = docs.len();
    let reused = if args.reuse && path.exists() {
        std::fs::read(&report).ok().and_then(|b| serde_json::from_slice::<Json>(&b).ok()).and_then(|j| j["build_s"].as_f64())
    } else {
        None
    };
    let (db, build_s) = match reused {
        Some(s) => (Db::open(path, serve_opts())?, s),
        None => {
            for p in [path.to_path_buf(), wal_path(path)] {
                if p.exists() {
                    std::fs::remove_file(&p)?;
                }
            }
            let mut schema = data::beir_schema(dim, VectorIndexKind::Hnsw { m: 16, ef_construction: 200 });
            schema.fields[col::EMB].codes = codes;
            let opts = DbOptions { build: BuildOptions::default().set("vector.threads", args.threads), segment_max_docs: n.max(1), ..serve_opts() };
            let t = Instant::now();
            let db = Db::create(path, schema, opts)?;
            db.load(docs)?;
            let s = t.elapsed().as_secs_f64();
            std::fs::write(&report, json!({"build_s": s}).to_string())?;
            (db, s)
        }
    };
    let mut components: BTreeMap<String, u64> = BTreeMap::new();
    let mut code_bytes = 0u64;
    let snap = db.snapshot();
    for v in snap.segments() {
        for (name, len) in v.file.section_sizes() {
            let mut parts = name.split('.');
            let key = match parts.next().unwrap_or("") {
                "text" => format!("text index ({})", parts.next().unwrap_or("")),
                "vec" => {
                    let _field = parts.next();
                    format!("vectors ({})", parts.next().unwrap_or(""))
                }
                "col" | "range" | "kw" => "structured columns".to_string(),
                "store" => "doc store".to_string(),
                other => other.to_string(),
            };
            *components.entry(key).or_insert(0) += len as u64;
        }
        code_bytes += VectorReader::open(&v.file, "emb")?.code_bytes() as u64;
    }
    let docs = snap.n_live_docs() as usize;
    let file_bytes = db.storage().file_stats().file_bytes;
    eprintln!("{name}: {docs} docs, {:.1} MB, built in {build_s:.1} s", file_bytes as f64 / 1e6);
    Ok(Built { name: name.to_string(), db: Arc::new(db), build_s, file_bytes, docs, components, code_bytes })
}

fn machine() -> Json {
    let sh = |cmd: &str, args: &[&str]| std::process::Command::new(cmd).args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
    let cpu = sh("sysctl", &["-n", "machdep.cpu.brand_string"]).or_else(|| {
        std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
    });
    let mem = sh("sysctl", &["-n", "hw.memsize"]).and_then(|s| s.parse::<f64>().ok()).or_else(|| {
        std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| s.lines().next().and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok()).map(|kb| kb * 1024.0))
    });
    let os = sh("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}")).or_else(|| sh("uname", &["-sr"]));
    json!({
        "cpu": cpu,
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "memory_gb": mem.map(|b| (b / 1e9 * 10.0).round() / 10.0),
        "os": os,
        "arch": std::env::consts::ARCH,
    })
}

/// The longest ASCII word of `s` with at least 4 letters (for MATCH; short
/// words are often stopwords, which MATCH drops).
fn match_word(s: &str) -> Option<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() >= 4).max_by_key(|w| w.len()).map(|w| w.to_lowercase())
}

const KINDS: &[(&str, &str)] = &[
    ("bm25", "SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 10"),
    ("bm25 + filter", "SELECT id, score() FROM docs WHERE year >= 2010 AND category IN ('c00', 'c01', 'c02', 'c03') ORDER BY bm25(body, :t) DESC LIMIT 10"),
    ("vector", "SELECT id, score() FROM docs ORDER BY similarity(emb, :v) DESC LIMIT 10"),
    ("vector + filter", "SELECT id, score() FROM docs WHERE rating >= 4 ORDER BY similarity(emb, :v) DESC LIMIT 10"),
    ("hybrid (RRF)", "SELECT id, score() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :v)) DESC LIMIT 10"),
    ("hybrid (weighted)", "SELECT id, score() FROM docs ORDER BY 0.2 * bm25(body, :t) + 0.8 * similarity(emb, :v) DESC LIMIT 10"),
    ("boosted (bm25 × decay)", "SELECT id, score() FROM docs ORDER BY bm25(body, :t) * decay(year, 2024, 5) DESC LIMIT 10"),
    ("facet", "SELECT category, COUNT(*) FROM docs WHERE MATCH(body, :w) GROUP BY category ORDER BY 2 DESC LIMIT 5"),
    ("key lookup", "SELECT id, title, year, rating FROM docs WHERE id = :k"),
];

/// Request bodies for each kind, from the queries (and document keys).
fn bodies(queries: &[data::Query], keys: &[String]) -> Vec<(&'static str, Vec<Json>)> {
    KINDS
        .iter()
        .map(|&(kind, sql)| {
            let reqs = queries
                .iter()
                .enumerate()
                .filter_map(|(i, q)| {
                    let mut p = serde_json::Map::new();
                    if sql.contains(":t") {
                        p.insert("t".into(), json!(q.text));
                    }
                    if sql.contains(":v") {
                        p.insert("v".into(), json!(q.vector.to_vec()));
                    }
                    if sql.contains(":w") {
                        p.insert("w".into(), json!(match_word(&q.text)?));
                    }
                    if sql.contains(":k") {
                        p.insert("k".into(), json!(keys[(i * 7919) % keys.len()]));
                    }
                    Some(json!({"sql": sql, "params": p}))
                })
                .collect();
            (kind, reqs)
        })
        .collect()
}

#[derive(Default)]
struct Timed {
    ms: Vec<f64>,
    server_ms: Vec<f64>,
    errors: usize,
    first_error: Option<String>,
    seconds: f64,
}

impl Timed {
    fn json(&self, setup: &str, kind: &str, concurrency: usize) -> Json {
        json!({
            "setup": setup, "kind": kind, "concurrency": concurrency, "requests": self.ms.len(),
            "qps": self.ms.len() as f64 / self.seconds.max(1e-9),
            "p50_ms": percentile(&self.ms, 50.0), "p95_ms": percentile(&self.ms, 95.0), "p99_ms": percentile(&self.ms, 99.0),
            "mean_ms": mean(&self.ms), "server_p50_ms": percentile(&self.server_ms, 50.0),
            "errors": self.errors, "first_error": self.first_error,
        })
    }
}

/// Posts `bodies` round-robin from `concurrency` clients for `seconds`
/// (or, with `once`, each body once from one client).
async fn drive(client: &reqwest::Client, url: &str, bodies: Arc<Vec<Vec<u8>>>, concurrency: usize, seconds: f64, once: bool) -> Timed {
    let next = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let t0 = Instant::now();
    let tasks: Vec<_> = (0..concurrency)
        .map(|_| {
            let (client, url, bodies, next) = (client.clone(), url.to_string(), bodies.clone(), next.clone());
            tokio::spawn(async move {
                let mut t = Timed::default();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if (once && i >= bodies.len()) || (!once && Instant::now() >= deadline) {
                        break;
                    }
                    let start = Instant::now();
                    let r = client.post(&url).header("content-type", "application/json").body(bodies[i % bodies.len()].clone()).send().await;
                    let (ok, answer) = match r {
                        Ok(r) => {
                            let ok = r.status().is_success();
                            (ok, r.json::<Json>().await.unwrap_or(Json::Null))
                        }
                        Err(e) => (false, json!({"error": e.to_string()})),
                    };
                    let ms = start.elapsed().as_secs_f64() * 1e3;
                    if ok {
                        t.ms.push(ms);
                        if let Some(s) = answer["took_ms"].as_f64() {
                            t.server_ms.push(s);
                        }
                    } else {
                        t.errors += 1;
                        t.first_error.get_or_insert_with(|| answer.to_string());
                    }
                }
                t
            })
        })
        .collect();
    let mut all = Timed::default();
    for task in tasks {
        let t = task.await.expect("client task");
        all.ms.extend(t.ms);
        all.server_ms.extend(t.server_ms);
        all.errors += t.errors;
        if all.first_error.is_none() {
            all.first_error = t.first_error;
        }
    }
    all.seconds = t0.elapsed().as_secs_f64();
    all
}

fn encode(bodies: &[Json]) -> Arc<Vec<Vec<u8>>> {
    Arc::new(bodies.iter().map(|b| serde_json::to_vec(b).unwrap()).collect())
}

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn service(backend: Backend, schema: strata::Schema) -> Arc<Service> {
    Arc::new(Service::new(backend, schema, agent(), 256, Duration::from_secs(30)))
}

async fn serve_http(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(server::serve_http(svc, l, None));
    format!("http://{addr}")
}

async fn serve_grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(server::serve_grpc(svc, l));
    format!("http://{addr}")
}

/// Times every kind at every concurrency against `base`.
async fn read_mix(args: &Args, client: &reqwest::Client, base: &str, setup: &str, kinds: &[(&'static str, Vec<Json>)], out: &mut Vec<Json>) {
    let url = format!("{base}/v1/query");
    for (kind, reqs) in kinds {
        if reqs.is_empty() {
            continue;
        }
        let enc = encode(reqs);
        // Warm-up: every body once.
        drive(client, &url, enc.clone(), 1, 0.0, true).await;
        for &c in &args.concurrency {
            let t = drive(client, &url, enc.clone(), c, args.seconds, false).await;
            eprintln!(
                "{setup:<12} {kind:<24} c={c:<3} {:>8.0} q/s  p50 {:>7.3} ms  p99 {:>7.3} ms  server p50 {:>7.3} ms  errors {}",
                t.ms.len() as f64 / t.seconds,
                percentile(&t.ms, 50.0),
                percentile(&t.ms, 99.0),
                percentile(&t.server_ms, 50.0),
                t.errors
            );
            if let Some(e) = &t.first_error {
                eprintln!("  first error: {e}");
            }
            out.push(t.json(setup, kind, c));
        }
    }
}

fn md_table(head: &[&str], rows: &[Vec<String>]) -> String {
    let mut s = format!("| {} |\n|{}|\n", head.join(" | "), head.iter().map(|_| "---").collect::<Vec<_>>().join("|"));
    for r in rows {
        s.push_str(&format!("| {} |\n", r.join(" | ")));
    }
    s
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = Args::parse();
    if let Err(e) = run(args).await {
        eprintln!("strata-server-bench: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let e = |x: strata::Error| x.to_string();
    std::fs::create_dir_all(&args.work).map_err(|x| x.to_string())?;
    let t = Instant::now();
    let coll = data::load(&args.collection, args.limit).map_err(e)?;
    let n = coll.docs.len();
    eprintln!("{}: {n} documents, {} dimensions, loaded in {:.1} s", coll.name, coll.dim, t.elapsed().as_secs_f64());
    let raw_text: u64 = coll.docs.iter().map(|d| (d[col::TITLE].as_str().unwrap_or("").len() + d[col::TEXT].as_str().unwrap_or("").len()) as u64).sum();
    let raw_vectors = (n * coll.dim * 4) as u64;
    let queries: Vec<data::Query> = coll.queries.iter().filter(|q| !q.text.trim().is_empty()).take(args.queries).cloned().collect();
    let keys: Vec<String> = coll.docs.iter().map(|d| d[col::ID].as_str().unwrap_or("").to_string()).collect();

    // 1. Builds.
    let base = args.work.join(&args.collection);
    let sq8 = build(&args, "8-bit codes", &PathBuf::from(format!("{}.sq8.strata", base.display())), VectorCodes::Sq8, coll.dim, coll.docs.clone()).map_err(e)?;
    let bin = build(&args, "1-bit codes", &PathBuf::from(format!("{}.binary.strata", base.display())), VectorCodes::Binary, coll.dim, coll.docs.clone()).map_err(e)?;
    let mut shards = Vec::new();
    for s in 0..args.shards {
        let docs: Vec<Document> = coll.docs.iter().filter(|d| shard_of(&d[col::ID], args.shards) == s).cloned().collect();
        let path = PathBuf::from(format!("{}.shard{s}of{}.strata", base.display(), args.shards));
        shards.push(build(&args, &format!("shard {s} of {}", args.shards), &path, VectorCodes::Sq8, coll.dim, docs).map_err(e)?);
    }

    // 2. Vector recall and latency through SQL, one thread.
    let rows: Vec<&[f32]> = coll
        .docs
        .iter()
        .map(|d| match &d[col::EMB] {
            Value::Vector(v) => &v[..],
            _ => &[][..],
        })
        .collect();
    let qs: Vec<&[f32]> = queries.iter().map(|q| &q.vector[..]).collect();
    let t = Instant::now();
    let truth: Vec<Vec<String>> = exact_top_k(&rows, &qs, 10, None, args.threads).into_iter().map(|h| h.into_iter().map(|h| keys[h.doc as usize].clone()).collect()).collect();
    eprintln!("exact float32 top 10 of {} queries in {:.1} s", qs.len(), t.elapsed().as_secs_f64());
    let mut vectors = Vec::new();
    for (label, b, opts) in [
        ("8-bit codes, HNSW ef 64 (default)", &sq8, ""),
        ("8-bit codes, HNSW ef 320", &sq8, ", ef => 320"),
        ("8-bit codes, exact scan", &sq8, ", strategy => 'flat'"),
        ("1-bit codes, HNSW (breadth 320)", &bin, ""),
        ("1-bit codes, exact scan", &bin, ", strategy => 'flat'"),
    ] {
        let sql = format!("SELECT id FROM docs ORDER BY similarity(emb, :v{opts}) DESC LIMIT 10");
        let params: Vec<Params> = queries.iter().map(|q| Params::new().set("v", Value::Vector(q.vector.clone()))).collect();
        for p in &params {
            b.db.query(&sql, p).map_err(e)?;
        }
        let (mut ms, mut rec) = (Vec::new(), Vec::new());
        for (p, want) in params.iter().zip(&truth) {
            let t = Instant::now();
            let r = b.db.query(&sql, p).map_err(e)?;
            ms.push(t.elapsed().as_secs_f64() * 1e3);
            let got: Vec<String> = r.rows.iter().map(|r| r[0].as_str().unwrap_or("").to_string()).collect();
            rec.push(recall(&got, want));
        }
        eprintln!("{label:<36} recall@10 {:.3}  p50 {:.3} ms  p99 {:.3} ms", mean(&rec), percentile(&ms, 50.0), percentile(&ms, 99.0));
        vectors.push(json!({"config": label, "recall_at_10": mean(&rec), "p50_ms": percentile(&ms, 50.0), "p99_ms": percentile(&ms, 99.0), "mean_ms": mean(&ms)}));
    }
    drop(rows);

    // 3. Reads over HTTP from one database.
    let client = reqwest::Client::builder().pool_max_idle_per_host(1024).build().map_err(|x| x.to_string())?;
    let kinds = bodies(&queries, &keys);
    let one = serve_http(service(Backend::Local(sq8.db.clone()), sq8.db.schema().clone())).await;
    let mut http = Vec::new();
    read_mix(&args, &client, &one, "one database", &kinds, &mut http).await;
    let bin_url = serve_http(service(Backend::Local(bin.db.clone()), bin.db.schema().clone())).await;
    let vector_kinds: Vec<(&'static str, Vec<Json>)> = kinds.iter().filter(|(k, _)| k.starts_with("vector") || k.starts_with("hybrid")).cloned().collect();
    read_mix(&args, &client, &bin_url, "1-bit codes", &vector_kinds, &mut http).await;

    // 4. Writes over HTTP, to a copy of the 8-bit database: the shards are
    // compared with the database as built (and so are later runs with --reuse).
    let mut writes = Vec::new();
    let wpath = PathBuf::from(format!("{}.writes.strata", base.display()));
    let sq8_path = PathBuf::from(format!("{}.sq8.strata", base.display()));
    for (from, to) in [(sq8_path.clone(), wpath.clone()), (wal_path(&sq8_path), wal_path(&wpath))] {
        if to.exists() {
            std::fs::remove_file(&to).map_err(|x| x.to_string())?;
        }
        if from.exists() {
            std::fs::copy(&from, &to).map_err(|x| x.to_string())?;
        }
    }
    let wdb = Arc::new(Db::open(&wpath, serve_opts()).map_err(e)?);
    let wone = serve_http(service(Backend::Local(wdb.clone()), wdb.schema().clone())).await;
    let write_url = format!("{wone}/v1/write");
    let sql_url = format!("{wone}/v1/sql");
    let sets: Vec<Json> = (0..2000).map(|i| json!({"ops": [{"op": "set", "key": keys[(i * 104729) % n], "field": "rating", "value": (i % 50) as f64 / 10.0}]})).collect();
    let updates: Vec<Json> = (0..2000).map(|i| json!({"sql": "UPDATE docs SET rating = :r WHERE id = :k", "params": {"r": (i % 50) as f64 / 10.0, "k": keys[(i * 7907) % n]}})).collect();
    for (kind, url, reqs) in [("in-place update (one per request)", &write_url, &sets), ("SQL UPDATE by key", &sql_url, &updates)] {
        for &c in &args.concurrency {
            let t = drive(&client, url, encode(reqs), c, args.seconds, false).await;
            eprintln!("write {kind:<34} c={c:<3} {:>7.0} writes/s  p50 {:.3} ms  p99 {:.3} ms  errors {}", t.ms.len() as f64 / t.seconds, percentile(&t.ms, 50.0), percentile(&t.ms, 99.0), t.errors);
            writes.push(t.json("one database", kind, c));
        }
    }
    // Inserts: copies of existing documents under new keys, 100 per request.
    let batch = 100;
    let batches: Vec<Json> = (0..20)
        .map(|b| {
            let ops: Vec<Json> = (0..batch)
                .map(|i| {
                    let d = &coll.docs[(b * batch + i) * 31 % n];
                    let doc: serde_json::Map<String, Json> = sq8.db.schema().fields.iter().zip(d).filter(|(f, _)| f.name != "body").map(|(f, v)| (f.name.clone(), strata_server::json::to_json(v))).collect();
                    let mut doc = doc;
                    doc.insert("id".into(), json!(format!("bench-new-{b}-{i}")));
                    json!({"op": "insert", "doc": doc})
                })
                .collect();
            json!({"ops": ops})
        })
        .collect();
    let t = drive(&client, &write_url, encode(&batches), 1, 0.0, true).await;
    eprintln!("write inserts, {batch} per request: {:.0} docs/s, p50 {:.1} ms per request, errors {}", (t.ms.len() * batch) as f64 / t.seconds, percentile(&t.ms, 50.0), t.errors);
    let mut ins = t.json("one database", &format!("insert ({batch} documents per request)"), 1);
    ins["docs_per_s"] = json!((t.ms.len() * batch) as f64 / t.seconds);
    writes.push(ins);
    let deletes: Vec<Json> = (0..20).map(|b| json!({"ops": (0..batch).map(|i| json!({"op": "delete", "key": format!("bench-new-{b}-{i}")})).collect::<Vec<_>>()})).collect();
    drive(&client, &write_url, encode(&deletes), 1, 0.0, true).await;

    // 5. A coordinator over the shards.
    let mut agreement = Vec::new();
    if !shards.is_empty() {
        let mut urls = Vec::new();
        for s in &shards {
            urls.push(serve_grpc(service(Backend::Local(s.db.clone()), s.db.schema().clone())).await);
        }
        let c = Coordinator::connect(urls).await.map_err(|x| x.message)?;
        let schema = c.schema.clone();
        let coord = serve_http(service(Backend::Cluster(Arc::new(c)), schema)).await;
        let setup = format!("{} shards", shards.len());
        read_mix(&args, &client, &coord, &setup, &kinds, &mut http).await;
        for (kind, reqs) in &kinds {
            let (mut same_keys, mut same_scores, mut total) = (0, 0, 0);
            // Vector searches are approximate, on one database and on each
            // shard alike: score both against an exact scan instead.
            let approximate = kind.starts_with("vector") || kind.starts_with("hybrid");
            let (mut found_one, mut found_shards, mut wanted) = (0, 0, 0);
            for body in reqs.iter().take(50) {
                let a: Json = client.post(format!("{one}/v1/query")).json(body).send().await.map_err(|x| x.to_string())?.json().await.map_err(|x| x.to_string())?;
                let b: Json = client.post(format!("{coord}/v1/query")).json(body).send().await.map_err(|x| x.to_string())?.json().await.map_err(|x| x.to_string())?;
                total += 1;
                // Facets compare their counts; ranked kinds their keys in
                // order, and their scores (rows tied on score may swap).
                let field = if *kind == "facet" { "rows" } else { "keys" };
                let keys_equal = a.get("error").is_none() && a[field] == b[field];
                same_keys += keys_equal as usize;
                let scores = |j: &Json| j["scores"].as_array().cloned().unwrap_or_default().iter().map(|s| s.as_f64().unwrap_or(f64::NAN)).collect::<Vec<_>>();
                let (sa, sb) = (scores(&a), scores(&b));
                let scores_equal = sa.len() == sb.len() && sa.iter().zip(&sb).all(|(x, y)| (x - y).abs() <= 1e-5 * x.abs().max(1.0));
                same_scores += (keys_equal || scores_equal) as usize;
                if approximate {
                    let mut exact = body.clone();
                    exact["sql"] = json!(body["sql"].as_str().unwrap().replace("similarity(emb, :v)", "similarity(emb, :v, strategy => 'flat')"));
                    let e: Json = client.post(format!("{one}/v1/query")).json(&exact).send().await.map_err(|x| x.to_string())?.json().await.map_err(|x| x.to_string())?;
                    let keys = |j: &Json| j["keys"].as_array().cloned().unwrap_or_default();
                    let truth = keys(&e);
                    wanted += truth.len();
                    found_one += keys(&a).iter().filter(|k| truth.contains(k)).count();
                    found_shards += keys(&b).iter().filter(|k| truth.contains(k)).count();
                }
            }
            let mut row = json!({"kind": kind, "same_keys": same_keys, "same_scores": same_scores, "of": total});
            if approximate && wanted > 0 {
                row["recall_one"] = json!(found_one as f64 / wanted as f64);
                row["recall_shards"] = json!(found_shards as f64 / wanted as f64);
            }
            agreement.push(row);
        }
    }

    // Report.
    let builds: Vec<Json> = [&sq8, &bin]
        .into_iter()
        .chain(shards.iter())
        .map(|b| {
            json!({
                "name": b.name, "docs": b.docs, "build_s": b.build_s, "file_bytes": b.file_bytes,
                "bytes_per_doc": b.file_bytes as f64 / b.docs.max(1) as f64, "vector_code_bytes": b.code_bytes, "components": b.components,
            })
        })
        .collect();
    let report = json!({
        "machine": machine(),
        "collection": {"name": coll.name, "docs": n, "dim": coll.dim, "queries": queries.len(), "raw_text_bytes": raw_text, "raw_vector_bytes": raw_vectors},
        "params": {"threads_build": args.threads, "concurrency": args.concurrency, "seconds": args.seconds, "shards": args.shards, "index": "HNSW m 16, ef_construction 200", "segments": 1},
        "builds": builds, "vectors": vectors, "http": http, "writes": writes, "shard_agreement": agreement,
    });
    std::fs::create_dir_all(&args.out).map_err(|x| x.to_string())?;
    let path = args.out.join(format!("{}.json", args.collection));
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).map_err(|x| x.to_string())?;
    eprintln!("wrote {}", path.display());

    let mb = |b: u64| format!("{:.1}", b as f64 / 1e6);
    println!("## {} on {}\n", coll.name, report["machine"]["cpu"].as_str().unwrap_or("?"));
    println!("{n} documents; raw text {} MB, raw float32 vectors {} MB.\n", mb(raw_text), mb(raw_vectors));
    let rows: Vec<Vec<String>> = [&sq8, &bin]
        .into_iter()
        .chain(shards.iter())
        .map(|b| vec![b.name.clone(), b.docs.to_string(), mb(b.file_bytes), format!("{:.0}", b.file_bytes as f64 / b.docs.max(1) as f64), mb(b.code_bytes), format!("{:.1}", b.build_s)])
        .collect();
    println!("{}", md_table(&["database", "docs", "file MB", "bytes/doc", "vector codes MB", "build s"], &rows));
    let comp: Vec<Vec<String>> = sq8.components.iter().map(|(k, v)| vec![k.clone(), mb(*v), mb(*bin.components.get(k).unwrap_or(&0))]).collect();
    println!("{}", md_table(&["component", "8-bit MB", "1-bit MB"], &comp));
    let rows: Vec<Vec<String>> = vectors
        .iter()
        .map(|v| vec![v["config"].as_str().unwrap().to_string(), format!("{:.3}", v["recall_at_10"].as_f64().unwrap()), format!("{:.3}", v["p50_ms"].as_f64().unwrap()), format!("{:.3}", v["p99_ms"].as_f64().unwrap())])
        .collect();
    println!("{}", md_table(&["vector search (SQL, one thread)", "recall@10", "p50 ms", "p99 ms"], &rows));
    let rows: Vec<Vec<String>> = http
        .iter()
        .chain(writes.iter())
        .map(|h| {
            let f = |k: &str| h[k].as_f64().unwrap_or(f64::NAN);
            vec![
                h["setup"].as_str().unwrap_or("").to_string(),
                h["kind"].as_str().unwrap_or("").to_string(),
                h["concurrency"].to_string(),
                format!("{:.0}", f("qps")),
                format!("{:.3}", f("p50_ms")),
                format!("{:.3}", f("p99_ms")),
                format!("{:.3}", f("server_p50_ms")),
                h["errors"].to_string(),
            ]
        })
        .collect();
    println!("{}", md_table(&["setup", "kind", "clients", "per s", "p50 ms", "p99 ms", "server p50 ms", "errors"], &rows));
    if !agreement.is_empty() {
        let rows: Vec<Vec<String>> =
            agreement
                .iter()
                .map(|a| {
                    let recall = |k: &str| a.get(k).and_then(|r| r.as_f64()).map(|r| format!("{r:.3}")).unwrap_or_else(|| "exact".into());
                    vec![
                        a["kind"].as_str().unwrap().to_string(),
                        format!("{} of {}", a["same_keys"], a["of"]),
                        format!("{} of {}", a["same_scores"], a["of"]),
                        recall("recall_one"),
                        recall("recall_shards"),
                    ]
                })
                .collect();
        println!(
            "{}",
            md_table(&["kind", "same top 10 as one database", "same scores (ties may swap)", "recall@10, one database", "recall@10, shards"], &rows)
        );
    }
    Ok(())
}
