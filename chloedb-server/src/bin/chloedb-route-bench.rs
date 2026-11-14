//! `chloedb-route-bench`: how a coordinator's `--route` choice (round-robin
//! or least-cost) answers reads when the copies of a shard are not alike.
//!
//! Builds `--shards` databases of synthetic text (an INT key and a TEXT
//! field `body` of 12 Zipf words of 5,000), copies each `--copies` times
//! and serves every copy over gRPC on localhost, in this process. A fresh
//! coordinator (default `CoordOptions`, only `route` changed: a 100 ms
//! hedge, kept BM25 statistics) is connected for each run, warmed up, then
//! timed for `--seconds` at each `--concurrency`, the two routes taking
//! turns, `--rounds` times. Scenarios (`--scenarios`):
//!
//! * `equal`: the copies alike (the control).
//! * `slow`: copy 0 of shard 0 behind a delay line adding `--delay-ms` to
//!   each answer (latency, not a throughput cap), less than the hedge.
//! * `slowhedge`: the same with `--hedge-delay-ms`, more than the hedge,
//!   so hedges and the cool-off act under either route.
//! * `busy`: copy 0 of shard 0 with `--busy-slots` query slots, where the
//!   others have 64 (a copy with less capacity).
//! * `recover`: `slow` for a third of the run, then not; the slow copy's
//!   share of shard 0's answers each second.
//! * `two`: two coordinators sharing nothing, half the clients each, over
//!   the `slow` copies: each copy's share and its calls in flight.
//! * `stall`: copy 0 of shard 0 stalls: each second, what it answers in
//!   50 ms is held 60 ms. Fixed against adaptive hedges
//!   (`--hedge-adaptive`), under both routes.
//! * `ryw`: one shard, a primary and two replicas, what the primary ships
//!   replica 1 arriving `--lag-ms` late (`Acks::Primary`, so writes do not
//!   wait for it). A quarter of each client's operations write a row and
//!   read it back with `min_seq_nos`, the rest are plain queries: least-cost
//!   with and without `--prefer-caught-up`.
//!
//! Scenarios run several configurations each, in turn: `equal` round-robin
//! and least-cost each with fixed and adaptive hedges; `busy` least-cost
//! with and without the load the copies report (`route_load`); `ryw`
//! round-robin, least-cost, and least-cost preferring caught-up copies.
//!
//! Each run reports the client's latency (p50, p90, p99, max), queries per
//! second, errors, the coordinator's hedges, 429s and timeouts, and each
//! copy of shard 0's share of the answers. Writes a JSON report naming the
//! machine and prints Markdown tables.
//!
//! The copies can run elsewhere, each in a container with a CPU limit of
//! its own, so that a copy asked more, or a hedge, takes capacity the
//! others do not share: `--build-only DIR` builds the shards' databases
//! and stops, and `--external URLS` runs one scenario against copies
//! served from them (`URLS` as a coordinator's `--shards`: shards by
//! commas, copies by `|`; copy 0 of shard 0 is the one slowed, stalled or
//! busy), the delay line and the coordinator here; for `ryw`, `URLS` is
//! the shard's two replicas and its primary, and `--late-link
//! LISTEN=URL` the late link the primary ships replica 1 through.
//! `results/route-containers/run.sh` runs it on Docker.
//!
//! ```text
//! caffeinate -i cargo run --release --bin chloedb-route-bench -- [--rows 3000000] [--seconds 10] [--out results/route-mac]
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chloedb::db::Db;
use chloedb::metrics::percentile;
use chloedb::storage::DbOptions;
use chloedb::{Field, FieldType, Schema, Value};
use chloedb_server::coord::{CoordOptions, Coordinator, Route};
use chloedb_server::server;
use chloedb_server::replication::{Acks, Follow, Primary, PrimaryOptions};
use chloedb_server::service::{shard_of, After, AgentConfig, Backend, QueryReq, Replication, Service};
use clap::Parser;
use serde_json::{json, Value as Json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(about = "Round-robin against least-cost routing over copies that are not alike")]
struct Args {
    #[arg(long, default_value_t = 3_000_000)]
    rows: usize,
    #[arg(long, default_value_t = 3)]
    shards: usize,
    #[arg(long, default_value_t = 3)]
    copies: usize,
    /// Seconds timed per run (after `--warmup`).
    #[arg(long, default_value_t = 10.0)]
    seconds: f64,
    #[arg(long, default_value_t = 3.0)]
    warmup: f64,
    /// Concurrent clients, comma-separated.
    #[arg(long, value_delimiter = ',', default_value = "1,16,64")]
    concurrency: Vec<usize>,
    #[arg(long, default_value_t = 2)]
    rounds: usize,
    #[arg(long, value_delimiter = ',', default_value = "equal,slow,slowhedge,busy,recover,two")]
    scenarios: Vec<String>,
    /// Added to each answer of the slow copy (`slow`, `recover`, `two`):
    /// below the hedge, so the copy is never outrun.
    #[arg(long, default_value_t = 30)]
    delay_ms: u64,
    /// Added to each answer of the slow copy in `slowhedge`: above the
    /// hedge.
    #[arg(long, default_value_t = 150)]
    hedge_delay_ms: u64,
    /// How late what the primary ships the lagging replica arrives (`ryw`).
    #[arg(long, default_value_t = 50)]
    lag_ms: u64,
    /// The share of a client's operations that write and read back (`ryw`).
    #[arg(long, default_value_t = 0.25)]
    ryw_share: f64,
    /// Query slots of the busy copy.
    #[arg(long, default_value_t = 1)]
    busy_slots: usize,
    /// Terms whose BM25 statistics each coordinator keeps per copy
    /// (`--stats-cache-terms`; the default's, 100,000, when not given). A
    /// read with kept statistics asks the copy they are of first.
    #[arg(long)]
    stats_cache_terms: Option<usize>,
    /// Where the report goes (`route.json`, `route.md`).
    #[arg(long, default_value = "results/route-mac")]
    out: PathBuf,
    /// Where the databases are built, and removed from after.
    #[arg(long)]
    tmp: Option<PathBuf>,
    /// Build the shards' databases into this directory
    /// (`shard<i>/c0/db.strata`) and stop.
    #[arg(long)]
    build_only: Option<PathBuf>,
    /// Run against copies served elsewhere (one scenario): the shards' gRPC
    /// URLs, comma-separated, a shard's copies joined by `|`.
    #[arg(long)]
    external: Option<String>,
    /// With --external, for `ryw`: a late link `LISTEN=URL` (bytes each way
    /// `--lag-ms` late), which the primary ships replica 1 through.
    #[arg(long)]
    late_link: Option<String>,
}

