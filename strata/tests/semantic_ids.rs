//! Semantic-ID fields: the writer fills every prefix of a row's code from
//! its vector, prefix queries are exact, codes stay the same across
//! flushes, merges and reopening, a tie number is never given twice (also
//! after its row is deleted and merged away), upserts keep a code whose
//! levels do not change, and a table records the codebook it was made with.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use strata::db::Db;
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::sid::{self, Codebook, TrainOptions};
use strata::storage::{DbOptions, MergePolicy};
use strata::value::Value;
use strata::VectorIndexKind;

/// Two levels of four centroids in two dimensions: a vector near
/// `L0[a] + L1[b]` has code `a.b`.
const L0: [[f32; 2]; 4] = [[0.0, 0.0], [10.0, 0.0], [0.0, 10.0], [10.0, 10.0]];
const L1: [[f32; 2]; 4] = [[-1.0, -1.0], [1.0, -1.0], [-1.0, 1.0], [1.0, 1.0]];

fn toy() -> Codebook {
    Codebook::new(2, 4, vec![L0.concat(), L1.concat()]).unwrap()
}

/// A vector whose code is `a.b`, moved by `eps` (below 0.5).
fn at(a: usize, b: usize, eps: f32) -> Arc<[f32]> {
    Arc::from(vec![L0[a][0] + L1[b][0] + eps, L0[a][1] + L1[b][1] - eps])
}

fn schema() -> Schema {
    Schema::new(
        "items",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("tag", FieldType::Keyword),
            Field::new("emb", FieldType::Vector { dim: 2, index: VectorIndexKind::Flat }),
            Field::semantic_id("sid", "emb", "toy"),
        ],
        Some("id"),
    )
    .unwrap()
}

fn row(id: &str, tag: &str, v: Arc<[f32]>) -> Vec<Value> {
    vec![Value::Str(id.into()), Value::Str(tag.into()), Value::Vector(v), Value::Null]
}

/// A database of `schema()` with the toy codebook in `dir/models`.
fn create(dir: &Path, opts: DbOptions) -> Db {
    toy().save("toy", &dir.join("models")).unwrap();
    Db::create_in(&dir.join("items.strata"), schema(), opts, &dir.join("models")).unwrap()
}

/// Every row's code, by id.
fn codes(db: &Db) -> BTreeMap<String, Vec<u32>> {
    let rs = db.query("SELECT id, sid FROM items LIMIT 100000", &Params::new()).unwrap();
    rs.rows
        .iter()
        .map(|r| {
            let code = sid::code_of(&r[1]).unwrap_or_else(|| panic!("{:?} has no code: {:?}", r[0], r[1]));
            (r[0].as_str().unwrap().to_string(), code)
        })
        .collect()
}

/// A code as text: `12.7.0`.
fn text(code: &[u32]) -> String {
    code.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(".")
}

