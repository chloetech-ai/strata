//! Correlated subqueries in rankings: a subquery that names the row being
//! ranked (`first.cat`) runs once for each distinct value of what it names
//! among the rows a rerank phase, or an ORDER BY over a bounded set of
//! rows, ranks. Each query is checked against a reference evaluator that
//! computes the same ranking from the rows themselves: the candidates as a
//! plain query gives them, each subquery's value from the events in
//! memory, and the sort.

use std::collections::HashMap;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use strata::db::Db;
use strata::query::Params;
use strata::schema::Schema;
use strata::storage::DbOptions;
use strata::value::Value;

const WORDS: [&str; 10] = ["red", "blue", "tent", "lamp", "boot", "cord", "mug", "kettle", "chair", "rope"];
const ITEMS: usize = 300;
const USERS: usize = 12;
const CATS: usize = 7;

fn items() -> Schema {
    Schema::from_json(
        r#"{"table": "items", "key": "id", "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "title", "type": "text"},
            {"name": "cat", "type": "keyword"},
            {"name": "price", "type": "float"}]}"#,
    )
    .unwrap()
}

struct Item {
    id: String,
    cat: String,
    price: Option<f64>,
}

struct Event {
    usr: String,
    item: String,
    cat: String,
    price: f64,
    ts: i64,
}

/// The database, and its rows as the reference reads them.
fn data(dir: &std::path::Path, rng: &mut StdRng) -> (Db, Vec<Item>, Vec<Event>) {
    let db = Db::create(&dir.join("db"), items(), DbOptions { segment_max_docs: 80, memtable_max_docs: 40, ..DbOptions::default() }).unwrap();
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, cat KEYWORD, price FLOAT, ts INT)", &Params::new()).unwrap();
    let mut items = Vec::new();
    for i in 0..ITEMS {
        let title: Vec<&str> = (0..rng.gen_range(1..4)).map(|_| WORDS[rng.gen_range(0..WORDS.len())]).collect();
        let item = Item { id: format!("i{i:03}"), cat: format!("c{}", rng.gen_range(0..CATS)), price: rng.gen_bool(0.85).then(|| rng.gen_range(0..50) as f64) };
        let p = Params::new()
            .set("id", Value::Str(item.id.clone()))
            .set("t", Value::Str(title.join(" ")))
            .set("c", Value::Str(item.cat.clone()))
            .set("p", item.price.map_or(Value::Null, Value::Float));
        db.sql("INSERT INTO items (id, title, cat, price) VALUES (:id, :t, :c, :p)", &p).unwrap();
        items.push(item);
    }
    let mut events = Vec::new();
    for e in 0..1500 {
        let it = &items[rng.gen_range(0..ITEMS)];
        let ev = Event { usr: format!("u{}", rng.gen_range(0..USERS)), item: it.id.clone(), cat: it.cat.clone(), price: rng.gen_range(0..50) as f64, ts: rng.gen_range(0..100_000) * 10 + e % 10 };
        let p = Params::new()
            .set("id", Value::Int(e))
            .set("u", Value::Str(ev.usr.clone()))
            .set("i", Value::Str(ev.item.clone()))
            .set("c", Value::Str(ev.cat.clone()))
            .set("p", Value::Float(ev.price))
            .set("ts", Value::Int(ev.ts));
        db.sql("INSERT INTO events (id, usr, item, cat, price, ts) VALUES (:id, :u, :i, :c, :p, :ts)", &p).unwrap();
        events.push(ev);
    }
    (db, items, events)
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Float(x) => Some(*x),
        Value::Int(x) => Some(*x as f64),
        _ => None,
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        v => panic!("not text: {v:?}"),
    }
}

/// What a subquery counts, for a row of `items`, from the events in memory.
type Sub<'a> = Box<dyn Fn(&Item) -> Option<f64> + 'a>;

/// The rows a ranking puts first: `(key, score)` best first, ties in the
/// order the rows came.
fn rank(rows: Vec<(String, Option<f64>)>, limit: usize) -> Vec<(String, Option<f64>)> {
    let mut rows = rows;
    // NULL ranks last.
    rows.sort_by(|a, b| b.1.unwrap_or(f64::NEG_INFINITY).total_cmp(&a.1.unwrap_or(f64::NEG_INFINITY)));
    rows.truncate(limit);
    rows
}

