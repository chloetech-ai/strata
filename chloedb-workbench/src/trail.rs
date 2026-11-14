//! The decision trail: from a line an agent wrote to who decided it (the
//! agent, its lineage, the prompt and who gave it), when, where, what it
//! knew and why, and the moment of its terminal to rewind to. It asks
//! ChloeCtx (`chloectx blame --json`, `sql --json`, `ask --json`), and when
//! ChloeCtx is not installed or knows nothing it answers from git and
//! Workbench's own rows, and says what it could not find.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value as Json};

use crate::db::{iso, Agent, Event};
use crate::git;
use crate::supervisor::{ApiError, Supervisor};

/// The most bytes of a file `why` reads for its line.
const MAX_FILE: u64 = 8 << 20;
/// The tools whose calls change a file.
const EDIT_TOOLS: [&str; 4] = ["Edit", "MultiEdit", "Write", "NotebookEdit"];
/// Rewind's window around the edit's tool call.
const REWIND_BEFORE_MS: i64 = 20_000;
const REWIND_AFTER_MS: i64 = 3_000;

/// What running `chloectx` gave.
pub enum Ran {
    /// There is no such program.
    Missing,
    /// It ran and failed, or printed no JSON: why, in a sentence.
    Failed(String),
    Out(Json),
}

/// Runs `<bin> <args>` in `cwd` and reads its JSON, waiting at most `timeout`.
pub fn run_chloectx(bin: &str, cwd: &Path, args: &[&str], timeout: Duration) -> Ran {
    run_chloectx_env(bin, cwd, args, &[], timeout)
}

/// `run_chloectx` with these environment variables set.
pub fn run_chloectx_env(bin: &str, cwd: &Path, args: &[&str], env: &[(&str, String)], timeout: Duration) -> Ran {
    let what = format!("chloectx {}", args.first().copied().unwrap_or(""));
    match run_raw_env(bin, cwd, args, env, timeout) {
        Ran::Out(Json::String(out)) => match serde_json::from_str::<Json>(&out) {
            Ok(v) if v.get("error").and_then(Json::as_str).is_some() => Ran::Failed(format!("{what} failed: {}", v["error"].as_str().unwrap_or(""))),
            Ok(v) => Ran::Out(v),
            Err(_) => Ran::Failed(format!("{what} printed no JSON (is this chloectx too old for --json?)")),
        },
        other => other,
    }
}

/// Runs `<bin> <args>` in `cwd` and answers what it printed, as
/// `Ran::Out(Json::String(text))`, waiting at most `timeout`.
pub fn run_raw(bin: &str, cwd: &Path, args: &[&str], timeout: Duration) -> Ran {
    run_raw_env(bin, cwd, args, &[], timeout)
}

fn run_raw_env(bin: &str, cwd: &Path, args: &[&str], env: &[(&str, String)], timeout: Duration) -> Ran {
    let mut cmd = Command::new(bin);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = match cmd.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ran::Missing,
        Err(e) => return Ran::Failed(format!("cannot run {bin}: {e}")),
    };
    let reader = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(r) = r {
                let _ = r.take(64 << 20).read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = reader(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let err = reader(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let end = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = out.join().unwrap_or_default();
    let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).trim().to_string();
    let what = format!("chloectx {}", args.first().copied().unwrap_or(""));
    let Some(status) = status else { return Ran::Failed(format!("{what} did not answer within {} s", timeout.as_secs())) };
    if !status.success() {
        let msg = err.lines().last().unwrap_or("").trim_start_matches("chloectx: ").to_string();
        return Ran::Failed(if msg.is_empty() { format!("{what} failed ({status})") } else { format!("{what} failed: {msg}") });
    }
    Ran::Out(Json::String(String::from_utf8_lossy(&out).into_owned()))
}

pub(crate) fn not_installed(bin: &str) -> String {
    format!("ChloeCtx is not installed (no program {bin}), so there are no memory records, notes or receipts to show")
}

/// `ts` of ChloeCtx (seconds since the epoch; milliseconds when it is that
/// large; text as it is) as ISO 8601 text.
pub(crate) fn ctx_time(v: &Json) -> Json {
    match v {
        Json::Number(n) => {
            let t = n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0);
            json!(iso(if t.abs() < 100_000_000_000 { t * 1000 } else { t }))
        }
        Json::String(s) => json!(s),
        _ => Json::Null,
    }
}

pub fn ctx_ms(v: &Json) -> Option<i64> {
    let t = v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))?;
    Some(if t.abs() < 100_000_000_000 { t * 1000 } else { t })
}

/// The agents and the Claude Code sessions each ran: its `session_id` and
/// every one its hooks reported (a `/clear` starts a new one).
pub struct Sessions {
    agents: Vec<Agent>,
    by_session: HashMap<String, usize>,
}

impl Sessions {
    pub fn load(sup: &Supervisor) -> Sessions {
        let agents = sup.agent_rows();
        let mut by_session = HashMap::new();
        for (i, a) in agents.iter().enumerate() {
            for ev in sup.all_events(&a.id).unwrap_or_default() {
                if let Some(sid) = ev.data.as_deref().and_then(|d| serde_json::from_str::<Json>(d).ok()).and_then(|d| d.get("session_id").and_then(Json::as_str).map(str::to_string)) {
                    by_session.entry(sid).or_insert(i);
                }
            }
            if let Some(sid) = &a.session_id {
                by_session.insert(sid.clone(), i);
            }
        }
        Sessions { agents, by_session }
    }

