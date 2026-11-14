//! Health's alerts (SPEC.md, "Fourth build", row 4). A rule is a row of
//! `alert_rules`: one SELECT over the state database whose rows each name
//! an `entity` and a value `v`, a comparison with a threshold, a window
//! (`:since` is now minus it), a severity and an action. Every 30 s each
//! rule runs; a row past its threshold is a firing, keyed by rule, entity
//! and agent, and a firing not seen in two runs in a row is resolved.
//! Firings in one repository that share an agent or a file are one
//! incident, which has a board: the firings, what its agents say they do,
//! their events around it, the tests in it, the error, and what ChloeCtx
//! knows of its files. A rule's action runs once per incident: `notify`
//! (an item in Needs you), `ask` (a question in Decide, answered by
//! pausing the agent, steering it, or letting it carry on), `pause` (Esc
//! to the agent's terminal).
//!
//! The tables the rules read beside the agents' own: `test_runs`
//! ([`crate::testruns`]), `file_edits` (each Edit and Write, its path
//! relative to the agent's checkout) and `token_use` (each assistant
//! message's usage, read from the agent's transcript).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, patch, post};
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
use crate::trail::{run_chloectx, Ran};

/// How often the rules run, in seconds.
pub const EVERY_S: u64 = 30;
/// Runs in a row without its row that resolve a firing.
const MISSES: i64 = 2;
/// How long after its last change an open incident still takes firings.
const GROUP_MS: i64 = 10 * 60_000;
/// The tables a rule may read (what the editor lists).
pub const TABLES: [&str; 7] = ["events", "agents", "test_runs", "file_edits", "token_use", "questions", "outcomes"];
/// The most of a transcript read the first time: its end.
const TRANSCRIPT_TAIL: u64 = 16 << 20;

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/incidents", get(incidents_route))
        .route("/api/incidents/:id", get(board_route))
        .route("/api/incidents/:id/resolve", post(resolve_route))
        .route("/api/incidents/:id/act", post(act_route))
        .route("/api/alerts/rules", get(rules_route).post(create_rule_route))
        .route("/api/alerts/rules/:id", patch(patch_rule_route).delete(delete_rule_route))
        .route("/api/alerts/test", post(test_route))
        .route("/api/alerts/check", post(check_route))
}

// ------------------------------------------------------------------ state

/// What the alerts keep in memory: the transcripts whose tokens are read,
/// when the rules last ran, ChloeCtx's answers for boards.
#[derive(Default)]
struct Mem {
    /// agent -> (transcript, bytes read).
    transcripts: HashMap<String, (PathBuf, u64)>,
    last_eval_ms: Option<i64>,
    related: HashMap<String, (Instant, Json)>,
}

fn mem() -> &'static Mutex<Mem> {
    static M: OnceLock<Mutex<Mem>> = OnceLock::new();
    M.get_or_init(Mutex::default)
}

/// One evaluation at a time (the loop's and `POST /api/alerts/check`).
fn eval_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(Mutex::default)
}

/// The rules' loop and the listener for answers to the alerts' questions.
pub fn start(sup: Arc<Supervisor>, lv: Arc<Levels>) {
    seed(&sup);
    let (s, l) = (sup.clone(), lv.clone());
    let _ = std::thread::Builder::new().name("alerts".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_secs(EVERY_S));
        evaluate(&s, &l);
    });
    let mut rx = lv.questions.subscribe();
    let _ = std::thread::Builder::new().name("alerts-answers".into()).spawn(move || loop {
        match rx.blocking_recv() {
            Ok(qid) => on_question(&sup, &lv, &qid),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    });
}

// ------------------------------------------------------------------ rules

#[derive(Clone, Debug)]
pub struct Rule {
    pub id: String,
    pub repo: Option<String>,
    pub name: String,
    pub description: String,
    pub sql: String,
    pub cmp: String,
    pub threshold: f64,
    pub window_s: i64,
    pub severity: String,
    pub action: String,
    pub builtin: bool,
    pub enabled: bool,
    pub created: i64,
    pub updated: i64,
    pub last_eval: Option<i64>,
    pub last_ms: Option<f64>,
    pub last_rows: Option<i64>,
    pub last_error: Option<String>,
    pub firing: i64,
}

const RULE_COLS: &str = "id, repo, name, description, sql_text, cmp, threshold, window_s, severity, action, builtin, enabled, created, updated, last_eval, last_ms, last_rows, last_error, firing";

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Bool(b) => Some(*b as i64 as f64),
        Value::Str(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn bool_of(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Int(i) => Some(*i != 0),
        _ => None,
    }
}

impl Rule {
    fn of(r: &[Value]) -> Rule {
        Rule {
            id: get_str(&r[0]).unwrap_or_default(),
            repo: get_str(&r[1]).filter(|s| !s.is_empty()),
            name: get_str(&r[2]).unwrap_or_default(),
            description: get_str(&r[3]).unwrap_or_default(),
            sql: get_str(&r[4]).unwrap_or_default(),
            cmp: get_str(&r[5]).unwrap_or_else(|| ">=".into()),
            threshold: num(&r[6]).unwrap_or(1.0),
            window_s: get_int(&r[7]).unwrap_or(3600),
            severity: get_str(&r[8]).unwrap_or_else(|| "minor".into()),
            action: get_str(&r[9]).unwrap_or_else(|| "notify".into()),
            builtin: bool_of(&r[10]).unwrap_or(false),
            enabled: bool_of(&r[11]).unwrap_or(true),
            created: get_ms(&r[12]).unwrap_or(0),
            updated: get_ms(&r[13]).unwrap_or(0),
            last_eval: get_ms(&r[14]),
            last_ms: num(&r[15]),
            last_rows: get_int(&r[16]),
            last_error: get_str(&r[17]),
            firing: get_int(&r[18]).unwrap_or(0),
        }
    }

    fn put(&self, sup: &Supervisor) -> Result<(), ApiError> {
        let p = Params::new()
            .set("id", Value::Str(self.id.clone()))
            .set("repo", opt_str(&self.repo))
            .set("name", Value::Str(self.name.clone()))
            .set("description", Value::Str(self.description.clone()))
            .set("sql_text", Value::Str(self.sql.clone()))
            .set("cmp", Value::Str(self.cmp.clone()))
            .set("threshold", Value::Float(self.threshold))
            .set("window_s", Value::Int(self.window_s))
            .set("severity", Value::Str(self.severity.clone()))
            .set("action", Value::Str(self.action.clone()))
            .set("builtin", Value::Bool(self.builtin))
            .set("enabled", Value::Bool(self.enabled))
            .set("created", ts(self.created))
            .set("updated", ts(self.updated))
            .set("last_eval", self.last_eval.map_or(Value::Null, ts))
            .set("last_ms", self.last_ms.map_or(Value::Null, Value::Float))
            .set("last_rows", self.last_rows.map_or(Value::Null, Value::Int))
            .set("last_error", opt_str(&self.last_error))
            .set("firing", Value::Int(self.firing));
        let names: Vec<String> = RULE_COLS.split(", ").map(|c| format!(":{c}")).collect();
        sup.db().query(&format!("INSERT OR REPLACE INTO alert_rules ({RULE_COLS}) VALUES ({})", names.join(", ")), &p).map_err(ApiError::internal)?;
        Ok(())
    }

    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id, "repo": self.repo.clone().unwrap_or_default(), "name": self.name, "description": self.description, "sql": self.sql,
            "cmp": self.cmp, "threshold": self.threshold, "window_s": self.window_s, "severity": self.severity, "action": self.action,
            "builtin": self.builtin, "enabled": self.enabled, "created": iso(self.created), "updated": iso(self.updated),
            "last_eval": self.last_eval.map(iso), "last_eval_ms": self.last_eval, "last_ms": self.last_ms, "last_rows": self.last_rows,
            "last_error": self.last_error, "firing": self.firing,
        })
    }
}

/// The rules Workbench comes with: (id, name, description, sql, cmp,
/// threshold, window, severity, action).
const BUILTINS: [(&str, &str, &str, &str, &str, f64, i64, &str, &str); 4] = [
    (
        "test-refail",
        "A test re-run failing 5 times",
        "One agent runs a test again and again and it keeps failing: it is stuck, and a person may know why.",
        "SELECT test AS entity, agent, COUNT(*) AS v\nFROM test_runs\nWHERE outcome = 'failed' AND ts >= :since\nGROUP BY test, agent",
        ">=",
        5.0,
        3600,
        "major",
        "ask",
    ),
    (
        "tokens-hour",
        "Tokens per hour above a limit",
        "An agent's input and output tokens in the last hour, from its transcript, over a budget.",
        "SELECT agent AS entity, agent, SUM(input) + SUM(output) AS v\nFROM token_use\nWHERE ts >= :since\nGROUP BY agent",
        ">",
        400_000.0,
        3600,
        "minor",
        "notify",
    ),
    (
        "same-file",
        "Two agents editing one file",
        "Edits from two or more agents to the same path of one repository within the window: a merge conflict on its way.",
        "SELECT file AS entity, repo, COUNT(DISTINCT agent) AS v\nFROM file_edits\nWHERE ts >= :since\nGROUP BY repo, file",
        ">=",
        2.0,
        1800,
        "minor",
        "notify",
    ),
    (
        "idle-asked",
        "An agent idle while asked",
        "Its turn is over but a question it asked is still open after the window: nobody is answering, and it does nothing meanwhile.",
        "SELECT a.id AS entity, a.id AS agent, COUNT(*) AS v\nFROM agents a JOIN questions q ON q.agent = a.id\nWHERE q.state = 'open' AND a.state IN ('idle', 'waiting') AND q.created < :since\nGROUP BY a.id",
        ">=",
        1.0,
        600,
        "minor",
        "notify",
    ),
];

