//! The cost of a sequence model alone (`encode(model, rows)` without the
//! history's read): a model directory's model over random histories of
//! each length, on one thread.
//!
//! Prints a line per length, the mean and p99 microseconds per history.

use std::time::Instant;

use clap::Parser;
use rand::{Rng, SeedableRng};
use strata::metrics::{mean, percentile};
use strata::query::sequence::{Event, SequenceModel, CONFIG_FILE, WEIGHTS_FILE};

#[derive(Parser)]
#[command(about = "a sequence model's cost per history")]
struct Args {
    /// The model's directory (config.json, model.safetensors).
    model: std::path::PathBuf,
    #[arg(long, value_delimiter = ',', default_value = "10,50,100,200")]
    lengths: Vec<usize>,
    #[arg(long, default_value_t = 2000)]
    requests: usize,
}

fn main() {
    let a = Args::parse();
    let read = |f: &str| std::fs::read(a.model.join(f)).unwrap_or_else(|e| panic!("{}: {e}", a.model.join(f).display()));
    let m = SequenceModel::parse("bench", &read(CONFIG_FILE), &read(WEIGHTS_FILE), None).unwrap();
    println!("{m:?}");
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let items: Vec<Vec<f32>> = (0..1000).map(|_| (0..m.dim).map(|_| rng.gen_range(-0.2f32..0.2)).collect()).collect();
    for &n in &a.lengths {
        let mut us = Vec::with_capacity(a.requests);
        for r in 0..a.requests + 100 {
            let h: Vec<Event> = (0..n).map(|i| Event { item: &items[rng.gen_range(0..items.len())], action: None, time: Some(1e9 + i as f64 * 600.0) }).collect();
            let t = Instant::now();
            let v = m.encode(&h, Some(1e9 + n as f64 * 600.0 + 60.0));
            if r >= 100 {
                us.push(t.elapsed().as_secs_f64() * 1e6);
            }
            assert!(v.is_some_and(|v| v.len() == m.dim));
        }
        println!("{n} events: mean {:.1} us, p50 {:.1}, p99 {:.1}", mean(&us), percentile(&us, 50.0), percentile(&us, 99.0));
    }
}
