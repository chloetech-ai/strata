//! Vectors of images through a server (feature `vision`): write
//! operations whose images get their vectors before the writer, queries by
//! an image file (`{"embed": {"image": "<base64>"}}`) and `/v1/embed` with
//! an image, and what is refused. The `hash:16` model embeds an image by
//! its colours.
#![cfg(feature = "vision")]

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::media::MediaKind;
use strata::schema::{Field, FieldType, Schema, VectorIndexKind};
use strata::storage::DbOptions;
use strata_server::auth::Access;
use strata_server::server;
use strata_server::service::{AgentConfig, Backend, Service};
use tokio::net::TcpListener;

fn photos() -> Schema {
    let emb = Field::new("emb", FieldType::Vector { dim: 16, index: VectorIndexKind::Flat }).embedded("hash:16", &["image"]);
    let temb = Field::new("temb", FieldType::Vector { dim: 16, index: VectorIndexKind::Flat }).embedded("hash:16", &["title"]);
    Schema::new("photos", vec![Field::new("id", FieldType::Keyword), Field::new("title", FieldType::Text { positions: true }), Field::media("image", MediaKind::Image), emb, temb], Some("id")).unwrap()
}

/// A PNG of an 8 by 8 image of one colour, in base64.
fn png(rgb: [u8; 3]) -> String {
    let mut out = Cursor::new(Vec::new());
    image::RgbImage::from_pixel(8, 8, image::Rgb(rgb)).write_to(&mut out, image::ImageFormat::Png).unwrap();
    base64::engine::general_purpose::STANDARD.encode(out.into_inner())
}

async fn post(c: &reqwest::Client, url: &str, body: Json) -> (u16, Json) {
    let r = c.post(url).json(&body).send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn queries_by_image_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("p.strata"), photos(), DbOptions { publish_snapshots: true, sync: false, ..DbOptions::default() }).unwrap();
    let agent = AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), photos(), agent, 16, Duration::from_secs(10));
    svc.open_models(std::path::Path::new("/nonexistent"));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(server::serve_http(Arc::new(svc), l, Access::default()));
    let c = reqwest::Client::new();

    let colours = [("red", [250, 10, 10]), ("green", [10, 250, 10]), ("blue", [10, 10, 250])];
    let ops: Vec<Json> = colours.iter().map(|(id, rgb)| json!({"op": "insert", "doc": {"id": id, "title": format!("a {id} square"), "image": {"uri": format!("{id}.png"), "data": png(*rgb)}}})).collect();
    let (status, w) = post(&c, &format!("{base}/v1/write"), json!({"ops": ops})).await;
    assert_eq!((status, w["written"].as_u64()), (200, Some(3)), "{w}");

    let nearest = |q: Json| json!({"sql": "SELECT id FROM photos ORDER BY similarity(emb, :q) DESC LIMIT 1", "params": {"q": q}});
    for (id, rgb) in colours {
        let (status, r) = post(&c, &format!("{base}/v1/query"), nearest(json!({"embed": {"image": png(rgb)}, "field": "emb"}))).await;
        assert_eq!((status, &r["rows"][0][0]), (200, &json!(id)), "{r}");
    }
    // /v1/embed gives the vector the parameter stands for.
    let (status, e) = post(&c, &format!("{base}/v1/embed"), json!({"image": png([10, 250, 10]), "field": "emb"})).await;
    assert_eq!((status, &e["field"]), (200, &json!("emb")), "{e}");
    let (_, r) = post(&c, &format!("{base}/v1/query"), nearest(e["vector"].clone())).await;
    assert_eq!(r["rows"][0][0], json!("green"), "{r}");
    // A text query by the same model still embeds text.
    let (status, r) = post(&c, &format!("{base}/v1/query"), json!({"sql": "SELECT id FROM photos ORDER BY similarity(temb, :q) DESC LIMIT 1", "params": {"q": {"embed": "blue square", "field": "temb"}}})).await;
    assert_eq!((status, &r["rows"][0][0]), (200, &json!("blue")), "{r}");

    for (bad, says) in [
        (json!({"embed": {"image": "!!!"}, "field": "emb"}), "base64"),
        (json!({"embed": {"image": base64::engine::general_purpose::STANDARD.encode(b"not an image")}, "field": "emb"}), "could not be"),
        (json!({"embed": {"image": png([1, 2, 3]), "x": 1}, "field": "emb"}), "\"embed\" is a text"),
        (json!({"embed": {"image": png([1, 2, 3])}, "field": "title"}), "not an embedded VECTOR field"),
    ] {
        let (status, r) = post(&c, &format!("{base}/v1/query"), nearest(bad.clone())).await;
        assert!(status == 400 && r["error"].as_str().is_some_and(|m| m.contains(says)), "{bad}: {status} {r}");
    }
    let (status, r) = post(&c, &format!("{base}/v1/embed"), json!({"image": 3})).await;
    assert_eq!(status, 400, "{r}");
}
