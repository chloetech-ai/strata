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
use strata::storage::{DbOptions, Layout};
use strata::value::Value;
use strata_server::auth::{self, Access, Secret, Tls};
use strata_server::service::{shard_of, AgentConfig, Backend, QueryReq, Service, MAX_LEASES};
use strata_server::coord::{CoordOptions, Coordinator};
use strata_server::{demo, json as js, pb, server};
use tokio::net::TcpListener;

const N: usize = 3000;

fn agent_config(base_url: &str) -> AgentConfig {
    AgentConfig { api_key: Some("test-key".into()), model: strata_server::agent::DEFAULT_MODEL.into(), base_url: base_url.into() }
}

fn create(dir: &std::path::Path, name: &str, keep: impl Fn(usize) -> bool) -> Db {
    let opts = DbOptions { segment_max_docs: 700, publish_snapshots: true, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), demo::schema(N), opts).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).unwrap();
    db
}

/// The demo's vector of product 42 (what `{"vector_of": "p0000042"}` binds).
fn vector_42(db: &Db) -> Value {
    db.query("SELECT emb FROM products WHERE id = 'p0000042'", &Params::new()).unwrap().rows[0][0].clone()
}

fn service(db: Db, base_url: &str) -> Arc<Service> {
    let schema = db.schema().clone();
    Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent_config(base_url), 16, Duration::from_secs(10)))
}

async fn http(svc: Arc<Service>, token: Option<&str>) -> String {
    http_with(svc, Access { token: token.map(Secret::new), ..Default::default() }).await
}

/// The server's URL, `https://localhost:port` with TLS.
async fn http_with(svc: Arc<Service>, access: Access) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let url = if access.tls.is_some() { format!("https://localhost:{port}") } else { format!("http://127.0.0.1:{port}") };
    tokio::spawn(server::serve_http(svc, l, access));
    url
}

async fn grpc(svc: Arc<Service>) -> String {
    grpc_with(svc, Access::default()).await
}

async fn grpc_with(svc: Arc<Service>, access: Access) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let url = if access.tls.is_some() { format!("https://localhost:{port}") } else { format!("http://127.0.0.1:{port}") };
    tokio::spawn(server::serve_grpc(svc, l, access));
    url
}

/// The test certificate for localhost and the CA that signed it.
fn test_tls() -> (Tls, Vec<u8>) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tls");
    let read = |f: &str| std::fs::read(dir.join(f)).unwrap();
    (Tls { cert: read("cert.pem"), key: read("key.pem") }, read("ca.pem"))
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
        let p = if params["q"].is_object() { Params::new().set("q", vector_42(&db)) } else { Params::new() };
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
    let cores = std::thread::available_parallelism().unwrap().get();
    assert!(metrics.contains(&format!("strata_query_pool_threads {cores}\n")) && metrics.contains("strata_query_pool_queued 0\n"), "{metrics}");
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

/// Write requests that wait together share one commit; one that fails is
/// not applied at all, and the others sharing its commit still are.
#[tokio::test(flavor = "multi_thread")]
async fn grouped_writes_fail_alone() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(create(dir.path(), "g.strata", |i| i < 200));
    let svc = Arc::new(Service::new(Backend::Local(db.clone()), db.schema().clone(), agent_config("http://unused"), 16, Duration::from_secs(30)));
    let base = http(svc, None).await;
    let c = reqwest::Client::new();
    let sold = |r: &Json| r["rows"][0][0].as_i64().unwrap();
    let count = json!({"sql": "SELECT sold FROM products WHERE id = 'p0000004'"});
    let sold0 = sold(&post(&c, &format!("{base}/v1/query"), count.clone()).await.1);
    let requests: Vec<_> = (0..64)
        .map(|i| {
            let (c, base) = (c.clone(), base.clone());
            tokio::spawn(async move {
                let mut ops = vec![json!({"op": "add", "key": "p0000004", "field": "sold", "value": 1})];
                if i % 8 == 0 {
                    // A key that is already live: this request fails as a whole.
                    ops.push(json!({"op": "insert", "doc": {"id": "p0000001", "title": "again"}}));
                }
                post(&c, &format!("{base}/v1/write"), json!({"ops": ops})).await.0
            })
        })
        .collect();
    let mut statuses = Vec::new();
    for r in requests {
        statuses.push(r.await.unwrap());
    }
    assert_eq!(statuses.iter().filter(|&&s| s == 200).count(), 56, "{statuses:?}");
    assert_eq!(statuses.iter().filter(|&&s| s == 400).count(), 8, "{statuses:?}");
    assert_eq!(sold(&post(&c, &format!("{base}/v1/query"), count).await.1), sold0 + 56);
}

/// A retried write with an idempotency key is applied once (DB-2), and an
/// `add` after an upsert of the same key in one request reads the upserted
/// value (bug 13).
#[tokio::test(flavor = "multi_thread")]
async fn retries_apply_once_and_adds_read_earlier_ops() {
    let dir = tempfile::tempdir().unwrap();
    let base = http(service(create(dir.path(), "i.strata", |i| i < 100), "http://unused"), None).await;
    let c = reqwest::Client::new();
    let url = format!("{base}/v1/write");
    let sold = |id: &'static str| {
        let (c, base) = (c.clone(), base.clone());
        async move { post(&c, &format!("{base}/v1/query"), json!({"sql": "SELECT sold FROM products WHERE id = :k", "params": {"k": id}})).await.1["rows"][0][0].clone() }
    };

    let ops = json!([
        {"op": "upsert", "doc": {"id": "p0000005", "title": "Five", "sold": 1}},
        {"op": "add", "key": "p0000005", "field": "sold", "value": 5},
    ]);
    let (status, r) = post(&c, &url, json!({"ops": ops})).await;
    assert_eq!((status, r["written"].clone()), (200, json!(2)), "{r}");
    assert_eq!(sold("p0000005").await, json!(6));

    // The same request three times, with a key: applied once, answered alike.
    let add = json!([{"op": "add", "key": "p0000005", "field": "sold", "value": 10}, {"op": "insert", "doc": {"id": "n9", "title": "New"}}]);
    for _ in 0..3 {
        let (status, r) = post(&c, &url, json!({"ops": add, "idempotency_key": "req-1"})).await;
        assert_eq!((status, r["written"].clone()), (200, json!(2)), "{r}");
    }
    assert_eq!(sold("p0000005").await, json!(16));
    // The key in a header works the same.
    for _ in 0..2 {
        let r = c.post(&url).header("Idempotency-Key", "req-2").json(&json!({"ops": [{"op": "add", "key": "p0000005", "field": "sold", "value": 1}]})).send().await.unwrap();
        assert_eq!(r.status(), 200);
    }
    assert_eq!(sold("p0000005").await, json!(17));
    // A key reused for other operations is refused.
    let (status, _) = post(&c, &url, json!({"ops": [{"op": "delete", "key": "n9"}], "idempotency_key": "req-1"})).await;
    assert_eq!(status, 422);
    // A request that failed is not remembered, so its retry runs.
    let bad = json!([{"op": "insert", "doc": {"id": "n9", "title": "again"}}]);
    assert_eq!(post(&c, &url, json!({"ops": bad, "idempotency_key": "req-3"})).await.0, 400);
    let (status, _) = post(&c, &url, json!({"ops": [{"op": "delete", "key": "n9"}], "idempotency_key": "req-4"})).await;
    assert_eq!(status, 200);
    let (status, r) = post(&c, &url, json!({"ops": bad, "idempotency_key": "req-3"})).await;
    assert_eq!((status, r["written"].clone()), (200, json!(1)), "{r}");

    // Integers stay exact past 2^53; an INT takes integer increments only.
    let big = 9007199254740993i64;
    let set = json!([{"op": "set", "key": "p0000005", "field": "sold", "value": big - 1}, {"op": "add", "key": "p0000005", "field": "sold", "value": 1}]);
    assert_eq!(post(&c, &url, json!({"ops": set})).await.0, 200);
    assert_eq!(sold("p0000005").await, json!(big));
    let (status, _) = post(&c, &url, json!({"ops": [{"op": "add", "key": "p0000005", "field": "sold", "value": 1.5}]})).await;
    assert_eq!(status, 400);
    let (status, _) = post(&c, &url, json!({"ops": [{"op": "add", "key": "p0000005", "field": "sold", "value": 1}], "idempotency_key": ""})).await;
    assert_eq!(status, 400);
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

