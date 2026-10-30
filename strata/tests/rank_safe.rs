//! Rank-safe boosts (`ORDER BY bm25(body, :t) * (1 + 0.2 * rating)`): the
//! text index's bounds drive an expression of one BM25 score and row values,
//! and its top k is the one that scoring every matching row gives, with
//! values written in place and rows deleted, on one database and over
//! shards.

use std::sync::Arc;

use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::cluster;
use strata::data::{self, col};
use strata::db::Db;
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::plan::FilterMode;
use strata::query::stats::TermStats;
use strata::query::{bind, Params};
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

const N: usize = 6000;
const DIM: usize = 8;
/// Segment sizes: a top k per segment of a few thousand rows meets a bound
/// that is too low far more often than one of a few hundred.
const SEG: usize = 400;
const BIG: usize = 3000;

fn open(dir: &std::path::Path, name: &str, docs: &[strata::Document], seg: usize) -> Db {
    let opts = DbOptions { segment_max_docs: seg, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), data::beir_schema(DIM, VectorIndexKind::Flat), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db
}

/// The corpus ([`docs`]); then ratings set in place far above the others (a bound that missed them would drop their
/// rows), some set to NULL, and some rows deleted.
fn corpus(dir: &std::path::Path, name: &str, docs: &[strata::Document], seg: usize) -> Db {
    let db = open(dir, name, docs, seg);
    let p = Params::new();
    db.sql("UPDATE docs SET rating = 40 WHERE id IN ('d0000017', 'd0000500', 'd0001234', 'd0002999', 'd0002001')", &p).unwrap();
    db.sql("UPDATE docs SET rating = 25.5 WHERE year = 2003", &p).unwrap();
    db.sql("UPDATE docs SET rating = NULL WHERE id IN ('d0000018', 'd0000501')", &p).unwrap();
    db.sql("DELETE FROM docs WHERE id IN ('d0000019', 'd0001235', 'd0002000')", &p).unwrap();
    db
}

/// Every seventh row without a rating, every eleventh without a year.
fn docs() -> Vec<strata::Document> {
    let mut docs = data::synthetic(N, DIM, 23);
    for d in docs.iter_mut().step_by(7) {
        d[col::RATING] = Value::Null;
    }
    for d in docs.iter_mut().skip(3).step_by(11) {
        d[col::YEAR] = Value::Null;
    }
    docs
}

/// Expressions a query might rank by, written out.
const SHAPES: [&str; 12] = [
    "B * (1 + 0.2 * coalesce(rating, 0))",
    "B * (1 + 0.2 * rating)",
    "B + 0.5 * rating",
    "ln(1 + B) * decay(year, 2015, 3)",
    "B * decay(year, 2015, 3)",
    "B * rating",
    "CASE WHEN category = 'c01' THEN 2 * B ELSE B END",
    "B / (1 + abs(rating))",
    "greatest(B, 2 * rating)",
    "B * (1 + year / 1000)",
    "rating - B",
    "sqrt(B) + least(rating, 3)",
];

/// A random expression with BM25 (`B`) in it.
fn random_expr(rng: &mut StdRng, depth: u32) -> String {
    fn go(rng: &mut StdRng, depth: u32) -> String {
        if depth == 0 || rng.gen_bool(0.3) {
            return match rng.gen_range(0..10) {
                0..=3 => "B".into(),
                4 => "rating".into(),
                5 => "coalesce(rating, 0)".into(),
                6 => "decay(year, 2012, 4)".into(),
                7 => "CASE WHEN category = 'c01' THEN 1 ELSE 0 END".into(),
                8 => "year / 1000".into(),
                _ => ["0.5", "2", "1", "0.2", "3", "-1"][rng.gen_range(0..6)].into(),
            };
        }
        let a = go(rng, depth - 1);
        match rng.gen_range(0..13) {
            0 => format!("({a} + {})", go(rng, depth - 1)),
            1 => format!("({a} - {})", go(rng, depth - 1)),
            2 | 3 => format!("({a} * {})", go(rng, depth - 1)),
            4 => format!("({a} / {})", go(rng, depth - 1)),
            5 => format!("ln(1 + abs({a}))"),
            6 => format!("sqrt({a})"),
            7 => format!("least({a}, {})", go(rng, depth - 1)),
            8 => format!("greatest({a}, {})", go(rng, depth - 1)),
            9 => format!("clamp({a}, 0, 5)"),
            10 => format!("sigmoid({a})"),
            11 => format!("pow({a}, 2)"),
            _ => format!("coalesce({a}, {})", go(rng, depth - 1)),
        }
    }
    match go(rng, depth) {
        e if e == "B" || !e.contains('B') => format!("B * (1 + 0.1 * {e})"),
        e => e,
    }
}

