//! A capsule with every pull request (SPEC.md, "Fourth build", use case
//! 11). `POST /api/agents/:id/capsule` runs `chloectx pack` in the agent's
//! worktree into `<session dir>/capsule`: the decisions, rules, facts,
//! prompts and answers ChloeCtx kept since the agent started (`pack` has
//! no session filter, so it is the repository's memory over that time).
//! `GET /api/agents/:id/pr` drafts the pull request: a title, and a body
//! with a summary, the decisions behind the change (◆ ids), its blast
//! radius, the tests it ran, and how to ask the capsule (`chloectx
//! mount`); the body is saved to `<session dir>/pr.md` for `gh pr create
//! --body-file`. Workbench never opens the pull request itself.
//! `POST /api/agents/:id/capsule/ask` asks the capsule (`chloectx mount`
//! over stdio, its `capsule_ask` tool: search, no model), or, with no
//! capsule yet, `chloectx ask --json` in the worktree.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use serde_json::{json, Value as Json};

use crate::blast::{self, git_out, plural};
use crate::db::{iso, now_ms, Agent};
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{ApiError, Supervisor};
use crate::trail::{self, Ran, Sessions};

/// How long packing may take (it copies the store and opens its embedder).
const PACK_TIMEOUT: Duration = Duration::from_secs(600);
/// How long asking the capsule may take (mounting checks its hash).
const ASK_TIMEOUT: Duration = Duration::from_secs(120);

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/agents/:id/capsule", get(get_route).post(pack_route))
        .route("/api/agents/:id/capsule/ask", post(ask_route))
        .route("/api/agents/:id/pr", get(pr_route))
        .route("/api/agents/:id/pr/stack", get(pr_stack_route))
}

async fn pr_stack_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || pr_stack(&sup, &id)).await?).into_response())
}

async fn get_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || get_capsule(&sup, &id)).await?).into_response())
}

async fn pack_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || pack(&sup, &id)).await?).into_response())
}

#[derive(serde::Deserialize)]
struct AskB {
    question: Option<String>,
}

async fn ask_route(State(app): State<App>, AxPath(id): AxPath<String>, body: Bytes) -> ApiResult {
    let b: AskB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || ask(&sup, &id, b.question.as_deref().unwrap_or(""))).await?).into_response())
}

async fn pr_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || pr(&sup, &id)).await?).into_response())
}

fn agent_of(sup: &Supervisor, id: &str) -> Result<Agent, ApiError> {
    sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))
}

/// Where ChloeCtx runs for an agent: its worktree, else its repository.
fn place(a: &Agent) -> PathBuf {
    let cwd = PathBuf::from(&a.cwd);
    if cwd.is_dir() {
        cwd
    } else {
        PathBuf::from(&a.repo)
    }
}

fn capsule_dir(sup: &Supervisor, id: &str) -> PathBuf {
    sup.session_dir(id).join("capsule")
}

fn mark_key(id: &str) -> String {
    format!("capsule:{id}")
}

/// A span as people say it: 52 min, 3 h 5 min.
fn span(ms: i64) -> String {
    let m = (ms / 60_000).max(1);
    if m < 60 {
        format!("{m} min")
    } else {
        format!("{} h {} min", m / 60, m % 60)
    }
}

fn first_sentence(s: &str, max: usize) -> String {
    let s = s.trim();
    let end = s.find(['.', '\n', '!', '?']).unwrap_or(s.len());
    crate::supervisor::one_line(&s[..end], max)
}

/// The capsule kept for an agent, if its directory is still there.
fn stored(sup: &Supervisor, id: &str) -> Option<Json> {
    let v: Json = serde_json::from_str(&sup.mark(&mark_key(id))?).ok()?;
    Path::new(v["dir"].as_str()?).join("manifest.json").is_file().then_some(v)
}

/// `GET /api/agents/:id/capsule`.
pub fn get_capsule(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    agent_of(sup, id)?;
    Ok(json!({ "available": true, "capsule": stored(sup, id) }))
}

/// The capsule as the API answers it, from `chloectx pack --json`'s
/// manifest: its scope is the sessions of the branch's agents when ChloeCtx
/// filtered by them (the manifest names them), else the repository's memory.
fn capsule_json(a: &Agent, dir: &Path, m: &Json, names: &[String]) -> Json {
    let d = dir.to_string_lossy();
    let started = a.started_ms.unwrap_or(a.created);
    let filtered = m["sessions"].as_array().map(|v| v.len());
    let (scope, short) = match filtered {
        Some(n) => (
            format!("{} of this branch's agents ({}), since {} started ({})", plural(n, "session"), names.join(", "), a.name, span(now_ms() - started)),
            format!("{} of this branch's agents ({})", plural(n, "session"), names.join(", ")),
        ),
        None => (
            format!("the repository's memory since {} started ({}), every session in it: this chloectx cannot pack by session", a.name, span(now_ms() - started)),
            "the repository's memory over that time".to_string(),
        ),
    };
    json!({
        "dir": d,
        "title": m["title"],
        "records": m["records"],
        "by_kind": m["by_kind"],
        "bytes": m["bytes"],
        "sha256": m["sha256"],
        "created": m["created_ts"].as_i64().map(|t| iso(t * 1000)).unwrap_or_else(|| iso(now_ms())),
        "since": m["since_ts"].as_i64().map(|t| iso(t * 1000)),
        "pack_ms": m["total_ms"].as_f64().or(m["pack_ms"].as_f64()),
        "scope": scope, "scope_short": short, "sessions": m["sessions"],
        "mount": format!("chloectx mount {d}"),
        "claude": format!("claude mcp add {}-capsule -- chloectx mount {d}", a.name),
    })
}

