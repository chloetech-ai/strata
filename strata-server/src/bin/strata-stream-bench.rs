//! Streaming writes over HTTP (`POST /v1/stream`): sustained events per
//! second, and the lag from an event to the moment the server has made its
//! rows visible, on one server and through a coordinator over shards.
//!
//! ```text
//! cd strata-server
//! cargo run --release --bin strata-stream-bench -- --work /path/outside/the/repo
//! ```
//!
//! Each event is a click: a row of `events` and an `add` to its user's
//! count in `users`, with the event's source offset, as a Kafka consumer
//! would send it. Servers run in this process, with the server's own
//! database options (a durable commit per batch, background flushes and
//! merges), so the clients share the machine's cores with them.
//!
//! 1. Throughput: `--events` events as fast as one stream sends them, then
//!    split over four streams of their own sources at once.
//! 2. Lag: one stream sending at each of `--rates` events per second for
//!    `--seconds`, each event stamped with the time it is due. An event's
//!    lag runs from that time to the answer of the batch holding it (the
//!    server answers a batch once its rows are visible), so it includes
//!    the trip back to the client; the server's `strata_stream_lag_seconds`
//!    measures the same from the server's side.
//! 3. Throughput through a coordinator over `--shards` shards (gRPC on
//!    localhost): every shard keeps every batch's offsets.
//!
//! Each run ends by checking that the rows equal the events sent, the
//! users' counts included. Writes `--out/<host>.json` naming the machine,
//! and prints the tables in Markdown.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use futures::StreamExt;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::metrics::percentile;
use strata::query::Params;
use strata::Schema;
use strata_server::auth::Access;
use strata_server::coord::Coordinator;
use strata_server::service::{AgentConfig, Backend, Service};
use strata_server::server;
use tokio::net::TcpListener;

#[derive(Parser)]
struct Args {
    /// Where the databases go (removed afterwards).
    #[arg(long)]
    work: PathBuf,
    #[arg(long, default_value = "results/stream")]
    out: PathBuf,
    /// Events of the throughput runs.
    #[arg(long, default_value_t = 1_000_000)]
    events: usize,
    /// Events per second of the lag runs.
    #[arg(long, value_delimiter = ',', default_value = "1000,10000,50000,100000")]
    rates: Vec<usize>,
    /// Seconds of each lag run.
    #[arg(long, default_value_t = 10)]
    seconds: u64,
    #[arg(long, default_value_t = 3)]
    shards: usize,
    #[arg(long, default_value_t = 10_000)]
    users: usize,
}

const DDL: [&str; 2] = [
    "CREATE TABLE users (id KEYWORD PRIMARY KEY, n INT DEFAULT 0)",
    "CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, ts INT) OPTIONS(shard_by = 'usr')",
];

fn items() -> Schema {
    Schema::from_json(r#"{"table": "items", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "title", "type": "text"}]}"#).unwrap()
}

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

/// A database as the server opens one, with the bench's tables.
fn database(path: &std::path::Path) -> Arc<Db> {
    let db = Db::create(path, items(), server::db_options(false)).unwrap();
    for ddl in DDL {
        db.sql(ddl, &Params::new()).unwrap();
    }
    db.spawn_flusher();
    db.spawn_merger(Default::default());
    Arc::new(db)
}

async fn http(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(svc, l, Access::default()));
    url
}

async fn grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc, l, Access::default()));
    url
}

async fn post(url: &str, path: &str, body: Json) -> Json {
    reqwest::Client::new().post(format!("{url}{path}")).json(&body).send().await.unwrap().json().await.unwrap()
}

async fn add_users(url: &str, users: usize) {
    for lo in (0..users).step_by(1000) {
        let ops: Vec<Json> = (lo..(lo + 1000).min(users)).map(|u| json!({"op": "insert", "table": "users", "doc": {"id": format!("u{u}")}})).collect();
        post(url, "/v1/write", json!({"ops": ops})).await;
    }
}

/// Event `i` of `source` as a line: its row and its user's count.
fn line(source: &str, i: usize, id: usize, users: usize, ts_micros: i64) -> String {
    let usr = format!("u{}", (id.wrapping_mul(2654435761)) % users);
    format!(
        "{}\n",
        json!({
            "source": {"name": source, "offset": i, "ts": strata::timestamp::format(ts_micros)},
            "ops": [
                {"op": "insert", "table": "events", "doc": {"id": id, "usr": usr, "item": format!("i{}", id % 5000), "ts": ts_micros}},
                {"op": "add", "table": "users", "key": usr, "field": "n", "value": 1, "routing": usr},
            ],
        })
    )
}

fn now_micros() -> i64 {
    strata::timestamp::now()
}

/// What a stream's answer said and when each batch's answer came.
struct Answered {
    done: Json,
    /// Lines committed, as each answer arrived.
    acks: Vec<(u64, Instant)>,
}

