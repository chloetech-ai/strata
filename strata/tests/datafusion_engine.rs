//! DataFusion as the engine of plans' operators (`strata::query::df`,
//! feature `datafusion`) against Strata's own operators
//! (`strata::query::ops`): the same compound queries (joins, groups over
//! joined rows, `WITH`, `DISTINCT`, `DISTINCT ON`, sorts, limits, and
//! expressions over derived rows) give the same rows, keys and scores, in
//! the same order, on random tables with NULLs; and the queries the
//! lowering covers do run on DataFusion.
#![cfg(feature = "datafusion")]

use rand::{Rng, SeedableRng};
use strata::db::Db;
use strata::query::exec::{Engine, ExecOptions, ResultSet};
use strata::query::steps::Step;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::{Field, FieldType, Schema};

const PAPERS: usize = 400;
const AUTHORS: usize = 40;

fn papers() -> Schema {
    Schema::new(
        "papers",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: false }),
            Field::new("year", FieldType::Int),
            Field::new("venue", FieldType::Int),
            Field::new("author", FieldType::Keyword),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("weight", FieldType::Float),
            Field::new("cites", FieldType::Int),
        ],
        Some("id"),
    )
    .unwrap()
}

/// Papers with NULLs in every column but the key, venues 1 to 12 of which
/// 1 to 9 exist, authors some of whom are not in the authors table.
fn open(dir: &std::path::Path) -> Db {
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let opts = DbOptions { segment_max_docs: 150, sync: false, ..DbOptions::default() };
    let db = Db::create(&dir.join("t.strata"), papers(), opts).unwrap();
    let maybe = |rng: &mut rand::rngs::StdRng, v: Value| if rng.gen_range(0..8) == 0 { Value::Null } else { v };
    let topics = ["graph search", "vector indexes", "query planning", "learned ranking"];
    let rows: Vec<Vec<Value>> = (0..PAPERS)
        .map(|i| {
            let tags: Vec<Value> = (0..rng.gen_range(0..3)).map(|_| Value::Str(["db", "ir", "ml", "sys"][rng.gen_range(0..4)].into())).collect();
            let (year, venue, author) = (rng.gen_range(0..35), rng.gen_range(1..=12), rng.gen_range(0..AUTHORS + 5));
            let (weight, cites) = (rng.gen_range(0..1000) as f64 / 100.0, rng.gen_range(0..50) - 5);
            vec![
                Value::Str(format!("p{i:03}")),
                Value::Str(format!("paper {i} on {}", topics[i % 4])),
                maybe(&mut rng, Value::Int(1990 + year)),
                maybe(&mut rng, Value::Int(venue)),
                maybe(&mut rng, Value::Str(format!("a{author:02}"))),
                Value::List(tags),
                maybe(&mut rng, Value::Float(weight)),
                maybe(&mut rng, Value::Int(cites)),
            ]
        })
        .collect();
    db.load(rows).unwrap();
    db.sql("CREATE TABLE authors (id KEYWORD PRIMARY KEY, name KEYWORD, born INT, country KEYWORD)", &Params::new()).unwrap();
    let countries = ["cl", "us", "de", "jp"];
    db.load_into(
        "authors",
        (0..AUTHORS).map(|i| vec![Value::Str(format!("a{i:02}")), Value::Str(format!("Author {i}")), if i % 7 == 3 { Value::Null } else { Value::Int(1940 + (i as i64 * 37) % 60) }, Value::Str(countries[i % 4].into())]),
    )
    .unwrap();
    db.sql("CREATE TABLE venues (id INT PRIMARY KEY, name KEYWORD, rank FLOAT, field KEYWORD)", &Params::new()).unwrap();
    db.load_into("venues", (1..=9).map(|v: i64| vec![Value::Int(v), Value::Str(format!("venue{v}")), if v == 4 { Value::Null } else { Value::Float(1.0 + (v % 5) as f64) }, Value::Str(["db", "ir", "ml"][v as usize % 3].into())])).unwrap();
    db
}

