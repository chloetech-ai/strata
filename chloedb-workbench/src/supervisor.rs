//! The supervisor: each agent's process in a pseudo-terminal, the last
//! 256 KiB of its output, its state as the hooks report it, and the rows
//! of the state database that record all of it.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value as Json};
use tokio::sync::broadcast;

use crate::db::{now_ms, Agent, Event, StateDb};
use crate::git;
use crate::recording::{Recorder, Wrote};

/// What a terminal keeps of its output for a WebSocket that attaches.
pub const SCROLLBACK: usize = 256 * 1024;
/// What an event keeps of its hook's JSON.
pub const DATA_MAX: usize = 16 * 1024;
const COLS: u16 = 120;
const ROWS: u16 = 32;

pub struct Config {
    pub data_dir: PathBuf,
    pub claude_bin: String,
    pub max_agents: usize,
    pub max_depth: i64,
    pub token: String,
    /// The server's own URL, `http://127.0.0.1:<port>`, which sessions'
    /// hooks and MCP servers call.
    pub url: String,
    /// This program, which sessions' hooks and MCP servers run.
    pub exe: PathBuf,
    /// The ChloeCtx program the decision trail asks.
    pub chloectx_bin: String,
    /// Whether each session must get ChloeCtx (`--no-chloectx` turns it
    /// off, for tests): see `ctxwire`.
    pub chloectx_required: bool,
    /// The most a session's terminal recording grows to.
    pub record_max: u64,
}

/// An error the API answers with.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError { status, code, message: message.into() }
    }
    pub fn bad(m: impl Into<String>) -> ApiError {
        ApiError::new(400, "bad_request", m)
    }
    pub fn not_found(m: impl Into<String>) -> ApiError {
        ApiError::new(404, "not_found", m)
    }
    pub fn conflict(m: impl Into<String>) -> ApiError {
        ApiError::new(409, "conflict", m)
    }
    pub fn internal(m: impl Into<String>) -> ApiError {
        ApiError::new(500, "internal", m)
    }
}

/// A change the stream sends: an agent's row, or a new event.
#[derive(Clone, Debug)]
pub enum Change {
    Agent { json: Json },
    Event { agent: String, json: Json },
}

/// The output of a terminal: its scrollback, and the live feed.
struct Output {
    ring: VecDeque<u8>,
    tx: Option<broadcast::Sender<Bytes>>,
}

/// A running agent's terminal.
pub struct Term {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    output: Mutex<Output>,
    /// Its recording, `sessions/<id>/term.cast`, while it records.
    rec: Mutex<Option<Recorder>>,
    /// What the terminal shows now (src/screen.rs).
    screen: Mutex<crate::screen::Screen>,
    alive: AtomicBool,
    pid: i32,
}

impl Term {
    fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes)?;
        w.flush()
    }
}

struct Slot {
    agent: Agent,
    term: Option<Arc<Term>>,
    /// What the row does not hold (the mark `agent:<id>`): `read_only`,
    /// `restarted_from`, `resumed` (the session it continued).
    extra: Json,
}

struct Inner {
    slots: HashMap<String, Slot>,
    seq: i64,
}

pub struct Supervisor {
    pub cfg: Config,
    db: StateDb,
    inner: Mutex<Inner>,
    /// Spawns run one at a time, so that two cannot take one name.
    spawning: Mutex<()>,
    pub changes: broadcast::Sender<Change>,
}

/// What `POST /api/agents` takes.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct SpawnRequest {
    pub repo: String,
    pub task: Option<String>,
    pub name: Option<String>,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub worktree: Option<bool>,
    pub model: Option<String>,
    pub parent: Option<String>,
    /// The goal and outcome it works on (set by `levels::spawn`).
    pub goal: Option<String>,
    pub outcome: Option<String>,
    /// Claude Code's `--effort` (low, medium, high, xhigh, max); sixth
    /// build, for a goal's lead and agents.
    pub effort: Option<String>,
    /// Read-only (SPEC.md, "Sixth build: E"): no Edit, Write, MultiEdit or
    /// NotebookEdit, and Bash only for commands that read. An agent a
    /// read-only agent starts is read-only too.
    pub read_only: Option<bool>,
    /// Set by [`Supervisor::restart_request`]: the agent this one restarts.
    #[serde(skip)]
    pub restart: Option<Restart>,
    /// Ask before edits, tests and git in its own worktree (by default
    /// it does them there without asking: `flow::worktree_allow`).
    pub ask_edits: Option<bool>,
    /// What it may do without asking, must ask about, or may not do
    /// (JOURNEYS.md, "Start a change"): Claude Code rules such as `Read`,
    /// `Bash(python3 -m pytest:*)`, written into the session's settings.
    pub permissions: Option<PermissionProfile>,
}

/// A session's permission profile: Claude Code's `permissions.allow`,
/// `ask` and `deny` rules.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionProfile {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub ask: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl PermissionProfile {
    /// Each rule is a tool name (`Read`, `mcp__chloectx__memory_search`),
    /// optionally with a specifier in parentheses (`Bash(npm test:*)`).
    pub fn check(&self) -> Result<(), String> {
        let all = self.allow.iter().chain(&self.ask).chain(&self.deny);
        if self.allow.len() + self.ask.len() + self.deny.len() > 200 {
            return Err("a permission profile has at most 200 rules".into());
        }
        for r in all {
            let r = r.trim();
            let (tool, spec) = match r.find('(') {
                Some(i) if r.ends_with(')') => (&r[..i], Some(&r[i + 1..r.len() - 1])),
                Some(_) => return Err(format!("permission rule {r:?} opens a parenthesis it does not close")),
                None => (r, None),
            };
            let tool_ok = tool.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && tool.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if !tool_ok || r.len() > 400 || spec.is_some_and(|s| s.trim().is_empty() || s.contains('\n')) {
                return Err(format!("{r:?} is not a permission rule: a tool name such as Read, or Tool(specifier) such as Bash(npm test:*)"));
            }
        }
        Ok(())
    }

    /// Settings with these rules merged in. Read-only wins: its allow list
    /// is not widened.
    pub fn apply(&self, mut settings: Json, read_only: bool) -> Json {
        if !settings["permissions"].is_object() {
            settings["permissions"] = json!({});
        }
        for (key, rules) in [("allow", &self.allow), ("ask", &self.ask), ("deny", &self.deny)] {
            if key == "allow" && read_only {
                continue;
            }
            let mut list = settings["permissions"][key].as_array().cloned().unwrap_or_default();
            for r in rules.iter().map(|r| r.trim()).filter(|r| !r.is_empty()) {
                if !list.contains(&json!(r)) {
                    list.push(json!(r));
                }
            }
            if !list.is_empty() {
                settings["permissions"][key] = Json::Array(list);
            }
        }
        settings
    }
}