fn ids(db: &Db, sql: &str, params: &Params) -> BTreeSet<String> {
    db.query(sql, params).unwrap().rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

fn insert(db: &Db, rows: Vec<Vec<Value>>) {
    let mut t = db.storage().begin().unwrap();
    for r in rows {
        t.insert(r).unwrap();
    }
    t.commit().unwrap();
}

#[test]
fn codes_are_filled_and_prefix_queries_are_exact() {
    let dir = tempfile::tempdir().unwrap();
    let db = create(dir.path(), DbOptions { memtable_max_docs: 50, ..DbOptions::default() });
    // A bulk load, transactions and SQL all get codes.
    let mut vectors = HashMap::new();
    let mut n = 0;
    let mut next = |vectors: &mut HashMap<String, Arc<[f32]>>| {
        n += 1;
        let (a, b) = ((n * 7) % 4, (n * 3 + n / 5) % 4);
        let v = at(a, b, (n % 9) as f32 * 0.03);
        let id = format!("i{n:04}");
        vectors.insert(id.clone(), v.clone());
        row(&id, if n % 3 == 0 { "red" } else { "blue" }, v)
    };
    db.load((0..120).map(|_| next(&mut vectors)).collect::<Vec<_>>()).unwrap();
    for _ in 0..4 {
        insert(&db, (0..40).map(|_| next(&mut vectors)).collect());
    }
    for _ in 0..5 {
        let r = next(&mut vectors);
        let p = Params::new().set("id", r[0].clone()).set("tag", r[1].clone()).set("v", r[2].clone());
        db.sql("INSERT INTO items (id, tag, emb) VALUES (:id, :tag, :v)", &p).unwrap();
    }
    let got = codes(&db);
    assert_eq!(got.len(), 120 + 160 + 5);
    // Each code is the codebook's levels and a tie; ties under a prefix are
    // 0, 1, 2, ... in the order rows were written.
    let cb = toy();
    let mut ties: BTreeMap<Vec<u32>, Vec<u32>> = BTreeMap::new();
    for (id, code) in &got {
        assert_eq!(code[..2], cb.code(&vectors[id])[..], "{id}");
        ties.entry(code[..2].to_vec()).or_default().push(code[2]);
    }
    for (prefix, mut t) in ties {
        t.sort();
        assert_eq!(t, (0..t.len() as u32).collect::<Vec<_>>(), "{prefix:?}");
    }
    // Every prefix of every code, alone and under a filter, matches the
    // rows a scan of the codes finds.
    let mut prefixes = BTreeSet::new();
    for code in got.values() {
        for n in 1..=code.len() {
            prefixes.insert(text(&code[..n]));
        }
    }
    let tags: HashMap<String, String> = db.query("SELECT id, tag FROM items LIMIT 1000", &Params::new()).unwrap().rows.iter().map(|r| (r[0].as_str().unwrap().into(), r[1].as_str().unwrap().into())).collect();
    for p in &prefixes {
        let want: BTreeSet<String> = got.iter().filter(|(_, c)| text(c) == *p || text(c).starts_with(&format!("{p}."))).map(|(id, _)| id.clone()).collect();
        let params = Params::new().set("p", Value::Str(p.clone()));
        assert_eq!(ids(&db, "SELECT id FROM items WHERE sid = :p LIMIT 1000", &params), want, "{p}");
        let red: BTreeSet<String> = want.iter().filter(|id| tags[*id] == "red").cloned().collect();
        assert_eq!(ids(&db, "SELECT id FROM items WHERE sid = :p AND tag = 'red' LIMIT 1000", &params), red, "{p} red");
    }
    // A row without a vector has no code.
    insert(&db, vec![vec![Value::Str("none".into()), Value::Str("blue".into()), Value::Null, Value::Null]]);
    let rs = db.query("SELECT sid FROM items WHERE id = 'none'", &Params::new()).unwrap();
    assert!(matches!(&rs.rows[0][0], Value::Null) || matches!(&rs.rows[0][0], Value::List(xs) if xs.is_empty()), "{:?}", rs.rows);
}

#[test]
fn codes_stay_across_flushes_merges_and_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("items.strata");
    let opts = || DbOptions { memtable_max_docs: 30, ..DbOptions::default() };
    let db = create(dir.path(), opts());
    for k in 0..8 {
        insert(&db, (0..25).map(|i| row(&format!("r{k}-{i}"), "t", at(i % 4, (i / 4) % 4, 0.01 * k as f32))).collect());
    }
    let before = codes(&db);
    db.storage().flush().unwrap();
    assert!(db.snapshot().segments().len() > 2);
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
    assert_eq!(db.snapshot().segments().len(), 1);
    assert_eq!(codes(&db), before);
    drop(db);
    let db = Db::open(&path, opts()).unwrap();
    assert!(db.open_embedders(&dir.path().join("models")).is_empty());
    assert_eq!(codes(&db), before);
    // New rows continue each prefix's ties.
    insert(&db, (0..16).map(|i| row(&format!("new{i}"), "t", at(i % 4, i / 4, 0.2))).collect());
    let after = codes(&db);
    let full: BTreeSet<&Vec<u32>> = after.values().collect();
    assert_eq!(full.len(), after.len(), "every code is given once");
    for i in 0..16 {
        let code = &after[&format!("new{i}")];
        let under = before.values().filter(|c| c[..2] == code[..2]).count() as u32;
        assert_eq!(code[2], under, "new{i}: {code:?}");
    }
}

