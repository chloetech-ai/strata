//! Term weights, required words and x of y in `bm25` and `MATCH`:
//! `word^w` weighs a word, `+word` and `+"a phrase"` must occur in a row,
//! `-word` must not, `min => n` (or `'p%'`) asks for n of the optional
//! words, and `syntax => 'plain'` reads the text as plain words. Every
//! answer is checked against BM25 computed by hand over a naive index, for
//! each top-k algorithm, with filters, as a rank-safe boost and as a rerank
//! phase's values, and over shards. `MATCH` reads a weight and matches as
//! without it, and its groups `(a b c)@2` and `min =>` match as counted by
//! hand.

mod text_common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::cluster;
use strata::db::Db;
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::ir::{Plan, MIN_MATCH, TEXT_OPERATORS};
use strata::query::plan::FilterMode;
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::text::query::{has_operators, parse_match, ranked_items, Need, Occur};
use strata::text::{Analyzer, BoolQuery};
use strata::value::Value;
use text_common::Naive;

const N: usize = 1500;
const K1: f32 = 1.2;
const B: f32 = 0.75;

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![Field::new("id", FieldType::Int), Field::new("body", FieldType::Text { positions: true }), Field::new("year", FieldType::Int)],
        Some("id"),
    )
    .unwrap()
}

/// Every document has a word, so each counts in BM25's statistics.
fn texts() -> Vec<String> {
    text_common::synthetic(17, N).into_iter().map(|t| format!("{t} doc")).collect()
}

fn year(d: usize) -> i64 {
    1990 + (d * 7 % 30) as i64
}

fn rows(texts: &[String], keep: impl Fn(usize) -> bool) -> Vec<Vec<Value>> {
    texts.iter().enumerate().filter(|(d, _)| keep(*d)).map(|(d, t)| vec![Value::Int(d as i64), Value::Str(t.clone()), Value::Int(year(d))]).collect()
}

fn create(dir: &std::path::Path, name: &str, rows: Vec<Vec<Value>>) -> Db {
    let db = Db::create(&dir.join(name), schema(), DbOptions { segment_max_docs: 400, ..DbOptions::default() }).unwrap();
    db.load(rows).unwrap();
    db
}

fn query(db: &Db, sql: &str, q: &str) -> ResultSet {
    db.query(sql, &Params::new().set("q", Value::Str(q.into()))).unwrap_or_else(|e| panic!("{sql} [{q}]: {e}"))
}

/// BM25 by hand: every matching row's score, by decreasing score then id.
struct Reference<'a> {
    naive: &'a Naive,
    /// Each term's BM25 of each document that has it.
    per_term: BTreeMap<String, BTreeMap<u32, f64>>,
    total_len: u64,
}

impl<'a> Reference<'a> {
    fn new(naive: &'a Naive) -> Self {
        Reference { naive, per_term: BTreeMap::new(), total_len: naive.lens.iter().map(|&l| l as u64).sum() }
    }

    fn term(&mut self, t: &str) -> &BTreeMap<u32, f64> {
        if !self.per_term.contains_key(t) {
            let s = self.naive.bm25(&[t.to_string()], N as u64, self.total_len, K1, B).into_iter().collect();
            self.per_term.insert(t.to_string(), s);
        }
        &self.per_term[t]
    }

    /// The rows `q` matches among `rows`, scored, best first.
    fn rank(&mut self, q: &str, rows: impl Fn(u32) -> bool) -> Vec<(u32, f64)> {
        self.rank_min(q, None, rows)
    }

    /// [`Self::rank`], the rows also holding `min` of its optional items.
    fn rank_min(&mut self, q: &str, min: Option<Need>, rows: impl Fn(u32) -> bool) -> Vec<(u32, f64)> {
        let (weights, mut must, not, should) = parse(q);
        if let (Some(need), false) = (min, should.is_empty()) {
            must.push(BoolQuery::AtLeast { n: need.of(should.len()), items: should });
        }
        let mut scores: BTreeMap<u32, f64> = BTreeMap::new();
        for (t, w) in &weights {
            for (&d, &s) in self.term(t) {
                *scores.entry(d).or_default() += w * s;
            }
        }
        let must: Vec<BTreeSet<u32>> = must.iter().map(|q| self.naive.eval(q)).collect();
        let not: Vec<BTreeSet<u32>> = not.iter().map(|q| self.naive.eval(q)).collect();
        let mut out: Vec<(u32, f64)> = scores.into_iter().filter(|(d, _)| rows(*d) && must.iter().all(|m| m.contains(d)) && !not.iter().any(|m| m.contains(d))).collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }
}

