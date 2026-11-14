//! What the rebuilt command center reads of a cluster: the command
//! center's own requests tagged and left out of the clients' traffic, an
//! audit log of changes only, each shard's capability (whether it serves
//! reads and takes writes, and why not) with copies named and a dead
//! primary that is no longer anyone's primary, fleet health, events, time
//! beyond an hour, and the images a version picker offers.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use chloedb::db::Db;
use chloedb::storage::DbOptions;
use chloedb_server::coord::Coordinator;
use chloedb_server::service::{AgentConfig, Backend, Service};
use chloedb_server::{demo, server};
use tokio::net::TcpListener;

/// A copy's short name, as the cluster names it from its URL.
fn short(url: &str) -> String {
    chloedb_server::coord::copy_short(0, url)
}

const N: usize = 300;
const AFTER_MS: u64 = 600;

/// A port free now, of a block of 20 this test process takes alone (by
/// its process id), as `tests/failover.rs` takes them.
fn port() -> u16 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let base = 20_000 + (std::process::id() % 500) * 20;
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(n < 20, "this test process took its 20 ports");
        let p = (base + n) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return p;
        }
    }
}

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: chloedb_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

/// A numbered database of the demo products (`rows` of them).
fn create(path: &Path, rows: usize) {
    let opts = DbOptions { publish_snapshots: true, ship_log_bytes: 16 << 20, ..DbOptions::default() };
    let db = Db::create(path, demo::schema(N), opts).unwrap();
    if rows > 0 {
        db.set_embedder("emb", Arc::new(chloedb::embed::HashEmbedder { dim: demo::DIM })).unwrap();
        db.load((0..rows).map(|i| demo::product(i, 7))).unwrap();
    }
    db.storage().start_seq_no().unwrap();
}

/// A `chloedb-server` process of one database, with failover.
struct Node {
    name: String,
    path: PathBuf,
    log: PathBuf,
    http: u16,
    grpc: u16,
    child: Option<Child>,
    /// More flags for the server.
    args: Vec<String>,
}

