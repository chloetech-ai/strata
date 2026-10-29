//! `SET` takes an expression of the row (UPDATE, and ON CONFLICT DO UPDATE
//! with `excluded.f`), computed for each row by the plan IR's evaluator;
//! and UPDATE and DELETE bind their own clauses instead of printing them
//! back into SQL, which read `- -1` as a comment and a column named
//! `order` as a keyword.

use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

fn db(path: &std::path::Path, create: &str) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(path, seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.sql(create, &Params::new()).unwrap();
    db
}

fn items(path: &std::path::Path) -> Db {
    let db = db(path, "CREATE TABLE items (id KEYWORD PRIMARY KEY, title TEXT, label KEYWORD, tags KEYWORD[], clicks INT, price FLOAT, done BOOL, due TIMESTAMP, emb VECTOR(2) OPTIONS(index = 'flat'))");
    write(&db, "INSERT INTO items (id, title, label, tags, clicks, price, done, due, emb) VALUES
        ('a', 'Red Shoes', 'Sale', ARRAY['x'], 3, 10.5, TRUE, '2026-10-01T00:00:00Z', ARRAY[1.0, 0.0]),
        ('b', 'Blue Hat', NULL, NULL, NULL, 2.0, FALSE, NULL, ARRAY[0.0, 1.0]),
        ('c', NULL, 'new', ARRAY['y', 'z'], 7, NULL, NULL, '2026-10-05T00:00:00Z', NULL)");
    db
}

fn write(db: &Db, sql: &str) -> u64 {
    match db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlResult::Written(n) => n,
        SqlResult::Rows(_) => panic!("{sql}: rows"),
    }
}

fn fails(db: &Db, sql: &str) -> String {
    db.sql(sql, &Params::new()).err().unwrap_or_else(|| panic!("{sql} ran")).to_string()
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => panic!("{sql}: a write"),
    }
}

/// One column of the rows, by id.
fn col(db: &Db, c: &str) -> Vec<Value> {
    let mut r = rows(db, &format!("SELECT id, {c} FROM items"));
    r.sort_by_key(|r| r[0].as_str().unwrap().to_string());
    r.into_iter().map(|mut r| r.remove(1)).collect()
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn list(xs: &[&str]) -> Value {
    Value::List(xs.iter().map(|x| s(x)).collect())
}

const N: Value = Value::Null;

#[test]
fn set_computes_from_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = items(&dir.path().join("d.strata"));
    // Numbers change in place: the vectors stay.
    assert_eq!(write(&db, "UPDATE items SET price = price * 2 WHERE id IN ('a', 'b')"), 2);
    assert_eq!(col(&db, "price"), [Value::Float(21.0), Value::Float(4.0), N]);
    assert_eq!(col(&db, "emb")[0], Value::Vector(vec![1.0, 0.0].into()));
    // Text rewrites the row, and the index follows.
    assert_eq!(write(&db, "UPDATE items SET title = upper(title) || '!' WHERE id = 'a'"), 1);
    assert_eq!(col(&db, "title"), [s("RED SHOES!"), s("Blue Hat"), N]);
    assert_eq!(rows(&db, "SELECT id FROM items WHERE MATCH(title, 'shoes')"), [[s("a")]]);
    // NOT of NULL is NULL.
    assert_eq!(write(&db, "UPDATE items SET done = NOT done"), 3);
    assert_eq!(col(&db, "done"), [Value::Bool(false), Value::Bool(true), N]);
    // CASE: a condition that is UNKNOWN takes the ELSE.
    write(&db, "UPDATE items SET clicks = CASE WHEN price > 10 THEN clicks * 10 ELSE length(label) END");
    assert_eq!(col(&db, "clicks"), [Value::Int(30), N, Value::Int(3)]);
    // Columns of the table, by its name or alias.
    write(&db, "UPDATE items AS i SET clicks = i.clicks + 1, price = items.price - 1 WHERE id = 'a'");
    assert_eq!((col(&db, "clicks")[0].clone(), col(&db, "price")[0].clone()), (Value::Int(31), Value::Float(20.0)));
    write(&db, "UPDATE items i SET clicks = i.clicks - 1, price = i.price * 1 + 1 WHERE id = 'a'");
    assert!(fails(&db, "UPDATE items AS i SET clicks = x.clicks * 2").contains("no such column: x.clicks"));
    // Every right-hand side reads the row as it was.
    write(&db, "UPDATE items SET label = id, tags = label WHERE id = 'a'");
    assert_eq!(col(&db, "label")[0], s("a"));
    assert_eq!(col(&db, "tags")[0], list(&["Sale"]));
    // One value for a multi-valued field is a list of it; a list stays one.
    // A field without values is NULL to coalesce, as to IS NULL.
    write(&db, "UPDATE items SET tags = coalesce(tags, label, 'none')");
    assert_eq!(col(&db, "tags"), [list(&["Sale"]), list(&["none"]), list(&["y", "z"])]);
    // A time from text or from another time; a whole FLOAT fits an INT.
    write(&db, "UPDATE items SET due = coalesce(due, '2026-12-24T00:00:00Z'), clicks = price * 2 WHERE id = 'b'");
    assert_eq!(col(&db, "due")[1], Value::Timestamp(strata::timestamp::parse("2026-12-24T00:00:00Z").unwrap()));
    assert_eq!(col(&db, "clicks")[1], Value::Int(8));
    // ON CONFLICT DO UPDATE reads the proposed row as excluded.
    write(&db, "INSERT INTO items (id, title, clicks) VALUES ('a', 'Shoes', 5), ('d', 'Socks', 1) ON CONFLICT (id) DO UPDATE SET clicks = clicks + excluded.clicks, title = excluded.title || ' (again)'");
    assert_eq!(col(&db, "clicks"), [Value::Int(35), Value::Int(8), Value::Int(3), Value::Int(1)]);
    assert_eq!(col(&db, "title"), [s("Shoes (again)"), s("Blue Hat"), N, s("Socks")]);
    // `excluded.f + 1` adds to the proposed value: it was read as `f + 1`.
    write(&db, "INSERT INTO items (id, clicks) VALUES ('c', 100) ON CONFLICT (id) DO UPDATE SET clicks = excluded.clicks + 1");
    assert_eq!(col(&db, "clicks")[2], Value::Int(101));
}

