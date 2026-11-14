//! Docs on tap (SPEC.md, "Fourth build", row 10): the repository's
//! dependencies, read from its manifests (`Cargo.toml` with `Cargo.lock`,
//! `package.json` with `node_modules`, `pyproject.toml`,
//! `requirements.txt`, `go.mod`), each mapped to its doc site (docs.rs,
//! the project's own site for well-known npm and PyPI packages, else its
//! homepage or its registry page, pkg.go.dev). A crawl fetches the site's
//! first page and the pages it links to under the same folder of the same
//! host (depth 1, at most 200 pages), as robots.txt allows, with `curl`
//! (which has TLS and the machine's proxy; this crate's HTTP client has
//! neither), and keeps each page's text as a row of `doc_pages`.
//! `chloedb-crawl` is not used: it crawls into a cluster it starts, and the
//! pages belong in the state database beside the code.
//!
//! `GET /api/library?repo`, `GET /api/library/:id/pages?repo`,
//! `POST /api/library/crawl {repo, library | libraries | url}`,
//! `GET /api/search/docs?repo&q&library&tokens&limit&by` (an agent's
//! search, `by`, records the pages it was answered as cited).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::db::{get_int, get_ms, get_str, now_ms, ts};
use crate::levels::{blocking, parse_body, ApiResult};
use crate::server::App;
use crate::supervisor::{ApiError, Supervisor};
use crate::{git, search, trail};

/// Pages a crawl keeps at most, and how many manifests and libraries are read.
pub const MAX_PAGES: usize = 200;
const MAX_MANIFESTS: usize = 12;
const MAX_LIBS: usize = 60;
/// The text kept of a page, in characters.
const MAX_TEXT: usize = 100_000;
/// The crawler's name, as robots.txt groups know it, and its User-Agent.
const ROBOT: &str = "chloedb-workbench";
const USER_AGENT: &str = "chloedb-workbench/0.1 (docs for coding agents; robots.txt respected)";

pub fn routes() -> Router<App> {
    Router::new()
        .route("/api/library", get(list_route))
        .route("/api/library/crawl", post(crawl_route))
        .route("/api/library/:id/pages", get(pages_route))
        .route("/api/search/docs", get(docs_route))
}

fn q(sup: &Supervisor, sql: &str, p: &Params) -> Result<Vec<Vec<Value>>, String> {
    sup.db().query(sql, p)
}

fn sv(s: &str) -> Value {
    Value::Str(s.to_string())
}

// ----------------------------------------------------------- manifests

/// A dependency, as a manifest names it.
#[derive(Clone, Debug, PartialEq)]
pub struct Dep {
    pub name: String,
    pub version: Option<String>,
    pub ecosystem: &'static str,
    pub manifest: String,
    pub url: String,
}

/// The manifests among a checkout's files, shallowest first.
pub fn manifests(files: &[String]) -> Vec<String> {
    const NAMES: [&str; 5] = ["Cargo.toml", "package.json", "pyproject.toml", "requirements.txt", "go.mod"];
    let mut m: Vec<&String> = files
        .iter()
        .filter(|f| NAMES.contains(&f.rsplit('/').next().unwrap_or(f)))
        .filter(|f| !["examples/", "fixtures/", "testdata/", "test/", "tests/"].iter().any(|d| f.starts_with(d) || f.contains(&format!("/{d}"))))
        .collect();
    m.sort_by_key(|f| (f.matches('/').count(), (*f).clone()));
    m.into_iter().take(MAX_MANIFESTS).cloned().collect()
}

/// A TOML string's value: `"x"` or `'x'`.
fn toml_str(v: &str) -> Option<String> {
    let v = v.trim();
    let quote = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let rest = &v[1..];
    rest.find(quote).map(|end| rest[..end].to_string())
}

/// `key = value` of an inline table: `{ version = "1", features = [..] }`.
fn inline_field(table: &str, key: &str) -> Option<String> {
    let inner = table.trim().trim_start_matches('{').trim_end_matches('}');
    for part in inner.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            if k.trim() == key {
                return toml_str(v).or_else(|| Some(v.trim().to_string()));
            }
        }
    }
    None
}

fn clean_version(v: &str) -> Option<String> {
    let v = v.trim().trim_start_matches(['^', '~', '=', '>', '<', 'v', ' ']).split([',', ' ', '|']).next().unwrap_or("").trim();
    (!v.is_empty() && v.chars().next().is_some_and(|c| c.is_ascii_digit())).then(|| v.to_string())
}

/// `Cargo.lock`'s versions by crate name.
pub fn cargo_lock(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut name: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if t == "[[package]]" {
            name = None;
        } else if let Some(v) = t.strip_prefix("name =") {
            name = toml_str(v);
        } else if let Some(v) = t.strip_prefix("version =") {
            if let (Some(n), Some(v)) = (name.take(), toml_str(v)) {
                out.insert(n, v);
            }
        }
    }
    out
}

/// A `Cargo.toml`'s dependencies (not dev or build ones): `(name, version)`.
pub fn cargo_deps(text: &str, lock: &HashMap<String, String>) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut section = String::new();
    let mut table_dep: Option<(String, Option<String>, bool)> = None;
    let flush = |d: &mut Option<(String, Option<String>, bool)>, out: &mut Vec<(String, Option<String>)>| {
        if let Some((n, v, local)) = d.take() {
            if !local {
                let v = lock.get(&n).cloned().or(v.and_then(|v| clean_version(&v)));
                out.push((n, v));
            }
        }
    };
    let is_deps = |s: &str| s == "dependencies" || s == "workspace.dependencies" || (s.starts_with("target.") && s.ends_with(".dependencies"));
    for line in text.lines() {
        let t = line.split(" #").next().unwrap_or(line).trim();
        if t.starts_with('[') {
            flush(&mut table_dep, &mut out);
            section = t.trim_matches(['[', ']']).trim().to_string();
            // `[dependencies.serde]`
            if let Some(n) = section.strip_prefix("dependencies.").or_else(|| section.strip_prefix("workspace.dependencies.")) {
                table_dep = Some((n.trim_matches('"').to_string(), None, false));
            }
            continue;
        }
        if let Some(d) = table_dep.as_mut() {
            if let Some((k, v)) = t.split_once('=') {
                match k.trim() {
                    "version" => d.1 = toml_str(v),
                    "path" | "workspace" => d.2 = true,
                    "package" => d.0 = toml_str(v).unwrap_or(d.0.clone()),
                    _ => {}
                }
            }
            continue;
        }
        if !is_deps(&section) {
            continue;
        }
        let Some((k, v)) = t.split_once('=') else { continue };
        let name = k.trim().trim_matches('"').to_string();
        if name.is_empty() || name.contains(' ') {
            continue;
        }
        let v = v.trim();
        if v.starts_with('{') {
            if inline_field(v, "path").is_some() && inline_field(v, "version").is_none() || inline_field(v, "workspace").is_some() || inline_field(v, "git").is_some() {
                continue;
            }
            let name = inline_field(v, "package").unwrap_or(name);
            let ver = lock.get(&name).cloned().or_else(|| inline_field(v, "version").and_then(|v| clean_version(&v)));
            out.push((name, ver));
        } else {
            let ver = lock.get(&name).cloned().or_else(|| toml_str(v).and_then(|v| clean_version(&v)));
            out.push((name, ver));
        }
    }
    flush(&mut table_dep, &mut out);
    out
}

/// A `package.json`'s dependencies: `(name, version spec)`.
pub fn npm_deps(text: &str) -> Vec<(String, String)> {
    let v: Json = serde_json::from_str(text).unwrap_or(Json::Null);
    let mut out = Vec::new();
    for key in ["dependencies", "peerDependencies"] {
        for (n, spec) in v[key].as_object().into_iter().flatten() {
            let spec = spec.as_str().unwrap_or("");
            if ["file:", "link:", "workspace:", "git", "http", "portal:"].iter().any(|p| spec.starts_with(p)) || out.iter().any(|(m, _): &(String, String)| m == n) {
                continue;
            }
            out.push((n.clone(), spec.to_string()));
        }
    }
    out
}

/// A requirement's name: `fastapi[all]>=0.110` → fastapi.
fn py_name(req: &str) -> Option<String> {
    let n: String = req.trim().chars().take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
    let n = n.to_lowercase().replace('_', "-");
    (!n.is_empty() && n != "python").then_some(n)
}

