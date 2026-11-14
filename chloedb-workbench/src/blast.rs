//! Blast radius (SPEC.md, "Fourth build", use case 7): what an agent's
//! change reaches before it merges. From its diff against its base, the
//! functions it changed (a symbol finder per language, over both sides of
//! the diff); the tests that cover them (test files that name them or
//! import their files, ranked, and the `test_runs` table when Health made
//! one); the decisions about those lines (`chloectx blame` of a changed
//! line per function, ChloeCtx's records that name the files, and its
//! graph: notes that touch the files and notes about the same entities);
//! the other agents whose diffs touch the same files, call the changed
//! functions, or read the files; and a risk line from all of it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use axum::extract::{Path as AxPath, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json as AxJson, Router};
use serde_json::{json, Value as Json};

use crate::git;
use crate::server::{blocking, ApiResult, App};
use crate::supervisor::{ApiError, Supervisor};
use crate::trail::{self, Ran, Sessions};

/// The most functions blamed with `chloectx blame` (one line each).
const MAX_BLAMES: usize = 8;
/// The most functions whose callers are looked for.
const MAX_CALLER_SEARCHES: usize = 40;
/// Callers kept per function (all are counted).
const CALLERS_SHOWN: usize = 8;
/// Test files read, and the bytes read from them in all.
const MAX_TEST_FILES: usize = 3000;
const TEST_BYTES: u64 = 64 << 20;
/// Source files the symbol finder reads (each side).
const MAX_SOURCE: u64 = 2 << 20;
/// Names too common to look for callers or tests by.
const COMMON: [&str; 28] = [
    "new", "main", "run", "get", "set", "init", "test", "tests", "default", "from", "into", "len", "call", "drop", "fmt", "clone", "eq", "hash", "next", "build", "parse", "open",
    "close", "read", "write", "update", "render", "handle",
];

pub fn routes() -> Router<App> {
    Router::new().route("/api/agents/:id/blast", get(blast_route))
}

async fn blast_route(State(app): State<App>, AxPath(id): AxPath<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || blast(&sup, &id)).await?).into_response())
}

