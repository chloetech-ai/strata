//! Apps: programs of their own that the command center shows as views
//! (`--app crawler=http://127.0.0.1:7800`). An app answers
//!
//! - `GET /app.json`: its name, title, icon (`icon_svg`, the inside of a
//!   24-unit outline icon), `view`, the path of its view module, and
//!   optionally `maturity` (`"demo"`: the command center marks the app as
//!   a demo, on a release train of its own);
//! - its view module: JavaScript the command center loads and runs with
//!   its own helpers (the app's code is trusted as the server's is: an
//!   admin names it on the server's command line);
//! - its own API, which the view calls.
//!
//! The browser reaches an app only through this server: `GET /v1/apps`
//! lists them, and `/v1/apps/<name>/<path>` is forwarded to the app's
//! `<path>`, with the app's token (`Authorization: Bearer`) in place of
//! the caller's key: `CHLOEDB_APP_TOKEN_<NAME>` (the name in capitals, `-`
//! as `_`), else the server's `CHLOEDB_APP_TOKEN`, so that each app can
//! hold a token of its own and none can call another as the server. So an
//! app needs no address that browsers reach: the server checks the
//! caller's key (read scope for `GET`, admin for the rest; write for the
//! rest of an app that checks its callers itself, [`checks_callers`],
//! which alone takes a key limited to tables), and tells the
//! app who it was in headers it sets itself, which the app may trust
//! because they come with its token: `x-chloedb-principal` (the key's
//! name, `token` or `open`), `x-chloedb-key` (its id), `x-chloedb-scope`
//! and, for a key limited to tables, `x-chloedb-tables`
//! (`crawl_*=admin,*=read`). A caller's own headers of those names are
//! never passed on. A key limited to tables calls an app only when the
//! app's manifest says `"limited_keys": true`, that it holds such a key to
//! the grants of that header ([`Apps::holds_grants`]); it then needs only
//! a grant that reads, and the app decides the rest.
//!
//! The path is the app's own: `<path>` is joined to the app's URL segment
//! by segment, each encoded again, so no `..` (written `%2e%2e`, once or
//! twice encoded) climbs above the app's URL and no decoded `?` or `#`
//! starts a query (audit D-B9, G-B17). The listing asks every app at once
//! and keeps a failure as long as a manifest, so an app that does not
//! answer neither slows the others nor is waited for at every page (G-B10).
//!
//! The server also starts apps itself (`--start-apps`, [`crate::app_procs`]):
//! those are listed here too, each with a token of its own ([`App::token`]),
//! and with their process's state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value as Json};

use crate::app_procs::{AppKeys, Plan, Proc};

/// The most an app's answer may be.
const MAX_ANSWER: usize = 32 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    pub name: String,
    pub url: String,
    /// The token sent to this app in place of the server's
    /// (`CHLOEDB_APP_TOKEN_<NAME>`; an app the server starts has one of its
    /// own, [`crate::app_procs`]).
    pub token: Option<String>,
}

/// The variable that holds app `name`'s own token: `CHLOEDB_APP_TOKEN_`
/// and its name in capitals, `-` as `_`.
pub fn token_var(name: &str) -> String {
    format!("CHLOEDB_APP_TOKEN_{}", name.to_ascii_uppercase().replace('-', "_"))
}

/// `name=url` for each `--app`, each with its own token when the
/// environment holds one ([`token_var`]).
pub fn parse(specs: &[String]) -> Result<Vec<App>, String> {
    parse_with(specs, |v| std::env::var(v).ok())
}

/// [`parse`], the variables read by `var`.
pub fn parse_with(specs: &[String], var: impl Fn(&str) -> Option<String>) -> Result<Vec<App>, String> {
    let mut out: Vec<App> = Vec::new();
    for s in specs.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let (name, url) = s.split_once('=').ok_or_else(|| format!("--app {s:?}: name=url, as crawler=http://127.0.0.1:7800"))?;
        let (name, url) = (name.trim(), url.trim().trim_end_matches('/'));
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            return Err(format!("--app {s:?}: a name is lowercase letters, digits, - and _"));
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) || reqwest::Url::parse(url).is_err() {
            return Err(format!("--app {s:?}: {url:?} is not an http(s) URL"));
        }
        if out.iter().any(|a| a.name == name) {
            return Err(format!("--app: two apps named {name}"));
        }
        let token = var(&token_var(name)).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        out.push(App { name: name.to_string(), url: url.to_string(), token });
    }
    Ok(out)
}

