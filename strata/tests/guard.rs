//! `min_rows` and `max_rows` on `AT_LEAST`, `MATCH(..., min => n)` and
//! `bm25(..., min => n)` (term weights PR D): a guarded query keeps the
//! rows of the level that a reference picks from row counts it asks for
//! level by level, at fixed levels, over a table of two segments and the
//! memtable with rows rewritten and deleted across them.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::query::exec::ResultSet;
use strata::query::ir::{Plan, GUARD};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

const TABLE: &str = "CREATE TABLE t (id KEYWORD PRIMARY KEY, a INT, c KEYWORD, tags KEYWORD[], body TEXT)";
/// Words of `body`, from common to rare, and how often a row has each.
const WORDS: [(&str, f64); 6] = [("amber", 0.6), ("birch", 0.45), ("cedar", 0.3), ("dune", 0.2), ("elm", 0.1), ("fern", 0.05)];
const KEYS: [&str; 4] = ["x", "y", "z", "w"];

fn rows(db: &Db, s: &str) -> ResultSet {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r,
        SqlResult::Written(_) => panic!("{s} wrote"),
    }
}

/// A query's rows by key, with their scores: rows of a filter come in no
/// order.
fn sorted(r: &ResultSet) -> Vec<(String, u64)> {
    let mut v: Vec<(String, u64)> = r.keys.iter().zip(&r.scores).map(|(k, s)| (format!("{k:?}"), s.to_bits())).collect();
    v.sort();
    v
}

fn write(db: &Db, s: &str) {
    db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}"));
}

fn count(db: &Db, filter: &str) -> u64 {
    match rows(db, &format!("SELECT COUNT(*) FROM t WHERE {filter}")).rows[0][0] {
        Value::Int(n) => n as u64,
        ref v => panic!("COUNT(*) is {v:?}"),
    }
}

fn insert(db: &Db, rng: &mut StdRng, id: &str) {
    let body: Vec<&str> = WORDS.iter().filter(|(_, p)| rng.gen_bool(*p)).map(|(w, _)| *w).collect();
    let body = if body.is_empty() { "NULL".to_string() } else { format!("'{}'", body.join(" ")) };
    let tags: Vec<String> = KEYS.iter().filter(|_| rng.gen_ratio(1, 3)).map(|k| format!("'{k}'")).collect();
    let tags = if tags.is_empty() { "NULL".to_string() } else { format!("ARRAY[{}]", tags.join(", ")) };
    let a = if rng.gen_ratio(1, 6) { "NULL".to_string() } else { rng.gen_range(0..10).to_string() };
    let c = KEYS[rng.gen_range(0..KEYS.len())];
    write(
        db,
        &format!(
            "INSERT INTO t (id, a, c, tags, body) VALUES ('{id}', {a}, '{c}', {tags}, {body}) ON CONFLICT (id) DO UPDATE SET a = excluded.a, c = excluded.c, tags = excluded.tags, body = excluded.body"
        ),
    );
}

/// A table over two segments and the memtable, with rows rewritten and
/// deleted across them.
fn table(path: &std::path::Path, rng: &mut StdRng, n: usize) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(path, seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    write(&db, TABLE);
    for i in 0..n {
        insert(&db, rng, &format!("r{i:04}"));
        if i == n / 3 || i == 2 * n / 3 {
            db.storage().flush().unwrap();
        }
    }
    for i in (0..n).step_by(7) {
        insert(&db, rng, &format!("r{i:04}"));
    }
    for i in (0..n).step_by(11) {
        write(&db, &format!("DELETE FROM t WHERE id = 'r{i:04}'"));
    }
    db
}

/// The level the guard rule picks: from the strictest, `y`, relax while
/// the level has fewer than `min_rows` rows and the next looser one no
/// more than `max_rows`, down to `n` at most.
fn choose(n: usize, y: usize, rows: &dyn Fn(usize) -> u64, min_rows: Option<u64>, max_rows: Option<u64>) -> usize {
    let mut m = y;
    while m > n {
        let few = min_rows.is_none_or(|x| rows(m) < x);
        let room = max_rows.is_none_or(|x| rows(m - 1) <= x);
        if !(few && room) {
            break;
        }
        m -= 1;
    }
    m
}