/// OPS-1: the gRPC port took writes from anyone, a token on HTTP or not.
/// Every gRPC call now needs the token, and a coordinator sends it.
#[tokio::test(flavor = "multi_thread")]
async fn a_token_guards_grpc() {
    let dir = tempfile::tempdir().unwrap();
    let token = Some(Secret::new("sh4rd"));
    let url = grpc_with(service(create(dir.path(), "g.strata", |i| i < 100), "http://unused"), Access { token: token.clone(), ..Default::default() }).await;
    let mut c = pb::strata_client::StrataClient::connect(url.clone()).await.unwrap();
    let delete = pb::WriteRequest { sql: "DELETE FROM products".into(), ..Default::default() };
    let e = c.write(delete.clone()).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unauthenticated);
    let mut req = tonic::Request::new(delete);
    req.metadata_mut().insert("authorization", "Bearer wrong".parse().unwrap());
    assert_eq!(c.write(req).await.unwrap_err().code(), tonic::Code::Unauthenticated);
    assert!(c.health(pb::Empty {}).await.is_err());

    let err = Coordinator::connect(vec![url.clone()]).await.err().unwrap();
    assert!(err.message.contains("token"), "{}", err.message);
    let coord = Coordinator::connect_with(vec![url], CoordOptions { token, ..Default::default() }).await.unwrap();
    let svc = Arc::new(Service::new(Backend::Cluster(Arc::new(coord)), demo::schema(N), agent_config("http://unused"), 16, Duration::from_secs(10)));
    let rs = svc.query(QueryReq { sql: "SELECT COUNT(*) FROM products".into(), ..Default::default() }).await.unwrap().rs;
    assert_eq!(rs.rows[0][0], Value::Int(100));
}

/// Bug 14 and OPS-2: with no token, any web page could write through the
/// visitor's browser: a text/plain POST to /v1/import needs no preflight,
/// and a page that rebinds its own name to 127.0.0.1 reads answers too.
#[tokio::test(flavor = "multi_thread")]
async fn pages_on_other_sites_are_kept_out() {
    let dir = tempfile::tempdir().unwrap();
    let hosts = auth::allowed_hosts("127.0.0.1".parse().unwrap(), &["db.internal".into()]);
    let base = http_with(service(create(dir.path(), "b.strata", |i| i < 100), "http://unused"), Access { hosts, ..Default::default() }).await;
    let host = base.trim_start_matches("http://").to_string();
    let c = reqwest::Client::new();
    let csv = "id,title\nx1,from a page\n";
    let import = format!("{base}/v1/import?format=csv");

    // The attack as the review sent it: refused for its origin, and for its
    // content type even without one.
    let r = c.post(&import).header("content-type", "text/plain").header("origin", "https://evil.example").body(csv).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let r = c.post(&import).header("content-type", "text/plain").body(csv).send().await.unwrap();
    assert_eq!(r.status(), 415);
    let r = c.post(&import).body(csv).send().await.unwrap();
    assert_eq!(r.status(), 415);
    let count = json!({"sql": "SELECT COUNT(*) FROM products"});
    assert_eq!(post(&c, &format!("{base}/v1/query"), count.clone()).await.1["rows"][0][0], 100);
    // A JSON write from another origin, too.
    let w = json!({"ops": [{"op": "delete", "key": "p0000001"}]});
    let r = c.post(format!("{base}/v1/write")).header("origin", "https://evil.example").json(&w).send().await.unwrap();
    assert_eq!(r.status(), 403);

    // The UI's own requests, and clients that say what they send, pass.
    let r = c.post(&import).header("content-type", "text/csv").header("origin", format!("http://{host}")).body(csv).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = c.post(format!("{base}/v1/write")).header("origin", format!("http://{host}")).json(&w).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(post(&c, &format!("{base}/v1/query"), count).await.1["rows"][0][0], 100);

    // DNS rebinding: the page's own host name, on every route.
    let port = host.rsplit(':').next().unwrap();
    for path in ["/", "/v1/health", "/v1/schema"] {
        let r = c.get(format!("{base}{path}")).header("host", format!("evil.example:{port}")).send().await.unwrap();
        assert_eq!(r.status(), 403, "{path}");
    }
    for name in ["localhost", "db.internal", "DB.internal."] {
        let r = c.get(format!("{base}/v1/health")).header("host", format!("{name}:{port}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{name}");
    }
}

/// OPS-1: TLS on both listeners, and a coordinator that checks its shards'
/// certificates against a CA.
#[tokio::test(flavor = "multi_thread")]
async fn tls_on_both_listeners() {
    let dir = tempfile::tempdir().unwrap();
    let (tls, ca) = test_tls();
    let svc = service(create(dir.path(), "s.strata", |i| i < 100), "http://unused");
    let access = Access { token: Some(Secret::new("t")), tls: Some(tls), ..Default::default() };
    let base = http_with(svc.clone(), access.clone()).await;
    let shard = grpc_with(svc, access).await;
    assert!(base.starts_with("https://") && shard.starts_with("https://"));

    let c = reqwest::Client::builder().add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap()).build().unwrap();
    let r = c.post(format!("{base}/v1/query")).bearer_auth("t").json(&json!({"sql": "SELECT COUNT(*) FROM products"})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Json>().await.unwrap()["rows"][0][0], 100);
    // Plain HTTP to the TLS port gets nothing, and a client without the CA
    // does not trust the certificate.
    assert!(reqwest::get(base.replace("https://", "http://") + "/v1/health").await.is_err());
    assert!(reqwest::get(format!("{base}/v1/health")).await.is_err());

    let opts = |ca| CoordOptions { token: Some(Secret::new("t")), ca, ..Default::default() };
    assert!(Coordinator::connect_with(vec![shard.clone()], opts(None)).await.is_err());
    let coord = Coordinator::connect_with(vec![shard], opts(Some(ca))).await.unwrap();
    assert_eq!(coord.schema.table, "products");
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
        let p = if params["q"].is_object() { Params::new().set("q", vector_42(&whole)) } else { Params::new() };
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

/// A shard server that answers `Schema` and never anything else, like one
/// whose machine froze after the coordinator started.
struct Stuck(String);

#[tonic::async_trait]
impl pb::strata_server::Strata for Stuck {
    async fn query(&self, _: tonic::Request<pb::QueryRequest>) -> Result<tonic::Response<pb::QueryResponse>, tonic::Status> {
        std::future::pending().await
    }
    async fn term_stats(&self, _: tonic::Request<pb::QueryRequest>) -> Result<tonic::Response<pb::TermStatsResponse>, tonic::Status> {
        std::future::pending().await
    }
    async fn write(&self, _: tonic::Request<pb::WriteRequest>) -> Result<tonic::Response<pb::WriteResponse>, tonic::Status> {
        std::future::pending().await
    }
    async fn schema(&self, _: tonic::Request<pb::Empty>) -> Result<tonic::Response<pb::SchemaResponse>, tonic::Status> {
        Ok(tonic::Response::new(pb::SchemaResponse { schema_json: self.0.clone() }))
    }
    async fn health(&self, _: tonic::Request<pb::Empty>) -> Result<tonic::Response<pb::HealthResponse>, tonic::Status> {
        std::future::pending().await
    }
}

/// A shard server that refuses every query because too many are running.
struct Overloaded(String);

#[tonic::async_trait]
impl pb::strata_server::Strata for Overloaded {
    async fn query(&self, _: tonic::Request<pb::QueryRequest>) -> Result<tonic::Response<pb::QueryResponse>, tonic::Status> {
        Err(tonic::Status::resource_exhausted("too many queries running; try again"))
    }
    async fn term_stats(&self, _: tonic::Request<pb::QueryRequest>) -> Result<tonic::Response<pb::TermStatsResponse>, tonic::Status> {
        Err(tonic::Status::resource_exhausted("too many queries running; try again"))
    }
    async fn write(&self, _: tonic::Request<pb::WriteRequest>) -> Result<tonic::Response<pb::WriteResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("read only"))
    }
    async fn schema(&self, _: tonic::Request<pb::Empty>) -> Result<tonic::Response<pb::SchemaResponse>, tonic::Status> {
        Ok(tonic::Response::new(pb::SchemaResponse { schema_json: self.0.clone() }))
    }
    async fn health(&self, _: tonic::Request<pb::Empty>) -> Result<tonic::Response<pb::HealthResponse>, tonic::Status> {
        Ok(tonic::Response::new(pb::HealthResponse { status: "ok".into(), ..Default::default() }))
    }
}