/// A query's scored terms and weights, its required and excluded words,
/// and its optional ones (each once), read by hand from the items.
fn parse(q: &str) -> (BTreeMap<String, f64>, Vec<BoolQuery>, Vec<BoolQuery>, Vec<BoolQuery>) {
    let (mut weights, mut must, mut not, mut should) = (BTreeMap::new(), Vec::new(), Vec::new(), Vec::new());
    for item in ranked_items(q) {
        let terms = Analyzer::English.query_term_seq(item.text, true);
        let bool_q = match terms.len() {
            0 => continue,
            1 => BoolQuery::Term(terms[0].clone()),
            _ => BoolQuery::Phrase(terms.clone()),
        };
        match item.occur {
            Occur::MustNot => {
                not.push(bool_q);
                continue;
            }
            Occur::Must => must.push(bool_q),
            Occur::Should if !should.contains(&bool_q) => should.push(bool_q),
            Occur::Should => {}
        }
        let w = item.weight.unwrap_or(1.0) as f64;
        if w > 0.0 {
            for t in terms {
                *weights.entry(t).or_insert(0.0) += w;
            }
        }
    }
    (weights, must, not, should)
}

/// `got` (ids and scores) against the reference's best `k`: the same
/// number of rows, each rank's score and each row's own score within 1e-4
/// relative (rows tied on score may come in either order).
fn check(got: &ResultSet, want: &[(u32, f64)], k: usize, what: &str) {
    let got: Vec<(u32, f32)> = got.keys.iter().zip(&got.scores).map(|(key, s)| (key.as_i64().unwrap() as u32, *s as f32)).collect();
    text_common::check_topk(&got, want, k, what);
}

/// A random query over the corpus's words: plain, weighted, required and
/// excluded words, and phrases cut from a document.
fn random_query(rng: &mut StdRng, texts: &[String]) -> String {
    let doc_words = |rng: &mut StdRng| -> Vec<String> {
        loop {
            let d = rng.gen_range(0..texts.len());
            let words: Vec<String> = texts[d].split(|c: char| c == ' ' || c == ',').filter(|w| w.starts_with('w')).map(str::to_string).collect();
            if words.len() >= 2 {
                break words;
            }
        }
    };
    let common = ["topic", "even", "sub3", "w0z", "w1z", "w2z", "w5z"];
    let mut items = Vec::new();
    for _ in 0..rng.gen_range(1..6) {
        let words = doc_words(rng);
        let body = match rng.gen_range(0..5) {
            0 => common[rng.gen_range(0..common.len())].to_string(),
            1 => {
                let i = rng.gen_range(0..words.len() - 1);
                format!("\"{} {}\"", words[i], words[i + 1])
            }
            _ => words[rng.gen_range(0..words.len())].clone(),
        };
        let sign = ["", "", "", "+", "-"][rng.gen_range(0..5)];
        let weight = ["", "", "^2", "^0.5", "^0", "^1.5", "^3"][rng.gen_range(0..7)];
        items.push(format!("{sign}{body}{weight}"));
    }
    items.join(" ")
}

