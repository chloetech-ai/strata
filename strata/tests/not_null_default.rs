//! NOT NULL and DEFAULT, features of format 3: declared in SQL and in the
//! schema's JSON form; defaults taken by SQL INSERTs, `DEFAULT` in values
//! and assignments, and JSON and CSV rows that leave a field out; NOT NULL
//! refused on every write path; and the file format they need, in both
//! layouts.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use strata::db::{Db, SqlResult};
use strata::ingest::{self, Format, Rows};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::pager::{Meta, PAGE};
use strata::storage::{DbOptions, Layout};
use strata::value::Value;

/// 2026-12-31T00:00:00Z in microseconds.
const DEC31: i64 = 1_798_675_200_000_000;

fn papers() -> Schema {
    Schema::new("papers", vec![Field::new("id", FieldType::Keyword), Field::new("title", FieldType::Text { positions: true })], Some("id")).unwrap()
}

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn written(db: &Db, sql: &str) -> u64 {
    match run(db, sql) {
        SqlResult::Written(n) => n,
        SqlResult::Rows(_) => panic!("{sql}: expected a write"),
    }
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => panic!("{sql}: expected rows"),
    }
}

/// The one row of `sql`.
fn row(db: &Db, sql: &str) -> Vec<Value> {
    let mut r = rows(db, sql);
    assert_eq!(r.len(), 1, "{sql}: {r:?}");
    r.remove(0)
}

fn fails(db: &Db, sql: &str) -> String {
    match db.sql(sql, &Params::new()) {
        Ok(_) => panic!("{sql}: expected an error"),
        Err(e) => e.to_string(),
    }
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn now() -> i64 {
    strata::timestamp::now()
}

fn format_on_disk(path: &Path) -> u32 {
    if path.is_dir() {
        let cur: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("CURRENT")).unwrap()).unwrap();
        return cur.get("format").map_or(1, |f| f.as_u64().unwrap() as u32);
    }
    meta_slots(path).into_iter().flatten().max_by_key(|m| m.txn).unwrap().format
}

fn meta_slots(path: &Path) -> Vec<Option<Meta>> {
    let f = File::open(path).unwrap();
    (0..2u64)
        .map(|slot| {
            let mut page = vec![0u8; PAGE as usize];
            f.read_exact_at(&mut page, slot * PAGE).unwrap();
            Meta::decode(&page).unwrap()
        })
        .collect()
}

const TASKS: &str = "CREATE TABLE tasks (
    id KEYWORD PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    state KEYWORD NOT NULL DEFAULT 'open',
    priority INT DEFAULT 2,
    weight FLOAT DEFAULT -0.5,
    done BOOL NOT NULL DEFAULT FALSE,
    created TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    due TIMESTAMP DEFAULT '2026-12-31',
    note TEXT NULL
)";

#[test]
fn sql_single_file() {
    through_sql(Layout::SingleFile);
}

#[test]
fn sql_directory() {
    through_sql(Layout::Directory);
}

