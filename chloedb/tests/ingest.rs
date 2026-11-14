//! The short JSON form of schemas, and rows read from JSON Lines, JSON and
//! CSV files into a database.

use std::sync::Arc;

use chloedb::db::Db;
use chloedb::embed::{Embedder, HashEmbedder};
use chloedb::ingest::{self, Format, Rows};
use chloedb::query::Params;
use chloedb::schema::{FieldType, Schema, VectorCodes, VectorIndexKind};
use chloedb::storage::DbOptions;
use chloedb::value::Value;

const SHORT: &str = r#"{"table": "products", "key": "id", "fields": [
  {"name": "id", "type": "keyword"},
  {"name": "title", "type": "text"},
  {"name": "description", "type": "text", "positions": false},
  {"name": "tags", "type": "keyword", "multi": true},
  {"name": "price", "type": "float"},
  {"name": "stock", "type": "int"},
  {"name": "body", "type": "text", "from": ["title", "description"]},
  {"name": "emb", "type": "vector", "embed": {"from": ["title", "description"], "model": "hash:16"}},
  {"name": "img", "type": "vector", "dim": 8, "index": "flat", "codes": "binary"}
]}"#;

#[test]
fn the_short_schema_form_fills_in_defaults_and_round_trips() {
    let s = Schema::from_json(SHORT).unwrap();
    assert_eq!(s.table, "products");
    assert_eq!(s.primary_key, Some(0));
    let f = |n: &str| s.field(n).unwrap().clone();
    assert_eq!(f("title").ty, FieldType::Text { positions: true });
    assert_eq!(f("description").ty, FieldType::Text { positions: false });
    assert!(f("title").stored && f("title").indexed);
    assert!(f("tags").multi);
    assert!(!f("body").stored && f("body").generated_from == ["title", "description"]);
    // A known model gives the dimensions; vectors default to HNSW, unstored.
    assert_eq!(f("emb").ty, FieldType::Vector { dim: 16, index: VectorIndexKind::Hnsw { m: 16, ef_construction: 200 } });
    assert!(!f("emb").stored);
    assert_eq!(f("img").ty, FieldType::Vector { dim: 8, index: VectorIndexKind::Flat });
    assert_eq!(f("img").codes, VectorCodes::Binary);

    // Printed short and read back, or read in the long form, it is the same schema.
    let short = s.to_short_json();
    assert_eq!(Schema::from_json_value(&short).unwrap(), s);
    assert_eq!(short["fields"][1], serde_json::json!({"name": "title", "type": "text"}));
    let long = serde_json::to_string(&s).unwrap();
    assert_eq!(Schema::from_json(&long).unwrap(), s);
    // The long form may name its key.
    let mut named: serde_json::Value = serde_json::from_str(&long).unwrap();
    named["primary_key"] = serde_json::json!("id");
    assert_eq!(Schema::from_json_value(&named).unwrap(), s);
}

