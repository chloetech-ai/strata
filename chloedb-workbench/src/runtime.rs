//! Runtime blame (SPEC.md, "Fourth build", use case 3): what an agent's app
//! does wrong while it runs (what its page throws, through the preview
//! overlay; its 5xx answers, through the preview proxy; its server's logs,
//! through OTLP/HTTP at `POST /v1/logs`) as rows `runtime_errors`, and each
//! error joined to the edit that probably caused it: the latest edit before
//! the error first appeared, in the worktree its stack points into, of the
//! file its stack names (else of any file there). The join is an `ASOF
//! JOIN` of `runtime_errors` with `runtime_edits`, the agents' edits
//! (PostToolUse of Edit, MultiEdit, Write, NotebookEdit) kept from the
//! events as rows with their worktree, file and line.
//!
//! A repeat of an error (same agent, source, message with its numbers
//! masked, and file) within 15 minutes counts on the same row.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path as FsPath;
use std::sync::Mutex;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::db::{get_int, get_str, iso, now_ms, opt_int, opt_str, Agent};
use crate::map;
use crate::server::{blocking, parse_body, same_bytes, ApiResult, App};
use crate::supervisor::{one_line, ApiError, Supervisor};
use crate::trail;

/// A repeat within this long counts on the same row.
const REPEAT_MS: i64 = 15 * 60_000;
/// A culprit found by time alone is at most this old.
const TIME_BLAME_MS: i64 = 60 * 60_000;
const MESSAGE_MAX: usize = 2000;
const STACK_MAX: usize = 8 * 1024;
/// The most of a 5xx answer's body that is read for its message and stack.
const BODY_KEEP: usize = 64 * 1024;

/// Read-modify-writes of the error rows, and the edits' sync, one at a time.
static WRITE: Mutex<()> = Mutex::new(());

pub fn routes() -> Router<App> {
    Router::new()
        .route("/v1/logs", post(otlp_logs))
        .route("/preview/:agent/__wb/runtime", post(from_overlay))
        .route("/api/runtime/errors", get(list_route).post(report_route))
        .route("/api/runtime/errors/:id/resolve", post(resolve_route))
}

const COLS: &str = "id, agent, repo, cwd, source, level, message, stack, file, line, col, url, status, service, fingerprint, hits, first_ms, last_ms, state";

/// An error's row.
#[derive(Clone, Debug, Default)]
pub struct RuntimeError {
    pub id: String,
    pub agent: String,
    pub repo: String,
    /// The worktree its stack points into (else its agent's).
    pub cwd: String,
    /// `overlay` (the page threw), `console` (console.error), `proxy` (a
    /// 5xx through the preview), `otlp` (a server's log), `api`.
    pub source: String,
    pub level: String,
    pub message: String,
    pub stack: String,
    pub file: Option<String>,
    pub line: Option<i64>,
    pub col: Option<i64>,
    pub url: Option<String>,
    pub status: Option<i64>,
    pub service: Option<String>,
    pub fingerprint: String,
    pub hits: i64,
    pub first_ms: i64,
    pub last_ms: i64,
    pub state: String,
}

impl RuntimeError {
    fn of(r: &[Value]) -> RuntimeError {
        let s = |i: usize| r.get(i).and_then(get_str);
        let n = |i: usize| r.get(i).and_then(get_int);
        RuntimeError {
            id: s(0).unwrap_or_default(),
            agent: s(1).unwrap_or_default(),
            repo: s(2).unwrap_or_default(),
            cwd: s(3).unwrap_or_default(),
            source: s(4).unwrap_or_default(),
            level: s(5).unwrap_or_default(),
            message: s(6).unwrap_or_default(),
            stack: s(7).unwrap_or_default(),
            file: s(8),
            line: n(9),
            col: n(10),
            url: s(11),
            status: n(12),
            service: s(13),
            fingerprint: s(14).unwrap_or_default(),
            hits: n(15).unwrap_or(1),
            first_ms: n(16).unwrap_or(0),
            last_ms: n(17).unwrap_or(0),
            state: s(18).unwrap_or_default(),
        }
    }

    fn put(&self, sup: &Supervisor) -> Result<(), ApiError> {
        let p = Params::new()
            .set("id", Value::Str(self.id.clone()))
            .set("agent", Value::Str(self.agent.clone()))
            .set("repo", Value::Str(self.repo.clone()))
            .set("cwd", Value::Str(self.cwd.clone()))
            .set("source", Value::Str(self.source.clone()))
            .set("level", Value::Str(self.level.clone()))
            .set("message", Value::Str(self.message.clone()))
            .set("stack", Value::Str(self.stack.clone()))
            .set("file", opt_str(&self.file))
            .set("line", opt_int(self.line))
            .set("col", opt_int(self.col))
            .set("url", opt_str(&self.url))
            .set("status", opt_int(self.status))
            .set("service", opt_str(&self.service))
            .set("fingerprint", Value::Str(self.fingerprint.clone()))
            .set("hits", Value::Int(self.hits))
            .set("first_ms", Value::Int(self.first_ms))
            .set("last_ms", Value::Int(self.last_ms))
            .set("state", Value::Str(self.state.clone()));
        let names: Vec<String> = COLS.split(", ").map(|c| format!(":{c}")).collect();
        sup.db().query(&format!("INSERT OR REPLACE INTO runtime_errors ({COLS}) VALUES ({})", names.join(", ")), &p).map_err(ApiError::internal)?;
        Ok(())
    }

