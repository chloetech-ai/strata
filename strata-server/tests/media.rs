//! MEDIA fields through a server: what writes may fetch (`--media-fetch`:
//! `file:` roots and listed hosts, no redirect off the list, a size cap
//! and a timeout), the media store, the server's write paths (JSON
//! operations, SQL, imports), and replicas, which apply the resolved rows
//! their primary ships with their vectors, and never fetch.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::media::{sha256_hex, Allowlist, Media, MediaKind, Resolver};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema, VectorIndexKind};
use strata::storage::DbOptions;
use strata::value::Value;
use strata_server::auth::Access;
use strata_server::keys::Principal;
use strata_server::media::HttpFetcher;
use strata_server::replication::{Acks, Follow, Primary, PrimaryOptions};
use strata_server::service::{After, AgentConfig, Backend, Replication, Service};
use strata_server::server;
use tokio::net::TcpListener;

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn photos() -> Schema {
    let emb = Field::new("emb", FieldType::Vector { dim: 16, index: VectorIndexKind::Flat }).embedded("hash:16", &["title"]).not_stored();
    Schema::new("photos", vec![Field::new("id", FieldType::Keyword), Field::new("title", FieldType::Text { positions: true }), Field::media("image", MediaKind::Image), emb], Some("id")).unwrap()
}

/// A PNG header of a `w` by `h` image.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

fn create(dir: &std::path::Path, name: &str) -> Arc<Db> {
    let db = Db::create(&dir.join(name), photos(), DbOptions { publish_snapshots: true, sync: false, ship_log_bytes: 64 << 20, ..DbOptions::default() }).unwrap();
    db.storage().start_seq_no().unwrap();
    Arc::new(db)
}

/// A service of `db` that embeds `emb` with its model.
fn service(db: Arc<Db>) -> Service {
    let mut svc = Service::new(Backend::Local(db.clone()), db.schema().clone(), agent(), 16, Duration::from_secs(10));
    svc.open_models(std::path::Path::new("/nonexistent"));
    svc
}

