//! Apps (`--app name=url`): the server lists them with their manifests,
//! and forwards `/v1/apps/<name>/...` to them with its own app token in
//! place of the caller's key, a read key for `GET` and an admin key for
//! the rest.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value as Json};
use chloedb::db::Db;
use chloedb::storage::DbOptions;
use chloedb_server::apps::{parse, Apps};
use chloedb_server::auth::{Access, Secret};
use chloedb_server::keys::Keys;
use chloedb_server::service::{AgentConfig, Backend, Service};
use chloedb_server::{demo, server};
use tokio::net::TcpListener;

/// What the app was asked: method, path and query, its bearer token, body.
type Seen = Arc<Mutex<Vec<(String, String, String, String)>>>;

async fn fake_app() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let s = seen.clone();
    let app = Router::new()
        .route(
            "/app.json",
            get(|| async { axum::Json(json!({"name": "crawler", "title": "Crawler", "description": "a test app", "icon_svg": "<circle cx=\"12\" cy=\"12\" r=\"9\"/>", "view": "ui/view.js", "version": "9.9"})) }),
        )
        .fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let s = s.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                s.lock().unwrap().push((method.to_string(), uri.to_string(), auth, String::from_utf8_lossy(&body).into_owned()));
                if uri.path() == "/ui/view.js" {
                    // Tagged, and not sent again to a browser that holds it.
                    if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some("\"v1\"") {
                        return (axum::http::StatusCode::NOT_MODIFIED, [("etag", "\"v1\"")]).into_response();
                    }
                    return ([("content-type", "text/javascript"), ("etag", "\"v1\"")], "export default { render() {} };").into_response();
                }
                ([("content-type", "application/json")], json!({"ok": true, "path": uri.path()}).to_string()).into_response()
            }
        });
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

async fn call(base: &str, method: &str, path: &str, auth: Option<&str>, body: Option<Json>) -> (u16, String) {
    let c = chloedb_server::http_client().build().unwrap();
    let mut r = c.request(method.parse().unwrap(), format!("{base}{path}"));
    if let Some(a) = auth {
        r = r.bearer_auth(a);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn apps_are_listed_and_forwarded_with_the_servers_token() {
    let (app_url, seen) = fake_app().await;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("a.strata"), demo::schema(10), DbOptions::default()).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent, 16, Duration::from_secs(10));
    svc.keys = Arc::new(Keys::open(dir.path().join("a.strata.keys.json")).unwrap());
    // One app that answers, one that does not.
    svc.apps = Apps::new(parse(&[format!("crawler={app_url}/"), "gone=http://127.0.0.1:9".into()]).unwrap(), Some("app-secret".into()));
    let svc = Arc::new(svc);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_http(svc, l, Access { token: Some(Secret::new("root-token")), ..Default::default() }));
    let root = Some("root-token");
    let (s, j) = call(&base, "POST", "/v1/admin/keys", root, Some(json!({"name": "viewer", "scope": "read"}))).await;
    assert_eq!(s, 200, "{j}");
    let reader: Json = serde_json::from_str(&j).unwrap();
    let reader = reader["secret"].as_str().unwrap().to_string();
    let read = Some(reader.as_str());

    // The listing: each app's manifest, or why it does not answer.
    assert_eq!(call(&base, "GET", "/v1/apps", None, None).await.0, 401);
    let (s, j) = call(&base, "GET", "/v1/apps", read, None).await;
    assert_eq!(s, 200, "{j}");
    let j: Json = serde_json::from_str(&j).unwrap();
    let apps = j["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 2);
    assert_eq!(apps[0]["name"], "crawler");
    assert_eq!(apps[0]["up"], true);
    assert_eq!(apps[0]["title"], "Crawler");
    assert_eq!(apps[0]["view"], "/v1/apps/crawler/ui/view.js");
    assert_eq!(apps[1]["name"], "gone");
    assert_eq!(apps[1]["up"], false);
    assert!(apps[1]["error"].as_str().unwrap().contains("does not answer"), "{}", apps[1]);

    // A read key reads through it; the app sees the server's token, never the key.
    let (s, body) = call(&base, "GET", "/v1/apps/crawler/ui/view.js", read, None).await;
    assert_eq!((s, body.as_str()), (200, "export default { render() {} };"));
    let (s, body) = call(&base, "GET", "/v1/apps/crawler/v1/crawls/c1?x=1", read, None).await;
    assert_eq!(s, 200, "{body}");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], ("GET".into(), "/ui/view.js".into(), "Bearer app-secret".into(), String::new()));
        assert_eq!(seen[1], ("GET".into(), "/v1/crawls/c1?x=1".into(), "Bearer app-secret".into(), String::new()));
        assert!(seen.iter().all(|(.., auth, _)| !auth.contains(&reader) && !auth.contains("root-token")));
    }

    // Changes need an admin key.
    let start = json!({"seeds": ["https://example.com/"]});
    assert_eq!(call(&base, "POST", "/v1/apps/crawler/v1/crawls", read, Some(start.clone())).await.0, 403);
    assert_eq!(call(&base, "DELETE", "/v1/apps/crawler/v1/crawls/c1", read, None).await.0, 403);
    let (s, body) = call(&base, "POST", "/v1/apps/crawler/v1/crawls", root, Some(start.clone())).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(serde_json::from_str::<Json>(&body).unwrap()["path"], "/v1/crawls");
    {
        let seen = seen.lock().unwrap();
        let last = seen.last().unwrap();
        assert_eq!((last.0.as_str(), last.1.as_str(), last.2.as_str()), ("POST", "/v1/crawls", "Bearer app-secret"));
        assert_eq!(serde_json::from_str::<Json>(&last.3).unwrap(), start);
    }

    // No such app; an app that does not answer.
    let (s, body) = call(&base, "GET", "/v1/apps/nope/x", read, None).await;
    assert_eq!(s, 404, "{body}");
    assert!(body.contains("crawler, gone"), "{body}");
    let (s, body) = call(&base, "GET", "/v1/apps/gone/x", read, None).await;
    assert_eq!(s, 502, "{body}");
}