    /// The row as the API answers it, with its frames and its culprit.
    fn to_json(&self, sup: &Supervisor, culprit: Option<&Culprit>) -> Json {
        let a = sup.agent_row(&self.agent);
        let frames: Vec<Json> = a.as_ref().map(|a| frames_of(sup, a, &self.stack, None)).unwrap_or_default().iter().map(Frame::to_json).collect();
        json!({
            "id": self.id, "agent": self.agent, "agent_name": a.map(|a| a.name), "repo": self.repo, "cwd": self.cwd,
            "source": self.source, "level": self.level, "message": self.message, "stack": self.stack, "frames": frames,
            "file": self.file, "line": self.line, "col": self.col, "url": self.url, "status": self.status, "service": self.service,
            "count": self.hits, "first_ms": self.first_ms, "last_ms": self.last_ms, "first": iso(self.first_ms), "last": iso(self.last_ms),
            "state": self.state,
            "culprit": culprit.map(|c| c.to_json(sup, self)),
        })
    }
}

// ---------------------------------------------------------------- frames

/// A frame of a stack, its location read back to a file of a worktree
/// when it is one.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub func: Option<String>,
    /// Relative to `cwd` when `in_repo`, else as written.
    pub file: String,
    pub line: i64,
    pub col: Option<i64>,
    pub in_repo: bool,
    pub cwd: Option<String>,
    pub raw: String,
}

impl Frame {
    fn to_json(&self) -> Json {
        json!({ "fn": self.func, "file": self.file, "line": self.line, "col": self.col, "in_repo": self.in_repo, "raw": self.raw })
    }
}

/// `(function, location, line, column)` of one line of a stack: V8's `at
/// f (loc:l:c)` and `at loc:l:c`, Firefox and Safari's `f@loc:l:c`,
/// Python's `File "loc", line l, in f`, and a bare `loc:l[:c]`.
pub fn parse_frame(line: &str) -> Option<(Option<String>, String, i64, Option<i64>)> {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix("File \"") {
        let (loc, after) = rest.split_once('"')?;
        let n: i64 = after.split("line ").nth(1)?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
        let func = after.split(", in ").nth(1).map(|f| f.trim().to_string());
        return Some((func, loc.to_string(), n, None));
    }
    let (func, loc) = if let Some(rest) = t.strip_prefix("at ") {
        let rest = rest.strip_prefix("async ").unwrap_or(rest);
        match (rest.rfind(" ("), rest.ends_with(')')) {
            (Some(i), true) => (Some(rest[..i].trim().to_string()), &rest[i + 2..rest.len() - 1]),
            _ => (None, rest),
        }
    } else if let Some((f, loc)) = t.split_once('@').filter(|(f, l)| !f.contains(' ') && l.contains(':')) {
        (Some(f.to_string()).filter(|f| !f.is_empty()), loc)
    } else {
        // A bare location anywhere in the line: the last word that ends in
        // :line or :line:col.
        let word = t.split(|c: char| c.is_whitespace() || c == '(' || c == ')').filter(|w| w.contains(':')).last()?;
        (None, word)
    };
    // loc:line[:col], from the right (a URL has colons of its own).
    let mut parts = loc.rsplitn(3, ':');
    let last = parts.next()?;
    let mid = parts.next()?;
    let (file, line, col) = match (mid.parse::<i64>(), last.parse::<i64>()) {
        (Ok(l), Ok(c)) => (parts.next()?.to_string(), l, Some(c)),
        (_, Ok(l)) => {
            let rest: Vec<&str> = loc.rsplitn(2, ':').collect();
            (rest.get(1)?.to_string(), l, None)
        }
        _ => return None,
    };
    if file.is_empty() || line <= 0 || file.contains(' ') {
        return None;
    }
    Some((func, file, line, col))
}

/// A location as a path: no scheme and host, query or fragment, no
/// `/preview/<agent>/` or base path, no bundler prefix.
pub fn loc_path(loc: &str, prefix_paths: &[String]) -> String {
    let mut p = loc.trim();
    for pre in ["webpack-internal:///", "webpack:///", "file://"] {
        if let Some(r) = p.strip_prefix(pre) {
            p = r;
        }
    }
    let mut s = p.to_string();
    if let Some(i) = s.find("://") {
        let after = &s[i + 3..];
        s = after.find('/').map_or_else(|| "/".to_string(), |j| after[j..].to_string());
    }
    if let Some(i) = s.find(['?', '#']) {
        s.truncate(i);
    }
    for pre in prefix_paths {
        if let Some(r) = s.strip_prefix(pre.as_str()) {
            s = format!("/{}", r.trim_start_matches('/'));
            break;
        }
    }
    if let Some(r) = s.strip_prefix("/@fs/") {
        s = format!("/{r}");
    }
    s.trim_start_matches("./").to_string()
}

