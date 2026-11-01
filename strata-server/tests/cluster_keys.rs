//! Keys that hold across a cluster ([`strata_server::keysync`]): a key
//! made on a coordinator reaches every server of every shard, a
//! coordinator started again takes its cluster's keys back from the
//! shards, a revocation spreads, a key made on a shard reaches the
//! coordinators, and a primary passes its keys to its replicas. The calls
//! between servers take only the cluster's token, which clients never
//! hold: not a key, and not the admin's `--token`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::auth::{Access, Secret};
use strata_server::coord::{CoordOptions, Coordinator};
use strata_server::keys::{KeySet, Keys, Scope};
use strata_server::replication::{self, Acks, Follow, Primary, PrimaryOptions};
use strata_server::service::{shard_of, AgentConfig, Backend, Replication, Service};
use strata_server::{demo, keysync, pb, server, slots};
use tokio::net::TcpListener;

const N: usize = 200;
const CLUSTER: &str = "cluster-secret";
const ADMIN: &str = "admin-token";

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn cluster() -> Option<Secret> {
    Some(Secret::new(CLUSTER))
}

/// A numbered database at `dir/name` of the demo products `keep` keeps.
fn create(dir: &Path, name: &str, keep: impl Fn(usize) -> bool) -> Arc<Db> {
    let opts = DbOptions { publish_snapshots: true, ship_log_bytes: 16 << 20, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), demo::schema(N), opts).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    let docs: Vec<_> = (0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7)).collect();
    if !docs.is_empty() {
        db.load(docs).unwrap();
    }
    db.storage().start_seq_no().unwrap();
    Arc::new(db)
}

/// A server of `db` (at `dir/name`) as `strata-server --db` makes one,
/// its keys in `<db>.keys.json`, serving gRPC with `access`; it calls the
/// other copies of its shard with `access.cluster`.
async fn serve(db: Arc<Db>, dir: &Path, name: &str, access: Access, set: impl FnOnce(&mut Service)) -> (Arc<Service>, String) {
    let path = dir.join(name);
    let schema = (*db.schema()).clone();
    let mut svc = Service::new(Backend::Local(db), schema, agent(), 16, Duration::from_secs(10));
    svc.keep_slots_in(slots::file_of(&path)).unwrap();
    svc.keep_replication_in(replication::file_of(&path));
    svc.keys = Arc::new(Keys::open(Keys::file_of(&path)).unwrap());
    svc.promote_options = Some(PrimaryOptions { token: access.cluster.clone(), heartbeat: Duration::from_millis(100), retry_max: Duration::from_millis(200), ..Default::default() });
    set(&mut svc);
    let svc = Arc::new(svc);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc.clone(), l, access));
    keysync::spawn(&svc);
    (svc, url)
}

/// A coordinator of `urls` sending `token`, its keys in memory, serving
/// HTTP with `access`: its service and HTTP base URL. It takes the
/// cluster's keys before it answers ([`keysync::start`]).
async fn coordinator(urls: &[String], token: Option<Secret>, access: Access) -> (Arc<Coordinator>, Arc<Service>, String) {
    let c = Arc::new(Coordinator::connect_with(urls.to_vec(), CoordOptions { token, ..Default::default() }).await.unwrap());
    let schema = c.schema.clone();
    let svc = Arc::new(Service::new(Backend::Cluster(c.clone()), schema, agent(), 16, Duration::from_secs(10)));
    keysync::start(&svc).await;
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(svc.clone(), l, access));
    (c, svc, base)
}

