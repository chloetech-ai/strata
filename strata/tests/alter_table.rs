//! `ALTER TABLE ... ADD / DROP / RENAME COLUMN` on tables that hold rows
//! (DESIGN.md §3.7): rows written before a column was added read it as NULL
//! or the column's default, everywhere a query looks at it, before and after
//! a reopen and a merge; dropped columns leave the table; renamed ones keep
//! their data.

use std::path::Path;

use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::schema::{Alter, Field, FieldType, Schema};
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::value::Value;

fn items() -> Schema {
    Schema::new(
        "items",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: true }),
            Field::new("n", FieldType::Int),
        ],
        Some("id"),
    )
    .unwrap()
}

fn opts() -> DbOptions {
    DbOptions { sync: false, layout: layout(), ..DbOptions::default() }
}

thread_local! {
    static LAYOUT: std::cell::Cell<Layout> = const { std::cell::Cell::new(Layout::SingleFile) };
}

fn layout() -> Layout {
    LAYOUT.with(|l| l.get())
}

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    rows_with(db, sql, &Params::new())
}

fn rows_with(db: &Db, sql: &str, p: &Params) -> Vec<Vec<Value>> {
    match db.sql(sql, p).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => panic!("{sql}: expected rows"),
    }
}

fn ids(db: &Db, sql: &str) -> Vec<String> {
    rows(db, sql).into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

/// The error `sql` fails with.
fn fails(db: &Db, sql: &str) -> String {
    match db.sql(sql, &Params::new()) {
        Ok(_) => panic!("{sql}: expected an error"),
        Err(e) => e.to_string(),
    }
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn segments(path: &Path) -> usize {
    strata::storage::manifest::read(path).unwrap().segments().count()
}

fn merge(db: &Db) {
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
}

/// Three rows in two segments and one in the memtable.
fn filled(path: &Path) -> Db {
    let db = Db::create(path, items(), opts()).unwrap();
    run(&db, "INSERT INTO items (id, title, n) VALUES ('a', 'red apple', 1), ('b', 'green apple', 2)");
    db.storage().flush().unwrap();
    run(&db, "INSERT INTO items (id, title, n) VALUES ('c', 'yellow pear', 3)");
    db.storage().flush().unwrap();
    run(&db, "INSERT INTO items (id, title, n) VALUES ('e', 'blue plum', 5)");
    db
}

/// What the tests below expect of `items` once `price FLOAT DEFAULT 1.5`,
/// `tag KEYWORD` and `note TEXT` were added and `d` written with all three.
fn check_added(db: &Db) {
    assert_eq!(
        rows(db, "SELECT id, price, tag, n FROM items ORDER BY id"),
        [
            vec![s("a"), Value::Float(1.5), Value::Null, Value::Int(1)],
            vec![s("b"), Value::Float(1.5), Value::Null, Value::Int(2)],
            vec![s("c"), Value::Float(1.5), Value::Null, Value::Int(3)],
            vec![s("d"), Value::Float(3.0), s("x"), Value::Int(4)],
            vec![s("e"), Value::Float(1.5), Value::Null, Value::Int(5)],
        ]
    );
    assert_eq!(ids(db, "SELECT id FROM items WHERE price > 2"), ["d"]);
    assert_eq!(ids(db, "SELECT id FROM items WHERE price = 1.5 ORDER BY id"), ["a", "b", "c", "e"]);
    assert_eq!(ids(db, "SELECT id FROM items WHERE tag = 'x'"), ["d"]);
    assert_eq!(ids(db, "SELECT id FROM items WHERE tag IS NULL ORDER BY id"), ["a", "b", "c", "e"]);
    assert_eq!(ids(db, "SELECT id FROM items ORDER BY price DESC, id LIMIT 2"), ["d", "a"]);
    assert_eq!(ids(db, "SELECT id FROM items WHERE MATCH(note, 'pear')"), ["d"]);
    assert_eq!(ids(db, "SELECT id FROM items WHERE MATCH(title, 'apple') ORDER BY id"), ["a", "b"]);
    assert_eq!(
        rows(db, "SELECT tag, COUNT(*) FROM items GROUP BY tag ORDER BY tag"),
        [vec![s("x"), Value::Int(1)], vec![Value::Null, Value::Int(4)]]
    );
    assert_eq!(rows(db, "SELECT SUM(price) FROM items"), [[Value::Float(9.0)]]);
}

#[test]
fn added_columns_read_their_default_in_rows_written_before() {
    added_columns_read_their_default(Layout::SingleFile);
}

#[test]
fn added_columns_read_their_default_in_a_directory() {
    LAYOUT.with(|l| l.set(Layout::Directory));
    added_columns_read_their_default(Layout::Directory);
}

fn added_columns_read_their_default(layout: Layout) {
    assert_eq!(self::layout(), layout);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.strata");
    let db = filled(&path);
    assert!(fails(&db, "INSERT INTO items (id, price) VALUES ('d', 3.0)").contains("no such column"));

    run(&db, "ALTER TABLE items ADD COLUMN price FLOAT DEFAULT 1.5, ADD COLUMN tag KEYWORD, ADD note TEXT");
    let names: Vec<String> = db.schema().fields.iter().map(|f| f.name.clone()).collect();
    assert_eq!(names, ["id", "title", "n", "price", "tag", "note"]);
    run(&db, "INSERT INTO items (id, title, n, price, tag, note) VALUES ('d', 'a pear', 4, 3.0, 'x', 'a ripe pear')");
    check_added(&db);
    assert!(strata::storage::manifest::read(&path).unwrap().features().contains(&"added_fields".to_string()));

    // Rows written without the new columns read their defaults too.
    run(&db, "INSERT INTO items (id, n) VALUES ('f', 6)");
    assert_eq!(rows(&db, "SELECT price, tag FROM items WHERE id = 'f'"), [[Value::Float(1.5), Value::Null]]);
    run(&db, "DELETE FROM items WHERE id = 'f'");

    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    check_added(&db);

    // A merge writes the columns into the merged segment.
    db.storage().flush().unwrap();
    assert!(segments(&path) > 1);
    merge(&db);
    assert_eq!(segments(&path), 1);
    check_added(&db);
    db.storage().check().unwrap();
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    check_added(&db);
}

#[test]
fn rows_written_after_an_alter_survive_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.strata");
    let db = filled(&path);
    run(&db, "ALTER TABLE items ADD COLUMN price FLOAT DEFAULT 1.5, ADD COLUMN tag KEYWORD, ADD COLUMN note TEXT");
    run(&db, "INSERT INTO items (id, title, n, price, tag, note) VALUES ('d', 'a pear', 4, 3.0, 'x', 'a ripe pear')");
    // No flush: `d` is only in the WAL, which is replayed with the new schema.
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    check_added(&db);
}

#[test]
fn an_added_vector_is_missing_in_older_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.strata");
    let db = filled(&path);
    run(&db, "ALTER TABLE items ADD COLUMN v VECTOR(2), ADD COLUMN labels KEYWORD[]");
    let p = Params::new().set("v", Value::Vector(vec![1.0, 0.0].into()));
    db.sql("INSERT INTO items (id, v, labels) VALUES ('d', :v, ARRAY['l1', 'l2'])", &p).unwrap();
    let q = Params::new().set("q", Value::Vector(vec![0.9, 0.1].into()));
    let near = |db: &Db| rows_with(db, "SELECT id FROM items ORDER BY similarity(v, :q) DESC LIMIT 2", &q);
    assert_eq!(near(&db)[0], [s("d")]);
    let stored = |db: &Db| rows(db, "SELECT id, v, labels FROM items WHERE id IN ('a', 'd') ORDER BY id");
    let want = [
        vec![s("a"), Value::Null, Value::Null],
        vec![s("d"), Value::Vector(vec![1.0, 0.0].into()), Value::List(vec![s("l1"), s("l2")])],
    ];
    assert_eq!(stored(&db), want);
    assert_eq!(ids(&db, "SELECT id FROM items WHERE labels = 'l2'"), ["d"]);
    // Binary codes, searched by HNSW, and flat.
    run(&db, "ALTER TABLE items ADD COLUMN b VECTOR(64) OPTIONS(codes = 'binary'), ADD COLUMN f VECTOR(3) OPTIONS(index = 'flat')");
    let mut v64 = vec![0.0f32; 64];
    v64[3] = 1.0;
    let p = Params::new().set("b", Value::Vector(v64.clone().into())).set("f", Value::Vector(vec![0.0, 1.0, 0.0].into()));
    db.sql("INSERT INTO items (id, b, f) VALUES ('g', :b, :f)", &p).unwrap();
    let qb = Params::new().set("q", Value::Vector(v64.into()));
    let qf = Params::new().set("q", Value::Vector(vec![0.1, 0.9, 0.0].into()));
    let by = |db: &Db, sql: &str, p: &Params| rows_with(db, sql, p).into_iter().map(|r| r[0].clone()).collect::<Vec<_>>();
    {
        let db = &db;
        assert_eq!(by(db, "SELECT id FROM items ORDER BY similarity(b, :q) DESC LIMIT 3", &qb)[0], s("g"));
        assert_eq!(by(db, "SELECT id FROM items ORDER BY similarity(f, :q) DESC LIMIT 3", &qf)[0], s("g"));
        // Older rows are not ranked, as no row without a vector is.
        run(db, "INSERT INTO items (id) VALUES ('h')");
        for id in ["a", "h"] {
            assert_eq!(by(db, &format!("SELECT id FROM items WHERE id = '{id}' ORDER BY similarity(f, :q) DESC"), &qf), Vec::<Value>::new());
        }
    }
    db.storage().flush().unwrap();
    merge(&db);
    assert_eq!(near(&db)[0], [s("d")]);
    assert_eq!(stored(&db), want);
    assert_eq!(by(&db, "SELECT id FROM items ORDER BY similarity(b, :q) DESC LIMIT 3", &qb)[0], s("g"));
    assert_eq!(rows(&db, "SELECT f FROM items WHERE id = 'a'"), [[Value::Null]]);
    db.storage().check().unwrap();
}