/// Each row's id, image and vector, by id.
fn rows(db: &Db) -> Vec<(String, Media, Option<Vec<f32>>)> {
    let r = db.query("SELECT id, image, emb FROM photos LIMIT 1000", &Params::new()).unwrap();
    let mut out: Vec<_> = r.rows.into_iter().map(|r| (r[0].as_str().unwrap().to_string(), Media::parse(r[1].as_str().unwrap()).unwrap(), r[2].as_vector().map(<[f32]>::to_vec))).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn insert(id: &str, image: Json) -> Json {
    json!({"op": "insert", "doc": {"id": id, "title": format!("photo {id}"), "image": image}})
}

/// An HTTP server of images, and of the ways a fetch goes wrong; its
/// redirects send to `elsewhere`.
async fn files(elsewhere: String) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let here = base.clone();
    let app = axum::Router::new()
        .route("/a.png", get(|| async { ([("content-type", "image/png")], png(4, 3)) }))
        .route("/big.png", get(|| async { png(4, 3).repeat(10) }))
        .route("/stream.png", get(|| async { axum::body::Body::from_stream(futures::stream::iter((0..10).map(|_| Ok::<_, std::io::Error>(png(4, 3))))) }))
        .route("/to-a", get(move || async move { axum::response::Redirect::temporary(&format!("{here}/a.png")) }))
        .route("/away", get(move || async move { axum::response::Redirect::temporary(&format!("{elsewhere}/a.png")) }))
        .route(
            "/slow.png",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                png(1, 1)
            }),
        )
        .route("/missing.png", get(|| async { (axum::http::StatusCode::NOT_FOUND, "no").into_response() }));
    tokio::spawn(async move { axum::serve(l, app).await });
    base
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_fetch_what_the_list_allows() {
    let dir = tempfile::tempdir().unwrap();
    let (root, store) = (dir.path().join("media"), dir.path().join("store"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("b.png"), png(8, 8)).unwrap();
    let other = files("http://127.0.0.1:9".into()).await;
    let cdn = files(other.clone()).await;
    let db = create(dir.path(), "p.strata");
    let svc = Arc::new(service(db.clone()));
    let allow = Allowlist::parse(&[root.display().to_string(), cdn.clone()]).unwrap();
    let http = HttpFetcher::new(allow.clone(), Duration::from_millis(500)).unwrap();
    svc.set_media(Resolver { allow, max_bytes: 100, store: Some(store.clone()), http: Some(Arc::new(http)) }).unwrap();

    let (a, b) = (png(4, 3), png(8, 8));
    let w = svc
        .write_ops(
            vec![
                insert("a", json!(format!("{cdn}/a.png"))),
                insert("r", json!(format!("{cdn}/to-a"))),
                insert("b", json!(format!("file:{}", root.join("b.png").display()))),
                insert("s", json!({"data": base64::engine::general_purpose::STANDARD.encode(&b)})),
            ],
            None,
        )
        .await
        .unwrap();
    assert_eq!(w.rows, 4);
    // SQL and imports resolve too.
    svc.sql(&Principal::open(), &format!("INSERT INTO photos (id, title, image) VALUES ('q', 'from sql', '{cdn}/a.png')"), &Params::new(), After::Any, 0).await.unwrap();
    let csv = format!("id,title,image\ni,imported,{cdn}/a.png\n");
    assert_eq!(svc.import(None, axum::body::Bytes::from(csv), strata::ingest::Format::Csv).await.unwrap().0, 1);
    let got = rows(&db);
    let ids: Vec<&str> = got.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(ids, ["a", "b", "i", "q", "r", "s"]);
    for (id, m, v) in &got {
        let want = if ["b", "s"].contains(&id.as_str()) { &b } else { &a };
        assert_eq!((m.sha256.as_str(), m.mime.as_str(), m.bytes), (sha256_hex(want).as_str(), "image/png", want.len() as u64), "{id}");
        assert!(v.as_ref().is_some_and(|v| v.len() == 16), "{id} has its vector");
    }
    assert_eq!(got[4].1.uri, format!("{cdn}/to-a"), "a redirect keeps the URI written");
    assert_eq!(got[5].1.uri, format!("media:{}", sha256_hex(&b)));
    assert_eq!(std::fs::read(store.join(sha256_hex(&b))).unwrap(), b);

    // What is refused, and how: nothing of a refused request is written.
    let ok = insert("ok", json!(format!("{cdn}/a.png")));
    let refused = |image: Json, status: u16, says: &'static str| {
        let (svc, ok) = (svc.clone(), ok.clone());
        async move {
            let e = svc.write_ops(vec![ok, insert("x", image.clone())], None).await.unwrap_err();
            assert!(e.status == status && e.message.contains(says), "{image}: {} {}", e.status, e.message);
        }
    };
    refused(json!(format!("{other}/a.png")), 400, "not on the media fetch list").await;
    refused(json!(format!("{cdn}/away")), 400, "which is not on the media fetch list").await;
    refused(json!(format!("{cdn}/big.png")), 400, "media size cap of 100 bytes").await;
    refused(json!(format!("{cdn}/stream.png")), 400, "media size cap of 100 bytes").await;
    refused(json!(format!("{cdn}/missing.png")), 400, "404").await;
    refused(json!("https://cdn.example.com/a.png"), 400, "not on the media fetch list").await;
    refused(json!(format!("file:{}", dir.path().join("p.strata").display())), 400, "not inside a file: root").await;
    let t0 = Instant::now();
    refused(json!(format!("{cdn}/slow.png")), 504, "--media-timeout-s").await;
    assert!(t0.elapsed() < Duration::from_secs(4), "the fetch timed out at 0.5 s: {:?}", t0.elapsed());
    assert_eq!(rows(&db).len(), 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replica_gets_media_and_vectors_without_fetching() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..5 {
        std::fs::write(root.join(format!("{i}.png")), png(i + 1, 2)).unwrap();
    }
    // The replica fetches nothing, has no store, and its model is the
    // primary's: what it holds comes from what it is shipped.
    let rdb = create(dir.path(), "r.strata");
    let mut r = service(rdb.clone());
    r.set_replication(Replication::Replica(Arc::new(Follow::open(rdb.clone(), None).unwrap())));
    let r = Arc::new(r);
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ru = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_grpc(r.clone(), l, Access::default()));

    let pdb = create(dir.path(), "p.strata");
    let mut p = service(pdb.clone());
    let opts = PrimaryOptions { replicas: vec![ru], acks: Acks::All, heartbeat: Duration::from_millis(100), retry_max: Duration::from_millis(200), ..Default::default() };
    p.set_replication(Replication::Primary(Primary::start(pdb.clone(), None, opts).unwrap()));
    let p = Arc::new(p);
    p.set_media(Resolver { allow: Allowlist::parse(&[root.display().to_string()]).unwrap(), ..Resolver::default() }).unwrap();

    let ops = (0..5).map(|i| insert(&format!("k{i}"), json!(format!("file://{}", root.join(format!("{i}.png")).display())))).collect();
    let w = p.write_ops(ops, None).await.unwrap();
    // Answered once the replica holds it (all acks), and the files gone.
    assert!(rdb.storage().seq_no().unwrap().seq >= w.seq_no.unwrap());
    std::fs::remove_dir_all(&root).unwrap();
    let (on_primary, on_replica) = (rows(&pdb), rows(&rdb));
    assert_eq!(on_primary.len(), 5);
    assert_eq!(on_replica, on_primary);
    for (i, (_, m, v)) in on_replica.iter().enumerate() {
        assert_eq!((m.width, m.height), (Some(i as u32 + 1), Some(2)));
        assert!(v.is_some());
    }
    // The replica's own resolver would have refused a fetch.
    let e = rdb.resolve_media_in("photos", &mut [vec![Value::Null, Value::Null, Value::Str(json!({"uri": "file:/x.png"}).to_string()), Value::Null]]).unwrap_err();
    assert!(e.to_string().contains("--media-fetch"), "{e}");
}
