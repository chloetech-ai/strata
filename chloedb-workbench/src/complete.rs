//! Autocomplete and intent checks (SPEC.md, "Fifth build: Autocomplete").
//!
//! In an IDE for code the unit of edit is code: the editor completes
//! identifiers and checks the syntax. In an IDE of agents the unit is the
//! task, an intent: the box completes references into the project's memory
//! (agents, files, symbols, tests, ChloeCtx's decisions, the user's rules,
//! earlier tasks with how they ended, SQL tables and columns, personas, doc
//! pages), and its syntax check is an intent check: a task with no done
//! check, one against a decision or a rule, one that reaches a guarded
//! command, one that overlaps a running agent's task or files, a file that
//! is a hotspot.
//!
//! Completion reads ChloeDB: `complete_items` holds one row per thing to
//! complete (its keys lowercased in a multi-valued KEYWORD field, so
//! `PREFIX(pfx, :p)` finds `cart_total` from `tot` and `total.rs` from
//! `tot`, with typos as a fallback through `fuzzy => 'auto'`), refreshed
//! in the background from the tables it mirrors (code_chunks, test_runs,
//! rules, doc_pages) and from `chloectx sql --json`; agents come from the
//! `agents` table by `PREFIX(name)`, and earlier tasks and goals by BM25
//! over their text. Nothing here calls a model.
//!
//! Offsets in and out are UTF-16 code units, as a browser's
//! `selectionStart` counts them.
//!
//! `GET /api/complete?field&q&cursor&repo&agent&force`,
//! `POST /api/intent/check {text, repo?, field?, agent?}`.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::db::{get_int, get_ms, get_str, iso, now_ms, StateDb};
use crate::server::{blocking, parse_body, ApiResult, App};
use crate::supervisor::{ApiError, Supervisor};
use crate::trail::{self, Ran};

/// Proposals, the ghost sentence, "because" lines, habits, the model ghost.
pub mod foresee;

/// The most suggestions answered, and of one kind.
const MAX_ITEMS: usize = 12;
const MAX_PER_KIND: usize = 5;
/// A repository's rows are refreshed in the background when this old.
const REFRESH: Duration = Duration::from_secs(3);
/// ChloeCtx is asked again when its records are this old.
const CTX_AGAIN: Duration = Duration::from_secs(60);
/// The kinds of ChloeCtx's records, and the user's rules: the ◆ kinds.
const CTX_KINDS: [&str; 4] = ["decision", "preference", "fact", "rule"];
/// The repository column of rows every repository shares (SQL, personas).
const ALL: &str = "*";

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/complete", get(complete_route))
        .route("/api/intent/check", post(check_route))
        .route("/api/complete/events", post(events_route))
        .route("/api/complete/stats", get(stats_route))
        .route("/api/complete/ghost", post(ghost_route))
}

// ------------------------------------------------------------- text

/// The byte index of UTF-16 offset `n` in `s` (the end when past it).
pub fn byte_at(s: &str, n: usize) -> usize {
    let mut u = 0;
    for (b, c) in s.char_indices() {
        if u >= n {
            return b;
        }
        u += c.len_utf16();
    }
    s.len()
}

/// The UTF-16 offset of byte index `b` in `s`.
pub fn u16_at(s: &str, b: usize) -> usize {
    let mut b = b.min(s.len());
    while !s.is_char_boundary(b) {
        b -= 1;
    }
    s[..b].encode_utf16().count()
}

const TRIGGERS: [char; 3] = ['@', '◆', '#'];

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '-')
}

/// The word at a cursor (bytes): a trigger (@ ◆ #) and what follows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub trigger: Option<char>,
}

impl Token {
    /// What follows the trigger, lowercased: what the prefix is.
    fn bare(&self) -> String {
        self.text.trim_start_matches(TRIGGERS).to_lowercase()
    }
}

pub fn token_at(text: &str, cursor: usize) -> Token {
    let cursor = cursor.min(text.len());
    let before = &text[..cursor];
    let mut start = cursor;
    for (i, c) in before.char_indices().rev() {
        if word_char(c) {
            start = i;
            continue;
        }
        if TRIGGERS.contains(&c) {
            start = i;
        }
        break;
    }
    let t = &text[start..cursor];
    Token { text: t.to_string(), start, end: cursor, trigger: t.chars().next().filter(|c| TRIGGERS.contains(c)) }
}

const STOP: [&str; 58] = [
    "the", "and", "for", "with", "not", "but", "you", "your", "this", "that", "from", "into", "then", "than", "are", "was", "its", "all", "any", "one", "use",
    "make", "sure", "when", "what", "which", "have", "has", "will", "should", "can", "also", "just", "each", "only", "more", "some", "our", "out", "them", "it's",
    "how", "why", "who", "did", "does", "doing", "done", "there", "their", "they", "were", "been", "being", "would", "could", "about", "like",
];

