//! Single-row writes to a table with text and vector indexes, with the
//! background merger the Python, JavaScript and Java packages run, on or
//! off: what merges running beside them cost the foreground's commits.
//! The table is `strata-py/bench/laptop/a_writes.py`'s products (title and
//! description searched as one text, a 384-number vector, price, stock, a
//! counter), with made-up text and vectors; the phases follow that
//! script: inserts of 1, 100, 1,000 and 10,000 rows a transaction, price
//! and stock updates by key, one a transaction, and an events table
//! written at 1,000, 10,000 and 50,000 rows a second while four readers
//! query it.
//!
//! ```text
//! writes_beside_merges DIR [ROWS] [--no-merger] [--threads N] [--nice N]
//!     [--stream] [--rates 1000,10000,50000] [--publish] [--flusher] [--bulk N]
//! ```
//!
//! `--nice` sets the merger's nice value ([`MergerOptions::nice`]; 0 runs
//! merges at the priority of the writes and reads). `--stream` runs only
//! the stream, at the `--rates` given. `--publish` has the writer build
//! each version's snapshot ([`DbOptions::publish_snapshots`]) and
//! `--flusher` flushes the memtable in the background
//! ([`Db::spawn_flusher`]), both as strata-server does. `--bulk N`
//! bulk-loads stream inserts of N rows or more into segments of their own
//! (default 10,000, the memtable's size, as strata-py; it was 1,000 until
//! 2 November).
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::{Rng, SeedableRng};
use serde_json::json;
use strata::db::{Db, WriteOp};
use strata::query::Params;
use strata::segment::build::BuildOptions;
use strata::storage::{DbOptions, Layout, MergerOptions};
use strata::{Schema, Value};

const DIM: usize = 384;

fn words(rng: &mut impl Rng, n: usize) -> String {
    (0..n).map(|_| format!("w{}", (rng.gen::<f64>().powi(3) * 20_000.0) as u32)).collect::<Vec<_>>().join(" ")
}

fn vector(rng: &mut impl Rng, centres: &[Vec<f32>]) -> Vec<f32> {
    let c = &centres[rng.gen_range(0..centres.len())];
    let mut v: Vec<f32> = c.iter().map(|x| x + rng.gen_range(-0.4f32..0.4)).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= n);
    v
}

struct Gen {
    rng: rand::rngs::StdRng,
    centres: Vec<Vec<f32>>,
    next: usize,
}

impl Gen {
    fn docs(&mut self, schema: &Schema, n: usize) -> Vec<strata::Document> {
        let emb = schema.field_index("emb").unwrap();
        (0..n)
            .map(|_| {
                let i = self.next;
                self.next += 1;
                let j = json!({"id": format!("p{i}"), "title": words(&mut self.rng, 8), "description": words(&mut self.rng, 60),
                    "brand": format!("b{}", i % 500), "category": format!("c{}", i % 40), "price": self.rng.gen_range(5.0..80.0), "stock": self.rng.gen_range(0..200), "rating_count": 0});
                let v = vector(&mut self.rng, &self.centres);
                strata::ingest::document_with_vectors(schema, &j, vec![(emb, v)], strata::timestamp::now()).unwrap()
            })
            .collect()
    }
}

