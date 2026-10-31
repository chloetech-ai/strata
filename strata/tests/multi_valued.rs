//! Multi-valued KEYWORD and INT fields: a predicate matches a document
//! when any of its values matches, facets count every value, and the lists
//! survive the WAL, flushes, merges, reopening and the directory layout.

use std::collections::BTreeMap;

use strata::db::Db;
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::{DbOptions, Layout, MergePolicy};
use strata::value::{Document, Value};

fn schema() -> Schema {
    Schema::new(
        "items",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("sizes", FieldType::Int).multi(),
            Field::new("year", FieldType::Int),
        ],
        Some("id"),
    )
    .unwrap()
}

const TAGS: [&str; 6] = ["red", "blue", "green", "sale", "new", "kids"];

/// Document `i`: up to three tags and up to four sizes (some negative),
/// with repeats, and a few documents with none.
fn doc(i: usize) -> Document {
    let tags: Vec<Value> = (0..i % 4).map(|k| Value::Str(TAGS[(i * 7 + k * 5) % 6].to_string())).collect();
    let sizes: Vec<Value> = (0..(i * 3) % 5).map(|k| Value::Int(((i * 13 + k * 29) % 40) as i64 - 10)).collect();
    vec![Value::Str(format!("i{i:05}")), Value::List(tags), Value::List(sizes), Value::Int(2000 + (i % 25) as i64)]
}

fn has_tag(d: &Document, t: &str) -> bool {
    d[1].items().iter().any(|v| v.as_str() == Some(t))
}

fn sizes(d: &Document) -> Vec<i64> {
    d[2].items().iter().filter_map(|v| v.as_i64()).collect()
}

fn ids(db: &Db, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = db.query(sql, &Params::new()).unwrap().rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    out.sort();
    out
}

fn want(docs: &[Document], f: impl Fn(&Document) -> bool) -> Vec<String> {
    docs.iter().filter(|d| f(d)).map(|d| d[0].as_str().unwrap().to_string()).collect()
}

fn check(db: &Db, docs: &[Document]) {
    let all = "SELECT id FROM items";
    assert_eq!(ids(db, &format!("{all} WHERE tags = 'sale'")), want(docs, |d| has_tag(d, "sale")));
    assert_eq!(ids(db, &format!("{all} WHERE tags IN ('red', 'kids', 'none')")), want(docs, |d| has_tag(d, "red") || has_tag(d, "kids")));
    assert_eq!(ids(db, &format!("{all} WHERE tags != 'red'")), want(docs, |d| !has_tag(d, "red")));
    assert_eq!(ids(db, &format!("{all} WHERE sizes = 4")), want(docs, |d| sizes(d).contains(&4)));
    assert_eq!(ids(db, &format!("{all} WHERE sizes BETWEEN -3 AND 2")), want(docs, |d| sizes(d).iter().any(|&s| (-3..=2).contains(&s))));
    assert_eq!(ids(db, &format!("{all} WHERE sizes > 25")), want(docs, |d| sizes(d).iter().any(|&s| s > 25)));
    assert_eq!(ids(db, &format!("{all} WHERE sizes < -8")), want(docs, |d| sizes(d).iter().any(|&s| s < -8)));
    assert_eq!(ids(db, &format!("{all} WHERE sizes IN (-10, 7)")), want(docs, |d| sizes(d).iter().any(|&s| s == -10 || s == 7)));
    // Probed against a selective conjunct.
    assert_eq!(
        ids(db, &format!("{all} WHERE id = 'i00042' AND tags = '{}'", TAGS[(42 * 7 + 5) % 6])),
        want(docs, |d| d[0].as_str() == Some("i00042") && has_tag(d, TAGS[(42 * 7 + 5) % 6]))
    );
    assert_eq!(
        ids(db, &format!("{all} WHERE year = 2003 AND (tags = 'new' OR sizes >= 20)")),
        want(docs, |d| d[3] == Value::Int(2003) && (has_tag(d, "new") || sizes(d).iter().any(|&s| s >= 20)))
    );

    let count = db.query("SELECT COUNT(*) FROM items WHERE sizes >= 0", &Params::new()).unwrap();
    assert_eq!(count.rows[0][0], Value::Int(want(docs, |d| sizes(d).iter().any(|&s| s >= 0)).len() as i64));

    // Facets count each distinct value of a document once.
    let mut tag_counts: BTreeMap<String, i64> = BTreeMap::new();
    let mut size_counts: BTreeMap<i64, i64> = BTreeMap::new();
    for d in docs {
        let mut t: Vec<&str> = d[1].items().iter().filter_map(|v| v.as_str()).collect();
        t.sort();
        t.dedup();
        t.into_iter().for_each(|t| *tag_counts.entry(t.to_string()).or_default() += 1);
        let mut s = sizes(d);
        s.sort();
        s.dedup();
        s.into_iter().for_each(|s| *size_counts.entry(s).or_default() += 1);
    }
    // A document without values is in the group NULL, last in the order.
    let none = |f: usize| Value::Int(docs.iter().filter(|d| d[f].items().is_empty()).count() as i64);
    let got = db.query("SELECT tags, COUNT(*) FROM items GROUP BY tags ORDER BY tags", &Params::new()).unwrap();
    assert_eq!(got.rows.last().unwrap(), &vec![Value::Null, none(1)]);
    let got: Vec<(String, i64)> = got.rows[..got.rows.len() - 1].iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap())).collect();
    assert_eq!(got, tag_counts.into_iter().collect::<Vec<_>>());
    let got = db.query("SELECT sizes, COUNT(*) FROM items GROUP BY sizes ORDER BY sizes", &Params::new()).unwrap();
    assert_eq!(got.rows.last().unwrap(), &vec![Value::Null, none(2)]);
    let got: Vec<(i64, i64)> = got.rows[..got.rows.len() - 1].iter().map(|r| (r[0].as_i64().unwrap(), r[1].as_i64().unwrap())).collect();
    assert_eq!(got, size_counts.into_iter().collect::<Vec<_>>(), "integer groups in numeric order");

    // Rows come back with their lists.
    let r = db.query("SELECT id, tags, sizes FROM items WHERE id = 'i00007'", &Params::new()).unwrap();
    assert_eq!(r.rows[0][1], docs[7][1]);
    assert_eq!(r.rows[0][2], docs[7][2]);
}