/// The words of a text worth matching, lowercased, with their byte ranges.
fn words(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        let w = c.is_alphanumeric() || c == '_' || (c == '\'' && start.is_some());
        match (w, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let word = text[s..i].trim_end_matches('\'').to_lowercase();
                if word.chars().count() >= 3 && !STOP.contains(&word.as_str()) {
                    out.push((word, s, s + text[s..i].trim_end_matches('\'').len()));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// A word's crude stem, so "rounds", "rounding" and "rounded" meet "round".
fn stem(w: &str) -> String {
    let w = w.to_lowercase();
    for suf in ["ing", "ed", "es", "s"] {
        if w.len() > suf.len() + 3 && w.ends_with(suf) && !w.ends_with("ss") {
            return w[..w.len() - suf.len()].to_string();
        }
    }
    w
}

fn split_keys(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |k: String| {
        if !k.is_empty() && !out.contains(&k) {
            out.push(k);
        }
    };
    push(s.to_lowercase());
    for part in s.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if part.is_empty() {
            continue;
        }
        push(part.to_lowercase());
        for p in crate::search::split_ident(part) {
            push(p);
        }
    }
    out
}

fn q(sup: &Supervisor, sql: &str, p: &Params) -> Result<Vec<Vec<Value>>, String> {
    sup.db().query(sql, p)
}

fn sv(s: &str) -> Value {
    Value::Str(s.to_string())
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn hash_of(parts: impl IntoIterator<Item = String>) -> String {
    let mut h = DefaultHasher::new();
    let mut n = 0;
    for p in parts {
        p.hash(&mut h);
        n += 1;
    }
    format!("{n}:{:x}", h.finish())
}

fn float(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        v => get_int(v).unwrap_or(0) as f64,
    }
}

fn list(v: &Value) -> Vec<String> {
    match v {
        Value::List(l) => l.iter().filter_map(get_str).collect(),
        v => get_str(v).into_iter().collect(),
    }
}

// ------------------------------------------------------------- the rows

/// One thing to complete, as a row of `complete_items`.
#[derive(Clone, Debug)]
struct Item {
    id: String,
    kind: &'static str,
    label: String,
    ins: String,
    detail: String,
    refj: Json,
    body: String,
    weight: f64,
    pfx: Vec<String>,
    grp: String,
}

impl Item {
    fn new(kind: &'static str, key: &str, label: impl Into<String>) -> Item {
        let label = label.into();
        Item { id: key.to_string(), kind, ins: label.clone(), label, detail: String::new(), refj: Json::Null, body: String::new(), weight: 0.5, pfx: Vec::new(), grp: String::new() }
    }
}

/// Replaces a repository's rows of `kinds` with `items`.
fn replace(sup: &Supervisor, repo: &str, kinds: &[&str], items: &[Item]) -> Result<(), String> {
    for k in kinds {
        q(sup, "DELETE FROM complete_items WHERE repo = :repo AND kind = :kind", &Params::new().set("repo", sv(repo)).set("kind", sv(k)))?;
    }
    let mut seen = HashSet::new();
    let items: Vec<&Item> = items.iter().filter(|it| seen.insert(format!("{}|{}", it.kind, it.id))).collect();
    for batch in items.chunks(64) {
        let mut p = Params::new().set("repo", sv(repo));
        let mut values = Vec::new();
        for (i, it) in batch.iter().enumerate() {
            let mut pfx: Vec<String> = it.pfx.iter().filter(|k| !k.is_empty()).take(40).cloned().collect();
            pfx.dedup();
            p = p
                .set(&format!("id{i}"), Value::Str(format!("{repo}|{}|{}", it.kind, it.id)))
                .set(&format!("k{i}"), sv(it.kind))
                .set(&format!("l{i}"), sv(&it.label))
                .set(&format!("n{i}"), sv(&it.ins))
                .set(&format!("d{i}"), sv(&it.detail))
                .set(&format!("r{i}"), Value::Str(it.refj.to_string()))
                .set(&format!("b{i}"), Value::Str(if it.body.is_empty() { it.label.clone() } else { it.body.clone() }))
                .set(&format!("w{i}"), Value::Float(it.weight))
                .set(&format!("p{i}"), Value::List(pfx.into_iter().map(Value::Str).collect()))
                .set(&format!("g{i}"), sv(&it.grp));
            values.push(format!("(:id{i}, :repo, :k{i}, :p{i}, :l{i}, :n{i}, :d{i}, :r{i}, :b{i}, :w{i}, :g{i})"));
        }
        let sql = format!("INSERT OR REPLACE INTO complete_items (id, repo, kind, pfx, label, ins, detail, refj, body, weight, grp) VALUES {}", values.join(", "));
        q(sup, &sql, &p)?;
    }
    Ok(())
}

// ------------------------------------------------------------- ChloeCtx

/// A ChloeCtx record, or a rule of the user's, as the checks read it.
#[derive(Clone, Debug)]
pub struct Rec {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub ts_ms: Option<i64>,
    pub files: Vec<String>,
    pub by: Option<String>,
    pub superseded: bool,
}

impl Rec {
    fn to_json(&self) -> Json {
        json!({ "id": self.id, "kind": self.kind, "text": self.text, "ts": self.ts_ms.map(iso), "files": self.files, "by": self.by, "superseded": self.superseded })
    }
}

fn fetch_ctx(sup: &Supervisor, dir: &Path) -> Result<Vec<Rec>, String> {
    let sql = "SELECT id, kind, text, ts, files, session, tag, links FROM memory WHERE kind = 'decision' OR kind = 'preference' OR kind = 'fact' ORDER BY ts DESC LIMIT 500";
    let out = match trail::run_chloectx(&sup.cfg.chloectx_bin, dir, &["sql", "--json", sql], Duration::from_secs(20)) {
        Ran::Out(v) => v,
        Ran::Missing => return Err(trail::not_installed(&sup.cfg.chloectx_bin)),
        Ran::Failed(e) => return Err(e),
    };
    let cols: Vec<String> = out["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
    let sessions = trail::Sessions::load(sup);
    let mut recs = Vec::new();
    for row in out["rows"].as_array().into_iter().flatten().filter_map(Json::as_array) {
        let col = |name: &str| cols.iter().position(|c| c == name).and_then(|i| row.get(i).cloned()).unwrap_or(Json::Null);
        let kind = col("kind").as_str().unwrap_or("").to_string();
        if !matches!(kind.as_str(), "decision" | "preference" | "fact") {
            continue;
        }
        let id = col("id").as_str().map(str::to_string).unwrap_or_else(|| col("id").to_string());
        let links = col("links").to_string();
        let by = col("session").as_str().and_then(|s| sessions.agent(s)).or_else(|| sessions.of_record(&id)).map(|a| a.name.clone());
        recs.push(Rec {
            superseded: col("tag").as_str() == Some("superseded") || links.contains("superseded_by"),
            id,
            kind,
            text: col("text").as_str().unwrap_or("").trim().to_string(),
            ts_ms: trail::ctx_ms(&col("ts")),
            files: trail::files_of(&col("files")).into_iter().map(|f| crate::map::rel_path(&f, dir, dir).unwrap_or(f)).collect(),
            by,
        });
    }
    Ok(recs)
}

fn rules_of(sup: &Supervisor, repo: &str) -> Vec<Rec> {
    let rows = q(sup, "SELECT id, text, active, created, ctx_id FROM rules WHERE repo = :repo LIMIT 2000", &Params::new().set("repo", sv(repo))).unwrap_or_default();
    rows.iter()
        .filter(|r| get_int(&r[2]).unwrap_or(1) != 0)
        .map(|r| Rec {
            id: get_str(&r[0]).unwrap_or_default(),
            kind: "rule".into(),
            text: get_str(&r[1]).unwrap_or_default(),
            ts_ms: get_ms(&r[3]),
            files: Vec::new(),
            by: Some("you".into()),
            superseded: false,
        })
        .collect()
}

// ------------------------------------------------------------- freshness

#[derive(Default)]
struct RepoMem {
    sigs: HashMap<&'static str, String>,
    /// The tracked files, as of the last refresh.
    files: Arc<Vec<String>>,
    /// ChloeCtx's records and the rules, as of the last refresh.
    recs: Arc<Vec<Rec>>,
    ctx_at: Option<Instant>,
    ctx_missing: Option<String>,
    refreshed: Option<Instant>,
    busy: bool,
    code_asked: bool,
}

fn mem() -> &'static Mutex<HashMap<String, RepoMem>> {
    static M: OnceLock<Mutex<HashMap<String, RepoMem>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One refresh at a time writes the rows.
fn write_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Brings a repository's rows up to date: the first time before answering,
/// after that in the background once they are `REFRESH` old.
fn fresh(sup: &Arc<Supervisor>, repo: &str) {
    schema(sup);
    if repo.is_empty() {
        return;
    }
    let first = {
        let mut m = mem().lock().unwrap();
        let e = m.entry(repo.to_string()).or_default();
        if e.busy || e.refreshed.is_some_and(|t| t.elapsed() < REFRESH) {
            return;
        }
        e.busy = true;
        e.refreshed.is_none()
    };
    if first {
        refresh(sup, repo);
    } else {
        let (sup, repo) = (sup.clone(), repo.to_string());
        let _ = std::thread::Builder::new().name("complete".into()).spawn(move || refresh(&sup, &repo));
    }
}

fn sig_of(sup: &Supervisor, sql: &str, repo: &str) -> String {
    match q(sup, sql, &Params::new().set("repo", sv(repo))) {
        Ok(rows) => rows.first().map(|r| r.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join("|")).unwrap_or_default(),
        Err(e) => format!("error {e}"),
    }
}

/// Whether the signature of a source changed since its rows were written.
fn changed(repo: &str, source: &'static str, sig: &str) -> bool {
    let m = mem().lock().unwrap();
    m.get(repo).and_then(|e| e.sigs.get(source)).map(|s| s != sig).unwrap_or(true)
}

fn set_sig(repo: &str, source: &'static str, sig: String) {
    if let Some(e) = mem().lock().unwrap().get_mut(repo) {
        e.sigs.insert(source, sig);
    }
}

fn refresh(sup: &Arc<Supervisor>, repo: &str) {
    let main = PathBuf::from(repo);
    // ChloeCtx first (it is slow): outside the write lock.
    let ask_ctx = mem().lock().unwrap().get(repo).map(|e| e.ctx_at.is_none_or(|t| t.elapsed() > CTX_AGAIN)).unwrap_or(true);
    let ctx = ask_ctx.then(|| fetch_ctx(sup, &main));
    let prompts = if matches!(ctx, Some(Ok(_))) { foresee::fetch_prompts(sup, &main) } else { None };
    let _w = write_lock().lock().unwrap();
    if let Some(items) = prompts {
        let _ = replace(sup, repo, &["prompt"], &items);
    }
    // Files: what git tracks, and the new files it does not ignore.
    let files = crate::search::list_files(&main).unwrap_or_default();
    let sig = hash_of(files.iter().cloned());
    if changed(repo, "file", &sig) {
        let lines: HashMap<String, i64> = q(sup, "SELECT path, lines FROM code_files WHERE repo = :repo AND tree = 'main' LIMIT 100000", &Params::new().set("repo", sv(repo)))
            .unwrap_or_default()
            .iter()
            .filter_map(|r| Some((get_str(&r[0])?, get_int(&r[1]).unwrap_or(0))))
            .collect();
        let items: Vec<Item> = files
            .iter()
            .map(|f| {
                let base = f.rsplit('/').next().unwrap_or(f);
                let stem_ = base.split('.').next().unwrap_or(base);
                let mut it = Item::new("file", f, f.clone());
                it.ins = format!("{f} ");
                it.pfx = vec![base.to_lowercase(), f.to_lowercase(), stem_.to_lowercase()];
                it.pfx.extend(f.split('/').map(str::to_lowercase));
                it.pfx.extend(split_keys(stem_));
                it.detail = lines.get(f).map(|n| format!("{n} lines")).unwrap_or_default();
                it.weight = 0.6 - (f.matches('/').count().min(6) as f64) * 0.05;
                it.refj = json!({ "path": f });
                it
            })
            .collect();
        if replace(sup, repo, &["file"], &items).is_ok() {
            set_sig(repo, "file", sig);
        }
    }
    if let Some(e) = mem().lock().unwrap().get_mut(repo) {
        e.files = Arc::new(files);
    }
    // Symbols: the code index's definitions (Search's tables), which the
    // first completion of a repository asks Search to build.
    let sig = sig_of(sup, "SELECT COUNT(*), MAX(indexed) FROM code_files WHERE repo = :repo AND tree = 'main'", repo);
    let none = sig.starts_with("Int(0)") || sig.is_empty();
    let ask_code = none && mem().lock().unwrap().get_mut(repo).map(|e| !std::mem::replace(&mut e.code_asked, true)).unwrap_or(false);
    if ask_code {
        let (sup2, repo2) = (sup.clone(), repo.to_string());
        let _ = std::thread::Builder::new().name("complete-code".into()).spawn(move || {
            let _ = crate::search::code(&sup2, crate::search::CodeQ { repo: Some(repo2), q: Some(String::new()), agent: None, lang: None, limit: Some(1), tokens: None, by: None });
        });
    }
    if !none && changed(repo, "symbol", &sig) {
        let rows = q(sup, "SELECT symbol, kind, path, start_line FROM code_chunks WHERE repo = :repo AND tree = 'main' AND symbol IS NOT NULL LIMIT 60000", &Params::new().set("repo", sv(repo))).unwrap_or_default();
        let mut items = Vec::new();
        for r in &rows {
            let (Some(sym), Some(path)) = (get_str(&r[0]), get_str(&r[2])) else { continue };
            if sym.is_empty() || sym.len() > 80 {
                continue;
            }
            let kind = get_str(&r[1]).unwrap_or_default();
            if kind == "heading" || kind == "section" {
                continue;
            }
            let line = get_int(&r[3]).unwrap_or(1);
            let mut it = Item::new("symbol", &format!("{path}#{sym}"), sym.clone());
            it.detail = format!("{kind} · {path}:{line}");
            it.pfx = split_keys(&sym);
            it.weight = if path.contains("test") { 0.4 } else { 0.6 };
            it.refj = json!({ "path": path, "line": line, "kind": kind });
            items.push(it);
        }
        if replace(sup, repo, &["symbol"], &items).is_ok() {
            set_sig(repo, "symbol", sig);
        }
    }
    // Tests: every test the agents ran (Health's test_runs).
    let sig = sig_of(sup, "SELECT COUNT(*), MAX(ts) FROM test_runs WHERE repo = :repo", repo);
    if changed(repo, "test", &sig) {
        let rows = q(sup, "SELECT test, outcome, command, ms, agent, ts FROM test_runs WHERE repo = :repo ORDER BY ts DESC LIMIT 20000", &Params::new().set("repo", sv(repo))).unwrap_or_default();
        let mut by: Vec<(String, (i64, i64, i64, String))> = Vec::new();
        // The newest run of each test (rows come newest first).
        let mut last: HashMap<String, (String, i64)> = HashMap::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for r in &rows {
            let Some(t) = get_str(&r[0]) else { continue };
            let i = *at.entry(t.clone()).or_insert_with(|| {
                by.push((t.clone(), (0, 0, 0, get_str(&r[2]).unwrap_or_default())));
                by.len() - 1
            });
            last.entry(t.clone()).or_insert_with(|| (get_str(&r[1]).unwrap_or_default(), get_ms(&r[5]).unwrap_or(0)));
            let e = &mut by[i].1;
            e.0 += 1;
            match get_str(&r[1]).as_deref() {
                Some("passed") => e.1 += 1,
                Some("failed") => e.2 += 1,
                _ => {}
            }
        }
        let items: Vec<Item> = by
            .iter()
            .map(|(t, (n, ok, bad, cmd))| {
                let short = t.rsplit(" › ").next().unwrap_or(t).to_string();
                let mut it = Item::new("test", t, t.clone());
                it.ins = short.clone();
                it.detail = format!("{ok} of {n} passed{}{}", if *bad > 0 && *ok > 0 { " · flaky or fixed" } else { "" }, if cmd.is_empty() { String::new() } else { format!(" · {cmd}") });
                it.pfx = split_keys(&short);
                for seg in short.split([':', '/', '.', ' ']).filter(|s| !s.is_empty()) {
                    it.pfx.push(seg.to_lowercase());
                }
                it.weight = 0.4 + (*ok as f64 / (*n).max(1) as f64) * 0.2;
                let (lo, lms) = last.get(t).cloned().unwrap_or_default();
                it.refj = json!({ "test": t, "runs": n, "passed": ok, "failed": bad, "command": cmd, "last": lo, "last_ms": lms });
                it
            })
            .collect();
        if replace(sup, repo, &["test"], &items).is_ok() {
            set_sig(repo, "test", sig);
        }
    }
    // Docs: the titles of the pages the Library crawled.
    let sig = sig_of(sup, "SELECT COUNT(*), MAX(fetched) FROM doc_pages WHERE repo = :repo", repo);
    if changed(repo, "doc", &sig) {
        let rows = q(sup, "SELECT url, title, lib FROM doc_pages WHERE repo = :repo LIMIT 3000", &Params::new().set("repo", sv(repo))).unwrap_or_default();
        let items: Vec<Item> = rows
            .iter()
            .filter_map(|r| {
                let (url, title) = (get_str(&r[0])?, get_str(&r[1]).unwrap_or_default());
                let mut it = Item::new("doc", &url, if title.is_empty() { url.clone() } else { title.clone() });
                it.ins = url.clone();
                it.detail = format!("{} · {}", get_str(&r[2]).unwrap_or_default(), url);
                it.pfx = split_keys(&title);
                it.refj = json!({ "url": url, "library": get_str(&r[2]) });
                it.weight = 0.3;
                Some(it)
            })
            .collect();
        if replace(sup, repo, &["doc"], &items).is_ok() {
            set_sig(repo, "doc", sig);
        }
    }
    // ◆: the rules and ChloeCtx's records.
    let rules = rules_of(sup, repo);
    let (mut recs, missing) = match ctx {
        Some(Ok(r)) => (r, None),
        Some(Err(e)) => (Vec::new(), Some(e)),
        None => (mem().lock().unwrap().get(repo).map(|e| e.recs.iter().filter(|r| r.kind != "rule").cloned().collect()).unwrap_or_default(), None),
    };
    let ctx_ids: HashSet<String> = q(sup, "SELECT ctx_id FROM rules WHERE repo = :repo LIMIT 2000", &Params::new().set("repo", sv(repo))).unwrap_or_default().iter().filter_map(|r| get_str(&r[0])).collect();
    recs.retain(|r| !ctx_ids.contains(&r.id));
    let mut all = rules;
    all.extend(recs);
    let sig = hash_of(all.iter().map(|r| format!("{}|{}|{}|{}", r.id, r.kind, r.superseded, r.text)));
    if changed(repo, "ctx", &sig) {
        let now = now_ms();
        let items: Vec<Item> = all
            .iter()
            .map(|r| {
                let kind: &'static str = match r.kind.as_str() {
                    "rule" => "rule",
                    "preference" => "preference",
                    "fact" => "fact",
                    _ => "decision",
                };
                let mut it = Item::new(kind, &r.id, r.text.clone());
                it.ins = format!("◆{} ", r.id);
                let place = r.files.first().cloned().unwrap_or_default();
                it.detail = [r.by.clone().unwrap_or_else(|| "another session".into()), if kind == "rule" { "every agent is told it".into() } else { place }, if r.superseded { "superseded".into() } else { String::new() }]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                it.pfx = vec![r.id.to_lowercase()];
                it.pfx.extend(words(&r.text).into_iter().map(|w| w.0));
                for f in &r.files {
                    it.pfx.push(f.rsplit('/').next().unwrap_or(f).to_lowercase());
                }
                it.body = format!("{} {}", r.text, r.files.join(" "));
                let age_days = r.ts_ms.map(|t| (now - t).max(0) as f64 / 86_400_000.0).unwrap_or(30.0);
                it.weight = if kind == "rule" { 1.0 } else if r.superseded { 0.1 } else { 0.5 + 0.4 / (1.0 + age_days / 7.0) };
                it.refj = r.to_json();
                it
            })
            .collect();
        if replace(sup, repo, &["rule", "decision", "preference", "fact"], &items).is_ok() {
            set_sig(repo, "ctx", sig);
        }
    }
    let mut m = mem().lock().unwrap();
    let e = m.entry(repo.to_string()).or_default();
    e.recs = Arc::new(all);
    if ask_ctx {
        e.ctx_at = Some(Instant::now());
        e.ctx_missing = missing;
    }
    e.refreshed = Some(Instant::now());
    e.busy = false;
}

/// The rows every repository shares: the state database's tables and
/// columns, words of its SQL, the personas.
fn schema(sup: &Supervisor) {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let mut items = Vec::new();
        for t in crate::alerts::TABLES {
            let Ok((cols, rows)) = sup.db().query_cols(&format!("SELECT * FROM {t} LIMIT 1"), &Params::new()) else { continue };
            let mut it = Item::new("table", t, t);
            it.ins = format!("{t} ");
            it.detail = format!("{} columns · {}", cols.len(), cols.iter().take(6).cloned().collect::<Vec<_>>().join(", "));
            it.pfx = vec![t.to_string()];
            it.pfx.extend(t.split('_').map(str::to_string));
            it.refj = json!({ "table": t, "columns": cols });
            it.weight = 0.8;
            items.push(it);
            for (i, c) in cols.iter().enumerate() {
                let ty = rows.first().and_then(|r| r.get(i)).map(|v| match v {
                    Value::Int(_) => "INT",
                    Value::Float(_) => "FLOAT",
                    Value::Bool(_) => "BOOL",
                    Value::Timestamp(_) => "TIMESTAMP",
                    Value::List(_) => "LIST",
                    Value::Str(_) => "TEXT",
                    _ => "",
                });
                let name = if c == "commit" { "\"commit\"".to_string() } else { c.clone() };
                let mut it = Item::new("column", &format!("{t}.{c}"), c.clone());
                it.ins = name;
                it.detail = match ty.filter(|t| !t.is_empty()) {
                    Some(ty) => format!("{t} · {ty}"),
                    None => t.to_string(),
                };
                it.grp = t.to_string();
                it.pfx = vec![c.to_lowercase()];
                it.refj = json!({ "table": t, "column": c });
                it.weight = 0.7 - i as f64 * 0.01;
                items.push(it);
            }
        }
        const KEYWORDS: [&str; 30] = [
            "SELECT", "FROM", "WHERE", "GROUP BY", "ORDER BY", "HAVING", "LIMIT", "AND", "OR", "NOT", "IN", "AS", "ON", "JOIN", "LEFT JOIN", "ASOF JOIN", "MATCH_CONDITION", "DESC", "ASC",
            "DISTINCT", "COUNT(*)", "CASE WHEN", "BETWEEN", "IS NULL", "IS NOT NULL", "LIKE", "WITH", ":since", ":now", ":repo",
        ];
        for (i, k) in KEYWORDS.iter().enumerate() {
            let mut it = Item::new("keyword", k, *k);
            it.ins = if k.ends_with(')') || k.starts_with(':') { k.to_string() } else { format!("{k} ") };
            it.detail = match *k {
                ":since" => "now less the rule's window".into(),
                ":now" => "the time of the check".into(),
                ":repo" => "the rule's repository".into(),
                "ASOF JOIN" => "the last row before, per key".into(),
                _ => String::new(),
            };
            it.pfx = vec![k.to_lowercase(), k.trim_start_matches(':').to_lowercase()];
            it.weight = 0.5 - i as f64 * 0.005;
            items.push(it);
        }
        const FUNCTIONS: [(&str, &str); 12] = [
            ("COUNT(", "rows, or distinct values"), ("SUM(", ""), ("AVG(", ""), ("MIN(", ""), ("MAX(", ""), ("approx_percentile(", "approx_percentile(ms, 0.99): p99"),
            ("date_bin(", "date_bin('5 minutes', ts): time buckets"), ("bm25(", "rank by BM25 over a TEXT column"), ("MATCH(", "full-text match on a TEXT column"),
            ("PREFIX(", "values that start with a prefix"), ("lower(", ""), ("coalesce(", ""),
        ];
        for (f, d) in FUNCTIONS {
            let mut it = Item::new("function", f, f);
            it.detail = d.to_string();
            it.pfx = vec![f.trim_end_matches('(').to_lowercase()];
            it.weight = 0.45;
            items.push(it);
        }
        for p in crate::swarm::personas().as_array().into_iter().flatten() {
            let (id, name, role) = (p["id"].as_str().unwrap_or(""), p["name"].as_str().unwrap_or(""), p["role"].as_str().unwrap_or(""));
            let mut it = Item::new("persona", id, name);
            it.ins = format!("like {name}, {role}, ");
            it.detail = role.to_string();
            it.pfx = split_keys(&format!("{id} {name} {role}"));
            it.refj = p.clone();
            items.push(it);
        }
        let _ = replace(sup, ALL, &["table", "column", "keyword", "function", "persona"], &items);
    });
}

