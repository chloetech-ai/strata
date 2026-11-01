//! JSON fields: sparse attributes no schema names, filtered by their paths
//! (`attrs.color = 'red'`) from the field's terms. For random conditions
//! over rows whose objects hold a few of many keys, with values of mixed
//! types, arrays, nested objects and escaped keys, a query returns exactly
//! the rows a model of SQL's three-valued logic over the objects keeps,
//! and so do the plan the binder made of it, evaluated row by row, and a
//! join, which filters its rows in memory. Paths are read in SELECT, ORDER
//! BY, GROUP BY and aggregates; the objects survive the WAL, flushes,
//! merges, reopening and the directory layout.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as Json};
use strata::db::{Db, SqlResult};
use strata::query::ir::eval::{holds, Ref};
use strata::query::ir::Expr;
use strata::query::paths::Call;
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::value::Value;

const TABLE: &str = "CREATE TABLE t (id KEYWORD PRIMARY KEY, name KEYWORD, attrs JSON)";

fn seed() -> Schema {
    Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap()
}

fn opts(layout: Layout) -> DbOptions {
    DbOptions { sync: false, layout, segment_max_docs: 150, ..DbOptions::default() }
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    sql_with(db, s, &Params::new())
}

fn sql_with(db: &Db, s: &str, p: &Params) -> Vec<Vec<Value>> {
    match db.sql(s, p).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

fn written(db: &Db, s: &str) -> u64 {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Written(n) => n as u64,
        SqlResult::Rows(_) => panic!("{s}: rows"),
    }
}

