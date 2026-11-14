//! Review and ship (JOURNEYS.md, "C. Review and ship"): what is ready to
//! merge in a repository, the checks run on each branch (its tests by name,
//! whether it merges cleanly into the landing branch and with each other
//! ready branch, by `git merge-tree`), the order to merge them in, and for
//! one branch the blast radius as a verdict, three rings (functions,
//! callers, public names) and only the exceptions, then its diff grouped
//! into pieces of work, riskiest first, with mechanical changes folded.
//! Merge does it: into the landing branch, refusing what is not committed
//! or does not merge cleanly.
//!
//! Built on what Ship already computes: `blast::blast` (cached per facts)
//! and `capsule::part` (the title, this branch's decisions, its tests).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::blast::{self, git_out, plural};
use crate::db::Agent;
use crate::git;
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{ApiError, Supervisor};
use crate::trail;

/// Names a ring shows (the rest are counted).
const RING_NAMES: usize = 8;
/// Pieces of work shown (the rest fold into one line).
const MAX_PIECES: usize = 8;
/// Lines of the hunk that matters.
const HUNK_LINES: usize = 14;
/// Lines of a piece's hunks sent in all ("All hunks of this piece").
const PIECE_LINES: usize = 400;

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/review", get(list_route))
        .route("/api/review/merge", post(merge_route))
        .route("/api/review/undo", post(undo_route))
        .route("/api/review/confirm", post(confirm_route))
        .route("/api/review/pr", post(pr_route))
        .route("/api/agents/:id/review", get(one_route))
}

#[derive(Deserialize)]
struct ConfirmB {
    record: String,
    /// `true`: still right; `false` takes the confirmation back.
    ok: Option<bool>,
}

/// `POST /api/review/confirm {record, ok}`: a decision a change replaces
/// another with is confirmed as still right (Merge waits for it).
async fn confirm_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: ConfirmB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || {
        if b.record.trim().is_empty() || b.record.len() > 100 {
            return Err(ApiError::bad("record is the id of a decision"));
        }
        let ok = b.ok.unwrap_or(true);
        sup.set_mark(&confirm_key(&b.record), if ok { "1" } else { "" })?;
        Ok(json!({ "ok": true, "record": b.record, "confirmed": ok }))
    })
    .await?)
    .into_response())
}

fn confirm_key(record: &str) -> String {
    format!("review-ok:{record}")
}

fn confirmed(sup: &Supervisor, record: &str) -> bool {
    sup.mark(&confirm_key(record)).is_some_and(|v| v == "1")
}

/// Whether a branch may merge, and what the page's primary action is:
/// what is not committed or conflicts blocks it; a decision it replaces
/// another with waits for "Still right"; failing tests or a high risk make
/// Send back the primary, and Merge then wants the branch's name typed.
fn gate(branch: &str, tests: &Json, decisions: &Json, level: &str, dirty: &[String], clean: Option<bool>) -> Json {
    let mut blocked: Vec<String> = Vec::new();
    if !dirty.is_empty() {
        blocked.push(format!("{} not committed", plural(dirty.len(), "file")));
    }
    if clean == Some(false) {
        blocked.push("it conflicts with the branch it lands on".into());
    }
    let unconfirmed = decisions["matter"].as_array().map_or(0, |v| v.iter().filter(|d| d["why"] == "replaces" && d["confirmed"] != true).count());
    let failing = tests["failed"].as_u64().unwrap_or(0) as usize;
    let mut warn: Vec<String> = Vec::new();
    if failing > 0 {
        warn.push(format!("{} on {branch}", fails_words(failing)));
    }
    if level == "high" {
        warn.push("its risk is high".into());
    }
    let can = blocked.is_empty() && unconfirmed == 0;
    let label = if !blocked.is_empty() {
        format!("Cannot merge: {}", blocked.join(", "))
    } else if unconfirmed > 0 {
        format!("Confirm {} first", plural(unconfirmed, "decision"))
    } else if !warn.is_empty() {
        format!("Merge anyway: {}", warn.join(", "))
    } else {
        "Merge".into()
    };
    json!({ "can_merge": can, "blocked": blocked, "unconfirmed": unconfirmed, "anyway": !warn.is_empty(), "warn": warn, "primary": if can && warn.is_empty() { "merge" } else { "send_back" }, "label": label })
}

/// "1 test fails", "2 tests fail".
pub(crate) fn fails_words(n: usize) -> String {
    if n == 1 {
        "1 test fails".into()
    } else {
        format!("{n} tests fail")
    }
}

#[derive(Deserialize)]
struct RepoQ {
    repo: Option<String>,
}

#[derive(Deserialize)]
struct PrB {
    agents: Vec<String>,
}

/// `POST /api/review/pr {agents}`: one pull request description for several branches.
async fn pr_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: PrB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || crate::capsule::pr_many(&sup, &b.agents)).await?).into_response())
}

async fn list_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || list(&sup, q.repo.as_deref().unwrap_or(""))).await?).into_response())
}

async fn one_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || one(&sup, &id)).await?).into_response())
}

#[derive(Deserialize)]
struct MergeB {
    agents: Vec<String>,
    /// "Merge anyway": the names of the branches typed, for those whose
    /// tests fail or whose risk is high.
    #[serde(default)]
    anyway: Vec<String>,
}

async fn merge_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: MergeB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || merge(&sup, &b.agents, &b.anyway)).await?).into_response())
}

#[derive(Deserialize)]
struct UndoB {
    agent: String,
}

async fn undo_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: UndoB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || undo(&sup, &b.agent)).await?).into_response())
}

// ---------------------------------------------------------------- helpers

/// A repository named by its path or by its folder's name (`shop`), as the
/// routes `#/p/<project>` name it.
pub(crate) fn repo_of(sup: &Supervisor, repo: &str) -> Result<PathBuf, ApiError> {
    let repo = repo.trim();
    if repo.is_empty() {
        return Err(ApiError::bad("repo is required: its path or its folder's name"));
    }
    if repo.starts_with('/') {
        let p = PathBuf::from(repo);
        return Ok(p.canonicalize().unwrap_or(p));
    }
    let mut known: Vec<String> = sup.agent_rows().into_iter().map(|a| a.repo).collect();
    if let Ok(rows) = sup.db().query("SELECT id FROM repos LIMIT 1000", &chloedb::query::Params::new()) {
        known.extend(rows.iter().filter_map(|r| crate::db::get_str(&r[0])));
    }
    known.into_iter().find(|p| Path::new(p).file_name().is_some_and(|n| n == repo)).map(PathBuf::from).ok_or_else(|| ApiError::not_found(format!("no project named {repo}")))
}

/// The agents of a repository with a branch of their own, the newest of
/// each branch, whose worktree is still there.
fn branch_agents(sup: &Supervisor, repo: &Path) -> Vec<Agent> {
    let repo = repo.to_string_lossy();
    let mut by: BTreeMap<String, Agent> = BTreeMap::new();
    for a in sup.agent_rows() {
        let Some(b) = a.branch.clone() else { continue };
        if a.repo != repo || !Path::new(&a.cwd).is_dir() || a.cwd == a.repo {
            continue;
        }
        if by.get(&b).is_none_or(|k| k.created <= a.created) {
            by.insert(b, a);
        }
    }
    by.into_values().collect()
}

