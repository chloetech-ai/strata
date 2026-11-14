//! Story: one agent's session read as chapters (a chapter per turn), its
//! tool calls grouped into steps, what ChloeCtx put in its context
//! (`chloectx receipts --json`), the déjà vus among those, and its tokens.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value as Json};

use crate::db::{Agent, Event};
use crate::narrate;
use crate::supervisor::{one_line, ApiError, Supervisor};
use crate::trail::{self, Ran};

/// A recall this long after a failed tool call is a déjà vu.
const DEJA_VU_MS: i64 = 120_000;

/// One tool call: its `PreToolUse` and `PostToolUse` events.
#[derive(Clone, Debug)]
pub struct Call {
    pub tool: String,
    pub input: Json,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub failed: bool,
    pub seqs: Vec<i64>,
    use_id: Option<String>,
}

fn data_of(ev: &Event) -> Json {
    ev.data.as_deref().and_then(|d| serde_json::from_str(d).ok()).unwrap_or(Json::Null)
}

/// A step's kind for a tool.
pub fn kind_of(tool: &str) -> &'static str {
    match tool {
        "Read" | "LS" | "NotebookRead" => "read",
        "Grep" | "Glob" => "search",
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => "edit",
        "Bash" | "BashOutput" => "run",
        "WebFetch" | "WebSearch" => "web",
        t if t.starts_with("mcp__chloectx__") => "memory",
        t if t.starts_with("mcp__workbench__") || t == "Task" || t == "Agent" => "agents",
        _ => "other",
    }
}

/// A Bash command's first two words: what its calls are grouped by.
fn command_key(cmd: &str) -> String {
    cmd.split_whitespace().filter(|w| !w.contains('=') || w.starts_with('-')).take(2).collect::<Vec<_>>().join(" ")
}

fn group_key(c: &Call) -> String {
    let k = kind_of(&c.tool);
    match k {
        "run" => format!("run:{}", command_key(c.input["command"].as_str().unwrap_or(""))),
        "other" => format!("other:{}", c.tool),
        _ => k.to_string(),
    }
}

/// Whether a tool's response says it failed.
fn response_failed(r: &Json) -> bool {
    r.get("is_error").and_then(Json::as_bool).unwrap_or(false) || r.get("interrupted").and_then(Json::as_bool).unwrap_or(false) || r.get("success").and_then(Json::as_bool) == Some(false)
}

/// The calls of a run of events: a `PreToolUse` opens one, its
/// `PostToolUse` (same `tool_use_id`, else the oldest open call of the
/// tool) closes it, `PostToolUseFailure` fails it, and one still open when
/// the turn ended failed (Claude Code fires no `PostToolUse` then).
pub fn calls(evs: &[Event], turn_over: bool) -> Vec<Call> {
    let mut out: Vec<Call> = Vec::new();
    for ev in evs {
        let Some(tool) = ev.tool.clone() else { continue };
        let d = data_of(ev);
        let use_id = d["tool_use_id"].as_str().map(str::to_string);
        match ev.kind.as_str() {
            "PreToolUse" => out.push(Call { tool, input: d["tool_input"].clone(), started_ms: ev.ts, ended_ms: None, failed: false, seqs: vec![ev.seq], use_id }),
            "PostToolUse" | "PostToolUseFailure" => {
                let failed = ev.kind == "PostToolUseFailure" || response_failed(&d["tool_response"]);
                // Its call: by tool_use_id, else the open call of the tool
                // with the same input, else the newest open call of the tool.
                let input = &d["tool_input"];
                let pick = match &use_id {
                    Some(u) => out.iter().rposition(|c| c.ended_ms.is_none() && c.use_id.as_ref() == Some(u)),
                    None => None,
                }
                .or_else(|| out.iter().rposition(|c| c.ended_ms.is_none() && c.tool == tool && &c.input == input))
                .or_else(|| out.iter().rposition(|c| c.ended_ms.is_none() && c.tool == tool));
                let open = pick.map(|i| &mut out[i]);
                match open {
                    Some(c) => {
                        c.ended_ms = Some(ev.ts);
                        c.failed = failed;
                        c.seqs.push(ev.seq);
                    }
                    None => out.push(Call { tool, input: d["tool_input"].clone(), started_ms: ev.ts, ended_ms: Some(ev.ts), failed, seqs: vec![ev.seq], use_id }),
                }
            }
            _ => {}
        }
    }
    let n = out.len();
    for (i, c) in out.iter_mut().enumerate() {
        // Open with a later call after it, or when the turn is over: it failed.
        if c.ended_ms.is_none() && (turn_over || i + 1 < n) {
            c.failed = true;
        }
    }
    out
}

