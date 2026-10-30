//! Rerank phases (`WITH top AS (... ORDER BY bm25(body, :t) DESC LIMIT
//! 100) SELECT ... FROM top ORDER BY <expression> DESC`): a phase ranks the
//! rows of the one before as the general plan of the same SQL does; one
//! database answers like the same rows split over 1, 2 and 4 shards; a
//! phase scores only the rows it reranks; and a plan's phases are checked,
//! refused by a reader without the feature, and kept through JSON.

use std::sync::Arc;
use std::time::{Duration, Instant};

use strata::cluster;
use strata::data::{self, col};
use strata::db::Db;
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::ir::{self, Plan, OPERATORS, RANK_EXPRESSIONS, RANK_PHASES, SEQ_NO};
use strata::query::logical::Select;
use strata::query::stats::TermStats;
use strata::query::{compound, Params};
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

const N: usize = 2400;
const DIM: usize = 16;
const SEG: usize = 300;

fn open(dir: &std::path::Path, name: &str, docs: &[strata::Document]) -> Db {
    let opts = DbOptions { segment_max_docs: SEG, ..DbOptions::default() };
    let db = Db::create(&dir.join(name), data::beir_schema(DIM, VectorIndexKind::Flat), opts).unwrap();
    db.load(docs.iter().cloned()).unwrap();
    db
}

/// The corpus, every seventh row without a rating.
fn docs() -> Vec<strata::Document> {
    let mut docs = data::synthetic(N, DIM, 11);
    for d in docs.iter_mut().step_by(7) {
        d[col::RATING] = Value::Null;
    }
    docs
}

/// The one Search a phased query plans into, as the executor runs it.
fn select(db: &Db, sql: &str, params: &Params) -> Select {
    let snap = db.snapshot();
    let plan = compound::plan(sql, snap.schema(), params).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(plan.features.iter().any(|f| f == RANK_PHASES), "{sql}: {:?}", plan.features);
    plan.to_select(snap.schema()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// The coordinator's rounds over `shards`.
fn sharded(shards: &[Db], select: &Select) -> ResultSet {
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
    let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, select).unwrap()).collect();
    let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() };
    let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, select, &opts).unwrap()).collect();
    cluster::merge(select, answers).unwrap()
}