/// An exited agent started again: same name, task, worktree and model,
/// continuing its Claude Code session when it has one.
#[derive(Debug, Clone, Default)]
pub struct Restart {
    pub of: Agent,
    /// The session to continue (`--resume <id> --fork-session`).
    pub resume: Option<String>,
    /// What it is told first: the task again when nothing is resumed.
    pub prompt: Option<String>,
}

/// A notification that says only that a finished turn waits for a prompt
/// (by its type where Claude Code sends one, else by its words).
fn is_idle_notice(hook: &Json) -> bool {
    if let Some(t) = hook.get("notification_type").and_then(Json::as_str) {
        return t == "idle_prompt";
    }
    hook.get("message").and_then(Json::as_str).is_some_and(|m| m.to_ascii_lowercase().contains("waiting for your input"))
}

pub fn one_line(s: &str, max: usize) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    out
}

fn trim_to(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// A random lowercase hex string of `n` digits.
pub fn hex(n: usize) -> String {
    use rand::Rng;
    let mut r = rand::thread_rng();
    (0..n).map(|_| format!("{:x}", r.gen_range(0..16u8))).collect()
}

/// A random version 4 UUID.
fn uuid4() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !n.starts_with('-')
}

/// A name from a task's first words: `fix-the-login-bug`.
fn slug(task: Option<&str>) -> String {
    let mut out = String::new();
    for word in task.unwrap_or("").split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).take(4) {
        let w = word.to_ascii_lowercase();
        if out.len() + w.len() + 1 > 24 {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&w);
    }
    if out.is_empty() {
        "agent".into()
    } else {
        out
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The first string of a tool's input worth showing: its command, file,
/// pattern, URL or description.
fn tool_detail(input: &Json) -> Option<String> {
    for k in ["command", "file_path", "path", "pattern", "url", "query", "description", "prompt", "task"] {
        if let Some(s) = input.get(k).and_then(Json::as_str) {
            return Some(one_line(s, 160));
        }
    }
    input.as_object().and_then(|o| o.values().find_map(|v| v.as_str().map(|s| one_line(s, 160))))
}

/// One line for people of what a hook says.
pub fn summarize(kind: &str, hook: &Json) -> String {
    let s = |k: &str| hook.get(k).and_then(Json::as_str).unwrap_or("");
    let tool = s("tool_name");
    let detail = hook.get("tool_input").and_then(tool_detail);
    match kind {
        "SessionStart" => match s("source") {
            "" => "session started".into(),
            src => format!("session started ({src})"),
        },
        "UserPromptSubmit" => format!("prompt: {}", one_line(s("prompt"), 200)),
        "PreToolUse" => match detail {
            Some(d) => format!("{tool}: {d}"),
            None => tool.to_string(),
        },
        "PostToolUse" => match detail {
            Some(d) => format!("{tool} done: {d}"),
            None => format!("{tool} done"),
        },
        "PostToolUseFailure" => match detail {
            Some(d) => format!("{tool} failed: {d}"),
            None => format!("{tool} failed"),
        },
        "Notification" => one_line(s("message"), 200),
        "Stop" => "turn finished".into(),
        "SubagentStop" => "subagent finished".into(),
        "SessionEnd" => crate::endings::session_end_words(s("reason")),
        other => other.to_string(),
    }
}

fn expand_home(p: &str) -> PathBuf {
    if p == "~" || p.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(p.trim_start_matches('~').trim_start_matches('/'));
        }
    }
    PathBuf::from(p)
}

impl Supervisor {
    /// Opens the state database and marks the agents a previous server
    /// left behind as exited.
    pub fn open(cfg: Config) -> Result<Arc<Supervisor>, String> {
        let db = StateDb::open(&cfg.data_dir)?;
        let mut seq = db.last_seq()?;
        let mut slots = HashMap::new();
        let now = now_ms();
        for mut a in db.agents()? {
            if a.state != "exited" {
                a.state = "exited".into();
                a.state_since = now;
                a.waiting_for = None;
                a.ended = a.ended.or(Some(now));
                a.last_event = Some("the server stopped".into());
                seq += 1;
                db.add_event(&Event {
                    id: format!("{}:{seq:012}", a.id),
                    seq,
                    ts: now,
                    agent: a.id.clone(),
                    kind: "exited".into(),
                    tool: None,
                    summary: Some("the server stopped".into()),
                    data: None,
                })?;
                db.put_agent(&a)?;
            }
            let extra = db.mark(&extra_key(&a.id)).ok().flatten().and_then(|(v, _)| serde_json::from_str(&v).ok()).unwrap_or_else(|| json!({}));
            slots.insert(a.id.clone(), Slot { agent: a, term: None, extra });
        }
        let (changes, _) = broadcast::channel(4096);
        Ok(Arc::new(Supervisor { cfg, db, inner: Mutex::new(Inner { slots, seq }), spawning: Mutex::new(()), changes }))
    }

    fn running(slot: &Slot) -> bool {
        slot.term.as_ref().is_some_and(|t| t.alive.load(Ordering::SeqCst))
    }

    fn agent_json(inner: &Inner, id: &str) -> Option<Json> {
        let slot = inner.slots.get(id)?;
        let mut children: Vec<&Agent> = inner.slots.values().map(|s| &s.agent).filter(|a| a.parent.as_deref() == Some(id)).collect();
        children.sort_by_key(|a| a.created);
        let mut j = slot.agent.to_json(Self::running(slot), children.into_iter().map(|a| a.id.clone()).collect());
        if let (Some(o), Some(x)) = (j.as_object_mut(), slot.extra.as_object()) {
            for (k, v) in x {
                o.insert(k.clone(), v.clone());
            }
        }
        Some(j)
    }

    fn emit_agent(&self, inner: &Inner, id: &str) {
        if let Some(json) = Self::agent_json(inner, id) {
            let _ = self.changes.send(Change::Agent { json });
        }
    }