#[test]
fn nulls_follow_sql_but_a_counter_starts_at_zero() {
    let dir = tempfile::tempdir().unwrap();
    let db = items(&dir.path().join("d.strata"));
    // `f = f + number` counts: NULL is zero.
    write(&db, "UPDATE items SET clicks = clicks + 1 WHERE id = 'b'");
    assert_eq!(col(&db, "clicks")[1], Value::Int(1));
    // Any other expression gives NULL for NULL, as in SQL.
    write(&db, "UPDATE items SET clicks = NULL WHERE id = 'b'");
    write(&db, "UPDATE items SET clicks = clicks * 2, price = price + clicks");
    assert_eq!(col(&db, "clicks"), [Value::Int(6), N, Value::Int(14)]);
    assert_eq!(col(&db, "price"), [Value::Float(13.5), N, N]);
    write(&db, "UPDATE items SET clicks = coalesce(clicks, 0) + 5 WHERE id = 'b'");
    assert_eq!(col(&db, "clicks")[1], Value::Int(5));
}

#[test]
fn set_refuses_what_a_row_cannot_give() {
    let dir = tempfile::tempdir().unwrap();
    let db = items(&dir.path().join("d.strata"));
    for (sql, want) in [
        ("UPDATE items SET clicks = nope * 2", "no such column: nope"),
        ("UPDATE items SET clicks = excluded.clicks + 1", "no such column: excluded.clicks"),
        ("UPDATE items SET clicks = bm25(title, 'red') + 1", "BM25(...) works on one table's scan"),
        ("UPDATE items SET clicks = score()", "SCORE(...) works on one table's scan"),
        ("UPDATE items SET clicks = count(*)", "COUNT(...) is an aggregate"),
        ("UPDATE items SET clicks = price / 4 WHERE id = 'a'", "clicks is INT, and 2.625 is not an integer"),
        ("UPDATE items SET clicks = title || '' WHERE id = 'a'", "does not fit"),
        ("UPDATE items SET clicks = title + 1 WHERE id = 'a'", "cannot compute"),
        ("UPDATE items SET emb = title || '' WHERE id = 'a'", "emb is a VECTOR field"),
        ("UPDATE items SET label = tags WHERE id = 'c'", "label holds one value"),
        ("UPDATE items SET due = clicks WHERE id = 'a'", "due is a TIMESTAMP"),
        // Unchanged: `f + number` of a field that is no number, and a
        // proposed field of another type.
        ("UPDATE items SET title = title + 1", "not numeric"),
        ("UPDATE items SET done = done + 1", "not numeric"),
        ("INSERT INTO items (id, clicks) VALUES ('a', 1) ON CONFLICT (id) DO UPDATE SET label = excluded.clicks", "different types"),
    ] {
        let e = fails(&db, sql);
        assert!(e.contains(want), "{sql}: {e}");
    }
    // A statement that fails on one row writes none: a's clicks would be
    // 1, c's 7 / 3.
    let e = fails(&db, "UPDATE items SET clicks = clicks / 3");
    assert!(e.contains("2.33") && e.contains("is not an integer"), "{e}");
    assert_eq!(col(&db, "clicks"), [Value::Int(3), N, Value::Int(7)]);
}

