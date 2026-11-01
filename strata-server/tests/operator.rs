//! The operator's steps ([`strata_server::operator::reconcile`]) on a
//! platform of server processes on this machine, which stand for a
//! Kubernetes cluster's pods: each set of copies is a StatefulSet's pods,
//! each with a directory for its volume, and the coordinator a process
//! started again when its `--shards` list changes. A cluster is made,
//! grows and shrinks in shards and in copies, hands a primary over before
//! its copy goes, starts every copy again with new settings one at a
//! time, and takes back a copy that lost its volume, answering
//! every row throughout: open, and with a cluster token of its own, the
//! admin token held by the coordinator alone.

#![cfg(unix)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata_server::operator::{noted, reconcile, Admin, Gates, Outcome, Phase, Platform, Rollout, RolloutSpec, Set, Spec};

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
    /// The settings it was started with ([`Spec::server_config`]).
    config: String,
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
    /// The settings each set starts its copies with, as its StatefulSet's
    /// pod template holds them.
    configs: BTreeMap<u32, Spec>,
    ports: HashMap<(u32, usize), (u16, u16)>,
    coordinator: Option<(Vec<Vec<String>>, Pod)>,
}

struct Procs {
    dir: PathBuf,
    schema: PathBuf,
    coordinator_port: u16,
    /// The admin token and the cluster token, as `tokenSecret` and
    /// `clusterTokenSecret` give them: the servers take the cluster token,
    /// the coordinator both.
    tokens: Option<(String, String)>,
    /// The spec the operator works to, as the `StrataCluster` the
    /// platform's objects are made from.
    spec: Mutex<Spec>,
    state: Mutex<State>,
}

