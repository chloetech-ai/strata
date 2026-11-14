//! The levels (SPEC.md, "Levels (third build)"): what the server keeps for
//! them, their routes, and what it does after each hook: narration,
//! questions made from permission prompts, and what an agent is told
//! (rules, steers, its outcome) as the hook's `additionalContext`.
//!
//! The routes live here and in [`crate::goals`], [`crate::questions`],
//! [`crate::story`] and [`crate::digest`]; `server.rs` merges them.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json_};
use tokio::sync::broadcast;

use crate::db::{get_str, now_ms, ts, Agent};
use crate::server::App;
use crate::supervisor::{one_line, ApiError, Supervisor};
use crate::{digest, goals, narrate, questions, story};

/// The most of `additionalContext` a hook prints: Claude Code saves more
/// than 10,000 characters to a file and shows only a preview.
const CONTEXT_MAX: usize = 9_000;

/// What the levels keep in memory beside the database.
pub struct Levels {
    /// The id of each question that changed (asked, answered, expired):
    /// what `GET /api/questions/:id/wait` waits on.
    pub questions: broadcast::Sender<String>,
    /// Read-modify-writes of the levels' rows, one at a time.
    pub write: Mutex<()>,
    /// Agents whose narration is being made, with the next hook to make it
    /// from when another came meanwhile.
    narrating: Mutex<HashMap<String, Option<Json_>>>,
    /// Each agent's `PreToolUse` hook while it is the last hook it sent:
    /// what a permission question asks about.
    pub last_pre: Mutex<HashMap<String, Json_>>,
    /// Each agent's open permission question.
    pub open_perm: Mutex<HashMap<String, String>>,
    /// Agents Workbench types an answer into now (src/questions.rs).
    pub typing: Mutex<HashSet<String>>,
}

impl Levels {
    /// The levels' state; questions a previous server left open expire
    /// (their agents ended with it).
    /// Starts the screen watcher (src/questions.rs).
    pub fn new(sup: &Arc<Supervisor>) -> Arc<Levels> {
        let (questions, _) = broadcast::channel(1024);
        let lv = Arc::new(Levels { questions, write: Mutex::new(()), narrating: Mutex::new(HashMap::new()), last_pre: Mutex::new(HashMap::new()), open_perm: Mutex::new(HashMap::new()), typing: Mutex::new(HashSet::new()) });
        questions::migrate(sup);
        questions::expire_stale(sup, &lv);
        questions::watch_screens(sup.clone(), lv.clone());
        lv
    }

    pub fn changed(&self, question: &str) {
        let _ = self.questions.send(question.to_string());
    }
}

/// A new id: `prefix` and 8 hex digits.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", crate::supervisor::hex(8))
}

pub fn parse_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::bad(format!("the body is not the JSON this route takes: {e}")))
}

pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError::internal(e.to_string()))?
}

pub type ApiResult = Result<Response, ApiError>;

fn json_ok(v: Json_) -> ApiResult {
    Ok(Json(v).into_response())
}

fn created(v: Json_) -> ApiResult {
    Ok((StatusCode::CREATED, Json(v)).into_response())
}

/// The agent a request comes from (the MCP server sends its caller).
fn caller(headers: &HeaderMap) -> Option<String> {
    headers.get("x-workbench-agent").and_then(|h| h.to_str().ok()).filter(|s| !s.is_empty()).map(str::to_string)
}

/// The levels' routes, merged into the server's router (under its guard).
pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/goals", get(goals_list).post(goals_create))
        .route("/api/goals/:id", get(goal_get).patch(goal_patch))
        .route("/api/outcomes", get(outcomes_list).post(outcomes_create))
        .route("/api/outcomes/:id", axum::routing::patch(outcome_patch))
        .route("/api/questions", get(questions_list).post(questions_create))
        .route("/api/questions/:id", get(question_get))
        .route("/api/questions/:id/wait", get(question_wait))
        .route("/api/questions/:id/answer", post(question_answer))
        .route("/api/rules", get(rules_list).post(rules_create))
        .route("/api/rules/:id", axum::routing::patch(rule_patch).delete(rule_delete))
        .route("/api/agents/:id/steer", post(steer_route))
        .route("/api/agents/:id/story", get(story_route))
        .route("/api/seen", post(seen_route))
        .route("/api/digest", get(digest_route))
}

