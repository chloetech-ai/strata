//! The interrupt model (JOURNEYS.md, "Rules and notifications"; the UX
//! critique's power-user recommendation 5): when Workbench interrupts you.
//!
//! - A level per project: every question (`all`), only an agent blocked on
//!   you for more than N minutes (`blocked`, the default, 2 min), or only
//!   alerts (`alerts`). A critical alert interrupts at every level.
//! - Quiet mode ("quiet for 25 min"): nothing interrupts but a critical
//!   alert; the rest is held.
//! - A digest when you come back from being away (no heartbeat from the
//!   page, `POST /api/here` in src/digest.rs) or from quiet: one summary of
//!   what happened, rather than a ping per prompt.
//!
//! The server decides what is due; the page shows it (a desktop
//! notification carries the command and Claude Code's options, and never
//! answers anything: clicking it opens the card). State is in marks:
//! `notify:settings`, `notify:quiet`, `notify:digest_seen`.

use std::collections::{BTreeMap, HashSet};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use serde_json::{json, Value as Json};

use crate::db::{iso, now_ms, Agent};
use crate::questions::{self, Question};
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{one_line, ApiError, Supervisor};

/// The levels, in the order the page offers them.
pub const LEVELS: [(&str, &str, &str); 3] = [
    ("all", "Every question", "Each permission prompt and question as it comes, and every alert that needs you."),
    ("blocked", "Only when an agent is blocked", "An agent that has waited on you for more than N minutes, and major or critical alerts."),
    ("alerts", "Only alerts", "Alerts that need you. Questions wait in Needs you without a notification."),
];
const DEFAULT_LEVEL: &str = "blocked";
const DEFAULT_MINUTES: i64 = 2;
const QUIET_DEFAULT_MIN: i64 = 25;
/// No heartbeat for this long: you were away.
const AWAY_AFTER_MS: i64 = 5 * 60_000;

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/notify/settings", get(settings_route).put(put_route))
        .route("/api/notify/quiet", post(quiet_route))
        .route("/api/notify/pending", get(pending_route))
        .route("/api/notify/seen", post(seen_route))
}

fn mark_json(sup: &Supervisor, key: &str) -> Json {
    sup.mark(key).and_then(|v| serde_json::from_str(&v).ok()).unwrap_or(Json::Null)
}

/// The settings, with the defaults filled in.
fn settings(sup: &Supervisor) -> Json {
    let mut s = mark_json(sup, "notify:settings");
    if !s.is_object() {
        s = json!({});
    }
    if !s["default"].is_object() {
        s["default"] = json!({ "level": DEFAULT_LEVEL, "minutes": DEFAULT_MINUTES });
    }
    if !s["projects"].is_object() {
        s["projects"] = json!({});
    }
    if !s["desktop"].is_boolean() {
        s["desktop"] = json!(false);
    }
    s
}

/// A project's level and minutes.
fn level_of(s: &Json, repo: &str) -> (String, i64) {
    let p = &s["projects"][repo];
    let d = &s["default"];
    let level = p["level"].as_str().or(d["level"].as_str()).unwrap_or(DEFAULT_LEVEL).to_string();
    let minutes = p["minutes"].as_i64().or(d["minutes"].as_i64()).unwrap_or(DEFAULT_MINUTES);
    (level, minutes)
}

fn quiet_of(sup: &Supervisor, now: i64) -> Json {
    let q = mark_json(sup, "notify:quiet");
    let from = q["from_ms"].as_i64();
    let until = q["until_ms"].as_i64();
    let on = until.is_some_and(|u| u > now);
    json!({ "on": on, "from_ms": from, "until_ms": until, "left_ms": until.filter(|_| on).map(|u| u - now), "ended_ms": until.filter(|_| !on) })
}

