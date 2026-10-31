//! What the command center reads: the queries running now, which an
//! operator cancels; the latest queries with their traces (a
//! coordinator's with a step per shard call); statistics per statement;
//! the series of rates and gauges; the process; and the cluster's copies.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::coord::Coordinator;
use strata_server::service::{shard_of, AgentConfig, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

const N: usize = 4000;

/// Every product meets every product of its category, and no pair passes
/// the rest of the condition: a query that runs a while.
const JOIN: &str = "SELECT x.id, y.id FROM products x JOIN products y ON x.category = y.category AND x.price < y.price - 1000000";

fn open(dir: &std::path::Path, name: &str, keep: impl Fn(usize) -> bool) -> Arc<Db> {
    let db = Db::create(&dir.join(name), demo::schema(N), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).unwrap();
    Arc::new(db)
}

fn service(backend: Backend, schema: strata::Schema) -> Arc<Service> {
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    Arc::new(Service::new(backend, schema, agent, 16, Duration::from_secs(60)))
}

async fn listen() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    (l, url)
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

async fn get(base: &str, path: &str) -> Json {
    let r = reqwest::get(format!("{base}{path}")).await.unwrap();
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

async fn query(base: &str, body: Json) -> (u16, Json) {
    let r = reqwest::Client::new().post(format!("{base}/v1/query")).json(&body).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Json::Null))
}

fn names(trace: &Json) -> Vec<String> {
    trace.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect()
}

