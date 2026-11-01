//! Peak memory of a merge per byte of input image, which
//! `storage::merge::memory_per_image_byte` and `memory_per_streamed_byte`
//! are set from: anonymous memory
//! (`RssAnon`, so Linux only) sampled every 5 ms while one merge runs, above
//! what the process held before it. The database is built by one process
//! and merged by another, so that nothing the load freed is held over.
//!
//! ```text
//! merge_memory SHAPE DOCS_PER_SEGMENT SEGMENTS PATH make [KEY=VALUE ...]
//! merge_memory SHAPE DOCS_PER_SEGMENT SEGMENTS PATH merge [KEY=VALUE ...]
//! ```
//!
//! `KEY=VALUE` pairs are build options, such as `build.threads=1`.
//! SHAPE: `short` (10 words of text), `long` (300 words), `vector` (30 words
//! and a 64-dim HNSW vector), `big-vector` (30 words and 384 dims) or
//! `probe` (the scale probe's table, [`strata::data::beir_schema`] with
//! 256-dim HNSW vectors: an 8-word title and a 55-word passage of
//! Zipf-distributed words, and its columns).
//!
//! With `MERGE_TIMELINE=FILE`, the merge's anonymous memory is written to
//! FILE every 5 ms, as `seconds bytes` above what the process held before.
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
    let mut build = BuildOptions::default().set("vector.threads", 4);
    for kv in &args[6..] {
        let (k, v) = kv.split_once('=').expect("KEY=VALUE");
        build = build.set(k, v);
    }
    let opts = DbOptions { memtable_max_docs: 1_000_000_000, segment_max_docs: 1_000_000_000, build, ..DbOptions::default() };
    if args[5] == "merge" {
        return measure(shape, per, segs, &Db::open(&path, opts).unwrap());
    }
    let (words, dim) = match shape {
        "short" => (10, 0),
        "long" => (300, 0),
        "vector" => (30, 64),
        "big-vector" => (30, 384),
        "probe" => return make_probe(per, segs, &path, opts),
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

/// The scale probe's table (`src/bin/strata-scale.rs`), with made-up rows.
fn make_probe(per: usize, segs: usize, path: &std::path::Path, opts: DbOptions) {
    const DIM: usize = 256;
    let schema = strata::data::beir_schema(DIM, strata::schema::VectorIndexKind::Hnsw { m: 16, ef_construction: 200 });
    let db = Db::create(path, schema, opts).unwrap();
    // Word ranks drawn with probability 1/rank over 100,000 words.
    let vocab: Vec<String> = (0..100_000).map(|i| format!("w{i}")).collect();
    let mut cdf = Vec::with_capacity(vocab.len());
    let mut acc = 0.0f64;
    for r in 1..=vocab.len() {
        acc += 1.0 / r as f64;
        cdf.push(acc);
    }
    let mut rng = StdRng::seed_from_u64(1);
    let words = |rng: &mut StdRng, n: usize| -> String {
        let w: Vec<&str> = (0..n).map(|_| vocab[cdf.partition_point(|&c| c < rng.gen::<f64>() * acc).min(vocab.len() - 1)].as_str()).collect();
        w.join(" ")
    };
    for s in 0..segs {
        let docs: Vec<Vec<Value>> = (s * per..(s + 1) * per)
            .map(|k| {
                let mut v: Vec<f32> = (0..DIM).map(|_| rng.gen::<f32>() - 0.5).collect();
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                v.iter_mut().for_each(|x| *x /= norm);
                let (title, text) = (words(&mut rng, 8), words(&mut rng, 55));
                let body = format!("{title}\n{text}");
                vec![
                    Value::Str(format!("msmarco_v2.1_doc_{:02}_{k:09}#{}", k % 60, k % 7)),
                    Value::Str(title),
                    Value::Str(text),
                    Value::Str(body),
                    Value::Int(1990 + (k % 35) as i64),
                    Value::Str(format!("c{:02}", k % 32)),
                    Value::Float((k % 500) as f64 / 100.0),
                    Value::Str(format!("t{:02}", k % 64)),
                    Value::Vector(v.into()),
                ]
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
        let mut timeline = std::env::var_os("MERGE_TIMELINE").map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
        std::thread::spawn(move || {
            use std::io::Write;
            while !stop.load(Ordering::Relaxed) {
                let now = rss_anon();
                peak.fetch_max(now, Ordering::Relaxed);
                if let Some(w) = &mut timeline {
                    writeln!(w, "{:.3} {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64(), now.saturating_sub(base)).unwrap();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let policy = MergePolicy { merge_factor: segs, floor_docs: per * segs * 10, max_merged_docs: per * segs, ..MergePolicy::default() };
    let t = std::time::Instant::now();
    let stats = db.storage().merge(&policy).unwrap();
    assert_eq!(stats.segments_in, segs, "every segment is merged");
    let secs = t.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    let used = peak.load(Ordering::Relaxed) - base;
    println!(
        "{shape}: {segs} x {per} docs, {} merge(s) in {secs:.1} s, inputs {:.1} MB, merge peak {:.1} MB: {:.2} x input",
        stats.merges,
        input as f64 / 1e6,
        used as f64 / 1e6,
        used as f64 / input as f64
    );
}