/// The frames of a stack (and of the place `onerror` named), each read back
/// to a file of the repository's worktrees when it is one: an absolute
/// path under an agent's worktree (or the main checkout) is that
/// worktree's; a URL's path is the reporting agent's worktree's when the
/// file is there. `node_modules` is never the repository's.
pub fn frames_of(sup: &Supervisor, a: &Agent, stack: &str, at: Option<(&str, i64, Option<i64>)>) -> Vec<Frame> {
    let mut others: Vec<Agent> = sup.agent_rows().into_iter().filter(|x| x.repo == a.repo && x.id != a.id).collect();
    others.sort_by_key(|x| std::cmp::Reverse(x.cwd.len()));
    let mut prefixes = vec![format!("/preview/{}/", a.id)];
    if let Some(u) = &a.preview_url {
        let after = u.split("://").nth(1).unwrap_or("");
        if let Some(i) = after.find('/') {
            prefixes.push(format!("{}/", &after[i..]));
        }
    }
    let mut out: Vec<Frame> = Vec::new();
    let mut lines: Vec<(Option<String>, String, i64, Option<i64>, String)> = Vec::new();
    for l in stack.lines().take(60) {
        if let Some((f, loc, n, c)) = parse_frame(l) {
            lines.push((f, loc, n, c, l.trim().to_string()));
        }
    }
    if let Some((file, n, c)) = at.filter(|(f, n, _)| !f.is_empty() && *n > 0) {
        if !lines.iter().any(|(_, loc, ln, _, _)| loc == file && *ln == n) {
            lines.insert(0, (None, file.to_string(), n, c, format!("{file}:{n}")));
        }
    }
    for (func, loc, n, c, raw) in lines.into_iter().take(24) {
        let path = loc_path(&loc, &prefixes);
        let mut frame = Frame { func, file: path.clone(), line: n, col: c, in_repo: false, cwd: None, raw };
        if !path.contains("node_modules/") && !path.starts_with("node:") && !path.starts_with('<') {
            let abs = FsPath::new(&path);
            let mut place: Option<(String, String)> = None;
            if abs.is_absolute() {
                let dirs: Vec<&String> = std::iter::once(&a.cwd).chain(others.iter().map(|o| &o.cwd)).chain(std::iter::once(&a.repo)).collect();
                for dir in dirs {
                    if let Some(rel) = map::rel_path(&path, FsPath::new(dir), FsPath::new(dir)) {
                        place = Some((dir.clone(), rel));
                        break;
                    }
                }
                if place.is_none() && FsPath::new(&a.cwd).join(path.trim_start_matches('/')).is_file() {
                    place = Some((a.cwd.clone(), path.trim_start_matches('/').to_string()));
                }
            } else if FsPath::new(&a.cwd).join(&path).is_file() {
                place = Some((a.cwd.clone(), path.clone()));
            }
            if let Some((dir, rel)) = place {
                frame.file = rel;
                frame.cwd = Some(dir);
                frame.in_repo = true;
            }
        }
        out.push(frame);
    }
    out
}

// --------------------------------------------------------------- recording

/// An error as it comes in.
#[derive(Debug, Default, Deserialize)]
pub struct Incoming {
    pub agent: Option<String>,
    pub source: Option<String>,
    pub level: Option<String>,
    pub message: Option<String>,
    pub stack: Option<String>,
    pub url: Option<String>,
    pub status: Option<i64>,
    pub service: Option<String>,
    /// Where `onerror` says it happened.
    pub filename: Option<String>,
    pub lineno: Option<i64>,
    pub colno: Option<i64>,
    pub ts_ms: Option<i64>,
}

