//! A Kubernetes operator for Strata clusters (`strata-server operator`).
//!
//! A `StrataCluster` resource says how many shards a cluster has, how many
//! copies each shard keeps, and what each server needs (image, volume,
//! schema). The operator makes one StatefulSet of copies per shard, with a
//! volume each, and a coordinator (a Deployment behind a Service), and
//! then changes the cluster through the coordinator's admin calls, one
//! step at a time, until it is as the resource says:
//!
//! - a pod that is ready and not in its shard joins it as a replica;
//! - more copies per shard: the StatefulSets grow, and the new pods join;
//!   fewer: the highest pod of each set leaves its shard (its primary is
//!   handed to another copy first), then the set shrinks, deleting its
//!   volume;
//! - more shards: a new set, which joins as a shard, then the slots are
//!   evened out; fewer: the newest shard is drained (its slots move to the
//!   others), removed, and its set and volumes deleted;
//! - a pod that comes back without its volume (its node was replaced)
//!   leaves its shard and joins again, and is sent a copy of the primary;
//! - the coordinator's `--shards` list is kept naming servers of the
//!   cluster, so that a coordinator that starts again finds it;
//! - other settings (image, arguments, resources, failover, write acks,
//!   tokens): each set starts its copies with the new ones, and the copies
//!   running older ones restart one at a time, each shard's primary handed
//!   to a copy already restarted first, and the next only once every copy
//!   is back and caught up; a bigger volume grows each copy's claim.
//!
//! What it does goes through [`Platform`] (Kubernetes objects, in
//! [`kube`]; processes on one machine in the tests) and [`Admin`] (the
//! coordinator's HTTP API), so that [`reconcile`] is the same everywhere.
//! Failover is the servers' own (`--failover-after-ms`): the operator does
//! not choose primaries, except to hand one over before its pod goes.

pub mod kube;

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

/// The API group, version and kind of the resource.
pub const GROUP: &str = "strata.chloetech.ai";
pub const VERSION: &str = "v1alpha1";
pub const KIND: &str = "StrataCluster";
pub const PLURAL: &str = "strataclusters";

/// What a `StrataCluster` asks for (`spec`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Spec {
    /// The strata-server image of every server.
    pub image: String,
    pub image_pull_policy: Option<String>,
    /// Shards, each holding a part of the slots.
    pub shards: usize,
    /// Copies of each shard: a primary and its replicas (3 keeps a
    /// shard writable through the loss of one).
    pub replicas: usize,
    /// Coordinators behind the cluster's Service.
    pub coordinators: usize,
    pub storage: Storage,
    /// The first table's schema (the JSON `--schema` takes). Other tables
    /// are made through the coordinator.
    pub schema: Option<Json>,
    /// Instead of a schema: the demo products, this many, split over the
    /// shards the cluster starts with.
    pub demo: Option<usize>,
    /// `--failover-after-ms` of every server (0: failover by hand).
    pub failover_after_ms: u64,
    /// `--write-acks` of every primary: all, majority or primary.
    pub write_acks: String,
    /// A Secret whose key `token` every server and coordinator requires
    /// (none: the servers answer anyone who reaches them, `--no-token`).
    /// With `cluster_token_secret`, only the coordinators take it: it is
    /// then the admin token of the API, the operator's included, and it
    /// may not make the calls between servers.
    pub token_secret: Option<String>,
    /// A Secret whose key `token` is the cluster's own token
    /// (`--cluster-token`): every server and coordinator sends it to the
    /// others, and only it makes the calls between them (replication,
    /// slot moves, key sharing, votes). Clients and the operator never
    /// hold it. It needs `token_secret`.
    pub cluster_token_secret: Option<String>,
    /// Resources of each server's and each coordinator's container.
    pub resources: Option<Json>,
    pub coordinator_resources: Option<Json>,
    /// More arguments for every server.
    pub args: Vec<String>,
    /// More arguments for every coordinator (`--hedge-ms 50`,
    /// `--partial`, ...).
    pub coordinator_args: Vec<String>,
    /// How changes are made: paused, or held by gates on the cluster's
    /// live numbers.
    pub rollout: RolloutSpec,
}

/// How the operator makes a change (`spec.rollout`). Not part of a
/// version of the cluster ([`Spec::version`]): pausing, or moving a gate,
/// changes no server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RolloutSpec {
    /// While true the operator takes no step toward the spec: no copy or
    /// shard is added or removed, no slot moves (a move under way stops
    /// after its run) and no copy restarts. A copy that comes back
    /// without its data still joins its shard again.
    pub paused: bool,
    /// Limits on the cluster's live numbers: one over its limit for
    /// `hold_after_seconds` holds the change until it is resumed
    /// (`POST /v1/admin/kube/resume`) or the spec changes.
    pub gates: Gates,
    pub hold_after_seconds: u64,
    /// How long a copy restarted with new settings serves before the next
    /// one restarts, so that the gates see it.
    pub soak_seconds: u64,
}

impl Default for RolloutSpec {
    fn default() -> RolloutSpec {
        RolloutSpec { paused: false, gates: Gates::default(), hold_after_seconds: 30, soak_seconds: 0 }
    }
}

/// The limits of `spec.rollout.gates`, each read from [`Numbers`]; none
/// is set by default.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Gates {
    /// The search p99 the coordinator's clients see, in milliseconds.
    pub p99_ms: Option<f64>,
    /// The share of searches that fail, 0 to 1.
    pub error_ratio: Option<f64>,
    /// The transactions a replica is behind its primary.
    pub replica_behind: Option<f64>,
    /// The fullest volume's used share, 0 to 1.
    pub disk_used_ratio: Option<f64>,
}

impl Gates {
    /// Each gate's name (its field in the spec) and limit.
    pub fn limits(&self) -> [(&'static str, Option<f64>); 4] {
        [("p99Ms", self.p99_ms), ("errorRatio", self.error_ratio), ("replicaBehind", self.replica_behind), ("diskUsedRatio", self.disk_used_ratio)]
    }
}

/// What a gate's number is, said in words.
fn gate_words(name: &str, v: f64) -> String {
    match name {
        "p99Ms" => format!("{v:.0} ms"),
        "errorRatio" | "diskUsedRatio" => {
            // Two digits that mean something: 64%, 2.5%, 0.01%.
            let p = v * 100.0;
            let d = if p >= 10.0 || p <= 0.0 { 0 } else if p >= 1.0 { 1 } else { (1.0 - p.log10().floor()).min(6.0) as usize };
            let t = format!("{p:.d$}");
            let t = if t.contains('.') { t.trim_end_matches('0').trim_end_matches('.').to_string() } else { t };
            format!("{t}%")
        }
        "replicaBehind" => format!("{v:.0} transactions"),
        _ => format!("{v}"),
    }
}

fn gate_title(name: &str) -> &'static str {
    match name {
        "p99Ms" => "search p99",
        "errorRatio" => "failed searches",
        "replicaBehind" => "replica lag",
        "diskUsedRatio" => "the fullest volume",
        _ => "a gate",
    }
}

/// The cluster's live numbers that the gates read, from the coordinator's
/// `/v1/cluster`: each over the servers' last sample (two seconds).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Numbers {
    /// The coordinator's search p99 (or, before it has one, the slowest
    /// server's).
    pub p99_ms: Option<f64>,
    /// The coordinator's share of failed searches (or the worst server's).
    pub error_ratio: Option<f64>,
    /// The most transactions a replica is behind, as its primary says.
    pub replica_behind: Option<f64>,
    /// The fullest volume of a server that answers.
    pub disk_used_ratio: Option<f64>,
}

impl Numbers {
    pub fn from(cluster: &Json) -> Numbers {
        let servers: Vec<&Json> = cluster["shards"].as_array().into_iter().flatten().flat_map(|s| s["replicas"].as_array().into_iter().flatten()).filter(|r| r["status"] != "down").collect();
        let worst = |k: &str| servers.iter().filter_map(|r| r["detail"]["load"][k].as_f64()).reduce(f64::max);
        let coordinator = &cluster["coordinator"]["load"];
        Numbers {
            p99_ms: coordinator["p99_ms"].as_f64().or_else(|| worst("p99_ms")),
            error_ratio: coordinator["error_ratio"].as_f64().or_else(|| worst("error_ratio")),
            replica_behind: worst("replica_behind_max"),
            disk_used_ratio: worst("disk_used_ratio"),
        }
    }