/// The queries, each with whether DataFusion must run it (or the
/// operators may, for what the lowering does not cover yet).
const QUERIES: &[(&str, bool)] = &[
    // Joins.
    ("SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id", true),
    ("SELECT p.id, p.year, v.name, v.rank FROM papers p LEFT JOIN venues v ON p.venue = v.id", true),
    ("SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id WHERE v.rank >= 3 AND p.year > 2000", true),
    ("SELECT p.id, a.name FROM papers p JOIN authors a ON p.author = a.id WHERE MATCH(p.title, 'graph')", true),
    ("SELECT p.id, a.name, v.name FROM papers p JOIN authors a ON p.author = a.id LEFT JOIN venues v ON p.venue = v.id", true),
    ("SELECT p.id, v.id FROM papers p LEFT JOIN venues v ON p.venue = v.id AND v.rank > 2", true),
    ("SELECT a.id, b.id FROM papers a JOIN papers b ON a.author = b.author AND b.id != a.id WHERE a.year = 2001", true),
    ("SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id ORDER BY bm25(p.title, 'vector') DESC LIMIT 7", true),
    ("SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id ORDER BY p.year DESC, p.id LIMIT 15 OFFSET 5", true),
    // Groups over joined rows.
    ("SELECT v.field, COUNT(*), SUM(p.cites), AVG(p.weight), MIN(p.year), MAX(p.id) FROM papers p JOIN venues v ON p.venue = v.id GROUP BY v.field", true),
    ("SELECT a.country, COUNT(DISTINCT p.venue) AS n, AVG(p.cites) FROM papers p JOIN authors a ON p.author = a.id GROUP BY a.country ORDER BY n DESC, a.country", true),
    ("SELECT v.name, COUNT(*) AS n FROM papers p JOIN venues v ON p.venue = v.id GROUP BY v.name HAVING n > 30 ORDER BY n DESC", true),
    ("SELECT COUNT(*), SUM(p.weight), AVG(p.year) FROM papers p JOIN venues v ON p.venue = v.id WHERE v.rank > 100", true),
    ("SELECT v.field, a.country, COUNT(*) FROM papers p JOIN venues v ON p.venue = v.id JOIN authors a ON p.author = a.id GROUP BY v.field, a.country ORDER BY 3 DESC, 1, 2", true),
    ("SELECT p.year % 5 AS b, COUNT(*) FROM papers p JOIN venues v ON p.venue = v.id GROUP BY b ORDER BY b", true),
    // WITH and derived tables.
    ("WITH top AS (SELECT id, venue FROM papers ORDER BY bm25(title, 'graph') DESC LIMIT 50) SELECT v.field, COUNT(*) FROM top t JOIN venues v ON t.venue = v.id GROUP BY v.field", true),
    ("WITH old AS (SELECT id, author, year FROM papers WHERE year < 2000) SELECT a.id, b.id FROM old a JOIN old b ON a.author = b.author WHERE a.id < b.id", true),
    ("SELECT field, n FROM (SELECT v.field AS field, COUNT(*) AS n FROM papers p JOIN venues v ON p.venue = v.id GROUP BY v.field) AS t WHERE n > 10 ORDER BY n DESC", true),
    // DISTINCT and DISTINCT ON.
    ("SELECT DISTINCT v.field, a.country FROM papers p JOIN venues v ON p.venue = v.id JOIN authors a ON p.author = a.id", true),
    ("SELECT DISTINCT p.year FROM papers p JOIN venues v ON p.venue = v.id ORDER BY p.year DESC NULLS FIRST", true),
    ("SELECT DISTINCT ON (v.field) v.field, p.id, p.weight FROM papers p JOIN venues v ON p.venue = v.id ORDER BY p.weight DESC, p.id", true),
    ("SELECT DISTINCT ON (author) author, id, year FROM papers ORDER BY year DESC, id LIMIT 12", true),
    // Expressions over joined rows.
    ("SELECT p.id, p.year + p.cites, p.year - v.id, p.cites * v.id, p.cites / v.id, p.cites % 4, -p.cites FROM papers p JOIN venues v ON p.venue = v.id", true),
    ("SELECT p.id, p.weight * v.rank, p.weight / (v.rank - 2), p.weight + 1 FROM papers p JOIN venues v ON p.venue = v.id", true),
    ("SELECT p.id, CASE WHEN v.rank > 3 THEN 'high' WHEN v.rank > 1 THEN 'mid' ELSE 'low' END FROM papers p LEFT JOIN venues v ON p.venue = v.id", true),
    ("SELECT p.id, COALESCE(a.name, p.author, 'none'), LOWER(v.name), UPPER(a.country), LENGTH(p.author) FROM papers p LEFT JOIN venues v ON p.venue = v.id LEFT JOIN authors a ON p.author = a.id", true),
    ("SELECT p.id, ROUND(p.weight * v.rank, 1), ROUND(p.weight), ABS(p.cites - 20), ln(p.weight), pow(v.rank, 2), least(p.weight, v.rank) FROM papers p JOIN venues v ON p.venue = v.id", true),
    ("SELECT p.id, v.name || '-' || p.author FROM papers p JOIN venues v ON p.venue = v.id WHERE v.name || p.author LIKE 'venue%a1%' OR (v.name || p.author) ILIKE 'VENUE2%'", true),
    ("SELECT p.id FROM papers p JOIN venues v ON p.venue = v.id WHERE p.weight BETWEEN 2 AND 5 AND v.field IN ('db', 'ml') AND NOT (p.year IS NULL)", true),
    ("SELECT p.id, a.born FROM papers p LEFT JOIN authors a ON p.author = a.id WHERE a.born IS NULL ORDER BY p.id DESC LIMIT 20", true),
    ("SELECT p.id, v.rank FROM papers p JOIN venues v ON p.venue = v.id ORDER BY v.rank NULLS FIRST, p.cites DESC, p.id", true),
    // Not lowered yet: a multi-valued column as a join key, links, a
    // subquery in a leaf. They must still answer as the operators do.
    ("SELECT p.id, a.name FROM papers p JOIN authors a ON p.tags = a.id", false),
    ("SELECT id FROM papers WHERE venue IN (SELECT id FROM venues WHERE field = 'ir') ORDER BY id", false),
    // Subqueries in a leaf, over a WITH result or another table.
    ("WITH ir AS (SELECT id FROM venues WHERE field = 'ir') SELECT p.id, a.name FROM papers p JOIN authors a ON p.author = a.id WHERE p.venue IN (SELECT id FROM ir)", true),
    ("SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id WHERE p.year > (SELECT AVG(year) FROM papers) AND EXISTS (SELECT id FROM authors WHERE country = 'cl')", true),
    ("SELECT DISTINCT tags FROM papers", false),
];