fn close(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

/// The same scores, in order, to 1e-9 relative (NULL as −∞); keys in the
/// same order, except that rows tied on score may swap within their tie,
/// and a tie cut by the page may hold different rows.
fn same(sql: &str, whole: &ResultSet, split: &ResultSet) {
    assert_eq!(whole.keys.len(), split.keys.len(), "{sql}: row counts");
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

fn params(docs: &[strata::Document]) -> Params {
    let title = docs[42][col::TITLE].as_str().unwrap().to_string();
    let text = title.split(' ').take(3).collect::<Vec<_>>().join(" ");
    let other = docs[7][col::TITLE].as_str().unwrap().split(' ').take(2).collect::<Vec<_>>().join(" ");
    Params::new().set("q", docs[42][col::EMB].clone()).set("t", Value::Str(text)).set("u", Value::Str(other))
}

/// The Search of a plan of one table.
fn search(n: &mut ir::Node) -> &mut ir::Search {
    match n {
        ir::Node::Search(s) => s,
        ir::Node::Project { input, .. } | ir::Node::Limit { input, .. } => search(input),
        n => panic!("{n:?}"),
    }
}

/// Phased queries: a first phase, then one or two more.
fn phased() -> Vec<String> {
    let first = [
        "ORDER BY bm25(body, :t) DESC LIMIT 100",
        "ORDER BY similarity(emb, :q) DESC LIMIT 80",
        "WHERE year >= 2005 ORDER BY bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0)) DESC LIMIT 60",
        "ORDER BY 0.3 * minmax(bm25(body, :t)) + 0.7 * minmax(similarity(emb, :q)) DESC LIMIT 90",
        "ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 50",
        "WHERE category IN ('c00', 'c01', 'c02') ORDER BY rating DESC LIMIT 70",
        "ORDER BY year LIMIT 40",
    ];
    let then = [
        // Second phases: each shard scores its own rows.
        "similarity(emb, :q)",
        "bm25(body, :t) + similarity(emb, :q)",
        "ln(1 + bm25(body, :u)) - 0.1 * rating",
        "rating",
        "CASE WHEN category = 'c01' THEN 2 * similarity(emb, :q) ELSE similarity(emb, :q) END",
        // Global phases: one set function over every row reranked.
        "minmax(bm25(body, :u)) + minmax(similarity(emb, :q))",
        "rrf(similarity(emb, :q), rating, k => 10)",
        "-rank(rating) + similarity(emb, :q)",
    ];
    let mut out = Vec::new();
    for (i, f) in first.iter().enumerate() {
        for (j, t) in then.iter().enumerate() {
            let cut = ["LIMIT 10", "LIMIT 15 OFFSET 5", "LIMIT 200"][(i + j) % 3];
            out.push(format!("WITH top AS (SELECT * FROM docs {f}) SELECT id, score() FROM top ORDER BY {t} DESC {cut}"));
        }
    }
    // Chains: a second phase then another second or a global one; a
    // global one then another.
    out.extend([
        "WITH a AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 120), b AS (SELECT * FROM a ORDER BY similarity(emb, :q) DESC LIMIT 40) SELECT id, title, score() FROM b ORDER BY coalesce(rating, 0) + similarity(emb, :q) DESC LIMIT 10".to_string(),
        "WITH a AS (SELECT id FROM docs WHERE year < 2015 ORDER BY similarity(emb, :q) DESC LIMIT 150), b AS (SELECT * FROM a ORDER BY bm25(body, :t) DESC LIMIT 60) SELECT id, score() FROM b ORDER BY minmax(similarity(emb, :q)) + 0.5 * minmax(rating) DESC LIMIT 20".into(),
        "WITH a AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 100), b AS (SELECT * FROM a ORDER BY rank(similarity(emb, :q)) * -1 DESC LIMIT 30) SELECT a.id AS id, score() FROM b a ORDER BY similarity(emb, :q) * a.year DESC LIMIT 10".into(),
        "WITH a AS (SELECT * FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 2000) SELECT id, score() FROM a ORDER BY bm25(body, :t) + similarity(emb, :q) DESC LIMIT 2000".into(),
    ]);
    out
}

/// A phase ranks the rows of the one before as the general plan of the
/// same SQL does (the table restricted to the rows of the `WITH`, which
/// a `WHERE` on the key that every row passes keeps it to), where every
/// row has a value and there are no set functions over rows the general
/// plan would not count; and one database answers like 1, 2 and 4 shards.
#[test]
fn phases_rank_like_the_sql_they_plan_from_and_shards_like_one_database() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let whole = open(dir.path(), "whole.strata", &docs);
    let splits: Vec<Vec<Db>> = [1, 2, 4]
        .iter()
        .map(|&k| (0..k).map(|i| open(dir.path(), &format!("s{k}-{i}.strata"), &docs[i * N / k..(i + 1) * N / k])).collect())
        .collect();
    let params = params(&docs);
    let mut nulls = 0;
    for sql in phased() {
        let s = select(&whole, &sql, &params);
        let want = whole.query(&sql, &params).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(!want.keys.is_empty(), "{sql}");
        same(&format!("{sql} as its plan"), &want, &whole.execute(&whole.snapshot(), &s, &ExecOptions::default()).unwrap());
        nulls += want.scores.iter().filter(|s| **s == f64::NEG_INFINITY).count();
        for shards in &splits {
            same(&format!("{sql} on {} shards", shards.len()), &want, &sharded(shards, &s));
        }
    }
    assert!(nulls > 10, "{nulls}");

    // Against the general plan: a similarity every row has.
    for (first, then) in [
        ("ORDER BY bm25(body, :t) DESC LIMIT 100", "similarity(emb, :q)"),
        ("WHERE year >= 2005 ORDER BY rating DESC LIMIT 60", "similarity(emb, :q) * 2 + year / 1000"),
        ("ORDER BY similarity(emb, :q) DESC LIMIT 80", "similarity(emb, :q) - 0.01 * year"),
    ] {
        let sql = format!("WITH top AS (SELECT * FROM docs {first}) SELECT id, score() FROM top ORDER BY {then} DESC LIMIT 25");
        let general = sql.replace("FROM top ORDER", "FROM top WHERE id IS NOT NULL ORDER");
        let plan = whole.explain(&general, &params).unwrap();
        assert!(!plan.features.iter().any(|f| f == RANK_PHASES), "{general}");
        same(&sql, &whole.query(&general, &params).unwrap(), &whole.query(&sql, &params).unwrap());
    }
}

/// Every row a phase reads goes on, one it gives no value last with a
/// NULL score (and one a text phase's terms miss with BM25 0); rows tied
/// on a phase keep the order the phase before gave them.
#[test]
fn a_phase_keeps_every_row_and_the_order_of_ties() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "t.strata", &docs);
    let params = params(&docs);
    let before = db.query("SELECT id, score() FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 70", &params).unwrap();
    let r = db.query("WITH top AS (SELECT * FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 70) SELECT id, score() FROM top ORDER BY rating DESC LIMIT 100", &params).unwrap();
    assert_eq!(r.keys.len(), 70);
    let mut a: Vec<String> = before.keys.iter().map(|k| format!("{k:?}")).collect();
    let mut b: Vec<String> = r.keys.iter().map(|k| format!("{k:?}")).collect();
    a.sort();
    b.sort();
    assert_eq!(a, b, "the same rows");
    let first_null = r.rows.iter().position(|row| row[1] == Value::Null).expect("a row without a rating");
    assert!(r.rows[first_null..].iter().all(|row| row[1] == Value::Null));
    assert!(r.scores[..first_null].windows(2).all(|w| w[0] >= w[1]));
    // Rows tied on a rating (and the NULLs) in the order of similarity.
    let at = |k: &Value| before.keys.iter().position(|x| x == k).unwrap();
    for i in 1..r.keys.len() {
        if r.scores[i] == r.scores[i - 1] {
            assert!(at(&r.keys[i - 1]) < at(&r.keys[i]), "row {i}");
        }
    }
    // A text phase: rows its terms miss stay, BM25 0, after those it
    // matches.
    let r = db.query("WITH top AS (SELECT * FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 70) SELECT id, score() FROM top ORDER BY bm25(body, :u) DESC LIMIT 100", &params).unwrap();
    assert_eq!(r.keys.len(), 70);
    let missed = r.scores.iter().position(|s| *s == 0.0).expect("a row the terms miss");
    assert!(missed > 0 && r.scores[missed..].iter().all(|s| *s == 0.0), "{:?}", r.scores);
}

