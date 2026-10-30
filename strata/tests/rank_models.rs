//! Ranking models: `gbdt('name', features...)` scores rows as LightGBM
//! and XGBoost themselves do (tests/models, made by make.py there); a
//! query ranks by it exactly, in rerank phases and in subqueries; a plan
//! names the file of its model, and a database whose file differs refuses
//! the plan.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::cluster;
use strata::data::{self, col};
use strata::db::{Db, SqlResult};
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::ir::{Plan, RANK_MODELS};
use strata::query::model::{Gbdt, Models};
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

const MODELS: [&str; 6] = ["lgb_rank", "lgb_zero", "lgb_rf", "xgb_rank", "xgb_logistic", "xgb_reg"];

fn models_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models")
}

fn model(name: &str) -> Gbdt {
    Gbdt::parse(name, &std::fs::read(models_dir().join(name).join("gbdt.json")).unwrap()).unwrap()
}

/// A database of synthetic documents, some without a rating, whose models
/// directory is `models`.
fn create(path: &Path, models: &Path) -> (Db, Params) {
    let db = Db::create(path, data::beir_schema(16, VectorIndexKind::Flat), DbOptions { segment_max_docs: 200, memtable_max_docs: 1_000_000, ..DbOptions::default() }).unwrap();
    // Where its models are; the embedding model of `emb` is not there,
    // and the documents come with their vectors.
    db.open_embedders(models);
    let mut docs = data::synthetic(800, 16, 5);
    for d in docs.iter_mut().step_by(7) {
        d[col::RATING] = Value::Null;
    }
    db.load(docs.iter().cloned()).unwrap();
    let words = docs[42][col::TITLE].as_str().unwrap().split(' ').take(2).collect::<Vec<_>>().join(" ");
    (db, Params::new().set("t", Value::Str(words)))
}

fn query(db: &Db, text: &str, params: &Params) -> ResultSet {
    db.query(text, params).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn refused(db: &Db, text: &str, params: &Params, says: &str) {
    let e = db.query(text, params).err().unwrap_or_else(|| panic!("{text}: not refused")).to_string();
    assert!(e.contains(says), "{text}: {e}");
}

/// A number, NULL as NaN.
fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

/// The ranking by model `name` of the three features.
fn ranking(name: &str) -> String {
    format!("gbdt('{name}', bm25(body, :t), rating, year % 6)")
}

/// The reference rows, NULL as NaN, and each model's predictions of them.
fn reference() -> (Vec<Vec<f64>>, serde_json::Map<String, serde_json::Value>) {
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(models_dir().join("reference.json")).unwrap()).unwrap();
    let rows = j["rows"].as_array().unwrap().iter().map(|r| r.as_array().unwrap().iter().map(|v| v.as_f64().unwrap_or(f64::NAN)).collect()).collect();
    (rows, j["predictions"].as_object().unwrap().clone())
}

#[test]
fn predictions_equal_lightgbm_and_xgboost() {
    let (rows, predictions) = reference();
    let models = Models::default();
    assert_eq!(predictions.len(), 6);
    for (name, want) in &predictions {
        let m = models.gbdt(&models_dir(), name).unwrap();
        assert_eq!(m.features, 3, "{name}");
        let want: Vec<f64> = want.as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
        let mut differ = 0;
        for (r, w) in rows.iter().zip(&want) {
            let got = m.predict(r);
            if got.to_bits() != w.to_bits() {
                differ += 1;
                eprintln!("{name}: row {r:?} gives {got}, the library {w}");
            }
        }
        assert_eq!(differ, 0, "{name}: {differ} of {} rows differ", rows.len());
        // Bounds hold each row, from the row's own box and from all rows'.
        let all: Vec<(f64, f64, bool)> = (0..3)
            .map(|f| {
                let v: Vec<f64> = rows.iter().map(|r| r[f]).filter(|x| !x.is_nan()).collect();
                (v.iter().copied().fold(f64::INFINITY, f64::min), v.iter().copied().fold(f64::NEG_INFINITY, f64::max), rows.iter().any(|r| r[f].is_nan()))
            })
            .collect();
        let (lo, hi) = m.bounds(&all);
        for (r, &w) in rows.iter().zip(&want) {
            assert!(w >= lo && w <= hi, "{name}: {w} outside [{lo}, {hi}]");
            let own: Vec<(f64, f64, bool)> = r.iter().map(|&x| if x.is_nan() { (1.0, 0.0, true) } else { (x, x, false) }).collect();
            let (a, b) = m.bounds(&own);
            assert!(w >= a && w <= b, "{name}: one row's box [{a}, {b}] for {w}");
        }
    }
}

