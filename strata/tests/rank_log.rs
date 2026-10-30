//! The rank log: a logged query writes what it served, with the values its
//! ranking read, and the export gives each logged row back as it was
//! served, after the tables have changed under it (new rows that move
//! BM25's statistics, ratings rewritten, rows deleted, flushes and merges,
//! the database reopened), with the context's history before the row and
//! a label from the events after it.

use std::collections::HashMap;

use serde_json::{json, Value as Json};
use strata::data::{self, col};
use strata::db::{Db, SqlResult};
use strata::query::sql::with_features;
use strata::query::Params;
use strata::rank_log::{Export, Log};
use strata::storage::{DbOptions, MergePolicy};
use strata::value::Value;
use strata::VectorIndexKind;

const DIM: usize = 16;
const MINUTE: i64 = 60_000_000;
const USERS: usize = 5;
const HISTORY: usize = 4;

fn opts() -> DbOptions {
    DbOptions { segment_max_docs: 300, memtable_max_docs: 64, ..DbOptions::default() }
}

/// The queries logged, each with the query that gives the values it
/// served (its candidates included) and its candidates.
fn queries() -> Vec<(&'static str, &'static str, Option<usize>)> {
    vec![
        (
            "SELECT id FROM docs ORDER BY bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0)) DESC LIMIT 10",
            "SELECT id FROM docs ORDER BY bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0)) DESC LIMIT 10",
            None,
        ),
        (
            "SELECT id, title FROM docs ORDER BY 0.3 * minmax(bm25(body, :t)) + 0.7 * minmax(similarity(emb, :q)) DESC LIMIT 6",
            "SELECT id, title FROM docs ORDER BY 0.3 * minmax(bm25(body, :t)) + 0.7 * minmax(similarity(emb, :q)) DESC LIMIT 6",
            None,
        ),
        (
            "WITH a AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 40) SELECT id FROM a ORDER BY bm25(body, :t) * (1 + 0.3 * coalesce(rating, 0)) + 0.1 * coalesce(year, 0) DESC LIMIT 5",
            "WITH a AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 40) SELECT id FROM a ORDER BY bm25(body, :t) * (1 + 0.3 * coalesce(rating, 0)) + 0.1 * coalesce(year, 0) DESC LIMIT 5",
            None,
        ),
        (
            "SELECT id, year FROM docs WHERE year >= 2000 ORDER BY similarity(emb, :q) + 0.01 * coalesce(rating, 0) DESC LIMIT 4",
            "SELECT id, year FROM docs WHERE year >= 2000 ORDER BY similarity(emb, :q) + 0.01 * coalesce(rating, 0) DESC LIMIT 12",
            Some(12),
        ),
    ]
}

/// An event the test wrote, as the export should read it.
#[derive(Clone, Debug)]
struct Ev {
    id: i64,
    usr: String,
    item: String,
    action: &'static str,
    ts: i64,
}

/// What a logged request served, as the test saw it.
struct Served {
    ts: i64,
    context: Option<String>,
    /// Per position: the row's key, its score() and its features, as the
    /// query computed them when served.
    rows: Vec<(String, Value, Vec<(String, Value)>)>,
    shown: usize,
    sql: String,
    params: Params,
}

fn insert_events(db: &Db, evs: &[Ev]) {
    let mut t = db.storage().begin().unwrap();
    {
        let mut tt = t.table("events").unwrap();
        for e in evs {
            tt.insert(vec![Value::Int(e.id), Value::Str(e.usr.clone()), Value::Str(e.item.clone()), Value::Str(e.action.into()), Value::Timestamp(e.ts)]).unwrap();
        }
    }
    t.commit().unwrap();
}

