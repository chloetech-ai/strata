//! Repositories Workbench knows without an agent (sixth build): a person
//! points Workbench at a checkout to read it (search, Map, Trace, Library,
//! Health) before, or instead of, starting an agent on it. Each one opened
//! is a row of `repos`; the list a picker shows is those rows and the
//! repositories agents worked in, the most recent first. Opening one
//! indexes it for code search in the background.
//!
//! `GET /api/repos`, `POST /api/repos {path, trust?}`, `DELETE /api/repos?path=`,
//! `GET /api/repos/dirs?q=` (path completion: folders under what was
//! typed, git checkouts first).
//!
//! The journeys (JOURNEYS.md, "Start"): each repository has a short name,
//! its `project` (its folder's name, made unique: `shop`, `shop-2`), the
//! name routes carry (`#/p/shop`); its agents counted as the person reads
//! them (needs you, working, ready to review, finished), what is ready
//! (`ready`: the agents whose branch changed something main does not
//! have), the `wb/` branches not merged, its last activity, and `needs`,
//! its share of the one count (src/needs.rs). `trust: true` lets agents
//! work in the repository's own checkout without Claude Code's trust prompt
//! stopping them: Workbench answers it, as it does in its own worktrees.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::db::{get_ms, get_str, iso, now_ms, ts};
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{ApiError, Supervisor};

pub const TABLE: &str = "CREATE TABLE IF NOT EXISTS repos (id KEYWORD PRIMARY KEY, name KEYWORD, added TIMESTAMP, opened TIMESTAMP)";
/// Repositories whose own checkout the person trusts agents in.
pub const TRUST_TABLE: &str = "CREATE TABLE IF NOT EXISTS repo_trust (id KEYWORD PRIMARY KEY, trusted BOOL, at TIMESTAMP)";

/// Folders listed by one completion, and entries read to find them.
const MAX_DIRS: usize = 40;
const MAX_SCAN: usize = 5000;

pub fn routes() -> Router<App> {
    Router::new().route("/api/repos", get(list_route).post(add_route).delete(forget_route)).route("/api/repos/dirs", get(dirs_route))
}

/// `~` and `~/x` as the home folder.
pub fn expand_home(p: &str) -> PathBuf {
    if p == "~" || p.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(p.trim_start_matches('~').trim_start_matches('/'));
        }
    }
    PathBuf::from(p)
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.to_string_lossy().to_string())
}

/// The main checkout of what a person typed: an absolute path (or `~/…`)
/// of a git checkout, any worktree or folder of it.
pub fn resolve(path: &str) -> Result<PathBuf, ApiError> {
    let t = path.trim();
    if t.is_empty() {
        return Err(ApiError::bad("path is empty: the folder of a git checkout, such as /home/you/src/shop"));
    }
    let p = expand_home(t);
    if !p.is_absolute() {
        return Err(ApiError::bad(format!("{t} is not an absolute path: start it with / or ~/")));
    }
    if !p.is_dir() {
        return Err(ApiError::bad(format!("there is no folder {}", p.display())));
    }
    crate::git::checkout(&p).map(|c| c.main).map_err(|_| ApiError::bad(format!("{} is not in a git checkout: Workbench reads a repository's tracked files and history", p.display())))
}

/// Opens a repository: its row (added, or opened again), and its index
/// started; with `trust` set, whether Workbench answers Claude Code's trust
/// prompt for agents in its folders.
pub fn add_trusting(sup: &std::sync::Arc<Supervisor>, path: &str, trust: Option<bool>) -> Result<Json, ApiError> {
    let main = resolve(path)?;
    let id = main.to_string_lossy().to_string();
    let now = now_ms();
    let had = sup.db().query("SELECT added FROM repos WHERE id = :id LIMIT 1", &Params::new().set("id", Value::Str(id.clone()))).map_err(ApiError::internal)?;
    let added = had.first().and_then(|r| get_ms(&r[0])).unwrap_or(now);
    let p = Params::new().set("id", Value::Str(id.clone())).set("name", Value::Str(name_of(&main))).set("added", ts(added)).set("opened", ts(now));
    sup.db().query("INSERT OR REPLACE INTO repos (id, name, added, opened) VALUES (:id, :name, :added, :opened)", &p).map_err(ApiError::internal)?;
    if let Some(t) = trust {
        set_trusted(sup, &id, t)?;
    }
    let index = crate::search::open_repo(sup, &id);
    let mut r = one(&main, None, Some(now), &HashMap::new());
    r["index"] = index;
    r["project"] = json!(project_names(sup).get(&id).cloned().unwrap_or_else(|| name_of(&main)));
    r["trusted"] = json!(trusted(sup, &id));
    Ok(json!({ "repo": r, "created": had.is_empty() }))
}

