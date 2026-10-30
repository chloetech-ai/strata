//! A TEXT field's analyzer, recorded in its schema: `simple` keeps words
//! whole where `english` drops stopwords and stems, for documents and
//! every kind of search alike (MATCH words, phrases, prefixes, typos and
//! synonyms; bm25 and its query stopwords), across flushes, merges and
//! reopening; and the file format it needs, in both layouts.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use strata::db::{Db, SqlResult};
use strata::query::exec::ExecOptions;
use strata::query::Params;
use strata::schema::{Analyzer, Field, FieldType, Schema};
use strata::storage::pager::{Meta, PAGE};
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::text::Synonyms;
use strata::value::Value;

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The ids `sql` returns, sorted.
fn ids(db: &Db, sql: &str) -> Vec<String> {
    let SqlResult::Rows(r) = run(db, sql) else { panic!("{sql}: expected rows") };
    let mut v: Vec<String> = r.rows.into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

fn format_on_disk(path: &Path) -> u32 {
    if path.is_dir() {
        let cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
        return cur.get("format").map_or(1, |f| f.as_u64().unwrap() as u32);
    }
    let f = File::open(path).unwrap();
    let slot = |n: u64| {
        let mut page = vec![0u8; PAGE as usize];
        f.read_exact_at(&mut page, n * PAGE).unwrap();
        Meta::decode(&page).unwrap()
    };
    [slot(0), slot(1)].into_iter().flatten().max_by_key(|m| m.txn).unwrap().format
}

/// A directory database's manifest, and a function that writes it back
/// (with its checksum in CURRENT).
fn manifest(path: &Path) -> (serde_json::Value, impl FnMut(&serde_json::Value) + '_) {
    let mut cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
    let name = cur["manifest"].as_str().unwrap().to_string();
    let m = serde_json::from_slice(&std::fs::read(path.join(&name)).unwrap()).unwrap();
    let rewrite = move |m: &serde_json::Value| {
        let bytes = serde_json::to_vec(m).unwrap();
        std::fs::write(path.join(&name), &bytes).unwrap();
        cur["crc"] = crc32fast::hash(&bytes).into();
        std::fs::write(path.join("CURRENT"), serde_json::to_vec(&cur).unwrap()).unwrap();
    };
    (m, rewrite)
}

const NOTES: &str = "CREATE TABLE notes (id KEYWORD PRIMARY KEY, title TEXT OPTIONS(analyzer = 'simple'), body TEXT)";

/// Rows whose title and body say the same thing.
fn add_notes(db: &Db, rows: &[(&str, &str)]) {
    for (id, text) in rows {
        run(db, &format!("INSERT INTO notes (id, title, body) VALUES ('{id}', '{text}', '{text}')"));
    }
}

fn searches(db: &Db) {
    let m = |field: &str, q: &str| ids(db, &format!("SELECT id FROM notes WHERE MATCH({field}, '{q}')"));
    // english stems and drops stopwords; simple matches words as written.
    assert_eq!(m("body", "run"), ["a", "c"]);
    assert_eq!(m("title", "run"), ["c"]);
    assert_eq!(m("title", "running"), ["a"]);
    assert_eq!(m("body", "the"), Vec::<String>::new());
    assert_eq!(m("title", "the"), ["a", "b"]);
    assert_eq!(m("title", "\"the running\""), ["a"]);
    assert_eq!(m("body", "\"the running\""), ["a", "c"]);
    assert_eq!(m("title", "Über"), ["b"]);
    assert_eq!(m("title", "runn*"), ["a"]);
    assert_eq!(m("title", "runing~"), ["a"]);
    assert_eq!(m("title", "cat"), ["b"]);
    assert_eq!(m("title", "cats"), ["a"]);
    assert_eq!(m("body", "cats"), ["a", "b"]);

    // bm25: a simple field keeps the query's stopwords unless told not to.
    let ranked = |field: &str, q: &str, opts: &str| -> Vec<(String, f64)> {
        let SqlResult::Rows(r) = run(db, &format!("SELECT id, score() FROM notes ORDER BY bm25({field}, '{q}'{opts}) DESC LIMIT 10")) else { panic!() };
        r.rows.into_iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).filter(|r| r.1 > 0.0).collect()
    };
    let top = |field: &str, q: &str| ranked(field, q, "")[0].0.clone();
    assert_eq!(top("title", "the cat"), "b");
    assert_eq!(top("title", "the cats"), "a");
    let found = |field: &str, q: &str, opts: &str| -> Vec<String> {
        let mut v: Vec<String> = ranked(field, q, opts).into_iter().map(|r| r.0).collect();
        v.sort();
        v
    };
    assert_eq!(found("title", "the", ""), ["a", "b"]);
    assert_eq!(found("title", "the", ", stopwords => 1"), Vec::<String>::new());
    assert_eq!(found("body", "the", ", stopwords => 0"), Vec::<String>::new());
    assert_eq!(found("body", "running", ""), ["a", "c"]);
    assert_eq!(found("title", "running", ""), ["a"]);
}