async fn stuck(schema: &strata::Schema) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let svc = pb::strata_server::StrataServer::new(Stuck(serde_json::to_string(schema).unwrap()));
    tokio::spawn(tonic::transport::Server::builder().add_service(svc).serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(l)));
    format!("http://{addr}")
}

fn cluster(c: Coordinator) -> Arc<Service> {
    let schema = c.schema.clone();
    Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent_config("http://unused"), 16, Duration::from_secs(10)))
}

/// With two replicas, a shard whose first replica is down or hangs still
/// answers, as one database would, and quickly. A shard with no replica
/// answering fails the query, or, with partial answers allowed, is left
/// out and named (and not waited for again for a second). Writes reach
/// every replica.
#[tokio::test(flavor = "multi_thread")]
async fn replicas_cover_for_a_shard_down_or_hung() {
    let dir = tempfile::tempdir().unwrap();
    let whole = create(dir.path(), "whole.strata", |_| true);
    let key = |i: usize| Value::Str(format!("p{i:07}"));
    let mut good = Vec::new();
    for s in 0..2 {
        let mut replicas = Vec::new();
        for r in 0..2 {
            let db = create(dir.path(), &format!("s{s}r{r}.strata"), |i| shard_of(&key(i), 2) == s);
            replicas.push(grpc(service(db, "http://unused")).await);
        }
        good.push(replicas);
    }
    let dead = "http://127.0.0.1:1".to_string();
    let hung = stuck(whole.schema()).await;

    // Shard 0's first replica refuses connections; shard 1's never answers.
    let opts = CoordOptions { hedge: Duration::from_millis(50), ..Default::default() };
    let c = Coordinator::connect_with(vec![format!("{dead}|{}", good[0][0]), format!("{hung}|{}", good[1][0])], opts.clone()).await.unwrap();
    let coord = cluster(c);
    for (sql, params) in queries() {
        let p = if params["q"].is_object() { Params::new().set("q", vector_42(&whole)) } else { Params::new() };
        let want = js::result(&whole.query(sql, &p).unwrap(), 0.0);
        let params = coord.params(Some(&params)).await.unwrap();
        let t = std::time::Instant::now();
        let got = coord.query(QueryReq { sql: sql.into(), params, ..Default::default() }).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(2), "{sql}: {:?}", t.elapsed());
        assert!(got.missing.is_empty());
        same(sql, &want, &js::result(&got.rs, 0.0));
    }
    let h = coord.health().await;
    assert_eq!(h["status"], "degraded", "{h}");
    assert_eq!(h["docs"].as_u64(), Some(N as u64), "{h}");
    assert!(h["retries"].as_u64().unwrap() >= 1 && h["hedges"].as_u64().unwrap() >= 1, "{h}");

    // Shard 1 with no replica answering, given up after 200 ms.
    let sql = "SELECT id, score() FROM products ORDER BY bm25(body, 'waterproof hiking boots') DESC LIMIT 10";
    let opts = CoordOptions { shard_timeout: Some(Duration::from_millis(200)), ..opts };
    let c = Coordinator::connect_with(vec![good[0][1].clone(), hung.clone()], opts.clone()).await.unwrap();
    let strict = cluster(c);
    let t = std::time::Instant::now();
    let err = strict.query(QueryReq { sql: sql.into(), ..Default::default() }).await.err().unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(err.status, 504, "{}", err.message);
    assert!(err.message.contains(&hung), "{}", err.message);
    let a = strict.query(QueryReq { sql: sql.into(), partial: Some(true), ..Default::default() }).await.unwrap();
    assert_eq!(a.missing, vec![hung.clone()]);
    // For the next second, partial answers leave the shard out without waiting for it.
    let t = std::time::Instant::now();
    let again = strict.query(QueryReq { sql: sql.into(), partial: Some(true), ..Default::default() }).await.unwrap();
    assert!(t.elapsed() < Duration::from_millis(150), "{:?}", t.elapsed());
    assert_eq!(again.missing, vec![hung.clone()]);
    assert!(!a.rs.rows.is_empty());
    for row in &a.rs.rows {
        assert_eq!(shard_of(&row[0], 2), 0, "{row:?}");
    }
    // Scored over the shard that answered, as a database of its documents would.
    let only0 = create(dir.path(), "only0.strata", |i| shard_of(&key(i), 2) == 0);
    same(sql, &js::result(&only0.query(sql, &Params::new()).unwrap(), 0.0), &js::result(&a.rs, 0.0));
    let c = Coordinator::connect_with(vec![good[0][1].clone(), hung.clone()], CoordOptions { partial: true, ..opts }).await.unwrap();
    let lenient = cluster(c);
    assert_eq!(lenient.query(QueryReq { sql: sql.into(), ..Default::default() }).await.unwrap().missing, vec![hung.clone()]);
    assert!(lenient.query(QueryReq { sql: sql.into(), partial: Some(false), ..Default::default() }).await.is_err());

    // Writes go to every replica of their shard.
    let c = Coordinator::connect(vec![good[0].join("|"), good[1].join("|")]).await.unwrap();
    let both = cluster(c);
    let ids: Vec<String> = (0..20).map(|i| format!("rep{i}")).collect();
    let values: Vec<String> = ids.iter().map(|id| format!("('{id}', 'Tent {id}', 'outdoor', 10.0, 0)")).collect();
    both.sql(&format!("INSERT INTO products (id, title, category, price, sold) VALUES {}", values.join(", ")), &Params::new(), 0).await.unwrap();
    let h = both.health().await;
    assert_eq!(h["status"], "ok", "{h}");
    for (s, shard) in h["shards"].as_array().unwrap().iter().enumerate() {
        let want = (0..N).filter(|&i| shard_of(&key(i), 2) == s).count() + ids.iter().filter(|id| shard_of(&Value::Str(id.to_string()), 2) == s).count();
        for r in shard["replicas"].as_array().unwrap() {
            assert_eq!(r["docs"].as_u64(), Some(want as u64), "shard {s}: {r}");
        }
    }
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