    /// Records an event of `id`, sets its last event, writes both rows and
    /// sends both changes. The caller has changed the agent's state.
    fn record(&self, inner: &mut Inner, id: &str, kind: &str, tool: Option<String>, summary: String, data: Option<String>) -> Result<(), ApiError> {
        inner.seq += 1;
        let seq = inner.seq;
        let now = now_ms();
        let ev = Event { id: format!("{id}:{seq:012}"), seq, ts: now, agent: id.to_string(), kind: kind.to_string(), tool, summary: Some(summary.clone()), data: data.map(|d| trim_to(d, DATA_MAX)) };
        self.db.add_event(&ev).map_err(ApiError::internal)?;
        let slot = inner.slots.get_mut(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
        if !summary.is_empty() {
            slot.agent.last_event = Some(summary);
        }
        self.db.put_agent(&slot.agent).map_err(ApiError::internal)?;
        let _ = self.changes.send(Change::Event { agent: id.to_string(), json: ev.to_json(false) });
        self.emit_agent(inner, id);
        Ok(())
    }

    fn set_state(agent: &mut Agent, state: &str) {
        if agent.state != state {
            agent.state = state.to_string();
            agent.state_since = now_ms();
        }
        if state != "waiting" {
            agent.waiting_for = None;
        }
    }

    pub fn running_count(&self) -> usize {
        self.inner.lock().unwrap().slots.values().filter(|s| Self::running(s)).count()
    }

    /// Every agent as JSON: running ones first, then by when they started;
    /// exited ones only with `all`.
    pub fn list(&self, all: bool) -> Vec<Json> {
        let inner = self.inner.lock().unwrap();
        let mut slots: Vec<&Slot> = inner.slots.values().filter(|s| all || Self::running(s)).collect();
        slots.sort_by_key(|s| (!Self::running(s), s.agent.created, s.agent.id.clone()));
        slots.iter().filter_map(|s| Self::agent_json(&inner, &s.agent.id)).collect()
    }

    /// The agents waiting or idle, longest in that state first.
    pub fn attention(&self) -> Vec<Json> {
        let inner = self.inner.lock().unwrap();
        let mut slots: Vec<&Slot> = inner.slots.values().filter(|s| Self::running(s) && matches!(s.agent.state.as_str(), "waiting" | "idle")).collect();
        slots.sort_by_key(|s| (s.agent.state_since, s.agent.created));
        slots.iter().filter_map(|s| Self::agent_json(&inner, &s.agent.id)).collect()
    }

    pub fn get(&self, id: &str) -> Result<Json, ApiError> {
        let inner = self.inner.lock().unwrap();
        Self::agent_json(&inner, id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))
    }

    /// An agent's state, and whether it runs.
    pub fn state(&self, id: &str) -> Option<(String, bool)> {
        let inner = self.inner.lock().unwrap();
        inner.slots.get(id).map(|s| (s.agent.state.clone(), Self::running(s)))
    }

    pub fn events(&self, id: &str, after: i64, limit: usize, data: bool) -> Result<Vec<Json>, ApiError> {
        if !self.inner.lock().unwrap().slots.contains_key(id) {
            return Err(ApiError::not_found(format!("no agent {id}")));
        }
        let evs = self.db.events(id, after, limit).map_err(ApiError::internal)?;
        Ok(evs.iter().map(|e| e.to_json(data)).collect())
    }

