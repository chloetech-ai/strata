//! Alert rules over the series ([`crate::telemetry::Series`]): a rule
//! names a value of each point (`p99_ms`, `error_ratio`, `disk_used_ratio`,
//! `replica_behind_max` ...), a comparison and a threshold, and how long
//! the condition must hold before the alert fires. Each new point is
//! checked against every rule; an alert goes pending, then firing, then
//! resolved, and each change is kept in the history, logged, and posted to
//! the webhook if one is set (`{"text": ..., "alert": ...}`, which Slack's
//! incoming webhooks take as is).
//!
//! Rules are kept in `--alerts FILE` (JSON), or in memory; a server starts
//! with [`defaults`] when there is no file.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::telemetry::now_ms;
use crate::ApiError;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub name: String,
    /// The point's value it watches.
    pub metric: String,
    /// `>`, `>=`, `<` or `<=`.
    pub op: String,
    pub threshold: f64,
    /// How long the condition must hold before the alert fires, in seconds.
    #[serde(default)]
    pub for_s: u64,
    /// `critical`, `warning` or `info`.
    #[serde(default = "warning")]
    pub severity: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

fn warning() -> String {
    "warning".into()
}

fn yes() -> bool {
    true
}

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
        if !self.threshold.is_finite() {
            return Err(ApiError::bad(format!("rule {}: the threshold is a number", self.id)));
        }
        Ok(())
    }

    /// Whether `v` meets the condition.
    pub fn holds(&self, v: f64) -> bool {
        match self.op.as_str() {
            ">" => v > self.threshold,
            ">=" => v >= self.threshold,
            "<" => v < self.threshold,
            _ => v <= self.threshold,
        }
    }
}

/// The rules a server starts with.
pub fn defaults() -> Vec<Rule> {
    let r = |id: &str, name: &str, metric: &str, op: &str, threshold: f64, for_s: u64, severity: &str, description: &str| Rule {
        id: id.into(),
        name: name.into(),
        metric: metric.into(),
        op: op.into(),
        threshold,
        for_s,
        severity: severity.into(),
        enabled: true,
        description: description.into(),
    };
    vec![
        r("failing", "Database refuses writes", "failing", ">", 0.0, 0, "critical", "A write failed partway (a full disk, an I/O error): restart the server once the cause is fixed."),
        r("flush-failing", "Flushes fail", "flush_failing", ">", 0.0, 0, "critical", "Writes still commit to the log until the memtable holds twice its limit. Usually a full disk."),
        r("merge-failing", "Merges fail", "merge_failing", ">", 0.0, 60, "warning", "Segments pile up and queries slow down until merges succeed again."),
        r("disk-full", "Disk almost full", "disk_used_ratio", ">", 0.9, 60, "warning", "Under 10% of the database's disk is left."),
        r("p99", "Query p99 over 1 s", "p99_ms", ">", 1000.0, 60, "warning", "The slowest 1% of queries took over a second for a minute."),
        r("errors", "Query errors over 5%", "error_ratio", ">", 0.05, 60, "warning", "More than 5% of queries failed or timed out for a minute."),
        r("rejected", "Queries refused (429)", "rejected_s", ">", 0.0, 30, "warning", "Queries found no slot (--max-concurrent) or no memory before their deadline."),
        r("5xx", "Server errors", "http_5xx_s", ">", 0.1, 60, "warning", "Requests failed on the server's side."),
        r("replica-behind", "A replica is behind", "replica_behind_max", ">", 1000.0, 60, "warning", "A replica misses over 1,000 transactions its primary committed."),
        r("out-of-sync", "A replica is out of sync", "replicas_out_of_sync", ">", 0.0, 60, "critical", "A replica does not hold what its primary committed: a failover now could lose writes."),
        r("replica-reads", "A replica does not serve reads", "serves_reads", "<", 1.0, 60, "warning", "The replica is catching up with its primary, or lost it."),
        r("query-memory", "Query memory almost spent", "query_memory_ratio", ">", 0.9, 30, "warning", "The running queries hold over 90% of --query-memory-mb; more are refused with 429."),
        r("write-queue", "Writes queue up", "write_queue", ">", 512.0, 30, "warning", "Write requests wait for the writer; at --max-queued-writes they are refused."),
    ]
}

/// A rule's state now.
#[derive(Clone, Debug, Serialize)]
pub struct State {
    /// `ok`, `pending`, `firing`, or `no data` (the point has no such value).
    pub state: &'static str,
    pub since_ms: u64,
    pub value: Option<f64>,
}

/// A change of an alert's state.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub ts_ms: u64,
    pub rule: String,
    pub name: String,
    pub severity: String,
    /// `firing` or `resolved`.
    pub state: &'static str,
    pub value: Option<f64>,
    pub threshold: f64,
    pub op: String,
    pub metric: String,
}

