//! Code search for agents (SPEC.md, "Fourth build", row 1): the
//! repository's files as chunks in the state database (`code_files`,
//! `code_chunks`), a chunk a function, a class or a heading where a line
//! says so, else 60 lines; ranked by BM25 over the chunk's text with the
//! `simple` analyzer, plus its identifiers split (`cartTotal` is also
//! `cart total`, `cart_total` also `carttotal`) and its symbol and path
//! weighed twice. The main checkout is the tree `main`; an agent with a
//! worktree has the files it changed as a tree of its own (its id) laid
//! over it, so a search "as seen by" that agent finds its version.
//!
//! Fresh: a full pass reads `git ls-files` (files whose size and time
//! changed are chunked again), run on the first search of a repository,
//! again after a minute on the next search, and on demand; every
//! `PostToolUse` of an edit tool reindexes the file it names in that
//! agent's tree (a watcher on the stream of changes, and a catch-up before
//! each search).
//!
//! `GET /api/search/code?repo&q&agent&tokens&limit&lang&by`,
//! `GET /api/search/index?repo`, `POST /api/search/reindex {repo}`,
//! `GET /api/search/quick?repo&q` (code, docs and ChloeCtx's decisions).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock, Weak};
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

use crate::db::{get_int, get_ms, get_str, now_ms, ts};
use crate::map::{self, EDIT_TOOLS};
use crate::levels::{blocking, parse_body, ApiResult};
use crate::server::App;
use crate::supervisor::{ApiError, Change, Supervisor};
use crate::{git, trail};

/// Chunks of files without definitions, and the most one chunk holds.
const WINDOW: usize = 60;
const MAX_CHUNK: usize = 80;
/// Files read: at most this big each, this many, this much in all.
const MAX_FILE: u64 = 512 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_TOTAL: u64 = 96 << 20;
/// A full pass again when the last is older than this (on a search).
const RESYNC: Duration = Duration::from_secs(60);
/// How long a first search waits for the first pass.
const FIRST_WAIT: Duration = Duration::from_secs(20);
/// Agents whose worktrees are overlaid: running, or ended this recently.
const OVERLAY_KEEP_MS: i64 = 24 * 3600 * 1000;

pub fn routes(app: &App) -> Router<App> {
    watch(&app.sup);
    Router::new()
        .route("/api/search/code", get(code_route))
        .route("/api/search/index", get(index_route))
        .route("/api/search/reindex", post(reindex_route))
        .route("/api/search/quick", get(quick_route))
}

// ------------------------------------------------------------- chunking

/// One chunk of a file: lines `start..=end` (from 1).
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub symbol: Option<String>,
    pub kind: String,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// The language of a path, by its extension.
pub fn lang_of(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" | "pyi" => "python",
        "go" => "go",
        "md" | "markdown" | "mdx" => "markdown",
        "sql" => "sql",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "php" => "php",
        "sh" | "bash" | "zsh" => "shell",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" | "less" => "css",
        "vue" | "svelte" => "javascript",
        _ => "other",
    }
}

fn ident(s: &str) -> String {
    s.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect()
}

fn strip_any<'a>(mut t: &'a str, prefixes: &[&str]) -> &'a str {
    loop {
        let before = t;
        for p in prefixes {
            if let Some(r) = t.strip_prefix(p) {
                t = r.trim_start();
            }
        }
        if t == before {
            return t;
        }
    }
}

/// The definition a line starts, as `(kind, name)`: a function, type or
/// class near the left margin, or a Markdown heading.
pub fn def_of(line: &str, lang: &str) -> Option<(String, String)> {
    let indent = line.chars().take_while(|c| c.is_whitespace()).map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>();
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    let named = |kind: &str, rest: &str| {
        let n = ident(rest.trim_start());
        (!n.is_empty()).then(|| (kind.to_string(), n))
    };
    match lang {
        "markdown" => {
            let hashes = t.chars().take_while(|c| *c == '#').count();
            if indent == 0 && (1..=3).contains(&hashes) && t[hashes..].starts_with(' ') {
                let title = t[hashes..].trim();
                return (!title.is_empty()).then(|| ("heading".to_string(), title.chars().take(80).collect()));
            }
            None
        }
        "rust" if indent <= 4 => {
            let t = strip_any(t, &["pub(crate) ", "pub(super) ", "pub(in crate) ", "pub ", "async ", "const fn", "unsafe ", "extern \"C\" ", "default "]);
            let t = if line.trim_start().contains("const fn") && !t.starts_with("fn ") { &format!("fn {t}")[..] } else { t };
            for (kw, kind) in [("fn ", "fn"), ("struct ", "struct"), ("enum ", "enum"), ("trait ", "trait"), ("union ", "union"), ("type ", "type"), ("macro_rules! ", "macro")] {
                if let Some(r) = t.strip_prefix(kw) {
                    return named(kind, r).map(|(k, n)| (k, n));
                }
            }
            if let Some(r) = t.strip_prefix("mod ") {
                if t.ends_with('{') {
                    return named("mod", r);
                }
            }
            if t.starts_with("impl") && (t.starts_with("impl ") || t.starts_with("impl<")) && indent == 0 {
                let head = t.trim_end_matches('{').trim();
                let name: String = head.chars().take(80).collect();
                return Some(("impl".into(), name));
            }
            None
        }
        "python" if indent <= 4 => {
            if let Some(r) = t.strip_prefix("async def ").or_else(|| t.strip_prefix("def ")) {
                return named("def", r);
            }
            t.strip_prefix("class ").and_then(|r| named("class", r))
        }
        "typescript" | "javascript" if indent <= 4 => {
            let t = strip_any(t, &["export ", "default ", "declare ", "abstract ", "async "]);
            if let Some(r) = t.strip_prefix("function* ").or_else(|| t.strip_prefix("function ")) {
                return named("function", r);
            }
            for (kw, kind) in [("class ", "class"), ("interface ", "interface"), ("enum ", "enum")] {
                if let Some(r) = t.strip_prefix(kw) {
                    return named(kind, r);
                }
            }
            if let Some(r) = t.strip_prefix("type ") {
                if r.contains('=') {
                    return named("type", r);
                }
            }
            if indent == 0 {
                if let Some(r) = t.strip_prefix("const ").or_else(|| t.strip_prefix("let ")) {
                    let rest = r.split_once('=').map(|(_, v)| v.trim()).unwrap_or("");
                    if rest.contains("=>") || rest.starts_with("function") || rest.starts_with("async") || rest.starts_with("React.memo") || rest.starts_with("memo(") {
                        return named("function", r);
                    }
                }
            }
            None
        }
        "go" if indent == 0 => {
            if let Some(r) = t.strip_prefix("func ") {
                let r = if r.starts_with('(') { r.split_once(')').map(|(_, x)| x).unwrap_or("") } else { r };
                return named("func", r);
            }
            t.strip_prefix("type ").and_then(|r| named("type", r))
        }
        _ => None,
    }
}