/// Sends `chunks` (each some lines, sent when the stream asks for it) to
/// `url`'s `/v1/stream`, reading the answers as they come.
async fn stream(url: &str, chunks: impl futures::Stream<Item = String> + Send + 'static) -> Answered {
    let body = reqwest::Body::wrap_stream(chunks.map(Ok::<_, std::io::Error>));
    let r = reqwest::Client::new().post(format!("{url}/v1/stream")).header("content-type", "application/x-ndjson").body(body).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let mut body = r.bytes_stream();
    let (mut buf, mut acks, mut done) = (Vec::new(), Vec::new(), Json::Null);
    while let Some(b) = body.next().await {
        let b = b.unwrap();
        let at = Instant::now();
        buf.extend_from_slice(&b);
        while let Some(nl) = buf.iter().position(|&c| c == b'\n') {
            let j: Json = serde_json::from_slice(&buf[..nl]).unwrap();
            buf.drain(..=nl);
            assert!(j["error"].is_null(), "the stream failed: {j}");
            if j["done"] == true {
                done = j;
            } else {
                acks.push((j["lines"].as_u64().unwrap(), at));
            }
        }
    }
    Answered { done, acks }
}

/// `n` events of `source` from id `base`, in chunks of 1,000 lines, as
/// fast as the stream takes them.
fn flat_out(source: String, base: usize, n: usize, users: usize) -> impl futures::Stream<Item = String> + Send + 'static {
    futures::stream::iter((0..n).step_by(1000)).map(move |lo| (lo..(lo + 1000).min(n)).map(|i| line(&source, i, base + i, users, now_micros())).collect::<String>())
}

/// Events/s of `streams` streams of `per` events each, at once.
async fn throughput(url: &str, streams: usize, per: usize, base: usize, users: usize, tag: &str) -> Json {
    let t0 = Instant::now();
    let runs = (0..streams).map(|s| {
        let url = url.to_string();
        let chunks = flat_out(format!("{tag}/{s}"), base + s * per, per, users);
        tokio::spawn(async move { stream(&url, chunks).await })
    });
    let answers: Vec<Answered> = futures::future::join_all(runs).await.into_iter().map(|r| r.unwrap()).collect();
    let secs = t0.elapsed().as_secs_f64();
    let events = (streams * per) as f64;
    let batches: u64 = answers.iter().map(|a| a.done["batches"].as_u64().unwrap()).sum();
    json!({
        "streams": streams,
        "events": streams * per,
        "seconds": secs,
        "events_per_s": events / secs,
        "operations_per_s": 2.0 * events / secs,
        "batches": batches,
        "events_per_batch": events / batches as f64,
    })
}

/// One stream at `rate` events per second for `seconds`: each event's lag
/// from its due time to its batch's answer.
async fn paced(url: &str, rate: usize, seconds: u64, base: usize, users: usize, source: String) -> Json {
    let n = rate * seconds as usize;
    let t0 = Instant::now();
    let micros0 = now_micros();
    let due = move |i: usize| Duration::from_secs_f64(i as f64 / rate as f64);
    // Every millisecond, the events due by then.
    let chunks = futures::stream::unfold(0usize, move |next| {
        let source = source.clone();
        async move {
            if next >= n {
                return None;
            }
            let mut at = t0 + due(next);
            let now = Instant::now();
            if at > now {
                tokio::time::sleep_until(at.into()).await;
            } else {
                at = now;
            }
            let upto = ((at.duration_since(t0).as_secs_f64() * rate as f64) as usize + 1).clamp(next + 1, n);
            let text: String = (next..upto).map(|i| line(&source, i, base + i, users, micros0 + due(i).as_micros() as i64)).collect();
            Some((text, upto))
        }
    });
    let a = stream(url, chunks).await;
    let mut lags = Vec::with_capacity(n);
    let mut from = 0usize;
    for (lines, at) in &a.acks {
        let to = *lines as usize;
        for i in from..to {
            lags.push(at.duration_since(t0).saturating_sub(due(i)).as_secs_f64() * 1e3);
        }
        from = to;
    }
    assert_eq!(lags.len(), n, "every event answered");
    let sent_s = t0.elapsed().as_secs_f64();
    lags.sort_by(|a, b| a.total_cmp(b));
    json!({
        "rate": rate,
        "events": n,
        "achieved_events_per_s": n as f64 / sent_s,
        "batches": a.done["batches"],
        "lag_ms": {"p50": percentile(&lags, 50.0), "p90": percentile(&lags, 90.0), "p99": percentile(&lags, 99.0), "p999": percentile(&lags, 99.9), "max": lags.last()},
    })
}

/// The events and the users' counts, which must both equal the events sent.
async fn check(url: &str, want: usize) {
    let one = |j: Json| j["rows"][0][0].as_i64().unwrap_or(-1);
    let events = one(post(url, "/v1/sql", json!({"sql": "SELECT COUNT(*) FROM events"})).await);
    let n = one(post(url, "/v1/sql", json!({"sql": "SELECT SUM(n) FROM users"})).await);
    assert_eq!((events, n), (want as i64, want as i64), "rows written: events and the users' counts");
}

