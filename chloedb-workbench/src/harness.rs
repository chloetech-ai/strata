//! Optimize the harness (HARNESS.md), its first two steps: versions and
//! findings, and the permissions and memory editors with their impact.
//!
//! A **run version** is what one session ran: a hash of the parts Workbench
//! writes into its `--settings` and `--mcp-config` (permissions, hooks,
//! environment, MCP servers), the brief it gives (the repository's rules),
//! the recall settings and pins, and Claude Code's version. It is recorded at
//! spawn on the agent's row (`agents.harness`), with its parts in
//! `harness_parts`. A **config version** is what the user edits: the rules a
//! version adds to or takes from Workbench's own, the recall settings and
//! pins, and whether hooks are timed. One is `current` per repository (new
//! agents get it), others are `draft` or `retired`. Promote makes a draft
//! current, roll back makes the one before current again. Nothing of the
//! user's is written: a version changes only the per-session files
//! Workbench writes for each agent.
//!
//! Findings are counted from rows (permission storms, unused skills and
//! servers, slow hooks, redo loops, your corrections) or read from ChloeCtx's
//! receipts (missed memory, `recomputed` when a replay on the snapshot
//! ChloeCtx kept gives the same decision). No model is called.

mod fixes;
mod memory;
mod perms;
mod stats;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::db::{get_int, get_ms, get_str, iso, now_ms, ts, Agent, Event};
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{one_line, ApiError, Supervisor};

pub use memory::{MemEdits, Pin};
pub use perms::{PermEdits, Rule};

/// The tables (src/db.rs makes them with the others). `kind` is `config`
/// (a version the user edits) or `run` (what a session ran); a run
/// version's `config` is the config version it applied.
pub const TABLES: [&str; 5] = [
    "CREATE TABLE IF NOT EXISTS harness_versions (id KEYWORD PRIMARY KEY, repo KEYWORD, name KEYWORD, kind KEYWORD, config KEYWORD, parent KEYWORD, parent2 KEYWORD, branch KEYWORD, state KEYWORD, made_by KEYWORD, note TEXT, claude_version KEYWORD, widens BOOL, approved_by KEYWORD, approved TIMESTAMP, created TIMESTAMP, promoted TIMESTAMP, retired TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS harness_parts (id KEYWORD PRIMARY KEY, kind KEYWORD, text TEXT OPTIONS(indexed = false), model TEXT OPTIONS(indexed = false), tokens INT)",
    "CREATE TABLE IF NOT EXISTS harness_version_parts (id KEYWORD PRIMARY KEY, version KEYWORD, kind KEYWORD, part KEYWORD)",
    "CREATE TABLE IF NOT EXISTS harness_findings (id KEYWORD PRIMARY KEY, repo KEYWORD, kind KEYWORD, title TEXT, detail TEXT, sessions TEXT, agents TEXT, cost_s FLOAT, cost_tokens INT, cost_checks INT, level KEYWORD, row_ids TEXT OPTIONS(indexed = false), version KEYWORD, state KEYWORD, first_ts TIMESTAMP, last_ts TIMESTAMP)",
    "CREATE TABLE IF NOT EXISTS hook_runs (id KEYWORD PRIMARY KEY, agent KEYWORD, version KEYWORD, event KEYWORD, source KEYWORD, command TEXT, ms INT, exit_code INT, tokens_added INT, ts TIMESTAMP)",
];

/// The default window, in days.
const WINDOW_DAYS: i64 = 7;
/// A hook slower than this at p95 is a finding.
const SLOW_HOOK_MS: f64 = 1000.0;
/// Prompts for one command from which it is a storm.
const STORM_MIN: usize = 3;

fn sv(s: &str) -> Value {
    Value::Str(s.to_string())
}

/// The first 16 hex of the SHA-256 of `text`.
fn hash(text: &str) -> String {
    chloedb::media::sha256_hex(text.as_bytes())[..16].to_string()
}

fn short(id: &str) -> String {
    id.chars().take(7).collect()
}

// ------------------------------------------------------------- versions

/// A version's row.
#[derive(Clone, Debug, Default)]
pub struct Version {
    pub id: String,
    pub repo: String,
    pub name: String,
    pub kind: String,
    pub config: Option<String>,
    pub parent: Option<String>,
    pub state: String,
    pub made_by: String,
    pub note: Option<String>,
    pub claude_version: Option<String>,
    pub widens: bool,
    pub approved_by: Option<String>,
    pub approved: Option<i64>,
    pub created: i64,
    pub promoted: Option<i64>,
    pub retired: Option<i64>,
}

const V_COLS: &str = "id, repo, name, kind, config, parent, state, made_by, note, claude_version, widens, approved_by, approved, created, promoted, retired";

impl Version {
    fn of(r: &[Value]) -> Version {
        Version {
            id: get_str(&r[0]).unwrap_or_default(),
            repo: get_str(&r[1]).unwrap_or_default(),
            name: get_str(&r[2]).unwrap_or_default(),
            kind: get_str(&r[3]).unwrap_or_default(),
            config: get_str(&r[4]),
            parent: get_str(&r[5]),
            state: get_str(&r[6]).unwrap_or_default(),
            made_by: get_str(&r[7]).unwrap_or_default(),
            note: get_str(&r[8]),
            claude_version: get_str(&r[9]),
            widens: get_int(&r[10]).unwrap_or(0) != 0,
            approved_by: get_str(&r[11]),
            approved: get_ms(&r[12]),
            created: get_ms(&r[13]).unwrap_or(0),
            promoted: get_ms(&r[14]),
            retired: get_ms(&r[15]),
        }
    }

    fn put(&self, sup: &Supervisor) -> Result<(), ApiError> {
        let o = |v: &Option<String>| v.as_ref().map_or(Value::Null, |s| sv(s));
        let t = |v: Option<i64>| v.map_or(Value::Null, ts);
        let p = Params::new()
            .set("id", sv(&self.id))
            .set("repo", sv(&self.repo))
            .set("name", sv(&self.name))
            .set("kind", sv(&self.kind))
            .set("config", o(&self.config))
            .set("parent", o(&self.parent))
            .set("state", sv(&self.state))
            .set("made_by", sv(&self.made_by))
            .set("note", o(&self.note))
            .set("claude_version", o(&self.claude_version))
            .set("widens", Value::Bool(self.widens))
            .set("approved_by", o(&self.approved_by))
            .set("approved", t(self.approved))
            .set("created", ts(self.created))
            .set("promoted", t(self.promoted))
            .set("retired", t(self.retired));
        let names: Vec<String> = V_COLS.split(", ").map(|c| format!(":{c}")).collect();
        sup.db().query(&format!("INSERT OR REPLACE INTO harness_versions ({V_COLS}) VALUES ({})", names.join(", ")), &p).map_err(ApiError::internal)?;
        Ok(())
    }

    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id, "short": short(&self.id), "repo": self.repo, "name": self.name, "kind": self.kind, "config": self.config,
            "parent": self.parent, "state": self.state, "made_by": self.made_by, "note": self.note, "claude_version": self.claude_version,
            "widens": self.widens, "approved_by": self.approved_by, "approved": self.approved.map(iso), "created": iso(self.created),
            "promoted": self.promoted.map(iso), "retired": self.retired.map(iso),
        })
    }
}

fn version(sup: &Supervisor, id: &str) -> Option<Version> {
    let rows = sup.db().query(&format!("SELECT {V_COLS} FROM harness_versions WHERE id = :id LIMIT 1"), &Params::new().set("id", sv(id))).ok()?;
    rows.first().map(|r| Version::of(r))
}

fn versions(sup: &Supervisor, repo: &str) -> Vec<Version> {
    let rows = sup.db().query(&format!("SELECT {V_COLS} FROM harness_versions WHERE repo = :repo ORDER BY created ASC LIMIT 10000"), &Params::new().set("repo", sv(repo))).unwrap_or_default();
    rows.iter().map(|r| Version::of(r)).collect()
}

/// Writes a part (shared by hash) and links it to a version.
fn put_part(sup: &Supervisor, version: &str, kind: &str, text: &str) -> Result<String, ApiError> {
    let id = hash(&format!("{kind}\n{text}"));
    let p = Params::new().set("id", sv(&id)).set("kind", sv(kind)).set("text", sv(text)).set("tokens", Value::Int((text.len() / 4) as i64));
    sup.db().query("INSERT OR REPLACE INTO harness_parts (id, kind, text, tokens) VALUES (:id, :kind, :text, :tokens)", &p).map_err(ApiError::internal)?;
    let p = Params::new().set("id", sv(&format!("{version}:{kind}"))).set("version", sv(version)).set("kind", sv(kind)).set("part", sv(&id));
    sup.db().query("INSERT OR REPLACE INTO harness_version_parts (id, version, kind, part) VALUES (:id, :version, :kind, :part)", &p).map_err(ApiError::internal)?;
    Ok(id)
}

/// A version's parts, `kind → text`.
fn parts_of(sup: &Supervisor, version: &str) -> BTreeMap<String, String> {
    let links = sup.db().query("SELECT kind, part FROM harness_version_parts WHERE version = :v LIMIT 100", &Params::new().set("v", sv(version))).unwrap_or_default();
    let mut out = BTreeMap::new();
    for l in links {
        let (Some(kind), Some(part)) = (get_str(&l[0]), get_str(&l[1])) else { continue };
        if let Some(r) = sup.db().query("SELECT text FROM harness_parts WHERE id = :id LIMIT 1", &Params::new().set("id", sv(&part))).ok().and_then(|r| r.into_iter().next()) {
            out.insert(kind, get_str(&r[0]).unwrap_or_default());
        }
    }
    out
}

/// What a config version edits.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Edits {
    pub permissions: PermEdits,
    pub memory: MemEdits,
    pub timing: bool,
}

impl Edits {
    fn texts(&self) -> [(&'static str, String); 3] {
        [
            ("permissions", serde_json::to_string_pretty(&self.permissions).unwrap_or_default()),
            ("memory", serde_json::to_string_pretty(&self.memory).unwrap_or_default()),
            ("hooks", serde_json::to_string_pretty(&json!({ "timing": self.timing })).unwrap_or_default()),
        ]
    }
    fn id(&self) -> String {
        hash(&self.texts().iter().map(|(k, t)| format!("{k}\n{t}\n")).collect::<String>())
    }
    fn to_json(&self) -> Json {
        json!({ "permissions": self.permissions, "memory": self.memory, "hooks": { "timing": self.timing } })
    }
}

fn edits_of(sup: &Supervisor, version: &str) -> Edits {
    let parts = parts_of(sup, version);
    Edits {
        permissions: parts.get("permissions").and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default(),
        memory: parts.get("memory").and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default(),
        timing: parts.get("hooks").and_then(|t| serde_json::from_str::<Json>(t).ok()).and_then(|v| v["timing"].as_bool()).unwrap_or(false),
    }
}

/// One repository's changes to its versions, one at a time.
fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

fn history_key(repo: &str) -> String {
    format!("harness:history:{repo}")
}

fn history(sup: &Supervisor, repo: &str) -> Vec<String> {
    sup.mark(&history_key(repo)).and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

/// The repository's current config version, made (1.0, Workbench's own
/// harness, nothing edited) the first time it is asked for.
pub fn current(sup: &Supervisor, repo: &str) -> Version {
    let _g = lock().lock().unwrap();
    current_locked(sup, repo)
}

fn current_locked(sup: &Supervisor, repo: &str) -> Version {
    let vs = versions(sup, repo);
    if let Some(v) = vs.iter().rev().find(|v| v.kind == "config" && v.state == "current") {
        return v.clone();
    }
    let e = Edits::default();
    let id = format!("{}{}", e.id(), "").chars().take(12).collect::<String>() + &hash(repo)[..4];
    let v = Version { id: id.clone(), repo: repo.into(), name: "1.0".into(), kind: "config".into(), state: "current".into(), made_by: "workbench".into(), note: Some("Workbench's own harness: worktree defaults, ChloeCtx wired, guarded commands".into()), created: now_ms(), promoted: Some(now_ms()), ..Version::default() };
    if v.put(sup).is_ok() {
        for (k, t) in e.texts() {
            let _ = put_part(sup, &id, k, &t);
        }
        let _ = sup.set_mark(&history_key(repo), &json!([id]).to_string());
    }
    v
}

/// The next version number of a repository: one more than the highest.
fn next_name(vs: &[Version]) -> String {
    let max = vs
        .iter()
        .filter(|v| v.kind == "config")
        .filter_map(|v| {
            let n = v.name.split('-').next()?;
            let (a, b) = n.split_once('.')?;
            Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?))
        })
        .max()
        .unwrap_or((1, 0));
    format!("{}.{}", max.0, max.1 + 1)
}

