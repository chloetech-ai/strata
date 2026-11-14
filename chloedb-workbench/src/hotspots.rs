//! Hotspots (SPEC.md, "Fifth build: Hotspots"): the files, and where the
//! code index knows them the functions, that the agents keep correcting,
//! redoing or breaking.
//!
//! Everything a hotspot is made of is already a row of the state database:
//! the agents' edits (`file_edits`), the tests that failed after an edit
//! (`test_runs`, an `ASOF JOIN` to the edit before each failure), the
//! runtime errors put down to a file (`runtime_errors`), the user's steers
//! and corrections (`events`), the questions agents asked about it
//! (`questions`) and the incidents that name it (`incidents`), with the
//! git log's revert commits and the edits an agent put back. Each counts
//! points ([`W`]); a file's score is their sum over a window (14 days by
//! default). The scores are rows of `hotspots`, made again every minute and
//! on demand, each with its reasons, the ids of its evidence, edits per day
//! and what ChloeCtx (◆) and the user's rules say of the file.
//!
//! An agent's first Edit or Write of a hotspot in a session gets the
//! history as the PreToolUse hook's `additionalContext` ([`check`]), once per
//! session and file, with what to do first: a test to run, a rule to
//! follow, or the model the user wants for it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::db::{get_int, get_ms, get_str, iso, now_ms, ts, StateDb};
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{one_line, ApiError, Supervisor};
use crate::{map, questions, routing, search, trail};

/// The table of scores (src/db.rs makes it with the others).
pub const TABLE: &str = "CREATE TABLE IF NOT EXISTS hotspots (id KEYWORD PRIMARY KEY, repo KEYWORD, file KEYWORD, symbol KEYWORD, score FLOAT, level KEYWORD, edits INT, re_edits INT, agents INT, reverts INT, test_fails INT, flaky INT, errors INT, steers INT, questions INT, incidents INT, first_at TIMESTAMP, last_at TIMESTAMP, days INT, data TEXT OPTIONS(indexed = false), evidence TEXT OPTIONS(indexed = false), updated TIMESTAMP)";

/// How often the scores are made again.
const EVERY_S: u64 = 60;
/// A list older than this is made again when asked for.
const STALE_MS: i64 = 30_000;
/// The default window.
pub const WINDOW_DAYS: i64 = 14;
/// A file scoring less is not a hotspot.
const MIN_SCORE: f64 = 3.0;
/// The score from which an agent about to edit the file is told.
pub const TELL_SCORE: f64 = 8.0;
/// A failure, a correction or a question this soon after an agent's edit
/// is put down to the file it edited.
const NEAR_MS: i64 = 30 * 60_000;
/// ChloeCtx's records are read again after this long.
const CTX_KEEP: Duration = Duration::from_secs(120);
/// The most evidence kept of a hotspot.
const EVIDENCE_MAX: usize = 80;

/// Points per counted thing; `agents` counts each agent after the first,
/// `commits` each commit after the first (sixth build: the git log seeds
/// the score, so a file changed back and forth before any agent shows).
pub const W: [(&str, f64); 11] = [
    ("re_edits", 1.0),
    ("agents", 2.0),
    ("reverts", 3.0),
    ("test_fails", 1.5),
    ("flaky", 3.0),
    ("errors", 2.5),
    ("steers", 3.0),
    ("questions", 1.5),
    ("incidents", 2.0),
    ("commits", 1.5),
    ("fixes", 1.5),
];

fn weight(kind: &str) -> f64 {
    W.iter().find(|(k, _)| *k == kind).map_or(0.0, |(_, w)| *w)
}

pub fn level_of(score: f64) -> &'static str {
    if score >= 15.0 {
        "hot"
    } else if score >= 8.0 {
        "warm"
    } else {
        "mild"
    }
}