fn py_version(req: &str) -> Option<String> {
    let i = req.find("==").map(|i| i + 2).or_else(|| req.find(">=").map(|i| i + 2)).or_else(|| req.find("~=").map(|i| i + 2))?;
    clean_version(req[i..].split([';', ',']).next().unwrap_or(""))
}

/// A `pyproject.toml`'s dependencies (PEP 621's list, or Poetry's table).
pub fn pyproject_deps(text: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut in_list = false;
    for line in text.lines() {
        let t = line.split(" #").next().unwrap_or(line).trim();
        if t.starts_with('[') && !in_list {
            section = t.trim_matches(['[', ']']).trim().to_string();
            continue;
        }
        if section == "project" && (t.starts_with("dependencies") && t.contains('=')) {
            let after = t.split_once('=').map(|x| x.1).unwrap_or("").trim();
            in_list = !after.contains(']');
            for s in after.trim_start_matches('[').split(',') {
                if let Some(r) = toml_str(s) {
                    if let Some(n) = py_name(&r) {
                        out.push((n, py_version(&r)));
                    }
                }
            }
            continue;
        }
        if in_list {
            if let Some(r) = toml_str(t.trim_end_matches(',')) {
                if let Some(n) = py_name(&r) {
                    out.push((n, py_version(&r)));
                }
            }
            if t.contains(']') {
                in_list = false;
            }
            continue;
        }
        if section == "tool.poetry.dependencies" {
            if let Some((k, v)) = t.split_once('=') {
                if let Some(n) = py_name(k.trim().trim_matches('"')) {
                    let ver = toml_str(v).or_else(|| inline_field(v, "version")).and_then(|v| clean_version(&v));
                    out.push((n, ver));
                }
            }
        }
    }
    out
}

/// A `requirements.txt`'s requirements.
pub fn requirements_deps(text: &str) -> Vec<(String, Option<String>)> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty() && !l.starts_with('-'))
        .filter_map(|l| py_name(l).map(|n| (n, py_version(l))))
        .collect()
}

/// A `go.mod`'s direct requirements: `(module, version)`.
pub fn go_deps(text: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut block = false;
    for line in text.lines() {
        let t = line.trim();
        let req = if block {
            if t == ")" {
                block = false;
                continue;
            }
            t
        } else if t == "require (" {
            block = true;
            continue;
        } else if let Some(r) = t.strip_prefix("require ") {
            r
        } else {
            continue;
        };
        if req.contains("// indirect") || req.is_empty() || req.starts_with("//") {
            continue;
        }
        let mut it = req.split_whitespace();
        if let (Some(m), v) = (it.next(), it.next()) {
            out.push((m.to_string(), v.map(str::to_string)));
        }
    }
    out
}

/// Doc sites of well-known npm and PyPI packages whose registry page is not
/// where their docs are.
fn known(ecosystem: &str, name: &str) -> Option<&'static str> {
    Some(match (ecosystem, name) {
        ("npm", "react") | ("npm", "react-dom") => "https://react.dev/reference/react",
        ("npm", "next") => "https://nextjs.org/docs",
        ("npm", "vue") => "https://vuejs.org/guide/",
        ("npm", "svelte") => "https://svelte.dev/docs",
        ("npm", "zod") => "https://zod.dev/",
        ("npm", "express") => "https://expressjs.com/en/api.html",
        ("npm", "typescript") => "https://www.typescriptlang.org/docs/",
        ("npm", "vite") => "https://vite.dev/guide/",
        ("npm", "vitest") => "https://vitest.dev/guide/",
        ("npm", "tailwindcss") => "https://tailwindcss.com/docs",
        ("npm", "lodash") => "https://lodash.com/docs/",
        ("npm", "axios") => "https://axios-http.com/docs/intro",
        ("npm", "@tanstack/react-query") => "https://tanstack.com/query/latest/docs/framework/react/overview",
        ("npm", "playwright") | ("npm", "@playwright/test") => "https://playwright.dev/docs/intro",
        ("pypi", "fastapi") => "https://fastapi.tiangolo.com/",
        ("pypi", "django") => "https://docs.djangoproject.com/en/stable/",
        ("pypi", "flask") => "https://flask.palletsprojects.com/en/stable/",
        ("pypi", "numpy") => "https://numpy.org/doc/stable/",
        ("pypi", "pandas") => "https://pandas.pydata.org/docs/",
        ("pypi", "requests") => "https://requests.readthedocs.io/en/latest/",
        ("pypi", "pydantic") => "https://docs.pydantic.dev/latest/",
        ("pypi", "sqlalchemy") => "https://docs.sqlalchemy.org/en/20/",
        ("pypi", "pytest") => "https://docs.pytest.org/en/stable/",
        ("pypi", "httpx") => "https://www.python-httpx.org/",
        _ => return None,
    })
}

/// Where a dependency's docs are.
pub fn doc_url(ecosystem: &str, name: &str, version: Option<&str>, homepage: Option<&str>) -> String {
    if let Some(u) = known(ecosystem, name) {
        return u.to_string();
    }
    match ecosystem {
        "crates" => format!("https://docs.rs/{name}/{}/", version.unwrap_or("latest")),
        "go" => match version {
            Some(v) => format!("https://pkg.go.dev/{name}@{v}"),
            None => format!("https://pkg.go.dev/{name}"),
        },
        _ => match homepage.filter(|h| h.starts_with("http") && !h.contains("github.com")) {
            Some(h) => h.split('#').next().unwrap_or(h).to_string(),
            None if ecosystem == "npm" => format!("https://www.npmjs.com/package/{name}"),
            None => format!("https://pypi.org/project/{name}/"),
        },
    }
}

/// A dependency's id in the API: `crates-serde`, `npm-tanstack-react-query`.
pub fn lib_id(ecosystem: &str, name: &str) -> String {
    let mut s = format!("{ecosystem}-");
    let mut dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            s.push(c);
            dash = false;
        } else if !dash && !s.ends_with('-') {
            s.push('-');
            dash = true;
        }
    }
    s.trim_end_matches('-').to_string()
}

