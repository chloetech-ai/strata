//! Storage engine experiments (DESIGN.md §3, `notes/storage.md`).
//!
//! * `wal`: commit throughput (documents/s, commits/s, latency) for
//!   transactions of 1, 10, 100 and 1000 documents, with and without fsync.
//! * `bulk`: bulk-load throughput, with the segment build timed separately.
//! * `open`: cold and warm open time with 10..1000 segments, and WAL replay.
//! * `flush`, `merge`: flush and merge times.
//! * `space`: file size against live data under a delete-heavy workload,
//!   before and after merges and `vacuum`.
//! * `snapshot`: cost of the first snapshot after a write (memtable rebuild).
//! * `pk`: primary-key lookup latency and pages touched per lookup, radix
//!   directory against binary search.
//!
//! Run from `strata/`:
//! `cargo run --release --bin strata-storage-bench -- --exp wal,bulk --dataset fiqa`.
//! Each experiment writes `results/storage/<exp>.json`. Segments are built by
//! `build_segment`, so the numbers include whatever index builders exist;
//! with the component stubs they isolate the storage layer (primary-key
//! index, extents, commits).

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use posting_bench::bytes::{ByteReader, Slice};
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as Json};

use strata::segment::build::{build_segment, BuildOptions};
use strata::segment::{DocId, SegmentFile};
use strata::storage::{key_hash, Database, DbOptions, DocumentSource, KeyRef, MergePolicy, PkIndex};
use strata::{data, iostat, Document, Result, Schema, Value, VectorIndexKind};

#[derive(Parser)]
struct Args {
    /// Comma-separated experiments: wal, bulk, open, flush, merge, space, snapshot, pk.
    #[arg(long, value_delimiter = ',', default_value = "wal,bulk,open,flush,merge,space,snapshot,pk")]
    exp: Vec<String>,
    #[arg(long, default_value = "fiqa")]
    dataset: String,
    /// Load only the first `limit` documents.
    #[arg(long)]
    limit: Option<usize>,
    /// Repetitions per measurement (medians are reported).
    #[arg(long, default_value_t = 3)]
    reps: usize,
    /// Scratch directory for database files; must be on the disk under test
    /// (not tmpfs) for fsync numbers to mean anything.
    #[arg(long, default_value = "target/storage-bench")]
    dir: PathBuf,
    #[arg(long, default_value = "results/storage")]
    out: PathBuf,
    /// Time limit per WAL configuration and repetition, in seconds.
    #[arg(long, default_value_t = 2.0)]
    wal_secs: f64,
    /// Document limit per WAL configuration and repetition.
    #[arg(long, default_value_t = 20_000)]
    wal_docs: usize,
}

struct Ctx {
    args: Args,
    schema: Schema,
    docs: Vec<Document>,
}

impl Ctx {
    /// A fresh database path (old files removed).
    fn fresh(&self, name: &str) -> PathBuf {
        let p = self.args.dir.join(name);
        remove_db(&p);
        p
    }

    /// Document `i` of the collection (cyclically) under a new key.
    fn keyed(&self, i: usize, key: String) -> Document {
        let mut d = self.docs[i % self.docs.len()].clone();
        d[data::col::ID] = Value::Str(key);
        d
    }

    fn source(&self, docs: &[Document]) -> Arc<MapSource> {
        Arc::new(MapSource { docs: docs.iter().map(|d| (d[data::col::ID].as_str().unwrap().to_string(), d.clone())).collect() })
    }
}

fn remove_db(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(wal_of(p));
}

fn wal_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

/// Rebuilds documents from the loaded collection by primary key.
struct MapSource {
    docs: HashMap<String, Document>,
}

impl DocumentSource for MapSource {
    fn documents(&self, _schema: &Schema, seg: &SegmentFile, docs: &[DocId]) -> Result<Vec<Document>> {
        let pk = PkIndex::open(seg)?.expect("pk index");
        Ok(docs.iter().map(|&d| self.docs[pk.key(d).as_str().unwrap()].clone()).collect())
    }
}

