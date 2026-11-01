//! A coordinator's compound queries on DataFusion take the rows of their
//! leaves from the shards as Arrow columns (shard protocol 10), when it
//! puts the shards' rows one after another: they answer as one database
//! does on the operators, with shards whose columns are all NULL or hold
//! only empty lists, and a shard with no rows of a table.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::exec::Engine;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::Schema;
use strata_server::auth::Access;
use strata_server::coord::Coordinator;
use strata_server::keys::Principal;
use strata_server::service::{After, AgentConfig, Backend, QueryReq, Service};
use strata_server::{json as js, server, slots};
use tokio::net::TcpListener;

const T: i64 = 600;
const U: i64 = 900;

fn first() -> Schema {
    Schema::from_json(
        r#"{"table": "t", "key": "id", "fields": [
            {"name": "id", "type": "int"},
            {"name": "g", "type": "keyword"},
            {"name": "nums", "type": "int", "multi": true},
            {"name": "x", "type": "float"},
            {"name": "note", "type": "keyword"}]}"#,
    )
    .unwrap()
}

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn local(dir: &std::path::Path, name: &str, engine: Engine) -> Arc<Service> {
    let db = Db::create(&dir.join(name), first(), DbOptions { segment_max_docs: 200, publish_snapshots: true, ..DbOptions::default() }).unwrap();
    let schema = (*db.schema()).clone();
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(20));
    svc.engine = engine;
    Arc::new(svc)
}

async fn grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc, l, Access::default()));
    url
}

/// The rows: those of `t` on shard 0 without `nums` (NULL or empty) and
/// without `note`; no rows of `u` on shard 2.
async fn load(svc: &Arc<Service>) {
    let map = slots::SlotMap::even(3, 1);
    svc.sql(&Principal::open(), "CREATE TABLE u (id INT PRIMARY KEY, t_id INT, w INT, tags KEYWORD[])", &Params::new(), After::Any, 0).await.unwrap();
    let t = (0..T)
        .map(|i| {
            let bare = map.shard_of(&Value::Int(i)) == 0;
            let nums = match (bare, i % 2) {
                (true, 0) => json!([]),
                (true, _) => Json::Null,
                (false, _) => json!([i % 5, i % 7]),
            };
            let note = if bare { Json::Null } else { json!(format!("n{}", i % 4)) };
            json!({"op": "upsert", "doc": {"id": i, "g": format!("g{}", i % 9), "nums": nums, "x": (i * 37 % 100) as f64 / 100.0, "note": note}})
        })
        .collect();
    svc.write_ops(t, None).await.unwrap();
    let u = (0..U)
        .filter(|&i| map.shard_of(&Value::Int(i)) != 2)
        .map(|i| json!({"op": "insert", "table": "u", "doc": {"id": i, "t_id": (i * 7) % T, "w": i % 101, "tags": [format!("k{}", i % 3)]}}))
        .collect();
    svc.write_ops(u, None).await.unwrap_or_else(|e| panic!("{}", e.message));
}

/// Compound queries whose leaves read whole tables or filtered ones, with
/// whether their order is total.
const QUERIES: &[(&str, bool)] = &[
    ("SELECT t.g, COUNT(*), SUM(u.w) FROM t JOIN u ON u.t_id = t.id GROUP BY t.g ORDER BY t.g", true),
    ("SELECT t.note, COUNT(*), MAX(t.x) FROM t JOIN u ON u.t_id = t.id GROUP BY t.note ORDER BY t.note", true),
    ("SELECT t.id, t.nums, t.note, u.w FROM t JOIN u ON u.t_id = t.id WHERE u.w > 50 ORDER BY u.w DESC, t.id, u.id LIMIT 40", true),
    ("SELECT u.id, t.nums, u.tags FROM u LEFT JOIN t ON t.id = u.t_id ORDER BY u.id LIMIT 60", true),
    ("WITH j AS (SELECT t.nums AS n, u.w AS w FROM t JOIN u ON u.t_id = t.id) SELECT n, COUNT(*), SUM(w) FROM j GROUP BY n ORDER BY n", true),
    ("SELECT DISTINCT t.note, t.g FROM t JOIN u ON u.t_id = t.id", false),
    ("SELECT id, g, ROW_NUMBER() OVER (PARTITION BY g ORDER BY x DESC, id) AS r FROM t QUALIFY r <= 2 ORDER BY g, r", true),
    ("SELECT t_id FROM u WHERE w < 10 UNION SELECT id FROM t WHERE x > 0.9 ORDER BY t_id", true),
];

