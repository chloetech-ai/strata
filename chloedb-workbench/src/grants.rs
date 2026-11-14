//! What agents were allowed to do without asking (JOURNEYS.md, "More ›
//! Rules", and the grant sheet): the `permissions.allow` rules Claude Code
//! wrote into a Workbench worktree's `.claude/settings.local.json` when
//! someone picked "Yes, and don't ask again for …", with Revoke, and the
//! commands such a rule would have let through.
//!
//! - `GET /api/grants?repo=`: `{grants: [{agent, name, cwd, file, rule}]}`,
//!   one row per rule of each agent of that repository that has its own
//!   worktree (the repository's own checkout is the user's: never read).
//! - `POST /api/grants/revoke {agent, rule}`: removes that rule from that
//!   agent's file and keeps everything else in it as it was.
//! - `GET /api/grants/preview?repo=&prefix=`: up to 8 commands agents of
//!   the repository asked to run that start with `prefix`, newest first.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::supervisor::ApiError;
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::Supervisor;

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/grants", get(list_route))
        .route("/api/grants/revoke", post(revoke_route))
        .route("/api/grants/preview", get(preview_route))
}

/// The settings file Claude Code writes a lasting grant into, in a session's folder.
fn settings_file(cwd: &Path) -> PathBuf {
    cwd.join(".claude").join("settings.local.json")
}

fn read_json(p: &Path) -> Option<Json> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// The agents of a repository that run in their own worktree, newest first.
fn worktree_agents(sup: &Supervisor, repo: &str) -> Vec<crate::db::Agent> {
    let mut rows: Vec<_> = sup.agent_rows().into_iter().filter(|a| a.repo == repo && !a.cwd.is_empty() && a.cwd != a.repo).collect();
    rows.sort_by_key(|a| -a.created);
    rows
}

pub fn list(sup: &Supervisor, repo: &str) -> Json {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for a in worktree_agents(sup, repo) {
        if !seen.insert(a.cwd.clone()) {
            continue;
        }
        let file = settings_file(Path::new(&a.cwd));
        let Some(s) = read_json(&file) else { continue };
        for r in s["permissions"]["allow"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
            out.push(json!({ "agent": a.id, "name": a.name, "cwd": a.cwd, "file": file.to_string_lossy(), "rule": r }));
        }
    }
    json!({ "grants": out })
}

pub fn revoke(sup: &Supervisor, agent: &str, rule: &str) -> Result<Json, ApiError> {
    let a = sup.agent_row(agent).ok_or_else(|| ApiError::not_found(format!("no agent {agent}")))?;
    if a.cwd.is_empty() || a.cwd == a.repo {
        return Err(ApiError::bad("that agent runs in the repository's own checkout: Workbench leaves its settings to you"));
    }
    let file = settings_file(Path::new(&a.cwd));
    let mut s = read_json(&file).ok_or_else(|| ApiError::not_found(format!("{} has no settings", file.display())))?;
    let Some(allow) = s["permissions"]["allow"].as_array_mut() else {
        return Err(ApiError::not_found("no such rule"));
    };
    let before = allow.len();
    allow.retain(|v| v.as_str() != Some(rule));
    if allow.len() == before {
        return Err(ApiError::not_found("no such rule"));
    }
    let text = serde_json::to_string_pretty(&s).map_err(|e| ApiError::internal(e.to_string()))?;
    std::fs::write(&file, text + "\n").map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(json!({ "revoked": rule, "agent": agent, "file": file.to_string_lossy() }))
}

/// Commands agents of the repository asked to run that start with `prefix`.
pub fn preview(sup: &Arc<Supervisor>, lv: &Arc<crate::levels::Levels>, repo: &str, prefix: &str) -> Json {
    let prefix = prefix.trim();
    let mut cmds: Vec<(i64, String, String)> = Vec::new();
    if !prefix.is_empty() {
        let qs = crate::questions::list(sup, lv, Some(repo), None, None).ok().and_then(|v| v["questions"].as_array().cloned()).unwrap_or_default();
        for q in qs {
            let Some(c) = q["detail"]["command"].as_str() else { continue };
            let c = c.trim();
            if c.starts_with(prefix) && !cmds.iter().any(|(_, x, _)| x == c) {
                cmds.push((q["created_ms"].as_i64().unwrap_or(0), c.to_string(), q["agent_name"].as_str().unwrap_or("").to_string()));
            }
        }
    }
    cmds.sort_by_key(|(t, _, _)| -*t);
    let list: Vec<Json> = cmds.into_iter().take(8).map(|(t, c, n)| json!({ "command": c, "agent": n, "at_ms": t, "risky": risky(&c) })).collect();
    json!({ "prefix": prefix, "commands": list })
}

/// Whether a command changes something beyond reading: a migration, a
/// delete, a write outside the worktree, a push, an install.
pub fn risky(c: &str) -> bool {
    let l = c.to_lowercase();
    ["migrate", "rm -", "drop ", "delete", "truncate", "push", "install", "curl ", "wget ", "> /", "~/", "sudo", "chmod", "--force", "reset --hard"].iter().any(|w| l.contains(w))
}

#[derive(Deserialize)]
struct RepoQ {
    repo: Option<String>,
    prefix: Option<String>,
}

fn repo_of(q: Option<&str>) -> Result<String, ApiError> {
    let r = q.filter(|r| !r.is_empty()).ok_or_else(|| ApiError::bad("repo is required"))?;
    Ok(crate::trail::repo_dir(Some(r))?.to_string_lossy().to_string())
}

async fn list_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || Ok(list(&sup, &repo_of(q.repo.as_deref())?))).await?;
    Ok(AxJson(v).into_response())
}

#[derive(Deserialize)]
struct RevokeB {
    agent: String,
    rule: String,
}

async fn revoke_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: RevokeB = parse_body(&body)?;
    let sup = app.sup.clone();
    let v = blocking(move || revoke(&sup, &b.agent, &b.rule)).await?;
    Ok(AxJson(v).into_response())
}

async fn preview_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    let v = blocking(move || Ok(preview(&sup, &lv, &repo_of(q.repo.as_deref())?, q.prefix.as_deref().unwrap_or("")))).await?;
    Ok(AxJson(v).into_response())
}

#[cfg(test)]
mod tests {
    use super::risky;

    #[test]
    fn risky_commands() {
        assert!(risky("python3 -m orders.migrate --db dev.sqlite3"));
        assert!(risky("git push origin main"));
        assert!(!risky("python3 -m pytest -q"));
    }
}
