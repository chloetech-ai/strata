//! API keys and their scopes over HTTP and gRPC, the audit log, and
//! alert rules with their webhook.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::alerts::{Alerts, Rule};
use strata_server::auth::{Access, Secret};
use strata_server::keys::Keys;
use strata_server::service::{AgentConfig, Backend, Service};
use strata_server::{demo, pb, server};
use tokio::net::TcpListener;

const N: usize = 300;

fn service(dir: &std::path::Path) -> Service {
    let db = Db::create(&dir.join("a.strata"), demo::schema(N), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).map(|i| demo::product(i, 7))).unwrap();
    let schema = db.schema().clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent, 16, Duration::from_secs(10));
    svc.keys = Arc::new(Keys::open(dir.join("a.strata.keys.json")).unwrap());
    svc
}

async fn listen() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    (l, url)
}

/// Sends `method path body` with `auth` as the bearer credential; the
/// status and the answer.
async fn call(base: &str, method: &str, path: &str, auth: Option<&str>, body: Option<Json>) -> (u16, Json) {
    let c = reqwest::Client::new();
    let mut r = match method {
        "GET" => c.get(format!("{base}{path}")),
        "DELETE" => c.delete(format!("{base}{path}")),
        "PUT" => c.put(format!("{base}{path}")),
        _ => c.post(format!("{base}{path}")),
    };
    if let Some(a) = auth {
        r = r.bearer_auth(a);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Json::Null))
}

