//! SQLancer's test oracles over random tables and queries, as Rigger and Su
//! use them to find logic bugs in database engines: Ternary Logic
//! Partitioning (TLP, OOPSLA 2020) and Non-optimizing Reference Engine
//! Construction (NoREC, ESEC/FSE 2020). Neither needs a second engine to
//! say what the answer is; each runs one query in several ways that must
//! agree.
//!
//! * **TLP.** A condition `p` splits every table into three parts: the rows
//!   where `p` is TRUE, where `NOT p` is TRUE, and where it is UNKNOWN. A
//!   query over the whole table answers what the same query over the three
//!   parts answers combined: rows by union, `COUNT` and `SUM` by sum, `MIN`
//!   and `MAX` by min and max, groups and `DISTINCT` values group by group,
//!   and a `HAVING` condition splits the groups the same way.
//! * **NoREC.** The same condition counted by `COUNT(*)`, returned row by
//!   row, and run over a derived table (`FROM (SELECT * FROM t) d`, which
//!   the engine filters, groups and sorts in memory, with no index) gives
//!   the same rows, counts and groups.
//!
//! On top of them: a page (`ORDER BY .. LIMIT .. OFFSET ..`) is the slice
//! of the whole order, the order is the one the rows' values give, and two
//! and three shards merged answer as the one database. The table has rows
//! in segments and in the memtable, deleted and updated rows, NULLs in
//! every column and multi-valued fields with no values.
//!
//! `STRATA_SQL_ORACLE_RUNS` scales the number of queries (1 by default).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use strata::cluster;
use strata::db::{Db, SqlResult};
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

const TABLE: &str = "CREATE TABLE t (id INT PRIMARY KEY, a INT, b FLOAT, c KEYWORD, d KEYWORD, flag BOOL, tags KEYWORD[])";

fn runs(n: usize) -> usize {
    let scale: f64 = std::env::var("STRATA_SQL_ORACLE_RUNS").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    ((n as f64) * scale).max(1.0) as usize
}

fn open(path: &std::path::Path, segment_max_docs: usize) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(path, seed, DbOptions { sync: false, segment_max_docs, ..DbOptions::default() }).unwrap();
    db.sql(TABLE, &Params::new()).unwrap();
    db
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

/// `v`, or NULL one time in five.
fn maybe(rng: &mut StdRng, v: String) -> String {
    if rng.gen_ratio(1, 5) { "NULL".into() } else { v }
}

/// Row `i`'s values as SQL literals.
fn row(rng: &mut StdRng, i: usize) -> String {
    const WORDS: [&str; 4] = ["x", "y", "z", "xy"];
    let a = rng.gen_range(-4..=4).to_string();
    let a = maybe(rng, a);
    let b = ["-1.5", "0.0", "0.5", "2.0", "2.25", "3.0"][rng.gen_range(0..6)].to_string();
    let b = maybe(rng, b);
    let c = format!("'{}'", WORDS[rng.gen_range(0..4)]);
    let c = maybe(rng, c);
    let d = format!("'{}'", ["p", "q"][rng.gen_range(0..2)]);
    let d = maybe(rng, d);
    let flag = ["TRUE", "FALSE"][rng.gen_range(0..2)].to_string();
    let flag = maybe(rng, flag);
    let tags: Vec<String> = WORDS[..3].iter().filter(|_| rng.gen_ratio(1, 3)).map(|w| format!("'{w}'")).collect();
    let tags = if tags.is_empty() { "NULL".into() } else { format!("ARRAY[{}]", tags.join(", ")) };
    format!("({i}, {a}, {b}, {c}, {d}, {flag}, {tags})")
}