#[test]
fn set_reads_generated_fields() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("d.strata"), "CREATE TABLE notes (id KEYWORD PRIMARY KEY, title TEXT, body TEXT, both_ TEXT OPTIONS(from = 'title, body', stored = false), n INT)");
    write(&db, "INSERT INTO notes (id, title, body) VALUES ('a', 'ab', 'cde')");
    // Not stored, so a query reads no value; SET computes it.
    assert_eq!(rows(&db, "SELECT both_ FROM notes"), [[N]]);
    write(&db, "UPDATE notes SET n = length(both_)");
    assert_eq!(rows(&db, "SELECT n FROM notes"), [[Value::Int(6)]]);
}

#[test]
fn writes_bind_their_own_clauses() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("d.strata"), "CREATE TABLE t (id KEYWORD PRIMARY KEY, \"order\" INT, \"limit\" KEYWORD, n INT)");
    write(&db, "INSERT INTO t (id, \"order\", n) VALUES ('a', 1, 1), ('b', 2, 2), ('c', 3, 3), ('d', 4, 4)");
    let ns = || {
        let mut r = rows(&db, "SELECT id, \"order\", n FROM t");
        r.sort_by_key(|r| r[0].as_str().unwrap().to_string());
        r
    };
    // Columns named like keywords: printed into SQL, `SELECT id, order`
    // did not parse, so no UPDATE, DELETE or ON CONFLICT DO UPDATE ran;
    // nor did `SET "order" = ...`, which looked for a column `"order"`.
    assert_eq!(write(&db, "UPDATE t SET n = n + 10 WHERE \"order\" >= 3"), 2);
    assert_eq!(write(&db, "UPDATE t SET \"order\" = \"order\" * 10, \"limit\" = 'x' WHERE id = 'a'"), 1);
    assert_eq!(write(&db, "INSERT INTO t (id, \"order\") VALUES ('b', 7) ON CONFLICT (id) DO UPDATE SET \"order\" = excluded.\"order\""), 1);
    assert_eq!(ns(), [[s("a"), Value::Int(10), Value::Int(1)], [s("b"), Value::Int(7), Value::Int(2)], [s("c"), Value::Int(3), Value::Int(13)], [s("d"), Value::Int(4), Value::Int(14)]]);
    // `- -1` printed is `--1`, which starts a comment.
    assert_eq!(write(&db, "UPDATE t SET n = 0 WHERE n > - -13"), 1);
    assert_eq!(write(&db, "DELETE FROM t WHERE n < - -2 ORDER BY \"order\" DESC LIMIT 1"), 1);
    // d (n = 0) and a (n = 1) match; a has the larger order.
    assert_eq!(ns(), [[s("b"), Value::Int(7), Value::Int(2)], [s("c"), Value::Int(3), Value::Int(13)], [s("d"), Value::Int(4), Value::Int(0)]]);
}
