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
//! (`fuzzy => 'auto'`); or a boost of BM25 by a row's `importance`, a
//! synthetic column independent of content (uniform in [0, 5), by row):
//! `boost` ranks by `bm25(body, :q) * (1 + 0.2 * importance)` and
//! `strong_boost` by `bm25(body, :q) * (1 + importance)` (rank-safe: the
//! exact top 10); `boost_top100` and `strong_boost_top100` rank by the same
//! expression over BM25's top 100 rows only, as before rank-safe boosts
//! ([`ExecOptions::rank_safe`] off), with how many of their top 10 are the
//! exact ones. `boost_rerank100` ranks BM25's top 100 again by the boost in
//! a rerank phase (a `WITH`), and `minmax_rerank100` by `0.8 *
//! minmax(bm25) + 0.2 * minmax(importance)` in a global one: the time a
//! 100-row phase adds to BM25 alone. `--reps N` runs the
//! variants N times in turn and keeps each query's median time. Writes
//! `<out>/bm25_<tag>.json` (default `results/text/`). Tune on `--split dev` (or `train` where a collection
//! has no dev split) and report `test`; `tests/relevance_gate.rs` holds the
//! test numbers.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use serde_json::{json, Value as Json};
use strata::beir;
use strata::db::Db;
use strata::metrics::{mean, ndcg_at_k, percentile};
use strata::query::exec::ExecOptions;
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
    /// Timed runs of every variant, in turn; each query's median is kept.
    #[arg(long, default_value_t = 1)]
    reps: usize,
}

/// A variant's query and whether it ranks rank-safe.
fn query(variant: &str) -> (String, bool) {
    let top10 = |rank: &str| format!("SELECT id FROM docs ORDER BY {rank} DESC LIMIT 10");
    // A rerank phase over BM25's top 100.
    let rerank = |rank: &str| format!("WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :q) DESC LIMIT 100) SELECT id FROM top ORDER BY {rank} DESC LIMIT 10");
    match variant {
        "plain" => (top10("bm25(body, :q)"), true),
        "fuzzy" => (top10("bm25(body, :q, fuzzy => 'auto')"), true),
        "boost" => (top10("bm25(body, :q) * (1 + 0.2 * importance)"), true),
        "boost_top100" => (top10("bm25(body, :q) * (1 + 0.2 * importance)"), false),
        "boost_rerank100" => (rerank("bm25(body, :q) * (1 + 0.2 * importance)"), true),
        "minmax_rerank100" => (rerank("0.8 * minmax(bm25(body, :q)) + 0.2 * minmax(importance)"), true),
        "strong_boost" => (top10("bm25(body, :q) * (1 + importance)"), true),
        "strong_boost_top100" => (top10("bm25(body, :q) * (1 + importance)"), false),
        v => panic!("unknown variant {v}: plain, fuzzy, boost, strong_boost, either boost with _top100, boost_rerank100 or minmax_rerank100"),
    }
}

/// Row `i`'s importance: uniform in [0, 5) by a hash of `i`, to 0.01.
fn importance(i: usize) -> f64 {
    let mut x = (i as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    (x % 500) as f64 / 100.0
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
        let schema = Schema::new(
            "docs",
            vec![Field::new("id", FieldType::Keyword), Field::new("body", FieldType::Text { positions: false }), Field::new("importance", FieldType::Float)],
            Some("id"),
        )
        .unwrap();
        let db = Db::create(&path.join("db.strata"), schema, DbOptions::default()).unwrap();
        db.load(coll.docs.iter().enumerate().map(|(i, (id, body))| vec![Value::Str(id.clone()), Value::Str(body.clone()), Value::Float(importance(i))]))
            .unwrap();
        let load_s = t0.elapsed().as_secs_f64();
        // Per variant: each query's top ids (from the first run) and times.
        let mut ids: Vec<Vec<Vec<String>>> = vec![Vec::new(); args.variants.len()];
        let mut times: Vec<Vec<Vec<f64>>> = vec![vec![Vec::new(); coll.queries.len()]; args.variants.len()];
        let mut failed = vec![0; args.variants.len()];
        for rep in 0..args.reps.max(1) {
            for (j, v) in args.variants.iter().enumerate() {
                let (sql, safe) = query(v);
                let opts = ExecOptions { rank_safe: safe, ..ExecOptions::default() };
                for (q, (_, text)) in coll.queries.iter().enumerate() {
                    let t = Instant::now();
                    let got: Vec<String> = match db.query_with(&sql, &Params::new().set("q", Value::Str(text.clone())), &opts) {
                        Ok(r) => r.keys.iter().filter_map(|k| k.as_str().map(str::to_string)).collect(),
                        Err(_) => {
                            failed[j] += (rep == 0) as usize;
                            Vec::new()
                        }
                    };
                    times[j][q].push(t.elapsed().as_secs_f64() * 1e3);
                    if rep == 0 {
                        ids[j].push(got);
                    }
                }
            }
        }
        let mut variants = serde_json::Map::new();
        for (j, v) in args.variants.iter().enumerate() {
            let ndcg: Vec<f64> = coll.queries.iter().zip(&ids[j]).map(|((qid, _), got)| ndcg_at_k(got, &coll.qrels[qid], 10)).collect();
            let ms: Vec<f64> = times[j].iter().map(|t| percentile(t, 50.0)).collect();
            let mut doc = json!({ "ndcg10": mean(&ndcg), "ms_mean": mean(&ms), "ms_p50": percentile(&ms, 50.0), "ms_p99": percentile(&ms, 99.0), "failed": failed[j] });
            let mut line = format!(
                "{name} {v}: nDCG@10 {:.4} over {} queries ({} failed), {:.2} ms/query, p50 {:.2}, p99 {:.2}",
                mean(&ndcg),
                ndcg.len(),
                failed[j],
                mean(&ms),
                percentile(&ms, 50.0),
                percentile(&ms, 99.0)
            );
            // The share of the exact top 10 that BM25's top 100 holds.
            if let Some(b) = v.strip_suffix("_top100").or_else(|| v.strip_suffix("_rerank100")).and_then(|base| args.variants.iter().position(|w| w == base)) {
                let share: Vec<f64> = ids[j]
                    .iter()
                    .zip(&ids[b])
                    .filter(|(_, exact)| !exact.is_empty())
                    .map(|(got, exact)| got.iter().filter(|id| exact.contains(id)).count() as f64 / exact.len() as f64)
                    .collect();
                let whole = ids[j].iter().zip(&ids[b]).filter(|(got, exact)| got == exact).count();
                doc["exact_top10_share"] = json!(mean(&share));
                doc["queries_with_the_exact_top10"] = json!(whole);
                line += &format!(", {:.4} of the exact top 10, all of it for {whole} queries", mean(&share));
            }
            eprintln!("{line}");
            variants.insert(v.clone(), doc);
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
