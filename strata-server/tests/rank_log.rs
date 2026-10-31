//! The rank log over HTTP and through a coordinator: a query with `"log"`
//! answers as it would without, and writes what it served, with the
//! values its ranking read, to a log table made by the first such query
//! (on every shard through a coordinator, whose rows are placed by their
//! keys). Three shards log what one database logs. A local server exports
//! the log as NDJSON, the records `Db::export_rank_log` gives.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::Params;
use strata::rank_log::Export;
use strata::storage::DbOptions;
use strata::Schema;
use strata_server::auth::Access;
use strata_server::keys::Principal;
use strata_server::coord::Coordinator;
use strata_server::service::{After, AgentConfig, Backend, QueryReq, Service};
use strata_server::{json as js, server};
use tokio::net::TcpListener;

const ITEMS: usize = 400;
const WORDS: [&str; 12] = ["red", "blue", "tent", "lamp", "boot", "cord", "mug", "kettle", "chair", "rope", "map", "stove"];

fn items() -> Schema {
    Schema::from_json(
        r#"{"table": "items", "key": "id", "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "title", "type": "text"},
            {"name": "cat", "type": "keyword"},
            {"name": "price", "type": "float"}]}"#,
    )
    .unwrap()
}

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn local(dir: &std::path::Path, name: &str) -> (Arc<Service>, Arc<Db>) {
    let db = Arc::new(Db::create(&dir.join(name), items(), DbOptions { segment_max_docs: 100, publish_snapshots: true, ..DbOptions::default() }).unwrap());
    let schema = db.schema().clone();
    (Arc::new(Service::new(Backend::Local(db.clone()), schema, agent(), 16, Duration::from_secs(20))), db)
}

async fn grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc, l, Access::default()));
    url
}

async fn http(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(svc, l, Access::default()));
    url
}

/// The items, and events of 20 users on them.
async fn load(svc: &Arc<Service>) {
    let ops: Vec<Json> = (0..ITEMS)
        .map(|i| json!({"op": "upsert", "doc": {"id": format!("i{i}"), "title": format!("{} {} {}", WORDS[i % 12], WORDS[i * 7 % 12], WORDS[i * 5 % 11]), "cat": format!("c{}", i % 5), "price": (i * 37 % 97) as f64 + 0.5}}))
        .collect();
    svc.write_ops(ops, None).await.unwrap();
    svc.sql(&Principal::open(), "CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, action KEYWORD, ts INT) OPTIONS(shard_by = 'usr')", &Params::new(), After::Any, 0).await.unwrap();
    let ops: Vec<Json> = (0..600)
        .map(|e| json!({"op": "insert", "table": "events", "doc": {"id": e, "usr": format!("u{}", e % 20), "item": format!("i{}", e * 13 % ITEMS), "action": (["view", "click"][e % 2]), "ts": e * 10}}))
        .collect();
    svc.write_ops(ops, None).await.unwrap();
}

const QUERIES: [&str; 3] = [
    "SELECT id, title FROM items WHERE MATCH(title, :t) ORDER BY bm25(title, :t) * (1 + 0.01 * price) DESC LIMIT 5",
    "SELECT id FROM items ORDER BY bm25(title, :t) + 0.1 * coalesce(price, 0) DESC LIMIT 8",
    "WITH a AS (SELECT * FROM items ORDER BY bm25(title, :t) DESC LIMIT 30) SELECT id, price FROM a ORDER BY bm25(title, :t) - 0.02 * price DESC LIMIT 4",
];

fn params(k: usize) -> Params {
    Params::new().set("t", strata::Value::Str(format!("{} {}", WORDS[k % 12], WORDS[(k * 5 + 3) % 12])))
}

/// The log's rows of a request: position, row key, shown, score and
/// features (as JSON text parsed).
async fn logged_rows(svc: &Arc<Service>, request: &str) -> Vec<Json> {
    let p = Params::new().set("r", strata::Value::Str(request.into()));
    let a = svc.query(QueryReq { sql: "SELECT position, row_key, shown, score, features, context FROM rank_log WHERE request = :r LIMIT 1000".into(), params: p, ..Default::default() }).await.unwrap();
    let mut rows: Vec<Json> = js::result(&a.rs, 0.0)["rows"].as_array().unwrap().clone();
    rows.sort_by_key(|r| r[0].as_i64().unwrap());
    for r in rows.iter_mut() {
        r[4] = serde_json::from_str(r[4].as_str().unwrap()).unwrap();
    }
    rows
}

