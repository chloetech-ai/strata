//! `MAX k PER field`: at most k rows of each value of a field, the first
//! k in the query's order, against the same computed over the query's
//! whole ranked list: by BM25, by a column and by similarity, with a
//! filter, LIMIT and OFFSET, NULL as one value, in a subquery, the plan
//! (`DistinctOn` with `per`, feature `per_cap`) and what it refuses.

use std::collections::HashMap;

use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::query::ir::Plan;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::{Field, FieldType, Schema, VectorIndexKind};

const CLIPS: usize = 800;
const VIDEOS: usize = 30;
const WORDS: [&str; 8] = ["dog", "beach", "goal", "kitchen", "rain", "city", "train", "song"];

fn schema() -> Schema {
    let mut tags = Field::new("tags", FieldType::Keyword);
    tags.multi = true;
    Schema::new(
        "clips",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("video", FieldType::Keyword),
            Field::new("lang", FieldType::Keyword),
            Field::new("views", FieldType::Int),
            Field::new("transcript", FieldType::Text { positions: false }),
            Field::new("emb", FieldType::Vector { dim: 8, index: VectorIndexKind::Flat }),
            tags,
        ],
        Some("id"),
    )
    .unwrap()
}

/// Clips of videos with very different numbers of clips (`v0` has many,
/// the last few one each), a quarter without a language.
fn load(dir: &std::path::Path) -> Db {
    let db = Db::create(&dir.join("c.strata"), schema(), DbOptions { sync: false, segment_max_docs: 150, ..DbOptions::default() }).unwrap();
    let mut g = rand::rngs::StdRng::seed_from_u64(3);
    db.load((0..CLIPS).map(|i| {
        let v = (g.gen::<f64>().powi(3) * VIDEOS as f64) as usize;
        let lang = match g.gen_range(0..4) {
            0 => Value::Null,
            1 => Value::Str("es".into()),
            _ => Value::Str("en".into()),
        };
        let words: Vec<&str> = (0..g.gen_range(3..9)).map(|_| WORDS[g.gen_range(0..WORDS.len())]).collect();
        let emb: Vec<f32> = (0..8).map(|_| g.gen::<f32>() - 0.5).collect();
        vec![
            Value::Str(format!("c{i:03}")),
            Value::Str(format!("v{v}")),
            lang,
            Value::Int(g.gen_range(0..50)),
            Value::Str(words.join(" ")),
            Value::Vector(emb.into()),
            Value::List(vec![Value::Str(WORDS[i % 8].into())]),
        ]
    }))
    .unwrap();
    db
}

/// The rows of `q`, with `:v` a vector.
fn rows(db: &Db, q: &str) -> Vec<Vec<Value>> {
    let params = Params::new().set("v", Value::Vector(vec![0.1, -0.2, 0.3, 0.0, 0.2, -0.1, 0.4, 0.1].into()));
    db.query(q, &params).unwrap_or_else(|e| panic!("{q}: {e}")).rows
}

/// Of `ordered`, at most `k` rows of each value of column 1, from
/// `offset`, at most `limit`.
fn capped(ordered: &[Vec<Value>], k: usize, offset: usize, limit: Option<usize>) -> Vec<Vec<Value>> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    ordered
        .iter()
        .filter(|r| {
            let n = seen.entry(format!("{:?}", r[1])).or_default();
            *n += 1;
            *n <= k
        })
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .cloned()
        .collect()
}

#[test]
fn each_value_keeps_its_first_k_rows_in_the_query_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = load(dir.path());
    let mut g = rand::rngs::StdRng::seed_from_u64(9);
    let mut checked = 0;
    for case in 0..120 {
        let field = ["video", "lang"][case % 2];
        let order = match g.gen_range(0..3) {
            0 => format!("bm25(transcript, '{} {}') DESC", WORDS[g.gen_range(0..8)], WORDS[g.gen_range(0..8)]),
            1 => "views DESC".to_string(),
            _ => "similarity(emb, :v) DESC".to_string(),
        };
        let filter = if g.gen_bool(0.5) { format!(" WHERE views >= {}", g.gen_range(0..30)) } else { String::new() };
        let k = g.gen_range(1..5);
        let (offset, limit) = (if g.gen_bool(0.3) { g.gen_range(0..6) } else { 0 }, if g.gen_bool(0.9) { Some(g.gen_range(1..40)) } else { None });
        let cut = format!("{}{}", limit.map_or(String::new(), |l| format!(" LIMIT {l}")), if offset > 0 { format!(" OFFSET {offset}") } else { String::new() });
        // `views DESC` ties; the key orders them, as the scan does.
        let then = if order == "views DESC" { ", id" } else { "" };
        let all = rows(&db, &format!("SELECT id, {field}, score() FROM clips{filter} ORDER BY {order}{then} LIMIT 100000").replace(", score()", if order == "views DESC" { "" } else { ", score()" }));
        let q = format!("SELECT id, {field}{} FROM clips{filter} ORDER BY {order}{then} MAX {k} PER {field}{cut}", if order == "views DESC" { "" } else { ", score()" });
        let got = rows(&db, &q);
        assert_eq!(got, capped(&all, k, offset, limit), "{q}");
        checked += got.len();
    }
    assert!(checked > 500, "{checked}");
}