// Goals and outcomes.

#[derive(Deserialize)]
struct RepoQ {
    repo: Option<String>,
    goal: Option<String>,
    state: Option<String>,
    agent: Option<String>,
}

async fn goals_list(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    json_ok(blocking(move || goals::list(&sup, &lv, q.repo.as_deref())).await?)
}

async fn goals_create(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: goals::GoalBody = parse_body(&body)?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    let v = blocking(move || goals::create(&sup, &lv, b)).await?;
    // A draft (`plan: "draft"`) creates nothing.
    if v["draft"] == true {
        json_ok(v)
    } else {
        created(v)
    }
}

async fn goal_get(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    json_ok(blocking(move || goals::detail(&sup, &lv, &id)).await?)
}

async fn goal_patch(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    let b: goals::GoalPatch = parse_body(&body)?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    json_ok(blocking(move || goals::patch(&sup, &lv, &id, b)).await?)
}

async fn outcomes_list(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    let goal = q.goal.ok_or_else(|| ApiError::bad("goal is required"))?;
    json_ok(blocking(move || Ok(json!({ "outcomes": goals::refresh(&sup, &lv, Some(&goal)).iter().map(|o| o.to_json(&sup)).collect::<Vec<_>>() }))).await?)
}

async fn outcomes_create(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let b: goals::OutcomeBody = parse_body(&body)?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    let by = caller(&headers);
    created(blocking(move || goals::add_outcome(&sup, &lv, b, by.as_deref())).await?)
}

async fn outcome_patch(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    let b: goals::OutcomePatch = parse_body(&body)?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    json_ok(blocking(move || goals::patch_outcome(&sup, &lv, &id, b)).await?)
}

// Questions and rules.

async fn questions_list(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    json_ok(blocking(move || questions::list(&sup, &lv, q.repo.as_deref(), q.state.as_deref(), q.agent.as_deref())).await?)
}

async fn questions_create(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let b: questions::AskBody = parse_body(&body)?;
    let agent = caller(&headers).or_else(|| b.agent.clone()).ok_or_else(|| ApiError::bad("a question comes from an agent: the header x-workbench-agent (or agent) is required"))?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    created(blocking(move || questions::create_ask(&sup, &lv, &agent, b)).await?)
}

async fn question_get(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let sup = app.sup.clone();
    json_ok(blocking(move || Ok(json!({ "question": questions::get(&sup, &id)?.to_json(&sup) }))).await?)
}

#[derive(Deserialize)]
struct WaitQ {
    timeout_s: Option<f64>,
}

/// Waits until the question is no longer open, or the timeout.
async fn question_wait(State(app): State<App>, Path(id): Path<String>, Query(q): Query<WaitQ>) -> ApiResult {
    let timeout = Duration::from_secs_f64(q.timeout_s.unwrap_or(30.0).clamp(0.0, 600.0));
    let deadline = tokio::time::Instant::now() + timeout;
    let mut rx = app.lv.questions.subscribe();
    loop {
        let sup = app.sup.clone();
        let qid = id.clone();
        let q = blocking(move || questions::get(&sup, &qid)).await?;
        if q.state != "open" {
            return json_ok(json!({ "question": q.to_json(&app.sup), "answered": q.state == "answered" }));
        }
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => return json_ok(json!({ "question": q.to_json(&app.sup), "answered": false })),
                Ok(Ok(changed)) if changed != id => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => return json_ok(json!({ "question": q.to_json(&app.sup), "answered": false })),
                Ok(_) => break,
            }
        }
    }
}

async fn question_answer(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    let b: questions::AnswerBody = parse_body(&body)?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    json_ok(blocking(move || questions::answer(&sup, &lv, &id, b)).await?)
}

async fn rules_list(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    json_ok(blocking(move || Ok(json!({ "rules": questions::rules(&sup, q.repo.as_deref(), false).iter().map(|r| r.to_json()).collect::<Vec<_>>() }))).await?)
}

