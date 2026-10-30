//! Edge tables kept up by a stream (`POST /v1/stream`): what the ten edge
//! operations of each event cost, how large the edge table's graph is, and
//! how a walk over it compares with one over an EDGES field holding the
//! same edges.
//!
//! ```text
//! cd strata-server
//! cargo run --release --bin strata-edges-bench -- --work /path/outside/the/repo
//! ```
//!
//! Each event is a click of user *u* on item *i*: a row of `events` and an
//! `add` to *u*'s count in `users`, with the event's source offset, as a
//! Kafka consumer would send it. With edges, the event also adds 1 to the
//! weight of the edges between *i* and *u*'s last five items, both ways:
//! up to ten `add` operations on `cooc`, an edge table, whose `doc`
//! creates an edge's row on its first event. Items are drawn from a Zipf
//! law, so popular items gather many edges.
//!
//! 1. Writes: the same `--events` events as fast as one stream sends them,
//!    into a database without `cooc` and into one with it. The difference
//!    in time per event is what the edges cost.
//! 2. Size: the bytes of `cooc`'s index (its `edge_table` sections) per
//!    edge, and of the graph of an EDGES field of `items` holding the same
//!    edges, written afterwards.
//! 3. Walks: `ppr` seeded by a user's last 20 items, over `cooc` and over
//!    the EDGES field, for `--queries` users: latency, and whether both
//!    rank the same rows.
//!
//! Servers run in this process with the server's database options (a
//! durable commit per batch, background flushes and merges). Writes
//! `--out/<host>.json` naming the machine, and prints the tables in
//! Markdown.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use futures::StreamExt;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::metrics::percentile;
use strata::query::Params;
use strata::storage::MergePolicy;
use strata::Schema;
use strata_server::auth::Access;
use strata_server::server;
use strata_server::service::{AgentConfig, Backend, Service};
use tokio::net::TcpListener;

#[derive(Parser)]
struct Args {
    /// Where the databases go (removed afterwards).
    #[arg(long)]
    work: PathBuf,
    #[arg(long, default_value = "results/edges")]
    out: PathBuf,
    #[arg(long, default_value_t = 300_000)]
    events: usize,
    #[arg(long, default_value_t = 10_000)]
    users: usize,
    #[arg(long, default_value_t = 50_000)]
    items: usize,
    /// The Zipf exponent of item popularity.
    #[arg(long, default_value_t = 1.0)]
    zipf: f64,
    /// Walks of each kind.
    #[arg(long, default_value_t = 1000)]
    queries: usize,
}

const DDL: [&str; 2] = ["CREATE TABLE users (id KEYWORD PRIMARY KEY, n INT DEFAULT 0)", "CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, ts INT)"];
const COOC: &str = "CREATE TABLE cooc (id KEYWORD PRIMARY KEY, src KEYWORD NOT NULL REFERENCES items, dst KEYWORD NOT NULL REFERENCES items, w FLOAT DEFAULT 0) OPTIONS(edges = 'src, dst, w')";
/// A user's last items that an event links its item to.
const WINDOW: usize = 5;
/// A walk's seeds: a user's last items.
const SEEDS: usize = 20;