pub struct Apps {
    pub list: Vec<App>,
    token: Option<String>,
    /// A folder of each app's own token, in a file of its name
    /// (`--app-token-dir`).
    token_dir: Option<std::path::PathBuf>,
    http: Option<reqwest::Client>,
    /// Each app's manifest, or why it could not be read, read at most every
    /// 15 s.
    manifests: Mutex<HashMap<String, (Instant, Result<Json, String>)>>,
    /// The apps this server runs as child processes ([`crate::app_procs`]).
    pub procs: Vec<Arc<Proc>>,
}

impl Default for Apps {
    fn default() -> Apps {
        Apps::new(Vec::new(), None)
    }
}

impl Apps {
    pub fn new(list: Vec<App>, token: Option<String>) -> Apps {
        let http = (!list.is_empty()).then(|| crate::http_client().connect_timeout(Duration::from_secs(5)).build().expect("an HTTP client"));
        Apps { list, token: token.filter(|t| !t.is_empty()), token_dir: None, http, manifests: Mutex::new(HashMap::new()), procs: Vec::new() }
    }

    /// These apps, each one's own token read from `dir` (a file of its
    /// name) when it holds one.
    pub fn with_token_dir(mut self, dir: Option<std::path::PathBuf>) -> Apps {
        self.token_dir = dir;
        self
    }

    /// These apps and the ones the server starts (`plans`), which are
    /// listed after the others under their own names and tokens.
    pub fn with_started(list: Vec<App>, token: Option<String>, plans: Vec<Plan>) -> Apps {
        let mut list = list;
        list.extend(plans.iter().map(|p| App { name: p.name.clone(), url: p.url(), token: Some(p.token.clone()) }));
        let mut apps = Apps::new(list, token);
        apps.procs = plans.into_iter().map(Proc::new).collect();
        apps
    }

    /// Starts the apps' processes, against the server at `server`.
    pub fn start(&self, server: &str, keys: Arc<dyn AppKeys>) {
        for p in &self.procs {
            tokio::spawn(p.clone().supervise(server.to_string(), keys.clone()));
        }
    }

    /// Stops the apps' processes.
    pub async fn stop(&self) {
        futures::future::join_all(self.procs.iter().map(|p| p.stop())).await;
    }

    fn proc_of(&self, name: &str) -> Option<&Arc<Proc>> {
        self.procs.iter().find(|p| p.plan.name == name)
    }

    fn find(&self, name: &str) -> Option<&App> {
        self.list.iter().find(|a| a.name == name)
    }

