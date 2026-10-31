//! Columns computed over one table (`SELECT n * n, lower(k) FROM t`) are
//! the row evaluator's, typed and exact, as over derived rows; ORDER BY a
//! product of a row's own values ranks as the numbers multiply; AVG of
//! integers sums them exactly. Review bugs BUG-1, BUG-2, BUG-8 and BUG-10
//! (30 October), and the differential test the review asked for: every
//! query over `t` answers as over `(SELECT * FROM t)`.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

fn db(path: &std::path::Path) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(path, seed, DbOptions { sync: false, segment_max_docs: 16, ..DbOptions::default() }).unwrap();
    db.sql("CREATE TABLE t (id INT PRIMARY KEY, a INT, b FLOAT, c KEYWORD, tags KEYWORD[])", &Params::new()).unwrap();
    db
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

fn int(x: i64) -> Value {
    Value::Int(x)
}

fn float(x: f64) -> Value {
    Value::Float(x)
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

/// Review BUG-1 and BUG-8: a computed column over one table went through
/// the ranking evaluator, so `n * n` was 0.0 for n = -3 (a product clamped
/// its factors at 0), every result a FLOAT, integers past 2^53 rounded,
/// and `lower`, `length`, `coalesce` of a keyword and `LIKE` failed.
#[test]
fn computed_columns_are_typed_and_exact() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("t.strata"));
    let big = (1i64 << 53) + 1;
    sql(&db, &format!("INSERT INTO t (id, a, b, c) VALUES (1, -3, -2.0, 'Ab'), (2, NULL, NULL, NULL), (3, {big}, 0.5, 'b')"));
    let one = |q: &str| sql(&db, q).remove(0);
    assert_eq!(one("SELECT a * a, b * a, b * b FROM t WHERE id = 1"), vec![int(9), float(6.0), float(4.0)]);
    assert_eq!(one("SELECT a * a, a + 1, -a, b * b FROM t WHERE id = 2"), vec![Value::Null; 4]);
    assert_eq!(one("SELECT a - 1, a * 1, a FROM t WHERE id = 3"), vec![int(big - 1), int(big), int(big)]);
    // Too big for an integer: a FLOAT, as over derived rows.
    assert_eq!(one("SELECT a * a FROM t WHERE id = 3"), vec![float(big as f64 * big as f64)]);
    assert_eq!(
        one("SELECT length(c), lower(c), upper(c), coalesce(c, 'z'), c LIKE 'A%', c || '!', abs(a), ln(1 + b * b) FROM t WHERE id = 1"),
        vec![int(2), s("ab"), s("AB"), s("Ab"), Value::Bool(true), s("Ab!"), int(3), float(5f64.ln())]
    );
    assert_eq!(one("SELECT length(c), coalesce(c, 'z'), c LIKE 'A%' FROM t WHERE id = 2"), vec![Value::Null, s("z"), Value::Null]);
    // With aliases, `*`, a constant, and score() of the query's ranking.
    let rs = db.query("SELECT id, a * 2 AS twice, 7 FROM t ORDER BY id LIMIT 10", &Params::new()).unwrap();
    assert_eq!(rs.columns, vec!["id", "twice", "7"]);
    assert_eq!(rs.rows, vec![vec![int(1), int(-6), int(7)], vec![int(2), Value::Null, int(7)], vec![int(3), int(2 * big), int(7)]]);
    assert_eq!(sql(&db, "SELECT *, a + 0 FROM t WHERE id = 1"), vec![vec![int(1), int(-3), float(-2.0), s("Ab"), Value::Null, int(-3)]]);
    // Aggregates of computed values, and computed values of aggregates.
    assert_eq!(one("SELECT SUM(a * 2), COUNT(*) * 10, MAX(length(c)) FROM t"), vec![int(2 * (big - 3)), int(30), int(2)]);
    // A computed column with a ranking in the same SELECT stays the
    // ranking's (f64), and now multiplies as arithmetic.
    let rs = db.query("SELECT id, b * a, a * a FROM t ORDER BY ranked((1, 3)) DESC LIMIT 1", &Params::new()).unwrap();
    assert_eq!(rs.rows, vec![vec![int(1), float(6.0), float(9.0)]]);
}

