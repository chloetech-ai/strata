//! The cluster simulator ([`chloedb_server::sim`]): seeded runs of a
//! shard's copies and its coordinators under faults keep the protocol's
//! promises, a seed replays exactly, and each bug the simulator found, put
//! back, is found again.
//!
//! `CHLOEDB_SIM_SEEDS=A..B` runs those seeds instead of the first few
//! hundred; `chloedb-sim --seed N --trace` shows what happened in one.

use std::ops::Range;

use chloedb_server::sim::{self, Bug, Config};

fn jobs() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

fn seeds() -> Range<u64> {
    let Ok(s) = chloedb::env::var("CHLOEDB_SIM_SEEDS") else { return 0..400 };
    let (a, b) = s.split_once("..").expect("CHLOEDB_SIM_SEEDS is A..B");
    a.parse().expect("a seed")..b.parse().expect("a seed")
}

fn explain(failed: &[sim::Report]) -> String {
    let mut out = String::new();
    for r in failed {
        out += &format!("\nseed {} (chloedb-sim --seed {} --trace):", r.seed, r.seed);
        for v in &r.violations {
            out += &format!("\n  {v}");
        }
    }
    out
}

#[test]
fn seeded_runs_keep_the_protocols_promises() {
    let seeds = seeds();
    let failed = sim::run_seeds(seeds.clone(), &Config::of, jobs());
    assert!(failed.is_empty(), "{} of seeds {seeds:?} broke a promise:{}", failed.len(), explain(&failed));
}

/// Seeds the nightly run found breaking a promise on `main`, kept here
/// once fixed. 624036738 (the night of 10 November 2026, main at
/// 768197e0): the primary elected for term 2 lacked 34 writes that the
/// primary of term 1 had acknowledged. Its trace there: s0 sends s1 a
/// copy ending at transaction 60 at 4,601 ms, which a held-up call
/// delivers at 5,906 ms; s1 meanwhile installed a newer copy (ending at
/// 61) and applied and acknowledged 62 to 94; the late copy dropped them,
/// and s1 won term 2 at 8,301 ms (`Bug::InstallsALateCopy`, now refused).
/// From release 0.2.0 on, the seed still meets the late copy, but
/// with the bug put back s0 sends s1 a third copy, ending at 94, 6 ms
/// before the network cuts s0 off, and nothing is lost: it passed by that
/// timing. The tests of late copies below find the bug again.
///
/// 624144435 (found on 14 November 2026, main at 7984b237, and
/// 0.3.2) was the checker's mistake, not the protocol's: s1, promoted by
/// hand at 67 ms, waited the call's timeout (10 s) for the answer of s0,
/// cut off, and won term 2 at 10,084 ms with votes given at 80 ms, after
/// terms 3 and 4 had begun; s2, the primary of term 4, had acknowledged a
/// client's retry of write 12 at 9,921 ms as transaction 10 of term 1,
/// which s1 lacked. The checker asked a primary of term 2 to hold every
/// write acknowledged as a transaction of an older term, rather than by a
/// primary of an older term: s1 served nothing, as Raft's stale leader
/// cannot, and stepped down 20 ms later, on its first call to s2.
const FOUND: &[u64] = &[624036738, 624144435];

#[test]
fn seeds_the_nightly_run_found_keep_the_protocols_promises() {
    let failed: Vec<sim::Report> = FOUND.iter().flat_map(|&s| sim::run_seeds(s..s + 1, &Config::of, 1)).collect();
    assert!(failed.is_empty(), "{} of the seeds found before broke a promise again:{}", failed.len(), explain(&failed));
}

/// Copies of the primary arriving late, after a newer copy and the
/// transactions that follow it (`Config::late_copies`): each is refused,
/// and no seed loses a write or fails to settle.
#[test]
fn late_copies_are_refused_and_lose_nothing() {
    let late = std::sync::atomic::AtomicU64::new(0);
    let failed = std::sync::Mutex::new(Vec::new());
    sim::on_seeds(0..400, jobs(), &|seed| {
        let r = sim::run(Config::late_copies(seed), false);
        late.fetch_add(r.stats.late_copies, std::sync::atomic::Ordering::Relaxed);
        if !r.ok() {
            failed.lock().unwrap().push(r);
        }
    });
    let failed = failed.into_inner().unwrap();
    assert!(failed.is_empty(), "{} of 400 seeds with late copies broke a promise:{}", failed.len(), explain(&failed));
    let late = late.into_inner();
    assert!(late > 50, "only {late} copies arrived late in 400 seeds: the setup no longer tests them");
}

/// Finding E-0g (seed 624036738 of the nightly run): a replica that
/// installs a copy of its primary ending before transactions of the
/// primary's term it holds since drops writes it acknowledged, and an
/// election it then wins loses them.
#[test]
fn a_late_copy_installed_is_found_again() {
    let r = finds_with(Bug::InstallsALateCopy, 1000, Config::late_copies).expect("a seed that loses an acknowledged write to a late copy");
    assert!(r.violations.iter().any(|v| v.contains("is not held by")), "{:?}", r.violations);
}