#[derive(Deserialize)]
struct RuleB {
    repo: Option<String>,
    text: Option<String>,
}

async fn rules_create(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: RuleB = parse_body(&body)?;
    let sup = app.sup.clone();
    created(blocking(move || {
        let repo = crate::trail::repo_dir(b.repo.as_deref())?;
        let text = b.text.unwrap_or_default();
        if text.trim().is_empty() {
            return Err(ApiError::bad("text is empty"));
        }
        let (r, missing) = questions::add_rule(&sup, &repo.to_string_lossy(), text.trim(), None);
        Ok(json!({ "rule": r.to_json(), "missing": missing }))
    })
    .await?)
}

async fn rule_patch(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    let b: questions::RulePatch = parse_body(&body)?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    json_ok(blocking(move || questions::patch_rule(&sup, &lv, &id, b)).await?)
}

/// `DELETE /api/rules/:id` (sixth build, the Rules panel): the rule stops
/// reaching agents. ChloeCtx keeps its copy as a past decision.
async fn rule_delete(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    json_ok(blocking(move || {
        let _w = lv.write.lock().unwrap();
        let p = Params::new().set("id", Value::Str(id.clone()));
        let had = sup.db().query("SELECT ctx_id FROM rules WHERE id = :id LIMIT 1", &p).map_err(ApiError::internal)?;
        let Some(row) = had.first() else { return Err(ApiError::not_found(format!("no rule {id}"))) };
        sup.db().query("DELETE FROM rules WHERE id = :id", &p).map_err(ApiError::internal)?;
        Ok(json!({ "ok": true, "id": id, "ctx_id": get_str(&row[0]) }))
    })
    .await?)
}

// Steer, story, away.

#[derive(Deserialize)]
struct SteerB {
    text: Option<String>,
}

async fn steer_route(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    let b: SteerB = parse_body(&body)?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    json_ok(blocking(move || steer(&sup, &lv, &id, b.text.as_deref().unwrap_or(""))).await?)
}

async fn story_route(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let sup = app.sup.clone();
    json_ok(blocking(move || story::story(&sup, &id)).await?)
}

#[derive(Deserialize, Default)]
struct SeenB {
    ts_ms: Option<i64>,
}

async fn seen_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: SeenB = if body.is_empty() { SeenB::default() } else { parse_body(&body)? };
    let sup = app.sup.clone();
    json_ok(blocking(move || digest::seen(&sup, b.ts_ms)).await?)
}

#[derive(Deserialize)]
struct DigestQ {
    since: Option<i64>,
    repo: Option<String>,
}

async fn digest_route(State(app): State<App>, Query(q): Query<DigestQ>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    json_ok(blocking(move || digest::digest(&sup, &lv, q.since, q.repo.as_deref())).await?)
}

// After each hook.

/// What the server does after a hook's event was recorded; answers the
/// hook's output (`{"hookSpecificOutput": ...}`) for `SessionStart`,
/// `UserPromptSubmit` and `PostToolUse` when there is something to tell.
/// Never fails: what goes wrong is left out.
pub fn after_hook(sup: &Arc<Supervisor>, lv: &Arc<Levels>, id: &str, hook: &Json_) -> Option<Json_> {
    let kind = hook.get("hook_event_name").and_then(Json_::as_str).unwrap_or("");
    let tool = hook.get("tool_name").and_then(Json_::as_str).unwrap_or("");
    match kind {
        "PreToolUse" => {
            lv.last_pre.lock().unwrap().insert(id.to_string(), hook.clone());
        }
        "Notification" => {
            if questions::is_permission(hook) {
                let pre = lv.last_pre.lock().unwrap().get(id).cloned();
                questions::create_permission(sup, lv, id, hook, pre);
            }
        }
        _ => {
            lv.last_pre.lock().unwrap().remove(id);
        }
    }
    if kind != "Notification" {
        // A permission prompt answered in the terminal: anything else the
        // session does next.
        questions::answered_in_terminal(sup, lv, id);
    }
    if kind == "PostToolUse" && tool == "mcp__workbench__ask_human" {
        questions::expire_asks_of(sup, lv, id);
    }
    if matches!(kind, "PostToolUse" | "Stop" | "UserPromptSubmit") {
        narrate_later(sup, lv, id, hook.clone());
    }
    if kind == "Stop" {
        type_pending_steers(sup, id);
    }
    if matches!(kind, "SessionStart" | "UserPromptSubmit" | "PostToolUse") {
        let source = hook.get("source").and_then(Json_::as_str).unwrap_or("");
        return deliver(sup, id, kind, source);
    }
    None
}