// --------------------------------------------------------------- baseline

/// Workbench's own rules for an agent of `repo` in its worktree (the
/// worktree as `<worktree>`), with where each comes from.
pub fn baseline(sup: &Supervisor, repo: &str) -> Vec<Rule> {
    let flow = sup.allowed(repo);
    let s = crate::flow::with_worktree_defaults(crate::flow::with_allowed(json!({}), &flow), Path::new("<worktree>"));
    let mut out = Vec::new();
    for d in ["deny", "ask", "allow"] {
        for r in s.pointer(&format!("/permissions/{d}")).and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str) {
            let from = if flow.iter().any(|f| f == r) { "Flow" } else { "Workbench" };
            out.push(Rule { decision: d.into(), text: r.to_string(), from: from.into() });
        }
    }
    out
}

/// The rules agents of `repo` get under a config version.
fn rules_under(sup: &Supervisor, repo: &str, v: &Version, e: &Edits) -> Vec<Rule> {
    let by = if v.made_by == "workbench" { "Workbench".to_string() } else { format!("you, {}", v.name) };
    perms::effective(&baseline(sup, repo), &e.permissions, &by)
}

// ------------------------------------------------------------------ spawn

/// What a session gets, after the current version is applied.
pub struct Applied {
    pub settings: Json,
    pub mcp: Json,
    /// The run version it got.
    pub version: Option<String>,
}

/// Claude Code's version (`claude --version`), once per program file.
fn claude_version(sup: &Supervisor) -> String {
    static C: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let bin = sup.cfg.claude_bin.clone();
    let stamp = std::fs::metadata(&bin).and_then(|m| m.modified()).ok().map(|t| format!("{t:?}")).unwrap_or_default();
    let key = format!("{bin}\n{stamp}");
    if let Some(v) = C.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().get(&key) {
        return v.clone();
    }
    let dir = sup.cfg.data_dir.join("harness");
    let _ = std::fs::create_dir_all(&dir);
    let v = match crate::trail::run_raw(&bin, &dir, &["--version"], Duration::from_secs(5)) {
        crate::trail::Ran::Out(Json::String(s)) => {
            let first = s.lines().next().unwrap_or("").trim().to_string();
            let ok = first.split_whitespace().next().is_some_and(|w| w.split('.').count() >= 2 && w.split('.').all(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit())));
            if ok {
                first.split_whitespace().next().unwrap_or("").to_string()
            } else {
                "unknown".into()
            }
        }
        _ => "unknown".into(),
    };
    C.get().unwrap().lock().unwrap().insert(key, v.clone());
    v
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A hook command timed into `log` (HARNESS.md, "hook_runs"): the time
/// before and after, the exit code kept. In a subshell, so that a shell
/// that runs it with `eval` keeps going.
fn timed(cmd: &str, event: &str, source: &str, log: &Path) -> String {
    format!(
        "( __t0=$(date +%s%N); ( {cmd} ); __c=$?; printf '%s\\t%s\\t%s\\t%s\\t%s\\n' {} {} \"$__t0\" \"$(date +%s%N)\" \"$__c\" >> {}; exit $__c )",
        shell_quote(event),
        shell_quote(source),
        shell_quote(&log.to_string_lossy())
    )
}

fn hook_source(cmd: &str) -> &'static str {
    if cmd.contains("chloectx") {
        "chloectx"
    } else if cmd.trim_end().ends_with(" hook") {
        "workbench"
    } else if cmd.contains("harness-pins") {
        "harness"
    } else {
        "yours"
    }
}

/// Text with the session's own values made placeholders, so that two
/// sessions of the same harness hash the same.
fn normalize(text: &str, cwd: &str, session_dir: &str, agent: &str, name: &str) -> String {
    let mut t = text.replace(session_dir, "<session>").replace(cwd, "<worktree>");
    if !agent.is_empty() {
        t = t.replace(agent, "<agent>");
    }
    if !name.is_empty() {
        t = t.replace(&format!("\"{name}\""), "\"<name>\"");
    }
    t
}

/// Applies the repository's current version to a session's files, writes
/// what it needs beside them, and records the run version it makes.
/// Never fails: a harness that cannot be read leaves the files as they were.
pub fn at_spawn(sup: &Supervisor, repo: &str, cwd: &Path, session_dir: &Path, agent: &str, name: &str, mut settings: Json, mut mcp: Json) -> Applied {
    let cfg = current(sup, repo);
    let e = edits_of(sup, &cfg.id);
    let cwd_s = cwd.to_string_lossy().to_string();
    // Permissions: Workbench's rules the version takes away, and its own.
    if !e.permissions.is_empty() {
        if !settings["permissions"].is_object() {
            settings["permissions"] = json!({});
        }
        for d in ["allow", "ask", "deny"] {
            let mut list: Vec<Json> = settings["permissions"][d].as_array().cloned().unwrap_or_default();
            let expand = |r: &str| r.replace("<worktree>", &cwd_s);
            list.retain(|x| x.as_str().is_none_or(|s| !e.permissions.remove.iter().any(|r| expand(r) == s)));
            let add = match d {
                "allow" => &e.permissions.allow,
                "ask" => &e.permissions.ask,
                _ => &e.permissions.deny,
            };
            for r in add {
                let r = json!(expand(r));
                if !list.contains(&r) {
                    list.push(r);
                }
            }
            if list.is_empty() {
                if let Some(o) = settings["permissions"].as_object_mut() {
                    o.remove(d);
                }
            } else {
                settings["permissions"][d] = Json::Array(list);
            }
        }
    }
    // Memory: the recall settings in the session's environment, and the
    // pins given at its start.
    if e.memory.settings_set() {
        if !settings["env"].is_object() {
            settings["env"] = json!({});
        }
        settings["env"]["CHLOECTX_RECALL"] = json!(e.memory.env_json().to_string());
    }
    if let Some(text) = e.memory.pins_text() {
        let file = session_dir.join("harness-pins.md");
        if std::fs::write(&file, text).is_ok() {
            if !settings["hooks"].is_object() {
                settings["hooks"] = json!({});
            }
            let mut list = settings["hooks"]["SessionStart"].as_array().cloned().unwrap_or_default();
            list.push(json!({ "hooks": [{ "type": "command", "command": format!("cat {}", shell_quote(&file.to_string_lossy())), "timeout": 5 }] }));
            settings["hooks"]["SessionStart"] = Json::Array(list);
        }
    }
    // Hooks: timed, when the version says so.
    if e.timing {
        let log = session_dir.join("hook-times.log");
        if let Some(hooks) = settings["hooks"].as_object_mut() {
            for (event, entries) in hooks.iter_mut() {
                for entry in entries.as_array_mut().into_iter().flatten() {
                    for h in entry["hooks"].as_array_mut().into_iter().flatten() {
                        if let Some(c) = h["command"].as_str().map(str::to_string) {
                            h["command"] = json!(timed(&c, event, hook_source(&c), &log));
                        }
                    }
                }
            }
        }
    }
    // The run version: what this session got, made comparable.
    let sd = session_dir.to_string_lossy().to_string();
    let norm = |v: &Json| normalize(&serde_json::to_string_pretty(v).unwrap_or_default(), &cwd_s, &sd, agent, name);
    let mut env = settings["env"].clone();
    for k in ["CHLOECTX_AGENT_NAME", "CHLOECTX_PROMPT_SOURCE", "CHLOECTX_PROMPT_LOG"] {
        if env.get(k).is_some() {
            env[k] = json!("<session>");
        }
    }
    let mut tools = mcp.clone();
    for (_, s) in tools["mcpServers"].as_object_mut().into_iter().flatten() {
        if let Some(env) = s["env"].as_object_mut() {
            for (_, v) in env.iter_mut() {
                *v = json!("<set>");
            }
        }
    }
    let brief: Vec<String> = crate::questions::rules(sup, Some(repo), true).into_iter().map(|r| format!("- {}", one_line(&r.text, 600))).collect();
    let claude = claude_version(sup);
    let parts: Vec<(&str, String)> = vec![
        ("permissions", norm(&settings["permissions"])),
        ("hooks", norm(&settings["hooks"])),
        ("env", norm(&env)),
        ("tools", norm(&tools)),
        ("brief", brief.join("\n")),
        ("memory", serde_json::to_string_pretty(&e.memory).unwrap_or_default()),
        ("claude", claude.clone()),
    ];
    let id = hash(&parts.iter().map(|(k, t)| format!("{k}\n{t}\n")).collect::<String>());
    if version(sup, &id).is_none() {
        let v = Version { id: id.clone(), repo: repo.into(), name: cfg.name.clone(), kind: "run".into(), config: Some(cfg.id.clone()), parent: Some(cfg.id.clone()), state: "observed".into(), made_by: "spawn".into(), claude_version: Some(claude), created: now_ms(), ..Version::default() };
        if v.put(sup).is_ok() {
            for (k, t) in &parts {
                let _ = put_part(sup, &id, k, t);
            }
        }
    }
    let _ = &mut mcp;
    Applied { settings, mcp, version: Some(id) }
}

// ------------------------------------------------------------------ window

/// The rows of a repository's window.
struct Win {
    repo: String,
    from: i64,
    to: i64,
    agents: Vec<Agent>,
    events: HashMap<String, Vec<Event>>,
    calls: Vec<perms::Call>,
    /// Each session's permission mode (the last `permission_mode` its
    /// hooks reported), `unknown` when none did.
    modes: HashMap<String, String>,
}

impl Win {
    fn load(sup: &Supervisor, repo: &str, from: i64, to: i64) -> Win {
        let mut agents: Vec<Agent> = sup.agent_rows().into_iter().filter(|a| a.repo == repo && a.created < to && a.ended.unwrap_or(i64::MAX) >= from).collect();
        agents.sort_by_key(|a| a.created);
        let mut events = HashMap::new();
        let mut calls = Vec::new();
        let mut modes = HashMap::new();
        for a in &agents {
            let evs: Vec<Event> = sup.all_events(&a.id).unwrap_or_default().into_iter().filter(|e| e.ts >= from && e.ts < to).collect();
            calls.extend(perms::calls_of(&a.id, &a.name, &a.cwd, &evs));
            let mode = evs.iter().rev().filter_map(|e| e.data.as_deref()).filter(|d| d.contains("permission_mode")).find_map(|d| serde_json::from_str::<Json>(d).ok()?["permission_mode"].as_str().map(str::to_string)).unwrap_or_else(|| "unknown".into());
            modes.insert(a.id.clone(), mode);
            events.insert(a.id.clone(), evs);
        }
        Win { repo: repo.into(), from, to, agents, events, calls, modes }
    }

    fn cwd(&self) -> String {
        self.agents.iter().rev().find(|a| a.cwd != a.repo).map_or_else(|| self.repo.clone(), |a| a.cwd.clone())
    }
}

fn window_of(days: Option<i64>) -> (i64, i64) {
    let to = now_ms() + 1;
    (to - days.unwrap_or(WINDOW_DAYS).clamp(1, 90) * 86_400_000, to)
}

/// The repository asked for, else the one of the newest agent.
fn repo_or_default(sup: &Supervisor, repo: Option<String>) -> Result<String, ApiError> {
    if let Some(r) = repo.filter(|r| !r.trim().is_empty()) {
        let p = PathBuf::from(r.trim());
        // An agent's worktree reads as its repository.
        if let Some(a) = sup.agent_rows().into_iter().find(|a| a.cwd == p.to_string_lossy() || a.repo == p.to_string_lossy()) {
            return Ok(a.repo);
        }
        return Ok(p.canonicalize().unwrap_or(p).to_string_lossy().to_string());
    }
    sup.agent_rows().into_iter().max_by_key(|a| a.created).map(|a| a.repo).ok_or_else(|| ApiError::bad("no repository yet: start an agent, or pass ?repo="))
}

// ---------------------------------------------------------------- findings

fn storm_key(cmd: &str) -> Option<String> {
    let first = perms::parts(cmd).into_iter().find(|p| !p.starts_with("cd ") && p != "cd")?;
    let words: Vec<&str> = first.split_whitespace().collect();
    let vars = words.iter().take_while(|w| w.contains('=') && !w.starts_with('-')).count();
    let rest = &words[vars..];
    if rest.is_empty() {
        return None;
    }
    let n = if rest.len() >= 3 && matches!(rest[1], "-m" | "run" | "exec") { 3 } else { 2.min(rest.len()) };
    let plain = |w: &&str| !w.contains(['"', '\'', '$', '`', '(', ')', '*', '>', '<', '/']) || w.starts_with("./");
    let mut k = n;
    while k > 1 && !rest[..k].iter().all(plain) {
        k -= 1;
    }
    Some(words[..vars + k].join(" "))
}