fn through_sql(path: &Path, layout: Layout) {
    let opts = || DbOptions { sync: false, layout, ..DbOptions::default() };
    let plain = Schema::new("papers", vec![Field::new("id", FieldType::Keyword), Field::new("title", FieldType::Text { positions: true })], Some("id")).unwrap();
    let db = Db::create(path, plain, opts()).unwrap();
    // english by name is english: it needs no feature.
    run(&db, "CREATE TABLE plain (id KEYWORD PRIMARY KEY, body TEXT OPTIONS(analyzer = 'english'))");
    assert_eq!(format_on_disk(path), 2);
    assert_eq!(db.storage().table("plain").unwrap().fields[1].analyzer, None);
    for bad in [
        "CREATE TABLE t (id KEYWORD PRIMARY KEY, body TEXT OPTIONS(analyzer = 'french'))",
        "CREATE TABLE t (id KEYWORD PRIMARY KEY, tag KEYWORD OPTIONS(analyzer = 'simple'))",
        "CREATE TABLE t (id KEYWORD PRIMARY KEY, body TEXT OPTIONS(analyzer = 1))",
    ] {
        assert!(db.sql(bad, &Params::new()).is_err(), "{bad}");
    }

    run(&db, NOTES);
    assert_eq!(format_on_disk(path), 3);
    if layout == Layout::Directory {
        assert_eq!(manifest(path).0["features"]["required"], serde_json::json!(["analyzer:simple:1"]));
    }
    let notes = db.storage().table("notes").unwrap();
    assert_eq!(notes.fields[1].analyzer, Some(Analyzer::Simple));
    assert_eq!(notes.to_short_json()["fields"][1], serde_json::json!({"name": "title", "type": "text", "analyzer": "simple"}));

    // In the memtable, then in segments, then merged into one.
    add_notes(&db, &[("a", "The Running Cats"), ("b", "the cat sat über alles")]);
    add_notes(&db, &[("c", "they run")]);
    searches(&db);
    db.storage().flush().unwrap();
    searches(&db);
    add_notes(&db, &[("d", "nothing here")]);
    db.storage().flush().unwrap();
    let stats = db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
    assert!(stats.docs_merged > 0, "{stats:?}");
    searches(&db);
    drop(db);

    assert_eq!(format_on_disk(path), 3);
    let db = Db::open(path, opts()).unwrap();
    searches(&db);
    run(&db, "DROP TABLE notes");
    assert_eq!(format_on_disk(path), 2);
    run(&db, "DROP TABLE plain");
    assert_eq!(format_on_disk(path), 1);
}

