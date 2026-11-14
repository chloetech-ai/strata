//! Simulated users (SPEC.md, "Fourth build", use case 2): a persona (a model
//! told who it is) does a task on an agent's previewed app, in a real
//! Chromium through Playwright, and what got in its way becomes findings.
//! The runner is `chloedb-swarm`'s `src/workbench.mjs` (Node): it reads a
//! job as JSON on stdin and writes JSON lines (`start`, `step`, `finding`,
//! `done`, `error`); the model is `claude -p` on the user's own login, or
//! the API with the user's key when one is set. Runs and findings are rows
//! `swarm_runs` and `swarm_findings`; a finding goes to the preview's agent
//! as a steer, or to you as a Decide question.
//!
//! Nothing runs until the user presses Start or an agent calls
//! `test_with_persona`. Without Node, the script or Playwright, the routes
//! answer `{"available": false, "why": ...}`.
//!
//! The runner: `CHLOEDB_WORKBENCH_SWARM_CMD` (a program run as it is, for
//! tests), else `node` (or `CHLOEDB_WORKBENCH_NODE`) with the script at
//! `CHLOEDB_WORKBENCH_SWARM` (the file, or a chloedb-swarm checkout), else
//! the `chloedb-swarm/src/workbench.mjs` beside this program's checkout.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::db::{get_int, get_ms, get_str, iso, now_ms, opt_str, ts};
use crate::levels::{new_id, Levels};
use crate::questions::Question;
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{one_line, ApiError, Supervisor};

/// Runs at once, over all agents.
const MAX_LIVE: usize = 2;
/// Steps kept of a run, and findings of a run.
const STEPS_MAX: usize = 200;
const FINDINGS_MAX: usize = 30;
/// The longest a run plays.
const RUN_SECONDS: u64 = 900;

/// The personas a test can pick by id.
pub fn personas() -> Json {
    json!([
        { "id": "first-timer", "name": "Maya Chen", "role": "a first-time visitor on her phone", "traits": ["Skims; taps the biggest button.", "Gives up after two things go wrong."], "device": "phone" },
        { "id": "keyboard", "name": "Ken Okafor", "role": "a power user who uses only the keyboard", "traits": ["Tabs through every form.", "Expects Enter to submit."], "device": "desktop" },
        { "id": "careful", "name": "Ruth Alvarez", "role": "71, careful, reads every label", "traits": ["Reads prices twice.", "Distrusts anything that changes without asking."], "device": "desktop" },
        { "id": "hurried", "name": "Sam Patel", "role": "in a hurry between meetings", "traits": ["Double-clicks.", "Uses the back button a lot."], "device": "desktop" },
        { "id": "edge-cases", "name": "Ines Duarte", "role": "a QA engineer trying edge cases", "traits": ["Empty fields, long text, emoji, negative numbers.", "Reloads in the middle of things."], "device": "desktop" }
    ])
}

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/swarm/personas", get(personas_route))
        .route("/api/swarm/runs", get(runs_route).post(start_route))
        .route("/api/swarm/runs/:id", get(run_route))
        .route("/api/swarm/runs/:id/stop", post(stop_route))
        .route("/api/swarm/findings/:id/:verb", post(finding_route))
}

// ------------------------------------------------------------- the runner

/// How to start the runner: a program and its first arguments.
#[derive(Clone, Debug)]
struct Runner {
    program: String,
    args: Vec<String>,
}

fn script_path() -> Option<PathBuf> {
    let from = |p: PathBuf| if p.is_dir() { p.join("src/workbench.mjs") } else { p };
    if let Some(p) = std::env::var_os("CHLOEDB_WORKBENCH_SWARM").filter(|p| !p.is_empty()) {
        return Some(from(PathBuf::from(p)));
    }
    let exe = std::env::current_exe().ok();
    for dir in exe.iter().flat_map(|e| e.ancestors()) {
        let p = dir.join("chloedb-swarm/src/workbench.mjs");
        if p.is_file() {
            return Some(p);
        }
    }
    let built = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../chloedb-swarm/src/workbench.mjs"));
    built.is_file().then_some(built)
}

fn runner() -> Result<Runner, String> {
    if let Some(cmd) = std::env::var_os("CHLOEDB_WORKBENCH_SWARM_CMD").filter(|c| !c.is_empty()) {
        return Ok(Runner { program: cmd.to_string_lossy().to_string(), args: Vec::new() });
    }
    let script = script_path().ok_or("chloedb-swarm's src/workbench.mjs is not found: set CHLOEDB_WORKBENCH_SWARM to a chloedb-swarm checkout")?;
    let node = std::env::var("CHLOEDB_WORKBENCH_NODE").ok().filter(|n| !n.is_empty()).unwrap_or_else(|| "node".into());
    Ok(Runner { program: node, args: vec![script.to_string_lossy().to_string()] })
}

