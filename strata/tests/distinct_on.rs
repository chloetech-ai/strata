//! `SELECT DISTINCT ON`: the first row of each group in the query's order,
//! against the same computed over the rows: groups of one and two keys, of
//! an expression and with NULLs, ordered by a column, by similarity and by
//! BM25, with a filter, OFFSET and no LIMIT, over groups, the scan read in
//! growing slices, and the plan (node `DistinctOn`, feature `distinct_on`)
//! run as JSON.

use std::collections::HashSet;

use rand::{Rng, SeedableRng};
use strata::db::{Db, SqlResult};
use strata::query::ir::Plan;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::{Field, FieldType, Schema, VectorIndexKind};

const CLIPS: usize = 600;
const VIDEOS: usize = 40;
const WORDS: [&str; 8] = ["dog", "beach", "goal", "kitchen", "rain", "city", "train", "song"];

fn schema() -> Schema {
    Schema::new(
        "clips",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("video", FieldType::Keyword),
            Field::new("lang", FieldType::Keyword),
            Field::new("start_ms", FieldType::Int),
            Field::new("views", FieldType::Int),
            Field::new("transcript", FieldType::Text { positions: false }),
            Field::new("emb", FieldType::Vector { dim: 8, index: VectorIndexKind::Flat }),
        ],
        Some("id"),
    )
    .unwrap()
}

#[derive(Clone)]
struct Clip {
    id: String,
    video: String,
    lang: Option<String>,
    start_ms: i64,
    views: i64,
    emb: Vec<f32>,
}

fn unit(v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.into_iter().map(|x| x / n).collect()
}

/// Clips of videos with very different numbers of clips (video `v0` has
/// many, the last few one each), some without a language.
fn clips() -> Vec<Clip> {
    let mut g = rand::rngs::StdRng::seed_from_u64(11);
    (0..CLIPS)
        .map(|i| {
            let v = ((g.gen::<f64>().powi(2)) * VIDEOS as f64) as usize;
            let lang = match g.gen_range(0..4) {
                0 => None,
                1 => Some("es".to_string()),
                _ => Some("en".to_string()),
            };
            Clip { id: format!("c{i:03}"), video: format!("v{v}"), lang, start_ms: g.gen_range(0..2_000) * 100, views: g.gen_range(0..50), emb: unit((0..8).map(|_| g.gen::<f32>() - 0.5).collect()) }
        })
        .collect()
}

fn load(dir: &std::path::Path, clips: &[Clip]) -> Db {
    let db = Db::create(&dir.join("c.strata"), schema(), DbOptions { sync: false, segment_max_docs: 150, ..DbOptions::default() }).unwrap();
    let mut g = rand::rngs::StdRng::seed_from_u64(5);
    db.load(clips.iter().map(|c| {
        let words: Vec<&str> = (0..g.gen_range(3..9)).map(|_| WORDS[g.gen_range(0..WORDS.len())]).collect();
        vec![
            Value::Str(c.id.clone()),
            Value::Str(c.video.clone()),
            c.lang.clone().map_or(Value::Null, Value::Str),
            Value::Int(c.start_ms),
            Value::Int(c.views),
            Value::Str(words.join(" ")),
            Value::Vector(c.emb.clone().into()),
        ]
    }))
    .unwrap();
    db
}

fn rows(db: &Db, q: &str, params: &Params) -> Vec<Vec<Value>> {
    db.query(q, params).unwrap_or_else(|e| panic!("{q}: {e}")).rows
}

/// The first row of each group of `ordered` by `key`, from `offset`, at
/// most `limit`.
fn firsts<T: Clone>(ordered: &[T], key: impl Fn(&T) -> String, offset: usize, limit: Option<usize>) -> Vec<T> {
    let mut seen = HashSet::new();
    ordered.iter().filter(|r| seen.insert(key(r))).skip(offset).take(limit.unwrap_or(usize::MAX)).cloned().collect()
}