#[test]
fn ranked_items_read_signs_phrases_and_weights() {
    let items = |q: &str| ranked_items(q).into_iter().map(|i| (i.text.to_string(), i.phrase, i.occur, i.weight)).collect::<Vec<_>>();
    let w = |t: &str, o: Occur, x: Option<f32>| (t.to_string(), false, o, x);
    let p = |t: &str, o: Occur, x: Option<f32>| (t.to_string(), true, o, x);
    use Occur::*;
    assert_eq!(
        items("vaccine^2 +mrna -animal \"messenger rna\"^0.5 +\"phase three\" trial"),
        vec![w("vaccine", Should, Some(2.0)), w("mrna", Must, None), w("animal", MustNot, None), p("messenger rna", Should, Some(0.5)), p("phase three", Must, None), w("trial", Should, None)]
    );
    // Lone or doubled signs, a sign inside a word, an unpaired quote and a
    // ^ without a number are words.
    assert_eq!(items("covid-19 - x + y +-z"), vec![w("covid-19", Should, None), w("-", Should, None), w("x", Should, None), w("+", Should, None), w("y", Should, None), w("+-z", Should, None)]);
    assert_eq!(items("a^ b^x c^.5 d^1.5.2 ^3"), vec![w("a^", Should, None), w("b^x", Should, None), w("c", Should, Some(0.5)), w("d^1.5.2", Should, None), w("^3", Should, None)]);
    assert_eq!(items("+\"open quote and more"), vec![w("\"open", Must, None), w("quote", Should, None), w("and", Should, None), w("more", Should, None)]);
    assert_eq!(items("say\"hello there\"^2x"), vec![w("say", Should, None), p("hello there", Should, None), w("^2x", Should, None)]);
    assert!(items("").is_empty() && items("  \t ").is_empty());
    assert!(has_operators("a +b") && has_operators("a -b") && has_operators("a^1") && has_operators("\"a b\"^2"));
    assert!(!has_operators("a \"b c\" covid-19 - + a^ +-x"));
    // Text without operators scores the words plain text does.
    let mut rng = StdRng::seed_from_u64(3);
    let alphabet = ['a', 'b', 'c', 'D', ' ', ' ', ',', '.', '\'', '"', '-', '+', '^', '1', '2', 'é', '\n'];
    for _ in 0..3000 {
        let text: String = (0..rng.gen_range(0..30)).map(|_| alphabet[rng.gen_range(0..alphabet.len())]).collect();
        if has_operators(&text) {
            continue;
        }
        let plain = Analyzer::English.query_term_seq(&text, true);
        let items: Vec<String> = ranked_items(&text).iter().flat_map(|i| Analyzer::English.query_term_seq(i.text, true)).collect();
        assert_eq!(items, plain, "{text:?}");
    }
}

#[test]
fn weighted_and_required_words_score_as_by_hand() {
    let dir = tempfile::tempdir().unwrap();
    let texts = texts();
    let db = create(dir.path(), "t.strata", rows(&texts, |_| true));
    let naive = Naive::new(&texts);
    let mut reference = Reference::new(&naive);
    let mut rng = StdRng::seed_from_u64(29);
    let mut matched = 0;
    for i in 0..150 {
        let q = random_query(&mut rng, &texts);
        let (lo, hi) = if i % 3 == 0 { (1990, 2019) } else { (1995 + (i % 10) as i64, 2005 + (i % 10) as i64) };
        let want = reference.rank(&q, |d| (lo..=hi).contains(&year(d as usize)));
        matched += want.len().min(1);
        for algo in ["exhaustive", "maxscore", "bmw"] {
            for k in [1, 10, 50] {
                let sql = format!("SELECT id, score() FROM t WHERE year BETWEEN {lo} AND {hi} ORDER BY bm25(body, :q, algo => '{algo}') DESC LIMIT {k}");
                for mode in [FilterMode::Materialize, FilterMode::Probe] {
                    let opts = ExecOptions { filter_mode: Some(mode), ..ExecOptions::default() };
                    let got = db.query_with(&sql, &Params::new().set("q", Value::Str(q.clone())), &opts).unwrap_or_else(|e| panic!("{sql} [{q}]: {e}"));
                    check(&got, &want, k, &format!("{q} | {algo} k={k} {mode:?}"));
                }
            }
        }
    }
    assert!(matched > 100, "most queries match rows: {matched}");
}