#[test]
fn over_derived_and_joined_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = load(dir.path());
    let all = rows(&db, "SELECT id, video FROM clips WHERE views > 10 ORDER BY views DESC, id LIMIT 100000");
    let want = capped(&all, 2, 0, Some(30));
    for q in [
        "WITH c AS (SELECT id, video, views FROM clips WHERE views > 10) SELECT id, video FROM c ORDER BY views DESC, id MAX 2 PER video LIMIT 30",
        "SELECT a.id, a.video FROM clips a JOIN clips b ON a.id = b.id WHERE a.views > 10 ORDER BY a.views DESC, a.id MAX 2 PER a.video LIMIT 30",
    ] {
        assert_eq!(rows(&db, q), want, "{q}");
    }
}

#[test]
fn a_cap_of_one_is_distinct_on() {
    let dir = tempfile::tempdir().unwrap();
    let db = load(dir.path());
    for order in ["views DESC, id", "bm25(transcript, 'dog rain') DESC"] {
        let a = rows(&db, &format!("SELECT id, video FROM clips ORDER BY {order} MAX 1 PER video LIMIT 12"));
        let b = rows(&db, &format!("SELECT DISTINCT ON (video) id, video FROM clips ORDER BY {order} LIMIT 12"));
        assert_eq!(a, b, "{order}");
    }
}

#[test]
fn a_capped_subquery_and_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let db = load(dir.path());
    // Each video's two most viewed clips, then those that mention a dog.
    let inner = rows(&db, "SELECT id, video FROM clips ORDER BY views DESC, id MAX 2 PER video LIMIT 1000");
    let mut want: Vec<String> = inner.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    let dogs: Vec<String> = rows(&db, "SELECT id FROM clips WHERE MATCH(transcript, 'dog') LIMIT 100000").iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    want.retain(|id| dogs.contains(id));
    want.sort();
    let mut got: Vec<String> = rows(&db, "SELECT id FROM clips WHERE MATCH(transcript, 'dog') AND id IN (SELECT id FROM clips ORDER BY views DESC, id MAX 2 PER video LIMIT 1000) LIMIT 1000")
        .iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .collect();
    got.sort();
    assert_eq!(got, want);
    assert!(!want.is_empty());

    let q = "SELECT id FROM clips ORDER BY bm25(transcript, 'dog') DESC MAX 3 PER video LIMIT 10";
    let plan = db.explain(q, &Params::new()).unwrap();
    assert!(plan.features.iter().any(|f| f == "per_cap") && plan.features.iter().any(|f| f == "distinct_on"), "{:?}", plan.features);
    let text = plan.explain(&[db.schema()]).join("\n");
    assert!(text.contains("At most 3 per: video"), "{text}");
    // The plan as JSON runs alike.
    let json = plan.to_json_text().unwrap();
    assert!(json.contains("\"per\":3"), "{json}");
    let again = Plan::from_json(&serde_json::from_str(&json).unwrap()).unwrap();
    match db.run(&again).unwrap() {
        SqlResult::Rows(r) => assert_eq!(r.rows, rows(&db, q)),
        SqlResult::Written(_) => panic!("a query"),
    }
    // A reader without the feature refuses the plan by name.
    let e = Plan::readable_by(1, &["operators", "distinct_on", "per_cap"], &["operators", "distinct_on"]).unwrap_err().to_string();
    assert!(e.contains("per_cap"), "{e}");
    // MAX 1 PER is DISTINCT ON, which a Strata before per_cap reads.
    let one = db.explain("SELECT id FROM clips ORDER BY views DESC MAX 1 PER video LIMIT 5", &Params::new()).unwrap();
    assert!(!one.features.iter().any(|f| f == "per_cap"), "{:?}", one.features);
    assert!(!one.to_json_text().unwrap().contains("\"per\""));
}

#[test]
fn what_a_cap_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let db = load(dir.path());
    for (q, want) in [
        ("SELECT id FROM clips ORDER BY views DESC MAX 2 PER tags LIMIT 5", "tags is multi-valued"),
        ("SELECT id FROM clips MAX 2 PER video LIMIT 5", "needs an ORDER BY"),
        ("SELECT id FROM clips ORDER BY views DESC MAX 0 PER video LIMIT 5", "1 or more"),
        ("SELECT DISTINCT ON (lang) id FROM clips ORDER BY views DESC MAX 2 PER video LIMIT 5", "not with DISTINCT"),
        ("SELECT id FROM clips ORDER BY __max_per(2, video), views DESC LIMIT 5", "ends ORDER BY"),
    ] {
        let e = db.query(q, &Params::new()).map(|r| r.rows.len()).unwrap_err().to_string();
        assert!(e.contains(want), "{q}: {e}");
    }
}