fn ids(rows: impl IntoIterator<Item = Vec<Value>>) -> Vec<String> {
    let mut v: Vec<String> = rows.into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

const LONG: usize = 300;

/// Row `i`'s object: a few of the keys, or none (NULL).
fn object(rng: &mut StdRng) -> Option<Json> {
    if rng.gen_ratio(1, 10) {
        return None;
    }
    let mut m = serde_json::Map::new();
    let mut put = |k: &str, p: (u32, u32), v: &mut dyn FnMut(&mut StdRng) -> Json, rng: &mut StdRng| {
        if rng.gen_ratio(p.0, p.1) {
            let v = v(rng);
            m.insert(k.to_string(), v);
        }
    };
    let pick = |rng: &mut StdRng, xs: &[Json]| xs[rng.gen_range(0..xs.len())].clone();
    put("color", (2, 5), &mut |r| pick(r, &[json!("red"), json!("blue"), json!("black"), json!("bl"), json!("Red")]), rng);
    put(
        "voltage",
        (2, 5),
        &mut |r| pick(r, &[json!(-1), json!(0), json!(2.5), json!(5), json!(5.0), json!(7), json!(12), json!("n/a"), json!("5"), json!([3, "n/a"]), json!([2, 9]), json!(null)]),
        rng,
    );
    put(
        "certs",
        (2, 5),
        &mut |r| {
            let all = ["CE", "UL", "FCC"];
            Json::Array(all.iter().filter(|_| r.gen_ratio(1, 2)).map(|c| json!(c)).collect())
        },
        rng,
    );
    put(
        "dims",
        (1, 3),
        &mut |r| {
            let mut d = serde_json::Map::new();
            if r.gen_ratio(3, 4) {
                d.insert("w".into(), pick(r, &[json!(1), json!(4), json!(10.5), json!(40), json!("wide")]));
            }
            if r.gen_ratio(1, 2) {
                d.insert("h".into(), pick(r, &[json!(2), json!(3), json!(7.25)]));
            }
            Json::Object(d)
        },
        rng,
    );
    put("ok", (2, 5), &mut |r| pick(r, &[json!(true), json!(false), json!("yes")]), rng);
    put("a.b", (1, 4), &mut |r| json!(r.gen_range(1..=3)), rng);
    put(
        "items",
        (1, 3),
        &mut |r| {
            let n = r.gen_range(0..=3);
            Json::Array(
                (0..n)
                    .map(|_| match r.gen_ratio(1, 2) {
                        true => json!({ "sku": pick(r, &[json!("x"), json!("y")]), "n": r.gen_range(1..=4) }),
                        false => json!({ "sku": pick(r, &[json!("x"), json!("z")]) }),
                    })
                    .collect(),
            )
        },
        rng,
    );
    put("none", (1, 5), &mut |_| json!(null), rng);
    put("e", (1, 5), &mut |r| pick(r, &[json!([]), json!([[]]), json!({})]), rng);
    put("long", (1, 8), &mut |r| json!(format!("abc{}", "z".repeat(LONG - 3 + r.gen_range(0..2)))), rng);
    Some(Json::Object(m))
}

/// A value a condition compares a path with.
#[derive(Clone, Debug, PartialEq)]
enum Lit {
    S(String),
    N(f64),
    B(bool),
}

impl Lit {
    fn sql(&self) -> String {
        match self {
            Lit::S(s) => format!("'{}'", s.replace('\'', "''")),
            Lit::N(x) if x.fract() == 0.0 => format!("{}", *x as i64),
            Lit::N(x) => format!("{x:?}"),
            Lit::B(b) => b.to_string().to_uppercase(),
        }
    }

    fn kind(&self) -> u8 {
        match self {
            Lit::S(_) => 0,
            Lit::N(_) => 1,
            Lit::B(_) => 2,
        }
    }

    fn cmp(&self, o: &Lit) -> Option<std::cmp::Ordering> {
        match (self, o) {
            (Lit::S(a), Lit::S(b)) => Some(a.as_bytes().cmp(b.as_bytes())),
            (Lit::N(a), Lit::N(b)) => a.partial_cmp(b),
            (Lit::B(a), Lit::B(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }
}

/// What JSON is at `keys` below `j`, through arrays.
fn gather<'j>(j: &'j Json, keys: &[&str], out: &mut Vec<&'j Json>) {
    match keys.split_first() {
        None => out.push(j),
        Some((k, rest)) => match j {
            Json::Object(m) => {
                if let Some(v) = m.get(*k) {
                    gather(v, rest, out)
                }
            }
            Json::Array(xs) => xs.iter().for_each(|x| gather(x, keys, out)),
            _ => {}
        },
    }
}

/// The values a comparison sees in `j`: strings (no longer than the
/// index keeps), numbers and booleans, through arrays.
fn leaves(j: &Json, out: &mut Vec<Lit>) {
    match j {
        Json::Array(xs) => xs.iter().for_each(|x| leaves(x, out)),
        Json::String(s) if s.len() <= 256 => out.push(Lit::S(s.clone())),
        Json::Number(n) => out.push(Lit::N(n.as_f64().unwrap())),
        Json::Bool(b) => out.push(Lit::B(*b)),
        _ => {}
    }
}

/// Whether anything (a value or an object) is in `j`, at any depth of arrays.
fn present(j: &Json) -> bool {
    match j {
        Json::Null => false,
        Json::Array(xs) => xs.iter().any(present),
        _ => true,
    }
}

/// A condition, as the model reads it.
#[derive(Clone, Debug)]
enum Cond {
    /// `path op lit`.
    Cmp(usize, &'static str, Lit),
    /// `path [NOT] IN (lits)`.
    In(usize, Vec<Lit>, bool),
    /// `path [NOT] BETWEEN lo AND hi`.
    Between(usize, Lit, Lit, bool),
    /// `path IS [NOT] NULL`.
    IsNull(usize, bool),
    /// `path LIKE 'prefix%'`.
    Prefix(usize, String),
    /// `name = 'n'`, a declared field.
    Name(String),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
}

/// The paths conditions read, as SQL spells them (`{a}` the field, perhaps
/// qualified) and as keys.
const PATHS: [(&str, &[&str]); 14] = [
    ("{a}.color", &["color"]),
    ("{a}.voltage", &["voltage"]),
    ("{a}.certs", &["certs"]),
    ("{a}.dims.w", &["dims", "w"]),
    ("json_extract({a}, 'dims.h')", &["dims", "h"]),
    ("{a}.ok", &["ok"]),
    ("{a}['a.b']", &["a.b"]),
    ("{a}.items.sku", &["items", "sku"]),
    ("{a}.items.n", &["items", "n"]),
    ("{a}.long", &["long"]),
    // For IS NULL only.
    ("{a}.dims", &["dims"]),
    ("{a}.items", &["items"]),
    ("{a}['none']", &["none"]),
    ("{a}.e", &["e"]),
];
const COMPARED: usize = 10;

fn path_sql(p: usize, a: &str) -> String {
    PATHS[p].0.replace("{a}", a)
}

fn and3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn or3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

impl Cond {
    fn sql(&self, a: &str, name: &str) -> String {
        match self {
            Cond::Cmp(p, op, v) => format!("{} {op} {}", path_sql(*p, a), v.sql()),
            Cond::In(p, vs, neg) => format!("{} {}IN ({})", path_sql(*p, a), if *neg { "NOT " } else { "" }, vs.iter().map(Lit::sql).collect::<Vec<_>>().join(", ")),
            Cond::Between(p, lo, hi, neg) => format!("{} {}BETWEEN {} AND {}", path_sql(*p, a), if *neg { "NOT " } else { "" }, lo.sql(), hi.sql()),
            Cond::IsNull(p, neg) => format!("{} IS {}NULL", path_sql(*p, a), if *neg { "NOT " } else { "" }),
            Cond::Prefix(p, s) => format!("{} LIKE '{s}%'", path_sql(*p, a)),
            Cond::Name(n) => format!("{name} = '{n}'"),
            Cond::And(x, y) => format!("({} AND {})", x.sql(a, name), y.sql(a, name)),
            Cond::Or(x, y) => format!("({} OR {})", x.sql(a, name), y.sql(a, name)),
            Cond::Not(x) => format!("NOT ({})", x.sql(a, name)),
        }
    }

    /// TRUE, FALSE or UNKNOWN for a row.
    fn eval(&self, name: &str, attrs: &Option<Json>) -> Option<bool> {
        let found = |p: usize| -> Vec<&Json> {
            let mut out = Vec::new();
            if let Some(j) = attrs {
                gather(j, PATHS[p].1, &mut out);
            }
            out
        };
        // The values of `kind` at path `p`; UNKNOWN to a test when none.
        let vals = |p: usize, kind: u8| -> Vec<Lit> {
            let mut out = Vec::new();
            found(p).into_iter().for_each(|j| leaves(j, &mut out));
            out.retain(|v| v.kind() == kind);
            out
        };
        let any = |p: usize, kind: u8, f: &dyn Fn(&Lit) -> bool| -> Option<bool> {
            let vs = vals(p, kind);
            if vs.is_empty() { None } else { Some(vs.iter().any(f)) }
        };
        match self {
            Cond::Cmp(p, op, v) => {
                use std::cmp::Ordering::*;
                let eq = any(*p, v.kind(), &|x| x.cmp(v) == Some(Equal));
                match *op {
                    "=" => eq,
                    "<>" => eq.map(|b| !b),
                    "<" => any(*p, v.kind(), &|x| x.cmp(v) == Some(Less)),
                    "<=" => any(*p, v.kind(), &|x| x.cmp(v) != Some(Greater)),
                    ">" => any(*p, v.kind(), &|x| x.cmp(v) == Some(Greater)),
                    ">=" => any(*p, v.kind(), &|x| x.cmp(v) != Some(Less)),
                    _ => unreachable!(),
                }
            }
            Cond::In(p, vs, neg) => {
                let mut kinds: Vec<u8> = vs.iter().map(Lit::kind).collect();
                kinds.sort();
                kinds.dedup();
                let r = kinds.iter().fold(Some(false), |acc, k| or3(acc, any(*p, *k, &|x| vs.iter().any(|v| v.kind() == *k && x.cmp(v) == Some(std::cmp::Ordering::Equal)))));
                if *neg { r.map(|b| !b) } else { r }
            }
            Cond::Between(p, lo, hi, neg) => {
                let r = any(*p, lo.kind(), &|x| x.cmp(lo) != Some(std::cmp::Ordering::Less) && x.cmp(hi) != Some(std::cmp::Ordering::Greater));
                if *neg { r.map(|b| !b) } else { r }
            }
            Cond::IsNull(p, neg) => Some(found(*p).into_iter().any(present) == *neg),
            Cond::Prefix(p, s) => any(*p, 0, &|x| matches!(x, Lit::S(v) if v.starts_with(s.as_str()))),
            Cond::Name(n) => Some(name == n),
            Cond::And(x, y) => and3(x.eval(name, attrs), y.eval(name, attrs)),
            Cond::Or(x, y) => or3(x.eval(name, attrs), y.eval(name, attrs)),
            Cond::Not(x) => x.eval(name, attrs).map(|b| !b),
        }
    }
}

fn lit(rng: &mut StdRng) -> Lit {
    const S: [&str; 12] = ["red", "blue", "black", "bl", "x", "y", "z", "CE", "UL", "n/a", "5", "wide"];
    const N: [f64; 10] = [-1.0, 0.0, 1.0, 2.0, 2.5, 3.0, 5.0, 7.0, 10.5, 12.0];
    match rng.gen_range(0..10) {
        0..=4 => Lit::S(S[rng.gen_range(0..S.len())].to_string()),
        5..=8 => Lit::N(N[rng.gen_range(0..N.len())]),
        _ => Lit::B(rng.gen_bool(0.5)),
    }
}

/// A value path `p` holds, mostly: conditions that keep some rows.
fn lit_for(rng: &mut StdRng, p: usize) -> Lit {
    let s = |x: &str| Lit::S(x.to_string());
    let n = Lit::N;
    let pool: Vec<Lit> = match p {
        0 => vec![s("red"), s("blue"), s("black"), s("bl"), s("Red")],
        1 => vec![n(-1.0), n(0.0), n(2.5), n(3.0), n(5.0), n(7.0), n(12.0), s("n/a"), s("5")],
        2 => vec![s("CE"), s("UL"), s("FCC")],
        3 => vec![n(1.0), n(4.0), n(10.5), n(40.0), s("wide")],
        4 => vec![n(2.0), n(3.0), n(7.25)],
        5 => vec![Lit::B(true), Lit::B(false), s("yes")],
        6 => vec![n(1.0), n(2.0), n(3.0)],
        7 => vec![s("x"), s("y"), s("z")],
        8 => vec![n(1.0), n(2.0), n(3.0), n(4.0)],
        _ => vec![s("abc"), s("z")],
    };
    if rng.gen_ratio(1, 5) { lit(rng) } else { pool[rng.gen_range(0..pool.len())].clone() }
}

fn number(rng: &mut StdRng) -> Lit {
    Lit::N([-1.0, 0.0, 2.0, 2.5, 4.0, 5.0, 7.0, 12.0][rng.gen_range(0..8)])
}

fn cond(rng: &mut StdRng, depth: u32) -> Cond {
    let pick = if depth == 0 { rng.gen_range(0..7) } else { rng.gen_range(0..10) };
    let p = rng.gen_range(0..COMPARED);
    match pick {
        0 | 1 => {
            let op = ["=", "<>", "<", "<=", ">", ">="][rng.gen_range(0..6)];
            let mut v = lit_for(rng, p);
            // Ranges of numbers and strings.
            while op != "=" && op != "<>" && v.kind() == 2 {
                v = lit(rng);
            }
            Cond::Cmp(p, op, v)
        }
        2 => Cond::In(p, (0..rng.gen_range(1..=3)).map(|_| lit_for(rng, p)).collect(), rng.gen_ratio(1, 3)),
        3 => {
            let (a, b) = (number(rng), number(rng));
            let (lo, hi) = if a.cmp(&b) == Some(std::cmp::Ordering::Greater) { (b, a) } else { (a, b) };
            Cond::Between(p, lo, hi, rng.gen_ratio(1, 3))
        }
        4 => Cond::IsNull(rng.gen_range(0..PATHS.len()), rng.gen_bool(0.5)),
        5 => Cond::Prefix(p, ["b", "bl", "re", "abc", "", "n/"][rng.gen_range(0..6)].to_string()),
        6 => Cond::Name(format!("n{}", rng.gen_range(0..3))),
        7 => Cond::And(Box::new(cond(rng, depth - 1)), Box::new(cond(rng, depth - 1))),
        8 => Cond::Or(Box::new(cond(rng, depth - 1)), Box::new(cond(rng, depth - 1))),
        _ => Cond::Not(Box::new(cond(rng, depth - 1))),
    }
}

struct Row {
    id: String,
    name: String,
    attrs: Option<Json>,
}

/// Fills `t`: through SQL text, parameters and the bulk loader's JSON
/// objects, half flushed to segments.
fn fill(db: &Db, rng: &mut StdRng, n: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for i in 0..n {
        let r = Row { id: format!("r{i:04}"), name: format!("n{}", i % 3), attrs: object(rng) };
        match i % 3 {
            0 => {
                let a = r.attrs.as_ref().map_or("NULL".to_string(), |j| format!("'{}'", serde_json::to_string_pretty(j).unwrap().replace('\'', "''")));
                sql(db, &format!("INSERT INTO t (id, name, attrs) VALUES ('{}', '{}', {a})", r.id, r.name));
            }
            1 => {
                let p = Params::new().set("a", r.attrs.as_ref().map_or(Value::Null, |j| Value::Str(j.to_string())));
                sql_with(db, &format!("INSERT INTO t (id, name, attrs) VALUES ('{}', '{}', :a)", r.id, r.name), &p);
            }
            _ => {
                let schema = db.storage().table("t").unwrap();
                let j = json!({ "id": r.id, "name": r.name, "attrs": r.attrs.clone().unwrap_or(Json::Null) });
                let d = strata::ingest::document(&schema, &j).unwrap();
                let mut t = db.storage().begin().unwrap();
                t.table("t").unwrap().insert(d).unwrap();
                t.commit().unwrap();
            }
        }
        if i == n / 2 {
            db.storage().flush().unwrap();
        }
        rows.push(r);
    }
    rows
}

/// Each row as the evaluator reads it, by field id.
fn values(db: &Db) -> Vec<Vec<Value>> {
    sql(db, "SELECT id, name, attrs FROM t")
}

fn check(db: &Db, rows: &[Row], all: &[Vec<Value>], rng: &mut StdRng, cases: usize, joins: usize) {
    let (mut some, mut none) = (0, 0);
    for i in 0..cases {
        let c = cond(rng, 3);
        let text = c.sql("attrs", "name");
        let want: Vec<String> = rows.iter().filter(|r| c.eval(&r.name, &r.attrs) == Some(true)).map(|r| r.id.clone()).collect();
        let q = format!("SELECT id FROM t WHERE {text}");
        assert_eq!(ids(sql(db, &q)), want, "case {i}: {q}");
        // The plan the binder made of it, evaluated row by row.
        let plan = db.explain(&q, &Params::new()).unwrap().to_json().unwrap();
        let filter: Expr = match plan["root"]["input"].get("filter") {
            Some(f) => serde_json::from_value(f.clone()).unwrap(),
            None => Expr::Lit(Value::Bool(true)),
        };
        let got = ids(all.iter().filter(|r| holds(&filter, &|x| match x {
            Ref::Field(f) => Ok(r[f as usize].clone()),
            x => panic!("{x:?}"),
        }).unwrap_or_else(|e| panic!("case {i}, the bound plan: {q}: {e}"))).cloned());
        assert_eq!(got, want, "case {i}, the bound plan: {q}");
        // A join filters its rows in memory: `y.id IS NULL` is FALSE, but
        // naming both tables keeps the condition off the scans.
        if i < joins {
            let q = format!("SELECT x.id FROM t x JOIN t y ON x.id = y.id WHERE {} OR y.id IS NULL", c.sql("x.attrs", "x.name"));
            assert_eq!(ids(sql(db, &q)), want, "case {i}, joined: {q}");
        }
        match want.len() {
            0 => none += 1,
            n if n < rows.len() => some += 1,
            _ => {}
        }
    }
    assert!(some > cases / 2 && none > cases / 50, "{some} with some rows, {none} with none");
}

fn run(name: &str, layout: Layout) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    let mut rng = StdRng::seed_from_u64(17);
    let rows = {
        let db = Db::create(&path, seed(), opts(layout)).unwrap();
        sql(&db, TABLE);
        let rows = fill(&db, &mut rng, 400);
        let all = values(&db);
        assert_eq!(all.len(), rows.len());
        check(&db, &rows, &all, &mut rng, 1500, 200);
        listings(&db, &rows);
        rows
    };
    // Reopen (the memtable's objects come back from the WAL), flush, merge.
    let db = Db::open(&path, opts(layout)).unwrap();
    let all = values(&db);
    check(&db, &rows, &all, &mut rng, 300, 30);
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
    check(&db, &rows, &all, &mut rng, 300, 30);
    reads(&db, &rows);
    listings(&db, &rows);
    // Deleted rows leave the listing, in the segments and the memtable.
    written(&db, "DELETE FROM t WHERE name = 'n2' OR attrs.color = 'red'");
    sql(&db, "UPDATE t SET attrs = '{\"color\": \"teal\", \"new\": {\"k\": [1, 2]}}' WHERE id = 'r0001'");
    let rows: Vec<Row> = rows
        .into_iter()
        .filter(|r| !(r.name == "n2" || value_at(&r.attrs, &["color"]) == Value::Str("red".into())))
        .map(|r| match r.id.as_str() {
            "r0001" => Row { attrs: Some(json!({"color": "teal", "new": {"k": [1, 2]}})), ..r },
            _ => r,
        })
        .collect();
    listings(&db, &rows);
}

#[test]
fn paths_filter_as_sql_would_in_a_single_file() {
    run("j.strata", Layout::SingleFile);
}

#[test]
fn paths_filter_as_sql_would_in_a_directory() {
    run("j.dir", Layout::Directory);
}

/// The value a query returns for `keys` of an object: as `json_extract`
/// documents it.
fn value_at(attrs: &Option<Json>, keys: &[&str]) -> Value {
    let Some(j) = attrs else { return Value::Null };
    let mut found = Vec::new();
    gather(j, keys, &mut found);
    let mut out = Vec::new();
    for x in found {
        match x {
            Json::Null => {}
            Json::Array(xs) if xs.iter().all(|x| !x.is_object() && !x.is_array()) => out.extend(xs.iter().filter_map(plain)),
            Json::Array(_) | Json::Object(_) => {
                if present(x) {
                    out.push(Value::Str(x.to_string()))
                }
            }
            x => out.extend(plain(x)),
        }
    }
    match out.len() {
        0 => Value::Null,
        1 => out.pop().unwrap(),
        _ => Value::List(out),
    }
}

fn plain(x: &Json) -> Option<Value> {
    Some(match x {
        Json::String(s) => Value::Str(s.clone()),
        Json::Number(n) => n.as_i64().map_or_else(|| Value::Float(n.as_f64().unwrap()), Value::Int),
        Json::Bool(b) => Value::Bool(*b),
        _ => return None,
    })
}

/// SELECT, ORDER BY, GROUP BY and aggregates of paths.
fn reads(db: &Db, rows: &[Row]) {
    // SELECT of paths returns their values; of the field, its text.
    let got = sql(db, "SELECT id, attrs.color, attrs.dims.w, attrs.certs, attrs['a.b'], attrs.items.sku, attrs.dims, attrs FROM t ORDER BY id");
    assert_eq!(got.len(), rows.len());
    for (g, r) in got.iter().zip(rows) {
        assert_eq!(g[0], Value::Str(r.id.clone()));
        for (i, keys) in [&["color"][..], &["dims", "w"], &["certs"], &["a.b"], &["items", "sku"], &["dims"]].iter().enumerate() {
            let want = value_at(&r.attrs, keys);
            // A missing array and an empty one are both NULL to a test.
            let same = g[i + 1] == want || (want == Value::Null && g[i + 1] == Value::List(vec![]));
            assert!(same, "{} {keys:?}: {:?}, not {want:?}", r.id, g[i + 1]);
        }
        let text = r.attrs.as_ref().map_or(Value::Null, |j| Value::Str(j.to_string()));
        assert_eq!(g[7], text, "{}: stored as compact JSON with sorted keys", r.id);
    }
    // A path's column is named as the query writes it; one named by AS
    // keeps its name.
    let SqlResult::Rows(r) = db.sql("SELECT attrs.dims.w, attrs['a.b'], attrs.color AS c, COUNT(*) FROM t GROUP BY attrs.dims.w, attrs['a.b'], attrs.color", &Params::new()).unwrap() else { panic!("rows") };
    assert_eq!(r.columns, ["attrs.dims.w", "attrs['a.b']", "c", "COUNT(*)"]);

    // GROUP BY a path: a group per value, NULL for rows without one.
    let mut want: std::collections::BTreeMap<String, i64> = Default::default();
    let mut nulls = 0;
    for r in rows {
        match value_at(&r.attrs, &["color"]) {
            Value::Str(s) => *want.entry(s).or_default() += 1,
            _ => nulls += 1,
        }
    }
    let got = sql(db, "SELECT attrs.color, COUNT(*) FROM t GROUP BY attrs.color ORDER BY attrs.color");
    let mut groups: std::collections::BTreeMap<String, i64> = Default::default();
    let mut null_group = 0;
    for g in &got {
        match &g[0] {
            Value::Str(s) => {
                groups.insert(s.clone(), g[1].as_i64().unwrap());
            }
            Value::Null => null_group = g[1].as_i64().unwrap(),
            v => panic!("{v:?}"),
        }
    }
    assert_eq!((groups, null_group), (want, nulls));

    // ORDER BY a path's numbers, with a filter on another.
    let mut want: Vec<(f64, String)> = rows
        .iter()
        .filter_map(|r| match value_at(&r.attrs, &["dims", "w"]) {
            Value::Int(x) => Some((x as f64, r.id.clone())),
            Value::Float(x) => Some((x, r.id.clone())),
            _ => None,
        })
        .collect();
    want.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(a.1.cmp(&b.1)));
    let got = sql(db, "SELECT id, attrs.dims.w FROM t WHERE attrs.dims.w > -100 ORDER BY attrs.dims.w DESC, id LIMIT 10");
    let got: Vec<String> = got.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    assert_eq!(got, want.iter().take(10).map(|w| w.1.clone()).collect::<Vec<_>>());

    // Aggregates of a path's numbers.
    let ws: Vec<f64> = want.iter().map(|w| w.0).collect();
    let got = sql(db, "SELECT COUNT(attrs.dims.h), MAX(attrs.dims.w), MIN(attrs.dims.w) FROM t WHERE attrs.dims.w >= -100");
    let hs = rows.iter().filter(|r| value_at(&r.attrs, &["dims", "w"]).as_f64().is_some() && value_at(&r.attrs, &["dims", "h"]) != Value::Null).count();
    assert_eq!(got[0][0].as_i64(), Some(hs as i64));
    assert_eq!(got[0][1].as_f64(), ws.iter().cloned().reduce(f64::max));
    assert_eq!(got[0][2].as_f64(), ws.iter().cloned().reduce(f64::min));
}

/// A path's listing as a model computes it from the objects.
#[derive(Debug, Default, PartialEq)]
struct Listed {
    rows: u64,
    /// Rows with a string, a number, a boolean there.
    kinds: [u64; 3],
    /// Each value (typed: `s:`, `n:`, `b:` and its text) and its rows.
    values: std::collections::BTreeMap<String, u64>,
    min: Option<f64>,
    max: Option<f64>,
}

/// A key as a path spells it: dots and backslashes escaped.
fn escaped(k: &str) -> String {
    k.chars().flat_map(|c| if c == '.' || c == '\\' { vec!['\\', c] } else { vec![c] }).collect()
}

/// What one object holds at each path: whether it holds anything there,
/// its types, its values (each once).
fn holds_at(j: &Json, path: &str, out: &mut std::collections::BTreeMap<String, ([bool; 3], std::collections::BTreeSet<String>, Vec<f64>)>) {
    let mut mark = |path: &str, kind: Option<(usize, String)>, x: Option<f64>| {
        let e = out.entry(path.to_string()).or_default();
        if let Some((k, v)) = kind {
            e.0[k] = true;
            e.1.insert(v);
        }
        e.2.extend(x);
    };
    match j {
        Json::Null => {}
        Json::Object(m) => {
            if !path.is_empty() {
                mark(path, None, None);
            }
            for (k, v) in m {
                let child = if path.is_empty() { escaped(k) } else { format!("{path}.{}", escaped(k)) };
                holds_at(v, &child, out);
            }
        }
        Json::Array(xs) => xs.iter().for_each(|x| holds_at(x, path, out)),
        Json::String(s) if s.len() > 256 => mark(path, None, None),
        Json::String(s) => mark(path, Some((0, format!("s:{s}"))), None),
        Json::Number(n) => {
            let x = n.as_f64().unwrap();
            mark(path, Some((1, format!("n:{x}"))), Some(x));
        }
        Json::Bool(b) => mark(path, Some((2, format!("b:{b}"))), None),
    }
}

fn model_listing<'r>(rows: impl IntoIterator<Item = &'r Row>) -> std::collections::BTreeMap<String, Listed> {
    let mut out: std::collections::BTreeMap<String, Listed> = Default::default();
    for r in rows {
        let mut at = Default::default();
        if let Some(j) = &r.attrs {
            holds_at(j, "", &mut at);
        }
        for (path, (kinds, values, xs)) in at {
            let l = out.entry(path).or_default();
            l.rows += 1;
            for k in 0..3 {
                l.kinds[k] += kinds[k] as u64;
            }
            for v in values {
                *l.values.entry(v).or_default() += 1;
            }
            for x in xs {
                l.min = Some(l.min.map_or(x, |m: f64| m.min(x)));
                l.max = Some(l.max.map_or(x, |m: f64| m.max(x)));
            }
        }
    }
    out
}

fn typed(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("s:{s}"),
        Value::Float(x) => format!("n:{x}"),
        Value::Bool(b) => format!("b:{b}"),
        v => panic!("{v:?}"),
    }
}