#[test]
fn integer_weights_are_repeated_words_and_plain_syntax_reads_words() {
    let dir = tempfile::tempdir().unwrap();
    let texts = texts();
    let db = create(dir.path(), "t.strata", rows(&texts, |_| true));
    let sql = |opts: &str| format!("SELECT id, score() FROM t ORDER BY bm25(body, :q{opts}) DESC LIMIT 40");
    let same = |a: &ResultSet, b: &ResultSet, what: &str| {
        assert_eq!(a.keys, b.keys, "{what}");
        assert_eq!(a.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>(), b.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>(), "{what}");
    };
    for (weighted, repeated) in [("topic^2 w1z", "topic topic w1z"), ("even^3 sub3^2 w5z", "even even even sub3 sub3 w5z"), ("\"w0z w1z\"^2", "w0z w1z w0z w1z")] {
        same(&query(&db, &sql(""), weighted), &query(&db, &sql(""), repeated), weighted);
    }
    // Plain syntax reads signs, quotes and weights as the words they hold.
    let q = "+topic -even \"w0z w1z\"^2 sub3^0";
    same(&query(&db, &sql(", syntax => 'plain'"), q), &query(&db, &sql(""), "topic even w0z w1z 2 sub3 0"), "plain");
    same(&query(&db, &sql(", syntax => 'query'"), q), &query(&db, &sql(""), q), "query is the default");
    let e = db.query(&sql(", syntax => 'lucene'"), &Params::new().set("q", Value::Str(q.into()))).unwrap_err().to_string();
    assert!(e.contains("bm25's syntax is 'query' or 'plain'"), "{e}");
    // A weight of 0 scores nothing; a required word with it still filters.
    let only_zero = query(&db, &sql(""), "topic^0");
    assert!(only_zero.keys.is_empty());
    let r = query(&db, &sql(""), "+topic^0 even");
    let plain_even = query(&db, &format!("SELECT id, score() FROM t WHERE MATCH(body, 'topic') ORDER BY bm25(body, :q) DESC LIMIT 40"), "even");
    same(&r, &plain_even, "+topic^0 even");
    // Only excluded words: nothing to score, nothing matches.
    assert!(query(&db, &sql(""), "-topic -even").keys.is_empty());
    // With fuzzy, a required or excluded word's typo neighbours count, as
    // in MATCH.
    let fuzzy = format!("SELECT id, score() FROM t ORDER BY bm25(body, :q, fuzzy => 1) DESC LIMIT 40");
    let by_match = |m: &str| format!("SELECT id, score() FROM t WHERE MATCH(body, '{m}', fuzzy => 1) ORDER BY bm25(body, :q, fuzzy => 1) DESC LIMIT 40");
    let r = query(&db, &fuzzy, "+topc even");
    assert!(!r.keys.is_empty());
    same(&r, &query(&db, &by_match("topc"), "topc even"), "+topc");
    same(&query(&db, &fuzzy, "-topc even"), &query(&db, &by_match("even -topc"), "even"), "-topc");
    // A word's typo terms take its weight: every score doubles.
    let (one, two) = (query(&db, &fuzzy, "topc"), query(&db, &fuzzy, "topc^2"));
    assert!(!one.keys.is_empty());
    assert_eq!(one.keys, two.keys);
    for (a, b) in one.scores.iter().zip(&two.scores) {
        assert!((2.0 * a - b).abs() <= 1e-5 * b, "{a} {b}");
    }
}

#[test]
fn required_words_hold_in_boosts_and_phases() {
    let dir = tempfile::tempdir().unwrap();
    let texts = texts();
    let db = create(dir.path(), "t.strata", rows(&texts, |_| true));
    let naive = Naive::new(&texts);
    let mut reference = Reference::new(&naive);
    let mut rng = StdRng::seed_from_u64(41);
    for _ in 0..60 {
        let q = random_query(&mut rng, &texts);
        let want = reference.rank(&q, |_| true);
        let by_id: BTreeMap<u32, f64> = want.iter().copied().collect();
        // A rank-safe boost, driven by the text score's postings.
        let sql = "SELECT id, score() FROM t ORDER BY bm25(body, :q) * (1 + 0.01 * (year - 1990)) DESC LIMIT 20";
        let got = query(&db, sql, &q);
        assert!(got.stats.plan.iter().any(|l| l.contains("rank-safe")), "{:?}", got.stats.plan);
        let mut boosted: Vec<(u32, f64)> = want.iter().map(|&(d, s)| (d, s * (1.0 + 0.01 * (year(d as usize) - 1990) as f64))).collect();
        boosted.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        check(&got, &boosted, 20, &format!("{q} | boost"));
        // A rerank phase over rows the text did not choose: a row the
        // query does not match scores 0.
        let sql = "WITH top AS (SELECT * FROM t ORDER BY year DESC LIMIT 300) SELECT id, bm25(body, :q) AS b FROM top ORDER BY bm25(body, :q) DESC LIMIT 300";
        let got = query(&db, sql, &q);
        assert_eq!(got.rows.len(), 300, "{q}");
        for (row, s) in got.rows.iter().zip(&got.scores) {
            let d = row[0].as_i64().unwrap() as u32;
            let w = by_id.get(&d).copied().unwrap_or(0.0);
            assert!((s - w).abs() <= 1e-4 * w.abs().max(1.0), "{q}: row {d} scored {s}, by hand {w}");
            assert_eq!(row[1].as_f64().unwrap().to_bits(), s.to_bits(), "{q}: the column is the score");
        }
    }
}

