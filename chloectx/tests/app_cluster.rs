//! `chloectx app` against a real ChloeDB cluster: two shard servers and a
//! coordinator that shows the app (`--app chloectx=URL`). Spaces are made,
//! notes written, replaced and retired, contexts asked for within a budget,
//! an exchange distilled, incidents recorded and matched to an alert that
//! fires, an app made from a space by a stub of the app engine and loaded
//! with the notes picked, and everything read back after the app restarts.
//! The model is a stub of Anthropic's API.
//!
//! It needs the chloedb-server binary (`CHLOEDB_SERVER_BIN`, else
//! `../chloedb-server/target/{ci,release}/chloedb-server`), and is skipped
//! without it unless `CHLOECTX_NEED_SERVER` is set (as CI does).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TOKEN: &str = "chloectx-test-token";

fn server_bin() -> Option<PathBuf> {
    if let Some(b) = std::env::var_os("CHLOEDB_SERVER_BIN") {
        return Some(PathBuf::from(b));
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../chloedb-server/target");
    ["ci", "release"].iter().map(|p| root.join(p).join("chloedb-server")).find(|p| p.exists())
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A child process, killed when dropped.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts a process; the servers' logs show with CHLOECTX_TEST_LOGS set.
fn spawn(cmd: &mut Command) -> Proc {
    let logs = if std::env::var_os("CHLOECTX_TEST_LOGS").is_some() { Stdio::inherit() } else { Stdio::null() };
    Proc(cmd.stdout(Stdio::null()).stderr(logs).spawn().expect("start a process"))
}

struct Http {
    agent: ureq::Agent,
}

impl Http {
    fn new() -> Http {
        Http { agent: ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(60))).build().into() }
    }

    fn call(&self, method: &str, url: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> (u16, Value) {
        let mut req = ureq::http::Request::builder().method(method).uri(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let r = match body {
            Some(b) => self.agent.run(req.header("content-type", "application/json").body(b.to_string()).unwrap()),
            None => self.agent.run(req.body(()).unwrap()),
        };
        let mut r = match r {
            Ok(r) => r,
            Err(e) => return (0, json!({"error": e.to_string()})),
        };
        let status = r.status().as_u16();
        let text = r.body_mut().read_to_string().unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

/// The app's API, called with its token.
struct App<'a> {
    http: &'a Http,
    url: String,
}

impl App<'_> {
    fn get(&self, path: &str) -> Value {
        let (s, j) = self.http.call("GET", &format!("{}{path}", self.url), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
        assert_eq!(s, 200, "GET {path}: {j}");
        j
    }

    fn post(&self, path: &str, body: Value) -> Value {
        let (s, j) = self.try_post(path, body);
        assert_eq!(s, 200, "POST {path}: {j}");
        j
    }

    fn try_post(&self, path: &str, body: Value) -> (u16, Value) {
        self.http.call("POST", &format!("{}{path}", self.url), Some(&body), &[("authorization", &format!("Bearer {TOKEN}"))])
    }
}

/// Anthropic's Messages API, answering as the app's two prompts expect:
/// a decision for a distillation, an incident for a draft.
fn stub_model() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut len = 0usize;
            let mut key_ok = false;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                key_ok |= lower.starts_with("x-api-key: test-key");
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let body = String::from_utf8_lossy(&body);
            let prompt: String = serde_json::from_str::<Value>(&body).ok().and_then(|j| j["messages"][0]["content"].as_str().map(str::to_string)).unwrap_or_default();
            let text = if !key_ok {
                String::new()
            } else if prompt.contains("Suggest ") && prompt.contains("apps") {
                assert!(!prompt.contains("old mailer"), "a retired note is not shown to the model");
                json!({"apps": [
                    {"title": "Refund approvals", "pitch": "See who approves which refund.", "why": "The space decides refund limits.", "kinds": ["decision", "rule"], "description": "A tracker of the billing team's decisions and rules, one row each, with its text, kind and date, and a screen to search them."},
                    {"title": "Dunning calendar", "pitch": "When each email goes.", "why": "A decision sets the schedule.", "kinds": ["decision", "mood"], "description": "A calendar of the dunning emails the billing agents send after a failed payment."},
                    {"title": "", "description": "no title: left out"}]}).to_string()
            } else if prompt.contains("Records to load") {
                let rows: Vec<Value> = prompt.split("Records to load").nth(1).unwrap_or("").lines()
                    .filter_map(|l| l.strip_prefix("- ")).filter_map(|l| l.split(", ").next())
                    .map(|id| json!({"from": id, "table": "decisions", "fields": {"text": format!("row of {id}"), "id": "mine"}}))
                    .chain([json!({"from": "invented", "table": "decisions", "fields": {"text": "never"}})]).collect();
                json!({"rows": rows}).to_string()
            } else if body.contains("long-term notes") {
                r#"{"kind": "decision", "text": "The billing agents send dunning emails on day 1, 4 and 10 after a failed payment, because customers complained about daily emails.", "entities": ["dunning emails"], "when": ["changing dunning"]}"#.to_string()
            } else {
                r#"{"title": "Shard 2 ran out of disk during a backfill", "symptom": "disk-filling fired on shard 2", "cause": "old segments kept until the next snapshot", "fix": "took a snapshot, then backfilled in batches", "alert": "disk-filling"}"#.to_string()
            };
            let answer = json!({"content": [{"type": "text", "text": text}], "usage": {"input_tokens": 100, "output_tokens": 20}});
            let a = answer.to_string();
            let status = if key_ok { "200 OK" } else { "401 Unauthorized" };
            let _ = write!(s, "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{a}", a.len());
        }
    });
    url
}