    /// The token app `a` is sent: its own (its file in the token folder,
    /// read now, else its variable), else the server's.
    fn token_of(&self, a: &App) -> Option<String> {
        let file = self.token_dir.as_ref().and_then(|d| std::fs::read_to_string(d.join(&a.name)).ok()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        file.or_else(|| a.token.clone()).or_else(|| self.token.clone())
    }

    /// `GET /v1/apps`: each app with its manifest, or why it does not answer.
    pub async fn listing(&self) -> Json {
        let manifests = futures::future::join_all(self.list.iter().map(|a| self.manifest(a))).await;
        let mut out = Vec::new();
        for (a, m) in self.list.iter().zip(manifests) {
            let process = self.proc_of(&a.name).map(|p| p.json());
            let mut entry = match m {
                Ok(m) => json!({
                    "name": a.name, "url": a.url, "up": true,
                    "title": m["title"].as_str().unwrap_or(&a.name), "description": m["description"], "icon_svg": m["icon_svg"],
                    "view": m["view"].as_str().map(|v| format!("/v1/apps/{}/{}", a.name, v.trim_start_matches('/'))), "version": m["version"],
                    // "demo" for an app on the demo train (netops, the
                    // feature store, the swarm): the command center marks it.
                    "maturity": m["maturity"],
                    "checks_callers": m["checks_callers"] == json!(true) || m["limited_keys"] == json!(true),
                }),
                Err(e) => json!({"name": a.name, "url": a.url, "up": false, "title": a.name, "error": e}),
            };
            if let Some(p) = process {
                entry["process"] = p;
            }
            out.push(entry);
        }
        json!({ "apps": out })
    }

    async fn manifest(&self, a: &App) -> Result<Json, String> {
        if let Some((at, m)) = self.manifests.lock().unwrap_or_else(|e| e.into_inner()).get(&a.name) {
            if at.elapsed() < Duration::from_secs(15) {
                return m.clone();
            }
        }
        let m = self.read_manifest(a).await;
        self.manifests.lock().unwrap_or_else(|e| e.into_inner()).insert(a.name.clone(), (Instant::now(), m.clone()));
        m
    }

    /// Whether app `name` holds a key limited to tables to its grants
    /// (`"limited_keys": true` in its manifest): it is then called by such
    /// keys too, as it reads `x-chloedb-tables` and refuses what they do
    /// not grant. Other apps are not called by them at all.
    pub async fn holds_grants(&self, name: &str) -> bool {
        let Some(a) = self.find(name) else { return false };
        checks_callers(&self.manifest(a).await)
    }

    async fn read_manifest(&self, a: &App) -> Result<Json, String> {
        let http = self.http.as_ref().ok_or("no apps")?;
        let mut req = http.get(format!("{}/app.json", a.url)).timeout(Duration::from_secs(5));
        if let Some(t) = self.token_of(a) {
            req = req.bearer_auth(t);
        }
        let r = req.send().await.map_err(|e| format!("{} does not answer: {e}", a.url))?;
        if !r.status().is_success() {
            return Err(format!("{}/app.json answers {}", a.url, r.status()));
        }
        r.json().await.map_err(|e| format!("{}/app.json is not JSON: {e}", a.url))
    }

    /// `/v1/apps/<name>/<rest>`: the request, sent on to the app, with who
    /// made it (`who`) in its headers.
    #[allow(clippy::too_many_arguments)]
    pub async fn forward(&self, name: &str, who: &crate::keys::Principal, method: Method, rest: &str, query: Option<&str>, headers: &HeaderMap, body: Bytes) -> Response {
        let err = |status: StatusCode, m: String| (status, axum::Json(json!({ "error": m }))).into_response();
        let Some(a) = self.find(name) else {
            return err(StatusCode::NOT_FOUND, format!("no app {name}: this server's are {}", if self.list.is_empty() { "none (--app)".into() } else { self.list.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ") }));
        };
        let url = match app_url(&a.url, rest, query) {
            Ok(u) => u,
            Err(m) => return err(StatusCode::BAD_REQUEST, m),
        };
        let Some(http) = &self.http else { return err(StatusCode::NOT_FOUND, "no apps".into()) };
        // An app that checks its callers itself ([`checks_callers`]) takes a
        // key limited to tables, and a change from a key of write scope; any
        // other takes a key of every table, and changes from an admin's.
        let get = method == Method::GET || method == Method::HEAD;
        let unchecked = who.tables.is_some() || (!get && who.scope < crate::keys::Scope::Admin);
        if unchecked && !checks_callers(&self.manifest(a).await) {
            // A key limited to tables that was given the app reads through
            // it, and only if it reads every table: the app reads as its own
            // token may, which is no more than the key may then.
            let given = who.allow.as_ref().is_some_and(|x| x.apps.iter().any(|n| n == name));
            if let Some(g) = who.tables.as_ref().filter(|_| !(given && get && who.reads_all())) {
                let why = match (given, get) {
                    (true, true) => "it was given the app, which reads as its own token may, but does not read every table (*=read)".to_string(),
                    (true, false) => "it was given the app to read through, and a change is the app's own token's".to_string(),
                    (false, _) => "it takes a key of every table".to_string(),
                };
                return err(StatusCode::FORBIDDEN, format!("the app {name} does not check a key limited to tables, so {why}: the key {:?} may touch only {}", who.name, crate::keys::grants_text(g)));
            }
            if !get && who.scope < crate::keys::Scope::Admin {
                return err(StatusCode::FORBIDDEN, format!("{method} /v1/apps/{name}/... needs a key with the admin scope; {:?} has {}", who.name, who.scope.name()));
            }
        }
        let mut req = http.request(method, url).timeout(Duration::from_secs(120));
        // The validators of what the browser holds go too, so that an app
        // that tags its answers (`ETag`) can answer 304 without the body.
        for h in [header::CONTENT_TYPE, header::ACCEPT, header::IF_NONE_MATCH, header::IF_MODIFIED_SINCE] {
            if let Some(v) = headers.get(&h) {
                req = req.header(h, v);
            }
        }
        if let Some(t) = self.token_of(a) {
            req = req.bearer_auth(t);
        }
        for (h, v) in caller_headers(who) {
            req = req.header(h, v);
        }
        let r = match req.body(body).send().await {
            Ok(r) => r,
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("the app {name} ({}) does not answer: {e}", a.url)),
        };
        let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let ctype = r.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
        // A download's name, what an app says of its answer in its own
        // headers (`x-app-*`: what an export left out), and its validators
        // (`ETag`, `Last-Modified`), go back as they came. The answer is
        // still `no-cache`: the browser asks again each time, with the tag.
        let mut passed = HeaderMap::new();
        for (k, v) in r.headers() {
            if k == reqwest::header::CONTENT_DISPOSITION || k == reqwest::header::ETAG || k == reqwest::header::LAST_MODIFIED || k.as_str().starts_with("x-app-") {
                if let (Ok(k), Ok(v)) = (header::HeaderName::from_bytes(k.as_str().as_bytes()), header::HeaderValue::from_bytes(v.as_bytes())) {
                    passed.insert(k, v);
                }
            }
        }
        // What the browser holds is current: no body.
        if status == StatusCode::NOT_MODIFIED {
            return (status, passed, [(header::CACHE_CONTROL, "no-cache")]).into_response();
        }
        if r.content_length().is_some_and(|n| n as usize > MAX_ANSWER) {
            return err(StatusCode::BAD_GATEWAY, format!("the app {name}'s answer is larger than {} MB", MAX_ANSWER >> 20));
        }
        match r.bytes().await {
            Ok(b) if b.len() <= MAX_ANSWER => (status, passed, [(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-cache".to_string())], b).into_response(),
            Ok(_) => err(StatusCode::BAD_GATEWAY, format!("the app {name}'s answer is larger than {} MB", MAX_ANSWER >> 20)),
            Err(e) => err(StatusCode::BAD_GATEWAY, format!("the app {name}'s answer was cut off: {e}")),
        }
    }
}

/// Whether an app's manifest says that it checks who called it itself:
/// `"checks_callers": true` in its `app.json`, for an app that refuses what
/// `x-chloedb-scope` and `x-chloedb-tables` do not allow (ChloeCtx's
/// spaces). Such an app is forwarded a key limited to tables, and a change
/// (not `GET`) from a key of write scope; any other is not.
pub fn checks_callers(manifest: &Result<Json, String>) -> bool {
    manifest.as_ref().is_ok_and(|m| m["checks_callers"] == json!(true) || m["limited_keys"] == json!(true))
}

/// Whether a path segment (as the router decoded it) is a dot segment, or
/// one once decoded again (`%2e`, `%2E`, `%252e`...).
pub fn dot_segment(seg: &str) -> bool {
    let mut s = seg.to_string();
    for _ in 0..4 {
        if s == "." || s == ".." {
            return true;
        }
        let next = percent_decode(&s);
        if next == s {
            return false;
        }
        s = next;
    }
    true
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The app's URL for `rest` (the decoded path after `/v1/apps/<name>/`) and
/// the request's query as it came: `rest`'s segments added to the app's
/// path, each encoded, so the URL stays under the app's.
pub fn app_url(base: &str, rest: &str, query: Option<&str>) -> Result<reqwest::Url, String> {
    let segs: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
    if segs.iter().any(|s| dot_segment(s)) {
        return Err("a path of an app has no . or .. in it".into());
    }
    let mut u = reqwest::Url::parse(base).map_err(|e| format!("the app's URL {base}: {e}"))?;
    let prefix = u.path().trim_end_matches('/').to_string();
    u.path_segments_mut().map_err(|_| format!("the app's URL {base} has no path"))?.pop_if_empty().extend(segs);
    u.set_query(query.filter(|q| !q.is_empty()));
    u.set_fragment(None);
    if !u.path().starts_with(&format!("{prefix}/")) {
        return Err(format!("{rest} is not a path of the app"));
    }
    Ok(u)
}

/// The headers that tell an app who called it through the server
/// ([`Apps::forward`]): only these are sent of the caller, so a caller's
/// own `x-chloedb-principal` never reaches the app.
pub fn caller_headers(who: &crate::keys::Principal) -> Vec<(&'static str, String)> {
    // A header holds visible ASCII: a key's name is kept to that.
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' }).collect::<String>();
    let mut out = vec![("x-chloedb-principal", clean(&who.name)), ("x-chloedb-scope", who.scope.name().to_string())];
    if let Some(k) = &who.key {
        out.push(("x-chloedb-key", clean(k)));
    }
    if let Some(t) = &who.tables {
        out.push(("x-chloedb-tables", t.iter().map(|g| format!("{}={}", g.table, g.scope.name())).collect::<Vec<_>>().join(",")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_apps() {
        let a = parse_with(&["crawler=http://127.0.0.1:7800/".into(), " x-2 = https://h ".into()], |_| None).unwrap();
        assert_eq!(a, vec![App { name: "crawler".into(), url: "http://127.0.0.1:7800".into(), token: None }, App { name: "x-2".into(), url: "https://h".into(), token: None }]);
        // Each app's own token, by its variable.
        let a = parse_with(&["crawler=http://c".into(), "x-2=http://x".into()], |v| (v == "CHLOEDB_APP_TOKEN_X_2").then(|| "t-x2".to_string())).unwrap();
        assert_eq!((a[0].token.as_deref(), a[1].token.as_deref()), (None, Some("t-x2")));
        let apps = Apps::new(a, Some("shared".into()));
        assert_eq!((apps.token_of(&apps.list[0]).as_deref(), apps.token_of(&apps.list[1]).as_deref()), (Some("shared"), Some("t-x2")));
        // A file of the token folder, read as it is sent: written later,
        // it is taken without a restart.
        let dir = tempfile::tempdir().unwrap();
        let apps = apps.with_token_dir(Some(dir.path().to_path_buf()));
        assert_eq!(apps.token_of(&apps.list[0]).as_deref(), Some("shared"));
        std::fs::write(dir.path().join("crawler"), "t-crawler\n").unwrap();
        assert_eq!(apps.token_of(&apps.list[0]).as_deref(), Some("t-crawler"));
        assert!(parse(&["crawler".into()]).is_err());
        assert!(parse(&["Crawler=http://h".into()]).is_err());
        assert!(parse(&["c=ftp://h".into()]).is_err());
        assert!(parse(&["c=http://a".into(), "c=http://b".into()]).is_err());
    }

    #[test]
    fn an_apps_path_stays_under_its_url() {
        let u = |base: &str, rest: &str, q: Option<&str>| app_url(base, rest, q).map(|u| u.to_string());
        assert_eq!(u("http://h:1", "v1/crawls/c1", Some("x=1")).unwrap(), "http://h:1/v1/crawls/c1?x=1");
        assert_eq!(u("http://h:1/tools/x", "ui/view.js", None).unwrap(), "http://h:1/tools/x/ui/view.js");
        assert_eq!(u("http://h:1/tools/x/", "a", None).unwrap(), "http://h:1/tools/x/a");
        // The router decodes once: %252e%252e arrives as %2e%2e.
        for rest in ["..", "a/../b", ".", "%2e%2e/admin", "a/%2E%2e/b", "%252e%252e", "%2e"] {
            assert!(app_url("http://h:1/tools/x", rest, None).is_err(), "{rest}");
        }
        // A decoded ? or # stays in its segment.
        assert_eq!(u("http://h:1/tools/x", "a?admin=1#y", None).unwrap(), "http://h:1/tools/x/a%3Fadmin=1%23y");
        assert_eq!(u("http://h:1", "a\\..\\b", None).unwrap(), "http://h:1/a%5C..%5Cb");
        assert!(!dot_segment("...") && !dot_segment("a.") && !dot_segment("%2"));
    }
}

#[cfg(test)]
mod caller_tests {
    use super::*;
    use crate::keys::{Grant, Principal, Scope};

    #[test]
    fn an_app_is_told_who_called() {
        let p = Principal { name: "ana\nx".into(), key: Some("0a1b".into()), scope: Scope::Write, root: false, tables: Some(vec![Grant::parse("sheets_*=write").unwrap()].into()), role: None, agent: false, on_behalf_of: None, allow: None };
        let h = caller_headers(&p);
        assert!(h.contains(&("x-chloedb-principal", "ana?x".into())));
        assert!(h.contains(&("x-chloedb-scope", "write".into())) && h.contains(&("x-chloedb-key", "0a1b".into())));
        assert!(h.contains(&("x-chloedb-tables", "sheets_*=write".into())));
        assert_eq!(caller_headers(&Principal::open()).len(), 2);
    }
}
