//! A Kubernetes operator for ChloeDB clusters (`chloedb-server operator`).
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

pub mod apps;
pub mod autoscale;
pub mod chloectx;
pub mod credentials;
pub mod crawler;
pub mod follow;
pub mod kube;
pub mod lease;
pub mod netops;
pub mod sheets;
pub mod upgrade;

pub use apps::AppsSpec;
pub use chloectx::ChloectxSpec;
pub use crawler::CrawlerSpec;
pub use follow::FollowSpec;
pub use netops::NetopsSpec;
pub use sheets::SheetsSpec;

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
    /// The chloedb-server image of every server.
    pub image: String,
    /// What the cluster is to its people (`production`, `staging`): the
    /// command center shows it on every page (`STRATA_ENVIRONMENT`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
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
    /// Environment variables of every server and coordinator taken from
    /// Secrets: the keys of models behind an API, such as
    /// `ANTHROPIC_API_KEY` for the agents, LLM fields and reranking, or
    /// `OPENAI_API_KEY` for hosted embeddings. Changing it restarts the
    /// copies one at a time, as other settings do.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secret_env: Vec<SecretEnv>,
    /// Resources of each server's and each coordinator's container.
    pub resources: Option<Json>,
    pub coordinator_resources: Option<Json>,
    /// More arguments for every server.
    pub args: Vec<String>,
    /// More arguments for every coordinator (`--hedge-ms 50`,
    /// `--partial`, ...).
    pub coordinator_args: Vec<String>,
    /// A volume for each coordinator, which keeps its history across
    /// restarts: its log store (events, alerts, query statistics, table
    /// growth: `--log-store`), its audit log (`--audit-log`) and its alert
    /// rules and their states (`--alerts`). With it the coordinators run as
    /// a StatefulSet, each with a volume of its own; without it (the
    /// default, as before) as a Deployment that keeps all of that in memory
    /// only, so that every rollout forgets it (LIV-2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_storage: Option<CoordinatorStorage>,
    /// Whether a shard's copies must run on different nodes: required of
    /// the scheduler rather than preferred. A copy then waits, unscheduled,
    /// for a node that runs none of its shard's other copies, and a
    /// rollout's extra copy ([`RolloutSpec::extra_copies`]) for one more
    /// node than the shard has copies. Turning it on or off restarts the
    /// copies one at a time, as other settings do, each placed by the new
    /// rule. Off (the default), the spread is preferred, so that a cluster
    /// of fewer nodes than copies still runs them all, and the cluster's
    /// objects, settings and version are as before.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub require_copies_apart: bool,
    /// How changes are made: paused, or held by gates on the cluster's
    /// live numbers.
    pub rollout: RolloutSpec,
    /// Rules that change the copies, shards or volumes from how busy and
    /// how full the servers are ([`autoscale`]). Like `rollout`, not part
    /// of a version: the changes they make are.
    pub autoscale: autoscale::Autoscale,
    /// Following an image tag for new versions of Strata ([`upgrade`]):
    /// a new image the tag points to is suggested, or rolled out. Not part
    /// of a version either.
    pub upgrade: upgrade::Upgrade,
    /// Following another cluster ([`follow`]): a follow process of its
    /// own, while the cluster refuses its clients' writes. Not part of a
    /// version: it changes no server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow: Option<FollowSpec>,
    /// A crawler beside the cluster ([`crawler`]): `chloedb-crawl serve`
    /// in a Deployment of one, which the coordinators list as an app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crawler: Option<CrawlerSpec>,
    /// Spreadsheets beside the cluster ([`sheets`]): `chloedb-sheets
    /// serve` in a Deployment of one, which the coordinators list as an app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheets: Option<SheetsSpec>,
    /// ChloeCtx beside the cluster ([`chloectx`]): `chloectx app` in a
    /// Deployment of one, which the coordinators list as an app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chloectx: Option<ChloectxSpec>,
    /// The app engine beside the cluster ([`apps`]): chloedb-apps in a
    /// Deployment of one, which the coordinators list as an app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apps: Option<AppsSpec>,
    /// Network operations beside the cluster ([`netops`]): `chloedb-netops
    /// serve` in a Deployment of one, which the coordinators list as an app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub netops: Option<NetopsSpec>,
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
    /// What a version that fails its checks gets: `rollback` restores
    /// the last version that ran everywhere (one that is itself a
    /// rollback is held instead), `hold` holds it until it is resumed or
    /// the spec changes.
    pub on_failure: OnFailure,
    /// How long one step of a rollout may take before its version fails:
    /// the new image starting on every node (the pre-flight), the
    /// coordinators restarting, a copy restarting and catching up, and
    /// the verification.
    pub progress_deadline_seconds: u64,
    /// How long, once every server runs a version that restarted them,
    /// the cluster must stay healthy (every copy back and in sync, no
    /// server restarting, the gates under their limits and a query
    /// answered through the coordinators) before the version counts as
    /// rolled out; 0: not verified.
    pub verify_seconds: u64,
    /// A shard of fewer than three copies takes more, started with the new
    /// settings, before any of its copies restarts, and lets them go once
    /// every one of its own runs them: of two copies, the one left while
    /// the other restarts is not a majority, so the shard would serve no
    /// reads and hold no writes meanwhile. Each extra copy is sent a copy
    /// of the shard's data, once a rollout.
    pub extra_copies: bool,
}

impl Default for RolloutSpec {
    fn default() -> RolloutSpec {
        RolloutSpec {
            paused: false,
            gates: Gates::default(),
            hold_after_seconds: 30,
            soak_seconds: 0,
            on_failure: OnFailure::Rollback,
            progress_deadline_seconds: 600,
            verify_seconds: 60,
            extra_copies: true,
        }
    }
}

/// What a version that fails its checks gets (`spec.rollout.onFailure`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnFailure {
    /// The operator restores the last version that ran everywhere.
    #[default]
    Rollback,
    /// The change is held until a person resumes it or changes the spec.
    Hold,
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
    /// How busy the busiest shard's copies were over the last minute: the
    /// mean, over its copies that answer, of the share of its cores each
    /// used (autoscaling reads it, no gate does).
    pub busy: Option<f64>,
}

impl Numbers {
    pub fn from(cluster: &Json) -> Numbers {
        let up = |s: &Json| -> Vec<Json> { s["replicas"].as_array().into_iter().flatten().filter(|r| r["status"] != "down").cloned().collect() };
        let shards: Vec<Vec<Json>> = cluster["shards"].as_array().into_iter().flatten().map(up).collect();
        let servers: Vec<&Json> = shards.iter().flatten().collect();
        let worst = |k: &str| servers.iter().filter_map(|r| r["detail"]["load"][k].as_f64()).reduce(f64::max);
        let coordinator = &cluster["coordinator"]["load"];
        // A copy's minute, or (from an older server) its last point.
        let busy_of = |r: &Json| {
            let d = &r["detail"];
            d["minute"]["busy"].as_f64().or_else(|| Some(d["load"]["cpu"].as_f64()? / d["system"]["cores"].as_f64().filter(|c| *c > 0.0)?))
        };
        let busy = shards
            .iter()
            .filter_map(|copies| {
                let b: Vec<f64> = copies.iter().filter_map(busy_of).collect();
                (!b.is_empty()).then(|| b.iter().sum::<f64>() / b.len() as f64)
            })
            .reduce(f64::max);
        Numbers {
            p99_ms: coordinator["p99_ms"].as_f64().or_else(|| worst("p99_ms")),
            error_ratio: coordinator["error_ratio"].as_f64().or_else(|| worst("error_ratio")),
            replica_behind: worst("replica_behind_max"),
            disk_used_ratio: worst("disk_used_ratio"),
            busy,
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

/// Each coordinator's volume (`spec.coordinatorStorage`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CoordinatorStorage {
    /// Each coordinator's volume: its log store takes up to
    /// `--log-store-mb` (1 GiB by default) of it.
    pub size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
}

impl Default for CoordinatorStorage {
    fn default() -> CoordinatorStorage {
        CoordinatorStorage { size: "2Gi".into(), storage_class_name: None }
    }
}

/// Where a coordinator with a volume keeps its history, and the flag
/// that says so: given unless `coordinatorArgs` gives the flag itself.
pub const COORDINATOR_FILES: [(&str, &str); 3] = [("--log-store", "/data/logs"), ("--audit-log", "/data/audit.jsonl"), ("--alerts", "/data/alerts.json")];

/// An environment variable taken from a Secret (`spec.secretEnv`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretEnv {
    /// The variable, such as `ANTHROPIC_API_KEY`.
    pub name: String,
    /// The Secret, in the cluster's namespace.
    pub secret: String,
    /// The Secret's key that holds the value.
    #[serde(default = "SecretEnv::default_key")]
    pub key: String,
}

impl SecretEnv {
    fn default_key() -> String {
        "key".into()
    }
}

impl Default for Spec {
    fn default() -> Spec {
        Spec {
            image: "chloedb-server:dev".into(),
            environment: None,
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
            secret_env: Vec::new(),
            resources: None,
            coordinator_resources: None,
            args: Vec::new(),
            coordinator_args: Vec::new(),
            coordinator_storage: None,
            require_copies_apart: false,
            rollout: RolloutSpec::default(),
            autoscale: autoscale::Autoscale::default(),
            upgrade: upgrade::Upgrade::default(),
            follow: None,
            crawler: None,
            sheets: None,
            chloectx: None,
            apps: None,
            netops: None,
        }
    }
}

impl Spec {
    /// The settings every server starts with, as a short hash: a copy
    /// started with another hash runs older settings, and is restarted.
    pub fn server_config(&self) -> String {
        let mut j = self.with_secret_env(json!({
            "image": self.image, "imagePullPolicy": self.image_pull_policy, "args": self.args, "resources": self.resources,
            "failoverAfterMs": self.failover_after_ms, "writeAcks": self.write_acks,
            "tokenSecret": self.token_secret, "clusterTokenSecret": self.cluster_token_secret,
        }));
        // Only when given, so that a cluster without it keeps its hash (and
        // its copies) across the operator's upgrade. Given, the copies
        // restart one at a time, each placed by the rule it now requires.
        if self.require_copies_apart {
            j["copiesApart"] = json!("required");
        }
        short_hash(&j)
    }

