//! Debug context and project (JOURNEYS.md, "D"): an agent's session as
//! turns, what it knew at one turn (its prompt, the files it read and
//! edited, how full its context was, the ChloeCtx records put there and
//! those it had but did not put there), the blast radius of a bad turn as
//! a ranked list ("changed total(): 9 callers reached, 2 tests caught it,
//! 0 reached main"), and the fixes: ChloeCtx's records about a project
//! (correct, forget, pin to a file, make a rule) and the recall threshold.
//!
//! What ChloeCtx put in a turn's context comes from its receipts
//! (`chloectx receipts --json`, and `chloectx receipt ID --json` for every
//! candidate with its score and why it was left out); records are read
//! with `chloectx sql` and changed through its daemon (`chloectx rpc`:
//! `remember` with `supersedes`, `forget`); the threshold is the store's
//! `knowledge.min_cos` in its config.json.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json as AxJson, Router};
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::blast::{self, git_out, plural};
use crate::db::{Agent, Event};
use crate::git;
use crate::narrate;
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::story::{self, Call};
use crate::supervisor::{one_line, ApiError, Supervisor};
use crate::trail::{self, Ran};

/// The kinds ChloeCtx gives a prompt as knowledge (recall.rs, KNOWLEDGE),
/// and the rules Workbench stores as decisions.
const KNOWLEDGE: [&str; 6] = ["decision", "preference", "fact", "todo", "note", "rule"];
/// ChloeCtx's default `knowledge.min_cos`.
const DEFAULT_MIN_COS: f64 = 0.40;
/// Full receipts read for one session (each is one call to the daemon).
const MAX_FULL: usize = 30;
/// Turns listed as standing out.
const STANDOUT: usize = 6;
/// Functions whose callers are looked for in a turn's blast.
const MAX_FUNCS: usize = 10;

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/debug/sessions", get(sessions_route))
        .route("/api/agents/:id/turns", get(turns_route))
        .route("/api/agents/:id/turns/:n/context", get(context_route))
        .route("/api/agents/:id/turns/:n/blast", get(blast_route))
        .route("/api/agents/:id/turns/:n/flag", axum::routing::post(flag_route))
        .route("/api/records", get(records_route))
        .route("/api/records/:id", axum::routing::patch(record_patch).delete(record_delete))
        .route("/api/repos/:p/threshold", get(threshold_get).put(threshold_put))
}

#[derive(Deserialize)]
struct RepoQ {
    repo: Option<String>,
}

async fn sessions_route(State(app): State<App>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || sessions(&sup, q.repo.as_deref().unwrap_or(""))).await?).into_response())
}

async fn turns_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || turns(&sup, &id)).await?).into_response())
}

async fn context_route(State(app): State<App>, AxPath((id, n)): AxPath<(String, usize)>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || context(&sup, &id, n)).await?).into_response())
}

async fn blast_route(State(app): State<App>, AxPath((id, n)): AxPath<(String, usize)>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || turn_blast(&sup, &id, n)).await?).into_response())
}

#[derive(Deserialize)]
struct FlagB {
    /// `true` (the default) flags the turn as where it went wrong; `false` clears it.
    on: Option<bool>,
}

/// `POST /api/agents/:id/turns/:n/flag {on}`: "This went wrong".
async fn flag_route(State(app): State<App>, AxPath((id, n)): AxPath<(String, usize)>, body: Bytes) -> ApiResult {
    let b: FlagB = if body.is_empty() { FlagB { on: None } } else { parse_body(&body)? };
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || {
        let a = sup.agent_row(&id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
        let turns = sup.all_events(&id).map(|e| e.iter().filter(|e| e.kind == "UserPromptSubmit").count()).unwrap_or(0);
        if n == 0 || n > turns {
            return Err(ApiError::not_found(format!("{} has no turn {n} ({turns} so far)", a.name)));
        }
        let mut flags = flags_of(&sup, &id);
        flags.retain(|x| *x != n);
        if b.on.unwrap_or(true) {
            flags.push(n);
        }
        sup.set_mark(&format!("turn-flags:{id}"), &json!(flags).to_string())?;
        Ok(json!({ "ok": true, "agent": a.id, "flagged": flags }))
    })
    .await?)
    .into_response())
}

