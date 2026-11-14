//! Trace (SPEC.md, "Trace (as built)"): a question in a sentence ("when and
//! why did we change x to y, and how does it work now?") answered with a
//! dossier of where it happened, without a model.
//!
//! The question is read by rules (quoted text, identifiers, `code`, file
//! names, "from x to y", "x to y", "x → y", "in api/", and the kind: when,
//! why, how, who); the reading comes back as chips the user can fix, and
//! each chip can be passed back to override it. Then:
//!
//! - **when**: the commits where x became y (or where the subject changed),
//!   from git's pickaxe (`git log --all -G`, so agents' `wb/*` branches and
//!   worktrees count), each mapped to the agent whose branch brought it or
//!   whose session ran `git commit` then; and the edits Workbench saw
//!   (PostToolUse of Edit, MultiEdit, Write with their old and new text),
//!   folded into the commit that holds them, else kept as not committed;
//! - **why**: ChloeCtx's decisions (`chloectx ask --json` with the question,
//!   `chloectx sql --json` for the records that name the subject, `chloectx
//!   blame --json` of the line as it is now), Decide's questions and answers
//!   and rules that name it, and the goal and outcome the agent was on;
//! - **the state then**: links to the agent's Story and the Rewind of its
//!   terminal at that moment, and (the UI checks `GET /api/time`) Mission
//!   and Map as of then;
//! - **how it works now**: the code that holds the subject now, by BM25
//!   over the Search part's `code_chunks` with `PACK n TOKENS` when that
//!   table exists, else `git grep` in the main checkout and the function
//!   around each hit; and pages of `doc_pages` when that table exists;
//! - a one-line answer assembled from the evidence. `POST
//!   /api/trace/explain` asks `chloectx ask --deep --json` (a model) for a
//!   fuller account citing the evidence, only when the user asks.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json_};

use crate::db::{get_int, get_str, iso, Agent, Event};
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{one_line, ApiError, Supervisor};
use crate::trail::{self, Ran};

/// The tools whose calls change a file.
const EDIT_TOOLS: [&str; 4] = ["Edit", "MultiEdit", "Write", "NotebookEdit"];
/// The most commits a trace shows.
const MAX_COMMITS: usize = 12;
/// The most commits a needle's `git log` reads.
const LOG_PER_NEEDLE: &str = "40";
/// Rewind's window around a moment.
const REWIND_BEFORE_MS: i64 = 20_000;
const REWIND_AFTER_MS: i64 = 3_000;
/// Diff lines kept per card.
const MAX_LINES: usize = 6;

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/trace", get(trace_route))
        .route("/api/trace/examples", get(examples_route))
        .route("/api/trace/explain", post(explain_route))
        .route("/api/trace/commit", get(commit_route))
}

/// What `GET /api/trace` takes. `x`, `y`, `topic`, `subject` (comma
/// separated), `in` (comma separated) and `kinds` override the reading
/// when given, even empty.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct TraceQ {
    pub q: Option<String>,
    pub repo: Option<String>,
    pub agent: Option<String>,
    pub x: Option<String>,
    pub y: Option<String>,
    pub topic: Option<String>,
    pub subject: Option<String>,
    #[serde(rename = "in")]
    pub in_: Option<String>,
    pub kinds: Option<String>,
}

async fn trace_route(State(app): State<App>, Query(q): Query<TraceQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(Json(blocking(move || trace(&sup, &q)).await?).into_response())
}

#[derive(Deserialize)]
struct ExamplesQ {
    repo: Option<String>,
}

async fn examples_route(Query(q): Query<ExamplesQ>) -> ApiResult {
    let v = blocking(move || Ok(examples(&trail::repo_dir(q.repo.as_deref())?))).await?;
    Ok(Json(v).into_response())
}

/// `GET /api/trace/examples`: questions to try, made of this repository's
/// history (Sam: the examples spoke of a cart and money.rs in a repo with
/// neither): the files changed most often, and a recent commit's subject.
pub fn examples(repo: &Path) -> Json_ {
    let log = git(repo, &["log", "-n", "300", "--no-merges", "--format=%x1e%s", "--name-only"]).unwrap_or_default();
    let mut count: HashMap<String, usize> = HashMap::new();
    let mut subjects: Vec<String> = Vec::new();
    for chunk in log.split('\u{1e}').skip(1) {
        let mut it = chunk.lines();
        let subj = it.next().unwrap_or("").trim().to_string();
        if !subj.is_empty() && subjects.len() < 40 {
            subjects.push(subj);
        }
        for f in it.map(str::trim).filter(|f| !f.is_empty()) {
            let name = f.rsplit('/').next().unwrap_or(f);
            let generated = name.ends_with(".lock") || name.ends_with("-lock.json") || name.ends_with(".min.js") || name == "go.sum" || name.ends_with(".md") || name.starts_with('.');
            if !generated && !crate::search::is_test_path(f) {
                *count.entry(f.to_string()).or_default() += 1;
            }
        }
    }
    let mut files: Vec<(String, usize)> = count.into_iter().collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut out: Vec<String> = Vec::new();
    if let Some((f, _)) = files.first() {
        out.push(format!("who changed {f} and why?"));
    }
    let plain = |s: &&String| {
        let l = s.to_lowercase();
        s.split_whitespace().count() >= 3 && !l.starts_with("merge") && !l.starts_with("initial") && !l.starts_with("wip") && !l.starts_with("wb:") && !l.starts_with("revert")
    };
    if let Some(s) = subjects.iter().find(plain) {
        let s = s.trim_end_matches('.');
        let mut c = s.chars();
        let lower = c.next().map(|f| f.to_lowercase().collect::<String>() + c.as_str()).unwrap_or_default();
        out.push(format!("when and why did we {}?", one_line(&lower, 90)));
    }
    if let Some((f, _)) = files.get(1) {
        out.push(format!("when did {f} last change, and how does it work now?"));
    }
    json!({ "examples": out, "files": files.iter().take(5).map(|(f, n)| json!({ "path": f, "commits": n })).collect::<Vec<_>>() })
}

async fn explain_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let q: TraceQ = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(Json(blocking(move || explain(&sup, &q)).await?).into_response())
}

#[derive(Deserialize)]
struct CommitQ {
    repo: Option<String>,
    agent: Option<String>,
    sha: Option<String>,
}

async fn commit_route(State(app): State<App>, Query(q): Query<CommitQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(Json(blocking(move || commit(&sup, q.repo.as_deref(), q.agent.as_deref(), q.sha.as_deref())).await?).into_response())
}

// ------------------------------------------------------------------ reading

const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "of", "to", "in", "on", "at", "for", "with", "from", "by", "into", "onto", "as", "so", "if", "than", "then", "there", "here", "it", "its", "it's",
    "is", "was", "were", "be", "been", "are", "am", "do", "did", "does", "done", "doing", "we", "i", "you", "they", "he", "she", "our", "my", "your", "their", "me", "us", "them", "this", "that",
    "these", "those", "why", "when", "how", "who", "whom", "whose", "what", "which", "where", "change", "changed", "changes", "changing", "switch", "switched", "move", "moved", "make", "made",
    "go", "went", "gone", "set", "become", "became", "work", "works", "working", "worked", "now", "about", "use", "used", "uses", "using", "get", "got", "have", "has", "had", "can", "could",
    "should", "would", "will", "start", "started", "stop", "stopped", "begin", "began", "add", "added", "remove", "removed", "decide", "decided", "happen", "happened", "current", "currently",
    "today", "yesterday", "ever", "time", "reason", "all", "any", "some", "not", "no", "yes", "up", "down", "out", "over", "turn", "turned", "bump", "bumped", "replace", "replaced", "rename",
    "renamed", "since", "still", "again", "way", "thing", "things", "code", "please", "tell", "show", "explain", "know", "one", "new", "old", "who's", "what's", "didn't", "don't", "doesn't",
];
const CHANGE_VERBS: &[&str] = &[
    "change", "changed", "changes", "changing", "switch", "switched", "move", "moved", "go", "went", "gone", "become", "became", "set", "bump", "bumped", "turn", "turned", "replace", "replaced",
    "rename", "renamed", "raise", "raised", "lower", "lowered", "update", "updated", "flip", "flipped", "convert", "converted", "migrate", "migrated",
];
const FILE_EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "md", "toml", "json", "yaml", "yml", "sql", "css", "scss", "html", "java", "kt", "rb", "c", "h", "cc", "cpp", "hpp", "swift", "sh",
    "lock", "txt", "proto", "vue", "svelte", "php", "cs", "ex", "exs", "lua", "zig", "env", "ini", "cfg", "conf",
];

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Reading {
    pub kinds: Vec<String>,
    pub x: Option<String>,
    pub y: Option<String>,
    /// The words the change is about, when x and y are values ("the cart
    /// total rounding" of "change the cart total rounding from floor to …").
    pub topic: Option<String>,
    /// Quoted text, `code` and identifiers.
    pub subjects: Vec<String>,
    /// File names.
    pub files: Vec<String>,
    /// "in api/": where to look.
    pub paths: Vec<String>,
    /// The other words that matter, for ChloeCtx and Decide.
    pub words: Vec<String>,
    /// x and y came from "between x and y": the change may go either way.
    pub between: bool,
}

#[derive(Debug, Clone)]
struct Tok {
    text: String,
    lower: String,
    lit: bool,
}

fn is_stop(w: &str) -> bool {
    STOP.contains(&w)
}

