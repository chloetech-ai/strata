//! SQLite files into Strata and back (`strata::sqlite`, feature `sqlite`).

#![cfg(feature = "sqlite")]

use std::path::Path;

use rusqlite::Connection;
use serde_json::json;
use strata::db::Db;
use strata::query::sql::Params;
use strata::sqlite::{self, ExportOptions, ImportOptions};
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema, Value};

/// A small shop: keys of both kinds, references, dates as text and as
/// Unix time, booleans, a DECIMAL holding whole and fractional numbers, a
/// JSON list, float32 vectors, a BLOB, an FTS5 index over two columns, a
/// composite key, names that are not identifiers, a column whose values
/// break its type, a view and a CHECK constraint.
fn shop(path: &Path) {
    let c = Connection::open(path).unwrap();
    let v = |xs: [f32; 4]| xs.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
    c.execute_batch(
        "PRAGMA foreign_keys = OFF;
         CREATE TABLE customers (
           id TEXT PRIMARY KEY,
           name TEXT NOT NULL,
           city VARCHAR(40),
           \"Signup Date\" DATETIME DEFAULT CURRENT_TIMESTAMP,
           vip BOOLEAN DEFAULT 0
         );
         CREATE TABLE products (
           id INTEGER PRIMARY KEY,
           name TEXT NOT NULL,
           description TEXT,
           price DECIMAL(10,2) NOT NULL,
           tags JSON,
           emb F32_BLOB(4),
           photo BLOB,
           sku TEXT UNIQUE
         );
         CREATE VIRTUAL TABLE products_fts USING fts5(name, description, content='products', content_rowid='id', tokenize='porter unicode61');
         CREATE TABLE orders (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           customer_id TEXT REFERENCES customers(id),
           placed_at TIMESTAMP,
           total REAL,
           shipped BOOLEAN,
           note TEXT,
           CHECK (total >= 0)
         );
         CREATE INDEX orders_customer ON orders(customer_id);
         CREATE TABLE order_items (
           order_id INTEGER REFERENCES orders(id),
           product_id INTEGER REFERENCES products(id),
           qty INTEGER DEFAULT 1,
           PRIMARY KEY (order_id, product_id)
         );
         CREATE TABLE \"Odd Names\" (\"Unit Price\" REAL, \"2nd\" INTEGER, mixed INTEGER);
         CREATE TABLE events (id INTEGER PRIMARY KEY, at DATETIME, kind);
         CREATE VIEW big_orders AS SELECT * FROM orders WHERE total > 100;
         CREATE VIRTUAL TABLE faq USING fts4(question, answer, tokenize=porter);
         INSERT INTO faq VALUES ('How do returns work?', 'Send the item back within thirty days for a full refund.');
         INSERT INTO customers VALUES ('c1', 'Ada Lovelace', 'London', '2024-01-02 10:00:00', 1);
         INSERT INTO customers VALUES ('c2', 'Alan Turing', 'Manchester', '2024-03-04T05:06:07Z', 0);
         INSERT INTO customers (id, name, city, vip) VALUES ('c3', 'Grace Hopper', 'New York', 1);
         INSERT INTO orders VALUES (1, 'c1', '2024-05-01 12:00:00', 30.0, 1, 'leave it at the door please if nobody answers');
         INSERT INTO orders VALUES (2, 'c2', '2024-05-02', 150.0, 0, NULL);
         INSERT INTO orders VALUES (3, 'c1', NULL, 5.25, 1, 'gift');
         INSERT INTO order_items VALUES (1, 1, 2), (1, 2, 1), (2, 2, 10);
         INSERT INTO \"Odd Names\" VALUES (1.5, 2, 3), (2.5, 3, 'n/a');
         INSERT INTO events VALUES (1, 1700000000, 5), (2, 1700000100, 'x');",
    )
    .unwrap();
    let mut ins = c.prepare("INSERT INTO products VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)").unwrap();
    ins.execute(rusqlite::params![1, "Red mug", "A sturdy red ceramic mug for hot drinks and long mornings", 12.5, r#"["kitchen","red"]"#, v([1.0, 0.0, 0.0, 0.0]), vec![0u8, 255], "SKU-1"]).unwrap();
    ins.execute(rusqlite::params![2, "Blue pen", "Writes smoothly on paper, refills sold separately in packs of three", 2, r#"["office"]"#, v([0.0, 1.0, 0.0, 0.0]), None::<Vec<u8>>, "SKU-2"]).unwrap();
    c.execute_batch("INSERT INTO products_fts (products_fts) VALUES ('rebuild');").unwrap();
}

fn ids(db: &Db, sql: &str) -> Vec<Value> {
    db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")).rows.into_iter().map(|r| r[0].clone()).collect()
}

/// [`ids`] in order (Strata orders by numbers only).
fn sorted(db: &Db, sql: &str) -> Vec<Value> {
    let mut v = ids(db, sql);
    v.sort_by_key(|x| format!("{x:?}"));
    v
}

fn field<'a>(s: &'a Schema, name: &str) -> &'a Field {
    s.field(name).unwrap_or_else(|| panic!("{} has no field {name}", s.table))
}

#[test]
fn a_sqlite_shop_becomes_strata_tables() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("shop.sqlite");
    shop(&src);
    assert!(sqlite::is_sqlite_file(&src));
    let plan = sqlite::plan(&src, None, &ImportOptions::default()).unwrap();
    let order: Vec<&str> = plan.tables.iter().map(|t| t.schema.table.as_str()).collect();
    assert_eq!(order, ["customers", "products", "orders", "order_items", "Odd_Names", "events", "faq"]);
    let skipped: Vec<&str> = plan.skipped.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(skipped, ["products_fts", "big_orders"]);

    let (db, done) = sqlite::import_new(&src, &dir.path().join("shop.strata"), DbOptions::default(), &ImportOptions::default()).unwrap();
    let rows: Vec<(String, u64)> = done.tables.iter().map(|t| (t.table.clone(), t.rows)).collect();
    assert_eq!(rows, [("customers".into(), 3), ("products".into(), 2), ("orders".into(), 3), ("order_items".into(), 3), ("Odd_Names".into(), 2), ("events".into(), 2), ("faq".into(), 1)]);
    assert!(done.tables.iter().all(|t| t.created && t.bulk));
    let schema = |t: &str| db.catalog().into_iter().find(|s| s.table == t).unwrap();

    let customers = schema("customers");
    assert_eq!(customers.primary_key.map(|k| customers.fields[k].name.as_str()), Some("id"));
    assert_eq!(field(&customers, "name").ty, FieldType::Keyword);
    assert!(field(&customers, "name").not_null);
    assert_eq!(field(&customers, "Signup_Date").type_name(), "TIMESTAMP");
    assert_eq!(field(&customers, "Signup_Date").default, Some(json!("now")));
    assert_eq!(field(&customers, "vip").type_name(), "BOOL");
    assert_eq!(field(&customers, "vip").default, Some(json!(0)));

    let products = schema("products");
    assert!(matches!(field(&products, "name").ty, FieldType::Text { .. }), "indexed by FTS5");
    assert!(matches!(field(&products, "description").ty, FieldType::Text { .. }));
    assert_eq!(field(&products, "price").ty, FieldType::Float);
    assert!(field(&products, "tags").multi && field(&products, "tags").ty == FieldType::Keyword);
    assert!(matches!(field(&products, "emb").ty, FieldType::Vector { dim: 4, .. }));
    assert!(products.field("photo").is_none(), "a BLOB is left out");
    let notes = &done.tables[1].notes;
    assert!(notes.iter().any(|n| n.contains("photo") && n.contains("BLOB")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("UNIQUE (sku)")), "{notes:?}");

    let orders = schema("orders");
    assert_eq!(field(&orders, "customer_id").references.as_deref(), Some("customers"));
    assert_eq!(field(&orders, "placed_at").type_name(), "TIMESTAMP");
    assert_eq!(field(&orders, "shipped").type_name(), "BOOL");
    assert!(matches!(field(&orders, "note").ty, FieldType::Text { .. }), "long text");
    assert!(done.tables[2].notes.iter().any(|n| n.contains("CHECK")));

    let items = schema("order_items");
    assert_eq!(items.primary_key, None, "a composite key");
    assert_eq!(field(&items, "order_id").references.as_deref(), Some("orders"));
    assert_eq!(field(&items, "product_id").references.as_deref(), Some("products"));
    assert_eq!(field(&items, "qty").default, Some(json!(1)));

    let odd = schema("Odd_Names");
    assert_eq!(field(&odd, "Unit_Price").ty, FieldType::Float);
    assert_eq!(field(&odd, "_2nd").ty, FieldType::Int);
    assert_eq!(field(&odd, "mixed").ty, FieldType::Keyword, "text in an INTEGER column");
    // An FTS4 table holding its own text: its columns are TEXT, its own tables left out.
    let faq = schema("faq");
    assert_eq!(faq.fields.len(), 2);
    assert_eq!(field(&faq, "answer").text_analyzer(), strata::schema::Analyzer::English);
    assert_eq!(ids(&db, "SELECT question FROM faq WHERE MATCH(answer, 'refunds')"), [Value::from("How do returns work?")]);
    assert!(done.tables[1].notes.iter().all(|n| !n.contains("table \"")), "not renamed");
    assert_eq!(done.tables[4].notes[0], "table \"Odd Names\" is Odd_Names");
    let events = schema("events");
    assert_eq!(field(&events, "at").type_name(), "TIMESTAMP");
    assert_eq!(field(&events, "kind").ty, FieldType::Keyword);

    // The rows, through SQL.
    assert_eq!(ids(&db, "SELECT id FROM orders WHERE shipped = true ORDER BY id"), [Value::Int(1), Value::Int(3)]);
    assert_eq!(ids(&db, "SELECT id FROM orders WHERE placed_at >= '2024-05-02' ORDER BY id"), [Value::Int(2)]);
    assert_eq!(ids(&db, "SELECT id FROM products WHERE MATCH(description, 'morning')"), [Value::Int(1)], "stemmed");
    assert_eq!(ids(&db, "SELECT id FROM products WHERE tags = 'office'"), [Value::Int(2)]);
    assert_eq!(ids(&db, "SELECT price FROM products ORDER BY id"), [Value::Float(12.5), Value::Float(2.0)]);
    assert_eq!(ids(&db, "SELECT name FROM customers WHERE city = 'New York'"), [Value::Str("Grace Hopper".into())]);
    assert_eq!(ids(&db, "SELECT Signup_Date FROM customers WHERE id = 'c2'"), [Value::Timestamp(strata::timestamp::parse("2024-03-04T05:06:07Z").unwrap())]);
    assert_eq!(ids(&db, "SELECT at FROM events WHERE id = 1"), [Value::Timestamp(1_700_000_000_000_000)]);
    assert_eq!(sorted(&db, "SELECT mixed FROM Odd_Names"), [Value::Str("3".into()), Value::Str("n/a".into())]);
    // SQLite gave c3 its default when the row was written.
    assert!(matches!(ids(&db, "SELECT Signup_Date FROM customers WHERE id = 'c3'")[..], [Value::Timestamp(_)]));
    let snap = db.snapshot();
    let p = snap.table("products").unwrap();
    let docs: Vec<_> = db.documents(&p).collect::<strata::Result<_>>().unwrap();
    let emb = products.field_index("emb").unwrap();
    assert_eq!(docs[0][emb].as_vector(), Some(&[1.0, 0.0, 0.0, 0.0][..]));
    assert_eq!(ids(&db, "SELECT o.id FROM orders o JOIN customers c ON o.customer_id = c.id WHERE c.name = 'Ada Lovelace' ORDER BY o.id"), [Value::Int(1), Value::Int(3)]);
}

#[test]
fn some_tables_and_given_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("shop.sqlite");
    shop(&src);
    // The plan as JSON, edited: customers' names become full text.
    let opts = ImportOptions { tables: vec!["CUSTOMERS".into(), "orders".into()], ..Default::default() };
    let plan = sqlite::plan(&src, None, &opts).unwrap();
    assert_eq!(plan.tables.len(), 2);
    assert!(plan.skipped.is_empty(), "only the tables asked for: {:?}", plan.skipped);
    let mut j = plan.to_json();
    assert_eq!(j["tables"][0]["source"], "customers");
    assert_eq!(j["tables"][0]["columns"]["Signup Date"], "Signup_Date");
    j["tables"][0]["table"] = json!("people");
    j["tables"][0]["fields"][1]["type"] = json!("text");
    // References name the table as it is now called.
    j["tables"][1]["fields"][1]["references"] = json!("people");
    let given = sqlite::schemas_from_json(&j.to_string()).unwrap();
    assert_eq!(given[0].source.as_deref(), Some("customers"));
    let opts = ImportOptions { schemas: given, ..opts };
    let (db, done) = sqlite::import_new(&src, &dir.path().join("shop.strata"), DbOptions::default(), &opts).unwrap();
    assert_eq!(done.tables.iter().map(|t| t.table.as_str()).collect::<Vec<_>>(), ["people", "orders"]);
    let people = db.catalog().into_iter().find(|s| s.table == "people").unwrap();
    assert!(matches!(field(&people, "name").ty, FieldType::Text { .. }));
    assert_eq!(ids(&db, "SELECT id FROM people WHERE MATCH(name, 'turing')"), [Value::Str("c2".into())]);

    // An unknown table is an error naming it.
    let bad = ImportOptions { tables: vec!["nope".into()], ..Default::default() };
    let e = sqlite::plan(&src, None, &bad).unwrap_err().to_string();
    assert!(e.contains("no table nope"), "{e}");
    let view = ImportOptions { tables: vec!["big_orders".into()], ..Default::default() };
    let e = sqlite::plan(&src, None, &view).unwrap_err().to_string();
    assert!(e.contains("view"), "{e}");
}