/// The turns a person flagged as where it went wrong, oldest flag first.
fn flags_of(sup: &Supervisor, id: &str) -> Vec<usize> {
    sup.mark(&format!("turn-flags:{id}")).and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

// ---------------------------------------------------------------- a session

struct Turn {
    n: usize,
    started: i64,
    /// Its end: its Stop, else the next prompt, else its last event.
    ended: i64,
    over: bool,
    prompt: String,
    by: String,
    calls: Vec<Call>,
    read: BTreeSet<String>,
    edited: BTreeSet<String>,
    failed: usize,
    ran: usize,
    waited: bool,
    /// Tokens in its context at its last model call.
    ctx_tokens: Option<i64>,
}

struct Session {
    a: Agent,
    dir: PathBuf,
    turns: Vec<Turn>,
    sessions: Vec<String>,
    window: i64,
    missing: Vec<String>,
}

fn data_of(ev: &Event) -> Json {
    ev.data.as_deref().and_then(|d| serde_json::from_str(d).ok()).unwrap_or(Json::Null)
}

fn rel(path: &str, cwd: &str) -> String {
    narrate::rel(path, cwd)
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

/// An agent's session read into turns: a turn from each prompt to the next.
fn load(sup: &Supervisor, id: &str) -> Result<Session, ApiError> {
    let a = sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
    let evs = sup.all_events(id)?;
    let names: HashMap<String, String> = sup.agent_rows().into_iter().map(|a| (a.id, a.name)).collect();
    let dir = if Path::new(&a.cwd).is_dir() { PathBuf::from(&a.cwd) } else { PathBuf::from(&a.repo) };
    let mut missing = Vec::new();

    // The context's size at each model call, from the transcripts.
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut sessions: Vec<String> = Vec::new();
    for ev in &evs {
        let d = data_of(ev);
        if let Some(p) = d["transcript_path"].as_str() {
            let p = PathBuf::from(p);
            if !paths.contains(&p) && narrate::transcript_ok(&p.to_string_lossy()) {
                paths.push(p);
            }
        }
        if let Some(s) = d["session_id"].as_str() {
            if !sessions.iter().any(|x| x == s) {
                sessions.push(s.to_string());
            }
        }
    }
    if sessions.is_empty() {
        sessions.extend(a.session_id.clone());
    }
    let mut sizes: Vec<(i64, i64)> = Vec::new();
    for p in &paths {
        for e in narrate::read_tail(p, narrate::STORY_MAX).iter().filter(|e| e["type"] == "assistant") {
            let u = &e["message"]["usage"];
            let n = |k: &str| u[k].as_i64().unwrap_or(0);
            let size = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
            let ts = e["timestamp"].as_str().and_then(chloedb::timestamp::parse).map(|t| t.div_euclid(1000)).unwrap_or(0);
            if size > 0 && ts > 0 {
                sizes.push((ts, size));
            }
        }
    }
    sizes.sort();
    if paths.is_empty() {
        missing.push("No session transcript was named by its hooks, so how full its context was is not known.".into());
    }

    let starts: Vec<usize> = evs.iter().enumerate().filter(|(_, e)| e.kind == "UserPromptSubmit").map(|(i, _)| i).collect();
    let mut turns = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(evs.len());
        let slice = &evs[s..end];
        let stop = slice.iter().find(|e| e.kind == "Stop");
        let over = stop.is_some() || k + 1 < starts.len() || a.state == "exited";
        let calls = story::calls(slice, over);
        let d = data_of(&evs[s]);
        let prompt = d["prompt"].as_str().map(str::to_string).or_else(|| evs[s].summary.as_deref().map(|x| x.trim_start_matches("prompt: ").to_string())).unwrap_or_default();
        // Who typed it: an agent's input (send_to_agent) before it, else you.
        let from = if k == 0 { 0 } else { starts[k - 1] + 1 };
        let by = evs[from..s].iter().rev().find(|e| e.kind == "input").and_then(|e| data_of(e)["by"].as_str().map(|b| names.get(b).cloned().unwrap_or_else(|| b.to_string()))).unwrap_or_else(|| {
            if k == 0 && a.parent.is_some() {
                a.parent.as_deref().and_then(|p| names.get(p).cloned()).unwrap_or_else(|| "you".into())
            } else {
                "you".into()
            }
        });
        let started = evs[s].ts;
        let ended = stop.map(|e| e.ts).or_else(|| starts.get(k + 1).map(|&n| evs[n].ts - 1)).unwrap_or_else(|| slice.last().map_or(started, |e| e.ts));
        let mut read = BTreeSet::new();
        let mut edited = BTreeSet::new();
        let (mut failed, mut ran) = (0, 0);
        for c in &calls {
            let f = c.input["file_path"].as_str().or_else(|| c.input["notebook_path"].as_str()).map(|p| rel(p, &a.cwd));
            match story::kind_of(&c.tool) {
                "read" => {
                    read.extend(f);
                }
                "edit" => {
                    edited.extend(f);
                }
                "run" => ran += 1,
                _ => {}
            }
            if c.failed {
                failed += 1;
            }
        }
        let waited = slice.iter().any(|e| e.kind == "Notification" || e.kind == "PermissionRequest");
        let ctx_tokens = sizes.iter().filter(|(t, _)| *t >= started && *t <= ended.max(started)).map(|(_, n)| *n).next_back();
        turns.push(Turn { n: k + 1, started, ended, over, prompt, by, calls, read, edited, failed, ran, waited, ctx_tokens });
    }
    let model = a.model.clone().unwrap_or_default();
    let window = if model.contains("1m") { 1_000_000 } else { 200_000 };
    Ok(Session { a, dir, turns, sessions, window, missing })
}

/// The receipts of the agent's sessions (the rows each showed), oldest first.
fn receipts(sup: &Supervisor, s: &Session, missing: &mut Vec<String>) -> Vec<Json> {
    let mut out: Vec<Json> = Vec::new();
    for sid in &s.sessions {
        match trail::run_chloectx(&sup.cfg.chloectx_bin, &s.dir, &["receipts", "--session", sid, "--limit", "500", "--json"], Duration::from_secs(30)) {
            Ran::Out(v) => out.extend(v["receipts"].as_array().cloned().unwrap_or_default()),
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
    out.sort_by_key(|r| trail::ctx_ms(&r["ts"]).unwrap_or(0));
    out
}

/// The turn a receipt belongs to: ChloeCtx's own count when the agent ran
/// one session, else the turn its time falls in.
fn turn_of(r: &Json, s: &Session) -> usize {
    if s.sessions.len() == 1 {
        if let Some(t) = r["turn"].as_i64().filter(|t| *t > 0) {
            return t as usize;
        }
    }
    let ts = trail::ctx_ms(&r["ts"]).unwrap_or(0) + 999;
    s.turns.iter().filter(|t| t.started <= ts).count()
}

fn full_cache() -> &'static Mutex<HashMap<String, Json>> {
    static C: std::sync::OnceLock<Mutex<HashMap<String, Json>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One receipt with every candidate (a receipt never changes, so it is kept).
fn full_receipt(sup: &Supervisor, dir: &Path, rid: &str) -> Option<Json> {
    if !rid.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.')) {
        return None;
    }
    let key = format!("{}\u{1}{rid}", sup.cfg.data_dir.display());
    if let Some(v) = full_cache().lock().unwrap().get(&key) {
        return Some(v.clone());
    }
    let Ran::Out(v) = trail::run_chloectx(&sup.cfg.chloectx_bin, dir, &["receipt", rid, "--no-replay", "--json"], Duration::from_secs(30)) else { return None };
    let mut c = full_cache().lock().unwrap();
    if c.len() > 5000 {
        c.clear();
    }
    c.insert(key, v.clone());
    Some(v)
}

fn score_of(row: &Json) -> f64 {
    row["cos"].as_f64().or_else(|| row["score"].as_f64()).unwrap_or(0.0)
}

fn knowledge_kind(k: &str) -> bool {
    KNOWLEDGE.contains(&k)
}

/// Why a candidate was left out, in plain words ("58% match; notes under
/// 60% are left out").
fn reason(row: &Json, min_cos: f64) -> String {
    let d = row["decision"].as_str().unwrap_or("");
    let s = (score_of(row) * 100.0).round();
    let m = (min_cos * 100.0).round();
    if row["cos"].as_f64().is_some_and(|c| c < min_cos) {
        format!("{s:.0}% match; notes under {m:.0}% are left out")
    } else if d.contains("gate") {
        format!("{s:.0}% match; this prompt looked like one that needs no reminder, so none was given")
    } else if d.contains("budget") || d.contains("not reached") {
        format!("{s:.0}% match; the room for reminders ran out before it")
    } else {
        format!("{s:.0}% match; left out ({})", d.trim_start_matches("skip: "))
    }
}

/// Words a record must name to be about a turn's change: its files' names
/// (without folders and extensions) and the functions it changed.
fn about_words(t: &Turn, funcs: &[String]) -> Vec<String> {
    let mut w: Vec<String> = Vec::new();
    for f in t.edited.iter().chain(t.read.iter()) {
        if let Some(stem) = Path::new(f).file_stem().and_then(|s| s.to_str()) {
            let stem = stem.trim_start_matches("test_").to_lowercase();
            if stem.len() >= 3 && !w.contains(&stem) {
                w.push(stem);
            }
        }
    }
    for f in funcs {
        let f = f.trim_end_matches("()").to_lowercase();
        if f.len() >= 3 && !w.contains(&f) {
            w.push(f);
        }
    }
    w
}

fn about(text: &str, words: &[String]) -> bool {
    let t = text.to_lowercase();
    words.iter().any(|w| t.contains(w.as_str()))
}

/// What ChloeCtx put in the context at one turn and what it left out.
struct Memory {
    injected: Vec<Json>,
    missed: Vec<Json>,
    left_out: Vec<Json>,
    min_cos: f64,
    tokens: i64,
    /// Candidates of knowledge kinds left out, with their scores (for the
    /// threshold's estimate).
    near: Vec<(f64, i64, String)>,
}

fn memory_at(sup: &Supervisor, s: &Session, rs: &[Json], t: &Turn, funcs: &[String]) -> Memory {
    let mut injected = Vec::new();
    let mut tokens = 0;
    let mut shown_before: HashSet<String> = HashSet::new();
    let mut mine: Vec<&Json> = Vec::new();
    for r in rs {
        let n = turn_of(r, s);
        if n < t.n {
            for row in r["rows"].as_array().into_iter().flatten() {
                shown_before.extend(row["id"].as_str().map(str::to_string));
            }
        } else if n == t.n {
            mine.push(r);
        }
    }
    let mut seen = HashSet::new();
    for r in &mine {
        tokens += r["tokens"].as_i64().unwrap_or(0);
        for row in r["rows"].as_array().into_iter().flatten() {
            let id = row["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            injected.push(json!({ "id": id, "kind": row["kind"], "text": row["text"], "score": row["score"], "decision": row["decision"], "source": r["source"] }));
        }
    }
    let words = about_words(t, funcs);
    let mut missed = Vec::new();
    let mut left_out = Vec::new();
    let mut min_cos = DEFAULT_MIN_COS;
    let mut near = Vec::new();
    let mut considered: HashSet<String> = HashSet::new();
    for r in mine.iter().take(MAX_FULL) {
        let Some(full) = r["id"].as_str().and_then(|rid| full_receipt(sup, &s.dir, rid)) else { continue };
        let rc = &full["receipt"];
        if let Some(m) = rc["inputs"]["knowledge"]["min_cos"].as_f64() {
            min_cos = m;
        }
        for row in rc["rows"].as_array().into_iter().flatten() {
            let id = row["id"].as_str().unwrap_or("").to_string();
            let kind = row["kind"].as_str().unwrap_or("");
            let d = row["decision"].as_str().unwrap_or("");
            if !d.starts_with("skip") || d.contains("shown as knowledge") || !knowledge_kind(kind) || seen.contains(&id) || shown_before.contains(&id) || !considered.insert(id.clone()) {
                continue;
            }
            let text = full["records"][&id]["text"].as_str().map(str::to_string);
            if full["records"][&id]["tag"] == "superseded" {
                continue;
            }
            let Some(text) = text else { continue };
            near.push((score_of(row), row["tokens"].as_i64().unwrap_or(0), text.clone()));
            let item = json!({ "id": id, "kind": kind, "text": text, "score": (score_of(row) * 100.0).round() / 100.0, "threshold": min_cos, "reason": reason(row, min_cos), "decision": d });
            if about(&text, &words) {
                missed.push(item);
            } else {
                left_out.push(item);
            }
        }
    }
    let by_score = |v: &mut Vec<Json>| v.sort_by(|a, b| b["score"].as_f64().unwrap_or(0.0).total_cmp(&a["score"].as_f64().unwrap_or(0.0)));
    by_score(&mut missed);
    by_score(&mut left_out);
    left_out.truncate(3);
    Memory { injected, missed, left_out, min_cos, tokens, near }
}

/// The test runs of an agent that broke: a failing run of a test whose run
/// before it passed (or that never ran before), with when.
fn breaks(sup: &Supervisor, id: &str) -> Vec<(String, i64)> {
    let Some(runs) = blast::test_runs(sup) else { return Vec::new() };
    let mut last: HashMap<&str, bool> = HashMap::new();
    let mut out = Vec::new();
    for r in runs.iter().filter(|r| r.agent == id) {
        let before = last.get(r.test.as_str()).copied();
        if !r.passed && before != Some(false) {
            out.push((r.test.clone(), r.ts));
        }
        last.insert(r.test.as_str(), r.passed);
    }
    out
}

/// The turn that went wrong: the last turn with an edit before a test broke.
fn bad_turn(s: &Session, br: &[(String, i64)]) -> Option<usize> {
    let first = br.iter().map(|(_, t)| *t).min()?;
    s.turns.iter().filter(|t| t.started <= first && !t.edited.is_empty()).map(|t| t.n).next_back().or_else(|| s.turns.iter().filter(|t| t.started <= first).map(|t| t.n).next_back())
}

fn turn_kind(t: &Turn, missed: usize) -> &'static str {
    if t.failed > 0 {
        "fail"
    } else if t.waited {
        "you"
    } else if missed > 0 {
        "ctx"
    } else if !t.edited.is_empty() {
        "edit"
    } else if t.ran > 0 {
        "run"
    } else {
        "read"
    }
}

fn kind_counts(rows: &[Json]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for r in rows {
        *m.entry(r["kind"].as_str().unwrap_or("note").to_string()).or_insert(0) += 1;
    }
    m
}

/// `GET /api/agents/:id/turns`: the session as a strip of turns, its
/// phases, the turns that stand out, the bad turn, and the memory it was
/// given, as counts by kind with the top record and the missed one.
pub fn turns(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let s = load(sup, id)?;
    let mut missing = s.missing.clone();
    let rs = receipts(sup, &s, &mut missing);
    let br = breaks(sup, id);
    // Your flag first; else the last edit before a test broke.
    let flags = flags_of(sup, id);
    let computed = bad_turn(&s, &br);
    let bad = flags.last().copied().filter(|n| *n <= s.turns.len()).or(computed);
    let bad_from = if flags.last().is_some_and(|n| Some(*n) == bad) { "you" } else if bad.is_some() { "tests" } else { "" };

    // Memory: shown rows per turn, and the missed ones for the turns most
    // worth it (the bad one, those that edited most), each a daemon call.
    let mut shown_by_turn: HashMap<usize, Vec<Json>> = HashMap::new();
    let mut times: HashMap<String, (usize, Json)> = HashMap::new();
    for r in &rs {
        let n = turn_of(r, &s);
        for row in r["rows"].as_array().into_iter().flatten() {
            shown_by_turn.entry(n).or_default().push(row.clone());
            let e = times.entry(row["id"].as_str().unwrap_or("").to_string()).or_insert((0, row.clone()));
            e.0 += 1;
        }
    }
    let mut ranked: Vec<&Turn> = s.turns.iter().filter(|t| shown_by_turn.contains_key(&t.n) || !t.edited.is_empty()).collect();
    ranked.sort_by_key(|t| (Some(t.n) != bad, std::cmp::Reverse(t.edited.len()), std::cmp::Reverse(t.n)));
    let checked: HashSet<usize> = ranked.iter().take(MAX_FULL / 2).map(|t| t.n).collect();
    let mut missed_by_turn: HashMap<usize, Vec<Json>> = HashMap::new();
    if !rs.is_empty() {
        for t in s.turns.iter().filter(|t| checked.contains(&t.n)) {
            let m = memory_at(sup, &s, &rs, t, &[]);
            if !m.missed.is_empty() {
                missed_by_turn.insert(t.n, m.missed);
            }
        }
    }

    let list: Vec<Json> = s
        .turns
        .iter()
        .map(|t| {
            let shown = shown_by_turn.get(&t.n).map_or(0, |v| v.len());
            let missed = missed_by_turn.get(&t.n).map_or(0, |v| v.len());
            let tests_broke: Vec<&str> = br.iter().filter(|(_, ts)| *ts >= t.started && *ts <= t.ended.max(t.started) + 1).map(|(n, _)| n.as_str()).collect();
            json!({
                "n": t.n, "title": title_of(&t.prompt), "prompt": { "text": one_line(&t.prompt, 400), "by": t.by },
                "started_ms": t.started, "ended_ms": t.ended, "over": t.over,
                "kind": turn_kind(t, missed), "read": t.read.len(), "edited": t.edited.iter().collect::<Vec<_>>(), "failed": t.failed, "ran": t.ran, "waited": t.waited,
                "tokens": t.ctx_tokens, "injected": shown, "missed": if checked.contains(&t.n) { json!(missed) } else { Json::Null },
                "tests_broke": tests_broke, "bad": Some(t.n) == bad, "flagged": flags.contains(&t.n),
            })
        })
        .collect();

    // Phases: at most five, cut at the longest pauses, each named by the
    // file it edited most.
    let mut cuts: Vec<usize> = Vec::new();
    if s.turns.len() >= 6 {
        let mut gaps: Vec<(i64, usize)> = s.turns.windows(2).map(|w| (w[1].started - w[0].ended, w[1].n)).collect();
        gaps.sort_by_key(|g| std::cmp::Reverse(g.0));
        cuts = gaps.iter().take(4).map(|g| g.1).collect();
        cuts.sort();
    }
    let mut phases = Vec::new();
    let mut from = 1;
    for end in cuts.iter().map(|c| c - 1).chain(std::iter::once(s.turns.len())) {
        if end < from {
            continue;
        }
        let mut edits: BTreeMap<&str, usize> = BTreeMap::new();
        for t in s.turns.iter().filter(|t| t.n >= from && t.n <= end) {
            for f in &t.edited {
                *edits.entry(f.as_str()).or_default() += 1;
            }
        }
        let name = edits.iter().max_by_key(|(_, n)| **n).map(|(f, _)| Path::new(f).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| f.to_string())).unwrap_or_else(|| "Reading".into());
        phases.push(json!({ "name": name, "from": from, "to": end }));
        from = end + 1;
    }

    // The turns that stand out, in turn order.
    let mut out: Vec<(usize, u8, &'static str, String)> = Vec::new();
    if let Some(b) = bad {
        let names: Vec<&str> = br.iter().map(|(n, _)| n.as_str()).take(3).collect();
        let text = match (bad_from, names.is_empty()) {
            ("you", true) => "You flagged it: this went wrong".to_string(),
            ("you", false) => format!("You flagged it; tests failed after it: {}", names.join(", ")),
            _ => format!("Tests failed after it: {}", names.join(", ")),
        };
        out.push((b, 0, "fail", text));
    }
    for t in &s.turns {
        if let Some(m) = missed_by_turn.get(&t.n).and_then(|v| v.first()) {
            out.push((t.n, 1, "ctx", format!("Had a note it was not reminded of: “{}” ({:.0}% match; under {:.0}% is left out)", one_line(m["text"].as_str().unwrap_or(""), 70), m["score"].as_f64().unwrap_or(0.0) * 100.0, m["threshold"].as_f64().unwrap_or(DEFAULT_MIN_COS) * 100.0)));
        }
        if t.failed > 0 {
            let what = t.calls.iter().find(|c| c.failed).map(|c| c.input["command"].as_str().map(|x| one_line(x, 60)).unwrap_or_else(|| c.tool.clone())).unwrap_or_default();
            out.push((t.n, 2, "fail", format!("{} failed: {what}", plural(t.failed, "tool call"))));
        }
        if t.waited {
            out.push((t.n, 3, "you", "Waited for you".into()));
        }
    }
    if let Some(t) = s.turns.iter().filter(|t| !t.edited.is_empty()).max_by_key(|t| t.edited.len()) {
        out.push((t.n, 4, "edit", format!("Edited {}: {}", plural(t.edited.len(), "file"), t.edited.iter().take(3).cloned().collect::<Vec<_>>().join(", "))));
    }
    out.sort_by_key(|o| (o.1, o.0));
    let mut kept: Vec<(usize, &'static str, String)> = Vec::new();
    for (n, _, k, text) in out {
        if kept.len() < STANDOUT && !kept.iter().any(|x| x.0 == n) {
            kept.push((n, k, text));
        }
    }
    kept.sort_by_key(|k| k.0);

    // Memory given: counts by kind, the top record and the missed one.
    let all: Vec<Json> = times.values().map(|(_, r)| r.clone()).collect();
    let top = times.values().max_by_key(|(n, _)| *n).map(|(n, r)| json!({ "id": r["id"], "kind": r["kind"], "text": r["text"], "times": n }));
    let missed_one = bad.and_then(|b| missed_by_turn.get(&b)).or_else(|| missed_by_turn.values().next()).and_then(|v| v.first()).cloned();
    let missed_turn = missed_one.as_ref().and_then(|m| missed_by_turn.iter().find(|(_, v)| v.first() == Some(m)).map(|(n, _)| *n));
    let now = s.turns.last().and_then(|t| t.ctx_tokens);
    Ok(json!({
        "agent": trail::agent_json(sup, &s.a), "turns": list, "phases": phases,
        "standout": kept.iter().map(|(n, k, t)| json!({ "n": n, "kind": k, "text": t })).collect::<Vec<_>>(),
        "bad": bad, "bad_from": bad_from, "flagged": flags, "breaks": br.iter().map(|(n, ts)| json!({ "test": n, "ts": crate::db::iso(*ts) })).collect::<Vec<_>>(),
        "memory": { "counts": kind_counts(&all), "total": all.len(), "top": top, "missed": missed_one, "missed_turn": missed_turn, "receipts": rs.len() },
        "context": { "tokens": now, "window": s.window, "pct": now.map(|n| (n * 100 / s.window).min(100)) },
        "missing": missing,
    }))
}

/// `GET /api/agents/:id/turns/:n/context`: what it knew at turn n.
pub fn context(sup: &Supervisor, id: &str, n: usize) -> Result<Json, ApiError> {
    let s = load(sup, id)?;
    let t = s.turns.iter().find(|t| t.n == n).ok_or_else(|| ApiError::not_found(format!("{} has no turn {n} ({} so far)", s.a.name, s.turns.len())))?;
    let mut missing = s.missing.clone();
    let rs = receipts(sup, &s, &mut missing);
    let funcs: Vec<String> = changes(&s, t).1.into_iter().map(|(_, f)| f).collect();
    let m = memory_at(sup, &s, &rs, t, &funcs);
    if rs.is_empty() && missing.is_empty() {
        missing.push("ChloeCtx kept no receipt of this session's recalls (receipts are off, or it ran before ChloeCtx was wired), so what it was given is not known.".into());
    }
    let mut files: Vec<Json> = Vec::new();
    for f in t.read.union(&t.edited) {
        let how = match (t.read.contains(f), t.edited.contains(f)) {
            (true, true) => "read, edited",
            (true, false) => "read",
            _ => "edited",
        };
        files.push(json!({ "path": f, "how": how }));
    }
    files.sort_by_key(|f| (f["how"] == "read", f["path"].as_str().unwrap_or("").to_string()));
    // Lowering the threshold to take the missed record: how many more
    // records this session's recalls would have shown, and their tokens.
    let to = m.missed.first().and_then(|x| x["score"].as_f64()).map(|sc| ((sc - 0.005) * 20.0).floor() / 20.0).unwrap_or(m.min_cos - 0.05).clamp(0.05, 0.95);
    let more: Vec<&(f64, i64, String)> = m.near.iter().filter(|(sc, _, _)| *sc >= to && *sc < m.min_cos).collect();
    Ok(json!({
        "agent": trail::agent_json(sup, &s.a), "turn": n, "turns": s.turns.len(), "title": title_of(&t.prompt),
        "prompt": { "text": t.prompt, "by": t.by }, "started_ms": t.started, "ended_ms": t.ended, "over": t.over,
        "files": files,
        "context": { "tokens": t.ctx_tokens, "window": s.window, "pct": t.ctx_tokens.map(|x| (x * 100 / s.window).min(100)), "memory_tokens": m.tokens },
        "memory": {
            "injected": m.injected, "counts": kind_counts(&m.injected),
            "missed": m.missed, "left_out": m.left_out,
            "threshold": { "value": m.min_cos, "name": "knowledge.min_cos", "what": "Workbench reminds an agent of one of your decisions, rules or facts when it matches the prompt at least this well" },
            // Project-wide: every agent of the project; what else it would have given at this turn.
            "lower": { "to": to, "more": more.len(), "tokens": more.iter().map(|x| x.1).sum::<i64>(), "notes": more.iter().map(|x| json!({ "text": one_line(&x.2, 140), "score": (x.0 * 100.0).round() / 100.0 })).collect::<Vec<_>>(), "scope": "project" },
        },
        "missing": missing,
    }))
}

// ---------------------------------------------------------------- a turn's blast

/// The turn's change: per file, its hunks (from its commits when it made
/// any, else from its edits), and the functions those lines are in.
fn changes(s: &Session, t: &Turn) -> (Vec<Json>, Vec<(String, String)>, Vec<String>) {
    let dir = &s.dir;
    let base = s.a.base.clone().unwrap_or_else(|| "HEAD".into());
    let until = s.turns.iter().find(|x| x.n == t.n + 1).map_or(t.ended + 5_000, |x| x.started);
    let commits: Vec<String> = git_out(dir, &["log", "--reverse", "--format=%H %ct", &format!("{base}..HEAD")])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(_, ct)| ct.parse::<i64>().is_ok_and(|c| c * 1000 >= t.started - 1000 && c * 1000 <= until + 1000))
        .map(|(h, _)| h.to_string())
        .collect();
    let mut files: Vec<Json> = Vec::new();
    if let (Some(first), Some(last)) = (commits.first(), commits.last()) {
        if let Ok(text) = git_out(dir, &["-c", "core.quotePath=false", "diff", "--no-color", "--no-ext-diff", "-M", &format!("{first}^"), last, "--"]) {
            files = git::parse_diff(&text);
        }
    }
    if files.is_empty() {
        // Its edits, as Claude Code made them.
        let mut by: BTreeMap<String, Vec<Json>> = BTreeMap::new();
        for c in t.calls.iter().filter(|c| story::kind_of(&c.tool) == "edit") {
            let Some(p) = c.input["file_path"].as_str() else { continue };
            let r = rel(p, &s.a.cwd);
            let now = std::fs::read_to_string(dir.join(&r)).unwrap_or_default();
            let mut pairs: Vec<(String, String)> = Vec::new();
            if let Some(es) = c.input["edits"].as_array() {
                pairs.extend(es.iter().map(|e| (e["old_string"].as_str().unwrap_or("").to_string(), e["new_string"].as_str().unwrap_or("").to_string())));
            } else if let Some(content) = c.input["content"].as_str() {
                pairs.push((String::new(), content.lines().take(40).collect::<Vec<_>>().join("\n")));
            } else {
                pairs.push((c.input["old_string"].as_str().unwrap_or("").to_string(), c.input["new_string"].as_str().unwrap_or("").to_string()));
            }
            for (old, new) in pairs {
                let at = if new.is_empty() { None } else { now.find(&new).map(|i| now[..i].lines().count().max(1)) };
                let start = at.unwrap_or(0) as i64;
                let mut lines: Vec<Json> = old.lines().map(|l| json!({ "kind": "-", "text": l, "old_line": Json::Null, "new_line": Json::Null })).collect();
                lines.extend(new.lines().enumerate().map(|(i, l)| json!({ "kind": "+", "text": l, "new_line": if at.is_some() { json!(start + i as i64) } else { Json::Null }, "old_line": Json::Null })));
                if !lines.is_empty() {
                    by.entry(r.clone()).or_default().push(json!({ "new_start": start, "lines": lines }));
                }
            }
        }
        files = by.into_iter().map(|(p, hs)| json!({ "path": p, "status": "modified", "hunks": hs, "from": "edits" })).collect();
    }
    // The functions its added lines are in.
    let mut funcs: Vec<(String, String)> = Vec::new();
    for f in &files {
        let p = f["path"].as_str().unwrap_or("");
        let Some(lang) = blast::Lang::of(p) else { continue };
        let Ok(text) = std::fs::read_to_string(dir.join(p)) else { continue };
        let defs = blast::defs(lang, &text);
        for h in f["hunks"].as_array().into_iter().flatten() {
            for l in h["lines"].as_array().into_iter().flatten().filter(|l| l["kind"] == "+") {
                if let Some(d) = l["new_line"].as_u64().and_then(|n| blast::enclosing(&defs, n as usize)) {
                    let name = format!("{}()", d.name);
                    if !funcs.iter().any(|(fp, n)| fp == p && *n == name) {
                        funcs.push((p.to_string(), name));
                    }
                }
            }
        }
    }
    (files, funcs, commits)
}

/// Callers of `name` outside its definition and tests: how many, and the
/// functions (or files) they are in.
fn callers_of(dir: &Path, name: &str) -> (usize, Vec<String>) {
    let bare = name.trim_end_matches("()");
    if bare.len() < 3 {
        return (0, Vec::new());
    }
    let Ok(out) = git_out(dir, &["grep", "-n", "-w", "-I", "-F", "--untracked", "-e", bare, "--", "."]) else { return (0, Vec::new()) };
    let mut n = 0;
    let mut names: Vec<String> = Vec::new();
    let mut texts: HashMap<String, Option<String>> = HashMap::new();
    for l in out.lines().take(5000) {
        let mut parts = l.splitn(3, ':');
        let (Some(p), Some(ln), Some(text)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Ok(ln) = ln.parse::<usize>() else { continue };
        let lang = blast::Lang::of(p);
        if blast::is_test_path(p) || lang.and_then(|lg| blast::def_of(lg, text)).is_some_and(|(d, _)| d == bare) {
            continue;
        }
        let t = text.trim();
        if t.starts_with("import ") || t.starts_with("from ") || t.starts_with("use ") || t.starts_with("//") || t.starts_with('#') {
            continue;
        }
        n += 1;
        let file = texts.entry(p.to_string()).or_insert_with(|| std::fs::read_to_string(dir.join(p)).ok());
        let who = lang.zip(file.as_deref()).and_then(|(lg, f)| blast::enclosing(&blast::defs(lg, f), ln).map(|d| d.name.clone())).unwrap_or_else(|| p.to_string());
        if who != bare && !names.contains(&who) {
            names.push(who);
        }
    }
    (n, names)
}

/// `GET /api/agents/:id/turns/:n/blast`: what a turn's change reached,
/// worst first.
pub fn turn_blast(sup: &Supervisor, id: &str, n: usize) -> Result<Json, ApiError> {
    let s = load(sup, id)?;
    let t = s.turns.iter().find(|t| t.n == n).ok_or_else(|| ApiError::not_found(format!("{} has no turn {n} ({} so far)", s.a.name, s.turns.len())))?;
    let mut missing = s.missing.clone();
    let (files, funcs, commits) = changes(&s, t);
    if files.is_empty() {
        missing.push(format!("Turn {n} changed no file: nothing it did can have broken a test."));
    }
    let mut callers_total = 0;
    let mut caller_names: Vec<String> = Vec::new();
    for (_, f) in funcs.iter().take(MAX_FUNCS) {
        let (c, names) = callers_of(&s.dir, f);
        callers_total += c;
        for nm in names {
            if !caller_names.contains(&nm) {
                caller_names.push(nm);
            }
        }
    }
    // Tests that failed after it, having passed before it (or new).
    let mut failed_after: Vec<String> = Vec::new();
    if let Some(runs) = blast::test_runs(sup) {
        let mine: Vec<&blast::Run> = runs.iter().filter(|r| r.agent == id).collect();
        for r in mine.iter().filter(|r| r.ts >= t.started && !r.passed) {
            let before = mine.iter().rfind(|x| x.test == r.test && x.ts < t.started);
            if before.is_none_or(|b| b.passed) && !failed_after.contains(&r.test) {
                failed_after.push(r.test.clone());
            }
        }
    }
    // Runtime errors of its app since it.
    let p = chloedb::query::Params::new().set("agent", chloedb::Value::Str(id.to_string()));
    let errors: Vec<String> = sup
        .db()
        .query("SELECT message, first_ms FROM runtime_errors WHERE agent = :agent LIMIT 1000", &p)
        .unwrap_or_default()
        .iter()
        .filter(|r| crate::db::get_int(&r[1]).is_some_and(|ms| ms >= t.started))
        .filter_map(|r| crate::db::get_str(&r[0]).map(|m| one_line(&m, 80)))
        .collect();
    // Whether it reached the landing branch.
    let main = git::checkout(&s.dir).map(|c| c.main).unwrap_or_else(|_| PathBuf::from(&s.a.repo));
    let onto = blast::landing_branch(&main, &s.a).unwrap_or_else(|| "main".into());
    let tip = git::rev_parse(&main, &format!("refs/heads/{onto}"));
    let branch = s.a.branch.clone().unwrap_or_else(|| "its branch".into());
    let reached: Vec<&String> = commits.iter().filter(|c| tip.as_deref().is_some_and(|tp| git::is_ancestor(&main, c, tp))).collect();
    let reached_note = if !reached.is_empty() {
        format!("{} of it on {onto}", plural(reached.len(), "commit"))
    } else if commits.is_empty() {
        format!("its edits were not committed in this turn; {branch} is not merged")
    } else {
        format!("{branch} is not merged")
    };
    let fn_list: Vec<String> = funcs.iter().map(|(_, f)| f.clone()).collect();
    let changed = if fn_list.is_empty() { files.iter().filter_map(|f| f["path"].as_str().map(str::to_string)).take(2).collect::<Vec<_>>().join(", ") } else { fn_list.iter().take(2).cloned().collect::<Vec<_>>().join(", ") };
    let verdict = format!(
        "Turn {n} changed {}: {} reached, {} caught it, {} reached {onto}.",
        if changed.is_empty() { "nothing".into() } else { changed },
        plural(callers_total, "caller"),
        plural(failed_after.len(), "test"),
        reached.len()
    );
    let tone = if !failed_after.is_empty() || !errors.is_empty() || !reached.is_empty() { "crit" } else if callers_total > 0 { "warn" } else { "good" };
    let sev = |t: &str| match t {
        "crit" => 0,
        "warn" => 1,
        "" => 2,
        _ => 3,
    };
    let mut rows = vec![
        json!({ "n": failed_after.len(), "what": "tests failed after it", "names": failed_after, "tone": if failed_after.is_empty() { "ok" } else { "crit" } }),
        json!({ "n": callers_total, "what": "callers reached", "names": caller_names.iter().take(8).collect::<Vec<_>>(), "more": caller_names.len().saturating_sub(8), "tone": if callers_total > 0 { "warn" } else { "ok" } }),
        json!({ "n": fn_list.len(), "what": if fn_list.len() == 1 { "function changed" } else { "functions changed" }, "names": fn_list, "tone": "", "code": true }),
        json!({ "n": errors.len(), "what": "runtime errors blamed on it", "names": if errors.is_empty() { json!(["none in its app's log"]) } else { json!(errors) }, "tone": if errors.is_empty() { "ok" } else { "crit" } }),
        json!({ "n": reached.len(), "what": format!("reached {onto}"), "names": [reached_note], "tone": if reached.is_empty() { "ok" } else { "crit" } }),
    ];
    rows.sort_by_key(|r| (sev(r["tone"].as_str().unwrap_or("")), std::cmp::Reverse(r["n"].as_u64().unwrap_or(0))));
    // The lines that matter: the first added line in each changed function.
    let mut diff = files.clone();
    let hot: HashSet<(String, i64)> = funcs
        .iter()
        .filter_map(|(p, f)| {
            let text = std::fs::read_to_string(s.dir.join(p)).ok()?;
            let d = blast::defs(blast::Lang::of(p)?, &text).into_iter().find(|d| format!("{}()", d.name) == *f)?;
            Some((p.clone(), d.line as i64, d.end as i64))
        })
        .flat_map(|(p, from, to)| {
            files.iter().filter(|f| f["path"] == p.as_str()).flat_map(|f| f["hunks"].as_array().cloned().unwrap_or_default()).flat_map(|h| h["lines"].as_array().cloned().unwrap_or_default()).filter_map(|l| l["new_line"].as_i64().filter(|n| l["kind"] == "+" && *n >= from && *n <= to)).min().map(|n| (p.clone(), n))
        })
        .collect();
    for f in diff.iter_mut() {
        let p = f["path"].as_str().unwrap_or("").to_string();
        for h in f["hunks"].as_array_mut().into_iter().flatten() {
            for l in h["lines"].as_array_mut().into_iter().flatten() {
                if l["new_line"].as_i64().is_some_and(|nl| hot.contains(&(p.clone(), nl))) {
                    l["cur"] = json!(true);
                }
            }
        }
    }
    Ok(json!({
        "agent": trail::agent_json(sup, &s.a), "turn": n, "verdict": verdict, "tone": tone, "rows": rows,
        "diff": { "files": diff, "commits": commits.iter().map(|c| c[..c.len().min(7)].to_string()).collect::<Vec<_>>(), "from": if commits.is_empty() { "edits" } else { "commits" } },
        "files": t.edited.iter().collect::<Vec<_>>(), "functions": funcs.iter().map(|(p, f)| json!({ "path": p, "name": f })).collect::<Vec<_>>(),
        "missing": missing,
    }))
}

// ---------------------------------------------------------------- sessions

/// `GET /api/debug/sessions?repo=`: the project's agents, newest first,
/// with their turns and whether a test broke on them.
pub fn sessions(sup: &Supervisor, repo: &str) -> Result<Json, ApiError> {
    let repo = crate::review::repo_of(sup, repo)?;
    let repo_s = repo.to_string_lossy().to_string();
    let mut list: Vec<Agent> = sup.agent_rows().into_iter().filter(|a| a.repo == repo_s).collect();
    list.sort_by_key(|a| std::cmp::Reverse(a.created));
    let out: Vec<Json> = list
        .iter()
        .map(|a| {
            let evs = sup.all_events(&a.id).unwrap_or_default();
            let turns = evs.iter().filter(|e| e.kind == "UserPromptSubmit").count();
            let br = breaks(sup, &a.id);
            json!({ "agent": trail::agent_json(sup, a), "turns": turns, "broke": br.iter().map(|(t, _)| t.clone()).take(5).collect::<Vec<_>>(), "flagged": !br.is_empty() || !flags_of(sup, &a.id).is_empty(), "flags": flags_of(sup, &a.id) })
        })
        .collect();
    let flagged = out.iter().find(|s| s["flagged"] == true).or_else(|| out.iter().find(|s| s["turns"].as_u64().unwrap_or(0) > 0)).map(|s| s["agent"]["id"].clone());
    Ok(json!({ "repo": repo, "sessions": out, "flagged": flagged }))
}

// ---------------------------------------------------------------- records

#[derive(Deserialize)]
struct RecordsQ {
    repo: Option<String>,
    kind: Option<String>,
    by: Option<String>,
    branch: Option<String>,
    superseded: Option<String>,
    id: Option<String>,
}

async fn records_route(State(app): State<App>, Query(q): Query<RecordsQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || records(&sup, &q)).await?).into_response())
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() < 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'))
}

