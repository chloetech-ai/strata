//! Connectors (`crate::connectors`): configs kept as rows of a table,
//! pulls of Prometheus, Jira, ServiceNow and Oxidized (stood in for by a
//! local server that answers as they do) mapped into tables of the
//! network operations app's shape, pushes of an email and of
//! Alertmanager's webhook written with the connector's own key, which
//! may write its target and nothing else, health that turns to failing
//! and back, secrets that never reach a table or an answer, and the
//! schedule running a pull by itself.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Query;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json as AxJson, Router};
use chloedb::db::Db;
use chloedb::storage::DbOptions;
use chloedb_server::auth::{Access, Secret};
use chloedb_server::server;
use chloedb_server::service::{AgentConfig, Backend, Service};
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use tokio::net::TcpListener;

const ROOT: &str = "root-token";

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: chloedb_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

async fn start(dir: &std::path::Path, schedule: bool) -> (String, Arc<Service>) {
    let db = Db::create_empty(&dir.join("c.strata"), DbOptions::default()).unwrap();
    let schema = (*db.schema()).clone();
    let svc = Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(20)));
    svc.connectors.keep_secrets(dir.join("c.strata.connectors.json")).unwrap();
    if schedule {
        svc.connectors.enable();
    }
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(svc.clone(), l, Access { token: Some(Secret::new(ROOT)), ..Default::default() }));
    (url, svc)
}

async fn call(c: &reqwest::Client, method: reqwest::Method, url: &str, key: &str, body: Option<Json>) -> (u16, Json) {
    let mut r = c.request(method, url).bearer_auth(key);
    if let Some(b) = body {
        r = r.json(&b);
    }
    let res = r.send().await.unwrap();
    let status = res.status().as_u16();
    let text = res.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Json::String(text)))
}

async fn post(c: &reqwest::Client, url: &str, key: &str, body: Json) -> (u16, Json) {
    call(c, reqwest::Method::POST, url, key, Some(body)).await
}

async fn sql(c: &reqwest::Client, base: &str, q: &str) -> Vec<Json> {
    let (s, j) = post(c, &format!("{base}/v1/sql"), ROOT, json!({"sql": q, "objects": true})).await;
    assert_eq!(s, 200, "{q}: {j}");
    j["rows"].as_array().cloned().unwrap_or_default()
}

/// What the stand-in upstream saw: each request's path, query and
/// Authorization.
type Seen = Arc<Mutex<Vec<(String, HashMap<String, String>, Option<String>)>>>;

/// A server answering as Prometheus, Jira Cloud, ServiceNow and
/// oxidized-web do, for the requests the connectors make.
async fn upstream(fail: Arc<std::sync::atomic::AtomicBool>) -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let note = |seen: &Seen, path: &str, q: &HashMap<String, String>, h: &HeaderMap| {
        seen.lock().unwrap().push((path.to_string(), q.clone(), h.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string)));
    };
    let s1 = seen.clone();
    let s2 = seen.clone();
    let s3 = seen.clone();
    let s4 = seen.clone();
    let s5 = seen.clone();
    let f1 = fail.clone();
    let app = Router::new()
        .route(
            "/api/v1/query_range",
            get(move |Query(q): Query<HashMap<String, String>>, h: HeaderMap| {
                let seen = s1.clone();
                let fail = f1.clone();
                async move {
                    note(&seen, "/api/v1/query_range", &q, &h);
                    if fail.load(std::sync::atomic::Ordering::SeqCst) {
                        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, AxJson(json!({"status": "error", "error": "down for the test"})));
                    }
                    let start: u64 = q["start"].parse().unwrap();
                    let end: u64 = q["end"].parse().unwrap();
                    let step: u64 = q["step"].parse().unwrap();
                    let base = if q["query"].contains("cpu") { 0.5 } else { 0.25 };
                    let series = |dev: &str, k: f64| {
                        let values: Vec<Json> = (0..).map(|i| start + i * step).take_while(|t| *t <= end).map(|t| json!([t as f64, format!("{}", base * k)])).collect();
                        json!({"metric": {"__name__": "x", "instance": format!("{dev}:9100"), "device": dev}, "values": values})
                    };
                    (axum::http::StatusCode::OK, AxJson(json!({"status": "success", "data": {"resultType": "matrix", "result": [series("r1", 1.0), series("r2", 2.0)]}})))
                }
            }),
        )
        .route(
            "/rest/api/3/search/jql",
            get(move |Query(q): Query<HashMap<String, String>>, h: HeaderMap| {
                let seen = s2.clone();
                async move {
                    note(&seen, "/rest/api/3/search/jql", &q, &h);
                    let issue = |k: &str, status: &str| json!({"key": k, "id": "1", "fields": {"summary": format!("Change {k}"), "status": {"name": status, "statusCategory": {"name": "x"}}, "issuetype": {"name": "Change"}, "project": {"key": "NET"}, "assignee": {"displayName": "Ana"}, "labels": ["core"], "created": "2026-11-12T10:00:00.000+0000", "updated": "2026-11-13T02:00:00.000+0000", "description": {"type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Reload r1"}]}]}, "customfield_1": "2026-11-20T06:00:00.000+0000"}});
                    match q.get("nextPageToken").map(String::as_str) {
                        None => AxJson(json!({"issues": [issue("NET-1", "To Do")], "nextPageToken": "p2", "isLast": false})),
                        _ => AxJson(json!({"issues": [issue("NET-2", "In Progress")], "isLast": true})),
                    }
                }
            }),
        )
        .route(
            "/api/now/table/change_request",
            get(move |Query(q): Query<HashMap<String, String>>, h: HeaderMap| {
                let seen = s3.clone();
                async move {
                    note(&seen, "/api/now/table/change_request", &q, &h);
                    let f = |v: &str, d: &str| json!({"value": v, "display_value": d});
                    AxJson(json!({"result": [{
                        "number": f("CHG0030001", "CHG0030001"), "short_description": f("Replace optic on r2", "Replace optic on r2"),
                        "state": f("-2", "Scheduled"), "type": f("normal", "Normal"), "risk": f("3", "Moderate"),
                        "start_date": f("2026-11-20 06:00:00", "11/20/2026 01:00:00 AM"), "end_date": f("2026-11-20 08:00:00", "11/20/2026 03:00:00 AM"),
                        "assigned_to": f("6816f79c", "Beth Anglin"), "cmdb_ci": f("a1b2", "r2"), "sys_updated_on": f("2026-11-13 02:30:00", "11/12/2026 09:30:00 PM")
                    }]}))
                }
            }),
        )
        .route(
            "/nodes.json",
            get(move |h: HeaderMap| {
                let seen = s4.clone();
                async move {
                    note(&seen, "/nodes.json", &HashMap::new(), &h);
                    AxJson(json!([{"name": "r1", "ip": "10.0.0.1", "model": "junos", "group": "core", "mtime": "2026-11-13 02:00:00 UTC", "last": {"status": "success"}}]))
                }
            }),
        )
        .route(
            "/node/fetch/core/r1",
            get(move |h: HeaderMap| {
                let seen = s5.clone();
                async move {
                    note(&seen, "/node/fetch/core/r1", &HashMap::new(), &h);
                    "system {\n  host-name r1;\n}\ninterfaces {\n  et-0/0/0 { description to-r2; }\n}\n"
                }
            }),
        );
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