/// `git <args>` in `dir`: its output, or why it failed.
pub(crate) fn git_out(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").output().map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

// ---------------------------------------------------------------- landing

/// Another agent's branch this one holds commits of: it was rebased on
/// top of it (`stacked`) or merged it in (`merged`).
#[derive(Clone, Debug)]
pub struct Under {
    pub agent: crate::db::Agent,
    pub branch: String,
    pub tip: String,
    /// The last commit the two branches share.
    pub shared: String,
    /// Whether this branch holds the other's tip (else an older commit of it).
    pub contained: bool,
    /// Whether it came in by a merge rather than on the first-parent line.
    pub merged: bool,
    /// Its commits this branch holds, past the landing branch.
    pub commits: usize,
}

/// Where a branch will land and what its own change is: the branch it
/// lands on, where it forked from it, the agents' branches it holds
/// (stacked on or merged in), the agents stacked on top of it, and the
/// commit its own change is counted from.
#[derive(Clone, Debug)]
pub struct Landing {
    pub head: Option<String>,
    pub onto: Option<String>,
    pub fork: Option<String>,
    pub under: Vec<Under>,
    pub above: Vec<(crate::db::Agent, String)>,
    pub base: String,
    /// `landing` (the fork from the landing branch), `stack` (an agent's
    /// branch this one holds) or `start` (the commit the agent started from).
    pub base_kind: &'static str,
}

fn merge_base(dir: &Path, a: &str, b: &str) -> Option<String> {
    git_out(dir, &["merge-base", a, b]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn ancestor(dir: &Path, a: &str, b: &str) -> bool {
    git::is_ancestor(dir, a, b)
}

fn count_commits(dir: &Path, range: &str) -> usize {
    git_out(dir, &["rev-list", "--count", range]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// The branch an agent's work lands on: the main checkout's branch when it
/// is not the agent's own, else `main` or `master`.
pub fn landing_branch(main: &Path, a: &crate::db::Agent) -> Option<String> {
    let mine = a.branch.as_deref();
    if let Some(b) = git::current_branch(main).filter(|b| Some(b.as_str()) != mine) {
        return Some(b);
    }
    ["main", "master"].into_iter().find(|b| Some(*b) != mine && git::branch_exists(main, b)).map(str::to_string)
}

/// Where the agent's branch lands and the commit its own change counts from.
pub fn landing(sup: &Supervisor, a: &crate::db::Agent, dir: &Path, main: &Path) -> Landing {
    let head = git::rev_parse(dir, "HEAD");
    let start = a.base.clone();
    let onto = landing_branch(main, a);
    let fallback = start.clone().or_else(|| head.clone()).unwrap_or_default();
    let Some(h) = head.clone() else {
        return Landing { head, onto, fork: None, under: Vec::new(), above: Vec::new(), base: fallback, base_kind: "start" };
    };
    let fork = onto.as_deref().and_then(|b| merge_base(dir, "HEAD", b));
    // The other agents' branches, the newest agent of each.
    let mut others: BTreeMap<String, crate::db::Agent> = BTreeMap::new();
    for o in sup.agent_rows() {
        let Some(b) = o.branch.clone() else { continue };
        if o.repo != a.repo || o.id == a.id || Some(&b) == a.branch.as_ref() || Some(&b) == onto.as_ref() {
            continue;
        }
        let keep = others.get(&b).is_none_or(|k| k.created <= o.created);
        if keep {
            others.insert(b, o);
        }
    }
    let first_parent: HashSet<String> = match &fork {
        Some(f) => git_out(dir, &["rev-list", "--first-parent", &format!("{f}..HEAD")]).unwrap_or_default().lines().map(str::to_string).collect(),
        None => HashSet::new(),
    };
    let floor = fork.clone().or_else(|| start.clone());
    let mut under = Vec::new();
    let mut above = Vec::new();
    for (branch, o) in others {
        let Some(tip) = git::rev_parse(main, &format!("refs/heads/{branch}")) else { continue };
        if tip == h {
            continue;
        }
        let Some(mb) = merge_base(dir, "HEAD", &tip) else { continue };
        if mb == h {
            above.push((o, branch));
            continue;
        }
        // Shares only what the landing branch has: an independent branch.
        let beyond = match &floor {
            Some(f) => mb != *f && ancestor(dir, f, &mb),
            None => false,
        };
        if !beyond {
            continue;
        }
        let contained = mb == tip;
        let merged = contained && !first_parent.contains(&tip);
        let commits = floor.as_deref().map_or(0, |f| count_commits(dir, &format!("{f}..{mb}")));
        under.push(Under { agent: o, branch, tip, shared: mb, contained, merged, commits });
    }
    // The base: the top of what this branch holds of others, else the fork,
    // else the start (an agent started on top of a branch no agent has).
    let mut base = fork.clone().unwrap_or_else(|| fallback.clone());
    let mut kind = if fork.is_some() { "landing" } else { "start" };
    if let (Some(s), Some(f)) = (&start, &fork) {
        if s != f && ancestor(dir, f, s) && ancestor(dir, s, &h) && !under.iter().any(|u| ancestor(dir, s, &u.shared)) {
            base = s.clone();
            kind = "start";
        }
    }
    if !under.is_empty() {
        let top = under.iter().map(|u| u.shared.clone()).max_by_key(|c| under.iter().filter(|u| ancestor(dir, &u.shared, c)).count());
        if let Some(t) = top {
            base = t;
            kind = "stack";
        }
    }
    Landing { head, onto, fork, under, above, base, base_kind: kind }
}

impl Landing {
    /// The landing as the API answers it.
    pub fn json(&self, sup: &Supervisor) -> Json {
        json!({
            "onto": self.onto, "fork": self.fork, "base": self.base, "base_kind": self.base_kind,
            "under": self.under.iter().map(|u| json!({
                "agent": trail::agent_json(sup, &u.agent), "branch": u.branch, "tip": u.tip, "shared": u.shared,
                "contained": u.contained, "merged": u.merged, "commits": u.commits,
            })).collect::<Vec<_>>(),
            "above": self.above.iter().map(|(o, b)| json!({ "agent": trail::agent_json(sup, o), "branch": b })).collect::<Vec<_>>(),
            "line": self.line(),
        })
    }

    /// One sentence: "on top of freeship (2 commits, not counted here)".
    pub fn line(&self) -> Option<String> {
        if self.under.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .under
            .iter()
            .map(|u| {
                let how = if u.merged { format!("has {}'s branch {} merged in", u.agent.name, u.branch) } else { format!("is on top of {} ({})", u.agent.name, u.branch) };
                format!("{how}: {} of its {}, not counted as this change", plural(u.commits, "commit"), if u.contained { "work" } else { "older work" })
            })
            .collect();
        Some(format!("This branch {}.", parts.join("; it ")))
    }
}

/// Whether two commits merge without a conflict (`git merge-tree
/// --write-tree`), and the files that conflict.
pub(crate) fn merges_clean(dir: &Path, a: &str, b: &str) -> Option<(bool, Vec<String>)> {
    let out = Command::new("git").arg("-C").arg(dir).args(["merge-tree", "--write-tree", "--name-only", "--no-messages", a, b]).env("GIT_TERMINAL_PROMPT", "0").output().ok()?;
    match out.status.code() {
        Some(0) => Some((true, Vec::new())),
        Some(1) => {
            let text = String::from_utf8_lossy(&out.stdout);
            Some((false, text.lines().skip(1).map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- symbols

/// A language the finder reads, from a path's extension.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lang {
    Rust,
    Js,
    Python,
    Go,
    Java,
    C,
    Ruby,
    Php,
    Swift,
}

impl Lang {
    pub fn of(path: &str) -> Option<Lang> {
        let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" | "vue" | "svelte" => Lang::Js,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "java" | "kt" | "kts" | "cs" | "scala" => Lang::Java,
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "m" | "mm" => Lang::C,
            "rb" => Lang::Ruby,
            "php" => Lang::Php,
            "swift" => Lang::Swift,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Js => "typescript",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::Java => "java",
            Lang::C => "c",
            Lang::Ruby => "ruby",
            Lang::Php => "php",
            Lang::Swift => "swift",
        }
    }

    /// Whether blocks are braces (else indentation, as Python and Ruby).
    fn braces(self) -> bool {
        !matches!(self, Lang::Python | Lang::Ruby)
    }
}

/// A definition the finder saw: its name, kind, and lines (1-based, both
/// ends in).
#[derive(Clone, Debug, PartialEq)]
pub struct Def {
    pub name: String,
    pub kind: &'static str,
    pub line: usize,
    pub end: usize,
}

fn ident_start(s: &str) -> Option<&str> {
    let end = s.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(s.len());
    let id = &s[..end];
    (!id.is_empty() && !id.chars().next().unwrap().is_ascii_digit()).then_some(id)
}

/// `s` without the leading words in `mods` (and `pub(crate)` and the like).
fn strip_mods<'a>(mut s: &'a str, mods: &[&str]) -> &'a str {
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("pub(") {
            if let Some(i) = rest.find(')') {
                s = &rest[i + 1..];
                continue;
            }
        }
        if let Some(rest) = t.strip_prefix("extern \"") {
            if let Some(i) = rest.find('"') {
                s = &rest[i + 1..];
                continue;
            }
        }
        if t.starts_with('@') && !t.starts_with("@interface") {
            // An annotation or decorator on the same line.
            match t.find(' ') {
                Some(i) => {
                    s = &t[i..];
                    continue;
                }
                None => return "",
            }
        }
        let Some(w) = ident_start(t) else { return t };
        if mods.contains(&w) && t[w.len()..].starts_with(|c: char| c.is_whitespace()) {
            s = &t[w.len()..];
            continue;
        }
        return t;
    }
}

/// After the keyword `kw` (followed by a space), the name.
fn after_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(kw)?;
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '*') {
        return None;
    }
    ident_start(rest.trim_start_matches(|c: char| c.is_whitespace() || c == '*'))
}

const KEYWORDS: [&str; 22] = [
    "if", "for", "while", "switch", "catch", "return", "function", "else", "do", "try", "with", "new", "throw", "await", "typeof", "delete", "case", "sizeof", "using", "match",
    "yield", "super",
];

/// The definition a line starts, if it starts one.
pub fn def_of(lang: Lang, line: &str) -> Option<(String, &'static str)> {
    let indent = line.len() - line.trim_start().len();
    let t = line.trim();
    if t.is_empty() || t.starts_with("//") || t.starts_with('#') && lang != Lang::Rust || t.starts_with("/*") || t.starts_with('*') {
        return None;
    }
    let named = |n: &str, k: &'static str| Some((n.to_string(), k));
    match lang {
        Lang::Rust => {
            let s = strip_mods(t, &["pub", "async", "const", "unsafe", "default", "extern"]);
            for (kw, kind) in [("fn", "function"), ("struct", "struct"), ("enum", "enum"), ("trait", "trait"), ("union", "struct")] {
                if let Some(n) = after_kw(s, kw) {
                    return named(n, kind);
                }
            }
            if let Some(rest) = s.strip_prefix("macro_rules!") {
                return ident_start(rest.trim()).and_then(|n| named(n, "macro"));
            }
            None
        }
        Lang::Js => {
            let s = strip_mods(t, &["export", "default", "async", "declare", "abstract", "public", "private", "protected", "static", "readonly", "override"]);
            for (kw, kind) in [("function", "function"), ("class", "class"), ("interface", "interface"), ("enum", "enum")] {
                if let Some(n) = after_kw(s, kw) {
                    return named(n, kind);
                }
            }
            if let Some(n) = after_kw(s, "type") {
                if s[s.find(n).unwrap() + n.len()..].trim_start().starts_with(['=', '<']) {
                    return named(n, "type");
                }
            }
            for kw in ["const", "let", "var"] {
                if let Some(n) = after_kw(s, kw) {
                    let rest = s[s.find(n).unwrap() + n.len()..].trim_start();
                    let rest = rest.strip_prefix(':').map(|r| r.split_once('=').map(|x| x.1).unwrap_or("")).or_else(|| rest.strip_prefix('=')).unwrap_or("");
                    let rest = rest.trim_start();
                    if rest.starts_with("function") || rest.starts_with("async") || (rest.starts_with('(') && rest.contains("=>")) || (ident_start(rest).is_some() && rest.contains("=>") && rest.find("=>") < rest.find('{').or(Some(usize::MAX))) {
                        return named(n, "function");
                    }
                    return None;
                }
            }
            // A method: `name(args) {` inside a class or object.
            if indent > 0 {
                let s = s.strip_prefix("get ").or_else(|| s.strip_prefix("set ")).unwrap_or(s);
                let s = s.trim_start_matches('*').trim_start_matches('#');
                if let Some(n) = ident_start(s) {
                    let rest = &s[n.len()..];
                    let rest = rest.strip_prefix('<').map(|r| r.split_once('>').map(|x| x.1).unwrap_or("")).unwrap_or(rest);
                    if rest.starts_with('(') && t.ends_with('{') && !KEYWORDS.contains(&n) && !t.contains("=>") && !t.contains(" = ") {
                        return named(n, "method");
                    }
                }
            }
            None
        }
        Lang::Python => {
            let s = t.strip_prefix("async ").unwrap_or(t);
            if let Some(n) = after_kw(s, "def") {
                return named(n, if indent > 0 { "method" } else { "function" });
            }
            after_kw(s, "class").and_then(|n| named(n, "class"))
        }
        Lang::Go => {
            if let Some(rest) = t.strip_prefix("func ") {
                let rest = rest.trim_start();
                let rest = if rest.starts_with('(') { rest.split_once(')').map(|x| x.1.trim_start()).unwrap_or("") } else { rest };
                return ident_start(rest).and_then(|n| named(n, "function"));
            }
            if let Some(n) = after_kw(t, "type") {
                let rest = t[t.find(n).unwrap() + n.len()..].trim_start();
                if rest.starts_with("struct") || rest.starts_with("interface") {
                    return named(n, "type");
                }
            }
            None
        }
        Lang::Java | Lang::Php | Lang::Swift | Lang::C => {
            let mods: &[&str] = &[
                "public", "private", "protected", "internal", "static", "final", "abstract", "sealed", "open", "override", "virtual", "async", "suspend", "inline", "data",
                "partial", "readonly", "extern", "const", "unsafe", "synchronized", "native", "fileprivate", "mutating", "export",
            ];
            let s = strip_mods(t, mods);
            for (kw, kind) in [("class", "class"), ("interface", "interface"), ("enum", "enum"), ("struct", "struct"), ("record", "class"), ("protocol", "interface"), ("object", "class"), ("trait", "trait")] {
                if let Some(n) = after_kw(s, kw) {
                    if lang == Lang::C && !t.ends_with('{') && !t.ends_with(':') {
                        continue; // a declaration or a forward one
                    }
                    return named(n, kind);
                }
            }
            for kw in ["fun", "func", "function", "def"] {
                if let Some(n) = after_kw(s, kw) {
                    return named(n, "function");
                }
            }
            if t.ends_with(';') || t.starts_with('#') || t.contains('=') && !t.contains("==") && t.find('=') < t.find('(') {
                return None;
            }
            // `Type name(args) {`, or at the top level in C `type name(args)`.
            let p = s.find('(')?;
            let head = s[..p].trim_end();
            let n = head.rsplit(|c: char| !(c.is_alphanumeric() || c == '_' || c == '~')).next()?;
            let before = head[..head.len() - n.len()].trim_end();
            // C may put the return type on the line before (`static int` then `name(args)`).
            let k_and_r = lang == Lang::C && indent == 0 && t.ends_with(')');
            if n.is_empty() || KEYWORDS.contains(&n) || (before.is_empty() && !k_and_r) || before.ends_with(['.', '>']) && !before.ends_with("::") {
                return None;
            }
            let opens = t.ends_with('{') || (lang == Lang::C && indent == 0 && t.ends_with(')'));
            (opens && ident_start(n).is_some()).then(|| (n.to_string(), "function"))
        }
        Lang::Ruby => {
            if let Some(rest) = t.strip_prefix("def ") {
                let rest = rest.trim_start().strip_prefix("self.").unwrap_or(rest.trim_start());
                let end = rest.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '?' | '!' | '='))).unwrap_or(rest.len());
                return (end > 0).then(|| (rest[..end].to_string(), "method"));
            }
            for (kw, kind) in [("class", "class"), ("module", "module")] {
                if let Some(n) = after_kw(t, kw) {
                    return named(n, kind);
                }
            }
            None
        }
    }
}

