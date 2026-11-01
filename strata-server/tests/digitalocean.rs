//! The cloud view's DigitalOcean routes against a stand-in for its API: a
//! token checked and kept readable by its owner only, never in an answer or
//! an audit entry, the sizes DOKS takes with their prices and the droplet
//! limit, a plan and its checks, and only an administrator reaching any of
//! it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::auth::{Access, Secret};
use strata_server::digitalocean::DigitalOcean;
use strata_server::keys::Keys;
use strata_server::service::{AgentConfig, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

const TOKEN: &str = "dop_v1_the-token-never-shown-0123456789abcdef";
const REFUSED: &str = "dop_v1_refused-token-0000000000000000000000";

const SIZES: &str = include_str!("fixtures/digitalocean/sizes.json");
const OPTIONS: &str = include_str!("fixtures/digitalocean/kubernetes-options.json");

/// DigitalOcean's API, as far as the view uses it: reads only, with the
/// token as a bearer; any other token is refused as the API refuses it.
async fn api(State(seen): State<Arc<Mutex<Vec<String>>>>, method: Method, OriginalUri(uri): OriginalUri, headers: HeaderMap) -> axum::response::Response {
    let path = uri.path().to_string();
    seen.lock().unwrap().push(format!("{method} {path}"));
    assert_eq!(method, Method::GET, "{path}: the view only reads");
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
    if auth != format!("Bearer {TOKEN}") {
        return (StatusCode::UNAUTHORIZED, json!({"id": "unauthorized", "message": "Unable to authenticate you"}).to_string()).into_response();
    }
    match path.as_str() {
        "/v2/account" => json!({"account": {"droplet_limit": 25, "volume_limit": 5000, "email": "someone@example.com", "uuid": "acct-uuid", "status": "active", "team": {"uuid": "team-uuid", "name": "My Team"}}}).to_string(),
        "/v2/sizes" => SIZES.to_string(),
        "/v2/kubernetes/options" => OPTIONS.to_string(),
        "/v2/droplets" => json!({"droplets": [], "meta": {"total": 3}}).to_string(),
        other => panic!("DigitalOcean was not asked for {other}"),
    }
    .into_response()
}

async fn listen() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    (l, url)
}