fn settings_json(sup: &Supervisor) -> Json {
    let s = settings(sup);
    let now = now_ms();
    let mut repos: Vec<String> = sup.agent_rows().into_iter().map(|a| a.repo).collect();
    repos.extend(s["projects"].as_object().into_iter().flat_map(|o| o.keys().cloned()));
    repos.sort();
    repos.dedup();
    let projects: Vec<Json> = repos
        .iter()
        .filter(|r| !r.is_empty())
        .map(|r| {
            let (level, minutes) = level_of(&s, r);
            json!({ "repo": r, "name": base(r), "level": level, "minutes": minutes, "own": s["projects"].get(r).is_some() })
        })
        .collect();
    json!({
        "default": s["default"],
        "projects": projects,
        "desktop": s["desktop"],
        "levels": LEVELS.iter().map(|(id, label, says)| json!({ "id": id, "label": label, "says": says })).collect::<Vec<_>>(),
        "quiet": quiet_of(sup, now),
        "critical_always": true,
    })
}

fn base(p: &str) -> String {
    std::path::Path::new(p).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string())
}

#[derive(serde::Deserialize, Default)]
struct PutB {
    /// A project's own level; without it, the default.
    repo: Option<String>,
    level: Option<String>,
    minutes: Option<i64>,
    /// The project goes back to the default.
    reset: Option<bool>,
    /// The page's "Notify me" toggle (the browser's permission is the page's).
    desktop: Option<bool>,
}

/// `PUT /api/notify/settings {repo?, level?, minutes?, reset?, desktop?}`.
fn put(sup: &Supervisor, b: PutB) -> Result<Json, ApiError> {
    let mut s = settings(sup);
    if let Some(l) = &b.level {
        if !LEVELS.iter().any(|(id, _, _)| id == l) {
            return Err(ApiError::bad(format!("level is one of {}", LEVELS.iter().map(|l| l.0).collect::<Vec<_>>().join(", "))));
        }
    }
    if let Some(m) = b.minutes {
        if !(1..=240).contains(&m) {
            return Err(ApiError::bad("minutes is from 1 to 240"));
        }
    }
    match b.repo.as_deref().filter(|r| !r.is_empty()) {
        Some(r) => {
            let repo = crate::trail::repo_dir(Some(r))?.to_string_lossy().to_string();
            if b.reset == Some(true) {
                if let Some(o) = s["projects"].as_object_mut() {
                    o.remove(&repo);
                }
            } else {
                let mut p = s["projects"].get(&repo).cloned().filter(Json::is_object).unwrap_or_else(|| json!({}));
                if let Some(l) = &b.level {
                    p["level"] = json!(l);
                }
                if let Some(m) = b.minutes {
                    p["minutes"] = json!(m);
                }
                s["projects"][repo] = p;
            }
        }
        None => {
            if let Some(l) = &b.level {
                s["default"]["level"] = json!(l);
            }
            if let Some(m) = b.minutes {
                s["default"]["minutes"] = json!(m);
            }
        }
    }
    if let Some(d) = b.desktop {
        s["desktop"] = json!(d);
    }
    sup.set_mark("notify:settings", &s.to_string())?;
    Ok(settings_json(sup))
}

#[derive(serde::Deserialize, Default)]
struct QuietB {
    /// How long; 0 ends quiet now. Default 25.
    minutes: Option<i64>,
}

/// `POST /api/notify/quiet {minutes}`.
fn quiet(sup: &Supervisor, b: QuietB) -> Result<Json, ApiError> {
    let now = now_ms();
    let m = b.minutes.unwrap_or(QUIET_DEFAULT_MIN);
    if !(0..=24 * 60).contains(&m) {
        return Err(ApiError::bad("minutes is from 0 (end quiet now) to 1440"));
    }
    let cur = quiet_of(sup, now);
    let v = if m == 0 {
        // Ending it keeps when it began: the digest covers the quiet time.
        json!({ "from_ms": cur["from_ms"].as_i64().unwrap_or(now), "until_ms": now })
    } else {
        let from = if cur["on"] == true { cur["from_ms"].as_i64().unwrap_or(now) } else { now };
        json!({ "from_ms": from, "until_ms": now + m * 60_000 })
    };
    sup.set_mark("notify:quiet", &v.to_string())?;
    Ok(json!({ "ok": true, "quiet": quiet_of(sup, now) }))
}