#[test]
fn deleted_numbers_are_not_given_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("items.strata");
    let models = dir.path().join("models");
    let db = create(dir.path(), DbOptions::default());
    insert(&db, (0..3).map(|i| row(&format!("a{i}"), "t", at(1, 2, 0.0))).collect());
    insert(&db, vec![row("other", "t", at(0, 0, 0.0))]);
    assert_eq!(codes(&db)["a2"], [1, 2, 2]);
    db.sql("DELETE FROM items WHERE id IN ('a1', 'a2')", &Params::new()).unwrap();
    insert(&db, vec![row("b", "t", at(1, 2, 0.0))]);
    assert_eq!(codes(&db)["b"], [1, 2, 3]);
    // Flushed, merged away (the deleted rows are dropped), reopened.
    db.storage().flush().unwrap();
    db.sql("DELETE FROM items WHERE id = 'b'", &Params::new()).unwrap();
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, max_deleted_ratio: 0.0, ..MergePolicy::default() }).unwrap();
    assert_eq!(db.snapshot().segments().iter().map(|s| s.file.n_docs()).sum::<usize>(), 2);
    drop(db);
    let db = Db::open(&path, DbOptions::default()).unwrap();
    db.open_embedders(&models);
    insert(&db, vec![row("c", "t", at(1, 2, 0.1))]);
    assert_eq!(codes(&db)["c"], [1, 2, 4]);

    // Rows written and deleted before any flush: the flush that drops them
    // keeps their numbers, in a segment without documents if need be.
    insert(&db, vec![row("d", "t", at(3, 3, 0.0))]);
    db.sql("DELETE FROM items WHERE id = 'd'", &Params::new()).unwrap();
    db.storage().flush().unwrap();
    drop(db);
    let db = Db::open(&path, DbOptions::default()).unwrap();
    db.open_embedders(&models);
    insert(&db, vec![row("e", "t", at(3, 3, 0.0))]);
    assert_eq!(codes(&db)["e"], [3, 3, 1]);
    // Every row deleted and merged into nothing: the numbers stay.
    db.sql("DELETE FROM items WHERE id IN ('a0', 'other', 'c', 'e')", &Params::new()).unwrap();
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, max_deleted_ratio: 0.0, ..MergePolicy::default() }).unwrap();
    assert_eq!(db.snapshot().n_live_docs(), 0);
    drop(db);
    let db = Db::open(&path, DbOptions::default()).unwrap();
    db.open_embedders(&models);
    insert(&db, vec![row("f", "t", at(3, 3, 0.0)), row("g", "t", at(1, 2, 0.0)), row("h", "t", at(0, 0, 0.0))]);
    let got = codes(&db);
    assert_eq!((&got["f"], &got["g"], &got["h"]), (&vec![3, 3, 2], &vec![1, 2, 5], &vec![0, 0, 1]));
    // The database stays whole.
    db.storage().check().unwrap();
}