#[test]
fn not_null_columns_need_a_default_once_the_table_has_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("t.strata"));
    let e = fails(&db, "ALTER TABLE items ADD COLUMN size INT NOT NULL");
    assert!(e.contains("NOT NULL") && e.contains("DEFAULT"), "{e}");
    run(&db, "ALTER TABLE items ADD COLUMN size INT NOT NULL DEFAULT 7");
    assert_eq!(rows(&db, "SELECT DISTINCT size FROM items"), [[Value::Int(7)]]);

    run(&db, "CREATE TABLE empty (id KEYWORD PRIMARY KEY)");
    run(&db, "ALTER TABLE empty ADD COLUMN size INT NOT NULL");
    let e = fails(&db, "INSERT INTO empty (id) VALUES ('a')");
    assert!(e.contains("size"), "{e}");
}

#[test]
fn dropped_columns_leave_the_table_and_their_data_with_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.strata");
    let db = filled(&path);
    run(&db, "ALTER TABLE items DROP COLUMN n");
    assert!(fails(&db, "SELECT n FROM items").contains("no such column"));
    assert_eq!(rows(&db, "SELECT * FROM items WHERE id = 'a'"), [[s("a"), s("red apple")]]);
    assert!(fails(&db, "INSERT INTO items (id, n) VALUES ('d', 4)").contains("no such column"));

    // A column added under the old name is a new column: older rows read
    // NULL, not the dropped values.
    run(&db, "ALTER TABLE items ADD COLUMN n INT");
    assert_eq!(ids(&db, "SELECT id FROM items WHERE n IS NULL ORDER BY id"), ["a", "b", "c", "e"]);
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    assert_eq!(ids(&db, "SELECT id FROM items WHERE n IS NULL ORDER BY id"), ["a", "b", "c", "e"]);
    db.storage().flush().unwrap();
    merge(&db);
    assert_eq!(ids(&db, "SELECT id FROM items WHERE n IS NULL ORDER BY id"), ["a", "b", "c", "e"]);
    assert_eq!(ids(&db, "SELECT id FROM items WHERE MATCH(title, 'pear')"), ["c"]);

    // The key, the only column and columns other things name stay.
    assert!(fails(&db, "ALTER TABLE items DROP COLUMN id").contains("primary key"));
    run(&db, "CREATE TABLE one (id KEYWORD)");
    assert!(fails(&db, "ALTER TABLE one DROP COLUMN id").contains("only column"));
    run(&db, "CREATE RANK PROFILE by_n ON items AS SELECT * FROM items ORDER BY n DESC");
    let e = fails(&db, "ALTER TABLE items DROP COLUMN n");
    assert!(e.contains("by_n"), "{e}");
}

