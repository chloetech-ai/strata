//! `strata-guard-bench`: what a guard (`min_rows`, `max_rows`) costs. A
//! guarded query counts the rows of each of its levels before it runs at
//! the one they choose; it is timed against the same query written at
//! that level, which is what a caller who knew the counts would send.
//!
//! The table has synthetic rows: INT columns `v0` to `v15` uniform in
//! 0..1000, so that `v_k < 200` holds for an independent 20% of the rows,
//! and a TEXT field `body` of 12 words drawn from 5,000 by Zipf's law
//! (`w0` the most common). The AT_LEAST queries guard 4, 8 and 16 of the
//! conditions with `max_rows => 10000`; the text queries are four words of
//! different frequencies, guarded in `bm25(..., min => 1, max_rows =>
//! 1000)` and in `MATCH(..., min => 1, max_rows => 1000)`.
//!
//! ```text
//! strata-guard-bench [--rows 1000000] [--queries 50] [--reps 9] [--out FILE]
//! ```

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema, Value};

#[derive(Parser)]
#[command(about = "Guarded queries against the same queries at the level chosen")]
struct Args {
    #[arg(long, default_value_t = 1_000_000)]
    rows: usize,
    /// Text queries timed.
    #[arg(long, default_value_t = 50)]
    queries: usize,
    /// Timed repetitions of each query (the median is kept), after one
    /// untimed run.
    #[arg(long, default_value_t = 9)]
    reps: usize,
    /// The JSON report; printed to stdout without it.
    #[arg(long)]
    out: Option<PathBuf>,
    /// The directory the table is built in, and removed from after (the
    /// system's temporary directory by default).
    #[arg(long)]
    tmp: Option<PathBuf>,
}

const Y: usize = 16;
const WORDS: usize = 5_000;
const BODY: usize = 12;

fn machine() -> Json {
    let sh = |cmd: &str, args: &[&str]| std::process::Command::new(cmd).args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
    let cpu = sh("sysctl", &["-n", "machdep.cpu.brand_string"]).or_else(|| {
        std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
    });
    let os = sh("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}")).or_else(|| sh("uname", &["-sr"]));
    json!({
        "cpu": cpu,
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "os": os,
        "arch": std::env::consts::ARCH,
    })
}

/// A word by Zipf's law over `WORDS`, from its cumulative weights.
fn zipf(rng: &mut StdRng, cum: &[f64]) -> usize {
    let x = rng.gen::<f64>() * cum[cum.len() - 1];
    cum.partition_point(|&c| c < x).min(cum.len() - 1)
}

