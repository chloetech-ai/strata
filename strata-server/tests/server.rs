//! The server answers like the embedded database: over HTTP, and as a
//! coordinator over shard servers reached by gRPC. Writes over HTTP and
//! through a coordinator, the token, and the agent's tool loop against a
//! stand-in for the Messages API.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata_server::service::{shard_of, AgentConfig, Backend, QueryReq, Service};
use strata_server::{coord::Coordinator, demo, json as js, server};
use tokio::net::TcpListener;

const N: usize = 3000;

fn agent_config(base_url: &str) -> AgentConfig {
    AgentConfig { api_key: Some("test-key".into()), model: strata_server::agent::DEFAULT_MODEL.into(), base_url: base_url.into() }
}

fn create(dir: &std::path::Path, name: &str, keep: impl Fn(usize) -> bool) -> Db {
    let opts = DbOptions { segment_max_docs: 700, publish_snapshots: true, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), demo::schema(N), opts).unwrap();
    db.load((0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).unwrap();
    db
}

fn service(db: Db, base_url: &str) -> Arc<Service> {
    let schema = db.schema().clone();
    Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent_config(base_url), 16, Duration::from_secs(10)))
}

async fn http(svc: Arc<Service>, token: Option<&str>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let token = token.map(String::from);
    tokio::spawn(server::serve_http(svc, l, token));
    format!("http://{addr}")
}

async fn grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(server::serve_grpc(svc, l));
    format!("http://{addr}")
}

async fn post(c: &reqwest::Client, url: &str, body: Json) -> (u16, Json) {
    let r = c.post(url).json(&body).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Json::Null))
}

/// Rows equal, in order, except that rows tied on score may swap and
/// floats may differ in the last digits.
fn same(sql: &str, want: &Json, got: &Json) {
    let (wr, gr) = (want["rows"].as_array().unwrap(), got["rows"].as_array().unwrap());
    assert_eq!(wr.len(), gr.len(), "{sql}: row counts");
    assert_eq!(want["columns"], got["columns"], "{sql}");
    let score = |j: &Json, i: usize| j["scores"].get(i).and_then(|s| s.as_f64());
    let mut i = 0;
    while i < wr.len() {
        let mut j = i + 1;
        while j < wr.len() && score(want, j).zip(score(want, i)).is_some_and(|(a, b)| (a - b).abs() < 1e-9) {
            j += 1;
        }
        let key = |r: &Json| r[0].to_string();
        let mut a: Vec<String> = wr[i..j].iter().map(key).collect();
        let mut b: Vec<String> = gr[i..j].iter().map(key).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b, "{sql}: rows {i}..{j}");
        if j == i + 1 {
            for (u, v) in wr[i].as_array().unwrap().iter().zip(gr[i].as_array().unwrap()) {
                match (u.as_f64(), v.as_f64()) {
                    (Some(p), Some(q)) => assert!((p - q).abs() < 1e-6 * p.abs().max(1.0), "{sql}: {p} vs {q}"),
                    _ => assert_eq!(u, v, "{sql}: row {i}"),
                }
            }
        }
        i = j;
    }
}

