//! The one evaluator of plan expressions (`strata::query::ir::eval`) and
//! the indexes agree: for random conditions over rows with NULLs and
//! lists, a query returns exactly the rows for which its condition,
//! evaluated row by row in SQL's three-valued logic, is TRUE. Joins and
//! `WITH` results, which filter rows in memory with that evaluator, keep
//! the same rows. And HAVING, which runs in it too, keeps only the groups
//! for which it is TRUE.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::query::ir::eval::{holds, Ref};
use strata::query::ir::{CmpOp, Expr};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

const TABLE: &str = "CREATE TABLE t (id KEYWORD PRIMARY KEY, a INT, b FLOAT, c KEYWORD, tags KEYWORD[], n INT[], flag BOOL, ts TIMESTAMP)";
/// The fields by id, as the table declares them.
const A: u32 = 1;
const B: u32 = 2;
const C: u32 = 3;
const TAGS: u32 = 4;
const N: u32 = 5;
const FLAG: u32 = 6;
const TS: u32 = 7;
const NAMES: [&str; 8] = ["id", "a", "b", "c", "tags", "n", "flag", "ts"];
const MULTI: [u32; 2] = [TAGS, N];

fn db(path: &std::path::Path) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(path, seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.sql(TABLE, &Params::new()).unwrap();
    db
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

/// `v`, or NULL when `null`.
fn maybe(null: bool, v: String) -> String {
    if null { "NULL".into() } else { v }
}

fn fill(db: &Db, rng: &mut StdRng, rows: usize) {
    const WORDS: [&str; 3] = ["x", "y", "z"];
    for i in 0..rows {
        let a = maybe(rng.gen_ratio(1, 5), rng.gen_range(-5..=5).to_string());
        let b = maybe(rng.gen_ratio(1, 5), ["-1.5", "0.0", "0.5", "2.0", "2.25", "3.0"][rng.gen_range(0..6)].to_string());
        let c = maybe(rng.gen_ratio(1, 5), format!("'{}'", WORDS[rng.gen_range(0..3)]));
        let tags: Vec<String> = WORDS.iter().filter(|_| rng.gen_ratio(1, 3)).map(|w| format!("'{w}'")).collect();
        let tags = if tags.is_empty() { "NULL".into() } else { format!("ARRAY[{}]", tags.join(", ")) };
        let n: Vec<String> = (0..rng.gen_range(0..3)).map(|_| rng.gen_range(-3..=3).to_string()).collect();
        let n = if n.is_empty() { "NULL".into() } else { format!("ARRAY[{}]", n.join(", ")) };
        let flag = maybe(rng.gen_ratio(1, 5), ["TRUE", "FALSE"][rng.gen_range(0..2)].to_string());
        let ts = maybe(rng.gen_ratio(1, 5), format!("'2026-10-0{}'", rng.gen_range(1..=5)));
        sql(db, &format!("INSERT INTO t (id, a, b, c, tags, n, flag, ts) VALUES ('r{i}', {a}, {b}, {c}, {tags}, {n}, {flag}, {ts})"));
        // Half the rows in a segment, half in the memtable.
        if i == rows / 2 {
            db.storage().flush().unwrap();
        }
    }
}

fn field(f: u32) -> Box<Expr> {
    Box::new(Expr::Field(f))
}

fn lit(v: Value) -> Box<Expr> {
    Box::new(Expr::Lit(v))
}

const OPS: [(&str, CmpOp); 6] = [("=", CmpOp::Eq), ("!=", CmpOp::Ne), ("<", CmpOp::Lt), ("<=", CmpOp::Le), (">", CmpOp::Gt), (">=", CmpOp::Ge)];

/// A random condition, as SQL and as the expression it means.
fn cond(rng: &mut StdRng, depth: u32) -> (String, Expr) {
    let word = |rng: &mut StdRng| ["x", "y", "z", "w"][rng.gen_range(0..4)];
    let leaves = 11;
    let pick = if depth == 0 { rng.gen_range(0..leaves) } else { rng.gen_range(0..leaves + 3) };
    let (name, op) = OPS[rng.gen_range(0..6)];
    match pick {
        0 => {
            let v = rng.gen_range(-6..=6);
            (format!("a {name} {v}"), Expr::Cmp { op, left: field(A), right: lit(Value::Int(v)) })
        }
        1 => {
            let v = [-2.0, 0.0, 0.5, 1.75, 2.0, 3.0][rng.gen_range(0..6)];
            (format!("b {name} {v:?}"), Expr::Cmp { op, left: field(B), right: lit(Value::Float(v)) })
        }
        2 => {
            let (name, op) = [("=", CmpOp::Eq), ("!=", CmpOp::Ne)][rng.gen_range(0..2)];
            let (f, col) = [(C, "c"), (TAGS, "tags")][rng.gen_range(0..2)];
            let w = word(rng);
            (format!("{col} {name} '{w}'"), Expr::Cmp { op, left: field(f), right: lit(Value::Str(w.into())) })
        }
        3 => {
            let (f, col) = [(C, "c"), (TAGS, "tags")][rng.gen_range(0..2)];
            let ws: Vec<&str> = (0..rng.gen_range(1..=2)).map(|_| word(rng)).collect();
            let list = ws.iter().map(|w| format!("'{w}'")).collect::<Vec<_>>().join(", ");
            (format!("{col} IN ({list})"), Expr::In { expr: field(f), list: ws.iter().map(|w| Expr::Lit(Value::Str(w.to_string()))).collect() })
        }
        4 => {
            let v = rng.gen_range(-3..=3);
            (format!("n {name} {v}"), Expr::Cmp { op, left: field(N), right: lit(Value::Int(v)) })
        }
        5 => match rng.gen_range(0..3) {
            0 => ("flag".into(), Expr::Field(FLAG)),
            1 => ("flag = TRUE".into(), Expr::Cmp { op: CmpOp::Eq, left: field(FLAG), right: lit(Value::Bool(true)) }),
            _ => ("flag = FALSE".into(), Expr::Cmp { op: CmpOp::Eq, left: field(FLAG), right: lit(Value::Bool(false)) }),
        },
        6 => {
            let d = format!("2026-10-0{}", rng.gen_range(1..=5));
            (format!("ts {name} '{d}'"), Expr::Cmp { op, left: field(TS), right: lit(Value::Str(d)) })
        }
        7 | 8 => {
            let f = rng.gen_range(1..8u32);
            match pick == 7 {
                true => (format!("{} IS NULL", NAMES[f as usize]), Expr::IsNull(field(f))),
                false => (format!("{} IS NOT NULL", NAMES[f as usize]), Expr::Not(Box::new(Expr::IsNull(field(f))))),
            }
        }
        9 => {
            let lo = rng.gen_range(-5..=3);
            let hi = lo + rng.gen_range(0..=4);
            let e = Expr::Between { expr: field(A), lo: Some(lit(Value::Int(lo))), hi: Some(lit(Value::Int(hi))) };
            match rng.gen_bool(0.5) {
                true => (format!("a BETWEEN {lo} AND {hi}"), e),
                false => (format!("a NOT BETWEEN {lo} AND {hi}"), Expr::Not(Box::new(e))),
            }
        }
        10 => {
            let vs: Vec<i64> = (0..rng.gen_range(1..=3)).map(|_| rng.gen_range(-4..=4)).collect();
            let list = vs.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ");
            (format!("a IN ({list})"), Expr::In { expr: field(A), list: vs.iter().map(|v| Expr::Lit(Value::Int(*v))).collect() })
        }
        11 | 12 => {
            let (l, le) = cond(rng, depth - 1);
            let (r, re) = cond(rng, depth - 1);
            match pick == 11 {
                true => (format!("({l} AND {r})"), Expr::And(vec![le, re])),
                false => (format!("({l} OR {r})"), Expr::Or(vec![le, re])),
            }
        }
        _ => {
            let (x, xe) = cond(rng, depth - 1);
            (format!("NOT ({x})"), Expr::Not(Box::new(xe)))
        }
    }
}

/// Each row's values as the evaluator reads them: a multi-valued field
/// with no values is an empty list.
fn rows(db: &Db) -> Vec<Vec<Value>> {
    let mut rows = sql(db, "SELECT id, a, b, c, tags, n, flag, ts FROM t");
    for r in &mut rows {
        for f in MULTI {
            if r[f as usize] == Value::Null {
                r[f as usize] = Value::List(Vec::new());
            }
        }
    }
    rows
}

/// The rows of `all` for which `e` is TRUE.
fn passing(all: &[Vec<Value>], e: &Expr, text: &str) -> Vec<String> {
    ids(all.iter().filter(|r| holds(e, &|x| match x {
        Ref::Field(f) => Ok(r[f as usize].clone()),
        x => panic!("{x:?}"),
    }).unwrap_or_else(|err| panic!("{text}: {err}"))).cloned())
}

/// `text` with each column qualified by `alias`.
fn qualify(text: &str, alias: &str) -> String {
    let (mut out, mut word, mut quoted) = (String::new(), String::new(), false);
    for ch in text.chars().chain([' ']) {
        if !quoted && (ch.is_ascii_lowercase() || ch == '_') {
            word.push(ch);
            continue;
        }
        if NAMES.contains(&word.as_str()) {
            out += alias;
            out.push('.');
        }
        out += &std::mem::take(&mut word);
        quoted ^= ch == '\'';
        out.push(ch);
    }
    out.pop();
    out
}

fn ids(rows: impl IntoIterator<Item = Vec<Value>>) -> Vec<String> {
    let mut v: Vec<String> = rows.into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

#[test]
fn the_evaluator_and_the_indexes_agree() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("t.strata"));
    let mut rng = StdRng::seed_from_u64(7);
    fill(&db, &mut rng, 300);
    let all = rows(&db);
    let (mut some, mut none) = (0, 0);
    for i in 0..1500 {
        let (text, e) = cond(&mut rng, 3);
        let q = format!("SELECT id FROM t WHERE {text}");
        let want = passing(&all, &e, &text);
        assert_eq!(ids(sql(&db, &q)), want, "case {i}: {q}");
        // The plan the binder made of it, evaluated row by row, agrees too.
        let plan = db.explain(&q, &Params::new()).unwrap().to_json().unwrap();
        let filter: Expr = match plan["root"]["input"].get("filter") {
            Some(f) => serde_json::from_value(f.clone()).unwrap(),
            None => Expr::Lit(Value::Bool(true)),
        };
        let got: Vec<String> = ids(all.iter().filter(|r| holds(&filter, &|x| match x {
            Ref::Field(f) => Ok(r[f as usize].clone()),
            x => panic!("{x:?}"),
        }).unwrap()).cloned());
        assert_eq!(got, want, "case {i}, the bound plan: {q}");
        match want.len() {
            0 => none += 1,
            n if n < all.len() => some += 1,
            _ => {}
        }
    }
    assert!(some > 800 && none > 50, "{some} with some rows, {none} with none");
}