#[test]
fn upserts_keep_codes_whose_levels_stay() {
    let dir = tempfile::tempdir().unwrap();
    let db = create(dir.path(), DbOptions::default());
    insert(&db, vec![row("a", "t", at(1, 2, 0.0)), row("b", "t", at(1, 2, 0.1))]);
    assert_eq!(codes(&db)["a"], [1, 2, 0]);
    // The same levels: the same code, by upsert and by SQL.
    let mut t = db.storage().begin().unwrap();
    t.upsert(row("a", "u", at(1, 2, 0.2))).unwrap();
    t.commit().unwrap();
    assert_eq!(codes(&db)["a"], [1, 2, 0]);
    let p = Params::new().set("v", Value::Vector(at(1, 2, 0.3)));
    db.sql("INSERT OR REPLACE INTO items (id, tag, emb) VALUES ('a', 'v', :v)", &p).unwrap();
    assert_eq!(codes(&db)["a"], [1, 2, 0]);
    db.sql("UPDATE items SET tag = 'w' WHERE id = 'a'", &Params::new()).unwrap();
    assert_eq!(codes(&db)["a"], [1, 2, 0]);
    // A vector with other levels: a new code, and back again a new tie.
    db.sql("UPDATE items SET emb = :v WHERE id = 'a'", &Params::new().set("v", Value::Vector(at(2, 0, 0.0)))).unwrap();
    assert_eq!(codes(&db)["a"], [2, 0, 0]);
    db.sql("UPDATE items SET emb = :v WHERE id = 'a'", &Params::new().set("v", Value::Vector(at(1, 2, 0.0)))).unwrap();
    assert_eq!(codes(&db)["a"], [1, 2, 2]);
    // The code is derived, not set.
    let e = db.sql("UPDATE items SET sid = ARRAY['1', '1.2', '1.2.9'] WHERE id = 'a'", &Params::new()).unwrap_err().to_string();
    assert!(e.contains("semantic ID"), "{e}");
    // A code given in full is kept; levels alone get a tie.
    let given = |id: &str, code: &[&str]| {
        let mut r = row(id, "t", at(0, 1, 0.0));
        r[3] = Value::List(code.iter().map(|c| Value::Str(c.to_string())).collect());
        r
    };
    insert(&db, vec![given("g1", &["3", "3.1", "3.1.7"]), given("g2", &["3", "3.1"])]);
    let got = codes(&db);
    assert_eq!((&got["g1"], &got["g2"]), (&vec![3, 1, 7], &vec![3, 1, 8]));
    let mut t = db.storage().begin().unwrap();
    assert!(t.insert(given("bad", &["3", "3.1", "3.1.2", "3.1.2.0"])).is_err());
    assert!(t.insert(given("bad", &["9", "9.1"])).is_err());
    assert!(t.insert(given("bad", &["3.1", "3.1.2"])).is_err());
}

