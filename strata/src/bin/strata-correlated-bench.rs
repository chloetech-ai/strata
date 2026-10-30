//! The cost of correlated subqueries in a rerank phase of 1,000 candidates.
//!
//! Loads `n` items (a title, one of `cats` categories, a price and an
//! embedding) and `events` events of `users` users on them, then runs
//! `requests` requests of each shape, alternating, on one thread: the top
//! 1,000 items by BM25 for a query of two words, reranked to 20 by a
//! ranking that reads the user's history. The history (`h`) is the user's
//! last 50 events joined with their items, a `WITH` result in memory, or
//! the events table read through its indexes. The baseline reranks the
//! same candidates without subqueries.
//!
//! A subquery takes a value for each distinct value among the candidates
//! of the columns it names (a category: `cats` of them at most; an item or
//! its embedding: one per candidate). One whose outer values meet only
//! equalities runs once, grouped by them; others run once per value. The
//! report counts both.
//!
//! Writes `results/correlated/<n>.json` and prints a table.

use std::time::Instant;

use clap::Parser;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::metrics::{mean, percentile};
use strata::query::exec::ExecOptions;
use strata::query::Params;
use strata::storage::DbOptions;
use strata::{Schema, Value};

#[derive(Parser)]
#[command(about = "correlated subqueries in a rerank phase of 1,000 candidates")]
struct Args {
    #[arg(long, default_value_t = 100_000)]
    n: usize,
    #[arg(long, default_value_t = 200)]
    cats: usize,
    #[arg(long, default_value_t = 2_000)]
    users: usize,
    #[arg(long, default_value_t = 400_000)]
    events: usize,
    #[arg(long, default_value_t = 32)]
    dim: usize,
    #[arg(long, default_value_t = 1000)]
    candidates: usize,
    /// Requests of each shape.
    #[arg(long, default_value_t = 300)]
    requests: usize,
    #[arg(long, default_value = "results/correlated")]
    out: String,
    /// Where the database is built (removed at the end).
    #[arg(long)]
    dir: Option<std::path::PathBuf>,
}

const WORDS: usize = 400;

fn machine() -> Json {
    let sh = |cmd: &str, args: &[&str]| std::process::Command::new(cmd).args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
    let cpu = sh("sysctl", &["-n", "machdep.cpu.brand_string"]).or_else(|| {
        std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
    });
    let mem = sh("sysctl", &["-n", "hw.memsize"]).and_then(|s| s.parse::<f64>().ok()).or_else(|| {
        std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| s.lines().next().and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok()).map(|kb| kb * 1024.0))
    });
    let os = sh("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}")).or_else(|| sh("uname", &["-sr"]));
    json!({
        "cpu": cpu,
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "memory_gb": mem.map(|b| (b / 1e9 * 10.0).round() / 10.0),
        "os": os,
        "arch": std::env::consts::ARCH,
    })
}

/// A word of the vocabulary, the first ones the most frequent (Zipf).
fn word(rng: &mut impl Rng) -> String {
    let u: f64 = rng.gen_range(0.0..1.0);
    format!("w{}", ((WORDS as f64).powf(u) as usize).min(WORDS - 1))
}

