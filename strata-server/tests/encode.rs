//! `encode(model, rows)` through a coordinator: it opens the sequence model
//! from its own models directory, runs the history and the items' vectors
//! over the shards, and sends the vector to them inside the plan, so three
//! shards rank what one database ranks. Shards need no model.

use std::path::Path;
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

fn items() -> Schema {
    Schema::from_json(r#"{"table": "items", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "cat", "type": "keyword"}, {"name": "emb", "type": "vector", "dim": 8, "index": "flat"}]}"#).unwrap()
}

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn models() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../strata/tests/models")
}

fn local(dir: &Path, name: &str, with_models: bool) -> Arc<Service> {
    let db = Arc::new(Db::create(&dir.join(name), items(), DbOptions { segment_max_docs: 20, publish_snapshots: true, ..DbOptions::default() }).unwrap());
    let schema = db.schema().clone();
    let mut svc = Service::new(Backend::Local(db), schema, agent(), 16, Duration::from_secs(20));
    if with_models {
        svc.open_models(&models());
    }
    Arc::new(svc)
}

async fn grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc, l, Access::default()));
    url
}

/// 60 items with vectors, and 300 events of 10 users on them.
async fn load(svc: &Arc<Service>) {
    let ops: Vec<Json> = (0..60)
        .map(|i| {
            // Distinct vectors, so that no two items tie.
            let emb: Vec<f32> = (0..8u64).map(|d| (((i as u64 * 8 + d).wrapping_mul(0x9e3779b97f4a7c15) >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0).collect();
            json!({"op": "upsert", "doc": {"id": format!("i{i}"), "cat": format!("c{}", i % 4), "emb": emb}})
        })
        .collect();
    svc.write_ops(ops, None).await.unwrap();
    svc.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, action KEYWORD, ts INT) OPTIONS(shard_by = 'usr')", &Params::new(), After::Any, 0).await.unwrap();
    let ops: Vec<Json> = (0..300)
        .map(|e| json!({"op": "insert", "table": "events", "doc": {"id": e, "usr": format!("u{}", e % 10), "item": format!("i{}", e * 13 % 60), "action": ACTIONS[e % 3], "ts": (1_700_000_000 + e * 37) * 1_000_000}}))
        .collect();
    svc.write_ops(ops, None).await.unwrap();
}

const ACTIONS: [&str; 3] = ["view", "buy", "other"];

const HISTORY: &str = "(SELECT item, action, ts FROM events WHERE usr = :u ORDER BY ts DESC LIMIT 200)";

#[tokio::test(flavor = "multi_thread")]
async fn three_shards_rank_what_one_database_ranks() {
    let dir = tempfile::tempdir().unwrap();
    let whole = local(dir.path(), "whole.strata", true);
    let mut urls = Vec::new();
    for s in 0..3 {
        urls.push(grpc(local(dir.path(), &format!("s{s}.strata"), false)).await);
    }
    let c = Coordinator::connect(urls).await.unwrap();
    let schema = c.schema.clone();
    let mut coord = Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(20));
    coord.open_models(&models());
    let coord = Arc::new(coord);
    load(&whole).await;
    load(&coord).await;
    let queries = [
        format!("SELECT id, score() FROM items ORDER BY similarity(emb, encode('seq_small', {HISTORY}, now => 1700020000000000)) DESC LIMIT 8"),
        format!("SELECT id, score() FROM items WHERE cat = 'c1' ORDER BY maxsim(emb, encode('seq_small', {HISTORY}, now => 1700020000000000)) DESC LIMIT 5"),
        format!("SELECT id, dot(emb, encode('seq_small', {HISTORY}, now => 1700020000000000)) AS s FROM items ORDER BY s DESC, id LIMIT 6"),
    ];
    for k in 0..12 {
        let sql = &queries[k % queries.len()];
        let p = Params::new().set("u", strata::Value::Str(format!("u{k}")));
        let mut answers = Vec::new();
        for svc in [&whole, &coord] {
            let a = svc.query(QueryReq { sql: sql.clone(), params: p.clone(), ..Default::default() }).await.unwrap_or_else(|e| panic!("{sql}: {}", e.message));
            assert!(a.rs.stats.plan.iter().any(|l| l.starts_with("encode: seq_small over")), "{sql}: {:?}", a.rs.stats.plan);
            let j = js::result(&a.rs, 0.0);
            answers.push((j["columns"].clone(), j["rows"].clone()));
        }
        // u10 and u11 have no events: no vector, so no rows by similarity,
        // and NULL for dot().
        let rows = answers[0].1.as_array().unwrap();
        match k % queries.len() {
            2 => assert_eq!(rows.iter().all(|r| r[1].is_null()), k >= 10, "{sql} u{k}"),
            _ => assert_eq!(rows.is_empty(), k >= 10, "{sql} u{k}"),
        }
        assert_eq!(answers[0], answers[1], "{sql}");
    }
}