#[test]
fn a_table_records_its_codebook() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    let db = create(dir.path(), DbOptions::default());
    let s = db.schema().field("sid").unwrap().semantic_id.clone().unwrap();
    assert_eq!((s.from.as_str(), s.codebook.as_str(), s.hash.clone()), ("emb", "toy", sid::hash(&toy().to_bytes())));
    assert!(db.schema().features().any(|f| f == "semantic_id"));
    // The short JSON form keeps it.
    let back = Schema::from_json_value(&db.schema().to_short_json()).unwrap();
    assert_eq!(&back, db.schema());

    // SQL: a table made with a codebook in the database's models directory.
    assert!(db.open_embedders(&models).is_empty());
    db.sql("CREATE TABLE more (id KEYWORD PRIMARY KEY, emb VECTOR(2) OPTIONS(index = 'flat'), sid KEYWORD[] OPTIONS(semantic_id = 'emb', codebook = 'toy'))", &Params::new()).unwrap();
    let p = Params::new().set("v", Value::Vector(at(2, 3, 0.0)));
    db.sql("INSERT INTO more (id, emb) VALUES ('m', :v)", &p).unwrap();
    let rs = db.query("SELECT sid FROM more WHERE sid = '2.3.0'", &Params::new()).unwrap();
    assert_eq!(rs.rows.len(), 1);
    // Without the codebook, or with one of other dimensions, or on a field
    // that is not a vector's list of codes, no table.
    for (sql, why) in [
        ("CREATE TABLE x (id KEYWORD PRIMARY KEY, emb VECTOR(2), sid KEYWORD[] OPTIONS(semantic_id = 'emb', codebook = 'nope'))", "nope"),
        ("CREATE TABLE x (id KEYWORD PRIMARY KEY, emb VECTOR(3), sid KEYWORD[] OPTIONS(semantic_id = 'emb', codebook = 'toy'))", "3"),
        ("CREATE TABLE x (id KEYWORD PRIMARY KEY, emb VECTOR(2), sid KEYWORD[] OPTIONS(semantic_id = 'id', codebook = 'toy'))", "VECTOR"),
        ("CREATE TABLE x (id KEYWORD PRIMARY KEY, emb VECTOR(2), sid KEYWORD OPTIONS(semantic_id = 'emb', codebook = 'toy'))", "semantic_id"),
    ] {
        let e = db.sql(sql, &Params::new()).unwrap_err().to_string();
        assert!(e.contains(why), "{sql}: {e}");
    }
    drop(db);

    // Opened elsewhere, without the codebook: rows with a vector cannot get
    // codes, and say why; codes given in full are kept.
    let db = Db::open(&dir.path().join("items.strata"), DbOptions::default()).unwrap();
    let mut t = db.storage().begin().unwrap();
    let e = t.insert(row("x", "t", at(0, 0, 0.0))).unwrap_err().to_string();
    assert!(e.contains("codebook toy is not open"), "{e}");
    let mut r = row("y", "t", at(0, 0, 0.0));
    r[3] = sid::value(&[0, 0], 5);
    t.insert(r).unwrap();
    t.commit().unwrap();
    // Another codebook under the same name is refused by its hash.
    let other = dir.path().join("other");
    Codebook::new(2, 4, vec![L1.concat(), L0.concat()]).unwrap().save("toy", &other).unwrap();
    let failed = db.open_codebooks(&other);
    assert!(failed.len() == 2 && failed[0].1.to_string().contains("hash"), "{failed:?}");
    assert!(db.open_codebooks(&models).is_empty());
    insert(&db, vec![row("x", "t", at(0, 0, 0.0))]);
    assert_eq!(codes(&db)["x"], [0, 0, 6]);
}

#[test]
fn a_codebook_trains_on_a_tables_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    let src = Schema::new("docs", vec![Field::new("id", FieldType::Keyword), Field::new("emb", FieldType::Vector { dim: 2, index: VectorIndexKind::Flat })], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("docs.strata"), src, DbOptions::default()).unwrap();
    db.load((0..400).map(|i| vec![Value::Str(format!("d{i}")), Value::Vector(at(i % 4, (i / 4) % 4, (i % 7) as f32 * 0.05))])).unwrap();
    let opts = TrainOptions { levels: 2, k: 4, iters: 20, ..TrainOptions::default() };
    let hash = db.train_codebook("docs", "emb", "learned", &models, &opts).unwrap();
    let (cb, h) = Codebook::open("learned", &models).unwrap();
    assert_eq!((h, cb.n_levels(), cb.k), (hash, 2, 4));
    // Rows near one point of the toy grid share a level-1 code.
    let a = cb.code(&at(3, 1, 0.0));
    assert_eq!(cb.code(&at(3, 1, 0.2))[0], a[0]);
    assert_ne!(cb.code(&at(0, 1, 0.0))[0], a[0]);
    // A table made with it.
    let mut s = schema();
    s.fields[3].semantic_id.as_mut().unwrap().codebook = "learned".into();
    let items = Db::create_in(&dir.path().join("items.strata"), s, DbOptions::default(), &models).unwrap();
    insert(&items, vec![row("a", "t", at(3, 1, 0.0))]);
    assert_eq!(codes(&items)["a"], [a[0], a[1], 0]);
}