fn cut(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// The message with its numbers and hex runs masked, for telling repeats.
pub fn mask(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut in_num = false;
    for c in msg.chars() {
        if c.is_ascii_digit() {
            if !in_num {
                out.push('#');
            }
            in_num = true;
        } else {
            in_num = false;
            out.push(c);
        }
    }
    out
}

fn fingerprint(agent: &str, source: &str, msg: &str, file: Option<&str>) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (agent, source, mask(msg).trim(), file.unwrap_or("")).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Records an error of `agent`'s app (or counts a repeat). Answers its row.
pub fn record(sup: &Supervisor, agent: &str, inc: Incoming) -> Result<RuntimeError, ApiError> {
    let a = sup.agent_row(agent).ok_or_else(|| ApiError::not_found(format!("no agent {agent}")))?;
    let message = one_line(inc.message.as_deref().unwrap_or("").trim(), MESSAGE_MAX);
    if message.is_empty() {
        return Err(ApiError::bad("message is required"));
    }
    let source = inc.source.as_deref().filter(|s| matches!(*s, "overlay" | "console" | "proxy" | "otlp" | "api" | "persona")).unwrap_or("api").to_string();
    let level = inc.level.as_deref().filter(|l| matches!(*l, "error" | "warn" | "fatal")).unwrap_or(if message.starts_with("Warning:") { "warn" } else { "error" }).to_string();
    let stack = cut(inc.stack.as_deref().unwrap_or(""), STACK_MAX);
    let at = inc.filename.as_deref().map(|f| (f, inc.lineno.unwrap_or(0), inc.colno));
    let frames = frames_of(sup, &a, &stack, at);
    let top = frames.iter().find(|f| f.in_repo);
    let stack = if stack.is_empty() { frames.first().map(|f| f.raw.clone()).unwrap_or_default() } else { stack };
    let file = top.map(|f| f.file.clone());
    let fp = fingerprint(&a.id, &source, &message, file.as_deref());
    let now = inc.ts_ms.filter(|t| (now_ms() - t).abs() < 10 * 60_000).unwrap_or_else(now_ms);
    let _w = WRITE.lock().unwrap();
    let p = Params::new().set("agent", Value::Str(a.id.clone())).set("fp", Value::Str(fp.clone())).set("since", Value::Int(now - REPEAT_MS));
    let rows = sup.db().query(&format!("SELECT {COLS} FROM runtime_errors WHERE agent = :agent AND fingerprint = :fp AND state = 'open' AND last_ms >= :since ORDER BY last_ms DESC LIMIT 1"), &p).map_err(ApiError::internal)?;
    if let Some(r) = rows.first() {
        let mut e = RuntimeError::of(r);
        e.hits += 1;
        e.last_ms = e.last_ms.max(now);
        e.put(sup)?;
        return Ok(e);
    }
    let e = RuntimeError {
        id: crate::levels::new_id("e"),
        agent: a.id.clone(),
        repo: a.repo.clone(),
        cwd: top.and_then(|f| f.cwd.clone()).unwrap_or_else(|| a.cwd.clone()),
        source,
        level,
        message,
        stack,
        file,
        line: top.map(|f| f.line),
        col: top.and_then(|f| f.col),
        url: inc.url.map(|u| cut(&u, 500)).filter(|u| !u.is_empty()),
        status: inc.status,
        service: inc.service.map(|s| cut(&s, 120)),
        fingerprint: fp,
        hits: 1,
        first_ms: now,
        last_ms: now,
        state: "open".into(),
    };
    e.put(sup)?;
    Ok(e)
}

// ------------------------------------------------------------------- edits

/// The line of a file where an edit's new text starts, as the file is now.
fn edit_line(dir: &FsPath, rel: &str, tool: &str, input: &Json) -> Option<i64> {
    if tool == "Write" {
        return Some(1);
    }
    let new = input["new_string"].as_str().or_else(|| input["edits"].get(0).and_then(|e| e["new_string"].as_str()))?;
    let first = new.lines().map(str::trim).find(|l| l.len() >= 3)?;
    let full = dir.join(rel);
    if std::fs::metadata(&full).ok()?.len() > 8 << 20 {
        return None;
    }
    let body = std::fs::read_to_string(full).ok()?;
    body.lines().position(|l| l.contains(first)).map(|i| i as i64 + 1)
}

/// Brings `runtime_edits` up to the events: each PostToolUse of an edit
/// tool after the last one kept, with its worktree, file and line.
pub fn sync_edits(sup: &Supervisor) -> Result<(), ApiError> {
    let last: i64 = sup.mark("runtime:edits_seq").and_then(|v| v.parse().ok()).unwrap_or(0);
    let p = Params::new().set("after", Value::Int(last));
    let rows = sup.db().query("SELECT id, seq, ts, agent, tool, data FROM events WHERE seq > :after AND kind = 'PostToolUse' ORDER BY seq ASC LIMIT 20000", &p).map_err(ApiError::internal)?;
    let mut top = last;
    let mut agents: HashMap<String, Option<Agent>> = HashMap::new();
    for r in &rows {
        let seq = get_int(&r[1]).unwrap_or(0);
        top = top.max(seq);
        let tool = get_str(&r[4]).unwrap_or_default();
        if !map::EDIT_TOOLS.contains(&tool.as_str()) {
            continue;
        }
        let agent = get_str(&r[3]).unwrap_or_default();
        let Some(a) = agents.entry(agent.clone()).or_insert_with(|| sup.agent_row(&agent)).clone() else { continue };
        let Some(d) = get_str(&r[5]).and_then(|d| serde_json::from_str::<Json>(&d).ok()) else { continue };
        let input = &d["tool_input"];
        let Some(fp) = ["file_path", "notebook_path", "path"].iter().find_map(|k| input.get(*k).and_then(Json::as_str)) else { continue };
        let Some(rel) = map::rel_path(fp, FsPath::new(&a.cwd), FsPath::new(&a.repo)) else { continue };
        let line = edit_line(FsPath::new(&a.cwd), &rel, &tool, input);
        let ts = crate::db::get_ms(&r[2]).unwrap_or(0);
        let p = Params::new()
            .set("id", Value::Str(get_str(&r[0]).unwrap_or_default()))
            .set("seq", Value::Int(seq))
            .set("ts_ms", Value::Int(ts))
            .set("agent", Value::Str(a.id.clone()))
            .set("repo", Value::Str(a.repo.clone()))
            .set("cwd", Value::Str(a.cwd.clone()))
            .set("file", Value::Str(rel))
            .set("line", opt_int(line))
            .set("tool", Value::Str(tool));
        sup.db().query("INSERT OR REPLACE INTO runtime_edits (id, seq, ts_ms, agent, repo, cwd, file, line, tool) VALUES (:id, :seq, :ts_ms, :agent, :repo, :cwd, :file, :line, :tool)", &p).map_err(ApiError::internal)?;
    }
    if top > last {
        sup.set_mark("runtime:edits_seq", &top.to_string())?;
    }
    Ok(())
}

/// The edit an error is put down to.
#[derive(Clone, Debug, PartialEq)]
pub struct Culprit {
    pub edit: String,
    pub agent: String,
    pub file: String,
    pub edit_line: Option<i64>,
    pub edit_ms: i64,
    /// `stack`: the edit is to the file the error's stack names; `time`:
    /// the last edit in that worktree before it.
    pub how: &'static str,
}

impl Culprit {
    fn to_json(&self, sup: &Supervisor, e: &RuntimeError) -> Json {
        let a = sup.agent_row(&self.agent);
        let line = if self.how == "stack" { e.line.or(self.edit_line) } else { self.edit_line };
        json!({
            "agent": a.as_ref().map(|a| trail::agent_json(sup, a)).unwrap_or(json!({ "id": self.agent, "name": self.agent })),
            "agent_id": self.agent,
            "file": self.file, "line": line, "edit_line": self.edit_line,
            "edit_ms": self.edit_ms, "edit": iso(self.edit_ms), "before_ms": e.first_ms - self.edit_ms,
            "how": self.how, "event": self.edit, "summary": format!("Edit {}", self.file),
        })
    }
}

/// The culprits of open errors (of `repo`, or all): an `ASOF JOIN` of each
/// error with the latest edit at or before its first time, in its worktree,
/// of its file; else (by time) of any file there, when at most an hour
/// before. Falls back to the same lookup over the rows in order, and says
/// so, if the join cannot run.
pub fn culprits(sup: &Supervisor, repo: Option<&str>) -> (HashMap<String, Culprit>, Vec<String>) {
    let mut missing = Vec::new();
    if let Err(e) = sync_edits(sup) {
        missing.push(format!("the agents' edits could not be read: {}", e.message));
    }
    let mut out: HashMap<String, Culprit> = HashMap::new();
    let p = match repo {
        Some(r) => Params::new().set("repo", Value::Str(r.to_string())),
        None => Params::new(),
    };
    let only = if repo.is_some() { " AND e.repo = :repo" } else { "" };
    let mut join = |on: &str, how: &'static str| -> Result<(), String> {
        let sql = format!(
            "SELECT e.id, d.id, d.agent, d.file, d.line, d.ts_ms, e.first_ms FROM runtime_errors e ASOF JOIN runtime_edits d MATCH_CONDITION (e.first_ms >= d.ts_ms) ON {on} WHERE e.state = 'open'{only} LIMIT 100000"
        );
        for r in sup.db().query(&sql, &p)? {
            let (Some(id), Some(edit)) = (get_str(&r[0]), get_str(&r[1])) else { continue };
            let edit_ms = get_int(&r[5]).unwrap_or(0);
            if how == "time" && get_int(&r[6]).unwrap_or(0) - edit_ms > TIME_BLAME_MS {
                continue;
            }
            out.entry(id).or_insert(Culprit { edit, agent: get_str(&r[2]).unwrap_or_default(), file: get_str(&r[3]).unwrap_or_default(), edit_line: get_int(&r[4]), edit_ms, how });
        }
        Ok(())
    };
    let joined = join("e.cwd = d.cwd AND e.file = d.file", "stack").and_then(|_| join("e.cwd = d.cwd", "time"));
    if let Err(err) = joined {
        missing.push(format!("ASOF JOIN failed ({err}); the culprits come from an ordered lookup"));
        out = ordered_lookup(sup, repo);
    }
    (out, missing)
}

