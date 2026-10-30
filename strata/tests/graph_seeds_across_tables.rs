//! Graph seeds from a subquery of another table: `(SELECT item FROM events
//! WHERE usr = 'u1')` seeds a walk over `items` with the items the events
//! name, as the same keys written out do, for `REACHABLE`, `PATH` and
//! `ppr`; it used to seed with the events' own keys and find nothing.

use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::{Field, FieldType, Schema};

const ITEMS: usize = 200;
const USERS: usize = 12;
const EVENTS: usize = 600;

fn run(db: &Db, sql: &str) -> SqlResult {
    db.sql(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        SqlResult::Rows(r) => r.rows,
        _ => panic!("{sql}: expected rows"),
    }
}

fn strs(db: &Db, sql: &str) -> Vec<String> {
    rows(db, sql).into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

/// Items linked to a few others, and users' events naming items.
fn db(dir: &std::path::Path) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.join("seeds.strata"), seed, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    run(&db, "CREATE TABLE items (id KEYWORD PRIMARY KEY, cat KEYWORD, cooc EDGES)");
    run(&db, "CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD, item KEYWORD REFERENCES items, ts INT)");
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let mut values = Vec::new();
    for i in 0..ITEMS {
        let links: Vec<String> = (0..rng.gen_range(0..4)).map(|_| format!("\"i{}\"", rng.gen_range(0..ITEMS))).collect();
        values.push(format!("('i{i}', 'c{}', '[{}]')", i % 7, links.join(",")));
    }
    run(&db, &format!("INSERT INTO items (id, cat, cooc) VALUES {}", values.join(", ")));
    let events: Vec<String> = (0..EVENTS).map(|e| format!("({e}, 'u{}', 'i{}', {})", rng.gen_range(0..USERS), rng.gen_range(0..ITEMS), rng.gen_range(0..1000))).collect();
    run(&db, &format!("INSERT INTO events (id, usr, item, ts) VALUES {}", events.join(", ")));
    db
}

/// The items user `u`'s last `n` events name, newest first and each once
/// (a subquery's values are a set), as a SQL tuple.
fn written_out(db: &Db, u: usize, n: usize) -> String {
    tuple(strs(db, &format!("SELECT item FROM events WHERE usr = 'u{u}' ORDER BY ts DESC, id LIMIT {n}")))
}

fn tuple(items: Vec<String>) -> String {
    assert!(!items.is_empty(), "the user has events");
    let mut seen = std::collections::HashSet::new();
    let once: Vec<String> = items.into_iter().filter(|i| seen.insert(i.clone())).map(|i| format!("'{i}'")).collect();
    format!("({})", once.join(", "))
}

#[test]
fn seeds_from_another_tables_column_are_its_values() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let mut nonempty = 0;
    for u in 0..USERS {
        let sub = format!("(SELECT item FROM events WHERE usr = 'u{u}' ORDER BY ts DESC, id LIMIT 20)");
        let keys = written_out(&db, u, 20);
        for (from_sub, from_keys) in [
            (format!("SELECT id FROM items WHERE REACHABLE(cooc, {sub}, hops => 2) LIMIT 1000"), format!("SELECT id FROM items WHERE REACHABLE(cooc, {keys}, hops => 2) LIMIT 1000")),
            (format!("SELECT id FROM items WHERE REACHABLE(cooc, {sub}, hops => 0, min_hops => 0) LIMIT 1000"), format!("SELECT id FROM items WHERE REACHABLE(cooc, {keys}, hops => 0, min_hops => 0) LIMIT 1000")),
            (format!("SELECT id, score() FROM items ORDER BY ppr(cooc, {sub}) DESC LIMIT 50"), format!("SELECT id, score() FROM items ORDER BY ppr(cooc, {keys}) DESC LIMIT 50")),
        ] {
            let (mut got, mut want) = (rows(&db, &from_sub), rows(&db, &from_keys));
            if !from_sub.contains("ppr(") {
                got.sort_by_key(|r| r[0].as_str().unwrap().to_string());
                want.sort_by_key(|r| r[0].as_str().unwrap().to_string());
            }
            assert_eq!(got, want, "{from_sub}");
            nonempty += !got.is_empty() as usize;
        }
        // The seeds themselves, removed from the walk's answer.
        let seen = format!("SELECT id FROM items WHERE NOT id IN {sub} AND REACHABLE(cooc, {sub}) LIMIT 1000");
        let want = format!("SELECT id FROM items WHERE NOT id IN {keys} AND REACHABLE(cooc, {keys}) LIMIT 1000");
        let sorted = |sql: &str| {
            let mut v = strs(&db, sql);
            v.sort();
            v
        };
        assert_eq!(sorted(&seen), sorted(&want), "{seen}");
    }
    assert!(nonempty > USERS, "the walks find items ({nonempty} non-empty answers)");

    // A path from the first item of one user's history to the last.
    let first = strs(&db, "SELECT item FROM events WHERE usr = 'u0' ORDER BY ts DESC, id LIMIT 1").remove(0);
    let path = |to: &str| format!("SELECT id, hops() FROM items WHERE PATH(cooc, '{first}', {to}, max_hops => 8) ORDER BY hops()");
    assert_eq!(rows(&db, &path("(SELECT item FROM events WHERE usr = 'u1' ORDER BY ts, id LIMIT 3)")), rows(&db, &path(&written_out_oldest(&db, 1, 3))));
}

fn written_out_oldest(db: &Db, u: usize, n: usize) -> String {
    tuple(strs(db, &format!("SELECT item FROM events WHERE usr = 'u{u}' ORDER BY ts, id LIMIT {n}")))
}

#[test]
fn explain_names_values_for_another_table_and_keys_for_the_same() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    let text = |sql: &str| rows(&db, sql).into_iter().map(|r| r[0].as_str().unwrap().to_string()).collect::<Vec<_>>().join("\n");
    let other = text("EXPLAIN SELECT id FROM items ORDER BY ppr(cooc, (SELECT item FROM events WHERE usr = 'u1' LIMIT 20)) DESC LIMIT 5");
    assert!(other.contains("ppr(cooc, values of (Project: item;"), "{other}");
    // A subquery of the scanned table that needs the compound planner (a
    // WITH) still seeds by its rows' keys.
    let same = text("EXPLAIN WITH c AS (SELECT id FROM items WHERE cat = 'c1') SELECT id FROM items WHERE REACHABLE(cooc, (SELECT id FROM c)) LIMIT 5");
    assert!(same.contains("keys of"), "{same}");
}

#[test]
fn seeds_from_another_table_take_one_column() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    // Another table's primary key is a column like any other.
    let by_key = rows(&db, "SELECT id FROM items WHERE REACHABLE(cooc, (SELECT id FROM events WHERE usr = 'u2')) LIMIT 10");
    assert!(by_key.is_empty(), "event ids are not item keys: {by_key:?}");
    let e = db.sql("SELECT id FROM items WHERE REACHABLE(cooc, (SELECT item, ts FROM events WHERE usr = 'u2')) LIMIT 10", &Params::new()).unwrap_err().to_string();
    assert!(e.contains("one column"), "{e}");
}
