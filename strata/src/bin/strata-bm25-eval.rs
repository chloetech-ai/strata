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
//! 100-row phase adds to BM25 alone. The `gbdt` variants rank by
//! `gbdt('ltr_<collection>', bm25(body, :q), importance)`, a LightGBM
//! model in `--models` trained on a training split: `gbdt` its exact top
//! 10 (rank-safe, from the model's bounds), `gbdt_top100` its top 10 of
//! BM25's top 100 with rank-safe off, and `gbdt_rerank100` BM25's top 100
//! reranked by it in a second phase. The term-operator variants rank the
//! query's words (signs, quotes and carets dropped) with the default
//! syntax: `words` as they are, `weight_first` with its first word that is
//! not a stopword weighing 2, `require_first` with that word required
//! (`+word`) and `exclude_last` with its last such word excluded
//! (`-word`): what weights and the filters they make cost against
//! `words`. `min2` and `min50` rank the rows with at least 2, or half, of
//! the query's words (`min => 2`, `min => '50%'`). The `_r100` variants
//! take 100 rows, with their recall@100 beside nDCG@10: `plain_r100` any
//! of the words, `all_r100` all of them (`min => '100%'`), and
//! `guard_r100` all of them while 100 rows have them, else as many as do
//! (`min => 1, min_rows => 100`); with `all_r100`, the recalls are given
//! again over the queries fewer than 100 rows match all the words of.
//! `--features DIR` makes their training
//! data instead of timing anything: each query's BM25 top 100 with
//! `features()` and its grades, in `<DIR>/<collection>.jsonl`, which
//! `scripts/train_ltr.py` trains the models on. `--reps N` runs the
//! variants N times in turn and keeps each query's median time. Writes
//! `<out>/bm25_<tag>.json` (default `results/text/`). Tune on `--split dev` (or `train` where a collection
//! has no dev split) and report `test`; `tests/relevance_gate.rs` holds the
//! test numbers.
//!
//! Every `bm25` reads a query as plain words (`syntax => 'plain'`): BEIR's
//! queries are text, and a few hold `+1` or `-word`.

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
use strata::text::Analyzer;
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
    /// Where the gbdt variants' models are (`ltr_<collection>/gbdt.json`).
    #[arg(long)]
    models: Option<PathBuf>,
    /// Writes each query's BM25 top 100, `features()` and grades to
    /// `<DIR>/<collection>.jsonl` instead of timing the variants.
    #[arg(long)]
    features: Option<PathBuf>,
}

/// A variant's query on collection `coll` and whether it ranks rank-safe.
fn query(variant: &str, coll: &str) -> (String, bool) {
    let top10 = |rank: &str| format!("SELECT id FROM docs ORDER BY {rank} DESC LIMIT 10");
    let top100 = |rank: &str| format!("SELECT id FROM docs ORDER BY {rank} DESC LIMIT 100");
    // A rerank phase over BM25's top 100.
    let rerank = |rank: &str| format!("WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :q, syntax => 'plain') DESC LIMIT 100) SELECT id FROM top ORDER BY {rank} DESC LIMIT 10");
    let model = format!("gbdt('ltr_{coll}', bm25(body, :q, syntax => 'plain'), importance)");
    match variant {
        "plain" => (top10("bm25(body, :q, syntax => 'plain')"), true),
        "fuzzy" => (top10("bm25(body, :q, syntax => 'plain', fuzzy => 'auto')"), true),
        "boost" => (top10("bm25(body, :q, syntax => 'plain') * (1 + 0.2 * importance)"), true),
        "boost_top100" => (top10("bm25(body, :q, syntax => 'plain') * (1 + 0.2 * importance)"), false),
        "boost_rerank100" => (rerank("bm25(body, :q, syntax => 'plain') * (1 + 0.2 * importance)"), true),
        "minmax_rerank100" => (rerank("0.8 * minmax(bm25(body, :q, syntax => 'plain')) + 0.2 * minmax(importance)"), true),
        "strong_boost" => (top10("bm25(body, :q, syntax => 'plain') * (1 + importance)"), true),
        "strong_boost_top100" => (top10("bm25(body, :q, syntax => 'plain') * (1 + importance)"), false),
        "gbdt" => (top10(&model), true),
        "gbdt_top100" => (top10(&model), false),
        "gbdt_rerank100" => (rerank(&model), true),
        "words" | "weight_first" | "require_first" | "exclude_last" => (top10("bm25(body, :q)"), true),
        "min2" => (top10("bm25(body, :q, syntax => 'plain', min => 2)"), true),
        "min50" => (top10("bm25(body, :q, syntax => 'plain', min => '50%')"), true),
        "plain_r100" => (top100("bm25(body, :q, syntax => 'plain')"), true),
        "all_r100" => (top100("bm25(body, :q, syntax => 'plain', min => '100%')"), true),
        "guard_r100" => (top100("bm25(body, :q, syntax => 'plain', min => 1, min_rows => 100)"), true),
        v => panic!("unknown variant {v}: plain, fuzzy, boost, strong_boost, either boost with _top100, boost_rerank100, minmax_rerank100, gbdt, gbdt_top100, gbdt_rerank100, words, weight_first, require_first, exclude_last, min2, min50, plain_r100, all_r100 or guard_r100"),
    }
}

