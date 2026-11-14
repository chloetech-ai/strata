//! Set operators on multi-valued KEYWORD and INT fields: `@>` (contains
//! all), `&&` (overlaps) and `<@` (contained by, on INT[]), answered by the
//! fields' value lists, against the same conditions computed row by row,
//! in segments, the memtable and after a merge; with `NOT`, parameters,
//! a vector search's filter, joins and rows in memory.

use std::sync::Arc;

use chloedb::db::Db;
use chloedb::query::Params;
use chloedb::schema::{Field, FieldType, Schema, VectorIndexKind};
use chloedb::storage::{DbOptions, MergePolicy};
use chloedb::value::{Document, Value};

const TAGS: [&str; 6] = ["red", "blue", "green", "sale", "new", "kids"];

fn schema() -> Schema {
    Schema::new(
        "items",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("groups", FieldType::Int).multi(),
            Field::new("emb", FieldType::Vector { dim: 4, index: VectorIndexKind::Flat }),
        ],
        Some("id"),
    )
    .unwrap()
}

/// Row `i`: up to three tags and up to four groups from -2 to 9, with
/// repeats, some rows with none.
fn doc(i: usize) -> Document {
    let tags: Vec<Value> = (0..i % 4).map(|k| Value::Str(TAGS[(i * 7 + k * 5) % 6].to_string())).collect();
    let groups: Vec<Value> = (0..(i * 3) % 5).map(|k| Value::Int(((i * 13 + k * 29) % 12) as i64 - 2)).collect();
    let emb: Vec<f32> = (0..4).map(|k| (((i * 31 + k * 17) % 23) as f32 - 11.0) / 11.0).collect();
    vec![Value::Str(format!("i{i:05}")), Value::List(tags), Value::List(groups), Value::Vector(Arc::from(emb.as_slice()))]
}

fn tags(d: &Document) -> Vec<&str> {
    d[1].items().iter().filter_map(|v| v.as_str()).collect()
}

fn groups(d: &Document) -> Vec<i64> {
    d[2].items().iter().filter_map(|v| v.as_i64()).collect()
}

fn ids_with(db: &Db, sql: &str, p: &Params) -> Vec<String> {
    let r = db.query(sql, p).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut out: Vec<String> = r.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    out.sort();
    out
}

fn ids(db: &Db, sql: &str) -> Vec<String> {
    ids_with(db, sql, &Params::new())
}

fn want(docs: &[Document], f: impl Fn(&Document) -> bool) -> Vec<String> {
    docs.iter().filter(|d| f(d)).map(|d| d[0].as_str().unwrap().to_string()).collect()
}