impl Procs {
    fn new(dir: &Path, tokens: Option<(&str, &str)>) -> Procs {
        let schema = dir.join("schema.json");
        std::fs::write(&schema, serde_json::to_string(&strata_server::demo::schema(ROWS)).unwrap()).unwrap();
        let tokens = tokens.map(|(a, c)| (a.to_string(), c.to_string()));
        Procs { dir: dir.to_path_buf(), schema, coordinator_port: port(), tokens, spec: Mutex::default(), state: Mutex::new(State::default()) }
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

    /// Starts copy `k` of set `id` as the StatefulSet's script does, with
    /// its set's settings.
    fn start(&self, id: u32, k: usize, demo: Option<(usize, usize)>) -> Pod {
        let set = self.state.lock().unwrap().configs.get(&id).cloned();
        let set = set.unwrap_or_else(|| self.spec.lock().unwrap().clone());
        let (http, grpc) = self.ports(id, k);
        let vol = self.volume(id, k);
        std::fs::create_dir_all(&vol).unwrap();
        let db = vol.join("db.strata");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_strata-server"));
        cmd.arg("--db").arg(&db).args(["--seq-no", "--listen", &format!("127.0.0.1:{http}"), "--grpc", &format!("127.0.0.1:{grpc}")]);
        cmd.args(["--failover-after-ms", &set.failover_after_ms.to_string(), "--log", "info"]);
        cmd.args(&set.args);
        if let Some((_, cluster)) = &self.tokens {
            cmd.args(["--cluster-token", cluster]);
        }
        if !db.exists() {
            match demo {
                Some((i, n)) if k == 0 => cmd.args(["--demo", &ROWS.to_string(), "--demo-shard", &format!("{i}/{n}")]),
                _ => cmd.arg("--schema").arg(&self.schema),
            };
        }
        let child = cmd.stdout(Stdio::null()).stderr(self.log(&format!("s{id}-{k}"))).spawn().unwrap();
        Pod { child, http, config: set.server_config() }
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
        let spec = self.spec.lock().unwrap().clone();
        self.state.lock().unwrap().configs.insert(id, spec);
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
        let pods = {
            let mut st = self.state.lock().unwrap();
            st.configs.remove(&id);
            st.sets.remove(&id)
        };
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_strata-server"));
        cmd.args(["--shards", &list, "--listen", &format!("127.0.0.1:{http}"), "--grpc", "off", "--log", "info"]);
        if let Some((admin, cluster)) = &self.tokens {
            cmd.args(["--token", admin, "--cluster-token", cluster]);
        }
        let child = cmd
            .stdout(Stdio::null())
            .stderr(self.log("coordinator"))
            .spawn()
            .unwrap();
        let config = self.spec.lock().unwrap().coordinator_config();
        self.state.lock().unwrap().coordinator = Some((shards.to_vec(), Pod { child, http, config }));
        Ok(())
    }

    async fn coordinator_config(&self) -> Result<Option<String>, String> {
        Ok(self.state.lock().unwrap().coordinator.as_ref().map(|(_, p)| p.config.clone()))
    }

    async fn configure(&self, id: u32, spec: &Spec) -> Result<bool, String> {
        let mut st = self.state.lock().unwrap();
        let changed = st.configs.get(&id).is_none_or(|c| c.server_config() != spec.server_config());
        if changed {
            st.configs.insert(id, spec.clone());
        }
        Ok(changed)
    }

    async fn stale(&self, spec: &Spec) -> Result<Vec<(u32, usize)>, String> {
        let want = spec.server_config();
        let st = self.state.lock().unwrap();
        Ok(st.sets.iter().flat_map(|(&id, (_, pods))| pods.iter().filter(|(_, p)| p.config != want).map(move |(&k, _)| (id, k))).collect())
    }

    async fn restart(&self, id: u32, ordinal: usize) -> Result<(), String> {
        let demo = {
            let mut st = self.state.lock().unwrap();
            let (demo, pods) = st.sets.get_mut(&id).ok_or("no such set")?;
            drop(pods.remove(&ordinal));
            *demo
        };
        let pod = self.start(id, ordinal, demo);
        self.state.lock().unwrap().sets.get_mut(&id).unwrap().1.insert(ordinal, pod);
        Ok(())
    }

    async fn grow_volumes(&self, _size: &str, _apply: bool) -> Result<usize, String> {
        // A directory has no size.
        Ok(0)
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
    /// What the last step left for the next (the resource's
    /// `status.rollout`).
    rollout: Mutex<Rollout>,
}

impl Run {
    fn new(procs: Procs, admin: Admin, spec: Spec) -> Run {
        Run { procs, admin, spec, rollout: Mutex::new(Rollout::default()) }
    }

    /// One step toward the spec.
    async fn once(&self) -> Outcome {
        *self.procs.spec.lock().unwrap() = self.spec.clone();
        let before = self.rollout.lock().unwrap().clone();
        let out = reconcile(&self.spec, &self.procs, &self.admin, &before).await;
        *self.rollout.lock().unwrap() = out.rollout.clone();
        out
    }

    /// Steps until `done` holds of a step, with every step's message.
    async fn until(&self, what: &str, done: impl Fn(&Outcome) -> bool) -> (Outcome, Vec<String>) {
        let t0 = Instant::now();
        let mut said: Vec<String> = Vec::new();
        loop {
            let out = self.once().await;
            if said.last() != Some(&out.message) {
                said.push(out.message.clone());
            }
            if done(&out) {
                return (out, said);
            }
            if out.phase == Phase::Failed {
                panic!("{what}: failed: {}\n{}", out.message, said.join("\n"));
            }
            if t0.elapsed() > Duration::from_secs(120) {
                panic!("{what}: not there after 120 s:\n{}\n{}", said.join("\n"), self.procs.logs());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Steps until the cluster is as the spec says, with every step's
    /// message.
    async fn settle(&self, what: &str) -> Vec<String> {
        self.until(what, |o| o.phase == Phase::Ready).await.1
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

    /// Every server started with `--slow-ms` `ms`.
    async fn slow_ms_everywhere(&self, ms: f64) {
        let servers: Vec<u16> = self.procs.state.lock().unwrap().sets.values().flat_map(|(_, pods)| pods.values().map(|p| p.http)).collect();
        assert_eq!(servers.len(), 9);
        for http in servers {
            let mut r = reqwest::Client::new().get(format!("http://127.0.0.1:{http}/v1/profile/settings"));
            if let Some((_, cluster)) = &self.procs.tokens {
                r = r.bearer_auth(cluster);
            }
            let j: Json = r.send().await.unwrap().json().await.unwrap();
            assert_eq!(j["started_slow_ms"], json!(ms), "{http}: {j}");
        }
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
    let procs = Procs::new(dir.path(), None);
    let admin = Admin::new(&format!("http://127.0.0.1:{}", procs.coordinator_port), None);
    let spec = Spec { shards: 2, replicas: 2, demo: Some(ROWS), failover_after_ms: 1000, ..Spec::default() };
    every_step(&mut Run::new(procs, admin, spec)).await;
}

/// The same steps with a cluster token: the operator holds the admin
/// token, which the servers do not take, and every step the coordinator
/// passes on to them goes with the cluster's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cluster_with_its_own_token_is_made_grown_shrunk_and_healed() {
    let dir = tempfile::tempdir().unwrap();
    let procs = Procs::new(dir.path(), Some(("admin-token", "cluster-token")));
    let admin = Admin::new(&format!("http://127.0.0.1:{}", procs.coordinator_port), Some("admin-token".into()));
    let spec = Spec {
        shards: 2,
        replicas: 2,
        demo: Some(ROWS),
        failover_after_ms: 1000,
        token_secret: Some("admin".into()),
        cluster_token_secret: Some("inner".into()),
        ..Spec::default()
    };
    let mut run = Run::new(procs, admin, spec);
    every_step(&mut run).await;
    // A server takes the cluster's token, not the admin's.
    let server = run.procs.state.lock().unwrap().sets[&0].1[&0].http;
    let ask = |token: &'static str| reqwest::Client::new().get(format!("http://127.0.0.1:{server}/v1/stats")).bearer_auth(token).send();
    assert_eq!(ask("admin-token").await.unwrap().status(), 401);
    assert_eq!(ask("cluster-token").await.unwrap().status(), 200);
}

/// A copy that cannot start with its settings (here an argument its
/// server does not take; on Kubernetes, requests no node has room for)
/// restarts at once with the settings that fix it, rather than being
/// waited for: while the cluster is made, and when a rollout of a bad
/// setting is rolled back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_that_cannot_start_restarts_with_the_settings_that_fix_it() {
    let dir = tempfile::tempdir().unwrap();
    let procs = Procs::new(dir.path(), None);
    let admin = Admin::new(&format!("http://127.0.0.1:{}", procs.coordinator_port), None);
    let bad = vec!["--no-such-flag".to_string()];
    let spec = Spec { shards: 1, replicas: 2, demo: Some(ROWS), failover_after_ms: 1000, args: bad.clone(), ..Spec::default() };
    let mut run = Run::new(procs, admin, spec);

    // Made with an argument no copy starts with: it waits for them.
    for _ in 0..20 {
        let o = run.once().await;
        assert!(o.phase == Phase::Creating && !o.message.contains("which has not started"), "{o:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Fixed, the copies restart with it at once.
    run.spec.args = Vec::new();
    let said = run.settle("made once its settings are fixed").await;
    assert!(said.iter().any(|m| m.contains("which has not started")), "{said:?}");
    assert_eq!(run.shape().await.0, vec![2]);
    assert_eq!(run.count().await, ROWS as i64);

    // The bad argument rolls out to a replica, which does not come back:
    // the rollout waits for it, and the primary serves.
    run.spec.args = bad;
    let (_, said) = run.until("a copy down", |o| o.message.contains("to be back")).await;
    assert!(said.iter().any(|m| m.starts_with("restarting")), "{said:?}");
    for _ in 0..10 {
        let o = run.once().await;
        assert!(o.message.contains("to be back"), "{o:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(run.count().await, ROWS as i64);
    // Rolled back, the copy restarts with the settings it had.
    run.spec.args = Vec::new();
    let said = run.settle("rolled back").await;
    assert!(said.iter().any(|m| m.contains("which has not started")), "{said:?}");
    assert_eq!(run.shape().await.0, vec![2]);
    assert_eq!(run.count().await, ROWS as i64);
}

/// Makes the cluster, grows it in shards and copies, heals a copy that
/// lost its volume, hands a primary over, shrinks it back and starts the
/// coordinator again, every row and write kept.
async fn every_step(run: &mut Run) {

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

    // A third shard, which takes a third of the slots. Paused while they
    // move, the moves stop after the one under way, and go on once the
    // pause is lifted.
    run.spec.shards = 3;
    let (_, mut said) = run.until("moving", |o| o.message.contains("evening out the slots")).await;
    run.spec.rollout.paused = true;
    let (out, more) = run.until("paused", |o| o.phase == Phase::Paused).await;
    said.extend(more);
    assert!(out.message.starts_with("paused (spec.rollout.paused); next:"), "{out:?}");
    let t0 = Instant::now();
    while run.admin.call(reqwest::Method::GET, "/v1/admin/moves", None).await.unwrap()["job"]["state"] == "running" {
        assert!(t0.elapsed() < Duration::from_secs(30), "the slot moves did not stop: {said:?}");
        assert_eq!(run.once().await.phase, Phase::Paused);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for _ in 0..5 {
        let o = run.once().await;
        assert!(o.phase == Phase::Paused && o.message.contains("slots"), "{o:?}");
    }
    run.spec.rollout.paused = false;
    said.extend(run.settle("three shards").await);
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

    // New settings: every copy starts again with them, one at a time, each
    // primary handing its shard over to a copy in step before it goes.
    run.spec.args = vec!["--slow-ms".into(), "777".into()];
    let said = run.settle("new settings").await;
    assert!(said.iter().any(|m| m.contains("restarting") && m.contains("with the new settings")), "{said:?}");
    assert!(said.iter().any(|m| m.contains("before") && m.contains("restarts with the new settings")), "{said:?}");
    run.slow_ms_everywhere(777.0).await;
    assert!(run.procs.stale(&run.spec).await.unwrap().is_empty());
    assert_eq!(run.shape().await.0, vec![3, 3, 3]);
    assert_eq!((run.count().await, run.sold().await), (ROWS as i64, sum));

    // Paused, the next settings wait.
    run.spec.args = vec!["--slow-ms".into(), "778".into()];
    run.spec.rollout.paused = true;
    let (out, _) = run.until("paused", |o| o.phase == Phase::Paused).await;
    assert!(out.message.starts_with("paused") && out.message.contains("restarting") && out.message.contains("(9 copies left)"), "{out:?}");
    assert!(out.rollout.paused && out.rollout.stale.len() == 9 && out.rollout.copies == 9, "{:?}", out.rollout);
    for _ in 0..10 {
        assert_eq!(run.once().await.phase, Phase::Paused);
    }
    assert_eq!(run.procs.stale(&run.spec).await.unwrap().len(), 9, "no copy restarts while paused");
    // Gated: the fullest volume over 0.01% of its disk, which every volume
    // here is, for a second holds the rollout, until it is resumed.
    run.spec.rollout = RolloutSpec { paused: false, gates: Gates { disk_used_ratio: Some(0.0001), ..Gates::default() }, hold_after_seconds: 1, soak_seconds: 1 };
    let (held, _) = run.until("held", |o| o.rollout.held.is_some()).await;
    let h = held.rollout.held.clone().unwrap();
    assert!(h.gate == "diskUsedRatio" && h.text.starts_with("the fullest volume was") && h.text.contains("over its 0.01% limit"), "{h:?}");
    assert!(held.phase == Phase::Paused && held.message.starts_with("held: the fullest volume was"), "{held:?}");
    for _ in 0..10 {
        let o = run.once().await;
        assert_eq!((o.phase, o.rollout.held.as_ref()), (Phase::Paused, Some(&h)), "{o:?}");
    }
    let left = run.procs.stale(&run.spec).await.unwrap().len();
    assert!((7..=9).contains(&left), "{left} copies left: a second goes before the gate holds");
    // A resume starts the gate's clock again: a second later it holds again.
    let resumed = noted(run.rollout.lock().unwrap().clone(), Some(&h.at_ms.to_string()), None, &run.spec);
    assert!(resumed.held.is_none() && resumed.gates.iter().all(|g| g.over_since_ms.is_none()), "{resumed:?}");
    *run.rollout.lock().unwrap() = resumed;
    let (again, _) = run.until("held again", |o| o.rollout.held.is_some()).await;
    assert!(again.rollout.held.as_ref().unwrap().at_ms > h.at_ms, "{again:?}");
    // Its limit raised over the number, the hold lifts, and the rest
    // restart, each restarted copy serving a second before the next.
    run.spec.rollout.gates.disk_used_ratio = Some(1.0);
    let said = run.settle("gate raised").await;
    assert!(said.iter().any(|m| m.contains("serves 1 s with them before the next restarts")), "{said:?}");
    run.slow_ms_everywhere(778.0).await;
    assert_eq!((run.count().await, run.sold().await), (ROWS as i64, sum));
    run.spec.rollout = RolloutSpec::default();

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