/// Runs a program with a deadline: its stdout, or why not.
fn run_for(program: &str, args: &[String], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("cannot run {program}: {e}"))?;
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let end = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} did not answer in {} s", timeout.as_secs()));
            }
        }
    }
    reader.join().map_err(|_| "the runner's output was lost".to_string())
}

/// Whether persona runs can start here: `{available, why, adapter}`, from
/// the runner's `--check`, kept for a minute.
pub fn availability(claude_bin: &str) -> Json {
    static CACHE: Mutex<Option<(Instant, Json)>> = Mutex::new(None);
    if let Some((at, v)) = CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(60) {
            return v.clone();
        }
    }
    let v = match runner() {
        Err(why) => json!({ "available": false, "why": why, "adapter": null }),
        Ok(r) => {
            let mut args = r.args.clone();
            args.extend(["--check".to_string(), "--claude".to_string(), claude_bin.to_string()]);
            match run_for(&r.program, &args, Duration::from_secs(20)) {
                Err(e) => json!({ "available": false, "why": format!("{e}: persona tests need Node 20 or later (https://nodejs.org) and chloedb-swarm"), "adapter": null }),
                Ok(out) => match serde_json::from_str::<Json>(out.trim()) {
                    Ok(c) if c["ok"] == true => json!({ "available": true, "why": null, "adapter": c["adapter"] }),
                    Ok(c) => json!({ "available": false, "why": c["why"].as_str().unwrap_or("the runner says it cannot run"), "adapter": c["adapter"] }),
                    Err(_) => json!({ "available": false, "why": format!("the runner's check printed no JSON: {}", one_line(out.trim(), 200)), "adapter": null }),
                },
            }
        }
    };
    *CACHE.lock().unwrap() = Some((Instant::now(), v.clone()));
    v
}

// ------------------------------------------------------------------- rows

const RUN_COLS: &str = "id, agent, repo, persona, task, model, adapter, started_by, state, outcome, answer, steps, step_count, max_steps, error, cost_usd, created, ended";
const F_COLS: &str = "id, run, agent, repo, kind, severity, title, detail, step, url, state, question, steer, created, updated";

#[derive(Clone, Debug, Default)]
pub struct Run {
    pub id: String,
    pub agent: String,
    pub repo: String,
    pub persona: Json,
    pub task: String,
    pub model: String,
    pub adapter: Option<String>,
    pub by: String,
    /// starting | running | done | failed | stopped
    pub state: String,
    /// done | gave_up | out_of_steps | out_of_time | error | stopped
    pub outcome: Option<String>,
    pub answer: Option<String>,
    pub steps: Vec<Json>,
    pub max_steps: i64,
    pub error: Option<String>,
    pub cost_usd: Option<f64>,
    pub created: i64,
    pub ended: Option<i64>,
}

impl Run {
    fn of(r: &[Value]) -> Run {
        let s = |i: usize| r.get(i).and_then(get_str);
        Run {
            id: s(0).unwrap_or_default(),
            agent: s(1).unwrap_or_default(),
            repo: s(2).unwrap_or_default(),
            persona: s(3).and_then(|p| serde_json::from_str(&p).ok()).unwrap_or(Json::Null),
            task: s(4).unwrap_or_default(),
            model: s(5).unwrap_or_default(),
            adapter: s(6),
            by: s(7).unwrap_or_else(|| "you".into()),
            state: s(8).unwrap_or_default(),
            outcome: s(9),
            answer: s(10),
            steps: s(11).and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default(),
            max_steps: r.get(13).and_then(get_int).unwrap_or(25),
            error: s(14),
            cost_usd: match r.get(15) {
                Some(Value::Float(f)) => Some(*f),
                Some(Value::Int(i)) => Some(*i as f64),
                _ => None,
            },
            created: r.get(16).and_then(get_ms).unwrap_or(0),
            ended: r.get(17).and_then(get_ms),
        }
    }

    fn put(&self, sup: &Supervisor) -> Result<(), String> {
        let p = Params::new()
            .set("id", Value::Str(self.id.clone()))
            .set("agent", Value::Str(self.agent.clone()))
            .set("repo", Value::Str(self.repo.clone()))
            .set("persona", Value::Str(self.persona.to_string()))
            .set("task", Value::Str(self.task.clone()))
            .set("model", Value::Str(self.model.clone()))
            .set("adapter", opt_str(&self.adapter))
            .set("started_by", Value::Str(self.by.clone()))
            .set("state", Value::Str(self.state.clone()))
            .set("outcome", opt_str(&self.outcome))
            .set("answer", opt_str(&self.answer))
            .set("steps", Value::Str(Json::Array(self.steps.clone()).to_string()))
            .set("step_count", Value::Int(self.steps.len() as i64))
            .set("max_steps", Value::Int(self.max_steps))
            .set("error", opt_str(&self.error))
            .set("cost_usd", self.cost_usd.map_or(Value::Null, Value::Float))
            .set("created", ts(self.created))
            .set("ended", self.ended.map_or(Value::Null, ts));
        let names: Vec<String> = RUN_COLS.split(", ").map(|c| format!(":{c}")).collect();
        sup.db().query(&format!("INSERT OR REPLACE INTO swarm_runs ({RUN_COLS}) VALUES ({})", names.join(", ")), &p).map(|_| ())
    }