const WORDS: usize = 5_000;
const BODY: usize = 12;
const SLOTS: usize = 64;

fn machine() -> Json {
    let sh = |cmd: &str, args: &[&str]| std::process::Command::new(cmd).args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
    let cpu = sh("sysctl", &["-n", "machdep.cpu.brand_string"]).or_else(|| {
        std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
    });
    let mem = sh("sysctl", &["-n", "hw.memsize"]).and_then(|s| s.parse::<u64>().ok()).map(|b| b >> 30);
    let os = sh("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}")).or_else(|| sh("uname", &["-sr"]));
    json!({
        "cpu": cpu,
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "memory_gb": mem,
        "os": os,
        "arch": std::env::consts::ARCH,
        // Run elsewhere (`run.sh` says how): its own words.
        "setup": std::env::var("CHLOEDB_BENCH_SETUP").unwrap_or_else(|_| "one process: shards, coordinators and clients over loopback gRPC".into()),
    })
}

/// splitmix64.
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

fn zipf(rng: &mut Rng, cum: &[f64]) -> usize {
    let x = rng.unit() * cum[cum.len() - 1];
    cum.partition_point(|&c| c < x).min(cum.len() - 1)
}

fn agent() -> AgentConfig {
    AgentConfig { api_key: None, model: chloedb_server::agent::DEFAULT_MODEL.into(), base_url: "http://unused".into() }
}