/// Writes the built-in rules that are not there (one changed by you stays
/// as you left it).
fn seed(sup: &Supervisor) {
    let have: HashSet<String> = all_rules(sup).into_iter().map(|r| r.id).collect();
    let now = now_ms();
    for (i, (id, name, description, sql, cmp, threshold, window_s, severity, action)) in BUILTINS.into_iter().enumerate() {
        if have.contains(id) {
            continue;
        }
        let r = Rule {
            id: id.into(),
            repo: None,
            name: name.into(),
            description: description.into(),
            sql: sql.into(),
            cmp: cmp.into(),
            threshold,
            window_s,
            severity: severity.into(),
            action: action.into(),
            builtin: true,
            enabled: true,
            created: now + i as i64,
            updated: now,
            last_eval: None,
            last_ms: None,
            last_rows: None,
            last_error: None,
            firing: 0,
        };
        let _ = r.put(sup);
    }
}

fn all_rules(sup: &Supervisor) -> Vec<Rule> {
    let mut v: Vec<Rule> = sup.db().query(&format!("SELECT {RULE_COLS} FROM alert_rules LIMIT 10000"), &Params::new()).unwrap_or_default().iter().map(|r| Rule::of(r)).collect();
    v.sort_by(|a, b| b.builtin.cmp(&a.builtin).then(a.created.cmp(&b.created)).then(a.id.cmp(&b.id)));
    v
}

fn rule(sup: &Supervisor, id: &str) -> Result<Rule, ApiError> {
    let p = Params::new().set("id", Value::Str(id.to_string()));
    let rows = sup.db().query(&format!("SELECT {RULE_COLS} FROM alert_rules WHERE id = :id"), &p).map_err(ApiError::internal)?;
    rows.first().map(|r| Rule::of(r)).ok_or_else(|| ApiError::not_found(format!("no rule {id}")))
}

const CMPS: [&str; 6] = [">", ">=", "<", "<=", "=", "!="];
const SEVERITIES: [&str; 4] = ["critical", "major", "minor", "info"];
const ACTIONS: [&str; 4] = ["notify", "ask", "pause", "none"];

fn passes(cmp: &str, v: f64, th: f64) -> bool {
    match cmp {
        ">" => v > th,
        ">=" => v >= th,
        "<" => v < th,
        "<=" => v <= th,
        "=" => (v - th).abs() < 1e-9,
        "!=" => (v - th).abs() >= 1e-9,
        _ => false,
    }
}

fn sev_rank(s: &str) -> usize {
    SEVERITIES.iter().position(|x| *x == s).map(|i| SEVERITIES.len() - i).unwrap_or(0)
}

/// A rule's SQL as it runs: one SELECT (or WITH … SELECT), no `;`.
fn checked_sql(sql: &str) -> Result<String, ApiError> {
    let s = sql.trim().trim_end_matches(';').trim();
    let head = s.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    if head != "select" && head != "with" {
        return Err(ApiError::bad("a rule is one SELECT (or WITH … SELECT)"));
    }
    if s.contains(';') {
        return Err(ApiError::bad("a rule is one statement: no ';' inside it"));
    }
    if s.len() > 20_000 {
        return Err(ApiError::bad("a rule's SQL is at most 20,000 bytes"));
    }
    Ok(s.to_string())
}

/// The parameters a rule's SQL names: `:since` (now minus the window),
/// `:now`, `:repo` (the rule's repository).
fn params_of(sql: &str, now: i64, window_s: i64, repo: Option<&str>) -> Params {
    let mut p = Params::new();
    if sql.contains(":since") {
        p = p.set("since", ts(now - window_s * 1000));
    }
    if sql.contains(":now") {
        p = p.set("now", ts(now));
    }
    if sql.contains(":repo") {
        p = p.set("repo", Value::Str(repo.unwrap_or("").to_string()));
    }
    p
}

/// A value of a result as JSON.
fn value_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::Str(s) => json!(s),
        Value::Bool(b) => json!(b),
        Value::Timestamp(t) => json!(iso(t.div_euclid(1000))),
        Value::List(l) => Json::Array(l.iter().map(value_json).collect()),
        Value::Vector(v) => json!(format!("[vector of {}]", v.len())),
    }
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Float(f) => Some(f.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Timestamp(t) => Some(iso(t.div_euclid(1000))),
        _ => get_str(v),
    }
    .filter(|s| !s.is_empty())
}

/// A row of a rule's result.
#[derive(Clone, Debug)]
struct Hit {
    entity: String,
    agent: Option<String>,
    file: Option<String>,
    repo: Option<String>,
    detail: Option<String>,
    v: f64,
}

/// What a rule's SQL gave: its columns, its rows, and its rows as hits.
struct Ran2 {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    hits: Vec<Hit>,
    ms: f64,
    note: Option<String>,
}

fn run_rule(sup: &Supervisor, sql: &str, now: i64, window_s: i64, repo: Option<&str>) -> Result<Ran2, ApiError> {
    let sql = checked_sql(sql)?;
    let t = Instant::now();
    let (columns, rows) = sup.db().query_cols(&sql, &params_of(&sql, now, window_s, repo)).map_err(|e| ApiError::bad(e.to_string()))?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let lower: Vec<String> = columns.iter().map(|c| c.to_ascii_lowercase()).collect();
    let at = |n: &str| lower.iter().position(|c| c == n);
    let (ie, iv) = (at("entity"), at("v").or_else(|| at("value")));
    let note = match (ie, iv) {
        (None, _) => Some("the result has no column entity: name what fires with … AS entity".to_string()),
        (_, None) => Some("the result has no column v: name the value compared with the threshold with … AS v".to_string()),
        _ => None,
    };
    let mut hits = Vec::new();
    if let (Some(ie), Some(iv)) = (ie, iv) {
        let (ia, ifl, ir, id) = (at("agent"), at("file"), at("repo"), at("detail"));
        for r in &rows {
            let Some(entity) = text_of(&r[ie]) else { continue };
            let Some(v) = num(&r[iv]) else { continue };
            let pick = |i: Option<usize>| i.and_then(|i| text_of(&r[i]));
            hits.push(Hit { entity, agent: pick(ia), file: pick(ifl), repo: pick(ir), detail: pick(id), v });
        }
    }
    Ok(Ran2 { columns, rows, hits, ms, note })
}

// ---------------------------------------------------------------- firings

#[derive(Clone, Debug)]
struct Firing {
    id: String,
    rule: String,
    repo: String,
    entity: String,
    agent: Option<String>,
    file: Option<String>,
    detail: Option<String>,
    value: f64,
    summary: String,
    /// `firing`, `silenced` (its incident was resolved by you while it
    /// still fires) or `resolved`.
    state: String,
    started: i64,
    last_seen: i64,
    ended: Option<i64>,
    incident: Option<String>,
    misses: i64,
    /// The agents it is about: its agent, or those who edited its file.
    agents: Vec<String>,
}

const F_COLS: &str = "id, rule, repo, entity, agent, file, detail, value, summary, state, started, last_seen, ended, incident, misses, agents";

fn list_of(v: &Value) -> Vec<String> {
    get_str(v).and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()).unwrap_or_default()
}

impl Firing {
    fn of(r: &[Value]) -> Firing {
        Firing {
            id: get_str(&r[0]).unwrap_or_default(),
            rule: get_str(&r[1]).unwrap_or_default(),
            repo: get_str(&r[2]).unwrap_or_default(),
            entity: get_str(&r[3]).unwrap_or_default(),
            agent: get_str(&r[4]),
            file: get_str(&r[5]),
            detail: get_str(&r[6]),
            value: num(&r[7]).unwrap_or(0.0),
            summary: get_str(&r[8]).unwrap_or_default(),
            state: get_str(&r[9]).unwrap_or_default(),
            started: get_ms(&r[10]).unwrap_or(0),
            last_seen: get_ms(&r[11]).unwrap_or(0),
            ended: get_ms(&r[12]),
            incident: get_str(&r[13]),
            misses: get_int(&r[14]).unwrap_or(0),
            agents: list_of(&r[15]),
        }
    }