/// A commit of a worktree as it is now, its uncommitted changes and new
/// files included (a private index; nothing in the worktree or its branch
/// changes), to check merges before the agent commits. `None` when git
/// cannot make it.
pub(crate) fn worktree_commit(dir: &Path) -> Option<String> {
    let head = git::rev_parse(dir, "HEAD")?;
    let idx = std::env::temp_dir().join(format!("wb-index-{}-{}-{}", std::process::id(), crate::db::now_ms(), dir.file_name().and_then(|n| n.to_str()).unwrap_or("wt")));
    let run = |args: &[&str]| {
        Command::new("git").arg("-C").arg(dir).args(args).env("GIT_INDEX_FILE", &idx).env("GIT_TERMINAL_PROMPT", "0").output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let out = (|| {
        run(&["read-tree", &head])?;
        run(&["add", "-A"])?;
        let tree = run(&["write-tree"])?;
        run(&["-c", "user.name=Workbench", "-c", "user.email=workbench@localhost", "commit-tree", &tree, "-p", &head, "-m", "uncommitted changes (Workbench's merge check)"])
    })();
    let _ = std::fs::remove_file(&idx);
    out.filter(|s| !s.is_empty())
}

/// An agent's branch as it would merge: its worktree with what is not committed.
fn effective_head(dir: &Path) -> Option<String> {
    if uncommitted(dir).is_empty() {
        git::rev_parse(dir, "HEAD")
    } else {
        worktree_commit(dir)
    }
}

fn main_of(a: &Agent) -> PathBuf {
    git::checkout(Path::new(&a.cwd)).map(|c| c.main).unwrap_or_else(|_| PathBuf::from(&a.repo))
}

/// Files with uncommitted changes in a worktree (tracked and untracked).
fn uncommitted(dir: &Path) -> Vec<String> {
    git_out(dir, &["status", "--porcelain", "--untracked-files=all"]).unwrap_or_default().lines().filter(|l| l.len() > 3).map(|l| l[3..].trim().to_string()).collect()
}

/// `+added −removed` and the files changed since `base`, committed or not.
fn numstat(dir: &Path, base: &str) -> (usize, usize, Vec<String>) {
    let (mut a, mut d, mut files) = (0, 0, Vec::new());
    for l in git_out(dir, &["diff", "--numstat", base, "--"]).unwrap_or_default().lines() {
        let mut p = l.splitn(3, '\t');
        let (Some(x), Some(y), Some(f)) = (p.next(), p.next(), p.next()) else { continue };
        a += x.parse::<usize>().unwrap_or(0);
        d += y.parse::<usize>().unwrap_or(0);
        files.push(f.to_string());
    }
    for f in git_out(dir, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default().lines().filter(|l| !l.is_empty()) {
        a += std::fs::read_to_string(dir.join(f)).map(|t| t.lines().count()).unwrap_or(0);
        files.push(f.to_string());
    }
    (a, d, files)
}

/// What an agent is doing, in words.
fn state_words(sup: &Supervisor, a: &Agent) -> &'static str {
    match a.state.as_str() {
        "waiting" => "Waiting for you",
        "working" | "starting" => "Working",
        "idle" if sup.is_running(&a.id) => "Ready to review",
        _ => "Finished",
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// Failing tests a rule of yours names (a known flaky test: "always ignore
/// test_promo_window"): they do not hold the merge.
fn known_by_rule(sup: &Supervisor, repo: &str, failing: &[String]) -> Vec<String> {
    let rules = crate::questions::rules(sup, Some(repo), true);
    failing
        .iter()
        .filter(|t| {
            let short = t.rsplit("::").next().unwrap_or(t).trim();
            short.len() >= 4 && rules.iter().any(|r| r.text.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w == short))
        })
        .cloned()
        .collect()
}

/// The last error each failing test printed, from Health's test_runs.
fn failing_out(sup: &Supervisor, agents: &[String], failing: &[String]) -> Vec<Json> {
    if failing.is_empty() {
        return Vec::new();
    }
    let rows = sup.db().query("SELECT agent, test, error, ts, passed FROM test_runs LIMIT 100000", &chloedb::query::Params::new()).unwrap_or_default();
    let mut last: HashMap<String, (i64, Option<String>)> = HashMap::new();
    for r in &rows {
        let (Some(ag), Some(t)) = (crate::db::get_str(&r[0]), crate::db::get_str(&r[1])) else { continue };
        if !agents.contains(&ag) || !failing.contains(&t) || crate::db::get_int(&r[4]).unwrap_or(0) != 0 {
            continue;
        }
        let ts = crate::db::get_ms(&r[3]).unwrap_or(0);
        if last.get(&t).is_none_or(|(at, _)| *at <= ts) {
            last.insert(t, (ts, crate::db::get_str(&r[2]).filter(|e| !e.trim().is_empty())));
        }
    }
    failing.iter().map(|t| json!({ "name": t, "error": last.get(t).and_then(|(_, e)| e.as_ref()).map(|e| e.chars().take(4000).collect::<String>()) })).collect()
}

/// [`tests_of`], with the failing tests a rule names left out of the count
/// (said, not hidden) and each failing test's output.
fn tests_for(sup: &Supervisor, a: &Agent, part: &Json, branch: &str) -> Json {
    let mut t = tests_of(part, branch);
    let failing: Vec<String> = t["failing"].as_array().into_iter().flatten().filter_map(Json::as_str).map(str::to_string).collect();
    let ignored = known_by_rule(sup, &a.repo, &failing);
    if !ignored.is_empty() {
        let left: Vec<String> = failing.iter().filter(|f| !ignored.contains(f)).cloned().collect();
        let passed = t["passed"].as_u64().unwrap_or(0);
        t["failed"] = json!(left.len());
        t["failing"] = json!(left);
        t["ok"] = json!(left.is_empty());
        t["line"] = json!(if left.is_empty() {
            format!("{passed} pass on {branch} · {} your rule calls known: {}", plural(ignored.len(), "failing test"), top(&ignored, 3))
        } else {
            format!("{passed} pass · {} fail on {branch}: {} · {} your rule calls known", left.len(), top(&left, 3), plural(ignored.len(), "other"))
        });
    }
    t["ignored"] = json!(ignored);
    let mut ids = vec![a.id.clone()];
    for n in part["agents"].as_array().into_iter().flatten().filter_map(Json::as_str) {
        if let Some(o) = sup.agent_rows().into_iter().find(|o| o.name == n && o.repo == a.repo) {
            ids.push(o.id);
        }
    }
    t["failing_out"] = json!(failing_out(sup, &ids, &failing));
    t
}

/// A test summary of a branch, from `capsule::part`'s tests.
fn tests_of(part: &Json, branch: &str) -> Json {
    let list = part["tests"].as_array().cloned().unwrap_or_default();
    let named: Vec<&Json> = list.iter().filter(|t| t["kind"] == "test").collect();
    let (mut passed, mut failed) = (0usize, 0usize);
    let mut failing = Vec::new();
    let mut flaky = Vec::new();
    for t in &named {
        let name = t["name"].as_str().unwrap_or("").to_string();
        if t["flaky"] == true {
            flaky.push(name.clone());
        }
        if t["ok"] == true {
            passed += 1;
        } else {
            failed += 1;
            failing.push(name);
        }
    }
    // Only summary lines (a runner Health could not split by test).
    if named.is_empty() {
        for t in list.iter().filter(|t| t["kind"] == "summary") {
            passed = passed.max(t["passed"].as_u64().unwrap_or(0) as usize);
            failed = failed.max(t["failed"].as_u64().unwrap_or(0) as usize);
        }
    }
    let ran = passed + failed > 0;
    let line = if !ran {
        format!("No test has run on {branch} yet")
    } else if failed > 0 {
        let names = if failing.is_empty() { String::new() } else { format!(": {}", top(&failing, 3)) };
        format!("{passed} pass · {failed} fail on {branch}{names}")
    } else if !flaky.is_empty() {
        format!("{passed} pass on {branch} · {} flaky: {}", flaky.len(), top(&flaky, 3))
    } else {
        format!("{passed} pass on {branch}")
    };
    json!({ "ran": ran, "ok": if ran { json!(failed == 0) } else { Json::Null }, "passed": passed, "failed": failed, "failing": failing, "flaky": flaky, "line": line, "from": part["tests_from"] })
}

/// "a, b, c and 2 more".
fn top(names: &[String], n: usize) -> String {
    if names.len() <= n {
        names.join(", ")
    } else {
        format!("{} and {} more", names[..n].join(", "), names.len() - n)
    }
}

/// Whether a rule's words name one of the changed files (`money.py`, or
/// `money` for money.py).
fn names_a_file(text: &str, files: &[String]) -> bool {
    let words: Vec<&str> = text.split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'))).map(|w| w.trim_end_matches('.')).filter(|w| !w.is_empty()).collect();
    files.iter().any(|f| {
        let name = Path::new(f).file_name().and_then(|n| n.to_str()).unwrap_or(f);
        let stem = Path::new(f).file_stem().and_then(|n| n.to_str()).unwrap_or("");
        words.iter().any(|w| *w == f || *w == name || (stem.len() >= 4 && *w == stem))
    })
}

/// Every decision behind a branch, from one source, so Review, Ship and
/// the project page count the same: ChloeCtx's decisions and preferences of
/// the branch's agents' sessions (the user's own among them, "My decision:
/// …", signed as yours), and the rules of yours that name a changed file.
/// Facts are not decisions. Those that matter come first: one that replaces
/// an earlier decision (waits for Still right), yours, a rule.
pub(crate) fn decisions_of(sup: &Supervisor, a: &Agent, part: &Json) -> Json {
    let ds = part["decisions"].as_array().cloned().unwrap_or_default();
    let files: Vec<String> = part["blast"]["files"].as_array().into_iter().flatten().filter_map(|f| f["path"].as_str().map(str::to_string)).collect();
    let wanted: Vec<String> = ds.iter().filter_map(|d| d["supersedes"].as_str().map(str::to_string)).collect();
    let texts = if wanted.is_empty() { HashMap::new() } else { crate::capsule::record_texts(sup, a, &wanted) };
    let mut list: Vec<Json> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for d in &ds {
        if d["superseded"] == true || d["kind"] == "fact" {
            continue;
        }
        let id = d["id"].as_str().unwrap_or("").to_string();
        seen.insert(id.clone());
        let raw = d["text"].as_str().unwrap_or("");
        let yours = d["by"] == "you";
        let mut m = json!({
            "id": id, "kind": d["kind"], "text": crate::capsule::plain_words(raw), "raw": raw, "ts": d["ts"],
            "by": if yours { "you" } else { "agent" }, "who": if yours { json!("you") } else { d["agent"].clone() },
            "agent": d["agent"], "confirmed": confirmed(sup, &id), "why": Json::Null,
        });
        if let Some(old) = d["supersedes"].as_str() {
            let was = texts.get(old).map(String::as_str).unwrap_or("");
            m["why"] = json!("replaces");
            m["replaces"] = json!({ "id": old, "text": if was.is_empty() { Json::Null } else { json!(crate::capsule::plain_words(was)) },
                "who": if was.is_empty() { Json::Null } else if crate::capsule::by_user(was, "", "") { json!("yours") } else { Json::Null } });
        } else if yours {
            m["why"] = json!("yours");
        } else if d["kind"] == "preference" {
            m["why"] = json!("new rule");
        }
        list.push(m);
    }
    if !files.is_empty() {
        for r in crate::questions::rules(sup, Some(&a.repo), true) {
            if r.ctx_id.as_ref().is_some_and(|c| seen.contains(c)) || !names_a_file(&r.text, &files) {
                continue;
            }
            list.push(json!({ "id": r.id, "kind": "rule", "text": r.text, "ts": crate::db::iso(r.created), "by": "rule", "who": "you", "confirmed": true, "why": "rule" }));
        }
    }
    let rank = |d: &Json| match d["why"].as_str() {
        Some("replaces") => 0,
        Some("yours") => 1,
        Some("rule") | Some("new rule") => 2,
        _ => 3,
    };
    list.sort_by_key(rank);
    let matter: Vec<Json> = list.iter().filter(|d| !d["why"].is_null()).cloned().collect();
    let routine: Vec<Json> = list.iter().filter(|d| d["why"].is_null()).cloned().collect();
    json!({ "list": list, "count": list.len(), "matter": matter, "routine": routine.len(), "routine_list": routine })
}

// ---------------------------------------------------------------- the list

struct Ready {
    a: Agent,
    branch: String,
    head: String,
    files: Vec<String>,
    row: Json,
    under: Vec<String>,
    score: i64,
}

/// `GET /api/review?repo=`.
pub fn list(sup: &Supervisor, repo: &str) -> Result<Json, ApiError> {
    let repo = repo_of(sup, repo)?;
    let mut missing: Vec<String> = Vec::new();
    let mut ready: Vec<Ready> = Vec::new();
    let mut not_ready: Vec<Json> = Vec::new();
    let mut pending: Vec<(Agent, Vec<String>)> = Vec::new();
    let mut onto: Option<String> = None;
    for a in branch_agents(sup, &repo) {
        let dir = PathBuf::from(&a.cwd);
        let main = main_of(&a);
        let land = blast::landing(sup, &a, &dir, &main);
        let Some(head) = land.head.clone() else { continue };
        let branch = a.branch.clone().unwrap_or_default();
        let target = land.onto.clone();
        if onto.is_none() {
            onto = target.clone();
        }
        let (added, removed, files) = numstat(&dir, &land.base);
        let merged = target.as_deref().and_then(|t| git::rev_parse(&main, &format!("refs/heads/{t}"))).is_some_and(|t| git::is_ancestor(&dir, &head, &t));
        if files.is_empty() || (merged && uncommitted(&dir).is_empty()) {
            continue;
        }
        let words = state_words(sup, &a);
        if matches!(words, "Working" | "Waiting for you") {
            not_ready.push(json!({ "agent": trail::agent_json(sup, &a), "branch": branch, "state": words, "added": added, "removed": removed, "files": files.len(),
                "line": format!("{} · {}", words.to_lowercase(), plural(files.len(), "file")) }));
            pending.push((a, files));
            continue;
        }
        let part = match crate::capsule::part(sup, &a, &mut missing) {
            Ok(p) => p,
            Err(e) => {
                missing.push(format!("{}: {}", a.name, e.message));
                continue;
            }
        };
        let b = &part["blast"];
        let tests = tests_for(sup, &a, &part, &branch);
        let decisions = decisions_of(sup, &a, &part);
        let dirty = uncommitted(&dir);
        // Merge-ability is counted on the worktree as it is, uncommitted
        // changes included, so it says now what merging will say once they
        // are committed (and the same as the blast radius's exceptions).
        let head = if dirty.is_empty() { head } else { worktree_commit(&dir).unwrap_or(head) };
        let onto_tip = target.as_deref().and_then(|t| git::rev_parse(&main, &format!("refs/heads/{t}")));
        let into_main = match (&onto_tip, blast::merges_clean(&main, onto_tip.as_deref().unwrap_or(""), &head)) {
            (Some(_), Some((clean, conflicts))) => json!({ "clean": clean, "conflicts": conflicts, "with_uncommitted": !dirty.is_empty() }),
            _ => json!({ "clean": Json::Null, "conflicts": [] }),
        };
        let level = b["risk"]["level"].as_str().unwrap_or("medium").to_string();
        let score = b["risk"]["score"].as_i64().unwrap_or(0);
        let tone = if tests["ok"] == false || into_main["clean"] == false {
            "crit"
        } else if level == "low" && dirty.is_empty() {
            "good"
        } else if level == "high" {
            "crit"
        } else {
            "warn"
        };
        let verdict = if into_main["clean"] == false {
            "conflicts with main".to_string()
        } else if tests["ok"] == false {
            "tests fail".to_string()
        } else if !dirty.is_empty() {
            "not committed".to_string()
        } else {
            format!("{level} risk")
        };
        let under: Vec<String> = b["landing"]["under"].as_array().into_iter().flatten().filter(|u| u["merged"] != true).filter_map(|u| u["agent"]["id"].as_str().map(str::to_string)).collect();
        let row = json!({
            "agent": trail::agent_json(sup, &a), "branch": branch, "onto": target, "state": state_words(sup, &a),
            "title": part["title"], "title_from": part["title_from"], "said": part["summary"],
            "added": added, "removed": removed, "files": files.len(), "commits": part["commits"].as_array().map_or(0, |c| c.len()),
            "uncommitted": dirty,
            "tests": tests, "merge": { "onto": target, "main": into_main, "others": [] },
            "decisions": { "matter": decisions["matter"], "routine": decisions["routine"] },
            "risk": { "level": level, "line": b["risk"]["line"] },
            "verdict": { "tone": tone, "text": verdict },
            "stack": b["landing"]["line"],
            "gate": gate(&branch, &tests, &decisions, &level, &dirty, into_main["clean"].as_bool()),
            "last_merge": last_merge(sup, &a.id),
        });
        ready.push(Ready { a, branch: row["branch"].as_str().unwrap_or("").to_string(), head, files, row, under, score });
    }

    // Each ready branch with each other one.
    let n = ready.len();
    let mut conflicts_with: Vec<usize> = vec![0; n];
    for i in 0..n {
        for j in 0..n {
            if i == j {
                continue;
            }
            let main = main_of(&ready[i].a);
            let stacked = ready[i].under.contains(&ready[j].a.id) || ready[j].under.contains(&ready[i].a.id);
            let (clean, files) = if stacked { (Some(true), Vec::new()) } else { blast::merges_clean(&main, &ready[i].head, &ready[j].head).map_or((None, Vec::new()), |(c, f)| (Some(c), f)) };
            if clean == Some(false) {
                conflicts_with[i] += 1;
            }
            let shared: Vec<&String> = ready[i].files.iter().filter(|f| ready[j].files.contains(f)).take(3).collect();
            let other = json!({ "agent": { "id": ready[j].a.id, "name": ready[j].a.name }, "branch": ready[j].branch, "clean": clean, "conflicts": files, "shared": shared, "stacked": stacked });
            ready[i].row["merge"]["others"].as_array_mut().unwrap().push(other);
        }
    }
    for r in ready.iter_mut() {
        let m = &r.row["merge"];
        let bad: Vec<String> = m["others"].as_array().into_iter().flatten().filter(|o| o["clean"] == false).filter_map(|o| o["agent"]["name"].as_str().map(str::to_string)).collect();
        let onto = m["onto"].as_str().unwrap_or("main").to_string();
        let line = match m["main"]["clean"].as_bool() {
            Some(false) => format!("Conflicts with {onto} in {}", m["main"]["conflicts"].as_array().into_iter().flatten().filter_map(Json::as_str).take(3).collect::<Vec<_>>().join(", ")),
            Some(true) if bad.is_empty() => format!("Merges cleanly into {onto}"),
            Some(true) => format!("Merges cleanly into {onto}; conflicts with {}", bad.join(", ")),
            None => format!("Not checked: no branch {onto} to merge into"),
        };
        r.row["merge"]["ok"] = json!(m["main"]["clean"] == true && bad.is_empty());
        r.row["merge"]["line"] = json!(line);
    }

    // The order: a branch others are stacked on first; then those that
    // conflict with fewer; then lower risk; then the one that finished first.
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by_key(|&i| (conflicts_with[i], ready[i].score, ready[i].a.state_since));
    let mut order: Vec<usize> = Vec::new();
    let mut placed: HashSet<usize> = HashSet::new();
    fn place(i: usize, ready: &[Ready], idx: &[usize], order: &mut Vec<usize>, placed: &mut HashSet<usize>, depth: usize) {
        if placed.contains(&i) || depth > 16 {
            return;
        }
        for &j in idx {
            if ready[i].under.contains(&ready[j].a.id) {
                place(j, ready, idx, order, placed, depth + 1);
            }
        }
        if placed.insert(i) {
            order.push(i);
        }
    }
    for &i in &idx {
        place(i, &ready, &idx, &mut order, &mut placed, 0);
    }
    let names: Vec<String> = order.iter().map(|&i| ready[i].a.name.clone()).collect();
    let mut rebase = Vec::new();
    for (a, files) in &pending {
        if let Some(&i) = order.iter().find(|&&i| ready[i].files.iter().any(|f| files.contains(f))) {
            let f = ready[i].files.iter().find(|f| files.contains(f)).cloned().unwrap_or_default();
            rebase.push(json!({ "agent": { "id": a.id, "name": a.name }, "on": { "id": ready[i].a.id, "name": ready[i].a.name }, "file": f }));
        }
    }
    let mut line = match names.len() {
        0 => String::new(),
        1 => format!("Merge {}.", names[0]),
        _ => format!("Merge {}.", names.join(", then ")),
    };
    if names.len() > 1 {
        // The first pair, in merge order, that conflicts: named, with the file.
        let mut pair: Option<(String, String, String)> = None;
        'find: for (pos, &i) in order.iter().enumerate() {
            for &j in &order[pos + 1..] {
                let o = ready[i].row["merge"]["others"].as_array().and_then(|o| o.iter().find(|x| x["agent"]["id"] == ready[j].a.id.as_str()).cloned());
                if let Some(o) = o.filter(|o| o["clean"] == false) {
                    let file = o["conflicts"].as_array().and_then(|c| c.first()).and_then(Json::as_str).unwrap_or("the same lines").to_string();
                    pair = Some((ready[i].a.name.clone(), ready[j].a.name.clone(), file));
                    break 'find;
                }
            }
        }
        let any_shared = order.iter().any(|&i| ready[i].row["merge"]["others"].as_array().is_some_and(|o| o.iter().any(|x| x["shared"].as_array().is_some_and(|s| !s.is_empty()))));
        line += &if let Some((first, then, file)) = pair {
            format!(" {first} and {then} conflict in {file}: merge {first} first, then send {then} back to rebase.")
        } else if any_shared {
            " They change some of the same files, and merge without a conflict.".to_string()
        } else {
            " They touch different files.".to_string()
        };
    }
    for r in &rebase {
        line += &format!(" {} (still working) will need rebasing on {}, since both change {}.", r["agent"]["name"].as_str().unwrap_or(""), r["on"]["name"].as_str().unwrap_or(""), r["file"].as_str().unwrap_or(""));
    }
    for (pos, &i) in order.iter().enumerate() {
        ready[i].row["order"] = json!(pos + 1);
    }
    let rows: Vec<Json> = order.iter().map(|&i| ready[i].row.clone()).collect();
    Ok(json!({
        "repo": repo, "onto": onto, "ready": rows, "not_ready": not_ready,
        "order": { "agents": order.iter().map(|&i| ready[i].a.id.clone()).collect::<Vec<_>>(), "names": names, "line": line.trim(), "rebase": rebase },
        "missing": missing,
    }))
}

// ---------------------------------------------------------------- one change

/// Entry points: files whose code a user reaches from outside (a page, a
/// server's routes, a command line).
fn entry_point(path: &str) -> bool {
    let p = path.to_lowercase();
    let stem = Path::new(&p).file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let ext = Path::new(&p).extension().and_then(|s| s.to_str()).unwrap_or("");
    matches!(stem.as_str(), "main" | "app" | "server" | "serve" | "index" | "cli" | "routes" | "api" | "handlers" | "views" | "urls" | "wsgi" | "asgi" | "manage")
        || matches!(ext, "html" | "htm")
        || ["web/", "pages/", "routes/", "api/", "bin/", "public/", "handlers/", "views/", "cmd/"].iter().any(|d| p.starts_with(d) || p.contains(&format!("/{d}")))
}

fn lockfile(path: &str) -> bool {
    let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(name, "Cargo.lock" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" | "poetry.lock" | "Gemfile.lock" | "go.sum" | "composer.lock" | "uv.lock" | "Pipfile.lock" | "bun.lockb")
}

fn generated(path: &str) -> bool {
    let p = path.to_lowercase();
    p.contains(".min.") || p.starts_with("dist/") || p.contains("/dist/") || p.starts_with("build/") || p.contains("__generated__") || p.ends_with(".map") || p.ends_with(".snap")
}

fn doc(path: &str) -> bool {
    let p = path.to_lowercase();
    p.ends_with(".md") || p.ends_with(".rst") || p.ends_with(".txt") || p.starts_with("docs/")
}

fn import_line(t: &str) -> bool {
    let t = t.trim();
    t.is_empty() || t.starts_with("import ") || t.starts_with("from ") && t.contains(" import ") || t.starts_with("use ") || t.starts_with("#include") || t.starts_with("require(") || (t.starts_with("const ") && t.contains("require("))
}

/// Why a hunk is mechanical: only imports, or the same code reformatted.
fn mechanical_hunk(h: &Json) -> Option<&'static str> {
    let lines = h["lines"].as_array()?;
    let changed: Vec<(&str, &str)> = lines.iter().filter(|l| l["kind"] != " ").map(|l| (l["kind"].as_str().unwrap_or(""), l["text"].as_str().unwrap_or(""))).collect();
    if changed.is_empty() {
        return Some("formatting");
    }
    if changed.iter().all(|(_, t)| import_line(t)) {
        return Some("imports");
    }
    let squash = |k: &str| changed.iter().filter(|(kk, _)| *kk == k).map(|(_, t)| t.chars().filter(|c| !c.is_whitespace()).collect::<String>()).collect::<String>();
    let (a, d) = (squash("+"), squash("-"));
    (!a.is_empty() && a == d).then_some("formatting")
}

/// The tests a change adds or edits in these files: test functions, not
/// their helpers (`_run_page`); all of them when none is named test….
fn test_count(b: &Json, paths: &[String]) -> usize {
    let names: Vec<&str> = b["test_functions"].as_array().into_iter().flatten().filter(|f| f["status"] != "deleted" && f["path"].as_str().is_none_or(|p| paths.iter().any(|x| x == p))).filter_map(|f| f["name"].as_str()).collect();
    let tests = names.iter().filter(|n| n.trim_start_matches('_').to_lowercase().starts_with("test")).count();
    if tests > 0 {
        tests
    } else {
        names.len()
    }
}

fn count(h: &Json, k: &str) -> usize {
    h["lines"].as_array().map_or(0, |ls| ls.iter().filter(|l| l["kind"] == k).count())
}

/// A hunk trimmed to the lines around its changes.
fn trim_hunk(h: &Json, max: usize) -> Vec<Json> {
    let lines = h["lines"].as_array().cloned().unwrap_or_default();
    let first = lines.iter().position(|l| l["kind"] != " ").unwrap_or(0);
    let from = first.saturating_sub(2);
    lines.into_iter().skip(from).take(max).collect()
}

/// `GET /api/agents/:id/review`: one change, checked.
pub fn one(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let a = sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
    let dir = PathBuf::from(&a.cwd);
    if !dir.is_dir() {
        return Err(ApiError::conflict(format!("{}'s worktree {} is gone", a.name, a.cwd)));
    }
    let mut missing: Vec<String> = Vec::new();
    let part = crate::capsule::part(sup, &a, &mut missing)?;
    let b = &part["blast"];
    let branch = a.branch.clone().unwrap_or_else(|| "HEAD".into());
    let onto = b["landing"]["onto"].as_str().unwrap_or("main").to_string();
    let main = main_of(&a);
    let head = b["head"].as_str().unwrap_or("").to_string();
    let tests = tests_for(sup, &a, &part, &branch);
    let decisions = decisions_of(sup, &a, &part);
    let dirty = uncommitted(&dir);
    let onto_tip = git::rev_parse(&main, &format!("refs/heads/{onto}"));
    // Checked on the worktree as it is, as the list does.
    let eff = if dirty.is_empty() { head.clone() } else { worktree_commit(&dir).unwrap_or_else(|| head.clone()) };
    let into_main = onto_tip.as_deref().and_then(|t| blast::merges_clean(&main, t, &eff));
    // Merged: its last merge stands and its branch is in the landing branch.
    let lm = last_merge(sup, id);
    let merged = !lm.is_null()
        && lm["undone"] != true
        && dirty.is_empty()
        && git::rev_parse(&dir, "HEAD").zip(onto_tip.as_deref()).is_some_and(|(h, t)| git::is_ancestor(&main, &h, t));
    let merged_commits: Vec<Json> = if merged {
        git_out(&main, &["log", "--no-merges", "--format=%h%x09%s", &format!("{}..{}", lm["before"].as_str().unwrap_or(""), lm["commit"].as_str().unwrap_or(""))])
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(sha, s)| json!({ "sha": sha, "subject": s }))
            .collect()
    } else {
        Vec::new()
    };

    // The rings.
    let funcs: Vec<&Json> = b["functions"].as_array().into_iter().flatten().filter(|f| f["status"] != "deleted").collect();
    let mut by_callers = funcs.clone();
    by_callers.sort_by_key(|f| std::cmp::Reverse(f["callers_total"].as_u64().unwrap_or(0)));
    let fn_names: Vec<String> = by_callers.iter().filter_map(|f| f["name"].as_str().map(|n| format!("{n}()"))).collect();
    let mut callers: Vec<String> = Vec::new();
    let mut texts: HashMap<String, Option<String>> = HashMap::new();
    let mut caller_files: BTreeSet<String> = BTreeSet::new();
    for f in &by_callers {
        for c in f["callers"].as_array().into_iter().flatten() {
            let (Some(p), Some(n)) = (c["path"].as_str(), c["line"].as_u64()) else { continue };
            caller_files.insert(p.to_string());
            let text = texts.entry(p.to_string()).or_insert_with(|| std::fs::read_to_string(dir.join(p)).ok());
            let name = blast::Lang::of(p)
                .zip(text.as_deref())
                .and_then(|(lang, t)| {
                    let defs = blast::defs(lang, t);
                    blast::enclosing(&defs, n as usize).map(|d| d.name.clone())
                })
                .unwrap_or_else(|| p.to_string());
            if !callers.contains(&name) {
                callers.push(name);
            }
        }
    }
    let n_callers: usize = funcs.iter().filter_map(|f| f["callers_total"].as_u64()).sum::<u64>() as usize;
    let files_json = b["files"].as_array().cloned().unwrap_or_default();
    let mut public: Vec<String> = caller_files.iter().filter(|p| entry_point(p)).cloned().collect();
    for f in &files_json {
        if let Some(p) = f["path"].as_str().filter(|p| entry_point(p) && !blast::is_test_path(p)) {
            if !public.iter().any(|x| x == p) {
                public.push(p.to_string());
            }
        }
    }
    // A page that shows what a changed function computes (the checkout page
    // showing the tax) is something users see, though it does not call it.
    let short_names: Vec<String> = funcs
        .iter()
        .filter_map(|f| f["name"].as_str())
        .map(|n| n.rsplit(['.', ':']).next().unwrap_or(n).to_string())
        .filter(|n| n.len() >= 3 && !matches!(n.as_str(), "get" | "set" | "add" | "run" | "main" | "init" | "new" | "__init__" | "len" | "str"))
        .collect();
    if !short_names.is_empty() {
        for p in git_out(&dir, &["ls-files"]).unwrap_or_default().lines().filter(|p| entry_point(p) && !blast::is_test_path(p)).take(300) {
            if public.iter().any(|x| x == p) {
                continue;
            }
            let Ok(t) = std::fs::read_to_string(dir.join(p)) else { continue };
            if t.len() > 400_000 {
                continue;
            }
            if let Some(n) = short_names.iter().find(|n| t.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w == n.as_str())) {
                public.push(format!("{p} (shows {n})"));
            }
        }
    }
    let rings = json!([
        { "n": funcs.len(), "label": if funcs.len() == 1 { "function changed" } else { "functions changed" }, "names": fn_names.iter().take(RING_NAMES).collect::<Vec<_>>(), "code": true },
        { "n": n_callers, "label": if n_callers == 1 { "caller" } else { "callers" }, "names": callers.iter().take(RING_NAMES).collect::<Vec<_>>() },
        { "n": public.len(), "label": if public.len() == 1 { "public name" } else { "public names" }, "names": public.iter().take(RING_NAMES).collect::<Vec<_>>(), "code": true, "what": "entry points a user reaches: pages, servers' routes, commands" },
    ]);

    // The exceptions: only what you must know.
    let mut exceptions: Vec<Json> = Vec::new();
    let untested: Vec<String> = funcs.iter().filter(|f| f["tests"].as_array().is_none_or(|t| t.is_empty())).filter_map(|f| f["name"].as_str().map(|n| format!("{n}()"))).collect();
    if !untested.is_empty() {
        let failing = tests["failing"].as_array().map(|v| v.iter().filter_map(Json::as_str).take(3).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        exceptions.push(json!({ "tone": "crit", "text": format!("Untested: {}", top(&untested, 4)), "code": untested, "sub": if failing.is_empty() { Json::Null } else { json!(format!("{failing} fail on the branch")) } }));
    }
    for f in b["functions"].as_array().into_iter().flatten().filter(|f| f["status"] == "deleted" && f["callers_total"].as_u64().unwrap_or(0) > 0) {
        exceptions.push(json!({ "tone": "crit", "text": format!("{}() is removed but still called {}", f["name"].as_str().unwrap_or(""), plural(f["callers_total"].as_u64().unwrap_or(0) as usize, "time")) }));
    }
    for g in b["agents"].as_array().into_iter().flatten() {
        let who = g["agent"]["name"].as_str().unwrap_or("another agent");
        let kinds: Vec<&str> = g["kinds"].as_array().into_iter().flatten().filter_map(Json::as_str).collect();
        let file = g["files"].as_array().and_then(|f| f.first()).and_then(Json::as_str).unwrap_or("");
        let other = g["agent"]["id"].as_str().and_then(|i| sup.agent_row(i)).filter(|o| Path::new(&o.cwd).is_dir());
        let other_head = other.as_ref().and_then(|o| effective_head(Path::new(&o.cwd)));
        // A branch already merged into the landing branch is part of it now: nothing to say.
        if other.as_ref().is_some_and(|o| uncommitted(Path::new(&o.cwd)).is_empty()) && other_head.as_deref().zip(onto_tip.as_deref()).is_some_and(|(h, t)| git::is_ancestor(&main, h, t)) {
            continue;
        }
        if kinds.contains(&"same_file") {
            let clean = other_head.as_deref().and_then(|h| blast::merges_clean(&main, &eff, h)).map(|m| m.0).or(g["merges_clean"].as_bool());
            let sub = match clean {
                Some(false) => format!("Merging the two conflicts: merge {} first, then send {who} back to rebase", a.name),
                Some(true) => format!("They merge without a conflict; merge {} first", a.name),
                None => format!("Whether they conflict is not known yet; merge {} first", a.name),
            };
            exceptions.push(json!({ "tone": "warn", "text": format!("{who} also changes {file}"), "sub": sub }));
        } else if kinds.contains(&"calls_changed") {
            exceptions.push(json!({ "tone": "warn", "text": format!("{who} calls a changed function"), "sub": g["detail"] }));
        }
    }
    for d in decisions["matter"].as_array().into_iter().flatten().filter(|d| d["why"] == "replaces") {
        let old = d["replaces"]["text"].as_str().map(|t| format!("“{}”", crate::supervisor::one_line(t, 90))).unwrap_or_else(|| format!("decision {}", d["replaces"]["id"].as_str().unwrap_or("")));
        exceptions.push(json!({ "tone": "ctx", "text": format!("Replaces {old}"), "sub": d["text"], "record": d["id"], "confirm": true }));
    }
    if !dirty.is_empty() {
        exceptions.push(json!({ "tone": "warn", "text": format!("{} not committed", plural(dirty.len(), "file")), "sub": format!("Merge takes commits only: {}", top(&dirty, 3)) }));
    }
    if let Some(l) = b["landing"]["line"].as_str() {
        exceptions.push(json!({ "tone": "warn", "text": l }));
    }
    let merge_line = match into_main {
        Some((true, _)) => format!("merges cleanly into {onto}"),
        Some((false, ref c)) => format!("conflicts with {onto} in {}", top(c, 3)),
        None => format!("no branch {onto} to merge into"),
    };
    let tests_line = format!("{} · {merge_line}", tests["line"].as_str().unwrap_or(""));

    // The verdict, in one sentence.
    let level = b["risk"]["level"].as_str().unwrap_or("medium");
    let tone = if tests["ok"] == false || matches!(into_main, Some((false, _))) || level == "high" {
        "crit"
    } else if level == "low" && dirty.is_empty() {
        "good"
    } else {
        "warn"
    };
    // What it changes, in its own terms (functions, else files): never what
    // the agent said in chat, which can cut a URL at its first dot.
    let mut changed_files: Vec<String> = files_json.iter().filter_map(|f| f["path"].as_str().map(str::to_string)).collect();
    // The code it changes before its tests.
    changed_files.sort_by_key(|p| blast::is_test_path(p));
    let what_changed = if !fn_names.is_empty() {
        format!("Changes {}", top(&fn_names, 2))
    } else if !changed_files.is_empty() {
        format!("Changes {}", top(&changed_files, 2))
    } else {
        "Changes nothing yet".to_string()
    };
    let because = if !untested.is_empty() {
        format!("{} {} no test", plural(untested.len(), "changed function"), if untested.len() == 1 { "has" } else { "have" })
    } else if tests["ok"] == false {
        format!("{} on the branch", fails_words(tests["failed"].as_u64().unwrap_or(0) as usize))
    } else if funcs.is_empty() {
        "no function changed".to_string()
    } else if n_callers > 0 {
        format!("every changed function is tested, {}", plural(n_callers, "caller"))
    } else {
        "every changed function is tested".to_string()
    };
    let cap = |s: &str| s[..1].to_uppercase() + &s[1..];
    let verdict = json!({ "tone": tone, "level": level, "head": format!("{} risk.", cap(level)), "text": format!("{what_changed}; {because}.") });

    // The pieces of work.
    let diff = git::diff(&dir, b["base"].as_str().unwrap_or("HEAD")).unwrap_or_default();
    let subjects = |p: &str| -> Option<String> { git_out(&dir, &["log", "-1", "--format=%s", &format!("{}..HEAD", b["base"].as_str().unwrap_or("HEAD")), "--", p]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) };
    let fn_of: HashMap<&str, Vec<&Json>> = {
        let mut m: HashMap<&str, Vec<&Json>> = HashMap::new();
        for f in b["functions"].as_array().into_iter().flatten() {
            if let Some(p) = f["path"].as_str() {
                m.entry(p).or_default().push(f);
            }
        }
        m
    };
    let mut mech_files: Vec<String> = Vec::new();
    let mut mech_what: BTreeMap<&'static str, usize> = BTreeMap::new();
    let (mut mech_add, mut mech_del) = (0usize, 0usize);
    // Group: tests, docs, else each source file.
    let mut groups: BTreeMap<String, Vec<(String, Vec<Json>)>> = BTreeMap::new();
    for f in &diff {
        let p = f["path"].as_str().unwrap_or("").to_string();
        let hunks = f["hunks"].as_array().cloned().unwrap_or_default();
        let whole = if lockfile(&p) {
            Some("lockfiles")
        } else if generated(&p) {
            Some("generated files")
        } else if f["binary"] == true {
            Some("binary files")
        } else if f["status"] == "renamed" && hunks.is_empty() {
            Some("renames")
        } else {
            None
        };
        if let Some(w) = whole {
            *mech_what.entry(w).or_default() += 1;
            mech_files.push(p.clone());
            mech_add += hunks.iter().map(|h| count(h, "+")).sum::<usize>();
            mech_del += hunks.iter().map(|h| count(h, "-")).sum::<usize>();
            continue;
        }
        let mut keep = Vec::new();
        for h in hunks {
            match mechanical_hunk(&h) {
                Some(w) => {
                    *mech_what.entry(if w == "imports" { "import-only hunks" } else { "formatting-only hunks" }).or_default() += 1;
                    mech_add += count(&h, "+");
                    mech_del += count(&h, "-");
                }
                None => keep.push(h),
            }
        }
        if keep.is_empty() {
            if !mech_files.contains(&p) {
                mech_files.push(p);
            }
            continue;
        }
        let key = if blast::is_test_path(&p) {
            "~tests".to_string()
        } else if doc(&p) {
            "~docs".to_string()
        } else {
            p.clone()
        };
        groups.entry(key).or_default().push((p, keep));
    }
    // Beyond MAX_PIECES source files, fold them by folder.
    let sources: Vec<String> = groups.keys().filter(|k| !k.starts_with('~')).cloned().collect();
    if sources.len() > MAX_PIECES {
        for k in sources {
            let dirname = Path::new(&k).parent().map(|d| d.to_string_lossy().to_string()).filter(|d| !d.is_empty()).unwrap_or_else(|| ".".into());
            let v = groups.remove(&k).unwrap_or_default();
            groups.entry(format!("{dirname}/")).or_default().extend(v);
        }
    }
    let mut pieces: Vec<Json> = Vec::new();
    for (key, files) in &groups {
        let add: usize = files.iter().flat_map(|(_, hs)| hs.iter()).map(|h| count(h, "+")).sum();
        let del: usize = files.iter().flat_map(|(_, hs)| hs.iter()).map(|h| count(h, "-")).sum();
        let paths: Vec<String> = files.iter().map(|(p, _)| p.clone()).collect();
        let fs: Vec<&Json> = paths.iter().flat_map(|p| fn_of.get(p.as_str()).cloned().unwrap_or_default()).collect();
        let untested_here: Vec<&Json> = fs.iter().copied().filter(|f| f["status"] != "deleted" && f["tests"].as_array().is_none_or(|t| t.is_empty())).collect();
        let others = b["agents"].as_array().into_iter().flatten().any(|g| g["files"].as_array().is_some_and(|fl| fl.iter().any(|x| paths.iter().any(|p| x == p))));
        let calls: u64 = fs.iter().filter_map(|f| f["callers_total"].as_u64()).sum();
        let (risk, title, words) = if key == "~tests" {
            let n = test_count(b, &paths);
            ("low", "Tests".to_string(), if n > 0 { format!("{} added or changed in {}.", plural(n, "test"), plural(paths.len(), "file")) } else { format!("Changes in {}.", plural(paths.len(), "test file")) })
        } else if key == "~docs" {
            ("low", "Docs".to_string(), format!("{} of text.", plural(paths.len(), "file")))
        } else {
            let risk = if untested_here.iter().any(|f| f["callers_total"].as_u64().unwrap_or(0) > 0) || fs.iter().any(|f| f["status"] == "deleted" && f["callers_total"].as_u64().unwrap_or(0) > 0) {
                "high"
            } else if !untested_here.is_empty() || others {
                "medium"
            } else {
                "low"
            };
            let names: Vec<String> = fs.iter().filter_map(|f| f["name"].as_str().map(|n| format!("{n}()"))).collect();
            let title = paths.iter().find_map(|p| subjects(p)).unwrap_or_else(|| if names.is_empty() { format!("Changes {}", paths[0]) } else { format!("Changes {}", top(&names, 2)) });
            let mut w = if names.is_empty() { format!("{} changed.", top(&paths, 2)) } else { format!("{} changed; {}", top(&names, 3), plural(calls as usize, "caller")) };
            if !names.is_empty() {
                w += &if untested_here.is_empty() { ", all tested.".to_string() } else { format!(", {} untested.", top(&untested_here.iter().filter_map(|f| f["name"].as_str().map(|n| format!("{n}()"))).collect::<Vec<_>>(), 2)) };
            }
            if others {
                w += " Another agent changes this file too.";
            }
            (risk, title, w)
        };
        // The hunk that matters: the one holding the function with most callers, else the largest.
        let best_fn = fs.iter().max_by_key(|f| f["callers_total"].as_u64().unwrap_or(0) * 1000 + f["added"].as_u64().unwrap_or(0) + f["removed"].as_u64().unwrap_or(0));
        let mut pick: Option<(&String, &Json)> = None;
        if let Some(f) = best_fn {
            let line = f["line"].as_i64().unwrap_or(0);
            let fp = f["path"].as_str().unwrap_or("");
            for (p, hs) in files {
                if p == fp {
                    pick = hs.iter().find(|h| h["lines"].as_array().is_some_and(|ls| ls.iter().any(|l| l["kind"] != " " && l["new_line"].as_i64().or(l["old_line"].as_i64()).is_some_and(|n| n >= line)))).map(|h| (p, h));
                }
            }
        }
        if pick.is_none() {
            pick = files.iter().flat_map(|(p, hs)| hs.iter().map(move |h| (p, h))).max_by_key(|(_, h)| count(h, "+") + count(h, "-"));
        }
        let hunk = pick.map(|(p, h)| {
            let wher = best_fn.filter(|f| f["path"].as_str() == Some(p.as_str())).and_then(|f| f["name"].as_str()).map(|n| format!("{p} · {n}()")).unwrap_or_else(|| p.clone());
            json!({ "path": p, "where": wher, "lines": trim_hunk(h, HUNK_LINES) })
        });
        let mut budget = PIECE_LINES;
        let all: Vec<Json> = files
            .iter()
            .map(|(p, hs)| {
                let hs: Vec<Json> = hs
                    .iter()
                    .filter_map(|h| {
                        let ls = h["lines"].as_array().cloned().unwrap_or_default();
                        if budget == 0 {
                            return None;
                        }
                        let take = ls.len().min(budget);
                        budget -= take;
                        Some(json!({ "new_start": h["new_start"], "lines": ls.into_iter().take(take).collect::<Vec<_>>() }))
                    })
                    .collect();
                json!({ "path": p, "hunks": hs })
            })
            .collect();
        let n_hunks: usize = files.iter().map(|(_, hs)| hs.len()).sum();
        let kind = if key == "~tests" { "tests" } else if key == "~docs" { "docs" } else { "code" };
        pieces.push(json!({ "kind": kind, "title": title, "words": words, "files": paths, "added": add, "removed": del, "risk": risk, "hunk": hunk, "hunks": n_hunks, "all": all, "truncated": budget == 0 }));
    }
    let rank = |r: &str| match r {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    };
    // Code before its tests and docs at the same risk: the key change is the fix, not its test helper.
    let aside = |p: &Json| p["kind"] != "code";
    pieces.sort_by_key(|p| (rank(p["risk"].as_str().unwrap_or("")), aside(p), std::cmp::Reverse(p["added"].as_u64().unwrap_or(0) + p["removed"].as_u64().unwrap_or(0))));
    let more = pieces.len().saturating_sub(MAX_PIECES);
    let shown: Vec<Json> = pieces.iter().take(MAX_PIECES).cloned().collect();
    let what: Vec<String> = mech_what.iter().map(|(w, n)| if *n == 1 { format!("1 {}", w.trim_end_matches('s')) } else { format!("{n} {w}") }).collect();
    let totals = &b["totals"];

    Ok(json!({
        "agent": trail::agent_json(sup, &a), "branch": branch, "onto": onto, "repo": a.repo, "state": if merged { "Merged" } else { state_words(sup, &a) },
        "merged": if merged { lm.clone() } else { Json::Null }, "merged_commits": merged_commits,
        "title": part["title"], "said": part["summary"], "commits": part["commits"],
        "totals": { "files": totals["files"], "added": totals["added"], "removed": totals["removed"] },
        "turns": sup.all_events(id).map(|e| e.iter().filter(|e| e.kind == "UserPromptSubmit").count()).unwrap_or(0),
        "verdict": verdict,
        "blast": { "rings": rings, "exceptions": exceptions, "tests": tests_line, "risk": b["risk"] },
        "checks": {
            "tests": tests,
            "merge": { "onto": onto, "clean": into_main.as_ref().map(|m| m.0), "conflicts": into_main.as_ref().map(|m| m.1.clone()).unwrap_or_default(), "line": merge_line, "with_uncommitted": !dirty.is_empty() },
            "uncommitted": dirty,
        },
        "pieces": shown, "more_pieces": more,
        "mechanical": { "files": mech_files, "added": mech_add, "removed": mech_del, "what": what.join(", ") },
        "decisions": decisions,
        "gate": gate(&branch, &tests, &decisions, level, &dirty, into_main.as_ref().map(|m| m.0)),
        "last_merge": lm,
        "plan": {
            "how": if git::current_branch(&main).as_deref() == Some(onto.as_str()) { "merge commit" } else { "merge commit, written without touching a checkout" },
            "where": main,
            "onto_checked_out": git::current_branch(&main).as_deref() == Some(onto.as_str()),
            "onto_tip": onto_tip,
        },
        "missing": missing,
    }))
}

