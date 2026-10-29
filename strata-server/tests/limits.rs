//! What one request may ask of a server: rows (`--max-result-window`),
//! groups (`--max-groups`), body size (`--max-body-mb`, 16 MiB on other
//! routes), places in the write queue (`--max-queued-writes`) and memory
//! (`--query-memory-mb`), on a single server and through a coordinator.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::memory::MemoryBudget;
use strata::storage::DbOptions;
use strata_server::coord::Coordinator;
use strata_server::service::{shard_of, AgentConfig, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

const N: usize = 600;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: String::new(), base_url: String::new() }
}

fn open(dir: &std::path::Path, name: &str, keep: impl Fn(usize) -> bool) -> Arc<Db> {
    let db = Db::create(&dir.join(name), demo::schema(N), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).unwrap();
    Arc::new(db)
}

/// A server of `db` that returns at most 100 rows and makes at most 5
/// groups, as `strata-server --max-result-window 100 --max-groups 5`.
fn capped(db: Arc<Db>) -> Service {
    let schema = db.schema().clone();
    let mut s = Service::new(Backend::Local(db), schema, agent(), 16, Duration::from_secs(10));
    s.max_rows = Some(100);
    s.max_groups = Some(5);
    s
}

async fn http(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_http(svc, l, Default::default()));
    url
}

async fn grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_grpc(svc, l, Default::default()));
    url
}

/// The status and body of a query.
async fn query(base: &str, sql: &str) -> (u16, Json) {
    let r = reqwest::Client::new().post(format!("{base}/v1/query")).json(&json!({"sql": sql})).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Json::Null))
}