fn queries() -> Vec<(&'static str, Json)> {
    let like = json!({"q": {"vector_of": "p0000042"}});
    let none = json!({});
    vec![
        ("SELECT id, title, score() FROM products ORDER BY bm25(body, 'waterproof hiking boots') DESC LIMIT 10", none.clone()),
        ("SELECT id, price, score() FROM products WHERE price < 150 AND category IN ('outdoor', 'sports') ORDER BY bm25(body, 'lightweight') DESC LIMIT 20", none.clone()),
        ("SELECT id, score() FROM products ORDER BY bm25(body, 'quiet kettle') DESC LIMIT 5 OFFSET 4", none.clone()),
        ("SELECT id, score() FROM products ORDER BY similarity(emb, :q) DESC LIMIT 10", like.clone()),
        ("SELECT id, score() FROM products WHERE rating >= 4 ORDER BY similarity(emb, :q) DESC LIMIT 10", like.clone()),
        ("SELECT id, score() FROM products ORDER BY rrf(bm25(body, 'espresso'), similarity(emb, :q)) DESC LIMIT 10", like.clone()),
        ("SELECT id, score() FROM products WHERE year >= 2020 ORDER BY 0.4 * bm25(body, 'recycled') + 0.6 * similarity(emb, :q) DESC LIMIT 15", like.clone()),
        ("SELECT id, price FROM products WHERE tags = 'bestseller' ORDER BY price ASC, id LIMIT 12", none.clone()),
        ("SELECT id, tags FROM products WHERE tags IN ('sale', 'new') AND MATCH(body, '\"cast iron\"') ORDER BY sold DESC, id LIMIT 10", none.clone()),
        ("SELECT brand, COUNT(*) FROM products WHERE MATCH(body, 'waterproof') GROUP BY brand ORDER BY 2 DESC, 1 LIMIT 8", none.clone()),
        ("SELECT tags, COUNT(*) FROM products GROUP BY tags ORDER BY 2 DESC, 1 LIMIT 10", none.clone()),
        ("SELECT COUNT(*) FROM products WHERE MATCH(body, 'leather OR suede') AND price BETWEEN 20 AND 300", none.clone()),
        ("SELECT id, year, score() FROM products ORDER BY bm25(body, 'waterproof jacket') * decay(year, 2026, 2) DESC LIMIT 10", none.clone()),
        ("SELECT id, price, score() FROM products WHERE category = 'kitchen' ORDER BY decay(price, 50, 20, shape => 'gauss') DESC LIMIT 10", none.clone()),
        ("SELECT id, score() FROM products ORDER BY similarity(emb, :q) * decay(rating, 5, 1) DESC LIMIT 10", like.clone()),
    ]
}

