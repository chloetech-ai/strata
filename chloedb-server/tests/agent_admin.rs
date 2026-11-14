//! Administration by agents ([`chloedb_server::ops`]): role keys, tiers,
//! reasons and caps, error codes, dry runs, change requests a person
//! approves (and an agent never does), the catalog, jobs and runbooks.

use std::sync::Arc;
use std::time::Duration;

use chloedb::db::Db;
use chloedb::storage::DbOptions;
use chloedb_server::auth::{Access, Secret};
use chloedb_server::keys::Keys;
use chloedb_server::service::{AgentConfig, Backend, Service};
use chloedb_server::{demo, server};
use serde_json::{json, Value as Json};
use tokio::net::TcpListener;

const N: usize = 200;
const ROOT: &str = "root-token";

async fn start(dir: &std::path::Path) -> String {
    let db = Db::create(&dir.join("a.strata"), demo::schema(N), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(chloedb::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).map(|i| demo::product(i, 7))).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent, 16, Duration::from_secs(10));
    svc.keys = Arc::new(Keys::open(dir.join("a.strata.keys.json")).unwrap());
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_http(Arc::new(svc), l, Access { token: Some(Secret::new(ROOT)), ..Default::default() }));
    base
}

async fn call(base: &str, method: &str, path: &str, auth: &str, headers: &[(&str, &str)], body: Option<Json>) -> (u16, Json) {
    let c = chloedb_server::http_client().build().unwrap();
    let mut r = c.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{base}{path}")).bearer_auth(auth);
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Json::String(text)))
}

async fn key(base: &str, body: Json) -> String {
    let (s, j) = call(base, "POST", "/v1/admin/keys", ROOT, &[], Some(body)).await;
    assert_eq!(s, 200, "{j}");
    j["secret"].as_str().unwrap().to_string()
}

/// An MCP tool's answer: whether it failed, and its text as JSON.
async fn tool(base: &str, auth: &str, name: &str, args: Json) -> (bool, Json) {
    let (s, j) = call(base, "POST", "/v1/mcp", auth, &[], Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}))).await;
    assert_eq!(s, 200, "{j}");
    let r = &j["result"];
    let text = r["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{j}")).to_string();
    (r["isError"].as_bool().unwrap_or(false), serde_json::from_str(&text).unwrap_or(Json::String(text)))
}

