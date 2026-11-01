//! Autoscaling end to end, without a Kubernetes cluster: the servers keep
//! a minute of load each (their log stores' `load`), a coordinator
//! replays rules on a past week of it (`POST /v1/admin/autoscale/replay`),
//! the rules are saved to the `StrataCluster`'s spec and checked, and the
//! operator's autoscaler acts on the live numbers, asks a person (who
//! dismisses or applies), or only says what it would do, against a
//! stand-in for the API server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::Method;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::coord::Coordinator;
use strata_server::logstore::{Load, LogOptions, LogStore};
use strata_server::operator::kube::{self, Kube, Link, Options};
use strata_server::operator::{merge_patch, Numbers, Outcome, Phase, Rollout, Spec};
use strata_server::service::{shard_of, AgentConfig, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

const N: usize = 100;
const MIN: u64 = 60_000;
const PATH: &str = "/apis/strata.chloetech.ai/v1alpha1/namespaces/db/strataclusters/orders";

async fn listen() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    (l, format!("http://127.0.0.1:{port}"))
}

async fn post(base: &str, path: &str, body: Json) -> (u16, Json) {
    let r = reqwest::Client::new().post(format!("{base}{path}")).json(&body).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Json::Null))
}

/// A shard of the demo catalogue with a log store of its own.
fn shard(dir: &std::path::Path, i: usize) -> Arc<Service> {
    let name = format!("s{i}.strata");
    let db = Db::create(&dir.join(&name), demo::schema(N), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).filter(|&k| shard_of(&strata::Value::Str(format!("p{k:07}")), 2) == i).map(|k| demo::product(k, 7))).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent, 16, Duration::from_secs(10));
    svc.log_store = Some(LogStore::open(LogOptions { capture: false, ..LogOptions::new(dir.join(format!("{name}.logs")), format!("s{i}")) }).unwrap());
    Arc::new(svc)
}

/// Six hours of minutes ending at `end`, three copies' rows a minute
/// (as three servers of the shard would keep them), `busy(minute)` busy.
fn minutes(store: &LogStore, shard: usize, end: u64, busy: impl Fn(u64) -> f64) {
    let rows: Vec<Load> = (0..360u64)
        .flat_map(|m| {
            let ts_ms = end - (360 - m) * MIN;
            let b = busy(m);
            (0..3).map(move |copy| Load { ts_ms: ts_ms + copy, role: if copy == 0 { "primary" } else { "replica" }.into(), shard: Some(shard), qps: 100.0, cpu: b * 4.0, cores: 4, p99_ms: Some(20.0), disk_used_ratio: Some(0.4 + m as f64 / 3600.0) })
        })
        .collect();
    store.write_loads(&rows).unwrap();
}

/// A stand-in for the API server: one `StrataCluster`, read and patched
/// (a merge patch of the resource or of its status), its generation up
/// when its spec changes.
async fn api_server(obj: Json) -> (Arc<Mutex<Json>>, String) {
    let state = Arc::new(Mutex::new(obj));
    async fn handle(State(s): State<Arc<Mutex<Json>>>, method: Method, OriginalUri(uri): OriginalUri, body: Bytes) -> axum::response::Response {
        use axum::response::IntoResponse;
        let path = uri.path();
        let mut o = s.lock().unwrap();
        if !path.starts_with(PATH) {
            return (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"message": "not found"}))).into_response();
        }
        if method == Method::PATCH {
            let patch: Json = serde_json::from_slice(&body).unwrap();
            if path.ends_with("/status") {
                merge_patch(&mut o["status"], &patch["status"]);
            } else {
                let spec = o["spec"].clone();
                merge_patch(&mut o, &patch);
                if o["spec"] != spec {
                    let g = o["metadata"]["generation"].as_u64().unwrap_or(1) + 1;
                    o["metadata"]["generation"] = json!(g);
                }
            }
        }
        axum::Json(o.clone()).into_response()
    }
    let (l, url) = listen().await;
    let app = axum::Router::new().fallback(handle).with_state(state.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (state, url)
}

