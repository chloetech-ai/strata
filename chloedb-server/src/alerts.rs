//! Alert rules over the series ([`crate::telemetry::Series`]) and, on a
//! coordinator, over the cluster's state.
//!
//! A rule names a value of each point (`p99_ms`, `error_ratio`,
//! `disk_used_ratio`, `replica_behind_max` ...), how the points of a
//! window make one value (`eval`: their `mean`, `max`, `min`, the `rate`
//! of a count, or `all`, the value that every point must pass), a
//! comparison and a threshold, how long the condition must hold before the
//! alert fires (`for_s`), and the threshold that resolves it (`resolve`,
//! by default the threshold itself). A window without any value of the
//! metric is no data: it keeps the alert as it was (a firing alert stays
//! firing) and never resolves it.
//!
//! A rule's `scope` is `self` (this server's points) or `each_copy`: every
//! copy of every shard evaluates it on its own points and reports its
//! states in its detail (`detail.alerts`, which `/v1/cluster` carries), and
//! a coordinator gathers them into one state per rule with `where`, the
//! copies that are not ok. A coordinator also has built-in rules over the
//! cluster's state ([`builtins`]): a shard without a majority, a copy down,
//! a shard under-replicated, a primary that changed, partial answers, a
//! copy that stopped reporting, and the availability of searches.
//!
//! Each change of an alert (firing, resolved, acknowledged, silenced, a
//! note) is an event, kept in memory, in the log store's `alerts` table
//! for 7 days, and in the server's events; firing and resolved ones are
//! posted to the routes ([`Route`]) that take their severity, unless the
//! alert is silenced. Rules and routes are kept in `--alerts FILE` (JSON),
//! or in memory.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::telemetry::now_ms;
use crate::ApiError;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub name: String,
    /// The point's value it watches.
    pub metric: String,
    /// `>`, `>=`, `<` or `<=`.
    pub op: String,
    pub threshold: f64,
    /// How long the condition must hold before the alert fires, in
    /// seconds (on top of the window).
    #[serde(default)]
    pub for_s: u64,
    /// `critical`, `warning` or `info`.
    #[serde(default = "warning")]
    pub severity: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// How a window's points make one value: `mean`, `max`, `min`, `rate`
    /// (the change per second of a count) or `all` (every point must meet
    /// the condition: the least extreme value).
    #[serde(default = "mean")]
    pub eval: String,
    /// The window, in seconds.
    #[serde(default = "five_minutes")]
    pub window_s: u64,
    /// The threshold a firing alert resolves at (by default `threshold`):
    /// with `>` it resolves once the value is at most this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolve: Option<f64>,
    /// `self` (this server's points) or `each_copy` (every copy's own).
    #[serde(default = "self_scope")]
    pub scope: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    /// A URL that says what to do.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub runbook: String,
    /// The route its notifications go to (empty: every route that takes
    /// its severity).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub route: String,
    /// A coordinator's rule over the cluster's state ([`builtins`]): only
    /// its severity, `enabled` and route change.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub builtin: bool,
    /// `before`: the value is the window's against the window before it
    /// (now / before; a p99 that doubled is 2), not the window's own.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vs: String,
    /// A query whose answer's first number is the value watched, run
    /// every [`SQL_EVERY_MS`] ([`refresh_sql`]) with `:since` and `:now`,
    /// the window's start and end as timestamps: the rule's metric is then
    /// `sql_<id>`, which the series' points carry. A `SELECT` of the
    /// cluster's tables (`SELECT COUNT(*) FROM otel_logs WHERE severity =
    /// 'ERROR' AND ts > :since`), so that a rule watches the data as well
    /// as the server.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sql: String,
}

fn warning() -> String {
    "warning".into()
}

fn yes() -> bool {
    true
}

fn mean() -> String {
    "mean".into()
}

fn five_minutes() -> u64 {
    300
}

fn self_scope() -> String {
    "self".into()
}

/// The longest window a rule takes: the series holds an hour.
pub const MAX_WINDOW_S: u64 = 3600;

impl Rule {
    fn check(&self) -> Result<(), ApiError> {
        if self.id.is_empty() || self.id.len() > 64 || !self.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(ApiError::bad(format!("a rule's id is 1 to 64 letters, digits, '-' or '_', not {:?}", self.id)));
        }
        if self.name.trim().is_empty() {
            return Err(ApiError::bad(format!("rule {} needs a name", self.id)));
        }
        if !matches!(self.op.as_str(), ">" | ">=" | "<" | "<=") {
            return Err(ApiError::bad(format!("rule {}: op is >, >=, < or <=, not {:?}", self.id, self.op)));
        }
        if !matches!(self.severity.as_str(), "critical" | "warning" | "info") {
            return Err(ApiError::bad(format!("rule {}: severity is critical, warning or info, not {:?}", self.id, self.severity)));
        }
        if !matches!(self.eval.as_str(), "mean" | "max" | "min" | "rate" | "all") {
            return Err(ApiError::bad(format!("rule {}: eval is mean, max, min, rate or all, not {:?}", self.id, self.eval)));
        }
        if !matches!(self.vs.as_str(), "" | "before") {
            return Err(ApiError::bad(format!("rule {}: vs is \"before\" (the window before) or left out, not {:?}", self.id, self.vs)));
        }
        if !matches!(self.scope.as_str(), "self" | "each_copy") {
            return Err(ApiError::bad(format!("rule {}: scope is self or each_copy, not {:?}", self.id, self.scope)));
        }
        if self.window_s == 0 || self.window_s > MAX_WINDOW_S {
            return Err(ApiError::bad(format!("rule {}: window_s is 1 to {MAX_WINDOW_S} seconds", self.id)));
        }
        if !self.threshold.is_finite() || self.resolve.is_some_and(|r| !r.is_finite()) {
            return Err(ApiError::bad(format!("rule {}: the threshold and resolve are numbers", self.id)));
        }
        if let Some(r) = self.resolve {
            let above = self.op.starts_with('>');
            if (above && r > self.threshold) || (!above && r < self.threshold) {
                return Err(ApiError::bad(format!("rule {}: it fires at {} {} and so resolves at {} the threshold or less extreme, not {r}", self.id, self.op, self.threshold, if above { "at most" } else { "at least" })));
            }
        }
        if !self.runbook.is_empty() && !(self.runbook.starts_with("https://") || self.runbook.starts_with("http://")) {
            return Err(ApiError::bad(format!("rule {}: the runbook is an http(s) URL", self.id)));
        }
        if !self.sql.is_empty() {
            let q = self.sql.trim().trim_end_matches(';').trim();
            let first = q.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            if !matches!(first.as_str(), "select" | "with") || q.contains(';') {
                return Err(ApiError::bad(format!("rule {}: sql is one SELECT, whose first number is the value watched", self.id)));
            }
            if q.len() > MAX_SQL {
                return Err(ApiError::bad(format!("rule {}: sql is at most {MAX_SQL} bytes", self.id)));
            }
            if self.metric != sql_metric(&self.id) {
                return Err(ApiError::bad(format!("rule {}: a rule with sql watches the metric {:?}", self.id, sql_metric(&self.id))));
            }
            if self.scope != "self" {
                return Err(ApiError::bad(format!("rule {}: a rule with sql runs its query here (scope self)", self.id)));
            }
        }
        Ok(())
    }

    /// Whether `v` meets the condition.
    pub fn holds(&self, v: f64) -> bool {
        cmp(&self.op, v, self.threshold)
    }

    /// Whether a firing alert with value `v` still fires: `v` is past the
    /// resolve threshold.
    pub fn still(&self, v: f64) -> bool {
        cmp(&self.op, v, self.resolve.unwrap_or(self.threshold))
    }

    /// What decides when it fires and resolves; a change of it starts the
    /// alert again.
    fn condition(&self) -> (String, String, String, u64, u64, String, String, bool) {
        let n = |x: f64| format!("{x}");
        (self.metric.clone(), self.op.clone(), n(self.threshold), self.for_s, self.window_s, self.eval.clone(), self.resolve.map(n).unwrap_or_default() + &self.scope + &self.vs + &self.sql, self.enabled)
    }

    /// A short digest of [`Self::condition`]: a coordinator tells a copy
    /// that evaluates another version of the rule by it.
    pub fn sig(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let c = self.condition();
        (c.0, c.1, c.2, c.3, c.4, c.5, c.6).hash(&mut h);
        format!("{:08x}", h.finish() as u32)
    }

    fn copies(&self) -> bool {
        self.scope == "each_copy"
    }
}

fn cmp(op: &str, v: f64, t: f64) -> bool {
    match op {
        ">" => v > t,
        ">=" => v >= t,
        "<" => v < t,
        _ => v <= t,
    }
}

#[allow(clippy::too_many_arguments)]
fn rule(id: &str, name: &str, metric: &str, op: &str, threshold: f64, eval: &str, window_s: u64, for_s: u64, severity: &str, scope: &str, description: &str) -> Rule {
    Rule {
        id: id.into(),
        name: name.into(),
        metric: metric.into(),
        op: op.into(),
        threshold,
        for_s,
        severity: severity.into(),
        enabled: true,
        description: description.into(),
        eval: eval.into(),
        window_s,
        resolve: None,
        scope: scope.into(),
        owner: String::new(),
        runbook: String::new(),
        route: String::new(),
        builtin: false,
        vs: String::new(),
        sql: String::new(),
    }
}

/// The rules a server starts with, each with a resolve threshold less
/// extreme than the one it fires at, so that a value around it does not
/// flap. Those over a copy's own gauges (its disk, writes, merges,
/// replicas) are every copy's: a coordinator has none of those gauges, and
/// gathers its copies' states.
pub fn defaults() -> Vec<Rule> {
    let c = "each_copy";
    let s = "self";
    #[rustfmt::skip]
    let v = [
        (rule("failing", "Database refuses writes", "failing", ">", 0.0, "max", 30, 0, "critical", c, "A write failed partway (a full disk, an I/O error), and the database refuses writes until it is reopened. On a primary, hand the primary over to a replica in sync first (Cluster: its menu, Make it the primary), so writes go on; then fix the cause and restart the server."), 0.0),
        (rule("flush-failing", "Flushes fail", "flush_failing", ">", 0.0, "max", 30, 0, "critical", c, "Writes still commit to the log until the memtable holds twice its limit. Usually a full disk."), 0.0),
        (rule("merge-failing", "Merges fail", "merge_failing", ">", 0.0, "all", 60, 0, "warning", c, "Segments pile up and queries slow down until merges succeed again."), 0.0),
        (rule("damaged", "Data damaged on disk", "damaged_segments", ">", 0.0, "max", 30, 0, "critical", c, "A scrub found segments that fail their checksums on this copy: searches that read them fail there, and it must not become the primary. Replace the copy (Cluster: Plan the repair), or restore it from a checkpoint."), 0.0),
        (rule("disk-full", "Disk almost full", "disk_used_ratio", ">", 0.9, "mean", 300, 0, "warning", c, "Under 10% of the copy's disk is left."), 0.88),
        (rule("disk-critical", "Disk nearly full", "disk_used_ratio", ">", 0.97, "mean", 120, 0, "critical", c, "Under 3% of the copy's disk is left: writes fail when it is full. Free space or move slots off the copy now."), 0.95),
        (rule("disk-filling", "Disk full within 12 hours", "disk_hours_to_full", "<", 12.0, "mean", 1800, 0, "warning", c, "At the rate the copy's disk filled over the last 30 minutes, it is full in under 12 hours."), 24.0),
        (rule("p99", "", "p99_ms", ">", 0.0, "mean", 300, 0, "warning", s, ""), 0.0),
        (rule("p99-jump", "Query p99 doubled", "p99_ms", ">", 2.0, "mean", 300, 0, "warning", s, "The p99 of the last 5 minutes is over twice the p99 of the 5 minutes before, and over half the p99 objective (--slo-p99-ms), 20 ms at least: a lower p99 is not judged."), 1.5),
        (rule("errors", "", "error_ratio", ">", 0.0, "mean", 300, 0, "warning", s, ""), 0.0),
        (rule("key-failing", "A client's calls fail", "key_error_ratio", ">", 0.2, "mean", 300, 0, "warning", s, "Over 20% of one key's calls failed in 5 minutes (its own errors included: bad SQL, its own deadline), for a key with 30 calls or more. Tell the key's owner."), 0.1),
        (rule("key-flood", "A client crowds out the others", "key_busy_ratio", ">", 1.0, "mean", 60, 0, "warning", s, "Over a minute, one key's queries took over half of this server's query time, and ran more at once than the server has cores, while other keys queried too: their queries wait. Queries, Running now, grouped by key, shows its queries; Contain a key limits it. Tell the key's owner."), 0.5),
        (rule("rejected", "Queries refused (429)", "rejected_s", ">", 0.5, "mean", 300, 0, "warning", s, "Queries found no slot (--max-concurrent) or no memory before their deadline, or a Contain rule refused them."), 0.2),
        (rule("5xx", "Server errors over 1% of requests", "http_5xx_ratio", ">", 0.01, "mean", 300, 0, "warning", s, "Over 1% of the HTTP requests of 5 minutes failed on the server's side (5xx), judged over 100 requests or more, or 10 failures or more: one failure of a few requests is not a server failing."), 0.005),
        (rule("replica-behind", "A replica is behind", "replica_behind_s", ">", 30.0, "mean", 300, 0, "warning", c, "A replica has lacked a transaction its primary committed for over 30 seconds, on average over 5 minutes. A replica being sent a whole copy, or not heard from since the primary started, is not counted: A replica does not serve reads and A copy is down say those."), 10.0),
        (rule("out-of-sync", "A replica is out of sync", "replicas_out_of_sync", ">", 0.0, "all", 60, 0, "critical", c, "A replica does not hold what its primary committed: a failover now could lose writes."), 0.0),
        (rule("replica-reads", "A replica does not serve reads", "serves_reads", "<", 1.0, "all", 60, 0, "warning", c, "The replica is catching up with its primary, or lost it."), 1.0),
        (rule("query-memory", "Query memory almost spent", "query_memory_ratio", ">", 0.9, "mean", 60, 0, "warning", s, "The running queries hold over 90% of --query-memory-mb; more are refused with 429."), 0.8),
        (rule("write-queue", "Writes queue up", "write_queue", ">", 512.0, "mean", 60, 0, "warning", c, "Write requests wait for the writer; at --max-queued-writes they are refused."), 256.0),
        (rule("write-p99", "Writes are slow", "write_p99_ms", ">", 500.0, "mean", 60, 0, "warning", s, "The p99 of writes over a minute is over 500 ms: a replica or the link to it is slow (with --write-acks all a write waits for every copy), or the disk is."), 250.0),
        (rule("follow-behind", "A follower is behind its source", "follow_lag_s", ">", 60.0, "min", 60, 0, "warning", s, "This cluster follows another, and its follow process has been over a minute behind for a minute: it is down, catching up, or held (GET /v1/admin/follow says which)."), 30.0),
    ];
    let slo = crate::service::Slo::default();
    v.into_iter()
        .map(|(mut r, resolve)| {
            r.resolve = Some(resolve);
            if r.id == "p99-jump" {
                r.vs = "before".into();
            }
            follow(&mut r, slo.error_ratio, slo.p99_ms);
            r
        })
        .collect()
}

/// A coordinator's rules over its cluster's state, besides its defaults.
/// Their metrics are the coordinator's points (`partial_s`,
/// `availability`) or its view of the cluster ([`ClusterView`]).
pub fn builtins() -> Vec<Rule> {
    let s = "self";
    #[rustfmt::skip]
    let v = [
        (rule("shard-no-majority", "A shard has no majority", "shards_no_majority", ">", 0.0, "max", 10, 0, "critical", s, "Fewer than half of a shard's copies answer: with failover its primary serves no reads, so every search fails. Bring a copy back, or remove the dead one."), 0.0),
        (rule("copy-down", "A copy is down", "copies_down", ">", 0.0, "max", 10, 30, "warning", s, "A copy of a shard has not answered the coordinator for 30 seconds."), 0.0),
        (rule("under-replicated", "A shard is under-replicated", "shards_under_replicated", ">", 0.0, "max", 10, 60, "warning", s, "A shard has fewer copies in sync and serving than it has copies, for a minute."), 0.0),
        (rule("primary-changed", "A shard's primary changed", "primary_changes", ">", 0.0, "max", 10, 0, "info", s, "A shard elected another primary in the last 10 minutes; the alert resolves 10 minutes after the last change."), 0.0),
        (rule("partial-answers", "Partial answers", "partial_s", ">", 0.1, "mean", 120, 0, "warning", s, "Over 2 minutes, more than one answer in 10 seconds left out shards that did not answer."), 0.02),
        (rule("copy-silent", "A copy stopped reporting", "copies_silent", ">", 0.0, "max", 10, 0, "warning", s, "A copy answers but its own numbers are over 60 seconds old."), 0.0),
        (rule("availability", "Searches fail", "availability", "<", 0.0, "mean", 300, 0, "critical", s, ""), 0.0),
    ];
    let slo = crate::service::Slo::default();
    v.into_iter()
        .map(|(mut r, resolve)| {
            r.builtin = true;
            r.resolve = Some(resolve);
            follow(&mut r, slo.error_ratio, slo.p99_ms);
            r
        })
        .collect()
}

/// `x` as a percent with the digits it takes: 0.01 is "1%", 0.005 "0.5%".
fn pct(x: f64) -> String {
    let t = format!("{:.3}", x * 100.0);
    format!("{}%", t.trim_end_matches('0').trim_end_matches('.'))
}

/// Of the default and built-in rules, those that follow the service
/// level objective (`--slo-error-ratio`, `--slo-p99-ms`), as Home's
/// checks do (OV-8): query errors over it, the p99 over it, searches that succeed under
/// what it leaves. Each resolves at half its margin.
fn follow(r: &mut Rule, error_ratio: f64, p99_ms: f64) {
    let ms = |v: f64| if v >= 1000.0 && v % 1000.0 == 0.0 { format!("{} s", v / 1000.0) } else { format!("{v} ms") };
    match r.id.as_str() {
        "errors" => {
            (r.threshold, r.resolve) = (error_ratio, Some(error_ratio / 2.0));
            r.name = format!("Query errors over {}", pct(error_ratio));
            r.description = format!("Over {} of the queries that ended in 5 minutes failed on the server's side (errors and the server's deadline; not bad SQL nor a client's own deadline), judged over 100 queries or more, or 10 failures or more. {} is the service level objective (--slo-error-ratio), which Home checks too.", pct(error_ratio), pct(error_ratio));
        }
        "p99" => {
            (r.threshold, r.resolve) = (p99_ms, Some(p99_ms * 0.8));
            r.name = format!("Query p99 over {}", ms(p99_ms));
            r.description = format!("The slowest 1% of queries took over {}, on average over 5 minutes. {} is the service level objective (--slo-p99-ms), which Home checks too.", ms(p99_ms), ms(p99_ms));
        }
        "availability" => {
            (r.threshold, r.resolve) = (1.0 - error_ratio, Some(1.0 - error_ratio / 2.0));
            r.description = format!("Under {} of the searches that ended in 5 minutes succeeded (failures on the server's side): over {} failed, the service level objective (--slo-error-ratio). Judged over 100 searches or more, or 10 failures or more.", pct(1.0 - error_ratio), pct(error_ratio));
        }
        _ => {}
    }
}

/// Metrics a coordinator computes from its cluster's state.
pub const CLUSTER_METRICS: [&str; 5] = ["shards_no_majority", "copies_down", "shards_under_replicated", "primary_changes", "copies_silent"];

/// Built-in rules whose alert explains others ([`Alerts::hold`]).
pub const CAUSES: [&str; 2] = ["shard-no-majority", "copy-down"];
/// Metrics of a shard's replicas, which a copy of it that is down explains.
pub const REPLICA_METRICS: [&str; 5] = ["replicas_out_of_sync", "replica_behind_max", "replica_behind_s", "serves_reads", "shards_under_replicated"];

/// The metrics a server reports in one role only: a primary its replicas
/// out of sync and behind, a replica whether it serves reads. A server
/// whose role changed stops reporting one, and one elected a moment ago
/// has not reported it for long ([`role_key`]).
const ROLE_METRICS: [&str; 4] = ["replicas_out_of_sync", "replica_behind_max", "replica_behind_s", "serves_reads"];

/// The point key a role metric is read from (`replica_behind_s` is made of
/// `replica_behind_max`).
fn role_key(metric: &str) -> &str {
    if metric == "replica_behind_s" {
        "replica_behind_max"
    } else {
        metric
    }
}
/// Metrics of the searches a coordinator serves, which a shard's trouble
/// explains.
pub const TRAFFIC_METRICS: [&str; 15] = ["availability", "error_ratio", "http_5xx_ratio", "http_5xx_s", "errors_s", "timeouts_s", "p99_ms", "p95_ms", "p50_ms", "partial_s", "rejected_s", "qps", "slow_s", "key_error_ratio", "key_busy_ratio"];

fn sev_rank(s: &str) -> u8 {
    match s {
        "critical" => 0,
        "warning" => 1,
        _ => 2,
    }
}

/// `s` as one segment of a URL's path.
fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A coordinator's name in a message: its host and port.
fn name_of(url: &str) -> String {
    let u = url.split("://").nth(1).unwrap_or(url);
    if u.is_empty() {
        "this server".into()
    } else {
        u.trim_end_matches('/').to_string()
    }
}

/// A time of day in UTC, as messages say it: `14:05:09 UTC`.
fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02} UTC", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

/// An acknowledgement: someone is on it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    pub by: String,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Notifications held back until `until_ms`, set at `at_ms`. A silence
/// lifted is kept as one that ended when it was lifted, so that the
/// newest word wins between coordinators ([`Alerts::merge`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Silence {
    pub by: String,
    pub until_ms: u64,
    pub reason: String,
    #[serde(default)]
    pub at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub by: String,
    pub at_ms: u64,
    pub text: String,
}

/// Who sent an alert's FIRING message (a coordinator), when, and the key
/// its messages carry ([`Alerts::dedup_key`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Paged {
    pub by: String,
    pub at_ms: u64,
    #[serde(default)]
    pub key: String,
}

/// Notes kept per rule.
pub const NOTES: usize = 100;

/// A rule's state now.
#[derive(Clone, Debug)]
struct St {
    /// `ok`, `pending`, `firing` or `no_data`.
    state: &'static str,
    since_ms: u64,
    value: Option<f64>,
    /// Since when the window holds no value, if it holds none now.
    no_data_since_ms: Option<u64>,
    ack: Option<Ack>,
    silenced: Option<Silence>,
    notes: Vec<Note>,
    /// What is not ok: copies, shards.
    where_: Vec<Json>,
    /// An each_copy rule's copies, by state.
    copies: Option<Json>,
    /// Whether this server's points carry the rule's metric at all (a
    /// replica has no `replicas_out_of_sync`, a primary no
    /// `serves_reads`): a coordinator does not count a copy that does not.
    reports: bool,
    /// While the series spans less than the window (a server just
    /// started): how long it spans (ms). The window is evaluated over the
    /// points there are.
    partial_ms: Option<u64>,
    /// Who sent this episode's FIRING message.
    paged: Option<Paged>,
    /// When its last episode ended: what a peer says of an earlier one
    /// (an acknowledgement, a page) is not this one's.
    cleared_ms: u64,
    /// The alert that holds its messages back now, and why ([`hold`]).
    held: Option<(String, String)>,
    /// Its rule was edited, by whom and when: a resolution soon after says
    /// so ([`EDIT_SAYS_MS`]).
    edited: Option<(String, u64)>,
    /// The page of the episode that just ended (for its resolution).
    paged_before: Option<Paged>,
    /// Another coordinator's page heard while this one did not see the
    /// alert fire yet: it is this episode's if it fires within
    /// [`HINT_MS`].
    paged_hint: Option<Paged>,
    /// Why its FIRING message goes late, once it does ([`Alerts::due`]):
    /// the cause that held it back, the silence that ended.
    late: Option<String>,
    /// Until when it stays held after its cause resolved: its window,
    /// which still holds the outage, passes first ([`Alerts::hold`]).
    settle_ms: Option<u64>,
    /// Since when it could be sent here (firing, neither held nor
    /// silenced): another coordinator waits [`PAGER_WAIT_MS`] from then.
    due_ms: Option<u64>,
}

/// How long after an edit a resolution is said to be the edit's.
const EDIT_SAYS_MS: u64 = 120_000;
/// How long a page heard from another coordinator before this one saw the
/// alert fire counts as this episode's.
const HINT_MS: u64 = 30_000;
/// How long a coordinator that does not send messages waits for the one
/// that does to say it sent an alert's, before it sends it itself.
pub const PAGER_WAIT_MS: u64 = 60_000;
/// A coordinator not heard for this long does not send messages.
pub const PEER_LIVE_MS: u64 = 45_000;
/// A coordinator that does not send messages sends an alert's at once when
/// the one that does said, this long after the alert fired here, that it
/// does not see it: a rule over each coordinator's own traffic fires on
/// one alone (R2-scenarios-1). The margin covers two coordinators judging
/// the same cluster a few seconds apart.
const NOT_SEEN_MS: u64 = 15_000;

/// What another coordinator said of one alert ([`Alerts::merge`]).
#[derive(Clone, Debug)]
struct PeerSt {
    state: String,
    since_ms: u64,
    value: Option<f64>,
    held_by: Option<String>,
    ack: Option<Json>,
}
/// A copy not heard from for this long has no data for its each_copy
/// rules: what it last said is not what it holds now ("A copy is down"
/// says why).
pub const UNHEARD_MS: u64 = 60_000;
/// A coordinator restarted (its state file was there) sends no FIRING
/// message for this long: it first hears the other coordinators, which
/// may have sent it while it was down.
pub const STARTUP_MS: u64 = 20_000;
/// How soon after a coordinator starts its first surveys are done: an
/// alert that fired by then was already the case when it started.
const FIRST_SURVEY_MS: u64 = 15_000;

impl St {
    fn new(t: u64) -> St {
        St { state: "no_data", since_ms: t, value: None, no_data_since_ms: Some(t), ack: None, silenced: None, notes: Vec::new(), where_: Vec::new(), copies: None, reports: true, partial_ms: None, paged: None, cleared_ms: 0, held: None, edited: None, paged_before: None, paged_hint: None, late: None, settle_ms: None, due_ms: None }
    }

    /// The episode ended at `t`: its acknowledgement and page go with it.
    fn clear(&mut self, t: u64) {
        self.paged_before = self.paged.take();
        (self.ack, self.held, self.paged_hint, self.late, self.cleared_ms) = (None, None, None, None, t);
    }

    /// It fires from `t`: a page another coordinator sent just before is
    /// this episode's.
    fn fired(&mut self, t: u64) {
        if let Some(h) = self.paged_hint.take().filter(|h| h.at_ms + HINT_MS >= t) {
            self.paged.get_or_insert(h);
        }
    }

    /// Takes the window's value `v` at `t`: returns "firing" or
    /// "resolved" when the alert changes so.
    fn step(&mut self, r: &Rule, v: Option<f64>, t: u64) -> Option<&'static str> {
        let Some(v) = v else {
            // No data keeps the state; it never resolves.
            self.no_data_since_ms.get_or_insert(t);
            if self.state == "ok" {
                (self.state, self.since_ms) = ("no_data", t);
            }
            return None;
        };
        self.no_data_since_ms = None;
        self.value = Some(v);
        let was = self.state;
        let next = match was {
            "firing" if r.still(v) => "firing",
            "firing" => "ok",
            "pending" if r.holds(v) && t.saturating_sub(self.since_ms) >= r.for_s * 1000 => "firing",
            "pending" if r.holds(v) => "pending",
            _ if r.holds(v) && r.for_s == 0 => "firing",
            _ if r.holds(v) => "pending",
            _ => "ok",
        };
        if next != was {
            (self.state, self.since_ms) = (next, t);
        }
        match (was, next) {
            ("firing", "firing") => None,
            (_, "firing") => {
                self.fired(t);
                Some("firing")
            }
            ("firing", _) => {
                self.clear(t);
                Some("resolved")
            }
            _ => None,
        }
    }

    /// Its server no longer reports the rule's metric, as its role changed
    /// (a primary that stepped down reports no replicas out of sync):
    /// nothing of it is left here, so it is ok, and resolves if it fired.
    fn gone(&mut self, t: u64) -> Option<&'static str> {
        self.no_data_since_ms = None;
        let was = self.state;
        if was != "ok" {
            (self.state, self.since_ms) = ("ok", t);
        }
        (was == "firing").then(|| {
            self.clear(t);
            "resolved"
        })
    }

    /// What survives a restart ([`Saved`]).
    fn saved(&self) -> SavedSt {
        SavedSt { state: self.state.to_string(), since_ms: self.since_ms, value: self.value, ack: self.ack.clone(), silenced: self.silenced.clone(), notes: self.notes.clone(), paged: self.paged.clone(), cleared_ms: self.cleared_ms, where_: self.where_.iter().take(50).cloned().collect(), copies: self.copies.clone() }
    }

    fn restored(k: SavedSt, t: u64) -> St {
        let state = match k.state.as_str() {
            "firing" => "firing",
            "pending" => "pending",
            "ok" => "ok",
            _ => "no_data",
        };
        St { state, since_ms: k.since_ms, value: k.value, no_data_since_ms: (state == "no_data").then_some(t), ack: k.ack, silenced: k.silenced, notes: k.notes, where_: k.where_, copies: k.copies, reports: true, partial_ms: None, paged: k.paged, cleared_ms: k.cleared_ms, held: None, edited: None, paged_before: None, paged_hint: None, late: None, settle_ms: None, due_ms: None }
    }
}

/// A rule's state as the state file keeps it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SavedSt {
    state: String,
    since_ms: u64,
    #[serde(default)]
    value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ack: Option<Ack>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    silenced: Option<Silence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    notes: Vec<Note>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    paged: Option<Paged>,
    #[serde(default)]
    cleared_ms: u64,
    #[serde(default, rename = "where", skip_serializing_if = "Vec::is_empty")]
    where_: Vec<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    copies: Option<Json>,
}

/// The state file (`<rules file>.state.json`): what fires, what was sent,
/// acknowledgements, silences and notes, and each shard's primary, so that
/// a restarted server (a rolling upgrade restarts every one) neither
/// forgets them nor sends again what it sent.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    saved_ms: u64,
    states: HashMap<String, SavedSt>,
    #[serde(default)]
    primaries: HashMap<String, String>,
    #[serde(default)]
    changes: Vec<(u64, Json)>,
}

fn state_rank(s: &str) -> u8 {
    match s {
        "firing" => 0,
        "pending" => 1,
        "no_data" => 2,
        "ok" => 3,
        _ => 4,
    }
}

