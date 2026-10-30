//! Cross-encoders on servers: a coordinator ranks by one in the global
//! phase, once per query, over the text its fetch round reads of the rows
//! the shards ranked best, so a query answers as on one database; shards
//! need no model; `--max-rerank` caps the rows a model scores.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use strata::db::Db;
use strata::query::{compound, Params};
use strata::storage::DbOptions;
use strata::value::Value;
use strata_server::auth::Access;
use strata_server::coord::Coordinator;
use strata_server::service::{shard_of, AgentConfig, Backend, QueryReq, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

const N: usize = 400;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

/// The demo products `keep` keeps, of the first `N`.
fn service(dir: &Path, name: &str, keep: impl Fn(usize) -> bool) -> Service {
    let db = Db::create(&dir.join(name), demo::schema(N), DbOptions { publish_snapshots: true, segment_max_docs: 150, ..DbOptions::default() }).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).unwrap();
    let schema = db.schema().clone();
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent(), 16, Duration::from_secs(10));
    svc.open_models(&dir.join("models"));
    svc
}

async fn grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc, l, Access::default()));
    url
}

async fn coordinator(urls: &[String], dir: &Path) -> Service {
    let c = Coordinator::connect(urls.to_vec()).await.unwrap();
    let schema = c.schema.clone();
    let mut svc = Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(10));
    svc.open_models(&dir.join("models"));
    svc
}

/// Ids and scores.
async fn query(svc: &Arc<Service>, s: &str) -> Result<Vec<(String, f64)>, String> {
    let a = svc.query(QueryReq { sql: s.into(), ..Default::default() }).await.map_err(|e| e.message)?;
    Ok(a.rs.rows.iter().zip(&a.rs.scores).map(|(r, s)| (r[0].as_str().unwrap().to_string(), *s)).collect())
}

const TOP: &str = "WITH top AS (SELECT * FROM products ORDER BY bm25(body, 'kettle lamp steel') DESC LIMIT 40)";
const CE: &str = "cross_encoder('builtin:overlap', title, description, 'a steel kettle or a desk lamp')";

#[tokio::test(flavor = "multi_thread")]
async fn shards_rerank_by_a_cross_encoder_like_one_database() {
    let dir = tempfile::tempdir().unwrap();
    let whole = Arc::new(service(dir.path(), "whole.strata", |_| true));
    let key = |i: usize| Value::Str(format!("p{i:07}"));
    let mut urls = Vec::new();
    for s in 0..3 {
        urls.push(grpc(Arc::new(service(dir.path(), &format!("s{s}.strata"), |i| shard_of(&key(i), 3) == s))).await);
    }
    let coord = Arc::new(coordinator(&urls, dir.path()).await);
    let queries = [
        format!("{TOP} SELECT id, score() FROM top ORDER BY {CE} DESC LIMIT 10"),
        format!("{} SELECT id, title, score() FROM top ORDER BY {CE} DESC LIMIT 6 OFFSET 3", TOP.replace("ORDER BY", "WHERE price < 300 ORDER BY")),
        format!("{TOP} SELECT id, score() FROM top ORDER BY {CE} + 0.05 * rating DESC LIMIT 10"),
        format!("{TOP}, ce AS (SELECT * FROM top ORDER BY {CE} DESC LIMIT 12) SELECT id, score() FROM ce ORDER BY bm25(body, 'kettle') + rating DESC LIMIT 5"),
    ];
    for q in &queries {
        let want = query(&whole, q).await.unwrap_or_else(|e| panic!("{q}: {e}"));
        assert!(!want.is_empty(), "{q}");
        assert!(want.iter().any(|w| w.1 > 0.0), "{q}: the model scored nothing");
        let got = query(&coord, q).await.unwrap_or_else(|e| panic!("{q}: {e}"));
        assert_eq!(want.iter().map(|w| &w.0).collect::<Vec<_>>(), got.iter().map(|g| &g.0).collect::<Vec<_>>(), "{q}");
        for (w, g) in want.iter().zip(&got) {
            assert_eq!(w.1.to_bits(), g.1.to_bits(), "{q}: {w:?} against {g:?}");
        }
    }
    // The hidden text the fetch round read is not among the columns.
    let a = coord.query(QueryReq { sql: queries[1].clone(), ..Default::default() }).await.unwrap();
    assert_eq!(a.rs.columns, ["id", "title", "score()"]);

    // --max-rerank caps the rows a model scores, on a coordinator and on
    // one database alike; a shard leaves it to its coordinator.
    let mut capped = coordinator(&urls, dir.path()).await;
    capped.max_rerank = Some(30);
    let capped = Arc::new(capped);
    let e = query(&capped, &queries[0]).await.err().unwrap();
    assert!(e.contains("a cross-encoder scores at most 30 rows of a query here (--max-rerank), not 40: rerank fewer rows by it"), "{e}");
    assert!(query(&capped, &queries[0].replace("LIMIT 40", "LIMIT 30")).await.is_ok());
    let mut one = service(dir.path(), "one.strata", |_| true);
    one.max_rerank = Some(30);
    let one = Arc::new(one);
    let e = query(&one, &queries[0]).await.err().unwrap();
    assert!(e.contains("at most 30 rows"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shard_needs_no_model() {
    let dir = tempfile::tempdir().unwrap();
    let svc = Arc::new(service(dir.path(), "s.strata", |_| true));
    let catalog = svc.catalog();
    let tables: Vec<&strata::Schema> = catalog.iter().map(|t| &**t).collect();
    let sql = format!("{TOP} SELECT id, score() FROM top ORDER BY cross_encoder('absent', title, 'kettle') DESC LIMIT 10");
    let plan = Arc::new(compound::plan_read(&sql, &tables, &Params::new(), &|_, _| None).unwrap());
    let req = |shard| QueryReq { sql: sql.clone(), plan: Some(plan.clone()), shard, ..Default::default() };
    // As a shard it returns the rows of the phase before the model's, for
    // its coordinator to fetch the text of and score.
    let a = svc.query(req(true)).await.unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(a.rs.rows.len(), 40);
    let e = svc.query(req(false)).await.err().unwrap().message;
    assert!(e.contains("no cross-encoder named absent"), "{e}");
}