/// A hotspot's id: the same for a file of a repository whatever the window.
pub fn id_of(repo: &str, file: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (repo, file).hash(&mut h);
    format!("h{:08x}", h.finish() as u32)
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn times(n: usize) -> String {
    match n {
        1 => "once".into(),
        2 => "twice".into(),
        n => format!("{n} times"),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn base(f: &str) -> &str {
    f.rsplit('/').next().unwrap_or(f)
}

// ------------------------------------------------------------- the signals

/// One piece of evidence: an edit, a failure, a correction, ...
#[derive(Clone, Debug)]
struct Item {
    id: String,
    /// edit, revert, test_fail, error, steer, prompt, question, incident.
    kind: &'static str,
    ts_ms: i64,
    agent: Option<String>,
    text: String,
    detail: Option<String>,
    link: Json,
    /// The test's name (test_fail), the error's hits.
    key: Option<String>,
    n: i64,
    /// The branch it came from: the agent's, or the commit's.
    branch: Option<String>,
}

impl Item {
    fn new(id: String, kind: &'static str, ts_ms: i64, agent: Option<String>, text: String) -> Item {
        Item { id, kind, ts_ms, agent, text, detail: None, link: Json::Null, key: None, n: 1, branch: None }
    }
    fn to_json(&self) -> Json {
        json!({ "id": self.id, "kind": self.kind, "ts_ms": self.ts_ms, "ts": iso(self.ts_ms), "agent": self.agent, "text": self.text, "detail": self.detail, "link": self.link, "branch": self.branch })
    }
}

/// A test's runs in the window.
#[derive(Clone, Debug, Default)]
struct TestStat {
    runs: i64,
    failed: i64,
    flaky: bool,
    command: Option<String>,
}

/// What the rows say about one repository's files over a window.
#[derive(Default)]
struct Signals {
    /// Per file: edits per agent (count, first, last).
    edits: BTreeMap<String, BTreeMap<String, (i64, i64, i64)>>,
    /// Per file: edits per day (the day's start, ms).
    churn: HashMap<String, BTreeMap<i64, i64>>,
    /// Per file: the evidence, by kind.
    items: BTreeMap<String, Vec<Item>>,
    tests: HashMap<String, TestStat>,
    /// Every correction of the window (steers, prompts that correct, edits
    /// refused), put down to a file or not.
    corrections: HashSet<String>,
    missing: Vec<String>,
}

/// A repository's totals over a window: the sums of every file's counts
/// (sixth build: one set of rows for the header and the ranks).
#[derive(Default, Clone, Copy)]
struct Totals {
    files: i64,
    edits: i64,
    reverts: i64,
    test_fails: i64,
    errors: i64,
    steers: i64,
    commits: i64,
    fixes: i64,
}

impl Totals {
    /// A commit that touched 3 files is one commit, a test failure put
    /// down to its edit and to its test file one failure: the distinct
    /// things over every file, and every correction of the window.
    fn of(all: &[Scored], s: &Signals) -> Totals {
        let sum = |f: &dyn Fn(&Counts) -> i64| all.iter().map(|x| f(&x.counts)).sum::<i64>();
        let distinct = |kinds: &[&str], commit: bool| -> i64 {
            all.iter().flat_map(|x| x.items.iter()).filter(|i| kinds.contains(&i.kind) && (!commit || i.link.get("commit").is_some())).map(|i| i.id.as_str()).collect::<HashSet<_>>().len() as i64
        };
        let steers = distinct(&["steer", "prompt"], false);
        Totals {
            files: all.len() as i64,
            edits: sum(&|c| c.edits),
            reverts: distinct(&["revert"], false),
            test_fails: distinct(&["test_fail"], false),
            errors: distinct(&["error"], false),
            steers: steers.max(s.corrections.len() as i64),
            commits: distinct(&["commit", "fix", "revert"], true),
            fixes: distinct(&["fix"], true),
        }
    }
    fn to_json(self, hotspots: &[Json]) -> Json {
        let level = |l: &str| hotspots.iter().filter(|h| h["level"] == l).count();
        json!({ "hotspots": hotspots.len(), "hot": level("hot"), "warm": level("warm"), "mild": level("mild"), "files": self.files, "edits": self.edits, "reverts": self.reverts,
                "test_fails": self.test_fails, "errors": self.errors, "steers": self.steers, "commits": self.commits, "fixes": self.fixes })
    }
}

impl Signals {
    fn add(&mut self, file: &str, it: Item) {
        self.items.entry(file.to_string()).or_default().push(it);
    }
}

/// Words with which a prompt typed to an agent corrects it.
const CORRECTING: [&str; 22] = [
    "no", "not", "don't", "dont", "stop", "wrong", "instead", "revert", "undo", "back", "again", "still", "broke", "broken", "fix", "should", "shouldn't", "never", "keep", "leave", "why", "didn't",
];

fn corrects(text: &str) -> bool {
    text.to_lowercase().split(|c: char| !(c.is_alphanumeric() || c == '\'')).any(|w| CORRECTING.contains(&w))
}

/// The telling line of a test's error: its assertion's two sides, else
/// the first line that is not a header.
fn err_line(e: &str) -> String {
    let lines: Vec<&str> = e.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if let Some(i) = lines.iter().position(|l| l.starts_with("left:")) {
        if let Some(r) = lines.get(i + 1).filter(|l| l.starts_with("right:")) {
            return format!("{}, {}", lines[i], r);
        }
    }
    let header = |l: &str| l.starts_with("thread '") || l.starts_with("----") || l.starts_with("failures:") || l.starts_with("running ");
    one_line(lines.iter().copied().find(|l| !header(l)).or(lines.first().copied()).unwrap_or(""), 140)
}

/// Reads the signals of `repo` since `since_ms` from the state database:
/// `GROUP BY` and `date_bin` for the edits, `ASOF JOIN`s from failures,
/// corrections and questions to the edit before each. `agents` maps the
/// repository's agents to their names.
fn collect(db: &StateDb, repo: &str, since_ms: i64, agents: &HashMap<String, String>) -> Signals {
    let mut s = Signals::default();
    let p = Params::new().set("repo", Value::Str(repo.to_string())).set("since", ts(since_ms));
    let miss = |s: &mut Signals, what: &str, e: String| s.missing.push(format!("{what}: {e}"));

    // Edits: per file and agent, and per day.
    match db.query("SELECT file, agent, COUNT(*) AS n, MIN(ts) AS a, MAX(ts) AS b FROM file_edits WHERE repo = :repo AND ts >= :since GROUP BY file, agent LIMIT 100000", &p) {
        Ok(rows) => {
            for r in rows {
                let (Some(f), Some(a)) = (get_str(&r[0]), get_str(&r[1])) else { continue };
                let n = get_int(&r[2]).unwrap_or(0);
                s.edits.entry(f).or_default().insert(a, (n, get_ms(&r[3]).unwrap_or(0), get_ms(&r[4]).unwrap_or(0)));
            }
        }
        Err(e) => miss(&mut s, "file_edits could not be read", e),
    }
    match db.query("SELECT file, date_bin('1 day', ts) AS d, COUNT(*) AS n FROM file_edits WHERE repo = :repo AND ts >= :since GROUP BY file, d LIMIT 100000", &p) {
        Ok(rows) => {
            for r in rows {
                let (Some(f), Some(d)) = (get_str(&r[0]), get_ms(&r[1])) else { continue };
                s.churn.entry(f).or_default().insert(d, get_int(&r[2]).unwrap_or(0));
            }
        }
        Err(e) => miss(&mut s, "edits per day could not be read", e),
    }
    let mut edits_of: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    for r in db.query("SELECT id, file, agent, tool, ts FROM file_edits WHERE repo = :repo AND ts >= :since ORDER BY ts DESC LIMIT 20000", &p).unwrap_or_default() {
        let (Some(id), Some(f), Some(a)) = (get_str(&r[0]), get_str(&r[1]), get_str(&r[2])) else { continue };
        let at = get_ms(&r[4]).unwrap_or(0);
        let tool = get_str(&r[3]).unwrap_or_else(|| "Edit".into());
        edits_of.entry(a.clone()).or_default().push((at, f.clone()));
        let mut it = Item::new(id, "edit", at, Some(a.clone()), format!("{tool} {}", base(&f)));
        it.detail = Some(format!("{tool} of {f} by {}", agents.get(&a).map_or(a.as_str(), String::as_str)));
        it.link = json!({ "story": { "agent": a, "at_ms": at } });
        s.add(&f, it);
    }
    for v in edits_of.values_mut() {
        v.sort();
    }
    // The edit of `agent` at or before `at`, within NEAR_MS: what the join
    // gives, over the rows in order when it cannot run.
    let before = |agent: &str, at: i64| -> Option<(String, i64)> {
        let v = edits_of.get(agent)?;
        let i = v.partition_point(|(t, _)| *t <= at);
        let (t, f) = v.get(i.checked_sub(1)?)?;
        (at - t <= NEAR_MS).then(|| (f.clone(), *t))
    };

    // Tests: each one's runs, which are flaky (passed and failed on the
    // same code), and the failures after an edit.
    for r in db.query("SELECT test, COUNT(*) AS runs, COUNT(*) FILTER (WHERE outcome = 'failed') AS failed FROM test_runs WHERE repo = :repo AND ts >= :since GROUP BY test LIMIT 100000", &p).unwrap_or_default() {
        let Some(t) = get_str(&r[0]) else { continue };
        let failed = get_int(&r[2]).unwrap_or(0);
        s.tests.insert(t, TestStat { runs: get_int(&r[1]).unwrap_or(0), failed, ..TestStat::default() });
    }
    match db.query("SELECT test, \"commit\", tree, COUNT(*) FILTER (WHERE outcome = 'passed') AS p, COUNT(*) FILTER (WHERE outcome = 'failed') AS f FROM test_runs WHERE repo = :repo AND ts >= :since GROUP BY test, \"commit\", tree HAVING p > 0 AND f > 0 LIMIT 10000", &p) {
        Ok(rows) => {
            for r in rows {
                if let Some(t) = get_str(&r[0]).and_then(|t| s.tests.get_mut(&t)) {
                    t.flaky = true;
                }
            }
        }
        Err(e) => miss(&mut s, "flaky tests could not be found", e),
    }
    let fails = db.query(
        "SELECT t.id, t.run, t.test, t.agent, t.ts, t.command, t.error, e.file, e.ts FROM test_runs t ASOF JOIN file_edits e MATCH_CONDITION (t.ts >= e.ts) ON t.agent = e.agent WHERE t.repo = :repo AND t.ts >= :since AND t.outcome = 'failed' LIMIT 100000",
        &p,
    );
    let fails = match fails {
        Ok(rows) => rows.into_iter().map(|r| (r[..7].to_vec(), get_str(&r[7]).zip(get_ms(&r[8])))).collect::<Vec<_>>(),
        Err(e) => {
            s.missing.push(format!("ASOF JOIN of failures to edits failed ({e}); they come from an ordered lookup"));
            db.query("SELECT id, run, test, agent, ts, command, error FROM test_runs WHERE repo = :repo AND ts >= :since AND outcome = 'failed' LIMIT 100000", &p)
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    let b = get_str(&r[3]).and_then(|a| before(&a, get_ms(&r[4]).unwrap_or(0)));
                    (r, b)
                })
                .collect()
        }
    };
    for (r, edit) in fails {
        let (Some(id), Some(test), Some(agent)) = (get_str(&r[0]), get_str(&r[2]), get_str(&r[3])) else { continue };
        let at = get_ms(&r[4]).unwrap_or(0);
        if let Some(c) = get_str(&r[5]) {
            s.tests.entry(test.clone()).or_default().command.get_or_insert(c);
        }
        // Put down to the file edited just before, and to the test's own
        // file: a test file whose tests fail is trouble too.
        let edit = edit.filter(|(_, t)| at - t <= NEAR_MS && at >= *t);
        let own = test_file(&test).filter(|f| s.edits.contains_key(*f) || FsPath::new(repo).join(f).is_file()).map(str::to_string);
        if edit.is_none() && own.is_none() {
            continue;
        }
        let mut it = Item::new(id, "test_fail", at, Some(agent), format!("{test} failed"));
        let err = get_str(&r[6]).map(|e| err_line(&e));
        let after = edit.as_ref().map(|(_, edit_ms)| {
            let mins = (at - edit_ms) / 60_000;
            format!("{} after the edit", if mins < 1 { "under a minute".into() } else { plural(mins as usize, "min", "min") })
        });
        it.detail = Some([err, after].into_iter().flatten().collect::<Vec<_>>().join(" · ")).filter(|d| !d.is_empty());
        it.link = json!({ "test": test, "run": get_str(&r[1]) });
        it.key = Some(test);
        if let Some((file, _)) = &edit {
            s.add(file, it.clone());
        }
        if let Some(o) = own.filter(|o| edit.as_ref().is_none_or(|(f, _)| f != o)) {
            s.add(&o, it);
        }
    }

    // Runtime errors put down to a file.
    match db.query("SELECT id, agent, file, message, hits, first_ms, last_ms, line, source FROM runtime_errors WHERE repo = :repo AND last_ms >= :since_ms AND file IS NOT NULL LIMIT 10000", &p.clone().set("since_ms", Value::Int(since_ms))) {
        Ok(rows) => {
            for r in rows {
                let (Some(id), Some(file)) = (get_str(&r[0]), get_str(&r[2])) else { continue };
                let file = file.trim_start_matches('/').to_string();
                let hits = get_int(&r[4]).unwrap_or(1).max(1);
                let msg = one_line(&get_str(&r[3]).unwrap_or_default(), 160);
                let mut it = Item::new(id.clone(), "error", get_int(&r[5]).unwrap_or(0), get_str(&r[1]), msg);
                it.detail = Some(format!("{} at {file}:{} · {}", get_str(&r[8]).unwrap_or_else(|| "error".into()), get_int(&r[7]).unwrap_or(0), plural(hits as usize, "hit", "hits")));
                it.link = json!({ "error": id });
                it.n = hits;
                s.add(&file, it);
            }
        }
        Err(e) => miss(&mut s, "runtime errors could not be read", e),
    }

    // The user's corrections: steers, and prompts typed to an agent soon
    // after its edit that correct it.
    let mut steer_texts: HashSet<String> = HashSet::new();
    let ev = db.query(
        "SELECT v.id, v.agent, v.kind, v.ts, v.data, e.file, e.ts FROM events v ASOF JOIN file_edits e MATCH_CONDITION (v.ts >= e.ts) ON v.agent = e.agent WHERE v.ts >= :since AND v.kind IN ('steer', 'UserPromptSubmit') LIMIT 100000",
        &p,
    );
    let ev = match ev {
        Ok(rows) => rows.into_iter().map(|r| (r[..5].to_vec(), get_str(&r[5]).zip(get_ms(&r[6])))).collect::<Vec<_>>(),
        Err(e) => {
            s.missing.push(format!("ASOF JOIN of steers to edits failed ({e}); they come from an ordered lookup"));
            db.query("SELECT id, agent, kind, ts, data FROM events WHERE ts >= :since AND kind IN ('steer', 'UserPromptSubmit') LIMIT 100000", &p)
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    let b = get_str(&r[1]).and_then(|a| before(&a, get_ms(&r[3]).unwrap_or(0)));
                    (r, b)
                })
                .collect()
        }
    };
    // The edit of `agent` after `at`, within NEAR_MS: what a correction
    // typed before the agent touched anything led to.
    let after = |agent: &str, at: i64| -> Option<String> {
        let v = edits_of.get(agent)?;
        let i = v.partition_point(|(t, _)| *t < at);
        let (t, f) = v.get(i)?;
        (t - at <= NEAR_MS).then(|| f.clone())
    };
    let mut ev: Vec<_> = ev.into_iter().filter(|(r, _)| get_str(&r[1]).is_some_and(|a| agents.contains_key(&a))).collect();
    // Steers first: a steer typed to an idle agent is its prompt too.
    ev.sort_by_key(|(r, _)| (get_str(&r[2]).as_deref() != Some("steer"), get_ms(&r[3]).unwrap_or(0)));
    for (r, edit) in ev {
        let (Some(id), Some(agent), Some(kind)) = (get_str(&r[0]), get_str(&r[1]), get_str(&r[2])) else { continue };
        let at = get_ms(&r[3]).unwrap_or(0);
        let d: Json = get_str(&r[4]).and_then(|d| serde_json::from_str(&d).ok()).unwrap_or(Json::Null);
        let text = if kind == "steer" { d["text"].as_str() } else { d["prompt"].as_str() }.unwrap_or("").trim().to_string();
        if text.is_empty() {
            continue;
        }
        if kind == "steer" {
            steer_texts.insert(text.clone());
        } else if steer_texts.contains(&text) || !corrects(&text) {
            continue;
        }
        let cid = if kind == "steer" { d["id"].as_str().map(str::to_string).unwrap_or_else(|| id.clone()) } else { id.clone() };
        s.corrections.insert(cid);
        let file = edit.filter(|(_, t)| at - t <= NEAR_MS && at >= *t).map(|(f, _)| f).or_else(|| known_file_in(&text, repo, &s)).or_else(|| after(&agent, at));
        let Some(file) = file else { continue };
        let name = agents.get(&agent).cloned().unwrap_or_else(|| agent.clone());
        let mut it = Item::new(if kind == "steer" { d["id"].as_str().map(str::to_string).unwrap_or(id) } else { id }, if kind == "steer" { "steer" } else { "prompt" }, at, None, one_line(&text, 300));
        it.detail = Some(format!("you, to {name}{}", if kind == "steer" { "" } else { " (typed in its terminal)" }));
        it.link = json!({ "story": { "agent": agent, "at_ms": at } });
        it.key = Some(agent);
        s.add(&file, it);
    }

    // An edit you refused at its permission prompt ("No"): a correction of
    // the file it would have changed.
    let refused = db.query("SELECT id, agent, context, text, answer, answered FROM questions WHERE repo = :repo AND kind = 'permission' AND answered >= :since LIMIT 10000", &p).unwrap_or_default();
    for r in refused {
        let (Some(id), Some(agent)) = (get_str(&r[0]), get_str(&r[1])) else { continue };
        let answer = get_str(&r[4]).unwrap_or_default();
        if !answer.trim_start().to_ascii_lowercase().starts_with("no") || !agents.contains_key(&agent) {
            continue;
        }
        let words = format!("{} {}", get_str(&r[2]).unwrap_or_default(), get_str(&r[3]).unwrap_or_default());
        s.corrections.insert(id.clone());
        let Some(file) = known_file_in(&words, repo, &s) else { continue };
        let at = get_ms(&r[5]).unwrap_or(0);
        let name = agents.get(&agent).cloned().unwrap_or_else(|| agent.clone());
        let mut it = Item::new(id.clone(), "steer", at, None, format!("No: {}", one_line(&words, 200)));
        it.detail = Some(format!("you refused {name}'s change at its permission prompt"));
        it.link = json!({ "question": id });
        it.key = Some(agent);
        s.add(&file, it);
    }

    // Questions to the user while working on a file, or naming it.
    let qs = db.query(
        "SELECT q.id, q.agent, q.text, q.created, q.answer, q.context, e.file, e.ts FROM questions q ASOF JOIN file_edits e MATCH_CONDITION (q.created >= e.ts) ON q.agent = e.agent WHERE q.repo = :repo AND q.created >= :since AND q.kind = 'ask' LIMIT 10000",
        &p,
    );
    let qs = match qs {
        Ok(rows) => rows.into_iter().map(|r| (r[..6].to_vec(), get_str(&r[6]).zip(get_ms(&r[7])))).collect::<Vec<_>>(),
        Err(e) => {
            s.missing.push(format!("ASOF JOIN of questions to edits failed ({e}); they come from an ordered lookup"));
            db.query("SELECT id, agent, text, created, answer, context FROM questions WHERE repo = :repo AND created >= :since AND kind = 'ask' LIMIT 10000", &p)
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    let b = get_str(&r[1]).and_then(|a| before(&a, get_ms(&r[3]).unwrap_or(0)));
                    (r, b)
                })
                .collect()
        }
    };
    let known: Vec<String> = s.edits.keys().cloned().collect();
    for (r, edit) in qs {
        let Some(id) = get_str(&r[0]) else { continue };
        let at = get_ms(&r[3]).unwrap_or(0);
        let text = get_str(&r[2]).unwrap_or_default();
        let words = format!("{text}\n{}", get_str(&r[5]).unwrap_or_default());
        let mut files: Vec<String> = known.iter().filter(|f| words.contains(f.as_str())).cloned().collect();
        if let Some((f, _)) = edit.filter(|(_, t)| at - t <= NEAR_MS && at >= *t) {
            if !files.contains(&f) {
                files.push(f);
            }
        }
        for f in files {
            let mut it = Item::new(id.clone(), "question", at, get_str(&r[1]), one_line(&text, 200));
            it.detail = get_str(&r[4]).map(|a| format!("you answered “{}”", one_line(&a, 120)));
            it.link = json!({ "question": id });
            s.add(&f, it);
        }
    }

    // Incidents naming a file, or a test of a file (their firings').
    let mut test_files: HashMap<String, Vec<String>> = HashMap::new();
    for r in db.query("SELECT incident, entity, detail FROM alert_firings WHERE repo = :repo AND last_seen >= :since LIMIT 100000", &p).unwrap_or_default() {
        let Some(i) = get_str(&r[0]) else { continue };
        for t in [get_str(&r[1]), get_str(&r[2])].into_iter().flatten() {
            if let Some(f) = test_file(&t).filter(|f| s.edits.contains_key(*f) || FsPath::new(repo).join(f).is_file()) {
                let v = test_files.entry(i.clone()).or_default();
                if !v.iter().any(|x| x == f) {
                    v.push(f.to_string());
                }
            }
        }
    }
    for r in db.query("SELECT id, title, files, started, agents FROM incidents WHERE repo = :repo AND updated >= :since LIMIT 10000", &p).unwrap_or_default() {
        let Some(id) = get_str(&r[0]) else { continue };
        let mut files: Vec<String> = get_str(&r[2]).and_then(|f| serde_json::from_str(&f).ok()).unwrap_or_default();
        for f in test_files.remove(&id).unwrap_or_default() {
            if !files.contains(&f) {
                files.push(f);
            }
        }
        let first_agent: Option<String> = get_str(&r[4]).and_then(|a| serde_json::from_str::<Vec<String>>(&a).ok()).and_then(|a| a.into_iter().next());
        for f in files {
            let mut it = Item::new(id.clone(), "incident", get_ms(&r[3]).unwrap_or(0), first_agent.clone(), one_line(&get_str(&r[1]).unwrap_or_default(), 200));
            it.link = json!({ "incident": id });
            s.add(&f, it);
        }
    }
    s
}

