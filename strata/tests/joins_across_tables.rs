//! Queries across the tables of one database (multi-table phase 3): joins
//! on KEYWORD (multi-valued too), INT and FLOAT keys, LEFT JOIN, three
//! tables, subqueries and `WITH` over another table, groups, ranked cuts
//! and plans, against the same computed over the documents.

use std::collections::{BTreeMap, BTreeSet};

use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::graph::{edges_value, Edge};
use strata::query::ir::Plan;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::{Field, FieldType, Schema};

const N: usize = 300;
const AUTHORS: usize = 30;
const FIELDS: [&str; 3] = ["db", "ir", "ml"];

fn schema() -> Schema {
    Schema::new(
        "papers",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: false }),
            Field::new("year", FieldType::Int),
            Field::new("venue", FieldType::Int),
            Field::new("authors", FieldType::Keyword).multi(),
            Field::new("cites", FieldType::Edges),
            Field::new("weight", FieldType::Float),
        ],
        Some("id"),
    )
    .unwrap()
}

struct Paper {
    id: String,
    year: i64,
    venue: i64,
    authors: Vec<String>,
    weight: f64,
    cites: Vec<String>,
}

struct Author {
    id: String,
    born: i64,
}

struct Venue {
    id: i64,
    name: String,
    rank: f64,
    field: String,
}

struct Data {
    papers: Vec<Paper>,
    authors: Vec<Author>,
    venues: Vec<Venue>,
}

impl Data {
    fn venue(&self, id: i64) -> Option<&Venue> {
        self.venues.iter().find(|v| v.id == id)
    }

    fn author(&self, id: &str) -> Option<&Author> {
        self.authors.iter().find(|a| a.id == id)
    }
}

fn author_key(i: usize) -> String {
    format!("a{i:02}")
}

/// Papers in venues 1 to 10, of which 1 to 8 exist; with up to three
/// authors, some of whom are not in the authors table.
fn data() -> Data {
    let mut rng = rand::rngs::StdRng::seed_from_u64(11);
    let authors = (0..AUTHORS).map(|i| Author { id: author_key(i), born: 1940 + rng.gen_range(0..60) }).collect();
    let venues = (1..=8).map(|id| Venue { id, name: format!("venue{id}"), rank: 1.0 + (id % 5) as f64, field: FIELDS[id as usize % 3].to_string() }).collect();
    let mut links = rand::rngs::StdRng::seed_from_u64(5);
    let papers = (0..N)
        .map(|i| {
            // Papers cite older papers (lower numbers).
            let mut cites: Vec<String> = (0..if i < 3 { 0 } else { links.gen_range(0..3) }).map(|_| format!("p{:03}", links.gen_range(0..i))).collect();
            cites.sort();
            cites.dedup();
            let mut authors: Vec<String> = (0..rng.gen_range(0..4)).map(|_| if rng.gen_range(0..10) == 0 { "zz".to_string() } else { author_key(rng.gen_range(0..AUTHORS)) }).collect();
            authors.sort();
            authors.dedup();
            Paper { id: format!("p{i:03}"), year: 2000 + (i % 20) as i64, venue: rng.gen_range(1..=10), authors, weight: 1.0 + (i % 6) as f64, cites }
        })
        .collect();
    Data { papers, authors, venues }
}

fn open(dir: &std::path::Path, d: &Data) -> Db {
    let opts = DbOptions { segment_max_docs: 120, sync: false, ..DbOptions::default() };
    let db = Db::create(&dir.join("t.strata"), schema(), opts).unwrap();
    db.load(d.papers.iter().enumerate().map(|(i, p)| {
        let cites: Vec<Edge> = p.cites.iter().map(|c| Edge::to(c.as_str())).collect();
        let topic = if i % 3 == 0 { "graph search" } else { "vector indexes" };
        vec![
            Value::Str(p.id.clone()),
            Value::Str(format!("paper {i} on {topic} {}", "and more ".repeat(i % 4))),
            Value::Int(p.year),
            Value::Int(p.venue),
            Value::List(p.authors.iter().map(|a| Value::Str(a.clone())).collect()),
            edges_value(&cites),
            Value::Float(p.weight),
        ]
    }))
    .unwrap();
    sql(&db, "CREATE TABLE authors (id KEYWORD PRIMARY KEY, name TEXT, born INT)");
    db.load_into("authors", d.authors.iter().map(|a| vec![Value::Str(a.id.clone()), Value::Str(format!("author {}", a.id)), Value::Int(a.born)])).unwrap();
    sql(&db, "CREATE TABLE venues (id INT PRIMARY KEY, name KEYWORD, rank FLOAT, field KEYWORD)");
    db.load_into("venues", d.venues.iter().map(|v| vec![Value::Int(v.id), Value::Str(v.name.clone()), Value::Float(v.rank), Value::Str(v.field.clone())])).unwrap();
    db
}