const NOTICE: &str = "From: Example Transit NOC <noc@ops.example-transit.net>\r\nSubject: Planned maintenance MAINT-42\r\nDate: Fri, 13 Nov 2026 03:00:00 +0000\r\nMessage-ID: <abc123@ops.example-transit.net>\r\nContent-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: text/plain\r\n\r\nFiber work on circuit EXT-1001.\r\n--b1\r\nContent-Type: text/calendar\r\n\r\nBEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:20261120T060000Z\r\nDTEND:20261120T080000Z\r\nX-MAINTNOTE-PROVIDER:example-transit\r\nX-MAINTNOTE-MAINTENANCE-ID:MAINT-42\r\nX-MAINTNOTE-OBJECT-ID:EXT-1001\r\nX-MAINTNOTE-IMPACT:OUTAGE\r\nX-MAINTNOTE-STATUS:CONFIRMED\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b1--\r\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connectors_pull_and_take_pushes_into_tables_with_keys_of_their_own() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _svc) = start(dir.path(), false).await;
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (up, seen) = upstream(fail.clone()).await;
    let c = reqwest::Client::new();
    let admin = |p: &str| format!("{base}/v1/admin/connectors{p}");

    // The network operations app's tables, as it makes them.
    for ddl in [
        "CREATE TABLE netops_device_metrics (id KEYWORD PRIMARY KEY, ts TIMESTAMP, device KEYWORD, cpu FLOAT, mem FLOAT, up INT)",
        "CREATE TABLE netops_changes (id KEYWORD PRIMARY KEY, title TEXT, kind KEYWORD, risk KEYWORD, status KEYWORD, window_start TIMESTAMP, window_end TIMESTAMP, owner KEYWORD, devices KEYWORD[])",
        "CREATE TABLE netops_configs (id KEYWORD PRIMARY KEY, device KEYWORD, version INT, ts TIMESTAMP, author KEYWORD, lines INT, body TEXT)",
        "CREATE TABLE netops_maintenance (id KEYWORD PRIMARY KEY, provider KEYWORD, reference KEYWORD, circuit KEYWORD, window_start TIMESTAMP, window_end TIMESTAMP, impact KEYWORD, status KEYWORD, received TIMESTAMP, subject TEXT, raw TEXT)",
    ] {
        let (s, j) = post(&c, &format!("{base}/v1/sql"), ROOT, json!({ "sql": ddl })).await;
        assert_eq!(s, 200, "{ddl}: {j}");
    }

    // Prometheus: two queries joined by device into one row a minute.
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "prom", "kind": "prometheus", "target": "netops_device_metrics", "every_s": 60,
        "config": {"allow_internal": true, "url": up, "queries": {"cpu": "avg by (device) (rate(cpu[1m]))", "mem": "mem_used_ratio"}, "key": ["device"], "backfill_s": 600},
        "secrets": {"token": "prom-secret-1"}})).await;
    assert_eq!(s, 200, "{j}");
    assert!(j.get("push_secret").is_none(), "a pull connector has no push key: {j}");
    let (s, r) = post(&c, &admin("/prom/run"), ROOT, json!({})).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["ok"], true, "{r}");
    assert!(r["rows"].as_u64().unwrap() >= 20, "ten minutes of two devices: {r}");
    assert_eq!(r["dropped"], json!(["labels", "subject", "values"]), "the columns the table lacks are named: {r}");
    let rows = sql(&c, &base, "SELECT device, cpu, mem FROM netops_device_metrics WHERE device = 'r2' LIMIT 1").await;
    assert_eq!(rows[0]["cpu"], 1.0);
    assert_eq!(rows[0]["mem"], 0.5);
    let prom_auth = seen.lock().unwrap().iter().find(|(p, _, _)| p == "/api/v1/query_range").unwrap().2.clone();
    assert_eq!(prom_auth.as_deref(), Some("Bearer prom-secret-1"));
    // A second run reads from where the first stopped: nothing new yet.
    let (_, r2) = post(&c, &admin("/prom/run"), ROOT, json!({})).await;
    assert_eq!((r2["ok"].clone(), r2["rows"].clone()), (json!(true), json!(0)), "{r2}");

    // Jira Cloud, two pages, into the connector's own table.
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "jira-net", "kind": "jira", "config": {"allow_internal": true, "url": up, "api": "cloud", "jql": "project = NET", "username": "bot@example.com", "fields": {"window_start": "customfield_1"}}, "secrets": {"token": "jira-api-token"}})).await;
    assert_eq!(s, 200, "{j}");
    assert_eq!(j["made_target"], true);
    let (_, r) = post(&c, &admin("/jira-net/run"), ROOT, json!({})).await;
    assert_eq!((r["ok"].clone(), r["rows"].clone()), (json!(true), json!(2)), "{r}");
    let rows = sql(&c, &base, "SELECT id, title, status, description, updated FROM jira_issues ORDER BY id").await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["status"], "In Progress");
    assert_eq!(rows[0]["description"], "Reload r1");
    let jql = seen.lock().unwrap().iter().find(|(p, _, _)| p == "/rest/api/3/search/jql").map(|(_, q, a)| (q["jql"].clone(), a.clone())).unwrap();
    assert!(jql.0.starts_with("(project = NET) AND updated >= \"-") && jql.0.ends_with("ORDER BY updated ASC"), "{}", jql.0);
    assert!(jql.1.unwrap().starts_with("Basic "), "Jira Cloud signs in with the account and its API token");

    // ServiceNow changes into the netops app's table, by the preset map.
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "snow", "kind": "servicenow", "target": "netops_changes", "config": {"allow_internal": true, "instance": up, "table": "change_request", "username": "svc"}, "secrets": {"password": "sn-pass"}})).await;
    assert_eq!(s, 200, "{j}");
    let (_, r) = post(&c, &admin("/snow/run"), ROOT, json!({})).await;
    assert_eq!(r["ok"], true, "{r}");
    let rows = sql(&c, &base, "SELECT * FROM netops_changes").await;
    assert_eq!(rows[0]["id"], "CHG0030001");
    assert_eq!(rows[0]["status"], "approved", "Scheduled is approved in the app's words");
    assert_eq!(rows[0]["window_start"], "2026-11-20T06:00:00Z", "ServiceNow's value, UTC");
    assert_eq!(rows[0]["devices"], json!(["r2"]));
    assert_eq!(rows[0]["owner"], "Beth Anglin");
    let q = seen.lock().unwrap().iter().find(|(p, _, _)| p == "/api/now/table/change_request").map(|(_, q, _)| q["sysparm_query"].clone()).unwrap();
    assert!(q.contains("sys_updated_on>=javascript:gs.minutesAgoStart("), "{q}");

    // Oxidized: the changed node's config, a version.
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "ox", "kind": "oxidized", "target": "netops_configs", "config": {"allow_internal": true, "url": up}})).await;
    assert_eq!(s, 200, "{j}");
    let ox_secret = j["push_secret"].as_str().expect("oxidized takes pushes from its hook: it has a key").to_string();
    let (_, r) = post(&c, &admin("/ox/run"), ROOT, json!({})).await;
    assert_eq!((r["ok"].clone(), r["rows"].clone()), (json!(true), json!(1)), "{r}");
    let (_, r) = post(&c, &admin("/ox/run"), ROOT, json!({})).await;
    assert_eq!(r["rows"], 0, "an unchanged node is not fetched again: {r}");
    let rows = sql(&c, &base, "SELECT device, lines, author, body FROM netops_configs").await;
    assert_eq!((rows[0]["device"].clone(), rows[0]["lines"].clone(), rows[0]["author"].clone()), (json!("r1"), json!(6), json!("oxidized")));
    // Its hook's push fetches the node at once, with the connector's key.
    let (s, r) = post(&c, &format!("{base}/v1/connectors/ox/push"), &ox_secret, json!({"node": "r1", "group": "core"})).await;
    assert_eq!(s, 200, "{r}");

    // Mail: a carrier's notice, pushed raw with the connector's key.
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "carriers", "kind": "mail", "target": "netops_maintenance"})).await;
    assert_eq!(s, 200, "{j}");
    let mail_key = j["push_secret"].as_str().unwrap().to_string();
    let res = c.post(format!("{base}/v1/connectors/carriers/push")).bearer_auth(&mail_key).header("content-type", "message/rfc822").body(NOTICE).send().await.unwrap();
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    let rows = sql(&c, &base, "SELECT * FROM netops_maintenance").await;
    assert_eq!(rows[0]["id"], "example-transit-MAINT-42");
    assert_eq!(rows[0]["circuit"], "EXT-1001");
    assert_eq!(rows[0]["window_start"], "2026-11-20T06:00:00Z");
    assert_eq!((rows[0]["impact"].clone(), rows[0]["status"].clone()), (json!("outage"), json!("confirmed")));
    // The same notice again is the same row.
    let res = c.post(format!("{base}/v1/connectors/carriers/push")).bearer_auth(&mail_key).body(NOTICE).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(sql(&c, &base, "SELECT id FROM netops_maintenance").await.len(), 1);

    // A push key writes its target alone: not another connector's, not a
    // query, not the connectors' own tables.
    let (s, e) = post(&c, &format!("{base}/v1/connectors/ox/push"), &mail_key, json!({"node": "r1", "text": "x"})).await;
    assert_eq!(s, 403, "the mail key may not write netops_configs: {e}");
    let (s, _) = post(&c, &format!("{base}/v1/sql"), &mail_key, json!({"sql": "SELECT * FROM netops_changes"})).await;
    assert_eq!(s, 403);
    let (s, _) = post(&c, &format!("{base}/v1/sql"), &mail_key, json!({"sql": "SELECT * FROM chloedb_connectors"})).await;
    assert_eq!(s, 403);
    let (s, _) = call(&c, reqwest::Method::GET, &admin(""), &mail_key, None).await;
    assert_eq!(s, 403, "a push key is not an admin's");
    let (s, _) = post(&c, &format!("{base}/v1/connectors/carriers/push"), "sk_wrong_key", json!({"raw": "x"})).await;
    assert_eq!(s, 401);

    // Alertmanager's webhook, by its preset, into its own table.
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "am", "kind": "webhook", "config": {"preset": "alertmanager"}})).await;
    assert_eq!(s, 200, "{j}");
    let am_key = j["push_secret"].as_str().unwrap().to_string();
    let hook = json!({"version": "4", "status": "firing", "alerts": [
        {"status": "firing", "labels": {"alertname": "BGPDown", "severity": "critical", "instance": "r1"}, "annotations": {"summary": "BGP session to r2 down"}, "startsAt": "2026-11-13T03:00:00Z", "fingerprint": "f1"},
        {"status": "firing", "labels": {"alertname": "HighCPU", "severity": "warning", "instance": "r2"}, "annotations": {"summary": "CPU over 90%"}, "startsAt": "2026-11-13T03:01:00Z", "fingerprint": "f2"}]});
    let (s, r) = post(&c, &format!("{base}/v1/connectors/am/push"), &am_key, hook).await;
    assert_eq!((s, r["rows"].clone()), (200, json!(2)), "{r}");
    let rows = sql(&c, &base, "SELECT id, text FROM alertmanager_alerts WHERE MATCH(text, 'bgp') LIMIT 5").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "f1-2026-11-13T03:00:00Z");
    // A push key reads nothing, not even the table it writes: one leaked
    // from the sending system reads no alert.
    for (path, body) in [("/v1/query", json!({"sql": "SELECT COUNT(*) FROM alertmanager_alerts"})), ("/v1/sql", json!({"sql": "SELECT * FROM alertmanager_alerts"})), ("/v1/kv/get", json!({"table": "alertmanager_alerts", "keys": ["f1-2026-11-13T03:00:00Z"]}))] {
        let (s, e) = post(&c, &format!("{base}{path}"), &am_key, body).await;
        assert_eq!(s, 403, "{path}: {e}");
        assert_eq!(e["error_code"], "scope_refused", "{e}");
    }
    for path in ["/v1/tables", "/v1/schema?table=alertmanager_alerts", "/v1/export?table=alertmanager_alerts"] {
        let (s, e) = call(&c, reqwest::Method::GET, &format!("{base}{path}"), &am_key, None).await;
        assert_eq!(s, 403, "{path}: {e}");
    }
    let (s, _) = call(&c, reqwest::Method::GET, &format!("{base}/v1/whoami"), &am_key, None).await;
    assert_eq!(s, 200, "a push key may ask who it is");

    // A dry run shows the rows a config would make, writing nothing.
    let (s, t) = post(&c, &admin("/test"), ROOT, json!({"kind": "webhook", "target": "alertmanager_alerts", "config": {"records": "$.items", "map": {"id": "{n}", "text": {"path": "$.msg", "as": "upper"}, "nope": "$.n"}}, "sample": {"items": [{"n": "a", "msg": "hello"}]}})).await;
    assert_eq!(s, 200, "{t}");
    assert_eq!(t["rows"][0], json!({"id": "a", "text": "HELLO"}));
    assert_eq!(t["dropped"], json!(["nope"]));
    assert_eq!(sql(&c, &base, "SELECT id FROM alertmanager_alerts").await.len(), 2);

    // Health: ok; failing while Prometheus is down, with its error; ok again.
    let (_, l) = call(&c, reqwest::Method::GET, &admin(""), ROOT, None).await;
    let health = |l: &Json, n: &str| l["connectors"].as_array().unwrap().iter().find(|x| x["name"] == n).unwrap()["health"].clone();
    assert_eq!(health(&l, "prom"), "ok", "{l}");
    assert_eq!(health(&l, "carriers"), "ok");
    assert!(l["kinds"].as_array().unwrap().len() >= 7);
    fail.store(true, std::sync::atomic::Ordering::SeqCst);
    // Far enough on that a minute is new.
    let (_, r) = post(&c, &admin("/prom"), ROOT, json!({"config": {"allow_internal": true, "url": up, "queries": {"cpu": "cpu"}, "key": ["device"], "lag_s": 0, "step_s": 10}})).await;
    assert!(r["connector"]["name"] == "prom", "{r}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (_, r) = post(&c, &admin("/prom/run"), ROOT, json!({})).await;
    assert_eq!(r["ok"], false, "{r}");
    assert!(r["error"].as_str().unwrap().contains("503"), "{r}");
    let (_, one) = call(&c, reqwest::Method::GET, &admin("/prom"), ROOT, None).await;
    assert_eq!(one["health"], "failing", "{one}");
    assert_eq!(one["state"]["failures"], 1);
    assert!(one["runs"].as_array().unwrap().len() >= 3, "each run is a row: {one}");
    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    let (_, r) = post(&c, &admin("/prom/run"), ROOT, json!({})).await;
    assert_eq!(r["ok"], true, "{r}");

    // Secrets: names in answers, values nowhere but the private file.
    let (_, l) = call(&c, reqwest::Method::GET, &admin(""), ROOT, None).await;
    let text = l.to_string();
    for secret in ["prom-secret-1", "jira-api-token", "sn-pass"] {
        assert!(!text.contains(secret), "{secret} in the list");
    }
    let jira = l["connectors"].as_array().unwrap().iter().find(|x| x["name"] == "jira-net").unwrap();
    assert_eq!(jira["secrets"], json!(["token"]));
    let configs = sql(&c, &base, "SELECT * FROM chloedb_connectors").await;
    assert_eq!(configs.len(), 6);
    assert!(!serde_json::to_string(&configs).unwrap().contains("jira-api-token"));
    let file = dir.path().join("c.strata.connectors.json");
    assert!(std::fs::read_to_string(&file).unwrap().contains("jira-api-token"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }

    // A connector of the same name is refused; a deleted one's key ends.
    let (s, _) = post(&c, &admin(""), ROOT, json!({"name": "am", "kind": "webhook"})).await;
    assert_eq!(s, 409);
    let (s, _) = call(&c, reqwest::Method::DELETE, &admin("/am"), ROOT, None).await;
    assert_eq!(s, 200);
    let (s, _) = post(&c, &format!("{base}/v1/connectors/am/push"), &am_key, json!({"alerts": []})).await;
    assert_eq!(s, 401, "a deleted connector's key is revoked");
    // A rotated key replaces the old.
    let (s, k) = post(&c, &admin("/carriers/key"), ROOT, json!({})).await;
    assert_eq!(s, 200, "{k}");
    let res = c.post(format!("{base}/v1/connectors/carriers/push")).bearer_auth(&mail_key).body(NOTICE).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let res = c.post(format!("{base}/v1/connectors/carriers/push")).bearer_auth(k["push_secret"].as_str().unwrap()).body(NOTICE).send().await.unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_schedule_runs_a_pull_by_itself_and_skips_rows_no_key_could_write() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _svc) = start(dir.path(), true).await;
    let (up, _seen) = upstream(Arc::new(std::sync::atomic::AtomicBool::new(false))).await;
    let c = reqwest::Client::new();
    let (s, j) = post(&c, &format!("{base}/v1/admin/connectors"), ROOT, json!({"name": "prom", "kind": "prometheus", "every_s": 10, "config": {"allow_internal": true, "url": up, "queries": {"cpu": "cpu"}, "key": ["device"], "backfill_s": 300}})).await;
    assert_eq!(s, 200, "{j}");
    assert_eq!(j["connector"]["updated_by"], "token", "who changed it: {j}");
    let mut rows = 0;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        rows = sql(&c, &base, "SELECT COUNT(*) AS n FROM prometheus_rollups").await.first().and_then(|r| r["n"].as_u64()).unwrap_or(0);
        if rows > 0 {
            break;
        }
    }
    assert!(rows >= 10, "the schedule ran the pull: {rows} rows");
    let (_, l) = call(&c, reqwest::Method::GET, &format!("{base}/v1/admin/connectors"), ROOT, None).await;
    assert_eq!(l["scheduler"]["on"], true, "{l}");
    let p = &l["connectors"][0];
    assert_eq!(p["runs"][0]["trigger"], "schedule", "{p}");

    // No key writes the connectors' tables, an admin's neither: a row
    // would pick where the connector's credentials go (audit 4, X-N1).
    let mut keys = Vec::new();
    for scope in ["write", "admin"] {
        let (s, k) = post(&c, &format!("{base}/v1/admin/keys"), ROOT, json!({"name": format!("k-{scope}"), "scope": scope})).await;
        assert_eq!(s, 200, "{k}");
        keys.push(k["secret"].as_str().unwrap().to_string());
    }
    for key in &keys {
        for q in [
            "UPDATE chloedb_connectors SET config = '{\"url\":\"https://attacker.example\"}' WHERE id = 'prom'",
            "INSERT INTO chloedb_connectors (id, kind, target, enabled, every_s, config) VALUES ('m', 'http_json', 'loot', true, 10, '{}')",
            "DELETE FROM chloedb_connector_runs WHERE ok = false",
            "DROP TABLE chloedb_connector_state",
        ] {
            let (s, j) = post(&c, &format!("{base}/v1/sql"), key, json!({ "sql": q })).await;
            assert_eq!(s, 403, "{q}: {j}");
            if key == &keys[1] {
                assert!(j["error"].as_str().unwrap().contains("POST /v1/admin/connectors"), "it says the way: {j}");
            }
        }
        let (s, j) = post(&c, &format!("{base}/v1/write"), key, json!({"ops": [{"op": "upsert", "table": "chloedb_connectors", "doc": {"id": "prom", "kind": "prometheus", "target": "*"}}]})).await;
        assert_eq!(s, 403, "{j}");
        let (s, j) = call(&c, reqwest::Method::PUT, &format!("{base}/v1/kv/chloedb_connectors/prom"), key, Some(json!({"target": "*"}))).await;
        assert_eq!(s, 403, "{j}");
    }
    // A table of that name made before the connectors' own is refused too.
    let (s, j) = post(&c, &format!("{base}/v1/sql"), &keys[1], json!({"sql": "CREATE TABLE chloedb_connectorsx (id KEYWORD PRIMARY KEY)"})).await;
    assert_eq!(s, 403, "{j}");
    // Reading them is a read key's as before.
    assert_eq!(sql(&c, &base, "SELECT id FROM chloedb_connectors").await.len(), 1);

    // A row written past the API (here with the root token) is checked
    // again where it is read: one that writes every table on a second's
    // schedule is shown as invalid and never runs.
    let (s, j) = post(&c, &format!("{base}/v1/sql"), ROOT, json!({"sql": "INSERT INTO chloedb_connectors (id, kind, target, enabled, every_s, config) VALUES ('m', 'http_json', '*', true, 1, '{\"url\":\"http://169.254.169.254/latest/meta-data/\",\"records\":\"$\"}')"})).await;
    assert_eq!(s, 200, "{j}");
    let (_, one) = call(&c, reqwest::Method::GET, &format!("{base}/v1/admin/connectors/m"), ROOT, None).await;
    assert_eq!(one["health"], "invalid", "{one}");
    assert!(one["invalid"].as_str().unwrap().contains("not a table a connector may write"), "{one}");
    let (s, r) = post(&c, &format!("{base}/v1/admin/connectors/m/run"), ROOT, json!({})).await;
    assert_eq!(s, 409, "{r}");
    tokio::time::sleep(Duration::from_millis(6000)).await;
    let runs = sql(&c, &base, "SELECT id FROM chloedb_connector_runs WHERE connector = 'm'").await;
    assert!(runs.is_empty(), "the schedule never ran it: {runs:?}");
    // An admin mends it through the API, or deletes it.
    let (s, j) = call(&c, reqwest::Method::DELETE, &format!("{base}/v1/admin/connectors/m"), ROOT, None).await;
    assert_eq!(s, 200, "{j}");

    // Turned off through the API, the row the schedule reads.
    let (s, j) = post(&c, &format!("{base}/v1/admin/connectors/prom"), ROOT, json!({"enabled": false})).await;
    assert_eq!(s, 200, "{j}");
    let (_, one) = call(&c, reqwest::Method::GET, &format!("{base}/v1/admin/connectors/prom"), ROOT, None).await;
    assert_eq!(one["health"], "disabled", "{one}");
}