/// Forgets a repository a person opened (its agents' still list it).
pub fn forget(sup: &Supervisor, path: &str) -> Result<Json, ApiError> {
    let p = expand_home(path.trim());
    let id = crate::git::checkout(&p).map(|c| c.main).unwrap_or(p).to_string_lossy().to_string();
    sup.db().query("DELETE FROM repos WHERE id = :id", &Params::new().set("id", Value::Str(id.clone()))).map_err(ApiError::internal)?;
    Ok(json!({ "ok": true, "repo": id }))
}

/// One repository as the API answers it.
fn one(main: &Path, added: Option<i64>, opened: Option<i64>, agents: &HashMap<String, (usize, usize, i64)>) -> Json {
    let id = main.to_string_lossy().to_string();
    let (running, total, last_agent) = agents.get(&id).copied().unwrap_or((0, 0, 0));
    let last = opened.unwrap_or(0).max(last_agent);
    json!({
        "path": id,
        "name": name_of(main),
        "exists": main.is_dir(),
        "branch": if main.is_dir() { crate::git::current_branch(main) } else { None },
        "opened": opened.map(iso),
        "added": added.map(iso),
        "agents_running": running,
        "agents": total,
        "last_ms": last,
    })
}

/// Whether the person trusts agents in this repository's own checkout.
pub fn trusted(sup: &Supervisor, repo: &str) -> bool {
    let rows = sup.db().query("SELECT trusted FROM repo_trust WHERE id = :id LIMIT 1", &Params::new().set("id", Value::Str(repo.to_string()))).unwrap_or_default();
    rows.first().is_some_and(|r| matches!(r[0], Value::Bool(true)))
}

fn set_trusted(sup: &Supervisor, repo: &str, on: bool) -> Result<(), ApiError> {
    let p = Params::new().set("id", Value::Str(repo.to_string())).set("trusted", Value::Bool(on)).set("at", ts(now_ms()));
    sup.db().query("INSERT OR REPLACE INTO repo_trust (id, trusted, at) VALUES (:id, :trusted, :at)", &p).map_err(ApiError::internal)?;
    Ok(())
}

/// Each repository's short name (JOURNEYS.md, "URL routes"): its folder's
/// name, made unique in the order Workbench met them (`shop`, `shop-2`).
pub fn project_names(sup: &Supervisor) -> HashMap<String, String> {
    let mut first: HashMap<String, i64> = HashMap::new();
    for r in sup.db().query("SELECT id, added FROM repos LIMIT 10000", &Params::new()).unwrap_or_default() {
        if let Some(id) = get_str(&r[0]) {
            let t = get_ms(&r[1]).unwrap_or(i64::MAX);
            first.entry(id).and_modify(|x| *x = (*x).min(t)).or_insert(t);
        }
    }
    for a in sup.agent_rows() {
        if !a.repo.is_empty() {
            first.entry(a.repo.clone()).and_modify(|x| *x = (*x).min(a.created)).or_insert(a.created);
        }
    }
    let mut order: Vec<(i64, String)> = first.into_iter().map(|(id, t)| (t, id)).collect();
    order.sort();
    let mut taken: HashSet<String> = HashSet::new();
    let mut out = HashMap::new();
    for (_, id) in order {
        let base: String = name_of(Path::new(&id)).to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
        let base = if base.trim_matches(|c| c == '-' || c == '.').is_empty() { "project".to_string() } else { base };
        let mut n = base.clone();
        let mut i = 2;
        while taken.contains(&n) {
            n = format!("{base}-{i}");
            i += 1;
        }
        taken.insert(n.clone());
        out.insert(id, n);
    }
    out
}