fn service(backend: Backend, schema: Schema, slots: usize) -> Arc<Service> {
    Arc::new(Service::new(backend, schema, agent(), slots, Duration::from_secs(30)))
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

/// A delay line in front of `to`: each chunk of an answer is written
/// `delay` ms after it was read, in order, without holding the next back
/// (HTTP/2 carries every call to a copy on one connection, so a proxy
/// sleeping between reads would serialize them).
async fn delay_line(to: String, delay: Arc<AtomicU64>) -> String {
    let to = to.trim_start_matches("http://").to_string();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = l.accept().await else { return };
            let (to, delay) = (to.clone(), delay.clone());
            tokio::spawn(async move {
                let Ok(server) = tokio::net::TcpStream::connect(&to).await else { return };
                let _ = client.set_nodelay(true);
                let _ = server.set_nodelay(true);
                let (mut cr, mut cw) = client.into_split();
                let (mut sr, mut sw) = server.into_split();
                tokio::spawn(async move { tokio::io::copy(&mut cr, &mut sw).await });
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 64 << 10];
                    while let Ok(n) = sr.read(&mut buf).await {
                        if n == 0 || tx.send((Instant::now(), buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                });
                while let Some((at, bytes)) = rx.recv().await {
                    let due = at + Duration::from_millis(delay.load(Ordering::Relaxed));
                    tokio::time::sleep_until(due.into()).await;
                    if cw.write_all(&bytes).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    url
}

/// A proxy to `to` passing bytes each way `delay` after they arrive, in
/// order, without holding the next back: a link that takes that long.
async fn late_link(to: &str, delay: Duration) -> String {
    late_link_on("127.0.0.1:0", to, delay).await
}

/// [`late_link`], listening on `listen`.
async fn late_link_on(listen: &str, to: &str, delay: Duration) -> String {
    let to = to.trim_start_matches("http://").to_string();
    let l = TcpListener::bind(listen).await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    fn pipe(mut from: tokio::net::tcp::OwnedReadHalf, mut to: tokio::net::tcp::OwnedWriteHalf, delay: Duration) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 << 10];
            while let Ok(n) = from.read(&mut buf).await {
                if n == 0 || tx.send((Instant::now(), buf[..n].to_vec())).is_err() {
                    return;
                }
            }
        });
        tokio::spawn(async move {
            while let Some((at, bytes)) = rx.recv().await {
                tokio::time::sleep_until((at + delay).into()).await;
                if to.write_all(&bytes).await.is_err() {
                    return;
                }
            }
        });
    }
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = l.accept().await else { return };
            let Ok(server) = tokio::net::TcpStream::connect(&to).await else { continue };
            let _ = client.set_nodelay(true);
            let _ = server.set_nodelay(true);
            let (cr, cw) = client.into_split();
            let (sr, sw) = server.into_split();
            pipe(cr, sw, delay);
            pipe(sr, cw, delay);
        }
    });
    url
}

/// The copies of each shard a scenario's coordinators connect to: fresh
/// copies of the built shards for each scenario (a coordinator gives the
/// copies a cluster map naming them, so scenarios do not share servers).
struct Fleet {
    /// One entry per shard: its copies' URLs joined by `|`.
    shards: Vec<String>,
    /// The delay line's delay in front of copy 0 of shard 0, if any.
    delay: Arc<AtomicU64>,
}

/// Families of a coordinator's metrics, summed by name and (optionally) a
/// label it must carry.
fn metric(c: &Coordinator, name: &str, label: &str) -> f64 {
    c.metrics().into_iter().filter(|f| f.name == name).flat_map(|f| f.values).filter(|(l, _)| l.contains(label)).map(|(_, v)| v).sum()
}

#[derive(Clone, Copy, Default)]
struct Counters {
    hedges: f64,
    refused: f64,
    errors: f64,
    timeouts: f64,
}

fn counters(c: &Coordinator, refused: &[u64]) -> Counters {
    Counters {
        hedges: metric(c, "chloedb_shard_hedges_total", ""),
        refused: refused.iter().sum::<u64>() as f64,
        errors: metric(c, "chloedb_shard_calls_total", "outcome=\"error\""),
        timeouts: metric(c, "chloedb_shard_calls_total", "outcome=\"timeout\""),
    }
}

/// Per copy of shard 0: answers, refusals and calls in flight, as `c`
/// observed.
async fn shard0(c: &Coordinator) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
    let snap = c.snapshot_within(Duration::ZERO).await;
    let reps = snap["shards"][0]["replicas"].as_array().cloned().unwrap_or_default();
    let get = |k: &str| reps.iter().map(|r| r["observed"]["calls"][k].as_u64().unwrap_or(0)).collect::<Vec<_>>();
    let inflight = reps.iter().map(|r| r["observed"]["inflight"].as_u64().unwrap_or(0)).collect();
    (get("ok"), get("refused"), inflight)
}

/// What one run measured.
struct Run {
    lat: Vec<f64>,
    /// Reads after a write (`ryw`).
    ryw: Vec<f64>,
    /// Mean replicas avoided (in their cool-off), over the run.
    avoided: f64,
    errors: usize,
    secs: f64,
    per_second: Vec<Vec<u64>>,
    inflight: Vec<Vec<f64>>,
}