/// Every query the caps refuse, with what the refusal says, and some they
/// let through with their row counts.
async fn check_caps(base: &str) {
    let ok = [
        ("SELECT id FROM products LIMIT 100", 100),
        ("SELECT id FROM products LIMIT 40 OFFSET 60", 40),
        ("SELECT id FROM products WHERE price > 0 ORDER BY price DESC LIMIT 100", 100),
        ("SELECT COUNT(*) FROM products", 1),
        // A compound query's steps may hold more rows than its answer.
        ("SELECT id FROM products WHERE id IN (SELECT id FROM products WHERE price > 0) LIMIT 7", 7),
    ];
    for (sql, n) in ok {
        let (status, body) = query(base, sql).await;
        assert_eq!(status, 200, "{sql}: {body}");
        assert_eq!(body["rows"].as_array().unwrap().len(), n, "{sql}: {body}");
    }
    let refused = [
        ("SELECT id FROM products LIMIT 101", "reach row 101"),
        ("SELECT id FROM products LIMIT 10 OFFSET 95", "reach row 105"),
        ("SELECT id FROM products", "more than 100 rows"),
        ("SELECT id FROM products WHERE id IN (SELECT id FROM products WHERE price > 0)", "more than 100 rows"),
        // A keyword's facet counters, and document by document; a LIMIT
        // does not lift it.
        ("SELECT brand, COUNT(*) FROM products GROUP BY brand LIMIT 3", "more than 5 groups"),
        ("SELECT category, AVG(price) FROM products GROUP BY category, brand", "more than 5 groups"),
    ];
    for (sql, why) in refused {
        let (status, body) = query(base, sql).await;
        assert_eq!(status, 400, "{sql}: {body}");
        let e = body["error"].as_str().unwrap();
        assert!(e.contains(why), "{sql}: {e}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_refuses_too_many_rows_and_groups() {
    let dir = tempfile::tempdir().unwrap();
    let base = http(Arc::new(capped(open(dir.path(), "one.strata", |_| true)))).await;
    check_caps(&base).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coordinator_and_its_shards_refuse_them_too() {
    let dir = tempfile::tempdir().unwrap();
    let shard = |s: usize| move |i: usize| shard_of(&strata::Value::Str(format!("p{i:07}")), 2) == s;
    // The shards are capped as the coordinator is, and still answer its
    // compound queries' steps.
    let shards = [grpc(Arc::new(capped(open(dir.path(), "s0.strata", shard(0))))).await, grpc(Arc::new(capped(open(dir.path(), "s1.strata", shard(1))))).await];
    let c = Coordinator::connect(shards.to_vec()).await.unwrap();
    let schema = c.schema.clone();
    let mut coord = Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(10));
    coord.max_rows = Some(100);
    coord.max_groups = Some(5);
    let base = http(Arc::new(coord)).await;
    check_caps(&base).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bodies_are_capped_by_route() {
    let dir = tempfile::tempdir().unwrap();
    let mut svc = capped(open(dir.path(), "b.strata", |_| true));
    svc.max_body = 1 << 20;
    let base = http(Arc::new(svc)).await;
    let client = reqwest::Client::new();
    let post = |path: &str, body: String| client.post(format!("{base}{path}")).header("content-type", "application/json").body(body).send();
    // Queries take 16 MiB.
    let pad = "x".repeat(17 << 20);
    let r = post("/v1/query", json!({"sql": "SELECT id FROM products LIMIT 1", "pad": pad}).to_string()).await.unwrap();
    assert_eq!(r.status(), 413);
    let pad = "x".repeat(2 << 20);
    let r = post("/v1/query", json!({"sql": "SELECT id FROM products LIMIT 1", "pad": pad}).to_string()).await.unwrap();
    assert_eq!(r.status(), 200);
    // Writes take --max-body-mb: here 1 MiB.
    let doc = |i: usize, len: usize| json!({"op": "upsert", "doc": {"id": format!("big{i}"), "title": "y".repeat(len)}});
    let r = post("/v1/write", json!({"ops": [doc(0, 1100 << 10)]}).to_string()).await.unwrap();
    assert_eq!(r.status(), 413);
    let r = post("/v1/write", json!({"ops": [doc(1, 900 << 10)]}).to_string()).await.unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_write_queue_refuses_writes_until_it_drains() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "q.strata", |_| true);
    let mut svc = capped(db.clone());
    svc.max_queued_writes = 2;
    let base = http(Arc::new(svc)).await;
    let client = reqwest::Client::new();
    let write = |i: usize| client.post(format!("{base}/v1/write")).json(&json!({"ops": [{"op": "upsert", "doc": {"id": format!("q{i}"), "title": "queued"}}]})).send();
    // A transaction held here keeps the writer waiting.
    let held = db.storage().begin().unwrap();
    let first = tokio::spawn(write(0));
    let second = tokio::spawn(write(1));
    let gauge = || async {
        let m = client.get(format!("{base}/metrics")).send().await.unwrap().text().await.unwrap();
        m.lines().find_map(|l| l.strip_prefix("strata_write_queue_requests ")).unwrap().parse::<f64>().unwrap()
    };
    let t0 = std::time::Instant::now();
    while gauge().await < 2.0 {
        assert!(t0.elapsed() < Duration::from_secs(5), "the writes never queued");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let r = write(2).await.unwrap();
    assert_eq!(r.status(), 429);
    assert!(r.text().await.unwrap().contains("2 writes are queued"));
    let r = client.post(format!("{base}/v1/sql")).json(&json!({"sql": "DELETE FROM products WHERE id = 'p0000001'"})).send().await.unwrap();
    assert_eq!(r.status(), 429);
    drop(held);
    for w in [first, second] {
        assert_eq!(w.await.unwrap().unwrap().status(), 200);
    }
    // Drained: writes go through again.
    assert_eq!(write(3).await.unwrap().status(), 200);
    let m = client.get(format!("{base}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("strata_write_errors_total{status=\"429\"} 2"), "{m}");
}

#[tokio::test(flavor = "multi_thread")]
async fn imports_write_as_they_read() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "i.strata", |_| false);
    let base = http(Arc::new(capped(db.clone()))).await;
    let client = reqwest::Client::new();
    let import = |body: String| client.post(format!("{base}/v1/import?format=jsonl")).header("content-type", "application/x-ndjson").body(body).send();
    let line = |i: usize| json!({"id": format!("r{i}"), "title": "imported", "price": i}).to_string();
    let rows: Vec<String> = (0..2500).map(line).collect();
    let r = import(rows.join("\n")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Json>().await.unwrap()["written"], 2500);
    assert_eq!(db.snapshot().n_live_docs(), 2500);
    // A bad line in the third thousand: nothing is written.
    let mut rows: Vec<String> = (2500..5000).map(line).collect();
    rows[2100] = "not json".into();
    let r = import(rows.join("\n")).await.unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(db.snapshot().n_live_docs(), 2500);
}

/// Rows of every product with their text: about 200 KB counted.
const WIDE: &str = "SELECT id, title, description FROM products LIMIT 100";

async fn metric(base: &str, name: &str) -> f64 {
    let m = reqwest::get(format!("{base}/metrics")).await.unwrap().text().await.unwrap();
    m.lines().find_map(|l| l.strip_prefix(&format!("{name} "))).unwrap_or_else(|| panic!("no {name}")).parse().unwrap()
}

/// A server whose queries may hold 16 KiB together.
fn tight(db: Arc<Db>) -> Service {
    let mut s = capped(db);
    s.query_memory = Some(MemoryBudget::new(16 << 10));
    s
}

/// `sql` is refused with 429, naming the memory budget.
async fn out_of_memory(base: &str, sql: &str) {
    let (status, body) = query(base, sql).await;
    assert_eq!(status, 429, "{sql}: {body}");
    assert!(body["error"].as_str().unwrap().contains("the server lets queries hold"), "{sql}: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn queries_past_the_memory_budget_get_429() {
    let dir = tempfile::tempdir().unwrap();
    let base = http(Arc::new(tight(open(dir.path(), "m.strata", |_| true)))).await;
    out_of_memory(&base, WIDE).await;
    // A compound query's steps count too.
    out_of_memory(&base, "SELECT id FROM products WHERE id IN (SELECT id FROM products WHERE price > 0) LIMIT 5").await;
    let (status, body) = query(&base, "SELECT COUNT(*) FROM products").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(metric(&base, "strata_query_memory_trips_total").await, 2.0);
    assert_eq!(metric(&base, "strata_query_memory_limit_bytes").await, (16 << 10) as f64);
    assert_eq!(metric(&base, "strata_query_memory_bytes").await, 0.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coordinator_passes_on_its_shards_429_and_counts_its_joins() {
    let dir = tempfile::tempdir().unwrap();
    let shard = |s: usize| move |i: usize| shard_of(&strata::Value::Str(format!("p{i:07}")), 2) == s;
    let connect = |shards: Vec<String>| async move {
        let c = Coordinator::connect(shards).await.unwrap();
        let schema = c.schema.clone();
        Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(10))
    };
    // Tight shards: the coordinator answers their 429.
    let shards = vec![grpc(Arc::new(tight(open(dir.path(), "t0.strata", shard(0))))).await, grpc(Arc::new(tight(open(dir.path(), "t1.strata", shard(1))))).await];
    let base = http(Arc::new(connect(shards).await)).await;
    out_of_memory(&base, WIDE).await;
    // Roomy shards and a tight coordinator: the steps' rows it holds for a
    // compound query count against its own budget.
    let shards = vec![grpc(Arc::new(capped(open(dir.path(), "r0.strata", shard(0))))).await, grpc(Arc::new(capped(open(dir.path(), "r1.strata", shard(1))))).await];
    let mut coord = connect(shards).await;
    coord.query_memory = Some(MemoryBudget::new(16 << 10));
    let base = http(Arc::new(coord)).await;
    let (status, body) = query(&base, WIDE).await;
    assert_eq!(status, 200, "{body}");
    out_of_memory(&base, "SELECT x.id, y.title FROM products x JOIN products y ON x.brand = y.brand LIMIT 5").await;
    assert_eq!(metric(&base, "strata_query_memory_trips_total").await, 1.0);
}
