//! Rank profiles on servers: made through a coordinator, they are on every
//! shard, and a query by one answers as on one database; a coordinator
//! that planned with a profile since replaced asks the shards again; a
//! shard refuses a plan made with another version of its profile; and a
//! primary ships its profiles to its replicas as it ships writes.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use strata::db::Db;
use strata::query::{compound, Params};
use strata::storage::DbOptions;
use strata::value::Value;
use strata_server::auth::Access;
use strata_server::keys::Principal;
use strata_server::coord::Coordinator;
use strata_server::replication::{Acks, Follow, Primary, PrimaryOptions};
use strata_server::service::{shard_of, After, AgentConfig, Backend, QueryReq, Replication, Service, SqlAnswer};
use strata_server::{demo, server};
use tokio::net::TcpListener;

const N: usize = 400;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

/// The demo products `keep` keeps, of the first `N`, numbered when the
/// database ships its log.
fn create(dir: &Path, name: &str, ship: bool, keep: impl Fn(usize) -> bool) -> Arc<Db> {
    let opts = DbOptions { publish_snapshots: true, segment_max_docs: 150, ship_log_bytes: if ship { 16 << 20 } else { 0 }, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), demo::schema(N), opts).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).unwrap();
    if ship {
        db.storage().start_seq_no().unwrap();
    }
    Arc::new(db)
}

fn service(db: Arc<Db>) -> Service {
    let schema = db.schema().clone();
    Service::new(Backend::Local(db), schema, agent(), 16, Duration::from_secs(10))
}

async fn grpc(svc: Arc<Service>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc, l, Access::default()));
    url
}

async fn coordinator(urls: &[String]) -> Arc<Service> {
    let c = Coordinator::connect(urls.to_vec()).await.unwrap();
    let schema = c.schema.clone();
    Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(10)))
}

async fn sql(svc: &Arc<Service>, s: &str) -> Result<SqlAnswer, String> {
    svc.sql(&Principal::open(), s, &Params::new(), After::Any, 0).await.map_err(|e| e.message)
}

/// Ids and scores, as one database or a coordinator answers.
async fn query(svc: &Arc<Service>, s: &str) -> Result<Vec<(String, f64)>, String> {
    let a = svc.query(QueryReq { sql: s.into(), ..Default::default() }).await.map_err(|e| e.message)?;
    Ok(a.rs.rows.iter().zip(&a.rs.scores).map(|(r, s)| (r[0].as_str().unwrap().to_string(), *s)).collect())
}

/// The same rows with the same scores (to rounding), in the same order but
/// for rows tied on score.
fn same(what: &str, want: &[(String, f64)], got: &[(String, f64)]) {
    assert!(!want.is_empty(), "{what}: finds rows");
    assert_eq!(want.len(), got.len(), "{what}: {want:?} against {got:?}");
    for (i, (w, g)) in want.iter().zip(got).enumerate() {
        assert!((w.1 - g.1).abs() < 1e-6, "{what}: row {i}: {w:?} against {g:?}");
    }
    let mut i = 0;
    while i < want.len() {
        let mut j = i + 1;
        while j < want.len() && (want[j].1 - want[i].1).abs() < 1e-6 {
            j += 1;
        }
        let ids = |rows: &[(String, f64)]| {
            let mut v: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
            v.sort();
            v
        };
        assert_eq!(ids(&want[i..j]), ids(&got[i..j]), "{what}: rows {i}..{j}");
        i = j;
    }
}

const CHEAP: &str = "CREATE RANK PROFILE cheap ON products AS
    WITH top AS (SELECT * FROM products ORDER BY bm25(body, :t) DESC LIMIT 40)
    SELECT * FROM top ORDER BY bm25(body, :t) / (1 + :w * price) DESC
    WITH (t => 'kettle', w => 0.01)";