/// Writes and reads back (`ryw`): the share of operations, and the keys.
#[derive(Clone)]
struct Ryw {
    share: f64,
    keys: Arc<Vec<i64>>,
}

#[allow(clippy::too_many_arguments)]
async fn drive(svcs: &[Arc<Service>], coords: &[Arc<Coordinator>], queries: Arc<Vec<String>>, clients: usize, warmup: f64, seconds: f64, at_third: Option<Arc<AtomicU64>>, ryw: Option<Ryw>) -> (Run, Vec<Counters>, Vec<Vec<u64>>) {
    let stop = Arc::new(AtomicBool::new(false));
    let timing = Arc::new(AtomicBool::new(false));
    let next = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for k in 0..clients {
        let svc = svcs[k % svcs.len()].clone();
        let (stop, timing, next, queries, ryw) = (stop.clone(), timing.clone(), next.clone(), queries.clone(), ryw.clone());
        tasks.push(tokio::spawn(async move {
            let (mut lat, mut back, mut errors) = (Vec::new(), Vec::new(), 0usize);
            let mut rng = Rng(k as u64 + 1);
            while !stop.load(Ordering::Relaxed) {
                let n = next.fetch_add(1, Ordering::Relaxed);
                if let Some(w) = ryw.as_ref().filter(|w| rng.unit() < w.share) {
                    // Write a row, then read it back as the write answered.
                    // Each client its own keys, so a read sees its own write.
                    let key = w.keys[(k + clients * rng.below(0, w.keys.len() / clients.max(1)).max(0)) % w.keys.len()];
                    let body = format!("w{} w{} w{}", n % 50, rng.below(0, 300), rng.below(0, 3000));
                    let Ok(written) = svc.write_ops(vec![json!({"op": "set", "key": key, "field": "body", "value": body})], None).await else {
                        errors += timing.load(Ordering::Relaxed) as usize;
                        continue;
                    };
                    let t = Instant::now();
                    let sql = format!("SELECT id, body FROM t WHERE id = {key}");
                    let ok = match svc.query(QueryReq { sql, after: After::Shards(written.seq_nos, written.seq_terms), ..Default::default() }).await {
                        Ok(a) => a.rs.rows.first().and_then(|r| r.get(1)).and_then(|v| v.as_str()) == Some(body.as_str()),
                        Err(_) => false,
                    };
                    if timing.load(Ordering::Relaxed) {
                        if ok {
                            back.push(t.elapsed().as_secs_f64() * 1e3);
                        } else {
                            errors += 1;
                        }
                    }
                    continue;
                }
                let sql = queries[n % queries.len()].clone();
                let t = Instant::now();
                let ok = svc.query(QueryReq { sql, ..Default::default() }).await.is_ok();
                if timing.load(Ordering::Relaxed) {
                    if ok {
                        lat.push(t.elapsed().as_secs_f64() * 1e3);
                    } else {
                        errors += 1;
                    }
                }
            }
            (lat, back, errors)
        }));
    }
    tokio::time::sleep(Duration::from_secs_f64(warmup)).await;
    let mut before = Vec::new();
    let mut ok0 = Vec::new();
    for c in coords {
        let (ok, refused, _) = shard0(c).await;
        before.push(counters(c, &refused));
        ok0.push(ok);
    }
    timing.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    let (mut per_second, mut inflight) = (Vec::new(), Vec::new());
    let mut last: Vec<u64> = ok0.iter().fold(vec![0; ok0[0].len()], |a, o| a.iter().zip(o).map(|(x, y)| x + y).collect());
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut ticks = 0u64;
    let mut avoided = Vec::new();
    while t0.elapsed().as_secs_f64() < seconds {
        tick.tick().await;
        ticks += 1;
        if let Some(d) = &at_third {
            if t0.elapsed().as_secs_f64() >= seconds / 3.0 {
                d.store(0, Ordering::Relaxed);
            }
        }
        let mut sum = vec![0u64; last.len()];
        let mut fl = Vec::new();
        for c in coords {
            let (ok, _, inf) = shard0(c).await;
            sum = sum.iter().zip(&ok).map(|(a, b)| a + b).collect();
            fl.push(inf.iter().map(|&v| v as f64).collect::<Vec<_>>());
        }
        inflight.push(fl.concat());
        avoided.push(coords.iter().map(|c| metric(c, "chloedb_replicas_avoided", "")).sum::<f64>());
        if ticks % 4 == 0 {
            per_second.push(sum.iter().zip(&last).map(|(a, b)| a - b).collect());
            last = sum;
        }
    }
    timing.store(false, Ordering::Relaxed);
    let secs = t0.elapsed().as_secs_f64();
    let mut after = Vec::new();
    let mut ok1 = Vec::new();
    for c in coords {
        let (ok, refused, _) = shard0(c).await;
        after.push(counters(c, &refused));
        ok1.push(ok);
    }
    stop.store(true, Ordering::Relaxed);
    let (mut lat, mut ryw_lat, mut errors) = (Vec::new(), Vec::new(), 0);
    for t in tasks {
        let (l, b, e) = t.await.unwrap();
        lat.extend(l);
        ryw_lat.extend(b);
        errors += e;
    }
    let avoided = avoided.iter().sum::<f64>() / avoided.len().max(1) as f64;
    let delta: Vec<Counters> = before
        .iter()
        .zip(&after)
        .map(|(b, a)| Counters { hedges: a.hedges - b.hedges, refused: a.refused - b.refused, errors: a.errors - b.errors, timeouts: a.timeouts - b.timeouts })
        .collect();
    let answers: Vec<Vec<u64>> = ok0.iter().zip(&ok1).map(|(b, a)| a.iter().zip(b).map(|(x, y)| x - y).collect()).collect();
    (Run { lat, ryw: ryw_lat, avoided, errors, secs, per_second, inflight }, delta, answers)
}