/// Review BUG-2: ORDER BY a product of columns clamped each factor at 0, so
/// `ORDER BY b * a DESC` put the product 6 of two negatives below 4, and
/// `-(b * b)` put -4 and -1 on top at 0.
#[test]
fn ordering_by_a_product_of_columns_is_arithmetic() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("t.strata"));
    sql(&db, "INSERT INTO t (id, a, b) VALUES (1, -3, -2.0), (2, NULL, NULL), (3, 2, 3.5), (4, 4, 1.0), (5, -1, 0.5)");
    let ids = |q: &str| -> Vec<i64> { sql(&db, q).iter().map(|r| r[0].as_i64().unwrap()).collect() };
    // Products 6, NULL, 7, 4, -0.5.
    assert_eq!(ids("SELECT id FROM t ORDER BY b * a DESC LIMIT 10"), vec![3, 1, 4, 5, 2]);
    assert_eq!(ids("SELECT id FROM t ORDER BY -(b * a) DESC LIMIT 10"), vec![5, 4, 1, 3, 2]);
    assert_eq!(ids("SELECT id FROM t ORDER BY -(b * b) DESC LIMIT 10"), vec![5, 4, 1, 3, 2]);
    assert_eq!(ids("SELECT id FROM t ORDER BY a * a * a DESC LIMIT 10"), vec![4, 3, 5, 1, 2]);
    assert_eq!(ids("SELECT id FROM t ORDER BY CASE WHEN a > 0 THEN 1 ELSE a * b END DESC, id LIMIT 10"), vec![1, 3, 4, 5, 2]);
    // The value selected is the value ranked by.
    let rs = db.query("SELECT id, score() FROM t ORDER BY b * a DESC LIMIT 2", &Params::new()).unwrap();
    assert_eq!(rs.rows, vec![vec![int(3), float(7.0)], vec![int(1), float(6.0)]]);
}

/// Review BUG-10: AVG of INT summed in f64.
#[test]
fn avg_of_integers_is_exact() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("t.strata"));
    let big = (1i64 << 53) + 1;
    sql(&db, &format!("INSERT INTO t (id, a, c) VALUES (1, {big}, 'x'), (2, {big}, 'x'), (3, {}, 'x'), (4, 1, 'x'), (5, NULL, 'x')", -2 * big + 2));
    assert_eq!(sql(&db, "SELECT SUM(a), AVG(a), COUNT(a) FROM t"), vec![vec![int(3), float(0.75), int(4)]]);
    assert_eq!(sql(&db, "SELECT c, AVG(a) FROM t GROUP BY c"), vec![vec![s("x"), float(0.75)]]);
    assert_eq!(sql(&db, "SELECT AVG(a) FROM (SELECT * FROM t) d"), vec![vec![float(0.75)]]);
}

const WORDS: [&str; 3] = ["x", "y", "z"];

fn fill(db: &Db, rng: &mut StdRng, rows: usize) {
    let maybe = |rng: &mut StdRng, v: String| if rng.gen_ratio(1, 5) { "NULL".to_string() } else { v };
    let mut values = Vec::new();
    for i in 0..rows {
        let a = rng.gen_range(-5..=5).to_string();
        let a = maybe(rng, a);
        let b = ["-1.5", "0.0", "0.5", "2.0", "2.25", "-3.0"][rng.gen_range(0..6)].to_string();
        let b = maybe(rng, b);
        let c = format!("'{}'", WORDS[rng.gen_range(0..3)]);
        let c = maybe(rng, c);
        let tags: Vec<String> = WORDS.iter().filter(|_| rng.gen_ratio(1, 3)).map(|w| format!("'{w}'")).collect();
        let tags = if tags.is_empty() { "NULL".into() } else { format!("ARRAY[{}]", tags.join(", ")) };
        values.push(format!("({i}, {a}, {b}, {c}, {tags})"));
    }
    for chunk in values.chunks(20) {
        sql(db, &format!("INSERT INTO t (id, a, b, c, tags) VALUES {}", chunk.join(", ")));
    }
}

