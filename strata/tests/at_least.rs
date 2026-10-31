//! `AT_LEAST(n, c1, ..., cy)` and `count_true(c1, ..., cy)` against row
//! by row evaluation (term weights PR C). Random conditions over rows with
//! NULLs, lists and text, nested in AND, OR, NOT and each other, keep
//! exactly the rows a hand-written three-valued evaluation keeps: in a
//! Search's filter (sets and probes), under a ranking, over rows in memory
//! (`WITH`), ordered by a column, and as `count_true` in ORDER BY and
//! SELECT.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::query::ir::eval::{holds, Ref};
use strata::query::ir::{Expr, Plan, AT_LEAST};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

const TABLE: &str = "CREATE TABLE t (id KEYWORD PRIMARY KEY, a INT, b FLOAT, c KEYWORD, tags KEYWORD[], body TEXT)";
const WORDS: [&str; 4] = ["x", "y", "z", "w"];

/// A row as the conditions read it.
#[derive(Clone, Debug)]
struct Row {
    id: String,
    a: Option<i64>,
    b: Option<f64>,
    c: Option<&'static str>,
    tags: Vec<&'static str>,
    body: Vec<&'static str>,
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

fn some<T>(rng: &mut StdRng, v: T) -> Option<T> {
    (!rng.gen_ratio(1, 5)).then_some(v)
}

fn random_row(rng: &mut StdRng, id: String) -> Row {
    let pick = |rng: &mut StdRng| WORDS[rng.gen_range(0..WORDS.len())];
    let (a, b, c) = (rng.gen_range(-5..=5), [-1.5, 0.0, 0.5, 2.0, 3.0][rng.gen_range(0..5)], pick(rng));
    Row {
        id,
        a: some(rng, a),
        b: some(rng, b),
        c: some(rng, c),
        tags: WORDS.iter().copied().filter(|_| rng.gen_ratio(1, 3)).collect(),
        body: WORDS.iter().copied().filter(|_| rng.gen_ratio(2, 5)).collect(),
    }
}

fn write(db: &Db, r: &Row) {
    let text = |v: Option<String>| v.unwrap_or_else(|| "NULL".into());
    let list = |ws: &[&str]| if ws.is_empty() { "NULL".into() } else { format!("ARRAY[{}]", ws.iter().map(|w| format!("'{w}'")).collect::<Vec<_>>().join(", ")) };
    let body = if r.body.is_empty() { "NULL".into() } else { format!("'{}'", r.body.join(" ")) };
    sql(
        db,
        &format!(
            "INSERT INTO t (id, a, b, c, tags, body) VALUES ('{}', {}, {}, {}, {}, {body}) ON CONFLICT (id) DO UPDATE SET a = excluded.a, b = excluded.b, c = excluded.c, tags = excluded.tags, body = excluded.body",
            r.id,
            text(r.a.map(|x| x.to_string())),
            text(r.b.map(|x| format!("{x:?}"))),
            text(r.c.map(|x| format!("'{x}'"))),
            list(&r.tags),
        ),
    );
}

/// A table over two segments and the memtable, with rows rewritten and
/// deleted across them; the rows it holds.
fn table(path: &std::path::Path, rng: &mut StdRng, n: usize) -> (Db, Vec<Row>) {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(path, seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    sql(&db, TABLE);
    let mut rows: Vec<Row> = (0..n).map(|i| random_row(rng, format!("r{i:04}"))).collect();
    for (i, r) in rows.iter().enumerate() {
        write(&db, r);
        if i == n / 3 || i == 2 * n / 3 {
            db.storage().flush().unwrap();
        }
    }
    for i in (0..n).step_by(7) {
        rows[i] = random_row(rng, rows[i].id.clone());
        write(&db, &rows[i]);
    }
    let gone: Vec<String> = rows.iter().step_by(11).map(|r| r.id.clone()).collect();
    for id in &gone {
        sql(&db, &format!("DELETE FROM t WHERE id = '{id}'"));
    }
    rows.retain(|r| !gone.contains(&r.id));
    (db, rows)
}

/// A condition: its SQL, and its truth for a row (`None` for UNKNOWN).
struct Cond {
    sql: String,
    /// Whether it holds a MATCH, which only an index answers.
    text: bool,
    truth: Box<dyn Fn(&Row) -> Option<bool>>,
}

fn cmp<T: PartialOrd>(op: &str, x: T, y: T) -> bool {
    match op {
        "=" => x == y,
        "!=" => x != y,
        "<" => x < y,
        "<=" => x <= y,
        ">" => x > y,
        _ => x >= y,
    }
}

fn leaf(rng: &mut StdRng) -> Cond {
    let op = ["=", "!=", "<", "<=", ">", ">="][rng.gen_range(0..6)];
    let w = WORDS[rng.gen_range(0..WORDS.len())];
    let plain = |sql: String, truth: Box<dyn Fn(&Row) -> Option<bool>>| Cond { sql, text: false, truth };
    match rng.gen_range(0..8) {
        0 => {
            let v = rng.gen_range(-6..=6);
            plain(format!("a {op} {v}"), Box::new(move |r| r.a.map(|a| cmp(op, a, v))))
        }
        1 => {
            let v = [-2.0, 0.0, 0.5, 2.0, 2.5][rng.gen_range(0..5)];
            plain(format!("b {op} {v:?}"), Box::new(move |r| r.b.map(|b| cmp(op, b, v))))
        }
        2 => match rng.gen_bool(0.5) {
            true => plain(format!("c = '{w}'"), Box::new(move |r| r.c.map(|c| c == w))),
            false => plain(format!("c != '{w}'"), Box::new(move |r| r.c.map(|c| c != w))),
        },
        // A multi-valued field holds when a value matches, and is never
        // UNKNOWN.
        3 => plain(format!("tags = '{w}'"), Box::new(move |r| Some(r.tags.contains(&w)))),
        4 => {
            let (lo, hi) = (rng.gen_range(-5..=2), rng.gen_range(0..=4));
            plain(format!("a BETWEEN {lo} AND {}", lo + hi), Box::new(move |r| r.a.map(|a| lo <= a && a <= lo + hi)))
        }
        5 => match rng.gen_bool(0.5) {
            true => plain("a IS NULL".into(), Box::new(|r| Some(r.a.is_none()))),
            false => plain("c IS NOT NULL".into(), Box::new(|r| Some(r.c.is_some()))),
        },
        6 => plain(format!("c IN ('{w}', 'z')"), Box::new(move |r| r.c.map(|c| c == w || c == "z"))),
        _ => Cond { sql: format!("MATCH(body, '{w}')"), text: true, truth: Box::new(move |r| Some(r.body.contains(&w))) },
    }
}

/// `n` (a count or a percentage) of `y` items, as SQL reads it.
fn need(rng: &mut StdRng, y: usize) -> (String, usize) {
    match rng.gen_range(0..3) {
        0 => {
            let p = [1, 25, 50, 75, 100][rng.gen_range(0..5)];
            (format!("'{p}%'"), (y * p / 100).clamp(1, y))
        }
        _ => {
            let n = rng.gen_range(1..=y + 1);
            (n.to_string(), n.clamp(1, y))
        }
    }
}

fn at_least(n: usize, truths: impl Iterator<Item = Option<bool>>) -> Option<bool> {
    let (mut yes, mut maybe) = (0, 0);
    for t in truths {
        match t {
            Some(true) => yes += 1,
            None => maybe += 1,
            Some(false) => {}
        }
    }
    if yes >= n {
        Some(true)
    } else if yes + maybe >= n {
        None
    } else {
        Some(false)
    }
}

/// A random condition with AT_LEAST somewhere in it once `depth` allows.
fn cond(rng: &mut StdRng, depth: u32) -> Cond {
    let pick = if depth == 0 { 0 } else { rng.gen_range(0..7) };
    match pick {
        0 | 1 => leaf(rng),
        2 | 3 => {
            let y = rng.gen_range(1..=5);
            let items: Vec<Cond> = (0..y).map(|_| cond(rng, depth - 1)).collect();
            let (n_sql, n) = need(rng, y);
            let sql = format!("AT_LEAST({n_sql}, {})", items.iter().map(|c| c.sql.as_str()).collect::<Vec<_>>().join(", "));
            let text = items.iter().any(|c| c.text);
            Cond { sql, text, truth: Box::new(move |r| at_least(n, items.iter().map(|c| (c.truth)(r)))) }
        }
        4 => {
            let x = cond(rng, depth - 1);
            Cond { sql: format!("NOT ({})", x.sql), text: x.text, truth: Box::new(move |r| (x.truth)(r).map(|b| !b)) }
        }
        _ => {
            let (l, r) = (cond(rng, depth - 1), cond(rng, depth - 1));
            let and = rng.gen_bool(0.5);
            let sql = format!("({} {} {})", l.sql, if and { "AND" } else { "OR" }, r.sql);
            let text = l.text || r.text;
            Cond {
                sql,
                text,
                truth: Box::new(move |row| {
                    let (a, b) = ((l.truth)(row), (r.truth)(row));
                    match and {
                        true if a == Some(false) || b == Some(false) => Some(false),
                        true if a == Some(true) && b == Some(true) => Some(true),
                        false if a == Some(true) || b == Some(true) => Some(true),
                        false if a == Some(false) && b == Some(false) => Some(false),
                        _ => None,
                    }
                }),
            }
        }
    }
}

fn ids(rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut v: Vec<String> = rows.into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

fn passing(rows: &[Row], c: &Cond) -> Vec<String> {
    rows.iter().filter(|r| (c.truth)(r) == Some(true)).map(|r| r.id.clone()).collect()
}

/// Each row's values as the plan evaluator reads them.
fn values(r: &Row) -> Vec<Value> {
    let list = |ws: &[&str]| Value::List(ws.iter().map(|w| Value::Str(w.to_string())).collect());
    vec![
        Value::Str(r.id.clone()),
        r.a.map_or(Value::Null, Value::Int),
        r.b.map_or(Value::Null, Value::Float),
        r.c.map_or(Value::Null, |c| Value::Str(c.into())),
        list(&r.tags),
    ]
}

#[test]
fn at_least_keeps_the_rows_a_row_by_row_count_keeps() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(31);
    let (db, rows) = table(&dir.path().join("t.strata"), &mut rng, 900);
    let (mut some, mut none, mut negated) = (0, 0, 0);
    for i in 0..1200 {
        let c = cond(&mut rng, 3);
        if !c.sql.contains("AT_LEAST") {
            continue;
        }
        negated += c.sql.contains("NOT (AT_LEAST") as usize;
        let want = passing(&rows, &c);
        let q = format!("SELECT id FROM t WHERE {}", c.sql);
        assert_eq!(ids(sql(&db, &q)), want, "case {i}: {q}");
        // Under a ranking, whose filter is probed or materialised.
        let ranked: Vec<String> = want.iter().filter(|id| rows.iter().any(|r| &r.id == *id && (r.body.contains(&"x") || r.body.contains(&"y")))).cloned().collect();
        let q = format!("SELECT id FROM t WHERE {} ORDER BY bm25(body, 'x y') DESC LIMIT 2000", c.sql);
        assert_eq!(ids(sql(&db, &q)), ranked, "case {i}: {q}");
        // Ordered by a column, whose blocks are read best first and
        // skipped when too few of the conditions can hold in them.
        let mut by_a: Vec<&Row> = rows.iter().filter(|r| r.a.is_some() && want.contains(&r.id)).collect();
        by_a.sort_by(|x, y| y.a.cmp(&x.a).then_with(|| x.id.cmp(&y.id)));
        let top: Vec<String> = by_a.iter().take(25).map(|r| r.id.clone()).collect();
        let q = format!("SELECT id FROM t WHERE ({}) AND a IS NOT NULL ORDER BY a DESC, id LIMIT 25", c.sql);
        let got: Vec<String> = sql(&db, &q).into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
        assert_eq!(got, top, "case {i}: {q}");
        if !c.text {
            // The plan the binder made, evaluated row by row.
            let q = format!("SELECT id FROM t WHERE {}", c.sql);
            let plan = db.explain(&q, &Params::new()).unwrap();
            assert!(plan.features.iter().any(|f| f == AT_LEAST) || !plan.to_json().unwrap().to_string().contains("at_least"), "{q}");
            let filter: Expr = match plan.to_json().unwrap()["root"]["input"].get("filter") {
                Some(f) => serde_json::from_value(f.clone()).unwrap(),
                None => Expr::Lit(Value::Bool(true)),
            };
            let got: Vec<String> = rows
                .iter()
                .filter(|r| {
                    let v = values(r);
                    holds(&filter, &|x| match x {
                        Ref::Field(f) => Ok(v[f as usize].clone()),
                        x => panic!("{x:?}"),
                    })
                    .unwrap()
                })
                .map(|r| r.id.clone())
                .collect();
            assert_eq!(got, want, "case {i}, the bound plan: {q}");
            // Over rows in memory, as a WITH result filters them.
            let q = format!("WITH r AS (SELECT id, a, b, c, tags FROM t) SELECT id FROM r WHERE {}", c.sql);
            assert_eq!(ids(sql(&db, &q)), want, "case {i}: {q}");
        }
        match want.len() {
            0 => none += 1,
            n if n < rows.len() => some += 1,
            _ => {}
        }
    }
    assert!(some > 300 && none > 20 && negated > 20, "{some} with some rows, {none} with none, {negated} negated");
}

#[test]
fn count_true_counts_what_where_would_keep() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(32);
    let (db, rows) = table(&dir.path().join("t.strata"), &mut rng, 600);
    let bm25: std::collections::HashMap<String, f64> =
        sql(&db, "SELECT id, score() FROM t ORDER BY bm25(body, 'z') DESC LIMIT 2000").into_iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect();
    for i in 0..150 {
        let cs: Vec<Cond> = (0..rng.gen_range(1..=6)).map(|_| cond(&mut rng, 1)).collect();
        let list = cs.iter().map(|c| c.sql.as_str()).collect::<Vec<_>>().join(", ");
        let count = |r: &Row| cs.iter().filter(|c| (c.truth)(r) == Some(true)).count() as f64;
        let mut want: Vec<(f64, String)> = rows.iter().map(|r| (count(r), r.id.clone())).collect();
        want.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        want.truncate(200);
        let q = format!("SELECT id, count_true({list}) FROM t ORDER BY count_true({list}) DESC, id LIMIT 200");
        let got: Vec<(f64, String)> = sql(&db, &q).into_iter().map(|r| (r[1].as_f64().unwrap(), r[0].as_str().unwrap().to_string())).collect();
        assert_eq!(got, want, "case {i}: {q}");
        // In a boost of relevance, and over rows in memory.
        let q = format!("SELECT id, score() FROM t ORDER BY bm25(body, 'z') * (1 + count_true({list})) DESC, id LIMIT 2000");
        let boosted = sql(&db, &q);
        assert!(!boosted.is_empty(), "{q}");
        for r in boosted {
            let row = rows.iter().find(|x| x.id == r[0].as_str().unwrap()).unwrap();
            let bm = bm25[&row.id];
            assert!((r[1].as_f64().unwrap() - bm * (1.0 + count(row))).abs() < 1e-6 * bm.max(1.0), "case {i}: {q}");
        }
        if cs.iter().all(|c| !c.text) {
            let q = format!("WITH r AS (SELECT id, a, b, c, tags FROM t) SELECT id, count_true({list}) AS k FROM r");
            let mut got: Vec<(f64, String)> = sql(&db, &q).into_iter().map(|r| (r[1].as_f64().unwrap(), r[0].as_str().unwrap().to_string())).collect();
            got.sort_by(|a, b| a.1.cmp(&b.1));
            let mut all: Vec<(f64, String)> = rows.iter().map(|r| (count(r), r.id.clone())).collect();
            all.sort_by(|a, b| a.1.cmp(&b.1));
            assert_eq!(got, all, "case {i}: {q}");
        }
    }
}

#[test]
fn at_least_reads_counts_and_shares_and_refuses_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(33);
    let (db, rows) = table(&dir.path().join("t.strata"), &mut rng, 200);
    let three = "a > 0, c = 'x', tags = 'y'";
    let count = |r: &Row| [r.a.is_some_and(|a| a > 0), r.c == Some("x"), r.tags.contains(&"y")].iter().filter(|b| **b).count();
    for (n, want) in [("1", 1), ("2", 2), ("3", 3), ("9", 3), ("'50%'", 1), ("'67%'", 2), ("'100%'", 3), ("' 34% '", 1), (":n", 2)] {
        let expect: Vec<String> = rows.iter().filter(|r| count(r) >= want).map(|r| r.id.clone()).collect();
        let q = format!("SELECT id FROM t WHERE AT_LEAST({n}, {three})");
        let got = match db.sql(&q, &Params::new().set("n", Value::Int(2))).unwrap() {
            SqlResult::Rows(r) => ids(r.rows),
            _ => unreachable!(),
        };
        assert_eq!(got, expect, "{q}");
    }
    // A subquery condition counts as the condition it stands for.
    let q = "SELECT id FROM t WHERE AT_LEAST(2, id IN (SELECT id FROM t WHERE a > 0), c = 'x', tags = 'y')";
    let got = match db.sql(q, &Params::new()).unwrap() {
        SqlResult::Rows(r) => ids(r.rows),
        _ => unreachable!(),
    };
    assert_eq!(got, rows.iter().filter(|r| count(r) >= 2).map(|r| r.id.clone()).collect::<Vec<_>>(), "{q}");
    for (q, msg) in [
        ("SELECT id FROM t WHERE AT_LEAST(2)", "AT_LEAST(n, condition, ...)"),
        ("SELECT id FROM t WHERE AT_LEAST(-1, a > 0, c = 'x')", "a count (2) or a percentage ('75%')"),
        ("SELECT id FROM t WHERE AT_LEAST(0, a > 0, c = 'x')", "above 0"),
        ("SELECT id FROM t WHERE AT_LEAST('0%', a > 0, c = 'x')", "above 0"),
        ("SELECT id FROM t WHERE AT_LEAST(1.5, a > 0, c = 'x')", "a count (2) or a percentage ('75%')"),
        ("SELECT id FROM t WHERE AT_LEAST('120%', a > 0, c = 'x')", "a count (2) or a percentage ('75%')"),
        ("SELECT id FROM t WHERE AT_LEAST(2, a > 0, c = 'x', limit => 10)", "unknown option limit for AT_LEAST: min_rows, max_rows"),
        ("SELECT id FROM t ORDER BY count_true() DESC", "count_true(condition, ...)"),
    ] {
        let e = db.sql(q, &Params::new()).err().unwrap_or_else(|| panic!("{q} ran")).to_string();
        assert!(e.contains(msg), "{q}: {e}");
    }
    // The plan says it counts, which a Strata from before AT_LEAST refuses.
    for q in [format!("SELECT id FROM t WHERE AT_LEAST(2, {three})"), format!("SELECT id FROM t ORDER BY count_true({three}) DESC")] {
        let plan = db.explain(&q, &Params::new()).unwrap();
        assert!(plan.features.iter().any(|f| f == AT_LEAST), "{q}: {:?}", plan.features);
        let back = Plan::from_json(&plan.to_json().unwrap()).unwrap();
        assert_eq!(back, plan, "{q}");
    }
    let plan = db.explain("SELECT id FROM t WHERE a > 0 OR c = 'x'", &Params::new()).unwrap();
    assert!(!plan.features.iter().any(|f| f == AT_LEAST));
    let e = Plan::readable_by(1, &[AT_LEAST], &["operators"]).unwrap_err().to_string();
    assert!(e.contains("at_least"), "{e}");
    // EXPLAIN prints it as written, with NOT pushed into the count.
    let schema = db.snapshot().table("t").unwrap().schema().clone();
    let text = db.explain("SELECT id FROM t WHERE NOT AT_LEAST(2, a > 0, c = 'x', tags = 'y')", &Params::new()).unwrap().explain(&[&schema]).join("\n");
    assert!(text.contains("AT_LEAST(2, "), "{text}");
}
