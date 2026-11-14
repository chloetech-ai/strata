//! Time travel (SPEC.md, "Fourth build", row 8): what Mission and Map
//! showed at any minute of the last day.
//!
//! The state database is copied on a timer (every 10 minutes, when
//! anything happened since the last copy) and at each change of a goal or
//! an outcome, with ChloeDB's checkpoint in the directory layout under
//! `<data-dir>/snapshots/<id>/`, each hard-linking the segments of the one
//! before that no merge replaced (as `chloectx/src/snapshots.rs` does).
//! A row of `time_snapshots` names each, with what each agent's worktree
//! held then (`heads`): its branch, its HEAD, its uncommitted changes as a
//! commit no branch names (`tree`, written through a temporary index, so
//! the agent's own index is not touched), its diff from where it started
//! and what it had read. Snapshots and their rows are kept a day.
//!
//! `GET /api/time?at=` opens the snapshot nearest that time read-only and
//! answers what Mission's and Map's routes would have: goals, outcomes,
//! questions and rules from the snapshot's tables; the pulse from the live
//! events (they are never changed, only added); what ChloeCtx learned, up
//! to then; and the map of the work the snapshot kept, through
//! `map::map_of` (the tree is the repository's files now).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::db::{get_ms, get_str, now_ms, ts, Agent, StateDb};
use crate::goals::{self, Goal, Outcome, GOAL_COLS, OUTCOME_COLS};
use crate::map::{self, Work};
use crate::questions::{Question, Rule, Q_COLS, R_COLS};
use crate::server::{blocking, ApiResult, App};
use crate::supervisor::{ApiError, Supervisor};
use crate::trail;

/// How often a snapshot is taken when anything happened.
pub const EVERY_MS: i64 = 10 * 60 * 1000;
/// How long snapshots are kept.
pub const KEEP_MS: i64 = 24 * 3600 * 1000;
/// How often goals and outcomes are looked at for a change.
const GOAL_CHECK: Duration = Duration::from_secs(5);

pub fn routes() -> Router<App> {
    Router::new().route("/api/time", get(time_route)).route("/api/time/snapshot", post(snapshot_route))
}

#[derive(Deserialize)]
struct TimeQ {
    at: Option<String>,
    repo: Option<String>,
}

async fn time_route(State(app): State<App>, Query(q): Query<TimeQ>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || match q.at.as_deref().filter(|a| !a.is_empty()) {
        Some(at) => {
            let at = parse_at(at).ok_or_else(|| ApiError::bad(format!("at is a time in milliseconds or ISO 8601, not {at:?}")))?;
            as_of(&sup, q.repo.as_deref(), at)
        }
        None => timeline(&sup, q.repo.as_deref()),
    })
    .await?;
    Ok(AxJson(v).into_response())
}

async fn snapshot_route(State(app): State<App>) -> ApiResult {
    let sup = app.sup.clone();
    let k = blocking(move || take(&sup, "manual", None).map_err(ApiError::internal)).await?;
    Ok((StatusCode::CREATED, AxJson(json!({ "snapshot": k.to_json() }))).into_response())
}

fn parse_at(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        // Seconds or milliseconds.
        return Some(if n.abs() < 100_000_000_000 { n * 1000 } else { n });
    }
    chloedb::timestamp::parse(s).map(|t| t.div_euclid(1000))
}

// The snapshots.

#[derive(Clone, Debug)]
struct Kept {
    id: String,
    ts: i64,
    reason: String,
    detail: Option<String>,
    heads: Json,
}

impl Kept {
    fn to_json(&self) -> Json {
        json!({ "id": self.id, "ts_ms": self.ts, "reason": self.reason, "detail": self.detail })
    }
}

fn snapshots_dir(sup: &Supervisor) -> PathBuf {
    sup.cfg.data_dir.join("snapshots")
}

/// Every snapshot kept, oldest first.
fn kept(sup: &Supervisor) -> Vec<Kept> {
    let rows = sup.db().query("SELECT id, ts, reason, detail, heads FROM time_snapshots ORDER BY ts ASC LIMIT 100000", &Params::new()).unwrap_or_default();
    rows.iter()
        .map(|r| Kept {
            id: get_str(&r[0]).unwrap_or_default(),
            ts: get_ms(&r[1]).unwrap_or(0),
            reason: get_str(&r[2]).unwrap_or_default(),
            detail: get_str(&r[3]),
            heads: get_str(&r[4]).and_then(|h| serde_json::from_str(&h).ok()).unwrap_or(json!([])),
        })
        .collect()
}

