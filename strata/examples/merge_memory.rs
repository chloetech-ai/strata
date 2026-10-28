//! Peak memory of a merge per byte of input image, which
//! `storage::merge::MEMORY_PER_IMAGE_BYTE` is set from: anonymous memory
//! (`RssAnon`, so Linux only) sampled every 5 ms while one merge runs, above
//! what the process held before it. The database is built by one process
//! and merged by another, so that nothing the load freed is held over.
//!
//! ```text
//! merge_memory SHAPE DOCS_PER_SEGMENT SEGMENTS PATH make
//! merge_memory SHAPE DOCS_PER_SEGMENT SEGMENTS PATH merge
//! ```
//!
//! SHAPE: `short` (10 words of text), `long` (300 words), `vector` (30 words
//! and a 64-dim HNSW vector) or `big-vector` (30 words and 384 dims).
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::db::Db;
use strata::schema::DEFAULT_HNSW;
use strata::segment::build::BuildOptions;
use strata::storage::{DbOptions, MergePolicy};
use strata::{Field, FieldType, Schema, Value};

fn rss_anon() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    let kb: u64 = s.lines().find(|l| l.starts_with("RssAnon:")).unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    kb * 1024
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (shape, per, segs, path) = (args[1].as_str(), args[2].parse::<usize>().unwrap(), args[3].parse::<usize>().unwrap(), PathBuf::from(&args[4]));
    let opts = DbOptions {
        memtable_max_docs: 1_000_000_000,
        segment_max_docs: 1_000_000_000,
        build: BuildOptions::default().set("vector.threads", 4),
        ..DbOptions::default()
    };
    if args[5] == "merge" {
        return measure(shape, per, segs, &Db::open(&path, opts).unwrap());
    }
    let (words, dim) = match shape {
        "short" => (10, 0),
        "long" => (300, 0),
        "vector" => (30, 64),
        "big-vector" => (30, 384),
        _ => panic!("unknown shape {shape}"),
    };
    let mut fields = vec![Field::new("id", FieldType::Keyword), Field::new("n", FieldType::Int), Field::new("body", FieldType::Text { positions: true })];
    if dim > 0 {
        fields.push(Field::new("v", FieldType::Vector { dim, index: DEFAULT_HNSW }));
    }
    let db = Db::create(&path, Schema::new("t", fields, Some("id")).unwrap(), opts).unwrap();
    let vocab: Vec<String> = (0..20000).map(|i| format!("w{i}")).collect();
    let mut rng = StdRng::seed_from_u64(1);
    for s in 0..segs {
        let docs: Vec<Vec<Value>> = (s * per..(s + 1) * per)
            .map(|k| {
                let body: Vec<&str> = (0..words).map(|_| vocab[rng.gen_range(0..vocab.len())].as_str()).collect();
                let mut d = vec![Value::Str(format!("k{k:09}")), Value::Int(k as i64), Value::Str(body.join(" "))];
                if dim > 0 {
                    d.push(Value::Vector((0..dim).map(|_| rng.gen::<f32>()).collect::<Vec<f32>>().into()));
                }
                d
            })
            .collect();
        db.storage().bulk_load(docs).unwrap();
    }
}

fn measure(shape: &str, per: usize, segs: usize, db: &Db) {
    let input: u64 = db.storage().file_stats().segments.iter().map(|s| s.bytes).sum();
    let base = rss_anon();
    let peak = Arc::new(AtomicU64::new(base));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, stop) = (peak.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(rss_anon(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let policy = MergePolicy { merge_factor: segs, floor_docs: per * segs * 10, ..MergePolicy::default() };
    let stats = db.storage().merge(&policy).unwrap();
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    let used = peak.load(Ordering::Relaxed) - base;
    println!(
        "{shape}: {segs} x {per} docs, {} merge(s), inputs {:.1} MB, merge peak {:.1} MB: {:.2} x input",
        stats.merges,
        input as f64 / 1e6,
        used as f64 / 1e6,
        used as f64 / input as f64
    );
}
