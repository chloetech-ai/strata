//! Flow: where the fleet's time went over a window, from Workbench's
//! events (model, tools, tests, waiting on you, redo, idle), the tokens the
//! sessions' transcripts say they used, and a coach: suggestions, each a
//! function over those rows, some of which Apply carries out.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use serde_json::{json, Value as Json};

use crate::db::{now_ms, Agent, Event};
use crate::git;
use crate::map;
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{ApiError, Supervisor};
use crate::trail::{self, Ran};

/// The most of a transcript read, from its end.
const TRANSCRIPT_MAX: u64 = 64 << 20;
/// How many times something repeats before the coach speaks.
const ALLOW_AFTER: usize = 3;
const FIX_AFTER: usize = 3;
const REREAD_AFTER: usize = 4;
const COLLIDED_AFTER: usize = 2;

pub fn routes() -> Router<App> {
    Router::new().route("/api/flow", get(flow_route)).route("/api/flow/apply", post(apply_route))
}

#[derive(serde::Deserialize)]
struct FlowQ {
    repo: Option<String>,
    minutes: Option<i64>,
}

async fn flow_route(State(app): State<App>, Query(q): Query<FlowQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || flow(&sup, q.repo.as_deref(), q.minutes.unwrap_or(60))).await?).into_response())
}

#[derive(serde::Deserialize)]
struct ApplyB {
    id: Option<String>,
    repo: Option<String>,
    minutes: Option<i64>,
    /// The fix, for a `fix` suggestion.
    text: Option<String>,
}

async fn apply_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: ApplyB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || apply(&sup, b)).await?).into_response())
}

/// A session's settings with the patterns allowed for its repository
/// added to `permissions.allow` (Claude Code adds them to the user's own).
pub fn with_allowed(mut settings: Json, allow: &[String]) -> Json {
    if !allow.is_empty() {
        settings["permissions"] = json!({ "allow": allow });
    }
    settings
}

/// The test runners an agent of a worktree runs without asking: Claude
/// Code's `Bash(<prefix>:*)` rules.
pub(crate) const TEST_PREFIXES: [&str; 30] = [
    "cargo test", "cargo nextest", "pytest", "py.test", "python -m pytest", "python3 -m pytest", "npm test", "npm run test", "yarn test", "pnpm test", "bun test", "deno test",
    "go test", "jest", "vitest", "npx jest", "npx vitest", "mvn test", "./gradlew test", "gradle test", "make test", "make check", "rspec", "bundle exec rspec", "phpunit", "ctest",
    "tox", "mix test", "dotnet test", "uv run pytest",
];

/// The git commands that stay in the worktree (no push, fetch or pull, no
/// reset or clean).
const LOCAL_GIT: [&str; 20] = [
    "status", "diff", "log", "show", "add", "commit", "stash", "branch", "checkout", "switch", "restore", "rebase", "merge", "rev-parse", "blame", "grep", "ls-files", "cherry-pick", "mv", "rm",
];

/// What an agent of a worktree Workbench made may do there without asking
/// (SPEC.md, "Sixth build: Safe answering"): edit and read files inside its
/// worktree (Claude Code's `Edit` rule covers Write, MultiEdit and
/// NotebookEdit, its `Read` rule Grep and Glob), run the tests, use git
/// locally, and use Workbench's and ChloeCtx's tools. Guarded commands are
/// still denied by their hook (src/guarded.rs), and anything else, or
/// outside the worktree, still asks.
pub fn worktree_allow(worktree: &Path) -> Vec<String> {
    let inside = format!("/{}/**", worktree.display());
    let mut out = vec![format!("Edit({inside})"), format!("Read({inside})")];
    out.extend(TEST_PREFIXES.iter().map(|p| format!("Bash({p}:*)")));
    out.extend(LOCAL_GIT.iter().map(|g| format!("Bash(git {g}:*)")));
    out.extend(["mcp__workbench".to_string(), "mcp__chloectx".to_string()]);
    out
}

/// `settings` with [`worktree_allow`] added to its `permissions.allow`.
pub fn with_worktree_defaults(mut settings: Json, worktree: &Path) -> Json {
    let mut list = settings.pointer("/permissions/allow").and_then(Json::as_array).cloned().unwrap_or_default();
    for a in worktree_allow(worktree) {
        if !list.contains(&json!(a)) {
            list.push(json!(a));
        }
    }
    if !settings["permissions"].is_object() {
        settings["permissions"] = json!({});
    }
    settings["permissions"]["allow"] = Json::Array(list);
    settings
}

// Commands.