/// Lines that belong to the definition after them: doc comments,
/// attributes, decorators.
fn attached(line: &str, lang: &str) -> bool {
    let t = line.trim_start();
    if t.is_empty() {
        return false;
    }
    match lang {
        "rust" => t.starts_with("///") || t.starts_with("//!") || t.starts_with("#[") || t.starts_with("//"),
        "python" => t.starts_with('@') || t.starts_with('#'),
        "typescript" | "javascript" => t.starts_with("/**") || t.starts_with("* ") || t == "*" || t.starts_with("*/") || t.starts_with("//") || t.starts_with('@'),
        "go" => t.starts_with("//"),
        _ => false,
    }
}

/// A file's chunks: a chunk per definition (with its doc comments), the
/// lines before the first one as `head` (joined to the first when short),
/// long ones cut at 60 lines; without definitions, 60 lines each.
pub fn chunk(text: &str, lang: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    // Starts of definitions, walked back over what is attached to them.
    let mut starts: Vec<(usize, String, String)> = Vec::new();
    let mut in_fence = false;
    for (i, l) in lines.iter().enumerate() {
        if lang == "markdown" && l.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if in_fence {
            continue;
        }
        if let Some((kind, name)) = def_of(l, lang) {
            let mut s = i;
            while s > 0 && attached(lines[s - 1], lang) && starts.last().is_none_or(|p| p.0 < s - 1) {
                s -= 1;
            }
            if starts.last().is_none_or(|p| p.0 < s) {
                starts.push((s, kind, name));
            }
        }
    }
    let mut raw: Vec<(usize, usize, Option<String>, String)> = Vec::new();
    if starts.is_empty() {
        let mut s = 0;
        while s < lines.len() {
            let e = (s + WINDOW).min(lines.len());
            raw.push((s, e, None, "lines".into()));
            s = e;
        }
    } else {
        if starts[0].0 > 0 {
            raw.push((0, starts[0].0, None, "head".into()));
        }
        for (k, (s, kind, name)) in starts.iter().enumerate() {
            let e = starts.get(k + 1).map(|n| n.0).unwrap_or(lines.len());
            raw.push((*s, e, Some(name.clone()), kind.clone()));
        }
    }
    // Long ones cut; short ones joined to the next.
    let mut cut: Vec<(usize, usize, Option<String>, String)> = Vec::new();
    for (s, e, sym, kind) in raw {
        if e - s > MAX_CHUNK {
            let mut a = s;
            while a < e {
                let b = if e - a > MAX_CHUNK { a + WINDOW } else { e };
                cut.push((a, b, sym.clone(), kind.clone()));
                a = b;
            }
        } else {
            cut.push((s, e, sym, kind));
        }
    }
    let solid = |s: usize, e: usize| lines[s..e].iter().filter(|l| !l.trim().is_empty()).count();
    let mut joined: Vec<(usize, usize, Option<String>, String)> = Vec::new();
    for c in cut {
        if let Some(last) = joined.last_mut() {
            // A short head joins the definition after it; two named
            // definitions stay apart, so each keeps its symbol.
            if last.1 == c.0 && last.2.is_none() && solid(last.0, last.1) < 4 && c.1 - last.0 <= WINDOW {
                if last.2.is_none() {
                    last.2 = c.2.clone();
                    last.3 = c.3.clone();
                }
                last.1 = c.1;
                continue;
            }
        }
        joined.push(c);
    }
    let mut out = Vec::new();
    for (s, mut e, sym, kind) in joined {
        while e > s && lines[e - 1].trim().is_empty() {
            e -= 1;
        }
        let mut s2 = s;
        while s2 < e && lines[s2].trim().is_empty() {
            s2 += 1;
        }
        if s2 >= e {
            continue;
        }
        let text: String = lines[s2..e].iter().map(|l| if l.len() > 400 { cut_at(l, 400) } else { l }).collect::<Vec<_>>().join("\n");
        out.push(Chunk { symbol: sym, kind, start: s2 + 1, end: e, text });
    }
    out
}

fn cut_at(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The parts of an identifier: `fetchCartTotal` → fetch, cart, total;
/// `HTTPServer` → http, server; `cart_total` → cart, total.
pub fn split_ident(id: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for piece in id.split(|c: char| !c.is_alphanumeric()) {
        let cs: Vec<char> = piece.chars().collect();
        let mut cur = String::new();
        for (i, &c) in cs.iter().enumerate() {
            let prev = if i > 0 { Some(cs[i - 1]) } else { None };
            let next = cs.get(i + 1).copied();
            let boundary = match prev {
                Some(p) => (p.is_lowercase() || p.is_ascii_digit()) && c.is_uppercase() || p.is_uppercase() && c.is_uppercase() && next.is_some_and(|n| n.is_lowercase()),
                None => false,
            };
            if boundary && !cur.is_empty() {
                parts.push(cur.to_lowercase());
                cur.clear();
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            parts.push(cur.to_lowercase());
        }
    }
    parts
}

/// The terms a text adds beyond its words: each compound identifier's
/// parts and its parts joined (`cartTotal` → cart total; `cart_total` →
/// carttotal), once each.
pub fn ident_terms(text: &str) -> String {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for tok in text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|t| t.len() > 2) {
        let parts = split_ident(tok);
        if parts.len() < 2 {
            continue;
        }
        let joined = parts.concat();
        for p in parts.into_iter().chain(std::iter::once(joined)) {
            if p.len() > 1 && seen.insert(p.clone()) {
                out.push(p);
            }
        }
    }
    out.join(" ")
}

/// A query's text for the three fields: its words, plus each compound
/// identifier's parts and its parts joined.
pub fn query_text(q: &str) -> String {
    let extra = ident_terms(q);
    if extra.is_empty() {
        q.to_string()
    } else {
        format!("{q} {extra}")
    }
}

// ---------------------------------------------------------------- files

/// The files of a checkout worth reading: tracked ones and new ones not
/// ignored, without lock files, minified bundles, vendored folders and
/// the like.
pub fn list_files(dir: &Path) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git ls-files failed in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    let mut seen = HashSet::new();
    let mut files: Vec<String> = out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).to_string()).filter(|p| worth(p) && seen.insert(p.clone())).collect();
    files.sort();
    files.truncate(MAX_FILES);
    Ok(files)
}