#[test]
fn a_changed_file_is_read_again() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("m")).unwrap();
    let copy = |from: &str| std::fs::copy(models_dir().join(from).join("gbdt.json"), dir.path().join("m/gbdt.json")).unwrap();
    copy("lgb_rank");
    let models = Models::default();
    let a = models.gbdt(dir.path(), "m").unwrap();
    assert!(std::sync::Arc::ptr_eq(&a, &models.gbdt(dir.path(), "m").unwrap()), "opened once");
    copy("xgb_rank");
    let b = models.gbdt(dir.path(), "m").unwrap();
    assert_ne!(a.hash, b.hash);
    let text = std::fs::read(models_dir().join("xgb_rank/gbdt.json")).unwrap();
    assert_eq!(Gbdt::parse("m", &text).unwrap().hash, b.hash);
}

#[test]
fn a_query_ranks_by_what_the_model_predicts() {
    let dir = tempfile::tempdir().unwrap();
    let (db, params) = create(&dir.path().join("db.strata"), &models_dir());
    for name in MODELS {
        let m = model(name);
        let by = ranking(name);
        // Every row BM25 finds, each with its features and the model's
        // score of them.
        let sql = format!("SELECT id, features() FROM docs ORDER BY {by} DESC, id LIMIT 1000");
        let all = query(&db, &sql, &params);
        // The leaves of the ranking, then the model's score.
        assert_eq!(all.columns, ["id", "bm25(body, :t)", "rating", "year", by.as_str()], "{name}");
        assert!(all.rows.len() > 30 && all.rows.len() < 800, "{name}: {} rows", all.rows.len());
        assert!(all.rows.iter().any(|r| r[2] == Value::Null), "{name}: a row without a rating");
        let mut want: Vec<(f64, String)> = Vec::new();
        for (r, &score) in all.rows.iter().zip(&all.scores) {
            let x = [num(&r[1]), num(&r[2]), num(&r[3]) % 6.0];
            let p = m.predict(&x);
            assert_eq!(p.to_bits(), score.to_bits(), "{name}: {r:?} scores {score}, the model {p}");
            assert_eq!(p.to_bits(), num(&r[4]).to_bits(), "{name}: the column of {r:?}");
            want.push((p, r[0].as_str().unwrap().to_string()));
        }
        want.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let ids: Vec<&str> = all.rows.iter().map(|r| r[0].as_str().unwrap()).collect();
        assert_eq!(ids, want.iter().map(|(_, id)| id.as_str()).collect::<Vec<_>>(), "{name}: order");
        // The top 10 alone is the same, found from the model's bounds
        // without scoring every row; and through a function of it.
        for (expr, f) in [(by.clone(), (|x| x) as fn(f64) -> f64), (format!("sigmoid({by})"), |x: f64| 1.0 / (1.0 + (-x).exp()))] {
            let mut want: Vec<(f64, &str)> = want.iter().map(|(p, id)| (f(*p), id.as_str())).collect();
            want.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
            let top = query(&db, &format!("SELECT id, score() FROM docs ORDER BY {expr} DESC, id LIMIT 10"), &params);
            let got: Vec<(f64, &str)> = top.scores.iter().zip(&top.rows).map(|(s, r)| (*s, r[0].as_str().unwrap())).collect();
            assert_eq!(got, want[..10], "{name}: top 10 by {expr}");
        }
    }
}