/// `rows` rows into `dbs`, the first database whole and the others split
/// between them by id, in batches with flushes between (the last batch
/// stays in the memtable), then the same rows deleted and updated in each.
fn fill(dbs: &[&Db], seed: u64, rows: usize) {
    let mut rng = StdRng::seed_from_u64(seed);
    let all: Vec<String> = (0..rows).map(|i| row(&mut rng, i)).collect();
    // Some rows by id, one in `every` of them.
    let some = |rng: &mut StdRng, every: u32| {
        let ids: Vec<String> = (0..rows).filter(|_| rng.gen_ratio(1, every)).map(|i| i.to_string()).collect();
        if ids.is_empty() { "FALSE".to_string() } else { format!("id IN ({})", ids.join(", ")) }
    };
    let deletes: Vec<String> = (0..2).map(|_| some(&mut rng, 6)).collect();
    let updates: Vec<String> = (0..3)
        .map(|_| {
            let rng = &mut rng;
            let set = match rng.gen_range(0..4) {
                0 => format!("a = {}", rng.gen_range(-4..=4)),
                1 => "a = NULL".to_string(),
                2 => format!("b = {}", ["1.0", "-1.5", "NULL"][rng.gen_range(0..3)]),
                _ => format!("c = '{}'", ["x", "z"][rng.gen_range(0..2)]),
            };
            format!("UPDATE t SET {set} WHERE {}", some(rng, 5))
        })
        .collect();
    let parts = dbs.len().max(2) - 1;
    for (k, db) in dbs.iter().enumerate() {
        // Shards hold the rows whose id falls to them; the whole database
        // (k = 0) holds every row.
        let mine: Vec<&String> = all.iter().enumerate().filter(|(i, _)| k == 0 || i % parts == k - 1).map(|(_, r)| r).collect();
        for (n, batch) in mine.chunks(mine.len().div_ceil(4).max(1)).enumerate() {
            sql(db, &format!("INSERT INTO t (id, a, b, c, d, flag, tags) VALUES {}", batch.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
            // The last batch stays in the memtable.
            if n < 3 {
                db.storage().flush().unwrap();
            }
        }
        sql(db, &format!("DELETE FROM t WHERE {}", deletes[0]));
        for u in &updates {
            sql(db, u);
        }
        sql(db, &format!("DELETE FROM t WHERE {} AND a IS NULL", deletes[1]));
    }
}

/// A random condition over the table's columns, `depth` levels of AND, OR
/// and NOT deep.
fn cond(rng: &mut StdRng, depth: u32) -> String {
    const OPS: [&str; 6] = ["=", "!=", "<", "<=", ">", ">="];
    let op = OPS[rng.gen_range(0..6)];
    let word = |rng: &mut StdRng| ["x", "y", "z", "xy", "w"][rng.gen_range(0..5)];
    let pick = if depth == 0 { rng.gen_range(0..12) } else { rng.gen_range(0..16) };
    match pick {
        0 => format!("a {op} {}", rng.gen_range(-5..=5)),
        1 => format!("b {op} {:?}", [-2.0, 0.0, 0.5, 1.75, 2.25, 3.0][rng.gen_range(0..6)]),
        2 => format!("{} {} '{}'", ["c", "tags"][rng.gen_range(0..2)], ["=", "!="][rng.gen_range(0..2)], word(rng)),
        3 => {
            let ws: Vec<String> = (0..rng.gen_range(1..=3)).map(|_| format!("'{}'", word(rng))).collect();
            format!("{} {}IN ({})", ["c", "tags", "d"][rng.gen_range(0..3)], ["", "NOT "][rng.gen_range(0..2)], ws.join(", "))
        }
        4 => format!("d = '{}'", ["p", "q"][rng.gen_range(0..2)]),
        5 => ["flag", "NOT flag", "flag = TRUE", "flag = FALSE", "flag != TRUE"][rng.gen_range(0..5)].to_string(),
        6 | 7 => format!("{} IS {}NULL", ["a", "b", "c", "d", "flag", "tags"][rng.gen_range(0..6)], ["", "NOT "][rng.gen_range(0..2)]),
        8 => {
            let lo = rng.gen_range(-5..=3);
            format!("a {}BETWEEN {lo} AND {}", ["", "NOT "][rng.gen_range(0..2)], lo + rng.gen_range(0..=4))
        }
        9 => {
            let vs: Vec<String> = (0..rng.gen_range(1..=3)).map(|_| rng.gen_range(-4..=4).to_string()).collect();
            format!("a {}IN ({})", ["", "NOT "][rng.gen_range(0..2)], vs.join(", "))
        }
        10 => format!("c {}LIKE '{}%'", ["", "NOT "][rng.gen_range(0..2)], ["x", "y", "xy", ""][rng.gen_range(0..4)]),
        11 => format!("id {op} {}", rng.gen_range(0..300)),
        12 | 13 => format!("({} {} {})", cond(rng, depth - 1), ["AND", "OR"][(pick - 12) as usize], cond(rng, depth - 1)),
        _ => format!("NOT ({})", cond(rng, depth - 1)),
    }
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("{v:?}"))
}

/// The ids of `rows` (first column), sorted.
fn ids(rows: &[Vec<Value>]) -> Vec<i64> {
    let mut v: Vec<i64> = rows.iter().map(|r| int(&r[0])).collect();
    v.sort_unstable();
    v
}

/// The three parts of the table a condition splits it into: TRUE, FALSE
/// and UNKNOWN, as conditions to add to a query's own. UNKNOWN is the rows
/// left by the other two, named by id: SQL has no `IS UNKNOWN` here, and
/// naming them keeps that part an ordinary condition.
fn parts(db: &Db, p: &str, all: &[i64]) -> [String; 3] {
    let yes = ids(&sql(db, &format!("SELECT id FROM t WHERE {p}")));
    let no = ids(&sql(db, &format!("SELECT id FROM t WHERE NOT ({p})")));
    let both: BTreeSet<i64> = yes.iter().copied().collect::<BTreeSet<_>>().intersection(&no.iter().copied().collect()).copied().collect();
    assert!(both.is_empty(), "TLP: rows where both `{p}` and its negation are TRUE: {both:?}");
    let seen: BTreeSet<i64> = yes.iter().chain(&no).copied().collect();
    assert!(seen.iter().all(|i| all.binary_search(i).is_ok()), "TLP: `{p}` or its negation returns rows the table does not have");
    let unknown: Vec<String> = all.iter().filter(|i| !seen.contains(i)).map(|i| i.to_string()).collect();
    let unknown = if unknown.is_empty() { "FALSE".to_string() } else { format!("id IN ({})", unknown.join(", ")) };
    [format!("({p})"), format!("NOT ({p})"), unknown]
}

/// The aggregates of a group, combinable across parts: COUNT(*), COUNT(b),
/// SUM(a), MIN(a), MAX(b), and SUM(b) (compared to 1e-9).
#[derive(Clone, Debug, PartialEq)]
struct Aggs {
    rows: i64,
    count_b: i64,
    sum_a: Option<i64>,
    min_a: Option<i64>,
    max_b: Option<f64>,
    sum_b: Option<f64>,
}

const AGGS: &str = "COUNT(*), COUNT(b), SUM(a), MIN(a), MAX(b), SUM(b)";

impl Aggs {
    fn of(r: &[Value]) -> Aggs {
        let f = |v: &Value| match v {
            Value::Null => None,
            v => Some(v.as_f64().unwrap_or_else(|| panic!("{v:?}"))),
        };
        Aggs { rows: int(&r[0]), count_b: int(&r[1]), sum_a: f(&r[2]).map(|x| x as i64), min_a: f(&r[3]).map(|x| x as i64), max_b: f(&r[4]), sum_b: f(&r[5]) }
    }

    fn add(&self, o: &Aggs) -> Aggs {
        fn join<T: Copy>(a: Option<T>, b: Option<T>, f: impl Fn(T, T) -> T) -> Option<T> {
            match (a, b) {
                (Some(x), Some(y)) => Some(f(x, y)),
                (x, None) => x,
                (None, y) => y,
            }
        }
        Aggs {
            rows: self.rows + o.rows,
            count_b: self.count_b + o.count_b,
            sum_a: join(self.sum_a, o.sum_a, |x, y| x + y),
            min_a: join(self.min_a, o.min_a, i64::min),
            max_b: join(self.max_b, o.max_b, f64::max),
            sum_b: join(self.sum_b, o.sum_b, |x, y| x + y),
        }
    }

    fn same(&self, o: &Aggs) -> bool {
        let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(x), Some(y)) => (x - y).abs() <= 1e-9 * x.abs().max(1.0),
            (x, y) => x == y,
        };
        self.rows == o.rows && self.count_b == o.count_b && self.sum_a == o.sum_a && self.min_a == o.min_a && self.max_b == o.max_b && close(self.sum_b, o.sum_b)
    }
}