    fn put(&self, sup: &Supervisor) {
        let p = Params::new()
            .set("id", Value::Str(self.id.clone()))
            .set("rule", Value::Str(self.rule.clone()))
            .set("repo", Value::Str(self.repo.clone()))
            .set("entity", Value::Str(self.entity.clone()))
            .set("agent", opt_str(&self.agent))
            .set("file", opt_str(&self.file))
            .set("detail", opt_str(&self.detail))
            .set("value", Value::Float(self.value))
            .set("summary", Value::Str(self.summary.clone()))
            .set("state", Value::Str(self.state.clone()))
            .set("started", ts(self.started))
            .set("last_seen", ts(self.last_seen))
            .set("ended", self.ended.map_or(Value::Null, ts))
            .set("incident", opt_str(&self.incident))
            .set("misses", Value::Int(self.misses))
            .set("agents", Value::Str(json!(self.agents).to_string()));
        let names: Vec<String> = F_COLS.split(", ").map(|c| format!(":{c}")).collect();
        let _ = sup.db().query(&format!("INSERT OR REPLACE INTO alert_firings ({F_COLS}) VALUES ({})", names.join(", ")), &p);
    }

    fn to_json(&self, rules: &HashMap<String, Rule>) -> Json {
        let r = rules.get(&self.rule);
        json!({
            "id": self.id, "rule": self.rule, "rule_name": r.map(|r| r.name.clone()).unwrap_or_else(|| self.rule.clone()),
            "severity": r.map(|r| r.severity.clone()), "cmp": r.map(|r| r.cmp.clone()), "threshold": r.map(|r| r.threshold),
            "state": self.state, "repo": self.repo, "entity": self.entity, "agent": self.agent, "agents": self.agents, "file": self.file, "detail": self.detail,
            "value": self.value, "summary": self.summary, "started_ms": self.started, "started": iso(self.started),
            "last_ms": self.last_seen, "ended_ms": self.ended, "incident": self.incident,
        })
    }
}

fn firings_where(sup: &Supervisor, cond: &str, p: &Params) -> Vec<Firing> {
    sup.db().query(&format!("SELECT {F_COLS} FROM alert_firings WHERE {cond} LIMIT 100000"), p).unwrap_or_default().iter().map(|r| Firing::of(r)).collect()
}

fn name_of(sup: &Supervisor, agent: &str) -> String {
    sup.agent_row(agent).map(|a| a.name).unwrap_or_else(|| agent.to_string())
}