fn fmt_dur(ms: i64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i.min(v.len() - 1)]
}

fn names_of(set: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = set.iter().cloned().collect();
    v.sort();
    v
}

/// Which modes the prompts came in, as a sentence's end.
fn modes_sentence<'a>(modes: impl Iterator<Item = &'a str>, in_auto: usize) -> String {
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    for m in modes {
        *by.entry(m).or_default() += 1;
    }
    if by.keys().all(|m| *m == "unknown") {
        return " The sessions did not report their permission mode.".into();
    }
    let list: Vec<String> = by.iter().map(|(m, n)| format!("{n} in {m}")).collect();
    let auto = if in_auto > 0 { format!(" The {in_auto} in auto mode are auto mode handing the call back to you.") } else { String::new() };
    format!(" By permission mode: {}.{auto}", list.join(", "))
}

/// A finding as the API answers it.
fn finding(kind: &str, tone: &str, title: String, detail: String, agents: &HashSet<String>, sessions: usize, cost_ms: i64, cost_label: String, level: &str, rows: Vec<String>, extra: Json) -> Json {
    json!({
        "id": hash(&format!("{kind}\n{title}")), "kind": kind, "tone": tone, "title": title, "detail": detail,
        "agents": names_of(agents), "sessions": sessions, "cost_ms": cost_ms, "cost": cost_label, "level": level,
        "rows": rows.into_iter().take(50).collect::<Vec<_>>(), "extra": extra,
    })
}

/// Hook timings of the window's agents (their `hook-times.log`), written to
/// `hook_runs` as they are read: `(agent, event, source, ms, exit, ts)`.
fn hook_times(sup: &Supervisor, w: &Win) -> Vec<(String, String, String, f64, i64, i64)> {
    let mut out = Vec::new();
    for a in &w.agents {
        let log = sup.session_dir(&a.id).join("hook-times.log");
        let Ok(text) = std::fs::read_to_string(&log) else { continue };
        for (i, line) in text.lines().enumerate() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 {
                continue;
            }
            let (Ok(t0), Ok(t1)) = (f[2].parse::<i128>(), f[3].parse::<i128>()) else { continue };
            if t1 < t0 {
                continue;
            }
            let ms = (t1 - t0) as f64 / 1e6;
            let at = (t1 / 1_000_000) as i64;
            if at < w.from || at >= w.to {
                continue;
            }
            let code = f[4].trim().parse::<i64>().unwrap_or(0);
            let id = format!("{}:{i}", a.id);
            let p = Params::new().set("id", sv(&id)).set("agent", sv(&a.id)).set("version", a.harness.as_deref().map_or(Value::Null, sv)).set("event", sv(f[0])).set("source", sv(f[1])).set("ms", Value::Int(ms.round() as i64)).set("exit_code", Value::Int(code)).set("ts", ts(at));
            let _ = sup.db().query("INSERT OR REPLACE INTO hook_runs (id, agent, version, event, source, ms, exit_code, ts) VALUES (:id, :agent, :version, :event, :source, :ms, :exit_code, :ts)", &p);
            out.push((a.name.clone(), f[0].to_string(), f[1].to_string(), ms, code, at));
        }
    }
    out
}

/// Each hook's p50 and p95 in the window.
fn hook_stats(times: &[(String, String, String, f64, i64, i64)]) -> Vec<Json> {
    let mut by: BTreeMap<(String, String), (Vec<f64>, HashSet<String>, i64)> = BTreeMap::new();
    for (agent, ev, src, ms, code, _) in times {
        let e = by.entry((ev.clone(), src.clone())).or_default();
        e.0.push(*ms);
        e.1.insert(agent.clone());
        if *code != 0 {
            e.2 += 1;
        }
    }
    by.into_iter()
        .map(|((ev, src), (mut v, agents, fails))| {
            let total: f64 = v.iter().sum();
            json!({ "event": ev, "source": src, "runs": v.len(), "p50_ms": pct(&mut v, 0.5), "p95_ms": pct(&mut v, 0.95), "total_ms": total, "failures": fails, "agents": names_of(&agents) })
        })
        .collect()
}