fn sql_rows(sup: &Supervisor, dir: &Path, sql: &str) -> Result<Vec<HashMap<String, Json>>, String> {
    match trail::run_chloectx(&sup.cfg.chloectx_bin, dir, &["sql", "--json", sql], Duration::from_secs(30)) {
        Ran::Out(v) => {
            let cols: Vec<String> = v["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
            Ok(v["rows"].as_array().into_iter().flatten().filter_map(Json::as_array).map(|r| cols.iter().cloned().zip(r.iter().cloned()).collect()).collect())
        }
        Ran::Missing => Err(trail::not_installed(&sup.cfg.chloectx_bin)),
        Ran::Failed(e) => Err(e),
    }
}

const RECORD_COLS: &str = "id, session, kind, tag, ts, files, text, ctx, links";

/// Who said a record, from its context's `By:` line: `you`, an agent's
/// name, or Workbench.
fn said_by(r: &HashMap<String, Json>, session_agent: Option<&Agent>) -> (String, String) {
    let ctx = r.get("ctx").and_then(Json::as_str).unwrap_or("");
    let by = ctx.lines().find_map(|l| l.strip_prefix("By: ")).map(str::trim).unwrap_or("");
    let name = |rest: &str| rest.trim_start_matches(':').trim().to_string();
    if by.is_empty() {
        return ("unknown".into(), "not recorded".into());
    }
    if by == "workbench" {
        return ("workbench".into(), "Workbench".into());
    }
    if let Some(rest) = by.strip_prefix("agent") {
        let n = name(rest);
        return (format!("agent:{n}"), if n.is_empty() { "an agent".into() } else { n });
    }
    if let Some(rest) = by.strip_prefix("assistant") {
        let n = name(rest);
        let n = if n.is_empty() { session_agent.map(|a| a.name.clone()).unwrap_or_default() } else { n };
        return (format!("assistant:{n}"), if n.is_empty() { "an agent".into() } else { n });
    }
    ("user".into(), "you".into())
}

fn record_json(r: &HashMap<String, Json>, s: &trail::Sessions) -> Json {
    let g = |k: &str| r.get(k).cloned().unwrap_or(Json::Null);
    let id = g("id").as_str().unwrap_or("").to_string();
    let ag = g("session").as_str().and_then(|sid| s.agent(sid)).or_else(|| s.of_record(&id));
    let (by, by_label) = said_by(r, ag);
    let links = g("links").to_string();
    let superseded = g("tag") == "superseded" || links.contains("superseded_by");
    let files: Vec<String> = g("files").as_str().unwrap_or("").split_whitespace().map(str::to_string).collect();
    json!({
        "id": id, "kind": g("kind"), "text": g("text"), "ts": trail::ctx_time(&g("ts")), "tag": g("tag"),
        "by": by, "by_label": by_label,
        "agent": ag.map(|a| json!({ "id": a.id, "name": a.name })), "branch": ag.and_then(|a| a.branch.clone()),
        "superseded": superseded, "task_only": g("tag") == "task", "files": files,
        "pinned": g("tag") == "remembered" && r.get("files").and_then(Json::as_str).is_some_and(|f| !f.trim().is_empty()),
    })
}

/// `GET /api/records?repo=&kind=&by=&branch=&superseded=`: what agents
/// remember about the project, newest first, with who said each.
fn records(sup: &Supervisor, q: &RecordsQ) -> Result<Json, ApiError> {
    let repo = crate::review::repo_of(sup, q.repo.as_deref().unwrap_or(""))?;
    let mut sql = format!("SELECT {RECORD_COLS} FROM memory WHERE (kind = 'decision' OR kind = 'preference' OR kind = 'fact' OR kind = 'error' OR kind = 'note')");
    if let Some(id) = q.id.as_deref().filter(|i| safe_id(i)) {
        sql = format!("SELECT {RECORD_COLS} FROM memory WHERE id = '{id}'");
    }
    sql += " ORDER BY ts DESC LIMIT 400";
    let rows = match sql_rows(sup, &repo, &sql) {
        Ok(r) => r,
        Err(e) => return Ok(json!({ "repo": repo, "records": [], "facets": {}, "missing": [e] })),
    };
    let s = trail::Sessions::load(sup);
    let all: Vec<Json> = rows.iter().map(|r| record_json(r, &s)).collect();
    let mut facets: BTreeMap<&str, BTreeMap<String, usize>> = BTreeMap::new();
    for r in &all {
        *facets.entry("kind").or_default().entry(r["kind"].as_str().unwrap_or("").to_string()).or_default() += 1;
        *facets.entry("by").or_default().entry(r["by_label"].as_str().unwrap_or("").to_string()).or_default() += 1;
        if let Some(b) = r["branch"].as_str() {
            *facets.entry("branch").or_default().entry(b.to_string()).or_default() += 1;
        }
    }
    let keep = |r: &Json| {
        q.kind.as_deref().filter(|k| !k.is_empty()).is_none_or(|k| r["kind"] == k)
            && q.by.as_deref().filter(|k| !k.is_empty()).is_none_or(|k| r["by_label"] == k || r["by"] == k)
            && q.branch.as_deref().filter(|k| !k.is_empty()).is_none_or(|k| r["branch"] == k)
            && match q.superseded.as_deref() {
                Some("1" | "true" | "yes") => true,
                Some("only") => r["superseded"] == true,
                _ => r["superseded"] != true,
            }
    };
    let list: Vec<&Json> = all.iter().filter(|r| keep(r)).collect();
    let superseded = all.iter().filter(|r| r["superseded"] == true).count();
    Ok(json!({ "repo": repo, "records": list, "total": all.len(), "superseded": superseded, "facets": facets, "missing": [] }))
}

#[derive(Deserialize)]
struct PatchB {
    /// Correct: the record's new text (it replaces the record).
    text: Option<String>,
    /// Pin: a file whose edits always bring the record (a copy that names it).
    pin: Option<String>,
    /// Make it a rule: every agent of the project is told it.
    rule: Option<bool>,
}

async fn record_patch(State(app): State<App>, AxPath(id): AxPath<String>, Query(q): Query<RepoQ>, body: Bytes) -> ApiResult {
    let b: PatchB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || patch(&sup, &id, q.repo.as_deref().unwrap_or(""), b)).await?).into_response())
}