/// A field declared as the embedding of others: the server embeds the rows
/// writes send without it and the text of `{"embed": ...}` parameters with
/// the same model, and says so in /v1/schema.
#[tokio::test(flavor = "multi_thread")]
async fn embedded_fields_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("e.strata"), demo::schema(500), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    let schema = db.schema().clone();
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent_config("http://unused"), 16, Duration::from_secs(10));
    svc.open_models(dir.path());
    let base = http(Arc::new(svc), None).await;
    let c = reqwest::Client::new();
    let r: Json = c.get(format!("{base}/v1/schema")).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["embeddings"], json!([{"field": "emb", "from": ["title", "description"], "model": "hash:64", "loaded": true, "error": null}]));

    let docs: Vec<Json> = (0..500).map(|i| js::document_json(&demo::schema(500), &demo::product(i, 7))).collect();
    let mut ops: Vec<Json> = docs.iter().map(|d| json!({"op": "insert", "doc": d})).collect();
    ops.push(json!({"op": "insert", "doc": {"id": "zz1", "title": "Zebra striped umbrella", "description": "A zebra umbrella for rainy days."}}));
    let (status, w) = post(&c, &format!("{base}/v1/write"), json!({"ops": ops})).await;
    assert_eq!((status, w["written"].as_u64()), (200, Some(501)), "{w}");

    let find = |text: &str| json!({"sql": "SELECT id FROM products ORDER BY similarity(emb, :q) DESC LIMIT 1", "params": {"q": {"embed": text, "field": "emb"}}});
    let (status, r) = post(&c, &format!("{base}/v1/query"), find("zebra umbrella")).await;
    assert_eq!((status, &r["rows"][0][0]), (200, &json!("zz1")), "{r}");
    // A rewrite of a source re-embeds the row.
    let (status, _) = post(&c, &format!("{base}/v1/sql"), json!({"sql": "UPDATE products SET title = 'Giraffe desk lamp', description = 'A tall giraffe lamp.' WHERE id = 'zz1'"})).await;
    assert_eq!(status, 200);
    let (_, r) = post(&c, &format!("{base}/v1/query"), find("giraffe lamp")).await;
    assert_eq!(r["rows"][0][0], json!("zz1"), "{r}");
    // Only an embedded field with a model embeds text.
    let (status, r) = post(&c, &format!("{base}/v1/query"), json!({"sql": "SELECT id FROM products ORDER BY similarity(emb, :q) DESC LIMIT 1", "params": {"q": {"embed": "x", "field": "title"}}})).await;
    assert_eq!(status, 400, "{r}");
}

const ITEMS: &str = r#"{"table": "items", "key": "id", "fields": [
    {"name": "id", "type": "keyword"},
    {"name": "title", "type": "text"},
    {"name": "tags", "type": "keyword", "multi": true},
    {"name": "price", "type": "float"},
    {"name": "emb", "type": "vector", "embed": {"from": "title", "model": "hash:16"}}
]}"#;

