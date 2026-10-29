//! Bodies of Strata's fuzz targets: `fuzz_targets/` runs them under
//! `cargo fuzz` (nightly, AddressSanitizer), and `strata/tests/corrupt_input.rs`
//! includes this file to run them over pseudo-random inputs on stable.
//!
//! The file-format targets read their input as damage to a valid seed
//! ([`damage`]), so that the fuzzer explores files a little off from real
//! ones rather than noise that fails the first check. Every target must
//! return without undefined behaviour, a hang or a runaway allocation.
//! Beyond that:
//! - `loom`, `wal`, `manifest`, `sql` and `text_query` must not panic: a damaged input is
//!   an error, and a list that passes [`Loom::validate`] decodes, seeks and
//!   combines like its docIDs.
//! - `segment` must open without panicking (a damaged segment is an error)
//!   but may panic inside a query: it opens the database without
//!   `DbOptions::check_sections`, so that damage reaches the readers
//!   rather than stopping at a section's checksum, and Strata then trusts
//!   a segment's sections as far as their structure checks go (whole
//!   segments are checked by merges and with `DbOptions::verify_checksums`);
//!   a reader that finds one inconsistent may stop the query. It must not
//!   read outside the file, which AddressSanitizer checks.

use std::ops::Range;
use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use strata::db::Db;
use strata::graph::{edges_value, Edge};
use strata::postings::bytes::OwnedBytes;
use strata::postings::codecs::loom::Loom;
use strata::postings::codecs::{Cursor, CursorList, PostingList, SetOps};
use strata::query::Params;
use strata::storage::freelist::FreeList;
use strata::storage::manifest::Manifest;
use strata::storage::pager::{read_extent, read_metas};
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema, Value, VectorIndexKind};

/// `seed` with the damage `data` describes, in 5-byte steps: a
/// little-endian u32 offset and a byte XORed into the seed there. An
/// offset with its top bit set cuts the bytes there instead, and ends the
/// damage. Offsets wrap around the bytes of `within` (ranges of the seed;
/// empty: all of it).
pub fn damage(seed: &[u8], data: &[u8], within: &[Range<usize>]) -> Vec<u8> {
    let mut b = seed.to_vec();
    let total = if within.is_empty() { b.len() } else { within.iter().map(|r| r.len()).sum() };
    if total == 0 {
        return b;
    }
    for step in data.chunks_exact(5) {
        let off = u32::from_le_bytes(step[..4].try_into().unwrap());
        let mut i = (off & 0x7fff_ffff) as usize % total;
        if !within.is_empty() {
            for r in within {
                if i < r.len() {
                    i += r.start;
                    break;
                }
                i -= r.len();
            }
        }
        if off >> 31 == 1 {
            b.truncate(i);
            break;
        }
        b[i] ^= step[4];
    }
    b
}

/// A deterministic pseudo-random sequence (no `rand` in the fuzz crate).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// Three lists between them using run, bitvector and EF partitions, each
/// with its encoding and built.
fn loom_seeds() -> &'static [(Vec<u32>, Vec<u8>, Loom)] {
    static L: OnceLock<Vec<(Vec<u32>, Vec<u8>, Loom)>> = OnceLock::new();
    L.get_or_init(|| {
        let mut r = Lcg(7);
        let mixed = (0..2000u32).chain((10_000..30_000).filter(|x| x % 3 == 0)).chain((1 << 20..(1 << 20) + 2000).map(|x| x * 7)).collect();
        let mut sparse: Vec<u32> = (0..3000).map(|_| (r.next() % (1 << 22)) as u32).collect();
        sparse.sort_unstable();
        sparse.dedup();
        let mut clustered = Vec::new();
        for _ in 0..40 {
            let start = (r.next() % (1 << 22)) as u32;
            clustered.extend((0..(r.next() % 300) as u32).map(|i| start + i * (1 + (r.next() % 3) as u32)));
        }
        clustered.sort_unstable();
        clustered.dedup();
        [mixed, sparse, clustered]
            .into_iter()
            .map(|docs| {
                let list = Loom::build(&docs, 0);
                (docs, list.to_bytes(), list)
            })
            .collect()
    })
}

fn sorted_and(a: &[u32], b: &[u32]) -> Vec<u32> {
    a.iter().copied().filter(|x| b.binary_search(x).is_ok()).collect()
}