    pub fn agent(&self, session: &str) -> Option<&Agent> {
        self.by_session.get(session).map(|&i| &self.agents[i])
    }

    /// Every Claude Code session an agent ran.
    pub fn sessions_of(&self, id: &str) -> Vec<String> {
        let Some(i) = self.agents.iter().position(|a| a.id == id) else { return Vec::new() };
        let mut v: Vec<String> = self.by_session.iter().filter(|(_, &j)| j == i).map(|(s, _)| s.clone()).collect();
        v.sort();
        v
    }

    /// The agent of a ChloeCtx record id, `<the session's first 8 letters
    /// and digits>:<seq>`.
    pub fn of_record(&self, id: &str) -> Option<&Agent> {
        let prefix = id.rsplit_once(':')?.0;
        self.by_session.iter().find(|(s, _)| s.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect::<String>() == prefix).map(|(_, &i)| &self.agents[i])
    }

    fn by_id(&self, id: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }
}

pub(crate) fn agent_json(sup: &Supervisor, a: &Agent) -> Json {
    sup.agent_json_of(&a.id).unwrap_or(Json::Null)
}

// Workbench's events of an edit.

pub(crate) fn data_of(ev: &Event) -> Option<Json> {
    ev.data.as_deref().and_then(|d| serde_json::from_str(d).ok())
}

/// The file an edit event's tool call changed.
fn edit_file(ev: &Event) -> Option<String> {
    let tool = ev.tool.as_deref()?;
    if let Some(d) = data_of(ev) {
        for k in ["file_path", "notebook_path", "path"] {
            if let Some(p) = d["tool_input"].get(k).and_then(Json::as_str) {
                return Some(p.to_string());
            }
        }
    }
    // The data was trimmed: the summary names the file.
    let s = ev.summary.as_deref()?;
    s.strip_prefix(&format!("{tool}: ")).or_else(|| s.strip_prefix(&format!("{tool} done: "))).map(str::to_string)
}

/// Whether an edit event's new text holds `line` as a whole line.
fn writes_line(ev: &Event, line: &str) -> bool {
    let line = line.trim();
    if line.chars().count() < 2 {
        return false;
    }
    let Some(d) = data_of(ev) else { return ev.data.as_deref().is_some_and(|d| d.contains(line)) };
    let ti = &d["tool_input"];
    let mut texts: Vec<&str> = ["new_string", "content", "new_source"].iter().filter_map(|k| ti.get(*k).and_then(Json::as_str)).collect();
    if let Some(edits) = ti.get("edits").and_then(Json::as_array) {
        texts.extend(edits.iter().filter_map(|e| e.get("new_string").and_then(Json::as_str)));
    }
    texts.iter().any(|t| t.lines().any(|l| l.trim() == line))
}

fn same_file(fp: &str, cwd: &Path, rel: &str) -> bool {
    let p = Path::new(fp);
    if p.is_absolute() {
        p == cwd.join(rel) || fp.ends_with(&format!("/{rel}"))
    } else {
        fp.trim_start_matches("./") == rel
    }
}

/// The tool call (its `PreToolUse`, else its `PostToolUse`) among `evs`
/// that most likely wrote `line` of `rel`: one whose new text holds the
/// line, nearest `near_ms` when known, else the newest.
fn edit_event<'a>(evs: &'a [Event], cwd: &Path, rel: &str, line: &str, near_ms: Option<i64>) -> Option<&'a Event> {
    let is_edit = |e: &Event| matches!(e.kind.as_str(), "PreToolUse" | "PostToolUse") && e.tool.as_deref().is_some_and(|t| EDIT_TOOLS.contains(&t)) && edit_file(e).is_some_and(|f| same_file(&f, cwd, rel));
    let cands: Vec<&Event> = evs.iter().filter(|e| is_edit(e)).collect();
    let best = cands.iter().copied().max_by_key(|e| (writes_line(e, line), near_ms.map_or(e.seq, |t| -(e.ts - t).abs()), e.seq))?;
    if best.kind == "PreToolUse" {
        return Some(best);
    }
    // The call's start: the PreToolUse of the same tool and file before it.
    let file = edit_file(best);
    let pre = evs.iter().rev().filter(|e| e.seq < best.seq).take_while(|e| e.kind != "UserPromptSubmit").find(|e| e.kind == "PreToolUse" && e.tool == best.tool && edit_file(e) == file);
    Some(pre.unwrap_or(best))
}