/// A query ranking by `expr`, its BM25 leaf with `opts`.
fn sql(expr: &str, opts: &str, filter: &str, tail: &str) -> String {
    let b = format!("bm25(body, :t{opts})");
    format!("SELECT id, score() FROM docs {filter} ORDER BY {} DESC{tail}", expr.replace('B', &b))
}

fn run(db: &Db, sql: &str, params: &Params, opts: &ExecOptions) -> ResultSet {
    let snap = db.snapshot();
    let select = bind(sql, snap.schema(), params).unwrap_or_else(|e| panic!("{sql}: {e}"));
    db.execute(&snap, &select, opts).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The coordinator's rounds over `shards`.
fn sharded(shards: &[Db], sql: &str, params: &Params) -> ResultSet {
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
    let select = bind(sql, snaps[0].schema(), params).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
    let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() };
    let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &select, &opts).unwrap_or_else(|e| panic!("{sql}: {e}"))).collect();
    cluster::merge(&select, answers).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The same rows with the same scores, bit for bit, in the same order.
fn identical(what: &str, a: &ResultSet, b: &ResultSet) {
    assert_eq!(a.keys, b.keys, "{what}: keys");
    let bits = |r: &ResultSet| r.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(a), bits(b), "{what}: scores");
}

/// The same scores, in order, to 1e-9 relative; keys in the same order,
/// except that rows tied on score may swap within their tie, and a tie
/// cut by the page may hold different rows.
fn same(sql: &str, whole: &ResultSet, split: &ResultSet) {
    assert_eq!(whole.keys.len(), split.keys.len(), "{sql}: row counts");
    let close = |a: f64, b: f64| a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0);
    for (i, (a, b)) in whole.scores.iter().zip(&split.scores).enumerate() {
        assert!(close(*a, *b), "{sql}: row {i}: {a} vs {b}");
    }
    let mut i = 0;
    while i < whole.keys.len() {
        let mut j = i + 1;
        while j < whole.keys.len() && close(whole.scores[i], whole.scores[j]) {
            j += 1;
        }
        if j < whole.keys.len() {
            let mut a: Vec<String> = whole.keys[i..j].iter().map(|k| format!("{k:?}")).collect();
            let mut b: Vec<String> = split.keys[i..j].iter().map(|k| format!("{k:?}")).collect();
            a.sort();
            b.sort();
            assert_eq!(a, b, "{sql}: rows {i}..{j}");
        }
        i = j;
    }
}

/// The text of a few words of row `i`'s title.
fn words(docs: &[strata::Document], i: usize, n: usize) -> String {
    docs[i][col::TITLE].as_str().unwrap().split(' ').take(n).collect::<Vec<_>>().join(" ")
}

/// Over shapes and random expressions, filters, pages, tiebreaks, top-k
/// algorithms and filter modes, the rank-safe top k is the top k of every
/// matching row (each one a candidate: `depth` past the matches, the
/// path from before), bit for bit; and the bounds skip rows.
#[test]
fn the_top_k_is_scoring_every_matching_row() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let mut rng = StdRng::seed_from_u64(29);
    let reference = ExecOptions { rank_safe: false, ..ExecOptions::default() };
    let (mut queries, mut safe, mut skipped, mut driven, mut every) = (0, 0, 0usize, 0, 0);
    // Each shape 12 times (each top-k algorithm thrice), then random
    // expressions.
    let shaped = SHAPES.len() * 12;
    for (db, qi) in [BIG, SEG].iter().map(|&seg| corpus(dir.path(), &format!("s{seg}.strata"), &docs, seg)).flat_map(|db| {
        let db = Arc::new(db);
        (0..shaped + 200).map(move |qi| (db.clone(), qi))
    }) {
        let expr = match SHAPES.get(qi / 12) {
            Some(s) => s.to_string(),
            None => random_expr(&mut rng, 3),
        };
        let params = Params::new().set("t", Value::Str(words(&docs, rng.gen_range(0..N), rng.gen_range(1..5))));
        let algo = ["", ", algo => 'maxscore'", ", algo => 'bmw'", ", algo => 'exhaustive'"][qi % 4];
        let filter = ["", "WHERE year >= 2005", "WHERE category IN ('c00', 'c01')", "WHERE rating > 2", "WHERE topic = 't03'"][rng.gen_range(0..5)];
        let tail = [" LIMIT 1", " LIMIT 10", " LIMIT 100", " LIMIT 10 OFFSET 15", ", id DESC LIMIT 7", " LIMIT 1000"][rng.gen_range(0..6)];
        let sql = sql(&expr, &format!("{algo}, depth => 100000"), filter, tail);
        let want = run(&db, &sql, &params, &reference);
        for mode in [None, Some(FilterMode::Probe), Some(FilterMode::Materialize)] {
            let opts = ExecOptions { filter_mode: mode, ..ExecOptions::default() };
            let got = run(&db, &sql, &params, &opts);
            identical(&format!("{sql} {mode:?}"), &want, &got);
            // Every shape is a rank-safe boost; a random expression is
            // not where a product of row values alone also takes rows.
            let plan = got.stats.plan.join("\n");
            assert!(qi >= shaped || plan.contains("rank-safe"), "{sql}:\n{plan}");
            safe += plan.contains("rank-safe") as usize;
            driven += plan.matches("ranks by the expression").count();
            every += plan.matches("every match scored").count();
            skipped += (want.stats.scored - got.stats.scored.min(want.stats.scored)) as usize;
        }
        queries += 1;
    }
    assert_eq!(queries, 2 * (shaped + 200));
    assert!(safe > queries * 3 * 9 / 10, "{safe} of {} runs rank-safe", queries * 3);
    // Most segments ran a top-k loop on the bound; some expressions have
    // none that narrows, and every match was scored.
    assert!(driven > 10_000 && every > 1000 && every < driven / 3, "{driven} {every}");
    assert!(skipped > 1_000_000, "rows left unscored: {skipped}");
}

