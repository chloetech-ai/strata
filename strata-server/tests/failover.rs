//! Automatic failover between server processes (`--failover-after-ms`):
//! a shard of three copies whose primary is killed (SIGKILL) elects a
//! replica, which holds every write the old primary acknowledged; the old
//! primary started again is a replica of the new one, and a primary is
//! handed over by hand through the coordinator.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::coord::Coordinator;
use strata_server::demo;
use strata_server::service::{AgentConfig, Backend, QueryReq, Service};

const N: usize = 300;
const AFTER_MS: u64 = 600;

/// A port free now, of a block of 20 that this test process takes alone
/// (by its process id) below the ranges the other tests and the system's
/// own connections take ports from: a port taken with port 0 can be handed
/// to a connection's own end before the server, started again, binds it.
fn port() -> u16 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let base = 10_000 + (std::process::id() % 500) * 20;
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(n < 20, "this test process took its 20 ports");
        let p = (base + n) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return p;
        }
    }
}

/// A database of the demo products (`rows` of them), numbered.
fn create(path: &Path, rows: usize) {
    let opts = DbOptions { publish_snapshots: true, ship_log_bytes: 16 << 20, ..DbOptions::default() };
    let db = Db::create(path, demo::schema(N), opts).unwrap();
    if rows > 0 {
        db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
        db.load((0..rows).map(|i| demo::product(i, 7))).unwrap();
    }
    db.storage().start_seq_no().unwrap();
}

/// A `strata-server` process of one database, on ports it keeps when it
/// starts again.
struct Node {
    path: PathBuf,
    log: PathBuf,
    http: u16,
    grpc: u16,
    child: Option<Child>,
}

impl Node {
    fn new(dir: &Path, name: &str, rows: usize) -> Node {
        let path = dir.join(format!("{name}.strata"));
        create(&path, rows);
        Node { path, log: dir.join(format!("{name}.log")), http: port(), grpc: port(), child: None }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.grpc)
    }

    async fn start(&mut self) {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&self.log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_strata-server"))
            .arg("--db")
            .arg(&self.path)
            .args(["--listen", &format!("127.0.0.1:{}", self.http), "--grpc", &format!("127.0.0.1:{}", self.grpc)])
            .args(["--seq-no", "--log", "info", "--failover-after-ms", &AFTER_MS.to_string()])
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
        let r = reqwest::get(format!("http://127.0.0.1:{}/v1/health", self.http)).await.ok()?;
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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn set(i: usize, v: i64) -> Vec<Json> {
    vec![json!({"op": "set", "key": format!("p{i:07}"), "field": "sold", "value": v})]
}

/// A coordinator of `url`'s shard, and a server of it.
async fn coordinator(url: String) -> (Arc<Coordinator>, Arc<Service>) {
    let c = Arc::new(Coordinator::connect(vec![url]).await.unwrap());
    let agent = AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() };
    let schema = c.schema.clone();
    (c.clone(), Arc::new(Service::new(Backend::Cluster(c), schema, agent, 16, Duration::from_secs(10))))
}

/// What the coordinator reads of `sold`, by product.
async fn sold(c: &Arc<Service>) -> std::collections::BTreeMap<String, i64> {
    let q = QueryReq { sql: "SELECT id, sold FROM products LIMIT 10000".into(), ..Default::default() };
    let a = c.query(q).await.unwrap();
    a.rs.rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap())).collect()
}

