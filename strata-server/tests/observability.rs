//! Request ids, logs and metrics: an id the client sends (or a new one)
//! comes back in the answer, names every log line of the request, and
//! reaches the shards a coordinator asks; slow queries are logged with
//! their SQL; `/metrics` counts requests by route and the coordinator's
//! calls by shard, every family with help and type.

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::coord::Coordinator;
use strata_server::service::{shard_of, AgentConfig, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

const N: usize = 600;

/// Log lines of every test in this binary, as JSON.
fn logs() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let w = buf.clone();
        tracing_subscriber::fmt()
            .with_env_filter("debug,h2=warn,hyper=warn,hyper_util=warn,tower=warn,tonic=warn")
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_writer(move || Shared(w.clone()))
            .init();
        buf
    })
}

struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The log lines whose request has id `id`.
fn lines_of(id: &str) -> Vec<Json> {
    let text = String::from_utf8(logs().lock().unwrap().clone()).unwrap();
    text.lines().filter_map(|l| serde_json::from_str::<Json>(l).ok()).filter(|j| j["span"]["id"] == id).collect()
}

fn local(dir: &std::path::Path, name: &str, keep: impl Fn(usize) -> bool) -> Arc<Service> {
    let db = Db::create(&dir.join(name), demo::schema(N), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent, 16, Duration::from_secs(10)))
}

async fn listen() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    (l, format!("http://127.0.0.1:{port}"))
}

async fn http(svc: Arc<Service>) -> String {
    let (l, url) = listen().await;
    tokio::spawn(server::serve_http(svc, l, Default::default()));
    url
}

async fn grpc(svc: Arc<Service>) -> String {
    let (l, url) = listen().await;
    tokio::spawn(server::serve_grpc(svc, l, Default::default()));
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn ids_reach_the_shards_and_name_the_log_lines() {
    logs();
    let dir = tempfile::tempdir().unwrap();
    let shards = [grpc(local(dir.path(), "s0.strata", |i| shard_of(&strata::Value::Str(format!("p{i:07}")), 2) == 0)).await, grpc(local(dir.path(), "s1.strata", |i| shard_of(&strata::Value::Str(format!("p{i:07}")), 2) == 1)).await];
    let c = Coordinator::connect(shards.to_vec()).await.unwrap();
    let schema = c.schema.clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let coord = Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent, 16, Duration::from_secs(10)));
    let base = http(coord).await;
    let client = reqwest::Client::new();
    let ask = |id: Option<&str>| {
        let mut r = client.post(format!("{base}/v1/query")).json(&json!({"sql": "SELECT id FROM products WHERE MATCH(body, 'laptop') ORDER BY bm25(body, 'laptop') DESC LIMIT 5"}));
        if let Some(id) = id {
            r = r.header("x-request-id", id);
        }
        r.send()
    };

    let r = ask(Some("trace-me-1")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-request-id"], "trace-me-1");
    let lines = lines_of("trace-me-1");
    // The coordinator's request, and each shard's term statistics and query.
    let methods = |m: &str| lines.iter().filter(|l| l["span"]["method"] == m && l["message"] == "answered").count();
    assert_eq!(methods("POST"), 1, "{lines:#?}");
    assert_eq!(methods("TermStats"), 2, "{lines:#?}");
    assert_eq!(methods("Query"), 2, "{lines:#?}");
    assert!(lines.iter().all(|l| l["span"]["name"] == "request"), "{lines:#?}");

    // No id, or one that is not plain: a new one.
    for given in [None, Some("not a plain id")] {
        let r = ask(given).await.unwrap();
        let id = r.headers()["x-request-id"].to_str().unwrap().to_string();
        assert_eq!(id.len(), 16, "{id}");
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        assert!(lines_of(&id).len() >= 5);
    }

    // The coordinator's metrics: requests by route, calls by shard.
    let m = client.get(format!("{base}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("strata_http_requests_total{route=\"/v1/query\",status=\"200\"} 3"), "{m}");
    for s in 0..2 {
        assert!(m.contains(&format!("strata_shard_calls_total{{shard=\"{s}\",outcome=\"ok\"}} 6")), "{m}");
    }
    assert!(m.contains("strata_shard_hedges_total "), "{m}");
    let r = client.get(format!("{base}/no/such/route")).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let m = client.get(format!("{base}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("strata_http_requests_total{route=\"other\",status=\"404\"} 1"), "{m}");
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_queries_and_writes_are_logged_and_counted() {
    logs();
    let dir = tempfile::tempdir().unwrap();
    let svc = local(dir.path(), "slow.strata", |_| true);
    // Every request is slow.
    let mut s = Arc::try_unwrap(svc).ok().unwrap();
    s.slow = Some(Duration::ZERO);
    let base = http(Arc::new(s)).await;
    let client = reqwest::Client::new();
    let sql = "SELECT id FROM products WHERE price > 10 LIMIT 3";
    let r = client.post(format!("{base}/v1/query")).header("x-request-id", "slow-1").json(&json!({"sql": sql})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = client.post(format!("{base}/v1/write")).header("x-request-id", "slow-2").json(&json!({"ops": [{"op": "upsert", "doc": {"id": "new", "title": "a new product"}}]})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let q = lines_of("slow-1");
    let slow = q.iter().find(|l| l["message"] == "slow query").unwrap_or_else(|| panic!("{q:#?}"));
    assert_eq!((slow["level"].as_str(), slow["sql"].as_str(), slow["kind"].as_str(), slow["rows"].as_u64()), (Some("WARN"), Some(sql), Some("filter"), Some(3)));
    let w = lines_of("slow-2");
    let slow = w.iter().find(|l| l["message"] == "slow write").unwrap_or_else(|| panic!("{w:#?}"));
    assert_eq!(slow["rows"].as_u64(), Some(1));

    let m = client.get(format!("{base}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("strata_slow_requests_total{what=\"query\"} 1") && m.contains("strata_slow_requests_total{what=\"write\"} 1"), "{m}");
    assert!(m.contains("strata_write_duration_seconds_count 1") && m.contains("strata_query_duration_seconds_count{kind=\"filter\"} 1"), "{m}");
    assert!(m.contains("strata_write_queue_requests 0") && m.contains("strata_write_batches_total 1"), "{m}");
    assert!(m.contains("strata_docs 601") && m.contains("strata_memtable_docs "), "{m}");
    // Every series follows its family's help and type.
    let mut family = String::new();
    for line in m.lines() {
        if let Some(rest) = line.strip_prefix("# HELP ") {
            family = rest.split(' ').next().unwrap().to_string();
            assert!(rest.len() > family.len() + 5, "{line}");
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            assert_eq!(rest.split(' ').next().unwrap(), family);
        } else {
            assert!(line.starts_with(&family), "{line} is not in {family}");
        }
    }
    assert!(!m.contains("_ms"), "durations are in seconds: {m}");
}