fn sorted_or(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut v: Vec<u32> = a.iter().chain(b).copied().collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// A Loom list read from damaged bytes: rejected, or usable like the
/// docIDs it decodes to (decoding, cursors, seeks, AND and OR).
pub fn loom(data: &[u8]) {
    let Some((&pick, data)) = data.split_first() else { return };
    let seeds = loom_seeds();
    let Ok(list) = Loom::from_bytes(OwnedBytes::new(damage(&seeds[pick as usize % 3].1, data, &[]))) else { return };
    if list.validate().is_err() {
        return;
    }
    let mut docs = Vec::new();
    list.decode_into(&mut docs);
    assert_eq!(docs.len(), list.len());
    assert!(docs.windows(2).all(|w| w[0] < w[1]), "a validated list decodes in order");
    let mut c = list.cursor();
    for &d in &docs {
        assert_eq!(c.docid(), d);
        c.next();
    }
    assert_eq!(c.docid(), u32::MAX);
    let mut c = list.cursor();
    for &t in docs.iter().step_by(61) {
        let t = t.saturating_sub(1);
        assert_eq!(c.next_geq(t), docs[docs.partition_point(|&x| x < t)]);
    }
    let (other, _, other_l) = &seeds[(pick as usize + 1) % 3];
    let mut out = Vec::new();
    Loom::intersect(&[&list, other_l], &mut out);
    assert_eq!(out, sorted_and(&docs, other));
    out.clear();
    Loom::union(&[&list, other_l], &mut out);
    assert_eq!(out, sorted_or(&docs, other));
}

/// The seed database: a table with every kind of index, in two segments.
struct Seed {
    _dir: tempfile::TempDir,
    path: PathBuf,
    /// The database file and the byte ranges of its segments.
    file: Vec<u8>,
    segments: Vec<Range<usize>>,
    /// The manifest and free list of the newest version, and its page count.
    manifest: Vec<u8>,
    freelist: Vec<u8>,
    page_count: u64,
    /// A database whose WAL holds transactions of every kind, and the WAL.
    wal_db: Vec<u8>,
    wal: Vec<u8>,
}

const WORDS: [&str; 12] = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet", "kilo", "lima"];
const DOCS: u64 = 1400;

fn schema() -> Schema {
    Schema::new(
        "docs",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("n", FieldType::Int),
            Field::new("f", FieldType::Float),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("body", FieldType::Text { positions: true }),
            Field::new("splade", FieldType::Sparse),
            Field::new("v", FieldType::Vector { dim: 8, index: VectorIndexKind::Hnsw { m: 8, ef_construction: 32 } }),
            Field::new("cites", FieldType::Edges),
        ],
        Some("id"),
    )
    .unwrap()
}

fn key(i: u64) -> String {
    format!("d{i:05}")
}

fn doc(i: u64, r: &mut Lcg) -> Vec<Value> {
    // Word k is in about (12 - k) / 12 of the documents, so the frequent
    // ones get Loom lists (at least 512 postings in a segment).
    let body: Vec<&str> = WORDS.iter().enumerate().filter(|&(k, _)| r.next() % 12 >= k as u64).map(|(_, w)| *w).collect();
    let sparse: Vec<(String, f32)> = body.iter().take(4).map(|w| (w.to_string(), (r.next() % 100) as f32 / 30.0)).collect();
    let v: Vec<f32> = (0..8).map(|_| (r.next() % 1000) as f32 / 500.0 - 1.0).collect();
    let cites: Vec<Edge> = (0..r.next() % 4).filter(|_| i > 0).map(|_| Edge::to(key(r.next() % i).as_str())).collect();
    vec![
        Value::Str(key(i)),
        Value::Int((r.next() % 1000) as i64),
        Value::Float((r.next() % 10_000) as f64 / 7.0),
        Value::List((0..r.next() % 3).map(|_| Value::Str(format!("t{}", r.next() % 6))).collect()),
        Value::Str(body.join(" ")),
        strata::text::sparse::to_value(&sparse),
        Value::Vector(Arc::from(v)),
        edges_value(&cites),
    ]
}

fn opts() -> DbOptions {
    DbOptions { sync: false, segment_max_docs: 800, memtable_max_docs: 100_000, ..DbOptions::default() }
}