/// The listing of `call` is the model's over `rows` (those it reads), and
/// `SELECT * FROM json_paths(...)` answers it as rows.
fn listing<'r>(db: &Db, call: &Call, rows: impl IntoIterator<Item = &'r Row> + Clone) {
    let got = db.json_paths(call, &Params::new()).unwrap_or_else(|e| panic!("{call:?}: {e}"));
    let n = rows.clone().into_iter().count() as u64;
    let want: Vec<(String, Listed)> = model_listing(rows)
        .into_iter()
        .filter(|(p, _)| call.prefix.as_ref().is_none_or(|q| p == q || p.starts_with(&format!("{q}."))))
        .collect();
    assert_eq!(got.rows, n, "{call:?}: rows read");
    assert!(!got.truncated);
    assert_eq!(got.paths.iter().map(|p| p.path.clone()).collect::<Vec<_>>(), want.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(), "{call:?}: paths");
    for (g, (path, w)) in got.paths.iter().zip(&want) {
        let what = format!("{call:?}: {path}");
        assert_eq!((g.rows, [g.strings, g.numbers, g.bools]), (w.rows, w.kinds), "{what}");
        assert_eq!((g.min, g.max), (w.min, w.max), "{what}");
        assert_eq!(g.distinct, Some(w.values.len() as u64), "{what}");
        let vs = g.values.as_ref().unwrap_or_else(|| panic!("{what}: values"));
        assert!(vs.windows(2).all(|p| p[0].1 >= p[1].1), "{what}: most rows first");
        let listed: std::collections::BTreeMap<String, u64> = vs.iter().map(|(v, c)| (typed(v), *c)).collect();
        assert_eq!(listed, w.values, "{what}");
    }
    // As a query: the same listing, as rows.
    let rs = db.query(&call.sql(), &Params::new()).unwrap();
    assert_eq!(rs.rows, got.to_result().rows, "{}", call.sql());
    assert_eq!(strata::query::paths::Paths::from_result(&rs).unwrap(), got);
}

