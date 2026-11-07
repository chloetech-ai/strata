//! The hub (`crate::hub`): a server whose command center reaches other
//! clusters. A connection is added once its address answers and takes its
//! key; calls to `/c/<name>/v1/...` go to it with that key, never with the
//! caller's credential; a self-signed certificate is pinned once trusted
//! and another one refused; a key the cluster stops taking is answered as
//! the hub's trouble (502), not as a login the page should ask for.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::auth::{Access, Secret, Tls};
use strata_server::keys::Scope;
use strata_server::service::{AgentConfig, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn service(dir: &std::path::Path, name: &str) -> Arc<Service> {
    let db = Db::create(&dir.join(name), demo::schema(20), DbOptions::default()).unwrap();
    let schema = (*db.schema()).clone();
    Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(20)))
}

async fn serve(svc: Arc<Service>, access: Access) -> (String, u16) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let scheme = if access.tls.is_some() { "https" } else { "http" };
    tokio::spawn(server::serve_http(svc, l, access));
    (format!("{scheme}://127.0.0.1:{port}"), port)
}

fn client() -> reqwest::Client {
    strata_server::http_client().build().unwrap()
}

async fn call(method: &str, url: &str, auth: Option<&str>, body: Option<Json>) -> (u16, Json) {
    let c = client();
    let mut r = match method {
        "GET" => c.get(url),
        "DELETE" => c.delete(url),
        _ => c.post(url).json(&body.unwrap_or(Json::Null)),
    };
    if let Some(a) = auth {
        r = r.bearer_auth(a);
    }
    let r = r.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Json::Null))
}

