//! Suggested rules and every grant (JOURNEYS.md, "Rules and notifications";
//! the UX critique's issue 2 and its power-user recommendation 3).
//!
//! From the permission prompts you approved again and again (the questions
//! table, checked against the events table when a prompt was answered in its
//! terminal), each group of one tool and one command pattern gets the
//! narrowest rule that covers it: `Bash(python3 -m pytest:*)`, never
//! `Bash(python3:*)`. Each comes with what it would have done over the last
//! 7 and 30 days: every distinct command it would have allowed (the risky
//! ones flagged: migrations, deleting files, the network, writes outside
//! the worktree), the prompts it would have removed and the agents' wait it
//! would have saved. A rule never covers a command you said No to.
//!
//! Accepting one writes it where Flow's "allow" already does: the mark
//! `allow:<repo>`, which `Supervisor::spawn` adds to `permissions.allow` of
//! each new agent's own session settings (`<data>/sessions/<id>/settings.json`,
//! `flow::with_allowed`). Nothing under `~/.claude`, and no settings file of
//! the user's, is ever written. Accept writes nothing unless the request
//! names the exact pattern the confirm sheet showed.
//!
//! `GET /api/rules/grants` lists every grant: Workbench's, the ones Claude
//! Code wrote to a worktree's `.claude/settings.local.json` when a prompt was
//! answered "don't ask again", and the session-wide ones (all edits, auto
//! mode); with who granted it, when, where, and how often it was used.
//! (`GET /api/rules` and `DELETE /api/rules/:id` were taken by Decide's
//! rules, so grants live under `/api/rules/grants`.)

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde_json::{json, Value as Json};

use crate::db::{get_ms, get_str, iso, now_ms, opt_str, ts, Agent};
use crate::questions::{self, Question};
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{one_line, ApiError, Supervisor};

/// Workbench's grants: a row per rule it wrote (accepted suggestions, the
/// grant sheet's narrower choice), kept after a revoke.
pub const TABLE: &str = "CREATE TABLE IF NOT EXISTS grants (id KEYWORD PRIMARY KEY, repo KEYWORD, pattern KEYWORD, via KEYWORD, granted_by KEYWORD, question KEYWORD, evidence TEXT OPTIONS(indexed = false), state KEYWORD, created TIMESTAMP, revoked TIMESTAMP)";

const DAY_MS: i64 = 24 * 3600 * 1000;
/// Approvals of one pattern in 30 days before it is suggested.
const MIN_APPROVALS: usize = 3;
/// The windows each suggestion is measured over, in days.
const WINDOWS: [i64; 2] = [7, 30];
/// The most commands a suggestion lists.
const LIST_MAX: usize = 40;

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/rules/suggested", get(suggested_route))
        .route("/api/rules/suggested/:id/accept", post(accept_route))
        .route("/api/rules/suggested/:id/dismiss", post(dismiss_route))
        .route("/api/rules/check", get(check_route))
        .route("/api/rules/grants", get(grants_route).post(grant_route))
        .route("/api/rules/grants/:id", delete(revoke_route))
}

// ------------------------------------------------------------ commands

/// The simple commands of a command line, split at `&&`, `||`, `;`, `|`
/// and newlines outside quotes, each as its words (quotes kept), without
/// leading `VAR=x` and wrappers that change nothing (`time`, `command`).
pub fn simple_commands(cmd: &str) -> Vec<Vec<String>> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let (mut sq, mut dq) = (false, false);
    let cs: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\'' if !dq => sq = !sq,
            '"' if !sq => dq = !dq,
            _ => {}
        }
        if !sq && !dq {
            let two = if i + 1 < cs.len() { Some((c, cs[i + 1])) } else { None };
            if matches!(two, Some(('&', '&')) | Some(('|', '|'))) {
                parts.push(std::mem::take(&mut cur));
                i += 2;
                continue;
            }
            if matches!(c, ';' | '|' | '\n') {
                parts.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
        }
        cur.push(c);
        i += 1;
    }
    parts.push(cur);
    parts
        .iter()
        .filter_map(|p| {
            let mut words = split_words(p);
            while words.first().is_some_and(|w| (w.contains('=') && !w.starts_with('=') && !w.starts_with('-') && !w.contains(['\'', '"'])) || matches!(w.as_str(), "time" | "command" | "builtin")) {
                words.remove(0);
            }
            (!words.is_empty()).then_some(words)
        })
        .collect()
}