/// One snapshot is taken at a time.
fn taking() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Takes a snapshot now: the worktrees' heads, then the checkpoint, then
/// its row.
fn take(sup: &Supervisor, reason: &str, detail: Option<String>) -> Result<Kept, String> {
    let _g = taking().lock().unwrap();
    let dir = snapshots_dir(sup);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let heads = heads(sup, &dir);
    let mut t = now_ms();
    let mut id = format!("t{t}");
    while dir.join(&id).exists() {
        t += 1;
        id = format!("t{t}");
    }
    let dest = dir.join(&id);
    let prev = kept(sup).iter().rev().map(|k| dir.join(&k.id)).find(|p| p.is_dir());
    let txn = sup.db().checkpoint(&dest, prev.as_deref())?;
    let k = Kept { id, ts: t, reason: reason.to_string(), detail, heads };
    let p = Params::new()
        .set("id", Value::Str(k.id.clone()))
        .set("ts", ts(k.ts))
        .set("reason", Value::Str(k.reason.clone()))
        .set("detail", k.detail.clone().map_or(Value::Null, Value::Str))
        .set("txn", Value::Int(txn as i64))
        .set("heads", Value::Str(k.heads.to_string()));
    sup.db().query("INSERT OR REPLACE INTO time_snapshots (id, ts, reason, detail, txn, heads) VALUES (:id, :ts, :reason, :detail, :txn, :heads)", &p)?;
    Ok(k)
}

/// Removes the snapshots older than a day, and folders under `snapshots/`
/// that no row names (a checkpoint a stopped server left half written).
fn prune(sup: &Supervisor) {
    let _g = taking().lock().unwrap();
    let dir = snapshots_dir(sup);
    let old = now_ms() - KEEP_MS;
    let all = kept(sup);
    let mut keep: HashSet<String> = HashSet::new();
    for k in &all {
        if k.ts < old {
            let _ = sup.db().query("DELETE FROM time_snapshots WHERE id = :id", &Params::new().set("id", Value::Str(k.id.clone())));
            let _ = std::fs::remove_dir_all(dir.join(&k.id));
        } else {
            keep.insert(k.id.clone());
        }
    }
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !keep.contains(&name) {
                let p = e.path();
                let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            }
        }
    }
}

/// The last snapshot opened, kept open for the next request.
fn opened() -> &'static Mutex<Option<(String, Arc<StateDb>)>> {
    static O: OnceLock<Mutex<Option<(String, Arc<StateDb>)>>> = OnceLock::new();
    O.get_or_init(|| Mutex::new(None))
}

fn open(sup: &Supervisor, id: &str) -> Result<Arc<StateDb>, String> {
    let mut o = opened().lock().unwrap();
    if let Some((k, db)) = o.as_ref() {
        if k == id {
            return Ok(db.clone());
        }
    }
    let db = Arc::new(StateDb::open_read_only(&snapshots_dir(sup).join(id))?);
    *o = Some((id.to_string(), db.clone()));
    Ok(db)
}

// The worktrees' heads.