async fn call(base: &str, method: &str, path: &str, auth: Option<&str>, body: Option<Json>) -> (u16, String) {
    let c = reqwest::Client::new();
    let mut r = match method {
        "GET" => c.get(format!("{base}{path}")),
        "DELETE" => c.delete(format!("{base}{path}")),
        "PUT" => c.put(format!("{base}{path}")),
        _ => c.post(format!("{base}{path}")),
    };
    if let Some(a) = auth {
        r = r.bearer_auth(a);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

fn parse(s: &str) -> Json {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("{e}: {s}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_digitalocean_view_keeps_its_token_private_and_plans_from_its_prices() {
    let env_token = ["DIGITALOCEAN_TOKEN", "DIGITALOCEAN_ACCESS_TOKEN"].iter().any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()));

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (l, api_url) = listen().await;
    let app = axum::Router::new().fallback(api).with_state(seen.clone());
    tokio::spawn(async move { axum::serve(l, app).await });

    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("a.strata"), demo::schema(10), DbOptions::default()).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent, 4, Duration::from_secs(10));
    svc.keys = Arc::new(Keys::open(dir.path().join("a.strata.keys.json")).unwrap());
    let file = DigitalOcean::file_of(&dir.path().join("a.strata"));
    svc.digitalocean = Arc::new(DigitalOcean::open(file.clone()).unwrap().with_endpoint(&api_url));
    let svc = Arc::new(svc);
    let (l, base) = listen().await;
    let access = Access { token: Some(Secret::new("root-token")), ..Default::default() };
    tokio::spawn(server::serve_http(svc.clone(), l, access));
    let root = Some("root-token");
    let mut answers = Vec::new();

    // Only an administrator reaches it.
    let (status, j) = call(&base, "POST", "/v1/admin/keys", root, Some(json!({"name": "ingest", "scope": "write"}))).await;
    assert_eq!(status, 200, "{j}");
    let writer = parse(&j)["secret"].as_str().unwrap().to_string();
    for (method, path, body) in [("GET", "/v1/admin/digitalocean", None), ("PUT", "/v1/admin/digitalocean/token", Some(json!({"token": TOKEN}))), ("GET", "/v1/admin/digitalocean/machines", None), ("POST", "/v1/admin/digitalocean/plan", Some(json!({})))] {
        assert_eq!(call(&base, method, path, None, body.clone()).await.0, 401, "{method} {path}");
        assert_eq!(call(&base, method, path, Some(&writer), body).await.0, 403, "{method} {path}");
    }
    assert!(!file.exists() && seen.lock().unwrap().is_empty());
    if !env_token {
        assert_eq!(parse(&call(&base, "GET", "/v1/admin/digitalocean", root, None).await.1)["token"], Json::Null);
        let (status, j) = call(&base, "GET", "/v1/admin/digitalocean/machines", root, None).await;
        assert!(status == 400 && j.contains("no DigitalOcean token"), "{status} {j}");
    }

    // A token the API refuses is not kept; nor is a malformed one.
    let (status, j) = call(&base, "PUT", "/v1/admin/digitalocean/token", root, Some(json!({"token": REFUSED}))).await;
    assert!(status == 400 && j.contains("DigitalOcean refused this token") && j.contains("Unable to authenticate you"), "{status} {j}");
    answers.push(j);
    for (body, says) in [(json!({"token": ""}), "token is required"), (json!({"token": "dop v1"}), "letters, digits and punctuation"), (json!({}), "the body is")] {
        let (status, j) = call(&base, "PUT", "/v1/admin/digitalocean/token", root, Some(body)).await;
        assert!(status == 400 && j.contains(says), "{status} {j}");
    }
    assert!(!file.exists());

    // A good one: checked, kept mode 0600, shown masked, without the e-mail.
    let (status, j) = call(&base, "PUT", "/v1/admin/digitalocean/token", root, Some(json!({"token": TOKEN}))).await;
    assert_eq!(status, 200, "{j}");
    let saved = parse(&j);
    assert_eq!((saved["account"]["team"].as_str(), saved["account"]["droplet_limit"].as_u64()), (Some("My Team"), Some(25)), "{j}");
    assert_eq!(saved["token"]["token"], "dop_…cdef");
    assert!(!j.contains("someone@example.com"), "{j}");
    answers.push(j);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert_eq!(DigitalOcean::open(file.clone()).unwrap().token().unwrap().0, TOKEN);

    // The regions, and nyc3's sizes with their prices and the droplets.
    let (status, j) = call(&base, "GET", "/v1/admin/digitalocean/regions", root, None).await;
    assert_eq!(status, 200, "{j}");
    assert!(parse(&j)["regions"].as_array().unwrap().iter().any(|r| r["slug"] == "nyc3"), "{j}");
    let (status, j) = call(&base, "GET", "/v1/admin/digitalocean/machines?region=nyc3", root, None).await;
    assert_eq!(status, 200, "{j}");
    let cat = parse(&j);
    let m = cat["machines"].as_array().unwrap().iter().find(|m| m["type"] == "m-8vcpu-64gb").unwrap_or_else(|| panic!("{j}"));
    assert_eq!((m["usd_hour"].as_f64(), m["vcpu"].as_u64(), m["memory_gib"].as_f64(), m["max_pods"].as_u64()), (Some(0.5), Some(8), Some(64.0), Some(110)), "{m}");
    assert_eq!((cat["droplet_limit"].as_u64(), cat["droplets"].as_u64(), cat["versions"][0].as_str()), (Some(25), Some(3), Some("1.37.0-do.2")), "{j}");
    assert!(cat["machines"].as_array().unwrap().iter().all(|m| m["type"] != "so-8vcpu-64gb" && m["type"] != "gpu-h100x1-80gb"), "{j}");
    assert_eq!(cat["recommended"], json!(["g-8vcpu-32gb", "m-2vcpu-16gb", "m-4vcpu-32gb", "m-8vcpu-64gb"]));
    answers.push(j);
    for bad in ["NYC 3", "mars1"] {
        let (status, j) = call(&base, "GET", &format!("/v1/admin/digitalocean/machines?region={}", bad.replace(' ', "%20")), root, None).await;
        assert_eq!(status, 400, "{bad}: {j}");
    }

    // A plan, and its checks: the droplets fit until there are too many.
    let plan = json!({"name": "search", "region": "nyc3", "machine": "m-8vcpu-64gb", "nodes": 4, "volume_gb": 10, "cluster": {"shards": 4, "replicas": 2}});
    let (status, j) = call(&base, "POST", "/v1/admin/digitalocean/plan", root, Some(plan.clone())).await;
    assert_eq!(status, 200, "{j}");
    let p = parse(&j);
    assert!((p["cost"]["hour"].as_f64().unwrap() - (2.0 + 80.0 * 0.10 / 672.0)).abs() < 1e-9, "{}", p["cost"]);
    assert_eq!(p["request"]["node_pools"][0]["count"], 4);
    assert_eq!(p["quota"], json!({"droplets": 25, "running": 3, "needs": 4}));
    answers.push(j);
    let (status, j) = call(&base, "POST", "/v1/admin/digitalocean/validate", root, Some(plan.clone())).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(parse(&j)["validated"]["ok"], true, "{j}");
    let mut big = plan.clone();
    big["nodes"] = json!(23);
    let (status, j) = call(&base, "POST", "/v1/admin/digitalocean/validate", root, Some(big)).await;
    assert_eq!(status, 200, "{j}");
    let v = parse(&j);
    assert_eq!(v["validated"]["ok"], false, "{j}");
    assert!(v["warnings"].to_string().contains("may run 25 droplets and runs 3"), "{j}");
    answers.push(j);
    let (status, j) = call(&base, "POST", "/v1/admin/digitalocean/plan", root, Some(json!({"name": "search", "region": "nyc3", "machine": "so-8vcpu-64gb", "nodes": 3, "volume_gb": 10, "cluster": {}}))).await;
    assert!(status == 400 && j.contains("is not a size a DOKS node may be"), "{status} {j}");

    // Only reads were made.
    let calls = seen.lock().unwrap().clone();
    for want in ["GET /v2/account", "GET /v2/sizes", "GET /v2/kubernetes/options", "GET /v2/droplets"] {
        assert!(calls.iter().any(|c| c == want), "{want} not in {calls:?}");
    }

    // Forgetting deletes the file; the audit log names the token masked.
    let (status, j) = call(&base, "DELETE", "/v1/admin/digitalocean/token", root, None).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(parse(&j)["forgot"], "dop_…cdef");
    assert!(!file.exists());
    answers.push(j);
    let (_, j) = call(&base, "GET", "/v1/admin/audit", root, None).await;
    let details: Vec<String> = parse(&j)["entries"].as_array().unwrap().iter().filter_map(|e| e["detail"].as_str().map(String::from)).collect();
    assert!(details.iter().any(|d| d == "saved the DigitalOcean token dop_…cdef (team My Team, account acct-uuid)"), "{details:?}");
    assert!(details.iter().any(|d| d == "forgot the DigitalOcean token dop_…cdef"), "{details:?}");
    answers.push(j);
    for a in &answers {
        assert!(!a.contains(TOKEN) && !a.contains("0123456789abcdef") && !a.contains(REFUSED), "{a}");
    }
}
