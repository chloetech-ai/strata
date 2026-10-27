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