/// The words of a command's simple commands (split at `&&`, `||`, `;` and
/// `|`), without leading variable assignments and wrappers.
fn simple_commands(cmd: &str) -> Vec<Vec<String>> {
    let mut parts = Vec::new();
    for part in cmd.split("&&").flat_map(|p| p.split("||")).flat_map(|p| p.split([';', '|', '\n'])) {
        let mut words: Vec<String> = part.split_whitespace().map(str::to_string).collect();
        while words.first().is_some_and(|w| (w.contains('=') && !w.starts_with('=') && !w.starts_with('-')) || matches!(w.as_str(), "time" | "env" | "nice" | "command")) {
            words.remove(0);
        }
        if !words.is_empty() {
            parts.push(words);
        }
    }
    parts
}

/// Whether a command runs a test runner.
pub fn is_test(cmd: &str) -> bool {
    const RUNNERS: [&[&str]; 33] = [
        &["cargo", "test"], &["cargo", "nextest"], &["pytest"], &["py.test"], &["python", "-m", "pytest"], &["python3", "-m", "pytest"], &["npm", "test"], &["npm", "t"],
        &["yarn", "test"], &["pnpm", "test"], &["bun", "test"], &["deno", "test"], &["go", "test"], &["jest"], &["vitest"], &["npx", "jest"], &["npx", "vitest"],
        &["mvn", "test"], &["mvn", "verify"], &["./gradlew", "test"], &["gradle", "test"], &["make", "test"], &["make", "check"], &["rspec"], &["bundle", "exec", "rspec"],
        &["phpunit"], &["ctest"], &["tox"], &["mix", "test"], &["dotnet", "test"], &["swift", "test"], &["uv", "run", "pytest"], &["poetry", "run", "pytest"],
    ];
    simple_commands(cmd).iter().any(|w| {
        RUNNERS.iter().any(|r| w.len() >= r.len() && r.iter().zip(w).all(|(a, b)| a == b))
            || (w.len() >= 3 && matches!(w[0].as_str(), "npm" | "pnpm" | "yarn" | "bun") && w[1] == "run" && w[2].starts_with("test"))
    })
}

/// The prefix of a command worth allowing: its first two words (three
/// after `npm run` and the like), or `None` for one never to allow
/// wholesale.
pub fn allow_prefix(cmd: &str) -> Option<String> {
    let words = simple_commands(cmd).into_iter().find(|w| w[0] != "cd")?;
    let first = words[0].as_str();
    if matches!(first, "rm" | "sudo" | "su" | "dd" | "mkfs" | "chmod" | "chown" | "curl" | "wget" | "ssh" | "scp" | "kill" | "pkill" | "killall" | "shutdown" | "reboot" | "eval" | "sh" | "bash" | "zsh") {
        return None;
    }
    let plain = |w: &String| !w.starts_with('-') && !w.contains(['/', '"', '\'', '$', '`', '(', ')', '*']);
    let three = words.len() >= 3 && matches!((first, words[1].as_str()), ("npm" | "pnpm" | "yarn" | "uv" | "poetry", "run") | ("python" | "python3", "-m") | ("bundle", "exec"));
    let n = if three && plain(&words[2]) {
        3
    } else if words.len() >= 2 && plain(&words[1]) {
        2
    } else {
        1
    };
    let prefix = words[..n].join(" ");
    if prefix.starts_with("git push") || prefix.starts_with("git reset") || !plain(&words[0]) {
        return None;
    }
    Some(prefix)
}

/// An error's text as a key: its line that says error (else its first),
/// numbers masked, at most 160 characters.
pub fn error_key(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let line = lines.iter().find(|l| l.to_ascii_lowercase().contains("error")).or(lines.first()).copied().unwrap_or("");
    let mut out = String::new();
    let mut in_num = false;
    for c in line.chars() {
        if c.is_ascii_digit() {
            if !in_num {
                out.push('N');
            }
            in_num = true;
        } else {
            in_num = false;
            out.push(c);
        }
    }
    out.chars().take(160).collect()
}

fn hash8(s: &str) -> String {
    git::hash8(Path::new(s))
}

// Transcripts.

#[derive(Default, Clone, Copy)]
struct Tokens {
    input: i64,
    output: i64,
    cache_read: i64,
}

impl Tokens {
    fn add(&mut self, o: Tokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
    }
    fn to_json(self) -> Json {
        json!({ "input": self.input, "output": self.output, "cache_read": self.cache_read })
    }
}

/// What a session's transcript says: each assistant message's usage (by
/// message id, with its time) and each failed tool call's error text (by
/// tool use id).
#[derive(Default)]
struct Transcript {
    usage: HashMap<String, (i64, Tokens)>,
    errors: HashMap<String, String>,
}

