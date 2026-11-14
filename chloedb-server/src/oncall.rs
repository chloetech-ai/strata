//! The on-call agent (`POST /v1/admin/oncall`): a Claude model looks into
//! a firing alert, or a question about the cluster, with the read-only
//! views an operator has (health, alerts, events, queries, logs, load,
//! metrics, contain rules), and answers with what it found and what it
//! would do. It changes nothing: each action it would take is a
//! proposal, kept here, that an admin approves or rejects
//! (`/v1/admin/oncall/proposals`).
//!
//! Every tool call is a request to this server's own route, made with the
//! caller's credential through the same checks and audit as any other
//! (the router copy [`crate::mcp`] calls too), named `oncall` in the query
//! log. An approved proposal is a request to its route with the
//! approver's credential, its reason the proposal's, so the audit log names
//! the admin who approved it, not the agent.
//!
//! A proposal holds for [`PROPOSAL_FOR_MS`] (an hour): an action seen fit
//! then is not applied to a cluster that has moved on since.
//!
//! Each run is kept while it goes, with its steps as they are taken
//! (`/v1/admin/oncall/runs/:id`), so the command center shows it live; a
//! run started with `wait: false` answers at once with its id. A run can
//! be stopped, and followed up with another question in the same
//! conversation (`follows`). The latest [`RUNS`] runs and the proposals
//! are kept beside the audit log (`<audit>.oncall.json`, or
//! `CHLOEDB_ONCALL_STATE`), so a restart does not lose them; the whole
//! conversations of the latest [`CONVERSATIONS`] are kept in memory only
//! (an older run is followed up from its questions and reports).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, Method};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};
use tokio::sync::Notify;

use crate::mcp::{forward, Caller};
use crate::service::Service;
use crate::telemetry::now_ms;
use crate::ApiError;

/// Model calls at most, by default and at most.
pub const STEPS: usize = 12;
pub const MAX_STEPS: usize = 30;
/// Characters of one tool's answer the model sees.
const SHOWN: usize = 12_000;
/// Characters of one tool's answer kept with its step, for people.
const PREVIEW: usize = 4_000;
/// How long a proposal can be approved.
pub const PROPOSAL_FOR_MS: u64 = 3_600_000;
/// Proposals kept, the oldest decided ones dropped first.
const KEPT: usize = 200;
/// Runs kept, the oldest finished ones dropped first.
pub const RUNS: usize = 50;
/// Runs whose whole conversation (tool answers included) is kept for a
/// follow-up.
pub const CONVERSATIONS: usize = 20;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// An action the agent would take, for an admin to approve.
#[derive(Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    /// The run that proposed it.
    pub run: String,
    pub created_ms: u64,
    pub expires_ms: u64,
    /// Who asked the agent.
    pub asked_by: String,
    /// `contain`, `remove_contain`, `cancel_query` or `silence_alert`; a
    /// change request ([`crate::changes`]) is `change`.
    pub action: String,
    /// What it does, in words.
    pub what: String,
    pub why: String,
    /// The request an approval makes.
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Json>,
    /// `pending`, `applying`, `applied`, `failed`, `rejected`, `withdrawn`
    /// (a change request its asker took back) or `expired`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_ms: Option<u64>,
    /// The route's answer when applied, its error when it failed, the
    /// reason when rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Json>,
    /// A change request's tier ([`crate::ops::tier`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<u8>,
    /// A change request's dry run when it was filed ([`crate::changes`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Json>,
    /// The fingerprint of that dry run: an approval dry runs it again, and
    /// a different one fails `plan_stale`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Whether an agent filed it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent: bool,
}

impl Proposal {
    fn state_now(&self, t: u64) -> &str {
        if self.state == "pending" && t >= self.expires_ms {
            "expired"
        } else {
            &self.state
        }
    }

    fn json(&self, t: u64) -> Json {
        let mut j = json!(self);
        j["state"] = json!(self.state_now(t));
        // Every role key reads the list: no credential in what it shows
        // (audit X-D1).
        for k in ["body", "result"] {
            if let Some(v) = j.get(k) {
                j[k] = crate::changes::scrub(v);
            }
        }
        j
    }
}

/// One run of the agent, kept while it goes and after.
#[derive(Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub question: String,
    /// The alert it looks into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<String>,
    /// The run it follows up, in the same conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follows: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub max_steps: usize,
    pub asked_by: String,
    pub started_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<u64>,
    /// `running`, `done`, `failed` or `stopped`.
    pub state: String,
    /// While it runs: what it waits for, `model` or a tool's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doing: Option<String>,
    /// Model calls made.
    pub turns: usize,
    /// The tool calls, in order: `{tool, input, ok, took_ms, at_ms, turn,
    /// error?, proposed?, preview}` (`at_ms` from the start, `preview` the
    /// answer's first [`PREVIEW`] characters).
    pub steps: Vec<Json>,
    /// What the model wrote beside its tool calls: `{before, turn, text}`,
    /// `before` the index of the step it came before.
    pub said: Vec<Json>,
    pub answer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_by: Option<String>,
    pub stop_reason: String,
    pub usage: Json,
    pub took_ms: f64,
    /// The note it wrote on the events timeline, when asked to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<Json>,
    /// The conversation, for a follow-up (never written to disk).
    #[serde(skip)]
    messages: Vec<Json>,
}

/// The first line of a report's summary, for a list.
fn headline(answer: &str) -> String {
    let text = answer.trim();
    let line = match text.find("Summary:") {
        Some(at) => text[at + "Summary:".len()..].trim_start().lines().next().unwrap_or(""),
        None => text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(""),
    };
    let line = line.replace("**", "");
    let line = line.trim().trim_start_matches(['#', ' ']).trim_end();
    if line.chars().count() > 240 {
        format!("{}…", line.chars().take(239).collect::<String>())
    } else {
        line.to_string()
    }
}

/// The proposals and runs of every run, newest last.
#[derive(Default)]
pub struct Desk {
    inner: Mutex<(u64, VecDeque<Proposal>)>,
    runs: Mutex<VecDeque<Run>>,
    /// A running run's stop, by its id.
    stops: Mutex<HashMap<String, Arc<Notify>>>,
    /// Where the runs and proposals are kept, if anywhere.
    file: OnceLock<Option<PathBuf>>,
    saving: Mutex<()>,
}

impl Desk {
    /// Loads what an earlier start of this server kept, once.
    pub fn ready(&self, svc: &Service) {
        if self.file.get().is_none() {
            let path = match chloedb::env::var("CHLOEDB_ONCALL_STATE") {
                Ok(p) if !p.is_empty() => Some(PathBuf::from(p)),
                _ => svc.audit.path().map(|p| p.with_extension("oncall.json")),
            };
            self.open_at(path);
        }
    }

    fn open_at(&self, path: Option<PathBuf>) {
        if self.file.set(path).is_ok() {
            self.load();
        }
    }