/// A server on `addr` that takes connections and never answers.
async fn silent() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((c, _)) = l.accept().await {
            held.push(c);
        }
    });
    url
}

async fn serve_apps(apps: Apps) -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("a.strata"), demo::schema(10), DbOptions::default()).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent, 16, Duration::from_secs(10));
    svc.apps = apps;
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_http(Arc::new(svc), l, Access { token: Some(Secret::new("root-token")), ..Default::default() }));
    (base, dir)
}

// Audit D-B9, G-B17: a path the router decoded once still climbed out of an
// app mounted under a path, and a decoded ? started a query.
#[tokio::test(flavor = "multi_thread")]
async fn an_apps_path_cannot_climb_out_of_its_url_or_add_a_query() {
    let (app_url, seen) = fake_app().await;
    let (base, _dir) = serve_apps(Apps::new(parse(&[format!("tools={app_url}/tools/x")]).unwrap(), Some("app-secret".into()))).await;
    let root = Some("root-token");
    let (s, body) = call(&base, "GET", "/v1/apps/tools/a/%252e%252e/%252e%252e/admin", root, None).await;
    assert_eq!(s, 400, "{body}");
    let (s, body) = call(&base, "GET", "/v1/apps/tools/a/%252E%252e/admin", root, None).await;
    assert_eq!(s, 400, "{body}");
    let (s, body) = call(&base, "GET", "/v1/apps/tools/a%3Fadmin=1", root, None).await;
    assert_eq!(s, 200, "{body}");
    let (s, body) = call(&base, "GET", "/v1/apps/tools/v1/crawls?x=1", root, None).await;
    assert_eq!(s, 200, "{body}");
    let seen = seen.lock().unwrap();
    let paths: Vec<&str> = seen.iter().map(|(_, p, ..)| p.as_str()).collect();
    assert_eq!(paths, ["/tools/x/a%3Fadmin=1", "/tools/x/v1/crawls?x=1"], "only the app's own paths, the ? kept in its segment");
}

// Audit G-B10: apps that do not answer made every listing wait 5 s each, one
// after another, and the command center gave up on all of them.
#[tokio::test(flavor = "multi_thread")]
async fn apps_that_do_not_answer_do_not_hold_up_the_listing() {
    let (app_url, _seen) = fake_app().await;
    let (dead1, dead2) = (silent().await, silent().await);
    let (base, _dir) = serve_apps(Apps::new(parse(&[format!("crawler={app_url}"), format!("a={dead1}"), format!("b={dead2}")]).unwrap(), None)).await;
    let t0 = std::time::Instant::now();
    let (s, j) = call(&base, "GET", "/v1/apps", Some("root-token"), None).await;
    let first = t0.elapsed();
    assert_eq!(s, 200, "{j}");
    let j: Json = serde_json::from_str(&j).unwrap();
    let up: Vec<bool> = j["apps"].as_array().unwrap().iter().map(|a| a["up"].as_bool().unwrap()).collect();
    assert_eq!(up, [true, false, false]);
    assert!(first < Duration::from_millis(7500), "the apps are asked at once: {first:?}");
    // Their failures are kept: the next listing does not wait for them.
    let t0 = std::time::Instant::now();
    let (s, _) = call(&base, "GET", "/v1/apps", Some("root-token"), None).await;
    assert_eq!(s, 200);
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
}

