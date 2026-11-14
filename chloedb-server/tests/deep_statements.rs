//! Statements that nest deeply, sent by any key that may read: a long
//! chain of `UNION ALL`, `INTERSECT` or `EXCEPT`, of `||` or `+`, a
//! `GROUP BY` of a long sum. Each once aborted the server with a stack
//! overflow (finding PRF-1). Now each answers, or is refused with a 400
//! that says why, and the server goes on answering, on a single server
//! and on a coordinator. The test's runtime keeps tokio's 2 MiB stacks,
//! which the server itself raises.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use chloedb::db::Db;
use chloedb::storage::DbOptions;
use chloedb_server::coord::Coordinator;
use chloedb_server::service::{shard_of, AgentConfig, Backend, Service};
use chloedb_server::{demo, server};
use tokio::net::TcpListener;

const N: usize = 200;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: String::new(), base_url: String::new() }
}

fn open(dir: &std::path::Path, name: &str, keep: impl Fn(usize) -> bool) -> Arc<Db> {
    let db = Db::create(&dir.join(name), demo::schema(N), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(chloedb::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).unwrap();
    Arc::new(db)
}

fn local(db: Arc<Db>) -> Service {
    let schema = (*db.schema()).clone();
    Service::new(Backend::Local(db), schema, agent(), 16, Duration::from_secs(10))
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

async fn query(base: &str, sql: &str, timeout_ms: u64) -> (u16, Json) {
    let r = chloedb_server::http_client().build().unwrap().post(format!("{base}/v1/query")).json(&json!({"sql": sql, "timeout_ms": timeout_ms})).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Json::Null))
}

fn chain(sep: &str, term: &str, n: usize) -> String {
    vec![term; n].join(sep)
}

/// The statements of the finding, and longer ones of each shape.
fn reproducers() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for n in [125, 250, 300, 1000] {
        out.push((format!("{n} UNION ALL"), chain(" UNION ALL ", "SELECT 1", n)));
        out.push((format!("{n} UNION of a row"), chain(" UNION ALL ", "SELECT id FROM products WHERE id = 'p0000001'", n)));
        out.push((format!("{n} IN (UNION ALL)"), format!("SELECT id FROM products WHERE id IN ({})", chain(" UNION ALL ", "SELECT 'p0000001'", n))));
    }
    for n in [260, 1000] {
        out.push((format!("{n} INTERSECT"), chain(" INTERSECT ", "SELECT 1", n)));
        out.push((format!("{n} EXCEPT"), chain(" EXCEPT ", "SELECT 1", n)));
    }
    for n in [990, 1200] {
        out.push((format!("{n} ||"), format!("SELECT {} FROM products LIMIT 1", chain(" || ", "'a'", n))));
        out.push((format!("{n} || of a column"), format!("SELECT {} FROM products LIMIT 1", chain(" || ", "brand", n))));
        out.push((format!("{n} +"), format!("SELECT {} FROM products LIMIT 1", chain(" + ", "1", n))));
        out.push((format!("{n} + in GROUP BY"), format!("SELECT {} AS k, COUNT(*) FROM products GROUP BY k", chain(" + ", "year", n))));
        out.push((format!("{n} + in ORDER BY"), format!("SELECT id FROM products ORDER BY {} DESC LIMIT 1", chain(" + ", "price", n))));
    }
    out
}

/// Sends each reproducer to `base`, then checks it still answers.
async fn survive(base: &str) {
    for (name, sql) in reproducers() {
        let (status, body) = query(base, &sql, 30_000).await;
        match status {
            200 => {}
            400 => {
                assert_eq!(body["error_code"], "query_invalid", "{name}: {body}");
                // Past the parser's limit, or, through a coordinator, past
                // what a shard decodes of a plan.
                let e = body["error"].as_str().unwrap();
                assert!(e.contains("nests more than") || e.contains("nests too deep to be sent to the shards"), "{name}: {body}");
            }
            s => panic!("{name}: {s} {body}"),
        }
        let (status, body) = query(base, "SELECT COUNT(*) FROM products", 5_000).await;
        assert_eq!(status, 200, "after {name}: {body}");
    }
    // At the limit, a chain of set operations answers its rows.
    let n = (chloedb::query::parse::MAX_EXPR_DEPTH - 1) / chloedb::query::parse::SET_OP_LEVELS + 1;
    let (status, body) = query(base, &chain(" UNION ALL ", "SELECT id FROM products WHERE id = 'p0000001'", n), 30_000).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rows"].as_array().unwrap().len(), n);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deep_statement_does_not_stop_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let base = http(Arc::new(local(open(dir.path(), "d.strata", |_| true)))).await;
    survive(&base).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deep_statement_does_not_stop_a_coordinator() {
    let dir = tempfile::tempdir().unwrap();
    let shard = |s: usize| move |i: usize| shard_of(&chloedb::Value::Str(format!("p{i:07}")), 2) == s;
    let shards = [grpc(Arc::new(local(open(dir.path(), "s0.strata", shard(0))))).await, grpc(Arc::new(local(open(dir.path(), "s1.strata", shard(1))))).await];
    let c = Coordinator::connect(shards.to_vec()).await.unwrap();
    let schema = c.schema.clone();
    let base = http(Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(10)))).await;
    survive(&base).await;
}
