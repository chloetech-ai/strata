//! Autocomplete over a query log: `PREFIX(id, :p) ORDER BY n DESC LIMIT
//! 10`, the ten most asked queries starting with what was typed, by the
//! prefix's length.
//!
//! Loads a TSV of `query<TAB>count` (one line per distinct query) into a
//! table `queries(id KEYWORD PRIMARY KEY, n INT)`, in a shuffled order.
//! For ORCAS's 10.4M
//! clicked Bing queries, counted by clicks:
//!
//! ```text
//! curl -O https://msmarco.z22.web.core.windows.net/msmarcoranking/orcas.tsv.gz
//! zcat orcas.tsv.gz | cut -f2 | LC_ALL=C sort | LC_ALL=C uniq -c \
//!   | awk 'BEGIN{OFS="\t"} {n=$1; sub(/^ *[0-9]+ /,""); print $0, n}' > orcas-queries.tsv
//! ```
//!
//! Draws `samples` queries with probability proportional to their count,
//! as typed queries arrive, and for each length cuts each at that many
//! characters. Each prefix runs
//! once to warm up, then once timed, on one thread, first on the segments
//! the load made, then after merging with the default policy until it
//! stops, as a server's merger leaves a table; each time by two methods:
//! `rows` reads every matching row's count, `blocks` reads the count
//! column's blocks best first and stops at the first that cannot reach
//! the top 10 (`ExecOptions::column_blocks`). The answers of the
//! first `check` prefixes of each length are compared with a scan of every
//! query (exact top 10, ties by key). With `--fuzzy auto` (or 1, 2), the
//! prefix allows typos (`fuzzy => 'auto'`: none up to two letters, one up
//! to five, else two), and the scan tests every prefix of every query by
//! edit distance.
//!
//! Writes `results/text/complete-<name>.json` and prints a table.

use std::io::BufRead;
use std::time::Instant;

use clap::Parser;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::metrics::{mean, percentile};
use strata::query::exec::ExecOptions;
use strata::query::{bind, Params};
use strata::storage::{DbOptions, MergePolicy};
use strata::{Field, FieldType, Schema, Value};

#[derive(Parser)]
#[command(about = "autocomplete latency by prefix length over a query log")]
struct Args {
    /// `query<TAB>count` lines.
    #[arg(long)]
    queries: std::path::PathBuf,
    /// The data's name in the output file.
    #[arg(long, default_value = "orcas")]
    name: String,
    /// Typed queries to cut into prefixes.
    #[arg(long, default_value_t = 2000)]
    samples: usize,
    /// Prefix lengths in characters, comma-separated.
    #[arg(long, default_value = "1,2,3,4,6,8,12")]
    lengths: String,
    /// Prefixes of each length checked against a scan.
    #[arg(long, default_value_t = 50)]
    check: usize,
    #[arg(long, default_value = "results/text")]
    out: String,
    /// Where the database is built (removed at the end).
    #[arg(long)]
    dir: Option<std::path::PathBuf>,
    /// Keep each segment's rows sorted by query (`cluster_by = 'id'`), so
    /// that a prefix's rows sit together.
    #[arg(long)]
    cluster: bool,
    /// Prefixes with typos, `auto`, `1` or `2`: `PREFIX(id, :p, fuzzy =>
    /// 'auto')`.
    #[arg(long)]
    fuzzy: Option<String>,
}

/// The `PREFIX` condition timed, without typos or with `--fuzzy`'s.
fn prefix(a: &Args) -> String {
    match a.fuzzy.as_deref() {
        None => "PREFIX(id, :p)".into(),
        Some("auto") => "PREFIX(id, :p, fuzzy => 'auto')".into(),
        Some(k @ ("1" | "2")) => format!("PREFIX(id, :p, fuzzy => {k})"),
        Some(k) => panic!("--fuzzy takes auto, 1 or 2, not {k}"),
    }
}