/// A change of an alert: `firing`, `resolved`, `acked`, `silenced`,
/// `unsilenced` or `noted`.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub ts_ms: u64,
    pub rule: String,
    pub name: String,
    pub severity: String,
    pub state: &'static str,
    pub value: Option<f64>,
    pub threshold: f64,
    pub op: String,
    pub metric: String,
    pub eval: String,
    pub window_s: u64,
    /// Short names of what is not ok (copies, shards).
    #[serde(rename = "where", skip_serializing_if = "Vec::is_empty")]
    pub where_: Vec<String>,
    /// What is not ok, as the state's `where` has it (20 at most).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub place: Vec<Json>,
    /// Why it resolved without its value: "the rule was edited".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// A note's text, a silence's reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// When a resolved alert fired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fired_ms: Option<u64>,
    /// Silenced: not posted to the routes.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub silenced: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub runbook: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub owner: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub route: String,
    /// The alert that held its message back ([`hold`]), and why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_why: Option<String>,
    /// The coordinator that sent its FIRING message (of a resolution: the
    /// episode's), if one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paged_by: Option<String>,
    /// Copies that run another version of its rule (each_copy rules).
    #[serde(skip_serializing_if = "is_zero")]
    pub rule_differs: u64,
    /// Said after the message: why it is sent now and not when it fired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// When its episode began (it fired), for the key that names it in
    /// every message ([`Alerts::dedup_key`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_ms: Option<u64>,
    /// The key that names its episode in every message of it, the one the
    /// coordinator that sent its FIRING message chose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedup_key: Option<String>,
    /// The rule's `vs`: `before` when the value is a ratio to the window
    /// before, not the metric's own.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub vs: String,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Event {
    fn of(r: &Rule, state: &'static str, st: &St, t: u64) -> Event {
        // A firing (or resolved) alert names where it fires, not the copies
        // without data beside it.
        let shown: Vec<&Json> = st.where_.iter().filter(|w| !matches!(state, "firing" | "resolved") || w["state"].as_str().is_none_or(|s| s == "firing")).collect();
        Event {
            ts_ms: t,
            rule: r.id.clone(),
            name: r.name.clone(),
            severity: r.severity.clone(),
            state,
            value: st.value,
            threshold: if state == "resolved" { r.resolve.unwrap_or(r.threshold) } else { r.threshold },
            op: r.op.clone(),
            metric: r.metric.clone(),
            eval: r.eval.clone(),
            window_s: r.window_s,
            where_: shown.iter().filter_map(|w| w["short"].as_str().or_else(|| w["name"].as_str()).map(String::from)).collect(),
            place: shown.iter().take(20).map(|w| (*w).clone()).collect(),
            why: None,
            by: None,
            text: None,
            fired_ms: None,
            silenced: st.silenced.as_ref().is_some_and(|s| s.until_ms > now_ms()),
            description: r.description.clone(),
            runbook: r.runbook.clone(),
            owner: r.owner.clone(),
            route: r.route.clone(),
            held_by: st.held.as_ref().map(|h| h.0.clone()),
            held_why: st.held.as_ref().map(|h| h.1.clone()),
            paged_by: if state == "resolved" { st.paged.as_ref().or(st.paged_before.as_ref()).map(|p| p.by.clone()) } else { st.paged.as_ref().map(|p| p.by.clone()) },
            rule_differs: st.copies.as_ref().and_then(|c| c["rule_differs"].as_u64()).unwrap_or(0),
            context: None,
            episode_ms: match state {
                "firing" => Some(t),
                "resolved" => None,
                _ => (st.state == "firing").then_some(st.since_ms),
            },
            dedup_key: (if state == "resolved" { st.paged.as_ref().or(st.paged_before.as_ref()) } else { st.paged.as_ref() }).map(|p| p.key.clone()).filter(|k| !k.is_empty()),
            vs: r.vs.clone(),
        }
    }

    /// Whether it is posted to routes: firing and resolved, unless
    /// silenced.
    pub fn notifies(&self) -> bool {
        matches!(self.state, "firing" | "resolved") && !self.silenced
    }
}

/// Events kept in memory.
pub const EVENTS: usize = 2000;

/// Where notifications go, for the severities listed (all when empty):
/// a URL taking `{"text", "alert"}` (Slack's incoming webhooks take it as
/// is), or PagerDuty's or Opsgenie's API with their own shape and a key
/// that names the alert, so that its resolution closes the incident its
/// firing opened.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub severities: Vec<String>,
    /// `webhook` (the default), `pagerduty` (Events API v2) or `opsgenie`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// PagerDuty's routing key or Opsgenie's API key: a secret, kept as
    /// the URL is and never shown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// Minutes after which a critical alert that nobody acknowledged is
    /// sent again, until it is (0: never).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub repeat_min: u64,
}

impl Route {
    fn takes(&self, severity: &str) -> bool {
        self.severities.is_empty() || self.severities.iter().any(|s| s == severity)
    }