fn through_sql(layout: Layout) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.strata");
    let opts = || DbOptions { layout, sync: false, ..DbOptions::default() };
    let db = Db::create(&path, papers(), opts()).unwrap();

    // Defaults that do not fit their column, and the clauses that are still
    // refused.
    for (sql, says) in [
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, n INT DEFAULT 'many')", "n"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, n INT DEFAULT CURRENT_TIMESTAMP)", "n"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, b BOOL DEFAULT 2)", "b"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, t TIMESTAMP DEFAULT 'soon')", "t"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, n INT DEFAULT (1 + 2))", "not a constant"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY DEFAULT 'k')", "key"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, tags KEYWORD[] DEFAULT 'a')", "tags"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, v VECTOR(4) DEFAULT 1)", "v"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, n INT NULL NOT NULL)", "NULL and NOT NULL"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, n INT DEFAULT 1 DEFAULT 2)", "twice"),
        ("CREATE TABLE z (id KEYWORD PRIMARY KEY, n INT UNIQUE)", "UNIQUE"),
    ] {
        let e = fails(&db, sql);
        assert!(e.contains(says), "{sql}: {e}");
    }
    // A key declared NOT NULL needs no feature: it always holds a value.
    written(&db, "CREATE TABLE plain (id KEYWORD PRIMARY KEY NOT NULL, n INT NULL, m INT DEFAULT NULL)");
    assert_eq!(format_on_disk(&path), 2);
    let plain = db.catalog().into_iter().find(|t| t.table == "plain").unwrap();
    assert!(plain.fields.iter().all(|f| !f.not_null && f.default.is_none()), "{plain:?}");

    written(&db, TASKS);
    assert_eq!(format_on_disk(&path), 3);
    let schema = db.catalog().into_iter().find(|t| t.table == "tasks").unwrap();
    let short = schema.to_short_json();
    assert_eq!(short["fields"][0], serde_json::json!({"name": "id", "type": "keyword"}));
    assert_eq!(short["fields"][2], serde_json::json!({"name": "state", "type": "keyword", "not_null": true, "default": "open"}));
    assert_eq!(short["fields"][6], serde_json::json!({"name": "created", "type": "timestamp", "not_null": true, "default": "now"}));
    assert_eq!(Schema::from_json_value(&short).unwrap(), *schema);

    // Columns left out take their defaults; NULL given stays NULL.
    let before = now();
    written(&db, "INSERT INTO tasks (id, title) VALUES ('a', 'write it')");
    written(&db, "INSERT INTO tasks (id, title, state, priority, weight) VALUES ('b', 'test it', 'doing', NULL, 1)");
    written(&db, "INSERT INTO tasks (id, title, priority, due, note) VALUES ('c', 'ship it', DEFAULT, DEFAULT, DEFAULT)");
    written(&db, "INSERT INTO tasks VALUES ('d', 'all of it', 'done', 9, 0.0, TRUE, '2026-10-01', NULL, 'by hand')");
    let after = now();
    let a = row(&db, "SELECT id, title, state, priority, weight, done, due, note FROM tasks WHERE id = 'a'");
    assert_eq!(a, [s("a"), s("write it"), s("open"), Value::Int(2), Value::Float(-0.5), Value::Bool(false), Value::Timestamp(DEC31), Value::Null]);
    let b = row(&db, "SELECT state, priority, weight FROM tasks WHERE id = 'b'");
    assert_eq!(b, [s("doing"), Value::Null, Value::Float(1.0)]);
    assert_eq!(row(&db, "SELECT priority, due, note FROM tasks WHERE id = 'c'"), [Value::Int(2), Value::Timestamp(DEC31), Value::Null]);
    assert_eq!(row(&db, "SELECT state, priority, done, created, due FROM tasks WHERE id = 'd'"), [
        s("done"),
        Value::Int(9),
        Value::Bool(true),
        Value::Timestamp(1_790_812_800_000_000),
        Value::Null
    ]);
    // CURRENT_TIMESTAMP is the time of the statement, the same for its rows.
    let created: Vec<i64> = rows(&db, "SELECT created FROM tasks WHERE id IN ('a', 'b', 'c') ORDER BY created")
        .into_iter()
        .map(|r| match r[0] {
            Value::Timestamp(t) => t,
            ref v => panic!("{v:?}"),
        })
        .collect();
    assert!(created.iter().all(|&t| (before..=after).contains(&t)), "{created:?} not in {before}..={after}");
    written(&db, "INSERT INTO tasks (id, title) VALUES ('e', 'one'), ('f', 'two')");
    let ef = rows(&db, "SELECT created FROM tasks WHERE id IN ('e', 'f')");
    assert_eq!(ef[0], ef[1]);

    // NOT NULL is refused, whether left out, NULL or DEFAULT without one.
    for sql in [
        "INSERT INTO tasks (id) VALUES ('x')",
        "INSERT INTO tasks (id, title, state) VALUES ('x', 'x', NULL)",
        "INSERT INTO tasks (id, title, done) VALUES ('x', 'x', NULL)",
        "INSERT INTO tasks (id, title) VALUES ('x', DEFAULT)",
        "INSERT OR REPLACE INTO tasks (id, title, created) VALUES ('a', 'x', NULL)",
        "UPDATE tasks SET title = NULL WHERE id = 'a'",
        "UPDATE tasks SET done = NULL WHERE id = 'a'",
        "INSERT INTO tasks (id, title) VALUES ('a', 'x') ON CONFLICT (id) DO UPDATE SET title = excluded.note",
    ] {
        let e = fails(&db, sql);
        assert!(e.contains("is NOT NULL"), "{sql}: {e}");
    }
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM tasks"), [vec![Value::Int(6)]]);
    assert_eq!(row(&db, "SELECT title, done FROM tasks WHERE id = 'a'"), [s("write it"), Value::Bool(false)]);

    // DEFAULT in an assignment sets the default, or NULL without one.
    written(&db, "UPDATE tasks SET priority = DEFAULT, state = DEFAULT, note = DEFAULT WHERE id = 'd'");
    assert_eq!(row(&db, "SELECT state, priority, note FROM tasks WHERE id = 'd'"), [s("open"), Value::Int(2), Value::Null]);
    // An upsert's proposed row holds the defaults of what it leaves out.
    written(&db, "INSERT INTO tasks (id, title) VALUES ('b', 'test it again') ON CONFLICT (id) DO UPDATE SET title = excluded.title, state = excluded.state");
    assert_eq!(row(&db, "SELECT title, state, priority FROM tasks WHERE id = 'b'"), [s("test it again"), s("open"), Value::Null]);
    written(&db, "INSERT OR REPLACE INTO tasks (id, title) VALUES ('b', 'replaced')");
    assert_eq!(row(&db, "SELECT title, state, priority FROM tasks WHERE id = 'b'"), [s("replaced"), s("open"), Value::Int(2)]);

    // Values set in place: NULL is refused only where NOT NULL.
    let mut txn = db.storage().begin().unwrap();
    let mut t = txn.table("tasks").unwrap();
    assert!(t.set(&s("a"), "priority", Value::Null).unwrap());
    assert!(t.set(&s("a"), "done", Value::Null).unwrap_err().to_string().contains("done is NOT NULL"));
    assert!(t.set(&s("a"), "created", Value::Null).is_err());
    assert!(t.insert(vec![s("y"), s("t"), Value::Null, Value::Null, Value::Null, Value::Int(0), Value::Int(0), Value::Null, Value::Null]).unwrap_err().to_string().contains("state is NOT NULL"));
    txn.commit().unwrap();
    assert_eq!(row(&db, "SELECT priority, done FROM tasks WHERE id = 'a'"), [Value::Null, Value::Bool(false)]);

    let check = |db: &Db| {
        assert_eq!(rows(db, "SELECT COUNT(*) FROM tasks"), [vec![Value::Int(6)]]);
        assert_eq!(rows(db, "SELECT COUNT(*) FROM tasks WHERE state = 'open'"), [vec![Value::Int(6)]]);
        assert!(fails(db, "INSERT INTO tasks (id, title, state) VALUES ('z', 'z', NULL)").contains("state is NOT NULL"));
        written(db, "INSERT INTO tasks (id, title) VALUES ('z', 'z')");
        assert_eq!(row(db, "SELECT state, priority, due FROM tasks WHERE id = 'z'"), [s("open"), Value::Int(2), Value::Timestamp(DEC31)]);
        written(db, "DELETE FROM tasks WHERE id = 'z'");
    };
    check(&db);
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    assert_eq!(format_on_disk(&path), 3);
    check(&db);

    // Without the table, the database needs format 2 again (two tables).
    written(&db, "DROP TABLE tasks");
    assert_eq!(format_on_disk(&path), 2);
    written(&db, "DROP TABLE plain");
    assert_eq!(format_on_disk(&path), 1);
    if layout == Layout::SingleFile {
        let slots = meta_slots(&path);
        assert!(slots.iter().all(|m| m.is_some_and(|m| m.format == 1)), "{slots:?}");
    }
}