fn text_of(content: &Json) -> String {
    match content {
        Json::String(s) => s.clone(),
        Json::Array(a) => a.iter().filter_map(|c| c.get("text").and_then(Json::as_str).or_else(|| c.as_str())).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn read_transcript(path: &Path, into: &mut Transcript) {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return;
    }
    let Ok(mut f) = std::fs::File::open(path) else { return };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > TRANSCRIPT_MAX && f.seek(SeekFrom::Start(len - TRANSCRIPT_MAX)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if f.take(TRANSCRIPT_MAX).read_to_end(&mut buf).is_err() {
        return;
    }
    for line in String::from_utf8_lossy(&buf).lines() {
        let usage = line.contains("\"usage\"");
        // Claude Code writes compact JSON; other writers may not, so parse to be sure.
        let error = line.contains("\"tool_result\"") && line.contains("\"is_error\"");
        if !usage && !error {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Json>(line) else { continue };
        let msg = &v["message"];
        if usage {
            if let Some(u) = msg.get("usage").filter(|u| u.is_object()) {
                let n = |k: &str| u.get(k).and_then(Json::as_i64).unwrap_or(0);
                let id = msg["id"].as_str().or_else(|| v["uuid"].as_str()).unwrap_or("").to_string();
                let ts = v["timestamp"].as_str().and_then(chloedb::timestamp::parse).map_or(0, |t| t / 1000);
                into.usage.insert(id, (ts, Tokens { input: n("input_tokens"), output: n("output_tokens"), cache_read: n("cache_read_input_tokens") }));
            }
        }
        if error {
            for c in msg["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "tool_result" && c["is_error"] == true) {
                if let Some(id) = c["tool_use_id"].as_str() {
                    into.errors.insert(id.to_string(), text_of(&c["content"]));
                }
            }
        }
    }
}

// Segments.

/// What a stretch of an agent's time was.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Model,
    Tools,
    Tests,
    Waiting,
    Redo,
    Idle,
}

impl Kind {
    const ALL: [Kind; 6] = [Kind::Model, Kind::Tools, Kind::Tests, Kind::Waiting, Kind::Redo, Kind::Idle];
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Model => "model",
            Kind::Tools => "tools",
            Kind::Tests => "tests",
            Kind::Waiting => "waiting",
            Kind::Redo => "redo",
            Kind::Idle => "idle",
        }
    }
}

/// A tool call that failed.
struct Failure {
    ts: i64,
    key: String,
    text: String,
    /// The agent's segments that were the call.
    segs: std::ops::Range<usize>,
}

/// A tool call the user approved at a permission prompt.
struct Approval {
    ts: i64,
    command: String,
    waited_ms: i64,
}

#[derive(Default)]
struct Timeline {
    segs: Vec<(i64, i64, Kind)>,
    failures: Vec<Failure>,
    approvals: Vec<Approval>,
}

struct Call {
    tool: String,
    /// Its input as JSON text: what a PostToolUse without an id is
    /// matched by.
    input: String,
    command: Option<String>,
    use_id: Option<String>,
    test: bool,
    seg_from: usize,
    asked: bool,
}

fn is_permission(ev: &Event, d: &Json) -> bool {
    if let Some(t) = d.get("notification_type").and_then(Json::as_str) {
        return t.contains("permission");
    }
    ev.summary.as_deref().or(d["message"].as_str()).is_some_and(|m| m.to_ascii_lowercase().contains("permission"))
}

fn is_idle_notice(ev: &Event, d: &Json) -> bool {
    if let Some(t) = d.get("notification_type").and_then(Json::as_str) {
        return t == "idle_prompt";
    }
    ev.summary.as_deref().or(d["message"].as_str()).is_some_and(|m| m.to_ascii_lowercase().contains("waiting for your input"))
}

