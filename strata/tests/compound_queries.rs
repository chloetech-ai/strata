//! Compound queries (subqueries, `WITH`, joins of the table with itself,
//! `SELECT DISTINCT`, `FROM (SELECT ...)`) against the same computed over
//! the documents.

use std::collections::{BTreeMap, BTreeSet};

use rand::{Rng, SeedableRng};
use strata::db::Db;
use strata::graph::{edges_value, Edge};
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::{Field, FieldType, Schema};

const N: usize = 400;
const KINDS: [&str; 4] = ["survey", "method", "dataset", "position"];

fn schema() -> Schema {
    Schema::new(
        "papers",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: false }),
            Field::new("year", FieldType::Int),
            Field::new("kind", FieldType::Keyword),
            Field::new("cites", FieldType::Edges),
        ],
        Some("id"),
    )
    .unwrap()
}

struct Paper {
    id: String,
    title: String,
    year: i64,
    kind: String,
    cites: Vec<String>,
}

fn key(i: usize) -> String {
    format!("p{i:03}")
}

fn papers() -> Vec<Paper> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    (0..N)
        .map(|i| {
            // Papers cite older papers (lower numbers), some none.
            let n = if i < 5 { 0 } else { rng.gen_range(0..4) };
            let mut cites: Vec<String> = (0..n).map(|_| key(rng.gen_range(0..i))).collect();
            cites.sort();
            cites.dedup();
            Paper {
                id: key(i),
                title: format!("paper {i} on {}", if i % 3 == 0 { "graph search" } else { "vector indexes" }),
                year: 1990 + (i as i64 * 30 / N as i64),
                kind: KINDS[rng.gen_range(0..KINDS.len())].to_string(),
                cites,
            }
        })
        .collect()
}

fn open(dir: &std::path::Path, ps: &[Paper]) -> Db {
    let opts = DbOptions { segment_max_docs: 150, ..DbOptions::default() };
    let db = Db::create(&dir.join("p.strata"), schema(), opts).unwrap();
    db.load(ps.iter().map(|p| {
        let es: Vec<Edge> = p.cites.iter().map(|t| Edge::to(t.as_str())).collect();
        vec![Value::Str(p.id.clone()), Value::Str(p.title.clone()), Value::Int(p.year), Value::Str(p.kind.clone()), edges_value(&es)]
    }))
    .unwrap();
    db
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")).rows
}

fn strs(rows: &[Vec<Value>], i: usize) -> Vec<String> {
    rows.iter().map(|r| r[i].as_str().unwrap_or("NULL").to_string()).collect()
}

fn set(rows: &[Vec<Value>], i: usize) -> BTreeSet<String> {
    strs(rows, i).into_iter().collect()
}

#[test]
fn subqueries() {
    let ps = papers();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), &ps);

    // IN (SELECT ...): papers of the kinds that 2001's papers have.
    let kinds: BTreeSet<&str> = ps.iter().filter(|p| p.year == 2001).map(|p| p.kind.as_str()).collect();
    let want: BTreeSet<String> = ps.iter().filter(|p| kinds.contains(p.kind.as_str())).map(|p| p.id.clone()).collect();
    let got = rows(&db, "SELECT id FROM papers WHERE kind IN (SELECT kind FROM papers WHERE year = 2001) LIMIT 1000");
    assert_eq!(set(&got, 0), want);
    // NOT IN, with another condition.
    let want: BTreeSet<String> = ps.iter().filter(|p| !kinds.contains(p.kind.as_str()) && p.year < 2000).map(|p| p.id.clone()).collect();
    let got = rows(&db, "SELECT id FROM papers WHERE year < 2000 AND kind NOT IN (SELECT kind FROM papers WHERE year = 2001) LIMIT 1000");
    assert_eq!(set(&got, 0), want);

    // A scalar subquery.
    let last = ps.iter().map(|p| p.year).max().unwrap();
    let want: BTreeSet<String> = ps.iter().filter(|p| p.year == last).map(|p| p.id.clone()).collect();
    assert_eq!(set(&rows(&db, "SELECT id FROM papers WHERE year = (SELECT MAX(year) FROM papers) LIMIT 1000"), 0), want);
    assert!(db.query("SELECT id FROM papers WHERE year = (SELECT year FROM papers)", &Params::new()).is_err(), "many rows");

    // EXISTS.
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM papers WHERE EXISTS (SELECT id FROM papers WHERE year = 1990)")[0][0], Value::Int(N as i64));
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM papers WHERE EXISTS (SELECT id FROM papers WHERE year = 1900)")[0][0], Value::Int(0));

    // Papers citing a 1990 survey, through a subquery of keys.
    let surveys: BTreeSet<&str> = ps.iter().filter(|p| p.year == 1990 && p.kind == "survey").map(|p| p.id.as_str()).collect();
    let want: BTreeSet<String> = ps.iter().filter(|p| p.cites.iter().any(|c| surveys.contains(c.as_str()))).map(|p| p.id.clone()).collect();
    let got = rows(&db, "SELECT id FROM papers WHERE REACHABLE(cites, (SELECT id FROM papers WHERE year = 1990 AND kind = 'survey'), dir => 'in') LIMIT 1000");
    assert_eq!(set(&got, 0), want);
}

