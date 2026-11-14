//! The relevance autopilot over HTTP: logged queries that rank by a rank
//! profile, graded in the judgments table, give a proposal with its proof,
//! kept, listed and applied through the server's writer; without an API
//! key, asking Claude to grade the rows no one graded leaves the report as
//! measured and says why.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use chloedb::autopilot;
use chloedb::db::Db;
use chloedb::query::Params;
use chloedb::storage::DbOptions;
use chloedb::value::Value;
use chloedb_server::auth::Access;
use chloedb_server::service::{AgentConfig, Backend, Service};
use chloedb_server::server;
use tokio::net::TcpListener;

const WORDS: usize = 120;
const DOCS: usize = 700;
const QUERIES: usize = 80;
const SQL: &str = "SELECT id FROM docs ORDER BY rank_profile('p', q => :q) LIMIT 10";

fn word(i: usize) -> String {
    format!("w{i:03}")
}

/// Splitmix64, for a corpus that is the same every run.
fn rng(seed: &mut u64) -> usize {
    *seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (z ^ (z >> 31)) as usize
}

async fn post(c: &reqwest::Client, url: &str, body: Json) -> (u16, Json) {
    let r = c.post(url).json(&body).send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Json::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_autopilot_proposes_keeps_and_applies_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::create_empty(&dir.path().join("db"), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap());
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: chloedb_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() };
    let svc = Arc::new(Service::new(Backend::Local(db.clone()), schema, agent, 16, Duration::from_secs(20)));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(svc, l, Access::default()));
    let c = chloedb_server::http_client().build().unwrap();
    let sql_url = format!("{base}/v1/sql");
    let sql = |s: String| post(&c, &sql_url, json!({"sql": s}));

    let (st, j) = sql("CREATE TABLE docs (id KEYWORD PRIMARY KEY, title TEXT, body TEXT)".into()).await;
    assert_eq!(st, 200, "{j}");
    let mut seed = 11u64;
    let mut docs = Vec::new();
    let mut ops = Vec::new();
    for i in 0..DOCS {
        let title: Vec<usize> = (0..2).map(|_| rng(&mut seed) % WORDS).collect();
        let body: Vec<usize> = (0..30).map(|_| rng(&mut seed) % WORDS).collect();
        let text = |ws: &[usize]| ws.iter().map(|w| word(*w)).collect::<Vec<_>>().join(" ");
        ops.push(json!({"op": "upsert", "table": "docs", "doc": {"id": format!("d{i}"), "title": text(&title), "body": text(&body)}}));
        docs.push((title, body));
    }
    let (st, j) = post(&c, &format!("{base}/v1/write"), json!({"ops": ops})).await;
    assert_eq!(st, 200, "{j}");
    let (st, j) = sql("CREATE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY bm25(body, :q) + 0.05 * bm25(title, :q) DESC".into()).await;
    assert_eq!(st, 200, "{j}");

    // Nothing logged yet: the autopilot says so.
    let (st, j) = post(&c, &format!("{base}/v1/relevance/autopilot"), json!({})).await;
    assert_eq!(st, 200, "{j}");
    assert_eq!(j["report"]["verdict"], "no_workload", "{j}");
    assert_eq!(j["proposal"], Json::Null);

    // Logged queries, and their rows graded.
    let (st, j) = sql(autopilot::judgments_sql(autopilot::JUDGMENTS).unwrap()).await;
    assert_eq!(st, 200, "{j}");
    let mut judgments = Vec::new();
    for w in 0..QUERIES {
        let (st, j) = post(&c, &format!("{base}/v1/query"), json!({"sql": SQL, "params": {"q": word(w)}, "log": true})).await;
        assert_eq!(st, 200, "{j}");
        let qid = autopilot::qid_of(SQL, &Params::new().set("q", Value::Str(word(w))));
        for (i, (t, b)) in docs.iter().enumerate() {
            // Some related rows are left ungraded, for Claude to fill.
            let g = if t.contains(&w) { 3.0 } else if b.contains(&w) && i % 2 == 0 { 1.0 } else { continue };
            judgments.push(json!({"op": "upsert", "table": autopilot::JUDGMENTS, "doc": autopilot::judgment_doc(&qid, &word(w), &format!("d{i}"), g, "human", 0)}));
        }
    }
    let (st, j) = post(&c, &format!("{base}/v1/write"), json!({"ops": judgments})).await;
    assert_eq!(st, 200, "{j}");

    // Asking Claude without a key: the report stands, the error is said.
    let (st, j) = post(&c, &format!("{base}/v1/relevance/autopilot"), json!({"judge": true, "keep": false, "max_holes": 5})).await;
    assert_eq!(st, 200, "{j}");
    assert_eq!(j["proposal"], Json::Null);
    let errs = j["judged"]["errors"].as_array().cloned().unwrap_or_default();
    assert!(errs.iter().any(|e| e.as_str().unwrap_or("").contains("API key")), "{j}");
    assert!(options_refused(&c, &base).await);

    let (st, j) = post(&c, &format!("{base}/v1/relevance/autopilot"), json!({"profile": "p", "table": "docs"})).await;
    assert_eq!(st, 200, "{j}");
    let r = &j["report"];
    assert_eq!(r["verdict"], "propose", "{}", serde_json::to_string_pretty(r).unwrap());
    assert!(r["held_out"]["after"].as_f64().unwrap() > r["held_out"]["before"].as_f64().unwrap(), "{r}");
    assert_eq!(r["workload"]["queries"], QUERIES);
    let id = j["proposal"].as_str().unwrap().to_string();

    let r2 = c.get(format!("{base}/v1/relevance/proposals")).send().await.unwrap();
    let list: Json = r2.json().await.unwrap();
    assert_eq!(list["proposals"][0]["id"], json!(id), "{list}");
    assert_eq!(list["proposals"][0]["status"], "open");
    let one: Json = c.get(format!("{base}/v1/relevance/proposals/{id}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(one["proof"]["held_out"], r["held_out"], "{one}");
    assert_eq!(c.get(format!("{base}/v1/relevance/proposals/nope")).send().await.unwrap().status().as_u16(), 404);

    let (st, j) = post(&c, &format!("{base}/v1/relevance/proposals/{id}/apply"), json!({})).await;
    assert_eq!(st, 200, "{j}");
    assert_eq!(j["status"], "applied", "{j}");
    let now = db.snapshot().table("docs").unwrap().rank_profile("p").unwrap().hash();
    assert_eq!(json!(now), r["candidate"]["hash"]);
    let (st, j) = post(&c, &format!("{base}/v1/relevance/proposals/{id}/dismiss"), json!({})).await;
    assert_eq!(st, 400, "{j}");
}

/// Options the autopilot does not know are refused.
async fn options_refused(c: &reqwest::Client, base: &str) -> bool {
    let (st, j) = post(c, &format!("{base}/v1/relevance/autopilot"), json!({"profiles": "p"})).await;
    st == 400 && j.to_string().contains("no option")
}

/// The autopilot's proposals and judgments are written by an admin key
/// only (audit 4, N-C1): a writer could otherwise plant a proposal, or
/// the grades a proposal's verdict is drawn from, for an admin's Apply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_an_admin_writes_the_proposals_and_the_judgments() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::create_empty(&dir.path().join("db"), DbOptions { publish_snapshots: true, ..DbOptions::default() }).unwrap());
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: chloedb_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() };
    let mut svc = Service::new(Backend::Local(db.clone()), schema, agent, 16, Duration::from_secs(20));
    svc.keys = Arc::new(chloedb_server::keys::Keys::open(dir.path().join("keys.json")).unwrap());
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let access = Access { token: Some(chloedb_server::auth::Secret::new("root-token")), ..Default::default() };
    tokio::spawn(server::serve_http(Arc::new(svc), l, access));
    let c = chloedb_server::http_client().build().unwrap();
    let call = |auth: String, path: &'static str, body: Json| {
        let c = c.clone();
        let url = format!("{base}{path}");
        async move {
            let r = c.post(url).bearer_auth(auth).json(&body).send().await.unwrap();
            (r.status().as_u16(), r.json::<Json>().await.unwrap_or(Json::Null))
        }
    };
    let root = "root-token".to_string();
    let key = |scope: &'static str| {
        let call = call.clone();
        let root = root.clone();
        async move {
            let (st, j) = call(root, "/v1/admin/keys", json!({"name": format!("k-{scope}"), "scope": scope})).await;
            assert_eq!(st, 200, "{j}");
            j["secret"].as_str().unwrap().to_string()
        }
    };
    let (writer, admin) = (key("write").await, key("admin").await);
    for create in [autopilot::judgments_sql(autopilot::JUDGMENTS).unwrap(), autopilot::proposals_sql(autopilot::PROPOSALS).unwrap(), "CREATE TABLE notes (id KEYWORD PRIMARY KEY, body TEXT)".into()] {
        let (st, j) = call(root.clone(), "/v1/sql", json!({"sql": create})).await;
        assert_eq!(st, 200, "{j}");
    }
    let planted = json!({"id": "planted", "target": "docs", "profile": "p", "ts": "2026-11-14T00:00:00Z", "status": "open", "verdict": "propose", "gain": 0.9, "statement": "CREATE OR REPLACE RANK PROFILE p ON docs AS SELECT * FROM docs ORDER BY id DESC"});
    let grade = autopilot::judgment_doc("q1", "w001", "d1", 3.0, "human", 0);
    let writes = [
        ("/v1/write", json!({"ops": [{"op": "upsert", "table": autopilot::PROPOSALS, "doc": planted}]})),
        ("/v1/write", json!({"ops": [{"op": "upsert", "table": "notes", "doc": {"id": "n1", "body": "x"}}, {"op": "upsert", "table": autopilot::JUDGMENTS, "doc": grade}]})),
        ("/v1/sql", json!({"sql": format!("UPDATE {} SET status = 'open' WHERE id = 'x'", autopilot::PROPOSALS)})),
        ("/v1/sql", json!({"sql": format!("INSERT OR REPLACE INTO {} (id, qid, row_key, grade) VALUES ('q1#d1', 'q1', 'd1', 3.0)", autopilot::JUDGMENTS)})),
    ];
    for (path, body) in &writes {
        let (st, j) = call(writer.clone(), path, body.clone()).await;
        assert_eq!(st, 403, "{path} {body}: {j}");
        assert_eq!(j["error_code"], "scope_refused", "{j}");
        assert!(j["error"].as_str().unwrap().contains("admin key"), "{j}");
    }
    // Nothing of a refused write was written, and the writer's other
    // tables are its own.
    let (st, j) = call(writer.clone(), "/v1/write", json!({"ops": [{"op": "upsert", "table": "notes", "doc": {"id": "n1", "body": "x"}}]})).await;
    assert_eq!(st, 200, "{j}");
    for (path, body) in &writes {
        let (st, j) = call(admin.clone(), path, body.clone()).await;
        assert_eq!(st, 200, "{path} {body}: {j}");
    }
}
