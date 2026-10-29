//! Replication between servers: a primary ships its numbered transactions
//! to its replicas over gRPC, and answers a write once the copies
//! `--write-acks` names hold it. Replicas take no writes from clients,
//! follow one primary per term and fence an older one, and a coordinator
//! sends a shard's writes to its primary.

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::Params;
use strata::storage::{CheckpointOptions, Database, DbOptions, Layout, LogRead};
use strata::value::Value;
use strata_server::auth::Access;
use strata_server::coord::Coordinator;
use strata_server::replication::{self, Acks, Follow, Primary, PrimaryOptions};
use strata_server::service::{shard_of, AgentConfig, Backend, QueryReq, Replication, Service, SqlAnswer};
use strata_server::{demo, pb, server};
use tokio::net::TcpListener;

const N: usize = 300;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn options(ship_log_bytes: usize) -> DbOptions {
    DbOptions { publish_snapshots: true, ship_log_bytes, ..DbOptions::default() }
}

/// A numbered database of the demo products `keep` keeps, of the first
/// `N`, with a ship log of `log` bytes.
fn create_with(dir: &Path, name: &str, log: usize, keep: impl Fn(usize) -> bool) -> Arc<Db> {
    let db = Db::create(&dir.join(name), demo::schema(N), options(log)).unwrap();
    db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    db.load((0..N).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).unwrap();
    db.storage().start_seq_no().unwrap();
    Arc::new(db)
}

fn create(dir: &Path, name: &str) -> Arc<Db> {
    create_with(dir, name, 64 << 20, |_| true)
}

fn service(db: Arc<Db>, timeout: Duration) -> Service {
    let schema = db.schema().clone();
    Service::new(Backend::Local(db), schema, agent(), 16, timeout)
}

async fn grpc_on(svc: Arc<Service>, l: TcpListener) -> String {
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(svc, l, Access::default()));
    url
}

async fn grpc(svc: Arc<Service>) -> String {
    grpc_on(svc, TcpListener::bind("127.0.0.1:0").await.unwrap()).await
}

/// A replica of `db` served over gRPC (on `l`, or a new port).
async fn replica_on(db: Arc<Db>, file: Option<&Path>, l: Option<TcpListener>) -> (Arc<Service>, String) {
    let mut svc = service(db.clone(), Duration::from_secs(10));
    svc.replication = Replication::Replica(Arc::new(Follow::open(db, file).unwrap()));
    let svc = Arc::new(svc);
    let l = match l {
        Some(l) => l,
        None => TcpListener::bind("127.0.0.1:0").await.unwrap(),
    };
    (svc.clone(), grpc_on(svc, l).await)
}

async fn replica(db: Arc<Db>, file: Option<&Path>) -> (Arc<Service>, String) {
    replica_on(db, file, None).await
}

/// Quick heartbeats and retries, for tests.
fn ships_to(replicas: &[&str], acks: Acks) -> PrimaryOptions {
    PrimaryOptions {
        replicas: replicas.iter().map(|u| u.to_string()).collect(),
        acks,
        heartbeat: Duration::from_millis(100),
        retry_max: Duration::from_millis(200),
        ..Default::default()
    }
}

/// A primary of `db`, whose writes wait up to `timeout`.
fn primary(db: Arc<Db>, file: Option<&Path>, opts: PrimaryOptions, timeout: Duration) -> Arc<Service> {
    let mut svc = service(db.clone(), timeout);
    svc.replication = Replication::Primary(Primary::start(db, file, opts).unwrap());
    Arc::new(svc)
}

/// A gRPC URL whose port nothing listens on, and the address to listen
/// on later.
async fn nobody() -> (String, std::net::SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    (format!("http://{addr}"), addr)
}

/// Every row's stored fields and sequence number, in key order, and how
/// many rows the text index finds for a word.
fn rows(db: &Db) -> (Vec<Vec<Value>>, Json) {
    let r = db.query("SELECT id, title, brand, tags, price, reviews, sold, seq_no() FROM products LIMIT 100000", &Params::new()).unwrap();
    let mut rows = r.rows;
    rows.sort_by_key(|r| r[0].as_str().unwrap().to_string());
    let found = db.query("SELECT COUNT(*) FROM products WHERE MATCH(body, 'kettle')", &Params::new()).unwrap().rows[0][0].clone();
    (rows, strata_server::json::to_json(&found))
}

fn last(db: &Db) -> u64 {
    db.storage().seq_no().unwrap().seq
}

