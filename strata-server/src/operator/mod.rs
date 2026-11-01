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

    /// What is wrong with it, if anything.
    pub fn check(&self) -> Result<(), String> {
        if self.shards == 0 || self.replicas == 0 || self.coordinators == 0 {
            return Err("shards, replicas and coordinators are 1 or more".into());
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
    /// Grows every copy's volume smaller than `size` to it; how many it
    /// asked to grow.
    async fn grow_volumes(&self, size: &str) -> Result<usize, String>;
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
    pub can_change: Result<(), String>,
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
                            Server { in_sync: takes_writes || in_sync(&url), up: r["status"] != "down", role: r["role"].as_str().unwrap_or_default().to_string(), takes_writes, url }
                        })
                        .collect(),
                    slots: s["slots_held"].as_u64().unwrap_or(0) as usize,
                }
            })
            .collect();
        let moving = !moves["moving"].is_null() || moves["job"]["state"] == "running";
        let can_change = match cluster["changes"]["ok"].as_bool() {
            Some(false) => Err(cluster["changes"]["why"].as_str().unwrap_or("the cluster map cannot change").to_string()),
            _ => Ok(()),
        };
        Ok(View { shards, moving, can_change })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Phase {
    /// The cluster is being made.
    Creating,
    /// It changes toward the spec.
    Scaling,
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
}

impl Outcome {
    fn new(phase: Phase, message: impl Into<String>) -> Outcome {
        Outcome { phase, message: message.into(), shards: Json::Null }
    }
}

/// Takes the cluster one step toward `spec`: one change to the platform
/// or one admin call, or none when it is there (or waits for something).
/// Called again and again; each call starts from what it finds.
pub async fn reconcile<P: Platform>(spec: &Spec, p: &P, admin: &Admin) -> Outcome {
    match step(spec, p, admin).await {
        Ok(o) => o,
        Err(e) => Outcome::new(Phase::Scaling, e),
    }
}

async fn step<P: Platform>(spec: &Spec, p: &P, admin: &Admin) -> Result<Outcome, String> {
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
    let mut out = act(spec, p, admin, &sets, &view, &listed, coordinators).await?;
    out.shards = shards_json(p, &view);
    Ok(out)
}

/// The set of each shard of `view`, by its first server of this cluster.
fn set_of_shard<P: Platform>(p: &P, s: &Shard) -> Option<u32> {
    s.servers.iter().find_map(|r| p.set_of(&r.url)).map(|(id, _)| id)
}