// ------------------------------------------------------------- completing

/// The sources a field completes from, in their order.
fn order(field: &str) -> &'static [&'static str] {
    match field {
        "goal" => &["task", "goal", "rule", "decision", "preference", "file", "agent", "symbol"],
        "steer" => &["file", "symbol", "rule", "decision", "preference", "fact", "test", "agent"],
        "answer" => &["rule", "decision", "preference", "fact", "file", "symbol", "agent", "test"],
        "trace" => &["symbol", "file", "decision", "rule", "preference", "fact", "agent", "test"],
        "quick" => &["trace", "symbol", "file", "decision", "rule", "preference", "agent", "test", "doc"],
        "why" => &["trace", "decision", "rule", "preference", "fact", "symbol", "file", "agent"],
        "persona" => &["task", "persona", "file"],
        "change" => &["file", "rule", "decision", "preference", "agent", "test"],
        "search" => &["symbol", "file", "test", "doc"],
        "sql" => &["column", "table", "keyword", "function"],
        // task, and anything else
        _ => &["task", "agent", "file", "symbol", "rule", "decision", "preference", "fact", "test", "goal"],
    }
}

/// A suggestion before it is answered: its JSON, its score, its kind.
struct Sugg {
    kind: String,
    score: f64,
    json: Json,
}

#[derive(Deserialize)]
pub struct CompleteQ {
    pub field: Option<String>,
    pub q: Option<String>,
    pub cursor: Option<usize>,
    pub repo: Option<String>,
    pub agent: Option<String>,
    pub force: Option<String>,
    pub limit: Option<usize>,
    /// Who types (the browser's id): habits are theirs.
    pub user: Option<String>,
}

/// The repository of a request: `repo`, else the agent's; "" when neither.
fn repo_for(sup: &Supervisor, repo: Option<&str>, agent: Option<&str>) -> Result<String, ApiError> {
    if let Some(r) = repo.filter(|r| !r.trim().is_empty()) {
        return Ok(trail::repo_dir(Some(r.trim()))?.to_string_lossy().to_string());
    }
    Ok(agent.filter(|a| !a.is_empty()).and_then(|a| sup.agent_row(a)).map(|a| a.repo).unwrap_or_default())
}

/// How well a row's keys meet the prefix: 3 the whole key, 2.5 its label's
/// start, 2 its first key's start, 1.5 another key's start.
fn quality(label: &str, keys: &[String], p: &str) -> f64 {
    if p.is_empty() {
        return 1.0;
    }
    let l = label.to_lowercase();
    if l == p || keys.iter().any(|k| k == p) {
        3.0
    } else if l.starts_with(p) {
        2.5
    } else if keys.first().is_some_and(|k| k.starts_with(p)) {
        2.0
    } else {
        1.5
    }
}

