//! What logging a query costs ([`strata::rank_log`]): the latency a query
//! with `"log"` adds over the same query without, how many logged queries
//! a server answers per second, how large the log is per row, and how fast
//! the export reads it back with each row's history and label.
//!
//! ```text
//! cd strata-server
//! cargo run --release --bin strata-rank-log-bench -- --work /path/outside/the/repo
//! ```
//!
//! A server runs in this process with the server's database options (a
//! durable commit per write, background flushes and merges) over
//! `--docs` synthetic documents, with an events table of `--events` views
//! by `--users` users. Each request ranks by BM25 times a rating, or by a
//! fusion of BM25 and vector similarity, and serves a user.
//!
//! 1. Latency: `--queries` requests from one client over HTTP, each sent
//!    in turn (so drift hits all alike) plain, with the values a log keeps
//!    added to its columns but not logged, logged, and logged with 100
//!    candidates.
//! 2. Throughput: `--clients` clients sending the same requests, plain,
//!    then logged. Then the rows one request logged, written again
//!    through `/v1/write` alone: what the log's write costs.
//! 3. Size: the bytes of the log table's segments per logged row, after a
//!    flush and merges.
//! 4. Export: every logged row, with its user's last 20 events and a label
//!    from the 30 minutes after.
//!
//! Writes `--out/<host>.json` naming the machine, and prints the tables in
//! Markdown.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{json, Value as Json};
use strata::data::{self, col};
use strata::db::Db;
use strata::metrics::percentile;
use strata::rank_log::Export;
use strata::storage::MergePolicy;
use strata::VectorIndexKind;
use strata_server::auth::Access;
use strata_server::server;
use strata_server::service::{AgentConfig, Backend, Service};
use tokio::net::TcpListener;

#[derive(Parser)]
struct Args {
    /// Where the database goes (removed afterwards).
    #[arg(long)]
    work: PathBuf,
    #[arg(long, default_value = "results/rank_log")]
    out: PathBuf,
    #[arg(long, default_value_t = 100_000)]
    docs: usize,
    #[arg(long, default_value_t = 2_000)]
    users: usize,
    #[arg(long, default_value_t = 200_000)]
    events: usize,
    /// Requests of each kind in the latency run.
    #[arg(long, default_value_t = 2_000)]
    queries: usize,
    #[arg(long, default_value_t = 4)]
    clients: usize,
    /// Commit without syncing the write-ahead log (the server's `--no-sync`):
    /// what the log costs without the disk's flush.
    #[arg(long)]
    no_sync: bool,
}

const DIM: usize = 32;
const QUERIES: [&str; 2] = [
    "SELECT id, title FROM docs ORDER BY bm25(body, :t) * (1 + 0.1 * coalesce(rating, 0)) DESC LIMIT 10",
    "SELECT id, title FROM docs ORDER BY 0.3 * minmax(bm25(body, :t)) + 0.7 * minmax(similarity(emb, :q)) DESC LIMIT 10",
];

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
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
        "host": sh("hostname", &[]),
        "cpu": cpu,
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "memory_gb": mem.map(|b| (b / 1e9 * 10.0).round() / 10.0),
        "os": os,
        "arch": std::env::consts::ARCH,
    })
}

/// A small deterministic generator.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) % n as u64) as usize
    }
}

/// Request `k`: its query and parameters, and the user it serves.
fn request(k: usize, docs: &[strata::Document], g: &mut Lcg, users: usize) -> (Json, String) {
    let d = &docs[g.below(docs.len())];
    let words: Vec<&str> = d[col::TITLE].as_str().unwrap().split(' ').take(3).collect();
    let q: Vec<f32> = match &d[col::EMB] {
        strata::Value::Vector(v) => v.to_vec(),
        _ => unreachable!("synthetic documents have vectors"),
    };
    (json!({"sql": QUERIES[k % 2], "params": {"t": words.join(" "), "q": q}}), format!("u{}", g.below(users)))
}

async fn http(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(svc, l, Access::default()));
    url
}

/// Sends `body` to `/v1/query`: the time it took, in ms, and the rows it
/// logged.
async fn send(c: &reqwest::Client, url: &str, body: &Json) -> (f64, usize) {
    let t = Instant::now();
    let r = c.post(format!("{url}/v1/query")).json(body).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let a: Json = r.json().await.unwrap();
    assert!(a["rows"].as_array().is_some_and(|r| !r.is_empty()), "{a}");
    (t.elapsed().as_secs_f64() * 1e3, a["logged"]["rows"].as_u64().unwrap_or(0) as usize)
}

