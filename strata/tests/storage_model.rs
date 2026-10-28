//! A model-based randomized test of the storage API, through [`Db`]:
//! random transactions (insert, upsert, conditional upsert, delete, in-place
//! set and add, rollback) on two tables, SQL writes, flushes, merges, vacuums, bulk
//! loads, reopens and pinned snapshots, against a model of every row.
//!
//! After each step the rows of the database must equal the model's, field
//! by field, and so must its lookups and a few queries (ranges, keywords,
//! text, NULLs, counts, a top-k). Every pinned snapshot must keep returning
//! the rows of the moment it was taken, whatever was written, merged or
//! vacuumed since. Seeds alternate the single-file and directory layouts,
//! and snapshots published by the writer or built by readers.
//! `STRATA_MODEL_SEEDS` sets how many seeds run (default 8).

use std::collections::{BTreeMap, BTreeSet};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::query::exec::ExecOptions;
use strata::query::Params;
use strata::storage::{DbOptions, Layout, MergePolicy, Snapshot};
use strata::{Document, Field, FieldType, Schema, Value};

const WORDS: [&str; 12] = ["amber", "basil", "cedar", "delta", "ember", "flint", "grove", "heron", "iris", "jade", "kelp", "lotus"];
const TAGS: [&str; 4] = ["t0", "t1", "t2", "t3"];
/// Keys written by transactions and SQL; bulk loads add keys above.
const KEYS: u64 = 300;

#[derive(Clone, Debug, PartialEq)]
struct Row {
    n: Option<i64>,
    x: Option<f64>,
    tag: String,
    body: String,
}

/// Rows by primary key.
type Model = BTreeMap<String, Row>;

/// The second table's rows: an INT by key.
type Side = BTreeMap<String, Option<i64>>;

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("n", FieldType::Int),
            Field::new("x", FieldType::Float),
            Field::new("tag", FieldType::Keyword),
            Field::new("body", FieldType::Text { positions: false }),
        ],
        Some("id"),
    )
    .unwrap()
}

fn side_schema() -> Schema {
    Schema::new("u", vec![Field::new("id", FieldType::Keyword), Field::new("n", FieldType::Int)], Some("id")).unwrap()
}

fn key(i: u64) -> String {
    format!("k{i:05}")
}

fn opt_int(v: Option<i64>) -> Value {
    v.map_or(Value::Null, Value::Int)
}

fn opt_float(v: Option<f64>) -> Value {
    v.map_or(Value::Null, Value::Float)
}

fn doc(k: &str, r: &Row) -> Document {
    vec![Value::Str(k.to_string()), opt_int(r.n), opt_float(r.x), Value::Str(r.tag.clone()), Value::Str(r.body.clone())]
}

fn random_row(rng: &mut StdRng) -> Row {
    let words: Vec<&str> = (0..rng.gen_range(1..5)).map(|_| WORDS[rng.gen_range(0..WORDS.len())]).collect();
    Row {
        n: rng.gen_bool(0.9).then(|| rng.gen_range(-50..50)),
        // Quarters add exactly, so sums agree with the model's.
        x: rng.gen_bool(0.9).then(|| rng.gen_range(-40..40) as f64 / 4.0),
        tag: TAGS[rng.gen_range(0..TAGS.len())].to_string(),
        body: words.join(" "),
    }
}

fn has_word(r: &Row, w: &str) -> bool {
    r.body.split(' ').any(|x| x == w)
}

fn rows_of(db: &Db, snap: &Snapshot) -> Model {
    let mut out = Model::new();
    for d in db.documents(snap) {
        let d = d.unwrap();
        let k = d[0].as_str().unwrap().to_string();
        let row = Row { n: d[1].as_i64(), x: d[2].as_f64(), tag: d[3].as_str().unwrap().to_string(), body: d[4].as_str().unwrap().to_string() };
        assert!(out.insert(k.clone(), row).is_none(), "{k} is live twice");
    }
    out
}

fn side_of(db: &Db, snap: &Snapshot) -> Side {
    let snap = snap.table("u").expect("table u");
    db.documents(&snap).map(|d| d.unwrap()).map(|d| (d[0].as_str().unwrap().to_string(), d[1].as_i64())).collect()
}

