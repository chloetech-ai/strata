//! Every table of a database, over HTTP, over gRPC and through a
//! coordinator of three shards: the sequences schema of the media and
//! sequences design (items, users, and their events placed on shards by
//! user, `shard_by`), answering as one database does.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::Schema;
use strata_server::auth::Access;
use strata_server::keys::Principal;
use strata_server::coord::Coordinator;
use strata_server::replication::Follow;
use strata_server::service::{After, AgentConfig, Backend, QueryReq, Replication, Service, SqlAnswer};
use strata_server::{json as js, pb, server, slots};
use tokio::net::TcpListener;

const ITEMS: usize = 300;
const USERS: usize = 40;
const EVENTS: usize = 2400;

const USERS_DDL: &str = "CREATE TABLE users (id KEYWORD PRIMARY KEY, n_events INT DEFAULT 0, country KEYWORD)";
const EVENTS_DDL: &str =
    "CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL REFERENCES users, item KEYWORD REFERENCES items, action KEYWORD, ts INT) OPTIONS(shard_by = 'usr')";

fn items() -> Schema {
    Schema::from_json(
        r#"{"table": "items", "key": "id", "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "title", "type": "text"},
            {"name": "cat", "type": "keyword"},
            {"name": "price", "type": "float"},
            {"name": "cooc", "type": "edges"}]}"#,
    )
    .unwrap()
}

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn local(dir: &std::path::Path, name: &str) -> Arc<Service> {
    let db = Db::create(&dir.join(name), items(), DbOptions { segment_max_docs: 500, publish_snapshots: true, ..DbOptions::default() }).unwrap();
    let schema = (*db.schema()).clone();
    Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(20)))
}

fn cluster(c: Coordinator) -> Arc<Service> {
    let schema = c.schema.clone();
    Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(20)))
}

async fn grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc, l, Access::default()));
    url
}