fn short_list(items: &[String], n: usize) -> String {
    let mut seen = Vec::new();
    for i in items {
        if !seen.contains(i) {
            seen.push(i.clone());
        }
    }
    let more = seen.len() > n;
    let mut s = seen.into_iter().take(n).collect::<Vec<_>>().join(", ");
    if more {
        s.push_str(", …");
    }
    s
}

fn times(n: usize) -> String {
    if n == 1 {
        String::new()
    } else {
        format!(" ×{n}")
    }
}

/// A step of consecutive calls of one kind.
fn step(group: &[Call], cwd: &str) -> Json {
    let k = kind_of(&group[0].tool);
    let n = group.len();
    let files: Vec<String> = group.iter().map(|c| narrate::rel(c.input["file_path"].as_str().or_else(|| c.input["notebook_path"].as_str()).or_else(|| c.input["path"].as_str()).unwrap_or(""), cwd)).filter(|f| !f.is_empty()).collect();
    let quoted = |key: &str| group.iter().filter_map(|c| c.input[key].as_str()).map(|s| format!("“{}”", one_line(s, 40))).collect::<Vec<_>>();
    let (label, detail) = match k {
        "read" => (if n == 1 { format!("Read {}", files.first().cloned().unwrap_or_default()) } else { format!("Read {n} files") }, short_list(&files, 4)),
        "search" => (format!("Searched {}", if n == 1 { "once".to_string() } else { format!("{n} times") }), short_list(&quoted("pattern"), 4)),
        "edit" => {
            let distinct: HashSet<&String> = files.iter().collect();
            (if distinct.len() == 1 { format!("Edited {}", files[0]) } else { format!("Edited {} files", distinct.len()) }, short_list(&files, 4))
        }
        "run" => {
            let key = command_key(group[0].input["command"].as_str().unwrap_or(""));
            let outcomes: Vec<String> = group.iter().enumerate().map(|(i, c)| format!("{}: {}", i + 1, if c.failed { "failed" } else if c.ended_ms.is_none() { "running" } else { "ok" })).collect();
            (format!("Ran {key}{}", times(n)), if n > 1 { outcomes.join(" · ") } else { one_line(group[0].input["command"].as_str().unwrap_or(""), 120) })
        }
        "memory" => (format!("Searched memory{}", times(n)), short_list(&group.iter().filter_map(|c| c.input["query"].as_str().or_else(|| c.input["question"].as_str())).map(|s| format!("“{}”", one_line(s, 40))).collect::<Vec<_>>(), 4)),
        "agents" => {
            let spawned = group.iter().filter(|c| c.tool.ends_with("spawn_agent") || c.tool == "Task" || c.tool == "Agent").count();
            (if spawned > 0 { format!("Started {spawned} agent{}", if spawned == 1 { "" } else { "s" }) } else { format!("Worked with agents{}", times(n)) }, short_list(&group.iter().map(|c| c.tool.rsplit("__").next().unwrap_or("").to_string()).collect::<Vec<_>>(), 4))
        }
        "web" => (if n == 1 { "Read the web".to_string() } else { format!("Read the web {n} times") }, short_list(&group.iter().filter_map(|c| c.input["url"].as_str().or_else(|| c.input["query"].as_str())).map(|s| one_line(s, 60)).collect::<Vec<_>>(), 3)),
        _ => (format!("Used {}{}", group[0].tool, times(n)), String::new()),
    };
    let start = group[0].started_ms;
    let end = group.iter().filter_map(|c| c.ended_ms).max().unwrap_or(start);
    json!({
        "kind": k, "label": label, "detail": detail, "count": n, "started_ms": start, "ms": (end - start).max(0),
        "failed": group.iter().filter(|c| c.failed).count(),
        "running": group.iter().any(|c| c.ended_ms.is_none() && !c.failed),
        "events": group.iter().flat_map(|c| c.seqs.clone()).collect::<Vec<_>>(),
    })
}