    /// The number gate `name` reads.
    pub fn get(&self, name: &str) -> Option<f64> {
        match name {
            "p99Ms" => self.p99_ms,
            "errorRatio" => self.error_ratio,
            "replicaBehind" => self.replica_behind,
            "diskUsedRatio" => self.disk_used_ratio,
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Storage {
    /// Each copy's volume.
    pub size: String,
    pub storage_class_name: Option<String>,
}

impl Default for Storage {
    fn default() -> Storage {
        Storage { size: "10Gi".into(), storage_class_name: None }
    }
}

impl Default for Spec {
    fn default() -> Spec {
        Spec {
            image: "strata-server:dev".into(),
            image_pull_policy: None,
            shards: 1,
            replicas: 3,
            coordinators: 1,
            storage: Storage::default(),
            schema: None,
            demo: None,
            failover_after_ms: 3000,
            write_acks: "all".into(),
            token_secret: None,
            cluster_token_secret: None,
            resources: None,
            coordinator_resources: None,
            args: Vec::new(),
            coordinator_args: Vec::new(),
            rollout: RolloutSpec::default(),
        }
    }
}

impl Spec {
    /// The settings every server starts with, as a short hash: a copy
    /// started with another hash runs older settings, and is restarted.
    pub fn server_config(&self) -> String {
        short_hash(&json!({
            "image": self.image, "imagePullPolicy": self.image_pull_policy, "args": self.args, "resources": self.resources,
            "failoverAfterMs": self.failover_after_ms, "writeAcks": self.write_acks,
            "tokenSecret": self.token_secret, "clusterTokenSecret": self.cluster_token_secret,
        }))
    }

    /// The settings every coordinator starts with, as a short hash.
    pub fn coordinator_config(&self) -> String {
        short_hash(&json!({
            "image": self.image, "imagePullPolicy": self.image_pull_policy, "resources": self.coordinator_resources,
            "args": self.coordinator_args, "tokenSecret": self.token_secret, "clusterTokenSecret": self.cluster_token_secret,
        }))
    }

    /// The spec but for `rollout`, as a short hash: another one is
    /// another version of the cluster.
    pub fn version(&self) -> String {
        let s = Spec { rollout: RolloutSpec::default(), ..self.clone() };
        short_hash(&serde_json::to_value(&s).unwrap_or(Json::Null))
    }

    /// What is wrong with it, if anything.
    pub fn check(&self) -> Result<(), String> {
        if self.shards == 0 || self.replicas == 0 || self.coordinators == 0 {
            return Err("shards, replicas and coordinators are 1 or more".into());
        }
        for (name, limit) in self.rollout.gates.limits() {
            match limit {
                Some(v) if !v.is_finite() || v < 0.0 => return Err(format!("rollout.gates.{name} is a number of 0 or more, not {v}")),
                Some(v) if v > 1.0 && name.ends_with("Ratio") => return Err(format!("rollout.gates.{name} is a share, 0 to 1, not {v}")),
                _ => {}
            }
        }
        if self.shards > 4096 {
            return Err("a cluster has at most 4,096 shards, one a slot".into());
        }
        if let Some(c) = &self.cluster_token_secret {
            if self.token_secret.is_none() {
                return Err("clusterTokenSecret needs tokenSecret: the operator, and whoever runs the cluster, call the coordinators with that token".into());
            }
            if self.token_secret.as_ref() == Some(c) {
                return Err("clusterTokenSecret and tokenSecret name the same Secret: the cluster's token would be the admin's".into());
            }
        }
        if !matches!(self.write_acks.as_str(), "all" | "majority" | "primary") {
            return Err(format!("writeAcks is all, majority or primary, not {:?}", self.write_acks));
        }
        match (&self.schema, self.demo) {
            (Some(_), Some(_)) => Err("give a schema or demo, not both".into()),
            (None, None) => Err("give the first table's schema, or demo (a number of demo products)".into()),
            (Some(s), None) => strata::schema::Schema::from_json_value(s).map(|_| ()).map_err(|e| format!("schema: {e}")),
            (None, Some(_)) => Ok(()),
        }
    }

    /// The schema every server is created with, as JSON.
    pub fn schema_json(&self) -> String {
        match (&self.schema, self.demo) {
            (Some(s), _) => s.to_string(),
            (None, n) => serde_json::to_string(&crate::demo::schema(n.unwrap_or(0))).expect("a schema serialises"),
        }
    }
}

/// The first 12 hex digits of the SHA-256 of `j`'s text.
fn short_hash(j: &Json) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(j.to_string().as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// A volume size as Kubernetes writes it (`10Gi`, `500M`, `1T`), in bytes.
pub fn quantity(q: &str) -> Option<u128> {
    let q = q.trim();
    let at = q.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(q.len());
    let (n, unit) = q.split_at(at);
    let n: f64 = n.parse().ok()?;
    let m: u128 = match unit {
        "" => 1,
        "Ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "Ti" => 1 << 40,
        "Pi" => 1 << 50,
        "k" => 1_000,
        "M" => 1_000_000,
        "G" => 1_000_000_000,
        "T" => 1_000_000_000_000,
        "P" => 1_000_000_000_000_000,
        _ => return None,
    };
    (n >= 0.0).then(|| (n * m as f64) as u128)
}

/// A set of copies of one shard (a StatefulSet): its id, how many copies
/// it asks for, and the ordinals of those ready.
#[derive(Clone, Debug, PartialEq)]
pub struct Set {
    pub id: u32,
    pub replicas: usize,
    pub ready: Vec<usize>,
}

/// What the cluster runs on.
#[allow(async_fn_in_trait)]
pub trait Platform {
    /// The shard sets there are.
    async fn sets(&self) -> Result<Vec<Set>, String>;
    /// Makes set `id` of `replicas` copies; with `demo` `(i, n)`, its
    /// first copy loads shard `i` of `n` of the demo products.
    async fn create_set(&self, id: u32, replicas: usize, demo: Option<(usize, usize)>) -> Result<(), String>;
    async fn scale_set(&self, id: u32, replicas: usize) -> Result<(), String>;
    /// Deletes set `id` and its copies' volumes.
    async fn delete_set(&self, id: u32) -> Result<(), String>;
    /// The coordinators' `--shards` list and how many there are, if they
    /// were made.
    async fn coordinator(&self) -> Result<Option<(Vec<Vec<String>>, usize)>, String>;
    async fn set_coordinator(&self, shards: &[Vec<String>], replicas: usize) -> Result<(), String>;
    /// The settings the coordinators were started with
    /// ([`Spec::coordinator_config`]), if they were made.
    async fn coordinator_config(&self) -> Result<Option<String>, String>;
    /// Makes set `id` start its copies with `spec`'s settings, without
    /// restarting those that run; true when it changed.
    async fn configure(&self, id: u32, spec: &Spec) -> Result<bool, String>;
    /// The copies (set and ordinal) running with other settings than
    /// `spec`'s.
    async fn stale(&self, spec: &Spec) -> Result<Vec<(u32, usize)>, String>;
    /// Restarts copy `ordinal` of set `id` with its set's settings; it
    /// keeps its volume.
    async fn restart(&self, id: u32, ordinal: usize) -> Result<(), String>;
    /// The copies' volumes smaller than `size`: how many, each grown to
    /// it with `apply`.
    async fn grow_volumes(&self, size: &str, apply: bool) -> Result<usize, String>;
    /// The gRPC URL of copy `ordinal` of set `id`.
    fn url(&self, id: u32, ordinal: usize) -> String;
    /// The set and ordinal of the copy at `url`, if it is one of this
    /// cluster's.
    fn set_of(&self, url: &str) -> Option<(u32, usize)>;
}

/// A coordinator's HTTP API.
#[derive(Clone)]
pub struct Admin {
    pub base: String,
    pub token: Option<String>,
    http: reqwest::Client,
}

impl Admin {
    pub fn new(base: &str, token: Option<String>) -> Admin {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().expect("an HTTP client");
        Admin { base: base.trim_end_matches('/').to_string(), token, http }
    }

    pub async fn call(&self, method: reqwest::Method, path: &str, body: Option<Json>) -> Result<Json, String> {
        let mut r = self.http.request(method.clone(), format!("{}{path}", self.base));
        if let Some(t) = &self.token {
            r = r.bearer_auth(t);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let a = r.send().await.map_err(|e| format!("{method} {path}: {e}"))?;
        let status = a.status();
        let j: Json = a.json().await.unwrap_or(Json::Null);
        if !status.is_success() {
            let why = j["error"].as_str().map(str::to_string).unwrap_or_else(|| j.to_string());
            return Err(format!("{method} {path}: {status}: {why}"));
        }
        Ok(j)
    }

    async fn post(&self, path: &str, body: Json) -> Result<Json, String> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    async fn delete(&self, path: &str) -> Result<Json, String> {
        self.call(reqwest::Method::DELETE, path, None).await
    }

    /// The cluster as the coordinator sees it.
    pub async fn view(&self) -> Result<View, String> {
        let (c, m) = tokio::join!(self.call(reqwest::Method::GET, "/v1/cluster", None), self.call(reqwest::Method::GET, "/v1/admin/moves", None));
        View::from(&c?, &m?)
    }
}

/// One server of a shard, as the coordinator last heard from it.
#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub url: String,
    pub up: bool,
    pub role: String,
    pub takes_writes: bool,
    /// A primary, or a replica its primary says is in sync.
    pub in_sync: bool,
    /// How long its process has run, as it says.
    pub uptime_s: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shard {
    pub servers: Vec<Server>,
    pub slots: usize,
}

/// The cluster map's shards and whether slots are moving.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub shards: Vec<Shard>,
    pub moving: bool,
    /// A job of moves runs on this coordinator (it can be stopped).
    pub job_running: bool,
    pub can_change: Result<(), String>,
    pub numbers: Numbers,
}

impl View {
    fn from(cluster: &Json, moves: &Json) -> Result<View, String> {
        let shards = cluster["shards"].as_array().ok_or_else(|| format!("the coordinator's /v1/cluster has no shards: {cluster}"))?;
        let shards = shards
            .iter()
            .map(|s| {
                let copies = s["replicas"].as_array().map(Vec::as_slice).unwrap_or_default();
                // Whether each replica is in sync, as its primary says.
                let primary = copies.iter().find(|r| r["takes_writes"] == true);
                let said = primary.and_then(|p| p["detail"]["health"]["replication"]["replicas"].as_array());
                let in_sync = |url: &str| said.is_some_and(|rs| rs.iter().any(|x| x["url"] == url && x["in_sync"] == true));
                Shard {
                    servers: copies
                        .iter()
                        .map(|r| {
                            let url = r["url"].as_str().unwrap_or_default().to_string();
                            let takes_writes = r["takes_writes"] == true;
                            let uptime_s = r["detail"]["system"]["uptime_s"].as_f64();
                            Server { in_sync: takes_writes || in_sync(&url), up: r["status"] != "down", role: r["role"].as_str().unwrap_or_default().to_string(), takes_writes, url, uptime_s }
                        })
                        .collect(),
                    slots: s["slots_held"].as_u64().unwrap_or(0) as usize,
                }
            })
            .collect();
        let job_running = moves["job"]["state"] == "running";
        let moving = !moves["moving"].is_null() || job_running;
        let can_change = match cluster["changes"]["ok"].as_bool() {
            Some(false) => Err(cluster["changes"]["why"].as_str().unwrap_or("the cluster map cannot change").to_string()),
            _ => Ok(()),
        };
        Ok(View { shards, moving, job_running, can_change, numbers: Numbers::from(cluster) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Phase {
    /// The cluster is being made.
    Creating,
    /// It changes toward the spec.
    Scaling,
    /// It would change toward the spec, but the change is paused
    /// (`spec.rollout.paused`) or held by a gate.
    Paused,
    /// As the spec says, every server answering.
    Ready,
    /// As the spec says, but servers do not answer.
    Degraded,
    /// The spec or the cluster is in a state the operator cannot change.
    Failed,
}

/// What a [`reconcile`] did or waits for, for the resource's status.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub phase: Phase,
    pub message: String,
    /// The shards: each one's set, slots and servers.
    pub shards: Json,
    /// What the next call takes from this one (`status.rollout`).
    pub rollout: Rollout,
}

impl Outcome {
    fn new(phase: Phase, message: impl Into<String>) -> Outcome {
        Outcome { phase, message: message.into(), shards: Json::Null, rollout: Rollout::default() }
    }
}

/// What the operator keeps of a change from one step to the next, in the
/// resource's status (`status.rollout`), and shows of it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Rollout {
    /// The version of the spec it is about ([`Spec::version`]).
    pub version: String,
    /// Each gate set, and since when its number is over its limit.
    pub gates: Vec<Gate>,
    /// What holds the change, until it is resumed or the spec changes.
    pub held: Option<Held>,
    pub paused: bool,
    /// A rollback, which no gate holds (its caller says so).
    pub ungated: bool,
    /// The copies that run older settings, of `copies`.
    pub stale: Vec<String>,
    pub copies: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Gate {
    /// Its field in `spec.rollout.gates`.
    pub name: String,
    pub limit: f64,
    /// Since when (milliseconds since the epoch) its number is over the
    /// limit.
    pub over_since_ms: Option<u64>,
}

/// A gate that held a change: its number then, and why, in words.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Held {
    pub gate: String,
    pub value: f64,
    pub limit: f64,
    pub at_ms: u64,
    pub text: String,
}

/// What stops the next step toward the spec, if anything.
enum Hold {
    Go,
    Paused,
    /// A hold from an earlier step, or one a gate makes now.
    Held(Held),
}

impl Hold {
    /// The outcome of not taking step `next`, when it is not taken.
    fn stop(&self, next: &str) -> Option<Outcome> {
        match self {
            Hold::Go => None,
            Hold::Paused => Some(Outcome::new(Phase::Paused, format!("paused (spec.rollout.paused); next: {next}"))),
            Hold::Held(h) => Some(Outcome::new(Phase::Paused, format!("held: {}; next: {next}", h.text))),
        }
    }

    /// The same for a step while a job of slot moves runs: the job stops
    /// once its move under way ends.
    async fn stop_moves(&self, admin: &Admin, view: &View, next: &str) -> Result<Option<Outcome>, String> {
        let Some(mut o) = self.stop(next) else { return Ok(None) };
        if view.job_running {
            admin.post("/v1/admin/moves/stop", json!({})).await?;
            o.message = format!("{}; the slot moves stop once the one under way ends", o.message);
        }
        Ok(Some(o))
    }
}

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The gates' clocks once `numbers` is read at `now`, with the hold
/// carried from `before` (a call for the same version, its gate set as
/// it was), and the hold a gate makes now: one over its limit for
/// `hold_after_seconds`. Another version starts every clock again, as
/// does a resume ([`noted`]).
pub fn gates(spec: &Spec, numbers: &Numbers, before: &Rollout, now: u64) -> (Rollout, Option<Held>) {
    let version = spec.version();
    let same = before.version == version;
    let gates: Vec<Gate> = spec
        .rollout
        .gates
        .limits()
        .into_iter()
        .filter_map(|(name, limit)| {
            let limit = limit?;
            let was = before.gates.iter().find(|g| same && g.name == name && g.limit == limit).and_then(|g| g.over_since_ms);
            let over = numbers.get(name).is_some_and(|v| v > limit);
            Some(Gate { name: name.into(), limit, over_since_ms: over.then(|| was.unwrap_or(now)) })
        })
        .collect();
    let ungated = before.ungated;
    // A hold lasts until it is resumed, the version changes, or its gate
    // is changed.
    let held = before.held.clone().filter(|h| same && !ungated && gates.iter().any(|g| g.name == h.gate && g.limit == h.limit));
    let after = spec.rollout.hold_after_seconds.saturating_mul(1000);
    let trip = match (ungated, &held) {
        (false, None) => gates.iter().find(|g| g.over_since_ms.is_some_and(|t| now.saturating_sub(t) >= after)).map(|g| {
            let value = numbers.get(&g.name).unwrap_or(g.limit);
            let over = now.saturating_sub(g.over_since_ms.unwrap_or(now)) / 1000;
            let text = format!("{} was {}, over its {} limit, for {over} s", gate_title(&g.name), gate_words(&g.name, value), gate_words(&g.name, g.limit));
            Held { gate: g.name.clone(), value, limit: g.limit, at_ms: now, text }
        }),
        _ => None,
    };
    (Rollout { version, gates, held, paused: spec.rollout.paused, ungated, stale: Vec::new(), copies: 0 }, trip)
}

/// Takes the cluster one step toward `spec`: one change to the platform
/// or one admin call, or none when it is there (or waits for something).
/// Called again and again; each call starts from what it finds, and from
/// `before`, the [`Outcome::rollout`] of the call before.
pub async fn reconcile<P: Platform>(spec: &Spec, p: &P, admin: &Admin, before: &Rollout) -> Outcome {
    let mut out = match step(spec, p, admin, before).await {
        Ok(o) => o,
        Err(e) => Outcome::new(Phase::Scaling, e),
    };
    // A step that read no live numbers keeps what `before` had of this
    // version; another version starts afresh.
    if out.rollout.version.is_empty() {
        let version = spec.version();
        let kept = if before.version == version || before.version.is_empty() { before.clone() } else { Rollout { ungated: before.ungated, ..Rollout::default() } };
        out.rollout = Rollout { version, paused: spec.rollout.paused, ..kept };
    }
    out
}

async fn step<P: Platform>(spec: &Spec, p: &P, admin: &Admin, before: &Rollout) -> Result<Outcome, String> {
    if let Err(e) = spec.check() {
        return Ok(Outcome::new(Phase::Failed, e));
    }
    let mut sets = p.sets().await?;
    sets.sort_by_key(|s| s.id);
    if sets.is_empty() {
        for i in 0..spec.shards {
            p.create_set(i as u32, spec.replicas, spec.demo.map(|_| (i, spec.shards))).await?;
        }
        return Ok(Outcome::new(Phase::Creating, format!("creating {} shards of {} copies", spec.shards, spec.replicas)));
    }
    let Some((listed, coordinators)) = p.coordinator().await? else {
        // The cluster is laid out by the first coordinator, over the first
        // copy of each set (the others join as replicas).
        let waiting = sets.iter().filter(|s| !s.ready.contains(&0)).count();
        if waiting > 0 {
            return Ok(Outcome::new(Phase::Creating, format!("waiting for the first copy of {waiting} of {} shards to start", sets.len())));
        }
        let first: Vec<Vec<String>> = sets.iter().map(|s| vec![p.url(s.id, 0)]).collect();
        p.set_coordinator(&first, 1).await?;
        return Ok(Outcome::new(Phase::Creating, "starting the coordinator, which lays the cluster out"));
    };
    let view = match admin.view().await {
        Ok(v) => v,
        Err(e) => return Ok(Outcome::new(Phase::Creating, format!("waiting for the coordinator: {e}"))),
    };
    let stale = p.stale(spec).await?;
    let (mut rollout, trip) = gates(spec, &view.numbers, before, now_ms());
    rollout.stale = stale.iter().map(|&(id, k)| p.url(id, k)).collect();
    rollout.copies = sets.iter().map(|s| s.replicas).sum();
    let hold = match rollout.held.clone().or_else(|| trip.clone()) {
        _ if spec.rollout.paused => Hold::Paused,
        Some(h) => Hold::Held(h),
        None => Hold::Go,
    };
    let mut out = act(spec, p, admin, &sets, &view, &listed, coordinators, &hold, &stale).await?;
    match out.phase {
        // A gate's hold stays once it held a step back.
        Phase::Paused if !spec.rollout.paused => rollout.held = rollout.held.or(trip),
        // Nothing is left to hold.
        Phase::Ready | Phase::Degraded => rollout.held = None,
        _ => {}
    }
    out.shards = shards_json(p, &view);
    out.rollout = rollout;
    Ok(out)
}

/// The set of each shard of `view`, by its first server of this cluster.
fn set_of_shard<P: Platform>(p: &P, s: &Shard) -> Option<u32> {
    s.servers.iter().find_map(|r| p.set_of(&r.url)).map(|(id, _)| id)
}

/// The first step toward `spec` of those below, each but the ones that
/// heal held back by `hold`.
#[allow(clippy::too_many_arguments)]
async fn act<P: Platform>(spec: &Spec, p: &P, admin: &Admin, sets: &[Set], view: &View, listed: &[Vec<String>], coordinators: usize, hold: &Hold, stale: &[(u32, usize)]) -> Result<Outcome, String> {
    let scaling = |m: String| Ok(Outcome::new(Phase::Scaling, m));
    macro_rules! unless_held {
        ($next:expr) => {
            if let Some(o) = hold.stop(&$next) {
                return Ok(o);
            }
        };
    }
    let shard_of: HashMap<u32, usize> = view.shards.iter().enumerate().filter_map(|(i, s)| set_of_shard(p, s).map(|id| (id, i))).collect();
    let in_map: Vec<&str> = view.shards.iter().flat_map(|s| s.servers.iter().map(|r| r.url.as_str())).collect();
    // A copy of a set that serves a shard, which joins it (again) if it is
    // not in it.
    let kept = |id: u32, k: usize| sets.iter().any(|s| s.id == id && k < s.replicas.min(spec.replicas)) && shard_of.contains_key(&id);
    // The coordinators' list must name servers of the cluster only: one
    // that starts again on a list naming a server the cluster left out for
    // good is refused.
    let gone = |u: &String| !in_map.contains(&u.as_str()) && !p.set_of(u).is_some_and(|(id, k)| kept(id, k));
    let mend = listed.iter().flatten().any(gone);
    if mend || coordinators != spec.coordinators {
        if !mend {
            unless_held!(format!("{} coordinators, from {coordinators}", spec.coordinators));
        }
        let now: Vec<Vec<String>> = view.shards.iter().map(|s| s.servers.iter().map(|r| r.url.clone()).collect()).collect();
        p.set_coordinator(&now, spec.coordinators).await?;
        return scaling(format!("updating the coordinators ({}) to the cluster's servers", spec.coordinators));
    }
    if p.coordinator_config().await?.is_some_and(|c| c != spec.coordinator_config()) {
        unless_held!("restarting the coordinators with the new settings".to_string());
        let now: Vec<Vec<String>> = view.shards.iter().map(|s| s.servers.iter().map(|r| r.url.clone()).collect()).collect();
        p.set_coordinator(&now, spec.coordinators).await?;
        return scaling("restarting the coordinators with the new settings, one at a time".into());
    }
    if let Err(why) = &view.can_change {
        return Ok(Outcome::new(Phase::Failed, format!("the cluster map cannot change: {why}")));
    }
    // A copy that came back without its volume is standalone in a shard of
    // several servers: it leaves, and joins again below.
    for (i, s) in view.shards.iter().enumerate() {
        if s.servers.len() < 2 {
            continue;
        }
        if let Some(r) = s.servers.iter().find(|r| r.up && r.role == "standalone") {
            admin.delete(&format!("/v1/admin/shards/{i}/replicas?url={}", r.url)).await?;
            return scaling(format!("{} came back without its data: it leaves shard {i}, to join it again", r.url));
        }
    }
    // Ready copies a shard lacks join it.
    for set in sets {
        let Some(&i) = shard_of.get(&set.id) else { continue };
        // Not one that leaves because the set shrinks.
        for &k in set.ready.iter().filter(|&&k| kept(set.id, k)) {
            let url = p.url(set.id, k);
            if !view.shards[i].servers.iter().any(|r| r.url == url) {
                admin.post(&format!("/v1/admin/shards/{i}/replicas"), json!({"url": url, "force": true})).await?;
                return scaling(format!("{url} joins shard {i}"));
            }
        }
    }
    // Copies per shard.
    for set in sets {
        let Some(&i) = shard_of.get(&set.id) else { continue };
        if set.replicas != spec.replicas {
            unless_held!(format!("shard {i}: {} copies, from {}", spec.replicas, set.replicas));
        }
        if set.replicas < spec.replicas {
            p.scale_set(set.id, spec.replicas).await?;
            return scaling(format!("shard {i}: {} copies, from {}", spec.replicas, set.replicas));
        }
        if set.replicas > spec.replicas {
            let url = p.url(set.id, set.replicas - 1);
            let servers = &view.shards[i].servers;
            match servers.iter().find(|r| r.url == url) {
                Some(r) if r.takes_writes => {
                    let to = servers.iter().find(|o| o.url != url && o.up && o.role == "replica").ok_or_else(|| format!("shard {i}: no replica answers to hand the primary {url} over to"))?;
                    admin.post(&format!("/v1/admin/shards/{i}/promote"), json!({"url": to.url})).await?;
                    return scaling(format!("shard {i}: handing the primary over from {url} to {}, before {url} goes", to.url));
                }
                Some(_) => {
                    admin.delete(&format!("/v1/admin/shards/{i}/replicas?url={url}")).await?;
                    return scaling(format!("{url} leaves shard {i}"));
                }
                None => {
                    p.scale_set(set.id, set.replicas - 1).await?;
                    return scaling(format!("shard {i}: {} copies, from {}", set.replicas - 1, set.replicas));
                }
            }
        }
    }
    // Shards.
    let pending: Vec<&Set> = sets.iter().filter(|s| !shard_of.contains_key(&s.id)).collect();
    let n = view.shards.len();
    if spec.shards > n {
        let Some(set) = pending.first() else {
            unless_held!(format!("a new shard: making its {} copies", spec.replicas));
            let id = sets.iter().map(|s| s.id + 1).max().unwrap_or(0);
            p.create_set(id, spec.replicas, None).await?;
            return scaling(format!("a new shard: making its {} copies", spec.replicas));
        };
        if !set.ready.contains(&0) {
            return scaling(format!("a new shard: waiting for its first copy ({} of {} ready)", set.ready.len(), set.replicas));
        }
        unless_held!(format!("shard {n} joins the cluster"));
        let mut ready: Vec<usize> = set.ready.iter().copied().filter(|&k| k < set.replicas).collect();
        ready.sort();
        let servers: Vec<String> = ready.iter().map(|&k| p.url(set.id, k)).collect();
        admin.post("/v1/admin/shards", json!({"servers": servers, "write_acks": spec.write_acks})).await?;
        return scaling(format!("shard {n} joins the cluster, served by {}", servers.join(", ")));
    }
    if spec.shards < n {
        // The newest shard goes.
        let (i, _) = view.shards.iter().enumerate().max_by_key(|(_, s)| set_of_shard(p, s)).ok_or("no shard")?;
        let s = &view.shards[i];
        if view.moving {
            if let Some(o) = hold.stop_moves(admin, view, &format!("moving slots off shard {i} ({} left)", s.slots)).await? {
                return Ok(o);
            }
            return scaling(format!("moving slots off shard {i} ({} left)", s.slots));
        }
        if s.slots > 0 {
            unless_held!(format!("draining shard {i} ({} slots)", s.slots));
            admin.post(&format!("/v1/admin/shards/{i}/drain"), json!({})).await?;
            return scaling(format!("draining shard {i}: its {} slots move to the others", s.slots));
        }
        unless_held!(format!("shard {i} leaves the cluster"));
        admin.delete(&format!("/v1/admin/shards/{i}")).await?;
        return scaling(format!("shard {i} leaves the cluster"));
    }
    if let Some(set) = pending.first() {
        p.delete_set(set.id).await?;
        return scaling(format!("deleting the copies of a shard that left (set {})", set.id));
    }
    // Slots even over the shards.
    if view.moving {
        if let Some(o) = hold.stop_moves(admin, view, "moving slots between shards").await? {
            return Ok(o);
        }
        return scaling("moving slots between shards".into());
    }
    let (lo, hi) = (view.shards.iter().map(|s| s.slots).min().unwrap_or(0), view.shards.iter().map(|s| s.slots).max().unwrap_or(0));
    if hi - lo > 1 {
        unless_held!(format!("evening out the slots over {n} shards ({lo} to {hi} each)"));
        admin.post("/v1/admin/rebalance", json!({})).await?;
        return scaling(format!("evening out the slots over {n} shards ({lo} to {hi} each)"));
    }
    if let Some(o) = roll(spec, p, admin, sets, view, hold, stale).await? {
        return Ok(o);
    }
    let down: Vec<&str> = view.shards.iter().flat_map(|s| s.servers.iter().filter(|r| !r.up).map(|r| r.url.as_str())).collect();
    let short = view.shards.iter().enumerate().filter(|(_, s)| s.servers.len() < spec.replicas).map(|(i, _)| i.to_string()).collect::<Vec<_>>();
    if !down.is_empty() {
        return Ok(Outcome::new(Phase::Degraded, format!("not answering: {}", down.join(", "))));
    }
    if !short.is_empty() {
        return Ok(Outcome::new(Phase::Scaling, format!("shards {} wait for copies to start", short.join(", "))));
    }
    Ok(Outcome::new(Phase::Ready, format!("{n} shards of {} copies", spec.replicas)))
}

/// New settings: each set starts its copies with them, the volumes grow,
/// and the copies running older settings (`stale`) restart one at a time.
/// The next restarts only once every copy is back and in sync, and the
/// copies restarted have served `soak_seconds`; a primary is first handed
/// to a copy of its shard in sync, one that runs the new settings if
/// there is one. A shard of one copy cannot be handed over: its copy
/// restarts, and its writes wait for it.
async fn roll<P: Platform>(spec: &Spec, p: &P, admin: &Admin, sets: &[Set], view: &View, hold: &Hold, stale: &[(u32, usize)]) -> Result<Option<Outcome>, String> {
    let scaling = |m: String| Ok(Some(Outcome::new(Phase::Scaling, m)));
    for set in sets {
        if p.configure(set.id, spec).await? {
            return scaling(format!("set {}: new copies start with the new settings", set.id));
        }
    }
    let small = p.grow_volumes(&spec.storage.size, false).await?;
    if small > 0 {
        let what = format!("growing {small} volume{} to {}", if small == 1 { "" } else { "s" }, spec.storage.size);
        if let Some(o) = hold.stop(&what) {
            return Ok(Some(o));
        }
        p.grow_volumes(&spec.storage.size, true).await?;
        return scaling(what);
    }
    if stale.is_empty() {
        return Ok(None);
    }
    let left = |n: usize| format!("{n} {}", if n == 1 { "copy" } else { "copies" });
    // One copy at a time: the last one restarted is back (its pod ready,
    // as the platform says, which knows before the coordinator does) and
    // every copy is caught up with its primary.
    let mut unsettled: Vec<String> = sets.iter().flat_map(|s| (0..s.replicas).filter(|k| !s.ready.contains(k)).map(|k| p.url(s.id, k))).collect();
    for r in view.shards.iter().flat_map(|s| &s.servers).filter(|r| !r.up || !r.in_sync) {
        if !unsettled.contains(&r.url) {
            unsettled.push(r.url.clone());
        }
    }
    if !unsettled.is_empty() {
        return scaling(format!("rolling out the new settings ({} left): waiting for {} to be back and caught up", left(stale.len()), unsettled.join(", ")));
    }
    let fresh = |url: &str| !stale.iter().any(|&(id, k)| p.url(id, k) == url);
    // And serves a while, for the gates to see it.
    let soak = spec.rollout.soak_seconds as f64;
    let young = view.shards.iter().flat_map(|s| &s.servers).filter(|r| fresh(&r.url)).filter_map(|r| Some((r, r.uptime_s?))).filter(|&(_, u)| u < soak).min_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((r, _)) = young {
        return scaling(format!("rolling out the new settings ({} left): {} serves {soak:.0} s with them before the next restarts", left(stale.len()), r.url));
    }
    let primary = |url: &str| view.shards.iter().any(|s| s.servers.iter().any(|r| r.url == url && r.takes_writes));
    // A replica first, so that each primary can go to a restarted copy.
    let &(id, k) = stale.iter().find(|&&(id, k)| !primary(&p.url(id, k))).unwrap_or(&stale[0]);
    let url = p.url(id, k);
    if let Some(o) = hold.stop(&format!("restarting {url} with the new settings ({} left)", left(stale.len()))) {
        return Ok(Some(o));
    }
    if let Some((i, s)) = view.shards.iter().enumerate().find(|(_, s)| s.servers.iter().any(|r| r.url == url && r.takes_writes)) {
        let mut to: Vec<&Server> = s.servers.iter().filter(|o| o.url != url && o.up && o.in_sync && o.role == "replica").collect();
        to.sort_by_key(|o| !fresh(&o.url));
        if let Some(to) = to.first() {
            admin.post(&format!("/v1/admin/shards/{i}/promote"), json!({"url": to.url})).await?;
            return scaling(format!("shard {i}: handing the primary over from {url} to {}, before {url} restarts with the new settings", to.url));
        }
    }
    p.restart(id, k).await?;
    scaling(format!("restarting {url} with the new settings ({} left)", left(stale.len() - 1)))
}

fn shards_json<P: Platform>(p: &P, view: &View) -> Json {
    Json::Array(
        view.shards
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let servers: Vec<Json> = s.servers.iter().map(|r| json!({"url": r.url, "role": r.role, "up": r.up, "primary": r.takes_writes})).collect();
                json!({"shard": i, "set": set_of_shard(p, s), "slots": s.slots, "servers": servers})
            })
            .collect(),
    )
}

/// `target` with `patch` merged in (RFC 7386: a null removes a field, an
/// object merges, anything else replaces).
pub fn merge_patch(target: &mut Json, patch: &Json) {
    match patch {
        Json::Object(p) => {
            if !target.is_object() {
                *target = json!({});
            }
            let t = target.as_object_mut().expect("an object");
            for (k, v) in p {
                if v.is_null() {
                    t.remove(k);
                } else {
                    merge_patch(t.entry(k.clone()).or_insert(Json::Null), v);
                }
            }
        }
        _ => *target = patch.clone(),
    }
}

/// The spec `now` (a `StrataCluster`'s, as stored) with `patch` merged in,
/// both read as specs.
pub fn patched(now: &Json, patch: &Json) -> Result<(Spec, Spec), String> {
    let old: Spec = serde_json::from_value(if now.is_null() { json!({}) } else { now.clone() }).map_err(|e| format!("the cluster's spec: {e}"))?;
    let mut next = if now.is_null() { json!({}) } else { now.clone() };
    merge_patch(&mut next, patch);
    let new: Spec = serde_json::from_value(next).map_err(|e| format!("spec: {e}"))?;
    Ok((old, new))
}

/// The merge patch that takes `from` to `to` (RFC 7386; a null is a
/// field left out).
pub fn merge_diff(from: &Json, to: &Json) -> Json {
    match (from, to) {
        (Json::Object(a), Json::Object(b)) => {
            let mut p = serde_json::Map::new();
            for (k, v) in a {
                if !v.is_null() && b.get(k).is_none_or(Json::is_null) {
                    p.insert(k.clone(), Json::Null);
                }
            }
            for (k, v) in b {
                let was = a.get(k).unwrap_or(&Json::Null);
                if v.is_null() || was == v {
                    continue;
                }
                p.insert(k.clone(), if was.is_object() && v.is_object() { merge_diff(was, v) } else { merge_diff(&Json::Null, v) });
            }
            Json::Object(p)
        }
        // A new object: its fields but the null ones.
        (_, Json::Object(b)) => Json::Object(b.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), merge_diff(&Json::Null, v))).collect()),
        _ => to.clone(),
    }
}

/// How many versions `status.history` keeps.
pub const VERSIONS: usize = 20;

/// The versions of a cluster (`status.history`, oldest first): `history`
/// and, when `spec` is another version than its last ([`Spec::version`]),
/// `spec` as a new one, changed by `by` if known (one that is an earlier
/// version restores it: a rollback). When `ready` the version runs
/// everywhere, and its first time so is kept.
pub fn versions(history: &Json, spec: &Spec, generation: Option<u64>, by: Option<&str>, now: u64, ready: bool) -> Json {
    let mut h: Vec<Json> = history.as_array().cloned().unwrap_or_default();
    let version = spec.version();
    if h.last().is_none_or(|v| v["hash"] != version.as_str()) {
        let n = h.last().and_then(|v| v["version"].as_u64()).unwrap_or(0) + 1;
        let restores = h.iter().rev().find(|v| v["hash"] == version.as_str()).and_then(|v| v["version"].as_u64());
        let mut s = serde_json::to_value(spec).unwrap_or(Json::Null);
        if let Some(o) = s.as_object_mut() {
            o.remove("rollout");
        }
        h.push(json!({"version": n, "hash": version, "generation": generation, "atMs": now, "by": by, "restores": restores, "spec": s, "readyMs": null}));
    }
    if let Some(last) = h.last_mut().filter(|l| ready && l["readyMs"].is_null()) {
        last["readyMs"] = json!(now);
    }
    let skip = h.len().saturating_sub(VERSIONS);
    Json::Array(h.split_off(skip))
}

/// How many of the operator's messages `status.log` keeps.
pub const LOGGED: usize = 50;

/// `log` (`status.log`, oldest first) and `message`, when it is a new one.
pub fn logged(log: &Json, phase: Phase, message: &str, now: u64) -> Json {
    let mut l: Vec<Json> = log.as_array().cloned().unwrap_or_default();
    if l.last().is_none_or(|e| e["message"] != message) {
        l.push(json!({"atMs": now, "phase": phase, "message": message}));
    }
    let skip = l.len().saturating_sub(LOGGED);
    Json::Array(l.split_off(skip))
}

/// `before` as the resource's annotations leave it: a resume naming the
/// hold (its `at_ms`) clears it and starts the gates' clocks again, and a
/// rollback naming `spec`'s version is held by no gate.
pub fn noted(mut before: Rollout, resume: Option<&str>, rollback: Option<&str>, spec: &Spec) -> Rollout {
    if before.held.as_ref().is_some_and(|h| resume == Some(h.at_ms.to_string().as_str())) {
        before.held = None;
        for g in &mut before.gates {
            g.over_since_ms = None;
        }
    }
    before.ungated = rollback == Some(spec.version().as_str());
    before
}

/// What changing the spec from `old` to `new` changes, and the steps the
/// operator takes for it, in its order, with what to know before (for
/// `POST /v1/admin/kube/plan`). `shards_now` and `copies_now` are what the
/// cluster has; `refused` says why the change cannot be made.
pub fn plan(old: &Spec, new: &Spec, shards_now: usize, copies_now: usize) -> Json {
    let o = serde_json::to_value(old).unwrap_or(Json::Null);
    let n = serde_json::to_value(new).unwrap_or(Json::Null);
    let mut changes = Vec::new();
    diff("", &o, &n, &mut changes);
    let (mut steps, mut warnings, mut refused) = (Vec::<String>::new(), Vec::<String>::new(), None::<String>);
    if let Err(e) = new.check() {
        refused = Some(e);
    }
    let (from, to) = (quantity(&old.storage.size), quantity(&new.storage.size));
    if let (Some(a), Some(b)) = (from, to) {
        if b < a {
            refused = Some(format!("a volume only grows: {} is smaller than {}", new.storage.size, old.storage.size));
        }
    } else if to.is_none() {
        refused = Some(format!("the volume size {:?} is not one Kubernetes reads (10Gi, 500Mi, 1Ti)", new.storage.size));
    }
    if new.coordinators != old.coordinators {
        steps.push(format!("{} coordinators, from {}", new.coordinators, old.coordinators));
    }
    if new.coordinator_config() != old.coordinator_config() {
        steps.push("the coordinators restart with the new settings, one at a time (Kubernetes rolls the Deployment)".into());
    }
    let shards = new.shards;
    if new.replicas > copies_now {
        steps.push(format!("each shard's set grows to {} copies; each new copy joins its shard and is sent a copy of the primary ({} new servers)", new.replicas, (new.replicas - copies_now) * shards_now));
    } else if new.replicas < copies_now {
        steps.push(format!("the highest copy of each shard leaves it (a primary is handed over first) and its volume is deleted, down to {} copies ({} servers fewer)", new.replicas, (copies_now - new.replicas) * shards_now));
    }
    if shards > shards_now {
        let k = shards - shards_now;
        steps.push(format!("{k} new shard{} of {} copies start and join the cluster", if k == 1 { "" } else { "s" }, new.replicas));
        let moved = crate::slots::SLOTS - crate::slots::SLOTS / shards * shards_now;
        steps.push(format!("the slots even out: about {} of the {} move to the new shard{}, a run at a time, while the cluster serves", moved.min(crate::slots::SLOTS), crate::slots::SLOTS, if k == 1 { "" } else { "s" }));
    } else if shards < shards_now {
        let k = shards_now - shards;
        steps.push(format!("the {} newest shard{} drain{} (each one's slots move to the others), leave{} the cluster, and their volumes are deleted", k, if k == 1 { "" } else { "s" }, if k == 1 { "s" } else { "" }, if k == 1 { "s" } else { "" }));
    }
    if new.server_config() != old.server_config() {
        let copies = shards * new.replicas;
        steps.push(format!("every server restarts with the new settings, one at a time ({copies} restarts): each primary is handed to a copy already restarted first, and the next waits until every copy is back and caught up"));
    }
    if let (Some(a), Some(b)) = (from, to) {
        if b > a {
            steps.push(format!("each copy's volume grows from {} to {} (the storage class must allow it)", old.storage.size, new.storage.size));
        }
    }
    if new.storage.storage_class_name != old.storage.storage_class_name {
        warnings.push("a new storage class applies to the volumes made from now on: the copies that run keep theirs".into());
    }
    if new.schema != old.schema || new.demo != old.demo {
        warnings.push("the first table's schema (or the demo) makes new servers only: change the tables that exist through the coordinator (ALTER TABLE)".into());
    }
    if new.replicas == 1 {
        warnings.push("one copy a shard: losing its server loses the shard until it is back, and its writes stop".into());
    } else if new.replicas == 2 && new.write_acks != "primary" {
        warnings.push("two copies a shard: while one is down, its shard stops taking writes (a write waits for a majority, and one of two is not)".into());
    }
    if new.failover_after_ms == 0 && new.replicas > 1 {
        warnings.push("failover by hand: when a primary fails, its shard takes no writes until someone makes a replica its primary".into());
    }
    if new.write_acks == "primary" && new.replicas > 1 {
        warnings.push("a write is answered once the primary has it: a failover may lose the last writes its replicas had not received".into());
    }
    if new.token_secret.is_none() {
        warnings.push("no token: anything that reaches the pods may read, write and change the cluster".into());
    }
    if shards < shards_now || new.replicas < copies_now {
        warnings.push("what goes is deleted with its volumes once its data has moved".into());
    }
    let (a, b) = (&old.rollout, &new.rollout);
    if b.paused != a.paused {
        steps.push(if b.paused { "the change pauses: the operator takes no step toward the spec until it is resumed" } else { "the change goes on from where it paused" }.into());
    } else if b.paused && new.version() != old.version() {
        warnings.push("the change is paused: none of these steps is taken until rollout.paused is false".into());
    }
    if b.gates != a.gates || b.hold_after_seconds != a.hold_after_seconds {
        let set: Vec<String> = b.gates.limits().iter().filter_map(|(k, v)| v.map(|v| format!("{} over {}", gate_title(k), gate_words(k, v)))).collect();
        steps.push(match set.is_empty() {
            true => "no gates: nothing holds a change but a pause".into(),
            false => format!("new gates, no restart: {} for {} s holds a change", set.join(" or "), b.hold_after_seconds),
        });
    }
    if b.soak_seconds != a.soak_seconds {
        steps.push(format!("each copy restarted with new settings serves {} s before the next restarts", b.soak_seconds));
    }
    if steps.is_empty() && changes.is_empty() {
        steps.push("nothing: the spec says what it says now".into());
    }
    json!({"changes": changes, "steps": steps, "warnings": warnings, "refused": refused, "restarts": new.server_config() != old.server_config(), "spec": n})
}

/// The leaves where `a` and `b` differ, as `{field, from, to}` with dotted
/// paths.
fn diff(at: &str, a: &Json, b: &Json, out: &mut Vec<Json>) {
    match (a, b) {
        (Json::Object(x), Json::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                let p = if at.is_empty() { k.clone() } else { format!("{at}.{k}") };
                diff(&p, x.get(k).unwrap_or(&Json::Null), y.get(k).unwrap_or(&Json::Null), out);
            }
        }
        _ if a != b => out.push(json!({"field": at, "from": a, "to": b})),
        _ => {}
    }
}

