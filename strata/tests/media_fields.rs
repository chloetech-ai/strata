//! MEDIA fields: a file's reference, which the write that gives it hashes
//! and describes before the writer (from its bytes, a file under an
//! allowed root, or the media store), stored as JSON and indexed by its
//! SHA-256; through JSON rows, SQL INSERT and UPDATE, merges and reopens;
//! the writes that cannot be resolved, refused whole; and the manifest
//! feature `media`, recorded while a table has such a field.

use std::path::Path;

use base64::Engine;
use serde_json::json;
use strata::db::{Db, SqlResult};
use strata::ingest;
use strata::media::{sha256_hex, Allowlist, Media, MediaKind, Resolver};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::value::Value;

fn photos() -> Schema {
    Schema::new("photos", vec![Field::new("id", FieldType::Keyword), Field::new("title", FieldType::Text { positions: true }), Field::media("image", MediaKind::Image)], Some("id")).unwrap()
}

fn opts() -> DbOptions {
    DbOptions { layout: Layout::Directory, sync: false, ..DbOptions::default() }
}

/// A PNG header of a `w` by `h` image.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
    b
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn run(db: &Db, sql: &str) -> strata::Result<SqlResult> {
    db.sql(sql, &Params::new())
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => panic!("{sql}: expected rows"),
    }
}

/// Each row's id and its image's value.
fn images(db: &Db) -> Vec<(String, Media)> {
    rows(db, "SELECT id, image FROM photos LIMIT 100").into_iter().map(|r| (r[0].as_str().unwrap().to_string(), Media::parse(r[1].as_str().unwrap()).unwrap())).collect()
}

fn ids(db: &Db, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = rows(db, sql).into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

/// JSON rows written as servers and the Python package write them:
/// resolved and embedded (`fill_in`), then inserted in one transaction.
fn write_json(db: &Db, rows: &[serde_json::Value]) -> strata::Result<()> {
    let schema = db.storage().table("photos").expect("the table");
    let mut docs = rows.iter().map(|j| ingest::document(&schema, j)).collect::<strata::Result<Vec<_>>>()?;
    db.fill_in("photos", &mut docs)?;
    let mut t = db.storage().begin()?;
    let mut tt = t.table("photos")?;
    for d in docs {
        tt.insert(d)?;
    }
    t.commit()?;
    Ok(())
}

/// The features the manifest of the database at `path` (directory
/// layout) requires.
fn required_features(path: &Path) -> serde_json::Value {
    let cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join(cur["manifest"].as_str().unwrap())).unwrap()).unwrap();
    manifest["features"]["required"].clone()
}

