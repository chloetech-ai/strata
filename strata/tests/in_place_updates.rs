//! Numeric fields set in place (`WriteTxn::set`): filters, orders,
//! projections and counts see the new values, in the memtable and on disk,
//! before and after a flush, after reopening (the WAL), after a merge and
//! in both layouts, and the file stays consistent.

use strata::data::{self, col};
use strata::db::Db;
use strata::query::Params;
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::value::{Document, Value};
use strata::VectorIndexKind;

const N: usize = 2400;

/// Rows of `sql` as (id, year, rating).
fn rows(db: &Db, sql: &str, params: &Params) -> Vec<(String, i64, f64)> {
    db.query(sql, params)
        .unwrap()
        .rows
        .iter()
        .map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap_or(i64::MIN), r[2].as_f64().unwrap_or(f64::NAN)))
        .collect()
}

fn ids(db: &Db, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = db.query(sql, &Params::new()).unwrap().rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

fn want(docs: &[Document], f: impl Fn(&Document) -> bool) -> Vec<String> {
    let mut v: Vec<String> = docs.iter().filter(|d| f(d)).map(|d| d[col::ID].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

fn rating(d: &Document) -> Option<f64> {
    d[col::RATING].as_f64()
}

fn year(d: &Document) -> Option<i64> {
    d[col::YEAR].as_i64()
}

fn check(db: &Db, docs: &[Document]) {
    let q = "SELECT id, year, rating FROM docs";
    assert_eq!(ids(db, &format!("{q} WHERE rating BETWEEN 1 AND 2")), want(docs, |d| rating(d).is_some_and(|r| (1.0..=2.0).contains(&r))));
    assert_eq!(ids(db, &format!("{q} WHERE year = 2010")), want(docs, |d| year(d) == Some(2010)));
    assert_eq!(ids(db, &format!("{q} WHERE year IN (2001, 2030)")), want(docs, |d| matches!(year(d), Some(2001 | 2030))));
    assert_eq!(
        ids(db, &format!("{q} WHERE category = 'c00' AND rating < 1")),
        want(docs, |d| d[col::CATEGORY].as_str() == Some("c00") && rating(d).is_some_and(|r| r < 1.0))
    );
    let count = db.query("SELECT COUNT(*) FROM docs WHERE rating > 4", &Params::new()).unwrap();
    assert_eq!(count.rows[0][0], Value::Int(want(docs, |d| rating(d).is_some_and(|r| r > 4.0)).len() as i64));

    // Top by the updated column, ties by key.
    let got = rows(db, &format!("{q} ORDER BY rating DESC, id LIMIT 25"), &Params::new());
    let mut all: Vec<(String, i64, f64)> =
        docs.iter().filter_map(|d| Some((d[col::ID].as_str()?.to_string(), year(d).unwrap_or(i64::MIN), rating(d)?))).collect();
    all.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap().then_with(|| a.0.cmp(&b.0)));
    all.truncate(25);
    assert_eq!(got, all);

    // A filtered vector search only returns documents passing the new values.
    let params = Params::new().set("q", docs[5][col::EMB].clone());
    let got = rows(db, &format!("{q} WHERE rating >= 4.5 ORDER BY similarity(emb, :q) DESC LIMIT 10"), &params);
    assert_eq!(got.len(), 10);
    for (id, _, r) in &got {
        let d = docs.iter().find(|d| d[col::ID].as_str() == Some(id)).unwrap();
        assert!(r >= &4.5 && rating(d) == Some(*r), "{id}: {r} vs {:?}", rating(d));
    }
    db.storage().check().unwrap();
}

/// Sets `rating` (and every third time `year`) of `n` documents from
/// `start`, stepping through the collection, in `docs` and the database.
fn set_some(db: &Db, docs: &mut [Document], start: usize, n: usize, salt: i64) {
    let mut t = db.storage().begin().unwrap();
    for k in 0..n {
        let i = (start + k * 37) % docs.len();
        let id = docs[i][col::ID].clone();
        let r = ((i as i64 * 7 + salt * 13) % 500) as f64 / 100.0;
        assert!(t.set(&id, "rating", Value::Float(r)).unwrap());
        docs[i][col::RATING] = Value::Float(r);
        if k % 3 == 0 {
            let y = 2000 + (i as i64 + salt) % 31;
            assert!(t.set(&id, "year", Value::Int(y)).unwrap());
            docs[i][col::YEAR] = Value::Int(y);
        }
    }
    t.commit().unwrap();
}

fn run(name: &str, layout: Layout) {
    let mut docs = data::synthetic(N, 8, 21);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    let opts = || DbOptions { segment_max_docs: 500, layout, ..DbOptions::default() };
    {
        let db = Db::create(&path, data::beir_schema(8, VectorIndexKind::Flat), opts()).unwrap();
        db.load(docs[..2000].iter().cloned()).unwrap();
        let mut t = db.storage().begin().unwrap();
        for d in &docs[2000..] {
            t.insert(d.clone()).unwrap();
        }
        t.commit().unwrap();

        // Refused: unknown keys, non-numeric fields, the key, bad values.
        let mut t = db.storage().begin().unwrap();
        assert!(!t.set(&Value::Str("nope".into()), "rating", Value::Float(1.0)).unwrap());
        assert!(t.set(&docs[0][col::ID], "category", Value::Str("c01".into())).is_err());
        assert!(t.set(&docs[0][col::ID], "id", Value::Str("x".into())).is_err());
        assert!(t.set(&docs[0][col::ID], "year", Value::Float(2.5)).is_err());
        t.rollback();

        // Disk segments and the memtable both.
        set_some(&db, &mut docs, 3, 400, 1);
        set_some(&db, &mut docs, 2001, 60, 2);
        check(&db, &docs);
        // The same field set twice in one transaction keeps the last value.
        let mut t = db.storage().begin().unwrap();
        t.set(&docs[10][col::ID], "rating", Value::Float(0.25)).unwrap();
        t.set(&docs[10][col::ID], "rating", Value::Float(4.75)).unwrap();
        t.commit().unwrap();
        docs[10][col::RATING] = Value::Float(4.75);
        check(&db, &docs);

        db.storage().flush().unwrap();
        check(&db, &docs);
        // Stored updates with pending ones over them.
        set_some(&db, &mut docs, 11, 300, 3);
        check(&db, &docs);
    }
    // The last sets come back from the WAL.
    let db = Db::open(&path, opts()).unwrap();
    check(&db, &docs);
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 100_000, ..MergePolicy::default() }).unwrap();
    assert_eq!(db.snapshot().segments().len(), 1);
    assert!(db.snapshot().segments()[0].updates.is_none(), "merged into the documents");
    check(&db, &docs);
    set_some(&db, &mut docs, 5, 200, 4);
    db.storage().flush().unwrap();
    db.storage().vacuum().unwrap();
    check(&db, &docs);
}