fn test_tls() -> Tls {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tls");
    let read = |f: &str| std::fs::read(dir.join(f)).unwrap();
    Tls { cert: read("cert.pem"), key: read("key.pem") }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hub_forwards_with_each_connections_key() {
    let dir = tempfile::tempdir().unwrap();
    // The other cluster: a token of its own, and a write key the hub holds.
    let remote = service(dir.path(), "remote.strata");
    let (writer, secret) = remote.keys.create("hub-laptop", Scope::Write, None, true, false).unwrap();
    let (remote_url, _) = serve(remote.clone(), Access { token: Some(Secret::new("remote-root")), ..Access::default() }).await;
    // The hub: open on loopback, as a laptop's server is.
    let hub = service(dir.path(), "hub.strata");
    let (hub_url, _) = serve(hub.clone(), Access::default()).await;

    let (s, j) = call("POST", &format!("{hub_url}/v1/admin/hub/connections"), None, Some(json!({"name": "remote", "url": remote_url, "key": "sk_wrong_key"}))).await;
    assert_eq!(s, 409, "a key the cluster refuses is not kept: {j}");
    let (s, j) = call("POST", &format!("{hub_url}/v1/admin/hub/connections"), None, Some(json!({"name": "Bad Name", "url": remote_url, "key": secret}))).await;
    assert_eq!(s, 400, "{j}");
    let (s, j) = call("POST", &format!("{hub_url}/v1/admin/hub/connections"), None, Some(json!({"name": "remote", "url": remote_url, "key": secret}))).await;
    assert_eq!(s, 201, "{j}");
    assert_eq!((j["key"]["scope"].as_str(), j["key"]["id"].as_str()), (Some("write"), Some(writer.id.as_str())), "{j}");
    let (_, book) = call("GET", &format!("{hub_url}/v1/admin/hub"), None, None).await;
    assert_eq!(book["connections"][0]["name"], "remote");
    assert!(!book.to_string().contains(&secret), "the book holds no key: {book}");

    // Calls through the hub carry the connection's key: a read, a write.
    let (s, j) = call("GET", &format!("{hub_url}/c/remote/v1/stats"), None, None).await;
    assert_eq!(s, 200, "{j}");
    let (s, j) = call("POST", &format!("{hub_url}/c/remote/v1/write"), None, Some(json!({"ops": [{"op": "upsert", "doc": {"id": "hub-1", "title": "from the hub"}}]}))).await;
    assert_eq!(s, 200, "{j}");
    let (s, j) = call("POST", &format!("{remote_url}/v1/query"), Some("remote-root"), Some(json!({"sql": "SELECT title FROM products WHERE id = 'hub-1'"}))).await;
    assert_eq!((s, &j["rows"]), (200, &json!([["from the hub"]])), "{j}");
    // An admin route is past the key's scope there: the cluster says so.
    let (s, j) = call("GET", &format!("{hub_url}/c/remote/v1/admin/keys"), None, None).await;
    assert_eq!(s, 502, "a refused call is the hub's trouble, not a login: {j}");
    assert!(j["error"].as_str().unwrap().contains("refused this hub's key"), "{j}");
    // Only Strata's API is forwarded, and only to connections in the book.
    assert_eq!(call("GET", &format!("{hub_url}/c/remote/etc/passwd"), None, None).await.0, 404);
    assert_eq!(call("GET", &format!("{hub_url}/c/nobody/v1/stats"), None, None).await.0, 404);
    // The connection's page is the command center.
    let page = client().get(format!("{hub_url}/c/remote")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let main_js = strata_server::ui::tag().map_or("/ui/main.js".to_string(), |t| format!("/ui/{t}/main.js"));
    assert!(page.text().await.unwrap().contains(&main_js));

    // A page on another site cannot drive the hub's connections.
    let r = client().post(format!("{hub_url}/c/remote/v1/write")).header("origin", "https://evil.example").json(&json!({"ops": []})).send().await.unwrap();
    assert_eq!(r.status(), 403);

    // The key revoked on the cluster: calls answer 502 and say so.
    remote.keys.revoke(&writer.id, true).unwrap();
    let (s, j) = call("GET", &format!("{hub_url}/c/remote/v1/stats"), None, None).await;
    assert_eq!(s, 502, "{j}");
    let (s, _) = call("POST", &format!("{hub_url}/v1/admin/hub/connections/remote/check"), None, Some(json!({}))).await;
    assert_eq!(s, 409);
    let (_, book) = call("GET", &format!("{hub_url}/v1/admin/hub"), None, None).await;
    assert!(book["connections"][0]["last_error"].as_str().is_some(), "{book}");

    // Out of the book: its calls are not forwarded any more.
    assert_eq!(call("DELETE", &format!("{hub_url}/v1/admin/hub/connections/remote"), None, None).await.0, 200);
    assert_eq!(call("GET", &format!("{hub_url}/c/remote/v1/stats"), None, None).await.0, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_self_signed_certificate_is_pinned_once_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let remote = service(dir.path(), "remote.strata");
    let (remote_url, port) = serve(remote, Access { token: Some(Secret::new("remote-root")), tls: Some(test_tls()), ..Access::default() }).await;
    let hub = service(dir.path(), "hub.strata");
    let (hub_url, _) = serve(hub, Access::default()).await;
    let add = |trust: Option<String>| {
        let mut b = json!({"name": "tls", "url": remote_url, "key": "remote-root"});
        if let Some(t) = trust {
            b["trust"] = json!(t);
        }
        b
    };
    // Its certificate is signed by no authority this machine knows.
    let (s, j) = call("POST", &format!("{hub_url}/v1/admin/hub/connections"), None, Some(add(None))).await;
    assert_eq!(s, 409, "{j}");
    assert_eq!(j["needs_trust"], true);
    let fp = j["fingerprint"].as_str().unwrap().to_string();
    assert_eq!(fp.len(), 64);
    let (s, _) = call("POST", &format!("{hub_url}/v1/admin/hub/connections"), None, Some(add(Some("00".repeat(32))))).await;
    assert_eq!(s, 409, "another fingerprint is not this certificate's");
    let (s, j) = call("POST", &format!("{hub_url}/v1/admin/hub/connections"), None, Some(add(Some(fp.clone())))).await;
    assert_eq!(s, 201, "{j}");
    assert_eq!(j["pin"].as_str(), Some(fp.as_str()));
    let (s, j) = call("GET", &format!("{hub_url}/c/tls/v1/stats"), None, None).await;
    assert_eq!(s, 200, "{j}");

    // Plain HTTP beyond loopback needs saying so.
    let (s, j) = call("POST", &format!("{hub_url}/v1/admin/hub/connections"), None, Some(json!({"name": "plain", "url": format!("http://10.255.255.1:{port}"), "key": "k"}))).await;
    assert_eq!(s, 409, "{j}");
    assert!(j["error"].as_str().unwrap().contains("plain HTTP"), "{j}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tables_are_copied_between_this_server_and_a_connection() {
    use strata::schema::{Field, FieldType, Schema};
    let dir = tempfile::tempdir().unwrap();
    // This server: products, kept by brand, with rows.
    let mut products = demo::schema(30);
    if let Some(f) = products.fields.iter_mut().find(|f| f.name == "brand") {
        *f = f.clone().not_null();
    }
    let db = Db::create(&dir.path().join("hub.strata"), products.with_shard_by("brand").unwrap(), DbOptions::default()).unwrap();
    db.load((0..30).map(|i| demo::product(i, 7))).unwrap();
    let schema = (*db.schema()).clone();
    let hub = Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(20)));
    let (hub_url, _) = serve(hub.clone(), Access::default()).await;
    // The other server: only notes, a note in it, and an admin key the
    // hub holds (a copy may make tables there).
    let notes = Schema::new("notes", vec![Field::new("id", FieldType::Keyword), Field::new("text", FieldType::Text { positions: false })], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("remote.strata"), notes, DbOptions::default()).unwrap();
    let schema = (*db.schema()).clone();
    let remote = Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(20)));
    let (_, secret) = remote.keys.create("hub-laptop", Scope::Admin, None, true, false).unwrap();
    let (remote_url, _) = serve(remote.clone(), Access { token: Some(Secret::new("remote-root")), ..Access::default() }).await;
    let (s, j) = call("POST", &format!("{hub_url}/v1/admin/hub/connections"), None, Some(json!({"name": "remote", "url": remote_url, "key": secret}))).await;
    assert_eq!(s, 201, "{j}");
    let (s, j) = call("POST", &format!("{remote_url}/v1/write"), Some("remote-root"), Some(json!({"ops": [{"op": "upsert", "table": "notes", "doc": {"id": "n1", "text": "a note"}}]}))).await;
    assert_eq!(s, 200, "{j}");

    // Here to there: the table is made with its schema, then its rows.
    let copy_url = format!("{hub_url}/v1/admin/hub/copy");
    let copy = |b: Json| call("POST", &copy_url, None, Some(b));
    let (s, j) = copy(json!({"from": "", "to": "remote", "tables": ["products"]})).await;
    assert_eq!(s, 200, "{j}");
    assert_eq!((j["written"].as_u64(), j["from"].as_str(), j["to"].as_str()), (Some(30), Some("this server"), Some("remote")), "{j}");
    let (s, j) = call("POST", &format!("{remote_url}/v1/query"), Some("remote-root"), Some(json!({"sql": "SELECT count(*) FROM products"}))).await;
    assert_eq!((s, &j["rows"]), (200, &json!([[30]])), "{j}");
    let (_, tables) = call("GET", &format!("{remote_url}/v1/tables"), Some("remote-root"), None).await;
    let products = tables["tables"].as_array().unwrap().iter().find(|t| t["table"] == "products").cloned().unwrap_or_default();
    assert_eq!(products["shard_by"], "brand", "the shard key went with it: {products}");
    // Again: the same rows by their keys, no more.
    let (s, j) = copy(json!({"from": "", "to": "remote", "tables": ["products"]})).await;
    assert_eq!((s, j["written"].as_u64()), (200, Some(30)), "{j}");
    let (_, j) = call("POST", &format!("{remote_url}/v1/query"), Some("remote-root"), Some(json!({"sql": "SELECT count(*) FROM products"}))).await;
    assert_eq!(j["rows"], json!([[30]]));

    // There to here.
    let (s, j) = copy(json!({"from": "remote", "to": "", "tables": ["notes"]})).await;
    assert_eq!((s, j["written"].as_u64()), (200, Some(1)), "{j}");
    let (_, j) = call("POST", &format!("{hub_url}/v1/query"), None, Some(json!({"sql": "SELECT text FROM notes WHERE id = 'n1'"}))).await;
    assert_eq!(j["rows"], json!([["a note"]]), "{j}");

    // What is not a copy.
    assert_eq!(copy(json!({"from": "remote", "to": "remote", "tables": ["notes"]})).await.0, 400);
    assert_eq!(copy(json!({"from": "", "to": "remote", "tables": []})).await.0, 400);
    assert_eq!(copy(json!({"from": "", "to": "nobody", "tables": ["products"]})).await.0, 404);
    let (s, j) = copy(json!({"from": "", "to": "remote", "tables": ["products; DROP"]})).await;
    assert_eq!(s, 400, "{j}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_follows_another_only_where_the_operator_runs_its_follow_process() {
    let dir = tempfile::tempdir().unwrap();
    // Two clusters of their own (no operator), each with an admin key the hub holds.
    let mut ends = Vec::new();
    for name in ["east", "west"] {
        let svc = service(dir.path(), &format!("{name}.strata"));
        let (_, secret) = svc.keys.create("hub-laptop", Scope::Admin, None, true, false).unwrap();
        let (url, _) = serve(svc, Access { token: Some(Secret::new(format!("{name}-root"))), ..Access::default() }).await;
        ends.push((name, url, secret));
    }
    let hub = service(dir.path(), "hub.strata");
    let (hub_url, _) = serve(hub, Access::default()).await;
    for (name, url, secret) in &ends {
        let (s, j) = call("POST", &format!("{hub_url}/v1/admin/hub/connections"), None, Some(json!({"name": name, "url": url, "key": secret}))).await;
        assert_eq!(s, 201, "{j}");
    }
    let follow_url = format!("{hub_url}/v1/admin/hub/follow");
    let follow = |b: Json| call("POST", &follow_url, None, Some(b));

    // The plan says what it would do, and why it cannot here.
    let (s, plan) = follow(json!({"follower": "west", "source": "east", "plan": true})).await;
    assert_eq!(s, 200, "{plan}");
    assert_eq!((plan["name"].as_str(), plan["start"].as_str(), plan["hub_writes_secret"].as_bool()), (Some("east"), Some("follow"), Some(true)), "{plan}");
    let refused = plan["refused"].as_str().unwrap_or_default();
    for says in ["west is not run by the Kubernetes operator", "an address of this machine", "cannot keep the source's key there"] {
        assert!(refused.contains(says), "{says}: {refused}");
    }
    assert!(plan["steps"].as_array().is_some_and(|s| s.len() == 4), "{plan}");
    // A source URL given, and a Secret made already: only the operator is missing.
    let (_, plan) = follow(json!({"follower": "west", "source": "east", "plan": true, "source_url": "https://east.example:7700/", "key_secret": "east-feed", "start": "copy"})).await;
    assert_eq!((plan["source_url"].as_str(), plan["source_url_from"].as_str(), plan["hub_writes_secret"].as_bool()), (Some("https://east.example:7700"), Some("as given"), Some(false)), "{plan}");
    assert!(plan["refused"].as_str().is_some_and(|r| r.contains("not run by the Kubernetes operator") && !r.contains("address")), "{plan}");
    // Without "plan", nothing is made: 409 with the same reasons, and no key on east.
    let (s, j) = follow(json!({"follower": "west", "source": "east"})).await;
    assert_eq!(s, 409, "{j}");
    assert!(j["error"].as_str().unwrap_or_default().contains("not run by the Kubernetes operator"), "{j}");
    let (_, keys) = call("GET", &format!("{}/v1/admin/keys", ends[0].1), Some("east-root"), None).await;
    assert!(!keys.to_string().contains("follow west"), "no feed key was made: {keys}");
    // What is not a follow.
    assert_eq!(follow(json!({"follower": "west", "source": "west"})).await.0, 400);
    assert_eq!(follow(json!({"follower": "west", "source": "nobody", "plan": true})).await.0, 404);
    assert_eq!(follow(json!({"follower": "west", "source": "east", "name": "east coast", "plan": true})).await.0, 400);

    // Through the hub, with its admin key: pausing needs the operator, and
    // stopping names a cluster this one followed.
    let (s, j) = call("POST", &format!("{hub_url}/c/west/v1/admin/follow/pause"), None, Some(json!({"name": "east"}))).await;
    assert_eq!(s, 409, "{j}");
    assert!(j["error"].as_str().unwrap_or_default().contains("not this cluster's operator's"), "{j}");
    let (s, j) = call("POST", &format!("{hub_url}/c/west/v1/admin/follow/stop"), None, Some(json!({"name": "east"}))).await;
    assert_eq!(s, 404, "an admin key may stop following, of a cluster this one followed: {j}");
}