fn ids(rows: &[Vec<Value>]) -> Vec<String> {
    rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect()
}

fn lang(c: &Clip) -> String {
    format!("{:?}", c.lang)
}

#[test]
fn the_first_row_of_each_group_in_the_query_order() {
    let dir = tempfile::tempdir().unwrap();
    let data = clips();
    let db = load(dir.path(), &data);
    let none = Params::new();

    // By a column: each video's latest clip, the videos by it.
    let mut by_start = data.clone();
    by_start.sort_by(|a, b| b.start_ms.cmp(&a.start_ms).then(a.id.cmp(&b.id)));
    for (offset, limit) in [(0, Some(5)), (0, Some(1)), (3, Some(10)), (0, Some(1000)), (0, None)] {
        let cut = match limit {
            Some(l) => format!(" LIMIT {l} OFFSET {offset}"),
            None => String::new(),
        };
        let q = format!("SELECT DISTINCT ON (video) id, video, start_ms FROM clips ORDER BY start_ms DESC, id{cut}");
        let want: Vec<String> = firsts(&by_start, |c| c.video.clone(), offset, limit).into_iter().map(|c| c.id).collect();
        assert_eq!(ids(&rows(&db, &q, &none)), want, "{q}");
    }
    let q = "SELECT DISTINCT ON (video) id, video FROM clips ORDER BY start_ms DESC, id";
    assert_eq!(rows(&db, q, &none).len(), data.iter().map(|c| &c.video).collect::<HashSet<_>>().len(), "{q}: a row per video");

    // Two keys, one with NULLs (one group of them), and a key that is not
    // a column of the answer.
    let q = "SELECT DISTINCT ON (video, lang) id FROM clips WHERE views >= 10 ORDER BY views DESC, id LIMIT 30";
    let mut by_views: Vec<Clip> = data.iter().filter(|c| c.views >= 10).cloned().collect();
    by_views.sort_by(|a, b| b.views.cmp(&a.views).then(a.id.cmp(&b.id)));
    let want: Vec<String> = firsts(&by_views, |c| format!("{} {}", c.video, lang(c)), 0, Some(30)).into_iter().map(|c| c.id).collect();
    assert_eq!(ids(&rows(&db, q, &none)), want, "{q}");
    let q = "SELECT DISTINCT ON (lang) id, lang FROM clips ORDER BY views DESC, id";
    let got = rows(&db, q, &none);
    assert_eq!(ids(&got), firsts(&by_start_views(&data), lang, 0, None).into_iter().map(|c| c.id).collect::<Vec<_>>(), "{q}");
    assert!(got.iter().any(|r| r[1] == Value::Null), "{q}: NULL is a group");

    // An expression, and a column of the answer by name and by number.
    let q = "SELECT DISTINCT ON (views % 5) id, views % 5 AS part FROM clips ORDER BY views % 5, start_ms DESC, id";
    let mut by_part = data.clone();
    by_part.sort_by(|a, b| (a.views % 5).cmp(&(b.views % 5)).then(b.start_ms.cmp(&a.start_ms)).then(a.id.cmp(&b.id)));
    let want: Vec<String> = firsts(&by_part, |c| (c.views % 5).to_string(), 0, None).into_iter().map(|c| c.id).collect();
    assert_eq!(want.len(), 5);
    for q in [q.to_string(), q.replacen("DISTINCT ON (views % 5)", "DISTINCT ON (part)", 1), q.replacen("DISTINCT ON (views % 5)", "DISTINCT ON (2)", 1)] {
        assert_eq!(ids(&rows(&db, &q, &none)), want, "{q}");
    }

    // Over the rows of a WITH result.
    let q = "WITH busy AS (SELECT id FROM clips WHERE views > 20) SELECT DISTINCT ON (video) id FROM busy ORDER BY start_ms DESC, id LIMIT 8";
    let busy: Vec<Clip> = by_start.iter().filter(|c| c.views > 20).cloned().collect();
    assert_eq!(ids(&rows(&db, q, &none)), firsts(&busy, |c| c.video.clone(), 0, Some(8)).into_iter().map(|c| c.id).collect::<Vec<_>>(), "{q}");

    // By similarity: each video's nearest clip, the nearest videos first.
    let query = unit(vec![0.3, -0.2, 0.5, 0.1, -0.4, 0.2, 0.0, 0.1]);
    let p = Params::new().set("q", Value::Vector(query.clone().into()));
    let mut near = data.clone();
    let sim = |c: &Clip| c.emb.iter().zip(&query).map(|(x, y)| x * y).sum::<f32>();
    near.sort_by(|a, b| sim(b).partial_cmp(&sim(a)).unwrap());
    for limit in [1, 4, 25] {
        let q = format!("SELECT DISTINCT ON (video) id, video, score() FROM clips ORDER BY similarity(emb, :q) DESC LIMIT {limit}");
        let got = db.query(&q, &p).unwrap();
        let want: Vec<String> = firsts(&near, |c| c.video.clone(), 0, Some(limit)).into_iter().map(|c| c.id).collect();
        assert_eq!(ids(&got.rows), want, "{q}");
        for (r, c) in got.rows.iter().zip(&want) {
            let c = data.iter().find(|x| x.id == *c).unwrap();
            assert!((r[2].as_f64().unwrap() - sim(c) as f64).abs() < 1e-4, "{q}: score() is the row's");
        }
    }

    // By BM25: the first rows of groups of the ranked rows, whatever their
    // number.
    let ranked = rows(&db, "SELECT id, video FROM clips WHERE MATCH(transcript, 'goal rain') ORDER BY bm25(transcript, 'goal rain') DESC LIMIT 100000", &none);
    for limit in [2, 10, 100] {
        let q = format!("SELECT DISTINCT ON (video) id, video FROM clips WHERE MATCH(transcript, 'goal rain') ORDER BY bm25(transcript, 'goal rain') DESC LIMIT {limit}");
        let want: Vec<Vec<Value>> = firsts(&ranked, |r| format!("{:?}", r[1]), 0, Some(limit));
        assert_eq!(rows(&db, &q, &none), want, "{q}");
    }
}