/// A stand-in for chloedb-sheets: serves the socket the server hands it
/// (`CHLOEDB_LISTEN_FD`, which it requires: the port is the server's, and
/// binding it would fail), asks for its token, and answers `/env` with the
/// names of the settings it was given.
const FAKE_SHEETS: &str = r#"#!/usr/bin/env python3
import http.server, json, os, socket, sys
args = sys.argv[1:]
listen = args[args.index("--listen") + 1]
host, port = listen.rsplit(":", 1)
token = os.environ.get("CHLOEDB_SHEETS_TOKEN")
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        if self.headers.get("authorization") != "Bearer " + str(token):
            self.send_response(401); self.end_headers(); return
        if self.path == "/app.json":
            body = {"name": "sheets", "title": "Sheets", "view": "ui/view.js"}
        else:
            body = {"env": sorted(os.environ), "args": args, "start_apps": os.environ.get("CHLOEDB_START_APPS"), "key_prefix": os.environ.get("CHLOEDB_KEY", "")[:3]}
        b = json.dumps(body).encode()
        self.send_response(200); self.send_header("content-type", "application/json"); self.end_headers(); self.wfile.write(b)
sock = socket.socket(fileno=int(os.environ["CHLOEDB_LISTEN_FD"]))
assert sock.getsockname()[1] == int(port), (sock.getsockname(), port)
srv = http.server.HTTPServer((host, int(port)), H, bind_and_activate=False)
srv.socket.close()
srv.socket = sock
print("fake sheets up on " + listen, file=sys.stderr, flush=True)
srv.serve_forever()
"#;

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// `--start-apps`: the server starts the app as a child with a token of its
/// own, an ephemeral key scoped to its tables and none of the server's
/// credentials, starts it again (with a new key) when it exits, and stops
/// it when the server stops.
#[tokio::test(flavor = "multi_thread")]
async fn the_server_starts_its_apps_and_keeps_them_running() {
    use std::os::unix::fs::PermissionsExt;
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: no python3 for the stand-in app");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("chloedb-sheets");
    std::fs::write(&fake, FAKE_SHEETS).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let log = std::fs::File::create(dir.path().join("server.log")).unwrap();
    let mut server = std::process::Command::new(env!("CARGO_BIN_EXE_chloedb-server"))
        .args(["--db", &dir.path().join("a.strata").to_string_lossy(), "--listen", &format!("127.0.0.1:{port}"), "--grpc", "off", "--log-store", "off", "--start-apps", "sheets"])
        .env("PATH", path)
        // Credentials the app must not see.
        .env("AWS_SECRET_ACCESS_KEY", "not-for-apps")
        .env("CHLOEDB_APP_TOKEN", "not-for-apps")
        .env("CHLOEDB_TOKEN", "root-of-the-server")
        .env_remove("CHLOEDB_CLUSTER_TOKEN")
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let logs = || std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default();
    let listing = || async {
        let (s, b) = call(&base, "GET", "/v1/apps", Some("root-of-the-server"), None).await;
        assert_eq!(s, 200, "{b}");
        let j: Json = serde_json::from_str(&b).unwrap();
        j["apps"].as_array().unwrap().iter().find(|a| a["name"] == "sheets").cloned().unwrap_or(Json::Null)
    };
    let t0 = std::time::Instant::now();
    let up = loop {
        if chloedb_server::http_client().build().unwrap().get(format!("{base}/v1/health")).send().await.is_ok() {
            let a = listing().await;
            if a["up"] == true {
                break a;
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "the app did not come up: {}", logs());
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(up["title"], "Sheets");
    assert_eq!(up["process"]["running"], true, "{up}");
    assert!(up["url"].as_str().unwrap().starts_with("http://127.0.0.1:"), "{up}");
    // Forwarded with the app's own token (the stand-in refuses any other).
    let (s, b) = call(&base, "GET", "/v1/apps/sheets/env", Some("root-of-the-server"), None).await;
    assert_eq!(s, 200, "{b}");
    let env: Json = serde_json::from_str(&b).unwrap();
    let names: Vec<&str> = env["env"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    for leak in ["AWS_SECRET_ACCESS_KEY", "CHLOEDB_APP_TOKEN", "CHLOEDB_TOKEN", "CHLOEDB_CLUSTER_TOKEN"] {
        assert!(!names.contains(&leak), "{leak} reached the app: {names:?}");
    }
    assert!(names.contains(&"CHLOEDB_SHEETS_TOKEN") && names.contains(&"PATH"), "{names:?}");
    assert_eq!(env["start_apps"], "off");
    // A key of its own: an API key, not the server's token, scoped to its
    // tables, in memory only.
    assert_eq!(env["key_prefix"], "sk_", "{env}");
    let app_keys = || async {
        let (s, b) = call(&base, "GET", "/v1/admin/keys", Some("root-of-the-server"), None).await;
        assert_eq!(s, 200, "{b}");
        let j: Json = serde_json::from_str(&b).unwrap();
        j["keys"].as_array().unwrap().iter().filter(|k| k["name"] == "app:sheets" && k["revoked_ms"].is_null()).cloned().collect::<Vec<_>>()
    };
    let keys = app_keys().await;
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert_eq!(keys[0]["ephemeral"], true, "{keys:?}");
    assert!(!dir.path().join("a.strata.keys.json").exists() || !std::fs::read_to_string(dir.path().join("a.strata.keys.json")).unwrap().contains("app:sheets"), "an app's key is not written down");
    let first_key = keys[0]["id"].clone();
    assert!(env["args"].as_array().unwrap().iter().any(|a| a == &json!(base)), "it calls this server back: {env}");
    // Its output is the server's log, and the listing's tail.
    assert!(up["process"]["tail"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("fake sheets up")), "{up}");
    // Killed, it is started again, on the same socket: the port stays the
    // server's while the app is down, so no one else can take it.
    let pid = up["process"]["pid"].as_i64().unwrap() as i32;
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let app_addr = up["url"].as_str().unwrap().trim_start_matches("http://").to_string();
    assert!(std::net::TcpListener::bind(&app_addr).is_err(), "{app_addr} is free while the app is down");
    let t0 = std::time::Instant::now();
    let again = loop {
        let a = listing().await;
        if a["up"] == true && a["process"]["pid"].as_i64().is_some_and(|p| p as i32 != pid) {
            break a;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "the app was not started again: {a} {}", logs());
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(again["process"]["restarts"], 1, "{again}");
    assert_eq!(again["url"], up["url"], "the same port");
    let keys = app_keys().await;
    assert!(keys.len() == 1 && keys[0]["id"] != first_key, "started again with a new key, the old one ended: {keys:?}");
    assert!(again["process"]["last_exit"].as_str().unwrap().contains("exited"), "{again}");
    // The server stopped, the app stops too.
    let pid = again["process"]["pid"].as_i64().unwrap() as i32;
    unsafe { libc::kill(server.id() as i32, libc::SIGTERM) };
    let _ = server.wait();
    let t0 = std::time::Instant::now();
    while alive(pid) {
        assert!(t0.elapsed() < Duration::from_secs(15), "the app outlived the server: {}", logs());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// NET-19: the proxy passed only Content-Type and Accept, so an app's view
// (326 KB for netops) was fetched whole on every visit: it passes the
// browser's If-None-Match and If-Modified-Since on, and the app's ETag and
// a 304 back.
#[tokio::test(flavor = "multi_thread")]
async fn an_apps_tags_reach_the_browser_and_a_304_comes_back() {
    let (app_url, _seen) = fake_app().await;
    let (base, _dir) = serve_apps(Apps::new(parse(&[format!("crawler={app_url}/")]).unwrap(), Some("app-secret".into()))).await;
    let c = chloedb_server::http_client().build().unwrap();
    let url = format!("{base}/v1/apps/crawler/ui/view.js");
    let r = c.get(&url).bearer_auth("root-token").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers().get("etag").and_then(|v| v.to_str().ok()), Some("\"v1\""));
    assert_eq!(r.headers().get("cache-control").and_then(|v| v.to_str().ok()), Some("no-cache"), "asked again each time, with the tag");
    assert!(r.text().await.unwrap().contains("render"));
    let r = c.get(&url).bearer_auth("root-token").header("if-none-match", "\"v1\"").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 304);
    assert_eq!(r.headers().get("etag").and_then(|v| v.to_str().ok()), Some("\"v1\""));
    assert!(r.bytes().await.unwrap().is_empty());
    // Another tag: the view again.
    let r = c.get(&url).bearer_auth("root-token").header("if-none-match", "\"v0\"").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
}