async fn record_delete(State(app): State<App>, AxPath(id): AxPath<String>, Query(q): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || forget(&sup, &id, q.repo.as_deref().unwrap_or(""))).await?).into_response())
}

fn rpc(sup: &Supervisor, dir: &Path, req: &Json) -> Result<Json, ApiError> {
    match trail::run_chloectx(&sup.cfg.chloectx_bin, dir, &["rpc", &req.to_string()], Duration::from_secs(60)) {
        Ran::Out(v) => Ok(v),
        Ran::Missing => Err(ApiError::new(409, "chloectx_missing", trail::not_installed(&sup.cfg.chloectx_bin))),
        Ran::Failed(e) => Err(ApiError::new(502, "chloectx_failed", e)),
    }
}

fn record_row(sup: &Supervisor, dir: &Path, id: &str) -> Result<HashMap<String, Json>, ApiError> {
    if !safe_id(id) {
        return Err(ApiError::bad(format!("{id:?} is not a record id")));
    }
    let rows = sql_rows(sup, dir, &format!("SELECT {RECORD_COLS} FROM memory WHERE id = '{id}' LIMIT 1")).map_err(|e| ApiError::new(502, "chloectx_failed", e))?;
    rows.into_iter().next().ok_or_else(|| ApiError::not_found(format!("no record {id}")))
}