/// Something that may interrupt you.
struct Item {
    kind: &'static str,
    repo: String,
    since: i64,
    critical: bool,
    severity: Option<String>,
    json: Json,
}

/// What a question shows in a notification: the command (or the diff's
/// file, or the question) and Claude Code's options with their numbers.
fn question_item(sup: &Supervisor, q: &Question, a: Option<&Agent>, now: i64) -> Item {
    let d = q.detail.clone().unwrap_or(Json::Null);
    let command = d["command"].as_str().map(str::to_string).or_else(|| d["rel"].as_str().or(d["file"].as_str()).map(|f| format!("{} {f}", d["tool"].as_str().unwrap_or("Edit"))));
    let options: Vec<Json> = q
        .options
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, o)| {
            let label = o["label"].as_str().or(o.as_str()).unwrap_or("").to_string();
            json!({ "n": o["n"].as_i64().unwrap_or(i as i64 + 1), "label": label, "grants": o.get("grants") })
        })
        .collect();
    let name = a.map(|a| a.name.clone()).unwrap_or_else(|| "an agent".into());
    let ask = if q.kind == "permission" { d.pointer("/menu/question").and_then(Json::as_str).unwrap_or(&q.text).to_string() } else { q.text.clone() };
    let agent = q.agent.clone().unwrap_or_default();
    Item {
        kind: "question",
        repo: q.repo.clone().unwrap_or_default(),
        since: q.created,
        critical: false,
        severity: None,
        json: json!({
            "id": format!("q:{}", q.id), "kind": "question", "question": q.id, "qkind": q.kind, "agent": agent, "name": name,
            "repo": q.repo, "project": q.repo.as_deref().map(base), "since_ms": q.created, "waited_ms": now - q.created,
            "title": format!("{name} waits for you"), "ask": one_line(&ask, 200),
            "command": command.map(|c| cap_lines(&c, 6)), "tool": d["tool"], "options": options,
            "href": format!("#/a/{agent}?q={}", q.id),
            "running": sup.is_running(&agent),
        }),
    }
}

/// At most `n` lines of a command, and how many there were.
fn cap_lines(c: &str, n: usize) -> Json {
    let lines: Vec<&str> = c.lines().collect();
    json!({ "text": lines.iter().take(n).copied().collect::<Vec<_>>().join("\n"), "lines": lines.len(), "cut": lines.len() > n })
}

/// Everything that could interrupt you now: open questions, agents waiting
/// with no question (a prompt Workbench could not read), open alerts.
fn items(sup: &Supervisor, lv: &crate::levels::Levels, now: i64) -> Vec<Item> {
    questions::expire_stale(sup, lv);
    let agents: BTreeMap<String, Agent> = sup.agent_rows().into_iter().map(|a| (a.id.clone(), a)).collect();
    let mut out = Vec::new();
    let mut asking: HashSet<String> = HashSet::new();
    for q in questions::all(sup).into_iter().filter(|q| q.state == "open" && matches!(q.kind.as_str(), "permission" | "ask")) {
        let a = q.agent.as_deref().and_then(|id| agents.get(id));
        if let Some(a) = a {
            if !sup.is_running(&a.id) {
                continue;
            }
            asking.insert(a.id.clone());
        }
        out.push(question_item(sup, &q, a, now));
    }
    for a in agents.values().filter(|a| a.state == "waiting" && sup.is_running(&a.id) && !asking.contains(&a.id)) {
        out.push(Item {
            kind: "blocked",
            repo: a.repo.clone(),
            since: a.state_since,
            critical: false,
            severity: None,
            json: json!({
                "id": format!("w:{}:{}", a.id, a.state_since), "kind": "blocked", "agent": a.id, "name": a.name, "repo": a.repo, "project": base(&a.repo),
                "since_ms": a.state_since, "waited_ms": now - a.state_since, "title": format!("{} waits for you", a.name),
                "ask": one_line(a.waiting_for.as_deref().unwrap_or("a prompt in its terminal"), 200), "command": Json::Null, "options": [],
                "href": format!("#/a/{}/terminal", a.id),
            }),
        });
    }
    let inc = crate::alerts::list(sup, None, Some("open"));
    for i in inc["incidents"].as_array().into_iter().flatten() {
        let sev = i["severity"].as_str().unwrap_or("minor").to_string();
        let critical = sev == "critical";
        if !(i["needs_you"] == true || critical) {
            continue;
        }
        let id = i["id"].as_str().unwrap_or("").to_string();
        let since = i["started_ms"].as_i64().unwrap_or(now);
        out.push(Item {
            kind: "alert",
            repo: i["repo"].as_str().unwrap_or("").to_string(),
            since,
            critical,
            severity: Some(sev.clone()),
            json: json!({
                "id": format!("i:{id}"), "kind": "alert", "incident": id, "severity": sev, "critical": critical, "repo": i["repo"], "project": i["repo"].as_str().map(base),
                "since_ms": since, "waited_ms": now - since, "title": format!("{} alert", if critical { "Critical" } else { sev.as_str() }),
                "ask": i["title"], "command": Json::Null, "options": [], "href": "#/debug/failing",
            }),
        });
    }
    out.sort_by_key(|i| i.since);
    out
}

