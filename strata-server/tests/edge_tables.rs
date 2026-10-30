//! An edge table over HTTP, kept up as events arrive: each event adds 1
//! to the weight of the edges between its item and the user's last five,
//! with `add` operations whose `doc` creates an edge's row on its first
//! event. `ppr` over the edge table then ranks as `ppr` over an EDGES
//! field holding the same edges and weights.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata::Schema;
use strata_server::auth::Access;
use strata_server::server;
use strata_server::service::{AgentConfig, Backend, Service};
use tokio::net::TcpListener;

const ITEMS: usize = 120;
const USERS: usize = 15;
const EVENTS: usize = 1500;

const COOC: &str = "CREATE TABLE cooc (id KEYWORD PRIMARY KEY, src KEYWORD NOT NULL REFERENCES items, dst KEYWORD NOT NULL REFERENCES items, w FLOAT DEFAULT 0) OPTIONS(edges = 'src, dst, w')";

fn items() -> Schema {
    Schema::from_json(r#"{"table": "items", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "links", "type": "edges"}]}"#).unwrap()
}

async fn http(dir: &std::path::Path) -> String {
    let db = Db::create(&dir.join("e.strata"), items(), DbOptions { segment_max_docs: 400, memtable_chunk_docs: 128, publish_snapshots: true, ..DbOptions::default() }).unwrap();
    let schema = db.schema().clone();
    let agent = AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() };
    let svc = Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent, 16, Duration::from_secs(20)));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(svc, l, Access::default()));
    url
}

/// A small deterministic generator (the tests need no `rand`).
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) % n as u64) as usize
    }
}

/// The operations of one edge's event: add 1 to its weight, creating the
/// row on its first.
fn edge_op(a: &str, b: &str) -> Json {
    json!({"op": "add", "table": "cooc", "field": "w", "value": 1, "doc": {"id": format!("{a}>{b}"), "src": a, "dst": b}})
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edge_table_kept_up_by_events_ranks_as_an_edges_field() {
    let dir = tempfile::tempdir().unwrap();
    let h = http(dir.path()).await;
    let c = reqwest::Client::new();
    let post = |path: &str, body: Json| {
        let (c, url) = (c.clone(), format!("{h}{path}"));
        async move {
            let r = c.post(url).json(&body).send().await.unwrap();
            (r.status().as_u16(), r.json::<Json>().await.unwrap())
        }
    };
    let (status, a) = post("/v1/sql", json!({"sql": COOC})).await;
    assert_eq!(status, 200, "{a}");
    let items: Vec<Json> = (0..ITEMS).map(|i| json!({"op": "upsert", "doc": {"id": format!("i{i}")}})).collect();
    assert_eq!(post("/v1/write", json!({"ops": items})).await.0, 200);

    // Events in requests of 50, each with its ten edge operations.
    let mut g = Lcg(3);
    let mut last: HashMap<usize, Vec<String>> = HashMap::new();
    let mut weights: BTreeMap<(String, String), f64> = BTreeMap::new();
    let mut ops = Vec::new();
    for e in 0..EVENTS {
        let (u, i) = (g.below(USERS), format!("i{}", g.below(ITEMS)));
        let seen = last.entry(u).or_default();
        for j in seen.iter().filter(|j| **j != i) {
            for (a, b) in [(&i, j), (j, &i)] {
                ops.push(edge_op(a, b));
                *weights.entry((a.clone(), b.clone())).or_insert(0.0) += 1.0;
            }
        }
        seen.retain(|j| *j != i);
        seen.push(i);
        if seen.len() > 5 {
            seen.remove(0);
        }
        if e % 50 == 49 {
            let (status, a) = post("/v1/write", json!({"ops": std::mem::take(&mut ops)})).await;
            assert_eq!(status, 200, "{a}");
        }
    }
    let (_, a) = post("/v1/query", json!({"sql": "SELECT COUNT(*) FROM cooc"})).await;
    assert_eq!(a["rows"][0][0], weights.len(), "{a}");
    let ((x, y), w) = weights.iter().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
    let (_, a) = post("/v1/query", json!({"sql": format!("SELECT w FROM cooc WHERE id = '{x}>{y}'")})).await;
    assert_eq!(a["rows"][0][0].as_f64(), Some(*w), "{a}");
    assert!(*w > 1.0);

    // The same edges in the EDGES field.
    let mut links: BTreeMap<&String, Vec<Json>> = BTreeMap::new();
    for ((a, b), w) in &weights {
        links.entry(a).or_default().push(json!([null, b, w]));
    }
    let ups: Vec<Json> = links.iter().map(|(a, l)| json!({"op": "upsert", "doc": {"id": a, "links": l}})).collect();
    assert_eq!(post("/v1/write", json!({"ops": ups})).await.0, 200);
    for s in ["i1", "i7", "i50"] {
        let q = |g: &str| json!({"sql": format!("SELECT id, score() FROM items ORDER BY ppr({g}, '{s}', eps => 1e-10) DESC LIMIT 200")});
        let (field, table) = (post("/v1/query", q("links")).await.1, post("/v1/query", q("cooc")).await.1);
        let scores = |j: &Json| -> HashMap<String, f64> {
            j["rows"].as_array().unwrap().iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect()
        };
        let (f, t) = (scores(&field), scores(&table));
        assert!(f.len() > 10, "{field}");
        assert_eq!(f.len(), t.len(), "from {s}");
        for (k, v) in &f {
            assert!((v - t[k]).abs() < 1e-6, "from {s}, {k}: {v} against {}", t[k]);
        }
    }

    // An add whose key is not its doc's is refused.
    let mut bad = edge_op("i1", "i2");
    bad["key"] = json!("i3>i4");
    let (status, a) = post("/v1/write", json!({"ops": [bad]})).await;
    assert!(status == 400 && a["error"].as_str().unwrap().contains("differ"), "{a}");
}