/// The same join as [`culprits`], over the rows in order.
fn ordered_lookup(sup: &Supervisor, repo: Option<&str>) -> HashMap<String, Culprit> {
    let rows = sup.db().query("SELECT id, agent, cwd, file, line, ts_ms FROM runtime_edits ORDER BY ts_ms ASC LIMIT 1000000", &Params::new()).unwrap_or_default();
    let edits: Vec<(String, String, String, String, Option<i64>, i64)> = rows
        .iter()
        .map(|r| (get_str(&r[0]).unwrap_or_default(), get_str(&r[1]).unwrap_or_default(), get_str(&r[2]).unwrap_or_default(), get_str(&r[3]).unwrap_or_default(), get_int(&r[4]), get_int(&r[5]).unwrap_or(0)))
        .collect();
    let mut out = HashMap::new();
    for e in errors(sup, None, repo, "open", 100_000) {
        let before = edits.iter().filter(|d| d.2 == e.cwd && d.5 <= e.first_ms);
        let by_file = before.clone().filter(|d| e.file.as_deref() == Some(d.3.as_str())).last();
        let pick = by_file.map(|d| (d, "stack")).or_else(|| before.last().filter(|d| e.first_ms - d.5 <= TIME_BLAME_MS).map(|d| (d, "time")));
        if let Some((d, how)) = pick {
            out.insert(e.id.clone(), Culprit { edit: d.0.clone(), agent: d.1.clone(), file: d.3.clone(), edit_line: d.4, edit_ms: d.5, how });
        }
    }
    out
}

/// Error rows, newest first.
pub fn errors(sup: &Supervisor, agent: Option<&str>, repo: Option<&str>, state: &str, limit: usize) -> Vec<RuntimeError> {
    let mut wh = Vec::new();
    let mut p = Params::new();
    if let Some(a) = agent {
        wh.push("agent = :agent");
        p = p.set("agent", Value::Str(a.to_string()));
    }
    if let Some(r) = repo {
        wh.push("repo = :repo");
        p = p.set("repo", Value::Str(r.to_string()));
    }
    if state != "all" {
        wh.push("state = :state");
        p = p.set("state", Value::Str(state.to_string()));
    }
    let w = if wh.is_empty() { String::new() } else { format!(" WHERE {}", wh.join(" AND ")) };
    let rows = sup.db().query(&format!("SELECT {COLS} FROM runtime_errors{w} ORDER BY last_ms DESC LIMIT {}", limit.clamp(1, 100_000)), &p).unwrap_or_default();
    rows.iter().map(|r| RuntimeError::of(r)).collect()
}

/// `GET /api/runtime/errors`: errors with their culprits.
pub fn list(sup: &Supervisor, agent: Option<&str>, repo: Option<&str>, state: &str, limit: usize) -> Json {
    let list = errors(sup, agent, repo, state, limit);
    let (found, missing) = if list.is_empty() { (HashMap::new(), Vec::new()) } else { culprits(sup, repo) };
    let total = list.len();
    json!({
        "errors": list.iter().map(|e| e.to_json(sup, found.get(&e.id))).collect::<Vec<_>>(),
        "total": total,
        "missing": missing,
        "otlp": { "endpoint": format!("{}/v1/logs", sup.cfg.url), "env": "OTEL_EXPORTER_OTLP_ENDPOINT=$CHLOEDB_WORKBENCH_URL OTEL_EXPORTER_OTLP_PROTOCOL=http/json OTEL_EXPORTER_OTLP_HEADERS=\"Authorization=Bearer $CHLOEDB_WORKBENCH_TOKEN,x-workbench-agent=$CHLOEDB_WORKBENCH_AGENT\"" },
    })
}

// ------------------------------------------------------------------ routes

#[derive(Deserialize)]
struct ListQ {
    agent: Option<String>,
    repo: Option<String>,
    state: Option<String>,
    limit: Option<usize>,
}

async fn list_route(State(app): State<App>, Query(q): Query<ListQ>) -> ApiResult {
    let sup = app.sup.clone();
    let state = q.state.filter(|s| matches!(s.as_str(), "open" | "resolved" | "all")).unwrap_or_else(|| "open".into());
    let v = blocking(move || Ok(list(&sup, q.agent.as_deref().filter(|a| !a.is_empty()), q.repo.as_deref().filter(|r| !r.is_empty()), &state, q.limit.unwrap_or(50)))).await?;
    Ok(AxJson(v).into_response())
}