/// The definitions of a file's text, each with the line it ends on.
pub fn defs(lang: Lang, text: &str) -> Vec<Def> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some((name, kind)) = def_of(lang, l) else { continue };
        let end = if lang.braces() { brace_end(&lines, i) } else { indent_end(&lines, i) };
        out.push(Def { name, kind, line: i + 1, end: end + 1 });
    }
    out
}

/// Where the block a definition at `i` opens ends (0-based), counting
/// braces outside strings and comments, roughly; a declaration (a `;`
/// before any `{`) ends on its line.
fn brace_end(lines: &[&str], i: usize) -> usize {
    let mut depth = 0i64;
    let mut opened = false;
    for (j, l) in lines.iter().enumerate().skip(i).take(5000) {
        let mut quote: Option<char> = None;
        let mut prev = ' ';
        for c in l.chars() {
            if let Some(q) = quote {
                if c == q && prev != '\\' {
                    quote = None;
                }
            } else if c == '/' && prev == '/' {
                break;
            } else if c == '"' || c == '`' || (c == '\'' && !prev.is_alphanumeric()) {
                quote = Some(c);
            } else if c == '{' {
                depth += 1;
                opened = true;
            } else if c == '}' {
                depth -= 1;
            } else if c == ';' && !opened && depth == 0 {
                return j;
            }
            prev = if prev == '\\' && c == '\\' { ' ' } else { c };
        }
        if opened && depth <= 0 {
            return j;
        }
        if !opened && j > i + 8 {
            return i; // no block in sight
        }
    }
    (i + 1).min(lines.len().saturating_sub(1)).max(i)
}

/// Where an indented block at `i` ends (0-based): before the next line,
/// not blank, indented no deeper than it.
fn indent_end(lines: &[&str], i: usize) -> usize {
    let ind = |l: &str| l.len() - l.trim_start().len();
    let base = ind(lines[i]);
    let mut last = i;
    for (j, l) in lines.iter().enumerate().skip(i + 1) {
        if l.trim().is_empty() {
            continue;
        }
        if ind(l) <= base {
            // Ruby's `end` closes the block on the definition's column.
            if l.trim() == "end" {
                return j;
            }
            break;
        }
        last = j;
    }
    last
}

/// The innermost definition holding line `n` (1-based).
pub fn enclosing(defs: &[Def], n: usize) -> Option<&Def> {
    defs.iter().filter(|d| d.line <= n && n <= d.end).min_by_key(|d| d.end - d.line)
}

/// Whether a path is a test file (or, for Rust, may hold a test module).
pub fn is_test_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    let dirs = format!("/{p}");
    dirs.contains("/tests/") || dirs.contains("/test/") || dirs.contains("/__tests__/") || dirs.contains("/spec/") || dirs.contains("/e2e/")
        || name.contains(".test.") || name.contains(".spec.") || name.ends_with("_test.go") || name.ends_with("_test.py") || name.starts_with("test_")
        || name.ends_with("_spec.rb") || name.ends_with("test.java") || name.ends_with("tests.java") || name.ends_with("test.kt") || name.ends_with("tests.cs") || name.ends_with("test.cs")
}

fn word_count(text: &str, w: &str) -> usize {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    text.match_indices(w).filter(|(i, _)| !text[..*i].chars().next_back().is_some_and(is_word) && !text[i + w.len()..].chars().next().is_some_and(is_word)).count()
}

/// The ways a file's module is named in imports: `cart/total`,
/// `cart::total`, `cart.total` (without `src/`, `lib/` and the extension).
fn module_names(path: &str) -> Vec<String> {
    let stem = path.rsplit_once('.').map(|x| x.0).unwrap_or(path);
    let stem = stem.strip_suffix("/mod").or_else(|| stem.strip_suffix("/index")).or_else(|| stem.strip_suffix("/__init__")).unwrap_or(stem);
    let trimmed = ["src/", "lib/", "app/", "pkg/"].iter().find_map(|p| stem.strip_prefix(p)).unwrap_or(stem);
    let mut out = vec![trimmed.to_string()];
    if trimmed.contains('/') {
        out.push(trimmed.replace('/', "::"));
        out.push(trimmed.replace('/', "."));
    }
    out.retain(|s| s.len() >= 3);
    out
}

fn imports(text: &str, names: &[String]) -> bool {
    text.lines().any(|l| {
        let t = l.trim_start();
        (t.starts_with("import ") || t.starts_with("from ") || t.starts_with("use ") || t.starts_with("mod ") || t.starts_with("#include") || t.starts_with("require") || t.contains("require(") || t.contains("import("))
            && names.iter().any(|n| t.contains(n.as_str()))
    })
}

// ---------------------------------------------------------------- the blast

/// A changed function.
#[derive(Clone, Debug)]
struct Func {
    id: String,
    name: String,
    kind: &'static str,
    lang: Lang,
    path: String,
    line: usize,
    status: &'static str,
    added: usize,
    removed: usize,
    first_added: Option<usize>,
    callers: Vec<Json>,
    callers_total: usize,
    /// Uses of it in test files (not in `callers_total`).
    test_callers: usize,
    caller_files: BTreeSet<String>,
}

/// One decision found, by id.
#[derive(Clone, Debug, Default)]
struct Dec {
    id: String,
    kind: String,
    text: String,
    ts: Json,
    session: Option<String>,
    files: Vec<String>,
    via: BTreeSet<&'static str>,
    functions: BTreeSet<String>,
    superseded: bool,
    superseded_by: Option<String>,
    line: Option<(String, usize)>,
    entity: Option<String>,
}

fn file_text(dir: &Path, rel: &str) -> Option<String> {
    let p = dir.join(rel);
    let m = std::fs::metadata(&p).ok()?;
    if !m.is_file() || m.len() > MAX_SOURCE {
        return None;
    }
    String::from_utf8(std::fs::read(p).ok()?).ok()
}

fn old_text(dir: &Path, base: &str, rel: &str) -> Option<String> {
    let t = git_out(dir, &["show", &format!("{base}:{rel}")]).ok()?;
    (t.len() as u64 <= MAX_SOURCE).then_some(t)
}

/// The functions a diff changed: each `+` line counts to the definition
/// that holds it in the new text, each `-` line to the one in the old.
/// With `tests`, those of test files (else those of the other files).
fn changed_functions(dir: &Path, base: &str, files: &[Json], tests: bool) -> Vec<Func> {
    let mut by_id: BTreeMap<String, Func> = BTreeMap::new();
    for f in files {
        let path = f["path"].as_str().unwrap_or("").to_string();
        let old_path = f["old_path"].as_str().unwrap_or(&path).to_string();
        if f["binary"].as_bool() == Some(true) || is_test_path(&path) != tests {
            continue;
        }
        let Some(lang) = Lang::of(&path) else { continue };
        let status = f["status"].as_str().unwrap_or("modified");
        let new_defs = if status == "deleted" { Vec::new() } else { file_text(dir, &path).map(|t| defs(lang, &t)).unwrap_or_default() };
        let old_defs = if status == "added" { Vec::new() } else { old_text(dir, base, &old_path).map(|t| defs(lang, &t)).unwrap_or_default() };
        let new_names: HashSet<&str> = new_defs.iter().map(|d| d.name.as_str()).collect();
        let old_names: HashSet<&str> = old_defs.iter().map(|d| d.name.as_str()).collect();
        for hunk in f["hunks"].as_array().into_iter().flatten() {
            for l in hunk["lines"].as_array().into_iter().flatten() {
                let (def, added) = match l["kind"].as_str() {
                    Some("+") => (l["new_line"].as_u64().and_then(|n| enclosing(&new_defs, n as usize)), true),
                    Some("-") => (l["old_line"].as_u64().and_then(|n| enclosing(&old_defs, n as usize)), false),
                    _ => continue,
                };
                let Some(d) = def else { continue };
                let id = format!("{path}#{}", d.name);
                let in_new = new_names.contains(d.name.as_str());
                let st = match (old_names.contains(d.name.as_str()), in_new) {
                    (false, true) => "added",
                    (true, false) => "deleted",
                    _ => "changed",
                };
                let line = if in_new { new_defs.iter().find(|x| x.name == d.name).map(|x| x.line).unwrap_or(d.line) } else { d.line };
                let e = by_id.entry(id.clone()).or_insert_with(|| Func {
                    id,
                    name: d.name.clone(),
                    kind: d.kind,
                    lang,
                    path: path.clone(),
                    line,
                    status: st,
                    added: 0,
                    removed: 0,
                    first_added: None,
                    callers: Vec::new(),
                    callers_total: 0,
                    test_callers: 0,
                    caller_files: BTreeSet::new(),
                });
                if added {
                    e.added += 1;
                    if e.first_added.is_none() {
                        e.first_added = l["new_line"].as_u64().map(|n| n as usize);
                    }
                } else {
                    e.removed += 1;
                }
            }
        }
    }
    let mut out: Vec<Func> = by_id.into_values().collect();
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    out
}