/// 486120 as "486k", 1204000 as "1.2M".
fn short_num(v: f64) -> String {
    let a = v.abs();
    if a >= 1e6 {
        format!("{:.1}M", v / 1e6).replace(".0M", "M")
    } else if a >= 1e4 {
        format!("{:.0}k", v / 1e3)
    } else if a >= 1e3 {
        format!("{:.1}k", v / 1e3).replace(".0k", "k")
    } else if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

fn window_words(s: i64) -> String {
    match s {
        3600 => "the last hour".into(),
        s if s % 3600 == 0 => format!("the last {} hours", s / 3600),
        s if s >= 60 => format!("the last {} min", s / 60),
        s => format!("the last {s} s"),
    }
}

/// The agents who edited a file of a repository since `since`, with their
/// edits, most first.
fn editors(sup: &Supervisor, repo: &str, file: &str, since: i64) -> Vec<(String, i64)> {
    let p = Params::new().set("repo", Value::Str(repo.to_string())).set("file", Value::Str(file.to_string())).set("since", ts(since));
    let rows = sup.db().query("SELECT agent, COUNT(*) AS n FROM file_edits WHERE repo = :repo AND file = :file AND ts >= :since GROUP BY agent", &p).unwrap_or_default();
    let mut v: Vec<(String, i64)> = rows.iter().filter_map(|r| Some((get_str(&r[0])?, get_int(&r[1]).unwrap_or(0)))).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// A firing's sentence and the title of an incident it opens.
fn words(sup: &Supervisor, r: &Rule, h: &Hit, agents: &[String], now: i64) -> (String, String) {
    let who = h.agent.as_deref().map(|a| name_of(sup, a));
    let w = window_words(r.window_s);
    match r.id.as_str() {
        "test-refail" => {
            let who = who.unwrap_or_else(|| "an agent".into());
            let test = h.detail.clone().unwrap_or_else(|| h.entity.clone());
            (format!("{who}'s {test} failed {} times in {w}", short_num(h.v)), format!("{who} is stuck: {test} failed {} times", short_num(h.v)))
        }
        "tokens-hour" => {
            let who = who.unwrap_or_else(|| h.entity.clone());
            (format!("{who} used {} tokens in {w} (limit {})", short_num(h.v), short_num(r.threshold)), format!("{who}: {} tokens in {w}", short_num(h.v)))
        }
        "same-file" => {
            let file = h.file.clone().unwrap_or_else(|| h.entity.clone());
            let eds = editors(sup, h.repo.as_deref().unwrap_or(""), &file, now - r.window_s * 1000);
            let list: Vec<String> = eds.iter().map(|(a, n)| format!("{} ({n} edit{})", name_of(sup, a), if *n == 1 { "" } else { "s" })).collect();
            let names: Vec<String> = agents.iter().map(|a| name_of(sup, a)).collect();
            let both = match names.len() {
                0 | 1 => "agents".to_string(),
                2 => format!("{} and {} both", names[0], names[1]),
                _ => format!("{} and {} more", names[..2].join(", "), names.len() - 2),
            };
            (format!("{} agents edited {file} in {w}: {}", short_num(h.v), list.join(", ")), format!("{both} edit {file}"))
        }
        "idle-asked" => {
            let who = who.unwrap_or_else(|| h.entity.clone());
            let q = h.agent.as_deref().and_then(|a| open_question_text(sup, a));
            let s = match &q {
                Some(t) => format!("{who}'s turn is over and its question has waited: “{}”", one_line(t, 120)),
                None => format!("{who}'s turn is over and its question has waited past {w}"),
            };
            let t = match &q {
                Some(t) => format!("{who} idle while asked: “{}”", one_line(t, 80)),
                None => format!("{who} idle while asked"),
            };
            (s, t)
        }
        _ => {
            let what = h.detail.clone().unwrap_or_else(|| h.entity.clone());
            let lead = match &who {
                Some(n) if *n != what && h.agent.as_deref() != Some(what.as_str()) => format!("{n}: {what}"),
                Some(n) => n.clone(),
                None => what.clone(),
            };
            // A rule over the test runs is about a test: its title names it.
            let title = if reads_tests(r) { format!("{what}: {}", r.name) } else { format!("{}: {}", who.unwrap_or_else(|| h.entity.clone()), r.name) };
            (format!("{lead}: {} {} {} in {w}", short_num(h.v), r.cmp, short_num(r.threshold)), title)
        }
    }
}

/// Whether a rule reads the test runs (its rows name tests).
fn reads_tests(r: &Rule) -> bool {
    r.id == "test-refail" || r.sql.to_lowercase().contains("test_runs")
}

/// The tests a set of firings of test rules fired on, each once.
fn tests_of<'a>(firings: impl Iterator<Item = &'a Firing>, rules: &HashMap<String, Rule>) -> Vec<String> {
    let mut out = Vec::new();
    for f in firings.filter(|f| f.rule == "test-refail" || rules.get(&f.rule).is_some_and(reads_tests)) {
        add_unique(&mut out, f.detail.as_deref().unwrap_or(&f.entity));
    }
    out
}

fn open_question_text(sup: &Supervisor, agent: &str) -> Option<String> {
    let p = Params::new().set("agent", Value::Str(agent.to_string()));
    let rows = sup.db().query("SELECT text FROM questions WHERE agent = :agent AND state = 'open' ORDER BY created LIMIT 1", &p).ok()?;
    rows.first().and_then(|r| get_str(&r[0]))
}

// -------------------------------------------------------------- incidents

#[derive(Clone, Debug)]
struct Incident {
    id: String,
    repo: String,
    title: String,
    severity: String,
    state: String,
    agents: Vec<String>,
    files: Vec<String>,
    rules: Vec<String>,
    actions: Vec<Json>,
    needs_you: bool,
    started: i64,
    updated: i64,
    resolved: Option<i64>,
}

const I_COLS: &str = "id, repo, title, severity, state, agents, files, rules, actions, needs_you, started, updated, resolved";

impl Incident {
    fn of(r: &[Value]) -> Incident {
        Incident {
            id: get_str(&r[0]).unwrap_or_default(),
            repo: get_str(&r[1]).unwrap_or_default(),
            title: get_str(&r[2]).unwrap_or_default(),
            severity: get_str(&r[3]).unwrap_or_default(),
            state: get_str(&r[4]).unwrap_or_default(),
            agents: list_of(&r[5]),
            files: list_of(&r[6]),
            rules: list_of(&r[7]),
            actions: get_str(&r[8]).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
            needs_you: bool_of(&r[9]).unwrap_or(false),
            started: get_ms(&r[10]).unwrap_or(0),
            updated: get_ms(&r[11]).unwrap_or(0),
            resolved: get_ms(&r[12]),
        }
    }

    fn put(&self, sup: &Supervisor) {
        let p = Params::new()
            .set("id", Value::Str(self.id.clone()))
            .set("repo", Value::Str(self.repo.clone()))
            .set("title", Value::Str(self.title.clone()))
            .set("severity", Value::Str(self.severity.clone()))
            .set("state", Value::Str(self.state.clone()))
            .set("agents", Value::Str(json!(self.agents).to_string()))
            .set("files", Value::Str(json!(self.files).to_string()))
            .set("rules", Value::Str(json!(self.rules).to_string()))
            .set("actions", Value::Str(Json::Array(self.actions.clone()).to_string()))
            .set("needs_you", Value::Bool(self.needs_you))
            .set("started", ts(self.started))
            .set("updated", ts(self.updated))
            .set("resolved", self.resolved.map_or(Value::Null, ts));
        let names: Vec<String> = I_COLS.split(", ").map(|c| format!(":{c}")).collect();
        let _ = sup.db().query(&format!("INSERT OR REPLACE INTO incidents ({I_COLS}) VALUES ({})", names.join(", ")), &p);
    }

    fn to_json(&self, sup: &Supervisor, firings: &[Firing], rules: &HashMap<String, Rule>) -> Json {
        let agents: Vec<Json> = self.agents.iter().map(|a| sup.agent_json_of(a).unwrap_or_else(|| json!({ "id": a, "name": a }))).collect();
        let mut fs: Vec<&Firing> = firings.iter().filter(|f| f.incident.as_deref() == Some(self.id.as_str())).collect();
        fs.sort_by(|a, b| b.started.cmp(&a.started));
        let mut actions = self.actions.clone();
        actions.reverse();
        // Its tests, and where each passes and fails now: an incident that
        // an agent's branch fixed says so, and does not lead with that agent.
        let tests = tests_of(fs.iter().copied(), rules);
        let wheres: Vec<Json> = if self.state == "open" { tests.iter().take(3).filter_map(|t| crate::testruns::where_now(sup, &self.repo, t)).collect() } else { Vec::new() };
        let mut fixed_by: Vec<String> = Vec::new();
        for w in &wheres {
            for a in w["fixed_by"].as_array().into_iter().flatten().filter_map(Json::as_str) {
                add_unique(&mut fixed_by, a);
            }
        }
        let summary = wheres
            .iter()
            .map(|w| {
                let s = w["sentence"].as_str().unwrap_or("");
                if wheres.len() > 1 { format!("{}: {s}", w["test"].as_str().unwrap_or("")) } else { s.to_string() }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let summary = if summary.is_empty() { fs.iter().find(|f| f.state == "firing").or(fs.first()).map(|f| f.summary.clone()).unwrap_or_default() } else { summary };
        json!({
            "tests": tests, "where": wheres, "fixed_by": fixed_by, "summary": summary,
            "id": self.id, "repo": self.repo, "title": self.title, "severity": self.severity, "state": self.state,
            "started": iso(self.started), "started_ms": self.started, "updated": iso(self.updated), "updated_ms": self.updated,
            "resolved": self.resolved.map(iso), "resolved_ms": self.resolved,
            "agents": agents, "files": self.files, "rules": self.rules, "actions": actions, "needs_you": self.needs_you && self.state == "open",
            "firing_now": fs.iter().filter(|f| f.state == "firing").count(),
            "firings": fs.iter().map(|f| f.to_json(rules)).collect::<Vec<_>>(), "firings_n": fs.len(),
        })
    }
}

fn incidents_where(sup: &Supervisor, cond: &str, p: &Params) -> Vec<Incident> {
    sup.db().query(&format!("SELECT {I_COLS} FROM incidents WHERE {cond} LIMIT 100000"), p).unwrap_or_default().iter().map(|r| Incident::of(r)).collect()
}

fn incident(sup: &Supervisor, id: &str) -> Result<Incident, ApiError> {
    incidents_where(sup, "id = :id", &Params::new().set("id", Value::Str(id.to_string()))).into_iter().next().ok_or_else(|| ApiError::not_found(format!("no incident {id}")))
}

fn add_unique(v: &mut Vec<String>, x: &str) {
    if !x.is_empty() && !v.iter().any(|y| y == x) {
        v.push(x.to_string());
    }
}

/// The open incident a new firing belongs to (one of its repository that
/// still fires or changed in the last 10 min, sharing an agent or a file
/// with it, or of the same rule when it names neither), else a new one.
fn place(sup: &Supervisor, f: &Firing, r: &Rule, title: &str, now: i64) -> Incident {
    let p = Params::new().set("repo", Value::Str(f.repo.clone()));
    let mut open = incidents_where(sup, "state = 'open' AND repo = :repo", &p);
    open.sort_by(|a, b| b.updated.cmp(&a.updated));
    let firing: HashSet<String> = firings_where(sup, "state = 'firing' AND repo = :repo", &p).into_iter().filter_map(|f| f.incident).collect();
    let file = f.file.clone();
    let found = open.into_iter().filter(|i| now - i.updated <= GROUP_MS || firing.contains(&i.id)).find(|i| {
        f.agents.iter().any(|a| i.agents.contains(a)) || file.as_ref().is_some_and(|x| i.files.contains(x)) || (f.agents.is_empty() && file.is_none() && i.rules.contains(&f.rule))
    });
    let mut inc = found.unwrap_or_else(|| Incident {
        id: new_id("i"),
        repo: f.repo.clone(),
        title: title.to_string(),
        severity: r.severity.clone(),
        state: "open".into(),
        agents: Vec::new(),
        files: Vec::new(),
        rules: Vec::new(),
        actions: Vec::new(),
        needs_you: false,
        started: now,
        updated: now,
        resolved: None,
    });
    for a in &f.agents {
        add_unique(&mut inc.agents, a);
    }
    if let Some(x) = &file {
        add_unique(&mut inc.files, x);
    }
    add_unique(&mut inc.rules, &f.rule);
    if sev_rank(&r.severity) > sev_rank(&inc.severity) {
        inc.severity = r.severity.clone();
    }
    inc.updated = now;
    inc
}

// ------------------------------------------------------------- evaluation

/// Runs every enabled rule once: tokens are read from the transcripts
/// first, firings start, carry on or resolve, incidents open and close,
/// and new incidents' actions run. Answers how many firings started.
pub fn evaluate(sup: &Arc<Supervisor>, lv: &Arc<Levels>) -> usize {
    let _g = eval_lock().lock().unwrap_or_else(|e| e.into_inner());
    ingest_tokens(sup);
    let now = now_ms();
    let rules: Vec<Rule> = all_rules(sup);
    let by_id: HashMap<String, Rule> = rules.iter().map(|r| (r.id.clone(), r.clone())).collect();
    let mut started = 0;
    let mut acts: Vec<(String, String, String)> = Vec::new(); // (incident, rule, agent)
    for mut r in rules.into_iter() {
        let active = firings_where(sup, "rule = :rule AND (state = 'firing' OR state = 'silenced')", &Params::new().set("rule", Value::Str(r.id.clone())));
        if !r.enabled {
            // A rule switched off ends its firings.
            for mut f in active {
                f.state = "resolved".into();
                f.ended = Some(now);
                f.put(sup);
            }
            if r.firing != 0 {
                r.firing = 0;
                let _ = r.put(sup);
            }
            continue;
        }
        let ran = match run_rule(sup, &r.sql, now, r.window_s, r.repo.as_deref()) {
            Ok(x) => x,
            Err(e) => {
                r.last_error = Some(e.message);
                r.last_eval = Some(now);
                let _ = r.put(sup);
                continue;
            }
        };
        let mut seen: HashSet<String> = HashSet::new();
        let key = |entity: &str, agent: &Option<String>| format!("{entity}\u{0}{}", agent.as_deref().unwrap_or(""));
        let mut by_key: HashMap<String, Firing> = active.into_iter().map(|f| (key(&f.entity, &f.agent), f)).collect();
        for h in ran.hits.iter().filter(|h| passes(&r.cmp, h.v, r.threshold)) {
            let k = key(&h.entity, &h.agent);
            if !seen.insert(k.clone()) {
                continue;
            }
            let agent_row = h.agent.as_deref().and_then(|a| sup.agent_row(a));
            let repo = h.repo.clone().or_else(|| agent_row.as_ref().map(|a| a.repo.clone())).or_else(|| r.repo.clone()).unwrap_or_default();
            let mut agents: Vec<String> = h.agent.iter().cloned().collect();
            if agents.is_empty() {
                if let Some(file) = h.file.as_deref().or((r.id == "same-file").then_some(h.entity.as_str())) {
                    agents = editors(sup, &repo, file, now - r.window_s * 1000).into_iter().map(|(a, _)| a).collect();
                }
            }
            let file = h.file.clone().or_else(|| (r.id == "same-file").then(|| h.entity.clone()));
            let hit = Hit { file: file.clone(), repo: Some(repo.clone()), ..h.clone() };
            let (summary, title) = words(sup, &r, &hit, &agents, now);
            if let Some(f) = by_key.get_mut(&k) {
                f.value = h.v;
                f.summary = summary;
                f.last_seen = now;
                f.misses = 0;
                for a in agents {
                    add_unique(&mut f.agents, &a);
                }
                f.put(sup);
                // New agents on a shared file join its incident.
                if let Some(iid) = f.incident.clone() {
                    if let Ok(mut inc) = incident(sup, &iid) {
                        let before = inc.agents.len();
                        for a in &f.agents {
                            add_unique(&mut inc.agents, a);
                        }
                        if inc.agents.len() != before && inc.state == "open" {
                            inc.updated = now;
                            inc.put(sup);
                        }
                    }
                }
                continue;
            }
            let mut f = Firing {
                id: new_id("f"),
                rule: r.id.clone(),
                repo,
                entity: h.entity.clone(),
                agent: h.agent.clone(),
                file,
                detail: h.detail.clone(),
                value: h.v,
                summary: summary.clone(),
                state: "firing".into(),
                started: now,
                last_seen: now,
                ended: None,
                incident: None,
                misses: 0,
                agents,
            };
            let inc = place(sup, &f, &r, &title, now);
            f.incident = Some(inc.id.clone());
            f.put(sup);
            inc.put(sup);
            started += 1;
            let at = f.agent.clone().or_else(|| f.agents.first().cloned());
            if let Some(a) = &at {
                let data = json!({ "incident": inc.id, "rule": r.id, "firing": f.id, "value": f.value });
                let _ = sup.note(a, "alert", format!("▲ {}: {}", r.name, one_line(&summary, 200)), Some(data.to_string()), |_| {});
            }
            if r.action != "none" {
                acts.push((inc.id.clone(), r.id.clone(), at.unwrap_or_default()));
            }
        }
        // The firings it did not see this time.
        for (k, mut f) in by_key {
            if seen.contains(&k) {
                continue;
            }
            f.misses += 1;
            if f.misses >= MISSES {
                f.state = "resolved".into();
                f.ended = Some(now);
            }
            f.put(sup);
        }
        r.last_eval = Some(now);
        r.last_ms = Some((ran.ms * 100.0).round() / 100.0);
        r.last_rows = Some(ran.rows.len() as i64);
        r.last_error = ran.note;
        r.firing = firings_where(sup, "rule = :rule AND state = 'firing'", &Params::new().set("rule", Value::Str(r.id.clone()))).len() as i64;
        let _ = r.put(sup);
    }
    // Each rule's action, once per incident.
    for (iid, rid, agent) in acts {
        let Ok(inc) = incident(sup, &iid) else { continue };
        if inc.actions.iter().any(|a| a["rule"] == rid.as_str()) {
            continue;
        }
        let Some(r) = by_id.get(&rid) else { continue };
        let _ = act(sup, lv, &iid, &r.action, Some(&agent).filter(|a| !a.is_empty()).map(|a| a.as_str()), Some(&rid));
    }
    // Incidents whose firings all ended.
    let firing_now: HashSet<String> = firings_where(sup, "state = 'firing'", &Params::new()).into_iter().filter_map(|f| f.incident).collect();
    for mut inc in incidents_where(sup, "state = 'open'", &Params::new()) {
        if !firing_now.contains(&inc.id) {
            inc.state = "resolved".into();
            inc.resolved = Some(now);
            inc.updated = now;
            inc.needs_you = false;
            inc.put(sup);
            expire_questions(sup, lv, &inc);
        }
    }
    mem().lock().unwrap().last_eval_ms = Some(now);
    started
}

// ---------------------------------------------------------------- actions

/// Does `kind` for an incident (`notify`, `ask`, `pause`) and records it
/// in its "Done about it"; `rule` when a rule's action does it. Answers the
/// action as recorded.
fn act(sup: &Arc<Supervisor>, lv: &Arc<Levels>, iid: &str, kind: &str, agent: Option<&str>, rule: Option<&str>) -> Result<Json, ApiError> {
    let mut inc = incident(sup, iid)?;
    let firings = firings_where(sup, "incident = :i", &Params::new().set("i", Value::Str(iid.to_string())));
    let (fixed_by, where_words) = where_of(sup, &inc, &firings);
    // The agent it is about: not one whose branch fixed its test, when
    // another is in it.
    let agent = agent.map(str::to_string).or_else(|| inc.agents.iter().find(|a| !fixed_by.contains(a)).or(inc.agents.first()).cloned());
    let name = agent.as_deref().map(|a| name_of(sup, a));
    let fixer = agent.as_ref().is_some_and(|a| fixed_by.contains(a));
    let now = now_ms();
    let lead = firings
        .into_iter()
        .filter(|f| rule.is_none_or(|r| f.rule == r))
        .max_by_key(|f| f.started)
        .map(|f| f.summary)
        .unwrap_or_else(|| inc.title.clone());
    let lead = if where_words.is_empty() { lead } else { format!("{}. {where_words}", lead.trim_end_matches('.')) };
    let mut action = json!({ "kind": kind, "at_ms": now, "at": iso(now), "agent": agent, "rule": rule, "ref": null, "state": "done", "by": if rule.is_some() { "rule" } else { "you" } });
    match kind {
        "notify" => {
            inc.needs_you = true;
            action["detail"] = json!(format!("In Needs you: {}", one_line(&lead, 160)));
        }
        "ask" => {
            let a = agent.as_deref().and_then(|a| sup.agent_row(a)).ok_or_else(|| ApiError::bad("the incident has no agent to ask about"))?;
            let n = a.name.clone();
            // Its branch fixed the test: what is left is to merge the fix,
            // and pausing it is not offered first.
            let (options, recommended) = if fixer {
                (
                    json!([
                        { "label": format!("Tell {n} to get its fix merged"), "detail": "It is told its branch passes the test and the project's checkout still fails, and to commit the fix and open a pull request", "matches_precedent": false },
                        { "label": "Let it carry on", "detail": "Nothing is sent; the incident stays open until its rules stop firing", "matches_precedent": false },
                        { "label": format!("Pause {n}"), "detail": "Esc in its terminal: it stops what it is doing and waits for you", "matches_precedent": false },
                    ]),
                    0,
                )
            } else {
                (
                    json!([
                        { "label": format!("Pause {n}"), "detail": "Esc in its terminal: it stops what it is doing and waits for you", "matches_precedent": false },
                        { "label": format!("Tell {n} to stop and report"), "detail": "It is told to stop retrying and say what it found", "matches_precedent": false },
                        { "label": "Let it carry on", "detail": "Nothing is sent; the incident stays open until its rules stop firing", "matches_precedent": false },
                    ]),
                    1,
                )
            };
            let q = Question {
                id: new_id("q"),
                agent: Some(a.id.clone()),
                outcome: a.outcome.clone(),
                repo: Some(a.repo.clone()),
                kind: "alert".into(),
                text: format!("{}. What should {n} do?", one_line(&lead, 400).trim_end_matches('.')),
                context: Some(format!("Health: {} (incident {})", inc.title, inc.id)),
                options,
                recommended: Some(recommended),
                precedent: None,
                state: "open".into(),
                created: now,
                ..Question::default()
            };
            {
                let _w = lv.write.lock().unwrap_or_else(|e| e.into_inner());
                q.put(sup)?;
            }
            let _ = sup.note(&a.id, "question", format!("asked you: what should {n} do?"), Some(json!({ "question": q.id, "incident": inc.id }).to_string()), |_| {});
            lv.changed(&q.id);
            inc.needs_you = true;
            action["ref"] = json!(q.id);
            action["state"] = json!("open");
            action["detail"] = json!(format!("Asked you in Decide: what should {n} do?"));
        }
        "pause" if fixer && rule.is_some() => {
            // A rule does not pause the agent whose branch fixed the test.
            let n = name.clone().unwrap_or_default();
            action["state"] = json!("skipped");
            action["detail"] = json!(format!("Not paused: {n}'s branch fixed it. {where_words}"));
        }
        "pause" => {
            let a = agent.clone().ok_or_else(|| ApiError::bad("the incident has no agent to pause"))?;
            let n = name.clone().unwrap_or_else(|| a.clone());
            match sup.keys(&a, b"\x1b") {
                Ok(()) => {
                    action["detail"] = json!(format!("Paused {n} (Esc)"));
                    let _ = sup.note(&a, "alert", format!("paused by Health: {}", one_line(&lead, 160)), Some(json!({ "incident": inc.id, "action": "pause" }).to_string()), |_| {});
                }
                Err(e) => {
                    action["state"] = json!("failed");
                    action["detail"] = json!(format!("Could not pause {n}: {}", e.message));
                }
            }
        }
        _ => return Err(ApiError::bad("kind is notify, ask or pause")),
    }
    inc.actions.push(action.clone());
    inc.updated = now;
    inc.put(sup);
    Ok(action)
}

/// The agents whose branch fixed an incident's tests, and where its tests
/// pass and fail now, in a sentence ("" when it names no test).
fn where_of(sup: &Supervisor, inc: &Incident, firings: &[Firing]) -> (Vec<String>, String) {
    let rules = rules_by_id(sup);
    let mut fixed_by = Vec::new();
    let mut words = Vec::new();
    for t in tests_of(firings.iter(), &rules).iter().take(3) {
        let Some(w) = crate::testruns::where_now(sup, &inc.repo, t) else { continue };
        for a in w["fixed_by"].as_array().into_iter().flatten().filter_map(Json::as_str) {
            add_unique(&mut fixed_by, a);
        }
        words.push(format!("{t}: {}", w["sentence"].as_str().unwrap_or("")));
    }
    (fixed_by, words.join(" "))
}

/// An alert's question was answered: Pause sends Esc, "Tell … to stop and
/// report" steers the agent, "Let it carry on" does nothing; other words
/// are sent to the agent as they are.
fn on_question(sup: &Arc<Supervisor>, lv: &Arc<Levels>, qid: &str) {
    let Ok(q) = crate::questions::get(sup, qid) else { return };
    if q.kind != "alert" || q.state == "open" {
        return;
    }
    if let Some(inc) = incidents_where(sup, "id != ''", &Params::new()).into_iter().find(|i| i.actions.iter().any(|a| a["ref"] == qid)) {
        answer_of(sup, lv, inc, &q);
    }
}

fn answer_of(sup: &Arc<Supervisor>, lv: &Arc<Levels>, mut inc: Incident, q: &Question) {
    let Some(i) = inc.actions.iter().position(|a| a["ref"] == q.id.as_str() && a["state"] == "open") else { return };
    let agent = q.agent.clone().unwrap_or_default();
    let n = name_of(sup, &agent);
    let answer = q.answer.clone().unwrap_or_default();
    let first = answer.lines().next().unwrap_or("").trim().to_string();
    let said = one_line(&answer, 200);
    inc.actions[i]["state"] = json!(if q.state == "answered" { "answered" } else { "expired" });
    if q.state != "answered" {
        inc.put(sup);
        return;
    }
    inc.actions[i]["detail"] = json!(format!("Asked you in Decide: you said “{said}”"));
    inc.needs_you = false;
    inc.put(sup);
    let rest = answer.lines().skip(1).collect::<Vec<_>>().join("\n");
    if first == format!("Pause {n}") {
        let _ = act(sup, lv, &inc.id, "pause", Some(&agent), None);
    } else if first == format!("Tell {n} to get its fix merged") {
        let text = if rest.trim().is_empty() { "Your branch passes the test this alert is about, and the project's checkout still fails it. Get the fix merged: commit it, open a pull request, and say if anything is left.".to_string() } else { rest };
        let _ = crate::levels::steer(sup, lv, &agent, &text);
    } else if first == format!("Tell {n} to stop and report") {
        let text = if rest.trim().is_empty() { "Stop retrying. Report what you found so far: what fails, what you tried, and what you think is wrong.".to_string() } else { rest };
        let _ = crate::levels::steer(sup, lv, &agent, &text);
    } else if first != "Let it carry on" && !answer.trim().is_empty() {
        let _ = crate::levels::steer(sup, lv, &agent, &answer);
    }
}

/// The open alert questions of an incident expire when it is resolved.
fn expire_questions(sup: &Supervisor, lv: &Levels, inc: &Incident) {
    for a in inc.actions.iter().filter(|a| a["state"] == "open") {
        let Some(qid) = a["ref"].as_str() else { continue };
        let _w = lv.write.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut q) = crate::questions::get(sup, qid) {
            if q.state == "open" {
                q.state = "expired".into();
                q.answered = Some(now_ms());
                let _ = q.put(sup);
                drop(_w);
                lv.changed(&q.id);
            }
        }
    }
}

/// You resolve an incident: its firings that still fire are silenced
/// until they stop (so it does not open again right away).
fn resolve(sup: &Arc<Supervisor>, lv: &Arc<Levels>, iid: &str) -> Result<Incident, ApiError> {
    let mut inc = incident(sup, iid)?;
    let now = now_ms();
    for mut f in firings_where(sup, "incident = :i AND state = 'firing'", &Params::new().set("i", Value::Str(iid.to_string()))) {
        f.state = "silenced".into();
        f.put(sup);
    }
    if inc.state != "resolved" {
        inc.state = "resolved".into();
        inc.resolved = Some(now);
        inc.updated = now;
        inc.needs_you = false;
        inc.actions.push(json!({ "kind": "resolve", "at_ms": now, "at": iso(now), "agent": null, "rule": null, "ref": null, "state": "done", "by": "you", "detail": "Resolved by you" }));
        inc.put(sup);
        expire_questions(sup, lv, &inc);
    }
    Ok(inc)
}

// ------------------------------------------------------- what hooks bring

/// Remembers an agent's transcript, whose usage lines become `token_use`.
pub fn note_transcript(id: &str, path: &str) {
    let mut m = mem().lock().unwrap();
    let e = m.transcripts.entry(id.to_string()).or_insert_with(|| (PathBuf::from(path), u64::MAX));
    if e.0 != std::path::Path::new(path) {
        *e = (PathBuf::from(path), u64::MAX);
    }
}

/// An Edit or Write of an agent: a row of `file_edits`, its path relative
/// to the agent's checkout.
pub fn record_edit(sup: &Arc<Supervisor>, id: &str, hook: &Json) {
    let Some(a) = sup.agent_row(id) else { return };
    let input = &hook["tool_input"];
    let Some(fp) = input["file_path"].as_str().or_else(|| input["notebook_path"].as_str()).filter(|s| !s.is_empty()) else { return };
    let rel = rel_path(fp, &a.cwd).or_else(|| rel_path(fp, &a.repo)).unwrap_or_else(|| fp.to_string());
    let seq = sup.db().query("SELECT seq FROM events WHERE agent = :a ORDER BY seq DESC LIMIT 1", &Params::new().set("a", Value::Str(id.to_string()))).ok().and_then(|r| r.first().and_then(|r| get_int(&r[0])));
    let p = Params::new()
        .set("id", Value::Str(new_id("e")))
        .set("repo", Value::Str(a.repo.clone()))
        .set("agent", Value::Str(id.to_string()))
        .set("file", Value::Str(rel))
        .set("tool", Value::Str(hook["tool_name"].as_str().unwrap_or("").to_string()))
        .set("ts", ts(now_ms()))
        .set("seq", seq.map_or(Value::Null, Value::Int));
    let _ = sup.db().query("INSERT INTO file_edits (id, repo, agent, file, tool, ts, seq) VALUES (:id, :repo, :agent, :file, :tool, :ts, :seq)", &p);
}

fn rel_path(fp: &str, base: &str) -> Option<String> {
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    fp.strip_prefix(base).and_then(|r| r.strip_prefix('/')).map(str::to_string).filter(|r| !r.is_empty())
}

/// Reads what each noted transcript gained since the last time (its last
/// 16 MiB the first time): each assistant message's usage is a row of
/// `token_use`, keyed by the message's id.
fn ingest_tokens(sup: &Supervisor) {
    let list: Vec<(String, PathBuf, u64)> = mem().lock().unwrap().transcripts.iter().map(|(a, (p, o))| (a.clone(), p.clone(), *o)).collect();
    for (agent, path, from) in list {
        let Ok(mut f) = std::fs::File::open(&path) else { continue };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut start = if from == u64::MAX { len.saturating_sub(TRANSCRIPT_TAIL) } else { from.min(len) };
        if from != u64::MAX && from > len {
            start = 0; // It was rewritten.
        }
        if start >= len {
            continue;
        }
        if f.seek(SeekFrom::Start(start)).is_err() {
            continue;
        }
        let mut buf = Vec::new();
        if f.take(len - start).read_to_end(&mut buf).is_err() {
            continue;
        }
        let mut text = String::from_utf8_lossy(&buf).into_owned();
        if from == u64::MAX && start > 0 {
            // Started mid-line: skip to the next.
            text = text.split_once('\n').map(|x| x.1.to_string()).unwrap_or_default();
        }
        // Only whole lines; the rest waits for the next run.
        let whole = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let read_to = len - (text.len() - whole) as u64;
        let repo = sup.agent_row(&agent).map(|a| a.repo).unwrap_or_default();
        for line in text[..whole].lines() {
            if !line.contains("\"usage\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Json>(line) else { continue };
            let msg = &v["message"];
            let u = &msg["usage"];
            if !u.is_object() {
                continue;
            }
            let g = |k: &str| u[k].as_i64().unwrap_or(0);
            let mid = msg["id"].as_str().or_else(|| v["uuid"].as_str()).unwrap_or("").to_string();
            if mid.is_empty() {
                continue;
            }
            let when = v["timestamp"].as_str().and_then(chloedb::timestamp::parse).map(Value::Timestamp).unwrap_or_else(|| ts(now_ms()));
            let p = Params::new()
                .set("id", Value::Str(format!("{agent}:{mid}")))
                .set("repo", Value::Str(repo.clone()))
                .set("agent", Value::Str(agent.clone()))
                .set("input", Value::Int(g("input_tokens") + g("cache_creation_input_tokens")))
                .set("output", Value::Int(g("output_tokens")))
                .set("cache_read", Value::Int(g("cache_read_input_tokens")))
                .set("ts", when);
            let _ = sup.db().query("INSERT OR REPLACE INTO token_use (id, repo, agent, input, output, cache_read, ts) VALUES (:id, :repo, :agent, :input, :output, :cache_read, :ts)", &p);
        }
        if let Some(e) = mem().lock().unwrap().transcripts.get_mut(&agent) {
            if e.0 == path {
                e.1 = read_to;
            }
        }
    }
}

// ------------------------------------------------------------------ board

fn rules_by_id(sup: &Supervisor) -> HashMap<String, Rule> {
    all_rules(sup).into_iter().map(|r| (r.id.clone(), r)).collect()
}

/// `GET /api/incidents`.
pub fn list(sup: &Supervisor, repo: Option<&str>, state: Option<&str>) -> Json {
    let rules = rules_by_id(sup);
    let firings = firings_where(sup, "id != ''", &Params::new());
    let mut all = incidents_where(sup, "id != ''", &Params::new());
    if let Some(r) = repo.filter(|r| !r.is_empty()) {
        all.retain(|i| i.repo == r);
    }
    all.sort_by(|a, b| (b.state == "open").cmp(&(a.state == "open")).then(b.updated.cmp(&a.updated)));
    let counts = json!({ "open": all.iter().filter(|i| i.state == "open").count(), "resolved": all.iter().filter(|i| i.state == "resolved").count() });
    let st = state.filter(|s| !s.is_empty()).unwrap_or("all");
    let shown: Vec<Json> = all.iter().filter(|i| st == "all" || i.state == st).take(200).map(|i| i.to_json(sup, &firings, &rules)).collect();
    let mut ids: Vec<String> = Vec::new();
    for i in &all {
        for a in &i.agents {
            add_unique(&mut ids, a);
        }
    }
    let agents: Vec<Json> = ids.iter().filter_map(|a| sup.agent_json_of(a)).collect();
    json!({ "incidents": shown, "counts": counts, "agents": agents, "eval_every_s": EVERY_S, "last_eval_ms": mem().lock().unwrap().last_eval_ms })
}

/// `GET /api/incidents/:id`: the incident and its board.
pub fn board(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let inc = incident(sup, id)?;
    let rules = rules_by_id(sup);
    let firings = firings_where(sup, "incident = :i", &Params::new().set("i", Value::Str(id.to_string())));
    let ij = inc.to_json(sup, &firings, &rules);
    let from = inc.started - 6 * 60_000;
    let to = inc.resolved.unwrap_or_else(now_ms);
    let mut missing = Vec::new();
    let sentences: Vec<Json> = inc
        .agents
        .iter()
        .filter_map(|a| sup.agent_row(a).map(|r| (a, r)))
        .map(|(a, r)| json!({ "agent": sup.agent_json_of(a), "narration": r.narration, "narration_ts": r.narration_ts.map(iso), "state": r.state }))
        .collect();
    // Its agents' events in the window, oldest first.
    let mut events: Vec<Json> = Vec::new();
    for a in &inc.agents {
        let p = Params::new().set("a", Value::Str(a.clone())).set("from", ts(from)).set("to", ts(to + 1000));
        let rows = sup.db().query("SELECT id, seq, ts, agent, kind, tool, summary FROM events WHERE agent = :a AND ts >= :from AND ts <= :to ORDER BY seq DESC LIMIT 150", &p).unwrap_or_default();
        for r in rows {
            let kind = get_str(&r[4]).unwrap_or_default();
            if matches!(kind.as_str(), "input" | "SessionStart" | "narration") {
                continue;
            }
            let ms = get_ms(&r[2]).unwrap_or(0);
            let summary = get_str(&r[6]).unwrap_or_default();
            let failed = kind == "PostToolUseFailure" || kind == "alert" && summary.starts_with('▲');
            events.push(json!({
                "id": get_str(&r[0]), "seq": get_int(&r[1]), "ts": iso(ms), "ts_ms": ms, "agent": get_str(&r[3]),
                "kind": if kind == "alert" && summary.starts_with('▲') { "fired".to_string() } else { kind }, "tool": get_str(&r[5]), "summary": summary, "failed": failed,
            }));
        }
    }
    events.sort_by_key(|e| (e["ts_ms"].as_i64().unwrap_or(0), e["seq"].as_i64().unwrap_or(0)));
    if events.len() > 150 {
        events.drain(..events.len() - 150);
    }
    // The tests its test rules fired on.
    let tests_named = tests_of(firings.iter(), &rules);
    let tests = crate::testruns::board_tests(sup, &inc.repo, &tests_named);
    let error = tests.iter().find_map(|t| t["error"].as_str().map(str::to_string));
    let related = related(sup, &inc, &mut missing);
    Ok(json!({
        "incident": ij,
        "board": {
            "firings": ij["firings"], "sentences": sentences, "events": events, "tests": tests, "error": error, "related": related,
            "window": { "from_ms": from, "to_ms": to }, "missing": missing,
        }
    }))
}

/// What ChloeCtx keeps about the incident's files: decisions, preferences
/// and facts naming them (asked at most once a minute per incident).
fn related(sup: &Supervisor, inc: &Incident, missing: &mut Vec<String>) -> Vec<Json> {
    if inc.files.is_empty() {
        return Vec::new();
    }
    if let Some((at, v)) = mem().lock().unwrap().related.get(&inc.id) {
        if at.elapsed() < Duration::from_secs(60) {
            if let Some(m) = v["missing"].as_str() {
                missing.push(m.to_string());
            }
            return v["related"].as_array().cloned().unwrap_or_default();
        }
    }
    let dir = crate::trail::repo_dir(Some(&inc.repo)).unwrap_or_else(|_| PathBuf::from(&inc.repo));
    let sql = "SELECT id, ts, kind, text, files FROM memory WHERE kind = 'decision' OR kind = 'preference' OR kind = 'fact' ORDER BY ts DESC LIMIT 500";
    let (out, miss) = if !dir.is_dir() {
        (Vec::new(), Some(format!("the repository {} is not on this machine, so ChloeCtx was not asked", inc.repo)))
    } else {
        match run_chloectx(&sup.cfg.chloectx_bin, &dir, &["sql", "--json", sql], Duration::from_secs(30)) {
            Ran::Missing => (Vec::new(), Some(crate::trail::not_installed(&sup.cfg.chloectx_bin))),
            Ran::Failed(e) => (Vec::new(), Some(e)),
            Ran::Out(v) => {
                let cols: Vec<String> = v["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
                let col = |row: &Vec<Json>, name: &str| cols.iter().position(|c| c == name).and_then(|i| row.get(i).cloned()).unwrap_or(Json::Null);
                let mut list = Vec::new();
                for row in v["rows"].as_array().into_iter().flatten().filter_map(Json::as_array) {
                    let files: Vec<String> = match col(row, "files") {
                        Json::String(s) => serde_json::from_str::<Vec<String>>(&s).unwrap_or_else(|_| s.split([',', ' ', '\n']).filter(|x| !x.is_empty()).map(str::to_string).collect()),
                        Json::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                        _ => Vec::new(),
                    };
                    let text = col(row, "text").as_str().unwrap_or("").to_string();
                    let hit = inc.files.iter().any(|f| files.iter().any(|g| g == f || g.ends_with(&format!("/{f}")) || f.ends_with(&format!("/{g}"))) || text.contains(f.as_str()));
                    if hit {
                        list.push(json!({ "id": col(row, "id"), "kind": col(row, "kind"), "text": text, "ts": col(row, "ts"), "files": files }));
                    }
                    if list.len() >= 5 {
                        break;
                    }
                }
                (list, None)
            }
        }
    };
    if let Some(m) = &miss {
        missing.push(m.clone());
    }
    mem().lock().unwrap().related.insert(inc.id.clone(), (Instant::now(), json!({ "related": out, "missing": miss })));
    out
}

/// `POST /api/alerts/test`: a rule's SQL run now, and what it would fire on.
fn test_now(sup: &Supervisor, b: &RuleBody) -> Json {
    let sql = b.sql.clone().unwrap_or_default();
    let cmp = b.cmp.clone().unwrap_or_else(|| ">=".into());
    let th = b.threshold.unwrap_or(1.0);
    let window_s = b.window_s.unwrap_or(3600);
    let repo = b.repo.clone().filter(|r| !r.is_empty());
    let now = now_ms();
    let params = json!({ "since": iso(now - window_s * 1000), "now": iso(now), "repo": repo });
    match run_rule(sup, &sql, now, window_s, repo.as_deref()) {
        Err(e) => json!({ "ok": false, "error": e.message, "columns": [], "rows": [], "count": 0, "would_fire": [], "params": params, "note": null }),
        Ok(ran) => {
            let r = Rule {
                id: b.id.clone().unwrap_or_default(),
                repo: repo.clone(),
                name: b.name.clone().unwrap_or_else(|| "this rule".into()),
                description: String::new(),
                sql: sql.clone(),
                cmp: cmp.clone(),
                threshold: th,
                window_s,
                severity: "minor".into(),
                action: "none".into(),
                builtin: false,
                enabled: true,
                created: now,
                updated: now,
                last_eval: None,
                last_ms: None,
                last_rows: None,
                last_error: None,
                firing: 0,
            };
            let would: Vec<Json> = ran
                .hits
                .iter()
                .filter(|h| passes(&cmp, h.v, th))
                .take(50)
                .map(|h| {
                    let agents: Vec<String> = h.agent.iter().cloned().collect();
                    let (summary, _) = words(sup, &r, h, &agents, now);
                    json!({ "entity": h.entity, "agent": h.agent, "agent_name": h.agent.as_deref().map(|a| name_of(sup, a)), "file": h.file, "detail": h.detail, "v": h.v, "summary": summary })
                })
                .collect();
            json!({
                "ok": true, "columns": ran.columns, "rows": ran.rows.iter().take(200).map(|r| r.iter().map(value_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
                "count": ran.rows.len(), "ms": (ran.ms * 100.0).round() / 100.0, "would_fire": would, "params": params, "note": ran.note,
            })
        }
    }
}

// --------------------------------------------------------- for mcp.rs

/// `list_incidents`' text: the open (or all) incidents, a line each.
pub fn incidents_text(v: &Json) -> String {
    let list = v["incidents"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No incidents.".into();
    }
    let mut out = Vec::new();
    for i in list {
        let names: Vec<&str> = i["agents"].as_array().into_iter().flatten().filter_map(|a| a["name"].as_str()).collect();
        out.push(format!("{} [{} {}] {} (agents: {}; rules: {})", i["id"].as_str().unwrap_or(""), i["state"].as_str().unwrap_or(""), i["severity"].as_str().unwrap_or(""), i["title"].as_str().unwrap_or(""), names.join(", "), i["rules"].as_array().into_iter().flatten().filter_map(Json::as_str).collect::<Vec<_>>().join(", ")));
        for f in i["firings"].as_array().into_iter().flatten().filter(|f| f["state"] == "firing").take(4) {
            out.push(format!("  ▲ {}: {}", f["rule_name"].as_str().unwrap_or(""), f["summary"].as_str().unwrap_or("")));
        }
    }
    out.join("\n")
}

// ----------------------------------------------------------------- routes

#[derive(Deserialize)]
struct ListQ {
    repo: Option<String>,
    state: Option<String>,
}

async fn incidents_route(State(app): State<App>, Query(q): Query<ListQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || Ok(list(&sup, q.repo.as_deref(), q.state.as_deref()))).await?).into_response())
}

async fn board_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || board(&sup, &id)).await?).into_response())
}

