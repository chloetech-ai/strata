//! Ranking features: a ranking expression that scores each row by itself
//! is a column (`SELECT bm25(title, :q) AS t`), and `features()` lists the
//! inputs of the query's ranking (its leaves, then each phase's expression
//! that scores a row by itself), each named by its SQL text. A feature is
//! the value the ranking computes for the row: one database and the same
//! rows split over 1, 2 and 4 shards give the same values, in one round and
//! in query then fetch, which computes them for the page alone.

use std::collections::HashMap;
use std::sync::Arc;

use strata::cluster;
use strata::data::{self, col};
use strata::db::Db;
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::ir::{Plan, OPERATORS, RANK_EXPRESSIONS, RANK_FEATURES, RANK_PHASES, SEQ_NO};
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

fn params(docs: &[strata::Document]) -> Params {
    let title = docs[42][col::TITLE].as_str().unwrap().to_string();
    let text = title.split(' ').take(3).collect::<Vec<_>>().join(" ");
    let other = docs[7][col::TITLE].as_str().unwrap().split(' ').take(2).collect::<Vec<_>>().join(" ");
    Params::new().set("q", docs[42][col::EMB].clone()).set("t", Value::Str(text)).set("u", Value::Str(other))
}

/// The one Search `sql` plans into, as the executor runs it.
fn select(db: &Db, sql: &str, params: &Params) -> Select {
    let snap = db.snapshot();
    let plan = compound::plan(sql, snap.schema(), params).unwrap_or_else(|e| panic!("{sql}: {e}"));
    plan.to_select(snap.schema()).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Shard-mode options with the BM25 statistics of `select` summed over
/// `shards`, as a coordinator gathers them.
fn summed(shards: &[Db], select: &Select) -> ExecOptions {
    let parts: Vec<TermStats> = shards.iter().map(|d| d.term_stats(&d.snapshot(), select).unwrap()).collect();
    ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() }
}

/// The coordinator's rounds over `shards`, every column in one round.
fn sharded(shards: &[Db], select: &Select) -> ResultSet {
    let opts = summed(shards, select);
    let answers = shards.iter().map(|d| d.execute(&d.snapshot(), select, &opts).unwrap()).collect();
    cluster::merge(select, answers).unwrap()
}

/// Query then fetch over `shards`: the keys first, then the page's
/// columns from the shards that hold its rows, with `fetch` options.
fn fetched(shards: &[Db], select: &Select, fetch: &ExecOptions) -> ResultSet {
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
    let schema = snaps[0].schema();
    assert!(cluster::fetches(select, schema));
    let opts = summed(shards, select);
    let ids = cluster::ids(select, schema).unwrap();
    let answers: Vec<ResultSet> = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &ids, &opts).unwrap()).collect();
    let owner: HashMap<String, usize> = answers.iter().enumerate().flat_map(|(i, a)| a.keys.iter().map(move |k| (format!("{k:?}"), i))).collect();
    let ranked = cluster::merge(&ids, answers).unwrap();
    let mut keys = vec![Vec::new(); shards.len()];
    for k in &ranked.keys {
        keys[owner[&format!("{k:?}")]].push(k.clone());
    }
    let rows = shards.iter().zip(&snaps).zip(keys).map(|((d, s), k)| d.execute(s, &cluster::fetch(select, schema, k).unwrap(), fetch).unwrap()).collect();
    let (rs, lost) = cluster::fill(select, ranked, rows).unwrap();
    assert_eq!(lost, 0);
    rs
}

fn close(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => close(*x, *y),
        _ => a == b,
    }
}

/// The same rows in the same order (rows tied on score may swap, and a
/// tie cut by the page may hold others), and each row present in both the
/// same in every column, floats to 1e-9 relative.
fn same(sql: &str, whole: &ResultSet, split: &ResultSet) {
    assert_eq!(whole.columns, split.columns, "{sql}");
    assert_eq!(whole.keys.len(), split.keys.len(), "{sql}: row counts");
    for (i, (a, b)) in whole.scores.iter().zip(&split.scores).enumerate() {
        assert!(close(*a, *b), "{sql}: row {i}: {a} vs {b}");
    }
    let at: HashMap<String, usize> = split.keys.iter().enumerate().map(|(i, k)| (format!("{k:?}"), i)).collect();
    let mut found = 0;
    for (i, k) in whole.keys.iter().enumerate() {
        let Some(&j) = at.get(&format!("{k:?}")) else { continue };
        found += 1;
        for (c, (a, b)) in whole.rows[i].iter().zip(&split.rows[j]).enumerate() {
            assert!(same_value(a, b), "{sql}: {k:?}, {}: {a:?} vs {b:?}", whole.columns[c]);
        }
    }
    assert!(found * 10 >= whole.keys.len() * 9, "{sql}: {found} of {} rows in both", whole.keys.len());
}