async fn until<F: std::future::Future<Output = bool>>(what: &str, f: impl Fn() -> F) {
    let t0 = Instant::now();
    while !f().await {
        assert!(t0.elapsed() < Duration::from_secs(30), "waited 30 s for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The primary's view of replica `k`.
async fn follower(p: &Arc<Service>, k: usize) -> Json {
    p.health().await["replication"]["replicas"][k].clone()
}

/// Holds `db`'s writer, as a long write would, until the sender is dropped.
fn stall(db: &Arc<Db>) -> mpsc::Sender<()> {
    let (release, wait) = mpsc::channel::<()>();
    let (held, ready) = mpsc::channel();
    let db = db.clone();
    std::thread::spawn(move || {
        // A heartbeat may be applying, and holds the writer for a moment.
        let t0 = Instant::now();
        let _t = loop {
            match db.storage().begin() {
                Ok(t) => break t,
                Err(e) => assert!(t0.elapsed() < Duration::from_secs(10), "{e}"),
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        held.send(()).unwrap();
        let _ = wait.recv();
    });
    ready.recv().unwrap();
    release
}

fn set(key: &str, field: &str, value: Json) -> Json {
    json!({"op": "set", "key": key, "field": field, "value": value})
}

fn written(a: SqlAnswer) -> (u64, Option<u64>) {
    match a {
        SqlAnswer::Written { n, seq_no, .. } => (n, seq_no),
        SqlAnswer::Rows(_) => panic!("not a write"),
    }
}

async fn hello(url: &str) -> pb::HelloResponse {
    let mut c = pb::strata_client::StrataClient::connect(url.to_string()).await.unwrap();
    c.hello(pb::HelloRequest { min_protocol: 1, max_protocol: 4 }).await.unwrap().into_inner()
}

#[tokio::test(flavor = "multi_thread")]
async fn replicas_hold_what_their_primary_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let (r1db, r2db) = (create(dir.path(), "r1.strata"), create(dir.path(), "r2.strata"));
    let (r1, u1) = replica(r1db.clone(), Some(&replication::file_of(&dir.path().join("r1.strata")))).await;
    let (_r2, u2) = replica(r2db.clone(), None).await;
    let pdb = create(dir.path(), "p.strata");
    let p = primary(pdb.clone(), Some(&replication::file_of(&dir.path().join("p.strata"))), ships_to(&[&u1, &u2], Acks::All), Duration::from_secs(10));
    let pu = grpc(p.clone()).await;
    let holds = |seq: u64| {
        for db in [&r1db, &r2db] {
            assert!(last(db) >= seq, "a replica holds {} when the write of {seq} is answered", last(db));
        }
    };

    // Write operations of every kind, answered once both replicas hold them.
    let doc = json!({"id": "n1", "title": "Test kettle", "description": "A quiet kettle", "brand": "Hearthly", "category": "kitchen", "tags": ["quiet", "new"], "price": 20.5, "rating": 4.0, "reviews": 3, "sold": 0, "year": 2026});
    let ops = vec![
        json!({"op": "insert", "doc": doc}),
        json!({"op": "add", "key": "p0000003", "field": "sold", "value": 5}),
        set("p0000003", "price", json!(9.99)),
        json!({"op": "upsert_if_newer", "doc": {"id": "n1", "reviews": 2, "price": 1.0}, "version": "reviews"}),
        json!({"op": "delete", "key": "p0000004"}),
    ];
    let w = p.write_ops(ops, None).await.unwrap();
    assert_eq!((w.rows, w.seq_no), (4, Some(1)));
    holds(1);
    // SQL, a conditional write, and an import.
    let (n, seq) = written(p.sql("UPDATE products SET sold = sold + 1 WHERE category = 'kitchen'", &Params::new(), 0).await.unwrap());
    assert!(n > 0);
    holds(seq.unwrap());
    written(p.sql("INSERT OR REPLACE INTO products (id, title, price, tags, reviews) VALUES ('n2', 'Steel kettle', 25.0, ARRAY['steel'], 9)", &Params::new(), 0).await.unwrap());
    written(p.sql("DELETE FROM products WHERE price > 300", &Params::new(), 0).await.unwrap());
    let seq3 = pdb.storage().seq_of(None, &Value::Str("p0000003".into())).unwrap().unwrap();
    let w = p.write_ops(vec![json!({"op": "add", "key": "p0000003", "field": "sold", "value": 1, "if_seq_no": seq3})], None).await.unwrap();
    holds(w.seq_no.unwrap());
    let (n, _) = p.import(axum::body::Bytes::from("id,title,price\nc1,Copper kettle,12.5\nc2,Iron pan,30\n"), strata::ingest::Format::Csv).await.unwrap();
    assert_eq!(n, 2);
    holds(last(&pdb));
    // Writers at once, which the writer groups into shared transactions. Each
    // adds to a row the primary holds, so that each write numbers one.
    let keys: Arc<Vec<String>> = Arc::new(rows(&pdb).0.iter().map(|r| r[0].as_str().unwrap().to_string()).collect());
    let tasks: Vec<_> = (0..24)
        .map(|t| {
            let (p, keys) = (p.clone(), keys.clone());
            tokio::spawn(async move {
                let mut seqs = Vec::new();
                for i in 0..5 {
                    let key = &keys[(t * 7 + i * 3) % keys.len()];
                    let w = p.write_ops(vec![json!({"op": "add", "key": key, "field": "reviews", "value": 1})], Some(format!("w{t}-{i}"))).await.unwrap();
                    seqs.push(w.seq_no.unwrap());
                }
                seqs
            })
        })
        .collect();
    for t in tasks {
        holds(*t.await.unwrap().iter().max().unwrap());
    }
    let end = last(&pdb);
    assert!(end > 10, "{end}");
    assert_eq!(rows(&r1db), rows(&pdb), "a replica holds the primary's rows, each with the primary's number");
    assert_eq!(rows(&r2db), rows(&pdb));
    assert!(rows(&pdb).1.as_u64().unwrap() >= 3, "the text index finds the written kettles, on every copy");

    // What the copies say of themselves.
    let h = p.health().await["replication"].clone();
    assert_eq!((h["role"].clone(), h["seq_no"].clone(), h["write_acks"].clone()), (json!("primary"), json!(end), json!("all")));
    for k in 0..2 {
        let f = &h["replicas"][k];
        assert_eq!((f["in_sync"].clone(), f["seq_no"].clone(), f["behind"].clone()), (json!(true), json!(end), json!(0)), "{h}");
    }
    assert_eq!(h["global_checkpoint"], end);
    let id = h["id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 32);
    until("the replica hears the global checkpoint", || async { r1.health().await["replication"]["global_checkpoint"] == end }).await;
    let rh = r1.health().await["replication"].clone();
    assert_eq!((rh["role"].clone(), rh["seq_no"].clone(), rh["follows"]["primary"].clone(), rh["follows"]["term"].clone()), (json!("replica"), json!(end), json!(id), json!(1)));
    let kept: Json = serde_json::from_slice(&std::fs::read(replication::file_of(&dir.path().join("r1.strata"))).unwrap()).unwrap();
    assert_eq!(kept["follows"]["primary"], json!(id), "the replica keeps the primary it follows");
    let kept: Json = serde_json::from_slice(&std::fs::read(replication::file_of(&dir.path().join("p.strata"))).unwrap()).unwrap();
    assert_eq!(kept["id"], json!(id), "the primary keeps its id");
    let (hp, hr) = (hello(&pu).await, hello(&u1).await);
    assert_eq!((hp.role(), hp.term, hp.seq_no), (pb::Role::Primary, 1, end));
    assert_eq!((hr.role(), hr.term, hr.seq_no), (pb::Role::Replica, 1, end));
    let m = p.prometheus().await;
    assert!(m.contains(&format!("strata_replica_in_sync{{replica=\"{u1}\"}} 1")), "{m}");
    assert!(m.contains(&format!("strata_replica_shipped_total{{replica=\"{u2}\"}} {}", end)), "{m}");
    assert!(r1.prometheus().await.contains(&format!("strata_replica_applied_total {end}")));

    // A replica takes no writes from clients, nor a server that is not
    // one a primary's.
    let e = r1.write_ops(vec![set("p0000003", "price", json!(1.0))], None).await.unwrap_err();
    assert!(e.status == 409 && e.message.contains("this server is a replica"), "{}", e.message);
    let e = r1.sql("UPDATE products SET sold = 0", &Params::new(), 0).await.err().unwrap();
    assert_eq!(e.status, 409, "{}", e.message);
    let e = r1.import(axum::body::Bytes::from("id,title\nc3,x\n"), strata::ingest::Format::Csv).await.unwrap_err();
    assert_eq!(e.status, 409, "{}", e.message);
    assert_eq!(last(&r1db), end);
    let e = p.replicate(pb::ReplicateRequest::default()).await.unwrap_err();
    assert!(e.status == 409 && e.message.contains("not a replica"), "{}", e.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_wait_for_the_copies_their_acks_name() {
    let dir = tempfile::tempdir().unwrap();
    let ms = |d: Duration| d.as_millis();

    // all: a replica that is down drops out of sync, and the write is
    // answered; back, it catches up and is in sync again.
    let adb = create(dir.path(), "a.strata");
    let (_a, ua) = replica(adb.clone(), None).await;
    let (down, addr) = nobody().await;
    let pdb = create(dir.path(), "p.strata");
    let p = primary(pdb.clone(), None, ships_to(&[&ua, &down], Acks::All), Duration::from_secs(2));
    let w = p.write_ops(vec![set("p0000001", "price", json!(1.0))], None).await.unwrap();
    assert!(last(&adb) >= w.seq_no.unwrap());
    let f = follower(&p, 1).await;
    assert_eq!(f["in_sync"], false, "{f}");
    assert!(f["error"].as_str().unwrap().contains("Unavailable"), "{f}");
    let back = create(dir.path(), "back.strata");
    replica_on(back.clone(), None, Some(TcpListener::bind(addr).await.unwrap())).await;
    until("the replica back is in sync", || async { follower(&p, 1).await["in_sync"] == true }).await;
    assert_eq!(rows(&back), rows(&pdb));
    // A replica that does not acknowledge within the write's deadline
    // drops out of sync, and the write is answered.
    let held = stall(&adb);
    let t0 = Instant::now();
    let w = p.write_ops(vec![set("p0000001", "price", json!(2.0))], None).await.unwrap();
    assert!(t0.elapsed() >= Duration::from_secs(2), "it waited for the replica: {} ms", ms(t0.elapsed()));
    assert!(last(&back) >= w.seq_no.unwrap());
    let f = follower(&p, 0).await;
    assert!(f["in_sync"] == false && f["error"].as_str().unwrap().contains(&format!("did not acknowledge transaction {} within the write's deadline", w.seq_no.unwrap())), "{f}");
    // Out of sync, it holds up no write.
    let t0 = Instant::now();
    p.write_ops(vec![set("p0000001", "price", json!(3.0))], None).await.unwrap();
    assert!(t0.elapsed() < Duration::from_secs(1), "{} ms", ms(t0.elapsed()));
    drop(held);
    until("the stalled replica is in sync", || async { follower(&p, 0).await["in_sync"] == true }).await;
    assert_eq!(rows(&adb), rows(&pdb));

    // majority of three copies: one replica stalled is no matter.
    let (bdb, cdb) = (create(dir.path(), "b.strata"), create(dir.path(), "c.strata"));
    let ((_b, ub), (_c, uc)) = (replica(bdb.clone(), None).await, replica(cdb.clone(), None).await);
    let qdb = create(dir.path(), "q.strata");
    let q = primary(qdb.clone(), None, ships_to(&[&ub, &uc], Acks::Majority), Duration::from_secs(1));
    q.write_ops(vec![set("p0000002", "sold", json!(1))], None).await.unwrap();
    let held_b = stall(&bdb);
    let w = q.write_ops(vec![set("p0000002", "sold", json!(2))], None).await.unwrap();
    assert!(last(&cdb) >= w.seq_no.unwrap());
    // Both stalled: the write is durable on the primary, but answered 504.
    let held_c = stall(&cdb);
    let op = vec![set("p0000002", "sold", json!(3))];
    let e = q.write_ops(op.clone(), Some("k3".into())).await.unwrap_err();
    let seq = last(&qdb);
    assert!(e.status == 504 && e.message.contains(&format!("durable on the primary as sequence number {seq}, but 1 of 3 copies held it")), "{}", e.message);
    // Once their calls fail, too few copies are in sync, and a write is
    // refused before it is applied.
    until("both replicas are out of sync", || async { follower(&q, 0).await["in_sync"] == false && follower(&q, 1).await["in_sync"] == false }).await;
    let e = q.write_ops(vec![set("p0000002", "sold", json!(4))], None).await.unwrap_err();
    assert!(e.status == 503 && e.message.contains("1 of 3 copies are in sync, and --write-acks majority needs 2: the write was not applied"), "{}", e.message);
    assert_eq!(last(&qdb), seq, "the refused write took no number");
    // A retry of the write answered 504 is not a new write: it waits for
    // the copies again.
    let e = q.write_ops(op.clone(), Some("k3".into())).await.unwrap_err();
    assert!(e.status == 504 && e.message.contains(&format!("sequence number {seq},")), "{}", e.message);
    // Back: a retry of the write answered 504 answers its first answer,
    // once a majority holds it.
    drop((held_b, held_c));
    until("the replicas are in sync", || async { follower(&q, 0).await["in_sync"] == true && follower(&q, 1).await["in_sync"] == true }).await;
    let w = q.write_ops(op, Some("k3".into())).await.unwrap();
    assert_eq!(w.seq_no, Some(seq));
    assert_eq!(last(&qdb), seq, "applied once");
    assert_eq!(rows(&bdb), rows(&qdb));
    assert_eq!(rows(&cdb), rows(&qdb));

    // primary: answered at once, the replica getting it after.
    let ddb = create(dir.path(), "d.strata");
    let (_d, ud) = replica(ddb.clone(), None).await;
    let sdb = create(dir.path(), "s.strata");
    let s = primary(sdb.clone(), None, ships_to(&[&ud], Acks::Primary), Duration::from_secs(5));
    s.write_ops(vec![set("p0000005", "sold", json!(1))], None).await.unwrap();
    let held = stall(&ddb);
    let t0 = Instant::now();
    let w = s.write_ops(vec![set("p0000005", "sold", json!(2))], None).await.unwrap();
    assert!(t0.elapsed() < Duration::from_secs(1), "{} ms", ms(t0.elapsed()));
    assert!(last(&ddb) < w.seq_no.unwrap());
    drop(held);
    until("the replica gets the write", || async { last(&ddb) == w.seq_no.unwrap() }).await;
    assert_eq!(rows(&ddb), rows(&sdb));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replica_follows_one_primary_a_term_and_fences_an_older_one() {
    let dir = tempfile::tempdir().unwrap();
    let rdb = create(dir.path(), "r.strata");
    let file = replication::file_of(&dir.path().join("r.strata"));
    let (_r, ur) = replica(rdb.clone(), Some(&file)).await;
    let p1 = primary(create(dir.path(), "p1.strata"), None, ships_to(&[&ur], Acks::All), Duration::from_secs(5));
    let w = p1.write_ops(vec![set("p0000001", "sold", json!(1))], None).await.unwrap();
    assert_eq!(last(&rdb), w.seq_no.unwrap());
    let id1 = p1.health().await["replication"]["id"].as_str().unwrap().to_string();

    // A second primary in the same term is refused: the two would ship
    // different histories.
    let p2 = primary(create(dir.path(), "p2.strata"), None, ships_to(&[&ur], Acks::All), Duration::from_secs(5));
    p2.write_ops(vec![set("p0000002", "sold", json!(2))], None).await.unwrap();
    let f = follower(&p2, 0).await;
    assert!(f["in_sync"] == false && f["error"].as_str().unwrap().contains(&format!("follows primary {id1} in term 1")), "{f}");
    assert_eq!(last(&rdb), 1);
    // The replica keeps whom it follows: opened again, it still refuses.
    let again = Follow::open(rdb.clone(), Some(&file)).unwrap();
    assert_eq!(again.follows().await, Some(id1.clone()));
    let e = again.replicate(pb::ReplicateRequest { term: 1, prev_term: 1, prev_seq: 1, primary: "another".into(), ..Default::default() }).await.unwrap_err();
    assert_eq!(e.status, 409, "{}", e.message);

    // A replica of a newer term (as one is when promoted) fences the
    // primary of the older one, which then takes no writes.
    rdb.storage().begin_term(2).unwrap();
    until("the first primary is fenced", || async { p1.health().await["replication"]["replaced_by_term"] == 2 }).await;
    let e = p1.write_ops(vec![set("p0000001", "sold", json!(3))], None).await.unwrap_err();
    assert!(e.status == 409 && e.message.contains("a replica holds term 2, newer than this primary's (1): another primary replaced this one"), "{}", e.message);
    let f = follower(&p1, 0).await;
    assert!(f["error"].as_str().unwrap().contains("holds term 2"), "{f}");
    assert_eq!(last(&rdb), 1, "the replica applied nothing from an older term");
    // The primary of a newer term is followed, and kept.
    let a = again.replicate(pb::ReplicateRequest { term: 3, prev_term: 1, prev_seq: 1, primary: "p3".into(), ..Default::default() }).await.unwrap();
    assert!(a.matched && a.replica_term == 3, "{a:?}");
    assert_eq!(Follow::open(rdb.clone(), Some(&file)).unwrap().follows().await, Some("p3".into()));
    // An older term's primary is answered with the newer term, and
    // changes nothing.
    let a = again.replicate(pb::ReplicateRequest { term: 1, prev_term: 1, prev_seq: 1, primary: id1.clone(), ..Default::default() }).await.unwrap();
    assert!(!a.matched && a.replica_term == 3, "{a:?}");
    assert_eq!(Follow::open(rdb.clone(), Some(&file)).unwrap().follows().await, Some("p3".into()));
}

/// Copies of a database that `dir` holds beside it, which a copy leaves
/// only while it runs.
fn copies_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.contains(".copy-") || n.contains(".restore-")).collect()
}

/// Sends `data`, said to be `size` bytes, to `url`'s `Restore` in chunks of
/// 1,000 bytes, as primary `primary` of `term`.
async fn restore(url: &str, term: u64, primary: &str, size: u64, data: &[u8]) -> Result<pb::ReplicateResponse, tonic::Status> {
    let mut c = pb::strata_client::StrataClient::connect(url.to_string()).await.unwrap();
    let mut chunks: Vec<pb::RestoreChunk> = data.chunks(1000).map(|d| pb::RestoreChunk { data: d.to_vec(), ..Default::default() }).collect();
    if chunks.is_empty() {
        chunks.push(pb::RestoreChunk::default());
    }
    chunks[0] = pb::RestoreChunk { term, primary: primary.into(), size, ..chunks[0].clone() };
    c.restore(futures::stream::iter(chunks)).await.map(|r| r.into_inner())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replica_the_log_does_not_reach_is_sent_a_copy() {
    let dir = tempfile::tempdir().unwrap();
    let [(fresh, fresh_at), (dir_layout, dir_at), (other, other_at), (restored, restored_at), (ahead, ahead_at)] =
        [nobody().await, nobody().await, nobody().await, nobody().await, nobody().await];
    // A ship log of a few transactions, and copies sent in chunks of 64 KiB.
    let pdb = create_with(dir.path(), "p.strata", 2000, |_| true);
    let opts = PrimaryOptions { copy_chunk_bytes: 64 << 10, copy_retry: Duration::from_millis(300), ..ships_to(&[&fresh, &dir_layout, &other, &restored, &ahead], Acks::All) };
    let p = primary(pdb.clone(), None, opts, Duration::from_secs(5));
    let pu = grpc(p.clone()).await;
    for i in 0..100 {
        p.write_ops(vec![set(&format!("p{:07}", i), "sold", json!(i))], None).await.unwrap();
    }
    let id = p.health().await["replication"]["id"].as_str().unwrap().to_string();

    // Replicas from before the first transaction the log holds are sent a
    // copy of the primary while it takes writes, one in each layout, and
    // then follow it.
    let fdb = create(dir.path(), "fresh.strata");
    let ddb = {
        let o = DbOptions { layout: Layout::Directory, ..options(64 << 20) };
        let db = Db::create(&dir.path().join("dir.strata"), demo::schema(N), o).unwrap();
        db.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
        db.storage().start_seq_no().unwrap();
        Arc::new(db)
    };
    let writing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let writer = {
        let (p, writing) = (p.clone(), writing.clone());
        tokio::spawn(async move {
            let mut i = 0;
            while writing.load(std::sync::atomic::Ordering::Relaxed) {
                p.write_ops(vec![set(&format!("p{:07}", i % N), "reviews", json!(i))], None).await.unwrap();
                i += 1;
            }
            i
        })
    };
    let (fr, _) = replica_on(fdb.clone(), None, Some(TcpListener::bind(fresh_at).await.unwrap())).await;
    let (dr, _) = replica_on(ddb.clone(), None, Some(TcpListener::bind(dir_at).await.unwrap())).await;
    until("both are sent a copy", || async { follower(&p, 0).await["copies"] == 1 && follower(&p, 1).await["copies"] == 1 }).await;
    writing.store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(writer.await.unwrap() > 0);
    let end = last(&pdb);
    until("both hold the primary's last", || async { follower(&p, 0).await["seq_no"] == end && follower(&p, 1).await["seq_no"] == end }).await;
    for (k, db, r) in [(0, &fdb, &fr), (1, &ddb, &dr)] {
        let f = follower(&p, k).await;
        assert!(f["in_sync"] == true && f["copying"] == false && f.get("error").is_none(), "{f}");
        assert_eq!(rows(db), rows(&pdb), "the copy holds the primary's rows, each with the primary's number");
        assert_eq!(r.health().await["replication"]["restores"], 1);
        assert!(r.prometheus().await.contains("strata_replica_restores_total 1"));
        db.storage().check().unwrap();
    }
    assert!(p.prometheus().await.contains(&format!("strata_replica_copies_total{{replica=\"{fresh}\"}} 1")));
    // Then shipped to as any replica.
    p.write_ops(vec![set("p0000001", "sold", json!(-1))], None).await.unwrap();
    assert_eq!(last(&fdb), last(&pdb));
    assert_eq!(rows(&ddb), rows(&pdb));

    // One whose tables are not the primary's is not changed by the copy.
    let odb = {
        let schema = strata::Schema::new("products", vec![strata::Field::new("id", strata::FieldType::Keyword)], Some("id")).unwrap();
        let db = Db::create(&dir.path().join("other.strata"), schema, options(64 << 20)).unwrap();
        db.storage().start_seq_no().unwrap();
        Arc::new(db)
    };
    replica_on(odb.clone(), None, Some(TcpListener::bind(other_at).await.unwrap())).await;
    until("the copy is refused", || async { follower(&p, 2).await["error"].as_str().is_some_and(|e| e.contains("sending it a copy of the primary failed")) }).await;
    let f = follower(&p, 2).await;
    assert!(f["in_sync"] == false && f["copies"] == 0 && f["error"].as_str().unwrap().contains("the copy's tables (products) are not this database's (products), or their schemas differ"), "{f}");
    assert_eq!(odb.snapshot().n_live_docs(), 0);

    // One started from a checkpoint of the primary follows from there,
    // with no copy.
    let copy = dir.path().join("restored.strata");
    pdb.storage().checkpoint(&copy, &CheckpointOptions::default()).unwrap();
    for i in 100..103 {
        p.write_ops(vec![set(&format!("p{:07}", i), "sold", json!(i))], None).await.unwrap();
    }
    let rdb = Arc::new(Db::open(&copy, options(1 << 20)).unwrap());
    rdb.set_embedder("emb", Arc::new(strata::embed::HashEmbedder { dim: demo::DIM })).unwrap();
    replica_on(rdb.clone(), None, Some(TcpListener::bind(restored_at).await.unwrap())).await;
    until("the restored replica is in sync", || async { follower(&p, 3).await["in_sync"] == true }).await;
    assert_eq!(rows(&rdb), rows(&pdb));
    assert_eq!(follower(&p, 3).await["copies"], 0);
    // One that took writes of its own past the primary's is not a copy,
    // and is not sent one.
    let adb = create(dir.path(), "ahead.strata");
    let far = last(&pdb) + 20;
    for i in 0..far {
        let mut t = adb.storage().begin().unwrap();
        t.set(&Value::Str(format!("p{:07}", i as usize % N)), "sold", Value::Int(1)).unwrap();
        t.commit().unwrap();
    }
    replica_on(adb.clone(), None, Some(TcpListener::bind(ahead_at).await.unwrap())).await;
    until("the replica ahead is found to differ", || async { follower(&p, 4).await["error"].as_str().is_some_and(|e| e.contains("its history differs")) }).await;
    assert_eq!((last(&adb), follower(&p, 4).await["copies"].clone()), (far, json!(0)), "it applied nothing");

    // Copies refused, changing nothing: to a server that is not a replica,
    // from a primary of an older term or a second one of the term, cut
    // short, damaged, or not a database.
    let before = rows(&fdb);
    let image = {
        let to = dir.path().join("image.strata");
        pdb.storage().checkpoint(&to, &CheckpointOptions::default()).unwrap();
        std::fs::read(&to).unwrap()
    };
    let size = image.len() as u64;
    let e = restore(&pu, 1, &id, size, &image).await.unwrap_err();
    assert!(e.message().contains("not a replica"), "{e}");
    let fu = &fresh;
    let r = restore(fu, 0, "old", size, &image).await.unwrap();
    assert_eq!((r.matched, r.seq, r.replica_term), (false, last(&fdb), 1), "an older term's primary is told the replica's");
    let e = restore(fu, 1, "another", size, &image).await.unwrap_err();
    assert!(e.message().contains("a shard has one primary in a term"), "{e}");
    let e = restore(fu, 1, &id, size + 5, &image).await.unwrap_err();
    assert!(e.message().contains(&format!("the copy ended at {size} of its {} bytes", size + 5)), "{e}");
    let e = restore(fu, 1, &id, size - 5, &image).await.unwrap_err();
    assert!(e.message().contains("the copy is longer than"), "{e}");
    // A byte that only the segments' checksums find wrong, and one that
    // opening finds.
    let damaged = (1..image.len() / 4096)
        .find_map(|page| {
            let mut d = image.clone();
            d[page * 4096 + 2000] ^= 0x5a;
            let at = dir.path().join(format!("damaged-{page}.strata"));
            std::fs::write(&at, &d).unwrap();
            let clean = |verify_checksums| Database::open(&at, DbOptions { verify_checksums, ..DbOptions::default() }).is_ok_and(|db| db.damaged().is_none());
            let found = clean(false) && !clean(true);
            for p in [at.clone(), dir.path().join(format!("damaged-{page}.strata-wal"))] {
                let _ = std::fs::remove_file(p);
            }
            found.then_some(d)
        })
        .expect("a byte only the checksums find");
    let e = restore(fu, 1, &id, size, &damaged).await.unwrap_err();
    assert!(e.message().starts_with("the copy"), "{e}");
    let mut damaged = image.clone();
    for b in damaged.iter_mut().skip(100).step_by(4096) {
        *b ^= 0x5a;
    }
    let e = restore(fu, 1, &id, size, &damaged).await.unwrap_err();
    assert!(e.message().starts_with("the copy: "), "{e}");
    let e = restore(fu, 1, &id, 3, b"abc").await.unwrap_err();
    assert!(e.message().starts_with("the copy: "), "{e}");
    assert_eq!(rows(&fdb), before);
    assert_eq!(fr.health().await["replication"]["restores"], 1);
    // The copy of the primary itself is taken as the copy the primary sends.
    let r = restore(fu, 1, &id, size, &image).await.unwrap();
    assert!(r.matched && r.replica_term == 1, "{r:?}");
    assert_eq!(rows(&fdb), rows(&pdb));
    assert!(copies_in(dir.path()).is_empty(), "{:?}", copies_in(dir.path()));
}

/// Waits until a replica in `dir` holds all of the copy its primary sends
/// beside it, and is installing it.
async fn copy_arrived(dir: &Path) {
    until("the replica holds the whole copy", || async {
        let sizes = |what: &str| std::fs::read_dir(dir).unwrap().map(|e| e.unwrap()).filter(|e| e.file_name().to_string_lossy().contains(what)).map(|e| e.metadata().unwrap().len()).collect::<Vec<_>>();
        // The copy, and the log that opening it made.
        let (sent, got) = (sizes(".copy-"), sizes(".restore-"));
        sent.len() == 1 && got.contains(&sent[0])
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_is_followed_by_what_was_written_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let ((near, near_at), (far, far_at)) = (nobody().await, nobody().await);
    let pdb = create_with(dir.path(), "p.strata", 2000, |_| true);
    let p = primary(pdb.clone(), None, ships_to(&[&near, &far], Acks::All), Duration::from_secs(5));
    let write = |from: usize, n: usize| {
        let p = p.clone();
        async move {
            for i in from..from + n {
                p.write_ops(vec![set(&format!("p{:07}", i % N), "sold", json!(i))], None).await.unwrap();
            }
        }
    };
    write(0, 100).await;
    // The replica's writer is held while it installs the copy, and the
    // primary takes more than its 2,000-byte log meanwhile: it holds them,
    // and the replica follows from the copy.
    let ndb = create(dir.path(), "near.strata");
    let held = stall(&ndb);
    let (nr, _) = replica_on(ndb.clone(), None, Some(TcpListener::bind(near_at).await.unwrap())).await;
    copy_arrived(dir.path()).await;
    let from = last(&pdb) + 1;
    write(100, 40).await;
    let LogRead::Txns(meanwhile) = pdb.storage().log_from(from, usize::MAX) else { panic!("the log holds what follows the copy") };
    let bytes: usize = meanwhile.iter().map(|t| t.bytes.len()).sum();
    assert!(meanwhile.len() == 40 && bytes > 2000 && bytes < 4 * 2000, "{} transactions, {bytes} bytes", meanwhile.len());
    drop(held);
    until("the replica is in sync", || async { follower(&p, 0).await["in_sync"] == true }).await;
    assert_eq!((follower(&p, 0).await["copies"].clone(), nr.health().await["replication"]["restores"].clone()), (json!(1), json!(1)), "one copy");
    assert_eq!(rows(&ndb), rows(&pdb));
    // Released once it caught up: the log keeps its 2,000 bytes again.
    write(200, 30).await;
    assert!(matches!(pdb.storage().log_from(from, usize::MAX), LogRead::Gone { .. }));

    // Past four times the log, what came meanwhile is gone, and the
    // replica is sent another copy.
    let fdb = create(dir.path(), "far.strata");
    let held = stall(&fdb);
    let (fr, _) = replica_on(fdb.clone(), None, Some(TcpListener::bind(far_at).await.unwrap())).await;
    copy_arrived(dir.path()).await;
    write(300, 200).await;
    drop(held);
    until("the replica is in sync", || async { follower(&p, 1).await["in_sync"] == true }).await;
    assert_eq!((follower(&p, 1).await["copies"].clone(), fr.health().await["replication"]["restores"].clone()), (json!(2), json!(2)), "two copies");
    assert_eq!(rows(&fdb), rows(&pdb));
    assert!(copies_in(dir.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replica_whose_last_transaction_has_another_term_differs() {
    let dir = tempfile::tempdir().unwrap();
    let pdb = create(dir.path(), "p.strata");
    pdb.storage().begin_term(2).unwrap();
    let (url, at) = nobody().await;
    let p = primary(pdb.clone(), None, ships_to(&[&url], Acks::All), Duration::from_secs(5));
    for i in 0..5 {
        p.write_ops(vec![set(&format!("p{i:07}"), "sold", json!(i))], None).await.unwrap();
    }
    // Its third transaction is of term 1; the primary's third, of term 2.
    let odb = create(dir.path(), "o.strata");
    for i in 0..3 {
        let mut t = odb.storage().begin().unwrap();
        t.set(&Value::Str(format!("p{i:07}")), "sold", Value::Int(1)).unwrap();
        t.commit().unwrap();
    }
    replica_on(odb.clone(), None, Some(TcpListener::bind(at).await.unwrap())).await;
    until("the replica is found to differ", || async { follower(&p, 0).await["error"].as_str().is_some_and(|e| e.contains("its last transaction, 3 of term 1, is not this primary's")) }).await;
    assert_eq!(last(&odb), 3, "it applied nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coordinator_writes_to_each_shards_primary() {
    let dir = tempfile::tempdir().unwrap();
    let key = |i: usize| Value::Str(format!("p{i:07}"));
    let mut shards = Vec::new();
    let mut copies = Vec::new();
    for s in 0..2 {
        let keep = |i: usize| shard_of(&key(i), 2) == s;
        let rdb = create_with(dir.path(), &format!("r{s}.strata"), 1 << 20, keep);
        let (_r, ur) = replica(rdb.clone(), None).await;
        let pdb = create_with(dir.path(), &format!("p{s}.strata"), 1 << 20, keep);
        let pu = grpc(primary(pdb.clone(), None, ships_to(&[&ur], Acks::All), Duration::from_secs(5))).await;
        // The replica listed first in one shard, second in the other.
        shards.push(if s == 0 { format!("{pu}|{ur}") } else { format!("{ur}|{pu}") });
        copies.push((pdb, rdb));
    }
    let c = Coordinator::connect(shards.clone()).await.unwrap();
    let schema = c.schema.clone();
    let coord = Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(10)));
    let ops: Vec<Json> = (0..20).map(|i| set(&format!("p{i:07}"), "sold", json!(100 + i))).collect();
    assert_eq!(coord.write_ops(ops, None).await.unwrap().rows, 20);
    written(coord.sql("UPDATE products SET reviews = reviews + 1 WHERE category = 'kitchen'", &Params::new(), 0).await.unwrap());
    for (pdb, rdb) in &copies {
        assert!((1..=2).contains(&last(pdb)), "each primary took the writes once: {}", last(pdb));
        assert_eq!(rows(rdb), rows(pdb), "each replica holds its primary's rows");
    }
    // seq_no() and if_seq_no pass through: the copies of a shard number
    // alike.
    let seq_of = |id: &'static str| {
        let coord = coord.clone();
        async move { coord.query(QueryReq { sql: format!("SELECT seq_no() FROM products WHERE id = '{id}'"), ..Default::default() }).await.unwrap().rs.rows[0][0].as_i64().unwrap() as u64 }
    };
    let n = seq_of("p0000003").await;
    assert!(n > 0);
    let cas = |n: u64| vec![json!({"op": "add", "key": "p0000003", "field": "sold", "value": 1, "if_seq_no": n})];
    coord.write_ops(cas(n), None).await.unwrap();
    let e = coord.write_ops(cas(n), None).await.unwrap_err();
    assert_eq!(e.status, 409, "{}", e.message);
    let h = coord.health().await;
    let roles: Vec<Json> = (0..2).map(|i| h["shards"][i]["replicas"].as_array().unwrap().iter().map(|r| r["role"].clone()).collect()).collect();
    assert_eq!(roles, [json!(["primary", "replica"]), json!(["replica", "primary"])], "{h}");

    // A shard with replicas and no primary answering takes no writes.
    let lone = shards.iter().enumerate().map(|(i, s)| s.split('|').nth(if i == 0 { 1 } else { 0 }).unwrap().to_string()).collect();
    let c = Coordinator::connect(lone).await.unwrap();
    let e = c.write_ops(vec![set("p0000003", "sold", json!(1))], None).await.unwrap_err();
    assert!(e.status == 503 && e.message.contains("has no primary answering"), "{}", e.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_starts_as_a_primary_or_a_replica() {
    let dir = tempfile::tempdir().unwrap();
    let opts = |name: &str| server::Options { db: Some(dir.path().join(name)), demo: Some(20), seq_no: true, timeout_ms: 5000, ..Default::default() };
    let r = server::service(&server::Options { replica: true, ..opts("r.strata") }).await.unwrap();
    assert_eq!(r.role(), pb::Role::Replica);
    assert!(replication::file_of(&dir.path().join("r.strata")).exists(), "a replica keeps its id");
    let ur = grpc(r.clone()).await;
    let p = server::service(&server::Options { replicas: vec![ur], write_acks: Acks::Majority, ..opts("p.strata") }).await.unwrap();
    assert_eq!(p.role(), pb::Role::Primary);
    assert!(p.local_db().unwrap().storage().keeps_ship_log());
    let w = p.write_ops(vec![set("p0000001", "sold", json!(7))], None).await.unwrap();
    assert_eq!(w.seq_no, Some(1));
    // With a majority of two copies, the replica holds it.
    let (pdb, rdb) = (p.local_db().unwrap(), r.local_db().unwrap());
    assert_eq!(last(rdb), 1);
    assert_eq!(rows(rdb), rows(pdb), "both loaded the same demo rows, and the write shipped");
    assert_eq!(p.health().await["replication"]["write_acks"], "majority");

    let e = server::service(&server::Options { replica: true, seq_no: false, ..opts("plain.strata") }).await.err().unwrap();
    assert!(e.contains("does not number its writes, which replication ships by: pass --seq-no"), "{e}");
    let e = server::service(&server::Options { replica: true, replicas: vec!["http://127.0.0.1:1".into()], ..opts("both.strata") }).await.err().unwrap();
    assert!(e.contains("not both"), "{e}");
    let e = server::service(&server::Options { replica: true, shards: vec!["http://127.0.0.1:1".into()], db: None, ..opts("x.strata") }).await.err().unwrap();
    assert!(e.contains("not the coordinator"), "{e}");
}
