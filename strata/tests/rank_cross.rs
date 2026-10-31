//! Cross-encoders: `cross_encoder('name', field, ..., query)` scores rows'
//! text as Hugging Face transformers does (tests/models, made by
//! make_cross.py there; feature `bert`), only in a global rerank phase,
//! once over the rows of every shard; a plan names the model's files.
//! `builtin:overlap` stands in for a model where the files do not matter.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use strata::cluster;
use strata::data::{self, col};
use strata::db::{Db, SqlResult};
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::ir::{Plan, RANK_MODELS};
use strata::query::stats::TermStats;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

fn models_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models")
}

fn docs() -> Vec<Vec<Value>> {
    data::synthetic(600, 16, 5)
}

/// A database of synthetic documents whose models directory is `models`.
fn create(path: &Path, models: &Path, docs: &[Vec<Value>]) -> Db {
    let db = Db::create(path, data::beir_schema(16, VectorIndexKind::Flat), DbOptions { segment_max_docs: 150, memtable_max_docs: 1_000_000, ..DbOptions::default() }).unwrap();
    db.open_embedders(models);
    db.load(docs.iter().cloned()).unwrap();
    db
}

fn query(db: &Db, text: &str, params: &Params) -> ResultSet {
    db.query(text, params).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn refused(db: &Db, text: &str, params: &Params, says: &str) {
    let e = db.query(text, params).err().unwrap_or_else(|| panic!("{text}: not refused")).to_string();
    assert!(e.contains(says), "{text}: {e}");
}

/// What `builtin:overlap` scores: the share of the query's distinct words
/// in the text.
fn overlap(query: &str, text: &str) -> f64 {
    let words = |s: &str| s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect::<std::collections::HashSet<_>>();
    let (q, t) = (words(query), words(text));
    if q.is_empty() {
        return 0.0;
    }
    q.iter().filter(|w| t.contains(*w)).count() as f64 / q.len() as f64
}

/// A query of a few words of document `i`'s title, and of another's.
fn words(docs: &[Vec<Value>], i: usize) -> String {
    let title = |i: usize| docs[i % docs.len()][col::TITLE].as_str().unwrap().split(' ').take(2).collect::<Vec<_>>().join(" ");
    format!("{} {}", title(i), title(i * 7 + 3))
}

const RERANK: &str = "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 30) SELECT id, title, score() FROM top ORDER BY cross_encoder('builtin:overlap', title, text, :t) DESC LIMIT 10";

#[test]
fn a_cross_encoder_reranks_the_top_rows_of_a_phase() {
    let dir = tempfile::tempdir().unwrap();
    let docs = docs();
    let db = create(&dir.path().join("db.strata"), &models_dir(), &docs);
    for i in 0..20 {
        let t = words(&docs, i * 13);
        let params = Params::new().set("t", Value::Str(t.clone()));
        // BM25's top 30 with their text, scored as the model scores them,
        // ranked by it, ties keeping BM25's order.
        let top = query(&db, "SELECT id, title, text FROM docs ORDER BY bm25(body, :t) DESC LIMIT 30", &params);
        let mut want: Vec<(Value, f64)> = top
            .rows
            .iter()
            .map(|r| {
                let text = [&r[1], &r[2]].iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" ");
                (r[0].clone(), overlap(&t, &text))
            })
            .collect();
        want.sort_by(|a, b| b.1.total_cmp(&a.1));
        want.truncate(10);
        let got = query(&db, RERANK, &params);
        assert_eq!(got.keys, want.iter().map(|w| w.0.clone()).collect::<Vec<_>>(), "{t}");
        assert_eq!(got.scores, want.iter().map(|w| w.1).collect::<Vec<_>>(), "{t}");
        assert_eq!(got.rows.iter().map(|r| r[2].as_f64().unwrap()).collect::<Vec<_>>(), got.scores, "score() is the model's");
        let plan = got.stats.plan.join("\n");
        assert!(plan.contains("rerank 30 rows by cross_encoder('builtin:overlap', title, text)"), "{plan}");
    }
}

