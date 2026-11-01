//! A compound query stops at its deadline while it joins, on a single
//! server and on a coordinator, where the join runs after the shards have
//! answered its steps: on Strata's operators and on DataFusion, which
//! joins the same rows many times faster and so is given more of them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::exec::Engine;
use strata::storage::DbOptions;
use strata_server::coord::Coordinator;
use strata_server::service::{shard_of, AgentConfig, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

/// The products the operators join, and DataFusion.
const N: usize = 4000;
const N_DATAFUSION: usize = 14_000;

/// Every product meets every product of its category, and no pair passes
/// the rest of the condition.
const JOIN: &str = "SELECT x.id, y.id FROM products x JOIN products y ON x.category = y.category AND x.price < y.price - 1000000";

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: String::new(), base_url: String::new() }
}

fn open(dir: &std::path::Path, name: &str, n: usize, keep: impl Fn(usize) -> bool) -> Arc<Db> {
    let db = Db::create(&dir.join(name), demo::schema(n), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..n).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).unwrap();
    Arc::new(db)
}

/// A server whose compound queries run on `engine`.
fn service(backend: Backend, schema: strata::Schema, engine: Engine) -> Service {
    let mut svc = Service::new(backend, schema, agent(), 16, Duration::from_secs(10));
    svc.engine = engine;
    svc
}

/// The products for `engine` to join.
fn products(engine: Engine) -> usize {
    match engine {
        Engine::Operators => N,
        _ => N_DATAFUSION,
    }
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

/// The status, body and time of the join with a deadline of `timeout_ms`.
async fn join(base: &str, timeout_ms: u64) -> (u16, Json, Duration) {
    let t0 = Instant::now();
    let r = reqwest::Client::new().post(format!("{base}/v1/query")).json(&json!({"sql": JOIN, "timeout_ms": timeout_ms})).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Json::Null), t0.elapsed())
}

/// The join takes a while with time to finish, and stops at a deadline a
/// tenth of that, answering 504.
async fn stops_at_deadline(base: &str) {
    let (status, body, full) = join(base, 60_000).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rows"].as_array().unwrap().len(), 0);
    assert!(full >= Duration::from_millis(200), "the join took {full:?}: too short to stop in the middle");
    let timeout = full / 10;
    let (status, body, took) = join(base, timeout.as_millis() as u64).await;
    assert_eq!(status, 504, "{body}");
    assert!(took < full / 2, "stopped after {took:?} of {full:?}");
}

async fn a_server_stops_a_join(engine: Engine) {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "one.strata", products(engine), |_| true);
    let schema = (*db.schema()).clone();
    let base = http(Arc::new(service(Backend::Local(db), schema, engine))).await;
    stops_at_deadline(&base).await;
}

async fn a_coordinator_stops_a_join(engine: Engine) {
    let dir = tempfile::tempdir().unwrap();
    let n = products(engine);
    let shard = |s: usize| move |i: usize| shard_of(&strata::Value::Str(format!("p{i:07}")), 2) == s;
    let mut shards = Vec::new();
    for s in 0..2 {
        let db = open(dir.path(), &format!("s{s}.strata"), n, shard(s));
        let schema = (*db.schema()).clone();
        shards.push(grpc(Arc::new(service(Backend::Local(db), schema, engine))).await);
    }
    let c = Coordinator::connect(shards).await.unwrap();
    let schema = c.schema.clone();
    let base = http(Arc::new(service(Backend::Cluster(Arc::new(c)), schema, engine))).await;
    stops_at_deadline(&base).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_stops_a_join_at_its_deadline() {
    a_server_stops_a_join(Engine::Operators).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coordinator_stops_a_join_at_its_deadline() {
    a_coordinator_stops_a_join(Engine::Operators).await;
}

#[cfg(feature = "datafusion")]
#[tokio::test(flavor = "multi_thread")]
async fn a_server_stops_a_join_on_datafusion_at_its_deadline() {
    a_server_stops_a_join(Engine::DataFusion).await;
}

#[cfg(feature = "datafusion")]
#[tokio::test(flavor = "multi_thread")]
async fn a_coordinator_stops_a_join_on_datafusion_at_its_deadline() {
    a_coordinator_stops_a_join(Engine::DataFusion).await;
}