fn searchable(name: &str) -> bool {
    name.chars().count() >= 3 && !COMMON.contains(&name.to_ascii_lowercase().as_str())
}

/// The callers of each function: where its name is used as a word in the
/// worktree's files, outside its own definition; uses in test files are
/// counted apart. A method counts only where it is called or named as an
/// attribute (`.add`, `add(`), so `add` is not every word "add".
fn find_callers(dir: &Path, funcs: &mut [Func]) {
    let mut order: Vec<usize> = (0..funcs.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(funcs[i].added + funcs[i].removed));
    for &i in order.iter().take(MAX_CALLER_SEARCHES) {
        let f = &funcs[i];
        if !searchable(&f.name) {
            continue;
        }
        let Ok(out) = git_out(dir, &["grep", "-n", "-w", "-I", "-F", "--untracked", "-e", &f.name, "--", "."]) else { continue };
        let (name, path, lang) = (f.name.clone(), f.path.clone(), f.lang);
        let method = f.kind == "method";
        let mut shown = Vec::new();
        let mut total = 0;
        let mut in_tests = 0;
        let mut files = BTreeSet::new();
        for l in out.lines().take(5000) {
            let mut parts = l.splitn(3, ':');
            let (Some(p), Some(n), Some(text)) = (parts.next(), parts.next(), parts.next()) else { continue };
            let Ok(n) = n.parse::<usize>() else { continue };
            if def_of(Lang::of(p).unwrap_or(lang), text).is_some_and(|(d, _)| d == name) {
                continue;
            }
            let t = text.trim();
            if t.starts_with("//") || t.starts_with('#') && lang != Lang::Rust || t.starts_with("use ") || t.starts_with("import ") || t.starts_with("from ") && t.contains(" import ") || (p == path && t.starts_with("pub use")) {
                continue;
            }
            if method && !called(t, &name) {
                continue;
            }
            if is_test_path(p) {
                in_tests += 1;
                continue;
            }
            total += 1;
            files.insert(p.to_string());
            if shown.len() < CALLERS_SHOWN {
                shown.push(json!({ "path": p, "line": n, "text": crate::supervisor::one_line(t, 160) }));
            }
        }
        let f = &mut funcs[i];
        f.callers = shown;
        f.callers_total = total;
        f.test_callers = in_tests;
        f.caller_files = files;
    }
}

/// Whether `text` calls `name` or names it as an attribute.
fn called(text: &str, name: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    text.match_indices(name).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + name.len()..].trim_start().chars().next();
        !before.is_some_and(is_word) && !text[i + name.len()..].chars().next().is_some_and(is_word) && (before == Some('.') || after == Some('('))
    })
}

/// A test file that covers changed functions.
struct TestHit {
    path: String,
    score: i64,
    covers: BTreeSet<String>,
    why: Vec<String>,
}

fn find_tests(dir: &Path, funcs: &[Func], changed_files: &[String]) -> Vec<TestHit> {
    let listed = git_out(dir, &["ls-files", "-z", "--cached", "--others", "--exclude-standard"]).unwrap_or_default();
    let mut files: Vec<&str> = listed.split('\0').filter(|p| !p.is_empty()).collect();
    files.sort();
    files.dedup();
    let mut budget = TEST_BYTES;
    let mut hits = Vec::new();
    let names: Vec<&Func> = funcs.iter().filter(|f| searchable(&f.name)).collect();
    let mods: Vec<(String, Vec<String>)> = changed_files.iter().filter(|p| !is_test_path(p) && Lang::of(p).is_some()).map(|p| (p.clone(), module_names(p))).collect();
    let mut read = 0;
    for p in files {
        let rust_src = p.ends_with(".rs") && !is_test_path(p);
        if !is_test_path(p) && !rust_src {
            continue;
        }
        if read >= MAX_TEST_FILES || budget == 0 {
            break;
        }
        let Some(text) = file_text(dir, p) else { continue };
        // A Rust source file's tests are its `#[cfg(test)]` module.
        let text = if rust_src {
            match text.find("#[cfg(test)]") {
                Some(i) => text[i..].to_string(),
                None => continue,
            }
        } else {
            text
        };
        read += 1;
        budget = budget.saturating_sub(text.len() as u64);
        let mut hit = TestHit { path: p.to_string(), score: 0, covers: BTreeSet::new(), why: Vec::new() };
        let mut named = Vec::new();
        for f in &names {
            let n = word_count(&text, &f.name);
            if n > 0 {
                hit.score += 3 + (n.min(5) as i64 - 1) / 2;
                hit.covers.insert(f.id.clone());
                named.push(if n > 1 { format!("{} ×{n}", f.name) } else { f.name.clone() });
            }
        }
        if !named.is_empty() {
            named.dedup();
            hit.why.push(format!("names {}", named.join(", ")));
        }
        for (path, m) in &mods {
            if rust_src && p == path {
                continue;
            }
            if imports(&text, m) {
                hit.score += 2;
                for f in funcs.iter().filter(|f| &f.path == path) {
                    hit.covers.insert(f.id.clone());
                }
                hit.why.push(format!("imports {}", path.rsplit_once('.').map(|x| x.0).unwrap_or(path)));
            }
        }
        if hit.score > 0 {
            if rust_src {
                hit.why.push("its #[cfg(test)] module".into());
            }
            hits.push(hit);
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.cmp(&b.path)));
    hits.truncate(20);
    hits
}

/// One row of `test_runs`.
pub(crate) struct Run {
    pub test: String,
    pub agent: String,
    pub commit: String,
    pub passed: bool,
    pub ms: Option<i64>,
    pub ts: i64,
}

/// The rows of Health's `test_runs`, oldest first, or `None` when there is
/// no such table. Rows from before the sixth build that name a whole
/// command as one test (`runner = 'other'`, the test is the command) are
/// left out: they are counts, not tests.
pub(crate) fn test_runs(sup: &Supervisor) -> Option<Vec<Run>> {
    let rows = sup.db().query("SELECT test, agent, \"commit\", passed, ms, ts, runner, command FROM test_runs LIMIT 50000", &chloedb::query::Params::new()).ok()?;
    let mut out: Vec<Run> = rows
        .iter()
        .filter(|r| !(crate::db::get_str(&r[6]).as_deref() == Some("other") && crate::db::get_str(&r[0]) == crate::db::get_str(&r[7])))
        .map(|r| Run {
            test: crate::db::get_str(&r[0]).unwrap_or_default(),
            agent: crate::db::get_str(&r[1]).unwrap_or_default(),
            commit: crate::db::get_str(&r[2]).unwrap_or_default(),
            passed: match &r[3] {
                chloedb::Value::Bool(b) => *b,
                v => crate::db::get_int(v).is_some_and(|n| n != 0),
            },
            ms: crate::db::get_int(&r[4]),
            ts: crate::db::get_ms(&r[5]).unwrap_or(0),
        })
        .collect();
    out.sort_by_key(|r| r.ts);
    Some(out)
}

/// Whether a run's commit is `head` (either may be abbreviated).
fn same_commit(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

/// What runs of a test say: totals, flaky (passed and failed on one
/// commit), whether each test's last run passed, the runs on `head`, and
/// failures fixed later (a failure before the fix is not a failing test).
fn runs_json(runs: &[&Run], head: Option<&str>) -> Json {
    if runs.is_empty() {
        return Json::Null;
    }
    let passed = runs.iter().filter(|r| r.passed).count();
    let mut by: HashMap<(&str, &str), (bool, bool)> = HashMap::new();
    for r in runs {
        let e = by.entry((r.test.as_str(), r.commit.as_str())).or_default();
        if r.passed {
            e.0 = true;
        } else {
            e.1 = true;
        }
    }
    let flaky = by.values().any(|(p, f)| *p && *f);
    let mut ms: Vec<i64> = runs.iter().filter_map(|r| r.ms).collect();
    ms.sort();
    let agents: HashSet<&str> = runs.iter().map(|r| r.agent.as_str()).filter(|a| !a.is_empty()).collect();
    let mut names: Vec<&str> = runs.iter().map(|r| r.test.as_str()).collect();
    names.sort();
    names.dedup();
    // Per test, its last run decides; a failure followed by a pass on
    // another commit (not a flaky pair) is "fixed".
    let mut last: BTreeMap<&str, &Run> = BTreeMap::new();
    let mut fixed: BTreeSet<&str> = BTreeSet::new();
    for r in runs {
        if let Some(prev) = last.get(r.test.as_str()) {
            let flaky_here = by.get(&(r.test.as_str(), r.commit.as_str())).is_some_and(|(p, f)| *p && *f);
            if !prev.passed && r.passed && prev.commit != r.commit && !flaky_here {
                fixed.insert(r.test.as_str());
            }
        }
        last.insert(r.test.as_str(), r);
    }
    let failing: Vec<&str> = last.values().filter(|r| !r.passed).map(|r| r.test.as_str()).collect();
    let on_head: Vec<&&Run> = runs.iter().filter(|r| head.is_some_and(|h| same_commit(&r.commit, h))).collect();
    let head_passed = on_head.iter().filter(|r| r.passed).count();
    json!({
        "total": runs.len(), "passed": passed, "failed": runs.len() - passed, "flaky": flaky,
        "p50_ms": ms.get(ms.len() / 2), "last_passed": failing.is_empty(), "last": null, "agents": agents.len(),
        "names": names.into_iter().take(6).collect::<Vec<_>>(),
        "failing": failing, "fixed": fixed,
        "on_head": if on_head.is_empty() { Json::Null } else { json!({ "total": on_head.len(), "passed": head_passed, "failed": on_head.len() - head_passed }) },
    })
}

/// A test file's stem (`total` of `tests/cart/total.test.ts`).
fn test_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let name = name.split('.').next().unwrap_or(name);
    name.trim_start_matches("test_").trim_end_matches("_test").trim_end_matches("_spec").trim_end_matches("Test").trim_end_matches("Tests").to_string()
}

