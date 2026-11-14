//! Model routing that learns (SPEC.md, "Fourth build", row 6): which model
//! tasks like this one did best on in this repository.
//!
//! When an agent exits, or ends a turn with its outcome done, a row of
//! `task_features` keeps what its task was: its text, the files it edited,
//! its model, the tokens its transcripts say it used and what they cost,
//! how long it ran, how often it was prompted again or steered, and
//! whether it finished. `GET /api/route?repo=&task=` ranks the
//! repository's past tasks against the new task's words with BM25 in
//! ChloeDB, and weighs each model by how often it finished the similar
//! ones and what it cost: the cheapest model about as likely to finish as
//! the best is the suggestion. The New agent dialog shows it; an agent's
//! `spawn_agent` without a model takes it only when 5 or more past tasks
//! are similar, else the default model.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};
use tokio::sync::broadcast::error::RecvError;

use crate::db::{get_int, get_ms, get_str, iso, now_ms, opt_int, opt_str, ts};
use crate::server::{blocking, ApiResult, App};
use crate::supervisor::{Change, Supervisor};
use crate::{map, narrate, trail};

/// Past tasks similar enough before `spawn_agent` follows the suggestion.
pub const APPLY_AT: usize = 5;
/// The most of a transcript read, from its end.
const TRANSCRIPT_MAX: u64 = 64 << 20;
/// Rate of finishing within which a cheaper model is as good.
const AS_GOOD: f64 = 0.1;

/// Dollars per million tokens: input, output, cache reads. Claude Code's
/// aliases are today's models (Haiku 5.5, Sonnet 5.5, Opus 5.5); the
/// default and any other name are priced as Opus 5.5, the default model.
fn price(model: &str) -> (f64, f64, f64) {
    match model {
        "haiku" => (0.10, 0.50, 0.01),
        "sonnet" => (2.0, 10.0, 0.10),
        _ => (4.0, 20.0, 0.20),
    }
}

/// A model's name as routing groups it: haiku, sonnet, opus, the name
/// given, or `default` when none was.
pub fn model_of(m: Option<&str>) -> String {
    let m = m.map(str::trim).unwrap_or("").to_ascii_lowercase();
    for k in ["haiku", "sonnet", "opus"] {
        if m.contains(k) {
            return k.into();
        }
    }
    if m.is_empty() {
        "default".into()
    } else {
        m
    }
}

pub fn cost_usd(model: &str, input: i64, output: i64, cache_read: i64) -> f64 {
    let (i, o, c) = price(model);
    (input as f64 * i + output as f64 * o + cache_read as f64 * c) / 1e6
}

// Recording.