// Narration.

/// Makes the agent's sentence from this hook off the request path; one at
/// a time per agent, the newest hook winning.
fn narrate_later(sup: &Arc<Supervisor>, lv: &Arc<Levels>, id: &str, hook: Json_) {
    {
        let mut n = lv.narrating.lock().unwrap();
        if let Some(next) = n.get_mut(id) {
            *next = Some(hook);
            return;
        }
        n.insert(id.to_string(), None);
    }
    let (sup, lv, id) = (sup.clone(), lv.clone(), id.to_string());
    let spawned = std::thread::Builder::new().name(format!("narrate-{id}")).spawn({
        let id = id.clone();
        let lv = lv.clone();
        move || {
            let mut hook = hook;
            loop {
                narrate_now(&sup, &id, &hook);
                let mut n = lv.narrating.lock().unwrap();
                match n.get_mut(&id).and_then(Option::take) {
                    Some(next) => hook = next,
                    None => {
                        n.remove(&id);
                        return;
                    }
                }
            }
        }
    });
    if spawned.is_err() {
        lv.narrating.lock().unwrap().remove(&id);
    }
}

/// The sentence a hook implies.
pub fn sentence_of(hook: &Json_, cwd: &str) -> Option<String> {
    let s = |k: &str| hook.get(k).and_then(Json_::as_str).unwrap_or("");
    match s("hook_event_name") {
        "UserPromptSubmit" => {
            let p = narrate::sentences(s("prompt"), 1, narrate::MAX_CHARS - 13);
            (!p.is_empty()).then(|| format!("Starting on: {p}"))
        }
        "Stop" if !s("last_assistant_message").trim().is_empty() => Some(narrate::sentences(s("last_assistant_message"), 1, narrate::MAX_CHARS)),
        _ => {
            let path = s("transcript_path");
            let from_transcript = if narrate::transcript_ok(path) { narrate::narrate(&narrate::read_tail(std::path::Path::new(path), narrate::TAIL), cwd) } else { None };
            from_transcript.or_else(|| {
                let tool = s("tool_name");
                (!tool.is_empty()).then(|| narrate::tool_sentence(&[(tool.to_string(), hook.get("tool_input").cloned().unwrap_or(Json_::Null))], cwd))
            })
        }
    }
}

fn narrate_now(sup: &Supervisor, id: &str, hook: &Json_) {
    let Some(a) = sup.agent_row(id) else { return };
    // A waiting agent's sentence is its question.
    if a.state == "waiting" {
        return;
    }
    let Some(s) = sentence_of(hook, &a.cwd).filter(|s| !s.is_empty()) else { return };
    if a.narration.as_deref() == Some(s.as_str()) {
        return;
    }
    let _ = sup.update_agent(id, |a| {
        a.narration = Some(s);
        a.narration_ts = Some(now_ms());
    });
}

// Delivery: rules, steers and an agent's outcome, as additional context.

/// The items an agent has been told, by id.
fn delivered(sup: &Supervisor, agent: &str) -> HashSet<String> {
    let p = Params::new().set("agent", Value::Str(agent.to_string()));
    sup.db().query("SELECT item FROM deliveries WHERE agent = :agent LIMIT 100000", &p).unwrap_or_default().iter().filter_map(|r| get_str(&r[0])).collect()
}

fn record_delivery(sup: &Supervisor, agent: &str, item: &str, via: &str) {
    let p = Params::new()
        .set("id", Value::Str(format!("{agent}:{item}")))
        .set("agent", Value::Str(agent.to_string()))
        .set("item", Value::Str(item.to_string()))
        .set("via", Value::Str(via.to_string()))
        .set("ts", ts(now_ms()));
    let _ = sup.db().query("INSERT OR REPLACE INTO deliveries (id, agent, item, via, ts) VALUES (:id, :agent, :item, :via, :ts)", &p);
}