/// Quoted text out of a question: `"…"`, `“…”`, `` `…` ``, and `'…'` or
/// `‘…’` opened after a space. Answers the literals and the question with
/// each one as a placeholder word `\u{1}<n>`.
fn quoted(q: &str) -> (Vec<String>, String) {
    let cs: Vec<char> = q.chars().collect();
    let mut lits = Vec::new();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        let close = match c {
            '"' => Some('"'),
            '“' => Some('”'),
            '`' => Some('`'),
            '\'' | '‘' if i == 0 || cs[i - 1].is_whitespace() || cs[i - 1] == '(' => Some(if c == '\'' { '\'' } else { '’' }),
            _ => None,
        };
        if let Some(close) = close {
            let end = (i + 1..cs.len()).find(|&j| cs[j] == close && (close != '\'' || j + 1 == cs.len() || !cs[j + 1].is_alphanumeric()));
            if let Some(j) = end {
                let lit: String = cs[i + 1..j].iter().collect();
                if !lit.trim().is_empty() {
                    out.push_str(&format!(" \u{1}{} ", lits.len()));
                    lits.push(lit.trim().to_string());
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    (lits, out)
}

fn tokens(q: &str) -> Vec<Tok> {
    let (lits, text) = quoted(q);
    let mut out = Vec::new();
    for raw in text.split_whitespace() {
        // Arrows inside a word: floor→half-up, 3->5.
        let mut pieces = vec![raw.to_string()];
        for arrow in ["→", "->", "=>"] {
            pieces = pieces
                .into_iter()
                .flat_map(|p| {
                    if p == arrow || !p.contains(arrow) {
                        return vec![p];
                    }
                    let mut v = Vec::new();
                    for (k, part) in p.split(arrow).enumerate() {
                        if k > 0 {
                            v.push(arrow.to_string());
                        }
                        if !part.is_empty() {
                            v.push(part.to_string());
                        }
                    }
                    v
                })
                .collect();
        }
        for p in pieces {
            if let Some(n) = p.strip_prefix('\u{1}').and_then(|n| n.parse::<usize>().ok()) {
                let t = lits[n].clone();
                out.push(Tok { lower: t.to_lowercase(), text: t, lit: true });
                continue;
            }
            if is_arrow(&p) {
                out.push(Tok { lower: p.clone(), text: p, lit: false });
                continue;
            }
            let mut t = p.trim_start_matches(['(', '[', '{', '<']).trim_end_matches([',', '.', '?', '!', ';', ':', ')', ']', '}', '>']).to_string();
            for s in ["'s", "’s"] {
                if let Some(x) = t.strip_suffix(s) {
                    t = x.to_string();
                }
            }
            if t.is_empty() {
                continue;
            }
            out.push(Tok { lower: t.to_lowercase(), text: t, lit: false });
        }
    }
    out
}

fn is_arrow(t: &str) -> bool {
    matches!(t, "->" | "→" | "=>")
}

fn is_file(t: &str) -> bool {
    if t.starts_with("http://") || t.starts_with("https://") {
        return false;
    }
    let last = t.rsplit('/').next().unwrap_or(t);
    match last.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && FILE_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

fn is_ident(t: &str) -> bool {
    let t = t.trim_end_matches("()");
    if t.len() < 2 || !t.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') {
        return false;
    }
    if !t.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '.' | '$')) {
        return false;
    }
    let cs: Vec<char> = t.chars().collect();
    let camel = cs.windows(2).any(|w| w[0].is_lowercase() && w[1].is_uppercase());
    let constant = t.chars().count() >= 4 && t.chars().any(|c| c.is_ascii_uppercase()) && t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    (t.contains('_') && t.chars().any(char::is_alphanumeric)) || t.contains("::") || camel || constant || (t.contains('.') && !t.ends_with('.') && !is_file(t))
}

/// A value of x or y: a literal, or a word that is not a stop word.
fn value_tok(t: &Tok) -> bool {
    t.lit || (!is_stop(&t.lower) && !is_arrow(&t.lower) && t.lower != "from")
}

/// Reads a question by rules: no model.
pub fn read(q: &str, main: Option<&Path>) -> Reading {
    let toks = tokens(q);
    let mut r = Reading::default();
    let mut used = vec![false; toks.len()];
    let has = |w: &str| toks.iter().any(|t| !t.lit && t.lower == w);
    for (k, words) in [("when", &["when", "since", "whenever"][..]), ("why", &["why", "reason"][..]), ("how", &["how"][..]), ("who", &["who", "whom", "whose", "who's"][..])] {
        if words.iter().any(|w| has(w)) {
            r.kinds.push(k.to_string());
        }
    }
    if r.kinds.is_empty() {
        r.kinds = vec!["when".into(), "why".into()];
    }

    // "from x to y", "change x to y", "x → y".
    for i in 1..toks.len().saturating_sub(1) {
        let t = &toks[i];
        if t.lit {
            continue;
        }
        let arrow = is_arrow(&t.lower);
        if !(arrow || t.lower == "to" || t.lower == "into") {
            continue;
        }
        let from = (i.saturating_sub(8)..i).rev().find(|&j| !toks[j].lit && toks[j].lower == "from");
        let verb = (0..i).rev().find(|&j| !toks[j].lit && CHANGE_VERBS.contains(&toks[j].lower.as_str()));
        if !arrow && from.is_none() && verb.is_none() {
            continue;
        }
        let y = &toks[i + 1];
        if !value_tok(y) {
            continue;
        }
        let (x, topic_from, topic_to) = match from {
            Some(f) if f + 1 < i => {
                let x: Vec<&Tok> = toks[f + 1..i].iter().collect();
                let x = if x.len() == 1 { x[0].text.clone() } else { x.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ") };
                for u in &mut used[f..=i + 1] {
                    *u = true;
                }
                (Some(x), verb.map_or(0, |v| v + 1), f)
            }
            _ if arrow => {
                for u in &mut used[i - 1..=i + 1] {
                    *u = true;
                }
                (Some(toks[i - 1].text.clone()).filter(|_| value_tok(&toks[i - 1])), 0, 0)
            }
            _ => {
                // "change <phrase> to y": one value is x, more words are the topic.
                let v = verb.unwrap_or(0);
                let phrase: Vec<usize> = (v + 1..i).filter(|&j| toks[j].lit || !is_stop(&toks[j].lower)).collect();
                for u in &mut used[v..=i + 1] {
                    *u = true;
                }
                // "change <the thing> to y": an identifier or a quote is a
                // subject, other words the topic; the old value is not said.
                if phrase.len() == 1 && (toks[phrase[0]].lit || is_ident(&toks[phrase[0]].text)) {
                    r.subjects.push(toks[phrase[0]].text.trim_end_matches("()").to_string());
                    (None, 0, 0)
                } else {
                    (None, v + 1, i)
                }
            }
        };
        if topic_to > topic_from {
            let words: Vec<&str> = toks[topic_from..topic_to].iter().filter(|t| t.lit || !is_stop(&t.lower)).map(|t| t.text.as_str()).collect();
            if !words.is_empty() {
                r.topic = Some(words.join(" "));
            }
        }
        r.x = x;
        r.y = Some(y.text.clone());
        break;
    }

    // "between x and y" (a switch either way): x and y, the topic before.
    if r.y.is_none() {
        if let Some(b) = toks.iter().position(|t| !t.lit && t.lower == "between") {
            if let Some(and) = (b + 2..toks.len().min(b + 10)).find(|&j| !toks[j].lit && toks[j].lower == "and") {
                let end = (and + 1..toks.len()).find(|&j| !toks[j].lit && matches!(toks[j].lower.as_str(), "and" | "or" | "in" | "when" | "why" | "how" | "?")).unwrap_or(toks.len());
                let side = |a: usize, z: usize| -> Option<String> {
                    let w: Vec<&str> = toks[a..z].iter().filter(|t| t.lit || !matches!(t.lower.as_str(), "the" | "a" | "an")).map(|t| t.text.trim_end_matches(['?', ',', '.'])).filter(|w| !w.is_empty()).collect();
                    (!w.is_empty()).then(|| w.join(" "))
                };
                if let (Some(x), Some(y)) = (side(b + 1, and), side(and + 1, end.min(and + 4))) {
                    let verb = (0..b).rev().find(|&j| !toks[j].lit && CHANGE_VERBS.contains(&toks[j].lower.as_str()));
                    if let Some(v) = verb {
                        let words: Vec<&str> = toks[0..v].iter().filter(|t| !t.lit && !is_stop(&t.lower)).map(|t| t.text.as_str()).collect();
                        if !words.is_empty() && toks[..v].iter().all(|t| !is_ident(&t.text) || is_stop(&t.lower)) {
                            r.topic = Some(words.join(" "));
                        }
                    }
                    for u in &mut used[b..end.min(and + 4)] {
                        *u = true;
                    }
                    r.x = Some(x);
                    r.y = Some(y);
                    r.between = true;
                }
            }
        }
    }

    // "in api/", "under web/src".
    for i in 0..toks.len().saturating_sub(1) {
        if used[i] || toks[i].lit || !matches!(toks[i].lower.as_str(), "in" | "under" | "inside" | "within") {
            continue;
        }
        let p = &toks[i + 1];
        let dir = main.is_some_and(|m| !p.text.contains("..") && m.join(&p.text).is_dir());
        if p.text.ends_with('/') || (p.text.contains('/') && !is_file(&p.text)) || dir {
            r.paths.push(p.text.clone());
            used[i] = true;
            used[i + 1] = true;
        }
    }

    // The rest: literals, files, identifiers, words.
    for (i, t) in toks.iter().enumerate() {
        if used[i] {
            continue;
        }
        if t.lit {
            if is_file(&t.text) {
                r.files.push(t.text.clone());
            } else {
                r.subjects.push(t.text.clone());
            }
        } else if is_file(&t.text) {
            r.files.push(t.text.clone());
        } else if is_ident(&t.text) {
            r.subjects.push(t.text.trim_end_matches("()").to_string());
        } else {
            let w: String = t.lower.chars().filter(|c| c.is_alphanumeric() || *c == '-').collect();
            if w.chars().count() >= 3 && !is_stop(&w) && !r.words.contains(&w) {
                r.words.push(w);
            }
        }
    }
    if let Some(topic) = &r.topic {
        for w in topic.split_whitespace() {
            let w = w.to_lowercase();
            if w.chars().count() >= 3 && !r.words.contains(&w) {
                r.words.push(w);
            }
        }
    }
    r.subjects.dedup();
    r
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// The reading with the user's fixes.
fn apply(mut r: Reading, q: &TraceQ) -> Reading {
    let opt = |v: &str| Some(v.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(x) = &q.x {
        r.x = opt(x);
    }
    if let Some(y) = &q.y {
        r.y = opt(y);
    }
    if let Some(t) = &q.topic {
        r.topic = opt(t);
    }
    if let Some(s) = &q.subject {
        let all = split_list(s);
        r.files = all.iter().filter(|s| is_file(s)).cloned().collect();
        r.subjects = all.into_iter().filter(|s| !is_file(s)).collect();
    }
    if let Some(p) = &q.in_ {
        r.paths = split_list(p);
    }
    if let Some(k) = &q.kinds {
        let ks: Vec<String> = split_list(k).into_iter().filter(|k| matches!(k.as_str(), "when" | "why" | "how" | "who")).collect();
        if !ks.is_empty() {
            r.kinds = ks;
        }
    }
    if let Some(t) = &r.topic {
        for w in t.split_whitespace() {
            let w = w.to_lowercase();
            if w.chars().count() >= 3 && !is_stop(&w) && !r.words.contains(&w) {
                r.words.push(w);
            }
        }
    }
    r
}

fn reading_json(r: &Reading, needles: &[Needle]) -> Json_ {
    let mut chips = Vec::new();
    if let Some(x) = &r.x {
        chips.push(json!({ "key": "x", "label": "x", "text": x }));
    }
    if let Some(y) = &r.y {
        chips.push(json!({ "key": "y", "label": "y", "text": y }));
    }
    for s in r.subjects.iter().chain(&r.files) {
        chips.push(json!({ "key": "subject", "label": "subject", "text": s }));
    }
    if let Some(t) = &r.topic {
        chips.push(json!({ "key": "topic", "label": "about", "text": t }));
    }
    for p in &r.paths {
        chips.push(json!({ "key": "in", "label": "in", "text": p }));
    }
    json!({
        "kinds": r.kinds, "x": r.x, "y": r.y, "topic": r.topic,
        "subjects": r.subjects.iter().chain(&r.files).collect::<Vec<_>>(), "files": r.files, "paths": r.paths, "words": r.words,
        "needles": needles.iter().map(|n| n.label()).collect::<Vec<_>>(),
        "chips": chips,
    })
}

// ------------------------------------------------------------------ needles

/// What is searched for: a text matched loosely (case, and `-`, `_` or a
/// space between its parts: `half-up` finds `half_up`, `halfUp`, `HALF UP`),
/// after a word (`retries … 5`) when the value alone is too short to mean
/// anything.
#[derive(Debug, Clone, PartialEq)]
pub struct Needle {
    pub text: String,
    pub pre: Option<String>,
    /// x, y or s (a subject).
    pub role: char,
}

fn parts(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if matches!(c, '-' | '_') || c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            if c.is_uppercase() && prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.push(c);
        }
        prev = Some(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A text with case and the separators `-`, `_` and spaces taken out.
fn norm(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '-' | '_') && !c.is_whitespace()).flat_map(char::to_lowercase).collect()
}

/// A POSIX extended regular expression for git (`-G`, `grep -E`): each
/// letter in both cases, the parts joined by an optional `-`, `_` or space.
fn ere(s: &str) -> String {
    let esc = |p: &str| {
        let mut o = String::new();
        for c in p.chars() {
            if c.is_ascii_alphabetic() {
                o.push('[');
                o.push(c.to_ascii_lowercase());
                o.push(c.to_ascii_uppercase());
                o.push(']');
            } else if "\\.[]{}()*+?^$|".contains(c) {
                o.push('\\');
                o.push(c);
            } else {
                o.push(c);
            }
        }
        o
    };
    parts(s).iter().map(|p| esc(p)).collect::<Vec<_>>().join("[-_ ]?")
}

impl Needle {
    fn new(text: &str, role: char) -> Needle {
        Needle { text: text.to_string(), pre: None, role }
    }

    fn label(&self) -> String {
        match &self.pre {
            Some(p) => format!("{p} … {}", self.text),
            None => self.text.clone(),
        }
    }

    fn ere(&self) -> String {
        match &self.pre {
            Some(p) => format!("{}.{{0,40}}{}", ere(p), ere(&self.text)),
            None => ere(&self.text),
        }
    }

    pub fn finds(&self, line: &str) -> bool {
        let n = norm(&self.text);
        if n.is_empty() {
            return false;
        }
        let l = norm(line);
        match &self.pre {
            None => l.contains(&n),
            Some(p) => {
                let p = norm(p);
                let mut from = 0;
                while let Some(i) = l[from..].find(&p) {
                    let after = from + i + p.len();
                    let win: String = l[after..].chars().take(40).collect();
                    if win.contains(&n) {
                        return true;
                    }
                    from = after;
                }
                false
            }
        }
    }
}

/// Too short to search alone: a number or one or two letters.
fn weak(v: &str) -> bool {
    let n = norm(v);
    n.chars().count() < 3 || n.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// What to search git and the code for.
pub fn needles(r: &Reading) -> Vec<Needle> {
    let pre = r.topic.as_ref().and_then(|t| t.split_whitespace().last()).map(str::to_string).or_else(|| r.subjects.first().cloned());
    let mut out = Vec::new();
    for (v, role) in [(&r.x, 'x'), (&r.y, 'y')] {
        if let Some(v) = v {
            let mut n = Needle::new(v, role);
            if weak(v) {
                match &pre {
                    Some(p) => n.pre = Some(p.clone()),
                    None if norm(v).is_empty() => continue,
                    None => {}
                }
            }
            out.push(n);
        }
    }
    for s in &r.subjects {
        if !weak(s) && !out.iter().any(|n: &Needle| norm(&n.text) == norm(s)) {
            out.push(Needle::new(s, 's'));
        }
    }
    if out.is_empty() && r.files.is_empty() {
        match &r.topic {
            Some(t) if t.split_whitespace().count() <= 3 => out.push(Needle::new(t, 's')),
            _ => {
                let mut ws: Vec<&String> = r.words.iter().filter(|w| w.chars().count() >= 5).collect();
                ws.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));
                for w in ws.into_iter().take(2) {
                    out.push(Needle::new(w, 's'));
                }
            }
        }
    }
    out.truncate(4);
    out
}

/// How much a text is about the question: 3 for each needle it holds, 1
/// for each other word.
fn relevance(text: &str, needles: &[Needle], words: &[String]) -> usize {
    let lower = text.to_lowercase();
    let toks: HashSet<String> = lower.split(|c: char| !c.is_alphanumeric() && c != '-').map(str::to_string).collect();
    let n = needles.iter().filter(|n| n.finds(text)).count() * 3;
    let w = words.iter().filter(|w| toks.contains(w.as_str()) || (w.len() >= 5 && lower.contains(w.as_str()))).count();
    n + w
}

// ---------------------------------------------------------------------- git

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[derive(Debug, Clone)]
struct Commit {
    sha: String,
    at_ms: i64,
    author: String,
    subject: String,
    change: &'static str,
    /// (path, sign, line number, text).
    lines: Vec<(String, char, Option<i64>, String)>,
    files: Vec<String>,
    /// The message's body (after its subject line).
    body: String,
    /// Every line it added and removed in the traced files, normalized
    /// (whitespace collapsed), for "a later commit undid this one".
    added: Vec<String>,
    removed: Vec<String>,
}

fn pathspecs(r: &Reading) -> Vec<String> {
    r.paths.iter().chain(&r.files).filter(|p| !p.contains("..") && !p.starts_with('/') && !p.starts_with('-')).cloned().collect()
}

/// The commits where the needles changed (or that touched the files when
/// there are no needles), oldest first.
fn commits(main: &Path, r: &Reading, ns: &[Needle], missing: &mut Vec<String>) -> Vec<Commit> {
    let specs = pathspecs(r);
    let fmt = "--format=%H%x1f%at%x1f%an%x1f%s";
    let mut metas: HashMap<String, (i64, String, String)> = HashMap::new();
    let mut runs: Vec<Vec<String>> = Vec::new();
    for n in ns {
        let mut a: Vec<String> = vec!["log".into(), "--all".into(), "-n".into(), LOG_PER_NEEDLE.into(), fmt.into(), "-G".into(), n.ere()];
        a.push("--".into());
        a.extend(specs.iter().cloned());
        runs.push(a);
    }
    if ns.is_empty() && !r.files.is_empty() {
        let mut a: Vec<String> = vec!["log".into(), "--all".into(), "-n".into(), "15".into(), fmt.into(), "--".into()];
        a.extend(specs.iter().cloned());
        runs.push(a);
    }
    for a in runs {
        let args: Vec<&str> = a.iter().map(String::as_str).collect();
        match git(main, &args) {
            Ok(out) => {
                for l in out.lines() {
                    let f: Vec<&str> = l.splitn(4, '\u{1f}').collect();
                    if f.len() == 4 {
                        metas.entry(f[0].to_string()).or_insert((f[1].parse::<i64>().unwrap_or(0) * 1000, f[2].to_string(), f[3].to_string()));
                    }
                }
            }
            Err(e) => missing.push(format!("git log failed: {}", one_line(&e, 160))),
        }
    }
    let mut list: Vec<(String, (i64, String, String))> = metas.into_iter().collect();
    list.sort_by_key(|(_, m)| std::cmp::Reverse(m.0));
    list.truncate(24);
    // Commits made in the same second: the one that descends from more of
    // the others is the later (git's own order is lost in the map).
    let depth: HashMap<String, usize> = list
        .iter()
        .map(|(sha, m)| {
            let tied = list.iter().filter(|(o, n)| n.0 == m.0 && o != sha);
            (sha.clone(), tied.filter(|(o, _)| git(main, &["merge-base", "--is-ancestor", o, sha]).is_ok()).count())
        })
        .collect();
    list.sort_by_key(|(sha, m)| (std::cmp::Reverse(m.0), std::cmp::Reverse(depth.get(sha).copied().unwrap_or(0))));
    let (x, y) = (ns.iter().find(|n| n.role == 'x'), ns.iter().find(|n| n.role == 'y'));
    let mut out = Vec::new();
    for (sha, (at_ms, author, subject)) in list {
        let mut a: Vec<String> = vec!["show".into(), "--format=%b%x1e".into(), "--no-color".into(), "--no-ext-diff".into(), "-U0".into(), sha.clone(), "--".into()];
        a.extend(specs.iter().cloned());
        let args: Vec<&str> = a.iter().map(String::as_str).collect();
        let Ok(shown) = git(main, &args) else { continue };
        let (body, text) = shown.split_once('\u{1e}').unwrap_or(("", shown.as_str()));
        let body = body.trim().to_string();
        let files = crate::git::parse_diff(text);
        let (mut added, mut removed): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        let mut lines = Vec::new();
        let (mut minus_x, mut plus_y, mut plus_x, mut minus_y, mut any) = (false, false, false, false, false);
        let mut touched = Vec::new();
        for f in &files {
            let path = f["path"].as_str().unwrap_or("").to_string();
            touched.push(path.clone());
            for hk in f["hunks"].as_array().into_iter().flatten() {
                for l in hk["lines"].as_array().into_iter().flatten() {
                    let sign = l["kind"].as_str().and_then(|k| k.chars().next()).unwrap_or(' ');
                    if sign == ' ' {
                        continue;
                    }
                    let t = l["text"].as_str().unwrap_or("");
                    let norm_line = t.split_whitespace().collect::<Vec<_>>().join(" ");
                    if norm_line.chars().filter(|c| c.is_alphanumeric()).count() >= 3 && added.len() + removed.len() < 600 {
                        if sign == '+' { added.push(norm_line) } else { removed.push(norm_line) }
                    }
                    let hit = ns.iter().filter(|n| n.finds(t)).collect::<Vec<_>>();
                    let no = if sign == '+' { l["new_line"].as_i64() } else { l["old_line"].as_i64() };
                    if ns.is_empty() {
                        if lines.len() < 4 {
                            lines.push((path.clone(), sign, no, t.to_string()));
                        }
                        any = true;
                        continue;
                    }
                    if hit.is_empty() {
                        continue;
                    }
                    any = true;
                    minus_x |= sign == '-' && x.is_some_and(|x| x.finds(t));
                    plus_x |= sign == '+' && x.is_some_and(|x| x.finds(t));
                    plus_y |= sign == '+' && y.is_some_and(|y| y.finds(t));
                    minus_y |= sign == '-' && y.is_some_and(|y| y.finds(t));
                    lines.push((path.clone(), sign, no, t.to_string()));
                }
            }
        }
        if !any {
            continue;
        }
        let change = if (minus_x && plus_y) || (r.between && minus_y && plus_x) {
            "changed"
        } else if plus_y && !minus_y {
            "added"
        } else if plus_x && !minus_x && y.is_none() {
            "added"
        } else if plus_x && !minus_x {
            "added_x"
        } else if minus_x && !plus_x {
            "removed"
        } else {
            "touched"
        };
        // The lines that say most first: x going out, y coming in.
        lines.sort_by_key(|(_, s, _, t)| {
            let rank = if *s == '-' && x.is_some_and(|x| x.finds(t)) {
                0
            } else if *s == '+' && y.is_some_and(|y| y.finds(t)) {
                1
            } else {
                2
            };
            (rank, *s != '-')
        });
        lines.truncate(MAX_LINES);
        lines.sort_by(|a, b| (a.0.as_str(), a.2.unwrap_or(0), a.1 != '-').cmp(&(b.0.as_str(), b.2.unwrap_or(0), b.1 != '-')));
        out.push(Commit { sha, at_ms, author, subject, change, lines, files: touched, body, added, removed });
    }
    // With x and y: the commits that only brought x in count where y came
    // later to the same file (where it started), the others are noise.
    if x.is_some() && y.is_some() {
        let later: HashSet<String> = out.iter().filter(|c| matches!(c.change, "changed" | "added")).flat_map(|c| c.lines.iter().map(|l| l.0.clone())).collect();
        out.retain(|c| c.change != "added_x" || c.lines.iter().any(|l| later.contains(&l.0)));
        out.retain(|c| c.change != "touched" || c.lines.iter().any(|l| y.unwrap().finds(&l.3)));
    }
    // Oldest first; within a second, in git's order (it listed newest first).
    out.reverse();
    out.sort_by_key(|c| c.at_ms);
    if out.len() > MAX_COMMITS {
        out.drain(..out.len() - MAX_COMMITS);
    }
    out
}

fn branches_of(main: &Path, sha: &str) -> Vec<String> {
    let out = git(main, &["branch", "--all", "--contains", sha, "--format=%(refname:short)"]).unwrap_or_default();
    let mut v: Vec<String> = out.lines().map(str::trim).filter(|l| !l.is_empty() && !l.contains("HEAD")).map(str::to_string).collect();
    v.sort_by_key(|b| (!matches!(b.as_str(), "main" | "master"), b.clone()));
    v.truncate(4);
    v
}

/// The agent that made a commit: the one whose branch brought it (in its
/// branch, not in its base), else the one whose session ran `git commit`
/// within two minutes of it.
fn agent_of_commit(main: &Path, agents: &[Agent], evs: &HashMap<String, Vec<Event>>, sha: &str, at_ms: i64) -> Option<Agent> {
    let mut by_branch: Vec<&Agent> = agents
        .iter()
        .filter(|a| match (a.branch.as_deref(), a.base.as_deref()) {
            (Some(b), Some(base)) => crate::git::is_ancestor(main, sha, b) && !crate::git::is_ancestor(main, sha, base),
            _ => false,
        })
        .collect();
    by_branch.sort_by_key(|a| {
        let inside = a.created - 60_000 <= at_ms && a.ended.is_none_or(|e| at_ms <= e + 60_000);
        (!inside, a.created)
    });
    if let Some(a) = by_branch.first() {
        return Some((*a).clone());
    }
    agents
        .iter()
        .find(|a| {
            evs.get(&a.id).is_some_and(|list| {
                list.iter().any(|e| {
                    e.tool.as_deref() == Some("Bash") && (e.ts - at_ms).abs() <= 120_000 && (e.summary.as_deref().unwrap_or("").contains("git commit") || e.data.as_deref().is_some_and(|d| d.contains("git commit")))
                })
            })
        })
        .cloned()
}

/// The line of `rel` in `dir` whose text is `text` (trimmed), from 1.
fn locate(dir: &Path, rel: &str, text: &str) -> Option<usize> {
    let want = text.trim();
    if want.chars().count() < 2 || rel.contains("..") {
        return None;
    }
    let p = dir.join(rel);
    let meta = std::fs::metadata(&p).ok()?;
    if !meta.is_file() || meta.len() > 8 << 20 {
        return None;
    }
    let body = String::from_utf8_lossy(&std::fs::read(&p).ok()?).into_owned();
    body.lines().position(|l| l.trim() == want).map(|i| i + 1)
}

// -------------------------------------------------------------------- edits

#[derive(Debug, Clone)]
struct Edit {
    agent: Agent,
    seq: i64,
    ts: i64,
    tool: String,
    rel: String,
    change: &'static str,
    lines: Vec<(String, char, Option<i64>, String)>,
}

fn rel_of(path: &str, cwd: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        if let Ok(r) = p.strip_prefix(cwd) {
            return r.to_string_lossy().to_string();
        }
        return path.to_string();
    }
    path.trim_start_matches("./").to_string()
}

fn in_scope(rel: &str, specs: &[String]) -> bool {
    specs.is_empty() || specs.iter().any(|s| rel == s || rel.starts_with(&format!("{}/", s.trim_end_matches('/'))) || rel.ends_with(&format!("/{s}")))
}

/// The edits the agents' hooks reported that hold a needle.
fn edits(agents: &[Agent], evs: &HashMap<String, Vec<Event>>, r: &Reading, ns: &[Needle]) -> Vec<Edit> {
    let specs = pathspecs(r);
    let (x, y) = (ns.iter().find(|n| n.role == 'x'), ns.iter().find(|n| n.role == 'y'));
    let mut out = Vec::new();
    for a in agents {
        for e in evs.get(&a.id).into_iter().flatten() {
            if e.kind != "PostToolUse" || !e.tool.as_deref().is_some_and(|t| EDIT_TOOLS.contains(&t)) {
                continue;
            }
            let Some(d) = trail::data_of(e) else { continue };
            let ti = &d["tool_input"];
            let Some(file) = ["file_path", "notebook_path", "path"].iter().find_map(|k| ti.get(*k).and_then(Json_::as_str)) else { continue };
            let rel = rel_of(file, &a.cwd);
            // A file outside the agent's checkout (its memory notes, a
            // scratch file in /tmp) is not a change to the repository.
            if Path::new(&rel).is_absolute() || rel.starts_with("../") || !in_scope(&rel, &specs) {
                continue;
            }
            let mut old: Vec<&str> = ["old_string"].iter().filter_map(|k| ti.get(*k).and_then(Json_::as_str)).collect();
            let mut new: Vec<&str> = ["new_string", "content", "new_source"].iter().filter_map(|k| ti.get(*k).and_then(Json_::as_str)).collect();
            for ed in ti.get("edits").and_then(Json_::as_array).into_iter().flatten() {
                old.extend(ed.get("old_string").and_then(Json_::as_str));
                new.extend(ed.get("new_string").and_then(Json_::as_str));
            }
            let old_l: Vec<&str> = old.iter().flat_map(|t| t.lines()).collect();
            let new_l: Vec<&str> = new.iter().flat_map(|t| t.lines()).collect();
            let m_old: Vec<&str> = old_l.iter().copied().filter(|l| ns.iter().any(|n| n.finds(l))).collect();
            let m_new: Vec<&str> = new_l.iter().copied().filter(|l| ns.iter().any(|n| n.finds(l))).collect();
            if m_old.is_empty() && m_new.is_empty() {
                continue;
            }
            let old_x = x.is_some_and(|x| m_old.iter().any(|l| x.finds(l)));
            let new_y = y.is_some_and(|y| m_new.iter().any(|l| y.finds(l)));
            let change = if (old_x && new_y) || (r.between && y.is_some_and(|y| m_old.iter().any(|l| y.finds(l))) && x.is_some_and(|x| m_new.iter().any(|l| x.finds(l)))) {
                "changed"
            } else if !m_new.is_empty() {
                "added"
            } else {
                "removed"
            };
            // With x and y, an edit that holds neither change nor y is noise.
            if x.is_some() && y.is_some() && change != "changed" && !new_y {
                continue;
            }
            let dir = Path::new(&a.cwd);
            let mut lines: Vec<(String, char, Option<i64>, String)> = Vec::new();
            for l in m_old.iter().take(MAX_LINES / 2) {
                lines.push((rel.clone(), '-', None, l.to_string()));
            }
            for l in m_new.iter().take(MAX_LINES - lines.len()) {
                lines.push((rel.clone(), '+', locate(dir, &rel, l).map(|n| n as i64), l.to_string()));
            }
            out.push(Edit { agent: a.clone(), seq: e.seq, ts: e.ts, tool: e.tool.clone().unwrap_or_default(), rel, change, lines });
        }
    }
    out
}

// ------------------------------------------------------------------- moment

fn rewind(sup: &Supervisor, a: &Agent, at: i64) -> Json_ {
    let Some(st) = a.started_ms else { return Json_::Null };
    if !sup.session_dir(&a.id).join("term.cast").is_file() || at < st - 1000 {
        return Json_::Null;
    }
    let rel = (at - st).max(0);
    json!({ "agent": a.id, "from_ms": (rel - REWIND_BEFORE_MS).max(0), "to_ms": rel + REWIND_AFTER_MS, "at_ms": at })
}

fn brief(sup: &Supervisor, a: &Agent) -> Json_ {
    trail::agent_json(sup, a)
}

fn lines_json(lines: &[(String, char, Option<i64>, String)]) -> Json_ {
    Json_::Array(lines.iter().map(|(p, s, n, t)| json!({ "path": p, "sign": s.to_string(), "line": n, "text": t })).collect())
}

fn utc(ms: i64) -> String {
    let s = iso(ms);
    format!("{} UTC", s.get(..16).unwrap_or(&s).replace('T', " "))
}

// -------------------------------------------------------------------- trace

/// `GET /api/trace`.
pub fn trace(sup: &Supervisor, q: &TraceQ) -> Result<Json_, ApiError> {
    let started = Instant::now();
    let question = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| ApiError::bad("q is required: the question, in a sentence"))?.to_string();
    if question.len() > 2000 {
        return Err(ApiError::bad("q is longer than 2000 bytes"));
    }
    let repo = match (q.repo.as_deref().filter(|r| !r.is_empty()), q.agent.as_deref().filter(|a| !a.is_empty())) {
        (Some(r), _) => Some(r.to_string()),
        (None, Some(id)) => Some(sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?.repo),
        (None, None) => None,
    };
    let main = trail::repo_dir(repo.as_deref())?;
    let main_s = main.to_string_lossy().to_string();
    let reading = apply(read(&question, Some(&main)), q);
    let ns = needles(&reading);
    let mut missing: Vec<String> = Vec::new();
    if ns.is_empty() && reading.files.is_empty() {
        missing.push("The question names nothing to look for in the code: put the value, identifier or file in quotes, or fix the reading.".into());
    }

    let agents: Vec<Agent> = sup.agent_rows().into_iter().filter(|a| a.repo == main_s).collect();
    let evs: HashMap<String, Vec<Event>> = agents.iter().map(|a| (a.id.clone(), sup.all_events(&a.id).unwrap_or_default())).collect();
    let by_id: HashMap<String, Agent> = agents.iter().map(|a| (a.id.clone(), a.clone())).collect();

    // When: commits and edits.
    let cs = if git(&main, &["rev-parse", "--git-dir"]).is_ok() {
        commits(&main, &reading, &ns, &mut missing)
    } else {
        missing.push(format!("{main_s} is not a git checkout, so there is no history to search."));
        Vec::new()
    };
    let mut items: Vec<Json_> = Vec::new();
    let mut commit_agent: Vec<Option<Agent>> = Vec::new();
    for c in &cs {
        commit_agent.push(agent_of_commit(&main, &agents, &evs, &c.sha, c.at_ms));
    }
    let mut eds = edits(&agents, &evs, &reading, &ns);
    // An edit a commit holds is the moment that commit's line was written.
    let mut written: HashMap<usize, Edit> = HashMap::new();
    eds.sort_by_key(|e| e.ts);
    eds.retain(|e| {
        let plus: Vec<&str> = e.lines.iter().filter(|l| l.1 == '+').map(|l| l.3.trim()).collect();
        let held = cs.iter().enumerate().find(|(i, c)| {
            commit_agent[*i].as_ref().is_some_and(|a| a.id == e.agent.id) && c.at_ms + 1000 >= e.ts && c.lines.iter().any(|l| l.1 == '+' && plus.contains(&l.3.trim()))
        });
        match held {
            Some((i, _)) => {
                written.entry(i).or_insert_with(|| e.clone());
                false
            }
            None => true,
        }
    });
    let (x, y) = (reading.x.clone(), reading.y.clone());
    for (i, c) in cs.iter().enumerate() {
        let a = commit_agent[i].as_ref();
        let w = written.get(&i);
        let moment = w.map_or(c.at_ms, |w| w.ts);
        let short: String = c.sha.chars().take(7).collect();
        let first_file = c.lines.first().map(|l| l.0.clone()).or_else(|| c.files.first().cloned()).unwrap_or_default();
        let text = match (c.change, &x, &y) {
            ("changed", Some(x), Some(y)) => format!("{x} became {y} in {first_file}."),
            ("added", _, Some(y)) => format!("{y} came in, in {first_file}."),
            ("added", _, _) | ("added_x", _, _) => format!("{} came in, in {first_file}.", x.as_deref().or(ns.first().map(|n| n.text.as_str())).unwrap_or("it")),
            ("removed", Some(x), _) => format!("{x} went out of {first_file}."),
            _ => format!("Touched {}.", c.files.iter().take(3).cloned().collect::<Vec<_>>().join(", ")),
        };
        // Why? on the line as it is now: in the agent's worktree, else the main checkout.
        let plus = c.lines.iter().find(|l| l.1 == '+' && y.as_deref().is_none_or(|_| ns.iter().any(|n| n.role == 'y' && n.finds(&l.3)))).or_else(|| c.lines.iter().find(|l| l.1 == '+'));
        // The line as this commit has it, blamed at the commit (so a later
        // edit of the same line is not its author).
        let why = plus.and_then(|l| {
            let n = l.2.or_else(|| locate(&main, &l.0, &l.3).map(|n| n as i64))?;
            Some(match a.filter(|a| Path::new(&a.cwd).is_dir()) {
                Some(a) => json!({ "agent": a.id, "path": l.0, "line": n }),
                None => json!({ "repo": main_s, "path": l.0, "line": n }),
            })
        });
        items.push(json!({
            "id": format!("c-{short}"), "kind": "commit", "ts": iso(c.at_ms), "ts_ms": c.at_ms,
            "agent": a.map(|a| brief(sup, a)), "by": a.map_or(c.author.clone(), |a| a.name.clone()),
            "change": if c.change == "added_x" { "added" } else { c.change },
            "title": c.subject, "text": text,
            "commit": { "sha": c.sha, "short": short, "subject": c.subject, "author": c.author, "branches": branches_of(&main, &c.sha), "files": c.files },
            "written": w.map(|w| json!({ "ts": iso(w.ts), "ts_ms": w.ts, "tool": w.tool, "file": w.rel })),
            "lines": lines_json(&c.lines),
            "links": {
                "story": a.map(|a| json!({ "agent": a.id, "at_ms": moment })),
                "rewind": a.map_or(Json_::Null, |a| rewind(sup, a, moment)),
                "why": why, "diff": { "sha": c.sha }, "time_ms": moment,
            },
        }));
    }
    for e in &eds {
        let plus = e.lines.iter().find(|l| l.1 == '+' && l.2.is_some());
        items.push(json!({
            "id": format!("w-{}-{}", e.agent.id, e.seq), "kind": "edit", "ts": iso(e.ts), "ts_ms": e.ts,
            "agent": brief(sup, &e.agent), "by": e.agent.name, "change": e.change, "committed": false,
            "title": format!("{} {}", e.tool, e.rel),
            "text": if plus.is_some() { format!("No commit holds it yet: it is in {}'s worktree.", e.agent.name) } else { "No commit holds it, and the file no longer has those lines.".to_string() },
            "lines": lines_json(&e.lines),
            "links": {
                "story": { "agent": e.agent.id, "at_ms": e.ts }, "rewind": rewind(sup, &e.agent, e.ts),
                "why": plus.map(|l| json!({ "agent": e.agent.id, "path": l.0, "line": l.2 })), "diff": null, "time_ms": e.ts,
            },
        }));
    }
    let hands: HashSet<String> = commit_agent.iter().flatten().map(|a| a.id.clone()).chain(eds.iter().map(|e| e.agent.id.clone())).collect();

    // How it works now (before why: blame reads its first line).
    let now = now_code(sup, &main, &reading, &ns, &agents.iter().filter(|a| hands.contains(&a.id)).cloned().collect::<Vec<_>>(), &mut missing);

    // Why: ChloeCtx, Decide, the goal.
    let s = trail::Sessions::load(sup);
    ctx_items(sup, &main, &question, &reading, &ns, &hands, &s, &now, &mut items, &mut missing);
    decide_items(sup, &main_s, &reading, &ns, &hands, &by_id, &mut items);
    goal_items(sup, &hands, &by_id, &mut items);

    items.sort_by_key(|i| i["ts_ms"].as_i64().unwrap_or(0));
    for (n, it) in items.iter_mut().enumerate() {
        it["tag"] = json!(format!("E{}", n + 1));
    }

    // Two recorded reasons that pull apart (Priya: half up after a ticket
    // about totals a cent short, then half even for finance's ledger).
    let conflict = conflict_of(&cs);

    // The answer.
    let answer = answer_of(&reading, &ns, &items);
    let key = answer.as_ref().and_then(|a| a["key"].as_str().map(str::to_string));
    let mut links = json!({ "time_ms": null, "story": null, "rewind": null });
    if let Some(k) = &key {
        if let Some(it) = items.iter_mut().find(|i| i["id"] == k.as_str()) {
            it["key"] = json!(true);
            links = json!({ "time_ms": it["links"]["time_ms"], "story": it["links"]["story"], "rewind": it["links"]["rewind"] });
        }
    }
    Ok(json!({
        "question": question,
        "repo": main_s,
        "reading": reading_json(&reading, &ns),
        "answer": answer,
        "timeline": items,
        "conflict": conflict,
        "now": now,
        "links": links,
        "missing": missing,
        "ms": started.elapsed().as_millis() as u64,
    }))
}