#[test]
fn with_and_derived_tables() {
    let ps = papers();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), &ps);

    // WITH rows of the table, in their order, read by an outer query.
    let mut recent: Vec<&Paper> = ps.iter().collect();
    recent.sort_by(|a, b| b.year.cmp(&a.year).then(a.id.cmp(&b.id)));
    let top: Vec<String> = recent[..10].iter().map(|p| p.id.clone()).collect();
    let got = rows(&db, "WITH recent AS (SELECT id FROM papers ORDER BY year DESC, id LIMIT 10) SELECT id, kind FROM recent");
    assert_eq!(strs(&got, 0), top);
    let got = rows(&db, "WITH recent AS (SELECT id FROM papers ORDER BY year DESC, id LIMIT 10) SELECT id FROM recent LIMIT 3 OFFSET 2");
    assert_eq!(strs(&got, 0), top[2..5].to_vec());
    let mut want: BTreeMap<String, i64> = BTreeMap::new();
    for p in &recent[..10] {
        *want.entry(p.kind.clone()).or_default() += 1;
    }
    let got = rows(&db, "WITH recent AS (SELECT id FROM papers ORDER BY year DESC, id LIMIT 10) SELECT kind, COUNT(*) FROM recent GROUP BY kind ORDER BY kind");
    assert_eq!(got.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap())).collect::<BTreeMap<_, _>>(), want);

    // A WITH result as graph seeds: what the ten newest papers cite.
    let want: BTreeSet<String> = recent[..10].iter().flat_map(|p| p.cites.iter().cloned()).collect();
    let got = rows(&db, "WITH recent AS (SELECT id FROM papers ORDER BY year DESC, id LIMIT 10) SELECT id FROM papers WHERE REACHABLE(cites, (SELECT id FROM recent)) LIMIT 1000");
    assert_eq!(set(&got, 0), want);
    // And in IN.
    let got = rows(&db, "WITH recent AS (SELECT id FROM papers ORDER BY year DESC, id LIMIT 10) SELECT id FROM papers WHERE id IN (SELECT id FROM recent) AND kind = 'survey' LIMIT 100");
    assert_eq!(set(&got, 0), recent[..10].iter().filter(|p| p.kind == "survey").map(|p| p.id.clone()).collect());

    // A derived table of aggregates, filtered and ordered in memory.
    let mut by_kind: BTreeMap<&str, i64> = BTreeMap::new();
    for p in &ps {
        *by_kind.entry(&p.kind).or_default() += 1;
    }
    let mut want: Vec<(String, i64)> = by_kind.iter().filter(|(_, n)| **n > 50).map(|(k, n)| (k.to_string(), *n)).collect();
    want.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let got = rows(&db, "SELECT kind, n FROM (SELECT kind, COUNT(*) AS n FROM papers GROUP BY kind) AS t WHERE n > 50 ORDER BY n DESC, kind");
    assert_eq!(got.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap())).collect::<Vec<_>>(), want);
    // WITH of aggregates, named columns.
    let got = rows(&db, "WITH k(kind, n) AS (SELECT kind, COUNT(*) FROM papers GROUP BY kind) SELECT kind FROM k WHERE n = (SELECT MAX(n) FROM k)");
    let most = *by_kind.values().max().unwrap();
    assert_eq!(set(&got, 0), by_kind.iter().filter(|(_, n)| **n == most).map(|(k, _)| k.to_string()).collect());

    // DISTINCT.
    let got = rows(&db, "SELECT DISTINCT kind FROM papers");
    assert_eq!(set(&got, 0), KINDS.iter().map(|k| k.to_string()).collect());
    assert!(db.query("WITH RECURSIVE r AS (SELECT id FROM papers) SELECT id FROM r", &Params::new()).is_err());
}