#[tokio::test(flavor = "multi_thread")]
async fn imports_files_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let schema = strata::Schema::from_json(ITEMS).unwrap();
    let db = Db::create(&dir.path().join("i.strata"), schema.clone(), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent_config("http://unused"), 16, Duration::from_secs(10));
    svc.open_models(dir.path());
    let base = http(Arc::new(svc), None).await;
    let c = reqwest::Client::new();

    let jsonl = (0..2500).map(|i| json!({"id": format!("i{i}"), "title": format!("item number {i}"), "tags": ["a", "b"], "price": i, "colour": "red"}).to_string()).collect::<Vec<_>>().join("\n");
    let ndjson = "application/x-ndjson";
    let r = c.post(format!("{base}/v1/import?format=jsonl")).header("content-type", ndjson).body(jsonl).send().await.unwrap();
    let (status, r): (u16, Json) = (r.status().as_u16(), r.json().await.unwrap());
    assert_eq!((status, &r["written"], &r["skipped_fields"]), (200, &json!(2500), &json!(["colour"])), "{r}");

    // CSV by its content type, replacing a row and adding one; the model
    // embeds the new titles.
    let csv = "id,title,tags,price\ni7,\"zebra striped, umbrella\",x|y,7.5\nnew,giraffe desk lamp,,1\n";
    let r = c.post(format!("{base}/v1/import")).header("content-type", "text/csv").body(csv).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let (_, r) = post(&c, &format!("{base}/v1/query"), json!({"sql": "SELECT COUNT(*) FROM items"})).await;
    assert_eq!(r["rows"][0][0], json!(2501), "{r}");
    let (_, r) = post(&c, &format!("{base}/v1/query"), json!({"sql": "SELECT title, tags, price FROM items WHERE id = 'i7'"})).await;
    assert_eq!(r["rows"][0], json!(["zebra striped, umbrella", ["x", "y"], 7.5]), "{r}");
    let find = |text: &str| json!({"sql": "SELECT id FROM items ORDER BY similarity(emb, :q) DESC LIMIT 1", "params": {"q": {"embed": text, "field": "emb"}}});
    let (_, r) = post(&c, &format!("{base}/v1/query"), find("giraffe desk lamp")).await;
    assert_eq!(r["rows"][0][0], json!("new"), "{r}");

    // A bad row names its line; no format is an error.
    let r = c.post(format!("{base}/v1/import?format=jsonl")).header("content-type", ndjson).body("{\"id\": \"ok\"}\n{\"id\": \"bad\", \"price\": \"cheap\"}\n").send().await.unwrap();
    let (status, r): (u16, Json) = (r.status().as_u16(), r.json().await.unwrap());
    assert_eq!(status, 400);
    assert!(r["error"].as_str().unwrap().contains("line 2"), "{r}");
    let r = c.post(format!("{base}/v1/import")).header("content-type", "application/octet-stream").body("id\nx\n").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);
    // No content type at all is what a page's Blob upload sends.
    let r = c.post(format!("{base}/v1/import?format=csv")).body("id\nx\n").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 415);

    // The schema in its short form, and a schema checked.
    let r: Json = c.get(format!("{base}/v1/schema")).send().await.unwrap().json().await.unwrap();
    let mut short: Json = serde_json::from_str(ITEMS).unwrap();
    short["fields"][4]["embed"]["from"] = json!(["title"]);
    assert_eq!(r["short"], short, "{r}");
    let (status, r) = post(&c, &format!("{base}/v1/schema/check"), json!({"table": "t", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "v", "type": "vector"}]})).await;
    assert_eq!(status, 400);
    assert!(r["error"].as_str().unwrap().contains("dim"), "{r}");
    let (status, r) = post(&c, &format!("{base}/v1/schema/check"), json!({"table": "t", "key": "id", "fields": [{"name": "id", "type": "keyword"}]})).await;
    assert_eq!((status, &r["short"]["key"]), (200, &json!("id")), "{r}");
    assert_eq!(r["schema"]["primary_key"], json!(0), "{r}");
}

#[test]
fn imports_files_from_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let (schema, rows, more) = (dir.path().join("items.json"), dir.path().join("rows.jsonl"), dir.path().join("more.tsv"));
    std::fs::write(&schema, ITEMS).unwrap();
    std::fs::write(&rows, "{\"id\": \"a\", \"title\": \"first\"}\n{\"id\": \"b\", \"title\": \"second\", \"price\": 2}\n").unwrap();
    std::fs::write(&more, "id\ttitle\tprice\nb\tsecond again\t3\nc\tthird\t\n").unwrap();
    let db = dir.path().join("items.strata");
    server::import_files(&db, Some(&schema), dir.path(), None, false, &[rows.clone(), more]).unwrap();
    let opened = Db::open(&db, DbOptions::default()).unwrap();
    let r = opened.query("SELECT id, title, price FROM items", &Params::new()).unwrap();
    let mut got: Vec<String> = r.rows.iter().map(|r| format!("{:?}", r)).collect();
    got.sort();
    assert_eq!(got.len(), 3, "{got:?}");
    assert!(got[1].contains("second again") && got[1].contains("3.0"), "{got:?}");
    drop(opened);
    // An existing database takes no schema.
    assert!(server::import_files(&db, Some(&schema), dir.path(), None, false, &[rows]).unwrap_err().contains("exists"));
}

// OPS-3: a replica that refused a query because too many were running was
// retried on its sibling, which then carried both loads. The refusal (429,
// ResourceExhausted over gRPC) now fails the query instead.
#[tokio::test(flavor = "multi_thread")]
async fn an_overloaded_replica_is_not_relieved_by_its_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let db = create(dir.path(), "db.strata", |_| true);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let over = format!("http://{}", l.local_addr().unwrap());
    let fake = pb::strata_server::StrataServer::new(Overloaded(serde_json::to_string(db.schema()).unwrap()));
    tokio::spawn(tonic::transport::Server::builder().add_service(fake).serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(l)));
    let good = grpc(service(db, "http://unused")).await;
    let coord = cluster(Coordinator::connect(vec![format!("{over}|{good}")]).await.unwrap());
    // Reads take the replicas in turn: one of two asks the overloaded one first.
    let mut statuses = Vec::new();
    for _ in 0..2 {
        let r = coord.query(QueryReq { sql: "SELECT COUNT(*) FROM products".into(), ..Default::default() }).await;
        statuses.push(r.map_or_else(|e| e.status, |_| 200));
    }
    statuses.sort();
    assert_eq!(statuses, vec![200, 429]);
    assert_eq!(coord.health().await["retries"], 0);
}

// SCALE-2/OPS-3: a query whose caller went away (a client that hung up, a
// hedge that lost, a shard timeout) never took itself off the running
// count, so every later query searched its segments on fewer threads until
// a restart; and it gave its slot back while its work went on. The work now
// holds both, and stops at its next check.
#[tokio::test(flavor = "multi_thread")]
async fn dropped_queries_leave_the_running_count() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service(create(dir.path(), "db.strata", |_| true), "http://unused");
    let sql = "SELECT id, score() FROM products ORDER BY bm25(body, 'waterproof hiking boots') DESC LIMIT 100";
    for i in 0..60u64 {
        let s = svc.clone();
        let t = tokio::spawn(async move { s.query(QueryReq { sql: sql.into(), ..Default::default() }).await });
        tokio::time::sleep(Duration::from_micros(50 * (i % 20))).await;
        t.abort();
    }
    let t0 = std::time::Instant::now();
    while svc.running() > 0 {
        assert!(t0.elapsed() < Duration::from_secs(5), "{} queries still count as running", svc.running());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    svc.query(QueryReq { sql: sql.into(), ..Default::default() }).await.unwrap();
    assert_eq!(svc.running(), 0);
}

// SCALE-5: each BM25 query through a coordinator pinned a version on every
// shard for 60 s, released only when a later lease was made, and a client
// could make leases without limit. A statistics round's lease now lasts for
// its query's deadline and a second, expired ones are swept, and at most
// MAX_LEASES are held. A request's deadline is capped too.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_leases_end_with_their_query() {
    let dir = tempfile::tempdir().unwrap();
    let svc = service(create(dir.path(), "db.strata", |_| true), "http://unused");
    let sql = "SELECT id FROM products ORDER BY bm25(body, 'waterproof hiking boots') DESC LIMIT 5";
    let (_, lease) = svc.term_stats(sql, &Params::new(), 200).await.unwrap();
    let q = |snapshot| QueryReq { sql: sql.into(), snapshot, ..Default::default() };
    svc.query(q(lease)).await.unwrap();
    assert_eq!(svc.sweep_leases(), 1);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(svc.sweep_leases(), 0);
    let e = svc.query(q(lease)).await.err().unwrap();
    assert!(e.status == 400 && e.message.contains("expired"), "{}", e.message);

    for _ in 0..MAX_LEASES {
        svc.lease().unwrap();
    }
    assert_eq!(svc.lease().err().unwrap().status, 429);
    assert_eq!(svc.timeout_of(u64::MAX), svc.max_timeout);
}

