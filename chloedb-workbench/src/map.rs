//! Map: the repository as territory. The tree of its tracked files sized by
//! lines, each agent's changes (its diff from `base`) and what it read in
//! the last ten minutes, the collisions between agents, and ChloeCtx's
//! decisions, preferences and facts pinned to the files they name.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json as AxJson, Router};
use serde_json::{json, Value as Json};

use crate::db::{now_ms, Agent, Event};
use crate::git;
use crate::server::{blocking, ApiResult, App};
use crate::supervisor::{ApiError, Supervisor};
use crate::trail::{self, Ran, Sessions};

/// What `reading` looks back over.
pub const READ_WINDOW_MS: i64 = 10 * 60 * 1000;
/// Folders deeper than this are shown folded, with their size.
const MAX_DEPTH: usize = 3;
/// In a folder, files smaller than this share of it (and than
/// `FOLD_LINES`) fold into one "…" entry, and so do files past the
/// `MAX_FILES` biggest.
const FOLD_SHARE: f64 = 0.05;
const FOLD_LINES: u64 = 50;
const MAX_FILES: usize = 16;
/// Small files fold only while together they are at most this share of
/// their folder: in a small repository most files are small, and folding
/// them hid the repository (sixth build).
const FOLD_MAX_SHARE: f64 = 0.2;
/// A fold lists this many of its files, which the map opens into.
const FOLD_ITEMS: usize = 400;
/// Line counting: files above this are estimated from their size, and so
/// is everything once this much has been read for one tree.
const COUNT_FILE_MAX: u64 = 4 << 20;
const COUNT_BUDGET: u64 = 256 << 20;
/// How long a counted tree is reused while the checkout's HEAD and index
/// stay the same.
const TREE_TTL: Duration = Duration::from_secs(30);
/// Exited agents stay on the map this long after they ended.
const EXITED_KEEP_MS: i64 = 24 * 3600 * 1000;
/// The tools whose calls change a file.
pub const EDIT_TOOLS: [&str; 4] = ["Edit", "MultiEdit", "Write", "NotebookEdit"];

pub fn routes() -> Router<App> {
    Router::new().route("/api/map", get(map_route))
}

#[derive(serde::Deserialize)]
struct MapQ {
    repo: Option<String>,
}

async fn map_route(State(app): State<App>, Query(q): Query<MapQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || map(&sup, q.repo.as_deref())).await?).into_response())
}

// Events.

/// The file a tool event names (`file_path`, `notebook_path` or `path` of
/// its input), as the hook sent it.
pub fn tool_file(ev: &Event) -> Option<String> {
    let d = trail::data_of(ev)?;
    ["file_path", "notebook_path", "path"].iter().find_map(|k| d["tool_input"].get(*k).and_then(Json::as_str).map(str::to_string))
}

/// A path a tool named, relative to the checkout: the agent's worktree, or
/// the repository's main checkout. `None` for a path outside both.
pub fn rel_path(fp: &str, cwd: &Path, main: &Path) -> Option<String> {
    let p = Path::new(fp);
    let rel = if p.is_absolute() {
        p.strip_prefix(cwd).or_else(|_| p.strip_prefix(main)).ok()?.to_path_buf()
    } else {
        PathBuf::from(fp.trim_start_matches("./"))
    };
    if rel.as_os_str().is_empty() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(rel.to_string_lossy().to_string())
}

/// The files an agent's Read calls named since `since_ms`, each with the
/// time it was last read and how many calls read it.
pub fn reads(evs: &[Event], cwd: &Path, main: &Path, since_ms: i64) -> BTreeMap<String, (i64, usize)> {
    let mut out: BTreeMap<String, (i64, usize)> = BTreeMap::new();
    for ev in evs.iter().filter(|e| e.ts >= since_ms && e.kind == "PreToolUse" && e.tool.as_deref() == Some("Read")) {
        if let Some(p) = tool_file(ev).and_then(|f| rel_path(&f, cwd, main)) {
            let e = out.entry(p).or_insert((0, 0));
            e.0 = e.0.max(ev.ts);
            e.1 += 1;
        }
    }
    out
}