/// A path ChloeCtx named, relative to the worktree or the main checkout.
fn rel_of(f: &str, dir: &Path, main: &Path) -> String {
    let (p, _) = crate::map::split_line(f);
    crate::map::rel_path(&p, dir, main).unwrap_or(p)
}

/// How long a blast radius is kept for the same facts.
const KEEP_MS: i64 = 10 * 60_000;

/// The blast radii computed, by data directory and agent: the facts they
/// were computed from (a hash), when, and the answer.
fn cache() -> &'static std::sync::Mutex<HashMap<String, (u64, i64, Json)>> {
    static C: std::sync::OnceLock<std::sync::Mutex<HashMap<String, (u64, i64, Json)>>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// The number of rows of a table, or `None` when there is none.
fn rows_of(sup: &Supervisor, table: &str) -> Option<i64> {
    let r = sup.db().query(&format!("SELECT COUNT(*) FROM {table}"), &chloedb::query::Params::new()).ok()?;
    r.first().and_then(|row| row.first()).and_then(crate::db::get_int)
}

/// `GET /api/agents/:id/blast`: computed once for the same facts (the
/// diff, the branches, the test runs), so every screen shows the same risk
/// for the same change.
pub fn blast(sup: &Supervisor, id: &str) -> Result<Json, ApiError> {
    let a = sup.agent_row(id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
    let dir = PathBuf::from(&a.cwd);
    if !dir.is_dir() {
        return Err(ApiError::conflict(format!("agent {}'s worktree {} is gone", a.name, a.cwd)));
    }
    let main = git::checkout(&dir).map(|c| c.main).unwrap_or_else(|_| PathBuf::from(&a.repo));
    if a.base.is_none() && git::rev_parse(&dir, "HEAD").is_none() {
        return Err(ApiError::conflict(format!("{} has no commit to compare with", a.cwd)));
    }
    let land = landing(sup, &a, &dir, &main);
    let base = land.base.clone();
    let files = git::diff(&dir, &base).map_err(|e| ApiError::internal(format!("git diff failed: {e}")))?;
    let facts = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(&files).unwrap_or_default().hash(&mut h);
        (&land.head, &base).hash(&mut h);
        for u in &land.under {
            u.tip.hash(&mut h);
        }
        for o in sup.agent_rows().iter().filter(|o| o.repo == a.repo && o.id != a.id) {
            (&o.branch, o.branch.as_deref().and_then(|b| git::rev_parse(&main, &format!("refs/heads/{b}"))), &o.state).hash(&mut h);
        }
        (rows_of(sup, "test_runs"), rows_of(sup, "test_summaries")).hash(&mut h);
        h.finish()
    };
    let key = format!("{}\u{1}{id}", sup.cfg.data_dir.display());
    if let Some((f, at, v)) = cache().lock().unwrap().get(&key) {
        if *f == facts && crate::db::now_ms() - at < KEEP_MS {
            return Ok(v.clone());
        }
    }
    let v = compute(sup, &a, &dir, &main, &land, files)?;
    cache().lock().unwrap().insert(key, (facts, crate::db::now_ms(), v.clone()));
    Ok(v)
}