#[test]
fn a_seed_replays_exactly() {
    for seed in 0..4 {
        let (a, b) = (sim::run(Config::of(seed), true), sim::run(Config::of(seed), true));
        assert_eq!(a.fingerprint, b.fingerprint, "seed {seed}");
        assert_eq!(a.events, b.events, "seed {seed}");
        assert_eq!(a.trace, b.trace, "seed {seed}");
        assert!(a.events > 1000 && a.stats.writes_ok > 0 && a.stats.reads_ok > 0, "seed {seed} did little: {:?}", a.stats);
    }
    assert_ne!(sim::run(Config::of(0), false).fingerprint, sim::run(Config::of(1), false).fingerprint);
}

/// The first seed below `within` that breaks a promise with `bug` put
/// back, looking a hundred seeds at a time, each seed's configuration
/// from `of`.
fn finds_with(bug: Bug, within: u64, of: impl Fn(u64) -> Config + Sync) -> Option<sim::Report> {
    let config = |s| {
        let mut c = of(s);
        c.bugs.push(bug);
        c
    };
    (0..within).step_by(100).find_map(|a| sim::run_seeds(a..(a + 100).min(within), &config, jobs()).into_iter().next())
}

// A promotion by hand that takes writes inside the replaced primary's
// lease (`Bug::TakesWritesInALease`) broke a read in one seed of 20,000,
// and in one of 5,000 at `--promote-ppm 100000` with pauses and slow links
// in every seed: too few for a test here, the unit tests of `cores::shard`
// check its fix.

#[test]
fn a_stalled_election_is_found_again() {
    // With elections that wait for no copy they do not need, and replicas
    // that wait after a vote, a minority votes for a candidate that loses
    // far less often: none of 12,000 seeds stalls with only this bug put
    // back. The server had both when the simulator found it.
    let of = |s| Config { bugs: vec![Bug::ReplacesTheCopyItElected], ..Config::of(s) };
    let r = finds_with(Bug::AsksAKnownTerm, 2000, of).expect("a seed that never elects a primary");
    assert!(r.violations.iter().any(|v| v.contains("0 primaries serve")), "{:?}", r.violations);
}

#[test]
fn a_replica_refusing_an_older_term_is_found_again() {
    // The bug bites a copy that began a term in which its primary wrote
    // nothing: since each new primary commits a transaction of its term as
    // it begins it (finding DS-5), only with that put back too
    // (`Bug::AcksAnOlderTerm`), which loses writes of its own; the seed
    // looked for is one that does not settle.
    let of = |s| Config { bugs: vec![Bug::RefusesOlderTerms, Bug::AcksAnOlderTerm], ..Config::of(s) };
    let unsettled = (0..3000).step_by(100).find_map(|a| sim::run_seeds(a..a + 100, &of, jobs()).into_iter().find(|r| r.violations.iter().any(|v| v.contains("did not settle"))));
    assert!(unsettled.is_some(), "a seed whose replicas never catch up");
}

/// Calls dropped 15% of the time (`--drop-ppm 150000`), where a first
/// primary may stop before any replica heard from it.
fn lossy(s: u64) -> Config {
    Config { drop_ppm: 150_000, ..Config::of(s) }
}

#[test]
fn a_first_primary_lost_before_its_first_call_is_found_again() {
    let r = finds_with(Bug::WaitsForAFirstCall, 1000, lossy).expect("a seed whose first copies never elect a primary");
    assert!(r.violations.iter().any(|v| v.contains("0 primaries serve")), "{:?}", r.violations);
}

#[test]
fn a_shard_whose_calls_are_lost_elects_a_primary() {
    let failed = sim::run_seeds(0..400, &lossy, jobs());
    assert!(failed.is_empty(), "{} of 400 seeds at --drop-ppm 150000 broke a promise:{}", failed.len(), explain(&failed));
}

/// Seed `seed` with no drawn faults: its first primary (copy 0) is cut
/// off from every other node from 3 s for a minute, as the chaos monkey's
/// `primary-isolated` does.
fn primary_cut_off(seed: u64) -> Config {
    use std::time::Duration;
    let c = Config::of(seed);
    Config {
        copies: 3,
        crash_every: Duration::ZERO,
        cut_every: Duration::ZERO,
        pause_every: Duration::ZERO,
        slow_every: Duration::ZERO,
        drop_ppm: 0,
        promote_ppm: 0,
        faults_for: Duration::from_secs(66),
        isolate: vec![(0, Duration::from_secs(3), Duration::from_secs(60))],
        ..c
    }
}

/// The most terms a seed of `seeds` began while its primary was cut off,
/// and that seed, with `bugs` put back; each run keeps the protocol's
/// promises.
fn most_elections(seeds: Range<u64>, bugs: &[Bug]) -> (usize, u64) {
    let runs = std::sync::Mutex::new((0, 0));
    sim::on_seeds(seeds, jobs(), &|seed| {
        let r = sim::run(Config { bugs: bugs.to_vec(), ..primary_cut_off(seed) }, false);
        assert!(r.ok(), "seed {seed}: {:?}", r.violations);
        let cut = 3_000_000..63_000_000;
        let terms = r.stats.began_at.iter().filter(|t| cut.contains(*t)).count();
        let mut worst = runs.lock().unwrap();
        *worst = (*worst).max((terms, seed));
    });
    runs.into_inner().unwrap()
}