#[test]
fn a_plan_names_the_model_and_runs_alike() {
    let dir = tempfile::tempdir().unwrap();
    let docs = docs();
    let db = create(&dir.path().join("db.strata"), &models_dir(), &docs);
    let params = Params::new().set("t", Value::Str(words(&docs, 5)));
    let plan = db.explain(RERANK, &params).unwrap();
    assert!(plan.features.iter().any(|f| f == RANK_MODELS), "{:?}", plan.features);
    let text = plan.explain(&[&db.schema()]).join("\n");
    assert!(text.contains("cross_encoder('builtin:overlap', title, text, '"), "{text}");
    assert!(text.contains("a global phase"), "{text}");
    assert!(text.contains("ranking model: builtin:overlap (hash builtin)"), "{text}");
    let SqlResult::Rows(a) = db.run(&plan).unwrap() else { panic!("rows") };
    assert_eq!(a.rows, query(&db, RERANK, &params).rows);
    let back = Plan::from_json(&plan.to_json().unwrap()).unwrap();
    assert_eq!(back, plan);
    let SqlResult::Rows(b) = db.run(&back).unwrap() else { panic!("rows") };
    assert_eq!((a.rows, a.scores), (b.rows, b.scores));
    // A plan made with another file of the model is refused.
    let mut j = plan.to_json().unwrap();
    let text = serde_json::to_string(&j).unwrap().replace("\"hash\":\"builtin\"", "\"hash\":\"0123456789abcdef\"");
    j = serde_json::from_str(&text).unwrap();
    let e = db.run(&Plan::from_json(&j).unwrap()).err().expect("refused").to_string();
    assert!(e.contains("ranking model builtin:overlap here is not the one the plan was made with"), "{e}");
}

#[test]
fn a_cross_encoder_mixes_with_other_scores_in_its_phase() {
    let dir = tempfile::tempdir().unwrap();
    let docs = docs();
    let db = create(&dir.path().join("db.strata"), &models_dir(), &docs);
    let t = words(&docs, 8);
    let params = Params::new().set("t", Value::Str(t.clone()));
    let sql = "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 25) SELECT id, title, text, rating, score() FROM top ORDER BY cross_encoder('builtin:overlap', title, :t) + 0.01 * coalesce(rating, 0) DESC LIMIT 25";
    let got = query(&db, sql, &params);
    assert_eq!(got.rows.len(), 25);
    for r in &got.rows {
        let want = overlap(&t, r[1].as_str().unwrap_or("")) + 0.01 * r[3].as_f64().unwrap_or(0.0);
        assert!((r[4].as_f64().unwrap() - want).abs() < 1e-12, "{r:?}");
    }
    assert!(got.scores.windows(2).all(|w| w[0] >= w[1]), "{:?}", got.scores);
    // With BM25 through minmax, a set function: scored over the 25 rows.
    let sql = "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 25) SELECT id FROM top ORDER BY 0.7 * minmax(cross_encoder('builtin:overlap', title, text, :t)) + 0.3 * minmax(bm25(body, :t)) DESC LIMIT 5";
    assert_eq!(query(&db, sql, &params).rows.len(), 5);
    // Two phases after it: the model's rows ranked again by a column.
    let sql = "WITH a AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 40), b AS (SELECT * FROM a ORDER BY cross_encoder('builtin:overlap', text, :t) DESC LIMIT 12) SELECT id, rating FROM b ORDER BY coalesce(rating, -1) DESC LIMIT 4";
    let got = query(&db, sql, &params);
    assert_eq!(got.rows.len(), 4);
    assert!(got.scores.windows(2).all(|w| w[0] >= w[1]), "{:?}", got.scores);
}

#[test]
fn what_a_cross_encoder_cannot_do_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let docs = docs();
    let db = create(&dir.path().join("db.strata"), &models_dir(), &docs);
    let params = Params::new().set("t", Value::Str(words(&docs, 1)));
    let phase = |by: &str| format!("WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 20) SELECT id FROM top ORDER BY {by} DESC LIMIT 5");
    // First: it scores every match.
    refused(&db, "SELECT id FROM docs ORDER BY cross_encoder('builtin:overlap', title, :t) DESC LIMIT 5", &params, "a cross-encoder reranks the top rows of a cheaper ranking");
    // In a column, rather than ranking.
    let sql = "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 20) SELECT id, cross_encoder('builtin:overlap', title, :t) FROM top ORDER BY rating DESC LIMIT 5";
    refused(&db, sql, &params, "a cross-encoder ranks in a rerank phase, not in a column");
    refused(&db, &phase("cross_encoder('nope', title, :t)"), &params, "no cross-encoder named nope");
    refused(&db, &phase("cross_encoder('../x', title, :t)"), &params, "a ranking model's name is letters");
    refused(&db, &phase("cross_encoder('builtin:overlap', body, :t)"), &params, "body is not stored");
    refused(&db, &phase("cross_encoder('builtin:overlap', year, :t)"), &params, "year is not a TEXT or KEYWORD field");
    refused(&db, &phase("cross_encoder('builtin:overlap', :t)"), &params, "cross_encoder('model', field, ..., query)");
    refused(&db, &phase("cross_encoder('builtin:overlap', title, 3)"), &params, "cross_encoder('model', field, ..., query)");
    // A model a build without the `bert` feature cannot run.
    #[cfg(not(feature = "bert"))]
    refused(&db, &phase("cross_encoder('ce_bert', title, :t)"), &params, "build Strata with the `bert` feature");
    // features() lists the ranking's features, which it is not one of.
    let sql = "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 20) SELECT id, features() FROM top ORDER BY cross_encoder('builtin:overlap', title, :t) + rating DESC LIMIT 5";
    let rs = query(&db, sql, &params);
    assert_eq!(rs.columns, ["id", "bm25(body, :t)", "rating"]);
}

