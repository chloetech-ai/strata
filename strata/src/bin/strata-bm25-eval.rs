//! nDCG@10 of BM25 ranked through SQL, on BEIR collections in their
//! published layout ([`strata::beir`]):
//! the whole query path (query analysis, expansions, statistics, top-k), for
//! comparing ranking changes before and after.
//!
//! ```sh
//! cargo run --release --bin strata-bm25-eval -- --dir /home/user/data/beir-raw \
//!     --collections scifact,nfcorpus,arguana --tag after
//! ```
//!
//! Each variant is a `bm25` call's options: `plain` (none), `fuzzy`
//! (`fuzzy => 'auto'`). Writes `<out>/bm25_<tag>.json` (default
//! `results/text/`). Tune on `--split dev` (or `train` where a collection
//! has no dev split) and report `test`; `tests/relevance_gate.rs` holds the
//! test numbers.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use serde_json::{json, Value as Json};
use strata::beir;
use strata::db::Db;
use strata::metrics::{mean, ndcg_at_k};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

#[derive(Parser)]
#[command(about = "nDCG@10 of BM25 through SQL on BEIR collections")]
struct Args {
    /// Directory holding one BEIR folder per collection.
    #[arg(long)]
    dir: PathBuf,
    #[arg(long, value_delimiter = ',', default_value = "scifact,nfcorpus,arguana,fiqa,scidocs,trec-covid,quora")]
    collections: Vec<String>,
    #[arg(long, value_delimiter = ',', default_value = "plain,fuzzy")]
    variants: Vec<String>,
    /// The judgements to evaluate: `test`, or `dev` / `train` for tuning.
    #[arg(long, default_value = "test")]
    split: String,
    /// Names the output file (`bm25_<tag>.json`).
    #[arg(long)]
    tag: String,
    #[arg(long, default_value = "results/text")]
    out: PathBuf,
    /// Directory for the databases (default: system temp); each is removed
    /// after its collection.
    #[arg(long)]
    tmp: Option<PathBuf>,
}

fn options(variant: &str) -> &'static str {
    match variant {
        "plain" => "",
        "fuzzy" => ", fuzzy => 'auto'",
        v => panic!("unknown variant {v}: plain or fuzzy"),
    }
}

fn machine() -> Json {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split(':').nth(1)).map(|m| m.trim().to_string()));
    json!({
        "cpu": cpu,
        "cpus": std::thread::available_parallelism().map_or(0, |n| n.get()),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    })
}

fn main() {
    let args = Args::parse();
    let tmp = args.tmp.clone().unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&args.out).unwrap();
    let file = args.out.join(format!("bm25_{}.json", args.tag));
    let mut report = serde_json::Map::new();
    for name in &args.collections {
        let t0 = Instant::now();
        let coll = beir::load(&args.dir.join(name), &args.split).unwrap_or_else(|e| panic!("{name}: {e}"));
        let path = tmp.join(format!("strata-bm25-eval-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        let schema =
            Schema::new("docs", vec![Field::new("id", FieldType::Keyword), Field::new("body", FieldType::Text { positions: false })], Some("id"))
                .unwrap();
        let db = Db::create(&path.join("db.strata"), schema, DbOptions::default()).unwrap();
        db.load(coll.docs.iter().map(|(id, body)| vec![Value::Str(id.clone()), Value::Str(body.clone())])).unwrap();
        let load_s = t0.elapsed().as_secs_f64();
        let mut variants = serde_json::Map::new();
        for v in &args.variants {
            let sql = format!("SELECT id FROM docs ORDER BY bm25(body, :q{}) DESC LIMIT 10", options(v));
            let (mut ndcg, mut ms, mut failed) = (Vec::new(), Vec::new(), 0);
            for (qid, text) in &coll.queries {
                let t = Instant::now();
                let ids: Vec<String> = match db.query(&sql, &Params::new().set("q", Value::Str(text.clone()))) {
                    Ok(r) => r.keys.iter().filter_map(|k| k.as_str().map(str::to_string)).collect(),
                    Err(_) => {
                        failed += 1;
                        Vec::new()
                    }
                };
                ms.push(t.elapsed().as_secs_f64() * 1e3);
                ndcg.push(ndcg_at_k(&ids, &coll.qrels[qid], 10));
            }
            eprintln!("{name} {v}: nDCG@10 {:.4} over {} queries ({failed} failed), {:.2} ms/query", mean(&ndcg), ndcg.len(), mean(&ms));
            variants.insert(v.clone(), json!({ "ndcg10": mean(&ndcg), "ms_mean": mean(&ms), "failed": failed }));
        }
        report.insert(name.clone(), json!({ "docs": coll.docs.len(), "queries": coll.queries.len(), "load_s": load_s, "variants": variants }));
        drop(db);
        let _ = std::fs::remove_dir_all(&path);
        // Written after each collection, so a stopped run keeps what it measured.
        let out = json!({ "tag": args.tag, "split": args.split, "machine": machine(), "collections": report });
        std::fs::write(&file, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
    }
    eprintln!("wrote {}", file.display());
}