    fn term(&self, id: &str) -> Result<Arc<Term>, ApiError> {
        let inner = self.inner.lock().unwrap();
        let slot = inner.slots.get(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
        match &slot.term {
            Some(t) if t.alive.load(Ordering::SeqCst) => Ok(t.clone()),
            _ => Err(ApiError::conflict(format!("agent {id} is not running"))),
        }
    }

    /// Starts an agent.
    pub fn spawn(self: &Arc<Self>, req: SpawnRequest) -> Result<Json, ApiError> {
        let _one = self.spawning.lock().unwrap();
        if req.repo.trim().is_empty() {
            return Err(ApiError::bad("repo is required: the path of a git checkout"));
        }
        if let Some(p) = &req.permissions {
            p.check().map_err(ApiError::bad)?;
        }
        let path = expand_home(req.repo.trim());
        if !path.is_absolute() {
            return Err(ApiError::bad(format!("repo must be an absolute path, not {}", req.repo)));
        }
        let co = git::checkout(&path).map_err(ApiError::bad)?;
        // Before a worktree is made: a session without ChloeCtx would miss
        // most of what Workbench shows.
        if self.cfg.chloectx_required {
            crate::ctxwire::wiring(&self.cfg.chloectx_bin, &co.main).map_err(crate::ctxwire::missing)?;
        }
        let worktree = req.worktree.unwrap_or(true);
        if !worktree && (req.branch.is_some() || req.base.is_some()) {
            return Err(ApiError::bad("branch and base need a worktree of the agent's own (worktree: true)"));
        }
        if let Some(n) = &req.name {
            if !valid_name(n) {
                return Err(ApiError::bad(format!("name {n:?} must be 1 to 32 of a-z, 0-9 and -")));
            }
        }
        let model = req.model.clone().filter(|m| !m.trim().is_empty());
        let task = req.task.clone().filter(|t| !t.trim().is_empty());
        let restart = req.restart.clone();
        // Read-only when asked, or when the agent that starts it is.
        let read_only = req.read_only.unwrap_or(false) || req.parent.as_deref().is_some_and(|p| self.is_read_only(p));

        // Limits, the parent and the name, checked together.
        let (depth, name) = {
            let inner = self.inner.lock().unwrap();
            let running = inner.slots.values().filter(|s| Self::running(s)).count();
            if running >= self.cfg.max_agents {
                return Err(ApiError::new(429, "limit_exceeded", format!("{running} agents are running, the most allowed (--max-agents {})", self.cfg.max_agents)));
            }
            let depth = match &req.parent {
                None => 0,
                Some(p) => {
                    let parent = inner.slots.get(p).ok_or_else(|| ApiError::not_found(format!("no parent agent {p}")))?;
                    let d = parent.agent.depth + 1;
                    if d >= self.cfg.max_depth {
                        return Err(ApiError::conflict(format!(
                            "agent {} is {} deep and may start no more agents (--max-depth {})",
                            parent.agent.name,
                            parent.agent.depth + 1,
                            self.cfg.max_depth
                        )));
                    }
                    d
                }
            };
            let in_use = |n: &str| inner.slots.values().any(|s| Self::running(s) && s.agent.name == n);
            let wt_dir = git::worktrees_dir(&self.cfg.data_dir, &co.main);
            let name = match &req.name {
                Some(n) => {
                    if in_use(n) {
                        return Err(ApiError::conflict(format!("an agent named {n} is running")));
                    }
                    n.clone()
                }
                None => {
                    let base = slug(task.as_deref());
                    let free = |n: &str| !in_use(n) && (!worktree || (req.branch.is_some() || !git::branch_exists(&co.main, &format!("wb/{n}"))) && !wt_dir.join(n).exists());
                    let mut n = base.clone();
                    let mut i = 2;
                    while !free(&n) {
                        n = format!("{base}-{i}");
                        i += 1;
                    }
                    n
                }
            };
            (depth, name)
        };

        let mut id = format!("a{}", hex(8));
        while self.inner.lock().unwrap().slots.contains_key(&id) {
            id = format!("a{}", hex(8));
        }

        // Where it runs, and the commit it starts from.
        let (cwd, branch, base) = if worktree {
            let branch = req.branch.clone().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| format!("wb/{name}"));
            let dir = git::worktrees_dir(&self.cfg.data_dir, &co.main);
            // A restart carries on in the worktree its agent had.
            let mut wt = restart.as_ref().map(|r| PathBuf::from(&r.of.cwd)).filter(|p| p.is_dir() && p.as_path() != co.main.as_path()).unwrap_or_else(|| dir.join(&name));
            // The worktree an earlier agent of this name left, on this
            // branch: the new one carries on there.
            let reuse = wt.is_dir() && git::current_branch(&wt).as_deref() == Some(branch.as_str());
            let base = if reuse {
                // Where the earlier agent of this worktree started.
                let wt_s = wt.canonicalize().unwrap_or(wt.clone()).to_string_lossy().to_string();
                let inner = self.inner.lock().unwrap();
                let mut earlier: Vec<&Agent> = inner.slots.values().map(|s| &s.agent).filter(|a| a.cwd == wt_s && a.base.is_some()).collect();
                earlier.sort_by_key(|a| a.created);
                earlier.first().and_then(|a| a.base.clone()).or_else(|| git::rev_parse(&wt, "HEAD"))
            } else {
                if wt.exists() {
                    wt = dir.join(format!("{name}-{id}"));
                }
                let from = if git::branch_exists(&co.main, &branch) { branch.clone() } else { req.base.clone().unwrap_or_else(|| "HEAD".into()) };
                let base = git::rev_parse(&co.main, &from);
                git::add_worktree(&co.main, &wt, &branch, req.base.as_deref()).map_err(|e| ApiError::conflict(format!("cannot add a worktree: {e}")))?;
                base
            };
            (wt.canonicalize().unwrap_or(wt), Some(branch), base)
        } else {
            let cwd = path.canonicalize().unwrap_or(path);
            let branch = git::current_branch(&cwd);
            let base = git::rev_parse(&cwd, "HEAD");
            (cwd, branch, base)
        };

        // Its settings and MCP config.
        let session_dir = self.cfg.data_dir.join("sessions").join(&id);
        std::fs::create_dir_all(&session_dir).map_err(|e| ApiError::internal(format!("cannot create {}: {e}", session_dir.display())))?;
        let settings = session_dir.join("settings.json");
        let mcp = session_dir.join("mcp.json");
        let exe = self.cfg.exe.to_string_lossy().to_string();
        // Bash prefixes allowed from Flow's suggestions, for this repository.
        // A key of its own, which cannot decide a change request (src/guarded.rs).
        let token = crate::guarded::agent_token(&id);
        // Who writes this session's prompts, for ChloeCtx (SPEC.md, "Sixth
        // build: ChloeCtx remembers the right things"): an agent's brief is
        // not the user's preference.
        let parent_name = req.parent.as_deref().and_then(|p| self.agent_row(p)).map(|a| a.name);
        let who = crate::ctxwire::Who::new(&name, &session_dir, parent_name.as_deref(), req.goal.is_some() && req.outcome.is_none() && req.parent.is_none(), task.as_deref());
        let (settings_v, mcp_v) = crate::ctxwire::wire_as(
            &self.cfg.chloectx_bin,
            self.cfg.chloectx_required,
            &cwd,
            {
                let s = crate::flow::with_allowed(settings_json(&exe), &self.allowed(&co.main.to_string_lossy()));
                let s = if read_only {
                    // Read-only wins: no worktree defaults that allow edits.
                    crate::guarded::read_only_settings(s)
                } else if worktree && !req.ask_edits.unwrap_or(false) {
                    crate::flow::with_worktree_defaults(s, &cwd)
                } else {
                    s
                };
                match &req.permissions {
                    Some(p) => p.apply(s, read_only),
                    None => s,
                }
            },
            mcp_json(&exe, &self.cfg.url, &token, &id),
            &who,
        )?;
        // The repository's current harness version, applied and recorded (src/harness.rs).
        let applied = crate::harness::at_spawn(self, &co.main.to_string_lossy(), &cwd, &session_dir, &id, &name, settings_v, mcp_v);
        write_private(&settings, &applied.settings)?;
        write_private(&mcp, &applied.mcp)?;

        // The process.
        let session_id = uuid4();
        let mut cmd = CommandBuilder::new(&self.cfg.claude_bin);
        cmd.arg("--session-id");
        cmd.arg(&session_id);
        // A restart continues its agent's conversation in a session of its
        // own (Claude Code wants --fork-session with --session-id).
        let resume = restart.as_ref().and_then(|r| r.resume.clone());
        if let Some(r) = &resume {
            cmd.arg("--resume");
            cmd.arg(r);
            cmd.arg("--fork-session");
        }
        cmd.arg("--settings");
        cmd.arg(&settings);
        cmd.arg("--mcp-config");
        cmd.arg(&mcp);
        if let Some(m) = &model {
            cmd.arg("--model");
            cmd.arg(m);
        }
        if let Some(e) = req.effort.as_deref().filter(|e| crate::goals::EFFORTS.contains(e)) {
            cmd.arg("--effort");
            cmd.arg(e);
        }
        let first = match &restart {
            Some(r) => r.prompt.clone().filter(|p| !p.trim().is_empty()),
            None => task.clone(),
        };
        if let Some(t) = &first {
            // `--mcp-config` takes any number of files, so without `--` a
            // task right after it (no `--model` between) is read as a file.
            cmd.arg("--");
            cmd.arg(t);
        }
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("CHLOEDB_WORKBENCH_URL", &self.cfg.url);
        cmd.env("CHLOEDB_WORKBENCH_TOKEN", &token);
        cmd.env("CHLOEDB_WORKBENCH_AGENT", &id);
        // Sixth build: pytest writes a junit report Health reads test by test (src/testruns.rs).
        for (k, v) in crate::testruns::session_env(Path::new(&cwd), &session_dir) {
            cmd.env(k, v);
        }
        // A server started from inside a Claude Code session must not make
        // its agents look nested in it.
        for marker in ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ID", "CLAUDE_PID"] {
            cmd.env_remove(marker);
        }

