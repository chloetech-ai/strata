//! JSON fields through a coordinator: a table with a JSON field is created
//! on every shard, path conditions reach the shards in plans (as
//! `json_extract` calls, plan feature "json") and filter there from the
//! field's terms, paths in GROUP BY, aggregates and ORDER BY are read from
//! the shards' terms (plan feature "json_columns"), each shard's groups or
//! rows merged by value, and paths of the rows returned from their objects.
//! Each answer is the one database's of the same rows.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use strata::db::{Db, SqlResult};
use strata::query::paths::Call;
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
        // Read from the shards' terms: each shard's groups and partial
        // aggregates, merged by value.
        "SELECT attrs.color, COUNT(*) FROM items GROUP BY attrs.color ORDER BY COUNT(*) DESC, attrs.color LIMIT 2",
        "SELECT attrs.voltage, COUNT(*), MIN(attrs.dims.w) FROM items GROUP BY attrs.voltage HAVING COUNT(*) > 3",
        "SELECT COUNT(*), AVG(attrs.voltage), MIN(attrs.voltage), MAX(attrs.voltage), SUM(attrs.dims.w), COUNT(DISTINCT attrs.certs) FROM items",
        "SELECT attrs.certs, name, COUNT(*) FROM items GROUP BY attrs.certs, name",
    ] {
        let want = sorted(local(&one, q));
        assert!(!want.is_empty(), "{q}");
        assert_eq!(sorted(select(&svc, q).await), want, "{q}");
    }
    // Ordered by a path, across shards: merged by the path's column, which
    // a leaf selects for it when the query does not.
    for q in [
        "SELECT id, attrs.dims.w FROM items WHERE attrs.dims.w > 0 ORDER BY attrs.dims.w DESC, id LIMIT 7",
        "SELECT id, attrs.voltage FROM items ORDER BY attrs.voltage, id LIMIT 9 OFFSET 3",
        "SELECT id FROM items WHERE attrs.certs = 'UL' ORDER BY attrs.voltage DESC, id LIMIT 6",
        "SELECT id, attrs.color FROM items ORDER BY attrs.color DESC NULLS FIRST, id LIMIT 5",
    ] {
        let want = local(&one, q);
        assert!(!want.is_empty(), "{q}");
        assert_eq!(select(&svc, q).await, want, "{q}");
    }

    // The paths listing: each shard's, summed, is the one database's, as
    // SQL and over HTTP. A few rows change first: deleted, rewritten.
    for w in ["DELETE FROM items WHERE attrs.color = 'blue' AND name = 'n2'", "UPDATE items SET attrs = '{\"color\": \"teal\", \"tags\": [1, \"a\"]}' WHERE id = 'i007'"] {
        run(&svc, w).await;
        local(&one, w);
    }
    let call = |prefix: Option<&str>, filter: Option<&str>| Call { table: "items".into(), field: "attrs".into(), prefix: prefix.map(String::from), filter: filter.map(String::from) };
    for c in [call(None, None), call(Some("dims"), None), call(Some("certs"), Some("name IN ('n1', 'n2')")), call(None, Some("attrs.voltage > 3"))] {
        let want = one.json_paths(&c, &Params::new()).unwrap();
        assert!(want.paths.len() > 1 || c.prefix.is_some() && !want.paths.is_empty(), "{c:?}: {want:?}");
        let got = svc.json_paths(&c, &Params::new(), &After::Any, 0).await.unwrap_or_else(|e| panic!("{c:?}: {}", e.message));
        assert_eq!(got, want, "{c:?}");
        assert_eq!(select(&svc, &c.sql()).await, local(&one, &c.sql()), "{}", c.sql());
    }
    let paths = one.json_paths(&call(None, None), &Params::new()).unwrap();
    assert!(paths.paths.iter().any(|p| p.path == "voltage" && p.strings > 0 && p.numbers > 0), "{paths:?}");
    assert!(paths.paths.iter().any(|p| p.path == "tags" && p.values.as_ref().is_some_and(|v| v.len() == 2)), "{paths:?}");
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(svc.clone(), l, Access::default()));
    let c = reqwest::Client::new();
    let get = |q: &str| c.get(format!("{url}/v1/tables/items/paths?{q}")).send();
    let j: serde_json::Value = get("field=attrs&path=dims&where=name%20%3D%20%27n1%27").await.unwrap().json().await.unwrap();
    let want = one.json_paths(&call(Some("dims"), Some("name = 'n1'")), &Params::new()).unwrap().to_json();
    assert_eq!((&j["rows"], &j["paths"], &j["truncated"], &j["field"]), (&want["rows"], &want["paths"], &want["truncated"], &serde_json::json!("attrs")), "{j}");
    assert_eq!(j["paths"][0]["keys"], serde_json::json!(["dims"]));
    for (q, status) in [("", 400), ("field=name", 400), ("field=nope", 400), ("field=attrs&path=a..b", 400)] {
        assert_eq!(get(q).await.unwrap().status().as_u16(), status, "{q}");
    }
    assert_eq!(c.get(format!("{url}/v1/tables/nope/paths?field=attrs")).send().await.unwrap().status().as_u16(), 400);
}

