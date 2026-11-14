//! Statement budgets on a server ([`chloedb::query::budget`]): the
//! server's `--statement-budget`, a key's budget and a request's
//! `budget`, the tightest holding, through `/v1/query`, `/v1/sql`,
//! `/v1/explain` and `/v1/sql/check` (audit 4, X-B3 and X-B12).

use std::sync::Arc;
use std::time::Duration;

use chloedb::db::Db;
use chloedb::query::budget::Budget;
use chloedb::storage::DbOptions;
use chloedb_server::auth::{Access, Secret};
use chloedb_server::keys::Keys;
use chloedb_server::service::{AgentConfig, Backend, Service};
use chloedb_server::{demo, server};
use serde_json::{json, Value as Json};
use tokio::net::TcpListener;

const N: usize = 3_000;

fn service(dir: &std::path::Path) -> Service {
    let db = Db::create(&dir.join("b.strata"), demo::schema(N), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(chloedb::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).map(|i| demo::product(i, 7))).unwrap();
    db.sql("CREATE FUNCTION wide(x FLOAT) RETURNS FLOAT AS :x + :x + :x + :x + :x + :x + :x + :x", &chloedb::query::Params::new()).unwrap();
    db.sql("CREATE FUNCTION wider(x FLOAT) RETURNS FLOAT AS wide(:x) + wide(:x) + wide(:x) + wide(:x)", &chloedb::query::Params::new()).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent, 16, Duration::from_secs(10));
    svc.keys = Arc::new(Keys::open(dir.join("b.strata.keys.json")).unwrap());
    svc
}

async fn call(base: &str, path: &str, auth: &str, body: Json) -> (u16, Json) {
    let c = chloedb_server::http_client().build().unwrap();
    let r = c.post(format!("{base}{path}")).bearer_auth(auth).json(&body).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Json::Null))
}

fn says(j: &Json, what: &str) -> bool {
    j["error"].as_str().is_some_and(|e| e.contains(what))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_statement_is_held_to_the_tightest_of_the_servers_its_keys_and_its_requests_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut svc = service(dir.path());
    svc.statement_budget = Budget::parse("depth=200").unwrap();
    let svc = Arc::new(svc);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_http(svc.clone(), l, Access { token: Some(Secret::new("root")), ..Default::default() }));
    // A read key that may read 2,000 rows a statement and write out 30
    // expressions.
    let (s, j) = call(&base, "/v1/admin/keys", "root", json!({"name": "dash", "scope": "read", "budget": {"nodes": 30, "rows_read": 2000}})).await;
    assert_eq!(s, 200, "{j}");
    assert_eq!(j["key"]["budget"], json!({"nodes": 30, "rows_read": 2000}));
    let (dash, id) = (j["secret"].as_str().unwrap().to_string(), j["key"]["id"].as_str().unwrap().to_string());
    let (s, j) = call(&base, "/v1/admin/keys", "root", json!({"name": "typo", "scope": "read", "budget": {"rows": 5}})).await;
    assert_eq!(s, 400, "a budget it does not know: {j}");

    let wider = json!({"sql": "SELECT wider(price) AS v FROM products WHERE id = 'p0000003'"});
    let scan = json!({"sql": "SELECT COUNT(*) FROM products WHERE price + 1 > 0"});
    // The root holds the server's budget only.
    for path in ["/v1/query", "/v1/sql"] {
        assert_eq!(call(&base, path, "root", wider.clone()).await.0, 200, "{path}");
        let (s, j) = call(&base, path, "root", scan.clone()).await;
        assert_eq!((s, j["rows"][0][0].as_u64()), (200, Some(N as u64)), "{path}: {j}");
    }
    // The key: its nodes written out, its rows read.
    for path in ["/v1/query", "/v1/sql", "/v1/explain"] {
        let (s, j) = call(&base, path, &dash, wider.clone()).await;
        assert!(s == 400 && says(&j, "(nodes;") && says(&j, "written out"), "{path}: {s} {j}");
    }
    for path in ["/v1/query", "/v1/sql"] {
        let (s, j) = call(&base, path, &dash, scan.clone()).await;
        assert!(s == 400 && says(&j, "(rows_read;"), "{path}: {s} {j}");
    }
    let (s, j) = call(&base, "/v1/sql/check", &dash, json!({"sql": "SELECT wider(price) FROM products"})).await;
    assert!(s == 200 && j["runs"] == false && says(&j, "(nodes;"), "check: {s} {j}");
    // A request may lower its key's budget, not raise it.
    let few = json!({"sql": "SELECT id FROM products WHERE id IN (:ids)", "params": {"ids": ["p0000001", "p0000002", "p0000003"]}});
    assert_eq!(call(&base, "/v1/query", &dash, few.clone()).await.0, 200);
    let mut lower = few.clone();
    lower["budget"] = json!({"list": 2});
    let (s, j) = call(&base, "/v1/query", &dash, lower).await;
    assert!(s == 400 && says(&j, "list parameter holds 3"), "{s} {j}");
    let mut raise = scan.clone();
    raise["budget"] = json!({"rows_read": 1_000_000});
    let (s, j) = call(&base, "/v1/query", &dash, raise).await;
    assert!(s == 400 && says(&j, "(rows_read;"), "a request does not raise its key's budget: {s} {j}");
    // The root's request may ask for less too.
    let mut lower = scan.clone();
    lower["budget"] = json!("rows_read=100");
    let (s, j) = call(&base, "/v1/sql", "root", lower).await;
    assert!(s == 400 && says(&j, "(rows_read;"), "{s} {j}");
    // The key's budget is lowered, never raised.
    let (s, j) = call(&base, &format!("/v1/admin/keys/{id}/budget"), "root", json!({"rows_read": 1_000_000, "list": 2})).await;
    assert_eq!((s, &j["budget"]), (200, &json!({"nodes": 30, "list": 2, "rows_read": 2000})), "{j}");
    let (s, j) = call(&base, "/v1/query", &dash, few).await;
    assert!(s == 400 && says(&j, "(list;"), "{s} {j}");
    // The server's budget holds every key: 200 levels.
    let deep = format!("SELECT COUNT(*) FROM products WHERE price > 0{}", " + 1".repeat(250));
    let (s, j) = call(&base, "/v1/query", "root", json!({"sql": deep})).await;
    assert!(s == 400 && says(&j, "(depth;"), "{s} {j}");
}

/// A short statement with a long list parameter plans off the request's
/// thread and stops at its deadline (X-B3); `/v1/explain` plans under it
/// too (X-B12).
#[tokio::test(flavor = "multi_thread")]
async fn a_long_list_plans_under_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let svc = Arc::new(service(dir.path()));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_http(svc.clone(), l, Access { token: Some(Secret::new("root")), ..Default::default() }));
    let tags: Vec<String> = (0..100_000).map(|i| format!("t{i}")).collect();
    for path in ["/v1/query", "/v1/explain"] {
        let body = json!({"sql": "SELECT COUNT(*) FROM products WHERE tags @> :tags", "params": {"tags": tags}, "budget": {"plan_ms": 1}});
        let (s, j) = call(&base, path, "root", body).await;
        assert!(s != 200, "{path}: {j}");
        assert!(says(&j, "plan_ms") || says(&j, "deadline"), "{path}: {s} {j}");
    }
    // Under its budget it answers.
    let (s, j) = call(&base, "/v1/query", "root", json!({"sql": "SELECT COUNT(*) FROM products WHERE tags @> :tags", "params": {"tags": &tags[..50]}})).await;
    assert_eq!(s, 200, "{j}");
}