fn merge_key(id: &str) -> String {
    format!("review-merged:{id}")
}

/// The last merge of an agent's branch, with whether Undo can still take it back.
fn last_merge(sup: &Supervisor, id: &str) -> Json {
    let Some(v) = sup.mark(&merge_key(id)).and_then(|s| serde_json::from_str::<Json>(&s).ok()) else { return Json::Null };
    let main = PathBuf::from(v["where"].as_str().unwrap_or(""));
    let tip = git::rev_parse(&main, &format!("refs/heads/{}", v["onto"].as_str().unwrap_or("main")));
    let mut v = v;
    let after = v["commit"].as_str().unwrap_or("").to_string();
    let pushed = !git_out(&main, &["branch", "-r", "--contains", &after]).unwrap_or_default().trim().is_empty();
    v["can_undo"] = json!(v["undone"] != true && tip.as_deref() == Some(after.as_str()) && !pushed);
    v["pushed"] = json!(pushed);
    v
}

// ---------------------------------------------------------------- merge

/// `git` with an identity when the repository has none set.
fn git_id(dir: &Path) -> Vec<String> {
    let has = git_out(dir, &["config", "user.email"]).is_ok_and(|s| !s.trim().is_empty());
    if has {
        Vec::new()
    } else {
        ["-c", "user.name=Workbench", "-c", "user.email=workbench@localhost"].iter().map(|s| s.to_string()).collect()
    }
}