const BY_CHEAP: [(&str, &str); 3] = [
    ("SELECT id, score() FROM products ORDER BY rank_profile('cheap') LIMIT 10", "WITH top AS (SELECT * FROM products ORDER BY bm25(body, 'kettle') DESC LIMIT 40) SELECT id, score() FROM top ORDER BY bm25(body, 'kettle') / (1 + {w} * price) DESC LIMIT 10"),
    (
        "SELECT id, score() FROM products WHERE price < 200 ORDER BY rank_profile('cheap', t => 'lamp') LIMIT 8 OFFSET 2",
        "WITH top AS (SELECT * FROM products WHERE price < 200 ORDER BY bm25(body, 'lamp') DESC LIMIT 40) SELECT id, score() FROM top ORDER BY bm25(body, 'lamp') / (1 + {w} * price) DESC LIMIT 8 OFFSET 2",
    ),
    ("SELECT id, score() FROM products ORDER BY rank_profile('cheap', w => 0) LIMIT 10", "WITH top AS (SELECT * FROM products ORDER BY bm25(body, 'kettle') DESC LIMIT 40) SELECT id, score() FROM top ORDER BY bm25(body, 'kettle') / (1 + 0 * price) DESC LIMIT 10"),
];

#[tokio::test(flavor = "multi_thread")]
async fn shards_rank_by_a_profile_like_one_database() {
    let dir = tempfile::tempdir().unwrap();
    let whole = Arc::new(service(create(dir.path(), "whole.strata", false, |_| true)));
    let key = |i: usize| Value::Str(format!("p{i:07}"));
    let shards: Vec<Arc<Service>> = (0..3).map(|s| Arc::new(service(create(dir.path(), &format!("s{s}.strata"), false, |i| shard_of(&key(i), 3) == s)))).collect();
    let mut urls = Vec::new();
    for s in &shards {
        urls.push(grpc(s.clone()).await);
    }
    let coord = coordinator(&urls).await;
    // Another coordinator, which learns of changes only when a shard
    // refuses what it planned.
    let other = coordinator(&urls).await;
    for svc in [&whole, &coord] {
        sql(svc, CHEAP).await.unwrap();
    }
    // `other` asks its shards again, at most once a second, when a query
    // names a profile it does not know.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let names = |svc: &Arc<Service>| svc.rank_profiles().into_iter().map(|(t, p)| format!("{t}.{}", p.name)).collect::<Vec<_>>();
    assert_eq!(names(&coord), ["products.cheap"]);
    for s in &shards {
        assert_eq!(names(s), ["products.cheap"], "on every shard");
        assert_eq!(s.rank_profile("PRODUCTS", "Cheap").unwrap().hash(), whole.rank_profile("products", "cheap").unwrap().hash());
    }
    let e = sql(&coord, CHEAP).await.err().unwrap();
    assert!(e.contains("has a rank profile named cheap already"), "{e}");
    for (by, out) in BY_CHEAP {
        let want = query(&whole, by).await.unwrap();
        same(by, &want, &query(&coord, by).await.unwrap());
        same(by, &want, &query(&whole, &out.replace("{w}", "0.01")).await.unwrap());
        same(by, &want, &query(&coord, &out.replace("{w}", "0.01")).await.unwrap());
        // `other` learned of it when it first met the name.
        same(by, &want, &query(&other, by).await.unwrap());
    }

    // Replaced through one coordinator: the other plans with the old
    // definition, which the shards refuse, and plans again.
    let replace = CHEAP.replace("CREATE", "CREATE OR REPLACE").replace("w => 0.01", "w => 0.05");
    for svc in [&whole, &coord] {
        sql(svc, &replace).await.unwrap();
    }
    let stale = other.rank_profile("products", "cheap").unwrap();
    assert_ne!(stale.hash(), whole.rank_profile("products", "cheap").unwrap().hash(), "`other` has not asked yet");
    for (by, out) in BY_CHEAP {
        let want = query(&whole, &out.replace("{w}", "0.05")).await.unwrap();
        same(by, &want, &query(&whole, by).await.unwrap());
        same(by, &want, &query(&other, by).await.unwrap());
        same(by, &want, &query(&coord, by).await.unwrap());
    }
    assert_eq!(other.rank_profile("products", "cheap").unwrap().hash(), whole.rank_profile("products", "cheap").unwrap().hash());

    // A shard refuses a plan made with another definition, and one that
    // ranks by a profile it lacks.
    let catalog = shards[0].catalog();
    let tables: Vec<&strata::Schema> = catalog.iter().map(|t| &**t).collect();
    let plan = compound::plan_read(BY_CHEAP[0].0, &tables, &Params::new(), &|_, _| Some(stale.clone())).unwrap();
    let e = shards[0].run_plan(&Principal::open(), plan, After::Any, 0).await.err().unwrap().message;
    assert!(e.contains("rank profile cheap of products is not the one the plan was made with"), "{e}");
    let mut gone = stale.clone();
    gone.name = "gone".into();
    let plan = compound::plan_read(&BY_CHEAP[0].0.replace("cheap", "gone"), &tables, &Params::new(), &|_, _| Some(gone.clone())).unwrap();
    let e = shards[1].run_plan(&Principal::open(), plan.clone(), After::Any, 0).await.err().unwrap().message;
    assert!(e.contains("products has no rank profile gone, which the plan ranks as"), "{e}");
    // So does a coordinator's shards, for a plan sent as is.
    let e = coord.run_plan(&Principal::open(), plan, After::Any, 0).await.err().unwrap().message;
    assert!(e.contains("which the plan ranks as"), "{e}");

    // Dropped through a coordinator: from every shard.
    sql(&coord, "DROP RANK PROFILE cheap").await.unwrap();
    for s in shards.iter().chain([&coord]) {
        assert!(s.rank_profiles().is_empty());
    }
    let e = query(&coord, BY_CHEAP[0].0).await.err().unwrap();
    assert!(e.contains("products has no rank profile named cheap"), "{e}");
    let e = query(&other, BY_CHEAP[0].0).await.err().unwrap();
    assert!(e.contains("which the plan ranks as") || e.contains("has no rank profile named cheap"), "{e}");
    sql(&coord, "DROP RANK PROFILE IF EXISTS cheap ON products").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_primary_ships_its_profiles_to_its_replicas() {
    let dir = tempfile::tempdir().unwrap();
    let rdb = create(dir.path(), "r.strata", true, |_| true);
    let mut r = service(rdb.clone());
    r.set_replication(Replication::Replica(Arc::new(Follow::open(rdb.clone(), None).unwrap())));
    let r = Arc::new(r);
    let ur = grpc(r.clone()).await;
    let pdb = create(dir.path(), "p.strata", true, |_| true);
    let mut p = service(pdb.clone());
    let opts = PrimaryOptions { replicas: vec![ur], acks: Acks::All, heartbeat: Duration::from_millis(100), retry_max: Duration::from_millis(200), ..Default::default() };
    p.set_replication(Replication::Primary(Primary::start(pdb.clone(), None, opts).unwrap()));
    let p = Arc::new(p);

    // The primary answers once the replica holds it.
    let SqlAnswer::Written { w, .. } = sql(&p, CHEAP).await.unwrap() else { panic!("written") };
    assert_eq!(w.seq_no, Some(1), "a numbered transaction of its own");
    let held = |db: &Db| db.storage().rank_profiles("products").unwrap();
    assert_eq!(held(&rdb), held(&pdb));
    assert_eq!(held(&rdb).len(), 1);
    for (by, _) in BY_CHEAP {
        same(by, &query(&p, by).await.unwrap(), &query(&r, by).await.unwrap());
    }
    // A write and a replacement after it reach the replica in order.
    sql(&p, "UPDATE products SET price = 1 WHERE id = 'p0000003'").await.unwrap();
    sql(&p, &CHEAP.replace("CREATE", "CREATE OR REPLACE").replace("w => 0.01", "w => 0.2")).await.unwrap();
    assert_eq!(held(&rdb), held(&pdb));
    assert_eq!(held(&rdb)[0].defaults["w"], serde_json::json!(0.2));
    for (by, _) in BY_CHEAP {
        same(by, &query(&p, by).await.unwrap(), &query(&r, by).await.unwrap());
    }
    assert_eq!(rdb.storage().seq_no(), pdb.storage().seq_no());
    // A replica takes none itself.
    let e = sql(&r, "DROP RANK PROFILE cheap").await.err().unwrap();
    assert!(e.contains("replica"), "{e}");
    sql(&p, "DROP RANK PROFILE cheap ON products").await.unwrap();
    assert!(held(&rdb).is_empty());
    // A table's DDL still does not reach replicas, so a primary refuses it.
    let e = sql(&p, "CREATE TABLE notes (id INT PRIMARY KEY)").await.err().unwrap();
    assert!(e.contains("do not reach replicas yet"), "{e}");
}