fn keys_of(db: &Db, snap: &Snapshot, sql: &str, p: &Params) -> BTreeSet<String> {
    let r = db.query_at(snap, sql, p, &ExecOptions::default()).unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.keys.iter().map(|k| k.as_str().unwrap().to_string()).collect()
}

fn want(model: &Model, f: impl Fn(&Row) -> bool) -> BTreeSet<String> {
    model.iter().filter(|(_, r)| f(r)).map(|(k, _)| k.clone()).collect()
}

/// Checks a snapshot against its model: every row, some lookups and one
/// query of each kind with random parameters.
fn check(db: &Db, snap: &Snapshot, model: &Model, side: &Side, rng: &mut StdRng, what: &str) {
    assert_eq!(&rows_of(db, snap), model, "{what}: rows");
    assert_eq!(snap.n_live_docs(), model.len() as u64, "{what}: live count");
    assert_eq!(&side_of(db, snap), side, "{what}: rows of u");
    for _ in 0..5 {
        let k = key(rng.gen_range(0..KEYS));
        assert_eq!(snap.lookup(&Value::Str(k.clone())).is_some(), model.contains_key(&k), "{what}: lookup {k}");
    }
    let (a, b) = {
        let a = rng.gen_range(-60..60);
        (a, a + rng.gen_range(0..40))
    };
    let (tag, word) = (TAGS[rng.gen_range(0..TAGS.len())], WORDS[rng.gen_range(0..WORDS.len())]);
    let f = rng.gen_range(-40..40) as f64 / 4.0;
    let p = Params::new()
        .set("a", Value::Int(a))
        .set("b", Value::Int(b))
        .set("t", Value::Str(tag.into()))
        .set("w", Value::Str(word.into()))
        .set("f", Value::Float(f));
    // A comparison with NULL is unknown, and so is its negation.
    let in_range = |r: &Row| r.n.is_some_and(|n| n >= a && n < b);
    for (sql, expected) in [
        ("SELECT id FROM t WHERE n >= :a AND n < :b LIMIT 100000", want(model, in_range)),
        ("SELECT id FROM t WHERE tag = :t LIMIT 100000", want(model, |r| r.tag == tag)),
        ("SELECT id FROM t WHERE MATCH(body, :w) LIMIT 100000", want(model, |r| has_word(r, word))),
        ("SELECT id FROM t WHERE x > :f LIMIT 100000", want(model, |r| r.x.is_some_and(|x| x > f))),
        ("SELECT id FROM t WHERE n IS NULL LIMIT 100000", want(model, |r| r.n.is_none())),
        ("SELECT id FROM t WHERE MATCH(body, :w) AND tag = :t AND NOT (n >= :a AND n < :b) LIMIT 100000", want(model, |r| has_word(r, word) && r.tag == tag && r.n.is_some() && !in_range(r))),
    ] {
        assert_eq!(keys_of(db, snap, sql, &p), expected, "{what}: {sql} with a={a} b={b} t={tag} w={word} f={f}");
    }
    let count = db.query_at(snap, "SELECT COUNT(*) FROM t WHERE tag = :t AND n < :a", &p, &ExecOptions::default()).unwrap();
    assert_eq!(count.rows[0][0], Value::Int(want(model, |r| r.tag == tag && r.n.is_some_and(|n| n < a)).len() as i64), "{what}: count");
    let top = db.query_at(snap, "SELECT id, n FROM t WHERE n IS NOT NULL ORDER BY n DESC LIMIT 7", &p, &ExecOptions::default()).unwrap();
    let got: Vec<i64> = top.rows.iter().map(|r| r[1].as_i64().unwrap()).collect();
    let mut all: Vec<i64> = model.values().filter_map(|r| r.n).collect();
    all.sort_unstable_by(|x, y| y.cmp(x));
    all.truncate(7);
    assert_eq!(got, all, "{what}: top 7 by n");
}

