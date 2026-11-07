//! A coordinator in a shard server's process (`--coordinator-listen`)
//! calls that shard's gRPC service in-process ([`CoordOptions::local`]):
//! it needs no connection to it, and answers, writes and fails as a
//! coordinator reaching every shard over the network does.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use strata::db::Db;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata_server::auth::Access;
use strata_server::coord::{CoordOptions, Coordinator, LocalShard};
use strata_server::keys::Principal;
use strata_server::service::{shard_of, After, AgentConfig, Backend, QueryReq, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

const N: usize = 300;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn key(i: usize) -> Value {
    Value::Str(format!("p{i:07}"))
}

/// Shard `s` of 2 of the demo products, as `strata-server --db` serves it.
fn shard(dir: &Path, s: usize) -> Arc<Service> {
    let opts = DbOptions { publish_snapshots: true, ..DbOptions::default() };
    let db = Db::create(&dir.join(format!("s{s}.strata")), demo::schema(N), opts).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).filter(|&i| shard_of(&key(i), 2) == s).map(|i| demo::product(i, 7))).unwrap();
    let schema = (*db.schema()).clone();
    Arc::new(Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(10)))
}

/// Serves `svc` over gRPC on a port of its own; answers its URL.
async fn serve(svc: &Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc.clone(), l, Access::default()));
    url
}

async fn coordinator(urls: &[String], local: Option<LocalShard>) -> Arc<Service> {
    let c = Arc::new(Coordinator::connect_with(urls.to_vec(), CoordOptions { local, ..Default::default() }).await.unwrap());
    let schema = c.schema.clone();
    Arc::new(Service::new(Backend::Cluster(c), schema, agent(), 16, Duration::from_secs(10)))
}

async fn rows(svc: &Arc<Service>, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    // :v, for the vector queries: a unit vector of the products' dimension.
    let v: Vec<f32> = (0..demo::DIM).map(|i| if i % 3 == 0 { 0.5 } else { -0.25 }).collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let params = Params::new().set("v", Value::Vector(v.iter().map(|x| x / norm).collect::<Vec<_>>().into()));
    svc.query(QueryReq { sql: sql.into(), params, ..Default::default() }).await.map(|a| a.rs.rows).map_err(|e| e.message)
}

/// Shard 1 has no listener at all: nothing answers at its URL, and the
/// coordinator reaches it in-process only, for reads by key, queries of
/// both rounds and writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_own_shard_needs_no_connection() {
    let dir = tempfile::tempdir().unwrap();
    let (s0, s1) = (shard(dir.path(), 0), shard(dir.path(), 1));
    let url0 = serve(&s0).await;
    // A port nothing listens on: a call over the network there is refused.
    let url1 = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let local = LocalShard { url: url1.clone(), service: server::grpc_service(s1.clone(), &Access::default()) };
    let coord = coordinator(&[url0, url1], Some(local)).await;

    let all = coord.get_rows(Some("products"), (0..N).map(key).collect(), None, &After::Any, 0, true).await.unwrap();
    assert!(all.rows.iter().all(Option::is_some), "every key is found, those of shard 1 in-process");
    assert_eq!(rows(&coord, "SELECT COUNT(*) FROM products").await.unwrap(), vec![vec![Value::Int(N as i64)]]);
    // BM25 asks both shards for term statistics, then queries them.
    let top = rows(&coord, "SELECT id FROM products ORDER BY bm25(body, 'kettle') DESC LIMIT 5").await.unwrap();
    assert!(!top.is_empty());

    // A write to each shard, read back through the coordinator and from the shard itself.
    let (k0, k1) = ((N..).find(|&i| shard_of(&key(i), 2) == 0).unwrap(), (N..).find(|&i| shard_of(&key(i), 2) == 1).unwrap());
    for k in [k0, k1] {
        let s = format!("INSERT INTO products (id, title, price) VALUES ('{}', 'in-process kettle', 1.5)", key(k).as_str().unwrap());
        coord.sql(&Principal::open(), &s, &Params::new(), After::Any, 0).await.unwrap_or_else(|e| panic!("{s}: {}", e.message));
    }
    let got = coord.get_rows(Some("products"), vec![key(k0), key(k1)], None, &After::Any, 0, true).await.unwrap();
    assert!(got.rows.iter().all(Option::is_some));
    let mine = rows(&s1, &format!("SELECT title FROM products WHERE id = '{}'", key(k1).as_str().unwrap())).await.unwrap();
    assert_eq!(mine, vec![vec![Value::Str("in-process kettle".into())]], "shard 1 holds its write");
}

/// Two coordinators over the same two shards, both served over gRPC: one
/// reaching every shard over the network, one reaching shard 1 in-process.
/// They answer alike, errors among them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_process_answers_as_the_network_does() {
    let dir = tempfile::tempdir().unwrap();
    let (s0, s1) = (shard(dir.path(), 0), shard(dir.path(), 1));
    let urls = vec![serve(&s0).await, serve(&s1).await];
    let net = coordinator(&urls, None).await;
    let local = LocalShard { url: urls[1].clone(), service: server::grpc_service(s1.clone(), &Access::default()) };
    let near = coordinator(&urls, Some(local)).await;
    for sql in [
        "SELECT id, price FROM products ORDER BY bm25(body, 'kettle steel') DESC LIMIT 10",
        "SELECT id FROM products ORDER BY similarity(emb, :v) DESC LIMIT 10",
        "SELECT id FROM products ORDER BY rrf(bm25(body, 'kettle'), similarity(emb, :v)) DESC LIMIT 10",
        "SELECT category, COUNT(*), AVG(price) FROM products GROUP BY category ORDER BY category",
        "SELECT id, title FROM products WHERE price > 50 ORDER BY id LIMIT 20 OFFSET 5",
        "SELECT id FROM products WHERE id IN ('p0000003', 'p0000004', 'p0000005', 'nope') ORDER BY id",
        // Errors: the shards' own, and the coordinator's.
        "SELECT nope FROM products",
        "SELECT id FROM nowhere",
    ] {
        assert_eq!(rows(&near, sql).await, rows(&net, sql).await, "{sql}");
    }
    let keys: Vec<Value> = (0..N).step_by(7).map(key).chain([Value::Str("nope".into())]).collect();
    let (a, b) = (
        near.get_rows(Some("products"), keys.clone(), Some(vec!["title".into(), "price".into()]), &After::Any, 0, true).await.unwrap(),
        net.get_rows(Some("products"), keys, Some(vec!["title".into(), "price".into()]), &After::Any, 0, true).await.unwrap(),
    );
    assert_eq!((a.columns, a.rows), (b.columns, b.rows));
}