/// A source of JSON records, ten a page.
async fn items(n: usize) -> String {
    let app = Router::new().route(
        "/items",
        get(move |Query(q): Query<HashMap<String, String>>| async move {
            let p: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(0);
            let items: Vec<Json> = (p * 10..((p + 1) * 10).min(n)).map(|i| json!({"id": format!("i{i}"), "title": format!("item {i}"), "n": format!("{i}"), "when": format!("2026-11-13T03:{:02}:00Z", i % 60)})).collect();
            let next = ((p + 1) * 10 < n).then(|| format!("/items?page={}", p + 1));
            AxJson(json!({"data": {"items": items}, "next": next}))
        }),
    );
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_pull_names_no_secret_and_a_mapped_target_has_its_columns() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _svc) = start(dir.path(), false).await;
    let c = reqwest::Client::new();
    let admin = |p: &str| format!("{base}/v1/admin/connectors{p}");
    let (s, k) = post(&c, &format!("{base}/v1/admin/keys"), ROOT, json!({"name": "reader", "scope": "read"})).await;
    assert_eq!(s, 200, "{k}");
    let reader = k["secret"].as_str().unwrap().to_string();

    // A secret in the URL's query, and nothing listening there.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "leaky", "kind": "http_json", "every_s": 0, "config": {"allow_internal": true, "url": format!("http://127.0.0.1:{port}/x?api_key={{secret:token}}"), "records": "$.items"}, "secrets": {"token": "SUPERSECRET123"}})).await;
    assert_eq!(s, 200, "{j}");
    let (_, r) = post(&c, &admin("/leaky/run"), ROOT, json!({})).await;
    assert_eq!(r["ok"], false, "{r}");
    let e = r["error"].as_str().unwrap();
    assert!(e.contains(&format!("127.0.0.1:{port}/x")) && e.contains("error sending request"), "it still says what failed: {e}");
    // And why: reqwest's causes, connection refused here (DAP-13).
    assert!(e.to_lowercase().contains("connection refused"), "it says why: {e}");
    let (_, dry) = post(&c, &admin("/test"), ROOT, json!({"name": "leaky"})).await;
    let mut seen = vec![r.to_string(), dry.to_string()];
    for q in ["SELECT connector, error FROM chloedb_connector_runs", "SELECT id, last_error FROM chloedb_connector_state"] {
        let (s, rows) = post(&c, &format!("{base}/v1/query"), &reader, json!({"sql": q})).await;
        assert_eq!(s, 200, "{q}: {rows}");
        assert!(rows.to_string().contains("error sending request"), "{rows}");
        seen.push(rows.to_string());
    }
    let (_, one) = call(&c, reqwest::Method::GET, &admin("/leaky"), ROOT, None).await;
    seen.push(one.to_string());
    for text in seen {
        // The connector's own config keeps its placeholder; no error has the query.
        let text = text.replace("api_key={secret:token}", "");
        assert!(!text.contains("SUPERSECRET123") && !text.contains("api_key="), "a secret in an error: {text}");
    }

    // A target made from the connector's own map has the mapped columns.
    let up = items(25).await;
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "items", "kind": "http_json", "target": "items_t", "every_s": 0,
        "config": {"allow_internal": true, "url": format!("{up}/items"), "records": "$.data.items", "next": "$.next", "create_target": true,
            "map": {"id": "$.id", "title": "$.title", "n": {"path": "$.n", "as": "int"}, "when": {"path": "$.when", "as": "time"}}}})).await;
    assert_eq!(s, 200, "{j}");
    assert_eq!(j["made_target"], true);
    let (_, r) = post(&c, &admin("/items/run"), ROOT, json!({})).await;
    assert_eq!((r["ok"].clone(), r["rows"].clone(), r["dropped"].clone()), (json!(true), json!(25), json!([])), "{r}");
    let rows = sql(&c, &base, "SELECT id, title, n, \"when\" FROM items_t WHERE id = 'i7'").await;
    assert_eq!(rows[0]["title"], "item 7");
    assert_eq!(rows[0]["n"], 7);
    assert!(rows[0]["when"].as_str().unwrap().starts_with("2026-11-"), "{}", rows[0]);
}