fn run(db: &Db, sql: &str, engine: Engine) -> ResultSet {
    let opts = ExecOptions { engine, ..db.exec.clone() };
    db.query_with(sql, &Params::new(), &opts).unwrap_or_else(|e| panic!("{sql} ({engine:?}): {e}"))
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x == y || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0) || (x.is_nan() && y.is_nan()),
        (Value::List(x), Value::List(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same_value(a, b)),
        _ => a == b,
    }
}

fn same(sql: &str, want: &ResultSet, got: &ResultSet) {
    assert_eq!(want.columns, got.columns, "{sql}: columns");
    assert_eq!(want.rows.len(), got.rows.len(), "{sql}: rows");
    for (i, (w, g)) in want.rows.iter().zip(&got.rows).enumerate() {
        assert!(w.len() == g.len() && w.iter().zip(g).all(|(a, b)| same_value(a, b)), "{sql}: row {i}: operators {w:?}, datafusion {g:?}");
    }
    assert_eq!(want.keys, got.keys, "{sql}: keys");
    assert_eq!(want.scores.len(), got.scores.len(), "{sql}: scores");
    for (i, (w, g)) in want.scores.iter().zip(&got.scores).enumerate() {
        assert!((w - g).abs() <= 1e-9 * w.abs().max(1.0), "{sql}: score {i}: {w} vs {g}");
    }
}