#[test]
fn a_primary_cut_off_is_replaced_once() {
    let (terms, seed) = most_elections(0..300, &[]);
    assert!(terms <= 1, "seed {seed} began {terms} terms while its primary was cut off");
}

#[test]
fn replicas_replacing_each_other_are_found_again() {
    let (terms, seed) = most_elections(0..300, &[Bug::ReplacesTheCopyItElected]);
    assert!(terms > 5, "seed {seed} began only {terms} terms while its primary was cut off");
}

/// Crashes every 2 s on average, whose clients retry each write that
/// failed (as a client with an idempotency key does).
fn crashy(s: u64) -> Config {
    Config { crash_every: std::time::Duration::from_secs(2), ..Config::of(s) }
}

/// Finding DS-5 (Raft's figure 8): a primary elected that commits
/// nothing of its own term answers a client's retry of a write it holds
/// from an older term once a majority holds it, dies, and a copy holding
/// a newer term's transaction at the same number is elected and drops
/// the acknowledged write.
#[test]
fn an_older_terms_write_acknowledged_by_count_is_found_again() {
    let r = finds_with(Bug::AcksAnOlderTerm, 3000, crashy).expect("a seed that loses an acknowledged write");
    assert!(r.violations.iter().any(|v| v.contains("is not held by")), "{:?}", r.violations);
}

/// Whether violation `v` is a copy left ahead of its primary under the
/// primary's own term, by transactions the primary does not hold: the
/// shard does not settle, as the server leaves such a copy to an operator
/// ("start it again from a checkpoint of the primary"), which a copy of
/// the primary would drop transactions of the primary's term from.
fn ahead_in_the_primarys_term(v: &str) -> bool {
    let num = |s: &str, after: &str| s.split_once(after).and_then(|(_, r)| r.split(|c: char| !c.is_ascii_digit()).next()?.parse::<u64>().ok());
    let Some((copy, primary)) = v.strip_prefix("the shard did not settle: ").and_then(|r| r.split_once(", its primary, ")) else { return false };
    match (num(copy, "up to "), num(copy, " of term "), num(primary, "up to "), num(primary, " of term ")) {
        (Some(a), Some(t), Some(b), Some(u)) => t == u && a > b,
        _ => false,
    }
}

/// A primary's writer that commits a client's write after the primary
/// stepped down and applied its new primary's first batch
/// (`Bug::CommitsAfterSteppingDown`; the server refuses the write since,
/// as its term is over) leaves a copy holding another transaction than
/// the new primary's under one term and number. The copy refuses the
/// call that follows the primary's, by its digest (protocol 20), keeps
/// that it differs, and is sent a copy of the primary: no acknowledged
/// write is lost and no two copies hold different histories under one
/// term and number, and in many seeds the copy was found so. A copy whose
/// write was the last before the clients stopped stays ahead of its
/// primary, under the primary's term: left to an operator, as the server
/// leaves it, and not counted here.
#[test]
fn a_write_committed_after_stepping_down_is_found_on_the_call_that_follows() {
    let found = std::sync::Mutex::new((0, 0, 0, Vec::new()));
    sim::on_seeds(0..300, jobs(), &|seed| {
        let mut r = sim::run(Config { bugs: vec![Bug::CommitsAfterSteppingDown], ..Config::of(seed) }, false);
        let mut f = found.lock().unwrap();
        f.0 += r.stats.stale_commits;
        f.1 += r.stats.mismatches;
        let all = r.violations.len();
        r.violations.retain(|v| !ahead_in_the_primarys_term(v));
        f.2 += all - r.violations.len();
        if !r.ok() {
            f.3.push(r);
        }
    });
    let (stale, mismatches, ahead, failed) = found.into_inner().unwrap();
    assert!(failed.is_empty(), "{} of 300 seeds broke a promise:{}", failed.len(), explain(&failed));
    assert!(stale > 100 && mismatches > 100, "{stale} writes committed after stepping down, {mismatches} calls refused for them");
    assert!(ahead <= 5, "{ahead} seeds left a copy ahead of its primary");
}

/// The same with replicas that apply by term and number alone, as before
/// protocol 20: the copies' histories stay apart.
#[test]
fn a_replica_trusting_the_same_term_and_number_is_found_again() {
    let of = |s| Config { bugs: vec![Bug::CommitsAfterSteppingDown], ..Config::of(s) };
    let r = finds_with(Bug::TrustsTheSameTerm, 1000, of).expect("a seed whose copies hold different histories");
    assert!(r.violations.iter().any(|v| v.contains("their histories differ")), "{:?}", r.violations);
}

#[test]
fn crash_heavy_runs_with_retries_keep_the_protocols_promises() {
    let failed = sim::run_seeds(0..300, &crashy, jobs());
    assert!(failed.is_empty(), "{} of 300 crash-heavy seeds broke a promise:{}", failed.len(), explain(&failed));
}