/// Groups by their keys (the first `keys` columns, as text), each with its
/// aggregates (the columns after).
fn groups(rows: &[Vec<Value>], keys: usize) -> BTreeMap<String, Aggs> {
    let mut out = BTreeMap::new();
    for r in rows {
        let k = format!("{:?}", &r[..keys]);
        assert!(out.insert(k.clone(), Aggs::of(&r[keys..])).is_none(), "group {k} twice");
    }
    out
}

fn same_groups(what: &str, want: &BTreeMap<String, Aggs>, got: &BTreeMap<String, Aggs>) {
    let keys = |m: &BTreeMap<String, Aggs>| m.keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(want), keys(got), "{what}: groups");
    for (k, w) in want {
        assert!(w.same(&got[k]), "{what}: group {k}: {w:?} against {:?}", got[k]);
    }
}

/// The parts' groups merged.
fn merged(per_part: &[BTreeMap<String, Aggs>]) -> BTreeMap<String, Aggs> {
    let mut out: BTreeMap<String, Aggs> = BTreeMap::new();
    for m in per_part {
        for (k, a) in m {
            let v = match out.get(k) {
                Some(b) => b.add(a),
                None => a.clone(),
            };
            out.insert(k.clone(), v);
        }
    }
    out
}

/// The group keys the queries try: one column, two, and a multi-valued one
/// (a row in each of its values' groups).
const GROUP_KEYS: [(&str, usize); 4] = [("c", 1), ("d, flag", 2), ("tags", 1), ("a", 1)];