fn ms(xs: &mut [f64]) -> Json {
    xs.sort_by(|a, b| a.total_cmp(b));
    let r = |x: f64| (x * 1000.0).round() / 1000.0;
    json!({"p50": r(percentile(xs, 50.0)), "p90": r(percentile(xs, 90.0)), "p99": r(percentile(xs, 99.0))})
}

fn section_bytes(db: &Db, table: &str) -> usize {
    let snap = db.snapshot().table(table).unwrap();
    snap.segments().iter().filter(|v| !v.in_memory).flat_map(|v| v.file.section_sizes()).map(|(_, b)| b).sum()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let a = Args::parse();
    std::fs::create_dir_all(&a.work).unwrap();
    let path = a.work.join("rank-log-bench.strata");
    let _ = std::fs::remove_file(&path);
    let t0 = Instant::now();
    let docs = data::synthetic(a.docs, DIM, 7);
    let opts = strata::storage::DbOptions { sync: !a.no_sync, ..server::db_options(false) };
    let db = Db::create(&path, data::beir_schema(DIM, VectorIndexKind::Flat), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, ts TIMESTAMP) OPTIONS(cluster_by = 'usr')", &strata::query::Params::new()).unwrap();
    // Views over the hour before the first request, a second apart.
    let start = strata::timestamp::now();
    let mut g = Lcg(11);
    let mut evs = Vec::with_capacity(a.events);
    for e in 0..a.events {
        let item = docs[g.below(docs.len())][col::ID].clone();
        evs.push(vec![strata::Value::Int(e as i64), strata::Value::Str(format!("u{}", g.below(a.users))), item, strata::Value::Timestamp(start - 3_600_000_000 + (e as i64 * 3_600_000_000 / a.events as i64))]);
    }
    db.load_into("events", evs).unwrap();
    db.spawn_flusher();
    db.spawn_merger(Default::default());
    let db = Arc::new(db);
    let schema = db.schema().clone();
    let svc = Arc::new(Service::new(Backend::Local(db.clone()), schema, agent(), 64, Duration::from_secs(60)));
    let url = http(svc).await;
    eprintln!("loaded {} docs and {} events in {:.1} s", a.docs, a.events, t0.elapsed().as_secs_f64());
    let c = reqwest::Client::new();

    // 1. Latency, one client: plain and logged in turn.
    let mut g = Lcg(5);
    let reqs: Vec<(Json, String)> = (0..a.queries).map(|k| request(k, &docs, &mut g, a.users)).collect();
    for (body, _) in reqs.iter().take(200) {
        send(&c, &url, body).await;
    }
    let (mut plain, mut features, mut logged, mut wide) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut rows_logged = 0usize;
    for (k, (body, user)) in reqs.iter().enumerate() {
        let with = |candidates: Option<usize>| {
            let mut b = body.clone();
            b["log"] = json!({"context": user, "request": format!("lat{k}{}", if candidates.is_some() { "w" } else { "" }), "candidates": candidates});
            b
        };
        plain.push(send(&c, &url, body).await.0);
        // The same query with the values a log keeps, not logged.
        let mut f = body.clone();
        f["sql"] = json!(strata::query::sql::with_features(body["sql"].as_str().unwrap()).unwrap());
        features.push(send(&c, &url, &f).await.0);
        for (lat, candidates) in [(&mut logged, None), (&mut wide, Some(100))] {
            let (ms, n) = send(&c, &url, &with(candidates)).await;
            lat.push(ms);
            rows_logged += n;
        }
    }
    let latency = json!({"requests": a.queries, "plain_ms": ms(&mut plain), "with_features_ms": ms(&mut features), "logged_ms": ms(&mut logged), "logged_100_candidates_ms": ms(&mut wide)});
    eprintln!("latency: {latency}");

    // 2. Throughput: several clients, plain then logged.
    let mut throughput = serde_json::Map::new();
    for mode in ["plain", "logged"] {
        let t = Instant::now();
        let mut tasks = Vec::new();
        for w in 0..a.clients {
            let (c, url) = (c.clone(), url.clone());
            let mine: Vec<(Json, String)> = reqs.iter().skip(w).step_by(a.clients).cloned().collect();
            tasks.push(tokio::spawn(async move {
                let (mut lat, mut rows) = (Vec::new(), 0);
                for (k, (mut body, user)) in mine.into_iter().enumerate() {
                    if mode == "logged" {
                        body["log"] = json!({"context": user, "request": format!("tp{w}-{k}")});
                    }
                    let (ms, n) = send(&c, &url, &body).await;
                    lat.push(ms);
                    rows += n;
                }
                (lat, rows)
            }));
        }
        let mut lat: Vec<f64> = Vec::new();
        for t in tasks {
            let (l, rows) = t.await.unwrap();
            lat.extend(l);
            rows_logged += rows;
        }
        let n = lat.len();
        throughput.insert(mode.into(), json!({"requests": n, "per_s": (n as f64 / t.elapsed().as_secs_f64()).round(), "ms": ms(&mut lat)}));
    }
    eprintln!("throughput: {}", Json::Object(throughput.clone()));

    // What writing ten such rows costs alone: the log's rows of a request
    // written again under new ids, through /v1/write.
    let sample = c.post(format!("{url}/v1/query")).json(&json!({"sql": "SELECT * FROM rank_log WHERE request = 'lat0'"})).send().await.unwrap().json::<Json>().await.unwrap();
    let cols: Vec<String> = sample["columns"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
    let mut writes = Vec::new();
    for k in 0..a.queries / 4 {
        let ops: Vec<Json> = sample["rows"].as_array().unwrap().iter().enumerate().map(|(i, r)| {
            let mut doc: serde_json::Map<String, Json> = cols.iter().cloned().zip(r.as_array().unwrap().iter().cloned()).collect();
            doc.insert("id".into(), json!(format!("w{k}#{i}")));
            doc.insert("request".into(), json!(format!("w{k}")));
            json!({"op": "upsert", "table": "rank_log", "doc": doc})
        }).collect();
        rows_logged += ops.len();
        let t = Instant::now();
        let r = c.post(format!("{url}/v1/write")).json(&json!({"ops": ops})).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
        writes.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let writes = json!({"requests": writes.len(), "rows_each": sample["rows"].as_array().unwrap().len(), "ms": ms(&mut writes)});
    eprintln!("writes alone: {writes}");

    // 3. Size, once flushed and merged.
    db.storage().flush().unwrap();
    while db.storage().merge(&MergePolicy::default()).unwrap().merges > 0 {}
    let count = db.query("SELECT COUNT(*) FROM rank_log", &strata::query::Params::new()).unwrap().rows[0][0].as_i64().unwrap() as usize;
    assert_eq!(count, rows_logged);
    let bytes = section_bytes(&db, "rank_log");
    let size = json!({"rows": count, "bytes": bytes, "bytes_per_row": (bytes as f64 / count as f64 * 10.0).round() / 10.0});
    eprintln!("size: {size}");

    // 4. The export, with history and labels.
    let x = Export::from_json(&json!({"events": {"table": "events", "user": "usr", "item": "item", "time": "ts"}, "history": 20, "window": "30 minutes"})).unwrap();
    let t = Instant::now();
    let mut n = 0u64;
    let mut with_history = 0u64;
    db.export_rank_log(&x, |r| {
        n += 1;
        with_history += r["history_items"].as_array().is_some_and(|h| !h.is_empty()) as u64;
        Ok(())
    })
    .unwrap();
    let secs = t.elapsed().as_secs_f64();
    let export = json!({"records": n, "with_history": with_history, "seconds": (secs * 100.0).round() / 100.0, "records_per_s": (n as f64 / secs).round()});
    eprintln!("export: {export}");

    let m = machine();
    let out = json!({
        "machine": m, "docs": a.docs, "users": a.users, "events": a.events, "clients": a.clients, "sync": !a.no_sync, "queries": QUERIES,
        "latency": latency, "throughput": throughput, "writes_alone": writes, "size": size, "export": export,
    });
    std::fs::create_dir_all(&a.out).unwrap();
    let host = m["host"].as_str().unwrap_or("host").split('.').next().unwrap_or("host").to_string();
    let name = if a.no_sync { format!("{host}-no-sync.json") } else { format!("{host}.json") };
    std::fs::write(a.out.join(name), serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
    println!("\n| one client, ms | p50 | p90 | p99 |\n|---|---|---|---|");
    for (name, k) in [("plain", "plain_ms"), ("with score() and features(), not logged", "with_features_ms"), ("logged", "logged_ms"), ("logged, 100 candidates", "logged_100_candidates_ms")] {
        let l = &out["latency"][k];
        println!("| {name} | {} | {} | {} |", l["p50"], l["p90"], l["p99"]);
    }
    println!("| the log's rows written alone (/v1/write) | {} | {} | {} |", writes["ms"]["p50"], writes["ms"]["p90"], writes["ms"]["p99"]);
    println!("\n{} clients: plain {} requests/s, logged {} requests/s.", a.clients, out["throughput"]["plain"]["per_s"], out["throughput"]["logged"]["per_s"]);
    println!("Log: {} bytes per row over {} rows. Export: {} records/s.", size["bytes_per_row"], count, export["records_per_s"]);
    drop(db);
    let _ = std::fs::remove_file(&path);
}