async fn act<P: Platform>(spec: &Spec, p: &P, admin: &Admin, sets: &[Set], view: &View, listed: &[Vec<String>], coordinators: usize) -> Result<Outcome, String> {
    let scaling = |m: String| Ok(Outcome::new(Phase::Scaling, m));
    let shard_of: HashMap<u32, usize> = view.shards.iter().enumerate().filter_map(|(i, s)| set_of_shard(p, s).map(|id| (id, i))).collect();
    let in_map: Vec<&str> = view.shards.iter().flat_map(|s| s.servers.iter().map(|r| r.url.as_str())).collect();
    // A copy of a set that serves a shard, which joins it (again) if it is
    // not in it.
    let kept = |id: u32, k: usize| sets.iter().any(|s| s.id == id && k < s.replicas.min(spec.replicas)) && shard_of.contains_key(&id);
    // The coordinators' list must name servers of the cluster only: one
    // that starts again on a list naming a server the cluster left out for
    // good is refused.
    let gone = |u: &String| !in_map.contains(&u.as_str()) && !p.set_of(u).is_some_and(|(id, k)| kept(id, k));
    if listed.iter().flatten().any(gone) || coordinators != spec.coordinators {
        let now: Vec<Vec<String>> = view.shards.iter().map(|s| s.servers.iter().map(|r| r.url.clone()).collect()).collect();
        p.set_coordinator(&now, spec.coordinators).await?;
        return scaling(format!("updating the coordinators ({}) to the cluster's servers", spec.coordinators));
    }
    if p.coordinator_config().await?.is_some_and(|c| c != spec.coordinator_config()) {
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
            let id = sets.iter().map(|s| s.id + 1).max().unwrap_or(0);
            p.create_set(id, spec.replicas, None).await?;
            return scaling(format!("a new shard: making its {} copies", spec.replicas));
        };
        if !set.ready.contains(&0) {
            return scaling(format!("a new shard: waiting for its first copy ({} of {} ready)", set.ready.len(), set.replicas));
        }
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
            return scaling(format!("moving slots off shard {i} ({} left)", s.slots));
        }
        if s.slots > 0 {
            admin.post(&format!("/v1/admin/shards/{i}/drain"), json!({})).await?;
            return scaling(format!("draining shard {i}: its {} slots move to the others", s.slots));
        }
        admin.delete(&format!("/v1/admin/shards/{i}")).await?;
        return scaling(format!("shard {i} leaves the cluster"));
    }
    if let Some(set) = pending.first() {
        p.delete_set(set.id).await?;
        return scaling(format!("deleting the copies of a shard that left (set {})", set.id));
    }
    // Slots even over the shards.
    if view.moving {
        return scaling("moving slots between shards".into());
    }
    let (lo, hi) = (view.shards.iter().map(|s| s.slots).min().unwrap_or(0), view.shards.iter().map(|s| s.slots).max().unwrap_or(0));
    if hi - lo > 1 {
        admin.post("/v1/admin/rebalance", json!({})).await?;
        return scaling(format!("evening out the slots over {n} shards ({lo} to {hi} each)"));
    }
    if let Some(o) = roll(spec, p, admin, sets, view).await? {
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
/// and the copies running older settings restart one at a time. The next
/// restarts only once every copy is back and in sync; a primary is first
/// handed to a copy of its shard in sync, one that runs the new settings
/// if there is one. A shard of one copy cannot be handed over: its copy
/// restarts, and its writes wait for it.
async fn roll<P: Platform>(spec: &Spec, p: &P, admin: &Admin, sets: &[Set], view: &View) -> Result<Option<Outcome>, String> {
    let scaling = |m: String| Ok(Some(Outcome::new(Phase::Scaling, m)));
    for set in sets {
        if p.configure(set.id, spec).await? {
            return scaling(format!("set {}: new copies start with the new settings", set.id));
        }
    }
    let grown = p.grow_volumes(&spec.storage.size).await?;
    if grown > 0 {
        return scaling(format!("growing {grown} volume{} to {}", if grown == 1 { "" } else { "s" }, spec.storage.size));
    }
    let stale = p.stale(spec).await?;
    if stale.is_empty() {
        return Ok(None);
    }
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
        return scaling(format!("rolling out the new settings ({} copies left): waiting for {} to be back and caught up", stale.len(), unsettled.join(", ")));
    }
    let fresh = |url: &str| !stale.iter().any(|&(id, k)| p.url(id, k) == url);
    let primary = |url: &str| view.shards.iter().any(|s| s.servers.iter().any(|r| r.url == url && r.takes_writes));
    // A replica first, so that each primary can go to a restarted copy.
    let &(id, k) = stale.iter().find(|&&(id, k)| !primary(&p.url(id, k))).unwrap_or(&stale[0]);
    let url = p.url(id, k);
    if let Some((i, s)) = view.shards.iter().enumerate().find(|(_, s)| s.servers.iter().any(|r| r.url == url && r.takes_writes)) {
        let mut to: Vec<&Server> = s.servers.iter().filter(|o| o.url != url && o.up && o.in_sync && o.role == "replica").collect();
        to.sort_by_key(|o| !fresh(&o.url));
        if let Some(to) = to.first() {
            admin.post(&format!("/v1/admin/shards/{i}/promote"), json!({"url": to.url})).await?;
            return scaling(format!("shard {i}: handing the primary over from {url} to {}, before {url} restarts with the new settings", to.url));
        }
    }
    p.restart(id, k).await?;
    scaling(format!("restarting {url} with the new settings ({} {} left)", stale.len() - 1, if stale.len() == 2 { "copy" } else { "copies" }))
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
        assert_eq!((quantity("1.5Gi"), quantity("10 Gi"), quantity("-1Gi")), (Some(3 << 29), None, None));
        assert_eq!((quantity("10Gi"), quantity("500M"), quantity("2Ti")), (Some(10 << 30), Some(500_000_000), Some(2 << 40)));
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