/// What a random case guards: a condition of `WHERE` or the ranking.
enum Site {
    AtLeast(Vec<String>),
    Match(String),
    Bm25(String),
}

impl Site {
    fn random(rng: &mut StdRng) -> Site {
        let words = |rng: &mut StdRng| {
            let k = rng.gen_range(2..=WORDS.len());
            let mut ws: Vec<&str> = WORDS.iter().map(|(w, _)| *w).collect();
            while ws.len() > k {
                ws.remove(rng.gen_range(0..ws.len()));
            }
            ws.join(" ")
        };
        match rng.gen_range(0..3) {
            0 => {
                let k = rng.gen_range(2..=5);
                let conds = (0..k)
                    .map(|_| {
                        let key = KEYS[rng.gen_range(0..KEYS.len())];
                        match rng.gen_range(0..6) {
                            0 => format!("a > {}", rng.gen_range(0..9)),
                            1 => format!("a < {}", rng.gen_range(1..9)),
                            2 => format!("c = '{key}'"),
                            3 => format!("tags = '{key}'"),
                            4 => format!("MATCH(body, '{}')", WORDS[rng.gen_range(0..WORDS.len())].0),
                            _ => "a IS NULL".to_string(),
                        }
                    })
                    .collect();
                Site::AtLeast(conds)
            }
            1 => Site::Match(words(rng)),
            _ => Site::Bm25(words(rng)),
        }
    }

    fn items(&self) -> usize {
        match self {
            Site::AtLeast(c) => c.len(),
            Site::Match(t) | Site::Bm25(t) => t.split(' ').count(),
        }
    }

    /// The condition of level `m`, or of `need` with `opts`.
    fn cond(&self, need: &str, opts: &str) -> String {
        match self {
            Site::AtLeast(c) => format!("AT_LEAST({need}, {}{opts})", c.join(", ")),
            Site::Match(t) | Site::Bm25(t) => format!("MATCH(body, '{t}', min => {need}{opts})"),
        }
    }

    fn what(&self) -> &str {
        match self {
            Site::AtLeast(_) => "AT_LEAST",
            Site::Match(_) => "MATCH min",
            Site::Bm25(_) => "bm25 min",
        }
    }

    /// The query, guarded (`opts`) or at a fixed level.
    fn query(&self, rest: &str, need: &str, opts: &str) -> String {
        let and = |c: String| if rest.is_empty() { c } else { format!("{rest} AND {c}") };
        match self {
            Site::Bm25(t) => {
                let filter = if rest.is_empty() { String::new() } else { format!("WHERE {rest} ") };
                format!("SELECT id, score() FROM t {filter}ORDER BY bm25(body, '{t}', min => {need}{opts}) DESC LIMIT 30")
            }
            _ => format!("SELECT id FROM t WHERE {} LIMIT 2000", and(self.cond(need, opts))),
        }
    }
}

