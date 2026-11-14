//! Away: when you last looked (`seen`), and the digest of what happened
//! since: outcomes done, questions decided (by you, a rule or the
//! terminal) and still open, what ChloeCtx learned, a story line per
//! agent, and the moments of decisions to replay.
//!
//! Sixth build: whether you were away at all (`here`, a heartbeat the page
//! sends while it is shown and used), "I'm stepping away" (`away_mark`,
//! with the agents likely to need you soon), how long each agent was
//! blocked on you in the window, and what needs you now counted as the
//! top bar counts it (running agents that wait or sit idle).

use std::collections::{BTreeMap, HashSet};

use axum::body::Bytes;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde_json::{json, Value as Json};

use crate::db::{get_ms, get_str, iso, now_ms, ts, Agent, Event};
use crate::levels::Levels;
use crate::server::{blocking, ApiResult, App};
use crate::supervisor::{one_line, ApiError, Supervisor};
use crate::{goals, questions};

/// When there is no `last_seen` yet and no agent: the last day.
const DEFAULT_WINDOW_MS: i64 = 24 * 3600 * 1000;
/// A heartbeat this long after `since` means you were here, not away.
const HERE_AFTER_MS: i64 = 90_000;
/// An agent that asked you this recently is likely to ask again.
const ASKS_RECENT_MS: i64 = 15 * 60_000;
/// A replay's window before and after its moment.
const BEFORE_MS: i64 = 20_000;
const AFTER_MS: i64 = 3_000;

/// The value of a key of `seen`, and when it was set.
pub fn get_seen(sup: &Supervisor, key: &str) -> Option<(String, i64)> {
    let p = Params::new().set("key", Value::Str(key.to_string()));
    let rows = sup.db().query("SELECT value, ts FROM seen WHERE id = :key LIMIT 1", &p).ok()?;
    let r = rows.first()?;
    Some((get_str(&r[0])?, get_ms(&r[1]).unwrap_or(0)))
}

pub fn set_seen(sup: &Supervisor, key: &str, value: &str) -> Result<(), ApiError> {
    let p = Params::new().set("key", Value::Str(key.to_string())).set("value", Value::Str(value.to_string())).set("ts", ts(now_ms()));
    sup.db().query("INSERT OR REPLACE INTO seen (id, value, ts) VALUES (:key, :value, :ts)", &p).map_err(ApiError::internal)?;
    Ok(())
}

fn last_seen(sup: &Supervisor) -> Option<i64> {
    get_seen(sup, "last_seen").and_then(|(v, _)| v.parse().ok())
}

/// `POST /api/seen`: you looked now (or at `ts_ms`).
pub fn seen(sup: &Supervisor, at: Option<i64>) -> Result<Json, ApiError> {
    let before = last_seen(sup);
    let t = at.filter(|t| *t > 0).unwrap_or_else(now_ms);
    set_seen(sup, "last_seen", &t.to_string())?;
    Ok(json!({ "ok": true, "last_seen_ms": t, "previous_ms": before }))
}

/// Events since `since` (ms), oldest first.
fn events_since(sup: &Supervisor, since: i64) -> Vec<Event> {
    let p = Params::new().set("since", ts(since));
    let rows = sup.db().query("SELECT id, seq, ts, agent, kind, tool, summary FROM events WHERE ts >= :since ORDER BY seq ASC LIMIT 200000", &p).unwrap_or_default();
    rows.iter()
        .map(|r| Event {
            id: get_str(&r[0]).unwrap_or_default(),
            seq: crate::db::get_int(&r[1]).unwrap_or(0),
            ts: get_ms(&r[2]).unwrap_or(0),
            agent: get_str(&r[3]).unwrap_or_default(),
            kind: get_str(&r[4]).unwrap_or_default(),
            tool: get_str(&r[5]),
            summary: get_str(&r[6]),
            data: None,
        })
        .collect()
}