/// Edits an agent put back: an Edit whose old and new text are a former
/// edit's of the same file the other way round. From the PostToolUse
/// events' inputs.
fn undone_edits(sup: &Supervisor, repo: &str, since_ms: i64, s: &mut Signals) {
    let rows = sup.db().query("SELECT id, agent, ts, tool, data FROM events WHERE kind = 'PostToolUse' AND tool IN ('Edit', 'MultiEdit') AND ts >= :since ORDER BY seq DESC LIMIT 5000", &Params::new().set("since", ts(since_ms))).unwrap_or_default();
    let mut agents: HashMap<String, Option<crate::db::Agent>> = HashMap::new();
    // (file) -> earlier edits: (old, new, agent, ms)
    let mut seen: HashMap<String, Vec<(String, String, String, i64)>> = HashMap::new();
    for r in rows.iter().rev() {
        let (Some(id), Some(agent)) = (get_str(&r[0]), get_str(&r[1])) else { continue };
        let Some(a) = agents.entry(agent.clone()).or_insert_with(|| sup.agent_row(&agent)).clone() else { continue };
        if a.repo != repo {
            continue;
        }
        let Some(d) = get_str(&r[4]).and_then(|d| serde_json::from_str::<Json>(&d).ok()) else { continue };
        let input = &d["tool_input"];
        let Some(rel) = input["file_path"].as_str().and_then(|fp| map::rel_path(fp, FsPath::new(&a.cwd), FsPath::new(&a.repo))) else { continue };
        let at = get_ms(&r[2]).unwrap_or(0);
        let pairs: Vec<(String, String)> = match input["edits"].as_array() {
            Some(list) => list.iter().map(|e| (e["old_string"].as_str().unwrap_or("").to_string(), e["new_string"].as_str().unwrap_or("").to_string())).collect(),
            None => vec![(input["old_string"].as_str().unwrap_or("").to_string(), input["new_string"].as_str().unwrap_or("").to_string())],
        };
        let earlier = seen.entry(rel.clone()).or_default();
        for (old, new) in pairs.into_iter().filter(|(o, n)| o != n && !(o.trim().is_empty() && n.trim().is_empty())) {
            if let Some((_, _, who, was)) = earlier.iter().rev().find(|(o, n, _, _)| *o == new && *n == old) {
                let own = *who == a.id;
                let mins = ((at - was) / 60_000).max(0);
                let mut it = Item::new(id.clone(), "revert", at, Some(a.id.clone()), if own { "Put back its own edit".into() } else { "Put back another agent's edit".into() });
                it.detail = Some(format!("{} → {}, the text of {} before", one_line(&old, 50), one_line(&new, 50), if mins < 1 { "a moment".into() } else { plural(mins as usize, "min", "min") }));
                it.link = json!({ "story": { "agent": a.id, "at_ms": at } });
                it.branch = a.branch.clone();
                s.items.entry(rel.clone()).or_default().push(it);
            }
            earlier.push((old, new, a.id.clone(), at));
        }
    }
}

fn git_out(dir: &FsPath, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").stdin(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// A file of the repository a text names: one the agents edited, else a
/// path in it that exists in the checkout.
fn known_file_in(text: &str, repo: &str, s: &Signals) -> Option<String> {
    if let Some(f) = s.edits.keys().find(|f| text.contains(f.as_str())) {
        return Some(f.clone());
    }
    let prefix = format!("{}/", repo.trim_end_matches('/'));
    text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ',' | '(' | ')'))
        .map(|w| w.trim_end_matches([':', '.', '?']))
        .filter(|w| w.contains('.') && !w.starts_with('-'))
        .map(|w| w.strip_prefix(&prefix).unwrap_or(w).trim_start_matches("./").to_string())
        .find(|w| !w.starts_with('/') && FsPath::new(repo).join(w).is_file())
}

/// The file of a test named as its runner names it: `tests/test_x.py` of
/// `tests/test_x.py::test_y` (pytest, and the junit reports that name a
/// file), `None` for a name without one (`module::tests::name`).
fn test_file(test: &str) -> Option<&str> {
    let (f, _) = test.split_once("::")?;
    let ext = f.rsplit_once('.').map(|(_, e)| e)?;
    (!f.is_empty() && !f.contains(char::is_whitespace) && !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric())).then_some(f)
}

/// Words with which a commit's subject says it fixes something.
const FIXING: [&str; 9] = ["fix", "fixes", "fixed", "bug", "broken", "wrong", "regression", "hotfix", "correct"];

fn is_fix(subject: &str) -> bool {
    subject.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| FIXING.contains(&w))
}

/// The git log since `since_ms` (sixth build: it seeds the scores, so a
/// file changed back and forth shows before any agent touches it):
/// revert commits on any branch, and every other commit (no merges) with
/// the files it touched, each with its branch: the main checkout's when
/// its HEAD has it, else the branch the log reached it from.
fn git_log(main: &FsPath, since_ms: i64, s: &mut Signals) {
    let since = format!("--since=@{}", since_ms / 1000);
    let head_branch = git_out(main, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|b| b.trim().to_string()).filter(|b| !b.is_empty() && b != "HEAD").unwrap_or_else(|| "main".into());
    let on_head: HashSet<String> = git_out(main, &["rev-list", "HEAD", &since]).unwrap_or_default().lines().map(str::to_string).collect();
    let Some(out) = git_out(main, &["log", "--all", "--source", "--no-merges", &since, "--name-only", "--format=%x1e%H%x1f%ct%x1f%S%x1f%s"]) else { return };
    for c in out.split('\x1e').filter(|c| !c.trim().is_empty()) {
        let mut lines = c.lines();
        let head: Vec<&str> = lines.next().unwrap_or("").split('\x1f').collect();
        if head.len() < 4 {
            continue;
        }
        let (sha, at, source, subject) = (head[0], head[1].parse::<i64>().unwrap_or(0) * 1000, head[2], head[3]);
        let branch = if on_head.contains(sha) { head_branch.clone() } else { source.trim_start_matches("refs/heads/").trim_start_matches("refs/remotes/").to_string() };
        let short: String = sha.chars().take(8).collect();
        let kind = if subject.to_lowercase().starts_with("revert") {
            "revert"
        } else if is_fix(subject) {
            "fix"
        } else {
            "commit"
        };
        for f in lines.map(str::trim).filter(|l| !l.is_empty()) {
            let mut it = Item::new(short.clone(), kind, at, None, one_line(subject, 160));
            it.detail = Some(format!("commit {short} on {branch}"));
            it.link = json!({ "commit": sha });
            it.key = Some(sha.to_string());
            it.branch = Some(branch.clone());
            s.items.entry(f.to_string()).or_default().push(it);
        }
    }
}

// ----------------------------------------------------------------- scoring

/// One file's counts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    pub edits: i64,
    pub re_edits: i64,
    pub agents: i64,
    pub reverts: i64,
    pub test_fails: i64,
    pub flaky: i64,
    pub errors: i64,
    pub steers: i64,
    pub questions: i64,
    pub incidents: i64,
    /// Commits that touched it in the window (git log, any branch), and
    /// those whose subject says they fix something.
    pub commits: i64,
    pub fixes: i64,
    /// Failures after edits of a flaky test (not scored as failures, but
    /// counted in the header's "test failures").
    pub flaky_fails: i64,
}

impl Counts {
    fn get(&self, k: &str) -> i64 {
        match k {
            "re_edits" => self.re_edits,
            "agents" => self.agents,
            "reverts" => self.reverts,
            "test_fails" => self.test_fails,
            "flaky" => self.flaky,
            "errors" => self.errors,
            "steers" => self.steers,
            "questions" => self.questions,
            "incidents" => self.incidents,
            "commits" => self.commits,
            "fixes" => self.fixes,
            _ => 0,
        }
    }
    fn to_json(self) -> Json {
        json!({ "edits": self.edits, "re_edits": self.re_edits, "agents": self.agents, "reverts": self.reverts, "test_fails": self.test_fails, "flaky": self.flaky, "errors": self.errors, "steers": self.steers, "questions": self.questions, "incidents": self.incidents, "commits": self.commits, "fixes": self.fixes })
    }
    pub fn score(&self) -> f64 {
        round1(W.iter().map(|(k, w)| w * if matches!(*k, "agents" | "commits") { (self.get(k) - 1).max(0) } else { self.get(k) } as f64).sum())
    }
}

/// A hotspot as scored, before what ChloeCtx and the code index add.
struct Scored {
    file: String,
    counts: Counts,
    items: Vec<Item>,
    agents: Vec<String>,
    per_agent: Vec<(String, i64)>,
    first_ms: i64,
    last_ms: i64,
    tests: Vec<(String, TestStat, i64)>,
}

/// Every file with a signal, scored, best first: the hotspots are those
/// scoring [`MIN_SCORE`] or more, and the totals sum them all.
fn score_files(s: &Signals) -> Vec<Scored> {
    let files: HashSet<&String> = s.edits.keys().chain(s.items.keys()).collect();
    let mut out = Vec::new();
    for f in files {
        let per: Vec<(String, i64)> = s.edits.get(f).map(|m| m.iter().map(|(a, (n, _, _))| (a.clone(), *n)).collect()).unwrap_or_default();
        let items = s.items.get(f).cloned().unwrap_or_default();
        let of = |k: &str| items.iter().filter(|i| i.kind == k).count() as i64;
        let mut c = Counts { edits: per.iter().map(|p| p.1).sum(), re_edits: per.iter().map(|p| (p.1 - 1).max(0)).sum(), agents: per.len() as i64, ..Counts::default() };
        // A revert commit counts once per file, whatever its rows.
        c.reverts = items.iter().filter(|i| i.kind == "revert").map(|i| &i.id).collect::<HashSet<_>>().len() as i64;
        let mut by_test: BTreeMap<String, i64> = BTreeMap::new();
        for i in items.iter().filter(|i| i.kind == "test_fail") {
            *by_test.entry(i.key.clone().unwrap_or_default()).or_default() += 1;
        }
        let flaky_of = |t: &str| s.tests.get(t).is_some_and(|x| x.flaky);
        c.test_fails = by_test.iter().filter(|(t, _)| !flaky_of(t)).map(|(_, n)| n).sum();
        c.flaky = by_test.keys().filter(|t| flaky_of(t)).count() as i64;
        c.flaky_fails = by_test.iter().filter(|(t, _)| flaky_of(t)).map(|(_, n)| n).sum();
        c.errors = of("error");
        c.steers = of("steer") + of("prompt");
        c.questions = items.iter().filter(|i| i.kind == "question").map(|i| &i.id).collect::<HashSet<_>>().len() as i64;
        c.incidents = items.iter().filter(|i| i.kind == "incident").map(|i| &i.id).collect::<HashSet<_>>().len() as i64;
        c.commits = items.iter().filter(|i| matches!(i.kind, "commit" | "fix" | "revert") && i.link.get("commit").is_some()).map(|i| &i.id).collect::<HashSet<_>>().len() as i64;
        c.fixes = items.iter().filter(|i| i.kind == "fix").map(|i| &i.id).collect::<HashSet<_>>().len() as i64;
        let mut agents: Vec<(String, i64)> = per.clone();
        for i in &items {
            if let Some(a) = i.agent.as_ref().or(i.key.as_ref().filter(|_| matches!(i.kind, "steer" | "prompt"))) {
                if !agents.iter().any(|(x, _)| x == a) {
                    agents.push((a.clone(), 0));
                }
            }
        }
        agents.sort_by(|a, b| b.1.cmp(&a.1));
        let times: Vec<i64> = s.edits.get(f).map(|m| m.values().flat_map(|(_, a, b)| [*a, *b]).collect::<Vec<i64>>()).unwrap_or_default().into_iter().chain(items.iter().map(|i| i.ts_ms)).filter(|t| *t > 0).collect();
        let mut tests: Vec<(String, TestStat, i64)> = by_test.iter().map(|(t, n)| (t.clone(), s.tests.get(t).cloned().unwrap_or_default(), *n)).collect();
        tests.sort_by(|a, b| b.2.cmp(&a.2));
        let mut per_agent = per;
        per_agent.sort_by(|a, b| b.1.cmp(&a.1));
        out.push(Scored {
            file: f.clone(),
            counts: c,
            items,
            agents: agents.into_iter().map(|a| a.0).collect(),
            per_agent,
            first_ms: times.iter().copied().min().unwrap_or(0),
            last_ms: times.iter().copied().max().unwrap_or(0),
            tests,
        });
    }
    out.sort_by(|a, b| b.counts.score().total_cmp(&a.counts.score()).then(b.last_ms.cmp(&a.last_ms)));
    out
}