/// Whether a path is source worth indexing.
pub fn worth(path: &str) -> bool {
    const SKIP_DIRS: [&str; 10] = ["node_modules/", "vendor/", "dist/", "build/", "target/", ".git/", "__pycache__/", ".venv/", ".next/", "coverage/"];
    const SKIP_NAMES: [&str; 8] = ["Cargo.lock", "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "poetry.lock", "go.sum", "uv.lock", "bun.lockb"];
    const SKIP_EXT: [&str; 24] = ["png", "jpg", "jpeg", "gif", "webp", "ico", "svg", "pdf", "zip", "gz", "tgz", "woff", "woff2", "ttf", "otf", "mp4", "mp3", "wasm", "so", "dylib", "a", "bin", "lock", "map"];
    let name = path.rsplit('/').next().unwrap_or(path);
    if SKIP_NAMES.contains(&name) || name.ends_with(".min.js") || name.ends_with(".min.css") {
        return false;
    }
    if SKIP_DIRS.iter().any(|d| path.starts_with(d) || path.contains(&format!("/{d}"))) {
        return false;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    !SKIP_EXT.contains(&ext.as_str())
}

/// A file's signature (size and modification time), or `None` when it is
/// gone, not a file, or too big.
fn sig_of(p: &Path) -> Option<(String, u64)> {
    let m = std::fs::metadata(p).ok()?;
    if !m.is_file() || m.len() > MAX_FILE {
        return None;
    }
    let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    Some((format!("{}:{t}", m.len()), m.len()))
}

/// A file's text, unless it is binary.
fn read_text(p: &Path) -> Option<String> {
    let bytes = std::fs::read(p).ok()?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).to_string())
}

// ---------------------------------------------------------- the index

/// What the server keeps of a repository's index beside its rows.
#[derive(Default)]
struct RepoState {
    indexing: bool,
    done: usize,
    total: usize,
    last_sync: Option<Instant>,
    recent: VecDeque<Json>,
    error: Option<String>,
}

fn states() -> &'static Mutex<HashMap<String, RepoState>> {
    static S: OnceLock<Mutex<HashMap<String, RepoState>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Writes to the index, one at a time.
fn write_lock() -> &'static Mutex<()> {
    static W: OnceLock<Mutex<()>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(()))
}

fn with_state<T>(repo: &str, f: impl FnOnce(&mut RepoState) -> T) -> T {
    let mut s = states().lock().unwrap();
    f(s.entry(repo.to_string()).or_default())
}

fn remember(repo: &str, item: Json) {
    with_state(repo, |s| {
        s.recent.push_front(item);
        s.recent.truncate(40);
    });
}

fn meta_key(repo: &str) -> String {
    format!("search:index:{repo}")
}

/// The id of a file's row, and the prefix of its chunks'.
fn file_id(repo: &str, tree: &str, path: &str) -> String {
    format!("{}:{tree}:{path}", git::hash8(Path::new(repo)))
}

fn q(sup: &Supervisor, sql: &str, p: &Params) -> Result<Vec<Vec<Value>>, String> {
    sup.db().query(sql, p)
}

fn sv(s: &str) -> Value {
    Value::Str(s.to_string())
}

/// The files of one tree of a repository: path → (signature, deleted).
fn files_of(sup: &Supervisor, repo: &str, tree: &str) -> Result<HashMap<String, (String, bool)>, String> {
    let rows = q(sup, "SELECT path, sig, deleted FROM code_files WHERE repo = :repo AND tree = :tree LIMIT 1000000", &Params::new().set("repo", sv(repo)).set("tree", sv(tree)))?;
    Ok(rows.iter().filter_map(|r| Some((get_str(&r[0])?, (get_str(&r[1]).unwrap_or_default(), matches!(r[2], Value::Bool(true)))))).collect())
}

fn drop_file(sup: &Supervisor, repo: &str, tree: &str, path: &str) -> Result<(), String> {
    let p = Params::new().set("repo", sv(repo)).set("tree", sv(tree)).set("path", sv(path));
    q(sup, "DELETE FROM code_chunks WHERE repo = :repo AND tree = :tree AND path = :path", &p)?;
    q(sup, "DELETE FROM code_files WHERE repo = :repo AND tree = :tree AND path = :path", &p)?;
    Ok(())
}

/// Chunks a file of `dir` again into `tree`: its rows replaced. A file
/// that is gone is a row marked deleted in an agent's tree (it hides the
/// main checkout's), and nothing in `main`. Answers the chunks written.
fn index_file(sup: &Supervisor, repo: &str, tree: &str, dir: &Path, path: &str, had: bool) -> Result<usize, String> {
    let full = dir.join(path);
    let p = Params::new().set("repo", sv(repo)).set("tree", sv(tree)).set("path", sv(path));
    if had {
        q(sup, "DELETE FROM code_chunks WHERE repo = :repo AND tree = :tree AND path = :path", &p)?;
    }
    let lang = lang_of(path);
    let sig = sig_of(&full);
    let text = sig.as_ref().and_then(|_| read_text(&full));
    let chunks = text.as_deref().map(|t| chunk(t, lang)).unwrap_or_default();
    let deleted = !full.exists();
    if deleted && tree == "main" {
        q(sup, "DELETE FROM code_files WHERE repo = :repo AND tree = :tree AND path = :path", &p)?;
        return Ok(0);
    }
    let fid = file_id(repo, tree, path);
    let path_terms = {
        let parts: Vec<String> = path.split(['/', '.', '-']).flat_map(split_ident).collect();
        parts.join(" ")
    };
    for batch in chunks.chunks(64) {
        let mut params = Params::new();
        let mut values = Vec::new();
        for (i, c) in batch.iter().enumerate() {
            let sym = c.symbol.clone().unwrap_or_default();
            let head = format!("{sym} {} {path_terms}", ident_terms(&sym));
            params = params
                .set(&format!("id{i}"), Value::Str(format!("{fid}#{}", c.start)))
                .set(&format!("st{i}"), Value::Int(c.start as i64))
                .set(&format!("en{i}"), Value::Int(c.end as i64))
                .set(&format!("sy{i}"), c.symbol.clone().map_or(Value::Null, Value::Str))
                .set(&format!("ki{i}"), Value::Str(c.kind.clone()))
                .set(&format!("tx{i}"), Value::Str(c.text.clone()))
                .set(&format!("te{i}"), Value::Str(ident_terms(&c.text)))
                .set(&format!("he{i}"), Value::Str(head));
            values.push(format!("(:id{i}, :repo, :tree, :path, :lang, :sy{i}, :ki{i}, :st{i}, :en{i}, :tx{i}, :te{i}, :he{i})"));
        }
        params = params.set("repo", sv(repo)).set("tree", sv(tree)).set("path", sv(path)).set("lang", sv(lang));
        let sql = format!("INSERT OR REPLACE INTO code_chunks (id, repo, tree, path, lang, symbol, kind, start_line, end_line, text, terms, head) VALUES {}", values.join(", "));
        q(sup, &sql, &params)?;
    }
    let lines = text.as_deref().map(|t| t.lines().count()).unwrap_or(0);
    let fp = Params::new()
        .set("id", Value::Str(fid))
        .set("repo", sv(repo))
        .set("tree", sv(tree))
        .set("path", sv(path))
        .set("lang", sv(lang))
        .set("sig", sig.map_or(Value::Str("gone".into()), |s| Value::Str(s.0)))
        .set("chunks", Value::Int(chunks.len() as i64))
        .set("lines", Value::Int(lines as i64))
        .set("deleted", Value::Bool(deleted))
        .set("ts", ts(now_ms()));
    q(sup, "INSERT OR REPLACE INTO code_files (id, repo, tree, path, lang, sig, chunks, lines, deleted, indexed) VALUES (:id, :repo, :tree, :path, :lang, :sig, :chunks, :lines, :deleted, :ts)", &fp)?;
    Ok(chunks.len())
}