    fn load(&self) {
        let Some(Some(path)) = self.file.get() else { return };
        let Ok(bytes) = std::fs::read(path) else { return };
        let j: Json = match serde_json::from_slice(&bytes) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "the on-call agent's runs could not be read");
                return;
            }
        };
        let t = now_ms();
        let mut runs: VecDeque<Run> = serde_json::from_value(j["runs"].clone()).unwrap_or_default();
        for r in runs.iter_mut().filter(|r| r.state == "running") {
            r.state = "failed".into();
            r.doing = None;
            r.error = Some("the server restarted during the run".into());
            r.finished_ms.get_or_insert(t);
        }
        let mut ps: VecDeque<Proposal> = serde_json::from_value(j["proposals"].clone()).unwrap_or_default();
        for p in ps.iter_mut().filter(|p| p.state == "applying") {
            p.state = "failed".into();
            p.result = Some(json!({"error": "the server restarted while it was being applied: check whether it took"}));
            p.decided_ms.get_or_insert(t);
        }
        // A desk kept by a server from before audit X-D1 may hold answers
        // with secrets in them, and decided bodies: they are scrubbed, and
        // the file written again (0600) once the desk is loaded.
        let mut scrubbed = false;
        for p in ps.iter_mut() {
            if let Some(r) = &p.result {
                let c = crate::changes::scrub(r);
                scrubbed |= c != *r;
                p.result = Some(c);
            }
            // A change request filed before it that carries a credential
            // is not applied: it is filed again, without it.
            let carries = p.action == "change" && (crate::catalog::route_of(&p.path).is_some_and(crate::ops::carries_credentials) || p.body.as_ref().is_some_and(|b| crate::changes::secret_in(b).is_some()));
            if p.state == "pending" && carries {
                p.state = "failed".into();
                p.result = Some(json!({"error_code": "scope_refused", "error": "it carries or answers a credential, which a change request no longer does: a person calls the route with their own key"}));
                p.decided_ms.get_or_insert(t);
                scrubbed = true;
            }
            if p.state != "pending" {
                if let Some(b) = &p.body {
                    let c = crate::changes::scrub(b);
                    scrubbed |= c != *b;
                    p.body = Some(c);
                }
            }
        }
        let top = ps.iter().filter_map(|p| p.id.strip_prefix(['p', 'c'])?.parse::<u64>().ok()).max().unwrap_or(0);
        let next = j["next"].as_u64().unwrap_or(0).max(top);
        *lock(&self.inner) = (next, ps);
        if scrubbed {
            self.save();
        } else {
            private(path);
        }
        let mut g = lock(&self.runs);
        if g.is_empty() {
            *g = runs;
        }
    }

    /// Writes the runs and proposals where they are kept.
    fn save(&self) {
        let Some(Some(path)) = self.file.get() else { return };
        let _one = lock(&self.saving);
        let body = {
            let g = lock(&self.inner);
            let mut j = json!({"next": g.0, "proposals": g.1});
            drop(g);
            j["runs"] = json!(*lock(&self.runs));
            serde_json::to_vec(&j).unwrap_or_default()
        };
        let tmp = path.with_extension("oncall.tmp");
        if let Err(e) = write_private(&tmp, &body).and_then(|_| std::fs::rename(&tmp, path)) {
            tracing::warn!(error = %e, path = %path.display(), "could not keep the on-call agent's runs");
        }
    }

    fn add(&self, p: Proposal) -> String {
        self.add_as(p, 'p')
    }

    /// Keeps change request `p` ([`crate::changes`]), pending: its id,
    /// `c<n>`.
    pub fn add_change(&self, p: Proposal) -> String {
        let id = self.add_as(p, 'c');
        self.save();
        id
    }

    /// Proposal or change request `id`, if kept.
    pub fn get(&self, id: &str) -> Option<Json> {
        let t = now_ms();
        lock(&self.inner).1.iter().find(|p| p.id == id).map(|p| p.json(t))
    }

    /// Proposal or change request `id` itself, if kept.
    pub fn proposal_of(&self, id: &str) -> Option<Proposal> {
        lock(&self.inner).1.iter().find(|p| p.id == id).cloned()
    }

    /// Withdraws pending change request `id`: only by whoever filed it.
    pub fn withdraw(&self, id: &str, by: &str, reason: &str) -> Result<Json, ApiError> {
        match self.proposal_of(id) {
            Some(p) if p.asked_by != by => return Err(ApiError::coded(403, "scope_refused", format!("{id} was filed by {}: only they withdraw it (a person rejects it)", p.asked_by))),
            None => return Err(ApiError::coded(404, "not_found", format!("no change request {id}: the server keeps the latest {KEPT}"))),
            _ => {}
        }
        let p = self.take(id)?;
        Ok(self.decide(&p.id, "withdrawn", by, json!({"reason": reason})))
    }

    fn add_as(&self, mut p: Proposal, prefix: char) -> String {
        let mut g = lock(&self.inner);
        g.0 += 1;
        p.id = format!("{prefix}{}", g.0);
        let id = p.id.clone();
        let t = now_ms();
        if g.1.len() >= KEPT {
            // The oldest decided (or expired) one goes; else the oldest.
            let at = g.1.iter().position(|p| p.state_now(t) != "pending" && p.state != "applying").unwrap_or(0);
            g.1.remove(at);
        }
        g.1.push_back(p);
        id
    }

    /// The proposals, newest first; of one state, or of one run.
    pub fn list(&self, state: Option<&str>, run: Option<&str>) -> Vec<Json> {
        let t = now_ms();
        let g = lock(&self.inner);
        g.1.iter().rev().filter(|p| state.is_none_or(|s| p.state_now(t) == s)).filter(|p| run.is_none_or(|r| p.run == r)).map(|p| p.json(t)).collect()
    }

    fn of_run(&self, run: &str) -> Vec<Json> {
        let mut v = self.list(None, Some(run));
        v.reverse();
        v
    }

    /// Marks proposal `id` as being applied, if it is pending: the request
    /// to make.
    fn take(&self, id: &str) -> Result<Proposal, ApiError> {
        let t = now_ms();
        let mut g = lock(&self.inner);
        let p = g.1.iter_mut().find(|p| p.id == id).ok_or_else(|| ApiError { code: None, status: 404, message: format!("no proposal {id}: the server keeps the latest {KEPT}") })?;
        match p.state_now(t) {
            "pending" => {
                p.state = "applying".into();
                Ok(p.clone())
            }
            s => Err(ApiError::coded(409, "change_not_pending", format!("{} {id} is {s}, not pending", if id.starts_with('c') { "change request" } else { "proposal" }))),
        }
    }

    fn decide(&self, id: &str, state: &str, by: &str, result: Json) -> Json {
        let t = now_ms();
        let j = {
            let mut g = lock(&self.inner);
            match g.1.iter_mut().find(|p| p.id == id) {
                Some(p) => {
                    p.state = state.into();
                    p.decided_by = Some(by.to_string());
                    p.decided_ms = Some(t);
                    // What it was answered, and the body once it is decided,
                    // without a credential they may hold (audit X-D1).
                    p.result = Some(crate::changes::scrub(&result));
                    p.body = p.body.as_ref().map(crate::changes::scrub);
                    p.json(t)
                }
                None => Json::Null,
            }
        };
        self.save();
        j
    }

    /// Rejects pending proposal `id`.
    pub fn reject(&self, id: &str, by: &str, reason: &str) -> Result<Json, ApiError> {
        let p = self.take(id)?;
        Ok(self.decide(&p.id, "rejected", by, json!({"reason": reason})))
    }

    /// Adds a run, its id made unique, and drops the oldest finished one
    /// past [`RUNS`], and the conversations past [`CONVERSATIONS`].
    fn begin(&self, mut r: Run) -> (String, Arc<Notify>) {
        let mut g = lock(&self.runs);
        let mut n = r.started_ms;
        while g.iter().any(|x| x.id == format!("oc{n}")) {
            n += 1;
        }
        r.id = format!("oc{n}");
        let id = r.id.clone();
        g.push_back(r);
        while g.len() > RUNS {
            match g.iter().position(|x| x.state != "running") {
                Some(at) => {
                    g.remove(at);
                }
                None => break,
            }
        }
        let keep = g.len().saturating_sub(CONVERSATIONS);
        for x in g.iter_mut().take(keep) {
            x.messages = Vec::new();
        }
        drop(g);
        let stop = Arc::new(Notify::new());
        lock(&self.stops).insert(id.clone(), stop.clone());
        (id, stop)
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut Run)) {
        if let Some(r) = lock(&self.runs).iter_mut().find(|r| r.id == id) {
            f(r);
        }
    }

    fn stopped(&self, id: &str) -> bool {
        lock(&self.runs).iter().any(|r| r.id == id && r.stopped_by.is_some())
    }

    /// Run `id` as it stands: with its proposals, and `run` its id again.
    pub fn run_json(&self, id: &str) -> Option<Json> {
        let mut j = json!(lock(&self.runs).iter().find(|r| r.id == id)?);
        j["run"] = json!(id);
        j["proposals"] = json!(self.of_run(id));
        Some(j)
    }

    /// The runs, newest first, each in short: what it was asked, its state,
    /// its summary's first line, and its proposals (all, and pending).
    pub fn runs(&self) -> Vec<Json> {
        let t = now_ms();
        let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
        for p in lock(&self.inner).1.iter() {
            let c = counts.entry(p.run.clone()).or_default();
            c.0 += 1;
            if p.state_now(t) == "pending" {
                c.1 += 1;
            }
        }
        let g = lock(&self.runs);
        g.iter()
            .rev()
            .map(|r| {
                let (total, pending) = counts.get(&r.id).copied().unwrap_or_default();
                let question = if r.question.chars().count() > 300 { format!("{}…", r.question.chars().take(299).collect::<String>()) } else { r.question.clone() };
                let mut j = json!({
                    "id": r.id, "question": question, "state": r.state, "asked_by": r.asked_by,
                    "started_ms": r.started_ms, "steps": r.steps.len(), "turns": r.turns, "max_steps": r.max_steps,
                    "summary": headline(&r.answer), "model": r.model, "took_ms": r.took_ms, "usage": r.usage,
                    "proposals": {"total": total, "pending": pending},
                });
                for (k, v) in [("alert", &r.alert), ("follows", &r.follows), ("doing", &r.doing), ("error", &r.error), ("stopped_by", &r.stopped_by)] {
                    if let Some(v) = v {
                        j[k] = json!(v);
                    }
                }
                if let Some(f) = r.finished_ms {
                    j["finished_ms"] = json!(f);
                }
                j
            })
            .collect()
    }

    /// How many runs are under way, and proposals wait.
    pub fn counts(&self) -> (usize, usize) {
        let t = now_ms();
        let pending = lock(&self.inner).1.iter().filter(|p| p.state_now(t) == "pending").count();
        (lock(&self.runs).iter().filter(|r| r.state == "running").count(), pending)
    }

    /// Stops run `id` before its next model call (a call under way is
    /// dropped).
    pub fn stop(&self, id: &str, by: &str) -> Result<Json, ApiError> {
        {
            let mut g = lock(&self.runs);
            let r = g.iter_mut().find(|r| r.id == id).ok_or_else(|| ApiError { code: None, status: 404, message: format!("no run {id}: the server keeps the latest {RUNS}") })?;
            if r.state != "running" {
                return Err(ApiError { code: None, status: 409, message: format!("run {id} is {}, not running", r.state) });
            }
            r.stopped_by.get_or_insert_with(|| by.to_string());
        }
        if let Some(n) = lock(&self.stops).get(id) {
            n.notify_one();
        }
        Ok(self.run_json(id).unwrap_or(Json::Null))
    }

    /// The conversation a follow-up of run `id` continues, and the alert it
    /// was about: the whole one while it is kept, else the questions and
    /// reports of the runs it follows, oldest first.
    fn conversation(&self, id: &str) -> Result<(Vec<Json>, Option<String>), ApiError> {
        let g = lock(&self.runs);
        let r = g.iter().find(|r| r.id == id).ok_or_else(|| ApiError { code: None, status: 404, message: format!("no run {id} to follow up: the server keeps the latest {RUNS}") })?;
        if r.state == "running" {
            return Err(ApiError { code: None, status: 409, message: format!("run {id} is still running: follow it up once it has reported") });
        }
        if !r.messages.is_empty() {
            return Ok((r.messages.clone(), r.alert.clone()));
        }
        let mut chain = vec![r];
        while let Some(prev) = chain.last().and_then(|r| r.follows.as_deref()).and_then(|f| g.iter().find(|x| x.id == f)) {
            if chain.len() > RUNS {
                break;
            }
            chain.push(prev);
        }
        let mut messages = Vec::new();
        for r in chain.iter().rev() {
            push_user(&mut messages, &r.question);
            let said = if r.answer.is_empty() { r.error.clone().unwrap_or_else(|| "(no report)".into()) } else { r.answer.clone() };
            messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": said}]}));
        }
        Ok((messages, r.alert.clone()))
    }
}