/// A phase scores the rows it reranks and no others: the first phase's
/// cut, then each phase's.
#[test]
fn a_phase_scores_only_the_rows_it_reranks() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "c.strata", &docs);
    let params = params(&docs);
    let sql = "WITH a AS (SELECT * FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 90), b AS (SELECT * FROM a ORDER BY rating DESC LIMIT 30) SELECT id, score() FROM b ORDER BY bm25(body, :t) + similarity(emb, :q) DESC LIMIT 5";
    let r = db.query(sql, &params).unwrap();
    assert_eq!(r.keys.len(), 5);
    assert_eq!(r.stats.reranked, 90 + 30);
    let plan = r.stats.plan.join("\n");
    assert!(plan.contains("rerank 90 rows by") && plan.contains("rerank 30 rows by"), "{plan}");
    // Fewer rows than the cut: each phase reranks what there is.
    let small = open(dir.path(), "small.strata", &docs[..20]);
    let r = small.query(sql, &params).unwrap();
    assert_eq!(r.stats.reranked, 20 + 20);
}

/// The first phase's cut counts against the rows a query may reach, and
/// the phases against its deadline.
#[test]
fn caps_and_deadlines_hold() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "d.strata", &docs);
    let params = params(&docs);
    let s = select(&db, "WITH top AS (SELECT * FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 500) SELECT id FROM top ORDER BY rating DESC LIMIT 5", &params);
    let opts = ExecOptions { max_rows: Some(100), ..ExecOptions::default() };
    let e = db.execute(&db.snapshot(), &s, &opts).unwrap_err().to_string();
    assert!(e.contains("the first rerank phase takes the top 500 rows; a query reaches at most 100"), "{e}");
    let opts = ExecOptions { deadline: Some(Instant::now() - Duration::from_millis(1)), ..ExecOptions::default() };
    let e = db.execute(&db.snapshot(), &s, &opts).unwrap_err();
    assert!(matches!(e, strata::Error::Timeout(_)), "{e}");
}