#[tokio::test(flavor = "multi_thread")]
async fn role_keys_act_by_tier_with_reasons_and_caps() {
    let dir = tempfile::tempdir().unwrap();
    let base = start(dir.path()).await;
    let ops = key(&base, json!({"name": "oncall-bot", "scope": "ops", "expires_minutes": 60, "on_behalf_of": "dana"})).await;
    let propose = key(&base, json!({"name": "planner-bot", "scope": "propose"})).await;

    // whoami says what each may do.
    let (_, w) = call(&base, "GET", "/v1/whoami", &ops, &[], None).await;
    assert_eq!(w["may"]["act_alone_up_to_tier"], 1, "{w}");
    assert_eq!(w["may"]["approve_changes"], false, "{w}");
    assert_eq!(w["may"]["agent"], true, "{w}");
    assert_eq!(w["role"], "ops", "{w}");
    let (_, w) = call(&base, "GET", "/v1/whoami", &propose, &[], None).await;
    assert_eq!(w["may"]["act_alone_up_to_tier"], 0, "{w}");
    assert_eq!(w["may"]["propose_changes"], true, "{w}");
    let (_, w) = call(&base, "GET", "/v1/whoami", ROOT, &[], None).await;
    assert_eq!(w["may"]["approve_changes"], true, "{w}");

    // Both read the server's views; neither writes rows.
    let (s, j) = call(&base, "GET", "/v1/admin/audit", &propose, &[], None).await;
    assert_eq!(s, 200, "{j}");
    let (s, j) = call(&base, "POST", "/v1/write", &ops, &[], Some(json!({"rows": [{"id": "x"}]}))).await;
    assert_eq!((s, j["error_code"].as_str()), (403, Some("scope_refused")), "{j}");
    assert!(j["hint"].as_str().unwrap().contains("change request"), "{j}");
    assert!(j["docs"].as_str().unwrap().starts_with("https://"), "{j}");

    // A contain rule: refused to a proposer, needs a reason, capped.
    let rule = json!({"match": {"fingerprint": "ab12"}, "action": "refuse", "expires_s": 600});
    let (s, j) = call(&base, "POST", "/v1/admin/contain", &propose, &[("x-chloedb-reason", "x")], Some(rule.clone())).await;
    assert_eq!((s, j["error_code"].as_str()), (403, Some("scope_refused")), "{j}");
    let (s, j) = call(&base, "POST", "/v1/admin/contain", &ops, &[], Some(rule.clone())).await;
    assert_eq!((s, j["error_code"].as_str()), (400, Some("reason_required")), "{j}");
    let mut long = rule.clone();
    long["expires_s"] = json!(7200);
    let (s, j) = call(&base, "POST", "/v1/admin/contain", &ops, &[("x-chloedb-reason", "it holds every core")], Some(long)).await;
    assert_eq!((s, j["error_code"].as_str()), (400, Some("limit_exceeded")), "{j}");
    let (s, j) = call(&base, "POST", "/v1/admin/contain", &ops, &[("x-chloedb-reason", "it holds every core"), ("x-chloedb-agent-run", "run-7")], Some(rule)).await;
    assert_eq!(s, 200, "{j}");

    // A tier 2 change is not an ops key's.
    let (s, j) = call(&base, "PUT", "/v1/synonyms", &ops, &[("x-chloedb-reason", "x")], Some(json!({"synonyms": []}))).await;
    assert_eq!(s, 403, "{j}");

    // The audit log names the agent, its run and its person.
    let (_, a) = call(&base, "GET", "/v1/admin/audit?limit=200", ROOT, &[], None).await;
    let entries = a["entries"].as_array().or(a.as_array()).cloned().unwrap_or_default();
    let e = entries.iter().find(|e| e["route"] == "/v1/admin/contain" && e["status"] == 200).unwrap_or_else(|| panic!("{a}"));
    assert_eq!(e["actor"], "agent", "{e}");
    assert_eq!(e["agent"]["run"], "run-7", "{e}");
    assert_eq!(e["on_behalf_of"], "dana", "{e}");
    assert_eq!(e["reason"], "it holds every core", "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_file_changes_people_approve_them_and_stale_plans_fail() {
    let dir = tempfile::tempdir().unwrap();
    let base = start(dir.path()).await;
    let propose = key(&base, json!({"name": "planner-bot", "scope": "propose"})).await;
    let agent_admin = key(&base, json!({"name": "admin-bot", "scope": "admin", "agent": true})).await;

    // An agent's admin key does not drop a table, or delete every row, alone.
    let (s, j) = call(&base, "POST", "/v1/sql", &agent_admin, &[("x-chloedb-reason", "clean up")], Some(json!({"sql": "DELETE FROM products"}))).await;
    assert_eq!((s, j["error_code"].as_str()), (403, Some("change_request_required")), "{j}");
    let (s, j) = call(&base, "POST", "/v1/sql", &agent_admin, &[("x-chloedb-reason", "clean up")], Some(json!({"sql": "DROP TABLE products"}))).await;
    assert_eq!((s, j["error_code"].as_str()), (403, Some("change_request_required")), "{j}");

    // The dry run says what a DELETE would touch.
    let (s, d) = call(&base, "POST", "/v1/admin/changes/preview", &propose, &[], Some(json!({"method": "POST", "path": "/v1/sql", "body": {"sql": "DELETE FROM products"}}))).await;
    assert_eq!(s, 200, "{d}");
    assert_eq!(d["tier"], 3, "{d}");
    assert_eq!(d["affects"]["rows"], N, "{d}");
    assert_eq!(d["reversible"], false, "{d}");
    assert!(d["plan_id"].as_str().unwrap().starts_with("pl_"), "{d}");

    // One row's id, to delete it by a change request.
    let (_, q) = call(&base, "POST", "/v1/query", ROOT, &[], Some(json!({"sql": "SELECT id FROM products LIMIT 1"}))).await;
    let id = q["rows"][0][0].as_str().or(q["rows"][0]["id"].as_str()).unwrap_or_else(|| panic!("{q}")).to_string();
    let one = json!({"method": "POST", "path": "/v1/sql", "body": {"sql": format!("DELETE FROM products WHERE id = '{id}'")}});
    let mut filed = one.clone();
    filed["reason"] = json!("a duplicate row");
    let (s, c) = call(&base, "POST", "/v1/admin/changes", &propose, &[], Some(filed)).await;
    assert_eq!(s, 200, "{c}");
    let cid = c["id"].as_str().unwrap().to_string();
    assert!(cid.starts_with('c'), "{c}");
    assert_eq!((c["state"].as_str(), c["action"].as_str(), c["agent"].as_bool()), (Some("pending"), Some("change"), Some(true)), "{c}");

    // A reason is needed; a proposer may not approve; nor may an agent's admin key.
    let (s, j) = call(&base, "POST", "/v1/admin/changes", &propose, &[], Some(one.clone())).await;
    assert_eq!((s, j["error_code"].as_str()), (400, Some("reason_required")), "{j}");
    let (s, _) = call(&base, "POST", &format!("/v1/admin/changes/{cid}/approve"), &propose, &[], None).await;
    assert_eq!(s, 403);
    let (s, j) = call(&base, "POST", &format!("/v1/admin/changes/{cid}/approve"), &agent_admin, &[], None).await;
    assert_eq!((s, j["error_code"].as_str()), (403, Some("approval_refused")), "{j}");
    // Nor an admin key through MCP's headers.
    let (s, j) = call(&base, "POST", &format!("/v1/admin/changes/{cid}/approve"), ROOT, &[("x-chloedb-agent-session", "s1")], None).await;
    assert_eq!((s, j["error_code"].as_str()), (403, Some("approval_refused")), "{j}");

    // It shows as a job, and in the list.
    let (_, jobs) = call(&base, "GET", "/v1/admin/jobs?kind=change", &propose, &[], None).await;
    assert!(jobs["jobs"].as_array().unwrap().iter().any(|j| j["id"] == cid), "{jobs}");
    let (_, list) = call(&base, "GET", "/v1/admin/changes?state=pending", &propose, &[], None).await;
    assert_eq!(list["pending"], 1, "{list}");

    // A person approves: applied, audited as theirs, naming the request.
    let (s, a) = call(&base, "POST", &format!("/v1/admin/changes/{cid}/approve"), ROOT, &[], None).await;
    assert_eq!(s, 200, "{a}");
    assert_eq!(a["state"], "applied", "{a}");
    let (s, j) = call(&base, "POST", &format!("/v1/admin/changes/{cid}/approve"), ROOT, &[], None).await;
    assert_eq!((s, j["error_code"].as_str()), (409, Some("change_not_pending")), "{j}");
    let (_, audit) = call(&base, "GET", "/v1/admin/audit?limit=200", ROOT, &[], None).await;
    let entries = audit["entries"].as_array().or(audit.as_array()).cloned().unwrap_or_default();
    let e = entries.iter().find(|e| e["change_request"] == cid.as_str()).unwrap_or_else(|| panic!("{audit}"));
    assert_eq!(e["actor"], "person", "{e}");
    assert!(e["reason"].as_str().unwrap().contains("a duplicate row"), "{e}");

    // Filed for 199 rows; the table then loses most of them: stale.
    let mut all = json!({"method": "POST", "path": "/v1/sql", "body": {"sql": "DELETE FROM products WHERE price >= 0"}});
    all["reason"] = json!("start over");
    let (s, c) = call(&base, "POST", "/v1/admin/changes", &propose, &[], Some(all)).await;
    assert_eq!(s, 200, "{c}");
    let stale = c["id"].as_str().unwrap().to_string();
    let (s, j) = call(&base, "POST", "/v1/sql", ROOT, &[], Some(json!({"sql": "DELETE FROM products"}))).await;
    assert_eq!(s, 200, "{j}");
    let (s, j) = call(&base, "POST", &format!("/v1/admin/changes/{stale}/approve"), ROOT, &[], None).await;
    assert_eq!((s, j["error_code"].as_str()), (409, Some("plan_stale")), "{j}");
    let (_, got) = call(&base, "GET", &format!("/v1/admin/changes/{stale}"), &propose, &[], None).await;
    assert_eq!(got["state"], "failed", "{got}");
    assert_eq!(got["result"]["error_code"], "plan_stale", "{got}");

    // Withdrawn by whoever filed it, and no one else.
    let mut again = one;
    again["reason"] = json!("try again");
    let (_, c) = call(&base, "POST", "/v1/admin/changes", &propose, &[], Some(again)).await;
    let w = c["id"].as_str().unwrap().to_string();
    let (s, _) = call(&base, "POST", &format!("/v1/admin/changes/{w}/withdraw"), &agent_admin, &[], None).await;
    assert_eq!(s, 403);
    let (s, j) = call(&base, "POST", &format!("/v1/admin/changes/{w}/withdraw"), &propose, &[], None).await;
    assert_eq!((s, j["state"].as_str()), (200, Some("withdrawn")), "{j}");
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_find_routes_tools_jobs_events_and_runbooks() {
    let dir = tempfile::tempdir().unwrap();
    let base = start(dir.path()).await;
    let ops = key(&base, json!({"name": "oncall-bot", "scope": "ops"})).await;
    let read = key(&base, json!({"name": "reader", "scope": "read"})).await;

    let (s, c) = call(&base, "GET", "/v1/catalog", &read, &[], None).await;
    assert_eq!(s, 200, "{c}");
    assert!(c["count"].as_u64().unwrap() > 180, "{}", c["count"]);
    let routes = c["routes"].as_array().unwrap();
    let contain = routes.iter().find(|r| r["method"] == "POST" && r["path"] == "/v1/admin/contain").unwrap();
    assert_eq!((contain["tier"].as_u64(), contain["tool"].as_str(), contain["roles"]["ops"].as_bool(), contain["roles"]["propose"].as_bool()), (Some(1), Some("contain"), Some(true), Some(false)), "{contain}");
    let apply = routes.iter().find(|r| r["method"] == "POST" && r["path"] == "/v1/admin/plan/apply").unwrap();
    assert_eq!((apply["tier"].as_u64(), apply["dry_run"].as_str()), (Some(2), Some("POST /v1/admin/plan")), "{apply}");
    assert!(c["errors"].as_array().unwrap().iter().any(|e| e["code"] == "plan_stale"), "{c}");
    let (_, c) = call(&base, "GET", "/v1/catalog?q=contain", &read, &[], None).await;
    assert!(c["routes"].as_array().unwrap().iter().all(|r| r["path"].as_str().unwrap().contains("contain")), "{c}");

    // Experimental features: marked in the catalog, and their answers say so.
    let oncall = routes.iter().find(|r| r["method"] == "GET" && r["path"] == "/v1/admin/oncall").unwrap();
    assert_eq!(oncall["experimental"], "oncall", "{oncall}");
    assert!(contain.get("experimental").is_none(), "{contain}");
    let marked = |path: &'static str| {
        let base = base.clone();
        async move {
            let r = chloedb_server::http_client().build().unwrap().get(format!("{base}{path}")).bearer_auth(ROOT).send().await.unwrap();
            r.headers().get("x-chloedb-experimental").map(|v| v.to_str().unwrap().to_string())
        }
    };
    assert_eq!(marked("/v1/admin/oncall").await.as_deref(), Some("oncall"));
    assert_eq!(marked("/v1/admin/hub").await.as_deref(), Some("hub"));
    assert_eq!(marked("/v1/health").await, None);

    // The operations tools are listed to a role key, not to a read key.
    let list = |auth: String| {
        let base = base.clone();
        async move {
            let (_, j) = call(&base, "POST", "/v1/mcp", &auth, &[], Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))).await;
            j["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        }
    };
    let names = list(ops.clone()).await;
    for t in ["health", "catalog", "jobs", "runbook", "dry_run", "propose_change", "contain", "cancel_query"] {
        assert!(names.iter().any(|n| n == t), "{t} not in {names:?}");
    }
    let names = list(read.clone()).await;
    assert!(!names.iter().any(|n| n == "propose_change"), "{names:?}");

    // Through MCP: a contain rule with a reason, a runbook.
    let (err, j) = tool(&base, &ops, "contain", json!({"fingerprint": "ab12", "action": "refuse", "expires_s": 600, "reason": "it holds every core"})).await;
    assert!(!err, "{j}");
    let (err, j) = tool(&base, &ops, "runbook", json!({"for": "copy-down"})).await;
    assert!(!err, "{j}");
    assert_eq!(j["runbooks"][0]["title"], "A copy is down", "{j}");
    let (err, j) = tool(&base, &ops, "propose_change", json!({"method": "PUT", "path": "/v1/synonyms", "body": {"synonyms": []}, "reason": "the list is stale"})).await;
    assert!(!err, "{j}");
    assert_eq!(j["state"], "pending", "{j}");

    let (s, r) = call(&base, "GET", "/v1/runbooks?for=disk-full", &read, &[], None).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["runbooks"][0]["title"], "The disk is filling", "{r}");

    // Waiting on the next event: answers when one comes, or at wait_s.
    let (_, ev) = call(&base, "GET", "/v1/events?limit=1", &ops, &[], None).await;
    let last = ev["events"][0]["id"].as_i64().unwrap_or(0);
    let t0 = std::time::Instant::now();
    let (s, ev) = call(&base, "GET", &format!("/v1/events?after={last}&wait_s=1"), &ops, &[], None).await;
    assert_eq!(s, 200, "{ev}");
    assert!(ev["events"].as_array().unwrap().iter().all(|e| e["id"].as_i64().unwrap() > last), "{ev}");
    assert!(t0.elapsed() < Duration::from_secs(5));
    let waiter = {
        let (base, ops) = (base.clone(), ops.clone());
        tokio::spawn(async move { call(&base, "GET", &format!("/v1/events?after={last}&wait_s=20"), &ops, &[], None).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (s, j) = call(&base, "POST", "/v1/events/notes", ROOT, &[], Some(json!({"text": "a person looked"}))).await;
    assert_eq!(s, 200, "{j}");
    let (s, ev) = tokio::time::timeout(Duration::from_secs(10), waiter).await.unwrap().unwrap();
    assert_eq!(s, 200, "{ev}");
    assert!(!ev["events"].as_array().unwrap().is_empty(), "{ev}");
}

/// Audit X-D2: a tier 3 route (it loses data or costs money) is refused to
/// an agent whatever its key, outside SQL too, with a reason or without;
/// a person's key, without an agent's headers, still calls it.
#[tokio::test(flavor = "multi_thread")]
async fn agents_never_act_alone_at_tier_3() {
    let dir = tempfile::tempdir().unwrap();
    let base = start(dir.path()).await;
    let agent_admin = key(&base, json!({"name": "admin-bot", "scope": "admin", "agent": true})).await;
    let (s, made) = call(&base, "POST", "/v1/admin/keys", ROOT, &[], Some(json!({"name": "spare", "scope": "read"}))).await;
    assert_eq!(s, 200, "{made}");
    let spare = made["key"]["id"].as_str().unwrap().to_string();
    let why = ("x-chloedb-reason", "it is not used any more");
    let revoke = format!("/v1/admin/keys/{spare}");
    for (method, path, auth, headers) in [
        ("DELETE", revoke.as_str(), agent_admin.as_str(), vec![why]),
        ("POST", "/v1/admin/vacuum", agent_admin.as_str(), vec![why]),
        ("POST", "/v1/admin/hold-writes", agent_admin.as_str(), vec![why]),
        ("DELETE", "/v1/admin/shards/0", agent_admin.as_str(), vec![why]),
        // A person's key through an agent's headers is an agent's request.
        ("DELETE", revoke.as_str(), ROOT, vec![why, ("x-chloedb-agent-session", "s1")]),
        ("DELETE", revoke.as_str(), ROOT, vec![why, ("x-chloedb-app", "mcp")]),
    ] {
        let (s, j) = call(&base, method, path, auth, &headers, Some(json!({}))).await;
        assert_eq!((s, j["error_code"].as_str()), (403, Some("change_request_required")), "{method} {path}: {j}");
    }
    // The key is still there; a person revokes it.
    let (s, j) = call(&base, "DELETE", &revoke, ROOT, &[], None).await;
    assert_eq!(s, 200, "{j}");
    // Tier 2 with a reason is still an admin agent's to do.
    let (s, j) = call(&base, "PUT", "/v1/synonyms", &agent_admin, &[why], Some(json!({"synonyms": []}))).await;
    assert_ne!(j["error_code"].as_str(), Some("change_request_required"), "{s} {j}");
}

/// Audit X-D1: a change request carries no credential and makes none: the
/// routes that make or show a key's secret, or take another cluster's or
/// a cloud's, are refused as targets, and so is a body holding one; what
/// role keys read of change requests holds no secret.
#[tokio::test(flavor = "multi_thread")]
async fn change_requests_carry_no_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let base = start(dir.path()).await;
    let propose = key(&base, json!({"name": "planner-bot", "scope": "propose"})).await;
    for (method, path, body) in [
        ("POST", "/v1/admin/keys", json!({"name": "mine", "scope": "admin"})),
        ("POST", "/v1/admin/keys/k1/rotate", json!({})),
        ("POST", "/v1/keys/own", json!({"name": "mine"})),
        ("POST", "/v1/admin/hub/connections", json!({"name": "other", "urls": ["https://x"], "key": "sk_x"})),
        ("PUT", "/v1/admin/aws/credentials", json!({"access_key_id": "AKIA", "secret_access_key": "s"})),
        ("PUT", "/v1/admin/digitalocean/token", json!({"token": "dop_v1_x"})),
        ("POST", "/v1/admin/connectors", json!({"name": "c", "kind": "http"})),
        ("POST", "/v1/admin/connectors/c/key", json!({})),
    ] {
        let (s, j) = call(&base, "POST", "/v1/admin/changes", &propose, &[], Some(json!({"method": method, "path": path, "body": body, "reason": "x"}))).await;
        assert_eq!((s, j["error_code"].as_str()), (403, Some("scope_refused")), "{method} {path}: {j}");
        assert!(j["error"].as_str().unwrap().contains("credential"), "{j}");
    }
    // A body with a credential in it, for a route that takes none.
    let (s, j) = call(&base, "POST", "/v1/admin/changes", &propose, &[], Some(json!({"method": "PUT", "path": "/v1/alerts/routes", "body": {"routes": [{"name": "pager", "url": "https://x", "headers": {"Authorization": "Bearer abc"}}]}, "reason": "page me"}))).await;
    assert_eq!((s, j["error_code"].as_str()), (400, Some("bad_request")), "{j}");
    assert!(j["error"].as_str().unwrap().contains("Authorization"), "{j}");
    // Nothing was filed.
    let (_, list) = call(&base, "GET", "/v1/admin/changes", &propose, &[], None).await;
    assert!(!list.to_string().contains("sk_x") && !list.to_string().contains("dop_v1_x") && !list.to_string().contains("Bearer"), "{list}");
}