fn compute(sup: &Supervisor, a: &crate::db::Agent, dir: &Path, main: &Path, land: &Landing, files: Vec<Json>) -> Result<Json, ApiError> {
    let (dir, main) = (dir.to_path_buf(), main.to_path_buf());
    let base = land.base.clone();
    let head = land.head.clone();
    let mut missing: Vec<String> = Vec::new();

    // Files and functions.
    let count = |f: &Json, k: &str| f["hunks"].as_array().into_iter().flatten().flat_map(|h| h["lines"].as_array().into_iter().flatten()).filter(|l| l["kind"] == k).count();
    let changed: Vec<String> = files.iter().filter_map(|f| f["path"].as_str().map(str::to_string)).collect();
    let mut funcs = changed_functions(&dir, &base, &files, false);
    find_callers(&dir, &mut funcs);
    let test_funcs = changed_functions(&dir, &base, &files, true);
    let unread: Vec<&str> = files.iter().filter(|f| f["binary"] != true && !is_test_path(f["path"].as_str().unwrap_or("")) && Lang::of(f["path"].as_str().unwrap_or("")).is_none()).filter_map(|f| f["path"].as_str()).collect();
    if !unread.is_empty() && unread.len() == files.len() {
        missing.push(format!("No changed file is in a language the symbol finder reads ({}), so no function is named.", unread.iter().take(3).copied().collect::<Vec<_>>().join(", ")));
    }

    // Tests.
    let hits = find_tests(&dir, &funcs, &changed);
    let runs = test_runs(sup);
    let mut tests: Vec<Json> = Vec::new();
    let mut matched_runs: HashSet<usize> = HashSet::new();
    for h in &hits {
        let stem = test_stem(&h.path).to_ascii_lowercase();
        let fnames: Vec<String> = h.covers.iter().filter_map(|c| funcs.iter().find(|f| &f.id == c)).map(|f| f.name.to_ascii_lowercase()).collect();
        let mine: Vec<&Run> = match &runs {
            Some(rs) => rs
                .iter()
                .enumerate()
                .filter(|(_, r)| {
                    let t = r.test.to_ascii_lowercase();
                    t.contains(&h.path.to_ascii_lowercase()) || (stem.len() >= 3 && t.contains(&stem)) || fnames.iter().any(|n| n.len() >= 3 && t.contains(n.as_str()))
                })
                .map(|(i, r)| {
                    matched_runs.insert(i);
                    r
                })
                .collect(),
            None => Vec::new(),
        };
        tests.push(json!({ "path": h.path, "score": h.score, "covers": h.covers, "why": h.why, "runs": runs_json(&mine, head.as_deref()) }));
    }
    // Runs that name a changed function but match no test file found.
    if let Some(rs) = &runs {
        let mut by_name: BTreeMap<String, (Vec<&Run>, BTreeSet<String>)> = BTreeMap::new();
        for (i, r) in rs.iter().enumerate() {
            if matched_runs.contains(&i) {
                continue;
            }
            let t = r.test.to_ascii_lowercase();
            for f in funcs.iter().filter(|f| searchable(&f.name) && t.contains(&f.name.to_ascii_lowercase())) {
                let e = by_name.entry(r.test.clone()).or_default();
                e.0.push(r);
                e.1.insert(f.id.clone());
            }
        }
        for (name, (rs, covers)) in by_name.into_iter().take(10) {
            let names: Vec<&str> = covers.iter().map(|c| c.rsplit('#').next().unwrap_or(c)).collect();
            tests.push(json!({ "path": name, "score": 1, "covers": covers, "why": [format!("test_runs: names {}", names.join(", "))], "runs": runs_json(&rs, head.as_deref()), "from": "test_runs" }));
        }
    }
    let tests_of = |fid: &str| -> Vec<String> { tests.iter().filter(|t| t["covers"].as_array().is_some_and(|c| c.iter().any(|x| x == fid))).filter_map(|t| t["path"].as_str().map(str::to_string)).collect() };

    // Decisions: blame, records that name the files, the graph.
    let bin = sup.cfg.chloectx_bin.clone();
    let mut decs: BTreeMap<String, Dec> = BTreeMap::new();
    let mut ctx_ok = true;
    let mut blamed = 0;
    let mut by_added: Vec<&Func> = funcs.iter().filter(|f| f.first_added.is_some()).collect();
    by_added.sort_by_key(|f| std::cmp::Reverse(f.added));
    for f in by_added.into_iter().take(MAX_BLAMES) {
        let target = format!("{}:{}", f.path, f.first_added.unwrap());
        match trail::run_chloectx(&bin, &dir, &["blame", &target, "--json", "--no-replay"], Duration::from_secs(30)) {
            Ran::Out(b) => {
                blamed += 1;
                for n in b["notes"].as_array().into_iter().flatten() {
                    let kind = n["kind"].as_str().unwrap_or("");
                    let Some(nid) = n["id"].as_str() else { continue };
                    if !matches!(kind, "decision" | "preference" | "fact" | "rule") {
                        continue;
                    }
                    let d = decs.entry(nid.to_string()).or_insert_with(|| Dec { id: nid.to_string(), kind: kind.to_string(), text: n["text"].as_str().unwrap_or("").to_string(), ts: trail::ctx_time(&n["ts"]), ..Default::default() });
                    d.via.insert("blame");
                    d.functions.insert(f.id.clone());
                    d.line.get_or_insert((f.path.clone(), f.first_added.unwrap()));
                    if d.session.is_none() {
                        d.session = b["edit"]["session"].as_str().map(str::to_string);
                    }
                    if d.files.is_empty() {
                        d.files.push(f.path.clone());
                    }
                }
            }
            Ran::Missing => {
                missing.push(trail::not_installed(&bin) + ": no decisions, and no graph.");
                ctx_ok = false;
                break;
            }
            Ran::Failed(e) => {
                missing.push(format!("{e} (for {target})"));
                break;
            }
        }
    }
    let changed_set: HashSet<&str> = changed.iter().map(String::as_str).collect();
    let funcs_in = |path: &str| -> Vec<String> { funcs.iter().filter(|f| f.path == path).map(|f| f.id.clone()).collect() };
    let mut related: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut graph_ok = false;
    if ctx_ok {
        match crate::map::file_records(sup, &main) {
            Ok(recs) => {
                for r in recs {
                    let files: Vec<String> = r.files.iter().map(|f| rel_of(f, &dir, &main)).collect();
                    let hit: Vec<&String> = files.iter().filter(|f| changed_set.contains(f.as_str())).collect();
                    if hit.is_empty() {
                        continue;
                    }
                    let d = decs.entry(r.id.clone()).or_insert_with(|| Dec { id: r.id.clone(), kind: r.kind.clone(), text: r.text.clone(), ts: r.ts.clone(), session: r.session.clone(), ..Default::default() });
                    d.via.insert("file");
                    d.superseded |= r.tag == "superseded";
                    for p in &hit {
                        // The functions it names, else all of the file's.
                        let named: Vec<String> = funcs.iter().filter(|f| &f.path == *p && (word_count(&r.text, &f.name) > 0 || word_count(&r.ctx, &f.name) > 0)).map(|f| f.id.clone()).collect();
                        d.functions.extend(if named.is_empty() { funcs_in(p) } else { named });
                    }
                    for f in &files {
                        if !d.files.contains(f) {
                            d.files.push(f.clone());
                        }
                    }
                }
            }
            Err(e) => missing.push(e),
        }
        match trail::run_raw(&bin, &main, &["rpc", r#"{"op":"view","what":"graph"}"#], Duration::from_secs(60)) {
            Ran::Out(Json::String(text)) => match serde_json::from_str::<Json>(&text) {
                Ok(g) if g.get("nodes").is_some() => {
                    graph_ok = true;
                    graph_decisions(&g, &dir, &main, &changed_set, &funcs, &mut decs, &mut related);
                }
                _ => missing.push("ChloeCtx's graph could not be read (chloectx rpc view graph printed no graph).".into()),
            },
            Ran::Failed(e) => missing.push(format!("ChloeCtx's graph could not be read: {e}")),
            _ => {}
        }
    }
    let s = Sessions::load(sup);
    let decisions: Vec<Json> = decs
        .values()
        .map(|d| {
            let agent = d.session.as_deref().and_then(|sid| s.agent(sid)).or_else(|| s.of_record(&d.id)).map(|ag| trail::agent_json(sup, ag));
            json!({
                "id": d.id, "kind": d.kind, "text": d.text, "ts": d.ts, "session": d.session, "agent": agent, "files": d.files,
                "via": d.via, "functions": d.functions, "superseded": d.superseded, "superseded_by": d.superseded_by,
                "line": d.line.as_ref().map(|(p, l)| json!({ "path": p, "line": l })), "entity": d.entity,
            })
        })
        .collect();
    let decs_of = |fid: &str| -> Vec<String> { decs.values().filter(|d| d.functions.contains(fid)).map(|d| d.id.clone()).collect() };

    // Other agents.
    let mut agents: Vec<Json> = Vec::new();
    let mut agents_of: HashMap<String, Vec<String>> = HashMap::new();
    let under_branches: HashSet<&str> = land.under.iter().map(|u| u.branch.as_str()).collect();
    let above_ids: HashSet<&str> = land.above.iter().map(|(o, _)| o.id.as_str()).collect();
    for mut w in crate::map::territory(sup, &main) {
        if w.agent.id == a.id || a.cwd == w.agent.cwd && a.cwd != a.repo {
            continue;
        }
        // A branch this one holds is not in its way: its work is in here.
        if w.agent.branch.as_deref().is_some_and(|b| under_branches.contains(b)) {
            continue;
        }
        // A branch on top of this one: only what it adds counts.
        let stacked = above_ids.contains(w.agent.id.as_str());
        if stacked {
            if let Some(h) = head.as_deref() {
                match git::diff(Path::new(&w.agent.cwd), h) {
                    Ok(fs) => {
                        w.files = fs
                            .iter()
                            .map(|f| (f["path"].as_str().unwrap_or("").to_string(), count(f, "+") as i64, count(f, "-") as i64, f["status"].as_str().unwrap_or("modified").to_string()))
                            .collect()
                    }
                    Err(_) => w.files.clear(),
                }
            }
        }
        let theirs: Vec<&str> = w.files.iter().map(|f| f.0.as_str()).collect();
        let mut kinds = Vec::new();
        let mut on_files = BTreeSet::new();
        let mut on_funcs = BTreeSet::new();
        let mut detail = Vec::new();
        let mut clean: Option<bool> = None;
        let same: Vec<&str> = theirs.iter().copied().filter(|p| changed_set.contains(p)).collect();
        if !same.is_empty() {
            kinds.push("same_file");
            for p in &same {
                on_files.insert(p.to_string());
                on_funcs.extend(funcs_in(p));
                let f = w.files.iter().find(|f| f.0 == *p).unwrap();
                detail.push(format!("{} also changed {p} (+{} −{})", w.agent.name, f.1, f.2));
            }
            // Committed on both sides: whether the two branches merge.
            let tip = w.agent.branch.as_deref().and_then(|b| git::rev_parse(&main, &format!("refs/heads/{b}")));
            if let (Some(h), Some(t), false) = (head.as_deref(), tip.as_deref(), stacked) {
                let committed = |from: &str, to: &str| -> HashSet<String> { git_out(&dir, &["diff", "--name-only", from, to]).unwrap_or_default().lines().map(str::to_string).collect() };
                let fork_t = merge_base(&dir, h, t).unwrap_or_default();
                let (mine, other) = (committed(&fork_t, h), committed(&fork_t, t));
                if !fork_t.is_empty() && same.iter().all(|p| mine.contains(*p) && other.contains(*p)) {
                    if let Some((ok, conflicts)) = merges_clean(&dir, h, t) {
                        clean = Some(ok);
                        detail.push(if ok {
                            format!("the two branches merge without a conflict (git merge-tree of {} and {})", &h[..h.len().min(7)], &t[..t.len().min(7)])
                        } else {
                            format!("merging the two branches conflicts in {}", if conflicts.is_empty() { same.join(", ") } else { conflicts.join(", ") })
                        });
                    }
                }
            }
        }
        let calls: Vec<(&str, &Func)> = funcs.iter().flat_map(|f| f.caller_files.iter().filter(|c| theirs.contains(&c.as_str()) && !changed_set.contains(c.as_str())).map(move |c| (c.as_str(), f))).collect();
        if !calls.is_empty() {
            kinds.push("calls_changed");
            for (p, f) in &calls {
                on_files.insert(p.to_string());
                on_funcs.insert(f.id.clone());
            }
            let (p, f) = calls[0];
            detail.push(format!("{} changed {p}, which calls {}", w.agent.name, f.name));
        }
        let reading: Vec<&(String, i64)> = w.reading.iter().filter(|(p, _)| changed_set.contains(p.as_str()) && !same.contains(&p.as_str())).collect();
        if !reading.is_empty() {
            kinds.push("reading");
            for (p, _) in &reading {
                on_files.insert(p.clone());
                on_funcs.extend(funcs_in(p));
            }
            let (p, t) = reading[0];
            detail.push(format!("{} read {p} {} min ago", w.agent.name, ((crate::db::now_ms() - t) / 60_000).max(0)));
        }
        if kinds.is_empty() {
            continue;
        }
        for f in &on_funcs {
            agents_of.entry(f.clone()).or_default().push(w.agent.id.clone());
        }
        agents.push(json!({ "agent": trail::agent_json(sup, &w.agent), "kinds": kinds, "files": on_files, "functions": on_funcs, "detail": detail.join("; "), "merges_clean": clean, "stacked_on_this": stacked }));
    }

    // The functions as JSON.
    let functions: Vec<Json> = funcs
        .iter()
        .map(|f| {
            json!({
                "id": f.id, "name": f.name, "kind": f.kind, "lang": f.lang.name(), "path": f.path, "line": f.line, "status": f.status,
                "added": f.added, "removed": f.removed, "callers": f.callers, "callers_total": f.callers_total, "test_callers": f.test_callers,
                "tests": tests_of(&f.id), "decisions": decs_of(&f.id), "agents": agents_of.get(&f.id).cloned().unwrap_or_default(),
            })
        })
        .collect();
    let files_json: Vec<Json> = files
        .iter()
        .map(|f| {
            let p = f["path"].as_str().unwrap_or("");
            json!({ "path": p, "status": f["status"], "added": count(f, "+"), "removed": count(f, "-"), "functions": funcs_in(p) })
        })
        .collect();
    let added: usize = files.iter().map(|f| count(f, "+")).sum();
    let removed: usize = files.iter().map(|f| count(f, "-")).sum();
    let callers: usize = funcs.iter().map(|f| f.callers_total).sum();
    let test_callers: usize = funcs.iter().map(|f| f.test_callers).sum();
    let test_functions: Vec<Json> = test_funcs
        .iter()
        .map(|f| json!({ "id": f.id, "name": f.name, "kind": f.kind, "path": f.path, "line": f.line, "status": f.status, "added": f.added, "removed": f.removed }))
        .collect();
    let totals = json!({
        "files": files.len(), "added": added, "removed": removed, "functions": funcs.len(), "callers": callers, "test_callers": test_callers,
        "tests": tests.len(), "decisions": decs.len(), "agents": agents.len(), "test_functions": test_funcs.len(),
    });
    let mut risk = risk(&functions, &tests, &decisions, &agents, callers);
    if let Some(l) = land.line() {
        risk["stack"] = json!(l);
    }
    let related_files: Vec<Json> = related.into_iter().filter(|(p, _)| !changed_set.contains(p.as_str())).take(12).map(|(p, via)| json!({ "path": p, "via": via })).collect();
    if runs.is_none() {
        missing.push("There is no test_runs table (Health keeps one), so tests are ranked by the test files that name the functions or import their files, with no runs.".into());
    }
    Ok(json!({
        "agent": trail::agent_json(sup, &a), "repo": a.repo, "base": base, "head": head, "branch": a.branch,
        "totals": totals, "risk": risk, "files": files_json, "functions": functions, "tests": tests, "decisions": decisions,
        "related_files": related_files, "agents": agents, "test_functions": test_functions, "landing": land.json(sup),
        "sources": { "test_runs": runs.is_some(), "chloectx": ctx_ok, "graph": graph_ok, "blame": blamed },
        "computed": crate::db::iso(crate::db::now_ms()),
        "missing": missing,
    }))
}

/// Decisions from ChloeCtx's graph (`view graph`: notes, entities, files,
/// sessions): the notes that touch a changed file, and one hop further,
/// the notes about the same entities; and the other files they touch.
#[allow(clippy::too_many_arguments)]
fn graph_decisions(g: &Json, dir: &Path, main: &Path, changed: &HashSet<&str>, funcs: &[Func], decs: &mut BTreeMap<String, Dec>, related: &mut BTreeMap<String, BTreeSet<String>>) {
    let nodes: HashMap<&str, &Json> = g["nodes"].as_array().into_iter().flatten().filter_map(|n| Some((n["id"].as_str()?, n))).collect();
    let mut touches: HashMap<&str, Vec<String>> = HashMap::new();
    let mut about: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut replaced_by: HashMap<&str, &str> = HashMap::new();
    for e in g["edges"].as_array().into_iter().flatten() {
        let (Some(s), Some(t), Some(l)) = (e["s"].as_str(), e["t"].as_str(), e["label"].as_str()) else { continue };
        match l {
            "touches" => touches.entry(s).or_default().push(rel_of(t.strip_prefix("f:").unwrap_or(t), dir, main)),
            "about" => about.entry(s).or_default().push(t),
            "supersedes" => {
                replaced_by.insert(t, s);
            }
            _ => {}
        }
    }
    let is_note = |id: &str| nodes.get(id).is_some_and(|n| n["type"] == "note" && matches!(n["kind"].as_str(), Some("decision" | "preference" | "fact" | "rule")));
    let add = |id: &str, fns: Vec<String>, entity: Option<&str>, decs: &mut BTreeMap<String, Dec>| {
        let n = nodes[id];
        let d = decs.entry(id.to_string()).or_insert_with(|| Dec {
            id: id.to_string(),
            kind: n["kind"].as_str().unwrap_or("decision").to_string(),
            text: n["text"].as_str().or(n["label"].as_str()).unwrap_or("").to_string(),
            ts: trail::ctx_time(&n["ts"]),
            session: n["session"].as_str().map(str::to_string),
            ..Default::default()
        });
        d.via.insert("graph");
        d.functions.extend(fns);
        if n["current"] == false {
            d.superseded = true;
        }
        if let Some(by) = replaced_by.get(id) {
            d.superseded = true;
            d.superseded_by = Some(by.to_string());
        }
        if d.entity.is_none() {
            d.entity = entity.map(|e| nodes.get(e).and_then(|n| n["label"].as_str()).unwrap_or(e.trim_start_matches("e:")).to_string());
        }
        for f in touches.get(id).into_iter().flatten() {
            if !d.files.contains(f) {
                d.files.push(f.clone());
            }
        }
    };
    let mut first: Vec<&str> = Vec::new();
    for (id, files) in &touches {
        if !is_note(id) {
            continue;
        }
        let hit: Vec<&String> = files.iter().filter(|f| changed.contains(f.as_str())).collect();
        if hit.is_empty() {
            continue;
        }
        let fns: Vec<String> = funcs.iter().filter(|f| hit.contains(&&f.path)).map(|f| f.id.clone()).collect();
        add(id, fns, None, decs);
        first.push(id);
    }
    // One hop: notes about an entity a first note is about.
    let mut hops = 0;
    for id in first.clone() {
        let fns: Vec<String> = decs.get(id).map(|d| d.functions.iter().cloned().collect()).unwrap_or_default();
        for e in about.get(id).cloned().unwrap_or_default() {
            for (other, ents) in &about {
                if hops >= 10 || *other == id || first.contains(other) || !is_note(other) || !ents.contains(&e) {
                    continue;
                }
                add(other, fns.clone(), Some(e), decs);
                hops += 1;
            }
        }
    }
    // Other files the decisions about these lines touch.
    for d in decs.values() {
        for f in touches.get(d.id.as_str()).into_iter().flatten() {
            if !changed.contains(f.as_str()) {
                related.entry(f.clone()).or_default().insert(d.id.clone());
            }
        }
    }
}

/// The risk: a level, a score, a sentence, and its reasons.
fn risk(functions: &[Json], tests: &[Json], decisions: &[Json], agents: &[Json], callers: usize) -> Json {
    // Uses in test files are counted apart, on each function.
    let test_callers: usize = functions.iter().filter_map(|f| f["test_callers"].as_u64()).sum::<u64>() as usize;
    let mut score = 0i64;
    let mut reasons = Vec::new();
    let untested: Vec<&Json> = functions.iter().filter(|f| f["status"] != "deleted" && f["tests"].as_array().is_none_or(|t| t.is_empty())).collect();
    if !untested.is_empty() {
        score += (2 * untested.len() as i64).min(6);
        reasons.push(format!("{} {} no test that names {}: {}", plural(untested.len(), "changed function"), if untested.len() == 1 { "has" } else { "have" }, if untested.len() == 1 { "it" } else { "them" }, names_of(&untested).join(", ")));
    }
    let gone: Vec<&Json> = functions.iter().filter(|f| f["status"] == "deleted" && f["callers_total"].as_u64().unwrap_or(0) > 0).collect();
    if !gone.is_empty() {
        score += 3 * gone.len() as i64;
        reasons.push(format!("{} removed but still called: {}", plural(gone.len(), "function"), names_of(&gone).join(", ")));
    }
    let kind_of = |g: &Json, k: &str| g["kinds"].as_array().is_some_and(|ks| ks.iter().any(|x| x == k));
    for g in agents {
        if kind_of(g, "same_file") {
            // Proven to merge cleanly: a note, not a collision; a conflict weighs more.
            score += match g["merges_clean"].as_bool() {
                Some(true) => 1,
                Some(false) => 4,
                None => 3,
            };
        } else if kind_of(g, "calls_changed") {
            score += 2;
        } else {
            score += 1;
        }
        if let Some(d) = g["detail"].as_str() {
            reasons.push(d.to_string());
        }
    }
    for t in tests {
        let r = &t["runs"];
        if r["flaky"] == true {
            score += 1;
            reasons.push(format!("{} is flaky: passed and failed on one commit", t["path"].as_str().unwrap_or("")));
        } else if r["last_passed"] == false {
            score += 2;
            let names: Vec<&str> = r["failing"].as_array().into_iter().flatten().filter_map(Json::as_str).take(3).collect();
            reasons.push(if names.is_empty() { format!("{} failed its last run", t["path"].as_str().unwrap_or("")) } else { format!("{} failed its last run ({})", t["path"].as_str().unwrap_or(""), names.join(", ")) });
        }
    }
    let old: Vec<&str> = decisions.iter().filter(|d| d["superseded"] == true).filter_map(|d| d["id"].as_str()).collect();
    if !old.is_empty() {
        score += old.len() as i64;
        reasons.push(format!("{} about these lines {} superseded: {}", plural(old.len(), "decision"), if old.len() == 1 { "was" } else { "were" }, old.join(", ")));
    }
    if callers > 10 {
        score += 1;
    }
    let level = if score < 3 { "low" } else if score < 13 { "medium" } else { "high" };
    let callers_said = if callers == 0 && test_callers > 0 { format!("no caller outside tests and {} in tests", test_callers) } else { plural(callers, "caller") };
    let mut parts = vec![format!("{} changed with {callers_said}", plural(functions.len(), "function"))];
    if functions.is_empty() {
        parts[0] = "no function changed".into();
    }
    if !untested.is_empty() {
        let n = names_of(&untested);
        let shown = if n.len() > 3 { format!("{} and {} more", n[..3].join(", "), n.len() - 3) } else { join_and(&n) };
        parts.push(format!("{shown} {} no test that names {}", if n.len() == 1 { "has" } else { "have" }, if n.len() == 1 { "it" } else { "them" }));
    }
    if let Some(g) = agents.iter().find(|g| kind_of(g, "same_file")).or_else(|| agents.first()) {
        let fname = g["functions"].as_array().and_then(|f| f.first()).and_then(Json::as_str).map(|f| f.rsplit('#').next().unwrap_or(f).to_string());
        let who = g["agent"]["name"].as_str().unwrap_or("another agent");
        parts.push(if kind_of(g, "same_file") {
            let file = g["files"].as_array().and_then(|f| f.first()).and_then(Json::as_str).unwrap_or("the same file");
            match g["merges_clean"].as_bool() {
                Some(true) => format!("{who} changes {file} too, and the two branches merge without a conflict"),
                Some(false) => format!("{who} changes {file} too, and merging the two conflicts"),
                None => format!("{who} changes {file} too"),
            }
        } else if kind_of(g, "calls_changed") {
            format!("{who} calls {}", fname.unwrap_or_else(|| "a changed function".into()))
        } else {
            format!("{who} is reading these files")
        });
    }
    let line = match parts.len() {
        1 => parts[0].clone(),
        _ => format!("{}; {}", parts[0], join_and(&parts[1..].iter().map(String::as_str).collect::<Vec<_>>())),
    };
    let cap = |s: &str| s[..1].to_uppercase() + &s[1..];
    json!({ "level": level, "score": score, "line": format!("{} risk: {line}.", cap(level)), "reasons": reasons })
}

fn names_of<'a>(v: &[&'a Json]) -> Vec<&'a str> {
    v.iter().filter_map(|f| f["name"].as_str()).collect()
}