/// `sid_at(sid, n)` cuts a code to its first `n` numbers, in SELECT, WHERE
/// and GROUP BY, and a plan that uses it needs feature "semantic_id".
#[test]
fn sid_at_cuts_a_code() {
    let dir = tempfile::tempdir().unwrap();
    let db = create(dir.path(), DbOptions::default());
    insert(&db, vec![row("a", "red", at(1, 2, 0.0)), row("b", "red", at(1, 2, 0.1)), row("c", "blue", at(1, 3, 0.0)), row("d", "blue", at(2, 0, 0.0))]);
    let rs = db.query("SELECT id, sid_at(sid, 1) AS l1, sid_at(sid, 2) AS l2, sid_at(sid, 3) AS l3, sid_at(sid, 4) AS l4, sid_at(sid, 0) AS l0 FROM items ORDER BY id LIMIT 10", &Params::new()).unwrap();
    let got: Vec<Vec<Value>> = rs.rows.iter().map(|r| r[1..].to_vec()).collect();
    let s = |x: &str| Value::Str(x.into());
    assert_eq!(
        got,
        [
            [s("1"), s("1.2"), s("1.2.0"), Value::Null, Value::Null],
            [s("1"), s("1.2"), s("1.2.1"), Value::Null, Value::Null],
            [s("1"), s("1.3"), s("1.3.0"), Value::Null, Value::Null],
            [s("2"), s("2.0"), s("2.0.0"), Value::Null, Value::Null],
        ]
    );
    assert_eq!(ids(&db, "SELECT id FROM items WHERE sid_at(sid, 2) = '1.2' LIMIT 10", &Params::new()), ["a", "b"].map(String::from).into());
    let rs = db.query("SELECT sid_at(sid, 1) AS l1, COUNT(*) AS n FROM items GROUP BY sid_at(sid, 1) ORDER BY l1", &Params::new());
    match rs {
        Ok(rs) => assert_eq!(rs.rows, [vec![s("1"), Value::Int(3)], vec![s("2"), Value::Int(1)]]),
        Err(e) => panic!("GROUP BY sid_at: {e}"),
    }
    assert!(db.query("SELECT sid_at(sid) FROM items", &Params::new()).unwrap_err().to_string().contains("two arguments"));
    assert!(db.query("SELECT sid_at(sid, 'x') FROM items", &Params::new()).unwrap_err().to_string().contains("level number"));
    let plan = db.explain("SELECT id, sid_at(sid, 1) FROM items LIMIT 5", &Params::new()).unwrap();
    assert!(plan.features.iter().any(|f| f == strata::query::ir::SEMANTIC_ID), "{:?}", plan.features);
    let e = strata::query::ir::Plan::readable_by(1, &[strata::query::ir::SEMANTIC_ID], &[strata::query::ir::OPERATORS]).unwrap_err();
    assert!(e.to_string().contains("semantic_id"), "{e}");
    let plan = db.explain("SELECT id FROM items WHERE sid = '1' LIMIT 5", &Params::new()).unwrap();
    assert!(plan.features.is_empty(), "{:?}", plan.features);
}