/// The prompt event before event number `seq` (or before `ms`), and who
/// gave it: `you`, or the name of the agent that typed it or spawned this
/// one with it as its task.
fn prompt_of(evs: &[Event], agent: &Agent, s: &Sessions, seq: Option<i64>, ms: Option<i64>) -> Option<(Json, String)> {
    let i = evs.iter().rposition(|e| e.kind == "UserPromptSubmit" && seq.map_or(true, |q| e.seq < q) && ms.map_or(true, |t| e.ts <= t))?;
    let ev = &evs[i];
    let prev = evs[..i].iter().rposition(|e| e.kind == "UserPromptSubmit");
    let from = prev.map_or(0, |p| p + 1);
    let name_of = |id: &str| s.by_id(id).map(|a| a.name.clone()).unwrap_or_else(|| id.to_string());
    let by = match evs[from..i].iter().rev().find(|e| e.kind == "input") {
        Some(input) => match data_of(input).and_then(|d| d.get("by").and_then(Json::as_str).map(str::to_string)) {
            Some(b) => name_of(&b),
            None => "you".into(),
        },
        // The first prompt with no input before it: the task it was spawned with.
        None if prev.is_none() && agent.task.is_some() => agent.parent.as_deref().map_or("you".into(), name_of),
        None => "you".into(),
    };
    let text = data_of(ev).and_then(|d| d.get("prompt").and_then(Json::as_str).map(str::to_string)).or_else(|| ev.summary.as_deref().map(|s| s.trim_start_matches("prompt: ").to_string())).unwrap_or_default();
    Some((json!({ "text": text, "ts": iso(ev.ts), "ts_ms": ev.ts, "by": by }), by))
}

/// What became of a record ChloeCtx had put in context, from blame's flags.
fn now_of(flags: &[String]) -> (String, bool) {
    let mut now = "current".to_string();
    let mut untrusted = false;
    for f in flags {
        if let Some(rest) = f.strip_prefix("superseded since") {
            now = match rest.strip_prefix(", by ") {
                Some(by) => format!("superseded by {by}"),
                None => "superseded".into(),
            };
        } else if f.starts_with("deleted") {
            now = "deleted".into();
        } else if f.starts_with("untrusted") || f.starts_with("from a tool") || f.starts_with("from another tool") {
            untrusted = true;
        }
    }
    (now, untrusted)
}

fn knew_of(blame: &Json) -> Vec<Json> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for rc in blame["receipts"].as_array().into_iter().flatten() {
        for r in rc["rows"].as_array().into_iter().flatten() {
            let row = &r["row"];
            let id = row["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            let flags: Vec<String> = r["now"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(str::to_string)).collect();
            let (now, untrusted) = now_of(&flags);
            out.push(json!({
                "kind": row["kind"], "id": id, "text": r["text"], "score": row["score"], "decision": row["decision"],
                "now": now, "untrusted": untrusted, "receipt": rc["receipt"], "turn": rc["turn"],
            }));
        }
    }
    out
}