#[test]
fn a_guard_keeps_the_rows_of_the_level_its_counts_choose() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(0x9a7d);
    let db = table(&dir.path().join("g.strata"), &mut rng, 900);
    let mut chose = [false; 3];
    for case in 0..240 {
        let site = Site::random(&mut rng);
        let rest = ["", "a > 2", "c != 'x'", "tags = 'y'", "a IS NOT NULL"][rng.gen_range(0..5)];
        let y = site.items();
        let (need, n) = match rng.gen_bool(0.7) {
            true => {
                let c = rng.gen_range(1..=y + 1);
                (c.to_string(), c.clamp(1, y))
            }
            false => {
                let p = [25, 50, 75][rng.gen_range(0..3)];
                (format!("'{p}%'"), (y * p / 100).clamp(1, y))
            }
        };
        let and = |c: String| if rest.is_empty() { c } else { format!("{rest} AND {c}") };
        let c = |m: usize| count(&db, &and(site.cond(&m.to_string(), "")));
        let counts: Vec<u64> = (n..=y).map(c).collect();
        // Limits around the counts, so that each stops the relaxing somewhere.
        let near = |rng: &mut StdRng| {
            let x = counts[rng.gen_range(0..counts.len())];
            [x.saturating_sub(1), x, x + 1, 0, 100_000][rng.gen_range(0..5)]
        };
        let (min_rows, max_rows) = match rng.gen_range(0..3) {
            0 => (None, Some(near(&mut rng))),
            1 => (Some(near(&mut rng)), None),
            _ => (Some(near(&mut rng)), Some(near(&mut rng))),
        };
        let mut opts = String::new();
        if let Some(x) = min_rows {
            opts += &format!(", min_rows => {x}");
        }
        if let Some(x) = max_rows {
            opts += &format!(", max_rows => {x}");
        }
        let m = choose(n, y, &|m| counts[m - n], min_rows, max_rows);
        if n < y {
            chose[if m == n { 0 } else if m == y { 1 } else { 2 }] = true;
        }
        let guarded = site.query(rest, &need, &opts);
        let got = rows(&db, &guarded);
        let want = rows(&db, &site.query(rest, &m.to_string(), ""));
        assert_eq!(sorted(&got), sorted(&want), "case {case}: {guarded}: level {m} of {n} to {y}, counts {counts:?}");
        // The plan says which level ran, and why.
        let levels: Vec<String> = (n..=y).rev().map(|l| format!("{l}: {}", counts[l - n])).collect();
        let limits: Vec<String> = [min_rows.map(|x| format!("min_rows {x}")), max_rows.map(|x| format!("max_rows {x}"))].into_iter().flatten().collect();
        let over = if max_rows.is_some_and(|x| counts[y - n] > x) { ", over max_rows at every level" } else { "" };
        let line = format!("guard: {} {m} of {y} ({}; {}){over}", site.what(), levels.join(", "), limits.join(", "));
        assert!(got.stats.plan.contains(&line), "case {case}: {guarded}: {line:?} not in {:?}", got.stats.plan);
    }
    // Some cases relaxed all the way, some not at all, some in between.
    assert_eq!(chose, [true; 3]);
}

#[test]
fn over_max_rows_at_every_level_a_guard_keeps_the_strictest() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(7);
    let db = table(&dir.path().join("g.strata"), &mut rng, 300);
    let strict = count(&db, "AT_LEAST(2, MATCH(body, 'amber'), MATCH(body, 'birch'))");
    assert!(strict > 5);
    let got = rows(&db, "SELECT id FROM t WHERE AT_LEAST(1, MATCH(body, 'amber'), MATCH(body, 'birch'), max_rows => 5) LIMIT 1000");
    assert_eq!(got.keys.len() as u64, strict);
    let line = got.stats.plan.iter().find(|l| l.starts_with("guard: ")).unwrap();
    assert!(line.starts_with("guard: AT_LEAST 2 of 2 (") && line.ends_with("; max_rows 5), over max_rows at every level"), "{line}");
    // Without rows to count, a guard reads its loosest level.
    write(&db, "CREATE TABLE empty (id KEYWORD PRIMARY KEY, body TEXT)");
    let got = rows(&db, "SELECT id FROM empty WHERE MATCH(body, 'amber birch', min => 1, min_rows => 10) LIMIT 5");
    assert!(got.keys.is_empty() && got.stats.plan.iter().any(|l| l.starts_with("guard: MATCH min 1 of 2 (2: 0, 1: 0; min_rows 10)")), "{:?}", got.stats.plan);
}

#[test]
fn guards_are_refused_where_they_guard_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(3);
    let db = table(&dir.path().join("g.strata"), &mut rng, 50);
    for (q, msg) in [
        ("SELECT id FROM t WHERE AT_LEAST(1, a > 0, c = 'x', max_rows => 'many')", "max_rows => a count of rows (500)"),
        ("SELECT id FROM t WHERE AT_LEAST(1, a > 0, c = 'x', max_rows => 2.5)", "max_rows => a count of rows (500)"),
        ("SELECT id FROM t WHERE AT_LEAST(1, a > 0, c = 'x', rows => 10)", "unknown option rows for AT_LEAST: min_rows, max_rows"),
        ("SELECT id FROM t WHERE MATCH(body, 'amber birch', max_rows => 10)", "min_rows and max_rows on MATCH relax its min => n, which it needs"),
        ("SELECT id FROM t ORDER BY bm25(body, 'amber birch', min_rows => 10) DESC LIMIT 5", "relax its min => n, which it needs"),
        ("SELECT id FROM t WHERE a > 3 OR AT_LEAST(1, a > 0, c = 'x', max_rows => 10)", "joined to the others by AND, not one inside OR, NOT or another condition"),
        ("SELECT id FROM t WHERE NOT AT_LEAST(1, a > 0, c = 'x', max_rows => 10)", "joined to the others by AND"),
        ("SELECT id FROM t WHERE AT_LEAST(1, a > 0, MATCH(body, 'amber birch', min => 1, max_rows => 3))", "joined to the others by AND"),
        (
            "SELECT id FROM t WHERE AT_LEAST(1, a > 0, c = 'x', max_rows => 10) AND MATCH(body, 'amber birch', min => 1, max_rows => 10)",
            "a query has one guard (min_rows, max_rows) at most",
        ),
        ("SELECT bm25(body, 'amber birch', min => 1, max_rows => 10) AS s FROM t LIMIT 5", "min_rows and max_rows on bm25 guard the ranking of ORDER BY"),
    ] {
        let e = db.sql(q, &Params::new()).err().unwrap_or_else(|| panic!("{q} ran")).to_string();
        assert!(e.contains(msg), "{q}: {e}");
    }
}