/// The dependencies of a checkout, from its manifests.
pub fn discover(dir: &Path, files: &[String]) -> (Vec<String>, Vec<Dep>) {
    let found = manifests(files);
    let mut deps: Vec<Dep> = Vec::new();
    let mut seen = HashSet::new();
    for m in &found {
        let full = dir.join(m);
        let Ok(text) = std::fs::read_to_string(&full) else { continue };
        let folder = full.parent().unwrap_or(dir).to_path_buf();
        let base = m.rsplit('/').next().unwrap_or(m);
        let list: Vec<(&'static str, String, Option<String>, Option<String>)> = match base {
            "Cargo.toml" => {
                let lock = lock_near(&folder, dir).map(|t| cargo_lock(&t)).unwrap_or_default();
                cargo_deps(&text, &lock).into_iter().map(|(n, v)| ("crates", n, v, None)).collect()
            }
            "package.json" => npm_deps(&text)
                .into_iter()
                .map(|(n, spec)| {
                    let installed: Json = std::fs::read_to_string(folder.join("node_modules").join(&n).join("package.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Json::Null);
                    let ver = installed["version"].as_str().map(str::to_string).or_else(|| clean_version(&spec));
                    ("npm", n, ver, installed["homepage"].as_str().map(str::to_string))
                })
                .collect(),
            "pyproject.toml" => pyproject_deps(&text).into_iter().map(|(n, v)| ("pypi", n, v, None)).collect(),
            "requirements.txt" => requirements_deps(&text).into_iter().map(|(n, v)| ("pypi", n, v, None)).collect(),
            "go.mod" => go_deps(&text).into_iter().map(|(n, v)| ("go", n, v, None)).collect(),
            _ => Vec::new(),
        };
        for (eco, name, version, home) in list {
            if !seen.insert((eco, name.clone())) {
                continue;
            }
            let url = doc_url(eco, &name, version.as_deref(), home.as_deref());
            deps.push(Dep { name, version, ecosystem: eco, manifest: m.clone(), url });
        }
    }
    deps.truncate(MAX_LIBS);
    (found, deps)
}

/// The `Cargo.lock` of a crate's folder or the nearest above it in the checkout.
fn lock_near(folder: &Path, root: &Path) -> Option<String> {
    let mut d = folder.to_path_buf();
    loop {
        if let Ok(t) = std::fs::read_to_string(d.join("Cargo.lock")) {
            return Some(t);
        }
        if d == root || !d.pop() || !d.starts_with(root) {
            return None;
        }
    }
}

// ----------------------------------------------------------------- URLs

/// A URL, as much of one as the crawl needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub path: String,
}

impl Url {
    pub fn parse(s: &str) -> Option<Url> {
        let (scheme, rest) = s.trim().split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return None;
        }
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let host = rest[..end].to_ascii_lowercase();
        if host.is_empty() || host.contains('@') {
            return None;
        }
        let tail = &rest[end..];
        let path = tail.split(['?', '#']).next().unwrap_or("");
        Some(Url { scheme, host, path: normalize(if path.is_empty() { "/" } else { path }) })
    }

    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.host)
    }

    pub fn as_string(&self) -> String {
        format!("{}{}", self.origin(), self.path)
    }

    /// The folder of the path, ending in `/`.
    pub fn folder(&self) -> String {
        match self.path.rfind('/') {
            Some(i) => self.path[..=i].to_string(),
            None => "/".into(),
        }
    }

    /// A link of this page, resolved; `None` for another scheme.
    pub fn join(&self, href: &str) -> Option<Url> {
        let h = href.trim();
        if h.is_empty() || h.starts_with('#') {
            return None;
        }
        if h.contains("://") {
            return Url::parse(h);
        }
        if let Some(r) = h.strip_prefix("//") {
            return Url::parse(&format!("{}://{r}", self.scheme));
        }
        if h.split(['/', '?', '#']).next().is_some_and(|first| first.contains(':')) {
            return None; // mailto:, javascript:, data:
        }
        let path = h.split(['?', '#']).next().unwrap_or("");
        if path.is_empty() {
            return Some(self.clone());
        }
        let full = if path.starts_with('/') { path.to_string() } else { format!("{}{path}", self.folder()) };
        Some(Url { scheme: self.scheme.clone(), host: self.host.clone(), path: normalize(&full) })
    }

    fn is_loopback(&self) -> bool {
        let h = self.host.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map(|(h, _)| h).unwrap_or(&self.host);
        h == "localhost" || h.starts_with("127.") || h == "[::1]"
    }
}

/// `/a/./b/../c` → `/a/c`.
fn normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let trailing = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut p = format!("/{}", out.join("/"));
    if trailing && p != "/" {
        p.push('/');
    }
    p
}

// --------------------------------------------------------------- robots

/// What robots.txt says this crawler may fetch.
#[derive(Default, Debug)]
pub struct Robots {
    rules: Vec<(bool, String)>,
}

impl Robots {
    /// The rules of the group naming this crawler, else of `*`.
    pub fn parse(text: &str) -> Robots {
        let mut groups: Vec<(Vec<String>, Vec<(bool, String)>)> = Vec::new();
        let mut last_was_agent = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once(':') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
            match k.as_str() {
                "user-agent" => {
                    if !last_was_agent || groups.is_empty() {
                        groups.push((Vec::new(), Vec::new()));
                    }
                    groups.last_mut().unwrap().0.push(v.to_ascii_lowercase());
                    last_was_agent = true;
                }
                "allow" | "disallow" => {
                    last_was_agent = false;
                    if let Some(g) = groups.last_mut() {
                        if !v.is_empty() {
                            g.1.push((k == "allow", v));
                        } else if k == "disallow" {
                            // `Disallow:` with nothing allows all.
                        }
                    }
                }
                _ => last_was_agent = false,
            }
        }
        let mine: Vec<(bool, String)> = groups.iter().filter(|g| g.0.iter().any(|a| a != "*" && ROBOT.starts_with(a.as_str()))).flat_map(|g| g.1.clone()).collect();
        let rules = if mine.is_empty() && !groups.iter().any(|g| g.0.iter().any(|a| a != "*" && ROBOT.starts_with(a.as_str()))) {
            groups.iter().filter(|g| g.0.iter().any(|a| a == "*")).flat_map(|g| g.1.clone()).collect()
        } else {
            mine
        };
        Robots { rules }
    }

    /// Whether a path may be fetched: the longest rule that matches wins,
    /// `Allow` on a tie.
    pub fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, pat) in &self.rules {
            if robots_match(pat, path) {
                let len = pat.len();
                if best.is_none_or(|(l, a)| len > l || len == l && *allow && !a) {
                    best = Some((len, *allow));
                }
            }
        }
        best.is_none_or(|(_, a)| a)
    }
}

/// robots.txt's patterns: a prefix, with `*` for any run and `$` for the end.
fn robots_match(pat: &str, path: &str) -> bool {
    let (pat, anchored) = match pat.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pat, false),
    };
    let parts: Vec<&str> = pat.split('*').collect();
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !path.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 && anchored {
            return path.len() >= pos + part.len() && path.ends_with(part);
        } else {
            match path[pos..].find(part) {
                Some(j) => pos += j + part.len(),
                None => return false,
            }
        }
    }
    !anchored || pos == path.len()
}

// ----------------------------------------------------------------- HTML

/// A page's title, text and links.
#[derive(Debug, Default)]
pub struct Page {
    pub title: String,
    pub text: String,
    pub links: Vec<String>,
}

fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let lower = hay.get(from..)?.to_ascii_lowercase();
    lower.find(needle).map(|i| i + from)
}

/// Drops the elements `tag` and what they hold.
fn drop_elements(html: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while let Some(s) = lower[i..].find(&open).map(|s| s + i) {
        let after = lower.as_bytes().get(s + open.len()).copied().unwrap_or(b' ');
        if !(after == b'>' || after.is_ascii_whitespace() || after == b'/') {
            out.push_str(&html[i..s + open.len()]);
            i = s + open.len();
            continue;
        }
        out.push_str(&html[i..s]);
        match lower[s..].find(&close) {
            Some(e) => i = s + e + close.len(),
            None => {
                i = html.len();
                break;
            }
        }
    }
    out.push_str(&html[i.min(html.len())..]);
    out
}