/// Consecutive calls of one kind (Bash: of one command) as steps.
pub fn steps(calls: &[Call], cwd: &str) -> Vec<Json> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < calls.len() {
        let key = group_key(&calls[i]);
        let mut j = i + 1;
        while j < calls.len() && group_key(&calls[j]) == key {
            j += 1;
        }
        out.push(step(&calls[i..j], cwd));
        i = j;
    }
    out
}

fn title_of(prompt: &str) -> String {
    let line = narrate::strip_markdown(prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or(""));
    let ws: Vec<&str> = line.split_whitespace().collect();
    let mut t = ws.iter().take(8).copied().collect::<Vec<_>>().join(" ");
    if t.chars().count() > 60 {
        t = t.chars().take(59).collect::<String>() + "…";
    } else if ws.len() > 8 {
        t.push('…');
    }
    t
}

/// Who gave the prompt of a turn: `you`, or the name of the agent that
/// typed it (`send_to_agent`) or spawned this one with it as its task.
fn prompt_by(evs: &[Event], at: usize, first: bool, a: &Agent, name_of: &dyn Fn(&str) -> String) -> String {
    let from = evs[..at].iter().rposition(|e| e.kind == "UserPromptSubmit").map_or(0, |p| p + 1);
    match evs[from..at].iter().rev().find(|e| e.kind == "input") {
        Some(input) => data_of(input)["by"].as_str().map_or("you".into(), name_of),
        None if first && a.task.is_some() => a.parent.as_deref().map_or("you".into(), name_of),
        None => "you".into(),
    }
}

/// The transcripts an agent's hooks named, in the order first named.
fn transcripts(evs: &[Event]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for ev in evs {
        if let Some(p) = data_of(ev)["transcript_path"].as_str() {
            let p = PathBuf::from(p);
            if !out.contains(&p) && narrate::transcript_ok(&p.to_string_lossy()) {
                out.push(p);
            }
        }
    }
    out
}