#[test]
fn renamed_columns_keep_their_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.strata");
    let db = filled(&path);
    run(&db, "ALTER TABLE items RENAME COLUMN title TO headline");
    assert_eq!(ids(&db, "SELECT id FROM items WHERE MATCH(headline, 'apple') ORDER BY id"), ["a", "b"]);
    assert!(fails(&db, "SELECT title FROM items").contains("no such column"));
    run(&db, "INSERT INTO items (id, headline) VALUES ('d', 'apple pie')");
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    assert_eq!(ids(&db, "SELECT id FROM items WHERE MATCH(headline, 'apple') ORDER BY id"), ["a", "b", "d"]);
    db.storage().flush().unwrap();
    merge(&db);
    assert_eq!(rows(&db, "SELECT headline FROM items WHERE id = 'c'"), [[s("yellow pear")]]);
    assert!(fails(&db, "ALTER TABLE items RENAME COLUMN n TO headline").contains("headline"));
    run(&db, "CREATE RANK PROFILE by_n ON items AS SELECT * FROM items ORDER BY n DESC");
    let e = fails(&db, "ALTER TABLE items RENAME COLUMN n TO m");
    assert!(e.contains("by_n"), "{e}");
}

#[test]
fn if_exists_and_if_not_exists_skip_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("t.strata"));
    run(&db, "ALTER TABLE IF EXISTS nosuch ADD COLUMN x INT");
    assert!(fails(&db, "ALTER TABLE nosuch ADD COLUMN x INT").contains("nosuch"));
    run(&db, "ALTER TABLE items ADD COLUMN IF NOT EXISTS n INT");
    run(&db, "ALTER TABLE items DROP COLUMN IF EXISTS nosuch");
    assert!(fails(&db, "ALTER TABLE items ADD COLUMN n INT").contains("n"));
    assert!(fails(&db, "ALTER TABLE items DROP COLUMN nosuch").contains("nosuch"));
    // The operations of one statement apply in order.
    run(&db, "ALTER TABLE items ADD COLUMN x INT, DROP COLUMN x, ADD COLUMN IF NOT EXISTS x KEYWORD");
    assert_eq!(db.schema().field("x").unwrap().ty, FieldType::Keyword);
    let names: Vec<String> = db.schema().fields.iter().map(|f| f.name.clone()).collect();
    assert_eq!(names, ["id", "title", "n", "x"]);
}