/// The words that say a commit message gives a reason.
const REASON_WORDS: [&str; 16] = ["because", "after ", " for ", "ticket", "complain", "asked", "so that", "to match", "to fix", "request", "per ", "reported", "customer", "issue", "required", "since "];

/// The reason a commit message gives, quoted from it: its subject, and the
/// body's first sentence that says why when the subject does not. None when
/// neither says why (no model: the words of [`REASON_WORDS`], or `#41`).
fn reason_of(subject: &str, body: &str) -> Option<String> {
    let says = |s: &str| {
        let l = format!(" {} ", s.to_lowercase());
        REASON_WORDS.iter().any(|w| l.contains(w)) || l.as_bytes().windows(2).any(|w| w[0] == b'#' && w[1].is_ascii_digit()) || (l.contains('(') && l.contains(')'))
    };
    let subject = one_line(subject, 140);
    if says(&subject) {
        return Some(subject);
    }
    let sentence = body.split(['.', '\n']).map(str::trim).find(|s| s.len() > 6 && says(s))?;
    Some(format!("{subject}: {}", one_line(sentence, 140)))
}

/// When a later commit takes out what an earlier one brought in (a line it
/// added) or brings back what it took out, and the earlier message gave a
/// reason: both reasons, quoted, in one line. The newest such pair; no
/// model, only the diffs and the messages.
fn conflict_of(cs: &[Commit]) -> Json_ {
    let mut list: Vec<&Commit> = cs.iter().collect();
    list.sort_by_key(|c| c.at_ms);
    for j in (1..list.len()).rev() {
        let later = list[j];
        for i in (0..j).rev() {
            let earlier = list[i];
            let Some(why) = reason_of(&earlier.subject, &earlier.body) else { continue };
            let undid = later.removed.iter().any(|l| earlier.added.contains(l));
            let back = later.added.iter().any(|l| earlier.removed.contains(l));
            if !undid && !back {
                continue;
            }
            let later_why = reason_of(&later.subject, &later.body).unwrap_or_else(|| one_line(&later.subject, 140));
            let (es, ls): (String, String) = (earlier.sha.chars().take(7).collect(), later.sha.chars().take(7).collect());
            let verb = if back { "reverses" } else { "changes what" };
            let text = if back {
                format!("Two reasons conflict: {ls} reverses {es}, which was made for \u{201c}{why}\u{201d}; {ls} says \u{201c}{later_why}\u{201d}. Nothing records which one holds now.")
            } else {
                format!("Two reasons conflict: {es} was made for \u{201c}{why}\u{201d}, and {ls} changed it for \u{201c}{later_why}\u{201d}. Nothing records which one holds now.")
            };
            return json!({
                "text": text, "kind": verb,
                "earlier": { "sha": earlier.sha, "short": es, "reason": why, "ts_ms": earlier.at_ms, "id": format!("c-{es}") },
                "later": { "sha": later.sha, "short": ls, "reason": later_why, "ts_ms": later.at_ms, "id": format!("c-{ls}") },
            });
        }
    }
    Json_::Null
}