/// Adds the user's `text` to a conversation: a turn of its own, or added
/// to the last when that is the user's (a run cut off after its tools).
fn push_user(messages: &mut Vec<Json>, text: &str) {
    if let Some(last) = messages.last_mut().filter(|m| m["role"] == "user") {
        let block = json!({"type": "text", "text": text});
        match last["content"].take() {
            Json::String(s) => last["content"] = json!([{"type": "text", "text": s}, block]),
            Json::Array(mut a) => {
                a.push(block);
                last["content"] = Json::Array(a);
            }
            _ => last["content"] = json!([block]),
        }
    } else {
        messages.push(json!({"role": "user", "content": text}));
    }
}

/// What a run is asked.
pub struct OncallReq {
    /// A question, or what to look into.
    pub question: Option<String>,
    /// A firing alert's rule, to look into.
    pub alert: Option<String>,
    /// A run to follow up: the question goes on in its conversation.
    pub follows: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Overrides the server's key for this run (never stored).
    pub api_key: Option<String>,
    pub max_steps: usize,
    /// Also writes the answer as a note on the events timeline.
    pub note: bool,
    /// Answers once the run is done (the default), or at once with its id.
    pub wait: bool,
    /// Who asks, as the audit names them.
    pub by: String,
    /// Who asks: each step with the server's key takes from its model
    /// budget ([`crate::budget`]).
    pub who: Option<crate::keys::Principal>,
}