/// Drops `<!-- … -->`.
fn strip_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(s) = rest.find("<!--") {
        out.push_str(&rest[..s]);
        match rest[s + 4..].find("-->") {
            Some(e) => rest = &rest[s + 4 + e + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The inside of the first element `tag`, if there is one.
fn inside(html: &str, tag: &str) -> Option<String> {
    let s = find_ci(html, &format!("<{tag}"), 0)?;
    let after = html.as_bytes().get(s + tag.len() + 1).copied()?;
    if !(after == b'>' || after.is_ascii_whitespace()) {
        return None;
    }
    let open_end = html[s..].find('>')? + s + 1;
    let e = find_ci(html, &format!("</{tag}>"), open_end).unwrap_or(html.len());
    Some(html[open_end..e].to_string())
}

/// `&amp;`, `&#39;`, `&#x27;` and the like.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[1..].find(';').map(|e| e + 1).filter(|e| *e <= 10);
        let decoded = end.and_then(|e| {
            let name = &rest[1..e];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                "ndash" => Some('–'),
                "mdash" => Some('—'),
                "hellip" => Some('…'),
                "rsquo" | "lsquo" => Some('\''),
                "rdquo" | "ldquo" => Some('"'),
                "copy" => Some('©'),
                _ => name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")).and_then(|h| u32::from_str_radix(h, 16).ok()).or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok())).and_then(char::from_u32),
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// An attribute's value in a tag's text: `href="x"`, `href='x'`, `href=x`.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name).map(|i| i + from) {
        let before = lower.as_bytes().get(i.wrapping_sub(1)).copied().unwrap_or(b' ');
        let rest = lower[i + name.len()..].trim_start();
        if before.is_ascii_whitespace() && rest.starts_with('=') {
            let off = tag.len() - rest.len() + 1;
            let v = tag[off..].trim_start();
            let val = match v.chars().next() {
                Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or(""),
                _ => v.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(""),
            };
            return Some(decode_entities(val));
        }
        from = i + name.len();
    }
    None
}

/// A page's title, readable text (its `<main>` or `<article>` when it has
/// one) and the links of all of it.
pub fn html_to_page(html: &str) -> Page {
    let html = strip_comments(html);
    let mut links = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while let Some(s) = lower[i..].find("<a").map(|s| s + i) {
        let Some(e) = lower[s..].find('>').map(|e| e + s) else { break };
        if lower.as_bytes().get(s + 2).is_some_and(|b| b.is_ascii_whitespace()) {
            if let Some(h) = attr(&html[s..e], "href") {
                links.push(h);
            }
        }
        i = e;
    }
    let title = inside(&html, "title").map(|t| collapse(&decode_entities(&t))).unwrap_or_default();
    let mut body = html.clone();
    for tag in ["script", "style", "noscript", "svg", "template", "head"] {
        body = drop_elements(&body, tag);
    }
    let main = inside(&body, "main").or_else(|| inside(&body, "article")).unwrap_or_else(|| inside(&body, "body").unwrap_or(body.clone()));
    let mut main = main;
    for tag in ["nav", "footer"] {
        main = drop_elements(&main, tag);
    }
    Page { title, text: tags_to_text(&main), links }
}

const INLINE: [&str; 16] = ["a", "span", "code", "em", "strong", "b", "i", "u", "wbr", "sup", "sub", "kbd", "var", "abbr", "mark", "small"];

fn tags_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    while let Some(s) = rest.find('<') {
        out.push_str(&rest[..s]);
        let Some(e) = rest[s..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[s + 1..s + e];
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        if !INLINE.contains(&name.as_str()) {
            out.push('\n');
        }
        rest = &rest[s + e + 1..];
    }
    out.push_str(rest);
    collapse_lines(&decode_entities(&out))
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collapse_lines(s: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for l in s.lines() {
        let l = collapse(l);
        if l.is_empty() {
            if out.last().is_some_and(|p| !p.is_empty()) {
                out.push(String::new());
            }
        } else {
            out.push(l);
        }
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

// ---------------------------------------------------------------- fetch

/// The `curl` the crawl runs: `CHLOEDB_WORKBENCH_CURL`, or `curl` on PATH.
fn curl_bin() -> PathBuf {
    std::env::var_os("CHLOEDB_WORKBENCH_CURL").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("curl"))
}

/// Whether `curl` runs here, or why not.
pub fn curl_available() -> Result<String, String> {
    curl_check(&curl_bin())
}

fn curl_check(bin: &Path) -> Result<String, String> {
    match Command::new(bin).arg("--version").output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).split_whitespace().take(2).collect::<Vec<_>>().join(" ")),
        Ok(_) => Err("curl --version failed".into()),
        Err(e) => Err(format!("curl is not on PATH ({e}); install curl to crawl doc sites")),
    }
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// A fetched page.
pub struct Fetched {
    pub status: u16,
    pub url: String,
    pub content_type: String,
    pub body: String,
}

const MARK: &str = "\n@@chloedb-workbench@@ ";

/// Fetches a URL with `curl`, following redirects, at most 3 MB and 20 s.
pub fn fetch(url: &Url) -> Result<Fetched, String> {
    let mut cmd = Command::new(curl_bin());
    cmd.args(["-sS", "-L", "--compressed", "--max-redirs", "5", "--max-time", "20", "--max-filesize", "3000000", "-A", USER_AGENT, "-w"])
        .arg(format!("{MARK}%{{http_code}} %{{url_effective}} %{{content_type}}"))
        .arg(url.as_string());
    if url.is_loopback() {
        cmd.args(["--noproxy", "*"]);
    }
    let out = cmd.output().map_err(|e| format!("cannot run curl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let Some(i) = text.rfind(MARK) else {
        return Err(format!("curl failed on {}: {}", url.as_string(), String::from_utf8_lossy(&out.stderr).trim()));
    };
    let meta = &text[i + MARK.len()..];
    let mut it = meta.splitn(3, ' ');
    let status: u16 = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let eff = it.next().unwrap_or("").to_string();
    let ct = it.next().unwrap_or("").trim().to_ascii_lowercase();
    if status == 0 {
        return Err(format!("cannot fetch {}: {}", url.as_string(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(Fetched { status, url: eff, content_type: ct, body: text[..i].to_string() })
}

// ---------------------------------------------------------------- crawl

/// The crawls this server runs: a library's row id → (done, total).
fn progress() -> &'static Mutex<HashMap<String, (usize, usize)>> {
    static P: OnceLock<Mutex<HashMap<String, (usize, usize)>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One crawl at a time.
fn crawl_lock() -> &'static Mutex<()> {
    static C: OnceLock<Mutex<()>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(()))
}

fn row_id(repo: &str, lib: &str) -> String {
    format!("{}:{lib}", git::hash8(Path::new(repo)))
}

fn page_id(row: &str, url: &str) -> String {
    // FNV-1a: short and stable.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in url.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{row}#{h:016x}")
}

fn set_state(sup: &Supervisor, row: &str, state: &str, pages: Option<usize>, error: Option<&str>) {
    let mut sets = vec!["state = :state", "updated = :now"];
    let mut p = Params::new().set("id", sv(row)).set("state", sv(state)).set("now", ts(now_ms()));
    if let Some(n) = pages {
        sets.push("pages = :pages");
        p = p.set("pages", Value::Int(n as i64));
    }
    sets.push("error = :error");
    p = p.set("error", error.map_or(Value::Null, sv));
    if state == "done" {
        sets.push("crawled = :now");
    }
    let _ = q(sup, &format!("UPDATE doc_libs SET {} WHERE id = :id", sets.join(", ")), &p);
}

fn store_page(sup: &Supervisor, repo: &str, row: &str, url: &str, page: &Page, depth: usize) -> Result<(), String> {
    let text: String = page.text.chars().take(MAX_TEXT).collect();
    let words = text.split_whitespace().count();
    let title = if page.title.is_empty() { url.to_string() } else { page.title.clone() };
    let p = Params::new()
        .set("id", Value::Str(page_id(row, url)))
        .set("lib", sv(row))
        .set("repo", sv(repo))
        .set("url", sv(url))
        .set("title", Value::Str(title))
        .set("text", Value::Str(text))
        .set("words", Value::Int(words as i64))
        .set("depth", Value::Int(depth as i64))
        .set("ts", ts(now_ms()));
    q(sup, "INSERT OR REPLACE INTO doc_pages (id, lib, repo, url, title, text, words, depth, fetched) VALUES (:id, :lib, :repo, :url, :title, :text, :words, :depth, :ts)", &p).map(|_| ())
}

fn readable(f: &Fetched) -> Option<Page> {
    if !(200..300).contains(&f.status) {
        return None;
    }
    let ct = &f.content_type;
    if ct.contains("html") || ct.is_empty() && f.body.trim_start().starts_with('<') {
        Some(html_to_page(&f.body))
    } else if ct.starts_with("text/") {
        Some(Page { title: String::new(), text: collapse_lines(&f.body), links: Vec::new() })
    } else {
        None
    }
}

/// Crawls a library's site into its rows: the first page and the pages it
/// links to under its folder, as robots.txt allows. Answers the pages kept.
pub fn crawl(sup: &Supervisor, repo: &str, row: &str, start: &str) -> Result<usize, String> {
    let start = Url::parse(start).ok_or_else(|| format!("not an http(s) URL: {start}"))?;
    progress().lock().unwrap().insert(row.to_string(), (0, 1));
    let robots_for = |u: &Url| -> Robots {
        match fetch(&Url { scheme: u.scheme.clone(), host: u.host.clone(), path: "/robots.txt".into() }) {
            Ok(f) if (200..300).contains(&f.status) => Robots::parse(&f.body),
            _ => Robots::default(),
        }
    };
    let mut robots = robots_for(&start);
    if !robots.allows(&start.path) {
        return Err(format!("robots.txt disallows {} for this crawler", start.path));
    }
    let first = fetch(&start)?;
    if !(200..300).contains(&first.status) {
        return Err(format!("{} answered HTTP {}", start.as_string(), first.status));
    }
    // Where redirects led: its host and folder bound the crawl.
    let base = Url::parse(&first.url).unwrap_or_else(|| start.clone());
    if base.host != start.host {
        robots = robots_for(&base);
        if !robots.allows(&base.path) {
            return Err(format!("robots.txt of {} disallows {}", base.host, base.path));
        }
    }
    let Some(page) = readable(&first) else {
        return Err(format!("{} is not a page of text ({})", base.as_string(), first.content_type));
    };
    let _ = q(sup, "DELETE FROM doc_pages WHERE lib = :lib", &Params::new().set("lib", sv(row)));
    store_page(sup, repo, row, &base.as_string(), &page, 0)?;
    let folder = base.folder();
    // `dir/` and `dir/index.html` are one page.
    let canon = |u: &Url| u.as_string().trim_end_matches("index.html").to_string();
    let mut seen: HashSet<String> = HashSet::from([canon(&base), canon(&start)]);
    let mut todo: Vec<Url> = Vec::new();
    for href in &page.links {
        let Some(u) = base.join(href) else { continue };
        let ext = u.path.rsplit('/').next().and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        if u.host != base.host || u.scheme != base.scheme || !u.path.starts_with(&folder) || !robots.allows(&u.path) {
            continue;
        }
        if !ext.is_empty() && !["html", "htm", "md", "txt", "php", "aspx"].contains(&ext.as_str()) {
            continue;
        }
        if seen.insert(canon(&u)) {
            todo.push(u);
        }
        if todo.len() + 1 >= MAX_PAGES {
            break;
        }
    }
    let total = todo.len() + 1;
    let mut kept = 1;
    progress().lock().unwrap().insert(row.to_string(), (1, total));
    let polite = !base.is_loopback();
    for (i, u) in todo.iter().enumerate() {
        if polite {
            std::thread::sleep(Duration::from_millis(150));
        }
        if let Ok(f) = fetch(u) {
            if let Some(p) = readable(&f) {
                let at = Url::parse(&f.url).filter(|e| e.host == base.host).unwrap_or_else(|| u.clone());
                if store_page(sup, repo, row, &at.as_string(), &p, 1).is_ok() {
                    kept += 1;
                }
            }
        }
        progress().lock().unwrap().insert(row.to_string(), (i + 2, total));
        if i % 10 == 0 {
            let _ = q(sup, "UPDATE doc_libs SET pages = :n WHERE id = :id", &Params::new().set("n", Value::Int(kept as i64)).set("id", sv(row)));
        }
    }
    Ok(kept)
}

/// Queues a crawl on a thread of its own; crawls run one at a time.
fn start_crawl(sup: &Arc<Supervisor>, repo: &str, row: &str, url: &str) {
    set_state(sup, row, "queued", None, None);
    progress().lock().unwrap().insert(row.to_string(), (0, 0));
    let (sup, repo, row, url) = (sup.clone(), repo.to_string(), row.to_string(), url.to_string());
    let spawned = std::thread::Builder::new().name("docs-crawl".into()).spawn({
        let row = row.clone();
        let sup = sup.clone();
        move || {
            let _one = crawl_lock().lock().unwrap_or_else(|e| e.into_inner());
            set_state(&sup, &row, "crawling", None, None);
            match crawl(&sup, &repo, &row, &url) {
                Ok(n) => set_state(&sup, &row, "done", Some(n), None),
                Err(e) => set_state(&sup, &row, "failed", None, Some(&e)),
            }
            progress().lock().unwrap().remove(&row);
        }
    });
    if spawned.is_err() {
        set_state(&sup, &row, "failed", None, Some("cannot start a thread"));
        progress().lock().unwrap().remove(&row);
    }
}

// ------------------------------------------------------------- the rows

/// What was found in a repository's manifests, cached for a few seconds
/// (the Docs tab polls while a crawl runs).
fn discovered(repo: &str) -> Result<(Vec<String>, Vec<Dep>), String> {
    type Cache = Mutex<HashMap<String, (Instant, Vec<String>, Vec<Dep>)>>;
    static C: OnceLock<Cache> = OnceLock::new();
    let cache = C.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((t, m, d)) = cache.lock().unwrap().get(repo) {
        if t.elapsed() < Duration::from_secs(10) {
            return Ok((m.clone(), d.clone()));
        }
    }
    let dir = PathBuf::from(repo);
    let files = search::list_files(&dir)?;
    let (m, d) = discover(&dir, &files);
    cache.lock().unwrap().insert(repo.to_string(), (Instant::now(), m.clone(), d.clone()));
    Ok((m, d))
}

/// Writes the rows of the dependencies found that have none, and updates
/// the version and URL of those whose changed; drops the rows of
/// dependencies gone that were never crawled.
fn sync_libs(sup: &Supervisor, repo: &str, deps: &[Dep]) -> Result<(), String> {
    let rows = q(sup, "SELECT id, version, url, state, ecosystem FROM doc_libs WHERE repo = :repo LIMIT 10000", &Params::new().set("repo", sv(repo)))?;
    let have: HashMap<String, (Option<String>, Option<String>, String, String)> = rows.iter().filter_map(|r| Some((get_str(&r[0])?, (get_str(&r[1]), get_str(&r[2]), get_str(&r[3]).unwrap_or_default(), get_str(&r[4]).unwrap_or_default())))).collect();
    let mut keep = HashSet::new();
    for d in deps {
        let lib = lib_id(d.ecosystem, &d.name);
        let row = row_id(repo, &lib);
        keep.insert(row.clone());
        let p = Params::new()
            .set("id", sv(&row))
            .set("repo", sv(repo))
            .set("lib", sv(&lib))
            .set("name", sv(&d.name))
            .set("version", d.version.as_deref().map_or(Value::Null, sv))
            .set("eco", sv(d.ecosystem))
            .set("manifest", sv(&d.manifest))
            .set("url", sv(&d.url))
            .set("now", ts(now_ms()));
        match have.get(&row) {
            None => {
                q(sup, "INSERT OR REPLACE INTO doc_libs (id, repo, lib, name, version, ecosystem, manifest, url, state, pages, error, crawled, updated) VALUES (:id, :repo, :lib, :name, :version, :eco, :manifest, :url, 'none', 0, NULL, NULL, :now)", &p)?;
            }
            Some((v, u, _, _)) if v.as_deref() != d.version.as_deref() || u.as_deref() != Some(d.url.as_str()) => {
                q(sup, "UPDATE doc_libs SET version = :version, url = :url, manifest = :manifest, updated = :now WHERE id = :id", &p)?;
            }
            Some(_) => {}
        }
    }
    for (row, (_, _, state, eco)) in &have {
        if !keep.contains(row) && eco != "site" && state == "none" {
            q(sup, "DELETE FROM doc_libs WHERE id = :id", &Params::new().set("id", sv(row)))?;
        }
    }
    Ok(())
}

fn agent_ref(sup: &Supervisor, id: &str) -> Json {
    sup.agent_json_of(id).unwrap_or_else(|| json!({ "id": id, "name": id }))
}

/// Citations of a repository's pages, newest first.
fn cites(sup: &Supervisor, repo: &str, lib: Option<&str>, limit: usize) -> Vec<(i64, String, String, String, String, String)> {
    let (w, mut p) = match lib {
        Some(l) => ("repo = :repo AND lib = :lib", Params::new().set("lib", sv(l))),
        None => ("repo = :repo", Params::new()),
    };
    p = p.set("repo", sv(repo));
    q(sup, &format!("SELECT ts, agent, lib, url, title, q FROM doc_cites WHERE {w} ORDER BY ts DESC LIMIT {limit}"), &p)
        .unwrap_or_default()
        .iter()
        .map(|r| (get_ms(&r[0]).unwrap_or(0), get_str(&r[1]).unwrap_or_default(), get_str(&r[2]).unwrap_or_default(), get_str(&r[3]).unwrap_or_default(), get_str(&r[4]).unwrap_or_default(), get_str(&r[5]).unwrap_or_default()))
        .collect()
}

/// A library's row as the API answers it.
fn lib_json(sup: &Supervisor, r: &[Value], cited: &HashMap<String, (usize, Vec<String>)>) -> Json {
    let row = get_str(&r[0]).unwrap_or_default();
    let mut state = get_str(&r[7]).unwrap_or_else(|| "none".into());
    let mut error = get_str(&r[9]);
    let prog = progress().lock().unwrap().get(&row).copied();
    if (state == "crawling" || state == "queued") && prog.is_none() {
        state = "failed".into();
        error = Some("the crawl was cut short (the server stopped); crawl it again".into());
    }
    let (n, by) = cited.get(&row).cloned().unwrap_or_default();
    json!({
        "id": get_str(&r[1]),
        "name": get_str(&r[2]),
        "version": get_str(&r[3]),
        "ecosystem": get_str(&r[4]),
        "manifest": get_str(&r[5]),
        "url": get_str(&r[6]),
        "state": state,
        "pages": get_int(&r[8]).unwrap_or(0),
        "error": error,
        "crawled_ms": get_ms(&r[10]),
        "cited": n,
        "cited_by": by.iter().map(|a| agent_ref(sup, a)).collect::<Vec<_>>(),
        "progress": prog.filter(|p| p.1 > 0).map(|(done, total)| json!({ "done": done, "total": total })),
    })
}

const LIB_COLS: &str = "id, lib, name, version, ecosystem, manifest, url, state, pages, error, crawled";

fn cited_by_lib(sup: &Supervisor, repo: &str) -> HashMap<String, (usize, Vec<String>)> {
    let mut m: HashMap<String, (usize, Vec<String>)> = HashMap::new();
    for (_, agent, lib, ..) in cites(sup, repo, None, 5000) {
        let e = m.entry(lib).or_default();
        e.0 += 1;
        if !e.1.contains(&agent) {
            e.1.push(agent);
        }
    }
    m
}

fn crawler_json() -> Json {
    let crawl_bin = on_path("chloedb-crawl");
    let note = format!(
        "same host and folder, depth 1, at most {MAX_PAGES} pages, robots.txt respected{}",
        if crawl_bin { "; chloedb-crawl is on PATH but crawls into a cluster of its own, so the built-in fetcher keeps the pages here" } else { "" }
    );
    match curl_available() {
        Ok(v) => json!({ "kind": "built-in", "available": true, "via": v, "note": note, "chloedb_crawl": crawl_bin }),
        Err(why) => json!({ "kind": "built-in", "available": false, "why": why, "via": null, "note": note, "chloedb_crawl": crawl_bin }),
    }
}

/// `GET /api/library`.
pub fn list(sup: &Supervisor, repo: Option<&str>) -> Result<Json, ApiError> {
    let repo = trail::repo_dir(repo)?.to_string_lossy().to_string();
    let mut missing = Vec::new();
    let manifests = match discovered(&repo) {
        Ok((m, deps)) => {
            if let Err(e) = sync_libs(sup, &repo, &deps) {
                missing.push(format!("cannot write the libraries: {e}"));
            }
            if m.is_empty() {
                missing.push("No manifest found (Cargo.toml, package.json, pyproject.toml, requirements.txt, go.mod): add a doc site by its URL.".to_string());
            }
            m
        }
        Err(e) => {
            missing.push(e);
            Vec::new()
        }
    };
    let rows = q(sup, &format!("SELECT {LIB_COLS} FROM doc_libs WHERE repo = :repo LIMIT 1000"), &Params::new().set("repo", sv(&repo))).map_err(ApiError::internal)?;
    let cited = cited_by_lib(sup, &repo);
    let mut libs: Vec<Json> = rows.iter().map(|r| lib_json(sup, r, &cited)).collect();
    let rank = |l: &Json| match l["state"].as_str() {
        Some("crawling") | Some("queued") | Some("done") => 0,
        Some("failed") => 1,
        _ => 2,
    };
    libs.sort_by(|a, b| rank(a).cmp(&rank(b)).then(b["cited"].as_u64().cmp(&a["cited"].as_u64())).then(b["pages"].as_i64().cmp(&a["pages"].as_i64())).then(a["name"].as_str().cmp(&b["name"].as_str())));
    let names: HashMap<String, String> = rows.iter().filter_map(|r| Some((get_str(&r[0])?, get_str(&r[2])?))).collect();
    let citations: Vec<Json> = cites(sup, &repo, None, 40)
        .into_iter()
        .map(|(t, agent, lib, url, title, qq)| json!({ "ts_ms": t, "agent": agent_ref(sup, &agent), "q": qq, "url": url, "title": title, "library": names.get(&lib).cloned().unwrap_or(lib) }))
        .collect();
    let crawler = crawler_json();
    if crawler["available"] == false {
        missing.push(crawler["why"].as_str().unwrap_or("").to_string());
    }
    Ok(json!({ "repo": repo, "libraries": libs, "crawler": crawler, "citations": citations, "manifests": manifests, "missing": missing }))
}

/// `GET /api/library/:id/pages`.
pub fn pages(sup: &Supervisor, repo: Option<&str>, lib: &str) -> Result<Json, ApiError> {
    let repo = trail::repo_dir(repo)?.to_string_lossy().to_string();
    let row = row_id(&repo, lib);
    let rows = q(sup, &format!("SELECT {LIB_COLS} FROM doc_libs WHERE id = :id"), &Params::new().set("id", sv(&row))).map_err(ApiError::internal)?;
    let Some(r) = rows.first() else { return Err(ApiError::not_found(format!("no library {lib} in {repo}"))) };
    let cited = cited_by_lib(sup, &repo);
    let library = lib_json(sup, r, &cited);
    let mut by_url: HashMap<String, Vec<Json>> = HashMap::new();
    for (t, agent, _, url, _, qq) in cites(sup, &repo, Some(&row), 2000) {
        by_url.entry(url).or_default().push(json!({ "agent": agent_ref(sup, &agent), "ts_ms": t, "q": qq }));
    }
    let rows = q(sup, "SELECT url, title, words, depth, fetched FROM doc_pages WHERE lib = :lib LIMIT 1000", &Params::new().set("lib", sv(&row))).map_err(ApiError::internal)?;
    let mut list: Vec<Json> = rows
        .iter()
        .map(|r| {
            let url = get_str(&r[0]).unwrap_or_default();
            let by = by_url.get(&url).cloned().unwrap_or_default();
            json!({ "url": url, "title": get_str(&r[1]), "words": get_int(&r[2]).unwrap_or(0), "depth": get_int(&r[3]).unwrap_or(0), "fetched_ms": get_ms(&r[4]), "cited": by.len(), "cited_by": by })
        })
        .collect();
    list.sort_by(|a, b| b["cited"].as_u64().cmp(&a["cited"].as_u64()).then(a["depth"].as_i64().cmp(&b["depth"].as_i64())).then(a["title"].as_str().cmp(&b["title"].as_str())));
    Ok(json!({ "library": library, "pages": list }))
}

#[derive(Deserialize)]
struct CrawlB {
    repo: Option<String>,
    library: Option<String>,
    libraries: Option<Vec<String>>,
    url: Option<String>,
}

/// `POST /api/library/crawl`: a library of the list, several, or a doc site
/// by its URL (a library of its own, `site-<host>`).
fn crawl_req(sup: &Arc<Supervisor>, b: CrawlB) -> Result<(StatusCode, Json), ApiError> {
    let repo = trail::repo_dir(b.repo.as_deref())?.to_string_lossy().to_string();
    if let Err(why) = curl_available() {
        return Ok((StatusCode::OK, json!({ "available": false, "why": why })));
    }
    let mut ids: Vec<String> = b.libraries.unwrap_or_default();
    ids.extend(b.library);
    if let Some(u) = b.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        let url = Url::parse(u).ok_or_else(|| ApiError::bad(format!("not an http(s) URL: {u}")))?;
        let lib = lib_id("site", &format!("{}{}", url.host, url.path.trim_end_matches('/')));
        let row = row_id(&repo, &lib);
        let p = Params::new()
            .set("id", sv(&row))
            .set("repo", sv(&repo))
            .set("lib", sv(&lib))
            .set("name", sv(&url.host))
            .set("url", Value::Str(url.as_string()))
            .set("now", ts(now_ms()));
        q(sup, "INSERT OR REPLACE INTO doc_libs (id, repo, lib, name, version, ecosystem, manifest, url, state, pages, error, crawled, updated) VALUES (:id, :repo, :lib, :name, NULL, 'site', NULL, :url, 'none', 0, NULL, NULL, :now)", &p).map_err(ApiError::internal)?;
        ids.push(lib);
    }
    if ids.is_empty() {
        return Err(ApiError::bad("library, libraries or url is required"));
    }
    let mut started = Vec::new();
    for lib in &ids {
        let row = row_id(&repo, lib);
        let rows = q(sup, &format!("SELECT {LIB_COLS} FROM doc_libs WHERE id = :id"), &Params::new().set("id", sv(&row))).map_err(ApiError::internal)?;
        let Some(r) = rows.first() else { return Err(ApiError::not_found(format!("no library {lib} in {repo}"))) };
        if progress().lock().unwrap().contains_key(&row) {
            continue; // already queued or crawling
        }
        let url = get_str(&r[6]).unwrap_or_default();
        start_crawl(sup, &repo, &row, &url);
        started.push(row);
    }
    let cited = cited_by_lib(sup, &repo);
    let mut out = Vec::new();
    for lib in &ids {
        if let Some(r) = q(sup, &format!("SELECT {LIB_COLS} FROM doc_libs WHERE id = :id"), &Params::new().set("id", sv(&row_id(&repo, lib)))).ok().and_then(|r| r.into_iter().next()) {
            out.push(lib_json(sup, &r, &cited));
        }
    }
    let v = if out.len() == 1 { json!({ "library": out[0], "started": started.len() }) } else { json!({ "libraries": out, "started": started.len() }) };
    Ok((StatusCode::ACCEPTED, v))
}

// --------------------------------------------------------------- search

const DOC_RANK: &str = "bm25(text, :q, syntax => 'plain') + 2 * bm25(title, :q, syntax => 'plain')";

/// `i`, or the char boundary before it, within `s`.
fn floor_char(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The stretch of a page's text with the most of the query's words.
pub fn excerpt(text: &str, words: &[String], max: usize) -> String {
    let lower = text.to_lowercase();
    let mut best = (0usize, 0usize);
    if lower.len() == text.len() {
        for w in words.iter().filter(|w| w.len() > 2) {
            let mut from = 0;
            while let Some(i) = lower[from..].find(w.as_str()).map(|i| i + from) {
                let window = &lower[floor_char(&lower, i.saturating_sub(max / 4))..floor_char(&lower, i + max)];
                let n = words.iter().filter(|w| window.contains(w.as_str())).count();
                if n > best.1 {
                    best = (i, n);
                }
                from = i + w.len();
                if from >= lower.len() {
                    break;
                }
            }
        }
    }
    let mut s = floor_char(text, best.0.saturating_sub(max / 4));
    // Start at a word.
    if s > 0 {
        if let Some((sp, c)) = text[s..].char_indices().find(|(_, c)| c.is_whitespace()) {
            if sp < 30 {
                s += sp + c.len_utf8();
            }
        }
    }
    let e = floor_char(text, s + max);
    let mut out = text[s..e].replace('\n', " ");
    if s > 0 {
        out = format!("… {out}");
    }
    if e < text.len() {
        out.push_str(" …");
    }
    out
}

/// Doc pages of a repository's libraries for a query, best first:
/// `{url, title, library: {id, name}, text, score}`.
pub fn search_pages(sup: &Supervisor, repo: &str, text: &str, lib: Option<&str>, limit: usize) -> Result<Vec<Json>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut p = Params::new().set("repo", sv(repo)).set("q", sv(text));
    let mut w = String::from("repo = :repo");
    if let Some(l) = lib {
        w.push_str(" AND lib = :lib");
        p = p.set("lib", Value::Str(row_id(repo, l)));
    }
    let rows = q(sup, &format!("SELECT lib, url, title, text, score() FROM doc_pages WHERE {w} ORDER BY {DOC_RANK} DESC LIMIT {}", limit.clamp(1, 100)), &p)?;
    let names = lib_names(sup, repo);
    let words: Vec<String> = text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1).map(str::to_string).collect();
    Ok(rows
        .iter()
        .filter_map(|r| {
            let score = match &r[4] {
                Value::Float(f) => *f,
                v => get_int(v).unwrap_or(0) as f64,
            };
            (score > 0.0).then_some(())?;
            let row = get_str(&r[0]).unwrap_or_default();
            let (id, name) = names.get(&row).cloned().unwrap_or((row.clone(), row.clone()));
            Some(json!({
                "url": get_str(&r[1]),
                "title": get_str(&r[2]),
                "library": { "id": id, "name": name },
                "text": excerpt(&get_str(&r[3]).unwrap_or_default(), &words, 280),
                "score": (score * 100.0).round() / 100.0,
            }))
        })
        .collect())
}

/// Row id → (library id, name).
fn lib_names(sup: &Supervisor, repo: &str) -> HashMap<String, (String, String)> {
    q(sup, "SELECT id, lib, name FROM doc_libs WHERE repo = :repo LIMIT 10000", &Params::new().set("repo", sv(repo)))
        .unwrap_or_default()
        .iter()
        .filter_map(|r| Some((get_str(&r[0])?, (get_str(&r[1])?, get_str(&r[2])?))))
        .collect()
}

/// `PACK n TOKENS` of the pages: the text an agent gets, each page under
/// its URL.
fn pack_pages(sup: &Supervisor, repo: &str, text: &str, lib: Option<&str>, budget: i64) -> Result<Json, String> {
    let mut p = Params::new().set("repo", sv(repo)).set("q", sv(text));
    let mut w = String::from("repo = :repo");
    if let Some(l) = lib {
        w.push_str(" AND lib = :lib");
        p = p.set("lib", Value::Str(row_id(repo, l)));
    }
    let excerpt_n = (budget / 3).clamp(64, 2000);
    let rows = q(sup, &format!("SELECT id, url, title, text FROM doc_pages WHERE {w} ORDER BY {DOC_RANK} DESC PACK {budget} TOKENS OF text EXCERPT {excerpt_n} WITH RECEIPT LIMIT 100"), &p)?;
    let (mut used, mut parts, mut list) = (0i64, Vec::new(), Vec::new());
    for r in &rows {
        let url = get_str(&r[1]).unwrap_or_default();
        let title = get_str(&r[2]);
        let tokens = r.get(4).and_then(get_int).unwrap_or(0);
        let decision = r.get(5).and_then(get_str).unwrap_or_else(|| "take".into());
        let score = r.get(6).map(|v| matches!(v, Value::Float(f) if *f > 0.0)).unwrap_or(true);
        if !score {
            continue;
        }
        if !decision.starts_with("skip") {
            used += tokens;
            parts.push(format!("<{url}> {}\n{}", title.clone().unwrap_or_default(), get_str(&r[3]).unwrap_or_default()));
        }
        list.push(json!({ "url": url, "title": title, "decision": decision, "tokens": tokens }));
    }
    Ok(json!({ "budget": budget, "used": used, "text": parts.join("\n\n"), "rows": list }))
}

/// Records that an agent's search was answered with these pages.
fn record_cites(sup: &Supervisor, repo: &str, by: &str, qq: &str, hits: &[Json]) {
    let rows: HashMap<String, String> = lib_names(sup, repo).into_iter().map(|(row, (id, _))| (id, row)).collect();
    for h in hits {
        let url = h["url"].as_str().unwrap_or("");
        let lib = h["library"]["id"].as_str().and_then(|id| rows.get(id)).cloned().unwrap_or_default();
        let p = Params::new()
            .set("id", Value::Str(format!("c{}", crate::supervisor::hex(10))))
            .set("ts", ts(now_ms()))
            .set("repo", sv(repo))
            .set("agent", sv(by))
            .set("lib", Value::Str(lib))
            .set("url", sv(url))
            .set("title", sv(h["title"].as_str().unwrap_or("")))
            .set("q", sv(qq));
        let _ = q(sup, "INSERT INTO doc_cites (id, ts, repo, agent, lib, url, title, q) VALUES (:id, :ts, :repo, :agent, :lib, :url, :title, :q)", &p);
    }
}

#[derive(Deserialize)]
pub struct DocsQ {
    pub repo: Option<String>,
    pub q: Option<String>,
    pub library: Option<String>,
    pub tokens: Option<i64>,
    pub limit: Option<usize>,
    pub by: Option<String>,
}

/// The pages an answer cites: its first three.
const CITED: usize = 3;

/// `GET /api/search/docs`.
pub fn docs(sup: &Supervisor, d: DocsQ) -> Result<Json, ApiError> {
    // Without repo, the repository of the agent searching.
    let hint = d.repo.clone().filter(|r| !r.is_empty()).or_else(|| d.by.as_deref().and_then(|id| sup.agent_row(id)).map(|a| a.repo));
    let repo = trail::repo_dir(hint.as_deref())?.to_string_lossy().to_string();
    let text = d.q.unwrap_or_default();
    let lib = d.library.as_deref().filter(|l| !l.is_empty());
    let mut missing = Vec::new();
    let hits = search_pages(sup, &repo, &text, lib, d.limit.unwrap_or(8)).unwrap_or_else(|e| {
        missing.push(format!("the docs search failed: {e}"));
        Vec::new()
    });
    let pack = match d.tokens {
        Some(n) if n > 0 && !text.trim().is_empty() => match pack_pages(sup, &repo, &text, lib, n.clamp(100, 50_000)) {
            Ok(p) => Some(p),
            Err(e) => {
                missing.push(format!("PACK failed: {e}"));
                None
            }
        },
        _ => None,
    };
    let crawled = q(sup, "SELECT id FROM doc_pages WHERE repo = :repo LIMIT 1", &Params::new().set("repo", sv(&repo))).map(|r| !r.is_empty()).unwrap_or(false);
    if !crawled {
        missing.push("No docs are crawled for this repository yet: crawl a library on the Library's Docs tab (POST /api/library/crawl).".to_string());
    }
    if let Some(by) = d.by.as_deref().filter(|b| !b.is_empty()) {
        let cited: Vec<Json> = hits.iter().take(CITED).cloned().collect();
        record_cites(sup, &repo, by, &text, &cited);
        let urls: Vec<String> = cited.iter().filter_map(|h| h["url"].as_str().map(str::to_string)).collect();
        search::log(sup, &repo, by, "docs", &text, hits.len(), pack.as_ref().and_then(|p| p["used"].as_i64()).unwrap_or(0), &urls);
    }
    Ok(json!({ "repo": repo, "q": text, "hits": hits, "pack": pack, "missing": missing }))
}

// --------------------------------------------------------------- routes

#[derive(Deserialize)]
struct RepoQ {
    repo: Option<String>,
}

async fn list_route(State(app): State<App>, Query(rq): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || list(&sup, rq.repo.as_deref())).await?).into_response())
}

async fn pages_route(State(app): State<App>, AxPath(id): AxPath<String>, Query(rq): Query<RepoQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || pages(&sup, rq.repo.as_deref(), &id)).await?).into_response())
}