#[test]
fn shards_answer_as_one_database() {
    let dir = tempfile::tempdir().unwrap();
    let texts = texts();
    let db = create(dir.path(), "all.strata", rows(&texts, |_| true));
    let shards: Vec<Db> = (0..3).map(|s| create(dir.path(), &format!("s{s}.strata"), rows(&texts, |d| d % 3 == s))).collect();
    let mut rng = StdRng::seed_from_u64(53);
    for _ in 0..40 {
        let q = random_query(&mut rng, &texts);
        let params = Params::new().set("q", Value::Str(q.clone()));
        for sql in [
            "SELECT id, score() FROM t WHERE year >= 2000 ORDER BY bm25(body, :q) DESC LIMIT 15",
            "SELECT id, score() FROM t ORDER BY bm25(body, :q) * (1 + 0.01 * (year - 1990)) DESC LIMIT 15",
            "SELECT id, score() FROM t WHERE year >= 2000 ORDER BY bm25(body, :q, min => 2) DESC LIMIT 15",
            "SELECT id, score() FROM t ORDER BY bm25(body, :q, min => '50%') * (1 + 0.01 * (year - 1990)) DESC LIMIT 15",
        ] {
            let whole = db.query(sql, &params).unwrap();
            let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
            let select = bind(sql, snaps[0].schema(), &params).unwrap();
            let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
            let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() };
            let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &select, &opts).unwrap()).collect();
            let split = cluster::merge(&select, answers).unwrap();
            assert_eq!(whole.keys.len(), split.keys.len(), "{q}");
            for (i, (a, b)) in whole.scores.iter().zip(&split.scores).enumerate() {
                assert!((a - b).abs() <= 1e-6 * a.abs().max(1.0), "{q}: rank {i}: {a} vs {b}");
            }
        }
    }
}

#[test]
fn plans_name_text_operators() {
    let dir = tempfile::tempdir().unwrap();
    let texts = texts();
    let db = create(dir.path(), "t.strata", rows(&texts, |_| true));
    let features = |sql: &str, q: &str| db.explain(sql, &Params::new().set("q", Value::Str(q.into()))).unwrap().features.iter().any(|f| f == TEXT_OPERATORS);
    let rank = "SELECT id FROM t ORDER BY bm25(body, :q) DESC LIMIT 5";
    assert!(features(rank, "+topic even"));
    assert!(features(rank, "topic^2"));
    assert!(features(rank, "topic -even"));
    assert!(!features(rank, "topic \"even w1z\" covid-19"));
    assert!(!features("SELECT id FROM t ORDER BY bm25(body, :q, syntax => 'plain') DESC LIMIT 5", "+topic^2"));
    assert!(features("SELECT id FROM t WHERE MATCH(body, 'topic^2 even') LIMIT 5", ""));
    assert!(!features("SELECT id FROM t WHERE MATCH(body, 'topic even') LIMIT 5", ""));
    let e = Plan::readable_by(1, &[TEXT_OPERATORS], &["operators"]).unwrap_err().to_string();
    assert!(e.contains("text_operators"), "{e}");
    // Groups and min: min_match, and text_operators only with weights or signs.
    let min = |sql: &str, q: &str| db.explain(sql, &Params::new().set("q", Value::Str(q.into()))).unwrap().features.iter().any(|f| f == MIN_MATCH);
    let ranked_min = "SELECT id FROM t ORDER BY bm25(body, :q, min => 2) DESC LIMIT 5";
    assert!(min(ranked_min, "topic even w1z") && !features(ranked_min, "topic even w1z"));
    assert!(min("SELECT id FROM t ORDER BY bm25(body, :q, min => '50%', syntax => 'plain') DESC LIMIT 5", "topic even"));
    assert!(!min(rank, "+topic even"));
    for m in ["(topic even w1z)@2", "topic AND (even OR w1z)@50%"] {
        let sql = format!("SELECT id FROM t WHERE MATCH(body, '{m}') LIMIT 5");
        assert!(min(&sql, "") && !features(&sql, ""), "{m}");
    }
    assert!(min("SELECT id FROM t WHERE MATCH(body, 'topic even w1z', min => 2) LIMIT 5", ""));
    let e = Plan::readable_by(1, &[MIN_MATCH], &["operators", TEXT_OPERATORS]).unwrap_err().to_string();
    assert!(e.contains("min_match"), "{e}");
    let params = Params::new().set("q", Value::Str("topic even w1z sub3".into()));
    let plan = db.explain(ranked_min, &params).unwrap();
    let strata::db::SqlResult::Rows(a) = db.run(&plan).unwrap() else { panic!("rows") };
    assert_eq!(a.keys, db.query(ranked_min, &params).unwrap().keys);
    // A plan with them runs as the SQL does.
    let params = Params::new().set("q", Value::Str("+topic even^2 -w1z".into()));
    let plan = db.explain(rank, &params).unwrap();
    let strata::db::SqlResult::Rows(a) = db.run(&plan).unwrap() else { panic!("rows") };
    assert_eq!(a.keys, db.query(rank, &params).unwrap().keys);
}