/// The features of agent `id`'s task, written to `task_features`.
pub fn record(sup: &Supervisor, id: &str) {
    let Some(a) = sup.agent_row(id) else { return };
    let Some(task) = a.task.as_deref().map(str::trim).filter(|t| !t.is_empty()) else { return };
    let evs = sup.all_events(id).unwrap_or_default();
    let cwd = PathBuf::from(&a.cwd);
    let main = PathBuf::from(&a.repo);
    let mut files: Vec<String> = map::edited(&evs, &cwd, &main).into_iter().collect();
    files.sort();
    let mut seen = HashSet::new();
    let (mut input, mut output, mut cache) = (0, 0, 0);
    let paths: HashSet<String> = evs.iter().filter_map(|e| trail::data_of(e).and_then(|d| d["transcript_path"].as_str().map(str::to_string))).collect();
    for p in paths.iter().filter(|p| narrate::transcript_ok(p)) {
        let (i, o, c) = narrate::tokens(&narrate::read_tail(Path::new(p), TRANSCRIPT_MAX), &mut seen);
        input += i;
        output += o;
        cache += c;
    }
    let model = model_of(a.model.as_deref());
    let start = a.started_ms.unwrap_or(a.created);
    let end = a.ended.or_else(|| evs.last().map(|e| e.ts)).unwrap_or_else(now_ms);
    let prompts = evs.iter().filter(|e| e.kind == "UserPromptSubmit").count() as i64;
    let steers = evs.iter().filter(|e| e.kind == "steer").count() as i64;
    let outcome = a.outcome.as_deref().and_then(|o| crate::goals::outcome(sup, o).ok()).map(|o| o.state);
    // Finished: its outcome is done; without one, its last turn ended (a
    // Stop after its last prompt) and it was not left asking.
    let last_turn = evs.iter().rev().find(|e| matches!(e.kind.as_str(), "Stop" | "UserPromptSubmit" | "Notification")).map(|e| e.kind.as_str());
    let done = match outcome.as_deref() {
        Some(s) => s == "done",
        None => last_turn == Some("Stop"),
    };
    let p = Params::new()
        .set("id", Value::Str(a.id.clone()))
        .set("repo", Value::Str(a.repo.clone()))
        .set("name", Value::Str(a.name.clone()))
        .set("task", Value::Str(task.to_string()))
        .set("files", Value::Str(files.join("\n")))
        .set("model", Value::Str(model.clone()))
        .set("input", Value::Int(input))
        .set("output", Value::Int(output))
        .set("cache_read", Value::Int(cache))
        .set("cost_usd", Value::Float(cost_usd(&model, input, output, cache)))
        .set("minutes", Value::Float(((end - start).max(0)) as f64 / 60_000.0))
        .set("redo", Value::Int((prompts - 1).max(0) + steers))
        .set("done", Value::Bool(done))
        .set("outcome", opt_str(&outcome))
        .set("exit_code", opt_int(a.exit_code))
        .set("ended", ts(end));
    let _ = sup.db().query(
        "INSERT OR REPLACE INTO task_features (id, repo, name, task, files, model, input, output, cache_read, cost_usd, minutes, redo, done, outcome, exit_code, ended) VALUES (:id, :repo, :name, :task, :files, :model, :input, :output, :cache_read, :cost_usd, :minutes, :redo, :done, :outcome, :exit_code, :ended)",
        &p,
    );
}