/// `POST /api/agents/:id/capsule`: packs it again.
pub fn pack(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let a = agent_of(sup, id)?;
    let out = capsule_dir(sup, id);
    std::fs::create_dir_all(sup.session_dir(id)).map_err(|e| ApiError::internal(format!("cannot create {}: {e}", sup.session_dir(id).display())))?;
    // `pack` wants a new directory: the old capsule waits aside until the
    // new one is there.
    let old = out.with_file_name(format!("capsule.old-{}", now_ms()));
    let had = out.exists();
    if had {
        std::fs::rename(&out, &old).map_err(|e| ApiError::internal(format!("cannot move {} aside: {e}", out.display())))?;
    }
    let restore = || {
        if had {
            let _ = std::fs::remove_dir_all(&out);
            let _ = std::fs::rename(&old, &out);
        }
    };
    let started = a.started_ms.unwrap_or(a.created);
    // A minute before it started, in days (`--since-days` takes a fraction).
    let days = ((now_ms() - started + 60_000).max(60_000) as f64) / 86_400_000.0;
    let title = format!("{}: {}", a.name, first_sentence(a.task.as_deref().unwrap_or(""), 60));
    let out_s = out.to_string_lossy().to_string();
    let days_s = format!("{days:.6}");
    // Only the sessions of the agents whose work is in the branch
    // (ChloeCtx reads CHLOECTX_PACK_SESSIONS; an older one packs them all).
    let head = crate::git::rev_parse(&place(&a), "HEAD");
    let mine = branch_agents(sup, &a, head.as_deref());
    let s = Sessions::load(sup);
    let sessions: Vec<String> = mine.iter().flat_map(|m| s.sessions_of(&m.id)).collect();
    let names: Vec<String> = mine.iter().map(|m| m.name.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let env = [("CHLOECTX_PACK_SESSIONS", sessions.join(","))];
    let env: &[(&str, String)] = if sessions.is_empty() { &[] } else { &env };
    let m = match trail::run_chloectx_env(&sup.cfg.chloectx_bin, &place(&a), &["pack", &out_s, "--since-days", &days_s, "--title", title.trim_end_matches(": "), "--json"], env, PACK_TIMEOUT) {
        Ran::Out(m) => m,
        Ran::Missing => {
            restore();
            return Ok(json!({ "available": false, "why": format!("{}. Install it (./install.sh in the ChloeDB checkout) to pack capsules.", trail::not_installed(&sup.cfg.chloectx_bin)) }));
        }
        Ran::Failed(e) => {
            restore();
            return Ok(json!({ "available": false, "why": e }));
        }
    };
    if !out.join("manifest.json").is_file() {
        restore();
        return Ok(json!({ "available": false, "why": format!("chloectx pack wrote no capsule in {out_s}") }));
    }
    if had {
        let _ = std::fs::remove_dir_all(&old);
    }
    let c = capsule_json(&a, &out, &m, &names);
    sup.set_mark(&mark_key(id), &c.to_string())?;
    Ok(json!({ "available": true, "capsule": c }))
}

/// Asks `chloectx mount <dir>` (MCP over stdio) its `capsule_ask` tool:
/// the text it answered, or why not.
fn ask_mounted(bin: &str, dir: &Path, question: &str) -> Result<String, String> {
    let mut child = Command::new(bin).arg("mount").arg(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("cannot run {bin} mount: {e}"))?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    let msgs = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "chloedb-workbench", "version": env!("CARGO_PKG_VERSION") } } }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "capsule_ask", "arguments": { "question": question, "budget": 3000 } } }),
    ];
    for m in &msgs {
        if writeln!(stdin, "{m}").is_err() {
            break;
        }
    }
    let _ = stdin.flush();
    let end = Instant::now() + ASK_TIMEOUT;
    let mut answer: Option<Result<String, String>> = None;
    while answer.is_none() {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) => {
                let Ok(v) = serde_json::from_str::<Json>(&line) else { continue };
                if v["id"] != 2 {
                    continue;
                }
                answer = Some(if let Some(e) = v.get("error") {
                    Err(e["message"].as_str().unwrap_or("error").to_string())
                } else {
                    let text = v["result"]["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n");
                    if v["result"]["isError"] == true {
                        Err(text)
                    } else {
                        Ok(text)
                    }
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => answer = Some(Err(format!("chloectx mount did not answer within {} s", ASK_TIMEOUT.as_secs()))),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let err = err_reader.join().unwrap_or_default();
    answer.unwrap_or_else(|| {
        let why = err.lines().last().unwrap_or("").trim_start_matches("chloectx mount: ").trim().to_string();
        Err(if why.is_empty() { "chloectx mount ended without an answer".into() } else { format!("chloectx mount failed: {why}") })
    })
}

/// The record ids an answer names: the trailer `(capsule "…", records: a, b)`
/// and `[id]` marks.
fn ids_in(text: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for l in text.lines() {
        if let Some(i) = l.find("records: ") {
            if l.trim_start().starts_with("(capsule") {
                ids.extend(l[i + 9..].trim_end_matches(')').split(", ").map(|s| s.trim().to_string()).filter(|s| s.contains(':')));
            }
        }
        let mut rest = l;
        while let Some(i) = rest.find('[') {
            let Some(j) = rest[i..].find(']') else { break };
            let id = &rest[i + 1..i + j];
            if id.contains(':') && !id.contains(' ') && id.len() < 40 {
                ids.push(id.to_string());
            }
            rest = &rest[i + j..];
        }
    }
    let mut seen = HashSet::new();
    ids.retain(|i| seen.insert(i.clone()));
    ids
}

/// `POST /api/agents/:id/capsule/ask`.
pub fn ask(sup: &Supervisor, id: &str, question: &str) -> Result<Json, ApiError> {
    let a = agent_of(sup, id)?;
    let q = question.trim();
    if q.is_empty() {
        return Err(ApiError::bad("question is empty"));
    }
    let t0 = Instant::now();
    let bin = sup.cfg.chloectx_bin.clone();
    let s = Sessions::load(sup);
    let agents_of = |ids: &[String]| -> Json {
        let mut m = serde_json::Map::new();
        for i in ids {
            if let Some(ag) = s.of_record(i) {
                m.insert(i.clone(), trail::agent_json(sup, ag));
            }
        }
        Json::Object(m)
    };
    if let Some(c) = stored(sup, id) {
        let dir = PathBuf::from(c["dir"].as_str().unwrap_or(""));
        return Ok(match ask_mounted(&bin, &dir, q) {
            Ok(text) => {
                let ids = ids_in(&text);
                json!({ "available": true, "via": "capsule", "question": q, "text": text, "ids": ids, "agents": agents_of(&ids), "ms": t0.elapsed().as_millis() as u64, "missing": [] })
            }
            Err(e) if e.contains("No such file") || e.starts_with("cannot run") => json!({ "available": false, "why": format!("{e}. {}", trail::not_installed(&bin)) }),
            Err(e) => json!({ "available": false, "why": e }),
        });
    }
    // No capsule: the repository's memory, which `ask` reads whole.
    Ok(match trail::run_chloectx(&bin, &place(&a), &["ask", "--json", "--", q], Duration::from_secs(300)) {
        Ran::Out(v) => {
            let text = v["text"].as_str().or(v["answer"].as_str()).unwrap_or("").to_string();
            let mut ids: Vec<String> = v["ids"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
            if ids.is_empty() {
                ids = ids_in(&text);
            }
            let mine: Vec<&String> = ids.iter().filter(|i| s.of_record(i).is_some_and(|ag| ag.id == a.id)).collect();
            json!({
                "available": true, "via": "repo", "question": q, "text": text, "ids": ids, "agents": agents_of(&ids), "ms": t0.elapsed().as_millis() as u64,
                "missing": [format!("There is no capsule yet, so the repository's memory answered (all sessions; {} from {}'s).", if mine.len() == 1 { "1 of these records is".to_string() } else { format!("{} of these records are", mine.len()) }, a.name)],
            })
        }
        Ran::Missing => json!({ "available": false, "why": trail::not_installed(&bin) }),
        Ran::Failed(e) => json!({ "available": false, "why": e }),
    })
}

/// The counts a test summary line says: passed and failed (failed counts
/// errors too), when the line says any.
fn counts_of(line: &str) -> Option<(u64, u64)> {
    let w: Vec<&str> = line.split(|c: char| !c.is_alphanumeric()).filter(|x| !x.is_empty()).collect();
    let mut passed = None;
    let mut failed = 0u64;
    for p in w.windows(2) {
        let Ok(n) = p[0].parse::<u64>() else { continue };
        match p[1] {
            "passed" | "passing" | "pass" => passed = Some(passed.unwrap_or(0) + n),
            "failed" | "failing" | "errors" | "error" | "fail" => failed += n,
            _ => {}
        }
    }
    // cargo: "test result: ok. 12 passed; 0 failed".
    passed.map(|p| (p, failed)).or_else(|| (failed > 0).then_some((0, failed)))
}

/// The test commands an agent ran, from its Bash calls, when Workbench
/// has no test runs table: the command, ok (from the summary line's counts
/// first, then the exit), the summary line, when.
fn tests_run(sup: &Supervisor, id: &str) -> Vec<Json> {
    let evs = sup.all_events(id).unwrap_or_default();
    let mut out = Vec::new();
    for ev in &evs {
        if ev.tool.as_deref() != Some("Bash") || !matches!(ev.kind.as_str(), "PostToolUse" | "PostToolUseFailure") {
            continue;
        }
        let Some(d) = trail::data_of(ev) else { continue };
        let cmd = d["tool_input"]["command"].as_str().unwrap_or("");
        if !crate::flow::is_test(cmd) {
            continue;
        }
        let r = &d["tool_response"];
        let text = [r["stdout"].as_str(), r["stderr"].as_str(), r.as_str(), r["output"].as_str()].into_iter().flatten().collect::<Vec<_>>().join("\n");
        let summary_line = text.lines().rev().find(|l| counts_of(l).is_some() || l.contains("test result:"));
        let summary = summary_line.map(|l| crate::supervisor::one_line(l.trim(), 140));
        let counts = summary_line.and_then(counts_of);
        let ok = match counts {
            Some((_, f)) => Some(f == 0),
            None if ev.kind == "PostToolUseFailure" || r["is_error"] == true || r["interrupted"] == true => Some(false),
            None => None,
        };
        out.push(json!({ "kind": "command", "name": crate::supervisor::one_line(cmd, 160), "command": crate::supervisor::one_line(cmd, 160), "ok": ok, "summary": summary, "ts": iso(ev.ts) }));
    }
    out
}

/// The agents whose work is in a branch: the agent, its earlier runs in
/// the same worktree (a restart), and the agents it spawned whose work is
/// in its worktree or its branch.
fn branch_agents(sup: &Supervisor, a: &Agent, head: Option<&str>) -> Vec<Agent> {
    let main = PathBuf::from(&a.repo);
    let mut out = vec![a.clone()];
    for o in sup.agent_rows() {
        if o.id == a.id || o.repo != a.repo {
            continue;
        }
        let same_tree = o.cwd == a.cwd && a.cwd != a.repo;
        let child_in = o.parent.as_deref() == Some(a.id.as_str())
            && (same_tree || o.branch.as_deref().and_then(|b| crate::git::rev_parse(&main, &format!("refs/heads/{b}"))).zip(head).is_some_and(|(t, h)| crate::git::is_ancestor(&main, &t, h)));
        if same_tree || child_in {
            out.push(o);
        }
    }
    out
}

/// The decisions, preferences and facts of the branch's agents' sessions
/// since the first of them started: `chloectx sql --json` in its worktree.
fn session_decisions(sup: &Supervisor, a: &Agent, mine: &[Agent], missing: &mut Vec<String>) -> Vec<Json> {
    let since = mine.iter().map(|m| m.started_ms.unwrap_or(m.created)).min().unwrap_or(a.created) / 1000 - 60;
    let sql = format!("SELECT id, kind, text, ts, session, tag, links FROM memory WHERE (kind = 'decision' OR kind = 'preference' OR kind = 'fact') AND ts >= {since} ORDER BY ts LIMIT 500");
    let out = match trail::run_chloectx(&sup.cfg.chloectx_bin, &place(a), &["sql", "--json", &sql], Duration::from_secs(60)) {
        Ran::Out(v) => v,
        Ran::Missing => {
            missing.push(trail::not_installed(&sup.cfg.chloectx_bin));
            return Vec::new();
        }
        Ran::Failed(e) => {
            missing.push(e);
            return Vec::new();
        }
    };
    let s = Sessions::load(sup);
    let ids: HashSet<&str> = mine.iter().map(|m| m.id.as_str()).collect();
    let cols: Vec<String> = out["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
    let mut list = Vec::new();
    for row in out["rows"].as_array().into_iter().flatten().filter_map(Json::as_array) {
        let col = |n: &str| cols.iter().position(|c| c == n).and_then(|i| row.get(i).cloned()).unwrap_or(Json::Null);
        let rid = col("id").as_str().unwrap_or("").to_string();
        let ag = col("session").as_str().and_then(|sid| s.agent(sid)).or_else(|| s.of_record(&rid));
        if rid.is_empty() || !ag.is_some_and(|ag| ids.contains(ag.id.as_str())) {
            continue;
        }
        let links = col("links").to_string();
        let superseded_by = links.split("superseded_by").nth(1).and_then(|r| r.split('"').find(|p| p.contains(':') && !p.contains(' ') && p.len() > 2)).map(|x| x.trim_start_matches(':').to_string());
        // What it replaced (Review's "decisions that matter").
        let supersedes = links.split("supersedes").nth(1).and_then(|r| r.split('"').find(|p| p.contains(':') && !p.contains(' ') && p.len() > 2)).map(|x| x.trim_start_matches(':').to_string());
        list.push(json!({
            "id": rid, "kind": col("kind"), "text": col("text"), "ts": trail::ctx_time(&col("ts")), "agent": ag.map(|g| g.name.clone()),
            "superseded": col("tag") == "superseded" || superseded_by.is_some(), "superseded_by": superseded_by, "supersedes": supersedes, "via": ["session"],
        }));
    }
    list
}

/// The texts of records by id (`chloectx sql`), for the successors of
/// superseded decisions.
pub(crate) fn record_texts(sup: &Supervisor, a: &Agent, ids: &[String]) -> HashMap<String, String> {
    let ids: Vec<String> = ids.iter().filter(|i| i.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_'))).map(|i| format!("'{i}'")).collect();
    if ids.is_empty() {
        return HashMap::new();
    }
    let sql = format!("SELECT id, text FROM memory WHERE id IN ({}) LIMIT 50", ids.join(", "));
    let Ran::Out(out) = trail::run_chloectx(&sup.cfg.chloectx_bin, &place(a), &["sql", "--json", &sql], Duration::from_secs(30)) else { return HashMap::new() };
    let cols: Vec<String> = out["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
    let (Some(i), Some(t)) = (cols.iter().position(|c| c == "id"), cols.iter().position(|c| c == "text")) else { return HashMap::new() };
    out["rows"].as_array().into_iter().flatten().filter_map(Json::as_array).filter_map(|r| Some((r.get(i)?.as_str()?.to_string(), r.get(t)?.as_str()?.to_string()))).collect()
}

/// The last thing the agent said when its turn ended (Claude Code's
/// `last_assistant_message`), if it said anything.
fn last_summary(sup: &Supervisor, id: &str) -> Option<String> {
    let evs = sup.all_events(id).ok()?;
    evs.iter().rev().filter(|e| e.kind == "Stop").find_map(|e| trail::data_of(e).and_then(|d| d["last_assistant_message"].as_str().map(str::trim).filter(|t| t.len() > 20).map(str::to_string)))
}

/// Whether a task is Workbench's own brief to an agent, not words a person
/// would put on a pull request.
fn is_brief(task: &str) -> bool {
    let t = task.trim_start();
    ["You lead the work on a goal", "You are in a git worktree", "(About chapter", "You plan and coordinate"].iter().any(|p| t.starts_with(p))
}

/// The tests the branch's agents ran, from Health's tables: each test's
/// last result, failures fixed later, runs only a summary line said, and
/// runs on a merge of this branch with others (the merged tree). `None`
/// when there is no test_runs table.
fn branch_tests(sup: &Supervisor, dir: &Path, mine: &[Agent], head: Option<&str>) -> Option<(Vec<Json>, Vec<Json>)> {
    let runs = blast::test_runs(sup)?;
    let ids: HashSet<&str> = mine.iter().map(|m| m.id.as_str()).collect();
    let mut by_test: std::collections::BTreeMap<&str, Vec<&blast::Run>> = std::collections::BTreeMap::new();
    for r in runs.iter().filter(|r| ids.contains(r.agent.as_str())) {
        by_test.entry(r.test.as_str()).or_default().push(r);
    }
    let mut tests = Vec::new();
    for (name, rs) in &by_test {
        let last = rs.last().unwrap();
        let mut fixed = false;
        let mut failed_before = false;
        let mut commits: HashMap<&str, (bool, bool)> = HashMap::new();
        for r in rs {
            let e = commits.entry(r.commit.as_str()).or_default();
            if r.passed {
                e.0 = true;
                fixed |= failed_before;
            } else {
                e.1 = true;
                failed_before = true;
                fixed = false;
            }
        }
        let flaky = commits.values().any(|(p, f)| *p && *f);
        let passed = rs.iter().filter(|r| r.passed).count();
        tests.push(json!({
            "kind": "test", "name": name, "ok": last.passed, "runs": rs.len(), "passed": passed, "failed": rs.len() - passed,
            "fixed": fixed && last.passed && !flaky, "flaky": flaky, "ts": iso(last.ts),
            "on_head": head.is_some_and(|h| !last.commit.is_empty() && (h.starts_with(&last.commit) || last.commit.starts_with(h))),
        }));
    }
    // Runs only a summary line said (Health's test_summaries), when no test
    // of the same run is named.
    let p = chloedb::query::Params::new();
    if let Ok(rows) = sup.db().query("SELECT agent, command, n_passed, n_failed, ts FROM test_summaries LIMIT 5000", &p) {
        for r in rows {
            let ag = crate::db::get_str(&r[0]).unwrap_or_default();
            if !ids.contains(ag.as_str()) {
                continue;
            }
            let (np, nf) = (crate::db::get_int(&r[2]).unwrap_or(0), crate::db::get_int(&r[3]).unwrap_or(0));
            tests.push(json!({ "kind": "summary", "name": crate::db::get_str(&r[1]).unwrap_or_default(), "ok": nf == 0 && np > 0, "summary": format!("{np} passed, {nf} failed"), "passed": np, "failed": nf, "ts": crate::db::get_ms(&r[4]).map(iso) }));
        }
    }
    // The merged tree: runs on a merge commit that holds this branch's head.
    let mut merged = Vec::new();
    if let Some(h) = head {
        let mut seen: HashSet<&str> = HashSet::new();
        for r in runs.iter().rev() {
            if r.commit.is_empty() || seen.len() >= 12 || !seen.insert(r.commit.as_str()) || h.starts_with(&r.commit) || r.commit.starts_with(h) {
                continue;
            }
            let parents = git_out(dir, &["rev-list", "--parents", "-n", "1", &r.commit]).unwrap_or_default();
            if parents.split_whitespace().count() < 3 || !crate::git::is_ancestor(dir, h, &r.commit) {
                continue;
            }
            let of: Vec<&blast::Run> = runs.iter().filter(|x| x.commit == r.commit).collect();
            let passed = of.iter().filter(|x| x.passed).count();
            let branches = git_out(dir, &["branch", "--format=%(refname:short)", "--merged", &r.commit]).unwrap_or_default().lines().filter(|b| b.starts_with("wb/")).map(str::to_string).take(8).collect::<Vec<_>>();
            let by = sup.agent_row(&r.agent).map(|a| a.name);
            merged.push(json!({ "commit": r.commit, "tests": of.len(), "passed": passed, "failed": of.len() - passed, "branches": branches, "by": by, "ts": iso(r.ts) }));
        }
    }
    Some((tests, merged))
}

/// One branch's part of a pull request: its title, summary, commits,
/// decisions, tests and blast radius.
pub(crate) fn part(sup: &Supervisor, a: &Agent, missing: &mut Vec<String>) -> Result<Json, ApiError> {
    let b = blast::blast(sup, &a.id)?;
    for m in b["missing"].as_array().into_iter().flatten().filter_map(Json::as_str) {
        if !missing.iter().any(|x| x == m) {
            missing.push(m.to_string());
        }
    }
    let dir = place(a);
    let base = b["base"].as_str().unwrap_or("").to_string();
    let head = b["head"].as_str().map(str::to_string);
    let commits: Vec<Json> = git_out(&dir, &["log", "--no-merges", "--format=%h%x09%s", &format!("{base}..HEAD")])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(sha, s)| json!({ "sha": sha, "subject": s }))
        .collect();
    let mine = branch_agents(sup, a, head.as_deref());

    // Title: the outcome, the goal it leads, its one commit, what it said
    // when it finished, the first commit; the task only when nothing else.
    let q1 = |sql: &str, id: &str| -> Option<String> {
        let rows = sup.db().query(sql, &chloedb::query::Params::new().set("id", chloedb::Value::Str(id.to_string()))).ok()?;
        rows.first().and_then(|r| crate::db::get_str(&r[0])).filter(|t| !t.trim().is_empty())
    };
    let outcome = a.outcome.as_deref().and_then(|o| q1("SELECT title FROM outcomes WHERE id = :id", o));
    let goal = a.goal.as_deref().filter(|_| outcome.is_none()).and_then(|g| q1("SELECT text FROM goals WHERE id = :id", g));
    let summary = mine.iter().find_map(|m| last_summary(sup, &m.id));
    let task = a.task.as_deref().filter(|t| !t.trim().is_empty() && !is_brief(t));
    let (title, title_from) = if let Some(t) = outcome {
        (t, "outcome")
    } else if let Some(g) = goal {
        (first_sentence(&g, 72), "goal")
    } else if commits.len() == 1 {
        (commits[0]["subject"].as_str().unwrap_or("").to_string(), "commit")
    } else if let Some(s) = summary.as_deref().map(|s| first_sentence(s.trim_start_matches(['#', ' ', '*']), 72)).filter(|s| s.len() > 8) {
        (s, "summary")
    } else if let Some(c) = commits.last() {
        (c["subject"].as_str().unwrap_or("").to_string(), "commit")
    } else if let Some(t) = task {
        (first_sentence(t, 72), "task")
    } else {
        (format!("Changes from {}", a.name), "name")
    };

    // Decisions: the branch's agents' own, then those of the blast radius
    // made by the branch's agents (never another branch's).
    let mut decisions = session_decisions(sup, a, &mine, missing);
    let mut seen: BTreeSet<String> = decisions.iter().filter_map(|d| d["id"].as_str().map(str::to_string)).collect();
    let ids: HashSet<&str> = mine.iter().map(|m| m.id.as_str()).collect();
    for d in b["decisions"].as_array().into_iter().flatten() {
        let did = d["id"].as_str().unwrap_or("").to_string();
        let ours = d["agent"]["id"].as_str().is_some_and(|x| ids.contains(x));
        if ours && seen.insert(did.clone()) {
            decisions.push(json!({ "id": did, "kind": d["kind"], "text": d["text"], "ts": d["ts"], "superseded": d["superseded"], "superseded_by": d["superseded_by"], "via": d["via"] }));
        }
    }
    let others = b["decisions"].as_array().map_or(0, |v| v.iter().filter(|d| !seen.contains(d["id"].as_str().unwrap_or(""))).count());
    // Successors of superseded ones, by text.
    let wanted: Vec<String> = decisions.iter().filter_map(|d| d["superseded_by"].as_str().map(str::to_string)).collect();
    let texts = record_texts(sup, a, &wanted);
    for d in decisions.iter_mut() {
        if let Some(by) = d["superseded_by"].as_str().map(str::to_string) {
            d["successor"] = texts.get(&by).map_or(Json::Null, |t| json!(t));
        }
    }
    let tests = branch_tests(sup, &dir, &mine, head.as_deref());
    let (tests, merged, from_table) = match tests {
        Some((t, m)) if !t.is_empty() || !m.is_empty() => (t, m, true),
        _ => (mine.iter().flat_map(|m| tests_run(sup, &m.id)).collect(), Vec::new(), false),
    };
    Ok(json!({
        "agent": trail::agent_json(sup, a), "title": title, "title_from": title_from, "summary": summary, "task": task.map(|t| first_sentence(t, 200)),
        "commits": commits, "decisions": decisions, "other_decisions": others, "tests": tests, "tests_from": if from_table { "test_runs" } else { "events" }, "merged": merged,
        "agents": mine.iter().map(|m| m.name.clone()).collect::<Vec<_>>(), "blast": b,
    }))
}

fn decision_lines(decisions: &[Json]) -> String {
    let mut body = String::new();
    for d in decisions {
        let did = d["id"].as_str().unwrap_or("");
        let kind = d["kind"].as_str().unwrap_or("decision");
        let label = match (d["superseded_by"].as_str(), d["successor"].as_str()) {
            (Some(by), Some(t)) => format!("(superseded by `{by}`: {})", crate::supervisor::one_line(t, 160)),
            (Some(by), None) => format!("(superseded by `{by}`)"),
            (None, _) if d["superseded"] == true => "(marked superseded; ChloeCtx names no successor)".into(),
            (None, _) => format!("{kind}:"),
        };
        body += &format!("- ◆ `{did}` {label} {}\n", d["text"].as_str().unwrap_or("").trim());
    }
    body
}

fn test_lines(p: &Json) -> String {
    let mut body = String::new();
    let tests = p["tests"].as_array().cloned().unwrap_or_default();
    if tests.is_empty() && p["merged"].as_array().is_none_or(|m| m.is_empty()) {
        body += "Workbench saw no test run by this branch's agents.\n";
    }
    let named: Vec<&Json> = tests.iter().filter(|t| t["kind"] == "test").collect();
    if !named.is_empty() {
        let pass = named.iter().filter(|t| t["ok"] == true).count();
        body += &format!("{} of {} pass on their last run.\n\n", pass, plural(named.len(), "test"));
    }
    for t in &tests {
        let name = t["name"].as_str().unwrap_or("");
        match t["kind"].as_str() {
            Some("test") => {
                let how = if t["ok"] == true { "passed" } else { "failed" };
                let mut extra = Vec::new();
                if t["fixed"] == true {
                    extra.push("failed first, fixed in this work".to_string());
                }
                if t["flaky"] == true {
                    extra.push("flaky: passed and failed on one commit".to_string());
                }
                if t["runs"].as_u64().unwrap_or(0) > 1 {
                    extra.push(format!("{} runs", t["runs"]));
                }
                body += &format!("- `{name}`: {how}{}\n", if extra.is_empty() { String::new() } else { format!(" ({})", extra.join("; ")) });
            }
            Some("summary") => body += &format!("- `{name}`: {} (only the summary line was read)\n", t["summary"].as_str().unwrap_or("")),
            _ => {
                let how = match t["ok"].as_bool() {
                    Some(true) => "passed",
                    Some(false) => "failed",
                    None => "ran, result unknown",
                };
                body += &format!("- command `{name}`: {how}{}\n", t["summary"].as_str().map(|s| format!(" ({s})")).unwrap_or_default());
            }
        }
    }
    for m in p["merged"].as_array().into_iter().flatten() {
        let br: Vec<&str> = m["branches"].as_array().into_iter().flatten().filter_map(Json::as_str).collect();
        body += &format!(
            "- On a merge of {} (`{}`){}: {} of {} passed\n",
            if br.is_empty() { "this branch with others".to_string() } else { br.join(", ") },
            &m["commit"].as_str().unwrap_or("")[..m["commit"].as_str().unwrap_or("").len().min(7)],
            m["by"].as_str().map(|b| format!(", run by {b}")).unwrap_or_default(),
            m["passed"],
            plural(m["tests"].as_u64().unwrap_or(0) as usize, "test")
        );
    }
    body
}

fn summary_lines(p: &Json) -> String {
    let mut body = String::new();
    if let Some(s) = p["summary"].as_str() {
        let s: String = s.chars().take(1600).collect();
        body += &format!("{}\n\n", s.trim());
    }
    // Never the task's prompt (SPEC.md, "Sixth build"): what was done is
    // the commits, or the files when nothing is committed yet.
    let commits = p["commits"].as_array().cloned().unwrap_or_default();
    for c in commits.iter().take(12) {
        body += &format!("- {} (`{}`)\n", c["subject"].as_str().unwrap_or(""), c["sha"].as_str().unwrap_or(""));
    }
    if commits.is_empty() {
        for f in p["blast"]["files"].as_array().into_iter().flatten().take(12) {
            body += &format!("- `{}` +{} −{}\n", f["path"].as_str().unwrap_or(""), f["added"], f["removed"]);
        }
    }
    body
}

fn blast_lines(b: &Json) -> String {
    let mut body = format!("{}\n\n", b["risk"]["line"].as_str().unwrap_or(""));
    if let Some(l) = b["landing"]["line"].as_str() {
        body += &format!("{l}\n\n");
    }
    let fns: Vec<String> = b["functions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| {
            let name = f["name"].as_str().unwrap_or("");
            match f["status"].as_str() {
                Some("added") => format!("`{name}` (new)"),
                Some("deleted") => format!("`{name}` (removed)"),
                _ => format!("`{name}`"),
            }
        })
        .collect();
    if !fns.is_empty() {
        body += &format!("- Functions: {}\n", fns.join(", "));
    }
    let ts: Vec<String> = b["tests"]
        .as_array()
        .into_iter()
        .flatten()
        .take(8)
        .map(|t| {
            let r = &t["runs"];
            let runs = if r.is_null() {
                "not run".to_string()
            } else if r["last_passed"] == true {
                format!("passing{}", if r["flaky"] == true { ", flaky" } else { "" })
            } else {
                format!("failing: {}", r["failing"].as_array().into_iter().flatten().filter_map(Json::as_str).take(3).collect::<Vec<_>>().join(", "))
            };
            format!("`{}` ({runs})", t["path"].as_str().unwrap_or(""))
        })
        .collect();
    if !ts.is_empty() {
        body += &format!("- Tests that cover them: {}\n", ts.join(", "));
    }
    let ags: Vec<String> = b["agents"].as_array().into_iter().flatten().map(|g| g["detail"].as_str().unwrap_or("").to_string()).collect();
    if !ags.is_empty() {
        body += &format!("- Other agents: {}\n", ags.join("; "));
    }
    body
}

/// `GET /api/agents/:id/pr`: the pull request's draft.
pub fn pr(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let a = agent_of(sup, id)?;
    let mut missing: Vec<String> = Vec::new();
    let p = part(sup, &a, &mut missing)?;
    let b = &p["blast"];
    let dir = place(&a);
    let title = p["title"].as_str().unwrap_or("").to_string();
    let main = crate::git::checkout(&dir).map(|c| c.main).unwrap_or_else(|_| PathBuf::from(&a.repo));
    let branch = a.branch.clone().or_else(|| crate::git::current_branch(&dir)).unwrap_or_else(|| "HEAD".into());
    // Stacked on another agent's branch: the pull request goes onto it.
    let stacked: Vec<&Json> = b["landing"]["under"].as_array().into_iter().flatten().filter(|u| u["merged"] != true).collect();
    let onto = b["landing"]["onto"].as_str().map(str::to_string).or_else(|| crate::git::current_branch(&main).filter(|x| Some(x) != a.branch.as_ref())).unwrap_or_else(|| "main".into());
    let base_branch = match stacked.as_slice() {
        [u] => u["branch"].as_str().unwrap_or(&onto).to_string(),
        _ => onto.clone(),
    };
    let base = b["base"].as_str().unwrap_or("").to_string();
    let t = &b["totals"];
    let n = |k: &str| t[k].as_u64().unwrap_or(0) as usize;
    let capsule = stored(sup, id);

    let mut body = String::new();
    body += "## Summary\n\n";
    body += &summary_lines(&p);
    body += &format!("\n{}, +{} −{}, on `{branch}` from `{}`.\n\n", plural(n("files"), "file"), n("added"), n("removed"), &base[..base.len().min(7)]);
    if let Some(l) = b["landing"]["line"].as_str() {
        body += &format!("{l} {}\n\n", if base_branch != onto { format!("This pull request goes onto `{base_branch}`; merge that one first.") } else { "Merge those branches first, or ship them together.".to_string() });
    }
    body += "## Decisions behind it\n\n";
    let decisions = p["decisions"].as_array().cloned().unwrap_or_default();
    if decisions.is_empty() {
        body += "ChloeCtx holds no decision from this branch's work.\n\n";
    } else {
        body += &decision_lines(&decisions);
        body += "\n";
    }
    body += "## Blast radius\n\n";
    body += &blast_lines(b);
    body += "\n## Tests run\n\n";
    body += &test_lines(&p);
    body += "\n## Ask why\n\n";
    match &capsule {
        Some(c) => {
            body += &format!(
                "This pull request carries a capsule of the work behind it: {} packed by ChloeCtx from {}. Ask it instead of guessing:\n\n```\n{}\n```\n\nthen, in Claude Code, `capsule_ask` with the question (\"why was it done this way?\"). It answers from the records, with no model call.\n",
                plural(c["records"].as_u64().unwrap_or(0) as usize, "record"),
                c["scope_short"].as_str().map(str::to_string).unwrap_or_else(|| format!("{}'s session", a.name)),
                c["claude"].as_str().unwrap_or("")
            );
        }
        None => {
            body += "No capsule was packed for it yet (Workbench's Ship screen packs one with `chloectx pack`).\n";
            missing.push("No capsule yet: pack one to give the pull request its Ask why section.".into());
        }
    }
    let body_file = sup.session_dir(id).join("pr.md");
    if let Err(e) = std::fs::create_dir_all(sup.session_dir(id)).and_then(|_| std::fs::write(&body_file, &body)) {
        missing.push(format!("The body could not be saved to {}: {e}", body_file.display()));
    }
    let gh = format!("gh pr create --base {base_branch} --head {branch} --title {} --body-file {}", shell_q(&title), body_file.display());
    let stack = stack_of(sup, &a, b);
    Ok(json!({
        "agent": trail::agent_json(sup, &a), "title": title, "title_from": p["title_from"], "branch": branch, "base_branch": base_branch, "onto": onto, "base": base,
        "body": body, "body_file": body_file.to_string_lossy(), "gh": gh, "push": format!("git push -u origin {branch}"),
        "sections": {
            "summary": p["commits"].as_array().into_iter().flatten().filter_map(|c| c["subject"].as_str().map(str::to_string)).collect::<Vec<_>>(),
            "said": p["summary"],
            "commits": p["commits"], "decisions": decisions, "other_decisions": p["other_decisions"],
            "blast": { "level": b["risk"]["level"], "line": b["risk"]["line"], "functions": n("functions"), "tests": n("tests"), "agents": b["agents"].as_array().into_iter().flatten().filter_map(|g| g["agent"]["name"].as_str()).collect::<Vec<_>>() },
            "tests": p["tests"], "tests_from": p["tests_from"], "merged": p["merged"],
        },
        "landing": b["landing"], "stack": stack,
        "capsule": capsule, "missing": missing,
    }))
}

fn shell_q(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$").replace('`', "\\`"))
}

/// The stack an agent's branch is part of, bottom first: the agents'
/// branches it is on top of, itself, and those on top of it (each with
/// nothing else between). Empty when it stands alone.
fn stack_of(sup: &Supervisor, a: &Agent, b: &Json) -> Vec<Json> {
    let mut chain: Vec<Json> = b["landing"]["under"].as_array().cloned().unwrap_or_default().into_iter().filter(|u| u["merged"] != true).map(|u| u["agent"].clone()).collect();
    // Bottom first: by how many of the others each one holds.
    let dir = place(a);
    let tip = |x: &Json| -> Option<String> { x["branch"].as_str().and_then(|br| crate::git::rev_parse(&dir, &format!("refs/heads/{br}"))) };
    chain.sort_by_key(|x| {
        let t = tip(x);
        chain_rank(&dir, t.as_deref(), b)
    });
    if chain.is_empty() && b["landing"]["above"].as_array().is_none_or(|v| v.is_empty()) {
        return Vec::new();
    }
    chain.push(trail::agent_json(sup, a));
    let mut above: Vec<Json> = b["landing"]["above"].as_array().cloned().unwrap_or_default().into_iter().map(|u| u["agent"].clone()).collect();
    above.sort_by_key(|x| {
        let t = tip(x);
        chain_rank(&dir, t.as_deref(), b)
    });
    chain.extend(above);
    chain
}

fn chain_rank(dir: &Path, tip: Option<&str>, b: &Json) -> usize {
    let Some(t) = tip else { return 0 };
    let fork = b["landing"]["fork"].as_str().unwrap_or("");
    if fork.is_empty() {
        return 0;
    }
    git_out(dir, &["rev-list", "--count", &format!("{fork}..{t}")]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// `GET /api/agents/:id/pr/stack`: one draft for a stack of branches, in
/// merge order, each branch's part under its own heading; the pull request
/// is the top branch onto the landing branch.
pub fn pr_stack(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let a = agent_of(sup, id)?;
    let b = blast::blast(sup, id)?;
    let chain = stack_of(sup, &a, &b);
    if chain.len() < 2 {
        return Err(ApiError::conflict(format!("{}'s branch is not stacked with another agent's", a.name)));
    }
    let mut missing = Vec::new();
    let mut parts = Vec::new();
    for g in &chain {
        let Some(ag) = g["id"].as_str().and_then(|i| sup.agent_row(i)) else { continue };
        if !Path::new(&ag.cwd).is_dir() {
            missing.push(format!("{}'s worktree is gone, so its part is left out.", ag.name));
            continue;
        }
        parts.push((ag.clone(), part(sup, &ag, &mut missing)?));
    }
    let (top, _) = parts.last().cloned().ok_or_else(|| ApiError::conflict("no branch of the stack can be read"))?;
    let onto = b["landing"]["onto"].as_str().unwrap_or("main").to_string();
    let titles: Vec<String> = parts.iter().map(|(_, p)| p["title"].as_str().unwrap_or("").to_string()).collect();
    let title = if titles.len() == 2 { format!("{}; {}", titles[0], titles[1]) } else { format!("{} and {} more", titles[0], titles.len() - 1) };
    let order: Vec<String> = parts.iter().map(|(g, _)| format!("`{}`", g.branch.clone().unwrap_or_else(|| g.name.clone()))).collect();
    let mut body = format!("## Summary\n\nA stack of {} branches, merged in this order: {}. Each part below is that branch's own change (counted from the branch under it).\n\n", parts.len(), order.join(" → "));
    for (i, (g, p)) in parts.iter().enumerate() {
        body += &format!("## {}. {} (`{}`, {})\n\n", i + 1, p["title"].as_str().unwrap_or(""), g.branch.as_deref().unwrap_or(""), g.name);
        body += &summary_lines(p);
        body += "\n### Decisions behind it\n\n";
        let ds = p["decisions"].as_array().cloned().unwrap_or_default();
        body += &if ds.is_empty() { "ChloeCtx holds no decision from this branch's work.\n".to_string() } else { decision_lines(&ds) };
        body += "\n### Blast radius\n\n";
        body += &blast_lines(&p["blast"]);
        body += "\n### Tests run\n\n";
        body += &test_lines(p);
        body += "\n";
    }
    let body_file = sup.session_dir(id).join("pr-stack.md");
    if let Err(e) = std::fs::create_dir_all(sup.session_dir(id)).and_then(|_| std::fs::write(&body_file, &body)) {
        missing.push(format!("The body could not be saved to {}: {e}", body_file.display()));
    }
    let head = top.branch.clone().unwrap_or_else(|| "HEAD".into());
    Ok(json!({
        "agent": trail::agent_json(sup, &a), "title": title, "branch": head, "base_branch": onto, "body": body, "body_file": body_file.to_string_lossy(),
        "gh": format!("gh pr create --base {onto} --head {head} --title {} --body-file {}", shell_q(&title), body_file.display()),
        "push": format!("git push -u origin {}", parts.iter().filter_map(|(g, _)| g.branch.clone()).collect::<Vec<_>>().join(" ")),
        "parts": parts.iter().map(|(g, p)| json!({ "agent": trail::agent_json(sup, g), "branch": g.branch, "title": p["title"], "risk": p["blast"]["risk"], "commits": p["commits"], "tests": p["tests"] })).collect::<Vec<_>>(),
        "missing": missing,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_record_ids_from_an_answer() {
        let t = "Rules:\n- [e3b1c2d4:212] decision: per line.\n\n(capsule \"checkout\" sha256:9b1e0c47d2a3, records: e3b1c2d4:212, e3b1c2d4:198)";
        assert_eq!(ids_in(t), ["e3b1c2d4:212", "e3b1c2d4:198"]);
        assert_eq!(first_sentence("Charge tax by address. Then more", 72), "Charge tax by address");
    }
}