/// Skills and subagents of the repository, with the tokens their
/// descriptions add to every turn.
fn skills_of(repo: &str) -> Vec<(String, String, i64)> {
    let mut out = Vec::new();
    let desc = |text: &str| -> i64 {
        let fm: String = text.lines().skip_while(|l| l.trim() != "---").skip(1).take_while(|l| l.trim() != "---").filter(|l| l.starts_with("description:") || l.starts_with("name:")).collect::<Vec<_>>().join(" ");
        ((if fm.is_empty() { text.chars().take(400).count() } else { fm.len() }) / 4) as i64 + 8
    };
    if let Ok(rd) = std::fs::read_dir(Path::new(repo).join(".claude/skills")) {
        for d in rd.flatten() {
            if let Ok(t) = std::fs::read_to_string(d.path().join("SKILL.md")) {
                out.push(("skill".to_string(), d.file_name().to_string_lossy().to_string(), desc(&t)));
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(Path::new(repo).join(".claude/agents")) {
        for d in rd.flatten() {
            let p = d.path();
            if p.extension().is_some_and(|e| e == "md") {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    out.push(("subagent".to_string(), p.file_stem().unwrap_or_default().to_string_lossy().to_string(), desc(&t)));
                }
            }
        }
    }
    out
}

/// The MCP servers the window's sessions were given: Workbench's file and
/// the repository's `.mcp.json` (names only; nothing of yours is written).
fn servers_of(sup: &Supervisor, w: &Win) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for a in &w.agents {
        if let Ok(t) = std::fs::read_to_string(sup.session_dir(&a.id).join("mcp.json")) {
            if let Ok(v) = serde_json::from_str::<Json>(&t) {
                for k in v["mcpServers"].as_object().into_iter().flatten().map(|(k, _)| k) {
                    out.entry(k.clone()).or_insert_with(|| "Workbench".to_string());
                }
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(Path::new(&w.repo).join(".mcp.json")) {
        if let Ok(v) = serde_json::from_str::<Json>(&t) {
            for k in v["mcpServers"].as_object().into_iter().flatten().map(|(k, _)| k) {
                out.entry(k.clone()).or_insert_with(|| ".mcp.json".to_string());
            }
        }
    }
    out
}

struct Found {
    findings: Vec<Json>,
    misses: Vec<memory::Miss>,
    rcs: Vec<memory::Rc>,
    hooks: Vec<Json>,
    timing: bool,
    missing: Vec<String>,
    storms: Vec<(String, usize)>,
}

/// The detectors (HARNESS.md, "1. Find"), worst first.
fn find(sup: &Supervisor, w: &Win, rules: &[Rule], with_memory: bool) -> Found {
    let mut out: Vec<(f64, Json)> = Vec::new();
    // Permission storms.
    let mut storms: BTreeMap<String, Vec<&perms::Call>> = BTreeMap::new();
    for c in w.calls.iter().filter(|c| c.prompted && c.probe.tool == "Bash") {
        if let Some(k) = c.probe.command.as_deref().and_then(storm_key) {
            storms.entry(k).or_default().push(c);
        }
    }
    let mut storm_list = Vec::new();
    for (key, cs) in &storms {
        if cs.len() < STORM_MIN {
            continue;
        }
        // A storm the current rules already allow is shown as handled,
        // and suggests nothing.
        let handled = cs.last().is_some_and(|c| perms::decide(rules, &c.probe) == "allow");
        if !handled {
            storm_list.push((key.clone(), cs.len()));
        }
        let agents: HashSet<String> = cs.iter().map(|c| c.name.clone()).collect();
        let sessions: HashSet<&str> = cs.iter().map(|c| c.agent.as_str()).collect();
        let allowed = cs.iter().filter(|c| c.outcome == "ran").count();
        let wait: i64 = cs.iter().map(|c| c.wait_ms).sum();
        let b = perms::bare(key);
        let near = rules.iter().find(|r| r.decision == "allow" && b != *key && perms::parse(&r.text).1.is_some_and(|s| s.trim_end_matches(":*").trim_end_matches(" *") == b || b.starts_with(&format!("{} ", s.trim_end_matches(":*")))));
        let in_auto = cs.iter().filter(|c| c.mode == "auto").count();
        let mut detail = match near {
            _ if handled => format!("`{key}` asked {} times. The current version's rules allow it now.", cs.len()),
            Some(r) => format!("`{key}` asked {} times. The rule `{}` never matches it: a rule matches from the first word typed.", cs.len(), r.text),
            None => format!("`{key}` asked {} times; no rule allows it.", cs.len()),
        };
        detail.push_str(&modes_sentence(cs.iter().map(|c| c.mode.as_str()), in_auto));
        out.push((
            (wait as f64 / 1000.0 + cs.len() as f64 * 10.0) * if handled { 0.2 } else { 1.0 },
            finding("storm", if handled { "" } else { "warn" }, format!("Permission storm: {key}"), detail, &agents, sessions.len(), wait, if wait > 0 { fmt_dur(wait) } else { format!("{} prompts", cs.len()) }, "counted", cs.iter().map(|c| format!("{}@{}", c.agent, c.ts)).collect(), json!({ "command": key, "prompts": cs.len(), "allowed": allowed, "refused": cs.len() - allowed, "rule": format!("Bash({key}:*)"), "near": near.map(|r| r.text.clone()), "in_auto": in_auto, "handled": handled })),
        ));
    }
    // Auto mode: what its classifier blocked, and the prompts it handed
    // back to you (a permission prompt in a session in auto mode).
    let blocked: Vec<&perms::Call> = w.calls.iter().filter(|c| c.outcome == "blocked").collect();
    let escalated: Vec<&perms::Call> = w.calls.iter().filter(|c| c.prompted && c.mode == "auto").collect();
    if !blocked.is_empty() || !escalated.is_empty() {
        let both: Vec<&perms::Call> = blocked.iter().chain(escalated.iter()).copied().collect();
        let agents: HashSet<String> = both.iter().map(|c| c.name.clone()).collect();
        let sessions: HashSet<&str> = both.iter().map(|c| c.agent.as_str()).collect();
        let mut what: BTreeMap<String, usize> = BTreeMap::new();
        for c in &both {
            *what.entry(c.probe.command.as_deref().and_then(storm_key).unwrap_or_else(|| c.probe.tool.clone())).or_default() += 1;
        }
        let mut top: Vec<(String, usize)> = what.into_iter().collect();
        top.sort_by_key(|x| std::cmp::Reverse(x.1));
        let wait: i64 = escalated.iter().map(|c| c.wait_ms).sum();
        let s = |n: usize| if n == 1 { "" } else { "s" };
        let detail = format!(
            "In auto mode the classifier blocked {} call{} (PermissionDenied events) and {} call{} still asked you. Most: {}.",
            blocked.len(),
            s(blocked.len()),
            escalated.len(),
            s(escalated.len()),
            top.iter().take(3).map(|(k, n)| format!("`{k}` {n}")).collect::<Vec<_>>().join(", ")
        );
        let cost = if wait > 0 { fmt_dur(wait) } else { format!("{} calls", both.len()) };
        let extra = json!({ "blocked": blocked.len(), "escalated": escalated.len(), "top": top.iter().take(5).collect::<Vec<_>>() });
        out.push((
            blocked.len() as f64 * 8.0 + escalated.len() as f64 * 10.0 + wait as f64 / 1000.0,
            finding("auto_mode", "warn", format!("Auto mode: {} blocked, {} handed back to you", blocked.len(), escalated.len()), detail, &agents, sessions.len(), wait, cost, "counted", both.iter().map(|c| format!("{}@{}", c.agent, c.ts)).collect(), extra),
        ));
    }
    // Slow hooks.
    let times = hook_times(sup, w);
    let hooks = hook_stats(&times);
    for h in &hooks {
        let p95 = h["p95_ms"].as_f64().unwrap_or(0.0);
        if p95 > SLOW_HOOK_MS && h["runs"].as_u64().unwrap_or(0) >= 3 {
            let total = h["total_ms"].as_f64().unwrap_or(0.0) as i64;
            let agents: HashSet<String> = h["agents"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
            out.push((
                total as f64 / 1000.0,
                finding("slow_hook", "", format!("Slow hook: {}'s {}", h["source"].as_str().unwrap_or(""), h["event"].as_str().unwrap_or("")), format!("p50 {:.0} ms, p95 {:.1} s over {} runs; every run waits before the turn goes on.", h["p50_ms"].as_f64().unwrap_or(0.0), p95 / 1000.0, h["runs"]), &agents, agents.len(), total, fmt_dur(total), "counted", Vec::new(), h.clone()),
            ));
        }
    }
    // Unused skills, subagents and MCP servers.
    let mut used: HashMap<String, usize> = HashMap::new();
    let mut turns = 0usize;
    for evs in w.events.values() {
        for ev in evs {
            if ev.kind == "UserPromptSubmit" {
                turns += 1;
            }
            if ev.kind != "PreToolUse" {
                continue;
            }
            let tool = ev.tool.as_deref().unwrap_or("");
            if let Some(rest) = tool.strip_prefix("mcp__") {
                *used.entry(format!("mcp:{}", rest.split("__").next().unwrap_or(""))).or_default() += 1;
            }
            let d = crate::trail::data_of(ev).unwrap_or(Json::Null);
            if tool == "Skill" {
                if let Some(s) = d["tool_input"]["skill"].as_str().or(d["tool_input"]["command"].as_str()) {
                    *used.entry(format!("skill:{s}")).or_default() += 1;
                }
            }
            if tool == "Task" || tool == "Agent" {
                if let Some(s) = d["tool_input"]["subagent_type"].as_str() {
                    *used.entry(format!("subagent:{s}")).or_default() += 1;
                }
            }
        }
    }
    let all: HashSet<String> = w.agents.iter().map(|a| a.name.clone()).collect();
    if w.agents.len() >= 3 {
        for (kind, name, tokens) in skills_of(&w.repo) {
            if used.get(&format!("{kind}:{name}")).copied().unwrap_or(0) == 0 {
                let cost = tokens * turns.max(1) as i64;
                out.push((cost as f64 / 2000.0, finding("unused", "", format!("Unused {kind}: {name}"), format!("Its description costs about {tokens} tokens every turn (estimated from its text); no agent used it in {} sessions.", w.agents.len()), &all, w.agents.len(), 0, format!("{}k tok", (cost + 500) / 1000), "counted", Vec::new(), json!({ "what": kind, "name": name, "tokens_turn": tokens, "turns": turns }))));
            }
        }
        for (server, from) in servers_of(sup, w) {
            if matches!(server.as_str(), "workbench" | "chloectx") {
                continue;
            }
            if used.get(&format!("mcp:{server}")).copied().unwrap_or(0) == 0 {
                out.push((5.0, finding("unused", "", format!("Unused MCP server: {server}"), format!("From {from}; no agent called its tools in {} sessions. Its tool definitions are in every turn (their tokens are not recorded).", w.agents.len()), &all, w.agents.len(), 0, "every turn".into(), "counted", Vec::new(), json!({ "what": "mcp", "name": server, "from": from }))));
            }
        }
    }
    // Redo loops: the same test failing the same way 3 times in a session.
    let p = Params::new().set("repo", sv(&w.repo)).set("since", ts(w.from));
    let tr = sup.db().query("SELECT agent, test, passed, error, ts FROM test_runs WHERE repo = :repo AND ts >= :since ORDER BY ts ASC LIMIT 50000", &p).unwrap_or_default();
    let name_of: HashMap<&str, &str> = w.agents.iter().map(|a| (a.id.as_str(), a.name.as_str())).collect();
    let mut loops: BTreeMap<(String, String, String), Vec<i64>> = BTreeMap::new();
    let mut tests: Vec<(String, bool, i64)> = Vec::new();
    for r in &tr {
        let (Some(agent), Some(test)) = (get_str(&r[0]), get_str(&r[1])) else { continue };
        let passed = get_int(&r[2]).unwrap_or(0) != 0;
        let at = get_ms(&r[4]).unwrap_or(0);
        tests.push((agent.clone(), passed, at));
        if !passed {
            let key = crate::flow::error_key(&get_str(&r[3]).unwrap_or_default());
            loops.entry((agent, test, key)).or_default().push(at);
        }
    }
    for ((agent, test, key), at) in &loops {
        if at.len() >= 3 {
            let cost = at.last().unwrap() - at.first().unwrap();
            let n = name_of.get(agent.as_str()).copied().unwrap_or(agent);
            let agents: HashSet<String> = [n.to_string()].into_iter().collect();
            out.push((cost as f64 / 1000.0, finding("redo", "warn", format!("Redo loop: {n} ran {test} {} times", at.len()), format!("{n} ran `{test}` {} times on one error{}.", at.len(), if key.is_empty() { String::new() } else { format!(": {}", one_line(key, 120)) }), &agents, 1, cost, fmt_dur(cost), "counted", Vec::new(), json!({ "test": test, "runs": at.len() }))));
        }
    }
    // Your corrections: the same words typed to 2 or more agents, and the
    // files Hotspots says you corrected agents on.
    let mut said: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    for a in &w.agents {
        for ev in w.events.get(&a.id).into_iter().flatten().filter(|e| e.kind == "input") {
            if crate::trail::data_of(ev).is_some_and(|d| d.get("by").is_some()) {
                continue;
            }
            let text = ev.summary.as_deref().unwrap_or("").trim_start_matches("input: ").trim().to_lowercase();
            let words = text.split_whitespace().count();
            if words >= 3 && text != "typed in the terminal" {
                said.entry(text.split_whitespace().collect::<Vec<_>>().join(" ")).or_default().insert(a.name.clone());
            }
        }
    }
    for (text, agents) in said.iter().filter(|(_, a)| a.len() >= 2) {
        out.push((agents.len() as f64 * 30.0, finding("corrected", "", format!("Corrected by you: “{}”", one_line(text, 80)), format!("You said it to {} agents.", agents.len()), agents, agents.len(), 0, format!("{} times", agents.len()), "counted", Vec::new(), json!({ "text": text }))));
    }
    let hs = sup.db().query("SELECT file, steers, re_edits FROM hotspots WHERE repo = :repo AND steers >= 2 ORDER BY steers DESC LIMIT 5", &Params::new().set("repo", sv(&w.repo))).unwrap_or_default();
    for r in hs {
        let (Some(file), Some(n)) = (get_str(&r[0]), get_int(&r[1])) else { continue };
        out.push((n as f64 * 20.0, finding("corrected", "", format!("Corrected by you on {file}"), format!("You steered agents {n} times about `{file}` (Hotspots, 14 days)."), &HashSet::new(), 0, 0, format!("{n} times"), "counted", Vec::new(), json!({ "file": file, "hotspot": true }))));
    }
    // Missed memory, from ChloeCtx's receipts.
    let (mut misses, mut rcs, mut missing) = (Vec::new(), Vec::new(), Vec::new());
    if with_memory && sup.cfg.chloectx_required {
        let (r, m) = memory::receipts(sup, &w.agents, w.from);
        rcs = r;
        missing = m;
        let p = Params::new().set("repo", sv(&w.repo)).set("since", ts(w.from));
        let edits: Vec<(String, String, i64)> = sup.db().query("SELECT agent, file, ts FROM file_edits WHERE repo = :repo AND ts >= :since LIMIT 50000", &p).unwrap_or_default().iter().filter_map(|r| Some((get_str(&r[0])?, get_str(&r[1])?, get_ms(&r[2])?))).collect();
        let last: HashMap<String, i64> = w.events.iter().filter_map(|(k, v)| v.last().map(|e| (k.clone(), e.ts))).collect();
        misses = memory::misses(&rcs, &edits, &tests, &last);
        memory::confirm(sup, Path::new(&w.cwd()), &mut misses);
        for m in &misses {
            let agents: HashSet<String> = [m.name.clone()].into_iter().collect();
            out.push((
                m.cost_ms as f64 / 1000.0 + 60.0,
                finding("missed_memory", "bad", format!("Missed memory: {}", one_line(&m.text, 60)), format!("“{}” scored {:.2} and was left out ({}) at {}'s turn {}; {} edited {} and {} test run{} failed after it.", one_line(&m.text, 120), m.score, m.decision, m.name, m.turn, m.name, m.file, m.failed, if m.failed == 1 { "" } else { "s" }), &agents, 1, m.cost_ms, fmt_dur(m.cost_ms), &m.level, vec![m.receipt.clone()], serde_json::to_value(m).unwrap_or_default()),
            ));
        }
    }
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let timing = w.agents.iter().any(|a| sup.session_dir(&a.id).join("hook-times.log").exists());
    Found { findings: out.into_iter().map(|x| x.1).collect(), misses, rcs, hooks, timing, missing, storms: storm_list }
}

/// Writes the findings to `harness_findings`.
fn save_findings(sup: &Supervisor, repo: &str, fs: &[Json], version: &str, from: i64, to: i64) {
    let _ = sup.db().query("DELETE FROM harness_findings WHERE repo = :repo", &Params::new().set("repo", sv(repo)));
    for f in fs {
        let p = Params::new()
            .set("id", sv(f["id"].as_str().unwrap_or("")))
            .set("repo", sv(repo))
            .set("kind", sv(f["kind"].as_str().unwrap_or("")))
            .set("title", sv(f["title"].as_str().unwrap_or("")))
            .set("detail", sv(f["detail"].as_str().unwrap_or("")))
            .set("sessions", Value::Str(f["sessions"].to_string()))
            .set("agents", Value::Str(f["agents"].to_string()))
            .set("cost_s", Value::Float(f["cost_ms"].as_f64().unwrap_or(0.0) / 1000.0))
            .set("level", sv(f["level"].as_str().unwrap_or("counted")))
            .set("row_ids", Value::Str(f["rows"].to_string()))
            .set("version", sv(version))
            .set("state", sv("open"))
            .set("first_ts", ts(from))
            .set("last_ts", ts(to));
        let _ = sup.db().query("INSERT OR REPLACE INTO harness_findings (id, repo, kind, title, detail, sessions, agents, cost_s, level, row_ids, version, state, first_ts, last_ts) VALUES (:id, :repo, :kind, :title, :detail, :sessions, :agents, :cost_s, :level, :row_ids, :version, :state, :first_ts, :last_ts)", &p);
    }
}

// --------------------------------------------------------------- scorecard

fn group_of(a: &Agent, running: bool) -> &'static str {
    match a.state.as_str() {
        "waiting" => "needs",
        "working" | "starting" => "working",
        "idle" if running => "ready",
        "exited" if a.exit_code.unwrap_or(0) == 0 => "finished",
        "exited" => "failed",
        _ => "ready",
    }
}

/// The week's scorecard (HARNESS.md, "The scorecard"), from rows.
fn scorecard(sup: &Supervisor, w: &Win, misses: usize, lost_ms: i64) -> Json {
    let n = w.agents.len();
    // Done by checks: the last run of each test an agent ran passed.
    let p = Params::new().set("repo", sv(&w.repo)).set("since", ts(w.from)).set("to", ts(w.to));
    let tr = sup.db().query("SELECT agent, test, passed, ts FROM test_runs WHERE repo = :repo AND ts >= :since AND ts < :to ORDER BY ts ASC LIMIT 50000", &p).unwrap_or_default();
    let mut last: HashMap<(String, String), bool> = HashMap::new();
    for r in &tr {
        if let (Some(a), Some(t)) = (get_str(&r[0]), get_str(&r[1])) {
            last.insert((a, t), get_int(&r[2]).unwrap_or(0) != 0);
        }
    }
    let ran: HashSet<&String> = last.keys().map(|(a, _)| a).collect();
    let done = ran.iter().filter(|a| last.iter().filter(|((x, _), _)| x == **a).all(|(_, p)| *p)).count();
    // Prompts and interventions.
    let prompts = w.calls.iter().filter(|c| c.prompted).count();
    let mut by_cmd: HashMap<String, usize> = HashMap::new();
    for c in w.calls.iter().filter(|c| c.prompted) {
        if let Some(k) = c.probe.command.as_deref().and_then(storm_key) {
            *by_cmd.entry(k).or_default() += 1;
        }
    }
    let top = by_cmd.iter().max_by_key(|(_, n)| **n).map(|(k, n)| (k.clone(), *n));
    let mut typed = 0;
    for evs in w.events.values() {
        let inputs = evs.iter().filter(|e| e.kind == "input" && !crate::trail::data_of(e).is_some_and(|d| d.get("by").is_some())).count();
        typed += inputs;
    }
    let asks = sup.db().query("SELECT agent FROM questions WHERE repo = :repo AND kind = 'ask' AND created >= :since AND created < :to LIMIT 10000", &p).unwrap_or_default().len();
    let interventions = prompts + typed + asks;
    // Wall time: each session's span, the median.
    let mut spans: Vec<f64> = w
        .agents
        .iter()
        .filter_map(|a| {
            let end = a.ended.or_else(|| w.events.get(&a.id).and_then(|e| e.last()).map(|e| e.ts))?;
            Some((end - a.created).max(0) as f64)
        })
        .collect();
    let median = pct(&mut spans, 0.5) as i64;
    // Tokens and cost.
    let tk = sup.db().query("SELECT agent, input, output, cache_read FROM token_use WHERE repo = :repo AND ts >= :since AND ts < :to LIMIT 100000", &p).unwrap_or_default();
    let model_of: HashMap<&str, Option<&str>> = w.agents.iter().map(|a| (a.id.as_str(), a.model.as_deref())).collect();
    let (mut tokens, mut cost) = (0i64, 0f64);
    for r in &tk {
        let a = get_str(&r[0]).unwrap_or_default();
        let (i, o, c) = (get_int(&r[1]).unwrap_or(0), get_int(&r[2]).unwrap_or(0), get_int(&r[3]).unwrap_or(0));
        tokens += i + o;
        cost += crate::routing::cost_usd(&crate::routing::model_of(model_of.get(a.as_str()).copied().flatten()), i, o, c);
    }
    json!({
        "sessions": n,
        "done": done, "ran_tests": ran.len(),
        "interventions": interventions, "per_session": if n > 0 { interventions as f64 / n as f64 } else { 0.0 },
        "typed": typed, "asks": asks,
        "prompts": prompts, "top_command": top.as_ref().map(|t| t.0.clone()), "top_count": top.map_or(0, |t| t.1),
        "wall_ms": median, "lost_ms": lost_ms,
        "tokens": tokens, "cost_usd": (cost * 100.0).round() / 100.0,
        "memory_missed": misses,
        "modes": modes_of(w),
        "blocked": w.calls.iter().filter(|c| c.outcome == "blocked").count(),
        "escalated": w.calls.iter().filter(|c| c.prompted && c.mode == "auto").count(),
    })
}

/// Sessions per permission mode.
fn modes_of(w: &Win) -> Json {
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    for a in &w.agents {
        *by.entry(w.modes.get(&a.id).map_or("unknown", |m| m.as_str())).or_default() += 1;
    }
    json!(by)
}

/// Each session that ran tests: whether the last run of each passed.
fn done_by_agent(sup: &Supervisor, w: &Win) -> HashMap<String, bool> {
    let p = Params::new().set("repo", sv(&w.repo)).set("since", ts(w.from)).set("to", ts(w.to));
    let tr = sup.db().query("SELECT agent, test, passed FROM test_runs WHERE repo = :repo AND ts >= :since AND ts < :to ORDER BY ts ASC LIMIT 50000", &p).unwrap_or_default();
    let mut last: HashMap<(String, String), bool> = HashMap::new();
    for r in &tr {
        if let (Some(a), Some(t)) = (get_str(&r[0]), get_str(&r[1])) {
            last.insert((a, t), get_int(&r[2]).unwrap_or(0) != 0);
        }
    }
    let mut out: HashMap<String, bool> = HashMap::new();
    for ((a, _), passed) in last {
        let e = out.entry(a).or_insert(true);
        *e = *e && passed;
    }
    out
}

/// The current version's sessions against the version before it, on what
/// rows count: done by checks and prompts per session. "No detectable
/// difference" is a result, with the size the counts could have shown.
fn compare_versions(sup: &Supervisor, w: &Win, all: &[Version], cur: &Version) -> Json {
    let cfg_of: HashMap<&str, &str> = all.iter().filter(|v| v.kind == "run").filter_map(|v| Some((v.id.as_str(), v.config.as_deref()?))).collect();
    let mut by_cfg: HashMap<String, Vec<&Agent>> = HashMap::new();
    for a in &w.agents {
        if let Some(c) = a.harness.as_deref().and_then(|h| cfg_of.get(h)) {
            by_cfg.entry(c.to_string()).or_default().push(a);
        }
    }
    let hist = history(sup, &w.repo);
    let prev = hist.iter().rev().filter(|id| **id != cur.id).find(|id| by_cfg.contains_key(id.as_str())).and_then(|id| all.iter().find(|v| v.id == *id));
    let (Some(prev), Some(b)) = (prev, by_cfg.get(&cur.id)) else {
        return json!({ "result": "none", "note": format!("Only one version has sessions in the window ({} of them): nothing to compare yet.", w.agents.len()) });
    };
    let a = &by_cfg[&prev.id];
    let done = done_by_agent(sup, w);
    let share = |list: &[&Agent]| {
        let ran: Vec<bool> = list.iter().filter_map(|x| done.get(&x.id).copied()).collect();
        (ran.iter().filter(|d| **d).count(), ran.len())
    };
    let prompts = |list: &[&Agent]| w.calls.iter().filter(|c| c.prompted && list.iter().any(|x| x.id == c.agent)).count();
    let (ak, an) = share(a);
    let (bk, bn) = share(b);
    let d = stats::shares(ak, an, bk, bn);
    let r = stats::rates(prompts(a), a.len(), prompts(b), b.len());
    json!({
        "a": { "id": prev.id, "name": prev.name, "sessions": a.len() },
        "b": { "id": cur.id, "name": cur.name, "sessions": b.len() },
        "done": d, "prompts": r,
        "sentences": [stats::sentence(&d, "done by checks", &prev.name, &cur.name), stats::sentence(&r, "prompts per session", &prev.name, &cur.name)],
        "level": "counted",
    })
}

/// The suggestions made by rule from the findings (HARNESS.md, "2.
/// Propose", rules only): each with its diff, why, impact and level.
fn suggestions(sup: &Supervisor, w: &Win, cur_v: &Version, cur_e: &Edits, found: &Found) -> Vec<Json> {
    let mut out = Vec::new();
    let rules = rules_under(sup, &w.repo, cur_v, cur_e);
    for (key, n) in found.storms.iter().take(3) {
        let rule = format!("Bash({key}:*)");
        let mut e = cur_e.permissions.clone();
        e.allow.push(rule.clone());
        let after = perms::effective(&baseline(sup, &w.repo), &e, "draft");
        let imp = perms::impact(&rules, &after, &w.calls);
        let wid = perms::widening(&rules, &after, &w.calls, &w.cwd());
        out.push(json!({
            "id": hash(&format!("allow\n{rule}")), "part": "permissions", "title": format!("Allow `{key}` as agents type it"),
            "diff": [["+", format!("allow  {rule}")]],
            "why": format!("{n} prompts for it in the window; you allowed {}.", w.calls.iter().filter(|c| c.prompted && c.outcome == "ran" && c.probe.command.as_deref().and_then(storm_key).as_deref() == Some(key.as_str())).count()),
            "impact": format!("−{} of {} prompts · {} denies changed", imp["saved"], imp["prompts_before"], imp["denies_changed"]),
            "level": "counted", "widens": !wid.is_empty(),
            "edit": { "permissions": { "allow": [rule] } },
            "alt": "Or make agents need no prefix (a change to the repository, through Review).",
        }));
    }
    for m in found.misses.iter().take(2) {
        let pin = Pin { file: m.file.clone(), record: m.record.clone(), text: m.text.clone() };
        let mut d = cur_e.memory.clone();
        d.pins.push(pin.clone());
        let rc = memory::recompute(&found.rcs, &cur_e.memory, &d, &found.misses);
        out.push(json!({
            "id": hash(&format!("pin\n{}", m.record)), "part": "memory", "title": format!("Pin “{}” to {}", one_line(&m.text, 50), m.file),
            "diff": [["+", format!("pin    {} → “{}”", m.file, one_line(&m.text, 60))]],
            "why": format!("{}'s turn {} left it out at {:.2}; {} test run{} failed after.", m.name, m.turn, m.score, m.failed, if m.failed == 1 { "" } else { "s" }),
            "impact": format!("+{} records in {} sessions · {} misses fixed", rc["added"], rc["sessions_changed"].as_array().map_or(0, |a| a.len()), rc["misses_fixed"].as_array().map_or(0, |a| a.len())),
            "level": "recomputed", "widens": false,
            "edit": { "memory": { "pins": [pin] } },
        }));
    }
    out
}

// ------------------------------------------------------------------- API

#[derive(Deserialize)]
struct RepoQ {
    repo: Option<String>,
    days: Option<i64>,
    version: Option<String>,
    receipt: Option<String>,
    kind: Option<String>,
    q: Option<String>,
}

fn overview(sup: &Supervisor, q: RepoQ) -> Result<Json, ApiError> {
    let repo = repo_or_default(sup, q.repo)?;
    let days = q.days.unwrap_or(WINDOW_DAYS).clamp(1, 90);
    let (from, to) = window_of(Some(days));
    let cur = current(sup, &repo);
    let cur_e = edits_of(sup, &cur.id);
    let w = Win::load(sup, &repo, from, to);
    let rules = rules_under(sup, &repo, &cur, &cur_e);
    let found = find(sup, &w, &rules, true);
    save_findings(sup, &repo, &found.findings, &cur.id, from, to);
    let lost: i64 = found.findings.iter().filter_map(|f| f["cost_ms"].as_i64()).sum();
    let mut card = scorecard(sup, &w, found.misses.len(), lost);
    // Last week, for the deltas.
    let prev = Win::load(sup, &repo, from - days * 86_400_000, from);
    let prev_card = scorecard(sup, &prev, 0, 0);
    card["prev"] = json!({ "sessions": prev_card["sessions"], "prompts": prev_card["prompts"], "done": prev_card["done"], "ran_tests": prev_card["ran_tests"], "per_session": prev_card["per_session"], "tokens": prev_card["tokens"] });
    // Agents by the version they ran.
    let all = versions(sup, &repo);
    let by_id: HashMap<&str, &Version> = all.iter().map(|v| (v.id.as_str(), v)).collect();
    let mut groups: BTreeMap<String, Vec<Json>> = BTreeMap::new();
    let running: HashSet<String> = sup.list(false).iter().filter_map(|a| a["id"].as_str().map(str::to_string)).collect();
    let mut agents = w.agents.clone();
    agents.sort_by_key(|a| std::cmp::Reverse(a.created));
    for a in &agents {
        let key = a.harness.clone().unwrap_or_default();
        groups.entry(key).or_default().push(json!({ "id": a.id, "name": a.name, "state": a.state, "group": group_of(a, running.contains(&a.id)), "created": iso(a.created), "created_ms": a.created, "mode": w.modes.get(&a.id).cloned().unwrap_or_else(|| "unknown".into()) }));
    }
    let mut by_version: Vec<Json> = groups
        .into_iter()
        .map(|(k, list)| {
            let v = by_id.get(k.as_str());
            let cfg = v.and_then(|v| v.config.as_deref()).and_then(|c| by_id.get(c));
            let older = cfg.is_some_and(|c| c.id != cur.id);
            let newest = list.iter().filter_map(|a| a["created_ms"].as_i64()).max().unwrap_or(0);
            json!({
                "version": k, "short": short(&k), "name": v.map(|v| v.name.clone()).unwrap_or_else(|| "–".into()),
                "config": cfg.map(|c| c.id.clone()), "current": !older && v.is_some(), "older": older, "recorded": v.is_some(),
                "note": if v.is_none() { Some("started before versions were recorded".to_string()) } else if older { Some(format!("started before {}", cur.name)) } else { None },
                "claude": v.and_then(|v| v.claude_version.clone()), "agents": list, "newest": newest,
            })
        })
        .collect();
    by_version.sort_by_key(|g| std::cmp::Reverse(g["newest"].as_i64().unwrap_or(0)));
    let compare = compare_versions(sup, &w, &all, &cur);
    let sugg = suggestions(sup, &w, &cur, &cur_e, &found);
    let drafts: Vec<Json> = all.iter().filter(|v| v.kind == "config" && v.state == "draft").map(Version::to_json).collect();
    let names: Vec<String> = {
        let mut s: Vec<String> = all.iter().filter(|v| v.kind == "run" && w.agents.iter().any(|a| a.harness.as_deref() == Some(v.id.as_str()))).map(|v| v.name.clone()).collect();
        s.sort();
        s.dedup();
        s
    };
    Ok(json!({
        "repo": repo,
        "window": { "days": days, "from": iso(from), "to": iso(to), "versions": names },
        "current": cur.to_json(),
        "drafts": drafts,
        "scorecard": card,
        "findings": found.findings,
        "hooks": found.hooks,
        "timing": { "on": cur_e.timing, "recorded": found.timing },
        "by_version": by_version,
        "compare": compare,
        "suggestions": sugg,
        "mode": "suggest",
        "missing": found.missing,
    }))
}

/// The version a request edits: the one named, else the newest draft,
/// else the current.
fn edited(sup: &Supervisor, repo: &str, id: Option<&str>) -> Result<(Version, Edits), ApiError> {
    let cur = current(sup, repo);
    let v = match id.filter(|s| !s.is_empty()) {
        Some(id) => version(sup, id).filter(|v| v.kind == "config" && v.repo == repo).ok_or_else(|| ApiError::not_found(format!("no version {id} of {repo}")))?,
        None => versions(sup, repo).into_iter().rev().find(|v| v.kind == "config" && v.state == "draft").unwrap_or(cur),
    };
    let e = edits_of(sup, &v.id);
    Ok((v, e))
}

fn permissions_view(sup: &Supervisor, q: RepoQ) -> Result<Json, ApiError> {
    let repo = repo_or_default(sup, q.repo)?;
    let (from, to) = window_of(q.days);
    let cur = current(sup, &repo);
    let cur_e = edits_of(sup, &cur.id);
    let (v, e) = edited(sup, &repo, q.version.as_deref())?;
    let w = Win::load(sup, &repo, from, to);
    let base_rules = rules_under(sup, &repo, &cur, &cur_e);
    let rules = rules_under(sup, &repo, &v, &e);
    let cwd = w.cwd();
    Ok(json!({
        "repo": repo, "version": v.to_json(), "current": cur.to_json(),
        "baseline": baseline(sup, &repo),
        "edits": e.permissions,
        "rules": perms::rule_rows(&rules, &w.calls),
        "block": serde_json::to_string_pretty(&json!({ "permissions": perms::block(&rules) })).unwrap_or_default(),
        "lint": lint_all(&rules, &base_rules, &w.calls, &cwd, &serde_json::to_string(&perms::block(&rules)).unwrap_or_default(), sup, &w),
        "impact": with_result(perms::impact(&base_rules, &rules, &w.calls), w.calls.len()),
        "widens": widen_report(&base_rules, &rules, &w.calls, &cwd),
        "calls": w.calls.len(),
        "prompts": w.calls.iter().filter(|c| c.prompted).count(),
        "window": { "from": iso(from), "to": iso(to) },
    }))
}

/// The lint of the rules and the secrets of the text, and of the
/// environment Workbench passes every agent.
fn lint_all(rules: &[Rule], current: &[Rule], calls: &[perms::Call], cwd: &str, text: &str, sup: &Supervisor, w: &Win) -> Vec<Json> {
    let mut out = perms::lint(rules, current, calls, cwd);
    out.extend(perms::secrets(text));
    // The environment the newest session got (from Workbench and ChloeCtx).
    if let Some(a) = w.agents.last() {
        if let Ok(t) = std::fs::read_to_string(sup.session_dir(&a.id).join("settings.json")) {
            if let Ok(v) = serde_json::from_str::<Json>(&t) {
                let env = serde_json::to_string_pretty(&v["env"]).unwrap_or_default();
                for mut s in perms::secrets(&env) {
                    s["message"] = json!(format!("In the environment every session gets: {}", s["message"].as_str().unwrap_or("")));
                    s["range"] = Json::Null;
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Whether `after` widens `before`, and the patterns a promote must name.
fn widen_report(before: &[Rule], after: &[Rule], calls: &[perms::Call], cwd: &str) -> Json {
    let items = perms::widening(before, after, calls, cwd);
    let added: Vec<&Rule> = after.iter().filter(|r| r.decision == "allow" && !before.iter().any(|b| b.decision == "allow" && b.text == r.text)).collect();
    let removed: Vec<&Rule> = before.iter().filter(|r| r.decision != "allow" && !after.iter().any(|a| a.decision == r.decision && a.text == r.text)).collect();
    let mut patterns: Vec<String> = Vec::new();
    for it in &items {
        let what = it["what"].as_str().unwrap_or("");
        let mut hit = false;
        for r in &added {
            let in_rule = what == format!("rule {}", r.text);
            let by_call = calls.iter().any(|c| perms::hits(&r.text, &c.probe) && c.probe.command.as_deref().or(c.probe.path.as_deref()).is_some_and(|x| what.ends_with(x)));
            if in_rule || by_call {
                hit = true;
                if !patterns.contains(&r.text) {
                    patterns.push(r.text.clone());
                }
            }
        }
        for r in &removed {
            if calls.iter().any(|c| perms::hits(&r.text, &c.probe) && c.probe.command.as_deref().or(c.probe.path.as_deref()).is_some_and(|x| what.ends_with(x))) {
                hit = true;
                let t = format!("remove {} {}", r.decision, r.text);
                if !patterns.contains(&t) {
                    patterns.push(t);
                }
            }
        }
        if !hit {
            // Widened by the rules together: every new allow is named.
            for r in &added {
                if !patterns.contains(&r.text) {
                    patterns.push(r.text.clone());
                }
            }
        }
    }
    for r in &removed {
        let t = format!("remove {} {}", r.decision, r.text);
        if !patterns.contains(&t) {
            patterns.push(t);
        }
    }
    json!({ "widens": !patterns.is_empty(), "patterns": patterns, "items": items.into_iter().take(50).collect::<Vec<_>>() })
}

#[derive(Deserialize, Default)]
struct PermB {
    repo: Option<String>,
    base: Option<String>,
    kind: Option<String>,
    text: Option<String>,
    edits: Option<PermEdits>,
    window_days: Option<i64>,
}

/// The edits a body gives: `edits`, or `text` (a `permissions` block or a
/// whole settings file) parsed against Workbench's rules.
fn perm_edits_of(sup: &Supervisor, repo: &str, b: &PermB) -> Result<PermEdits, ApiError> {
    if let Some(e) = &b.edits {
        return Ok(e.clone());
    }
    let text = b.text.as_deref().ok_or_else(|| ApiError::bad("give edits or text"))?;
    let v: Json = serde_json::from_str(text).map_err(|e| ApiError::new(400, "parse", format!("line {}, column {}: {e}", e.line(), e.column())))?;
    let block = if v.get("permissions").is_some() { v["permissions"].clone() } else { v };
    perms::edits_of_block(&baseline(sup, repo), &block).map_err(|e| ApiError::new(400, "parse", e))
}

fn lint_route_body(sup: &Supervisor, b: PermB) -> Result<Json, ApiError> {
    let repo = repo_or_default(sup, b.repo.clone())?;
    match b.kind.as_deref().unwrap_or("permissions") {
        "permissions" => {
            let (from, to) = window_of(b.window_days);
            let w = Win::load(sup, &repo, from, to);
            let cur = current(sup, &repo);
            let cur_e = edits_of(sup, &cur.id);
            let e = match perm_edits_of(sup, &repo, &b) {
                Ok(e) => e,
                Err(err) if err.code == "parse" => return Ok(json!({ "ok": false, "error": err.message, "flags": [] })),
                Err(err) => return Err(err),
            };
            let rules = perms::effective(&baseline(sup, &repo), &e, "draft");
            let text = b.text.clone().unwrap_or_else(|| serde_json::to_string(&perms::block(&rules)).unwrap_or_default());
            Ok(json!({ "ok": true, "edits": e, "flags": lint_all(&rules, &rules_under(sup, &repo, &cur, &cur_e), &w.calls, &w.cwd(), &text, sup, &w), "rules": perms::rule_rows(&rules, &w.calls) }))
        }
        "memory" => Ok(json!({ "ok": true, "flags": perms::secrets(b.text.as_deref().unwrap_or("")) })),
        k => Err(ApiError::bad(format!("no lint for the part {k}: permissions or memory"))),
    }
}

fn impact_body(sup: &Supervisor, b: PermB) -> Result<Json, ApiError> {
    let repo = repo_or_default(sup, b.repo.clone())?;
    if !matches!(b.kind.as_deref().unwrap_or("permissions"), "permissions") {
        return Ok(json!({ "level": "needs_replay", "why": "only permissions are counted here; memory's impact is at /api/harness/memory/preview" }));
    }
    let (from, to) = window_of(b.window_days);
    let w = Win::load(sup, &repo, from, to);
    let base_v = match b.base.as_deref() {
        Some(id) => version(sup, id).ok_or_else(|| ApiError::not_found(format!("no version {id}")))?,
        None => current(sup, &repo),
    };
    let base_e = edits_of(sup, &base_v.id);
    let before = rules_under(sup, &repo, &base_v, &base_e);
    let e = perm_edits_of(sup, &repo, &b)?;
    let after = perms::effective(&baseline(sup, &repo), &e, "draft");
    let mut imp = with_result(perms::impact(&before, &after, &w.calls), w.calls.len());
    imp["widens"] = widen_report(&before, &after, &w.calls, &w.cwd());
    imp["window"] = json!({ "from": iso(from), "to": iso(to) });
    Ok(imp)
}

/// An impact with its result: "no difference" is a result, said with what
/// the count could have shown.
fn with_result(mut imp: Json, calls: usize) -> Json {
    let same = imp["saved"] == 0 && imp["added"] == 0 && imp["denies_changed"] == 0;
    imp["result"] = json!(if same { "no_difference" } else { "changed" });
    imp["cases"] = json!(calls);
    imp["note"] = json!(format!("Exact over the {calls} calls the window recorded: one call decided differently would show. A command no agent ran in the window cannot show."));
    imp
}

fn complete_view(sup: &Supervisor, q: RepoQ) -> Result<Json, ApiError> {
    let repo = repo_or_default(sup, q.repo)?;
    let (from, to) = window_of(q.days);
    let w = Win::load(sup, &repo, from, to);
    let cur = current(sup, &repo);
    let rules = rules_under(sup, &repo, &cur, &edits_of(sup, &cur.id));
    match q.kind.as_deref().unwrap_or("command") {
        "command" => Ok(json!({ "items": perms::complete(&w.calls, &rules, q.q.as_deref().unwrap_or("")) })),
        "tool" => {
            let mut by: BTreeMap<String, usize> = BTreeMap::new();
            for c in &w.calls {
                *by.entry(c.probe.tool.clone()).or_default() += 1;
            }
            let ql = q.q.unwrap_or_default().to_lowercase();
            Ok(json!({ "items": by.into_iter().filter(|(t, _)| t.to_lowercase().starts_with(&ql)).map(|(t, n)| json!({ "insert": t, "label": t, "runs": n, "source": "called here" })).collect::<Vec<_>>() }))
        }
        k => Err(ApiError::bad(format!("no completions of kind {k}: command or tool"))),
    }
}

fn memory_view(sup: &Supervisor, q: RepoQ, draft: Option<MemEdits>) -> Result<Json, ApiError> {
    let repo = repo_or_default(sup, q.repo)?;
    let (from, to) = window_of(q.days);
    let cur = current(sup, &repo);
    let cur_e = edits_of(sup, &cur.id);
    let (v, e) = edited(sup, &repo, q.version.as_deref())?;
    let d = draft.unwrap_or(e.memory.clone());
    let w = Win::load(sup, &repo, from, to);
    let rules = rules_under(sup, &repo, &cur, &cur_e);
    let found = find(sup, &w, &rules, true);
    let pick = q.receipt.as_deref().and_then(|id| found.rcs.iter().find(|r| r.id == id)).or_else(|| found.misses.first().and_then(|m| found.rcs.iter().find(|r| r.id == m.receipt))).or_else(|| found.rcs.iter().rev().find(|r| r.full)).or_else(|| found.rcs.last());
    let turns: Vec<Json> = found.rcs.iter().rev().take(60).map(|r| json!({ "receipt": r.id, "name": r.name, "turn": r.turn, "ts": iso(r.ts), "query": one_line(&r.query, 80), "full": r.full, "missed": found.misses.iter().any(|m| m.receipt == r.id) })).collect();
    let records: Vec<Json> = {
        let mut seen = HashSet::new();
        found.rcs.iter().flat_map(|r| r.rows.iter()).filter(|r| matches!(r.kind.as_str(), "decision" | "preference" | "rule" | "fact") && seen.insert(r.id.clone())).take(200).map(|r| json!({ "id": r.id, "kind": r.kind, "text": r.text, "files": memory::files_named(&r.text) })).collect()
    };
    let mut recompute = memory::recompute(&found.rcs, &cur_e.memory, &d, &found.misses);
    let same = recompute["changed"] == 0 && recompute["added"] == 0 && recompute["dropped"] == 0;
    recompute["result"] = json!(if found.rcs.is_empty() { "none" } else if same { "no_difference" } else { "changed" });
    Ok(json!({
        "repo": repo, "version": v.to_json(), "current": cur.to_json(),
        "observed": memory::observed(&found.rcs),
        "base": cur_e.memory, "edits": d,
        "preview": pick.map(|r| memory::preview(r, &cur_e.memory, &d)),
        "recompute": recompute,
        "misses": found.misses,
        "turns": turns,
        "records": records,
        "lint": perms::secrets(&serde_json::to_string_pretty(&d).unwrap_or_default()),
        "applied": "Pins reach new agents of the version at their session start (a SessionStart hook Workbench writes into the session's own settings). The knowledge threshold is set at Promote through Debug's threshold (the project's ChloeCtx store: every agent, from its next prompt). The gate and the budgets are passed to new sessions as CHLOECTX_RECALL and recomputed here; ChloeCtx reads them from its store's config.json, so they are not yet applied per session.",
        "missing": found.missing,
        "window": { "from": iso(from), "to": iso(to) },
    }))
}

#[derive(Deserialize)]
struct SeesQ {
    agent: String,
    /// The end of the turn (Debug's `ended_ms`): what was given before it.
    until_ms: Option<i64>,
}

/// A part of what the model saw.
fn sees_part(part: &str, source: String, tokens: Option<i64>, level: &str, text: Option<String>) -> Json {
    json!({ "part": part, "source": source, "tokens": tokens, "level": level, "text": text.map(|t| if t.len() > 6000 { let mut e = 6000; while !t.is_char_boundary(e) { e -= 1; } format!("{}…", &t[..e]) } else { t }) })
}

/// What the harness put in an agent's context by a turn, and what Claude
/// Code loads by itself (HARNESS.md, "What the model sees"). The turn
/// itself (its prompt, its size from the transcript's usage, what ChloeCtx
/// gave and left out) is Debug's, `GET /api/agents/:id/turns/:n/context`
/// (src/turns.rs); this adds what Workbench injected (`injected`: its own
/// record of giving it), what is `estimated` (CLAUDE.md, skills, MCP
/// servers), the harness version, and whether Claude Code's raw request
/// bodies are exported. Workbench never turns that export on.
fn sees_view(sup: &Supervisor, q: SeesQ) -> Result<Json, ApiError> {
    let a = sup.agent_rows().into_iter().find(|a| a.id == q.agent).ok_or_else(|| ApiError::not_found(format!("no agent {}", q.agent)))?;
    let next = q.until_ms.unwrap_or(i64::MAX);
    let est = |t: &str| (t.chars().count() as i64 + 3) / 4;
    let mut parts: Vec<Json> = Vec::new();
    // Injected by Workbench: the rules and steers it gave by this turn.
    let given = sup.db().query("SELECT item, via, ts FROM deliveries WHERE agent = :agent LIMIT 10000", &Params::new().set("agent", sv(&a.id))).unwrap_or_default();
    let rules = crate::questions::rules(sup, Some(&a.repo), false);
    let mut told: Vec<String> = Vec::new();
    for r in &given {
        let (Some(item), Some(t)) = (get_str(&r[0]), get_ms(&r[2])) else { continue };
        if t > next {
            continue;
        }
        if let Some(rule) = rules.iter().find(|x| x.id == item) {
            told.push(format!("- {}", one_line(&rule.text, 600)));
        } else if item.starts_with("assign:") {
            told.push("- the outcome it works on".into());
        }
    }
    for st in crate::levels::steers(sup, &a.id) {
        if given.iter().any(|r| get_str(&r[0]).as_deref() == Some(st.id.as_str()) && get_ms(&r[2]).is_some_and(|t| t <= next)) {
            told.push(format!("The user adds: {}", st.text.trim()));
        }
    }
    if !told.is_empty() {
        let t = told.join("\n");
        parts.push(sees_part("Rules and steers from Workbench", format!("{} item{} · hook additionalContext", told.len(), if told.len() == 1 { "" } else { "s" }), Some(est(&t)), "injected", Some(t)));
    }
    if let Ok(t) = std::fs::read_to_string(sup.session_dir(&a.id).join("harness-pins.md")) {
        parts.push(sees_part("Pins of this harness", "SessionStart · harness-pins.md".into(), Some(est(&t)), "injected", Some(t)));
    }
    // Estimated: what Claude Code loads by itself.
    let cwd = Path::new(&a.cwd);
    let mut md: Vec<(String, String)> = Vec::new();
    for f in ["CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md"] {
        if let Ok(t) = std::fs::read_to_string(cwd.join(f)) {
            md.push((f.to_string(), t));
        }
    }
    if !md.is_empty() {
        let tokens: i64 = md.iter().map(|(_, t)| est(t)).sum();
        let files = md.iter().map(|(f, t)| format!("{f} {} lines", t.lines().count())).collect::<Vec<_>>().join(" · ");
        parts.push(sees_part("CLAUDE.md", format!("{files} · as on disk now; your own ~/.claude/CLAUDE.md is not read"), Some(tokens), "estimated", None));
    }
    let skills = skills_of(&a.cwd);
    if !skills.is_empty() {
        parts.push(sees_part("Skills and subagents", format!("{} descriptions of the worktree's", skills.len()), Some(skills.iter().map(|s| s.2).sum()), "estimated", None));
    }
    let mut servers: Vec<String> = Vec::new();
    for path in [sup.session_dir(&a.id).join("mcp.json"), cwd.join(".mcp.json")] {
        if let Some(v) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Json>(&t).ok()) {
            servers.extend(v["mcpServers"].as_object().into_iter().flatten().map(|(k, _)| k.clone()));
        }
    }
    servers.sort();
    servers.dedup();
    // Claude Code's raw request bodies: only if you export them.
    let named = |k: &str| std::env::var_os(k).is_some() || std::fs::read_to_string(sup.session_dir(&a.id).join("settings.json")).ok().and_then(|t| serde_json::from_str::<Json>(&t).ok()).is_some_and(|v| v["env"].get(k).is_some());
    let raw = if named("OTEL_LOG_RAW_API_BODIES") {
        json!({ "exported": true, "read": false, "note": "Claude Code's raw request bodies are exported (OTEL_LOG_RAW_API_BODIES is set) to your OpenTelemetry collector. Workbench's own OTLP intake keeps runtime errors only, so they are not read here yet; the parts stay labelled as they are." })
    } else {
        json!({ "exported": false, "read": false, "note": "Claude Code's raw request bodies are not exported (OTEL_LOG_RAW_API_BODIES is not set). Workbench never turns it on: the bodies hold your whole conversation." })
    };
    let mode = sup.all_events(&a.id).unwrap_or_default().iter().rev().filter(|e| e.ts <= next).filter_map(crate::trail::data_of).find_map(|d| d["permission_mode"].as_str().map(str::to_string));
    let version = a.harness.as_deref().and_then(|h| version(sup, h));
    Ok(json!({
        "agent": { "id": a.id, "name": a.name, "harness": a.harness, "version": version.map(|v| v.name), "mode": mode },
        "parts": parts,
        "tools": { "servers": servers, "level": "estimated", "note": "Tool definitions of Claude Code and these MCP servers are in every turn; their tokens are not recorded." },
        "raw_bodies": raw,
        "note": "Injected parts are the text Workbench recorded giving, their tokens about a quarter of their characters. The turn's size and ChloeCtx's part come from Debug's context of the turn. Everything Claude Code assembles itself is estimated.",
    }))
}

#[derive(Deserialize, Default)]
struct NewVersionB {
    repo: Option<String>,
    parent: Option<String>,
    name: Option<String>,
    note: Option<String>,
    /// `kind → text or object`: `permissions` (a settings block, or edits),
    /// `memory` (MemEdits), `hooks` (`{timing}`).
    #[serde(default)]
    parts: serde_json::Map<String, Json>,
}

fn new_version(sup: &Supervisor, b: NewVersionB) -> Result<Json, ApiError> {
    let repo = repo_or_default(sup, b.repo.clone())?;
    let cur = current(sup, &repo);
    let parent = match b.parent.as_deref() {
        Some(p) => version(sup, p).filter(|v| v.kind == "config" && v.repo == repo).ok_or_else(|| ApiError::not_found(format!("no version {p} of {repo}")))?,
        None => cur.clone(),
    };
    let mut e = edits_of(sup, &parent.id);
    for (kind, v) in &b.parts {
        match kind.as_str() {
            "permissions" => {
                e.permissions = match v {
                    Json::String(t) => perm_edits_of(sup, &repo, &PermB { text: Some(t.clone()), ..PermB::default() })?,
                    // Edits carry `remove`; a settings block never does.
                    Json::Object(o) if o.contains_key("remove") => {
                        serde_json::from_value(v.clone()).map_err(|e| ApiError::bad(format!("permissions: {e}")))?
                    }
                    Json::Object(_) => {
                        let block = if v.get("permissions").is_some() { v["permissions"].clone() } else { v.clone() };
                        perms::edits_of_block(&baseline(sup, &repo), &block).map_err(ApiError::bad)?
                    }
                    _ => return Err(ApiError::bad("permissions is a settings block (text or object)")),
                }
            }
            "memory" => {
                let t = if let Json::String(s) = v { serde_json::from_str(s).map_err(|e| ApiError::bad(format!("memory: {e}")))? } else { v.clone() };
                e.memory = serde_json::from_value(t).map_err(|e| ApiError::bad(format!("memory: {e}")))?;
            }
            "hooks" => e.timing = v["timing"].as_bool().unwrap_or(false),
            k => return Err(ApiError::bad(format!("the part {k} is not edited in this build: permissions, memory or hooks"))),
        }
    }
    for (k, t) in e.texts() {
        if let Some(s) = perms::secrets(&t).first() {
            return Err(ApiError::new(400, "secret", format!("{k}: {}", s["message"].as_str().unwrap_or("a secret"))));
        }
    }
    let _g = lock().lock().unwrap();
    let id = format!("{}{}", &e.id()[..12], &hash(&repo)[..4]);
    let all = versions(sup, &repo);
    if let Some(same) = all.iter().find(|v| v.id == id) {
        if same.state == "draft" {
            return Ok(json!({ "version": same.to_json(), "edits": e.to_json(), "same": true }));
        }
        return Err(ApiError::new(409, "unchanged", format!("that is version {} ({}): nothing to save", same.name, same.state)));
    }
    // One draft at a time per repository: a new save replaces the draft.
    for d in all.iter().filter(|v| v.kind == "config" && v.state == "draft") {
        let mut d = d.clone();
        d.state = "dropped".into();
        d.retired = Some(now_ms());
        d.put(sup)?;
    }
    let name = b.name.filter(|n| !n.trim().is_empty() && n.len() <= 32).unwrap_or_else(|| format!("{}-draft", next_name(&all)));
    let v = Version { id: id.clone(), repo: repo.clone(), name, kind: "config".into(), parent: Some(parent.id.clone()), state: "draft".into(), made_by: "you".into(), note: b.note, created: now_ms(), ..Version::default() };
    v.put(sup)?;
    for (k, t) in e.texts() {
        put_part(sup, &id, k, &t)?;
    }
    Ok(json!({ "version": v.to_json(), "edits": e.to_json() }))
}

#[derive(Deserialize, Default)]
struct PromoteB {
    approve_widening: Option<Json>,
}

fn promote(sup: &Supervisor, id: &str, b: PromoteB) -> Result<Json, ApiError> {
    let v = version(sup, id).filter(|v| v.kind == "config").ok_or_else(|| ApiError::not_found(format!("no version {id}")))?;
    let repo = v.repo.clone();
    let cur = current(sup, &repo);
    if v.id == cur.id {
        return Err(ApiError::conflict(format!("{} is already current", v.name)));
    }
    if !matches!(v.state.as_str(), "draft" | "candidate" | "retired") {
        return Err(ApiError::conflict(format!("{} is {}: only a draft, a candidate or a retired version is promoted", v.name, v.state)));
    }
    let e = edits_of(sup, &v.id);
    let (from, to) = window_of(None);
    let w = Win::load(sup, &repo, from, to);
    let before = rules_under(sup, &repo, &cur, &edits_of(sup, &cur.id));
    let after = rules_under(sup, &repo, &v, &e);
    let rep = widen_report(&before, &after, &w.calls, &w.cwd());
    let patterns: Vec<String> = rep["patterns"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
    if !patterns.is_empty() {
        let named: Vec<String> = match &b.approve_widening {
            Some(Json::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
            Some(Json::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        let unnamed: Vec<&String> = patterns.iter().filter(|p| !named.contains(p)).collect();
        if !unnamed.is_empty() {
            return Err(ApiError::new(
                409,
                "widens",
                format!(
                    "{} widens what agents may do: {}. To promote it, confirm by naming {}: approve_widening: {}",
                    v.name,
                    unnamed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                    if unnamed.len() == 1 { "the pattern" } else { "each pattern" },
                    json!(patterns)
                ),
            ));
        }
    }
    let _g = lock().lock().unwrap();
    let now = now_ms();
    let mut old = current_locked(sup, &repo);
    old.state = "retired".into();
    old.retired = Some(now);
    old.put(sup)?;
    let mut nv = v.clone();
    nv.state = "current".into();
    nv.name = nv.name.trim_end_matches("-draft").to_string();
    nv.promoted = Some(now);
    nv.retired = None;
    nv.widens = !patterns.is_empty();
    if nv.widens {
        nv.approved_by = Some("you".into());
        nv.approved = Some(now);
    }
    nv.put(sup)?;
    let mut h = history(sup, &repo);
    h.push(nv.id.clone());
    sup.set_mark(&history_key(&repo), &json!(h).to_string())?;
    Ok(json!({ "version": nv.to_json(), "previous": old.to_json(), "widens": rep }))
}

#[derive(Deserialize, Default)]
struct RollbackB {
    reason: Option<String>,
}

fn rollback(sup: &Supervisor, id: &str, b: RollbackB) -> Result<Json, ApiError> {
    let v = version(sup, id).filter(|v| v.kind == "config").ok_or_else(|| ApiError::not_found(format!("no version {id}")))?;
    let _g = lock().lock().unwrap();
    let cur = current_locked(sup, &v.repo);
    if cur.id != v.id {
        return Err(ApiError::conflict(format!("{} is not current ({} is): only the current version is rolled back", v.name, cur.name)));
    }
    let mut h = history(sup, &v.repo);
    while h.last().is_some_and(|x| *x == cur.id) {
        h.pop();
    }
    let prev = h.last().and_then(|p| version(sup, p)).ok_or_else(|| ApiError::conflict(format!("{} is the first version: there is none to roll back to", cur.name)))?;
    let now = now_ms();
    let mut old = cur.clone();
    old.state = "retired".into();
    old.retired = Some(now);
    old.note = Some(format!("{}rolled back {}{}", old.note.as_deref().map(|n| format!("{n} · ")).unwrap_or_default(), &iso(now)[..16], b.reason.as_deref().filter(|r| !r.trim().is_empty()).map(|r| format!(": {}", one_line(r, 200))).unwrap_or_default()));
    old.put(sup)?;
    let mut nv = prev.clone();
    nv.state = "current".into();
    nv.retired = None;
    nv.promoted = Some(now);
    nv.put(sup)?;
    sup.set_mark(&history_key(&v.repo), &json!(h).to_string())?;
    Ok(json!({ "version": nv.to_json(), "rolled_back": old.to_json() }))
}

fn versions_view(sup: &Supervisor, q: RepoQ) -> Result<Json, ApiError> {
    let repo = repo_or_default(sup, q.repo)?;
    let cur = current(sup, &repo);
    let all = versions(sup, &repo);
    let agents = sup.agent_rows();
    let list: Vec<Json> = all
        .iter()
        .rev()
        .filter(|v| v.kind == "config" && v.state != "dropped")
        .map(|v| {
            let runs: Vec<&Version> = all.iter().filter(|r| r.kind == "run" && r.config.as_deref() == Some(v.id.as_str())).collect();
            let n = agents.iter().filter(|a| a.harness.as_deref().is_some_and(|h| runs.iter().any(|r| r.id == h))).count();
            let mut j = v.to_json();
            j["agents"] = json!(n);
            j["runs"] = json!(runs.iter().map(|r| json!({ "id": r.id, "short": short(&r.id), "claude": r.claude_version })).collect::<Vec<_>>());
            j["edits"] = edits_of(sup, &v.id).to_json();
            j
        })
        .collect();
    Ok(json!({ "repo": repo, "current": cur.to_json(), "versions": list, "history": history(sup, &repo) }))
}

fn version_view(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let v = version(sup, id).ok_or_else(|| ApiError::not_found(format!("no version {id}")))?;
    let parts: Vec<Json> = parts_of(sup, id).into_iter().map(|(k, t)| json!({ "kind": k, "text": t, "tokens": t.len() / 4 })).collect();
    Ok(json!({ "version": v.to_json(), "parts": parts }))
}

async fn overview_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || overview(&sup, q)).await?).into_response())
}

/// `GET /api/harness/fixes`: every problem of the window as one ranked list
/// of changes to the harness (src/harness/fixes.rs).
async fn fixes_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || {
        let repo = repo_or_default(&sup, q.repo)?;
        let days = q.days.unwrap_or(WINDOW_DAYS).clamp(1, 90);
        let (from, to) = window_of(Some(days));
        let cur = current(&sup, &repo);
        let cur_e = edits_of(&sup, &cur.id);
        let w = Win::load(&sup, &repo, from, to);
        let rules = rules_under(&sup, &repo, &cur, &cur_e);
        let found = find(&sup, &w, &rules, true);
        let mut v = fixes::build(&sup, &w, &cur, &cur_e, &found);
        v["window"] = json!({ "days": days, "from": iso(from), "to": iso(to) });
        v["draft"] = versions(&sup, &repo).into_iter().find(|v| v.kind == "config" && v.state == "draft").map(|v| v.to_json()).unwrap_or(Json::Null);
        Ok::<Json, ApiError>(v)
    })
    .await?;
    Ok(AxJson(v).into_response())
}

/// `POST /api/harness/fixes/apply {repo, ids}`: the ticked fixes into one draft.
async fn fixes_apply_route(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult {
    user_only(&app, &headers)?;
    let b: fixes::ApplyB = parse_body(&body)?;
    let sup = app.sup.clone();
    let v = blocking(move || fixes::apply(&sup, b)).await?;
    let status = if v["same"] == json!(true) { StatusCode::OK } else { StatusCode::CREATED };
    Ok((status, AxJson(v)).into_response())
}

async fn findings_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || overview(&sup, q)).await?;
    Ok(AxJson(json!({ "findings": v["findings"], "hooks": v["hooks"], "repo": v["repo"] })).into_response())
}

async fn versions_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || versions_view(&sup, q)).await?).into_response())
}

/// Only the token Workbench printed changes the harness: an agent's own key
/// (src/guarded.rs) reads it, but saves, promotes and rolls back nothing.
fn user_only(app: &App, headers: &HeaderMap) -> Result<(), ApiError> {
    let given = headers.get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer ")).map(str::trim).unwrap_or("");
    let want = app.sup.cfg.token.as_bytes();
    let same = given.len() == want.len() && given.bytes().zip(want.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
    if same {
        Ok(())
    } else {
        Err(ApiError::new(403, "forbidden", "only the user changes the harness: an agent's key can read it, not save, promote or roll back a version"))
    }
}

async fn new_version_route(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult {
    user_only(&app, &headers)?;
    let b: NewVersionB = parse_body(&body)?;
    let sup = app.sup.clone();
    let v = blocking(move || new_version(&sup, b)).await?;
    let status = if v["same"] == json!(true) { StatusCode::OK } else { StatusCode::CREATED };
    Ok((status, AxJson(v)).into_response())
}

async fn version_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || version_view(&sup, &id)).await?).into_response())
}