/// `sql` over `shards`, as a coordinator runs it with the plan `plan`
/// (opened on a whole database, as a coordinator opens it on its own): the
/// keys alone first, merged up to the model's phase; then that phase's
/// rows fetched with their text; then the model over all of them, `check`
/// between its batches.
fn split(shards: &[Db], plan: &Plan, check: &mut dyn FnMut() -> strata::Result<()>) -> strata::Result<ResultSet> {
    let snaps: Vec<_> = shards.iter().map(|d| d.snapshot()).collect();
    let schema = snaps[0].schema();
    let select = plan.to_select(schema)?;
    assert!(select.model_phase().is_some());
    let parts: Vec<TermStats> = shards.iter().zip(&snaps).map(|(d, s)| d.term_stats(s, &select)).collect::<strata::Result<_>>()?;
    let opts = ExecOptions { stats: Some(Arc::new(TermStats::sum(&parts))), shard: true, ..ExecOptions::default() };
    let ids = cluster::ids(&select, schema)?;
    let answers = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &ids, &opts)).collect::<strata::Result<_>>()?;
    let ranked = cluster::merge(&ids, answers)?;
    assert!(ranked.keys.len() <= select.phases[select.model_phase().unwrap()].count, "the merge keeps the model phase's rows");
    let with = cluster::with_texts(&select);
    let fetch = cluster::fetch(&with, schema, ranked.keys.clone())?;
    let fetched = shards.iter().zip(&snaps).map(|(d, s)| d.execute(s, &fetch, &opts)).collect::<strata::Result<_>>()?;
    let (filled, lost) = cluster::fill(&with, ranked, fetched)?;
    assert_eq!(lost, 0);
    cluster::rerank_models(&select, filled, check)
}

/// Three shards of the same rows answer as one database, bit for bit,
/// the model running once over the rows of all of them.
#[test]
fn over_shards_the_model_runs_once_on_their_rows() {
    let dir = tempfile::tempdir().unwrap();
    let docs = docs();
    let db = create(&dir.path().join("db.strata"), &models_dir(), &docs);
    let shards: Vec<Db> = (0..3).map(|s| create(&dir.path().join(format!("s{s}.strata")), &models_dir(), &docs.iter().skip(s).step_by(3).cloned().collect::<Vec<_>>())).collect();
    let queries = [
        RERANK.to_string(),
        "WITH top AS (SELECT * FROM docs WHERE year >= 2005 ORDER BY bm25(body, :t) DESC LIMIT 40) SELECT id, title, score() FROM top ORDER BY cross_encoder('builtin:overlap', text, title, :t) + 0.01 * coalesce(rating, 0) DESC LIMIT 7 OFFSET 2".into(),
        "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 30) SELECT id, category FROM top ORDER BY 0.6 * minmax(cross_encoder('builtin:overlap', title, :t)) + 0.4 * minmax(bm25(body, :t)) DESC LIMIT 10".into(),
        "WITH a AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 50), b AS (SELECT * FROM a ORDER BY rating DESC LIMIT 30) SELECT id FROM b ORDER BY cross_encoder('builtin:overlap', title, text, :t) DESC LIMIT 5".into(),
    ];
    for (i, sql) in queries.iter().enumerate() {
        for t in 0..6 {
            let params = Params::new().set("t", Value::Str(words(&docs, i * 31 + t * 11)));
            let want = query(&db, sql, &params);
            let got = split(&shards, &db.explain(sql, &params).unwrap(), &mut || Ok(())).unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert_eq!(got.columns, want.columns, "{sql}");
            assert_eq!(got.keys, want.keys, "{sql}");
            assert_eq!(got.rows, want.rows, "{sql}");
            assert_eq!(got.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>(), want.scores.iter().map(|s| s.to_bits()).collect::<Vec<_>>(), "{sql}");
            assert!(got.stats.plan.iter().any(|l| l.starts_with("cross-encoder builtin:overlap: scored")), "{:?}", got.stats.plan);
        }
    }
    // The model's batches stop at the deadline.
    let params = Params::new().set("t", Value::Str(words(&docs, 2)));
    let e = split(&shards, &db.explain(RERANK, &params).unwrap(), &mut || Err(strata::Error::Timeout("late".into()))).err().unwrap().to_string();
    assert!(e.contains("timeout: late"), "{e}");
}