/// A column's values, by name.
fn column<'r>(r: &'r ResultSet, name: &str) -> Vec<&'r Value> {
    let c = r.columns.iter().position(|n| n == name).unwrap_or_else(|| panic!("no column {name}: {:?}", r.columns));
    r.rows.iter().map(|row| &row[c]).collect()
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

#[test]
fn features_name_the_inputs_of_the_ranking() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "n.strata", &docs);
    let params = params(&docs);
    for (sql, want) in [
        ("SELECT id, features() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 5", vec!["id", "bm25(body, :t)"]),
        // Leaves first, then the expression; each once.
        (
            "SELECT features(), id FROM docs ORDER BY bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0)) + 0.1 * bm25(body, :t) DESC LIMIT 5",
            vec!["bm25(body, :t)", "rating", "bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0)) + 0.1 * bm25(body, :t)", "id"],
        ),
        // A set function ranks rows against each other: its inputs only.
        ("SELECT id, features() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 5", vec!["id", "bm25(body, :t)", "similarity(emb, :q)"]),
        ("SELECT id, features() FROM docs ORDER BY 0.3 * minmax(bm25(body, :t)) + 0.7 * minmax(similarity(emb, :q)) DESC LIMIT 5", vec!["id", "bm25(body, :t)", "similarity(emb, :q)"]),
        ("SELECT id, features() FROM docs ORDER BY similarity(emb, :q) - rank(rating) DESC LIMIT 5", vec!["id", "similarity(emb, :q)", "rating"]),
        // A CASE: its branches' leaves, then the whole.
        (
            "SELECT id, features() FROM docs ORDER BY CASE WHEN category = 'c01' THEN 2 * similarity(emb, :q) ELSE decay(year, 2010, 5) END DESC LIMIT 5",
            vec!["id", "similarity(emb, :q)", "decay(year, 2010, 5)", "CASE WHEN category = 'c01' THEN 2 * similarity(emb, :q) ELSE decay(year, 2010, 5) END"],
        ),
        ("SELECT id, features() FROM docs ORDER BY distance(emb, :q) LIMIT 5", vec!["id", "similarity(emb, :q)"]),
        ("SELECT id, features() FROM docs WHERE year > 2000 ORDER BY rating DESC LIMIT 5", vec!["id", "rating"]),
        // By position.
        ("SELECT id, ln(1 + bm25(body, :t)) * 2, features() FROM docs ORDER BY 2 DESC LIMIT 5", vec!["id", "ln(1 + bm25(body, :t)) * 2", "bm25(body, :t)", "ln(1 + bm25(body, :t)) * 2"]),
        // Every phase's ranking, first to last; a global phase's inputs.
        (
            "WITH a AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 100), b AS (SELECT * FROM a ORDER BY similarity(emb, :q) + 0.1 * rating DESC LIMIT 40) SELECT id, features() FROM b ORDER BY minmax(bm25(body, :u)) + minmax(year) DESC LIMIT 10",
            vec!["id", "bm25(body, :t)", "similarity(emb, :q)", "rating", "bm25(body, :u)", "year", "similarity(emb, :q) + 0.1 * rating"],
        ),
        (
            "WITH c AS (SELECT * FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 50) SELECT c.id, features() FROM c ORDER BY ln(1 + bm25(body, :u)) - 0.1 * c.rating DESC LIMIT 5",
            vec!["id", "bm25(body, :t)", "similarity(emb, :q)", "bm25(body, :u)", "rating", "ln(1 + bm25(body, :u)) - 0.1 * rating"],
        ),
    ] {
        let r = db.query(sql, &params).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(r.columns, want, "{sql}");
        assert_eq!(r.rows.len(), r.keys.len(), "{sql}");
        if sql.starts_with("WITH") {
            assert!(db.explain(sql, &params).unwrap().features.iter().any(|f| f == RANK_PHASES), "{sql}: plans as phases");
        }
    }
}