async fn promote_route(State(app): State<App>, headers: HeaderMap, AxPath(id): AxPath<String>, body: Bytes) -> ApiResult {
    user_only(&app, &headers)?;
    let b: PromoteB = if body.is_empty() { PromoteB::default() } else { parse_body(&body)? };
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || promote(&sup, &id, b)).await?).into_response())
}

async fn rollback_route(State(app): State<App>, headers: HeaderMap, AxPath(id): AxPath<String>, body: Bytes) -> ApiResult {
    user_only(&app, &headers)?;
    let b: RollbackB = if body.is_empty() { RollbackB::default() } else { parse_body(&body)? };
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || rollback(&sup, &id, b)).await?).into_response())
}

async fn permissions_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || permissions_view(&sup, q)).await?).into_response())
}

async fn lint_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: PermB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || lint_route_body(&sup, b)).await?).into_response())
}

async fn impact_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: PermB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || impact_body(&sup, b)).await?).into_response())
}

async fn complete_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || complete_view(&sup, q)).await?).into_response())
}

async fn memory_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || memory_view(&sup, q, None)).await?).into_response())
}

#[derive(Deserialize, Default)]
struct MemB {
    repo: Option<String>,
    version: Option<String>,
    receipt: Option<String>,
    days: Option<i64>,
    edits: Option<MemEdits>,
}

