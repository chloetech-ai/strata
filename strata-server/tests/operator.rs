//! The operator's steps ([`strata_server::operator::reconcile`]) on a
//! platform of server processes on this machine, which stand for a
//! Kubernetes cluster's pods: each set of copies is a StatefulSet's pods,
//! each with a directory for its volume, and the coordinator a process
//! started again when its `--shards` list changes. A cluster is made,
//! grows and shrinks in shards and in copies, hands a primary over before
//! its copy goes, and takes back a copy that lost its volume, answering
//! every row throughout.

#![cfg(unix)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata_server::operator::{reconcile, Admin, Phase, Platform, Set, Spec};

const ROWS: usize = 300;

/// A port of a block of 60 this process takes alone (as in
/// tests/failover.rs).
fn port() -> u16 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let base = 10_000 + (std::process::id() % 160) * 60;
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(n < 60, "this test process took its 60 ports");
        let p = (base + n) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return p;
        }
    }
}

struct Pod {
    child: Child,
    http: u16,
}

impl Drop for Pod {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct State {
    /// Each set's demo shard, and its pods by ordinal.
    sets: BTreeMap<u32, (Option<(usize, usize)>, BTreeMap<usize, Pod>)>,
    ports: HashMap<(u32, usize), (u16, u16)>,
    coordinator: Option<(Vec<Vec<String>>, Pod)>,
}

struct Procs {
    dir: PathBuf,
    schema: PathBuf,
    coordinator_port: u16,
    failover_after_ms: u64,
    state: Mutex<State>,
}

impl Procs {
    fn new(dir: &Path, failover_after_ms: u64) -> Procs {
        let schema = dir.join("schema.json");
        std::fs::write(&schema, serde_json::to_string(&strata_server::demo::schema(ROWS)).unwrap()).unwrap();
        Procs { dir: dir.to_path_buf(), schema, coordinator_port: port(), failover_after_ms, state: Mutex::new(State::default()) }
    }

    fn ports(&self, id: u32, k: usize) -> (u16, u16) {
        *self.state.lock().unwrap().ports.entry((id, k)).or_insert_with(|| (port(), port()))
    }

    fn volume(&self, id: u32, k: usize) -> PathBuf {
        self.dir.join(format!("s{id}-{k}"))
    }

    fn log(&self, name: &str) -> std::fs::File {
        std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("{name}.log"))).unwrap()
    }

    /// Starts copy `k` of set `id` as the StatefulSet's script does.
    fn start(&self, id: u32, k: usize, demo: Option<(usize, usize)>) -> Pod {
        let (http, grpc) = self.ports(id, k);
        let vol = self.volume(id, k);
        std::fs::create_dir_all(&vol).unwrap();
        let db = vol.join("db.strata");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_strata-server"));
        cmd.arg("--db").arg(&db).args(["--seq-no", "--listen", &format!("127.0.0.1:{http}"), "--grpc", &format!("127.0.0.1:{grpc}")]);
        cmd.args(["--failover-after-ms", &self.failover_after_ms.to_string(), "--log", "info"]);
        if !db.exists() {
            match demo {
                Some((i, n)) if k == 0 => cmd.args(["--demo", &ROWS.to_string(), "--demo-shard", &format!("{i}/{n}")]),
                _ => cmd.arg("--schema").arg(&self.schema),
            };
        }
        let child = cmd.stdout(Stdio::null()).stderr(self.log(&format!("s{id}-{k}"))).spawn().unwrap();
        Pod { child, http }
    }

    /// Copy `k` of set `id` comes back without its volume.
    fn lose_volume(&self, id: u32, k: usize) {
        let demo = {
            let mut st = self.state.lock().unwrap();
            let (demo, pods) = st.sets.get_mut(&id).unwrap();
            drop(pods.remove(&k));
            *demo
        };
        std::fs::remove_dir_all(self.volume(id, k)).unwrap();
        let pod = self.start(id, k, demo);
        self.state.lock().unwrap().sets.get_mut(&id).unwrap().1.insert(k, pod);
    }

    fn logs(&self) -> String {
        let mut out = String::new();
        for e in std::fs::read_dir(&self.dir).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "log") {
                let text = std::fs::read_to_string(e.path()).unwrap_or_default();
                let tail: Vec<&str> = text.lines().rev().take(15).collect();
                out += &format!("--- {}\n{}\n", e.path().display(), tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
            }
        }
        out
    }
}

async fn ready(http: u16) -> bool {
    reqwest::get(format!("http://127.0.0.1:{http}/readyz")).await.is_ok_and(|r| r.status().is_success())
}