/// The text a variant searches for: a term-operator variant's words of
/// `text`, with the operator it measures on its first or last word that is
/// not a stopword; `text` itself for the others.
fn text_of(variant: &str, text: &str) -> String {
    if !matches!(variant, "words" | "weight_first" | "require_first" | "exclude_last") {
        return text.to_string();
    }
    let words: Vec<&str> =
        text.split(|c: char| c.is_whitespace() || c == '"' || c == '^').map(|w| w.trim_start_matches(['+', '-'])).filter(|w| !w.is_empty()).collect();
    let content = |w: &&str| !Analyzer::English.query_term_seq(w, true).is_empty();
    let at = match variant {
        "words" => None,
        "exclude_last" => words.iter().rposition(content),
        _ => words.iter().position(content),
    };
    let word = |(i, w): (usize, &&str)| match variant {
        _ if Some(i) != at => w.to_string(),
        "weight_first" => format!("{w}^2"),
        "require_first" => format!("+{w}"),
        _ => format!("-{w}"),
    };
    words.iter().enumerate().map(word).collect::<Vec<_>>().join(" ")
}

/// Row `i`'s importance: uniform in [0, 5) by a hash of `i`, to 0.01.
fn importance(i: usize) -> f64 {
    let mut x = (i as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    (x % 500) as f64 / 100.0
}

/// Writes one line per query of `coll`: the features of BM25's top 100
/// rows (`features()`: `bm25(body, :q)` and `importance`) and each row's
/// grade (0 where the judgements do not name it).
fn log_features(db: &Db, coll: &beir::RawCollection, file: &std::path::Path) {
    use std::io::Write;
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let mut out = std::io::BufWriter::new(std::fs::File::create(file).unwrap());
    let sql = "WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :q, syntax => 'plain') DESC LIMIT 100) SELECT id, features() FROM top ORDER BY importance DESC LIMIT 100";
    for (qid, text) in &coll.queries {
        let r = db.query(sql, &Params::new().set("q", Value::Str(text.clone()))).unwrap_or_else(|e| panic!("{qid}: {e}"));
        let grades = &coll.qrels[qid];
        let rows: Vec<Json> = r
            .rows
            .iter()
            .map(|row| {
                let id = row[0].as_str().unwrap_or_default();
                let mut line = vec![json!(id), json!(grades.get(id).copied().unwrap_or(0))];
                line.extend(row[1..].iter().map(|v| match v {
                    Value::Null => Json::Null,
                    v => json!(v.as_f64()),
                }));
                Json::Array(line)
            })
            .collect();
        let line = json!({ "qid": qid, "names": r.columns[1..], "rows": rows });
        writeln!(out, "{line}").unwrap();
    }
    out.flush().unwrap();
    eprintln!("{}: features of {} queries", file.display(), coll.queries.len());
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
        if let Some(models) = &args.models {
            db.open_embedders(models);
        }
        if let Some(dir) = &args.features {
            log_features(&db, &coll, &dir.join(format!("{name}.jsonl")));
            drop(db);
            let _ = std::fs::remove_dir_all(&path);
            continue;
        }
        // Per variant: each query's top ids (from the first run) and times.
        let mut ids: Vec<Vec<Vec<String>>> = vec![Vec::new(); args.variants.len()];
        let mut times: Vec<Vec<Vec<f64>>> = vec![vec![Vec::new(); coll.queries.len()]; args.variants.len()];
        let mut failed = vec![0; args.variants.len()];
        for rep in 0..args.reps.max(1) {
            for (j, v) in args.variants.iter().enumerate() {
                let (sql, safe) = query(v, name);
                let opts = ExecOptions { rank_safe: safe, ..ExecOptions::default() };
                for (q, (_, text)) in coll.queries.iter().enumerate() {
                    let text = text_of(v, text);
                    let t = Instant::now();
                    let got: Vec<String> = match db.query_with(&sql, &Params::new().set("q", Value::Str(text)), &opts) {
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
            // Recall@100, over every query and over those with few rows
            // holding all their words.
            if v.ends_with("_r100") {
                let recall = |q: usize| {
                    let relevant: Vec<&String> = coll.qrels[&coll.queries[q].0].iter().filter(|(_, &g)| g > 0).map(|(d, _)| d).collect();
                    let found = relevant.iter().filter(|d| ids[j][q].contains(d)).count();
                    if relevant.is_empty() { 0.0 } else { found as f64 / relevant.len() as f64 }
                };
                let all: Vec<f64> = (0..ids[j].len()).map(recall).collect();
                doc["recall100"] = json!(mean(&all));
                line += &format!(", recall@100 {:.4}", mean(&all));
                if let Some(a) = args.variants.iter().position(|w| w == "all_r100") {
                    let few: Vec<usize> = (0..ids[a].len()).filter(|&q| ids[a][q].len() < 100).collect();
                    let r: Vec<f64> = few.iter().map(|&q| recall(q)).collect();
                    let n: Vec<f64> = few.iter().map(|&q| ndcg_at_k(&ids[j][q], &coll.qrels[&coll.queries[q].0], 10)).collect();
                    doc["few_all_words"] = json!({ "queries": few.len(), "recall100": mean(&r), "ndcg10": mean(&n) });
                    line += &format!("; over the {} queries with under 100 rows of all their words: recall@100 {:.4}, nDCG@10 {:.4}", few.len(), mean(&r), mean(&n));
                }
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
    if args.features.is_none() {
        eprintln!("wrote {}", file.display());
    }
}