/// The files an agent's edit calls named.
pub fn edited(evs: &[Event], cwd: &Path, main: &Path) -> HashSet<String> {
    evs.iter()
        .filter(|e| matches!(e.kind.as_str(), "PreToolUse" | "PostToolUse") && e.tool.as_deref().is_some_and(|t| EDIT_TOOLS.contains(&t)))
        .filter_map(|e| tool_file(e).and_then(|f| rel_path(&f, cwd, main)))
        .collect()
}

// Territory: each agent's work.

/// One agent's changes and reading.
pub struct Work {
    pub agent: Agent,
    /// Its diff from `base`: path, lines added, lines removed, status.
    pub files: Vec<(String, i64, i64, String)>,
    /// What it read in the last ten minutes: path, when last.
    pub reading: Vec<(String, i64)>,
}

impl Work {
    fn changed(&self, path: &str) -> bool {
        self.files.iter().any(|f| f.0 == path)
    }
}

/// The repository's main checkout, from a request's `repo`.
pub fn main_of(repo: &str) -> Result<PathBuf, ApiError> {
    trail::repo_dir(Some(repo))
}

/// The agents of a repository whose work is on the map: running ones, and
/// exited ones that ended in the last day; oldest first.
pub fn repo_agents(sup: &Supervisor, main: &Path) -> Vec<Agent> {
    let repo = main.to_string_lossy().to_string();
    let now = now_ms();
    let mut agents: Vec<Agent> = sup
        .agent_rows()
        .into_iter()
        .filter(|a| a.repo == repo)
        .filter(|a| sup.state(&a.id).is_some_and(|(_, r)| r) || a.ended.is_some_and(|e| now - e < EXITED_KEEP_MS))
        .collect();
    agents.sort_by_key(|a| (a.created, a.id.clone()));
    agents
}

/// The agents of a repository on the map, each with its diff and reading.
pub fn territory(sup: &Supervisor, main: &Path) -> Vec<Work> {
    let now = now_ms();
    let agents = repo_agents(sup, main);
    // Agents that share a folder share its diff: each one keeps the files
    // its own edits named (its Edit and Write calls, and its rows of
    // `file_edits`); a file none of them named (written by a script, a
    // heredoc, a `git merge`) goes to the newest agent of the folder, the
    // one working there now. An agent started again in its worktree is the
    // newest, and keeps the folder's work (sixth build: Maya's cents).
    let mut per_cwd: HashMap<String, Vec<&Agent>> = HashMap::new();
    for a in &agents {
        per_cwd.entry(a.cwd.clone()).or_default().push(a);
    }
    let newest: HashMap<String, String> = per_cwd
        .iter()
        .filter_map(|(cwd, xs)| xs.iter().max_by_key(|a| (sup.is_running(&a.id), a.created)).map(|a| (cwd.clone(), a.id.clone())))
        .collect();
    let mut claims: HashMap<String, HashSet<String>> = HashMap::new();
    for a in agents.iter().filter(|a| per_cwd.get(&a.cwd).is_some_and(|xs| xs.len() > 1)) {
        let cwd = PathBuf::from(&a.cwd);
        let evs = sup.all_events(&a.id).unwrap_or_default();
        let mut mine = edited(&evs, &cwd, main);
        mine.extend(edit_rows(sup, &a.id));
        claims.insert(a.id.clone(), mine);
    }
    let per_cwd: HashMap<String, Vec<String>> = per_cwd.into_iter().map(|(k, xs)| (k, xs.iter().map(|a| a.id.clone()).collect())).collect();
    let mut out = Vec::new();
    for a in agents {
        let cwd = PathBuf::from(&a.cwd);
        if !cwd.is_dir() {
            continue;
        }
        let evs = sup.all_events(&a.id).unwrap_or_default();
        let base = a.base.clone().or_else(|| git::rev_parse(&cwd, "HEAD"));
        let mut files: Vec<(String, i64, i64, String)> = match base.as_deref().map(|b| git::diff(&cwd, b)) {
            Some(Ok(fs)) => fs
                .iter()
                .map(|f| {
                    let (mut add, mut del) = (0, 0);
                    for l in f["hunks"].as_array().into_iter().flatten().flat_map(|h| h["lines"].as_array().into_iter().flatten()) {
                        match l["kind"].as_str() {
                            Some("+") => add += 1,
                            Some("-") => del += 1,
                            _ => {}
                        }
                    }
                    (f["path"].as_str().unwrap_or("").to_string(), add, del, f["status"].as_str().unwrap_or("modified").to_string())
                })
                .collect(),
            _ => Vec::new(),
        };
        if let Some(sharers) = per_cwd.get(&a.cwd).filter(|xs| xs.len() > 1) {
            let empty = HashSet::new();
            let mine = claims.get(&a.id).unwrap_or(&empty);
            let is_newest = newest.get(&a.cwd) == Some(&a.id);
            files.retain(|f| mine.contains(&f.0) || (is_newest && !sharers.iter().any(|o| claims.get(o).is_some_and(|c| c.contains(&f.0)))));
        }
        let reading = reads(&evs, &cwd, main, now - READ_WINDOW_MS).into_iter().map(|(p, (t, _))| (p, t)).collect();
        out.push(Work { agent: a, files, reading });
    }
    out
}