/// A split collection answers rank-safe boosts as one database, and a
/// coordinator from before them (taking each shard's top `depth` of BM25)
/// still gets what it fused before.
#[test]
fn shards_rank_like_one_database() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let whole = corpus(dir.path(), "whole.strata", &docs, SEG);
    let splits: Vec<Vec<Db>> = [1, 2, 4]
        .iter()
        .map(|&k| (0..k).map(|i| corpus(dir.path(), &format!("s{k}-{i}.strata"), &docs[i * N / k..(i + 1) * N / k], SEG)).collect())
        .collect();
    let mut rng = StdRng::seed_from_u64(31);
    for qi in 0..60 {
        let expr = match SHAPES.get(qi) {
            Some(s) => s.to_string(),
            None => random_expr(&mut rng, 3),
        };
        let params = Params::new().set("t", Value::Str(words(&docs, rng.gen_range(0..N), rng.gen_range(1..5))));
        let filter = ["", "WHERE year >= 2005", "WHERE category IN ('c00', 'c01')"][qi % 3];
        let tail = [" LIMIT 10", " LIMIT 100", " LIMIT 10 OFFSET 15", ", id DESC LIMIT 7"][rng.gen_range(0..4)];
        let sql = sql(&expr, "", filter, tail);
        let want = whole.query(&sql, &params).unwrap_or_else(|e| panic!("{sql}: {e}"));
        for shards in &splits {
            same(&format!("{sql} on {} shards", shards.len()), &want, &sharded(shards, &sql, &params));
        }
    }
}

/// A rank-safe boost changes which rows are candidates, so its answer can
/// be better than the top `depth` of BM25 gave: a row far down by BM25
/// with a large rating, which the old path never scored.
#[test]
fn a_row_far_down_by_bm25_ranks_first() {
    let mut docs = data::synthetic(400, DIM, 3);
    for (i, d) in docs.iter_mut().enumerate() {
        d[col::TITLE] = Value::Str(if i == 399 { "apple".into() } else { "apple apple apple".into() });
        d[col::RATING] = Value::Float(if i == 399 { 1000.0 } else { 1.0 });
    }
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "far.strata", &docs, SEG);
    let params = Params::new().set("t", Value::Str("apple".into()));
    let sql = "SELECT id, score() FROM docs ORDER BY bm25(body, :t, depth => 20) * rating DESC LIMIT 1";
    let old = run(&db, sql, &params, &ExecOptions { rank_safe: false, ..ExecOptions::default() });
    let new = run(&db, sql, &params, &ExecOptions::default());
    assert_ne!(old.keys[0], Value::Str("d0000399".into()));
    assert_eq!(new.keys[0], Value::Str("d0000399".into()));
    let explain = new.stats.plan.join("\n");
    assert!(explain.contains("its own top 1 (rank-safe: the text score's bounds drive it)"), "{explain}");
}

/// `seq_no()` in a boost reads each row's write number, as the evaluator
/// does; nothing bounds it, so every match is scored.
#[test]
fn a_boost_by_seq_no_ranks_every_match() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { segment_max_docs: SEG, ..DbOptions::default() };
    let db = Db::create(&dir.path().join("seq.strata"), data::beir_schema(DIM, VectorIndexKind::Flat), opts).unwrap();
    db.storage().start_seq_no().unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db.sql("UPDATE docs SET rating = 3 WHERE year = 2003", &Params::new()).unwrap();
    let reference = ExecOptions { rank_safe: false, ..ExecOptions::default() };
    for i in 0..20 {
        let params = Params::new().set("t", Value::Str(words(&docs, i * 97, 2)));
        let sql = sql("B * (1 + seq_no() / 100000)", ", depth => 100000", "", " LIMIT 10");
        let want = run(&db, &sql, &params, &reference);
        let got = run(&db, &sql, &params, &ExecOptions::default());
        identical(&sql, &want, &got);
        assert!(got.stats.plan.join("\n").contains("every match scored"));
    }
}