    fn live(&self) -> bool {
        matches!(self.state.as_str(), "starting" | "running")
    }

    pub fn to_json(&self, sup: &Supervisor, with_steps: bool) -> Json {
        let findings = count_findings(sup, &self.id);
        let mut v = json!({
            "id": self.id, "agent": self.agent, "agent_name": sup.agent_row(&self.agent).map(|a| a.name), "repo": self.repo,
            "persona": self.persona, "task": self.task, "model": self.model, "adapter": self.adapter, "by": self.by,
            "state": self.state, "outcome": self.outcome, "answer": self.answer, "step_count": self.steps.len(), "max_steps": self.max_steps,
            "findings_count": findings, "error": self.error, "cost_usd": self.cost_usd,
            "created": iso(self.created), "created_ms": self.created, "ended": self.ended.map(iso), "ended_ms": self.ended,
        });
        if with_steps {
            v["steps"] = Json::Array(self.steps.clone());
        }
        v
    }
}

#[derive(Clone, Debug, Default)]
pub struct Finding {
    pub id: String,
    pub run: String,
    pub agent: String,
    pub repo: String,
    pub kind: String,
    pub severity: i64,
    pub title: String,
    pub detail: String,
    pub step: Option<i64>,
    pub url: Option<String>,
    /// open | sent | asked | dismissed
    pub state: String,
    pub question: Option<String>,
    pub steer: Option<String>,
    pub created: i64,
    pub updated: i64,
}

impl Finding {
    fn of(r: &[Value]) -> Finding {
        let s = |i: usize| r.get(i).and_then(get_str);
        Finding {
            id: s(0).unwrap_or_default(),
            run: s(1).unwrap_or_default(),
            agent: s(2).unwrap_or_default(),
            repo: s(3).unwrap_or_default(),
            kind: s(4).unwrap_or_default(),
            severity: r.get(5).and_then(get_int).unwrap_or(1),
            title: s(6).unwrap_or_default(),
            detail: s(7).unwrap_or_default(),
            step: r.get(8).and_then(get_int),
            url: s(9),
            state: s(10).unwrap_or_default(),
            question: s(11),
            steer: s(12),
            created: r.get(13).and_then(get_ms).unwrap_or(0),
            updated: r.get(14).and_then(get_ms).unwrap_or(0),
        }
    }

    fn put(&self, sup: &Supervisor) -> Result<(), String> {
        let p = Params::new()
            .set("id", Value::Str(self.id.clone()))
            .set("run", Value::Str(self.run.clone()))
            .set("agent", Value::Str(self.agent.clone()))
            .set("repo", Value::Str(self.repo.clone()))
            .set("kind", Value::Str(self.kind.clone()))
            .set("severity", Value::Int(self.severity))
            .set("title", Value::Str(self.title.clone()))
            .set("detail", Value::Str(self.detail.clone()))
            .set("step", self.step.map_or(Value::Null, Value::Int))
            .set("url", opt_str(&self.url))
            .set("state", Value::Str(self.state.clone()))
            .set("question", opt_str(&self.question))
            .set("steer", opt_str(&self.steer))
            .set("created", ts(self.created))
            .set("updated", ts(self.updated));
        let names: Vec<String> = F_COLS.split(", ").map(|c| format!(":{c}")).collect();
        sup.db().query(&format!("INSERT OR REPLACE INTO swarm_findings ({F_COLS}) VALUES ({})", names.join(", ")), &p).map(|_| ())
    }

    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id, "run": self.run, "agent": self.agent, "repo": self.repo, "kind": self.kind, "severity": self.severity,
            "title": self.title, "detail": self.detail, "step": self.step, "url": self.url, "state": self.state,
            "question": self.question, "steer": self.steer, "created_ms": self.created, "updated_ms": self.updated,
        })
    }
}

fn get_run(sup: &Supervisor, id: &str) -> Result<Run, ApiError> {
    let rows = sup.db().query(&format!("SELECT {RUN_COLS} FROM swarm_runs WHERE id = :id LIMIT 1"), &Params::new().set("id", Value::Str(id.to_string()))).map_err(ApiError::internal)?;
    rows.first().map(|r| Run::of(r)).ok_or_else(|| ApiError::not_found(format!("no persona run {id}")))
}

fn runs_of(sup: &Supervisor, agent: Option<&str>, limit: usize) -> Vec<Run> {
    let (w, p) = match agent {
        Some(a) => (" WHERE agent = :agent", Params::new().set("agent", Value::Str(a.to_string()))),
        None => ("", Params::new()),
    };
    let rows = sup.db().query(&format!("SELECT {RUN_COLS} FROM swarm_runs{w} ORDER BY created DESC LIMIT {limit}"), &p).unwrap_or_default();
    rows.iter().map(|r| Run::of(r)).collect()
}