fn rows_of(db: &Db, sql: &str, params: &Params) -> strata::query::exec::ResultSet {
    db.query(sql, params).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The values `with_features(sql)` gives each row now.
fn features_now(db: &Db, sql: &str, params: &Params) -> Vec<(String, Value, Vec<(String, Value)>)> {
    let rs = rows_of(db, &with_features(sql).unwrap(), params);
    let k = rs.columns.iter().rposition(|c| c == "score()").unwrap();
    rs.rows
        .iter()
        .zip(&rs.keys)
        .map(|(r, key)| (key.as_str().unwrap().to_string(), r[k].clone(), rs.columns[k + 1..].iter().cloned().zip(r[k + 1..].iter().cloned()).collect()))
        .collect()
}

/// A served value against its exported JSON, bit for bit.
fn same(v: &Value, j: &Json) -> bool {
    match (v, j) {
        (Value::Null, Json::Null) => true,
        (Value::Float(x), _) if !x.is_finite() => j.is_null(),
        (Value::Float(x), Json::Number(n)) => n.as_f64().is_some_and(|y| y.to_bits() == x.to_bits()),
        (Value::Int(x), Json::Number(n)) => n.as_i64() == Some(*x),
        (Value::Str(s), Json::String(t)) => s == t,
        _ => false,
    }
}

fn export(db: &Db, x: &Export) -> Vec<Json> {
    let mut out = Vec::new();
    let n = db.export_rank_log(x, |r| Ok(out.push(r))).unwrap();
    assert_eq!(n as usize, out.len());
    out
}

#[test]
fn the_export_gives_back_what_was_served() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let docs = data::synthetic(1500, DIM, 5);
    let db = Db::create(&path, data::beir_schema(DIM, VectorIndexKind::Flat), opts()).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, action KEYWORD, ts TIMESTAMP) OPTIONS(cluster_by = 'usr')", &Params::new()).unwrap();
    let base = strata::timestamp::parse("2026-10-01T00:00:00Z").unwrap();
    let queries = queries();
    let mut served: HashMap<String, Served> = HashMap::new();
    let mut events: Vec<Ev> = Vec::new();
    let mut next_event = 0i64;
    let mut ev = |usr: &str, item: &str, action: &'static str, ts: i64, out: &mut Vec<Ev>| {
        next_event += 1;
        out.push(Ev { id: next_event, usr: usr.into(), item: item.into(), action, ts });
    };
    for k in 0..48usize {
        let at = base + k as i64 * MINUTE;
        let (sql, served_sql, candidates) = queries[k % queries.len()];
        let d = &docs[(k * 37) % docs.len()];
        let words: Vec<&str> = d[col::TITLE].as_str().unwrap().split(' ').take(3).collect();
        let params = Params::new().set("t", Value::Str(words.join(" "))).set("q", d[col::EMB].clone());
        // Every eleventh request serves no one in particular.
        let context = (k % 11 != 10).then(|| format!("u{}", k % USERS));
        let expected = features_now(&db, served_sql, &params);
        let plain = rows_of(&db, sql, &params);
        let log = Log { request: Some(format!("r{k:02}")), context: context.clone(), candidates, ts: Some(at), variant: Some(format!("v{}", k % 2)), ..Log::default() };
        let got = db.query_logged(sql, &params, &log).unwrap();
        // The answer is the query's own.
        assert_eq!((&got.rs.columns, &got.rs.rows, &got.rs.keys), (&plain.columns, &plain.rows, &plain.keys), "{sql}");
        assert_eq!(got.request, format!("r{k:02}"));
        let shown = plain.rows.len();
        assert!(expected.len() >= shown && !expected.is_empty(), "{sql}");
        served.insert(got.request.clone(), Served { ts: at, context: context.clone(), rows: expected, shown, sql: served_sql.into(), params: params.clone() });
        // The user looked at a row before, clicks the first row served and
        // buys the second, inside the window or after it.
        if let Some(u) = &context {
            let rows = &served[&got.request].rows;
            let mut evs = Vec::new();
            ev(u, &docs[(k * 13) % docs.len()][col::ID].as_str().unwrap().to_string(), "view", at - 20_000_000, &mut evs);
            ev(u, &rows[0].0, "click", at + 30_000_000, &mut evs);
            if rows.len() > 1 {
                ev(u, &rows[1].0, "buy", at + if k % 2 == 0 { 90_000_000 } else { 150_000_000 }, &mut evs);
            }
            insert_events(&db, &evs);
            events.extend(evs);
        }
        // The tables change under the log.
        match k % 6 {
            0 => {
                let mut more = data::synthetic(60, DIM, 100 + k as u64);
                for (i, d) in more.iter_mut().enumerate() {
                    d[col::ID] = Value::Str(format!("n{k}_{i}"));
                }
                db.load(more).unwrap();
            }
            1 => {
                let key = &served[&format!("r{k:02}")].rows[0].0;
                db.sql("UPDATE docs SET rating = 4.75 WHERE id = :k", &Params::new().set("k", Value::Str(key.clone()))).unwrap();
            }
            2 => {
                let key = served[&format!("r{k:02}")].rows.last().unwrap().0.clone();
                db.sql("DELETE FROM docs WHERE id = :k", &Params::new().set("k", Value::Str(key))).unwrap();
            }
            3 => db.storage().flush().unwrap(),
            4 => {
                db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
            }
            _ => {}
        }
    }
    // More changes after the last request: every rating, more rows.
    let SqlResult::Written(n) = db.sql("UPDATE docs SET rating = 0.5 WHERE year >= 1990", &Params::new()).unwrap() else { panic!() };
    assert!(n > 1000);
    let mut more = data::synthetic(400, DIM, 999);
    for (i, d) in more.iter_mut().enumerate() {
        d[col::ID] = Value::Str(format!("late{i}"));
    }
    db.load(more).unwrap();
    db.storage().flush().unwrap();
    // Recomputed now, the values differ: the export must not recompute.
    let moved = served.values().filter(|s| features_now(&db, &s.sql, &s.params) != s.rows).count();
    assert!(moved > served.len() / 2, "only {moved} of {} requests rank differently now", served.len());
    // A judge grades one row.
    db.sql("UPDATE rank_log SET label = 2.5 WHERE id = 'r07#1'", &Params::new()).unwrap();

    let x = Export::from_json(&json!({
        "events": {"table": "events", "user": "usr", "item": "item", "time": "ts", "action": "action"},
        "history": HISTORY, "window": "2 minutes", "grades": {"click": 1, "buy": 3},
    }))
    .unwrap();
    let check = |records: &[Json]| {
        let logged: usize = served.values().map(|s| s.rows.len()).sum();
        assert_eq!(records.len(), logged);
        for r in records {
            let request = r["request"].as_str().unwrap();
            let s = &served[request];
            let position = r["position"].as_u64().unwrap() as usize;
            let (key, score, features) = &s.rows[position];
            assert_eq!(r["id"], json!(format!("{request}#{position}")));
            assert_eq!(r["row_key"].as_str(), Some(key.as_str()), "{request} {position}");
            assert_eq!(r["ts"].as_i64(), Some(s.ts));
            assert_eq!(r["context"].as_str(), s.context.as_deref());
            assert_eq!(r["shown"].as_bool(), Some(position < s.shown));
            assert!(same(score, &r["score"]), "{request} {position}: score {score:?} served, {} exported", r["score"]);
            let f = r["features"].as_object().unwrap();
            assert_eq!(f.len(), features.len(), "{}", r["features"]);
            for (name, v) in features {
                assert!(same(v, &f[name]), "{request} {position} {name}: {v:?} served, {} exported", f[name]);
            }
            // The history: the context's events before the row, the last
            // ones, oldest first.
            let mine: Vec<&Ev> = {
                let mut m: Vec<&Ev> = events.iter().filter(|e| Some(e.usr.as_str()) == s.context.as_deref()).collect();
                m.sort_by_key(|e| (e.ts, e.id));
                m
            };
            let before: Vec<&Ev> = mine.iter().copied().filter(|e| e.ts < s.ts).collect();
            let h = &before[before.len().saturating_sub(HISTORY)..];
            assert_eq!(r["history_items"], json!(h.iter().map(|e| &e.item).collect::<Vec<_>>()), "{request}");
            assert_eq!(r["history_times"], json!(h.iter().map(|e| e.ts).collect::<Vec<_>>()));
            assert_eq!(r["history_actions"], json!(h.iter().map(|e| e.action).collect::<Vec<_>>()));
            // The label: the best grade of the row's events within two
            // minutes after; a judge's grade wins.
            let label = match (request, position) {
                ("r07", 1) => Some(2.5),
                _ if s.context.is_none() => None,
                _ => Some(
                    mine.iter()
                        .filter(|e| e.item == *key && e.ts > s.ts && e.ts <= s.ts + 2 * MINUTE)
                        .map(|e| if e.action == "buy" { 3.0 } else if e.action == "click" { 1.0 } else { f64::NAN })
                        .filter(|g| !g.is_nan())
                        .fold(0.0, f64::max),
                ),
            };
            assert_eq!(r["label"].as_f64(), label, "{request} {position}");
            assert_eq!(r["variant"], json!(format!("v{}", request[1..].parse::<usize>().unwrap() % 2)));
        }
        // Each context's rows in the order they were served.
        let order: Vec<(Option<&str>, i64, u64)> = records.iter().map(|r| (r["context"].as_str(), r["ts"].as_i64().unwrap(), r["position"].as_u64().unwrap())).collect();
        for w in order.windows(2) {
            if w[0].0 == w[1].0 {
                assert!((w[0].1, w[0].2) < (w[1].1, w[1].2), "{w:?}");
            }
        }
    };
    let records = export(&db, &x);
    check(&records);
    // Clicks label the first row of most requests, buys the second of half.
    assert!(records.iter().filter(|r| r["label"].as_f64() == Some(1.0)).count() >= 30);
    assert!(records.iter().filter(|r| r["label"].as_f64() == Some(3.0)).count() >= 15);
    assert!(records.iter().any(|r| r["shown"] == json!(false)));
    // A time range keeps the rows served in it.
    let since = base + 10 * MINUTE;
    let until = base + 20 * MINUTE;
    let part = export(&db, &Export { since: Some(since), until: Some(until), ..x.clone() });
    let want = records.iter().filter(|r| (since..until).contains(&r["ts"].as_i64().unwrap())).count();
    assert_eq!(part.len(), want);
    assert!(want > 0);
    // Without events: the rows and labels written on them.
    let bare = export(&db, &Export::default());
    assert_eq!(bare.len(), records.len());
    assert!(bare.iter().all(|r| r.get("history_items").is_none() && (r["label"].is_null() || r["id"] == json!("r07#1"))));

    // The log is in the WAL and the segments: reopened, the same export.
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    assert_eq!(export(&db, &x), records);
}