/// A feature is the value the ranking computes: an expression that ranks
/// is its row's score, and each leaf is its own ranking's score of the row.
#[test]
fn features_are_the_values_ranked_by() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "v.strata", &docs);
    let params = params(&docs);
    // Each leaf's score of every row it ranks.
    let ranked = |order: &str| -> HashMap<String, f64> {
        let r = db.query(&format!("SELECT id, score() FROM docs ORDER BY {order} DESC LIMIT {N}"), &params).unwrap();
        r.keys.iter().zip(&r.scores).filter(|(_, s)| s.is_finite()).map(|(k, s)| (format!("{k:?}"), *s)).collect()
    };
    let leaves: Vec<(&str, HashMap<String, f64>)> =
        ["bm25(body, :t)", "bm25(body, :u)", "similarity(emb, :q)", "decay(year, 2010, 5)"].into_iter().map(|l| (l, ranked(l))).collect();
    let rating: HashMap<String, Option<f64>> = docs.iter().map(|d| (format!("{:?}", d[col::ID]), d[col::RATING].as_f64())).collect();
    let mut checked = 0;
    for (order, formula) in [
        ("bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0))", &(|f: &dyn Fn(&str) -> Option<f64>| Some(f("bm25(body, :t)")? * (1.0 + 0.2 * f("rating").unwrap_or(0.0)))) as &dyn Fn(&dyn Fn(&str) -> Option<f64>) -> Option<f64>),
        ("ln(1 + bm25(body, :u)) - 0.1 * rating", &|f| Some((1.0 + f("bm25(body, :u)")?).ln() - 0.1 * f("rating")?)),
        ("similarity(emb, :q) + 0.5 * decay(year, 2010, 5)", &|f| Some(f("similarity(emb, :q)")? + 0.5 * f("decay(year, 2010, 5)")?)),
        ("rrf(bm25(body, :t), similarity(emb, :q))", &|_| None),
        ("0.3 * minmax(bm25(body, :t)) + 0.7 * minmax(similarity(emb, :q))", &|_| None),
    ] {
        for filter in ["", "WHERE year >= 2005"] {
            let sql = format!("SELECT id, score(), features() FROM docs {filter} ORDER BY {order} DESC LIMIT 300");
            let r = db.query(&sql, &params).unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert!(r.rows.len() > 100, "{sql}");
            let row_local = r.columns.last().unwrap() == order;
            for (i, row) in r.rows.iter().enumerate() {
                let key = format!("{:?}", r.keys[i]);
                let f = |name: &str| num(&row[r.columns.iter().position(|c| c == name).unwrap()]);
                if row_local {
                    // The expression is the row's score, NULL alike.
                    assert!(same_value(row.last().unwrap(), &row[1]), "{sql}: row {i}: {:?} vs {:?}", row.last(), row[1]);
                }
                if let Some(want) = formula(&f) {
                    assert!(close(want, r.scores[i]), "{sql}: row {i}: {want} vs {}", r.scores[i]);
                }
                for (name, scores) in &leaves {
                    let Some(c) = r.columns.iter().position(|n| n == name) else { continue };
                    match (scores.get(&key), num(&row[c])) {
                        (Some(s), Some(v)) => assert!(close(*s, v), "{sql}: {key} {name}: {v} vs {s}"),
                        // A row a text leaf's terms miss scores 0.
                        (None, Some(v)) => assert!(name.starts_with("bm25") && v == 0.0, "{sql}: {key} {name}: {v}"),
                        (_, None) => panic!("{sql}: {key} {name}: NULL"),
                    }
                    checked += 1;
                }
                if let Some(c) = r.columns.iter().position(|n| n == "rating") {
                    assert_eq!(num(&row[c]), rating[&key], "{sql}: {key}");
                }
            }
        }
    }
    assert!(checked > 2000, "{checked}");

    // Phases: the last one's expression is its score.
    let sql = "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 200) SELECT id, score(), features() FROM top ORDER BY similarity(emb, :q) * (2 + 0.1 * rating) DESC LIMIT 200";
    let r = db.query(sql, &params).unwrap();
    assert_eq!(r.rows.len(), 200);
    assert_eq!(r.columns.last().unwrap(), "similarity(emb, :q) * (2 + 0.1 * rating)");
    let nulls = r.rows.iter().filter(|row| row[1] == Value::Null).count();
    assert!(nulls > 10, "{nulls}");
    for row in &r.rows {
        assert!(same_value(row.last().unwrap(), &row[1]), "{sql}: {row:?}");
    }
}