async fn http(svc: Arc<Service>) -> String {
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

const WORDS: [&str; 12] = ["red", "blue", "tent", "lamp", "boot", "cord", "mug", "kettle", "chair", "rope", "map", "stove"];

/// The rows, as the write operations and statements that make them: items
/// by operations of the first table (which name none), users by SQL, and
/// events by operations of their table, each adding to its user's count.
fn rows() -> (Vec<Json>, Vec<String>, Vec<Vec<Json>>) {
    let mut g = Lcg(7);
    let items = (0..ITEMS)
        .map(|i| {
            let title = format!("{} {} {}", WORDS[g.below(12)], WORDS[g.below(12)], WORDS[i % 12]);
            let links: Vec<String> = (0..g.below(4)).map(|_| format!("i{}", g.below(ITEMS))).collect();
            json!({"op": "upsert", "doc": {"id": format!("i{i}"), "title": title, "cat": format!("c{}", i % 7), "price": (i * 37 % ITEMS) as f64 + 0.5, "cooc": links}})
        })
        .collect();
    let users = (0..USERS).collect::<Vec<_>>().chunks(10).map(|c| format!("INSERT INTO users (id, country) VALUES {}", c.iter().map(|u| format!("('u{u}', 'k{}')", u % 3)).collect::<Vec<_>>().join(", "))).collect();
    let events = (0..EVENTS)
        .collect::<Vec<_>>()
        .chunks(200)
        .map(|c| {
            c.iter()
                .flat_map(|&e| {
                    let usr = format!("u{}", g.below(USERS));
                    let (item, action, ts) = (format!("i{}", g.below(ITEMS)), ["view", "click", "buy"][g.below(3)], g.below(5000));
                    let ev = json!({"op": "insert", "table": "events", "doc": {"id": e, "usr": usr, "item": item, "action": action, "ts": ts}});
                    [ev, json!({"op": "add", "table": "users", "key": usr, "field": "n_events", "value": 1})]
                })
                .collect()
        })
        .collect();
    (items, users, events)
}

async fn load(svc: &Arc<Service>) {
    let (items, users, events) = rows();
    svc.write_ops(items, None).await.unwrap();
    for sql in users {
        svc.sql(&Principal::open(), &sql, &Params::new(), After::Any, 0).await.unwrap();
    }
    for ops in events {
        svc.write_ops(ops, None).await.unwrap_or_else(|e| panic!("{}", e.message));
    }
}

async fn sql(svc: &Arc<Service>, s: &str) -> Result<SqlAnswer, String> {
    svc.sql(&Principal::open(), s, &Params::new(), After::Any, 0).await.map_err(|e| e.message)
}

async fn query(svc: &Arc<Service>, s: &str) -> Json {
    let a = svc.query(QueryReq { sql: s.into(), ..Default::default() }).await.unwrap_or_else(|e| panic!("{s}: {}", e.message));
    js::result(&a.rs, 0.0)
}

/// Queries of every table, and across them: those whose order is total
/// (`true`) answer in the same order, the others with the same rows.
const QUERIES: [(&str, bool); 19] = [
    ("SELECT item, action, ts FROM events WHERE usr = 'u3' ORDER BY ts DESC, id LIMIT 50", true),
    ("SELECT COUNT(*) FROM events", true),
    ("SELECT usr, COUNT(*) FROM events GROUP BY usr LIMIT 100", false),
    ("SELECT action, COUNT(*), MAX(ts) FROM events WHERE ts > 2500 GROUP BY action", false),
    ("SELECT id, n_events FROM users WHERE n_events > 60 LIMIT 100", false),
    ("SELECT country, SUM(n_events) FROM users GROUP BY country", false),
    ("SELECT id, price FROM items WHERE id IN (SELECT item FROM events WHERE usr = 'u5' AND action = 'buy') ORDER BY price DESC LIMIT 100", true),
    ("SELECT e.id, e.ts, i.title FROM events e JOIN items i ON e.item = i.id WHERE e.usr = 'u7' ORDER BY e.ts DESC, e.id LIMIT 20", true),
    ("SELECT u.country, COUNT(*) FROM events e JOIN users u ON e.usr = u.id WHERE e.action = 'buy' GROUP BY u.country", false),
    ("WITH h AS (SELECT item FROM events WHERE usr = 'u9' ORDER BY ts DESC LIMIT 30) SELECT id, cat FROM items WHERE id IN (SELECT item FROM h) AND cat <> 'c1' ORDER BY price LIMIT 30", true),
    ("SELECT id, score() FROM items WHERE MATCH(title, 'kettle') ORDER BY bm25(title, 'kettle') DESC LIMIT 10", true),
    // The first row of each group: of a column that places rows, of one
    // that does not, by a score, and over groups.
    ("SELECT DISTINCT ON (usr) usr, id, ts FROM events ORDER BY ts DESC, id LIMIT 20", true),
    ("SELECT DISTINCT ON (item) item, id, ts FROM events WHERE action = 'buy' ORDER BY ts DESC LIMIT 15", true),
    ("SELECT DISTINCT ON (cat) id, cat, score() FROM items WHERE MATCH(title, 'kettle rope') ORDER BY bm25(title, 'kettle rope') DESC LIMIT 5", true),
    ("SELECT DISTINCT ON (action) action, usr, COUNT(*) AS n FROM events GROUP BY action, usr ORDER BY action, n DESC, usr", true),
    // At most k rows of each group (MAX k PER).
    ("SELECT usr, id, ts FROM events ORDER BY ts DESC, id MAX 2 PER usr LIMIT 20", true),
    ("SELECT item, id, ts FROM events WHERE action = 'buy' ORDER BY ts DESC, id MAX 3 PER item LIMIT 15 OFFSET 2", true),
    ("SELECT id, cat, score() FROM items WHERE MATCH(title, 'kettle rope') ORDER BY bm25(title, 'kettle rope') DESC MAX 2 PER cat LIMIT 8", true),
    ("SELECT e.id, e.ts, i.cat FROM events e JOIN items i ON e.item = i.id ORDER BY e.ts DESC, e.id MAX 2 PER i.cat LIMIT 12", true),
];

/// The same rows: in the same order when `ordered`, but for rows tied on
/// score, which may swap.
fn same(sql: &str, ordered: bool, want: &Json, got: &Json) {
    assert_eq!(want["columns"], got["columns"], "{sql}");
    let (w, g) = (want["rows"].as_array().unwrap(), got["rows"].as_array().unwrap());
    assert!(!w.is_empty(), "{sql}: finds rows");
    assert_eq!(w.len(), g.len(), "{sql}: rows");
    let sorted = |rows: &[Json]| {
        let mut k: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
        k.sort();
        k
    };
    let scores = want["scores"].as_array().filter(|s| !s.is_empty());
    match (ordered, scores) {
        (false, _) => assert_eq!(sorted(w), sorted(g), "{sql}"),
        (true, None) => assert_eq!(w, g, "{sql}"),
        // Runs of rows tied on score.
        (true, Some(s)) => {
            let mut i = 0;
            while i < w.len() {
                let mut j = i + 1;
                while j < w.len() && (s[j].as_f64().unwrap() - s[i].as_f64().unwrap()).abs() < 1e-9 {
                    j += 1;
                }
                let keys = |rows: &[Json]| sorted(&rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>());
                assert_eq!(keys(&w[i..j]), keys(&g[i..j]), "{sql}: rows {i}..{j}");
                i = j;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn three_shards_answer_like_one_database() {
    let dir = tempfile::tempdir().unwrap();
    let whole = local(dir.path(), "whole.strata");
    let shards: Vec<Arc<Service>> = (0..3).map(|s| local(dir.path(), &format!("s{s}.strata"))).collect();
    let mut urls = Vec::new();
    for s in &shards {
        urls.push(grpc(s.clone()).await);
    }
    let coord = cluster(Coordinator::connect(urls.clone()).await.unwrap());
    // The tables are made through the coordinator, on every shard.
    for svc in [&whole, &coord] {
        for ddl in [USERS_DDL, EVENTS_DDL] {
            sql(svc, ddl).await.unwrap();
        }
    }
    let names = |svc: &Arc<Service>| svc.catalog().iter().map(|t| t.table.clone()).collect::<Vec<_>>();
    assert_eq!(names(&coord), ["items", "users", "events"]);
    for s in &shards {
        assert_eq!(names(s), ["items", "users", "events"]);
    }
    load(&whole).await;
    load(&coord).await;
    for (q, ordered) in QUERIES {
        same(q, ordered, &query(&whole, q).await, &query(&coord, q).await);
    }

    // A user's events are on one shard: the one holding their slot.
    let map = slots::SlotMap::even(3, 1);
    let mut held = 0;
    for (i, s) in shards.iter().enumerate() {
        let rows = query(s, "SELECT usr, COUNT(*) FROM events GROUP BY usr LIMIT 1000").await;
        for r in rows["rows"].as_array().unwrap() {
            let usr = strata::Value::Str(r[0].as_str().unwrap().into());
            assert_eq!(map.shard_of(&usr), i, "{usr:?} is on shard {i}");
            held += r[1].as_i64().unwrap();
        }
    }
    assert_eq!(held, EVENTS as i64);

    // An operation by key on a table placed by a column names the column's
    // value, so that the coordinator finds the row's shard.
    let usr = query(&whole, "SELECT usr FROM events WHERE id = 17").await["rows"][0][0].as_str().unwrap().to_string();
    let e = coord.write_ops(vec![json!({"op": "delete", "table": "events", "key": 17})], None).await.unwrap_err();
    assert!(e.status == 400 && e.message.contains("\"routing\""), "{}", e.message);
    let other = if usr == "u0" { "u1" } else { "u0" };
    let w = coord.write_ops(vec![json!({"op": "delete", "table": "events", "key": 17, "routing": other})], None).await.unwrap();
    // u0's shard may be the row's too.
    let elsewhere = map.shard_of(&strata::Value::Str(other.into())) != map.shard_of(&strata::Value::Str(usr.clone()));
    assert_eq!(w.rows, !elsewhere as u64);
    let w = coord.write_ops(vec![json!({"op": "delete", "table": "events", "key": 17, "routing": usr})], None).await.unwrap();
    assert_eq!(w.rows, elsewhere as u64);
    whole.write_ops(vec![json!({"op": "delete", "table": "events", "key": 17})], None).await.unwrap();
    // SQL deletes reach every shard.
    for svc in [&whole, &coord] {
        sql(svc, "DELETE FROM events WHERE action = 'view' AND ts < 1000").await.unwrap();
    }
    for (q, ordered) in QUERIES {
        same(q, ordered, &query(&whole, q).await, &query(&coord, q).await);
    }
    // The column that places a row is not changed in place.
    for svc in [&whole, &coord] {
        let e = sql(svc, "UPDATE events SET usr = 'u1' WHERE id = 20").await.err().unwrap();
        assert!(e.contains("shard_by"), "{e}");
    }
    // A shard refuses a row another shard's slots hold, named by its
    // placing column.
    let (u, theirs) = (0..USERS).map(|u| format!("u{u}")).map(|u| (u.clone(), map.shard_of(&strata::Value::Str(u)))).find(|(_, s)| *s != 0).unwrap();
    let e = shards[0].write_ops(vec![json!({"op": "insert", "table": "events", "doc": {"id": 99999, "usr": u, "ts": 1}})], None).await.unwrap_err();
    assert!(e.status == 400 && e.message.contains(&format!("usr \"{u}\" is in slot")) && e.message.contains(&format!("shard {theirs} holds")), "{}", e.message);

    // A coordinator started later serves the tables too.
    let again = cluster(Coordinator::connect(urls.clone()).await.unwrap());
    assert_eq!(names(&again), ["items", "users", "events"]);
    let (q, ordered) = QUERIES[0];
    same(q, ordered, &query(&whole, q).await, &query(&again, q).await);
    // A table made on each shard on its own is learned when a request
    // names it; one on some shards only is not served.
    for s in &shards {
        sql(s, "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)").await.unwrap();
    }
    sql(&shards[0], "CREATE TABLE only0 (id INT PRIMARY KEY)").await.unwrap();
    // The coordinator asks again at most once a second.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    sql(&again, "INSERT INTO notes (id, body) VALUES (1, 'a red tent'), (2, 'a blue lamp'), (3, 'rope')").await.unwrap();
    assert_eq!(query(&again, "SELECT COUNT(*) FROM notes WHERE MATCH(body, 'red OR rope')").await["rows"][0][0], 2);
    let e = again.query(QueryReq { sql: "SELECT id FROM only0 LIMIT 1".into(), ..Default::default() }).await.err().unwrap();
    assert!(e.message.contains("no such table: only0"), "{}", e.message);
    // DROP TABLE through the coordinator drops it on every shard.
    sql(&again, "DROP TABLE notes").await.unwrap();
    assert_eq!(names(&again), ["items", "users", "events"]);
    for s in &shards {
        assert!(!names(s).contains(&"notes".to_string()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_table_over_http_and_grpc() {
    let dir = tempfile::tempdir().unwrap();
    let svc = local(dir.path(), "one.strata");
    let (h, g) = (http(svc.clone()).await, grpc(svc.clone()).await);
    let c = reqwest::Client::new();
    let post = |path: &str, body: Json| {
        let (c, url) = (c.clone(), format!("{h}{path}"));
        async move {
            let r = c.post(url).json(&body).send().await.unwrap();
            (r.status().as_u16(), r.json::<Json>().await.unwrap())
        }
    };
    for ddl in [USERS_DDL, EVENTS_DDL] {
        let (status, a) = post("/v1/sql", json!({"sql": ddl})).await;
        assert_eq!(status, 200, "{a}");
    }
    let tables: Json = c.get(format!("{h}/v1/tables")).send().await.unwrap().json().await.unwrap();
    let names: Vec<&str> = tables["tables"].as_array().unwrap().iter().map(|t| t["table"].as_str().unwrap()).collect();
    assert_eq!(names, ["items", "users", "events"]);
    let events: Json = c.get(format!("{h}/v1/schema?table=EVENTS")).send().await.unwrap().json().await.unwrap();
    assert_eq!((events["table"].as_str(), events["shard_by"].as_str(), events["short"]["shard_by"].as_str()), (Some("events"), Some("usr"), Some("usr")));
    let first: Json = c.get(format!("{h}/v1/schema")).send().await.unwrap().json().await.unwrap();
    assert_eq!((first["table"].as_str(), first["shard_by"].is_null()), (Some("items"), true));
    let r = c.get(format!("{h}/v1/schema?table=nope")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400);

    // Rows of each table: operations that name it, an import, SQL.
    let (items, _, events) = rows();
    assert_eq!(post("/v1/write", json!({"ops": items})).await.0, 200);
    let csv = (0..USERS).map(|u| format!("u{u},k{}\n", u % 3)).collect::<String>();
    let r = c.post(format!("{h}/v1/import?format=csv&table=users")).header("content-type", "text/csv").body(format!("id,country\n{csv}")).send().await.unwrap();
    let a: Json = r.json().await.unwrap();
    assert_eq!(a["written"], USERS, "{a}");
    for ops in &events {
        let (status, a) = post("/v1/write", json!({"ops": ops})).await;
        assert_eq!(status, 200, "{a}");
    }
    let count = |table: &str| {
        let (post, table) = (post.clone(), table.to_string());
        async move { post("/v1/query", json!({"sql": format!("SELECT COUNT(*) FROM {table}")})).await.1["rows"][0][0].as_u64().unwrap() }
    };
    assert_eq!((count("items").await, count("users").await, count("events").await), (ITEMS as u64, USERS as u64, EVENTS as u64));
    let (_, a) = post("/v1/query", json!({"sql": "SELECT SUM(n_events) FROM users"})).await;
    assert_eq!(a["rows"][0][0], EVENTS, "{a}");
    let (status, a) = post("/v1/write", json!({"ops": [{"op": "upsert", "table": "nope", "doc": {"id": 1}}]})).await;
    assert!(status == 400 && a["error"].as_str().unwrap().contains("no such table: nope"), "{a}");
    let (_, a) = post("/v1/explain", json!({"sql": QUERIES[0].0})).await;
    assert!(a["tree"].to_string().contains("events"), "{a}");
    // A walk seeded by a user's history in another table.
    let (_, a) = post("/v1/query", json!({"sql": "SELECT id FROM items WHERE REACHABLE(cooc, (SELECT item FROM events WHERE usr = 'u2' ORDER BY ts DESC LIMIT 10), hops => 0, min_hops => 0) LIMIT 100"})).await;
    let (_, want) = post("/v1/query", json!({"sql": "SELECT id FROM items WHERE id IN (SELECT item FROM events WHERE usr = 'u2' ORDER BY ts DESC LIMIT 10) LIMIT 100"})).await;
    let sorted = |j: &Json| {
        let mut v: Vec<String> = j["rows"].as_array().unwrap().iter().map(|r| r[0].to_string()).collect();
        v.sort();
        v
    };
    assert!(!sorted(&want).is_empty());
    assert_eq!(sorted(&a), sorted(&want));

    // gRPC: every table's schema, a query of one and writes to one.
    let mut client = pb::strata_client::StrataClient::connect(g).await.unwrap();
    let s = client.schema(pb::Empty {}).await.unwrap().into_inner();
    assert_eq!(s.tables_json.len(), 3);
    assert_eq!(serde_json::from_str::<Schema>(&s.tables_json[2]).unwrap().shard_by.as_deref(), Some("usr"));
    let ops = json!([{"op": "upsert", "table": "users", "doc": {"id": "u999", "country": "k9"}}, {"op": "set", "table": "users", "key": "u0", "field": "n_events", "value": 0}]);
    let w = client.write(pb::WriteRequest { ops_json: serde_json::to_vec(&ops).unwrap(), ..Default::default() }).await.unwrap().into_inner();
    assert_eq!(w.written, 2);
    let a = client.query(pb::QueryRequest { sql: "SELECT id, n_events FROM users WHERE country = 'k9' OR id = 'u0' ORDER BY n_events, id".into(), ..Default::default() }).await.unwrap().into_inner();
    assert_eq!(a.rows.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replica_takes_no_tables() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("r.strata"), items(), DbOptions::default()).unwrap();
    db.storage().start_seq_no().unwrap();
    let db = Arc::new(db);
    let mut svc = Service::new(Backend::Local(db.clone()), items(), agent(), 4, Duration::from_secs(5));
    svc.set_replication(Replication::Replica(Arc::new(Follow::open(db, None).unwrap())));
    let svc = Arc::new(svc);
    let e = sql(&svc, USERS_DDL).await.err().unwrap();
    assert!(e.contains("replica"), "{e}");
}