/// HTTP answers equal the embedded database's.
#[tokio::test(flavor = "multi_thread")]
async fn http_answers_like_the_embedded_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = create(dir.path(), "p.strata", |_| true);
    let mut want = Vec::new();
    for (sql, params) in queries() {
        let p = if params["q"].is_object() { Params::new().set("q", demo::product(42, 7)[12].clone()) } else { Params::new() };
        want.push(js::result(&db.query(sql, &p).unwrap(), 0.0));
    }
    let base = http(service(db, "http://unused"), None).await;
    let c = reqwest::Client::new();
    for ((sql, params), want) in queries().into_iter().zip(&want) {
        let (status, got) = post(&c, &format!("{base}/v1/query"), json!({"sql": sql, "params": params})).await;
        assert_eq!(status, 200, "{sql}: {got}");
        same(sql, want, &got);
    }
    let (status, e) = post(&c, &format!("{base}/v1/query"), json!({"sql": "SELECT nope FROM products"})).await;
    assert_eq!(status, 400, "{e}");
    assert!(e["error"].as_str().unwrap().contains("nope"));
    let health: Json = c.get(format!("{base}/v1/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!(health["docs"], N);
    let metrics = c.get(format!("{base}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains("strata_queries_total{kind=\"bm25\",outcome=\"ok\"} 3"), "{metrics}");
    assert!(metrics.contains("strata_queries_total{kind=\"invalid\",outcome=\"error\"} 1"), "{metrics}");
}

/// Writes over HTTP: operations, SQL, counters, versions; then reads.
#[tokio::test(flavor = "multi_thread")]
async fn writes_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let base = http(service(create(dir.path(), "w.strata", |i| i < 500), "http://unused"), None).await;
    let c = reqwest::Client::new();
    let get = |id: &'static str| {
        let c = c.clone();
        let base = base.clone();
        async move {
            let (_, r) = post(&c, &format!("{base}/v1/query"), json!({"sql": "SELECT id, price, sold, tags, reviews FROM products WHERE id = :k", "params": {"k": id}})).await;
            r["rows"].get(0).cloned()
        }
    };
    let sold0 = get("p0000003").await.unwrap()[2].as_i64().unwrap();
    let doc = json!({"id": "n1", "title": "Test kettle", "description": "A quiet kettle", "brand": "Hearthly", "category": "kitchen", "tags": ["quiet", "new"], "price": 20.5, "rating": 4.0, "reviews": 3, "sold": 0, "year": 2026});
    let ops = json!({"ops": [
        {"op": "insert", "doc": doc},
        {"op": "add", "key": "p0000003", "field": "sold", "value": 5},
        {"op": "add", "key": "p0000003", "field": "sold", "value": 2},
        {"op": "set", "key": "p0000003", "field": "price", "value": 9.99},
        {"op": "upsert_if_newer", "doc": {"id": "n1", "reviews": 2, "price": 1.0}, "version": "reviews"},
        {"op": "delete", "key": "p0000004"},
    ]});
    let (status, r) = post(&c, &format!("{base}/v1/write"), ops).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["written"], 5, "the older version is skipped");
    let p3 = get("p0000003").await.unwrap();
    assert_eq!(p3[1], 9.99);
    assert_eq!(p3[2].as_i64().unwrap(), sold0 + 7);
    assert_eq!(get("n1").await.unwrap()[3], json!(["quiet", "new"]));
    assert!(get("p0000004").await.is_none());

    // SQL writes.
    let sql_url = format!("{base}/v1/sql");
    let sql = |s: &str| post(&c, &sql_url, json!({"sql": s}));
    let (_, r) = sql("UPDATE products SET sold = sold + 1 WHERE id = 'p0000003'").await;
    assert_eq!(r["written"], 1);
    let (_, r) = sql("INSERT OR REPLACE INTO products (id, title, price, tags, reviews) VALUES ('n1', 'Kettle v2', 25.0, ARRAY['steel'], 9)").await;
    assert_eq!(r["written"], 1, "{r}");
    let n1 = get("n1").await.unwrap();
    assert_eq!((n1[1].clone(), n1[3].clone(), n1[4].clone()), (json!(25.0), json!(["steel"]), json!(9)));
    let (_, r) = sql("DELETE FROM products WHERE category = 'kitchen' AND price > 300").await;
    assert!(r["written"].as_u64().unwrap() > 0, "{r}");
    let (_, r) = sql("SELECT COUNT(*) FROM products WHERE category = 'kitchen' AND price > 300").await;
    assert_eq!(r["rows"][0][0], 0);
    assert_eq!(get("p0000003").await.unwrap()[2].as_i64().unwrap(), sold0 + 8);

    // Refused: a bad op, an unknown field.
    let (status, _) = post(&c, &format!("{base}/v1/write"), json!({"ops": [{"op": "frobnicate"}]})).await;
    assert_eq!(status, 400);
    let (status, _) = post(&c, &format!("{base}/v1/write"), json!({"ops": [{"op": "insert", "doc": {"id": "x", "nope": 1}}]})).await;
    assert_eq!(status, 400);
}