fn count(items: &[Json_], kind: &str) -> usize {
    items.iter().filter(|i| i["kind"] == kind).count()
}

/// The one-line answer: the commit where x became y (or, without them,
/// the newest commit that changed the subject; an uncommitted edit only
/// when no commit did), with the answer, rule or decision behind it, and
/// the whole history in one line when it changed more than once.
fn answer_of(r: &Reading, ns: &[Needle], items: &[Json_]) -> Option<Json_> {
    let moment = |i: &Json_| i["written"]["ts_ms"].as_i64().or(i["ts_ms"].as_i64()).unwrap_or(0);
    let commits: Vec<&Json_> = items.iter().filter(|i| i["kind"] == "commit").collect();
    let edits: Vec<&Json_> = items.iter().filter(|i| i["kind"] == "edit").collect();
    let pick = |list: &[&Json_]| -> Option<Json_> {
        let changed = |i: &&&Json_| i["change"] == "changed";
        // "From x to y": the change that did that; else the newest change.
        let k = if r.x.is_some() && r.y.is_some() { list.iter().filter(changed).min_by_key(|i| moment(i)) } else { None };
        k.or_else(|| list.iter().filter(|i| i["change"] == "changed" || i["change"] == "added").max_by_key(|i| moment(i))).or_else(|| list.iter().max_by_key(|i| moment(i))).map(|i| (*i).clone())
    };
    let key = pick(&commits).or_else(|| pick(&edits))?;
    let key = &key;
    let when = moment(key);
    let who = key["agent"]["name"].as_str().map(str::to_string).unwrap_or_else(|| key["by"].as_str().unwrap_or("someone").to_string());
    let subject = r.topic.as_ref().map(|t| format!("The {t}")).or_else(|| r.subjects.first().map(|s| s.to_string())).or_else(|| r.files.first().cloned());
    let short = key["commit"]["short"].as_str().unwrap_or("");
    let file = key["written"]["file"].as_str().map(str::to_string).or_else(|| key["lines"][0]["path"].as_str().map(str::to_string)).unwrap_or_default();
    let head = match (key["change"].as_str(), &r.x, &r.y) {
        (Some("changed"), Some(x), Some(y)) => format!("{} went from {x} to {y}", subject.clone().unwrap_or_else(|| "It".into())),
        (Some("added"), _, Some(y)) => format!("{} became {y}", subject.clone().unwrap_or_else(|| "It".into())),
        _ => format!("{} last changed", subject.clone().or_else(|| ns.first().map(|n| n.text.clone())).unwrap_or_else(|| "It".into())),
    };
    let how = if key["kind"] == "edit" {
        format!("in {who}'s edit to {file} (not committed yet)")
    } else if key["written"].is_object() {
        format!("in {who}'s edit to {file} (committed as {short})")
    } else {
        format!("in {who}'s commit {short} \u{201c}{}\u{201d}", one_line(key["commit"]["subject"].as_str().unwrap_or(""), 80))
    };
    // What came before it, in the same agent's work and about the same
    // thing: your answer to its question, a rule, its decision.
    let agent = key["agent"]["id"].as_str();
    let before = |i: &&Json_| i["ts_ms"].as_i64().is_some_and(|t| t <= when + 60_000 && t >= when - 2 * 3600_000);
    let about = |text: &str| ns.iter().any(|n| n.finds(text)) || (!file.is_empty() && text.contains(file.rsplit('/').next().unwrap_or(&file)));
    let after = items
        .iter()
        .filter(before)
        .filter(|i| i["kind"] == "question" && i["question"]["answer"].is_string() && agent.is_some() && i["agent"]["id"].as_str() == agent)
        .max_by_key(|i| i["ts_ms"].as_i64())
        .map(|q| {
            let by = if q["question"]["answered_by"] == "you" { "you answered" } else { "a rule answered" };
            format!(", after {by} \u{201c}{}\u{201d} to its question \u{201c}{}\u{201d}", one_line(q["question"]["answer"].as_str().unwrap_or(""), 60), one_line(q["question"]["text"].as_str().unwrap_or(""), 90))
        })
        .or_else(|| items.iter().filter(before).filter(|i| i["kind"] == "rule" && about(i["title"].as_str().unwrap_or(""))).max_by_key(|i| i["ts_ms"].as_i64()).map(|r| format!(", under your rule \u{201c}{}\u{201d}", one_line(r["title"].as_str().unwrap_or(""), 90))))
        .or_else(|| {
            items
                .iter()
                .filter(before)
                .filter(|i| i["kind"] == "decision" && agent.is_some() && i["agent"]["id"].as_str() == agent)
                .filter(|i| about(i["title"].as_str().unwrap_or("")) || i["ctx"]["files"].as_array().is_some_and(|f| f.iter().any(|x| x.as_str().is_some_and(|x| !file.is_empty() && x.ends_with(file.as_str())))))
                .max_by_key(|i| i["ts_ms"].as_i64())
                .map(|d| format!(", recorded with the decision \u{201c}{}\u{201d}", one_line(d["title"].as_str().unwrap_or(""), 90)))
        })
        .unwrap_or_default();
    let mut text = format!("{head} {{when}} {how}{after}.");
    // Not committed since: say so after the committed answer.
    if key["kind"] == "commit" {
        if let Some(e) = edits.iter().filter(|e| moment(e) > when).max_by_key(|e| moment(e)) {
            text.push_str(&format!(" Since then {} has an uncommitted edit to {}.", e["agent"]["name"].as_str().unwrap_or("an agent"), e["lines"][0]["path"].as_str().unwrap_or("it")));
        }
    }
    // The history, in one line: every commit that changed or brought it in.
    let mut changes: Vec<&&Json_> = commits.iter().filter(|c| matches!(c["change"].as_str(), Some("changed" | "added" | "removed")) && !import_only(c)).collect();
    if changes.len() < 2 {
        changes = commits.iter().filter(|c| !import_only(c)).collect();
    }
    changes.sort_by_key(|c| moment(c));
    let history: Vec<Json_> = changes
        .iter()
        .map(|c| json!({ "id": c["id"], "sha": c["commit"]["short"], "ts_ms": moment(c), "label": history_label(c, r), "by": c["agent"]["name"].as_str().or(c["by"].as_str()) }))
        .collect();
    let history_plain = (history.len() >= 2).then(|| {
        format!(
            "{} changed {} times: {}.",
            subject.clone().or_else(|| ns.first().map(|n| n.text.clone())).unwrap_or_else(|| "It".into()),
            history.len(),
            history.iter().map(|h| format!("{} ({})", h["label"].as_str().unwrap_or(""), utc(h["ts_ms"].as_i64().unwrap_or(0)).get(..10).unwrap_or(""))).collect::<Vec<_>>().join(" \u{2192} ")
        )
    });
    Some(json!({
        "text": text, "when_ms": when, "plain": text.replace("{when}", &format!("on {}", utc(when))), "key": key["id"],
        "history": if history.len() >= 2 { Json_::Array(history) } else { Json_::Array(Vec::new()) }, "history_plain": history_plain,
        "evidence": {
            "commits": count(items, "commit"), "edits": count(items, "edit"), "decisions": count(items, "decision"),
            "questions": count(items, "question"), "rules": count(items, "rule"), "goals": count(items, "goal"),
        },
    }))
}