fn sql(db: &Db, q: &str) -> SqlResult {
    db.sql(q, &Params::new()).unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn rows(db: &Db, q: &str) -> Vec<Vec<Value>> {
    rows_with(db, q, &Params::new())
}

fn rows_with(db: &Db, q: &str, params: &Params) -> Vec<Vec<Value>> {
    db.query(q, params).unwrap_or_else(|e| panic!("{q}: {e}")).rows
}

fn fails(db: &Db, q: &str) -> String {
    match db.query(q, &Params::new()) {
        Ok(_) => panic!("{q}: expected an error"),
        Err(e) => e.to_string(),
    }
}

/// A value as text: strings as they are, NULL as "NULL".
fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Null => "NULL".into(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) => x.to_string(),
        v => format!("{v:?}"),
    }
}

/// The rows as text, each once, ordered.
fn set(rows: &[Vec<Value>]) -> BTreeSet<Vec<String>> {
    rows.iter().map(|r| r.iter().map(text).collect()).collect()
}

/// The rows as text, in their order.
fn list(rows: &[Vec<Value>]) -> Vec<Vec<String>> {
    rows.iter().map(|r| r.iter().map(text).collect()).collect()
}

fn row(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| x.to_string()).collect()
}

#[test]
fn joins_on_int_keyword_and_float_keys() {
    let d = data();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), &d);

    // INT: each paper with its venue, conditions on both sides.
    let want: BTreeSet<Vec<String>> =
        d.papers.iter().filter(|p| p.year < 2010).filter_map(|p| d.venue(p.venue).filter(|v| v.rank >= 3.0).map(|v| row(&[&p.id, &v.name]))).collect();
    assert!(want.len() > 20);
    let got = rows(&db, "SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id WHERE v.rank >= 3 AND p.year < 2010 LIMIT 10000");
    assert_eq!(got.len(), want.len());
    assert_eq!(set(&got), want);
    // The other way around: the venues drive, the papers are restricted to them.
    let got = rows(&db, "SELECT p.id, v.name FROM venues v JOIN papers p ON v.id = p.venue WHERE v.rank >= 3 AND p.year < 2010 LIMIT 10000");
    assert_eq!(set(&got), want);

    // LEFT JOIN: a paper whose venue is not a row keeps NULLs.
    let want: BTreeSet<Vec<String>> = d.papers.iter().filter(|p| p.year == 2003).map(|p| row(&[&p.id, d.venue(p.venue).map_or("NULL", |v| v.name.as_str())])).collect();
    assert!(want.iter().any(|r| r[1] == "NULL") && want.iter().any(|r| r[1] != "NULL"));
    assert_eq!(set(&rows(&db, "SELECT p.id, v.name FROM papers p LEFT JOIN venues v ON p.venue = v.id WHERE p.year = 2003 LIMIT 10000")), want);
    // ON conditions of the right side keep the left rows.
    let want: BTreeSet<Vec<String>> =
        d.papers.iter().filter(|p| p.year == 2003).map(|p| row(&[&p.id, d.venue(p.venue).filter(|v| v.field == "db").map_or("NULL", |v| v.name.as_str())])).collect();
    assert_eq!(set(&rows(&db, "SELECT p.id, v.name FROM papers p LEFT JOIN venues v ON p.venue = v.id AND v.field = 'db' WHERE p.year = 2003 LIMIT 10000")), want);

    // A multi-valued KEYWORD field: a paper joins each of its authors.
    let pairs = |born: &dyn Fn(i64) -> bool| -> BTreeSet<Vec<String>> {
        d.papers.iter().flat_map(|p| p.authors.iter().filter(|a| d.author(a).is_some_and(|a| born(a.born))).map(|a| row(&[&p.id, a]))).collect()
    };
    let want = pairs(&|b| b < 1970);
    assert!(want.len() > 30);
    assert_eq!(set(&rows(&db, "SELECT p.id, a.id FROM papers p JOIN authors a ON p.authors = a.id WHERE a.born < 1970 LIMIT 10000")), want);
    // And the authors first: each author with the papers listing them.
    assert_eq!(set(&rows(&db, "SELECT p.id, a.id FROM authors a JOIN papers p ON a.id = p.authors WHERE a.born < 1970 LIMIT 10000")), want);
    // LEFT JOIN of a multi-valued field: a paper without a known author keeps one row.
    let want: BTreeSet<Vec<String>> = d
        .papers
        .iter()
        .filter(|p| p.year == 2011)
        .flat_map(|p| {
            let known: Vec<Vec<String>> = p.authors.iter().filter(|a| d.author(a).is_some()).map(|a| row(&[&p.id, a])).collect();
            if known.is_empty() {
                vec![row(&[&p.id, "NULL"])]
            } else {
                known
            }
        })
        .collect();
    assert!(want.iter().any(|r| r[1] == "NULL"));
    assert_eq!(set(&rows(&db, "SELECT p.id, a.id FROM papers p LEFT JOIN authors a ON p.authors = a.id WHERE p.year = 2011 LIMIT 10000")), want);

    // FLOAT keys.
    let want: BTreeSet<Vec<String>> = d.papers.iter().filter(|p| p.year == 2001).flat_map(|p| d.venues.iter().filter(|v| v.rank == p.weight).map(|v| row(&[&p.id, &v.id.to_string()]))).collect();
    assert!(!want.is_empty());
    assert_eq!(set(&rows(&db, "SELECT p.id, v.id FROM papers p JOIN venues v ON p.weight = v.rank WHERE p.year = 2001 LIMIT 10000")), want);

    // Three tables.
    let want: BTreeSet<Vec<String>> = d
        .papers
        .iter()
        .filter_map(|p| d.venue(p.venue).filter(|v| v.field == "db").map(|v| (p, v)))
        .flat_map(|(p, v)| p.authors.iter().filter(|a| d.author(a).is_some_and(|a| a.born >= 1980)).map(move |a| row(&[&p.id, &format!("author {a}"), &v.name])))
        .collect();
    assert!(want.len() > 5);
    let got = rows(&db, "SELECT p.id, a.name, v.name FROM papers p JOIN authors a ON p.authors = a.id JOIN venues v ON p.venue = v.id WHERE v.field = 'db' AND a.born >= 1980 LIMIT 10000");
    assert_eq!(set(&got), want);

    // A LEFT JOIN after an inner one keeps the joined rows without a venue.
    let want: BTreeSet<Vec<String>> = d
        .papers
        .iter()
        .filter(|p| p.year >= 2012)
        .flat_map(|p| {
            let venue = d.venue(p.venue).map_or("NULL", |v| v.name.as_str());
            p.authors.iter().filter(|a| d.author(a).is_some()).map(move |a| row(&[&p.id, &format!("author {a}"), venue]))
        })
        .collect();
    assert!(want.iter().any(|r| r[2] == "NULL"));
    let got = rows(&db, "SELECT p.id, a.name, v.name FROM papers p JOIN authors a ON p.authors = a.id LEFT JOIN venues v ON p.venue = v.id WHERE p.year >= 2012 LIMIT 10000");
    assert_eq!(set(&got), want);

    // Parameters reach every side.
    let want: BTreeSet<Vec<String>> = d.papers.iter().filter(|p| p.year == 2007).filter_map(|p| d.venue(p.venue).filter(|v| v.rank >= 4.0).map(|v| row(&[&p.id, &v.name]))).collect();
    let params = Params::new().set("rank", Value::Float(4.0)).set("year", Value::Int(2007));
    assert_eq!(set(&rows_with(&db, "SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id WHERE v.rank >= :rank AND p.year = :year LIMIT 10000", &params)), want);
}

