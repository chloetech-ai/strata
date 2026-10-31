//! Typo lookups in a term dictionary: the terms within k edits of a word,
//! as `MATCH(f, 'word', fuzzy => k)` and `bm25(..., fuzzy => k)` expand it,
//! found by a walk of the sorted dictionary as a trie (`Dict::fuzzy`)
//! against a test of every term (`Dict::fuzzy_scan`, how Strata found them
//! before the walk).
//!
//! The dictionaries are the analysed terms of a query log of
//! `query<TAB>count` lines (ORCAS's 10.4M queries for the published
//! numbers; `strata-complete-bench` has the recipe), as a TEXT field of
//! the queries would hold them: all of them, and those of a tenth of the
//! queries, as a smaller segment's. The words are a term of each of
//! `samples` queries drawn by count, as typed and with two neighbouring
//! letters swapped. Every walk's answer is checked against the scan's.
//!
//! Writes `results/text/fuzzy-<name>.json` and prints a table.

use std::collections::HashSet;
use std::io::BufRead;
use std::time::Instant;

use clap::Parser;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as Json};
use strata::metrics::{mean, percentile};
use strata::postings::bytes::OwnedBytes;
use strata::text::analyze;
use strata::text::dict::{Dict, DictBuilder};

#[derive(Parser)]
#[command(about = "typo lookups in a term dictionary, walked as a trie or scanned")]
struct Args {
    /// `query<TAB>count` lines.
    #[arg(long)]
    queries: std::path::PathBuf,
    /// The data's name in the output file.
    #[arg(long, default_value = "orcas")]
    name: String,
    /// Typed queries to take a word from.
    #[arg(long, default_value_t = 1000)]
    samples: usize,
    #[arg(long, default_value = "results/text")]
    out: String,
}

fn main() {
    let a = Args::parse();
    let t = Instant::now();
    let mut log: Vec<(String, i64)> = Vec::new();
    for line in std::io::BufReader::new(std::fs::File::open(&a.queries).unwrap()).lines() {
        let line = line.unwrap();
        let Some((q, n)) = line.rsplit_once('\t') else { continue };
        if !q.is_empty() {
            log.push((q.to_string(), n.parse().unwrap()));
        }
    }
    eprintln!("{} queries read in {:.1} s", log.len(), t.elapsed().as_secs_f64());

    // Words: a term of each typed query, as typed and with a swap.
    let clicks: i64 = log.iter().map(|(_, n)| n).sum();
    let mut cum = Vec::with_capacity(log.len());
    let mut acc = 0i64;
    for (_, n) in &log {
        acc += n;
        cum.push(acc);
    }
    let mut rng = rand::rngs::StdRng::seed_from_u64(3);
    let mut typed = Vec::new();
    while typed.len() < a.samples {
        let q = &log[cum.partition_point(|&c| c <= rng.gen_range(0..clicks))].0;
        let terms = analyze(q);
        if !terms.is_empty() {
            typed.push(terms[rng.gen_range(0..terms.len())].clone());
        }
    }
    let swapped: Vec<String> = typed
        .iter()
        .map(|w| {
            let mut c: Vec<char> = w.chars().collect();
            if c.len() >= 2 {
                let i = rng.gen_range(0..c.len() - 1);
                c.swap(i, i + 1);
            }
            c.into_iter().collect()
        })
        .collect();

    let mut dicts: Vec<Json> = Vec::new();
    for (label, every) in [("all queries", 1), ("a tenth of the queries", 10)] {
        let t = Instant::now();
        let mut terms: HashSet<String> = HashSet::new();
        for (q, _) in log.iter().step_by(every) {
            terms.extend(analyze(q));
        }
        let mut terms: Vec<String> = terms.into_iter().collect();
        terms.sort_unstable();
        let mut b = DictBuilder::new();
        for t in &terms {
            b.add(t.as_bytes(), 1, 1, 0);
        }
        let dict = Dict::open(OwnedBytes::new(b.finish())).unwrap();
        eprintln!("{label}: {} terms, {} bytes, built in {:.1} s", terms.len(), dict.byte_len(), t.elapsed().as_secs_f64());
        println!("{label}: {} terms", terms.len());
        println!("{:>8} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>8}", "words", "edits", "walk p50", "walk p99", "walk mean", "scan p50", "scan p99", "scan mean", "found");
        let mut runs = Vec::new();
        for (words_label, words) in [("as typed", &typed), ("swapped", &swapped)] {
            for edits in ["1", "2", "auto"] {
                let k = |w: &str| match edits {
                    "auto" => strata::text::query::auto_edits(w.chars().count()),
                    e => e.parse().unwrap(),
                };
                let (mut walk, mut scan, mut found) = (Vec::new(), Vec::new(), Vec::new());
                for w in words.iter() {
                    let k = k(w);
                    std::hint::black_box(dict.fuzzy(w, k));
                    let t = Instant::now();
                    let got = std::hint::black_box(dict.fuzzy(w, k));
                    walk.push(t.elapsed().as_secs_f64() * 1e3);
                    let t = Instant::now();
                    let want = std::hint::black_box(dict.fuzzy_scan(w, k));
                    scan.push(t.elapsed().as_secs_f64() * 1e3);
                    assert_eq!(got, want, "{w:?} within {k}");
                    found.push(got.len() as f64);
                }
                found.sort_by(f64::total_cmp);
                let row = |ms: &[f64]| (percentile(ms, 50.0), percentile(ms, 99.0), mean(ms));
                let ((w50, w99, wm), (s50, s99, sm)) = (row(&walk), row(&scan));
                println!("{words_label:>8} {edits:>8} {w50:>9.3} {w99:>9.3} {wm:>9.3} {s50:>9.3} {s99:>9.3} {sm:>9.3} {:>8.0}", found[found.len() / 2]);
                runs.push(json!({
                    "words": words_label, "edits": edits,
                    "walk_ms": {"p50": w50, "p90": percentile(&walk, 90.0), "p99": w99, "mean": wm},
                    "scan_ms": {"p50": s50, "p90": percentile(&scan, 90.0), "p99": s99, "mean": sm},
                    "median_found": found[found.len() / 2], "max_found": found[found.len() - 1],
                }));
            }
        }
        dicts.push(json!({"dictionary": label, "terms": terms.len(), "bytes": dict.byte_len(), "runs": runs}));
    }
    let machine = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let report = json!({
        "data": {"name": a.name, "queries": log.len()},
        "params": {"samples": a.samples, "analyzer": "english", "threads": 1},
        "machine": {"cpu": machine, "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)},
        "dictionaries": dicts,
    });
    std::fs::create_dir_all(&a.out).unwrap();
    let path = format!("{}/fuzzy-{}.json", a.out, a.name);
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    eprintln!("wrote {path}");
}