/// A replay window around `at` (ms since the epoch) in an agent's
/// recording, when there is one.
fn replay_of(sup: &Supervisor, a: &Agent, at: i64, label: String) -> Option<Json> {
    let st = a.started_ms?;
    // ChloeCtx's records keep whole seconds: one of the session's first
    // second can read as a moment before it started.
    if !sup.session_dir(&a.id).join("term.cast").is_file() || at < st - 1000 {
        return None;
    }
    let rel = (at - st).max(0);
    Some(json!({ "agent": a.id, "from_ms": (rel - BEFORE_MS).max(0), "to_ms": rel + AFTER_MS, "at_ms": at, "label": label }))
}

fn seen_ms(sup: &Supervisor, key: &str) -> Option<i64> {
    get_seen(sup, key).and_then(|(v, _)| v.parse().ok())
}

/// What needs you now, as the top bar counts it: running agents waiting
/// on you or idle at their prompt, oldest first.
fn needs_you(sup: &Supervisor, agents: &[Agent]) -> Vec<Json> {
    let mut v: Vec<&Agent> = agents.iter().filter(|a| sup.is_running(&a.id) && (a.state == "waiting" || a.state == "idle")).collect();
    v.sort_by_key(|a| a.state_since);
    v.iter()
        .map(|a| json!({ "agent": a.id, "name": a.name, "state": a.state, "since_ms": a.state_since, "why": a.waiting_for.clone().or_else(|| a.narration.clone()) }))
        .collect()
}

/// How long each agent was blocked on you in `[since, now]`: its
/// questions (a permission prompt, an ask) from asked to answered, and
/// its idle stretches (a turn ended, no prompt yet) while it ran.
fn blocked(sup: &Supervisor, agents: &[Agent], qs: &[questions::Question], since: i64, now: i64) -> Vec<Json> {
    let clip = |a: i64, b: i64| (a.max(since), b.min(now));
    let mut out = Vec::new();
    for a in agents {
        let mut waits: Vec<Json> = Vec::new();
        let mut wait_ms = 0;
        for q in qs.iter().filter(|q| q.agent.as_deref() == Some(&a.id) && matches!(q.kind.as_str(), "permission" | "ask")) {
            let end = q.answered.unwrap_or(if q.state == "open" { now } else { q.created });
            let (f, t) = clip(q.created, end);
            if t <= f {
                continue;
            }
            wait_ms += t - f;
            let what = q.context.as_deref().map(|c| one_line(c, 120)).unwrap_or_else(|| one_line(&q.text, 120));
            waits.push(json!({ "kind": q.kind, "from_ms": f, "to_ms": t, "ms": t - f, "open": q.state == "open", "text": what, "question": q.id,
                               "answered_by": q.answered_by }));
        }
        // Idle stretches: from each Stop to the next prompt (or now, while it runs).
        let p = Params::new().set("agent", Value::Str(a.id.clone())).set("since", ts(since - DEFAULT_WINDOW_MS));
        let evs = sup.db().query("SELECT ts, kind FROM events WHERE agent = :agent AND ts >= :since AND kind IN ('Stop', 'UserPromptSubmit', 'steer', 'exited', 'SessionEnd') ORDER BY seq ASC LIMIT 20000", &p).unwrap_or_default();
        let mut idle_ms = 0;
        let mut idle_from: Option<i64> = None;
        for r in &evs {
            let at = get_ms(&r[0]).unwrap_or(0);
            match get_str(&r[1]).as_deref() {
                Some("Stop") => {
                    idle_from.get_or_insert(at);
                }
                _ => {
                    if let Some(f) = idle_from.take() {
                        let (f, t) = clip(f, at);
                        idle_ms += (t - f).max(0);
                    }
                }
            }
        }
        if let Some(f) = idle_from.filter(|_| sup.is_running(&a.id) && a.state == "idle") {
            let (f, t) = clip(f, now);
            idle_ms += (t - f).max(0);
        }
        if wait_ms == 0 && idle_ms == 0 {
            continue;
        }
        out.push(json!({ "agent": a.id, "name": a.name, "wait_ms": wait_ms, "idle_ms": idle_ms, "waits": waits, "running": sup.is_running(&a.id), "state": a.state }));
    }
    out.sort_by_key(|b| -(b["wait_ms"].as_i64().unwrap_or(0) + b["idle_ms"].as_i64().unwrap_or(0)));
    out
}