    /// The URL with its path left out: a webhook's path is its secret.
    fn shown_url(&self) -> String {
        match self.url.find("://").map(|i| i + 3) {
            Some(i) => {
                let host_end = self.url[i..].find('/').map_or(self.url.len(), |j| i + j);
                let more = if host_end < self.url.len() { "/..." } else { "" };
                format!("{}{more}", &self.url[..host_end])
            }
            None => String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
struct Delivery {
    sent: u64,
    failed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_ok_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error_ms: Option<u64>,
}

/// What the rules file holds.
#[derive(Default, Serialize, Deserialize)]
struct Kept {
    rules: Vec<Rule>,
    #[serde(default)]
    routes: Vec<Route>,
    /// When the rules were last edited and by whom: between coordinators,
    /// and from a coordinator to its copies, the newest edit wins.
    #[serde(default)]
    rules_ms: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    rules_by: String,
}

/// One copy's state of an each_copy rule, as the coordinator last heard.
#[derive(Clone, Debug)]
struct CopySt {
    state: String,
    value: Option<f64>,
    since_ms: u64,
    short: String,
    shard: usize,
    /// The copy evaluates another version of the rule.
    differs: bool,
    /// Since when it has not reported (down, or reporting no state).
    unheard_ms: Option<u64>,
    /// Its points carry the rule's metric ([`St::reports`]).
    reports: bool,
    /// The copy's version of the rule fires ([`Alerts::gather`] judges it
    /// by this coordinator's).
    own_state: String,
}

/// The cluster as the built-in rules see it: one row a check.
#[derive(Default)]
struct ClusterView {
    /// (time, metric values) of each check, the oldest first.
    points: VecDeque<(u64, HashMap<&'static str, f64>)>,
    /// Each built-in metric's `where`, from the latest check.
    places: HashMap<&'static str, Vec<Json>>,
    /// Each shard's primary as last seen, and when primaries changed.
    primaries: HashMap<usize, String>,
    changes: VecDeque<(u64, Json)>,
}

/// How often a coordinator checks its cluster ([`tick`]).
const CHECK_MS: u64 = 5_000;
/// How long a coordinator waits before it pushes its rules again to a
/// copy that still runs others ([`Alerts::to_push`]).
const REPUSH_MS: u64 = 30_000;
/// How long a primary change keeps `primary-changed` firing.
const PRIMARY_CHANGED_MS: u64 = 600_000;
/// A copy whose latest point is older than this is silent.
const SILENT_MS: u64 = 60_000;

pub struct Alerts {
    rules: Mutex<Vec<Rule>>,
    states: Mutex<HashMap<String, St>>,
    events: Mutex<VecDeque<Arc<Event>>>,
    routes: Mutex<Vec<Route>>,
    deliveries: Mutex<HashMap<String, Delivery>>,
    file: Option<PathBuf>,
    http: reqwest::Client,
    coordinator: AtomicBool,
    /// The cores `key_busy_ratio` counts per (the machine's, unless set).
    cores: AtomicUsize,
    copies: Mutex<HashMap<(String, String), CopySt>>,
    cluster: Mutex<ClusterView>,
    /// When the coordinator last checked its cluster (ms), and whether a
    /// check runs.
    pub(crate) cluster_checked: AtomicU64,
    pub(crate) cluster_running: AtomicBool,
    identity: Mutex<Identity>,
    /// Each route's turn: its messages go one at a time, in order, and a
    /// route that fails or does not answer holds up only its own.
    turns: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// When the rules were last edited, and by whom ([`Kept::rules_ms`]).
    rules_ms: AtomicU64,
    rules_by: Mutex<String>,
    /// `<rules file>.state.json` ([`Saved`]).
    state_file: Option<PathBuf>,
    /// The coordinators of this cluster, this one included, as last heard
    /// ([`peering`]); empty: this one alone.
    peers: Mutex<Vec<crate::peers::Peer>>,
    /// What to tell the other coordinators ([`peering`]).
    outbox: Mutex<Vec<Json>>,
    outbox_ready: Arc<tokio::sync::Notify>,
    peering: AtomicBool,
    /// Each copy's last push of the each_copy rules: when, and how it went.
    pushed: Mutex<HashMap<String, (u64, Result<(), String>)>>,
    /// The copies' each_copy rules version ([`Kept::rules_ms`]), by URL.
    copy_rules_ms: Mutex<HashMap<String, u64>>,
    /// What fires on the copies of their own rules (scope self, which a
    /// coordinator does not gather), as their details last said
    /// ([`Self::gather`]): the Alerts page shows them (LIV-3).
    copies_firing: Mutex<Vec<Json>>,
    /// When each critical alert was last sent again ([`Route::repeat_min`]).
    repeated: Mutex<HashMap<String, u64>>,
    /// What each other coordinator last said of its alerts, by its URL:
    /// when, and each rule's state there.
    peer_states: Mutex<HashMap<String, (u64, HashMap<String, PeerSt>)>>,
    /// This server's service level objective, once given
    /// ([`Alerts::follow_slo`]): error ratio, p99 in ms.
    slo: Mutex<Option<(f64, f64)>>,
    /// Until when a restarted coordinator sends no FIRING message
    /// ([`STARTUP_MS`]); 0 for a server that starts afresh.
    starting_until: u64,
    /// The rules with `sql`: each one's last value, by its metric, and
    /// how its last run went ([`refresh_sql`]).
    sql_values: Mutex<std::collections::BTreeMap<String, f64>>,
    sql_runs: Mutex<HashMap<String, SqlRun>>,
    pub(crate) sql_checked: AtomicU64,
    pub(crate) sql_running: AtomicBool,
}

/// A rule's last run of its query: when, how long it took, and its
/// value or why it has none.
#[derive(Clone, Debug, Serialize)]
pub struct SqlRun {
    pub at_ms: u64,
    pub took_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// How often the rules with `sql` run their queries.
pub const SQL_EVERY_MS: u64 = 30_000;
/// How long a rule's query may take.
const SQL_TIMEOUT_MS: u64 = 10_000;
/// The longest query a rule holds.
const MAX_SQL: usize = 4096;

/// The metric a rule with `sql` watches.
pub fn sql_metric(id: &str) -> String {
    format!("sql_{id}")
}

/// How long a route has to take a message, its three tries included.
pub const POST_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// What notifications say of where they come from.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    /// The cluster's name (`--cluster-name`), else this server's.
    pub name: String,
    /// The command center's address, for links (`--public-url`).
    pub public_url: Option<String>,
}

/// Where PagerDuty's Events API v2 and Opsgenie's Alert API take alerts.
pub const PAGERDUTY_URL: &str = "https://events.pagerduty.com/v2/enqueue";
pub const OPSGENIE_URL: &str = "https://api.opsgenie.com/v2/alerts";

/// The state file of rules file `p`: `alerts.json` keeps its states in
/// `alerts.state.json`.
fn state_path(p: &std::path::Path) -> PathBuf {
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "alerts".into());
    p.with_file_name(format!("{stem}.state.json"))
}

impl Alerts {
    /// The rules and routes of `file` ([`defaults`] and no route if it
    /// does not exist), with a route "default" to `webhook` if given.
    pub fn new(file: Option<PathBuf>, webhook: Option<String>) -> Result<Alerts, String> {
        let mut kept = match &file {
            Some(p) => match std::fs::read(p) {
                Ok(b) => {
                    let j: Json = serde_json::from_slice(&b).map_err(|e| format!("--alerts {}: {e}", p.display()))?;
                    // A list of rules, as files held before routes.
                    let kept: Kept = match j {
                        Json::Array(_) => Kept { rules: serde_json::from_value(j).map_err(|e| format!("--alerts {}: {e}", p.display()))?, ..Kept::default() },
                        j => serde_json::from_value(j).map_err(|e| format!("--alerts {}: {e}", p.display()))?,
                    };
                    for r in &kept.rules {
                        r.check().map_err(|e| format!("--alerts {}: {}", p.display(), e.message))?;
                    }
                    kept
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Kept { rules: defaults(), ..Kept::default() },
                Err(e) => return Err(format!("--alerts {}: {e}", p.display())),
            },
            None => Kept { rules: defaults(), ..Kept::default() },
        };
        if let Some(url) = webhook.filter(|u| !u.is_empty()) {
            kept.routes.retain(|r| r.name != "default");
            kept.routes.insert(0, Route { name: "default".into(), url, severities: Vec::new(), kind: String::new(), key: String::new(), repeat_min: 0 });
        }
        // The states it had: a restart neither forgets nor sends again.
        let state_file = file.as_ref().map(|p| state_path(p));
        let now = now_ms();
        let restarted = state_file.as_ref().is_some_and(|p| p.exists());
        let saved: Saved = match &state_file {
            Some(p) => match std::fs::read(p) {
                Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                    tracing::warn!(file = %p.display(), error = %e, "the alerts' state file does not read: its states start again");
                    Saved::default()
                }),
                Err(_) => Saved::default(),
            },
            None => Saved::default(),
        };
        let states: HashMap<String, St> = saved.states.into_iter().map(|(k, v)| (k, St::restored(v, now))).collect();
        let mut view = ClusterView::default();
        view.primaries = saved.primaries.into_iter().filter_map(|(k, v)| k.parse().ok().map(|k| (k, v))).collect();
        view.changes = saved.changes.into_iter().filter(|(t, _)| t + PRIMARY_CHANGED_MS > now).collect();
        let http = crate::http_client().timeout(std::time::Duration::from_secs(3)).build().map_err(|e| e.to_string())?;
        // The service's (`--cluster-name`, `--public-url`), which the
        // server sets as it starts ([`Alerts::set_identity`]); this
        // machine's name until then.
        let identity = Identity { name: crate::logstore::hostname(), public_url: None };
        Ok(Alerts {
            rules: Mutex::new(kept.rules),
            states: Mutex::new(states),
            events: Mutex::new(VecDeque::new()),
            routes: Mutex::new(kept.routes),
            deliveries: Mutex::new(HashMap::new()),
            file,
            http,
            coordinator: AtomicBool::new(false),
            cores: AtomicUsize::new(std::thread::available_parallelism().map_or(1, |n| n.get())),
            copies: Mutex::new(HashMap::new()),
            cluster: Mutex::new(view),
            cluster_checked: AtomicU64::new(0),
            cluster_running: AtomicBool::new(false),
            identity: Mutex::new(identity),
            turns: Mutex::new(HashMap::new()),
            rules_ms: AtomicU64::new(kept.rules_ms),
            rules_by: Mutex::new(kept.rules_by),
            state_file,
            peers: Mutex::new(Vec::new()),
            outbox: Mutex::new(Vec::new()),
            outbox_ready: Arc::new(tokio::sync::Notify::new()),
            peering: AtomicBool::new(false),
            pushed: Mutex::new(HashMap::new()),
            copy_rules_ms: Mutex::new(HashMap::new()),
            copies_firing: Mutex::new(Vec::new()),
            repeated: Mutex::new(HashMap::new()),
            peer_states: Mutex::new(HashMap::new()),
            slo: Mutex::new(None),
            starting_until: if restarted { now + STARTUP_MS } else { 0 },
            sql_values: Mutex::new(Default::default()),
            sql_runs: Mutex::new(HashMap::new()),
            sql_checked: AtomicU64::new(0),
            sql_running: AtomicBool::new(false),
        })
    }

    /// The rules over errors, the p99 and searches that fail follow this
    /// server's service level objective ([`follow`]): the built-in one,
    /// and the default ones as long as nobody changed their threshold
    /// (the defaults before they followed it, 5% and 1 s, included).
    /// The p99 objective the rules follow ([`Alerts::follow_slo`]), else
    /// the default one.
    fn slo_p99_ms(&self) -> f64 {
        lock(&self.slo).map_or(crate::service::Slo::default().p99_ms, |(_, p)| p)
    }

    pub fn follow_slo(&self, error_ratio: f64, p99_ms: f64) {
        let slo = crate::service::Slo::default();
        let stock = |r: &Rule| match r.id.as_str() {
            "errors" => [slo.error_ratio, 0.05].contains(&r.threshold),
            "p99" => [slo.p99_ms, 1000.0].contains(&r.threshold),
            _ => false,
        };
        *lock(&self.slo) = Some((error_ratio, p99_ms));
        for r in lock(&self.rules).iter_mut().filter(|r| r.builtin || stock(r)) {
            follow(r, error_ratio, p99_ms);
        }
    }

    /// Names the cluster and the command center's address in
    /// notifications.
    pub fn set_identity(&self, name: Option<String>, public_url: Option<String>) {
        let mut i = lock(&self.identity);
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            i.name = n;
        }
        if let Some(u) = public_url.filter(|u| !u.is_empty()) {
            i.public_url = Some(u.trim_end_matches('/').to_string());
        }
    }

    pub fn identity(&self) -> Identity {
        lock(&self.identity).clone()
    }

    /// The cores this server's queries share, which `key_busy_ratio`
    /// counts the queries a key runs at once per.
    pub fn cores(&self) -> usize {
        self.cores.load(Ordering::Relaxed)
    }

    pub fn set_cores(&self, n: usize) {
        self.cores.store(n.max(1), Ordering::Relaxed);
    }

    /// Whether this server coordinates shards: it then has the
    /// [`builtins`], and gathers its copies' states of each_copy rules
    /// rather than evaluating them.
    pub fn set_coordinator(&self, yes: bool) {
        if self.coordinator.swap(yes, Ordering::Relaxed) == yes {
            return;
        }
        let mut rules = lock(&self.rules);
        if yes {
            for b in builtins() {
                if !rules.iter().any(|r| r.id == b.id) {
                    rules.push(b);
                }
            }
        }
        // Built-in rules kept in a file take the definitions of this
        // version, but for what may change, and this server's SLO.
        let defs = builtins();
        let slo = *lock(&self.slo);
        for r in rules.iter_mut().filter(|r| r.builtin) {
            if let Some(d) = defs.iter().find(|d| d.id == r.id) {
                *r = Rule { severity: r.severity.clone(), enabled: r.enabled, route: r.route.clone(), ..d.clone() };
                if let Some((e, p)) = slo {
                    follow(r, e, p);
                }
            }
        }
        if !yes {
            rules.retain(|r| !r.builtin);
        }
    }

    pub fn is_coordinator(&self) -> bool {
        self.coordinator.load(Ordering::Relaxed)
    }

    pub fn rules(&self) -> Vec<Rule> {
        lock(&self.rules).clone()
    }

    pub fn file(&self) -> Option<&std::path::Path> {
        self.file.as_deref()
    }

    /// Whether any route is set.
    pub fn webhook(&self) -> bool {
        !lock(&self.routes).is_empty()
    }

    pub fn routes(&self) -> Vec<Route> {
        lock(&self.routes).clone()
    }

    fn save(&self, rules: &[Rule], routes: &[Route]) -> Result<(), ApiError> {
        let Some(p) = &self.file else { return Ok(()) };
        let tmp = p.with_extension("tmp");
        let kept = Kept { rules: rules.to_vec(), routes: routes.to_vec(), rules_ms: self.rules_ms.load(Ordering::Relaxed), rules_by: lock(&self.rules_by).clone() };
        let body = serde_json::to_vec_pretty(&kept).map_err(ApiError::internal)?;
        std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, p)).map_err(|e| ApiError::internal(format!("write {}: {e}", p.display())))
    }

    /// Writes the state file ([`Saved`]); a failure is logged, not
    /// answered: the alerts go on in memory.
    fn save_state(&self) {
        let Some(p) = &self.state_file else { return };
        let saved = {
            let states = lock(&self.states);
            let c = lock(&self.cluster);
            Saved {
                saved_ms: now_ms(),
                states: states.iter().map(|(k, v)| (k.clone(), v.saved())).collect(),
                primaries: c.primaries.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
                changes: c.changes.iter().cloned().collect(),
            }
        };
        let tmp = p.with_extension("tmp");
        let res = serde_json::to_vec(&saved).map_err(|e| e.to_string()).and_then(|b| std::fs::write(&tmp, b).and_then(|_| std::fs::rename(&tmp, p)).map_err(|e| e.to_string()));
        if let Err(e) = res {
            static LIMIT: crate::obs::RateLimit = crate::obs::RateLimit::new(1);
            if LIMIT.admit().is_some() {
                tracing::warn!(file = %p.display(), error = %e, "could not write the alerts' state file");
            }
        }
    }

    /// When the rules were last edited (ms; 0: never), and by whom.
    pub fn rules_version(&self) -> (u64, String) {
        (self.rules_ms.load(Ordering::Relaxed), lock(&self.rules_by).clone())
    }

    /// Replaces the rules (and their file), keeping the states of the
    /// rules whose condition stays the same. A built-in rule takes only
    /// its severity, `enabled` and route, and is never removed. An alert
    /// that fired and whose rule changed, went or was disabled resolves:
    /// its events are returned, with the rules before and after.
    pub fn set_rules(&self, rules: Vec<Rule>, by: &str) -> Result<(Vec<Arc<Event>>, Vec<Rule>, Vec<Rule>), ApiError> {
        // Later than the edit before, even within one millisecond.
        self.set_rules_at(rules, by, now_ms().max(self.rules_ms.load(Ordering::Relaxed) + 1))
    }

    /// [`Self::set_rules`] as edited at `at_ms` (another coordinator's
    /// edit keeps its time).
    pub fn set_rules_at(&self, rules: Vec<Rule>, by: &str, at_ms: u64) -> Result<(Vec<Arc<Event>>, Vec<Rule>, Vec<Rule>), ApiError> {
        let mut ids = HashSet::new();
        let rules: Vec<Rule> = rules.into_iter().map(|mut r| {
            if !r.sql.trim().is_empty() {
                r.sql = r.sql.trim().trim_end_matches(';').trim_end().to_string();
                r.metric = sql_metric(&r.id);
            }
            r
        }).collect();
        for r in &rules {
            r.check()?;
            if !ids.insert(r.id.clone()) {
                return Err(ApiError::bad(format!("two rules have the id {:?}", r.id)));
            }
        }
        let before = self.rules();
        let mut next: Vec<Rule> = Vec::new();
        for r in rules {
            match before.iter().find(|b| b.id == r.id && b.builtin) {
                Some(b) => next.push(Rule { severity: r.severity, enabled: r.enabled, route: r.route, ..b.clone() }),
                None if builtins().iter().any(|b| b.id == r.id) && self.is_coordinator() => return Err(ApiError::bad(format!("{} is the id of a built-in rule", r.id))),
                None => next.push(Rule { builtin: false, ..r }),
            }
        }
        for b in before.iter().filter(|b| b.builtin) {
            if !next.iter().any(|r| r.id == b.id) {
                next.push(b.clone());
            }
        }
        let was = (self.rules_ms.swap(at_ms, Ordering::Relaxed), std::mem::replace(&mut *lock(&self.rules_by), by.to_string()));
        if let Err(e) = self.save(&next, &self.routes()) {
            self.rules_ms.store(was.0, Ordering::Relaxed);
            *lock(&self.rules_by) = was.1;
            return Err(e);
        }
        let t = now_ms();
        let mut events = Vec::new();
        let mut dropped: Vec<String> = Vec::new();
        {
            let mut now = lock(&self.rules);
            let mut states = lock(&self.states);
            for old in before.iter() {
                let new = next.iter().find(|r| r.id == old.id);
                let same = new.is_some_and(|n| n.condition() == old.condition());
                if same {
                    continue;
                }
                // Edited and still on: the state stays, and the next check
                // judges it by the rule as it is now. It resolves (saying
                // that the edit did it) only if the new rule does not hold,
                // and fires again on nothing.
                if new.is_some_and(|n| n.enabled && old.enabled) {
                    if let Some(st) = states.get_mut(&old.id) {
                        st.edited = Some((by.to_string(), t));
                    }
                    continue;
                }
                if let Some(st) = states.remove(&old.id) {
                    if st.state == "firing" {
                        let why = match new {
                            None => "the rule was deleted",
                            Some(n) if !n.enabled => "the rule was disabled",
                            Some(_) => "the rule was edited",
                        };
                        let mut e = self.resolution(old, &st, t);
                        e.why = Some(why.into());
                        e.by = Some(by.to_string());
                        e.fired_ms = Some(st.since_ms);
                        events.push(Arc::new(e));
                    }
                    // Notes, acknowledgements and silences stay with an
                    // edited rule.
                    if let Some(n) = new {
                        let mut fresh = St::new(t);
                        (fresh.notes, fresh.silenced) = (st.notes, st.silenced);
                        states.insert(n.id.clone(), fresh);
                    }
                }
                dropped.push(old.id.clone());
            }
            *now = next.clone();
        }
        // The copies' states of the rules changed, once `states` is let go
        // ([`Self::gather`] takes `copies`, then `states`).
        lock(&self.copies).retain(|(r, _), _| !dropped.contains(r));
        self.keep(&events);
        Ok((events, before, next))
    }

    /// Replaces the routes; a route given without a URL keeps the URL
    /// its name had.
    pub fn set_routes(&self, routes: Vec<Route>) -> Result<Vec<Route>, ApiError> {
        let before = self.routes();
        let mut names = HashSet::new();
        let mut next = Vec::new();
        for mut r in routes {
            if r.name.is_empty() || r.name.len() > 64 || !r.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                return Err(ApiError::bad(format!("a route's name is 1 to 64 letters, digits, '-' or '_', not {:?}", r.name)));
            }
            if !names.insert(r.name.clone()) {
                return Err(ApiError::bad(format!("two routes are named {:?}", r.name)));
            }
            if !matches!(r.kind.as_str(), "" | "webhook" | "pagerduty" | "opsgenie") {
                return Err(ApiError::bad(format!("route {}: kind is webhook, pagerduty or opsgenie, not {:?}", r.name, r.kind)));
            }
            if r.kind == "webhook" {
                r.kind.clear();
            }
            let had = before.iter().find(|b| b.name == r.name);
            if r.url.is_empty() {
                r.url = match (had, r.kind.as_str()) {
                    (Some(b), _) if b.kind == r.kind => b.url.clone(),
                    (_, "pagerduty") => PAGERDUTY_URL.into(),
                    (_, "opsgenie") => OPSGENIE_URL.into(),
                    _ => return Err(ApiError::bad(format!("route {} needs a url", r.name))),
                };
            }
            if r.key.is_empty() {
                r.key = had.map(|b| b.key.clone()).unwrap_or_default();
            }
            if r.kind != "" && r.key.is_empty() {
                return Err(ApiError::bad(format!("route {}: a {} route needs its key (PagerDuty's routing key, Opsgenie's API key)", r.name, r.kind)));
            }
            if r.repeat_min > 24 * 60 {
                return Err(ApiError::bad(format!("route {}: repeat_min is 0 (never) to 1440 minutes", r.name)));
            }
            if !(r.url.starts_with("https://") || r.url.starts_with("http://")) {
                return Err(ApiError::bad(format!("route {}: the url is http(s)", r.name)));
            }
            if let Some(s) = r.severities.iter().find(|s| !matches!(s.as_str(), "critical" | "warning" | "info")) {
                return Err(ApiError::bad(format!("route {}: severities are critical, warning and info, not {s:?}", r.name)));
            }
            next.push(r);
        }
        self.save(&self.rules(), &next)?;
        *lock(&self.routes) = next.clone();
        lock(&self.deliveries).retain(|n, _| next.iter().any(|r| &r.name == n));
        Ok(before)
    }

    /// The rules this server evaluates on its own points: all of them, but
    /// a coordinator's each_copy ones (its copies evaluate them) and its
    /// rules over the cluster.
    fn own(&self, r: &Rule) -> bool {
        r.enabled && !(self.is_coordinator() && (r.copies() || CLUSTER_METRICS.contains(&r.metric.as_str())))
    }

    /// Evaluates this server's rules on its points (`points`, the oldest
    /// first, with the latest at `t`); returns the changes, which it keeps
    /// and logs.
    pub fn check(&self, points: &[Arc<Json>], t: u64) -> Vec<Arc<Event>> {
        self.check_with(points, t, &|_, _| Vec::new())
    }

    /// [`Self::check`], with each key's calls of a window (`keys(from,
    /// to)`: key, calls, failed, the time they ran, its owner) for
    /// `key_error_ratio` and `key_busy_ratio`.
    pub fn check_with(&self, points: &[Arc<Json>], t: u64, keys: &dyn Fn(u64, u64) -> Vec<KeyCalls>) -> Vec<Arc<Event>> {
        let rules = self.rules();
        let slo_p99_ms = self.slo_p99_ms();
        let routes = self.routes();
        let mut changes = Vec::new();
        {
            let mut states = lock(&self.states);
            for r in rules.iter().filter(|r| self.own(r)) {
                let st = states.entry(r.id.clone()).or_insert_with(|| St::new(t));
                let role = ROLE_METRICS.contains(&r.metric.as_str()).then(|| role_key(&r.metric));
                if role.is_some_and(|k| points.last().is_some_and(|p| value_of(p, k).is_none())) {
                    st.reports = false;
                    if st.gone(t).is_some() {
                        changes.push(Arc::new(self.resolution(r, st, t)));
                    }
                    continue;
                }
                let (v, partial) = if KEY_METRICS.contains(&r.metric.as_str()) {
                    let from = t.saturating_sub(r.window_s * 1000);
                    let calls = keys(from, t + 1);
                    let firing = st.state == "firing";
                    let (v, place) = if r.metric == "key_busy_ratio" { key_busy(r, firing, &calls, t + 1 - from, self.cores()) } else { key_window(r, firing, &calls) };
                    if v.is_some() {
                        st.where_ = place;
                    }
                    (v, None)
                } else {
                    window_of(r, points, t, slo_p99_ms)
                };
                // A server that took the role within the window (a primary
                // elected a moment ago, whose old primary is still starting
                // again) is judged once the metric spans the window: "all"
                // over a minute means a minute of it.
                let since = role.and_then(|k| points.iter().rev().take_while(|p| value_of(p, k).is_some()).last()).and_then(|p| p["t"].as_u64());
                let young = since.is_some_and(|s| s > t.saturating_sub(r.window_s * 1000) + crate::telemetry::INTERVAL.as_millis() as u64);
                if young && st.state != "firing" && v.is_some_and(|x| r.holds(x)) {
                    (st.value, st.no_data_since_ms, st.reports) = (v, None, true);
                    if st.state != "pending" {
                        (st.state, st.since_ms) = ("pending", t);
                    }
                    continue;
                }
                st.partial_ms = partial.filter(|_| v.is_some());
                st.reports = v.is_some() || points.iter().any(|p| value_of(p, &r.metric).is_some()) || role.is_some();
                if let Some(change) = st.step(r, v, t) {
                    let mut e = if change == "resolved" { self.resolution(r, st, t) } else { Event::of(r, change, st, t) };
                    if KEY_METRICS.contains(&r.metric.as_str()) {
                        owner_route(&mut e, &routes);
                    }
                    changes.push(Arc::new(e));
                }
            }
            // Rules removed or disabled have no state but their notes.
            states.retain(|id, s| rules.iter().any(|r| r.enabled && &r.id == id) || !s.notes.is_empty());
            self.hold(&rules, &mut states, t);
        }
        self.keep(&changes);
        changes
    }

    /// The event of the alert of rule `id` last firing.
    fn last_fired(&self, id: &str) -> Option<Arc<Event>> {
        lock(&self.events).iter().rev().find(|e| e.rule == id && e.state == "firing").cloned()
    }

    /// The event of rule `r`'s alert resolved: when it fired and where,
    /// from the event of its firing (where it fired no longer fires, so
    /// the state no longer names it).
    fn resolution(&self, r: &Rule, st: &St, t: u64) -> Event {
        let mut e = Event::of(r, "resolved", st, t);
        if let Some((by, _)) = st.edited.as_ref().filter(|(_, at)| t.saturating_sub(*at) <= EDIT_SAYS_MS) {
            e.why = Some("the rule was edited".into());
            e.by = Some(by.clone());
        }
        if let Some(f) = self.last_fired(&r.id) {
            e.fired_ms = Some(f.ts_ms);
            if e.where_.is_empty() {
                (e.where_, e.place) = (f.where_.clone(), f.place.clone());
            }
        }
        e
    }

    /// Keeps events and logs the changes.
    fn keep(&self, changes: &[Arc<Event>]) {
        if changes.is_empty() {
            return;
        }
        self.save_state();
        let mut events = lock(&self.events);
        for e in changes {
            if events.len() == EVENTS {
                events.pop_front();
            }
            events.push_back(e.clone());
            // The plain number, or "no data" (never `Some(0.036)`).
            let value = e.value.map_or_else(|| "no data".to_string(), number);
            match e.state {
                "firing" => tracing::warn!(rule = %e.rule, severity = %e.severity, value = %value, threshold = %number(e.threshold), place = %e.where_.join(", "), "alert firing: {}", e.name),
                "resolved" => tracing::info!(rule = %e.rule, value = %value, why = e.why.as_deref().unwrap_or(""), "alert resolved: {}", e.name),
                _ => tracing::info!(rule = %e.rule, by = e.by.as_deref().unwrap_or(""), "alert {}: {}", e.state, e.name),
            }
        }
    }

    /// A coordinator's check of its cluster's state (`/v1/cluster`'s
    /// answer): the built-in rules over it, and its copies' states of
    /// each_copy rules gathered.
    pub fn check_cluster(&self, snap: &Json, t: u64) -> Vec<Arc<Event>> {
        let rules = self.rules();
        let mut changes = Vec::new();
        let (values, places) = self.view(snap, t);
        {
            let mut c = lock(&self.cluster);
            c.points.push_back((t, values));
            while c.points.front().is_some_and(|(pt, _)| *pt + MAX_WINDOW_S * 1000 < t) {
                c.points.pop_front();
            }
            c.places = places;
        }
        // The built-in rules over the cluster's view.
        {
            let c = lock(&self.cluster);
            let mut states = lock(&self.states);
            for r in rules.iter().filter(|r| r.enabled && CLUSTER_METRICS.contains(&r.metric.as_str())) {
                let from = t.saturating_sub(r.window_s * 1000);
                let vals: Vec<(u64, f64)> = c.points.iter().filter(|(pt, _)| *pt > from && *pt <= t).filter_map(|(pt, m)| m.get(r.metric.as_str()).map(|v| (*pt, *v))).collect();
                let v = aggregate(r, &vals);
                let st = states.entry(r.id.clone()).or_insert_with(|| St::new(t));
                // Checked since less than the window: over the checks there are.
                st.partial_ms = c.points.front().map(|(pt, _)| *pt).filter(|pt| v.is_some() && *pt > from + CHECK_MS).map(|pt| t.saturating_sub(pt));
                st.where_ = c.places.get(r.metric.as_str()).cloned().unwrap_or_default();
                if let Some(change) = st.step(r, v, t) {
                    let e = if change == "resolved" { self.resolution(r, st, t) } else { Event::of(r, change, st, t) };
                    changes.push(Arc::new(e));
                }
            }
        }
        changes.extend(self.gather(snap, &rules, t));
        self.hold(&rules, &mut lock(&self.states), t);
        self.keep(&changes);
        changes
    }

    /// The built-in metrics of a cluster's state, and what each names.
    fn view(&self, snap: &Json, t: u64) -> (HashMap<&'static str, f64>, HashMap<&'static str, Vec<Json>>) {
        let mut places: HashMap<&'static str, Vec<Json>> = HashMap::new();
        let shards = snap["shards"].as_array().cloned().unwrap_or_default();
        let mut c = lock(&self.cluster);
        for s in &shards {
            let i = s["shard"].as_u64().unwrap_or(0) as usize;
            let replicas = s["replicas"].as_array().cloned().unwrap_or_default();
            let up: Vec<&Json> = replicas.iter().filter(|r| answers(r)).collect();
            let n = replicas.len();
            // The snapshot's `majority` (core's shard state), else from the
            // copies that answer. Without failover a shard serves reads
            // without a majority: that is copy-down's, not this rule's.
            let majority = s["majority"].as_bool().unwrap_or(n > 0 && up.len() * 2 > n);
            let failover = s["failover"].as_bool().unwrap_or(true);
            let shard_name = format!("shard {i}");
            if !majority && failover {
                let why = s["why"].as_str().map(String::from).unwrap_or_else(|| format!("{} of {n} copies answer", up.len()));
                places.entry("shards_no_majority").or_default().push(json!({"shard": i, "name": shard_name, "short": shard_name, "state": "firing", "why": why}));
            }
            // In sync and serving: answers, serves reads, and its primary
            // counts no replica out of sync.
            let healthy = up.iter().filter(|r| r["serves_reads"] != false && r["detail"]["load"]["serves_reads"].as_f64() != Some(0.0)).count();
            let out_of_sync = up.iter().filter_map(|r| r["detail"]["load"]["replicas_out_of_sync"].as_f64()).fold(0.0, f64::max) as usize;
            let in_sync = healthy.saturating_sub(out_of_sync);
            if n > 0 && in_sync < n {
                places.entry("shards_under_replicated").or_default().push(json!({"shard": i, "name": shard_name, "short": shard_name, "state": "firing", "value": in_sync, "of": n}));
            }
            for r in replicas.iter() {
                let url = r["url"].as_str().unwrap_or_default().to_string();
                let short = r["short"].as_str().map(String::from).unwrap_or_else(|| crate::coord::copy_short(i, &url));
                if !answers(r) {
                    let since = r["down_since_ms"].as_u64();
                    places.entry("copies_down").or_default().push(json!({"copy": url, "short": short, "shard": i, "state": "firing", "since_ms": since, "error": r["error"]}));
                } else {
                    let seen = r["detail"]["load"]["t"].as_u64().or_else(|| r["last_seen_ms"].as_u64());
                    if seen.is_none_or(|s| t.saturating_sub(s) > SILENT_MS) {
                        places.entry("copies_silent").or_default().push(json!({"copy": url, "short": short, "shard": i, "state": "firing", "last_ms": seen}));
                    }
                }
            }
            // The primary: the field, else the copy that answers as one
            // and takes writes.
            let primary = s["primary"].as_str().map(String::from).or_else(|| up.iter().find(|r| r["role"] == "primary" && r["takes_writes"] != false).and_then(|r| r["url"].as_str()).map(String::from));
            if let Some(p) = primary {
                let was = c.primaries.insert(i, p.clone());
                if let Some(w) = was.filter(|w| *w != p) {
                    c.changes.push_back((t, json!({"shard": i, "name": shard_name, "short": shard_name, "state": "firing", "from": w, "to": p, "at_ms": t})));
                }
            }
        }
        while c.changes.front().is_some_and(|(ct, _)| ct + PRIMARY_CHANGED_MS < t) {
            c.changes.pop_front();
        }
        if !c.changes.is_empty() {
            places.insert("primary_changes", c.changes.iter().map(|(_, j)| j.clone()).collect());
        }
        let mut values = HashMap::new();
        if !shards.is_empty() {
            for m in CLUSTER_METRICS {
                values.insert(m, places.get(m).map_or(0, Vec::len) as f64);
            }
        }
        (values, places)
    }

    /// Gathers the copies' states of the each_copy rules from their
    /// detail (`detail.alerts.rules`): a copy that stops reporting keeps
    /// its last state, and the rule's state is the worst copy's. A copy
    /// whose points do not carry the metric (only primaries, or only
    /// replicas, report some) does not count, nor one without data now:
    /// the rule has no data only when no copy has its metric, and no data
    /// keeps a firing alert firing. A copy that runs another version of the
    /// rule (its digest differs: it has not taken an edit yet) is judged by
    /// this coordinator's rule on the value it reports, so that no message
    /// names a value that does not pass the rule it names.
    fn gather(&self, snap: &Json, rules: &[Rule], t: u64) -> Vec<Arc<Event>> {
        let mut changes = Vec::new();
        // Copies the last push of the rules did not reach: one that is not
        // heard from either runs a rule older than this coordinator's.
        let not_pushed: HashSet<String> = lock(&self.pushed).iter().filter(|(_, (_, r))| r.is_err()).map(|(u, _)| u.clone()).collect();
        let mut copies = lock(&self.copies);
        let mut present = HashSet::new();
        let mut on_copies: Vec<Json> = Vec::new();
        let mut versions = lock(&self.copy_rules_ms);
        for s in snap["shards"].as_array().into_iter().flatten() {
            let i = s["shard"].as_u64().unwrap_or(0) as usize;
            for r in s["replicas"].as_array().into_iter().flatten() {
                let url = r["url"].as_str().unwrap_or_default().to_string();
                let short = r["short"].as_str().map(String::from).unwrap_or_else(|| crate::coord::copy_short(i, &url));
                let reported = &r["detail"]["alerts"]["rules"];
                if answers(r) {
                    for f in r["detail"]["alerts"]["firing"].as_array().into_iter().flatten() {
                        // Each_copy rules are gathered below, as the coordinator's own.
                        let id = f["id"].as_str().unwrap_or_default();
                        if !rules.iter().any(|x| x.id == id && x.copies()) {
                            let mut one = f.clone();
                            (one["copy"], one["short"], one["shard"]) = (json!(url), json!(short), json!(i));
                            on_copies.push(one);
                        }
                    }
                }
                if let Some(v) = r["detail"]["alerts"]["rules_ms"].as_u64() {
                    versions.insert(url.clone(), v);
                }
                for rule in rules.iter().filter(|r| r.enabled && r.copies()) {
                    present.insert((rule.id.clone(), url.clone()));
                    let key = (rule.id.clone(), url.clone());
                    let mine = &reported[&rule.id];
                    let e = copies.entry(key).or_insert_with(|| CopySt { state: "no_data".into(), value: None, since_ms: t, short: short.clone(), shard: i, differs: false, unheard_ms: Some(t), reports: true, own_state: "no_data".into() });
                    e.short = short.clone();
                    match mine["state"].as_str() {
                        Some(state) => {
                            e.value = mine["value"].as_f64();
                            e.differs = mine["sig"].as_str().is_some_and(|s| s != rule.sig());
                            e.reports = mine["reports"].as_bool().unwrap_or(true);
                            e.unheard_ms = None;
                            e.own_state = state.to_string();
                            let judged = if !e.differs {
                                state.to_string()
                            } else {
                                // The copy's rule is not this one: this one's
                                // threshold on its value.
                                match e.value {
                                    Some(v) if rule.holds(v) || (e.state == "firing" && rule.still(v)) => "firing".into(),
                                    Some(_) => "ok".into(),
                                    None => "no_data".into(),
                                }
                            };
                            if !e.differs {
                                e.since_ms = mine["since_ms"].as_u64().unwrap_or(t);
                            } else if judged != e.state {
                                e.since_ms = t;
                            }
                            e.state = judged;
                        }
                        None => {
                            let since = *e.unheard_ms.get_or_insert(t);
                            if t.saturating_sub(since) >= UNHEARD_MS {
                                if e.state != "no_data" {
                                    (e.state, e.since_ms) = ("no_data".into(), t);
                                }
                            } else if not_pushed.contains(&url) {
                                // Not heard, and the edit did not reach it: its
                                // last value, by this rule.
                                e.differs = true;
                                if let Some(v) = e.value {
                                    let judged = if rule.holds(v) || (e.state == "firing" && rule.still(v)) { "firing" } else { "ok" };
                                    if judged != e.state {
                                        (e.state, e.since_ms) = (judged.into(), t);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        copies.retain(|k, _| present.contains(k));
        versions.retain(|u, _| present.iter().any(|(_, x)| x == u));
        drop(versions);
        on_copies.sort_by(|a, b| (sev_rank(a["severity"].as_str().unwrap_or("")), a["short"].as_str()).cmp(&(sev_rank(b["severity"].as_str().unwrap_or("")), b["short"].as_str())));
        *lock(&self.copies_firing) = on_copies;
        let mut states = lock(&self.states);
        for rule in rules.iter().filter(|r| r.enabled && r.copies()) {
            let mine: Vec<&CopySt> = copies.iter().filter(|((r, _), _)| r == &rule.id).map(|(_, c)| c).collect();
            let urls: Vec<&String> = copies.iter().filter(|((r, _), _)| r == &rule.id).map(|((_, u), _)| u).collect();
            let count = |s: &str| mine.iter().filter(|c| c.reports && c.state == s).count();
            let counted: Vec<usize> = (0..mine.len()).filter(|&j| mine[j].reports && mine[j].state != "no_data").collect();
            let worst = counted.iter().map(|&j| state_rank(&mine[j].state)).min();
            let st = states.entry(rule.id.clone()).or_insert_with(|| St::new(t));
            let was = st.state;
            let state: &'static str = match worst {
                Some(0) => "firing",
                Some(1) => "pending",
                Some(_) => "ok",
                // No copy says: a firing alert stays firing.
                None if was == "firing" => "firing",
                None => "no_data",
            };
            let in_state: Vec<usize> = match worst {
                Some(w) => counted.iter().copied().filter(|&j| state_rank(&mine[j].state) == w).collect(),
                None => Vec::new(),
            };
            let value = in_state.iter().filter_map(|&j| mine[j].value).reduce(|a, b| if rule.op.starts_with('>') { a.max(b) } else { a.min(b) });
            if worst.is_some() {
                st.value = value;
                st.no_data_since_ms = None;
            } else {
                st.no_data_since_ms.get_or_insert(t);
            }
            let mut place: Vec<(u8, Json)> = (0..mine.len())
                .filter(|&j| (mine[j].reports && mine[j].state != "ok") || mine[j].differs)
                .map(|j| {
                    let c = mine[j];
                    let mut w = json!({"copy": urls[j], "short": c.short, "shard": c.shard, "state": c.state, "value": c.value, "since_ms": c.since_ms});
                    if c.differs {
                        w["rule_differs"] = json!(true);
                        w["copy_state"] = json!(c.own_state);
                    }
                    if let Some(u) = c.unheard_ms {
                        w["unheard_since_ms"] = json!(u);
                    }
                    (state_rank(&c.state), w)
                })
                .collect();
            place.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1["short"].as_str().cmp(&b.1["short"].as_str())));
            if worst.is_some() || st.where_.is_empty() {
                st.where_ = place.into_iter().map(|(_, w)| w).collect();
            }
            let pushed = lock(&self.pushed);
            let failed: Vec<Json> = urls.iter().enumerate().filter(|(j, _)| mine[*j].differs).filter_map(|(j, u)| pushed.get(*u).and_then(|(at, r)| r.as_ref().err().map(|e| json!({"copy": u, "short": mine[j].short, "error": e, "at_ms": at})))).collect();
            drop(pushed);
            st.copies = Some(json!({
                "total": mine.len(), "firing": count("firing"), "pending": count("pending"), "no_data": count("no_data"), "ok": count("ok"),
                "not_reported": mine.iter().filter(|c| !c.reports).count(), "rule_differs": mine.iter().filter(|c| c.differs).count(),
                "push_failed": failed,
            }));
            if state != was {
                st.since_ms = in_state.iter().map(|&j| mine[j].since_ms).min().unwrap_or(t);
                st.state = state;
                st.no_data_since_ms = (state == "no_data").then_some(t);
                let change = match (was, state) {
                    (_, "firing") => Some("firing"),
                    ("firing", _) => Some("resolved"),
                    _ => None,
                };
                if let Some(c) = change {
                    if c == "resolved" {
                        st.clear(t);
                    } else {
                        st.fired(t);
                    }
                    let e = if c == "resolved" { self.resolution(rule, st, t) } else { Event::of(rule, c, st, t) };
                    changes.push(Arc::new(e));
                }
            }
        }
        changes
    }

    /// The copies that run other each_copy rules than this coordinator's
    /// (a digest differs, a rule is missing or one is left over), older
    /// than its edit, and not pushed to in the last [`REPUSH_MS`]: the
    /// copies to push its rules to ([`push_rules`]).
    pub fn to_push(&self, snap: &Json, t: u64) -> Vec<String> {
        if !self.is_coordinator() {
            return Vec::new();
        }
        let rules: Vec<Rule> = self.rules().into_iter().filter(|r| r.copies()).collect();
        let mine = self.rules_ms.load(Ordering::Relaxed);
        let pushed = lock(&self.pushed);
        let mut out = Vec::new();
        for s in snap["shards"].as_array().into_iter().flatten() {
            for r in s["replicas"].as_array().into_iter().flatten().filter(|r| answers(r)) {
                let Some(url) = r["url"].as_str() else { continue };
                let a = &r["detail"]["alerts"];
                let Some(reported) = a["rules"].as_object() else { continue };
                // A copy whose rules are newer than this coordinator's was
                // edited after it: this one does not undo that edit.
                if a["rules_ms"].as_u64().unwrap_or(0) > mine {
                    continue;
                }
                let on: Vec<&Rule> = rules.iter().filter(|r| r.enabled).collect();
                let differs = on.iter().any(|r| reported.get(&r.id).is_none_or(|x| x["sig"].as_str().is_some_and(|s| s != r.sig())))
                    || reported.keys().any(|k| !on.iter().any(|r| &r.id == k));
                if differs && pushed.get(url).is_none_or(|(at, _)| t.saturating_sub(*at) >= REPUSH_MS) {
                    out.push(url.to_string());
                }
            }
        }
        out
    }

    /// Notes how a push of the rules to copy `url` went.
    fn pushed_to(&self, url: &str, t: u64, res: Result<(), String>) {
        lock(&self.pushed).insert(url.to_string(), (t, res));
    }

    /// The each_copy rules, as a coordinator gives them to its copies.
    pub fn copy_rules(&self) -> Vec<Rule> {
        self.rules().into_iter().filter(|r| r.copies()).collect()
    }

    /// On a copy: replaces its each_copy rules with a coordinator's (its
    /// own rules stay), edited at `at_ms` by `by`, unless its own are newer.
    /// Answers `{took, rules_ms}`.
    pub fn set_copy_rules(&self, rules: Vec<Rule>, by: &str, at_ms: u64) -> Result<Json, ApiError> {
        if self.is_coordinator() {
            return Err(ApiError { code: None, status: 409, message: "this server is a coordinator: its each_copy rules are its own".into() });
        }
        let mine = self.rules_ms.load(Ordering::Relaxed);
        if mine > at_ms {
            return Ok(json!({"took": false, "rules_ms": mine, "why": format!("this copy's rules were edited later, at {mine} by {}", lock(&self.rules_by))}));
        }
        if let Some(r) = rules.iter().find(|r| !r.copies()) {
            return Err(ApiError::bad(format!("rule {} is not an each_copy rule", r.id)));
        }
        let next: Vec<Rule> = self.rules().into_iter().filter(|r| !r.copies()).chain(rules).collect();
        let (events, ..) = self.set_rules_at(next, by, at_ms)?;
        Ok(json!({"took": true, "rules_ms": at_ms, "resolved": events.len()}))
    }

    /// Acknowledges rule `id`'s alert, which fires or is pending.
    pub fn ack(&self, id: &str, by: &str, note: Option<String>) -> Result<Arc<Event>, ApiError> {
        let r = self.rule(id)?;
        let t = now_ms();
        let live_here = lock(&self.states).get(id).is_some_and(|s| matches!(s.state, "firing" | "pending"));
        if !live_here {
            // It fires on another coordinator only: the acknowledgement
            // goes to the coordinators, and the one where it fires keeps it.
            let mut probe = json!({"rule": id, "state": "ok"});
            Self::add_elsewhere(&mut probe, &self.live_views());
            if probe.get("elsewhere").is_none() {
                return Err(ApiError { code: None, status: 409, message: format!("{id} is not firing nor pending: there is nothing to acknowledge") });
            }
            let mut st = St::new(t);
            st.ack = Some(Ack { by: by.into(), at_ms: t, note: note.clone() });
            if let Some(n) = note.filter(|n| !n.trim().is_empty()) {
                push_note(&mut st, by, t, n);
            }
            let mut e = Event::of(&r, "acked", &st, t);
            (e.by, e.text) = (Some(by.into()), st.ack.as_ref().and_then(|a| a.note.clone()));
            e.context = Some(format!("it fires on {}", probe["elsewhere"].as_array().into_iter().flatten().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>().join(", ")));
            let e = Arc::new(e);
            self.keep(std::slice::from_ref(&e));
            self.tell_peers(json!({"states": {id: {"ack": st.ack, "notes": st.notes}}}));
            return Ok(e);
        }
        let mut states = lock(&self.states);
        let st = states.get_mut(id).filter(|s| matches!(s.state, "firing" | "pending")).ok_or_else(|| ApiError { code: None, status: 409, message: format!("{id} is not firing nor pending: there is nothing to acknowledge") })?;
        st.ack = Some(Ack { by: by.into(), at_ms: t, note: note.clone() });
        if let Some(n) = note.filter(|n| !n.trim().is_empty()) {
            push_note(st, by, t, n);
        }
        let mut e = Event::of(&r, "acked", st, t);
        (e.by, e.text) = (Some(by.into()), st.ack.as_ref().and_then(|a| a.note.clone()));
        let told = json!({"states": {id: {"ack": st.ack, "notes": st.notes.last().filter(|n| n.at_ms == t).into_iter().collect::<Vec<_>>()}}});
        drop(states);
        let e = Arc::new(e);
        self.keep(std::slice::from_ref(&e));
        self.tell_peers(told);
        Ok(e)
    }

    /// Holds back rule `id`'s notifications for `minutes`.
    pub fn silence(&self, id: &str, by: &str, minutes: u64, reason: &str) -> Result<Arc<Event>, ApiError> {
        if minutes == 0 || minutes > 7 * 24 * 60 {
            return Err(ApiError::bad("minutes is 1 to 10080 (a week)"));
        }
        if reason.trim().is_empty() {
            return Err(ApiError::bad("say why it is silenced: the body needs a \"reason\""));
        }
        let r = self.rule(id)?;
        let t = now_ms();
        let mut states = lock(&self.states);
        let st = states.entry(id.to_string()).or_insert_with(|| St::new(t));
        st.silenced = Some(Silence { by: by.into(), until_ms: t + minutes * 60_000, reason: reason.into(), at_ms: t });
        let mut e = Event::of(&r, "silenced", st, t);
        (e.by, e.text) = (Some(by.into()), Some(format!("for {minutes} min: {reason}")));
        let told = json!({"states": {id: {"silenced": st.silenced}}});
        drop(states);
        let e = Arc::new(e);
        self.keep(std::slice::from_ref(&e));
        self.tell_peers(told);
        Ok(e)
    }

    pub fn unsilence(&self, id: &str, by: &str) -> Result<Arc<Event>, ApiError> {
        let r = self.rule(id)?;
        let t = now_ms();
        let mut states = lock(&self.states);
        let st = states.get_mut(id).filter(|s| s.silenced.as_ref().is_some_and(|s| s.until_ms > t)).ok_or_else(|| ApiError { code: None, status: 404, message: format!("{id} is not silenced") })?;
        // Kept as a silence that ended now: the newest word wins between
        // coordinators.
        if let Some(si) = st.silenced.as_mut() {
            (si.until_ms, si.at_ms, si.by) = (t, t, by.into());
        }
        let mut e = Event::of(&r, "unsilenced", st, t);
        e.by = Some(by.into());
        let told = json!({"states": {id: {"silenced": st.silenced}}});
        drop(states);
        let e = Arc::new(e);
        self.keep(std::slice::from_ref(&e));
        self.tell_peers(told);
        Ok(e)
    }

    /// Adds a note to rule `id`'s alert.
    pub fn note(&self, id: &str, by: &str, text: &str) -> Result<Arc<Event>, ApiError> {
        let text = text.trim();
        if text.is_empty() || text.len() > 4000 {
            return Err(ApiError::bad("a note is 1 to 4,000 characters of text"));
        }
        let r = self.rule(id)?;
        let t = now_ms();
        let mut states = lock(&self.states);
        let st = states.entry(id.to_string()).or_insert_with(|| St::new(t));
        push_note(st, by, t, text.to_string());
        let mut e = Event::of(&r, "noted", st, t);
        (e.by, e.text) = (Some(by.into()), Some(text.into()));
        let told = json!({"states": {id: {"notes": st.notes.last().into_iter().collect::<Vec<_>>()}}});
        drop(states);
        let e = Arc::new(e);
        self.keep(std::slice::from_ref(&e));
        self.tell_peers(told);
        Ok(e)
    }

    fn rule(&self, id: &str) -> Result<Rule, ApiError> {
        self.rules().into_iter().find(|r| r.id == id).ok_or_else(|| ApiError { code: None, status: 404, message: format!("no alert rule {id}") })
    }

    /// Rule `r`'s state as the API answers it.
    fn state_json(&self, r: &Rule, st: Option<&St>) -> Json {
        let now = now_ms();
        let mut j = json!({
            "rule": r.id, "name": r.name, "severity": r.severity, "scope": r.scope, "builtin": r.builtin,
            "metric": r.metric, "op": r.op, "threshold": r.threshold, "eval": r.eval, "window_s": r.window_s, "for_s": r.for_s,
            "resolve": r.resolve.unwrap_or(r.threshold), "description": r.description, "owner": r.owner, "runbook": r.runbook, "route": r.route, "vs": r.vs,
        });
        match (r.enabled, st) {
            (false, _) => j["state"] = json!("disabled"),
            (true, None) => {
                j["state"] = json!(if self.is_coordinator() && r.copies() { "no_data" } else { "no_data" });
            }
            (true, Some(s)) => {
                j["state"] = json!(s.state);
                j["since_ms"] = json!(s.since_ms);
                j["value"] = json!(s.value);
                if let Some(n) = s.no_data_since_ms {
                    j["no_data_since_ms"] = json!(n);
                }
                if let Some(p) = s.partial_ms {
                    // Evaluated over the points there are: the server has
                    // run less than the window.
                    j["window_partial"] = json!(true);
                    j["window_covered_s"] = json!(p / 1000);
                }
                j["where"] = json!(s.where_);
                if let Some(c) = &s.copies {
                    j["copies"] = c.clone();
                }
                if let Some(a) = &s.ack {
                    j["ack"] = json!(a);
                }
                if let Some(p) = &s.paged {
                    j["paged"] = json!(p);
                }
                if let Some((by, why)) = &s.held {
                    (j["held_by"], j["held_why"]) = (json!(by), json!(why));
                    if let Some(u) = s.settle_ms {
                        // Its cause resolved; it waits for its window.
                        j["held_until_ms"] = json!(u);
                    }
                }
            }
        }
        if let Some(s) = st {
            if let Some(si) = s.silenced.as_ref().filter(|si| si.until_ms > now) {
                j["silenced"] = json!(si);
            }
            j["notes"] = json!(s.notes);
        } else {
            j["notes"] = json!([]);
        }
        if j.get("where").is_none() {
            j["where"] = json!([]);
        }
        j
    }

    /// The rules, their states (firing first, then pending, no data, ok),
    /// the latest events (the newest first), the routes and their
    /// deliveries.
    pub fn json(&self) -> Json {
        let rules = self.rules();
        let states = lock(&self.states).clone();
        let mut out: Vec<Json> = rules.iter().map(|r| self.state_json(r, states.get(&r.id))).collect();
        let views = self.live_views();
        if !views.is_empty() {
            for s in &mut out {
                Self::add_elsewhere(s, &views);
            }
        }
        out.sort_by_key(|s| (state_rank(s["state"].as_str().unwrap_or("")), match s["severity"].as_str() {
            Some("critical") => 0,
            Some("warning") => 1,
            _ => 2,
        }));
        // The rules as before, each with its state.
        let with: Vec<Json> = rules
            .iter()
            .map(|r| {
                let mut j = json!(r);
                // A rule without a runbook of its own whose id has one compiled
                // in (every default and built-in rule): where it is (CCW-6, CCW-13).
                if r.runbook.is_empty() {
                    if let Some(b) = crate::runbooks::for_id(&r.id) {
                        j["runbook_docs"] = json!(format!("{}#{}", crate::runbooks::PAGE, b.anchor));
                    }
                }
                let s = out.iter().find(|s| s["rule"] == r.id.as_str()).cloned().unwrap_or(Json::Null);
                j["status"] = json!({"state": s["state"], "since_ms": s["since_ms"], "value": s["value"]});
                if let Some(run) = (!r.sql.is_empty()).then(|| lock(&self.sql_runs).get(&r.id).cloned()).flatten() {
                    j["sql_run"] = json!(run);
                }
                j
            })
            .collect();
        let count = |st: &str| out.iter().filter(|s| s["state"] == st).count();
        let events: Vec<Arc<Event>> = lock(&self.events).iter().rev().take(500).cloned().collect();
        let id = self.identity();
        let (me, pager) = self.pager();
        let (rules_ms, rules_by) = self.rules_version();
        let peers: Vec<Json> = lock(&self.peers).iter().map(|p| json!({"url": p.url, "name": p.name, "this": p.this, "last_seen_ms": p.last_seen_ms})).collect();
        json!({
            "rules": with, "states": out, "events": events,
            "firing": count("firing"), "pending": count("pending"), "no_data": count("no_data"),
            // Firing, neither silenced nor held back under their cause: the bell's count.
            "firing_shown": out.iter().filter(|s| s["state"] == "firing" && s.get("silenced").is_none() && s.get("held_by").is_none()).count(),
            // Of those, the ones that fire on another coordinator only.
            "firing_elsewhere": out.iter().filter(|s| s["state"] == "firing" && s.get("here").is_some()).count(),
            "routes": self.routes_json(), "file": self.file.as_ref().map(|p| p.display().to_string()), "webhook": self.webhook(),
            "state_file": self.state_file.as_ref().map(|p| p.display().to_string()),
            "coordinator": self.is_coordinator(), "cluster": {"name": id.name, "public_url": id.public_url},
            "rules_ms": rules_ms, "rules_by": rules_by,
            // The copies' own rules (scope self) that fire there now: not
            // this server's alerts, so not counted in `firing`, but shown.
            "copies_firing": *lock(&self.copies_firing),
            // Who sends the messages: with several coordinators, one.
            "pager": {"this": me, "sends": pager, "coordinators": peers},
        })
    }

    /// `GET /v1/alerts?brief=1`, for the bell on views that show no
    /// alerts: each rule's id, name, severity, metric and state, and of
    /// those not ok their since, value, acknowledgement, silence, cause
    /// and copies; no events, routes nor `where`: a fifth of the whole.
    pub fn brief_json(&self) -> Json {
        let full = self.json();
        let rules: Vec<Json> = full["rules"].as_array().map_or(&[][..], |v| v.as_slice()).iter().map(|r| json!({"id": r["id"], "name": r["name"], "severity": r["severity"], "metric": r["metric"], "status": {"state": r["status"]["state"]}})).collect();
        let states: Vec<Json> = full["states"]
            .as_array()
            .map_or(&[][..], |v| v.as_slice())
            .iter()
            .filter(|s| !matches!(s["state"].as_str(), Some("ok" | "disabled" | "no_data")))
            .map(|s| {
                let mut j = json!({"rule": s["rule"], "name": s["name"], "severity": s["severity"], "state": s["state"], "since_ms": s["since_ms"], "value": s["value"]});
                for k in ["ack", "silenced", "held_by", "copies", "here", "elsewhere"] {
                    if let Some(v) = s.get(k) {
                        j[k] = v.clone();
                    }
                }
                j
            })
            .collect();
        let mut j = json!({"brief": true, "rules": rules, "states": states});
        for k in ["firing", "pending", "no_data", "firing_shown", "firing_elsewhere", "coordinator", "cluster", "copies_firing"] {
            j[k] = full[k].clone();
        }
        j
    }

    /// One rule's state, with its events in memory, the newest first.
    pub fn one(&self, id: &str) -> Result<Json, ApiError> {
        let r = self.rule(id)?;
        let st = lock(&self.states).get(id).cloned();
        let mut j = self.state_json(&r, st.as_ref());
        Self::add_elsewhere(&mut j, &self.live_views());
        let events: Vec<Arc<Event>> = lock(&self.events).iter().rev().filter(|e| e.rule == id).cloned().collect();
        j["events"] = json!(events);
        j["rule_json"] = json!(r);
        Ok(j)
    }

    pub fn routes_json(&self) -> Vec<Json> {
        let d = lock(&self.deliveries).clone();
        self.routes()
            .iter()
            .map(|r| {
                let mut j = json!({"name": r.name, "url": r.shown_url(), "severities": r.severities, "kind": if r.kind.is_empty() { "webhook" } else { r.kind.as_str() }, "repeat_min": r.repeat_min, "has_key": !r.key.is_empty()});
                let s = d.get(&r.name).cloned().unwrap_or_default();
                for (k, v) in json!(s).as_object().into_iter().flatten() {
                    j[k] = v.clone();
                }
                j
            })
            .collect()
    }

    /// What a copy says of its alerts in its detail, for a coordinator
    /// (`detail.alerts`): the alerts firing, how many are pending and
    /// without data, and each each_copy rule's state with its rule's
    /// digest.
    pub fn summary(&self) -> Json {
        let rules = self.rules();
        let states = lock(&self.states);
        let mut firing = Vec::new();
        let (mut pending, mut no_data) = (0, 0);
        let mut each = serde_json::Map::new();
        for r in rules.iter().filter(|r| r.enabled) {
            let Some(s) = states.get(&r.id) else {
                // A rule just given (by a coordinator's push) and not checked yet.
                if r.copies() && !self.is_coordinator() {
                    each.insert(r.id.clone(), json!({"state": "no_data", "value": null, "since_ms": now_ms(), "sig": r.sig(), "reports": true}));
                }
                continue;
            };
            match s.state {
                "firing" => firing.push(json!({"id": r.id, "name": r.name, "severity": r.severity, "since_ms": s.since_ms, "value": s.value})),
                "pending" => pending += 1,
                "no_data" => no_data += 1,
                _ => {}
            }
            if r.copies() && !self.is_coordinator() {
                let mut one = json!({"state": s.state, "value": s.value, "since_ms": s.since_ms, "sig": r.sig(), "reports": s.reports});
                if s.partial_ms.is_some() {
                    one["window_partial"] = json!(true);
                }
                each.insert(r.id.clone(), one);
            }
        }
        json!({"firing": firing, "pending": pending, "no_data": no_data, "rules": each, "rules_ms": self.rules_ms.load(Ordering::Relaxed)})
    }

    /// Works out which firing (or pending) alerts a cause explains
    /// ([`CAUSES`]) and holds their messages back: a copy down explains
    /// its shard's replica states (out of sync, behind, not serving reads,
    /// under-replicated) whatever their severity; a shard without its
    /// majority explains its copies down; and either explains the
    /// coordinator's traffic rules (searches fail, errors, p99, partial
    /// answers) that are no more severe than it. A held alert shows under
    /// its cause, and its message is sent if it still fires once the cause
    /// has resolved and its own window has passed since, which held the
    /// outage ([`Self::due`]).
    fn hold(&self, rules: &[Rule], states: &mut HashMap<String, St>, t: u64) {
        let mut causes: Vec<(&Rule, Vec<u64>, Vec<String>)> = rules
            .iter()
            .filter(|r| r.enabled && CAUSES.contains(&r.id.as_str()))
            .filter_map(|r| {
                let st = states.get(&r.id).filter(|s| s.state == "firing")?;
                let firing: Vec<&Json> = st.where_.iter().filter(|w| w["state"].as_str().is_none_or(|s| s == "firing")).collect();
                // Firing nowhere now: it resolves at its next check.
                if firing.is_empty() {
                    return None;
                }
                Some((r, firing.iter().filter_map(|w| w["shard"].as_u64()).collect(), firing.iter().filter_map(|w| w["short"].as_str().map(String::from)).collect()))
            })
            .collect();
        causes.sort_by_key(|(r, ..)| sev_rank(&r.severity));
        for r in rules.iter().filter(|r| r.enabled) {
            let Some(st) = states.get_mut(&r.id) else { continue };
            if !matches!(st.state, "firing" | "pending") {
                (st.held, st.settle_ms) = (None, None);
                continue;
            }
            let shards: Vec<u64> = st.where_.iter().filter(|w| w["state"].as_str().is_none_or(|s| matches!(s, "firing" | "pending"))).filter_map(|w| w["shard"].as_u64()).collect();
            let held = causes.iter().find_map(|(c, cs, names)| {
                if c.id == r.id {
                    return None;
                }
                let same_shard = !shards.is_empty() && shards.iter().all(|s| cs.contains(s));
                let explains = if REPLICA_METRICS.contains(&r.metric.as_str()) {
                    same_shard
                } else if r.id == "copy-down" {
                    c.id == "shard-no-majority" && same_shard
                } else {
                    r.scope == "self" && TRAFFIC_METRICS.contains(&r.metric.as_str()) && sev_rank(&r.severity) >= sev_rank(&c.severity)
                };
                explains.then(|| (c.id.clone(), format!("{} (on {})", c.name, if names.is_empty() { "the cluster".to_string() } else { names.join(", ") })))
            });
            if held.is_some() {
                st.settle_ms = None;
            } else if let Some((_, why)) = st.held.clone().filter(|_| st.paged.is_none()) {
                // Its cause resolved: its window and its wait, which still
                // hold the outage, pass before it is sent (`held_until_ms`).
                let window = (r.window_s + r.for_s).min(600) * 1000;
                if t < *st.settle_ms.get_or_insert(t + window) {
                    continue;
                }
                st.late = Some(format!("held back while {why} fired, and still firing {} after it resolved", duration(window)));
                st.settle_ms = None;
            }
            st.held = held;
        }
    }

    /// What coordinator `url` last said of its alerts, if it said so within
    /// [`PEER_LIVE_MS`] of `t`.
    fn view_of(&self, url: &str, t: u64) -> Option<(u64, HashMap<String, PeerSt>)> {
        lock(&self.peer_states).get(url).filter(|(heard, _)| t.saturating_sub(*heard) <= PEER_LIVE_MS).cloned()
    }

    /// The other coordinators heard within [`PEER_LIVE_MS`]: URL, name and
    /// what each said of its alerts.
    fn live_views(&self) -> Vec<(String, String, HashMap<String, PeerSt>)> {
        let t = now_ms();
        let names: HashMap<String, String> = lock(&self.peers).iter().filter(|p| !p.this).map(|p| (p.url.clone(), p.name.clone())).collect();
        let views = lock(&self.peer_states);
        let mut out: Vec<_> = views
            .iter()
            .filter(|(url, (heard, _))| t.saturating_sub(*heard) <= PEER_LIVE_MS && names.contains_key(*url))
            .map(|(url, (_, m))| (url.clone(), names.get(url).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| name_of(url)), m.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Rule state `s` (as [`Self::state_json`] answers it) with what the
    /// other coordinators hold of it: an alert over a coordinator's own
    /// traffic fires there alone, and every coordinator shows it, as
    /// firing "on" that one (R2-scenarios-1). `here` keeps this one's own
    /// state; `elsewhere` lists the coordinators where it fires or waits.
    fn add_elsewhere(s: &mut Json, views: &[(String, String, HashMap<String, PeerSt>)]) {
        let id = s["rule"].as_str().unwrap_or("").to_string();
        let here = s["state"].as_str().unwrap_or("").to_string();
        if matches!(here.as_str(), "firing" | "disabled") {
            return;
        }
        let there: Vec<(&String, &String, &PeerSt)> = views.iter().filter_map(|(u, n, m)| m.get(&id).filter(|p| matches!(p.state.as_str(), "firing" | "pending")).map(|p| (u, n, p))).collect();
        let Some(&(_, name, best)) = there.iter().min_by_key(|(_, _, p)| (state_rank(&p.state), p.held_by.is_some(), p.since_ms)) else { return };
        if state_rank(&best.state) < state_rank(&here) {
            s["here"] = json!(here);
            s["state"] = json!(best.state);
            s["since_ms"] = json!(best.since_ms);
            s["value"] = json!(best.value);
            if let Some(o) = s.as_object_mut() {
                o.remove("held_by");
                o.remove("held_why");
                o.remove("held_until_ms");
            }
            if let Some(c) = &best.held_by {
                (s["held_by"], s["held_why"]) = (json!(c), json!(format!("its cause fires (on {name})")));
            }
            if s.get("ack").is_none_or(Json::is_null) {
                if let Some(a) = &best.ack {
                    s["ack"] = a.clone();
                }
            }
        }
        s["elsewhere"] = json!(there.iter().map(|(u, n, p)| json!({"url": u, "name": n, "state": p.state, "since_ms": p.since_ms, "value": p.value, "held_by": p.held_by, "ack": p.ack})).collect::<Vec<_>>());
    }

    /// This coordinator's URL and the URL of the one that sends the
    /// messages: the lowest URL among the coordinators heard in the last
    /// [`PEER_LIVE_MS`] (alone: itself; both empty).
    pub fn pager(&self) -> (String, String) {
        let now = now_ms();
        let peers = lock(&self.peers);
        let me = peers.iter().find(|p| p.this).map(|p| p.url.clone()).unwrap_or_default();
        let pager = peers.iter().filter(|p| p.this || now.saturating_sub(p.last_seen_ms) <= PEER_LIVE_MS).map(|p| p.url.clone()).min().unwrap_or_else(|| me.clone());
        (me, pager)
    }

    /// The coordinators of this cluster as [`peering`] last heard them.
    pub fn set_peers(&self, peers: Vec<crate::peers::Peer>) {
        *lock(&self.peers) = peers;
    }

    /// The FIRING messages due now: of each firing alert that nobody sent
    /// yet and that is neither silenced nor held back. This coordinator
    /// sends them when it is the one that sends messages, or when that one
    /// has not said it sent one within [`PAGER_WAIT_MS`] of the firing.
    /// Also the alerts a route sends again ([`Route::repeat_min`]). Each is
    /// noted as sent (and told to the other coordinators); its event is a
    /// message's, not a change.
    pub fn due(&self, t: u64) -> Vec<(Arc<Event>, Option<String>)> {
        let (me, pager) = self.pager();
        let rules = self.rules();
        let routes = self.routes();
        let cluster = self.identity().name;
        let mut out = Vec::new();
        let mut told = Vec::new();
        // What the one that sends the messages said of its alerts.
        let pager_view = if me == pager { None } else { self.view_of(&pager, t) };
        {
            let mut states = lock(&self.states);
            for r in rules.iter().filter(|r| r.enabled) {
                let Some(st) = states.get_mut(&r.id) else { continue };
                if st.state != "firing" || st.held.is_some() {
                    st.due_ms = None;
                    continue;
                }
                let silenced = st.silenced.as_ref().is_some_and(|s| s.until_ms > t);
                if silenced {
                    st.late = Some(format!("silenced until {}", st.silenced.as_ref().map_or(0, |s| s.until_ms)));
                    st.due_ms = None;
                    continue;
                }
                if st.paged.is_none() {
                    // Restarted: the others may have sent it meanwhile.
                    if t < self.starting_until && self.is_coordinator() {
                        continue;
                    }
                    let waited = t.saturating_sub(st.since_ms);
                    let mine = me == pager;
                    // The one that sends the messages said, a while after
                    // this fired here, that it does not see it: a rule
                    // over this coordinator's own traffic. Sent at once.
                    let unseen = !mine && pager_view.as_ref().is_some_and(|(heard, m)| *heard >= st.since_ms + NOT_SEEN_MS && m.get(&r.id).is_none_or(|p| !matches!(p.state.as_str(), "firing" | "pending")));
                    // Another coordinator waits from when this one could send it.
                    if !mine && !unseen && t.saturating_sub(*st.due_ms.get_or_insert(t)) < PAGER_WAIT_MS {
                        continue;
                    }
                    let mut e = Event::of(r, "firing", st, t);
                    (e.ts_ms, e.episode_ms) = (st.since_ms, Some(st.since_ms));
                    e.context = match (&st.late, mine) {
                        (_, false) if unseen => Some(format!("sent by {}: it fires on this coordinator's own traffic, and {}, which sends the messages, does not see it", name_of(&me), name_of(&pager))),
                        (_, false) => Some(format!("sent by {}: {} sends the messages but did not say it sent this within {} s", name_of(&me), name_of(&pager), PAGER_WAIT_MS / 1000)),
                        (Some(why), _) if why.starts_with("silenced") => Some(format!("firing since {}; its silence ended", clock(st.since_ms))),
                        (Some(why), _) => Some(format!("firing since {}; {why}", clock(st.since_ms))),
                        (None, _) if waited > 30_000 => Some(format!("firing since {}", clock(st.since_ms))),
                        (None, _) => None,
                    };
                    let p = Paged { by: if me.is_empty() { "this server".into() } else { me.clone() }, at_ms: t, key: format!("strata/{cluster}/{}/{}", r.id, st.since_ms) };
                    st.paged = Some(p.clone());
                    st.late = None;
                    e.paged_by = Some(p.by.clone());
                    e.dedup_key = Some(p.key.clone());
                    told.push(json!({"states": {r.id.clone(): {"paged": p}}}));
                    out.push((Arc::new(e), None));
                    continue;
                }
                // Sent again, by the routes that do, while nobody acknowledges a critical alert.
                if r.severity != "critical" || st.ack.is_some() || me != pager {
                    continue;
                }
                let mut repeated = lock(&self.repeated);
                for rt in routes.iter().filter(|rt| rt.repeat_min > 0 && (if r.route.is_empty() { rt.takes(&r.severity) } else { rt.name == r.route })) {
                    let key = format!("{}/{}", rt.name, r.id);
                    let last = repeated.get(&key).copied().unwrap_or(st.paged.as_ref().map_or(t, |p| p.at_ms)).max(st.since_ms);
                    if t.saturating_sub(last) >= rt.repeat_min * 60_000 {
                        repeated.insert(key, t);
                        let mut e = Event::of(r, "firing", st, t);
                        e.episode_ms = Some(st.since_ms);
                        e.context = Some(format!("still firing since {}, and nobody acknowledged it", clock(st.since_ms)));
                        out.push((Arc::new(e), Some(rt.name.clone())));
                    }
                }
            }
        }
        if !told.is_empty() {
            for op in told {
                self.tell_peers(op);
            }
            self.save_state();
        }
        out
    }

    /// Of `changes`, the resolutions to send: of alerts whose FIRING
    /// message was sent (by any coordinator), from the one that sends the
    /// messages, unless silenced.
    pub fn resolutions(&self, changes: &[Arc<Event>]) -> Vec<Arc<Event>> {
        let (me, pager) = self.pager();
        if me != pager {
            // What this one sent because the one that sends the messages
            // did not see it fire ([`Self::due`]): its resolution too.
            let Some((_, view)) = self.view_of(&pager, now_ms()) else { return Vec::new() };
            return changes.iter().filter(|e| e.state == "resolved" && !e.silenced && e.paged_by.as_deref() == Some(me.as_str()) && view.get(&e.rule).is_none_or(|p| !matches!(p.state.as_str(), "firing" | "pending"))).cloned().collect();
        }
        changes.iter().filter(|e| e.state == "resolved" && e.paged_by.is_some() && !e.silenced).cloned().collect()
    }

    /// Tells the other coordinators (`op`: part of what [`Self::peer_state`]
    /// answers), at once, through [`peering`].
    pub fn tell_peers(&self, op: Json) {
        if !self.is_coordinator() || lock(&self.peers).iter().all(|p| p.this) {
            return;
        }
        lock(&self.outbox).push(op);
        self.outbox_ready.notify_one();
    }

    /// What another coordinator merges of this one ([`Self::merge`]): its
    /// rules and their version, and each alert's acknowledgement, silence,
    /// notes and page.
    pub fn peer_state(&self) -> Json {
        let (me, _) = self.pager();
        let (rules_ms, rules_by) = self.rules_version();
        let states = lock(&self.states);
        let each: serde_json::Map<String, Json> = states
            .iter()
            .map(|(id, st)| {
                let mut j = json!({"state": st.state, "since_ms": st.since_ms, "cleared_ms": st.cleared_ms, "notes": st.notes, "value": st.value});
                for (k, v) in [("ack", json!(st.ack)), ("silenced", json!(st.silenced)), ("paged", json!(st.paged)), ("held_by", json!(st.held.as_ref().map(|h| &h.0)))] {
                    if !v.is_null() {
                        j[k] = v;
                    }
                }
                (id.clone(), j)
            })
            .collect();
        json!({"url": me, "rules_ms": rules_ms, "rules_by": rules_by, "rules": self.rules(), "states": each})
    }

    /// Merges what another coordinator holds or tells (`j`, shaped as
    /// [`Self::peer_state`]; `routes` too, with their URLs and keys): its
    /// rules when they are newer; an acknowledgement of an alert that fires
    /// or is pending here; the newest silence; notes not held yet; and a
    /// page. Returns the events of what changed here, for the timeline.
    pub fn merge(&self, j: &Json, from: &str) -> Result<Vec<Arc<Event>>, ApiError> {
        let mut events = Vec::new();
        let peer_ms = j["rules_ms"].as_u64().unwrap_or(0);
        if peer_ms > self.rules_ms.load(Ordering::Relaxed) {
            if let Some(rules) = j.get("rules").filter(|r| r.is_array()) {
                let rules: Vec<Rule> = serde_json::from_value(rules.clone()).map_err(|e| ApiError::bad(format!("bad rules: {e}")))?;
                let rules: Vec<Rule> = rules.into_iter().filter(|r| !r.builtin || self.is_coordinator()).collect();
                let by = j["rules_by"].as_str().unwrap_or("another coordinator");
                let (ev, ..) = self.set_rules_at(rules, by, peer_ms)?;
                tracing::info!(from, by, rules_ms = peer_ms, "alert rules taken from another coordinator");
                events.extend(ev);
            }
        }
        if let Some(routes) = j.get("routes").filter(|r| r.is_array()) {
            let routes: Vec<Route> = serde_json::from_value(routes.clone()).map_err(|e| ApiError::bad(format!("bad routes: {e}")))?;
            self.set_routes(routes)?;
        }
        let t = now_ms();
        // A whole answer (not a change told at once): each alert's state
        // there, which a rule over that coordinator's own traffic may hold
        // alone.
        if j.get("rules_ms").is_some() && !from.is_empty() {
            let each: HashMap<String, PeerSt> = j["states"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(id, x)| {
                    let state = x["state"].as_str()?.to_string();
                    Some((id.clone(), PeerSt { state, since_ms: x["since_ms"].as_u64().unwrap_or(0), value: x["value"].as_f64(), held_by: x["held_by"].as_str().map(str::to_string), ack: x.get("ack").filter(|a| !a.is_null()).cloned() }))
                })
                .collect();
            lock(&self.peer_states).insert(from.to_string(), (t, each));
        }
        let rules = self.rules();
        let mut changed = false;
        {
            let mut states = lock(&self.states);
            for (id, x) in j["states"].as_object().into_iter().flatten() {
                let Some(r) = rules.iter().find(|r| &r.id == id) else { continue };
                let st = states.entry(id.clone()).or_insert_with(|| St::new(t));
                let live = matches!(st.state, "firing" | "pending");
                if let Some(a) = x.get("ack").and_then(|a| serde_json::from_value::<Ack>(a.clone()).ok()) {
                    if live && st.ack.is_none() && a.at_ms > st.cleared_ms {
                        let mut e = Event::of(r, "acked", st, a.at_ms);
                        (e.by, e.text) = (Some(a.by.clone()), a.note.clone());
                        st.ack = Some(a);
                        events.push(Arc::new(e));
                        changed = true;
                    }
                }
                if let Some(si) = x.get("silenced").and_then(|a| serde_json::from_value::<Silence>(a.clone()).ok()) {
                    if st.silenced.as_ref().is_none_or(|mine| si.at_ms > mine.at_ms) && si.at_ms > 0 {
                        let lifted = si.until_ms <= si.at_ms;
                        let mut e = Event::of(r, if lifted { "unsilenced" } else { "silenced" }, st, si.at_ms);
                        e.by = Some(si.by.clone());
                        if !lifted {
                            e.text = Some(format!("until {}: {}", clock(si.until_ms), si.reason));
                        }
                        st.silenced = Some(si);
                        events.push(Arc::new(e));
                        changed = true;
                    }
                }
                for n in x["notes"].as_array().into_iter().flatten().filter_map(|n| serde_json::from_value::<Note>(n.clone()).ok()) {
                    if !st.notes.contains(&n) {
                        let mut e = Event::of(r, "noted", st, n.at_ms);
                        (e.by, e.text) = (Some(n.by.clone()), Some(n.text.clone()));
                        // In time, then by who and what: every coordinator lists them alike.
                        let at = st.notes.partition_point(|m| (m.at_ms, &m.by, &m.text) <= (n.at_ms, &n.by, &n.text));
                        st.notes.insert(at, n);
                        if st.notes.len() > NOTES {
                            st.notes.remove(0);
                        }
                        events.push(Arc::new(e));
                        changed = true;
                    }
                }
                if let Some(p) = x.get("paged").and_then(|a| serde_json::from_value::<Paged>(a.clone()).ok()) {
                    if st.paged.is_none() && p.at_ms > st.cleared_ms {
                        if live {
                            st.paged = Some(p);
                        } else {
                            st.paged_hint = Some(p);
                        }
                        changed = true;
                    }
                }
            }
        }
        // Their timeline's events here too; what went to the routes went
        // from the coordinator that took them.
        if changed && events.is_empty() {
            self.save_state();
        }
        self.keep(&events);
        Ok(events)
    }

    /// Alerts firing, by severity, for `/metrics`.
    pub fn firing(&self) -> Vec<(String, f64)> {
        let rules = self.rules();
        let states = lock(&self.states);
        ["critical", "warning", "info"]
            .iter()
            .map(|sev| {
                let n = rules.iter().filter(|r| r.severity == *sev && states.get(&r.id).is_some_and(|s| s.state == "firing")).count();
                (format!("severity=\"{sev}\""), n as f64)
            })
            .collect()
    }

    /// The text a notification of `e` says.
    pub fn message(&self, e: &Event) -> String {
        let id = self.identity();
        let place = match e.where_.len() {
            0 => String::new(),
            1..=3 => format!("/{}", e.where_.join(",")),
            n => format!("/{} and {} more", e.where_[..3].join(","), n - 3),
        };
        let flag = FLAG_METRICS.contains(&e.metric.as_str());
        let vs = self.rules().into_iter().find(|r| r.id == e.rule).is_some_and(|r| r.vs == "before");
        let value = match e.value {
            None => format!("{} = no data", e.metric),
            Some(v) if flag => format!("{} is {}", e.metric, if v > 0.0 { "on" } else { "off" }),
            Some(v) if vs => format!("{} = {} times the {} before", e.metric, number_vs(v, e.threshold), duration(e.window_s * 1000)),
            Some(v) => format!("{} = {}", e.metric, number_vs(v, e.threshold)),
        };
        let window = duration(e.window_s * 1000);
        let mut text = match e.state {
            "resolved" => {
                let after = e.fired_ms.map(|f| format!(" after {}", duration(e.ts_ms.saturating_sub(f)))).unwrap_or_default();
                let why = e.why.as_ref().map(|w| format!(" ({w}{})", e.by.as_ref().map(|b| format!(" by {b}")).unwrap_or_default())).unwrap_or_default();
                format!("[chloedb RESOLVED {}] {} on {}{place}: {value}{after}{why}.", e.severity, e.name, id.name)
            }
            state if flag => {
                let how = match e.eval.as_str() {
                    "all" => format!("every point of {window}"),
                    "max" | "min" => format!("a point in {window}"),
                    ev => format!("{ev} over {window}"),
                };
                format!("[chloedb {} {}] {} on {}{place}: {value} ({how}).", state.to_uppercase(), e.severity, e.name, id.name)
            }
            state => format!("[chloedb {} {}] {} on {}{place}: {value} ({} {}, {} over {window}).", state.to_uppercase(), e.severity, e.name, id.name, e.op, number(e.threshold), e.eval),
        };
        if e.state == "firing" && !e.description.is_empty() {
            text.push(' ');
            text.push_str(&e.description);
        }
        if !e.owner.is_empty() {
            text.push_str(&format!(" Owner: {}.", e.owner));
        }
        if e.rule_differs > 0 {
            text.push_str(&format!(" {} of its copies have not taken the rule's last edit yet; their values are judged by the rule as it is now.", e.rule_differs));
        }
        if let Some(c) = &e.context {
            text.push_str(&format!(" ({c}.)"));
        }
        if !e.runbook.is_empty() {
            text.push_str(&format!(" Runbook: {}", e.runbook));
        }
        if let Some(u) = &id.public_url {
            text.push_str(&format!(" {u}/#/alert/{}", e.rule));
        }
        text
    }

    /// Posts each message to the routes that take it ([`Self::notify_to`]).
    pub async fn notify(&self, messages: &[Arc<Event>]) {
        let all: Vec<(Arc<Event>, Option<String>)> = messages.iter().map(|e| (e.clone(), None)).collect();
        self.notify_to(&all).await;
    }

    /// Posts each message (an event, and the one route it goes to when
    /// named) to the routes that take it, and counts what was delivered.
    /// Each route takes its messages on its own, in order: one that fails
    /// or does not answer holds up only its own, [`POST_BUDGET`] a message
    /// at most.
    pub async fn notify_to(&self, messages: &[(Arc<Event>, Option<String>)]) {
        let routes = self.routes();
        let mut each: Vec<(Route, Vec<(Json, String)>)> = Vec::new();
        for (e, only) in messages {
            let text = self.message(e);
            for r in routes.iter().filter(|r| match only {
                Some(n) => &r.name == n,
                None if e.route.is_empty() => r.takes(&e.severity),
                None => r.name == e.route,
            }) {
                let (body, key) = self.body(r, e, &text);
                match each.iter_mut().find(|(x, _)| x.name == r.name) {
                    Some((_, bodies)) => bodies.push((body, key)),
                    None => each.push((r.clone(), vec![(body, key)])),
                }
            }
        }
        futures::future::join_all(each.into_iter().map(|(r, bodies)| async move {
            let turn = lock(&self.turns).entry(r.name.clone()).or_default().clone();
            let _turn = turn.lock().await;
            for (b, key) in &bodies {
                let url = if r.kind == "opsgenie" && b["action"] == "close" { format!("{}/{}/close?identifierType=alias", r.url.trim_end_matches('/'), urlencode(b["alias"].as_str().unwrap_or_default())) } else { r.url.clone() };
                let _ = self.post(&r, &url, b, key).await;
            }
        }))
        .await;
    }

    /// The key that names an alert's episode in every message of it, on
    /// every coordinator: `<cluster>/<rule>/<when it fired>`.
    pub fn dedup_key(&self, rule: &str, fired_ms: u64) -> String {
        format!("strata/{}/{rule}/{fired_ms}", self.identity().name)
    }

    /// Message `e` as route `r` takes it, and the key that says it is the
    /// same message when a try is repeated (`Idempotency-Key`).
    fn body(&self, r: &Route, e: &Event, text: &str) -> (Json, String) {
        let dedup = e.dedup_key.clone().unwrap_or_else(|| self.dedup_key(&e.rule, e.episode_ms.or(e.fired_ms).unwrap_or(e.ts_ms)));
        let idem = format!("{dedup}/{}/{}", e.state, e.ts_ms);
        let id = self.identity();
        let link = id.public_url.as_ref().map(|u| format!("{u}/#/alert/{}", e.rule));
        let summary: String = text.chars().take(1000).collect();
        let body = match (r.kind.as_str(), e.state) {
            ("pagerduty", state) => json!({
                "routing_key": r.key, "event_action": if state == "resolved" { "resolve" } else { "trigger" }, "dedup_key": dedup,
                "payload": {"summary": summary, "source": id.name, "severity": if e.severity == "critical" { "critical" } else if e.severity == "warning" { "warning" } else { "info" },
                    "component": e.where_.first().cloned().unwrap_or_default(), "group": e.rule, "custom_details": e},
                "links": link.iter().map(|l| json!({"href": l, "text": "The alert in the command center"})).collect::<Vec<_>>(),
            }),
            ("opsgenie", "resolved") => json!({"action": "close", "alias": dedup, "source": id.name, "note": summary}),
            ("opsgenie", _) => json!({
                "message": e.name.chars().take(130).collect::<String>(), "alias": dedup, "description": summary, "source": id.name,
                "priority": match e.severity.as_str() { "critical" => "P1", "warning" => "P3", _ => "P5" }, "entity": e.where_.join(", "),
                "details": {"rule": e.rule, "metric": e.metric, "value": e.value, "link": link},
            }),
            _ => json!({"text": text, "alert": e, "dedup_key": dedup}),
        };
        (body, idem)
    }

    /// Posts `body` to route `r` (at `url`), three times at most within
    /// [`POST_BUDGET`], each with the same `Idempotency-Key`; counts how it
    /// went.
    async fn post(&self, r: &Route, url: &str, body: &Json, key: &str) -> Result<(), String> {
        let tries = async {
            let mut last = String::new();
            for attempt in 0..3 {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(500 * (1 << attempt))).await;
                }
                let mut req = self.http.post(url).header("idempotency-key", key).json(body);
                if r.kind == "opsgenie" {
                    req = req.header("authorization", format!("GenieKey {}", r.key));
                }
                match req.send().await.and_then(|r| r.error_for_status()) {
                    Ok(_) => return Ok(()),
                    Err(e) => last = e.without_url().to_string(),
                }
            }
            Err(last)
        };
        let res = match tokio::time::timeout(POST_BUDGET, tries).await {
            Ok(res) => res,
            Err(_) => Err(format!("no answer in {} s", POST_BUDGET.as_secs())),
        };
        if let Err(e) = &res {
            tracing::warn!(route = %r.name, error = %e, "could not post an alert to its route");
        }
        let mut d = lock(&self.deliveries);
        let d = d.entry(r.name.clone()).or_default();
        match &res {
            Ok(()) => {
                d.sent += 1;
                d.last_ok_ms = Some(now_ms());
            }
            Err(e) => {
                d.failed += 1;
                d.last_error = Some(e.clone());
                d.last_error_ms = Some(now_ms());
            }
        }
        res
    }

    /// Posts a test message to route `name`.
    pub async fn test_route(&self, name: &str, by: &str) -> Result<Json, ApiError> {
        let r = self.routes().into_iter().find(|r| r.name == name).ok_or_else(|| ApiError { code: None, status: 404, message: format!("no route {name}") })?;
        let id = self.identity();
        let text = format!("[chloedb TEST] A test of route {} on {}, sent by {by}.{}", r.name, id.name, id.public_url.map(|u| format!(" {u}/#/alerts")).unwrap_or_default());
        let t0 = std::time::Instant::now();
        let key = format!("strata/{}/test/{}", id.name, now_ms());
        // PagerDuty and Opsgenie: an info alert that is closed at once.
        let (body, close) = match r.kind.as_str() {
            "pagerduty" => (
                json!({"routing_key": r.key, "event_action": "trigger", "dedup_key": key, "payload": {"summary": text, "source": id.name, "severity": "info"}}),
                Some((r.url.clone(), json!({"routing_key": r.key, "event_action": "resolve", "dedup_key": key}))),
            ),
            "opsgenie" => (
                json!({"message": format!("strata test of route {}", r.name), "alias": key, "description": text, "source": id.name, "priority": "P5"}),
                Some((format!("{}/{}/close?identifierType=alias", r.url.trim_end_matches('/'), urlencode(&key)), json!({"source": id.name}))),
            ),
            _ => (json!({"text": text, "test": true}), None),
        };
        let res = self.post(&r, &r.url, &body, &format!("{key}/test")).await;
        if let (Ok(()), Some((url, b))) = (&res, close) {
            let _ = self.post(&r, &url, &b, &format!("{key}/close")).await;
        }
        match res {
            Ok(()) => Ok(json!({"route": r.name, "ok": true, "ms": t0.elapsed().as_secs_f64() * 1e3, "text": text})),
            Err(e) => Ok(json!({"route": r.name, "ok": false, "error": e, "ms": t0.elapsed().as_secs_f64() * 1e3, "text": text})),
        }
    }

    /// Replays `r` over `points` (each with `t` and the rule's metric, the
    /// oldest first): its window's value at each point and when it fired.
    pub fn backtest(&self, r: &Rule, points: &[Arc<Json>]) -> Json {
        let mut st = St::new(points.first().and_then(|p| p["t"].as_u64()).unwrap_or(0));
        let mut fired: Vec<Json> = Vec::new();
        let mut open: Option<u64> = None;
        let mut out = Vec::with_capacity(points.len());
        for (i, p) in points.iter().enumerate() {
            let t = p["t"].as_u64().unwrap_or(0);
            // The points up to this one.
            let v = window_value(r, &points[..=i], t, self.slo_p99_ms());
            out.push(json!({"t": t, "value": p[&r.metric], "window": v, "state": st.state}));
            match st.step(r, v, t) {
                Some("firing") => open = Some(t),
                Some(_) => {
                    if let Some(f) = open.take() {
                        fired.push(json!({"from_ms": f, "to_ms": t}));
                    }
                }
                None => {}
            }
            if let Some(last) = out.last_mut() {
                last["state"] = json!(st.state);
            }
        }
        if let Some(f) = open {
            fired.push(json!({"from_ms": f, "to_ms": Json::Null}));
        }
        json!({"points": out, "fired": fired})
    }
}

fn push_note(st: &mut St, by: &str, t: u64, text: String) {
    if st.notes.len() == NOTES {
        st.notes.remove(0);
    }
    st.notes.push(Note { by: by.into(), at_ms: t, text });
}

/// Whether a copy in `/v1/cluster` answers.
fn answers(r: &Json) -> bool {
    r["status"] != "down" && r["role"] != "unknown"
}

/// The value of rule `r`'s window ending at `t` (from `t - window_s` to
/// `t`) over `points` (the oldest first): `None` when no point of the
/// window has the metric. While the series spans less than the window (a
/// server just started), over the points there are. The mean of
/// `error_ratio` and `availability` weighs each point by its queries.
pub fn window_value(r: &Rule, points: &[Arc<Json>], t: u64, slo_p99_ms: f64) -> Option<f64> {
    window_of(r, points, t, slo_p99_ms).0
}

/// How long a series must span before a window it does not cover yet is
/// judged on the points there are (or half the window, when shorter), but
/// for a rule that fires on any one point (`max` over `>`, `min` over `<`).
const PARTIAL_MIN_MS: u64 = 30_000;

/// One key's calls in a window: its name, how many, how many failed, and
/// the key's owner.
#[derive(Clone, Debug, Default)]
pub struct KeyCalls {
    pub key: String,
    pub calls: u64,
    pub failed: u64,
    /// The milliseconds its calls ran within the window, summed.
    pub ms: f64,
    pub owner: Option<String>,
}

/// The metrics counted from each key's calls of a window, not kept as
/// points ([`Alerts::check_with`]).
pub const KEY_METRICS: [&str; 2] = ["key_error_ratio", "key_busy_ratio"];

/// The share of a window's query time over which a key holds the most of it.
const KEY_BUSY_SHARE: f64 = 0.5;

/// `key_busy_ratio` over a window's calls (`span_ms` long): the queries a
/// key ran at once on average (its calls' time in the window over its
/// span), per core of this server, for a key that took over half of
/// the query time while another key made [`KEY_MIN_CALLS`] calls or more
/// (a client alone crowds out no one; a few long queries count as much as
/// many short ones); 0 when none does. The key that
/// passes the rule is its place, with its share, the queries it ran at
/// once and its owner.
fn key_busy(r: &Rule, firing: bool, calls: &[KeyCalls], span_ms: u64, cores: usize) -> (Option<f64>, Vec<Json>) {
    let named = |k: &&KeyCalls| k.key != "-" && k.key != "(other keys)";
    // No calls (a server just restarted keeps none): no data.
    let Some(top) = calls.iter().filter(named).max_by(|a, b| a.ms.total_cmp(&b.ms)) else { return (None, Vec::new()) };
    let total: f64 = calls.iter().map(|k| k.ms).sum();
    let share = if total > 0.0 { top.ms / total } else { 0.0 };
    let others = calls.iter().filter(named).any(|k| k.key != top.key && k.calls >= KEY_MIN_CALLS);
    // Once it fires, crowding every other key out keeps it firing.
    if share <= KEY_BUSY_SHARE || !(others || firing) {
        return (Some(0.0), Vec::new());
    }
    let running = top.ms / span_ms.max(1) as f64;
    let v = running / cores.max(1) as f64;
    let place = if r.holds(v) || (firing && r.still(v)) {
        vec![json!({"key": top.key, "name": format!("key {}", top.key), "short": format!("key {}", top.key), "state": "firing", "value": v, "calls": top.calls, "share": share, "running": running, "cores": cores, "owner": top.owner})]
    } else {
        Vec::new()
    };
    (Some(v), place)
}

/// The fewest calls a key makes in a window for `key_error_ratio` to judge
/// its failures: two failures of three calls are not a broken client.
pub const KEY_MIN_CALLS: u64 = 30;

/// `key_error_ratio` over a window's calls: the worst share of failed
/// calls of a key with [`KEY_MIN_CALLS`] or more (0 when no key fails, not
/// no data: a quiet server has no failing client), and the keys that pass
/// the rule (that still pass it, for a firing one), the worst first.
fn key_window(r: &Rule, firing: bool, calls: &[KeyCalls]) -> (Option<f64>, Vec<Json>) {
    // No key with calls enough to judge (a server just restarted keeps no
    // calls): no data, which resolves nothing.
    let mut worst: Option<f64> = None;
    let mut place: Vec<(f64, Json)> = Vec::new();
    let total: u64 = calls.iter().map(|k| k.calls).sum();
    for k in calls.iter().filter(|k| k.calls >= KEY_MIN_CALLS && k.key != "-" && k.key != "(other keys)") {
        let ratio = k.failed as f64 / k.calls as f64;
        worst = Some(worst.map_or(ratio, |w| w.max(ratio)));
        if r.holds(ratio) || (firing && r.still(ratio)) {
            let share = if total > 0 { k.calls as f64 / total as f64 } else { 0.0 };
            place.push((ratio, json!({"key": k.key, "name": format!("key {}", k.key), "short": format!("key {}", k.key), "state": "firing", "value": ratio, "calls": k.calls, "failed": k.failed, "share": share, "owner": k.owner})));
        }
    }
    place.sort_by(|a, b| b.0.total_cmp(&a.0));
    (worst, place.into_iter().take(20).map(|(_, j)| j).collect())
}

/// A key's alert goes to its owner: the route named as the worst key's
/// owner (`mobile-team`, or `Mobile team` as `mobile-team`), when there is
/// one; the message names the owner either way.
fn owner_route(e: &mut Event, routes: &[Route]) {
    let Some(owner) = e.place.first().and_then(|w| w["owner"].as_str()).filter(|o| !o.is_empty()).map(String::from) else { return };
    let slug: String = owner.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' }).collect();
    if let Some(r) = routes.iter().find(|r| r.name.eq_ignore_ascii_case(owner.trim()) || r.name.eq_ignore_ascii_case(&slug)) {
        e.route = r.name.clone();
    }
    e.owner = owner;
}

/// Metrics a window makes of others, which no point holds as such: the
/// share of HTTP requests that failed on the server's side, a replica's
/// lag in seconds (transactions behind over the primary's write rate), the
/// hours until the disk is full at the window's rate, the worst key's
/// share of failed calls and the busiest key's queries at once per core
/// ([`Alerts::check_with`]).
pub const DERIVED: [&str; 5] = ["http_5xx_ratio", "replica_behind_s", "disk_hours_to_full", "key_error_ratio", "key_busy_ratio"];

/// What `disk_hours_to_full` is when the disk does not fill: ten years.
pub const NOT_FILLING_H: f64 = 87_600.0;
/// The most seconds `replica_behind_s` says, a day.
const STUCK_S: f64 = 86_400.0;

/// A latency under this has not risen in a way anyone notices: a rule
/// `vs: "before"` over a latency (`p99_ms`, `write_p99_ms` ...) judges a
/// window whose own value is under it as unchanged (1), as Performance's
/// "p99 rose" asks for over 20 ms too. A copy's p99 that goes from 1 ms to
/// 2 ms doubled, and nobody waits for it (LIV-3).
pub const JUMP_MIN_MS: f64 = 20.0;

/// The latency under which a rule `vs: "before"` over `metric` judges a
/// window unchanged: [`JUMP_MIN_MS`], and for the query p99 half its
/// service level objective (`--slo-p99-ms`) too. A p99 that went from 26
/// ms to 54 ms at 4 queries a second doubled, against an objective of 250
/// ms, and nobody is to be paged for it (CCW-19).
pub fn jump_floor(metric: &str, slo_p99_ms: f64) -> f64 {
    match metric {
        "p99_ms" => JUMP_MIN_MS.max(slo_p99_ms / 2.0),
        _ => JUMP_MIN_MS,
    }
}
/// The fewest requests (or queries) of a window whose share that failed
/// (`http_5xx_ratio`, `error_ratio`, `availability`) is judged, unless
/// [`RATIO_MIN_FAILED`] of them failed: below both, the window counts as
/// none failed. One 5xx of 90 requests on a nearly idle copy is not 1% of
/// its traffic failing (LIV-3).
pub const RATIO_MIN_COUNT: f64 = 100.0;
/// The failures of a window that are judged as a share however few the
/// requests: ten of twelve failing is a server that fails.
pub const RATIO_MIN_FAILED: f64 = 10.0;

/// A share of failures `n` of `d`, judged only over [`RATIO_MIN_COUNT`]
/// requests or [`RATIO_MIN_FAILED`] failures (0 below both); `None` when
/// there were none.
fn judged_share(n: f64, d: f64) -> Option<f64> {
    if d <= 0.0 {
        return None;
    }
    Some(if d < RATIO_MIN_COUNT && n < RATIO_MIN_FAILED { 0.0 } else { n / d })
}

/// The transactions behind of the replica furthest behind in point `p`:
/// of those that apply the primary's log (`replica_behind_applying`: not
/// one being sent a whole copy, nor one that has not answered since the
/// primary started, whose number the primary does not know yet), else of
/// every replica (a server before it reported that).
fn behind_of(p: &Json) -> Option<f64> {
    p["replica_behind_applying"].as_f64().or_else(|| p["replica_behind_max"].as_f64())
}

/// `replica_behind_s` at `points[i]`: how long the replica furthest behind
/// has lacked a transaction the primary committed. It holds `seq_no -
/// behind`, and the primary committed past that between the last point
/// whose `seq_no` was at most that and the one after it: the lag is from
/// the one after (a replica one transaction behind just after the
/// primary's commit is 0 s behind, not a day). A replica that lacks
/// what the primary committed before the series starts is at least as far
/// behind as the series is long. Points without `seq_no` (an older
/// primary): transactions behind over the write rate, and no value when
/// nothing is written, as it cannot tell (it said a day: LIV-3).
fn lag_at(points: &[Arc<Json>], i: usize) -> Option<f64> {
    let p = &points[i];
    let behind = behind_of(p)?;
    if behind <= 0.0 {
        return Some(0.0);
    }
    let at = |p: &Json| p["t"].as_u64().unwrap_or(0);
    let Some(seq) = p["seq_no"].as_f64() else {
        let w = p["writes_s"].as_f64().unwrap_or(0.0);
        return (w > 0.0).then(|| (behind / w).min(STUCK_S));
    };
    let held = seq - behind;
    let mut since = at(p);
    for q in points[..i].iter().rev() {
        match q["seq_no"].as_f64() {
            Some(s) if s > held => since = at(q),
            Some(_) => break,
            // A point without the number: the primary's role began after it.
            None => break,
        }
    }
    Some((at(p).saturating_sub(since) as f64 / 1e3).min(STUCK_S))
}

/// A point's 5xx answers a second that are the server's: a query whose
/// client's own deadline passed answers 504 too, and is the client's
/// (R2-shell-monitor-2), as the error ratio counts it.
fn server_5xx_s(p: &Json) -> f64 {
    (p["http_5xx_s"].as_f64().unwrap_or(0.0) - p["client_deadlines_s"].as_f64().unwrap_or(0.0)).max(0.0)
}

/// A point's value of `metric`, those [`DERIVED`] of others included.
pub fn value_of(p: &Json, metric: &str) -> Option<f64> {
    match metric {
        "http_5xx_ratio" => {
            let all = p["http_s"].as_f64().filter(|a| *a > 0.0)?;
            Some(server_5xx_s(p) / all)
        }
        // One point alone: transactions behind over the write rate, unknown
        // when nothing is written ([`lag_at`] reads the series).
        "replica_behind_s" => {
            let behind = behind_of(p)?;
            let w = p["writes_s"].as_f64().unwrap_or(0.0);
            if behind <= 0.0 {
                Some(0.0)
            } else {
                (w > 0.0).then(|| (behind / w).min(STUCK_S))
            }
        }
        _ => p.get(metric).and_then(Json::as_f64),
    }
}

/// [`window_value`], and how long the series spans (ms) when it spans
/// less than the window; no value while it spans less than
/// [`PARTIAL_MIN_MS`]. A rule `vs: "before"` takes the window's value over
/// the one of the window before it (both whole).
fn window_of(r: &Rule, points: &[Arc<Json>], t: u64, slo_p99_ms: f64) -> (Option<f64>, Option<u64>) {
    if r.vs == "before" {
        let w = r.window_s * 1000;
        let (now, partial) = window_own(r, points, t);
        let (before, partial_before) = window_own(r, points, t.saturating_sub(w));
        if partial.is_some() || partial_before.is_some() {
            return (None, partial);
        }
        // A latency under the floor now has not risen that matters.
        let floor = r.metric.ends_with("_ms") && now.is_some_and(|n| n < jump_floor(&r.metric, slo_p99_ms));
        return (now.zip(before.filter(|b| *b > 0.0)).map(|(n, b)| if floor { (n / b).min(1.0) } else { n / b }), None);
    }
    window_own(r, points, t)
}

fn window_own(r: &Rule, points: &[Arc<Json>], t: u64) -> (Option<f64>, Option<u64>) {
    let w = r.window_s * 1000;
    let from = t.saturating_sub(w);
    let at = |p: &Json| p["t"].as_u64().unwrap_or(0);
    let start = points.partition_point(|p| at(p) < from);
    let window: Vec<&Arc<Json>> = points[start..].iter().filter(|p| at(p) <= t).collect();
    let Some(first) = window.first() else { return (None, None) };
    // Nothing before the window, and its first point well into it.
    let span = t.saturating_sub(at(first));
    let partial = (start == 0 && span + (crate::telemetry::INTERVAL.as_millis() as u64) < w).then_some(span);
    // Too few points to judge a mean or "every point" yet: a server
    // restarted a moment ago (in a rolling upgrade, every server) would
    // fire on its first point. A rule that fires on any one point does.
    let any_point = matches!((r.eval.as_str(), r.op.starts_with('>')), ("max", true) | ("min", false));
    if !any_point && partial.is_some_and(|s| s < PARTIAL_MIN_MS.min(w / 2)) {
        return (None, partial);
    }
    // Shares of what failed weigh each point by what it counts, and judge
    // only a window with enough of it (judged_share).
    let counted = |num: &dyn Fn(&Json) -> f64, den: &dyn Fn(&Json) -> f64| -> Option<f64> {
        let (mut n, mut d) = (0.0, 0.0);
        for p in &window {
            n += num(p);
            d += den(p);
        }
        judged_share(n, d)
    };
    let secs = |p: &Json| p["secs"].as_f64().unwrap_or(crate::telemetry::INTERVAL.as_secs_f64());
    if r.eval == "mean" {
        match r.metric.as_str() {
            "error_ratio" | "availability" if window.iter().any(|p| p.get("queries").is_some()) => {
                let Some(ratio) = counted(&|p| p["query_errors"].as_f64().unwrap_or(0.0), &|p| p["queries"].as_f64().unwrap_or(0.0)) else { return (None, partial) };
                return (Some(if r.metric == "availability" { 1.0 - ratio } else { ratio }), partial);
            }
            "http_5xx_ratio" => return (counted(&|p| server_5xx_s(p) * secs(p), &|p| p["http_s"].as_f64().unwrap_or(0.0) * secs(p)), partial),
            _ => {}
        }
    }
    if r.metric == "replica_behind_s" {
        let vals: Vec<(u64, f64)> = (start..start + window.len()).filter_map(|i| lag_at(points, i).map(|v| (at(&points[i]), v))).collect();
        return (aggregate(r, &vals), partial);
    }
    if r.metric == "disk_hours_to_full" {
        // The window's slope (least squares) and its latest value, once the
        // points span a third of the window: a server that just started
        // (and wrote its files) is not a disk filling.
        let vals: Vec<(f64, f64)> = window.iter().filter_map(|p| p["disk_used_ratio"].as_f64().map(|v| (at(p) as f64 / 3.6e6, v))).collect();
        if vals.len() < 3 || partial.is_some_and(|s| s < w / 3) {
            return (None, partial);
        }
        let n = vals.len() as f64;
        let (mx, my) = (vals.iter().map(|v| v.0).sum::<f64>() / n, vals.iter().map(|v| v.1).sum::<f64>() / n);
        let (sxy, sxx) = vals.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + (x - mx) * (y - my), b + (x - mx) * (x - mx)));
        let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
        let last = vals.last().map_or(0.0, |v| v.1);
        return (Some(if slope > 1e-9 { ((1.0 - last).max(0.0) / slope).min(NOT_FILLING_H) } else { NOT_FILLING_H }), partial);
    }
    let vals: Vec<(u64, f64)> = window.iter().filter_map(|p| value_of(p, &r.metric).map(|v| (at(p), v))).collect();
    (aggregate(r, &vals), partial)
}

/// One value of a window's `(time, value)` pairs, by the rule's `eval`.
fn aggregate(r: &Rule, vals: &[(u64, f64)]) -> Option<f64> {
    let (first, last) = (vals.first()?, vals.last()?);
    let it = || vals.iter().map(|(_, v)| *v);
    Some(match r.eval.as_str() {
        "max" => it().fold(f64::MIN, f64::max),
        "min" => it().fold(f64::MAX, f64::min),
        "rate" => {
            let secs = (last.0.saturating_sub(first.0)) as f64 / 1e3;
            if secs <= 0.0 {
                return None;
            }
            (last.1 - first.1) / secs
        }
        // Every point meets the condition when the least extreme does.
        "all" if r.op.starts_with('>') => it().fold(f64::MAX, f64::min),
        "all" => it().fold(f64::MIN, f64::max),
        _ => it().sum::<f64>() / vals.len() as f64,
    })
}

/// Metrics that are 0 or 1: a message says "is on" or "is off".
pub const FLAG_METRICS: [&str; 4] = ["failing", "flush_failing", "merge_failing", "serves_reads"];

/// `v` as messages show it next to threshold `t`: four significant
/// digits, or as many more as it takes not to read as `t` (1.003 is not
/// "1 (> 1)").
fn number_vs(v: f64, t: f64) -> String {
    let mut s = number(v);
    if v == t || !v.is_finite() {
        return s;
    }
    for extra in 1..=6 {
        if s != number(t) {
            break;
        }
        s = number_digits(v, 4 + extra);
    }
    s
}

fn number_digits(v: f64, sig: i32) -> String {
    if v == 0.0 || !v.is_finite() {
        return format!("{v}");
    }
    let digits = (sig - 1 - v.abs().log10().floor() as i32).clamp(0, 10) as usize;
    let s = format!("{v:.digits$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// A number as messages show it: up to four significant digits.
fn number(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return format!("{v}");
    }
    let digits = (3 - v.abs().log10().floor() as i32).clamp(0, 6) as usize;
    let s = format!("{v:.digits$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// A duration as messages show it: `45 s`, `5 min`, `2 h 10 min`.
fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=119 => format!("{s} s"),
        120..=7199 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// Evaluates the alerts after a new point of the series: this server's
/// rules now, and on a coordinator its cluster at most every 5 seconds,
/// in a task of its own. Changes go to the events, the log store and the
/// routes.
pub fn tick(svc: &Arc<crate::service::Service>, point: &Json) {
    crate::contain::sweep(svc);
    let coordinator = matches!(svc.backend, crate::service::Backend::Cluster(_));
    svc.alerts.set_coordinator(coordinator);
    identify(svc);
    let t = point["t"].as_u64().unwrap_or_else(now_ms);
    let points = svc.series.since(t.saturating_sub(MAX_WINDOW_S * 1000));
    let changes = svc.alerts.check_with(&points, t, &|from, to| key_calls(svc, from, to));
    deliver(svc, changes);
    let now = now_ms();
    if coordinator && now.saturating_sub(svc.alerts.cluster_checked.load(Ordering::Relaxed)) >= CHECK_MS && !svc.alerts.cluster_running.swap(true, Ordering::AcqRel) {
        let svc = svc.clone();
        tokio::spawn(async move {
            check_cluster(&svc).await;
            svc.alerts.cluster_running.store(false, Ordering::Release);
        });
    }
}

/// Runs the queries of the rules with `sql` when [`SQL_EVERY_MS`] has
/// passed since the last runs, in a task of its own, one query after the
/// other; each one's first number becomes the value its rule's metric
/// (`sql_<id>`) has in the series' next points ([`Alerts::sql_values`]).
/// A query that fails, or answers no number, leaves its rule without
/// data, and says why in the rule's `sql_run`.
pub fn refresh_sql(svc: &Arc<crate::service::Service>) {
    let now = now_ms();
    if now.saturating_sub(svc.alerts.sql_checked.load(Ordering::Relaxed)) < SQL_EVERY_MS {
        return;
    }
    let rules: Vec<Rule> = svc.alerts.rules().into_iter().filter(|r| r.enabled && !r.sql.is_empty()).collect();
    if rules.is_empty() {
        let mut v = lock(&svc.alerts.sql_values);
        if !v.is_empty() {
            v.clear();
            lock(&svc.alerts.sql_runs).clear();
        }
        return;
    }
    if svc.alerts.sql_running.swap(true, Ordering::AcqRel) {
        return;
    }
    svc.alerts.sql_checked.store(now, Ordering::Relaxed);
    let svc = svc.clone();
    tokio::spawn(async move {
        let mut values = std::collections::BTreeMap::new();
        let mut runs = HashMap::new();
        for r in rules {
            let t0 = std::time::Instant::now();
            // `:since` and `:now`: the rule's window, as timestamps, for a
            // query over the rows of the last minutes.
            let t = now_ms() as i64;
            let at = |ms: i64| chloedb::value::Value::Str(chloedb::timestamp::format(ms * 1000));
            let params = chloedb::query::Params::new().set("since", at(t - r.window_s as i64 * 1000)).set("now", at(t));
            let req = crate::service::QueryReq { sql: r.sql.clone(), params, timeout_ms: SQL_TIMEOUT_MS, ..Default::default() };
            let (value, error) = match Box::pin(svc.query(req)).await {
                Ok(a) => match a.rs.rows.first().and_then(|row| row.iter().find_map(|v| v.as_f64())) {
                    Some(v) if v.is_finite() => (Some(v), None),
                    _ => (None, Some("the query answered no number".to_string())),
                },
                Err(e) => (None, Some(e.message)),
            };
            if let Some(v) = value {
                values.insert(sql_metric(&r.id), v);
            }
            runs.insert(r.id.clone(), SqlRun { at_ms: now_ms(), took_ms: t0.elapsed().as_secs_f64() * 1e3, value, error });
        }
        *lock(&svc.alerts.sql_values) = values;
        *lock(&svc.alerts.sql_runs) = runs;
        svc.alerts.sql_running.store(false, Ordering::Release);
    });
}

impl Alerts {
    /// The last values of the rules with `sql`, by metric, for the
    /// series' points ([`refresh_sql`]).
    pub fn sql_values(&self) -> std::collections::BTreeMap<String, f64> {
        lock(&self.sql_values).clone()
    }
}

/// Each key's calls of `[from, to)` on this server, with its owner.
fn key_calls(svc: &crate::service::Service, from: u64, to: u64) -> Vec<KeyCalls> {
    let calls = svc.query_log.statements.key_calls(from, to);
    if calls.is_empty() {
        return Vec::new();
    }
    let owners: HashMap<String, String> = svc.keys.list().into_iter().filter_map(|k| Some((k["name"].as_str()?.to_string(), k["owner"].as_str()?.to_string()))).collect();
    calls.into_iter().map(|(key, calls, failed, ms)| KeyCalls { owner: owners.get(&key).cloned(), key, calls, failed, ms }).collect()
}

/// A coordinator's check of its cluster now ([`Alerts::check_cluster`]);
/// then the copies that run other each_copy rules get its own
/// ([`push_rules`]).
pub async fn check_cluster(svc: &Arc<crate::service::Service>) {
    let snap = svc.cluster().await;
    let t = now_ms();
    svc.alerts.cluster_checked.store(t, Ordering::Relaxed);
    let changes = svc.alerts.check_cluster(&snap, t);
    deliver(svc, changes);
    let urls = svc.alerts.to_push(&snap, t);
    if !urls.is_empty() {
        push_rules(svc, Some(urls), "the coordinator").await;
    }
}

/// Names the cluster in notifications as the service does
/// (`--cluster-name`, `--public-url`).
pub fn identify(svc: &crate::service::Service) {
    svc.alerts.set_identity(svc.identity.name.clone(), svc.identity.public_url.clone());
}

/// Days the log store keeps alert events (its table `alerts`).
pub const KEEP_DAYS: u64 = 7;

/// Hands alert events to the server's events and log store, and posts
/// the resolutions that are told ([`Alerts::resolutions`]); then the
/// FIRING messages due now ([`send_due`]).
pub fn deliver(svc: &Arc<crate::service::Service>, changes: Vec<Arc<Event>>) {
    if !changes.is_empty() {
        identify(svc);
        for e in &changes {
            svc.events.add(event_of(e));
            if let Some(store) = &svc.log_store {
                store.keep_alert(e);
            }
        }
        let told = svc.alerts.resolutions(&changes);
        if !told.is_empty() {
            let svc = svc.clone();
            tokio::spawn(async move { svc.alerts.notify(&told).await });
        }
    }
    send_due(svc);
}

/// Posts the FIRING messages due now ([`Alerts::due`]).
pub fn send_due(svc: &Arc<crate::service::Service>) {
    let due = svc.alerts.due(now_ms());
    if !due.is_empty() {
        let svc = svc.clone();
        tokio::spawn(async move { svc.alerts.notify_to(&due).await });
    }
}

/// How many copies a coordinator gives its rules to at once, and how long
/// each has.
const PUSH_AT_ONCE: usize = 16;
const PUSH_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Gives the coordinator's each_copy rules to its copies (`only` those,
/// else every copy), over the call a coordinator asks its copies things
/// with (`Profile`, kind `alerts`). Answers `{copies, took, failed: [{copy,
/// short, error}], rules_ms}`: a copy that did not take them gets them
/// again when it answers ([`Alerts::to_push`]).
pub async fn push_rules(svc: &Arc<crate::service::Service>, only: Option<Vec<String>>, by: &str) -> Json {
    let crate::service::Backend::Cluster(c) = &svc.backend else { return Json::Null };
    let snap = c.snapshot().await;
    let mut copies: Vec<(String, String)> = Vec::new();
    for s in snap["shards"].as_array().into_iter().flatten() {
        for r in s["replicas"].as_array().into_iter().flatten() {
            let url = r["url"].as_str().unwrap_or_default().to_string();
            if only.as_ref().is_none_or(|o| o.contains(&url)) {
                copies.push((url, r["short"].as_str().unwrap_or_default().to_string()));
            }
        }
    }
    let (rules_ms, _) = svc.alerts.rules_version();
    let body = json!({"op": "rules", "rules": svc.alerts.copy_rules(), "rules_ms": rules_ms, "by": by});
    let t = now_ms();
    use futures::StreamExt;
    let results: Vec<(String, String, Result<Json, String>)> = futures::stream::iter(copies.into_iter().map(|(url, short)| {
        let body = body.clone();
        async move {
            let a = crate::profiling::Asked { kind: crate::profiling::Kind::Alerts, body, folded: false, in_use: false };
            let res = match tokio::time::timeout(PUSH_WAIT, c.profile(&url, a)).await {
                Err(_) => Err(format!("no answer in {} s", PUSH_WAIT.as_secs())),
                Ok(Err(e)) => Err(e.message),
                Ok(Ok(crate::profiling::Answer::Json(j))) => Ok(j),
                Ok(Ok(_)) => Err("the copy answered folded lines".into()),
            };
            (url, short, res)
        }
    }))
    .buffer_unordered(PUSH_AT_ONCE)
    .collect()
    .await;
    let mut failed = Vec::new();
    let mut took = 0;
    for (url, short, res) in &results {
        let res = match res {
            Ok(j) if j["took"] == true => Ok(()),
            Ok(j) => Err(j["why"].as_str().unwrap_or("the copy kept its own rules").to_string()),
            // A copy of an older version knows no such call.
            Err(e) if e.contains("no profile of kind") => Err("the copy runs an older chloedb-server that takes no rules from its coordinator".to_string()),
            Err(e) => Err(e.clone()),
        };
        match &res {
            Ok(()) => took += 1,
            Err(e) => failed.push(json!({"copy": url, "short": short, "error": e})),
        }
        svc.alerts.pushed_to(url, t, res);
    }
    if !failed.is_empty() {
        tracing::warn!(failed = failed.len(), of = results.len(), "copies did not take the coordinator's alert rules");
    }
    json!({"copies": results.len(), "took": took, "failed": failed, "rules_ms": rules_ms})
}

/// What a copy does when its coordinator asks (`Profile`, kind `alerts`):
/// `{"op": "rules", "rules", "rules_ms", "by"}` takes its each_copy rules;
/// `{"op": "backtest", ...}` replays a rule over its own points
/// ([`backtest`]).
pub async fn copy_call(svc: &Arc<crate::service::Service>, b: &Json) -> Result<Json, ApiError> {
    match b["op"].as_str() {
        Some("rules") => {
            let rules: Vec<Rule> = serde_json::from_value(b["rules"].clone()).map_err(|e| ApiError::bad(format!("bad rules: {e}")))?;
            let by = b["by"].as_str().unwrap_or("a coordinator");
            let at = b["rules_ms"].as_u64().unwrap_or(0);
            let before = svc.alerts.rules();
            let j = svc.alerts.set_copy_rules(rules, by, at)?;
            if j["took"] == true && before != svc.alerts.rules() {
                tracing::info!(by, rules_ms = at, "took the alert rules of a coordinator");
            }
            Ok(j)
        }
        Some("backtest") => backtest(svc, b).await,
        other => Err(ApiError::bad(format!("no alerts call {other:?}: rules or backtest"))),
    }
}

/// The coordinators' exchange, in a task of its own once a coordinator
/// serves HTTP: who the coordinators are (every 5 s, [`crate::peers`]),
/// what this one tells them (as it happens: acknowledgements, silences,
/// notes, pages, rules and routes), and what each holds (every 30 s, and
/// when one is first heard), merged ([`Alerts::merge`]). A coordinator
/// that missed a word (it was down, it just started) gets it so.
/// How often a coordinator reads each other one's alerts whole.
const PULL_MS: u64 = 10_000;

pub fn peering(svc: &Arc<crate::service::Service>) {
    if svc.alerts.peering.swap(true, Ordering::AcqRel) || tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let weak = Arc::downgrade(svc);
    tokio::spawn(async move {
        let mut listed = 0u64;
        let mut pulled: HashMap<String, u64> = HashMap::new();
        // Since when a coordinator does not answer this one: it is not
        // heard from, whatever the list says ([`Alerts::pager`]).
        let mut failing: HashMap<String, u64> = HashMap::new();
        loop {
            let Some(ready) = weak.upgrade().map(|s| s.alerts.outbox_ready.clone()) else { return };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), ready.notified()).await;
            let Some(svc) = weak.upgrade() else { return };
            let app = &*svc;
            if !svc.alerts.is_coordinator() {
                continue;
            }
            let now = now_ms();
            if now.saturating_sub(listed) >= 5_000 {
                svc.alerts.set_peers(crate::peers::coordinators(app));
                listed = now;
            }
            let others: Vec<crate::peers::Peer> = lock(&svc.alerts.peers).iter().filter(|p| !p.this).cloned().collect();
            let mut heard = |url: &str, ok: bool| {
                if ok {
                    failing.remove(url);
                } else {
                    failing.entry(url.to_string()).or_insert(now);
                }
            };
            let ops = std::mem::take(&mut *lock(&svc.alerts.outbox));
            for op in &ops {
                let sent = futures::future::join_all(others.iter().map(|p| crate::peers::post(app, p, "/v1/alerts/peer", op))).await;
                for (p, r) in others.iter().zip(sent) {
                    heard(&p.url, r.is_ok());
                    if let Err(e) = r {
                        tracing::warn!(peer = %p.url, error = %e, "could not tell another coordinator of an alert");
                    }
                }
            }
            for p in &others {
                if pulled.get(&p.url).is_some_and(|at| now.saturating_sub(*at) < PULL_MS) {
                    continue;
                }
                pulled.insert(p.url.clone(), now);
                match crate::peers::get(app, p, "/v1/alerts/peer").await {
                    Ok(j) => {
                        heard(&p.url, true);
                        match svc.alerts.merge(&j, &p.url) {
                            Ok(ev) => deliver(&svc, ev),
                            Err(e) => tracing::warn!(peer = %p.url, error = %e.message, "could not merge another coordinator's alerts"),
                        }
                    }
                    Err(e) => {
                        heard(&p.url, false);
                        tracing::warn!(peer = %p.url, error = %e, "could not read another coordinator's alerts");
                    }
                }
            }
            pulled.retain(|u, _| others.iter().any(|p| &p.url == u));
            failing.retain(|u, _| others.iter().any(|p| &p.url == u));
            for p in lock(&svc.alerts.peers).iter_mut().filter(|p| !p.this) {
                if let Some(f) = failing.get(&p.url) {
                    p.last_seen_ms = p.last_seen_ms.min(*f);
                }
            }
        }
    });
}

/// An alert's change as one of the server's events ([`crate::events`]):
/// `alert_fired`, `alert_resolved`, `alert_acked`, `alert_silenced` (a
/// silence lifted too), or a `note`; where: the rule, and the copy or
/// shard when one is named.
pub fn event_of(e: &Event) -> crate::events::Event {
    let kind = match e.state {
        "firing" => "alert_fired",
        "resolved" => "alert_resolved",
        "acked" => "alert_acked",
        "silenced" | "unsilenced" => "alert_silenced",
        _ => "note",
    };
    let severity = if e.state == "firing" { e.severity.clone() } else { "info".to_string() };
    let by = e.by.clone().unwrap_or_else(|| "someone".into());
    let place = if e.where_.is_empty() { String::new() } else { format!(" on {}", e.where_.join(", ")) };
    // Enough digits to differ from the threshold (NEW-13).
    let value = e.value.map_or_else(|| "no data".to_string(), |v| number_vs(v, e.threshold));
    let text = e.text.as_ref().map(|t| format!(": {t}")).unwrap_or_default();
    // A rule against the window before has a ratio, not the metric's
    // own value: "p99 4.3x its baseline", never "p99_ms = 4.3".
    let ratio = e.vs == "before";
    let short = e.metric.strip_suffix("_ms").unwrap_or(&e.metric);
    let title = match e.state {
        "firing" if ratio => format!(
            "{} fires{place}: {short} {}x its baseline ({} {}x, {} over {} against the {} before)",
            e.name,
            e.value.map_or_else(|| "no data".to_string(), |v| number_vs(v, e.threshold)),
            e.op,
            number(e.threshold),
            e.eval,
            duration(e.window_s * 1000),
            duration(e.window_s * 1000)
        ),
        "resolved" if ratio => {
            let after = e.fired_ms.map(|f| format!(" after {}", duration(e.ts_ms.saturating_sub(f)))).unwrap_or_default();
            let why = e.why.as_ref().map(|w| format!(" ({w}{})", e.by.as_ref().map(|b| format!(" by {b}")).unwrap_or_default())).unwrap_or_default();
            format!("{} resolved{place}{after}: {short} {}x its baseline{why}", e.name, e.value.map_or_else(|| "no data".to_string(), |v| number_vs(v, e.threshold)))
        }
        "firing" => format!("{} fires{place}: {} = {value} ({} {}, {} over {})", e.name, e.metric, e.op, number(e.threshold), e.eval, duration(e.window_s * 1000)),
        "resolved" => {
            let after = e.fired_ms.map(|f| format!(" after {}", duration(e.ts_ms.saturating_sub(f)))).unwrap_or_default();
            let why = e.why.as_ref().map(|w| format!(" ({w}{})", e.by.as_ref().map(|b| format!(" by {b}")).unwrap_or_default())).unwrap_or_default();
            format!("{} resolved{place}{after}: {} = {value}{why}", e.name, e.metric)
        }
        "acked" => format!("{by} acknowledged {}{text}", e.name),
        "silenced" => format!("{by} silenced {} {}", e.name, e.text.as_deref().unwrap_or_default()),
        "unsilenced" => format!("{by} lifted the silence of {}", e.name),
        _ => format!("{by} on {}{text}", e.name),
    };
    // The copy or shard when it is one; the copies when several.
    let mut where_ = json!({"rule": e.rule});
    let distinct = |k: &str| {
        let mut v: Vec<&Json> = Vec::new();
        for x in e.place.iter().map(|w| &w[k]).filter(|x| !x.is_null()) {
            if !v.contains(&x) {
                v.push(x);
            }
        }
        v
    };
    if let [shard] = distinct("shard")[..] {
        where_["shard"] = shard.clone();
    }
    match distinct("copy")[..] {
        [] => {}
        [copy] => where_["copy"] = copy.clone(),
        ref copies => where_["copies"] = json!(copies),
    }
    crate::events::Event { kind: kind.into(), severity, title, detail: serde_json::to_value(e).unwrap_or(Json::Null), where_, who: e.by.clone() }
}

/// The rules an edit changed, before and after, by id (`null` where it
/// did not exist), for the audit entry.
pub fn changed(before: &[Rule], after: &[Rule]) -> (Json, Json) {
    let (mut b, mut a) = (serde_json::Map::new(), serde_json::Map::new());
    let ids: Vec<&String> = before.iter().map(|r| &r.id).chain(after.iter().map(|r| &r.id)).collect();
    for id in ids {
        let (x, y) = (before.iter().find(|r| &r.id == id), after.iter().find(|r| &r.id == id));
        if x != y && !b.contains_key(id) {
            b.insert(id.clone(), json!(x));
            a.insert(id.clone(), json!(y));
        }
    }
    (Json::Object(b), Json::Object(a))
}

/// What [`changed`] says, in words: "added x; edited y; removed z".
pub fn change_text(before: &Json, after: &Json) -> String {
    let ids = |f: &dyn Fn(&Json, &Json) -> bool| -> Vec<String> { before.as_object().into_iter().flatten().filter(|(k, b)| f(b, &after[k.as_str()])).map(|(k, _)| k.clone()).collect() };
    let parts: Vec<String> = [("added", ids(&|b, a| b.is_null() && !a.is_null())), ("edited", ids(&|b, a| !b.is_null() && !a.is_null())), ("removed", ids(&|b, a| !b.is_null() && a.is_null()))]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(w, v)| format!("{w} {}", v.join(", ")))
        .collect();
    if parts.is_empty() {
        "no rule changed".into()
    } else {
        parts.join("; ")
    }
}

/// The metrics the log store's `load` table keeps, a row a minute: a
/// backtest past memory's hour replays only these.
pub const LOAD_METRICS: [&str; 14] = ["qps", "cpu", "busy", "p99_ms", "p50_ms", "disk_used_ratio", "error_ratio", "availability", "errors_s", "client_errors_s", "timeouts_s", "rejected_s", "writes_s", "mem_bytes"];

/// Replays a rule (`{"rule", "from", "to"}`, 7 days at most) over this
/// server's points: memory's when it holds `from`, else a point a minute
/// from the log store's `load` table.
pub async fn backtest(svc: &Arc<crate::service::Service>, b: &Json) -> Result<Json, ApiError> {
    // A rule, or the id of one of this server's rules.
    let mut r = match &b["rule"] {
        Json::String(id) => {
            let rule = svc.alerts.rules().into_iter().find(|r| &r.id == id).ok_or_else(|| ApiError::bad(format!("no rule {id:?}: \"rule\" is a rule's id (GET /v1/alerts/rules lists them) or a rule")))?;
            serde_json::to_value(rule).map_err(|e| ApiError::bad(e.to_string()))?
        }
        r if r.is_object() => r.clone(),
        _ => return Err(ApiError::bad("the body needs \"rule\": a rule, or the id of one")),
    };
    for k in ["id", "name"] {
        if r[k].is_null() {
            r[k] = json!("backtest");
        }
    }
    let rule: Rule = serde_json::from_value(r).map_err(|e| ApiError::bad(format!("bad rule: {e}")))?;
    rule.check()?;
    let now = now_ms();
    let to = time_ms(&b["to"], "to")?.unwrap_or(now).min(now);
    let asked = time_ms(&b["from"], "from")?;
    let mut from = asked.unwrap_or(to.saturating_sub(86_400_000)).max(to.saturating_sub(KEEP_DAYS * 86_400_000));
    if from >= to {
        return Err(ApiError::bad("from comes before to"));
    }
    if KEY_METRICS.contains(&rule.metric.as_str()) {
        return Err(ApiError::bad(format!("{} is counted from each key's calls a minute at a time, not kept as points: it cannot be replayed", rule.metric)));
    }
    // A coordinator replays the cluster's rules from its events, and has
    // each copy replay an each_copy rule over the copy's own points.
    if svc.alerts.is_coordinator() && CLUSTER_METRICS.contains(&rule.metric.as_str()) {
        return Ok(backtest_events(svc, &rule, from, to));
    }
    if svc.alerts.is_coordinator() && rule.copies() {
        return backtest_copies(svc, &rule, from, to).await;
    }
    let memory = svc.series.since(0);
    let held = memory.first().and_then(|p| p["t"].as_u64());
    // With no log store, memory is all there is: the backtest covers what
    // it holds, and says so, rather than refusing the default day.
    let mut note = None;
    if svc.log_store.is_none() {
        let start = held.unwrap_or(to);
        if start > from {
            note = Some(format!(
                "this server keeps no log store, and memory holds points from {} on: the backtest covers {} rather than the {} asked{}",
                chloedb::timestamp::format(start as i64 * 1000),
                duration(to.saturating_sub(start)),
                duration(to.saturating_sub(from)),
                if asked.is_none() { " by default" } else { "" }
            ));
            from = start.min(to.saturating_sub(1));
        }
    }
    let lead = from.saturating_sub(rule.window_s * 1000 + rule.for_s * 1000);
    // Memory when it holds the time asked, or the time asked is within
    // the last hour (a server up a few minutes holds what there is), or
    // the load table does not keep the metric: the answer's covered_from
    // says from when.
    let in_memory = |h: u64| h <= from || from.saturating_add(3_600_000) >= now || !LOAD_METRICS.contains(&rule.metric.as_str());
    let (points, source) = match held {
        _ if svc.log_store.is_none() => (memory.into_iter().filter(|p| p["t"].as_u64().is_some_and(|t| t >= lead && t <= to)).collect::<Vec<_>>(), "memory"),
        Some(h) if in_memory(h) => (memory.into_iter().filter(|p| p["t"].as_u64().is_some_and(|t| t >= lead && t <= to)).collect::<Vec<_>>(), "memory"),
        _ => {
            let store = svc.log_store.clone().ok_or_else(|| ApiError { code: None, status: 409, message: "memory holds the last hour, and this server keeps no log store for older points".into() })?;
            if !LOAD_METRICS.contains(&rule.metric.as_str()) {
                return Err(ApiError::bad(format!("the log store's load table keeps {} a minute, not {}: a backtest past the last hour replays only those", LOAD_METRICS.join(", "), rule.metric)));
            }
            let col = match rule.metric.as_str() {
                "availability" => "error_ratio",
                // Made of the disk's points ([`window_of`]).
                "disk_hours_to_full" => "disk_used_ratio",
                m => m,
            };
            let mut points = Vec::new();
            // A day a query: a day's minutes are under the store's rows.
            let mut lo = lead;
            while lo < to {
                let hi = (lo + 86_400_000).min(to + 1);
                let params = chloedb::query::Params::new().set("lo", chloedb::value::Value::Timestamp(lo as i64 * 1000)).set("hi", chloedb::value::Value::Timestamp(hi as i64 * 1000));
                let sql = format!("SELECT ts, {col} FROM load WHERE ts >= :lo AND ts < :hi ORDER BY ts LIMIT 5000");
                let (rs, _) = store.query(&sql, &params, false, std::time::Duration::from_secs(10)).await?;
                for row in &rs.rows {
                    let (Some(chloedb::value::Value::Timestamp(ts)), Some(v)) = (row.first(), row.get(1).and_then(|v| v.as_f64())) else { continue };
                    let v = if rule.metric == "availability" { 1.0 - v } else { v };
                    let mut p = json!({"t": (*ts / 1000) as u64});
                    p[col] = json!(v);
                    if col != rule.metric {
                        p[rule.metric.as_str()] = json!(v);
                    }
                    points.push(Arc::new(p));
                }
                lo = hi;
            }
            (points, "load")
        }
    };
    let mut j = svc.alerts.backtest(&rule, &points);
    if let Some(ps) = j["points"].as_array_mut() {
        ps.retain(|p| p["t"].as_u64().is_some_and(|t| t >= from));
    }
    // What the points cover of the time asked: a load table a few
    // minutes old covers a few minutes of 7 days.
    let first = j["points"].as_array().and_then(|ps| ps.iter().find(|p| !p["window"].is_null() || !p["value"].is_null())).and_then(|p| p["t"].as_u64());
    j["covered_from"] = json!(first);
    j["source"] = json!(source);
    j["from"] = json!(from);
    j["to"] = json!(to);
    j["metric"] = json!(rule.metric);
    if let Some(n) = note {
        j["note"] = json!(n);
    }
    Ok(j)
}

/// A backtest's `from` or `to`: epoch milliseconds (seconds when under
/// 10^11, which as milliseconds is 1973), as a number or digits, or ISO
/// 8601 text (`2026-11-13T00:30:00Z`); anything else is refused.
fn time_ms(v: &Json, k: &str) -> Result<Option<u64>, ApiError> {
    let ms = |n: u64| if n < 100_000_000_000 { n * 1000 } else { n };
    let bad = || ApiError::bad(format!("\"{k}\" is epoch milliseconds or ISO 8601 time (\"2026-11-13T00:30:00Z\"), not {v}"));
    match v {
        Json::Null => Ok(None),
        Json::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)).map(|n| Some(ms(n))).ok_or_else(bad),
        Json::String(s) if !s.trim().is_empty() && s.trim().bytes().all(|c| c.is_ascii_digit()) => s.trim().parse::<u64>().map(|n| Some(ms(n))).map_err(|_| bad()),
        Json::String(s) => chloedb::timestamp::parse(s).filter(|us| *us >= 0).map(|us| Some(us as u64 / 1000)).ok_or_else(bad),
        _ => Err(bad()),
    }
}