// OPS-5/OPS-6: a database that could not write (a full disk) still said
// "ok", failed every write with a 500 nobody counted or logged, and went on
// merging every 5 s for nothing. Health now says "degraded" while flushes
// fail (writes still commit to the WAL) and "failing" once no write can
// succeed until a restart; /livez and /readyz, open without the token, then
// answer 503 so that the server is restarted. Failed writes are counted.
#[tokio::test(flavor = "multi_thread")]
async fn health_reports_a_database_that_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.strata");
    let opts = DbOptions { layout: Layout::Directory, publish_snapshots: true, ..DbOptions::default() };
    let db = Db::create(&path, demo::schema(10), opts).unwrap();
    db.load((0..10).map(|i| demo::product(i, 7))).unwrap();
    let svc = service(db, "http://unused");
    let db = svc.local_db().unwrap().clone();
    let url = http(svc.clone(), Some("tok")).await;
    let c = reqwest::Client::new();
    let get = |route: &str| {
        let r = c.get(format!("{url}{route}")).bearer_auth("tok").send();
        async move {
            let r = r.await.unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    };
    let open = |route: &str| {
        let r = c.get(format!("{url}{route}")).send();
        async move { r.await.unwrap().status().as_u16() }
    };
    let write = |i: usize| svc.write_ops(vec![json!({"op": "upsert", "doc": js::document_json(&svc.schema, &demo::product(i, 7))})], None);
    let status = |h: &str| serde_json::from_str::<Json>(h).unwrap()["status"].clone();
    assert_eq!((open("/livez").await, open("/readyz").await), (200, 200));
    let (code, h) = get("/v1/health").await;
    assert_eq!((code, status(&h)), (200, json!("ok")), "{h}");

    // Flushes fail (directories where the segment files go): degraded.
    let blockers: Vec<_> = (0..64).map(|id| path.join(format!("seg-{id:010}.strata"))).filter(|p| !p.exists()).collect();
    for b in &blockers {
        std::fs::create_dir(b).unwrap();
    }
    write(20).await.unwrap();
    assert!(db.storage().flush().is_err());
    let (code, h) = get("/v1/health").await;
    assert_eq!((code, status(&h)), (200, json!("degraded")), "{h}");
    assert_eq!(open("/livez").await, 200);
    for b in &blockers {
        std::fs::remove_dir(b).unwrap();
    }

    // A commit that fails partway (here pointing CURRENT at the new
    // manifest) leaves the database refusing writes until it is reopened.
    std::fs::create_dir(path.join("CURRENT.tmp")).unwrap();
    assert!(db.storage().flush().is_err());
    assert!(db.storage().is_poisoned());
    let (code, h) = get("/v1/health").await;
    assert_eq!((code, status(&h)), (503, json!("failing")), "{h}");
    assert_eq!((open("/livez").await, open("/readyz").await), (503, 503));
    assert_eq!(write(21).await.err().unwrap().status, 500);
    let (_, m) = get("/metrics").await;
    assert!(m.contains("strata_write_errors_total{status=\"500\"} 1") && m.contains("strata_failing 1"), "{m}");
}


/// Checkpoints into `--backup-dir` (a write still in the memtable included,
/// a directory checkpoint linked to the one before), names that would leave
/// the directory refused, and the check and vacuum routes.
#[tokio::test]
async fn admin_routes_checkpoint_check_and_vacuum() {
    let dir = tempfile::tempdir().unwrap();
    let db = create(dir.path(), "db.strata", |_| true);
    let schema = db.schema().clone();
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent_config("http://127.0.0.1:9"), 16, Duration::from_secs(10));
    let backups = dir.path().join("backups");
    svc.set_backup_dir(&backups).unwrap();
    let base = http(Arc::new(svc), None).await;
    let c = reqwest::Client::new();
    let (status, r) = post(&c, &format!("{base}/v1/write"), json!({"ops": [{"op": "delete", "key": "p0000001"}]})).await;
    assert_eq!(status, 200, "{r}");

    let (status, r) = post(&c, &format!("{base}/v1/admin/checkpoint"), json!({"name": "first"})).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["checkpoint"]["live_docs"], json!(N - 1));
    let copy = Db::open(&backups.join("first"), DbOptions::default()).unwrap();
    assert_eq!(copy.snapshot().n_live_docs(), (N - 1) as u64);
    assert!(copy.query("SELECT id FROM products WHERE id = 'p0000001'", &Params::new()).unwrap().rows.is_empty());
    drop(copy);

    let (status, r) = post(&c, &format!("{base}/v1/admin/checkpoint"), json!({"name": "d1", "layout": "dir"})).await;
    assert_eq!(status, 200, "{r}");
    let (status, r) = post(&c, &format!("{base}/v1/admin/checkpoint"), json!({"name": "d2", "layout": "dir", "link_from": "d1"})).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["checkpoint"]["segments_linked"], r["checkpoint"]["segments"], "{r}");

    for body in [json!({"name": "../out"}), json!({"name": ".hidden"}), json!({"name": "a/b"}), json!({"name": "x", "layout": "zip"}), json!({}), json!({"name": "y", "layout": "dir", "link_from": "../d1"})] {
        let (status, r) = post(&c, &format!("{base}/v1/admin/checkpoint"), body.clone()).await;
        assert_eq!(status, 400, "{body}: {r}");
    }
    let (status, _) = post(&c, &format!("{base}/v1/admin/checkpoint"), json!({"name": "first"})).await;
    assert_eq!(status, 500, "an existing name is not overwritten");
    // (`first-wal` is the log opening `first` above created.)
    let mut names: Vec<String> = std::fs::read_dir(&backups).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n != "first-wal").collect();
    names.sort();
    assert_eq!(names, ["d1", "d2", "first"]);
    assert!(!dir.path().join("out").exists());

    let (status, r) = post(&c, &format!("{base}/v1/admin/check"), json!({})).await;
    assert_eq!((status, &r), (200, &json!({"ok": true})));
    let (status, r) = post(&c, &format!("{base}/v1/admin/vacuum"), json!({})).await;
    assert_eq!(status, 200, "{r}");
    assert!(r["bytes_freed"].is_u64(), "{r}");

    // Without --backup-dir there is nowhere to write.
    let db = create(dir.path(), "other.strata", |i| i < 10);
    let base = http(service(db, "http://127.0.0.1:9"), None).await;
    let (status, r) = post(&c, &format!("{base}/v1/admin/checkpoint"), json!({"name": "x"})).await;
    assert_eq!(status, 400, "{r}");
}