fn same(sql: &str, ordered: bool, want: &Json, got: &Json) {
    assert_eq!(want["columns"], got["columns"], "{sql}");
    let (w, g) = (want["rows"].as_array().unwrap(), got["rows"].as_array().unwrap());
    assert!(!w.is_empty(), "{sql}: finds rows");
    let sorted = |rows: &[Json]| {
        let mut k: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
        k.sort();
        k
    };
    match ordered {
        true => assert_eq!(w, g, "{sql}"),
        false => assert_eq!(sorted(w), sorted(g), "{sql}"),
    }
}

/// The answer, and whether a leaf's shards answered it as Arrow columns.
async fn query(svc: &Arc<Service>, sql: &str) -> (Json, bool, Vec<String>) {
    let a = svc.query(QueryReq { sql: sql.into(), profile: true, ..Default::default() }).await.unwrap_or_else(|e| panic!("{sql}: {}", e.message));
    let arrow = a.rs.stats.steps.iter().any(|s| s.op == "merge" && s.detail.ends_with("as Arrow columns"));
    (js::result(&a.rs, 0.0), arrow, a.rs.stats.plan.clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaves_come_from_the_shards_as_arrow_columns() {
    let dir = tempfile::tempdir().unwrap();
    let whole = local(dir.path(), "whole.strata", Engine::Operators);
    let shards: Vec<Arc<Service>> = (0..3).map(|s| local(dir.path(), &format!("s{s}.strata"), Engine::Operators)).collect();
    let mut urls = Vec::new();
    for s in &shards {
        urls.push(grpc(s.clone()).await);
    }
    let coordinator = |engine| {
        let urls = urls.clone();
        async move {
            let c = Coordinator::connect(urls).await.unwrap();
            let schema = c.schema.clone();
            let mut svc = Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(20));
            svc.engine = engine;
            Arc::new(svc)
        }
    };
    let on_df = coordinator(Engine::DataFusion).await;
    load(&whole).await;
    load(&on_df).await;
    let (on_ops, auto) = (coordinator(Engine::Operators).await, coordinator(Engine::Auto).await);
    let mut arrow = 0;
    for &(q, ordered) in QUERIES {
        let (want, ..) = query(&whole, q).await;
        let (got, by_arrow, plan) = query(&on_df, q).await;
        same(q, ordered, &want, &got);
        assert!(plan.iter().any(|l| l.starts_with("datafusion:")), "{q}: {plan:#?}");
        arrow += by_arrow as usize;
        // The operators take the leaves as rows.
        let (got, by_arrow, plan) = query(&on_ops, q).await;
        same(q, ordered, &want, &got);
        assert!(!by_arrow, "{q}: {plan:#?}");
        // `auto`, with leaves this small, answers on the operators, which
        // take the shards' columns as rows.
        let (got, _, plan) = query(&auto, q).await;
        same(q, ordered, &want, &got);
        assert!(!plan.iter().any(|l| l.starts_with("datafusion:")), "{q}: {plan:#?}");
    }
    assert_eq!(arrow, QUERIES.len(), "every query had a leaf answered as Arrow columns");

    // A LIMIT and OFFSET without an order cut the rows the shards answer,
    // one after another, as they do with rows.
    let q = "WITH h AS (SELECT id, g FROM t LIMIT 50 OFFSET 20) SELECT h.g, COUNT(*), MIN(h.id) FROM h JOIN u ON u.t_id = h.id GROUP BY h.g ORDER BY h.g";
    let (want, by_arrow, _) = query(&on_ops, q).await;
    assert!(!by_arrow);
    let (got, by_arrow, _) = query(&on_df, q).await;
    assert!(by_arrow, "{q}");
    same(q, true, &want, &got);
}