/// TLP over a condition `p`: rows, aggregates over all rows, groups and
/// DISTINCT values, each over the whole table against its three parts.
fn tlp(db: &Db, p: &str, extra: &str, all: &[i64]) {
    let parts = parts(db, p, all);
    let wh = |c: &str| if extra.is_empty() { c.to_string() } else { format!("{extra} AND {c}") };
    let whole = if extra.is_empty() { "TRUE".to_string() } else { extra.to_string() };
    // Rows.
    let mut union: Vec<i64> = parts.iter().flat_map(|c| ids(&sql(db, &format!("SELECT id FROM t WHERE {}", wh(c))))).collect();
    union.sort_unstable();
    assert_eq!(ids(&sql(db, &format!("SELECT id FROM t WHERE {whole}"))), union, "TLP rows: {p} (within {whole})");
    // Aggregates over all rows.
    let one = |c: &str| Aggs::of(&sql(db, &format!("SELECT {AGGS} FROM t WHERE {c}"))[0]);
    let want = one(&whole);
    let got = parts.iter().map(|c| one(&wh(c))).reduce(|a, b| a.add(&b)).unwrap();
    assert!(want.same(&got), "TLP aggregates: {p} (within {whole}): {want:?} against {got:?}");
    // Groups and DISTINCT.
    for (keys, n) in GROUP_KEYS {
        let q = |c: &str| groups(&sql(db, &format!("SELECT {keys}, {AGGS} FROM t WHERE {c} GROUP BY {keys}")), n);
        same_groups(&format!("TLP GROUP BY {keys}: {p} (within {whole})"), &q(&whole), &merged(&parts.iter().map(|c| q(&wh(c))).collect::<Vec<_>>()));
        let d = |c: &str| sql(db, &format!("SELECT DISTINCT {keys} FROM t WHERE {c}")).into_iter().map(|r| format!("{r:?}")).collect::<BTreeSet<_>>();
        let want = d(&whole);
        let got: BTreeSet<String> = parts.iter().flat_map(|c| d(&wh(c))).collect();
        assert_eq!(want, got, "TLP DISTINCT {keys}: {p} (within {whole})");
    }
}

/// TLP over a HAVING condition: the groups it keeps, the groups its
/// negation keeps and the groups where it is UNKNOWN are every group.
fn tlp_having(db: &Db, h: &str, keys: &str, n: usize) {
    let q = |c: &str| groups(&sql(db, &format!("SELECT {keys}, {AGGS} FROM t GROUP BY {keys}{c}")), n);
    let all = q("");
    let yes = q(&format!(" HAVING {h}"));
    let no = q(&format!(" HAVING NOT ({h})"));
    for (k, a) in yes.iter().chain(&no) {
        assert!(all.get(k).is_some_and(|w| w.same(a)), "TLP HAVING {h}: group {k} {a:?} is not the group by itself");
    }
    let both: Vec<&String> = yes.keys().filter(|k| no.contains_key(*k)).collect();
    assert!(both.is_empty(), "TLP HAVING {h}: groups where it and its negation hold: {both:?}");
}