fn seed() -> &'static Seed {
    static S: OnceLock<Seed> = OnceLock::new();
    S.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seed.strata");
        let mut r = Lcg(11);
        {
            let db = Db::create(&path, schema(), opts()).unwrap();
            db.load((0..DOCS).map(|i| doc(i, &mut r))).unwrap();
        }
        let file = std::fs::read(&path).unwrap();
        let f = std::fs::File::open(&path).unwrap();
        let (meta, _) = read_metas(&f).unwrap()[0];
        let manifest = read_extent(&f, meta.manifest, meta.manifest_crc).unwrap();
        let freelist = read_extent(&f, meta.freelist, meta.freelist_crc).unwrap();
        let segments = Manifest::decode(&manifest).unwrap().segments().map(|s| s.extent.range()).collect();

        let wal_path = dir.path().join("wal.strata");
        {
            let db = Db::create(&wal_path, schema(), opts()).unwrap();
            let st = db.storage();
            for t in 0..6u64 {
                let mut tx = st.begin().unwrap();
                for i in t * 10..t * 10 + 10 {
                    tx.insert(doc(i, &mut r)).unwrap();
                }
                if t > 0 {
                    tx.delete(&Value::Str(key(t * 10 - 3))).unwrap();
                    tx.set(&Value::Str(key(t * 10 - 5)), "n", Value::Int(t as i64)).unwrap();
                }
                tx.commit().unwrap();
            }
        }
        let mut wal_file = wal_path.clone().into_os_string();
        wal_file.push("-wal");
        Seed {
            wal_db: std::fs::read(&wal_path).unwrap(),
            wal: std::fs::read(&wal_file).unwrap(),
            _dir: dir,
            path,
            file,
            segments,
            manifest,
            freelist,
            page_count: meta.page_count,
        }
    })
}

/// Queries that between them read every index of the seed's table.
pub const QUERIES: &[&str] = &[
    "SELECT COUNT(*) FROM docs",
    "SELECT * FROM docs ORDER BY n DESC LIMIT 40",
    "SELECT id FROM docs WHERE MATCH(body, 'alpha bravo') LIMIT 50",
    "SELECT COUNT(*) FROM docs WHERE MATCH(body, 'alpha OR kilo -charlie')",
    "SELECT COUNT(*) FROM docs WHERE MATCH(body, '\"bravo charlie\"')",
    "SELECT COUNT(*) FROM docs WHERE MATCH(body, 'delt* AND echoo~1')",
    "SELECT id, score() FROM docs ORDER BY bm25(body, 'alpha delta lima') DESC LIMIT 10",
    "SELECT id FROM docs WHERE MATCH(body, 'golf') ORDER BY bm25(body, 'golf hotel') DESC LIMIT 5",
    "SELECT id, n FROM docs WHERE n BETWEEN 100 AND 400 ORDER BY f DESC LIMIT 10",
    "SELECT tags, COUNT(*) FROM docs GROUP BY tags",
    "SELECT id FROM docs WHERE tags = 't3' AND n > 50 LIMIT 20",
    "SELECT AVG(f), SUM(n), MIN(id), MAX(n) FROM docs WHERE MATCH(body, 'india')",
    "SELECT id FROM docs ORDER BY similarity(v, :q) DESC LIMIT 10",
    "SELECT id FROM docs WHERE n < 300 ORDER BY similarity(v, :q) DESC LIMIT 10",
    "SELECT id FROM docs ORDER BY similarity(splade, '{\"alpha\": 1.0, \"echo\": 2.0}') DESC LIMIT 10",
    "SELECT COUNT(*) FROM docs WHERE REACHABLE(cites, 'd01000', hops => 3)",
    "SELECT id FROM docs WHERE id IN ('d00001', 'd00700', 'd01399')",
    "SELECT a.id, b.id FROM docs a JOIN docs b ON a.cites = b.id WHERE a.n < 20",
];

fn params() -> Params {
    Params::new().set("q", Value::Vector(Arc::from(vec![0.5f32, -0.25, 0.0, 1.0, -1.0, 0.75, 0.1, -0.6])))
}

/// Runs every query of [`QUERIES`], each until it errs or panics.
fn run_queries(db: &Db) {
    let p = params();
    for q in QUERIES {
        let _ = quietly(|| db.query(q, &p));
    }
}

/// A scratch directory per thread, for the damaged copies.
fn scratch() -> PathBuf {
    thread_local!(static DIR: tempfile::TempDir = tempfile::tempdir().unwrap());
    DIR.with(|d| d.path().to_path_buf())
}