fn summary(run: &Run) -> Json {
    let pct = |v: &[f64]| {
        let mut l = v.to_vec();
        l.sort_by(f64::total_cmp);
        let p = |q: f64| if l.is_empty() { Json::Null } else { json!((percentile(&l, q) * 100.0).round() / 100.0) };
        (l.len(), p(50.0), p(90.0), p(99.0), p(99.9), l.last().map(|v| (v * 100.0).round() / 100.0))
    };
    let (n, p50, p90, p99, p999, max) = pct(&run.lat);
    let (rn, r50, _, r99, _, rmax) = pct(&run.ryw);
    json!({
        "queries": n,
        "qps": (n as f64 / run.secs).round(),
        "p50_ms": p50, "p90_ms": p90, "p99_ms": p99, "p999_ms": p999, "max_ms": max,
        "reads_after_write": rn, "raw_p50_ms": r50, "raw_p99_ms": r99, "raw_max_ms": rmax,
        "errors": run.errors,
        "avoided_mean": (run.avoided * 100.0).round() / 100.0,
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let a = Args::parse();
    let tmp = match &a.build_only {
        Some(d) => d.clone(),
        None => a.tmp.clone().unwrap_or_else(std::env::temp_dir).join(format!("chloedb-route-bench-{}", std::process::id())),
    };
    std::fs::create_dir_all(&tmp).unwrap();
    let schema = Schema::new("t", vec![Field::new("id", FieldType::Int), Field::new("body", FieldType::Text { positions: false })], Some("id")).unwrap();
    let opts = || DbOptions { sync: false, publish_snapshots: true, ..DbOptions::default() };
    let cum: Vec<f64> = (1..=WORDS)
        .scan(0.0, |s, r| {
            *s += 1.0 / r as f64;
            Some(*s)
        })
        .collect();

    // Build each shard once, then copy it.
    let t = Instant::now();
    if a.external.is_none() {
        let dbs: Vec<Db> = (0..a.shards)
            .map(|i| {
                let d = tmp.join(format!("shard{i}/c0"));
                std::fs::create_dir_all(&d).unwrap();
                Db::create(&d.join("db.strata"), schema.clone(), opts()).unwrap()
            })
            .collect();
        let mut rng = Rng(7);
        for chunk in (0..a.rows).collect::<Vec<_>>().chunks(100_000) {
            let mut split: Vec<Vec<Vec<Value>>> = vec![Vec::new(); a.shards];
            for &i in chunk {
                let body: Vec<String> = (0..BODY).map(|_| format!("w{}", zipf(&mut rng, &cum))).collect();
                let key = Value::Int(i as i64);
                split[shard_of(&key, a.shards)].push(vec![key, Value::Str(body.join(" "))]);
            }
            for (db, rows) in dbs.iter().zip(split) {
                db.load(rows).unwrap();
            }
        }
        for db in &dbs {
            db.storage().start_seq_no().unwrap();
            db.storage().flush().unwrap();
        }
    }
    if a.external.is_none() {
        eprintln!("{} rows in {} shards x {} copies, built in {:.1} s", a.rows, a.shards, a.copies, t.elapsed().as_secs_f64());
    }
    if let Some(d) = &a.build_only {
        eprintln!("built in {}", d.display());
        return;
    }

    // Serves fresh copies for a scenario: copy 0 of shard 0 with few
    // slots (`busy`) or behind the delay line (the slow ones).
    let fleet_for = |scenario: String| {
        let (tmp, schema, a_shards, a_copies, busy_slots, lag) = (tmp.clone(), schema.clone(), a.shards, a.copies, a.busy_slots, Duration::from_millis(a.lag_ms));
        let (external, late) = (a.external.clone(), a.late_link.clone());
        async move {
            let delay = Arc::new(AtomicU64::new(0));
            let mut shards = Vec::new();
            if let Some(ext) = external {
                // Copies served elsewhere: copy 0 of shard 0 behind the
                // delay line for the slow scenarios; the late link for ryw.
                if let Some((listen, to)) = late.as_deref().and_then(|l| l.split_once('=')) {
                    late_link_on(listen, to, lag).await;
                }
                // Every copy behind a delay line, so that each takes the
                // same hop and only copy 0's delay differs (a hop in front
                // of one alone makes it look slower to least-cost).
                let none = Arc::new(AtomicU64::new(0));
                let slowed = matches!(scenario.as_str(), "slow" | "slowhedge" | "recover" | "two" | "stall");
                for (i, shard) in ext.split(',').enumerate() {
                    let mut copies = Vec::new();
                    for (k, url) in shard.split('|').enumerate() {
                        let d = if slowed && i == 0 && k == 0 { delay.clone() } else { none.clone() };
                        copies.push(delay_line(url.to_string(), d).await);
                    }
                    shards.push(copies.join("|"));
                }
                if scenario == "stall" {
                    let d = delay.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_millis(950)).await;
                            d.store(60, Ordering::Relaxed);
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            d.store(0, Ordering::Relaxed);
                        }
                    });
                }
                return Fleet { shards, delay };
            }
            if scenario == "ryw" {
                // Shard 0 alone: two replicas and a primary shipping to them,
                // replica 1 over a late link.
                let open = |k: &str| {
                    let d = tmp.join(format!("ryw/{k}"));
                    copy_dir(&tmp.join("shard0/c0"), &d);
                    Arc::new(Db::open(&d.join("db.strata"), DbOptions { ship_log_bytes: 64 << 20, ..opts() }).unwrap())
                };
                let serve = |svc: Arc<Service>| async move {
                    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let url = format!("http://{}", l.local_addr().unwrap());
                    tokio::spawn(server::serve_grpc(svc, l, Default::default()));
                    url
                };
                let (mut urls, mut replicas) = (Vec::new(), Vec::new());
                for k in ["r0", "r1"] {
                    let db = open(k);
                    let mut svc = Service::new(Backend::Local(db.clone()), schema.clone(), agent(), SLOTS, Duration::from_secs(30));
                    svc.set_replication(Replication::Replica(Arc::new(Follow::open(db, None).unwrap())));
                    let svc = Arc::new(svc);
                    urls.push(serve(svc.clone()).await);
                    replicas.push(svc);
                }
                let late = late_link(&urls[1], lag).await;
                let po = PrimaryOptions { replicas: vec![urls[0].clone(), late], acks: Acks::Primary, heartbeat: Duration::from_millis(100), retry_max: Duration::from_millis(200), ..Default::default() };
                let pdb = open("p");
                let mut psvc = Service::new(Backend::Local(pdb.clone()), schema.clone(), agent(), SLOTS, Duration::from_secs(30));
                psvc.set_replication(Replication::Primary(Primary::start(pdb, None, po).unwrap()));
                let pu = serve(Arc::new(psvc)).await;
                for r in &replicas {
                    let t0 = Instant::now();
                    while r.health().await["replication"]["serves_reads"] != true {
                        assert!(t0.elapsed() < Duration::from_secs(60), "the replicas serve reads");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                shards.push(format!("{}|{}|{pu}", urls[0], urls[1]));
                return Fleet { shards, delay };
            }
            for i in 0..a_shards {
                let mut copies = Vec::new();
                for k in 0..a_copies {
                    let d = tmp.join(format!("{scenario}/shard{i}/c{k}"));
                    copy_dir(&tmp.join(format!("shard{i}/c0")), &d);
                    let slots = if scenario == "busy" && i == 0 && k == 0 { busy_slots } else { SLOTS };
                    let db = Db::open(&d.join("db.strata"), opts()).unwrap();
                    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let mut url = format!("http://{}", l.local_addr().unwrap());
                    tokio::spawn(server::serve_grpc(service(Backend::Local(Arc::new(db)), schema.clone(), slots), l, Default::default()));
                    if matches!(scenario.as_str(), "slow" | "slowhedge" | "recover" | "two" | "stall") && i == 0 && k == 0 {
                        url = delay_line(url, delay.clone()).await;
                    }
                    copies.push(url);
                }
                shards.push(copies.join("|"));
            }
            if scenario == "stall" {
                // Each second, 50 ms of answers held 60 ms.
                let d = delay.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(950)).await;
                        d.store(60, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        d.store(0, Ordering::Relaxed);
                    }
                });
            }
            Fleet { shards, delay }
        }
    };

    // Two words each, one of ranks 1-20 and one of 20-300: long posting
    // lists, a few ms.
    let mut rng = Rng(11);
    let queries: Arc<Vec<String>> = Arc::new(
        (0..500)
            .map(|_| {
                let q = format!("w{} w{}", rng.below(0, 20), rng.below(20, 300));
                format!("SELECT id FROM t WHERE MATCH(body, '{q}') ORDER BY bm25(body, '{q}') DESC LIMIT 10")
            })
            .collect(),
    );

    let base = CoordOptions { stats_cache_terms: a.stats_cache_terms.unwrap_or(CoordOptions::default().stats_cache_terms), ..Default::default() };
    let rr = CoordOptions { route: Route::RoundRobin, ..base.clone() };
    let lc = CoordOptions { route: Route::LeastCost, ..base.clone() };
    let configs = |scenario: &str| -> Vec<(&'static str, CoordOptions)> {
        let adaptive = |o: &CoordOptions| CoordOptions { hedge_adaptive: true, ..o.clone() };
        match scenario {
            "equal" | "stall" => vec![("round-robin", rr.clone()), ("round-robin adaptive", adaptive(&rr)), ("least-cost", lc.clone()), ("least-cost adaptive", adaptive(&lc))],
            "busy" => vec![("round-robin", rr.clone()), ("least-cost", lc.clone()), ("least-cost no-load", CoordOptions { route_load: false, ..lc.clone() })],
            "ryw" => vec![("round-robin", rr.clone()), ("least-cost", lc.clone()), ("least-cost prefer", CoordOptions { prefer_caught_up: true, ..lc.clone() })],
            _ => vec![("round-robin", rr.clone()), ("least-cost", lc.clone())],
        }
    };
    // Keys of shard 0, written by `ryw`.
    let ryw_keys: Arc<Vec<i64>> = Arc::new((0..a.rows.min(30_000) as i64).filter(|&k| shard_of(&Value::Int(k), a.shards) == 0).collect());
    let mut report = Vec::new();
    let mut md = String::new();
    md.push_str(&format!("Machine: {}\n\n", machine()));
    for scenario in &a.scenarios {
        let fleet = fleet_for(scenario.clone()).await;
        let concurrency: Vec<usize> = match scenario.as_str() {
            "recover" => vec![16],
            "two" => vec![32],
            _ => a.concurrency.clone(),
        };
        let seconds = if scenario == "recover" { a.seconds * 3.0 } else { a.seconds };
        md.push_str(&format!("\n### {scenario}\n\n| clients | config | round | qps | p50 ms | p90 ms | p99 ms | p99.9 ms | max ms | read-after-write p50 / p99 / max ms | errors | hedges | 429s | timeouts | avoided | shard 0 answers by copy |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n"));
        for &clients in &concurrency {
            for round in 0..a.rounds {
                for (label, opts) in configs(scenario) {
                    fleet.delay.store(if scenario == "slowhedge" { a.hedge_delay_ms } else { a.delay_ms }, Ordering::Relaxed);
                    let n = if scenario == "two" { 2 } else { 1 };
                    let mut coords = Vec::new();
                    let mut svcs = Vec::new();
                    for _ in 0..n {
                        let c = Arc::new(Coordinator::connect_with(fleet.shards.clone(), opts.clone()).await.unwrap_or_else(|e| panic!("{}", e.message)));
                        let s = service(Backend::Cluster(c.clone()), schema.clone(), 1024);
                        server::spawn_watch(&s);
                        coords.push(c);
                        svcs.push(s);
                    }
                    let third = (scenario == "recover").then(|| fleet.delay.clone());
                    let ryw = (scenario == "ryw").then(|| Ryw { share: a.ryw_share, keys: ryw_keys.clone() });
                    let (run, delta, answers) = drive(&svcs, &coords, queries.clone(), clients, a.warmup, seconds, third, ryw).await;
                    let s = summary(&run);
                    let snap = coords[0].snapshot_within(Duration::ZERO).await;
                    let observed: Vec<Json> = snap["shards"][0]["replicas"].as_array().cloned().unwrap_or_default().iter().map(|r| r["observed"].clone()).collect();
                    let sum = |f: fn(&Counters) -> f64| delta.iter().map(f).sum::<f64>();
                    let mean_inflight: Vec<f64> = if run.inflight.is_empty() {
                        Vec::new()
                    } else {
                        (0..run.inflight[0].len()).map(|k| (run.inflight.iter().map(|v| v[k]).sum::<f64>() / run.inflight.len() as f64 * 100.0).round() / 100.0).collect()
                    };
                    let row = json!({
                        "scenario": scenario, "clients": clients, "config": label, "route": opts.route.name(), "hedge_adaptive": opts.hedge_adaptive, "prefer_caught_up": opts.prefer_caught_up, "route_load": opts.route_load, "round": round,
                        "client": s, "hedges": sum(|c| c.hedges), "refused_429": sum(|c| c.refused),
                        "shard_errors": sum(|c| c.errors), "shard_timeouts": sum(|c| c.timeouts),
                        "shard0_answers_by_copy": answers, "shard0_per_second": run.per_second,
                        "shard0_mean_inflight_by_coordinator_copy": mean_inflight,
                        "shard0_observed_by_copy": observed,
                    });
                    let line = format!(
                        "| {clients} | {label} | {round} | {} | {} | {} | {} | {} | {} | {} / {} / {} | {} | {} | {} | {} | {} | {:?} |\n",
                        s["qps"],
                        s["p50_ms"],
                        s["p90_ms"],
                        s["p99_ms"],
                        s["p999_ms"],
                        s["max_ms"],
                        s["raw_p50_ms"],
                        s["raw_p99_ms"],
                        s["raw_max_ms"],
                        s["errors"],
                        sum(|c| c.hedges),
                        sum(|c| c.refused),
                        sum(|c| c.timeouts),
                        s["avoided_mean"],
                        answers
                    );
                    eprint!("{scenario} {line}");
                    if scenario == "recover" {
                        eprintln!("  per second: {:?}", run.per_second);
                    }
                    if scenario == "ryw" {
                        for (k, o) in observed.iter().enumerate() {
                            eprintln!("  copy {k}: plain {} after-write {} held {} tail {}", o["plain_rtt_ms"], o["after_write_rtt_ms"], o["held"], o["tail_ms"]);
                        }
                    }
                    if scenario == "two" {
                        eprintln!("  mean in flight (coordinator x copy): {mean_inflight:?}");
                    }
                    md.push_str(&line);
                    report.push(row);
                    drop(svcs);
                    drop(coords);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }
    std::fs::create_dir_all(&a.out).unwrap();
    let out = json!({
        "machine": machine(),
        "args": {"rows": a.rows, "shards": a.shards, "copies": a.copies, "seconds": a.seconds, "warmup": a.warmup, "rounds": a.rounds, "delay_ms": a.delay_ms, "hedge_delay_ms": a.hedge_delay_ms, "busy_slots": a.busy_slots, "stats_cache_terms": a.stats_cache_terms.unwrap_or(CoordOptions::default().stats_cache_terms), "slots": SLOTS},
        "coordinator": "CoordOptions::default() with each run's config set: hedge 100 ms, kept BM25 statistics",
        "lag_ms": a.lag_ms, "ryw_share": a.ryw_share,
        "runs": report,
    });
    std::fs::write(a.out.join("route.json"), serde_json::to_string_pretty(&out).unwrap()).unwrap();
    std::fs::write(a.out.join("route.md"), &md).unwrap();
    println!("{md}");
    if a.external.is_none() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