/// Words split at whitespace outside quotes; the quotes stay in the words.
fn split_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut sq, mut dq) = (false, false);
    for c in s.chars() {
        match c {
            '\'' if !dq => sq = !sq,
            '"' if !sq => dq = !dq,
            _ => {}
        }
        if c.is_whitespace() && !sq && !dq {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The simple commands that do something: `cd <dir>` alone is left out,
/// as Claude Code leaves it out when it checks a compound command.
fn doing(cmd: &str) -> Vec<Vec<String>> {
    simple_commands(cmd).into_iter().filter(|w| !(w[0] == "cd" && w.len() <= 2)).collect()
}

/// A word a prefix rule may hold: no quotes, variables, globs, redirects.
fn plain(w: &str) -> bool {
    !w.is_empty() && !w.contains(['"', '\'', '$', '`', '(', ')', '*', '?', '>', '<', '&', '\\', '{', '}', '[', ']', '!', '~'])
}

/// Programs no rule is ever suggested for: they delete, escalate, reach
/// the network, run arbitrary code or wrap another command.
const NEVER: [&str; 52] = [
    "rm", "rmdir", "unlink", "shred", "truncate", "sudo", "su", "doas", "dd", "mkfs", "chmod", "chown", "chgrp", "curl", "wget", "ssh", "scp", "sftp", "rsync", "nc", "ncat", "telnet", "ftp", "kill",
    "pkill", "killall", "shutdown", "reboot", "eval", "exec", "source", ".", "sh", "bash", "zsh", "fish", "dash", "xargs", "env", "nohup", "timeout", "watch", "find", "mv", "cp", "tee", "ln", "crontab",
    "systemctl", "launchctl", "osascript", "open",
];
/// Infrastructure tools: what they change is not the worktree.
const INFRA: [&str; 12] = ["docker", "podman", "kubectl", "helm", "terraform", "aws", "gcloud", "az", "heroku", "fly", "flyctl", "vercel"];
/// Interpreters: a rule may name a module (`-m`) or a script, never the
/// interpreter alone (that is any program at all).
fn interpreter(w: &str) -> bool {
    w.starts_with("python") || matches!(w, "node" | "ruby" | "perl" | "php" | "Rscript" | "lua" | "julia")
}
/// Two words whose third is the real command (`npm run test`, `uv run pytest`).
fn wrapper(a: &str, b: &str) -> bool {
    matches!((a, b), ("npm" | "pnpm" | "yarn" | "bun", "run") | ("uv" | "poetry" | "pdm" | "hatch" | "pipenv" | "rye", "run") | ("bundle", "exec") | ("npm" | "pnpm", "exec"))
}
/// git subcommands no rule is suggested for: they reach the network or drop work.
const GIT_NEVER: [&str; 12] = ["push", "pull", "fetch", "clone", "reset", "clean", "rm", "config", "remote", "filter-branch", "gc", "update-ref"];

/// The head of a simple command: the words that say which program does
/// what (`python3 -m pytest`, `npm run lint`, `cargo test`, `pytest`), or
/// `None` for one no rule is suggested for.
pub fn head(words: &[String]) -> Option<Vec<String>> {
    let w0 = words.first()?.as_str();
    if NEVER.contains(&w0) || INFRA.contains(&w0) || !(plain(w0) || (w0.starts_with("./") && plain(&w0[2..]))) {
        return None;
    }
    let w1 = words.get(1).map(String::as_str);
    if interpreter(w0) {
        return match w1 {
            Some("-m") => words.get(2).filter(|m| plain(m) && !m.starts_with('-')).map(|m| vec![w0.into(), "-m".into(), m.clone()]),
            Some(s) if !s.starts_with('-') && plain(s) && (s.contains('/') || s.contains('.')) => Some(vec![w0.into(), s.into()]),
            _ => None,
        };
    }
    if let Some(b) = w1 {
        if wrapper(w0, b) {
            let rest = head(&words[2..])?;
            return Some([vec![w0.to_string(), b.to_string()], rest].concat());
        }
    }
    if w0 == "git" {
        let sub = w1?;
        return (plain(sub) && !sub.starts_with('-') && !GIT_NEVER.contains(&sub)).then(|| vec!["git".into(), sub.into()]);
    }
    if w0 == "npx" || w0 == "pnpx" || w0 == "bunx" {
        return w1.filter(|s| plain(s) && !s.starts_with('-')).map(|s| vec![w0.into(), s.into()]);
    }
    match w1 {
        // A subcommand (`cargo test`, `make lint`), not an argument (a path).
        Some(s) if plain(s) && !s.starts_with('-') && !s.contains(['/', '.']) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':') => Some(vec![w0.into(), s.into()]),
        _ => Some(vec![w0.into()]),
    }
}

/// What a command does that a rule should make you look twice at.
pub fn risks(cmd: &str, cwd: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    let mut add = |r: &'static str| {
        if !out.contains(&r) {
            out.push(r);
        }
    };
    let low = cmd.to_lowercase();
    if ["migrat", "alembic upgrade", "alembic downgrade", "db:migrate", "flyway", "liquibase", "prisma db push", "schema:update", "db:drop", "db:reset"].iter().any(|k| low.contains(k)) {
        add("migration");
    }
    if ["drop table", "drop database", "truncate table", "delete from"].iter().any(|k| low.contains(k)) {
        add("changes a database");
    }
    if low.contains("http://") || low.contains("https://") {
        add("network");
    }
    for w in simple_commands(cmd) {
        let w0 = w[0].as_str();
        let w1 = w.get(1).map(String::as_str).unwrap_or("");
        if matches!(w0, "rm" | "rmdir" | "unlink" | "shred" | "truncate") || (w0 == "git" && matches!(w1, "rm" | "clean")) || (w0 == "find" && w.iter().any(|x| x == "-delete")) {
            add("deletes files");
        }
        if matches!(w0, "curl" | "wget" | "ssh" | "scp" | "sftp" | "rsync" | "nc" | "ncat" | "telnet" | "ftp" | "http" | "https")
            || (w0 == "git" && matches!(w1, "push" | "pull" | "fetch" | "clone"))
            || (matches!(w0, "pip" | "pip3" | "npm" | "pnpm" | "yarn" | "gem" | "cargo" | "go" | "brew" | "apt" | "apt-get" | "uv") && matches!(w1, "install" | "add" | "i" | "get" | "update" | "upgrade"))
        {
            add("network");
        }
        if matches!(w0, "sudo" | "su" | "doas") {
            add("runs as root");
        }
        // Where it writes: after a redirect, tee's files, cp/mv's target, -o.
        let mut targets: Vec<&str> = Vec::new();
        for (i, x) in w.iter().enumerate() {
            if let Some(t) = x.strip_prefix(">>").or_else(|| x.strip_prefix('>')).or_else(|| x.strip_prefix("2>")) {
                if t.is_empty() {
                    if let Some(n) = w.get(i + 1) {
                        targets.push(n);
                    }
                } else {
                    targets.push(t);
                }
            }
            if matches!(x.as_str(), "-o" | "--output" | "--out" | "--db" | "--database") {
                if let Some(n) = w.get(i + 1) {
                    targets.push(n);
                }
            }
        }
        if w0 == "tee" {
            targets.extend(w[1..].iter().filter(|x| !x.starts_with('-')).map(String::as_str));
        }
        if matches!(w0, "cp" | "mv" | "install" | "ln") && w.len() >= 3 {
            targets.push(w.last().unwrap());
        }
        if targets.iter().any(|t| outside(t.trim_matches(['"', '\'']), cwd)) {
            add("writes outside the worktree");
        }
    }
    out
}

/// Whether a path a command writes is outside `cwd`.
fn outside(t: &str, cwd: &str) -> bool {
    if t.is_empty() || t.starts_with("/dev/") || t == "&1" || t == "&2" {
        return false;
    }
    if t.starts_with('~') || t.starts_with("$HOME") || t.starts_with("../") || t == ".." {
        return true;
    }
    t.starts_with('/') && (cwd.is_empty() || !Path::new(t).starts_with(cwd))
}

// ------------------------------------------------------------ rules

/// A permission rule as Claude Code writes it.
#[derive(Clone, Debug, PartialEq)]
pub enum Rule {
    /// `Bash(<words>:*)` (or `Bash(<words> *)`): commands that start with those words.
    BashPrefix(Vec<String>),
    /// `Bash(<command>)`: that command and nothing else.
    BashExact(String),
    /// `Bash`, `Bash(*)`: every command.
    BashAll,
    /// `WebFetch(domain:<host>)`.
    Domain(String),
    /// `Edit(<glob>)`, `Write(...)`, `Read(...)`: files under a folder.
    Files { tool: String, dir: String },
    /// A tool by name (`mcp__x__y`, `mcp__x`, `WebSearch`).
    Tool(String),
}