#[test]
fn files_are_given_by_bytes_by_path_or_by_value() {
    let dir = tempfile::tempdir().unwrap();
    let (root, store) = (dir.path().join("media"), dir.path().join("store"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    let (a, b) = (png(4, 3), png(8, 8));
    std::fs::write(root.join("a.png"), &a).unwrap();
    let (ha, hb) = (sha256_hex(&a), sha256_hex(&b));
    let path = dir.path().join("db");
    let db = Db::create(&path, photos(), opts()).unwrap();
    db.set_media_resolver(Resolver { allow: Allowlist::parse(&[root.display().to_string()]).unwrap(), store: Some(store.clone()), ..Resolver::default() });
    let file = format!("file://{}", root.join("a.png").display());

    // A file under an allowed root, bytes alone (kept in the store), bytes
    // with a URI (fetched from nowhere), and a value complete as given.
    let given = json!({"uri": "s3://bucket/given.jpg", "sha256": sha256_hex(b"given"), "mime": "image/jpeg", "bytes": 5, "width": 2, "height": 1});
    write_json(
        &db,
        &[
            json!({"id": "a", "title": "a cat", "image": file}),
            json!({"id": "b", "image": {"data": b64(&b)}}),
            json!({"id": "c", "image": {"uri": "https://cdn.example.com/a.png", "data": b64(&a)}}),
            json!({"id": "g", "image": given}),
        ],
    )
    .unwrap();
    db.storage().flush().unwrap();
    // SQL: a URI in an INSERT, bytes in an UPDATE.
    run(&db, &format!("INSERT INTO photos (id, image) VALUES ('d', '{file}')")).unwrap();
    let set = json!({"uri": "https://cdn.example.com/b.png", "data": b64(&b)}).to_string();
    run(&db, &format!("UPDATE photos SET image = '{set}' WHERE id = 'c'")).unwrap();

    let check = |db: &Db| {
        let got = images(db);
        let by = |id: &str| got.iter().find(|(i, _)| i == id).map(|(_, m)| m.clone()).unwrap();
        assert_eq!(by("a"), Media { uri: file.clone(), sha256: ha.clone(), mime: "image/png".into(), bytes: a.len() as u64, width: Some(4), height: Some(3), ..Media::default() });
        assert_eq!((by("b").uri, by("b").sha256, by("b").width), (format!("media:{hb}"), hb.clone(), Some(8)));
        assert_eq!((by("c").uri.as_str(), by("c").sha256.as_str()), ("https://cdn.example.com/b.png", hb.as_str()));
        assert_eq!(by("d").sha256, ha);
        assert_eq!(by("g"), Media::parse(&given.to_string()).unwrap());
        // One lookup finds every row of a file; facets count them.
        assert_eq!(ids(db, &format!("SELECT id FROM photos WHERE image = '{ha}'")), ["a", "d"]);
        assert_eq!(ids(db, &format!("SELECT id FROM photos WHERE image IN ('{hb}', '{}')", sha256_hex(b"given"))), ["b", "c", "g"]);
        let mut groups = rows(db, "SELECT image, COUNT(*) FROM photos GROUP BY image");
        groups.sort_by_key(|r| format!("{r:?}"));
        let mut want = vec![vec![Value::Str(ha.clone()), Value::Int(2)], vec![Value::Str(hb.clone()), Value::Int(2)], vec![Value::Str(sha256_hex(b"given")), Value::Int(1)]];
        want.sort_by_key(|r| format!("{r:?}"));
        assert_eq!(groups, want);
        // Text search and the media field together.
        assert_eq!(ids(db, &format!("SELECT id FROM photos WHERE image = '{ha}' ORDER BY bm25(title, 'cat') DESC LIMIT 5")), ["a"]);
    };
    check(&db);
    // The bytes given alone are in the store, and nowhere in the database.
    assert_eq!(std::fs::read(store.join(&hb)).unwrap(), b);
    let in_db: Vec<u8> = walk(&path).into_iter().flat_map(|p| std::fs::read(p).unwrap()).collect();
    assert!(!in_db.windows(b.len()).any(|w| w == b), "the bytes of b are in the database");
    assert_eq!(required_features(&path), json!(["field_ids", "media"]));

    // Merged, and opened again: the same values and lookups.
    db.storage().flush().unwrap();
    let merged = db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
    assert!(db.snapshot().segments().len() < 2, "{merged:?}");
    check(&db);
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    check(&db);
    // A new handle fetches nothing until it is told what it may fetch.
    let e = run(&db, &format!("INSERT INTO photos (id, image) VALUES ('f', '{file}')")).unwrap_err().to_string();
    assert!(e.contains("field image") && e.contains("--media-fetch"), "{e}");
}

fn walk(p: &Path) -> Vec<std::path::PathBuf> {
    if p.is_dir() {
        std::fs::read_dir(p).unwrap().flat_map(|e| walk(&e.unwrap().path())).collect()
    } else {
        vec![p.to_path_buf()]
    }
}

#[test]
fn writes_that_cannot_be_resolved_are_refused_whole() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.png"), png(4, 3)).unwrap();
    std::fs::write(root.join("big.png"), png(4, 3).repeat(10)).unwrap();
    let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
    wav.resize(64, 0);
    std::fs::write(root.join("a.wav"), &wav).unwrap();
    let outside = dir.path().join("outside.png");
    std::fs::write(&outside, png(1, 1)).unwrap();
    let db = Db::create(&dir.path().join("db"), photos(), opts()).unwrap();
    let file = |n: &str| format!("file:{}", root.join(n).display());
    let refused = |db: &Db, rows: &[serde_json::Value], says: &str| {
        let e = write_json(db, rows).unwrap_err().to_string();
        assert!(e.contains(says), "{rows:?}: {e}");
    };
    // Nothing is fetched or kept by default.
    refused(&db, &[json!({"id": "a", "image": file("a.png")})], "--media-fetch");
    refused(&db, &[json!({"id": "a", "image": {"data": b64(&png(1, 1))}})], "--media-store");
    refused(&db, &[json!({"id": "a", "image": "https://cdn.example.com/a.png"})], "not on the media fetch list");
    db.set_media_resolver(Resolver { allow: Allowlist::parse(&[root.display().to_string(), "cdn.example.com".to_string()]).unwrap(), max_bytes: 100, ..Resolver::default() });
    // A good row and a bad one: neither is written.
    refused(&db, &[json!({"id": "ok", "image": file("a.png")}), json!({"id": "a", "image": file("a.wav")})], "kind image does not hold audio/wav");
    refused(&db, &[json!({"id": "a", "image": file("big.png")})], "media size cap of 100 bytes");
    refused(&db, &[json!({"id": "a", "image": {"uri": "x", "data": b64(&png(4, 3).repeat(10))}})], "media size cap of 100 bytes");
    refused(&db, &[json!({"id": "a", "image": format!("file:{}", root.join("../outside.png").display())})], "not inside a file: root");
    refused(&db, &[json!({"id": "a", "image": "https://cdn.example.com/a.png"})], "fetches no URLs");
    refused(&db, &[json!({"id": "a", "image": {"uri": file("a.png"), "sha256": sha256_hex(b"other")}})], "hash to sha256");
    refused(&db, &[json!({"id": "a", "image": {"uri": file("a.png"), "color": "red"}})], "not a key of a MEDIA value");
    assert!(images(&db).is_empty());
    // A value the writer gets unresolved is refused there.
    let schema = db.schema().clone();
    let doc = ingest::document(&schema, &json!({"id": "a", "image": file("a.png")})).unwrap();
    let mut t = db.storage().begin().unwrap();
    let e = t.table("photos").unwrap().insert(doc).unwrap_err().to_string();
    assert!(e.contains("resolved before the writer"), "{e}");
    drop(t);
    write_json(&db, &[json!({"id": "ok", "image": file("a.png")})]).unwrap();
    assert_eq!(images(&db).len(), 1);
}

#[test]
fn the_feature_is_recorded_while_a_table_has_a_media_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let papers = Schema::new("papers", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&path, papers, opts()).unwrap();
    assert_eq!(required_features(&path), json!(["field_ids"]));
    run(&db, "CREATE TABLE scans (id KEYWORD PRIMARY KEY, page MEDIA OPTIONS(kind = 'pdf'))").unwrap();
    assert_eq!(required_features(&path), json!(["field_ids", "media"]));
    let pdf = b"%PDF-1.4\n2 0 obj << /Type /Pages /Count 3 >>";
    let row = json!({"uri": "s3://b/a.pdf", "data": b64(pdf)}).to_string();
    run(&db, &format!("INSERT INTO scans (id, page) VALUES ('s', '{row}')")).unwrap();
    let r = rows(&db, "SELECT page FROM scans");
    assert_eq!(Media::parse(r[0][0].as_str().unwrap()).unwrap().pages, Some(3));
    run(&db, "DROP TABLE scans").unwrap();
    assert_eq!(required_features(&path), json!(["field_ids"]));
}