#[tokio::test(flavor = "multi_thread")]
async fn three_shards_log_what_one_database_logs() {
    let dir = tempfile::tempdir().unwrap();
    let (whole, _) = local(dir.path(), "whole.strata");
    let mut urls = Vec::new();
    for s in 0..3 {
        urls.push(grpc(local(dir.path(), &format!("s{s}.strata")).0).await);
    }
    let c = Coordinator::connect(urls).await.unwrap();
    let schema = c.schema.clone();
    let coord = Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(20)));
    load(&whole).await;
    load(&coord).await;
    let mut n = 0;
    for k in 0..12 {
        let sql = QUERIES[k % QUERIES.len()];
        let log = strata::rank_log::Log { request: Some(format!("r{k}")), context: Some(format!("u{}", k % 4)), candidates: (k % 3 == 1).then_some(12), ..Default::default() };
        let mut answers = Vec::new();
        for svc in [&whole, &coord] {
            let plain = svc.query(QueryReq { sql: sql.into(), params: params(k), ..Default::default() }).await.unwrap();
            let (a, request, w) = svc.query_logged(&Principal::open(), QueryReq { sql: sql.into(), params: params(k), ..Default::default() }, log.clone()).await.unwrap_or_else(|e| panic!("{sql}: {}", e.message));
            assert_eq!(request, format!("r{k}"));
            assert_eq!((&a.rs.columns, &a.rs.rows, &a.rs.keys), (&plain.rs.columns, &plain.rs.rows, &plain.rs.keys), "{sql}");
            let rows = logged_rows(svc, &request).await;
            assert_eq!(w.rows as usize, rows.len());
            assert!(rows.len() >= plain.rs.rows.len() && !rows.is_empty(), "{sql}");
            answers.push(rows);
        }
        // The same rows, scores and features, bit for bit.
        assert_eq!(answers[0], answers[1], "{sql}");
        n += answers[0].len();
    }
    // The coordinator made the table on every shard and spread the rows.
    let count = |svc: Arc<Service>| async move { js::result(&svc.query(QueryReq { sql: "SELECT COUNT(*) FROM rank_log".into(), ..Default::default() }).await.unwrap().rs, 0.0)["rows"][0][0].as_u64().unwrap() as usize };
    assert_eq!((count(whole.clone()).await, count(coord.clone()).await), (n, n));
    // A shard's query is not logged, and a coordinator does not export.
    let h = http(coord.clone()).await;
    let r = reqwest::Client::new().post(format!("{h}/v1/rank_log/export")).json(&json!({})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let r = reqwest::Client::new().post(format!("{h}/v1/query")).json(&json!({"sql": QUERIES[0], "params": {"t": "red mug"}, "shard": true, "log": true})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_logged_query_over_http_and_its_export() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, db) = local(dir.path(), "one.strata");
    load(&svc).await;
    let h = http(svc.clone()).await;
    let c = reqwest::Client::new();
    let post = |path: &str, body: Json| {
        let (c, url) = (c.clone(), format!("{h}{path}"));
        async move {
            let r = c.post(url).json(&body).send().await.unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    };
    let mut served = 0;
    for k in 0..6 {
        let route = if k % 2 == 0 { "/v1/query" } else { "/v1/sql" };
        let log = match k {
            0 => json!(true),
            _ => json!({"context": format!("u{k}"), "variant": "a", "ts": k * 1000, "candidates": 10}),
        };
        let (status, plain) = post(route, json!({"sql": QUERIES[1], "params": {"t": "red mug"}})).await;
        assert_eq!(status, 200, "{plain}");
        let plain: Json = serde_json::from_str(&plain).unwrap();
        let (status, a) = post(route, json!({"sql": QUERIES[1], "params": {"t": "red mug"}, "log": log})).await;
        assert_eq!(status, 200, "{a}");
        let a: Json = serde_json::from_str(&a).unwrap();
        assert_eq!((&a["columns"], &a["rows"]), (&plain["columns"], &plain["rows"]));
        assert!(a["request"].as_str().is_some_and(|r| !r.is_empty()), "{a}");
        let logged = a["logged"]["rows"].as_u64().unwrap();
        assert_eq!(logged, if k == 0 { 8 } else { 10 });
        served += logged as usize;
    }
    let (status, a) = post("/v1/query", json!({"sql": "SELECT id FROM items WHERE price > 3", "log": true})).await;
    assert_eq!(status, 400, "{a}");
    let (status, a) = post("/v1/query", json!({"sql": QUERIES[1], "params": {"t": "red"}, "log": {"contxt": "u1"}})).await;
    assert!(status == 400 && a.contains("contxt"), "{a}");

    // The export: NDJSON, the records the database itself exports.
    let x = json!({"events": {"table": "events", "user": "usr", "item": "item", "time": "ts", "action": "action"}, "history": 3, "window": 2});
    let (status, text) = post("/v1/rank_log/export", x.clone()).await;
    assert_eq!(status, 200, "{text}");
    let records: Vec<Json> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(records.len(), served);
    let mut want = Vec::new();
    db.export_rank_log(&Export::from_json(&x).unwrap(), |r| Ok(want.push(r))).unwrap();
    assert_eq!(records, want);
    // Rows logged with a context have their history: that user's events
    // before the row's time (ts is microseconds here).
    let r = records.iter().find(|r| r["context"] == json!("u3")).unwrap();
    let times: Vec<i64> = r["history_times"].as_array().unwrap().iter().map(|t| t.as_i64().unwrap()).collect();
    assert_eq!(times, [2430, 2630, 2830]);
    assert_eq!(records.iter().filter(|r| r["shown"] == json!(false)).count(), 5 * 2);
    let (status, text) = post("/v1/rank_log/export", json!({"table": "items"})).await;
    assert!(status == 400 && text.contains("not a log table"), "{text}");
}