impl Rule {
    pub fn parse(p: &str) -> Option<Rule> {
        let p = p.trim();
        if p.is_empty() || p.len() > 400 || p.contains(['\n', '\r']) {
            return None;
        }
        if p == "Bash" || p == "Bash(*)" || p == "Bash(:*)" {
            return Some(Rule::BashAll);
        }
        if let Some(inner) = p.strip_prefix("Bash(").and_then(|s| s.strip_suffix(')')) {
            if let Some(pre) = inner.strip_suffix(":*").or_else(|| inner.strip_suffix(" *")) {
                let words = split_words(pre);
                return (!words.is_empty()).then_some(Rule::BashPrefix(words));
            }
            return (!inner.trim().is_empty()).then(|| Rule::BashExact(inner.trim().to_string()));
        }
        if let Some(d) = p.strip_prefix("WebFetch(domain:").and_then(|s| s.strip_suffix(')')) {
            return (!d.is_empty()).then(|| Rule::Domain(d.to_string()));
        }
        for tool in ["Edit", "Write", "MultiEdit", "Read", "NotebookEdit"] {
            if let Some(g) = p.strip_prefix(tool).and_then(|s| s.strip_prefix('(')).and_then(|s| s.strip_suffix(')')) {
                let g = g.strip_prefix("//").map(|x| format!("/{x}")).unwrap_or_else(|| g.to_string());
                let dir = g.trim_end_matches("/**").trim_end_matches("/*").to_string();
                return Some(Rule::Files { tool: tool.into(), dir });
            }
        }
        (p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then(|| Rule::Tool(p.to_string()))
    }

    pub fn text(&self) -> String {
        match self {
            Rule::BashPrefix(w) => format!("Bash({}:*)", w.join(" ")),
            Rule::BashExact(c) => format!("Bash({c})"),
            Rule::BashAll => "Bash".into(),
            Rule::Domain(d) => format!("WebFetch(domain:{d})"),
            // Claude Code's absolute paths start with `//`.
            Rule::Files { tool, dir } if dir.starts_with('/') => format!("{tool}(/{dir}/**)"),
            Rule::Files { tool, dir } => format!("{tool}({dir}/**)"),
            Rule::Tool(t) => t.clone(),
        }
    }

    /// Whether the rule allows a call: its tool, and its command, file or URL.
    pub fn covers(&self, c: &Call) -> bool {
        match self {
            Rule::BashAll => c.tool == "Bash",
            Rule::BashExact(x) => c.tool == "Bash" && c.what.trim() == x,
            Rule::BashPrefix(p) => {
                if c.tool != "Bash" {
                    return false;
                }
                let parts = doing(&c.what);
                !parts.is_empty() && parts.iter().all(|w| w.len() >= p.len() && p.iter().zip(w).all(|(a, b)| a == b))
            }
            Rule::Domain(d) => c.tool == "WebFetch" && host_of(&c.what).is_some_and(|h| h == *d),
            Rule::Files { tool, dir } => {
                let edit = matches!(c.tool.as_str(), "Edit" | "Write" | "MultiEdit" | "NotebookEdit");
                (if tool == "Read" { c.tool == "Read" } else { edit }) && Path::new(&c.what).starts_with(dir)
            }
            Rule::Tool(t) => c.tool == *t || (t.starts_with("mcp__") && t.matches("__").count() == 1 && c.tool.starts_with(&format!("{t}__"))),
        }
    }

    /// Why a rule is wider than any run of approvals can justify, if it is.
    pub fn too_wide(&self) -> Option<String> {
        match self {
            Rule::BashAll => Some("it allows every command".into()),
            Rule::BashPrefix(w) => {
                let w0 = w[0].as_str();
                if NEVER.contains(&w0) || INFRA.contains(&w0) {
                    Some(format!("`{w0}` can delete, escalate or reach the network"))
                } else if interpreter(w0) && w.len() == 1 {
                    Some(format!("`{w0}` alone runs any program at all; name a module (`{w0} -m pytest`) or a script"))
                } else if w.len() == 1 && matches!(w0, "npm" | "pnpm" | "yarn" | "bun" | "uv" | "poetry" | "npx" | "cargo" | "go" | "git" | "make" | "bundle") {
                    Some(format!("`{w0}` alone covers all of its subcommands; name one (`{w0} test`)"))
                } else if w.len() == 2 && wrapper(w0, &w[1]) {
                    Some(format!("`{} {}` alone runs any script; name it", w0, w[1]))
                } else {
                    None
                }
            }
            Rule::Files { dir, .. } if dir == "/" || dir.is_empty() => Some("it covers every file".into()),
            _ => None,
        }
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next()?.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_lowercase())
}

// ------------------------------------------------------------ evidence

/// A tool call: who made it, where, and what (the command, file or URL).
#[derive(Clone, Debug)]
pub struct Call {
    pub agent: String,
    pub cwd: String,
    pub tool: String,
    pub what: String,
    pub ts: i64,
}

/// A permission prompt and how it ended.
#[derive(Clone, Debug)]
struct Prompt {
    call: Call,
    asked: i64,
    wait_ms: i64,
    /// `Some(true)` approved, `Some(false)` refused, `None` not known.
    approved: Option<bool>,
}

/// The call a question's detail names.
fn call_of(q: &Question, a: Option<&Agent>) -> Option<Call> {
    let d = q.detail.as_ref()?;
    let tool = d["tool"].as_str().filter(|t| !t.is_empty())?.to_string();
    let input: Json = d["input"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Json::Null);
    let what = match tool.as_str() {
        "Bash" => d["command"].as_str()?.to_string(),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Read" => d["file"].as_str().or(input["file_path"].as_str())?.to_string(),
        "WebFetch" => input["url"].as_str()?.to_string(),
        _ => String::new(),
    };
    Some(Call { agent: q.agent.clone().unwrap_or_default(), cwd: a.map(|a| a.cwd.clone()).unwrap_or_default(), tool, what, ts: q.created })
}

/// What an event of a tool call holds as a [`Call`].
fn event_call(agent: &Agent, tool: &str, data: &Json, at: i64) -> Option<Call> {
    let i = &data["tool_input"];
    let what = match tool {
        "Bash" => i["command"].as_str()?.to_string(),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Read" => i["file_path"].as_str().or(i["notebook_path"].as_str())?.to_string(),
        "WebFetch" => i["url"].as_str()?.to_string(),
        _ => String::new(),
    };
    Some(Call { agent: agent.id.clone(), cwd: agent.cwd.clone(), tool: tool.to_string(), what, ts: at })
}

/// A repository's evidence over the last 30 days.
struct Evidence {
    repo: String,
    agents: HashMap<String, Agent>,
    prompts: Vec<Prompt>,
    /// Every tool call its agents started (PreToolUse), asked about or not.
    calls: Vec<Call>,
    now: i64,
}

fn evidence(sup: &Supervisor, repo: &str) -> Evidence {
    let now = now_ms();
    let since = now - 30 * DAY_MS;
    let agents: HashMap<String, Agent> = sup.agent_rows().into_iter().filter(|a| a.repo == repo).map(|a| (a.id.clone(), a)).collect();
    let mut calls = Vec::new();
    // What each agent did after a prompt it got: (ts, kind, tool, command).
    let mut after: HashMap<String, Vec<(i64, String, String, Option<String>)>> = HashMap::new();
    for a in agents.values() {
        let p = Params::new().set("agent", Value::Str(a.id.clone())).set("since", ts(since));
        let rows = sup
            .db()
            .query("SELECT ts, kind, tool, data FROM events WHERE agent = :agent AND ts >= :since AND kind IN ('PreToolUse', 'PostToolUse', 'PostToolUseFailure', 'UserPromptSubmit', 'Stop', 'exited') ORDER BY seq ASC LIMIT 200000", &p)
            .unwrap_or_default();
        let list = after.entry(a.id.clone()).or_default();
        for r in &rows {
            let at = get_ms(&r[0]).unwrap_or(0);
            let kind = get_str(&r[1]).unwrap_or_default();
            let tool = get_str(&r[2]).unwrap_or_default();
            let data: Json = get_str(&r[3]).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Json::Null);
            let cmd = data["tool_input"]["command"].as_str().map(str::to_string);
            if kind == "PreToolUse" {
                if let Some(c) = event_call(a, &tool, &data, at) {
                    calls.push(c);
                }
            }
            list.push((at, kind, tool, cmd));
        }
    }
    let mut prompts = Vec::new();
    for q in questions::all(sup) {
        if q.kind != "permission" || q.repo.as_deref() != Some(repo) || q.created < since {
            continue;
        }
        let a = q.agent.as_deref().and_then(|id| agents.get(id));
        let Some(call) = call_of(&q, a) else { continue };
        let answered = q.answered.unwrap_or(now);
        let by = q.answered_by.as_deref().unwrap_or("");
        let approved = match (q.state.as_str(), by) {
            ("answered", "you") => {
                let l = q.answer.as_deref().unwrap_or("").to_lowercase();
                Some(l.starts_with("yes"))
            }
            ("answered", "terminal") => {
                // What the agent did next: the call ran (approved), or a
                // new prompt or the turn's end came first (refused).
                let evs = after.get(&call.agent).map(Vec::as_slice).unwrap_or(&[]);
                let next = evs.iter().find(|(at, kind, tool, cmd)| {
                    *at >= q.created && (matches!(kind.as_str(), "UserPromptSubmit" | "Stop" | "exited") || (kind.starts_with("PostToolUse") && *tool == call.tool && (call.tool != "Bash" || cmd.as_deref().is_none_or(|c| c == call.what))))
                });
                next.map(|(_, kind, _, _)| kind.starts_with("PostToolUse"))
            }
            _ => None,
        };
        prompts.push(Prompt { call, asked: q.created, wait_ms: (answered - q.created).max(0), approved });
    }
    // Calls the questions table knows and the events do not (trimmed data).
    for p in &prompts {
        if !calls.iter().any(|c| c.agent == p.call.agent && c.tool == p.call.tool && c.what == p.call.what) {
            calls.push(p.call.clone());
        }
    }
    Evidence { repo: repo.to_string(), agents, prompts, calls, now }
}