fn span(ms: i64) -> String {
    let h = ms / 3_600_000;
    if ms < 3_600_000 {
        plural((ms / 60_000).max(1) as usize, "min", "min")
    } else if h < 36 {
        plural(h as usize, "hour", "hours")
    } else {
        plural(((ms + 43_200_000) / 86_400_000) as usize, "day", "days")
    }
}

/// The reasons of a scored file, most points first.
fn reasons(x: &Scored, names: &HashMap<String, String>) -> Vec<Json> {
    let name = |a: &str| names.get(a).cloned().unwrap_or_else(|| a.to_string());
    let ids = |kind: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for i in x.items.iter().rev().filter(|i| kind.contains(&i.kind)) {
            if !v.contains(&i.id) {
                v.push(i.id.clone());
            }
        }
        v.truncate(12);
        v
    };
    let c = &x.counts;
    let mut out: Vec<(f64, Json)> = Vec::new();
    let mut push = |kind: &str, n: i64, text: String, ids: Vec<String>| {
        if n <= 0 || (kind == "agents" && n < 2) {
            return;
        }
        let pts = round1(weight(kind) * if kind == "agents" { n - 1 } else { n } as f64);
        out.push((pts, json!({ "kind": kind, "n": n, "text": text, "ids": ids, "points": pts })));
    };
    let who: Vec<String> = x.per_agent.iter().filter(|(_, n)| *n > 1).map(|(a, _)| name(a)).collect();
    push("re_edits", c.re_edits, format!("re-edited {}{}", times(c.re_edits as usize), if who.is_empty() { String::new() } else { format!(" by {}", who.join(" and ")) }), ids(&["edit"]));
    push("agents", c.agents, format!("{} agents edited it: {}", c.agents, x.per_agent.iter().map(|(a, _)| name(a)).collect::<Vec<_>>().join(", ")), ids(&["edit"]));
    let commits = x.items.iter().filter(|i| i.kind == "revert" && i.link.get("commit").is_some()).map(|i| &i.id).collect::<HashSet<_>>().len();
    let back = c.reverts as usize - commits.min(c.reverts as usize);
    let rtext = match (commits, back) {
        (0, b) => format!("{} put back", plural(b, "edit", "edits")),
        (k, 0) => plural(k, "revert commit", "revert commits"),
        (k, b) => format!("{} reverts: {} and {} put back", k + b, plural(k, "a revert commit", "revert commits").trim_start_matches("1 "), plural(b, "edit", "edits")),
    };
    push("reverts", c.reverts, rtext, ids(&["revert"]));
    let hard: Vec<&(String, TestStat, i64)> = x.tests.iter().filter(|t| !t.1.flaky).collect();
    let own = |t: &str| test_file(t) == Some(x.file.as_str());
    let ttext = match hard.as_slice() {
        [one] if own(&one.0) => format!("{} failed {}", one.0, times(one.2 as usize)),
        [one] => format!("{} failed {} after edits here", one.0, times(one.2 as usize)),
        many if many.iter().all(|t| own(&t.0)) => format!("{} failures of its tests, most {}", c.test_fails, many.first().map(|t| t.0.as_str()).unwrap_or("")),
        _ => format!("{} test failures after edits here{}", c.test_fails, hard.first().map(|t| format!(", most {}", t.0)).unwrap_or_default()),
    };
    push("test_fails", c.test_fails, ttext, ids(&["test_fail"]));
    let fl: Vec<&(String, TestStat, i64)> = x.tests.iter().filter(|t| t.1.flaky).collect();
    let ftext = match fl.as_slice() {
        [one] => format!("{} is flaky", one.0),
        many if many.iter().all(|t| own(&t.0)) => format!("{} of its tests are flaky", many.len()),
        _ => format!("{} flaky tests fail after edits here", fl.len()),
    };
    push("flaky", c.flaky, ftext, ids(&["test_fail"]));
    let first_err = x.items.iter().rev().find(|i| i.kind == "error");
    let hits: i64 = x.items.iter().filter(|i| i.kind == "error").map(|i| i.n).sum();
    push(
        "errors",
        c.errors,
        format!("{}: {}{}", plural(c.errors as usize, "runtime error", "runtime errors"), first_err.map(|e| one_line(&e.text, 80)).unwrap_or_default(), if hits > c.errors { format!(" ({hits} hits)") } else { String::new() }),
        ids(&["error"]),
    );
    push("steers", c.steers, format!("you corrected it {}", times(c.steers as usize)), ids(&["steer", "prompt"]));
    push("questions", c.questions, format!("{} to you", plural(c.questions as usize, "question", "questions")), ids(&["question"]));
    push("incidents", c.incidents, format!("in {}", plural(c.incidents as usize, "incident", "incidents")), ids(&["incident"]));
    if c.commits >= 2 {
        let mut branches: Vec<String> = Vec::new();
        for i in x.items.iter().filter(|i| matches!(i.kind, "commit" | "fix" | "revert")) {
            if let Some(b) = i.branch.as_ref().filter(|b| !branches.contains(b)) {
                branches.push(b.clone());
            }
        }
        let on = if branches.is_empty() { String::new() } else { format!(" (on {})", branches.join(", ")) };
        push("commits", c.commits, format!("changed in {} commits{on}", c.commits), ids(&["commit", "fix", "revert"]));
    }
    if let Some(f) = x.items.iter().rev().find(|i| i.kind == "fix") {
        push("fixes", c.fixes, format!("{}, the last “{}”", plural(c.fixes as usize, "fix commit", "fix commits"), one_line(&f.text, 60)), ids(&["fix"]));
    }
    out.sort_by(|a, b| b.0.total_cmp(&a.0));
    out.into_iter().map(|(_, j)| j).collect()
}

// ------------------------------------------------- symbols, records, advice

/// A function or other definition of a file and how many edits fell in it.
#[derive(Clone, Debug)]
struct Sym {
    symbol: String,
    kind: String,
    line: i64,
    end: i64,
    edits: i64,
}

/// The definitions of a file: the code index's chunks (of the tree of the
/// agent that edited it most, else the main checkout's), else the file
/// chunked as the index would.
fn symbols_of(sup: &Supervisor, repo: &str, file: &str, agent: Option<&str>) -> Vec<Sym> {
    let p = Params::new().set("repo", Value::Str(repo.to_string())).set("path", Value::Str(file.to_string()));
    let rows = sup.db().query("SELECT symbol, kind, start_line, end_line, tree FROM code_chunks WHERE repo = :repo AND path = :path LIMIT 5000", &p).unwrap_or_default();
    let of_tree = |t: &str| -> Vec<Sym> {
        rows.iter()
            .filter(|r| get_str(&r[4]).as_deref() == Some(t))
            .filter_map(|r| Some(Sym { symbol: get_str(&r[0])?, kind: get_str(&r[1]).unwrap_or_default(), line: get_int(&r[2]).unwrap_or(0), end: get_int(&r[3]).unwrap_or(0), edits: 0 }))
            .collect()
    };
    let mut syms = agent.map(of_tree).unwrap_or_default();
    if syms.is_empty() {
        syms = of_tree("main");
    }
    if syms.is_empty() {
        let dirs: Vec<PathBuf> = agent.and_then(|a| sup.agent_row(a)).map(|a| PathBuf::from(a.cwd)).into_iter().chain([PathBuf::from(repo)]).collect();
        if let Some(text) = dirs.iter().find_map(|d| {
            let full = d.join(file);
            (std::fs::metadata(&full).ok()?.len() <= 4 << 20).then(|| std::fs::read_to_string(full).ok()).flatten()
        }) {
            syms = search::chunk(&text, search::lang_of(file))
                .into_iter()
                .filter_map(|c| Some(Sym { symbol: c.symbol?, kind: c.kind, line: c.start as i64, end: c.end as i64, edits: 0 }))
                .collect();
        }
    }
    syms
}

/// The line (1-based) in a chunk's span that defines its symbol: it names
/// the symbol after a word that makes definitions.
fn def_line(lines: &[&str], s: &Sym) -> Option<i64> {
    const DEFINES: [&str; 16] = ["def", "fn", "class", "function", "func", "struct", "enum", "trait", "impl", "interface", "type", "const", "let", "var", "async", "static"];
    let from = (s.line.max(1) - 1) as usize;
    let to = (s.end.max(s.line) as usize).min(lines.len());
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    (from..to)
        .find(|&i| {
            let l = lines[i];
            l.match_indices(s.symbol.as_str()).any(|(at, _)| {
                let before: Vec<&str> = l[..at].split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty()).collect();
                !word(l[..at].chars().next_back()) && !word(l[at + s.symbol.len()..].chars().next()) && before.iter().any(|w| DEFINES.contains(w))
            })
        })
        .map(|i| i as i64 + 1)
}

/// The edits of a file per definition, most first, from `runtime_edits`'
/// lines.
fn symbol_edits(sup: &Supervisor, repo: &str, file: &str, since_ms: i64, agent: Option<&str>) -> Vec<Sym> {
    let p = Params::new().set("repo", Value::Str(repo.to_string())).set("file", Value::Str(file.to_string())).set("since", Value::Int(since_ms));
    let lines: Vec<i64> = sup.db().query("SELECT line FROM runtime_edits WHERE repo = :repo AND file = :file AND ts_ms >= :since AND line IS NOT NULL LIMIT 100000", &p).unwrap_or_default().iter().filter_map(|r| get_int(&r[0])).collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let mut syms = symbols_of(sup, repo, file, agent);
    // A chunk starts with what is attached above its definition (imports,
    // comments, decorators): the definition's own span starts at the line
    // that names it (sixth build: an import line is no edit of test_pct).
    let text = agent.and_then(|a| sup.agent_row(a)).map(|a| PathBuf::from(a.cwd)).into_iter().chain([PathBuf::from(repo)]).find_map(|d| std::fs::read_to_string(d.join(file)).ok());
    if let Some(text) = &text {
        let lines: Vec<&str> = text.lines().collect();
        for s in syms.iter_mut() {
            if let Some(at) = def_line(&lines, s) {
                s.line = at;
            }
        }
    }
    for l in lines {
        // The innermost definition holding the line.
        if let Some(s) = syms.iter_mut().filter(|s| s.line <= l && l <= s.end.max(s.line)).min_by_key(|s| s.end - s.line) {
            s.edits += 1;
        }
    }
    syms.retain(|s| s.edits > 0);
    syms.sort_by(|a, b| b.edits.cmp(&a.edits).then(a.line.cmp(&b.line)));
    syms.truncate(8);
    syms
}

/// ChloeCtx's records of a repository, kept [`CTX_KEEP`].
fn ctx_records(sup: &Supervisor, main: &FsPath) -> Result<Vec<map::Record>, String> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Instant, Result<Vec<map::Record>, String>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((at, v)) = cache.lock().unwrap().get(main) {
        if at.elapsed() < CTX_KEEP {
            return v.clone();
        }
    }
    let v = map::file_records(sup, main);
    cache.lock().unwrap().insert(main.to_path_buf(), (Instant::now(), v.clone()));
    v
}