/// A join filters its rows in memory, with the evaluator, as the table's
/// indexes filter them: NULLs are UNKNOWN, and a multi-valued field with
/// no values compares as an empty list (`NOT (x.tags = 'a')` holds there).
/// Joins compared lists with a total order and treated such fields as
/// NULL before.
#[test]
fn joins_filter_as_the_table_does() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("j.strata"));
    let mut rng = StdRng::seed_from_u64(11);
    fill(&db, &mut rng, 120);
    let all = rows(&db);
    assert_eq!(qualify("NOT (tags = 'a' AND n IN (1)) OR ts > '2026-10-02'", "x"), "NOT (x.tags = 'a' AND x.n IN (1)) OR x.ts > '2026-10-02'");
    for i in 0..300 {
        let (text, e) = cond(&mut rng, 2);
        let want = passing(&all, &e, &text);
        // `y.id IS NULL` is FALSE, but naming both tables keeps the
        // condition off the scans: it runs over the joined rows.
        let q = format!("SELECT x.id FROM t x JOIN t y ON x.id = y.id WHERE {} OR y.id IS NULL", qualify(&text, "x"));
        assert_eq!(ids(sql(&db, &q)), want, "case {i}: {q}");
        // And as a condition of the join itself.
        let q = format!("SELECT x.id FROM t x JOIN t y ON x.id = y.id AND ({} OR y.id IS NULL)", qualify(&text, "x"));
        assert_eq!(ids(sql(&db, &q)), want, "case {i}: {q}");
    }
    // LIKE, CASE and functions over joined rows.
    let got = sql(&db, "SELECT x.id, CASE WHEN x.c LIKE 'x%' THEN upper(x.c) WHEN x.c IS NULL THEN 'none' ELSE x.c || '!' END AS k, abs(x.a) * 2 AS d FROM t x JOIN t y ON x.id = y.id WHERE NOT (y.a < 0) ORDER BY x.id LIMIT 300");
    let mut want: Vec<Vec<Value>> = all
        .iter()
        .filter(|r| r[1].as_i64().is_some_and(|a| a >= 0))
        .map(|r| {
            let k = match &r[3] {
                Value::Str(c) if c.starts_with('x') => c.to_uppercase(),
                Value::Null => "none".into(),
                Value::Str(c) => format!("{c}!"),
                v => panic!("{v:?}"),
            };
            vec![r[0].clone(), Value::Str(k), Value::Int(r[1].as_i64().unwrap().abs() * 2)]
        })
        .collect();
    want.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
    assert_eq!(got, want);
}