fn assert_ranked(sql: &str, got: &strata::query::exec::ResultSet, want: &[(String, Option<f64>)]) {
    let ids: Vec<String> = got.rows.iter().map(|r| text(&r[0])).collect();
    let want_ids: Vec<&String> = want.iter().map(|w| &w.0).collect();
    assert_eq!(ids.iter().collect::<Vec<_>>(), want_ids, "{sql}");
    let at = got.columns.iter().position(|c| c == "score()").unwrap();
    for (r, (_, w)) in got.rows.iter().zip(want) {
        let g = num(&r[at]);
        match (g, w) {
            (Some(g), Some(w)) => assert!((g - w).abs() <= 1e-9 * w.abs().max(1.0), "{sql}: {g} vs {w}"),
            (g, w) => assert_eq!(g.is_some(), w.is_some(), "{sql}: {g:?} vs {w:?}"),
        }
    }
}

#[test]
fn equal_to_a_reference_evaluator() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(7);
    let (db, items, events) = data(dir.path(), &mut rng);
    let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    let mut checked = 0;
    for trial in 0..60 {
        let u = format!("u{}", rng.gen_range(0..USERS));
        let q = format!("{} {}", WORDS[rng.gen_range(0..WORDS.len())], WORDS[rng.gen_range(0..WORDS.len())]);
        let k = rng.gen_range(5..60);
        let limit = rng.gen_range(1..15);
        let hist = rng.gen_range(1..30);
        let w = [0.5, 1.0, 0.01][trial % 3];
        let p = Params::new().set("u", Value::Str(u.clone())).set("q", Value::Str(q.clone()));
        // The user's last `hist` events, newest first.
        let mut mine: Vec<&Event> = events.iter().filter(|e| e.usr == u).collect();
        mine.sort_by(|a, b| b.ts.cmp(&a.ts));
        let h: Vec<&Event> = mine.iter().take(hist).copied().collect();
        // Shapes of correlated subquery, and how the reference computes each.
        let shapes: Vec<(String, Sub)> = vec![
            (format!("{w} * (SELECT COUNT(*) FROM h WHERE h.cat = first.cat)"), Box::new(|it: &Item| Some(w * h.iter().filter(|e| e.cat == it.cat).count() as f64))),
            (
                format!("{w} * (SELECT COUNT(*) FROM events e WHERE e.usr = :u AND e.cat = first.cat AND e.price < first.price)"),
                Box::new(|it: &Item| Some(w * it.price.map_or(0, |p| mine.iter().filter(|e| e.cat == it.cat && e.price < p).count()) as f64)),
            ),
            (
                // NULL without an event, which coalesce makes 0.
                "0.000001 * coalesce((SELECT MAX(e.ts) FROM events e WHERE e.usr = :u AND e.item = first.id), 0)".to_string(),
                Box::new(|it: &Item| Some(0.000001 * mine.iter().filter(|e| e.item == it.id).map(|e| e.ts).max().unwrap_or(0) as f64)),
            ),
            (
                // A NULL of the row equals nothing.
                format!("{w} * (SELECT COUNT(*) FROM events e WHERE e.usr = :u AND e.price = first.price)"),
                Box::new(|it: &Item| Some(w * it.price.map_or(0, |p| mine.iter().filter(|e| e.price == p).count()) as f64)),
            ),
            (
                // NULL, which ranks last, where the item's price is.
                format!("{w} * (SELECT COUNT(*) FROM h WHERE h.cat = first.cat AND h.ts > 0) - (SELECT MIN(h.price) FROM h WHERE h.cat = first.cat)"),
                Box::new(|it: &Item| {
                    let of: Vec<&&Event> = h.iter().filter(|e| e.cat == it.cat).collect();
                    of.iter().map(|e| e.price).reduce(f64::min).map(|m| w * of.len() as f64 - m)
                }),
            ),
        ];
        let history = format!("WITH h AS (SELECT item, cat, price, ts FROM events WHERE usr = :u ORDER BY ts DESC LIMIT {hist})");
        for (expr, sub) in &shapes {
            // A rerank phase over the top `k` by BM25.
            let sql = format!(
                "{history}, first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT {k}) SELECT id, cat, score() FROM first ORDER BY bm25(title, :q) + {expr} DESC LIMIT {limit}"
            );
            let got = db.query(&sql, &p).unwrap_or_else(|e| panic!("{sql}: {e}"));
            let cands = db.query(&format!("SELECT id, bm25(title, :q) AS b FROM items ORDER BY bm25(title, :q) DESC LIMIT {k}"), &p).unwrap();
            let want = rank(cands.rows.iter().map(|r| (text(&r[0]), num(&r[1]).and_then(|b| sub(by_id[text(&r[0]).as_str()]).map(|v| b + v)))).collect(), limit);
            assert_ranked(&sql, &got, &want);

            // An ORDER BY over a bounded set: the rows of a WITH result.
            let c = format!("c{}", rng.gen_range(0..CATS));
            let sql = format!(
                "{history}, first AS (SELECT * FROM items WHERE cat = '{c}' OR price < 3 LIMIT {k}) SELECT id, score() FROM first ORDER BY coalesce(price, 0) + {expr} DESC LIMIT {limit}"
            );
            let got = db.query(&sql, &p).unwrap_or_else(|e| panic!("{sql}: {e}"));
            let rows = db.query(&format!("SELECT id FROM items WHERE cat = '{c}' OR price < 3 LIMIT {k}"), &Params::new()).unwrap();
            // Ties by key, as the executor orders them.
            let mut cands: Vec<(String, Option<f64>)> = rows.rows.iter().map(|r| {
                let it = by_id[text(&r[0]).as_str()];
                (it.id.clone(), sub(it).map(|v| it.price.unwrap_or(0.0) + v))
            }).collect();
            cands.sort_by(|a, b| a.0.cmp(&b.0));
            assert_ranked(&sql, &got, &rank(cands, limit));

            // A correlated phase after another rerank phase.
            let k2 = rng.gen_range(1..=k);
            let second = format!("{history}, first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT {k}), second AS (SELECT * FROM first ORDER BY bm25(title, :q) * (1 + 0.01 * coalesce(price, 0)) DESC LIMIT {k2})");
            let sql = format!("{second} SELECT id, score() FROM second ORDER BY bm25(title, :q) + {expr} DESC LIMIT {limit}");
            let got = db.query(&sql, &p).unwrap_or_else(|e| panic!("{sql}: {e}"));
            let cands = db.query(&format!("{second} SELECT id, bm25(title, :q) AS b FROM second ORDER BY bm25(title, :q) * (1 + 0.01 * coalesce(price, 0)) DESC LIMIT {k2}"), &p).unwrap();
            let want = rank(cands.rows.iter().map(|r| (text(&r[0]), num(&r[1]).and_then(|b| sub(by_id[text(&r[0]).as_str()]).map(|v| b + v)))).collect(), limit);
            assert_ranked(&sql, &got, &want);

            // A column of the rows ranked, the ranking without it.
            let sql = format!("{history}, first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT {k}) SELECT id, {expr} AS n FROM first ORDER BY bm25(title, :q) * (1 + 0.01 * coalesce(price, 0)) DESC LIMIT {limit}");
            let got = db.query(&sql, &p).unwrap_or_else(|e| panic!("{sql}: {e}"));
            let plain = db.query(&format!("WITH first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT {k}) SELECT id FROM first ORDER BY bm25(title, :q) * (1 + 0.01 * coalesce(price, 0)) DESC LIMIT {limit}"), &p).unwrap();
            assert_eq!(got.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(), plain.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(), "{sql}");
            for r in &got.rows {
                let want = sub(by_id[text(&r[0]).as_str()]);
                match (num(&r[1]), want) {
                    (Some(g), Some(w)) => assert!((g - w).abs() <= 1e-9 * w.abs().max(1.0), "{sql}: {g} vs {w}"),
                    (g, w) => assert_eq!(g.is_some(), w.is_some(), "{sql}: {g:?} vs {w:?}"),
                }
            }
            checked += 4;
        }
    }
    assert_eq!(checked, 60 * 5 * 4);
}