/// `GET /api/digest?since=<ms>&repo=`.
pub fn digest(sup: &Supervisor, lv: &Levels, since: Option<i64>, repo: Option<&str>) -> Result<Json, ApiError> {
    let now = now_ms();
    let seen_at = last_seen(sup);
    let repo = match repo.filter(|r| !r.is_empty()) {
        Some(r) => Some(crate::trail::repo_dir(Some(r))?.to_string_lossy().to_string()),
        None => None,
    };
    let in_repo = |r: Option<&str>| repo.as_deref().is_none_or(|want| r == Some(want));
    questions::expire_stale(sup, lv);
    let agents: Vec<Agent> = sup.agent_rows().into_iter().filter(|a| in_repo(Some(&a.repo))).collect();
    // Your first visit: since the first agent started, not a made-up day.
    let first_start = agents.iter().map(|a| a.started_ms.unwrap_or(a.created)).filter(|t| *t > 0).min();
    let explicit = since.is_some();
    // A first look with nothing started: no window at all ("since you left"
    // makes no sense on a first visit), so nothing is counted twice later.
    let since = since.or(seen_at).or(first_start).unwrap_or(if repo.is_some() { now } else { now - DEFAULT_WINDOW_MS });
    let here = seen_ms(sup, "here");
    let mark = seen_ms(sup, "away_mark");
    // Away, stepped away (you said so), here all along (the page was in
    // use after `since`), or the first look.
    let mode = if explicit {
        "since"
    } else if seen_at.is_none() {
        "first"
    } else if mark == Some(since) && here.is_none_or(|h| h < since + HERE_AFTER_MS) {
        "stepped"
    } else if here.is_some_and(|h| h >= since + HERE_AFTER_MS) {
        "here"
    } else {
        "away"
    };
    let by_id = |id: &str| agents.iter().find(|a| a.id == id);
    let ids: HashSet<String> = agents.iter().map(|a| a.id.clone()).collect();

    // Outcomes done since.
    let goal_repo: BTreeMap<String, String> = goals::goals(sup, None).into_iter().map(|g| (g.id, g.repo)).collect();
    let done: Vec<goals::Outcome> = goals::refresh(sup, lv, None).into_iter().filter(|o| o.state == "done" && o.updated >= since && in_repo(goal_repo.get(&o.goal).map(String::as_str))).collect();

    // Questions answered since, and open ones.
    let qs: Vec<questions::Question> = questions::all(sup).into_iter().filter(|q| in_repo(q.repo.as_deref())).collect();
    let decided: Vec<&questions::Question> = qs.iter().filter(|q| q.state == "answered" && q.answered.is_some_and(|t| t >= since)).collect();
    let open: Vec<&questions::Question> = qs.iter().filter(|q| q.state == "open").collect();
    let by = |p: &str| decided.iter().filter(|q| q.answered_by.as_deref().unwrap_or("").starts_with(p)).count();
    let rules: Vec<questions::Rule> = questions::rules(sup, repo.as_deref(), false).into_iter().filter(|r| r.created >= since).collect();

    // What ChloeCtx learned since, in the repositories that saw work.
    let evs: Vec<Event> = events_since(sup, since).into_iter().filter(|e| ids.contains(&e.agent)).collect();
    let mut repos: Vec<String> = Vec::new();
    for e in &evs {
        if let Some(a) = by_id(&e.agent) {
            if !repos.contains(&a.repo) {
                repos.push(a.repo.clone());
            }
        }
    }
    if let Some(r) = &repo {
        if !repos.contains(r) {
            repos.insert(0, r.clone());
        }
    }
    let mut learned = Vec::new();
    let mut missing = Vec::new();
    for r in repos.iter().take(5) {
        let (l, m) = goals::learned(sup, r, since / 1000, 100);
        learned.extend(l);
        for x in m {
            if !missing.contains(&x) {
                missing.push(x);
            }
        }
    }

    // A story line per agent that did something since.
    let mut active: Vec<&Agent> = agents.iter().filter(|a| evs.iter().any(|e| e.agent == a.id)).collect();
    active.sort_by_key(|a| evs.iter().filter(|e| e.agent == a.id).map(|e| e.ts).max().unwrap_or(0));
    let mut story = Vec::new();
    for a in active {
        let mine: Vec<&Event> = evs.iter().filter(|e| e.agent == a.id).collect();
        let last = mine.last().map(|e| e.ts).unwrap_or(since);
        let turns: Vec<String> = mine.iter().filter(|e| e.kind == "UserPromptSubmit").filter_map(|e| e.summary.as_deref()).map(|s| one_line(s.trim_start_matches("prompt: "), 80)).collect();
        let mut text = match turns.len() {
            0 => String::new(),
            1 => format!("worked on “{}”", turns[0]),
            n => format!("worked on {n} turns, the last “{}”", turns[n - 1]),
        };
        let now_line = match a.state.as_str() {
            "waiting" => a.waiting_for.as_deref().map(|w| format!("is waiting on you: {}", one_line(w, 160))),
            "exited" => Some(format!("ended{}", a.exit_code.map(|c| format!(" (exit {c})")).unwrap_or_default())),
            _ => a.narration.as_deref().map(|n| format!("now: {}", one_line(n, 160))),
        };
        if let Some(l) = now_line {
            if !text.is_empty() {
                text.push_str("; ");
            }
            text.push_str(&l);
        }
        let mut chips = Vec::new();
        let edits: HashSet<String> = mine.iter().filter(|e| e.kind == "PostToolUse" && matches!(e.tool.as_deref(), Some("Edit" | "MultiEdit" | "Write" | "NotebookEdit"))).filter_map(|e| e.summary.clone()).collect();
        if !edits.is_empty() {
            chips.push(json!({ "kind": "edits", "text": format!("{} file{} edited", edits.len(), if edits.len() == 1 { "" } else { "s" }) }));
        }
        for o in done.iter().filter(|o| o.agent.as_deref() == Some(&a.id)) {
            chips.push(json!({ "kind": "done", "text": o.title }));
        }
        for q in decided.iter().filter(|q| q.agent.as_deref() == Some(&a.id)) {
            let who = q.answered_by.as_deref().unwrap_or("you");
            let kind = if who.starts_with("rule:") { "rule" } else { "decided" };
            chips.push(json!({ "kind": kind, "text": one_line(q.answer.as_deref().unwrap_or(""), 80), "question": q.id, "by": who }));
        }
        for q in open.iter().filter(|q| q.agent.as_deref() == Some(&a.id)) {
            chips.push(json!({ "kind": "waiting", "text": one_line(&q.text, 80), "question": q.id }));
        }
        story.push(json!({ "ts": iso(last), "ts_ms": last, "agent": sup.agent_json_of(&a.id), "text": text, "chips": chips }));
    }

    // The moments of decisions: answered questions, and decision records.
    let mut replay = Vec::new();
    for q in &decided {
        if let (Some(a), Some(t)) = (q.agent.as_deref().and_then(by_id), q.answered) {
            if let Some(r) = replay_of(sup, a, t, format!("{}: {}", a.name, one_line(&q.text, 80))) {
                replay.push(r);
            }
        }
    }
    let s = crate::trail::Sessions::load(sup);
    for rec in learned.iter().filter(|r| r["kind"] == "decision") {
        let a = rec["session"].as_str().and_then(|sid| s.agent(sid)).filter(|a| ids.contains(&a.id));
        if let (Some(a), Some(t)) = (a, rec["ts_ms"].as_i64()) {
            if let Some(r) = replay_of(sup, a, t, format!("{}: {}", a.name, one_line(rec["text"].as_str().unwrap_or(""), 80))) {
                replay.push(r);
            }
        }
    }
    replay.sort_by_key(|r| r["at_ms"].as_i64().unwrap_or(0));

    // Merges since (Review's notes), and agents that ended since: things
    // that happened even when no agent is working now.
    let merged: Vec<Json> = evs
        .iter()
        .filter(|e| e.kind == "merged")
        .filter_map(|e| {
            let d: Json = e.data.as_deref().and_then(|d| serde_json::from_str(d).ok()).unwrap_or(Json::Null);
            Some(json!({ "agent": e.agent, "name": by_id(&e.agent)?.name, "branch": d["branch"], "onto": d["onto"], "commit": d["commit"], "ts_ms": e.ts }))
        })
        .collect();
    let ended: Vec<Json> = agents.iter().filter(|a| a.ended.is_some_and(|t| t >= since)).map(|a| json!({ "agent": a.id, "name": a.name, "ended_ms": a.ended })).collect();
    let mut qlist: Vec<Json> = open.iter().map(|q| q.to_json(sup)).collect();
    qlist.extend(decided.iter().map(|q| q.to_json(sup)));
    let blocked = blocked(sup, &agents, &qs, since, now);
    let needs = needs_you(sup, &agents);
    Ok(json!({
        "since_ms": since,
        "until_ms": now,
        "last_seen_ms": seen_at,
        "away_ms": now - since,
        "mode": mode,
        "here_ms": here,
        "needs_you": needs,
        "blocked": blocked,
        "tiles": { "done": done.len(), "decided": decided.len(), "waiting": open.len(), "learned": learned.len() },
        "decided_by": { "you": by("you"), "rule": by("rule:"), "terminal": by("terminal") },
        "done": done.iter().map(|o| o.to_json(sup)).collect::<Vec<_>>(),
        "story": story,
        "questions": qlist,
        "rules": rules.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        "learned": learned,
        "replay": replay,
        "merged": merged,
        "ended": ended,
        "happened": !evs.is_empty() || !done.is_empty() || !decided.is_empty(),
        "missing": missing,
    }))
}