/// A commit in a history line: what it made of the subject (y, when it
/// brought y in), else its subject, short.
fn history_label(c: &Json_, r: &Reading) -> String {
    let subj = c["commit"]["subject"].as_str().unwrap_or("");
    let short = c["commit"]["short"].as_str().unwrap_or("");
    // A step is named by the value it brought in (y, or x for the step
    // that brought x in), else by its subject.
    let brings = |v: &str| c["lines"].as_array().is_some_and(|l| l.iter().any(|l| l["sign"] == "+" && norm(l["text"].as_str().unwrap_or("")).contains(&norm(v))));
    let label = match (c["change"].as_str(), &r.y, &r.x) {
        (Some("changed" | "added"), Some(y), _) if brings(y) => y.clone(),
        (Some("changed" | "added"), _, Some(x)) if brings(x) => x.clone(),
        _ => one_line(subj, 48),
    };
    if label.is_empty() {
        short.to_string()
    } else {
        format!("{label} {short}")
    }
}

// -------------------------------------------------------------------- why

fn sql_text(words: &[String]) -> String {
    let mut seen = HashSet::new();
    words
        .iter()
        .flat_map(|w| parts(w))
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase())
        .filter(|w| w.chars().count() >= 2 && !matches!(w.as_str(), "and" | "or" | "not" | "near"))
        .filter(|w| seen.insert(w.clone()))
        .take(16)
        .collect::<Vec<_>>()
        .join(" ")
}

