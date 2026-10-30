//! Rankings the engineering review found wrong: fused rankings cut at a
//! fixed depth whatever page was asked for (bug 9, IR-7), unchecked BM25
//! parameters (IR-18), query letters dropped and repeated terms counted
//! once (IR-4), rows without a value counted in N and avgdl (IR-5),
//! typos and synonyms scored with their own IDF (IR-1), and the ties of a
//! fused score under a descending key.

use std::sync::Arc;

use strata::db::Db;
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::text::Synonyms;
use strata::value::Value;
use strata::VectorIndexKind;

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Int),
            Field::new("body", FieldType::Text { positions: false }),
            Field::new("emb", FieldType::Vector { dim: 2, index: VectorIndexKind::Flat }),
        ],
        Some("id"),
    )
    .unwrap()
}

fn row(id: i64, body: Option<&str>) -> Vec<Value> {
    let a = id as f32 * 0.01;
    vec![Value::Int(id), body.map_or(Value::Null, |b| b.into()), Value::Vector(Arc::from([a.cos(), a.sin()]))]
}

fn db_with(rows: Vec<Vec<Value>>) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("t.strata"), schema(), DbOptions::default()).unwrap();
    db.load(rows).unwrap();
    (dir, db)
}

fn query(db: &Db, sql: &str) -> ResultSet {
    let p = Params::new().set("q", Value::Vector(Arc::from([1.0f32, 0.0])));
    db.query(sql, &p).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn ids(r: &ResultSet) -> Vec<i64> {
    r.keys.iter().map(|k| k.as_i64().unwrap()).collect()
}

/// The score of row `id` in `r`.
fn score_of(r: &ResultSet, id: i64) -> f64 {
    r.scores[ids(r).iter().position(|&x| x == id).unwrap_or_else(|| panic!("row {id} not in {:?}", ids(r)))]
}

// Bug 9: with the default depth of 100 per input, RRF's candidates were
// at most 200 rows, so OFFSET 200 answered nothing; a weighted sum asked
// for 250 rows answered 200.
#[test]
fn fused_pages_reach_past_the_depth() {
    // 300 rows match, each body a different length.
    let (_d, db) = db_with((0..300).map(|i| row(i, Some(&format!("apple {}", "pear ".repeat(i as usize % 17))))).collect());
    let rrf = "SELECT id FROM t ORDER BY rrf(bm25(body, 'apple'), similarity(emb, :q)) DESC";
    assert_eq!(query(&db, &format!("{rrf} LIMIT 10 OFFSET 200")).rows.len(), 10);
    assert_eq!(query(&db, &format!("{rrf} LIMIT 10 OFFSET 295")).rows.len(), 5);
    let weighted = "SELECT id FROM t ORDER BY 0.5 * minmax(bm25(body, 'apple')) + 0.5 * minmax(similarity(emb, :q)) DESC";
    assert_eq!(query(&db, &format!("{weighted} LIMIT 250")).rows.len(), 250);
    let product = "SELECT id FROM t ORDER BY bm25(body, 'apple') * similarity(emb, :q) DESC";
    assert_eq!(query(&db, &format!("{product} LIMIT 20 OFFSET 250")).rows.len(), 20);
    // A depth asked for still bounds the candidates when the page fits.
    let r = query(&db, "SELECT id FROM t ORDER BY rrf(bm25(body, 'apple'), similarity(emb, :q), depth => 5) DESC LIMIT 3");
    assert_eq!(r.rows.len(), 3);
}

// SCALE-14: without a LIMIT a ranked query answers 100 rows; an OFFSET
// skipped into them, so one database answered 100 − OFFSET rows (none for
// OFFSET 100) while a coordinator answered 100.
#[test]
fn an_offset_without_limit_keeps_the_default_page() {
    let (_d, db) = db_with((0..300).map(|i| row(i, Some(&format!("apple {}", "pear ".repeat(i as usize % 17))))).collect());
    let all = query(&db, "SELECT id FROM t ORDER BY bm25(body, 'apple') DESC LIMIT 300");
    for offset in [0, 30, 100, 250] {
        let r = query(&db, &format!("SELECT id FROM t ORDER BY bm25(body, 'apple') DESC OFFSET {offset}"));
        let want = &ids(&all)[offset..(offset + 100).min(300)];
        assert_eq!(ids(&r), want, "OFFSET {offset}");
    }
    let r = query(&db, "SELECT id FROM t ORDER BY similarity(emb, :q) DESC OFFSET 100");
    assert_eq!(r.rows.len(), 100);
}

// IR-18: k1 < 0 or b outside 0..=1 makes a score fall as a term repeats,
// which the bounds MaxScore and Block-Max WAND prune with assume it never
// does.
#[test]
fn bm25_parameters_are_checked() {
    let (_d, db) = db_with((0..20).map(|i| row(i, Some("apple pear"))).collect());
    let p = Params::new();
    for (opts, what) in [("k1 => -1", "k1"), ("b => 1.5", "b"), ("b => -0.1", "b"), ("k1 => 1e400", "k1")] {
        let sql = format!("SELECT id FROM t ORDER BY bm25(body, 'apple', {opts}) DESC LIMIT 5");
        let e = db.query(&sql, &p).err().unwrap_or_else(|| panic!("{sql} was accepted")).to_string();
        assert!(e.contains(&format!("bm25's {what}")), "{sql}: {e}");
    }
    for opts in ["k1 => 0", "k1 => 3", "b => 0", "b => 1", "k1 => 0.9, b => 0.4"] {
        let sql = format!("SELECT id FROM t ORDER BY bm25(body, 'apple', {opts}) DESC LIMIT 5");
        assert_eq!(query(&db, &sql).rows.len(), 5, "{sql}");
    }
}

// IR-4: "vitamin d deficiency" ranked on [vitamin, defici], so the rows of
// vitamin C and vitamin D tied; "dog cat cat" weighed cat once.
#[test]
fn query_letters_and_repeats_count() {
    let mut rows = vec![
        row(1, Some("vitamin c deficiency")),
        row(2, Some("vitamin d deficiency")),
        row(3, Some("cat collar")),
        row(4, Some("dog collar")),
        row(5, Some("t cell exhaustion")),
        row(6, Some("b cell exhaustion")),
    ];
    rows.extend((10..40).map(|i| row(i, Some("filler words here"))));
    let (_d, db) = db_with(rows);
    let r = query(&db, "SELECT id FROM t ORDER BY bm25(body, 'vitamin D deficiency') DESC LIMIT 5");
    assert_eq!(ids(&r)[..2], [2, 1]);
    assert!(r.scores[0] > r.scores[1]);
    let r = query(&db, "SELECT id FROM t ORDER BY bm25(body, 'T cell exhaustion') DESC LIMIT 5");
    assert_eq!(ids(&r)[..2], [5, 6]);
    assert!(r.scores[0] > r.scores[1]);
    // A contraction's letter is still dropped: "it's" adds no `s`.
    let r = query(&db, "SELECT id FROM t ORDER BY bm25(body, 'it''s vitamin D') DESC LIMIT 5");
    let plain = query(&db, "SELECT id FROM t ORDER BY bm25(body, 'vitamin D') DESC LIMIT 5");
    assert_eq!(r.scores, plain.scores);
    // Same length, same document frequency: a term asked for twice
    // scores twice.
    let r = query(&db, "SELECT id FROM t ORDER BY bm25(body, 'dog cat cat') DESC LIMIT 5");
    assert_eq!(ids(&r)[..2], [3, 4]);
    assert!((score_of(&r, 3) / score_of(&r, 4) - 2.0).abs() < 1e-4, "{:?}", r.scores);
}

// IR-5: rows without a value counted in N and avgdl: with 90% of bodies
// NULL, avgdl was ten times too small and every length looked long.
#[test]
fn rows_without_text_leave_bm25_alone() {
    let texts: Vec<String> = (0..20).map(|i| format!("apple {}", "pear plum ".repeat(i % 7))).collect();
    let (_d, some) = db_with(texts.iter().enumerate().map(|(i, t)| row(i as i64, Some(t))).collect());
    let mut rows: Vec<Vec<Value>> = texts.iter().enumerate().map(|(i, t)| row(i as i64, Some(t))).collect();
    rows.extend((100..280).map(|i| row(i, None)));
    // Only stopwords: no indexed term either.
    rows.extend((300..320).map(|i| row(i, Some("the and of"))));
    let (_d2, sparse) = db_with(rows);
    let sql = "SELECT id FROM t ORDER BY bm25(body, 'apple plum') DESC LIMIT 20";
    let (a, b) = (query(&some, sql), query(&sparse, sql));
    assert_eq!(a.keys, b.keys);
    for (x, y) in a.scores.iter().zip(&b.scores) {
        assert!((x - y).abs() < 1e-5, "{:?} vs {:?}", a.scores, b.scores);
    }
}

// IR-1: a typo's neighbour and a synonym scored with their own IDF, so a
// document with a rare misspelling or a rare synonym outranked every
// document with the word asked for. Now each shares its word's.
#[test]
fn typos_and_synonyms_score_like_their_word() {
    // Row 1 has the rare spelling, rows 2..=21 the common one, row 22 the
    // rare synonym; every body has two terms.
    let mut rows = vec![row(1, Some("colour pencil")), row(22, Some("hue pencil"))];
    rows.extend((2..=21).map(|i| row(i, Some("color pencil"))));
    rows.extend((30..80).map(|i| row(i, Some("rubber eraser"))));
    let (_d, mut db) = db_with(rows);

    let r = query(&db, "SELECT id FROM t ORDER BY bm25(body, 'color', fuzzy => 1) DESC LIMIT 30");
    assert!(ids(&r).contains(&1), "the typo's row is found: {:?}", ids(&r));
    let color = score_of(&r, 2);
    for i in 2..=21 {
        assert!((score_of(&r, i) - color).abs() < 1e-6);
    }
    // One edit in five letters weighs 0.8 of the word.
    assert!((score_of(&r, 1) / color - 0.8).abs() < 1e-4, "{} vs {color}", score_of(&r, 1));
    assert_eq!(ids(&r).last(), Some(&1));

    db.exec = ExecOptions { synonyms: Some(Arc::new(Synonyms::parse("color, hue\n").unwrap())), ..ExecOptions::default() };
    let r = query(&db, "SELECT id FROM t ORDER BY bm25(body, 'color', synonyms => 1) DESC LIMIT 30");
    assert!((score_of(&r, 22) - score_of(&r, 2)).abs() < 1e-6, "{:?}", r.scores);
    // The word's own score is what it was without the expansion.
    let plain = query(&db, "SELECT id FROM t ORDER BY bm25(body, 'color') DESC LIMIT 30");
    assert!((score_of(&plain, 2) - score_of(&r, 2)).abs() < 1e-6);
}

// Found comparing the old and new evaluators of fused scores: under
// `ORDER BY <fused> DESC, id DESC`, one database cut a weighted sum or a
// product to its LIMIT by ascending key before ordering the tied rows by
// descending key, so it gave the smallest keys of a tie where shards gave
// the largest.
#[test]
fn fused_ties_follow_a_descending_key() {
    let (_d, db) = db_with((1..=50).map(|i| row(i, Some("apple pie"))).collect());
    for score in [
        "bm25(body, 'apple') * bm25(body, 'pie')",
        "0.5 * minmax(bm25(body, 'apple')) + 0.5 * minmax(bm25(body, 'pie'))",
        "bm25(body, 'apple') + ln(1 + bm25(body, 'pie'))",
    ] {
        let r = query(&db, &format!("SELECT id FROM t ORDER BY {score} DESC, id DESC LIMIT 3"));
        assert_eq!(ids(&r), vec![50, 49, 48], "{score}");
        let r = query(&db, &format!("SELECT id FROM t ORDER BY {score} DESC, id LIMIT 3"));
        assert_eq!(ids(&r), vec![1, 2, 3], "{score}");
    }
}