/// HAVING is TRUE, FALSE or UNKNOWN as SQL says: an aggregate of no
/// values is NULL, so neither a comparison with it nor its negation holds.
#[test]
fn having_is_three_valued() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir.path().join("h.strata"));
    for (id, b, c) in [("p", "1.0", "x"), ("q", "5.0", "x"), ("r", "2.0", "y"), ("s", "NULL", "w"), ("u", "NULL", "w")] {
        sql(&db, &format!("INSERT INTO t (id, b, c) VALUES ('{id}', {b}, '{c}')"));
    }
    let groups = |having: &str| {
        let mut v: Vec<String> = sql(&db, &format!("SELECT c, COUNT(*) FROM t GROUP BY c HAVING {having}")).into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
        v.sort();
        v
    };
    assert_eq!(groups("AVG(b) < 2.5"), ["y"]);
    assert_eq!(groups("NOT (AVG(b) < 2.5)"), ["x"]);
    assert_eq!(groups("MAX(b) != 2"), ["x"]);
    assert_eq!(groups("NOT (MIN(b) BETWEEN 0 AND 1.5)"), ["y"]);
    assert_eq!(groups("NOT (AVG(b) < 2.5) OR COUNT(*) = 2"), ["w", "x"]);
    assert_eq!(groups("NOT (SUM(b) > 100 AND COUNT(*) > 5)"), ["w", "x", "y"]);
}