/// The error a finished tool call reports, if it failed: the hook's own
/// word for it, else the transcript's.
fn call_error(kind: &str, d: &Json, call: &Call, tr: &Transcript) -> Option<String> {
    if kind == "PostToolUseFailure" {
        return Some(d["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("{} failed", call.tool)));
    }
    let r = &d["tool_response"];
    if r.get("is_error") == Some(&Json::Bool(true)) || r.get("error").and_then(Json::as_str).is_some_and(|e| !e.is_empty()) {
        let text = r["error"].as_str().map(str::to_string).or_else(|| r["stderr"].as_str().map(str::to_string)).unwrap_or_else(|| text_of(&r["content"]));
        return Some(text);
    }
    call.use_id.as_ref().and_then(|id| tr.errors.get(id).cloned())
}

/// What an agent does while its calls are open: tests when one runs a
/// test runner, tools when any runs, else the model (in a turn) or idle.
fn busy(open: &[Call], in_turn: bool) -> Kind {
    if open.iter().any(|c| c.test) {
        Kind::Tests
    } else if !open.is_empty() {
        Kind::Tools
    } else if in_turn {
        Kind::Model
    } else {
        Kind::Idle
    }
}

/// An agent's time from its events: each event ends the stretch before it
/// and starts the one its state implies. Calls may overlap: a PostToolUse
/// ends the call of its `tool_use_id`, else the newest open one of its
/// tool and input, else of its tool.
fn timeline(evs: &[Event], end_ms: i64, tr: &Transcript) -> Timeline {
    let mut t = Timeline::default();
    let mut cur: Option<(i64, Kind)> = None;
    let mut open: Vec<Call> = Vec::new();
    let mut in_turn = false;
    let close = |t: &mut Timeline, cur: &mut Option<(i64, Kind)>, at: i64| {
        if let Some((s, k)) = cur.take() {
            if at > s {
                t.segs.push((s, at, k));
            }
        }
    };
    // Calls still open when the turn ends had no PostToolUse: they failed,
    // unless the user was asked (then they were refused).
    let unfinished = |t: &mut Timeline, open: &mut Vec<Call>, at: i64| {
        for c in open.drain(..).filter(|c| !c.asked) {
            let text = c.use_id.as_ref().and_then(|id| tr.errors.get(id).cloned()).unwrap_or_else(|| format!("{} failed: {}", c.tool, c.command.as_deref().unwrap_or("")));
            t.failures.push(Failure { ts: at, key: error_key(&text), text, segs: c.seg_from..t.segs.len() });
        }
    };
    for ev in evs {
        let d = trail::data_of(ev).unwrap_or(Json::Null);
        let next: Option<Kind> = match ev.kind.as_str() {
            "spawned" | "SessionStart" => Some(busy(&open, in_turn)),
            "UserPromptSubmit" => {
                close(&mut t, &mut cur, ev.ts);
                unfinished(&mut t, &mut open, ev.ts);
                in_turn = true;
                Some(Kind::Model)
            }
            "PreToolUse" => {
                close(&mut t, &mut cur, ev.ts);
                let command = d["tool_input"]["command"].as_str().map(str::to_string);
                let test = ev.tool.as_deref() == Some("Bash") && command.as_deref().is_some_and(is_test);
                open.push(Call { tool: ev.tool.clone().unwrap_or_default(), input: d["tool_input"].to_string(), command, use_id: d["tool_use_id"].as_str().map(str::to_string), test, seg_from: t.segs.len(), asked: false });
                in_turn = true;
                Some(busy(&open, in_turn))
            }
            "PostToolUse" | "PostToolUseFailure" => {
                close(&mut t, &mut cur, ev.ts);
                let tool = ev.tool.clone().unwrap_or_default();
                let id = d["tool_use_id"].as_str();
                let input = d["tool_input"].to_string();
                let at = open
                    .iter()
                    .rposition(|c| id.is_some() && c.use_id.as_deref() == id)
                    .or_else(|| open.iter().rposition(|c| c.tool == tool && c.input == input))
                    .or_else(|| open.iter().rposition(|c| c.tool == tool));
                if let Some(i) = at {
                    let mut c = open.remove(i);
                    if c.use_id.is_none() {
                        c.use_id = id.map(str::to_string);
                    }
                    if let Some(text) = call_error(&ev.kind, &d, &c, tr) {
                        t.failures.push(Failure { ts: ev.ts, key: error_key(&text), text, segs: c.seg_from..t.segs.len() });
                    } else if c.asked && c.tool == "Bash" {
                        let waited_ms = t.segs[c.seg_from..].iter().filter(|s| s.2 == Kind::Waiting).map(|s| s.1 - s.0).sum();
                        if let Some(command) = c.command.clone() {
                            t.approvals.push(Approval { ts: ev.ts, command, waited_ms });
                        }
                    }
                }
                Some(busy(&open, in_turn))
            }
            "Notification" if is_idle_notice(ev, &d) && !in_turn => None,
            "Notification" => {
                close(&mut t, &mut cur, ev.ts);
                if let Some(c) = open.last_mut() {
                    c.asked |= is_permission(ev, &d);
                }
                Some(Kind::Waiting)
            }
            "input" if cur.is_some_and(|c| c.1 == Kind::Waiting) => {
                close(&mut t, &mut cur, ev.ts);
                Some(busy(&open, in_turn))
            }
            "Stop" => {
                close(&mut t, &mut cur, ev.ts);
                unfinished(&mut t, &mut open, ev.ts);
                in_turn = false;
                Some(Kind::Idle)
            }
            "exited" | "SessionEnd" => {
                close(&mut t, &mut cur, ev.ts);
                open.clear();
                in_turn = false;
                None
            }
            _ => None,
        };
        if let Some(k) = next {
            if cur.map(|c| c.1) != Some(k) {
                close(&mut t, &mut cur, ev.ts);
                cur = Some((ev.ts, k));
            }
        }
    }
    close(&mut t, &mut cur, end_ms);
    t
}

/// Segments clipped to `[from, to]`, neighbours of one kind joined.
fn clip(segs: &[(i64, i64, Kind)], from: i64, to: i64) -> Vec<(i64, i64, Kind)> {
    let mut out: Vec<(i64, i64, Kind)> = Vec::new();
    for &(s, e, k) in segs {
        let (s, e) = (s.max(from), e.min(to));
        if e <= s {
            continue;
        }
        match out.last_mut() {
            Some(l) if l.2 == k && l.1 >= s => l.1 = l.1.max(e),
            _ => out.push((s, e, k)),
        }
    }
    out
}

fn dur(ms: i64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", (s + 30) / 60)
    } else {
        format!("{:.1} h", s as f64 / 3600.0)
    }
}

// The flow.

struct AgentFlow {
    agent: Agent,
    tl: Timeline,
    segs: Vec<(i64, i64, Kind)>,
    tokens: Tokens,
    /// Read calls in the window: path → count.
    reads: BTreeMap<String, usize>,
    /// Edit calls in the window: (time, path).
    edits: Vec<(i64, String)>,
}

fn window(minutes: i64) -> (i64, i64, i64) {
    let minutes = minutes.clamp(1, 7 * 24 * 60);
    let to = now_ms();
    (to - minutes * 60_000, to, minutes)
}

fn agent_flows(sup: &Supervisor, main: &Path, from: i64, to: i64) -> Vec<AgentFlow> {
    let repo = main.to_string_lossy().to_string();
    let mut agents: Vec<Agent> = sup.agent_rows().into_iter().filter(|a| a.repo == repo && a.ended.is_none_or(|e| e >= from) && a.created <= to).collect();
    agents.sort_by_key(|a| (a.created, a.id.clone()));
    let mut out = Vec::new();
    for a in agents {
        let evs = sup.all_events(&a.id).unwrap_or_default();
        let mut tr = Transcript::default();
        let paths: HashSet<String> = evs.iter().filter_map(|e| trail::data_of(e).and_then(|d| d["transcript_path"].as_str().map(str::to_string))).collect();
        for p in &paths {
            read_transcript(Path::new(p), &mut tr);
        }
        let end = a.ended.unwrap_or(to).min(to);
        let tl = timeline(&evs, end, &tr);
        let segs = clip(&tl.segs, from, to);
        let mut tokens = Tokens::default();
        for (ts, u) in tr.usage.values() {
            if *ts >= from && *ts <= to {
                tokens.add(*u);
            }
        }
        let cwd = PathBuf::from(&a.cwd);
        let reads = map::reads(&evs, &cwd, main, from).into_iter().map(|(p, (_, n))| (p, n)).collect();
        let edits = evs
            .iter()
            .filter(|e| e.ts >= from && e.kind == "PreToolUse" && e.tool.as_deref().is_some_and(|t| map::EDIT_TOOLS.contains(&t)))
            .filter_map(|e| map::tool_file(e).and_then(|f| map::rel_path(&f, &cwd, main)).map(|p| (e.ts, p)))
            .collect();
        out.push(AgentFlow { agent: a, tl, segs, tokens, reads, edits });
    }
    // Redo: a failure with the error of an earlier one in the window, of
    // any agent; its call's time is redo.
    let mut fails: Vec<(i64, usize, usize)> = Vec::new();
    for (ai, f) in out.iter().enumerate() {
        for (fi, x) in f.tl.failures.iter().enumerate() {
            if x.ts >= from && x.ts <= to {
                fails.push((x.ts, ai, fi));
            }
        }
    }
    fails.sort();
    let mut seen: HashSet<String> = HashSet::new();
    let mut redo: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
    for (_, ai, fi) in fails {
        let x = &out[ai].tl.failures[fi];
        if !seen.insert(x.key.clone()) {
            redo.push((ai, x.segs.clone()));
        }
    }
    for (ai, r) in redo {
        let f = &mut out[ai];
        for s in &mut f.tl.segs[r] {
            if matches!(s.2, Kind::Tools | Kind::Tests) {
                s.2 = Kind::Redo;
            }
        }
        f.segs = clip(&f.tl.segs, from, to);
    }
    out
}

fn sums(segs: &[(i64, i64, Kind)]) -> HashMap<Kind, i64> {
    let mut m = HashMap::new();
    for &(s, e, k) in segs {
        *m.entry(k).or_insert(0) += e - s;
    }
    m
}

fn applied_key(repo: &str, id: &str) -> String {
    format!("flow:{repo}:{id}")
}

/// The coach's suggestions over the window's rows, then the report's.
fn suggestions(sup: &Supervisor, repo: &str, flows: &[AgentFlow], from: i64, to: i64, minutes: i64, report: Option<&Json>) -> Vec<Json> {
    let mut out = Vec::new();
    let per_hour = |ms: i64| ms * 60 / minutes.max(1);
    let allowed: HashSet<String> = sup.allowed(repo).into_iter().collect();

    // Bash prefixes approved again and again.
    let mut approved: BTreeMap<String, (usize, i64, HashSet<String>)> = BTreeMap::new();
    for f in flows {
        for a in f.tl.approvals.iter().filter(|a| a.ts >= from && a.ts <= to) {
            if let Some(p) = allow_prefix(&a.command) {
                let e = approved.entry(p).or_default();
                e.0 += 1;
                e.1 += a.waited_ms;
                e.2.insert(f.agent.name.clone());
            }
        }
    }
    let mut allow: Vec<(String, (usize, i64, HashSet<String>))> = approved.into_iter().filter(|(_, v)| v.0 >= ALLOW_AFTER).collect();
    allow.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(&b.0)));
    for (prefix, (n, waited, _)) in allow {
        let pattern = format!("Bash({prefix}:*)");
        let saves = per_hour(waited);
        out.push(json!({
            "id": format!("allow:{pattern}"),
            "kind": "allow",
            "title": format!("Allow `{prefix}` in this repository"),
            "detail": format!("You approved it {n} times in the last {}; each one stopped an agent for {} on average.", dur(minutes * 60_000), dur(waited / n as i64)),
            "saves": format!("saves ~{}/h", dur(saves)),
            "saves_ms": saves,
            "action": { "kind": "allow", "pattern": pattern },
            "applied": allowed.contains(&pattern),
        }));
    }

    // The same error again and again.
    let mut errors: BTreeMap<String, (usize, String, Vec<String>, i64)> = BTreeMap::new();
    for f in flows {
        for x in f.tl.failures.iter().filter(|x| x.ts >= from && x.ts <= to) {
            let e = errors.entry(x.key.clone()).or_insert_with(|| (0, x.text.clone(), Vec::new(), 0));
            e.0 += 1;
            if !e.2.contains(&f.agent.name) {
                e.2.push(f.agent.name.clone());
            }
        }
        // What redo cost, by the error it repeated.
        for x in &f.tl.failures {
            let ms: i64 = f.tl.segs[x.segs.clone()].iter().filter(|s| s.2 == Kind::Redo).map(|s| s.1.min(to) - s.0.max(from)).filter(|d| *d > 0).sum();
            if let Some(e) = errors.get_mut(&x.key) {
                e.3 += ms;
            }
        }
    }
    for (key, (n, text, names, redo_ms)) in errors.into_iter().filter(|(_, v)| v.0 >= FIX_AFTER) {
        let id = format!("fix:{}", hash8(&key));
        let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(160).collect::<String>();
        out.push(json!({
            "id": id,
            "kind": "fix",
            "title": format!("{} hit “{}” {n} times", names.join(" and "), line),
            "detail": "Write the fix down once: ChloeCtx keeps it, and recalls it for the next agent that hits this error.",
            "saves": format!("saves ~{}", dur(redo_ms)),
            "saves_ms": redo_ms,
            "action": { "kind": "fix", "error": line },
            "applied": sup.mark(&applied_key(repo, &id)).is_some(),
        }));
    }

    // Files read again and again by one agent.
    for f in flows {
        let files: Vec<(&String, &usize)> = f.reads.iter().filter(|(_, n)| **n >= REREAD_AFTER).collect();
        if files.is_empty() {
            continue;
        }
        let title = match files.as_slice() {
            [(p, n)] => format!("{} re-read {p} {n} times", f.agent.name),
            _ => format!("{} re-read {} files {}+ times", f.agent.name, files.len(), REREAD_AFTER),
        };
        out.push(json!({
            "id": format!("capsule:{}", f.agent.id),
            "kind": "capsule",
            "title": title,
            "detail": "It starts cold each turn. Put what it keeps looking up in its task, or remember it (chloectx remember fact \"…\") so that recall brings it back, rather than reading the files again.",
            "saves": null,
            "saves_ms": null,
            "action": { "kind": "capsule", "agent": f.agent.id, "files": files.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>() },
            "applied": false,
        }));
    }

    // Agents colliding in one file: the editing agent changed twice.
    let mut edits: BTreeMap<&str, Vec<(i64, &Agent)>> = BTreeMap::new();
    for f in flows {
        for (ts, p) in &f.edits {
            edits.entry(p.as_str()).or_default().push((*ts, &f.agent));
        }
    }
    for (path, mut es) in edits {
        es.sort_by_key(|e| e.0);
        let switches = es.windows(2).filter(|w| w[0].1.id != w[1].1.id).count();
        if switches < COLLIDED_AFTER {
            continue;
        }
        let mut names: Vec<&str> = Vec::new();
        let mut ids: Vec<&str> = Vec::new();
        for (_, a) in &es {
            if !ids.contains(&a.id.as_str()) {
                ids.push(&a.id);
                names.push(&a.name);
            }
        }
        out.push(json!({
            "id": format!("sequence:{path}"),
            "kind": "sequence",
            "title": format!("{} collided {switches} times in {path}", names.join(" and ")),
            "detail": "Split the work by file, or run them one after the other there.",
            "saves": null,
            "saves_ms": null,
            "action": { "kind": "sequence", "path": path, "agents": ids },
            "applied": false,
        }));
    }

    // The report's own.
    for (i, s) in report.and_then(|r| r["suggestions"].as_array()).into_iter().flatten().enumerate() {
        let secs = s["seconds"].as_i64();
        out.push(json!({
            "id": format!("report:{i}"),
            "kind": s["kind"],
            "title": s["text"],
            "detail": "",
            "saves": secs.map(|x| format!("bears on ~{}", dur(x * 1000))),
            "saves_ms": secs.map(|x| x * 1000),
            "action": null,
            "applied": false,
            "from": "chloectx",
        }));
    }
    out
}