/// One database answers like 1, 2 and 4 shards, features and selected
/// expressions included: in one round, and in query then fetch, whose
/// fetch scores BM25 with the query's statistics.
#[test]
fn features_over_shards_are_one_databases() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let whole = open(dir.path(), "whole.strata", &docs);
    let splits: Vec<Vec<Db>> = [1, 2, 4]
        .iter()
        .map(|&k| (0..k).map(|i| open(dir.path(), &format!("s{k}-{i}.strata"), &docs[i * N / k..(i + 1) * N / k])).collect())
        .collect();
    let params = params(&docs);
    let queries = [
        "SELECT id, score(), features() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 20",
        "SELECT id, features(), bm25(body, :u) AS other FROM docs WHERE year >= 2005 ORDER BY bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0)) DESC LIMIT 15 OFFSET 5",
        "SELECT id, score(), features() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 25",
        "SELECT id, features() FROM docs ORDER BY 0.3 * minmax(bm25(body, :t)) + 0.7 * minmax(similarity(emb, :q)) DESC LIMIT 10",
        "SELECT id, bm25(body, :t) AS b, ln(1 + bm25(body, :u)) - 0.1 * rating AS x, year / 1000 FROM docs ORDER BY rating DESC LIMIT 30",
        "SELECT id, title, features() FROM docs ORDER BY similarity(emb, :q) + 0.5 * decay(year, 2010, 5) DESC LIMIT 40",
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 100) SELECT id, score(), features() FROM top ORDER BY ln(1 + bm25(body, :u)) - 0.1 * rating DESC LIMIT 20",
        "WITH a AS (SELECT * FROM docs ORDER BY similarity(emb, :q) DESC LIMIT 150), b AS (SELECT * FROM a ORDER BY bm25(body, :t) DESC LIMIT 60) SELECT id, features() FROM b ORDER BY minmax(similarity(emb, :q)) + 0.5 * minmax(rating) DESC LIMIT 20",
    ];
    let mut bm25_differs = 0;
    for sql in queries {
        let want = whole.query(sql, &params).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(!want.keys.is_empty(), "{sql}");
        let s = select(&whole, sql, &params);
        same(&format!("{sql} as its plan"), &want, &whole.execute(&whole.snapshot(), &s, &ExecOptions::default()).unwrap());
        for shards in &splits {
            let at = |how: &str| format!("{sql} on {} shards, {how}", shards.len());
            same(&at("one round"), &want, &sharded(shards, &s));
            same(&at("query then fetch"), &want, &fetched(shards, &s, &summed(shards, &s)));
            // Without the statistics, a fetch scores BM25 with each
            // shard's own counts.
            if shards.len() > 1 && sql.contains("bm25") {
                let local = fetched(shards, &s, &ExecOptions { shard: true, ..ExecOptions::default() });
                bm25_differs += (want.rows != local.rows) as usize;
            }
        }
    }
    assert!(bm25_differs >= 8, "{bm25_differs}");
}

/// Any ranking expression that scores rows by itself is a column; one that
/// ranks rows against each other, or a ranking of groups, is not.
#[test]
fn ranking_expressions_are_columns() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "c.strata", &docs);
    let params = params(&docs);
    let r = db.query("SELECT id, rating, bm25(body, :t) AS b, ln(1 + rating) AS r, (year - 2000) * 2 AS y FROM docs ORDER BY rating DESC NULLS FIRST LIMIT 500", &params).unwrap();
    assert_eq!(r.columns, ["id", "rating", "b", "r", "y"]);
    let (mut nulls, mut zeros) = (0, 0);
    for row in &r.rows {
        match num(&row[1]) {
            Some(x) => assert!(close(num(&row[3]).unwrap(), (1.0 + x).ln()), "{row:?}"),
            None => {
                assert_eq!(row[3], Value::Null, "{row:?}");
                nulls += 1;
            }
        }
        zeros += (num(&row[2]) == Some(0.0)) as usize;
    }
    assert!(nulls > 10 && zeros > 100, "{nulls} {zeros}");
    let years = column(&r, "y");
    assert!(years.iter().all(|y| num(y).is_some_and(|y| y.fract() == 0.0)), "{years:?}");
    // Without a ranking's value, the row evaluator computes the columns,
    // typed: text functions too.
    let r = db.query("SELECT id, lower(title), (year - 2000) * 2 AS y FROM docs ORDER BY rating DESC LIMIT 5", &params).unwrap();
    assert!(r.rows.iter().all(|row| matches!(row[1], Value::Str(_)) && matches!(row[2], Value::Int(_))), "{:?}", r.rows);

    // Ordered by a selected expression, by position.
    let r = db.query("SELECT id, score(), bm25(body, :t) * 2 + 1 FROM docs ORDER BY 3 DESC LIMIT 10", &params).unwrap();
    assert!(r.rows.iter().all(|row| same_value(&row[1], &row[2])), "{:?}", r.rows);

    for (sql, want) in [
        ("SELECT id, minmax(rating) FROM docs ORDER BY rating DESC LIMIT 5", "ranks rows against each other"),
        ("SELECT id, rrf(bm25(body, :t), similarity(emb, :q)) FROM docs ORDER BY rating DESC LIMIT 5", "ranks rows against each other"),
        ("SELECT id, 0.5 * minmax(bm25(body, :t)) + 0.5 * minmax(rating) AS f FROM docs ORDER BY rating DESC LIMIT 5", "ranks rows against each other"),
        ("SELECT id, features() FROM docs LIMIT 5", "it has no ORDER BY"),
        ("SELECT id, features() AS f FROM docs ORDER BY rating DESC LIMIT 5", "takes no alias"),
        ("SELECT id, features() + 1 FROM docs ORDER BY rating DESC LIMIT 5", "is not part of an expression"),
        ("SELECT id, features(rating) FROM docs ORDER BY rating DESC LIMIT 5", "takes no arguments"),
        ("SELECT id, features() FROM docs ORDER BY 2 DESC LIMIT 5", "is not one column"),
        ("SELECT category, COUNT(*), features() FROM docs GROUP BY category ORDER BY COUNT(*) DESC", "not of groups"),
        ("SELECT category, bm25(body, :t) FROM docs GROUP BY category", "scores rows, not groups"),
        ("SELECT id, bm25(year, :t) FROM docs LIMIT 5", "TEXT"),
    ] {
        let e = db.query(sql, &params).map(|r| r.columns).unwrap_err().to_string();
        assert!(e.contains(want), "{sql}: {e}");
    }
}