/// Events kept.
pub const EVENTS: usize = 500;

pub struct Alerts {
    rules: Mutex<Vec<Rule>>,
    states: Mutex<HashMap<String, State>>,
    events: Mutex<VecDeque<Arc<Event>>>,
    file: Option<PathBuf>,
    webhook: Option<String>,
    http: reqwest::Client,
}

impl Alerts {
    /// The rules of `file` ([`defaults`] if it does not exist), posting
    /// changes to `webhook`.
    pub fn new(file: Option<PathBuf>, webhook: Option<String>) -> Result<Alerts, String> {
        let rules = match &file {
            Some(p) => match std::fs::read(p) {
                Ok(b) => {
                    let rules: Vec<Rule> = serde_json::from_slice(&b).map_err(|e| format!("--alerts {}: {e}", p.display()))?;
                    for r in &rules {
                        r.check().map_err(|e| format!("--alerts {}: {}", p.display(), e.message))?;
                    }
                    rules
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => defaults(),
                Err(e) => return Err(format!("--alerts {}: {e}", p.display())),
            },
            None => defaults(),
        };
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().map_err(|e| e.to_string())?;
        Ok(Alerts { rules: Mutex::new(rules), states: Mutex::new(HashMap::new()), events: Mutex::new(VecDeque::new()), file, webhook, http })
    }

    pub fn rules(&self) -> Vec<Rule> {
        self.rules.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn file(&self) -> Option<&std::path::Path> {
        self.file.as_deref()
    }

    pub fn webhook(&self) -> bool {
        self.webhook.is_some()
    }

    /// Replaces the rules (and their file), keeping the states of the
    /// rules that stay the same.
    pub fn set_rules(&self, rules: Vec<Rule>) -> Result<(), ApiError> {
        let mut ids = std::collections::HashSet::new();
        for r in &rules {
            r.check()?;
            if !ids.insert(r.id.clone()) {
                return Err(ApiError::bad(format!("two rules have the id {:?}", r.id)));
            }
        }
        if let Some(p) = &self.file {
            let tmp = p.with_extension("tmp");
            let body = serde_json::to_vec_pretty(&rules).map_err(ApiError::internal)?;
            std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, p)).map_err(|e| ApiError::internal(format!("write {}: {e}", p.display())))?;
        }
        let mut now = self.rules.lock().unwrap_or_else(|e| e.into_inner());
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        states.retain(|id, _| rules.iter().any(|r| &r.id == id && now.iter().any(|o| o == r)));
        *now = rules;
        Ok(())
    }

    /// Checks every rule against a new point; returns the changes, which
    /// it keeps, logs and posts.
    pub fn check(&self, point: &Json) -> Vec<Arc<Event>> {
        let t = point["t"].as_u64().unwrap_or_else(now_ms);
        let rules = self.rules();
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let mut changes = Vec::new();
        for r in rules.iter().filter(|r| r.enabled) {
            let v = point.get(&r.metric).and_then(Json::as_f64);
            let s = states.entry(r.id.clone()).or_insert(State { state: "ok", since_ms: t, value: None });
            let was = s.state;
            s.value = v;
            let (next, since) = match (v.map(|v| r.holds(v)), was) {
                (None, "firing") => ("firing", s.since_ms),
                (None, _) => ("no data", if was == "no data" { s.since_ms } else { t }),
                (Some(false), _) => ("ok", if was == "ok" { s.since_ms } else { t }),
                (Some(true), "firing") => ("firing", s.since_ms),
                (Some(true), "pending") if t.saturating_sub(s.since_ms) >= r.for_s * 1000 => ("firing", t),
                (Some(true), "pending") => ("pending", s.since_ms),
                (Some(true), _) if r.for_s == 0 => ("firing", t),
                (Some(true), _) => ("pending", t),
            };
            s.state = next;
            s.since_ms = since;
            let change = match (was, next) {
                (w, "firing") if w != "firing" => Some("firing"),
                ("firing", n) if n != "firing" => Some("resolved"),
                _ => None,
            };
            if let Some(state) = change {
                changes.push(Arc::new(Event { ts_ms: t, rule: r.id.clone(), name: r.name.clone(), severity: r.severity.clone(), state, value: v, threshold: r.threshold, op: r.op.clone(), metric: r.metric.clone() }));
            }
        }
        // Rules removed or disabled have no state.
        states.retain(|id, _| rules.iter().any(|r| r.enabled && &r.id == id));
        drop(states);
        if !changes.is_empty() {
            let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
            for e in &changes {
                if events.len() == EVENTS {
                    events.pop_front();
                }
                events.push_back(e.clone());
                match e.state {
                    "firing" => tracing::warn!(rule = %e.rule, severity = %e.severity, value = ?e.value, threshold = e.threshold, "alert firing: {}", e.name),
                    _ => tracing::info!(rule = %e.rule, value = ?e.value, "alert resolved: {}", e.name),
                }
            }
        }
        changes
    }