fn median(v: &[f64]) -> f64 {
    percentile(v, 50.0)
}

fn percentile(v: &[f64], p: f64) -> f64 {
    strata::metrics::percentile(v, p)
}

fn secs(t: Instant) -> f64 {
    t.elapsed().as_secs_f64()
}

/// Seconds to milliseconds.
fn ms(v: &[f64]) -> Vec<f64> {
    v.iter().map(|s| s * 1e3).collect()
}

fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.dir)?;
    std::fs::create_dir_all(&args.out)?;
    let t = Instant::now();
    let c = data::load(&args.dataset, args.limit)?;
    eprintln!("loaded {} documents of {} in {:.1}s", c.docs.len(), args.dataset, secs(t));
    let schema = data::beir_schema(c.dim, VectorIndexKind::Flat);
    let cx = Ctx { args, schema, docs: c.docs };
    for exp in cx.args.exp.clone() {
        eprintln!("== {exp}");
        // Other work on the machine skews timings; keep a record of it.
        let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        let t = Instant::now();
        let rows = match exp.as_str() {
            "wal" => exp_wal(&cx)?,
            "bulk" => exp_bulk(&cx)?,
            "open" => exp_open(&cx)?,
            "flush" => exp_flush(&cx)?,
            "merge" => exp_merge(&cx)?,
            "space" => exp_space(&cx)?,
            "snapshot" => exp_snapshot(&cx)?,
            "pk" => exp_pk(&cx)?,
            other => panic!("unknown experiment {other}"),
        };
        let out = json!({
            "experiment": exp,
            "dataset": cx.args.dataset,
            "n_docs": cx.docs.len(),
            "reps": cx.args.reps,
            "loadavg_at_start": load.trim(),
            "seconds": secs(t),
            "results": rows,
        });
        let path = cx.args.out.join(format!("{exp}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&out).unwrap())?;
        eprintln!("wrote {} ({:.1}s)", path.display(), secs(t));
    }
    Ok(())
}

fn exp_wal(cx: &Ctx) -> Result<Json> {
    let mut rows = Vec::new();
    for sync in [true, false] {
        for txn in [1usize, 10, 100, 1000] {
            let (mut docs_s, mut commits_s, mut p50, mut p99, mut bpd) = (vec![], vec![], vec![], vec![], vec![]);
            for rep in 0..cx.args.reps {
                let path = cx.fresh("wal.strata");
                let db =
                    Database::create(&path, cx.schema.clone(), DbOptions { sync, memtable_max_docs: usize::MAX, ..Default::default() })?;
                let cap = cx.args.wal_docs.max(txn);
                let mut batch = (0..cap).map(|i| cx.keyed(i, format!("w{rep}-{i}")));
                let mut lat = Vec::new();
                let mut n = 0;
                let t0 = Instant::now();
                while n + txn <= cap && secs(t0) < cx.args.wal_secs {
                    let docs: Vec<Document> = batch.by_ref().take(txn).collect();
                    let t = Instant::now();
                    let mut tx = db.begin()?;
                    for d in docs {
                        tx.insert(d)?;
                    }
                    tx.commit()?;
                    lat.push(secs(t));
                    n += txn;
                }
                let total: f64 = lat.iter().sum();
                docs_s.push(n as f64 / total);
                commits_s.push(lat.len() as f64 / total);
                p50.push(percentile(&lat, 50.0) * 1e6);
                p99.push(percentile(&lat, 99.0) * 1e6);
                bpd.push(db.file_stats().wal_bytes as f64 / n as f64);
                drop(db);
                remove_db(&path);
            }
            eprintln!(
                "sync={sync} txn={txn}: {:.0} docs/s, {:.0} commits/s, p50 {:.0}us",
                median(&docs_s),
                median(&commits_s),
                median(&p50)
            );
            rows.push(json!({
                "sync": sync, "txn_docs": txn,
                "docs_per_s": median(&docs_s), "commits_per_s": median(&commits_s),
                "commit_us_p50": median(&p50), "commit_us_p99": median(&p99),
                "wal_bytes_per_doc": median(&bpd),
                "docs_per_s_reps": docs_s,
            }));
        }
    }
    Ok(json!(rows))
}

fn exp_bulk(cx: &Ctx) -> Result<Json> {
    let mut rows = Vec::new();
    for seg_max in [10_000usize, 100_000] {
        let (mut load, mut build) = (vec![], vec![]);
        let mut file_bytes = 0;
        let mut segments = 0;
        for _ in 0..cx.args.reps {
            let path = cx.fresh("bulk.strata");
            let db = Database::create(&path, cx.schema.clone(), DbOptions { segment_max_docs: seg_max, ..Default::default() })?;
            let docs = cx.docs.clone();
            let t = Instant::now();
            db.bulk_load(docs)?;
            load.push(secs(t));
            let st = db.file_stats();
            (file_bytes, segments) = (st.file_bytes, st.segments.len());
            drop(db);
            remove_db(&path);
            let t = Instant::now();
            for chunk in cx.docs.chunks(seg_max) {
                build_segment(&cx.schema, chunk, &BuildOptions::default())?;
            }
            build.push(secs(t));
        }
        let (l, b) = (median(&load), median(&build));
        eprintln!("segment_max={seg_max}: {:.0} docs/s, {:.3}s ({:.3}s building)", cx.docs.len() as f64 / l, l, b);
        rows.push(json!({
            "segment_max_docs": seg_max, "segments": segments, "file_bytes": file_bytes,
            "seconds": l, "docs_per_s": cx.docs.len() as f64 / l,
            "build_seconds": b, "storage_seconds": l - b,
            "seconds_reps": load,
        }));
    }
    Ok(json!(rows))
}

fn exp_open(cx: &Ctx) -> Result<Json> {
    let n = cx.docs.len();
    let probe = cx.docs[n / 2][data::col::ID].clone();
    let mut rows = Vec::new();
    for n_seg in [10usize, 100, 1000] {
        let path = cx.fresh("open.strata");
        let db = Database::create(&path, cx.schema.clone(), DbOptions { segment_max_docs: n.div_ceil(n_seg), ..Default::default() })?;
        db.bulk_load(cx.docs.clone())?;
        let segments = db.file_stats().segments.len();
        drop(db);
        for cold in [true, false] {
            let (mut open, mut snap, mut look) = (vec![], vec![], vec![]);
            for _ in 0..cx.args.reps {
                if cold {
                    iostat::evict(&path)?;
                    iostat::evict(&wal_of(&path))?;
                }
                let t = Instant::now();
                let db = Database::open(&path, DbOptions::default())?;
                open.push(secs(t));
                let t = Instant::now();
                let s = db.snapshot();
                snap.push(secs(t));
                let t = Instant::now();
                assert!(s.lookup(&probe).is_some());
                look.push(secs(t));
            }
            eprintln!("{segments} segments, cold={cold}: open {:.2}ms, first snapshot {:.2}ms", median(&open) * 1e3, median(&snap) * 1e3);
            rows.push(json!({
                "kind": "segments", "segments": segments, "cold": cold,
                "open_ms": median(&open) * 1e3, "first_snapshot_ms": median(&snap) * 1e3, "first_lookup_ms": median(&look) * 1e3,
            }));
        }
        remove_db(&path);
    }
    for wal_docs in [1000usize, 10_000] {
        let path = cx.fresh("replay.strata");
        let db =
            Database::create(&path, cx.schema.clone(), DbOptions { sync: false, memtable_max_docs: usize::MAX, ..Default::default() })?;
        for chunk in (0..wal_docs).collect::<Vec<_>>().chunks(100) {
            let mut tx = db.begin()?;
            for &i in chunk {
                tx.insert(cx.keyed(i, format!("r{i}")))?;
            }
            tx.commit()?;
        }
        let wal_bytes = db.file_stats().wal_bytes;
        drop(db);
        let (mut open, mut snap) = (vec![], vec![]);
        for _ in 0..cx.args.reps {
            iostat::evict(&path)?;
            iostat::evict(&wal_of(&path))?;
            let t = Instant::now();
            let db = Database::open(&path, DbOptions::default())?;
            open.push(secs(t));
            let t = Instant::now();
            assert_eq!(db.snapshot().n_live_docs(), wal_docs as u64);
            snap.push(secs(t));
        }
        eprintln!("WAL replay of {wal_docs} documents: open {:.1}ms", median(&open) * 1e3);
        rows.push(json!({
            "kind": "wal_replay", "wal_docs": wal_docs, "wal_bytes": wal_bytes, "cold": true,
            "open_ms": median(&open) * 1e3, "first_snapshot_ms": median(&snap) * 1e3,
        }));
        remove_db(&path);
    }
    Ok(json!(rows))
}

fn exp_flush(cx: &Ctx) -> Result<Json> {
    let mut rows = Vec::new();
    for n in [1000usize, 10_000] {
        let mut flush = vec![];
        for _ in 0..cx.args.reps {
            let path = cx.fresh("flush.strata");
            let db = Database::create(&path, cx.schema.clone(), DbOptions { memtable_max_docs: usize::MAX, ..Default::default() })?;
            for chunk in cx.docs[..n.min(cx.docs.len())].chunks(1000) {
                let mut tx = db.begin()?;
                for d in chunk {
                    tx.insert(d.clone())?;
                }
                tx.commit()?;
            }
            let t = Instant::now();
            db.flush()?;
            flush.push(secs(t));
            drop(db);
            remove_db(&path);
        }
        eprintln!("flush of {n} documents: {:.1}ms", median(&flush) * 1e3);
        rows.push(json!({ "memtable_docs": n, "flush_ms": median(&flush) * 1e3, "docs_per_s": n as f64 / median(&flush), "flush_ms_reps": ms(&flush) }));
    }
    Ok(json!(rows))
}

fn exp_merge(cx: &Ctx) -> Result<Json> {
    let segs = 8;
    let per = (cx.docs.len() / segs).min(5000);
    let docs = &cx.docs[..segs * per];
    let source = cx.source(docs);
    let mut rng = rand::rngs::StdRng::seed_from_u64(5);
    let (mut merge, mut bytes, mut merged) = (vec![], 0, 0);
    for _ in 0..cx.args.reps {
        let path = cx.fresh("merge.strata");
        let opts = DbOptions { segment_max_docs: per, doc_source: Some(source.clone()), ..Default::default() };
        let db = Database::create(&path, cx.schema.clone(), opts)?;
        db.bulk_load(docs.to_vec())?;
        let mut tx = db.begin()?;
        for d in docs.choose_multiple(&mut rng, docs.len() / 10) {
            tx.delete(&d[data::col::ID])?;
        }
        tx.commit()?;
        db.flush()?;
        let policy = MergePolicy { merge_factor: segs, floor_docs: per, ..MergePolicy::default() };
        let t = Instant::now();
        let stats = db.merge(&policy)?;
        merge.push(secs(t));
        assert_eq!(stats.merges, 1);
        (bytes, merged) = (stats.bytes_written, stats.docs_merged);
        drop(db);
        remove_db(&path);
    }
    let m = median(&merge);
    eprintln!("merge of {segs} x {per} documents: {:.1}ms", m * 1e3);
    Ok(json!([{
        "segments_in": segs, "docs_per_segment": per, "deleted_fraction": 0.1,
        "merge_ms": m * 1e3, "docs_per_s": merged as f64 / m, "bytes_written": bytes,
        "merge_ms_reps": ms(&merge),
    }]))
}

fn exp_space(cx: &Ctx) -> Result<Json> {
    let n = cx.docs.len();
    let seg_max = 10_000;
    let source = cx.source(&cx.docs);
    let path = cx.fresh("space.strata");
    let opts = DbOptions { segment_max_docs: seg_max, memtable_max_docs: 5000, doc_source: Some(source), ..Default::default() };
    let db = Database::create(&path, cx.schema.clone(), opts)?;
    db.bulk_load(cx.docs.clone())?;
    let mut live: Vec<usize> = (0..n).collect();
    let mut rng = rand::rngs::StdRng::seed_from_u64(11);
    let mut rows = Vec::new();
    let mut record = |db: &Database, round: usize, step: &str, live: usize| {
        let st = db.file_stats();
        let seg_bytes: u64 = st.segments.iter().map(|s| s.bytes + s.delete_set_bytes).sum();
        // Bytes of segment data attributable to live documents.
        let live_bytes: f64 = st.segments.iter().map(|s| s.bytes as f64 * (s.n_docs - s.n_deleted) as f64 / s.n_docs.max(1) as f64).sum();
        eprintln!(
            "round {round} {step}: {live} live, file {} KiB, live-data estimate {:.0} KiB",
            st.file_bytes / 1024,
            live_bytes / 1024.0
        );
        rows.push(json!({
            "round": round, "step": step, "live_docs": live, "file_bytes": st.file_bytes,
            "segment_bytes": seg_bytes, "live_bytes_estimate": live_bytes,
            "live_pages": st.live_pages, "free_pages": st.free_pages, "pending_pages": st.pending_pages,
            "segments": st.segments.len(), "space_amp_vs_estimate": st.file_bytes as f64 / live_bytes.max(1.0),
        }));
    };
    record(&db, 0, "load", n);
    for round in 1..=10 {
        live.shuffle(&mut rng);
        let gone: Vec<usize> = live.split_off(live.len() - n / 20);
        for chunk in gone.chunks(100) {
            let mut tx = db.begin()?;
            for &i in chunk {
                tx.delete(&cx.docs[i][data::col::ID])?;
            }
            tx.commit()?;
        }
        db.flush()?;
        record(&db, round, "deletes", live.len());
        if round % 5 == 0 {
            let t = Instant::now();
            let stats = db.merge(&MergePolicy::default())?;
            eprintln!("merge: {} merges in {:.2}s", stats.merges, secs(t));
            record(&db, round, "merge", live.len());
            let t = Instant::now();
            let shrunk = db.vacuum()?;
            eprintln!("vacuum: {} KiB returned in {:.3}s", shrunk / 1024, secs(t));
            record(&db, round, "vacuum", live.len());
        }
    }
    let final_bytes = db.file_stats().file_bytes;
    drop(db);
    remove_db(&path);
    // The same live documents loaded into a fresh file.
    live.sort();
    let compact = cx.fresh("compact.strata");
    let db = Database::create(&compact, cx.schema.clone(), DbOptions { segment_max_docs: seg_max, ..Default::default() })?;
    db.bulk_load(live.iter().map(|&i| cx.docs[i].clone()))?;
    let compact_bytes = db.file_stats().file_bytes;
    drop(db);
    remove_db(&compact);
    eprintln!("final file {} KiB, compact {} KiB", final_bytes / 1024, compact_bytes / 1024);
    Ok(json!({ "rounds": rows, "final_file_bytes": final_bytes, "compact_file_bytes": compact_bytes,
               "final_space_amp": final_bytes as f64 / compact_bytes as f64 }))
}

/// The first snapshot after a write rebuilds the memtable's last chunk;
/// chunk size `usize::MAX` is one segment for the whole memtable.
fn exp_snapshot(cx: &Ctx) -> Result<Json> {
    let mut rows = Vec::new();
    for (n, chunk) in [(100usize, 1024usize), (1000, 1024), (10_000, 1024), (10_000, 4096), (1000, usize::MAX), (10_000, usize::MAX)] {
        let (mut rebuild, mut cached, mut commit) = (vec![], vec![], vec![]);
        let mut segments = 0;
        for _ in 0..cx.args.reps {
            let path = cx.fresh("snap.strata");
            let opts = DbOptions { sync: false, memtable_max_docs: usize::MAX, memtable_chunk_docs: chunk, ..Default::default() };
            let db = Database::create(&path, cx.schema.clone(), opts)?;
            for chunk in (0..n).collect::<Vec<_>>().chunks(1000) {
                let mut tx = db.begin()?;
                for &i in chunk {
                    tx.insert(cx.keyed(i, format!("s{i}")))?;
                }
                tx.commit()?;
            }
            db.snapshot();
            let d = cx.keyed(n, format!("s{n}"));
            let t = Instant::now();
            let mut tx = db.begin()?;
            tx.insert(d)?;
            tx.commit()?;
            commit.push(secs(t));
            let t = Instant::now();
            let s = db.snapshot();
            rebuild.push(secs(t));
            assert_eq!(s.n_live_docs(), n as u64 + 1);
            segments = s.segments().len();
            let t = Instant::now();
            db.snapshot();
            cached.push(secs(t));
            drop(s);
            drop(db);
            remove_db(&path);
        }
        let chunk_label = if chunk == usize::MAX { "all".to_string() } else { chunk.to_string() };
        eprintln!(
            "memtable {n}, chunk {chunk_label}: snapshot after a write {:.2}ms ({segments} segments), cached {:.1}us",
            median(&rebuild) * 1e3,
            median(&cached) * 1e6
        );
        rows.push(json!({
            "memtable_docs": n, "chunk_docs": chunk_label, "memtable_segments": segments, "commit_us": median(&commit) * 1e6,
            "snapshot_after_write_ms": median(&rebuild) * 1e3, "snapshot_cached_us": median(&cached) * 1e6,
            "snapshot_after_write_ms_reps": ms(&rebuild),
        }));
    }
    Ok(json!(rows))
}

/// The `pk` section, parsed as documented in `storage::pk`, to count the
/// pages a lookup touches.
struct PkLayout {
    base: usize,
    bits: u32,
    dir: (usize, Slice<u32>),
    entries: (usize, Slice<u64>),
    offsets: (usize, Slice<u64>),
    bytes: (usize, Slice<u8>),
}

impl PkLayout {
    fn parse(seg: &SegmentFile) -> PkLayout {
        let base = seg.section_range("pk").unwrap().start;
        let mut r = ByteReader::new(seg.section("pk").unwrap());
        let (_, _, bits) = (r.u64().unwrap(), r.u64().unwrap(), r.u64().unwrap() as u32);
        // An array's elements follow its u64 length.
        let arr = |r: &mut ByteReader| -> usize { r.position() + 8 };
        let d0 = arr(&mut r);
        let dir: Slice<u32> = r.array().unwrap();
        let e0 = arr(&mut r);
        let entries: Slice<u64> = r.array().unwrap();
        let o0 = arr(&mut r);
        let offsets: Slice<u64> = r.array().unwrap();
        let b0 = arr(&mut r);
        let bytes: Slice<u8> = r.array().unwrap();
        PkLayout { base, bits, dir: (d0, dir), entries: (e0, entries), offsets: (o0, offsets), bytes: (b0, bytes) }
    }

    fn page(&self, off: usize) -> usize {
        (self.base + off) / iostat::PAGE
    }

    /// Pages read verifying a candidate's stored key.
    fn key_pages(&self, d: usize, pages: &mut BTreeSet<usize>) {
        let (o0, off) = (&self.offsets.0, &self.offsets.1);
        pages.insert(self.page(o0 + 8 * d));
        pages.insert(self.page(o0 + 8 * (d + 1)));
        let (s, e) = (off[d] as usize, off[d + 1] as usize);
        for b in [s, e.max(s + 1) - 1] {
            pages.insert(self.page(self.bytes.0 + b));
        }
    }

    fn radix(&self, h: u64, pages: &mut BTreeSet<usize>) {
        let b = h.checked_shr(64 - self.bits).unwrap_or(0) as usize;
        pages.insert(self.page(self.dir.0 + 4 * b));
        pages.insert(self.page(self.dir.0 + 4 * (b + 1)));
        for i in self.dir.1[b] as usize..self.dir.1[b + 1] as usize {
            pages.insert(self.page(self.entries.0 + 16 * i));
            if self.entries.1[2 * i] == h {
                self.key_pages(self.entries.1[2 * i + 1] as usize, pages);
                return;
            }
            if self.entries.1[2 * i] > h {
                return;
            }
        }
    }

    fn binary(&self, h: u64, pages: &mut BTreeSet<usize>) {
        let (mut lo, mut hi) = (0usize, self.entries.1.len() / 2);
        while lo < hi {
            let mid = (lo + hi) / 2;
            pages.insert(self.page(self.entries.0 + 16 * mid));
            if self.entries.1[2 * mid] < h {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < self.entries.1.len() / 2 && self.entries.1[2 * lo] == h {
            pages.insert(self.page(self.entries.0 + 16 * lo));
            self.key_pages(self.entries.1[2 * lo + 1] as usize, pages);
        }
    }

    fn binary_lookup(&self, h: u64) -> Option<u64> {
        let e = &self.entries.1;
        let (mut lo, mut hi) = (0usize, e.len() / 2);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if e[2 * mid] < h {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo < e.len() / 2 && e[2 * lo] == h).then(|| e[2 * lo + 1])
    }
}

fn exp_pk(cx: &Ctx) -> Result<Json> {
    let path = cx.fresh("pk.strata");
    let db = Database::create(&path, cx.schema.clone(), DbOptions { segment_max_docs: usize::MAX, ..Default::default() })?;
    db.bulk_load(cx.docs.clone())?;
    let snap = db.snapshot();
    let n = cx.docs.len();
    let mut rng = rand::rngs::StdRng::seed_from_u64(3);
    let hits: Vec<Value> = (0..100_000).map(|_| cx.docs[rng.gen_range(0..n)][data::col::ID].clone()).collect();
    let misses: Vec<Value> = (0..100_000).map(|i| Value::Str(format!("missing-{i}"))).collect();
    let layout = PkLayout::parse(&snap.segments()[0].file);
    let mut rows = Vec::new();
    for (kind, keys) in [("hit", &hits), ("miss", &misses)] {
        let (mut radix_ns, mut bin_ns) = (vec![], vec![]);
        for _ in 0..cx.args.reps {
            let t = Instant::now();
            let found = keys.iter().filter(|k| snap.lookup(k).is_some()).count();
            radix_ns.push(secs(t) * 1e9 / keys.len() as f64);
            assert_eq!(found, if kind == "hit" { keys.len() } else { 0 });
            // Binary search over the same entries (hash match only, without
            // the stored-key check the radix lookup also does).
            let t = Instant::now();
            let found = keys.iter().filter(|k| layout.binary_lookup(key_hash(KeyRef::of(k).unwrap())).is_some()).count();
            bin_ns.push(secs(t) * 1e9 / keys.len() as f64);
            assert_eq!(found, if kind == "hit" { keys.len() } else { 0 });
        }
        let (mut rp, mut bp) = (0usize, 0usize);
        for k in keys.iter().take(10_000) {
            let h = key_hash(KeyRef::of(k).unwrap());
            let (mut a, mut b) = (BTreeSet::new(), BTreeSet::new());
            layout.radix(h, &mut a);
            layout.binary(h, &mut b);
            rp += a.len();
            bp += b.len();
        }
        let (rp, bp) = (rp as f64 / 10_000.0, bp as f64 / 10_000.0);
        eprintln!("{kind}: radix {:.0}ns {rp:.2} pages; binary search {:.0}ns {bp:.2} pages", median(&radix_ns), median(&bin_ns));
        rows.push(json!({
            "keys": kind, "n_docs": n,
            "radix_lookup_ns": median(&radix_ns), "binary_search_ns": median(&bin_ns),
            "radix_pages_per_lookup": rp, "binary_search_pages_per_lookup": bp,
        }));
    }
    let pk_bytes = snap.segments()[0].file.section("pk").unwrap().len();
    drop(snap);
    drop(db);
    remove_db(&path);
    Ok(json!({ "pk_section_bytes": pk_bytes, "bytes_per_doc": pk_bytes as f64 / n as f64, "lookups": rows }))
}