async fn resolve_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    let v = blocking(move || {
        let inc = resolve(&sup, &lv, &id)?;
        let firings = firings_where(&sup, "incident = :i", &Params::new().set("i", Value::Str(id.clone())));
        Ok(json!({ "incident": inc.to_json(&sup, &firings, &rules_by_id(&sup)) }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

#[derive(Deserialize)]
struct ActBody {
    kind: String,
    agent: Option<String>,
}

async fn act_route(State(app): State<App>, AxPath(id): AxPath<String>, body: Bytes) -> ApiResult {
    let b: ActBody = parse_body(&body)?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    let v = blocking(move || {
        let action = act(&sup, &lv, &id, &b.kind, b.agent.as_deref().filter(|a| !a.is_empty()), None)?;
        let inc = incident(&sup, &id)?;
        let firings = firings_where(&sup, "incident = :i", &Params::new().set("i", Value::Str(id.clone())));
        Ok(json!({ "ok": action["state"] != "failed", "action": action, "incident": inc.to_json(&sup, &firings, &rules_by_id(&sup)) }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

async fn rules_route(State(app): State<App>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || Ok(json!({ "rules": all_rules(&sup).iter().map(Rule::to_json).collect::<Vec<_>>(), "eval_every_s": EVERY_S, "last_eval_ms": mem().lock().unwrap().last_eval_ms, "tables": TABLES }))).await?;
    Ok(AxJson(v).into_response())
}

/// A rule as the routes take it; every field optional (PATCH changes what
/// it names).
#[derive(Deserialize, Default)]
struct RuleBody {
    id: Option<String>,
    repo: Option<String>,
    name: Option<String>,
    description: Option<String>,
    sql: Option<String>,
    cmp: Option<String>,
    threshold: Option<f64>,
    window_s: Option<i64>,
    severity: Option<String>,
    action: Option<String>,
    enabled: Option<bool>,
}

fn apply(r: &mut Rule, b: RuleBody, sup: &Supervisor) -> Result<(), ApiError> {
    if let Some(n) = b.name {
        let n = n.trim().to_string();
        if n.is_empty() || n.len() > 200 {
            return Err(ApiError::bad("name is 1 to 200 bytes"));
        }
        r.name = n;
    }
    if let Some(d) = b.description {
        r.description = one_line(&d, 1000);
    }
    if let Some(s) = b.sql {
        let s = checked_sql(&s)?;
        // It must run: an error now is an error every 30 s.
        run_rule(sup, &s, now_ms(), b.window_s.unwrap_or(r.window_s), r.repo.as_deref())?;
        r.sql = s;
    }
    if let Some(c) = b.cmp {
        if !CMPS.contains(&c.as_str()) {
            return Err(ApiError::bad(format!("cmp is one of {}", CMPS.join(" "))));
        }
        r.cmp = c;
    }
    if let Some(t) = b.threshold {
        if !t.is_finite() {
            return Err(ApiError::bad("threshold is a number"));
        }
        r.threshold = t;
    }
    if let Some(w) = b.window_s {
        if !(10..=30 * 86_400).contains(&w) {
            return Err(ApiError::bad("window_s is from 10 s to 30 days"));
        }
        r.window_s = w;
    }
    if let Some(s) = b.severity {
        if !SEVERITIES.contains(&s.as_str()) {
            return Err(ApiError::bad(format!("severity is one of {}", SEVERITIES.join(", "))));
        }
        r.severity = s;
    }
    if let Some(a) = b.action {
        if !ACTIONS.contains(&a.as_str()) {
            return Err(ApiError::bad(format!("action is one of {}", ACTIONS.join(", "))));
        }
        r.action = a;
    }
    if let Some(e) = b.enabled {
        r.enabled = e;
    }
    Ok(())
}

async fn create_rule_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let mut b: RuleBody = parse_body(&body)?;
    let sup = app.sup.clone();
    let v = blocking(move || {
        if b.sql.as_deref().is_none_or(|s| s.trim().is_empty()) {
            return Err(ApiError::bad("sql is required"));
        }
        if b.name.as_deref().is_none_or(|s| s.trim().is_empty()) {
            return Err(ApiError::bad("name is required"));
        }
        let now = now_ms();
        let id = match b.id.take().filter(|i| !i.trim().is_empty()) {
            Some(i) => {
                let ok = i.len() <= 64 && i.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                if !ok {
                    return Err(ApiError::bad("id is letters, digits, - and _, at most 64"));
                }
                if rule(&sup, &i).is_ok() {
                    return Err(ApiError::conflict(format!("there is a rule {i} already")));
                }
                i
            }
            None => new_id("r"),
        };
        let repo = b.repo.take().filter(|r| !r.is_empty());
        let mut r = Rule {
            id,
            repo,
            name: String::new(),
            description: String::new(),
            sql: String::new(),
            cmp: ">=".into(),
            threshold: 1.0,
            window_s: 3600,
            severity: "minor".into(),
            action: "notify".into(),
            builtin: false,
            enabled: true,
            created: now,
            updated: now,
            last_eval: None,
            last_ms: None,
            last_rows: None,
            last_error: None,
            firing: 0,
        };
        apply(&mut r, b, &sup)?;
        r.put(&sup)?;
        Ok(json!({ "rule": r.to_json() }))
    })
    .await?;
    Ok((StatusCode::CREATED, AxJson(v)).into_response())
}

async fn patch_rule_route(State(app): State<App>, AxPath(id): AxPath<String>, body: Bytes) -> ApiResult {
    let b: RuleBody = parse_body(&body)?;
    let sup = app.sup.clone();
    let v = blocking(move || {
        let mut r = rule(&sup, &id)?;
        apply(&mut r, b, &sup)?;
        r.updated = now_ms();
        r.put(&sup)?;
        Ok(json!({ "rule": r.to_json() }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

async fn delete_rule_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || {
        let r = rule(&sup, &id)?;
        if r.builtin {
            return Err(ApiError::conflict("a built-in rule can be switched off, not deleted"));
        }
        let p = Params::new().set("id", Value::Str(id.clone()));
        sup.db().query("DELETE FROM alert_rules WHERE id = :id", &p).map_err(ApiError::internal)?;
        let now = now_ms();
        for mut f in firings_where(&sup, "rule = :id AND state != 'resolved'", &p) {
            f.state = "resolved".into();
            f.ended = Some(now);
            f.put(&sup);
        }
        Ok(json!({ "ok": true }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

async fn test_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: RuleBody = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || Ok(test_now(&sup, &b))).await?).into_response())
}

/// `POST /api/alerts/check`: the rules run now (what the loop does every
/// 30 s), and the incidents as `GET /api/incidents` answers them.
async fn check_route(State(app): State<App>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    let v = blocking(move || {
        let started = evaluate(&sup, &lv);
        let mut v = list(&sup, None, None);
        v["started"] = json!(started);
        Ok(v)
    })
    .await?;
    Ok(AxJson(v).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_comparisons() {
        assert_eq!(short_num(486_120.0), "486k");
        assert_eq!(short_num(1_204_000.0), "1.2M");
        assert_eq!(short_num(2_000_000.0), "2M");
        assert_eq!(short_num(6.0), "6");
        assert!(passes(">=", 5.0, 5.0) && !passes(">", 5.0, 5.0) && passes("!=", 4.0, 5.0));
        assert!(checked_sql("select 1;").is_ok());
        assert!(checked_sql("DELETE FROM agents").is_err());
        assert!(checked_sql("SELECT 1; DELETE FROM agents").is_err());
        assert_eq!(rel_path("/w/shop/src/a.rs", "/w/shop/"), Some("src/a.rs".into()));
        assert_eq!(rel_path("/elsewhere/a.rs", "/w/shop"), None);
    }
}