/// The clips by views, most first, then by id.
fn by_start_views(data: &[Clip]) -> Vec<Clip> {
    let mut v = data.to_vec();
    v.sort_by(|a, b| b.views.cmp(&a.views).then(a.id.cmp(&b.id)));
    v
}

#[test]
fn the_scan_keeps_each_groups_first_rows() {
    let dir = tempfile::tempdir().unwrap();
    let data = clips();
    let db = load(dir.path(), &data);
    let read = |q: &str| -> Vec<usize> {
        let rs = db.query(q, &Params::new()).unwrap();
        // A line per run of the scan: "query: <rows> rows: <its plan>".
        rs.stats.plan.iter().filter(|l| l.starts_with("query: ") && l.contains("Search clips")).map(|l| l.split(' ').nth(1).unwrap().parse().unwrap()).collect()
    };
    let plan = |q: &str| db.query(q, &Params::new()).unwrap().stats.plan.join("\n");
    // Ranked by BM25, the scan keeps each video's first row as it scores:
    // it returns the rows asked for, once.
    let q = "SELECT DISTINCT ON (video) id FROM clips ORDER BY bm25(transcript, 'dog') DESC LIMIT 5";
    assert_eq!(read(q), [5]);
    assert!(plan(q).contains("max 1 per video: kept as bm25 scores each segment"), "{}", plan(q));
    // By a column, the executor reads growing slices of the ranking (four
    // times the rows asked for, then four times as many) until enough
    // videos have a row, and the scan returns those.
    let q = "SELECT DISTINCT ON (video) id FROM clips ORDER BY views DESC LIMIT 38";
    assert_eq!(read(q), [38]);
    assert!(plan(q).contains(&format!("max 1 per video: read until enough rows are left ({CLIPS} read)")), "{}", plan(q));
    // More groups than there are: every row is read, and each video's first
    // returned.
    let q = "SELECT DISTINCT ON (video) id FROM clips ORDER BY views DESC LIMIT 1000";
    assert_eq!(read(q), [VIDEOS]);
    // Ordered in memory, the scan reads every row once.
    let q = "SELECT DISTINCT ON (video) id FROM clips ORDER BY views DESC, id LIMIT 5";
    assert_eq!(read(q), [CLIPS]);
}