#[test]
fn subqueries_and_with_over_another_table() {
    let d = data();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), &d);
    let ids = |f: &dyn Fn(&Paper) -> bool| -> BTreeSet<Vec<String>> { d.papers.iter().filter(|p| f(p)).map(|p| row(&[&p.id])).collect() };

    // IN and NOT IN.
    let ir: Vec<i64> = d.venues.iter().filter(|v| v.field == "ir").map(|v| v.id).collect();
    assert_eq!(set(&rows(&db, "SELECT id FROM papers WHERE venue IN (SELECT id FROM venues WHERE field = 'ir') LIMIT 10000")), ids(&|p| ir.contains(&p.venue)));
    assert_eq!(
        set(&rows(&db, "SELECT id FROM papers WHERE year > 2015 AND venue NOT IN (SELECT id FROM venues WHERE field = 'ir') LIMIT 10000")),
        ids(&|p| p.year > 2015 && !ir.contains(&p.venue))
    );
    // A multi-valued field IN: any of its values.
    let young: BTreeSet<&str> = d.authors.iter().filter(|a| a.born > 1990).map(|a| a.id.as_str()).collect();
    assert_eq!(set(&rows(&db, "SELECT id FROM papers WHERE authors IN (SELECT id FROM authors WHERE born > 1990) LIMIT 10000")), ids(&|p| p.authors.iter().any(|a| young.contains(a.as_str()))));
    // The other table's rows by a value of this one.
    let used: BTreeSet<i64> = d.papers.iter().filter(|p| p.year == 2019).map(|p| p.venue).collect();
    let want: BTreeSet<Vec<String>> = d.venues.iter().filter(|v| used.contains(&v.id)).map(|v| row(&[&v.name])).collect();
    assert_eq!(set(&rows(&db, "SELECT name FROM venues WHERE id IN (SELECT venue FROM papers WHERE year = 2019)")), want);

    // EXISTS and scalar subqueries.
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM papers WHERE EXISTS (SELECT id FROM venues WHERE rank > 100)"), [[Value::Int(0)]]);
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM papers WHERE EXISTS (SELECT id FROM venues WHERE field = 'ml')"), [[Value::Int(N as i64)]]);
    let top = d.venues.iter().map(|v| v.rank).fold(f64::MIN, f64::max);
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM papers WHERE weight = (SELECT MAX(rank) FROM venues)"), [[Value::Int(d.papers.iter().filter(|p| p.weight == top).count() as i64)]]);
    // Nested: the latest year of the papers in 'db' venues.
    let db_venues: Vec<i64> = d.venues.iter().filter(|v| v.field == "db").map(|v| v.id).collect();
    let last = d.papers.iter().filter(|p| db_venues.contains(&p.venue)).map(|p| p.year).max().unwrap();
    assert_eq!(
        set(&rows(&db, "SELECT id FROM papers WHERE year = (SELECT MAX(year) FROM papers WHERE venue IN (SELECT id FROM venues WHERE field = 'db')) LIMIT 10000")),
        ids(&|p| p.year == last)
    );

    // WITH over another table, joined.
    let want: BTreeSet<Vec<String>> = d.papers.iter().filter_map(|p| d.venue(p.venue).filter(|v| v.rank >= 4.0).map(|v| row(&[&p.id, &v.name]))).collect();
    assert_eq!(set(&rows(&db, "WITH good AS (SELECT id, name FROM venues WHERE rank >= 4) SELECT p.id, g.name FROM papers p JOIN good g ON p.venue = g.id LIMIT 10000")), want);
    // A WITH result of this table joined with another: its multi-valued column too.
    let want: BTreeSet<Vec<String>> =
        d.papers.iter().filter(|p| p.year >= 2017).flat_map(|p| p.authors.iter().filter_map(|a| d.author(a)).map(|a| row(&[&p.id, &a.id, &a.born.to_string()]))).collect();
    assert!(want.len() > 10);
    assert_eq!(
        set(&rows(&db, "WITH recent AS (SELECT id, authors FROM papers WHERE year >= 2017) SELECT r.id, a.id, a.born FROM recent r JOIN authors a ON r.authors = a.id LIMIT 10000")),
        want
    );
    // Links of a table that is not the first: to its own rows, scanned or
    // in memory.
    let early: BTreeSet<&str> = d.papers.iter().filter(|p| p.year == 2000).map(|p| p.id.as_str()).collect();
    let want: BTreeSet<Vec<String>> = d
        .papers
        .iter()
        .filter(|p| p.year >= 2010)
        .filter_map(|p| d.venue(p.venue).map(|v| (p, v)))
        .flat_map(|(p, v)| p.cites.iter().filter(|c| early.contains(c.as_str())).map(move |c| row(&[&v.id.to_string(), &p.id, c])))
        .collect();
    assert!(want.len() > 2);
    let got = rows(&db, "WITH early AS (SELECT id FROM papers WHERE year = 2000) SELECT v.id, p.id, e.id FROM venues v JOIN papers p ON v.id = p.venue JOIN early e ON p.cites = e.id WHERE p.year >= 2010 LIMIT 10000");
    assert_eq!(set(&got), want);
    let got = rows(&db, "SELECT v.id, p.id, q.id FROM venues v JOIN papers p ON v.id = p.venue JOIN papers q ON p.cites = q.id WHERE p.year >= 2010 AND q.year = 2000 LIMIT 10000");
    assert_eq!(set(&got), want);

    // A WITH result read by a subquery of another table.
    assert_eq!(
        set(&rows(&db, "WITH ir AS (SELECT id FROM venues WHERE field = 'ir') SELECT id FROM papers WHERE venue IN (SELECT id FROM ir) LIMIT 10000")),
        ids(&|p| ir.contains(&p.venue))
    );
}