/// A random HAVING condition over the aggregates.
fn having(rng: &mut StdRng) -> String {
    let op = ["=", "!=", "<", "<=", ">", ">="][rng.gen_range(0..6)];
    let one = match rng.gen_range(0..5) {
        0 => format!("COUNT(*) {op} {}", rng.gen_range(0..40)),
        1 => format!("SUM(a) {op} {}", rng.gen_range(-10..20)),
        2 => format!("MIN(a) {op} {}", rng.gen_range(-4..=4)),
        3 => format!("AVG(b) {op} {:?}", [0.0, 0.5, 1.0, 1.5][rng.gen_range(0..4)]),
        _ => format!("MAX(b) {op} {:?}", [0.0, 2.0, 3.0][rng.gen_range(0..3)]),
    };
    match rng.gen_range(0..3) {
        0 => format!("NOT ({one})"),
        1 => format!("{one} AND COUNT(b) > {}", rng.gen_range(0..5)),
        _ => one,
    }
}

/// NoREC: one condition counted, returned and run over a derived table
/// (filtered, grouped and counted in memory, with no index) answers alike.
fn norec(db: &Db, p: &str) {
    let rows = ids(&sql(db, &format!("SELECT id FROM t WHERE {p}")));
    let count = int(&sql(db, &format!("SELECT COUNT(*) FROM t WHERE {p}"))[0][0]);
    assert_eq!(count as usize, rows.len(), "NoREC COUNT(*): {p}");
    let derived = ids(&sql(db, &format!("SELECT id FROM (SELECT * FROM t) d WHERE {p}")));
    assert_eq!(derived, rows, "NoREC derived table: {p}");
    let counted = int(&sql(db, &format!("SELECT COUNT(*) FROM (SELECT * FROM t WHERE {p}) d"))[0][0]);
    assert_eq!(counted as usize, rows.len(), "NoREC count of a derived table: {p}");
    let with = ids(&sql(db, &format!("WITH w AS (SELECT * FROM t WHERE {p}) SELECT id FROM w")));
    assert_eq!(with, rows, "NoREC WITH: {p}");
    for (keys, n) in GROUP_KEYS {
        let direct = groups(&sql(db, &format!("SELECT {keys}, {AGGS} FROM t WHERE {p} GROUP BY {keys}")), n);
        let mem = groups(&sql(db, &format!("SELECT {keys}, {AGGS} FROM (SELECT * FROM t) d WHERE {p} GROUP BY {keys}")), n);
        same_groups(&format!("NoREC GROUP BY {keys}: {p}"), &direct, &mem);
        let distinct = |from: &str| sql(db, &format!("SELECT DISTINCT {keys} FROM {from} WHERE {p}")).into_iter().map(|r| format!("{r:?}")).collect::<BTreeSet<_>>();
        assert_eq!(distinct("t"), distinct("(SELECT * FROM t) d"), "NoREC DISTINCT {keys}: {p}");
    }
}

/// SQL's order of one sort key: NULL last whichever the direction, unless
/// NULLS FIRST.
fn sort_rows(rows: &mut [Vec<Value>], col: usize, desc: bool, nulls_first: bool) {
    rows.sort_by(|x, y| {
        let (a, b) = (&x[col], &y[col]);
        let by_value = match (a, b) {
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            (Value::Null, _) => if nulls_first { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater },
            (_, Value::Null) => if nulls_first { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less },
            (Value::Str(p), Value::Str(q)) => if desc { q.cmp(p) } else { p.cmp(q) },
            (p, q) => {
                let o = p.as_f64().unwrap().total_cmp(&q.as_f64().unwrap());
                if desc { o.reverse() } else { o }
            }
        };
        by_value.then(int(&x[0]).cmp(&int(&y[0])))
    });
}

/// Pages: the rows a condition keeps sorted by a column and the id, the
/// whole order as the rows' values give it, and every page its slice.
fn pages(db: &Db, rng: &mut StdRng, p: &str) {
    let (col, name) = [(1, "a"), (2, "b"), (3, "c")][rng.gen_range(0..3)];
    let desc = rng.gen_bool(0.5);
    let nulls = ["", " NULLS FIRST", " NULLS LAST"][rng.gen_range(0..3)];
    let nulls_first = nulls == " NULLS FIRST";
    let dir = if desc { "DESC" } else { "ASC" };
    let order = format!("ORDER BY {name} {dir}{nulls}, id");
    let mut want = sql(db, &format!("SELECT id, a, b, c FROM t WHERE {p}"));
    sort_rows(&mut want, col, desc, nulls_first);
    let whole = sql(db, &format!("SELECT id, a, b, c FROM t WHERE {p} {order} LIMIT 100000"));
    assert_eq!(ids_in_order(&whole), ids_in_order(&want), "the order: {p} {order}");
    for _ in 0..3 {
        let (limit, offset) = (rng.gen_range(1..12), rng.gen_range(0..want.len().max(1) + 2));
        let page = sql(db, &format!("SELECT id, a, b, c FROM t WHERE {p} {order} LIMIT {limit} OFFSET {offset}"));
        let slice: Vec<i64> = ids_in_order(&whole).into_iter().skip(offset).take(limit).collect();
        assert_eq!(ids_in_order(&page), slice, "a page: {p} {order} LIMIT {limit} OFFSET {offset}");
    }
}

