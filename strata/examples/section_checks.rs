//! What checking each section of a segment against its crc32 the first
//! time it is read (`DbOptions::check_sections`) costs: the time to open a
//! database, and the first and second run of four kinds of query after
//! it, with the check on and off, the database's pages evicted from the
//! page cache before opening (cold) or left in it (warm).
//!
//! ```text
//! section_checks PATH make DOCS DIM
//! section_checks PATH measure ROUNDS
//! ```
//!
//! `make` bulk-loads DOCS documents (30 words of text with positions, an
//! integer, a DIM-dimensional HNSW vector) in segments of 100,000.
//! `measure` alternates check on and off, cold then warm, ROUNDS times,
//! each in a fresh `Db`, and prints one JSON line per run.
use std::path::{Path, PathBuf};
use std::time::Instant;

use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::db::Db;
use strata::query::Params;
use strata::schema::DEFAULT_HNSW;
use strata::segment::build::BuildOptions;
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema, Value};

const PER_SEGMENT: usize = 100_000;

fn options(check_sections: bool) -> DbOptions {
    let build = BuildOptions::default().set("vector.threads", 4);
    DbOptions { memtable_max_docs: 1_000_000_000, segment_max_docs: PER_SEGMENT, check_sections, build, ..DbOptions::default() }
}

fn make(path: &Path, docs: usize, dim: usize) {
    let fields = vec![
        Field::new("id", FieldType::Keyword),
        Field::new("n", FieldType::Int),
        Field::new("body", FieldType::Text { positions: true }),
        Field::new("v", FieldType::Vector { dim, index: DEFAULT_HNSW }),
    ];
    let db = Db::create(path, Schema::new("t", fields, Some("id")).unwrap(), options(true)).unwrap();
    let vocab: Vec<String> = (0..20000).map(|i| format!("w{i}")).collect();
    let mut rng = StdRng::seed_from_u64(1);
    for start in (0..docs).step_by(PER_SEGMENT) {
        let batch: Vec<Vec<Value>> = (start..(start + PER_SEGMENT).min(docs))
            .map(|k| {
                // Word i about as often as 1 / (i + 1): a few long lists, many short ones.
                let body: Vec<&str> = (0..30).map(|_| vocab[(20000f64.powf(rng.gen::<f64>()) as usize - 1).min(19999)].as_str()).collect();
                let v: Vec<f32> = (0..dim).map(|_| rng.gen::<f32>()).collect();
                vec![Value::Str(format!("k{k:09}")), Value::Int(k as i64), Value::Str(body.join(" ")), Value::Vector(v.into())]
            })
            .collect();
        db.storage().bulk_load(batch).unwrap();
        eprintln!("loaded {}", (start + PER_SEGMENT).min(docs));
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn measure(path: &Path, rounds: usize) {
    let dim = Db::open(path, options(false)).unwrap().schema().fields.iter().find_map(|f| match f.ty {
        FieldType::Vector { dim, .. } => Some(dim),
        _ => None,
    });
    let mut rng = StdRng::seed_from_u64(7);
    let q: Vec<f32> = (0..dim.unwrap()).map(|_| rng.gen::<f32>()).collect();
    let p = Params::new().set("w", Value::Str("w3 w150".into())).set("q", Value::Vector(q.into()));
    let queries = [
        ("filter", "SELECT count(*) FROM t WHERE n >= 250000 AND n < 750000"),
        ("text", "SELECT score() FROM t ORDER BY bm25(body, :w) DESC LIMIT 10"),
        ("phrase", "SELECT count(*) FROM t WHERE MATCH(body, '\"w3 w150\"')"),
        ("vector", "SELECT score() FROM t ORDER BY similarity(v, :q) DESC LIMIT 10"),
        ("fetch", "SELECT id, body FROM t WHERE id = 'k000123456'"),
    ];
    for round in 0..rounds {
        for check in [true, false] {
            for cold in [true, false] {
                if cold {
                    strata::iostat::evict(path).unwrap();
                }
                let t = Instant::now();
                let db = Db::open(path, options(check)).unwrap();
                let open = ms(t);
                let mut runs = Vec::new();
                for (name, sql) in queries {
                    let t = Instant::now();
                    let rows = db.query(sql, &p).unwrap().rows.len();
                    let first = ms(t);
                    let t = Instant::now();
                    db.query(sql, &p).unwrap();
                    runs.push(format!(r#""{name}": {{"first_ms": {first:.2}, "second_ms": {:.2}, "rows": {rows}}}"#, ms(t)));
                }
                println!(r#"{{"round": {round}, "check_sections": {check}, "cold": {cold}, "open_ms": {open:.2}, {}}}"#, runs.join(", "));
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = PathBuf::from(&args[1]);
    match args[2].as_str() {
        "make" => make(&path, args[3].parse().unwrap(), args[4].parse().unwrap()),
        "measure" => measure(&path, args[3].parse().unwrap()),
        other => panic!("unknown command {other}"),
    }
}
