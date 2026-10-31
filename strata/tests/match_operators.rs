//! `MATCH`'s operators read as written. Review bugs of 30 October BUG-13
//! (a malformed `~N` or `^weight` was searched as words, so `apple~9`
//! matched nothing that `bm25` of the same text ranks) and of 31 October
//! BUG-N8 (a word written twice in a group counted twice towards `@n`
//! and `min =>`, where `bm25`'s `min =>` counts it once).

use strata::db::{Db, SqlResult};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

fn db(path: &std::path::Path) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&path.join("t.strata"), seed, DbOptions { sync: false, segment_max_docs: 2, ..DbOptions::default() }).unwrap();
    sql(&db, "CREATE TABLE t (id INT PRIMARY KEY, s TEXT)");
    sql(&db, "INSERT INTO t (id, s) VALUES (1, 'apple pie'), (2, 'apple cherry tart'), (3, 'vaccine trial'), (4, 'mrna vaccine'), (5, 'cherry jam')");
    db
}

fn sql(db: &Db, s: &str) -> Vec<Vec<Value>> {
    match db.sql(s, &Params::new()).unwrap_or_else(|e| panic!("{s}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

/// The ids `MATCH(s, q{opts})` keeps, in order.
fn matching(db: &Db, q: &str, opts: &str) -> Vec<i64> {
    let rows = sql(db, &format!("SELECT id FROM t WHERE MATCH(s, '{q}'{opts}) ORDER BY id LIMIT 10"));
    rows.iter().map(|r| r[0].as_i64().unwrap()).collect()
}

/// The ids `bm25(s, q{opts})` ranks, in id order.
fn ranked(db: &Db, q: &str, opts: &str) -> Vec<i64> {
    let rows = sql(db, &format!("SELECT id FROM t ORDER BY bm25(s, '{q}'{opts}) DESC LIMIT 10"));
    let mut ids: Vec<i64> = rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
    ids.sort();
    ids
}

/// Review BUG-13: `apple~9` and `vaccine^1e308` became the words `apple 9`
/// and `vaccine 1e308`, a phrase no row holds, while `bm25` ranked the
/// rows with `apple` or `vaccine`. A `~` with more than 2 typos and a `^`
/// before something that is not a weight are errors now.
#[test]
fn a_malformed_operator_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    for (q, what) in [
        ("apple~9", "at most 2 typos"),
        ("apple~10", "at most 2 typos"),
        ("apple~9^2", "at most 2 typos"),
        ("vaccine^1e308", "a weight is"),
        ("vaccine^-1", "a weight is"),
        ("vaccine^1.5.2", "a weight is"),
        ("vaccine^2x", "a weight is"),
        ("vaccine^99999999999999999999999999999999999999999", "a weight is"),
        ("\"mrna vaccine\"^1e3", "a weight is"),
        ("(trial OR \"mrna vaccine\"^.5.5)", "a weight is"),
    ] {
        let s = format!("SELECT id FROM t WHERE MATCH(s, '{q}') LIMIT 10");
        let e = db.sql(&s, &Params::new()).err().unwrap_or_else(|| panic!("{q}: no error")).to_string();
        assert!(e.contains(what), "{q}: {e}");
    }
    // The operators as written still read.
    assert_eq!(matching(&db, "aple~1", ""), vec![1, 2]);
    assert_eq!(matching(&db, "aple~", ""), vec![1, 2]);
    assert_eq!(matching(&db, "apple~0 cherry~2", ""), vec![2]);
    assert_eq!(matching(&db, "vaccine^2 trial^0.5", ""), vec![3]);
    assert_eq!(matching(&db, "vaccine^.5", ""), vec![3, 4]);
    assert_eq!(matching(&db, "\"mrna vaccine\"^2", ""), vec![4]);
    // A `^` or `~` before what is no number is part of the word, as before.
    assert_eq!(matching(&db, "vaccine^x", ""), matching(&db, "vaccine x", ""));
    assert_eq!(matching(&db, "apple~x", ""), matching(&db, "apple x", ""));
    // A weight of 0 changes nothing about which rows match, and bm25
    // scores nothing for it: alone it ranks no row, beside a required
    // word it ranks that word's rows by the others.
    assert_eq!(matching(&db, "vaccine^0", ""), vec![3, 4]);
    assert!(ranked(&db, "vaccine^0", "").is_empty());
    assert_eq!(ranked(&db, "+vaccine^0 trial", ""), vec![3]);
    // bm25 reads its text leniently, as documented: what is not an
    // operator is words.
    assert_eq!(ranked(&db, "apple~9", ""), vec![1, 2]);
    assert_eq!(ranked(&db, "vaccine^1e308", ""), vec![3, 4]);
}

/// Review BUG-N8: `(apple apple cherry)@2` kept the row with `apple`
/// alone, since `apple` was two of the group's three items, while `bm25`
/// with `min => 2` counted it once and ranked only the row with both.
#[test]
fn a_word_written_twice_counts_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(dir.path());
    assert_eq!(matching(&db, "(apple apple cherry)@2", ""), vec![2]);
    assert_eq!(matching(&db, "apple apple cherry", ", min => 2"), vec![2]);
    assert_eq!(ranked(&db, "apple apple cherry", ", min => 2"), vec![2]);
    // Words that analyse to one term are one item, and so are phrases.
    assert_eq!(matching(&db, "(apple Apples cherry)@2", ""), vec![2]);
    assert_eq!(matching(&db, "(\"apple pie\" \"apple pie\" jam)@2", ""), Vec::<i64>::new());
    // One item: a row needs it.
    assert_eq!(matching(&db, "(apple apple)@2", ""), vec![1, 2]);
    // A percentage is of the items once each: half of two is one.
    assert_eq!(matching(&db, "(apple apple cherry)@50%", ""), vec![1, 2, 5]);
    assert_eq!(matching(&db, "apple apple cherry", ", min => '50%'"), vec![1, 2, 5]);
    // A count over the items is all of them.
    assert_eq!(matching(&db, "(apple cherry cherry)@3", ""), vec![2]);
    // A guard's levels count the items once too: from both of the two,
    // min_rows relaxes to one of them, and never below min.
    assert_eq!(matching(&db, "apple apple cherry", ", min => 1, min_rows => 1"), vec![2]);
    assert_eq!(matching(&db, "apple apple cherry", ", min => 1, min_rows => 3"), vec![1, 2, 5]);
    assert_eq!(matching(&db, "apple apple cherry", ", min => 2, min_rows => 3"), vec![2]);
}