#[test]
fn the_rust_api_alters_any_table() {
    let dir = tempfile::tempdir().unwrap();
    let db = filled(&dir.path().join("t.strata"));
    run(&db, "CREATE TABLE notes (id KEYWORD PRIMARY KEY, body TEXT)");
    run(&db, "INSERT INTO notes (id, body) VALUES ('n1', 'hello')");
    let flag = Field::from_short_json(&serde_json::json!({"name": "pinned", "type": "bool", "default": false})).unwrap();
    db.alter_table("notes", &[Alter::AddField(flag), Alter::RenameField { from: "body".into(), to: "text".into() }]).unwrap();
    assert_eq!(rows(&db, "SELECT id, text, pinned FROM notes"), [[s("n1"), s("hello"), Value::Bool(false)]]);
    // The other table is untouched.
    assert_eq!(rows(&db, "SELECT n FROM items WHERE id = 'a'"), [[Value::Int(1)]]);
    assert!(db.alter_table("notes", &[Alter::DropField("nosuch".into())]).unwrap_err().to_string().contains("nosuch"));
}

#[test]
fn an_added_embedding_embeds_new_rows_and_follows_renames() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    let db = filled(&dir.path().join("t.strata"));
    assert!(db.open_embedders(&models).is_empty());
    run(&db, "ALTER TABLE items ADD COLUMN emb VECTOR OPTIONS(embed = 'title', model = 'hash:8', index = 'flat')");
    assert!(db.embedder_in("items", "emb").is_some());
    let h = strata::embed::HashEmbedder { dim: 8 };
    let vector = |text: &str| Value::Vector(strata::embed::Embedder::embed(&h, &[text]).remove(0).into());
    run(&db, "INSERT INTO items (id, title) VALUES ('d', 'ripe mango')");
    assert_eq!(rows(&db, "SELECT emb FROM items WHERE id = 'd'"), [[vector("ripe mango")]]);
    // Older rows have no vector until their source is written again.
    assert_eq!(rows(&db, "SELECT emb FROM items WHERE id = 'a'"), [[Value::Null]]);
    run(&db, "UPDATE items SET title = title WHERE id = 'a'");
    assert_eq!(rows(&db, "SELECT emb FROM items WHERE id = 'a'"), [[vector("red apple")]]);

    // The embedding follows its source's new name, and keeps it from a drop.
    run(&db, "ALTER TABLE items RENAME COLUMN title TO headline");
    assert_eq!(db.schema().field("emb").unwrap().embed.as_ref().unwrap().from, ["headline"]);
    run(&db, "INSERT INTO items (id, headline) VALUES ('f', 'sour lime')");
    assert_eq!(rows(&db, "SELECT emb FROM items WHERE id = 'f'"), [[vector("sour lime")]]);
    let e = fails(&db, "ALTER TABLE items DROP COLUMN headline");
    assert!(e.contains("emb"), "{e}");
    run(&db, "ALTER TABLE items DROP COLUMN emb");
    assert!(db.embedder_in("items", "emb").is_none());
    run(&db, "ALTER TABLE items DROP COLUMN headline");
}