async fn memory_preview_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: MemB = parse_body(&body)?;
    let sup = app.sup.clone();
    let q = RepoQ { repo: b.repo, days: b.days, version: b.version, receipt: b.receipt, kind: None, q: None };
    Ok(AxJson(blocking(move || memory_view(&sup, q, b.edits)).await?).into_response())
}

async fn sees_route(State(app): State<App>, Query(q): Query<SeesQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || sees_view(&sup, q)).await?).into_response())
}

#[derive(Deserialize)]
struct ModeB {
    mode: Option<String>,
}

async fn mode_route(body: Bytes) -> ApiResult {
    let b: ModeB = parse_body(&body)?;
    match b.mode.as_deref() {
        Some("suggest") | None => Ok(AxJson(json!({ "mode": "suggest" })).into_response()),
        Some(m) => Err(ApiError::new(409, "not_built", format!("auto-learn mode {m} is not in this build: suggestions wait for you"))),
    }
}

// --------------------------------------------------------------- the UI

fn ui_file(app: &App, rel: &str, built_in: Option<&'static str>, mime: &'static str) -> Response {
    let body: Vec<u8> = match (&app.ui_dir, built_in) {
        (Some(dir), _) => match std::fs::read(dir.join(rel)) {
            Ok(b) => b,
            Err(_) => return ApiError::not_found(format!("no file {rel}")).into_response(),
        },
        (None, Some(b)) => b.as_bytes().to_vec(),
        (None, None) => return ApiError::not_found("served from disk only (CHLOEDB_WORKBENCH_UI_DIR)").into_response(),
    };
    ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-store"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], body).into_response()
}