/// A records path that matches nothing in a pull's answer is a warning,
/// in the run, its row, the state and the health, not a run that went
/// well with 0 records; a push with no records at its path is refused
/// with 422, not taken with 0; a pull run by hand is "pending" before
/// its first run, not "waiting" for a push (DAP-13, DAP-18, DAP-19).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_records_path_that_matches_nothing_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _svc) = start(dir.path(), false).await;
    let c = reqwest::Client::new();
    let admin = |p: &str| format!("{base}/v1/admin/connectors{p}");
    let up = items(5).await;
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "wrong", "kind": "http_json", "target": "wrong_t", "every_s": 0,
        "config": {"allow_internal": true, "url": format!("{up}/items"), "records": "$.items", "create_target": true, "map": {"id": "$.id"}}})).await;
    assert_eq!(s, 200, "{j}");
    let (_, one) = call(&c, reqwest::Method::GET, &admin("/wrong"), ROOT, None).await;
    assert_eq!((one["health"].clone(), one["pulls"].clone()), (json!("pending"), json!(false)), "run by hand, not run yet: {one}");

    let said = "the records path $.items matched nothing; the top-level keys are data, next";
    let (_, r) = post(&c, &admin("/wrong/run"), ROOT, json!({})).await;
    assert_eq!((r["ok"].clone(), r["records"].clone()), (json!(true), json!(0)), "{r}");
    assert_eq!(r["warning"], said, "{r}");
    let (_, one) = call(&c, reqwest::Method::GET, &admin("/wrong"), ROOT, None).await;
    assert_eq!(one["health"], "warning", "{one}");
    assert_eq!(one["state"]["last_warning"], said, "{one}");
    assert_eq!(one["runs"][0]["warning"], said, "the run's row keeps it: {one}");
    let (s, t) = post(&c, &admin("/test"), ROOT, json!({"name": "wrong"})).await;
    assert_eq!((s, t["warning"].clone()), (200, json!(said)), "a dry run says it too: {t}");

    // The right path: rows, and the warning goes.
    let (s, j) = post(&c, &admin("/wrong"), ROOT, json!({"config": {"allow_internal": true, "url": format!("{up}/items"), "records": "$.data.items", "map": {"id": "$.id"}}})).await;
    assert_eq!(s, 200, "{j}");
    let (_, r) = post(&c, &admin("/wrong/run"), ROOT, json!({})).await;
    assert_eq!((r["ok"].clone(), r["rows"].clone(), r["warning"].clone()), (json!(true), json!(5), Json::Null), "{r}");
    let (_, one) = call(&c, reqwest::Method::GET, &admin("/wrong"), ROOT, None).await;
    assert_eq!(one["health"], "ok", "{one}");

    // Pushes with nothing at their path: 422, and the run is a failure.
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "am", "kind": "webhook", "config": {"preset": "alertmanager"}})).await;
    assert_eq!(s, 200, "{j}");
    let am_key = j["push_secret"].as_str().unwrap().to_string();
    let (s, e) = post(&c, &format!("{base}/v1/connectors/am/push"), &am_key, json!({"status": "firing", "groupLabels": {}})).await;
    assert_eq!(s, 422, "{e}");
    assert!(e["error"].as_str().unwrap().contains("the records path $.alerts matched nothing; the top-level keys are groupLabels, status"), "{e}");
    let (_, one) = call(&c, reqwest::Method::GET, &admin("/am"), ROOT, None).await;
    assert_eq!(one["health"], "failing", "{one}");
    let (s, r) = post(&c, &format!("{base}/v1/connectors/am/push"), &am_key, json!({"status": "resolved", "alerts": []})).await;
    assert_eq!((s, r["records"].clone()), (200, json!(0)), "an empty list is no alerts, not a wrong shape: {r}");
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "hook", "kind": "webhook", "target": "hook_t", "config": {"records": "$.items", "create_target": true, "map": {"id": "$.id"}}})).await;
    assert_eq!(s, 200, "{j}");
    let key = j["push_secret"].as_str().unwrap().to_string();
    let (s, e) = post(&c, &format!("{base}/v1/connectors/hook/push"), &key, json!({"data": {"items": [{"id": "1"}]}})).await;
    assert_eq!(s, 422, "{e}");
    let (s, e) = post(&c, &admin("/test"), ROOT, json!({"name": "hook", "sample": {"data": {}}})).await;
    assert_eq!(s, 422, "a dry run's sample too: {e}");
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "mailbox", "kind": "mail"})).await;
    assert_eq!(s, 200, "{j}");
    let key = j["push_secret"].as_str().unwrap().to_string();
    let (s, e) = post(&c, &format!("{base}/v1/connectors/mailbox/push"), &key, json!([])).await;
    assert_eq!(s, 422, "{e}");
}