/// The median time of `sql` in ms, its rows, and its plan's guard line.
fn time(db: &Db, sql: &str, reps: usize) -> (f64, usize, Option<String>) {
    let run = || db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let r = run();
    let line = r.stats.plan.iter().find(|l| l.starts_with("guard: ")).cloned();
    let mut ms: Vec<f64> = (0..reps)
        .map(|_| {
            let t = Instant::now();
            assert_eq!(run().keys.len(), r.keys.len());
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    (ms[ms.len() / 2], r.keys.len(), line)
}

/// The level a guard line says ran: `guard: AT_LEAST 3 of 4 (...)`.
fn level(line: &str) -> usize {
    line.split(" of ").next().and_then(|s| s.rsplit(' ').next()).and_then(|m| m.parse().ok()).unwrap_or_else(|| panic!("{line}"))
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let a = Args::parse();
    let tmp = a.tmp.clone().unwrap_or_else(std::env::temp_dir).join(format!("strata-guard-bench-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut fields = vec![Field::new("id", FieldType::Int)];
    fields.extend((0..Y).map(|k| Field::new(&format!("v{k}"), FieldType::Int)));
    fields.push(Field::new("body", FieldType::Text { positions: false }));
    let schema = Schema::new("t", fields, Some("id")).unwrap();
    let db = Db::create(&tmp.join("t.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    let mut rng = StdRng::seed_from_u64(7);
    let cum: Vec<f64> = (1..=WORDS).scan(0.0, |s, r| {
        *s += 1.0 / r as f64;
        Some(*s)
    }).collect();
    let t = Instant::now();
    for chunk in (0..a.rows).collect::<Vec<_>>().chunks(100_000) {
        let rows: Vec<Vec<Value>> = chunk
            .iter()
            .map(|&i| {
                let mut row = vec![Value::Int(i as i64)];
                row.extend((0..Y).map(|_| Value::Int(rng.gen_range(0..1000))));
                let body: Vec<String> = (0..BODY).map(|_| format!("w{}", zipf(&mut rng, &cum))).collect();
                row.push(Value::Str(body.join(" ")));
                row
            })
            .collect();
        db.load(rows).unwrap();
    }
    db.storage().flush().unwrap();
    let load_s = t.elapsed().as_secs_f64();
    let segments = db.snapshot().segments().len();
    eprintln!("{} rows in {segments} segments, loaded in {load_s:.1} s", a.rows);

    let mut at_least = Vec::new();
    for y in [4, 8, 16] {
        let conds: Vec<String> = (0..y).map(|k| format!("v{k} < 200")).collect();
        let guarded = format!("SELECT id FROM t WHERE AT_LEAST(1, {}, max_rows => 10000) LIMIT 100", conds.join(", "));
        let (ms, rows, line) = time(&db, &guarded, a.reps);
        let line = line.expect("a guard line");
        let m = level(&line);
        let (fixed_ms, fixed_rows, _) = time(&db, &format!("SELECT id FROM t WHERE AT_LEAST({m}, {}) LIMIT 100", conds.join(", ")), a.reps);
        assert_eq!(rows, fixed_rows);
        eprintln!("AT_LEAST of {y}: {ms:.2} ms guarded, {fixed_ms:.2} ms at level {m}; {line}");
        at_least.push(json!({ "conditions": y, "max_rows": 10000, "level": m, "guard": line, "guarded_ms_p50": ms, "fixed_ms_p50": fixed_ms }));
    }

    // Four words: one of the 10 most common, one of 10-100, 100-1000, 1000-5000.
    let bands = [(0, 10), (10, 100), (100, 1000), (1000, WORDS)];
    let queries: Vec<String> = (0..a.queries).map(|_| bands.iter().map(|&(lo, hi)| format!("w{}", rng.gen_range(lo..hi))).collect::<Vec<_>>().join(" ")).collect();
    let mut text = Vec::new();
    let (mut b_guard, mut b_fixed, mut b_plain, mut m_guard, mut m_fixed) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut levels = [0usize; 5];
    for q in &queries {
        let bm25 = |min: &str| format!("SELECT id, score() FROM t ORDER BY bm25(body, '{q}'{min}) DESC LIMIT 10");
        let (g, _, line) = time(&db, &bm25(", min => 1, max_rows => 1000"), a.reps);
        let line = line.expect("a guard line");
        let m = level(&line);
        levels[m] += 1;
        let (f, _, _) = time(&db, &bm25(&format!(", min => {m}")), a.reps);
        let (p, _, _) = time(&db, &bm25(""), a.reps);
        let filter = |min: &str| format!("SELECT id FROM t WHERE MATCH(body, '{q}', min => {min}) LIMIT 100");
        let (mg, _, mline) = time(&db, &filter("1, max_rows => 1000"), a.reps);
        let mm = level(&mline.expect("a guard line"));
        let (mf, _, _) = time(&db, &filter(&mm.to_string()), a.reps);
        text.push(json!({ "query": q, "guard": line, "bm25": { "guarded_ms": g, "fixed_ms": f, "plain_ms": p }, "match": { "level": mm, "guarded_ms": mg, "fixed_ms": mf } }));
        b_guard.push(g);
        b_fixed.push(f);
        b_plain.push(p);
        m_guard.push(mg);
        m_fixed.push(mf);
    }
    let summary = json!({
        "bm25_guarded_ms_p50": median(b_guard),
        "bm25_fixed_ms_p50": median(b_fixed),
        "bm25_plain_ms_p50": median(b_plain),
        "match_guarded_ms_p50": median(m_guard),
        "match_fixed_ms_p50": median(m_fixed),
        "bm25_levels": { "4": levels[4], "3": levels[3], "2": levels[2], "1": levels[1] },
    });
    eprintln!("text: {summary}");
    let report = json!({
        "machine": machine(),
        "rows": a.rows,
        "segments": segments,
        "load_s": load_s,
        "reps": a.reps,
        "table": "v0..v15 INT uniform in 0..1000 (v_k < 200: an independent 20% of rows each); body TEXT of 12 words drawn by Zipf's law from 5,000",
        "at_least": at_least,
        "text_queries": "four words, one from each of the ranks 0-9, 10-99, 100-999 and 1000-4999; bm25 ORDER BY ... LIMIT 10 and MATCH ... LIMIT 100, guarded with min => 1, max_rows => 1000",
        "text": summary,
        "text_runs": text,
    });
    let out = serde_json::to_string_pretty(&report).unwrap();
    match &a.out {
        Some(p) => std::fs::write(p, out + "\n").unwrap(),
        None => println!("{out}"),
    }
    drop(db);
    std::fs::remove_dir_all(&tmp).unwrap();
}
