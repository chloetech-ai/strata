//! Queries people type into the SQL workbench, over one table: conditions
//! the index cannot answer (functions of a column, two columns, `LIKE
//! '%x%'`, `ILIKE`, a stored text that is not indexed) are evaluated row by
//! row and agree with the rows computed here; `ORDER BY` takes several
//! keys; `CAST` and `SELECT` of no table work. And the errors of the
//! mistakes people make say where, why and what to write instead: a
//! misspelt keyword, column, table or function, a field searched that is
//! not indexed, a parameter without a value, a key already taken.

use chloedb::db::{Db, SqlResult};
use chloedb::query::Params;
use chloedb::storage::DbOptions;
use chloedb::value::Value;
use chloedb::{Field, FieldType, Schema};

const N: usize = 240;
const BRANDS: [&str; 4] = ["Northpeak", "Riverstone", "northwind", "Alpine Co"];

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => panic!("{sql}: expected rows"),
    }
}

fn error(db: &Db, sql: &str) -> String {
    match db.sql(sql, &Params::new()) {
        Ok(_) => panic!("{sql}: expected an error"),
        Err(e) => e.to_string(),
    }
}

/// The ids a query returns, in its order.
fn ids(db: &Db, sql: &str) -> Vec<String> {
    rows(db, sql).into_iter().map(|r| r[0].as_str().unwrap_or_else(|| panic!("{sql}: an id, not {:?}", r[0])).to_string()).collect()
}

/// One product's values: (id, title, brand, price, rating, note).
struct Product {
    id: String,
    title: String,
    brand: &'static str,
    price: f64,
    rating: Option<f64>,
    year: i64,
}

fn products() -> Vec<Product> {
    let words = ["Tent", "trail shoe", "Waterproof TENT", "lamp", "stove", "backpack"];
    (0..N)
        .map(|i| Product {
            id: format!("p{i:04}"),
            title: format!("{} {}", words[i % words.len()], i % 7),
            brand: BRANDS[i % BRANDS.len()],
            price: ((i * 37) % 500) as f64 + 0.25 * (i % 4) as f64,
            rating: (i % 9 != 0).then(|| ((i * 13) % 50) as f64 / 10.0),
            year: 2018 + (i % 6) as i64,
        })
        .collect()
}