    /// The settings every coordinator starts with, as a short hash.
    pub fn coordinator_config(&self) -> String {
        let mut j = self.with_secret_env(json!({
            "image": self.image, "imagePullPolicy": self.image_pull_policy, "resources": self.coordinator_resources,
            "args": self.coordinator_args, "tokenSecret": self.token_secret, "clusterTokenSecret": self.cluster_token_secret,
        }));
        // Only when there is one, so that a cluster without one keeps its
        // hash (and its coordinators) across the operator's upgrade.
        if let Some(st) = &self.coordinator_storage {
            j["storage"] = json!(st);
        }
        let apps = self.apps();
        if !apps.is_empty() {
            j["apps"] = json!(apps);
            // Each app's own token, a file of a folder mounted from its
            // credentials ([`credentials`]).
            j["appTokens"] = json!(credentials::TOKEN_DIR);
        }
        short_hash(&j)
    }

    /// The apps beside the cluster that the coordinators list, by name.
    pub fn apps(&self) -> Vec<&'static str> {
        [(self.crawler.is_some(), crawler::APP), (self.sheets.is_some(), sheets::APP), (self.chloectx.is_some(), chloectx::APP), (self.apps.is_some(), apps::APP), (self.netops.is_some(), netops::APP)].into_iter().filter(|a| a.0).map(|a| a.1).collect()
    }

    /// `settings` with `secretEnv` when there is one: a cluster without it
    /// keeps the hash it had before there was such a setting, so a new
    /// operator restarts none of its copies.
    fn with_secret_env(&self, mut settings: Json) -> Json {
        if !self.secret_env.is_empty() {
            settings["secretEnv"] = json!(self.secret_env);
        }
        settings
    }

    /// The spec but for `rollout`, `autoscale` and `upgrade`, as a short
    /// hash: another one is another version of the cluster.
    pub fn version(&self) -> String {
        let s = Spec { rollout: RolloutSpec::default(), ..self.clone() };
        let mut j = serde_json::to_value(&s).unwrap_or(Json::Null);
        if let Some(o) = j.as_object_mut() {
            for k in UNVERSIONED {
                o.remove(*k);
            }
        }
        short_hash(&j)
    }

    /// The name of the first table, which the verification queries.
    pub fn first_table(&self) -> String {
        match &self.schema {
            Some(s) => chloedb::schema::Schema::from_json_value(s).map(|s| s.table).unwrap_or_default(),
            None => crate::demo::schema(self.demo.unwrap_or(0)).table,
        }
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
        if let Some(st) = &self.coordinator_storage {
            if quantity(&st.size).is_none_or(|b| b == 0) {
                return Err(format!("coordinatorStorage.size is a size Kubernetes reads, such as 2Gi, not {:?}", st.size));
            }
        }
        if self.rollout.progress_deadline_seconds < 10 {
            return Err(format!("rollout.progressDeadlineSeconds is 10 or more (a copy takes seconds to restart), not {}", self.rollout.progress_deadline_seconds));
        }
        self.autoscale.check()?;
        self.upgrade.check(&self.image)?;
        if let Some(f) = &self.follow {
            f.check()?;
        }
        if let Some(c) = &self.crawler {
            c.check()?;
        }
        if let Some(s) = &self.sheets {
            s.check()?;
        }
        if let Some(c) = &self.chloectx {
            c.check()?;
        }
        if let Some(n) = &self.netops {
            n.check()?;
        }
        if let Some(a) = &self.apps {
            a.check()?;
            if self.token_secret.is_none() {
                return Err("apps needs tokenSecret: without the cluster's token anyone who reached the studio would build apps on its model key".into());
            }
        }
        if let Some(c) = &self.cluster_token_secret {
            if self.token_secret.is_none() {
                return Err("clusterTokenSecret needs tokenSecret: the operator, and whoever runs the cluster, call the coordinators with that token".into());
            }
            if self.token_secret.as_ref() == Some(c) {
                return Err("clusterTokenSecret and tokenSecret name the same Secret: the cluster's token would be the admin's".into());
            }
        }
        let mut names = std::collections::HashSet::new();
        for e in &self.secret_env {
            let mut chars = e.name.chars();
            let first = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            if !first || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!("secretEnv: {:?} is not an environment variable's name (letters, digits and _, not first a digit)", e.name));
            }
            if e.name.starts_with("STRATA_") {
                return Err(format!("secretEnv: {} is the operator's to set (STRATA_ variables are its own; tokens go in tokenSecret)", e.name));
            }
            if !names.insert(e.name.as_str()) {
                return Err(format!("secretEnv names {} twice", e.name));
            }
            if e.secret.is_empty() || e.key.is_empty() {
                return Err(format!("secretEnv: {} needs a secret, and a key in it", e.name));
            }
        }
        if !matches!(self.write_acks.as_str(), "all" | "majority" | "primary") {
            return Err(format!("writeAcks is all, majority or primary, not {:?}", self.write_acks));
        }
        match (&self.schema, self.demo) {
            (Some(_), Some(_)) => Err("give a schema or demo, not both".into()),
            // No tables yet: the servers start with the placeholder
            // (chloedb-server --db on a new path), and CREATE TABLE fills them.
            (None, None) => Ok(()),
            (Some(s), None) => chloedb::schema::Schema::from_json_value(s).map(|_| ()).map_err(|e| format!("schema: {e}")),
            (None, Some(_)) => Ok(()),
        }
    }

    /// The schema every server is created with, as JSON: the spec's, the
    /// demo's, or with neither the placeholder of a database with no tables
    /// ([`chloedb::schema::Schema::placeholder`]).
    pub fn schema_json(&self) -> String {
        match (&self.schema, self.demo) {
            (Some(s), _) => s.to_string(),
            (None, Some(n)) => serde_json::to_string(&crate::demo::schema(n)).expect("a schema serialises"),
            (None, None) => serde_json::to_string(&chloedb::schema::Schema::placeholder()).expect("a schema serialises"),
        }
    }
}

/// The fields of a spec that say how it changes, not what the cluster
/// is: no version of their own ([`Spec::version`]), and left as they are
/// by a rollback.
pub const UNVERSIONED: &[&str] = &["rollout", "autoscale", "upgrade", "follow"];

/// The first 12 hex digits of the SHA-256 of `j`'s text.
pub(crate) fn short_hash(j: &Json) -> String {
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
    /// How the coordinators' restart with new settings stands: done once
    /// each runs them and is ready, and none with older ones is left.
    async fn coordinators_rolled(&self) -> Result<Rolled, String>;
    /// The servers that cannot run, as the platform says: one that exited
    /// or restarts again and again, an image that cannot be pulled; each
    /// said of whether it runs `spec`'s settings.
    async fn trouble(&self, spec: &Spec) -> Result<Vec<Trouble>, String>;
    /// Starts `image` on every node that runs copies, without serving, to
    /// see that it can be pulled and runs there before any server restarts
    /// with it (each restart then finds it pulled).
    async fn preflight(&self, image: &str) -> Result<Check, String>;
    /// Why copy `k` of set `id` has not started, when no node has room
    /// for it (the scheduler's words); `None` otherwise.
    async fn no_room(&self, _id: u32, _k: usize) -> Result<Option<String>, String> {
        Ok(None)
    }
    /// Ends the pre-flight: what it started goes.
    async fn end_preflight(&self) -> Result<(), String>;
    /// Runs the follow process `spec.follow` asks for, or none
    /// ([`follow`]): a Deployment of one on Kubernetes. Applied once the
    /// coordinators are there.
    async fn follow(&self, _spec: &Spec) -> Result<(), String> {
        Ok(())
    }
    /// Runs the crawler `spec.crawler` asks for, or none ([`crawler`]): a
    /// Deployment of one and its Service on Kubernetes. Applied once the
    /// coordinators are there.
    async fn crawler(&self, _spec: &Spec) -> Result<(), String> {
        Ok(())
    }
    /// Runs the spreadsheets `spec.sheets` asks for, or none ([`sheets`]):
    /// a Deployment of one and its Service on Kubernetes. Applied once the
    /// coordinators are there.
    async fn sheets(&self, _spec: &Spec) -> Result<(), String> {
        Ok(())
    }
    /// Runs ChloeCtx as `spec.chloectx` asks, or not ([`chloectx`]): a
    /// Deployment of one and its Service on Kubernetes. Applied once the
    /// coordinators are there.
    async fn chloectx(&self, _spec: &Spec) -> Result<(), String> {
        Ok(())
    }
    /// Runs the app engine `spec.apps` asks for, or none ([`apps`]): a
    /// Deployment of one, its Service and its volume on Kubernetes.
    /// Applied once the coordinators are there.
    async fn apps(&self, _spec: &Spec) -> Result<(), String> {
        Ok(())
    }
    /// Runs the network operations app `spec.netops` asks for, or none
    /// ([`netops`]): a Deployment of one and its Service on Kubernetes.
    /// Applied once the coordinators are there.
    async fn netops(&self, _spec: &Spec) -> Result<(), String> {
        Ok(())
    }
    /// Gives each app beside the cluster a key limited to its tables and a
    /// token of its own, made through the coordinators (`admin`), and
    /// takes them from an app that went ([`credentials`]). Before the
    /// apps are applied, which use them once they are there.
    async fn app_credentials(&self, _spec: &Spec, _admin: &Admin) -> Result<(), String> {
        Ok(())
    }
}