fn why_of(blame: &Json) -> Vec<Json> {
    blame["notes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| matches!(n["kind"].as_str(), Some("decision" | "preference" | "fact")))
        .map(|n| json!({ "kind": n["kind"], "text": n["text"], "ts": ctx_time(&n["ts"]), "id": n["id"] }))
        .collect()
}

/// What `why` takes.
#[derive(Debug, Default, serde::Deserialize)]
pub struct WhyQ {
    pub agent: Option<String>,
    pub repo: Option<String>,
    pub path: Option<String>,
    pub line: Option<usize>,
    /// The commit the line was clicked from: the line is read and blamed
    /// as that commit has it.
    pub commit: Option<String>,
}

/// The folder a request names: an agent's worktree, or a repository's
/// main checkout.
fn place(sup: &Supervisor, agent: Option<&str>, repo: Option<&str>) -> Result<(Option<Agent>, PathBuf, String), ApiError> {
    match (agent.filter(|a| !a.is_empty()), repo.filter(|r| !r.is_empty())) {
        (Some(id), _) => {
            let a = sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
            let dir = PathBuf::from(&a.cwd);
            if !dir.is_dir() {
                return Err(ApiError::conflict(format!("agent {}'s worktree {} is gone", a.name, a.cwd)));
            }
            let repo = a.repo.clone();
            Ok((Some(a), dir, repo))
        }
        (None, Some(r)) => {
            let p = PathBuf::from(r);
            if !p.is_absolute() {
                return Err(ApiError::bad(format!("repo must be an absolute path, not {r}")));
            }
            let co = git::checkout(&p).map_err(ApiError::bad)?;
            let main = co.main.to_string_lossy().to_string();
            Ok((None, co.main, main))
        }
        (None, None) => Err(ApiError::bad("agent or repo is required")),
    }
}

/// The text of line `n` of `rel` in `dir`: a regular file inside it once
/// links and `..` are resolved, of at most 8 MiB. Answers the path
/// relative to `dir`.
fn read_line(dir: &Path, rel: &str, n: usize) -> Result<(String, String), ApiError> {
    let root = dir.canonicalize().map_err(|e| ApiError::internal(format!("{}: {e}", dir.display())))?;
    let full = root.join(rel).canonicalize().map_err(|e| ApiError::not_found(format!("{rel}: {e}")))?;
    let rel = full.strip_prefix(&root).map_err(|_| ApiError::bad(format!("{rel} is not a file of {}", root.display())))?.to_string_lossy().to_string();
    let meta = std::fs::metadata(&full).map_err(|e| ApiError::not_found(format!("{rel}: {e}")))?;
    if !meta.is_file() {
        return Err(ApiError::bad(format!("{rel} is not a regular file")));
    }
    if meta.len() > MAX_FILE {
        return Err(ApiError::bad(format!("{rel} is {} bytes; why reads files of at most {MAX_FILE}", meta.len())));
    }
    let body = std::fs::read(&full).map_err(|e| ApiError::internal(format!("{rel}: {e}")))?;
    let body = String::from_utf8_lossy(&body);
    let text = body.lines().nth(n.saturating_sub(1)).filter(|_| n > 0).ok_or_else(|| ApiError::bad(format!("{rel} has {} lines, not a line {n}", body.lines().count())))?;
    Ok((rel, text.to_string()))
}

/// `GET /api/why`: the decision card of a line.
pub fn why(sup: &Supervisor, q: WhyQ) -> Result<Json, ApiError> {
    let path = q.path.as_deref().filter(|p| !p.is_empty()).ok_or_else(|| ApiError::bad("path is required"))?;
    let n = q.line.ok_or_else(|| ApiError::bad("line is required"))?;
    let (asked, dir, repo) = place(sup, q.agent.as_deref(), q.repo.as_deref())?;
    let at_commit = q.commit.as_deref().map(str::trim).filter(|c| !c.is_empty());
    if let Some(c) = at_commit {
        if c.len() < 4 || c.len() > 64 || !c.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(ApiError::bad("commit must be a hex sha"));
        }
    }
    let (rel, text) = match at_commit {
        Some(c) => read_line_at(&dir, c, path, n)?,
        None => read_line(&dir, path, n)?,
    };
    let mut missing: Vec<String> = Vec::new();
    let s = Sessions::load(sup);

    // Where: the commit that brought the line (as of the commit clicked
    // from, when there is one).
    let blamed = match at_commit {
        Some(c) => blame_at(&dir, c, &rel, n),
        None => git::blame_line(&dir, &rel, n).map(|(sha, t, subj)| {
            let ct = committer_time(&dir, &sha).unwrap_or(t);
            (sha, t, subj, ct)
        }),
    };
    let commit = blamed.as_ref().map(|(sha, t, subj, _)| (sha.clone(), *t, subj.clone()));
    // Nothing done after the line's commit wrote it: no later edit, no later prompt.
    let until_ms = blamed.as_ref().map(|b| b.3 * 1000 + 999);
    let branch = asked.as_ref().and_then(|a| a.branch.clone()).or_else(|| git::current_branch(&dir));
    let where_ = json!({
        "repo": repo,
        "worktree": dir.to_string_lossy(),
        "branch": branch,
        "commit": commit.as_ref().map(|(sha, t, subject)| json!({ "sha": sha, "date": iso(t * 1000), "subject": subject })),
    });
    if commit.is_none() {
        missing.push("The line is not committed yet, so it has no commit.".into());
    }

    // ChloeCtx's blame, of the line as the working file has it now (when
    // asked about a commit: only where the line still is, by its text).
    let now_n = match at_commit {
        Some(_) => locate_line(&dir, &rel, &text),
        None => Some(n),
    };
    if at_commit.is_some() && now_n.is_none() {
        missing.push(format!("The line is no longer in {rel} as it is now, so ChloeCtx's blame (which reads the file as it is) was not asked."));
    }
    let target = format!("{rel}:{}", now_n.unwrap_or(n));
    let ran = match now_n {
        Some(_) => run_chloectx(&sup.cfg.chloectx_bin, &dir, &["blame", &target, "--json"], Duration::from_secs(120)),
        None => Ran::Failed(String::new()),
    };
    let blame = match ran {
        Ran::Failed(e) if e.is_empty() => None,
        Ran::Out(v) => Some(v),
        Ran::Missing => {
            missing.push(not_installed(&sup.cfg.chloectx_bin) + "; the agent and the prompt come from Workbench's rows.");
            None
        }
        Ran::Failed(e) => {
            missing.push(format!("{e}; the agent and the prompt come from Workbench's rows."));
            None
        }
    };
    // ChloeCtx's edit counts only when it holds the line's text and came
    // before the line's commit; its guess (the newest edit of the file) is
    // not who wrote the line.
    let guessed = blame.as_ref().and_then(|b| b["how"].as_str()).filter(|h| h.starts_with("no recorded edit")).map(str::to_string);
    let late = blame.as_ref().and_then(|b| ctx_ms(&b["edit"]["ts"])).zip(until_ms).is_some_and(|(e, u)| e > u);
    let edit = blame.as_ref().map(|b| &b["edit"]).filter(|e| e.is_object()).filter(|_| guessed.is_none() && !late);
    if let Some(how) = &guessed {
        missing.push(format!("ChloeCtx has no record of an edit that wrote this line's text ({how}), so it is not credited to that edit."));
    } else if late {
        missing.push("ChloeCtx's edit of the file came after the line's commit, so it is not the edit that wrote the line.".into());
    } else if blame.is_some() && edit.is_none() {
        missing.push("ChloeCtx has no record of an edit that wrote this line (it was written outside a recorded session, or by hand).".into());
    }

    // Who: the agent whose session made the edit, or Workbench's guess.
    let session = edit.and_then(|e| e["session"].as_str()).map(str::to_string);
    let edit_ms = edit.and_then(|e| ctx_ms(&e["ts"]));
    let who: Option<Agent> = match &session {
        Some(sid) => {
            let a = s.agent(sid).cloned();
            if a.is_none() {
                missing.push(format!("The session that made the edit ({}) is not one of Workbench's agents.", sid.chars().take(8).collect::<String>()));
            }
            a
        }
        None => guess_agent(sup, &s, asked.as_ref(), &dir, &rel, &text, commit.as_ref().map(|c| c.0.as_str()), &mut missing),
    };

    // Only what the agent did before the line's commit.
    let evs: Vec<Event> = who.as_ref().map(|a| sup.all_events(&a.id).unwrap_or_default()).unwrap_or_default().into_iter().filter(|e| until_ms.is_none_or(|u| e.ts <= u)).collect();
    let tool_ev = who.as_ref().and_then(|a| edit_event(&evs, Path::new(&a.cwd), &rel, &text, edit_ms));
    if who.is_some() && tool_ev.is_none() {
        missing.push(match until_ms {
            Some(_) => format!("Workbench recorded no tool call of {} that edited {rel} before the line's commit (it may have written it with a shell command).", who.as_ref().unwrap().name),
            None => format!("Workbench recorded no tool call of {} that edited {rel}.", who.as_ref().unwrap().name),
        });
    }

    // The prompt: the one before the edit, else the last before the commit.
    let mut prompt = who.as_ref().and_then(|a| prompt_of(&evs, a, &s, tool_ev.map(|e| e.seq), if tool_ev.is_none() { edit_ms.map(|t| t + 999).or(until_ms) } else { None })).map(|p| p.0);
    if let Some(bp) = blame.as_ref().filter(|_| edit.is_some()).map(|b| &b["prompt"]).filter(|p| p.is_object()) {
        let mut p = prompt.take().unwrap_or_else(|| json!({ "by": "you" }));
        p["text"] = bp["text"].clone();
        if p.get("ts").is_none_or(Json::is_null) {
            p["ts"] = ctx_time(&bp["ts"]);
        }
        prompt = Some(p);
    }
    if prompt.is_none() {
        missing.push("The prompt behind the edit could not be found.".into());
    }

    // Lineage: its parents, nearest first.
    let mut lineage = Vec::new();
    let mut at = who.as_ref().and_then(|a| a.parent.clone());
    while let Some(p) = at {
        if lineage.len() > 16 {
            break;
        }
        match s.by_id(&p) {
            Some(pa) => {
                lineage.push(agent_json(sup, pa));
                at = pa.parent.clone();
            }
            None => break,
        }
    }

    // When.
    let when = match (edit, tool_ev) {
        (Some(e), _) => Some(json!({ "edit_ts": ctx_time(&e["ts"]), "turn": e["turn"], "session": e["session"], "how": blame.as_ref().map(|b| b["how"].clone()) })),
        (None, Some(ev)) => Some(json!({ "edit_ts": iso(ev.ts), "turn": null, "session": who.as_ref().and_then(|a| a.session_id.clone()), "how": "Workbench's newest edit of the file by the agent" })),
        _ => None,
    };

    // Rewind: 20 s before the edit's tool call to 3 s after.
    let rewind = match (&who, tool_ev.map(|e| e.ts).or(edit_ms)) {
        (Some(a), Some(t)) => match a.started_ms {
            Some(st) if sup.session_dir(&a.id).join("term.cast").is_file() => {
                let at = t - st;
                Some(json!({ "agent": a.id, "from_ms": (at - REWIND_BEFORE_MS).max(0), "to_ms": (at + REWIND_AFTER_MS).max(0), "edit_ms": at.max(0) }))
            }
            _ => {
                missing.push(format!("There is no terminal recording of {}.", a.name));
                None
            }
        },
        _ => None,
    };

    let found = if edit.is_some() {
        "chloectx"
    } else if who.is_some() {
        "workbench"
    } else {
        "git"
    };
    let (knew, mut why_notes, worth) = match blame.as_ref().filter(|_| edit.is_some()) {
        Some(b) => (knew_of(b), why_of(b), b["causes"].as_array().cloned().unwrap_or_default()),
        None => (Vec::new(), Vec::new(), Vec::new()),
    };
    // No edit record: the notes ChloeCtx distilled from the agent's session
    // between the prompt and the edit (or the commit), by time.
    let mut why_from = if edit.is_some() { "blame" } else { "none" };
    if edit.is_none() {
        if let Some(a) = &who {
            let from = prompt.as_ref().and_then(|p| p["ts_ms"].as_i64()).or(a.started_ms).unwrap_or(a.created);
            let to = tool_ev.map(|e| e.ts + 5 * 60_000).or(until_ms.map(|u| u + 5 * 60_000)).unwrap_or_else(crate::db::now_ms);
            let sessions: Vec<String> = s.sessions_of(&a.id);
            let notes = turn_notes(sup, &dir, &sessions, from, to);
            if !notes.is_empty() {
                why_notes = notes;
                why_from = "session";
            }
        }
    }
    Ok(json!({
        "line": { "path": rel, "line": n, "text": text },
        "where": where_,
        "who": { "agent": who.as_ref().map(|a| agent_json(sup, a)), "lineage": lineage, "prompt": prompt },
        "when": when,
        "knew": knew,
        "why": why_notes,
        "worth_a_look": worth,
        "rewind": rewind,
        "found": found,
        "why_from": why_from,
        "at": at_commit,
        "missing": missing,
    }))
}