#[test]
fn a_model_reranks_the_top_rows_and_ranks_in_subqueries() {
    let dir = tempfile::tempdir().unwrap();
    let (db, params) = create(&dir.path().join("db.strata"), &models_dir());
    let m = model("xgb_rank");
    let by = ranking("xgb_rank");
    // The model reranks BM25's top 50.
    let first = query(&db, "SELECT id, bm25(body, :t) AS b, rating, year FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50", &params);
    let mut want: Vec<(f64, &str)> = first.rows.iter().map(|r| (m.predict(&[num(&r[1]), num(&r[2]), num(&r[3]) % 6.0]), r[0].as_str().unwrap())).collect();
    // Rows tied keep the order they had.
    want.sort_by(|a, b| b.0.total_cmp(&a.0));
    let sql = format!("WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50) SELECT id, score() FROM top ORDER BY {by} DESC LIMIT 10");
    let got = query(&db, &sql, &params);
    assert_eq!(got.rows.iter().map(|r| r[0].as_str().unwrap()).collect::<Vec<_>>(), want[..10].iter().map(|w| w.1).collect::<Vec<_>>());
    for (s, w) in got.scores.iter().zip(&want) {
        assert_eq!(s.to_bits(), w.0.to_bits());
    }
    // In a subquery: the plan's models are opened wherever they are.
    let top5 = query(&db, &format!("SELECT id FROM docs ORDER BY {by} DESC LIMIT 5"), &params);
    let sub = query(&db, &format!("SELECT id FROM docs WHERE id IN (SELECT id FROM docs ORDER BY {by} DESC LIMIT 5)"), &params);
    let sorted = |rs: &ResultSet| {
        let mut ids: Vec<String> = rs.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
        ids.sort();
        ids
    };
    assert_eq!(sorted(&sub), sorted(&top5));
}

#[test]
fn a_plan_names_its_model_file_and_another_refuses_it() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir_all(models.join("m")).unwrap();
    let put = |from: &str| std::fs::copy(models_dir().join(from).join("gbdt.json"), models.join("m/gbdt.json")).unwrap();
    put("lgb_rank");
    std::fs::create_dir_all(models.join("xgb_rank")).unwrap();
    std::fs::copy(models_dir().join("xgb_rank/gbdt.json"), models.join("xgb_rank/gbdt.json")).unwrap();
    let (db, params) = create(&dir.path().join("db.strata"), &models);
    let sql = "SELECT id, score() FROM docs ORDER BY gbdt('m', bm25(body, :t), rating, year % 6) DESC, id LIMIT 10";
    let plan = db.explain(sql, &params).unwrap();
    let lgb = model("lgb_rank");
    assert!(plan.features.iter().any(|f| f == RANK_MODELS), "{:?}", plan.features);
    let text = plan.explain(&[&db.schema()]).join("\n");
    assert!(text.contains("order: gbdt('m', bm25(body, '"), "{text}");
    assert!(text.contains(&format!("ranking model: m (hash {})", lgb.hash)), "{text}");
    let SqlResult::Rows(a) = db.run(&plan).unwrap() else { panic!("rows") };
    assert_eq!(a.rows, query(&db, sql, &params).rows);
    // The plan as JSON names the model and the hash, and runs the same.
    let j = plan.to_json().unwrap();
    let order = &j["root"]["input"]["input"]["order"][0]["expr"]["gbdt"];
    assert_eq!((order["model"].as_str(), order["hash"].as_str()), (Some("m"), Some(lgb.hash.as_str())), "{j}");
    let back = Plan::from_json(&j).unwrap();
    assert_eq!(back, plan);
    let SqlResult::Rows(b) = db.run(&back).unwrap() else { panic!("rows") };
    assert_eq!(a.rows, b.rows);
    assert_eq!(a.scores, b.scores);

    // Another file of the model: the plan made with the first is refused,
    // and SQL ranks with the new one.
    put("xgb_rank");
    let e = db.run(&plan).err().expect("a stale plan is refused").to_string();
    assert!(e.contains(&format!("ranking model m here is not the one the plan was made with (hash {} here, {} in the plan)", model("xgb_rank").hash, lgb.hash)), "{e}");
    let now = query(&db, sql, &params);
    let xgb = query(&db, &format!("SELECT id, score() FROM docs ORDER BY {} DESC, id LIMIT 10", ranking("xgb_rank")), &params);
    assert_eq!(now.rows, xgb.rows);
    assert_eq!(now.scores, xgb.scores);
    // A plan without a hash runs with the model there.
    let mut j = j;
    j["root"]["input"]["input"]["order"][0]["expr"]["gbdt"].as_object_mut().unwrap().remove("hash");
    let SqlResult::Rows(c) = db.run(&Plan::from_json(&j).unwrap()).unwrap() else { panic!("rows") };
    assert_eq!(c.rows, xgb.rows);
    // A reader without the feature refuses the plan by name.
    let e = Plan::readable_by(1, &[RANK_MODELS], &["operators"]).unwrap_err().to_string();
    assert!(e.contains("rank_models"), "{e}");
}