#[test]
fn values_set_in_place_keep_their_column() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.strata");
    let db = filled(&path);
    db.storage().flush().unwrap();
    // INT and FLOAT values of rows in segments are set in place.
    run(&db, "UPDATE items SET n = 10 WHERE id = 'a'");
    run(&db, "ALTER TABLE items ADD COLUMN price FLOAT, ADD COLUMN stock INT DEFAULT 3");
    run(&db, "UPDATE items SET price = 2.5, stock = 4 WHERE id = 'b'");
    let all = "SELECT id, n, price, stock FROM items ORDER BY id";
    let want = [
        vec![s("a"), Value::Int(10), Value::Null, Value::Int(3)],
        vec![s("b"), Value::Int(2), Value::Float(2.5), Value::Int(4)],
        vec![s("c"), Value::Int(3), Value::Null, Value::Int(3)],
        vec![s("e"), Value::Int(5), Value::Null, Value::Int(3)],
    ];
    assert_eq!(rows(&db, all), want);
    assert_eq!(ids(&db, "SELECT id FROM items WHERE stock > 3"), ["b"]);
    db.storage().check().unwrap();

    // Dropping a column before the one set in place moves it; the value
    // set stays with its column.
    run(&db, "ALTER TABLE items DROP COLUMN n");
    let after = "SELECT id, price, stock FROM items ORDER BY id";
    let want: Vec<Vec<Value>> = want.iter().map(|r| vec![r[0].clone(), r[2].clone(), r[3].clone()]).collect();
    assert_eq!(rows(&db, after), want);
    run(&db, "ALTER TABLE items ADD COLUMN n INT");
    assert_eq!(rows(&db, "SELECT n FROM items WHERE id = 'a'"), [[Value::Null]], "a dropped column's values stay dropped");
    db.storage().check().unwrap();
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    assert_eq!(rows(&db, after), want);
    merge(&db);
    assert_eq!(rows(&db, after), want);
    assert_eq!(rows(&db, "SELECT n FROM items WHERE id = 'a'"), [[Value::Null]]);
    db.storage().check().unwrap();
}