fn on_datafusion(rs: &ResultSet) -> bool {
    !rs.stats.plan.iter().any(|l| l.starts_with("operators (not datafusion"))
}

#[test]
fn datafusion_answers_as_the_operators() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let mut fell_back = Vec::new();
    for &(sql, must) in QUERIES {
        let want = run(&db, sql, Engine::Operators);
        let got = run(&db, sql, Engine::DataFusion);
        same(sql, &want, &got);
        // The leaves' counts, summed as the operators sum them.
        assert_eq!((want.stats.filter_docs, want.stats.scored), (got.stats.filter_docs, got.stats.scored), "{sql}: filtered and scored rows");
        if !on_datafusion(&got) {
            assert!(!must, "{sql}: ran on the operators: {:?}", got.stats.plan.first());
            fell_back.push(sql);
        }
    }
    assert!(!fell_back.is_empty() || QUERIES.iter().all(|q| q.1));
}

#[test]
fn integer_overflow_falls_back_to_a_float() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let sql = "SELECT p.id, p.cites * 9223372036854775807 FROM papers p JOIN venues v ON p.venue = v.id WHERE p.cites > 1";
    let want = run(&db, sql, Engine::Operators);
    let got = run(&db, sql, Engine::DataFusion);
    same(sql, &want, &got);
    assert!(!on_datafusion(&got), "an integer overflow is the operators' FLOAT");
    assert!(want.rows.iter().all(|r| matches!(r[1], Value::Float(_))));
}

