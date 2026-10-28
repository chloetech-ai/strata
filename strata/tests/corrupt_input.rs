//! The fuzz targets' bodies (`fuzz/src/lib.rs`) over pseudo-random
//! inputs, so that CI runs them on stable without cargo fuzz. Longer runs:
//! `cargo +nightly fuzz run <target>` in `strata/fuzz`.

#[path = "../fuzz/src/lib.rs"]
#[allow(dead_code)]
mod harness;

/// `n` inputs of 1 to `max` bytes, the same on every run (STRATA_FUZZ_RUNS
/// scales `n`).
fn inputs(seed: u64, n: usize, max: usize) -> impl Iterator<Item = Vec<u8>> {
    let scale: f64 = std::env::var("STRATA_FUZZ_RUNS").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let mut x = seed;
    let mut next = move || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        x >> 33
    };
    (0..(n as f64 * scale) as usize).map(move |_| {
        let len = 1 + next() as usize % max;
        (0..len).map(|_| next() as u8).collect()
    })
}

#[test]
fn the_queries_run_on_the_intact_seed() {
    harness::intact();
}

#[test]
fn damaged_loom_lists() {
    for data in inputs(1, 3000, 40) {
        harness::loom(&data);
    }
}

#[test]
fn damaged_segments() {
    for data in inputs(2, 300, 30) {
        harness::segment(&data);
    }
}

#[test]
fn damaged_wals() {
    for data in inputs(3, 600, 30) {
        harness::wal(&data);
    }
}

#[test]
fn damaged_manifests() {
    for data in inputs(4, 3000, 30) {
        harness::manifest(&data);
    }
}

#[test]
fn arbitrary_sql() {
    for q in harness::QUERIES {
        harness::sql(q.as_bytes());
    }
    for data in inputs(5, 2000, 80) {
        // Mutated queries: a known query with bytes of `data` spliced in.
        let q = harness::QUERIES[data[0] as usize % harness::QUERIES.len()].as_bytes();
        let at = data.get(1).map_or(0, |&b| b as usize % q.len());
        let mut m = q[..at].to_vec();
        m.extend_from_slice(&data[2.min(data.len())..data.len().min(8)]);
        m.extend_from_slice(&q[(at + data.len() % 7).min(q.len())..]);
        harness::sql(&m);
        harness::sql(&data);
    }
}

#[test]
fn arbitrary_text_queries() {
    for q in ["alpha", "\"bravo charlie\" -delta", "(echo OR golf) AND NOT kilo", "lim* india~2", "a'b", ""] {
        harness::text_query(q.as_bytes());
    }
    let alphabet = b"abcdeghiklo ()\"'*~-+:^0123|&!";
    for data in inputs(6, 2000, 40) {
        harness::text_query(&data);
        let mapped: Vec<u8> = data.iter().map(|b| alphabet[*b as usize % alphabet.len()]).collect();
        harness::text_query(&mapped);
    }
}