    /// Posts changes to the webhook, if there is one.
    pub async fn notify(&self, changes: &[Arc<Event>]) {
        let Some(url) = &self.webhook else { return };
        for e in changes {
            let icon = match (e.state, e.severity.as_str()) {
                ("resolved", _) => "resolved",
                (_, "critical") => "CRITICAL",
                _ => "firing",
            };
            let value = e.value.map_or("no data".to_string(), |v| format!("{v:.3}"));
            let text = format!("[strata {icon}] {}: {} = {value} ({} {})", e.name, e.metric, e.op, e.threshold);
            if let Err(err) = self.http.post(url).json(&json!({"text": text, "alert": &**e})).send().await.and_then(|r| r.error_for_status()) {
                tracing::warn!(error = %err, "could not post an alert to the webhook");
            }
        }
    }

    /// The rules with their states, the alerts firing, and the latest
    /// changes, the newest first.
    pub fn json(&self) -> Json {
        let rules = self.rules();
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let with: Vec<Json> = rules
            .iter()
            .map(|r| {
                let mut j = json!(r);
                j["status"] = json!(states.get(&r.id).cloned().unwrap_or(State { state: if r.enabled { "ok" } else { "disabled" }, since_ms: 0, value: None }));
                if !r.enabled {
                    j["status"]["state"] = json!("disabled");
                }
                j
            })
            .collect();
        let firing: Vec<&Json> = with.iter().filter(|r| r["status"]["state"] == "firing").collect();
        let events: Vec<Arc<Event>> = self.events.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().cloned().collect();
        json!({"rules": with, "firing": firing, "events": events, "file": self.file.as_ref().map(|p| p.display().to_string()), "webhook": self.webhook.is_some()})
    }

    /// Alerts firing, by severity, for `/metrics`.
    pub fn firing(&self) -> Vec<(String, f64)> {
        let rules = self.rules();
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        ["critical", "warning", "info"]
            .iter()
            .map(|sev| {
                let n = rules.iter().filter(|r| r.severity == *sev && states.get(&r.id).is_some_and(|s| s.state == "firing")).count();
                (format!("severity=\"{sev}\""), n as f64)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(t: u64, p99: f64) -> Json {
        json!({"t": t, "p99_ms": p99})
    }

    #[test]
    fn an_alert_fires_after_its_condition_held_and_resolves() {
        let a = Alerts::new(None, None).unwrap();
        a.set_rules(vec![Rule { id: "p99".into(), name: "slow".into(), metric: "p99_ms".into(), op: ">".into(), threshold: 100.0, for_s: 10, severity: "warning".into(), enabled: true, description: String::new() }]).unwrap();
        assert!(a.check(&point(0, 50.0)).is_empty());
        assert!(a.check(&point(2_000, 150.0)).is_empty());
        assert_eq!(a.json()["rules"][0]["status"]["state"], "pending");
        assert!(a.check(&point(8_000, 150.0)).is_empty());
        let fired = a.check(&point(12_000, 150.0));
        assert_eq!((fired.len(), fired[0].state), (1, "firing"));
        assert_eq!(a.json()["firing"].as_array().unwrap().len(), 1);
        assert_eq!(a.firing()[1], ("severity=\"warning\"".to_string(), 1.0));
        // No data keeps it firing; then it resolves.
        assert!(a.check(&json!({"t": 14_000})).is_empty());
        let resolved = a.check(&point(16_000, 10.0));
        assert_eq!(resolved[0].state, "resolved");
        assert_eq!(a.json()["events"].as_array().unwrap().len(), 2);
        assert_eq!(a.json()["events"][0]["state"], "resolved");
    }

    #[test]
    fn rules_are_checked_and_kept_in_their_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alerts.json");
        let a = Alerts::new(Some(path.clone()), None).unwrap();
        assert_eq!(a.rules(), defaults());
        let mut bad = defaults();
        bad[0].op = "!=".into();
        assert!(a.set_rules(bad).is_err());
        let mut two = defaults();
        two[1].id = two[0].id.clone();
        assert!(a.set_rules(two).is_err());
        let mut fewer = defaults();
        fewer.truncate(2);
        a.set_rules(fewer.clone()).unwrap();
        assert_eq!(Alerts::new(Some(path), None).unwrap().rules(), fewer);
        // A zero wait fires at once.
        let fired = a.check(&json!({"t": 1, "failing": 1.0}));
        assert_eq!(fired[0].rule, "failing");
    }
}