/// The page for screenshots before the shell mounts the goal; from disk only.
async fn dev_page(State(app): State<App>) -> Response {
    ui_file(&app, "dev/harness-live.html", None, "text/html; charset=utf-8")
}

async fn dev_font(State(app): State<App>, AxPath(file): AxPath<String>) -> Response {
    if !matches!(file.as_str(), "Geist-normal.woff2" | "GeistMono-normal.woff2") {
        return ApiError::not_found("no such font").into_response();
    }
    ui_file(&app, &format!("dev/harness/fonts/{file}"), None, "font/woff2")
}

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/harness", get(overview_route))
        .route("/api/harness/findings", get(findings_route))
        .route("/api/harness/fixes", get(fixes_route))
        .route("/api/harness/fixes/apply", post(fixes_apply_route))
        .route("/api/harness/versions", get(versions_route).post(new_version_route))
        .route("/api/harness/versions/:id", get(version_route))
        .route("/api/harness/versions/:id/promote", post(promote_route))
        .route("/api/harness/versions/:id/rollback", post(rollback_route))
        .route("/api/harness/permissions", get(permissions_route))
        .route("/api/harness/lint", post(lint_route))
        .route("/api/harness/impact", post(impact_route))
        .route("/api/harness/complete", get(complete_route))
        .route("/api/harness/memory", get(memory_route))
        .route("/api/harness/memory/preview", post(memory_preview_route))
        .route("/api/harness/mode", post(mode_route))
        .route("/api/harness/sees", get(sees_route))
        .route("/dev/harness-live.html", get(dev_page))
        .route("/dev/harness/fonts/:file", get(dev_font))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storm_keys_keep_what_is_typed() {
        assert_eq!(storm_key("PYTHONPATH=. python3 -m pytest -q tests/").as_deref(), Some("PYTHONPATH=. python3 -m pytest"));
        assert_eq!(storm_key("cd web && npm run test -- --watch").as_deref(), Some("npm run test"));
        assert_eq!(storm_key("cargo test -p calc").as_deref(), Some("cargo test"));
        assert_eq!(next_name(&[Version { kind: "config".into(), name: "1.0".into(), ..Version::default() }, Version { kind: "config".into(), name: "1.3-draft".into(), ..Version::default() }]), "1.4");
    }

    #[test]
    fn a_timed_hook_keeps_its_output_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("t.log");
        let cmd = timed("cat; exit 3", "Stop", "workbench", &log);
        let out = std::process::Command::new("sh").arg("-c").arg(&cmd).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(b"{\"x\":1}")?;
            c.wait_with_output()
        });
        let out = out.unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "{\"x\":1}");
        let line = std::fs::read_to_string(&log).unwrap();
        assert!(line.starts_with("Stop\tworkbench\t") && line.trim_end().ends_with("\t3"), "{line}");
    }
}