#[test]
fn a_guard_over_a_with_counts_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(5);
    let db = table(&dir.path().join("g.strata"), &mut rng, 300);
    // Not a rerank phase, which a guard cannot be, but a search of the
    // rows of `top`: the guard counts those.
    let q = "WITH top AS (SELECT * FROM t ORDER BY bm25(body, 'amber birch') DESC LIMIT 20) SELECT id, score() FROM top ORDER BY bm25(body, 'amber birch cedar', min => 1, max_rows => 12) DESC LIMIT 5";
    let r = db.query(q, &Params::new()).unwrap();
    // A compound query's plan indents its leaves' lines.
    let line = r.stats.plan.iter().find(|l| l.trim_start().starts_with("guard: ")).unwrap_or_else(|| panic!("{:?}", r.stats.plan));
    let counts: Vec<u64> = line.split(['(', ';']).nth(1).unwrap().split(", ").map(|l| l.split(": ").nth(1).unwrap().parse().unwrap()).collect();
    assert!(counts.len() == 3 && counts.iter().all(|&c| c <= 20) && counts[2] == 20, "{line}");
}

#[test]
fn a_plan_carries_its_guard() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(11);
    let db = table(&dir.path().join("g.strata"), &mut rng, 200);
    let schema = db.snapshot().table("t").unwrap().schema().clone();
    for q in [
        "SELECT id FROM t WHERE a > 1 AND AT_LEAST(2, MATCH(body, 'amber'), c = 'x', tags = 'y', max_rows => 40) LIMIT 500",
        "SELECT id FROM t WHERE MATCH(body, 'amber birch cedar dune', min => '50%', min_rows => 20, max_rows => 90) LIMIT 500",
        "SELECT id, score() FROM t WHERE c != 'w' ORDER BY bm25(body, 'birch cedar dune elm', min => 1, max_rows => 30) DESC LIMIT 10",
    ] {
        let plan = db.explain(q, &Params::new()).unwrap();
        assert!(plan.features.iter().any(|f| f == GUARD), "{q}: {:?}", plan.features);
        let back = Plan::from_json(&plan.to_json().unwrap()).unwrap();
        assert_eq!(back, plan, "{q}");
        let by_sql = rows(&db, q);
        let SqlResult::Rows(by_plan) = db.run(&back).unwrap() else { panic!("{q} wrote") };
        assert_eq!(sorted(&by_plan), sorted(&by_sql), "{q}");
        assert_eq!(by_plan.stats.plan.iter().find(|l| l.starts_with("guard: ")), by_sql.stats.plan.iter().find(|l| l.starts_with("guard: ")), "{q}");
        let text = plan.explain(&[&schema]).join("\n");
        assert!(text.contains("guard: "), "{q}: {text}");
    }
    let plan = db.explain("SELECT id FROM t WHERE AT_LEAST(2, a > 0, c = 'x', tags = 'y') LIMIT 5", &Params::new()).unwrap();
    assert!(!plan.features.iter().any(|f| f == GUARD));
    let e = Plan::readable_by(1, &[GUARD], &["operators"]).unwrap_err().to_string();
    assert!(e.contains("guard"), "{e}");
}