/// `PATCH /api/records/:id?repo=`: correct it, pin it to a file, or make it a rule.
fn patch(sup: &Supervisor, id: &str, repo: &str, b: PatchB) -> Result<Json, ApiError> {
    let dir = crate::review::repo_of(sup, repo)?;
    let r = record_row(sup, &dir, id)?;
    let kind = r.get("kind").and_then(Json::as_str).unwrap_or("decision").to_string();
    let old_text = r.get("text").and_then(Json::as_str).unwrap_or("").to_string();
    let mut files: Vec<String> = r.get("files").and_then(Json::as_str).unwrap_or("").split_whitespace().map(str::to_string).collect();
    let by = r.get("ctx").and_then(Json::as_str).unwrap_or("").lines().find_map(|l| l.strip_prefix("By: ")).map(|s| s.trim().to_string());
    let mut done = Vec::new();
    let mut new_id: Option<Json> = None;
    if let Some(text) = b.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        // A correction is yours: the new text replaces the record.
        let v = rpc(sup, &dir, &json!({ "op": "remember", "kind": kind, "text": text, "supersedes": id, "by": "user", "files": files }))?;
        new_id = v.get("id").cloned().or_else(|| v.get("ids").cloned());
        done.push(format!("Corrected: “{}” replaces it.", one_line(text, 80)));
    }
    if let Some(pin) = b.pin.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        if pin.contains("..") || pin.starts_with('/') {
            return Err(ApiError::bad("pin is a path inside the project, such as cart.py"));
        }
        if !files.iter().any(|f| f == pin) {
            files.push(pin.to_string());
        }
        // The same words, by the same author, now naming the file: ChloeCtx
        // gives a record about a file before an agent edits it.
        let text = b.text.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(&old_text).to_string();
        let mut req = json!({ "op": "remember", "kind": kind, "text": text, "supersedes": new_id.as_ref().and_then(Json::as_str).unwrap_or(id), "files": files });
        req["by"] = json!(by.clone().unwrap_or_else(|| "user".into()));
        rpc(sup, &dir, &req)?;
        done.push(format!("Pinned to {pin}: given whenever an agent edits it."));
    }
    let mut rule = Json::Null;
    if b.rule == Some(true) {
        let text = b.text.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(&old_text).to_string();
        let (r, missing) = crate::questions::add_rule(sup, &dir.to_string_lossy(), &text, None);
        rule = json!({ "rule": r.to_json(), "missing": missing });
        done.push("Made a rule: every agent of the project is told it at session start.".into());
    }
    if done.is_empty() {
        return Err(ApiError::bad("say what to do: text (correct), pin (a file) or rule: true"));
    }
    Ok(json!({ "ok": true, "id": id, "done": done, "rule": rule }))
}