#[test]
fn groups_orders_and_cuts_over_joins() {
    let d = data();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), &d);

    // GROUP BY a column of the other table.
    let mut want: BTreeMap<String, i64> = BTreeMap::new();
    for p in &d.papers {
        if let Some(v) = d.venue(p.venue) {
            *want.entry(v.field.clone()).or_default() += 1;
        }
    }
    let got = rows(&db, "SELECT v.field, COUNT(*) FROM papers p JOIN venues v ON p.venue = v.id GROUP BY v.field ORDER BY v.field");
    assert_eq!(list(&got), want.iter().map(|(f, n)| row(&[f, &n.to_string()])).collect::<Vec<_>>());
    // Papers per author, the busiest first.
    let mut per: BTreeMap<&str, i64> = BTreeMap::new();
    for p in &d.papers {
        for a in p.authors.iter().filter(|a| d.author(a).is_some()) {
            *per.entry(a).or_default() += 1;
        }
    }
    let mut want: Vec<(i64, &str)> = per.iter().filter(|(_, &n)| n >= 10).map(|(a, &n)| (-n, *a)).collect();
    want.sort();
    assert!(want.len() > 3);
    let got = rows(&db, "SELECT a.id, COUNT(*) AS n FROM papers p JOIN authors a ON p.authors = a.id GROUP BY a.id HAVING COUNT(*) >= 10 ORDER BY n DESC, a.id");
    assert_eq!(list(&got), want.iter().map(|(n, a)| row(&[a, &(-n).to_string()])).collect::<Vec<_>>());
    // By a TEXT column of the other table.
    let got = rows(&db, "SELECT a.name, COUNT(*) AS n FROM papers p JOIN authors a ON p.authors = a.id GROUP BY a.name HAVING COUNT(*) >= 10 ORDER BY n DESC, a.name");
    assert_eq!(list(&got), want.iter().map(|(n, a)| row(&[&format!("author {a}"), &(-n).to_string()])).collect::<Vec<_>>());

    // A ranked cut: the best graph papers that have a venue, as the
    // papers alone rank them.
    let ranked = rows(&db, "SELECT id, venue FROM papers WHERE MATCH(title, 'graph') ORDER BY bm25(title, 'graph') DESC LIMIT 1000");
    let want: Vec<Vec<String>> = ranked
        .iter()
        .filter_map(|r| match r[1] {
            Value::Int(v) => d.venue(v).map(|v| row(&[&text(&r[0]), &v.name])),
            _ => None,
        })
        .take(7)
        .collect();
    let got = rows(&db, "SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id WHERE MATCH(p.title, 'graph') ORDER BY bm25(p.title, 'graph') DESC LIMIT 7");
    assert_eq!(list(&got), want);
    // A cut by the first table's field, and by the other's.
    let mut by_year: Vec<(i64, &str, &str)> = d.papers.iter().filter_map(|p| d.venue(p.venue).map(|v| (-p.year, p.id.as_str(), v.name.as_str()))).collect();
    by_year.sort();
    let got = rows(&db, "SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id ORDER BY p.year DESC, p.id LIMIT 9");
    assert_eq!(list(&got), by_year.iter().take(9).map(|(_, p, v)| row(&[p, v])).collect::<Vec<_>>());
    let mut by_rank: Vec<(i64, &str, &str)> = d.papers.iter().filter_map(|p| d.venue(p.venue).map(|v| (-(v.rank as i64), p.id.as_str(), v.name.as_str()))).collect();
    by_rank.sort();
    let got = rows(&db, "SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id ORDER BY v.rank DESC, p.id LIMIT 6");
    assert_eq!(list(&got), by_rank.iter().take(6).map(|(_, p, v)| row(&[p, v])).collect::<Vec<_>>());
    // DISTINCT over a join.
    let want: BTreeSet<Vec<String>> = d.papers.iter().filter(|p| p.year == 2004).filter_map(|p| d.venue(p.venue).map(|v| row(&[&v.field]))).collect();
    assert_eq!(set(&rows(&db, "SELECT DISTINCT v.field FROM papers p JOIN venues v ON p.venue = v.id WHERE p.year = 2004")), want);
}