async fn report_route(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let mut inc: Incoming = parse_body(&body)?;
    let agent = inc.agent.take().or_else(|| headers.get("x-workbench-agent").and_then(|h| h.to_str().ok()).map(str::to_string)).filter(|a| !a.is_empty()).ok_or_else(|| ApiError::bad("agent is required"))?;
    if inc.source.is_none() {
        inc.source = Some("api".into());
    }
    let sup = app.sup.clone();
    let e = blocking(move || record(&sup, &agent, inc)).await?;
    let sup = app.sup.clone();
    let v = blocking(move || Ok(e.to_json(&sup, None))).await?;
    Ok((StatusCode::CREATED, AxJson(json!({ "error": v }))).into_response())
}

async fn resolve_route(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || {
        let _w = WRITE.lock().unwrap();
        let rows = sup.db().query(&format!("SELECT {COLS} FROM runtime_errors WHERE id = :id LIMIT 1"), &Params::new().set("id", Value::Str(id.clone()))).map_err(ApiError::internal)?;
        let mut e = rows.first().map(|r| RuntimeError::of(r)).ok_or_else(|| ApiError::not_found(format!("no error {id}")))?;
        e.state = "resolved".into();
        e.put(&sup)?;
        Ok(e.to_json(&sup, None))
    })
    .await?;
    Ok(AxJson(json!({ "error": v })).into_response())
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|h| h.to_str().ok()).flat_map(|h| h.split(';')).find_map(|kv| kv.trim().strip_prefix("wb_preview=").map(str::to_string))
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers.get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer ")).map(|b| b.trim().to_string())
}

/// `POST /preview/:agent/__wb/runtime`: the overlay's report of what the
/// page threw (`kind`: error, unhandledrejection, console). The preview's
/// cookie is the token, as on the proxy.
async fn from_overlay(State(app): State<App>, Path(agent): Path<String>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let token = &app.sup.cfg.token;
    if !cookie_token(&headers).or_else(|| bearer(&headers)).is_some_and(|t| same_bytes(&t, token)) {
        return Err(ApiError::new(401, "unauthenticated", "the preview's cookie (or the token) is required"));
    }
    if body.len() > 64 * 1024 {
        return Err(ApiError::bad("a report is at most 64 KiB"));
    }
    let v: Json = parse_body(&body)?;
    let s = |k: &str| v.get(k).and_then(Json::as_str).map(str::to_string);
    let kind = s("kind").unwrap_or_default();
    let inc = Incoming {
        source: Some(if kind == "console" { "console" } else { "overlay" }.into()),
        message: s("message"),
        stack: s("stack"),
        url: s("url"),
        filename: s("filename"),
        lineno: v.get("lineno").and_then(Json::as_i64),
        colno: v.get("colno").and_then(Json::as_i64),
        ts_ms: v.get("ts_ms").and_then(Json::as_i64),
        ..Incoming::default()
    };
    let sup = app.sup.clone();
    let e = blocking(move || record(&sup, &agent, inc)).await?;
    Ok(AxJson(json!({ "ok": true, "id": e.id })).into_response())
}

/// A body's first line worth reading: HTML without its tags.
fn first_line(text: &str) -> String {
    let plain = if text.trim_start().starts_with('<') {
        let mut out = String::new();
        let mut tag = false;
        let lower = text.to_ascii_lowercase();
        // Skip <head>: its title and styles are not the message.
        let start = lower.find("<body").unwrap_or(0);
        for c in text[start..].chars() {
            match c {
                '<' => tag = true,
                '>' => {
                    tag = false;
                    out.push('\n');
                }
                _ if !tag => out.push(c),
                _ => {}
            }
        }
        out
    } else {
        text.to_string()
    };
    plain.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| one_line(l, 300)).unwrap_or_default()
}

/// The preview proxy's hook: an answer of status 500 or more is recorded
/// as an error of the agent's app (its body's first line the message, the
/// body the stack when it holds one); the answer goes on unchanged.
pub async fn after_proxy(app: &App, agent: &str, method: &Method, rest: &str, res: Response) -> Response {
    let status = res.status().as_u16();
    if status < 500 {
        return res;
    }
    let (parts, body) = res.into_parts();
    let bytes = match axum::body::to_bytes(body, 64 << 20).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(BODY_KEEP)]).to_string();
    let line = first_line(&text);
    let path = format!("/{rest}");
    let inc = Incoming {
        source: Some("proxy".into()),
        message: Some(if line.is_empty() { format!("{status} {method} {path}") } else { format!("{status} {method} {path}: {line}") }),
        stack: Some(if text.lines().any(|l| parse_frame(l).is_some() && (l.contains("at ") || l.contains("File \""))) { text } else { String::new() }),
        url: Some(path),
        status: Some(status as i64),
        ..Incoming::default()
    };
    let (sup, agent) = (app.sup.clone(), agent.to_string());
    let _ = tokio::task::spawn_blocking(move || record(&sup, &agent, inc)).await;
    Response::from_parts(parts, Body::from(bytes))
}

// -------------------------------------------------------------------- OTLP

fn any_value(v: &Json) -> Option<String> {
    for k in ["stringValue", "intValue", "doubleValue", "boolValue"] {
        if let Some(x) = v.get(k) {
            return Some(x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()));
        }
    }
    if let Some(list) = v.get("arrayValue").and_then(|a| a["values"].as_array()) {
        return Some(list.iter().filter_map(any_value).collect::<Vec<_>>().join(" "));
    }
    v.get("kvlistValue").map(|kv| kv.to_string())
}

fn attr(attrs: &Json, key: &str) -> Option<String> {
    attrs.as_array()?.iter().find(|a| a["key"] == key).and_then(|a| any_value(&a["value"]))
}