fn system_prompt(svc: &Service) -> String {
    let name = svc.identity.name.as_deref().map_or("this cluster".to_string(), |n| format!("the cluster {n}"));
    let env = svc.identity.environment.as_deref().map_or(String::new(), |e| format!(" ({e})"));
    format!(
        r#"You are the on-call engineer for {name}{env}, a ChloeDB cluster (ChloeDB is a search database with SQL; a cluster is coordinators over shards, each shard a primary and its replicas). You look into an alert or a question with read-only tools, then report.

Tools:
- health, cluster, load, system: whether nodes are up, the shards and their copies, each copy's load, the host.
- alerts: the alert rules and their states; with `rule`, one rule with its recent events.
- events: the timeline (copies down, primaries changed, deploys, contain rules, notes) of the last minutes.
- queries: running, slow or recent queries, or statistics by statement (fingerprint), with the keys and apps that sent them.
- logs: this server's recent log lines, by level or words.
- timeseries: the server's metrics over the last minutes (rates, latencies, queues, memory).
- contain_rules: the rules that limit or refuse statements or keys now.
- propose_contain, propose_remove_contain, propose_cancel_query, propose_silence_alert: an action you would take. These change nothing: each is a proposal that an admin approves or rejects. Propose only what the evidence supports, the smallest action that helps (limit before refuse, a short expiry), and say why in a sentence.

How to work:
- Look before you conclude: base every claim on what a tool showed, and name it (an event's time, a query's fingerprint, a metric's value). Never invent nodes, numbers or causes.
- Start broad (health, alerts, events), then narrow to what moved.
- If a tool fails, say so and go on with the others.
- Stop as soon as you know enough.
- Never say an action was taken: proposals wait for an admin.

Answer in this shape, in plain sentences:
Summary: one or two sentences.
Evidence: a short list of what you saw.
Likely cause: your best explanation, and how sure you are.
Proposed actions: each proposal's id and what it does, or "none".
Watch: what would show it is getting better or worse."#
    )
}

fn tools() -> Json {
    let none = json!({"type": "object", "properties": {}});
    let why = json!({"type": "string", "description": "Why, in a sentence: the evidence for it."});
    json!([
        {"name": "health", "description": "Whether this server and its shards are up, with their row counts and versions; with fleet, every coordinator's.", "input_schema": {"type": "object", "properties": {"fleet": {"type": "boolean"}}}},
        {"name": "cluster", "description": "The cluster's shards and the copies of each: primary or replica, up or down, lag, slots.", "input_schema": none},
        {"name": "load", "description": "Each copy's load now: queries a second, latency, queue, CPU.", "input_schema": none},
        {"name": "system", "description": "The host: CPU, memory, disk, open files, threads.", "input_schema": none},
        {"name": "alerts", "description": "The alert rules and their states (firing, pending, ok, silenced); with rule, that rule's state and recent events.", "input_schema": {"type": "object", "properties": {"rule": {"type": "string"}}}},
        {"name": "events", "description": "The events timeline of the last `minutes` (default 60), newest first; kind narrows it (a comma-separated list, e.g. copy_down,primary_changed).", "input_schema": {"type": "object", "properties": {"minutes": {"type": "integer"}, "kind": {"type": "string"}}}},
        {"name": "queries", "description": "Queries: `running` now, the `slow` ones, the `recent` ones, or `stats` by statement fingerprint (counts, latency, errors, who sent them).", "input_schema": {"type": "object", "properties": {"which": {"type": "string", "enum": ["running", "slow", "recent", "stats"]}, "limit": {"type": "integer"}}, "required": ["which"]}},
        {"name": "logs", "description": "This server's recent log lines, newest last: of a level (error, warn, info) and up, holding words.", "input_schema": {"type": "object", "properties": {"level": {"type": "string"}, "words": {"type": "string"}, "limit": {"type": "integer"}}}},
        {"name": "timeseries", "description": "The server's metrics, a point each interval, over the last `minutes` (default 15).", "input_schema": {"type": "object", "properties": {"minutes": {"type": "integer"}}}},
        {"name": "contain_rules", "description": "The contain rules in force: what each matches, limits or refuses, until when, and how often it applied.", "input_schema": none},
        {"name": "propose_contain", "description": "Proposes a contain rule: limit (to `concurrency` at once) or refuse the queries of a statement fingerprint, a key, or both, for `expires_s` seconds (default 900). An admin approves it.", "input_schema": {"type": "object", "properties": {"fingerprint": {"type": "string"}, "key": {"type": "string"}, "action": {"type": "string", "enum": ["limit", "refuse"]}, "concurrency": {"type": "integer"}, "expires_s": {"type": "integer"}, "why": why}, "required": ["action", "why"]}},
        {"name": "propose_remove_contain", "description": "Proposes removing a contain rule, by its id. An admin approves it.", "input_schema": {"type": "object", "properties": {"id": {"type": "string"}, "why": why}, "required": ["id", "why"]}},
        {"name": "propose_cancel_query", "description": "Proposes cancelling a running query, by its id. An admin approves it.", "input_schema": {"type": "object", "properties": {"id": {"type": "string"}, "why": why}, "required": ["id", "why"]}},
        {"name": "propose_silence_alert", "description": "Proposes silencing an alert rule for `minutes`, while its cause is known and being handled. An admin approves it.", "input_schema": {"type": "object", "properties": {"rule": {"type": "string"}, "minutes": {"type": "integer"}, "why": why}, "required": ["rule", "minutes", "why"]}},
    ])
}

/// A path segment from the model, which names an id or a rule: only the
/// characters ids and rules have.
fn segment(v: &Json, what: &str) -> Result<String, String> {
    let s = v.as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| format!("{what} is required"))?;
    if s.len() > 200 || !s.chars().all(|c| c.is_ascii_alphanumeric() || "._-@:".contains(c)) {
        return Err(format!("{what} {s:?} is not an id"));
    }
    Ok(s.to_string())
}

fn uint(v: &Json, default: u64, max: u64) -> u64 {
    v.as_u64().unwrap_or(default).clamp(1, max)
}

/// The query string of `pairs`, each value percent-encoded.
fn query_string(pairs: &[(&str, String)]) -> String {
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~,".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    let q: Vec<String> = pairs.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}={}", enc(v))).collect();
    if q.is_empty() {
        String::new()
    } else {
        format!("?{}", q.join("&"))
    }
}