fn run(name: &str, layout: Layout) {
    let docs: Vec<Document> = (0..3000).map(doc).collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    let opts = || DbOptions { segment_max_docs: 700, layout, ..DbOptions::default() };
    {
        let db = Db::create(&path, schema(), opts()).unwrap();
        db.load(docs[..2000].iter().cloned()).unwrap();
        // The rest through the WAL, partly left in the memtable.
        let mut t = db.storage().begin().unwrap();
        for d in &docs[2000..2600] {
            t.insert(d.clone()).unwrap();
        }
        t.commit().unwrap();
        db.storage().flush().unwrap();
        let mut t = db.storage().begin().unwrap();
        for d in &docs[2600..] {
            t.insert(d.clone()).unwrap();
        }
        t.commit().unwrap();
        check(&db, &docs);
        assert!(db.query("SELECT id FROM items ORDER BY sizes DESC LIMIT 3", &Params::new()).is_err());
    }
    // Reopen: the memtable's lists come back from the WAL.
    let db = Db::open(&path, opts()).unwrap();
    check(&db, &docs);
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
    assert!(db.snapshot().segments().len() < 5);
    check(&db, &docs);
}

#[test]
fn multi_valued_fields_in_a_single_file() {
    run("m.strata", Layout::SingleFile);
}

#[test]
fn multi_valued_fields_in_a_directory() {
    run("m.dir", Layout::Directory);
}

#[test]
fn schema_rejects_bad_multi_fields() {
    let bad = |f: Field, pk| Schema::new("t", vec![Field::new("id", FieldType::Keyword), f], pk).is_err();
    assert!(bad(Field::new("x", FieldType::Float).multi(), None));
    assert!(bad(Field::new("x", FieldType::Text { positions: false }).multi(), None));
    assert!(Schema::new("t", vec![Field::new("id", FieldType::Keyword).multi()], Some("id")).is_err());
    // A list of the wrong kind of value is refused when written.
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("b.strata"), schema(), DbOptions::default()).unwrap();
    let mut d = doc(1);
    d[2] = Value::List(vec![Value::Str("big".into())]);
    let mut t = db.storage().begin().unwrap();
    assert!(t.insert(d).is_err());
}