/// The agent an OTLP request is for: the `x-workbench-agent` header, the
/// resource's `workbench.agent`, or its `service.name` when that is an
/// agent's id or name (a running one first).
fn otlp_agent(sup: &Supervisor, header: Option<&str>, res_attrs: &Json) -> Option<String> {
    if let Some(h) = header.filter(|h| sup.agent_row(h).is_some()) {
        return Some(h.to_string());
    }
    if let Some(a) = attr(res_attrs, "workbench.agent").filter(|a| sup.agent_row(a).is_some()) {
        return Some(a);
    }
    let svc = attr(res_attrs, "service.name")?;
    let mut rows = sup.agent_rows();
    rows.sort_by_key(|a| (!sup.is_running(&a.id), std::cmp::Reverse(a.created)));
    rows.into_iter().find(|a| a.id == svc || a.name == svc).map(|a| a.id)
}

/// `POST /v1/logs`: OTLP/HTTP logs in JSON. Records of severity ERROR and
/// above, and records with an exception, become errors of the agent the
/// request names. Answers as an OTLP collector does.
async fn otlp_logs(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    let token = &app.sup.cfg.token;
    if !bearer(&headers).is_some_and(|t| same_bytes(&t, token)) {
        return ApiError::new(401, "unauthenticated", "OTLP to Workbench needs its token: OTEL_EXPORTER_OTLP_HEADERS=\"Authorization=Bearer $CHLOEDB_WORKBENCH_TOKEN\"").into_response();
    }
    let ctype = headers.get(header::CONTENT_TYPE).and_then(|h| h.to_str().ok()).unwrap_or("");
    if ctype.contains("protobuf") {
        return ApiError::new(415, "unsupported", "Workbench takes OTLP logs as JSON: OTEL_EXPORTER_OTLP_PROTOCOL=http/json (or OTEL_EXPORTER_OTLP_LOGS_PROTOCOL)").into_response();
    }
    let v: Json = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return ApiError::bad(format!("not OTLP JSON: {e}")).into_response(),
    };
    let header_agent = headers.get("x-workbench-agent").and_then(|h| h.to_str().ok()).map(str::to_string);
    let sup = app.sup.clone();
    let res = tokio::task::spawn_blocking(move || {
        let (mut taken, mut rejected) = (0u64, 0u64);
        let mut why: Option<String> = None;
        for rl in v["resourceLogs"].as_array().into_iter().flatten() {
            let res_attrs = &rl["resource"]["attributes"];
            let agent = otlp_agent(&sup, header_agent.as_deref(), res_attrs);
            let service = attr(res_attrs, "service.name");
            for rec in rl["scopeLogs"].as_array().into_iter().flatten().flat_map(|s| s["logRecords"].as_array().into_iter().flatten()) {
                let attrs = &rec["attributes"];
                let sev = rec["severityNumber"].as_i64().unwrap_or(0);
                let sev_text = rec["severityText"].as_str().unwrap_or("").to_ascii_lowercase();
                let exc_type = attr(attrs, "exception.type");
                let exc_msg = attr(attrs, "exception.message");
                let exc_stack = attr(attrs, "exception.stacktrace");
                let is_error = sev >= 17 || matches!(sev_text.as_str(), "error" | "fatal" | "critical" | "crit" | "emerg" | "alert") || exc_type.is_some() || exc_stack.is_some();
                if !is_error {
                    continue;
                }
                let Some(agent) = agent.clone() else {
                    rejected += 1;
                    why = Some("no agent: send the header x-workbench-agent, or the resource attribute workbench.agent".into());
                    continue;
                };
                let body_text = any_value(&rec["body"]).unwrap_or_default();
                let message = match (&exc_type, &exc_msg) {
                    (Some(t), Some(m)) => format!("{t}: {m}"),
                    (None, Some(m)) => m.clone(),
                    (Some(t), None) if body_text.is_empty() => t.clone(),
                    _ => body_text.clone(),
                };
                let ts = rec["timeUnixNano"].as_str().and_then(|t| t.parse::<i64>().ok()).or_else(|| rec["timeUnixNano"].as_i64()).map(|n| n / 1_000_000).filter(|t| *t > 0);
                let url = attr(attrs, "url.path").or_else(|| attr(attrs, "http.target")).or_else(|| attr(attrs, "http.route"));
                let status = attr(attrs, "http.response.status_code").or_else(|| attr(attrs, "http.status_code")).and_then(|s| s.parse().ok());
                let inc = Incoming {
                    source: Some("otlp".into()),
                    level: Some(if sev >= 21 || sev_text == "fatal" { "fatal" } else { "error" }.into()),
                    message: Some(message),
                    stack: exc_stack.or_else(|| Some(body_text).filter(|b| b.lines().any(|l| parse_frame(l).is_some()))),
                    url,
                    status,
                    service: service.clone(),
                    ts_ms: ts,
                    ..Incoming::default()
                };
                match record(&sup, &agent, inc) {
                    Ok(_) => taken += 1,
                    Err(e) => {
                        rejected += 1;
                        why = Some(e.message);
                    }
                }
            }
        }
        (taken, rejected, why)
    })
    .await;
    match res {
        Ok((_, 0, _)) => AxJson(json!({})).into_response(),
        Ok((_, n, why)) => AxJson(json!({ "partialSuccess": { "rejectedLogRecords": n, "errorMessage": why.unwrap_or_default() } })).into_response(),
        Err(e) => ApiError::internal(e.to_string()).into_response(),
    }
}

// ----------------------------------------------------------------- the MCP

fn clock(ms: i64) -> String {
    iso(ms).get(11..16).unwrap_or("").to_string()
}