fn summary(name: &str, lat: &mut [f64], wall: f64) {
    lat.sort_by(f64::total_cmp);
    let q = |p: f64| lat[((lat.len() - 1) as f64 * p).round() as usize];
    println!("{name:14} {:6} txns, {:8.0} a second, p50 {:6.2} ms, p90 {:6.2}, p99 {:7.2}, max {:7.1}", lat.len(), lat.len() as f64 / wall, q(0.5), q(0.9), q(0.99), q(1.0));
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&a[1]);
    let rows: usize = a.get(2).filter(|x| !x.starts_with("--")).map_or(137_260, |x| x.parse().unwrap());
    let merger = !a.iter().any(|x| x == "--no-merger");
    let stream_only = a.iter().any(|x| x == "--stream");
    let publish = a.iter().any(|x| x == "--publish");
    let flusher = a.iter().any(|x| x == "--flusher");
    let bulk: usize = a.iter().position(|x| x == "--bulk").map_or(10_000, |i| a[i + 1].parse().unwrap());
    let rates: Vec<usize> = a.iter().position(|x| x == "--rates").map_or(vec![1000, 10_000, 50_000], |i| a[i + 1].split(',').map(|r| r.parse().unwrap()).collect());
    let nice: i32 = a.iter().position(|x| x == "--nice").map_or(strata::storage::MERGE_NICE, |i| a[i + 1].parse().unwrap());
    let threads: usize = a.iter().position(|x| x == "--threads").map_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()), |i| a[i + 1].parse().unwrap());
    let schema = Schema::from_json(
        &json!({"table": "products", "key": "id", "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "title", "type": "text", "indexed": false, "positions": false},
            {"name": "description", "type": "text", "indexed": false, "positions": false},
            {"name": "body", "type": "text", "from": ["title", "description"]},
            {"name": "brand", "type": "keyword"},
            {"name": "category", "type": "keyword"},
            {"name": "price", "type": "float"},
            {"name": "stock", "type": "int"},
            {"name": "rating_count", "type": "int"},
            {"name": "emb", "type": "vector", "dim": DIM}]})
        .to_string(),
    )
    .unwrap();
    // As strata-py's db_options and with_threads.
    let opts = DbOptions {
        layout: Layout::SingleFile,
        build: BuildOptions::default().set("vector.threads", threads),
        mem_build: Some(BuildOptions::default().set("vector.index", "flat")),
        memtable_max_docs: 10_000,
        publish_snapshots: publish,
        ..DbOptions::default()
    };
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = Db::create(&dir.join("w.strata"), schema.clone(), opts).unwrap();
    db.exec.threads = threads;
    let db = Arc::new(db);
    let mut rng = rand::rngs::StdRng::seed_from_u64(1);
    let centres: Vec<Vec<f32>> = (0..50).map(|_| (0..DIM).map(|_| rng.gen_range(-1.0f32..1.0)).collect()).collect();
    let mut g = Gen { rng, centres, next: 0 };
    let t = Instant::now();
    while g.next < rows && !stream_only {
        let docs = g.docs(&schema, (rows - g.next).min(10_000));
        db.load_into("products", docs).unwrap();
    }
    db.optimize().unwrap();
    println!("{rows} rows loaded and optimized in {:.1} s; {threads} threads; merger {}; publish_snapshots {publish}; flusher {flusher}; bulk loads from {bulk} rows", if stream_only { 0.0 } else { t.elapsed().as_secs_f64() }, if merger { format!("on, nice {nice}") } else { "off".into() });
    if flusher {
        db.spawn_flusher();
    }
    if merger {
        db.spawn_merger(MergerOptions { threads: 1, nice, ..MergerOptions::default() });
    }
    let insert = |docs: Vec<strata::Document>| {
        if docs.len() >= 10_000 {
            db.load_into("products", docs).unwrap();
        } else {
            db.write_ops(docs.into_iter().map(|doc| WriteOp::Insert { table: "products".into(), doc }).collect()).unwrap();
        }
    };
    // As a_writes.py with --seconds 5: 1 row a transaction for 5 s, then
    // 100 and 1,000 rows (up to 50 transactions in 5 s), then three of
    // 10,000.
    let phases: &[(usize, usize)] = if stream_only { &[] } else { &[(1, usize::MAX), (100, 50), (1000, 50), (10_000, 3)] };
    for &(n, times) in phases {
        let (mut lat, t0) = (Vec::new(), Instant::now());
        for _ in 0..times {
            let docs = g.docs(&schema, n);
            let t = Instant::now();
            insert(docs);
            lat.push(t.elapsed().as_secs_f64() * 1e3);
            if n < 10_000 && t0.elapsed() > Duration::from_secs(5) {
                break;
            }
        }
        summary(&format!("insert {n}"), &mut lat, t0.elapsed().as_secs_f64());
    }
    let mut rng = rand::rngs::StdRng::seed_from_u64(2);
    let keys: Vec<String> = (0..3000).map(|_| format!("p{}", rng.gen_range(0..rows))).collect();
    let sql = "UPDATE products SET price = :price, stock = :stock WHERE id = :id";
    for pass in 0..if stream_only { 0 } else { 3 } {
        let (mut lat, t0, mut merging) = (Vec::new(), Instant::now(), 0);
        for k in &keys {
            merging += (db.storage().merge_status().running > 0) as usize;
            let p = Params::new().set("price", Value::Float(rng.gen_range(5.0..80.0))).set("stock", Value::Int(rng.gen_range(0..200))).set("id", Value::Str(k.clone()));
            let t = Instant::now();
            db.sql(sql, &p).unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
            if t0.elapsed() > Duration::from_secs(5) / 3 {
                break;
            }
        }
        summary(&format!("update pass {pass}"), &mut lat, t0.elapsed().as_secs_f64());
        println!("               {merging} of them while a merge ran");
    }
    // The stream: review-like events appended at 1,000, 10,000 and 50,000
    // a second, a transaction per 50 ms, while 4 threads read (a product's
    // latest events, a word in their text), as a_writes.py's stream.
    db.sql("CREATE TABLE events (id KEYWORD PRIMARY KEY, user_id KEYWORD, product_id KEYWORD, rating INT, text TEXT OPTIONS(positions = false), ts INT)", &Params::new()).unwrap();
    let ev_schema = db.catalog().into_iter().find(|t| t.table == "events").unwrap();
    let seq = std::sync::atomic::AtomicUsize::new(0);
    let ev_rows = |rng: &mut rand::rngs::StdRng, k: usize| -> Vec<strata::Document> {
        let now = strata::timestamp::now() / 1000;
        (0..k)
            .map(|_| {
                let i = seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let j = json!({"id": format!("e{i}"), "user_id": format!("u{}", rng.gen_range(0..50_000)), "product_id": format!("p{}", rng.gen_range(0..20_000)), "rating": rng.gen_range(1..6), "text": words(rng, 40), "ts": now});
                strata::ingest::document(&ev_schema, &j).unwrap()
            })
            .collect()
    };
    let ev_insert = |docs: Vec<strata::Document>| {
        if docs.len() >= bulk {
            db.load_into("events", docs).unwrap();
        } else {
            db.write_ops(docs.into_iter().map(|doc| WriteOp::Insert { table: "events".into(), doc }).collect()).unwrap();
        }
    };
    ev_insert(ev_rows(&mut rng, 10_000));
    for rate in rates {
        let stop = std::sync::atomic::AtomicBool::new(false);
        let (written, wall, mut reads) = std::thread::scope(|sc| {
            let readers: Vec<_> = (0..4u64)
                .map(|t| {
                    let (db, stop) = (&db, &stop);
                    sc.spawn(move || {
                        let mut r = rand::rngs::StdRng::seed_from_u64(10 + t);
                        let mut lat = Vec::new();
                        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                            let t0 = Instant::now();
                            if r.gen_bool(0.5) {
                                let p = Params::new().set("p", Value::Str(format!("p{}", r.gen_range(0..20_000))));
                                db.query("SELECT id FROM events WHERE product_id = :p ORDER BY ts DESC LIMIT 10", &p).unwrap();
                            } else {
                                let p = Params::new().set("w", Value::Str(format!("w{}", r.gen_range(0..50))));
                                db.query("SELECT id FROM events ORDER BY bm25(text, :w, syntax => 'plain') DESC LIMIT 10", &p).unwrap();
                            }
                            lat.push(t0.elapsed().as_secs_f64() * 1e3);
                        }
                        lat
                    })
                })
                .collect();
            let (t0, mut written, mut ticks) = (Instant::now(), 0usize, 0u32);
            while t0.elapsed() < Duration::from_secs(15) {
                let k = rate / 20;
                ev_insert(ev_rows(&mut rng, k));
                written += k;
                ticks += 1;
                if let Some(slack) = (t0 + Duration::from_millis(50) * ticks).checked_duration_since(Instant::now()) {
                    std::thread::sleep(slack);
                }
            }
            let wall = t0.elapsed().as_secs_f64();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            (written, wall, readers.into_iter().flat_map(|h| h.join().unwrap()).collect::<Vec<f64>>())
        });
        println!("stream {rate:6}/s: written {:8.0} a second", written as f64 / wall);
        summary("  reads", &mut reads, wall);
    }
    println!("merges: {:?}", db.storage().merge_status());
}