pub fn complete(sup: &Arc<Supervisor>, cq: CompleteQ) -> Result<Json, ApiError> {
    let started = Instant::now();
    let field = cq.field.clone().filter(|f| !f.is_empty()).unwrap_or_else(|| "task".into());
    let text = cq.q.clone().unwrap_or_default();
    let cursor = byte_at(&text, cq.cursor.unwrap_or(usize::MAX));
    let repo = repo_for(sup, cq.repo.as_deref(), cq.agent.as_deref())?;
    let force = cq.force.as_deref().is_some_and(|f| f == "1" || f == "true");
    let limit = cq.limit.unwrap_or(MAX_ITEMS).clamp(1, 30);
    fresh(sup, &repo);
    let tok = token_at(&text, cursor);
    let mut missing: Vec<String> = Vec::new();
    if repo.is_empty() && field != "sql" {
        missing.push("No repository: name one, or start an agent, to complete its files, decisions and tasks.".into());
    } else if let Some(m) = mem().lock().unwrap().get(&repo).and_then(|e| e.ctx_missing.clone()) {
        missing.push(m);
    }
    let user = foresee::user_of(cq.user.as_deref());
    let agent_row = cq.agent.as_deref().filter(|a| !a.is_empty()).and_then(|a| sup.agent_row(a));
    // An empty box proposes what to write.
    if text.trim().is_empty() && field != "sql" && !force && !repo.is_empty() {
        let (props, context) = foresee::proposals(sup, &user, &repo, &field, agent_row.as_ref(), 3);
        let suggestions: Vec<Json> = props
            .into_iter()
            .map(|mut s| {
                s.json["replace"] = json!({ "start": 0, "end": u16_at(&text, text.len()) });
                s.json["score"] = json!((s.score * 100.0).round() / 100.0);
                s.json
            })
            .collect();
        return Ok(json!({
            "field": field, "repo": repo, "user": user, "mode": "propose",
            "token": { "text": "", "start": 0, "end": 0, "trigger": null },
            "suggestions": suggestions, "ghost": null, "context": context,
            "took_ms": (started.elapsed().as_secs_f64() * 10_000.0).round() / 10.0,
            "missing": missing,
        }));
    }
    let mut out: Vec<Sugg> = if field == "sql" { complete_sql(sup, &text, &tok) } else { complete_text(sup, &repo, &field, &text, &tok, cq.agent.as_deref(), force, &user) };
    // This user's habits: what they picked here before ranks higher.
    if field != "sql" && !repo.is_empty() {
        let h = foresee::habits(sup, &user, &repo, &field);
        for s in out.iter_mut() {
            let (b, n) = h.boost(&s.kind, s.json["label"].as_str().unwrap_or(""));
            s.score += b;
            if n > 0 {
                s.json["picked"] = json!(n);
            }
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut per: HashMap<String, usize> = HashMap::new();
    let only_one_kind = out.iter().map(|s| s.kind.as_str()).collect::<HashSet<_>>().len() <= 1;
    out.retain(|s| {
        let n = per.entry(s.kind.clone()).or_insert(0);
        *n += 1;
        only_one_kind || *n <= MAX_PER_KIND
    });
    out.truncate(limit);
    // Why each is offered.
    if field != "sql" && !repo.is_empty() {
        let paths: Vec<String> = out.iter().filter_map(|s| if s.kind == "symbol" { s.json["ref"]["path"].as_str().map(str::to_string) } else { None }).collect();
        let eds = editors(sup, &repo, &paths, cq.agent.as_deref());
        for s in out.iter_mut() {
            foresee::because(sup, &mut s.json, &eds);
        }
    }
    // The rest of the sentence, when the cursor is at the end.
    let ghost = if cursor >= text.len() && field != "sql" { foresee::ghost(sup, &user, &repo, &field, &text, &tok, agent_row.as_ref(), out.first().map(|s| &s.json)) } else { None };
    let context = match (&agent_row, field.as_str()) {
        (Some(a), "steer" | "answer") => json!({ "agent": a.name, "id": a.id, "now": foresee::doing_now(sup, a) }),
        _ => Json::Null,
    };
    // Offsets for the browser: UTF-16.
    let conv = |r: &Json| json!({ "start": u16_at(&text, r["start"].as_u64().unwrap_or(0) as usize), "end": u16_at(&text, r["end"].as_u64().unwrap_or(0) as usize) });
    let suggestions: Vec<Json> = out
        .into_iter()
        .map(|mut s| {
            let r = conv(&s.json["replace"]);
            s.json["replace"] = r;
            s.json["score"] = json!((s.score * 100.0).round() / 100.0);
            s.json
        })
        .collect();
    Ok(json!({
        "field": field,
        "repo": repo,
        "token": { "text": tok.text, "start": u16_at(&text, tok.start), "end": u16_at(&text, tok.end), "trigger": tok.trigger.map(|c| c.to_string()) },
        "suggestions": suggestions,
        "ghost": ghost,
        "context": context,
        "user": user,
        "mode": "complete",
        "took_ms": (started.elapsed().as_secs_f64() * 10_000.0).round() / 10.0,
        "missing": missing,
    }))
}

fn sugg(kind: &str, score: f64, label: &str, ins: &str, detail: &str, refj: Json, start: usize, end: usize) -> Sugg {
    Sugg { kind: kind.to_string(), score, json: json!({ "kind": kind, "label": label, "insert": ins, "detail": detail, "ref": refj, "replace": { "start": start, "end": end } }) }
}

/// The rows of `complete_items` of `kinds` whose keys start with `p`.
fn items(sup: &Supervisor, repo: &str, kinds: &[&str], p: &str, extra: &str, fuzzy: bool) -> Vec<(String, String, String, String, Json, f64, Vec<String>, f64)> {
    if kinds.is_empty() {
        return Vec::new();
    }
    let ks: Vec<String> = kinds.iter().map(|k| lit(k)).collect();
    let cond = if fuzzy { "PREFIX(pfx, :p, fuzzy => 'auto')" } else { "PREFIX(pfx, :p)" };
    let sql = format!(
        "SELECT kind, label, ins, detail, refj, weight, pfx FROM complete_items WHERE repo IN (:repo, '{ALL}') AND kind IN ({}) AND {cond}{extra} ORDER BY weight DESC LIMIT 300",
        ks.join(", ")
    );
    let rows = q(sup, &sql, &Params::new().set("repo", sv(repo)).set("p", sv(p))).unwrap_or_default();
    rows.iter()
        .map(|r| {
            let label = get_str(&r[1]).unwrap_or_default();
            let keys = list(&r[6]);
            let qual = if fuzzy { 0.6 } else { quality(&label, &keys, p) };
            (
                get_str(&r[0]).unwrap_or_default(),
                label,
                get_str(&r[2]).unwrap_or_default(),
                get_str(&r[3]).unwrap_or_default(),
                get_str(&r[4]).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Json::Null),
                float(&r[5]),
                keys,
                qual,
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn complete_text(sup: &Arc<Supervisor>, repo: &str, field: &str, text: &str, tok: &Token, agent: Option<&str>, force: bool, user: &str) -> Vec<Sugg> {
    let ord = order(field);
    let bonus = |kind: &str| ord.iter().position(|k| *k == kind).map(|i| (ord.len() - i) as f64 * 0.8).unwrap_or(0.0);
    let p = tok.bare();
    let mut out = Vec::new();
    // A question goes to Trace, first.
    let trimmed = text.trim();
    if ord.contains(&"trace") && trimmed.split_whitespace().count() >= 2 {
        let first = trimmed.split_whitespace().next().unwrap_or("").to_lowercase();
        if matches!(first.as_str(), "when" | "why" | "how" | "who") || trimmed.to_lowercase().contains("chang") {
            let go = format!("#/trace?q={}", urlencode(trimmed));
            let mut s = sugg("trace", 100.0, &format!("Trace: {trimmed}"), "", "when, why and how it changed, with the evidence", json!({ "q": trimmed }), 0, text.len());
            s.json["go"] = json!(go);
            out.push(s);
        }
    }
    if repo.is_empty() {
        return out;
    }
    // Earlier tasks and goals like the one being typed (BM25), with how they ended.
    let typed = &text[..tok.end.min(text.len())];
    let ws = words(typed);
    if ws.len() >= 2 && (ord.contains(&"task") || ord.contains(&"goal")) && tok.trigger.is_none() {
        out.extend(past(sup, repo, field, text, typed, &ws, ord, &bonus));
    }
    // The user's own words: what they wrote in this box before.
    if ws.len() >= 2 && tok.trigger.is_none() && matches!(field, "task" | "goal" | "steer" | "answer" | "persona" | "change" | "why" | "trace" | "quick") {
        let (qs, need) = foresee::terms_of(typed);
        out.extend(foresee::mine(sup, user, repo, field, text, &qs, need));
    }
    if p.is_empty() && tok.trigger.is_none() && !force {
        return out;
    }
    if p.chars().count() < 2 && tok.trigger.is_none() && !force {
        return out;
    }
    let kinds: Vec<&str> = match tok.trigger {
        Some('@') => vec!["agent"],
        Some(_) => CTX_KINDS.to_vec(),
        None => ord.iter().copied().filter(|k| !matches!(*k, "task" | "goal" | "trace")).collect(),
    };
    // Agents: by name; for a plain word, the running ones whose task has it.
    if kinds.contains(&"agent") {
        let rows = q(sup, "SELECT id, name, state, task FROM agents WHERE repo = :repo AND PREFIX(name, :p) LIMIT 50", &Params::new().set("repo", sv(repo)).set("p", sv(&p))).unwrap_or_default();
        let mut seen = HashSet::new();
        for r in &rows {
            let (Some(id), Some(name)) = (get_str(&r[0]), get_str(&r[1])) else { continue };
            if Some(id.as_str()) == agent || !seen.insert(id.clone()) {
                continue;
            }
            let running = sup.state(&id).is_some_and(|(_, r)| r);
            let state = get_str(&r[2]).unwrap_or_default();
            let task = get_str(&r[3]).unwrap_or_default();
            let qual = quality(&name, &[name.to_lowercase()], &p);
            out.push(sugg("agent", qual * 10.0 + bonus("agent") + if running { 1.0 } else { 0.0 }, &format!("@{name}"), &format!("@{name} "), &format!("{} · {}", if running { state.as_str() } else { "exited" }, crate::supervisor::one_line(&task, 90)), json!({ "id": id, "name": name, "state": state, "running": running }), tok.start, tok.end));
        }
        if tok.trigger.is_none() && p.chars().count() >= 3 {
            let rows = q(sup, "SELECT id, name, state, task, score() FROM agents WHERE repo = :repo ORDER BY bm25(task, :q) DESC LIMIT 8", &Params::new().set("repo", sv(repo)).set("q", Value::Str(format!("{p}*")))).unwrap_or_default();
            for r in &rows {
                let (Some(id), Some(name)) = (get_str(&r[0]), get_str(&r[1])) else { continue };
                if float(&r[4]) <= 0.0 || seen.contains(&id) || Some(id.as_str()) == agent || !sup.state(&id).is_some_and(|(_, r)| r) {
                    continue;
                }
                seen.insert(id.clone());
                let state = get_str(&r[2]).unwrap_or_default();
                out.push(sugg("agent", 10.0 + bonus("agent"), &format!("@{name}"), &format!("@{name} "), &format!("{state} · {}", crate::supervisor::one_line(&get_str(&r[3]).unwrap_or_default(), 90)), json!({ "id": id, "name": name, "state": state, "running": true, "by_task": true }), tok.start, tok.end));
            }
        }
    }
    let rest: Vec<&str> = kinds.iter().copied().filter(|k| *k != "agent").collect();
    let mut rows = items(sup, repo, &rest, &p, "", false);
    if rows.len() < 4 && p.chars().count() >= 3 {
        let have: HashSet<String> = rows.iter().map(|r| format!("{}|{}", r.0, r.1)).collect();
        rows.extend(items(sup, repo, &rest, &p, "", true).into_iter().filter(|r| !have.contains(&format!("{}|{}", r.0, r.1))));
    }
    // Who edits each file now: for the detail of file rows.
    let file_paths: Vec<String> = rows.iter().filter(|r| r.0 == "file").map(|r| r.1.clone()).take(60).collect();
    let editors = editors(sup, repo, &file_paths, agent);
    for (kind, label, ins, detail, refj, weight, _keys, qual) in rows {
        let edited = if kind == "file" { editors.get(&label).map(|(name, ago)| format!("{name} edited it {ago}")) } else { None };
        // A superseded record still completes, below every live one.
        let stale = if refj["superseded"] == true { 25.0 } else { 0.0 };
        let mut s = sugg(&kind, qual * 10.0 + bonus(&kind) + weight - stale, &label, &ins, &detail, refj, tok.start, tok.end);
        if let Some(e) = edited {
            s.json["because"] = json!(e);
        }
        out.push(s);
    }
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn ago(ms: i64) -> String {
    let s = ((now_ms() - ms).max(0) / 1000) as u64;
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=172_799 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

/// The running agents (other than `me`) that edited each path lately: its name and when.
fn editors(sup: &Supervisor, repo: &str, paths: &[String], me: Option<&str>) -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    if paths.is_empty() {
        return out;
    }
    let inl: Vec<String> = paths.iter().map(|p| lit(p)).collect();
    let since = now_ms() - 24 * 3600 * 1000;
    let sql = format!("SELECT agent, file, ts FROM file_edits WHERE repo = :repo AND file IN ({}) AND ts >= :since ORDER BY ts DESC LIMIT 500", inl.join(", "));
    for r in q(sup, &sql, &Params::new().set("repo", sv(repo)).set("since", crate::db::ts(since))).unwrap_or_default() {
        let (Some(a), Some(f)) = (get_str(&r[0]), get_str(&r[1])) else { continue };
        if Some(a.as_str()) == me || out.contains_key(&f) || !sup.state(&a).is_some_and(|(_, r)| r) {
            continue;
        }
        if let Some(row) = sup.agent_row(&a) {
            out.insert(f, (row.name, ago(get_ms(&r[2]).unwrap_or(0))));
        }
    }
    out
}

/// How an agent's task ended: done, running, finished, stopped (and why), with a line.
fn outcome_of(sup: &Supervisor, a: &crate::db::Agent) -> (String, String) {
    let running = sup.state(&a.id).is_some_and(|(_, r)| r);
    let by_outcome = a.outcome.as_deref().and_then(|o| crate::goals::outcome(sup, o).ok()).map(|o| o.state);
    let feat = q(sup, "SELECT done, minutes, model FROM task_features WHERE id = :id", &Params::new().set("id", sv(&a.id))).unwrap_or_default();
    let done = feat.first().map(|r| get_int(&r[0]).unwrap_or(0) != 0);
    let minutes = feat.first().map(|r| float(&r[1])).unwrap_or_else(|| ((a.ended.unwrap_or_else(now_ms) - a.started_ms.unwrap_or(a.created)).max(0) as f64) / 60_000.0);
    let state = match (by_outcome.as_deref(), running, done) {
        (Some(s), _, _) => s.to_string(),
        (None, true, _) => "running".into(),
        (None, false, Some(true)) => "done".into(),
        (None, false, _) if a.exit_code == Some(0) => "finished".into(),
        _ => "stopped".into(),
    };
    let tests = q(sup, "SELECT outcome FROM test_runs WHERE agent = :id LIMIT 5000", &Params::new().set("id", sv(&a.id))).unwrap_or_default();
    let (ok, bad) = tests.iter().fold((0, 0), |(o, b), r| match get_str(&r[0]).as_deref() {
        Some("passed") => (o + 1, b),
        Some("failed") => (o, b + 1),
        _ => (o, b),
    });
    let mut parts = vec![a.name.clone(), state.replace('_', " ")];
    if minutes >= 0.5 {
        parts.push(format!("{} min", minutes.round() as i64));
    }
    if let Some(m) = a.model.as_deref().filter(|m| !m.is_empty()) {
        parts.push(m.to_string());
    }
    if ok + bad > 0 {
        parts.push(if bad > 0 { format!("tests: {ok} passed, {bad} failed") } else { format!("tests: {ok} passed") });
    }
    (state, parts.join(" · "))
}

/// Earlier tasks (agents', persona runs') and goals like the text, ranked by
/// BM25 over their words, the last one typed as a prefix.
#[allow(clippy::too_many_arguments)]
fn past(sup: &Supervisor, repo: &str, field: &str, text: &str, typed: &str, ws: &[(String, usize, usize)], ord: &[&str], bonus: &dyn Fn(&str) -> f64) -> Vec<Sugg> {
    let mut out = Vec::new();
    let mut terms: Vec<String> = ws.iter().map(|w| w.0.clone()).collect();
    if typed.chars().last().is_some_and(|c| c.is_alphanumeric()) {
        if let Some(l) = terms.last_mut() {
            // The index stems its words ("rounding" is "round"): a prefix
            // longer than the stem would miss it, so it gives up its tail.
            let n = l.chars().count();
            if n > 4 {
                *l = l.chars().take((n - 2).max(4)).collect();
            }
            l.push('*');
        }
    }
    let need = terms.len().min(2);
    let qs = terms.join(" ");
    let p = Params::new().set("repo", sv(repo)).set("q", Value::Str(qs));
    let typed_l = text.trim().to_lowercase();
    if field == "persona" {
        let sql = format!("SELECT id, task, outcome, state, score() FROM swarm_runs WHERE repo = :repo ORDER BY bm25(task, :q, min => {need}) DESC LIMIT 6");
        let mut seen = HashSet::new();
        for r in q(sup, &sql, &p).unwrap_or_default() {
            let task = get_str(&r[1]).unwrap_or_default();
            if float(&r[4]) <= 0.0 || task.trim().to_lowercase() == typed_l || !seen.insert(task.clone()) {
                continue;
            }
            let outcome = get_str(&r[2]).or_else(|| get_str(&r[3])).unwrap_or_default();
            out.push(sugg("task", 22.0 + bonus("task") + float(&r[4]).min(5.0) * 0.1, &task, &task, &format!("an earlier persona run · {}", outcome.replace('_', " ")), json!({ "run": get_str(&r[0]), "outcome": outcome }), 0, text.len()));
        }
        return out;
    }
    if ord.contains(&"task") {
        let sql = format!("SELECT id, score() FROM agents WHERE repo = :repo ORDER BY bm25(task, :q, min => {need}) DESC LIMIT 8");
        let mut seen = HashSet::new();
        for r in q(sup, &sql, &p).unwrap_or_default() {
            let Some(a) = get_str(&r[0]).and_then(|id| sup.agent_row(&id)) else { continue };
            let task = a.task.clone().unwrap_or_default();
            if float(&r[1]) <= 0.0 || task.trim().is_empty() || task.trim().to_lowercase() == typed_l || !seen.insert(task.trim().to_lowercase()) {
                continue;
            }
            let (state, detail) = outcome_of(sup, &a);
            out.push(sugg("task", 22.0 + bonus("task") + float(&r[1]).min(5.0) * 0.1, &crate::supervisor::one_line(&task, 200), &task, &detail, json!({ "agent": a.id, "name": a.name, "outcome": state }), 0, text.len()));
        }
    }
    if ord.contains(&"goal") {
        let sql = format!("SELECT id, text, state, score() FROM goals WHERE repo = :repo ORDER BY bm25(text, :q, min => {need}) DESC LIMIT 4");
        for r in q(sup, &sql, &p).unwrap_or_default() {
            let g = get_str(&r[1]).unwrap_or_default();
            if float(&r[3]) <= 0.0 || g.trim().to_lowercase() == typed_l {
                continue;
            }
            let state = get_str(&r[2]).unwrap_or_default();
            out.push(sugg("goal", 21.0 + bonus("goal"), &g, &g, &format!("a goal · {state}"), json!({ "id": get_str(&r[0]), "outcome": state }), 0, text.len()));
        }
    }
    out
}

// ------------------------------------------------------------- SQL

const SQL_WORDS: [&str; 22] = [
    "select", "from", "where", "group", "order", "by", "having", "limit", "join", "left", "inner", "asof", "on", "as", "and", "or", "not", "in", "with", "desc", "asc", "match_condition",
];

/// The tables a statement names, with their aliases.
fn tables_named(sql: &str) -> Vec<(String, Option<String>)> {
    let toks: Vec<String> = sql.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')).filter(|t| !t.is_empty()).map(str::to_string).collect();
    let mut out = Vec::new();
    for i in 0..toks.len() {
        if matches!(toks[i].to_lowercase().as_str(), "from" | "join") {
            if let Some(t) = toks.get(i + 1) {
                let mut alias = toks.get(i + 2).cloned();
                if alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case("as")) {
                    alias = toks.get(i + 3).cloned();
                }
                let alias = alias.filter(|a| !SQL_WORDS.contains(&a.to_lowercase().as_str()));
                out.push((t.to_lowercase(), alias));
            }
        }
    }
    out
}

fn complete_sql(sup: &Supervisor, text: &str, tok: &Token) -> Vec<Sugg> {
    let before = &text[..tok.start];
    let prev = before.split(|c: char| c.is_whitespace() || c == '(' || c == ',').rev().find(|w| !w.is_empty()).unwrap_or("").to_lowercase();
    let named = tables_named(text);
    let mut out = Vec::new();
    let t = tok.text.as_str();
    // `alias.col`: that table's columns.
    if let Some((alias, col)) = t.rsplit_once('.') {
        let table = named.iter().find(|(tb, al)| al.as_deref() == Some(alias) || tb == &alias.to_lowercase()).map(|(tb, _)| tb.clone()).unwrap_or_else(|| alias.to_lowercase());
        for (kind, label, ins, detail, refj, weight, keys, _) in items(sup, ALL, &["column"], &col.to_lowercase(), &format!(" AND grp = {}", lit(&table)), false) {
            let qual = quality(&label, &keys, &col.to_lowercase());
            out.push(sugg(&kind, qual * 10.0 + weight, &label, &format!("{alias}.{ins}"), &detail, refj, tok.start, tok.end));
        }
        return out;
    }
    let p = t.to_lowercase();
    if matches!(prev.as_str(), "from" | "join") {
        for (kind, label, ins, detail, refj, weight, keys, _) in items(sup, ALL, &["table"], &p, "", false) {
            out.push(sugg(&kind, quality(&label, &keys, &p) * 10.0 + weight, &label, &ins, &detail, refj, tok.start, tok.end));
        }
        return out;
    }
    if p.is_empty() {
        return out;
    }
    let tabs: Vec<String> = named.iter().map(|(t, _)| lit(t)).collect();
    if !tabs.is_empty() {
        for (kind, label, ins, detail, refj, weight, keys, _) in items(sup, ALL, &["column"], &p, &format!(" AND grp IN ({})", tabs.join(", ")), false) {
            out.push(sugg(&kind, quality(&label, &keys, &p) * 10.0 + 2.0 + weight, &label, &ins, &detail, refj, tok.start, tok.end));
        }
    }
    for (kind, label, ins, detail, refj, weight, keys, _) in items(sup, ALL, &["keyword", "function", "table"], &p, "", false) {
        out.push(sugg(&kind, quality(&label, &keys, &p) * 10.0 + weight, &label, &ins, &detail, refj, tok.start, tok.end));
    }
    out
}

// ------------------------------------------------------------- checks

/// A mark on the text (bytes until answered).
struct Mark {
    start: usize,
    end: usize,
    severity: &'static str,
    kind: &'static str,
    message: String,
    fix: Option<Json>,
    refj: Option<Json>,
}

/// The words that say how an agent knows it is done.
const DONE_WORDS: [&str; 34] = [
    "test", "tests", "testing", "pytest", "jest", "vitest", "passes", "pass", "passing", "green", "assert", "asserts", "expect", "expects", "verify", "verified", "until",
    "returns", "return", "shows", "show", "displays", "prints", "outputs", "responds", "reproduce", "reproduces", "benchmark", "screenshot", "exits", "succeeds", "fails",
    "check", "checks",
];

/// Groups of choices of which a decision picks one: rounding, numbers, case, …
const GROUPS: [&[&str]; 9] = [
    &["half up", "half-up", "half_up", "halfup", "half even", "half-even", "bankers", "floor", "ceil", "ceiling", "truncate", "round down", "round up"],
    &["float", "floats", "double", "integer cents", "cents", "decimal", "bigdecimal"],
    &["case-insensitive", "case insensitive", "case-sensitive", "case sensitive"],
    &["sync", "synchronous", "async", "asynchronous"],
    &["tabs", "spaces"],
    &["snake_case", "camelcase", "kebab-case"],
    &["rest", "graphql", "grpc"],
    &["uuid", "uuids", "autoincrement", "serial"],
    &["utc", "local time"],
];

/// The same choice spelled two ways.
fn canon(m: &str) -> &str {
    match m {
        "half-up" | "half_up" | "halfup" => "half up",
        "half-even" | "bankers" => "half even",
        "ceiling" => "ceil",
        "floats" | "double" => "float",
        "cents" => "integer cents",
        "case insensitive" => "case-insensitive",
        "case sensitive" => "case-sensitive",
        "synchronous" => "sync",
        "asynchronous" => "async",
        "uuids" => "uuid",
        "serial" => "autoincrement",
        other => other,
    }
}

/// Where a phrase stands in a text, as a word (bytes), each time.
fn find_word(hay_l: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = hay_l[from..].find(needle) {
        let at = from + i;
        let before_ok = hay_l[..at].chars().last().is_none_or(|c| !c.is_alphanumeric());
        let after_ok = hay_l[at + needle.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            out.push(at);
        }
        from = at + needle.len().max(1);
    }
    out
}

const NEG: [&str; 12] = ["never ", "not ", "no ", "don't ", "do not ", "avoid ", "instead of ", "rather than ", "was ", "from ", "without ", "stop "];

/// Whether the phrase at `at` is said against: a negation just before it,
/// or "lost"/"was wrong" just after.
fn negated(hay_l: &str, at: usize, len: usize) -> bool {
    let mut b = at.saturating_sub(16);
    while !hay_l.is_char_boundary(b) {
        b -= 1;
    }
    let before = &hay_l[b..at];
    let after = hay_l[at + len..].trim_start();
    NEG.iter().any(|n| before.ends_with(n) || before.contains(&format!(" {n}")) && before.trim_end().split_whitespace().count() <= 3 && before.rsplit(n.trim()).next().is_some_and(|r| r.split_whitespace().count() <= 1))
        || after.starts_with("lost")
        || after.starts_with("loses")
        || after.starts_with("was wrong")
}

/// The choices of each group a text makes (`pos`) and speaks against (`neg`).
fn choices(text_l: &str) -> Vec<(usize, HashSet<&'static str>, HashSet<&'static str>, Vec<(usize, usize, &'static str)>)> {
    let mut out = Vec::new();
    for (g, members) in GROUPS.iter().enumerate() {
        let (mut pos, mut neg, mut spans) = (HashSet::new(), HashSet::new(), Vec::new());
        for m in members.iter() {
            for at in find_word(text_l, m) {
                if negated(text_l, at, m.len()) {
                    neg.insert(canon(m));
                } else {
                    pos.insert(canon(m));
                    spans.push((at, at + m.len(), canon(m)));
                }
            }
        }
        if !pos.is_empty() || !neg.is_empty() {
            out.push((g, pos, neg, spans));
        }
    }
    out
}

/// What a record forbids: the one or two words after never, don't, avoid, … .
fn forbidden(text_l: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for n in ["never ", "don't ", "do not ", "avoid ", "instead of ", "rather than ", "must not ", "no more "] {
        for at in find_word(text_l, n.trim()) {
            let rest = &text_l[at + n.trim().len()..];
            let ws: Vec<String> = words(rest).into_iter().take(2).map(|w| stem(&w.0)).collect();
            if !ws.is_empty() {
                out.push(ws);
            }
        }
    }
    out
}

fn default_branch(main: &Path) -> String {
    let out = std::process::Command::new("git").arg("-C").arg(main).args(["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]).env("GIT_TERMINAL_PROMPT", "0").output();
    if let Ok(o) = out {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() && !s.is_empty() {
            return s.trim_start_matches("origin/").to_string();
        }
    }
    for b in ["main", "master"] {
        if crate::git::branch_exists(main, b) {
            return b.to_string();
        }
    }
    crate::git::current_branch(main).unwrap_or_else(|| "main".into())
}

/// The hotspots of a repository, when the `hotspots` table exists: path,
/// a score and why (whatever its columns are called).
pub fn hotspots(db: &StateDb, repo: &str) -> HashMap<String, (f64, String)> {
    let mut out = HashMap::new();
    let res = db.query_cols("SELECT * FROM hotspots WHERE repo = :repo LIMIT 5000", &Params::new().set("repo", sv(repo))).or_else(|_| db.query_cols("SELECT * FROM hotspots LIMIT 5000", &Params::new()));
    let Ok((cols, rows)) = res else { return out };
    let at = |names: &[&str]| names.iter().find_map(|n| cols.iter().position(|c| c == n));
    let Some(pi) = at(&["path", "file"]) else { return out };
    let si = at(&["score", "heat", "churn", "edits", "changes", "hits"]);
    let wi = at(&["reason", "why", "detail", "summary", "note"]);
    // Hotspots' own table (src/hotspots.rs) keeps its sentence in `data`.
    let di = cols.iter().position(|c| c == "data");
    let ri = cols.iter().position(|c| c == "repo");
    for r in rows {
        if let Some(ri) = ri {
            if get_str(&r[ri]).is_some_and(|x| x != repo && !x.is_empty()) {
                continue;
            }
        }
        let Some(path) = get_str(&r[pi]) else { continue };
        let score = si.map(|i| float(&r[i])).unwrap_or(0.0);
        let why = wi.and_then(|i| get_str(&r[i])).or_else(|| {
            let d: Json = serde_json::from_str(&di.and_then(|i| get_str(&r[i]))?).ok()?;
            d["summary"].as_str().map(str::to_string)
        });
        out.insert(path, (score, why.unwrap_or_default()));
    }
    out
}

#[derive(Deserialize)]
pub struct CheckB {
    pub text: Option<String>,
    pub repo: Option<String>,
    pub field: Option<String>,
    pub agent: Option<String>,
    /// The New agent dialog's Read-only box: the task is read-only.
    pub read_only: Option<bool>,
}

pub fn check(sup: &Arc<Supervisor>, b: CheckB) -> Result<Json, ApiError> {
    let started = Instant::now();
    let text = b.text.unwrap_or_default();
    let field = b.field.filter(|f| !f.is_empty()).unwrap_or_else(|| "task".into());
    let repo = repo_for(sup, b.repo.as_deref(), b.agent.as_deref())?;
    let me = b.agent.filter(|a| !a.is_empty());
    let mut marks: Vec<Mark> = Vec::new();
    let mut refs: Vec<Json> = Vec::new();
    let mut missing = Vec::new();
    if text.trim().is_empty() || repo.is_empty() {
        if repo.is_empty() && !text.trim().is_empty() {
            missing.push("No repository: name one to check the text against its agents, decisions and guarded list.".to_string());
        }
        return Ok(json!({ "marks": [], "refs": [], "took_ms": 0, "missing": missing }));
    }
    fresh(sup, &repo);
    let (files, recs) = mem().lock().unwrap().get(&repo).map(|e| (e.files.clone(), e.recs.clone())).unwrap_or_default();
    if let Some(m) = mem().lock().unwrap().get(&repo).and_then(|e| e.ctx_missing.clone()) {
        missing.push(m);
    }
    let main = PathBuf::from(&repo);
    let text_l = text.to_lowercase();
    // Lowercasing can change byte lengths (rare letters): then spans come
    // from the text itself.
    let same_len = text_l.len() == text.len();
    let agents: Vec<crate::db::Agent> = sup.agent_rows().into_iter().filter(|a| a.repo == repo).collect();
    let running = |id: &str| sup.state(id).is_some_and(|(_, r)| r);

    // References: @agents, ◆ records, files.
    let mut file_refs: Vec<(usize, usize, String)> = Vec::new();
    let bytes: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < bytes.len() {
        let (at, c) = bytes[i];
        let prev_ok = i == 0 || !bytes[i - 1].1.is_alphanumeric();
        if (c == '@' || c == '◆') && prev_ok {
            let s = at + c.len_utf8();
            let e = text[s..].find(|ch: char| !(ch.is_alphanumeric() || ch == '-' || ch == '_' || ch == ':')).map(|x| s + x).unwrap_or(text.len());
            let e = s + text[s..e].trim_end_matches([':', '-']).len();
            let name = &text[s..e];
            if !name.is_empty() {
                if c == '@' {
                    match agents.iter().filter(|a| a.name == name).max_by_key(|a| (running(&a.id), a.created)) {
                        Some(a) => refs.push(json!({ "start": at, "end": e, "kind": "agent", "label": format!("@{}", a.name), "detail": format!("{}: {}", if running(&a.id) { a.state.as_str() } else { "exited" }, a.task.clone().unwrap_or_default()), "ref": { "id": a.id, "name": a.name } })),
                        None => {
                            let close = agents.iter().filter(|a| running(&a.id)).map(|a| (edit_distance(&a.name.to_lowercase(), &name.to_lowercase()), a.name.clone())).filter(|(d, _)| *d <= (name.chars().count() / 3).max(2)).min().map(|(_, n)| n);
                            marks.push(Mark {
                                start: at,
                                end: e,
                                severity: "error",
                                kind: "unknown_ref",
                                message: format!("No agent of this repository is called {name}{}.", close.as_ref().map(|n| format!(": did you mean @{n}?")).unwrap_or_default()),
                                fix: close.map(|n| json!({ "label": format!("Use @{n}"), "edits": [{ "start": at, "end": e, "text": format!("@{n}") }] })),
                                refj: None,
                            });
                        }
                    }
                } else {
                    let rec = recs.iter().find(|r| r.id == name);
                    if let Some(r) = rec {
                        refs.push(json!({ "start": at, "end": e, "kind": r.kind, "label": r.text, "detail": format!("{} · {}{}", r.kind, r.by.clone().unwrap_or_else(|| "another session".into()), if r.superseded { " · superseded" } else { "" }), "ref": r.to_json() }));
                    }
                    match rec {
                        Some(r) if r.superseded => marks.push(Mark { start: at, end: e, severity: "warning", kind: "superseded", message: "This decision was superseded: the agent would follow an old choice. Trace it to see what replaced it.".into(), fix: Some(json!({ "label": "Trace what replaced it", "go": format!("#/trace?q={}", urlencode(&format!("why did we change: {}", r.text))) })), refj: Some(r.to_json()) }),
                        Some(_) => {}
                        None => marks.push(Mark { start: at, end: e, severity: "warning", kind: "unknown_ref", message: format!("No decision, preference, fact or rule {name} is known in this repository."), fix: None, refj: None }),
                    }
                }
            }
            i = bytes.iter().position(|(b, _)| *b >= e).unwrap_or(bytes.len());
            continue;
        }
        i += 1;
    }
    // Files: a word with a / or an extension that names a file of the checkout.
    {
        let set: HashSet<&String> = files.iter().collect();
        let mut by_base: HashMap<&str, Vec<&String>> = HashMap::new();
        for f in files.iter() {
            by_base.entry(f.rsplit('/').next().unwrap_or(f)).or_default().push(f);
        }
        let mut s = None;
        for (at, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
            let ok = c.is_alphanumeric() || matches!(c, '_' | '.' | '/' | '-');
            match (ok, s) {
                (true, None) => s = Some(at),
                (false, Some(st)) => {
                    let w = text[st..at].trim_end_matches(['.', '-', '/']);
                    let w = w.trim_start_matches("./");
                    let st2 = st + text[st..at].find(w).unwrap_or(0);
                    if w.contains('/') || w.contains('.') {
                        let path = if set.contains(&w.to_string()) { Some(w.to_string()) } else { by_base.get(w).filter(|v| v.len() == 1).map(|v| v[0].clone()) };
                        if let Some(p) = path {
                            refs.push(json!({ "start": st2, "end": st2 + w.len(), "kind": "file", "label": p, "ref": { "path": p } }));
                            file_refs.push((st2, st2 + w.len(), p));
                        }
                    }
                    s = None;
                }
                _ => {}
            }
        }
    }
    let intent = matches!(field.as_str(), "task" | "goal" | "change" | "steer" | "answer" | "persona");
    // No done check: a task or a goal that says nothing of how the agent
    // knows it is finished.
    let ws = words(&text);
    // A task that says not to change files (SPEC.md, "Sixth build: E"):
    // nothing stops a session from editing, unless it starts read-only.
    let said_ro = said_read_only(&text);
    if field == "task" && b.read_only != Some(true) {
        if let Some((s, e)) = said_ro {
            marks.push(Mark {
                start: s,
                end: e,
                severity: "info",
                kind: "read_only",
                message: "You ask it not to change files, but words do not stop a session from editing. Start it read-only: Workbench then refuses its Edit and Write, and runs only commands that read.".into(),
                fix: Some(json!({ "label": "Start it read-only", "set": { "read_only": true } })),
                refj: None,
            });
        }
    }
    let read_only_task = b.read_only == Some(true) || said_ro.is_some() || (asks_answer(&text) && !changes_code(&text));
    if matches!(field.as_str(), "task" | "goal") && ws.len() >= 3 {
        if !foresee::has_done(&text) {
            let end = text.find(['.', ',', ';', ':', '!', '?', '\n']).unwrap_or(text.len());
            let mut n = 0;
            let mut cut = end;
            for (i, c) in text.char_indices() {
                if i >= end {
                    break;
                }
                if c.is_whitespace() {
                    n += 1;
                    if n >= 7 {
                        cut = i;
                        break;
                    }
                }
            }
            let start = text.len() - text.trim_start().len();
            let named: Vec<String> = file_refs.iter().map(|f| f.2.clone()).collect();
            // A test only for a task that changes code; a question is done
            // when it is answered.
            let cmd = if read_only_task { None } else { foresee::covering(sup, &repo, &main, &named).map(|c| c.0).or_else(|| done_command(sup, &repo, &main, &file_refs)) };
            let (label, clause) = match (&cmd, read_only_task) {
                (Some(c), _) => (format!("Add done check: {c} passes"), format!("Done when `{c}` passes.")),
                (None, true) => match answers_what(&text) {
                    Some(q) => (format!("Add: done when it answers {q}"), format!("Done when it answers: {q}.")),
                    None => ("Add a done check".to_string(), "Done when it has answered in its reply, changing no file.".to_string()),
                },
                (None, false) => ("Add a done check".to_string(), "Done when a test shows it works.".to_string()),
            };
            // As a clause of its own, after the last complete sentence: never
            // glued onto one still being written.
            let fix = done_insert(&text).map(|(at, lead)| json!({ "label": label, "edits": [{ "start": at, "end": at, "text": format!("{lead}{clause}") }] }));
            let open = sentence_open(&text);
            marks.push(Mark {
                start,
                end: cut.max(start + 1).min(text.len()),
                severity: "warning",
                kind: "no_done_check",
                message: if fix.is_none() && open {
                    "Nothing says how the agent knows it is done. Finish the sentence, then add a done check: a test that passes, or (for a question) what the answer must say.".into()
                } else if read_only_task {
                    "Nothing says how the agent knows it is done: say what the answer must cover, so it stops when it has answered.".into()
                } else {
                    "Nothing says how the agent knows it is done: a test that passes, or something you can see. Without one it stops when it thinks it is finished.".into()
                },
                fix,
                refj: cmd.map(|c| json!({ "command": c })),
            });
        }
    }
    // Against a decision or a rule: the records nearest the text (BM25 in
    // ChloeDB), read for a choice the text makes otherwise or a thing they forbid.
    if intent && same_len {
        let qtext: String = ws.iter().map(|w| w.0.clone()).collect::<Vec<_>>().join(" ");
        let near: Vec<String> = if qtext.is_empty() {
            Vec::new()
        } else {
            q(sup, "SELECT refj, score() FROM complete_items WHERE repo = :repo AND kind IN ('rule', 'decision', 'preference', 'fact') ORDER BY bm25(body, :q, syntax => 'plain') DESC LIMIT 30", &Params::new().set("repo", sv(&repo)).set("q", sv(&qtext)))
                .unwrap_or_default()
                .iter()
                .filter(|r| float(&r[1]) > 0.0)
                .filter_map(|r| get_str(&r[0]).and_then(|s| serde_json::from_str::<Json>(&s).ok()).and_then(|j| j["id"].as_str().map(str::to_string)))
                .collect()
        };
        let mut cands: Vec<&Rec> = near.iter().filter_map(|id| recs.iter().find(|r| &r.id == id)).filter(|r| !r.superseded).collect();
        cands.sort_by_key(|r| (r.kind != "rule", r.kind != "decision"));
        let task_choices = choices(&text_l);
        let task_stems: Vec<(String, usize, usize)> = ws.iter().map(|w| (stem(&w.0), w.1, w.2)).collect();
        let mut taken: Vec<(usize, usize)> = Vec::new();
        for r in cands {
            if marks.iter().filter(|m| m.kind == "contradicts").count() >= 3 {
                break;
            }
            let rl = r.text.to_lowercase();
            let mut hit: Option<(usize, usize, String, Option<String>)> = None;
            for (g, rpos, rneg, _) in choices(&rl) {
                if rpos.is_empty() {
                    continue;
                }
                if let Some((_, tpos, _, spans)) = task_choices.iter().find(|t| t.0 == g) {
                    if let Some(&(s, e, m)) = spans.iter().find(|sp| !rpos.contains(sp.2) && (rneg.contains(sp.2) || !tpos.iter().any(|x| rpos.contains(x)))) {
                        let want = rpos.iter().next().map(|x| x.to_string());
                        hit = Some((s, e, format!("“{}” goes against {}: {}", &text[s..e], if r.kind == "rule" { "your rule" } else { "what this repository decided" }, r.text), want));
                        let _ = m;
                        break;
                    }
                }
            }
            if hit.is_none() {
                for f in forbidden(&rl) {
                    let at = task_stems.iter().position(|w| w.0 == f[0]);
                    let all = f.iter().all(|x| task_stems.iter().any(|w| &w.0 == x));
                    if let (Some(i), true) = (at, all) {
                        let (s, e) = (task_stems[i].1, task_stems[i].2);
                        if negated(&text_l, s, e - s) {
                            continue;
                        }
                        hit = Some((s, e, format!("“{}” is something {} rules out: {}", &text[s..e], if r.kind == "rule" { "your rule" } else { "a decision of this repository" }, r.text), None));
                        break;
                    }
                }
            }
            if let Some((s, e, message, want)) = hit {
                if taken.iter().any(|&(a, b)| s < b && a < e) {
                    continue;
                }
                taken.push((s, e));
                let fix = match want {
                    Some(w) => json!({ "label": format!("Use {w} instead"), "edits": [{ "start": s, "end": e, "text": w }] }),
                    None => json!({ "label": "Trace why", "go": format!("#/trace?q={}", urlencode(&format!("why {}", r.text))) }),
                };
                marks.push(Mark { start: s, end: e, severity: if matches!(r.kind.as_str(), "rule" | "decision") { "error" } else { "warning" }, kind: "contradicts", message, fix: Some(fix), refj: Some(r.to_json()) });
            }
        }
    }
    // A guarded command: in backticks, or said in words.
    if intent {
        let rules = crate::guarded::rules_of(sup, &repo);
        let default = default_branch(&main);
        let current = crate::git::current_branch(&main);
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut cand: Vec<(usize, usize)> = Vec::new();
        let mut from = 0;
        while let Some(a) = text[from..].find('`') {
            let s = from + a + 1;
            let Some(b) = text[s..].find('`') else { break };
            cand.push((s, s + b));
            from = s + b + 1;
        }
        // Each clause, from each of its words: "then run cargo publish".
        for clause in split_clauses(&text) {
            let wpos: Vec<usize> = std::iter::once(clause.0).chain(text[clause.0..clause.1].char_indices().filter(|(_, c)| c.is_whitespace()).map(|(i, _)| clause.0 + i + 1)).collect();
            for &w in wpos.iter().take(24) {
                if w < clause.1 {
                    cand.push((w, clause.1));
                }
            }
        }
        for (s, e) in cand {
            let cmd = text[s..e].trim();
            if cmd.is_empty() || spans.iter().any(|&(a, b)| s < b && a < e) {
                continue;
            }
            if let Some((rule, label)) = crate::guarded::matches(cmd, &main, &main, &rules, &default, current.as_deref()) {
                let end = s + text[s..e].trim_end().len();
                spans.push((s, end));
                marks.push(Mark { start: s, end, severity: "info", kind: "guarded", message: format!("`{cmd}` is on this repository's guarded list ({label}): the agent cannot run it; it proposes it with propose_change and waits for you to approve it in Decide."), fix: None, refj: Some(json!({ "rule": rule, "label": label })) });
            }
        }
        // In words: push to the default branch, deploy to production, publish, run the migrations.
        let says: [(&str, &str, String); 6] = [
            ("push-default", "a push to the default branch", format!("push to {default}")),
            ("push-default", "a push to the default branch", "force push".to_string()),
            ("deploy", "a deploy", "deploy to prod".to_string()),
            ("deploy", "a deploy", "publish".to_string()),
            ("migrations", "a migration", "run the migration".to_string()),
            ("migrations", "a migration", "migrate the database".to_string()),
        ];
        for (rule, what, phrase) in says.iter() {
            if !rules.iter().any(|r| r.id == *rule && r.on) || !same_len {
                continue;
            }
            for at in find_word(&text_l, phrase).into_iter().chain(text_l.find(phrase.as_str()).filter(|_| phrase.ends_with("prod") || phrase.ends_with("migration"))) {
                let mut e = at + phrase.len();
                while e < text.len() && text[e..].chars().next().is_some_and(|c| c.is_alphanumeric()) {
                    e += text[e..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                }
                if spans.iter().any(|&(a, b)| at < b && a < e) {
                    continue;
                }
                spans.push((at, e));
                marks.push(Mark { start: at, end: e, severity: "info", kind: "guarded", message: format!("This asks for {what}, which is on this repository's guarded list: the agent proposes it with propose_change and waits for you to approve it in Decide."), fix: None, refj: Some(json!({ "rule": rule })) });
                break;
            }
        }
    }
    // Overlap with a running agent: the files it edits, or a task like its own.
    if intent {
        let paths: Vec<String> = file_refs.iter().map(|f| f.2.clone()).collect();
        let eds = editors(sup, &repo, &paths, me.as_deref());
        let ids: HashMap<String, (String, String)> = agents.iter().filter(|a| running(&a.id)).map(|a| (a.name.clone(), (a.id.clone(), a.task.clone().unwrap_or_default()))).collect();
        let mut seen = HashSet::new();
        for (s, e, p) in &file_refs {
            if let Some((name, when)) = eds.get(p) {
                if !seen.insert(p.clone()) {
                    continue;
                }
                let (id, task) = ids.get(name).cloned().unwrap_or_default();
                marks.push(Mark {
                    start: *s,
                    end: *e,
                    severity: "warning",
                    kind: "overlap",
                    message: format!("{name} is changing {p} now (edited {when}){}. Two agents on one file collide when they merge.", if task.is_empty() { String::new() } else { format!(" for “{}”", crate::supervisor::one_line(&task, 80)) }),
                    fix: Some(json!({ "label": format!("Steer @{name} instead"), "go": format!("#/story/{id}") })),
                    refj: Some(json!({ "kind": "agent", "id": id, "name": name, "text": task, "by": name })),
                });
            }
        }
        if field == "task" && ws.len() >= 3 {
            let qs = ws.iter().map(|w| w.0.clone()).collect::<Vec<_>>().join(" ");
            let need = (ws.len() / 2).clamp(2, 4);
            let sql = format!("SELECT id, score() FROM agents WHERE repo = :repo AND state != 'exited' ORDER BY bm25(task, :q, syntax => 'plain', min => {need}) DESC LIMIT 5");
            for r in q(sup, &sql, &Params::new().set("repo", sv(&repo)).set("q", sv(&qs))).unwrap_or_default() {
                let Some(a) = get_str(&r[0]).and_then(|id| sup.agent_row(&id)) else { continue };
                if float(&r[1]) <= 0.0 || !running(&a.id) || me.as_deref() == Some(a.id.as_str()) || marks.iter().any(|m| m.kind == "overlap" && m.refj.as_ref().is_some_and(|j| j["id"] == a.id.as_str())) {
                    continue;
                }
                let theirs: HashSet<String> = words(a.task.as_deref().unwrap_or("")).into_iter().map(|w| stem(&w.0)).collect();
                let shared: Vec<&(String, usize, usize)> = ws.iter().filter(|w| theirs.contains(&stem(&w.0))).collect();
                if shared.len() < 2 || (shared.len() as f64) < ws.len() as f64 * 0.5 {
                    continue;
                }
                let (s, e) = (shared.first().unwrap().1, shared.last().unwrap().2);
                marks.push(Mark {
                    start: s,
                    end: e,
                    severity: "warning",
                    kind: "overlap",
                    message: format!("{} is already working on this: “{}”.", a.name, crate::supervisor::one_line(a.task.as_deref().unwrap_or(""), 120)),
                    fix: Some(json!({ "label": format!("Steer @{} instead", a.name), "go": format!("#/story/{}", a.id) })),
                    refj: Some(json!({ "kind": "agent", "id": a.id, "name": a.name, "text": a.task, "by": a.name })),
                });
                break;
            }
        }
    }
    // Hotspots: when the table exists (src/hotspots.rs).
    if !file_refs.is_empty() {
        let hot = hotspots(sup.db(), &repo);
        let mut seen = HashSet::new();
        for (s, e, p) in &file_refs {
            if let Some((score, why)) = hot.get(p) {
                if !seen.insert(p.clone()) || marks.iter().any(|m| m.start == *s && m.kind == "overlap") {
                    continue;
                }
                marks.push(Mark {
                    start: *s,
                    end: *e,
                    severity: "info",
                    kind: "hotspot",
                    message: if why.is_empty() { format!("{p} is a hotspot (score {score:.1}): changes there often break or collide. Name the test that guards it.") } else { format!("{p} is a hotspot: {why}") },
                    fix: None,
                    refj: Some(json!({ "path": p, "score": score, "why": why })),
                });
            }
        }
    }
    marks.sort_by_key(|m| (m.start, m.end));
    let conv = |b: usize| u16_at(&text, b);
    let conv_fix = |f: Json| -> Json {
        let mut f = f;
        if let Some(edits) = f.get_mut("edits").and_then(Json::as_array_mut) {
            for e in edits.iter_mut() {
                let (s, en) = (e["start"].as_u64().unwrap_or(0) as usize, e["end"].as_u64().unwrap_or(0) as usize);
                e["start"] = json!(conv(s));
                e["end"] = json!(conv(en));
            }
        }
        f
    };
    let marks_json: Vec<Json> = marks
        .into_iter()
        .map(|m| {
            let mut j = json!({ "start": conv(m.start), "end": conv(m.end), "severity": m.severity, "kind": m.kind, "message": m.message });
            if let Some(f) = m.fix {
                j["fix"] = conv_fix(f);
            }
            if let Some(r) = m.refj {
                j["ref"] = r;
            }
            j
        })
        .collect();
    let refs_json: Vec<Json> = refs
        .into_iter()
        .map(|mut r| {
            let (s, e) = (r["start"].as_u64().unwrap_or(0) as usize, r["end"].as_u64().unwrap_or(0) as usize);
            r["start"] = json!(conv(s));
            r["end"] = json!(conv(e));
            r
        })
        .collect();
    Ok(json!({ "repo": repo, "field": field, "marks": marks_json, "refs": refs_json, "took_ms": (started.elapsed().as_secs_f64() * 10_000.0).round() / 10.0, "missing": missing }))
}

/// The words a sentence does not end on: it is still being written.
const DANGLING: [&str; 52] = [
    "from", "to", "the", "a", "an", "and", "or", "of", "in", "on", "with", "by", "for", "at", "into", "through", "about", "that", "which", "so", "but", "then", "when", "if", "because",
    "as", "than", "via", "using", "like", "is", "are", "was", "my", "our", "its", "their", "this", "these", "those", "your", "where", "how", "what", "why", "who", "not", "no", "until", "after", "before", "while",
];

/// Whether the text's last sentence is unfinished: it ends on a word such
/// as "from" or "the", or on a comma, colon or dash.
fn sentence_open(text: &str) -> bool {
    let t = text.trim_end();
    if t.ends_with([',', ':', ';', '-', '(', '—', '–', '/']) {
        return true;
    }
    if t.ends_with(['.', '!', '?', ')', '`', '"']) {
        return false;
    }
    let last = t.rsplit(|c: char| !c.is_alphanumeric() && c != '\'').next().unwrap_or("").to_lowercase();
    DANGLING.contains(&last.as_str())
}

/// Where a done clause goes (a byte offset, and what comes before it): at
/// the end when the last sentence is complete, else after the last
/// complete one; `None` when there is none yet.
fn done_insert(text: &str) -> Option<(usize, &'static str)> {
    let t = text.trim_end();
    if !sentence_open(t) {
        let lead = if t.ends_with(['.', '!', '?']) { " " } else { ". " };
        return Some((t.len(), lead));
    }
    // The last sentence end before the unfinished one.
    let cut = t.char_indices().filter(|(i, c)| matches!(c, '.' | '!' | '?' | '\n') && t[i + c.len_utf8()..].starts_with(char::is_whitespace)).map(|(i, c)| i + c.len_utf8()).last()?;
    let before = t[..cut].trim_end();
    if before.is_empty() {
        return None;
    }
    let lead = if before.ends_with(['.', '!', '?']) { " " } else { ". " };
    Some((before.len(), lead))
}

/// Words that change code; a task without one (or with each said against)
/// asks for an answer, not a change.
const CHANGE_WORDS: [&str; 32] = [
    "fix", "add", "change", "make", "implement", "refactor", "write", "update", "remove", "delete", "rename", "edit", "create", "build", "move", "replace", "convert", "support", "migrate",
    "bump", "upgrade", "rewrite", "introduce", "handle", "port", "wire", "patch", "drop", "extract", "split", "merge", "commit",
];

fn is_change_word(w: &str) -> bool {
    CHANGE_WORDS.contains(&w)
        || ["ing", "es", "ed", "s"].iter().any(|suf| w.strip_suffix(suf).filter(|b| b.len() >= 2).is_some_and(|b| CHANGE_WORDS.contains(&b) || CHANGE_WORDS.contains(&format!("{b}e").as_str())))
}

fn changes_code(text: &str) -> bool {
    let t = text.to_lowercase();
    if t.len() != text.len() {
        return true;
    }
    let mut start = None;
    for (i, c) in t.char_indices().chain(std::iter::once((t.len(), ' '))) {
        if c.is_alphanumeric() {
            start.get_or_insert(i);
            continue;
        }
        if let Some(s) = start.take() {
            if is_change_word(&t[s..i]) && !negated(&t, s, i - s) {
                return true;
            }
        }
    }
    false
}

/// Whether a task asks for an answer: "explain …", "how does …", a "?".
fn asks_answer(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    answers_what(text).is_some()
        || t.ends_with('?')
        || ["how ", "why ", "what ", "where ", "which ", "who ", "when ", "does ", "is ", "can ", "explain", "describe", "summari", "review", "investigate", "look into", "find out"].iter().any(|w| t.starts_with(w) || t.contains(&format!(": {w}")) || t.contains(&format!(". {w}")))
}

/// Where a task says not to change files: "read-only", "do not edit",
/// "don't change any files", "without changing", …
fn said_read_only(text: &str) -> Option<(usize, usize)> {
    let t = text.to_lowercase();
    if t.len() != text.len() {
        return None;
    }
    const SAYS: [&str; 16] = [
        "read-only", "read only", "readonly", "do not edit", "don't edit", "dont edit", "do not change any", "don't change any", "without changing", "do not modify", "don't modify",
        "no edits", "do not write", "don't write", "do not touch any file", "don't touch any file",
    ];
    SAYS.iter().filter_map(|s| t.find(s).map(|i| (i, i + s.len()))).min()
}

/// What a question-task asks, for "done when it answers: …": the words
/// after "explain", "describe", … up to the end of its sentence, without
/// an unfinished tail.
fn answers_what(text: &str) -> Option<String> {
    let t = text.to_lowercase();
    if t.len() != text.len() {
        return None;
    }
    const ASK: [&str; 12] = ["explain ", "describe ", "summarize ", "summarise ", "tell me ", "show me ", "walk me through ", "document ", "find out ", "figure out ", "map out ", "list "];
    let (i, n) = ASK.iter().filter_map(|a| t.find(a).map(|i| (i, a.len()))).min()?;
    let mut q = text[i + n..].split(['.', '!', '?', '\n']).next().unwrap_or("").trim().to_string();
    // Drop a tail still being written: back to the last comma, else word by word.
    while sentence_open(&q) {
        match q.rfind([',', ';', ':']) {
            Some(c) if c > 0 && q[c + 1..].split_whitespace().count() <= 3 => q.truncate(c),
            _ => match q.trim_end().rfind(' ') {
                Some(c) => q.truncate(c),
                None => q.clear(),
            },
        }
        q = q.trim_end().trim_end_matches([',', ';', ':', '-']).to_string();
    }
    let words = q.split_whitespace().count();
    (words >= 2 && q.len() <= 140).then_some(q)
}

/// Levenshtein's distance, in characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for j in 0..b.len() {
            let cur = row[j + 1];
            row[j + 1] = if ca == b[j] { prev } else { 1 + prev.min(row[j]).min(row[j + 1]) };
            prev = cur;
        }
    }
    row[b.len()]
}

/// A text's clauses (bytes): split at . ; , ! ? and newlines, and before
/// "then" and "and".
fn split_clauses(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut s = 0;
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | ';' | ',' | '!' | '?' | '\n') && !text[i + c.len_utf8()..].starts_with(|n: char| n.is_alphanumeric()) {
            if i > s {
                out.push((s, i));
            }
            s = i + c.len_utf8();
        }
    }
    if s < text.len() {
        out.push((s, text.len()));
    }
    let mut more = Vec::new();
    for (a, b) in out {
        let mut st = a;
        let lower = text[a..b].to_lowercase();
        for sep in [" then ", " and "] {
            for at in find_word(&lower, sep.trim()) {
                let cut = a + at + sep.trim().len();
                if cut > st && cut < b && lower[..at].ends_with(' ') {
                    more.push((st, a + at));
                    st = cut;
                }
            }
        }
        more.push((st, b));
    }
    more.into_iter().map(|(a, b)| (a + (text[a..b].len() - text[a..b].trim_start().len()), a + text[a..b].trim_end().len())).filter(|(a, b)| a < b).collect()
}

/// The command that would show a task done: the test run of the files it
/// names, else the repository's most run test command, else its runner's.
fn done_command(sup: &Supervisor, repo: &str, main: &Path, file_refs: &[(usize, usize, String)]) -> Option<String> {
    let rows = q(sup, "SELECT command, test FROM test_runs WHERE repo = :repo ORDER BY ts DESC LIMIT 2000", &Params::new().set("repo", sv(repo))).unwrap_or_default();
    let stems: Vec<String> = file_refs.iter().map(|f| f.2.rsplit('/').next().unwrap_or(&f.2).split('.').next().unwrap_or("").to_lowercase()).filter(|s| s.len() >= 3).collect();
    for r in &rows {
        let cmd = get_str(&r[0]).unwrap_or_default();
        let test = get_str(&r[1]).unwrap_or_default().to_lowercase();
        if stems.iter().any(|s| cmd.to_lowercase().contains(s) || test.contains(s)) && !cmd.is_empty() {
            return Some(crate::supervisor::one_line(&cmd, 80));
        }
    }
    let mut n: HashMap<String, usize> = HashMap::new();
    for r in &rows {
        if let Some(c) = get_str(&r[0]).filter(|c| !c.is_empty()) {
            *n.entry(c).or_default() += 1;
        }
    }
    if let Some((c, _)) = n.into_iter().max_by_key(|(c, k)| (*k, std::cmp::Reverse(c.len()))) {
        return Some(crate::supervisor::one_line(&c, 80));
    }
    let has = |f: &str| main.join(f).exists();
    if has("Cargo.toml") {
        Some("cargo test".into())
    } else if has("package.json") {
        Some("npm test".into())
    } else if has("pyproject.toml") || has("pytest.ini") || has("setup.py") || main.join("tests").is_dir() {
        Some("pytest".into())
    } else if has("go.mod") {
        Some("go test ./...".into())
    } else {
        None
    }
}

/// The marks as lines, for an agent (MCP `check_intent`).
pub fn as_text(v: &Json) -> String {
    let marks = v["marks"].as_array().cloned().unwrap_or_default();
    if marks.is_empty() {
        return "No problems found: it names how it will be checked, and crosses no decision, rule, guarded command or running agent.".into();
    }
    let mut out = vec![format!("{} to look at before you hand this over:", if marks.len() == 1 { "One thing".to_string() } else { format!("{} things", marks.len()) })];
    for m in &marks {
        let mut line = format!("- {} {}: {}", m["severity"].as_str().unwrap_or(""), m["kind"].as_str().unwrap_or(""), m["message"].as_str().unwrap_or(""));
        if let Some(id) = m["ref"]["id"].as_str() {
            if m["kind"] == "contradicts" {
                line.push_str(&format!(" (◆{id})"));
            }
        }
        if let Some(f) = m["fix"]["label"].as_str() {
            line.push_str(&format!(" Fix: {f}."));
        }
        out.push(line);
    }
    out.join("\n")
}

// ------------------------------------------------------------- routes

async fn events_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: foresee::EventsB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || foresee::record(&sup, b)).await?).into_response())
}