#[test]
fn json_csv_and_the_rust_api() {
    let schema = Schema::from_json(
        r#"{"table": "events", "key": "id", "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "kind", "type": "keyword", "not_null": true, "default": "click"},
            {"name": "n", "type": "int", "default": 1},
            {"name": "at", "type": "timestamp", "default": "now"},
            {"name": "ok", "type": "bool", "default": true},
            {"name": "body", "type": "text", "not_null": true}
        ]}"#,
    )
    .unwrap();
    assert_eq!(schema.fields[1], Field::new("kind", FieldType::Keyword).not_null().default(serde_json::json!("click")));
    for bad in [
        r#"{"name": "n", "type": "int", "default": "one"}"#,
        r#"{"name": "n", "type": "float", "default": true}"#,
        r#"{"name": "n", "type": "keyword", "default": 3}"#,
        r#"{"name": "n", "type": "int", "default": "now"}"#,
        r#"{"name": "n", "type": "text", "from": ["id"], "default": "x"}"#,
        r#"{"name": "n", "type": "int", "not_null": "yes"}"#,
    ] {
        let j = format!(r#"{{"table": "t", "key": "id", "fields": [{{"name": "id", "type": "keyword"}}, {bad}]}}"#);
        assert!(Schema::from_json(&j).is_err(), "{bad}");
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.strata");
    let db = Db::create(&path, schema.clone(), DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    assert_eq!(format_on_disk(&path), 3);

    // The Rust API writes rows as they are: start from the defaults.
    let t0 = now();
    let mut doc = schema.defaults(t0).unwrap();
    assert_eq!(doc, [Value::Null, s("click"), Value::Int(1), Value::Int(t0), Value::Int(1), Value::Null]);
    doc[0] = s("a");
    doc[5] = s("first");
    db.load([doc]).unwrap();
    let e = db.load([vec![s("b"), Value::Null, Value::Null, Value::Null, Value::Null, s("x")]]).unwrap_err().to_string();
    assert!(e.contains("kind is NOT NULL"), "{e}");

    // JSON: keys left out take their defaults, null stays null.
    let before = now();
    let jsonl = "{\"id\":\"b\",\"body\":\"second\"}\n{\"id\":\"c\",\"body\":\"third\",\"n\":null,\"at\":null,\"kind\":\"view\"}\n";
    assert_eq!(ingest::import(&db, Rows::new(&schema, std::io::Cursor::new(jsonl), Format::Jsonl).unwrap()).unwrap().rows, 2);
    let after = now();
    let b = row(&db, "SELECT kind, n, ok, body FROM events WHERE id = 'b'");
    assert_eq!(b, [s("click"), Value::Int(1), Value::Bool(true), s("second")]);
    let Value::Timestamp(at) = row(&db, "SELECT at FROM events WHERE id = 'b'")[0] else { panic!() };
    assert!((before..=after).contains(&at));
    assert_eq!(row(&db, "SELECT kind, n, at FROM events WHERE id = 'c'"), [s("view"), Value::Null, Value::Null]);
    for bad in ["{\"id\":\"x\"}", "{\"id\":\"x\",\"body\":\"y\",\"kind\":null}"] {
        let mut r = Rows::new(&schema, std::io::Cursor::new(bad), Format::Jsonl).unwrap();
        let e = r.next().unwrap().unwrap_err().to_string();
        assert!(e.starts_with("schema error: line 1: ") && e.contains("is NOT NULL"), "{bad}: {e}");
        assert!(ingest::document(&schema, &serde_json::from_str(bad).unwrap()).is_err(), "{bad}");
    }

    // CSV: a column the header lacks takes its default, an empty cell is null.
    let csv = "id,body,n\nd,fourth,\ne,fifth,7\n";
    ingest::import(&db, Rows::new(&schema, std::io::Cursor::new(csv), Format::Csv).unwrap()).unwrap();
    assert_eq!(row(&db, "SELECT kind, n, ok FROM events WHERE id = 'd'"), [s("click"), Value::Null, Value::Bool(true)]);
    assert_eq!(row(&db, "SELECT kind, n FROM events WHERE id = 'e'"), [s("click"), Value::Int(7)]);
    // The rows of one file share one time, and document_at takes it given.
    assert_eq!(row(&db, "SELECT at FROM events WHERE id = 'd'"), row(&db, "SELECT at FROM events WHERE id = 'e'"));
    assert_eq!(ingest::document_at(&schema, &serde_json::json!({"id": "g", "body": "x"}), DEC31).unwrap()[3], Value::Int(DEC31));
    let csv = "id,body,kind\nf,sixth,\n";
    let e = ingest::import(&db, Rows::new(&schema, std::io::Cursor::new(csv), Format::Csv).unwrap()).unwrap_err().to_string();
    assert!(e.contains("line 2: kind is NOT NULL"), "{e}");
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM events"), [vec![Value::Int(5)]]);
}
