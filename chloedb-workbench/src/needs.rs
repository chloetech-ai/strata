//! What needs you (JOURNEYS.md, "The rule" 4 and "Global elements"): the
//! one count the top bar's button, Start, Continue and the phone's tab badge
//! all show, and the cards of its drawer. It counts an agent blocked on you
//! (an open question: a permission prompt, Claude Code's trust prompt, an
//! `ask_human`; or an agent waiting with no question read yet) and an alert
//! that asks you. An agent that finished its turn is ready, not here.
//!
//! `GET /api/needs[?repo=]`: `{count, items: [...], by_repo: {path: n}}`,
//! the agents' items oldest first, then the alerts', oldest first.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json as AxJson, Router};
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::levels::Levels;
use crate::server::{blocking, ApiResult, App};
use crate::supervisor::Supervisor;

pub fn routes() -> Router<App> {
    Router::new().route("/api/needs", get(needs_route))
}

/// The items, oldest first within agents and within alerts; `repo` (a main
/// checkout's path) keeps that repository's.
pub fn needs(sup: &Arc<Supervisor>, lv: &Arc<Levels>, repo: Option<&str>) -> Json {
    let keep = |r: &str| repo.is_none_or(|want| want == r);
    let mut agent_items: Vec<(i64, Json)> = Vec::new();
    let mut asked: HashSet<String> = HashSet::new();
    let qs = crate::questions::list(sup, lv, None, Some("open"), None).ok().map(|v| v["questions"].as_array().cloned().unwrap_or_default()).unwrap_or_default();
    for q in qs {
        let Some(id) = q["agent"].as_str().map(str::to_string) else { continue };
        if !sup.is_running(&id) {
            continue;
        }
        let Some(a) = sup.agent_json_of(&id) else { continue };
        let r = a["repo"].as_str().unwrap_or("").to_string();
        if !keep(&r) {
            continue;
        }
        asked.insert(id.clone());
        let trust = q["detail"]["menu"]["kind"] == "trust";
        let since = q["created_ms"].as_i64().unwrap_or(0);
        agent_items.push((since, json!({ "kind": if trust { "trust" } else { "question" }, "id": q["id"], "repo": r, "since_ms": since, "agent": a, "question": q })));
    }
    // Waiting with no question read yet (a Notification before the screen
    // watcher reads the prompt): it still needs you.
    for row in sup.agent_rows() {
        if row.state != "waiting" || asked.contains(&row.id) || !sup.is_running(&row.id) || !keep(&row.repo) {
            continue;
        }
        let Some(a) = sup.agent_json_of(&row.id) else { continue };
        agent_items.push((row.state_since, json!({ "kind": "waiting", "id": format!("w-{}", row.id), "repo": row.repo, "since_ms": row.state_since, "agent": a, "text": row.waiting_for })));
    }
    // Idle at its prompt after a turn that ended by asking you something
    // ("Should I also update the README?"): that needs you too (Priya: such
    // an agent sat under Ready while it waited for her).
    for row in sup.agent_rows() {
        if row.state != "idle" || asked.contains(&row.id) || !sup.is_running(&row.id) || !keep(&row.repo) {
            continue;
        }
        let Some(q) = asked_at_end(sup, &row.id, row.state_since) else { continue };
        let Some(a) = sup.agent_json_of(&row.id) else { continue };
        agent_items.push((row.state_since, json!({ "kind": "waiting", "asked": true, "id": format!("q-{}", row.id), "repo": row.repo, "since_ms": row.state_since, "agent": a, "text": q })));
    }
    agent_items.sort_by_key(|(t, _)| *t);
    let mut alert_items: Vec<(i64, Json)> = Vec::new();
    let inc = crate::alerts::list(sup, None, Some("open"));
    for i in inc["incidents"].as_array().into_iter().flatten() {
        if i["needs_you"] != true {
            continue;
        }
        let r = i["repo"].as_str().unwrap_or("").to_string();
        if !keep(&r) {
            continue;
        }
        let since = i["updated_ms"].as_i64().or_else(|| i["started_ms"].as_i64()).unwrap_or(0);
        alert_items.push((since, json!({ "kind": "alert", "id": i["id"], "repo": r, "since_ms": since, "incident": i })));
    }
    alert_items.sort_by_key(|(t, _)| *t);
    let items: Vec<Json> = agent_items.into_iter().chain(alert_items).map(|(_, v)| v).collect();
    let mut by_repo: HashMap<String, usize> = HashMap::new();
    for it in &items {
        *by_repo.entry(it["repo"].as_str().unwrap_or("").to_string()).or_default() += 1;
    }
    json!({ "count": items.len(), "items": items, "by_repo": by_repo })
}

/// The question an agent's last turn ended with, when its last message's
/// last sentence asks one and nothing was said to it since (`idle_since`:
/// when it went idle).
fn asked_at_end(sup: &Supervisor, agent: &str, idle_since: i64) -> Option<String> {
    let p = chloedb::query::Params::new().set("agent", chloedb::Value::Str(agent.to_string()));
    let rows = sup.db().query("SELECT ts, kind, data FROM events WHERE agent = :agent AND (kind = 'Stop' OR kind = 'UserPromptSubmit' OR kind = 'steer') ORDER BY seq DESC LIMIT 1", &p).ok()?;
    let r = rows.first()?;
    if crate::db::get_str(&r[1]).as_deref() != Some("Stop") || crate::db::get_ms(&r[0]).unwrap_or(0) + 600_000 < idle_since {
        return None;
    }
    let d: Json = serde_json::from_str(&crate::db::get_str(&r[2])?).ok()?;
    last_question(d["last_assistant_message"].as_str()?)
}

/// The last sentence of a message, when it is a question to you.
pub fn last_question(msg: &str) -> Option<String> {
    let last = msg.trim().lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("```")).last()?;
    let last = last.trim_start_matches(['-', '*', '>', ' ']).trim_end_matches(['*', '_', ' ']).trim();
    if !last.ends_with('?') {
        return None;
    }
    // The sentence the question mark ends.
    let start = last[..last.len() - 1].rfind(['.', '!', '?']).map_or(0, |i| i + 1);
    let q = last[start..].trim();
    (q.split_whitespace().count() >= 3).then(|| crate::supervisor::one_line(q, 200))
}

#[derive(Deserialize)]
struct NeedsQ {
    repo: Option<String>,
}

async fn needs_route(State(app): State<App>, Query(q): Query<NeedsQ>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    let v = blocking(move || {
        let repo = match q.repo.as_deref().filter(|r| !r.is_empty()) {
            Some(r) => Some(crate::trail::repo_dir(Some(r))?.to_string_lossy().to_string()),
            None => None,
        };
        Ok(needs(&sup, &lv, repo.as_deref()))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_that_ends_with_a_question_is_found() {
        assert_eq!(last_question("Done: the totals round half up.\n\nShould I also update the README?").as_deref(), Some("Should I also update the README?"));
        assert_eq!(last_question("I fixed it. Want me to push it to main?").as_deref(), Some("Want me to push it to main?"));
        assert_eq!(last_question("All tests pass."), None);
        assert_eq!(last_question("Why? Because the tax rounds first. Done."), None);
        assert_eq!(last_question("Ok?"), None);
    }
}