/// The narrowest rule covering a group of calls of one tool.
fn narrowest(tool: &str, calls: &[&Call]) -> Option<Rule> {
    if calls.is_empty() {
        return None;
    }
    match tool {
        "Bash" => {
            let firsts: Vec<Vec<String>> = calls.iter().filter_map(|c| doing(&c.what).into_iter().next()).collect();
            if firsts.len() != calls.len() {
                return None;
            }
            let h = head(&firsts[0])?;
            // The words every command starts with, up to the first that is
            // not plain.
            let mut n = 0;
            while firsts.iter().all(|w| w.len() > n && w[n] == firsts[0][n] && plain(&w[n])) {
                n += 1;
            }
            if n < h.len() {
                return None;
            }
            let single = calls.iter().all(|c| doing(&c.what).len() == 1);
            let same = calls.iter().all(|c| c.what.trim() == calls[0].what.trim());
            if same && single && firsts[0].len() == n {
                return Some(Rule::BashExact(calls[0].what.trim().to_string()));
            }
            Some(Rule::BashPrefix(firsts[0][..n].to_vec()))
        }
        "WebFetch" => {
            let hosts: HashSet<String> = calls.iter().filter_map(|c| host_of(&c.what)).collect();
            (hosts.len() == 1).then(|| Rule::Domain(hosts.into_iter().next().unwrap()))
        }
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let dirs: Vec<PathBuf> = calls.iter().map(|c| Path::new(&c.what).parent().map(Path::to_path_buf).unwrap_or_default()).collect();
            let mut common = dirs[0].clone();
            while !dirs.iter().all(|d| d.starts_with(&common)) {
                if !common.pop() {
                    return None;
                }
            }
            let dir = common.to_string_lossy().to_string();
            (dir.len() > 1).then(|| Rule::Files { tool: "Edit".into(), dir })
        }
        t if t.starts_with("mcp__") || matches!(t, "WebSearch") => Some(Rule::Tool(t.to_string())),
        _ => None,
    }
}

/// The key prompts are grouped by: the tool and its command's head (or
/// host, folder, tool name).
fn group_key(c: &Call) -> Option<String> {
    match c.tool.as_str() {
        "Bash" => {
            let parts = doing(&c.what);
            let h = head(parts.first()?)?;
            Some(format!("Bash\u{1}{}", h.join(" ")))
        }
        "WebFetch" => host_of(&c.what).map(|h| format!("WebFetch\u{1}{h}")),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => Path::new(&c.what).parent().map(|p| format!("Edit\u{1}{}", p.display())),
        t if t.starts_with("mcp__") || t == "WebSearch" => Some(t.to_string()),
        _ => None,
    }
}

fn names_of(ev: &Evidence, ids: &BTreeMap<String, ()>) -> Vec<String> {
    ids.keys().map(|id| ev.agents.get(id).map(|a| a.name.clone()).unwrap_or_else(|| id.clone())).collect()
}

/// What a rule would have done over the last `days`: the prompts it
/// removes, the wait it saves, every distinct call it allows.
fn measure(ev: &Evidence, rule: &Rule, days: i64) -> Json {
    let from = ev.now - days * DAY_MS;
    let ps: Vec<&Prompt> = ev.prompts.iter().filter(|p| p.asked >= from && rule.covers(&p.call)).collect();
    let approved: Vec<&&Prompt> = ps.iter().filter(|p| p.approved == Some(true)).collect();
    let refused = ps.iter().filter(|p| p.approved == Some(false)).count();
    let all_prompts = ev.prompts.iter().filter(|p| p.asked >= from).count();
    let all_wait: i64 = ev.prompts.iter().filter(|p| p.asked >= from).map(|p| p.wait_ms).sum();
    let mut distinct: BTreeMap<String, (usize, i64, BTreeMap<String, ()>, bool, String)> = BTreeMap::new();
    for c in ev.calls.iter().filter(|c| c.ts >= from && rule.covers(c)) {
        let e = distinct.entry(c.what.clone()).or_insert_with(|| (0, 0, BTreeMap::new(), false, c.cwd.clone()));
        e.0 += 1;
        e.1 = e.1.max(c.ts);
        e.2.insert(c.agent.clone(), ());
    }
    for p in &ps {
        if let Some(e) = distinct.get_mut(&p.call.what) {
            e.3 = true;
        }
    }
    let mut cmds: Vec<Json> = distinct
        .iter()
        .map(|(what, (n, last, ids, asked, cwd))| {
            let r = if rule_is_bash(rule) { risks(what, cwd) } else { file_risks(what, cwd) };
            json!({ "command": what, "times": n, "last_ms": last, "agents": names_of(ev, ids), "asked": asked, "risky": !r.is_empty(), "risks": r })
        })
        .collect();
    cmds.sort_by(|a, b| b["risky"].as_bool().cmp(&a["risky"].as_bool()).then(b["last_ms"].as_i64().cmp(&a["last_ms"].as_i64())));
    let risky = cmds.iter().filter(|c| c["risky"] == true).count();
    let distinct_n = cmds.len();
    cmds.truncate(LIST_MAX);
    json!({
        "days": days,
        "prompts": approved.len(),
        "refused": refused,
        "wait_ms": approved.iter().map(|p| p.wait_ms).sum::<i64>(),
        "all_prompts": all_prompts,
        "all_wait_ms": all_wait,
        "distinct": distinct_n,
        "risky": risky,
        "commands": cmds,
    })
}

fn rule_is_bash(r: &Rule) -> bool {
    matches!(r, Rule::BashPrefix(_) | Rule::BashExact(_) | Rule::BashAll)
}

fn file_risks(what: &str, cwd: &str) -> Vec<&'static str> {
    if what.starts_with('/') && outside(what, cwd) {
        vec!["writes outside the worktree"]
    } else if what.starts_with("http") {
        vec!["network"]
    } else {
        Vec::new()
    }
}

fn dismissed_key(repo: &str) -> String {
    format!("rulesuggest:dismissed:{repo}")
}