fn main() {
    let a = Args::parse();
    let dir = a.dir.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("strata-correlated-bench-{}", std::process::id())));
    std::fs::create_dir_all(&dir).unwrap();
    let schema = Schema::from_json(&format!(
        r#"{{"table": "items", "key": "id", "fields": [
            {{"name": "id", "type": "keyword"}}, {{"name": "title", "type": "text"}}, {{"name": "cat", "type": "keyword"}},
            {{"name": "price", "type": "float"}}, {{"name": "emb", "type": "vector", "dim": {}, "index": "flat"}}]}}"#,
        a.dim
    ))
    .unwrap();
    let db = Db::create(&dir.join("bench.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    let mut rng = rand::rngs::StdRng::seed_from_u64(11);
    let t = Instant::now();
    let cat_of: Vec<usize> = (0..a.n).map(|_| rng.gen_range(0..a.cats)).collect();
    let items: Vec<Vec<Value>> = (0..a.n)
        .map(|i| {
            let title: Vec<String> = (0..rng.gen_range(3..8)).map(|_| word(&mut rng)).collect();
            let emb: Vec<f32> = (0..a.dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
            vec![Value::Str(format!("i{i}")), Value::Str(title.join(" ")), Value::Str(format!("c{}", cat_of[i])), Value::Float(rng.gen_range(1..500) as f64), Value::Vector(emb.into())]
        })
        .collect();
    db.load(items).unwrap();
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, cat KEYWORD, action KEYWORD, ts INT) OPTIONS(cluster_by = 'usr')", &Params::new()).unwrap();
    let actions = ["view", "view", "view", "click", "buy"];
    for chunk in (0..a.events).collect::<Vec<_>>().chunks(5_000) {
        let rows: Vec<String> = chunk
            .iter()
            .map(|&e| {
                // Each user keeps to a few categories, as people do.
                let u = rng.gen_range(0..a.users);
                let i = loop {
                    let i = rng.gen_range(0..a.n);
                    if rng.gen_bool(0.2) || cat_of[i] % 7 == u % 7 {
                        break i;
                    }
                };
                format!("({e}, 'u{u}', 'i{i}', 'c{}', '{}', {})", cat_of[i], actions[rng.gen_range(0..actions.len())], 1_000_000 + e as i64 * 10)
            })
            .collect();
        db.sql(&format!("INSERT INTO events (id, usr, item, cat, action, ts) VALUES {}", rows.join(", ")), &Params::new()).unwrap();
    }
    db.storage().flush().unwrap();
    let load_s = t.elapsed().as_secs_f64();
    eprintln!("loaded {} items and {} events in {load_s:.1} s", a.n, a.events);

    let k = a.candidates;
    let h = "h AS (SELECT e.item AS item, i.cat AS cat, i.emb AS emb, e.action AS action, e.ts AS ts FROM events e JOIN items i ON e.item = i.id WHERE e.usr = :u ORDER BY e.ts DESC LIMIT 50)";
    let first = format!("first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT {k})");
    let shapes: Vec<(&str, String)> = vec![
        ("no subquery", format!("WITH {first} SELECT id FROM first ORDER BY bm25(title, :q) + 0.001 * price DESC LIMIT 20")),
        ("history read, no subquery", format!("WITH {h}, {first} SELECT id FROM first ORDER BY bm25(title, :q) + 0.001 * price DESC LIMIT 20")),
        ("1 subquery by category, over h", format!("WITH {h}, {first} SELECT id FROM first ORDER BY bm25(title, :q) + 0.5 * (SELECT COUNT(*) FROM h WHERE h.cat = first.cat) DESC LIMIT 20")),
        (
            "3 subqueries by category, over h",
            format!(
                "WITH {h}, {first} SELECT id FROM first ORDER BY bm25(title, :q) + 0.5 * (SELECT COUNT(*) FROM h WHERE h.cat = first.cat) \
                 - 0.0001 * coalesce((SELECT MIN(:now - h.ts) FROM h WHERE h.cat = first.cat), 100000) \
                 + (SELECT COUNT(*) FROM h WHERE h.cat = first.cat AND h.action = 'buy') DESC LIMIT 20"
            ),
        ),
        ("1 subquery by category, events table", format!("WITH {first} SELECT id FROM first ORDER BY bm25(title, :q) + 0.1 * (SELECT COUNT(*) FROM events e WHERE e.usr = :u AND e.cat = first.cat) DESC LIMIT 20")),
        ("1 subquery by item, events table", format!("WITH {first} SELECT id FROM first ORDER BY bm25(title, :q) + coalesce((SELECT COUNT(*) FROM events e WHERE e.usr = :u AND e.item = first.id), 0) DESC LIMIT 20")),
        ("1 subquery by embedding, over h", format!("WITH {h}, {first} SELECT id FROM first ORDER BY bm25(title, :q) + coalesce((SELECT MAX(dot(h.emb, first.emb)) FROM h WHERE h.action = 'buy'), 0) DESC LIMIT 20")),
    ];
    let requests: Vec<Params> = (0..a.requests)
        .map(|_| {
            Params::new()
                .set("q", Value::Str(format!("{} {}", word(&mut rng), word(&mut rng))))
                .set("u", Value::Str(format!("u{}", rng.gen_range(0..a.users))))
                .set("now", Value::Int(1_000_000 + a.events as i64 * 10))
        })
        .collect();
    let exec = ExecOptions { threads: 1, ..ExecOptions::default() };
    // A warm-up pass of each shape.
    for (_, sql) in &shapes {
        for p in requests.iter().take(20) {
            std::hint::black_box(db.query_with(sql, p, &exec).unwrap_or_else(|e| panic!("{sql}: {e}")));
        }
    }
    let mut ms = vec![Vec::new(); shapes.len()];
    let mut runs = vec![Vec::new(); shapes.len()];
    let mut rows = vec![Vec::new(); shapes.len()];
    let mut values = vec![Vec::new(); shapes.len()];
    for p in &requests {
        for (s, (_, sql)) in shapes.iter().enumerate() {
            let t = Instant::now();
            let rs = db.query_with(sql, p, &exec).unwrap();
            ms[s].push(t.elapsed().as_secs_f64() * 1e3);
            // "correlated: k subqueries over n rows, v values, r runs"
            if let Some(l) = rs.stats.plan.iter().find(|l| l.starts_with("correlated:")) {
                let w: Vec<&str> = l.split_whitespace().collect();
                rows[s].push(w[4].parse::<f64>().unwrap());
                values[s].push(w[6].parse::<f64>().unwrap());
                runs[s].push(w[8].parse::<f64>().unwrap());
            }
        }
    }
    let base = mean(&ms[0]);
    let mut out = Vec::new();
    println!("| shape | p50 ms | mean ms | p99 ms | added mean ms | rows | values | runs |");
    println!("|---|---|---|---|---|---|---|---|");
    let avg_of = |xs: &Vec<f64>| if xs.is_empty() { 0.0 } else { mean(xs) };
    for (s, (name, sql)) in shapes.iter().enumerate() {
        let (p50, avg, p99) = (percentile(&ms[s], 50.0), mean(&ms[s]), percentile(&ms[s], 99.0));
        let (r, v, n) = (avg_of(&rows[s]), avg_of(&values[s]), avg_of(&runs[s]));
        println!("| {name} | {p50:.2} | {avg:.2} | {p99:.2} | {:.2} | {r:.0} | {v:.1} | {n:.1} |", avg - base);
        out.push(json!({"shape": name, "sql": sql, "p50_ms": p50, "mean_ms": avg, "p99_ms": p99, "added_mean_ms": avg - base, "rows_ranked": r, "distinct_values": v, "subquery_runs": n}));
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    let report = json!({
        "data": {"items": a.n, "cats": a.cats, "users": a.users, "events": a.events, "dim": a.dim, "synthetic": true},
        "params": {"candidates": k, "limit": 20, "history": 50, "requests": a.requests, "threads_query": 1},
        "load_s": load_s,
        "machine": machine(),
        "runs": out,
    });
    std::fs::create_dir_all(&a.out).unwrap();
    let path = format!("{}/{}.json", a.out, a.n);
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    eprintln!("wrote {path}");
}
