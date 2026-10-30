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
//! After an intended change, write the new baseline and commit it:
//! `STRATA_RELEVANCE_GATE=update cargo test --release --test relevance_gate`.
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

fn baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/relevance_gate.json")
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
fn baseline_text(collections: &serde_json::Map<String, Json>) -> String {
    let one = |j: &Json| serde_json::to_string(j).unwrap();
    let mut out = String::from("{\n  \"about\": \"BM25 through SQL on BEIR test splits: the baseline of tests/relevance_gate.rs, written by it with STRATA_RELEVANCE_GATE=update; not edited by hand.\",\n  \"collections\": {\n");
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

#[test]
fn bm25_relevance_holds() {
    let mode = std::env::var("STRATA_RELEVANCE_GATE").unwrap_or_default();
    let Some(data) = std::env::var_os("STRATA_BEIR_DIR").map(PathBuf::from).filter(|d| COLLECTIONS.iter().all(|c| d.join(c).join("corpus.jsonl").exists())) else {
        assert!(mode != "required", "the relevance gate needs $STRATA_BEIR_DIR with {COLLECTIONS:?} (tests/fixtures/beir/fetch.sh)");
        eprintln!("$STRATA_BEIR_DIR not set or incomplete; skipping the relevance gate");
        return;
    };
    let update = mode == "update";
    let baseline: Json = if update { Json::Null } else { serde_json::from_str(&std::fs::read_to_string(baseline_path()).unwrap()).unwrap() };
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
            let b = &base["variants"][variant];
            for (metric, values) in [("ndcg10", &run.ndcg10), ("recall100", &run.recall100), ("recall1000", &run.recall1000)] {
                let (now, was) = (mean(values), b[metric].as_f64().unwrap_or(f64::NAN));
                eprintln!("{name} {variant} {metric}: {now:.4}{}", if update { String::new() } else { format!(" (baseline {was:.4})") });
                if !update && !((now - was).abs() <= TOLERANCE) {
                    let per_query: Vec<f64> = b["ndcg10_per_query"].as_array().map(|a| a.iter().filter_map(Json::as_f64).collect()).unwrap_or_default();
                    let p = if per_query.len() == run.ndcg10.len() {
                        let now: Vec<f64> = run.ndcg10.iter().copied().map(round4).collect();
                        format!("{:.4}", paired_randomization(&now, &per_query, 10_000, 1))
                    } else {
                        "n/a".into()
                    };
                    failures.push(format!(
                        "{name} {variant}: {metric} {} from {was:.4} to {now:.4} (nDCG@10 paired randomization p = {p} over {} queries)",
                        if now > was { "rose" } else { "fell" },
                        ids.len()
                    ));
                }
            }
            measured.insert(
                variant.to_string(),
                json!({
                    "ndcg10": round4(mean(&run.ndcg10)),
                    "recall100": round4(mean(&run.recall100)),
                    "recall1000": round4(mean(&run.recall1000)),
                    "ndcg10_per_query": run.ndcg10.iter().copied().map(round4).collect::<Vec<_>>(),
                }),
            );
        }
        report.insert(name.to_string(), json!({ "docs": c.docs.len(), "queries": ids, "variants": measured }));
    }
    if update {
        std::fs::write(baseline_path(), baseline_text(&report)).unwrap();
        eprintln!("wrote {}", baseline_path().display());
        return;
    }
    assert!(
        failures.is_empty(),
        "relevance moved beyond {TOLERANCE}:\n  {}\nIf the change is intended, write the new baseline with STRATA_RELEVANCE_GATE=update and commit tests/fixtures/relevance_gate.json.",
        failures.join("\n  ")
    );
}
