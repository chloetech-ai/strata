//! `strata-guard-shards-bench`: what a guard (`min_rows`, `max_rows`)
//! costs through a coordinator, which first asks every shard for its
//! counts and then sends them the query at the level chosen: one round
//! more. The rows and queries are of `strata-guard-bench`'s kind (INT
//! columns `v0` to `v15` uniform in 0..1000, a TEXT field `body` of 12
//! Zipf words of 5,000), split by key into `--shards` databases served over gRPC on
//! localhost; each guarded query is timed against the same query at the
//! level chosen, on one database and through the coordinator, both called
//! in this process (no HTTP).
//!
//! ```text
//! strata-guard-shards-bench [--rows 1000000] [--shards 4] [--queries 50] [--reps 9] [--out FILE]
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema, Value};
use strata_server::coord::Coordinator;
use strata_server::server;
use strata_server::service::{shard_of, AgentConfig, Backend, QueryReq, Service};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(about = "Guarded queries on one database and through a coordinator over shards")]
struct Args {
    #[arg(long, default_value_t = 1_000_000)]
    rows: usize,
    #[arg(long, default_value_t = 4)]
    shards: usize,
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
    /// The directory the databases are built in, and removed from after
    /// (the system's temporary directory by default).
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

/// splitmix64: the bench's random numbers, without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() % (hi - lo) as u64) as usize
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A word by Zipf's law over `WORDS`, from its cumulative weights.
fn zipf(rng: &mut Rng, cum: &[f64]) -> usize {
    let x = rng.unit() * cum[cum.len() - 1];
    cum.partition_point(|&c| c < x).min(cum.len() - 1)
}

fn service(backend: Backend, schema: Schema) -> Arc<Service> {
    let agent = AgentConfig { api_key: None, model: strata_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() };
    Arc::new(Service::new(backend, schema, agent, 256, Duration::from_secs(30)))
}

/// The median time of `sql` in ms, its keys, and its plan's guard line.
async fn time(svc: &Arc<Service>, sql: &str, reps: usize) -> (f64, Vec<Value>, Option<String>) {
    let run = || async { svc.query(QueryReq { sql: sql.into(), ..Default::default() }).await.unwrap_or_else(|e| panic!("{sql}: {}", e.message)) };
    let r = run().await;
    let line = r.rs.stats.plan.iter().find(|l| l.trim_start().starts_with("guard: ")).map(|l| l.trim_start().to_string());
    let mut ms = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        assert_eq!(run().await.rs.keys.len(), r.rs.keys.len());
        ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    ms.sort_by(f64::total_cmp);
    (ms[ms.len() / 2], r.rs.keys, line)
}

/// The level a guard line says ran: `guard: AT_LEAST 3 of 4 (...)`.
fn level(line: &str) -> usize {
    line.split(" of ").next().and_then(|s| s.rsplit(' ').next()).and_then(|m| m.parse().ok()).unwrap_or_else(|| panic!("{line}"))
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

#[tokio::main]
async fn main() {
    let a = Args::parse();
    let tmp = a.tmp.clone().unwrap_or_else(std::env::temp_dir).join(format!("strata-guard-shards-bench-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut fields = vec![Field::new("id", FieldType::Int)];
    fields.extend((0..Y).map(|k| Field::new(&format!("v{k}"), FieldType::Int)));
    fields.push(Field::new("body", FieldType::Text { positions: false }));
    let schema = Schema::new("t", fields, Some("id")).unwrap();
    let opts = || DbOptions { sync: false, ..DbOptions::default() };
    let one = Db::create(&tmp.join("one.strata"), schema.clone(), opts()).unwrap();
    let shards: Vec<Db> = (0..a.shards).map(|i| Db::create(&tmp.join(format!("shard{i}.strata")), schema.clone(), opts()).unwrap()).collect();
    let mut rng = Rng(7);
    let cum: Vec<f64> = (1..=WORDS)
        .scan(0.0, |s, r| {
            *s += 1.0 / r as f64;
            Some(*s)
        })
        .collect();
    let t = Instant::now();
    for chunk in (0..a.rows).collect::<Vec<_>>().chunks(100_000) {
        let rows: Vec<Vec<Value>> = chunk
            .iter()
            .map(|&i| {
                let mut row = vec![Value::Int(i as i64)];
                row.extend((0..Y).map(|_| Value::Int(rng.below(0, 1000) as i64)));
                let body: Vec<String> = (0..BODY).map(|_| format!("w{}", zipf(&mut rng, &cum))).collect();
                row.push(Value::Str(body.join(" ")));
                row
            })
            .collect();
        let mut split: Vec<Vec<Vec<Value>>> = vec![Vec::new(); a.shards];
        for r in &rows {
            split[shard_of(&r[0], a.shards)].push(r.clone());
        }
        one.load(rows).unwrap();
        for (db, rows) in shards.iter().zip(split) {
            db.load(rows).unwrap();
        }
    }
    for db in std::iter::once(&one).chain(&shards) {
        db.storage().flush().unwrap();
    }
    eprintln!("{} rows, one database and {} shards, loaded in {:.1} s", a.rows, a.shards, t.elapsed().as_secs_f64());

    let local = service(Backend::Local(Arc::new(one)), schema.clone());
    let mut urls = Vec::new();
    for db in shards {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("http://{}", l.local_addr().unwrap()));
        tokio::spawn(server::serve_grpc(service(Backend::Local(Arc::new(db)), schema.clone()), l, Default::default()));
    }
    let c = Coordinator::connect(urls).await.unwrap_or_else(|e| panic!("{}", e.message));
    let cluster = service(Backend::Cluster(Arc::new(c)), schema.clone());
    let setups = [("one database", &local), ("coordinator", &cluster)];

    let mut at_least = Vec::new();
    for y in [4, 8, 16] {
        let conds: Vec<String> = (0..y).map(|k| format!("v{k} < 200")).collect();
        let guarded = format!("SELECT id FROM t WHERE AT_LEAST(1, {}, max_rows => 10000) LIMIT 100", conds.join(", "));
        let mut runs = Vec::new();
        for (setup, svc) in setups {
            let (ms, keys, line) = time(svc, &guarded, a.reps).await;
            let line = line.expect("a guard line");
            let m = level(&line);
            let (fixed_ms, fixed, _) = time(svc, &format!("SELECT id FROM t WHERE AT_LEAST({m}, {}) LIMIT 100", conds.join(", ")), a.reps).await;
            assert_eq!(keys.len(), fixed.len());
            eprintln!("{setup}, AT_LEAST of {y}: {ms:.2} ms guarded, {fixed_ms:.2} ms at level {m}; {line}");
            runs.push(json!({ "setup": setup, "level": m, "guard": line, "guarded_ms_p50": ms, "fixed_ms_p50": fixed_ms }));
        }
        at_least.push(json!({ "conditions": y, "max_rows": 10000, "runs": runs }));
    }

    // Four words: one of the 10 most common, one of 10-100, 100-1000, 1000-5000.
    let bands = [(0, 10), (10, 100), (100, 1000), (1000, WORDS)];
    let queries: Vec<String> = (0..a.queries).map(|_| bands.iter().map(|&(lo, hi)| format!("w{}", rng.below(lo, hi))).collect::<Vec<_>>().join(" ")).collect();
    let mut text = Vec::new();
    for (setup, svc) in setups {
        let (mut guarded, mut fixed) = (Vec::new(), Vec::new());
        for q in &queries {
            let (ms, keys, line) = time(svc, &format!("SELECT id FROM t ORDER BY bm25(body, '{q}', min => 1, max_rows => 1000) DESC LIMIT 10"), a.reps).await;
            let m = level(&line.expect("a guard line"));
            let (fixed_ms, fixed_keys, _) = time(svc, &format!("SELECT id FROM t ORDER BY bm25(body, '{q}', min => {m}) DESC LIMIT 10"), a.reps).await;
            assert_eq!(keys, fixed_keys, "{setup}: {q}");
            guarded.push(ms);
            fixed.push(fixed_ms);
        }
        let (g, f) = (median(guarded), median(fixed));
        eprintln!("{setup}, bm25 of four words: {g:.2} ms guarded, {f:.2} ms at the level chosen (medians of {} queries)", queries.len());
        text.push(json!({ "setup": setup, "bm25_guarded_ms_p50": g, "bm25_fixed_ms_p50": f }));
    }
    let report = json!({
        "machine": machine(),
        "rows": a.rows,
        "shards": a.shards,
        "reps": a.reps,
        "queries": a.queries,
        "at_least": at_least,
        "text": text,
    });
    let s = serde_json::to_string_pretty(&report).unwrap();
    match &a.out {
        Some(p) => std::fs::write(p, s + "\n").unwrap(),
        None => println!("{s}"),
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
