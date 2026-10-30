//! `strata-at-least-bench`: the time of `AT_LEAST(n, c1, ..., cy)` over a
//! table of synthetic rows, against the same condition written as an OR
//! of the ANDs of every n of the y conditions, and the time of one of the
//! conditions alone. Each condition is `v_k < 200` on its own INT column
//! of values uniform in 0..1000, so it holds for an independent 20% of the
//! rows. Queries count the rows (`SELECT COUNT(*) FROM t WHERE ...`), so
//! the time is the filter's.
//!
//! ```text
//! strata-at-least-bench [--rows 1000000] [--reps 9] [--out FILE]
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
#[command(about = "AT_LEAST against an OR of ANDs over synthetic rows")]
struct Args {
    #[arg(long, default_value_t = 1_000_000)]
    rows: usize,
    /// Timed repetitions of each query (the median and the fastest are
    /// reported), after one untimed run.
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

/// Columns, so the widest query has 16 conditions.
const Y: usize = 16;
/// The largest OR of ANDs timed: C(16, 8) = 12,870 ANDs is past the
/// 1,000 levels an expression may nest.
const MAX_TERMS: usize = 200;

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

fn cond(k: usize) -> String {
    format!("v{k} < 200")
}

/// Every way to choose `n` of `0..y`, in order.
fn subsets(y: usize, n: usize) -> Vec<Vec<usize>> {
    fn go(start: usize, y: usize, n: usize, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if cur.len() == n {
            out.push(cur.clone());
            return;
        }
        for k in start..y {
            cur.push(k);
            go(k + 1, y, n, cur, out);
            cur.pop();
        }
    }
    let mut out = Vec::new();
    go(0, y, n, &mut Vec::new(), &mut out);
    out
}

fn binomial(y: usize, n: usize) -> usize {
    (0..n).fold(1usize, |acc, i| acc * (y - i) / (i + 1))
}

/// The count `sql` gives, and its median and fastest time in ms.
fn time(db: &Db, sql: &str, reps: usize) -> (i64, f64, f64) {
    let run = || {
        let r = db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"));
        r.rows[0][0].as_i64().unwrap()
    };
    let count = run();
    let mut ms: Vec<f64> = (0..reps)
        .map(|_| {
            let t = Instant::now();
            assert_eq!(run(), count);
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    (count, ms[ms.len() / 2], ms[0])
}

fn main() {
    let a = Args::parse();
    let tmp = a.tmp.clone().unwrap_or_else(std::env::temp_dir).join(format!("strata-at-least-bench-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut fields = vec![Field::new("id", FieldType::Int)];
    fields.extend((0..Y).map(|k| Field::new(&format!("v{k}"), FieldType::Int)));
    let schema = Schema::new("t", fields, Some("id")).unwrap();
    let db = Db::create(&tmp.join("t.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    let mut rng = StdRng::seed_from_u64(7);
    let t = Instant::now();
    for chunk in (0..a.rows).collect::<Vec<_>>().chunks(100_000) {
        db.load(chunk.iter().map(|&i| std::iter::once(Value::Int(i as i64)).chain((0..Y).map(|_| Value::Int(rng.gen_range(0..1000)))).collect::<Vec<_>>())).unwrap();
    }
    db.storage().flush().unwrap();
    let load_s = t.elapsed().as_secs_f64();
    let segments = db.snapshot().segments().len();
    eprintln!("{} rows in {segments} segments, loaded in {load_s:.1} s", a.rows);

    let (_, one_ms, _) = time(&db, &format!("SELECT COUNT(*) FROM t WHERE {}", cond(0)), a.reps);
    let mut runs = Vec::new();
    for y in [2, 4, 8, 16] {
        let mut ns = vec![2, y / 2, y - 1];
        ns.sort_unstable();
        ns.dedup();
        for n in ns.into_iter().filter(|&n| n >= 2 && n <= y) {
            let conds: Vec<String> = (0..y).map(cond).collect();
            let at_least = format!("SELECT COUNT(*) FROM t WHERE AT_LEAST({n}, {})", conds.join(", "));
            let (rows, ms, min_ms) = time(&db, &at_least, a.reps);
            let terms = binomial(y, n);
            let or_of_ands = (terms <= MAX_TERMS).then(|| {
                let ors: Vec<String> = subsets(y, n).iter().map(|s| format!("({})", s.iter().map(|&k| cond(k)).collect::<Vec<_>>().join(" AND "))).collect();
                let (rows2, ms2, min2) = time(&db, &format!("SELECT COUNT(*) FROM t WHERE {}", ors.join(" OR ")), a.reps);
                assert_eq!(rows2, rows, "AT_LEAST({n}) of {y} and its OR of ANDs disagree");
                json!({ "ms_p50": ms2, "ms_min": min2 })
            });
            eprintln!(
                "{n} of {y}: {rows} rows, {ms:.2} ms (min {min_ms:.2}); OR of {terms} ANDs: {}",
                or_of_ands.as_ref().map_or("not timed".to_string(), |o| format!("{:.2} ms", o["ms_p50"].as_f64().unwrap()))
            );
            runs.push(json!({ "conditions": y, "need": n, "rows": rows, "ms_p50": ms, "ms_min": min_ms, "or_of_ands": { "ands": terms, "timed": or_of_ands } }));
        }
    }
    let report = json!({
        "machine": machine(),
        "rows": a.rows,
        "segments": segments,
        "load_s": load_s,
        "reps": a.reps,
        "condition": "v_k < 200 on INT columns uniform in 0..1000: an independent 20% of rows each",
        "one_condition_ms_p50": one_ms,
        "runs": runs,
    });
    let text = serde_json::to_string_pretty(&report).unwrap();
    match &a.out {
        Some(p) => std::fs::write(p, text + "\n").unwrap(),
        None => println!("{text}"),
    }
    drop(db);
    std::fs::remove_dir_all(&tmp).unwrap();
}