/// What ChloeCtx and the user's rules say of a file: ◆ records.
fn records_of(file: &str, symbol: Option<&str>, recs: &[map::Record], rules: &[questions::Rule]) -> Vec<Json> {
    let mut out: Vec<Json> = Vec::new();
    let b = base(file);
    for r in rules {
        let t = r.text.as_str();
        if t.contains(file) || t.contains(b) || symbol.is_some_and(|s| s.len() >= 4 && t.contains(s)) {
            out.push(json!({ "id": r.id, "kind": "rule", "text": r.text, "ts": iso(r.created), "via": "rule" }));
        }
    }
    for r in recs {
        if r.files.iter().any(|f| map::split_line(f).0 == file) {
            out.push(json!({ "id": r.id, "kind": r.kind, "text": r.text, "ts": r.ts, "via": "chloectx" }));
        }
    }
    out.truncate(8);
    out
}

/// What to do about a hotspot, the most useful first: make the user's
/// correction a rule when nothing says how the file should be; run the
/// test that keeps failing after edits here; or give the work to a
/// stronger model than the agents that kept redoing it.
fn suggestion(sup: &Supervisor, x: &Scored, records: &[Json], names: &HashMap<String, String>) -> Json {
    let c = &x.counts;
    if c.steers > 0 && records.is_empty() {
        if let Some(st) = x.items.iter().rev().find(|i| matches!(i.kind, "steer" | "prompt")) {
            let who = st.key.as_deref().and_then(|a| names.get(a)).cloned().unwrap_or_else(|| "the agent".into());
            return json!({
                "kind": "rule",
                "text": format!("No rule or decision names this file, and you corrected it {}. Make what you told {who} a rule:", times(c.steers as usize)),
                "rule": st.text,
            });
        }
    }
    if let Some((t, stat, n)) = x.tests.first().filter(|t| t.2 >= 2 || t.1.flaky) {
        let cmd = stat.command.clone().unwrap_or_else(|| t.clone());
        let why = if stat.flaky { "it is flaky, so run it twice".to_string() } else { format!("it failed {} after edits here", times(*n as usize)) };
        return json!({ "kind": "test", "text": format!("Run `{cmd}` before you edit, and again after: {why}."), "test": t, "command": cmd });
    }
    if c.re_edits >= 4 || c.reverts >= 2 {
        let models: Vec<String> = x.per_agent.iter().filter_map(|(a, _)| sup.agent_row(a)).map(|a| routing::model_of(a.model.as_deref())).collect();
        if !models.is_empty() && models.iter().all(|m| m == "haiku" || m == "sonnet") {
            let mut by: BTreeMap<&str, usize> = BTreeMap::new();
            for m in &models {
                *by.entry(m.as_str()).or_default() += 1;
            }
            let from = by.iter().max_by_key(|(_, n)| **n).map(|(m, _)| m.to_string()).unwrap_or_default();
            let model = if models.iter().all(|m| m == "haiku") { "sonnet" } else { "opus" };
            return json!({
                "kind": "model",
                "text": format!("Route work here to {model}: agents on {from} re-edited it {} and put back {}.", times(c.re_edits as usize), plural(c.reverts as usize, "edit", "edits")),
                "model": model, "from": from,
            });
        }
    }
    Json::Null
}

/// One sentence of a hotspot.
fn summary(x: &Scored, symbol: Option<&str>, reasons: &[Json], records: &[Json], names: &HashMap<String, String>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let c = &x.counts;
    if c.edits > 0 {
        let what = symbol.map(str::to_string).unwrap_or_else(|| "it".into());
        let edits_ms: Vec<i64> = x.items.iter().filter(|i| i.kind == "edit").map(|i| i.ts_ms).collect();
        let sp = match (edits_ms.iter().min(), edits_ms.iter().max()) {
            (Some(a), Some(b)) if b > a => format!(" in {}", span(b - a)),
            _ => String::new(),
        };
        let who = if x.per_agent.len() > 1 {
            format!(" ({})", x.per_agent.iter().map(|(a, n)| format!("{} {n}", names.get(a).map_or(a.as_str(), String::as_str))).collect::<Vec<_>>().join(", "))
        } else {
            String::new()
        };
        parts.push(if c.re_edits > 0 { format!("agents re-edited {what} {}{sp}{who}", times(c.re_edits as usize)) } else { format!("agents edited {what} {}{sp}{who}", times(c.edits as usize)) });
    }
    for r in reasons.iter().filter(|r| !matches!(r["kind"].as_str(), Some("re_edits" | "agents"))) {
        parts.push(r["text"].as_str().unwrap_or("").to_string());
    }
    if let Some(r) = records.first() {
        let t = r["text"].as_str().unwrap_or("");
        let mut lower = t.trim_end_matches('.').to_string();
        if let Some(c0) = lower.get(..1) {
            lower = c0.to_lowercase() + &lower[1..];
        }
        parts.push(format!("{} ◆ {}", r["kind"].as_str().unwrap_or("record"), one_line(&lower, 80)));
    }
    parts.join("; ")
}

/// What an agent about to edit the file is told: its history, what the
/// user and ChloeCtx say of it, and what to do first.
pub fn context_of(h: &Json) -> String {
    let s = |k: &str| h[k].as_str().unwrap_or("");
    let mut lines = vec![format!(
        "Workbench: {} is a hotspot of this repository (score {} in {} days): agents keep correcting it.",
        s("file"),
        h["score"].as_f64().map(|x| format!("{x}")).unwrap_or_default(),
        h["window_days"].as_i64().unwrap_or(WINDOW_DAYS)
    )];
    let summ = s("summary");
    for part in summ.split("; ").filter(|p| !p.contains('◆') && !p.starts_with("you corrected")) {
        lines.push(format!("- {part}"));
    }
    if let Some(t) = h["last_correction"].as_str() {
        lines.push(format!("- the user corrected work here: \"{}\"", one_line(t, 300)));
    }
    for r in h["records"].as_array().into_iter().flatten().take(4) {
        let kind = r["kind"].as_str().unwrap_or("record");
        if kind == "rule" {
            lines.push(format!("- ◆ rule: {}", one_line(r["text"].as_str().unwrap_or(""), 300)));
        } else {
            lines.push(format!("- ◆ {kind} ({}): {}", r["id"].as_str().unwrap_or(""), one_line(r["text"].as_str().unwrap_or(""), 300)));
        }
    }
    let mut todo: Vec<String> = Vec::new();
    if let Some(t) = h["pinned_test"].as_str() {
        let cmd = h["tests"].as_array().into_iter().flatten().find(|x| x["test"] == t).and_then(|x| x["command"].as_str()).unwrap_or(t);
        todo.push(format!("run `{cmd}` first, and again after (the user pinned it)."));
    } else if h["suggestion"]["kind"] == "test" {
        todo.push(format!("run `{}` first, and again after.", h["suggestion"]["command"].as_str().unwrap_or("")));
    }
    if let Some(m) = h["routed_model"].as_str() {
        todo.push(format!("the user wants work on this file done by {m}; if you are not {m} and the change is not small, ask the user (ask_human) whether to hand it to an agent on {m}."));
    }
    let mut last = String::from("Before you edit: ");
    if todo.is_empty() {
        last.push_str("read the history above. ");
    } else {
        last.push_str(&todo.join(" Also, "));
        last.push(' ');
    }
    last.push_str("If your change goes against a rule or decision above, ask the user (ask_human) instead of changing it back and forth.");
    lines.push(last);
    lines.join("\n")
}

// --------------------------------------------------------- making the rows

/// A repository's hotspots over a window, with their evidence.
pub struct Made {
    pub hotspots: Vec<Json>,
    pub evidence: HashMap<String, Vec<Json>>,
    /// The sums of every file's counts, hotspot or not, with the
    /// hotspots by level: `/api/hotspots`'s `totals`.
    totals: Json,
    pub missing: Vec<String>,
    ctx: Json,
}

/// The agents of a repository by id, with their names.
fn names_of(sup: &Supervisor, repo: &str) -> HashMap<String, String> {
    sup.agent_rows().into_iter().filter(|a| a.repo == repo).map(|a| (a.id, a.name)).collect()
}

/// Scores `repo`'s files since `since_ms`.
pub fn make(sup: &Supervisor, repo: &str, since_ms: i64) -> Made {
    let now = now_ms();
    let names = names_of(sup, repo);
    let mut s = collect(sup.db(), repo, since_ms, &names);
    undone_edits(sup, repo, since_ms, &mut s);
    git_log(FsPath::new(repo), since_ms, &mut s);
    // Each agent's evidence names its branch.
    let branches: HashMap<String, String> = sup.agent_rows().into_iter().filter(|a| a.repo == repo).filter_map(|a| Some((a.id, a.branch?))).collect();
    for items in s.items.values_mut() {
        for it in items.iter_mut().filter(|i| i.branch.is_none()) {
            let who = it.agent.as_ref().or(it.key.as_ref().filter(|_| matches!(it.kind, "steer" | "prompt")));
            it.branch = who.and_then(|a| branches.get(a)).cloned();
        }
    }
    // Commits per day join the edits per day.
    for (f, items) in &s.items {
        let mut days: HashSet<(i64, &str)> = HashSet::new();
        for i in items.iter().filter(|i| matches!(i.kind, "commit" | "fix" | "revert") && i.link.get("commit").is_some()) {
            if days.insert((i.ts_ms, i.id.as_str())) {
                *s.churn.entry(f.clone()).or_default().entry(i.ts_ms.div_euclid(86_400_000) * 86_400_000).or_default() += 1;
            }
        }
    }
    if let Err(e) = crate::runtime::sync_edits(sup) {
        s.missing.push(format!("the edits' lines could not be read: {}", e.message));
    }
    let recs = ctx_records(sup, FsPath::new(repo));
    let ctx = match &recs {
        Ok(r) => json!({ "available": true, "records": r.len() }),
        Err(e) => json!({ "available": false, "why": e }),
    };
    let recs = recs.unwrap_or_default();
    let rules = questions::rules(sup, Some(repo), true);
    let days = ((now - since_ms + 86_399_999) / 86_400_000).max(1);
    let today = now.div_euclid(86_400_000) * 86_400_000;
    let mut hotspots = Vec::new();
    let mut evidence = HashMap::new();
    let all = score_files(&s);
    let totals = Totals::of(&all, &s);
    for x in all.into_iter().filter(|x| x.counts.score() >= MIN_SCORE) {
        let id = id_of(repo, &x.file);
        let syms = symbol_edits(sup, repo, &x.file, since_ms, x.per_agent.first().map(|a| a.0.as_str()));
        let top = syms.first().filter(|t| t.edits >= 2 && t.edits * 10 >= syms.iter().map(|s| s.edits).sum::<i64>() * 4).cloned();
        let records = records_of(&x.file, top.as_ref().map(|t| t.symbol.as_str()), &recs, &rules);
        let rs = reasons(&x, &names);
        let score = x.counts.score();
        let churn_of = s.churn.get(&x.file);
        let churn: Vec<Json> = (0..days.min(90)).rev().map(|i| today - i * 86_400_000).map(|d| json!([d, churn_of.and_then(|c| c.get(&d)).copied().unwrap_or(0)])).collect();
        let sug = suggestion(sup, &x, &records, &names);
        let summ = summary(&x, top.as_ref().map(|t| t.symbol.as_str()), &rs, &records, &names);
        let last_correction = x.items.iter().filter(|i| matches!(i.kind, "steer" | "prompt")).max_by_key(|i| i.ts_ms).map(|i| i.text.clone());
        let mut items = x.items.clone();
        for r in &records {
            if r["via"] == "chloectx" {
                let at = trail::ctx_ms(&r["ts"]).unwrap_or(0);
                let mut it = Item::new(format!("rec:{}", r["id"].as_str().unwrap_or("")), "record", at, None, r["text"].as_str().unwrap_or("").to_string());
                it.detail = Some(format!("ChloeCtx {} {}", r["kind"].as_str().unwrap_or(""), r["id"].as_str().unwrap_or("")));
                it.link = json!({ "record": r["id"] });
                items.push(it);
            }
        }
        items.sort_by_key(|i| i.ts_ms);
        let skip = items.len().saturating_sub(EVIDENCE_MAX);
        evidence.insert(id.clone(), items[skip..].iter().map(Item::to_json).collect::<Vec<_>>());
        let tests: Vec<Json> = x.tests.iter().map(|(t, st, _)| json!({ "test": t, "runs": st.runs, "failed": st.failed, "flaky": st.flaky, "command": st.command })).collect();
        hotspots.push(json!({
            "id": id, "repo": repo, "file": x.file,
            "symbol": top.as_ref().map(|t| t.symbol.clone()), "symbol_kind": top.as_ref().map(|t| t.kind.clone()), "line": top.as_ref().map(|t| t.line),
            "score": score, "level": level_of(score), "counts": x.counts.to_json(), "reasons": rs,
            "agents": x.agents, "first_ms": x.first_ms, "last_ms": x.last_ms, "first": iso(x.first_ms), "last": iso(x.last_ms),
            "churn": churn,
            "symbols": syms.iter().map(|s| json!({ "symbol": s.symbol, "kind": s.kind, "line": s.line, "edits": s.edits })).collect::<Vec<_>>(),
            "tests": tests, "records": records, "suggestion": sug, "summary": summ,
            "last_correction": last_correction, "window_days": days,
            "pinned_test": Json::Null, "routed_model": Json::Null, "context": Json::Null,
        }));
    }
    let totals = totals.to_json(&hotspots);
    Made { hotspots, evidence, totals, missing: s.missing, ctx }
}