/// The `runtime_errors` tool: the caller's app's errors (with `all`, every
/// agent's of its repository), each with the edit it is put down to.
pub fn mcp_tool(call: &dyn Fn(&str, &str, Option<Json>, u64) -> Result<Json, String>, me: &str, args: &Json) -> Result<String, String> {
    if me.is_empty() {
        return Err("runtime_errors is for an agent of Workbench: CHLOEDB_WORKBENCH_AGENT is not set".into());
    }
    let all = args.get("all").and_then(Json::as_bool).unwrap_or(false);
    let limit = args.get("limit").and_then(Json::as_u64).unwrap_or(20).clamp(1, 100);
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    let path = if all {
        let a = call("GET", &format!("/api/agents/{}", enc(me)), None, 15)?;
        format!("/api/runtime/errors?repo={}&limit={limit}", enc(a["agent"]["repo"].as_str().unwrap_or("")))
    } else {
        format!("/api/runtime/errors?agent={}&limit={limit}", enc(me))
    };
    let v = call("GET", &path, None, 30)?;
    let list = v["errors"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return Ok(format!(
            "No open runtime errors{}. Workbench records what the previewed page throws, its 5xx answers through the preview, and OTLP logs sent to {} (OTEL_EXPORTER_OTLP_ENDPOINT=$CHLOEDB_WORKBENCH_URL, OTEL_EXPORTER_OTLP_PROTOCOL=http/json, OTEL_EXPORTER_OTLP_HEADERS=\"Authorization=Bearer $CHLOEDB_WORKBENCH_TOKEN,x-workbench-agent=$CHLOEDB_WORKBENCH_AGENT\").",
            if all { " in this repository" } else { " in your app" },
            v["otlp"]["endpoint"].as_str().unwrap_or("/v1/logs")
        ));
    }
    let s = |e: &Json, k: &str| e[k].as_str().unwrap_or("").to_string();
    let lines: Vec<String> = list
        .iter()
        .map(|e| {
            let mut l = format!("{} [{} ×{}] {}", s(e, "id"), s(e, "source"), e["count"].as_i64().unwrap_or(1), s(e, "message"));
            if let Some(f) = e["file"].as_str() {
                l.push_str(&format!(" ({f}:{})", e["line"].as_i64().unwrap_or(0)));
            }
            l.push_str(&format!("; first {}, last {}", clock(e["first_ms"].as_i64().unwrap_or(0)), clock(e["last_ms"].as_i64().unwrap_or(0))));
            let c = &e["culprit"];
            if c.is_object() {
                l.push_str(&format!(
                    "\n   probably {}'s edit to {} at {} ({}; line {})",
                    c["agent"]["name"].as_str().unwrap_or(c["agent_id"].as_str().unwrap_or("?")),
                    s(c, "file"),
                    clock(c["edit_ms"].as_i64().unwrap_or(0)),
                    if c["how"] == "stack" { "the stack names this file" } else { "the last edit before it" },
                    c["line"].as_i64().map_or("?".into(), |n| n.to_string())
                ));
            }
            let frames: Vec<String> = e["frames"].as_array().into_iter().flatten().filter(|f| f["in_repo"] == true).take(3).map(|f| format!("{}:{}", s(f, "file"), f["line"])).collect();
            if !frames.is_empty() {
                l.push_str(&format!("\n   stack: {}", frames.join(" < ")));
            }
            l
        })
        .collect();
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_of_v8_firefox_python_and_bare_lines() {
        assert_eq!(parse_frame("    at formatMoney (http://localhost:5173/src/lib/money.ts?t=17:8:31)"), Some((Some("formatMoney".into()), "http://localhost:5173/src/lib/money.ts?t=17".into(), 8, Some(31))));
        assert_eq!(parse_frame("    at async handle (/w/api/src/server/router.ts:41:5)"), Some((Some("handle".into()), "/w/api/src/server/router.ts".into(), 41, Some(5))));
        assert_eq!(parse_frame("    at /w/x.js:3:1"), Some((None, "/w/x.js".into(), 3, Some(1))));
        assert_eq!(parse_frame("render@http://127.0.0.1:5173/preview/a1/src/App.tsx:12:7"), Some((Some("render".into()), "http://127.0.0.1:5173/preview/a1/src/App.tsx".into(), 12, Some(7))));
        assert_eq!(parse_frame("  File \"/w/app/views.py\", line 88, in create_order"), Some((Some("create_order".into()), "/w/app/views.py".into(), 88, None)));
        assert_eq!(parse_frame("thread 'main' panicked at src/main.rs:12:5:"), None);
        assert_eq!(parse_frame("panicked at src/main.rs:12:5"), Some((None, "src/main.rs".into(), 12, Some(5))));
        assert_eq!(parse_frame("TypeError: x is undefined"), None);
        let pre = vec!["/preview/a1/".to_string(), "/base/".to_string()];
        assert_eq!(loc_path("http://localhost:5173/src/lib/money.ts?t=17", &pre), "/src/lib/money.ts");
        assert_eq!(loc_path("http://127.0.0.1:7740/preview/a1/src/App.tsx", &pre), "/src/App.tsx");
        assert_eq!(loc_path("http://localhost:5173/base/src/App.tsx", &pre), "/src/App.tsx");
        assert_eq!(loc_path("webpack-internal:///./src/a.js", &pre), "src/a.js");
        assert_eq!(loc_path("http://localhost:5173/@fs/home/u/w/src/a.ts", &pre), "/home/u/w/src/a.ts");
        assert_eq!(mask("row 12 of 0x3f failed in 250ms"), "row # of #x#f failed in #ms");
        assert_eq!(first_line("<html><head><title>T</title></head><body><h1>Oops</h1><p>x</p></body></html>"), "Oops");
        assert_eq!(first_line("\n  Error: boom\n at x"), "Error: boom");
    }
}
