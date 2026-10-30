//! Reading a user's history from an events table, its segments clustered
//! by user (`cluster_by`) against left in write order.
//!
//! ```text
//! cd strata
//! cargo run --release --bin strata-cluster-bench -- --work /path/outside/the/repo
//! ```
//!
//! Events are written in time order, each by a user drawn by activity
//! (a Pareto law between `--min-events` and `--max-events` per user), so
//! that in write order a user's events are spread over every segment.
//! Both tables are loaded the same way (transactions of 10,000 events,
//! flushed every `--memtable` events), then merged with the default
//! policy, whose time is reported. Then each reads the histories of
//! `--reads` users, as the full history and as its last 50 events, twice:
//! first from disk (the file dropped from the page cache, on Linux), then
//! warm. The users are drawn uniformly (most have few events) and by their
//! share of the events (the active ones). Writes `--out/<host>.json`
//! naming the machine, and prints the tables in Markdown.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Pareto};
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::metrics::percentile;
use strata::query::Params;
use strata::storage::{DbOptions, MergePolicy};
use strata::{Schema, Value};

#[derive(Parser)]
struct Args {
    /// Where the databases go (removed afterwards).
    #[arg(long)]
    work: PathBuf,
    #[arg(long, default_value = "results/cluster")]
    out: PathBuf,
    #[arg(long, default_value_t = 100_000)]
    users: usize,
    #[arg(long, default_value_t = 10)]
    min_events: usize,
    #[arg(long, default_value_t = 10_000)]
    max_events: usize,
    /// Events between flushes.
    #[arg(long, default_value_t = 200_000)]
    memtable: usize,
    /// Users whose histories are read.
    #[arg(long, default_value_t = 2_000)]
    reads: usize,
}

fn schema(clustered: bool) -> Schema {
    let cluster = if clustered { r#""cluster_by": "usr","# } else { "" };
    Schema::from_json(&format!(
        r#"{{"table": "events", "key": "id", {cluster} "fields": [
            {{"name": "id", "type": "int"}}, {{"name": "usr", "type": "keyword"}},
            {{"name": "item", "type": "keyword"}}, {{"name": "ts", "type": "timestamp"}}]}}"#
    ))
    .unwrap()
}

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
        "host": sh("hostname", &[]),
        "cpu": cpu,
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "memory_gb": mem.map(|b| (b / 1e9 * 10.0).round() / 10.0),
        "os": os,
        "arch": std::env::consts::ARCH,
    })
}

/// Each event's user, in time order: each user's count drawn from a
/// Pareto law, then the events shuffled together.
fn events(a: &Args) -> Vec<u32> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let law = Pareto::new(a.min_events as f64, 1.2).unwrap();
    let mut out: Vec<u32> = Vec::new();
    for u in 0..a.users as u32 {
        let n = (law.sample(&mut rng) as usize).min(a.max_events);
        out.extend(std::iter::repeat_n(u, n));
    }
    rand::seq::SliceRandom::shuffle(&mut out[..], &mut rng);
    out
}

/// Drops the file's pages from the page cache (Linux; elsewhere it says
/// it could not).
#[cfg(target_os = "linux")]
fn evict(path: &std::path::Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(f) = std::fs::File::open(path) else { return false };
    // SAFETY: an advisory call on a descriptor this function holds open.
    unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) == 0 }
}

#[cfg(not(target_os = "linux"))]
fn evict(_: &std::path::Path) -> bool {
    false
}

fn ms(xs: &mut [f64]) -> Json {
    xs.sort_by(|a, b| a.total_cmp(b));
    json!({"p50": percentile(xs, 50.0), "p90": percentile(xs, 90.0), "p99": percentile(xs, 99.0), "max": xs.last()})
}