#[tokio::test(flavor = "multi_thread")]
async fn rules_are_replayed_on_the_servers_load_saved_and_run() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (shard(dir.path(), 0), shard(dir.path(), 1));
    let end = strata_server::telemetry::now_ms() / MIN * MIN;
    // Shard 0 the busier: 50% busy, but 95% for two hours from the second.
    minutes(a.log_store.as_ref().unwrap(), 0, end, |m| if (120..240).contains(&m) { 0.95 } else { 0.5 });
    minutes(b.log_store.as_ref().unwrap(), 1, end, |_| 0.3);
    let urls = vec![
        { let (l, u) = listen().await; tokio::spawn(server::serve_grpc(a.clone(), l, Default::default())); u },
        { let (l, u) = listen().await; tokio::spawn(server::serve_grpc(b.clone(), l, Default::default())); u },
    ];
    let c = Coordinator::connect(urls).await.unwrap();
    let schema = c.schema.clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut coord = Service::new(Backend::Cluster(Arc::new(c)), schema, agent, 16, Duration::from_secs(10));
    // The coordinator's own minutes are not a shard's.
    let own = LogStore::open(LogOptions { capture: false, ..LogOptions::new(dir.path().join("coord.logs"), "coord".into()) }).unwrap();
    own.write_loads(&[Load { ts_ms: end - 100 * MIN, role: "coordinator".into(), shard: None, qps: 1.0, cpu: 4.0, cores: 4, p99_ms: None, disk_used_ratio: Some(0.99) }]).unwrap();
    coord.log_store = Some(own);
    let cluster = json!({"apiVersion": "strata.chloetech.ai/v1alpha1", "kind": "StrataCluster",
        "metadata": {"name": "orders", "namespace": "db", "generation": 1, "annotations": {}},
        "spec": {"shards": 2, "replicas": 3, "demo": N, "storage": {"size": "10Gi"}}, "status": {"phase": "Ready", "observedGeneration": 1}});
    let (api, api_url) = api_server(cluster).await;
    coord.kube = Some(Link::with("db/orders", &Options { api: Some(api_url.clone()), ..Default::default() }).unwrap());
    let base = { let (l, u) = listen().await; tokio::spawn(server::serve_http(Arc::new(coord), l, Default::default())); u };

    // The replay: the StrataCluster's size, the busiest shard's copies.
    let grow = json!({"grow": {"busyAbove": 0.7, "forMinutes": 10, "add": 1, "max": 5}, "cooldownMinutes": 20});
    let (s, r) = post(&base, "/v1/admin/autoscale/replay", json!({"autoscale": grow, "days": 1})).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!((&r["sizes"], &r["managed"], &r["stepMs"]), (&json!({"replicas": 3, "shards": 2, "volume": "10Gi"}), &json!(true), &json!(600_000)), "{}", r["sizes"]);
    let points = r["points"].as_array().unwrap();
    assert!((36..=37).contains(&points.len()), "six hours in ten-minute steps: {}", points.len());
    let calm = &points[3];
    assert!((calm["busy"].as_f64().unwrap() - 0.5).abs() < 1e-6 && (calm["fixed"].as_f64().unwrap() - 0.5).abs() < 1e-6, "shard 0's three copies, the coordinator left out: {calm}");
    assert!(calm["full"].as_f64().unwrap() < 0.5, "the coordinator's volume is not a shard's: {calm}");
    let events = r["events"].as_array().unwrap();
    let grew: Vec<&Json> = events.iter().filter(|e| e["kind"] == "grow").collect();
    assert!(!grew.is_empty() && grew[0]["text"].as_str().unwrap().contains("3 → 4 copies of each shard"), "{events:?}");
    assert!(events.iter().any(|e| e["kind"] == "caught-up"), "{events:?}");
    let sum = &r["summary"];
    assert!(sum["overMinutes"].as_u64().unwrap() < sum["fixedOverMinutes"].as_u64().unwrap(), "{sum}");
    // Two hours, give or take the ten-minute steps at their edges.
    assert!((100..=120).contains(&sum["fixedOverMinutes"].as_u64().unwrap()), "{sum}");
    assert_eq!(r["rules"], "1 copy a shard when its copies are over 70% busy for 10 min, up to 5; 20 min between changes; off (the rules are kept)");
    assert!(r["nodes"].as_array().unwrap().iter().all(|n| n["error"].is_null()), "{}", r["nodes"]);
    // Rules that undo each other, and more than a week, are refused.
    let (s, e) = post(&base, "/v1/admin/autoscale/replay", json!({"autoscale": {"grow": {"busyAbove": 0.5}, "shrink": {"busyBelow": 0.6}}})).await;
    assert!(s == 400 && e["error"].as_str().unwrap().contains("would undo each other"), "{e}");
    let (s, e) = post(&base, "/v1/admin/autoscale/replay", json!({"autoscale": grow, "days": 8})).await;
    assert!(s == 400 && e["error"].as_str().unwrap().contains("at most 7"), "{e}");

    // Saved to the spec, checked as the operator checks it, with its plan.
    let rules = json!({"mode": "suggest", "grow": {"busyAbove": 0.7, "forMinutes": 10, "add": 1, "max": 5}, "cooldownMinutes": 20});
    let (s, plan) = post(&base, "/v1/admin/kube/plan", json!({"spec": {"autoscale": rules}})).await;
    assert_eq!(s, 200, "{plan}");
    assert!(plan["steps"].as_array().unwrap().iter().any(|x| x.as_str().unwrap().starts_with("new autoscaling rules, no restart: 1 copy a shard")), "{plan}");
    let (s, _) = post(&base, "/v1/admin/kube", json!({"spec": {"autoscale": rules}})).await;
    assert_eq!(s, 200);
    assert_eq!(api.lock().unwrap()["spec"]["autoscale"]["mode"], "suggest");
    let (s, e) = post(&base, "/v1/admin/kube", json!({"spec": {"autoscale": {"grow": {"busyAbove": 2}}}})).await;
    assert!(s == 400 && e["error"].as_str().unwrap().contains("is a share"), "{e}");
    // Not a version of the cluster: the rules change no server.
    let saved: Spec = serde_json::from_value(api.lock().unwrap()["spec"].clone()).unwrap();
    assert_eq!(saved.version(), Spec { autoscale: Default::default(), ..saved.clone() }.version());

    // The operator's step, on the live numbers: busy for 15 minutes, a
    // change asked of a person.
    let k = Kube::new(&Options { api: Some(api_url.clone()), ..Default::default() }).unwrap();
    let now = strata_server::telemetry::now_ms();
    let history = json!([{"version": 1, "atMs": now - 120 * MIN}]);
    let out = Outcome { phase: Phase::Ready, message: "ready".into(), shards: Json::Null, rollout: Rollout::default(), numbers: Some(Numbers { busy: Some(0.9), disk_used_ratio: Some(0.4), ..Default::default() }) };
    let step = |status: Json| {
        let (k, out, history, api) = (k.clone(), out.clone(), history.clone(), api.clone());
        async move {
            let c = {
                let mut o = api.lock().unwrap();
                if !status.is_null() {
                    o["status"]["autoscale"] = status;
                }
                o.clone()
            };
            let spec: Spec = serde_json::from_value(c["spec"].clone()).unwrap();
            let st = kube::autoscaled(&k, &c, &spec, &out, &history, strata_server::telemetry::now_ms()).await;
            api.lock().unwrap()["status"]["autoscale"] = st.clone();
            st
        }
    };
    let st = step(json!({"busySinceMs": now - 15 * MIN})).await;
    let sg = st["suggestion"].clone();
    assert_eq!((&sg["rule"], &sg["patch"]), (&json!("grow"), &json!({"replicas": 4})), "{st}");
    assert!(st["log"][0]["text"].as_str().unwrap().starts_with("suggested: the copies were 90% busy for 10 min"), "{st}");
    assert_eq!(api.lock().unwrap()["spec"]["replicas"], 3, "asked, not made");
    // Dismissed through the coordinator: gone at the next step, noted.
    let (s, j) = post(&base, "/v1/admin/kube/autoscale/dismiss", json!({})).await;
    assert_eq!(s, 200, "{j}");
    assert_eq!(api.lock().unwrap()["metadata"]["annotations"][kube::DISMISS], sg["atMs"].to_string());
    let st = step(Json::Null).await;
    assert!(st["suggestion"].is_null() && st["log"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap().starts_with("dismissed: "), "{st}");
    assert!(st["waiting"].as_str().unwrap().contains("have not passed since the last change"), "{st}");
    let (s, e) = post(&base, "/v1/admin/kube/autoscale/dismiss", json!({})).await;
    assert!(s == 400 && e["error"].as_str().unwrap().contains("suggests nothing now"), "{e}");

    // Acting: the operator changes the spec itself, as the autoscaler.
    post(&base, "/v1/admin/kube", json!({"spec": {"autoscale": {"mode": "on"}}})).await;
    let st = step(json!({"busySinceMs": now - 15 * MIN, "dismissedMs": null})).await;
    let o = api.lock().unwrap().clone();
    assert_eq!(o["spec"]["replicas"], 4, "{st}");
    let by = o["metadata"]["annotations"][kube::CHANGED_BY].as_str().unwrap();
    let made: Spec = serde_json::from_value(o["spec"].clone()).unwrap();
    assert_eq!(by, format!("{} {}", kube::AUTOSCALER, made.version()));
    assert!(st["log"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap().starts_with("made: the copies were 90% busy"), "{st}");
    // Off: nothing changes, and it says what it would do.
    post(&base, "/v1/admin/kube", json!({"spec": {"autoscale": {"mode": "off"}, "replicas": 3}})).await;
    let st = step(json!({"busySinceMs": now - 15 * MIN, "lastChangeMs": null})).await;
    assert!(st["waiting"].as_str().unwrap().starts_with("autoscaling is off; on, it would now make this change"), "{st}");
    assert_eq!(api.lock().unwrap()["spec"]["replicas"], 3);

    // A server on its own replays its own minutes.
    let one = { let (l, u) = listen().await; tokio::spawn(server::serve_http(a.clone(), l, Default::default())); u };
    let (s, r) = post(&one, "/v1/admin/autoscale/replay", json!({"autoscale": grow, "days": 1, "sizes": {"replicas": 3}})).await;
    assert_eq!(s, 200, "{r}");
    assert_eq!((&r["sizes"]["replicas"], &r["managed"]), (&json!(3), &json!(false)));
    assert!(r["events"].as_array().unwrap().iter().any(|e| e["kind"] == "grow"), "{r}");
}