impl Platform for Procs {
    async fn sets(&self) -> Result<Vec<Set>, String> {
        let pods: Vec<(u32, usize, Vec<(usize, u16)>)> = {
            let st = self.state.lock().unwrap();
            st.sets.iter().map(|(&id, (_, pods))| (id, pods.len(), pods.iter().map(|(&k, p)| (k, p.http)).collect())).collect()
        };
        let mut sets = Vec::new();
        for (id, replicas, pods) in pods {
            let mut up = Vec::new();
            for (k, http) in pods {
                if ready(http).await {
                    up.push(k);
                }
            }
            sets.push(Set { id, replicas, ready: up });
        }
        Ok(sets)
    }

    async fn create_set(&self, id: u32, replicas: usize, demo: Option<(usize, usize)>) -> Result<(), String> {
        let pods = (0..replicas).map(|k| (k, self.start(id, k, demo))).collect();
        self.state.lock().unwrap().sets.insert(id, (demo, pods));
        Ok(())
    }

    async fn scale_set(&self, id: u32, replicas: usize) -> Result<(), String> {
        let (demo, have) = {
            let st = self.state.lock().unwrap();
            let (demo, pods) = &st.sets[&id];
            (*demo, pods.len())
        };
        for k in have..replicas {
            let pod = self.start(id, k, demo);
            self.state.lock().unwrap().sets.get_mut(&id).unwrap().1.insert(k, pod);
        }
        for k in replicas..have {
            drop(self.state.lock().unwrap().sets.get_mut(&id).unwrap().1.remove(&k));
            // The pod's volume goes with it (whenScaled: Delete).
            let _ = std::fs::remove_dir_all(self.volume(id, k));
        }
        Ok(())
    }

    async fn delete_set(&self, id: u32) -> Result<(), String> {
        let pods = self.state.lock().unwrap().sets.remove(&id);
        if let Some((_, pods)) = pods {
            for k in pods.keys() {
                let _ = std::fs::remove_dir_all(self.volume(id, *k));
            }
        }
        Ok(())
    }

    async fn coordinator(&self) -> Result<Option<(Vec<Vec<String>>, usize)>, String> {
        Ok(self.state.lock().unwrap().coordinator.as_ref().map(|(l, _)| (l.clone(), 1)))
    }

    async fn set_coordinator(&self, shards: &[Vec<String>], _replicas: usize) -> Result<(), String> {
        drop(self.state.lock().unwrap().coordinator.take());
        let list = shards.iter().map(|s| s.join("|")).collect::<Vec<_>>().join(",");
        let http = self.coordinator_port;
        let child = Command::new(env!("CARGO_BIN_EXE_strata-server"))
            .args(["--shards", &list, "--listen", &format!("127.0.0.1:{http}"), "--grpc", "off", "--log", "info"])
            .stdout(Stdio::null())
            .stderr(self.log("coordinator"))
            .spawn()
            .unwrap();
        self.state.lock().unwrap().coordinator = Some((shards.to_vec(), Pod { child, http }));
        Ok(())
    }

    fn url(&self, id: u32, k: usize) -> String {
        format!("http://127.0.0.1:{}", self.ports(id, k).1)
    }

    fn set_of(&self, url: &str) -> Option<(u32, usize)> {
        let p: u16 = url.rsplit(':').next()?.parse().ok()?;
        self.state.lock().unwrap().ports.iter().find(|(_, &(_, g))| g == p).map(|(&k, _)| k)
    }
}

struct Run {
    procs: Procs,
    admin: Admin,
    spec: Spec,
}