/// The text of line `n` of `rel` as commit `sha` has it.
fn read_line_at(dir: &Path, sha: &str, rel: &str, n: usize) -> Result<(String, String), ApiError> {
    let rel = rel.trim_start_matches("./").to_string();
    if rel.split('/').any(|p| p == "..") || rel.starts_with('/') {
        return Err(ApiError::bad(format!("{rel} is not a path of the repository")));
    }
    let body = crate::blast::git_out(dir, &["show", &format!("{sha}:{rel}")]).map_err(|e| ApiError::not_found(format!("{rel} at {sha}: {e}")))?;
    let text = body.lines().nth(n.saturating_sub(1)).filter(|_| n > 0).ok_or_else(|| ApiError::bad(format!("{rel} has {} lines at {sha}, not a line {n}", body.lines().count())))?;
    Ok((rel, text.to_string()))
}

/// `git blame` of one line as commit `sha` has it: the commit that brought
/// it, its author time, subject and committer time (seconds).
fn blame_at(dir: &Path, sha: &str, rel: &str, n: usize) -> Option<(String, i64, String, i64)> {
    let out = crate::blast::git_out(dir, &["blame", "--porcelain", "-L", &format!("{n},{n}"), sha, "--", rel]).ok()?;
    porcelain(&out)
}