/// What the stub of the app engine was asked.
#[derive(Default)]
struct Engine {
    descriptions: Vec<String>,
    rows: Vec<Value>,
    polls: usize,
    tokens_ok: bool,
}

/// The app engine (chloedb-apps) as the coordinator reaches it: it builds
/// app `a1` (done at the second look), with one table, and takes rows.
fn stub_engine(seen: Arc<Mutex<Engine>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            let _ = r.read_line(&mut first);
            let (mut len, mut token) = (0usize, false);
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                token |= lower.starts_with(&format!("authorization: bearer {TOKEN}"));
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let mut parts = first.split_whitespace();
            let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("").split('?').next().unwrap_or("").to_string());
            let mut e = seen.lock().unwrap();
            e.tokens_ok |= token;
            let answer = match (method, path.as_str()) {
                ("GET", "/app.json") => json!({"title": "App engine", "description": "stub", "view": "web/cc-view.js", "version": "0"}),
                ("GET", "/api/status") => json!({"model": "stub", "spend": {"usd": 1.0, "cap": 25.0}}),
                ("POST", "/api/apps") => {
                    e.descriptions.push(body["description"].as_str().unwrap_or("").to_string());
                    json!({"app": "a1", "request": "q1"})
                }
                ("GET", "/api/apps/a1") => {
                    e.polls += 1;
                    let status = if e.polls >= 2 { "done" } else { "running" };
                    json!({"requests": [{"id": "q1", "status": status, "cost": 0.12}], "tables": [{"name": "decisions", "description": "the team's decisions", "columns": [{"name": "text", "type": "text"}]}]})
                }
                ("POST", "/api/apps/a1/insert") => {
                    e.rows.push(body.clone());
                    json!({"row": body["row"]})
                }
                ("POST", "/api/handoff") => json!({"url": format!("http://studio.example/login?code=x&next={}", body["next"].as_str().unwrap_or(""))}),
                _ => json!({"error": format!("no {method} {path}")}),
            };
            drop(e);
            let a = answer.to_string();
            let status = if answer.get("error").is_some() { "404 Not Found" } else { "200 OK" };
            let _ = write!(s, "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{a}", a.len());
        }
    });
    url
}

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("{what}: not within {secs} s");
}

fn start_app(dir: &Path, coord: &str, port: u16, model: &str) -> Proc {
    spawn(
        Command::new(env!("CARGO_BIN_EXE_chloectx"))
            .args(["app", "--server", coord, "--port", &port.to_string(), "--embedder", "hash:256", "--token", TOKEN])
            .current_dir(dir)
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("ANTHROPIC_BASE_URL", model)
            .env_remove("CHLOECTX_APP_MODEL"),
    )
}

fn texts(notes: &Value) -> Vec<String> {
    notes["notes"].as_array().unwrap().iter().map(|n| n["text"].as_str().unwrap().to_string()).collect()
}