/// The agents of a repository that have a worktree of their own and ran
/// lately: their trees are laid over the main checkout.
fn overlay_agents(sup: &Supervisor, repo: &str) -> Vec<crate::db::Agent> {
    let now = now_ms();
    let mut list: Vec<_> = sup
        .agent_rows()
        .into_iter()
        .filter(|a| a.repo == repo && a.cwd != a.repo && Path::new(&a.cwd).is_dir())
        .filter(|a| sup.is_running(&a.id) || a.ended.is_some_and(|e| now - e < OVERLAY_KEEP_MS))
        .collect();
    list.sort_by_key(|a| a.created);
    list
}

/// The files an agent changed in its worktree from its base, committed or
/// not, new ones included.
fn changed_files(a: &crate::db::Agent) -> Vec<String> {
    let dir = Path::new(&a.cwd);
    let mut out = Vec::new();
    let base = a.base.clone().unwrap_or_else(|| "HEAD".into());
    if let Ok(o) = Command::new("git").arg("-C").arg(dir).args(["diff", "--name-only", "-z", &base]).env("GIT_TERMINAL_PROMPT", "0").output() {
        out.extend(o.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).to_string()));
    }
    if let Ok(o) = Command::new("git").arg("-C").arg(dir).args(["ls-files", "-z", "--others", "--exclude-standard"]).env("GIT_TERMINAL_PROMPT", "0").output() {
        out.extend(o.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).to_string()));
    }
    out.retain(|p| worth(p));
    out.sort();
    out.dedup();
    out
}

/// A full pass over a repository: the main checkout, then each agent's
/// changed files. Only files whose signature changed are chunked again.
pub fn sync(sup: &Supervisor, repo: &str) -> Result<Json, String> {
    let started = Instant::now();
    let main = PathBuf::from(repo);
    let files = list_files(&main)?;
    let _w = write_lock().lock().unwrap();
    let known = files_of(sup, repo, "main")?;
    with_state(repo, |s| {
        s.total = files.len();
        s.done = 0;
    });
    let (mut changed, mut removed, mut read) = (0usize, 0usize, 0u64);
    let listed: HashSet<&String> = files.iter().collect();
    for (i, f) in files.iter().enumerate() {
        if i % 50 == 0 {
            with_state(repo, |s| s.done = i);
        }
        let Some((sig, size)) = sig_of(&main.join(f)) else { continue };
        if known.get(f).is_some_and(|k| k.0 == sig) {
            continue;
        }
        if read + size > MAX_TOTAL {
            continue;
        }
        read += size;
        index_file(sup, repo, "main", &main, f, known.contains_key(f))?;
        changed += 1;
    }
    for path in known.keys().filter(|p| !listed.contains(p)) {
        drop_file(sup, repo, "main", path)?;
        removed += 1;
    }
    // Each agent's worktree: what it changed.
    let agents = overlay_agents(sup, repo);
    let keep: HashSet<String> = agents.iter().map(|a| a.id.clone()).collect();
    for a in &agents {
        let mine = files_of(sup, repo, &a.id)?;
        let now: Vec<String> = changed_files(a);
        let dir = PathBuf::from(&a.cwd);
        for f in &now {
            let sig = sig_of(&dir.join(f)).map(|s| s.0).unwrap_or_else(|| "gone".into());
            if mine.get(f).is_some_and(|k| k.0 == sig) {
                continue;
            }
            index_file(sup, repo, &a.id, &dir, f, mine.contains_key(f))?;
        }
        let now: HashSet<&String> = now.iter().collect();
        for f in mine.keys().filter(|f| !now.contains(f)) {
            drop_file(sup, repo, &a.id, f)?;
        }
    }
    // Trees of agents that are long gone.
    let trees = q(sup, "SELECT tree FROM code_files WHERE repo = :repo AND tree != 'main' LIMIT 1000000", &Params::new().set("repo", sv(repo)))?;
    let gone: HashSet<String> = trees.iter().filter_map(|r| get_str(&r[0])).filter(|t| !keep.contains(t)).collect();
    for t in gone {
        let p = Params::new().set("repo", sv(repo)).set("tree", Value::Str(t));
        q(sup, "DELETE FROM code_chunks WHERE repo = :repo AND tree = :tree", &p)?;
        q(sup, "DELETE FROM code_files WHERE repo = :repo AND tree = :tree", &p)?;
    }
    let took = started.elapsed().as_millis() as i64;
    let (nfiles, nchunks, langs) = totals(sup, repo)?;
    let head = Command::new("git").arg("-C").arg(&main).args(["rev-parse", "--short=12", "HEAD"]).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let meta = json!({ "indexed_ms": now_ms(), "took_ms": took, "head": head, "files": nfiles, "chunks": nchunks, "langs": langs });
    sup.set_mark(&meta_key(repo), &meta.to_string()).map_err(|e| e.message)?;
    if changed > 0 || removed > 0 {
        remember(repo, json!({ "ts_ms": now_ms(), "path": format!("{nfiles} files"), "tree": "main", "agent": null, "chunks": nchunks, "why": "full", "changed": changed, "removed": removed }));
    }
    with_state(repo, |s| {
        s.done = s.total;
        s.last_sync = Some(Instant::now());
        s.error = None;
    });
    Ok(json!({ "changed": changed, "removed": removed, "took_ms": took }))
}

/// The main tree's files, chunks and languages.
fn totals(sup: &Supervisor, repo: &str) -> Result<(i64, i64, Vec<Json>), String> {
    let rows = q(sup, "SELECT lang, chunks FROM code_files WHERE repo = :repo AND tree = 'main' AND deleted = false LIMIT 1000000", &Params::new().set("repo", sv(repo)))?;
    let mut by: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for r in &rows {
        let lang = get_str(&r[0]).unwrap_or_else(|| "other".into());
        let e = by.entry(lang).or_default();
        e.0 += 1;
        e.1 += get_int(&r[1]).unwrap_or(0);
    }
    let files = rows.len() as i64;
    let chunks = by.values().map(|v| v.1).sum();
    let mut langs: Vec<(String, i64, i64)> = by.into_iter().map(|(l, (f, c))| (l, f, c)).collect();
    langs.sort_by(|a, b| (a.0 == "other").cmp(&(b.0 == "other")).then(b.2.cmp(&a.2)));
    Ok((files, chunks, langs.into_iter().map(|(lang, files, chunks)| json!({ "lang": lang, "files": files, "chunks": chunks })).collect()))
}