/// The models of tests/models score as transformers does.
#[cfg(feature = "bert")]
mod models {
    use super::*;
    use strata::query::model::Models;

    #[test]
    fn scores_equal_pytorch() {
        let j: serde_json::Value = serde_json::from_slice(&std::fs::read(models_dir().join("cross_reference.json")).unwrap()).unwrap();
        let pairs: Vec<(String, String)> = j["pairs"].as_array().unwrap().iter().map(|p| (p[0].as_str().unwrap().to_string(), p[1].as_str().unwrap().to_string())).collect();
        let models = Models::default();
        for name in ["ce_bert", "ce_xlmr"] {
            let want: Vec<f64> = j["scores"][name].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            let m = models.cross_encoder(&models_dir(), name).unwrap();
            assert_eq!(m.hash.len(), 16);
            // One pair at a time, as the reference, and every text of a
            // query in one padded batch.
            let mut worst = 0f64;
            for ((q, t), w) in pairs.iter().zip(&want) {
                let one = m.score(q, &[t.as_str()], &mut || Ok(())).unwrap()[0];
                let texts: Vec<&str> = pairs.iter().filter(|p| p.0 == *q).map(|p| p.1.as_str()).collect();
                let all = m.score(q, &texts, &mut || Ok(())).unwrap();
                let batched = all[texts.iter().position(|x| *x == t.as_str()).unwrap()];
                for got in [one, batched] {
                    worst = worst.max((got - w).abs());
                    assert!((got - w).abs() <= 1e-5 * w.abs().max(1.0), "{name}: {q:?} / {t:?}: {got} vs {w}");
                }
            }
            eprintln!("{name}: largest difference from PyTorch {worst:e}");
        }
    }

    /// Queries that arrive together while a model is not open yet open it
    /// once and share that copy (PERF-N3 of the review of 31 October: a
    /// copy each, up to a gigabyte apiece).
    #[test]
    fn queries_arriving_together_open_a_model_once() {
        let models = Arc::new(Models::default());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let models = models.clone();
                std::thread::spawn(move || models.cross_encoder(&models_dir(), "ce_xlmr").unwrap())
            })
            .collect();
        let got: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(got.iter().all(|m| Arc::ptr_eq(m, &got[0])), "one copy of the model");
        assert!(Arc::ptr_eq(&models.cross_encoder(&models_dir(), "ce_xlmr").unwrap(), &got[0]));
    }

    /// A changed file of a model is a model of another hash: a plan made
    /// with the first is refused.
    #[test]
    fn a_changed_model_refuses_old_plans() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        let put = |from: &str| {
            std::fs::create_dir_all(models.join("ce")).unwrap();
            for f in strata::query::cross_encoder::FILES {
                std::fs::copy(models_dir().join(from).join(f), models.join("ce").join(f)).unwrap();
            }
        };
        put("ce_bert");
        let docs = docs();
        let db = create(&dir.path().join("db.strata"), &models, &docs);
        let params = Params::new().set("t", Value::Str(words(&docs, 4)));
        let sql = "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 20) SELECT id, score() FROM top ORDER BY cross_encoder('ce', title, text, :t) DESC LIMIT 5";
        let plan = db.explain(sql, &params).unwrap();
        let SqlResult::Rows(a) = db.run(&plan).unwrap() else { panic!("rows") };
        assert_eq!(a.rows.len(), 5);
        put("ce_xlmr");
        let e = db.run(&plan).err().expect("a stale plan is refused").to_string();
        assert!(e.contains("ranking model ce here is not the one the plan was made with"), "{e}");
        let now = query(&db, sql, &params);
        assert_ne!(now.scores, a.scores, "SQL ranks with the new model");
    }
}