/// A steer: its id and text.
pub struct Steer {
    pub id: String,
    pub text: String,
}

/// An agent's steers, oldest first.
pub fn steers(sup: &Supervisor, agent: &str) -> Vec<Steer> {
    let p = Params::new().set("agent", Value::Str(agent.to_string()));
    let rows = sup.db().query("SELECT data FROM events WHERE agent = :agent AND kind = 'steer' ORDER BY seq ASC LIMIT 10000", &p).unwrap_or_default();
    rows.iter()
        .filter_map(|r| {
            let d: Json_ = serde_json::from_str(&get_str(&r[0])?).ok()?;
            Some(Steer { id: d["id"].as_str()?.to_string(), text: d["text"].as_str()?.to_string() })
        })
        .collect()
}

/// The words that hand an agent its outcome.
fn assignment(sup: &Supervisor, a: &Agent) -> Option<String> {
    let o = goals::outcome(sup, a.outcome.as_deref()?).ok()?;
    let goal = goals::goal(sup, &o.goal).ok();
    let mut s = format!("You work on outcome {} of the user's goal", o.id);
    if let Some(g) = goal {
        s.push_str(&format!(" \"{}\"", one_line(&g.text, 200)));
    }
    s.push_str(&format!(": {}.", o.title.trim_end_matches('.')));
    if let Some(d) = o.detail.as_deref().filter(|d| !d.trim().is_empty()) {
        s.push_str(&format!(" Done when: {}", one_line(d, 400)));
    }
    s.push_str(&format!(
        " Report progress with the workbench tool update_outcome (id \"{0}\", progress 0 to 100) and set state \"done\" when it is done. When only the user can decide something, ask with ask_human (a question and 2 to 4 options) instead of asking in your terminal, which the user is not watching.",
        o.id
    ));
    Some(s)
}