/// A plan's phases go through JSON and back unchanged and run as their SQL;
/// a reader without the `rank_phases` feature refuses them by name; and a
/// plan from elsewhere is checked.
#[test]
fn plans_keep_check_and_name_their_phases() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "p.strata", &docs);
    let params = params(&docs);
    let snap = db.snapshot();
    let schema = snap.schema();
    for sql in phased().iter().step_by(5) {
        let plan = db.explain(sql, &params).unwrap();
        let back = Plan::from_json(&plan.to_json().unwrap()).unwrap();
        assert_eq!(plan, back, "{sql}");
        let features: Vec<&str> = back.features.iter().map(String::as_str).collect();
        let e = Plan::readable_by(back.version as u64, &features, &[OPERATORS, SEQ_NO, RANK_EXPRESSIONS]).unwrap_err().to_string();
        assert!(e.contains("rank_phases"), "{sql}: {e}");
        same(sql, &db.query(sql, &params).unwrap(), &db.run_at(&snap, &back, &ExecOptions::default()).unwrap());
        let text = back.explain(&[schema]).join("\n");
        assert!(text.contains("rerank: "), "{sql}: {text}");
    }
    // EXPLAIN says which phases each shard scores and which run once.
    let plan = db.explain("WITH a AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 100), b AS (SELECT * FROM a ORDER BY rating DESC LIMIT 40) SELECT id FROM b ORDER BY minmax(similarity(emb, :q)) DESC LIMIT 10", &params).unwrap();
    let text = plan.explain(&[schema]).join("\n");
    assert!(text.contains("over the top 100 (a second phase: each shard scores its rows)"), "{text}");
    assert!(text.contains("over the top 40 (a global phase: scored once over every row)"), "{text}");

    // A plan from elsewhere is checked.
    let base = db.explain("WITH a AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 100) SELECT id FROM a ORDER BY rating DESC LIMIT 10", &params).unwrap();
    let edit = |f: &dyn Fn(&mut Vec<ir::Phase>, &mut ir::Search)| {
        let mut p = base.clone();
        let s = search(&mut p.root);
        let mut phases = std::mem::take(&mut s.phases);
        f(&mut phases, s);
        s.phases = phases;
        p.to_select(schema).map(|_| ()).map_err(|e| e.to_string())
    };
    assert_eq!(edit(&|_, _| {}), Ok(()));
    let mut minmax = db.explain("SELECT id FROM docs ORDER BY minmax(rating) DESC LIMIT 3", &params).unwrap();
    let set_fn = search(&mut minmax.root).order[0].expr.clone();
    let cases: Vec<(Box<dyn Fn(&mut Vec<ir::Phase>, &mut ir::Search)>, &str)> = vec![
        (Box::new(|p, _| p[0].count = 0), "a rerank phase reranks at least one row"),
        (Box::new(|_, s| s.order.clear()), "rerank phases follow a Search's order"),
        (Box::new(move |p, _| p[0].expr = set_fn.clone()), "it is global"),
        (
            Box::new(|p, _| {
                let mut g = p[0].clone();
                g.global = true;
                p.insert(0, g);
            }),
            "a rerank phase after a global one is global",
        ),
    ];
    for (f, want) in cases {
        let e = edit(&*f).unwrap_err();
        assert!(e.contains(want), "{want}: {e}");
    }
}

/// Queries the WITH form does not cover plan as before: a filter, grouping
/// or a join after the first `WITH`, a second ranking key, an ascending
/// ranking, a `WITH` without a cut, and columns renamed.
#[test]
fn other_with_queries_plan_as_before() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "o.strata", &docs);
    let params = params(&docs);
    for sql in [
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50) SELECT id FROM top WHERE year > 2000 ORDER BY rating DESC LIMIT 5",
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50) SELECT category, COUNT(*) FROM top GROUP BY category",
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50) SELECT id FROM top ORDER BY rating DESC, id LIMIT 5",
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50) SELECT id FROM top ORDER BY rating LIMIT 5",
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC) SELECT id FROM top ORDER BY rating DESC LIMIT 5",
        "WITH top AS (SELECT * FROM docs WHERE year > 2000 LIMIT 50) SELECT id FROM top ORDER BY rating DESC LIMIT 5",
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50 OFFSET 5) SELECT id FROM top ORDER BY rating DESC LIMIT 5",
        "WITH top AS (SELECT id, year AS rating FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50) SELECT id FROM top ORDER BY rating DESC LIMIT 5",
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50) SELECT t.id FROM top t JOIN docs d ON t.id = d.id ORDER BY t.rating DESC LIMIT 5",
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50) SELECT id FROM top ORDER BY distance(emb, :q) LIMIT 5",
    ] {
        let plan = db.explain(sql, &params).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(!plan.features.iter().any(|f| f == RANK_PHASES), "{sql}");
        db.query(sql, &params).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