/// What happens to an item now: `notify` (now), `later` (when it is due),
/// `held` (quiet), `silent` (its project's level does not notify it).
fn verdict(it: &Item, level: &str, minutes: i64, quiet: bool, now: i64) -> (&'static str, Option<i64>, String) {
    if it.critical {
        return ("notify", None, "a critical alert interrupts at every level, quiet or not".into());
    }
    if quiet {
        return ("held", None, "quiet: kept for the digest".into());
    }
    match (level, it.kind) {
        ("all", _) => ("notify", None, "every question and alert".into()),
        ("alerts", "alert") => ("notify", None, "alerts only".into()),
        ("alerts", _) => ("silent", None, "this project notifies alerts only".into()),
        (_, "alert") => match it.severity.as_deref() {
            Some("major") => ("notify", None, "a major alert".into()),
            _ => ("silent", None, "a minor alert: in Needs you, no notification".into()),
        },
        _ => {
            let due = it.since + minutes * 60_000;
            if now >= due {
                ("notify", None, format!("blocked on you for more than {minutes} min"))
            } else {
                ("later", Some(due), format!("notifies if still waiting after {minutes} min"))
            }
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct PendingQ {
    repo: Option<String>,
}

/// `GET /api/notify/pending?repo=`.
fn pending(sup: &Supervisor, lv: &crate::levels::Levels, repo: Option<&str>) -> Result<Json, ApiError> {
    let now = now_ms();
    let s = settings(sup);
    let q = quiet_of(sup, now);
    let quiet = q["on"] == true;
    let repo = match repo.filter(|r| !r.is_empty()) {
        Some(r) => Some(crate::trail::repo_dir(Some(r))?.to_string_lossy().to_string()),
        None => None,
    };
    let mut buckets: BTreeMap<&str, Vec<Json>> = BTreeMap::new();
    for it in items(sup, lv, now).into_iter().filter(|i| repo.as_deref().is_none_or(|r| i.repo == r)) {
        let (level, minutes) = level_of(&s, &it.repo);
        let (v, due, why) = verdict(&it, &level, minutes, quiet, now);
        let mut j = it.json;
        j["level"] = json!(level);
        j["why"] = json!(why);
        if let Some(d) = due {
            j["due_ms"] = json!(d);
        }
        buckets.entry(v).or_default().push(j);
    }
    let take = |b: &mut BTreeMap<&str, Vec<Json>>, k: &str| b.remove(k).unwrap_or_default();
    let here = crate::digest::get_seen(sup, "here").and_then(|(v, _)| v.parse::<i64>().ok());
    let digest = digest_now(sup, lv, now, &q, here);
    Ok(json!({
        "now_ms": now,
        "quiet": q,
        "presence": { "here_ms": here, "here": here.is_some_and(|h| now - h < 90_000), "away_after_ms": AWAY_AFTER_MS },
        "notify": take(&mut buckets, "notify"),
        "later": take(&mut buckets, "later"),
        "held": take(&mut buckets, "held"),
        "silent": take(&mut buckets, "silent"),
        "digest": digest,
    }))
}

/// The digest, when there is one to show: quiet ended, or you were away
/// (no heartbeat for 5 min), and you have not seen it.
fn digest_now(sup: &Supervisor, lv: &crate::levels::Levels, now: i64, q: &Json, here: Option<i64>) -> Json {
    let seen = sup.mark("notify:digest_seen").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    // Quiet that ended since the digest was last seen.
    if let (Some(from), Some(until)) = (q["from_ms"].as_i64(), q["ended_ms"].as_i64()) {
        if until > seen && until > from {
            return build(sup, lv, "quiet", from.max(seen), now);
        }
    }
    // Away: the last heartbeat is old.
    if let Some(h) = here {
        if now - h >= AWAY_AFTER_MS && h >= seen {
            // One mark: "I'm stepping away", when the heartbeat stopped
            // soon after it (the page stays open a minute or two after the
            // tap), is when you left; else the last heartbeat.
            let mark = crate::digest::get_seen(sup, "away_mark").and_then(|(v, _)| v.parse::<i64>().ok()).filter(|m| *m <= h && h - m < AWAY_AFTER_MS);
            return build(sup, lv, "away", mark.unwrap_or(h), now);
        }
    }
    Json::Null
}

/// One summary of `[from, to]`: the prompts and who answered them, what
/// still waits, the alerts, the agents that finished.
pub fn build(sup: &Supervisor, lv: &crate::levels::Levels, reason: &str, from: i64, to: i64) -> Json {
    questions::expire_stale(sup, lv);
    let agents: BTreeMap<String, Agent> = sup.agent_rows().into_iter().map(|a| (a.id.clone(), a)).collect();
    let name = |id: &Option<String>| id.as_deref().and_then(|i| agents.get(i)).map(|a| a.name.clone()).unwrap_or_else(|| "an agent".into());
    let qs: Vec<Question> = questions::all(sup).into_iter().filter(|q| matches!(q.kind.as_str(), "permission" | "ask") && (q.created >= from && q.created <= to || q.state == "open")).collect();
    let asked: Vec<&Question> = qs.iter().filter(|q| q.created >= from).collect();
    let by = |p: &str| asked.iter().filter(|q| q.state == "answered" && q.answered_by.as_deref().unwrap_or("").starts_with(p)).count();
    let who: HashSet<String> = asked.iter().filter_map(|q| q.agent.clone()).collect();
    let mut waiting: Vec<&Question> = qs.iter().filter(|q| q.state == "open" && q.agent.as_deref().is_some_and(|a| sup.is_running(a))).collect();
    waiting.sort_by_key(|q| q.created);
    let waiting_j: Vec<Json> = waiting
        .iter()
        .map(|q| {
            let it = question_item(sup, q, q.agent.as_deref().and_then(|a| agents.get(a)), to);
            it.json
        })
        .collect();
    let inc = crate::alerts::list(sup, None, None);
    let alerts: Vec<Json> = inc["incidents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["started_ms"].as_i64().is_some_and(|t| t >= from && t <= to) || (i["state"] == "open" && i["updated_ms"].as_i64().is_some_and(|t| t >= from)))
        .map(|i| json!({ "id": i["id"], "title": i["title"], "severity": i["severity"], "state": i["state"], "critical": i["severity"] == "critical", "started_ms": i["started_ms"] }))
        .collect();
    let finished: Vec<Json> = agents
        .values()
        .filter(|a| a.ended.is_some_and(|t| t >= from && t <= to))
        .map(|a| json!({ "agent": a.id, "name": a.name, "exit_code": a.exit_code, "ended_ms": a.ended }))
        .collect();
    let critical = alerts.iter().filter(|a| a["critical"] == true).count();
    let mins = ((to - from) as f64 / 60_000.0).round() as i64;
    let mut parts: Vec<String> = Vec::new();
    let head = match reason {
        "quiet" => format!("While you were quiet ({})", dur(to - from)),
        _ => format!("While you were away ({})", dur(to - from)),
    };
    if asked.is_empty() {
        parts.push("no agent asked you anything".into());
    } else {
        let answered = by("you") + by("terminal") + by("rule:");
        let mut s = format!("{} prompt{} from {} agent{}", asked.len(), if asked.len() == 1 { "" } else { "s" }, who.len(), if who.len() == 1 { "" } else { "s" });
        if answered > 0 {
            let mut how = Vec::new();
            for (n, w) in [(by("you"), "by you here"), (by("terminal"), "in their terminals"), (by("rule:"), "by a rule")] {
                if n > 0 {
                    how.push(format!("{n} {w}"));
                }
            }
            s.push_str(&format!("; {answered} answered ({})", how.join(", ")));
        }
        parts.push(s);
    }
    if !waiting.is_empty() {
        let list: Vec<String> = waiting.iter().take(3).map(|q| format!("{} for {}", name(&q.agent), dur(to - q.created))).collect();
        parts.push(format!("{} still wait{}: {}", waiting.len(), if waiting.len() == 1 { "s" } else { "" }, list.join(", ")));
    }
    if !alerts.is_empty() {
        parts.push(format!("{} alert{}{}", alerts.len(), if alerts.len() == 1 { "" } else { "s" }, if critical > 0 { format!(" ({critical} critical, which interrupted you)") } else { String::new() }));
    }
    if !finished.is_empty() {
        parts.push(format!("{} agent{} finished", finished.len(), if finished.len() == 1 { "" } else { "s" }));
    }
    let sentence = format!("{head}: {}.", parts.join(". "));
    json!({
        "reason": reason,
        "since_ms": from,
        "until_ms": to,
        "since": iso(from),
        "minutes": mins,
        "sentence": sentence,
        "counts": {
            "prompts": asked.len(), "agents": who.len(), "by_you": by("you"), "by_terminal": by("terminal"), "by_rule": by("rule:"),
            "expired": asked.iter().filter(|q| q.state == "expired").count(), "waiting": waiting.len(), "alerts": alerts.len(), "critical": critical, "finished": finished.len(),
        },
        "waiting": waiting_j,
        "alerts": alerts,
        "finished": finished,
    })
}

fn dur(ms: i64) -> String {
    let m = (ms.max(0) + 30_000) / 60_000;
    if m < 1 {
        "under a minute".into()
    } else if m < 90 {
        format!("{m} min")
    } else {
        format!("{} h", (m + 30) / 60)
    }
}

#[derive(serde::Deserialize, Default)]
struct SeenB {
    until_ms: Option<i64>,
}

/// `POST /api/notify/seen {until_ms?}`: the digest was read.
fn seen(sup: &Supervisor, b: SeenB) -> Result<Json, ApiError> {
    let t = b.until_ms.filter(|t| *t > 0).unwrap_or_else(now_ms);
    sup.set_mark("notify:digest_seen", &t.to_string())?;
    Ok(json!({ "ok": true, "seen_ms": t }))
}

async fn settings_route(State(app): State<App>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || Ok(settings_json(&sup))).await?).into_response())
}

async fn put_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: PutB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || put(&sup, b)).await?).into_response())
}

async fn quiet_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: QuietB = if body.is_empty() { QuietB::default() } else { parse_body(&body)? };
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || quiet(&sup, b)).await?).into_response())
}

async fn pending_route(State(app): State<App>, Query(q): Query<PendingQ>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    Ok(AxJson(blocking(move || pending(&sup, &lv, q.repo.as_deref())).await?).into_response())
}

async fn seen_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: SeenB = if body.is_empty() { SeenB::default() } else { parse_body(&body)? };
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || seen(&sup, b)).await?).into_response())
}
