//! Strata as a key-value store over the server: rows read by key without
//! SQL (`POST /v1/kv/get`, `GET`, `PUT` and `DELETE /v1/kv/:table/:key`,
//! gRPC `Get`) from one server and through a coordinator, which asks each
//! key's shard; rows that expire at their `expire_by` time, which the
//! server's expirer deletes.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata_server::auth::Access;
use strata_server::coord::Coordinator;
use strata_server::keys::Principal;
use strata_server::service::{AgentConfig, After, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

const TABLE: &str = "CREATE TABLE kv (id KEYWORD PRIMARY KEY, n INT, note TEXT, until TIMESTAMP) OPTIONS(expire_by = 'until')";

fn create(dir: &Path, name: &str) -> Arc<Db> {
    let opts = DbOptions { memtable_chunk_docs: 16, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), demo::schema(10), opts).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    Arc::new(db)
}

fn service(backend: Backend, schema: strata::schema::Schema) -> Arc<Service> {
    Arc::new(Service::new(backend, schema, agent(), 16, Duration::from_secs(10)))
}

async fn grpc(db: Arc<Db>) -> String {
    let schema = (*db.schema()).clone();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(service(Backend::Local(db), schema), l, Access::default()));
    url
}

async fn http(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(svc, l, Access::default()));
    url
}

async fn run(svc: &Arc<Service>, s: &str) {
    svc.sql(&Principal::open(), s, &Params::new(), After::Any, 0).await.unwrap_or_else(|e| panic!("{s}: {}", e.message));
}

fn key(i: usize) -> String {
    format!("u{i:03}")
}

/// Every key's row by `POST /v1/kv/get`, by `GET /v1/kv/kv/:key` and by
/// SQL agree, from `url`.
async fn check(client: &reqwest::Client, url: &str, n: usize, gone: &[usize]) {
    let keys: Vec<Json> = (0..n + 3).map(|i| json!(key(i))).collect();
    let got: Json = client.post(format!("{url}/v1/kv/get")).json(&json!({"table": "kv", "keys": keys})).send().await.unwrap().json().await.unwrap();
    assert_eq!(got["columns"], json!(["id", "n", "note", "until"]), "{got}");
    let rows = got["rows"].as_array().unwrap();
    assert_eq!(rows.len(), n + 3);
    for (i, r) in rows.iter().enumerate() {
        let want = i < n && !gone.contains(&i);
        assert_eq!(!r.is_null(), want, "key {}: {r}", key(i));
        if want {
            assert_eq!(r["id"], json!(key(i)));
            assert_eq!(r["n"], json!(i * 10));
        }
    }
    for i in [0, 1, n - 1, n + 1] {
        let r = client.get(format!("{url}/v1/kv/kv/{}", key(i))).send().await.unwrap();
        let found = i < n && !gone.contains(&i);
        assert_eq!(r.status().as_u16(), if found { 200 } else { 404 }, "GET {}", key(i));
        let j: Json = r.json().await.unwrap();
        assert_eq!(j["found"], json!(found));
        if found {
            assert_eq!(j["row"]["n"], json!(i * 10));
            let sql: Json = client.post(format!("{url}/v1/query")).json(&json!({"sql": format!("SELECT * FROM kv WHERE id = '{}'", key(i))})).send().await.unwrap().json().await.unwrap();
            let cols: Vec<String> = sql["columns"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
            let row = &sql["rows"][0];
            for (c, v) in cols.iter().zip(row.as_array().unwrap()) {
                assert_eq!(&j["row"][c], v, "{c} of {}", key(i));
            }
        }
    }
}

/// Reads and writes by key from one server and through a coordinator of
/// two shards answer alike, and rows that have expired are none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rows_by_key_from_a_server_and_a_coordinator() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let urls = vec![grpc(create(d, "s0.strata")).await, grpc(create(d, "s1.strata")).await];
    let c = Arc::new(Coordinator::connect(urls).await.unwrap());
    let schema = c.schema.clone();
    let coord = service(Backend::Cluster(c), schema);
    let one_db = create(d, "one.strata");
    let one = service(Backend::Local(one_db.clone()), (*one_db.schema()).clone());
    let n = 60;
    for svc in [&coord, &one] {
        run(svc, TABLE).await;
        let values: Vec<String> = (0..n).map(|i| format!("('{}', {}, 'row {i}', {})", key(i), i * 10, if i % 9 == 4 { "'2001-01-01T00:00:00Z'" } else { "NULL" })).collect();
        run(svc, &format!("INSERT INTO kv (id, n, note, until) VALUES {}", values.join(", "))).await;
    }
    let expired: Vec<usize> = (0..n).filter(|i| i % 9 == 4).collect();
    let client = reqwest::Client::new();
    let (cu, ou) = (http(coord.clone()).await, http(one.clone()).await);
    for url in [&cu, &ou] {
        check(&client, url, n, &expired).await;
    }
    // PUT writes a row in place of any with its key, DELETE deletes one.
    for url in [&cu, &ou] {
        let r: Json = client.put(format!("{url}/v1/kv/kv/{}", key(1))).json(&json!({"n": 999, "note": "put"})).send().await.unwrap().json().await.unwrap();
        assert_eq!(r["written"], json!(1), "{r}");
        let r: Json = client.put(format!("{url}/v1/kv/kv/{}", key(n + 5))).json(&json!({"id": key(n + 5), "n": 5})).send().await.unwrap().json().await.unwrap();
        assert_eq!(r["written"], json!(1), "{r}");
        let bad = client.put(format!("{url}/v1/kv/kv/{}", key(2))).json(&json!({"id": "other"})).send().await.unwrap();
        assert_eq!(bad.status().as_u16(), 400);
        let r: Json = client.delete(format!("{url}/v1/kv/kv/{}", key(3))).send().await.unwrap().json().await.unwrap();
        assert_eq!(r["deleted"], json!(true), "{r}");
        let r: Json = client.delete(format!("{url}/v1/kv/kv/{}", key(3))).send().await.unwrap().json().await.unwrap();
        assert_eq!(r["deleted"], json!(false), "{r}");
        let got: Json = client.post(format!("{url}/v1/kv/get")).json(&json!({"table": "kv", "keys": [key(1), key(3), key(n + 5)]})).send().await.unwrap().json().await.unwrap();
        assert_eq!(got["rows"][0]["n"], json!(999));
        assert_eq!(got["rows"][0]["note"], json!("put"));
        assert!(got["rows"][1].is_null());
        assert_eq!(got["rows"][2]["n"], json!(5));
    }
    // Keys of the wrong type, a table without a key, too many keys.
    let e = coord.get_rows(Some("kv"), vec![Value::Int(3)], &After::Any, 0).await.unwrap_err();
    assert!(e.message.contains("KEYWORD"), "{}", e.message);
    // The expirer deletes the expired rows on the server; they were none
    // to reads already.
    let before = one_db.documents(&one_db.snapshot().table("kv").unwrap()).count();
    assert_eq!(one.expire_due().unwrap(), expired.len() as u64);
    assert_eq!(one_db.documents(&one_db.snapshot().table("kv").unwrap()).count(), before - expired.len());
    assert_eq!(coord.expire_due().unwrap(), 0, "a coordinator holds no rows");
}