/// Concurrent writers, and merges committing beside them, all get through:
/// a write waits for the writer instead of failing as busy.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writes_wait_for_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(create(dir.path(), "c.strata", |i| i < 500));
    let schema = db.schema().clone();
    let svc = Arc::new(Service::new(Backend::Local(db.clone()), schema, agent_config("http://unused"), 16, Duration::from_secs(30)));
    let base = http(svc, None).await;
    let c = reqwest::Client::new();
    let sold = |r: &Json| r["rows"][0][0].as_i64().unwrap();
    let count = json!({"sql": "SELECT sold FROM products WHERE id = 'p0000003'"});
    let sold0 = sold(&post(&c, &format!("{base}/v1/query"), count.clone()).await.1);

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let maintenance = {
        let (db, stop) = (db.clone(), stop.clone());
        std::thread::spawn(move || {
            let policy = strata::storage::MergePolicy { merge_factor: 2, floor_docs: 100_000, ..Default::default() };
            while !stop.load(Ordering::Relaxed) {
                let _ = db.storage().flush();
                let _ = db.storage().merge(&policy);
            }
        })
    };
    let writers: Vec<_> = (0..12)
        .map(|w| {
            let (c, base) = (c.clone(), base.clone());
            tokio::spawn(async move {
                for i in 0..10 {
                    let (status, r) = if (w + i) % 2 == 0 {
                        post(&c, &format!("{base}/v1/write"), json!({"ops": [{"op": "add", "key": "p0000003", "field": "sold", "value": 1}]})).await
                    } else {
                        post(&c, &format!("{base}/v1/sql"), json!({"sql": "UPDATE products SET sold = sold + 1 WHERE id = 'p0000003'"})).await
                    };
                    assert_eq!((status, r["written"].as_u64()), (200, Some(1)), "{r}");
                }
            })
        })
        .collect();
    for w in writers {
        w.await.unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    maintenance.join().unwrap();
    assert_eq!(sold(&post(&c, &format!("{base}/v1/query"), count).await.1), sold0 + 120);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_guards_the_api() {
    let dir = tempfile::tempdir().unwrap();
    let base = http(service(create(dir.path(), "t.strata", |i| i < 100), "http://unused"), Some("s3cret")).await;
    let c = reqwest::Client::new();
    let q = json!({"sql": "SELECT COUNT(*) FROM products"});
    assert_eq!(post(&c, &format!("{base}/v1/query"), q.clone()).await.0, 401);
    let r = c.post(format!("{base}/v1/query")).bearer_auth("wrong").json(&q).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = c.post(format!("{base}/v1/query")).bearer_auth("s3cret").json(&q).send().await.unwrap();
    assert_eq!(r.status(), 200);
    // The UI page and health stay open, so the page can ask for the token.
    assert_eq!(c.get(format!("{base}/")).send().await.unwrap().status(), 200);
    assert_eq!(c.get(format!("{base}/v1/health")).send().await.unwrap().status(), 200);
}

/// A coordinator over three shard servers answers like one database
/// holding every document, and routes writes by key.
#[tokio::test(flavor = "multi_thread")]
async fn coordinator_answers_like_one_database() {
    let dir = tempfile::tempdir().unwrap();
    let whole = create(dir.path(), "whole.strata", |_| true);
    let key = |i: usize| Value::Str(format!("p{i:07}"));
    let mut urls = Vec::new();
    let mut held = 0;
    for s in 0..3 {
        let db = create(dir.path(), &format!("s{s}.strata"), |i| shard_of(&key(i), 3) == s);
        held += db.snapshot().n_live_docs();
        urls.push(grpc(service(db, "http://unused")).await);
    }
    assert_eq!(held, N as u64);
    let c = Coordinator::connect(urls.clone()).await.unwrap();
    let schema = c.schema.clone();
    let coord = Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent_config("http://unused"), 16, Duration::from_secs(10)));
    for (sql, params) in queries() {
        let p = if params["q"].is_object() { Params::new().set("q", demo::product(42, 7)[12].clone()) } else { Params::new() };
        let want = js::result(&whole.query(sql, &p).unwrap(), 0.0);
        let params = coord.params(Some(&params)).await.unwrap();
        let got = coord.query(QueryReq { sql: sql.into(), params, ..Default::default() }).await.unwrap();
        same(sql, &want, &js::result(&got.rs, 0.0));
    }
    let h = coord.health().await;
    assert_eq!((h["status"].as_str(), h["docs"].as_u64()), (Some("ok"), Some(N as u64)), "{h}");

    // Writes: inserts go to their key's shard, UPDATE and DELETE to all.
    let sql = |s: String| {
        let coord = coord.clone();
        async move { coord.sql(&s, &Params::new(), 0).await.unwrap() }
    };
    let ids: Vec<String> = (0..30).map(|i| format!("new{i}")).collect();
    let values: Vec<String> = ids.iter().map(|id| format!("('{id}', 'Tent {id}', 'outdoor', 10.0, 0)")).collect();
    let n = match sql(format!("INSERT INTO products (id, title, category, price, sold) VALUES {}", values.join(", "))).await {
        strata_server::service::SqlAnswer::Written { n, .. } => n,
        _ => panic!("rows"),
    };
    assert_eq!(n, 30);
    let mut per_shard = [0u64; 3];
    for id in &ids {
        per_shard[shard_of(&Value::Str(id.clone()), 3)] += 1;
    }
    let h = coord.health().await;
    for (s, shard) in h["shards"].as_array().unwrap().iter().enumerate() {
        let before = (0..N).filter(|&i| shard_of(&key(i), 3) == s).count() as u64;
        assert_eq!(shard["docs"].as_u64().unwrap(), before + per_shard[s], "shard {s}");
    }
    sql("UPDATE products SET sold = sold + 4 WHERE MATCH(body, 'tent') AND price = 10".into()).await;
    let count = |s: &str| {
        let coord = coord.clone();
        let s = s.to_string();
        async move { coord.query(QueryReq { sql: s, ..Default::default() }).await.unwrap().rs.rows[0][0].as_i64().unwrap() }
    };
    assert_eq!(count("SELECT COUNT(*) FROM products WHERE sold = 4 AND price = 10").await, 30);
    sql("DELETE FROM products WHERE price = 10".into()).await;
    assert_eq!(count("SELECT COUNT(*) FROM products").await, N as i64);

    // A shard that is down: queries fail with its address, health says so.
    let dead = Coordinator::connect(vec![urls[0].clone()]).await.unwrap();
    drop(dead);
    let err = Coordinator::connect(vec![urls[0].clone(), "http://127.0.0.1:1".into()]).await.err().unwrap();
    assert!(err.message.contains("127.0.0.1:1"), "{}", err.message);
}