fn dismissed(sup: &Supervisor, repo: &str) -> Vec<String> {
    sup.mark(&dismissed_key(repo)).and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

fn hash(s: &str) -> String {
    crate::git::hash8(Path::new(s))
}

pub fn suggestion_id(repo: &str, pattern: &str) -> String {
    format!("rs{}", hash(&format!("{repo}\n{pattern}")))
}

/// The suggestions of a repository, most wait saved first.
fn suggestions(sup: &Supervisor, ev: &Evidence) -> Vec<Json> {
    let allowed: HashSet<String> = sup.allowed(&ev.repo).into_iter().collect();
    let gone: HashSet<String> = dismissed(sup, &ev.repo).into_iter().collect();
    let refused: Vec<&Prompt> = ev.prompts.iter().filter(|p| p.approved == Some(false)).collect();
    let mut groups: BTreeMap<String, Vec<&Prompt>> = BTreeMap::new();
    for p in ev.prompts.iter().filter(|p| p.approved == Some(true)) {
        if let Some(k) = group_key(&p.call) {
            groups.entry(k).or_default().push(p);
        }
    }
    let mut out = Vec::new();
    for (_, ps) in groups {
        let mut todo = vec![ps];
        let mut depth = 0;
        while let Some(ps) = todo.pop() {
            if ps.len() < MIN_APPROVALS {
                continue;
            }
            let tool = ps[0].call.tool.clone();
            let calls: Vec<&Call> = ps.iter().map(|p| &p.call).collect();
            let Some(rule) = narrowest(&tool, &calls) else { continue };
            if rule.too_wide().is_some() {
                continue;
            }
            // Never wider than the evidence: no command you refused.
            if refused.iter().any(|p| rule.covers(&p.call)) {
                // Split by the next word once, and try each part.
                if depth == 0 && tool == "Bash" {
                    depth += 1;
                    if let Rule::BashPrefix(w) = &rule {
                        let mut sub: BTreeMap<String, Vec<&Prompt>> = BTreeMap::new();
                        for p in &ps {
                            let first = doing(&p.call.what).into_iter().next().unwrap_or_default();
                            sub.entry(first.get(w.len()).cloned().unwrap_or_default()).or_default().push(p);
                        }
                        todo.extend(sub.into_values());
                    }
                }
                continue;
            }
            let pattern = rule.text();
            if allowed.contains(&pattern) || gone.contains(&pattern) {
                continue;
            }
            let windows: Vec<Json> = WINDOWS.iter().map(|d| measure(ev, &rule, *d)).collect();
            let w30 = &windows[1];
            let w7 = &windows[0];
            let ids: BTreeMap<String, ()> = ps.iter().map(|p| (p.call.agent.clone(), ())).collect();
            let label = match &rule {
                Rule::BashPrefix(w) => w.join(" "),
                Rule::BashExact(c) => one_line(c, 80),
                Rule::Domain(d) => d.clone(),
                Rule::Files { dir, .. } => format!("edits in {dir}"),
                Rule::Tool(t) => t.clone(),
                Rule::BashAll => "every command".into(),
            };
            let short = match &rule {
                Rule::BashPrefix(w) => w.iter().rev().find(|x| !x.starts_with('-')).cloned().unwrap_or_else(|| label.clone()),
                _ => label.clone(),
            };
            let n30 = w30["prompts"].as_u64().unwrap_or(0);
            let n7 = w7["prompts"].as_u64().unwrap_or(0);
            out.push(json!({
                "id": suggestion_id(&ev.repo, &pattern),
                "repo": ev.repo,
                "tool": tool,
                "pattern": pattern,
                "label": label,
                "short": short,
                "exact": matches!(rule, Rule::BashExact(_)),
                "agents": names_of(ev, &ids),
                "why": format!("You approved it {n30} time{} in 30 days ({n7} in the last 7), and it covers no command you said No to.", if n30 == 1 { "" } else { "s" }),
                "where": where_workbench(sup, &ev.repo),
                "windows": { "7": w7, "30": w30 },
                "risky": w30["risky"],
            }));
        }
    }
    out.sort_by_key(|s| -s["windows"]["30"]["wait_ms"].as_i64().unwrap_or(0));
    out
}

/// Where a Workbench grant is written, in words and as the files.
fn where_workbench(sup: &Supervisor, repo: &str) -> Json {
    let sessions = sup.cfg.data_dir.join("sessions");
    json!({
        "kind": "workbench",
        "text": format!("permissions.allow of each new agent's own session settings, for {}", base(repo)),
        "file": format!("{}/<agent>/settings.json", sessions.display()),
        "note": "Agents started from now on in this repository get it; running agents keep the settings they started with. Your ~/.claude and the project's .claude files are not touched.",
    })
}

fn base(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string())
}

fn repo_of(repo: Option<&str>) -> Result<String, ApiError> {
    Ok(crate::trail::repo_dir(repo)?.to_string_lossy().to_string())
}

/// `GET /api/rules/suggested?repo=`.
pub fn suggested(sup: &Supervisor, repo: Option<&str>) -> Result<Json, ApiError> {
    let repo = repo_of(repo)?;
    let ev = evidence(sup, &repo);
    let list = suggestions(sup, &ev);
    let w = |d: i64| -> (usize, i64) {
        let from = ev.now - d * DAY_MS;
        let ps: Vec<&Prompt> = ev.prompts.iter().filter(|p| p.asked >= from).collect();
        (ps.len(), ps.iter().map(|p| p.wait_ms).sum())
    };
    let (p7, w7) = w(7);
    let (p30, w30) = w(30);
    Ok(json!({
        "repo": repo,
        "suggestions": list,
        "prompts": { "7": { "prompts": p7, "wait_ms": w7 }, "30": { "prompts": p30, "wait_ms": w30 } },
        "dismissed": dismissed(sup, &repo),
        "min_approvals": MIN_APPROVALS,
    }))
}

#[derive(serde::Deserialize, Default)]
struct AcceptB {
    repo: Option<String>,
    /// The exact pattern the confirm sheet showed.
    pattern: Option<String>,
}

/// `POST /api/rules/suggested/:id/accept {repo, pattern}`.
fn accept(sup: &Supervisor, id: &str, b: AcceptB) -> Result<Json, ApiError> {
    let repo = repo_of(b.repo.as_deref())?;
    let want = b.pattern.as_deref().map(str::trim).filter(|p| !p.is_empty()).ok_or_else(|| ApiError::bad("pattern is required: the exact rule the confirm sheet showed. Nothing was written."))?;
    let ev = evidence(sup, &repo);
    let list = suggestions(sup, &ev);
    let s = list.iter().find(|s| s["id"] == id).ok_or_else(|| ApiError::not_found(format!("no suggestion {id} for {repo} now (accepted, dismissed, or the evidence changed). Nothing was written.")))?;
    let pattern = s["pattern"].as_str().unwrap_or("");
    if pattern != want {
        return Err(ApiError::new(409, "pattern_changed", format!("Nothing was written: the suggestion is now {pattern}, not {want}. Review it again.")));
    }
    let evidence = json!({ "windows": s["windows"], "why": s["why"], "agents": s["agents"] });
    let g = add_grant(sup, &repo, pattern, "a suggested rule", None, Some(evidence))?;
    Ok(json!({ "ok": true, "grant": g, "allowed": sup.allowed(&repo), "where": s["where"] }))
}

/// Writes a Workbench grant: the pattern into `allow:<repo>` (what new
/// agents' settings get) and its row.
fn add_grant(sup: &Supervisor, repo: &str, pattern: &str, via: &str, question: Option<&str>, evidence: Option<Json>) -> Result<Json, ApiError> {
    let mut allowed = sup.allowed(repo);
    if !allowed.iter().any(|p| p == pattern) {
        allowed.push(pattern.to_string());
    }
    sup.set_mark(&format!("allow:{repo}"), &serde_json::to_string(&allowed).unwrap())?;
    let id = grant_id("wb", repo, pattern);
    let now = now_ms();
    let p = Params::new()
        .set("id", Value::Str(id.clone()))
        .set("repo", Value::Str(repo.to_string()))
        .set("pattern", Value::Str(pattern.to_string()))
        .set("via", Value::Str(via.to_string()))
        .set("granted_by", Value::Str("you".into()))
        .set("question", opt_str(&question.map(str::to_string)))
        .set("evidence", opt_str(&evidence.map(|e| e.to_string())))
        .set("state", Value::Str("active".into()))
        .set("created", ts(now))
        .set("revoked", Value::Null);
    sup.db()
        .query("INSERT OR REPLACE INTO grants (id, repo, pattern, via, granted_by, question, evidence, state, created, revoked) VALUES (:id, :repo, :pattern, :via, :granted_by, :question, :evidence, :state, :created, :revoked)", &p)
        .map_err(ApiError::internal)?;
    Ok(json!({ "id": id, "repo": repo, "pattern": pattern, "via": via, "granted_by": "you", "created_ms": now, "created": iso(now), "where": where_workbench(sup, repo) }))
}