/// One transaction of random writes on both tables, committed or rolled
/// back. Each write's answer (duplicate key, key found) must be the model's.
fn transaction(db: &Db, rng: &mut StdRng, model: &mut Model, side: &mut Side) {
    let (mut local, mut local_side) = (model.clone(), side.clone());
    let mut tx = db.storage().begin().unwrap();
    for _ in 0..rng.gen_range(1..30) {
        let k = key(rng.gen_range(0..KEYS));
        let kv = Value::Str(k.clone());
        match rng.gen_range(0..10) {
            0 | 1 => {
                let r = random_row(rng);
                let ok = tx.insert(doc(&k, &r)).is_ok();
                assert_eq!(ok, !local.contains_key(&k), "insert {k}");
                local.entry(k).or_insert(r);
            }
            2 | 3 => {
                let r = random_row(rng);
                tx.upsert(doc(&k, &r)).unwrap();
                local.insert(k, r);
            }
            4 => {
                if rng.gen_bool(0.5) {
                    assert_eq!(tx.delete(&kv).unwrap(), local.remove(&k).is_some(), "delete {k}");
                } else {
                    // Written only over an older version (NULL is none).
                    let r = Row { n: Some(rng.gen_range(-50..50)), ..random_row(rng) };
                    let newer = local.get(&k).and_then(|o| o.n).is_none_or(|o| o < r.n.unwrap());
                    assert_eq!(tx.upsert_if_newer(doc(&k, &r), "n").unwrap(), newer, "upsert {k} if newer");
                    if newer {
                        local.insert(k, r);
                    }
                }
            }
            5 => {
                let v = rng.gen_bool(0.8).then(|| rng.gen_range(-50..50));
                assert_eq!(tx.set(&kv, "n", opt_int(v)).unwrap(), local.contains_key(&k), "set n of {k}");
                if let Some(r) = local.get_mut(&k) {
                    r.n = v;
                }
            }
            6 => {
                let v = rng.gen_bool(0.8).then(|| rng.gen_range(-40..40) as f64 / 4.0);
                assert_eq!(tx.set(&kv, "x", opt_float(v)).unwrap(), local.contains_key(&k), "set x of {k}");
                if let Some(r) = local.get_mut(&k) {
                    r.x = v;
                }
            }
            7 => {
                let d = rng.gen_range(-5..6);
                assert_eq!(tx.add(&kv, "n", &Value::Int(d)).unwrap(), local.contains_key(&k), "add to n of {k}");
                if let Some(r) = local.get_mut(&k) {
                    r.n = Some(r.n.unwrap_or(0) + d);
                }
            }
            8 => {
                let d = rng.gen_range(-8..8) as f64 / 4.0;
                assert_eq!(tx.add(&kv, "x", &Value::Float(d)).unwrap(), local.contains_key(&k), "add to x of {k}");
                if let Some(r) = local.get_mut(&k) {
                    r.x = Some(r.x.unwrap_or(0.0) + d);
                }
            }
            _ => {
                let mut u = tx.table("u").unwrap();
                match rng.gen_range(0..3) {
                    0 => {
                        let v = rng.gen_bool(0.9).then(|| rng.gen_range(0..100));
                        u.upsert(vec![kv, opt_int(v)]).unwrap();
                        local_side.insert(k, v);
                    }
                    1 => assert_eq!(u.delete(&kv).unwrap(), local_side.remove(&k).is_some(), "delete {k} from u"),
                    _ => {
                        assert_eq!(u.add(&kv, "n", &Value::Int(1)).unwrap(), local_side.contains_key(&k), "add to n of {k} in u");
                        if let Some(n) = local_side.get_mut(&k) {
                            *n = Some(n.unwrap_or(0) + 1);
                        }
                    }
                }
            }
        }
    }
    if rng.gen_bool(0.85) {
        tx.commit().unwrap();
        (*model, *side) = (local, local_side);
    } else {
        tx.rollback();
    }
}