fn sql(a: &Args) -> String {
    format!("SELECT id, n FROM queries WHERE {} ORDER BY n DESC, id LIMIT 10", prefix(a))
}

/// Whether query `q` matches prefix `p` as the timed query reads it: it
/// starts with `p`, or with `--fuzzy` has a prefix within `p`'s edits
/// whose first letter is `p`'s.
fn matches(a: &Args, q: &str, p: &str) -> bool {
    let Some(fuzzy) = a.fuzzy.as_deref() else { return q.starts_with(p) };
    let p: Vec<char> = p.chars().collect();
    let k = fuzzy.parse().unwrap_or_else(|_| strata::text::query::auto_edits(p.len()));
    let mut v = q.chars();
    if v.next() != p.first().copied() {
        return false;
    }
    let v: Vec<char> = q.chars().take(p.len() + k as usize).collect();
    (1..=v.len()).any(|j| strata::text::fuzzy::within(&p, &v[..j].iter().collect::<String>(), k).is_some())
}

fn main() {
    let a = Args::parse();
    let lengths: Vec<usize> = a.lengths.split(',').map(|x| x.parse().unwrap()).collect();
    let t = Instant::now();
    let mut log: Vec<(String, i64)> = Vec::new();
    for line in std::io::BufReader::new(std::fs::File::open(&a.queries).unwrap()).lines() {
        let line = line.unwrap();
        let Some((q, n)) = line.rsplit_once('\t') else { continue };
        if !q.is_empty() {
            log.push((q.to_string(), n.parse().unwrap()));
        }
    }
    // In no particular order, as a log's queries arrive (a sorted file
    // would put each prefix's rows side by side).
    log.shuffle(&mut rand::rngs::StdRng::seed_from_u64(7));
    let clicks: i64 = log.iter().map(|(_, n)| n).sum();
    eprintln!("{} queries, {clicks} clicks, read in {:.1} s", log.len(), t.elapsed().as_secs_f64());

    let dir = a.dir.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("strata-complete-bench-{}", std::process::id())));
    std::fs::create_dir_all(&dir).unwrap();
    let mut schema = Schema::new("queries", vec![Field::new("id", FieldType::Keyword), Field::new("n", FieldType::Int)], Some("id")).unwrap();
    if a.cluster {
        schema = schema.with_cluster_by("id").unwrap();
    }
    let t = Instant::now();
    let db = Db::create(&dir.join("q.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.load(log.iter().map(|(q, n)| vec![Value::Str(q.clone()), Value::Int(*n)])).unwrap();
    let load_s = t.elapsed().as_secs_f64();
    eprintln!("loaded in {load_s:.1} s");

    // Typed queries, drawn by count.
    let mut cum = Vec::with_capacity(log.len());
    let mut acc = 0i64;
    for (_, n) in &log {
        acc += n;
        cum.push(acc);
    }
    let mut rng = rand::rngs::StdRng::seed_from_u64(1);
    let typed: Vec<&str> = (0..a.samples).map(|_| log[cum.partition_point(|&c| c <= rng.gen_range(0..clicks))].0.as_str()).collect();

    let mut states: Vec<Json> = Vec::new();
    for blocks in [false, true] {
        let loaded = run(&db, &a, &lengths, &typed, &log, blocks);
        states.push(json!({"state": "loaded", "method": method(blocks), "segments": db.snapshot().segments().len(), "runs": loaded}));
    }
    // As a server's merger leaves it: the default policy until it stops.
    let t = Instant::now();
    while db.storage().merge(&MergePolicy::default()).unwrap().merges > 0 {}
    eprintln!("merged in {:.1} s", t.elapsed().as_secs_f64());
    for blocks in [false, true] {
        let merged = run(&db, &a, &lengths, &typed, &log, blocks);
        states.push(json!({"state": "merged", "method": method(blocks), "segments": db.snapshot().segments().len(), "runs": merged}));
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    let machine = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let report = json!({
        "data": {"name": a.name, "queries": log.len(), "clicks": clicks},
        "params": {"sql": sql(&a), "samples": a.samples, "threads_query": 1, "load_s": load_s, "cluster_by": a.cluster.then_some("id")},
        "machine": {"cpu": machine, "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)},
        "states": states,
    });
    std::fs::create_dir_all(&a.out).unwrap();
    let path = format!("{}/complete-{}.json", a.out, a.name);
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    eprintln!("wrote {path}");
}

fn method(blocks: bool) -> &'static str {
    if blocks {
        "blocks"
    } else {
        "rows"
    }
}

/// Times every length's prefixes on `db` as it is now, by the method
/// `blocks` names, prints a table and returns the runs.
fn run(db: &Db, a: &Args, lengths: &[usize], typed: &[&str], log: &[(String, i64)], blocks: bool) -> Vec<Json> {
    let snap = db.snapshot();
    let schema = snap.schema().clone();
    let exec = ExecOptions { threads: 1, column_blocks: blocks, ..ExecOptions::default() };
    let mut runs: Vec<Json> = Vec::new();
    println!("{} segments, {}", snap.segments().len(), method(blocks));
    println!("{:>6} {:>8} {:>8} {:>8} {:>8} {:>8} {:>12} {:>12}", "chars", "prefixes", "p50 ms", "p90 ms", "p99 ms", "max ms", "median rows", "max rows");
    for &len in lengths {
        let prefixes: Vec<String> = typed.iter().filter(|q| q.chars().count() >= len).map(|q| q.chars().take(len).collect()).collect();
        let selects: Vec<_> = prefixes.iter().map(|p| bind(&sql(a), &schema, &Params::new().set("p", Value::Str(p.clone()))).unwrap()).collect();
        for s in &selects {
            std::hint::black_box(db.execute(&snap, s, &exec).unwrap());
        }
        let mut ms = Vec::with_capacity(selects.len());
        for s in &selects {
            let t = Instant::now();
            std::hint::black_box(db.execute(&snap, s, &exec).unwrap());
            ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        // How many rows each prefix selects, untimed.
        let count = format!("SELECT COUNT(*) FROM queries WHERE {}", prefix(a));
        let mut rows: Vec<f64> = prefixes
            .iter()
            .map(|p| match db.query(&count, &Params::new().set("p", Value::Str(p.clone()))).unwrap().rows[0][0] {
                Value::Int(n) => n as f64,
                ref v => panic!("count {v:?}"),
            })
            .collect();
        rows.sort_by(f64::total_cmp);
        // The first prefixes against a scan of the log.
        for (p, s) in prefixes.iter().zip(&selects).take(a.check) {
            let mut want: Vec<(i64, &str)> = log.iter().filter(|(q, _)| matches(a, q, p)).map(|(q, n)| (-n, q.as_str())).collect();
            want.sort();
            let want: Vec<&str> = want.into_iter().take(10).map(|(_, q)| q).collect();
            let got = db.execute(&snap, s, &exec).unwrap();
            let got: Vec<&str> = got.rows.iter().map(|r| r[0].as_str().unwrap()).collect();
            assert_eq!(got, want, "prefix {p:?}");
        }
        let (p50, p90, p99, max) = (percentile(&ms, 50.0), percentile(&ms, 90.0), percentile(&ms, 99.0), ms.iter().cloned().fold(0.0, f64::max));
        let (mid, most) = (rows[rows.len() / 2], rows[rows.len() - 1]);
        println!("{len:>6} {:>8} {p50:>8.3} {p90:>8.3} {p99:>8.3} {max:>8.3} {mid:>12.0} {most:>12.0}", prefixes.len());
        runs.push(json!({"chars": len, "prefixes": prefixes.len(), "p50_ms": p50, "p90_ms": p90, "p99_ms": p99, "max_ms": max, "mean_ms": mean(&ms), "median_rows": mid, "max_rows": most, "checked": a.check.min(prefixes.len())}));
    }
    runs
}