#[test]
fn distinct_on_over_groups_and_as_a_plan() {
    let dir = tempfile::tempdir().unwrap();
    let data = clips();
    let db = load(dir.path(), &data);
    let none = Params::new();

    // Each language's video with the most clips (ties by video).
    let q = "SELECT DISTINCT ON (lang) lang, video, COUNT(*) AS n FROM clips GROUP BY lang, video ORDER BY lang, n DESC, video";
    let mut counts: std::collections::BTreeMap<(String, String), i64> = Default::default();
    for c in &data {
        *counts.entry((lang(c), c.video.clone())).or_default() += 1;
    }
    let mut groups: Vec<((String, String), i64)> = counts.into_iter().collect();
    groups.sort_by(|a, b| a.0 .0.cmp(&b.0 .0).then(b.1.cmp(&a.1)).then(a.0 .1.cmp(&b.0 .1)));
    let want: Vec<(String, i64)> = firsts(&groups, |g| g.0 .0.clone(), 0, None).into_iter().map(|g| (g.0 .1, g.1)).collect();
    let got: Vec<(String, i64)> = rows(&db, q, &none).into_iter().map(|r| (r[1].as_str().unwrap().to_string(), r[2].as_i64().unwrap())).collect();
    // NULL sorts last in SQL, first as "None" here.
    let mut got_sorted = got.clone();
    got_sorted.sort();
    let mut want_sorted = want.clone();
    want_sorted.sort();
    assert_eq!(got_sorted, want_sorted, "{q}");
    assert_eq!(got.len(), 3, "{q}");

    // The plan: a DistinctOn node, feature distinct_on, run from its JSON.
    let q = "SELECT DISTINCT ON (video) id, video FROM clips WHERE lang = 'en' ORDER BY start_ms DESC, id LIMIT 7";
    let plan = db.explain(q, &none).unwrap();
    assert!(plan.features.iter().any(|f| f == "distinct_on") && plan.features.iter().any(|f| f == "operators"), "{:?}", plan.features);
    let text = plan.explain(&[db.schema()]).join("\n");
    assert!(text.contains("Distinct on: video"), "{text}");
    let again = Plan::from_json(&serde_json::from_str(&plan.to_json_text().unwrap()).unwrap()).unwrap();
    match db.run(&again).unwrap() {
        SqlResult::Rows(r) => assert_eq!(r.rows, rows(&db, q, &none)),
        SqlResult::Written(_) => panic!("a query"),
    }
    let json = plan.to_json_text().unwrap();
    assert!(json.contains("\"node\":\"distinct_on\""), "{json}");
    // A reader without the feature refuses the plan by name.
    let e = Plan::readable_by(1, &["operators", "distinct_on"], &["operators"]).unwrap_err().to_string();
    assert!(e.contains("distinct_on"), "{e}");
    // A subquery's DISTINCT ON is the plan's too.
    let q = "SELECT id FROM clips WHERE id IN (SELECT DISTINCT ON (video) id FROM clips ORDER BY start_ms DESC, id) ORDER BY views DESC LIMIT 3";
    assert!(db.explain(q, &none).unwrap().features.iter().any(|f| f == "distinct_on"));
    assert_eq!(rows(&db, q, &none).len(), 3);
}