/// The files an agent's rows of `file_edits` name (relative to its
/// checkout, as `alerts::record_edit` wrote them).
fn edit_rows(sup: &Supervisor, id: &str) -> HashSet<String> {
    let p = chloedb::query::Params::new().set("a", chloedb::Value::Str(id.to_string()));
    sup.db()
        .query("SELECT file FROM file_edits WHERE agent = :a LIMIT 100000", &p)
        .unwrap_or_default()
        .iter()
        .filter_map(|r| crate::db::get_str(&r[0]))
        .filter(|f| !f.starts_with('/'))
        .collect()
}

fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s < 60 {
        format!("{s} s ago")
    } else {
        format!("{} min ago", s / 60)
    }
}

/// The collisions between agents' work: `both_changed`, two agents' diffs
/// touch one file; `heading_into`, one agent changed a file another read
/// in the last ten minutes.
pub fn collisions_of(work: &[Work]) -> Vec<Json> {
    let now = now_ms();
    let mut by_path: BTreeMap<&str, Vec<&Work>> = BTreeMap::new();
    for w in work {
        for f in &w.files {
            by_path.entry(f.0.as_str()).or_default().push(w);
        }
    }
    let mut out = Vec::new();
    for (path, ws) in &by_path {
        if ws.len() > 1 {
            let names: Vec<&str> = ws.iter().map(|w| w.agent.name.as_str()).collect();
            let detail = match names.as_slice() {
                [a, b] => format!("{a} and {b} both changed {path}"),
                _ => format!("{} agents changed {path}: {}", names.len(), names.join(", ")),
            };
            out.push(json!({ "path": path, "agents": ws.iter().map(|w| w.agent.id.clone()).collect::<Vec<_>>(), "kind": "both_changed", "detail": detail }));
        }
    }
    for (path, ws) in &by_path {
        for reader in work {
            let Some((_, t)) = reader.reading.iter().find(|(p, _)| p == path) else { continue };
            if reader.changed(path) {
                continue;
            }
            for w in ws.iter().filter(|w| w.agent.id != reader.agent.id) {
                out.push(json!({
                    "path": path,
                    "agents": [w.agent.id, reader.agent.id],
                    "kind": "heading_into",
                    "changed_by": w.agent.id,
                    "read_by": reader.agent.id,
                    "detail": format!("{} read {path} {}, and {} has changed it", reader.agent.name, ago(now - t), w.agent.name),
                }));
            }
        }
    }
    out
}

/// The collisions of a repository's agents now (for Mission's `heads_up`).
/// `repo` is the repository's path, or any checkout of it.
#[allow(dead_code)] // Mission's heads_up calls it.
pub fn collisions(sup: &Supervisor, repo: &str) -> Vec<Json> {
    match main_of(repo) {
        Ok(main) => collisions_of(&territory(sup, &main)),
        Err(_) => Vec::new(),
    }
}

// The tree.

type Counted = Arc<Vec<(String, u64)>>;