/// `DELETE /api/records/:id?repo=`: forget it (ChloeCtx scrubs it from its receipts too).
fn forget(sup: &Supervisor, id: &str, repo: &str) -> Result<Json, ApiError> {
    let dir = crate::review::repo_of(sup, repo)?;
    if !safe_id(id) {
        return Err(ApiError::bad(format!("{id:?} is not a record id")));
    }
    let v = rpc(sup, &dir, &json!({ "op": "forget", "id": id }))?;
    Ok(json!({ "ok": true, "id": id, "text": v["text"] }))
}

// ---------------------------------------------------------------- threshold

/// The store's config.json, found through the daemon (`stats` names its file).
fn config_path(sup: &Supervisor, dir: &Path) -> Result<PathBuf, ApiError> {
    let v = rpc(sup, dir, &json!({ "op": "stats" }))?;
    let db = v["store"].as_str().ok_or_else(|| ApiError::new(502, "chloectx_failed", "chloectx stats named no store"))?;
    Ok(Path::new(db).parent().unwrap_or(Path::new(".")).join("config.json"))
}

fn read_config(p: &Path) -> Json {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).filter(Json::is_object).unwrap_or_else(|| json!({}))
}

async fn threshold_get(State(app): State<App>, AxPath(p): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || {
        let dir = crate::review::repo_of(&sup, &p)?;
        let cfg = config_path(&sup, &dir)?;
        let v = read_config(&cfg)["knowledge"]["min_cos"].as_f64().unwrap_or(DEFAULT_MIN_COS);
        Ok(json!({ "repo": dir, "value": v, "default": DEFAULT_MIN_COS, "name": "knowledge.min_cos", "file": cfg }))
    })
    .await?)
    .into_response())
}