fn run(a: &Args, users: &[u32], clustered: bool) -> Json {
    let path = a.work.join(if clustered { "clustered.strata" } else { "plain.strata" });
    let _ = std::fs::remove_file(&path);
    let db = Db::create(&path, schema(clustered), DbOptions { memtable_max_docs: usize::MAX, ..DbOptions::default() }).unwrap();
    let t0 = Instant::now();
    let ts0 = strata::timestamp::now() - users.len() as i64 * 1000;
    let mut since_flush = 0;
    for (b, chunk) in users.chunks(10_000).enumerate() {
        let mut t = db.storage().begin().unwrap();
        for (j, &u) in chunk.iter().enumerate() {
            let i = (b * 10_000 + j) as i64;
            t.insert(vec![Value::Int(i), Value::Str(format!("u{u}")), Value::Str(format!("p{}", (i * 31 + u as i64) % 50_000)), Value::Timestamp(ts0 + i * 1000)]).unwrap();
        }
        t.commit().unwrap();
        since_flush += chunk.len();
        if since_flush >= a.memtable {
            db.storage().flush().unwrap();
            since_flush = 0;
        }
    }
    db.storage().flush().unwrap();
    let load_s = t0.elapsed().as_secs_f64();
    let segments_before = db.snapshot().segments().len();
    let t1 = Instant::now();
    let merge = db.storage().merge(&MergePolicy::default()).unwrap();
    let merge_s = t1.elapsed().as_secs_f64();
    let segments = db.snapshot().segments().len();
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let mut rng = rand::rngs::StdRng::seed_from_u64(11);
    let uniform: Vec<u32> = (0..a.reads).map(|_| rng.gen_range(0..a.users as u32)).collect();
    let by_events: Vec<u32> = (0..a.reads).map(|_| users[rng.gen_range(0..users.len())]).collect();
    let mut out = json!({});
    let mut db = Some(db);
    let mut evicted = true;
    for (users_name, picks) in [("uniform", &uniform), ("by_events", &by_events)] {
        for (name, sql) in [("history", "SELECT item, ts FROM events WHERE usr = :u LIMIT 1000000"), ("last_50", "SELECT item, ts FROM events WHERE usr = :u ORDER BY ts DESC LIMIT 50")] {
            // Each first pass reads the file from disk: closed, dropped
            // from the page cache, and opened again.
            drop(db.take());
            evicted &= evict(&path);
            let db = db.insert(Db::open(&path, DbOptions::default()).unwrap());
            let mut rows = 0usize;
            let mut passes = Vec::new();
            for _ in 0..2 {
                let mut lat = Vec::with_capacity(picks.len());
                for &u in picks {
                    let p = Params::new().set("u", Value::Str(format!("u{u}")));
                    let t = Instant::now();
                    let r = db.query(sql, &p).unwrap();
                    lat.push(t.elapsed().as_secs_f64() * 1e3);
                    rows += r.rows.len();
                }
                passes.push(ms(&mut lat));
            }
            out[users_name][name] = json!({"first_pass_ms": passes[0], "warm_ms": passes[1], "rows_per_read": rows as f64 / (2 * picks.len()) as f64});
        }
    }
    out["first_pass_from_disk"] = json!(evicted);
    drop(db);
    let _ = std::fs::remove_file(&path);
    json!({
        "clustered": clustered,
        "events": users.len(),
        "load_seconds": load_s,
        "segments_before_merge": segments_before,
        "merge_seconds": merge_s,
        "merge": {"merges": merge.merges, "docs_merged": merge.docs_merged, "bytes_written": merge.bytes_written},
        "segments": segments,
        "file_bytes": bytes,
        "reads": out,
    })
}

fn main() {
    let a = Args::parse();
    std::fs::create_dir_all(&a.work).unwrap();
    let m = machine();
    println!("machine: {m}");
    let users = events(&a);
    println!("{} events of {} users", users.len(), a.users);
    let plain = run(&a, &users, false);
    println!("plain: {plain}");
    let clustered = run(&a, &users, true);
    println!("clustered: {clustered}");
    let host = m["host"].as_str().unwrap_or("unknown").to_string();
    let doc = json!({"machine": m, "users": a.users, "events": users.len(), "min_events": a.min_events, "max_events": a.max_events, "memtable": a.memtable, "reads": a.reads, "plain": plain, "clustered": clustered});
    std::fs::create_dir_all(&a.out).unwrap();
    std::fs::write(a.out.join(format!("{host}.json")), serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    println!("\n| users | read | layout | first pass p50 ms | p99 | warm p50 ms | p99 | rows per read |\n|---|---|---|---|---|---|---|---|");
    for users_name in ["uniform", "by_events"] {
        for (name, r) in ["history", "last_50"].into_iter().flat_map(|n| [(n, &plain), (n, &clustered)]) {
            let x = &r["reads"][users_name][name];
            println!(
                "| {users_name} | {name} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.1} |",
                if r["clustered"] == true { "cluster_by usr" } else { "write order" },
                x["first_pass_ms"]["p50"].as_f64().unwrap(),
                x["first_pass_ms"]["p99"].as_f64().unwrap(),
                x["warm_ms"]["p50"].as_f64().unwrap(),
                x["warm_ms"]["p99"].as_f64().unwrap(),
                x["rows_per_read"].as_f64().unwrap()
            );
        }
    }
    println!("\n| layout | load s | merge s | segments | file MB |\n|---|---|---|---|---|");
    for r in [&plain, &clustered] {
        println!(
            "| {} | {:.1} | {:.1} | {} | {:.0} |",
            if r["clustered"] == true { "cluster_by usr" } else { "write order" },
            r["load_seconds"].as_f64().unwrap(),
            r["merge_seconds"].as_f64().unwrap(),
            r["segments"],
            r["file_bytes"].as_f64().unwrap() / 1e6
        );
    }
    println!("\nWrote {}", a.out.join(format!("{host}.json")).display());
}