/// Starts a full pass in the background unless one runs.
fn sync_later(sup: &Arc<Supervisor>, repo: &str) {
    let start = with_state(repo, |s| {
        if s.indexing {
            false
        } else {
            s.indexing = true;
            true
        }
    });
    if !start {
        return;
    }
    let (sup, repo) = (sup.clone(), repo.to_string());
    let spawned = std::thread::Builder::new().name("search-index".into()).spawn({
        let repo = repo.clone();
        move || {
            let r = sync(&sup, &repo);
            with_state(&repo, |s| {
                s.indexing = false;
                s.last_sync = Some(Instant::now());
                if let Err(e) = r {
                    s.error = Some(e);
                }
            });
        }
    });
    if spawned.is_err() {
        with_state(&repo, |s| s.indexing = false);
    }
}

/// Makes sure a repository has an index: the first time, a pass that a
/// search waits for (up to 20 s); later, one in the background when the
/// last is a minute old. Edits since are caught up first.
fn ensure(sup: &Arc<Supervisor>, repo: &str, wait: bool) {
    catch_up(sup);
    let has = sup.mark(&meta_key(repo)).is_some();
    let stale = with_state(repo, |s| s.last_sync.is_none_or(|t| t.elapsed() > RESYNC));
    if !has || stale {
        sync_later(sup, repo);
    }
    if !has && wait {
        let end = Instant::now() + FIRST_WAIT;
        while Instant::now() < end && with_state(repo, |s| s.indexing) {
            std::thread::sleep(Duration::from_millis(40));
        }
    }
}

/// Reindexes the files that agents' edit calls named since the last time
/// (the mark `search:seq`), each in its agent's tree. Repositories without
/// an index are left for their first pass.
pub fn catch_up(sup: &Supervisor) {
    let _w = write_lock().lock().unwrap();
    let last = match sup.mark("search:seq").and_then(|v| v.parse::<i64>().ok()) {
        Some(s) => s,
        None => {
            let s = sup.db().last_seq().unwrap_or(0);
            let _ = sup.set_mark("search:seq", &s.to_string());
            return;
        }
    };
    let rows = match q(sup, "SELECT seq, agent, tool, data FROM events WHERE seq > :last AND kind = 'PostToolUse' ORDER BY seq ASC LIMIT 5000", &Params::new().set("last", Value::Int(last))) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut top = last;
    let mut agents: HashMap<String, Option<crate::db::Agent>> = HashMap::new();
    let mut indexed: HashSet<(String, String, String)> = HashSet::new();
    for r in &rows {
        top = top.max(get_int(&r[0]).unwrap_or(top));
        let tool = get_str(&r[2]).unwrap_or_default();
        if !EDIT_TOOLS.contains(&tool.as_str()) {
            continue;
        }
        let id = get_str(&r[1]).unwrap_or_default();
        let a = agents.entry(id.clone()).or_insert_with(|| sup.agent_row(&id)).clone();
        let Some(a) = a else { continue };
        if sup.mark(&meta_key(&a.repo)).is_none() {
            continue;
        }
        let ev = crate::db::Event { data: get_str(&r[3]), ..Default::default() };
        let Some(fp) = map::tool_file(&ev) else { continue };
        let (cwd, main) = (PathBuf::from(&a.cwd), PathBuf::from(&a.repo));
        let Some(rel) = map::rel_path(&fp, &cwd, &main) else { continue };
        if !worth(&rel) {
            continue;
        }
        let tree = if a.cwd == a.repo { "main".to_string() } else { a.id.clone() };
        if !indexed.insert((a.repo.clone(), tree.clone(), rel.clone())) {
            continue;
        }
        let dir = if tree == "main" { main } else { cwd };
        let had = files_of(sup, &a.repo, &tree).map(|m| m.contains_key(&rel)).unwrap_or(true);
        if let Ok(n) = index_file(sup, &a.repo, &tree, &dir, &rel, had) {
            let gone = !dir.join(&rel).exists();
            remember(&a.repo, json!({ "ts_ms": now_ms(), "path": rel, "tree": tree, "agent": {"id": a.id, "name": a.name}, "chunks": n, "why": if gone { "removed" } else { "edit" } }));
        }
    }
    if top > last {
        let _ = sup.set_mark("search:seq", &top.to_string());
    }
}