fn ids_in_order(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter().map(|r| int(&r[0])).collect()
}

/// The coordinator's rounds over `shards`.
fn sharded(shards: &[Db], q: &str) -> ResultSet {
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot().table("t").unwrap()).collect();
    let params = Params::new();
    let select = bind(q, snaps[0].schema(), &params).unwrap_or_else(|e| panic!("{q}: {e}"));
    let stats: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
    let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&stats))), shard: true, ..ExecOptions::default() };
    let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &select, &opts).unwrap_or_else(|e| panic!("{q}: {e}"))).collect();
    cluster::merge(&select, answers).unwrap_or_else(|e| panic!("{q}: {e}"))
}

#[test]
fn tlp_and_norec_over_random_conditions() {
    let dir = tempfile::tempdir().unwrap();
    for (seed, rows, seg) in [(1u64, 240, 40), (2, 90, 7), (3, 400, 1000)] {
        let db = open(&dir.path().join(format!("t{seed}.strata")), seg);
        fill(&[&db], seed, rows);
        let all = ids(&sql(&db, "SELECT id FROM t"));
        assert!(all.len() > rows / 2, "{} rows left of {rows}", all.len());
        let mut rng = StdRng::seed_from_u64(seed * 1000);
        for _ in 0..runs(60) {
            let p = cond(&mut rng, 3);
            tlp(&db, &p, "", &all);
            norec(&db, &p);
            pages(&db, &mut rng, &p);
        }
        // TLP within another condition: the parts of a filtered table.
        for _ in 0..runs(25) {
            let (p, q) = (cond(&mut rng, 2), cond(&mut rng, 2));
            tlp(&db, &p, &q, &all);
        }
        for _ in 0..runs(40) {
            let (keys, n) = GROUP_KEYS[rng.gen_range(0..GROUP_KEYS.len())];
            tlp_having(&db, &having(&mut rng), keys, n);
        }
    }
}

/// The same queries over two and three shards, merged by the coordinator's
/// code, answer as the whole table does.
#[test]
fn shards_answer_as_the_whole_table() {
    let dir = tempfile::tempdir().unwrap();
    for shards in [2usize, 3] {
        let whole = open(&dir.path().join(format!("w{shards}.strata")), 30);
        let parts: Vec<Db> = (0..shards).map(|i| open(&dir.path().join(format!("s{shards}-{i}.strata")), 30)).collect();
        let mut dbs: Vec<&Db> = vec![&whole];
        dbs.extend(parts.iter());
        fill(&dbs, 40 + shards as u64, 300);
        let mut rng = StdRng::seed_from_u64(77 + shards as u64);
        for _ in 0..runs(80) {
            let p = cond(&mut rng, 3);
            let (keys, n) = GROUP_KEYS[rng.gen_range(0..GROUP_KEYS.len())];
            let qs = [
                format!("SELECT id FROM t WHERE {p} LIMIT 100000"),
                format!("SELECT {AGGS} FROM t WHERE {p}"),
                format!("SELECT {keys}, {AGGS} FROM t WHERE {p} GROUP BY {keys}"),
                format!("SELECT DISTINCT {keys} FROM t WHERE {p}"),
            ];
            for (i, q) in qs.iter().enumerate() {
                let want = sql(&whole, q);
                let got = sharded(&parts, q).rows;
                match i {
                    0 => assert_eq!(ids(&want), ids(&got), "{shards} shards: {q}"),
                    1 => assert!(Aggs::of(&want[0]).same(&Aggs::of(&got[0])), "{shards} shards: {q}: {:?} against {:?}", want[0], got[0]),
                    2 => same_groups(&format!("{shards} shards: {q}"), &groups(&want, n), &groups(&got, n)),
                    _ => {
                        let set = |r: &[Vec<Value>]| r.iter().map(|x| format!("{x:?}")).collect::<BTreeSet<_>>();
                        assert_eq!(set(&want), set(&got), "{shards} shards: {q}");
                    }
                }
            }
        }
    }
}