/// A JSON field with text through a coordinator: MATCH finds the rows the
/// one database finds, and bm25 scores them with the cluster's statistics
/// as the one database does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_text_of_json_fields_through_a_coordinator_scores_as_one_database() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let urls = vec![serve(create(d, "s0.strata")).await, serve(create(d, "s1.strata")).await, serve(create(d, "s2.strata")).await];
    let c = Arc::new(Coordinator::connect(urls).await.unwrap());
    let schema = c.schema.clone();
    let svc = Arc::new(Service::new(Backend::Cluster(c), schema, agent(), 16, Duration::from_secs(10)));
    let one = create(d, "one.strata");
    let table = "CREATE TABLE notes (id KEYWORD PRIMARY KEY, attrs JSON OPTIONS(text = true))";
    run(&svc, table).await;
    local(&one, table);
    let words = ["zebra", "quokka", "lynx", "otter", "heron"];
    for chunk in (0..180).collect::<Vec<_>>().chunks(30) {
        let values: Vec<String> = chunk
            .iter()
            .map(|&i| {
                let mut m = serde_json::Map::new();
                m.insert("title".into(), format!("{} {}", words[i % 5], words[i * 7 % 5]).into());
                if i % 3 == 0 {
                    m.insert("tags".into(), serde_json::json!([words[i % 4], "plain"]));
                }
                if i % 4 == 0 {
                    m.insert("spec".into(), serde_json::json!({ "note": words[(i / 4) % 5].repeat(1 + i % 2), "n": i }));
                }
                format!("('n{i:03}', '{}')", serde_json::Value::Object(m))
            })
            .collect();
        let insert = format!("INSERT INTO notes (id, attrs) VALUES {}", values.join(", "));
        run(&svc, &insert).await;
        local(&one, &insert);
    }
    let sorted = |mut rows: Vec<Vec<Value>>| {
        rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        rows
    };
    for q in ["SELECT id FROM notes WHERE MATCH(attrs, 'zebra')", "SELECT id FROM notes WHERE MATCH(attrs, 'otter OR plain') AND attrs.spec IS NOT NULL"] {
        let want = sorted(local(&one, q));
        assert!(!want.is_empty(), "{q}");
        assert_eq!(sorted(select(&svc, q).await), want, "{q}");
    }
    let scores = |rows: Vec<Vec<Value>>| -> Vec<(String, i64)> { rows.into_iter().map(|r| (r[0].as_str().unwrap().to_string(), (r[1].as_f64().unwrap() * 1e4).round() as i64)).collect() };
    let q = "SELECT id, score() FROM notes ORDER BY bm25(attrs, 'quokka heron') DESC, id LIMIT 20";
    let want = scores(local(&one, q));
    assert_eq!(want.len(), 20, "{q}");
    assert_eq!(scores(select(&svc, q).await), want, "{q}");
}