/// A plan with a feature column says so, keeps it through JSON and runs as
/// its SQL; a reader without the feature refuses it, and raising a plan
/// refuses a column that ranks rows against each other.
#[test]
fn plans_carry_features() {
    let docs = docs();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), "p.strata", &docs);
    let params = params(&docs);
    let snap = db.snapshot();
    let schema = snap.schema();
    for sql in [
        "SELECT id, features() FROM docs ORDER BY bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0)) DESC LIMIT 10",
        "SELECT id, bm25(body, :t) AS b FROM docs ORDER BY rating DESC LIMIT 10",
        "SELECT id, features() FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 10",
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 100) SELECT id, features() FROM top ORDER BY similarity(emb, :q) DESC LIMIT 10",
    ] {
        let plan = db.explain(sql, &params).unwrap();
        assert!(plan.features.iter().any(|f| f == RANK_FEATURES), "{sql}: {:?}", plan.features);
        let back = Plan::from_json(&plan.to_json().unwrap()).unwrap();
        assert_eq!(plan, back, "{sql}");
        let features: Vec<&str> = back.features.iter().map(String::as_str).collect();
        let e = Plan::readable_by(back.version as u64, &features, &[OPERATORS, SEQ_NO, RANK_EXPRESSIONS, RANK_PHASES]).unwrap_err().to_string();
        assert!(e.contains("rank_features"), "{sql}: {e}");
        same(sql, &db.query(sql, &params).unwrap(), &db.run_at(&snap, &back, &ExecOptions::default()).unwrap());
        let text = back.explain(&[schema]).join("\n");
        assert!(text.contains("Project: ") && text.contains("bm25(body"), "{sql}: {text}");
    }
    for sql in ["SELECT id, score() FROM docs ORDER BY bm25(body, :t) DESC LIMIT 10", "SELECT id, rating, features() FROM docs ORDER BY rating DESC LIMIT 10"] {
        assert!(!db.explain(sql, &params).unwrap().features.iter().any(|f| f == RANK_FEATURES), "{sql}");
    }
    // A plan from elsewhere is checked.
    let mut plan = db.explain("SELECT id, bm25(body, :t) AS b FROM docs ORDER BY rating DESC LIMIT 10", &params).unwrap();
    let set_fn = db.explain("SELECT id FROM docs ORDER BY minmax(rating) DESC LIMIT 3", &params).unwrap();
    let strata::query::ir::Node::Project { input, .. } = &set_fn.root else { panic!() };
    let strata::query::ir::Node::Limit { input, .. } = &**input else { panic!() };
    let strata::query::ir::Node::Search(s) = &**input else { panic!() };
    let strata::query::ir::Node::Project { columns, .. } = &mut plan.root else { panic!() };
    columns[1].expr = s.order[0].expr.clone();
    let e = plan.to_select(schema).unwrap_err().to_string();
    assert!(e.contains("rank rows against each other"), "{e}");
}