fn check(db: &Db, docs: &[Document]) {
    let all = "SELECT id FROM items WHERE";
    let has = |d: &Document, t: &str| tags(d).contains(&t);
    let within = |d: &Document, set: &[i64]| groups(d).iter().all(|g| set.contains(g));
    let cases: Vec<(String, Vec<String>)> = vec![
        // Contains all.
        (format!("{all} tags @> ARRAY['red', 'kids']"), want(docs, |d| has(d, "red") && has(d, "kids"))),
        (format!("{all} tags @> ARRAY['red', 'red']"), want(docs, |d| has(d, "red"))),
        (format!("{all} ARRAY['blue'] <@ tags"), want(docs, |d| has(d, "blue"))),
        (format!("{all} groups @> ARRAY[0, 3]"), want(docs, |d| groups(d).contains(&0) && groups(d).contains(&3))),
        (format!("{all} NOT (tags @> ARRAY['red', 'kids'])"), want(docs, |d| !(has(d, "red") && has(d, "kids")))),
        // Every row contains the empty set.
        (format!("{all} tags @> ARRAY[]"), want(docs, |_| true)),
        // Overlaps.
        (format!("{all} tags && ARRAY['kids', 'new', 'none']"), want(docs, |d| has(d, "kids") || has(d, "new"))),
        (format!("{all} groups && ARRAY[9, -2]"), want(docs, |d| groups(d).iter().any(|g| *g == 9 || *g == -2))),
        (format!("{all} NOT tags && ARRAY['kids']"), want(docs, |d| !has(d, "kids"))),
        (format!("{all} tags && ARRAY[]"), Vec::new()),
        // Contained by: no value outside the list; a row without values
        // holds none outside it.
        (format!("{all} groups <@ ARRAY[0, 1, 2, 3, 4]"), want(docs, |d| within(d, &[0, 1, 2, 3, 4]))),
        (format!("{all} groups <@ ARRAY[-2, 9, 5]"), want(docs, |d| within(d, &[-2, 9, 5]))),
        (format!("{all} ARRAY[4, 0, 7] @> groups"), want(docs, |d| within(d, &[0, 4, 7]))),
        (format!("{all} groups <@ ARRAY[]"), want(docs, |d| groups(d).is_empty())),
        (format!("{all} NOT groups <@ ARRAY[0, 1, 2]"), want(docs, |d| !within(d, &[0, 1, 2]))),
        (
            format!("{all} groups <@ ARRAY[{}, {}, 3]", i64::MIN, i64::MAX),
            want(docs, |d| within(d, &[i64::MIN, i64::MAX, 3])),
        ),
        // With other conditions.
        (
            format!("{all} groups <@ ARRAY[1, 2, 3] AND groups IS NOT NULL AND (tags @> ARRAY['sale'] OR tags && ARRAY['red'])"),
            want(docs, |d| !groups(d).is_empty() && within(d, &[1, 2, 3]) && (has(d, "sale") || has(d, "red"))),
        ),
    ];
    for (sql, want) in &cases {
        // Each case picks out some rows, but for the empty overlap.
        assert!(!want.is_empty() || sql.ends_with("&& ARRAY[]"), "{sql} selects no row of the sample");
        assert_eq!(&ids(db, sql), want, "{sql}");
    }

    // A list parameter.
    let p = Params::new().set("need", Value::List(vec![Value::Str("green".into()), Value::Str("blue".into())])).set("allowed", Value::List((0..6).map(Value::Int).collect()));
    assert_eq!(ids_with(db, &format!("{all} tags @> :need"), &p), want(docs, |d| has(d, "green") && has(d, "blue")));
    assert_eq!(ids_with(db, &format!("{all} tags && :need"), &p), want(docs, |d| has(d, "green") || has(d, "blue")));
    assert_eq!(ids_with(db, &format!("{all} groups <@ :allowed"), &p), want(docs, |d| within(d, &[0, 1, 2, 3, 4, 5])));
    let empty = Params::new().set("need", Value::List(Vec::new()));
    assert_eq!(ids_with(db, &format!("{all} tags && :need"), &empty), Vec::<String>::new());

    // A vector search filtered by a set: the best rows among those allowed.
    let q = Params::new().set("q", Value::Vector(Arc::from([0.5f32, -0.5, 0.25, 1.0].as_slice())));
    let got = db.query("SELECT id FROM items WHERE groups <@ ARRAY[0, 1, 2, 3] AND tags && ARRAY['red', 'blue'] ORDER BY similarity(emb, :q) DESC LIMIT 5", &q).unwrap();
    assert_eq!(got.rows.len(), 5);
    let mut best: Vec<(f32, String)> = docs
        .iter()
        .filter(|d| within(d, &[0, 1, 2, 3]) && (has(d, "red") || has(d, "blue")))
        .map(|d| {
            let Value::Vector(v) = &d[3] else { unreachable!() };
            (v.iter().zip([0.5f32, -0.5, 0.25, 1.0]).map(|(a, b)| a * b).sum::<f32>(), d[0].as_str().unwrap().to_string())
        })
        .collect();
    best.sort_by(|a, b| b.0.total_cmp(&a.0));
    // Rows share vectors, so ties are compared by their scores.
    let score = |id: &str| best.iter().find(|(_, x)| x == id).unwrap_or_else(|| panic!("{id} is not allowed")).0;
    let got: Vec<f32> = got.rows.iter().map(|r| score(r[0].as_str().unwrap())).collect();
    let want_top: Vec<f32> = best.iter().take(5).map(|(s, _)| *s).collect();
    assert_eq!(got, want_top);

    // Joined: the condition goes to its table's scan.
    let joined = ids(db, "SELECT a.id FROM items a JOIN items b ON a.id = b.id WHERE a.tags @> ARRAY['red', 'kids'] AND b.groups <@ ARRAY[0, 1, 2, 3, 4]");
    assert!(!joined.is_empty());
    assert_eq!(joined, want(docs, |d| has(d, "red") && has(d, "kids") && within(d, &[0, 1, 2, 3, 4])));
    // Rows in memory: && and @> of a list written out.
    let cte = "WITH r AS (SELECT id, tags FROM items WHERE tags IS NOT NULL) SELECT id FROM r WHERE";
    assert_eq!(ids(db, &format!("{cte} tags @> ARRAY['red', 'kids']")), want(docs, |d| has(d, "red") && has(d, "kids")));
    assert_eq!(ids(db, &format!("{cte} tags && ARRAY['kids', 'new']")), want(docs, |d| has(d, "kids") || has(d, "new")));
    assert_eq!(ids(db, &format!("{cte} ARRAY['blue'] <@ tags")), want(docs, |d| has(d, "blue")));
    assert_eq!(ids(db, &format!("{cte} NOT tags && ARRAY['kids']")), want(docs, |d| !tags(d).is_empty() && !has(d, "kids")));

    // Counting.
    let n = db.query("SELECT COUNT(*) FROM items WHERE groups <@ ARRAY[0, 1, 2, 3, 4]", &Params::new()).unwrap();
    assert_eq!(n.rows[0][0], Value::Int(want(docs, |d| within(d, &[0, 1, 2, 3, 4])).len() as i64));
}