#[test]
fn plans_snapshots_and_refusals() {
    let d = data();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), &d);
    let q = "SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id WHERE v.field = 'ml' LIMIT 10000";
    let want = set(&rows(&db, q));
    assert!(!want.is_empty());

    // EXPLAIN names each side's table; the plan, through JSON, runs the same.
    let text: Vec<String> = match sql(&db, &format!("EXPLAIN {q}")) {
        SqlResult::Rows(r) => r.rows.iter().map(|r| text(&r[0])).collect(),
        SqlResult::Written(_) => panic!("EXPLAIN gives rows"),
    };
    let text = text.join("\n");
    assert!(text.contains("papers") && text.contains("venues"), "{text}");
    let plan = db.explain(q, &Params::new()).unwrap();
    let plan = Plan::from_json(&serde_json::from_str(&plan.to_json_text().unwrap()).unwrap()).unwrap();
    match db.run(&plan).unwrap() {
        SqlResult::Rows(r) => assert_eq!(set(&r.rows), want),
        SqlResult::Written(_) => panic!("a read"),
    }

    // Every side reads the snapshot the query started on.
    let snap = db.snapshot();
    sql(&db, "INSERT INTO venues (id, name, rank, field) VALUES (9, 'venue9', 2.5, 'ml')");
    let at = |snap| db.query_at(snap, q, &Params::new(), &Default::default()).unwrap().rows;
    assert_eq!(set(&at(&snap)), want);
    let now = set(&at(&db.snapshot()));
    let nines: BTreeSet<Vec<String>> = d.papers.iter().filter(|p| p.venue == 9).map(|p| row(&[&p.id, "venue9"])).collect();
    assert!(!nines.is_empty());
    assert_eq!(now, want.union(&nines).cloned().collect());

    // An EDGES field links rows of its own table.
    for q in ["SELECT p.id FROM papers p JOIN authors a ON p.cites = a.id", "SELECT p.id FROM authors a JOIN papers p ON a.id = p.cites"] {
        let e = fails(&db, q);
        assert!(e.contains("an EDGES field of papers links rows of papers by their keys, not rows of authors"), "{q}: {e}");
    }
    // Keys of two kinds never equal.
    let e = fails(&db, "SELECT p.id FROM papers p JOIN authors a ON p.venue = a.id");
    assert!(e.contains("p.venue = a.id: a INT field never equals a KEYWORD field"), "{e}");
    let e = fails(&db, "SELECT p.id FROM papers p JOIN authors a ON p.title = a.name");
    assert!(e.contains("joins compare KEYWORD, INT or FLOAT fields"), "{e}");
    assert!(fails(&db, "SELECT p.id FROM papers p JOIN nope n ON p.venue = n.id").contains("no such table: nope"));
    assert!(fails(&db, "SELECT id FROM papers WHERE venue IN (SELECT id FROM nope)").contains("no such table: nope"));
}