#[test]
fn each_distinct_value_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(3);
    let (db, items, _) = data(dir.path(), &mut rng);
    let p = Params::new().set("u", Value::Str("u1".into())).set("q", Value::Str("red mug".into()));
    let cands = db.query("SELECT id FROM items ORDER BY bm25(title, :q) DESC LIMIT 200", &p).unwrap();
    let cats: std::collections::HashSet<&str> = cands.rows.iter().map(|r| items.iter().find(|i| Value::Str(i.id.clone()) == r[0]).unwrap().cat.as_str()).collect();
    let line = |sub: &str| {
        let sql = format!("WITH first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT 200) SELECT id FROM first ORDER BY bm25(title, :q) + {sub} DESC LIMIT 5");
        let rs = db.query(&sql, &p).unwrap();
        rs.stats.plan.iter().find(|l| l.starts_with("correlated:")).unwrap().clone()
    };
    // Outer values met only by equalities: one query grouped by them, and
    // one more for the value of no rows when a category has no events.
    let grouped = line("(SELECT COUNT(*) FROM events e WHERE e.usr = :u AND e.cat = first.cat)");
    let head = format!("correlated: 1 subqueries over {} rows, {} values, ", cands.rows.len(), cats.len());
    assert!(grouped == format!("{head}1 runs") || grouped == format!("{head}2 runs"), "{grouped}");
    // Else once for each.
    let each = line("(SELECT COUNT(*) FROM events e WHERE e.usr = :u AND e.cat != first.cat)");
    assert_eq!(each, format!("{head}{} runs", cats.len()));
}

