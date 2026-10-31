//! Memory: the heap counted by the server's allocator, allocation profiles
//! (where the bytes are allocated, and which are still held), the bytes a
//! query's steps allocate, and the process's threads.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::service::{AgentConfig, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

// As the server's binary does.
#[global_allocator]
static ALLOC: strata_server::heap::Counting = strata_server::heap::Counting;

const N: usize = 4000;
const BM25: &str = "SELECT id, title FROM products WHERE price > 5 ORDER BY bm25(body, 'laptop phone') DESC LIMIT 50";

/// One profile at a time in the process: the tests take turns.
static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serve() -> (String, Arc<Service>) {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let opts = DbOptions { publish_snapshots: true, segment_max_docs: 500, ..DbOptions::default() };
    let db = Db::create(&dir.path().join("p.strata"), demo::schema(N), opts).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).map(|i| demo::product(i, 7))).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let svc = Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent, 16, Duration::from_secs(60)));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    tokio::spawn(server::serve_http(svc.clone(), l, Default::default()));
    (url, svc)
}

async fn post(base: &str, path: &str, body: Json) -> (u16, String) {
    let r = reqwest::Client::new().post(format!("{base}{path}")).json(&body).send().await.unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

async fn get(base: &str, path: &str) -> Json {
    reqwest::get(format!("{base}{path}")).await.unwrap().json().await.unwrap()
}

fn steps(v: &Json) -> Vec<Json> {
    let mut out = Vec::new();
    for s in v.as_array().into_iter().flatten() {
        out.push(s.clone());
        out.extend(steps(&s["steps"]));
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn the_heap_is_counted_and_a_query_s_steps_carry_what_they_allocated() {
    let (base, svc) = serve().await;
    svc.series.add(svc.sample());
    let m = get(&base, "/v1/profile/memory").await;
    let heap = &m["heap"];
    assert_eq!(heap["installed"], true, "{m}");
    let in_use = heap["in_use_bytes"].as_i64().unwrap();
    // The server's own (the database's data is mapped from its file).
    assert!(in_use > 64 << 10, "{m}");
    assert!(heap["peak_bytes"].as_i64().unwrap() >= in_use, "{m}");
    assert!(heap["allocs"].as_u64().unwrap() >= heap["frees"].as_u64().unwrap(), "{m}");
    if cfg!(target_os = "linux") {
        assert!(m["rss_bytes"].as_u64().unwrap() > 0 && m["peak_rss_bytes"].as_u64().unwrap() >= m["rss_bytes"].as_u64().unwrap() / 2, "{m}");
        assert!(m["threads"].as_u64().unwrap() > 2, "{m}");
    }

    // A profiled query's steps say what they allocated; the query's own
    // figure is its outermost steps'.
    let before = heap["allocated_bytes"].as_u64().unwrap();
    let (status, body) = post(&base, "/v1/query", json!({"sql": BM25, "profile": true})).await;
    assert_eq!(status, 200, "{body}");
    let j: Json = serde_json::from_str(&body).unwrap();
    let st = steps(&j["stats"]["steps"]);
    let counted: Vec<&Json> = st.iter().filter(|s| s["alloc_bytes"].as_u64().is_some()).collect();
    assert!(!counted.is_empty(), "{}", j["stats"]["steps"]);
    let top: u64 = j["stats"]["steps"].as_array().unwrap().iter().filter_map(|s| s["alloc_bytes"].as_u64()).sum();
    assert!(top > 0, "{}", j["stats"]["steps"]);
    for s in &counted {
        assert!(s["allocs"].as_u64().is_some(), "{s}");
        assert!(s["alloc_bytes"].as_u64().unwrap() == 0 || s["allocs"].as_u64().unwrap() > 0, "{s}");
    }
    // Without "profile", steps carry no allocations (nor CPU time).
    let (_, plain) = post(&base, "/v1/query", json!({"sql": BM25})).await;
    let plain: Json = serde_json::from_str(&plain).unwrap();
    assert!(steps(&plain["stats"]["steps"]).iter().all(|s| s["alloc_bytes"].is_null()), "{plain}");

    let after = get(&base, "/v1/profile/memory").await;
    assert!(after["heap"]["allocated_bytes"].as_u64().unwrap() > before, "{after}");
    let recent = get(&base, "/v1/queries/recent").await;
    let q = recent["queries"].as_array().unwrap().iter().find(|q| q["profiled"] == true).cloned().unwrap_or_default();
    assert!(q["alloc_bytes"].as_u64().unwrap_or(0) > 0, "{recent}");

    // The series carries the heap and the allocation rate.
    svc.series.add(svc.sample());
    let ts = get(&base, "/v1/timeseries").await;
    let p = ts["points"].as_array().unwrap().last().cloned().unwrap_or_default();
    assert!(p["heap_bytes"].as_f64().unwrap_or(0.0) > 0.0, "{p}");
    assert!(p["heap_peak_bytes"].as_f64().unwrap_or(0.0) >= p["heap_bytes"].as_f64().unwrap_or(0.0), "{p}");
    assert!(p["alloc_bytes_s"].as_f64().is_some() && p["allocs_s"].as_f64().is_some(), "{p}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_allocation_profile_finds_where_a_query_allocates() {
    let _one = ONE.lock().await;
    let (base, _svc) = serve().await;

    // A query run for a second, sampled about every 16 KB a thread
    // allocates.
    let every = 16 << 10;
    let (status, body) = post(&base, "/v1/profile/heap", json!({"seconds": 1, "every": every, "sql": BM25, "clients": 2})).await;
    assert_eq!(status, 200, "{body}");
    let p: Json = serde_json::from_str(&body).unwrap();
    assert_eq!(p["every"], every);
    let runs = p["runs"]["count"].as_u64().unwrap();
    assert!(runs >= 10, "{}", p["runs"]);
    assert_eq!(p["runs"]["clients"], 2);
    let allocated = p["allocated_bytes"].as_u64().unwrap();
    assert!(allocated > 0 && p["allocs"].as_u64().unwrap() > 0, "{body}");
    let samples = p["samples"].as_u64().unwrap();
    assert!(samples >= 50, "{samples} samples of {allocated} bytes");
    let names: Vec<&str> = p["names"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
    let threads = p["threads"].as_array().unwrap();
    let stacks = p["stacks"].as_array().unwrap();
    assert!(!stacks.is_empty());
    let (mut bytes, mut held) = (0u64, 0u64);
    for s in stacks {
        assert!((s["thread"].as_u64().unwrap() as usize) < threads.len());
        let fs = s["frames"].as_array().unwrap();
        assert!(fs.iter().all(|f| (f.as_u64().unwrap() as usize) < names.len()));
        let (b, u) = (s["bytes"].as_u64().unwrap(), s["in_use"].as_u64().unwrap());
        assert!(u <= b + 1, "{s}");
        assert!(s["allocs"].as_u64().unwrap() > 0 || b == 0, "{s}");
        bytes += b;
        held += u;
    }
    assert!(held <= bytes);
    // The samples stand for the bytes counted: each byte as likely as any
    // other to be sampled.
    let ratio = bytes as f64 / allocated as f64;
    assert!((0.6..1.6).contains(&ratio), "sampled {bytes} for {allocated} counted");
    // Most of it is the query's work, named; the allocator's own frames
    // are not the callers'.
    let strata: u64 = stacks.iter().filter(|s| s["frames"].as_array().unwrap().iter().any(|f| { let n = names[f.as_u64().unwrap() as usize]; n.starts_with("strata::") || n.starts_with("<strata::") })).map(|s| s["bytes"].as_u64().unwrap()).sum();
    assert!(strata * 3 >= bytes, "{strata} of {bytes} bytes in strata: {:?}", &names[..names.len().min(40)]);
    for s in stacks {
        let last = s["frames"].as_array().unwrap().last().map(|f| names[f.as_u64().unwrap() as usize]).unwrap_or("");
        assert!(!last.contains("strata_server::heap::") && !last.starts_with("__rust_alloc") && !last.starts_with("alloc::alloc::"), "{last}");
    }
    let recent = get(&base, "/v1/queries/recent").await;
    assert!(recent["queries"].as_array().unwrap().is_empty(), "the runs are not kept: {recent}");

    // One at a time.
    let long = {
        let base = base.clone();
        tokio::spawn(async move { post(&base, "/v1/profile/heap", json!({"seconds": 1.5})).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(post(&base, "/v1/profile/heap", json!({"seconds": 0.2})).await.0, 409);
    assert_eq!(long.await.unwrap().0, 200);

    // Folded lines, by bytes or by bytes still held.
    let r = reqwest::Client::new().post(format!("{base}/v1/profile/heap?format=folded")).json(&json!({"seconds": 0.5, "every": 4096, "sql": BM25})).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.lines().count() > 0 && text.lines().all(|l| l.rsplit_once(' ').is_some_and(|(stack, n)| !stack.is_empty() && n.parse::<u64>().is_ok_and(|n| n > 0))), "{text}");
    let r = reqwest::Client::new().post(format!("{base}/v1/profile/heap?format=folded&value=in_use")).json(&json!({"seconds": 0.2})).send().await.unwrap();
    assert_eq!(r.status(), 200);

    // Bad inputs.
    for b in [json!({"seconds": 0}), json!({"seconds": 61}), json!({"every": 100}), json!({"every": 1u64 << 40}), json!({"sql": BM25, "clients": 0}), json!({"sql": "SELECT nope FROM products"})] {
        assert_eq!(post(&base, "/v1/profile/heap", b.clone()).await.0, 400, "{b}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_allocation_profile_of_the_whole_server_sees_what_its_clients_do() {
    let _one = ONE.lock().await;
    let (base, _svc) = serve().await;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load = {
        let (base, stop) = (base.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                post(&base, "/v1/query", json!({"sql": BM25})).await;
            }
        })
    };
    let (status, body) = post(&base, "/v1/profile/heap", json!({"seconds": 1, "every": 32 << 10})).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    load.await.unwrap();
    assert_eq!(status, 200, "{body}");
    let p: Json = serde_json::from_str(&body).unwrap();
    assert!(p["runs"].is_null());
    assert!(p["samples"].as_u64().unwrap() > 10, "{body}");
    let names: Vec<&str> = p["names"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
    assert!(names.iter().any(|n| n.starts_with("strata::") || n.starts_with("<strata::")), "{names:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn the_threads_are_listed_with_their_cpu_time() {
    let (base, _svc) = serve().await;
    let j = get(&base, "/v1/profile/threads").await;
    let ts = j["threads"].as_array().unwrap();
    assert!(ts.len() > 2, "{j}");
    assert!(j["at_ms"].as_u64().unwrap() > 0);
    for t in ts {
        assert!(t["tid"].as_u64().unwrap() > 0, "{t}");
        assert!(t["name"].is_string() && t["state"].is_string(), "{t}");
        assert!(t["cpu_s"].as_f64().unwrap() >= 0.0 && t["voluntary"].as_u64().is_some() && t["involuntary"].as_u64().is_some(), "{t}");
    }
    assert!(ts.iter().any(|t| t["name"].as_str().unwrap().starts_with("tokio")), "{j}");
    // The heap's watcher keeps the peak.
    assert!(ts.iter().any(|t| t["name"] == "strata-heap"), "{j}");
}