#[test]
fn what_a_model_cannot_rank_by_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (db, params) = create(&dir.path().join("db.strata"), &models_dir());
    let q = |by: &str| format!("SELECT id FROM docs ORDER BY {by} DESC LIMIT 5");
    refused(&db, &q("gbdt('lgb_rank', bm25(body, :t), rating)"), &params, "ranking model lgb_rank reads 3 features, not 2");
    refused(&db, &q("gbdt('nope', bm25(body, :t), rating, year)"), &params, "no ranking model named nope");
    refused(&db, &q("gbdt('../lgb_rank', bm25(body, :t), rating, year)"), &params, "a ranking model's name is letters");
    refused(&db, &q("gbdt(bm25(body, :t), rating, year)"), &params, "gbdt('model', feature, ...)");
    refused(&db, &q("gbdt('lgb_rank')"), &params, "gbdt('model', feature, ...)");
    refused(&db, &q("gbdt('lgb_rank', bm25(body, :t), rating, year, k => 1)"), &params, "gbdt('model', feature, ...)");
    refused(&db, &q("gbdt('lgb_rank', bm25(body, :t), rating, title)"), &params, "numeric");
    // A model file that is not one.
    let models = dir.path().join("models");
    std::fs::create_dir_all(models.join("bad")).unwrap();
    std::fs::write(models.join("bad/gbdt.json"), "{\"tree_info\": 3").unwrap();
    db.open_embedders(&models);
    refused(&db, &q("gbdt('bad', bm25(body, :t), rating, year)"), &params, "not JSON");
}