fn run_git(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Option<String> {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The worktree's state as a commit on its HEAD that no branch names (git
/// keeps it at least two weeks unless pruned by hand), through a copy of
/// its index; `None` when it is HEAD's.
fn worktree_commit(cwd: &Path, head: &str, scratch: &Path) -> Option<String> {
    let index = run_git(cwd, &["rev-parse", "--git-path", "index"], &[])?;
    let index = if Path::new(&index).is_absolute() { PathBuf::from(index) } else { cwd.join(index) };
    let tmp = scratch.join(format!(".index-{}", crate::supervisor::hex(8)));
    if index.is_file() {
        std::fs::copy(&index, &tmp).ok()?;
    }
    let ix = tmp.to_string_lossy().to_string();
    let env = [("GIT_INDEX_FILE", ix.as_str())];
    let tree = run_git(cwd, &["add", "-A"], &env).and_then(|_| run_git(cwd, &["write-tree"], &env));
    let _ = std::fs::remove_file(&tmp);
    let tree = tree?;
    if run_git(cwd, &["rev-parse", &format!("{head}^{{tree}}")], &[]).as_deref() == Some(tree.as_str()) {
        return None;
    }
    let who = [
        ("GIT_AUTHOR_NAME", "Workbench"),
        ("GIT_AUTHOR_EMAIL", "workbench@localhost"),
        ("GIT_COMMITTER_NAME", "Workbench"),
        ("GIT_COMMITTER_EMAIL", "workbench@localhost"),
    ];
    // Not signed: it is Workbench's object, on no branch, and a signing
    // program could wait for a passphrase nobody types.
    run_git(cwd, &["commit-tree", "--no-gpg-sign", &tree, "-p", head, "-m", "Workbench time travel: the worktree as it was"], &who)
}

/// Each repository's agents on the map now, with their work and heads.
fn heads(sup: &Supervisor, scratch: &Path) -> Json {
    let repos: BTreeMap<String, ()> = sup.agent_rows().into_iter().map(|a| (a.repo, ())).collect();
    let mut out = Vec::new();
    for repo in repos.keys() {
        let main = PathBuf::from(repo);
        if !main.is_dir() {
            continue;
        }
        for w in map::territory(sup, &main) {
            let cwd = PathBuf::from(&w.agent.cwd);
            let running = sup.is_running(&w.agent.id);
            let head = crate::git::rev_parse(&cwd, "HEAD");
            // Only a running agent's worktree changes.
            let tree = match (&head, running) {
                (Some(h), true) => worktree_commit(&cwd, h, scratch),
                _ => None,
            };
            out.push(json!({
                "agent": w.agent.id, "name": w.agent.name, "repo": repo, "branch": w.agent.branch,
                "head": head, "tree": tree.clone().or(head.clone()), "dirty": tree.is_some(),
                "files": w.files.iter().map(|(p, a, d, s)| json!([p, a, d, s])).collect::<Vec<_>>(),
                "reading": w.reading.iter().map(|(p, t)| json!([p, t])).collect::<Vec<_>>(),
            }));
        }
    }
    Json::Array(out)
}

// Taking them: the timer and goal changes.

/// What a goal change looks like: goals' and outcomes' states.
fn signature(db: &StateDb) -> BTreeMap<String, (String, String)> {
    let mut m = BTreeMap::new();
    for r in db.query("SELECT id, state, text FROM goals LIMIT 100000", &Params::new()).unwrap_or_default() {
        m.insert(format!("g:{}", get_str(&r[0]).unwrap_or_default()), (get_str(&r[1]).unwrap_or_default(), get_str(&r[2]).unwrap_or_default()));
    }
    for r in db.query("SELECT id, state, title FROM outcomes LIMIT 100000", &Params::new()).unwrap_or_default() {
        m.insert(format!("o:{}", get_str(&r[0]).unwrap_or_default()), (get_str(&r[1]).unwrap_or_default(), get_str(&r[2]).unwrap_or_default()));
    }
    m
}

fn short(s: &str) -> String {
    crate::supervisor::one_line(s, 80)
}

/// The change from `a` to `b` in a few words: "goal set: …", "outcome
/// done: …", "outcome needs you: …".
fn describe(a: &BTreeMap<String, (String, String)>, b: &BTreeMap<String, (String, String)>) -> Option<String> {
    let mut said = Vec::new();
    for (k, (state, text)) in b {
        let what = if k.starts_with("g:") { "goal" } else { "outcome" };
        let line = match a.get(k) {
            None if what == "goal" => format!("goal set: {}", short(text)),
            None => format!("outcome added: {}", short(text)),
            Some((s, _)) if s != state => format!("{what} {}: {}", state.replace("needs_you", "needs you"), short(text)),
            Some((_, t)) if t != text => format!("{what} changed: {}", short(text)),
            _ => continue,
        };
        said.push(line);
    }
    let first = said.first()?.clone();
    Some(if said.len() > 1 { format!("{first} (+{} more)", said.len() - 1) } else { first })
}

/// Starts the snapshots' thread: a goal change is looked for every 5 s,
/// the timer is checked on the way, and old snapshots are pruned.
pub fn start(sup: &Arc<Supervisor>) {
    let sup = sup.clone();
    let _ = std::thread::Builder::new().name("time-travel".into()).spawn(move || {
        prune(&sup);
        let mut sig = signature(sup.db());
        let last = kept(&sup).last().map(|k| k.ts).unwrap_or(0);
        // The first timer snapshot comes after 10 minutes of this run.
        let mut last_at = if last > 0 { last } else { now_ms() };
        // The events since the last snapshot: none known at start but
        // those after its time.
        let mut seq_then = if last > 0 { events_since(&sup, last) } else { -1 };
        let mut pruned = now_ms();
        loop {
            std::thread::sleep(GOAL_CHECK);
            let now_sig = signature(sup.db());
            if now_sig != sig {
                let detail = describe(&sig, &now_sig);
                sig = now_sig;
                if detail.is_some() && take(&sup, "goal", detail).is_ok() {
                    last_at = now_ms();
                    seq_then = sup.db().last_seq().unwrap_or(0);
                }
            }
            let now = now_ms();
            if now - last_at >= EVERY_MS {
                let seq = sup.db().last_seq().unwrap_or(0);
                if seq != seq_then && take(&sup, "timer", None).is_ok() {
                    seq_then = seq;
                }
                last_at = now;
            }
            if now - pruned >= 5 * 60 * 1000 {
                prune(&sup);
                pruned = now;
            }
        }
    });
}

/// -1 when events were written after `t`, else the last event's number.
fn events_since(sup: &Supervisor, t: i64) -> i64 {
    let rows = sup.db().query("SELECT seq FROM events WHERE ts > :t LIMIT 1", &Params::new().set("t", ts(t))).unwrap_or_default();
    if rows.is_empty() {
        sup.db().last_seq().unwrap_or(0)
    } else {
        -1
    }
}

// The answers.

/// The agents of `main`'s repository (all agents without one).
fn repo_ids(agents: &[Agent], repo: Option<&str>) -> HashSet<String> {
    agents.iter().filter(|a| repo.is_none_or(|r| a.repo == r)).map(|a| a.id.clone()).collect()
}

/// `GET /api/time`: the snapshots kept and the events per minute of the
/// last day, for the scrubber.
fn timeline(sup: &Supervisor, repo: Option<&str>) -> Result<Json, ApiError> {
    let main = match repo.filter(|r| !r.is_empty()) {
        Some(r) => Some(trail::repo_dir(Some(r))?.to_string_lossy().to_string()),
        None => None,
    };
    let now = now_ms();
    let from = now - KEEP_MS;
    let ids = repo_ids(&sup.agent_rows(), main.as_deref());
    let mut per: BTreeMap<i64, i64> = BTreeMap::new();
    for r in sup.db().query("SELECT agent, ts FROM events WHERE ts >= :since LIMIT 1000000", &Params::new().set("since", ts(from))).unwrap_or_default() {
        if get_str(&r[0]).is_some_and(|a| ids.contains(&a)) {
            if let Some(t) = get_ms(&r[1]) {
                *per.entry(t - t.rem_euclid(60_000)).or_default() += 1;
            }
        }
    }
    Ok(json!({
        "available": true, "repo": main, "now_ms": now, "from_ms": from, "keep_ms": KEEP_MS, "every_ms": EVERY_MS,
        "snapshots": kept(sup).iter().filter(|k| k.ts >= from).map(Kept::to_json).collect::<Vec<_>>(),
        "events": per.into_iter().map(|(m, n)| json!([m, n])).collect::<Vec<_>>(),
    }))
}

fn rows_of<T>(db: &StateDb, sql: &str, p: &Params, of: impl Fn(&[Value]) -> T) -> Vec<T> {
    db.query(sql, p).unwrap_or_default().iter().map(|r| of(r)).collect()
}

/// `GET /api/time?at=`: Mission and Map as of `at`, from the snapshot
/// nearest it.
fn as_of(sup: &Supervisor, repo: Option<&str>, at: i64) -> Result<Json, ApiError> {
    let main = match repo.filter(|r| !r.is_empty()) {
        Some(r) => Some(trail::repo_dir(Some(r))?),
        None => None,
    };
    let all = kept(sup);
    let Some(k) = all.iter().min_by_key(|k| (k.ts - at).abs()) else {
        return Ok(json!({ "available": false, "at_ms": at, "why": "No snapshot is kept yet: the first is taken when a goal changes, or after 10 minutes of work." }));
    };
    let db = open(sup, &k.id).map_err(ApiError::internal)?;
    let agents = db.agents().map_err(ApiError::internal)?;
    let main = main.or_else(|| agents.last().map(|a| PathBuf::from(&a.repo))).ok_or_else(|| ApiError::bad("repo is required"))?;
    let repo = main.to_string_lossy().to_string();
    let mut missing: Vec<String> = Vec::new();

    // The agents then.
    let by_id: HashMap<String, Agent> = agents.iter().map(|a| (a.id.clone(), a.clone())).collect();
    let then_json = |a: &Agent| {
        let children: Vec<String> = agents.iter().filter(|c| c.parent.as_deref() == Some(&a.id)).map(|c| c.id.clone()).collect();
        a.to_json(a.ended.is_none() && a.state != "exited", children)
    };
    let mine: Vec<&Agent> = agents.iter().filter(|a| a.repo == repo).collect();
    let ids: HashSet<String> = mine.iter().map(|a| a.id.clone()).collect();

    // Mission.
    let p = Params::new().set("repo", Value::Str(repo.clone()));
    let goals: Vec<Goal> = rows_of(&db, &format!("SELECT {GOAL_COLS} FROM goals WHERE repo = :repo ORDER BY created DESC LIMIT 10000"), &p, Goal::of);
    let mut outcomes: Vec<Outcome> = rows_of(&db, &format!("SELECT {OUTCOME_COLS} FROM outcomes LIMIT 100000"), &Params::new(), Outcome::of);
    outcomes.sort_by_key(|o| (o.goal.clone(), o.ord, o.created));
    let questions: Vec<Question> = rows_of(&db, &format!("SELECT {Q_COLS} FROM questions WHERE repo = :repo ORDER BY created DESC LIMIT 100000"), &p, Question::of);
    let rules: Vec<Rule> = rows_of(&db, &format!("SELECT {R_COLS} FROM rules WHERE repo = :repo ORDER BY created ASC LIMIT 10000"), &p, Rule::of);
    let (learned, lmissing) = goals::learned(sup, &repo, (at - KEEP_MS) / 1000, 200);
    missing.extend(lmissing);
    let learned: Vec<Json> = learned.into_iter().filter(|l| l["ts_ms"].as_i64().is_none_or(|t| t <= at)).take(50).collect();
    let outcome_json = |o: &Outcome| {
        let mut v = o.to_json(sup);
        if let Some(a) = o.agent.as_deref().and_then(|id| by_id.get(id)) {
            v["agent_state"] = json!(a.state);
        }
        v
    };

    // Map: the work the snapshot kept.
    let work: Vec<Work> = k
        .heads
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| h["repo"].as_str() == Some(repo.as_str()))
        .filter_map(|h| {
            let agent = by_id.get(h["agent"].as_str()?)?.clone();
            let files = h["files"].as_array().into_iter().flatten().map(|f| (f[0].as_str().unwrap_or("").to_string(), f[1].as_i64().unwrap_or(0), f[2].as_i64().unwrap_or(0), f[3].as_str().unwrap_or("modified").to_string())).collect();
            let reading = h["reading"].as_array().into_iter().flatten().map(|r| (r[0].as_str().unwrap_or("").to_string(), r[1].as_i64().unwrap_or(0))).collect();
            Some(Work { agent, files, reading })
        })
        .collect();
    let collisions = map::collisions_of(&work);
    let map_json = match map::map_of(sup, &main, work, Some(at)) {
        Ok(mut m) => {
            if let Some(list) = m["agents"].as_array_mut() {
                for x in list.iter_mut() {
                    if let Some(a) = x["agent"]["id"].as_str().and_then(|id| by_id.get(id)) {
                        x["agent"] = then_json(a);
                    }
                }
            }
            m
        }
        Err(e) => {
            missing.push(e.message);
            Json::Null
        }
    };

    let detail_of = |g: &Goal| {
        let os: Vec<&Outcome> = outcomes.iter().filter(|o| o.goal == g.id).collect();
        let mut gs: Vec<&Agent> = mine.iter().copied().filter(|a| a.goal.as_deref() == Some(g.id.as_str()) || os.iter().any(|o| o.agent.as_deref() == Some(a.id.as_str()))).collect();
        gs.sort_by_key(|a| a.created);
        let gids: HashSet<String> = gs.iter().map(|a| a.id.clone()).collect();
        let owned: Vec<Outcome> = os.iter().map(|o| (*o).clone()).collect();
        json!({
            "goal": g.to_json(&owned),
            "outcomes": os.iter().map(|o| outcome_json(o)).collect::<Vec<_>>(),
            "agents": gs.iter().map(|a| then_json(a)).collect::<Vec<_>>(),
            "open_questions": questions.iter().filter(|q| q.state == "open" && q.agent.as_ref().is_some_and(|a| gids.contains(a))).count(),
            "learned": learned,
            "heads_up": collisions.iter().filter(|c| c["agents"].as_array().is_some_and(|xs| xs.iter().any(|x| x.as_str().is_some_and(|x| gids.contains(x))))).cloned().collect::<Vec<_>>(),
            "pulse": goals::pulse(sup, &gids, at),
            "missing": Vec::<String>::new(),
        })
    };
    let details: serde_json::Map<String, Json> = goals.iter().map(|g| (g.id.clone(), detail_of(g))).collect();
    let main_goal = goals.iter().find(|g| g.state == "active").or(goals.first());
    let goals_json: Vec<Json> = goals
        .iter()
        .map(|g| {
            let os: Vec<Outcome> = outcomes.iter().filter(|o| o.goal == g.id).cloned().collect();
            g.to_json(&os)
        })
        .collect();

    let heads: Vec<Json> = k
        .heads
        .as_array()
        .into_iter()
        .flatten()
        .filter(|h| h["repo"].as_str() == Some(repo.as_str()) && h["agent"].as_str().is_some_and(|a| ids.contains(a)))
        .filter(|h| by_id.get(h["agent"].as_str().unwrap_or("")).is_some_and(|a| a.ended.is_none()))
        .map(|h| json!({ "agent": h["agent"], "name": h["name"], "branch": h["branch"], "head": h["head"], "tree": h["tree"], "dirty": h["dirty"], "files": h["files"].as_array().map_or(0, Vec::len) }))
        .collect();

    Ok(json!({
        "available": true,
        "at_ms": at,
        "repo": repo,
        "snapshot": k.to_json(),
        "distance_ms": (k.ts - at).abs(),
        "heads": heads,
        "agents": mine.iter().map(|a| then_json(a)).collect::<Vec<_>>(),
        "mission": {
            "goals": goals_json,
            "detail": main_goal.map(|g| details[&g.id].clone()),
            "details": details,
            "questions": questions.iter().map(|q| q.to_json(sup)).collect::<Vec<_>>(),
            "rules": rules.iter().map(Rule::to_json).collect::<Vec<_>>(),
        },
        "map": map_json,
        "missing": missing,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_what_changed() {
        let mut a = BTreeMap::new();
        a.insert("g:g1".to_string(), ("active".to_string(), "Checkout v2".to_string()));
        a.insert("o:o1".to_string(), ("active".to_string(), "Saved carts".to_string()));
        let mut b = a.clone();
        assert_eq!(describe(&a, &b), None);
        b.insert("o:o1".to_string(), ("needs_you".to_string(), "Saved carts".to_string()));
        assert_eq!(describe(&a, &b).as_deref(), Some("outcome needs you: Saved carts"));
        b.insert("g:g2".to_string(), ("active".to_string(), "Refunds".to_string()));
        assert_eq!(describe(&a, &b).as_deref(), Some("goal set: Refunds (+1 more)"));
        assert_eq!(parse_at("1700000000"), Some(1_700_000_000_000));
        assert_eq!(parse_at("1700000000123"), Some(1_700_000_000_123));
        assert_eq!(parse_at("2023-11-14T22:13:20Z"), Some(1_700_000_000_000));
    }
}