impl Run {
    /// Steps until the cluster is as the spec says, with every step's
    /// message.
    async fn settle(&self, what: &str) -> Vec<String> {
        let t0 = Instant::now();
        let mut said: Vec<String> = Vec::new();
        loop {
            let out = reconcile(&self.spec, &self.procs, &self.admin).await;
            if said.last() != Some(&out.message) {
                said.push(out.message.clone());
            }
            match out.phase {
                Phase::Ready => return said,
                Phase::Failed => panic!("{what}: failed: {}\n{}", out.message, said.join("\n")),
                _ => {}
            }
            if t0.elapsed() > Duration::from_secs(120) {
                panic!("{what}: not ready after 120 s:\n{}\n{}", said.join("\n"), self.procs.logs());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn sql(&self, sql: &str) -> Json {
        let a = self.admin.call(reqwest::Method::POST, "/v1/sql", Some(json!({"sql": sql}))).await;
        a.unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    async fn count(&self) -> i64 {
        self.sql("SELECT COUNT(*) FROM products").await["rows"][0][0].as_i64().unwrap()
    }

    async fn sold(&self) -> i64 {
        self.sql("SELECT SUM(sold) FROM products WHERE sold >= 100000").await["rows"][0][0].as_i64().unwrap_or(0)
    }

    async fn cluster(&self) -> Json {
        self.admin.call(reqwest::Method::GET, "/v1/cluster", None).await.unwrap()
    }

    /// Servers per shard and slots per shard.
    async fn shape(&self) -> (Vec<usize>, Vec<u64>) {
        let c = self.cluster().await;
        let shards = c["shards"].as_array().unwrap();
        (shards.iter().map(|s| s["replicas"].as_array().unwrap().len()).collect(), shards.iter().map(|s| s["slots_held"].as_u64().unwrap()).collect())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cluster_is_made_grown_shrunk_and_healed() {
    let dir = tempfile::tempdir().unwrap();
    let procs = Procs::new(dir.path(), 1000);
    let admin = Admin::new(&format!("http://127.0.0.1:{}", procs.coordinator_port), None);
    let spec = Spec { shards: 2, replicas: 2, demo: Some(ROWS), failover_after_ms: 1000, ..Spec::default() };
    let mut run = Run { procs, admin, spec };

    // Made: two shards of two copies, the demo products split over them.
    let said = run.settle("made").await;
    assert!(said.iter().any(|m| m.contains("starting the coordinator")), "{said:?}");
    assert_eq!(run.shape().await, (vec![2, 2], vec![2048, 2048]));
    assert_eq!(run.count().await, ROWS as i64);
    // Writes that each step must keep.
    let mut sum = 0;
    for i in 0..30 {
        let v = 100_000 + i;
        run.sql(&format!("UPDATE products SET sold = {v} WHERE id = 'p{i:07}'")).await;
        sum += v;
    }
    assert_eq!(run.sold().await, sum);

    // A third shard, which takes a third of the slots.
    run.spec.shards = 3;
    let said = run.settle("three shards").await;
    assert!(said.iter().any(|m| m.contains("joins the cluster")) && said.iter().any(|m| m.contains("evening out the slots")), "{said:?}");
    let (servers, slots) = run.shape().await;
    assert_eq!(servers, vec![2, 2, 2]);
    assert!(slots.iter().all(|&s| (1365..=1366).contains(&s)), "{slots:?}");
    assert_eq!((run.count().await, run.sold().await), (ROWS as i64, sum));

    // Three copies each.
    run.spec.replicas = 3;
    run.settle("three copies").await;
    assert_eq!(run.shape().await.0, vec![3, 3, 3]);

    // A copy that comes back without its volume leaves and joins again,
    // and is sent a copy of its primary.
    run.procs.lose_volume(1, 1);
    let lost = run.procs.url(1, 1);
    let t0 = Instant::now();
    loop {
        let said = run.settle("healed").await;
        if said.iter().any(|m| m.contains("came back without its data")) {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "the copy that lost its volume was not found: {said:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let c = run.cluster().await;
    assert!(c["shards"].as_array().unwrap().iter().any(|s| s["replicas"].as_array().unwrap().iter().any(|r| r["url"] == json!(lost))), "{c}");
    assert_eq!((run.count().await, run.sold().await), (ROWS as i64, sum));

    // Two copies each again. The highest copy of the first set is made its
    // shard's primary first: it hands the primary over before it goes.
    let top = run.procs.url(0, 2);
    let shard = c["shards"].as_array().unwrap().iter().position(|s| s["replicas"].as_array().unwrap().iter().any(|r| r["url"] == json!(top))).unwrap();
    run.admin.call(reqwest::Method::POST, &format!("/v1/admin/shards/{shard}/promote"), Some(json!({"url": top}))).await.unwrap();
    run.spec.replicas = 2;
    let said = run.settle("two copies").await;
    assert!(said.iter().any(|m| m.contains("handing the primary over")), "{said:?}");
    assert_eq!(run.shape().await.0, vec![2, 2, 2]);
    assert_eq!((run.count().await, run.sold().await), (ROWS as i64, sum));

    // Two shards again: the newest is drained, removed and deleted.
    run.spec.shards = 2;
    let said = run.settle("two shards").await;
    assert!(said.iter().any(|m| m.contains("draining shard")) && said.iter().any(|m| m.contains("leaves the cluster")), "{said:?}");
    assert_eq!(run.shape().await, (vec![2, 2], vec![2048, 2048]));
    assert_eq!((run.count().await, run.sold().await), (ROWS as i64, sum));
    assert!(!run.procs.volume(2, 0).exists(), "the removed shard's volumes are deleted");
    // And the coordinator, started again on its list, serves it all.
    let (list, _) = run.procs.coordinator().await.unwrap().unwrap();
    run.procs.set_coordinator(&list, 1).await.unwrap();
    let t0 = Instant::now();
    while run.admin.call(reqwest::Method::GET, "/v1/cluster", None).await.is_err() {
        assert!(t0.elapsed() < Duration::from_secs(30), "the coordinator did not start again: {}", run.procs.logs());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    run.settle("coordinator again").await;
    assert_eq!((run.count().await, run.sold().await), (ROWS as i64, sum));
}