fn tree_cache() -> &'static Mutex<HashMap<PathBuf, (String, Instant, Counted)>> {
    static C: OnceLock<Mutex<HashMap<PathBuf, (String, Instant, Counted)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn count_lines(p: &Path, budget: &mut u64) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else { return 0 };
    if !meta.is_file() || meta.len() == 0 {
        return 0;
    }
    let size = meta.len();
    if size > COUNT_FILE_MAX || *budget < size {
        return (size / 40).max(1);
    }
    let Ok(body) = std::fs::read(p) else { return 0 };
    *budget -= size;
    if body[..body.len().min(8000)].contains(&0) {
        return 0; // binary
    }
    let n = body.iter().filter(|&&b| b == b'\n').count() as u64;
    n + u64::from(body.last() != Some(&b'\n'))
}

/// The tracked files of the main checkout (`git ls-files`) with their
/// line counts, reused for 30 s while HEAD and the index stay the same.
fn counted(main: &Path) -> Result<Counted, String> {
    let head = git::rev_parse(main, "HEAD").unwrap_or_default();
    let index = std::fs::metadata(main.join(".git/index")).and_then(|m| m.modified()).map(|t| format!("{t:?}")).unwrap_or_default();
    let key = format!("{head} {index}");
    if let Some((k, at, c)) = tree_cache().lock().unwrap().get(main) {
        if *k == key && at.elapsed() < TREE_TTL {
            return Ok(c.clone());
        }
    }
    let out = std::process::Command::new("git").arg("-C").arg(main).args(["ls-files", "-z"]).env("GIT_TERMINAL_PROMPT", "0").output().map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git ls-files failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let mut budget = COUNT_BUDGET;
    let list: Vec<(String, u64)> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(|p| (p.to_string(), count_lines(&main.join(p), &mut budget)))
        .collect();
    let c = Arc::new(list);
    tree_cache().lock().unwrap().insert(main.to_path_buf(), (key, Instant::now(), c.clone()));
    Ok(c)
}

#[derive(Default)]
struct Dir {
    lines: u64,
    files: u64,
    dirs: BTreeMap<String, Dir>,
    leaves: Vec<(String, u64)>,
}