/// `GET /api/flow`.
pub fn flow(sup: &Supervisor, repo: Option<&str>, minutes: i64) -> Result<Json, ApiError> {
    let main = trail::repo_dir(repo)?;
    let repo_s = main.to_string_lossy().to_string();
    let (from, to, minutes) = window(minutes);
    let flows = agent_flows(sup, &main, from, to);
    let mut missing = Vec::new();
    let report = match trail::run_chloectx(&sup.cfg.chloectx_bin, &main, &["report", "--json", "--days", "1"], Duration::from_secs(60)) {
        Ran::Out(v) => Some(v),
        Ran::Missing => {
            missing.push(trail::not_installed(&sup.cfg.chloectx_bin));
            None
        }
        Ran::Failed(e) => {
            missing.push(e);
            None
        }
    };
    let mut totals: HashMap<Kind, i64> = HashMap::new();
    let mut tokens = Tokens::default();
    let mut agents = Vec::new();
    for f in &flows {
        let m = sums(&f.segs);
        let mut a = json!({
            "agent": trail::agent_json(sup, &f.agent),
            "segments": f.segs.iter().map(|(s, e, k)| json!([s, e, k.as_str()])).collect::<Vec<_>>(),
            "tokens": f.tokens.to_json(),
        });
        for k in Kind::ALL {
            let v = m.get(&k).copied().unwrap_or(0);
            a[format!("{}_ms", k.as_str())] = json!(v);
            *totals.entry(k).or_insert(0) += v;
        }
        tokens.add(f.tokens);
        agents.push(a);
    }
    let mut t = json!({});
    for k in Kind::ALL {
        t[format!("{}_ms", k.as_str())] = json!(totals.get(&k).copied().unwrap_or(0));
    }
    Ok(json!({
        "repo": repo_s,
        "window": { "from_ms": from, "to_ms": to, "minutes": minutes },
        "totals": t,
        "agents": agents,
        "tokens": tokens.to_json(),
        "suggestions": suggestions(sup, &repo_s, &flows, from, to, minutes, report.as_ref()),
        "report": report,
        "missing": missing,
    }))
}