/// Watches the stream of changes: an edit tool's `PostToolUse` reindexes
/// its file a moment later. The thread ends with the supervisor.
fn watch(sup: &Arc<Supervisor>) {
    let mut rx = sup.changes.subscribe();
    let weak: Weak<Supervisor> = Arc::downgrade(sup);
    let _ = std::thread::Builder::new().name("search-watch".into()).spawn(move || loop {
        let is_edit = |c: &Change| matches!(c, Change::Event { json, .. } if json["kind"] == "PostToolUse" && json["tool"].as_str().is_some_and(|t| EDIT_TOOLS.contains(&t)));
        match rx.blocking_recv() {
            Ok(c) if is_edit(&c) => {
                // Let a burst of edits arrive, then catch up once.
                std::thread::sleep(Duration::from_millis(250));
                while let Ok(_c) = rx.try_recv() {}
                let Some(sup) = weak.upgrade() else { return };
                catch_up(&sup);
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
        if weak.strong_count() == 0 {
            return;
        }
    });
}

/// Sixth build (src/repos.rs): a repository opened without an agent is
/// indexed in the background now, and its index answered as it stands.
pub fn open_repo(sup: &Arc<Supervisor>, repo: &str) -> Json {
    ensure(sup, repo, false);
    index_json(sup, repo)
}

/// The index as the API answers it.
pub(crate) fn index_json(sup: &Supervisor, repo: &str) -> Json {
    let meta: Json = sup.mark(&meta_key(repo)).and_then(|m| serde_json::from_str(&m).ok()).unwrap_or(Json::Null);
    let (indexing, done, total, error) = with_state(repo, |s| (s.indexing, s.done, s.total, s.error.clone()));
    let state = if indexing && meta.is_null() { "indexing" } else if meta.is_null() { "none" } else { "ready" };
    // Overlays: each agent's tree.
    let mut by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Ok(rows) = q(sup, "SELECT tree, path FROM code_files WHERE repo = :repo AND tree != 'main' LIMIT 100000", &Params::new().set("repo", sv(repo))) {
        for r in rows {
            if let (Some(t), Some(p)) = (get_str(&r[0]), get_str(&r[1])) {
                by.entry(t).or_default().push(p);
            }
        }
    }
    let overlays: Vec<Json> = by
        .into_iter()
        .filter_map(|(t, mut files)| {
            let a = sup.agent_row(&t)?;
            files.sort();
            Some((a.created, json!({ "agent": {"id": a.id, "name": a.name}, "files": files })))
        })
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect();
    json!({
        "state": state,
        "files": meta["files"].as_i64().unwrap_or(0),
        "chunks": meta["chunks"].as_i64().unwrap_or(0),
        "langs": meta.get("langs").cloned().unwrap_or(json!([])),
        "indexed_ms": meta["indexed_ms"],
        "took_ms": meta["took_ms"],
        "head": meta["head"],
        "progress": if indexing { json!({ "done": done, "total": total }) } else { Json::Null },
        "overlays": overlays,
        "error": error,
    })
}

// --------------------------------------------------------------- search

/// One search's options.
pub struct Ask<'a> {
    pub repo: &'a str,
    pub q: &'a str,
    pub agent: Option<&'a str>,
    pub lang: Option<&'a str>,
    pub limit: usize,
    pub tokens: Option<i64>,
}

const RANK: &str = "bm25(text, :q, syntax => 'plain') + bm25(terms, :q, syntax => 'plain') + 2 * bm25(head, :q, syntax => 'plain')";

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The `WHERE` of a search: the main tree, with the files an agent's tree
/// holds left out, and that tree.
fn scope(sup: &Supervisor, a: &Ask) -> Result<String, String> {
    let mut w = String::from("repo = :repo AND ");
    match a.agent {
        Some(id) => {
            let mine = files_of(sup, a.repo, id)?;
            if mine.is_empty() {
                w.push_str("tree = 'main'");
            } else {
                let list: Vec<String> = mine.keys().map(|p| lit(p)).collect();
                w.push_str(&format!("(tree = :agent OR (tree = 'main' AND path NOT IN ({})))", list.join(", ")));
            }
        }
        None => w.push_str("tree = 'main'"),
    }
    if a.lang.is_some() {
        w.push_str(" AND lang = :lang");
    }
    Ok(w)
}

fn params(a: &Ask) -> Params {
    let mut p = Params::new().set("repo", sv(a.repo)).set("q", Value::Str(query_text(a.q)));
    if let Some(id) = a.agent {
        p = p.set("agent", sv(id));
    }
    if let Some(l) = a.lang {
        p = p.set("lang", sv(l));
    }
    p
}

/// The hits of a search, best first, and with `tokens` the packed text.
pub fn search(sup: &Supervisor, a: &Ask) -> Result<(Vec<Json>, Option<Json>), String> {
    if a.q.trim().is_empty() {
        return Ok((Vec::new(), None));
    }
    let w = scope(sup, a)?;
    let p = params(a);
    let sql = format!("SELECT id, tree, path, lang, symbol, kind, start_line, end_line, text, score() FROM code_chunks WHERE {w} ORDER BY {RANK} DESC LIMIT {}", a.limit.clamp(1, 200));
    let rows = q(sup, &sql, &p)?;
    // Which agents changed each path (for hits of the main tree).
    let mut over: HashMap<String, Vec<Json>> = HashMap::new();
    if let Ok(rs) = q(sup, "SELECT tree, path FROM code_files WHERE repo = :repo AND tree != 'main' LIMIT 100000", &Params::new().set("repo", sv(a.repo))) {
        for r in rs {
            if let (Some(t), Some(path)) = (get_str(&r[0]), get_str(&r[1])) {
                if let Some(ag) = sup.agent_row(&t) {
                    over.entry(path).or_default().push(json!({"id": ag.id, "name": ag.name}));
                }
            }
        }
    }
    let hits: Vec<Json> = rows
        .iter()
        .map(|r| {
            let tree = get_str(&r[1]).unwrap_or_else(|| "main".into());
            let path = get_str(&r[2]).unwrap_or_default();
            let edited = (tree != "main").then(|| sup.agent_row(&tree).map(|ag| json!({"id": ag.id, "name": ag.name}))).flatten();
            let overlaid: Vec<Json> = if tree == "main" { over.get(&path).cloned().unwrap_or_default() } else { Vec::new() };
            let score = match &r[9] {
                Value::Float(f) => (*f * 100.0).round() / 100.0,
                v => get_int(v).unwrap_or(0) as f64,
            };
            json!({
                "id": get_str(&r[0]),
                "tree": tree,
                "path": path,
                "lang": get_str(&r[3]),
                "symbol": get_str(&r[4]),
                "kind": get_str(&r[5]),
                "start_line": get_int(&r[6]),
                "end_line": get_int(&r[7]),
                "text": get_str(&r[8]),
                "score": score,
                "edited_by": edited,
                "overlaid_by": overlaid,
            })
        })
        .collect();
    let hits = definitions_first(hits, a.q);
    let pack = match a.tokens {
        Some(n) if n > 0 => Some(pack(sup, a, &w, &p, n.clamp(100, 50_000))?),
        _ => None,
    };
    Ok((hits, pack))
}

/// Whether a path is a test file (`test_x.py`, `x_test.go`, `tests/`, `x.spec.ts`).
pub fn is_test_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_lowercase();
    let stem = name.split('.').next().unwrap_or("");
    stem.starts_with("test_") || stem.ends_with("_test") || stem.ends_with("_spec") || name.contains(".test.") || name.contains(".spec.") || path.split('/').any(|d| matches!(d, "tests" | "test" | "__tests__" | "spec"))
}

/// The definition a query names comes before the tests that use it (Lee:
/// `test_fmt` ranked above `def fmt`): a hit whose symbol is a word of the
/// query, outside the tests, moves to the front; the rest keep BM25's order.
pub fn definitions_first(hits: Vec<Json>, q: &str) -> Vec<Json> {
    let words: HashSet<String> = q.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    let names = |h: &Json| -> bool {
        let sym = h["symbol"].as_str().unwrap_or("").to_lowercase();
        let last = sym.rsplit(['.', ' ', ':']).next().unwrap_or("").to_string();
        !sym.is_empty() && (words.contains(&sym) || words.contains(&last)) && !is_test_path(h["path"].as_str().unwrap_or(""))
    };
    let (mut first, rest): (Vec<Json>, Vec<Json>) = hits.into_iter().partition(names);
    first.extend(rest);
    first
}

/// `PACK n TOKENS` on the same ranking: the one text an agent gets.
fn pack(sup: &Supervisor, a: &Ask, w: &str, p: &Params, budget: i64) -> Result<Json, String> {
    let sql = format!("SELECT id, path, start_line, end_line, symbol, kind, text FROM code_chunks WHERE {w} ORDER BY {RANK} DESC PACK {budget} TOKENS OF text WITH RECEIPT LIMIT 400");
    let rows = q(sup, &sql, p)?;
    let _ = a;
    let mut used = 0i64;
    let mut parts = Vec::new();
    let mut list = Vec::new();
    for r in &rows {
        let path = get_str(&r[1]).unwrap_or_default();
        let (start, end) = (get_int(&r[2]).unwrap_or(0), get_int(&r[3]).unwrap_or(0));
        let symbol = get_str(&r[4]);
        let kind = get_str(&r[5]).unwrap_or_default();
        let tokens = r.get(7).and_then(get_int).unwrap_or(0);
        let decision = r.get(8).and_then(get_str).unwrap_or_else(|| "take".into());
        if !decision.starts_with("skip") {
            used += tokens;
            let head = match &symbol {
                Some(s) => format!("// {path}:{start}-{end} {kind} {s}"),
                None => format!("// {path}:{start}-{end}"),
            };
            parts.push(format!("{head}\n{}", get_str(&r[6]).unwrap_or_default()));
        }
        list.push(json!({ "path": path, "start_line": start, "end_line": end, "symbol": symbol, "decision": decision, "tokens": tokens }));
    }
    Ok(json!({ "budget": budget, "used": used, "text": parts.join("\n\n"), "rows": list }))
}