/// How many points of each copy a coordinator's backtest answers.
const COPY_POINTS: usize = 240;

/// An each_copy rule replayed by each copy over its own points (memory,
/// or its load table), 16 at a time: when it would have fired on each,
/// and the points of the copies it fired on most (or the highest), at most
/// [`COPY_POINTS`] each; `fired` is when it fired on any.
async fn backtest_copies(svc: &Arc<crate::service::Service>, rule: &Rule, from: u64, to: u64) -> Result<Json, ApiError> {
    let crate::service::Backend::Cluster(c) = &svc.backend else { return Err(ApiError::bad("not a coordinator")) };
    let snap = c.snapshot().await;
    let mut copies: Vec<(String, String, u64)> = Vec::new();
    for s in snap["shards"].as_array().into_iter().flatten() {
        for r in s["replicas"].as_array().into_iter().flatten().filter(|r| answers(r)) {
            copies.push((r["url"].as_str().unwrap_or_default().into(), r["short"].as_str().unwrap_or_default().into(), s["shard"].as_u64().unwrap_or(0)));
        }
    }
    let body = json!({"op": "backtest", "rule": rule, "from": from, "to": to});
    use futures::StreamExt;
    let answers: Vec<Json> = futures::stream::iter(copies.into_iter().map(|(url, short, shard)| {
        let body = body.clone();
        async move {
            let a = crate::profiling::Asked { kind: crate::profiling::Kind::Alerts, body, folded: false, in_use: false };
            let mut j = match tokio::time::timeout(std::time::Duration::from_secs(10), c.profile(&url, a)).await {
                Ok(Ok(crate::profiling::Answer::Json(j))) => j,
                Ok(Ok(_)) => json!({"error": "the copy answered folded lines"}),
                Ok(Err(e)) => json!({"error": e.message}),
                Err(_) => json!({"error": "no answer in 10 s"}),
            };
            (j["copy"], j["short"], j["shard"]) = (json!(url), json!(short), json!(shard));
            j
        }
    }))
    .buffer_unordered(PUSH_AT_ONCE)
    .collect()
    .await;
    let above = rule.op.starts_with('>');
    let peak = |j: &Json| j["points"].as_array().into_iter().flatten().filter_map(|p| p["window"].as_f64()).fold(if above { f64::MIN } else { f64::MAX }, |a, b| if above { a.max(b) } else { a.min(b) });
    let mut ranked: Vec<(usize, f64, usize)> = answers.iter().enumerate().filter(|(_, j)| j.get("error").is_none()).map(|(i, j)| (j["fired"].as_array().map_or(0, Vec::len), peak(j), i)).collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(if above { b.1.total_cmp(&a.1) } else { a.1.total_cmp(&b.1) }));
    let shown: Vec<usize> = ranked.iter().take(6).map(|r| r.2).collect();
    let mut spans: Vec<(u64, Option<u64>)> = answers.iter().flat_map(|j| j["fired"].as_array().cloned().unwrap_or_default()).filter_map(|f| Some((f["from_ms"].as_u64()?, f["to_ms"].as_u64()))).collect();
    spans.sort();
    let mut fired: Vec<(u64, Option<u64>)> = Vec::new();
    for (a, b) in spans {
        match fired.last_mut() {
            Some((_, end)) if end.is_none_or(|e| e >= a) => {
                *end = match (*end, b) {
                    (Some(x), Some(y)) => Some(x.max(y)),
                    _ => None,
                }
            }
            _ => fired.push((a, b)),
        }
    }
    let thin = |ps: &[Json]| -> Vec<Json> {
        let step = ps.len().div_ceil(COPY_POINTS).max(1);
        ps.iter().step_by(step).map(|p| json!({"t": p["t"], "window": p["window"]})).collect()
    };
    let out: Vec<Json> = answers
        .iter()
        .enumerate()
        .map(|(i, j)| {
            let mut o = json!({"copy": j["copy"], "short": j["short"], "shard": j["shard"], "fired": j["fired"], "source": j["source"], "covered_from": j["covered_from"]});
            if let Some(e) = j.get("error") {
                o["error"] = e.clone();
            } else if shown.contains(&i) {
                o["points"] = json!(thin(j["points"].as_array().map_or(&[][..], |v| v.as_slice())));
            }
            o
        })
        .collect();
    let covered = answers.iter().filter_map(|j| j["covered_from"].as_u64()).min();
    let worst = shown.first().map(|&i| thin(answers[i]["points"].as_array().map_or(&[][..], |v| v.as_slice()))).unwrap_or_default();
    Ok(json!({
        "copies": out, "points": worst, "fired": fired.iter().map(|(a, b)| json!({"from_ms": a, "to_ms": b})).collect::<Vec<_>>(),
        "source": "copies", "from": from, "to": to, "metric": rule.metric, "covered_from": covered,
        "answered": answers.iter().filter(|j| j.get("error").is_none()).count(), "asked": answers.len(),
    }))
}