/// Runs `<bin> <args>` in `cwd`, for a command that prints text: its
/// output, or why it failed.
fn run_text(bin: &str, cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(bin).args(args).current_dir(cwd).stdin(Stdio::null()).output().map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { trail::not_installed(bin) } else { format!("cannot run {bin}: {e}") })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("chloectx {} failed: {}", args.first().copied().unwrap_or(""), err.trim().lines().last().unwrap_or("").trim_start_matches("chloectx: ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `POST /api/flow/apply {id, repo, minutes?, text?}`.
fn apply(sup: &Supervisor, b: ApplyB) -> Result<Json, ApiError> {
    let id = b.id.filter(|i| !i.is_empty()).ok_or_else(|| ApiError::bad("id is required: a suggestion's id"))?;
    let main = trail::repo_dir(b.repo.as_deref())?;
    let repo = main.to_string_lossy().to_string();
    if let Some(pattern) = id.strip_prefix("allow:") {
        if !(pattern.starts_with("Bash(") && pattern.ends_with(":*)")) || pattern.contains(['\n', '\r']) || pattern.len() > 200 {
            return Err(ApiError::bad(format!("{pattern:?} is not a pattern this suggestion makes")));
        }
        let mut allowed = sup.allowed(&repo);
        if !allowed.iter().any(|p| p == pattern) {
            allowed.push(pattern.to_string());
        }
        sup.set_mark(&format!("allow:{repo}"), &serde_json::to_string(&allowed).unwrap())?;
        sup.set_mark(&applied_key(&repo, &id), &json!({ "ts": now_ms() }).to_string())?;
        return Ok(json!({ "ok": true, "id": id, "applied": { "kind": "allow", "pattern": pattern }, "allowed": allowed, "note": "Agents started from now on in this repository have it in permissions.allow." }));
    }
    if id.starts_with("fix:") {
        let fix = b.text.as_deref().map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| ApiError::bad("text is required: the fix for this error"))?;
        let (from, to, minutes) = window(b.minutes.unwrap_or(60));
        let flows = agent_flows(sup, &main, from, to);
        let s = suggestions(sup, &repo, &flows, from, to, minutes, None);
        let found = s.iter().find(|x| x["id"] == id.as_str()).ok_or_else(|| ApiError::not_found(format!("no suggestion {id} in the last {minutes} minutes")))?;
        let error = found["action"]["error"].as_str().unwrap_or("").to_string();
        let text = format!("When this error comes back: “{error}”, the fix is: {fix}");
        let out = run_text(&sup.cfg.chloectx_bin, &main, &["remember", "fact", &text]).map_err(ApiError::conflict)?;
        sup.set_mark(&applied_key(&repo, &id), &json!({ "ts": now_ms(), "text": text }).to_string())?;
        return Ok(json!({ "ok": true, "id": id, "applied": { "kind": "fix", "error": error, "text": text }, "chloectx": out }));
    }
    Err(ApiError::bad(format!("{id} has nothing to apply: it says how")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: i64, ts: i64, kind: &str, tool: Option<&str>, data: Json) -> Event {
        Event { id: format!("a:{seq}"), seq, ts, agent: "a".into(), kind: kind.into(), tool: tool.map(str::to_string), summary: data["message"].as_str().map(str::to_string), data: Some(data.to_string()) }
    }

    #[test]
    fn commands() {
        assert!(is_test("cd /w && cargo test --release 2>&1 | tail -20"));
        assert!(is_test("RUST_LOG=1 cargo test -p x"));
        assert!(is_test("npm run test:unit"));
        assert!(is_test("python -m pytest tests"));
        assert!(!is_test("cargo build"));
        assert_eq!(allow_prefix("cargo test -p harbor").as_deref(), Some("cargo test"));
        assert_eq!(allow_prefix("cd /w && npm run lint --fix").as_deref(), Some("npm run lint"));
        assert_eq!(allow_prefix("ls -la").as_deref(), Some("ls"));
        assert_eq!(allow_prefix("rm -rf target"), None);
        assert_eq!(allow_prefix("git push origin main"), None);
        assert_eq!(allow_prefix("git status").as_deref(), Some("git status"));
        assert_eq!(error_key("warning: x\nerror[E0308]: mismatched types at 12:4"), "error[EN]: mismatched types at N:N");
    }

    #[test]
    fn a_timeline_of_events() {
        let tr = Transcript { errors: [("t2".to_string(), "error: sqlx offline data missing".to_string())].into(), ..Default::default() };
        let bash = |cmd: &str, id: &str| json!({ "tool_name": "Bash", "tool_input": { "command": cmd }, "tool_use_id": id });
        let evs = vec![
            ev(1, 0, "spawned", None, json!({})),
            ev(2, 100, "UserPromptSubmit", None, json!({ "prompt": "go" })),
            ev(3, 300, "PreToolUse", Some("Bash"), bash("cargo test", "t1")),
            ev(4, 400, "Notification", None, json!({ "message": "Claude needs your permission to use Bash" })),
            ev(5, 900, "input", None, json!({})),
            ev(6, 1000, "PostToolUse", Some("Bash"), bash("cargo test", "t1")),
            ev(7, 1200, "PreToolUse", Some("Bash"), bash("cargo build", "t2")),
            // Overlapping calls with no ids: matched by tool and input.
            ev(8, 1250, "PreToolUse", Some("Read"), json!({ "tool_name": "Read", "tool_input": { "file_path": "/a" } })),
            ev(9, 1300, "PreToolUse", Some("Read"), json!({ "tool_name": "Read", "tool_input": { "file_path": "/b" } })),
            ev(10, 1350, "PostToolUse", Some("Read"), json!({ "tool_name": "Read", "tool_input": { "file_path": "/a" } })),
            ev(11, 1400, "PostToolUse", Some("Read"), json!({ "tool_name": "Read", "tool_input": { "file_path": "/b" } })),
            ev(12, 1500, "Stop", None, json!({})),
        ];
        let t = timeline(&evs, 2000, &tr);
        let kinds: Vec<(i64, i64, &str)> = t.segs.iter().map(|s| (s.0, s.1, s.2.as_str())).collect();
        assert_eq!(kinds, [(0, 100, "idle"), (100, 300, "model"), (300, 400, "tests"), (400, 900, "waiting"), (900, 1000, "tests"), (1000, 1200, "model"), (1200, 1250, "tools"), (1250, 1300, "tools"), (1300, 1350, "tools"), (1350, 1400, "tools"), (1400, 1500, "tools"), (1500, 2000, "idle")]);
        assert_eq!(t.approvals.len(), 1);
        assert_eq!((t.approvals[0].command.as_str(), t.approvals[0].waited_ms), ("cargo test", 500));
        assert_eq!(t.failures.len(), 1);
        assert_eq!(t.failures[0].key, "error: sqlx offline data missing");
        assert_eq!(t.failures[0].segs, 6..11);
        assert_eq!(clip(&t.segs, 350, 1100).first().copied(), Some((350, 400, Kind::Tests)));
        assert_eq!(clip(&t.segs, 1100, 1600), [(1100, 1200, Kind::Model), (1200, 1500, Kind::Tools), (1500, 1600, Kind::Idle)]);
    }
}