#[test]
fn bad_short_schemas_say_what_is_wrong() {
    let err = |fields: &str| Schema::from_json(&format!(r#"{{"table": "t", "key": "id", "fields": [{{"name": "id", "type": "keyword"}}, {fields}]}}"#)).unwrap_err().to_string();
    assert!(err(r#"{"name": "x", "type": "strng"}"#).contains("unknown type \"strng\""));
    assert!(err(r#"{"name": "x", "type": "vector"}"#).contains("needs \"dim\""));
    assert!(err(r#"{"name": "x", "type": "text", "positons": true}"#).contains("unknown option \"positons\""));
    assert!(err(r#"{"name": "x", "type": "vector", "dim": 4, "index": "lsh"}"#).contains("index is"));
    assert!(err(r#"{"name": "x", "type": "vector", "dim": 4, "embed": {"from": ["nope"], "model": "hash:4"}}"#).contains("source nope"));
    assert!(Schema::from_json(r#"{"table": "t", "key": "nope", "fields": [{"name": "id", "type": "keyword"}]}"#).unwrap_err().to_string().contains("no primary key field nope"));
}

fn db(dir: &std::path::Path) -> Db {
    let db = Db::create(&dir.join("p.strata"), Schema::from_json(SHORT).unwrap(), DbOptions::default()).unwrap();
    db.set_embedder("emb", Arc::new(HashEmbedder { dim: 16 })).unwrap();
    db
}

fn ids(db: &Db, sql: &str) -> Vec<String> {
    db.query(sql, &Params::new()).unwrap().rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

#[test]
fn files_import_bulk_then_upsert() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let jsonl = dir.path().join("a.jsonl");
    std::fs::write(
        &jsonl,
        concat!(
            r#"{"id": "p1", "title": "red mug", "description": "ceramic", "tags": ["kitchen", "gift"], "price": 12.5, "stock": 3, "colour": "red"}"#,
            "\n\n",
            r#"{"id": "p2", "title": "blue kettle", "description": "steel", "tags": "kitchen", "price": 30, "img": [1, 0, 0, 0, 0, 0, 0, 1]}"#,
            "\n"
        ),
    )
    .unwrap();
    let r = ingest::import_file(&db, &jsonl, None).unwrap();
    assert_eq!((r.rows, r.bulk, r.unknown.clone()), (2, true, vec!["colour".to_string()]));
    assert_eq!(ids(&db, "SELECT id FROM products WHERE tags = 'gift'"), ["p1"]);
    assert_eq!(ids(&db, "SELECT id FROM products WHERE MATCH(body, 'steel')"), ["p2"]);
    // The embedded vector was computed from title + description.
    let q = Params::new().set("q", Value::Vector(HashEmbedder { dim: 16 }.embed_query("blue kettle\nsteel").unwrap().into()));
    assert_eq!(db.query("SELECT id FROM products ORDER BY similarity(emb, :q) DESC LIMIT 1", &q).unwrap().rows[0][0], Value::Str("p2".into()));

    // A CSV into a table that has rows: upserts (p2 replaced, p3 new).
    let csv = dir.path().join("b.csv");
    std::fs::write(&csv, "id,title,description,tags,price,stock,img\np2,blue kettle,\"steel, 1.7 l\",kitchen|sale,28.5,,\np3,green teapot,cast iron,,45,7,\"[0,1,0,0,0,0,0,0]\"\n").unwrap();
    let r = ingest::import_file(&db, &csv, None).unwrap();
    assert_eq!((r.rows, r.bulk), (2, false));
    let mut rows = db.query("SELECT id, price, stock, tags FROM products", &Params::new()).unwrap().rows;
    rows.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1][1], Value::Float(28.5));
    assert_eq!(rows[1][2], Value::Null);
    assert_eq!(rows[1][3], Value::List(vec![Value::Str("kitchen".into()), Value::Str("sale".into())]));
    assert_eq!(rows[2][2], Value::Int(7));
    assert_eq!(ids(&db, "SELECT id FROM products WHERE MATCH(body, 'iron')"), ["p3"]);

    // A JSON array, TSV, and an explicit format.
    let json = dir.path().join("c.data");
    std::fs::write(&json, r#"[{"id": "p4", "title": "lamp"}, {"id": "p5", "title": "rug"}]"#).unwrap();
    assert_eq!(ingest::import_file(&db, &json, Some(Format::Json)).unwrap().rows, 2);
    let tsv = dir.path().join("d.tsv");
    std::fs::write(&tsv, "id\ttitle\np6\tdesk\n").unwrap();
    assert_eq!(ingest::import_file(&db, &tsv, None).unwrap().rows, 1);
    assert_eq!(db.query("SELECT COUNT(*) FROM products", &Params::new()).unwrap().rows[0][0], Value::Int(6));
    assert!(ingest::import_file(&db, &json, None).unwrap_err().to_string().contains("name the format"));
}

#[test]
fn a_bad_row_names_its_line_and_a_bulk_load_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let f = dir.path().join("bad.jsonl");
    std::fs::write(&f, "{\"id\": \"a\", \"price\": 1}\n{\"id\": \"b\", \"price\": \"cheap\"}\n").unwrap();
    let e = ingest::import_file(&db, &f, None).unwrap_err().to_string();
    assert!(e.contains("line 2") && e.contains("price"), "{e}");
    assert_eq!(db.query("SELECT COUNT(*) FROM products", &Params::new()).unwrap().rows[0][0], Value::Int(0));
    let f = dir.path().join("bad.csv");
    std::fs::write(&f, "id,stock\na,1\nb,many\n").unwrap();
    let e = ingest::import_file(&db, &f, None).unwrap_err().to_string();
    assert!(e.contains("line 3") && e.contains("stock"), "{e}");
    // Reading alone, without a database.
    let schema = db.schema().clone();
    let rows: Vec<_> = Rows::new(&schema, std::io::Cursor::new(b"id,title\nx,hello\n".to_vec()), Format::Csv).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(rows[0][1], Value::Str("hello".into()));
}

/// The CSV rows of `text` for `schema`, or the first error.
fn csv_rows(schema: &Schema, text: &str, empty: ingest::EmptyCell) -> Result<Vec<Vec<Value>>, String> {
    Rows::new(schema, std::io::Cursor::new(text.as_bytes().to_vec()), Format::Csv).unwrap().empty_cells(empty).collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

#[test]
fn a_vector_past_f32s_range_says_so() {
    let schema = Schema::from_json(r#"{"table": "v", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "e", "type": "vector", "dim": 3, "index": "flat"}]}"#).unwrap();
    let doc = vec![Value::Str("a".into()), Value::Vector(vec![1e39_f64 as f32, 0.0, 0.0].into())];
    let e = chloedb::value::validate(&schema, &doc).unwrap_err().to_string();
    assert!(e.contains("element 1 of the vector is not a finite 32-bit float"), "{e}");
}

#[test]
fn csv_rows_that_do_not_fit_their_header() {
    let schema = Schema::from_json(r#"{"table": "t1", "key": "id", "fields": [{"name": "id", "type": "int"}, {"name": "n", "type": "int"}, {"name": "name", "type": "keyword"}]}"#).unwrap();
    let rows = |text: &str, strict: bool| -> Result<(usize, u64), String> {
        let mut r = Rows::new(&schema, std::io::Cursor::new(text.as_bytes().to_vec()), Format::Csv).map_err(|e| e.to_string())?.strict(strict);
        let n = r.by_ref().collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?.len();
        Ok((n, r.ragged))
    };
    // A row of more or fewer values than the header: counted, or refused
    // by its line when strict.
    assert_eq!(rows("id,n\n400,2,3\n401,5\n", false).unwrap(), (2, 1));
    assert_eq!(rows("id,n,name\n400,2,a\n401,5\n", false).unwrap(), (2, 1));
    let e = rows("id,n\n401,5\n400,2,3\n", true).unwrap_err();
    assert!(e.contains("line 3") && e.contains("3 values, and the header names 2 columns"), "{e}");
    assert!(rows("id,n,name\n401,5\n", true).unwrap_err().contains("line 2: 2 values"));
    assert_eq!(rows("id,n\n1,2\n", true).unwrap(), (1, 0));
    // A column twice, or a header that names no field.
    for strict in [false, true] {
        let e = rows("id,id\n403,404\n", strict).unwrap_err();
        assert!(e.contains("column 2 names field id again, as column 1 does"), "{e}");
        let e = rows("id;n\n301;2\n", strict).unwrap_err();
        assert!(e.contains("names no field of t1 (is the file comma-separated"), "{e}");
    }
}

#[test]
fn csv_lines_empty_cells_and_whole_floats() {
    let schema = Schema::from_json(
        r#"{"table": "t", "key": "id", "fields": [
            {"name": "id", "type": "int"},
            {"name": "n", "type": "int", "not_null": true, "default": 0},
            {"name": "name", "type": "keyword"},
            {"name": "x", "type": "float"}]}"#,
    )
    .unwrap();
    use ingest::EmptyCell;
    // A bad value names its line, with LF and with CRLF line endings, and
    // after a quoted cell that holds a newline.
    for text in ["id,n\n100,1\n101,2\n102,3\n103,bad\n", "id,n\r\n100,1\r\n101,2\r\n102,3\r\n103,bad\r\n", "id,name\r\n1,\"two\r\nlines\"\r\n2,b\r\nx,d\r\n"] {
        let e = csv_rows(&schema, text, EmptyCell::Default).unwrap_err();
        assert!(e.contains("line 5"), "{text:?}: {e}");
    }
    // An empty cell of a column with a DEFAULT takes it; of another, NULL.
    let rows = csv_rows(&schema, "id,n,name,x\n1,,,\n2,5,a,1.5\n", EmptyCell::Default).unwrap();
    assert_eq!(rows[0], vec![Value::Int(1), Value::Int(0), Value::Null, Value::Null]);
    assert_eq!(rows[1], vec![Value::Int(2), Value::Int(5), Value::Str("a".into()), Value::Float(1.5)]);
    // As NULL, which NOT NULL refuses; as the empty string in text.
    assert!(csv_rows(&schema, "id,n\n1,\n", EmptyCell::Null).unwrap_err().contains("NOT NULL"));
    assert_eq!(csv_rows(&schema, "id,n,name,x\n1,,\"\",\n", EmptyCell::Text).unwrap()[0], vec![Value::Int(1), Value::Int(0), Value::Str(String::new()), Value::Null]);
    assert_eq!(EmptyCell::parse("NULL"), Some(EmptyCell::Null));
    // 2.0 is a whole number for an INT, from CSV, JSON and SQL; 2.5 is not.
    assert_eq!(csv_rows(&schema, "id,n\n1,2.0\n", EmptyCell::Default).unwrap()[0][1], Value::Int(2));
    assert!(csv_rows(&schema, "id,n\n1,2.5\n", EmptyCell::Default).unwrap_err().contains("not a whole number"));
    assert_eq!(ingest::value(&schema, 1, &serde_json::json!(3.0)).unwrap(), Value::Int(3));
    assert!(ingest::value(&schema, 1, &serde_json::json!(3.5)).unwrap_err().to_string().contains("not a whole number"));
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("t.strata"), schema.clone(), DbOptions::default()).unwrap();
    db.sql("INSERT INTO t (id, n) VALUES (1, 4.0)", &Params::new()).unwrap();
    assert_eq!(db.query("SELECT n FROM t WHERE n = 4.0", &Params::new()).unwrap().rows, vec![vec![Value::Int(4)]]);
    assert!(db.sql("INSERT INTO t (id, n) VALUES (2, 4.5)", &Params::new()).unwrap_err().to_string().contains("whole number"));
}
