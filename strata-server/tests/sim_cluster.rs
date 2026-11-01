//! The cluster simulator's second world ([`strata_server::sim::cluster`]):
//! seeded runs of a cluster whose map changes and whose slots move while it
//! serves keep the cluster's promises, a seed replays exactly, and each bug
//! the simulator found in map changes and moves, put back, is found again.
//!
//! `STRATA_SIM_CLUSTER_SEEDS=A..B` runs those seeds instead of the first
//! few hundred; `strata-sim --cluster --seed N --trace` shows what happened
//! in one. Two coordinators changing the map at once (`--racing`) can still
//! fork it, which only one place ordering the changes ends: their seeds are
//! not checked here, but for a bug they find.

use std::ops::Range;

use strata_server::sim::cluster::{self, Bug, Config, Report};

fn jobs() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

fn seeds() -> Range<u64> {
    let Ok(s) = std::env::var("STRATA_SIM_CLUSTER_SEEDS") else { return 0..400 };
    let (a, b) = s.split_once("..").expect("STRATA_SIM_CLUSTER_SEEDS is A..B");
    a.parse().expect("a seed")..b.parse().expect("a seed")
}

fn explain(failed: &[Report]) -> String {
    let mut out = String::new();
    for r in failed {
        out += &format!("\nseed {} (strata-sim --cluster --seed {} --trace):", r.seed, r.seed);
        for v in &r.violations {
            out += &format!("\n  {v}");
        }
    }
    out
}

#[test]
fn seeded_cluster_runs_keep_the_maps_promises() {
    let seeds = seeds();
    let failed = cluster::run_seeds(seeds.clone(), &Config::of, jobs());
    assert!(failed.is_empty(), "{} of seeds {seeds:?} broke a promise:{}", failed.len(), explain(&failed));
}

#[test]
fn a_cluster_seed_replays_exactly() {
    for seed in 0..4 {
        let (a, b) = (cluster::run(Config::of(seed), true), cluster::run(Config::of(seed), true));
        assert_eq!(a.fingerprint, b.fingerprint, "seed {seed}");
        assert_eq!(a.events, b.events, "seed {seed}");
        assert_eq!(a.trace, b.trace, "seed {seed}");
        assert!(a.events > 1000 && a.stats.writes_ok > 0 && a.stats.reads_ok > 0 && a.stats.changes_made > 0, "seed {seed} did little: {:?}", a.stats);
    }
    assert_ne!(cluster::run(Config::of(0), false).fingerprint, cluster::run(Config::of(1), false).fingerprint);
    let moved: u64 = (0..20).map(|s| cluster::run(Config::of(s), false).stats.moves_done).sum();
    assert!(moved > 0, "no slots moved in 20 seeds");
}

/// The first seed below `within` that breaks a promise with `bug` put
/// back, looking a hundred seeds at a time.
fn finds(bug: Bug, within: u64) -> Option<Report> {
    let config = |s| Config { bugs: vec![bug], ..Config::of(s) };
    (0..within).step_by(100).find_map(|a| cluster::run_seeds(a..(a + 100).min(within), &config, jobs()).into_iter().next())
}

#[test]
fn a_flip_of_slots_whose_writes_were_not_held_is_found_again() {
    let r = finds(Bug::FlipsAnUnheldMove, 1000).expect("a seed that loses writes to moved slots");
    assert!(r.violations.iter().any(|v| v.contains("a stale read")), "{:?}", r.violations);
}

#[test]
fn a_late_copy_of_a_move_given_up_is_found_again() {
    let r = finds(Bug::TakesLateCopies, 1500).expect("a seed with a key on two shards");
    assert!(r.violations.iter().any(|v| v.contains("on 2 shards")), "{:?}", r.violations);
}

#[test]
fn a_change_given_around_the_source_of_a_move_is_found_again() {
    let r = finds(Bug::ChangesAroundTheSource, 1500).expect("a seed whose map forks while slots copy");
    assert!(r.stats.split_epochs > 0 && r.violations.iter().any(|v| v.contains("misses write")), "{:?} {:?}", r.violations, r.stats);
}

/// Found only with both coordinators changing the map: a seed that breaks
/// a promise with the bug put back, and keeps them all without it.
#[test]
fn routing_by_a_map_no_server_took_is_found_again() {
    let racing = |s| Config { racing: true, ..Config::of(s) };
    let with = |s| Config { bugs: vec![Bug::RoutesByAMapNoServerTook], ..racing(s) };
    let found = (0..1500).step_by(100).find_map(|a| cluster::run_seeds(a..a + 100, &with, jobs()).into_iter().find(|r| cluster::run(racing(r.seed), false).ok()));
    let r = found.expect("a seed of racing coordinators that only the bug breaks");
    assert!(!r.violations.is_empty(), "{:?}", r.stats);
}