fn grant_id(kind: &str, place: &str, pattern: &str) -> String {
    format!("g{}{}", &kind[..1], hash(&format!("{kind}\n{place}\n{pattern}")))
}

/// `POST /api/rules/suggested/:id/dismiss {repo}`.
fn dismiss(sup: &Supervisor, id: &str, b: AcceptB) -> Result<Json, ApiError> {
    let repo = repo_of(b.repo.as_deref())?;
    let ev = evidence(sup, &repo);
    let list = suggestions(sup, &ev);
    let s = list.iter().find(|s| s["id"] == id).ok_or_else(|| ApiError::not_found(format!("no suggestion {id} for {repo}")))?;
    let pattern = s["pattern"].as_str().unwrap_or("").to_string();
    let mut d = dismissed(sup, &repo);
    if !d.contains(&pattern) {
        d.push(pattern.clone());
    }
    sup.set_mark(&dismissed_key(&repo), &serde_json::to_string(&d).unwrap())?;
    Ok(json!({ "ok": true, "id": id, "pattern": pattern, "dismissed": d }))
}

// ------------------------------------------------------------ the grant sheet

#[derive(serde::Deserialize, Default)]
struct CheckQ {
    repo: Option<String>,
    pattern: Option<String>,
    agent: Option<String>,
    /// The command the prompt asks about.
    command: Option<String>,
}

/// `GET /api/rules/check?pattern=&agent=&repo=&command=`: what the grant
/// sheet shows before a wide option is typed: the rule, where it is
/// written, the commands it would also have allowed (risky first), and the
/// narrowest rule that covers this command instead.
fn check(sup: &Supervisor, q: CheckQ) -> Result<Json, ApiError> {
    let a = q.agent.as_deref().and_then(|id| sup.agent_row(id));
    let repo = match (q.repo.as_deref().filter(|r| !r.is_empty()), &a) {
        (Some(r), _) => repo_of(Some(r))?,
        (None, Some(a)) => a.repo.clone(),
        _ => return Err(ApiError::bad("repo or agent is required")),
    };
    let text = q.pattern.as_deref().map(str::trim).filter(|p| !p.is_empty()).ok_or_else(|| ApiError::bad("pattern is required"))?;
    let rule = Rule::parse(text).ok_or_else(|| ApiError::bad(format!("{text:?} is not a rule Workbench reads")))?;
    let ev = evidence(sup, &repo);
    let w30 = measure(&ev, &rule, 30);
    let file = a.as_ref().map(|a| format!("{}/.claude/settings.local.json", a.cwd));
    // The narrower choice: the narrowest rule over the approved prompts
    // that share this command's head, else this command alone.
    let cmd = q.command.as_deref().map(str::trim).filter(|c| !c.is_empty());
    let narrower = cmd.and_then(|c| {
        let me = Call { agent: a.as_ref().map(|a| a.id.clone()).unwrap_or_default(), cwd: a.as_ref().map(|a| a.cwd.clone()).unwrap_or_default(), tool: "Bash".into(), what: c.to_string(), ts: ev.now };
        let key = group_key(&me);
        let refused: Vec<&Prompt> = ev.prompts.iter().filter(|p| p.approved == Some(false)).collect();
        let mut same: Vec<&Call> = ev.prompts.iter().filter(|p| p.approved == Some(true) && key.is_some() && group_key(&p.call) == key).map(|p| &p.call).collect();
        same.push(&me);
        let r = narrowest("Bash", &same)
            .filter(|r| r.too_wide().is_none() && !refused.iter().any(|p| r.covers(&p.call)) && *r != rule)
            .unwrap_or_else(|| Rule::BashExact(c.to_string()));
        (r != rule).then(|| {
            let m = measure(&ev, &r, 30);
            json!({ "pattern": r.text(), "distinct": m["distinct"], "risky": m["risky"], "prompts": m["prompts"], "wait_ms": m["wait_ms"], "commands": m["commands"].as_array().map(|c| c.iter().take(3).cloned().collect::<Vec<_>>()) })
        })
    });
    Ok(json!({
        "repo": repo,
        "rule": rule.text(),
        "too_wide": rule.too_wide(),
        "file": file,
        "lasting": true,
        "window": w30,
        "narrower": narrower,
        "workbench_where": where_workbench(sup, &repo),
    }))
}

#[derive(serde::Deserialize, Default)]
struct GrantB {
    repo: Option<String>,
    pattern: Option<String>,
    question: Option<String>,
}

/// `POST /api/rules/grants {repo, pattern, question?}`: the grant sheet's
/// narrower choice, written for new agents of the repository.
fn grant(sup: &Supervisor, b: GrantB) -> Result<Json, ApiError> {
    let repo = repo_of(b.repo.as_deref())?;
    let text = b.pattern.as_deref().map(str::trim).filter(|p| !p.is_empty()).ok_or_else(|| ApiError::bad("pattern is required: the exact rule the sheet showed"))?;
    let rule = Rule::parse(text).ok_or_else(|| ApiError::bad(format!("{text:?} is not a rule Workbench writes")))?;
    if let Some(why) = rule.too_wide() {
        return Err(ApiError::bad(format!("Not written: {text} is too wide ({why}).")));
    }
    let g = add_grant(sup, &repo, &rule.text(), "the grant sheet", b.question.as_deref(), None)?;
    Ok(json!({ "ok": true, "grant": g, "allowed": sup.allowed(&repo) }))
}

// ------------------------------------------------------------ every grant

struct Row {
    id: String,
    pattern: String,
    via: String,
    question: Option<String>,
    state: String,
    created: i64,
    revoked: Option<i64>,
}

fn rows(sup: &Supervisor, repo: &str) -> Vec<Row> {
    let p = Params::new().set("repo", Value::Str(repo.to_string()));
    sup.db()
        .query("SELECT id, pattern, via, question, state, created, revoked FROM grants WHERE repo = :repo LIMIT 10000", &p)
        .unwrap_or_default()
        .iter()
        .map(|r| Row { id: get_str(&r[0]).unwrap_or_default(), pattern: get_str(&r[1]).unwrap_or_default(), via: get_str(&r[2]).unwrap_or_default(), question: get_str(&r[3]), state: get_str(&r[4]).unwrap_or_default(), created: get_ms(&r[5]).unwrap_or(0), revoked: get_ms(&r[6]) })
        .collect()
}

/// How often calls matched a rule since `from`, by agents `of` allows.
fn used(ev: &Evidence, rule: &Rule, from: i64, of: impl Fn(&Call) -> bool) -> (usize, Option<i64>) {
    let m: Vec<&Call> = ev.calls.iter().filter(|c| c.ts >= from && of(c) && rule.covers(c)).collect();
    (m.len(), m.iter().map(|c| c.ts).max())
}