#[test]
fn match_reads_weights_and_matches_without_them() {
    let dir = tempfile::tempdir().unwrap();
    let texts = texts();
    let db = create(dir.path(), "t.strata", rows(&texts, |_| true));
    let ids = |m: &str| query(&db, &format!("SELECT id FROM t WHERE MATCH(body, '{m}') ORDER BY id LIMIT 2000"), "").keys;
    for (weighted, plain) in [("topic^2 even", "topic even"), ("\"w0z w1z\"^0.5 OR sub3^3", "\"w0z w1z\" OR sub3"), ("topic -even^2", "topic -even"), ("w1* ^2", "w1* 2")] {
        assert_eq!(ids(weighted), ids(plain), "{weighted}");
    }
    assert!(!ids("topic^2").is_empty());
}

/// A random `min`: a count or a percentage.
fn random_need(rng: &mut StdRng) -> (Need, String) {
    match rng.gen_range(0..6) {
        0 => (Need::Count(1), "1".into()),
        1 | 2 => (Need::Count(2), "2".into()),
        3 => (Need::Count(3), "3".into()),
        4 => (Need::Percent(50), "'50%'".into()),
        _ => (Need::Percent(75), "'75%'".into()),
    }
}

#[test]
fn min_counts_optional_words_as_by_hand() {
    let dir = tempfile::tempdir().unwrap();
    let texts = texts();
    let db = create(dir.path(), "t.strata", rows(&texts, |_| true));
    let naive = Naive::new(&texts);
    let mut reference = Reference::new(&naive);
    let mut rng = StdRng::seed_from_u64(61);
    let (mut matched, mut fewer) = (0, 0);
    for i in 0..150 {
        let q = random_query(&mut rng, &texts);
        let (need, min) = random_need(&mut rng);
        let (lo, hi) = if i % 3 == 0 { (1990, 2019) } else { (1995 + (i % 10) as i64, 2005 + (i % 10) as i64) };
        let want = reference.rank_min(&q, Some(need), |d| (lo..=hi).contains(&year(d as usize)));
        matched += want.len().min(1);
        fewer += (want.len() < reference.rank(&q, |d| (lo..=hi).contains(&year(d as usize))).len()) as usize;
        for algo in ["exhaustive", "maxscore", "bmw"] {
            for k in [1, 10, 50] {
                let sql = format!("SELECT id, score() FROM t WHERE year BETWEEN {lo} AND {hi} ORDER BY bm25(body, :q, algo => '{algo}', min => {min}) DESC LIMIT {k}");
                for mode in [FilterMode::Materialize, FilterMode::Probe] {
                    let opts = ExecOptions { filter_mode: Some(mode), ..ExecOptions::default() };
                    let got = db.query_with(&sql, &Params::new().set("q", Value::Str(q.clone())), &opts).unwrap_or_else(|e| panic!("{sql} [{q}]: {e}"));
                    check(&got, &want, k, &format!("{q} | min {min} {algo} k={k} {mode:?}"));
                }
            }
        }
        // A rank-safe boost, and a rerank phase over rows the text did not
        // choose (a row with too few of the words scores 0).
        let all = reference.rank_min(&q, Some(need), |_| true);
        let mut boosted: Vec<(u32, f64)> = all.iter().map(|&(d, s)| (d, s * (1.0 + 0.01 * (year(d as usize) - 1990) as f64))).collect();
        boosted.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let sql = format!("SELECT id, score() FROM t ORDER BY bm25(body, :q, min => {min}) * (1 + 0.01 * (year - 1990)) DESC LIMIT 20");
        check(&query(&db, &sql, &q), &boosted, 20, &format!("{q} | min {min} boost"));
        let by_id: BTreeMap<u32, f64> = all.iter().copied().collect();
        let sql = format!("WITH top AS (SELECT * FROM t ORDER BY year DESC LIMIT 300) SELECT id FROM top ORDER BY bm25(body, :q, min => {min}) DESC LIMIT 300");
        let got = query(&db, &sql, &q);
        for (key, s) in got.keys.iter().zip(&got.scores) {
            let d = key.as_i64().unwrap() as u32;
            let w = by_id.get(&d).copied().unwrap_or(0.0);
            assert!((s - w).abs() <= 1e-4 * w.abs().max(1.0), "{q} min {min}: row {d} scored {s}, by hand {w}");
        }
    }
    assert!(matched > 60 && fewer > 30, "queries with rows: {matched}, cut by min: {fewer}");
}