fn findings_of(sup: &Supervisor, run: &str) -> Vec<Finding> {
    let rows = sup.db().query(&format!("SELECT {F_COLS} FROM swarm_findings WHERE run = :run ORDER BY created ASC LIMIT 1000"), &Params::new().set("run", Value::Str(run.to_string()))).unwrap_or_default();
    rows.iter().map(|r| Finding::of(r)).collect()
}

fn count_findings(sup: &Supervisor, run: &str) -> usize {
    findings_of(sup, run).iter().filter(|f| f.state != "dismissed").count()
}

fn get_finding(sup: &Supervisor, id: &str) -> Result<Finding, ApiError> {
    let rows = sup.db().query(&format!("SELECT {F_COLS} FROM swarm_findings WHERE id = :id LIMIT 1"), &Params::new().set("id", Value::Str(id.to_string()))).map_err(ApiError::internal)?;
    rows.first().map(|r| Finding::of(r)).ok_or_else(|| ApiError::not_found(format!("no finding {id}")))
}

// ------------------------------------------------------------------ live runs

/// The runs playing now: their process, to stop them.
struct Live {
    agent: String,
    pid: Option<u32>,
    stopped: bool,
}

static LIVE: Mutex<Option<HashMap<String, Live>>> = Mutex::new(None);
/// A run's row is read, changed and written by its thread and by stop.
static RUN_WRITE: Mutex<()> = Mutex::new(());

fn live_map<T>(f: impl FnOnce(&mut HashMap<String, Live>) -> T) -> T {
    let mut g = LIVE.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

/// Runs a server crashed in the middle of are marked failed.
fn settle_orphans(sup: &Supervisor) {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| {
        let rows = sup.db().query(&format!("SELECT {RUN_COLS} FROM swarm_runs WHERE state = 'running' OR state = 'starting' LIMIT 1000"), &Params::new()).unwrap_or_default();
        for r in rows {
            let mut run = Run::of(&r);
            run.state = "failed".into();
            run.outcome = Some("error".into());
            run.error = Some("Workbench stopped while it played".into());
            run.ended = Some(now_ms());
            let _ = run.put(sup);
        }
    });
}

/// What `POST /api/swarm/runs` takes.
#[derive(Debug, Default, Deserialize)]
pub struct StartBody {
    pub agent: Option<String>,
    /// A persona's id, or (with `persona_text`) none.
    pub persona: Option<String>,
    /// A person described in words, instead of a persona's id.
    pub persona_text: Option<String>,
    pub task: Option<String>,
    pub model: Option<String>,
    pub max_steps: Option<i64>,
}

/// Starts a persona run on an agent's preview. `by` is the agent that
/// asked (`test_with_persona`), or `you`.
pub fn start(sup: &Arc<Supervisor>, lv: &Arc<Levels>, b: StartBody, by: &str) -> Result<(StatusCode, Json), ApiError> {
    settle_orphans(sup);
    let agent = b.agent.filter(|a| !a.is_empty()).ok_or_else(|| ApiError::bad("agent is required: the agent whose preview to test"))?;
    let a = sup.agent_row(&agent).ok_or_else(|| ApiError::not_found(format!("no agent {agent}")))?;
    if a.preview_url.is_none() {
        return Err(ApiError::conflict(format!("{} has no preview to test: it sets one with set_preview, or PUT /api/agents/{}/preview", a.name, a.id)));
    }
    let task = b.task.as_deref().map(str::trim).unwrap_or("").to_string();
    if task.is_empty() || task.len() > 2000 {
        return Err(ApiError::bad("task is required: what the persona tries to do, at most 2000 bytes"));
    }
    let persona = match (b.persona.as_deref().filter(|p| !p.is_empty()), b.persona_text.as_deref().map(str::trim).filter(|t| !t.is_empty())) {
        (Some(id), _) => personas().as_array().unwrap().iter().find(|p| p["id"] == id).cloned().ok_or_else(|| ApiError::bad(format!("no persona {id}; persona_text describes one in words")))?,
        (None, Some(text)) => {
            if text.len() > 500 {
                return Err(ApiError::bad("persona_text is at most 500 bytes"));
            }
            json!({ "id": "custom", "name": "Someone", "role": text, "traits": [], "device": if text.to_lowercase().contains("phone") { "phone" } else { "desktop" } })
        }
        (None, None) => return Err(ApiError::bad("persona (an id from /api/swarm/personas) or persona_text is required")),
    };
    let model = b.model.filter(|m| !m.is_empty()).unwrap_or_else(|| "haiku".into());
    if model.len() > 64 || !model.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')) {
        return Err(ApiError::bad("model is a model's name, such as haiku or sonnet"));
    }
    let avail = availability(&sup.cfg.claude_bin);
    if avail["available"] != true {
        return Ok((StatusCode::OK, json!({ "available": false, "why": avail["why"], "run": null })));
    }
    let runner = runner().map_err(|e| ApiError::new(409, "unavailable", e))?;
    let run = {
        let _w = RUN_WRITE.lock().unwrap();
        let live: Vec<String> = live_map(|m| m.values().map(|l| l.agent.clone()).collect());
        if live.contains(&a.id) {
            return Err(ApiError::conflict(format!("a persona test of {}'s preview is playing; stop it first", a.name)));
        }
        if live.len() >= MAX_LIVE {
            return Err(ApiError::new(429, "limit_exceeded", format!("{MAX_LIVE} persona tests are playing; wait for one to end")));
        }
        let run = Run {
            id: new_id("p"),
            agent: a.id.clone(),
            repo: a.repo.clone(),
            persona,
            task,
            model,
            adapter: avail["adapter"].as_str().map(str::to_string),
            by: by.to_string(),
            state: "starting".into(),
            max_steps: b.max_steps.unwrap_or(25).clamp(3, 60),
            created: now_ms(),
            ..Run::default()
        };
        run.put(sup).map_err(ApiError::internal)?;
        live_map(|m| m.insert(run.id.clone(), Live { agent: a.id.clone(), pid: None, stopped: false }));
        run
    };
    let (sup2, lv2, run2) = (sup.clone(), lv.clone(), run.clone());
    std::thread::spawn(move || play(sup2, lv2, runner, run2));
    Ok((StatusCode::CREATED, json!({ "run": run.to_json(sup, true), "available": true })))
}

