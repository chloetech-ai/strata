//! INSERT, INSERT OR REPLACE, UPDATE and DELETE through `Db::sql`.

use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

fn schema() -> Schema {
    Schema::new(
        "items",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: false }),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("clicks", FieldType::Int),
            Field::new("price", FieldType::Float),
            Field::new("emb", FieldType::Vector { dim: 3, index: VectorIndexKind::Flat }),
        ],
        Some("id"),
    )
    .unwrap()
}

fn written(r: SqlResult) -> u64 {
    match r {
        SqlResult::Written(n) => n,
        SqlResult::Rows(_) => panic!("expected a write"),
    }
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match db.sql(sql, &Params::new()).unwrap() {
        SqlResult::Rows(r) => r.rows,
        _ => panic!("expected rows"),
    }
}

#[test]
fn sql_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("w.strata"), schema(), DbOptions::default()).unwrap();
    let p = Params::new().set("v", Value::Vector(vec![0.0, 1.0, 0.0].into()));
    let n = db
        .sql(
            "INSERT INTO items (id, title, tags, clicks, price, emb) VALUES
               ('a', 'red running shoes', ARRAY['shoes', 'sale'], 3, 59.5, ARRAY[1, 0, 0]),
               ('b', 'blue rain jacket', ARRAY['coats'], 0, 120, :v),
               ('c', 'green socks', ARRAY['socks', 'sale'], NULL, 4.25, '[0, 0, 1]')",
            &p,
        )
        .unwrap();
    assert_eq!(written(n), 3);
    assert!(db.sql("INSERT INTO items (id, title) VALUES ('a', 'dup')", &p).is_err(), "duplicate key");
    assert_eq!(rows(&db, "SELECT id FROM items WHERE tags = 'sale' ORDER BY price DESC").len(), 2);
    assert_eq!(rows(&db, "SELECT id FROM items WHERE MATCH(title, 'jacket')")[0][0], Value::Str("b".into()));

    // Counters in place, NULL counting as zero.
    assert_eq!(written(db.sql("UPDATE items SET clicks = clicks + 2 WHERE tags = 'sale'", &p).unwrap()), 2);
    assert_eq!(written(db.sql("UPDATE items SET price = price - 0.5, clicks = 10 WHERE id = 'b'", &p).unwrap()), 1);
    let r = rows(&db, "SELECT id, clicks, price FROM items ORDER BY clicks DESC, id");
    assert_eq!(
        r,
        vec![
            vec![Value::Str("b".into()), Value::Int(10), Value::Float(119.5)],
            vec![Value::Str("a".into()), Value::Int(5), Value::Float(59.5)],
            vec![Value::Str("c".into()), Value::Int(2), Value::Float(4.25)],
        ]
    );

    // A text field rewrites the document; the vector and counters survive.
    assert_eq!(written(db.sql("UPDATE items SET title = 'blue storm jacket', tags = ARRAY['coats', 'sale'] WHERE id = 'b'", &p).unwrap()), 1);
    let r = rows(&db, "SELECT id, clicks, emb FROM items WHERE MATCH(title, 'storm') AND tags = 'sale'");
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], Value::Int(10));
    assert_eq!(r[0][2], Value::Vector(vec![0.0, 1.0, 0.0].into()));
    assert!(rows(&db, "SELECT id FROM items WHERE MATCH(title, 'rain')").is_empty());

    // Upsert and delete.
    assert_eq!(written(db.sql("INSERT OR REPLACE INTO items (id, title, clicks) VALUES ('c', 'wool socks', 1)", &p).unwrap()), 1);
    assert_eq!(rows(&db, "SELECT clicks FROM items WHERE id = 'c'")[0][0], Value::Int(1));
    assert_eq!(written(db.sql("DELETE FROM items WHERE price > 100", &p).unwrap()), 1);
    assert_eq!(written(db.sql("DELETE FROM items WHERE id = 'zzz'", &p).unwrap()), 0);
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM items")[0][0], Value::Int(2));

    // Refused: the key, unknown tables and columns, non-numeric increments.
    assert!(db.sql("UPDATE items SET id = 'x'", &p).is_err());
    assert!(db.sql("UPDATE nope SET clicks = 1", &p).is_err());
    assert!(db.sql("UPDATE items SET title = title + 1", &p).is_err());
    assert!(db.sql("INSERT INTO items (id, nope) VALUES ('x', 1)", &p).is_err());
    assert!(db.sql("DROP TABLE items", &p).is_err());
}

fn seeded() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("w.strata"), schema(), DbOptions::default()).unwrap();
    let n = db
        .sql(
            "INSERT INTO items (id, title, tags, clicks, price) VALUES
               ('a', 'red shoes', ARRAY['sale'], 1, 10.0),
               ('b', 'blue coat', ARRAY['coats'], 2, 20.0),
               ('c', 'green socks', ARRAY['sale'], 3, 30.0),
               ('d', 'wool hat', ARRAY['hats', 'sale'], 4, 40.0),
               ('e', 'rain boots', ARRAY['shoes'], 5, 50.0)",
            &Params::new(),
        )
        .unwrap();
    assert_eq!(written(n), 5);
    (dir, db)
}