#[test]
fn a_join_past_the_operators_row_cap() {
    // 1,100 x 1,100 rows on one key: 1,210,000 joined rows, more than the
    // operators hold.
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::new("l", vec![Field::new("id", FieldType::Int), Field::new("k", FieldType::Int)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("big.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load((0..1_100).map(|i| vec![Value::Int(i), Value::Int(1)])).unwrap();
    db.sql("CREATE TABLE r (id INT PRIMARY KEY, k INT)", &Params::new()).unwrap();
    db.load_into("r", (0..1_100).map(|i| vec![Value::Int(i), Value::Int(1)])).unwrap();
    let sql = "SELECT COUNT(*), SUM(r.id) FROM l JOIN r ON l.k = r.k";
    let opts = ExecOptions { engine: Engine::Operators, ..db.exec.clone() };
    assert!(db.query_with(sql, &Params::new(), &opts).is_err(), "the operators refuse more than 1,000,000 joined rows");
    let got = run(&db, sql, Engine::DataFusion);
    assert!(on_datafusion(&got));
    assert_eq!(got.rows, vec![vec![Value::Int(1_210_000), Value::Int(1_100 * (1_099 * 1_100 / 2))]]);
}

#[test]
fn a_join_on_many_keys_reads_its_right_side_whole() {
    // 3,000 left keys against 4,000 right rows: restricting the right
    // side to the keys would cost more than reading it, so it is read
    // whole and hashed; the rows are the operators' all the same. Few keys
    // still restrict it, and a right side under a LIMIT (whose rows depend
    // on it) is restricted where the operators restrict it.
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::new("l", vec![Field::new("id", FieldType::Int), Field::new("g", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("keys.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load((0..3_000).map(|i| vec![Value::Int(i), Value::Str(format!("g{}", i % 7))])).unwrap();
    db.sql("CREATE TABLE r (id INT PRIMARY KEY, l_id INT, x FLOAT)", &Params::new()).unwrap();
    db.load_into("r", (0..4_000).map(|i| vec![Value::Int(i), if i % 9 == 0 { Value::Null } else { Value::Int((i * 7) % 3_500) }, Value::Float(i as f64 / 4.0)])).unwrap();
    let plan_line = |rs: &ResultSet| rs.stats.plan.iter().find(|l| l.starts_with("join:")).cloned().unwrap_or_default();
    for (sql, restricted) in [
        ("SELECT l.g, COUNT(*), SUM(r.x) FROM l JOIN r ON r.l_id = l.id GROUP BY l.g", false),
        ("SELECT l.id, r.id FROM l LEFT JOIN r ON r.l_id = l.id WHERE l.id < 2900", false),
        ("SELECT l.id, r.x FROM l JOIN r ON r.l_id = l.id WHERE r.x > 100 ORDER BY r.x DESC, l.id", false),
        ("SELECT l.id, t.x FROM l JOIN (SELECT l_id, x FROM r ORDER BY x DESC LIMIT 50) t ON t.l_id = l.id", true),
        ("SELECT l.id, r.x FROM l JOIN r ON r.l_id = l.id WHERE l.id < 40", true),
    ] {
        let want = run(&db, sql, Engine::Operators);
        let got = run(&db, sql, Engine::DataFusion);
        same(sql, &want, &got);
        assert!(on_datafusion(&got), "{sql}: {:?}", got.stats.plan.first());
        let restricted = restricted && plan_line(&want).contains("restricted to");
        assert_eq!(plan_line(&got).contains("restricted to"), restricted, "{sql}: {}", plan_line(&got));
    }
}

/// Two tables of 1,100 rows on one key: their join holds 1,210,000 rows.
fn big_join(dir: &std::path::Path) -> Db {
    let schema = Schema::new("l", vec![Field::new("id", FieldType::Int), Field::new("k", FieldType::Int)], Some("id")).unwrap();
    let db = Db::create(&dir.join("big.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load((0..1_100).map(|i| vec![Value::Int(i), Value::Int(1)])).unwrap();
    db.sql("CREATE TABLE r (id INT PRIMARY KEY, k INT)", &Params::new()).unwrap();
    db.load_into("r", (0..1_100).map(|i| vec![Value::Int(i), Value::Int(1)])).unwrap();
    db
}

#[test]
fn datafusion_memory_counts_against_the_query_budget() {
    let dir = tempfile::tempdir().unwrap();
    let db = big_join(dir.path());
    // A group a joined row: 1,210,000 groups, tens of MiB.
    let sql = "SELECT COUNT(*) FROM (SELECT l.id, r.id FROM l JOIN r ON l.k = r.k GROUP BY l.id, r.id) t";
    let budget = strata::query::memory::MemoryBudget::new(1 << 30);
    let opts = ExecOptions { engine: Engine::DataFusion, memory: Some(budget.clone()), ..db.exec.clone() };
    let rs = db.query_with(sql, &Params::new(), &opts).unwrap();
    assert!(on_datafusion(&rs));
    assert_eq!(rs.rows, vec![vec![Value::Int(1_210_000)]]);
    assert_eq!(budget.used(), 0, "given back when the query ends");
    assert_eq!(budget.trips(), 0);
    // Past a budget of 2 MiB the query fails as the operators' would, with
    // Strata's memory error, and does not run again on the operators.
    let small = strata::query::memory::MemoryBudget::new(2 << 20);
    let opts = ExecOptions { engine: Engine::DataFusion, memory: Some(small.clone()), ..db.exec.clone() };
    match db.query_with(sql, &Params::new(), &opts) {
        Err(strata::Error::Memory(m)) => assert!(m.contains("MiB"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(small.trips() >= 1);
    assert_eq!(small.used(), 0);
}

#[test]
fn a_datafusion_plan_stops_at_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let db = big_join(dir.path());
    let sql = "SELECT l.id, r.id FROM l JOIN r ON l.k = r.k ORDER BY l.id DESC, r.id DESC LIMIT 5";
    let opts = ExecOptions { engine: Engine::DataFusion, deadline: Some(std::time::Instant::now() + std::time::Duration::from_millis(30)), ..db.exec.clone() };
    let t = std::time::Instant::now();
    match db.query_with(sql, &Params::new(), &opts) {
        Err(strata::Error::Timeout(_)) => {}
        Ok(rs) => assert!(t.elapsed() < std::time::Duration::from_millis(30), "answered past the deadline: {:?}", rs.rows),
        Err(e) => panic!("{e}"),
    }
    assert!(t.elapsed() < std::time::Duration::from_secs(2), "stopped soon after the deadline: {:?}", t.elapsed());
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let opts = ExecOptions { engine: Engine::DataFusion, cancel: Some(cancel), ..db.exec.clone() };
    assert!(matches!(db.query_with(sql, &Params::new(), &opts), Err(strata::Error::Timeout(_))));
}

/// The steps directly inside the step at `at`.
fn children(steps: &[Step], at: usize) -> Vec<usize> {
    let d = steps[at].depth;
    steps[at + 1..].iter().enumerate().take_while(|(_, s)| s.depth > d).filter(|(_, s)| s.depth == d + 1).map(|(i, _)| at + 1 + i).collect()
}

#[test]
fn a_datafusion_plan_s_profile_holds_its_leaves_and_operators() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let sql = "SELECT a.country, COUNT(*) FROM papers p JOIN authors a ON p.author = a.id WHERE MATCH(p.title, 'graph') GROUP BY a.country";
    let opts = ExecOptions { engine: Engine::DataFusion, profile: true, ..db.exec.clone() };
    let rs = db.query_with(sql, &Params::new(), &opts).unwrap();
    assert!(on_datafusion(&rs), "{:?}", rs.stats.plan);
    let st = &rs.stats.steps;
    assert!(!st.is_empty() && st[0].depth == 0);
    for w in st.windows(2) {
        assert!(w[1].depth <= w[0].depth + 1, "{st:#?}");
    }
    for i in 0..st.len() {
        for c in children(st, i) {
            let (p, s) = (&st[i], &st[c]);
            assert!(s.start_us + 0.3 >= p.start_us && s.start_us + s.us <= p.start_us + p.us + 0.3, "{s:?} is not inside {p:?}");
        }
    }
    // Each leaf, its executor's steps inside.
    let leaves: Vec<usize> = (0..st.len()).filter(|&i| st[i].op == "leaf").collect();
    let tables: Vec<&str> = leaves.iter().map(|&i| st[i].detail.rsplit(": ").next().unwrap()).collect();
    assert_eq!(tables, ["papers", "authors"], "{st:#?}");
    assert!(leaves.iter().all(|&i| !children(st, i).is_empty()), "{st:#?}");
    assert!(leaves.iter().all(|&i| st[i].rows.is_some_and(|n| n > 0)) && st[leaves[1]].rows <= Some(AUTHORS as u64), "{st:#?}");
    // DataFusion's plans (the left side's rows, for the values that
    // restrict the right side's leaf, then the answer's), their operators
    // inside, with their rows and CPU time.
    let df = st.iter().rposition(|s| s.op == "datafusion").unwrap_or_else(|| panic!("no datafusion step in {st:#?}"));
    assert_eq!(st[df].rows, Some(rs.rows.len() as u64));
    assert!(st[df].cpu_us.is_some());
    let ops: Vec<&Step> = st[df + 1..].iter().take_while(|s| s.depth > st[df].depth).collect();
    assert!(ops.iter().any(|s| s.op == "hash join" && s.rows.is_some()), "{ops:#?}");
    assert!(ops.iter().any(|s| s.op == "aggregate" && s.detail.contains("gby")), "{ops:#?}");
    assert!(ops.iter().all(|s| s.cpu_us.is_some()), "{ops:#?}");
}

#[test]
fn auto_leaves_small_plans_to_the_operators_and_runs_large_ones_on_datafusion() {
    let ran_on_datafusion = |rs: &ResultSet| rs.stats.plan.iter().any(|l| l.starts_with("datafusion:"));
    // The papers and their tables: at most 4,096 rows read by the leaves,
    // the operators answer, and do not run again the leaves DataFusion ran.
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    for &(sql, _) in QUERIES {
        let want = run(&db, sql, Engine::Operators);
        let got = run(&db, sql, Engine::Auto);
        same(sql, &want, &got);
        assert_eq!((want.stats.filter_docs, want.stats.scored), (got.stats.filter_docs, got.stats.scored), "{sql}: filtered and scored rows");
        assert!(!ran_on_datafusion(&got), "{sql}: {:?}", got.stats.plan);
    }
    let sql = "SELECT a.country, COUNT(*) FROM papers p JOIN authors a ON p.author = a.id WHERE MATCH(p.title, 'graph') GROUP BY a.country";
    let opts = ExecOptions { engine: Engine::Auto, profile: true, ..db.exec.clone() };
    let st = db.query_with(sql, &Params::new(), &opts).unwrap().stats.steps;
    let leaves: Vec<usize> = (0..st.len()).filter(|&i| st[i].op == "leaf").collect();
    let searched: Vec<&str> = leaves.iter().filter(|&&i| !children(&st, i).is_empty()).map(|&i| st[i].detail.as_str()).collect();
    assert_eq!(searched, ["scan: papers", "join: authors"], "each leaf searched once: {st:#?}");
    assert_eq!(leaves.len(), 4, "the operators' leaves, given the rows DataFusion's read: {st:#?}");
    assert!(st.iter().all(|s| s.op != "datafusion"), "{st:#?}");

    // 3,000 rows joined with 4,000: DataFusion.
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::new("l", vec![Field::new("id", FieldType::Int), Field::new("g", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("keys.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load((0..3_000).map(|i| vec![Value::Int(i), Value::Str(format!("g{}", i % 7))])).unwrap();
    db.sql("CREATE TABLE r (id INT PRIMARY KEY, l_id INT, x FLOAT)", &Params::new()).unwrap();
    db.load_into("r", (0..4_000).map(|i| vec![Value::Int(i), Value::Int((i * 7) % 3_500), Value::Float(i as f64 / 4.0)])).unwrap();
    for (sql, big) in [
        ("SELECT l.g, COUNT(*), SUM(r.x) FROM l JOIN r ON r.l_id = l.id GROUP BY l.g", true),
        ("SELECT l.id, r.x FROM l JOIN r ON r.l_id = l.id WHERE l.id < 40", false),
    ] {
        let want = run(&db, sql, Engine::Operators);
        let got = run(&db, sql, Engine::Auto);
        same(sql, &want, &got);
        assert_eq!(ran_on_datafusion(&got), big, "{sql}: {:?}", got.stats.plan);
    }
}

// A leaf DataFusion reads as Arrow columns (`Db::execute_leaf`) holds what
// its rows hold (`Db::execute`): every type, NULLs, rows written since the
// last flush and rows changed since, keys and scores.
#[test]
fn a_leaf_s_arrow_columns_hold_what_its_rows_hold() {
    use strata::query::arrow;
    use strata::query::exec::Leaf;
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { segment_max_docs: 150, sync: false, ..DbOptions::default() };
    let first = Schema::new("l", vec![Field::new("id", FieldType::Int)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("t.strata"), first, opts).unwrap();
    let none = Params::new();
    db.sql("CREATE TABLE t (id KEYWORD PRIMARY KEY, n INT, x FLOAT, k KEYWORD, body TEXT, ok BOOL, at TIMESTAMP, v VECTOR(3), tags KEYWORD[], nums INT[], j JSON)", &none).unwrap();
    let mut rng = rand::rngs::StdRng::seed_from_u64(11);
    let insert = |db: &Db, rng: &mut rand::rngs::StdRng, from: usize, to: usize| {
        let rows: Vec<String> = (from..to)
            .map(|i| {
                let mut or_null = |s: String| if rng.gen_range(0..6) == 0 { "NULL".to_string() } else { s };
                let n = or_null(format!("{}", i % 17));
                let x = or_null(format!("{}.25", i % 13));
                let k = or_null(format!("'k{}'", i % 7));
                let body = or_null(format!("'row {i} on {}'", ["graphs", "vectors", "ranking"][i % 3]));
                let ok = or_null(["TRUE", "FALSE"][i % 2].to_string());
                let at = or_null(format!("'2026-10-{:02}T08:00:00Z'", 1 + i % 28));
                let v = or_null(format!("'[{}.0, 0.5, -1.0]'", i % 5));
                let tags = or_null(format!("ARRAY['t{}', 't{}']", i % 3, i % 4));
                let nums = or_null(format!("ARRAY[{}, {}]", i, i * 2));
                let j = or_null(format!("'{{\"a\": {i}, \"b\": \"x{}\"}}'", i % 3));
                format!("('r{i:04}', {n}, {x}, {k}, {body}, {ok}, {at}, {v}, {tags}, {nums}, {j})")
            })
            .collect();
        db.sql(&format!("INSERT INTO t (id, n, x, k, body, ok, at, v, tags, nums, j) VALUES {}", rows.join(", ")), &Params::new()).unwrap();
    };
    insert(&db, &mut rng, 0, 400);
    db.storage().flush().unwrap();
    insert(&db, &mut rng, 400, 450);
    db.sql("UPDATE t SET n = 99, k = 'changed' WHERE id IN ('r0001', 'r0007', 'r0130', 'r0420')", &none).unwrap();
    let queries = [
        "SELECT * FROM t",
        "SELECT id, n, x, k, ok, at, v, tags, nums, j, body FROM t WHERE n > 3",
        "SELECT k, id, x FROM t ORDER BY x DESC LIMIT 50",
        "SELECT n, x, k FROM t WHERE k IS NOT NULL",
        "SELECT id, score() FROM t ORDER BY bm25(body, 'graphs') DESC LIMIT 30",
        "SELECT body, id FROM t WHERE ok",
    ];
    for sql in queries {
        let snap = db.snapshot();
        let view = snap.table("t").unwrap();
        let select = strata::query::sql::bind(sql, view.schema(), &none).unwrap();
        let rs = db.execute(&view, &select, &db.exec).unwrap();
        assert!(rs.rows.len() > 1, "{sql}: {} rows", rs.rows.len());
        let Leaf::Arrow(c) = db.execute_leaf(&view, &select, &db.exec, 0).unwrap() else { panic!("{sql}: rows, not Arrow columns") };
        assert_eq!(c.names, rs.columns, "{sql}: names");
        assert_eq!(c.rows, rs.rows.len(), "{sql}: rows");
        let want = arrow::columns_of(rs.rows.clone(), rs.columns.len()).unwrap();
        for (i, (w, g)) in want.iter().zip(&c.arrays).enumerate() {
            assert_eq!(w.to_data(), g.to_data(), "{sql}: column {}", rs.columns[i]);
        }
        assert_eq!(arrow::column_of(rs.keys.clone()).unwrap().to_data(), c.keys.to_data(), "{sql}: keys");
        assert_eq!(rs.scores, c.scores, "{sql}: scores");
        // Over as many rows as it has, the leaf gives rows.
        assert!(matches!(db.execute_leaf(&view, &select, &db.exec, rs.rows.len()).unwrap(), Leaf::Rows(_)), "{sql}");
    }
}