fn items() -> Schema {
    Schema::from_json(r#"{"table": "items", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "links", "type": "edges"}]}"#).unwrap()
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

/// A small deterministic generator.
struct Lcg(u64);

impl Lcg {
    fn unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        ((self.unit() * n as f64) as usize).min(n - 1)
    }
}

/// The events: (user, item), items by a Zipf law over their ranks.
fn events(a: &Args) -> Vec<(usize, usize)> {
    let mut cdf: Vec<f64> = (1..=a.items).map(|r| (r as f64).powf(-a.zipf)).collect();
    let total: f64 = cdf.iter().sum();
    let mut acc = 0.0;
    for c in cdf.iter_mut() {
        acc += *c / total;
        *c = acc;
    }
    let mut g = Lcg(42);
    (0..a.events).map(|_| (g.below(a.users), cdf.partition_point(|&c| c < g.unit()).min(a.items - 1))).collect()
}

/// Each event's edges, and every edge's weight at the end.
struct Edges {
    per_event: Vec<Vec<(usize, usize)>>,
    weights: HashMap<(usize, usize), f64>,
    /// Each user's last items, newest last.
    last: HashMap<usize, Vec<usize>>,
}

fn edges(evs: &[(usize, usize)]) -> Edges {
    let mut e = Edges { per_event: Vec::with_capacity(evs.len()), weights: HashMap::new(), last: HashMap::new() };
    for &(u, i) in evs {
        let seen = e.last.entry(u).or_default();
        let mut mine = Vec::new();
        for &j in seen.iter().rev().take(WINDOW).filter(|&&j| j != i) {
            for p in [(i, j), (j, i)] {
                mine.push(p);
                *e.weights.entry(p).or_insert(0.0) += 1.0;
            }
        }
        seen.retain(|&j| j != i);
        seen.push(i);
        if seen.len() > SEEDS {
            seen.remove(0);
        }
        e.per_event.push(mine);
    }
    e
}

/// Event `n` as a line: its row, its user's count and, with `edges`, its
/// edge operations.
fn line(n: usize, (u, i): (usize, usize), edges: Option<&[(usize, usize)]>) -> String {
    let usr = format!("u{u}");
    let mut ops = vec![
        json!({"op": "insert", "table": "events", "doc": {"id": n, "usr": usr, "item": format!("i{i}"), "ts": n}}),
        json!({"op": "add", "table": "users", "key": usr, "field": "n", "value": 1}),
    ];
    for &(a, b) in edges.unwrap_or(&[]) {
        let (a, b) = (format!("i{a}"), format!("i{b}"));
        ops.push(json!({"op": "add", "table": "cooc", "field": "w", "value": 1, "doc": {"id": format!("{a}>{b}"), "src": a, "dst": b}}));
    }
    format!("{}\n", json!({"source": {"name": "clicks/0", "offset": n}, "ops": ops}))
}

fn database(path: &std::path::Path, with_edges: bool, items: usize) -> Arc<Db> {
    let db = Db::create(path, self::items(), server::db_options(false)).unwrap();
    for ddl in DDL.iter().chain(with_edges.then_some(&COOC)) {
        db.sql(ddl, &Params::new()).unwrap();
    }
    let mut txn = db.storage().begin().unwrap();
    for i in 0..items {
        txn.table("items").unwrap().insert(vec![strata::Value::Str(format!("i{i}")), strata::Value::Null]).unwrap();
    }
    txn.commit().unwrap();
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

async fn post(url: &str, path: &str, body: Json) -> Json {
    reqwest::Client::new().post(format!("{url}{path}")).json(&body).send().await.unwrap().json().await.unwrap()
}

/// Sends the events to `/v1/stream` in chunks of 1,000 lines, made as the
/// stream asks for them, as fast as it takes them, and returns its final
/// answer.
async fn stream(url: &str, evs: Arc<Vec<(usize, usize)>>, edges: Option<Arc<Vec<Vec<(usize, usize)>>>>) -> Json {
    let n = evs.len();
    let chunks = futures::stream::iter((0..n).step_by(1000)).map(move |lo| (lo..(lo + 1000).min(n)).map(|i| line(i, evs[i], edges.as_ref().map(|e| e[i].as_slice()))).collect::<String>());
    let body = reqwest::Body::wrap_stream(chunks.map(Ok::<_, std::io::Error>));
    let r = reqwest::Client::new().post(format!("{url}/v1/stream")).header("content-type", "application/x-ndjson").body(body).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let mut body = r.bytes_stream();
    let (mut buf, mut done) = (Vec::new(), Json::Null);
    while let Some(b) = body.next().await {
        buf.extend_from_slice(&b.unwrap());
        while let Some(nl) = buf.iter().position(|&c| c == b'\n') {
            let j: Json = serde_json::from_slice(&buf[..nl]).unwrap();
            buf.drain(..=nl);
            assert!(j["error"].is_null(), "the stream failed: {j}");
            if j["done"] == true {
                done = j;
            }
        }
    }
    done
}

/// Bytes of the sections named `name` in a table's segments.
fn section_bytes(db: &Db, table: &str, name: &str) -> usize {
    let snap = db.snapshot().table(table).unwrap();
    snap.segments().iter().filter(|v| !v.in_memory).flat_map(|v| v.file.section_sizes()).filter(|(n, _)| n == name).map(|(_, b)| b).sum()
}

/// Flushes and merges `db` until one merge pass changes nothing.
fn settle(db: &Db) {
    db.storage().flush().unwrap();
    while db.storage().merge(&MergePolicy::default()).unwrap().merges > 0 {}
}

fn ms(xs: &mut [f64]) -> Json {
    xs.sort_by(|a, b| a.total_cmp(b));
    json!({"p50": percentile(xs, 50.0), "p90": percentile(xs, 90.0), "p99": percentile(xs, 99.0)})
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let a = Args::parse();
    let work = a.work.join(format!("edges-bench-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let machine = machine();
    eprintln!("machine: {machine}");
    let evs = Arc::new(events(&a));
    let mut e = edges(&evs);
    let per_event = Arc::new(std::mem::take(&mut e.per_event));
    let edge_ops: usize = per_event.iter().map(Vec::len).sum();
    eprintln!("{} events, {edge_ops} edge operations, {} edges", evs.len(), e.weights.len());

    // 1. Writes, without and with the edge table.
    let mut writes = Vec::new();
    let mut dbs = Vec::new();
    for with_edges in [false, true] {
        let db = database(&work.join(format!("{with_edges}.strata")), with_edges, a.items);
        let schema = db.schema().clone();
        let url = http(Arc::new(Service::new(Backend::Local(db.clone()), schema, agent(), 64, Duration::from_secs(60)))).await;
        let users: Vec<Json> = (0..a.users).map(|u| json!({"op": "insert", "table": "users", "doc": {"id": format!("u{u}")}})).collect();
        for c in users.chunks(1000) {
            post(&url, "/v1/write", json!({"ops": c})).await;
        }
        let t0 = Instant::now();
        let done = stream(&url, evs.clone(), with_edges.then(|| per_event.clone())).await;
        let secs = t0.elapsed().as_secs_f64();
        let ops = 2 * evs.len() + if with_edges { edge_ops } else { 0 };
        let r = json!({
            "edges": with_edges,
            "events": evs.len(),
            "operations": ops,
            "seconds": secs,
            "events_per_s": evs.len() as f64 / secs,
            "operations_per_s": ops as f64 / secs,
            "us_per_event": secs * 1e6 / evs.len() as f64,
            "batches": done["batches"],
        });
        eprintln!("writes: {r}");
        writes.push(r);
        dbs.push((db, url));
    }
    let (plain, with) = (writes[0]["us_per_event"].as_f64().unwrap(), writes[1]["us_per_event"].as_f64().unwrap());
    let cost = json!({"us_per_event": with - plain, "us_per_edge_operation": (with - plain) * evs.len() as f64 / edge_ops as f64, "edge_operations_per_event": edge_ops as f64 / evs.len() as f64});
    eprintln!("cost: {cost}");

    // The edge table holds every edge with its weight.
    let (db, url) = dbs.pop().unwrap();
    drop(dbs);
    let one = |j: Json| j["rows"][0][0].clone();
    assert_eq!(one(post(&url, "/v1/sql", json!({"sql": "SELECT COUNT(*) FROM cooc"})).await).as_u64(), Some(e.weights.len() as u64));
    assert_eq!(one(post(&url, "/v1/sql", json!({"sql": "SELECT SUM(w) FROM cooc"})).await).as_f64(), Some(edge_ops as f64));

    // 2. Sizes: the edge table's graph, and an EDGES field of the same edges.
    settle(&db);
    let mut out: HashMap<usize, Vec<Json>> = HashMap::new();
    for (&(x, y), &w) in &e.weights {
        out.entry(x).or_default().push(json!([null, format!("i{y}"), w]));
    }
    let ups: Vec<Json> = out.iter().map(|(x, l)| json!({"op": "upsert", "doc": {"id": format!("i{x}"), "links": l}})).collect();
    for c in ups.chunks(2000) {
        post(&url, "/v1/write", json!({"ops": c})).await;
    }
    settle(&db);
    let (table_b, field_b) = (section_bytes(&db, "cooc", "edge_table"), section_bytes(&db, "items", "graph.links"));
    let n = e.weights.len() as f64;
    let sizes = json!({"edges": e.weights.len(), "edge_table_bytes": table_b, "edge_table_bytes_per_edge": table_b as f64 / n, "edges_field_bytes": field_b, "edges_field_bytes_per_edge": field_b as f64 / n,
        "segments": {"cooc": db.snapshot().table("cooc").unwrap().segments().len(), "items": db.snapshot().table("items").unwrap().segments().len()}});
    eprintln!("sizes: {sizes}");

    // 3. Walks from users' last items, over each; each query runs once to
    // warm up, then is timed.
    let mut g = Lcg(9);
    let mut lat: HashMap<&str, Vec<f64>> = HashMap::new();
    let (mut same, mut asked, mut found, mut overlap) = (0usize, 0usize, 0usize, 0.0f64);
    let p = Params::new();
    for _ in 0..a.queries {
        let u = g.below(a.users);
        let Some(seen) = e.last.get(&u) else { continue };
        let seeds = seen.iter().map(|i| format!("'i{i}'")).collect::<Vec<_>>().join(", ");
        let mut tops = Vec::new();
        for over in ["cooc", "links"] {
            let sql = format!("SELECT id, score() FROM items ORDER BY ppr({over}, ({seeds})) DESC LIMIT 200");
            db.query(&sql, &p).unwrap();
            let t = Instant::now();
            let rs = db.query(&sql, &p).unwrap();
            lat.entry(over).or_default().push(t.elapsed().as_secs_f64() * 1e3);
            tops.push(rs.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>());
        }
        asked += 1;
        same += (tops[0] == tops[1]) as usize;
        let mine: std::collections::HashSet<String> = tops[1].iter().map(|k| format!("{k:?}")).collect();
        // A walk from items no edge reaches finds nothing over either.
        if !tops[0].is_empty() {
            found += 1;
            overlap += tops[0].iter().filter(|k| mine.contains(&format!("{k:?}"))).count() as f64 / tops[0].len() as f64;
        }
    }
    let walks = json!({"queries": asked, "seeds": SEEDS, "same_top_200": same, "walks_with_rows": found, "top_200_overlap": overlap / found.max(1) as f64, "edge_table_ms": ms(lat.get_mut("cooc").unwrap()), "edges_field_ms": ms(lat.get_mut("links").unwrap())});
    eprintln!("walks: {walks}");

    let out = json!({"machine": machine, "users": a.users, "items": a.items, "zipf": a.zipf, "window": WINDOW, "writes": writes, "cost": cost, "sizes": sizes, "walks": walks});
    std::fs::create_dir_all(&a.out).unwrap();
    let host = machine["host"].as_str().unwrap_or("machine").to_string();
    let path = a.out.join(format!("{host}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    println!("Machine: {machine}\n");
    println!("| writes | events/s | operations/s | µs per event |\n|---|---|---|---|");
    for w in &writes {
        let name = if w["edges"] == true { "events + edges" } else { "events" };
        println!("| {name} | {:.0} | {:.0} | {:.1} |", w["events_per_s"].as_f64().unwrap(), w["operations_per_s"].as_f64().unwrap(), w["us_per_event"].as_f64().unwrap());
    }
    println!("\nEdges cost {:.1} µs per event, {:.2} µs per edge operation ({:.1} per event).\n", cost["us_per_event"].as_f64().unwrap(), cost["us_per_edge_operation"].as_f64().unwrap(), cost["edge_operations_per_event"].as_f64().unwrap());
    println!("Graph bytes per edge: edge table {:.1}, EDGES field {:.1} ({} edges).\n", sizes["edge_table_bytes_per_edge"].as_f64().unwrap(), sizes["edges_field_bytes_per_edge"].as_f64().unwrap(), e.weights.len());
    println!("| ppr over | p50 ms | p90 | p99 |\n|---|---|---|---|");
    for (name, k) in [("edge table", "edge_table_ms"), ("EDGES field", "edges_field_ms")] {
        println!("| {name} | {:.2} | {:.2} | {:.2} |", walks[k]["p50"].as_f64().unwrap(), walks[k]["p90"].as_f64().unwrap(), walks[k]["p99"].as_f64().unwrap());
    }
    println!("\nThe same top 200 rows, in the same order, in {same} of {asked} walks; {found} found rows, {:.4} of each top 200 in common on average.\nWrote {}", walks["top_200_overlap"].as_f64().unwrap(), path.display());
    drop(db);
    let _ = std::fs::remove_dir_all(&work);
}