impl Dir {
    fn insert(&mut self, parts: &[&str], lines: u64) {
        self.lines += lines;
        self.files += 1;
        match parts {
            [] => {}
            [file] => self.leaves.push((file.to_string(), lines)),
            [dir, rest @ ..] => self.dirs.entry(dir.to_string()).or_default().insert(rest, lines),
        }
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// A folder as the map shows it: folders to depth 3 opened, deeper ones
/// with their size only; small files folded into "…", except the `hot`
/// ones (an agent's or a pin's).
fn dir_json(name: &str, path: &str, d: &Dir, depth: usize, hot: &HashSet<String>) -> Json {
    if depth > MAX_DEPTH {
        return json!({ "name": name, "path": path, "kind": "dir", "lines": d.lines, "files": d.files, "collapsed": true, "children": [] });
    }
    let mut dirs: Vec<(&String, &Dir)> = d.dirs.iter().collect();
    dirs.sort_by(|a, b| b.1.lines.cmp(&a.1.lines).then(a.0.cmp(b.0)));
    let mut children: Vec<Json> = dirs.iter().map(|(n, sub)| dir_json(n, &join(path, n), sub, depth + 1, hot)).collect();
    let mut files: Vec<&(String, u64)> = d.leaves.iter().collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let small = ((d.lines as f64 * FOLD_SHARE) as u64).max(FOLD_LINES);
    let (mut shown, mut folded): (Vec<&(String, u64)>, Vec<&(String, u64)>) = (Vec::new(), Vec::new());
    for f in files {
        let is_hot = hot.contains(&join(path, &f.0));
        if is_hot || (f.1 >= small && shown.len() < MAX_FILES) {
            shown.push(f);
        } else {
            folded.push(f);
        }
    }
    // One file is not worth a fold; nor are small files that make up much
    // of their folder (then they are what the folder is), save those past
    // the `MAX_FILES` biggest.
    let folded_lines: u64 = folded.iter().map(|f| f.1).sum();
    if folded.len() == 1 || (folded_lines as f64) > d.lines as f64 * FOLD_MAX_SHARE {
        let room = MAX_FILES.saturating_sub(shown.len());
        let back: Vec<&(String, u64)> = if folded.len() == 1 { std::mem::take(&mut folded) } else { folded.drain(..room.min(folded.len())).collect() };
        shown.extend(back);
        if folded.len() == 1 {
            shown.push(folded.pop().unwrap());
        }
    }
    for (n, lines) in shown {
        children.push(json!({ "name": n, "path": join(path, n), "kind": "file", "lines": lines }));
    }
    if !folded.is_empty() {
        let lines: u64 = folded.iter().map(|f| f.1).sum();
        // Its files, so that a click on the fold opens it.
        let items: Vec<Json> = folded.iter().take(FOLD_ITEMS).map(|(n, l)| json!({ "name": n, "path": join(path, n), "kind": "file", "lines": l })).collect();
        children.push(json!({ "name": "…", "path": join(path, "…"), "kind": "more", "lines": lines, "files": folded.len(), "items": items }));
    }
    json!({ "name": name, "path": path, "kind": "dir", "lines": d.lines, "files": d.files, "children": children })
}

fn tree(main: &Path, list: &[(String, u64)], hot: &HashSet<String>) -> Json {
    let mut root = Dir::default();
    for (p, lines) in list {
        let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        root.insert(&parts, *lines);
    }
    let name = main.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    dir_json(&name, "", &root, 0, hot)
}

// ChloeCtx's records.

/// A memory record as `chloectx sql --json` printed it.
#[derive(Clone, Debug)]
pub struct Record {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub ts: Json,
    pub files: Vec<String>,
    pub ctx: String,
    pub session: Option<String>,
    pub tag: String,
}

impl Record {
    pub fn to_json(&self) -> Json {
        json!({ "id": self.id, "kind": self.kind, "text": self.text, "ts": self.ts, "files": self.files })
    }
}

/// The repository's decision, preference and fact records that name
/// files, newest first, from `chloectx sql --json` run in `dir`: the
/// records, or what could not be found.
pub fn file_records(sup: &Supervisor, dir: &Path) -> Result<Vec<Record>, String> {
    let sql = "SELECT id, kind, text, ts, files, ctx, session, tag FROM memory WHERE kind = 'decision' OR kind = 'preference' OR kind = 'fact' ORDER BY ts DESC LIMIT 1000";
    let out = match trail::run_chloectx(&sup.cfg.chloectx_bin, dir, &["sql", "--json", sql], Duration::from_secs(60)) {
        Ran::Out(v) => v,
        Ran::Missing => return Err(trail::not_installed(&sup.cfg.chloectx_bin)),
        Ran::Failed(e) => return Err(e),
    };
    let cols: Vec<String> = out["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
    let mut recs = Vec::new();
    for row in out["rows"].as_array().into_iter().flatten().filter_map(Json::as_array) {
        let col = |name: &str| cols.iter().position(|c| c == name).and_then(|i| row.get(i).cloned()).unwrap_or(Json::Null);
        let kind = col("kind").as_str().unwrap_or("").to_string();
        if !matches!(kind.as_str(), "decision" | "preference" | "fact") {
            continue;
        }
        let main = dir.to_path_buf();
        let files: Vec<String> = trail::files_of(&col("files")).into_iter().filter_map(|f| rel_path(&f, &main, &main).or(Some(f))).collect();
        if files.is_empty() {
            continue;
        }
        let ts = col("ts");
        recs.push(Record {
            id: col("id").as_str().map(str::to_string).unwrap_or_else(|| col("id").to_string()),
            kind,
            text: col("text").as_str().unwrap_or("").to_string(),
            ts: trail::ctx_time(&ts),
            files,
            ctx: col("ctx").as_str().unwrap_or("").to_string(),
            session: col("session").as_str().map(str::to_string),
            tag: col("tag").as_str().unwrap_or("").to_string(),
        });
    }
    Ok(recs)
}

/// A record's file entry as a path and a line: `path:42` and `path#L42`
/// name one.
pub fn split_line(f: &str) -> (String, Option<usize>) {
    if let Some((p, l)) = f.rsplit_once("#L").or_else(|| f.rsplit_once(':')) {
        if let Ok(n) = l.parse::<usize>() {
            if n > 0 && !p.is_empty() {
                return (p.to_string(), Some(n));
            }
        }
    }
    (f.to_string(), None)
}

/// The identifiers of a record's `Anchor:` line (ChloeCtx's anchor of a
/// note to the code it describes).
fn anchor_idents(ctx: &str) -> Vec<String> {
    let Some(line) = ctx.lines().find_map(|l| l.strip_prefix("Anchor: ")) else { return Vec::new() };
    line.split(' ').filter_map(|p| p.strip_prefix("idents=")).flat_map(|v| v.split(',')).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

/// The first line of `rel` in `main` that holds one of `idents` as a word.
fn ident_line(main: &Path, rel: &str, idents: &[String]) -> Option<usize> {
    if idents.is_empty() {
        return None;
    }
    let p = main.join(rel);
    if std::fs::metadata(&p).ok()?.len() > 2 << 20 {
        return None;
    }
    let body = std::fs::read_to_string(&p).ok()?;
    let word = |c: char| c.is_alphanumeric() || c == '_';
    body.lines().position(|l| {
        idents.iter().any(|id| {
            l.match_indices(id.as_str()).any(|(i, _)| !l[..i].chars().next_back().is_some_and(word) && !l[i + id.len()..].chars().next().is_some_and(word))
        })
    }).map(|i| i + 1)
}

/// `GET /api/map`.
pub fn map(sup: &Supervisor, repo: Option<&str>) -> Result<Json, ApiError> {
    let main = trail::repo_dir(repo)?;
    let work = territory(sup, &main);
    map_of(sup, &main, work, None)
}

/// The map of `work`; with `until_ms`, only the pins of records made by
/// then (Time travel, src/timetravel.rs: the work a snapshot kept).
pub fn map_of(sup: &Supervisor, main: &Path, work: Vec<Work>, until_ms: Option<i64>) -> Result<Json, ApiError> {
    let main = main.to_path_buf();
    let mut missing: Vec<String> = Vec::new();
    let list = counted(&main).map_err(ApiError::internal)?;
    let tracked: HashSet<&str> = list.iter().map(|(p, _)| p.as_str()).collect();

    // Pins.
    let s = Sessions::load(sup);
    let mut pins: Vec<Json> = Vec::new();
    let mut hot: HashSet<String> = HashSet::new();
    let changed: HashSet<&str> = work.iter().flat_map(|w| w.files.iter().map(|f| f.0.as_str())).collect();
    match file_records(sup, &main) {
        Ok(recs) => {
            let made_by = |r: &Record| until_ms.is_none_or(|u| r.ts.as_str().and_then(chloedb::timestamp::parse).is_none_or(|t| t / 1000 <= u));
            for r in recs.iter().filter(|r| r.tag != "superseded" && made_by(r)) {
                for f in &r.files {
                    let (path, line) = split_line(f);
                    if !tracked.contains(path.as_str()) && !changed.contains(path.as_str()) {
                        continue;
                    }
                    let line = line.or_else(|| ident_line(&main, &path, &anchor_idents(&r.ctx)));
                    hot.insert(path.clone());
                    pins.push(json!({
                        "path": path,
                        "line": line,
                        "record": { "id": r.id, "kind": r.kind, "text": r.text, "ts": r.ts },
                        "agent": r.session.as_deref().and_then(|sid| s.agent(sid)).map(|a| trail::agent_json(sup, a)),
                    }));
                }
                if pins.len() >= 300 {
                    break;
                }
            }
        }
        Err(e) => missing.push(e),
    }

    for w in &work {
        hot.extend(w.files.iter().map(|f| f.0.clone()));
        hot.extend(w.reading.iter().map(|r| r.0.clone()));
    }
    let agents: Vec<Json> = work
        .iter()
        .map(|w| {
            json!({
                "agent": trail::agent_json(sup, &w.agent),
                "files": w.files.iter().map(|(p, a, d, st)| json!({ "path": p, "added": a, "removed": d, "status": st })).collect::<Vec<_>>(),
                "reading": w.reading.iter().map(|r| r.0.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "repo": main.to_string_lossy(),
        "tree": tree(&main, &list, &hot),
        "agents": agents,
        "collisions": collisions_of(&work),
        "pins": pins,
        "missing": missing,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_small_files_and_deep_folders() {
        let list: Vec<(String, u64)> = vec![
            ("src/big.rs".into(), 900),
            ("src/tiny.rs".into(), 2),
            ("src/tiny2.rs".into(), 3),
            ("src/hot.rs".into(), 1),
            ("a/b/c/d/e.txt".into(), 10),
            ("README".into(), 5),
        ];
        let hot: HashSet<String> = ["src/hot.rs".to_string()].into();
        let t = tree(Path::new("/x/proj"), &list, &hot);
        assert_eq!((t["name"].as_str(), t["lines"].as_u64(), t["files"].as_u64()), (Some("proj"), Some(921), Some(6)));
        let src = t["children"].as_array().unwrap().iter().find(|c| c["name"] == "src").unwrap();
        let names: Vec<&str> = src["children"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["big.rs", "hot.rs", "…"]);
        assert_eq!(src["children"][2]["files"], 2);
        assert_eq!(src["children"][2]["lines"], 5);
        let c = &t["children"].as_array().unwrap().iter().find(|c| c["name"] == "a").unwrap()["children"][0]["children"][0];
        assert_eq!(c["path"], "a/b/c");
        assert_eq!(c["children"][0]["path"], "a/b/c/d");
        assert_eq!(c["children"][0]["collapsed"], true);
        assert_eq!(c["children"][0]["lines"], 10);
    }

    #[test]
    fn a_small_repository_is_not_folded_away_and_a_fold_lists_its_files() {
        // Lee's shop: ten files, 92 lines, all of them "small".
        let mut list: Vec<(String, u64)> = ["cart.py", "money.py", "coupons.py", "serve.py", "test_cart.py", "test_money.py", "test_coupons.py", "test_web.py", "README.md"].iter().map(|n| (n.to_string(), 9)).collect();
        list.push(("web/index.html".into(), 13));
        let t = tree(Path::new("/x/shop"), &list, &HashSet::new());
        let kids = t["children"].as_array().unwrap();
        assert!(kids.iter().all(|c| c["kind"] != "more"), "{t}");
        assert_eq!(kids.len(), 10);
        // A big folder of many tiny files still folds them, and the fold
        // names each one.
        let mut list: Vec<(String, u64)> = (0..30).map(|i| (format!("gen/f{i:02}.rs"), 2)).collect();
        list.push(("gen/big.rs".into(), 5000));
        let t = tree(Path::new("/x/p"), &list, &HashSet::new());
        let gen = &t["children"][0];
        let more = gen["children"].as_array().unwrap().iter().find(|c| c["kind"] == "more").unwrap();
        assert_eq!(more["files"], 30);
        assert_eq!(more["items"].as_array().unwrap().len(), 30);
        assert_eq!(more["items"][0]["path"], "gen/f00.rs");
    }

    #[test]
    fn paths_lines_and_anchors() {
        assert_eq!(rel_path("/w/t/src/a.rs", Path::new("/w/t"), Path::new("/r")), Some("src/a.rs".into()));
        assert_eq!(rel_path("/r/src/a.rs", Path::new("/w/t"), Path::new("/r")), Some("src/a.rs".into()));
        assert_eq!(rel_path("/etc/passwd", Path::new("/w/t"), Path::new("/r")), None);
        assert_eq!(rel_path("./a/../b", Path::new("/w"), Path::new("/r")), None);
        assert_eq!(split_line("src/a.rs:42"), ("src/a.rs".into(), Some(42)));
        assert_eq!(split_line("src/a.rs#L7"), ("src/a.rs".into(), Some(7)));
        assert_eq!(split_line("src/a.rs"), ("src/a.rs".into(), None));
        assert_eq!(anchor_idents("Turn 3\nAnchor: head=abc branch=main files=a.py#h1 idents=post_entry,Money"), ["post_entry", "Money"]);
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn other() {}\nfn my_post_entry() {}\nfn post_entry() {}\n").unwrap();
        assert_eq!(ident_line(d.path(), "a.rs", &["post_entry".into()]), Some(3));
    }
}