fn porcelain(out: &str) -> Option<(String, i64, String, i64)> {
    let sha = out.split_whitespace().next()?.to_string();
    if sha.chars().all(|c| c == '0') {
        return None;
    }
    let field = |k: &str| out.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().to_string());
    let at: i64 = field("author-time ")?.parse().ok()?;
    let ct: i64 = field("committer-time ").and_then(|v| v.parse().ok()).unwrap_or(at);
    Some((sha, at, field("summary ").unwrap_or_default(), ct))
}

fn committer_time(dir: &Path, sha: &str) -> Option<i64> {
    crate::blast::git_out(dir, &["show", "-s", "--format=%ct", sha]).ok()?.trim().parse().ok()
}

/// The line of `rel` in `dir` (as the file is now) whose text is `text`.
fn locate_line(dir: &Path, rel: &str, text: &str) -> Option<usize> {
    let want = text.trim();
    if want.is_empty() {
        return None;
    }
    let p = dir.join(rel);
    let meta = std::fs::metadata(&p).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    String::from_utf8_lossy(&std::fs::read(&p).ok()?).lines().position(|l| l.trim() == want).map(|i| i + 1)
}

/// The decisions, preferences and facts ChloeCtx kept from `sessions`
/// between `from_ms` and `to_ms`.
fn turn_notes(sup: &Supervisor, dir: &Path, sessions: &[String], from_ms: i64, to_ms: i64) -> Vec<Json> {
    let ids: Vec<String> = sessions.iter().filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).map(|s| format!("'{s}'")).collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let sql = format!(
        "SELECT id, kind, text, ts FROM memory WHERE kind IN ('decision', 'preference', 'fact') AND session IN ({}) AND ts >= {} AND ts <= {} ORDER BY ts LIMIT 20",
        ids.join(", "),
        from_ms / 1000 - 1,
        to_ms / 1000 + 1
    );
    let Ran::Out(out) = run_chloectx(&sup.cfg.chloectx_bin, dir, &["sql", "--json", &sql], Duration::from_secs(30)) else { return Vec::new() };
    let cols: Vec<String> = out["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
    let col = |row: &Vec<Json>, name: &str| cols.iter().position(|c| c == name).and_then(|i| row.get(i).cloned()).unwrap_or(Json::Null);
    out["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_array)
        .filter(|r| {
            // A canned answer that ignores the filter must not leak in.
            let t = ctx_ms(&col(r, "ts")).unwrap_or(0);
            t >= from_ms - 1000 && t <= to_ms + 1000 && matches!(col(r, "kind").as_str(), Some("decision" | "preference" | "fact"))
        })
        .map(|r| json!({ "kind": col(r, "kind"), "text": col(r, "text"), "ts": ctx_time(&col(r, "ts")), "id": col(r, "id") }))
        .collect()
}

/// Without ChloeCtx's record: the agent asked about, unless the line's
/// commit is older than its work; for a line of the main checkout, the
/// agent whose branch brought its commit, or (not committed) the newest
/// agent there that edited the file.
#[allow(clippy::too_many_arguments)]
fn guess_agent(sup: &Supervisor, s: &Sessions, asked: Option<&Agent>, dir: &Path, rel: &str, text: &str, sha: Option<&str>, missing: &mut Vec<String>) -> Option<Agent> {
    if let Some(a) = asked {
        if let (Some(sha), Some(base)) = (sha, a.base.as_deref()) {
            if git::is_ancestor(dir, sha, base) {
                missing.push(format!("The line's commit is older than {}'s work (it is in the commit it started from), so no agent is known to have written it.", a.name));
                return None;
            }
        }
        return Some(a.clone());
    }
    let repo = dir.to_string_lossy();
    let mut agents: Vec<&Agent> = s.agents.iter().filter(|a| a.repo == repo).collect();
    agents.sort_by_key(|a| a.created);
    let found = match sha {
        Some(sha) => agents.iter().find(|a| match (a.branch.as_deref(), a.base.as_deref()) {
            (Some(b), Some(base)) => git::is_ancestor(dir, sha, b) && !git::is_ancestor(dir, sha, base),
            _ => false,
        }),
        None => agents.iter().rev().find(|a| a.cwd == repo && edit_event(&sup.all_events(&a.id).unwrap_or_default(), Path::new(&a.cwd), rel, text, None).is_some()),
    };
    if found.is_none() {
        missing.push("No Workbench agent is known to have written the line.".into());
    }
    found.map(|a| (*a).clone())
}

/// A request's repository: a git checkout's main one, or the folder.
pub(crate) fn repo_dir(repo: Option<&str>) -> Result<PathBuf, ApiError> {
    let r = repo.filter(|r| !r.is_empty()).ok_or_else(|| ApiError::bad("repo is required"))?;
    let p = PathBuf::from(r);
    if !p.is_absolute() || !p.is_dir() {
        return Err(ApiError::bad(format!("repo must be the absolute path of a folder, not {r}")));
    }
    Ok(git::checkout(&p).map(|c| c.main).unwrap_or(p))
}

/// The links of a record as `chloectx sql --json` prints them: a list of
/// `{label, to}` (or `[label, to]`, or `label:to` text), or that as text.
fn links_of(v: &Json) -> Vec<(String, String)> {
    match v {
        Json::String(s) => match serde_json::from_str::<Json>(s) {
            Ok(j @ (Json::Array(_) | Json::Object(_))) => links_of(&j),
            _ => s.split_whitespace().filter_map(|w| w.split_once(':').map(|(a, b)| (a.to_string(), b.to_string()))).collect(),
        },
        Json::Array(a) => a
            .iter()
            .filter_map(|l| match l {
                Json::Object(o) => {
                    let label = ["label", "type", "rel"].iter().find_map(|k| o.get(*k).and_then(Json::as_str)).unwrap_or("");
                    let to = ["to", "target", "id"].iter().find_map(|k| o.get(*k).and_then(Json::as_str))?;
                    Some((label.to_string(), to.to_string()))
                }
                Json::Array(p) => Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string())),
                Json::String(s) => s.split_once(':').map(|(a, b)| (a.to_string(), b.to_string())),
                _ => None,
            })
            .collect(),
        Json::Object(_) => links_of(&Json::Array(vec![v.clone()])),
        _ => Vec::new(),
    }
}