#[derive(Deserialize)]
struct ThresholdB {
    value: f64,
}

async fn threshold_put(State(app): State<App>, AxPath(p): AxPath<String>, body: Bytes) -> ApiResult {
    let b: ThresholdB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || set_threshold(&sup, &p, b.value)).await?).into_response())
}

/// `PUT /api/repos/:p/threshold {value}`: ChloeCtx's knowledge threshold
/// for the project's store, written to its config.json; its daemon is
/// stopped so the next hook starts it with the new value.
fn set_threshold(sup: &Supervisor, p: &str, value: f64) -> Result<Json, ApiError> {
    if !(0.05..=0.95).contains(&value) {
        return Err(ApiError::bad("value is a similarity between 0.05 and 0.95"));
    }
    let dir = crate::review::repo_of(sup, p)?;
    let cfg = config_path(sup, &dir)?;
    let mut c = read_config(&cfg);
    let was = c["knowledge"]["min_cos"].as_f64().unwrap_or(DEFAULT_MIN_COS);
    if !c["knowledge"].is_object() {
        c["knowledge"] = json!({});
    }
    let value = (value * 100.0).round() / 100.0;
    c["knowledge"]["min_cos"] = json!(value);
    let tmp = cfg.with_extension("json.workbench");
    std::fs::write(&tmp, serde_json::to_string_pretty(&c).unwrap_or_default()).and_then(|_| std::fs::rename(&tmp, &cfg)).map_err(|e| ApiError::internal(format!("cannot write {}: {e}", cfg.display())))?;
    let _ = trail::run_raw(&sup.cfg.chloectx_bin, &dir, &["stop"], Duration::from_secs(20));
    Ok(json!({ "ok": true, "repo": dir, "value": value, "was": was, "undo": { "value": was }, "scope": "project", "file": cfg, "text": format!("Every agent of this project is now reminded of a decision, rule or fact from a {:.0}% match (was {:.0}%), from its next prompt.", value * 100.0, was * 100.0) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_said_a_record() {
        let r = |ctx: &str| -> HashMap<String, Json> { [("ctx".to_string(), json!(ctx))].into_iter().collect() };
        assert_eq!(said_by(&r("By: user"), None).1, "you");
        assert_eq!(said_by(&r("When: x\nBy: agent:lead"), None), ("agent:lead".into(), "lead".into()));
        assert_eq!(said_by(&r("By: assistant:freeship"), None).1, "freeship");
        assert_eq!(said_by(&r(""), None).1, "not recorded");
        assert!(about("Shipping is not taxed: cart.total() taxes items only", &["cart".into()]));
        assert!(!about("Money is integer cents", &["cart".into()]));
    }
}
