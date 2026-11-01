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
    let schema = (*db.schema()).clone();
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
            let (a, served, w) = svc.query_logged(&Principal::open(), QueryReq { sql: sql.into(), params: params(k), ..Default::default() }, log.clone()).await.unwrap_or_else(|e| panic!("{sql}: {}", e.message));
            let request = served.request;
            assert_eq!(request, format!("r{k}"));
            assert!(served.teams.is_empty());
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

/// Two rankings interleaved by team draft over HTTP: the rows shown are
/// drawn from both, each logged with its own ranking's score and values
/// and its team's name; the same request draws the same list; and the
/// credit endpoint gives each team the requests whose clicks went to it.
#[tokio::test(flavor = "multi_thread")]
async fn two_rankings_interleaved_and_credited() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, db) = local(dir.path(), "il.strata");
    load(&svc).await;
    let h = http(svc.clone()).await;
    let c = reqwest::Client::new();
    let post = |path: &str, body: Json| {
        let (c, url) = (c.clone(), format!("{h}{path}"));
        async move {
            let r = c.post(url).json(&body).send().await.unwrap();
            let status = r.status().as_u16();
            (status, r.json::<Json>().await.unwrap())
        }
    };
    let a_sql = "SELECT id, price FROM items ORDER BY bm25(title, :t) + 0.1 * coalesce(price, 0) DESC LIMIT 8";
    let b_sql = "SELECT id, price FROM items ORDER BY bm25(title, :t) - 0.1 * coalesce(price, 0) DESC LIMIT 8";
    let keys = |a: &Json| a["keys"].as_array().unwrap().iter().map(|k| k.as_str().unwrap().to_string()).collect::<Vec<_>>();
    let mut firsts = std::collections::BTreeSet::new();
    let mut want = [0u64; 2];
    let mut acted = 0;
    for k in 0..30 {
        let t = format!("{} {}", WORDS[k % 12], WORDS[(k * 5 + 3) % 12]);
        let (_, a) = post("/v1/query", json!({"sql": a_sql, "params": {"t": t}})).await;
        let (_, b) = post("/v1/query", json!({"sql": b_sql, "params": {"t": t}})).await;
        let log = json!({"request": format!("r{k}"), "context": format!("u{}", k % 20), "interleave": {"sql": b_sql, "variants": ["bm25", "cheap"]}});
        let (status, il) = post("/v1/query", json!({"sql": a_sql, "params": {"t": t}, "log": log})).await;
        assert_eq!(status, 200, "{il}");
        assert_eq!(il["columns"], a["columns"]);
        let (ka, kb, shown) = (keys(&a), keys(&b), keys(&il));
        let teams: Vec<String> = il["teams"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
        assert_eq!((shown.len(), teams.len()), (8, 8), "{il}");
        assert_eq!(shown.iter().collect::<std::collections::HashSet<_>>().len(), 8, "a row twice: {shown:?}");
        // Each row is its team's best row not shown before it.
        for (i, (key, team)) in shown.iter().zip(&teams).enumerate() {
            let list = if team == "bm25" { &ka } else { &kb };
            assert_eq!(list.iter().find(|x| !shown[..i].contains(x)), Some(key), "row {i} of {shown:?}, {teams:?}");
        }
        let n_a = teams.iter().filter(|t| *t == "bm25").count();
        assert!(n_a.abs_diff(8 - n_a) <= 1, "{teams:?}");
        firsts.insert(teams[0].clone());
        // The rows logged: each with its team, and its ranking's values.
        let rows = logged_rows(&svc, &format!("r{k}")).await;
        assert_eq!(rows.len(), 8);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r[1].as_str().unwrap(), shown[i]);
            let mine = if teams[i] == "bm25" { "bm25(title, :t) + 0.1 * coalesce(price, 0)" } else { "bm25(title, :t) - 0.1 * coalesce(price, 0)" };
            assert!(r[4].get(mine).is_some(), "row {i} of team {} logged {}", teams[i], r[4]);
        }
        // The user clicks the row the second ranking puts first, when shown.
        if let Some(i) = shown.iter().position(|x| *x == kb[0]) {
            let (status, w) = post("/v1/sql", json!({"sql": "UPDATE rank_log SET label = 1 WHERE request = :r AND row_key = :k", "params": {"r": format!("r{k}"), "k": kb[0]}})).await;
            assert_eq!(status, 200, "{w}");
            acted += 1;
            want[(teams[i] == "cheap") as usize] += 1;
        }
    }
    // The coin lets either ranking take first.
    assert_eq!(firsts.len(), 2, "{firsts:?}");
    // The same request draws the same list.
    let log = json!({"request": "r3", "interleave": {"sql": b_sql}});
    let t = format!("{} {}", WORDS[3], WORDS[18 % 12]);
    let (_, x) = post("/v1/query", json!({"sql": a_sql, "params": {"t": t}, "log": log})).await;
    let (_, y) = post("/v1/query", json!({"sql": a_sql, "params": {"t": t}, "log": log})).await;
    assert_eq!((&x["keys"], &x["teams"]), (&y["keys"], &y["teams"]));
    assert!(x["teams"].as_array().unwrap().iter().all(|t| t == "a" || t == "b"), "{x}");
    // That request is relogged under the default names: put it back.
    let log = json!({"request": "r3", "context": "u3", "interleave": {"sql": b_sql, "variants": ["bm25", "cheap"]}});
    let (status, _) = post("/v1/query", json!({"sql": a_sql, "params": {"t": t}, "log": log})).await;
    assert_eq!(status, 200);
    let (_, b3) = post("/v1/query", json!({"sql": b_sql, "params": {"t": t}})).await;
    let k3 = keys(&b3)[0].clone();
    let (status, _) = post("/v1/sql", json!({"sql": "UPDATE rank_log SET label = 1 WHERE request = 'r3' AND row_key = :k", "params": {"k": k3}})).await;
    assert_eq!(status, 200);

    // The credit: requests whose click went to each team.
    let (status, cr) = post("/v1/rank_log/interleaving", json!({})).await;
    assert_eq!(status, 200, "{cr}");
    let pairs = cr["pairs"].as_array().unwrap();
    assert_eq!(pairs.len(), 1, "{cr}");
    let p = &pairs[0];
    assert_eq!(p["variants"], json!(["bm25", "cheap"]));
    assert_eq!(p["requests"], json!(30));
    assert_eq!((p["acted"].as_u64().unwrap(), p["wins"].clone(), p["ties"].clone()), (acted, json!(want), json!(0)), "{cr}");
    assert!((p["p"].as_f64().unwrap() - strata::rank_log::sign_test(want[0], want[1])).abs() < 1e-12);
    let mine = db.interleaving_credit(&Export::default()).unwrap();
    assert_eq!(mine[0].to_json(), *p);

    // Refused: a page past the first, rankings of other columns, a variant.
    for (sql, log) in [
        ("SELECT id, price FROM items ORDER BY bm25(title, :t) DESC LIMIT 8 OFFSET 8", json!({"interleave": {"sql": b_sql}})),
        (a_sql, json!({"interleave": {"sql": "SELECT id FROM items ORDER BY bm25(title, :t) DESC LIMIT 8"}})),
        (a_sql, json!({"variant": "x", "interleave": {"sql": b_sql}})),
    ] {
        let (status, e) = post("/v1/query", json!({"sql": sql, "params": {"t": "red mug"}, "log": log})).await;
        assert_eq!(status, 400, "{sql}: {e}");
    }
}
