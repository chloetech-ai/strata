//! Runs the cluster simulator ([`strata_server::sim`]) over seeds.
//!
//! ```text
//! strata-sim --seeds 0..10000          # every seed, on every core; prints those that fail
//! strata-sim --seed 1234 --trace       # one seed, and what happened in it
//! strata-sim --seeds 0..2000 --bug asks-a-known-term   # a bug it found, put back
//! strata-sim --cluster --seeds 0..1000 # a cluster's map changing and slots moving
//! strata-sim --cluster --racing        # both coordinators changing the map
//! ```
//!
//! A seed that fails is run again to check that it replays exactly (the
//! same fingerprint), so that `--seed N --trace` shows the same run.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use strata_server::sim::{self, cluster, Bug, Config};

/// What a run of either world found.
struct Found {
    seed: u64,
    violations: Vec<String>,
    stats: String,
    events: u64,
    fingerprint: u64,
    trace: Vec<String>,
}

impl Found {
    fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

impl From<sim::Report> for Found {
    fn from(r: sim::Report) -> Found {
        Found { seed: r.seed, violations: r.violations, stats: format!("{:?}", r.stats), events: r.events, fingerprint: r.fingerprint, trace: r.trace }
    }
}

impl From<cluster::Report> for Found {
    fn from(r: cluster::Report) -> Found {
        Found { seed: r.seed, violations: r.violations, stats: format!("{:?}", r.stats), events: r.events, fingerprint: r.fingerprint, trace: r.trace }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut seeds = 0..1000u64;
    let mut trace = false;
    let mut jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut verbose = false;
    let mut bugs = Vec::new();
    let mut cluster_bugs = Vec::new();
    let mut promote_ppm = None;
    let mut drop_ppm = None;
    let mut world_cluster = false;
    let mut racing = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                let s: u64 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
                seeds = s..s + 1;
                i += 1;
            }
            "--seeds" => {
                let (a, b) = args.get(i + 1).and_then(|s| s.split_once("..")).unwrap_or_else(|| usage());
                seeds = a.parse().unwrap_or_else(|_| usage())..b.parse().unwrap_or_else(|_| usage());
                i += 1;
            }
            "--jobs" => {
                jobs = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
                i += 1;
            }
            "--bug" => {
                let name = args.get(i + 1).cloned().unwrap_or_else(|| usage());
                match (Bug::of(&name), cluster::Bug::of(&name)) {
                    (Some(b), _) => bugs.push(b),
                    (_, Some(b)) => cluster_bugs.push(b),
                    _ => usage(),
                }
                i += 1;
            }
            "--promote-ppm" => {
                promote_ppm = Some(args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()));
                i += 1;
            }
            "--drop-ppm" => {
                drop_ppm = Some(args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or_else(|| usage()));
                i += 1;
            }
            "--cluster" => world_cluster = true,
            "--racing" => racing = true,
            "--trace" => trace = true,
            "--verbose" | "-v" => verbose = true,
            _ => usage(),
        }
        i += 1;
    }
    let config = |s: u64| {
        let c = Config::of(s);
        Config { bugs: bugs.clone(), promote_ppm: promote_ppm.unwrap_or(c.promote_ppm), drop_ppm: drop_ppm.unwrap_or(c.drop_ppm), ..c }
    };
    let cluster_config = |s: u64| {
        let c = cluster::Config::of(s);
        cluster::Config { racing, bugs: cluster_bugs.clone(), drop_ppm: drop_ppm.unwrap_or(c.drop_ppm), ..c }
    };
    let run = |s: u64, trace: bool| -> Found {
        match world_cluster {
            true => cluster::run(cluster_config(s), trace).into(),
            false => sim::run(config(s), trace).into(),
        }
    };
    let describe = |s: u64| match world_cluster {
        true => format!("{:?}", cluster_config(s)),
        false => format!("{:?}", config(s)),
    };
    if trace {
        for s in seeds {
            let r = run(s, true);
            println!("seed {s}: {}", describe(s));
            for line in &r.trace {
                println!("{line}");
            }
            print_report(&r);
        }
        return;
    }
    let next = AtomicU64::new(seeds.start);
    let failed = Mutex::new(Vec::new());
    let started = std::time::Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| loop {
                let s = next.fetch_add(1, Ordering::Relaxed);
                if s >= seeds.end {
                    return;
                }
                let r = run(s, false);
                if verbose {
                    println!("seed {s}: {} events, {}", r.events, r.stats);
                }
                if !r.ok() {
                    let again = run(s, false);
                    let replays = again.fingerprint == r.fingerprint;
                    print_report(&r);
                    if !replays {
                        println!("  seed {s} does NOT replay: fingerprints {:x} and {:x}", r.fingerprint, again.fingerprint);
                    }
                    failed.lock().unwrap().push(s);
                }
            });
        }
    });
    let failed = failed.into_inner().unwrap();
    println!("{} seeds, {} failed, in {:.1} s{}", seeds.end - seeds.start, failed.len(), started.elapsed().as_secs_f64(), if failed.is_empty() { String::new() } else { format!(": {failed:?}") });
    if !failed.is_empty() {
        std::process::exit(1);
    }
}

fn print_report(r: &Found) {
    if r.ok() {
        println!("seed {}: ok, {} events, {}", r.seed, r.events, r.stats);
        return;
    }
    println!("seed {}: {} violations, {} events, {}", r.seed, r.violations.len(), r.events, r.stats);
    for v in &r.violations {
        println!("  {v}");
    }
}

fn usage() -> ! {
    let bugs: Vec<&str> = Bug::ALL.iter().map(|b| b.name()).chain(cluster::Bug::ALL.iter().map(|b| b.name())).collect();
    eprintln!("usage: strata-sim [--seeds A..B | --seed N] [--trace] [--jobs N] [--verbose] [--promote-ppm N] [--drop-ppm N] [--bug {}] [--cluster [--racing]]", bugs.join(" | "));
    std::process::exit(2)
}
