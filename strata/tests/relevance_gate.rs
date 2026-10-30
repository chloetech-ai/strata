//! The relevance gate: BM25 through SQL on three BEIR collections (test
//! split), against the numbers in `tests/fixtures/relevance_gate.json`.
//!
//! Each collection, variant (plain `bm25`, and `fuzzy => 'auto'` but on
//! ArguAna, whose long queries take 0.7 s each with it) and metric
//! (nDCG@10, recall@100, recall@1000) must stay within
//! [`TOLERANCE`] of the baseline, in either direction: a ranking change
//! shows up as a change of that file in the pull request that makes it.
//! A failure reports the paired randomization p-value of nDCG@10 over the
//! queries, to tell a real change from a few queries moving.
//!
//! The collections are read from `$STRATA_BEIR_DIR` (`tests/fixtures/beir/
//! fetch.sh` downloads them, pinned by hash). Without them the test is
//! skipped, unless `STRATA_RELEVANCE_GATE=required` (CI's relevance gate
//! job), when it fails.
//!
//! With feature `bert`, a cross-encoder variant runs too: BM25's top 20
//! reranked by `ms-marco-MiniLM-L-6-v2` from `$STRATA_MODELS`
//! (`tests/models/fetch_cross_encoder.sh` downloads it, pinned by hash), on
//! SciFact and NFCorpus, nDCG@10 and recall@10 against
//! `tests/fixtures/relevance_gate_cross_encoder.json`. Twenty rows, not the
//! hundred of the evaluation (`strata-bm25-eval --variants ce_rerank100`),
//! keep it to minutes on a CPU.
//!
//! After an intended change, write the new baseline and commit it:
//! `STRATA_RELEVANCE_GATE=update cargo test --release --test relevance_gate`
//! (with `--features bert` for the cross-encoder's).
//! Settings are tuned on the dev or train split (`strata-bm25-eval
//! --split`), never on these test queries.

use std::path::{Path, PathBuf};

use serde_json::{json, Value as Json};
use strata::beir::{self, RawCollection};
use strata::db::Db;
use strata::metrics::{mean, ndcg_at_k, paired_randomization, recall_at_k};
use strata::query::Params;
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema, Value};

const COLLECTIONS: [&str; 3] = ["scifact", "nfcorpus", "arguana"];
const VARIANTS: [(&str, &str); 2] = [("plain", ""), ("fuzzy", ", fuzzy => 'auto'")];

/// The variants run on a collection.
fn variants(collection: &str) -> &'static [(&'static str, &'static str)] {
    if collection == "arguana" {
        &VARIANTS[..1]
    } else {
        &VARIANTS
    }
}
/// Largest change of a mean metric that passes: rankings are deterministic,
/// so this only absorbs floating-point differences between platforms.
const TOLERANCE: f64 = 0.002;

fn baseline_path(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(file)
}

/// Per-query nDCG@10, recall@100 and recall@1000 of one variant.
struct Run {
    ndcg10: Vec<f64>,
    recall100: Vec<f64>,
    recall1000: Vec<f64>,
}

fn ranked(db: &Db, sql: &str, q: &str) -> Vec<String> {
    let r = db.query(sql, &Params::new().set("q", Value::Str(q.to_string()))).unwrap_or_else(|e| panic!("{sql} with {q:?}: {e}"));
    r.keys.iter().map(|k| k.as_str().expect("keyword key").to_string()).collect()
}