/// What a worktree changed, cached a little: `git` per worktree is cheap,
/// but Start asks for every project at once.
fn stat_cached(cwd: &str, main: &str, stamp: i64) -> Option<(usize, i64, i64)> {
    use std::sync::{Mutex, OnceLock};
    type Cache = HashMap<String, (std::time::Instant, i64, Option<(usize, i64, i64)>)>;
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    let c = C.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((at, st, v)) = c.lock().unwrap().get(cwd) {
        if *st == stamp && at.elapsed() < std::time::Duration::from_secs(8) {
            return *v;
        }
    }
    let v = crate::git::change_stat(Path::new(cwd), Path::new(main));
    c.lock().unwrap().insert(cwd.to_string(), (std::time::Instant::now(), stamp, v));
    v
}

/// The journeys' group of an agent (JOURNEYS.md, "Global elements"): needs
/// you, working, ready (its turn is over, or it finished, and its branch
/// changed something main does not have) or finished.
pub fn group_of(state: &str, running: bool, changed: bool) -> &'static str {
    match (running, state) {
        (true, "waiting") => "needs",
        (true, "idle") if changed => "ready",
        (true, _) => "working",
        (false, _) if changed => "ready",
        (false, _) => "finished",
    }
}

/// [`list`] with each project's short name, its agents counted by group,
/// what is ready, its unmerged branches, its last activity and its share
/// of what needs you; the most active first.
pub fn list_full(sup: &std::sync::Arc<Supervisor>, lv: &std::sync::Arc<crate::levels::Levels>) -> Json {
    let names = project_names(sup);
    let needs = crate::needs::needs(sup, lv, None);
    let by_repo = needs["by_repo"].clone();
    let mut rows = list(sup);
    let agents = sup.agent_rows();
    let now = now_ms();
    for r in rows.iter_mut() {
        let path = r["path"].as_str().unwrap_or("").to_string();
        let mut counts: HashMap<&str, i64> = HashMap::new();
        let mut ready: Vec<Json> = Vec::new();
        let mut groups = serde_json::Map::new();
        let mut merged_ids = serde_json::Map::new();
        let mut last = r["last_ms"].as_i64().unwrap_or(0);
        let mine: Vec<&crate::db::Agent> = agents.iter().filter(|a| a.repo == path).collect();
        for a in &mine {
            let running = sup.is_running(&a.id);
            // An exited agent counts once: the newest of its name.
            if !running && mine.iter().any(|b| b.name == a.name && b.created > a.created) {
                continue;
            }
            last = last.max(a.state_since).max(a.narration_ts.unwrap_or(0));
            // A worktree of its own, still there (and, exited, of the last week).
            let own = a.cwd != a.repo && Path::new(&a.cwd).is_dir() && (running || now - a.ended.unwrap_or(a.state_since) < 7 * 86_400_000);
            let stat = if own && (a.state == "idle" || !running) { stat_cached(&a.cwd, &path, a.state_since) } else { None };
            let changed = stat.is_some_and(|(f, _, _)| f > 0);
            // Merged by Review with nothing new since: done, not counted as at work.
            let merged = sup.mark(&format!("review-merged:{}", a.id)).and_then(|s| serde_json::from_str::<Json>(&s).ok()).filter(|m| m["undone"] != true);
            let g = if merged.is_some() && !changed { "finished" } else { group_of(&a.state, running, changed) };
            if let Some(m) = merged.filter(|_| !changed) {
                merged_ids.insert(a.id.clone(), json!({ "commit": m["commit"], "onto": m["onto"], "ts": m["ts"] }));
            }
            *counts.entry(g).or_default() += 1;
            if running {
                *counts.entry("running").or_default() += 1;
            }
            groups.insert(a.id.clone(), json!(g));
            if let (true, Some((files, add, del))) = (changed, stat) {
                ready.push(json!({ "id": a.id, "name": a.name, "branch": a.branch, "running": running, "files": files, "added": add, "removed": del, "since_ms": a.state_since }));
            }
        }
        ready.sort_by_key(|x| std::cmp::Reverse(x["since_ms"].as_i64().unwrap_or(0)));
        let main = Path::new(&path);
        let unmerged = if main.is_dir() { crate::git::unmerged_branches(main, "wb/") } else { Vec::new() };
        let c = |k: &str| counts.get(k).copied().unwrap_or(0);
        r["project"] = json!(names.get(&path).cloned().unwrap_or_else(|| name_of(main)));
        r["trusted"] = json!(trusted(sup, &path));
        r["counts"] = json!({ "needs": c("needs"), "working": c("working"), "ready": c("ready"), "finished": c("finished"), "running": c("running") });
        r["groups"] = Json::Object(groups);
        r["merged"] = Json::Object(merged_ids);
        r["ready"] = Json::Array(ready);
        r["unmerged"] = json!(unmerged.into_iter().take(20).collect::<Vec<_>>());
        r["needs"] = json!(by_repo[&path].as_u64().unwrap_or(0));
        r["last_activity_ms"] = json!(last);
        r["last_activity"] = if last > 0 { json!(iso(last)) } else { Json::Null };
    }
    // Most active first: a project that needs you, then by last activity.
    rows.sort_by(|a, b| {
        let k = |x: &Json| (x["needs"].as_u64().unwrap_or(0) > 0, x["last_activity_ms"].as_i64().unwrap_or(0));
        k(b).cmp(&k(a)).then(a["path"].as_str().cmp(&b["path"].as_str()))
    });
    json!({ "repos": rows, "needs": needs["count"] })
}