/// BOOL and TIMESTAMP fields answer as JSON booleans and ISO 8601 text,
/// take them as parameters and in writes, and come back the same from a
/// coordinator over shards reached by gRPC.
#[tokio::test(flavor = "multi_thread")]
async fn bool_and_timestamp_fields_over_http_and_grpc() {
    use strata::schema::{Field, FieldType, Schema};
    let schema = Schema::new("tasks", vec![Field::new("id", FieldType::Keyword), Field::boolean("done"), Field::timestamp("at"), Field::new("n", FieldType::Int)], Some("id")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let docs: Vec<Vec<Value>> = (0..40i64)
        .map(|i| {
            let done = if i % 4 == 3 { Value::Null } else { Value::Bool(i % 2 == 0) };
            vec![Value::Str(format!("t{i:02}")), done, Value::Str(format!("2026-10-{:02}T{:02}:00:00Z", 1 + i % 28, i % 24)), Value::Int(i)]
        })
        .collect();
    let open = |name: &str, keep: &dyn Fn(&Value) -> bool| {
        let opts = DbOptions { publish_snapshots: true, ..DbOptions::default() };
        let db = Db::create(&dir.path().join(name), schema.clone(), opts).unwrap();
        db.load(docs.iter().filter(|d| keep(&d[0])).cloned()).unwrap();
        db
    };
    let svc = |db: Db| Arc::new(Service::new(Backend::Local(Arc::new(db)), schema.clone(), agent_config("http://unused"), 16, Duration::from_secs(10)));

    let base = http(svc(open("whole.strata", &|_| true)), None).await;
    let c = reqwest::Client::new();
    let (query_url, write_url) = (format!("{base}/v1/query"), format!("{base}/v1/write"));
    let query = |sql: &str, params: Json| post(&c, &query_url, json!({"sql": sql, "params": params}));
    let (status, r) = query("SELECT id, done, at FROM tasks WHERE done = :d AND at >= :since ORDER BY n LIMIT 2", json!({"d": true, "since": "2026-10-20"})).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["rows"], json!([["t20", true, "2026-10-21T20:00:00Z"], ["t22", true, "2026-10-23T22:00:00Z"]]));
    let (_, r) = query("SELECT done, COUNT(*), MIN(at) FROM tasks WHERE done IS NOT NULL GROUP BY done", json!({})).await;
    let mut groups = r["rows"].as_array().unwrap().clone();
    groups.sort_by_key(|g| g[0].as_bool());
    assert_eq!(groups, [json!([false, 10, "2026-10-02T01:00:00Z"]), json!([true, 20, "2026-10-01T00:00:00Z"])]);

    // Writes take booleans and ISO 8601 text, and refuse what is neither.
    let write = |ops: Json| post(&c, &write_url, json!({"ops": ops}));
    let (status, r) = write(json!([
        {"op": "insert", "doc": {"id": "x1", "done": true, "at": "2026-11-01T02:00:00+02:00", "n": 100}},
        {"op": "set", "key": "x1", "field": "done", "value": false},
        {"op": "set", "key": "t00", "field": "at", "value": "2026-12-24"},
    ]))
    .await;
    assert_eq!((status, &r["written"]), (200, &json!(3)), "{r}");
    let (_, r) = query("SELECT id, done, at FROM tasks WHERE id IN ('x1', 't00') ORDER BY n", json!({})).await;
    assert_eq!(r["rows"], json!([["t00", true, "2026-12-24T00:00:00Z"], ["x1", false, "2026-11-01T00:00:00Z"]]));
    for bad in [json!({"op": "insert", "doc": {"id": "x2", "at": 1790812800}}), json!({"op": "set", "key": "x1", "field": "done", "value": 3}), json!({"op": "add", "key": "x1", "field": "at", "value": 1})] {
        let (status, r) = write(json!([bad])).await;
        assert_eq!(status, 400, "{bad}: {r}");
    }
    let (status, r) = post(&c, &format!("{base}/v1/sql"), json!({"sql": "UPDATE tasks SET done = TRUE, at = '2026-11-02' WHERE id = 'x1'"})).await;
    assert_eq!((status, &r["written"]), (200, &json!(1)), "{r}");
    let (_, r) = query("SELECT done, at FROM tasks WHERE id = 'x1'", json!({})).await;
    assert_eq!(r["rows"], json!([[true, "2026-11-02T00:00:00Z"]]));

    // A coordinator over two shards answers like the whole database, and
    // its shards carry the stored integers between them.
    let whole = open("whole2.strata", &|_| true);
    let mut urls = Vec::new();
    for s in 0..2 {
        urls.push(grpc(svc(open(&format!("s{s}.strata"), &|k| shard_of(k, 2) == s))).await);
    }
    let coord = Coordinator::connect(urls).await.unwrap();
    let coord = Arc::new(Service::new(Backend::Cluster(Arc::new(coord)), schema.clone(), agent_config("http://unused"), 16, Duration::from_secs(10)));
    for sql in [
        "SELECT done, COUNT(*), MIN(at), MAX(at) FROM tasks GROUP BY done",
        "SELECT id, done, at FROM tasks WHERE at < '2026-10-10' ORDER BY at DESC LIMIT 5",
        "SELECT id, done, at FROM tasks WHERE done IS NOT TRUE AND at >= '2026-10-03T12:00:00+02:00' ORDER BY n LIMIT 6",
    ] {
        let want = js::result(&whole.query(sql, &Params::new()).unwrap(), 0.0);
        let got = js::result(&coord.query(QueryReq { sql: sql.into(), ..Default::default() }).await.unwrap().rs, 0.0);
        let sorted = |j: &Json| {
            let mut rows = j["rows"].as_array().unwrap().clone();
            if sql.contains("GROUP BY") {
                rows.sort_by_key(|r| r.to_string());
            }
            rows
        };
        assert_eq!(sorted(&want), sorted(&got), "{sql}");
        let values: Vec<Json> = sorted(&got).iter().flat_map(|r| r.as_array().unwrap().clone()).collect();
        assert!(values.iter().any(Json::is_boolean) && values.iter().any(|v| v.as_str().is_some_and(|s| s.ends_with('Z'))), "{sql}: {got}");
    }
    // A parameter of each type reaches the shards.
    let params = coord.params(Some(&json!({"d": false, "since": "2026-10-25"}))).await.unwrap();
    let got = coord.query(QueryReq { sql: "SELECT id FROM tasks WHERE done = :d AND at >= :since ORDER BY n".into(), params, ..Default::default() }).await.unwrap();
    let want = whole.query("SELECT id FROM tasks WHERE done = FALSE AND at >= '2026-10-25' ORDER BY n", &Params::new()).unwrap();
    assert_eq!(got.rs.rows, want.rows);
    assert!(!want.rows.is_empty());
    // An INSERT through the coordinator reaches its key's shard as written.
    coord.sql("INSERT INTO tasks (id, done, at, n) VALUES ('y1', TRUE, '2026-10-31T23:59:59.5-03:00', 200)", &Params::new(), 0).await.unwrap();
    let got = coord.query(QueryReq { sql: "SELECT done, at FROM tasks WHERE id = 'y1'".into(), ..Default::default() }).await.unwrap();
    assert_eq!(js::result(&got.rs, 0.0)["rows"], json!([[true, "2026-11-01T02:59:59.500Z"]]));
}