fn evaluate(c: &RawCollection, dir: &Path, variants: &[(&str, &str)]) -> Vec<Run> {
    let schema = Schema::new("docs", vec![Field::new("id", FieldType::Keyword), Field::new("body", FieldType::Text { positions: false })], Some("id")).unwrap();
    let db = Db::create(&dir.join("db.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load(c.docs.iter().map(|(id, body)| vec![Value::Str(id.clone()), Value::Str(body.clone())])).unwrap();
    variants
        .iter()
        .map(|(_, opts)| {
            // BEIR's queries are text, and a few hold `+1` or `-word`.
            let (top10, top1000) = (
                format!("SELECT id FROM docs ORDER BY bm25(body, :q, syntax => 'plain'{opts}) DESC LIMIT 10"),
                format!("SELECT id FROM docs ORDER BY bm25(body, :q, syntax => 'plain'{opts}) DESC LIMIT 1000"),
            );
            let mut run = Run { ndcg10: Vec::new(), recall100: Vec::new(), recall1000: Vec::new() };
            for (qid, text) in &c.queries {
                let judged = &c.qrels[qid];
                run.ndcg10.push(ndcg_at_k(&ranked(&db, &top10, text), judged, 10));
                let deep = ranked(&db, &top1000, text);
                run.recall100.push(recall_at_k(&deep, judged, 100));
                run.recall1000.push(recall_at_k(&deep, judged, 1000));
            }
            run
        })
        .collect()
}

/// The baseline file: JSON with one line per variant and per query list,
/// so that a change reads as a short diff.
fn baseline_text(about: &str, collections: &serde_json::Map<String, Json>) -> String {
    let one = |j: &Json| serde_json::to_string(j).unwrap();
    let mut out = format!("{{\n  \"about\": {},\n  \"collections\": {{\n", one(&json!(about)));
    for (i, (name, c)) in collections.iter().enumerate() {
        out += &format!("    {}: {{\n      \"docs\": {},\n      \"queries\": {},\n      \"variants\": {{\n", one(&json!(name)), c["docs"], one(&c["queries"]));
        let variants = c["variants"].as_object().unwrap();
        for (j, (v, m)) in variants.iter().enumerate() {
            out += &format!("        {}: {}{}\n", one(&json!(v)), one(m), if j + 1 < variants.len() { "," } else { "" });
        }
        out += &format!("      }}\n    }}{}\n", if i + 1 < collections.len() { "," } else { "" });
    }
    out + "  }\n}\n"
}

fn round4(x: f64) -> f64 {
    (x * 1e4).round() / 1e4
}

/// The collections at `$STRATA_BEIR_DIR`, or `None` when the gate is
/// skipped.
fn collections(names: &[&str]) -> Option<PathBuf> {
    let mode = std::env::var("STRATA_RELEVANCE_GATE").unwrap_or_default();
    let data = std::env::var_os("STRATA_BEIR_DIR").map(PathBuf::from).filter(|d| names.iter().all(|c| d.join(c).join("corpus.jsonl").exists()));
    if data.is_none() {
        assert!(mode != "required", "the relevance gate needs $STRATA_BEIR_DIR with {names:?} (tests/fixtures/beir/fetch.sh)");
        eprintln!("$STRATA_BEIR_DIR not set or incomplete; skipping the relevance gate");
    }
    data
}

/// Checks one variant's per-query metrics (nDCG@10 first) against its
/// baseline `b`, adding what moved to `failures`; returns the baseline's
/// line for it.
fn check(name: &str, variant: &str, metrics: &[(&str, &[f64])], b: &Json, update: bool, failures: &mut Vec<String>) -> Json {
    let ndcg10 = metrics[0].1;
    let mut line = serde_json::Map::new();
    for &(metric, values) in metrics {
        let (now, was) = (mean(values), b[metric].as_f64().unwrap_or(f64::NAN));
        eprintln!("{name} {variant} {metric}: {now:.4}{}", if update { String::new() } else { format!(" (baseline {was:.4})") });
        if !update && !((now - was).abs() <= TOLERANCE) {
            let per_query: Vec<f64> = b["ndcg10_per_query"].as_array().map(|a| a.iter().filter_map(Json::as_f64).collect()).unwrap_or_default();
            let p = if per_query.len() == ndcg10.len() {
                let now: Vec<f64> = ndcg10.iter().copied().map(round4).collect();
                format!("{:.4}", paired_randomization(&now, &per_query, 10_000, 1))
            } else {
                "n/a".into()
            };
            failures.push(format!(
                "{name} {variant}: {metric} {} from {was:.4} to {now:.4} (nDCG@10 paired randomization p = {p} over {} queries)",
                if now > was { "rose" } else { "fell" },
                ndcg10.len()
            ));
        }
        line.insert(metric.to_string(), json!(round4(mean(values))));
    }
    line.insert("ndcg10_per_query".into(), json!(ndcg10.iter().copied().map(round4).collect::<Vec<_>>()));
    Json::Object(line)
}

#[test]
fn bm25_relevance_holds() {
    let Some(data) = collections(&COLLECTIONS) else { return };
    let update = std::env::var("STRATA_RELEVANCE_GATE").is_ok_and(|m| m == "update");
    let file = "relevance_gate.json";
    let baseline: Json = if update { Json::Null } else { serde_json::from_str(&std::fs::read_to_string(baseline_path(file)).unwrap()).unwrap() };
    let tmp = tempfile::tempdir().unwrap();
    let mut report = serde_json::Map::new();
    let mut failures = Vec::new();
    for name in COLLECTIONS {
        let c = beir::load(&data.join(name), "test").unwrap();
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let t = std::time::Instant::now();
        let runs = evaluate(&c, &dir, variants(name));
        eprintln!("{name}: {} documents, {} queries, {:.1} s", c.docs.len(), c.queries.len(), t.elapsed().as_secs_f64());
        let ids: Vec<&str> = c.queries.iter().map(|(id, _)| id.as_str()).collect();
        let base = &baseline["collections"][name];
        if !update && base["queries"] != json!(ids) {
            failures.push(format!("{name}: the judged test queries differ from the baseline's (another version of the collection?)"));
            continue;
        }
        let mut measured = serde_json::Map::new();
        for ((variant, _), run) in variants(name).iter().zip(&runs) {
            let metrics: [(&str, &[f64]); 3] = [("ndcg10", &run.ndcg10), ("recall100", &run.recall100), ("recall1000", &run.recall1000)];
            measured.insert(variant.to_string(), check(name, variant, &metrics, &base["variants"][variant], update, &mut failures));
        }
        report.insert(name.to_string(), json!({ "docs": c.docs.len(), "queries": ids, "variants": measured }));
    }
    finish(file, "BM25 through SQL on BEIR test splits: the baseline of tests/relevance_gate.rs, written by it with STRATA_RELEVANCE_GATE=update; not edited by hand.", update, &report, &failures);
}

/// Writes the baseline when updating, else fails with what moved.
fn finish(file: &str, about: &str, update: bool, report: &serde_json::Map<String, Json>, failures: &[String]) {
    if update {
        std::fs::write(baseline_path(file), baseline_text(about, report)).unwrap();
        eprintln!("wrote {}", baseline_path(file).display());
        return;
    }
    assert!(
        failures.is_empty(),
        "relevance moved beyond {TOLERANCE}:\n  {}\nIf the change is intended, write the new baseline with STRATA_RELEVANCE_GATE=update and commit tests/fixtures/{file}.",
        failures.join("\n  ")
    );
}

/// The cross-encoder variant ([module docs](self)).
#[cfg(feature = "bert")]
#[test]
fn cross_encoder_relevance_holds() {
    const MODEL: &str = "ms-marco-MiniLM-L-6-v2";
    const COLLECTIONS: [&str; 2] = ["scifact", "nfcorpus"];
    let Some(data) = collections(&COLLECTIONS) else { return };
    let mode = std::env::var("STRATA_RELEVANCE_GATE").unwrap_or_default();
    let models = std::env::var_os("STRATA_MODELS").map(PathBuf::from).filter(|d| d.join(MODEL).join("model.safetensors").exists());
    let Some(models) = models else {
        assert!(mode != "required", "the cross-encoder gate needs $STRATA_MODELS with {MODEL} (tests/models/fetch_cross_encoder.sh)");
        eprintln!("$STRATA_MODELS has no {MODEL}; skipping the cross-encoder gate");
        return;
    };
    let update = mode == "update";
    let file = "relevance_gate_cross_encoder.json";
    let baseline: Json = if update { Json::Null } else { serde_json::from_str(&std::fs::read_to_string(baseline_path(file)).unwrap()).unwrap() };
    let tmp = tempfile::tempdir().unwrap();
    let sql = format!("WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :q) DESC LIMIT 20) SELECT id FROM top ORDER BY cross_encoder('{MODEL}', body, :q) DESC LIMIT 10");
    let mut report = serde_json::Map::new();
    let mut failures = Vec::new();
    for name in COLLECTIONS {
        let c = beir::load(&data.join(name), "test").unwrap();
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let t = std::time::Instant::now();
        let schema = Schema::new("docs", vec![Field::new("id", FieldType::Keyword), Field::new("body", FieldType::Text { positions: false })], Some("id")).unwrap();
        let db = Db::create(&dir.join("db.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
        db.load(c.docs.iter().map(|(id, body)| vec![Value::Str(id.clone()), Value::Str(body.clone())])).unwrap();
        db.open_embedders(&models);
        let (mut ndcg10, mut recall10) = (Vec::new(), Vec::new());
        for (qid, text) in &c.queries {
            let got = ranked(&db, &sql, text);
            ndcg10.push(ndcg_at_k(&got, &c.qrels[qid], 10));
            recall10.push(recall_at_k(&got, &c.qrels[qid], 10));
        }
        eprintln!("{name}: {} documents, {} queries, {:.1} s", c.docs.len(), c.queries.len(), t.elapsed().as_secs_f64());
        let ids: Vec<&str> = c.queries.iter().map(|(id, _)| id.as_str()).collect();
        let base = &baseline["collections"][name];
        if !update && base["queries"] != json!(ids) {
            failures.push(format!("{name}: the judged test queries differ from the baseline's (another version of the collection?)"));
            continue;
        }
        let metrics: [(&str, &[f64]); 2] = [("ndcg10", &ndcg10), ("recall10", &recall10)];
        let line = check(name, "ce_rerank20", &metrics, &base["variants"]["ce_rerank20"], update, &mut failures);
        report.insert(name.to_string(), json!({ "docs": c.docs.len(), "queries": ids, "variants": { "ce_rerank20": line } }));
    }
    let about = format!("BM25's top 20 reranked by the cross-encoder {MODEL} on BEIR test splits: the baseline of tests/relevance_gate.rs (feature bert), written by it with STRATA_RELEVANCE_GATE=update; not edited by hand.");
    finish(file, &about, update, &report, &failures);
}