/// How the coordinators' restart stands ([`Platform::coordinators_rolled`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Rolled {
    pub done: bool,
    /// How far it is, in words ("1 of 2 restarted").
    pub text: String,
}

/// A server that cannot run ([`Platform::trouble`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Trouble {
    /// The copy's set and ordinal; none for a coordinator.
    pub copy: Option<(u32, usize)>,
    /// The pod, or process, in words.
    pub who: String,
    /// It runs the settings of the spec asked about.
    pub fresh: bool,
    pub text: String,
}

/// What a pre-flight found ([`Platform::preflight`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Check {
    /// The image runs on every node that runs copies, or nothing needs
    /// it: what it found.
    Done(String),
    /// Still starting, in words.
    Waiting(String),
    /// It does not run: why.
    Failed(String),
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
        let http = crate::http_client().timeout(Duration::from_secs(60)).build().expect("an HTTP client");
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
    /// The live numbers it read, if it read them.
    pub numbers: Option<Numbers>,
}

impl Outcome {
    fn new(phase: Phase, message: impl Into<String>) -> Outcome {
        Outcome { phase, message: message.into(), shards: Json::Null, rollout: Rollout::default(), numbers: None }
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
    /// The step of the rollout it waits on, and since when: the deadline
    /// (`progressDeadlineSeconds`) counts from it.
    pub step: Option<Step>,
    /// The image whose pre-flight passed for this version: it started on
    /// every node that runs copies.
    pub preflight: Option<String>,
    /// The version restarted servers: it is verified before it is ready.
    pub rolled: bool,
    /// The verification, once every server runs the version.
    pub verify: Option<Verify>,
    /// Why the version failed, once it did: rolled back, or held.
    pub failed: Option<Failed>,
}

/// A step of a rollout, and since when it is waited on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Step {
    pub name: String,
    pub since_ms: u64,
}

/// The verification of a version that restarted servers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Verify {
    pub since_ms: u64,
    /// Since when every check passed, without a break.
    pub healthy_since_ms: Option<u64>,
    /// A check that does not pass, and since when.
    pub failing: Option<String>,
    pub failing_since_ms: Option<u64>,
    /// When it passed: the version runs everywhere and is healthy.
    pub passed_ms: Option<u64>,
    /// What the checks found, in words.
    pub checks: Vec<String>,
}

/// Why a version failed, in words, and when.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Failed {
    pub at_ms: u64,
    /// The step it failed in (`preflight`, `coordinators`, `copies`,
    /// `verify`, `gate`).
    pub step: String,
    pub text: String,
}

impl Rollout {
    /// Waits on step `name` from now, unless it already does.
    fn wait(&mut self, name: &str, now: u64) {
        if self.step.as_ref().is_none_or(|s| s.name != name) {
            self.step = Some(Step { name: name.into(), since_ms: now });
        }
    }

    /// How long it has waited on step `name` (0 when it waits on another).
    fn waited(&self, name: &str, now: u64) -> u64 {
        self.step.as_ref().filter(|s| s.name == name).map_or(0, |s| now.saturating_sub(s.since_ms))
    }
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
    /// The version failed a check: it is rolled back, or held until it is
    /// resumed.
    Failed(Failed),
}

impl Hold {
    /// The outcome of not taking step `next`, when it is not taken.
    fn stop(&self, next: &str) -> Option<Outcome> {
        match self {
            Hold::Go => None,
            Hold::Paused => Some(Outcome::new(Phase::Paused, format!("paused (spec.rollout.paused); next: {next}"))),
            Hold::Held(h) => Some(Outcome::new(Phase::Paused, format!("held: {}; next: {next}", h.text))),
            Hold::Failed(f) => Some(Outcome::new(Phase::Paused, format!("the version failed: {}; next: {next}", f.text))),
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
    let kept = |r: &Rollout| if same { r.clone() } else { Rollout::default() };
    let k = kept(before);
    let r = Rollout { version, gates, held, paused: spec.rollout.paused, ungated, stale: Vec::new(), copies: 0, step: k.step, preflight: k.preflight, rolled: k.rolled, verify: k.verify, failed: k.failed };
    (r, trip)
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
            let stale = p.stale(spec).await?;
            if let Some(m) = restart_stuck(spec, p, &sets, |_| false, |_, _| true, &stale).await? {
                return Ok(Outcome::new(Phase::Creating, m));
            }
            return Ok(Outcome::new(Phase::Creating, format!("waiting for the first copy of {waiting} of {} shards to start", sets.len())));
        }
        let first: Vec<Vec<String>> = sets.iter().map(|s| vec![p.url(s.id, 0)]).collect();
        p.set_coordinator(&first, 1).await?;
        return Ok(Outcome::new(Phase::Creating, "starting the coordinator, which lays the cluster out"));
    };
    p.follow(spec).await?;
    // An app without a key of its own yet runs with the admin token, as
    // before such keys: a failure here is told, and holds nothing up.
    if let Err(e) = p.app_credentials(spec, admin).await {
        tracing::warn!(error = %e, "the apps' own keys");
    }
    p.crawler(spec).await?;
    p.sheets(spec).await?;
    p.chloectx(spec).await?;
    p.apps(spec).await?;
    p.netops(spec).await?;
    let view = match admin.view().await {
        Ok(v) => v,
        Err(e) => return Ok(Outcome::new(Phase::Creating, format!("waiting for the coordinator: {e}"))),
    };
    let stale = p.stale(spec).await?;
    let now = now_ms();
    let (mut rollout, trip) = gates(spec, &view.numbers, before, now);
    rollout.stale = stale.iter().map(|&(id, k)| p.url(id, k)).collect();
    rollout.copies = sets.iter().map(|s| s.replicas).sum();
    let trouble = p.trouble(spec).await?;
    // A gate over its limit while a version rolls out fails it, when a
    // failed version is rolled back; otherwise it holds the change.
    let trip = match trip {
        Some(t) if rolling(&rollout) && spec.rollout.on_failure == OnFailure::Rollback => {
            rollout.failed = Some(Failed { at_ms: now, step: "gate".into(), text: t.text });
            None
        }
        t => t,
    };
    let hold = match (rollout.failed.clone(), rollout.held.clone().or_else(|| trip.clone())) {
        _ if spec.rollout.paused => Hold::Paused,
        (Some(f), _) => Hold::Failed(f),
        (None, Some(h)) => Hold::Held(h),
        (None, None) => Hold::Go,
    };
    let mut out = act(spec, p, admin, &sets, &view, &listed, coordinators, &hold, &stale, &mut rollout, now, &trouble).await?;
    match out.phase {
        // A gate's hold stays once it held a step back.
        Phase::Paused if !spec.rollout.paused && rollout.failed.is_none() => rollout.held = rollout.held.or(trip),
        // Nothing is left to hold.
        Phase::Ready | Phase::Degraded => rollout.held = None,
        _ => {}
    }
    out.shards = shards_json(p, &view);
    out.rollout = rollout;
    out.numbers = Some(view.numbers);
    Ok(out)
}

/// The set of each shard of `view`, by its first server of this cluster.
fn set_of_shard<P: Platform>(p: &P, s: &Shard) -> Option<u32> {
    s.servers.iter().find_map(|r| p.set_of(&r.url)).map(|(id, _)| id)
}