/// The repositories a person opened and the ones agents worked in, the
/// most recently used first.
pub fn list(sup: &Supervisor) -> Vec<Json> {
    let mut agents: HashMap<String, (usize, usize, i64)> = HashMap::new();
    for a in sup.agent_rows() {
        let e = agents.entry(a.repo.clone()).or_default();
        e.0 += usize::from(sup.is_running(&a.id));
        e.1 += 1;
        e.2 = e.2.max(a.created).max(a.ended.unwrap_or(0));
    }
    let mut rows: HashMap<String, (Option<i64>, Option<i64>)> = HashMap::new();
    for r in sup.db().query("SELECT id, added, opened FROM repos LIMIT 10000", &Params::new()).unwrap_or_default() {
        if let Some(id) = get_str(&r[0]) {
            rows.insert(id, (get_ms(&r[1]), get_ms(&r[2])));
        }
    }
    let mut out: Vec<Json> = Vec::new();
    for id in rows.keys().chain(agents.keys().filter(|k| !rows.contains_key(*k))) {
        if id.is_empty() {
            continue;
        }
        let (added, opened) = rows.get(id).copied().unwrap_or((None, None));
        out.push(one(Path::new(id), added, opened, &agents));
    }
    out.sort_by(|a, b| b["last_ms"].as_i64().cmp(&a["last_ms"].as_i64()).then(a["path"].as_str().cmp(&b["path"].as_str())));
    out
}