fn listings(db: &Db, rows: &[Row]) {
    let call = |prefix: Option<&str>, filter: Option<&str>| Call { table: "t".into(), field: "attrs".into(), prefix: prefix.map(String::from), filter: filter.map(String::from) };
    listing(db, &call(None, None), rows);
    listing(db, &call(Some("dims"), None), rows);
    listing(db, &call(Some("items"), None), rows);
    listing(db, &call(Some("a\\.b"), None), rows);
    listing(db, &call(Some("nowhere"), None), rows);
    listing(db, &call(None, Some("name = 'n1'")), rows.iter().filter(|r| r.name == "n1"));
    listing(db, &call(Some("dims"), Some("attrs.color IS NOT NULL")), rows.iter().filter(|r| value_at(&r.attrs, &["color"]) != Value::Null));
    let all = model_listing(rows);
    assert!(all.len() >= 12 && all["color"].values.len() == 5 && all["dims"].kinds == [0, 0, 0] && all["voltage"].kinds[0] > 0 && all["voltage"].kinds[1] > 0 && all["long"].kinds == [0, 0, 0], "{all:?}");
}

fn table(path: &std::path::Path) -> Db {
    let db = Db::create(path, seed(), opts(Layout::SingleFile)).unwrap();
    sql(&db, TABLE);
    db
}

