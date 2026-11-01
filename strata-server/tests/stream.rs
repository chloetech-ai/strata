//! Streaming writes: `/v1/stream` and gRPC `WriteStream`, whose events
//! carry their source's offset. A server keeps each source's last offset
//! in the transaction that wrote its rows (`_sources`), so a stream that
//! resumes from it, or replays from before it, applies each event once:
//! on one server, through a coordinator (which resumes from the least
//! offset over its shards), on a replica, and after `kill -9`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::Schema;
use strata_server::auth::Access;
use strata_server::coord::Coordinator;
use strata_server::replication::{Acks, Follow, Primary, PrimaryOptions};
use strata_server::service::{AgentConfig, Backend, Replication, Service};
use strata_server::{pb, server};
use tokio::net::TcpListener;

const USERS: usize = 50;
const SOURCE: &str = "clicks/0";

const DDL: [&str; 2] = [
    "CREATE TABLE users (id KEYWORD PRIMARY KEY, n INT DEFAULT 0)",
    "CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, ts INT) OPTIONS(shard_by = 'usr')",
];

fn items() -> Schema {
    Schema::from_json(r#"{"table": "items", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "title", "type": "text"}]}"#).unwrap()
}

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn options() -> DbOptions {
    DbOptions { segment_max_docs: 2000, publish_snapshots: true, ..DbOptions::default() }
}

/// A database with the users and events tables.
fn create(path: &std::path::Path, opts: DbOptions) -> Arc<Db> {
    let db = Db::create(path, items(), opts).unwrap();
    for ddl in DDL {
        db.sql(ddl, &Params::new()).unwrap();
    }
    Arc::new(db)
}

fn service(db: Arc<Db>) -> Service {
    let schema = (*db.schema()).clone();
    Service::new(Backend::Local(db), schema, agent(), 16, Duration::from_secs(20))
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

fn user(i: usize) -> String {
    format!("u{}", i % USERS)
}

fn users() -> Vec<Json> {
    (0..USERS).map(|u| json!({"op": "insert", "table": "users", "doc": {"id": user(u)}})).collect()
}

/// Event `i`'s operations: its row, and one more on its user's count.
fn event_ops(i: usize) -> Vec<Json> {
    vec![
        json!({"op": "insert", "table": "events", "doc": {"id": i, "usr": user(i), "item": format!("i{}", i % 97), "ts": i}}),
        json!({"op": "add", "table": "users", "key": user(i), "field": "n", "value": 1, "routing": user(i)}),
    ]
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// Event `i` as a line of `/v1/stream`.
fn line(i: usize) -> String {
    format!("{}\n", json!({"source": {"name": SOURCE, "offset": i, "ts": now_ms()}, "ops": event_ops(i)}))
}

/// Streams `lines` to `url`'s `/v1/stream`, a few at a time; returns its
/// answer's lines.
async fn stream(url: &str, lines: Vec<String>) -> Vec<Json> {
    let chunks: Vec<Result<String, std::io::Error>> = lines.chunks(37).map(|c| Ok(c.concat())).collect();
    let body = reqwest::Body::wrap_stream(futures::stream::iter(chunks));
    let r = reqwest::Client::new().post(format!("{url}/v1/stream")).header("content-type", "application/x-ndjson").body(body).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.text().await.unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

async fn post(url: &str, path: &str, body: Json) -> Json {
    reqwest::Client::new().post(format!("{url}{path}")).json(&body).send().await.unwrap().json().await.unwrap()
}

async fn get(url: &str, path: &str) -> Json {
    reqwest::get(format!("{url}{path}")).await.unwrap().json().await.unwrap()
}

/// The events and the users' counts' sum, through `/v1/sql`.
async fn totals(url: &str) -> (i64, i64) {
    let one = |j: Json| j["rows"][0][0].as_i64().unwrap_or(0);
    let events = one(post(url, "/v1/sql", json!({"sql": "SELECT COUNT(*) FROM events"})).await);
    let n = one(post(url, "/v1/sql", json!({"sql": "SELECT SUM(n) FROM users"})).await);
    (events, n)
}

fn offset_of(db: &Db, name: &str) -> Option<i64> {
    db.sources().unwrap().into_iter().find(|s| s.name == name).map(|s| s.offset)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_stream_applies_each_event_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = create(&dir.path().join("s.strata"), options());
    let url = http(Arc::new(service(db.clone()))).await;
    assert_eq!(post(&url, "/v1/write", json!({"ops": users()})).await["written"], USERS);
    assert_eq!(get(&url, "/v1/sources").await, json!({"sources": []}));

    // Each line is one event's two operations; every batch is answered.
    let a = stream(&url, (0..3000).map(line).collect()).await;
    let done = a.last().unwrap();
    assert_eq!((done["done"].as_bool(), done["lines"].as_u64(), done["written"].as_u64(), done["skipped"].as_u64()), (Some(true), Some(3000), Some(6000), Some(0)), "{done}");
    assert!(a[..a.len() - 1].iter().all(|b| b["batch"].is_u64() && b["error"].is_null()), "{a:?}");
    assert_eq!(a[a.len() - 2]["lines"], 3000);
    // Each answer gives where its batch left the source.
    assert_eq!(a[a.len() - 2]["offsets"], json!({SOURCE: 2999}));
    assert!(a[..a.len() - 1].windows(2).all(|w| w[0]["offsets"][SOURCE].as_i64() < w[1]["offsets"][SOURCE].as_i64()), "{a:?}");
    assert_eq!(get(&url, &format!("/v1/sources/{SOURCE}")).await["offset"], 2999);
    assert_eq!(totals(&url).await, (3000, 3000));

    // A replay from before the last offset skips what was written.
    let a = stream(&url, (2000..4000).map(line).collect()).await;
    let done = a.last().unwrap();
    assert_eq!((done["written"].as_u64(), done["skipped"].as_u64()), (Some(2000), Some(2000)), "{done}");
    assert_eq!(totals(&url).await, (4000, 4000));
    assert_eq!(offset_of(&db, SOURCE), Some(3999));

    // So does a plain write; its answer says so.
    let w = post(&url, "/v1/write", json!({"ops": sourced(10)})).await;
    assert_eq!((w["written"].as_u64(), w["skipped"].as_u64()), (Some(0), Some(2)), "{w}");

    // Offsets increase along a stream: an event's operations go together,
    // not in two lines, which two batches could split. The lines before
    // the bad one are written, and the answer counts them.
    let a = stream(&url, vec![line(4000), line(4000), line(4001)]).await;
    let last = a.last().unwrap();
    assert!(last["error"].as_str().unwrap().contains("line 2: source clicks/0: offset 4000 comes after 4000"), "{a:?}");
    assert_eq!((last["status"].as_u64(), last["lines"].as_u64()), (Some(400), Some(1)));
    assert_eq!(totals(&url).await, (4001, 4001));

    // A line that is not an operation ends the stream there.
    let a = stream(&url, vec![line(4001), "[1, 2]\n".into(), line(4002)]).await;
    assert!(a.last().unwrap()["error"].as_str().unwrap().starts_with("line 2: a line is an operation"), "{a:?}");
    assert_eq!(offset_of(&db, SOURCE), Some(4001));

    // A source not written from is at no offset: start from the beginning.
    assert_eq!(get(&url, "/v1/sources/views/3").await, json!({"name": "views/3", "offset": null, "updated": null}));
    let all = get(&url, "/v1/sources").await;
    assert_eq!((all["sources"][0]["name"].as_str(), all["sources"][0]["offset"].as_i64()), (Some(SOURCE), Some(4001)), "{all}");
    assert!(all["sources"][0]["updated"].as_str().is_some_and(|t| t.ends_with('Z')), "{all}");

    // The system table is not one of the database's tables, and a client
    // does not write it.
    let tables = get(&url, "/v1/tables").await;
    assert_eq!(tables["tables"].as_array().unwrap().len(), 3, "{tables}");
    let r = post(&url, "/v1/write", json!({"ops": [{"op": "upsert", "table": "_sources", "doc": {"name": SOURCE, "last_offset": 0}}]})).await;
    assert!(r["error"].as_str().unwrap().contains("no such table: _sources"), "{r}");
    let r = post(&url, "/v1/sql", json!({"sql": "CREATE TABLE _sources (name KEYWORD PRIMARY KEY)"})).await;
    assert!(r["error"].as_str().unwrap().contains("system table"), "{r}");

    // Each event's lag was measured once, when its rows became visible.
    let m = reqwest::get(format!("{url}/metrics")).await.unwrap().text().await.unwrap();
    assert!(m.contains("strata_stream_lag_seconds_count 4002\n"), "{}", m.lines().filter(|l| l.contains("lag")).collect::<Vec<_>>().join("\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_coordinator_keeps_the_offsets_on_every_shard() {
    let dir = tempfile::tempdir().unwrap();
    let mut shards = Vec::new();
    let mut urls = Vec::new();
    for i in 0..3 {
        let db = Arc::new(Db::create(&dir.path().join(format!("s{i}.strata")), items(), options()).unwrap());
        let svc = Arc::new(service(db.clone()));
        urls.push(grpc(svc.clone()).await);
        shards.push((db, svc));
    }
    let c = Coordinator::connect(urls).await.unwrap();
    let schema = c.schema.clone();
    let url = http(Arc::new(Service::new(Backend::Cluster(Arc::new(c)), schema, agent(), 16, Duration::from_secs(20)))).await;
    for ddl in DDL {
        assert!(post(&url, "/v1/sql", json!({"sql": ddl})).await["error"].is_null());
    }
    post(&url, "/v1/write", json!({"ops": users()})).await;

    let a = stream(&url, (0..3000).map(line).collect()).await;
    let done = a.last().unwrap();
    assert_eq!((done["written"].as_u64(), done["skipped"].as_u64()), (Some(6000), Some(0)), "{done}");
    // Every shard keeps the offset, whether a batch gave it rows or not.
    for (db, _) in &shards {
        assert_eq!(offset_of(db, SOURCE), Some(2999));
        assert!(db.query("SELECT COUNT(*) FROM events", &Params::new()).unwrap().rows[0][0].as_i64().unwrap() > 0);
    }
    assert_eq!(get(&url, &format!("/v1/sources/{SOURCE}")).await["offset"], 2999);

    // A replay skips, on each shard, what that shard applied.
    let a = stream(&url, (1000..3500).map(line).collect()).await;
    let done = a.last().unwrap();
    assert_eq!((done["written"].as_u64(), done["skipped"].as_u64()), (Some(1000), Some(4000)), "{done}");
    assert_eq!(totals(&url).await, (3500, 3500));

    // A shard behind the others is where the stream resumes: the least
    // offset over the shards. A source some shard lacks is not listed.
    shards[1].1.write_ops(vec![json!({"op": "source", "source": {"name": SOURCE, "offset": 4100}})], None).await.unwrap();
    shards[0].1.write_ops(vec![json!({"op": "source", "source": {"name": "solo", "offset": 5}})], None).await.unwrap();
    let all = get(&url, "/v1/sources").await;
    assert_eq!(all["sources"].as_array().unwrap().len(), 1, "{all}");
    assert_eq!(all["sources"][0]["offset"], 3499, "{all}");
    assert_eq!(get(&url, "/v1/sources/solo").await["offset"], Json::Null);
}

fn msg(ops: Vec<Json>) -> pb::WriteRequest {
    pb::WriteRequest { ops_json: serde_json::to_vec(&ops).unwrap(), ..Default::default() }
}

/// Event `i`'s operations, each with its source.
fn sourced(i: usize) -> Vec<Json> {
    event_ops(i)
        .into_iter()
        .map(|mut op| {
            op["source"] = json!({"name": SOURCE, "offset": i});
            op
        })
        .collect()
}

/// gRPC `WriteStream`: each message a batch, answered once committed; the
/// first that fails ends the stream, and none after it is applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grpc_stream_stops_at_its_first_failure() {
    let dir = tempfile::tempdir().unwrap();
    let db = create(&dir.path().join("g.strata"), options());
    let svc = Arc::new(service(db.clone()));
    svc.write_ops(users(), None).await.unwrap();
    let url = grpc(svc).await;
    let mut c = pb::strata_client::StrataClient::connect(url).await.unwrap();

    let out = c.write_stream(futures::stream::iter((0..20).map(|i| msg(sourced(i))))).await.unwrap().into_inner();
    let answers: Vec<pb::WriteResponse> = out.map(|r| r.unwrap()).collect().await;
    assert_eq!(answers.len(), 20);
    assert!(answers.iter().all(|a| a.written == 2 && a.skipped == 0));
    // Each answer says where its sources stand.
    assert!(answers.iter().enumerate().all(|(i, a)| a.offsets.len() == 1 && a.offsets[SOURCE] == i as i64), "{answers:?}");

    // A bad operation in the second message: the third, sent before the
    // error came back, is not applied.
    let bad = vec![json!({"op": "insert", "table": "nope", "doc": {}, "source": {"name": SOURCE, "offset": 21}})];
    let mut out = c.write_stream(futures::stream::iter([msg(sourced(20)), msg(bad), msg(sourced(22))])).await.unwrap().into_inner();
    assert_eq!(out.next().await.unwrap().unwrap().written, 2);
    let e = out.next().await.unwrap().unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    assert!(e.message().contains("no such table: nope"), "{}", e.message());
    assert!(out.next().await.is_none());
    assert_eq!(offset_of(&db, SOURCE), Some(20));

    // A message whose offset does not pass the one before it.
    let mut out = c.write_stream(futures::stream::iter([msg(sourced(30)), msg(sourced(30))])).await.unwrap().into_inner();
    assert_eq!(out.next().await.unwrap().unwrap().written, 2);
    let e = out.next().await.unwrap().unwrap_err();
    assert!(e.message().contains("offset 30 comes after 30"), "{}", e.message());

    // The offsets, over gRPC.
    let s = c.sources(pb::SourcesRequest {}).await.unwrap().into_inner().sources;
    assert_eq!((s.len(), s[0].name.as_str(), s[0].offset), (1, SOURCE, 30));
    assert!(s[0].updated_micros > 0);
}

fn numbered(path: &std::path::Path, ship_log_bytes: usize) -> Arc<Db> {
    let db = create(path, DbOptions { ship_log_bytes, ..options() });
    db.storage().start_seq_no().unwrap();
    db
}

fn ships_to(replicas: &[&str]) -> PrimaryOptions {
    PrimaryOptions {
        replicas: replicas.iter().map(|u| u.to_string()).collect(),
        acks: Acks::Primary,
        heartbeat: Duration::from_millis(100),
        retry_max: Duration::from_millis(200),
        copy_retry: Duration::from_millis(300),
        ..Default::default()
    }
}

async fn replica_of(db: Arc<Db>) -> String {
    let mut svc = service(db.clone());
    svc.set_replication(Replication::Replica(Arc::new(Follow::open(db, None).unwrap())));
    grpc(Arc::new(svc)).await
}

async fn until(what: &str, f: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(30), "waited 30 s for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A replica makes the sources table when its primary's first offset
/// arrives, and a replica sent a copy takes the copy's offsets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicas_hold_their_primarys_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let shipped = numbered(&dir.path().join("r1.strata"), 64 << 20);
    let r1 = replica_of(shipped.clone()).await;
    // Its log holds a few transactions: a replica that starts later is
    // sent a copy.
    let pdb = numbered(&dir.path().join("p.strata"), 4000);
    let (copied_url, copied_at) = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        (format!("http://{a}"), a)
    };
    let mut p = service(pdb.clone());
    p.set_replication(Replication::Primary(Primary::start(pdb.clone(), None, ships_to(&[&r1, &copied_url])).unwrap()));
    let p = Arc::new(p);
    p.write_ops(users(), None).await.unwrap();
    for i in 0..200 {
        p.write_ops(sourced(i), None).await.unwrap();
    }
    // A replica's number reaches its primary's when it has applied the
    // last transaction, and its snapshot shows it a moment later.
    let seq = |db: &Db| db.storage().seq_no().unwrap().seq;
    until("the replica holds the primary's last", || seq(&shipped) == seq(&pdb) && offset_of(&shipped, SOURCE) == Some(199)).await;
    assert_eq!(shipped.sources().unwrap(), pdb.sources().unwrap());

    let copied = numbered(&dir.path().join("r2.strata"), 64 << 20);
    let mut svc = service(copied.clone());
    svc.set_replication(Replication::Replica(Arc::new(Follow::open(copied.clone(), None).unwrap())));
    tokio::spawn(server::serve_grpc(Arc::new(svc), TcpListener::bind(copied_at).await.unwrap(), Access::default()));
    let t0 = Instant::now();
    while seq(&copied) != seq(&pdb) || offset_of(&copied, SOURCE) != Some(199) {
        if t0.elapsed() > Duration::from_secs(20) {
            panic!("the late replica: {}", p.health().await["replication"]);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(copied.sources().unwrap(), pdb.sources().unwrap());
    let n = |db: &Db| db.query("SELECT COUNT(*) FROM events", &Params::new()).unwrap().rows[0][0].as_i64().unwrap();
    assert_eq!((n(&copied), n(&shipped)), (200, 200));
}

/// The server binary killed (SIGKILL) mid-stream and started again: the
/// stream resumes from the offset it kept, and every event is applied
/// once, its row and its user's count alike.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_9_mid_stream_neither_loses_nor_duplicates() {
    const EVENTS: usize = 60_000;
    let dir = tempfile::tempdir().unwrap();
    let schema = dir.path().join("items.json");
    std::fs::write(&schema, serde_json::to_string(&items()).unwrap()).unwrap();
    let path = dir.path().join("k.strata");
    let start = |first: bool| {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_strata-server"));
        cmd.arg("--db").arg(&path).args(["--listen", &format!("127.0.0.1:{port}"), "--grpc", "off", "--log", "warn"]);
        if first {
            cmd.arg("--schema").arg(&schema);
        }
        (cmd.spawn().unwrap(), format!("http://127.0.0.1:{port}"))
    };
    let up = |url: String| async move {
        let t0 = Instant::now();
        while reqwest::get(format!("{url}/v1/health")).await.is_err() {
            assert!(t0.elapsed() < Duration::from_secs(30), "the server did not start");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let (mut child, url) = start(true);
    up(url.clone()).await;
    for ddl in DDL {
        assert!(post(&url, "/v1/sql", json!({"sql": ddl})).await["error"].is_null());
    }
    post(&url, "/v1/write", json!({"ops": users()})).await;

    // The stream, killed once some of its batches are answered.
    let lines: Vec<Result<String, std::io::Error>> = (0..EVENTS).collect::<Vec<_>>().chunks(50).map(|c| Ok(c.iter().map(|&i| line(i)).collect::<String>())).collect();
    let body = reqwest::Body::wrap_stream(futures::stream::iter(lines).then(|c| async {
        tokio::time::sleep(Duration::from_micros(200)).await;
        c
    }));
    let r = reqwest::Client::new().post(format!("{url}/v1/stream")).header("content-type", "application/x-ndjson").body(body).send().await.unwrap();
    let mut answers = r.bytes_stream();
    let mut acked = 0;
    let mut text = String::new();
    while acked < 5 {
        let Some(Ok(b)) = answers.next().await else { break };
        text.push_str(std::str::from_utf8(&b).unwrap());
        acked = text.matches("\"batch\"").count();
    }
    assert!(acked >= 5, "the stream ended before the kill: {text}");
    child.kill().unwrap();
    child.wait().unwrap();

    let (mut child, url) = start(false);
    up(url.clone()).await;
    let at = get(&url, &format!("/v1/sources/{SOURCE}")).await["offset"].as_i64().unwrap();
    assert!(at > 0 && (at as usize) < EVENTS - 1, "killed at offset {at}: not mid-stream");
    let (events, n) = totals(&url).await;
    assert_eq!((events, n), (at + 1, at + 1), "the rows written are the events up to the offset kept");
    // Resumed from a little before it, as a consumer that committed its
    // own position late would.
    let from = (at as usize).saturating_sub(500);
    let a = stream(&url, (from..EVENTS).map(line).collect()).await;
    let done = a.last().unwrap();
    let replayed = (at as usize + 1 - from) as u64;
    assert_eq!((done["skipped"].as_u64(), done["written"].as_u64()), (Some(2 * replayed), Some(2 * (EVENTS as u64 - at as u64 - 1))), "{done}");
    assert_eq!(totals(&url).await, (EVENTS as i64, EVENTS as i64));
    child.kill().unwrap();
    child.wait().unwrap();
}