/// The server's own lag histogram, as `/metrics` shows it.
async fn server_lag(url: &str) -> Json {
    let m = reqwest::get(format!("{url}/metrics")).await.unwrap().text().await.unwrap();
    let mut buckets = Vec::new();
    let mut count = 0.0;
    for l in m.lines() {
        if let Some(rest) = l.strip_prefix("strata_stream_lag_seconds_bucket{le=\"") {
            let (le, v) = rest.split_once("\"} ").unwrap();
            buckets.push((le.to_string(), v.parse::<f64>().unwrap()));
        } else if let Some(v) = l.strip_prefix("strata_stream_lag_seconds_count ") {
            count = v.parse().unwrap();
        }
    }
    let at = |q: f64| buckets.iter().find(|(_, c)| *c >= q * count).map(|(le, _)| le.clone());
    json!({"events": count, "p50_at_most_s": at(0.5), "p99_at_most_s": at(0.99)})
}

fn table(rows: &[Json], cols: &[(&str, &str)]) -> String {
    let mut s = format!("| {} |\n|{}\n", cols.iter().map(|c| c.0).collect::<Vec<_>>().join(" | "), "---|".repeat(cols.len()));
    for r in rows {
        let cells: Vec<String> = cols
            .iter()
            .map(|(_, path)| {
                let v = path.split('.').fold(r, |v, k| &v[k]);
                match v.as_f64() {
                    Some(f) if f.fract() != 0.0 => format!("{f:.1}"),
                    _ => v.to_string().trim_matches('"').to_string(),
                }
            })
            .collect();
        s += &format!("| {} |\n", cells.join(" | "));
    }
    s
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let a = Args::parse();
    let work = a.work.join(format!("stream-bench-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let machine = machine();
    eprintln!("machine: {machine}");

    // One server.
    let db = database(&work.join("one.strata"));
    let schema = db.schema().clone();
    let url = http(Arc::new(Service::new(Backend::Local(db.clone()), schema, agent(), 64, Duration::from_secs(30)))).await;
    add_users(&url, a.users).await;
    let mut sent = 0;
    let mut runs = Vec::new();
    for streams in [1, 4] {
        let r = throughput(&url, streams, a.events / streams, sent, a.users, &format!("flat{streams}")).await;
        eprintln!("throughput: {r}");
        sent += a.events / streams * streams;
        runs.push(r);
    }
    let mut lags = Vec::new();
    for &rate in &a.rates {
        let r = paced(&url, rate, a.seconds, sent, a.users, format!("paced{rate}/0")).await;
        eprintln!("lag: {r}");
        sent += rate * a.seconds as usize;
        lags.push(r);
    }
    check(&url, sent).await;
    let server = server_lag(&url).await;

    // A coordinator over shards.
    let mut urls = Vec::new();
    for i in 0..a.shards {
        let db = database(&work.join(format!("shard{i}.strata")));
        let schema = db.schema().clone();
        urls.push(grpc(Arc::new(Service::new(Backend::Local(db), schema, agent(), 64, Duration::from_secs(30)))).await);
    }
    let c = Coordinator::connect(urls).await.unwrap();
    let schema = c.schema.clone();
    let curl = http(Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 64, Duration::from_secs(30)))).await;
    add_users(&curl, a.users).await;
    let coordinated = throughput(&curl, 1, a.events, 0, a.users, "coord").await;
    eprintln!("coordinator: {coordinated}");
    check(&curl, a.events).await;

    let out = json!({
        "machine": machine,
        "events_per_run": a.events,
        "users": a.users,
        "throughput": runs,
        "lag": lags,
        "server_lag_histogram": server,
        "coordinator": {"shards": a.shards, "run": coordinated},
    });
    std::fs::create_dir_all(&a.out).unwrap();
    let host = machine["host"].as_str().unwrap_or("machine").to_string();
    let path = a.out.join(format!("{host}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    println!("Machine: {machine}\n");
    println!("{}", table(&runs, &[("streams", "streams"), ("events", "events"), ("events/s", "events_per_s"), ("operations/s", "operations_per_s"), ("events per batch", "events_per_batch")]));
    println!("{}", table(&lags, &[("rate (events/s)", "rate"), ("achieved", "achieved_events_per_s"), ("lag p50 ms", "lag_ms.p50"), ("p90", "lag_ms.p90"), ("p99", "lag_ms.p99"), ("p99.9", "lag_ms.p999"), ("max", "lag_ms.max")]));
    println!("Through a coordinator over {} shards: {:.0} events/s\n", a.shards, coordinated["events_per_s"].as_f64().unwrap());
    println!("Wrote {}", path.display());
    let _ = std::fs::remove_dir_all(&work);
}