/// `GET /api/agents/:id/story`.
pub fn story(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let a = sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
    let evs = sup.all_events(id)?;
    let names: HashMap<String, String> = sup.agent_rows().into_iter().map(|a| (a.id, a.name)).collect();
    let name_of = |x: &str| names.get(x).cloned().unwrap_or_else(|| x.to_string());
    let mut missing: Vec<String> = Vec::new();

    // The transcripts: tokens and the assistant's texts.
    let paths = transcripts(&evs);
    let mut seen = HashSet::new();
    let mut tokens = (0, 0, 0);
    let mut texts: Vec<(i64, String)> = Vec::new();
    for p in &paths {
        let entries = narrate::read_tail(p, narrate::STORY_MAX);
        let t = narrate::tokens(&entries, &mut seen);
        tokens = (tokens.0 + t.0, tokens.1 + t.1, tokens.2 + t.2);
        texts.extend(narrate::assistant_texts(&entries));
    }
    if paths.is_empty() {
        missing.push("No session transcript was named by its hooks, so there are no tokens and no summaries from it.".into());
    }

    // Chapters: a turn from each prompt to its Stop.
    let starts: Vec<usize> = evs.iter().enumerate().filter(|(_, e)| e.kind == "UserPromptSubmit").map(|(i, _)| i).collect();
    let mut chapters = Vec::new();
    let mut all_calls: Vec<Call> = Vec::new();
    for (n, &s) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(evs.len());
        let turn = &evs[s..end];
        let stop = turn.iter().find(|e| e.kind == "Stop");
        let over = stop.is_some() || n + 1 < starts.len() || a.state == "exited";
        let cs = calls(turn, over);
        all_calls.extend(cs.iter().cloned());
        let prompt = data_of(&evs[s])["prompt"].as_str().map(str::to_string).or_else(|| evs[s].summary.as_deref().map(|x| x.trim_start_matches("prompt: ").to_string())).unwrap_or_default();
        let started = evs[s].ts;
        let ended = stop.map(|e| e.ts).or_else(|| if over { turn.last().map(|e| e.ts) } else { None });
        let last_text = stop.and_then(|e| data_of(e)["last_assistant_message"].as_str().map(str::to_string)).filter(|t| !t.trim().is_empty()).or_else(|| {
            let until = ended.unwrap_or(i64::MAX);
            texts.iter().rev().find(|(t, _)| *t >= started && *t <= until).map(|(_, x)| x.clone())
        });
        chapters.push(json!({
            "turn": n + 1,
            "prompt": { "text": prompt, "by": prompt_by(&evs, s, n == 0, &a, &name_of) },
            "started_ms": started,
            "ended_ms": ended,
            "title": title_of(&prompt),
            "summary": last_text.map(|t| narrate::sentences(&t, 2, 320)).unwrap_or_default(),
            "state": if over { "done" } else { "now" },
            "steps": steps(&cs, &a.cwd),
        }));
    }

    // What it knew: ChloeCtx's receipts of its sessions.
    let mut sessions: Vec<String> = Vec::new();
    for ev in &evs {
        if let Some(sid) = data_of(ev)["session_id"].as_str() {
            if !sessions.iter().any(|s| s == sid) {
                sessions.push(sid.to_string());
            }
        }
    }
    if sessions.is_empty() {
        sessions.extend(a.session_id.clone());
    }
    let failures: Vec<i64> = all_calls.iter().filter(|c| c.failed).map(|c| c.ended_ms.unwrap_or(c.started_ms)).collect();
    let (knew, deja_vu) = knew(sup, &a, &sessions, &failures, &starts.iter().map(|&s| evs[s].ts).collect::<Vec<_>>(), &mut missing);

    let intent = a.outcome.as_deref().and_then(|o| crate::goals::outcome(sup, o).ok()).map(|o| o.title).or_else(|| a.task.as_deref().map(intent_of)).unwrap_or_default();
    Ok(json!({
        "agent": sup.agent_json_of(id),
        "intent": intent,
        "chapters": chapters,
        "knew": knew,
        "deja_vu": deja_vu,
        "tokens": narrate::tokens_json(tokens),
        "missing": missing,
    }))
}