/// `POST /api/away/leave`: "I'm stepping away". Marks the moment (the next
/// digest counts from it) and answers nothing for you; it says which
/// agents are likely to need you while you are gone: those waiting now,
/// those idle (they have nothing to do), and those that asked you lately.
pub fn leave(sup: &Supervisor) -> Result<Json, ApiError> {
    let now = now_ms();
    set_seen(sup, "last_seen", &now.to_string())?;
    set_seen(sup, "away_mark", &now.to_string())?;
    let agents: Vec<Agent> = sup.agent_rows().into_iter().filter(|a| sup.is_running(&a.id)).collect();
    let qs = questions::all(sup);
    let mut warn = Vec::new();
    for a in &agents {
        let asked: Vec<&questions::Question> = qs.iter().filter(|q| q.agent.as_deref() == Some(&a.id) && q.created >= now - ASKS_RECENT_MS && matches!(q.kind.as_str(), "permission" | "ask")).collect();
        let open = qs.iter().find(|q| q.agent.as_deref() == Some(&a.id) && q.state == "open");
        let (level, text) = if let Some(q) = open {
            ("now", format!("waits for you now: {}", one_line(q.context.as_deref().unwrap_or(&q.text), 120)))
        } else if a.state == "waiting" {
            ("now", format!("waits for you now: {}", one_line(a.waiting_for.as_deref().unwrap_or("a prompt in its terminal"), 120)))
        } else if a.state == "idle" {
            ("idle", "is idle: it has nothing to do until you give it something".to_string())
        } else if !asked.is_empty() {
            ("soon", format!("asked you {} in the last {} min: it will likely ask again", match asked.len() {
                1 => "once".to_string(),
                2 => "twice".to_string(),
                n => format!("{n} times"),
            }, ASKS_RECENT_MS / 60_000))
        } else {
            ("ok", "is working and has not asked you lately".to_string())
        };
        warn.push(json!({ "agent": a.id, "name": a.name, "level": level, "text": text }));
    }
    let rank = |w: &Json| match w["level"].as_str() {
        Some("now") => 0,
        Some("soon") => 1,
        Some("idle") => 2,
        _ => 3,
    };
    warn.sort_by_key(rank);
    Ok(json!({ "ok": true, "at_ms": now, "agents": warn }))
}

pub fn routes() -> Router<App> {
    Router::new().route("/api/away/leave", post(leave_route)).route("/api/here", post(here_route))
}

async fn leave_route(State(app): State<App>, _body: Bytes) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || leave(&sup)).await?).into_response())
}

/// `POST /api/here`: the page is shown and in use (a heartbeat, at most a
/// minute apart): you are not away.
async fn here_route(State(app): State<App>, _body: Bytes) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || {
        let t = now_ms();
        set_seen(&sup, "here", &t.to_string())?;
        Ok(json!({ "ok": true, "here_ms": t }))
    })
    .await?)
    .into_response())
}