/// The agent's tool loop, against a stand-in for the Messages API that
/// asks for one query and then answers.
#[tokio::test(flavor = "multi_thread")]
async fn agent_runs_queries_and_answers() {
    use axum::{extract::State, routing::post as route_post, Json as AxJson, Router};
    #[derive(Clone)]
    struct Mock {
        calls: Arc<AtomicUsize>,
        seen: Arc<std::sync::Mutex<Vec<Json>>>,
    }
    async fn messages(State(m): State<Mock>, headers: axum::http::HeaderMap, AxJson(body): AxJson<Json>) -> AxJson<Json> {
        assert_eq!(headers["x-api-key"], "test-key");
        assert_eq!(headers["anthropic-version"], "2026-10-21");
        m.seen.lock().unwrap().push(body.clone());
        let n = m.calls.fetch_add(1, Ordering::SeqCst);
        let usage = json!({"input_tokens": 1000, "output_tokens": 50});
        AxJson(match n {
            0 => json!({"content": [
                {"type": "thinking", "thinking": "", "signature": "sig"},
                {"type": "tool_use", "id": "t1", "name": "run_sql", "input": {"sql": "SELECT brand, COUNT(*) FROM products WHERE MATCH(body, 'tent') GROUP BY brand ORDER BY 2 DESC LIMIT 3"}},
                {"type": "tool_use", "id": "t2", "name": "run_sql", "input": {"sql": "SELECT nope FROM products"}}
            ], "stop_reason": "tool_use", "usage": usage}),
            _ => json!({"content": [{"type": "text", "text": "Northpeak sells the most tents.\n\n```sql\nSELECT brand, COUNT(*) FROM products GROUP BY brand\n```"}], "stop_reason": "end_turn", "usage": usage}),
        })
    }
    let mock = Mock { calls: Arc::new(AtomicUsize::new(0)), seen: Arc::new(std::sync::Mutex::new(Vec::new())) };
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = l.local_addr().unwrap();
    let app = Router::new().route("/v1/messages", route_post(messages)).with_state(mock.clone());
    tokio::spawn(async move { axum::serve(l, app).await });

    let dir = tempfile::tempdir().unwrap();
    let base = http(service(create(dir.path(), "a.strata", |i| i < 1000), &format!("http://{addr}")), None).await;
    let c = reqwest::Client::new();
    let (status, r) = post(&c, &format!("{base}/v1/agent"), json!({"question": "Which brand sells the most tents?"})).await;
    assert_eq!(status, 200, "{r}");
    assert!(r["answer"].as_str().unwrap().starts_with("Northpeak"));
    assert_eq!(r["model"], "claude-haiku-5-5");
    assert_eq!(r["usage"]["input_tokens"], 2000);
    let steps = r["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["ok"], true);
    assert_eq!(steps[1]["ok"], false);

    {
        let seen = mock.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0]["system"].as_str().unwrap().contains("Table `products`, 1000 rows"));
        assert_eq!(seen[0]["tools"][0]["name"], "run_sql");
        // The second request carries the first answer unchanged (thinking
        // included) and both results in one user message.
        let msgs = seen[1]["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["content"][0]["type"], "thinking");
        let results = msgs[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_use_id"], "t1");
        assert!(results[0]["content"].as_str().unwrap().contains("\"row_count\":3"));
        assert_eq!(results[1]["is_error"], true);
    }

    // Writes are refused; no key means a clear error.
    let svc = service(create(dir.path(), "b.strata", |i| i < 10), "http://unused");
    let mut cfg = svc.agent.clone();
    cfg.api_key = None;
    let svc = Arc::new(Service::new(Backend::Local(match &svc.backend {
        Backend::Local(db) => db.clone(),
        _ => unreachable!(),
    }), svc.schema.clone(), cfg, 4, Duration::from_secs(5)));
    let base = http(svc, None).await;
    let (status, r) = post(&c, &format!("{base}/v1/agent"), json!({"question": "hi"})).await;
    assert_eq!(status, 400);
    assert!(r["error"].as_str().unwrap().contains("API key"));
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_and_binary_vector_fields_over_http() {
    use strata::{Field, FieldType, Schema, VectorCodes, VectorIndexKind};
    let schema = Schema::new(
        "notes",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("splade", FieldType::Sparse),
            Field::new("emb", FieldType::Vector { dim: 8, index: VectorIndexKind::Flat }).codes(VectorCodes::Binary),
        ],
        Some("id"),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { publish_snapshots: true, ..DbOptions::default() };
    let db = Db::create(&dir.path().join("s.strata"), schema, opts).unwrap();
    let base = http(service(db, "http://unused"), None).await;
    let c = reqwest::Client::new();
    let unit = |i: usize| (0..8).map(|j| if j == i % 8 { 1.0 } else { 0.1 }).collect::<Vec<f32>>();
    let docs: Vec<Json> = (0..40)
        .map(|i| json!({"id": format!("n{i:02}"), "splade": {"common": 0.5, format!("tok{}", i % 5): 1.0 + i as f64 / 100.0}, "emb": unit(i)}))
        .collect();
    let ops: Vec<Json> = docs.into_iter().map(|d| json!({"op": "insert", "doc": d})).collect();
    let (status, r) = post(&c, &format!("{base}/v1/write"), json!({ "ops": ops })).await;
    assert_eq!((status, r["written"].clone()), (200, json!(40)), "{r}");

    let (status, r) = post(
        &c,
        &format!("{base}/v1/query"),
        json!({"sql": "SELECT id, score() FROM notes ORDER BY similarity(splade, :w) DESC LIMIT 3", "params": {"w": {"tok3": 2.0, "common": 1.0}}}),
    )
    .await;
    assert_eq!(status, 200, "{r}");
    // tok3 is on n03, n08, ..., n38; the larger weight wins: n38 first.
    assert_eq!(r["keys"], json!(["n38", "n33", "n28"]));
    assert!((r["scores"][0].as_f64().unwrap() - (2.0 * 1.38 + 0.5)).abs() < 2e-3, "{r}");

    let (status, r) = post(&c, &format!("{base}/v1/query"), json!({"sql": "SELECT id FROM notes ORDER BY similarity(emb, :v) DESC LIMIT 5", "params": {"v": unit(2)}})).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["keys"].as_array().unwrap().len(), 5);
    assert!(r["keys"].as_array().unwrap().iter().all(|k| k.as_str().unwrap()[1..].parse::<usize>().unwrap() % 8 == 2), "{r}");

    let (status, r) = post(&c, &format!("{base}/v1/write"), json!({"ops": [{"op": "insert", "doc": {"id": "bad", "splade": {"x": -1}}}]})).await;
    assert_eq!(status, 400, "{r}");
    let metrics = c.get(format!("{base}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains(r#"kind="sparse",outcome="ok"} 1"#), "{metrics}");
}