/// The top k by a model is the top k of every row BM25 matches, bit for
/// bit, found without scoring them all (the model's bounds bound the
/// expression), over filters, pages, tiebreaks and top-k algorithms; and a
/// collection split over shards ranks as the whole one.
#[test]
fn the_top_k_by_a_model_is_exact_and_split_alike() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _) = create(&dir.path().join("db.strata"), &models_dir());
    let docs = {
        let mut docs = data::synthetic(800, 16, 5);
        for d in docs.iter_mut().step_by(7) {
            d[col::RATING] = Value::Null;
        }
        docs
    };
    let words = |i: usize, n: usize| docs[i][col::TITLE].as_str().unwrap().split(' ').take(n).collect::<Vec<_>>().join(" ");
    // Three shards of the same rows, with the same models directory.
    let shards: Vec<Db> = (0..3)
        .map(|s| {
            let d = Db::create(&dir.path().join(format!("s{s}.strata")), data::beir_schema(16, VectorIndexKind::Flat), DbOptions::default()).unwrap();
            d.open_embedders(&models_dir());
            d.load(docs.iter().skip(s).step_by(3).cloned()).unwrap();
            d
        })
        .collect();
    let reference = ExecOptions { rank_safe: false, ..ExecOptions::default() };
    let mut rng = StdRng::seed_from_u64(31);
    let (mut runs, mut safe, mut skipped, mut driven, mut every) = (0, 0, 0u64, 0, 0);
    for name in MODELS {
        for _ in 0..12 {
            let algo = ["", ", algo => 'maxscore'", ", algo => 'bmw'"][rng.gen_range(0..3)];
            let by = format!("gbdt('{name}', bm25(body, :t{algo}, depth => 100000), rating, year % 6)");
            let by = [by.clone(), format!("sigmoid({by})"), format!("{by} * (1 + 0.1 * coalesce(rating, 0))")][rng.gen_range(0..3)].clone();
            let filter = ["", "WHERE year >= 2005", "WHERE category IN ('c00', 'c01')", "WHERE rating > 2"][rng.gen_range(0..4)];
            let tail = [" LIMIT 1", " LIMIT 10", " LIMIT 10 OFFSET 15", ", id DESC LIMIT 7", " LIMIT 1000"][rng.gen_range(0..5)];
            let sql = format!("SELECT id, score() FROM docs {filter} ORDER BY {by} DESC{tail}");
            let params = Params::new().set("t", Value::Str(words(rng.gen_range(0..800), rng.gen_range(1..4))));
            let want = db.query_with(&sql, &params, &reference).unwrap_or_else(|e| panic!("{sql}: {e}"));
            let got = db.query_with(&sql, &params, &ExecOptions::default()).unwrap();
            assert_eq!(want.keys, got.keys, "{sql}");
            assert_eq!(want.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>(), got.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>(), "{sql}");
            let plan = got.stats.plan.join("\n");
            safe += plan.contains("rank-safe") as usize;
            driven += plan.matches("ranks by the expression").count();
            every += plan.matches("every match scored").count();
            skipped += want.stats.scored - got.stats.scored.min(want.stats.scored);
            runs += 1;
            // Over the shards, as a coordinator runs it: statistics summed,
            // each shard's rows, merged. The plan opened on the whole
            // database, as the coordinator opens it on its own.
            let plan = db.explain(&sql, &params).unwrap();
            let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
            let select = plan.to_select(snaps[0].schema()).unwrap();
            let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select).unwrap()).collect();
            let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() };
            let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &select, &opts).unwrap_or_else(|e| panic!("{sql}: {e}"))).collect();
            let split = cluster::merge(&select, answers).unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert_eq!(split.keys, want.keys, "{sql}: split");
            assert_eq!(split.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>(), want.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>(), "{sql}: split");
        }
    }
    assert!(safe * 10 >= runs * 9, "{safe} of {runs} rank-safe");
    assert!(skipped > 0, "the bounds skip rows");
    // The model's bounds narrow with the text score, so that the top-k
    // loop skips rows, in most segments (in the others the model's best
    // leaves do not need a high text score).
    assert!(every * 2 < driven, "{every} of {driven} segments scored every match");
    // A query bound without its models opened is refused, not ranked by
    // NULL: by the executor and by the merge.
    let snap = db.snapshot();
    let sql = format!("SELECT id FROM docs ORDER BY {} DESC LIMIT 5", ranking("lgb_rank"));
    let params = Params::new().set("t", Value::Str(words(3, 2)));
    let select = strata::query::bind(&sql, snap.schema(), &params).unwrap();
    let e = db.execute(&snap, &select, &ExecOptions::default()).err().unwrap().to_string();
    assert!(e.contains("ranking model lgb_rank is not open"), "{e}");
    let e = cluster::merge(&select, Vec::new()).err().unwrap().to_string();
    assert!(e.contains("ranking model lgb_rank is not open on the coordinator"), "{e}");
}