/// One line the runner printed, applied to the run.
fn apply(sup: &Supervisor, run: &mut Run, msg: &Json) {
    let s = |k: &str| msg.get(k).and_then(Json::as_str).map(str::to_string);
    match msg["type"].as_str().unwrap_or("") {
        "start" => {
            run.state = "running".into();
            if let Some(a) = s("adapter") {
                run.adapter = Some(a);
            }
        }
        "step" => {
            run.state = "running".into();
            if run.steps.len() < STEPS_MAX {
                let mut st = msg.clone();
                if let Some(o) = st.as_object_mut() {
                    o.remove("type");
                    o.entry("ts_ms").or_insert(json!(now_ms()));
                }
                run.steps.push(st);
            }
        }
        "finding" => {
            if findings_of(sup, &run.id).len() >= FINDINGS_MAX {
                return;
            }
            let title = one_line(s("title").as_deref().unwrap_or("").trim(), 200);
            if title.is_empty() {
                return;
            }
            let now = now_ms();
            let f = Finding {
                id: new_id("f"),
                run: run.id.clone(),
                agent: run.agent.clone(),
                repo: run.repo.clone(),
                kind: s("kind").filter(|k| k.len() <= 32).unwrap_or_else(|| "bug".into()),
                severity: msg["severity"].as_i64().unwrap_or(2).clamp(1, 3),
                title,
                detail: crate::supervisor::one_line(s("detail").as_deref().unwrap_or(""), 2000),
                step: msg["step"].as_i64(),
                url: s("url"),
                state: "open".into(),
                created: now,
                updated: now,
                ..Finding::default()
            };
            let _ = f.put(sup);
        }
        "done" => {
            run.outcome = s("outcome").or(Some("done".into()));
            run.answer = s("answer").filter(|a| !a.is_empty());
            run.cost_usd = msg["cost_usd"].as_f64();
            if run.state != "stopped" {
                run.state = if run.outcome.as_deref() == Some("error") { "failed".into() } else { "done".into() };
            }
            run.ended = Some(now_ms());
        }
        "error" => {
            run.error = s("message").map(|m| one_line(&m, 1000));
        }
        _ => {}
    }
}

/// Plays a run: the runner's process, its lines into the rows.
fn play(sup: Arc<Supervisor>, _lv: Arc<Levels>, runner: Runner, mut run: Run) {
    let workdir = sup.cfg.data_dir.join("swarm").join(&run.id);
    let _ = std::fs::create_dir_all(&workdir);
    let job = json!({
        "url": format!("{}/preview/{}", sup.cfg.url, run.agent),
        "token": sup.cfg.token,
        "persona": run.persona,
        "task": run.task,
        "model": run.model,
        "max_steps": run.max_steps,
        "max_seconds": RUN_SECONDS,
        "claude_bin": sup.cfg.claude_bin,
        "workdir": workdir,
    });
    let spawned = Command::new(&runner.program)
        .args(&runner.args)
        .current_dir(&workdir)
        .env_remove("CHLOEDB_WORKBENCH_TOKEN")
        .env_remove("CHLOEDB_WORKBENCH_AGENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            finish(&sup, &mut run, Some(format!("cannot start the runner {}: {e}", runner.program)));
            return;
        }
    };
    live_map(|m| {
        if let Some(l) = m.get_mut(&run.id) {
            l.pid = Some(child.id());
        }
    });
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(job.to_string().as_bytes());
    }
    let mut err = child.stderr.take().unwrap();
    let errs = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(RUN_SECONDS + 120);
    let out = BufReader::new(child.stdout.take().unwrap());
    for line in out.lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Json>(line.trim()) else { continue };
        let _w = RUN_WRITE.lock().unwrap();
        if let Ok(now) = get_run(&sup, &run.id) {
            // Stop may have changed it meanwhile.
            run.state = if now.state == "stopped" { now.state } else { run.state.clone() };
        }
        apply(&sup, &mut run, &msg);
        let _ = run.put(&sup);
        if Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
    }
    let status = child.wait().ok();
    let stderr = errs.join().unwrap_or_default();
    let why = if run.ended.is_some() {
        None
    } else {
        let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
        Some(match tail.last() {
            Some(l) => one_line(l, 500),
            None => format!("the runner ended ({}) before the run did", status.map(|s| s.to_string()).unwrap_or_else(|| "killed".into())),
        })
    };
    finish(&sup, &mut run, why);
}