/// Records a search an agent made (`by`), for the Library's feed.
pub fn log(sup: &Supervisor, repo: &str, by: &str, kind: &str, q_: &str, hits: usize, tokens: i64, cites: &[String]) {
    let p = Params::new()
        .set("id", Value::Str(format!("s{}", crate::supervisor::hex(10))))
        .set("ts", ts(now_ms()))
        .set("repo", sv(repo))
        .set("agent", sv(by))
        .set("kind", sv(kind))
        .set("q", sv(q_))
        .set("hits", Value::Int(hits as i64))
        .set("tokens", Value::Int(tokens))
        .set("cites", Value::Str(json!(cites).to_string()));
    let _ = q(sup, "INSERT INTO search_log (id, ts, repo, agent, kind, q, hits, tokens, cites) VALUES (:id, :ts, :repo, :agent, :kind, :q, :hits, :tokens, :cites)", &p);
}

fn searches(sup: &Supervisor, repo: &str, limit: usize) -> Vec<Json> {
    let rows = q(sup, &format!("SELECT ts, agent, kind, q, hits, tokens, cites FROM search_log WHERE repo = :repo ORDER BY ts DESC LIMIT {limit}"), &Params::new().set("repo", sv(repo))).unwrap_or_default();
    rows.iter()
        .map(|r| {
            let agent = get_str(&r[1]).and_then(|id| sup.agent_row(&id).map(|a| json!({"id": a.id, "name": a.name})).or(Some(json!({"id": id, "name": id}))));
            json!({
                "ts_ms": get_ms(&r[0]),
                "agent": agent,
                "kind": get_str(&r[2]),
                "q": get_str(&r[3]),
                "hits": get_int(&r[4]).unwrap_or(0),
                "tokens": get_int(&r[5]).unwrap_or(0),
                "cites": get_str(&r[6]).and_then(|c| serde_json::from_str::<Json>(&c).ok()).unwrap_or(json!([])),
            })
        })
        .collect()
}

// ---------------------------------------------------------------- routes

fn repo_of(repo: Option<&str>) -> Result<String, ApiError> {
    Ok(trail::repo_dir(repo)?.to_string_lossy().to_string())
}

#[derive(Deserialize)]
pub struct CodeQ {
    pub repo: Option<String>,
    pub q: Option<String>,
    pub agent: Option<String>,
    pub lang: Option<String>,
    pub limit: Option<usize>,
    pub tokens: Option<i64>,
    pub by: Option<String>,
}

/// `GET /api/search/code`.
pub fn code(sup: &Arc<Supervisor>, cq: CodeQ) -> Result<Json, ApiError> {
    let started = Instant::now();
    // Without repo, the repository of the agent searching.
    let hint = cq.repo.clone().filter(|r| !r.is_empty()).or_else(|| cq.agent.as_deref().or(cq.by.as_deref()).and_then(|id| sup.agent_row(id)).map(|a| a.repo));
    let repo = repo_of(hint.as_deref())?;
    let text = cq.q.unwrap_or_default();
    let agent = cq.agent.filter(|a| !a.is_empty());
    if let Some(id) = &agent {
        let a = sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
        if a.repo != repo {
            return Err(ApiError::bad(format!("agent {id} works in {}, not {repo}", a.repo)));
        }
    }
    ensure(sup, &repo, true);
    // An agent searching as itself, without a worktree of its own, sees the main tree.
    let tree_agent = agent.as_deref().filter(|id| sup.agent_row(id).is_some_and(|a| a.cwd != a.repo));
    let ask = Ask { repo: &repo, q: &text, agent: tree_agent, lang: cq.lang.as_deref().filter(|l| !l.is_empty()), limit: cq.limit.unwrap_or(20), tokens: cq.tokens };
    let mut missing = Vec::new();
    let (hits, pack) = match search(sup, &ask) {
        Ok(r) => r,
        Err(e) => {
            missing.push(format!("the search failed: {e}"));
            (Vec::new(), None)
        }
    };
    if let Some(by) = cq.by.as_deref().filter(|b| !b.is_empty()) {
        let cites: Vec<String> = hits.iter().take(10).map(|h| format!("{}:{}-{}", h["path"].as_str().unwrap_or(""), h["start_line"], h["end_line"])).collect();
        log(sup, &repo, by, "code", &text, hits.len(), pack.as_ref().and_then(|p| p["used"].as_i64()).unwrap_or(0), &cites);
    }
    let index = index_json(sup, &repo);
    if index["state"] == "indexing" {
        missing.push("The repository is still being indexed: these are the files read so far.".into());
    }
    Ok(json!({
        "repo": repo,
        "q": text,
        "agent": agent.as_deref().and_then(|id| sup.agent_json_of(id)),
        "hits": hits,
        "pack": pack,
        "index": index,
        "took_ms": started.elapsed().as_millis() as i64,
        "missing": missing,
    }))
}

async fn code_route(State(app): State<App>, Query(cq): Query<CodeQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || code(&sup, cq)).await?).into_response())
}

#[derive(Deserialize)]
struct RepoQ {
    repo: Option<String>,
    q: Option<String>,
}