#[test]
fn in_place_updates_in_a_single_file() {
    run("u.strata", Layout::SingleFile);
}

#[test]
fn in_place_updates_in_a_directory() {
    run("u.dir", Layout::Directory);
}

/// Sets committed while merges run end up in the merged segments.
#[test]
fn sets_during_merges_are_kept() {
    let mut docs = data::synthetic(3000, 8, 4);
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { segment_max_docs: 250, ..DbOptions::default() };
    let db = Db::create(&dir.path().join("m.strata"), data::beir_schema(8, VectorIndexKind::Flat), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    let before = db.snapshot().segments().len();
    let mut model = docs.clone();
    std::thread::scope(|s| {
        s.spawn(|| {
            db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 100_000, ..MergePolicy::default() }).unwrap();
        });
        for round in 0..40 {
            set_some(&db, &mut model, round * 7, 25, round as i64);
            if round % 10 == 9 {
                db.storage().flush().unwrap();
            }
        }
    });
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 100_000, ..MergePolicy::default() }).unwrap();
    assert!(db.snapshot().segments().len() < before);
    docs = model;
    check(&db, &docs);
}

/// `upsert_if_newer` keeps the document with the highest version, wherever
/// the live one is and whatever wrote it.
#[test]
fn versioned_upserts_keep_the_newest() {
    use strata::schema::{Field, FieldType, Schema};
    let schema = Schema::new(
        "v",
        vec![Field::new("id", FieldType::Keyword), Field::new("ver", FieldType::Int), Field::new("price", FieldType::Float)],
        Some("id"),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("v.strata"), schema, DbOptions::default()).unwrap();
    let doc = |id: &str, ver: i64, price: f64| vec![Value::Str(id.into()), Value::Int(ver), Value::Float(price)];
    let price = |id: &str| {
        let r = db.query(&format!("SELECT id, ver, price FROM v WHERE id = '{id}'"), &Params::new()).unwrap();
        r.rows.first().map(|r| (r[1].as_i64().unwrap(), r[2].as_f64().unwrap()))
    };
    let mut t = db.storage().begin().unwrap();
    assert!(t.upsert_if_newer(doc("a", 5, 1.0), "ver").unwrap());
    assert!(!t.upsert_if_newer(doc("a", 3, 2.0), "ver").unwrap(), "older, same transaction");
    assert!(t.upsert_if_newer(doc("b", 1, 1.0), "ver").unwrap());
    assert!(t.upsert_if_newer(doc("a", 9, 3.0), "ver").unwrap());
    assert!(!t.upsert_if_newer(doc("a", 9, 4.0), "ver").unwrap(), "equal versions keep the first");
    assert!(t.upsert_if_newer(doc("c", 1, 1.0), "price").is_err(), "the version must be an INT");
    t.commit().unwrap();
    assert_eq!(price("a"), Some((9, 3.0)));

    // In the memtable, then on disk.
    for flush in [false, true] {
        if flush {
            db.storage().flush().unwrap();
        }
        let mut t = db.storage().begin().unwrap();
        assert!(!t.upsert_if_newer(doc("a", 8, 5.0), "ver").unwrap());
        assert!(t.upsert_if_newer(doc("b", 2 + flush as i64, 6.0), "ver").unwrap());
        t.commit().unwrap();
    }
    assert_eq!(price("a"), Some((9, 3.0)));
    assert_eq!(price("b"), Some((3, 6.0)));

    // A version set in place counts, on disk and in the same transaction.
    db.storage().flush().unwrap();
    let mut t = db.storage().begin().unwrap();
    t.set(&Value::Str("a".into()), "ver", Value::Int(20)).unwrap();
    t.commit().unwrap();
    let mut t = db.storage().begin().unwrap();
    assert!(!t.upsert_if_newer(doc("a", 15, 7.0), "ver").unwrap());
    t.set(&Value::Str("b".into()), "ver", Value::Int(30)).unwrap();
    assert!(!t.upsert_if_newer(doc("b", 25, 7.0), "ver").unwrap());
    // A deleted document takes any version.
    t.delete(&Value::Str("a".into())).unwrap();
    assert!(t.upsert_if_newer(doc("a", 1, 8.0), "ver").unwrap());
    t.commit().unwrap();
    assert_eq!(price("a"), Some((1, 8.0)));
    assert_eq!(price("b"), Some((30, 6.0)));
}