// A version of Strata without an analyzer refuses the database by the
// analyzer's feature, before it reads a field.
#[test]
fn a_missing_analyzer_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d");
    let opts = || DbOptions { layout: Layout::Directory, sync: false, ..DbOptions::default() };
    let schema = Schema::new("notes", vec![Field::new("id", FieldType::Keyword), Field::new("title", FieldType::Text { positions: true }).analyzer(Analyzer::Simple)], Some("id")).unwrap();
    Db::create(&path, schema, opts()).unwrap();
    let (mut m, mut rewrite) = manifest(&path);
    assert_eq!(m["features"]["required"], serde_json::json!(["analyzer:simple:1"]));
    assert_eq!(m["tables"][0]["schema"]["fields"][1]["analyzer"], serde_json::json!({"name": "simple", "version": 1}));
    // As a later version would write a field analysed by simple version 2.
    m["features"]["required"] = serde_json::json!(["analyzer:simple:2"]);
    m["tables"][0]["schema"]["fields"][1]["analyzer"]["version"] = 2.into();
    rewrite(&m);
    let e = Db::open(&path, opts()).err().expect("a missing analyzer is refused").to_string();
    assert!(e.contains("\"analyzer:simple:2\"") && e.contains("newer"), "{e}");
}

#[test]
fn sql_single_file() {
    let dir = tempfile::tempdir().unwrap();
    through_sql(&dir.path().join("a.strata"), Layout::SingleFile);
}

#[test]
fn sql_directory() {
    let dir = tempfile::tempdir().unwrap();
    through_sql(&dir.path().join("a"), Layout::Directory);
}

// The first table's schema in JSON, and synonyms, which each field's
// analyzer analyses its own way.
#[test]
fn json_schema_and_synonyms() {
    let schema = Schema::from_json(
        r#"{"table": "notes", "key": "id", "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "title", "type": "text", "analyzer": {"name": "simple", "version": 1}},
            {"name": "body", "type": "text", "analyzer": "english"}
        ]}"#,
    )
    .unwrap();
    assert_eq!((schema.fields[1].analyzer, schema.fields[2].analyzer), (Some(Analyzer::Simple), None));
    for bad in [r#""analyzer": "simple2""#, r#""analyzer": {"name": "simple", "version": 2}"#, r#""analyzer": true"#] {
        let j = format!(r#"{{"table": "t", "key": "id", "fields": [{{"name": "id", "type": "keyword"}}, {{"name": "t", "type": "text", {bad}}}]}}"#);
        let e = Schema::from_json(&j).unwrap_err().to_string();
        assert!(e.contains("field t") && e.contains("analyzer"), "{bad}: {e}");
    }

    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::create(&dir.path().join("n.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    add_notes(&db, &[("a", "laptops for sale"), ("b", "a notebook for sale")]);
    let syn = Arc::new(Synonyms::parse("laptop, notebook").unwrap());
    db.exec = ExecOptions { synonyms: Some(syn), ..ExecOptions::default() };
    let m = |field: &str, q: &str| {
        let r = db.query(&format!("SELECT id FROM notes WHERE MATCH({field}, '{q}', synonyms => 1)"), &Params::new()).unwrap();
        let mut v: Vec<Value> = r.rows.into_iter().map(|r| r[0].clone()).collect();
        v.sort_by_key(|x| x.as_str().unwrap().to_string());
        v
    };
    let (a, b) = (Value::Str("a".into()), Value::Str("b".into()));
    // english: "laptops" is "laptop", which finds "notebook".
    assert_eq!(m("body", "laptops"), [a.clone(), b.clone()]);
    // simple: "laptops" is not "laptop", so the rule does not apply.
    assert_eq!(m("title", "laptops"), [a.clone()]);
    assert_eq!(m("title", "notebook"), [b.clone()]);
    run(&db, "INSERT INTO notes (id, title, body) VALUES ('c', 'laptop', 'laptop')");
    assert_eq!(m("title", "notebook"), [b, Value::Str("c".into())]);
}