fn finish(sup: &Supervisor, run: &mut Run, why: Option<String>) {
    let _w = RUN_WRITE.lock().unwrap();
    let stopped = live_map(|m| m.remove(&run.id)).is_some_and(|l| l.stopped);
    if stopped {
        run.state = "stopped".into();
        run.outcome = Some("stopped".into());
    } else if let Some(w) = why {
        run.state = "failed".into();
        run.outcome = Some("error".into());
        run.error = run.error.clone().or(Some(w));
    }
    run.ended = run.ended.or(Some(now_ms()));
    let _ = run.put(sup);
}

/// Stops a run: its process gets SIGTERM.
pub fn stop(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let run = get_run(sup, id)?;
    let pid = live_map(|m| {
        m.get_mut(id).map(|l| {
            l.stopped = true;
            l.pid
        })
    });
    match pid {
        Some(Some(pid)) => unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        },
        Some(None) => {}
        None if run.live() => {
            // Not this server's: a leftover row.
            let _w = RUN_WRITE.lock().unwrap();
            let mut r = run.clone();
            r.state = "stopped".into();
            r.outcome = Some("stopped".into());
            r.ended = Some(now_ms());
            r.put(sup).map_err(ApiError::internal)?;
        }
        None => {}
    }
    // Read and write under the lock: the runner may end meanwhile, and a
    // copy read before its end would write the end away.
    let r = {
        let _w = RUN_WRITE.lock().unwrap();
        let mut r = get_run(sup, id)?;
        if r.live() {
            r.state = "stopped".into();
            r.outcome = Some("stopped".into());
            r.put(sup).map_err(ApiError::internal)?;
        }
        r
    };
    Ok(json!({ "run": r.to_json(sup, true) }))
}

// --------------------------------------------------------------- findings

/// What a steer about a finding says.
fn finding_text(sup: &Supervisor, f: &Finding, extra: Option<&str>) -> String {
    let run = get_run(sup, &f.run).ok();
    let (who, task) = run.as_ref().map(|r| (format!("{}, {}", r.persona["name"].as_str().unwrap_or("someone"), r.persona["role"].as_str().unwrap_or("")), r.task.clone())).unwrap_or_default();
    let mut s = format!(
        "A simulated user ({who}) tested your app in Workbench's preview with the task \"{task}\" and ran into this: {}. {}{}",
        f.title.trim_end_matches('.'),
        f.detail,
        match (f.step, &f.url) {
            (Some(n), Some(u)) => format!(" (step {n}, at {u})"),
            (Some(n), None) => format!(" (step {n})"),
            (None, Some(u)) => format!(" (at {u})"),
            _ => String::new(),
        }
    );
    if let Some(x) = extra.map(str::trim).filter(|x| !x.is_empty()) {
        s.push_str(&format!("\nThe user adds: {x}"));
    }
    s.push_str("\nFix it, or say why it is not a problem.");
    s
}

#[derive(Debug, Default, Deserialize)]
pub struct FindingBody {
    pub text: Option<String>,
}