/// The rule a "don't ask again" answer made, from Claude Code's label.
fn rule_of_answer(q: &Question) -> Option<String> {
    let l = q.answer.as_deref()?;
    let low = l.to_lowercase();
    let tool = q.detail.as_ref().and_then(|d| d["tool"].as_str()).unwrap_or("Bash");
    let at = low.find("don't ask again for ").map(|i| i + "don't ask again for ".len()).or_else(|| low.find("do not ask again for ").map(|i| i + "do not ask again for ".len()))?;
    let rest = &l[at..];
    let rest = rest.split(" in ").next().unwrap_or(rest).split(" commands").next().unwrap_or(rest).split(" command").next().unwrap_or(rest);
    let prefix = rest.replace(['`', '\'', '"', '“', '”'], "").trim().trim_end_matches([':', '*', '-']).trim().to_string();
    (!prefix.is_empty()).then(|| if tool == "Bash" { format!("Bash({prefix}:*)") } else { format!("{tool}({prefix})") })
}

/// `GET /api/rules/grants?repo=`.
pub fn grants(sup: &Supervisor, repo: Option<&str>) -> Result<Json, ApiError> {
    let repo = repo_of(repo)?;
    let ev = evidence(sup, &repo);
    let mut out: Vec<Json> = Vec::new();
    let rs = rows(sup, &repo);
    // Workbench's, in `allow:<repo>`.
    for pattern in sup.allowed(&repo) {
        let row = rs.iter().find(|r| r.pattern == pattern && r.state == "active");
        let flow = sup.mark(&format!("flow:{repo}:allow:{pattern}")).and_then(|v| serde_json::from_str::<Json>(&v).ok()).and_then(|v| v["ts"].as_i64());
        let created = row.map(|r| r.created).or(flow).unwrap_or(0);
        let via = row.map(|r| r.via.clone()).unwrap_or_else(|| "Flow's suggestion".into());
        let (n, last) = Rule::parse(&pattern).map(|r| used(&ev, &r, created, |c| ev.agents.get(&c.agent).is_some_and(|a| a.created >= created))).unwrap_or((0, None));
        out.push(json!({
            "id": grant_id("wb", &repo, &pattern), "kind": "workbench", "pattern": pattern, "repo": repo,
            "granted_by": "you", "via": via, "question": row.and_then(|r| r.question.clone()),
            "created_ms": (created > 0).then_some(created), "created": (created > 0).then(|| iso(created)),
            "where": where_workbench(sup, &repo), "used": n, "last_used_ms": last, "lasting": true, "revocable": true, "state": "active",
        }));
    }
    // Claude Code's, in each agent's folder.
    let qs: Vec<Question> = questions::all(sup).into_iter().filter(|q| q.repo.as_deref() == Some(repo.as_str()) && q.kind == "permission" && q.answered_by.as_deref() == Some("you")).collect();
    let mut cwds: Vec<String> = ev.agents.values().map(|a| a.cwd.clone()).collect();
    cwds.sort();
    cwds.dedup();
    for cwd in &cwds {
        let file = Path::new(cwd).join(".claude").join("settings.local.json");
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let Ok(v) = serde_json::from_str::<Json>(&text) else { continue };
        let mtime = std::fs::metadata(&file).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64);
        let mine = workbench_worktree(sup, cwd);
        for p in v.pointer("/permissions/allow").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str) {
            let q = qs.iter().filter(|q| q.agent.as_deref().and_then(|a| ev.agents.get(a)).is_some_and(|a| a.cwd == *cwd) && rule_of_answer(q).as_deref() == Some(p)).max_by_key(|q| q.answered);
            let created = q.and_then(|q| q.answered).or(mtime);
            let (n, last) = Rule::parse(p).map(|r| used(&ev, &r, created.unwrap_or(0), |c| c.cwd == *cwd)).unwrap_or((0, None));
            let who = q.and_then(|q| q.agent.as_deref()).and_then(|a| ev.agents.get(a)).map(|a| a.name.clone());
            out.push(json!({
                "id": grant_id("cc", &file.to_string_lossy(), p), "kind": "claude-code", "pattern": p, "repo": repo,
                "granted_by": if q.is_some() { "you" } else { "Claude Code" },
                "via": match &who { Some(n) => format!("“don't ask again” on {n}'s prompt"), None => "a “don't ask again” answer".into() },
                "question": q.map(|q| q.id.clone()), "created_ms": created, "created": created.map(iso),
                "where": { "kind": "claude-code", "text": format!("{}'s .claude/settings.local.json", base(cwd)), "file": file.to_string_lossy() },
                "used": n, "last_used_ms": last, "lasting": true, "revocable": mine, "state": "active",
                "how": (!mine).then(|| format!("Workbench does not edit your own checkout: remove the line from {} yourself.", file.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default())),
            }));
        }
    }
    // Session-wide answers: all edits, auto mode.
    for q in &qs {
        let l = q.answer.as_deref().unwrap_or("");
        let Some(says) = crate::screen::grants_more(l).filter(|_| rule_of_answer(q).is_none()) else { continue };
        let a = q.agent.as_deref().and_then(|id| ev.agents.get(id));
        let running = a.is_some_and(|a| sup.is_running(&a.id));
        out.push(json!({
            "id": grant_id("ss", q.agent.as_deref().unwrap_or(""), &q.id), "kind": "session", "pattern": one_line(l, 120), "repo": repo,
            "granted_by": "you", "via": format!("{}'s prompt", a.map(|a| a.name.as_str()).unwrap_or("an agent")), "question": q.id,
            "created_ms": q.answered, "created": q.answered.map(iso),
            "where": { "kind": "session", "text": format!("{}'s session only", a.map(|a| a.name.as_str()).unwrap_or("the agent")), "file": Json::Null },
            "says": says, "used": Json::Null, "lasting": false, "revocable": false, "state": if running { "active" } else { "ended" },
            "how": if running { "It ends with the session: stop or restart the agent to end it now." } else { "It ended with the session." },
        }));
    }
    let revoked: Vec<Json> = rs
        .iter()
        .filter(|r| r.state == "revoked")
        .map(|r| json!({ "id": r.id, "pattern": r.pattern, "via": r.via, "created_ms": r.created, "revoked_ms": r.revoked, "state": "revoked" }))
        .collect();
    Ok(json!({
        "repo": repo,
        "grants": out,
        "revoked": revoked,
        "defaults": {
            "text": "In a worktree Workbench made, agents edit and read files there, run the tests and use git locally without asking (unless started with “Ask before edits”). Guarded commands still stop.",
            "rules": ev.agents.values().find(|a| workbench_worktree(sup, &a.cwd)).map(|a| crate::flow::worktree_allow(Path::new(&a.cwd)).len()),
        },
    }))
}

fn workbench_worktree(sup: &Supervisor, cwd: &str) -> bool {
    let root = sup.cfg.data_dir.join("worktrees");
    let root = root.canonicalize().unwrap_or(root);
    !cwd.is_empty() && Path::new(cwd).starts_with(&root)
}

#[derive(serde::Deserialize, Default)]
struct RevokeQ {
    repo: Option<String>,
}