/// The join shows in the running queries, an operator cancels it, and it
/// answers 499 at once, counted as cancelled.
async fn cancels_the_join(base: &str) {
    let b = base.to_string();
    let run = tokio::spawn(async move {
        let t0 = Instant::now();
        let r = query(&b, json!({"sql": JOIN, "timeout_ms": 60_000})).await;
        (r, t0.elapsed())
    });
    let id = loop {
        let live = get(base, "/v1/queries").await;
        if let Some(q) = live["queries"].as_array().unwrap().iter().find(|q| q["sql"] == JOIN) {
            assert_eq!(q["kind"], "compound");
            assert!(q["elapsed_ms"].as_f64().unwrap() >= 0.0);
            break q["id"].as_u64().unwrap();
        }
        assert!(!run.is_finished(), "the join ended before it was seen running");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let t0 = Instant::now();
    let r = reqwest::Client::new().delete(format!("{base}/v1/queries/{id}")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let ((status, body), _) = run.await.unwrap();
    assert_eq!(status, 499, "{body}");
    assert!(t0.elapsed() < Duration::from_secs(2), "answered {:?} after the cancel", t0.elapsed());
    assert!(body["error"].as_str().unwrap().contains("cancelled"), "{body}");
    // Not running any more, so not cancelled twice.
    let r = reqwest::Client::new().delete(format!("{base}/v1/queries/{id}")).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let recent = get(base, "/v1/queries/recent?limit=5").await;
    let q = &recent["queries"][0];
    assert_eq!(q["sql"], JOIN, "{recent}");
    assert_eq!(q["outcome"], "cancelled");
    let metrics = reqwest::get(format!("{base}/metrics")).await.unwrap().text().await.unwrap();
    assert!(metrics.contains("strata_queries_total{kind=\"compound\",outcome=\"cancelled\"} 1"), "{metrics}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_lists_times_and_cancels_its_queries() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "one.strata", |_| true);
    let schema = db.schema().clone();
    let svc = service(Backend::Local(db), schema);
    let base = http(svc.clone()).await;
    svc.series.add(svc.sample());

    // Two queries of one shape, and one of another.
    for (sql, t) in [("SELECT id FROM products WHERE MATCH(body, 'laptop') LIMIT 5", 'a'), ("SELECT id FROM products WHERE MATCH(body, 'phone') LIMIT 3", 'a'), ("SELECT COUNT(*) FROM products", 'b')] {
        let (status, body) = query(&base, json!({"sql": sql, "trace": true})).await;
        assert_eq!(status, 200, "{body}");
        let steps = names(&body["trace"]);
        assert_eq!(steps, ["admission", "execute"], "{t}: {body}");
    }
    let stats = get(&base, "/v1/queries/stats").await;
    let shapes = stats["statements"].as_array().unwrap();
    assert_eq!(shapes.len(), 2, "{stats}");
    let matched = shapes.iter().find(|s| s["fingerprint"] == "SELECT id FROM products WHERE MATCH(body, ?) LIMIT ?").unwrap();
    assert_eq!(matched["calls"], 2);
    assert_eq!(matched["kind"], "filter");
    let recent = get(&base, "/v1/queries/recent").await;
    assert_eq!(recent["queries"].as_array().unwrap().len(), 3);
    assert_eq!(recent["queries"][0]["sql"], "SELECT COUNT(*) FROM products");
    assert_eq!(names(&recent["queries"][0]["trace"]), ["admission", "execute"]);
    let id = recent["queries"][0]["request_id"].as_str().unwrap().to_string();
    assert_eq!(get(&base, &format!("/v1/queries/request/{id}")).await["queries"].as_array().unwrap().len(), 1);
    let r = reqwest::Client::new().post(format!("{base}/v1/queries/stats/reset")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(get(&base, "/v1/queries/stats").await["count"], 0);

    // A point of the series: the queries since the first sample.
    svc.series.add(svc.sample());
    let series = get(&base, "/v1/timeseries").await;
    let p = &series["points"][0];
    assert!(p["qps"].as_f64().unwrap() > 0.0, "{p}");
    assert_eq!(p["docs"], N as f64);
    assert!(p["p99_ms"].as_f64().is_some(), "{p}");
    assert!(p["disk_free_bytes"].as_f64().unwrap() > 0.0, "{p}");
    let t = p["t"].as_u64().unwrap();
    assert!(get(&base, &format!("/v1/timeseries?since={t}")).await["points"].as_array().unwrap().is_empty());

    let sys = get(&base, "/v1/system").await;
    assert_eq!(sys["backend"], "local");
    assert_eq!(sys["limits"]["max_concurrent"], 16);
    assert!(sys["db"]["path"].as_str().unwrap().ends_with("one.strata"), "{sys}");
    let cluster = get(&base, "/v1/cluster").await;
    assert_eq!(cluster["role"], "standalone");
    assert_eq!(cluster["node"]["health"]["docs"], N);
    let routes = get(&base, "/v1/routes").await;
    assert!(routes["http"].as_array().unwrap().iter().any(|r| r["route"] == "/v1/query" && r["requests"] == 3), "{routes}");
    assert!(routes["kinds"].as_array().unwrap().iter().any(|k| k["kind"] == "count"), "{routes}");
    // Logs: a test's own subscriber does not fill the ring, but the route answers.
    assert!(get(&base, "/v1/logs?level=warn").await["lines"].is_array());

    cancels_the_join(&base).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coordinator_traces_its_shard_calls_and_shows_its_copies() {
    let dir = tempfile::tempdir().unwrap();
    let shard = |s: usize| move |i: usize| shard_of(&strata::Value::Str(format!("p{i:07}")), 2) == s;
    let mut shards = Vec::new();
    let mut locals = Vec::new();
    for s in 0..2 {
        let db = open(dir.path(), &format!("s{s}.strata"), shard(s));
        let schema = db.schema().clone();
        let svc = service(Backend::Local(db), schema);
        // Each shard has a point to show.
        svc.series.add(svc.sample());
        svc.series.add(svc.sample());
        shards.push(grpc(svc.clone()).await);
        locals.push(svc);
    }
    let c = Coordinator::connect(shards.clone()).await.unwrap();
    let schema = c.schema.clone();
    let base = http(service(Backend::Cluster(Arc::new(c)), schema)).await;

    let (status, body) = query(&base, json!({"sql": "SELECT id FROM products WHERE MATCH(body, 'laptop') ORDER BY bm25(body, 'laptop') DESC LIMIT 5", "trace": true})).await;
    assert_eq!(status, 200, "{body}");
    let trace = body["trace"].as_array().unwrap();
    // Term statistics then the query, from each shard.
    for rpc in ["TermStats", "Query"] {
        let calls: Vec<&Json> = trace.iter().filter(|s| s["name"] == rpc).collect();
        assert_eq!(calls.len(), 2, "{rpc}: {body}");
        let mut on: Vec<u64> = calls.iter().map(|s| s["shard"].as_u64().unwrap()).collect();
        on.sort();
        assert_eq!(on, [0, 1]);
        assert!(calls.iter().all(|s| s["outcome"] == "ok" && shards.contains(&s["replica"].as_str().unwrap().to_string())), "{body}");
    }
    assert_eq!(trace[0]["name"], "admission");
    // Each shard kept the query it ran for the coordinator, under the same id.
    let id = get(&base, "/v1/queries/recent").await["queries"][0]["request_id"].as_str().unwrap().to_string();
    for svc in &locals {
        let ran = svc.query_log.by_request(&id);
        assert_eq!(ran.len(), 1);
        assert!(ran[0].shard);
    }

    let cluster = get(&base, "/v1/cluster").await;
    assert_eq!(cluster["role"], "coordinator");
    assert_eq!(cluster["status"], "ok");
    let ss = cluster["shards"].as_array().unwrap();
    assert_eq!(ss.len(), 2);
    for (i, s) in ss.iter().enumerate() {
        let r = &s["replicas"][0];
        assert_eq!(r["status"], "ok", "{cluster}");
        assert_eq!(r["detail"]["health"]["role"], "shard", "{cluster}");
        assert!(r["detail"]["load"]["docs"].as_f64().unwrap() > 0.0, "{cluster}");
        assert!(r["rtt_ms"].as_f64().is_some());
        assert_eq!(s["calls"]["ok"].as_u64().unwrap(), 2, "{cluster}");
        let slots = s["slots"].as_array().unwrap();
        assert_eq!(slots[0][0], if i == 0 { 0 } else { 2048 });
    }
    cancels_the_join(&base).await;
}