/// The first step toward `spec` of those below, each but the ones that
/// heal held back by `hold`.
#[allow(clippy::too_many_arguments)]
async fn act<P: Platform>(
    spec: &Spec,
    p: &P,
    admin: &Admin,
    sets: &[Set],
    view: &View,
    listed: &[Vec<String>],
    coordinators: usize,
    hold: &Hold,
    stale: &[(u32, usize)],
    r: &mut Rollout,
    now: u64,
    trouble: &[Trouble],
) -> Result<Outcome, String> {
    let scaling = |m: String| Ok(Outcome::new(Phase::Scaling, m));
    macro_rules! unless_held {
        ($next:expr) => {
            if let Some(o) = hold.stop(&$next) {
                return Ok(o);
            }
        };
    }
    let up = |url: &str| view.shards.iter().any(|s| s.servers.iter().any(|r| r.url == url && r.up));
    // Not an extra copy that is about to go.
    let keep = |id: u32, k: usize| sets.iter().any(|s| s.id == id && k < keeps(spec, p, s, view, stale));
    if let Some(m) = restart_stuck(spec, p, sets, up, keep, stale).await? {
        r.rolled = true;
        return scaling(m);
    }
    let shard_of: HashMap<u32, usize> = view.shards.iter().enumerate().filter_map(|(i, s)| set_of_shard(p, s).map(|id| (id, i))).collect();
    let in_map: Vec<&str> = view.shards.iter().flat_map(|s| s.servers.iter().map(|r| r.url.as_str())).collect();
    let want = |s: &Set| keeps(spec, p, s, view, stale);
    // A copy of a set that serves a shard, which joins it (again) if it is
    // not in it.
    let kept = |id: u32, k: usize| sets.iter().any(|s| s.id == id && k < s.replicas.min(want(s))) && shard_of.contains_key(&id);
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
        if let Some(o) = preflight(spec, p, r, now).await? {
            return Ok(o);
        }
        let list: Vec<Vec<String>> = view.shards.iter().map(|s| s.servers.iter().map(|r| r.url.clone()).collect()).collect();
        p.set_coordinator(&list, spec.coordinators).await?;
        r.rolled = true;
        r.wait("coordinators", now);
        return scaling("restarting the coordinators with the new settings, one at a time (each new one serves before an old one stops)".into());
    }
    // The coordinators finish restarting before a copy does: a new one
    // that cannot run fails the version, and the old ones serve on.
    if r.step.as_ref().is_some_and(|s| s.name == "coordinators") {
        if let Some(t) = trouble.iter().find(|t| t.copy.is_none() && t.fresh) {
            return Ok(failed(r, "coordinators", format!("{} cannot run the new settings: {}", t.who, t.text), now));
        }
        let rolled = p.coordinators_rolled().await?;
        if !rolled.done {
            let deadline = spec.rollout.progress_deadline_seconds;
            if r.waited("coordinators", now) > deadline * 1000 {
                return Ok(failed(r, "coordinators", format!("the coordinators did not restart with the new settings in {deadline} s ({})", rolled.text), now));
            }
            return scaling(format!("restarting the coordinators with the new settings: {}", rolled.text));
        }
        r.step = None;
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
    // Copies per shard (and the extra copies a rollout adds, in roll(),
    // kept until it no longer needs them).
    for set in sets {
        let Some(&i) = shard_of.get(&set.id) else { continue };
        let want = want(set);
        if set.replicas < spec.replicas || set.replicas > want {
            unless_held!(format!("shard {i}: {} copies, from {}", spec.replicas, set.replicas));
        }
        if set.replicas < spec.replicas {
            p.scale_set(set.id, spec.replicas).await?;
            return scaling(format!("shard {i}: {} copies, from {}", spec.replicas, set.replicas));
        }
        if set.replicas > want {
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
    if let Some(o) = roll(spec, p, admin, sets, view, hold, stale, r, now, trouble).await? {
        return Ok(o);
    }
    if rolling(r) {
        if let Some(o) = verify(spec, admin, sets, view, r, now, trouble).await {
            return Ok(o);
        }
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

/// A copy running older settings than `spec`'s that has not started (its
/// pod not ready, and not `up` in the cluster) serves nothing, and may
/// never start with them: requests no node has room for, say, or a flag
/// its server does not take. It restarts at once with `spec`'s, its set
/// first made to start copies with them, rather than being waited for,
/// for ever, by the cluster being made, a new shard or a rollout. Not
/// held by a pause or a gate: it heals.
async fn restart_stuck<P: Platform>(spec: &Spec, p: &P, sets: &[Set], up: impl Fn(&str) -> bool, keep: impl Fn(u32, usize) -> bool, stale: &[(u32, usize)]) -> Result<Option<String>, String> {
    let ready = |id: u32, k: usize| sets.iter().any(|s| s.id == id && s.ready.contains(&k));
    let Some(&(id, k)) = stale.iter().find(|&&(id, k)| !ready(id, k) && !up(&p.url(id, k)) && keep(id, k)) else { return Ok(None) };
    if p.configure(id, spec).await? {
        return Ok(Some(format!("set {id}: new copies start with the new settings")));
    }
    let url = p.url(id, k);
    p.restart(id, k).await?;
    Ok(Some(format!("restarting {url}, which has not started, with the new settings")))
}

/// The copies set `set` keeps now: `spec.replicas`, or three for a shard
/// of fewer ([`RolloutSpec::extra_copies`]) while one of its own copies
/// runs older settings, and, once it has the extra ones (in its shard),
/// until each of its own is back and in sync.
fn keeps<P: Platform>(spec: &Spec, p: &P, set: &Set, view: &View, stale: &[(u32, usize)]) -> usize {
    let n = spec.replicas;
    if !spec.rollout.extra_copies || n >= 3 {
        return n;
    }
    let settled = |k: usize| {
        let url = p.url(set.id, k);
        set.ready.contains(&k) && view.shards.iter().flat_map(|s| &s.servers).any(|r| r.url == url && r.up && r.in_sync)
    };
    let restarts = stale.iter().any(|&(id, k)| id == set.id && k < n);
    let extra = p.url(set.id, n);
    let joined = view.shards.iter().any(|s| s.servers.iter().any(|r| r.url == extra));
    if restarts || (set.replicas > n && joined && !(0..n).all(settled)) {
        3
    } else {
        n
    }
}

/// A version that restarted servers and has not yet failed or passed its
/// verification: it rolls out.
fn rolling(r: &Rollout) -> bool {
    r.rolled && r.failed.is_none() && r.verify.as_ref().is_none_or(|v| v.passed_ms.is_none())
}

/// The version fails in `step`: no step is taken toward it, and it is
/// rolled back or held ([`OnFailure`]).
fn failed(r: &mut Rollout, step: &str, text: String, now: u64) -> Outcome {
    tracing::warn!(step, why = %text, "the version failed");
    r.failed = Some(Failed { at_ms: now, step: step.into(), text: text.clone() });
    r.step = None;
    Outcome::new(Phase::Paused, format!("the version failed: {text}"))
}

/// Before a server restarts with an image it has not run, the image is
/// started on every node that runs copies ([`Platform::preflight`]): one
/// that cannot be pulled, or does not run there, fails the version before
/// any server stops. A rollback goes back to an image that ran: it is not
/// tried first.
async fn preflight<P: Platform>(spec: &Spec, p: &P, r: &mut Rollout, now: u64) -> Result<Option<Outcome>, String> {
    if r.ungated || r.preflight.as_deref() == Some(spec.image.as_str()) {
        return Ok(None);
    }
    match p.preflight(&spec.image).await? {
        Check::Done(found) => {
            p.end_preflight().await?;
            tracing::info!(image = %spec.image, %found, "pre-flight passed");
            r.preflight = Some(spec.image.clone());
            if r.step.as_ref().is_some_and(|s| s.name == "preflight") {
                r.step = None;
            }
            Ok(None)
        }
        Check::Failed(why) => {
            p.end_preflight().await?;
            Ok(Some(failed(r, "preflight", format!("the image {} does not run here, and no server was restarted with it: {why}", spec.image), now)))
        }
        Check::Waiting(what) => {
            r.wait("preflight", now);
            let deadline = spec.rollout.progress_deadline_seconds;
            if r.waited("preflight", now) > deadline * 1000 {
                p.end_preflight().await?;
                return Ok(Some(failed(r, "preflight", format!("the image {} did not start on every node in {deadline} s ({what}), and no server was restarted with it", spec.image), now)));
            }
            Ok(Some(Outcome::new(Phase::Scaling, format!("trying the image {} on every node before a server restarts with it: {what}", spec.image))))
        }
    }
}

/// New settings: each set starts its copies with them, the volumes grow,
/// a shard of fewer than three copies takes extra ones that run them
/// ([`RolloutSpec::extra_copies`]), and the copies running older settings
/// (`stale`) restart one at a time.
/// The next restarts only once every copy is back and in sync, and the
/// copies restarted have served `soak_seconds`; a primary is first handed
/// to a copy of its shard in sync, one that runs the new settings if
/// there is one (of the shard's own copies before an extra one). Without
/// extra copies, a shard of one copy cannot be handed over: its copy
/// restarts, and its writes wait for it. A copy restarted that cannot run
/// (it exits, or its image cannot be pulled), or is not back and caught up
/// within `progress_deadline_seconds`, fails the version.
#[allow(clippy::too_many_arguments)]
async fn roll<P: Platform>(
    spec: &Spec,
    p: &P,
    admin: &Admin,
    sets: &[Set],
    view: &View,
    hold: &Hold,
    stale: &[(u32, usize)],
    r: &mut Rollout,
    now: u64,
    trouble: &[Trouble],
) -> Result<Option<Outcome>, String> {
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
    if rolling(r) {
        if let Some(t) = trouble.iter().find(|t| t.fresh && t.copy.is_some()) {
            return Ok(Some(failed(r, "copies", format!("{} cannot run the new settings: {}", t.who, t.text), now)));
        }
    }
    // The last restart's step (or the extra copies'), while the copy it
    // restarted settles.
    let restarted = r.step.clone().filter(|s| s.name.starts_with("restart ") || s.name.starts_with("add "));
    if stale.is_empty() && restarted.is_none() {
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
        let deadline = spec.rollout.progress_deadline_seconds;
        // An extra copy no node has room for waits for room, and says so.
        let mut no_room = None;
        if let Some(urls) = restarted.as_ref().and_then(|s| s.name.strip_prefix("add ")) {
            for url in urls.split(" and ").filter(|u| unsettled.iter().any(|x| x == u)) {
                if let Some((id, k)) = p.set_of(url) {
                    if let Some(why) = p.no_room(id, k).await? {
                        no_room = Some(format!("no node has room for {url}, the extra copy its shard's copies restart beside ({why})"));
                        break;
                    }
                }
            }
        }
        if let Some(step) = &restarted {
            if now.saturating_sub(step.since_ms) > deadline * 1000 {
                let what = match (&no_room, step.name.strip_prefix("add ")) {
                    (Some(why), _) => format!(
                        "{why} for more than {deadline} s: add a node{} or lower spec.resources, or set rollout.extraCopies false to restart a shard's copies without one (the shard then serves no reads and takes no writes for the seconds each restart takes)",
                        if spec.require_copies_apart { " (with requireCopiesApart, one that runs none of the shard's copies)" } else { "" }
                    ),
                    (None, Some(urls)) => format!("{urls} started with the new settings more than {deadline} s ago, and {} not back and caught up", unsettled.join(", ")),
                    (None, None) => format!("{} restarted with the new settings more than {deadline} s ago, and {} not back and caught up", step.name.trim_start_matches("restart "), unsettled.join(", ")),
                };
                return Ok(Some(failed(r, "copies", what, now)));
            }
        }
        if let Some(why) = no_room {
            return scaling(format!("{why}: it starts once one has (a node added, or smaller spec.resources), or the version fails {deadline} s after it was made"));
        }
        return scaling(format!("rolling out the new settings ({} left): waiting for {} to be back and caught up", left(stale.len()), unsettled.join(", ")));
    }
    if restarted.is_some() {
        r.step = None;
    }
    if stale.is_empty() {
        return Ok(None);
    }
    let fresh = |url: &str| !stale.iter().any(|&(id, k)| p.url(id, k) == url);
    // And serves a while, for the gates to see it.
    let soak = spec.rollout.soak_seconds as f64;
    let young = view.shards.iter().flat_map(|s| &s.servers).filter(|r| fresh(&r.url)).filter_map(|r| Some((r, r.uptime_s?))).filter(|&(_, u)| u < soak).min_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((r, _)) = young {
        return scaling(format!("rolling out the new settings ({} left): {} serves {soak:.0} s with them before the next restarts", left(stale.len()), r.url));
    }
    let primary = |url: &str| view.shards.iter().any(|s| s.servers.iter().any(|r| r.url == url && r.takes_writes));
    // The copies of a shard that has its extra copies first, so that one
    // shard at a time has them; a replica first, so that each primary can
    // go to a restarted copy.
    let has_extra = |id: u32| sets.iter().any(|s| s.id == id && s.replicas > spec.replicas);
    let &(id, k) = stale.iter().min_by_key(|&&(id, k)| (!has_extra(id), primary(&p.url(id, k)))).unwrap_or(&stale[0]);
    let url = p.url(id, k);
    // A shard of fewer than three copies takes more first, which start
    // with the new settings: most of its copies serve while each of its
    // own restarts.
    if let Some(set) = sets.iter().find(|s| s.id == id && s.replicas < keeps(spec, p, s, view, stale)) {
        let n = keeps(spec, p, set, view, stale);
        let urls: Vec<String> = (set.replicas..n).map(|k| p.url(id, k)).collect();
        let i = view.shards.iter().position(|s| s.servers.iter().any(|r| r.url == url)).map_or_else(|| format!("of set {id}"), |i| i.to_string());
        let what = format!("shard {i}: {} {} with the new settings, so that most of its copies serve while each of its {} restarts", urls.join(" and "), if urls.len() == 1 { "starts as a third copy" } else { "start as extra copies" }, set.replicas);
        if let Some(o) = hold.stop(&what) {
            return Ok(Some(o));
        }
        if let Some(o) = preflight(spec, p, r, now).await? {
            return Ok(Some(o));
        }
        p.scale_set(id, n).await?;
        r.rolled = true;
        r.wait(&format!("add {}", urls.join(" and ")), now);
        return scaling(what);
    }
    if let Some(o) = hold.stop(&format!("restarting {url} with the new settings ({} left)", left(stale.len()))) {
        return Ok(Some(o));
    }
    if let Some(o) = preflight(spec, p, r, now).await? {
        return Ok(Some(o));
    }
    if let Some((i, s)) = view.shards.iter().enumerate().find(|(_, s)| s.servers.iter().any(|r| r.url == url && r.takes_writes)) {
        let mut to: Vec<&Server> = s.servers.iter().filter(|o| o.url != url && o.up && o.in_sync && o.role == "replica").collect();
        // A restarted copy of the shard's own before an extra one, which
        // then leaves without a handover.
        let extra = |url: &str| p.set_of(url).is_some_and(|(_, k)| k >= spec.replicas);
        to.sort_by_key(|o| (!fresh(&o.url), extra(&o.url)));
        if let Some(to) = to.first() {
            admin.post(&format!("/v1/admin/shards/{i}/promote"), json!({"url": to.url})).await?;
            return scaling(format!("shard {i}: handing the primary over from {url} to {}, before {url} restarts with the new settings", to.url));
        }
    }
    p.restart(id, k).await?;
    r.rolled = true;
    r.wait(&format!("restart {url}"), now);
    scaling(format!("restarting {url} with the new settings ({} left)", left(stale.len() - 1)))
}

/// Once every server runs the version that restarted them, it is watched
/// for `verify_seconds`: every copy back and in sync, no server of it
/// stopping, the gates under their limits and a query answered through
/// the coordinators. A check that fails for `hold_after_seconds` (a
/// server that stops, at once), or no `verify_seconds` healthy in a row
/// within `progress_deadline_seconds`, fails the version. Answers the
/// outcome while it verifies, and none once it passed.
async fn verify(spec: &Spec, admin: &Admin, sets: &[Set], view: &View, r: &mut Rollout, now: u64, trouble: &[Trouble]) -> Option<Outcome> {
    let need = spec.rollout.verify_seconds * 1000;
    let mut v = r.verify.clone().unwrap_or(Verify { since_ms: now, ..Verify::default() });
    if need == 0 {
        v.passed_ms = Some(now);
        r.verify = Some(v);
        return None;
    }
    if let Some(t) = trouble.iter().find(|t| t.fresh) {
        return Some(failed(r, "verify", format!("{} stopped running while the version was verified: {}", t.who, t.text), now));
    }
    let mut checks = Vec::new();
    let mut bad: Option<String> = None;
    let servers: Vec<&Server> = view.shards.iter().flat_map(|s| &s.servers).collect();
    let down: Vec<&str> = servers.iter().filter(|r| !r.up).map(|r| r.url.as_str()).collect();
    let behind: Vec<&str> = servers.iter().filter(|r| r.up && !r.in_sync).map(|r| r.url.as_str()).collect();
    let (copies, ready) = (sets.iter().map(|s| s.replicas).sum::<usize>(), sets.iter().map(|s| s.ready.iter().filter(|&&k| k < s.replicas).count()).sum::<usize>());
    if !down.is_empty() {
        bad = Some(format!("not answering: {}", down.join(", ")));
    } else if !behind.is_empty() {
        bad = Some(format!("not caught up: {}", behind.join(", ")));
    } else if ready < copies {
        bad = Some(format!("{ready} of {copies} copies ready"));
    } else {
        checks.push(format!("{copies} copies back and in sync"));
    }
    let set: Vec<(&str, f64)> = spec.rollout.gates.limits().into_iter().filter_map(|(k, l)| Some((k, l?))).collect();
    for &(name, limit) in &set {
        if let Some(n) = view.numbers.get(name).filter(|n| *n > limit) {
            bad.get_or_insert(format!("{} is {}, over its {} limit", gate_title(name), gate_words(name, n), gate_words(name, limit)));
        }
    }
    if !set.is_empty() && bad.is_none() {
        checks.push("the gates under their limits".into());
    }
    let table = spec.first_table();
    let t0 = std::time::Instant::now();
    match admin.post("/v1/sql", json!({"sql": format!("SELECT COUNT(*) FROM {table}")})).await {
        Ok(j) => checks.push(format!("a query through the coordinators answered ({} rows in {table}, {:.0} ms)", j["rows"][0][0], t0.elapsed().as_secs_f64() * 1000.0)),
        Err(e) => {
            bad.get_or_insert(format!("a query through the coordinators failed: {e}"));
        }
    }
    let secs = |ms: u64| ms / 1000;
    let deadline = spec.rollout.progress_deadline_seconds * 1000;
    let message = match &bad {
        None => {
            v.failing = None;
            v.failing_since_ms = None;
            let since = *v.healthy_since_ms.get_or_insert(now);
            if now.saturating_sub(since) >= need {
                v.passed_ms = Some(now);
                v.checks = checks;
                tracing::info!(checks = %v.checks.join("; "), "the version passed its verification");
                r.verify = Some(v);
                return None;
            }
            // The same words all along (the log keeps each new message):
            // how far it is, is in `status.rollout.verify`.
            let said: Vec<&str> = checks.iter().map(|c| c.split(" (").next().unwrap_or(c)).collect();
            format!("verifying the version: healthy for {} s in a row ({})", secs(need), said.join(", "))
        }
        Some(b) => {
            v.healthy_since_ms = None;
            let since = *v.failing_since_ms.get_or_insert(now);
            v.failing = Some(b.clone());
            let after = spec.rollout.hold_after_seconds * 1000;
            if now.saturating_sub(since) >= after {
                return Some(failed(r, "verify", format!("after it rolled out, {b} for {} s", secs(now.saturating_sub(since))), now));
            }
            format!("verifying the version: {b} (it fails if this lasts {} s)", secs(after))
        }
    };
    if now.saturating_sub(v.since_ms) > deadline {
        let last = bad.unwrap_or_else(|| "healthy, but not long enough".into());
        return Some(failed(r, "verify", format!("not healthy for {} s in a row within {} s of the rollout (last: {last})", secs(need), secs(deadline)), now));
    }
    v.checks = checks;
    r.verify = Some(v);
    Some(Outcome::new(Phase::Scaling, message))
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

/// The merge patch that takes the spec `now` back to `then` (a version's
/// spec, as `status.history` keeps it), leaving how it changes
/// ([`UNVERSIONED`]) as it is.
pub fn back_to(now: &Json, then: &Json) -> Json {
    let mut from = now.clone();
    let mut to = then.clone();
    for j in [&mut from, &mut to] {
        if let Some(o) = j.as_object_mut() {
            for k in UNVERSIONED {
                o.remove(*k);
            }
        }
    }
    merge_diff(&from, &to)
}

/// The version a failed one goes back to (`status.history`): the latest
/// before it that ran everywhere (`readyMs`), of another hash. None when
/// the failed one is itself a rollback (`rollback`: its rollout is
/// ungated), or there is none to go back to.
pub fn rollback_target<'a>(history: &'a Json, spec: &Spec, rollback: bool) -> Option<&'a Json> {
    let h = history.as_array()?;
    let version = spec.version();
    h.last().filter(|v| v["hash"] == version.as_str() && !rollback)?;
    h.iter().rev().skip(1).find(|v| v["hash"] != version.as_str() && !v["readyMs"].is_null())
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
            for k in UNVERSIONED {
                o.remove(*k);
            }
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
    // A failure resumed is tried again: its step's clock starts again.
    if before.failed.as_ref().is_some_and(|f| resume == Some(f.at_ms.to_string().as_str())) {
        before.failed = None;
        before.step = None;
        before.verify = None;
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
    if let (Some(a), Some(b)) = (&old.apps, &new.apps) {
        if quantity(&b.storage.size) < quantity(&a.storage.size) {
            refused = Some(format!("the app engine's volume only grows: {} is smaller than {}", b.storage.size, a.storage.size));
        }
        if b.storage.storage_class_name != a.storage.storage_class_name {
            warnings.push("apps.storage.storageClassName applies to a volume made from now on: the app engine keeps the one it has".into());
        }
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
        if new.replicas < 3 && new.rollout.extra_copies {
            let more = 3 - new.replicas;
            steps.push(format!(
                "each shard first takes {} with the new settings, sent a copy of its data, so that most of its copies serve while each of its {} restarts; {} once they run the new settings",
                if more == 1 { "a third copy".to_string() } else { format!("{more} extra copies") },
                new.replicas,
                if more == 1 { "it goes" } else { "they go" }
            ));
        }
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
        if new.crawler.is_some() {
            warnings.push("no token: anything that reaches the crawler's pod may start crawls into the cluster and grow it".into());
        }
        if new.sheets.is_some() {
            warnings.push("no token: anything that reaches the spreadsheets' pod may read and change the cluster's tables through it".into());
        }
        if new.netops.is_some() {
            warnings.push("no token: anything that reaches the network operations pod may read and change its tables through it, and start its simulator's faults".into());
        }
        if new.chloectx.is_some() {
            warnings.push("no token: anything that reaches ChloeCtx's pod may read and change the cluster's tables through it, and call the model with the cluster's key".into());
        }
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
    if new.autoscale != old.autoscale {
        steps.push(format!("new autoscaling rules, no restart: {}", new.autoscale.said()));
    }
    if new.follow != old.follow {
        steps.push(match (&old.follow, &new.follow) {
            (None, Some(f)) | (Some(_), Some(f)) if old.follow.as_ref().is_none_or(|o| o.name != f.name) => format!(
                "a follow process of {} starts (no server restarts): {}, then it applies {}'s feed; from its first write the cluster refuses its clients' writes (\"this cluster follows {}\")",
                f.name,
                match f.start {
                    follow::Start::Follow => "it copies the source the first time, else it goes on from where the cluster is".to_string(),
                    follow::Start::Copy => "it copies the source afresh, writing its rows over these and deleting the keys it lacks".to_string(),
                    follow::Start::Reverse => format!("it follows {} back from its promotion from this cluster, without a copy (refused if this cluster took writes since)", f.name),
                },
                f.name,
                f.name
            ),
            (Some(o), None) => format!("the follow process of {} stops (its Deployment goes); the cluster refuses its clients' writes until following is stopped or it is promoted", o.name),
            (Some(a), Some(b)) if b.paused && !a.paused => format!("the follow process of {} pauses: nothing more is applied, and the cluster still refuses its clients' writes", b.name),
            (Some(a), Some(b)) if a.paused && !b.paused => format!("the follow process of {} starts again, from where it was", b.name),
            (_, Some(b)) => format!("the follow process of {} restarts with the new settings, from where it was", b.name),
            (None, None) => unreachable!("the follow specs differ"),
        });
    }
    if new.crawler != old.crawler {
        let grows = |c: &CrawlerSpec| match (c.grow, c.split_rows) {
            (false, _) | (true, Some(0)) => "it never adds a shard".to_string(),
            (true, n) => format!(
                "it adds a shard when one holds more than {} rows of a crawl's table, up to {} shards",
                n.unwrap_or(100_000),
                c.max_shards.unwrap_or(8)
            ),
        };
        steps.push(match (&old.crawler, &new.crawler) {
            (None, Some(c)) => format!(
                "a crawler starts beside the cluster (chloedb-crawl serve, a Deployment of one) and the coordinators list it, so the command center shows Apps → Crawler; {}",
                grows(c)
            ),
            (Some(_), None) => "the crawler stops (its Deployment and Service go; the tables it filled stay) and the coordinators no longer list it".into(),
            (Some(a), Some(b)) if b.paused && !a.paused => "the crawler pauses: its pod goes, with the crawls running in it (their tables stay)".into(),
            (Some(a), Some(b)) if a.paused && !b.paused => format!("the crawler starts again, with no crawls; {}", grows(b)),
            (_, Some(b)) => format!("the crawler restarts with the new settings, ending the crawls running in it (their tables stay); {}", grows(b)),
            (None, None) => unreachable!("the crawler specs differ"),
        });
    }
    if let Some(a) = &new.apps {
        if !new.secret_env.iter().any(|e| e.name == "ANTHROPIC_API_KEY") {
            warnings.push("the app engine has no model key: name ANTHROPIC_API_KEY in secretEnv, or it builds no app".into());
        }
        if a.public_url.is_none() {
            warnings.push("apps.publicUrl is not set: the command center lists the engine's apps but cannot open the studio".into());
        }
        if a.service_type == "LoadBalancer" && old.apps.as_ref().is_none_or(|o| o.service_type != "LoadBalancer") {
            warnings.push("a LoadBalancer Service is billed by most clouds (on DigitalOcean about $12 a month)".into());
        }
        if a.spend_cap_usd == 0.0 {
            warnings.push("apps.spendCapUsd is 0: nothing limits what the model may cost".into());
        }
    }
    if new.apps != old.apps {
        let what = |a: &AppsSpec| match a.spend_cap_usd {
            c if c == 0.0 => "with no limit on what the model may cost".to_string(),
            _ => format!("the model may cost ${} before calls are refused", a.spend_cap()),
        };
        steps.push(match (&old.apps, &new.apps) {
            (None, Some(a)) => format!(
                "the app engine starts beside the cluster (chloedb-apps, a Deployment of one, with a {} volume) and the coordinators list it, so the command center shows Apps → App creator; its apps are kept in this cluster's tables; {}",
                a.storage.size,
                what(a)
            ),
            (Some(_), None) => "the app engine stops (its Deployment and Service go; the apps stay in their tables, and its volume stays until the cluster goes) and the coordinators no longer list it".into(),
            (Some(a), Some(b)) if b.paused && !a.paused => "the app engine pauses: its pod goes (the apps stay in their tables)".into(),
            (Some(a), Some(b)) if a.paused && !b.paused => format!("the app engine starts again, with the apps it had; {}", what(b)),
            (Some(a), Some(b)) if quantity(&b.storage.size) > quantity(&a.storage.size) => format!("the app engine's volume grows from {} to {} (the storage class must allow it)", a.storage.size, b.storage.size),
            (_, Some(b)) => format!("the app engine restarts with the new settings, with the apps it had; {}", what(b)),
            (None, None) => unreachable!("the apps specs differ"),
        });
    }
    if new.sheets != old.sheets {
        steps.push(match (&old.sheets, &new.sheets) {
            (None, Some(_)) => "spreadsheets start beside the cluster (chloedb-sheets serve, a Deployment of one) and the coordinators list them, so the command center shows Apps → Sheets; the workbooks are kept in the tables sheets_books and sheets_cells".into(),
            (Some(_), None) => "the spreadsheets stop (their Deployment and Service go; the workbooks stay in their tables) and the coordinators no longer list them".into(),
            (Some(a), Some(b)) if b.paused && !a.paused => "the spreadsheets pause: their pod goes (the workbooks stay in their tables)".into(),
            (Some(a), Some(b)) if a.paused && !b.paused => "the spreadsheets start again, with the workbooks they had".into(),
            (_, Some(_)) => "the spreadsheets restart with the new settings, with the workbooks they had".into(),
            (None, None) => unreachable!("the sheets specs differ"),
        });
    }
    if new.netops != old.netops {
        steps.push(match (&old.netops, &new.netops) {
            (None, Some(_)) => "network operations start beside the cluster (chloedb-netops serve, a Deployment of one) and the coordinators list them, so the command center shows Apps → Network operations; the network, its telemetry and its incidents are kept in the tables netops_*, which the app makes and its simulator writes to each minute".into(),
            (Some(_), None) => "network operations stop (their Deployment and Service go; the tables netops_* stay) and the coordinators no longer list them".into(),
            (Some(a), Some(b)) if b.paused && !a.paused => "network operations pause: their pod goes (the tables netops_* stay, and nothing is written to them)".into(),
            (Some(a), Some(b)) if a.paused && !b.paused => "network operations start again, from the tables they had".into(),
            (_, Some(_)) => "network operations restart with the new settings, from the tables they had".into(),
            (None, None) => unreachable!("the netops specs differ"),
        });
    }
    if new.chloectx != old.chloectx {
        steps.push(match (&old.chloectx, &new.chloectx) {
            (None, Some(c)) => format!("ChloeCtx starts beside the cluster (chloectx app, a Deployment of one{}) and the coordinators list it, so the command center shows Apps → ChloeCtx; its spaces are kept in the tables ctx_spaces and ctx_<space>", if c.needs_model() { ", which downloads its embedding model at each start" } else { "" }),
            (Some(_), None) => "ChloeCtx stops (its Deployment and Service go; the spaces stay in their tables) and the coordinators no longer list it".into(),
            (Some(a), Some(b)) if b.paused && !a.paused => "ChloeCtx pauses: its pod goes (the spaces stay in their tables)".into(),
            (Some(a), Some(b)) if a.paused && !b.paused => "ChloeCtx starts again, with the spaces it had".into(),
            (Some(a), Some(b)) if a.embedder() != b.embedder() => format!("ChloeCtx restarts with the embedder {}: notes written before match by their words only", b.embedder()),
            (_, Some(_)) => "ChloeCtx restarts with the new settings, with the spaces it had".into(),
            (None, None) => unreachable!("the chloectx specs differ"),
        });
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
            "image": {"type": "string", "description": "The chloedb-server image of every server (default chloedb-server:dev)."},
            "imagePullPolicy": {"type": "string"},
            "environment": {"type": "string", "description": "What the cluster is to its people (production, staging): the command center shows it on every page."},
            "shards": int(1, "Shards, each holding a part of the 4,096 slots (default 1)."),
            "replicas": int(1, "Copies of each shard: a primary and its replicas (default 3)."),
            "coordinators": int(1, "Coordinators behind the cluster's Service (default 1)."),
            "storage": {"type": "object", "properties": {
                "size": {"type": "string", "description": "Each copy's volume (default 10Gi)."},
                "storageClassName": {"type": "string"},
            }},
            "schema": free("The first table's schema, as chloedb-server --schema takes it. Without it and without demo, the cluster starts with no tables."),
            "demo": int(0, "Instead of a schema: this many demo products, split over the shards the cluster starts with."),
            "failoverAfterMs": int(0, "--failover-after-ms of every server (default 3000; 0: failover by hand)."),
            "writeAcks": {"type": "string", "enum": ["all", "majority", "primary"], "description": "--write-acks of every primary (default all)."},
            "tokenSecret": {"type": "string", "description": "A Secret whose key token every server and coordinator requires; with clusterTokenSecret, the coordinators only (the admin token)."},
            "clusterTokenSecret": {"type": "string", "description": "A Secret whose key token is the cluster's token (--cluster-token), which the servers and coordinators send each other and clients never hold. Needs tokenSecret."},
            "secretEnv": {"type": "array", "description": "Environment variables of every server and coordinator taken from Secrets: the keys of models behind an API (ANTHROPIC_API_KEY for the agents, LLM fields and reranking; OPENAI_API_KEY, COHERE_API_KEY, ... for hosted embeddings). Changing it restarts the copies one at a time.", "items": {
                "type": "object", "required": ["name", "secret"], "properties": {
                    "name": {"type": "string", "description": "The variable, such as ANTHROPIC_API_KEY."},
                    "secret": {"type": "string", "description": "The Secret, in the cluster's namespace."},
                    "key": {"type": "string", "description": "The Secret's key that holds the value (default key)."},
                },
            }},
            "resources": free("Resources of each server's container."),
            "coordinatorResources": free("Resources of each coordinator's container."),
            "args": {"type": "array", "items": {"type": "string"}, "description": "More arguments for every server."},
            "coordinatorArgs": {"type": "array", "items": {"type": "string"}, "description": "More arguments for every coordinator."},
            "coordinatorStorage": {"type": "object", "description": "A volume for each coordinator, which keeps its history across restarts and rollouts: its log store (events, alerts, query statistics, table growth), its audit log and its alert rules. With it the coordinators run as a StatefulSet, each with a volume; without it (the default) as a Deployment that keeps all of that in memory only. Run 2 coordinators or more with it: a StatefulSet restarts one before it starts its next.", "properties": {
                "size": {"type": "string", "description": "Each coordinator's volume (default 2Gi; its log store takes up to 1 GiB, --log-store-mb)."},
                "storageClassName": {"type": "string"},
            }},
            "requireCopiesApart": {"type": "boolean", "description": "A shard's copies must run on different nodes (required of the scheduler, not preferred): a copy waits for a node that runs none of its shard's other copies, and a rollout's extra copy for one more node than the shard has copies (or set rollout.extraCopies false). Default false: preferred, so that fewer nodes than copies still run them all."},
            "rollout": {"type": "object", "description": "How changes are made: paused, or held by gates on the cluster's live numbers.", "properties": {
                "paused": {"type": "boolean", "description": "While true the operator takes no step toward the spec (a copy that lost its data still joins again)."},
                "gates": {"type": "object", "description": "Limits on live numbers: one over its limit for holdAfterSeconds holds the change until it is resumed.", "properties": {
                    "p99Ms": {"type": "number", "minimum": 0, "description": "The search p99 the coordinator's clients see, in ms."},
                    "errorRatio": {"type": "number", "minimum": 0, "maximum": 1, "description": "The share of searches that fail."},
                    "replicaBehind": {"type": "number", "minimum": 0, "description": "The transactions a replica is behind its primary."},
                    "diskUsedRatio": {"type": "number", "minimum": 0, "maximum": 1, "description": "The fullest volume's used share."},
                }},
                "holdAfterSeconds": int(0, "How long a gate is over its limit before it holds the change, or a check fails while a version is verified (default 30)."),
                "soakSeconds": int(0, "How long a copy restarted with new settings serves before the next restarts (default 0)."),
                "onFailure": {"type": "string", "enum": ["rollback", "hold"], "description": "What a version that fails a check gets: rollback (default) restores the last version that ran everywhere; hold holds it until it is resumed."},
                "progressDeadlineSeconds": int(10, "How long one step of a rollout (the image tried on every node, the coordinators restarting, a copy restarting and catching up, the verification) may take before the version fails (default 600)."),
                "verifySeconds": int(0, "How long the cluster must stay healthy once every server runs a version that restarted them, before it counts as rolled out (default 60; 0: not verified)."),
                "extraCopies": {"type": "boolean", "description": "A shard of fewer than three copies takes more, with the new settings, before its copies restart, and lets them go after: most of its copies serve while each restarts (default true)."},
            }},
            "autoscale": autoscale::schema(),
            "upgrade": upgrade::schema(),
            "follow": follow::schema(),
            "crawler": crawler::schema(),
            "sheets": sheets::schema(),
            "chloectx": chloectx::schema(),
            "apps": apps::schema(),
            "netops": netops::schema(),
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
        // Neither a schema nor demo: no tables yet, the placeholder first.
        let empty: Spec = serde_json::from_value(json!({})).unwrap();
        empty.check().unwrap();
        assert!(chloedb::schema::Schema::from_json(&empty.schema_json()).unwrap().is_placeholder());
        assert!(bad(json!({"writeAcks": "most", "demo": 1})).contains("writeAcks"));
        assert!(bad(json!({"schema": {"table": "t"}})).contains("schema:"));
        assert!(bad(json!({"demo": 1, "clusterTokenSecret": "c"})).contains("needs tokenSecret"));
        assert!(bad(json!({"demo": 1, "clusterTokenSecret": "t", "tokenSecret": "t"})).contains("the same Secret"));
        serde_json::from_value::<Spec>(json!({"demo": 1, "clusterTokenSecret": "c", "tokenSecret": "t"})).unwrap().check().unwrap();
        let s: Spec = serde_json::from_value(json!({"demo": 30})).unwrap();
        assert_eq!(chloedb::schema::Schema::from_json(&s.schema_json()).unwrap(), crate::demo::schema(30));
    }

    #[test]
    fn the_app_engine_needs_the_clusters_token_and_says_what_it_costs() {
        let none: Spec = serde_json::from_value(json!({"demo": 10, "tokenSecret": "t"})).unwrap();
        let open: Spec = serde_json::from_value(json!({"demo": 10, "apps": {}})).unwrap();
        assert!(open.check().unwrap_err().contains("apps needs tokenSecret"));
        let bad: Spec = serde_json::from_value(json!({"demo": 10, "tokenSecret": "t", "apps": {"nodePort": 30731}})).unwrap();
        assert!(bad.check().unwrap_err().contains("needs serviceType NodePort"));
        let (_, new) = patched(&serde_json::to_value(&none).unwrap(), &json!({"apps": {}})).unwrap();
        assert_eq!(new.apps(), ["apps"]);
        let p = plan(&none, &new, 1, 3);
        assert!(p["refused"].is_null(), "{p}");
        let (steps, warnings) = (p["steps"].to_string(), p["warnings"].to_string());
        for want in ["the coordinators restart", "the app engine starts beside the cluster", "1Gi volume", "Apps → App creator", "may cost $25 before calls are refused"] {
            assert!(steps.contains(want), "{want}: {steps}");
        }
        for want in ["no model key", "publicUrl is not set"] {
            assert!(warnings.contains(want), "{want}: {warnings}");
        }
        assert_eq!(p["restarts"], false, "no server restarts for it");
        let with_key = |patch: Json| patched(&serde_json::to_value(&new).unwrap(), &patch).unwrap().1;
        let all = with_key(json!({"secretEnv": [{"name": "ANTHROPIC_API_KEY", "secret": "anthropic"}], "apps": {"publicUrl": "https://studio.example", "serviceType": "LoadBalancer", "spendCapUsd": 0}}));
        let p = plan(&new, &all, 1, 3);
        let warnings = p["warnings"].to_string();
        assert!(!warnings.contains("no model key") && !warnings.contains("publicUrl"), "{warnings}");
        assert!(warnings.contains("LoadBalancer Service is billed"), "{warnings}");
        assert!(warnings.contains("nothing limits what the model may cost"), "{warnings}");
        assert!(p["steps"].to_string().contains("no limit on what the model may cost"), "{p}");
        let steps = |a: &Spec, b: &Spec| plan(a, b, 1, 3)["steps"].to_string();
        assert!(steps(&new, &with_key(json!({"apps": {"paused": true}}))).contains("the app engine pauses"));
        assert!(steps(&new, &with_key(json!({"apps": {"storage": {"size": "5Gi"}}}))).contains("volume grows from 1Gi to 5Gi"));
        assert!(steps(&new, &none).contains("its volume stays until the cluster goes"));
        let smaller = plan(&with_key(json!({"apps": {"storage": {"size": "5Gi"}}})), &new, 1, 3);
        assert!(smaller["refused"].as_str().unwrap_or_default().contains("the app engine's volume only grows"), "{smaller}");
        // A cluster without it keeps its coordinators' hash.
        let mut j = json!({"image": none.image, "imagePullPolicy": null, "resources": null, "args": [], "tokenSecret": "t", "clusterTokenSecret": null});
        assert_eq!(none.coordinator_config(), short_hash(&j));
        j["apps"] = json!(["apps"]);
        j["appTokens"] = json!(credentials::TOKEN_DIR);
        assert_eq!(new.coordinator_config(), short_hash(&j));
        assert!(!serde_json::to_value(&none).unwrap().as_object().unwrap().contains_key("apps"));
    }

    #[test]
    fn a_plan_says_what_turning_chloectx_on_does() {
        let none: Spec = serde_json::from_value(json!({"demo": 10, "tokenSecret": "t", "sheets": {}})).unwrap();
        let (_, new) = patched(&serde_json::to_value(&none).unwrap(), &json!({"chloectx": {}})).unwrap();
        assert_eq!(new.apps(), ["sheets", "chloectx"]);
        let p = plan(&none, &new, 1, 3);
        assert!(p["refused"].is_null(), "{p}");
        let steps = p["steps"].to_string();
        for want in ["the coordinators restart", "ChloeCtx starts beside the cluster", "downloads its embedding model", "Apps → ChloeCtx", "ctx_spaces"] {
            assert!(steps.contains(want), "{want}: {steps}");
        }
        assert_eq!(p["restarts"], false, "no server restarts for it");
        assert!(p["warnings"].as_array().unwrap().is_empty(), "{p}");
        let open: Spec = serde_json::from_value(json!({"demo": 10, "chloectx": {"embedder": "hash:64"}})).unwrap();
        let p = plan(&Spec { chloectx: None, ..open.clone() }, &open, 1, 3);
        assert!(!p["steps"].to_string().contains("downloads"), "{p}");
        assert!(p["warnings"].to_string().contains("ChloeCtx's pod may read and change"), "{p}");
        let steps = |a: &Spec, b: &Spec| plan(a, b, 1, 3)["steps"].to_string();
        let with = |patch: Json| patched(&serde_json::to_value(&new).unwrap(), &patch).unwrap().1;
        assert!(steps(&new, &with(json!({"chloectx": {"paused": true}}))).contains("ChloeCtx pauses"));
        assert!(steps(&new, &with(json!({"chloectx": {"embedder": "hash:256"}}))).contains("match by their words only"));
        assert!(steps(&new, &none).contains("ChloeCtx stops"));
        let bad: Spec = serde_json::from_value(json!({"demo": 10, "chloectx": {"embedder": "bert"}})).unwrap();
        assert!(bad.check().unwrap_err().contains("chloectx.embedder"));
        assert!(!serde_json::to_value(&none).unwrap().as_object().unwrap().contains_key("chloectx"), "a cluster without it keeps its version");
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
        // A new image on two copies a shard: a third joins each first.
        let two: Spec = serde_json::from_value(json!({"shards": 2, "replicas": 2, "demo": 100})).unwrap();
        let (_, new) = patched(&serde_json::to_value(&two).unwrap(), &json!({"image": "chloedb-server:v2"})).unwrap();
        let steps = plan(&two, &new, 2, 2)["steps"].to_string();
        assert!(steps.contains("each shard first takes a third copy with the new settings") && steps.contains("(4 restarts)"), "{steps}");
        let (_, off) = patched(&serde_json::to_value(&two).unwrap(), &json!({"image": "chloedb-server:v2", "rollout": {"extraCopies": false}})).unwrap();
        assert!(!plan(&two, &off, 2, 2)["steps"].to_string().contains("third copy"));
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
        let other = Spec { image: "chloedb-server:0.2.0".into(), ..spec.clone() };
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
        assert_eq!(Numbers::from(&c), Numbers { p99_ms: Some(12.5), error_ratio: Some(0.0), replica_behind: Some(3.0), disk_used_ratio: Some(0.6), busy: None });
        // How busy: the busiest shard's copies that answer, on average,
        // from each one's minute (or its last point over its cores).
        let busy = json!({"shards": [
            {"replicas": [{"status": "up", "detail": {"minute": {"busy": 0.9}}}, {"status": "up", "detail": {"minute": {"busy": 0.5}}}, {"status": "down", "detail": {"minute": {"busy": 1.0}}}]},
            {"replicas": [{"status": "up", "detail": {"load": {"cpu": 1.2}, "system": {"cores": 2}}}]}]});
        assert_eq!(Numbers::from(&busy).busy, Some(0.7));
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
        let b = Spec { shards: 4, image: "chloedb-server:0.2.0".into(), resources: Some(json!({"limits": {"cpu": "2", "memory": "4Gi"}})), token_secret: Some("t".into()), ..a.clone() };
        let h = versions(&h, &b, Some(3), Some("roberto"), 400, false);
        assert_eq!((&h[1]["version"], &h[1]["by"], &h[1]["restores"], &h[1]["readyMs"]), (&json!(2), &json!("roberto"), &Json::Null, &Json::Null));
        // Back to version 1: the merge patch that takes the spec there, its
        // rollout kept; the version it makes restores the first.
        let now = json!({"spec": serde_json::to_value(&paused).unwrap(), "status": {"history": h}});
        let now = json!({"spec": {"rollout": now["spec"]["rollout"], "shards": 4, "demo": 1, "image": "chloedb-server:0.2.0", "tokenSecret": "t", "resources": {"limits": {"cpu": "2", "memory": "4Gi"}}}, "status": now["status"]});
        let patch = kube::Link::rollback(&now, 1).unwrap();
        assert_eq!(patch, json!({"shards": 2, "image": "chloedb-server:dev", "tokenSecret": null, "resources": {"limits": {"memory": null}}, "replicas": 3, "coordinators": 1, "storage": {"size": "10Gi"}, "failoverAfterMs": 3000, "writeAcks": "all", "args": [], "coordinatorArgs": []}));
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
    fn secret_env_is_checked_and_restarts_only_when_set() {
        let bad = |e: Json| serde_json::from_value::<Spec>(json!({"demo": 1, "secretEnv": e})).unwrap().check().unwrap_err();
        assert!(bad(json!([{"name": "1KEY", "secret": "s"}])).contains("not an environment variable's name"));
        assert!(bad(json!([{"name": "A-KEY", "secret": "s"}])).contains("not an environment variable's name"));
        assert!(bad(json!([{"name": "", "secret": "s"}])).contains("not an environment variable's name"));
        assert!(bad(json!([{"name": "STRATA_TOKEN", "secret": "s"}])).contains("the operator's to set"));
        assert!(bad(json!([{"name": "K", "secret": "a"}, {"name": "K", "secret": "b"}])).contains("twice"));
        assert!(bad(json!([{"name": "K", "secret": ""}])).contains("needs a secret"));
        assert!(bad(json!([{"name": "K", "secret": "s", "key": ""}])).contains("needs a secret"));
        let with: Spec = serde_json::from_value(json!({"demo": 1, "secretEnv": [{"name": "ANTHROPIC_API_KEY", "secret": "anthropic"}]})).unwrap();
        with.check().unwrap();
        assert_eq!(with.secret_env[0].key, "key");
        // A cluster without it keeps the hashes and version it had before
        // the setting existed: a new operator restarts none of its copies.
        let without = Spec { secret_env: Vec::new(), ..with.clone() };
        let before = |s: &Spec| {
            let server = json!({
                "image": s.image, "imagePullPolicy": s.image_pull_policy, "args": s.args, "resources": s.resources,
                "failoverAfterMs": s.failover_after_ms, "writeAcks": s.write_acks,
                "tokenSecret": s.token_secret, "clusterTokenSecret": s.cluster_token_secret,
            });
            let coordinator = json!({
                "image": s.image, "imagePullPolicy": s.image_pull_policy, "resources": s.coordinator_resources,
                "args": s.coordinator_args, "tokenSecret": s.token_secret, "clusterTokenSecret": s.cluster_token_secret,
            });
            (short_hash(&server), short_hash(&coordinator))
        };
        assert_eq!((without.server_config(), without.coordinator_config()), before(&without));
        assert!(serde_json::to_value(&without).unwrap().get("secretEnv").is_none());
        // Setting it (or changing it) is a new version that restarts them.
        assert_ne!(with.server_config(), without.server_config());
        assert_ne!(with.coordinator_config(), without.coordinator_config());
        assert_ne!(with.version(), without.version());
        let other = Spec { secret_env: vec![SecretEnv { key: "other".into(), ..with.secret_env[0].clone() }], ..with.clone() };
        assert_ne!(other.server_config(), with.server_config());
    }

    #[test]
    fn the_crd_file_is_the_one_the_operator_installs() {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/deploy/k8s/operator/crd.json");
        let want = serde_json::to_string_pretty(&crd()).unwrap() + "\n";
        if std::env::var_os("CHLOEDB_WRITE_CRD").or_else(|| std::env::var_os("STRATA_WRITE_CRD")).is_some() {
            std::fs::write(file, &want).unwrap();
        }
        assert_eq!(std::fs::read_to_string(file).unwrap_or_default(), want, "run with CHLOEDB_WRITE_CRD=1 to write {file}");
    }
}