fn sql(db: &Db, text: &str) -> u64 {
    written(db.sql(text, &Params::new()).unwrap_or_else(|e| panic!("{text}: {e}")))
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

/// Review bug 3: `ON CONFLICT DO NOTHING` replaced the row that was there.
#[test]
fn on_conflict_do_nothing_keeps_the_row_there() {
    let (_dir, db) = seeded();
    // 'a' is there and 'f' comes twice: only the first 'f' is written.
    assert_eq!(sql(&db, "INSERT INTO items (id, title, clicks) VALUES ('a', 'gold ring', 9), ('f', 'silk scarf', 6), ('f', 'linen scarf', 7) ON CONFLICT (id) DO NOTHING"), 1);
    assert_eq!(rows(&db, "SELECT id, title, clicks FROM items WHERE id IN ('a', 'f') ORDER BY clicks"), [[s("a"), s("red shoes"), Value::Int(1)], [s("f"), s("silk scarf"), Value::Int(6)]]);
    assert_eq!(sql(&db, "INSERT OR IGNORE INTO items (id, title) VALUES ('b', 'x'), ('g', 'cotton shirt')"), 1);
    assert_eq!(sql(&db, "INSERT INTO items (id, title) VALUES ('g', 'x') ON CONFLICT DO NOTHING"), 0);
    assert_eq!(rows(&db, "SELECT title FROM items WHERE id = 'g'"), [[s("cotton shirt")]]);
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM items"), [[Value::Int(7)]]);
    // The conflict is on the key.
    assert!(db.sql("INSERT INTO items (id, title) VALUES ('h', 'x') ON CONFLICT (title) DO NOTHING", &Params::new()).is_err());
}

/// `ON CONFLICT DO UPDATE` sets the fields of the rows there and inserts the rest.
#[test]
fn on_conflict_do_update_sets_the_row_there() {
    let (_dir, db) = seeded();
    let p = Params::new();
    // Counters in place; `excluded` is the row the statement gave.
    assert_eq!(sql(&db, "INSERT INTO items (id, title, clicks, price) VALUES ('a', 'x', 7, 11.5), ('f', 'silk scarf', 6, 60.0) ON CONFLICT (id) DO UPDATE SET clicks = clicks + 1, price = excluded.price"), 2);
    assert_eq!(rows(&db, "SELECT id, title, clicks, price FROM items WHERE id IN ('a', 'f') ORDER BY clicks"), [[s("a"), s("red shoes"), Value::Int(2), Value::Float(11.5)], [s("f"), s("silk scarf"), Value::Int(6), Value::Float(60.0)]]);
    // A text field rewrites the row, keeping the fields it does not set.
    assert_eq!(sql(&db, "INSERT INTO items (id, title) VALUES ('b', 'blue rain coat') ON CONFLICT (id) DO UPDATE SET title = excluded.title, tags = ARRAY['coats', 'sale']"), 1);
    assert_eq!(rows(&db, "SELECT id, clicks, price FROM items WHERE MATCH(title, 'rain') AND tags = 'sale'"), [[s("b"), Value::Int(2), Value::Float(20.0)]]);
    // A key twice is an error, and nothing is written.
    assert!(db.sql("INSERT INTO items (id, title) VALUES ('z', 'one'), ('z', 'two') ON CONFLICT (id) DO UPDATE SET title = excluded.title", &p).is_err());
    assert!(rows(&db, "SELECT id FROM items WHERE id = 'z'").is_empty());
    for bad in [
        "INSERT INTO items (id) VALUES ('a') ON CONFLICT (id) DO UPDATE SET id = 'q'",
        "INSERT INTO items (id) VALUES ('a') ON CONFLICT (id) DO UPDATE SET clicks = 1 WHERE clicks > 0",
        "INSERT INTO items (id) VALUES ('a') ON CONFLICT (id) DO UPDATE SET clicks = excluded.title",
        "INSERT INTO items (id) VALUES ('a') ON CONFLICT (id) DO UPDATE SET clicks = excluded.nope",
        "INSERT INTO items (id) VALUES ('a') ON DUPLICATE KEY UPDATE clicks = 1",
    ] {
        assert!(db.sql(bad, &p).is_err(), "{bad}");
    }
    assert_eq!(rows(&db, "SELECT clicks FROM items WHERE id = 'a'"), [[Value::Int(2)]]);
    // A sum that overflows is an error, as in an UPDATE, and changes nothing.
    db.sql("UPDATE items SET clicks = :big WHERE id = 'c'", &Params::new().set("big", Value::Int(i64::MAX))).unwrap();
    let e = db.sql("INSERT INTO items (id) VALUES ('d'), ('c') ON CONFLICT (id) DO UPDATE SET clicks = clicks + 1", &p).unwrap_err();
    assert!(e.to_string().contains("overflows"), "{e}");
    assert_eq!(rows(&db, "SELECT id, clicks FROM items WHERE id IN ('c', 'd') ORDER BY clicks"), [[s("d"), Value::Int(4)], [s("c"), Value::Int(i64::MAX)]]);
}

/// Review bug 4: `DELETE ... LIMIT` deleted every row the WHERE matched.
#[test]
fn delete_and_update_honor_limit() {
    let (_dir, db) = seeded();
    // The most expensive two of the four over 10.
    assert_eq!(sql(&db, "DELETE FROM items WHERE price > 10 ORDER BY price DESC LIMIT 2"), 2);
    assert_eq!(rows(&db, "SELECT id FROM items ORDER BY price"), [[s("a")], [s("b")], [s("c")]]);
    assert_eq!(sql(&db, "DELETE FROM items WHERE tags = 'sale' LIMIT 0"), 0);
    assert_eq!(sql(&db, "UPDATE items SET clicks = 100 WHERE price < 25 LIMIT 1"), 1);
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM items WHERE clicks = 100"), [[Value::Int(1)]]);
    assert_eq!(sql(&db, "DELETE FROM items WHERE price < 25 ORDER BY price LIMIT 5"), 2);
    assert_eq!(rows(&db, "SELECT id FROM items"), [[s("c")]]);
    // A row without the ORDER BY value comes last, as in a SELECT.
    sql(&db, "INSERT INTO items (id, title) VALUES ('n', 'no price')");
    assert_eq!(sql(&db, "DELETE FROM items ORDER BY price DESC LIMIT 1"), 1);
    assert_eq!(rows(&db, "SELECT id FROM items"), [[s("n")]]);
}

/// Clauses the binder does not run are errors, not ignored, and write nothing.
#[test]
fn clauses_the_binder_does_not_run_are_errors() {
    let (_dir, db) = seeded();
    for (bad, why) in [
        ("DELETE FROM items WHERE price > 0 RETURNING id", "RETURNING"),
        ("DELETE FROM items USING other WHERE items.id = other.id", "USING"),
        ("UPDATE items SET clicks = 0 RETURNING id", "RETURNING"),
        ("UPDATE items SET clicks = 0 FROM other WHERE items.id = other.id", "FROM"),
        ("INSERT INTO items (id) VALUES ('q') RETURNING id", "RETURNING"),
        ("INSERT INTO items (id) SELECT id FROM items", "VALUES"),
        ("SELECT id FROM items FETCH FIRST 1 ROWS ONLY", "FETCH"),
        ("SELECT id FROM items FOR UPDATE", "FOR UPDATE"),
        ("SELECT TOP 1 id FROM items", "TOP"),
        ("SELECT id INTO items_copy FROM items", "INTO"),
    ] {
        match db.sql(bad, &Params::new()) {
            Ok(r) => panic!("{bad}: ran, {r:?}"),
            Err(e) => assert!(e.to_string().contains(why), "{bad}: {e}"),
        }
    }
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM items WHERE clicks > 0"), [[Value::Int(5)]]);
    assert!(rows(&db, "SELECT id FROM items WHERE id = 'q'").is_empty());
}

/// Review bug 7: a bare name where a value goes was read as a string.
#[test]
fn bare_names_are_columns_not_strings() {
    let (_dir, db) = seeded();
    let p = Params::new();
    let e = db.sql("SELECT id FROM items WHERE tags = sale", &p).unwrap_err().to_string();
    assert!(e.contains("no such column: sale"), "{e}");
    let e = db.sql("SELECT id FROM items WHERE id = title", &p).unwrap_err().to_string();
    assert!(e.contains("title is a column"), "{e}");
    assert!(db.sql("SELECT id FROM items WHERE id IN (a, b)", &p).is_err());
    assert!(db.sql("INSERT INTO items (id, title) VALUES (q, 'x')", &p).is_err());
    assert!(db.sql("UPDATE items SET title = blue WHERE id = 'a'", &p).is_err());
    assert!(db.sql("DELETE FROM items WHERE id = a", &p).is_err());
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM items"), [[Value::Int(5)]]);
    assert_eq!(rows(&db, "SELECT title FROM items WHERE id = 'a'"), [[s("red shoes")]]);
    // Quoted strings, single or double.
    assert_eq!(rows(&db, "SELECT id FROM items WHERE tags = 'sale' ORDER BY price"), [[s("a")], [s("c")], [s("d")]]);
    assert_eq!(rows(&db, "SELECT id FROM items WHERE tags = \"sale\" ORDER BY price"), [[s("a")], [s("c")], [s("d")]]);
}