#[test]
fn into_an_existing_table() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("shop.sqlite");
    shop(&src);
    let schema = Schema::new("customers", vec![Field::new("id", FieldType::Keyword), Field::new("name", FieldType::Text { positions: true }), Field::new("visits", FieldType::Int)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("crm.strata"), schema, DbOptions::default()).unwrap();
    let mut t = db.storage().begin().unwrap();
    t.insert(vec!["c1".into(), "Old name".into(), Value::Int(7)]).unwrap();
    t.insert(vec!["c9".into(), "Someone else".into(), Value::Int(1)]).unwrap();
    t.commit().unwrap();
    let opts = ImportOptions { tables: vec!["customers".into(), "orders".into()], ..Default::default() };
    let done = sqlite::import(&db, &src, &opts).unwrap();
    let customers = &done.tables[0];
    assert!(!customers.created && !customers.bulk);
    assert_eq!(customers.rows, 3);
    assert!(customers.notes.iter().any(|n| n.contains("\"city\" left out")), "{:?}", customers.notes);
    assert!(done.tables[1].created && done.tables[1].bulk);
    // Upserted by key: c1 is replaced (its visits too: the row is SQLite's), c9 stays.
    assert_eq!(ids(&db, "SELECT name FROM customers WHERE id = 'c1'"), [Value::Str("Ada Lovelace".into())]);
    assert_eq!(ids(&db, "SELECT visits FROM customers WHERE id = 'c1'"), [Value::Null]);
    assert_eq!(sorted(&db, "SELECT id FROM customers"), ["c1", "c2", "c3", "c9"].map(Value::from));
    // The new table references the existing one.
    let orders = db.catalog().into_iter().find(|s| s.table == "orders").unwrap();
    assert_eq!(field(&orders, "customer_id").references.as_deref(), Some("customers"));
}