        let pair = native_pty_system()
            .openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| ApiError::internal(format!("cannot open a terminal: {e}")))?;
        let started = Instant::now();
        let started_ms = now_ms();
        let mut child = pair.slave.spawn_command(cmd).map_err(|e| ApiError::bad(format!("cannot start {}: {e}", self.cfg.claude_bin)))?;
        drop(pair.slave);
        let rec = Recorder::create(&session_dir.join("term.cast"), COLS, ROWS, started, started_ms, self.cfg.record_max);
        let pid = child.process_id().map_or(0, |p| p as i32);
        let reader = pair.master.try_clone_reader().map_err(|e| ApiError::internal(e.to_string()))?;
        let writer = pair.master.take_writer().map_err(|e| ApiError::internal(e.to_string()))?;
        let (out_tx, _) = broadcast::channel(1024);
        let term = Arc::new(Term {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            output: Mutex::new(Output { ring: VecDeque::with_capacity(64 * 1024), tx: Some(out_tx) }),
            rec: Mutex::new(rec),
            screen: Mutex::new(crate::screen::Screen::new(COLS, ROWS)),
            alive: AtomicBool::new(true),
            pid,
        });

        let now = now_ms();
        let agent = Agent {
            id: id.clone(),
            name,
            repo: co.main.to_string_lossy().to_string(),
            cwd: cwd.to_string_lossy().to_string(),
            branch,
            task: task.clone(),
            parent: req.parent.clone(),
            depth,
            state: "starting".into(),
            state_since: now,
            waiting_for: None,
            last_event: None,
            session_id: Some(session_id),
            pid: Some(pid as i64),
            exit_code: None,
            model,
            created: now,
            ended: None,
            base,
            started_ms: Some(started_ms),
            preview_url: None,
            goal: req.goal.clone(),
            outcome: req.outcome.clone(),
            narration: None,
            narration_ts: None,
            harness: applied.version,
        };
        let mut extra = serde_json::Map::new();
        if read_only {
            extra.insert("read_only".into(), json!(true));
        }
        if let Some(r) = &restart {
            extra.insert("restarted_from".into(), json!(r.of.id));
        }
        if let Some(r) = &resume {
            extra.insert("resumed".into(), json!(r));
        }
        let extra = Json::Object(extra);
        if extra.as_object().is_some_and(|o| !o.is_empty()) {
            let _ = self.db.set_mark(&extra_key(&id), &extra.to_string());
        }
        let json = {
            let mut inner = self.inner.lock().unwrap();
            inner.slots.insert(id.clone(), Slot { agent, term: Some(term.clone()), extra });
            let how = match (&restart, &resume) {
                (Some(r), Some(_)) => format!(" (restart of {}, continuing its session)", r.of.id),
                (Some(r), None) => format!(" (restart of {})", r.of.id),
                _ => String::new(),
            };
            let summary = match &task {
                Some(t) => format!("spawned{how}: {}", one_line(t, 200)),
                None => format!("spawned{how}"),
            };
            self.record(&mut inner, &id, "spawned", None, summary, None)?;
            if let Some(p) = &req.parent {
                // The parent's children changed.
                self.emit_agent(&inner, p);
            }
            Self::agent_json(&inner, &id).unwrap()
        };

        // Its output.
        let t = term.clone();
        let (sup, aid) = (self.clone(), id.clone());
        std::thread::Builder::new()
            .name(format!("pty-{id}"))
            .spawn(move || read_output(reader, &t, &sup, &aid))
            .map_err(|e| ApiError::internal(e.to_string()))?;
        // Its exit.
        let sup = self.clone();
        let aid = id.clone();
        std::thread::Builder::new()
            .name(format!("wait-{id}"))
            .spawn(move || {
                let code = match child.wait() {
                    Ok(s) => s.exit_code() as i64,
                    Err(_) => -1,
                };
                sup.exited(&aid, code);
            })
            .map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(json)
    }

    fn exited(&self, id: &str, code: i64) {
        // Why, in plain words: Stop pressed here, its SessionEnd reason, and
        // other agents' commands that ended processes just before.
        let end_reason = {
            let p = chloedb::query::Params::new().set("agent", chloedb::Value::Str(id.to_string()));
            let rows = self.db.query("SELECT data FROM events WHERE agent = :agent AND kind = 'SessionEnd' ORDER BY seq DESC LIMIT 1", &p).unwrap_or_default();
            rows.first().and_then(|r| crate::db::get_str(&r[0])).and_then(|d| serde_json::from_str::<Json>(&d).ok()).and_then(|d| d["reason"].as_str().map(str::to_string))
        };
        let suspects = if code == 0 || code > 128 { crate::endings::kill_suspects(self, id, now_ms(), 20_000) } else { Vec::new() };
        let mut inner = self.inner.lock().unwrap();
        let Some(slot) = inner.slots.get_mut(id) else { return };
        if let Some(t) = &slot.term {
            t.alive.store(false, Ordering::SeqCst);
        }
        Self::set_state(&mut slot.agent, "exited");
        slot.agent.exit_code = Some(code);
        slot.agent.ended = Some(now_ms());
        let stopped = slot.extra.get("stop_requested").is_some();
        let why = crate::endings::exit_words(code, stopped, end_reason.as_deref(), &suspects);
        if let Some(o) = slot.extra.as_object_mut() {
            o.insert("ended_why".into(), json!(why));
            o.remove("stop_requested");
        }
        let _ = self.db.set_mark(&extra_key(id), &slot.extra.to_string());
        let parent = slot.agent.parent.clone();
        let _ = self.record(&mut inner, id, "exited", None, format!("exited with code {code}: {why}"), Some(json!({ "code": code, "why": why, "stopped": stopped, "reason": end_reason, "suspects": suspects }).to_string()));
        if let Some(p) = parent {
            self.emit_agent(&inner, &p);
        }
        drop(inner);
        // Its servers go with it when the user chose so (SPEC.md, "Sixth
        // build: E", Servers).
        if crate::preview::servers::policy(self).as_deref() == Some("stop") {
            crate::preview::servers::stop_of_agent_later(self, id);
        }
    }

    /// A hook's report: an event, and the state it implies.
    pub fn hook(&self, id: &str, hook: Json) -> Result<(), ApiError> {
        let kind = hook.get("hook_event_name").and_then(Json::as_str).ok_or_else(|| ApiError::bad("the hook's JSON has no hook_event_name"))?.to_string();
        if kind.is_empty() || kind.len() > 64 {
            return Err(ApiError::bad("hook_event_name is empty or too long"));
        }
        let tool = hook.get("tool_name").and_then(Json::as_str).map(str::to_string);
        let summary = summarize(&kind, &hook);
        let data = serde_json::to_string(&hook).ok();
        let mut inner = self.inner.lock().unwrap();
        let slot = inner.slots.get_mut(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
        if let Some(sid) = hook.get("session_id").and_then(Json::as_str).filter(|s| !s.is_empty()) {
            slot.agent.session_id = Some(sid.to_string());
        }
        let a = &mut slot.agent;
        match kind.as_str() {
            "SessionStart" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Self::set_state(a, "working"),
            // Claude Code also notifies when a finished turn has sat idle a
            // while: that agent is idle, not stopped on a question.
            "Notification" if a.state == "idle" && is_idle_notice(&hook) => {}
            "Notification" => {
                Self::set_state(a, "waiting");
                a.waiting_for = Some(one_line(hook.get("message").and_then(Json::as_str).unwrap_or("waiting for you"), 500));
            }
            "Stop" => Self::set_state(a, "idle"),
            "SessionEnd" => Self::set_state(a, "exited"),
            _ => {}
        }
        self.record(&mut inner, id, &kind, tool, summary, data)
    }

    /// Types into an agent's terminal, from the API: an `input` event, and
    /// a waiting agent goes back to working.
    /// `by` is the agent that typed it (`send_to_agent`), or `None` for you.
    pub fn input(&self, id: &str, text: &str, enter: bool, by: Option<&str>) -> Result<(), ApiError> {
        let term = self.term(id)?;
        // Another agent's words, so ChloeCtx records them as that agent's,
        // not the user's.
        if let Some(b) = by.filter(|b| *b != id && !text.is_empty()) {
            if let Some(from) = self.agent_row(b) {
                crate::ctxwire::note_prompt(&crate::ctxwire::prompt_log(&self.cfg.data_dir.join("sessions").join(id)), text, &format!("agent:{}", from.name));
            }
        }
        term.write(text.as_bytes()).map_err(|e| ApiError::internal(format!("cannot write to the terminal: {e}")))?;
        if enter {
            if !text.is_empty() {
                // Text and Enter in one read is a paste to Claude Code's
                // prompt, where Enter is a new line.
                std::thread::sleep(Duration::from_millis(150));
            }
            term.write(b"\r").map_err(|e| ApiError::internal(format!("cannot write to the terminal: {e}")))?;
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(slot) = inner.slots.get_mut(id) {
            // An answer, or (with Enter) a prompt: so that a wait for idle
            // right after it waits for the turn it starts.
            if slot.agent.state == "waiting" || (enter && slot.agent.state == "idle") {
                Self::set_state(&mut slot.agent, "working");
            }
        }
        let summary = if text.is_empty() && enter { "input: Enter".to_string() } else { format!("input: {}", one_line(text, 200)) };
        let by = by.filter(|b| *b != id && inner.slots.contains_key(*b));
        let data = by.map(|b| json!({ "by": b }).to_string());
        self.record(&mut inner, id, "input", None, summary, data)
    }

    /// Keystrokes from a terminal's WebSocket. Enter while the agent waits
    /// counts as an answer, and while it is idle as a prompt: an `input`
    /// event, and it goes back to working.
    pub fn keys(&self, id: &str, bytes: &[u8]) -> Result<(), ApiError> {
        let term = self.term(id)?;
        term.write(bytes).map_err(|e| ApiError::internal(format!("cannot write to the terminal: {e}")))?;
        if bytes.contains(&b'\r') {
            let mut inner = self.inner.lock().unwrap();
            let answers = inner.slots.get(id).is_some_and(|s| matches!(s.agent.state.as_str(), "waiting" | "idle"));
            if answers {
                if let Some(slot) = inner.slots.get_mut(id) {
                    Self::set_state(&mut slot.agent, "working");
                }
                self.record(&mut inner, id, "input", None, "input: typed in the terminal".into(), None)?;
            }
        }
        Ok(())
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), ApiError> {
        if cols == 0 || rows == 0 || cols > 1000 || rows > 1000 {
            return Err(ApiError::bad("cols and rows must be 1 to 1000"));
        }
        let term = self.term(id)?;
        let m = term.master.lock().unwrap();
        m.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).map_err(|e| ApiError::internal(e.to_string()))?;
        drop(m);
        term.screen.lock().unwrap().resize(cols, rows);
        let full = term.rec.lock().unwrap().as_mut().map(|r| r.resize(cols, rows));
        self.recording_full(id, &term, full);
        Ok(())
    }

    /// When a recording reached its cap: it stops, and an event says so.
    fn recording_full(&self, id: &str, term: &Term, wrote: Option<Wrote>) {
        if wrote != Some(Wrote::Full) {
            return;
        }
        *term.rec.lock().unwrap() = None;
        let mut inner = self.inner.lock().unwrap();
        let mib = self.cfg.record_max as f64 / (1024.0 * 1024.0);
        let _ = self.record(&mut inner, id, "recording", None, format!("terminal recording stopped: it reached {mib:.0} MiB"), None);
    }

    /// An agent's row.
    pub fn agent_row(&self, id: &str) -> Option<Agent> {
        self.inner.lock().unwrap().slots.get(id).map(|s| s.agent.clone())
    }

    /// What an agent's row does not hold (`read_only`, `restarted_from`,
    /// `resumed`), `{}` when nothing.
    pub fn extra_of(&self, id: &str) -> Json {
        self.inner.lock().unwrap().slots.get(id).map(|s| s.extra.clone()).unwrap_or_else(|| json!({}))
    }

    /// Whether an agent was started read-only.
    pub fn is_read_only(&self, id: &str) -> bool {
        self.extra_of(id)["read_only"] == json!(true)
    }

    /// What starts an exited agent again (`POST /api/agents/:id/restart`):
    /// its name, task, model, worktree and branch, read-only as it was, its
    /// goal and outcome; it continues its Claude Code session when that
    /// session has a transcript, else it starts over on its task. `prompt`
    /// is what it is told first; a continued session that ended mid-turn is
    /// told to carry on.
    pub fn restart_request(&self, id: &str, prompt: Option<String>) -> Result<SpawnRequest, ApiError> {
        let (a, extra) = {
            let inner = self.inner.lock().unwrap();
            let slot = inner.slots.get(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
            if Self::running(slot) {
                return Err(ApiError::conflict(format!("agent {} is running: stop it first, or steer it", slot.agent.name)));
            }
            if let Some(live) = inner.slots.values().find(|s| Self::running(s) && s.agent.name == slot.agent.name && s.agent.repo == slot.agent.repo) {
                return Err(ApiError::conflict(format!("an agent named {} is running ({}): it already carries on", live.agent.name, live.agent.id)));
            }
            (slot.agent.clone(), slot.extra.clone())
        };
        let resume = a.session_id.clone().filter(|_| self.transcript_of(id).is_some());
        let prompt = prompt.filter(|p| !p.trim().is_empty()).or_else(|| match &resume {
            Some(_) if self.ended_mid_turn(id) => Some("Workbench restarted this session after it ended. Carry on with your task where you left off.".to_string()),
            Some(_) => None,
            None => a.task.clone(),
        });
        let worktree = a.cwd != a.repo;
        Ok(SpawnRequest {
            repo: a.repo.clone(),
            task: a.task.clone(),
            name: Some(a.name.clone()),
            branch: if worktree { a.branch.clone() } else { None },
            worktree: Some(worktree),
            model: a.model.clone(),
            parent: a.parent.clone().filter(|p| self.is_running(p)),
            goal: a.goal.clone(),
            outcome: a.outcome.clone(),
            read_only: Some(extra["read_only"] == json!(true)),
            restart: Some(Restart { of: a, resume, prompt }),
            ..SpawnRequest::default()
        })
    }

    /// The transcript of an agent's session, when it has one with lines:
    /// the last `transcript_path` its hooks reported.
    fn transcript_of(&self, id: &str) -> Option<PathBuf> {
        let evs = self.db.all_events(id).ok()?;
        evs.iter().rev().filter_map(|e| e.data.as_deref()).filter_map(|d| serde_json::from_str::<Json>(d).ok()).find_map(|d| {
            let p = PathBuf::from(d["transcript_path"].as_str()?);
            std::fs::metadata(&p).ok().filter(|m| m.len() > 0).map(|_| p)
        })
    }

    /// Whether an agent ended inside a turn (its last hook was not a Stop).
    fn ended_mid_turn(&self, id: &str) -> bool {
        let Ok(evs) = self.db.all_events(id) else { return false };
        const HOOKS: [&str; 6] = ["UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure", "Notification", "Stop"];
        evs.iter().rev().find(|e| HOOKS.contains(&e.kind.as_str())).is_some_and(|e| e.kind != "Stop")
    }

    /// Every agent's row.
    pub fn agent_rows(&self) -> Vec<Agent> {
        self.inner.lock().unwrap().slots.values().map(|s| s.agent.clone()).collect()
    }

    /// An agent as the API answers it, or `None`.
    pub fn agent_json_of(&self, id: &str) -> Option<Json> {
        Self::agent_json(&self.inner.lock().unwrap(), id)
    }

    /// Every event of an agent, oldest first, with its data.
    pub fn all_events(&self, id: &str) -> Result<Vec<Event>, ApiError> {
        self.db.all_events(id).map_err(ApiError::internal)
    }

    /// The state database, for the levels' tables.
    pub fn db(&self) -> &StateDb {
        &self.db
    }

    /// Changes an agent's row with `f`, writes it and sends the change;
    /// answers the agent as the API does.
    pub fn update_agent(&self, id: &str, f: impl FnOnce(&mut Agent)) -> Result<Json, ApiError> {
        let mut inner = self.inner.lock().unwrap();
        let slot = inner.slots.get_mut(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
        f(&mut slot.agent);
        self.db.put_agent(&slot.agent).map_err(ApiError::internal)?;
        self.emit_agent(&inner, id);
        Ok(Self::agent_json(&inner, id).unwrap_or(Json::Null))
    }

    /// Changes an agent's row with `f` and records an event of it (kinds
    /// the levels add: `question`, `answered`, `steer`).
    pub fn note(&self, id: &str, kind: &str, summary: String, data: Option<String>, f: impl FnOnce(&mut Agent)) -> Result<(), ApiError> {
        let mut inner = self.inner.lock().unwrap();
        let slot = inner.slots.get_mut(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
        f(&mut slot.agent);
        self.record(&mut inner, id, kind, None, summary, data)
    }

    /// Whether an agent's process runs.
    pub fn is_running(&self, id: &str) -> bool {
        self.inner.lock().unwrap().slots.get(id).is_some_and(Self::running)
    }

    /// Sets an agent's state as the hooks would (the time in state starts
    /// over when it changes; `waiting_for` is kept only while waiting).
    pub fn set_agent_state(agent: &mut Agent, state: &str) {
        Self::set_state(agent, state)
    }

    /// Where an agent's session files are: its settings, its recording.
    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.cfg.data_dir.join("sessions").join(id)
    }

    /// Stops an agent: SIGHUP to its process group, then SIGKILL after 5 s;
    /// and the servers it started when `servers` says so (else as the user
    /// chose once, else yes): see `preview::servers`.
    pub fn stop_with(&self, id: &str, servers: Option<bool>) -> Result<(), ApiError> {
        let term = {
            let mut inner = self.inner.lock().unwrap();
            let slot = inner.slots.get_mut(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
            // So that its end says "Stopped from Workbench" (src/endings.rs).
            if let Some(o) = slot.extra.as_object_mut() {
                o.insert("stop_requested".into(), json!(now_ms()));
            }
            slot.term.clone()
        };
        let with_servers = servers.unwrap_or_else(|| crate::preview::servers::policy(self).as_deref() != Some("keep"));
        if with_servers {
            crate::preview::servers::stop_of_agent_later(self, id);
        }
        if let Some(t) = term.filter(|t| t.alive.load(Ordering::SeqCst)) {
            hangup(&t);
            std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_secs(5);
                while Instant::now() < until && t.alive.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if t.alive.load(Ordering::SeqCst) {
                    kill(&t);
                }
            });
        }
        Ok(())
    }

    /// Stops every agent, waits for them (SIGKILL after 5 s), and closes
    /// the database.
    pub fn shutdown(&self) {
        let terms: Vec<Arc<Term>> = {
            let inner = self.inner.lock().unwrap();
            inner.slots.values().filter_map(|s| s.term.clone()).filter(|t| t.alive.load(Ordering::SeqCst)).collect()
        };
        for t in &terms {
            hangup(t);
        }
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until && terms.iter().any(|t| t.alive.load(Ordering::SeqCst)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        for t in terms.iter().filter(|t| t.alive.load(Ordering::SeqCst)) {
            kill(t);
        }
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until && terms.iter().any(|t| t.alive.load(Ordering::SeqCst)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _lock = self.inner.lock().unwrap();
        self.db.close();
    }

    /// The scrollback of an agent's terminal and a feed of what follows it
    /// (`None` once the terminal closed).
    pub fn attach(&self, id: &str) -> Result<(Vec<u8>, Option<broadcast::Receiver<Bytes>>), ApiError> {
        let term = {
            let inner = self.inner.lock().unwrap();
            let slot = inner.slots.get(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
            slot.term.clone()
        };
        let Some(term) = term else { return Ok((Vec::new(), None)) };
        let mut out = term.output.lock().unwrap();
        let back = out.ring.make_contiguous().to_vec();
        let rx = out.tx.as_ref().map(|tx| tx.subscribe());
        Ok((back, rx))
    }
}

// What a running agent's terminal shows, and keys typed into it to answer
// what it shows (SPEC.md, "Sixth build: Safe answering").
impl Supervisor {
    /// The lines its terminal shows now and a number that changes with
    /// them, or `None` when it does not run.
    pub fn screen(&self, id: &str) -> Option<(Vec<String>, u64)> {
        let term = self.term(id).ok()?;
        let s = term.screen.lock().unwrap();
        Some((s.lines(), s.version))
    }

    /// What its screen waits on now (`Unknown` when it does not run).
    pub fn waits(&self, id: &str) -> crate::screen::Waits {
        match self.screen(id) {
            Some((lines, _)) => crate::screen::read(&lines),
            None => crate::screen::Waits::Unknown,
        }
    }

    /// An arrow key as its terminal wants it (`up` or down).
    pub fn arrow(&self, id: &str, up: bool) -> Result<(), ApiError> {
        let term = self.term(id)?;
        let app = term.screen.lock().unwrap().app_cursor();
        let key: &[u8] = match (app, up) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1bOB",
            (false, true) => b"\x1b[A",
            (false, false) => b"\x1b[B",
        };
        term.write(key).map_err(|e| ApiError::internal(format!("cannot write to the terminal: {e}")))
    }

    /// Keys typed as they are, with no event (an answer records its own).
    pub fn type_raw(&self, id: &str, bytes: &[u8]) -> Result<(), ApiError> {
        let term = self.term(id)?;
        term.write(bytes).map_err(|e| ApiError::internal(format!("cannot write to the terminal: {e}")))
    }
}

// The levels' state: marks (small key/value rows) and an agent's preview.
impl Supervisor {
    /// A mark's value, or `None`.
    pub fn mark(&self, key: &str) -> Option<String> {
        self.db.mark(key).ok().flatten().map(|(v, _)| v)
    }

    pub fn set_mark(&self, key: &str, value: &str) -> Result<(), ApiError> {
        self.db.set_mark(key, value).map_err(ApiError::internal)
    }

    /// The `permissions.allow` patterns applied for a repository (Flow's
    /// "allow" suggestions), kept in the mark `allow:<repo>`.
    pub fn allowed(&self, repo: &str) -> Vec<String> {
        self.mark(&format!("allow:{repo}")).and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
    }

    /// Sets (or with `None` clears) where an agent's app runs, and sends
    /// the agent's change.
    pub fn set_preview_url(&self, id: &str, url: Option<String>) -> Result<Json, ApiError> {
        let mut inner = self.inner.lock().unwrap();
        let slot = inner.slots.get_mut(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
        slot.agent.preview_url = url;
        self.db.put_agent(&slot.agent).map_err(ApiError::internal)?;
        self.emit_agent(&inner, id);
        Ok(Self::agent_json(&inner, id).unwrap_or(Json::Null))
    }
}

fn hangup(t: &Term) {
    if t.pid > 0 {
        unsafe {
            libc::kill(-t.pid, libc::SIGHUP);
            libc::kill(t.pid, libc::SIGHUP);
        }
    }
}

fn kill(t: &Term) {
    if t.pid > 0 {
        unsafe {
            libc::kill(-t.pid, libc::SIGKILL);
            libc::kill(t.pid, libc::SIGKILL);
        }
    }
}

fn read_output(mut reader: Box<dyn Read + Send>, term: &Term, sup: &Supervisor, id: &str) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let wrote = term.rec.lock().unwrap().as_mut().map(|r| r.output(&buf[..n]));
                sup.recording_full(id, term, wrote);
                term.screen.lock().unwrap().feed(&buf[..n]);
                let mut out = term.output.lock().unwrap();
                out.ring.extend(&buf[..n]);
                let excess = out.ring.len().saturating_sub(SCROLLBACK);
                if excess > 0 {
                    out.ring.drain(..excess);
                }
                if let Some(tx) = &out.tx {
                    let _ = tx.send(Bytes::copy_from_slice(&buf[..n]));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    term.output.lock().unwrap().tx = None;
}

/// The mark that keeps what an agent's row does not.
fn extra_key(id: &str) -> String {
    format!("agent-extra:{id}")
}

fn write_private(path: &Path, body: &Json) -> Result<(), ApiError> {
    use std::os::unix::fs::OpenOptionsExt;
    let text = serde_json::to_string_pretty(body).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| ApiError::internal(format!("cannot write {}: {e}", path.display())))?;
    f.write_all(text.as_bytes()).map_err(|e| ApiError::internal(format!("cannot write {}: {e}", path.display())))
}

/// The hooks a session's settings file holds: every event runs
/// `<exe> hook`. Claude Code adds them to the user's own settings.
pub fn settings_json(exe: &str) -> Json {
    let command = format!("{} hook", shell_quote(exe));
    let hook = json!([{ "type": "command", "command": command, "timeout": 5 }]);
    let mut hooks = serde_json::Map::new();
    for ev in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure", "Notification", "Stop", "SubagentStop", "SessionEnd"] {
        let entry = if matches!(ev, "PreToolUse" | "PostToolUse" | "PostToolUseFailure") { json!({ "matcher": "*", "hooks": hook }) } else { json!({ "hooks": hook }) };
        hooks.insert(ev.to_string(), json!([entry]));
    }
    json!({ "hooks": hooks })
}

/// A session's MCP config: the server `workbench`, `<exe> mcp`.
pub fn mcp_json(exe: &str, url: &str, token: &str, agent: &str) -> Json {
    json!({
        "mcpServers": {
            "workbench": {
                "type": "stdio",
                "command": exe,
                "args": ["mcp"],
                "env": {
                    "CHLOEDB_WORKBENCH_URL": url,
                    "CHLOEDB_WORKBENCH_TOKEN": token,
                    "CHLOEDB_WORKBENCH_AGENT": agent,
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(slug(Some("Fix the login bug, then the logout one")), "fix-the-login-bug");
        assert_eq!(slug(None), "agent");
        assert!(valid_name("fix-login-2"));
        assert!(!valid_name("Fix"));
        assert!(!valid_name("-x"));
        assert!(!valid_name(&"a".repeat(33)));
    }

    #[test]
    fn summaries() {
        let h = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "ls -la\nmore"}});
        assert_eq!(summarize("PreToolUse", &h), "Bash: ls -la");
        assert_eq!(summarize("Notification", &json!({"message": "Claude needs your permission to use Bash"})), "Claude needs your permission to use Bash");
        assert!(is_idle_notice(&json!({"message": "Claude is waiting for your input"})));
        assert!(is_idle_notice(&json!({"notification_type": "idle_prompt", "message": "x"})));
        assert!(!is_idle_notice(&json!({"message": "Claude needs your permission to use Bash"})));
        assert_eq!(summarize("Stop", &json!({})), "turn finished");
    }

    #[test]
    fn uuids() {
        let u = uuid4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
    }

    #[test]
    fn trims_on_a_char_boundary() {
        let s = "é".repeat(10);
        assert_eq!(trim_to(s, 5).len(), 4);
    }
}
