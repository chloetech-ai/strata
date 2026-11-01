//! Changing a cluster while it serves (shard protocol 9): a coordinator's
//! admin calls add and remove shards and replicas, each change a new
//! cluster map (slots and the servers of each shard) that every server
//! keeps beside its database; other coordinators take it up, and one
//! started on an older list of shards routes by the map the servers hold.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use strata::db::Db;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata_server::auth::Access;
use strata_server::coord::Coordinator;
use strata_server::cores::slots::Moved;
use strata_server::keys::Principal;
use strata_server::replication::{self, Acks, KeptRole, PrimaryOptions};
use strata_server::service::{shard_of, After, AgentConfig, Backend, QueryReq, Service};
use strata_server::{demo, pb, server, slots};
use tokio::net::TcpListener;

const N: usize = 300;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn key(i: usize) -> Value {
    Value::Str(format!("p{i:07}"))
}

/// A numbered database at `dir/name` of the demo products `keep` keeps.
fn create(dir: &Path, name: &str, keep: impl Fn(usize) -> bool) -> Arc<Db> {
    let db = create_unnumbered(dir, name, keep);
    db.storage().start_seq_no().unwrap();
    db
}

/// [`create`], of a database that does not number its writes.
fn create_unnumbered(dir: &Path, name: &str, keep: impl Fn(usize) -> bool) -> Arc<Db> {
    let opts = DbOptions { publish_snapshots: true, ship_log_bytes: 16 << 20, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), demo::schema(N), opts).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    let docs: Vec<_> = (0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7)).collect();
    if !docs.is_empty() {
        db.load(docs).unwrap();
    }
    Arc::new(db)
}

/// A server of `db` (at `dir/name`) as `strata-server --db` makes one: it
/// keeps the cluster map and its part in replication beside the database,
/// and ships quickly when it becomes a primary.
async fn serve(db: Arc<Db>, dir: &Path, name: &str) -> (Arc<Service>, String) {
    let path = dir.join(name);
    let schema = (*db.schema()).clone();
    let mut svc = Service::new(Backend::Local(db), schema, agent(), 16, Duration::from_secs(10));
    svc.keep_slots_in(slots::file_of(&path)).unwrap();
    svc.keep_replication_in(replication::file_of(&path));
    svc.promote_options = Some(PrimaryOptions { heartbeat: Duration::from_millis(100), retry_max: Duration::from_millis(200), ..Default::default() });
    let svc = Arc::new(svc);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc.clone(), l, Access::default()));
    (svc, url)
}

async fn coordinator(urls: &[String]) -> (Arc<Coordinator>, Arc<Service>) {
    let c = Arc::new(Coordinator::connect(urls.to_vec()).await.unwrap());
    let schema = c.schema.clone();
    (c.clone(), Arc::new(Service::new(Backend::Cluster(c), schema, agent(), 16, Duration::from_secs(10))))
}