async fn http(base: &str, method: &str, path: &str, auth: Option<&str>, body: Option<Json>) -> (u16, Json) {
    let c = reqwest::Client::new();
    let mut r = match method {
        "GET" => c.get(format!("{base}{path}")),
        "DELETE" => c.delete(format!("{base}{path}")),
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

/// A gRPC client of `url` sending `auth` as its bearer credential.
async fn grpc_call<R>(url: &str, auth: Option<&str>, msg: R) -> (pb::strata_client::StrataClient<tonic::transport::Channel>, tonic::Request<R>) {
    let c = pb::strata_client::StrataClient::connect(url.to_string()).await.unwrap();
    let mut req = tonic::Request::new(msg);
    if let Some(a) = auth {
        req.metadata_mut().insert("authorization", format!("Bearer {a}").parse().unwrap());
    }
    (c, req)
}

async fn keys_call(url: &str, auth: Option<&str>, set: &KeySet) -> Result<KeySet, tonic::Status> {
    let (mut c, req) = grpc_call(url, auth, pb::KeysRequest { keys_json: serde_json::to_vec(set).unwrap() }).await;
    c.keys(req).await.map(|r| KeySet::from_json(&r.into_inner().keys_json).unwrap())
}

/// Whether `svc` admits the API key `secret`.
fn admits(svc: &Service, secret: &str) -> bool {
    svc.keys.resolve(None, cluster().as_ref(), Some(&format!("Bearer {secret}"))).is_some()
}

async fn until(what: &str, f: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(30), "waited 30 s for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn key(i: usize) -> strata::value::Value {
    strata::value::Value::Str(format!("p{i:07}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keys_hold_across_a_cluster() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let shard = Access { cluster: cluster(), ..Default::default() };
    // Two shards; the first gets a replica.
    let (s0, u0) = serve(create(d, "s0.strata", |i| shard_of(&key(i), 2) == 0), d, "s0.strata", shard.clone(), |_| {}).await;
    let (s1, u1) = serve(create(d, "s1.strata", |i| shard_of(&key(i), 2) == 1), d, "s1.strata", shard.clone(), |_| {}).await;
    let (r0, ur0) = serve(create(d, "r0.strata", |_| false), d, "r0.strata", shard.clone(), |_| {}).await;
    let coord_access = Access { token: Some(Secret::new(ADMIN)), cluster: cluster(), ..Default::default() };
    let (a, svc_a, base_a) = coordinator(&[u0.clone(), u1.clone()], cluster(), coord_access.clone()).await;
    a.add_replica(0, &ur0, false).await.unwrap();
    until("the replica follows its primary", || r0.role() == pb::Role::Replica).await;

    // A key made on the coordinator reaches every server, and works there.
    let (status, j) = http(&base_a, "POST", "/v1/admin/keys", Some(ADMIN), Some(json!({"name": "dashboards", "scope": "read"}))).await;
    assert_eq!(status, 200, "{j}");
    let reader = j["secret"].as_str().unwrap().to_string();
    let id = j["key"]["id"].as_str().unwrap().to_string();
    until("every server holds the key", || [&s0, &s1, &r0].iter().all(|s| admits(s, &reader))).await;
    let select = json!({"sql": "SELECT id FROM products LIMIT 500"});
    let (status, j) = http(&base_a, "POST", "/v1/query", Some(&reader), Some(select.clone())).await;
    assert_eq!((status, j["rows"].as_array().map(Vec::len)), (200, Some(N)), "{j}");
    // Kept beside each shard's database.
    assert!(std::fs::read_to_string(d.join("s1.strata.keys.json")).unwrap().contains(&id));
    let (_, keys) = http(&base_a, "GET", "/v1/admin/keys", Some(ADMIN), None).await;
    for u in [&u0, &u1, &ur0] {
        assert!(keys["peers"][u].is_object() && keys["peers"][u]["error"].is_null(), "{keys}");
    }

    // A coordinator started again (its keys were in memory) takes the
    // cluster's keys from the shards before it serves.
    let (_b, svc_b, base_b) = coordinator(&[u0.clone(), u1.clone()], cluster(), coord_access.clone()).await;
    assert!(admits(&svc_b, &reader));
    assert_eq!(http(&base_b, "POST", "/v1/query", Some(&reader), Some(select.clone())).await.0, 200);

    // Revoked on it, the key is refused everywhere, the first coordinator
    // included, and it does not come back from a server that held it.
    let held = svc_b.keys.set().keys.into_iter().find(|k| k.id == id).unwrap();
    let (status, j) = http(&base_b, "DELETE", &format!("/v1/admin/keys/{id}"), Some(ADMIN), None).await;
    assert_eq!(status, 200, "{j}");
    until("every server refuses the revoked key", || [&s0, &s1, &r0, &svc_a, &svc_b].iter().all(|s| !admits(s, &reader))).await;
    assert_eq!(http(&base_a, "POST", "/v1/query", Some(&reader), Some(select.clone())).await.0, 401);
    assert!(!s1.keys.merge(&KeySet { keys: vec![held], revoked: vec![] }).unwrap());
    assert!(!admits(&s1, &reader) && s1.keys.set().revoked.iter().any(|r| r.id == id));

    // A key made on a shard reaches the coordinators and the other shards.
    let (_, ingest) = s1.keys.create("ingest", Scope::Write, None, true, true).unwrap();
    until("the key made on a shard reaches every server", || [&s0, &r0, &svc_a, &svc_b].iter().all(|s| admits(s, &ingest))).await;
    let write = json!({"ops": [{"op": "set", "key": "p0000001", "field": "price", "value": 5.0}]});
    assert_eq!(http(&base_a, "POST", "/v1/write", Some(&ingest), Some(write)).await.0, 200);

    // The calls between servers take the cluster's token alone: not a
    // key, nor the admin's token.
    let ask = KeySet::default();
    let e = keys_call(&u1, Some(&ingest), &ask).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied, "{e:?}");
    assert!(keys_call(&u1, Some(CLUSTER), &ask).await.unwrap().keys.iter().any(|k| k.name == "ingest"));
    let (_, admin_shard) = serve(create(d, "x.strata", |_| false), d, "x.strata", Access { token: Some(Secret::new(ADMIN)), cluster: cluster(), ..Default::default() }, |_| {}).await;
    let (mut c, req) = grpc_call(&admin_shard, Some(ADMIN), pb::SlotsRequest::default()).await;
    let e = c.slots(req).await.unwrap_err();
    assert!(e.code() == tonic::Code::PermissionDenied && e.message().contains("cluster's token"), "{e:?}");
    let (mut c, req) = grpc_call(&admin_shard, Some(CLUSTER), pb::SlotsRequest::default()).await;
    assert!(c.slots(req).await.is_ok());
    let (mut c, req) = grpc_call(&admin_shard, Some(ADMIN), pb::QueryRequest { sql: "SELECT id FROM products LIMIT 1".into(), ..Default::default() }).await;
    assert!(c.query(req).await.is_ok(), "the admin's token still queries");
    // The audit log names the cluster's merges.
    let noted = s0.audit.latest(2000).iter().any(|e| e.who == "cluster" && e.route == "Keys" && e.detail.as_deref().is_some_and(|d| d.starts_with("merged keys")));
    assert!(noted);
}

/// An open server (no token, no cluster token) takes no keys from others,
/// which would close it to them, and makes none while it serves in a
/// cluster; its coordinator still reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_shard_is_not_cut_off_by_keys() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (s0, u0) = serve(create(d, "s0.strata", |_| true), d, "s0.strata", Access::default(), |_| {}).await;
    let (_c, svc, base) = coordinator(&[u0.clone()], None, Access::default()).await;
    assert!(s0.slots().is_some(), "the coordinator gave the shard its map");

    // The coordinator may close itself with a key; the shard refuses it.
    let (status, j) = http(&base, "POST", "/v1/admin/keys", None, Some(json!({"name": "ops", "scope": "admin"}))).await;
    assert_eq!(status, 200, "{j}");
    let ops = j["secret"].as_str().unwrap().to_string();
    let (status, keys) = http(&base, "GET", "/v1/admin/keys", Some(&ops), None).await;
    assert_eq!(status, 200);
    until("the shard's refusal is recorded", || svc.keys.peers()[&u0]["error"].as_str().is_some()).await;
    let (_, keys2) = http(&base, "GET", "/v1/admin/keys", Some(&ops), None).await;
    assert!(keys2["peers"][&u0]["error"].as_str().unwrap().contains("no token"), "{keys} {keys2}");
    assert!(s0.keys.is_empty());
    let e = keys_call(&u0, None, &svc.keys.set()).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::FailedPrecondition);
    assert!(s0.keys.is_empty());
    let (status, j) = http(&base, "POST", "/v1/query", Some(&ops), Some(json!({"sql": "SELECT COUNT(*) FROM products"}))).await;
    assert_eq!((status, j["rows"][0][0].as_i64()), (200, Some(N as i64)), "{j}");

    // The shard itself refuses a key that would shut its cluster out.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let shard_http = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(s0.clone(), l, Access::default()));
    let (status, j) = http(&shard_http, "POST", "/v1/admin/keys", None, Some(json!({"name": "ops", "scope": "admin"}))).await;
    assert_eq!(status, 409, "{j}");
    assert!(j["error"].as_str().unwrap().contains("--cluster-token"), "{j}");
    assert!(s0.keys.is_empty());
}

/// A primary passes its keys to its replicas and takes theirs, so that a
/// replica promoted to primary admits its primary's keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replica_holds_its_primarys_keys() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let access = Access { cluster: cluster(), ..Default::default() };
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ur = format!("http://{}", l.local_addr().unwrap());
    let rdb = create(d, "r.strata", |_| false);
    let schema = (*rdb.schema()).clone();
    let mut r = Service::new(Backend::Local(rdb.clone()), schema, agent(), 16, Duration::from_secs(10));
    r.set_replication(Replication::Replica(Arc::new(Follow::open(rdb, None).unwrap())));
    r.keys = Arc::new(Keys::open(d.join("r.strata.keys.json")).unwrap());
    let r = Arc::new(r);
    tokio::spawn(server::serve_grpc(r.clone(), l, access.clone()));
    keysync::spawn(&r);
    let pdb = create(d, "p.strata", |_| true);
    let opts = PrimaryOptions { replicas: vec![ur.clone()], acks: Acks::All, token: cluster(), heartbeat: Duration::from_millis(100), retry_max: Duration::from_millis(200), ..Default::default() };
    let (p, _) = serve(pdb.clone(), d, "p.strata", access.clone(), |svc| svc.set_replication(Replication::Primary(Primary::start(pdb.clone(), None, opts.clone()).unwrap()))).await;

    let (_, ops) = p.keys.create("ops", Scope::Admin, None, true, true).unwrap();
    until("the replica holds the primary's key", || admits(&r, &ops)).await;
    // One made on the replica reaches the primary, at its next merge.
    let (_, reader) = r.keys.create("reader", Scope::Read, None, true, true).unwrap();
    until("the primary holds the replica's key", || admits(&p, &reader)).await;
    assert!(p.keys.peers()[&ur]["error"].is_null());
}