async fn crawl_route(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: CrawlB = parse_body(&body)?;
    let sup = app.sup.clone();
    let (status, v) = blocking(move || crawl_req(&sup, b)).await?;
    Ok((status, AxJson(v)).into_response())
}

async fn docs_route(State(app): State<App>, Query(d): Query<DocsQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(AxJson(blocking(move || docs(&sup, d)).await?).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_name_their_dependencies() {
        let cargo = "[package]\nname = \"api\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\naxum = \"0.7\"\nlocal = { path = \"../local\" }\nstripe = { package = \"async-stripe\", version = \"0.39\" }\n\n[dependencies.tokio]\nversion = \"1.41\"\nfeatures = [\"full\"]\n\n[dev-dependencies]\ntempfile = \"3\"\n";
        let lock = cargo_lock("[[package]]\nname = \"serde\"\nversion = \"1.0.214\"\n\n[[package]]\nname = \"axum\"\nversion = \"0.7.9\"\n");
        let d = cargo_deps(cargo, &lock);
        assert_eq!(d, [("serde".into(), Some("1.0.214".into())), ("axum".into(), Some("0.7.9".into())), ("async-stripe".into(), Some("0.39".into())), ("tokio".into(), Some("1.41".into()))]);
        let npm = npm_deps(r#"{"dependencies": {"react": "^19.0.0", "ui": "workspace:*"}, "devDependencies": {"vitest": "2"}}"#);
        assert_eq!(npm, [("react".to_string(), "^19.0.0".to_string())]);
        let py = pyproject_deps("[project]\nname = \"recs\"\ndependencies = [\n  \"fastapi>=0.115\",\n  \"numpy==2.1.3\",\n  \"Pydantic_Core\",\n]\n");
        assert_eq!(py, [("fastapi".into(), Some("0.115".into())), ("numpy".into(), Some("2.1.3".into())), ("pydantic-core".into(), None)]);
        assert_eq!(requirements_deps("# x\nrequests==2.32.3\n-e .\nflask\n"), [("requests".into(), Some("2.32.3".into())), ("flask".into(), None)]);
        assert_eq!(go_deps("module x\n\nrequire (\n\tgithub.com/go-chi/chi/v5 v5.1.0\n\tgolang.org/x/text v0.3.0 // indirect\n)\nrequire github.com/a/b v1.0.0\n"), [("github.com/go-chi/chi/v5".into(), Some("v5.1.0".into())), ("github.com/a/b".into(), Some("v1.0.0".into()))]);
        assert_eq!(doc_url("crates", "serde", Some("1.0.214"), None), "https://docs.rs/serde/1.0.214/");
        assert_eq!(doc_url("npm", "react", None, None), "https://react.dev/reference/react");
        assert_eq!(doc_url("npm", "left-pad", None, Some("https://github.com/x/left-pad#readme")), "https://www.npmjs.com/package/left-pad");
        assert_eq!(lib_id("npm", "@tanstack/react-query"), "npm-tanstack-react-query");
        assert_eq!(manifests(&["web/package.json".into(), "Cargo.toml".into(), "examples/x/Cargo.toml".into(), "src/main.rs".into()]), ["Cargo.toml", "web/package.json"]);
    }

    #[test]
    fn urls_resolve_inside_their_site() {
        let u = Url::parse("https://docs.rs/serde/1.0.214/serde/index.html?x=1#top").unwrap();
        assert_eq!(u.path, "/serde/1.0.214/serde/index.html");
        assert_eq!(u.folder(), "/serde/1.0.214/serde/");
        assert_eq!(u.join("trait.Serialize.html").unwrap().as_string(), "https://docs.rs/serde/1.0.214/serde/trait.Serialize.html");
        assert_eq!(u.join("../../serde_json/").unwrap().path, "/serde/serde_json/");
        assert_eq!(u.join("/about").unwrap().path, "/about");
        assert!(u.join("mailto:a@b.c").is_none());
        assert!(u.join("#x").is_none());
        assert_eq!(u.join("//example.com/a").unwrap().host, "example.com");
        assert!(Url::parse("http://127.0.0.1:8080/").unwrap().is_loopback());
    }

    #[test]
    fn robots_txt_is_respected() {
        let r = Robots::parse("User-agent: *\nDisallow: /private/\nAllow: /private/ok\n\nUser-agent: other\nDisallow: /\n");
        assert!(r.allows("/docs/"));
        assert!(!r.allows("/private/x"));
        assert!(r.allows("/private/ok.html"));
        let mine = Robots::parse("User-agent: *\nDisallow: /\n\nUser-agent: chloedb-workbench\nDisallow: /secret\n");
        assert!(mine.allows("/docs"), "the group naming this crawler wins over *");
        assert!(!mine.allows("/secret/a"));
        let glob = Robots::parse("User-agent: *\nDisallow: /*.pdf$\n");
        assert!(!glob.allows("/a/b.pdf"));
        assert!(glob.allows("/a/b.pdf.html"));
        assert!(Robots::parse("User-agent: *\nDisallow:\n").allows("/x"));
    }

    #[test]
    fn without_curl_the_crawler_says_why() {
        let e = curl_check(Path::new("/nonexistent/curl")).unwrap_err();
        assert!(e.contains("curl is not on PATH"), "{e}");
    }

    #[test]
    fn pages_become_text_and_links() {
        let html = "<html><head><title>PaymentIntent &amp; you</title><style>x{}</style></head><body><nav><a href=\"a.html\">A</a></nav><main><h1>PaymentIntent</h1><p>Updates the <code>amount</code>&nbsp;of a<br>payment.</p><script>var x=1</script><a href='b.html'>B</a></main><!-- <a href=\"c.html\"> --></body></html>";
        let p = html_to_page(html);
        assert_eq!(p.title, "PaymentIntent & you");
        assert_eq!(p.text, "PaymentIntent\n\nUpdates the amount of a\npayment.\nB");
        assert_eq!(p.links, ["a.html", "b.html"]);
        let e = excerpt("one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen", &["fifteen".into()], 40);
        assert!(e.contains("fifteen") && e.starts_with("… "), "{e}");
        // Cut between characters, never inside one.
        let wide = format!("{}needle{}", "§\u{a0}".repeat(200), "§".repeat(300));
        assert!(excerpt(&wide, &["needle".into()], 41).contains("needle"));
    }
}