fn write_db(dir: &Path, main: &[u8], wal: Option<&[u8]>) -> PathBuf {
    let path = dir.join("db.strata");
    let mut wal_path = path.clone().into_os_string();
    wal_path.push("-wal");
    let _ = std::fs::remove_file(&wal_path);
    std::fs::write(&path, main).unwrap();
    if let Some(w) = wal {
        std::fs::write(&wal_path, w).unwrap();
    }
    path
}

/// The seed database with damaged segments, opened and queried.
pub fn segment(data: &[u8]) {
    let s = seed();
    let path = write_db(&scratch(), &damage(&s.file, data, &s.segments), None);
    if let Ok(db) = Db::open(&path, DbOptions { check_sections: false, ..opts() }) {
        run_queries(&db);
    }
}

/// Fixes the checksum of every whole record of a WAL, so that damage
/// reaches the record decoder rather than stopping at the checksum.
fn reseal_wal(b: &mut [u8]) {
    let mut pos = 16;
    while pos + 8 <= b.len() {
        let len = u32::from_le_bytes(b[pos..pos + 4].try_into().unwrap()) as usize;
        let Some(end) = (pos + 8).checked_add(len).filter(|&e| e <= b.len()) else { break };
        let crc = crc32fast::hash(&b[pos + 8..end]);
        b[pos + 4..pos + 8].copy_from_slice(&crc.to_le_bytes());
        pos = end;
    }
}

/// The seed database with a damaged WAL (resealed or not, opened with or
/// without `sync`), opened and queried. Must not panic.
pub fn wal(data: &[u8]) {
    let Some((&mode, data)) = data.split_first() else { return };
    let s = seed();
    // Past the header, which only says whose log it is.
    let records = 16..s.wal.len();
    let mut wal = damage(&s.wal, data, std::slice::from_ref(&records));
    if mode & 1 == 1 {
        reseal_wal(&mut wal);
    }
    let path = write_db(&scratch(), &s.wal_db, Some(&wal));
    if let Ok(db) = Db::open(&path, DbOptions { sync: mode & 2 == 2, wal_drop_damaged: mode & 4 == 4, ..opts() }) {
        let p = params();
        for q in &QUERIES[..4] {
            let _ = db.query(q, &p);
        }
    }
}

/// A damaged manifest and free list, decoded. Must not panic.
pub fn manifest(data: &[u8]) {
    let s = seed();
    let Some((&which, data)) = data.split_first() else { return };
    if which & 1 == 0 {
        if let Ok(m) = Manifest::decode(&damage(&s.manifest, data, &[])) {
            let _ = Manifest::decode(&m.encode());
        }
    } else {
        let _ = FreeList::decode(&damage(&s.freelist, data, &[]), s.page_count);
    }
}

/// The seed database, read-only and opened once.
fn seed_db() -> &'static Db {
    static D: OnceLock<Db> = OnceLock::new();
    D.get_or_init(|| Db::open(&seed().path, opts()).unwrap())
}

/// Every query of [`QUERIES`] runs on the intact seed database (else the
/// targets would test less than they claim).
pub fn intact() {
    let db = seed_db();
    let failed: Vec<String> = QUERIES.iter().filter_map(|q| db.query(q, &params()).err().map(|e| format!("{q}: {e}"))).collect();
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Any text as SQL against the seed database. Must not panic.
pub fn sql(data: &[u8]) {
    let q = String::from_utf8_lossy(data);
    let _ = seed_db().query(&q, &params());
}

/// Any text as a MATCH query and as BM25 terms against the seed database.
/// Must not panic.
pub fn text_query(data: &[u8]) {
    let q = String::from_utf8_lossy(data).replace('\'', "''");
    let db = seed_db();
    for sql in [
        format!("SELECT COUNT(*) FROM docs WHERE MATCH(body, '{q}')"),
        format!("SELECT id FROM docs WHERE MATCH(body, '{q}') ORDER BY bm25(body, '{q}') DESC LIMIT 5"),
    ] {
        let _ = db.query(&sql, &params());
    }
}

thread_local!(static QUIET: Cell<bool> = const { Cell::new(false) });

/// Runs `f`, catching a panic without reporting it. Other panics still go
/// to the hook that was set before (cargo fuzz's aborts the process).
fn quietly<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
    static HOOK: OnceLock<()> = OnceLock::new();
    HOOK.get_or_init(|| {
        let outer = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !QUIET.with(Cell::get) {
                outer(info)
            }
        }));
    });
    QUIET.with(|q| q.set(true));
    let r = catch_unwind(AssertUnwindSafe(f));
    QUIET.with(|q| q.set(false));
    r
}