/// The role a server says it has, and its term.
async fn role(n: &Node) -> (String, u64) {
    let r = n.replication().await;
    (r["role"].as_str().unwrap_or("down").to_string(), r["term"].as_u64().unwrap_or(0))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_primary_is_replaced_by_the_replica_its_copies_elect() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut a = Node::new(d, "a", N);
    let mut b = Node::new(d, "b", 0);
    let mut c = Node::new(d, "c", 0);
    for n in [&mut a, &mut b, &mut c] {
        n.start().await;
    }
    // One shard: a, its primary, and two replicas sent a copy of it.
    let (coord, svc) = coordinator(a.url()).await;
    coord.add_replica(0, &b.url(), false).await.unwrap();
    coord.add_replica(0, &c.url(), false).await.unwrap();
    until("both replicas are in sync and vote", &[&a, &b, &c], || async {
        let r = a.replication().await;
        r["replicas"].as_array().is_some_and(|v| v.len() == 2 && v.iter().all(|f| f["in_sync"] == true && f["member"] == true))
    })
    .await;
    let term = role(&a).await.1;

    // Writes, each acknowledged by a majority of the copies, until the
    // primary is killed; then each is tried again until one is answered.
    let mut acked = Vec::new();
    for i in 0..40 {
        svc.write_ops(set(i, 1000 + i as i64), None).await.unwrap();
        acked.push(i);
    }
    a.kill();
    let killed = Instant::now();
    let mut first = None;
    for i in 40..80 {
        let t0 = Instant::now();
        loop {
            match svc.write_ops(set(i, 1000 + i as i64), None).await {
                Ok(_) => break,
                Err(e) => {
                    assert!(t0.elapsed() < Duration::from_secs(30), "no write answered 30 s after the kill: {}\n{}\n{}", e.message, b.logs(), c.logs());
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        first.get_or_insert(killed.elapsed());
        acked.push(i);
    }
    let took = first.unwrap();
    assert!(took < Duration::from_secs(15), "writes were answered again {took:?} after the kill");

    // One replica was elected, in a newer term, and the other follows it.
    let (rb, rc) = (role(&b).await, role(&c).await);
    let (new, other) = match (rb.0.as_str(), rc.0.as_str()) {
        ("primary", "replica") => (&b, &c),
        ("replica", "primary") => (&c, &b),
        _ => panic!("one primary and one replica, not {rb:?} and {rc:?}"),
    };
    assert!(role(new).await.1 > term, "the new primary's term is newer than {term}");
    // No acknowledged write is lost.
    let got = sold(&svc).await;
    for &i in &acked {
        assert_eq!(got.get(&format!("p{i:07}")), Some(&(1000 + i as i64)), "acknowledged write {i}");
    }
    assert_eq!(got.len(), N);
    until("the other replica holds every write", &[new, other], || async {
        other.replication().await["seq_no"] == new.replication().await["seq_no"]
    })
    .await;

    // The old primary, started again, is a replica the new one ships to.
    a.start().await;
    until("the old primary follows the new one", &[&a, new, other], || async {
        let r = new.replication().await;
        role(&a).await.0 == "replica" && r["replicas"].as_array().is_some_and(|v| v.iter().all(|f| f["in_sync"] == true))
    })
    .await;
    svc.write_ops(set(80, 1080), None).await.unwrap();
    until("the old primary applies the new one's writes", &[&a, new], || async { a.replication().await["seq_no"] == new.replication().await["seq_no"] }).await;
    // And the shard has one primary.
    let h = coord.health().await;
    assert!(h["shards"][0].get("primaries").is_none(), "{h}");

    // A handover by hand: the old primary is made the primary again, by
    // the votes of the others, and the one it replaces steps down.
    let j = coord.promote(0, &a.url(), false).await.unwrap();
    assert_eq!(j["primary"], json!(a.url()), "{j}");
    until("the replaced primary steps down", &[&a, new], || async { role(new).await.0 == "replica" && role(&a).await.0 == "primary" }).await;
    svc.write_ops(set(81, 1081), None).await.unwrap();
    let got = sold(&svc).await;
    assert_eq!((got[&format!("p{:07}", 80)], got[&format!("p{:07}", 81)]), (1080, 1081));
    until("every copy holds every write", &[&a, &b, &c], || async {
        let last = a.replication().await["seq_no"].clone();
        b.replication().await["seq_no"] == last && c.replication().await["seq_no"] == last
    })
    .await;
}

/// A shard elects a primary only while a majority of its copies is up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shard_with_a_majority_up_elects_and_one_without_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut a = Node::new(d, "a", N);
    let mut b = Node::new(d, "b", 0);
    let mut c = Node::new(d, "c", 0);
    for n in [&mut a, &mut b, &mut c] {
        n.start().await;
    }
    let (coord, svc) = coordinator(a.url()).await;
    coord.add_replica(0, &b.url(), false).await.unwrap();
    coord.add_replica(0, &c.url(), false).await.unwrap();
    until("both replicas are in sync and vote", &[&a, &b, &c], || async {
        let r = a.replication().await;
        r["replicas"].as_array().is_some_and(|v| v.len() == 2 && v.iter().all(|f| f["in_sync"] == true && f["member"] == true))
    })
    .await;
    svc.write_ops(set(1, 1001), None).await.unwrap();

    // The primary and a replica killed, the one left is no majority: it
    // asks, is not elected, and stays a replica; writes are refused.
    a.kill();
    b.kill();
    tokio::time::sleep(Duration::from_millis(AFTER_MS * 4)).await;
    assert_eq!(role(&c).await.0, "replica", "{}", c.logs());
    let e = svc.write_ops(set(2, 1002), None).await.unwrap_err();
    assert!(e.status >= 500, "{}", e.message);
    assert!(c.logs().contains("not elected"), "it asked, and lost: {}", c.logs());

    // One more copy up is a majority: one of the two is elected.
    b.start().await;
    until("one of the two is elected", &[&b, &c], || async { role(&b).await.0 == "primary" || role(&c).await.0 == "primary" }).await;
    let t0 = Instant::now();
    while svc.write_ops(set(2, 1002), None).await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(30), "no write answered after the election");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let got = sold(&svc).await;
    assert_eq!((got[&format!("p{:07}", 1)], got[&format!("p{:07}", 2)]), (1001, 1002));
}
