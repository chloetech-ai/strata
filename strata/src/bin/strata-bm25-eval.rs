//! nDCG@10 of BM25 ranked through SQL, on BEIR collections in their
//! published layout (`corpus.jsonl`, `queries.jsonl`, `qrels/test.tsv`):
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
//! `results/text/`).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use serde_json::{json, Value as Json};
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

struct Coll {
    docs: Vec<(String, String)>,
    /// Queries with judgements, in file order.
    queries: Vec<(String, String)>,
    qrels: HashMap<String, HashMap<String, u32>>,
}

fn lines(path: &Path) -> impl Iterator<Item = String> {
    let f = File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    BufReader::new(f).lines().map(|l| l.expect("readable line"))
}

fn str_of<'a>(j: &'a Json, key: &str) -> &'a str {
    j.get(key).and_then(Json::as_str).unwrap_or("")
}

fn load(dir: &Path) -> Coll {
    let mut docs = Vec::new();
    for line in lines(&dir.join("corpus.jsonl")) {
        let j: Json = serde_json::from_str(&line).expect("corpus line is JSON");
        let (title, text) = (str_of(&j, "title"), str_of(&j, "text"));
        let body = if title.is_empty() { text.to_string() } else { format!("{title}\n{text}") };
        docs.push((str_of(&j, "_id").to_string(), body));
    }
    let mut qrels: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for line in lines(&dir.join("qrels/test.tsv")).skip(1) {
        let mut cols = line.split('\t');
        let (Some(q), Some(d), Some(g)) = (cols.next(), cols.next(), cols.next()) else { continue };
        qrels.entry(q.to_string()).or_default().insert(d.to_string(), g.trim().parse().unwrap_or(0));
    }
    let mut queries = Vec::new();
    for line in lines(&dir.join("queries.jsonl")) {
        let j: Json = serde_json::from_str(&line).expect("query line is JSON");
        let id = str_of(&j, "_id");
        if qrels.contains_key(id) {
            queries.push((id.to_string(), str_of(&j, "text").to_string()));
        }
    }
    Coll { docs, queries, qrels }
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
        let coll = load(&args.dir.join(name));
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
        let out = json!({ "tag": args.tag, "machine": machine(), "collections": report });
        std::fs::write(&file, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
    }
    eprintln!("wrote {}", file.display());
}