/// A server that records each request's path, query and Authorization,
/// and answers what `answer` makes of the path.
async fn recorder(answer: fn(&str, &str) -> axum::response::Response) -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let app = Router::new().fallback(move |uri: axum::http::Uri, Query(q): Query<HashMap<String, String>>, h: HeaderMap| {
        let s = s.clone();
        async move {
            s.lock().unwrap().push((uri.path().to_string(), q, h.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string)));
            answer(uri.path(), uri.query().unwrap_or_default())
        }
    });
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

static ELSEWHERE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connectors_reach_what_they_may_and_send_credentials_to_their_own_origin_only() {
    use axum::response::IntoResponse;
    let dir = tempfile::tempdir().unwrap();
    let (base, _svc) = start(dir.path(), false).await;
    let c = reqwest::Client::new();
    let admin = |p: &str| format!("{base}/v1/admin/connectors{p}");
    // Another origin (another port), which no credential may reach.
    let (other, other_seen) = recorder(|_, _| AxJson(json!({"items": [{"id": "x"}]})).into_response()).await;
    ELSEWHERE.set(other.clone()).unwrap();
    let (up, up_seen) = recorder(|path, _| match path {
        "/moved" => axum::response::Redirect::temporary(&format!("{}/stolen", ELSEWHERE.get().unwrap())).into_response(),
        "/here" => axum::response::Redirect::temporary("/items").into_response(),
        "/big" => "x".repeat(2 << 20).into_response(),
        "/paged" => AxJson(json!({"items": [{"id": "a"}], "next": format!("{}/page2?k={{secret:password}}", ELSEWHERE.get().unwrap())})).into_response(),
        _ => AxJson(json!({"items": [{"id": "i1"}]})).into_response(),
    })
    .await;
    let secrets = json!({"token": "TOKEN-1", "password": "PASSWORD-2"});

    // Without allow_internal a connector reaches the public internet only:
    // not this machine (nor metadata, even with it).
    let (s, j) = post(&c, &admin("/test"), ROOT, json!({"kind": "http_json", "config": {"url": format!("{up}/items"), "records": "$.items"}})).await;
    assert_eq!(s, 502, "{j}");
    assert!(j["error"].as_str().unwrap().contains("not an address a connector may reach"), "{j}");
    for u in ["http://169.254.169.254/latest/meta-data/", "http://[fe80::1]/"] {
        let (s, j) = post(&c, &admin("/test"), ROOT, json!({"kind": "http_json", "config": {"allow_internal": true, "url": u, "records": "$"}})).await;
        assert_eq!(s, 502, "{u}: {j}");
        assert!(j["error"].as_str().unwrap().contains("never"), "{j}");
    }
    assert!(up_seen.lock().unwrap().is_empty(), "nothing was asked");
    let (s, j) = post(&c, &admin("/test"), ROOT, json!({"kind": "http_json", "config": {"allow_internal": true, "url": format!("{up}/items"), "records": "$.items"}})).await;
    assert_eq!((s, j["records"].clone()), (200, json!(1)), "{j}");

    // A redirect within the origin is followed; to another it is not, and
    // the other origin never sees the token.
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "moved", "kind": "http_json", "target": "moved_t", "every_s": 0, "config": {"allow_internal": true, "url": format!("{up}/here"), "records": "$.items", "create_target": true, "map": {"id": "$.id"}}, "secrets": secrets})).await;
    assert_eq!(s, 200, "{j}");
    let (_, r) = post(&c, &admin("/moved/run"), ROOT, json!({})).await;
    assert_eq!((r["ok"].clone(), r["rows"].clone()), (json!(true), json!(1)), "{r}");
    let (s, _) = post(&c, &admin("/moved"), ROOT, json!({"config": {"allow_internal": true, "url": format!("{up}/moved"), "records": "$.items", "map": {"id": "$.id"}}})).await;
    assert_eq!(s, 200);
    let (_, r) = post(&c, &admin("/moved/run"), ROOT, json!({})).await;
    assert_eq!(r["ok"], false, "{r}");
    assert!(r["error"].as_str().unwrap().contains("not followed"), "{r}");
    assert!(other_seen.lock().unwrap().is_empty(), "the redirect was not followed: {:?}", other_seen.lock().unwrap());
    assert!(up_seen.lock().unwrap().iter().any(|(p, _, a)| p == "/moved" && a.as_deref() == Some("Bearer TOKEN-1")), "its own origin has the token");

    // A next page on another origin, read from the answer, is asked with
    // no credential and no secret expanded.
    let (s, j) = post(&c, &admin(""), ROOT, json!({"name": "paged", "kind": "http_json", "target": "moved_t", "every_s": 0, "config": {"allow_internal": true, "url": format!("{up}/paged?key={{secret:token}}"), "records": "$.items", "next": "$.next", "map": {"id": "$.id"}}, "secrets": secrets})).await;
    assert_eq!(s, 200, "{j}");
    let (_, r) = post(&c, &admin("/paged/run"), ROOT, json!({})).await;
    assert_eq!(r["ok"], true, "{r}");
    let asked = other_seen.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].0, "/page2");
    assert_eq!(asked[0].2, None, "no Authorization to another origin");
    assert!(!format!("{asked:?}").contains("PASSWORD-2") && !format!("{asked:?}").contains("TOKEN-1"), "no secret: {asked:?}");
    assert!(up_seen.lock().unwrap().iter().any(|(p, q, _)| p == "/paged" && q.get("key").map(String::as_str) == Some("TOKEN-1")), "its own URL has its secret");

    // An answer is read up to the connector's cap.
    let (s, j) = post(&c, &admin("/test"), ROOT, json!({"kind": "http_json", "config": {"allow_internal": true, "url": format!("{up}/big"), "records": "$", "max_body_mb": 1}})).await;
    assert_eq!(s, 502, "{j}");
    assert!(j["error"].as_str().unwrap().contains("larger than 1 MiB"), "{j}");
    let (s, j) = post(&c, &admin("/test"), ROOT, json!({"kind": "http_json", "config": {"allow_internal": true, "url": format!("{up}/items"), "max_body_mb": 0}})).await;
    assert_eq!(s, 400, "{j}");

    // env: secrets name variables with the operator's prefix only, and a
    // dry run takes none.
    std::env::set_var("CHLOEDB_CONNECTOR_SECRET_TEST_TOKEN", "FROM-ENV");
    let (s, j) = post(&c, &admin("/paged"), ROOT, json!({"secrets": {"token": "env:ROOT_TOKEN_OF_THE_SERVER"}})).await;
    assert_eq!(s, 400, "{j}");
    assert!(j["error"].as_str().unwrap().contains("CHLOEDB_CONNECTOR_SECRET_"), "{j}");
    let (s, j) = post(&c, &admin("/paged"), ROOT, json!({"secrets": {"token": "env:CHLOEDB_CONNECTOR_SECRET_TEST_TOKEN"}})).await;
    assert_eq!(s, 200, "{j}");
    let (_, r) = post(&c, &admin("/paged/run"), ROOT, json!({})).await;
    assert_eq!(r["ok"], true, "{r}");
    assert!(up_seen.lock().unwrap().iter().any(|(p, q, _)| p == "/paged" && q.get("key").map(String::as_str) == Some("FROM-ENV")), "read from its variable");
    let (s, j) = post(&c, &admin("/test"), ROOT, json!({"kind": "http_json", "config": {"allow_internal": true, "url": format!("{up}/items"), "records": "$.items"}, "secrets": {"token": "env:CHLOEDB_CONNECTOR_SECRET_TEST_TOKEN"}})).await;
    assert_eq!(s, 400, "{j}");

    // A push takes a run's records at most.
    let (s, k) = post(&c, &admin(""), ROOT, json!({"name": "hook", "kind": "webhook", "target": "hook_t", "config": {"create_target": true, "map": {"id": "$.id"}}})).await;
    assert_eq!(s, 200, "{k}");
    let key = k["push_secret"].as_str().unwrap().to_string();
    let many: Vec<Json> = (0..20_001).map(|i| json!({"id": format!("{i}")})).collect();
    let res = c.post(format!("{base}/v1/connectors/hook/push")).bearer_auth(&key).json(&many).send().await.unwrap();
    assert_eq!(res.status(), 413, "{}", res.text().await.unwrap());
    let res = c.post(format!("{base}/v1/connectors/hook/push")).bearer_auth(&key).json(&many[..100]).send().await.unwrap();
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
}
