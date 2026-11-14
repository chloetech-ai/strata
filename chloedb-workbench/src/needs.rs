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