/// One refresh at a time per repository.
fn refresh_lock(repo: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock().unwrap().entry(repo.to_string()).or_default().clone()
}

/// Scores `repo` over the default window and writes the rows of
/// `hotspots`; answers how many and how long it took.
pub fn refresh(sup: &Supervisor, repo: &str) -> Result<(usize, u128), ApiError> {
    let lock = refresh_lock(repo);
    let _g = lock.lock().unwrap();
    let t0 = Instant::now();
    let now = now_ms();
    let m = make(sup, repo, now - WINDOW_DAYS * 86_400_000);
    let db = sup.db();
    let rp = Params::new().set("repo", Value::Str(repo.to_string()));
    db.query("DELETE FROM hotspots WHERE repo = :repo", &rp).map_err(ApiError::internal)?;
    for h in &m.hotspots {
        let id = h["id"].as_str().unwrap_or("").to_string();
        let c = &h["counts"];
        let n = |k: &str| Value::Int(c[k].as_i64().unwrap_or(0));
        let p = Params::new()
            .set("id", Value::Str(id.clone()))
            .set("repo", Value::Str(repo.to_string()))
            .set("file", Value::Str(h["file"].as_str().unwrap_or("").to_string()))
            .set("symbol", h["symbol"].as_str().map_or(Value::Null, |s| Value::Str(s.to_string())))
            .set("score", Value::Float(h["score"].as_f64().unwrap_or(0.0)))
            .set("level", Value::Str(h["level"].as_str().unwrap_or("").to_string()))
            .set("edits", n("edits"))
            .set("re_edits", n("re_edits"))
            .set("agents", n("agents"))
            .set("reverts", n("reverts"))
            .set("test_fails", n("test_fails"))
            .set("flaky", n("flaky"))
            .set("errors", n("errors"))
            .set("steers", n("steers"))
            .set("questions", n("questions"))
            .set("incidents", n("incidents"))
            .set("first_at", ts(h["first_ms"].as_i64().unwrap_or(0)))
            .set("last_at", ts(h["last_ms"].as_i64().unwrap_or(0)))
            .set("days", Value::Int(WINDOW_DAYS))
            .set("data", Value::Str(h.to_string()))
            .set("evidence", Value::Str(Json::Array(m.evidence.get(&id).cloned().unwrap_or_default()).to_string()))
            .set("updated", ts(now));
        db.query(
            "INSERT OR REPLACE INTO hotspots (id, repo, file, symbol, score, level, edits, re_edits, agents, reverts, test_fails, flaky, errors, steers, questions, incidents, first_at, last_at, days, data, evidence, updated) VALUES (:id, :repo, :file, :symbol, :score, :level, :edits, :re_edits, :agents, :reverts, :test_fails, :flaky, :errors, :steers, :questions, :incidents, :first_at, :last_at, :days, :data, :evidence, :updated)",
            &p,
        )
        .map_err(ApiError::internal)?;
    }
    let meta = json!({ "updated_ms": now, "days": WINDOW_DAYS, "missing": m.missing, "ctx": m.ctx, "totals": m.totals });
    sup.set_mark(&format!("hotspots:{repo}"), &meta.to_string())?;
    Ok((m.hotspots.len(), t0.elapsed().as_millis()))
}

/// A stored hotspot with what the user set on it since (a pinned test, a
/// model) and what the next agent to edit it is told.
fn decorate(sup: &Supervisor, mut h: Json) -> Json {
    let id = h["id"].as_str().unwrap_or("").to_string();
    h["pinned_test"] = sup.mark(&format!("hotpin:{id}")).filter(|v| !v.is_empty()).map_or(Json::Null, Json::String);
    h["routed_model"] = sup.mark(&format!("hotroute:{id}")).filter(|v| !v.is_empty()).map_or(Json::Null, Json::String);
    h["context"] = Json::String(context_of(&h));
    h
}

fn meta_of(sup: &Supervisor, repo: &str) -> Option<Json> {
    sup.mark(&format!("hotspots:{repo}")).and_then(|v| serde_json::from_str(&v).ok())
}

/// The stored hotspots of `repo`, best first, made again when stale.
fn stored(sup: &Supervisor, repo: &str, limit: usize) -> Result<(Vec<Json>, Json), ApiError> {
    let fresh = meta_of(sup, repo).and_then(|m| m["updated_ms"].as_i64()).is_some_and(|t| now_ms() - t < STALE_MS);
    if !fresh {
        refresh(sup, repo)?;
    }
    let rows = sup.db().query(&format!("SELECT data FROM hotspots WHERE repo = :repo ORDER BY score DESC LIMIT {}", limit.clamp(1, 1000)), &Params::new().set("repo", Value::Str(repo.to_string()))).map_err(ApiError::internal)?;
    let list = rows.iter().filter_map(|r| get_str(&r[0])).filter_map(|d| serde_json::from_str::<Json>(&d).ok()).map(|h| decorate(sup, h)).collect();
    Ok((list, meta_of(sup, repo).unwrap_or(Json::Null)))
}

fn repo_of(sup: &Supervisor, repo: Option<&str>, agent: Option<&str>) -> Result<String, ApiError> {
    if let Some(a) = agent.filter(|a| !a.is_empty()) {
        return sup.agent_row(a).map(|a| a.repo).ok_or_else(|| ApiError::not_found(format!("no agent {a}")));
    }
    Ok(trail::repo_dir(repo)?.to_string_lossy().to_string())
}

#[derive(Deserialize, Default)]
pub struct ListQ {
    repo: Option<String>,
    agent: Option<String>,
    since: Option<String>,
    days: Option<i64>,
    limit: Option<usize>,
    file: Option<String>,
}

/// `GET /api/hotspots`.
pub fn list(sup: &Supervisor, q: &ListQ) -> Result<Json, ApiError> {
    let repo = repo_of(sup, q.repo.as_deref(), q.agent.as_deref())?;
    let now = now_ms();
    let since = q.since.as_deref().filter(|s| !s.is_empty()).map(|s| s.parse::<i64>().ok().or_else(|| chloedb::timestamp::parse(s).map(|t| t / 1000)).ok_or_else(|| ApiError::bad(format!("since is milliseconds or ISO 8601, not {s}")))).transpose()?;
    let days = q.days.unwrap_or(WINDOW_DAYS).clamp(1, 90);
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let (mut list, meta) = if since.is_none() && days == WINDOW_DAYS {
        stored(sup, &repo, if q.file.is_some() { 1000 } else { limit })?
    } else {
        let m = make(sup, &repo, since.unwrap_or(now - days * 86_400_000));
        let meta = json!({ "updated_ms": now, "missing": m.missing, "ctx": m.ctx, "totals": m.totals });
        (m.hotspots.into_iter().map(|h| decorate(sup, h)).collect::<Vec<_>>(), meta)
    };
    if let Some(f) = q.file.as_deref().filter(|f| !f.is_empty()) {
        let f = f.trim_start_matches("./");
        list.retain(|h| h["file"] == f);
    }
    list.truncate(limit);
    let from = since.unwrap_or(now - days * 86_400_000);
    let mut ids: Vec<String> = Vec::new();
    for h in &list {
        for a in h["agents"].as_array().into_iter().flatten().filter_map(Json::as_str) {
            if !ids.iter().any(|x| x == a) {
                ids.push(a.to_string());
            }
        }
    }
    let agents: Vec<Json> = ids.iter().filter_map(|a| sup.agent_json_of(a)).collect();
    Ok(json!({
        "repo": repo,
        "window": { "from_ms": from, "to_ms": now, "days": ((now - from + 86_399_999) / 86_400_000).max(1) },
        "updated_ms": meta["updated_ms"], "every_s": EVERY_S,
        "hotspots": list, "totals": meta["totals"], "agents": agents,
        "ctx": meta["ctx"], "missing": meta["missing"].as_array().cloned().unwrap_or_default(),
    }))
}

/// A stored hotspot by id: its row's data and evidence.
fn by_id(sup: &Supervisor, id: &str) -> Result<(Json, Vec<Json>), ApiError> {
    let rows = sup.db().query("SELECT data, evidence FROM hotspots WHERE id = :id LIMIT 1", &Params::new().set("id", Value::Str(id.to_string()))).map_err(ApiError::internal)?;
    let r = rows.first().ok_or_else(|| ApiError::not_found(format!("no hotspot {id}")))?;
    let h: Json = get_str(&r[0]).and_then(|d| serde_json::from_str(&d).ok()).ok_or_else(|| ApiError::internal("a hotspot row without its data"))?;
    let ev: Vec<Json> = get_str(&r[1]).and_then(|d| serde_json::from_str(&d).ok()).unwrap_or_default();
    Ok((h, ev))
}

/// `GET /api/hotspots/:id`: the hotspot and its evidence, oldest first.
pub fn detail(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let (h, ev) = by_id(sup, id)?;
    let repo = h["repo"].as_str().unwrap_or("").to_string();
    let mut ids: Vec<String> = h["agents"].as_array().into_iter().flatten().filter_map(Json::as_str).map(str::to_string).collect();
    for e in &ev {
        if let Some(a) = e["agent"].as_str() {
            if !ids.iter().any(|x| x == a) {
                ids.push(a.to_string());
            }
        }
    }
    let agents: Vec<Json> = ids.iter().filter_map(|a| sup.agent_json_of(a)).collect();
    let missing = meta_of(sup, &repo).and_then(|m| m["missing"].as_array().cloned()).unwrap_or_default();
    Ok(json!({ "hotspot": decorate(sup, h), "evidence": ev, "agents": agents, "missing": missing }))
}

#[derive(Deserialize)]
struct ActB {
    kind: String,
    test: Option<String>,
    model: Option<String>,
}

/// `POST /api/hotspots/:id/act`: pin a test, route to a model, or clear both.
fn act(sup: &Supervisor, id: &str, b: ActB) -> Result<Json, ApiError> {
    let (h, _) = by_id(sup, id)?;
    match b.kind.as_str() {
        "pin_test" => {
            let t = b.test.as_deref().map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| ApiError::bad("test is required"))?;
            sup.set_mark(&format!("hotpin:{id}"), t)?;
        }
        "route" => {
            let m = b.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).ok_or_else(|| ApiError::bad("model is required"))?;
            if m.len() > 64 || !m.chars().all(|c| c.is_ascii_alphanumeric() || "-_.[]".contains(c)) {
                return Err(ApiError::bad(format!("{m} is not a model's name")));
            }
            sup.set_mark(&format!("hotroute:{id}"), m)?;
        }
        "clear" => {
            sup.set_mark(&format!("hotpin:{id}"), "")?;
            sup.set_mark(&format!("hotroute:{id}"), "")?;
        }
        k => return Err(ApiError::bad(format!("kind is pin_test, route or clear, not {k}"))),
    }
    Ok(json!({ "hotspot": decorate(sup, h) }))
}