fn run_git(dir: &Path, pre: &[String], args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(dir).args(pre).args(args).env("GIT_TERMINAL_PROMPT", "0").output().map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `POST /api/review/merge {agents}`: each agent's branch into its landing
/// branch, in the order given, stopping at the first that cannot.
pub fn merge(sup: &Supervisor, ids: &[String], anyway: &[String]) -> Result<Json, ApiError> {
    if ids.is_empty() {
        return Err(ApiError::bad("agents is empty: name the agents whose branches to merge"));
    }
    let mut done = Vec::new();
    for id in ids {
        let a = sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
        let dir = PathBuf::from(&a.cwd);
        let branch = a.branch.clone().ok_or_else(|| ApiError::conflict(format!("{} has no branch of its own", a.name)))?;
        if !dir.is_dir() {
            return Err(ApiError::conflict(format!("{}'s worktree {} is gone", a.name, a.cwd)));
        }
        // The gate (the page shows the same): decisions confirmed, and failing
        // tests or a high risk only with the branch's name typed.
        let mut missing = Vec::new();
        let part = crate::capsule::part(sup, &a, &mut missing)?;
        let decisions = decisions_of(sup, &a, &part);
        let tests = tests_for(sup, &a, &part, &branch);
        let g = gate(&branch, &tests, &decisions, part["blast"]["risk"]["level"].as_str().unwrap_or(""), &[], None);
        if g["unconfirmed"].as_u64().unwrap_or(0) > 0 {
            return Err(ApiError::new(409, "unconfirmed", format!("{} replaces an earlier decision: confirm it is still right (or change it) before merging {branch}.", plural(g["unconfirmed"].as_u64().unwrap_or(0) as usize, "decision"))));
        }
        if g["anyway"] == true && !anyway.iter().any(|b| b.trim() == branch) {
            let why: Vec<&str> = g["warn"].as_array().into_iter().flatten().filter_map(Json::as_str).collect();
            return Err(ApiError::new(409, "gated", format!("{}: send {} back, or type the branch name {branch} to merge anyway.", why.join(" and "), a.name)));
        }
        let main = main_of(&a);
        let onto = blast::landing_branch(&main, &a).ok_or_else(|| ApiError::conflict(format!("there is no branch for {} to land on (no main or master)", a.name)))?;
        let dirty = uncommitted(&dir);
        if !dirty.is_empty() {
            return Err(ApiError::new(409, "uncommitted", format!("{} has {} not committed ({}): send it back to commit, or commit in its worktree. Nothing was merged{}.", a.name, plural(dirty.len(), "file"), top(&dirty, 3), if done.is_empty() { "" } else { " after the branches above" })));
        }
        let head = git::rev_parse(&dir, "HEAD").ok_or_else(|| ApiError::conflict(format!("{} has no commit", a.name)))?;
        let tip = git::rev_parse(&main, &format!("refs/heads/{onto}")).ok_or_else(|| ApiError::conflict(format!("no branch {onto}")))?;
        if git::is_ancestor(&main, &head, &tip) {
            done.push(json!({ "agent": { "id": a.id, "name": a.name }, "branch": branch, "onto": onto, "commit": tip, "how": "already" }));
            continue;
        }
        match blast::merges_clean(&main, &tip, &head) {
            Some((true, _)) => {}
            Some((false, c)) => return Err(ApiError::new(409, "conflict", format!("{branch} conflicts with {onto} in {}: send {} back to rebase on {onto}.", top(&c, 3), a.name))),
            None => return Err(ApiError::internal("git merge-tree failed (git 2.38 or newer is needed)")),
        }
        let msg = format!("Merge {branch} ({})", a.name);
        let id_args = git_id(&main);
        let (commit, how) = if git::current_branch(&main).as_deref() == Some(onto.as_str()) {
            let changed = git_out(&main, &["status", "--porcelain", "--untracked-files=no"]).unwrap_or_default();
            if !changed.trim().is_empty() {
                return Err(ApiError::new(409, "dirty", format!("{} has changes not committed on {onto}: commit or stash them, then merge.", main.display())));
            }
            run_git(&main, &id_args, &["merge", "--no-ff", "--no-edit", "-m", &msg, &branch]).map_err(|e| ApiError::conflict(format!("git merge failed: {e}")))?;
            (git::rev_parse(&main, "HEAD").unwrap_or_default(), "merge")
        } else {
            // The landing branch is not checked out: write the merge without a worktree.
            let tree = run_git(&main, &[], &["merge-tree", "--write-tree", &tip, &head]).map_err(|e| ApiError::conflict(format!("git merge-tree failed: {e}")))?;
            let tree = tree.lines().next().unwrap_or("").to_string();
            let c = run_git(&main, &id_args, &["commit-tree", &tree, "-p", &tip, "-p", &head, "-m", &msg]).map_err(|e| ApiError::internal(format!("git commit-tree failed: {e}")))?;
            run_git(&main, &[], &["update-ref", &format!("refs/heads/{onto}"), &c, &tip]).map_err(|e| ApiError::conflict(format!("{onto} moved while merging: {e}")))?;
            (c, "ref")
        };
        let rec = json!({ "agent": { "id": a.id, "name": a.name }, "branch": branch, "onto": onto, "commit": commit, "before": tip, "how": how, "where": main, "ts": crate::db::now_ms() });
        sup.set_mark(&merge_key(&a.id), &rec.to_string())?;
        let _ = sup.note(&a.id, "merged", format!("merged {branch} into {onto} ({})", short(&commit)), Some(rec.to_string()), |_| {});
        done.push(rec);
    }
    let names: Vec<&str> = done.iter().filter(|d| d["how"] != "already").filter_map(|d| d["branch"].as_str()).collect();
    let onto = done.last().and_then(|d| d["onto"].as_str()).unwrap_or("main").to_string();
    let message = if names.is_empty() { format!("Already in {onto}: nothing to merge.") } else { format!("Merged {} into {onto}.", names.join(", then ")) };
    Ok(json!({ "ok": true, "merged": done, "message": message }))
}

/// `POST /api/review/undo {agent}`: takes the agent's last merge back, while
/// the landing branch is still at it and it was not pushed: the branch
/// returns to where it was (`git reset --keep` when it is checked out, which
/// keeps your other changes and refuses rather than lose one).
pub fn undo(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let m = last_merge(sup, id);
    if m.is_null() {
        return Err(ApiError::not_found("Workbench merged nothing of this agent's"));
    }
    let (onto, before, after) = (m["onto"].as_str().unwrap_or("main").to_string(), m["before"].as_str().unwrap_or("").to_string(), m["commit"].as_str().unwrap_or("").to_string());
    let main = PathBuf::from(m["where"].as_str().unwrap_or(""));
    if m["undone"] == true {
        return Err(ApiError::conflict("this merge was undone already"));
    }
    if m["pushed"] == true {
        return Err(ApiError::new(409, "pushed", format!("the merge {} was pushed: take it back with `git revert -m 1 {}` instead.", short(&after), short(&after))));
    }
    if m["can_undo"] != true {
        return Err(ApiError::new(409, "moved", format!("{onto} has moved on since the merge, so Undo would lose that work: revert it with `git revert -m 1 {}`.", short(&after))));
    }
    if git::current_branch(&main).as_deref() == Some(onto.as_str()) {
        run_git(&main, &[], &["reset", "--keep", &before]).map_err(|e| ApiError::conflict(format!("git reset --keep refused: {e}")))?;
    } else {
        run_git(&main, &[], &["update-ref", &format!("refs/heads/{onto}"), &before, &after]).map_err(|e| ApiError::conflict(format!("{onto} moved: {e}")))?;
    }
    let mut rec = m.clone();
    rec["undone"] = json!(true);
    for k in ["can_undo", "pushed"] {
        rec.as_object_mut().map(|o| o.remove(k));
    }
    sup.set_mark(&merge_key(id), &rec.to_string())?;
    let _ = sup.note(id, "merge-undone", format!("undid the merge of {} into {onto}", m["branch"].as_str().unwrap_or("")), Some(rec.to_string()), |_| {});
    Ok(json!({ "ok": true, "onto": onto, "at": before, "message": format!("{onto} is back at {}; the branch {} is unchanged.", short(&before), m["branch"].as_str().unwrap_or("")) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mechanical_changes_are_told_apart() {
        let h = |lines: Json| json!({ "lines": lines });
        assert_eq!(mechanical_hunk(&h(json!([{ "kind": "-", "text": "import os" }, { "kind": "+", "text": "import os, sys" }]))), Some("imports"));
        assert_eq!(mechanical_hunk(&h(json!([{ "kind": "-", "text": "x=f( a,b )" }, { "kind": "+", "text": "x = f(a, b)" }]))), Some("formatting"));
        assert_eq!(mechanical_hunk(&h(json!([{ "kind": "-", "text": "return round(x)" }, { "kind": "+", "text": "return int(x)" }]))), None);
        assert!(lockfile("web/package-lock.json") && lockfile("Cargo.lock") && !lockfile("cart.py"));
        assert!(entry_point("serve.py") && entry_point("web/index.html") && entry_point("src/main.rs") && !entry_point("money.py"));
        assert_eq!(top(&["a".into(), "b".into(), "c".into(), "d".into()], 2), "a, b and 2 more");
    }
}