/// A random condition over the columns; with `rows`, also of a row's
/// values together (`a <= b`, `a * 2 > 3`), which a table's WHERE does not
/// take: its indexes compare a column with values.
fn cond(rng: &mut StdRng, depth: u32, rows: bool) -> String {
    if depth == 0 || rng.gen_ratio(1, 2) {
        let op = ["=", "!=", "<", "<=", ">", ">="][rng.gen_range(0..6)];
        let w = WORDS[rng.gen_range(0..3)];
        return match rng.gen_range(0..if rows { 11 } else { 9 }) {
            0 => format!("a {op} {}", rng.gen_range(-4..=4)),
            1 => format!("b {op} {}", ["0.5", "-1.5", "2.0"][rng.gen_range(0..3)]),
            2 => format!("c {} '{w}'", ["=", "!="][rng.gen_range(0..2)]),
            3 => ["a IS NULL", "b IS NOT NULL", "c IS NULL", "tags IS NULL"][rng.gen_range(0..4)].to_string(),
            4 => format!("a {}IN (1, -2, {})", ["", "NOT "][rng.gen_range(0..2)], rng.gen_range(-5..=5)),
            5 => format!("c {}IN ('{w}', 'q')", ["", "NOT "][rng.gen_range(0..2)]),
            6 => format!("a {}BETWEEN -1 AND {}", ["", "NOT "][rng.gen_range(0..2)], rng.gen_range(-1..=3)),
            7 => format!("tags {} '{w}'", ["=", "!="][rng.gen_range(0..2)]),
            8 => format!("c LIKE '{w}%'"),
            9 => format!("a {op} b"),
            _ => format!("a * 2 {op} {}", rng.gen_range(-6..=6)),
        };
    }
    match rng.gen_range(0..3) {
        0 => format!("NOT ({})", cond(rng, depth - 1, rows)),
        1 => format!("({} AND {})", cond(rng, depth - 1, rows), cond(rng, depth - 1, rows)),
        _ => format!("({} OR {})", cond(rng, depth - 1, rows), cond(rng, depth - 1, rows)),
    }
}

/// A random numeric expression over `a` and `b`.
fn num(rng: &mut StdRng, depth: u32) -> String {
    if depth == 0 || rng.gen_ratio(1, 3) {
        return match rng.gen_range(0..4) {
            0 => "a".into(),
            1 => "b".into(),
            2 => rng.gen_range(-3..=3).to_string(),
            _ => ["0.5", "-1.5", "2.0"][rng.gen_range(0..3)].into(),
        };
    }
    let x = num(rng, depth - 1);
    match rng.gen_range(0..7) {
        0 => format!("({x} + {})", num(rng, depth - 1)),
        1 => format!("({x} - {})", num(rng, depth - 1)),
        2 => format!("({x} * {})", num(rng, depth - 1)),
        3 => format!("-({x})"),
        4 => format!("abs({x})"),
        5 => format!("coalesce({x}, {})", rng.gen_range(-2..=2)),
        _ => format!("CASE WHEN {} THEN {x} ELSE {} END", cond(rng, 0, false), num(rng, depth - 1)),
    }
}

/// A random column of any type.
fn column(rng: &mut StdRng) -> String {
    match rng.gen_range(0..8) {
        0 => "lower(c)".into(),
        1 => "upper(coalesce(c, 'w'))".into(),
        2 => "length(c)".into(),
        3 => format!("c LIKE '{}%'", WORDS[rng.gen_range(0..3)]),
        4 => "c || '-'".into(),
        5 => cond(rng, 1, true),
        _ => num(rng, 3),
    }
}

/// The review's differential test: a query over the table and the same
/// query over its rows in memory answer alike, the table's WHERE through
/// its indexes and its ORDER BY through the ranking evaluator, the derived
/// rows' through the row evaluator.
#[test]
fn one_table_answers_like_its_derived_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("t.strata"));
    let mut rng = StdRng::seed_from_u64(30);
    fill(&db, &mut rng, 80);
    let derived = |q: &str| q.replacen("FROM t", "FROM (SELECT * FROM t) d", 1);
    for round in 0..400 {
        let w = cond(&mut rng, 2, false);
        let cols: Vec<String> = (0..3).map(|_| column(&mut rng)).collect();
        let q = format!("SELECT id, {} FROM t WHERE {w} ORDER BY id LIMIT 1000", cols.join(", "));
        assert_eq!(sql(&db, &q), sql(&db, &derived(&q)), "round {round}: {q}");
        // ORDER BY an expression (a table ranks by one DESC); a bare
        // number would be a position.
        let e = format!("({}) + 0", num(&mut rng, 3));
        for e in [e.clone(), format!("-({e})")] {
            let q = format!("SELECT id FROM t WHERE {w} ORDER BY {e} DESC, id LIMIT 1000");
            assert_eq!(sql(&db, &q), sql(&db, &derived(&q)), "round {round}: {q}");
        }
        // Counted and grouped.
        let q = format!("SELECT COUNT(*), SUM(a), AVG(b), MIN(c) FROM t WHERE {w}");
        assert_eq!(sql(&db, &q), sql(&db, &derived(&q)), "round {round}: {q}");
    }
}