/// The next codes of a prefix among the rows a filter matches are the
/// children of the prefix in a trie of those rows' codes, with the rows
/// under each: across segments, the memtable and deleted rows.
#[test]
fn next_codes_are_a_tries_children() {
    let dir = tempfile::tempdir().unwrap();
    let db = create(dir.path(), DbOptions { memtable_max_docs: 120, ..DbOptions::default() });
    let mut n = 0;
    let mut next = || {
        n += 1;
        let (a, b) = ((n * 5 + n / 7) % 4, (n * 3 + n / 11) % 4);
        row(&format!("i{n:04}"), ["red", "blue", "green"][n % 3], at(a, b, (n % 9) as f32 * 0.03))
    };
    db.load((0..300).map(|_| next()).collect::<Vec<_>>()).unwrap();
    for _ in 0..4 {
        insert(&db, (0..50).map(|_| next()).collect());
    }
    let mut t = db.storage().begin().unwrap();
    for i in (13..=500).step_by(13) {
        t.delete(&Value::Str(format!("i{i:04}"))).unwrap();
    }
    t.commit().unwrap();
    db.storage().merge(&MergePolicy::default()).unwrap();
    insert(&db, (0..40).map(|_| next()).collect());
    let mut t = db.storage().begin().unwrap();
    t.delete(&Value::Str("i0510".into())).unwrap();
    t.commit().unwrap();

    let rs = db.query("SELECT id, tag, sid FROM items LIMIT 100000", &Params::new()).unwrap();
    let rows: Vec<(String, Vec<u32>)> = rs.rows.iter().map(|r| (r[1].as_str().unwrap().to_string(), sid::code_of(&r[2]).unwrap())).collect();
    assert_eq!(rows.len(), 540 - 38 - 1);
    assert!(rows.iter().any(|(_, c)| c[2] >= 10), "ties past 9 test the dictionary's order");
    let mut prefixes = BTreeSet::from([String::new(), "3.3.999".into(), "9".into(), "0.0.0".into(), "2.1.10".into()]);
    for (_, c) in &rows {
        for k in 1..=c.len() {
            prefixes.insert(text(&c[..k]));
        }
    }
    let prefixes: Vec<String> = prefixes.into_iter().collect();
    let filters: [(Option<&str>, &dyn Fn(&str, &[u32]) -> bool); 5] = [
        (None, &|_, _| true),
        (Some("tag = 'red'"), &|t, _| t == "red"),
        (Some("tag IN ('red', 'green') AND sid != '1'"), &|t, c| t != "blue" && c[0] != 1),
        (Some("sid = '2.1'"), &|_, c| c[..2] == [2, 1]),
        (Some("tag = 'nope'"), &|_, _| false),
    ];
    for (filter, keep) in filters {
        for chunk in prefixes.chunks(sid::MAX_PREFIXES) {
            let got = db.sid_next("items", "sid", chunk, filter, &Params::new()).unwrap();
            for (p, got) in chunk.iter().zip(got) {
                let depth = if p.is_empty() { 0 } else { p.split('.').count() };
                let mut want: BTreeMap<u32, u64> = BTreeMap::new();
                for (tag, c) in &rows {
                    if c.len() > depth && text(&c[..depth]) == *p && keep(tag, c) {
                        *want.entry(c[depth]).or_insert(0) += 1;
                    }
                }
                let want: Vec<(String, u64)> = want.into_iter().map(|(x, k)| (if p.is_empty() { x.to_string() } else { format!("{p}.{x}") }, k)).collect();
                assert_eq!(got, want, "{p:?} under {filter:?}");
            }
        }
    }
    // Parameters, and the errors.
    let p = Params::new().set("t", Value::Str("blue".into()));
    let blue = db.sid_next("items", "sid", &[String::new()], Some("tag = :t"), &p).unwrap();
    assert_eq!(blue[0].iter().map(|(_, k)| k).sum::<u64>(), rows.iter().filter(|(t, _)| t == "blue").count() as u64);
    let e = |prefixes: &[String], field: &str| db.sid_next("items", field, prefixes, None, &Params::new()).unwrap_err().to_string();
    assert!(e(&vec![String::new(); 65], "sid").contains("1 to 64"));
    assert!(e(&[], "sid").contains("1 to 64"));
    assert!(e(&["1..2".into()], "sid").contains("not a prefix"));
    assert!(e(&["01".into()], "sid").contains("not a prefix"));
    assert!(e(&[String::new()], "tag").contains("not a semantic-ID field"));
    assert!(db.sid_next("items", "sid", &[String::new()], Some("nope = 1"), &Params::new()).is_err());
}