/// A table like the server's demo catalogue: a title stored but not
/// indexed, a body made from it and indexed; rows in segments and in the
/// memtable.
fn db(dir: &std::path::Path) -> (Db, Vec<Product>) {
    let brands = Schema::new("brands", vec![Field::new("name", FieldType::Keyword), Field::new("country", FieldType::Keyword)], Some("name")).unwrap();
    let opts = DbOptions { segment_max_docs: 70, memtable_chunk_docs: 64, sync: false, ..DbOptions::default() };
    let db = Db::create(&dir.join("w.strata"), brands, opts).unwrap();
    run(
        &db,
        "CREATE TABLE products (id KEYWORD PRIMARY KEY, title TEXT OPTIONS(indexed = FALSE), body TEXT OPTIONS(from = 'title'), \
         brand KEYWORD, price FLOAT, rating FLOAT, year INT)",
    );
    let ps = products();
    for (i, p) in ps.iter().enumerate() {
        let rating = p.rating.map_or("NULL".to_string(), |r| r.to_string());
        run(&db, &format!("INSERT INTO products VALUES ('{}', '{}', '{}', {}, {rating}, {})", p.id, p.title, p.brand, p.price, p.year));
        if i % 90 == 89 {
            db.storage().flush().unwrap();
        }
    }
    for b in BRANDS {
        run(&db, &format!("INSERT INTO brands VALUES ('{b}', 'NO')"));
    }
    (db, ps)
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn want(ps: &[Product], keep: impl Fn(&Product) -> bool) -> Vec<String> {
    sorted(ps.iter().filter(|p| keep(p)).map(|p| p.id.clone()).collect())
}

#[test]
fn conditions_of_rows_on_one_table() {
    let dir = tempfile::tempdir().unwrap();
    let (db, ps) = db(dir.path());
    let q = |w: &str| sorted(ids(&db, &format!("SELECT id FROM products WHERE {w}")));

    // A stored text that is not indexed, by its stored value.
    assert_eq!(q("title LIKE '%tent%'"), want(&ps, |p| p.title.contains("tent")));
    assert_eq!(q("title ILIKE '%tent%'"), want(&ps, |p| p.title.to_lowercase().contains("tent")));
    assert_eq!(q("title = 'lamp 3'"), want(&ps, |p| p.title == "lamp 3"));
    assert_eq!(q("title NOT LIKE '%e%'"), want(&ps, |p| !p.title.contains('e')));
    // A keyword by a pattern its dictionary does not run, or ignoring case.
    assert_eq!(q("brand LIKE '%peak'"), want(&ps, |p| p.brand.ends_with("peak")));
    assert_eq!(q("brand ILIKE 'north%'"), want(&ps, |p| p.brand.to_lowercase().starts_with("north")));
    assert_eq!(q("lower(brand) = 'northwind'"), want(&ps, |p| p.brand.to_lowercase() == "northwind"));
    assert_eq!(q("length(brand) > 9"), want(&ps, |p| p.brand.len() > 9));
    // Arithmetic and two columns.
    assert_eq!(q("price * 2 > 700"), want(&ps, |p| p.price * 2.0 > 700.0));
    assert_eq!(q("price < rating * 20"), want(&ps, |p| p.rating.is_some_and(|r| p.price < r * 20.0)));
    assert_eq!(q("CAST(price AS INT) = 37"), want(&ps, |p| p.price.round() == 37.0));
    // With conditions the index answers, and under OR and NOT.
    assert_eq!(q("year = 2020 AND title LIKE '%shoe%'"), want(&ps, |p| p.year == 2020 && p.title.contains("shoe")));
    assert_eq!(q("year = 2020 OR lower(title) LIKE 'lamp%'"), want(&ps, |p| p.year == 2020 || p.title.to_lowercase().starts_with("lamp")));
    assert_eq!(q("NOT (price > rating * 20)"), want(&ps, |p| p.rating.is_some_and(|r| p.price <= r * 20.0)));
    // A CASE, and the text of a search in the same WHERE.
    assert_eq!(q("CASE WHEN rating > 4 THEN 1 ELSE 0 END = 1"), want(&ps, |p| p.rating.is_some_and(|r| r > 4.0)));
    assert_eq!(q("MATCH(body, 'tent') AND title LIKE 'Tent%'"), want(&ps, |p| p.title.starts_with("Tent")));
    // With LIMIT: the first rows that pass.
    assert_eq!(ids(&db, "SELECT id FROM products WHERE title ILIKE '%tent%' ORDER BY price DESC LIMIT 3").len(), 3);
    // A count of rows that pass.
    let n = rows(&db, "SELECT COUNT(*) FROM products WHERE title ILIKE '%tent%'")[0][0].clone();
    assert_eq!(n, Value::Int(want(&ps, |p| p.title.to_lowercase().contains("tent")).len() as i64));
}

#[test]
fn order_by_several_keys() {
    let dir = tempfile::tempdir().unwrap();
    let (db, ps) = db(dir.path());
    let got = ids(&db, "SELECT id, rating, price FROM products WHERE rating IS NOT NULL ORDER BY rating DESC, price ASC LIMIT 25");
    let mut all: Vec<&Product> = ps.iter().filter(|p| p.rating.is_some()).collect();
    all.sort_by(|a, b| b.rating.partial_cmp(&a.rating).unwrap().then(a.price.partial_cmp(&b.price).unwrap()).then(a.id.cmp(&b.id)));
    let first: Vec<String> = all.iter().take(25).map(|p| p.id.clone()).collect();
    // Ties on both keys may come in any order; the keys may not.
    let key = |id: &String| ps.iter().find(|p| &p.id == id).map(|p| (p.rating.unwrap().to_bits(), p.price.to_bits())).unwrap();
    assert_eq!(got.iter().map(key).collect::<Vec<_>>(), first.iter().map(key).collect::<Vec<_>>());
    // Three keys, by name and by position.
    let three = rows(&db, "SELECT brand, year, price FROM products ORDER BY brand, 2 DESC, price LIMIT 400");
    let mut want: Vec<(String, i64, f64)> = ps.iter().map(|p| (p.brand.to_string(), p.year, p.price)).collect();
    want.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.partial_cmp(&b.2).unwrap()));
    let got: Vec<(String, i64, f64)> = three.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap(), r[2].as_f64().unwrap())).collect();
    assert_eq!(got, want);
}