/// The ids a record links to by `rel` (`supersedes`, `superseded_by`),
/// whatever shape the links come in: a list of `{label, to}`, a map of
/// `rel` to an id or ids, a JSON string of either, or `rel:id` words.
pub(crate) fn linked(v: &Json, rel: &str) -> Vec<String> {
    let id_like = |s: &str| {
        let s = s.trim().trim_matches(|c: char| c == '"' || c == '\\' || c == ',' || c == '}' || c == ']');
        (!s.is_empty() && s.contains(':') && !s.contains(' ')).then(|| s.to_string())
    };
    match v {
        Json::String(s) => match serde_json::from_str::<Json>(s) {
            Ok(j @ (Json::Array(_) | Json::Object(_))) => linked(&j, rel),
            _ => s.split_whitespace().filter_map(|w| w.strip_prefix(rel).and_then(|r| r.strip_prefix(':'))).filter_map(id_like).collect(),
        },
        Json::Object(o) if o.get(rel).is_some() => match &o[rel] {
            Json::String(x) => id_like(x).into_iter().collect(),
            Json::Array(a) => a.iter().filter_map(Json::as_str).filter_map(id_like).collect(),
            _ => Vec::new(),
        },
        _ => links_of(v).into_iter().filter(|(l, _)| l == rel).filter_map(|(_, to)| id_like(&to)).collect(),
    }
}