/// The `StrataCluster` custom resource definition.
pub fn crd() -> Json {
    let int = |min: u64, doc: &str| json!({"type": "integer", "minimum": min, "description": doc});
    let free = |doc: &str| json!({"type": "object", "x-kubernetes-preserve-unknown-fields": true, "description": doc});
    let spec = json!({
        "type": "object",
        "properties": {
            "image": {"type": "string", "description": "The strata-server image of every server (default strata-server:dev)."},
            "imagePullPolicy": {"type": "string"},
            "shards": int(1, "Shards, each holding a part of the 4,096 slots (default 1)."),
            "replicas": int(1, "Copies of each shard: a primary and its replicas (default 3)."),
            "coordinators": int(1, "Coordinators behind the cluster's Service (default 1)."),
            "storage": {"type": "object", "properties": {
                "size": {"type": "string", "description": "Each copy's volume (default 10Gi)."},
                "storageClassName": {"type": "string"},
            }},
            "schema": free("The first table's schema, as strata-server --schema takes it."),
            "demo": int(0, "Instead of a schema: this many demo products, split over the shards the cluster starts with."),
            "failoverAfterMs": int(0, "--failover-after-ms of every server (default 3000; 0: failover by hand)."),
            "writeAcks": {"type": "string", "enum": ["all", "majority", "primary"], "description": "--write-acks of every primary (default all)."},
            "tokenSecret": {"type": "string", "description": "A Secret whose key token every server and coordinator requires; with clusterTokenSecret, the coordinators only (the admin token)."},
            "clusterTokenSecret": {"type": "string", "description": "A Secret whose key token is the cluster's token (--cluster-token), which the servers and coordinators send each other and clients never hold. Needs tokenSecret."},
            "resources": free("Resources of each server's container."),
            "coordinatorResources": free("Resources of each coordinator's container."),
            "args": {"type": "array", "items": {"type": "string"}, "description": "More arguments for every server."},
            "coordinatorArgs": {"type": "array", "items": {"type": "string"}, "description": "More arguments for every coordinator."},
            "rollout": {"type": "object", "description": "How changes are made: paused, or held by gates on the cluster's live numbers.", "properties": {
                "paused": {"type": "boolean", "description": "While true the operator takes no step toward the spec (a copy that lost its data still joins again)."},
                "gates": {"type": "object", "description": "Limits on live numbers: one over its limit for holdAfterSeconds holds the change until it is resumed.", "properties": {
                    "p99Ms": {"type": "number", "minimum": 0, "description": "The search p99 the coordinator's clients see, in ms."},
                    "errorRatio": {"type": "number", "minimum": 0, "maximum": 1, "description": "The share of searches that fail."},
                    "replicaBehind": {"type": "number", "minimum": 0, "description": "The transactions a replica is behind its primary."},
                    "diskUsedRatio": {"type": "number", "minimum": 0, "maximum": 1, "description": "The fullest volume's used share."},
                }},
                "holdAfterSeconds": int(0, "How long a gate is over its limit before it holds the change (default 30)."),
                "soakSeconds": int(0, "How long a copy restarted with new settings serves before the next restarts (default 0)."),
            }},
        },
    });
    let status = free("What the operator found and does.");
    json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "CustomResourceDefinition",
        "metadata": {"name": format!("{PLURAL}.{GROUP}")},
        "spec": {
            "group": GROUP,
            "names": {"kind": KIND, "listKind": format!("{KIND}List"), "plural": PLURAL, "singular": "stratacluster", "shortNames": ["strata"]},
            "scope": "Namespaced",
            "versions": [{
                "name": VERSION,
                "served": true,
                "storage": true,
                "subresources": {"status": {}},
                "schema": {"openAPIV3Schema": {"type": "object", "properties": {"spec": spec, "status": status}}},
                "additionalPrinterColumns": [
                    {"name": "Shards", "type": "integer", "jsonPath": ".spec.shards"},
                    {"name": "Copies", "type": "integer", "jsonPath": ".spec.replicas"},
                    {"name": "Phase", "type": "string", "jsonPath": ".status.phase"},
                    {"name": "Message", "type": "string", "jsonPath": ".status.message", "priority": 1},
                    {"name": "Age", "type": "date", "jsonPath": ".metadata.creationTimestamp"},
                ],
            }],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_takes_defaults_and_is_checked() {
        let s: Spec = serde_json::from_value(json!({"shards": 2, "demo": 1000})).unwrap();
        assert_eq!((s.shards, s.replicas, s.coordinators, s.failover_after_ms, s.write_acks.as_str()), (2, 3, 1, 3000, "all"));
        s.check().unwrap();
        let bad = |j: Json| serde_json::from_value::<Spec>(j).unwrap().check().unwrap_err();
        assert!(bad(json!({"shards": 0, "demo": 1})).contains("1 or more"));
        assert!(bad(json!({})).contains("give the first table's schema"));
        assert!(bad(json!({"writeAcks": "most", "demo": 1})).contains("writeAcks"));
        assert!(bad(json!({"schema": {"table": "t"}})).contains("schema:"));
        assert!(bad(json!({"demo": 1, "clusterTokenSecret": "c"})).contains("needs tokenSecret"));
        assert!(bad(json!({"demo": 1, "clusterTokenSecret": "t", "tokenSecret": "t"})).contains("the same Secret"));
        serde_json::from_value::<Spec>(json!({"demo": 1, "clusterTokenSecret": "c", "tokenSecret": "t"})).unwrap().check().unwrap();
        let s: Spec = serde_json::from_value(json!({"demo": 30})).unwrap();
        assert_eq!(strata::schema::Schema::from_json(&s.schema_json()).unwrap(), crate::demo::schema(30));
    }

    #[test]
    fn a_plan_says_what_a_change_does_and_refuses_what_cannot_be() {
        let old: Spec = serde_json::from_value(json!({"shards": 2, "demo": 100, "tokenSecret": "t"})).unwrap();
        let (_, new) = patched(&serde_json::to_value(&old).unwrap(), &json!({"shards": 4, "args": ["--slow-ms", "50"], "storage": {"size": "20Gi"}, "coordinatorArgs": ["--hedge-ms", "40"]})).unwrap();
        let p = plan(&old, &new, 2, 3);
        assert!(p["refused"].is_null(), "{p}");
        let fields: Vec<&str> = p["changes"].as_array().unwrap().iter().map(|c| c["field"].as_str().unwrap()).collect();
        assert_eq!(fields, ["args", "coordinatorArgs", "shards", "storage.size"]);
        let steps = p["steps"].to_string();
        for want in ["coordinators restart", "2 new shards", "about 2048 of the 4096 move", "every server restarts", "(12 restarts)", "from 10Gi to 20Gi"] {
            assert!(steps.contains(want), "{want}: {steps}");
        }
        assert_eq!(p["restarts"], true);
        assert!(p["warnings"].as_array().unwrap().is_empty(), "{p}");
        // What cannot be: a smaller volume, a size Kubernetes does not read,
        // a spec that does not check.
        let refused = |patch: Json| plan(&old, &patched(&serde_json::to_value(&old).unwrap(), &patch).unwrap().1, 2, 3)["refused"].as_str().unwrap_or_default().to_string();
        assert!(refused(json!({"storage": {"size": "5Gi"}})).contains("only grows"));
        assert!(refused(json!({"storage": {"size": "lots"}})).contains("not one Kubernetes reads"));
        assert!(refused(json!({"shards": 0})).contains("1 or more"));
        // A null takes a field back to its default; fewer copies warn.
        let (_, new) = patched(&serde_json::to_value(&old).unwrap(), &json!({"tokenSecret": null, "replicas": 2})).unwrap();
        let p = plan(&old, &new, 2, 3);
        assert_eq!(new.token_secret, None);
        let warnings = p["warnings"].to_string();
        assert!(warnings.contains("no token") && warnings.contains("two copies a shard") && warnings.contains("deleted with its volumes"), "{warnings}");
        // Pausing and gates restart nothing.
        let (_, gated) = patched(&serde_json::to_value(&old).unwrap(), &json!({"rollout": {"paused": true, "gates": {"p99Ms": 200}}})).unwrap();
        let p = plan(&old, &gated, 2, 3);
        let steps = p["steps"].to_string();
        assert!(steps.contains("the change pauses") && steps.contains("new gates, no restart: search p99 over 200 ms for 30 s holds a change"), "{steps}");
        assert_eq!(p["restarts"], false);
        assert_eq!((quantity("1.5Gi"), quantity("10 Gi"), quantity("-1Gi")), (Some(3 << 29), None, None));
        assert_eq!((quantity("10Gi"), quantity("500M"), quantity("2Ti")), (Some(10 << 30), Some(500_000_000), Some(2 << 40)));
    }

    #[test]
    fn a_gate_over_its_limit_long_enough_holds_the_change_until_resumed() {
        let mut spec: Spec = serde_json::from_value(json!({"demo": 1, "rollout": {"gates": {"p99Ms": 200, "diskUsedRatio": 0.9}}})).unwrap();
        assert_eq!(spec.rollout.hold_after_seconds, 30);
        let hot = Numbers { p99_ms: Some(312.0), disk_used_ratio: Some(0.5), ..Numbers::default() };
        // Over its limit, a gate's clock starts; at 30 s it holds.
        let (r, trip) = gates(&spec, &hot, &Rollout::default(), 1_000);
        assert!(trip.is_none());
        assert_eq!(r.gates.iter().map(|g| (g.name.as_str(), g.over_since_ms)).collect::<Vec<_>>(), [("p99Ms", Some(1_000)), ("diskUsedRatio", None)]);
        let (r, trip) = gates(&spec, &hot, &r, 30_000);
        assert!(trip.is_none());
        let (r, trip) = gates(&spec, &hot, &r, 31_000);
        let h = trip.unwrap();
        assert_eq!((h.gate.as_str(), h.text.as_str()), ("p99Ms", "search p99 was 312 ms, over its 200 ms limit, for 30 s"));
        // Back under its limit, the clock stops; but a hold stays until it
        // is resumed, the version changes or its gate is changed.
        let cool = Numbers { p99_ms: Some(150.0), ..hot.clone() };
        let (r2, trip) = gates(&spec, &cool, &r, 32_000);
        assert!(trip.is_none() && r2.gates[0].over_since_ms.is_none());
        let held = Rollout { held: Some(h.clone()), ..r.clone() };
        let (r3, _) = gates(&spec, &cool, &held, 40_000);
        assert_eq!(r3.held.as_ref(), Some(&h));
        let resumed = noted(held.clone(), Some("31000"), None, &spec);
        assert!(resumed.held.is_none() && resumed.gates.iter().all(|g| g.over_since_ms.is_none()), "{resumed:?}");
        assert_eq!(noted(held.clone(), Some("123"), None, &spec).held.as_ref(), Some(&h), "a resume of another hold");
        spec.rollout.gates.p99_ms = Some(400.0);
        assert!(gates(&spec, &hot, &held, 40_000).0.held.is_none(), "its gate changed");
        spec.rollout.gates.p99_ms = Some(200.0);
        let other = Spec { image: "strata-server:0.2.0".into(), ..spec.clone() };
        assert!(gates(&other, &cool, &held, 40_000).0.held.is_none(), "another version");
        // A pause or a gate is no new version; a rollback is held by no gate.
        assert_eq!(Spec { rollout: RolloutSpec { paused: true, soak_seconds: 9, ..RolloutSpec::default() }, ..spec.clone() }.version(), spec.version());
        let back = noted(r.clone(), None, Some(&spec.version()), &spec);
        assert!(back.ungated && gates(&spec, &hot, &back, 99_000).1.is_none());
        assert!(!noted(r.clone(), None, Some("another"), &spec).ungated);
        // What the gates read, from a coordinator's /v1/cluster: its own
        // p99 and failures, the worst of the servers that answer.
        let c = json!({"coordinator": {"load": {"p99_ms": 12.5, "error_ratio": 0.0}}, "shards": [{"replicas": [
            {"status": "up", "detail": {"load": {"p99_ms": 40.0, "disk_used_ratio": 0.4, "replica_behind_max": 3.0}}},
            {"status": "down", "detail": {"load": {"disk_used_ratio": 0.99}}},
            {"status": "up", "detail": {"load": {"disk_used_ratio": 0.6}}}]}]});
        assert_eq!(Numbers::from(&c), Numbers { p99_ms: Some(12.5), error_ratio: Some(0.0), replica_behind: Some(3.0), disk_used_ratio: Some(0.6) });
        assert_eq!(Numbers::from(&json!({"shards": [{"replicas": [{"status": "up", "detail": {"load": {"p99_ms": 40.0}}}]}]})).p99_ms, Some(40.0));
        assert_eq!([gate_words("diskUsedRatio", 0.0001), gate_words("errorRatio", 0.025), gate_words("diskUsedRatio", 0.643)], ["0.01%", "2.5%", "64%"]);
        let bad = |j: Json| serde_json::from_value::<Spec>(j).unwrap().check().unwrap_err();
        assert!(bad(json!({"demo": 1, "rollout": {"gates": {"errorRatio": 2}}})).contains("a share"));
        assert!(bad(json!({"demo": 1, "rollout": {"gates": {"p99Ms": -1}}})).contains("0 or more"));
    }

    #[test]
    fn each_version_is_kept_and_a_rollback_is_the_patch_back_to_one() {
        let a: Spec = serde_json::from_value(json!({"demo": 1, "shards": 2, "resources": {"limits": {"cpu": "2"}}})).unwrap();
        let h = versions(&Json::Null, &a, Some(1), None, 100, false);
        assert_eq!((h.as_array().unwrap().len(), &h[0]["version"], &h[0]["spec"]["rollout"]), (1, &json!(1), &Json::Null));
        // The same version, ready: when it first was is kept. A pause is
        // another generation, not another version.
        let h = versions(&h, &a, Some(1), None, 200, true);
        let paused = Spec { rollout: RolloutSpec { paused: true, ..RolloutSpec::default() }, ..a.clone() };
        let h = versions(&h, &paused, Some(2), None, 300, true);
        assert_eq!((h.as_array().unwrap().len(), &h[0]["readyMs"]), (1, &json!(200)));
        let b = Spec { shards: 4, image: "strata-server:0.2.0".into(), resources: Some(json!({"limits": {"cpu": "2", "memory": "4Gi"}})), token_secret: Some("t".into()), ..a.clone() };
        let h = versions(&h, &b, Some(3), Some("roberto"), 400, false);
        assert_eq!((&h[1]["version"], &h[1]["by"], &h[1]["restores"], &h[1]["readyMs"]), (&json!(2), &json!("roberto"), &Json::Null, &Json::Null));
        // Back to version 1: the merge patch that takes the spec there, its
        // rollout kept; the version it makes restores the first.
        let now = json!({"spec": serde_json::to_value(&paused).unwrap(), "status": {"history": h}});
        let now = json!({"spec": {"rollout": now["spec"]["rollout"], "shards": 4, "demo": 1, "image": "strata-server:0.2.0", "tokenSecret": "t", "resources": {"limits": {"cpu": "2", "memory": "4Gi"}}}, "status": now["status"]});
        let patch = kube::Link::rollback(&now, 1).unwrap();
        assert_eq!(patch, json!({"shards": 2, "image": "strata-server:dev", "tokenSecret": null, "resources": {"limits": {"memory": null}}, "replicas": 3, "coordinators": 1, "storage": {"size": "10Gi"}, "failoverAfterMs": 3000, "writeAcks": "all", "args": [], "coordinatorArgs": []}));
        let (_, back) = patched(&now["spec"], &patch).unwrap();
        assert_eq!(back, paused, "the spec of version 1, still paused");
        let h = versions(&h, &back, Some(4), None, 500, false);
        assert_eq!((&h[2]["version"], &h[2]["restores"]), (&json!(3), &json!(1)));
        assert!(kube::Link::rollback(&now, 9).unwrap_err().contains("no version 9 in the cluster's history (it keeps 1, 2)"));
        // The last VERSIONS are kept.
        let mut h = Json::Null;
        for n in 0..30 {
            h = versions(&h, &Spec { shards: n + 1, ..a.clone() }, None, None, n as u64, false);
        }
        assert_eq!((h.as_array().unwrap().len(), &h[0]["version"]), (VERSIONS, &json!(11)));
        // The operator's messages, each new one once.
        let l = logged(&Json::Null, Phase::Scaling, "a", 1);
        let l = logged(&logged(&l, Phase::Scaling, "a", 2), Phase::Ready, "b", 3);
        assert_eq!(l, json!([{"atMs": 1, "phase": "Scaling", "message": "a"}, {"atMs": 3, "phase": "Ready", "message": "b"}]));
        assert_eq!(merge_diff(&json!({"a": 1, "b": {"c": 2}}), &json!({"a": 1, "b": {"c": 2}})), json!({}));
    }

    #[test]
    fn the_crd_file_is_the_one_the_operator_installs() {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/deploy/k8s/operator/crd.json");
        let want = serde_json::to_string_pretty(&crd()).unwrap() + "\n";
        if std::env::var_os("STRATA_WRITE_CRD").is_some() {
            std::fs::write(file, &want).unwrap();
        }
        assert_eq!(std::fs::read_to_string(file).unwrap_or_default(), want, "run with STRATA_WRITE_CRD=1 to write {file}");
    }
}