/// A rule over the cluster's state replayed from this coordinator's
/// events in memory: a shard that lost and got back its majority, a copy
/// down and up, a primary that changed.
fn backtest_events(svc: &crate::service::Service, rule: &Rule, from: u64, to: u64) -> Json {
    let (kinds, ends): (&[&str], &[&str]) = match rule.metric.as_str() {
        "shards_no_majority" => (&["majority_lost"], &["majority_back"]),
        "copies_down" => (&["copy_down"], &["copy_up", "copy_removed"]),
        "primary_changes" => (&["primary_changed"], &[]),
        _ => (&[], &[]),
    };
    let held = svc.events.held_from();
    let all: Vec<String> = kinds.iter().chain(ends.iter()).map(|k| k.to_string()).collect();
    let (mut evs, _) = svc.events.range(from.saturating_sub(PRIMARY_CHANGED_MS), to, &all, 20_000);
    evs.reverse();
    // What each names: a shard, a copy.
    let key = |e: &crate::events::Recorded| e.event.where_.get("copy").or_else(|| e.event.where_.get("shard")).map(|v| v.to_string()).unwrap_or_default();
    let mut open: HashMap<String, u64> = HashMap::new();
    let mut points: Vec<Json> = Vec::new();
    let mut spans: Vec<(u64, Option<u64>)> = Vec::new();
    let mut fired_at: Option<u64> = None;
    let mut changes: VecDeque<u64> = VecDeque::new();
    let value_at = |open: &HashMap<String, u64>, changes: &VecDeque<u64>, t: u64| -> f64 {
        if rule.metric == "primary_changes" {
            changes.iter().filter(|c| **c + PRIMARY_CHANGED_MS > t).count() as f64
        } else {
            open.values().filter(|s| t.saturating_sub(**s) >= rule.for_s * 1000).count() as f64
        }
    };
    // What fires now since before the events held (a restarted coordinator
    // has its alerts from its state file, not their events): open from then.
    let mut seeded = None;
    if rule.metric != "primary_changes" {
        if let Some(j) = svc.alerts.one(&rule.id).ok().filter(|j| j["state"] == "firing") {
            let since = j["since_ms"].as_u64().unwrap_or(held);
            if since < held {
                for w in j["where"].as_array().into_iter().flatten() {
                    // Events name a copy by its short name, a shard by its number.
                    if let Some(k) = if w.get("copy").is_some() { w.get("short") } else { w.get("shard") } {
                        open.insert(k.to_string(), since);
                    }
                }
                seeded = Some(since);
            }
        }
        // A coordinator's events start at its first survey: what was
        // already down then has no event of its own, but the alert that
        // fired at that survey names it.
        if seeded.is_none() {
            let early = lock(&svc.alerts.events).iter().find(|e| e.rule == rule.id && e.state == "firing" && e.ts_ms >= held && e.ts_ms <= held + FIRST_SURVEY_MS).cloned();
            if let Some(e) = early {
                for name in &e.where_ {
                    let k = name.strip_prefix("shard ").and_then(|n| n.parse::<u64>().ok()).map_or_else(|| json!(name).to_string(), |n| json!(n).to_string());
                    open.insert(k, held);
                }
                seeded = Some(held);
            }
        }
    }
    let mut times: Vec<u64> = evs.iter().map(|e| e.ts_ms).collect();
    times.extend(seeded.map(|s| s.max(from)));
    // The ends of the wait (`for_s`) and of a change's 10 minutes.
    times.extend(evs.iter().filter(|e| kinds.contains(&e.event.kind.as_str())).map(|e| e.ts_ms + if rule.metric == "primary_changes" { PRIMARY_CHANGED_MS } else { rule.for_s * 1000 }));
    times.push(to);
    times.sort();
    times.dedup();
    let mut i = 0;
    for t in times.into_iter().filter(|t| *t <= to) {
        while i < evs.len() && evs[i].ts_ms <= t {
            let e = &evs[i];
            if kinds.contains(&e.event.kind.as_str()) {
                open.entry(key(e)).or_insert(e.ts_ms);
                if rule.metric == "primary_changes" {
                    changes.push_back(e.ts_ms);
                }
            } else {
                open.remove(&key(e));
            }
            i += 1;
        }
        let v = value_at(&open, &changes, t);
        let firing = rule.holds(v);
        match (fired_at, firing) {
            (None, true) => fired_at = Some(t),
            (Some(f), false) => {
                spans.push((f, Some(t)));
                fired_at = None;
            }
            _ => {}
        }
        if t >= from {
            points.push(json!({"t": t, "value": v, "window": v}));
        }
    }
    if let Some(f) = fired_at {
        spans.push((f, None));
    }
    json!({
        "points": points, "fired": spans.iter().filter(|(_, b)| b.is_none_or(|b| b >= from)).map(|(a, b)| json!({"from_ms": a.max(&from), "to_ms": b})).collect::<Vec<_>>(),
        "source": "events", "from": from, "to": to, "metric": rule.metric, "covered_from": seeded.unwrap_or(held).min(held).max(from),
        "note": "Replayed from the coordinator's events in memory (copies down and up, majority lost and back, primaries changed).",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A primary that stepped down reports no replicas out of sync: its
    /// alert resolves rather than firing on (it kept a critical alert for
    /// 13 minutes in the chaos monkey's run); and a primary elected a
    /// moment ago, whose old primary is still starting again, is judged
    /// once it has been primary for the rule's window.
    #[test]
    fn a_role_metric_resolves_when_the_role_ends_and_waits_after_it_begins() {
        let sync = defaults().into_iter().find(|r| r.id == "out-of-sync").unwrap();
        let a = Alerts::new(None, None).unwrap();
        a.set_rules(vec![sync.clone()], "t").unwrap();
        let primary = |from: u64, to: u64, v: f64| (from..=to).map(move |s| Arc::new(json!({"t": s * 1000, "replicas_out_of_sync": v})));
        let replica = |from: u64, to: u64| (from..=to).map(|s| Arc::new(json!({"t": s * 1000, "serves_reads": 1.0})));
        // A replica for 5 minutes, then elected: its old primary is out of sync.
        let mut ps: Vec<Arc<Json>> = replica(0, 300).chain(primary(301, 330, 1.0)).collect();
        a.check(&ps, 330_000);
        assert_eq!(a.one("out-of-sync").unwrap()["state"], "pending", "30 s a primary");
        ps.extend(primary(331, 365, 1.0));
        a.check(&ps, 365_000);
        let s = a.one("out-of-sync").unwrap();
        assert_eq!(s["state"], "firing", "a whole minute out of sync: {s}");
        // It steps down: a replica again, which reports no replicas.
        ps.extend(replica(366, 370));
        let changes = a.check(&ps, 370_000);
        assert!(changes.iter().any(|e| e.rule == "out-of-sync" && e.state == "resolved"), "{changes:?}");
        let s = a.one("out-of-sync").unwrap();
        assert_eq!(s["state"], "ok", "{s}");
        // A primary from its start is judged as before.
        let b = Alerts::new(None, None).unwrap();
        b.set_rules(vec![sync], "t").unwrap();
        let ps: Vec<Arc<Json>> = primary(0, 300, 1.0).collect();
        b.check(&ps, 300_000);
        assert_eq!(b.one("out-of-sync").unwrap()["state"], "firing");
    }

    /// A rule can watch a query's first number instead of a metric: the
    /// query is one SELECT, its value is the series `sql_<id>` that the
    /// sampler adds, and it fires as any other rule does.
    #[test]
    fn a_rule_watches_the_first_number_of_a_query() {
        let a = Alerts::new(None, None).unwrap();
        let errs = |sql: &str| Rule { sql: sql.into(), ..rule("errs", "Errors in the logs", "", ">", 5.0, "max", 60, 0, "warning", "self", "") };
        for bad in ["DELETE FROM otel_logs", "SELECT 1; DROP TABLE t", "WITH x AS (SELECT 1) DELETE FROM t; SELECT 1"] {
            assert!(a.set_rules(vec![errs(bad)], "t").is_err(), "{bad:?}");
        }
        a.set_rules(vec![errs("SELECT COUNT(*) FROM otel_logs WHERE severity_number >= 17 AND ts > :since;")], "t").unwrap();
        let r = a.rules().into_iter().find(|r| r.id == "errs").unwrap();
        assert_eq!((r.metric.as_str(), r.sql.as_str()), ("sql_errs", "SELECT COUNT(*) FROM otel_logs WHERE severity_number >= 17 AND ts > :since"));
        let ps: Vec<Arc<Json>> = [(0u64, 1.0), (30_000, 9.0)].iter().map(|(t, v)| Arc::new(json!({"t": t, "sql_errs": v}))).collect();
        let fired = a.check(&ps, 30_000);
        assert!(fired.iter().any(|e| e.rule == "errs" && e.state == "firing"), "{fired:?}");
        assert!(a.json()["rules"].as_array().unwrap().iter().any(|r| r["id"] == "errs" && r["sql"].is_string()));
    }

    fn points(vals: &[(u64, f64)]) -> Vec<Arc<Json>> {
        vals.iter().map(|(t, v)| Arc::new(json!({"t": t, "p99_ms": v}))).collect()
    }

    fn slow(eval: &str, window_s: u64, for_s: u64) -> Rule {
        Rule { eval: eval.into(), window_s, for_s, ..rule("p99", "slow", "p99_ms", ">", 100.0, "mean", 300, 0, "warning", "self", "") }
    }

    #[test]
    fn a_client_s_own_deadline_is_not_a_server_error() {
        // R2-shell-monitor-2: 20 504s a second, all of them clients' own deadlines.
        let p = json!({"http_s": 1000.0, "http_5xx_s": 20.0, "client_deadlines_s": 20.0});
        assert_eq!(value_of(&p, "http_5xx_ratio"), Some(0.0));
        let p = json!({"http_s": 1000.0, "http_5xx_s": 30.0, "client_deadlines_s": 20.0});
        assert_eq!(value_of(&p, "http_5xx_ratio"), Some(0.01));
    }

    #[test]
    fn an_alert_fires_after_its_condition_held_and_resolves() {
        let a = Alerts::new(None, None).unwrap();
        a.set_rules(vec![slow("max", 2, 10)], "test").unwrap();
        let mut ps = Vec::new();
        let mut run = |t: u64, v: Option<f64>| {
            ps.push(Arc::new(match v {
                Some(v) => json!({"t": t, "p99_ms": v}),
                None => json!({"t": t}),
            }));
            a.check(&ps, t)
        };
        assert!(run(0, Some(50.0)).is_empty());
        assert!(run(2_000, Some(150.0)).is_empty());
        assert_eq!(a.json()["states"][0]["state"], "pending");
        assert!(run(8_000, Some(150.0)).is_empty());
        let fired = run(12_000, Some(150.0));
        assert_eq!((fired.len(), fired[0].state), (1, "firing"));
        assert_eq!(a.json()["firing"], 1);
        assert_eq!(a.firing()[1], ("severity=\"warning\"".to_string(), 1.0));
        // No data keeps it firing, however long; then it resolves.
        assert!(run(20_000, None).is_empty());
        assert_eq!(a.json()["states"][0]["state"], "firing");
        assert_eq!(a.json()["states"][0]["no_data_since_ms"], 20_000);
        let resolved = run(24_000, Some(10.0));
        assert_eq!(resolved[0].state, "resolved");
        assert_eq!(resolved[0].fired_ms, Some(12_000));
        assert_eq!(a.json()["events"].as_array().unwrap().len(), 2);
        assert_eq!(a.json()["events"][0]["state"], "resolved");
    }

    #[test]
    fn a_window_s_mean_fires_where_every_point_does_not() {
        // Above the threshold on average, with a dip every third point.
        let vals: Vec<(u64, f64)> = (0..120u64).map(|i| (i * 2000, if i % 3 == 2 { 20.0 } else { 160.0 })).collect();
        let ps = points(&vals);
        let mean = slow("mean", 60, 0);
        let every = slow("all", 60, 0);
        let (m, e) = (Alerts::new(None, None).unwrap(), Alerts::new(None, None).unwrap());
        m.set_rules(vec![mean], "t").unwrap();
        e.set_rules(vec![every], "t").unwrap();
        let (mut mf, mut ef) = (0, 0);
        // From the first full window.
        for i in 30..ps.len() {
            let t = ps[i]["t"].as_u64().unwrap();
            mf += m.check(&ps[..=i], t).iter().filter(|e| e.state == "firing").count();
            ef += e.check(&ps[..=i], t).iter().filter(|e| e.state == "firing").count();
        }
        assert_eq!((mf, ef), (1, 0), "the mean fires once and stays; every point never holds");
    }

    #[test]
    fn resolve_holds_a_firing_alert_until_it_is_passed() {
        let a = Alerts::new(None, None).unwrap();
        a.set_rules(vec![Rule { resolve: Some(80.0), ..slow("max", 1, 0) }], "t").unwrap();
        let mut ps = Vec::new();
        let mut run = |t: u64, v: f64| {
            ps.push(Arc::new(json!({"t": t, "p99_ms": v})));
            a.check(&ps, t).iter().map(|e| e.state).collect::<Vec<_>>()
        };
        assert_eq!(run(0, 120.0), ["firing"]);
        assert!(run(2000, 90.0).is_empty(), "under the threshold, over resolve: still firing");
        assert_eq!(run(4000, 70.0), ["resolved"]);
        let bad = Rule { resolve: Some(120.0), ..slow("max", 1, 0) };
        assert!(Alerts::new(None, None).unwrap().set_rules(vec![bad], "t").is_err());
    }

    #[test]
    fn editing_a_firing_rule_resolves_it_and_says_why() {
        let a = Alerts::new(None, None).unwrap();
        a.set_rules(vec![slow("max", 1, 0)], "t").unwrap();
        let ps = points(&[(0, 500.0)]);
        assert_eq!(a.check(&ps, 0)[0].state, "firing");
        let (resolved, before, after) = a.set_rules(vec![Rule { threshold: 1000.0, ..slow("max", 1, 0) }], "ana").unwrap();
        assert!(resolved.is_empty(), "an edited rule that stays on is judged by its new condition at the next check: {resolved:?}");
        assert_eq!((before[0].threshold, after[0].threshold), (100.0, 1000.0));
        let next = a.check(&points(&[(0, 500.0), (2_000, 500.0)]), 2_000);
        assert_eq!(next[0].state, "resolved");
        assert_eq!((next[0].why.as_deref(), next[0].by.as_deref()), (Some("the rule was edited"), Some("ana")));
        // Edited to a rule it still breaks: it stays firing, nothing is sent again.
        assert_eq!(a.check(&points(&[(4_000, 5000.0)]), 4_000)[0].state, "firing");
        a.set_rules(vec![Rule { threshold: 2000.0, ..slow("max", 1, 0) }], "ana").unwrap();
        assert!(a.check(&points(&[(4_000, 5000.0), (6_000, 5000.0)]), 6_000).is_empty());
        assert_eq!(a.one("p99").unwrap()["state"], "firing");
        // Turned off: it resolves at once.
        let (resolved, ..) = a.set_rules(vec![Rule { enabled: false, threshold: 2000.0, ..slow("max", 1, 0) }], "ana").unwrap();
        assert_eq!(resolved[0].state, "resolved");
        let (resolved, ..) = a.set_rules(vec![], "ana").unwrap();
        assert!(resolved.is_empty(), "it no longer fired");
    }

    #[test]
    fn rules_and_routes_are_checked_and_kept_in_their_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alerts.json");
        let a = Alerts::new(Some(path.clone()), Some("http://hooks.example/abc".into())).unwrap();
        assert_eq!(a.rules(), defaults());
        assert_eq!(a.routes()[0].name, "default");
        assert_eq!(a.routes_json()[0]["url"], "http://hooks.example/...");
        let mut bad = defaults();
        bad[0].op = "!=".into();
        assert!(a.set_rules(bad, "t").is_err());
        let mut two = defaults();
        two[1].id = two[0].id.clone();
        assert!(a.set_rules(two, "t").is_err());
        let mut fewer = defaults();
        fewer.truncate(2);
        a.set_rules(fewer.clone(), "t").unwrap();
        a.set_routes(vec![Route { name: "default".into(), url: String::new(), severities: vec!["critical".into()], ..Route::default() }, Route { name: "pager".into(), url: "https://pager.example/x".into(), severities: vec![], ..Route::default() }]).unwrap();
        let again = Alerts::new(Some(path.clone()), None).unwrap();
        assert_eq!(again.rules(), fewer);
        assert_eq!(again.routes()[0].url, "http://hooks.example/abc", "a route given without its url keeps it");
        // An old file: a list of rules.
        std::fs::write(&path, serde_json::to_vec(&json!([{"id": "x", "name": "x", "metric": "qps", "op": ">", "threshold": 1}])).unwrap()).unwrap();
        let old = Alerts::new(Some(path), None).unwrap();
        assert_eq!((old.rules()[0].eval.as_str(), old.rules()[0].window_s), ("mean", 300));
        // A zero wait fires on the points there are.
        let fired = a.check(&[Arc::new(json!({"t": 0, "failing": 1.0}))], 0);
        assert_eq!(fired[0].rule, "failing");
    }

    #[test]
    fn a_window_the_series_does_not_span_yet_is_evaluated_and_said_partial() {
        let a = Alerts::new(None, None).unwrap();
        a.set_rules(vec![slow("mean", 300, 0)], "t").unwrap();
        // A server up 10 s: too few points to judge a five-minute window.
        let ps = points(&[(0, 20.0), (2_000, 30.0), (4_000, 20.0), (6_000, 30.0), (8_000, 20.0), (10_000, 30.0)]);
        assert!(a.check(&ps, 10_000).is_empty());
        assert_eq!(a.one("p99").unwrap()["state"], "no_data");
        // Every point over the threshold: still too few to say so.
        let hot = points(&[(10_000, 200.0), (12_000, 300.0)]);
        assert!(a.check(&hot, 12_000).is_empty());
        // Up 40 s: judged on the points there are.
        let ps: Vec<(u64, f64)> = (0..=20u64).map(|i| (i * 2000, if i % 2 == 0 { 20.0 } else { 30.0 })).collect();
        assert!(a.check(&points(&ps), 40_000).is_empty());
        let s = a.one("p99").unwrap();
        assert_eq!(s["state"].as_str(), Some("ok"), "{s}");
        assert!((20.0..30.0).contains(&s["value"].as_f64().unwrap()), "{s}");
        assert_eq!((s["window_partial"].as_bool(), s["window_covered_s"].as_u64()), (Some(true), Some(40)), "{s}");
        // Once the series spans the window, it is not partial.
        let full: Vec<(u64, f64)> = (0..200u64).map(|i| (i * 2000, 20.0)).collect();
        a.check(&points(&full), 398_000);
        assert!(a.one("p99").unwrap().get("window_partial").is_none());
    }

    #[test]
    fn copies_that_do_not_report_a_metric_do_not_count() {
        let a = Alerts::new(None, None).unwrap();
        a.set_coordinator(true);
        let rule = defaults().into_iter().find(|r| r.id == "out-of-sync").unwrap();
        let copy = |url: &str, role: &str, state: &str, reports: bool| json!({"url": url, "status": "ok", "role": role, "detail": {"load": {"t": 1_000}, "alerts": {"rules": {"out-of-sync": {"state": state, "value": if reports { json!(0.0) } else { Json::Null }, "since_ms": 500, "sig": rule.sig(), "reports": reports}}}}});
        // Primaries report replicas out of sync; replicas do not.
        let snap = |p1: &str| json!({"shards": [
            {"shard": 0, "replicas": [copy("http://a", "primary", "ok", true), copy("http://b", "replica", "no_data", false)]},
            {"shard": 1, "replicas": [copy("http://c", "primary", p1, true), copy("http://d", "replica", "no_data", false)]},
        ]});
        a.check_cluster(&snap("ok"), 1_000);
        let s = a.one("out-of-sync").unwrap();
        assert_eq!(s["state"], "ok", "{s}");
        assert_eq!((s["copies"]["ok"].as_u64(), s["copies"]["not_reported"].as_u64(), s["copies"]["no_data"].as_u64()), (Some(2), Some(2), Some(0)), "{s}");
        assert!(s["where"].as_array().unwrap().is_empty(), "{s}");
        // A primary without data now does not count either, while one has.
        a.check_cluster(&snap("no_data"), 2_000);
        assert_eq!(a.one("out-of-sync").unwrap()["state"], "ok");
        // No copy has it: no data.
        let none = json!({"shards": [{"shard": 0, "replicas": [copy("http://a", "primary", "no_data", true), copy("http://b", "replica", "no_data", false)]}]});
        a.check_cluster(&none, 3_000);
        assert_eq!(a.one("out-of-sync").unwrap()["state"], "no_data");
    }

    #[test]
    fn an_alert_on_several_copies_names_them_and_its_resolution_says_where() {
        let a = Alerts::new(None, None).unwrap();
        a.set_coordinator(true);
        a.set_identity(Some("prod".into()), None);
        let disk = defaults().into_iter().find(|r| r.id == "disk-full").unwrap();
        let copy = |url: &str, state: &str| json!({"url": url, "status": "ok", "role": "primary", "takes_writes": true, "detail": {"load": {"t": 1_000}, "alerts": {"rules": {"disk-full": {"state": state, "value": 0.95, "since_ms": 500, "sig": disk.sig()}}}}});
        let snap = |s: &str| json!({"shards": [{"shard": 0, "replicas": [copy("http://a", s)]}, {"shard": 1, "replicas": [copy("http://b", s)]}]});
        let fired = a.check_cluster(&snap("firing"), 1_000).into_iter().find(|e| e.rule == "disk-full").unwrap();
        let ev = event_of(&fired);
        assert_eq!(ev.where_, json!({"rule": "disk-full", "copies": ["http://a", "http://b"]}), "neither one copy nor one shard: {:?}", ev.where_);
        let resolved = a.check_cluster(&snap("ok"), 30_000).into_iter().find(|e| e.rule == "disk-full").unwrap();
        assert_eq!((resolved.state, resolved.where_.clone()), ("resolved", vec!["a".to_string(), "b".to_string()]), "named from their URLs");
        assert_eq!(resolved.fired_ms, Some(1_000));
        assert!(a.message(&resolved).starts_with("[chloedb RESOLVED warning] Disk almost full on prod/a,b: "), "{}", a.message(&resolved));
        assert!(event_of(&resolved).title.starts_with("Disk almost full resolved on a, b after 29 s"), "{}", event_of(&resolved).title);
        // One copy: the copy and its shard.
        let one = json!({"shards": [{"shard": 0, "replicas": [copy("http://a", "firing")]}, {"shard": 1, "replicas": [copy("http://b", "ok")]}]});
        let fired = a.check_cluster(&one, 40_000).into_iter().find(|e| e.rule == "disk-full").unwrap();
        assert_eq!(event_of(&fired).where_, json!({"rule": "disk-full", "shard": 0, "copy": "http://a"}));
    }

    /// A rule against the window before is titled with a ratio, never
    /// as the metric's own value (p99_ms = 4.3 read as milliseconds).
    #[test]
    fn a_ratio_rule_is_titled_as_a_ratio() {
        let a = Alerts::new(None, None).unwrap();
        a.set_coordinator(true);
        let disk = defaults().into_iter().find(|r| r.id == "disk-full").unwrap();
        let copy = json!({"url": "http://a", "status": "ok", "role": "primary", "takes_writes": true, "detail": {"load": {"t": 1_000}, "alerts": {"rules": {"disk-full": {"state": "firing", "value": 0.95, "since_ms": 500, "sig": disk.sig()}}}}});
        let mut e = (*a.check_cluster(&json!({"shards": [{"shard": 0, "replicas": [copy]}]}), 1_000).into_iter().find(|e| e.rule == "disk-full").unwrap()).clone();
        (e.name, e.metric, e.value, e.threshold, e.op, e.eval, e.window_s, e.vs) = ("Query p99 doubled".into(), "p99_ms".into(), Some(4.343), 2.0, ">".into(), "mean".into(), 300, "before".into());
        let title = event_of(&e).title;
        assert!(title.contains("p99 4.343x its baseline (> 2x, mean over 5 min against the 5 min before)"), "{title}");
        assert!(!title.contains("p99_ms ="), "{title}");
        e.vs = String::new();
        assert!(event_of(&e).title.contains("p99_ms = 4.343 (> 2, mean over 5 min)"), "{}", event_of(&e).title);
    }

    #[test]
    fn a_coordinator_gathers_its_copies_and_checks_its_cluster() {
        let a = Alerts::new(None, None).unwrap();
        a.set_coordinator(true);
        assert!(a.rules().iter().any(|r| r.id == "shard-no-majority" && r.builtin));
        let disk = defaults().into_iter().find(|r| r.id == "disk-full").unwrap();
        let copy = |url: &str, state: &str, v: f64| json!({"url": url, "status": "ok", "role": "primary", "takes_writes": true, "detail": {"load": {"t": 1_000}, "alerts": {"rules": {"disk-full": {"state": state, "value": v, "since_ms": 500, "sig": disk.sig()}}}}});
        let snap = json!({"shards": [
            {"shard": 0, "replicas": [copy("http://a", "ok", 0.5)]},
            {"shard": 1, "replicas": [copy("http://b", "firing", 0.95), {"url": "http://c", "status": "down", "role": "replica"}]},
        ]});
        let changes = a.check_cluster(&snap, 1_000);
        let fired: Vec<&str> = changes.iter().filter(|e| e.state == "firing").map(|e| e.rule.as_str()).collect();
        assert!(fired.contains(&"disk-full") && fired.contains(&"shard-no-majority"), "{fired:?}");
        let j = a.one("disk-full").unwrap();
        assert_eq!(j["where"][0]["short"], "b", "{j}");
        assert_eq!(j["copies"]["firing"], 1);
        let m = a.one("shard-no-majority").unwrap();
        assert_eq!(m["where"][0]["shard"], 1, "{m}");
        assert!(changes.iter().any(|e| e.rule == "shard-no-majority" && e.where_ == ["shard 1"]));
        // The copy comes back: the shard has its majority again.
        let back = json!({"shards": [
            {"shard": 0, "replicas": [copy("http://a", "ok", 0.5)]},
            {"shard": 1, "replicas": [copy("http://b", "ok", 0.5), copy("http://c", "ok", 0.5)]},
        ]});
        let changes = a.check_cluster(&back, 30_000);
        assert!(changes.iter().any(|e| e.rule == "shard-no-majority" && e.state == "resolved"), "{changes:?}");
        assert!(changes.iter().any(|e| e.rule == "disk-full" && e.state == "resolved"));
    }

    #[test]
    fn messages_name_the_cluster_the_place_and_the_runbook() {
        let a = Alerts::new(None, None).unwrap();
        a.set_identity(Some("prod/search".into()), Some("https://cc.example/".into()));
        let r = Rule { runbook: "https://wiki/disk".into(), ..defaults().into_iter().find(|r| r.id == "disk-full").unwrap() };
        let mut st = St::new(0);
        st.value = Some(0.934);
        st.where_ = vec![json!({"short": "s1-0"})];
        let e = Event::of(&r, "firing", &st, 10);
        assert_eq!(
            a.message(&e),
            "[chloedb FIRING warning] Disk almost full on prod/search/s1-0: disk_used_ratio = 0.934 (> 0.9, mean over 5 min). Under 10% of the copy's disk is left. Runbook: https://wiki/disk https://cc.example/#/alert/disk-full"
        );
        let mut r2 = Event::of(&r, "resolved", &st, 600_010);
        (r2.fired_ms, r2.why, r2.by) = (Some(10), Some("the rule was deleted".into()), Some("ana".into()));
        assert_eq!(a.message(&r2), "[chloedb RESOLVED warning] Disk almost full on prod/search/s1-0: disk_used_ratio = 0.934 after 10 min (the rule was deleted by ana). Runbook: https://wiki/disk https://cc.example/#/alert/disk-full");
    }

    fn peer(url: &str, this: bool, seen: u64) -> crate::peers::Peer {
        crate::peers::Peer { url: url.into(), name: url.into(), this, last_seen_ms: seen }
    }

    /// An each_copy rule edited on the coordinator: a copy still on the old
    /// one is pushed the new one, and until it takes it the coordinator
    /// judges its value by the new rule (NEW-alerts-logs-1): no FIRING
    /// names a value that passes the rule it names.
    #[test]
    fn a_copy_on_an_old_rule_is_judged_by_the_new_one_and_pushed_it() {
        let a = Alerts::new(None, None).unwrap();
        a.set_coordinator(true);
        let old = rule("docs", "Many documents", "docs", ">", 10.0, "max", 10, 0, "warning", "each_copy", "");
        let new = Rule { threshold: 1000.0, ..old.clone() };
        a.set_rules(vec![new.clone()], "ana").unwrap();
        let copy = |url: &str, sig: &str, state: &str, v: f64, rules_ms: u64| json!({"url": url, "status": "ok", "role": "primary", "takes_writes": true, "detail": {"load": {"t": 1_000}, "alerts": {"rules_ms": rules_ms, "rules": {"docs": {"state": state, "value": v, "since_ms": 500, "sig": sig}}}}});
        // s0-0 took the edit; s1-0 runs the old rule and says firing on 300 documents.
        let snap = json!({"shards": [{"shard": 0, "replicas": [copy("http://a", &new.sig(), "ok", 300.0, now_ms())]}, {"shard": 1, "replicas": [copy("http://b", &old.sig(), "firing", 300.0, 1)]}]});
        let changes = a.check_cluster(&snap, 1_000);
        assert!(changes.iter().all(|e| e.rule != "docs"), "300 does not pass > 1000: {changes:?}");
        let s = a.one("docs").unwrap();
        assert_eq!(s["state"], "ok", "{s}");
        assert_eq!(s["copies"]["rule_differs"], 1, "{s}");
        let w = s["where"].as_array().unwrap();
        assert_eq!((w.len(), w[0]["short"].as_str(), w[0]["rule_differs"].as_bool(), w[0]["copy_state"].as_str()), (1, Some("b"), Some(true), Some("firing")), "{s}");
        assert_eq!(a.to_push(&snap, 1_000), ["http://b"], "only the copy on the old rule");
        a.pushed_to("http://b", 1_000, Err("older chloedb-server".into()));
        assert!(a.to_push(&snap, 2_000).is_empty(), "not again within 30 s");
        assert_eq!(a.to_push(&snap, 1_000 + REPUSH_MS), ["http://b"]);
        a.check_cluster(&snap, 2_000);
        assert_eq!(a.one("docs").unwrap()["copies"]["push_failed"][0]["error"], "older chloedb-server");
        // Over the new threshold on the old rule's copy: it fires, by the new rule.
        let hot = json!({"shards": [{"shard": 1, "replicas": [copy("http://b", &old.sig(), "firing", 5000.0, 1)]}]});
        let fired = a.check_cluster(&hot, 3_000);
        let e = fired.iter().find(|e| e.rule == "docs").expect("fires");
        assert!(new.holds(e.value.unwrap()) && e.threshold == 1000.0, "{e:?}");
        assert_eq!(e.rule_differs, 1);
        assert!(a.message(e).contains("1 of its copies have not taken the rule's last edit yet"), "{}", a.message(e));
        // A copy whose rules were edited after the coordinator's is left alone.
        let later = json!({"shards": [{"shard": 1, "replicas": [copy("http://c", &old.sig(), "ok", 1.0, u64::MAX)]}]});
        assert!(a.to_push(&later, 1_000).is_empty());
        // A copy takes the coordinator's each_copy rules, unless its own are newer.
        let c = Alerts::new(None, None).unwrap();
        c.set_rules(vec![rule("mine", "mine", "qps", ">", 1.0, "mean", 60, 0, "info", "self", ""), old.clone()], "t").unwrap();
        let took = c.set_copy_rules(vec![new.clone()], "the coordinator", now_ms() + 1_000).unwrap();
        assert_eq!(took["took"], true, "{took}");
        let ids: Vec<(String, f64)> = c.rules().into_iter().map(|r| (r.id, r.threshold)).collect();
        assert_eq!(ids, [("mine".to_string(), 1.0), ("docs".to_string(), 1000.0)], "its own rules stay");
        let refused = c.set_copy_rules(vec![old], "the coordinator", 5).unwrap();
        assert_eq!(refused["took"], false, "{refused}");
    }

    /// Acknowledgements, silences, notes and what was paged are kept in
    /// the state file next to the rules: a restarted coordinator has them,
    /// and does not page again what it paged (NEW-alerts-logs-3).
    #[test]
    fn state_survives_a_restart_and_a_restart_does_not_page_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alerts.json");
        let t = now_ms();
        let a = Alerts::new(Some(path.clone()), None).unwrap();
        a.set_rules(vec![slow("max", 2, 0), Rule { id: "other".into(), name: "Other".into(), ..slow("max", 2, 0) }], "t").unwrap();
        let mut ps = points(&[(t - 2_000, 500.0), (t, 500.0)]);
        assert_eq!(a.check(&ps, t).len(), 2);
        assert_eq!(a.due(t).len(), 2, "both paged");
        assert!(a.due(t + 2_000).is_empty(), "once");
        a.ack("p99", "ana", Some("on it".into())).unwrap();
        a.silence("other", "bo", 60, "deploying").unwrap();
        a.note("p99", "ana", "segment 12 merges slowly").unwrap();
        let file = dir.path().join("alerts.state.json");
        assert!(file.exists(), "kept next to the rules");
        assert_eq!(a.json()["state_file"], file.display().to_string());
        drop(a);
        let b = Alerts::new(Some(path.clone()), None).unwrap();
        let s = b.one("p99").unwrap();
        assert_eq!((s["state"].as_str(), s["ack"]["by"].as_str(), s["notes"].as_array().map(Vec::len)), (Some("firing"), Some("ana"), Some(2)), "{s}");
        assert!(s["paged"]["at_ms"].as_u64().is_some(), "{s}");
        assert_eq!(b.one("other").unwrap()["silenced"]["by"], "bo");
        // Still firing after the restart: no change, and nothing paged again.
        ps.push(Arc::new(json!({"t": t + 2_000, "p99_ms": 500.0})));
        assert!(b.check(&ps, t + 2_000).is_empty());
        assert!(b.due(t + 4_000).is_empty(), "a restart does not page again");
        // It resolves: its RESOLVED message goes, since it was paged; the silenced one's does not.
        ps.extend([Arc::new(json!({"t": t + 4_000, "p99_ms": 10.0})), Arc::new(json!({"t": t + 6_000, "p99_ms": 10.0}))]);
        let res = b.check(&ps, t + 6_000);
        assert_eq!(res.iter().map(|e| e.state).collect::<Vec<_>>(), ["resolved", "resolved"]);
        let told: Vec<String> = b.resolutions(&res).iter().map(|e| e.rule.clone()).collect();
        assert_eq!(told, ["p99"]);
        // A firing again is a new alert: paged, with no acknowledgement.
        ps.extend([Arc::new(json!({"t": t + 8_000, "p99_ms": 900.0})), Arc::new(json!({"t": t + 10_000, "p99_ms": 900.0}))]);
        b.check(&ps, t + 10_000);
        assert!(b.one("p99").unwrap().get("ack").is_none());
        assert_eq!(b.due(t + 10_000).len(), 1);
        drop(b);
        // A restarted coordinator first hears the others: what fires
        // meanwhile is not sent for STARTUP_MS.
        let c = Alerts::new(Some(path), None).unwrap();
        c.set_coordinator(true);
        c.set_rules(vec![slow("max", 2, 0), Rule { id: "third".into(), name: "Third".into(), threshold: 1000.0, ..slow("max", 2, 0) }], "t").unwrap();
        let t2 = now_ms();
        c.check(&points(&[(t2 - 2_000, 2000.0), (t2, 2000.0)]), t2);
        assert_eq!(c.one("third").unwrap()["state"], "firing");
        assert!(c.due(t2).iter().all(|(e, _)| e.rule != "third"), "not while it starts");
        assert!(c.due(t2 + STARTUP_MS).iter().any(|(e, _)| e.rule == "third"));
    }

    /// With several coordinators, the one with the lowest URL among those
    /// heard pages; the others wait, take what it says it paged, and page
    /// only if it said nothing for a minute. Acknowledgements, silences and
    /// notes reach every coordinator (NEW-alerts-logs-2).
    #[test]
    fn one_coordinator_pages_and_the_others_take_its_word() {
        let t = now_ms();
        let (xa, xb) = ("http://10.0.0.1:7700", "http://10.0.0.2:7700");
        let mk = |me: &str, other: &str| {
            let a = Alerts::new(None, None).unwrap();
            a.set_coordinator(true);
            a.set_peers(vec![peer(me, true, t), peer(other, false, t)]);
            a.set_rules(vec![slow("max", 2, 0)], "t").unwrap();
            a
        };
        let (a, b) = (mk(xa, xb), mk(xb, xa));
        assert_eq!(a.pager(), (xa.to_string(), xa.to_string()));
        assert_eq!(b.pager(), (xb.to_string(), xa.to_string()));
        assert_eq!(b.json()["pager"]["sends"], xa);
        let ps = points(&[(t - 2_000, 500.0), (t, 500.0)]);
        a.check(&ps, t);
        b.check(&ps, t);
        assert_eq!(a.due(t).len(), 1, "the lowest URL pages");
        assert!(b.due(t).is_empty(), "the other waits");
        b.merge(&a.peer_state(), xa).unwrap();
        assert_eq!(b.one("p99").unwrap()["paged"]["by"], xa);
        assert!(b.due(t + PAGER_WAIT_MS + 1_000).is_empty(), "paged by the other: once");
        // Acknowledged on b: a has it.
        b.ack("p99", "ana", None).unwrap();
        a.merge(&b.peer_state(), xb).unwrap();
        assert_eq!(a.one("p99").unwrap()["ack"]["by"], "ana");
        b.note("p99", "ana", "a note").unwrap();
        a.note("p99", "bo", "another").unwrap();
        a.merge(&b.peer_state(), xb).unwrap();
        b.merge(&a.peer_state(), xa).unwrap();
        assert_eq!(a.one("p99").unwrap()["notes"], b.one("p99").unwrap()["notes"]);
        assert_eq!(a.one("p99").unwrap()["notes"].as_array().unwrap().len(), 2);
        a.silence("p99", "bo", 30, "deploying").unwrap();
        b.merge(&a.peer_state(), xa).unwrap();
        assert_eq!(b.one("p99").unwrap()["silenced"]["reason"], "deploying");
        // A merged edit of the rules: the newer one is taken.
        a.set_rules_at(vec![Rule { threshold: 300.0, ..slow("max", 2, 0) }], "ana", t + 1_000).unwrap();
        b.merge(&a.peer_state(), xa).unwrap();
        assert_eq!(b.rules()[0].threshold, 300.0);
        // The pager is not heard for a while: the other one pages, and says so.
        let c = mk(xb, xa);
        c.set_peers(vec![peer(xb, true, t), peer(xa, false, t - PEER_LIVE_MS - 1_000)]);
        assert_eq!(c.pager().1, xb);
        c.check(&ps, t);
        assert_eq!(c.due(t).len(), 1);
        // Heard, but it did not say it paged within a minute: sent, saying why.
        let d = mk(xb, xa);
        d.check(&ps, t);
        assert!(d.due(t + 1_000).is_empty());
        let late = d.due(t + PAGER_WAIT_MS + 1_000);
        assert_eq!(late.len(), 1);
        assert!(late[0].0.context.as_deref().is_some_and(|c| c.contains("did not say it sent this")), "{:?}", late[0].0.context);
        // One that could not be sent for a while (silenced here) waits a
        // minute from when it could, not from when it fired.
        let e = mk(xb, xa);
        e.check(&ps, t);
        e.silence("p99", "ana", 1, "deploying").unwrap();
        assert!(e.due(t + 1_000).is_empty());
        let after = t + 120_000;
        assert!(e.due(after).is_empty(), "the one that sends gets its minute");
        assert_eq!(e.due(after + PAGER_WAIT_MS).len(), 1);
    }

    /// A rule over a coordinator's own traffic fires on one coordinator
    /// alone: every coordinator shows it, as firing there; the one where
    /// it fires pages at once when the one that sends the messages says it
    /// does not see it, and sends its resolution; an acknowledgement given
    /// on another coordinator reaches it (R2-scenarios-1).
    #[test]
    fn an_alert_on_one_coordinator_only_shows_and_pages_from_there() {
        let t = now_ms();
        let (xa, xb) = ("http://10.0.0.1:7700", "http://10.0.0.2:7700");
        let mk = |me: &str, other: &str| {
            let a = Alerts::new(None, None).unwrap();
            a.set_coordinator(true);
            a.set_peers(vec![peer(me, true, t), peer(other, false, t)]);
            a.set_rules(vec![slow("max", 2, 0)], "t").unwrap();
            a
        };
        let (a, b) = (mk(xa, xb), mk(xb, xa));
        // Slow through b only, for 20 s; a's own traffic is fast.
        let since = t - 20_000;
        b.check(&points(&[(since - 2_000, 500.0), (since, 500.0)]), since);
        a.check(&points(&[(t - 2_000, 5.0), (t, 5.0)]), t);
        assert_eq!(b.one("p99").unwrap()["state"], "firing");
        assert!(b.due(since).is_empty(), "a sends the messages: b waits for it");
        // a says it does not see it: b pages now, not a minute later.
        b.merge(&a.peer_state(), xa).unwrap();
        let sent = b.due(t);
        assert_eq!(sent.len(), 1, "sent at once");
        assert!(sent[0].0.context.as_deref().is_some_and(|c| c.contains("does not see it")), "{:?}", sent[0].0.context);
        // a shows it, firing on b, in its count and its brief answer.
        a.merge(&b.peer_state(), xb).unwrap();
        let s = a.one("p99").unwrap();
        assert_eq!((s["state"].as_str(), s["here"].as_str(), s["elsewhere"][0]["url"].as_str()), (Some("firing"), Some("ok"), Some(xb)), "{s}");
        let j = a.json();
        assert_eq!((j["firing_shown"].as_u64(), j["firing_elsewhere"].as_u64()), (Some(1), Some(1)));
        let brief = a.brief_json();
        assert_eq!(brief["states"][0]["elsewhere"][0]["name"], xb, "{brief}");
        // Acknowledged on a: told to b, which keeps it.
        a.ack("p99", "ana", Some("looking".into())).unwrap();
        let told: Vec<Json> = lock(&a.outbox).drain(..).collect();
        assert_eq!(told.len(), 1);
        b.merge(&told[0], xa).unwrap();
        assert_eq!(b.one("p99").unwrap()["ack"]["by"], "ana");
        a.merge(&b.peer_state(), xb).unwrap();
        assert_eq!(a.one("p99").unwrap()["ack"]["by"], "ana", "a shows b's acknowledgement");
        // Resolved on b: b sends the resolution, since a never saw it.
        let res = b.check(&points(&[(t + 2_000, 5.0), (t + 4_000, 5.0)]), t + 4_000);
        assert_eq!(res.iter().map(|e| e.state).collect::<Vec<_>>(), ["resolved"]);
        assert_eq!(b.resolutions(&res).len(), 1);
        a.merge(&b.peer_state(), xb).unwrap();
        assert_eq!(a.one("p99").unwrap()["state"], "ok");
        assert!(a.one("p99").unwrap().get("elsewhere").is_none());
        // A shared alert that a sees too: b waits for a, as before.
        let (a, b) = (mk(xa, xb), mk(xb, xa));
        let ps = points(&[(since - 2_000, 500.0), (since, 500.0)]);
        a.check(&ps, since);
        b.check(&ps, since);
        b.merge(&a.peer_state(), xa).unwrap();
        assert!(b.due(t).is_empty(), "a sees it firing: a pages it");
    }

    /// The built-in rules over errors and the p99 fire where Home's SLO
    /// checks do (OV-8); the bell's brief answer has what it counts.
    #[test]
    fn the_slo_rules_follow_the_slo_and_the_bell_reads_a_brief_answer() {
        let a = Alerts::new(None, None).unwrap();
        a.set_coordinator(true);
        let get = |id: &str| a.rules().into_iter().find(|r| r.id == id).unwrap();
        assert_eq!((get("errors").threshold, get("errors").name.as_str()), (0.01, "Query errors over 1%"));
        assert_eq!((get("p99").threshold, get("p99").name.as_str()), (250.0, "Query p99 over 250 ms"));
        assert_eq!(get("availability").threshold, 0.99);
        a.follow_slo(0.005, 2000.0);
        assert_eq!((get("errors").threshold, get("errors").resolve, get("errors").name.as_str()), (0.005, Some(0.0025), "Query errors over 0.5%"));
        assert_eq!(get("p99").name, "Query p99 over 2 s");
        assert!((get("availability").threshold - 0.995).abs() < 1e-9);
        // A server that becomes a coordinator later keeps it on its built-in rules.
        let c = Alerts::new(None, None).unwrap();
        c.follow_slo(0.005, 2000.0);
        c.set_coordinator(true);
        assert!((c.rules().into_iter().find(|r| r.id == "availability").unwrap().threshold - 0.995).abs() < 1e-9);
        // A threshold someone chose is theirs.
        let b = Alerts::new(None, None).unwrap();
        let mut rules = b.rules();
        rules.iter_mut().find(|r| r.id == "errors").unwrap().threshold = 0.03;
        b.set_rules(rules, "ana").unwrap();
        b.follow_slo(0.005, 2000.0);
        assert_eq!(b.rules().into_iter().find(|r| r.id == "errors").unwrap().threshold, 0.03);
        let t = now_ms();
        a.check(&points(&[(t - 300_000, 3000.0), (t, 3000.0)]), t);
        let (full, brief) = (a.json(), a.brief_json());
        assert_eq!(brief["firing"], full["firing"]);
        assert_eq!(brief["firing_shown"], full["firing_shown"]);
        let s = brief["states"].as_array().unwrap();
        assert!(s.iter().all(|x| x["state"] != "ok" && x.get("where").is_none()), "{brief}");
        assert!(s.iter().any(|x| x["rule"] == "p99" && x["name"] == "Query p99 over 2 s" && x["severity"] == "warning"), "{brief}");
        assert_eq!(brief["rules"].as_array().unwrap().len(), full["rules"].as_array().unwrap().len());
        assert!(brief.to_string().len() * 3 < full.to_string().len(), "{} of {}", brief.to_string().len(), full.to_string().len());
    }

    /// A route that sends an unacknowledged critical again does, every
    /// `repeat_min`, until someone acknowledges it (NEW-12).
    #[test]
    fn an_unacknowledged_critical_is_sent_again() {
        let t = now_ms();
        let a = Alerts::new(None, None).unwrap();
        a.set_rules(vec![Rule { severity: "critical".into(), ..slow("max", 2, 0) }], "t").unwrap();
        let rt = |name: &str, repeat_min: u64| Route { name: name.into(), url: "http://127.0.0.1:9/hook".into(), repeat_min, ..Route::default() };
        a.set_routes(vec![rt("oncall", 1), rt("chat", 0)]).unwrap();
        a.check(&points(&[(t - 2_000, 500.0), (t, 500.0)]), t);
        assert_eq!(a.due(t).len(), 1, "paged once");
        assert!(a.due(t + 30_000).is_empty(), "not again within the minute");
        let again = a.due(t + 61_000);
        assert_eq!(again.iter().map(|(_, r)| r.clone()).collect::<Vec<_>>(), [Some("oncall".to_string())], "again, by the route that repeats only");
        assert!(again[0].0.context.as_deref().is_some_and(|c| c.contains("nobody acknowledged")), "{:?}", again[0].0.context);
        assert!(a.due(t + 90_000).is_empty());
        assert_eq!(a.due(t + 122_000).len(), 1, "and again a minute later");
        a.ack("p99", "ana", None).unwrap();
        assert!(a.due(t + 600_000).is_empty(), "acknowledged: no more");
    }

    /// A copy that is down says nothing of its rules: after a minute
    /// what it last said no longer keeps an alert firing.
    #[test]
    fn a_copy_down_for_a_minute_has_no_data() {
        let a = Alerts::new(None, None).unwrap();
        a.set_coordinator(true);
        let disk = defaults().into_iter().find(|r| r.id == "disk-full").unwrap();
        a.set_rules(vec![disk.clone()], "t").unwrap();
        let copy = |url: &str, firing: bool| json!({"url": url, "status": "ok", "role": "primary", "takes_writes": true, "detail": {"load": {"t": 1_000}, "alerts": {"rules": {"disk-full": {"state": if firing { "firing" } else { "ok" }, "value": if firing { 0.95 } else { 0.5 }, "since_ms": 500, "sig": disk.sig()}}}}});
        let down = |url: &str| json!({"url": url, "status": "down", "role": "replica"});
        let snap = |a_up: bool| json!({"shards": [{"shard": 0, "replicas": [if a_up { copy("http://a", true) } else { down("http://a") }, copy("http://b", false), copy("http://c", false)]}]});
        a.check_cluster(&snap(true), 1_000);
        assert_eq!(a.one("disk-full").unwrap()["state"], "firing");
        a.check_cluster(&snap(false), 2_000);
        assert_eq!(a.one("disk-full").unwrap()["state"], "firing", "a missed survey changes nothing");
        a.check_cluster(&snap(false), 2_000 + UNHEARD_MS);
        let s = a.one("disk-full").unwrap();
        assert_eq!(s["state"], "ok", "{s}");
        assert_eq!(s["copies"]["no_data"], 1, "{s}");
    }

    /// A storm: a shard without its majority explains its replica rules,
    /// which are held back under it and page only if they still fire once
    /// it resolved (NEW-alerts-logs-4, NEW-scenarios-9).
    #[test]
    fn a_cause_holds_back_what_it_explains() {
        let a = Alerts::new(None, None).unwrap();
        a.set_coordinator(true);
        let sync = defaults().into_iter().find(|r| r.id == "out-of-sync").unwrap();
        let key = defaults().into_iter().find(|r| r.id == "key-failing").unwrap();
        a.set_rules(vec![sync.clone(), key], "t").unwrap();
        let copy = |url: &str, firing: bool| json!({"url": url, "status": "ok", "role": "primary", "takes_writes": true, "detail": {"load": {"t": 1_000}, "alerts": {"rules": {"out-of-sync": {"state": if firing { "firing" } else { "ok" }, "value": if firing { 1.0 } else { 0.0 }, "since_ms": 500, "sig": sync.sig()}}}}});
        let down = json!({"shards": [{"shard": 0, "replicas": [copy("http://a", false)]}, {"shard": 1, "replicas": [copy("http://b", true), {"url": "http://c", "status": "down", "role": "replica"}]}]});
        let fired: Vec<String> = a.check_cluster(&down, 1_000).iter().filter(|e| e.state == "firing").map(|e| e.rule.clone()).collect();
        assert!(fired.contains(&"shard-no-majority".to_string()) && fired.contains(&"out-of-sync".to_string()), "{fired:?}");
        let s = a.one("out-of-sync").unwrap();
        assert_eq!(s["held_by"], "shard-no-majority", "{s}");
        assert_eq!(s["held_why"], "A shard has no majority (on shard 1)", "{s}");
        // A key's calls fail while the shard is out: explained too.
        let keys = |_: u64, _: u64| vec![KeyCalls { key: "app".into(), calls: 100, failed: 90, ms: 0.0, owner: None }];
        a.check_with(&[Arc::new(json!({"t": 1_000, "qps": 1.0}))], 1_000, &keys);
        let k = a.one("key-failing").unwrap();
        assert_eq!((k["state"].as_str(), k["held_by"].as_str()), (Some("firing"), Some("shard-no-majority")), "{k}");
        assert_eq!(a.json()["firing_shown"], 1, "the bell counts the cause");
        let due: Vec<String> = a.due(1_000).iter().map(|(e, _)| e.rule.clone()).collect();
        assert_eq!(due, ["shard-no-majority"], "one page for the storm");
        // The shard is whole again but the replica is still out of sync: now it pages, saying it was held.
        let back = json!({"shards": [{"shard": 0, "replicas": [copy("http://a", false)]}, {"shard": 1, "replicas": [copy("http://b", true), copy("http://c", false)]}]});
        a.check_cluster(&back, 31_000);
        // Its window (60 s) still holds the outage: it waits for it to pass.
        let s = a.one("out-of-sync").unwrap();
        assert_eq!((s["held_by"].as_str(), s["held_until_ms"].as_u64()), (Some("shard-no-majority"), Some(91_000)), "{s}");
        assert!(a.due(31_000).iter().all(|(e, _)| e.rule != "out-of-sync"));
        a.check_cluster(&back, 91_000);
        assert!(a.one("out-of-sync").unwrap().get("held_by").is_none());
        let due = a.due(91_000);
        let e = &due.iter().find(|(e, _)| e.rule == "out-of-sync").expect("sent once its cause resolved and its window passed").0;
        assert!(e.context.as_deref().is_some_and(|c| c.contains("held back while") && c.contains("60 s after it resolved")), "{:?}", e.context);
    }

    /// Values made of others: 5xx as a share of requests, a replica's lag
    /// in seconds, hours until the disk is full; a key's failed calls; a
    /// p99 against the window before (A11, S5-4, S4-5).
    #[test]
    fn derived_values_keys_and_the_window_before() {
        let t = 600_000u64;
        let at = |f: &dyn Fn(u64) -> Json| -> Vec<Arc<Json>> { (0..=300u64).map(|i| { let mut p = f(i); p["t"] = json!(i * 2_000); Arc::new(p) }).collect() };
        let one = |r: Rule, ps: &[Arc<Json>]| { let a = Alerts::new(None, None).unwrap(); let id = r.id.clone(); a.set_rules(vec![r], "t").unwrap(); a.check(ps, t); a.one(&id).unwrap() };
        let d = |id: &str| defaults().into_iter().find(|r| r.id == id).unwrap();
        // 3 of 100 requests a second fail: 3%, over 1%.
        let s = one(d("5xx"), &at(&|_| json!({"http_s": 100.0, "http_5xx_s": 3.0})));
        assert_eq!(s["state"], "firing", "{s}");
        assert!((s["value"].as_f64().unwrap() - 0.03).abs() < 1e-9, "{s}");
        // 400 transactions behind at 10 writes a second: 40 s.
        let s = one(d("replica-behind"), &at(&|_| json!({"replica_behind_max": 400.0, "writes_s": 10.0})));
        assert!((s["value"].as_f64().unwrap() - 40.0).abs() < 1e-9 && s["state"] == "firing", "{s}");
        // A follower over a minute behind for a minute, and one that
        // only now fell behind.
        let s = one(d("follow-behind"), &at(&|_| json!({"follow_lag_s": 90.0})));
        assert_eq!(s["state"], "firing", "{s}");
        let s = one(d("follow-behind"), &at(&|i| json!({"follow_lag_s": if i > 290 { 90.0 } else { 1.0 }})));
        assert_eq!(s["state"], "ok", "{s}");
        // Filling 5% an hour at 60%: 8 hours.
        let s = one(d("disk-filling"), &at(&|i| json!({"disk_used_ratio": 0.6 - 0.05 * (300 - i) as f64 * 2.0 / 3600.0})));
        assert!((s["value"].as_f64().unwrap() - 8.0).abs() < 0.01 && s["state"] == "firing", "{s}");
        let s = one(d("disk-filling"), &at(&|_| json!({"disk_used_ratio": 0.6})));
        assert_eq!((s["value"].as_f64(), s["state"].as_str()), (Some(NOT_FILLING_H), Some("ok")), "{s}");
        // The p99 of the last 5 minutes over three times the 5 before.
        let s = one(d("p99-jump"), &at(&|i| json!({"p99_ms": if i > 150 { 300.0 } else { 100.0 }})));
        assert!((s["value"].as_f64().unwrap() - 3.0).abs() < 0.05 && s["state"] == "firing", "{s}");
        let s = one(d("p99-jump"), &at(&|_| json!({"p99_ms": 100.0})));
        assert_eq!(s["state"], "ok", "{s}");
        // One key's calls fail; its owner's route takes it.
        let a = Alerts::new(None, None).unwrap();
        a.set_rules(vec![d("key-failing")], "t").unwrap();
        a.set_routes(vec![Route { name: "mobile-team".into(), url: "http://hooks.example/m".into(), ..Route::default() }, Route { name: "all".into(), url: "http://hooks.example/a".into(), ..Route::default() }]).unwrap();
        let keys = |_: u64, _: u64| vec![KeyCalls { key: "app".into(), calls: 100, failed: 40, ms: 0.0, owner: Some("Mobile team".into()) }, KeyCalls { key: "few".into(), calls: 5, failed: 5, ms: 0.0, owner: None }, KeyCalls { key: "ok".into(), calls: 500, failed: 1, ms: 0.0, owner: None }];
        let fired = a.check_with(&at(&|_| json!({"qps": 1.0})), t, &keys);
        assert_eq!(fired.len(), 1, "{fired:?}");
        let s = a.one("key-failing").unwrap();
        assert_eq!((s["where"][0]["key"].as_str(), s["where"].as_array().map(Vec::len)), (Some("app"), Some(1)), "five calls are too few to judge: {s}");
        assert_eq!((fired[0].route.as_str(), fired[0].owner.as_str()), ("mobile-team", "Mobile team"));
        assert!(a.message(&fired[0]).contains("Owner: Mobile team."), "{}", a.message(&fired[0]));
        // No key with calls enough (a restart forgets them): no data, still firing.
        assert!(a.check_with(&at(&|_| json!({"qps": 1.0})), t + 2_000, &|_, _| Vec::new()).is_empty());
        assert_eq!(a.one("key-failing").unwrap()["state"], "firing");
    }

    /// One key's queries take most of the query time and outnumber the
    /// cores while others query: it crowds them out. Alone, under a query
    /// a core, or sharing the time evenly, it does not (the chaos
    /// monkey's query flood).
    #[test]
    fn a_key_that_takes_the_query_time_while_others_query_crowds_them_out() {
        let t = 600_000u64;
        let at: Vec<Arc<Json>> = (0..=300u64).map(|i| Arc::new(json!({"t": i * 2_000, "qps": 1.0}))).collect();
        let rule = defaults().into_iter().find(|r| r.id == "key-flood").unwrap();
        let fresh = || {
            let a = Alerts::new(None, None).unwrap();
            a.set_cores(4);
            a.set_rules(vec![rule.clone()], "t").unwrap();
            a
        };
        let k = |key: &str, calls: u64, ms: f64| KeyCalls { key: key.into(), calls, failed: 0, ms, owner: if key == "report" { Some("Data team".into()) } else { None } };
        // 960 s of query time in the window [t - 60 s, t] (60.001 s) is
        // 16 queries at once, 4 a core.
        let a = fresh();
        let fired = a.check_with(&at, t, &|_, _| vec![k("report", 30_000, 960_000.0), k("app", 3_000, 30_000.0)]);
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert_eq!(fired[0].owner, "Data team");
        let s = a.one("key-flood").unwrap();
        assert_eq!(s["state"], "firing", "{s}");
        assert!((s["value"].as_f64().unwrap() - 4.0).abs() < 0.01, "{s}");
        let w = &s["where"][0];
        assert_eq!((w["key"].as_str(), w["cores"].as_u64()), (Some("report"), Some(4)), "{s}");
        assert!((w["running"].as_f64().unwrap() - 16.0).abs() < 0.01 && (w["share"].as_f64().unwrap() - 0.97).abs() < 0.01, "{s}");
        // Crowding every other key out keeps it firing; once its share of
        // the time is under half, it resolves.
        assert!(a.check_with(&at, t + 2_000, &|_, _| vec![k("report", 30_000, 960_000.0), k("app", 2, 300.0)]).is_empty());
        let gone = a.check_with(&at, t + 4_000, &|_, _| vec![k("report", 300, 20_000.0), k("app", 3_000, 30_000.0)]);
        assert!(gone.len() == 1 && gone[0].state == "resolved", "{gone:?}");
        let quiet = |calls: Vec<KeyCalls>| {
            let a = fresh();
            let fired = a.check_with(&at, t, &move |_, _| calls.clone());
            (fired.len(), a.one("key-flood").unwrap()["value"].as_f64())
        };
        // The same load from a client alone crowds out no one.
        assert_eq!(quiet(vec![k("report", 30_000, 960_000.0)]), (0, Some(0.0)));
        // Most of the time, but 2 queries at once on 4 cores.
        assert_eq!(quiet(vec![k("app", 30_000, 120_000.0), k("ui", 100, 1_000.0)]).0, 0);
        // Two keys with half the time each.
        assert_eq!(quiet(vec![k("a", 1_000, 480_000.0), k("b", 1_000, 480_000.0)]), (0, Some(0.0)));
        // No calls (a restart forgets them): no data.
        assert_eq!(quiet(Vec::new()), (0, None));
    }

    /// A value next to its threshold has the digits to differ from it, and
    /// a 0/1 metric is on or off (NEW-13).
    #[test]
    fn values_differ_from_their_threshold_and_flags_are_on_or_off() {
        assert_eq!(number_vs(1.0003, 1.0), "1.0003");
        assert_eq!(number_vs(0.90004, 0.9), "0.90004");
        assert_eq!(number_vs(0.934, 0.9), "0.934");
        assert_eq!(number_vs(250.0, 100.0), "250");
        let a = Alerts::new(None, None).unwrap();
        let r = defaults().into_iter().find(|r| r.id == "failing").unwrap();
        let mut st = St::new(0);
        st.value = Some(1.0);
        let m = a.message(&Event::of(&r, "firing", &st, 10));
        assert!(m.contains(": failing is on ("), "{m}");
    }

    /// LIV-3: on a calm cluster the built-in rules fired 289 times in a
    /// day: a p99 from 1 ms to 2 ms "doubled", one 5xx of 90 requests was
    /// over 1%, one failed query of three was "errors over 1%", and a
    /// replica one transaction behind while nothing was written read as a
    /// day behind. None of those fires now; the real cases still do.
    #[test]
    fn a_calm_copy_fires_nothing_and_a_real_trouble_still_does() {
        let t = 600_000u64;
        let at = |f: &dyn Fn(u64) -> Json| -> Vec<Arc<Json>> { (0..=300u64).map(|i| { let mut p = f(i); p["t"] = json!(i * 2_000); Arc::new(p) }).collect() };
        let one = |r: Rule, ps: &[Arc<Json>]| { let a = Alerts::new(None, None).unwrap(); let id = r.id.clone(); a.set_rules(vec![r], "t").unwrap(); a.check(ps, t); a.one(&id).unwrap() };
        let d = |id: &str| defaults().into_iter().find(|r| r.id == id).unwrap();
        // The p99 from 1 ms to 2.1 ms: under 20 ms, unchanged.
        let s = one(d("p99-jump"), &at(&|i| json!({"p99_ms": if i > 150 { 2.1 } else { 1.0 }})));
        assert_eq!((s["state"].as_str(), s["value"].as_f64()), (Some("ok"), Some(1.0)), "{s}");
        // From 15 ms to 45 ms: tripled, and over 20 ms, but under half
        // the objective of 250 ms (CCW-19); as the live cluster's 26 ms to
        // 54 ms. Against an objective of 50 ms it is judged.
        let s = one(d("p99-jump"), &at(&|i| json!({"p99_ms": if i > 150 { 45.0 } else { 15.0 }})));
        assert_eq!((s["state"].as_str(), s["value"].as_f64()), (Some("ok"), Some(1.0)), "{s}");
        let s = one(d("p99-jump"), &at(&|i| json!({"p99_ms": if i > 150 { 54.0 } else { 26.0 }})));
        assert_eq!(s["state"], "ok", "{s}");
        let a = Alerts::new(None, None).unwrap();
        a.set_rules(vec![d("p99-jump")], "t").unwrap();
        a.follow_slo(0.01, 50.0);
        a.check(&at(&|i| json!({"p99_ms": if i > 150 { 45.0 } else { 15.0 }})), t);
        assert_eq!(a.one("p99-jump").unwrap()["state"], "firing");
        // From 50 ms to 150 ms: tripled, and over half the objective.
        let s = one(d("p99-jump"), &at(&|i| json!({"p99_ms": if i > 150 { 150.0 } else { 50.0 }})));
        assert_eq!(s["state"], "firing", "{s}");
        // One 5xx of 45 requests in 5 minutes: not judged.
        let s = one(d("5xx"), &at(&|i| json!({"http_s": 0.15, "http_5xx_s": if i == 200 { 0.5 } else { 0.0 }, "secs": 2.0})));
        assert_eq!((s["state"].as_str(), s["value"].as_f64()), (Some("ok"), Some(0.0)), "{s}");
        // Sixteen of 30 requests fail: ten failures or more are judged.
        let s = one(d("5xx"), &at(&|i| json!({"http_s": 0.1, "http_5xx_s": if i % 10 == 0 { 0.5 } else { 0.0 }, "secs": 2.0})));
        assert_eq!(s["state"], "firing", "{s}");
        // One failed query of three; then 5 of 200.
        let s = one(d("errors"), &at(&|i| json!({"queries": if i % 100 == 0 { 1.0 } else { 0.0 }, "query_errors": if i == 200 { 1.0 } else { 0.0 }})));
        assert_eq!((s["state"].as_str(), s["value"].as_f64()), (Some("ok"), Some(0.0)), "{s}");
        let s = one(d("errors"), &at(&|i| json!({"queries": if i > 150 { 1.0 } else { 0.0 }, "query_errors": if i > 150 && i % 30 == 0 { 1.0 } else { 0.0 }})));
        assert_eq!(s["state"], "firing", "{s}");
        // A quiet primary commits one transaction a minute; its replica is
        // one behind at the point just after each: 0 s, not a day.
        let quiet = at(&|i| json!({"seq_no": (i / 30) as f64, "replica_behind_max": if i % 30 == 0 { 1.0 } else { 0.0 }, "writes_s": 0.0}));
        let s = one(d("replica-behind"), &quiet);
        assert_eq!((s["state"].as_str(), s["value"].as_f64()), (Some("ok"), Some(0.0)), "{s}");
        // A replica that stopped at transaction 100 while the primary wrote
        // on for the last 5 minutes is that far behind.
        let stuck = at(&|i| json!({"seq_no": if i < 150 { 100.0 } else { 100.0 + (i - 150) as f64 }, "replica_behind_max": if i < 150 { 0.0 } else { (i - 150) as f64 }, "writes_s": 0.5}));
        let s = one(d("replica-behind"), &stuck);
        assert!(s["state"] == "firing" && (s["value"].as_f64().unwrap() - 149.0).abs() < 2.0, "{s}");
        // A replica being sent a whole copy is all of it behind: it is not
        // the lag rule's (a replica that does not serve reads says it).
        let copying = at(&|i| json!({"seq_no": 1000.0 + i as f64, "replica_behind_max": 1000.0 + i as f64, "replica_behind_applying": 0.0, "writes_s": 0.5}));
        assert_eq!(one(d("replica-behind"), &copying)["state"], "ok");
        // An older primary (no seq_no) writing nothing: no value, not a day.
        assert_eq!(value_of(&json!({"replica_behind_max": 1.0, "writes_s": 0.0}), "replica_behind_s"), None);
        assert_eq!(value_of(&json!({"replica_behind_max": 400.0, "writes_s": 10.0}), "replica_behind_s"), Some(40.0));
    }

    /// LIV-3: a copy's own rules (scope self) fire on the copy only; the
    /// coordinator's Alerts said "Nothing is firing" meanwhile. It lists
    /// them now, apart from its own alerts.
    #[test]
    fn a_coordinator_lists_what_fires_on_its_copies_own_rules() {
        let a = Alerts::new(None, None).unwrap();
        a.set_coordinator(true);
        let disk = defaults().into_iter().find(|r| r.id == "disk-full").unwrap();
        let copy = |url: &str, firing: Json| json!({"url": url, "status": "ok", "role": "primary", "takes_writes": true, "detail": {"load": {"t": 1_000}, "alerts": {"firing": firing, "rules": {"disk-full": {"state": "ok", "value": 0.5, "since_ms": 500, "sig": disk.sig()}}}}});
        let snap = json!({"shards": [
            {"shard": 0, "replicas": [copy("http://a", json!([{"id": "p99-jump", "name": "Query p99 doubled", "severity": "warning", "since_ms": 900, "value": 2.5}]))]},
            {"shard": 1, "replicas": [copy("http://b", json!([{"id": "disk-full", "name": "Disk almost full", "severity": "warning", "since_ms": 900, "value": 0.95}]))]},
        ]});
        a.check_cluster(&snap, 1_000);
        let j = a.json();
        let on = j["copies_firing"].as_array().unwrap();
        assert_eq!(on.len(), 1, "an each_copy rule is the coordinator's own alert: {j}");
        assert_eq!((on[0]["id"].as_str(), on[0]["short"].as_str(), on[0]["shard"].as_u64()), (Some("p99-jump"), Some("a"), Some(0)), "{j}");
        assert_eq!(j["firing"], 0, "{j}");
        assert_eq!(a.brief_json()["copies_firing"], j["copies_firing"]);
    }
}