#[derive(Deserialize)]
struct StatsQ {
    user: Option<String>,
    repo: Option<String>,
}

async fn stats_route(State(app): State<App>, Query(sq): Query<StatsQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(
        blocking(move || {
            let repo = match sq.repo.as_deref().filter(|r| !r.is_empty()) {
                Some(r) => Some(trail::repo_dir(Some(r))?.to_string_lossy().to_string()),
                None => None,
            };
            Ok(foresee::stats(&sup, &foresee::user_of(sq.user.as_deref()), repo.as_deref()))
        })
        .await?,
    )
    .into_response())
}

async fn ghost_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: foresee::GhostB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || foresee::model_ghost(&sup, b)).await?).into_response())
}

async fn complete_route(State(app): State<App>, Query(cq): Query<CompleteQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || complete(&sup, cq)).await?).into_response())
}

async fn check_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: CheckB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || check(&sup, b)).await?).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_done_check_goes_after_a_complete_sentence() {
        let lee = "Read-only: explain how money flows through this shop, from ";
        assert!(sentence_open(lee));
        // No complete sentence yet: nowhere to put it.
        assert_eq!(done_insert(lee), None);
        // After the last complete one, as its own clause.
        let t = "Read money.py and cart.py. Then explain how money flows through the shop, from";
        let (at, lead) = done_insert(t).unwrap();
        assert_eq!((&t[..at], lead), ("Read money.py and cart.py.", " "));
        assert_eq!(done_insert("Fix the rounding in money.py"), Some((28, ". ")));
        assert_eq!(done_insert("Fix the rounding in money.py.  "), Some((29, " ")));
        // Read-only or a question: no test, an answer.
        assert!(!changes_code(lee) && asks_answer(lee));
        assert!(!asks_answer("Floor each line total in money.py"));
        assert!(!changes_code("Do not edit any files. Read money.py and explain the rounding"));
        assert!(changes_code("Fix the rounding in money.py"));
        assert!(changes_code("Making totals use integer cents"));
        assert_eq!(answers_what(lee).as_deref(), Some("how money flows through this shop"));
        assert_eq!(answers_what("Explain why tax is 82 cents on $10.").as_deref(), Some("why tax is 82 cents on $10"));
        assert_eq!(said_read_only("Do not edit any files. Read money.py"), Some((0, 11)));
        assert_eq!(said_read_only("Fix it"), None);
    }

    #[test]
    fn tokens_and_offsets() {
        let t = token_at("ask @fix-to", 11);
        assert_eq!((t.text.as_str(), t.start, t.trigger), ("@fix-to", 4, Some('@')));
        assert_eq!(t.bare(), "fix-to");
        let s = "◆ab:1 then src/mo";
        let t = token_at(s, s.len());
        assert_eq!(t.text, "src/mo");
        let t = token_at("keep ◆c4a7", "keep ◆c4a7".len());
        assert_eq!((t.text.as_str(), t.trigger), ("◆c4a7", Some('◆')));
        // UTF-16: ◆ is one unit, three bytes.
        let s = "◆x é";
        assert_eq!(byte_at(s, 1), 3);
        assert_eq!(u16_at(s, 3), 1);
        assert_eq!(byte_at(s, 99), s.len());
    }

    #[test]
    fn choices_and_negation() {
        let r = "round each line's discount half up before summing; floor lost a cent on 3 of 40 carts.";
        let c = choices(r);
        assert_eq!(c.len(), 1);
        assert!(c[0].1.contains("half up") && c[0].2.contains("floor"), "{:?}", c);
        let t = choices("make the total floor the discount");
        assert!(t[0].1.contains("floor"));
        assert_eq!(forbidden("prices are integer cents; never floats."), vec![vec!["float".to_string()]]);
        assert_eq!(stem("rounding"), "round");
        assert_eq!(stem("class"), "class");
        let cl = split_clauses("Fix it, then run cargo publish and push to main.");
        let texts: Vec<&str> = cl.iter().map(|(a, b)| &"Fix it, then run cargo publish and push to main."[*a..*b]).collect();
        assert_eq!(texts, ["Fix it", "run cargo publish", "push to main"]);
    }

    #[test]
    fn the_index_answers_prefixes_and_hotspots_are_read_when_there() {
        let dir = tempfile::tempdir().unwrap();
        let db = StateDb::open(dir.path()).unwrap();
        let p = Params::new()
            .set("id", sv("r|file|a"))
            .set("pfx", Value::List(vec![sv("total.rs"), sv("api/src/cart/total.rs"), sv("total")]))
            .set("w", Value::Float(0.5));
        db.query("INSERT INTO complete_items (id, repo, kind, pfx, label, ins, detail, refj, body, weight, grp) VALUES (:id, 'r', 'file', :pfx, 'api/src/cart/total.rs', 'x', '', 'null', 'api src cart total', :w, '')", &p).unwrap();
        let hit = db.query("SELECT label FROM complete_items WHERE repo IN ('r', '*') AND kind IN ('file') AND PREFIX(pfx, :p) ORDER BY weight DESC LIMIT 5", &Params::new().set("p", sv("tot"))).unwrap();
        assert_eq!(hit.len(), 1);
        // The records near a text, by BM25 over their words.
        db.query("INSERT INTO complete_items (id, repo, kind, pfx, label, ins, detail, refj, body, weight, grp) VALUES ('d1', 'r', 'decision', :pfx, 'm', 'x', '', '{\"id\":\"d1\"}', 'Money rounds half up per line; never floor. money.py', 0.5, '')", &Params::new().set("pfx", Value::List(vec![sv("money")]))).unwrap();
        let near = db.query("SELECT refj, score() FROM complete_items WHERE repo = 'r' AND kind IN ('rule', 'decision') ORDER BY bm25(body, :q, syntax => 'plain') DESC LIMIT 30", &Params::new().set("q", sv("floor each line total pytest passes"))).unwrap();
        assert_eq!(near.len(), 1);
        let typo = db.query("SELECT label FROM complete_items WHERE repo = 'r' AND PREFIX(pfx, :p, fuzzy => 'auto') LIMIT 5", &Params::new().set("p", sv("totla"))).unwrap();
        assert_eq!(typo.len(), 1);
        assert!(hotspots(&db, "/r").is_empty());
        // The hotspots table is Hotspots' own (src/hotspots.rs), its sentence in `data`.
        db.query("INSERT INTO hotspots (id, repo, file, score, level, data) VALUES ('h1', '/r', 'money.py', 9.5, 'warm', :d)", &Params::new().set("d", sv(r#"{"summary": "changed 14 times by 4 agents this week"}"#))).unwrap();
        let h = hotspots(&db, "/r");
        assert_eq!(h.get("money.py").map(|x| x.1.as_str()), Some("changed 14 times by 4 agents this week"));
    }
}