/// NOT NULL and DEFAULT over HTTP writes and through a coordinator: a row
/// that leaves a field out takes its default, and a NOT NULL field left
/// NULL is refused with 400.
#[tokio::test(flavor = "multi_thread")]
async fn defaults_and_not_null_over_http_and_grpc() {
    use strata::schema::Schema;
    let schema = Schema::from_json(
        r#"{"table": "events", "key": "id", "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "kind", "type": "keyword", "not_null": true, "default": "click"},
            {"name": "n", "type": "int", "default": 1},
            {"name": "at", "type": "timestamp", "not_null": true, "default": "now"}
        ]}"#,
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let open = |name: &str| Db::create(&dir.path().join(name), schema.clone(), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    let svc = |db: Db| Arc::new(Service::new(Backend::Local(Arc::new(db)), schema.clone(), agent_config("http://unused"), 16, Duration::from_secs(10)));

    let base = http(svc(open("one.strata")), None).await;
    let c = reqwest::Client::new();
    let (status, r) = c.get(format!("{base}/v1/schema")).send().await.unwrap().json::<Json>().await.map(|j| (200, j)).unwrap();
    assert_eq!((status, &r["short"]["fields"][1]), (200, &json!({"name": "kind", "type": "keyword", "not_null": true, "default": "click"})));
    let (status, r) = post(&c, &format!("{base}/v1/write"), json!({"ops": [{"op": "insert", "doc": {"id": "a"}}, {"op": "insert", "doc": {"id": "b", "kind": "view", "n": null}}]})).await;
    assert_eq!((status, &r["written"]), (200, &json!(2)), "{r}");
    let (_, r) = post(&c, &format!("{base}/v1/query"), json!({"sql": "SELECT id, kind, n FROM events WHERE at <= :t", "params": {"t": "2100-01-01"}})).await;
    let mut got = r["rows"].as_array().cloned().unwrap_or_default();
    got.sort_by_key(|r| r[0].to_string());
    assert_eq!(got, [json!(["a", "click", 1]), json!(["b", "view", null])], "{r}");
    let (_, r) = post(&c, &format!("{base}/v1/query"), json!({"sql": "SELECT COUNT(*), MIN(at), MAX(at) FROM events"})).await;
    assert_eq!(r["rows"][0][0], json!(2), "{r}");
    assert_eq!(r["rows"][0][1], r["rows"][0][2], "one time for the request: {r}");
    for op in [json!({"op": "insert", "doc": {"id": "x", "kind": null}}), json!({"op": "set", "key": "a", "field": "at", "value": null})] {
        let (status, r) = post(&c, &format!("{base}/v1/write"), json!({"ops": [op]})).await;
        assert_eq!(status, 400, "{op}: {r}");
        assert!(r["error"].as_str().unwrap_or("").contains("is NOT NULL"), "{r}");
    }

    // Through a coordinator, an INSERT's defaults are filled in once, so
    // its shards store the same time.
    let mut urls = Vec::new();
    for s in 0..2 {
        urls.push(grpc(svc(open(&format!("s{s}.strata")))).await);
    }
    let coord = Coordinator::connect(urls).await.unwrap();
    let coord = Arc::new(Service::new(Backend::Cluster(Arc::new(coord)), schema.clone(), agent_config("http://unused"), 16, Duration::from_secs(10)));
    let values: Vec<String> = (0..20).map(|i| format!("('k{i}')")).collect();
    coord.sql(&format!("INSERT INTO events (id) VALUES {}", values.join(", ")), &Params::new(), 0).await.unwrap();
    let got = coord.query(QueryReq { sql: "SELECT kind, n, COUNT(*), MIN(at), MAX(at) FROM events GROUP BY kind, n".into(), ..Default::default() }).await.unwrap();
    let rows = js::result(&got.rs, 0.0)["rows"].clone();
    assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
    assert_eq!((&rows[0][0], &rows[0][1], &rows[0][2]), (&json!("click"), &json!(1), &json!(20)));
    assert_eq!(rows[0][3], rows[0][4], "one time for the statement: {rows}");
    assert!(coord.sql("INSERT INTO events (id, kind) VALUES ('y', NULL)", &Params::new(), 0).await.is_err());
}

/// A TEXT field's analyzer over HTTP: the schema names it, and searches
/// of the field analyse their words with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_simple_analyzer_over_http() {
    use strata::schema::Schema;
    let schema = Schema::from_json(
        r#"{"table": "notes", "key": "id", "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "title", "type": "text", "analyzer": "simple"},
            {"name": "body", "type": "text"}
        ]}"#,
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("n.strata"), schema.clone(), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    let svc = Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent_config("http://unused"), 16, Duration::from_secs(10)));
    let base = http(svc, None).await;
    let c = reqwest::Client::new();
    let r = c.get(format!("{base}/v1/schema")).send().await.unwrap().json::<Json>().await.unwrap();
    assert_eq!(r["short"]["fields"][1], json!({"name": "title", "type": "text", "analyzer": "simple"}));
    assert_eq!(r["schema"]["fields"][1]["analyzer"], json!({"name": "simple", "version": 1}));
    let (status, r) = post(&c, &format!("{base}/v1/schema/check"), json!({"table": "t", "fields": [{"name": "t", "type": "text", "analyzer": "klingon"}]})).await;
    assert_eq!(status, 400, "{r}");
    assert!(r["error"].as_str().unwrap_or("").contains("analyzer klingon"), "{r}");

    let doc = |id: &str, t: &str| json!({"op": "insert", "doc": {"id": id, "title": t, "body": t}});
    let (status, r) = post(&c, &format!("{base}/v1/write"), json!({"ops": [doc("a", "The Running Cats"), doc("b", "they run")]})).await;
    assert_eq!(status, 200, "{r}");
    let ids = |r: &Json| {
        let mut v: Vec<String> = r["rows"].as_array().unwrap().iter().map(|x| x[0].as_str().unwrap().to_string()).collect();
        v.sort();
        v
    };
    for (sql, want) in [
        ("SELECT id FROM notes WHERE MATCH(title, 'run')", vec!["b"]),
        ("SELECT id FROM notes WHERE MATCH(body, 'run')", vec!["a", "b"]),
        ("SELECT id FROM notes WHERE MATCH(title, 'the')", vec!["a"]),
        ("SELECT id FROM notes ORDER BY bm25(title, 'the running') DESC LIMIT 1", vec!["a"]),
    ] {
        let (status, r) = post(&c, &format!("{base}/v1/query"), json!({ "sql": sql })).await;
        assert_eq!(status, 200, "{sql}: {r}");
        assert_eq!(ids(&r), want, "{sql}: {r}");
    }
}