#[test]
fn min_reads_plain_words_and_ignores_a_text_without_optional_ones() {
    let dir = tempfile::tempdir().unwrap();
    let texts = texts();
    let db = create(dir.path(), "t.strata", rows(&texts, |_| true));
    let sql = |opts: &str| format!("SELECT id, score() FROM t ORDER BY bm25(body, :q{opts}) DESC LIMIT 40");
    let same = |a: &ResultSet, b: &ResultSet, what: &str| {
        assert_eq!(a.keys, b.keys, "{what}");
        assert_eq!(a.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>(), b.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>(), "{what}");
    };
    // Plain syntax counts each word once.
    for q in ["topic even w1z", "topic topic even sub3 w5z", "w0z w1z w2z w5z"] {
        same(&query(&db, &sql(", min => 2, syntax => 'plain'"), q), &query(&db, &sql(", min => 2"), q), q);
    }
    // A word written twice is one item: two of `topic topic even` is both.
    same(&query(&db, &sql(", min => 2"), "topic topic even"), &query(&db, &sql(""), "+topic topic +even"), "repeats");
    // Weight 0 scores nothing but counts.
    same(&query(&db, &sql(", min => 2"), "topic^0 even"), &query(&db, &sql(""), "+topic^0 +even"), "^0");
    // Required words and no optional ones: min changes nothing.
    same(&query(&db, &sql(", min => 3"), "+topic +even"), &query(&db, &sql(""), "+topic +even"), "no optional");
    // min => 1 is any of them, which bm25 already asks; over the count, all.
    same(&query(&db, &sql(", min => 1"), "topic even w1z"), &query(&db, &sql(""), "topic even w1z"), "min 1");
    same(&query(&db, &sql(", min => 9"), "topic even w1z"), &query(&db, &sql(""), "+topic +even +w1z"), "min 9");
    for bad in ["'x'", "-1", "1.5", "'150%'"] {
        let e = db.query(&sql(&format!(", min => {bad}")), &Params::new().set("q", Value::Str("topic".into()))).unwrap_err().to_string();
        assert!(e.contains("min => a count"), "{bad}: {e}");
    }
}