/// `GET /api/hotspots/agents`: the running agents editing a hotspot (warm
/// or hot) in the last hour, each with its hottest.
fn agents_on(sup: &Supervisor, repo: Option<&str>) -> Result<Json, ApiError> {
    let now = now_ms();
    let repo = repo.filter(|r| !r.is_empty()).map(|r| trail::repo_dir(Some(r))).transpose()?.map(|p| p.to_string_lossy().to_string());
    let running: Vec<crate::db::Agent> = sup.agent_rows().into_iter().filter(|a| a.ended.is_none() && a.state != "exited" && repo.as_ref().is_none_or(|r| *r == a.repo)).collect();
    let mut out = Vec::new();
    let mut hot: HashMap<String, HashMap<String, Json>> = HashMap::new();
    for a in &running {
        let files = hot.entry(a.repo.clone()).or_insert_with(|| {
            let p = Params::new().set("repo", Value::Str(a.repo.clone())).set("min", Value::Float(TELL_SCORE));
            sup.db().query("SELECT file, data FROM hotspots WHERE repo = :repo AND score >= :min LIMIT 1000", &p).unwrap_or_default().iter().filter_map(|r| Some((get_str(&r[0])?, serde_json::from_str(&get_str(&r[1])?).ok()?))).collect()
        });
        if files.is_empty() {
            continue;
        }
        let p = Params::new().set("agent", Value::Str(a.id.clone())).set("since", ts(now - 3_600_000));
        let mut best: Option<(&Json, i64)> = None;
        for r in sup.db().query("SELECT file, MAX(ts) AS last FROM file_edits WHERE agent = :agent AND ts >= :since GROUP BY file LIMIT 10000", &p).unwrap_or_default() {
            let Some(h) = get_str(&r[0]).and_then(|f| files.get(&f)) else { continue };
            let at = get_ms(&r[1]).unwrap_or(0);
            if best.is_none_or(|(b, _)| h["score"].as_f64() > b["score"].as_f64()) {
                best = Some((h, at));
            }
        }
        if let Some((h, at)) = best {
            out.push(json!({ "agent": a.id, "name": a.name, "hotspot": h["id"], "file": h["file"], "symbol": h["symbol"], "score": h["score"], "level": h["level"], "since_ms": at }));
        }
    }
    Ok(json!({ "agents": out }))
}

// ------------------------------------------------------ telling the agents

/// The PreToolUse hook of an Edit, Write, MultiEdit or NotebookEdit: when
/// it is the session's first edit of a hotspot scoring [`TELL_SCORE`] or
/// more, the hotspot's history as `additionalContext`, once per session
/// and file.
pub fn check(sup: &Supervisor, id: &str, hook: &Json) -> Option<Json> {
    if hook.get("hook_event_name").and_then(Json::as_str) != Some("PreToolUse") || !hook.get("tool_name").and_then(Json::as_str).is_some_and(|t| map::EDIT_TOOLS.contains(&t)) {
        return None;
    }
    let input = &hook["tool_input"];
    let fp = ["file_path", "notebook_path"].iter().find_map(|k| input.get(*k).and_then(Json::as_str))?;
    let a = sup.agent_row(id)?;
    let rel = map::rel_path(fp, FsPath::new(&a.cwd), FsPath::new(&a.repo))?;
    let p = Params::new().set("repo", Value::Str(a.repo.clone())).set("file", Value::Str(rel.clone()));
    let rows = sup.db().query("SELECT score, data FROM hotspots WHERE repo = :repo AND file = :file LIMIT 1", &p).ok()?;
    let r = rows.first()?;
    if f64_of(&r[0]) < TELL_SCORE {
        return None;
    }
    let session = hook.get("session_id").and_then(Json::as_str).map(str::to_string).or(a.session_id.clone()).unwrap_or_else(|| id.to_string());
    let item = format!("hot:{session}:{rel}");
    static ONE: Mutex<()> = Mutex::new(());
    let _g = ONE.lock().unwrap();
    let did = format!("{id}:{item}");
    let told = sup.db().query("SELECT id FROM deliveries WHERE id = :id LIMIT 1", &Params::new().set("id", Value::Str(did.clone()))).ok().is_some_and(|r| !r.is_empty());
    if told {
        return None;
    }
    let h = decorate(sup, serde_json::from_str(&get_str(&r[1])?).ok()?);
    let text = h["context"].as_str()?.to_string();
    let dp = Params::new().set("id", Value::Str(did)).set("agent", Value::Str(id.to_string())).set("item", Value::Str(item)).set("via", Value::Str("PreToolUse".into())).set("ts", ts(now_ms()));
    sup.db().query("INSERT OR REPLACE INTO deliveries (id, agent, item, via, ts) VALUES (:id, :agent, :item, :via, :ts)", &dp).ok()?;
    let _ = sup.note(id, "hotspot", format!("hotspot: told about {rel} (score {})", h["score"]), Some(json!({ "hotspot": h["id"], "file": rel, "score": h["score"] }).to_string()), |_| {});
    Some(json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "additionalContext": text } }))
}

fn f64_of(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        Value::Int(i) => *i as f64,
        _ => 0.0,
    }
}

// ------------------------------------------------------------- the timer

/// Makes every repository's scores again every minute: those with an agent
/// running, or one that ended within the window.
pub fn start(sup: &Arc<Supervisor>) {
    let sup = sup.clone();
    let _ = std::thread::Builder::new().name("hotspots".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        loop {
            let since = now_ms() - WINDOW_DAYS * 86_400_000;
            let repos: HashSet<String> = sup.agent_rows().into_iter().filter(|a| a.ended.is_none_or(|e| e >= since)).map(|a| a.repo).collect();
            for r in repos {
                let _ = refresh(&sup, &r);
            }
            std::thread::sleep(Duration::from_secs(EVERY_S));
        }
    });
}

// ----------------------------------------------------------------- routes

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/hotspots", get(list_get))
        .route("/api/hotspots/agents", get(agents_get))
        .route("/api/hotspots/refresh", post(refresh_post))
        .route("/api/hotspots/:id", get(detail_get))
        .route("/api/hotspots/:id/act", post(act_post))
}

async fn list_get(State(app): State<App>, Query(q): Query<ListQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || list(&sup, &q)).await?).into_response())
}

#[derive(Deserialize)]
struct RepoQ {
    repo: Option<String>,
}

async fn agents_get(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || agents_on(&sup, q.repo.as_deref())).await?).into_response())
}

#[derive(Deserialize)]
struct RefreshB {
    repo: Option<String>,
    agent: Option<String>,
}

async fn refresh_post(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: RefreshB = if body.is_empty() { RefreshB { repo: None, agent: None } } else { parse_body(&body)? };
    let sup = app.sup.clone();
    let v = blocking(move || {
        let repo = repo_of(&sup, b.repo.as_deref(), b.agent.as_deref())?;
        let (n, ms) = refresh(&sup, &repo)?;
        Ok(json!({ "ok": true, "repo": repo, "hotspots": n, "ms": ms }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

async fn detail_get(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || detail(&sup, &id)).await?).into_response())
}

async fn act_post(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    let b: ActB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || act(&sup, &id, b)).await?).into_response())
}

// --------------------------------------------------------------- MCP tool

/// The MCP tool's description and input (appended to src/mcp.rs's list).
pub fn mcp_def() -> Json {
    json!({
        "name": "hotspots",
        "description": "The files and functions of the repository that agents keep correcting, redoing or breaking: re-edited by several agents or in one task, reverted, followed by failing or flaky tests, runtime errors, the user's corrections, questions and incidents. Each with its score, why, what ChloeCtx's decisions and the user's rules say of it, and what to do first (a test to run, a rule to follow). With file, that file's history. Ask it before a change to a file you do not know.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "The repository's absolute path; yours when left out." },
                "file": { "type": "string", "description": "A path relative to the repository: only that file's hotspot, with its history." }
            }
        }
    })
}