#[test]
fn a_value_that_does_not_fit_names_its_row() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("shop.sqlite");
    shop(&src);
    // A schema saying "mixed" is an INT, which a row's 'n/a' is not.
    let s = Schema::new("Odd_Names", vec![Field::new("mixed", FieldType::Int)], None).unwrap();
    let opts = ImportOptions { tables: vec!["Odd Names".into()], schemas: vec![sqlite::Given { source: None, schema: s }], ..Default::default() };
    let e = sqlite::import_new(&src, &dir.path().join("x.strata"), DbOptions::default(), &opts).err().unwrap().to_string();
    assert!(e.contains("table Odd Names row 2") && e.contains("mixed"), "{e}");
    // Not a SQLite file.
    let text = dir.path().join("rows.csv");
    std::fs::write(&text, "a,b\n1,2\n").unwrap();
    assert!(!sqlite::is_sqlite_file(&text));
    let e = sqlite::plan(&text, None, &ImportOptions::default()).unwrap_err().to_string();
    assert!(e.contains("not a SQLite database"), "{e}");
}

const COVER: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

/// Every kind of field, in two tables that reference each other's keys.
fn library(path: &Path) -> Db {
    let mut authors = vec![
        Field::new("id", FieldType::Int),
        Field::new("name", FieldType::Keyword).not_null(),
        Field::new("bio", FieldType::Text { positions: true }).analyzer(strata::schema::Analyzer::Simple),
        Field::boolean("active").default(json!(true)),
    ];
    authors.push(Field::timestamp("joined").default(json!("now")));
    let authors = Schema::new("authors", authors, Some("id")).unwrap();
    let db = Db::create(path, authors, DbOptions::default()).unwrap();
    let books = Schema::new(
        "books",
        vec![
            Field::new("isbn", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: true }),
            Field::new("blurb", FieldType::Text { positions: false }),
            Field::new("body", FieldType::Text { positions: true }).generated(&["title", "blurb"]).not_stored(),
            Field::new("author", FieldType::Int).references("authors"),
            Field::new("price", FieldType::Float).default(json!(9.99)),
            Field::new("pages", FieldType::Int).not_null(),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("years", FieldType::Int).multi(),
            Field::new("cites", FieldType::Edges),
            Field::new("terms", FieldType::Sparse),
            Field::new("emb", FieldType::Vector { dim: 3, index: strata::VectorIndexKind::Flat }),
            Field::timestamp("published"),
            Field::boolean("in_print"),
            Field::new("shelf", FieldType::Keyword).default(json!("new")),
            Field::media("cover", strata::schema::MediaKind::Image),
        ],
        Some("isbn"),
    )
    .unwrap();
    db.create_table(books).unwrap();
    db.sql(
        "INSERT INTO authors (id, name, bio, joined) VALUES
           (1, 'Le Guin', 'Wrote about wizards and anarchists', '2020-01-01T00:00:00Z'),
           (2, 'Calvino', NULL, '2021-06-15 08:30:00.125')",
        &Params::new(),
    )
    .unwrap();
    db.sql("INSERT INTO authors (id, name, active) VALUES (3, 'Borges', false)", &Params::new()).unwrap();
    let mut t = db.storage().begin().unwrap();
    let mut b = t.table("books").unwrap();
    let row = |isbn: &str, title: &str, author: i64, tags: &[&str], cites: &str, emb: [f32; 3], published: &str, in_print: bool| {
        vec![
            isbn.into(),
            title.into(),
            Value::Str(format!("A novel called {title}")),
            Value::Null,
            Value::Int(author),
            Value::Float(12.0),
            Value::Int(200),
            Value::List(tags.iter().map(|&s| Value::from(s)).collect()),
            Value::List(vec![Value::Int(1970), Value::Int(1972)]),
            Value::Str(cites.into()),
            Value::Str(r#"{"wizard": 1.5, "sea": 0.25}"#.into()),
            Value::Vector(emb.to_vec().into()),
            Value::Timestamp(strata::timestamp::parse(published).unwrap()),
            Value::Bool(in_print),
            Value::Str("fiction".into()),
            Value::Str(strata::media::input_text(&json!({"uri": format!("https://example.com/{isbn}.png"), "sha256": COVER, "mime": "image/png", "bytes": 1234, "width": 600, "height": 900})).unwrap()),
        ]
    };
    b.insert(row("isbn-1", "A Wizard of Earthsea", 1, &["fantasy", "classic"], "[]", [1.0, 0.0, 0.0], "1968-11-01", true)).unwrap();
    b.insert(row("isbn-2", "Invisible Cities", 2, &["fable"], r#"["isbn-1"]"#, [0.0, 1.0, 0.0], "1972-01-01T12:00:00.5Z", false)).unwrap();
    t.commit().unwrap();
    db.sql("INSERT INTO books (isbn, title, author, pages) VALUES ('isbn-3', 'Ficciones', 3, 180)", &Params::new()).unwrap();
    db
}

fn rows_of(db: &Db, table: &str) -> Vec<Vec<Value>> {
    let snap = db.snapshot();
    let t = snap.table(table).unwrap();
    let schema = t.schema().clone();
    let mut docs: Vec<Vec<Value>> = db.documents(&t).collect::<strata::Result<_>>().unwrap();
    let k = schema.primary_key.unwrap();
    docs.sort_by_key(|d| format!("{:?}", d[k]));
    docs
}

#[test]
fn strata_to_sqlite_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = library(&dir.path().join("library.strata"));
    let out = dir.path().join("library.sqlite");
    let done = sqlite::export(&db, &out, &ExportOptions::default()).unwrap();
    assert_eq!(done.tables.iter().map(|t| (t.table.as_str(), t.rows)).collect::<Vec<_>>(), [("authors", 3), ("books", 3)]);
    assert_eq!(done.tables[0].fts.as_deref(), Some("authors_fts"));
    assert_eq!(done.tables[1].fts.as_deref(), Some("books_fts"));
    assert!(done.tables[1].indexes.contains(&"books_author_idx".to_string()));
    assert!(done.tables[1].indexes.contains(&"books_cover_idx".to_string()));
    assert!(done.tables[1].notes.iter().any(|n| n.starts_with("emb: vectors as F32_BLOB(3)")), "{:?}", done.tables[1].notes);
    assert!(sqlite::export(&db, &out, &ExportOptions::default()).unwrap_err().to_string().contains("exists"));

    // SQLite reads it as SQL.
    let c = Connection::open(&out).unwrap();
    let decl = |t: &str| -> Vec<(String, String)> {
        let mut s = c.prepare("SELECT name, type FROM pragma_table_info(?1) ORDER BY cid").unwrap();
        s.query_map([t], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    };
    let types: Vec<String> = decl("books").into_iter().map(|(n, t)| format!("{n} {t}")).collect();
    assert_eq!(
        types,
        [
            "isbn TEXT", "title TEXT", "blurb TEXT", "body TEXT", "author INTEGER", "price REAL", "pages INTEGER", "tags JSON", "years JSON", "cites JSON", "terms JSON", "emb F32_BLOB(3)", "published TIMESTAMP",
            "in_print BOOLEAN", "shelf TEXT", "cover JSON"
        ]
    );
    let one = |sql: &str| -> rusqlite::types::Value { c.query_row(sql, [], |r| r.get(0)).unwrap_or_else(|e| panic!("{sql}: {e}")) };
    use rusqlite::types::Value as S;
    assert_eq!(one("SELECT published FROM books WHERE isbn = 'isbn-2'"), S::Text("1972-01-01 12:00:00.500".into()));
    assert_eq!(one("SELECT joined FROM authors WHERE id = 2"), S::Text("2021-06-15 08:30:00.125".into()));
    assert_eq!(one("SELECT in_print FROM books WHERE isbn = 'isbn-2'"), S::Integer(0));
    assert_eq!(one("SELECT tags FROM books WHERE isbn = 'isbn-1'"), S::Text(r#"["fantasy","classic"]"#.into()));
    assert_eq!(one("SELECT count(*) FROM books, json_each(books.tags) WHERE json_each.value = 'fable'"), S::Integer(1));
    assert_eq!(one("SELECT body FROM books WHERE isbn = 'isbn-1'"), S::Text("A Wizard of Earthsea\nA novel called A Wizard of Earthsea".into()));
    assert_eq!(one("SELECT emb FROM books WHERE isbn = 'isbn-2'"), S::Blob([0.0f32, 1.0, 0.0].iter().flat_map(|x| x.to_le_bytes()).collect()));
    assert_eq!(one("SELECT isbn FROM books_fts JOIN books ON books.rowid = books_fts.rowid WHERE books_fts MATCH 'wizards'"), S::Text("isbn-1".into()), "porter stems");
    assert_eq!(one("SELECT a.name FROM books b JOIN authors a ON a.id = b.author WHERE b.isbn = 'isbn-3'"), S::Text("Borges".into()));
    assert_eq!(one(&format!("SELECT count(*) FROM books WHERE json_extract(cover, '$.sha256') = '{COVER}'")), S::Integer(2));
    assert_eq!(one("SELECT json_extract(cover, '$.uri') FROM books WHERE isbn = 'isbn-2'"), S::Text("https://example.com/isbn-2.png".into()));
    // Defaults and NOT NULL hold in SQLite too, and the triggers index new rows.
    c.execute("INSERT INTO books (isbn, title, pages) VALUES ('isbn-4', 'The Dispossessed', 400)", []).unwrap();
    assert_eq!(one("SELECT price || ' ' || shelf FROM books WHERE isbn = 'isbn-4'"), S::Text("9.99 new".into()));
    assert_eq!(one("SELECT count(*) FROM books_fts WHERE books_fts MATCH 'dispossessed'"), S::Integer(1));
    assert!(c.execute("INSERT INTO books (isbn, title) VALUES ('isbn-5', 'No pages')", []).is_err());
    c.execute("INSERT INTO authors (id, name) VALUES (4, 'Lem')", []).unwrap();
    assert!(matches!(one("SELECT joined FROM authors WHERE id = 4"), S::Text(t) if strata::timestamp::parse(&t).is_some()));
    assert_eq!(one("SELECT active FROM authors WHERE id = 4"), S::Integer(1));
    c.execute("DELETE FROM books WHERE isbn = 'isbn-4'", []).unwrap();
    c.execute("DELETE FROM authors WHERE id = 4", []).unwrap();
    assert_eq!(one("SELECT count(*) FROM books_fts WHERE books_fts MATCH 'dispossessed'"), S::Integer(0));
    drop(c);

    // And back: the same schemas and rows.
    let (back, done) = sqlite::import_new(&out, &dir.path().join("back.strata"), DbOptions::default(), &ImportOptions::default()).unwrap();
    assert_eq!(done.skipped.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["authors_fts", "books_fts"]);
    assert!(done.tables.iter().all(|t| t.notes.is_empty()), "{:?}", done.tables);
    for (a, b) in db.catalog().iter().zip(back.catalog()) {
        assert_eq!(a.to_short_json(), b.to_short_json());
    }
    for t in ["authors", "books"] {
        assert_eq!(rows_of(&db, t), rows_of(&back, t), "{t}");
    }
}

#[test]
fn export_some_tables_bare() {
    let dir = tempfile::tempdir().unwrap();
    let db = library(&dir.path().join("library.strata"));
    let out = dir.path().join("books.sqlite");
    let opts = ExportOptions { tables: vec!["BOOKS".into()], fts: false, indexes: false, meta: false };
    let done = sqlite::export(&db, &out, &opts).unwrap();
    assert_eq!(done.tables.len(), 1);
    assert!(done.tables[0].notes.iter().any(|n| n.contains("references authors, which is not exported")));
    let c = Connection::open(&out).unwrap();
    let names: Vec<String> = c.prepare("SELECT name FROM sqlite_master ORDER BY name").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(names, ["books", "sqlite_autoindex_books_1"]);
    drop(c);
    // Without the kept schemas, the import makes them from SQLite's.
    let (back, _) = sqlite::import_new(&out, &dir.path().join("back.strata"), DbOptions::default(), &ImportOptions::default()).unwrap();
    let s = &back.schema();
    assert_eq!(field(s, "published").type_name(), "TIMESTAMP");
    assert_eq!(field(s, "in_print").type_name(), "BOOL");
    assert!(matches!(field(s, "emb").ty, FieldType::Vector { dim: 3, .. }));
    assert!(field(s, "tags").multi && field(s, "years").multi && field(s, "years").ty == FieldType::Int);
    assert_eq!(field(s, "price").default, Some(json!(9.99)));
    assert_eq!(ids(&back, "SELECT isbn FROM books WHERE tags = 'fable'"), [Value::from("isbn-2")]);
}

/// Review BUG-N11 of 31 October: a table `x_y` was renamed `x_y_2` because
/// a table `x-y` before it took the name; `9223372036854775807` in a column
/// of integers and reals became a float with no note; and one infinite
/// real refused the whole import.
#[test]
fn names_as_written_and_values_noted() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("odd.sqlite");
    let c = Connection::open(&src).unwrap();
    c.execute_batch(
        "CREATE TABLE \"x-y\" (id INTEGER PRIMARY KEY, v TEXT);
         CREATE TABLE x_y (id INTEGER PRIMARY KEY, \"a-b\" TEXT, a_b TEXT, m, r REAL NOT NULL);
         INSERT INTO \"x-y\" VALUES (1, 'dash');
         INSERT INTO x_y VALUES (1, 'p', 'q', 9223372036854775807, 1.5), (2, 'p', 'q', 0.5, 9e999), (3, 'p', 'q', 2, -9e999);",
    )
    .unwrap();
    drop(c);
    let (db, done) = sqlite::import_new(&src, &dir.path().join("odd.strata"), DbOptions::default(), &ImportOptions::default()).unwrap();
    let names: Vec<(&str, &str, u64)> = done.tables.iter().map(|t| (t.source.as_str(), t.table.as_str(), t.rows)).collect();
    assert_eq!(names, [("x-y", "x_y_2", 1), ("x_y", "x_y", 3)], "a name Strata spells as SQLite does keeps it");
    let s = db.catalog().into_iter().find(|s| s.table == "x_y").unwrap();
    let fields: Vec<&str> = s.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(fields, ["id", "a_b_2", "a_b", "m", "r"]);
    let notes = &done.tables[1].notes;
    assert!(notes.iter().any(|n| n == "column \"a-b\" is field a_b_2"), "{notes:?}");
    assert!(notes.iter().all(|n| !n.contains("table \"x_y\"")), "{notes:?}");
    // The integers of a FLOAT column past 2^53 round, and the plan says so.
    assert_eq!(field(&s, "m").ty, FieldType::Float);
    assert!(notes.iter().any(|n| n.contains("\"m\"") && n.contains("2^53")), "{notes:?}");
    assert_eq!(ids(&db, "SELECT m FROM x_y ORDER BY id"), [Value::Float(9223372036854775807i64 as f64), Value::Float(0.5), Value::Float(2.0)]);
    // An infinite real is NULL, noted, and the column may hold NULL.
    assert!(notes.iter().any(|n| n.contains("\"r\"") && n.contains("2 infinite values are NULL")), "{notes:?}");
    assert!(!field(&s, "r").not_null);
    assert_eq!(ids(&db, "SELECT r FROM x_y ORDER BY id"), [Value::Float(1.5), Value::Null, Value::Null]);
    // A column of small integers beside reals needs no note.
    let c = Connection::open(&src).unwrap();
    c.execute_batch("UPDATE x_y SET m = 7 WHERE id = 1").unwrap();
    drop(c);
    let plan = sqlite::plan(&src, None, &ImportOptions::default()).unwrap();
    assert!(plan.tables[1].notes.iter().all(|n| !n.contains("2^53")), "{:?}", plan.tables[1].notes);
}