/// `DELETE /api/rules/grants/:id?repo=`.
fn revoke(sup: &Supervisor, id: &str, repo: Option<&str>) -> Result<Json, ApiError> {
    let list = grants(sup, repo)?;
    let repo = list["repo"].as_str().unwrap_or("").to_string();
    let g = list["grants"].as_array().into_iter().flatten().find(|g| g["id"] == id).cloned().ok_or_else(|| ApiError::not_found(format!("no grant {id} in {repo}")))?;
    let pattern = g["pattern"].as_str().unwrap_or("").to_string();
    match g["kind"].as_str() {
        Some("workbench") => {
            let allowed: Vec<String> = sup.allowed(&repo).into_iter().filter(|p| *p != pattern).collect();
            sup.set_mark(&format!("allow:{repo}"), &serde_json::to_string(&allowed).unwrap())?;
            let now = now_ms();
            let p = Params::new()
                .set("id", Value::Str(id.to_string()))
                .set("repo", Value::Str(repo.clone()))
                .set("pattern", Value::Str(pattern.clone()))
                .set("via", Value::Str(g["via"].as_str().unwrap_or("").to_string()))
                .set("granted_by", Value::Str("you".into()))
                .set("question", opt_str(&g["question"].as_str().map(str::to_string)))
                .set("state", Value::Str("revoked".into()))
                .set("created", g["created_ms"].as_i64().map_or(Value::Null, ts))
                .set("revoked", ts(now));
            sup.db()
                .query("INSERT OR REPLACE INTO grants (id, repo, pattern, via, granted_by, question, state, created, revoked) VALUES (:id, :repo, :pattern, :via, :granted_by, :question, :state, :created, :revoked)", &p)
                .map_err(ApiError::internal)?;
            Ok(json!({ "ok": true, "id": id, "pattern": pattern, "allowed": allowed, "note": "Agents started from now on ask again; running agents keep the settings they started with." }))
        }
        Some("claude-code") if g["revocable"] == true => {
            let file = PathBuf::from(g["where"]["file"].as_str().unwrap_or(""));
            let mut v: Json = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).ok_or_else(|| ApiError::conflict(format!("cannot read {}", file.display())))?;
            if let Some(list) = v.pointer_mut("/permissions/allow").and_then(Json::as_array_mut) {
                list.retain(|p| p.as_str() != Some(pattern.as_str()));
            }
            let text = serde_json::to_string_pretty(&v).unwrap();
            std::fs::write(&file, text + "\n").map_err(|e| ApiError::internal(format!("cannot write {}: {e}", file.display())))?;
            Ok(json!({ "ok": true, "id": id, "pattern": pattern, "file": file.to_string_lossy(), "note": "Removed from the worktree's settings: Claude Code asks again there." }))
        }
        _ => Err(ApiError::conflict(g["how"].as_str().unwrap_or("Workbench cannot revoke this grant").to_string())),
    }
}

// ------------------------------------------------------------ routes

#[derive(serde::Deserialize)]
struct RepoQ {
    repo: Option<String>,
}

async fn suggested_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || suggested(&sup, q.repo.as_deref())).await?).into_response())
}

async fn accept_route(State(app): State<App>, AxPath(id): AxPath<String>, body: Bytes) -> ApiResult {
    let b: AcceptB = if body.is_empty() { AcceptB::default() } else { parse_body(&body)? };
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || accept(&sup, &id, b)).await?).into_response())
}

async fn dismiss_route(State(app): State<App>, AxPath(id): AxPath<String>, body: Bytes) -> ApiResult {
    let b: AcceptB = if body.is_empty() { AcceptB::default() } else { parse_body(&body)? };
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || dismiss(&sup, &id, b)).await?).into_response())
}

async fn check_route(State(app): State<App>, Query(q): Query<CheckQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || check(&sup, q)).await?).into_response())
}

async fn grants_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || grants(&sup, q.repo.as_deref())).await?).into_response())
}

async fn grant_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: GrantB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || grant(&sup, b)).await?).into_response())
}

async fn revoke_route(State(app): State<App>, AxPath(id): AxPath<String>, Query(q): Query<RevokeQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || revoke(&sup, &id, q.repo.as_deref())).await?).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash(cmd: &str) -> Call {
        Call { agent: "a".into(), cwd: "/w/shop".into(), tool: "Bash".into(), what: cmd.into(), ts: 0 }
    }
    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn heads_name_the_program_and_what_it_does() {
        assert_eq!(head(&words("python3 -m pytest tests/a.py -q")), Some(words("python3 -m pytest")));
        assert_eq!(head(&words("python3 scripts/check.py --all")), Some(words("python3 scripts/check.py")));
        assert_eq!(head(&words("python3 -c print(1)")), None);
        assert_eq!(head(&words("npm run test -- --watch")), Some(words("npm run test")));
        assert_eq!(head(&words("uv run pytest -x")), Some(words("uv run pytest")));
        assert_eq!(head(&words("uv run python -c x")), None);
        assert_eq!(head(&words("cargo test -p calc")), Some(words("cargo test")));
        assert_eq!(head(&words("pytest tests/x.py")), Some(words("pytest")));
        assert_eq!(head(&words("rm -rf target")), None);
        assert_eq!(head(&words("git push origin main")), None);
        assert_eq!(head(&words("git status")), Some(words("git status")));
        assert_eq!(head(&words("curl https://x")), None);
    }

    #[test]
    fn the_narrowest_rule_covers_the_group_and_no_more() {
        let a = bash("python3 -m pytest tests/test_promo.py -q");
        let b = bash("cd /w/shop && python3 -m pytest tests/test_cart.py");
        assert_eq!(narrowest("Bash", &[&a, &b]).unwrap().text(), "Bash(python3 -m pytest:*)");
        assert_eq!(narrowest("Bash", &[&a, &a]).unwrap().text(), "Bash(python3 -m pytest tests/test_promo.py -q)");
        let c = bash("python3 -m pytest tests/test_promo.py -k rounding");
        assert_eq!(narrowest("Bash", &[&a, &c]).unwrap().text(), "Bash(python3 -m pytest tests/test_promo.py:*)");
        let r = Rule::parse("Bash(python3 -m pytest:*)").unwrap();
        assert!(r.covers(&a) && r.covers(&b));
        assert!(!r.covers(&bash("python3 -m orders.migrate --db dev.sqlite3")));
        assert!(!r.covers(&bash("python3 -m pytest && rm -rf /")));
        assert!(Rule::parse("Bash(python3:*)").unwrap().too_wide().is_some());
        assert!(Rule::parse("Bash(python3:*)").unwrap().covers(&bash("python3 -m orders.migrate")));
        assert!(Rule::parse("Bash(python3 -m pytest:*)").unwrap().too_wide().is_none());
    }

    #[test]
    fn risky_commands_say_why() {
        assert_eq!(risks("python3 -m orders.migrate --db dev.sqlite3", "/w/shop"), ["migration"]);
        assert_eq!(risks("rm -rf build", "/w/shop"), ["deletes files"]);
        assert_eq!(risks("curl -s https://api.example.com", "/w/shop"), ["network", ]);
        assert_eq!(risks("python3 gen.py > /etc/hosts", "/w/shop"), ["writes outside the worktree"]);
        assert!(risks("python3 gen.py > out.txt", "/w/shop").is_empty());
        assert!(risks("python3 -m pytest -q 2>&1", "/w/shop").is_empty());
        assert_eq!(risks("python3 -m pytest --db /w/shop/x.db", "/w/shop"), Vec::<&str>::new());
    }

    #[test]
    fn a_dont_ask_again_answer_names_its_rule() {
        let q = Question { answer: Some("Yes, and don't ask again for python3 commands in /w/shop".into()), detail: Some(json!({ "tool": "Bash" })), ..Question::default() };
        assert_eq!(rule_of_answer(&q).as_deref(), Some("Bash(python3:*)"));
    }
}