/// What to tell an agent at a hook: active rules of its repository and
/// steers it has not been told, and its outcome at the start; recorded as
/// told. `None` when there is nothing.
fn deliver(sup: &Supervisor, id: &str, event: &str, source: &str) -> Option<Json_> {
    let a = sup.agent_row(id)?;
    // A new context (after /clear or a compaction) has forgotten them.
    let fresh = event == "SessionStart" && matches!(source, "clear" | "compact");
    let told = if fresh { HashSet::new() } else { delivered(sup, id) };
    let mut parts: Vec<String> = Vec::new();
    let mut items: Vec<String> = Vec::new();
    if let Some(o) = a.outcome.as_deref() {
        let item = format!("assign:{o}");
        if !told.contains(&item) {
            if let Some(s) = assignment(sup, &a) {
                parts.push(s);
                items.push(item);
            }
        }
    }
    let rules: Vec<_> = questions::rules(sup, Some(&a.repo), true).into_iter().filter(|r| !told.contains(&r.id)).collect();
    if !rules.is_empty() {
        let mut s = String::from(if items.is_empty() && told.iter().any(|t| t.starts_with('r')) { "A new rule the user set for this repository (follow it):" } else { "Rules the user set for this repository (follow them):" });
        for r in &rules {
            s.push_str(&format!("\n- {}", one_line(&r.text, 600)));
            items.push(r.id.clone());
        }
        parts.push(s);
    }
    for st in steers(sup, id).into_iter().filter(|s| !told.contains(&s.id)) {
        parts.push(format!("The user adds: {}", st.text.trim()));
        items.push(st.id);
    }
    if parts.is_empty() {
        return None;
    }
    let mut text = format!("From Workbench, where the user watches this session:\n\n{}", parts.join("\n\n"));
    if text.len() > CONTEXT_MAX {
        let mut end = CONTEXT_MAX;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    for item in &items {
        record_delivery(sup, id, item, event);
    }
    Some(json!({ "hookSpecificOutput": { "hookEventName": event, "additionalContext": text } }))
}

/// `POST /api/agents/:id/steer`: an idle agent gets it typed with Enter; a
/// busy one gets it as additional context at its next hook that can carry
/// it (`PostToolUse`, `UserPromptSubmit`), or typed when its turn ends.
///
/// What its screen shows decides first (SPEC.md, "Sixth build: Safe
/// answering"): at its text prompt (idle, or interrupted while the hooks
/// still say working) it is typed in; at a menu nothing is typed, since a
/// key there chooses an option. `message` says what happened, for the
/// screen that sent it.
pub fn steer(sup: &Arc<Supervisor>, lv: &Levels, id: &str, text: &str) -> Result<Json_, ApiError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ApiError::bad("text is empty"));
    }
    if text.len() > 8000 {
        return Err(ApiError::bad("text is longer than 8000 bytes"));
    }
    let a = sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
    if !sup.is_running(id) {
        return Err(ApiError::conflict(format!("agent {} is not running", a.name)));
    }
    let sid = new_id("s");
    sup.note(id, "steer", format!("steer: {}", one_line(text, 200)), Some(json!({ "id": sid, "text": text }).to_string()), |_| {})?;
    let w = sup.waits(id);
    let (delivered, message) = match &w {
        crate::screen::Waits::Menu(m) => {
            let what = if m.question.is_empty() { "a choice".to_string() } else { format!("“{}”", one_line(&m.question, 120)) };
            ("queued", format!("Nothing was typed: {} shows a prompt ({what}), where a key would choose an option. Answer it first (Decide, or its terminal); your words reach {} at its next tool call, or as its next prompt when its turn ends.", a.name, a.name))
        }
        _ if a.state == "idle" || matches!(w, crate::screen::Waits::Input { .. }) => {
            let _typing = questions::Typing::start(lv, id);
            sup.input(id, text, true, None)?;
            record_delivery(sup, id, &sid, "typed");
            let after = if matches!(w, crate::screen::Waits::Input { interrupted: true }) { " It had been interrupted and was waiting for what to do instead." } else { "" };
            ("typed", format!("Typed into {}'s prompt as its next instruction.{after}", a.name))
        }
        _ => ("queued", format!("{} is working: it gets this at its next tool call, or as its next prompt when this turn ends.", a.name)),
    };
    Ok(json!({ "ok": true, "steer": { "id": sid, "text": text }, "delivered": delivered, "message": message }))
}

/// At the end of a turn (or when its screen shows it idle at its prompt),
/// steers that found no hook to ride are typed in.
pub(crate) fn type_pending_steers(sup: &Arc<Supervisor>, id: &str) {
    let told = delivered(sup, id);
    let pending: Vec<Steer> = steers(sup, id).into_iter().filter(|s| !told.contains(&s.id)).collect();
    if pending.is_empty() {
        return;
    }
    for s in &pending {
        record_delivery(sup, id, &s.id, "typed");
    }
    let text = pending.iter().map(|s| s.text.trim().to_string()).collect::<Vec<_>>().join(" ");
    let (sup, id) = (sup.clone(), id.to_string());
    std::thread::spawn(move || {
        // After the hook has answered and the prompt is back.
        std::thread::sleep(Duration::from_millis(300));
        if sup.agent_row(&id).is_some_and(|a| a.state == "idle") {
            let _ = sup.input(&id, &text, true, None);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_make_sentences() {
        let h = json!({ "hook_event_name": "UserPromptSubmit", "prompt": "Fix the **login** bug. Then the rest." });
        assert_eq!(sentence_of(&h, "/w").unwrap(), "Starting on: Fix the login bug.");
        let h = json!({ "hook_event_name": "Stop", "last_assistant_message": "All tests pass now. Details follow." });
        assert_eq!(sentence_of(&h, "/w").unwrap(), "All tests pass now.");
        let h = json!({ "hook_event_name": "PostToolUse", "tool_name": "Edit", "tool_input": { "file_path": "/w/src/a.rs" }, "transcript_path": "/nonexistent.jsonl" });
        assert_eq!(sentence_of(&h, "/w").unwrap(), "Editing src/a.rs");
    }
}