async fn new_key(base: &str, auth: Option<&str>, name: &str, scope: &str) -> String {
    let (status, j) = call(base, "POST", "/v1/admin/keys", auth, Some(json!({"name": name, "scope": scope}))).await;
    assert_eq!(status, 200, "{j}");
    assert!(j["secret"].as_str().unwrap().starts_with("sk_"));
    j["secret"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_have_scopes_and_the_audit_log_keeps_what_they_did() {
    let dir = tempfile::tempdir().unwrap();
    let svc = Arc::new(service(dir.path()));
    let (l, base) = listen().await;
    let access = Access { token: Some(Secret::new("root-token")), ..Default::default() };
    tokio::spawn(server::serve_http(svc.clone(), l, access.clone()));
    let root = Some("root-token");
    let select = json!({"sql": "SELECT id FROM products WHERE MATCH(body, 'laptop') LIMIT 3"});
    let insert = json!({"sql": "INSERT INTO products (id, title, category, price, sold) VALUES ('new-1', 'A desk lamp', 'home', 20.0, 3)"});
    let create = json!({"sql": "CREATE TABLE notes (id KEYWORD PRIMARY KEY, body TEXT)"});

    assert_eq!(call(&base, "POST", "/v1/query", None, Some(select.clone())).await.0, 401);
    let reader = new_key(&base, root, "dashboards", "read").await;
    let writer = new_key(&base, root, "ingest", "write").await;
    let (r, w) = (Some(reader.as_str()), Some(writer.as_str()));

    // read: queries, not writes nor the admin's routes.
    assert_eq!(call(&base, "GET", "/v1/whoami", r, None).await.1["scope"], "read");
    assert_eq!(call(&base, "POST", "/v1/query", r, Some(select.clone())).await.0, 200);
    assert_eq!(call(&base, "POST", "/v1/sql", r, Some(select.clone())).await.0, 200);
    assert_eq!(call(&base, "GET", "/v1/timeseries", r, None).await.0, 200);
    let (status, j) = call(&base, "POST", "/v1/sql", r, Some(insert.clone())).await;
    assert_eq!(status, 403, "{j}");
    assert!(j["error"].as_str().unwrap().contains("write scope"), "{j}");
    assert_eq!(call(&base, "POST", "/v1/write", r, Some(json!({"ops": []}))).await.0, 403);
    for (m, path) in [("POST", "/v1/admin/vacuum"), ("GET", "/v1/queries"), ("GET", "/v1/logs"), ("GET", "/v1/admin/keys"), ("GET", "/v1/export"), ("GET", "/v1/profile/settings"), ("POST", "/v1/profile/cpu"), ("GET", "/v1/profile/memory"), ("POST", "/v1/profile/heap"), ("GET", "/v1/profile/threads")] {
        assert_eq!(call(&base, m, path, r, None).await.0, 403, "{path}");
    }
    // write: rows, not tables.
    assert_eq!(call(&base, "POST", "/v1/sql", w, Some(insert)).await.0, 200);
    assert_eq!(call(&base, "POST", "/v1/sql", w, Some(create.clone())).await.0, 403);
    assert_eq!(call(&base, "POST", "/v1/admin/keys", w, Some(json!({"name": "x", "scope": "admin"}))).await.0, 403);
    assert_eq!(call(&base, "POST", "/v1/profile/cpu", w, Some(json!({"seconds": 0.1}))).await.0, 403);
    assert_eq!(call(&base, "POST", "/v1/profile/heap", w, Some(json!({"seconds": 0.1}))).await.0, 403);
    // The token may do it all; a table's statement is audited.
    assert_eq!(call(&base, "POST", "/v1/sql", root, Some(create)).await.0, 200);

    let (status, audit) = call(&base, "GET", "/v1/admin/audit", root, None).await;
    assert_eq!(status, 200);
    let entries = audit["entries"].as_array().unwrap();
    let find = |who: &str, route: &str, status: u64| entries.iter().any(|e| e["who"] == who && e["route"] == route && e["status"] == status);
    assert!(find("token", "/v1/sql", 200), "{audit}");
    assert!(entries.iter().any(|e| e["detail"].as_str().is_some_and(|d| d.starts_with("CREATE TABLE notes"))), "{audit}");
    assert!(find("dashboards", "/v1/admin/vacuum", 403), "{audit}");
    assert!(find("dashboards", "/v1/sql", 403), "{audit}");
    assert!(find("ingest", "/v1/admin/keys", 403), "{audit}");
    assert!(find("unknown", "/v1/query", 401), "{audit}");
    assert!(find("token", "/v1/admin/keys", 200), "{audit}");
    // Plain reads and writes are not audited.
    assert!(!entries.iter().any(|e| e["route"] == "/v1/query" && e["status"] == 200), "{audit}");

    // The keys, without secrets; revoked, a key is refused.
    let (_, keys) = call(&base, "GET", "/v1/admin/keys", root, None).await;
    let list = keys["keys"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert!(!keys.to_string().contains(&reader[12..]));
    let id = list.iter().find(|k| k["name"] == "dashboards").unwrap()["id"].as_str().unwrap().to_string();
    assert_eq!(call(&base, "DELETE", &format!("/v1/admin/keys/{id}"), root, None).await.0, 200);
    assert_eq!(call(&base, "POST", "/v1/query", r, Some(select)).await.0, 401);

    // gRPC: keys call by their scope; the internal calls need the token.
    let (gl, gurl) = listen().await;
    tokio::spawn(server::serve_grpc(svc.clone(), gl, access));
    let admin = new_key(&base, root, "ops", "admin").await;
    let client = |auth: String| {
        let url = gurl.clone();
        async move {
            let ch = tonic::transport::Channel::from_shared(url).unwrap().connect().await.unwrap();
            pb::strata_client::StrataClient::with_interceptor(ch, move |mut req: tonic::Request<()>| {
                req.metadata_mut().insert("authorization", format!("Bearer {auth}").parse().unwrap());
                Ok(req)
            })
        }
    };
    let mut ops = client(admin.clone()).await;
    assert!(ops.schema(pb::Empty {}).await.is_ok());
    let e = ops.replicate(pb::ReplicateRequest::default()).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied, "{e}");
    let e = ops.slots(pb::SlotsRequest::default()).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied, "{e}");
    // The token gets past the check (and the server then refuses a bad request).
    let mut rootc = client("root-token".into()).await;
    let e = rootc.slots(pb::SlotsRequest::default()).await.unwrap_err();
    assert_ne!(e.code(), tonic::Code::PermissionDenied, "{e}");
    let mut wr = client(writer.clone()).await;
    assert!(wr.write(pb::WriteRequest::default()).await.map_or_else(|e| e.code() != tonic::Code::PermissionDenied, |_| true));
    let read2 = new_key(&base, root, "reader2", "read").await;
    let mut rd = client(read2).await;
    assert_eq!(rd.write(pb::WriteRequest::default()).await.unwrap_err().code(), tonic::Code::PermissionDenied);
    assert!(rd.health(pb::Empty {}).await.is_ok());
    // A profile, as over HTTP, needs an admin key (or the token).
    let threads = || pb::ProfileRequest { kind: "threads".into(), ..Default::default() };
    assert_eq!(rd.profile(threads()).await.unwrap_err().code(), tonic::Code::PermissionDenied);
    assert_eq!(wr.profile(threads()).await.unwrap_err().code(), tonic::Code::PermissionDenied);
    let j: Json = serde_json::from_slice(&ops.profile(threads()).await.unwrap().into_inner().body).unwrap();
    assert!(j["threads"].is_array(), "{j}");
    assert!(rootc.profile(pb::ProfileRequest { kind: "memory".into(), ..Default::default() }).await.is_ok());
    assert_eq!(ops.profile(pb::ProfileRequest { kind: "disk".into(), ..Default::default() }).await.unwrap_err().code(), tonic::Code::InvalidArgument);
    // An admin's profile is audited, as over HTTP, and so is a refused one.
    let (_, audit) = call(&base, "GET", "/v1/admin/audit", root, None).await;
    let entries = audit["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["method"] == "gRPC" && e["route"] == "Profile" && e["who"] == "ops" && e["status"] == 200 && e["detail"] == "a threads profile"), "{audit}");
    assert!(entries.iter().any(|e| e["method"] == "gRPC" && e["route"] == "Profile" && e["status"] == 403), "{audit}");
    let mut nobody = client("sk_00000000_ff".into()).await;
    assert_eq!(nobody.health(pb::Empty {}).await.unwrap_err().code(), tonic::Code::Unauthenticated);
}

/// What a request does needs its scope, over HTTP and gRPC alike: the
/// three ways past the key scopes the review of 31 October found (OPS-N1:
/// BUG-N1, BUG-N2, BUG-N3) are closed, and each refusal is audited.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_s_statements_and_options_need_their_scope_over_http_and_grpc() {
    let dir = tempfile::tempdir().unwrap();
    let svc = Arc::new(service(dir.path()));
    let (l, base) = listen().await;
    let access = Access { token: Some(Secret::new("root-token")), ..Default::default() };
    tokio::spawn(server::serve_http(svc.clone(), l, access.clone()));
    let (gl, gurl) = listen().await;
    tokio::spawn(server::serve_grpc(svc.clone(), gl, access));
    let root = Some("root-token");
    let reader = new_key(&base, root, "dashboards", "read").await;
    let writer = new_key(&base, root, "ingest", "write").await;
    let admin = new_key(&base, root, "ops", "admin").await;
    let (r, w, a) = (Some(reader.as_str()), Some(writer.as_str()), Some(admin.as_str()));
    let client = |auth: String| {
        let url = gurl.clone();
        async move {
            let ch = tonic::transport::Channel::from_shared(url).unwrap().connect().await.unwrap();
            pb::strata_client::StrataClient::with_interceptor(ch, move |mut req: tonic::Request<()>| {
                req.metadata_mut().insert("authorization", format!("Bearer {auth}").parse().unwrap());
                Ok(req)
            })
        }
    };
    let tables = || svc.catalog().iter().map(|t| t.table.clone()).collect::<Vec<_>>();

    // BUG-N1: a write key's table and rank-profile statements over gRPC
    // Write are refused, as over HTTP; an admin key's are run and audited.
    let sql = |s: &str| pb::WriteRequest { sql: s.into(), ..Default::default() };
    let mut wr = client(writer.clone()).await;
    for s in [
        "CREATE TABLE notes (id KEYWORD PRIMARY KEY, body TEXT)",
        "DROP TABLE products",
        "CREATE RANK PROFILE cheap ON products AS SELECT * FROM products ORDER BY price ASC",
    ] {
        let e = wr.write(sql(s)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "{s}: {e}");
        assert!(e.message().contains("admin scope"), "{s}: {e}");
    }
    assert_eq!(tables(), vec!["products".to_string()]);
    assert!(wr.write(sql("INSERT INTO products (id, title, category, price, sold) VALUES ('g-1', 'A lamp', 'home', 9.0, 1)")).await.is_ok());
    let mut ops = client(admin.clone()).await;
    ops.write(sql("CREATE TABLE notes (id KEYWORD PRIMARY KEY, body TEXT)")).await.unwrap();
    assert!(tables().contains(&"notes".to_string()));

    // BUG-N2: a read key does not write the rank log, through /v1/sql nor
    // /v1/query; a write key logs to a log table that exists, and only an
    // admin key's first logged query creates it.
    let logged = json!({"sql": "SELECT id FROM products ORDER BY bm25(body, 'tent') DESC LIMIT 3", "log": true});
    for route in ["/v1/sql", "/v1/query"] {
        let (status, j) = call(&base, "POST", route, r, Some(logged.clone())).await;
        assert_eq!(status, 403, "{route}: {j}");
        assert!(j["error"].as_str().unwrap().contains("write scope"), "{j}");
    }
    let (status, j) = call(&base, "POST", "/v1/sql", w, Some(logged.clone())).await;
    assert_eq!(status, 403, "{j}");
    assert!(j["error"].as_str().unwrap().contains("creates the table"), "{j}");
    assert!(!tables().contains(&"rank_log".to_string()));
    let (status, j) = call(&base, "POST", "/v1/query", a, Some(logged.clone())).await;
    assert_eq!(status, 200, "{j}");
    let (status, j) = call(&base, "POST", "/v1/sql", w, Some(logged.clone())).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["logged"]["rows"], 3, "{j}");

    // BUG-N3: a write key's SQLite import that creates a table is refused
    // before anything is written; into tables that exist, it imports.
    let src = Db::create(&dir.path().join("src.strata"), demo::schema(N), DbOptions::default()).unwrap();
    src.load((0..3).map(|i| demo::product(i, 7))).unwrap();
    src.sql("CREATE TABLE extra (id INT PRIMARY KEY, body TEXT)", &strata::query::Params::new()).unwrap();
    src.sql("INSERT INTO extra (id, body) VALUES (1, 'one')", &strata::query::Params::new()).unwrap();
    let file = dir.path().join("src.sqlite");
    strata::sqlite::export(&src, &file, &Default::default()).unwrap();
    let bytes = std::fs::read(&file).unwrap();
    let import = |auth: &str, q: &str| {
        let (base, bytes, auth, q) = (base.clone(), bytes.clone(), auth.to_string(), q.to_string());
        async move {
            let r = reqwest::Client::new().post(format!("{base}/v1/import?format=sqlite{q}")).bearer_auth(auth).header("content-type", "application/vnd.sqlite3").body(bytes).send().await.unwrap();
            (r.status().as_u16(), r.json::<Json>().await.unwrap())
        }
    };
    let docs = || svc.local_db().unwrap().snapshot().n_live_docs();
    let before = docs();
    let (status, j) = import(&writer, "").await;
    assert_eq!(status, 403, "{j}");
    assert!(j["error"].as_str().unwrap().contains("creates tables (extra)"), "{j}");
    assert!(!tables().contains(&"extra".to_string()));
    assert_eq!(docs(), before, "nothing was written");
    // Its plan writes nothing, so a write key may ask for it.
    assert_eq!(import(&writer, "&plan=true").await.0, 200);
    let (status, j) = import(&writer, "&tables=products").await;
    assert_eq!(status, 200, "{j}");
    let (status, j) = import(&admin, "").await;
    assert_eq!(status, 200, "{j}");
    assert!(tables().contains(&"extra".to_string()));

    // Each refusal and each admin statement is in the audit log, for gRPC
    // as for HTTP.
    let (_, audit) = call(&base, "GET", "/v1/admin/audit?limit=2000", root, None).await;
    let entries = audit["entries"].as_array().unwrap();
    let find = |who: &str, method: &str, route: &str, status: u64| entries.iter().any(|e| e["who"] == who && e["method"] == method && e["route"] == route && e["status"] == status);
    assert!(find("ingest", "gRPC", "Write", 403), "{audit}");
    assert!(find("ops", "gRPC", "Write", 200), "{audit}");
    assert!(entries.iter().any(|e| e["method"] == "gRPC" && e["detail"].as_str().is_some_and(|d| d.starts_with("CREATE TABLE notes"))), "{audit}");
    assert!(find("dashboards", "POST", "/v1/sql", 403) && find("dashboards", "POST", "/v1/query", 403), "{audit}");
    assert!(entries.iter().any(|e| e["who"] == "ops" && e["route"] == "/v1/query" && e["detail"].as_str().is_some_and(|d| d.starts_with("CREATE TABLE IF NOT EXISTS rank_log"))), "{audit}");
    assert!(find("ingest", "POST", "/v1/import", 403), "{audit}");
    assert!(entries.iter().any(|e| e["who"] == "ops" && e["detail"] == "SQLite import created tables extra"), "{audit}");
    // Plain reads and writes are not.
    assert!(!entries.iter().any(|e| e["route"] == "Write" && e["who"] == "ingest" && e["status"] == 200), "{audit}");
    let mut nobody = client("sk_00000000_ff".into()).await;
    assert_eq!(nobody.health(pb::Empty {}).await.unwrap_err().code(), tonic::Code::Unauthenticated);
    let (_, audit) = call(&base, "GET", "/v1/admin/audit", root, None).await;
    assert!(audit["entries"].as_array().unwrap().iter().any(|e| e["method"] == "gRPC" && e["status"] == 401), "{audit}");
}