/// One SQL write, whose count of written rows must be the model's.
fn sql_write(db: &Db, rng: &mut StdRng, model: &mut Model) {
    let tag = TAGS[rng.gen_range(0..TAGS.len())];
    let (written, expected) = match rng.gen_range(0..4) {
        0 => {
            let d = rng.gen_range(-5..6);
            let p = Params::new().set("d", Value::Int(d)).set("t", Value::Str(tag.into()));
            let mut n = 0;
            for r in model.values_mut().filter(|r| r.tag == tag) {
                r.n = Some(r.n.unwrap_or(0) + d);
                n += 1;
            }
            (db.sql("UPDATE t SET n = n + :d WHERE tag = :t", &p), n)
        }
        1 => {
            let a = rng.gen_range(-60..60);
            let p = Params::new().set("a", Value::Int(a)).set("b", Value::Int(a + 10)).set("t", Value::Str(tag.into()));
            let mut n = 0;
            for r in model.values_mut().filter(|r| r.n.is_some_and(|n| n >= a && n < a + 10)) {
                r.tag = tag.to_string();
                n += 1;
            }
            (db.sql("UPDATE t SET tag = :t WHERE n >= :a AND n < :b", &p), n)
        }
        2 => {
            let w = WORDS[rng.gen_range(0..WORDS.len())];
            let p = Params::new().set("w", Value::Str(w.into())).set("t", Value::Str(tag.into()));
            let before = model.len();
            model.retain(|_, r| !(has_word(r, w) && r.tag == tag));
            (db.sql("DELETE FROM t WHERE MATCH(body, :w) AND tag = :t", &p), before - model.len())
        }
        _ => {
            let (k, r) = (key(rng.gen_range(0..KEYS)), random_row(rng));
            let p = Params::new()
                .set("id", Value::Str(k.clone()))
                .set("n", opt_int(r.n))
                .set("x", opt_float(r.x))
                .set("tag", Value::Str(r.tag.clone()))
                .set("body", Value::Str(r.body.clone()));
            model.insert(k, r);
            (db.sql("INSERT OR REPLACE INTO t (id, n, x, tag, body) VALUES (:id, :n, :x, :tag, :body)", &p), 1)
        }
    };
    match written.unwrap() {
        SqlResult::Written(n) => assert_eq!(n, expected as u64, "rows written"),
        other => panic!("a write returned {other:?}"),
    }
}

fn run(seed: u64, steps: usize) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.strata");
    let mut rng = StdRng::seed_from_u64(seed);
    let layout = if seed % 4 < 2 { Layout::SingleFile } else { Layout::Directory };
    let o = DbOptions { memtable_max_docs: 120, memtable_chunk_docs: 32, segment_max_docs: 100, sync: false, publish_snapshots: seed % 2 == 1, layout, ..DbOptions::default() };
    let policy = MergePolicy { merge_factor: 3, floor_docs: 30, max_deleted_ratio: 0.3, ..MergePolicy::default() };
    let mut db = Some(Db::create(&path, schema(), o.clone()).unwrap());
    db.as_ref().unwrap().create_table(side_schema()).unwrap();
    let (mut model, mut side) = (Model::new(), Side::new());
    let mut pinned: Vec<(Snapshot, Model, Side, usize)> = Vec::new();
    let mut next_bulk = KEYS;
    for step in 0..steps {
        let d = db.as_ref().unwrap();
        let op = rng.gen_range(0..100);
        match op {
            0..=49 => transaction(d, &mut rng, &mut model, &mut side),
            50..=59 => sql_write(d, &mut rng, &mut model),
            60..=66 => d.storage().flush().unwrap(),
            67..=74 => {
                d.storage().merge(&policy).unwrap();
            }
            75..=77 => {
                d.storage().vacuum().unwrap();
            }
            78..=81 => {
                let rows: Vec<(String, Row)> = (0..rng.gen_range(1..40)).map(|i| (key(next_bulk + i), random_row(&mut rng))).collect();
                next_bulk += rows.len() as u64;
                d.load(rows.iter().map(|(k, r)| doc(k, r))).unwrap();
                model.extend(rows);
            }
            82..=88 => {
                if pinned.len() == 3 {
                    pinned.swap_remove(rng.gen_range(0..3));
                }
                pinned.push((d.snapshot(), model.clone(), side.clone(), step));
            }
            89..=94 => {
                if !pinned.is_empty() {
                    pinned.swap_remove(rng.gen_range(0..pinned.len()));
                }
            }
            _ => {
                // A snapshot keeps its database open, and the file lock
                // admits one handle.
                pinned.clear();
                drop(db.take());
                db = Some(Db::open(&path, o.clone()).unwrap());
            }
        }
        let d = db.as_ref().unwrap();
        check(d, &d.snapshot(), &model, &side, &mut rng, &format!("seed {seed} step {step} (op {op})"));
        for (snap, m, s, at) in &pinned {
            check(d, snap, m, s, &mut rng, &format!("seed {seed} step {step}: snapshot pinned at step {at}"));
        }
        if step % 50 == 49 {
            d.storage().check().unwrap();
        }
    }
    db.unwrap().storage().check().unwrap();
}

#[test]
fn random_operations_match_the_model() {
    let seeds: u64 = std::env::var("STRATA_MODEL_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    for seed in 0..seeds {
        run(seed, 300);
    }
}
