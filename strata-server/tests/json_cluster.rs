//! JSON fields through a coordinator: a table with a JSON field is created
//! on every shard, path conditions reach the shards in plans (as
//! `json_extract` calls, plan feature "json") and filter there from the
//! field's terms, and paths in SELECT, ORDER BY and GROUP BY are read from
//! the rows the shards return. Each answer is the one database's of the
//! same rows.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata_server::auth::Access;
use strata_server::coord::Coordinator;
use strata_server::keys::Principal;
use strata_server::service::{AgentConfig, After, Backend, QueryReq, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

const TABLE: &str = "CREATE TABLE items (id KEYWORD PRIMARY KEY, name KEYWORD, attrs JSON)";

fn create(dir: &Path, name: &str) -> Arc<Db> {
    let db = Db::create(&dir.join(name), demo::schema(10), DbOptions::default()).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    Arc::new(db)
}

async fn serve(db: Arc<Db>) -> String {
    let schema = (*db.schema()).clone();
    let svc = Arc::new(Service::new(Backend::Local(db), schema, agent(), 16, Duration::from_secs(10)));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc, l, Access::default()));
    url
}

async fn run(svc: &Arc<Service>, s: &str) {
    svc.sql(&Principal::open(), s, &Params::new(), After::Any, 0).await.unwrap_or_else(|e| panic!("{s}: {}", e.message));
}

async fn select(svc: &Arc<Service>, sql: &str) -> Vec<Vec<Value>> {
    svc.query(QueryReq { sql: sql.into(), ..Default::default() }).await.unwrap_or_else(|e| panic!("{sql}: {}", e.message)).rs.rows
}

fn local(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

/// Row `i`'s object: a few sparse keys of mixed types.
fn object(i: usize) -> String {
    let mut m = serde_json::Map::new();
    if i % 3 == 0 {
        m.insert("color".into(), ["red", "blue", "black"][i % 9 / 3].into());
    }
    if i % 4 == 0 {
        m.insert("voltage".into(), if i % 20 == 0 { "n/a".into() } else { serde_json::json!(i % 13) });
    }
    if i % 2 == 0 {
        m.insert("certs".into(), serde_json::json!(["CE", "UL", "FCC"][..1 + i % 3]));
    }
    if i % 7 == 0 {
        m.insert("dims".into(), serde_json::json!({ "w": (i % 11) as f64 + 0.5 }));
    }
    serde_json::Value::Object(m).to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paths_through_a_coordinator_answer_as_one_database() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let urls = vec![serve(create(d, "s0.strata")).await, serve(create(d, "s1.strata")).await];
    let c = Arc::new(Coordinator::connect(urls).await.unwrap());
    let schema = c.schema.clone();
    let svc = Arc::new(Service::new(Backend::Cluster(c), schema, agent(), 16, Duration::from_secs(10)));
    let one = create(d, "one.strata");
    run(&svc, TABLE).await;
    local(&one, TABLE);
    for chunk in (0..240).collect::<Vec<_>>().chunks(40) {
        let values: Vec<String> = chunk.iter().map(|i| format!("('i{i:03}', 'n{}', '{}')", i % 4, object(*i))).collect();
        let insert = format!("INSERT INTO items (id, name, attrs) VALUES {}", values.join(", "));
        run(&svc, &insert).await;
        local(&one, &insert);
    }
    let sorted = |mut rows: Vec<Vec<Value>>| {
        rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        rows
    };
    for q in [
        "SELECT id FROM items WHERE attrs.color = 'red'",
        "SELECT id FROM items WHERE attrs.voltage > 5 AND attrs.certs = 'UL'",
        "SELECT id FROM items WHERE NOT (attrs.voltage > 5) OR attrs.dims.w BETWEEN 2 AND 6",
        "SELECT id FROM items WHERE attrs.voltage = 'n/a' OR attrs['color'] IN ('blue', 'black')",
        "SELECT id FROM items WHERE attrs.color IS NULL AND attrs.dims IS NOT NULL",
        "SELECT id FROM items WHERE attrs.color LIKE 'bl%' AND name <> 'n1'",
        "SELECT id, attrs.color, attrs.dims.w FROM items WHERE attrs.certs = 'CE'",
        "SELECT attrs.color, COUNT(*) FROM items GROUP BY attrs.color",
        "SELECT name, MAX(attrs.dims.w), COUNT(attrs.voltage) FROM items GROUP BY name",
    ] {
        let want = sorted(local(&one, q));
        assert!(!want.is_empty(), "{q}");
        assert_eq!(sorted(select(&svc, q).await), want, "{q}");
    }
    // Ordered by a path, across shards.
    let q = "SELECT id, attrs.dims.w FROM items WHERE attrs.dims.w > 0 ORDER BY attrs.dims.w DESC, id LIMIT 7";
    assert_eq!(select(&svc, q).await, local(&one, q), "{q}");
}