/// The read a tool makes: its route.
pub(crate) fn read_path(name: &str, input: &Json) -> Result<Option<String>, String> {
    let t = now_ms();
    Ok(Some(match name {
        "health" if input["fleet"].as_bool() == Some(true) => "/v1/health/fleet".into(),
        "health" => "/v1/health".into(),
        "cluster" => "/v1/cluster".into(),
        "load" => "/v1/load".into(),
        "system" => "/v1/system".into(),
        "contain_rules" => "/v1/admin/contain".into(),
        "alerts" => match &input["rule"] {
            Json::Null => "/v1/alerts".into(),
            r => format!("/v1/alerts/{}", segment(r, "rule")?),
        },
        "events" => {
            let minutes = uint(&input["minutes"], 60, 7 * 24 * 60);
            let kind = input["kind"].as_str().unwrap_or_default().to_string();
            format!("/v1/events{}", query_string(&[("from", t.saturating_sub(minutes * 60_000).to_string()), ("limit", "200".into()), ("kind", kind)]))
        }
        "queries" => {
            let limit = uint(&input["limit"], 20, 100).to_string();
            match input["which"].as_str() {
                Some("running") => "/v1/queries".into(),
                Some("slow") => format!("/v1/queries/slow?limit={limit}"),
                Some("recent") => format!("/v1/queries/recent?limit={limit}"),
                Some("stats") => format!("/v1/queries/stats?limit={limit}"),
                other => return Err(format!("which is running, slow, recent or stats, not {other:?}")),
            }
        }
        "logs" => {
            let limit = uint(&input["limit"], 100, 500).to_string();
            let level = input["level"].as_str().unwrap_or_default().to_string();
            let words = input["words"].as_str().unwrap_or_default().to_string();
            format!("/v1/logs{}", query_string(&[("limit", limit), ("level", level), ("q", words)]))
        }
        "timeseries" => {
            let minutes = uint(&input["minutes"], 15, 24 * 60);
            format!("/v1/timeseries?since={}", t.saturating_sub(minutes * 60_000))
        }
        _ => return Ok(None),
    }))
}