fn join_and(v: &[&str]) -> String {
    match v {
        [] => String::new(),
        [a] => a.to_string(),
        [a, b] => format!("{a} and {b}"),
        _ => format!("{}, and {}", v[..v.len() - 1].join(", "), v[v.len() - 1]),
    }
}

pub(crate) fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_definitions_per_language() {
        let rs = "pub(crate) fn total(cart: &Cart) -> i64 {\n    let x = 1;\n    x\n}\n\nimpl Cart {\n    pub async fn tax_for(&self) -> i64 {\n        0\n    }\n}\nstruct Line;\n";
        let d = defs(Lang::Rust, rs);
        assert_eq!(d.iter().map(|d| (d.name.as_str(), d.line, d.end)).collect::<Vec<_>>(), [("total", 1, 4), ("tax_for", 7, 9), ("Line", 11, 11)]);
        assert_eq!(enclosing(&d, 8).unwrap().name, "tax_for");
        assert_eq!(enclosing(&d, 3).unwrap().name, "total");
        assert!(enclosing(&d, 5).is_none());

        let ts = "export function cartTotal(cart: Cart): number {\n  return 1;\n}\nexport const taxFor = (line: Line, rate: number) => {\n  return 2;\n};\nclass RateTable {\n  rateFor(addr: Address): number {\n    if (x) {\n      return 0;\n    }\n  }\n}\nconst LIMIT = 5;\n";
        let d = defs(Lang::Js, ts);
        let names: Vec<&str> = d.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["cartTotal", "taxFor", "RateTable", "rateFor"]);
        assert_eq!(enclosing(&d, 10).unwrap().name, "rateFor");
        assert_eq!(enclosing(&d, 5).unwrap().name, "taxFor");

        let py = "class Cart:\n    def total(self):\n        return 1\n\n    async def tax(self):\n        return 2\n\ndef helper():\n    pass\n";
        let d = defs(Lang::Python, py);
        assert_eq!(d.iter().map(|d| (d.name.as_str(), d.end)).collect::<Vec<_>>(), [("Cart", 6), ("total", 3), ("tax", 6), ("helper", 9)]);
        assert_eq!(enclosing(&d, 6).unwrap().name, "tax");

        let go = "func (c *Cart) Total() int {\n\treturn 1\n}\ntype Rate struct {\n}\nfunc helper() {}\n";
        assert_eq!(defs(Lang::Go, go).iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Total", "Rate", "helper"]);
        let java = "public class Cart {\n    public static long total(List<Line> lines) {\n        return 0;\n    }\n    if (x) {\n    }\n}\n";
        assert_eq!(defs(Lang::Java, java).iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["Cart", "total"]);
        let c = "static int\ntotal(struct cart *c)\n{\n  return 0;\n}\nint x = f(1);\n";
        assert_eq!(defs(Lang::C, c).iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["total"]);
        let rb = "class Cart\n  def total\n    1\n  end\nend\n";
        assert_eq!(defs(Lang::Ruby, rb).iter().map(|d| (d.name.as_str(), d.end)).collect::<Vec<_>>(), [("Cart", 5), ("total", 4)]);
    }

    #[test]
    fn tells_test_files_and_imports() {
        for p in ["tests/cart.rs", "src/cart/total.test.ts", "e2e/checkout.spec.ts", "pkg/cart_test.go", "test_cart.py", "app/__tests__/x.js", "spec/cart_spec.rb", "src/test/java/CartTest.java"] {
            assert!(is_test_path(p), "{p}");
        }
        for p in ["src/cart/total.ts", "src/testing.rs", "latest.py"] {
            assert!(!is_test_path(p), "{p}");
        }
        assert_eq!(module_names("src/cart/total.ts"), ["cart/total", "cart::total", "cart.total"]);
        assert!(imports("import { cartTotal } from \"../../src/cart/total\";\n", &module_names("src/cart/total.ts")));
        assert!(imports("use shop::cart::total::cart_total;\n", &module_names("src/cart/total.rs")));
        assert!(!imports("const total = 1; // cart/total\n", &module_names("src/cart/total.ts")));
        assert_eq!(word_count("cartTotal(cartTotalX) cartTotal", "cartTotal"), 2);
        assert_eq!(test_stem("tests/cart/total.test.ts"), "total");
        assert_eq!(test_stem("tests/test_rates.py"), "rates");
    }

    #[test]
    fn says_the_risk_in_a_sentence() {
        let f = |name: &str, tests: Json| json!({ "name": name, "status": "changed", "tests": tests, "callers_total": 3 });
        let r = risk(&[f("cartTotal", json!(["t"])), f("rateFor", json!([]))], &[], &[], &[json!({ "agent": { "name": "cart-ui" }, "kinds": ["same_file"], "files": ["src/cart.ts"], "functions": ["a#cartTotal"], "detail": "cart-ui also changed src/cart.ts" })], 12);
        assert_eq!(r["level"], "medium");
        assert_eq!(r["line"], "Medium risk: 2 functions changed with 12 callers; rateFor has no test that names it and cart-ui changes src/cart.ts too.");
        let r = risk(&[f("cartTotal", json!(["t"]))], &[], &[], &[], 1);
        assert_eq!(r["level"], "low");
        assert_eq!(r["line"], "Low risk: 1 function changed with 1 caller.");
    }

    #[test]
    fn counts_callers_in_tests_and_a_clean_merge_is_a_note() {
        // Callers only in tests are callers; a branch proven to merge is a note.
        let f = json!({ "name": "add", "status": "changed", "tests": ["t"], "callers_total": 0, "test_callers": 12 });
        let g = json!({ "agent": { "name": "freeship" }, "kinds": ["same_file"], "files": ["cart.py"], "functions": [], "detail": "freeship also changed cart.py", "merges_clean": true });
        let r = risk(&[f], &[], &[], &[g], 0);
        assert_eq!(r["level"], "low", "{r}");
        assert_eq!(r["line"], "Low risk: 1 function changed with no caller outside tests and 12 in tests; freeship changes cart.py too, and the two branches merge without a conflict.");
    }
}