async fn index_route(State(app): State<App>, Query(rq): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    let v = blocking(move || {
        let repo = repo_of(rq.repo.as_deref())?;
        ensure(&sup, &repo, false);
        let recent: Vec<Json> = with_state(&repo, |s| s.recent.iter().cloned().collect());
        let main = PathBuf::from(&repo);
        let agents: Vec<Json> = map::repo_agents(&sup, &main).iter().filter_map(|a| sup.agent_json_of(&a.id)).collect();
        Ok(json!({ "repo": repo, "index": index_json(&sup, &repo), "recent": recent, "searches": searches(&sup, &repo, 30), "agents": agents, "missing": [] }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

#[derive(Deserialize)]
struct ReindexB {
    repo: Option<String>,
}

async fn reindex_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: ReindexB = parse_body(&body)?;
    let sup = app.sup.clone();
    let v = blocking(move || {
        let repo = repo_of(b.repo.as_deref())?;
        catch_up(&sup);
        // Wait for a pass that runs, then run one.
        let end = Instant::now() + Duration::from_secs(120);
        while with_state(&repo, |s| s.indexing) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(40));
        }
        with_state(&repo, |s| s.indexing = true);
        let r = sync(&sup, &repo);
        with_state(&repo, |s| {
            s.indexing = false;
            s.last_sync = Some(Instant::now());
        });
        let r = r.map_err(ApiError::internal)?;
        Ok(json!({ "index": index_json(&sup, &repo), "changed": r["changed"], "removed": r["removed"], "took_ms": r["took_ms"] }))
    })
    .await?;
    Ok(AxJson(v).into_response())
}

/// `GET /api/search/quick`: the top code hits, doc pages and ChloeCtx
/// decisions for a few words.
pub fn quick(sup: &Arc<Supervisor>, repo: Option<&str>, text: &str) -> Result<Json, ApiError> {
    let repo = repo_of(repo)?;
    let mut missing = Vec::new();
    if text.trim().is_empty() {
        return Ok(json!({ "repo": repo, "q": text, "code": [], "docs": [], "decisions": [], "missing": [] }));
    }
    ensure(sup, &repo, true);
    let ask = Ask { repo: &repo, q: text, agent: None, lang: None, limit: 6, tokens: None };
    let code = search(sup, &ask).map(|r| r.0).unwrap_or_else(|e| {
        missing.push(format!("code search failed: {e}"));
        Vec::new()
    });
    let docs = crate::library::search_pages(sup, &repo, text, None, 5).unwrap_or_else(|e| {
        missing.push(format!("docs search failed: {e}"));
        Vec::new()
    });
    // Decisions: ChloeCtx's notes of the repository, by the words they share.
    let words: Vec<String> = split_ident(text).into_iter().filter(|w| w.len() > 2).collect();
    let mut decisions = Vec::new();
    match trail::decisions(sup, Some(&repo), 300) {
        Ok(d) => {
            for m in d["missing"].as_array().into_iter().flatten().filter_map(Json::as_str) {
                missing.push(m.to_string());
            }
            for x in d["decisions"].as_array().into_iter().flatten() {
                if x["superseded"] == true {
                    continue;
                }
                let hay: HashSet<String> = split_ident(x["text"].as_str().unwrap_or("")).into_iter().chain(x["files"].as_array().into_iter().flatten().filter_map(Json::as_str).flat_map(|f| f.split(['/', '.']).flat_map(split_ident).collect::<Vec<_>>())).collect();
                let score = words.iter().filter(|w| hay.contains(*w) || hay.iter().any(|h| h.len() > 3 && h.starts_with(w.as_str()))).count();
                if score > 0 {
                    let mut x = x.clone();
                    x["score"] = json!(score);
                    decisions.push(x);
                }
            }
            decisions.sort_by(|a, b| b["score"].as_u64().cmp(&a["score"].as_u64()));
            decisions.truncate(5);
        }
        Err(e) => missing.push(e.message),
    }
    Ok(json!({ "repo": repo, "q": text, "code": code, "docs": docs, "decisions": decisions, "missing": missing }))
}

async fn quick_route(State(app): State<App>, Query(rq): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || quick(&sup, rq.repo.as_deref(), rq.q.as_deref().unwrap_or(""))).await?).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_split_both_ways() {
        assert_eq!(split_ident("fetchCartTotal"), ["fetch", "cart", "total"]);
        assert_eq!(split_ident("HTTPServer_v2"), ["http", "server", "v2"]);
        assert_eq!(ident_terms("let x = cart_total(cartTotal);"), "cart total carttotal");
        assert_eq!(query_text("cartTotal"), "cartTotal cart total carttotal");
        assert_eq!(query_text("total"), "total");
    }

    #[test]
    fn chunks_follow_definitions_with_their_comments() {
        let src = "use std::io;\n\n/// Adds.\n#[inline]\npub fn add(a: i32) -> i32 {\n    a + 1\n}\n\npub struct Cart {\n    lines: Vec<u8>,\n}\n\nimpl Cart {\n    pub fn total(&self) -> i64 {\n        0\n    }\n}\n";
        let c = chunk(src, "rust");
        let got: Vec<(Option<&str>, &str, usize, usize)> = c.iter().map(|c| (c.symbol.as_deref(), c.kind.as_str(), c.start, c.end)).collect();
        // A method is a chunk of its own, and so is its impl's line.
        assert_eq!(got, [(Some("add"), "fn", 1, 7), (Some("Cart"), "struct", 9, 11), (Some("impl Cart"), "impl", 13, 13), (Some("total"), "fn", 14, 17)], "{c:#?}");
        assert!(c[0].text.starts_with("use std::io;"), "a short head joins the next chunk");
        let py = "import os\nimport sys\nimport re\nimport json\n\n@app.get('/')\ndef home():\n    return 1\n\nclass Shop:\n    def total(self):\n        return 2\n";
        let c = chunk(py, "python");
        let syms: Vec<_> = c.iter().map(|c| (c.symbol.clone(), c.start)).collect();
        assert_eq!(syms, [(None, 1), (Some("home".into()), 6), (Some("Shop".into()), 10), (Some("total".into()), 11)]);
        let ts = "export const cartTotal = (lines: Line[]) => {\n  return 0;\n};\nexport function Summary() {\n  return null;\n}\n";
        let c = chunk(ts, "typescript");
        assert_eq!(c.iter().map(|c| c.symbol.clone().unwrap()).collect::<Vec<_>>(), ["cartTotal", "Summary"]);
        let long: String = (0..150).map(|i| format!("line {i}\n")).collect();
        let c = chunk(&long, "other");
        assert_eq!(c.iter().map(|c| (c.start, c.end)).collect::<Vec<_>>(), [(1, 60), (61, 120), (121, 150)]);
        let md = "# Shop\n\nIntro.\n\n## Totals\n\nRounded half up.\n\n```\n# not a heading\n```\n";
        let c = chunk(md, "markdown");
        assert_eq!(c.iter().map(|c| c.symbol.clone().unwrap()).collect::<Vec<_>>(), ["Shop", "Totals"]);
    }

    #[test]
    fn the_definition_comes_before_its_tests() {
        let hits = vec![json!({"path": "test_money.py", "symbol": "test_fmt"}), json!({"path": "web/index.html", "symbol": null}), json!({"path": "money.py", "symbol": "fmt"})];
        assert_eq!(definitions_first(hits, "fmt").iter().map(|h| h["path"].as_str().unwrap().to_string()).collect::<Vec<_>>(), ["money.py", "test_money.py", "web/index.html"]);
        assert!(is_test_path("tests/cart.rs") && is_test_path("web/a.spec.ts") && is_test_path("pkg/x_test.go") && !is_test_path("money.py") && !is_test_path("latest.py"));
    }

    #[test]
    fn what_is_worth_indexing() {
        assert!(worth("src/main.rs"));
        assert!(!worth("web/node_modules/react/index.js"));
        assert!(!worth("Cargo.lock"));
        assert!(!worth("public/app.min.js"));
        assert!(!worth("img/logo.png"));
        assert_eq!(lang_of("web/src/App.tsx"), "typescript");
        assert_eq!(lang_of("Makefile"), "other");
    }
}