#[allow(clippy::too_many_arguments)]
fn ctx_items(
    sup: &Supervisor, main: &Path, question: &str, r: &Reading, ns: &[Needle], hands: &HashSet<String>, s: &trail::Sessions, now: &Json_, items: &mut Vec<Json_>,
    missing: &mut Vec<String>,
) {
    let bin = &sup.cfg.chloectx_bin;
    let mut ask_ids: Vec<String> = Vec::new();
    match trail::run_chloectx(bin, main, &["ask", "--json", "--", question], Duration::from_secs(60)) {
        Ran::Out(v) => ask_ids = v["ids"].as_array().into_iter().flatten().filter_map(Json_::as_str).map(str::to_string).collect(),
        Ran::Missing => {
            missing.push(trail::not_installed(bin) + ": decisions are left out.");
            return;
        }
        Ran::Failed(e) => missing.push(e),
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |sup: &Supervisor, items: &mut Vec<Json_>, id: &str, kind: &str, text: &str, ts: &Json_, session: &str, files: Vec<String>, superseded: bool, via: &str| {
        if id.is_empty() || !seen.insert(id.to_string()) {
            if let Some(it) = items.iter_mut().find(|i| i["ctx"]["id"] == id) {
                if let Some(v) = it["ctx"]["via"].as_array_mut() {
                    if !v.iter().any(|x| x == via) {
                        v.push(json!(via));
                    }
                }
            }
            return;
        }
        let ms = trail::ctx_ms(ts).unwrap_or(0);
        let a = if session.is_empty() { s.of_record(id) } else { s.agent(session).or_else(|| s.of_record(id)) };
        items.push(json!({
            "id": format!("d-{id}"), "kind": "decision", "ts": iso(ms), "ts_ms": ms,
            "agent": a.map(|a| brief(sup, a)), "by": a.map_or("a session outside Workbench".to_string(), |a| a.name.clone()),
            "title": text, "text": "",
            "ctx": { "id": id, "kind": kind, "session": session, "superseded": superseded, "files": files, "via": [via] },
            "links": {
                "story": a.map(|a| json!({ "agent": a.id, "at_ms": ms })), "rewind": a.map_or(Json_::Null, |a| rewind(sup, a, ms)),
                "why": null, "diff": null, "time_ms": ms,
            },
        }));
    };
    // The records that name the subject, and those ask found.
    let mut terms: Vec<String> = ns.iter().map(|n| n.text.clone()).collect();
    terms.extend(r.words.iter().cloned());
    terms.extend(r.files.iter().map(|f| f.rsplit('/').next().unwrap_or(f).to_string()));
    let m = sql_text(&terms);
    let ids: Vec<String> = ask_ids.iter().filter(|i| i.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_'))).map(|i| format!("'{i}'")).collect();
    let mut cond: Vec<String> = Vec::new();
    if !m.is_empty() {
        cond.push(format!("MATCH(body, '{m}', min => 1)"));
    }
    if !ids.is_empty() {
        cond.push(format!("id IN ({})", ids.join(", ")));
    }
    if !cond.is_empty() {
        let sql = format!("SELECT id, kind, tag, ts, session, text, files, links FROM memory WHERE kind IN ('decision', 'preference', 'fact') AND ({}) ORDER BY ts DESC LIMIT 200", cond.join(" OR "));
        match trail::run_chloectx(bin, main, &["sql", "--json", &sql], Duration::from_secs(30)) {
            Ran::Out(out) => {
                let cols: Vec<String> = out["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
                let col = |row: &Vec<Json_>, name: &str| cols.iter().position(|c| c == name).and_then(|i| row.get(i).cloned()).unwrap_or(Json_::Null);
                let mut picked: Vec<(usize, Vec<Json_>)> = Vec::new();
                for row in out["rows"].as_array().into_iter().flatten().filter_map(Json_::as_array) {
                    let id = col(row, "id").as_str().unwrap_or("").to_string();
                    let text = col(row, "text").as_str().unwrap_or("").to_string();
                    let session = col(row, "session").as_str().unwrap_or("").to_string();
                    let files = trail::files_of(&col(row, "files")).join(" ");
                    let mut score = relevance(&format!("{text} {files}"), ns, &r.words);
                    if ask_ids.contains(&id) {
                        score += 3;
                    }
                    let mine = s.agent(&session).or_else(|| s.of_record(&id)).is_some_and(|a| hands.contains(&a.id));
                    if score >= 2 || (mine && score >= 1) {
                        picked.push((score + if mine { 2 } else { 0 }, row.clone()));
                    }
                }
                picked.sort_by_key(|(sc, _)| std::cmp::Reverse(*sc));
                for (_, row) in picked.into_iter().take(10) {
                    let links = col(&row, "links").to_string();
                    let superseded = col(&row, "tag") == "superseded" || links.contains("superseded_by");
                    add(
                        sup,
                        items,
                        col(&row, "id").as_str().unwrap_or(""),
                        col(&row, "kind").as_str().unwrap_or("decision"),
                        col(&row, "text").as_str().unwrap_or(""),
                        &col(&row, "ts"),
                        col(&row, "session").as_str().unwrap_or(""),
                        trail::files_of(&col(&row, "files")),
                        superseded,
                        "sql",
                    );
                }
            }
            Ran::Missing => {}
            Ran::Failed(e) => missing.push(e),
        }
    }
    // The decisions of the turn that wrote the line as it is now.
    if let Some(c) = now["code"].as_array().and_then(|c| c.first()) {
        let line = c["hits"].as_array().and_then(|h| h.first()).and_then(Json_::as_i64);
        let dir = match c["agent"]["id"].as_str().and_then(|id| sup.agent_row(id)) {
            Some(a) => PathBuf::from(a.cwd),
            None => main.to_path_buf(),
        };
        if let (Some(path), Some(line)) = (c["path"].as_str(), line) {
            let target = format!("{path}:{line}");
            match trail::run_chloectx(bin, &dir, &["blame", &target, "--json"], Duration::from_secs(60)) {
                Ran::Out(b) => {
                    let session = b["edit"]["session"].as_str().unwrap_or("").to_string();
                    for n in b["notes"].as_array().into_iter().flatten().filter(|n| n["kind"] == "decision" || n["kind"] == "preference") {
                        add(sup, items, n["id"].as_str().unwrap_or(""), n["kind"].as_str().unwrap_or("decision"), n["text"].as_str().unwrap_or(""), &n["ts"], &session, vec![path.to_string()], false, "blame");
                    }
                }
                Ran::Missing => {}
                Ran::Failed(e) => missing.push(e),
            }
        }
    }
}

fn decide_items(sup: &Supervisor, main: &str, r: &Reading, ns: &[Needle], hands: &HashSet<String>, by_id: &HashMap<String, Agent>, items: &mut Vec<Json_>) {
    let mut asked: HashSet<String> = HashSet::new();
    for q in crate::questions::all(sup).into_iter().filter(|q| q.repo.as_deref() == Some(main)) {
        let labels: Vec<String> = q.options.as_array().into_iter().flatten().map(|o| o["label"].as_str().unwrap_or("").to_string()).collect();
        let text = format!("{} {} {} {}", q.text, q.context.as_deref().unwrap_or(""), q.answer.as_deref().unwrap_or(""), labels.join(" "));
        let score = relevance(&text, ns, &r.words);
        let mine = q.agent.as_ref().is_some_and(|a| hands.contains(a));
        if !(score >= 2 || (mine && score >= 1)) {
            continue;
        }
        asked.insert(q.id.clone());
        let a = q.agent.as_ref().and_then(|id| by_id.get(id).cloned().or_else(|| sup.agent_row(id)));
        let at = q.answered.unwrap_or(q.created);
        let by = match q.answered_by.as_deref() {
            Some("you") => "you".to_string(),
            Some(b) if b.starts_with("rule:") => "a rule".to_string(),
            Some("terminal") => "you, in its terminal".to_string(),
            _ => "nobody yet".to_string(),
        };
        let text = match (q.state.as_str(), &q.answer) {
            ("answered", Some(ans)) => format!("{} answered \u{201c}{}\u{201d}{}.", capital(&by), one_line(ans, 80), q.answered.map(|t| format!(" {} after it asked", span(t - q.created))).unwrap_or_default()),
            ("open", _) => "Still open: nobody has answered it.".to_string(),
            _ => "It expired without an answer.".to_string(),
        };
        items.push(json!({
            "id": format!("q-{}", q.id), "kind": "question", "ts": iso(at), "ts_ms": at,
            "agent": a.as_ref().map(|a| brief(sup, a)), "by": by, "title": q.text, "text": text,
            "question": {
                "id": q.id, "text": q.text, "context": q.context, "answer": q.answer, "answered_by": q.answered_by, "state": q.state,
                "options": q.options, "asked": iso(q.created), "asked_ms": q.created, "rule": q.rule,
            },
            "links": {
                "story": a.as_ref().map(|a| json!({ "agent": a.id, "at_ms": q.created })), "rewind": a.as_ref().map_or(Json_::Null, |a| rewind(sup, a, q.created)),
                "why": null, "diff": null, "time_ms": at,
            },
        }));
    }
    for rule in crate::questions::rules(sup, Some(main), false) {
        let from_q = rule.question.as_ref().is_some_and(|q| asked.contains(q));
        if !(from_q || relevance(&rule.text, ns, &r.words) >= 2) {
            continue;
        }
        items.push(json!({
            "id": format!("r-{}", rule.id), "kind": "rule", "ts": iso(rule.created), "ts_ms": rule.created, "agent": null, "by": "you",
            "title": rule.text,
            "text": if from_q { "Made from your answer; every agent of the repository is told it." } else { "Every agent of the repository is told it." },
            "rule": { "id": rule.id, "text": rule.text, "active": rule.active, "question": rule.question, "ctx_id": rule.ctx_id },
            "links": { "story": null, "rewind": null, "why": null, "diff": null, "time_ms": rule.created },
        }));
    }
}

fn goal_items(sup: &Supervisor, hands: &HashSet<String>, by_id: &HashMap<String, Agent>, items: &mut Vec<Json_>) {
    let mut ids: Vec<&String> = hands.iter().collect();
    ids.sort();
    for id in ids {
        let Some(a) = by_id.get(id) else { continue };
        let Some(g) = a.goal.as_deref().and_then(|g| crate::goals::goal(sup, g).ok()) else { continue };
        let o = a.outcome.as_deref().and_then(|o| crate::goals::outcome(sup, o).ok());
        let at = o.as_ref().map_or(g.created, |o| o.created.max(g.created));
        items.push(json!({
            "id": format!("g-{}-{}", g.id, a.id), "kind": "goal", "ts": iso(at), "ts_ms": at,
            "agent": brief(sup, a), "by": "you", "title": g.text,
            "text": match &o { Some(o) => format!("{} worked on the outcome \u{201c}{}\u{201d}.", a.name, o.title), None => format!("{} worked toward this goal.", a.name) },
            "goal": { "id": g.id, "text": g.text, "state": g.state, "outcome": o.as_ref().map(|o| json!({ "id": o.id, "title": o.title, "state": o.state })) },
            "links": { "story": { "agent": a.id, "at_ms": at }, "rewind": null, "why": null, "diff": null, "time_ms": at },
        }));
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// 40 s, 2 min, 3 h.
pub fn span(ms: i64) -> String {
    let s = (ms.max(0) + 500) / 1000;
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", (s + 30) / 60)
    } else if s < 48 * 3600 {
        format!("{} h", (s + 1800) / 3600)
    } else {
        format!("{} days", (s + 43_200) / 86_400)
    }
}

// --------------------------------------------------------------------- now

/// Where a definition starts: `fn`, `def`, `function`, `class` and their kin,
/// after modifiers. Answers the defined name.
fn def_name(line: &str) -> Option<String> {
    let mut ws = line.trim_start().split_whitespace().peekable();
    const MODS: &[&str] = &["pub", "pub(crate)", "pub(super)", "export", "default", "async", "static", "public", "private", "protected", "unsafe", "extern", "const", "abstract", "final", "override"];
    const KW: &[&str] = &["fn", "def", "function", "func", "class", "impl", "struct", "enum", "trait", "interface", "type", "macro_rules!", "mod"];
    let mut saw_const = false;
    while let Some(w) = ws.peek() {
        if MODS.contains(w) || w.starts_with("pub(") {
            saw_const |= *w == "const";
            ws.next();
        } else {
            break;
        }
    }
    let w = ws.next()?;
    let w = if w.starts_with("impl<") { "impl" } else { w };
    let name_of = |n: &str| n.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).find(|s| !s.is_empty()).map(str::to_string);
    if KW.contains(&w) || w.starts_with("function") {
        let n = if w == "impl" { ws.find(|x| *x != "for" && !x.starts_with('<'))? } else { ws.next()? };
        return name_of(n);
    }
    // `const total = (…) =>` and `let f = function`.
    if saw_const || matches!(w, "let" | "var") {
        let n = if saw_const { w } else { ws.next()? };
        let rest: String = ws.collect::<Vec<_>>().join(" ");
        if rest.starts_with('=') && (rest.contains("=>") || rest.contains("function")) {
            return name_of(n);
        }
    }
    None
}

/// The definition around line `hit` (0-based) of `lines`: (first, last, name),
/// 0-based and inclusive, with its doc comment; a window when none is found.
fn around(lines: &[&str], hit: usize) -> (usize, usize, Option<String>) {
    let lo = hit.saturating_sub(80);
    // A hit in a doc comment is about the definition below it.
    let comment = |l: &str| {
        let t = l.trim_start();
        t.starts_with("//") || t.starts_with('#') || t.starts_with("/*") || t.starts_with('*') || t.starts_with("\"\"\"")
    };
    let below = if comment(lines[hit]) { (hit..lines.len().min(hit + 8)).take_while(|&i| i == hit || comment(lines[i]) || def_name(lines[i]).is_some()).find_map(|i| def_name(lines[i]).map(|n| (i, n))) } else { None };
    let found = below.or_else(|| (lo..=hit).rev().find_map(|i| def_name(lines[i]).map(|n| (i, n))));
    let Some((d, name)) = found else {
        return (hit.saturating_sub(6), (hit + 10).min(lines.len().saturating_sub(1)), None);
    };
    let indent = |l: &str| l.len() - l.trim_start().len();
    // Its end: braces back to level, or the indentation back to the def's.
    let mut end = d;
    let mut depth = 0i64;
    let mut opened = false;
    let python = lines[d].trim_end().ends_with(':');
    for (i, l) in lines.iter().enumerate().skip(d).take(80) {
        end = i;
        if python {
            if i > d && !l.trim().is_empty() && indent(l) <= indent(lines[d]) {
                end = i - 1;
                break;
            }
            continue;
        }
        for c in l.chars() {
            match c {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if opened && depth <= 0 {
            break;
        }
        if !opened && l.trim_end().ends_with(';') {
            break;
        }
    }
    while end > d && lines[end].trim().is_empty() {
        end -= 1;
    }
    // A definition that ends before the hit (a short one above it) is not around it.
    if end < hit && d <= hit {
        return (hit.saturating_sub(6), (hit + 10).min(lines.len().saturating_sub(1)), None);
    }
    let mut start = d;
    while start > 0 && start > d.saturating_sub(8) {
        let t = lines[start - 1].trim_start();
        if t.starts_with("///") || t.starts_with("//") || t.starts_with('#') || t.starts_with("/*") || t.starts_with('*') || t.starts_with('@') {
            start -= 1;
        } else {
            break;
        }
    }
    (start, end.min(start + 60), Some(name))
}

fn code_block(dir: &Path, rel: &str, hits: &[usize], tree: &str, agent: Option<&Agent>, main: &str) -> Option<Json_> {
    let p = dir.join(rel);
    let meta = std::fs::metadata(&p).ok()?;
    if !meta.is_file() || meta.len() > 4 << 20 {
        return None;
    }
    let body = String::from_utf8_lossy(&std::fs::read(&p).ok()?).into_owned();
    let lines: Vec<&str> = body.lines().collect();
    let first = *hits.first()?;
    if first == 0 || first > lines.len() {
        return None;
    }
    let (s, e, name) = around(&lines, first - 1);
    let in_block: Vec<usize> = hits.iter().copied().filter(|h| *h > s && *h <= e + 1).collect();
    let why = match agent {
        Some(a) => json!({ "agent": a.id, "path": rel, "line": first }),
        None => json!({ "repo": main, "path": rel, "line": first }),
    };
    Some(json!({
        "path": rel, "start_line": s + 1, "end_line": e + 1, "symbol": name, "tree": tree,
        "agent": agent.map(|a| json!({ "id": a.id, "name": a.name })), "hits": in_block,
        "text": lines[s..=e].join("\n"), "why": why,
    }))
}

/// `git grep` of the needles in `dir`: path to hit lines (1-based).
fn grep(dir: &Path, ns: &[Needle], specs: &[String]) -> Vec<(String, Vec<usize>)> {
    let mut a: Vec<String> = vec!["grep".into(), "-n".into(), "-I".into(), "-E".into(), "--no-color".into()];
    for n in ns {
        a.push("-e".into());
        a.push(n.ere());
    }
    a.push("--".into());
    a.extend(specs.iter().cloned());
    let args: Vec<&str> = a.iter().map(String::as_str).collect();
    let out = git(dir, &args).unwrap_or_default();
    let mut by: Vec<(String, Vec<usize>)> = Vec::new();
    for l in out.lines().take(2000) {
        let mut f = l.splitn(3, ':');
        let (Some(p), Some(n)) = (f.next(), f.next().and_then(|n| n.parse::<usize>().ok())) else { continue };
        match by.iter_mut().find(|(q, _)| q == p) {
            Some((_, v)) => v.push(n),
            None => by.push((p.to_string(), vec![n])),
        }
    }
    by
}

/// BM25 over the Search part's chunks of the main checkout, packed.
fn chunks_sql(budget: i64) -> String {
    format!("SELECT path, start_line, end_line, symbol, text FROM code_chunks WHERE repo = :repo AND tree = 'main' AND MATCH(text, :q, min => 1) ORDER BY bm25(text, :q) DESC PACK {budget} TOKENS OF text LIMIT 40")
}

const DOCS_SQL: &str = "SELECT title, url, text, lib FROM doc_pages WHERE repo = :repo AND MATCH(text, :q, min => 1) ORDER BY bm25(text, :q) DESC LIMIT 3";

fn query_rows(sup: &Supervisor, sql: &str, p: &Params) -> Result<Vec<Vec<Value>>, String> {
    sup.db().query(sql, p)
}

/// How it works now: the code that holds the subject, and docs pages.
fn now_code(sup: &Supervisor, main: &Path, r: &Reading, ns: &[Needle], hands: &[Agent], missing: &mut Vec<String>) -> Json_ {
    let main_s = main.to_string_lossy().to_string();
    let specs = pathspecs(r);
    // What it is now: y when there is one, else every needle.
    let want: Vec<Needle> = match ns.iter().find(|n| n.role == 'y') {
        Some(y) => std::iter::once(y.clone()).chain(ns.iter().filter(|n| n.role == 's').cloned()).collect(),
        None => ns.to_vec(),
    };
    // y itself first: a subject alone may only find where it still is x.
    let passes: Vec<Vec<Needle>> = match ns.iter().find(|n| n.role == 'y') {
        Some(y) if want.len() > 1 => vec![vec![y.clone()], want.clone()],
        _ => vec![want.clone()],
    };
    let mut terms: Vec<String> = want.iter().map(|n| n.text.clone()).collect();
    if let Some(t) = &r.topic {
        terms.push(t.clone());
    }
    let words = sql_text(&terms);
    let mut code: Vec<Json_> = Vec::new();
    let mut source = Json_::Null;
    let mut pack = Json_::Null;
    let mut lack: Vec<String> = Vec::new();

    // BM25 over the Search part's chunks, packed, when that table exists.
    if !words.is_empty() && !want.is_empty() {
        let p = Params::new().set("repo", Value::Str(main_s.clone())).set("q", Value::Str(words.clone()));
        let budget = 1200;
        if let Ok(rows) = query_rows(sup, &chunks_sql(budget), &p) {
            let mut used = 0i64;
            let mut found = Vec::new();
            for row in &rows {
                let decision = row.get(6).and_then(get_str).unwrap_or_else(|| "take".into());
                if decision.starts_with("skip") {
                    continue;
                }
                used += row.get(5).and_then(get_int).unwrap_or(0);
                let text = get_str(&row[4]).unwrap_or_default();
                let start = get_int(&row[1]).unwrap_or(1);
                let hits: Vec<i64> = text.lines().enumerate().filter(|(_, l)| passes[0].iter().any(|n| n.finds(l))).map(|(i, _)| start + i as i64).collect();
                let path = get_str(&row[0]).unwrap_or_default();
                if !in_scope(&path, &specs) {
                    continue;
                }
                let first = hits.first().copied().unwrap_or(start);
                found.push(json!({
                    "path": path, "start_line": start, "end_line": get_int(&row[2]).unwrap_or(start), "symbol": get_str(&row[3]),
                    "tree": "main", "agent": null, "hits": hits, "text": text, "why": { "repo": main_s, "path": get_str(&row[0]), "line": first },
                }));
            }
            // The chunks that hold the needles first.
            found.sort_by_key(|c| c["hits"].as_array().is_none_or(|h| h.is_empty()));
            if found.iter().any(|c| c["hits"].as_array().is_some_and(|h| !h.is_empty())) {
                found.truncate(3);
                code = found;
                source = json!("code_chunks");
                pack = json!({ "budget": budget, "used": used });
            }
        }
    }
    // Else git grep in the main checkout, then in the worktrees of the agents
    // that made the change.
    if code.is_empty() && !want.is_empty() {
        let mut trees: Vec<(PathBuf, String, Option<&Agent>)> = vec![(main.to_path_buf(), "main".into(), None)];
        for a in hands {
            if a.cwd != main_s && Path::new(&a.cwd).is_dir() {
                trees.push((PathBuf::from(&a.cwd), a.name.clone(), Some(a)));
            }
        }
        'passes: for pass in &passes {
            for (dir, tree, a) in &trees {
                let mut by = grep(dir, pass, &specs);
                if by.is_empty() {
                    continue;
                }
                by.sort_by_key(|(_, hits)| std::cmp::Reverse(hits.len().min(5)));
                for (p, hits) in by.iter().take(3) {
                    if let Some(b) = code_block(dir, p, hits, tree, *a, &main_s) {
                        code.push(b);
                    }
                }
                source = json!("git grep");
                break 'passes;
            }
        }
    }
    if code.is_empty() && !want.is_empty() {
        lack.push(format!("No file holds {} now.", want.iter().map(|n| n.label()).collect::<Vec<_>>().join(" or ")));
    }

    // Docs pages, when the Search part fetched some.
    let mut docs = Vec::new();
    if !words.is_empty() {
        let p = Params::new().set("repo", Value::Str(main_s.clone())).set("q", Value::Str(words.clone()));
        if let Ok(rows) = query_rows(sup, DOCS_SQL, &p) {
            for row in rows {
                let text = get_str(&row[2]).unwrap_or_default();
                docs.push(json!({ "title": get_str(&row[0]), "url": get_str(&row[1]), "lib": get_str(&row[3]), "excerpt": excerpt(&text, &words) }));
            }
        }
    }
    let _ = missing;
    json!({ "code": code, "docs": docs, "source": source, "pack": pack, "missing": lack })
}

/// About 300 characters of `text` around the first of `words`.
fn excerpt(text: &str, words: &str) -> String {
    let lower = text.to_lowercase();
    let at = words.split_whitespace().filter_map(|w| lower.find(w)).min().unwrap_or(0);
    let mut start = at.saturating_sub(100);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let s: String = text[start..].chars().take(300).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if start > 0 {
        format!("…{s}")
    } else {
        s
    }
}

// ------------------------------------------------------------------ explain

/// The evidence as lines a model can cite: `[E1] 2026-11-13 19:20 UTC
/// commit 8e41b07 by cart: …`.
pub fn evidence_text(t: &Json_) -> String {
    let mut out = String::new();
    for it in t["timeline"].as_array().into_iter().flatten() {
        let tag = it["tag"].as_str().unwrap_or("");
        let when = it["ts_ms"].as_i64().map(utc).unwrap_or_default();
        let by = it["by"].as_str().unwrap_or("");
        let head = match it["kind"].as_str().unwrap_or("") {
            "commit" => format!("commit {} by {by}: {}", it["commit"]["short"].as_str().unwrap_or(""), it["title"].as_str().unwrap_or("")),
            "edit" => format!("uncommitted edit by {by}: {}", it["title"].as_str().unwrap_or("")),
            "decision" => format!("decision ({}) of {by}: \"{}\"", it["ctx"]["id"].as_str().unwrap_or(""), it["title"].as_str().unwrap_or("")),
            "question" => format!("question of {by}: \"{}\" {}", it["title"].as_str().unwrap_or(""), it["text"].as_str().unwrap_or("")),
            "rule" => format!("rule the user made: \"{}\"", it["title"].as_str().unwrap_or("")),
            "goal" => format!("goal: \"{}\" {}", it["title"].as_str().unwrap_or(""), it["text"].as_str().unwrap_or("")),
            k => format!("{k}: {}", it["title"].as_str().unwrap_or("")),
        };
        out.push_str(&format!("[{tag}] {when} {head}\n"));
        for l in it["lines"].as_array().into_iter().flatten().take(4) {
            out.push_str(&format!("    {} {}:{} {}\n", l["sign"].as_str().unwrap_or(""), l["path"].as_str().unwrap_or(""), l["line"].as_i64().map(|n| n.to_string()).unwrap_or_default(), l["text"].as_str().unwrap_or("").trim()));
        }
    }
    for c in t["now"]["code"].as_array().into_iter().flatten().take(2) {
        out.push_str(&format!("[now] {}:{}-{} {}\n", c["path"].as_str().unwrap_or(""), c["start_line"], c["end_line"], c["symbol"].as_str().unwrap_or("")));
        for l in c["text"].as_str().unwrap_or("").lines().take(30) {
            out.push_str(&format!("    {l}\n"));
        }
    }
    out
}

/// Hex words of 7 to 40 characters in `text` (with at least one digit and
/// one letter, so "decade" is not a commit) that start no shown commit.
fn unshown_commits(text: &str, shown: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let hexy = (7..=40).contains(&w.len()) && w.chars().all(|c| c.is_ascii_hexdigit()) && w.chars().any(|c| c.is_ascii_digit()) && w.chars().any(|c| c.is_ascii_alphabetic());
        let w = w.to_ascii_lowercase();
        if hexy && !shown.iter().any(|s| s.to_ascii_lowercase().starts_with(&w)) && !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// A commit whose lines about the subject only import or re-export it:
/// not a change to how it works (Priya: "changed 5 times" counted them).
fn import_only(c: &Json_) -> bool {
    let lines: Vec<&str> = c["lines"].as_array().into_iter().flatten().filter_map(|l| l["text"].as_str()).map(str::trim).filter(|t| !t.is_empty()).collect();
    !lines.is_empty()
        && lines.iter().all(|t| {
            let t = t.trim_start_matches(['+', '-']).trim();
            t.starts_with("import ") || (t.starts_with("from ") && t.contains(" import ")) || t.starts_with("use ") || t.starts_with("pub use ") || t.starts_with("#include") || t.contains("require(") || (t.starts_with("export ") && t.contains(" from "))
        })
}

/// `POST /api/trace/explain`: the trace, then `chloectx ask --deep --json`
/// (a model) with the question and the evidence.
pub fn explain(sup: &Supervisor, q: &TraceQ) -> Result<Json_, ApiError> {
    let t = trace(sup, q)?;
    let main = PathBuf::from(t["repo"].as_str().unwrap_or(""));
    let mut ev = evidence_text(&t);
    if ev.len() > 6000 {
        let mut end = 6000;
        while !ev.is_char_boundary(end) {
            end -= 1;
        }
        ev.truncate(end);
        ev.push_str("\n…");
    }
    let tags: Vec<String> = t["timeline"].as_array().into_iter().flatten().map(|i| format!("{} {} {}", i["tag"].as_str().unwrap_or(""), i["kind"].as_str().unwrap_or(""), one_line(i["title"].as_str().unwrap_or(""), 60))).collect();
    let question = format!(
        "{}\n\nAnswer from the evidence Workbench found below, which is what the person sees; cite it as [E1], [E2]. Name only the commits listed below: if memory speaks of another commit, say it is not in this evidence. Say when, why and how it works now.\n\n{ev}",
        t["question"].as_str().unwrap_or("")
    );
    let bin = &sup.cfg.chloectx_bin;
    match trail::run_chloectx(bin, &main, &["ask", "--deep", "--json", "--", &question], Duration::from_secs(300)) {
        Ran::Out(v) => {
            // Priya: Explain named commits the timeline did not show. Say so.
            let shown: Vec<String> = t["timeline"].as_array().into_iter().flatten().filter_map(|i| i["commit"]["sha"].as_str().map(str::to_string)).collect();
            let said = v["text"].as_str().or(v["answer"].as_str()).unwrap_or("");
            let missing: Vec<String> = unshown_commits(said, &shown).into_iter().map(|s| format!("the answer names commit {s}, which is not in the evidence shown here")).collect();
            Ok(json!({
                "available": true,
                "answer": v["answer"], "text": v["text"], "confidence": v["confidence"], "cost_usd": v["cost_usd"], "ids": v["ids"], "ms": v["ms"],
                "evidence": tags, "missing": missing,
            }))
        }
        Ran::Missing => Ok(json!({ "available": false, "why": trail::not_installed(bin), "evidence": tags, "missing": [trail::not_installed(bin)] })),
        Ran::Failed(e) => Ok(json!({ "available": false, "why": e, "evidence": tags, "missing": [e] })),
    }
}

/// `GET /api/trace/commit`: a commit's diff.
pub fn commit(sup: &Supervisor, repo: Option<&str>, agent: Option<&str>, sha: Option<&str>) -> Result<Json_, ApiError> {
    let sha = sha.filter(|s| (4..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())).ok_or_else(|| ApiError::bad("sha is required: a commit's hex id"))?;
    let repo = match (repo.filter(|r| !r.is_empty()), agent.filter(|a| !a.is_empty())) {
        (Some(r), _) => Some(r.to_string()),
        (None, Some(id)) => Some(sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?.repo),
        _ => None,
    };
    let main = trail::repo_dir(repo.as_deref())?;
    let meta = git(&main, &["log", "-1", "--format=%H%x1f%at%x1f%an%x1f%s", sha, "--"]).map_err(|e| ApiError::not_found(format!("no commit {sha}: {}", one_line(&e, 120))))?;
    let f: Vec<&str> = meta.trim().splitn(4, '\u{1f}').collect();
    if f.len() < 4 {
        return Err(ApiError::not_found(format!("no commit {sha}")));
    }
    let mut text = git(&main, &["show", "--format=", "--no-color", "--no-ext-diff", f[0], "--"]).map_err(ApiError::internal)?;
    if text.len() > 2 << 20 {
        let mut end = 2 << 20;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    let ms = f[1].parse::<i64>().unwrap_or(0) * 1000;
    Ok(json!({
        "sha": f[0], "short": f[0].chars().take(7).collect::<String>(), "author": f[2], "subject": f[3], "ts": iso(ms), "ts_ms": ms,
        "files": crate::git::parse_diff(&text),
    }))
}

/// The trace as text with citations, for the MCP tool `trace`.
pub fn as_text(t: &Json_) -> String {
    let mut out = String::new();
    match t["answer"]["plain"].as_str() {
        Some(a) => out.push_str(&format!("Answer: {a}\n")),
        None => out.push_str("Answer: nothing in git, Workbench's events or ChloeCtx matches this.\n"),
    }
    let r = &t["reading"];
    let mut reading = Vec::new();
    for (k, label) in [("x", "x"), ("y", "y"), ("topic", "about")] {
        if let Some(v) = r[k].as_str() {
            reading.push(format!("{label}={v}"));
        }
    }
    for s in r["subjects"].as_array().into_iter().flatten().filter_map(Json_::as_str) {
        reading.push(format!("subject={s}"));
    }
    for p in r["paths"].as_array().into_iter().flatten().filter_map(Json_::as_str) {
        reading.push(format!("in {p}"));
    }
    out.push_str(&format!("Read as: {}\n", if reading.is_empty() { "nothing to search for".into() } else { reading.join(", ") }));
    if t["timeline"].as_array().is_some_and(|a| !a.is_empty()) {
        out.push_str("\nEvidence, oldest first:\n");
        out.push_str(&evidence_text(&json!({ "timeline": t["timeline"], "now": {} })));
    }
    let code = t["now"]["code"].as_array().cloned().unwrap_or_default();
    if !code.is_empty() {
        out.push_str(&format!("\nHow it works now ({}):\n", t["now"]["source"].as_str().unwrap_or("")));
        for c in code.iter().take(2) {
            out.push_str(&format!("{}:{}-{}{}\n", c["path"].as_str().unwrap_or(""), c["start_line"], c["end_line"], c["symbol"].as_str().map(|s| format!(" {s}")).unwrap_or_default()));
            for (i, l) in c["text"].as_str().unwrap_or("").lines().take(40).enumerate() {
                out.push_str(&format!("{:>5}  {l}\n", c["start_line"].as_i64().unwrap_or(1) + i as i64));
            }
        }
    }
    let missing: Vec<&str> = t["missing"].as_array().into_iter().flatten().filter_map(Json_::as_str).collect();
    if !missing.is_empty() {
        out.push_str(&format!("\nNot found: {}\n", missing.join(" ")));
    }
    out.push_str("\nCite evidence by its tag ([E1]) or commit id when you act on it.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_from_x_to_y_with_its_topic_and_kinds() {
        let r = read("when and why did we change the cart total rounding from floor to half-up, and how does it work now?", None);
        assert_eq!(r.x.as_deref(), Some("floor"));
        assert_eq!(r.y.as_deref(), Some("half-up"));
        assert_eq!(r.topic.as_deref(), Some("cart total rounding"));
        assert_eq!(r.kinds, ["when", "why", "how"]);
        assert!(r.words.contains(&"rounding".to_string()), "{r:?}");
        let ns = needles(&r);
        assert_eq!(ns.iter().map(|n| n.text.as_str()).collect::<Vec<_>>(), ["floor", "half-up"]);
        assert!(ns[1].finds("let off = round_half_up(x);") && ns[1].finds("roundHalfUp(off)") && !ns[1].finds("half of it, up"));
    }

    #[test]
    fn reads_between_x_and_y_as_a_change_either_way() {
        let r = read("when and why did to_cents switch between banker's rounding and half up?", None);
        assert_eq!((r.x.as_deref(), r.y.as_deref(), r.between), (Some("banker rounding"), Some("half up"), true), "{r:?}");
        assert_eq!(r.subjects, ["to_cents"]);
    }

    #[test]
    fn says_when_two_reasons_conflict() {
        let c = |sha: &str, at: i64, subject: &str, body: &str, added: &[&str], removed: &[&str]| Commit {
            sha: sha.into(), at_ms: at, author: "Dana".into(), subject: subject.into(), change: "changed", lines: Vec::new(), files: vec!["money.py".into()], body: body.into(),
            added: added.iter().map(|s| s.to_string()).collect(), removed: removed.iter().map(|s| s.to_string()).collect(),
        };
        let floor = c("4e51a20aa", 1_000, "money: rounding down", "", &["return int(amount * 100)"], &[]);
        let up = c("9355961bb", 2_000, "money: round half up", "Support ticket #41: customers complained floor lost a cent.", &["return int(Decimal(amount * 100).quantize(1, ROUND_HALF_UP))"], &["return int(amount * 100)"]);
        let even = c("b72f105cc", 3_000, "money: banker's rounding to match the ledger export (finance asked)", "", &["return round(amount * 100)"], &["return int(Decimal(amount * 100).quantize(1, ROUND_HALF_UP))"]);
        let v = conflict_of(&[floor.clone(), up.clone(), even.clone()]);
        let text = v["text"].as_str().unwrap();
        assert!(text.contains("9355961") && text.contains("b72f105") && text.contains("ticket #41") && text.contains("finance asked"), "{text}");
        assert_eq!((v["earlier"]["short"].as_str(), v["later"]["short"].as_str()), (Some("9355961"), Some("b72f105")));
        // A later commit that brings back what a reasoned one took out reverses it.
        let back = c("d00d000dd", 4_000, "money: back to half up", "", &["return int(Decimal(amount * 100).quantize(1, ROUND_HALF_UP))"], &["return round(amount * 100)"]);
        let v = conflict_of(&[up.clone(), even.clone(), back]);
        assert_eq!(v["kind"], "reverses", "{v}");
        // No reason given, or nothing of the earlier one touched: nothing to say.
        assert!(conflict_of(&[floor.clone(), c("5", 2_000, "tidy", "", &["x = 1 + 2"], &["y = 3 + 4"])]).is_null());
        assert!(conflict_of(&[c("1", 1_000, "money: rounding down", "", &["return int(amount * 100)"], &[]), c("2", 2_000, "half up", "", &["z"], &["return int(amount * 100)"])]).is_null());
        assert_eq!(reason_of("money: round half up", "Support ticket #41: customers complained.").as_deref(), Some("money: round half up: Support ticket #41: customers complained"));
        assert_eq!(reason_of("rename helpers", "Tidy."), None);
    }

    #[test]
    fn answers_with_the_commit_and_the_whole_history() {
        let r = read("when did the rounding change?", None);
        let ns = needles(&Reading { subjects: vec!["round".into()], ..r.clone() });
        let commit = |id: &str, ms: i64, subject: &str| json!({ "id": id, "kind": "commit", "ts_ms": ms, "change": "added", "by": "Dana", "agent": null, "commit": { "short": id, "subject": subject }, "lines": [{ "path": "money.py", "sign": "+", "text": "x" }] });
        let items = vec![
            commit("4e51a20", 1_000, "rounding down"),
            commit("9355961", 2_000, "half-up for ticket 41"),
            commit("b72f105", 3_000, "half-even for the ledger"),
            // A later uncommitted edit, and a decision of no one in particular.
            json!({ "id": "w-a-1", "kind": "edit", "ts_ms": 4_000, "change": "added", "agent": { "id": "a", "name": "rounding-doc" }, "lines": [{ "path": "money.py", "sign": "+", "text": "x" }] }),
            json!({ "id": "d-x", "kind": "decision", "ts_ms": 2_900, "title": "Leave test_promo_window alone", "agent": null }),
        ];
        let a = answer_of(&r, &ns, &items).unwrap();
        assert_eq!(a["key"], "b72f105", "the newest commit, not the edit: {a}");
        let text = a["text"].as_str().unwrap();
        assert!(text.contains("commit b72f105") && text.contains("Since then rounding-doc has an uncommitted edit") && !text.contains("following") && !text.contains("promo"), "{text}");
        assert_eq!(a["history"].as_array().unwrap().len(), 3);
        assert!(a["history_plain"].as_str().unwrap().contains("changed 3 times: rounding down 4e51a20"), "{a}");
    }

    #[test]
    fn reads_quotes_arrows_identifiers_files_and_places() {
        let r = read("why did `retries` go 3 -> 5 in api/ ?", None);
        assert_eq!((r.x.as_deref(), r.y.as_deref()), (Some("3"), Some("5")));
        assert_eq!(r.subjects, ["retries"]);
        assert_eq!(r.paths, ["api/"]);
        let ns = needles(&r);
        assert_eq!(ns[0].pre.as_deref(), Some("retries"));
        assert!(ns[1].finds("    retries: 5,") && !ns[1].finds("timeout: 5"));
        assert!(ere("half-up").contains("[hH][aA][lL][fF][-_ ]?[uU][pP]"));

        let r = read("who changed api/src/money.rs and why did cart_total start taking discounts?", None);
        assert_eq!(r.files, ["api/src/money.rs"]);
        assert_eq!(r.subjects, ["cart_total"]);
        assert!(r.kinds.contains(&"who".to_string()) && r.kinds.contains(&"why".to_string()));

        let r = read("when did we change ROUND to \"half_up\"", None);
        assert_eq!((r.x.as_deref(), r.y.as_deref()), (None, Some("half_up")));
        assert_eq!(r.subjects, ["ROUND"]);
        let r = read("why is the debounce 250ms → 150ms in web/src", None);
        assert_eq!((r.x.as_deref(), r.y.as_deref()), (Some("250ms"), Some("150ms")));
        assert_eq!(r.paths, ["web/src"]);
        // "how to" is not a change.
        let r = read("how to round totals?", None);
        assert_eq!(r.y, None);
    }

    #[test]
    fn the_search_parts_tables_answer_its_queries() {
        let dir = scratch_dir();
        let db = crate::db::StateDb::open(&dir).unwrap();
        // As the Search part makes them (SPEC.md, rows 1 and 10).
        db.query("CREATE TABLE IF NOT EXISTS code_chunks (id KEYWORD PRIMARY KEY, repo KEYWORD, tree KEYWORD, path KEYWORD, lang KEYWORD, symbol KEYWORD, kind KEYWORD, start_line INT, end_line INT, text TEXT OPTIONS(analyzer = 'simple'), terms TEXT OPTIONS(analyzer = 'simple'), head TEXT OPTIONS(analyzer = 'simple'))", &Params::new()).unwrap();
        db.query("CREATE TABLE IF NOT EXISTS doc_pages (id KEYWORD PRIMARY KEY, lib KEYWORD, repo KEYWORD, url KEYWORD, title TEXT, text TEXT, words INT, depth INT, fetched TIMESTAMP)", &Params::new()).unwrap();
        db.query("INSERT INTO code_chunks (id, repo, tree, path, symbol, start_line, end_line, text) VALUES ('a', '/r', 'main', 'api/money.rs', 'round_half_up', 10, 12, 'pub fn round_half_up(n: i64) -> i64 { n }'), ('b', '/r', 'main', 'api/other.rs', 'other', 1, 1, 'fn other() {}')", &Params::new()).unwrap();
        db.query("INSERT INTO doc_pages (id, lib, repo, url, title, text) VALUES ('d', 'rust_decimal', '/r', 'https://docs.rs/x', 'RoundingStrategy', 'MidpointAwayFromZero rounds half up away from zero')", &Params::new()).unwrap();
        let p = Params::new().set("repo", Value::Str("/r".into())).set("q", Value::Str(sql_text(&["half-up".to_string()])));
        let rows = db.query(&chunks_sql(1200), &p).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(get_str(&rows[0][0]).as_deref(), Some("api/money.rs"));
        assert!(get_str(&rows[0][4]).unwrap().contains("round_half_up"));
        let docs = db.query(DOCS_SQL, &p).unwrap();
        assert_eq!(docs.len(), 1, "{docs:?}");
        db.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("wb-trace-{}-{}", std::process::id(), crate::supervisor::hex(8)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finds_the_definition_around_a_line() {
        let src = "use x;\n\n/// Rounds.\npub fn round_half_up(n: i64) -> i64 {\n    if n > 0 {\n        n + 1\n    } else {\n        n\n    }\n}\n\nfn other() {}\n";
        let lines: Vec<&str> = src.lines().collect();
        let (s, e, n) = around(&lines, 5);
        assert_eq!((s, e, n.as_deref()), (2, 9, Some("round_half_up")));
        let py = "def total(xs):\n    t = 0\n    for x in xs:\n        t += x\n    return t\n\ndef other():\n    pass\n";
        let lines: Vec<&str> = py.lines().collect();
        assert_eq!(around(&lines, 3), (0, 4, Some("total".into())));
        assert_eq!(def_name("export const cartTotal = (lines) => {"), Some("cartTotal".into()));
        let money = "/// Cents.\npub type Cents = i64;\n\n/// Rounded half up.\npub fn round_half_up(n: i64) -> i64 {\n    n\n}\n";
        let lines: Vec<&str> = money.lines().collect();
        assert_eq!(around(&lines, 3), (3, 6, Some("round_half_up".into())));
        assert_eq!(def_name("impl<T> Cart for Shop {"), Some("Cart".into()));
    }
}