/// `POST /api/swarm/findings/:id/(send|ask|dismiss)`.
pub fn act(sup: &Arc<Supervisor>, lv: &Arc<Levels>, id: &str, verb: &str, b: FindingBody) -> Result<Json, ApiError> {
    let mut f = get_finding(sup, id)?;
    match verb {
        "send" => {
            let text = finding_text(sup, &f, b.text.as_deref());
            let r = crate::levels::steer(sup, lv, &f.agent, &text)?;
            f.state = "sent".into();
            f.steer = r["steer"]["id"].as_str().map(str::to_string);
            f.updated = now_ms();
            f.put(sup).map_err(ApiError::internal)?;
            Ok(json!({ "finding": f.to_json(), "steer": r["steer"], "delivered": r["delivered"] }))
        }
        "ask" => {
            if f.state == "asked" {
                return Err(ApiError::conflict("this finding is already asked in Decide"));
            }
            let name = sup.agent_row(&f.agent).map(|a| a.name).unwrap_or_else(|| f.agent.clone());
            let run = get_run(sup, &f.run).ok();
            let q = Question {
                id: new_id("q"),
                agent: Some(f.agent.clone()),
                outcome: sup.agent_row(&f.agent).and_then(|a| a.outcome),
                repo: Some(f.repo.clone()),
                kind: "ask".into(),
                text: format!("A persona test of {name}'s preview found: {}. Send it to {name} to fix?", f.title.trim_end_matches('.')),
                context: Some(format!(
                    "{}{}{}",
                    f.detail,
                    run.as_ref().map(|r| format!("\nFound by {} ({}) doing: {}", r.persona["name"].as_str().unwrap_or("a persona"), r.persona["role"].as_str().unwrap_or(""), r.task)).unwrap_or_default(),
                    f.url.as_ref().map(|u| format!("\nWhere: {u}")).unwrap_or_default()
                )),
                options: json!([
                    { "label": format!("Send it to {name}"), "detail": "It gets the finding as a steer, with what you add.", "matches_precedent": false },
                    { "label": "Not a problem", "detail": "The finding is dismissed.", "matches_precedent": false }
                ]),
                recommended: Some(if f.severity >= 2 { 0 } else { 1 }),
                state: "open".into(),
                created: now_ms(),
                ..Question::default()
            };
            q.put(sup)?;
            lv.changed(&q.id);
            f.state = "asked".into();
            f.question = Some(q.id.clone());
            f.updated = now_ms();
            f.put(sup).map_err(ApiError::internal)?;
            watch_answer(sup.clone(), lv.clone(), f.id.clone(), q.id.clone());
            Ok(json!({ "finding": f.to_json(), "question": q.to_json(sup) }))
        }
        "dismiss" => {
            f.state = "dismissed".into();
            f.updated = now_ms();
            f.put(sup).map_err(ApiError::internal)?;
            Ok(json!({ "finding": f.to_json() }))
        }
        _ => Err(ApiError::bad("the verbs are send, ask and dismiss")),
    }
}

/// When the question made from a finding is answered: "Send it" (with what
/// you said) steers the agent, "Not a problem" dismisses the finding, any
/// other words go to the agent with the finding; expired, it is open again.
fn watch_answer(sup: Arc<Supervisor>, lv: Arc<Levels>, finding: String, question: String) {
    let mut rx = lv.questions.subscribe();
    std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(24 * 3600);
        loop {
            let q = match crate::questions::get(&sup, &question) {
                Ok(q) => q,
                Err(_) => return,
            };
            if q.state != "open" {
                let Ok(mut f) = get_finding(&sup, &finding) else { return };
                if f.state != "asked" {
                    return;
                }
                let answer = q.answer.clone().unwrap_or_default();
                let (first, rest) = answer.split_once('\n').unwrap_or((answer.as_str(), ""));
                f.updated = now_ms();
                if q.state != "answered" {
                    f.state = "open".into();
                } else if first == "Not a problem" {
                    f.state = "dismissed".into();
                } else {
                    let extra = if first.starts_with("Send it to ") { rest.to_string() } else { answer.clone() };
                    match crate::levels::steer(&sup, &lv, &f.agent, &finding_text(&sup, &f, Some(&extra))) {
                        Ok(r) => {
                            f.state = "sent".into();
                            f.steer = r["steer"]["id"].as_str().map(str::to_string);
                        }
                        Err(_) => f.state = "open".into(),
                    }
                }
                let _ = f.put(&sup);
                return;
            }
            if Instant::now() > end {
                return;
            }
            // Wake on any question's change, or every few seconds.
            let wait = Instant::now() + Duration::from_secs(5);
            while Instant::now() < wait {
                match rx.try_recv() {
                    Ok(id) if id == question => break,
                    Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(200)),
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return,
                }
            }
        }
    });
}

// ------------------------------------------------------------------ routes

async fn personas_route(State(app): State<App>) -> ApiResult {
    let sup = app.sup.clone();
    let avail = blocking(move || Ok(availability(&sup.cfg.claude_bin))).await?;
    Ok(AxJson(json!({
        "personas": personas(), "available": avail["available"], "why": avail["why"], "adapter": avail["adapter"],
        "models": ["haiku", "sonnet", "opus"], "default_model": "haiku",
    }))
    .into_response())
}

#[derive(Deserialize)]
struct RunsQ {
    agent: Option<String>,
    limit: Option<usize>,
}