/// The proposal a `propose_*` tool makes, without its id.
pub(crate) fn proposal(name: &str, input: &Json, run: &str, by: &str) -> Result<Option<Proposal>, String> {
    let why = input["why"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("say why: `why` is required")?.to_string();
    let (action, what, method, path, body) = match name {
        "propose_contain" => {
            let fingerprint = input["fingerprint"].as_str().map(str::trim).filter(|s| !s.is_empty());
            let key = input["key"].as_str().map(str::trim).filter(|s| !s.is_empty());
            if fingerprint.is_none() && key.is_none() {
                return Err("a contain rule matches a fingerprint, a key or both".into());
            }
            let act = match input["action"].as_str() {
                Some(a @ ("limit" | "refuse")) => a,
                other => return Err(format!("action is limit or refuse, not {other:?}")),
            };
            let expires_s = uint(&input["expires_s"], 900, crate::contain::MAX_EXPIRES_S);
            let mut m = json!({});
            if let Some(f) = fingerprint {
                m["fingerprint"] = json!(f);
            }
            if let Some(k) = key {
                m["key"] = json!(k);
            }
            let mut body = json!({"match": m, "action": act, "expires_s": expires_s, "reason": why});
            let whom = [fingerprint.map(|f| format!("statement {f}")), key.map(|k| format!("key {k}"))].into_iter().flatten().collect::<Vec<_>>().join(" from ");
            let what = if act == "limit" {
                let c = input["concurrency"].as_u64().ok_or("a limit needs `concurrency`: the queries that run at once")?.min(10_000);
                body["concurrency"] = json!(c);
                format!("limit {whom} to {c} at once for {} min", expires_s.div_ceil(60))
            } else {
                format!("refuse {whom} for {} min", expires_s.div_ceil(60))
            };
            ("contain", what, Method::POST, "/v1/admin/contain".to_string(), Some(body))
        }
        "propose_remove_contain" => {
            let id = segment(&input["id"], "id")?;
            ("remove_contain", format!("remove contain rule {id}"), Method::DELETE, format!("/v1/admin/contain/{id}"), None)
        }
        "propose_cancel_query" => {
            let id = segment(&input["id"], "id")?;
            ("cancel_query", format!("cancel query {id}"), Method::DELETE, format!("/v1/queries/{id}"), None)
        }
        "propose_silence_alert" => {
            let rule = segment(&input["rule"], "rule")?;
            let minutes = uint(&input["minutes"], 30, 24 * 60);
            ("silence_alert", format!("silence alert {rule} for {minutes} min"), Method::POST, format!("/v1/alerts/{rule}/silence"), Some(json!({"minutes": minutes, "reason": why})))
        }
        _ => return Ok(None),
    };
    let t = now_ms();
    Ok(Some(Proposal {
        id: String::new(),
        run: run.to_string(),
        created_ms: t,
        expires_ms: t + PROPOSAL_FOR_MS,
        asked_by: by.to_string(),
        action: action.into(),
        what,
        why,
        method: method.to_string(),
        path,
        body,
        state: "pending".into(),
        decided_by: None,
        decided_ms: None,
        result: None,
        tier: Some(1),
        plan: None,
        fingerprint: None,
        agent: true,
    }))
}

/// What the model sees of a tool's answer: its JSON, cut to [`SHOWN`].
fn shown(j: &Json) -> String {
    let s = j.to_string();
    if s.len() <= SHOWN {
        return s;
    }
    format!("{}… (cut: {} characters in all; narrow the call to see more)", crate::obs::truncate(&s, SHOWN), s.len())
}

/// The caller's headers, its app named `oncall` unless it names one.
fn as_oncall(headers: &HeaderMap) -> HeaderMap {
    let mut h = headers.clone();
    if !h.contains_key("x-strata-app") {
        h.insert("x-strata-app", HeaderValue::from_static("oncall"));
    }
    h
}


/// How a run's tool loop ended, short of an error.
enum Outcome {
    Report { answer: String, stop: String },
    Stopped,
}

/// One run: the model's tool loop, then its answer, steps and proposals.
/// The run goes on in a task of its own, kept in the desk as it goes; with
/// `wait` the answer is the finished run, else its id at once.
pub async fn run(svc: &Arc<Service>, router: Option<Router>, headers: &HeaderMap, peer: Option<std::net::SocketAddr>, req: OncallReq) -> Result<Json, ApiError> {
    svc.oncall.ready(svc);
    let router = router.ok_or_else(|| ApiError::internal("the on-call agent's routes are missing"))?;
    let key = req.api_key.clone().or_else(|| svc.agent.api_key.clone()).ok_or_else(|| ApiError {
        code: None, status: 400,
        message: "no Anthropic API key: start the server with ANTHROPIC_API_KEY set, or pass api_key".into(),
    })?;
    let (mut messages, alert) = match &req.follows {
        Some(f) if req.question.is_none() => return Err(ApiError::bad(format!("a follow-up of {f} needs a \"question\""))),
        Some(f) => svc.oncall.conversation(f)?,
        None => (Vec::new(), req.alert.clone()),
    };
    let question = match (&req.question, &req.alert) {
        (Some(q), _) => q.clone(),
        (None, Some(rule)) => format!("Alert {rule} is firing. Find out why, how bad it is, and what to do."),
        (None, None) => return Err(ApiError::bad("the body needs a \"question\" or an \"alert\" (a rule's name)")),
    };
    let question = match (&req.question, &req.alert, &req.follows) {
        (Some(_), Some(rule), None) => format!("{question}\n\n(About alert {rule}.)"),
        _ => question,
    };
    push_user(&mut messages, &question);
    let model = req.model.clone().unwrap_or_else(|| svc.agent.model.clone());
    let (id, stop) = svc.oncall.begin(Run {
        id: String::new(),
        question,
        alert,
        follows: req.follows.clone(),
        model: model.clone(),
        effort: req.effort.clone(),
        max_steps: req.max_steps,
        asked_by: req.by.clone(),
        started_ms: now_ms(),
        finished_ms: None,
        state: "running".into(),
        doing: Some("model".into()),
        turns: 0,
        steps: Vec::new(),
        said: Vec::new(),
        answer: String::new(),
        error: None,
        stopped_by: None,
        stop_reason: String::new(),
        usage: json!({"input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0}),
        took_ms: 0.0,
        note: None,
        messages: Vec::new(),
    });
    let wait = req.wait;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (task_svc, task_id, headers) = (svc.clone(), id.clone(), as_oncall(headers));
    tokio::spawn(async move {
        let (svc, id) = (task_svc, task_id);
        let caller = Caller { headers: &headers, peer, who: None };
        let t0 = Instant::now();
        let r = drive(&svc, &router, &caller, &key, &id, &model, &mut messages, &req, &stop).await;
        let took = t0.elapsed().as_secs_f64() * 1e3;
        let note = match &r {
            Ok(Outcome::Report { answer, .. }) if req.note => Some(note(&svc, &router, &caller, &id, answer).await),
            _ => None,
        };
        svc.oncall.update(&id, |run| {
            match &r {
                Ok(Outcome::Report { answer, stop }) => {
                    run.state = "done".into();
                    run.answer = answer.clone();
                    run.stop_reason = stop.clone();
                }
                Ok(Outcome::Stopped) => {
                    run.state = "stopped".into();
                    run.stop_reason = "stopped".into();
                    run.answer = format!("Stopped by {} before it reported.", run.stopped_by.as_deref().unwrap_or("an admin"));
                }
                Err(e) => {
                    run.state = "failed".into();
                    run.error = Some(e.message.clone());
                }
            }
            run.doing = None;
            run.finished_ms = Some(now_ms());
            run.took_ms = took;
            run.note = note;
            run.messages = std::mem::take(&mut messages);
        });
        lock(&svc.oncall.stops).remove(&id);
        svc.oncall.save();
        let _ = tx.send(r.map(|_| ()));
    });
    if !wait {
        return Ok(json!({"run": id, "state": "running"}));
    }
    match rx.await {
        Ok(Ok(())) => svc.oncall.run_json(&id).ok_or_else(|| ApiError::internal(format!("run {id} is no longer kept"))),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ApiError::internal("the on-call run ended without an answer")),
    }
}

/// The model's tool loop of run `id`, its progress written to the desk.
#[allow(clippy::too_many_arguments)]
async fn drive(svc: &Arc<Service>, router: &Router, caller: &Caller<'_>, key: &str, id: &str, model: &str, messages: &mut Vec<Json>, req: &OncallReq, stop: &Notify) -> Result<Outcome, ApiError> {
    let client = crate::http_client().timeout(Duration::from_secs(300)).build().map_err(ApiError::internal)?;
    let t0 = Instant::now();
    let (mut input_tokens, mut output_tokens, mut cache_read) = (0u64, 0u64, 0u64);
    let mut stop_reason = String::new();
    let mut steps = 0usize;
    for step in 0..req.max_steps {
        if svc.oncall.stopped(id) {
            return Ok(Outcome::Stopped);
        }
        svc.oncall.update(id, |r| r.doing = Some("model".into()));
        let mut body = json!({
            "model": model,
            "max_tokens": 16000,
            "system": system_prompt(svc),
            "tools": tools(),
            "messages": messages,
            "cache_control": {"type": "ephemeral"},
        });
        if step + 1 == req.max_steps {
            body["tool_choice"] = json!({"type": "none"});
        }
        if let Some(e) = &req.effort {
            body["output_config"] = json!({"effort": e});
        }
        if req.api_key.is_none() {
            svc.model_budget.take(req.who.as_ref(), 1, "the on-call agent")?;
        }
        let resp = tokio::select! {
            r = crate::agent::call(&client, &svc.agent.base_url, key, &body) => r?,
            _ = stop.notified() => return Ok(Outcome::Stopped),
        };
        let u = &resp["usage"];
        input_tokens += u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        output_tokens += u["output_tokens"].as_u64().unwrap_or(0);
        stop_reason = resp["stop_reason"].as_str().unwrap_or("").to_string();
        let content = resp["content"].as_array().cloned().unwrap_or_default();
        messages.push(json!({"role": "assistant", "content": content}));
        let usage = json!({"input_tokens": input_tokens, "output_tokens": output_tokens, "cache_read_input_tokens": cache_read});
        svc.oncall.update(id, |r| {
            r.turns = step + 1;
            r.usage = usage;
        });
        match stop_reason.as_str() {
            "tool_use" => {
                let said: Vec<&str> = content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).map(str::trim).filter(|t| !t.is_empty()).collect();
                if !said.is_empty() {
                    let s = json!({"before": steps, "turn": step + 1, "text": said.join("\n")});
                    svc.oncall.update(id, |r| r.said.push(s));
                }
                let mut results = Vec::new();
                for b in content.iter().filter(|b| b["type"] == "tool_use") {
                    let (use_id, name, input) = (b["id"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or(""), &b["input"]);
                    svc.oncall.update(id, |r| r.doing = Some(name.to_string()));
                    let at_ms = t0.elapsed().as_secs_f64() * 1e3;
                    let t = Instant::now();
                    let r: Result<Json, String> = match read_path(name, input) {
                        Ok(Some(path)) => forward(router, caller, Method::GET, &path, None).await,
                        Ok(None) => match proposal(name, input, id, &req.by) {
                            Ok(Some(p)) => {
                                let what = p.what.clone();
                                let pid = svc.oncall.add(p);
                                Ok(json!({"proposed": pid, "what": what, "state": "pending: an admin approves or rejects it"}))
                            }
                            Ok(None) => Err(format!("no tool {name:?}")),
                            Err(e) => Err(e),
                        },
                        Err(e) => Err(e),
                    };
                    let ms = t.elapsed().as_secs_f64() * 1e3;
                    let text = match &r {
                        Ok(j) => shown(j),
                        Err(e) => e.clone(),
                    };
                    let mut ui = json!({"tool": name, "input": input, "ok": r.is_ok(), "took_ms": ms, "at_ms": at_ms, "turn": step + 1, "preview": crate::obs::truncate(&text, PREVIEW)});
                    match &r {
                        Ok(j) if j.get("proposed").is_some() => ui["proposed"] = j["proposed"].clone(),
                        Err(e) => ui["error"] = json!(e),
                        _ => {}
                    }
                    steps += 1;
                    svc.oncall.update(id, |r| r.steps.push(ui));
                    results.push(match r {
                        Ok(_) => json!({"type": "tool_result", "tool_use_id": use_id, "content": text}),
                        Err(e) => json!({"type": "tool_result", "tool_use_id": use_id, "content": e, "is_error": true}),
                    });
                }
                if step + 2 == req.max_steps {
                    results.push(json!({"type": "text", "text": "That was the last tool call: report now from what you found."}));
                }
                messages.push(json!({"role": "user", "content": results}));
            }
            "pause_turn" => continue,
            "refusal" => return Ok(Outcome::Report { answer: "The model declined to look into this.".into(), stop: stop_reason }),
            _ => {
                let text: Vec<&str> = content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
                let mut answer = text.join("\n");
                if stop_reason == "max_tokens" {
                    answer.push_str("\n\n(The report was cut off at the output limit.)");
                }
                if answer.trim().is_empty() {
                    answer = "The model ended without a report.".into();
                }
                return Ok(Outcome::Report { answer, stop: stop_reason });
            }
        }
    }
    Ok(Outcome::Report { answer: "No report within the step limit; the tools the agent called are listed below.".into(), stop: stop_reason })
}

/// Writes run `id`'s report as a note on the events timeline.
async fn note(svc: &Arc<Service>, router: &Router, caller: &Caller<'_>, id: &str, answer: &str) -> Json {
    let ids: Vec<String> = svc.oncall.of_run(id).iter().filter_map(|p| p["id"].as_str().map(String::from)).collect();
    let tail = if ids.is_empty() { String::new() } else { format!(" Proposals: {}.", ids.join(", ")) };
    let head = format!("On-call agent ({id}): ");
    let room = 2000usize.saturating_sub(head.chars().count() + tail.chars().count());
    let text = format!("{head}{}{tail}", answer.chars().take(room).collect::<String>());
    match crate::mcp::forward_with(router, caller, Method::POST, "/v1/events/notes", Some(json!({"text": text})), &[("x-strata-reason", format!("the report of on-call run {id}"))]).await {
        Ok(j) => j,
        Err(e) => json!({"error": e}),
    }
}

/// Applies pending proposal or change request `id`: its request, with
/// the approver's credential and the proposal's reason. Refused to an
/// agent (`approval_refused`); a change request whose dry run now differs
/// from the one it was filed with fails `plan_stale` and is not applied.
#[allow(clippy::too_many_arguments)]
pub async fn approve(svc: &Arc<Service>, router: Option<Router>, headers: &HeaderMap, peer: Option<std::net::SocketAddr>, id: &str, by: &str, approver_agent: bool) -> Result<Json, ApiError> {
    svc.oncall.ready(svc);
    if approver_agent {
        return Err(ApiError::coded(403, "approval_refused", format!("{id} is approved by a person with an admin key, in the command center or with the admin's own key; an agent (an agent key, MCP, the on-call agent, x-chloedb-agent-*) never approves")));
    }
    let router = router.ok_or_else(|| ApiError::internal("the on-call agent's routes are missing"))?;
    let p = svc.oncall.take(id)?;
    let h = crate::changes::applying(headers, &p.id, &p.why);
    let caller = Caller { headers: &h, peer, who: None };
    if p.fingerprint.is_some() {
        if let Err(now) = crate::changes::still_holds(svc, &router, &caller, &p).await {
            let was = p.plan.clone().unwrap_or(Json::Null);
            svc.oncall.decide(&p.id, "failed", by, json!({"error_code": "plan_stale", "error": "the cluster changed since the change was planned", "was": was, "now": now}));
            return Err(ApiError::coded(409, "plan_stale", format!("{}: the cluster changed since it was planned (its dry run differs now); it was not applied: file it again", p.id)));
        }
    }
    let method = Method::from_bytes(p.method.as_bytes()).unwrap_or(Method::POST);
    let (state, result) = match forward(&router, &caller, method, &p.path, p.body.clone()).await {
        Ok(j) => ("applied", j),
        Err(e) => ("failed", json!({"error": e})),
    };
    tracing::info!(proposal = %p.id, by, state, "{} {}: {}", if p.action == "change" { "change request" } else { "on-call proposal" }, if state == "applied" { "applied" } else { "failed" }, p.what);
    Ok(svc.oncall.decide(&p.id, state, by, result))
}

/// Writes `body` to `path`, readable by this server's user alone (0600 on
/// Unix): the desk holds what agents were told and asked (audit X-D1).
fn write_private(path: &std::path::Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(path)?;
    // A file left by an earlier write keeps its mode: set it.
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    f.write_all(body)?;
    f.sync_all()
}

/// Makes the desk file at `path` readable by this server's user alone, as
/// a server from before audit X-D1 did not.
fn private(path: &std::path::Path) {
    #[cfg(unix)]
    if let Err(e) = std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600)) {
        tracing::warn!(error = %e, path = %path.display(), "could not make the on-call desk readable by this server alone");
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_desk_is_this_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.oncall.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"{\"next\": 0}").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"next\": 0}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        private(&path);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn tools_read_their_routes() {
        assert_eq!(read_path("health", &json!({})).unwrap().unwrap(), "/v1/health");
        assert_eq!(read_path("health", &json!({"fleet": true})).unwrap().unwrap(), "/v1/health/fleet");
        assert_eq!(read_path("alerts", &json!({"rule": "p99_high"})).unwrap().unwrap(), "/v1/alerts/p99_high");
        assert!(read_path("alerts", &json!({"rule": "../admin/keys"})).is_err());
        assert_eq!(read_path("queries", &json!({"which": "slow", "limit": 500})).unwrap().unwrap(), "/v1/queries/slow?limit=100");
        assert!(read_path("queries", &json!({})).is_err());
        let logs = read_path("logs", &json!({"level": "warn", "words": "disk full&x=1"})).unwrap().unwrap();
        assert_eq!(logs, "/v1/logs?limit=100&level=warn&q=disk%20full%26x%3D1");
        let events = read_path("events", &json!({"kind": "copy_down,primary_changed"})).unwrap().unwrap();
        assert!(events.starts_with("/v1/events?from=") && events.ends_with("&limit=200&kind=copy_down,primary_changed"), "{events}");
        assert_eq!(read_path("propose_contain", &json!({})).unwrap(), None);
    }

    #[test]
    fn proposals_say_what_they_do() {
        let p = proposal("propose_contain", &json!({"fingerprint": "ab12", "action": "limit", "concurrency": 2, "why": "it holds every core"}), "oc1", "admin").unwrap().unwrap();
        assert_eq!((p.action.as_str(), p.what.as_str(), p.method.as_str(), p.path.as_str()), ("contain", "limit statement ab12 to 2 at once for 15 min", "POST", "/v1/admin/contain"));
        assert_eq!(p.body.unwrap(), json!({"match": {"fingerprint": "ab12"}, "action": "limit", "concurrency": 2, "expires_s": 900, "reason": "it holds every core"}));
        let e = |name: &str, input: Json| proposal(name, &input, "oc1", "admin").err().unwrap();
        assert!(e("propose_contain", json!({"action": "refuse", "why": "x"})).contains("fingerprint, a key"));
        assert!(e("propose_contain", json!({"key": "k1", "action": "limit", "why": "x"})).contains("concurrency"));
        assert!(e("propose_cancel_query", json!({"id": "7"})).contains("why"));
        assert!(e("propose_cancel_query", json!({"id": "7/../../x", "why": "y"})).contains("not an id"));
        let s = proposal("propose_silence_alert", &json!({"rule": "copy_down", "minutes": 20, "why": "a replica is being rebuilt"}), "oc1", "admin").unwrap().unwrap();
        assert_eq!((s.path.as_str(), s.body.unwrap()), ("/v1/alerts/copy_down/silence", json!({"minutes": 20, "reason": "a replica is being rebuilt"})));
    }

    #[test]
    fn a_proposal_is_decided_once_and_expires() {
        let desk = Desk::default();
        let p = || proposal("propose_cancel_query", &json!({"id": "9", "why": "it runs for minutes"}), "oc1", "admin").unwrap().unwrap();
        let a = desk.add(p());
        let b = desk.add(p());
        assert_eq!((a.as_str(), b.as_str()), ("p1", "p2"));
        assert_eq!(desk.list(Some("pending"), None).len(), 2);
        desk.reject(&a, "ops", "not now").unwrap();
        assert_eq!(desk.reject(&a, "ops", "again").err().unwrap().status, 409);
        assert_eq!(desk.list(Some("rejected"), None)[0]["result"]["reason"], "not now");
        // An hour later it can no longer be approved.
        lock(&desk.inner).1.iter_mut().find(|p| p.id == b).unwrap().expires_ms = now_ms() - 1;
        let e = desk.take(&b).err().unwrap();
        assert!(e.status == 409 && e.message.contains("expired"), "{}", e.message);
        assert_eq!(desk.take("p9").err().unwrap().status, 404);
        assert_eq!(desk.of_run("oc1").len(), 2);
    }

    fn a_run(question: &str, follows: Option<&str>) -> Run {
        Run {
            id: String::new(),
            question: question.into(),
            alert: Some("p99".into()),
            follows: follows.map(String::from),
            model: "m".into(),
            effort: None,
            max_steps: 12,
            asked_by: "admin".into(),
            started_ms: 1_000,
            finished_ms: None,
            state: "running".into(),
            doing: Some("model".into()),
            turns: 0,
            steps: Vec::new(),
            said: Vec::new(),
            answer: String::new(),
            error: None,
            stopped_by: None,
            stop_reason: String::new(),
            usage: json!({}),
            took_ms: 0.0,
            note: None,
            messages: Vec::new(),
        }
    }

    #[test]
    fn runs_and_proposals_outlive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.audit.oncall.json");
        let desk = Desk::default();
        desk.open_at(Some(path.clone()));
        let (a, _) = desk.begin(a_run("why is it slow?", None));
        let (b, _) = desk.begin(a_run("and now?", Some(&a)));
        assert_ne!(a, b, "two runs in the same millisecond get their own ids");
        desk.update(&a, |r| {
            r.state = "done".into();
            r.answer = "Summary: **one statement** holds every core.\nEvidence: ...".into();
            r.messages = vec![json!({"role": "user", "content": "x"})];
        });
        let p = || proposal("propose_cancel_query", &json!({"id": "9", "why": "it runs for minutes"}), &a, "admin").unwrap().unwrap();
        let (p1, p2) = (desk.add(p()), desk.add(p()));
        desk.take(&p1).unwrap();
        desk.reject(&p2, "ops", "no").unwrap();
        let list = desk.runs();
        assert_eq!((list[0]["id"].as_str(), list[1]["summary"].as_str()), (Some(b.as_str()), Some("one statement holds every core.")));
        assert_eq!(list[1]["proposals"], json!({"total": 2, "pending": 0}));
        assert_eq!(desk.counts(), (1, 0));
        // The conversation is kept in memory only.
        assert!(std::fs::read_to_string(&path).unwrap().contains("why is it slow?"));
        assert!(!std::fs::read_to_string(&path).unwrap().contains("\"messages\""));

        let again = Desk::default();
        again.open_at(Some(path));
        let run = again.run_json(&b).unwrap();
        assert_eq!((run["state"].as_str(), run["error"].as_str()), (Some("failed"), Some("the server restarted during the run")));
        let ps = again.list(None, Some(&a));
        assert_eq!((ps[0]["state"].as_str(), ps[1]["state"].as_str()), (Some("rejected"), Some("failed")), "{ps:?}");
        assert!(ps[1]["result"]["error"].as_str().unwrap().contains("restarted"));
        assert_eq!(again.add(p()), "p3");
        // Its follow-up goes on from the questions and reports.
        let (m, alert) = again.conversation(&a).unwrap();
        assert_eq!((m.len(), alert.as_deref()), (2, Some("p99")));
        assert_eq!(m[1]["content"][0]["text"].as_str().unwrap(), "Summary: **one statement** holds every core.\nEvidence: ...");
        let (m, _) = again.conversation(&b).unwrap();
        assert_eq!(m.len(), 4, "{m:?}");
        assert_eq!(m[3]["content"][0]["text"], "the server restarted during the run");
    }

    #[test]
    fn a_run_is_stopped_once_and_only_while_it_runs() {
        let desk = Desk::default();
        let (a, stop) = desk.begin(a_run("q", None));
        assert!(!desk.stopped(&a));
        assert_eq!(desk.conversation(&a).err().unwrap().status, 409);
        desk.stop(&a, "ops-ana").unwrap();
        desk.stop(&a, "ops-ben").unwrap();
        assert!(desk.stopped(&a));
        assert_eq!(desk.run_json(&a).unwrap()["stopped_by"], "ops-ana");
        // The permit waits for the model call under way, or the next.
        let waited = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async { tokio::time::timeout(Duration::from_secs(1), stop.notified()).await.is_ok() });
        assert!(waited);
        desk.update(&a, |r| r.state = "stopped".into());
        assert_eq!(desk.stop(&a, "ops").err().unwrap().status, 409);
        assert_eq!(desk.stop("oc1", "ops").err().unwrap().status, 404);
    }

    #[test]
    fn only_the_latest_runs_and_conversations_are_kept() {
        let desk = Desk::default();
        let (first, _) = desk.begin(a_run("first, still running", None));
        let mut ids = Vec::new();
        for i in 0..RUNS + 5 {
            let (id, _) = desk.begin(a_run(&format!("q{i}"), None));
            desk.update(&id, |r| {
                r.state = "done".into();
                r.messages = vec![json!({"role": "user", "content": "x"})];
            });
            ids.push(id);
        }
        let g = lock(&desk.runs);
        assert_eq!(g.len(), RUNS);
        assert_eq!(g[0].id, first, "a running run stays");
        assert_eq!(g.iter().filter(|r| !r.messages.is_empty()).count(), CONVERSATIONS);
        assert!(g.back().unwrap().id == *ids.last().unwrap());
    }

    #[test]
    fn a_follow_up_joins_a_cut_off_turn() {
        let mut m = vec![json!({"role": "user", "content": "q"}), json!({"role": "assistant", "content": []}), json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "{}"}]})];
        push_user(&mut m, "and now?");
        assert_eq!(m.len(), 3);
        assert_eq!(m[2]["content"][1], json!({"type": "text", "text": "and now?"}));
        let mut m = vec![json!({"role": "user", "content": "q"})];
        push_user(&mut m, "more");
        assert_eq!(m[0]["content"], json!([{"type": "text", "text": "q"}, {"type": "text", "text": "more"}]));
        assert_eq!(headline("Starting.\n\nSummary: all is well.\nEvidence: none"), "all is well.");
        assert_eq!(headline("## Summary:\n\n**Two copies are down.**"), "Two copies are down.");
        assert_eq!(headline("No sections here."), "No sections here.");
    }
}
