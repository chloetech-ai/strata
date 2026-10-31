//! Correlated subqueries through a coordinator: three shards rank what one
//! database ranks. The coordinator runs each subquery once per distinct
//! value of the row it names and sends the values to the shards inside
//! the plan, by the rows' keys, so every shard ranks with the same ones.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::Schema;
use strata_server::auth::Access;
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

fn local(dir: &std::path::Path, name: &str) -> Arc<Service> {
    let db = Arc::new(Db::create(&dir.join(name), items(), DbOptions { segment_max_docs: 100, publish_snapshots: true, ..DbOptions::default() }).unwrap());
    let schema = db.schema().clone();
    Arc::new(Service::new(Backend::Local(db), schema, agent(), 16, Duration::from_secs(20)))
}

async fn grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc, l, Access::default()));
    url
}

/// The items, and events of 20 users on them.
async fn load(svc: &Arc<Service>) {
    let cat = |i: usize| format!("c{}", i % 6);
    let ops: Vec<Json> = (0..ITEMS)
        .map(|i| {
            let price = (i % 9 != 0).then(|| (i * 37 % 97) as f64 + 0.5);
            json!({"op": "upsert", "doc": {"id": format!("i{i}"), "title": format!("{} {} {}", WORDS[i % 12], WORDS[i * 7 % 12], WORDS[i * 5 % 11]), "cat": cat(i), "price": price}})
        })
        .collect();
    svc.write_ops(ops, None).await.unwrap();
    svc.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, cat KEYWORD, price FLOAT, ts INT) OPTIONS(shard_by = 'usr')", &Params::new(), After::Any, 0)
        .await
        .unwrap();
    let ops: Vec<Json> = (0..900)
        .map(|e| {
            let i = e * 13 % ITEMS;
            json!({"op": "insert", "table": "events", "doc": {"id": e, "usr": format!("u{}", e % 20), "item": format!("i{i}"), "cat": cat(i * 7), "price": (e * 31 % 89) as f64, "ts": e * 10}})
        })
        .collect();
    svc.write_ops(ops, None).await.unwrap();
}

const HISTORY: &str = "WITH h AS (SELECT item, cat, price, ts FROM events WHERE usr = :u ORDER BY ts DESC LIMIT 20)";

fn queries() -> Vec<String> {
    vec![
        // A rerank phase, the history in memory.
        format!("{HISTORY}, first AS (SELECT * FROM items ORDER BY bm25(title, :t) DESC LIMIT 40) SELECT id, cat, score() FROM first ORDER BY bm25(title, :t) + 0.5 * (SELECT COUNT(*) FROM h WHERE h.cat = first.cat) DESC LIMIT 8"),
        // A table read through its indexes, a NULL of the row compared.
        "WITH first AS (SELECT * FROM items ORDER BY bm25(title, :t) DESC LIMIT 60) SELECT id, score() FROM first ORDER BY bm25(title, :t) + 0.2 * (SELECT COUNT(*) FROM events e WHERE e.usr = :u AND e.cat = first.cat AND e.price < first.price) DESC LIMIT 10".into(),
        // Two phases, the second correlated, with features.
        format!("{HISTORY}, first AS (SELECT * FROM items ORDER BY bm25(title, :t) DESC LIMIT 50), second AS (SELECT * FROM first ORDER BY bm25(title, :t) * (1 + 0.01 * coalesce(price, 0)) DESC LIMIT 20) SELECT id, score(), features() FROM second ORDER BY bm25(title, :t) + coalesce((SELECT MAX(h.price) FROM h WHERE h.cat = second.cat), 0) * 0.01 DESC LIMIT 6"),
        // An ORDER BY over a WITH result's rows (all of the rows it
        // finds, so that both rank the same ones).
        format!("{HISTORY}, first AS (SELECT * FROM items WHERE cat = 'c2' OR price < 5 LIMIT 200) SELECT id, score() FROM first ORDER BY coalesce(price, 0) - (SELECT COUNT(*) FROM h WHERE h.cat = first.cat) DESC LIMIT 7"),
        // A column of the ranked rows.
        format!("{HISTORY}, first AS (SELECT * FROM items ORDER BY bm25(title, :t) DESC LIMIT 30) SELECT id, (SELECT COUNT(*) FROM h WHERE h.cat = first.cat) AS seen, coalesce(price, 0) + (SELECT MIN(h.price) FROM h WHERE h.cat = first.cat) AS p FROM first ORDER BY bm25(title, :t) DESC LIMIT 9"),
    ]
}

fn params(k: usize) -> Params {
    Params::new()
        .set("t", strata::Value::Str(format!("{} {}", WORDS[k % 12], WORDS[(k * 5 + 3) % 12])))
        .set("u", strata::Value::Str(format!("u{}", k % 20)))
}

#[tokio::test(flavor = "multi_thread")]
async fn three_shards_rank_what_one_database_ranks() {
    let dir = tempfile::tempdir().unwrap();
    let whole = local(dir.path(), "whole.strata");
    let mut urls = Vec::new();
    for s in 0..3 {
        urls.push(grpc(local(dir.path(), &format!("s{s}.strata"))).await);
    }
    let c = Coordinator::connect(urls).await.unwrap();
    let schema = c.schema.clone();
    let coord = Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(20)));
    load(&whole).await;
    load(&coord).await;
    let qs = queries();
    for k in 0..20 {
        let sql = &qs[k % qs.len()];
        let mut answers = Vec::new();
        for svc in [&whole, &coord] {
            let a = svc.query(QueryReq { sql: sql.clone(), params: params(k), ..Default::default() }).await.unwrap_or_else(|e| panic!("{sql}: {}", e.message));
            assert!(a.rs.stats.plan.iter().any(|l| l.starts_with("correlated:")), "{sql}: {:?}", a.rs.stats.plan);
            let j = js::result(&a.rs, 0.0);
            answers.push((j["columns"].clone(), j["rows"].clone()));
        }
        assert!(!answers[0].1.as_array().unwrap().is_empty(), "{sql}");
        assert_eq!(answers[0], answers[1], "{sql}");
    }
}