async fn runs_route(State(app): State<App>, Query(q): Query<RunsQ>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || {
        settle_orphans(&sup);
        let runs: Vec<Json> = runs_of(&sup, q.agent.as_deref().filter(|a| !a.is_empty()), q.limit.unwrap_or(20).clamp(1, 200)).iter().map(|r| r.to_json(&sup, false)).collect();
        let avail = availability(&sup.cfg.claude_bin);
        Ok(json!({ "runs": runs, "available": avail["available"], "why": avail["why"] }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

async fn start_route(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let b: StartBody = parse_body(&body)?;
    let by = headers.get("x-workbench-agent").and_then(|h| h.to_str().ok()).filter(|s| !s.is_empty()).unwrap_or("you").to_string();
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    let (status, v) = blocking(move || start(&sup, &lv, b, &by)).await?;
    Ok((status, AxJson(v)).into_response())
}

async fn run_route(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || {
        let r = get_run(&sup, &id)?;
        let findings: Vec<Json> = findings_of(&sup, &id).iter().map(Finding::to_json).collect();
        Ok(json!({ "run": r.to_json(&sup, true), "findings": findings }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

async fn stop_route(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || stop(&sup, &id)).await?).into_response())
}

async fn finding_route(State(app): State<App>, Path((id, verb)): Path<(String, String)>, body: Bytes) -> ApiResult {
    let b: FindingBody = if body.is_empty() { FindingBody::default() } else { parse_body(&body)? };
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    Ok(AxJson(blocking(move || act(&sup, &lv, &id, &verb, b)).await?).into_response())
}

// ----------------------------------------------------------------- the MCP

/// The `test_with_persona` tool: starts a persona run on the caller's
/// preview and (unless `wait` is false) waits for it, answering its
/// outcome and findings.
pub fn mcp_tool(call: &dyn Fn(&str, &str, Option<Json>, u64) -> Result<Json, String>, me: &str, args: &Json) -> Result<String, String> {
    if me.is_empty() {
        return Err("test_with_persona is for an agent of Workbench: CHLOEDB_WORKBENCH_AGENT is not set".into());
    }
    let task = args.get("task").and_then(Json::as_str).unwrap_or("").trim();
    if task.is_empty() {
        return Err("task is required: what the persona tries to do in your app".into());
    }
    let persona = args.get("persona").and_then(Json::as_str).unwrap_or("first-timer").trim();
    let known = personas().as_array().unwrap().iter().any(|p| p["id"] == persona);
    let mut body = json!({ "agent": me, "task": task });
    if known {
        body["persona"] = json!(persona);
    } else {
        body["persona_text"] = json!(persona);
    }
    if let Some(m) = args.get("model").and_then(Json::as_str).filter(|m| !m.is_empty()) {
        body["model"] = json!(m);
    }
    let v = call("POST", "/api/swarm/runs", Some(body), 60)?;
    if v["available"] == false {
        return Err(format!("Persona tests are not available here: {}", v["why"].as_str().unwrap_or("the runner is missing")));
    }
    let id = v["run"]["id"].as_str().unwrap_or("").to_string();
    if args.get("wait").and_then(Json::as_bool) == Some(false) {
        return Ok(format!("Started persona test {id}. The user watches it in Workbench's Preview; read it later with wait true, or GET /api/swarm/runs/{id}."));
    }
    let timeout = args.get("timeout_s").and_then(Json::as_u64).unwrap_or(600).clamp(10, 1800);
    let end = Instant::now() + Duration::from_secs(timeout);
    let last = loop {
        let r = call("GET", &format!("/api/swarm/runs/{id}"), None, 30)?;
        let live = matches!(r["run"]["state"].as_str(), Some("starting" | "running"));
        if !live || Instant::now() >= end {
            break r;
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    let run = &last["run"];
    let s = |v: &Json, k: &str| v[k].as_str().unwrap_or("").to_string();
    let mut out = format!(
        "Persona test {} ({}, {}) of the task \"{}\": {} after {} steps.",
        id,
        run["persona"]["name"].as_str().unwrap_or("?"),
        run["persona"]["role"].as_str().unwrap_or(""),
        s(run, "task"),
        if matches!(run["state"].as_str(), Some("starting" | "running")) { format!("still playing after {timeout} s") } else { run["outcome"].as_str().unwrap_or(run["state"].as_str().unwrap_or("?")).replace('_', " ") },
        run["step_count"].as_i64().unwrap_or(0)
    );
    if let Some(a) = run["answer"].as_str() {
        out.push_str(&format!("\nTheir answer: {a}"));
    }
    if let Some(e) = run["error"].as_str() {
        out.push_str(&format!("\nError: {e}"));
    }
    let findings = last["findings"].as_array().cloned().unwrap_or_default();
    if findings.is_empty() {
        out.push_str("\nNo findings: nothing got in their way.");
    } else {
        out.push_str("\nFindings:");
        for f in &findings {
            let sev = match f["severity"].as_i64() {
                Some(3) => "blocker",
                Some(2) => "major",
                _ => "minor",
            };
            out.push_str(&format!("\n- [{sev}] {}: {} (step {}{}; {})", s(f, "title"), s(f, "detail"), f["step"], f["url"].as_str().map(|u| format!(", {u}")).unwrap_or_default(), s(f, "id")));
        }
    }
    out.push_str("\nThe user sees this run in Workbench's Preview. Runtime errors it caused are in runtime_errors.");
    Ok(out)
}