pub(crate) fn files_of(v: &Json) -> Vec<String> {
    match v {
        Json::String(s) => s.split_whitespace().map(str::to_string).collect(),
        Json::Array(a) => a.iter().filter_map(|f| f.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// `GET /api/decisions`: the repository's decision notes, newest first.
pub fn decisions(sup: &Supervisor, repo: Option<&str>, limit: usize) -> Result<Json, ApiError> {
    let dir = repo_dir(repo)?;
    let sql = format!("SELECT id, ts, session, text, tag, files, links, ctx FROM memory WHERE kind = 'decision' ORDER BY ts DESC LIMIT {limit}");
    let out = match run_chloectx(&sup.cfg.chloectx_bin, &dir, &["sql", "--json", &sql], Duration::from_secs(60)) {
        Ran::Out(v) => v,
        Ran::Missing => return Ok(json!({ "decisions": [], "missing": [not_installed(&sup.cfg.chloectx_bin)] })),
        Ran::Failed(e) => return Ok(json!({ "decisions": [], "missing": [e] })),
    };
    let cols: Vec<String> = out["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
    let col = |row: &Vec<Json>, name: &str| cols.iter().position(|c| c == name).and_then(|i| row.get(i).cloned()).unwrap_or(Json::Null);
    let s = Sessions::load(sup);
    let mut list = Vec::new();
    for row in out["rows"].as_array().into_iter().flatten().filter_map(Json::as_array) {
        let session = col(row, "session");
        let superseded_by = linked(&col(row, "links"), "superseded_by").into_iter().next();
        let text = col(row, "text").as_str().unwrap_or("").to_string();
        // Yours when you said it ("My decision: …" in a prompt), even in an
        // agent's session; an agent's otherwise, with your Still right if given.
        let yours = crate::capsule::by_user(&text, col(row, "tag").as_str().unwrap_or(""), col(row, "ctx").as_str().unwrap_or(""));
        let id = col(row, "id");
        list.push(json!({
            "id": id,
            "text": text,
            "words": crate::capsule::plain_words(&text),
            "by": if yours { "you" } else { "agent" },
            "confirmed": id.as_str().is_some_and(|i| sup.mark(&format!("review-ok:{i}")).is_some_and(|v| v == "1")),
            "ts": ctx_time(&col(row, "ts")),
            "session": session,
            "agent": session.as_str().and_then(|sid| s.agent(sid)).map(|a| agent_json(sup, a)),
            "superseded_by": superseded_by,
            "superseded": superseded_by.is_some() || col(row, "tag") == "superseded",
            "files": files_of(&col(row, "files")),
        }));
    }
    Ok(json!({ "decisions": list, "missing": [] }))
}

/// `POST /api/ask`: `chloectx ask --json` in the repository, as it prints
/// it, plus `agents`: the Workbench agent of each record whose session is one.
pub fn ask(sup: &Supervisor, repo: Option<&str>, question: &str) -> Result<Json, ApiError> {
    let dir = repo_dir(repo)?;
    if question.trim().is_empty() {
        return Err(ApiError::bad("question is empty"));
    }
    let mut out = match run_chloectx(&sup.cfg.chloectx_bin, &dir, &["ask", "--json", "--", question], Duration::from_secs(300)) {
        Ran::Out(v) if v.is_object() => v,
        Ran::Out(v) => json!({ "answer": v }),
        Ran::Missing => return Ok(json!({ "text": "", "ids": [], "agents": {}, "missing": [not_installed(&sup.cfg.chloectx_bin)] })),
        Ran::Failed(e) => return Ok(json!({ "text": "", "ids": [], "agents": {}, "missing": [e] })),
    };
    let s = Sessions::load(sup);
    let mut agents = Map::new();
    for id in out["ids"].as_array().into_iter().flatten().filter_map(Json::as_str) {
        if let Some(a) = s.of_record(id) {
            agents.insert(id.to_string(), agent_json(sup, a));
        }
    }
    out["agents"] = Json::Object(agents);
    out["missing"] = json!([]);
    Ok(out)
}

/// `GET /api/agents/:id/diff`: what the agent changed from its base,
/// committed or not, untracked files as added.
pub fn diff(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let a = sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
    let dir = PathBuf::from(&a.cwd);
    if !dir.is_dir() {
        return Err(ApiError::conflict(format!("agent {}'s worktree {} is gone", a.name, a.cwd)));
    }
    // An agent from before bases were kept: its HEAD's changes only.
    if a.base.is_none() && git::rev_parse(&dir, "HEAD").is_none() {
        return Err(ApiError::conflict(format!("{} has no commit to compare with", a.cwd)));
    }
    // From where its own change starts: the landing branch's fork, or the
    // top of another agent's branch it is stacked on (Ship's base).
    let main = git::checkout(&dir).map(|c| c.main).unwrap_or_else(|_| PathBuf::from(&a.repo));
    let land = crate::blast::landing(sup, &a, &dir, &main);
    let files = git::diff(&dir, &land.base).map_err(|e| ApiError::internal(format!("git diff failed: {e}")))?;
    Ok(json!({ "base": land.base, "files": files, "landing": land.json(sup) }))
}

/// `GET /api/agents/:id/recording`: a window of its terminal recording.
pub fn recording(sup: &Supervisor, id: &str, from_ms: Option<u64>, to_ms: Option<u64>) -> Result<Json, ApiError> {
    let a = sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
    let (from, to) = (from_ms.unwrap_or(0), to_ms.unwrap_or(u64::MAX));
    if from > to {
        return Err(ApiError::bad("from_ms is after to_ms"));
    }
    let path = sup.session_dir(id).join("term.cast");
    if !path.is_file() {
        return Err(ApiError::not_found(format!("agent {} has no terminal recording", a.name)));
    }
    let mut w = crate::recording::window(&path, from, to).map_err(|e| ApiError::internal(format!("cannot read {}: {e}", path.display())))?;
    w["started_ms"] = json!(a.started_ms);
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_became_of_a_record_and_links() {
        assert_eq!(now_of(&["superseded since, by 1c2e:58".into()]), ("superseded by 1c2e:58".into(), false));
        assert_eq!(now_of(&["deleted since".into()]), ("deleted".into(), false));
        assert_eq!(now_of(&["untrusted source: the web".into()]), ("current".into(), true));
        assert_eq!(now_of(&[]), ("current".into(), false));
        let want = vec![("superseded_by".to_string(), "x:40".to_string())];
        assert_eq!(links_of(&json!([{"label": "superseded_by", "to": "x:40"}])), want);
        assert_eq!(links_of(&json!([["superseded_by", "x:40"]])), want);
        assert_eq!(links_of(&json!("[{\"label\":\"superseded_by\",\"to\":\"x:40\"}]")), want);
        assert_eq!(links_of(&json!("superseded_by:x:40")), want);
        assert_eq!(files_of(&json!("a.rs b.rs")), ["a.rs", "b.rs"]);
        assert_eq!(ctx_time(&json!(0)), json!(iso(0)));
    }
}