/// The `hotspots` tool: the list, or one file's hotspot, as text.
pub fn mcp_tool(call: &dyn Fn(&str, &str, Option<Json>, u64) -> Result<Json, String>, me: &str, args: &Json) -> Result<String, String> {
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    let mut path = match args.get("repo").and_then(Json::as_str).filter(|r| !r.is_empty()) {
        Some(r) => format!("/api/hotspots?repo={}", enc(r)),
        None if !me.is_empty() => format!("/api/hotspots?agent={}", enc(me)),
        None => return Err("repo is required outside a Workbench session".into()),
    };
    let file = args.get("file").and_then(Json::as_str).filter(|f| !f.is_empty());
    match file {
        Some(f) => path.push_str(&format!("&file={}", enc(f))),
        None => path.push_str("&limit=10"),
    }
    let v = call("GET", &path, None, 120)?;
    let list = v["hotspots"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return Ok(match file {
            Some(f) => format!("{f} is not a hotspot: in the last {} days no agent kept correcting, redoing or breaking it.", v["window"]["days"].as_i64().unwrap_or(WINDOW_DAYS)),
            None => format!("No hotspots in the last {} days.", v["window"]["days"].as_i64().unwrap_or(WINDOW_DAYS)),
        });
    }
    if file.is_some() {
        let h = &list[0];
        let mut out = h["context"].as_str().unwrap_or("").to_string();
        let tests: Vec<String> = h["tests"].as_array().into_iter().flatten().map(|t| format!("- {} ({} runs, {} failed{}): `{}`", t["test"].as_str().unwrap_or(""), t["runs"], t["failed"], if t["flaky"] == true { ", flaky" } else { "" }, t["command"].as_str().unwrap_or(""))).collect();
        if !tests.is_empty() {
            out.push_str(&format!("\nTests that failed after edits here:\n{}", tests.join("\n")));
        }
        return Ok(out);
    }
    let lines: Vec<String> = list
        .iter()
        .map(|h| {
            let sym = h["symbol"].as_str().map(|s| format!(" › {s}")).unwrap_or_default();
            let mut l = format!("{} {}{sym} [{} {}]: {}", h["id"].as_str().unwrap_or(""), h["file"].as_str().unwrap_or(""), h["level"].as_str().unwrap_or(""), h["score"], h["summary"].as_str().unwrap_or(""));
            if let Some(t) = h["suggestion"]["text"].as_str() {
                l.push_str(&format!("\n   do: {t}{}", h["suggestion"]["rule"].as_str().map(|r| format!(" \"{r}\"")).unwrap_or_default()));
            }
            l
        })
        .collect();
    Ok(format!("Hotspots of the last {} days, hottest first (ask again with file for one's history):\n{}", v["window"]["days"].as_i64().unwrap_or(WINDOW_DAYS), lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(db: &StateDb, sql: &str, p: Params) {
        db.query(sql, &p).unwrap();
    }

    fn edit(db: &StateDb, id: &str, agent: &str, file: &str, at: i64) {
        let p = Params::new().set("id", Value::Str(id.into())).set("repo", Value::Str("/r".into())).set("agent", Value::Str(agent.into())).set("file", Value::Str(file.into())).set("tool", Value::Str("Edit".into())).set("ts", ts(at)).set("seq", Value::Int(at / 1000));
        put(db, "INSERT INTO file_edits (id, repo, agent, file, tool, ts, seq) VALUES (:id, :repo, :agent, :file, :tool, :ts, :seq)", p);
    }

    fn run(db: &StateDb, id: &str, agent: &str, test: &str, outcome: &str, at: i64, commit: &str) {
        let p = Params::new()
            .set("id", Value::Str(id.into()))
            .set("run", Value::Str(format!("t{id}")))
            .set("repo", Value::Str("/r".into()))
            .set("agent", Value::Str(agent.into()))
            .set("commit", Value::Str(commit.into()))
            .set("tree", Value::Str("clean".into()))
            .set("command", Value::Str("cargo test".into()))
            .set("test", Value::Str(test.into()))
            .set("outcome", Value::Str(outcome.into()))
            .set("error", if outcome == "failed" { Value::Str("left: 1799 right: 1800".into()) } else { Value::Null })
            .set("ts", ts(at));
        put(db, "INSERT INTO test_runs (id, run, repo, agent, \"commit\", tree, command, test, outcome, error, ts) VALUES (:id, :run, :repo, :agent, :commit, :tree, :command, :test, :outcome, :error, :ts)", p);
    }

    #[test]
    fn signals_from_rows_score_the_file_the_agents_keep_correcting() {
        let dir = tempfile::tempdir().unwrap();
        let db = StateDb::open(dir.path()).unwrap();
        db.query(TABLE, &Params::new()).unwrap();
        let t0 = now_ms() - 3_600_000;
        let m = 60_000;
        // a1 edits totals.rs five times, a2 twice; README once.
        for (i, (a, f, at)) in [("a1", "src/totals.rs", 0), ("a1", "src/totals.rs", 5), ("a1", "src/totals.rs", 10), ("a2", "src/totals.rs", 12), ("a1", "src/totals.rs", 20), ("a2", "src/totals.rs", 25), ("a1", "src/totals.rs", 30), ("a1", "README.md", 40)].iter().enumerate() {
            edit(&db, &format!("e{i}"), a, f, t0 + at * m);
        }
        // Failures after edits of totals.rs; a flaky one; one long after.
        run(&db, "1", "a1", "totals::coupon", "failed", t0 + 6 * m, "c1");
        run(&db, "2", "a1", "totals::coupon", "failed", t0 + 21 * m, "c1");
        run(&db, "3", "a1", "totals::flaky", "failed", t0 + 22 * m, "c2");
        run(&db, "4", "a1", "totals::flaky", "passed", t0 + 23 * m, "c2");
        run(&db, "5", "a1", "readme::check", "failed", t0 + 40 * m + 2 * m, "c3");
        // A steer and a correcting prompt to a1, a prompt that is not one.
        let ev = |id: &str, kind: &str, at: i64, data: Json| {
            let p = Params::new().set("id", Value::Str(id.into())).set("seq", Value::Int(at / 1000)).set("ts", ts(at)).set("agent", Value::Str("a1".into())).set("kind", Value::Str(kind.into())).set("data", Value::Str(data.to_string()));
            put(&db, "INSERT INTO events (id, seq, ts, agent, kind, data) VALUES (:id, :seq, :ts, :agent, :kind, :data)", p);
        };
        ev("v1", "steer", t0 + 11 * m, json!({ "id": "s1", "text": "Round once, on the line total." }));
        ev("v2", "UserPromptSubmit", t0 + 31 * m, json!({ "prompt": "No, don't round per unit again." }));
        ev("v3", "UserPromptSubmit", t0 + 32 * m, json!({ "prompt": "Great, now add a changelog entry." }));
        // An error put down to the file, and an incident naming it.
        let p = Params::new().set("id", Value::Str("x1".into())).set("repo", Value::Str("/r".into())).set("agent", Value::Str("a1".into())).set("file", Value::Str("src/totals.rs".into())).set("message", Value::Str("panicked: overflow".into())).set("hits", Value::Int(3)).set("first_ms", Value::Int(t0 + 26 * m)).set("last_ms", Value::Int(t0 + 27 * m)).set("line", Value::Int(88)).set("state", Value::Str("open".into()));
        put(&db, "INSERT INTO runtime_errors (id, repo, agent, file, message, hits, first_ms, last_ms, line, state) VALUES (:id, :repo, :agent, :file, :message, :hits, :first_ms, :last_ms, :line, :state)", p);
        let p = Params::new().set("id", Value::Str("i1".into())).set("repo", Value::Str("/r".into())).set("title", Value::Str("a1 is stuck".into())).set("files", Value::Str("[\"src/totals.rs\"]".into())).set("agents", Value::Str("[\"a1\"]".into())).set("started", ts(t0 + 22 * m)).set("updated", ts(t0 + 23 * m));
        put(&db, "INSERT INTO incidents (id, repo, title, files, agents, started, updated) VALUES (:id, :repo, :title, :files, :agents, :started, :updated)", p);
        // A question asked right after an edit.
        let p = Params::new().set("id", Value::Str("q1".into())).set("agent", Value::Str("a1".into())).set("repo", Value::Str("/r".into())).set("kind", Value::Str("ask".into())).set("text", Value::Str("Half-even or half-up?".into())).set("created", ts(t0 + 21 * m));
        put(&db, "INSERT INTO questions (id, agent, repo, kind, text, created) VALUES (:id, :agent, :repo, :kind, :text, :created)", p);

        let names: HashMap<String, String> = [("a1".to_string(), "cart".to_string()), ("a2".to_string(), "checkout".to_string())].into();
        let s = collect(&db, "/r", t0 - m, &names);
        assert!(s.missing.is_empty(), "{:?}", s.missing);
        assert_eq!(s.edits["src/totals.rs"]["a1"].0, 5);
        assert!(s.tests["totals::flaky"].flaky && !s.tests["totals::coupon"].flaky);
        let scored = score_files(&s);
        // The header's totals are the sums of the files' own counts: no
        // "2 test failures after an edit" beside "0 edits".
        let t = Totals::of(&scored, &s);
        assert_eq!(t.edits, 8);
        assert_eq!(t.test_fails, s.items.values().flatten().filter(|i| i.kind == "test_fail").count() as i64);
        assert_eq!(t.files, scored.len() as i64);
        let top = &scored[0];
        assert_eq!(top.file, "src/totals.rs");
        let c = top.counts;
        assert_eq!((c.edits, c.re_edits, c.agents), (7, 5, 2));
        assert_eq!((c.test_fails, c.flaky), (2, 1), "{:?}", top.items.iter().map(|i| (i.kind, i.key.clone())).collect::<Vec<_>>());
        assert_eq!((c.steers, c.errors, c.incidents, c.questions), (2, 1, 1, 1));
        // 5 + 2 + 3 + 3 + 6 + 2.5 + 2 + 1.5
        assert_eq!(c.score(), 25.0);
        assert_eq!(level_of(c.score()), "hot");
        // The README's failure blames README; one edit and one failure stay under the bar.
        assert!(scored.iter().filter(|x| x.file == "README.md").all(|x| x.counts.score() < MIN_SCORE));
        let rs = reasons(top, &names);
        assert_eq!(rs[0]["kind"], "steers");
        assert_eq!(rs[0]["text"], "you corrected it twice");
        assert!(rs.iter().any(|r| r["kind"] == "re_edits" && r["text"] == "re-edited 5 times by cart and checkout"), "{rs:?}");
        assert!(rs.iter().any(|r| r["kind"] == "test_fails" && r["text"] == "totals::coupon failed twice after edits here"), "{rs:?}");
        // The churn is by day.
        assert_eq!(s.churn["src/totals.rs"].values().sum::<i64>(), 7);
        let summ = summary(top, Some("round_line_total"), &rs, &[], &names);
        assert!(summ.starts_with("agents re-edited round_line_total 5 times in 30 min (cart 5, checkout 2)"), "{summ}");
        db.close();
    }

    #[test]
    fn a_test_file_whose_tests_fail_is_ranked_and_totals_count_things_once() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("shop");
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        std::fs::write(repo.join("tests/test_coupons.py"), "def test_promo_window():\n    pass\n").unwrap();
        let repo = repo.to_string_lossy().to_string();
        let db = StateDb::open(&dir.path().join("db")).unwrap();
        let t0 = now_ms() - 3_600_000;
        let m = 60_000;
        let test = "tests/test_coupons.py::test_promo_window";
        // No agent edited anything: it ran the tests, failing and passing
        // on one commit (flaky), and was steered.
        for (i, (outcome, at)) in [("failed", 1), ("passed", 2), ("failed", 3), ("passed", 4)].iter().enumerate() {
            let p = Params::new()
                .set("id", Value::Str(format!("r{i}")))
                .set("run", Value::Str(format!("t{i}")))
                .set("repo", Value::Str(repo.clone()))
                .set("agent", Value::Str("a1".into()))
                .set("commit", Value::Str("c1".into()))
                .set("tree", Value::Str("clean".into()))
                .set("command", Value::Str("python3 -m pytest".into()))
                .set("test", Value::Str(test.into()))
                .set("outcome", Value::Str((*outcome).into()))
                .set("ts", ts(t0 + at * m));
            put(&db, "INSERT INTO test_runs (id, run, repo, agent, \"commit\", tree, command, test, outcome, ts) VALUES (:id, :run, :repo, :agent, :commit, :tree, :command, :test, :outcome, :ts)", p);
        }
        let p = Params::new().set("id", Value::Str("v1".into())).set("seq", Value::Int(1)).set("ts", ts(t0 + 5 * m)).set("agent", Value::Str("a1".into())).set("kind", Value::Str("steer".into())).set("data", Value::Str(json!({ "id": "s1", "text": "Keep it simple: just run the tests." }).to_string()));
        put(&db, "INSERT INTO events (id, seq, ts, agent, kind, data) VALUES (:id, :seq, :ts, :agent, :kind, :data)", p);
        // An alert fired on the test, in an incident naming no file.
        let p = Params::new().set("id", Value::Str("f1".into())).set("rule", Value::Str("mine".into())).set("repo", Value::Str(repo.clone())).set("entity", Value::Str(test.into())).set("incident", Value::Str("i1".into())).set("last_seen", ts(t0 + 4 * m));
        put(&db, "INSERT INTO alert_firings (id, rule, repo, entity, incident, last_seen) VALUES (:id, :rule, :repo, :entity, :incident, :last_seen)", p);
        let p = Params::new().set("id", Value::Str("i1".into())).set("repo", Value::Str(repo.clone())).set("title", Value::Str("test failing twice".into())).set("files", Value::Str("[]".into())).set("agents", Value::Str("[]".into())).set("started", ts(t0 + 3 * m)).set("updated", ts(t0 + 4 * m));
        put(&db, "INSERT INTO incidents (id, repo, title, files, agents, started, updated) VALUES (:id, :repo, :title, :files, :agents, :started, :updated)", p);
        let names: HashMap<String, String> = [("a1".to_string(), "flake-hunt".to_string())].into();
        let mut s = collect(&db, &repo, t0 - m, &names);
        // One commit that touched three files is one commit.
        for f in ["money.py", "tests/test_money.py", "README.md"] {
            let mut it = Item::new("9f098a11".into(), "fix", t0, None, "fix rounding".into());
            it.link = json!({ "commit": "9f098a11aa" });
            s.add(f, it);
        }
        let scored = score_files(&s);
        let t = Totals::of(&scored, &s);
        assert_eq!((t.commits, t.fixes), (1, 1));
        // The steer counts though the agent had edited nothing.
        assert_eq!(t.steers, 1);
        assert_eq!(t.test_fails, 2);
        let x = scored.iter().find(|x| x.file == "tests/test_coupons.py").expect("the test file is scored");
        assert_eq!((x.counts.flaky, x.counts.incidents), (1, 1), "{:?}", x.items.iter().map(|i| i.kind).collect::<Vec<_>>());
        assert!(x.counts.score() >= MIN_SCORE, "{}", x.counts.score());
        let rs = reasons(x, &names);
        assert!(rs.iter().any(|r| r["kind"] == "flaky" && r["text"] == format!("{test} is flaky")), "{rs:?}");
        assert_eq!(test_file(test), Some("tests/test_coupons.py"));
        assert_eq!(test_file("totals::coupon"), None);
        assert_eq!(test_file("tests::it works"), None);
        db.close();
    }

    #[test]
    fn a_line_is_a_functions_only_inside_its_span() {
        // The code index's chunk of test_pct starts at the imports above it.
        let text = "import pytest\nfrom money import pct\n\n\ndef test_pct():\n    assert pct(5) == 1\n";
        let lines: Vec<&str> = text.lines().collect();
        let s = Sym { symbol: "test_pct".into(), kind: "function".into(), line: 1, end: 6, edits: 0 };
        assert_eq!(def_line(&lines, &s), Some(5));
        let s = Sym { symbol: "pct".into(), kind: "function".into(), line: 1, end: 6, edits: 0 };
        assert_eq!(def_line(&lines, &s), None, "an import of pct defines nothing");
        assert!(is_fix("Fix rounding: totals a cent short") && is_fix("money: correct the half-up bug") && !is_fix("Prefix the totals"));
        // Commits seed the score: a file changed three times in a week.
        let c = Counts { commits: 3, fixes: 1, ..Counts::default() };
        assert_eq!(c.score(), 4.5);
        assert!(c.score() >= MIN_SCORE);
    }

    #[test]
    fn the_context_says_what_to_do() {
        let h = json!({
            "file": "src/totals.rs", "score": 25.0, "window_days": 14,
            "summary": "agents re-edited it 5 times; you corrected it twice; totals::coupon failed twice after edits here; rule ◆ round once",
            "last_correction": "Round once, on the line total.",
            "records": [{ "id": "r1", "kind": "rule", "text": "Round once." }, { "id": "c1:2", "kind": "decision", "text": "Money is cents." }],
            "suggestion": { "kind": "test", "command": "cargo test totals" }, "tests": [],
            "pinned_test": null, "routed_model": "opus",
        });
        let t = context_of(&h);
        assert!(t.starts_with("Workbench: src/totals.rs is a hotspot of this repository (score 25 in 14 days)"), "{t}");
        assert!(t.contains("\n- agents re-edited it 5 times\n- totals::coupon failed twice after edits here\n- the user corrected work here: \"Round once, on the line total.\"\n- ◆ rule: Round once.\n- ◆ decision (c1:2): Money is cents."), "{t}");
        assert!(t.contains("Before you edit: run `cargo test totals` first, and again after. Also, the user wants work on this file done by opus"), "{t}");
        assert!(corrects("No, don't round per unit") && !corrects("Great, now add a changelog entry."));
        assert_eq!(Counts { re_edits: 11, agents: 2, reverts: 3, test_fails: 6, steers: 2, questions: 1, incidents: 2, ..Counts::default() }.score(), 42.5);
    }
}