impl Node {
    fn new(dir: &Path, name: &str, rows: usize) -> Node {
        let path = dir.join(format!("{name}.strata"));
        create(&path, rows);
        Node { name: name.into(), path, log: dir.join(format!("{name}.log")), http: port(), grpc: port(), child: None, args: Vec::new() }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.grpc)
    }

    async fn start(&mut self) {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&self.log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_chloedb-server"))
            .arg("--db")
            .arg(&self.path)
            .args(["--listen", &format!("127.0.0.1:{}", self.http), "--grpc", &format!("127.0.0.1:{}", self.grpc)])
            .args(["--seq-no", "--log", "info", "--failover-after-ms", &AFTER_MS.to_string(), "--log-store", "off"])
            .args(["--node-name", &format!("node-{}", self.name)])
            .args(&self.args)
            .env("CHLOEDB_ZONE", "zone-a")
            .env("CHLOEDB_IMAGE", format!("ghcr.io/chloetech-ai/chloedb-server@sha256:{}", "a".repeat(64)))
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        self.child = Some(child);
        let t0 = Instant::now();
        while self.health().await.is_none() {
            assert!(t0.elapsed() < Duration::from_secs(30), "{} did not start: {}", self.url(), self.logs());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    async fn health(&self) -> Option<Json> {
        let r = chloedb_server::http_client().build().unwrap().get(format!("http://127.0.0.1:{}/v1/health", self.http)).send().await.ok()?;
        r.json().await.ok()
    }

    async fn replication(&self) -> Json {
        self.health().await.map_or(Json::Null, |h| h["replication"].clone())
    }

    fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn until<F: std::future::Future<Output = bool>>(what: &str, nodes: &[&Node], f: impl Fn() -> F) {
    let t0 = Instant::now();
    while !f().await {
        if t0.elapsed() > Duration::from_secs(30) {
            let logs: Vec<String> = nodes.iter().map(|n| format!("--- {}\n{}", n.url(), n.logs())).collect();
            panic!("waited 30 s for: {what}\n{}", logs.join("\n"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A coordinator of `url`'s shard, and a server of it.
async fn coordinator(url: String) -> (Arc<Coordinator>, Arc<Service>) {
    let c = Arc::new(Coordinator::connect(vec![url]).await.unwrap());
    let schema = c.schema.clone();
    (c.clone(), Arc::new(Service::new(Backend::Cluster(c), schema, agent(), 16, Duration::from_secs(10))))
}

/// The cluster as a fresh survey finds it.
async fn fresh(c: &Coordinator) -> Json {
    (*c.snapshot_within(Duration::ZERO).await).clone()
}

/// A shard of `nodes`: the first its primary, the others replicas the
/// coordinator adds, each in sync.
async fn shard_of(nodes: &mut [&mut Node]) -> (Arc<Coordinator>, Arc<Service>) {
    for n in nodes.iter_mut() {
        n.start().await;
    }
    let (coord, svc) = coordinator(nodes[0].url()).await;
    for n in nodes.iter().skip(1) {
        coord.add_replica(0, &n.url(), false, false).await.unwrap();
    }
    let want = nodes.len() - 1;
    let primary: &Node = nodes[0];
    until("the replicas are in sync and vote", &[primary], || async {
        let r = primary.replication().await;
        r["replicas"].as_array().is_some_and(|v| v.len() == want && v.iter().all(|f| f["in_sync"] == true && f["member"] == true))
    })
    .await;
    (coord, svc)
}

/// A shard of two copies with failover that loses its replica serves no
/// reads: the primary alone is no majority. `/v1/cluster` says so, and
/// why, rather than "degraded"; the dead copy's role is unknown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_two_copy_shard_that_loses_a_copy_serves_no_reads() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Node::new(dir.path(), "a", N);
    let mut b = Node::new(dir.path(), "b", 0);
    let (coord, svc) = shard_of(&mut [&mut a, &mut b]).await;

    let c = fresh(&coord).await;
    let s = &c["shards"][0];
    assert_eq!((s["state"].as_str(), s["serves_reads"].as_bool(), s["takes_writes"].as_bool(), s["majority"].as_bool(), s["failover"].as_bool()), (Some("ok"), Some(true), Some(true), Some(true), Some(true)), "{s}");
    assert_eq!(s["primary"], json!(a.url()), "{s}");
    let copies = s["replicas"].as_array().unwrap();
    assert_eq!(copies.iter().map(|r| r["short"].as_str().unwrap()).collect::<Vec<_>>(), [short(&a.url()), short(&b.url())]);
    assert_eq!(copies[0]["name"], "node-a");
    assert_eq!(copies[0]["zone"], "zone-a");
    assert_eq!(copies[0]["version"]["digest"], json!(format!("sha256:{}", "a".repeat(64))));
    assert!(copies[0]["version"]["version"].is_string() && copies[0]["last_seen_ms"].as_u64().unwrap() > 0, "{}", copies[0]);
    assert!(c["at_ms"].as_u64().is_some());
    // One snapshot is shared: asked again at once, the same survey.
    let (x, y) = (coord.snapshot().await, coord.snapshot().await);
    assert!(Arc::ptr_eq(&x, &y));

    b.kill();
    until("the shard serves no reads", &[&a], || async { fresh(&coord).await["shards"][0]["state"] == "no_reads" }).await;
    let c = svc.cluster().await;
    let s = &c["shards"][0];
    assert_eq!((s["serves_reads"].as_bool(), s["majority"].as_bool(), s["status"].as_str()), (Some(false), Some(false), Some("degraded")), "{s}");
    let why = s["why"].as_str().unwrap();
    assert!(why.starts_with("1 of 2 copies answers") && why.contains("no majority"), "{why}");
    let dead = &s["replicas"][1];
    assert_eq!((dead["role"].as_str(), dead["last_role"].as_str(), dead["takes_writes"].as_bool()), (Some("unknown"), Some("replica"), Some(false)), "{dead}");
    assert!(dead["down_since_ms"].as_u64().unwrap() > 0);
    assert_eq!(c["status"], "down", "a shard serving no reads fails every search");
    assert!(c["cluster"].is_object(), "the cluster's name and environment");

    // Fleet health leads with it: what fails, where, and what to do.
    let f = svc.fleet_health().await;
    assert_eq!((f["status"].as_str(), f["headline"].as_str()), (Some("critical"), Some("Searches are failing: shard 0 has no majority")), "{f}");
    let p = &f["problems"][0];
    assert_eq!(p["where"], json!(["shard 0", short(&b.url())]));
    assert_eq!(p["next"]["text"], format!("Bring {} back, or remove it so {} has a majority", short(&b.url()), short(&a.url())));
    assert_eq!(p["since_ms"], dead["down_since_ms"]);
    let check = |id: &str| f["checks"].as_array().unwrap().iter().find(|c| c["id"] == id).cloned().unwrap();
    assert_eq!(check("shards")["value"]["no_reads"], 1);
    assert_eq!((check("copies")["down"].as_u64(), check("copies")["total"].as_u64()), (Some(1), Some(2)));
    assert_eq!(f["coordinators"][0]["this"], true);
}

/// A primary that dies is no longer the shard's primary in `/v1/cluster`
/// (its role is unknown), and the copy elected in its place is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_primary_loses_its_role() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Node::new(dir.path(), "a", N);
    let mut b = Node::new(dir.path(), "b", 0);
    let mut c = Node::new(dir.path(), "c", 0);
    let (coord, svc) = shard_of(&mut [&mut a, &mut b, &mut c]).await;
    assert_eq!(fresh(&coord).await["shards"][0]["primary"], json!(a.url()));
    // Writes the replicas catch up with: a copy that has not grants no votes.
    for i in 0..10 {
        svc.write_ops(vec![json!({"op": "set", "key": format!("p{i:07}"), "field": "sold", "value": i})], None).await.unwrap();
    }

    a.kill();
    until("another copy is the primary", &[&b, &c], || async {
        let s = fresh(&coord).await["shards"][0].clone();
        s["primary"].is_string() && s["primary"] != json!(a.url())
    })
    .await;
    let s = fresh(&coord).await["shards"][0].clone();
    let old = &s["replicas"][0];
    assert_eq!((old["role"].as_str(), old["last_role"].as_str(), old["takes_writes"].as_bool()), (Some("unknown"), Some("primary"), Some(false)), "{old}");
    let primaries = s["replicas"].as_array().unwrap().iter().filter(|r| r["takes_writes"] == true).count();
    assert_eq!(primaries, 1, "{s}");
    assert_eq!(s["state"], "degraded", "{s}");
    let sa = short(&a.url());
    assert!(s["why"].as_str().unwrap().contains(&format!("{sa} down")), "{s}");

    // The coordinator's events: the primary down, then another elected.
    let (events, _) = svc.events.range(0, u64::MAX, &[], 100);
    let down = events.iter().find(|e| e.event.kind == "copy_down").unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!((down.event.where_["copy"].as_str(), down.event.severity.as_str()), (Some(sa.as_str()), "warning"));
    let new = events.iter().find(|e| e.event.kind == "primary_changed").unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!((new.event.detail["from"].as_str(), new.event.detail["automatic"].as_bool()), (Some(sa.as_str()), Some(true)), "{:?}", new.event);
    assert_eq!(new.event.detail["to_url"], s["primary"]);
    assert!(down.ts_ms <= new.ts_ms);
}

async fn listen() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    (l, url)
}

/// A server of the demo products over HTTP, in this process.
async fn local(dir: &Path, tune: impl FnOnce(&mut Service)) -> (Arc<Service>, String) {
    let db = Db::create(&dir.join("local.strata"), demo::schema(N), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(chloedb::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).map(|i| demo::product(i, 7))).unwrap();
    let schema = (*db.schema()).clone();
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(10));
    tune(&mut svc);
    let svc = Arc::new(svc);
    let (l, url) = listen().await;
    tokio::spawn(server::serve_http(svc.clone(), l, Default::default()));
    (svc, url)
}

/// Past the hour memory holds, `/v1/timeseries?from=&to=` answers a point
/// a minute from the log store's `load` table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_timeseries_over_three_hours_is_a_point_a_minute_from_the_load_table() {
    use chloedb_server::logstore::{Load, LogOptions, LogStore};
    let dir = tempfile::tempdir().unwrap();
    let store = LogStore::open(LogOptions { capture: false, ..LogOptions::new(dir.path().join("l.logs"), "n1".into()) }).unwrap();
    let now = chloedb_server::telemetry::now_ms() / 60_000 * 60_000;
    let minutes: Vec<Load> = (0..180u64)
        .map(|m| Load {
            ts_ms: now - (180 - m) * 60_000,
            role: "standalone".into(),
            qps: 10.0 + m as f64,
            cpu: 1.5,
            cores: 4,
            p99_ms: Some(40.0),
            disk_used_ratio: Some(0.5),
            error_ratio: Some(0.001),
            p50_ms: Some(4.0),
            mem_bytes: Some(1 << 30),
            ..Default::default()
        })
        .collect();
    store.write_loads(&minutes).unwrap();
    let (_svc, base) = local(dir.path(), |s| s.log_store = Some(store)).await;
    let from = now - 3 * 3_600_000;
    let t: Json = client().get(format!("{base}/v1/timeseries?from={from}&to={now}")).send().await.unwrap().json().await.unwrap();
    assert_eq!((t["source"].as_str(), t["interval_ms"].as_u64()), (Some("load"), Some(60_000)), "{t}");
    let points = t["points"].as_array().unwrap();
    assert_eq!(points.len(), 180);
    assert_eq!(points[0]["t"].as_u64(), Some(now - 180 * 60_000), "oldest first");
    let p = &points[179];
    assert_eq!((p["qps"].as_f64(), p["error_ratio"].as_f64(), p["p50_ms"].as_f64(), p["mem_bytes"].as_u64(), p["cores"].as_u64()), (Some(189.0), Some(0.001), Some(4.0), Some(1 << 30), Some(4)), "{p}");
    assert!(p.get("timeouts_s").is_none(), "absent, not 0, when unknown");
    // Inside memory's hour: memory's points, as before.
    let m: Json = client().get(format!("{base}/v1/timeseries?since=0")).send().await.unwrap().json().await.unwrap();
    assert_eq!((m["source"].as_str(), m["interval_ms"].as_u64()), (Some("memory"), Some(2000)));

    // On machines of one's own no volume size is known: a replay of the
    // autoscaler leaves growing volumes out rather than name sizes.
    let r: Json = client().post(format!("{base}/v1/admin/autoscale/replay")).json(&json!({"autoscale": {"volumes": {"fullAbove": 0.4}}, "days": 1})).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["left_out"].as_array().map(Vec::len), Some(1), "{r}");
    assert_eq!(r["sizes"]["volume"], Json::Null, "{r}");
    assert!(!r.to_string().contains("10Gi"), "{r}");

    // A log search is a POST that only reads: not audited, while a reset
    // of the read counts is a change.
    let r = client().post(format!("{base}/v1/logs/query")).json(&json!({"sql": "SELECT COUNT(*) AS n FROM logs"})).send().await.unwrap();
    let (status, body) = (r.status(), r.text().await.unwrap());
    assert!(status.is_success(), "{body}");
    assert!(client().post(format!("{base}/v1/index/reads/reset")).send().await.unwrap().status().is_success());
    let audit: Json = client().get(format!("{base}/v1/admin/audit")).send().await.unwrap().json().await.unwrap();
    let routes: Vec<&str> = audit["entries"].as_array().unwrap().iter().filter_map(|e| e["route"].as_str()).collect();
    assert!(routes.contains(&"/v1/index/reads/reset") && !routes.contains(&"/v1/logs/query"), "{routes:?}");
}

/// On a coordinator, `PUT /v1/logs/filter?node=` sets a copy's log
/// filter over gRPC (no new call: a kind of `Profile`), for a while: the
/// copy says until when and by whom, then goes back on its own. A plan
/// of a change of shape and every table's size are asked of it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_coordinator_sets_a_copys_log_filter_for_a_while() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Node::new(dir.path(), "a", 10);
    let mut b = Node::new(dir.path(), "b", 0);
    let backups = dir.path().join("b-backups");
    b.args = vec!["--backup-dir".into(), backups.display().to_string()];
    let (c, svc) = shard_of(&mut [&mut a, &mut b]).await;
    let snap = fresh(&c).await;
    let (sa, sb) = (short(&a.url()), short(&b.url()));
    assert_eq!(snap["shards"][0]["replicas"][0]["short"], sa, "{snap}");
    let dirs: Vec<&Json> = snap["shards"][0]["replicas"].as_array().unwrap().iter().map(|r| &r["backup_dir"]).collect();
    assert_eq!(dirs, [&Json::Null, &json!(backups.display().to_string())], "{snap}");
    let (l, base) = listen().await;
    tokio::spawn(server::serve_http(svc.clone(), l, Default::default()));
    let copy = format!("http://127.0.0.1:{}", a.http);

    let r = client().put(format!("{base}/v1/logs/filter?node={sa}")).json(&json!({"filter": "debug", "expires_s": 2})).send().await.unwrap();
    let status = r.status();
    let j: Json = r.json().await.unwrap();
    assert!(status.is_success(), "{j}");
    assert_eq!((j["filter"].as_str(), j["previous"].as_str()), (Some("debug"), Some("info")), "{j}");
    assert!(j["expires_ms"].as_u64().is_some() && j["set_by"].is_string(), "{j}");
    let now: Json = client().get(format!("{copy}/v1/logs/filter")).send().await.unwrap().json().await.unwrap();
    assert_eq!(now["filter"], "debug", "{now}");
    let logs: Json = client().get(format!("{copy}/v1/logs?limit=1")).send().await.unwrap().json().await.unwrap();
    assert_eq!(logs["filter_expires_ms"], now["expires_ms"]);
    let t0 = Instant::now();
    loop {
        let now: Json = client().get(format!("{copy}/v1/logs/filter")).send().await.unwrap().json().await.unwrap();
        if now["filter"] == "info" && now["expires_ms"].is_null() {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "the copy's filter did not go back: {now}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let r = client().put(format!("{base}/v1/logs/filter?node=s9-9")).json(&json!({"filter": "debug"})).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let r = client().put(format!("{base}/v1/logs/filter")).json(&json!({"filter": "debug", "expires_s": -1})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let changes: Json = client().get(format!("{base}/v1/admin/audit?kind=change")).send().await.unwrap().json().await.unwrap();
    assert!(changes["entries"].as_array().unwrap().iter().any(|e| e["object"] == "log filter" && e["detail"].as_str().is_some_and(|d| d.contains(&format!("on {sa}")))), "{changes}");

    // A copy's profiling settings, read and set over the same call (PROF-2).
    let r = client().get(format!("{base}/v1/profile/settings?node={sa}")).send().await.unwrap();
    let was: Json = r.json().await.unwrap();
    assert_eq!((was["short"].as_str(), was["cpu_all"].as_bool()), (Some(sa.as_str()), Some(false)), "{was}");
    let r = client().put(format!("{base}/v1/profile/settings?node={sa}")).json(&json!({"slow_ms": 25})).send().await.unwrap();
    let status = r.status();
    let set: Json = r.json().await.unwrap();
    assert!(status.is_success(), "{set}");
    assert_eq!(set["slow_ms"].as_f64(), Some(25.0), "{set}");
    let own: Json = client().get(format!("{copy}/v1/profile/settings")).send().await.unwrap().json().await.unwrap();
    assert_eq!((own["slow_ms"].as_f64(), own["started_slow_ms"].clone()), (Some(25.0), was["started_slow_ms"].clone()), "the copy's own: {own}");
    let mine: Json = client().get(format!("{base}/v1/profile/settings")).send().await.unwrap().json().await.unwrap();
    assert_ne!(mine["slow_ms"].as_f64(), Some(25.0), "the coordinator's are its own: {mine}");
    let r = client().put(format!("{base}/v1/profile/settings?node={sa}")).json(&json!({"slow_ms": -1})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let changes: Json = client().get(format!("{base}/v1/admin/audit?kind=change")).send().await.unwrap().json().await.unwrap();
    assert!(changes["entries"].as_array().unwrap().iter().any(|e| e["object"] == format!("profiling settings of {sa}") && e["detail"] == format!("profiling on {sa}: slow_ms 25")), "{changes}");

    // A plan changes nothing: one more copy is added, none removed.
    let r = client().post(format!("{base}/v1/admin/plan")).json(&json!({"shards": [{"shard": 0, "copies": [a.url(), b.url(), "http://127.0.0.1:1"]}]})).send().await.unwrap();
    let status = r.status();
    let p: Json = r.json().await.unwrap();
    assert!(status.is_success(), "{p}");
    assert_eq!(p["steps"][0]["kind"], "add_copy", "{p}");
    assert_eq!(p["removes_copies"], json!([]), "{p}");
    assert_eq!(fresh(&c).await["shards"][0]["replicas"].as_array().unwrap().len(), 2);
    let audit: Json = client().get(format!("{base}/v1/admin/audit")).send().await.unwrap().json().await.unwrap();
    assert!(audit["entries"].as_array().unwrap().iter().all(|e| e["route"] != "/v1/admin/plan"), "a plan is a read: {audit}");

    // Every table, cheaply: of the cluster, by shard.
    let t: Json = client().get(format!("{base}/v1/index/tables")).send().await.unwrap().json().await.unwrap();
    let products = t["tables"].as_array().unwrap().iter().find(|t| t["table"] == "products").cloned().unwrap_or_else(|| panic!("{t}"));
    assert_eq!((products["docs"].as_u64(), products["shards"][0]["docs"].as_u64()), (Some(10), Some(10)), "{t}");
    assert!(products["bytes"].as_u64().unwrap() > 0, "{t}");

    // A copy's shard verified by its primary, and a checkpoint of a copy,
    // each audited with the copy as its object.
    let r = client().post(format!("{base}/v1/admin/copies/verify")).json(&json!({"url": sb})).send().await.unwrap();
    let status = r.status();
    let v: Json = r.json().await.unwrap();
    assert!(status.is_success(), "{v}");
    assert_eq!((v["replicas"][0]["url"].as_str(), v["replicas"][0]["in_sync"].as_bool(), v["replicas"][0]["compared"].as_bool()), (Some(b.url().as_str()), Some(true), Some(true)), "{v}");
    // Compared by this call, and it says when.
    assert!(v["replicas"][0]["compared_ms"].as_u64().unwrap() >= v["asked_ms"].as_u64().unwrap(), "{v}");
    let r = client().post(format!("{base}/v1/admin/copies/checkpoint")).json(&json!({"url": b.url(), "name": "before-upgrade"})).send().await.unwrap();
    let status = r.status();
    let k: Json = r.json().await.unwrap();
    assert!(status.is_success(), "{k}");
    assert!(backups.join("before-upgrade").exists() || std::fs::read_dir(&backups).unwrap().count() == 1, "{k}");
    let r = client().post(format!("{base}/v1/admin/copies/checkpoint")).json(&json!({"url": sa, "name": "x"})).send().await.unwrap();
    assert_eq!(r.status(), 400, "s0-0 has no --backup-dir");
    let changes: Json = client().get(format!("{base}/v1/admin/audit?kind=change")).send().await.unwrap().json().await.unwrap();
    let objects: Vec<&str> = changes["entries"].as_array().unwrap().iter().filter(|e| e["route"].as_str().is_some_and(|r| r.starts_with("/v1/admin/copies/"))).filter_map(|e| e["object"].as_str()).collect();
    assert_eq!(objects, [format!("copy {sa}"), format!("copy {sb}"), format!("copy {sb}")], "the newest first: {changes}");

    // A copy's data read against its checksums, from the coordinator
    // (finding chaos-6): nothing damaged, and the copy says when it ran.
    let r = client().post(format!("{base}/v1/admin/copies/scrub")).json(&json!({"url": sb})).send().await.unwrap();
    let status = r.status();
    let k: Json = r.json().await.unwrap();
    assert!(status.is_success(), "{k}");
    assert_eq!(k["scrub"]["damaged"], json!([]), "{k}");
    let h: Json = client().get(format!("http://127.0.0.1:{}/v1/health", b.http)).send().await.unwrap().json().await.unwrap();
    assert!(h["scrub"]["last_at_ms"].as_u64().is_some() && h.get("damaged").is_none(), "{h}");
}

/// Through a coordinator: a write a shard cannot take is refused before
/// any shard takes it; one that fails partway says what each shard did; a
/// write from the command center is audited whatever its answer, and so
/// are its imports and streams; a table another coordinator made is
/// listed; a copy that does not answer keeps its name, and every table's
/// view lists it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_through_a_coordinator_says_what_each_shard_did() {
    use chloedb_server::json::document_json;
    let dir = tempfile::tempdir().unwrap();
    let mut a = Node::new(dir.path(), "a", 0);
    let mut b = Node::new(dir.path(), "b", 0);
    a.start().await;
    b.start().await;
    let serve = |c: Arc<Coordinator>| async move {
        let svc = Arc::new(Service::new(Backend::Cluster(c.clone()), c.schema.clone(), agent(), 16, Duration::from_secs(10)));
        let (l, base) = listen().await;
        tokio::spawn(server::serve_http(svc.clone(), l, Default::default()));
        (svc, base)
    };
    let c = Arc::new(Coordinator::connect(vec![a.url(), b.url()]).await.unwrap());
    let (svc, base) = serve(c.clone()).await;
    let (_, other) = serve(Arc::new(Coordinator::connect(vec![a.url(), b.url()]).await.unwrap())).await;
    let schema = c.schema.clone();
    let doc = |i: usize| document_json(&schema, &demo::product(i, 7));
    svc.write_ops((0..40).map(|i| json!({"op": "upsert", "doc": doc(i)})).collect(), None).await.unwrap();
    let ui = |r: reqwest::RequestBuilder| r.header("x-strata-client", "command-center").header("x-strata-reason", "fix%20notes");
    let audit = |q: &str| {
        let url = format!("{base}/v1/admin/audit?{q}");
        async move { client().get(url).send().await.unwrap().json::<Json>().await.unwrap() }
    };

    // An import and a stream from the command center are audited as its
    // statements are.
    let rows: String = (40..42).map(|i| doc(i).to_string() + "\n").collect();
    let r = ui(client().post(format!("{base}/v1/import?table=products&format=jsonl"))).header("content-type", "application/x-ndjson").body(rows).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let lines: String = (42..45).map(|i| json!({"op": "upsert", "doc": doc(i)}).to_string() + "\n").collect();
    let r = ui(client().post(format!("{base}/v1/stream"))).header("content-type", "application/x-ndjson").body(lines).send().await.unwrap();
    let answers = r.text().await.unwrap();
    assert!(answers.contains("\"done\":true"), "{answers}");
    let t0 = Instant::now();
    loop {
        let changes = audit("kind=change").await;
        let entries = changes["entries"].as_array().unwrap();
        let (import, stream) = (entries.iter().find(|e| e["route"] == "/v1/import"), entries.iter().find(|e| e["route"] == "/v1/stream"));
        if let (Some(i), Some(s)) = (import, stream) {
            assert_eq!((i["object"].as_str(), i["after"]["rows"].as_u64(), i["reason"].as_str()), (Some("table products"), Some(2), Some("fix notes")), "{i}");
            assert_eq!((s["object"].as_str(), s["after"]["rows"].as_u64(), s["reason"].as_str(), s["client"].as_str()), (Some("table products"), Some(3), Some("fix notes"), Some("command-center")), "{s}");
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "{changes}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A table made through one coordinator is listed by another.
    let r = ui(client().post(format!("{base}/v1/sql"))).json(&json!({"sql": "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)"})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    // Its audit entry counts no rows (NEW-data-tools-12).
    let made = audit("kind=change").await;
    let e = made["entries"].as_array().unwrap().iter().find(|e| e["object"] == "table notes").expect("the CREATE is audited");
    assert_eq!((e["detail"].as_str(), e["after"]["rows"].as_u64()), (Some("ran: CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)"), None), "{e}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let t: Json = client().get(format!("{other}/v1/tables")).send().await.unwrap().json().await.unwrap();
    assert!(t["tables"].as_array().unwrap().iter().any(|t| t["table"] == "notes"), "{t}");
    // LIV-13: a coordinator's health counts the rows of every table, over
    // its shards, beside the first table's `docs`.
    let r = ui(client().post(format!("{base}/v1/sql"))).json(&json!({"sql": "INSERT INTO notes (id, body) VALUES (1, 'a'), (2, 'b'), (3, 'c')"})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let h: Json = client().get(format!("{base}/v1/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!((&h["docs"], &h["rows"], &h["tables"]), (&json!(45), &json!(48), &json!({"products": 45, "notes": 3})), "{h}");

    // Shard 1 goes down after the cluster was last seen whole: the write
    // goes, shard 0 takes it, and the answer says so, by short name.
    fresh(&c).await;
    b.kill();
    let r = ui(client().post(format!("{base}/v1/sql"))).json(&json!({"sql": "UPDATE products SET sold = 5 WHERE price >= 0"})).send().await.unwrap();
    let status = r.status();
    let j: Json = r.json().await.unwrap();
    assert!(status.is_server_error(), "{status}: {j}");
    assert_eq!(j["partial"], true, "{j}");
    let shards = j["shards"].as_array().unwrap();
    let (sa, sb) = (short(&a.url()), short(&b.url()));
    assert_eq!((shards[0]["short"].as_str(), shards[1]["short"].as_str()), (Some(sa.as_str()), Some(sb.as_str())), "{j}");
    assert!(shards[0]["rows"].as_u64().unwrap() > 0 && shards[1]["error"].as_str().unwrap().contains(&sb), "{j}");
    assert!(!j["error"].as_str().unwrap().starts_with("shard http"), "{j}");

    // Once the cluster is seen without it, a write is refused before any
    // shard takes it.
    let snap = fresh(&c).await;
    let copy = &snap["shards"][1]["replicas"][0];
    assert_eq!((snap["shards"][1]["takes_writes"].as_bool(), copy["status"].as_str()), (Some(false), Some("down")), "{snap}");
    assert_eq!((copy["name"].as_str(), copy["zone"].as_str()), (Some("node-b"), Some("zone-a")), "a copy down keeps what it said last: {copy}");
    assert!(copy["version"]["digest"].is_string(), "{copy}");
    let r = ui(client().post(format!("{base}/v1/sql"))).json(&json!({"sql": "UPDATE products SET sold = 9 WHERE price >= 0"})).send().await.unwrap();
    assert_eq!(r.status(), 503);
    let j: Json = r.json().await.unwrap();
    assert!(j["error"].as_str().unwrap().starts_with("shard 1 takes no writes now, so nothing was written"), "{j}");
    let n: Json = client().post(format!("http://127.0.0.1:{}/v1/sql", a.http)).json(&json!({"sql": "SELECT COUNT(*) FROM products WHERE sold = 9"})).send().await.unwrap().json().await.unwrap();
    assert_eq!(n["rows"][0][0], 0, "{n}");

    // Both are audited, whatever their answers.
    let changes = audit("kind=change").await;
    let sql: Vec<&Json> = changes["entries"].as_array().unwrap().iter().filter(|e| e["route"] == "/v1/sql" && e["object"] == "table products").collect();
    assert_eq!(sql.len(), 2, "{changes}");
    assert_eq!((sql[0]["status"].as_u64(), sql[0]["reason"].as_str()), (Some(503), Some("fix notes")), "the newest first: {changes}");
    assert!(sql[1]["after"]["shards"].is_array(), "{changes}");

    // Every table's view lists the copy that does not answer.
    let t: Json = client().get(format!("{base}/v1/index/tables")).send().await.unwrap().json().await.unwrap();
    let products = t["tables"].as_array().unwrap().iter().find(|t| t["table"] == "products").cloned().unwrap_or_else(|| panic!("{t}"));
    let down = products["stale"].as_array().unwrap().iter().find(|s| s["short"] == sb).cloned().unwrap_or_else(|| panic!("{t}"));
    assert_eq!(down["down"], true, "{t}");
    assert!(down["since_ms"].as_u64().is_some(), "{t}");
}

/// Every table's writes a minute are the last minutes', not an average
/// since the server started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tables_writes_a_minute_are_the_last_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let (svc, _) = local(dir.path(), |_| {}).await;
    chloedb_server::inspect::tables(&svc).await.unwrap();
    // That look, as if a minute ago; then six writes.
    for look in svc.tables_counts.lock().unwrap().iter_mut() {
        look.0 -= 60_000;
    }
    for i in 0..6 {
        svc.write_ops(vec![json!({"op": "set", "key": format!("p{i:07}"), "field": "sold", "value": 3})], None).await.unwrap();
    }
    *svc.tables_kept.lock().await = None;
    let t = chloedb_server::inspect::tables(&svc).await.unwrap();
    let p = &t["tables"][0];
    assert_eq!(p["rate_window_s"].as_u64(), Some(60), "{t}");
    let w = p["writes_per_min"].as_f64().unwrap();
    assert!((5.9..=6.0).contains(&w), "{t}");
}

/// A shard of two copies with failover, one of them down: no election can
/// win, and its `why` says what to do instead.
#[test]
fn a_shard_with_one_of_two_copies_cannot_elect_a_primary() {
    let mut s = json!({"replicas": [
        {"url": "u0", "short": "s0-0", "status": "down", "role": "unknown", "takes_writes": false},
        {"url": "u1", "short": "s0-1", "status": "ok", "role": "replica", "takes_writes": false, "detail": {"capability": {"failover_after_ms": 3000, "serves_reads": true}}},
    ]});
    chloedb_server::coord::shard_state(0, &mut s);
    assert_eq!((s["state"].as_str(), s["takes_writes"].as_bool()), (Some("no_writes"), Some(false)), "{s}");
    let why = s["why"].as_str().unwrap();
    assert!(why.contains("it cannot elect a primary until a second copy answers: bring s0-0 back, or, if it is gone for good, make s0-1 the primary by hand") && !why.contains("may be under way"), "{why}");
    assert_eq!(chloedb_server::coord::cannot_elect(5, 2), "it cannot elect a primary until 3 of its 5 copies answer");
}

fn client() -> reqwest::Client {
    chloedb_server::http_client().build().unwrap()
}

/// The command center's requests (`x-strata-client: command-center`) are
/// left out of `/v1/routes`; reads are not audited, changes are, with the
/// client, its address and the reason it gave.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_centers_requests_are_tagged_and_only_changes_audited() {
    let dir = tempfile::tempdir().unwrap();
    let (_svc, base) = local(dir.path(), |s| {
        s.identity = chloedb_server::service::Identity { name: Some("prod/search".into()), environment: Some("production".into()), public_url: None };
    })
    .await;
    let ui = |r: reqwest::RequestBuilder| r.header("x-strata-client", "command-center");
    for _ in 0..3 {
        assert!(ui(client().get(format!("{base}/v1/queries"))).send().await.unwrap().status().is_success());
        assert!(ui(client().get(format!("{base}/v1/admin/audit"))).send().await.unwrap().status().is_success());
    }
    client().get(format!("{base}/v1/stats")).header("user-agent", "nightly-reports/2.3 (linux)").send().await.unwrap();
    let routes: Json = client().get(format!("{base}/v1/routes")).send().await.unwrap().json().await.unwrap();
    let names: Vec<&str> = routes["http"].as_array().unwrap().iter().map(|r| r["route"].as_str().unwrap()).collect();
    assert!(names.contains(&"/v1/stats") && !names.contains(&"/v1/queries") && !names.contains(&"/v1/admin/audit"), "{names:?}");

    // A change, with a reason (encoded, as the command center sends one).
    let r = ui(client().post(format!("{base}/v1/admin/checkpoint")).header("x-strata-reason", "before%20the%20upgrade%20%E2%9C%93")).json(&json!({"name": "x"})).send().await.unwrap();
    assert!(r.status().is_client_error(), "no backup dir: refused, and still audited as a change asked for");
    let audit: Json = client().get(format!("{base}/v1/admin/audit")).send().await.unwrap().json().await.unwrap();
    let entries = audit["entries"].as_array().unwrap();
    assert!(entries.iter().all(|e| e["method"] != "GET"), "reads are not audited: {entries:?}");
    let e = entries.iter().find(|e| e["route"] == "/v1/admin/checkpoint").unwrap();
    assert_eq!((e["kind"].as_str(), e["client"].as_str(), e["reason"].as_str(), e["addr"].as_str(), e["actor"].as_str()), (Some("change"), Some("command-center"), Some("before the upgrade ✓"), Some("127.0.0.1"), Some("person")), "{e}");

    // A change is an event; a note is one too, and audited as a note.
    let r = ui(client().post(format!("{base}/v1/events/notes"))).json(&json!({"text": "deploying 1.2", "where": {"shard": 0}})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let ev: Json = client().get(format!("{base}/v1/events")).send().await.unwrap().json().await.unwrap();
    let kinds: Vec<&str> = ev["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["note", "change"], "the newest first: {ev}");
    assert_eq!((ev["events"][0]["title"].as_str(), ev["events"][0]["where"]["shard"].as_u64()), (Some("deploying 1.2"), Some(0)));
    assert!(ev["events"][1]["title"].as_str().unwrap().contains("/v1/admin/checkpoint"), "{ev}");
    let notes: Json = client().get(format!("{base}/v1/events?kind=note")).send().await.unwrap().json().await.unwrap();
    assert_eq!(notes["events"].as_array().unwrap().len(), 1);
    let audit: Json = client().get(format!("{base}/v1/admin/audit")).send().await.unwrap().json().await.unwrap();
    assert!(audit["entries"].as_array().unwrap().iter().any(|e| e["kind"] == "note" && e["route"] == "/v1/events/notes"), "{audit}");
    assert_eq!(client().post(format!("{base}/v1/events/notes")).json(&json!({"text": " "})).send().await.unwrap().status(), 400);

    // A row write from the command center is a change, with its statement.
    let r = ui(client().post(format!("{base}/v1/sql"))).header("x-strata-reason", "fix a price").json(&json!({"sql": "UPDATE products SET sold = 1 WHERE id = 'p0000001'"})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let r = client().post(format!("{base}/v1/sql")).json(&json!({"sql": "UPDATE products SET sold = 2 WHERE id = 'p0000002'"})).send().await.unwrap();
    assert!(r.status().is_success());
    let changes: Json = client().get(format!("{base}/v1/admin/audit?kind=change")).send().await.unwrap().json().await.unwrap();
    let e = &changes["entries"][0];
    assert_eq!((e["route"].as_str(), e["object"].as_str(), e["reason"].as_str(), e["after"]["rows"].as_u64()), (Some("/v1/sql"), Some("table products"), Some("fix a price"), Some(1)), "{changes}");
    assert_eq!(changes["entries"].as_array().unwrap().iter().filter(|e| e["route"] == "/v1/sql").count(), 1, "a client's own writes are its own: {changes}");
    assert!(changes["entries"].as_array().unwrap().iter().all(|e| e["kind"] == "change"));
    let found: Json = client().get(format!("{base}/v1/admin/audit?q=checkpoint")).send().await.unwrap().json().await.unwrap();
    assert_eq!(found["entries"].as_array().unwrap().len(), 1, "{found}");

    let me: Json = ui(client().get(format!("{base}/v1/whoami"))).send().await.unwrap().json().await.unwrap();
    assert_eq!(me["cluster"], json!({"name": "prod/search", "environment": "production", "public_url": null}));
    assert_eq!(me["caller"]["client"], "command-center");

    // A shard server's fleet health is of itself.
    let f: Json = client().get(format!("{base}/v1/health/fleet")).send().await.unwrap().json().await.unwrap();
    assert_eq!(f["api"], json!({"ok": true, "server": "shard"}));
    let shards = f["checks"].as_array().unwrap().iter().find(|c| c["id"] == "shards").cloned().unwrap();
    assert_eq!((shards["status"].as_str(), shards["value"]["total"].as_u64()), (Some("ok"), Some(1)), "{f}");
}