#[test]
fn writes_canonicalize_and_refuse_what_is_not_an_object() {
    let dir = tempfile::tempdir().unwrap();
    let db = table(&dir.path().join("w.strata"));
    sql(&db, r#"INSERT INTO t (id, attrs) VALUES ('a', '{ "b": 1, "a": {"y": [1, 2.5, "x"]} }')"#);
    assert_eq!(sql(&db, "SELECT attrs FROM t WHERE id = 'a'"), vec![vec![Value::Str(r#"{"a":{"y":[1,2.5,"x"]},"b":1}"#.into())]]);
    let bad = |v: &str| db.sql(&format!("INSERT INTO t (id, attrs) VALUES ('x', '{}')", v.replace('\'', "''")), &Params::new()).unwrap_err().to_string();
    assert!(bad("[1, 2]").contains("object"), "{}", bad("[1, 2]"));
    assert!(!bad("{not json").is_empty());
    assert!(bad(r#"{"": 1}"#).contains("empty key"));
    let deep = (0..20).fold(json!(1), |j, _| json!({ "k": j }));
    assert!(bad(&deep.to_string()).contains("nested"));
    let wide: serde_json::Map<String, Json> = (0..1001).map(|i| (format!("k{i}"), json!(i))).collect();
    assert!(bad(&Json::Object(wide).to_string()).contains("1000"));
    let long_key = json!({ "k".repeat(300): 1 });
    assert!(bad(&long_key.to_string()).contains("256"));
    // Nothing was written by the refusals.
    assert_eq!(sql(&db, "SELECT COUNT(*) FROM t"), vec![vec![Value::Int(1)]]);

    // UPDATE sets the object, and the index follows; DELETE by a path.
    sql(&db, r#"INSERT INTO t (id, name, attrs) VALUES ('b', 'n', '{"color": "red"}'), ('c', 'n', '{"color": "blue", "size": 3}')"#);
    assert_eq!(written(&db, r#"UPDATE t SET attrs = '{"color": "green"}' WHERE attrs.color = 'red'"#), 1);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE attrs.color = 'green'")), vec!["b"]);
    assert_eq!(written(&db, "UPDATE t SET name = 'm' WHERE attrs.size >= 3"), 1);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE name = 'm' AND attrs.color = 'blue'")), vec!["c"]);
    db.storage().flush().unwrap();
    assert_eq!(written(&db, "DELETE FROM t WHERE attrs.a.y = 'x'"), 1);
    assert_eq!(ids(sql(&db, "SELECT id FROM t")), vec!["b", "c"]);
}

#[test]
fn schemas_and_queries_refuse_what_a_json_field_cannot_be() {
    let dir = tempfile::tempdir().unwrap();
    let db = table(&dir.path().join("s.strata"));
    let bad = |s: &str| db.sql(s, &Params::new()).is_err();
    assert!(bad("CREATE TABLE k (attrs JSON PRIMARY KEY)"));
    assert!(bad("CREATE TABLE k (id KEYWORD PRIMARY KEY, attrs JSON[])"));
    assert!(bad("CREATE TABLE k (id KEYWORD PRIMARY KEY, attrs JSON DEFAULT '{}')"));
    assert!(Schema::new("k", vec![Field::new("id", FieldType::Keyword), Field::new("a", FieldType::Json).multi()], Some("id")).is_err());
    // A path is compared with values, not with another path; a range has
    // bounds of one type.
    sql(&db, r#"INSERT INTO t (id, attrs) VALUES ('a', '{"x": 1, "y": 2}')"#);
    assert!(bad("SELECT id FROM t WHERE attrs.x = attrs.y"));
    assert!(bad("SELECT id FROM t WHERE attrs.x BETWEEN 1 AND 'z'"));
    assert!(bad("SELECT id FROM t WHERE name.x = 1"), "a path of a field that is not JSON");
    // json_extract's third argument reads one type there.
    sql(&db, r#"INSERT INTO t (id, attrs) VALUES ('b', '{"x": "one"}')"#);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE json_extract(attrs, 'x', 'number') IS NULL")), vec!["b"]);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE json_extract(attrs, '$.x', 'string') IS NOT NULL")), vec!["b"]);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE json_extract(attrs, 'x', 'number') < 5")), vec!["a"]);
    assert!(bad("SELECT id FROM t WHERE json_extract(attrs, 'x', 'number') = 'one'"));
    // JSONB is a name for JSON.
    sql(&db, "CREATE TABLE k (id KEYWORD PRIMARY KEY, a JSONB)");
    sql(&db, r#"INSERT INTO k (id, a) VALUES ('1', '{"p": true}')"#);
    assert_eq!(ids(sql(&db, "SELECT id FROM k WHERE a.p")), vec!["1"]);
    assert_eq!(ids(sql(&db, "SELECT id FROM k WHERE a.p IS TRUE")), vec!["1"]);
    assert!(sql(&db, "SELECT id FROM k WHERE a.p IS FALSE").is_empty());
}

#[test]
fn a_json_column_added_to_a_table_with_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = table(&dir.path().join("a.strata"));
    sql(&db, "INSERT INTO t (id, name) VALUES ('a', 'x'), ('b', 'y')");
    db.storage().flush().unwrap();
    sql(&db, "ALTER TABLE t ADD COLUMN more JSON");
    sql(&db, r#"INSERT INTO t (id, name, more) VALUES ('c', 'z', '{"tier": "gold", "seats": 12}')"#);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE more.tier = 'gold'")), vec!["c"]);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE more.seats IS NULL")), vec!["a", "b"]);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE NOT (more.seats > 20)")), vec!["c"]);
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE more.seats BETWEEN 10 AND 12")), vec!["c"]);
}

#[test]
fn files_of_json_lines_fill_a_json_field() {
    let dir = tempfile::tempdir().unwrap();
    let db = table(&dir.path().join("i.strata"));
    let file = dir.path().join("rows.jsonl");
    std::fs::write(
        &file,
        concat!(
            r#"{"id": "1", "name": "a", "attrs": {"color": "red", "dims": {"w": 4}}}"#,
            "\n",
            r#"{"id": "2", "name": "b", "attrs": "{\"color\": \"blue\"}"}"#,
            "\n",
            r#"{"id": "3", "name": "c"}"#,
            "\n",
        ),
    )
    .unwrap();
    let got = strata::ingest::import_file_into(&db, "t", &file, None).unwrap();
    assert_eq!(got.rows, 3);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE attrs.color IN ('red', 'blue')")), vec!["1", "2"]);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE attrs.dims.w = 4.0")), vec!["1"]);
    assert_eq!(ids(sql(&db, "SELECT id FROM t WHERE attrs.color IS NULL")), vec!["3"]);
}

#[test]
fn plans_name_the_json_feature_and_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = table(&dir.path().join("p.strata"));
    sql(&db, r#"INSERT INTO t (id, attrs) VALUES ('a', '{"price": 9.5, "tags": ["x"]}'), ('b', '{"price": "n/a"}'), ('c', '{}')"#);
    let q = "SELECT id FROM t WHERE NOT (attrs.price > 5) OR attrs.tags = 'x'";
    let plan = db.explain(q, &Params::new()).unwrap();
    let j = plan.to_json().unwrap();
    assert!(j["features"].as_array().unwrap().iter().any(|f| f == "json"), "{j}");
    let text = sql(&db, &format!("EXPLAIN {q}")).into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("json_extract"), "{text}");
    // The plan as JSON runs as the SQL did.
    let back = strata::query::ir::Plan::from_json(&serde_json::from_str(&plan.to_json_text().unwrap()).unwrap()).unwrap();
    let SqlResult::Rows(rows) = db.run(&back).unwrap() else { panic!("rows") };
    assert_eq!(ids(rows.rows), ids(sql(&db, q)));
    assert_eq!(ids(sql(&db, q)), vec!["a"]);
}