/// A random `MATCH` group of words, phrases, prefixes, nested groups and
/// excluded words, with `@n` or `@p%`, alone or beside other operands.
fn random_group(rng: &mut StdRng, texts: &[String], depth: usize) -> String {
    let words: Vec<String> = loop {
        let d = rng.gen_range(0..texts.len());
        let w: Vec<String> = texts[d].split(|c: char| c == ' ' || c == ',').filter(|w| w.starts_with('w')).map(str::to_string).collect();
        if w.len() >= 2 {
            break w;
        }
    };
    let common = ["topic", "even", "sub3", "w0z", "w1z", "w2z", "w5z", "doc"];
    let mut items = Vec::new();
    for _ in 0..rng.gen_range(2..6) {
        items.push(match rng.gen_range(0..9) {
            0 | 1 => common[rng.gen_range(0..common.len())].to_string(),
            2 => {
                let i = rng.gen_range(0..words.len() - 1);
                format!("\"{} {}\"", words[i], words[i + 1])
            }
            3 => format!("w{}*", rng.gen_range(1..30)),
            4 if depth < 2 => random_group(rng, texts, depth + 1),
            5 => format!("({} OR {})", common[rng.gen_range(0..common.len())], words[0]),
            6 => format!("-{}", common[rng.gen_range(0..common.len())]),
            _ => words[rng.gen_range(0..words.len())].clone(),
        });
    }
    let need = ["@1", "@2", "@2", "@3", "@50%", "@75%", "@9"][rng.gen_range(0..7)];
    let join = if rng.gen_bool(0.3) { " OR " } else { " " };
    let group = format!("({}){need}", items.join(join));
    match (depth, rng.gen_range(0..4)) {
        (0, 0) => format!("topic AND {group}"),
        (0, 1) => format!("{group} OR sub2"),
        _ => group,
    }
}

/// Whether the query has a group that counts its items.
fn counts(q: &BoolQuery) -> bool {
    match q {
        BoolQuery::AtLeast { .. } => true,
        BoolQuery::And(cs) | BoolQuery::Or(cs) => cs.iter().any(counts),
        BoolQuery::Not(c) => counts(c),
        _ => false,
    }
}

#[test]
fn match_groups_match_as_counted_by_hand() {
    let dir = tempfile::tempdir().unwrap();
    let texts = texts();
    let db = create(dir.path(), "t.strata", rows(&texts, |_| true));
    let naive = Naive::new(&texts);
    let mut rng = StdRng::seed_from_u64(67);
    let ids = |sql: &str, mode: FilterMode| -> Vec<u32> {
        let opts = ExecOptions { filter_mode: Some(mode), ..ExecOptions::default() };
        db.query_with(sql, &Params::new(), &opts).unwrap_or_else(|e| panic!("{sql}: {e}")).keys.iter().map(|k| k.as_i64().unwrap() as u32).collect()
    };
    let (mut some, mut cut) = (0, 0);
    for _ in 0..200 {
        let m = random_group(&mut rng, &texts, 0);
        let want: Vec<u32> = naive.eval(&parse_match(&m).unwrap()).into_iter().collect();
        some += !want.is_empty() as usize;
        cut += counts(&parse_match(&m).unwrap()) as usize;
        for mode in [FilterMode::Materialize, FilterMode::Probe] {
            assert_eq!(ids(&format!("SELECT id FROM t WHERE MATCH(body, '{m}') ORDER BY id LIMIT 5000"), mode), want, "{m} {mode:?}");
            let sql = format!("SELECT id FROM t WHERE year BETWEEN 2000 AND 2004 AND MATCH(body, '{m}') ORDER BY id LIMIT 5000");
            let kept: Vec<u32> = want.iter().copied().filter(|&d| (2000..=2004).contains(&year(d as usize))).collect();
            assert_eq!(ids(&sql, mode), kept, "{m} {mode:?} with a filter");
        }
    }
    assert!(some > 100 && cut > 40, "groups with rows: {some}, counted groups: {cut}");
    // min => on MATCH is the whole query as one group.
    for (q, min) in [("topic even w1z sub3", "2"), ("topic OR even OR w1z", "'50%'"), ("topic even -w1z \"w0z w1z\"", "2"), ("the topic", "3")] {
        let a = ids(&format!("SELECT id FROM t WHERE MATCH(body, '{q}', min => {min}) ORDER BY id LIMIT 5000"), FilterMode::Materialize);
        let b = ids(&format!("SELECT id FROM t WHERE MATCH(body, '({q})@{}') ORDER BY id LIMIT 5000", min.trim_matches('\'')), FilterMode::Materialize);
        assert_eq!(a, b, "{q} min {min}");
    }
}
