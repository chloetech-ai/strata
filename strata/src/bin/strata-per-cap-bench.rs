//! `strata-per-cap-bench`: what `MAX k PER field` costs and changes on
//! ArguAna (BEIR), whose arguments come from debates on topics: each
//! document's debate and topic are in its id
//! (`test-<topic>-<debate>-<argument>`). Each query ranks the arguments by
//! `bm25(body, :q)`, top 20, the query's own argument left out (as BEIR
//! scores ArguAna), uncapped and with at most k of a debate or of a topic;
//! for each, the median and 99th percentile time, how many debates or
//! topics the top 20 holds, and nDCG@10.
//!
//! ```sh
//! cargo run --release --bin strata-per-cap-bench -- --dir /home/user/data/beir-raw
//! ```

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use serde_json::{json, Value as Json};
use strata::beir;
use strata::db::Db;
use strata::metrics::{mean, ndcg_at_k, percentile};
use strata::query::Params;
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema, Value};

#[derive(Parser)]
#[command(about = "MAX k PER field on ArguAna: time, groups in the top 20 and nDCG@10")]
struct Args {
    /// Directory holding one BEIR folder per collection.
    #[arg(long)]
    dir: PathBuf,
    /// The rows a debate or topic keeps, comma-separated.
    #[arg(long, value_delimiter = ',', default_value = "1,2,3")]
    ks: Vec<u64>,
    /// Timed runs of every query in turn; each query's median is kept.
    #[arg(long, default_value_t = 3)]
    reps: usize,
    #[arg(long, default_value = "results/text/per_cap.json")]
    out: PathBuf,
    /// Directory for the database (default: system temp).
    #[arg(long)]
    tmp: Option<PathBuf>,
}

fn machine() -> Json {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split(':').nth(1)).map(|m| m.trim().to_string()));
    json!({ "cpu": cpu, "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), "os": std::env::consts::OS, "arch": std::env::consts::ARCH })
}

/// An argument's topic and debate, from its id
/// (`test-digital-freedoms-aeghhgwpe-pro02b`).
fn topic_and_debate(id: &str) -> (String, String) {
    let parts: Vec<&str> = id.split('-').collect();
    match parts.len() {
        n if n >= 4 => (parts[1..n - 2].join("-"), parts[n - 2].to_string()),
        _ => (String::new(), id.to_string()),
    }
}

fn main() {
    let a = Args::parse();
    let coll = beir::load(&a.dir.join("arguana"), "test").unwrap_or_else(|e| panic!("{e}"));
    let tmp = a.tmp.clone().unwrap_or_else(std::env::temp_dir).join(format!("strata-per-cap-bench-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let schema = Schema::new(
        "docs",
        vec![Field::new("id", FieldType::Keyword), Field::new("topic", FieldType::Keyword), Field::new("debate", FieldType::Keyword), Field::new("body", FieldType::Text { positions: false })],
        Some("id"),
    )
    .unwrap();
    let db = Db::create(&tmp.join("arguana.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load(coll.docs.iter().map(|(id, body)| {
        let (topic, debate) = topic_and_debate(id);
        vec![Value::Str(id.clone()), Value::Str(topic), Value::Str(debate), Value::Str(body.clone())]
    }))
    .unwrap();
    db.storage().flush().unwrap();
    let topics: HashSet<String> = coll.docs.iter().map(|(id, _)| topic_and_debate(id).0).collect();
    let debates: HashSet<String> = coll.docs.iter().map(|(id, _)| topic_and_debate(id).1).collect();
    eprintln!("arguana: {} arguments, {} debates, {} topics, {} queries", coll.docs.len(), debates.len(), topics.len(), coll.queries.len());

    let ranked = "SELECT id, topic, debate FROM docs WHERE id <> :qid ORDER BY bm25(body, :q, syntax => 'plain') DESC";
    let mut variants: Vec<(String, String)> = vec![("uncapped".into(), format!("{ranked} LIMIT 20"))];
    for field in ["debate", "topic"] {
        for k in &a.ks {
            variants.push((format!("max_{k}_per_{field}"), format!("{ranked} MAX {k} PER {field} LIMIT 20")));
        }
    }
    let params: Vec<Params> = coll.queries.iter().map(|(qid, q)| Params::new().set("qid", Value::Str(qid.clone())).set("q", Value::Str(q.clone()))).collect();
    let mut times = vec![vec![Vec::new(); coll.queries.len()]; variants.len()];
    let mut answers: Vec<Vec<Vec<Vec<Value>>>> = vec![Vec::new(); variants.len()];
    for rep in 0..a.reps {
        for (j, (_, sql)) in variants.iter().enumerate() {
            for (i, p) in params.iter().enumerate() {
                let t = Instant::now();
                let r = db.query(sql, p).unwrap_or_else(|e| panic!("{sql}: {e}"));
                times[j][i].push(t.elapsed().as_secs_f64() * 1e3);
                if rep == 0 {
                    answers[j].push(r.rows);
                }
            }
        }
    }
    let mut report = serde_json::Map::new();
    for (j, (name, sql)) in variants.iter().enumerate() {
        let medians: Vec<f64> = times[j].iter().map(|t| percentile(t, 50.0)).collect();
        let ids = |rows: &[Vec<Value>]| rows.iter().map(|r| r[0].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();
        let ndcg: Vec<f64> = coll.queries.iter().zip(&answers[j]).map(|((qid, _), rows)| ndcg_at_k(&ids(rows), &coll.qrels[qid], 10)).collect();
        let distinct = |c: usize| mean(&answers[j].iter().map(|rows| rows.iter().map(|r| format!("{:?}", r[c])).collect::<HashSet<_>>().len() as f64).collect::<Vec<_>>());
        let rows = mean(&answers[j].iter().map(|r| r.len() as f64).collect::<Vec<_>>());
        let doc = json!({
            "sql": sql,
            "ndcg10": mean(&ndcg),
            "rows": rows,
            "topics_in_top20": distinct(1),
            "debates_in_top20": distinct(2),
            "ms_p50": percentile(&medians, 50.0),
            "ms_p99": percentile(&medians, 99.0),
        });
        eprintln!(
            "{name}: nDCG@10 {:.4}, {:.1} topics and {:.1} debates in the top 20, p50 {:.2} ms, p99 {:.2} ms",
            doc["ndcg10"].as_f64().unwrap(),
            doc["topics_in_top20"].as_f64().unwrap(),
            doc["debates_in_top20"].as_f64().unwrap(),
            doc["ms_p50"].as_f64().unwrap(),
            doc["ms_p99"].as_f64().unwrap()
        );
        report.insert(name.clone(), doc);
    }
    let out = json!({
        "machine": machine(),
        "collection": "arguana",
        "docs": coll.docs.len(),
        "debates": debates.len(),
        "topics": topics.len(),
        "queries": coll.queries.len(),
        "reps": a.reps,
        "variants": report,
    });
    if let Some(dir) = a.out.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(&a.out, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
    eprintln!("wrote {}", a.out.display());
    drop(db);
    let _ = std::fs::remove_dir_all(&tmp);
}