#[test]
fn app_in_a_cluster() {
    let Some(bin) = server_bin() else {
        assert!(std::env::var_os("CHLOECTX_NEED_SERVER").is_none(), "CHLOECTX_NEED_SERVER is set but there is no chloedb-server binary");
        eprintln!("skipped: no chloedb-server binary (set CHLOEDB_SERVER_BIN)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let http = Http::new();
    let model = stub_model();
    let engine = Arc::new(Mutex::new(Engine::default()));
    let engine_url = stub_engine(engine.clone());

    // Two shards and a coordinator that shows the app.
    let mut grpc = Vec::new();
    let mut procs = Vec::new();
    for i in 0..2 {
        let (h, g) = (free_port(), free_port());
        grpc.push(format!("http://127.0.0.1:{g}"));
        procs.push(spawn(Command::new(&bin).args(["--db", &dir.path().join(format!("s{i}.strata")).to_string_lossy(), "--listen", &format!("127.0.0.1:{h}"), "--grpc", &format!("127.0.0.1:{g}"), "--no-fetch-models"])));
    }
    let (cport, aport) = (free_port(), free_port());
    let coord = format!("http://127.0.0.1:{cport}");
    let app_url = format!("http://127.0.0.1:{aport}");
    wait_for("the shards", 30, || grpc.iter().all(|g| std::net::TcpStream::connect(g.trim_start_matches("http://")).is_ok()));
    let _coordinator = spawn(
        Command::new(&bin)
            .args(["--shards", &grpc.join(","), "--listen", &format!("127.0.0.1:{cport}"), "--grpc", "off", "--app", &format!("chloectx={app_url}"), "--app", &format!("apps={engine_url}")])
            .env("CHLOEDB_APP_TOKEN", TOKEN),
    );
    wait_for("the coordinator", 30, || http.call("GET", &format!("{coord}/v1/health"), None, &[]).0 == 200);
    let mut app_proc = start_app(dir.path(), &coord, aport, &model);
    let app = App { http: &http, url: app_url.clone() };
    wait_for("the app", 60, || http.call("GET", &format!("{app_url}/v1/info"), None, &[("authorization", &format!("Bearer {TOKEN}"))]).0 == 200);
    let info = app.get("/v1/info");
    assert_eq!(info["model"]["how"], "api", "{info}");
    assert_eq!(info["server"]["ok"], true, "{info}");

    // Local stores are off unless asked for.
    let (s, _) = http.call("GET", &format!("{app_url}/v1/local"), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
    assert_eq!(s, 404, "local stores are shown only with --local-stores");

    // Only callers with the token, and never a page in a browser.
    assert_eq!(http.call("GET", &format!("{app_url}/v1/spaces"), None, &[]).0, 401);
    assert_eq!(http.call("GET", &format!("{app_url}/v1/spaces"), None, &[("authorization", &format!("Bearer {TOKEN}")), ("origin", "http://evil.example")]).0, 403);
    // The command center reaches it through the coordinator.
    let (s, apps) = http.call("GET", &format!("{coord}/v1/apps"), None, &[]);
    assert_eq!(s, 200);
    let listed = apps["apps"].as_array().unwrap().iter().find(|a| a["name"] == "chloectx").cloned().unwrap_or_default();
    assert_eq!(listed["up"], true, "{apps}");
    let (s, view) = http.call("GET", &format!("{coord}/v1/apps/chloectx/app.json"), None, &[]);
    assert_eq!((s, view["view"].as_str()), (200, Some("ui/view.js")));

    // Spaces.
    for bad in ["Bad-Name", "spaces", "1abc", ""] {
        assert_eq!(app.try_post("/v1/spaces", json!({"name": bad})).0, 400, "{bad:?}");
    }
    let s = app.post("/v1/spaces", json!({"name": "billing", "title": "Billing agents", "description": "invoices and refunds"}));
    assert_eq!(s["table"], "ctx_billing");
    assert_eq!(app.post("/v1/spaces", json!({"name": "billing", "title": "Other"}))["title"], "Billing agents", "a space that exists is left as it is");
    let (s, _) = http.call("GET", &format!("{app_url}/v1/spaces/nothere/notes"), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
    assert_eq!(s, 404);

    // Notes: written, replaced (the old one kept as history), retired.
    let rule = app.post("/v1/spaces/billing/notes", json!({"kind": "rule", "text": "Invoices are issued in the customer's currency, never converted to USD.", "entities": ["invoices"]}));
    let old = app.post("/v1/spaces/billing/notes", json!({"kind": "decision", "text": "Refunds over 500 EUR need a second approver from finance.", "entities": ["refunds"]}));
    let new = app.post("/v1/spaces/billing/notes", json!({"kind": "decision", "text": "Refunds over 1000 EUR need a second approver from finance (raised from 500 EUR).", "entities": ["refunds"], "replaces": old["id"]}));
    assert_eq!(new["supersedes"], json!([old["id"]]));
    let gone = app.post("/v1/spaces/billing/notes", json!({"kind": "fact", "text": "The old mailer sends the dunning emails every day."}));
    assert_eq!(app.try_post("/v1/spaces/billing/notes", json!({"kind": "mood", "text": "happy"})).0, 400);
    let (s, retired) = http.call("DELETE", &format!("{app_url}/v1/spaces/billing/notes/{}", gone["id"].as_str().unwrap()), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
    assert_eq!(s, 200);
    // APC-22: nothing replaced it, so it is retired, not superseded.
    assert_eq!(retired["tag"], "retired", "{retired}");
    assert_eq!(app.get(&format!("/v1/spaces/billing/notes/{}", gone["id"].as_str().unwrap()))["later"], json!([]));
    let live = texts(&app.get("/v1/spaces/billing/notes"));
    assert_eq!(live.len(), 2, "{live:?}");
    assert!(live.iter().any(|t| t.contains("1000 EUR")) && !live.iter().any(|t| t.contains("500 EUR need") || t.contains("old mailer")), "{live:?}");
    assert_eq!(texts(&app.get("/v1/spaces/billing/notes?all=1")).len(), 4);
    // A space's counts: every record, and the notes a context still shows.
    let listed = app.get("/v1/spaces");
    let billing = listed["spaces"].as_array().unwrap().iter().find(|s| s["name"] == "billing").unwrap();
    assert_eq!((billing["by_kind"]["decision"].as_i64(), billing["by_kind"]["fact"].as_i64()), (Some(2), Some(1)), "{billing}");
    assert_eq!((billing["live_by_kind"]["decision"].as_i64(), billing["live_by_kind"]["fact"].as_i64(), billing["live_by_kind"]["preference"].as_i64()), (Some(1), None, Some(1)), "{billing}");
    let h = app.get(&format!("/v1/spaces/billing/notes/{}", old["id"].as_str().unwrap()));
    assert_eq!(h["note"]["tag"], "superseded");
    assert_eq!(h["later"][0]["id"], new["id"], "{h}");
    let h = app.get(&format!("/v1/spaces/billing/notes/{}", new["id"].as_str().unwrap()));
    assert_eq!(h["earlier"][0]["id"], old["id"], "{h}");

    // The space as an MCP server, for an agent whose key is limited to its
    // table (the grants chloedb-server sends in x-chloedb-tables).
    let auth = format!("Bearer {TOKEN}");
    let mcp = |space: &str, grants: &str, body: Value| http.call("POST", &format!("{app_url}/v1/spaces/{space}/mcp"), Some(&body), &[("authorization", &auth), ("x-chloedb-tables", grants)]);
    let (s, init) = mcp("billing", "ctx_billing=admin", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}}));
    assert_eq!((s, init["result"]["protocolVersion"].as_str(), init["result"]["serverInfo"]["name"].as_str()), (200, Some("2025-06-18"), Some("chloectx-billing")), "{init}");
    assert_eq!(mcp("billing", "ctx_billing=admin", json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).0, 202);
    let (_, listed) = mcp("billing", "ctx_billing=admin", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let names: Vec<&str> = listed["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names, ["ask", "remember", "notes"], "{listed}");
    let call = |grants: &str, name: &str, args: Value| mcp("billing", grants, json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": name, "arguments": args}})).1;
    let asked = call("ctx_billing=admin", "ask", json!({"question": "Who approves a refund of 700 EUR?"}));
    let text = asked["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("1000 EUR") && text.contains(new["id"].as_str().unwrap()), "{asked}");
    let kept = call("ctx_billing=admin", "remember", json!({"kind": "fact", "text": "Finance closes the books on the third working day."}));
    assert!(kept["result"]["content"][0]["text"].as_str().unwrap_or("").starts_with("Kept as "), "{kept}");
    let listed = call("ctx_billing=admin", "notes", json!({"kind": "fact"}));
    assert!(listed["result"]["content"][0]["text"].as_str().unwrap_or("").contains("third working day"), "{listed}");
    // A reader asks and may not write; another space, or the app's other
    // routes, are not its key's.
    let refused = call("ctx_billing=read", "remember", json!({"kind": "fact", "text": "x"}));
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert_eq!(mcp("payroll", "ctx_billing=admin", json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"})).0, 403);
    assert_eq!(http.call("GET", &format!("{app_url}/v1/memory"), None, &[("authorization", &auth), ("x-chloedb-tables", "ctx_billing=admin")]).0, 403);
    assert_eq!(http.call("POST", &format!("{app_url}/v1/spaces"), Some(&json!({"name": "other"})), &[("authorization", &auth), ("x-chloedb-tables", "ctx_*=admin")]).0, 403);
    let (_, mine) = http.call("GET", &format!("{app_url}/v1/spaces"), None, &[("authorization", &auth), ("x-chloedb-tables", "ctx_billing=read")]);
    assert_eq!(mine["spaces"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect::<Vec<_>>(), ["billing"], "{mine}");
    // Retire the fact the agent kept, for the counts below.
    let id = kept["result"]["content"][0]["text"].as_str().unwrap().trim_start_matches("Kept as ").trim_end_matches('.').to_string();
    assert_eq!(http.call("DELETE", &format!("{app_url}/v1/spaces/billing/notes/{id}"), None, &[("authorization", &auth)]).0, 200);

    // A context within a budget: the current version, with what it changed;
    // the old one left out, and why.
    let a = app.post("/v1/spaces/billing/ask", json!({"question": "Who approves a refund of 700 EUR?", "budget": 1000}));
    assert_eq!(a["model_calls"], 0);
    assert!(a["tokens"].as_u64().unwrap() <= 1000, "{a}");
    let items = a["items"].as_array().unwrap();
    let refund = items.iter().find(|i| i["id"] == new["id"]).unwrap_or_else(|| panic!("the new decision is in the context: {a}"));
    assert!(refund["replaced"].as_str().unwrap_or("").contains("500 EUR"), "{refund}");
    assert!(refund["bm25"].is_number() && refund["tokens"].as_u64().unwrap() > 0, "{refund}");
    assert!(items.iter().any(|i| i["id"] == rule["id"]), "the rule is in it: {a}");
    assert!(!items.iter().any(|i| i["id"] == old["id"] || i["id"] == gone["id"]), "{a}");
    assert!(a["left_out"].as_array().unwrap().iter().any(|l| l["id"] == old["id"] && l["why"] == "superseded"), "{a}");
    assert!(a["context"].as_str().unwrap().contains("1000 EUR"));
    // GET asks the same.
    let g = app.get("/v1/spaces/billing/ask?q=refund+approver&budget=500");
    assert!(g["items"].as_array().unwrap().iter().any(|i| i["id"] == new["id"]), "{g}");
    // APC-23: a budget out of range is refused, not changed in silence.
    for q in ["budget=50", "budget=60000", "budget=lots"] {
        let (s, j) = http.call("GET", &format!("{app_url}/v1/spaces/billing/ask?q=refund&{q}"), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
        assert_eq!(s, 400, "{q}: {j}");
        assert!(j["error"].as_str().unwrap_or("").contains("200 to 50000"), "{j}");
    }
    assert_eq!(app.try_post("/v1/spaces/billing/ask", json!({"question": "Who approves refunds?", "budget": 50})).0, 400);

    // An exchange, distilled by the model into a note.
    let x = app.post("/v1/spaces/billing/exchanges", json!({"user": "Stop sending dunning emails every day; day 1, 4 and 10 is enough.", "assistant": "Changed the schedule to day 1, 4 and 10."}));
    assert_eq!(x["distilling"], true);
    assert_eq!(x["stored"].as_array().unwrap().len(), 2);
    wait_for("the distillation", 60, || app.get("/v1/info")["distill"]["done"] == 1);
    assert_eq!(app.get("/v1/info")["distill"]["notes"], 1);
    assert!(texts(&app.get("/v1/spaces/billing/notes")).iter().any(|t| t.contains("day 1, 4 and 10")));

    // What the inspector reads: the space's overview, its sessions, a session's records.
    let ov = app.get("/v1/spaces/billing/overview");
    assert_eq!(ov["live"]["rule"], 1, "{ov}");
    assert!(ov["rules"].as_array().unwrap().iter().any(|n| n["text"].as_str().unwrap().contains("customer's currency")), "{ov}");
    let changed = ov["changes"].as_array().unwrap();
    assert!(changed.iter().any(|c| c["id"] == new["id"] && c["was"][0]["text"].as_str().unwrap().contains("500 EUR")), "{ov}");
    assert!(!ov["decisions"].as_array().unwrap().iter().any(|n| n["id"] == old["id"]), "a replaced decision is not current: {ov}");
    let ss = app.get("/v1/spaces/billing/sessions");
    let talk = ss["sessions"].as_array().unwrap().iter().find(|s| s["turns"] == 1).unwrap_or_else(|| panic!("{ss}"));
    assert!(talk["title"].as_str().unwrap().starts_with("Stop sending dunning emails"), "{ss}");
    let session = talk["session"].as_str().unwrap();
    let rs = app.get(&format!("/v1/spaces/billing/records?session={session}"));
    let kinds: Vec<&str> = rs["records"].as_array().unwrap().iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"prompt") && kinds.contains(&"answer"), "{rs}");
    let g = app.get("/v1/spaces/billing/graph");
    let gn = g["nodes"].as_array().unwrap();
    assert!(gn.iter().any(|n| n["id"] == new["id"] && n["current"] == true) && gn.iter().any(|n| n["id"] == old["id"] && n["current"] == false), "{g}");
    assert!(gn.iter().any(|n| n["type"] == "entity" && n["label"] == "refunds"), "{g}");
    assert!(g["edges"].as_array().unwrap().iter().any(|e| e["s"] == new["id"] && e["t"] == old["id"] && e["label"] == "supersedes"), "{g}");
    let found = app.get("/v1/spaces/billing/records?q=dunning");
    assert!(!found["records"].as_array().unwrap().is_empty(), "{found}");

    // Incidents: recorded, a better fix replacing one, matched to a text.
    let first = app.post("/v1/incidents", json!({"title": "Search p99 jumped during a large merge", "symptom": "search p99 over 300 ms on one shard", "cause": "a merge of the largest segments took the page cache", "fix": "restarted the shard", "alert": "p99"}));
    let better = app.post("/v1/incidents", json!({"title": "Search p99 jumped during a large merge", "symptom": "search p99 over 300 ms on one shard", "cause": "a merge of the largest segments took the page cache", "fix": "merges throttled while p99 is over its target", "alert": "p99", "replaces": first["id"]}));
    assert_eq!(better["replaced"][0]["fix"], "restarted the shard", "{better}");
    app.post("/v1/incidents", json!({"title": "Writes refused after a replica fell behind", "symptom": "write rejects and a full write queue on shard 2", "cause": "write acks all waited for a slow replica", "fix": "write acks set to majority", "alert": "rejected"}));
    let disk = app.post("/v1/incidents", json!({"title": "Disk filled during a backfill", "symptom": "disk usage over 90% on a shard", "cause": "old segments kept until a snapshot", "fix": "snapshot, then backfill in batches", "alert": "test-disk"}));
    assert_eq!(app.try_post("/v1/incidents", json!({"title": "no details"})).0, 400);
    let list = app.get("/v1/incidents");
    assert_eq!(list["incidents"].as_array().unwrap().len(), 3, "{list}");
    let m = app.get("/v1/incidents/match?q=write+rejects+on+shard+2+queue+full");
    assert_eq!(m["matches"][0]["title"], "Writes refused after a replica fell behind", "{m}");

    // A team's own incidents space, dated by when the incident happened,
    // matched from a selection of log lines; then deleted with its table.
    assert_eq!(app.try_post("/v1/spaces", json!({"name": "netops", "kind": "graph"})).0, 400);
    let team = app.post("/v1/spaces", json!({"name": "netops", "title": "Network incidents", "kind": "incidents"}));
    assert_eq!(team["kind"], "incidents", "{team}");
    let flap = app.post("/v1/incidents", json!({"space": "netops", "title": "Gi0/1 on core-rtr-02 flapped and BGP to 10.0.12.2 dropped", "symptom": "LINK-3-UPDOWN on GigabitEthernet0/1 every few seconds, BGP hold time expired, CRC errors climbing", "cause": "a dirty fibre connector", "fix": "cleaned and reseated the connector", "when": "2026-09-14T03:10:00Z"}));
    assert_eq!((flap["space"].as_str(), flap["ts"].as_i64()), (Some("netops"), Some(1_789_344_000 + 3 * 3600 + 600)), "{flap}");
    assert_eq!(app.try_post("/v1/incidents", json!({"space": "billing", "title": "x", "fix": "y"})).0, 400, "a notes space keeps no incidents");
    assert_eq!(app.try_post("/v1/spaces/billing/notes", json!({"text": "x", "when": "someday"})).0, 400);
    let dated = app.post("/v1/spaces/billing/notes", json!({"kind": "fact", "text": "The September invoice run was late by a day.", "when": "2026-09-30"}));
    assert_eq!(dated["ts"], 1_790_726_400, "{dated}");
    let lines = ["%LINK-3-UPDOWN: Interface GigabitEthernet0/1, changed state to down", "%BGP-5-ADJCHANGE: neighbor 10.0.12.2 Down BGP Notification sent (hold time expired)", "%LINK-3-UPDOWN: Interface GigabitEthernet0/1, changed state to up"];
    let m = app.post("/v1/incidents/match", json!({"lines": lines}));
    assert_eq!((m["matches"][0]["id"].as_str(), m["matches"][0]["space"].as_str()), (flap["id"].as_str(), Some("netops")), "{m}");
    assert!(m["spaces"].as_array().unwrap().iter().any(|s| s == "netops") && m["spaces"].as_array().unwrap().iter().any(|s| s == "incidents"), "{m}");
    let only = app.post("/v1/incidents/match", json!({"lines": lines, "space": "incidents"}));
    assert!(only["matches"].as_array().unwrap().iter().all(|i| i["space"] == "incidents"), "{only}");
    assert_eq!(app.get("/v1/incidents?space=netops")["incidents"].as_array().unwrap().len(), 1);
    assert_eq!(app.get("/v1/incidents")["incidents"].as_array().unwrap().len(), 4, "every incidents space");
    let (s, j) = http.call("DELETE", &format!("{app_url}/v1/spaces/netops"), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
    assert_eq!(s, 200, "{j}");
    assert_eq!(j["table"], "ctx_netops", "{j}");
    let (_, tables) = http.call("GET", &format!("{coord}/v1/tables"), None, &[]);
    assert!(!tables.to_string().contains("\"ctx_netops\""), "the table is dropped: {tables}");
    assert!(app.get("/v1/spaces")["spaces"].as_array().unwrap().iter().all(|s| s["name"] != "netops"));
    let (s, _) = http.call("DELETE", &format!("{app_url}/v1/spaces/netops"), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
    assert_eq!(s, 404);
    // The billing note dated in September stays where it was written.
    let (s, _) = http.call("DELETE", &format!("{app_url}/v1/spaces/billing/notes/{}", dated["id"].as_str().unwrap()), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
    assert_eq!(s, 200);

    // A model drafts an incident from a postmortem; nothing is kept.
    let d = app.post("/v1/incidents/draft", json!({"text": "Postmortem: on Tuesday shard 2 ran out of disk while we re-embedded..."}));
    assert_eq!(d["drafts"][0]["title"], "Shard 2 ran out of disk during a backfill", "{d}");
    assert_eq!(app.get("/v1/incidents")["incidents"].as_array().unwrap().len(), 3);

    // An alert that fires at once (every copy's disk is used at all), and
    // the incident recorded for it first among those like it.
    let (s, rules) = http.call("GET", &format!("{coord}/v1/alerts"), None, &[]);
    assert_eq!(s, 200);
    let mut rules: Vec<Value> = rules["rules"].as_array().unwrap().iter().map(|r| {
        let mut r = r.clone();
        r.as_object_mut().unwrap().remove("status");
        r
    }).collect();
    rules.push(json!({"id": "test-disk", "name": "Disk in use", "metric": "disk_used_ratio", "op": ">=", "threshold": 0.0, "eval": "mean", "window_s": 60, "for_s": 0, "scope": "each_copy", "severity": "warning", "enabled": true, "description": "Fires as soon as a copy reports its disk."}));
    let (s, j) = http.call("PUT", &format!("{coord}/v1/alerts/rules"), Some(&json!({"rules": rules})), &[]);
    assert_eq!(s, 200, "{j}");
    let mut firing = Value::Null;
    wait_for("the test alert to fire", 120, || {
        firing = app.get("/v1/incidents/firing");
        firing["alerts"].as_array().unwrap().iter().any(|a| a["rule"] == "test-disk")
    });
    let a = firing["alerts"].as_array().unwrap().iter().find(|a| a["rule"] == "test-disk").unwrap();
    assert_eq!(a["matches"][0]["id"], disk["id"], "{a}");
    assert_eq!(a["matches"][0]["same_alert"], true, "{a}");
    assert!(a["context"].as_str().unwrap().contains("snapshot, then backfill in batches"), "{a}");
    // Handing it over needs the server's on-call agent, which has no key
    // here: the app says so rather than failing silently.
    let (s, j) = app.try_post("/v1/incidents/handoff", json!({"alert": "test-disk"}));
    assert_ne!(s, 200, "{j}");
    assert!(j["error"].as_str().unwrap_or("").contains("on-call"), "{j}");
    assert_eq!(app.try_post("/v1/incidents/handoff", json!({"alert": "no-such-rule"})).0, 404);

    // An app made from the space: suggested from its live notes, built by
    // the app engine only once confirmed, loaded with the notes picked.
    let ideas = app.post("/v1/spaces/billing/apps/suggest", json!({}));
    let list = ideas["apps"].as_array().unwrap();
    assert_eq!(list.len(), 2, "the one with no title is left out: {ideas}");
    assert_eq!(list[0]["kinds"], json!(["decision", "rule"]), "{ideas}");
    assert_eq!(list[1]["kinds"], json!(["decision"]), "a kind it does not know is dropped: {ideas}");
    let e = app.get("/v1/spaces/billing/apps/estimate");
    assert_eq!((e["usd"].clone(), e["left"].as_f64()), (Value::Null, Some(24.0)), "no build measured yet: {e}");
    let items = app.post("/v1/spaces/billing/apps/items", json!({"kinds": ["decision", "rule"]}));
    let picked: Vec<String> = items["notes"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(picked.len(), 3, "the two live decisions and the rule: {items}");
    let ask = json!({"title": "Refund approvals", "description": list[0]["description"], "kinds": ["decision", "rule"], "exclude": [rule["id"]]});
    let (s, j) = app.try_post("/v1/spaces/billing/apps", ask.clone());
    assert_eq!(s, 409, "not without confirm: {j}");
    assert!(engine.lock().unwrap().descriptions.is_empty(), "nothing built before it is confirmed");
    let mut yes = ask.clone();
    yes["confirm"] = json!(true);
    let built = app.post("/v1/spaces/billing/apps", yes);
    assert_eq!((built["build"]["status"].as_str(), built["build"]["items"].as_u64()), (Some("building"), Some(2)), "{built}");
    let mut b = Value::Null;
    wait_for("the build", 60, || {
        b = app.get("/v1/spaces/billing/apps")["builds"][0].clone();
        b["status"] == "ready" || b["status"] == "failed"
    });
    assert_eq!(b["status"], "ready", "{b}");
    assert_eq!((b["loaded"].as_u64(), b["skipped"].as_u64(), b["cost"].as_f64()), (Some(2), Some(0), Some(0.12)), "{b}");
    {
        let e = engine.lock().unwrap();
        assert!(e.tokens_ok, "the coordinator sends the engine its app token");
        assert!(e.descriptions[0].contains("2 records from the team's memory") && e.descriptions[0].contains("leave out example rows"), "{:?}", e.descriptions);
        let from: Vec<String> = e.rows.iter().map(|r| r["row"]["text"].as_str().unwrap().trim_start_matches("row of ").to_string()).collect();
        assert_eq!(from.len(), 2, "{:?}", e.rows);
        assert!(!from.contains(&rule["id"].as_str().unwrap().to_string()), "the rule left out never reaches the app: {from:?}");
        assert!(e.rows.iter().all(|r| r["table"] == "decisions" && r["row"].get("id").is_none()), "{:?}", e.rows);
    }
    let link = app.post(&format!("/v1/spaces/billing/apps/{}/open", b["id"].as_str().unwrap()), json!({}));
    assert!(link["url"].as_str().unwrap().ends_with("next=#/app/a1"), "{link}");
    assert_eq!(app.try_post("/v1/spaces/billing/apps/nobuild/open", json!({})).0, 404);

    // Through the coordinator, as the command center calls it.
    let (s, j) = http.call("GET", &format!("{coord}/v1/apps/chloectx/v1/spaces/billing/ask?q=refund+approver"), None, &[]);
    assert_eq!(s, 200, "{j}");
    assert!(j["items"].as_array().unwrap().iter().any(|i| i["id"] == new["id"]));

    // The app keeps nothing itself: after a restart, the same spaces, and
    // new records numbered after the old ones.
    drop(std::mem::replace(&mut app_proc, Proc(Command::new("true").spawn().unwrap())));
    let _app_proc = start_app(dir.path(), &coord, aport, &model);
    wait_for("the app again", 60, || http.call("GET", &format!("{app_url}/v1/info"), None, &[("authorization", &format!("Bearer {TOKEN}"))]).0 == 200);
    let spaces = app.get("/v1/spaces");
    let names: Vec<&str> = spaces["spaces"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["billing", "incidents"], "{spaces}");
    let again = app.post("/v1/spaces/billing/notes", json!({"kind": "fact", "text": "Chargebacks arrive as webhooks and are matched by the invoice number."}));
    let all = app.get("/v1/spaces/billing/notes?all=1");
    assert_eq!(all["notes"].as_array().unwrap().iter().filter(|n| n["id"] == again["id"]).count(), 1, "a new id: {again}");
    assert_eq!(texts(&all).len(), 8, "the agent's retired fact and the dated note too: {all}");
    drop(procs);
}

/// With `--local-stores`, the app reads this machine's Claude Code stores
/// for the command center's Receipts and Blame pages: the stores, a store's
/// receipts, one replayed, blame, and a snapshot kept and dropped by name.
#[test]
fn local_stores_through_the_app() {
    use chloectx::daemon::Daemon;
    let Some(bin) = server_bin() else {
        assert!(std::env::var_os("CHLOECTX_NEED_SERVER").is_none(), "CHLOECTX_NEED_SERVER is set but there is no chloedb-server binary");
        eprintln!("skipped: no chloedb-server binary (set CHLOEDB_SERVER_BIN)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    let project = proj.canonicalize().unwrap().to_string_lossy().into_owned();
    let store = home.join("projects").join("proj-test");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("config.json"), r#"{"embedder": "hash:64", "sync": false, "distill": {"enabled": false}}"#).unwrap();
    std::fs::write(store.join("project.txt"), format!("{project}\n")).unwrap();
    {
        let d = Arc::new(Daemon::open(&store, &project).unwrap());
        let call = |req: Value| {
            let (r, f) = d.handle(&req);
            if let Some(f) = f {
                f();
            }
            assert!(r.get("error").is_none(), "{r}");
            r
        };
        let hook = |e: &str, p: Value| call(json!({"op": "hook", "event": e, "payload": p}));
        hook("SessionStart", json!({"session_id": "s1", "source": "startup"}));
        hook("UserPromptSubmit", json!({"session_id": "s1", "prompt": "How should payment retries back off?"}));
        call(json!({"op": "remember", "kind": "decision", "text": "Payment retries back off with a fixed 2 second delay between attempts."}));
        hook("Stop", json!({"session_id": "s1", "last_assistant_message": "Agreed: a fixed 2 second backoff."}));
        hook("SessionStart", json!({"session_id": "s2", "source": "startup"}));
        hook("UserPromptSubmit", json!({"session_id": "s2", "prompt": "Change this: payment retries back off with a fixed 2 second delay."}));
        hook("PostToolUse", json!({"session_id": "s2", "tool_name": "Edit", "tool_input": {"file_path": format!("{project}/src/retry.rs"), "old_string": "a", "new_string": "sleep(backoff_delay(attempt));"}, "tool_response": {}}));
        std::fs::write(proj.join("src/retry.rs"), "fn retry() {\n    sleep(backoff_delay(attempt));\n}\n").unwrap();
    }

    let srv_port = free_port();
    let srv = format!("http://127.0.0.1:{srv_port}");
    let _server = spawn(Command::new(&bin).args(["--db", &dir.path().join("s.strata").to_string_lossy(), "--listen", &format!("127.0.0.1:{srv_port}"), "--grpc", "off", "--no-fetch-models"]));
    let http = Http::new();
    wait_for("the server", 30, || http.call("GET", &format!("{srv}/v1/health"), None, &[]).0 == 200);
    let aport = free_port();
    let app_url = format!("http://127.0.0.1:{aport}");
    let _app = spawn(
        Command::new(env!("CARGO_BIN_EXE_chloectx"))
            .args(["app", "--server", &srv, "--port", &aport.to_string(), "--embedder", "hash:256", "--token", TOKEN, "--local-stores"])
            .env("CHLOECTX_HOME", &home)
            .env_remove("ANTHROPIC_API_KEY"),
    );
    let app = App { http: &http, url: app_url.clone() };
    wait_for("the app", 60, || http.call("GET", &format!("{app_url}/v1/info"), None, &[("authorization", &format!("Bearer {TOKEN}"))]).0 == 200);
    assert_eq!(app.get("/v1/info")["local"], true);

    let stores = app.get("/v1/local");
    let st = stores["stores"].as_array().unwrap();
    assert_eq!(st.len(), 1, "{stores}");
    assert_eq!(st[0]["id"], "proj-test");
    assert_eq!(st[0]["project"], json!(project));

    let rs = app.get("/v1/local/proj-test/receipts?session=s2");
    let list = rs["receipts"].as_array().unwrap();
    let brief = list.iter().find(|r| r["source"] == "briefing").unwrap_or_else(|| panic!("{rs}"));
    let one = app.get(&format!("/v1/local/proj-test/receipts/{}", brief["id"].as_str().unwrap()));
    assert!(one["receipt"]["text"].as_str().unwrap().contains("fixed 2 second"), "{one}");
    assert_eq!(one["replays"][0]["on"], "kept snapshot", "{one}");
    assert_eq!(one["replays"][0]["verdict"]["identical"], true, "{one}");
    let (s, _) = http.call("GET", &format!("{app_url}/v1/local/proj-test/receipts/rc-nothere"), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
    assert_eq!(s, 404);
    let (s, _) = http.call("GET", &format!("{app_url}/v1/local/elsewhere/receipts"), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
    assert_eq!(s, 404);

    let b = app.get("/v1/local/proj-test/blame?target=src%2Fretry.rs%3A2");
    assert_eq!(b["edit"]["session"], "s2", "{b}");
    assert!(b["receipts"].as_array().unwrap().iter().any(|r| r["source"] == "briefing"), "{b}");

    // What the inspector reads of a store: its overview, sessions, records, notes and an answer.
    let ov = app.get("/v1/local/proj-test/overview");
    assert_eq!(ov["live"]["decision"], 1, "{ov}");
    assert!(ov["decisions"][0]["text"].as_str().unwrap().contains("fixed 2 second"), "{ov}");
    let ss = app.get("/v1/local/proj-test/sessions");
    let titles: Vec<&str> = ss["sessions"].as_array().unwrap().iter().filter_map(|s| s["title"].as_str()).collect();
    assert!(titles.contains(&"How should payment retries back off?"), "{ss}");
    let rs = app.get("/v1/local/proj-test/records?session=s2");
    assert!(rs["records"].as_array().unwrap().iter().any(|r| r["kind"] == "tool_call"), "{rs}");
    let notes = app.get("/v1/local/proj-test/notes");
    let id = notes["notes"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(app.get(&format!("/v1/local/proj-test/notes/{id}"))["note"]["id"], json!(id));
    let a = app.post("/v1/local/proj-test/ask", json!({"question": "how do payment retries back off?", "budget": 1000}));
    assert!(a["items"].as_array().unwrap().iter().any(|i| i["id"] == json!(id)), "{a}");
    let g = app.get("/v1/local/proj-test/graph");
    assert!(g["nodes"].as_array().unwrap().iter().any(|n| n["id"] == json!(id) && n["type"] == "note"), "{g}");
    // A capsule cut from it, then listed.
    let cut = app.post("/v1/local/proj-test/capsules", json!({"title": "Retries"}));
    assert!(cut["records"].as_i64().unwrap() >= 1 && cut["title"] == "Retries", "{cut}");
    let caps = app.get("/v1/local/proj-test/capsules");
    assert_eq!(caps["capsules"].as_array().unwrap().len(), 1, "{caps}");

    let k = app.post("/v1/local/proj-test/snapshots", json!({"name": "before-release"}));
    assert!(k["text"].as_str().unwrap().starts_with("kept before-release"), "{k}");
    assert_eq!(app.try_post("/v1/local/proj-test/snapshots", json!({"name": "../x"})).0, 400);
    let l = app.get("/v1/local/proj-test/snapshots");
    assert!(l["snapshots"].as_array().unwrap().iter().any(|k| k["name"] == "before-release"), "{l}");
    let (s, _) = http.call("DELETE", &format!("{app_url}/v1/local/proj-test/snapshots/before-release"), None, &[("authorization", &format!("Bearer {TOKEN}"))]);
    assert_eq!(s, 200);
    // The daemon the app started for the store.
    let _ = chloectx::daemon::request(&store, &project, &json!({"op": "shutdown"}), Duration::from_secs(10));
}