#[test]
fn where_it_does_not_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(5);
    let (db, _, _) = data(dir.path(), &mut rng);
    let p = Params::new().set("u", Value::Str("u1".into())).set("q", Value::Str("red mug".into()));
    let fails = |sql: &str, says: &str| {
        let e = db.query(sql, &p).map(|_| ()).unwrap_err().to_string();
        assert!(e.contains(says), "{sql}: {e}");
    };
    // The outer row's column must exist.
    fails("WITH first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT 20) SELECT id FROM first ORDER BY bm25(title, :q) + (SELECT COUNT(*) FROM events e WHERE e.cat = first.colour) DESC LIMIT 5", "first has no column colour");
    // A subquery of a table compares the table's columns with the row's.
    fails("WITH first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT 20) SELECT id FROM first ORDER BY bm25(title, :q) + (SELECT COUNT(*) FROM events e WHERE e.price + 1 < first.price) DESC LIMIT 5", "compares a column of the table");
    // A number, not text.
    fails("WITH first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT 20) SELECT id FROM first ORDER BY bm25(title, :q) + (SELECT MAX(e.item) FROM events e WHERE e.cat = first.cat) DESC LIMIT 5", "gives a number");
    // A number of each row ranked, not a condition.
    fails("SELECT id FROM items WHERE price > (SELECT AVG(e.price) FROM events e WHERE e.cat = items.cat) LIMIT 5", "not in WHERE");
    fails("SELECT id FROM items WHERE id IN (SELECT e.item FROM events e WHERE e.cat = items.cat) LIMIT 5", "not in WHERE, IN or EXISTS");
    fails("SELECT id FROM items WHERE EXISTS (SELECT e.item FROM events e WHERE e.cat = items.cat) LIMIT 5", "not in WHERE, IN or EXISTS");
    // A column of a table's ranked rows, without phases.
    let rs = db.query("SELECT id, price, (SELECT COUNT(*) FROM events e WHERE e.usr = :u AND e.item = items.id) AS seen FROM items WHERE MATCH(title, :q) ORDER BY bm25(title, :q) DESC LIMIT 8", &p).unwrap();
    let plain = db.query("SELECT id, price FROM items WHERE MATCH(title, :q) ORDER BY bm25(title, :q) DESC LIMIT 8", &p).unwrap();
    assert_eq!(rs.rows.iter().map(|r| r[..2].to_vec()).collect::<Vec<_>>(), plain.rows);
    for r in &rs.rows {
        let n = db.query("SELECT COUNT(*) FROM events WHERE usr = :u AND item = :i", &p.clone().set("i", r[0].clone())).unwrap();
        assert_eq!(num(&r[2]), num(&n.rows[0][0]), "{r:?}");
    }
    // An ORDER BY over a table ranks its rows when they are few.
    let rs = db.query("SELECT id FROM items ORDER BY (SELECT COUNT(*) FROM events e WHERE e.cat = items.cat) DESC LIMIT 5", &p).unwrap();
    assert_eq!(rs.rows.len(), 5);
    // And refuses when they are many.
    db.sql("CREATE TABLE many (id INT PRIMARY KEY, cat KEYWORD)", &Params::new()).unwrap();
    let rows: Vec<String> = (0..strata::query::ops::CORRELATED_ROWS + 1).map(|i| format!("({i}, 'c{}')", i % CATS)).collect();
    db.sql(&format!("INSERT INTO many (id, cat) VALUES {}", rows.join(", ")), &Params::new()).unwrap();
    fails("SELECT id FROM many ORDER BY (SELECT COUNT(*) FROM events e WHERE e.cat = many.cat) DESC LIMIT 5", "rank its top rows in a rerank phase");
    let rs = db.query("WITH top AS (SELECT * FROM many ORDER BY id DESC LIMIT 100) SELECT id FROM top ORDER BY (SELECT COUNT(*) FROM events e WHERE e.cat = top.cat) DESC LIMIT 5", &p).unwrap();
    assert_eq!(rs.rows.len(), 5);
}