#[test]
fn casts_and_values_of_no_table() {
    let dir = tempfile::tempdir().unwrap();
    let (db, ps) = db(dir.path());
    assert_eq!(rows(&db, "SELECT 1 + 1 AS two"), vec![vec![Value::Int(2)]]);
    assert_eq!(rows(&db, "SELECT 'a' || 'b', 3 * 1.5"), vec![vec![Value::Str("ab".into()), Value::Float(4.5)]]);
    assert_eq!(rows(&db, "SELECT CAST('42' AS INT), CAST(2 AS FLOAT), CAST(7 AS TEXT), CAST('yes' AS BOOL)"), vec![vec![
        Value::Int(42),
        Value::Float(2.0),
        Value::Str("7".into()),
        Value::Bool(true)
    ]]);
    assert!(matches!(rows(&db, "SELECT now()")[0][0], Value::Timestamp(_)), "now() is a time");
    // CAST over rows: INT rounds, FLOAT and TEXT keep the value.
    let got = rows(&db, "SELECT id, CAST(price AS INT), CAST(year AS FLOAT), CAST(year AS TEXT), price::INT FROM products ORDER BY id LIMIT 8");
    for (r, p) in got.iter().zip(&ps) {
        assert_eq!(r[1], Value::Int(p.price.round() as i64), "{}", p.id);
        assert_eq!(r[2], Value::Float(p.year as f64));
        assert_eq!(r[3], Value::Str(p.year.to_string()));
        assert_eq!(r[4], r[1]);
    }
    // A cast of a value is a value the index compares with.
    assert_eq!(sorted(ids(&db, "SELECT id FROM products WHERE year = CAST('2020' AS INT)")), want(&ps, |p| p.year == 2020));
    let e = error(&db, "SELECT CAST('abc' AS INT)");
    assert!(e.contains("CAST('abc' AS INT)") && e.contains("not a number"), "{e}");
    let e = error(&db, "SELECT CAST(1 AS MONEY)");
    assert!(e.contains("INT") && e.contains("FLOAT") && e.contains("TEXT"), "{e}");
    // PRF-12: the plan of a SELECT of no table says it reads one row of
    // none, rather than counting the first table's rows.
    let plan: Vec<String> = rows(&db, "EXPLAIN SELECT 1 + 1").into_iter().flatten().map(|v| v.as_str().unwrap_or_default().to_string()).collect();
    let plan = plan.join("\n");
    assert!(plan.contains(chloedb::query::compound::NO_TABLE), "{plan}");
}

/// ONB-16: SHOW TABLES answers the tables' names, one a row, and leaves
/// out the tables ChloeDB keeps for itself.
#[test]
fn show_tables_names_the_tables() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = db(dir.path());
    let names: Vec<Value> = rows(&db, "SHOW TABLES").into_iter().map(|r| r[0].clone()).collect();
    assert!(names.contains(&Value::Str("products".into())) && names.contains(&Value::Str("brands".into())), "{names:?}");
    assert!(names.iter().all(|n| !n.as_str().unwrap().starts_with('_')), "{names:?}");
    match run(&db, "show tables") {
        SqlResult::Rows(r) => assert_eq!(r.columns, vec!["name".to_string()]),
        SqlResult::Written(_) => panic!("SHOW TABLES writes nothing"),
    }
    let e = error(&db, "SHOW FULL TABLES");
    assert!(e.contains("not a statement ChloeDB runs"), "{e}");
}

#[test]
fn errors_say_where_why_and_what_to_write() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = db(dir.path());
    let has = |sql: &str, parts: &[&str]| {
        let e = error(&db, sql);
        for p in parts {
            assert!(e.contains(p), "{sql}\n  error: {e}\n  wants: {p}");
        }
        // No Rust debug text: Str("..."), Int(5), Some(..).
        for debug in ["Str(", "Int(", "Float(", "Some(", "Ident {", "Value::"] {
            assert!(!e.contains(debug), "{sql}\n  error: {e}\n  has debug text {debug}");
        }
        e
    };
    // The parser's errors name the line, the column and the token.
    has("SELECT id\nFORM products", &["line 2, column 1", "FORM", "did you mean FROM?"]);
    has("SELECT id FROM products WHERE", &["the end of the statement"]);
    // Names: the near ones, and what there is.
    has("SELECT pricee FROM products", &["no such column: pricee", "did you mean price?", "its columns are id, title"]);
    has("SELECT * FROM product", &["no such table: product", "did you mean products?", "brands"]);
    has("SELECT * FROM information_schema.tables", &["no such table", "no system tables"]);
    has("SELECT id, substr(title, 1, 3) FROM products", &["unknown function substr", "LIKE"]);
    // A field that is stored but has no words to search.
    has("SELECT id FROM products WHERE MATCH(title, 'tent')", &["title is stored but not indexed", "body"]);
    has("SELECT id FROM products ORDER BY bm25(title, 'tent') DESC", &["title is stored but not indexed", "body"]);
    // Values of the wrong kind.
    has("SELECT id FROM products WHERE id = 1", &["id is a KEYWORD", "'1'"]);
    has("SELECT id FROM products WHERE price > 'abc'", &["price is a FLOAT", "'abc'"]);
    has("SELECT brand, AVG(brand) FROM products GROUP BY brand", &["brand"]);
    // Statements Strata does not run, and what does what they do.
    has("SHOW DATABASES", &["SHOW TABLES", "/v1/tables", "products"]);
    has("SELECT *", &["no columns to give", "SELECT 1 + 1"]);
    has("SELECT * FROM products, brands WHERE products.brand = brands.name", &["JOIN", "ON products.brand = brands.name"]);
    has("SELECT * FROM products CROSS JOIN brands", &["CROSS JOIN is not supported", "JOIN ... ON"]);
    // A parameter without a value, and a key taken.
    has("SELECT id FROM products WHERE brand = :b", &["unbound parameter :b", "params"]);
    has("INSERT INTO products (id, title) VALUES ('p0001', 'x')", &["duplicate primary key", "'p0001'", "INSERT OR REPLACE"]);
}