/// Starts the recorder: a task's features when its agent exits, or ends a
/// turn with its outcome done; first those of agents that ended before.
pub fn start(sup: &Arc<Supervisor>) {
    let sup = sup.clone();
    let mut rx = sup.changes.subscribe();
    let _ = std::thread::Builder::new().name("routing".into()).spawn(move || {
        let have: HashSet<String> = sup.db().query("SELECT id FROM task_features LIMIT 1000000", &Params::new()).unwrap_or_default().iter().filter_map(|r| get_str(&r[0])).collect();
        for a in sup.agent_rows().iter().filter(|a| a.ended.is_some() && !have.contains(&a.id)) {
            record(&sup, &a.id);
        }
        loop {
            match rx.blocking_recv() {
                Ok(Change::Event { agent, json }) => {
                    let kind = json["kind"].as_str().unwrap_or("");
                    let outcome_done = || sup.agent_row(&agent).and_then(|a| a.outcome).and_then(|o| crate::goals::outcome(&sup, &o).ok()).is_some_and(|o| o.state == "done");
                    if kind == "exited" || (kind == "Stop" && outcome_done()) {
                        record(&sup, &agent);
                    }
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
}

// Suggesting.

/// The words of a task the search ranks by: three letters or more, not
/// the commonest.
fn words(s: &str) -> Vec<String> {
    const COMMON: [&str; 40] = [
        "the", "and", "for", "with", "not", "but", "you", "your", "this", "that", "from", "into", "then", "than", "are", "was", "its", "all", "any", "one", "use",
        "make", "sure", "when", "what", "which", "have", "has", "will", "should", "can", "also", "just", "each", "only", "more", "some", "our", "out", "them",
    ];
    let mut out: Vec<String> = Vec::new();
    for w in s.split(|c: char| !c.is_alphanumeric()).map(str::to_lowercase) {
        if w.chars().count() >= 3 && !COMMON.contains(&w.as_str()) && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

#[derive(Clone, Debug)]
struct Past {
    id: String,
    name: String,
    task: String,
    model: String,
    done: bool,
    cost: f64,
    minutes: f64,
    tokens: i64,
    ended: i64,
    score: f64,
}

#[derive(Default)]
struct Tally {
    n: usize,
    done: usize,
    cost: f64,
    minutes: f64,
    tokens: i64,
}

impl Tally {
    fn rate(&self) -> f64 {
        // Laplace's rule: one finished and one not, before any is seen.
        (self.done as f64 + 1.0) / (self.n as f64 + 2.0)
    }

    fn to_json(&self, model: &str) -> Json {
        let n = self.n.max(1) as f64;
        json!({
            "model": model, "n": self.n, "done": self.done, "rate": (self.done as f64 / n * 100.0).round() / 100.0,
            "avg_cost_usd": (self.cost / n * 100.0).round() / 100.0, "avg_minutes": (self.minutes / n * 10.0).round() / 10.0,
            "avg_tokens": (self.tokens as f64 / n).round() as i64,
        })
    }
}

fn nice(m: &str) -> String {
    match m {
        "haiku" => "Haiku".into(),
        "sonnet" => "Sonnet".into(),
        "opus" => "Opus".into(),
        "default" => "The default model".into(),
        m => m.to_string(),
    }
}

fn usd(x: f64) -> String {
    format!("${x:.2}")
}

/// `GET /api/route`: the suggestion for `task` in `repo`.
pub fn suggest(sup: &Supervisor, repo: &str, task: &str) -> Json {
    let given = task.split_whitespace().count();
    let ws = words(task);
    if given < 3 || ws.is_empty() {
        return json!({ "available": true, "similar": 0, "apply": false, "suggestion": null, "alternatives": [], "examples": [], "default": "default", "missing": [], "why": "Say a few words more about the task to see which model did best on tasks like it." });
    }
    let need = if ws.len() >= 3 { 2 } else { 1 };
    let sql = format!(
        "SELECT id, name, task, model, done, cost_usd, minutes, input, output, ended, score() FROM task_features WHERE repo = :repo ORDER BY bm25(task, :q, syntax => 'plain', min => {need}) DESC LIMIT 40"
    );
    let p = Params::new().set("repo", Value::Str(repo.to_string())).set("q", Value::Str(ws.join(" ")));
    let (rows, missing) = match sup.db().query(&sql, &p) {
        Ok(r) => (r, Vec::new()),
        Err(e) => (Vec::new(), vec![e]),
    };
    let mut past: Vec<Past> = rows
        .iter()
        .map(|r| Past {
            id: get_str(&r[0]).unwrap_or_default(),
            name: get_str(&r[1]).unwrap_or_default(),
            task: get_str(&r[2]).unwrap_or_default(),
            model: get_str(&r[3]).unwrap_or_else(|| "default".into()),
            done: get_int(&r[4]).unwrap_or(0) != 0,
            cost: match &r[5] {
                Value::Float(f) => *f,
                v => get_int(v).unwrap_or(0) as f64,
            },
            minutes: match &r[6] {
                Value::Float(f) => *f,
                v => get_int(v).unwrap_or(0) as f64,
            },
            tokens: get_int(&r[7]).unwrap_or(0) + get_int(&r[8]).unwrap_or(0),
            ended: get_ms(&r[9]).unwrap_or(0),
            score: match &r[10] {
                Value::Float(f) => *f,
                v => get_int(v).unwrap_or(0) as f64,
            },
        })
        .collect();
    // Similar: within reach of the best match.
    let top = past.first().map_or(0.0, |p| p.score);
    past.retain(|p| p.score >= top * 0.3);
    let mut by: BTreeMap<String, Tally> = BTreeMap::new();
    for p in &past {
        let t = by.entry(p.model.clone()).or_default();
        t.n += 1;
        t.done += p.done as usize;
        t.cost += p.cost;
        t.minutes += p.minutes;
        t.tokens += p.tokens;
    }
    let mut alts: Vec<(&String, &Tally)> = by.iter().collect();
    alts.sort_by(|a, b| b.1.rate().total_cmp(&a.1.rate()).then(a.1.cost.total_cmp(&b.1.cost)));
    let best = alts.first().map(|a| a.1.rate()).unwrap_or(0.0);
    let pick = alts
        .iter()
        .filter(|a| a.1.rate() >= best - AS_GOOD)
        .min_by(|a, b| (a.1.cost / a.1.n.max(1) as f64).total_cmp(&(b.1.cost / b.1.n.max(1) as f64)))
        .map(|a| (a.0.clone(), a.1));
    let similar = past.len();
    // Sixth build: fewer than APPLY_AT similar tasks is no basis for a
    // suggestion ("0 of 1 like this finished"): say so, suggest nothing.
    let thin = similar > 0 && similar < APPLY_AT;
    let pick = if thin { None } else { pick };
    let why = match (&pick, alts.first()) {
        (None, _) if thin => format!(
            "Not enough history: {} similar past task{} in this repository, and Workbench suggests a model from {APPLY_AT}. The model is your choice.",
            similar,
            if similar == 1 { "" } else { "s" }
        ),
        (None, _) => "No past task of this repository is like this one yet: the model is your choice.".to_string(),
        (Some((m, t)), Some((bm, bt))) if *bm != m => format!(
            "{} finished {} of {} tasks like this at {} on average; {} finished {} of {} at {}: about as often, for less.",
            nice(m),
            t.done,
            t.n,
            usd(t.cost / t.n.max(1) as f64),
            nice(bm),
            bt.done,
            bt.n,
            usd(bt.cost / bt.n.max(1) as f64)
        ),
        (Some((m, t)), _) if alts.len() > 1 => format!("{} finished {} of {} tasks like this, more often than the others did, at {} on average.", nice(m), t.done, t.n, usd(t.cost / t.n.max(1) as f64)),
        (Some((m, t)), _) => format!("Only {} has done tasks like this here: it finished {} of {}, at {} on average.", if m == "default" { "the default model".to_string() } else { nice(m) }, t.done, t.n, usd(t.cost / t.n.max(1) as f64)),
    };
    json!({
        "available": true,
        "similar": similar,
        "enough": similar >= APPLY_AT,
        "apply": similar >= APPLY_AT && pick.is_some(),
        "suggestion": pick.as_ref().map(|(m, t)| t.to_json(m)),
        "why": why,
        "alternatives": alts.iter().map(|(m, t)| t.to_json(m)).collect::<Vec<_>>(),
        "examples": past.iter().take(5).map(|p| json!({
            "agent": p.id, "name": p.name, "task": p.task, "model": p.model, "done": p.done,
            "cost_usd": (p.cost * 100.0).round() / 100.0, "minutes": (p.minutes * 10.0).round() / 10.0, "ended": iso(p.ended), "score": p.score,
        })).collect::<Vec<_>>(),
        "default": "default",
        "missing": missing,
    })
}

pub fn routes() -> Router<App> {
    Router::new().route("/api/route", get(route_get))
}

#[derive(Deserialize)]
struct RouteQ {
    repo: Option<String>,
    task: Option<String>,
}

async fn route_get(State(app): State<App>, Query(q): Query<RouteQ>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || {
        let main = trail::repo_dir(q.repo.as_deref())?;
        Ok(suggest(&sup, &main.to_string_lossy(), q.task.as_deref().unwrap_or("")))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_and_prices() {
        assert_eq!(model_of(Some("claude-haiku-5-5")), "haiku");
        assert_eq!(model_of(Some("Sonnet")), "sonnet");
        assert_eq!(model_of(None), "default");
        assert_eq!(model_of(Some("")), "default");
        assert!((cost_usd("haiku", 1_000_000, 1_000_000, 0) - 0.60).abs() < 1e-9);
        assert!((cost_usd("default", 1_000_000, 0, 1_000_000) - 4.20).abs() < 1e-9);
        assert_eq!(words("Add the saved_at column to carts, and a test"), vec!["add", "saved", "column", "carts", "test"]);
    }
}