#[test]
fn logging_refuses_what_it_cannot_log() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("db"), data::beir_schema(DIM, VectorIndexKind::Flat), opts()).unwrap();
    db.load(data::synthetic(200, DIM, 3)).unwrap();
    let p = Params::new().set("t", Value::Str("kalo mine".into()));
    let log = Log::default();
    for sql in ["SELECT id FROM docs WHERE year > 2000", "SELECT category, COUNT(*) FROM docs GROUP BY category", "SELECT id FROM docs ORDER BY bm25(body, :t) DESC LIMIT 5; SELECT 1"] {
        assert!(db.query_logged(sql, &p, &log).is_err(), "{sql}");
    }
    let candidates = Log { candidates: Some(20), ..Log::default() };
    let e = db.query_logged("SELECT id FROM docs ORDER BY bm25(body, :t) DESC", &p, &candidates).unwrap_err().to_string();
    assert!(e.contains("LIMIT"), "{e}");
    // A table of that name that is not a log is refused.
    db.sql("CREATE TABLE served (id KEYWORD PRIMARY KEY, request KEYWORD)", &Params::new()).unwrap();
    let e = db.query_logged("SELECT id FROM docs ORDER BY bm25(body, :t) DESC LIMIT 5", &p, &Log { table: "served".into(), ..Log::default() }).unwrap_err().to_string();
    assert!(e.contains("not a log table"), "{e}");
    // A generated request id; logging it again rewrites its rows.
    let a = db.query_logged("SELECT id FROM docs ORDER BY bm25(body, :t) DESC LIMIT 5", &p, &log).unwrap();
    assert_eq!(a.request.len(), 32);
    let again = Log { request: Some(a.request.clone()), ..Log::default() };
    db.query_logged("SELECT id FROM docs ORDER BY bm25(body, :t) DESC LIMIT 5", &p, &again).unwrap();
    let rs = db.query("SELECT id FROM rank_log WHERE request = :r", &Params::new().set("r", Value::Str(a.request))).unwrap();
    assert_eq!(rs.rows.len(), 5);
}