#[test]
fn joins() {
    let ps = papers();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), &ps);
    let by_id: BTreeMap<&str, &Paper> = ps.iter().map(|p| (p.id.as_str(), p)).collect();

    // Each paper since 2015 with each paper it cites, through the EDGES field.
    let mut want: BTreeSet<(String, String, i64)> = BTreeSet::new();
    for p in ps.iter().filter(|p| p.year >= 2015) {
        for c in &p.cites {
            want.insert((p.id.clone(), c.clone(), by_id[c.as_str()].year));
        }
    }
    let got = rows(&db, "SELECT a.id, b.id AS cited, b.year FROM papers a JOIN papers b ON a.cites = b.id WHERE a.year >= 2015");
    let got: BTreeSet<(String, String, i64)> = got.iter().map(|r| (r[0].as_str().unwrap().into(), r[1].as_str().unwrap().into(), r[2].as_i64().unwrap())).collect();
    assert_eq!(got, want);
    // The other way round: the papers citing each 1990 paper.
    let mut want: BTreeSet<(String, String)> = BTreeSet::new();
    for p in &ps {
        for c in &p.cites {
            if by_id[c.as_str()].year == 1990 {
                want.insert((c.clone(), p.id.clone()));
            }
        }
    }
    let got = rows(&db, "SELECT a.id, b.id FROM papers a JOIN papers b ON b.cites = a.id WHERE a.year = 1990");
    assert_eq!(got.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_str().unwrap().to_string())).collect::<BTreeSet<_>>(), want);

    // A join on a keyword, with conditions on both sides and across them.
    let p1 = by_id["p101"];
    let want: BTreeSet<String> = ps.iter().filter(|p| p.kind == p1.kind && p.year > p1.year && p.id != p1.id).map(|p| p.id.clone()).collect();
    let got = rows(&db, "SELECT b.id FROM papers a JOIN papers b ON a.kind = b.kind WHERE a.id = 'p101' AND b.year > a.year AND b.id != a.id");
    assert_eq!(set(&got, 0), want);

    // LEFT JOIN keeps papers that cite nothing.
    let got = rows(&db, "SELECT a.id, b.id FROM papers a LEFT JOIN papers b ON a.cites = b.id WHERE a.year = 1990 ORDER BY a.id");
    let mut want: Vec<(String, String)> = Vec::new();
    for p in ps.iter().filter(|p| p.year == 1990) {
        if p.cites.is_empty() {
            want.push((p.id.clone(), "NULL".into()));
        }
        for c in &p.cites {
            want.push((p.id.clone(), c.clone()));
        }
    }
    want.sort();
    let mut got: Vec<(String, String)> = got.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_str().unwrap_or("NULL").to_string())).collect();
    got.sort();
    assert_eq!(got, want);
    assert!(want.iter().any(|w| w.1 == "NULL"));

    // Aggregates over a join: the most cited papers.
    let mut cited: BTreeMap<&str, i64> = BTreeMap::new();
    for p in &ps {
        for c in &p.cites {
            *cited.entry(c).or_default() += 1;
        }
    }
    let mut want: Vec<(String, i64)> = cited.iter().map(|(k, n)| (k.to_string(), *n)).collect();
    want.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    want.truncate(5);
    let got = rows(&db, "SELECT b.id, COUNT(*) AS n FROM papers a JOIN papers b ON a.cites = b.id GROUP BY b.id ORDER BY n DESC, b.id LIMIT 5");
    assert_eq!(got.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap())).collect::<Vec<_>>(), want);

    // Ranked by one side's score: the first rows follow its bm25 order.
    let ranked = db.query("SELECT id FROM papers WHERE year >= 2010 ORDER BY bm25(title, 'graph search') DESC LIMIT 400", &Params::new()).unwrap();
    let got = db
        .query("SELECT a.id, b.id, score() FROM papers a JOIN papers b ON a.cites = b.id WHERE a.year >= 2010 ORDER BY bm25(a.title, 'graph search') DESC LIMIT 8", &Params::new())
        .unwrap();
    assert_eq!(got.rows.len(), 8);
    let order: Vec<&Value> = ranked.keys.iter().filter(|k| !by_id[k.as_str().unwrap()].cites.is_empty()).collect();
    let mut firsts: Vec<String> = strs(&got.rows, 0);
    firsts.dedup();
    assert_eq!(firsts, order[..firsts.len()].iter().map(|v| v.as_str().unwrap().to_string()).collect::<Vec<_>>());
    assert!(got.rows.windows(2).all(|w| w[0][2].as_f64() >= w[1][2].as_f64()));

    // A WITH result joined with the table.
    let got = rows(&db, "WITH s AS (SELECT id FROM papers WHERE kind = 'survey' AND year = 1995) SELECT s.id, p.id FROM s JOIN papers p ON p.cites = s.id");
    let surveys: BTreeSet<&str> = ps.iter().filter(|p| p.kind == "survey" && p.year == 1995).map(|p| p.id.as_str()).collect();
    let want: BTreeSet<(String, String)> = ps.iter().flat_map(|p| p.cites.iter().filter(|c| surveys.contains(c.as_str())).map(move |c| (c.clone(), p.id.clone()))).collect();
    assert_eq!(got.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_str().unwrap().to_string())).collect::<BTreeSet<_>>(), want);

    for bad in [
        "SELECT a.id FROM papers a JOIN papers b ON a.year > b.year",
        "SELECT id FROM papers a JOIN papers b ON a.cites = b.id",
        "SELECT a.id FROM papers a JOIN papers a ON a.cites = a.id",
        "SELECT a.id FROM papers a JOIN papers b ON a.cites = b.id WHERE MATCH(a.title, 'graph') OR MATCH(b.title, 'graph')",
        "SELECT a.id FROM papers a JOIN other b ON a.id = b.id",
    ] {
        let e = db.query(bad, &Params::new());
        assert!(e.is_err(), "{bad}");
    }
}