/// A server without a token keeps an admin key that never expires, so the
/// last one expiring cannot lock everyone out (BUG-N5).
#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_a_token_keeps_an_admin_key_that_never_expires() {
    let dir = tempfile::tempdir().unwrap();
    let svc = Arc::new(service(dir.path()));
    let (l, base) = listen().await;
    tokio::spawn(server::serve_http(svc.clone(), l, Default::default()));
    let (status, j) = call(&base, "POST", "/v1/admin/keys", None, Some(json!({"name": "me", "scope": "admin", "expires_days": 1}))).await;
    assert_eq!(status, 400, "{j}");
    assert!(j["error"].as_str().unwrap().contains("never expire"), "{j}");
    assert_eq!(call(&base, "GET", "/v1/whoami", None, None).await.1["name"], "open", "still open");
    let admin = new_key(&base, None, "me", "admin").await;
    let (status, j) = call(&base, "POST", "/v1/admin/keys", Some(&admin), Some(json!({"name": "brief", "scope": "admin", "expires_days": 1}))).await;
    assert_eq!(status, 200, "{j}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_server_s_first_key_closes_it() {
    let dir = tempfile::tempdir().unwrap();
    let svc = Arc::new(service(dir.path()));
    let (l, base) = listen().await;
    tokio::spawn(server::serve_http(svc.clone(), l, Default::default()));
    let (_, me) = call(&base, "GET", "/v1/whoami", None, None).await;
    assert_eq!((me["name"].as_str(), me["auth"]["open"].as_bool()), (Some("open"), Some(true)));
    // Its first key is an admin key.
    assert_eq!(call(&base, "POST", "/v1/admin/keys", None, Some(json!({"name": "r", "scope": "read"}))).await.0, 400);
    let (status, j) = call(&base, "POST", "/v1/admin/keys", None, Some(json!({"name": "me", "scope": "admin"}))).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(j["closed"], true);
    let admin = j["secret"].as_str().unwrap().to_string();
    assert_eq!(call(&base, "GET", "/v1/stats", None, None).await.0, 401);
    assert_eq!(call(&base, "GET", "/v1/stats", Some(&admin), None).await.0, 200);
    // The UI and health stay open.
    assert_eq!(reqwest::get(format!("{base}/v1/health")).await.unwrap().status(), 200);
    // Its last admin key cannot be revoked: no one could manage it.
    let id = j["key"]["id"].as_str().unwrap();
    assert_eq!(call(&base, "DELETE", &format!("/v1/admin/keys/{id}"), Some(&admin), None).await.0, 409);
    // The keys file keeps it.
    let again = Keys::open(dir.path().join("a.strata.keys.json")).unwrap();
    assert!(again.resolve(None, Some(&format!("Bearer {admin}"))).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn alert_rules_fire_on_points_and_post_to_the_webhook() {
    // A webhook that keeps what it is sent.
    let got: Arc<Mutex<Vec<Json>>> = Arc::default();
    let keep = got.clone();
    let hook = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(j): axum::Json<Json>| {
            let keep = keep.clone();
            async move {
                keep.lock().unwrap().push(j);
                "ok"
            }
        }),
    );
    let (hl, hurl) = listen().await;
    tokio::spawn(async move { axum::serve(hl, hook).await });

    let dir = tempfile::tempdir().unwrap();
    let mut svc = service(dir.path());
    svc.alerts = Alerts::new(Some(dir.path().join("alerts.json")), Some(format!("{hurl}/hook"))).unwrap();
    let svc = Arc::new(svc);
    let (l, base) = listen().await;
    tokio::spawn(server::serve_http(svc.clone(), l, Default::default()));

    let (_, a) = call(&base, "GET", "/v1/alerts", None, None).await;
    assert!(a["rules"].as_array().unwrap().iter().any(|r| r["id"] == "failing"), "{a}");
    let rule = Rule { id: "rows".into(), name: "Many rows".into(), metric: "docs".into(), op: ">".into(), threshold: 10.0, for_s: 0, severity: "critical".into(), enabled: true, description: String::new() };
    let (status, j) = call(&base, "PUT", "/v1/alerts/rules", None, Some(json!({"rules": [rule]}))).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(call(&base, "PUT", "/v1/alerts/rules", None, Some(json!({"rules": [{"id": "x", "name": "x", "metric": "docs", "op": "~", "threshold": 1}]}))).await.0, 400);

    svc.series.add(svc.sample());
    let p = svc.series.add(svc.sample()).unwrap();
    let changes = svc.alerts.check(&p);
    assert_eq!(changes.len(), 1);
    svc.alerts.notify(&changes).await;
    let (_, a) = call(&base, "GET", "/v1/alerts", None, None).await;
    assert_eq!(a["firing"][0]["id"], "rows", "{a}");
    assert_eq!(a["events"][0]["state"], "firing");
    let sent = got.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0]["text"].as_str().unwrap().contains("Many rows"), "{sent:?}");
    let metrics = reqwest::get(format!("{base}/metrics")).await.unwrap().text().await.unwrap();
    assert!(metrics.contains("strata_alerts_firing{severity=\"critical\"} 1"), "{metrics}");
    // Kept in the file.
    assert_eq!(Alerts::new(Some(dir.path().join("alerts.json")), None).unwrap().rules()[0].id, "rows");
}