/// Path completion: the folders under what was typed (`/home/me/sr` lists
/// `/home/me/src…`; `/home/me/src/` lists its folders), git checkouts
/// first, then by name; hidden folders only when the typed part starts
/// with a dot. An empty `q` lists the home folder.
pub fn dirs(q: &str) -> Json {
    let q = q.trim();
    let typed = if q.is_empty() { "~/".to_string() } else { q.to_string() };
    let full = expand_home(&typed);
    if !full.is_absolute() {
        return json!({ "q": q, "dirs": [], "missing": ["a repository is an absolute path: start with / or ~/"] });
    }
    let s = full.to_string_lossy().to_string();
    let (parent, prefix) = if s.ends_with('/') { (full.clone(), String::new()) } else { (full.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/")), name_of(&full)) };
    let Ok(rd) = std::fs::read_dir(&parent) else {
        return json!({ "q": q, "parent": parent, "dirs": [], "missing": [format!("cannot read {}", parent.display())] });
    };
    let lower = prefix.to_lowercase();
    let mut found: Vec<(bool, String, PathBuf)> = Vec::new();
    for e in rd.flatten().take(MAX_SCAN) {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && !prefix.starts_with('.') {
            continue;
        }
        if !name.to_lowercase().starts_with(&lower) {
            continue;
        }
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let git = p.join(".git").exists();
        found.push((git, name, p));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    let more = found.len() > MAX_DIRS;
    found.truncate(MAX_DIRS);
    let here_git = parent.join(".git").exists();
    json!({
        "q": q,
        "parent": parent,
        "parent_git": here_git,
        "dirs": found.into_iter().map(|(git, name, p)| json!({ "path": p, "name": name, "git": git })).collect::<Vec<_>>(),
        "more": more,
        "missing": [],
    })
}

async fn list_route(State(app): State<App>) -> ApiResult {
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    let v = blocking(move || Ok(list_full(&sup, &lv))).await?;
    Ok(AxJson(v).into_response())
}

#[derive(Deserialize)]
struct AddB {
    path: Option<String>,
    repo: Option<String>,
    /// Agents may work in its own checkout: Workbench answers Claude
    /// Code's trust prompt there, as in its own worktrees.
    trust: Option<bool>,
    /// A new project (JOURNEYS.md, "A. Start a new project"): make the
    /// folder, `git init` it and make a first commit, when it is not a
    /// repository yet.
    init: Option<bool>,
}

/// Makes DIR a repository with a first, empty commit, when it is not one
/// yet (`chloedb-workbench new`, and `POST /api/repos` with `init`).
pub fn init_project(dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let dir = dir.canonicalize().map_err(|e| e.to_string())?;
    let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&dir).args(args).env("GIT_TERMINAL_PROMPT", "0").output();
    let ok = |args: &[&str]| git(args).map(|o| o.status.success()).unwrap_or(false);
    if !ok(&["rev-parse", "--is-inside-work-tree"]) {
        let o = git(&["init", "-q"]).map_err(|e| format!("cannot run git: {e}"))?;
        if !o.status.success() {
            return Err(format!("git init failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
        }
    }
    if !ok(&["rev-parse", "--verify", "-q", "HEAD"]) {
        // The user's own identity when git has one, a plain one when not.
        let mut args: Vec<&str> = Vec::new();
        if !ok(&["config", "user.email"]) {
            args.extend(["-c", "user.name=Workbench", "-c", "user.email=workbench@localhost"]);
        }
        args.extend(["commit", "-q", "--allow-empty", "-m", "Start the project"]);
        let o = git(&args).map_err(|e| format!("cannot run git: {e}"))?;
        if !o.status.success() {
            return Err(format!("the first commit failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
        }
    }
    Ok(dir)
}

async fn add_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: AddB = parse_body(&body)?;
    let sup = app.sup.clone();
    let path = b.path.or(b.repo).unwrap_or_default();
    let init = b.init.unwrap_or(false);
    let v = blocking(move || {
        let path = if init {
            let p = std::path::PathBuf::from(path.trim());
            if !p.is_absolute() {
                return Err(ApiError::bad("a new project's folder is an absolute path"));
            }
            init_project(&p).map_err(ApiError::bad)?.to_string_lossy().to_string()
        } else {
            path
        };
        add_trusting(&sup, &path, b.trust)
    })
    .await?;
    let status = if v["created"] == true { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, AxJson(v)).into_response())
}

#[derive(Deserialize)]
struct PathQ {
    path: Option<String>,
    q: Option<String>,
}

async fn forget_route(State(app): State<App>, Query(q): Query<PathQ>) -> ApiResult {
    let sup = app.sup.clone();
    let path = q.path.unwrap_or_default();
    Ok(AxJson(blocking(move || forget(&sup, &path)).await?).into_response())
}

async fn dirs_route(Query(q): Query<PathQ>) -> ApiResult {
    let typed = q.q.unwrap_or_default();
    Ok(AxJson(blocking(move || Ok(dirs(&typed))).await?).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_folders_with_git_checkouts_first() {
        let d = tempfile::tempdir().unwrap();
        for n in ["shop", "shelf", "Scripts", ".secret", "zed"] {
            std::fs::create_dir(d.path().join(n)).unwrap();
        }
        std::fs::create_dir(d.path().join("shelf/.git")).unwrap();
        std::fs::write(d.path().join("show.txt"), "not a folder").unwrap();
        let base = d.path().to_string_lossy().to_string();
        let v = dirs(&format!("{base}/s"));
        let names: Vec<&str> = v["dirs"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["shelf", "Scripts", "shop"], "{v}");
        assert_eq!(v["dirs"][0]["git"], true);
        let v = dirs(&format!("{base}/"));
        assert_eq!(v["dirs"].as_array().unwrap().len(), 4, "hidden ones only when asked: {v}");
        let v = dirs(&format!("{base}/.s"));
        assert_eq!(v["dirs"][0]["name"], ".secret");
        assert!(dirs("relative/path")["missing"][0].as_str().unwrap().contains("absolute"));
        assert!(resolve("relative").unwrap_err().message.contains("absolute"));
        assert!(resolve(&format!("{base}/zed")).unwrap_err().message.contains("not in a git checkout"));
        assert!(resolve(&format!("{base}/nope")).unwrap_err().message.contains("no folder"));
    }
}