/// What a task is for, from the whole task and not its first sentence
/// (Lee: the intent of a three-part task was its first clause): the
/// sentences that ask for something, without the ones that only say what
/// not to do and without the done check, up to about 300 characters.
pub fn intent_of(task: &str) -> String {
    let body = task.split("\n\nDone when:").next().unwrap_or(task);
    let all = narrate::sentences(body, 40, 4000);
    let mut parts: Vec<String> = Vec::new();
    let mut s = String::new();
    for c in all.chars() {
        s.push(c);
        if matches!(c, '.' | '!' | '?') {
            parts.push(std::mem::take(&mut s).trim().to_string());
        }
    }
    if !s.trim().is_empty() {
        parts.push(s.trim().to_string());
    }
    let constraint = |p: &str| {
        let l = p.to_lowercase();
        ["don't ", "don\u{2019}t ", "do not ", "never ", "avoid ", "keep ", "leave ", "make sure not", "without ", "no need", "done when", "note:", "p.s."].iter().any(|w| l.starts_with(w))
    };
    let asks: Vec<&String> = parts.iter().filter(|p| !p.is_empty() && !constraint(p)).collect();
    let asks: Vec<&String> = if asks.is_empty() { parts.iter().filter(|p| !p.is_empty()).collect() } else { asks };
    let mut out = String::new();
    for p in asks {
        if !out.is_empty() && out.len() + p.len() > 300 {
            out.push_str(" …");
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(p);
    }
    one_line(&out, 320)
}

/// Seconds or milliseconds since the epoch, as milliseconds.
fn ms_of(v: &Json) -> Option<i64> {
    trail::ctx_ms(v)
}

/// The records ChloeCtx showed the agent's sessions, each once (where it
/// was first shown), and the déjà vus among them: a fix (an `error` record
/// tagged `fix`), or a record shown right after a failed tool call.
fn knew(sup: &Supervisor, a: &Agent, sessions: &[String], failures: &[i64], turn_starts: &[i64], missing: &mut Vec<String>) -> (Vec<Json>, Vec<Json>) {
    let dir = Path::new(&a.cwd);
    let dir = if dir.is_dir() { dir.to_path_buf() } else { PathBuf::from(&a.repo) };
    let mut receipts: Vec<Json> = Vec::new();
    for sid in sessions {
        match trail::run_chloectx(&sup.cfg.chloectx_bin, &dir, &["receipts", "--session", sid, "--limit", "500", "--json"], Duration::from_secs(30)) {
            Ran::Out(v) => receipts.extend(v["receipts"].as_array().cloned().unwrap_or_default()),
            Ran::Missing => {
                missing.push(trail::not_installed(&sup.cfg.chloectx_bin));
                break;
            }
            Ran::Failed(e) => {
                missing.push(e);
                break;
            }
        }
    }
    receipts.sort_by_key(|r| ms_of(&r["ts"]).unwrap_or(0));
    let mut out = Vec::new();
    let mut deja = Vec::new();
    let mut seen = HashSet::new();
    for rc in &receipts {
        let shown_ms = ms_of(&rc["ts"]);
        // Its turn: ChloeCtx's, else the chapter it falls in.
        let turn = rc["turn"].as_i64().filter(|t| *t > 0).or_else(|| shown_ms.map(|t| turn_starts.iter().filter(|s| **s <= t).count() as i64));
        let after_failure = shown_ms.is_some_and(|t| failures.iter().any(|f| t >= *f && t - f <= DEJA_VU_MS));
        for row in rc["rows"].as_array().into_iter().flatten() {
            let id = row["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            let fix = row["kind"] == "fix" || (row["kind"] == "error" && row["tag"] == "fix");
            let kind = if fix { json!("fix") } else { row["kind"].clone() };
            let is_deja = fix || after_failure;
            let text = row["text"].as_str().unwrap_or("").to_string();
            out.push(json!({ "id": id, "kind": kind, "text": text, "score": row["score"], "decision": row["decision"], "turn": turn, "receipt": rc["id"], "shown_ms": shown_ms, "deja_vu": is_deja }));
            if is_deja {
                deja.push(json!({ "text": text, "record": id, "when": Json::Null, "session": Json::Null, "agent": Json::Null, "shown_ms": shown_ms }));
            }
        }
    }
    // When each déjà vu was first learned, and by whom.
    if !deja.is_empty() {
        let ids: Vec<String> = deja.iter().filter_map(|d| d["record"].as_str()).filter(|id| id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_'))).map(|id| format!("'{id}'")).collect();
        if !ids.is_empty() {
            let sql = format!("SELECT id, ts, session FROM memory WHERE id IN ({}) LIMIT 100", ids.join(", "));
            if let Ran::Out(v) = trail::run_chloectx(&sup.cfg.chloectx_bin, &dir, &["sql", "--json", &sql], Duration::from_secs(30)) {
                let cols: Vec<String> = v["columns"].as_array().into_iter().flatten().map(|c| c.as_str().unwrap_or("").to_string()).collect();
                let at = |n: &str| cols.iter().position(|c| c == n);
                let s = trail::Sessions::load(sup);
                for row in v["rows"].as_array().into_iter().flatten().filter_map(Json::as_array) {
                    let get = |n: &str| at(n).and_then(|i| row.get(i)).cloned().unwrap_or(Json::Null);
                    let rid = get("id");
                    for d in deja.iter_mut().filter(|d| d["record"] == rid) {
                        d["when"] = trail::ctx_time(&get("ts"));
                        d["session"] = get("session");
                        d["agent"] = get("session").as_str().and_then(|sid| s.agent(sid)).map(|x| json!({ "id": x.id, "name": x.name })).unwrap_or(Json::Null);
                    }
                }
            }
        }
    }
    (out, deja)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: i64, kind: &str, tool: Option<&str>, data: Json) -> Event {
        Event { id: format!("a:{seq}"), seq, ts: seq * 1000, agent: "a".into(), kind: kind.into(), tool: tool.map(str::to_string), summary: None, data: Some(data.to_string()) }
    }

    #[test]
    fn the_intent_is_the_whole_task() {
        let t = "Add a coupon field to the checkout. Apply the discount before tax. Don't touch the payment code.\n\nDone when: `pytest -q` passes.";
        let i = intent_of(t);
        assert!(i.contains("coupon field") && i.contains("before tax"), "{i}");
        assert!(!i.contains("payment") && !i.contains("pytest"), "{i}");
        assert_eq!(intent_of("Fix the login bug"), "Fix the login bug");
    }

    #[test]
    fn calls_pair_and_steps_group() {
        let read = |seq, kind, f: &str| ev(seq, kind, Some("Read"), json!({ "tool_input": { "file_path": format!("/w/src/{f}") } }));
        let bash = |seq, kind, c: &str| ev(seq, kind, Some("Bash"), json!({ "tool_input": { "command": c } }));
        let evs = vec![
            ev(1, "UserPromptSubmit", None, json!({ "prompt": "go" })),
            read(2, "PreToolUse", "a.rs"),
            read(3, "PostToolUse", "a.rs"),
            read(4, "PreToolUse", "b.rs"),
            read(5, "PostToolUse", "b.rs"),
            bash(6, "PreToolUse", "cargo test -p x"),
            // No PostToolUse: it failed.
            bash(7, "PreToolUse", "cargo test -p y"),
            bash(8, "PostToolUse", "cargo test -p y"),
            bash(9, "PreToolUse", "ls"),
            bash(10, "PostToolUse", "ls"),
        ];
        let cs = calls(&evs, true);
        assert_eq!(cs.len(), 5);
        assert!(cs[2].failed && !cs[3].failed);
        let st = steps(&cs, "/w");
        assert_eq!(st.len(), 3, "{st:?}");
        assert_eq!(st[0]["label"], "Read 2 files");
        assert_eq!(st[0]["detail"], "src/a.rs, src/b.rs");
        assert_eq!(st[0]["events"], json!([2, 3, 4, 5]));
        assert_eq!(st[1]["label"], "Ran cargo test ×2");
        assert_eq!(st[1]["failed"], 1);
        assert_eq!(st[1]["detail"], "1: failed · 2: ok");
        assert_eq!(st[1]["ms"], 2000);
        assert_eq!(st[2]["label"], "Ran ls");
        assert_eq!(kind_of("mcp__chloectx__memory_search"), "memory");
        assert_eq!(kind_of("mcp__workbench__spawn_agent"), "agents");
        assert_eq!(title_of("**Fix** the login bug in the auth module before release please"), "Fix the login bug in the auth module…");
    }
}