/// The ids of every product a server or coordinator serves, sorted.
async fn ids(svc: &Arc<Service>) -> Vec<String> {
    let a = svc.query(QueryReq { sql: "SELECT id FROM products LIMIT 10000".into(), ..Default::default() }).await.unwrap();
    let mut v: Vec<String> = a.rs.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

async fn sql(svc: &Arc<Service>, s: &str) -> Result<(), String> {
    svc.sql(&Principal::open(), s, &Params::new(), After::Any, 0).await.map(|_| ()).map_err(|e| e.message)
}

fn servers(c: &Coordinator) -> Vec<Vec<String>> {
    c.map().unwrap().replicas().to_vec()
}

async fn until<F: std::future::Future<Output = bool>>(what: &str, f: impl Fn() -> F) {
    let t0 = Instant::now();
    while !f().await {
        assert!(t0.elapsed() < Duration::from_secs(30), "waited 30 s for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const CHEAP: &str = "CREATE RANK PROFILE cheap ON products AS
    WITH top AS (SELECT * FROM products ORDER BY bm25(body, :t) DESC LIMIT 40)
    SELECT * FROM top ORDER BY bm25(body, :t) / (1 + :w * price) DESC
    WITH (t => 'kettle', w => 0.01)";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shards_join_and_leave_a_serving_cluster() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut shards = Vec::new();
    let mut urls = Vec::new();
    for s in 0..2 {
        let name = format!("s{s}.strata");
        let (svc, url) = serve(create(d, &name, |i| shard_of(&key(i), 2) == s), d, &name).await;
        shards.push(svc);
        urls.push(url);
    }
    let (a, svc_a) = coordinator(&urls).await;
    // Another coordinator, which follows what `a` changes.
    let (b, svc_b) = coordinator(&urls).await;
    let first = a.map().unwrap();
    assert_eq!(servers(&a), vec![vec![urls[0].clone()], vec![urls[1].clone()]], "the map names each shard's servers");
    assert_eq!(b.map().unwrap(), first);
    sql(&svc_a, "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)").await.unwrap();
    sql(&svc_a, CHEAP).await.unwrap();
    let all = ids(&svc_a).await;
    assert_eq!(all.len(), N);

    // What a new shard must be: new, empty, of the cluster's first table.
    let db2 = create(d, "s2.strata", |_| false);
    let (_, u2) = serve(db2.clone(), d, "s2.strata").await;
    let (_, full) = serve(create(d, "full.strata", |i| i < 5), d, "full.strata").await;
    let other = Db::create(&d.join("other.strata"), strata::Schema::new("other", vec![strata::Field::new("id", strata::FieldType::Keyword)], Some("id")).unwrap(), DbOptions::default()).unwrap();
    let (_, u_other) = serve(Arc::new(other), d, "other.strata").await;
    let e = a.add_shard(vec![urls[0].clone()], Acks::All).await.unwrap_err();
    assert!(e.message.contains("serves shard 0 already"), "{}", e.message);
    let e = a.add_shard(vec![full.clone()], Acks::All).await.unwrap_err();
    assert!(e.message.contains("holds 5 rows"), "{}", e.message);
    let e = a.add_shard(vec![u_other.clone()], Acks::All).await.unwrap_err();
    assert!(e.message.contains("another schema"), "{}", e.message);
    assert_eq!(a.map().unwrap(), first, "a refused change changes nothing");

    let j = a.add_shard(vec![u2.clone()], Acks::All).await.unwrap();
    assert_eq!(j["shard"], 2);
    assert_eq!(j["missed"], serde_json::json!([]));
    let added = a.map().unwrap();
    assert_eq!(added.epoch, first.epoch + 1);
    assert_eq!((added.shards(), added.count(2)), (3, 0), "the new shard holds no slot yet");
    assert_eq!(servers(&a)[2], vec![u2.clone()]);
    for (s, svc) in shards.iter().enumerate() {
        let (m, i) = svc.slots().unwrap();
        assert_eq!((m, i), (added.clone(), s), "shard {s} holds the new map");
    }
    // The new shard was given the cluster's other tables and profiles.
    assert!(db2.catalog().iter().any(|t| t.table == "notes"), "the new shard has the table made before it joined");
    assert!(db2.storage().rank_profiles("products").unwrap().iter().any(|p| p.name == "cheap"), "and the rank profile");
    assert_eq!(ids(&svc_a).await, all, "the coordinator reads every shard of the new map");

    // `b` routes by the map before until it takes up the new one: the
    // shards, whose slots did not change, still serve it.
    assert_eq!(b.map().unwrap(), first);
    assert_eq!(ids(&svc_b).await, all);
    sql(&svc_b, "DELETE FROM products WHERE id = 'p0000003'").await.unwrap();
    assert!(b.sync().await, "b takes up the newer map");
    assert_eq!(b.map().unwrap(), added);
    assert_eq!(ids(&svc_b).await.len(), N - 1);
    // A coordinator started on the old list of shards routes by the map
    // they hold.
    let (c, svc_c) = coordinator(&urls).await;
    assert_eq!(c.map().unwrap(), added);
    assert_eq!(ids(&svc_c).await.len(), N - 1);
    sql(&svc_a, "DELETE FROM products WHERE id = 'p0000004'").await.unwrap();

    let e = a.remove_shard(0).await.unwrap_err();
    assert!(e.message.contains("drain"), "a shard holding slots stays: {}", e.message);
    let j = a.remove_shard(2).await.unwrap();
    assert_eq!(j["removed"], serde_json::json!([u2.clone()]));
    let removed = a.map().unwrap();
    assert_eq!((removed.epoch, removed.shards()), (added.epoch + 1, 2));
    assert!(removed.same_slots_as(0, &first, 0) && removed.same_slots_as(1, &first, 1));
    assert_eq!(ids(&svc_a).await.len(), N - 2);
    assert!(c.sync().await);
    assert_eq!(c.map().unwrap(), removed);
    assert_eq!(ids(&svc_c).await.len(), N - 2);
    // The server that left holds the map it served under, and so does not
    // join another cluster by mistake.
    let e = a.add_shard(vec![u2.clone()], Acks::All).await.unwrap_err();
    assert!(e.message.contains("holds a cluster map already"), "{}", e.message);

    let cluster = svc_a.cluster().await;
    assert_eq!(cluster["slot_epoch"], removed.epoch);
    assert_eq!(cluster["changes"]["ok"], true);
    assert_eq!(cluster["shards"][0]["slots_held"].as_u64().unwrap() + cluster["shards"][1]["slots_held"].as_u64().unwrap(), slots::SLOTS as u64);
}

/// The rows `sql` reads through `svc`.
async fn select(svc: &Arc<Service>, sql: &str) -> Vec<Vec<Value>> {
    svc.query(QueryReq { sql: sql.into(), ..Default::default() }).await.unwrap_or_else(|e| panic!("{sql}: {}", e.message)).rs.rows
}

/// ALTER TABLE through a coordinator runs on every shard, and the
/// coordinator serves the columns it added or renamed, of the first table
/// and of others; a shard added after is given the altered tables. DROP
/// COLUMN is refused: the coordinator's plans name columns by place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tables_altered_through_a_coordinator() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut urls = Vec::new();
    for s in 0..2 {
        let name = format!("s{s}.strata");
        urls.push(serve(create(d, &name, |i| shard_of(&key(i), 2) == s), d, &name).await.1);
    }
    let (a, svc) = coordinator(&urls).await;
    sql(&svc, "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)").await.unwrap();
    sql(&svc, "INSERT INTO notes (id, body) VALUES (1, 'one'), (2, 'two'), (3, 'three')").await.unwrap();
    sql(&svc, "ALTER TABLE notes ADD COLUMN pinned INT DEFAULT 0, RENAME COLUMN body TO text").await.unwrap();
    sql(&svc, "ALTER TABLE products ADD COLUMN stock INT DEFAULT 5").await.unwrap();
    sql(&svc, "INSERT INTO notes (id, text, pinned) VALUES (4, 'four', 1)").await.unwrap();
    let notes = "SELECT id, text, pinned FROM notes ORDER BY id";
    let want: Vec<Vec<Value>> = [(1, "one", 0), (2, "two", 0), (3, "three", 0), (4, "four", 1)].iter().map(|&(i, t, p)| vec![Value::Int(i), Value::Str(t.into()), Value::Int(p)]).collect();
    assert_eq!(select(&svc, notes).await, want);
    assert_eq!(select(&svc, "SELECT COUNT(*) FROM products WHERE stock = 5").await, [[Value::Int(N as i64)]]);
    assert!(svc.first_table().field("stock").is_some(), "the coordinator serves the first table as altered");

    let e = sql(&svc, "ALTER TABLE notes DROP COLUMN pinned").await.unwrap_err();
    assert!(e.contains("does not run through a coordinator"), "{e}");

    // A new shard holds the first table as the cluster does, and is given
    // the others by CREATE TABLE.
    let db2 = create(d, "s2.strata", |_| false);
    let (svc2, u2) = serve(db2.clone(), d, "s2.strata").await;
    let e = a.add_shard(vec![u2.clone()], Acks::All).await.unwrap_err();
    assert!(e.message.contains("another schema"), "{}", e.message);
    sql(&svc2, "ALTER TABLE products ADD COLUMN stock INT DEFAULT 5").await.unwrap();
    a.add_shard(vec![u2], Acks::All).await.unwrap();
    assert!(db2.catalog().iter().any(|t| t.table == "notes" && t.field("pinned").is_some()));
    a.refresh(true).await;
    assert_eq!(select(&svc, notes).await, want, "the coordinator serves the table its shards made differently");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicas_join_and_leave_a_shard() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let mut shards = Vec::new();
    let mut urls = Vec::new();
    for s in 0..2 {
        let name = format!("s{s}.strata");
        let (svc, url) = serve(create(d, &name, |i| shard_of(&key(i), 2) == s), d, &name).await;
        shards.push(svc);
        urls.push(url);
    }
    let (a, svc_a) = coordinator(&urls).await;
    let all = ids(&svc_a).await;
    let mine = ids(&shards[0]).await;

    // A standalone shard becomes the primary of the new replica, which is
    // sent a copy of it (its rows were bulk loaded, and took no number).
    let r0db = create(d, "r0.strata", |_| false);
    let (r0, u_r0) = serve(r0db.clone(), d, "r0.strata").await;
    let j = a.add_replica(0, &u_r0, false).await.unwrap();
    assert_eq!(j["primary"], urls[0]);
    assert_eq!(j["replicas"], serde_json::json!([u_r0.clone()]));
    assert_eq!(servers(&a)[0], vec![urls[0].clone(), u_r0.clone()]);
    assert_eq!(shards[0].role(), pb::Role::Primary);
    assert_eq!(r0.role(), pb::Role::Replica);
    until("the new replica holds the shard's rows", || async { r0db.snapshot().n_live_docs() == mine.len() as u64 }).await;
    until("the new replica serves reads", || async {
        r0.query(QueryReq { sql: "SELECT id FROM products LIMIT 10000".into(), ..Default::default() }).await.is_ok()
    })
    .await;
    assert_eq!(ids(&r0).await, mine);
    // Their parts are kept with their databases, for when they start again.
    let (kept, _) = replication::kept_role(&d.join("s0.strata")).unwrap();
    assert_eq!(kept, Some(KeptRole::Primary { replicas: vec![u_r0.clone()], acks: Acks::All }));
    assert_eq!(replication::kept_role(&d.join("r0.strata")).unwrap().0, Some(KeptRole::Replica));

    // Writes reach the replica, and the coordinator reads from either copy.
    let gone = all.iter().find(|id| mine.contains(id)).unwrap().clone();
    sql(&svc_a, &format!("DELETE FROM products WHERE id = '{gone}'")).await.unwrap();
    until("the replica applies the write", || async { r0db.snapshot().n_live_docs() == mine.len() as u64 - 1 }).await;
    assert_eq!(ids(&svc_a).await.len(), N - 1);

    // A second replica, added to a shard that replicates already.
    let r1db = create(d, "r1.strata", |_| false);
    let (r1, u_r1) = serve(r1db.clone(), d, "r1.strata").await;
    let j = a.add_replica(0, &u_r1, false).await.unwrap();
    assert_eq!(j["replicas"], serde_json::json!([u_r0.clone(), u_r1.clone()]));
    until("the second replica catches up", || async { r1db.snapshot().n_live_docs() == mine.len() as u64 - 1 }).await;

    let e = a.remove_replica(0, &urls[0]).await.unwrap_err();
    assert!(e.message.contains("primary"), "the primary stays: {}", e.message);
    let e = a.remove_replica(1, &urls[1]).await.unwrap_err();
    assert!(e.message.contains("last server"), "{}", e.message);
    let j = a.remove_replica(0, &u_r0).await.unwrap();
    assert_eq!(j["shipping"]["replicas"], serde_json::json!([u_r1.clone()]));
    assert_eq!(servers(&a)[0], vec![urls[0].clone(), u_r1.clone()]);
    let (kept, _) = replication::kept_role(&d.join("s0.strata")).unwrap();
    assert_eq!(kept, Some(KeptRole::Primary { replicas: vec![u_r1.clone()], acks: Acks::All }));
    assert_eq!(ids(&svc_a).await.len(), N - 1);
    let health = shards[0].health().await;
    assert_eq!(health["replication"]["replicas"].as_array().unwrap().len(), 1, "{health}");

    // A replica that joined a shard holding no transaction (its rows came
    // in the copy) is handed the primary: no copy holds a transaction it
    // lacks.
    let r2db = create(d, "r2.strata", |_| false);
    let (r2, u_r2) = serve(r2db.clone(), d, "r2.strata").await;
    a.add_replica(1, &u_r2, false).await.unwrap();
    until("the new replica serves reads", || async {
        r2.query(QueryReq { sql: "SELECT id FROM products LIMIT 10000".into(), ..Default::default() }).await.is_ok()
    })
    .await;
    let j = a.promote(1, &u_r2, false).await.unwrap();
    assert_eq!((&j["primary"], &j["seq_no"]), (&serde_json::json!(u_r2.clone()), &serde_json::json!(0)), "{j}");
    assert_eq!((r2.role(), shards[1].role()), (pb::Role::Primary, pb::Role::Replica));
    sql(&svc_a, &format!("DELETE FROM products WHERE id = '{}'", all.iter().find(|id| !mine.contains(id)).unwrap())).await.unwrap();
    assert_eq!(ids(&svc_a).await.len(), N - 2);

    // A table made through the coordinator reaches each shard's replica,
    // in order with the rows written to it (shard 1's primary is the one
    // just handed over); a replica that joins later gets it with its copy
    // of the primary. (No notes while a replica catching up refuses
    // reads.)
    let notes = |svc: Arc<Service>| async move {
        let Ok(a) = svc.query(QueryReq { sql: "SELECT id FROM notes LIMIT 1000".into(), ..Default::default() }).await else { return Vec::new() };
        let mut v: Vec<String> = a.rs.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
        v.sort();
        v
    };
    sql(&svc_a, "CREATE TABLE notes (id KEYWORD PRIMARY KEY, body TEXT)").await.unwrap();
    let values: Vec<String> = (0..20).map(|i| format!("('n{i:02}', 'note {i}')")).collect();
    sql(&svc_a, &format!("INSERT INTO notes (id, body) VALUES {}", values.join(", "))).await.unwrap();
    assert_eq!(notes(svc_a.clone()).await.len(), 20);
    for (primary, replica, rdb) in [(&shards[0], &r1, &r1db), (&r2, &shards[1], &r2db)] {
        assert_eq!(rdb.catalog(), primary.catalog(), "the replica made the table");
        until("the replica holds its shard's notes", || async { notes(replica.clone()).await == notes(primary.clone()).await }).await;
    }
    let r3db = create(d, "r3.strata", |_| false);
    let (r3, u_r3) = serve(r3db.clone(), d, "r3.strata").await;
    a.add_replica(1, &u_r3, false).await.unwrap();
    until("the third replica holds the notes", || async { notes(r3.clone()).await == notes(r2.clone()).await }).await;
    assert_eq!(r3db.catalog(), r2.catalog(), "its copy brought the table");
    sql(&svc_a, "DROP TABLE notes").await.unwrap();
    until("the replicas drop it too", || async { [&r1db, &r3db].iter().all(|db| db.storage().table("notes").is_none()) }).await;
}

/// `Configure` refuses the changes of part that would lose writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configure_refuses_what_would_lose_writes() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (_, u) = serve(create(d, "a.strata", |i| i < 10), d, "a.strata").await;
    let mut c = pb::strata_client::StrataClient::connect(u.clone()).await.unwrap();
    let ask = |role: pb::Role, replicas: Vec<String>, force: bool| pb::ConfigureRequest { role: role as i32, replicas, write_acks: String::new(), force, promote: false };
    let e = c.configure(ask(pb::Role::Replica, vec![], false)).await.unwrap_err();
    assert!(e.message().contains("holds 10 rows"), "{}", e.message());
    let (_, r) = serve(create(d, "b.strata", |_| false), d, "b.strata").await;
    let a = c.configure(ask(pb::Role::Primary, vec![r.clone()], false)).await.unwrap().into_inner();
    assert_eq!((a.role(), a.replicas.clone()), (pb::Role::Primary, vec![r.clone()]));
    let e = c.configure(ask(pb::Role::Replica, vec![], false)).await.unwrap_err();
    assert!(e.message().contains("promote another copy first"), "{}", e.message());
    let e = c.configure(ask(pb::Role::Primary, vec![r.clone(), r.clone()], false)).await.unwrap_err();
    assert!(e.message().contains("named twice"), "{}", e.message());
    // Standalone again: it forgets its part.
    let a = c.configure(ask(pb::Role::Standalone, vec![], false)).await.unwrap().into_inner();
    assert_eq!(a.role(), pb::Role::Standalone);
    assert_eq!(replication::kept_role(&d.join("a.strata")).unwrap().0, None);
}

/// The products (`p…`) a coordinator serves, with their `sold`, and every
/// other key it serves; panics on a key served twice.
async fn rows(svc: &Arc<Service>) -> (std::collections::BTreeMap<String, i64>, std::collections::BTreeMap<String, f64>) {
    let a = svc.query(QueryReq { sql: "SELECT id, sold, price FROM products LIMIT 100000".into(), ..Default::default() }).await.unwrap();
    let (mut products, mut others) = (std::collections::BTreeMap::new(), std::collections::BTreeMap::new());
    for r in &a.rs.rows {
        let id = r[0].as_str().unwrap().to_string();
        let twice = match id.starts_with('p') {
            true => products.insert(id.clone(), r[1].as_i64().unwrap()).is_some(),
            false => others.insert(id.clone(), r[2].as_f64().unwrap()).is_some(),
        };
        if twice {
            let m = svc.coordinator().map(|c| c.map().map(|m| m.to_json())).ok().flatten();
            panic!("{id} (slot {}) is served twice; map {m:?}", slots::slot(&Value::Str(id.clone())));
        }
    }
    (products, others)
}

async fn job_done(c: &Coordinator) -> serde_json::Value {
    let t0 = Instant::now();
    loop {
        let m = c.moves();
        if m["job"]["state"] != "running" {
            return m;
        }
        assert!(t0.elapsed() < Duration::from_secs(120), "the job of moves runs still: {m}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Slots move between shards while writes and reads go on through two
/// coordinators: rebalancing onto a shard just added, then draining the
/// first to remove it. Every read sees each row once; every acknowledged
/// write is kept, a statement on every shard (UPDATE) changes each row
/// once, and rows placed by a column (shard_by) move with their slots.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slots_move_while_the_cluster_serves() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (s0, u0) = serve(create(d, "s0.strata", |_| true), d, "s0.strata").await;
    let (a, svc_a) = coordinator(&[u0.clone()]).await;
    sql(&svc_a, "CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, n INT) OPTIONS(shard_by = 'usr')").await.unwrap();
    let values: Vec<String> = (0..200).map(|i| format!("({i}, 'u{}', {i})", i % 13)).collect();
    sql(&svc_a, &format!("INSERT INTO events (id, usr, n) VALUES {}", values.join(", "))).await.unwrap();
    let (before, _) = rows(&svc_a).await;
    assert_eq!(before.len(), N);
    let (_, u1) = serve(create(d, "s1.strata", |_| false), d, "s1.strata").await;
    a.add_shard(vec![u1.clone()], Acks::All).await.unwrap();
    let (b, svc_b) = coordinator(&[u0.clone()]).await;
    assert_eq!(b.map().unwrap().shards(), 2, "a coordinator started on the first shard follows the map");

    // Writers: rows by key through one coordinator, statements on every
    // shard through the other; a reader through both.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let acked: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Option<f64>>>> = Default::default();
    let updates = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let mut tasks = Vec::new();
    {
        let (svc, stop, acked) = (svc_a.clone(), stop.clone(), acked.clone());
        tasks.push(tokio::spawn(async move {
            let mut r = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let k = format!("w{:03}", r % 97);
                let price = r as f64;
                let q = if r % 7 == 3 { format!("DELETE FROM products WHERE id = '{k}'") } else { format!("INSERT OR REPLACE INTO products (id, title, description, price) VALUES ('{k}', 'Writer kettle', 'Round {r}', {price})") };
                match svc.sql(&Principal::open(), &q, &Params::new(), After::Any, 0).await {
                    Ok(_) => {
                        acked.lock().unwrap().insert(k, (r % 7 != 3).then_some(price));
                    }
                    Err(e) => panic!("{q}: {}", e.message),
                }
                r += 1;
            }
            r
        }));
    }
    {
        let (svc, stop, updates) = (svc_b.clone(), stop.clone(), updates.clone());
        tasks.push(tokio::spawn(async move {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match svc.sql(&Principal::open(), "UPDATE products SET sold = sold + 1", &Params::new(), After::Any, 0).await {
                    Ok(_) => {
                        updates.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(e) => panic!("UPDATE: {}", e.message),
                }
                n += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            n
        }));
    }
    for svc in [svc_a.clone(), svc_b.clone()] {
        let stop = stop.clone();
        tasks.push(tokio::spawn(async move {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let (products, _) = rows(&svc).await;
                assert_eq!(products.len(), N, "every product, once, in every read");
                n += 1;
            }
            n
        }));
    }

    // Rebalance onto the new shard, in moves of 512 slots.
    let j = a.rebalance(512).unwrap();
    assert_eq!(j["job"]["of"], 4, "{j}");
    let done = job_done(&a).await;
    assert_eq!(done["job"]["state"], "done", "{done}");
    assert_eq!(done["job"]["slots_moved"], 2048);
    assert!(done["moving"].is_null(), "{done}");
    let map = a.map().unwrap();
    assert_eq!((map.count(0), map.count(1), map.moving().is_none()), (2048, 2048, true));
    for m in done["job"]["moves"].as_array().unwrap() {
        assert!(m["held_ms"].as_f64().unwrap() < 5_000.0, "{m}");
    }

    // Drain the first shard, then remove it.
    a.drain(0, 1024).unwrap();
    let done = job_done(&a).await;
    assert_eq!(done["job"]["state"], "done", "{done}");
    assert_eq!(a.map().unwrap().count(0), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut counts = Vec::new();
    for t in tasks {
        counts.push(t.await.unwrap());
    }
    assert!(counts.iter().all(|&n| n > 0), "{counts:?}");
    a.remove_shard(0).await.unwrap();
    assert_eq!(a.map().unwrap().replicas().to_vec(), vec![vec![u1.clone()]]);

    // Every acknowledged write, and each UPDATE once on every product.
    let (products, others) = rows(&svc_a).await;
    let n = updates.load(std::sync::atomic::Ordering::Relaxed);
    assert!(n > 0);
    for (id, sold) in &products {
        assert_eq!(*sold, before[id] + n, "{id}: sold {sold}, was {}, {n} updates", before[id]);
    }
    let acked = acked.lock().unwrap().clone();
    let kept: std::collections::BTreeMap<String, f64> = acked.iter().filter_map(|(k, p)| p.map(|p| (k.clone(), p))).collect();
    assert_eq!(others, kept, "the rows written by key are those last acknowledged");
    let a_events = svc_a.query(QueryReq { sql: "SELECT usr, COUNT(*) FROM events GROUP BY usr ORDER BY usr LIMIT 100".into(), ..Default::default() }).await.unwrap();
    assert_eq!(a_events.rs.rows.len(), 13);
    assert_eq!(a_events.rs.rows.iter().map(|r| r[1].as_i64().unwrap()).sum::<i64>(), 200);
    // The drained shard kept nothing it serves no more; the other holds
    // every row, hiding none.
    assert_eq!(s0.local_db().unwrap().snapshot().n_live_docs(), 0);
    let (_, svc_b2) = coordinator(&[u1.clone()]).await;
    assert_eq!(rows(&svc_b2).await.0.len(), N);
}

/// The slots of the products a server holds (raw, hiding nothing).
fn held_slots(svc: &Service) -> Vec<usize> {
    let snap = svc.local_db().unwrap().snapshot();
    (0..N).filter(|&i| snap.lookup(&key(i)).is_some()).map(|i| slots::slot(&key(i))).collect()
}

/// Shards that cannot tell what changed (they do not number their
/// writes) move slots holding the writes to them from the start; a move a
/// coordinator left half done is finished by another: one copying is
/// given up, and the rows it sent deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unnumbered_shards_move_and_a_move_left_is_given_up() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (s0, u0) = serve(create_unnumbered(d, "s0.strata", |_| true), d, "s0.strata").await;
    let (s1, u1) = serve(create_unnumbered(d, "s1.strata", |_| false), d, "s1.strata").await;
    let (a, svc_a) = coordinator(&[u0.clone()]).await;
    a.add_shard(vec![u1.clone()], Acks::All).await.unwrap();

    // A move from an unnumbered shard: writes held throughout, each row
    // copied once.
    let j = a.start_move((0..1024).collect(), 1).unwrap();
    assert_eq!(j["job"]["kind"], "move", "{j}");
    let done = job_done(&a).await;
    assert_eq!(done["job"]["state"], "done", "{done}");
    let step = &done["job"]["moves"][0];
    assert_eq!(step["rounds"], 0, "{step}");
    assert!(step["held_ms"].as_f64().is_some(), "{step}");
    let map = a.map().unwrap();
    assert_eq!((map.count(0), map.count(1), map.moving().is_none()), (3072, 1024, true));
    assert!(held_slots(&s1).iter().all(|&s| s < 1024) && !held_slots(&s1).is_empty());
    assert!(held_slots(&s0).iter().all(|&s| s >= 1024));
    assert_eq!(ids(&svc_a).await.len(), N);
    assert!(!s0.frozen().await, "the hold ends with the move");

    // A coordinator starts a move and copies part of it, then stops: the
    // map says copying, and the shard the slots were to go to holds rows
    // it does not serve.
    let moving: Vec<usize> = (1024..1536).collect();
    let started = map.start_move(&moving, 1).unwrap();
    s0.set_slots(started.clone(), 0).await.unwrap();
    s1.set_slots(started.clone(), 1).await.unwrap();
    let mut rx = s0.export_slots(false, 10).await.unwrap();
    let mut sent = 0;
    while let Some(chunk) = rx.recv().await {
        let chunk = chunk.unwrap();
        if chunk.last {
            assert!(chunk.frozen, "an unnumbered shard holds writes from the start");
            break;
        }
        let ops: Vec<serde_json::Value> = serde_json::from_slice(&chunk.ops_json).unwrap();
        sent += ops.len();
        s1.write_ops_as(ops, None, Moved::Copy).await.unwrap();
        // Half of it.
        if sent >= 20 {
            break;
        }
    }
    drop(rx);
    assert!(s0.frozen().await);
    assert!(held_slots(&s1).iter().any(|s| moving.contains(s)), "rows were sent");
    assert_eq!(ids(&svc_a).await.len(), N, "rows sent are not served before the flip");

    // Another coordinator finishes it: given up.
    let (b, svc_b) = coordinator(&[u0.clone()]).await;
    let f = b.finish_move().await.unwrap();
    assert_eq!(f["finished"]["was"], "copying", "{f}");
    assert_eq!(f["finished"]["moved"], false, "{f}");
    let map = b.map().unwrap();
    assert!(map.moving().is_none());
    assert_eq!((map.count(0), map.count(1)), (3072, 1024));
    assert!(held_slots(&s1).iter().all(|&s| s < 1024), "the rows sent are deleted");
    assert!(!s0.frozen().await, "giving up ends the hold");
    assert_eq!(ids(&svc_b).await.len(), N);
    sql(&svc_b, "UPDATE products SET sold = 1").await.unwrap();
    let a_sold = svc_a.query(QueryReq { sql: "SELECT COUNT(*) FROM products WHERE sold = 1".into(), ..Default::default() }).await.unwrap();
    assert_eq!(a_sold.rs.rows[0][0].as_i64(), Some(N as i64));
    // Nothing is left to finish.
    assert!(b.finish_move().await.unwrap()["finished"].is_null());
}