#[test]
fn set_operators_match_the_rows() {
    let docs: Vec<Document> = (0..2500).map(doc).collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.strata");
    let opts = || DbOptions { segment_max_docs: 600, sync: false, ..DbOptions::default() };
    let db = Db::create(&path, schema(), opts()).unwrap();
    db.load(docs[..1800].iter().cloned()).unwrap();
    // The rest in the memtable.
    let mut t = db.storage().begin().unwrap();
    for d in &docs[1800..] {
        t.insert(d.clone()).unwrap();
    }
    t.commit().unwrap();
    check(&db, &docs);
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 10_000, ..MergePolicy::default() }).unwrap();
    check(&db, &docs);
}

#[test]
fn set_operators_say_what_is_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("e.strata"), schema(), DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load((0..10).map(doc)).unwrap();
    let error = |sql: &str| db.query(sql, &Params::new()).err().unwrap_or_else(|| panic!("{sql} ran")).to_string();
    let e = error("SELECT id FROM items WHERE id @> ARRAY['a']");
    assert!(e.contains("KEYWORD[] or INT[]") && e.contains("= and IN"), "{e}");
    let e = error("SELECT id FROM items WHERE tags <@ ARRAY['red']");
    assert!(e.contains("KEYWORD[]") && e.contains("INT[]"), "{e}");
    let e = error("SELECT id FROM items WHERE tags @> 'red'");
    assert!(e.contains("list"), "{e}");
    let e = error("SELECT id FROM items WHERE tags && ARRAY['red', NULL]");
    assert!(e.contains("NULL"), "{e}");
    let e = error("SELECT id FROM items WHERE groups @> ARRAY['x']");
    assert!(e.contains("groups"), "{e}");
    let e = error("SELECT id FROM items WHERE tags @> groups");
    assert!(e.contains("another field"), "{e}");
}


/// Lists of a hundred thousand values (finding N-B4): binding `@>` and `<@`
/// compared each value with every one before it, and each `AND` child with
/// every earlier child, and `<@` complemented and intersected a set per
/// value. They answer the rows, in seconds rather than many minutes.
#[test]
fn long_lists_bind_and_run_in_linear_time() {
    let docs: Vec<Document> = (0..3000).map(doc).collect();
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("l.strata"), schema(), DbOptions { segment_max_docs: 1000, sync: false, ..DbOptions::default() }).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    const N: i64 = 100_000;
    // Groups 2, 4, 8 and 10, many times each, and odd ones above 9 that no
    // row has.
    let allowed: Vec<Value> = (0..N).map(|i| Value::Int(if i % 3 == 0 { 11 + 2 * i } else { 2 * (i % 6) })).collect();
    let even: Vec<i64> = vec![2, 4, 8, 10];
    let t0 = std::time::Instant::now();
    let p = Params::new().set("allowed", Value::List(allowed));
    assert_eq!(ids_with(&db, "SELECT id FROM items WHERE groups <@ :allowed", &p), want(&docs, |d| groups(d).iter().all(|g| even.contains(g))));
    assert_eq!(ids_with(&db, "SELECT id FROM items WHERE NOT (groups <@ :allowed)", &p), want(&docs, |d| !groups(d).iter().all(|g| even.contains(g))));
    // Many tags, all of which a row must have: none has them all.
    let many: Vec<Value> = (0..N).map(|i| Value::Str(if i % 2 == 0 { TAGS[(i % 6) as usize].to_string() } else { format!("t{i}") })).collect();
    let p = Params::new().set("many", Value::List(many));
    assert_eq!(ids_with(&db, "SELECT id FROM items WHERE tags @> :many", &p), Vec::<String>::new());
    let few = Params::new().set("few", Value::List((0..N).map(|i| Value::Str(["red", "sale"][(i % 2) as usize].into())).collect()));
    assert_eq!(ids_with(&db, "SELECT id FROM items WHERE tags @> :few", &few), want(&docs, |d| tags(d).contains(&"red") && tags(d).contains(&"sale")));
    let took = t0.elapsed();
    assert!(took < std::time::Duration::from_secs(if cfg!(debug_assertions) { 120 } else { 15 }), "{took:?}");
}
