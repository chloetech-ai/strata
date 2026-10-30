//! Scale probe on MS MARCO v2.1 passages (`notes/scale.md`): load, merge and
//! query one table as it grows, one step per invocation, each appending one
//! JSON line to `--out`.
//!
//! * `load`: bulk-loads rows `start..end` of a directory prepared by
//!   `scripts/prepare_msmarco.py` into segments of `--segment-docs`.
//! * `merge`: merges segments under a tiered policy until it selects nothing.
//! * `query`: warm latency of BM25, filtered BM25, vector, filtered vector
//!   and hybrid top-10 queries; vector recall@10 against exact search
//!   (`scripts/exact_topk.py`); the same queries with the segments split over
//!   `--parallel` threads (`ExecOptions::segments`, merged by score); and
//!   throughput with `--clients` concurrent callers.
//! * `stats`: file size, segments, bytes by component.
//! * `vacuum`: returns free space to the file system, timed.
//!
//! Every line names the machine. Run from `strata/` with `scripts/scale_probe.sh`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{json, Value as Json};

use strata::data::beir_schema;
use strata::db::Db;
use strata::metrics::{mean, percentile};
use strata::query::exec::ExecOptions;
use strata::query::Params;
use strata::segment::build::{take_build_times, BuildOptions};
use strata::storage::{DbOptions, MergePolicy, Snapshot};
use strata::{Document, Value, VectorIndexKind};

type Res<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Parser)]
#[command(about = "Strata scale probe on MS MARCO v2.1 passages")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
    /// JSON-lines file each step appends its result to.
    #[arg(long, global = true, default_value = "results/scale/steps.jsonl")]
    out: PathBuf,
    /// Free-form label stored with the step.
    #[arg(long, global = true, default_value = "")]
    label: String,
    /// Threads for vector index builds.
    #[arg(long, global = true, default_value_t = 4)]
    threads: usize,
}

#[derive(Subcommand)]
enum Cmd {
    Load {
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        start: usize,
        #[arg(long)]
        end: usize,
        #[arg(long, default_value_t = 100_000)]
        segment_docs: usize,
    },
    Merge {
        #[arg(long)]
        db: PathBuf,
        #[arg(long, default_value_t = 8)]
        merge_factor: usize,
        #[arg(long, default_value_t = 1000)]
        floor_docs: usize,
        #[arg(long, default_value_t = 2_000_000)]
        max_merged_docs: usize,
    },
    Query {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        data: PathBuf,
        /// Directory with queries.jsonl and query_vecs.f32.
        #[arg(long)]
        queries: PathBuf,
        /// Exact top-k ids per query (`scripts/exact_topk.py`) for this table's rows.
        #[arg(long)]
        gt: Option<PathBuf>,
        #[arg(long, default_value_t = 1000)]
        n: usize,
        /// Queries for the slower kinds (filtered, hybrid, parallel, throughput).
        #[arg(long, default_value_t = 300)]
        n_slow: usize,
        /// Threads for the segment-parallel runs (0: skip them).
        #[arg(long, default_value_t = 4)]
        parallel: usize,
        /// Concurrent callers for the throughput runs (0: skip them).
        #[arg(long, default_value_t = 4)]
        clients: usize,
        /// Kinds to run (default all).
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
    },
    Stats {
        #[arg(long)]
        db: PathBuf,
    },
    /// Returns free space to the file system (`Database::vacuum`).
    Vacuum {
        #[arg(long)]
        db: PathBuf,
    },
}

/// Highest anonymous resident memory seen, in KiB. `ru_maxrss` also counts
/// the file pages a step maps (queries and merges read the whole table
/// through the mapping), so heap use is sampled separately.
static PEAK_ANON_KIB: AtomicU64 = AtomicU64::new(0);

fn sample_anon() {
    let read = || -> Option<u64> {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        s.lines().find(|l| l.starts_with("RssAnon:"))?.split_whitespace().nth(1)?.parse().ok()
    };
    std::thread::spawn(move || loop {
        if let Some(k) = read() {
            PEAK_ANON_KIB.fetch_max(k, Ordering::Relaxed);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    });
}

fn main() {
    let args = Args::parse();
    sample_anon();
    let t = Instant::now();
    let result = match &args.cmd {
        Cmd::Load { data, db, start, end, segment_docs } => load(&args, data, db, *start, *end, *segment_docs),
        Cmd::Merge { db, merge_factor, floor_docs, max_merged_docs } => {
            merge(&args, db, MergePolicy { merge_factor: *merge_factor, floor_docs: *floor_docs, max_merged_docs: *max_merged_docs, ..MergePolicy::default() })
        }
        Cmd::Query { db, data, queries, gt, n, n_slow, parallel, clients, kinds } => {
            query(db, data, queries, gt.as_deref(), *n, *n_slow, *parallel, *clients, kinds)
        }
        Cmd::Stats { db } => Db::open(db, opts(&args, 100_000)).map_err(|e| e.into()).map(|d| stats(&d, db)),
        Cmd::Vacuum { db } => vacuum(&args, db),
    };
    let mut doc = match result {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let step = match &args.cmd {
        Cmd::Load { .. } => "load",
        Cmd::Merge { .. } => "merge",
        Cmd::Query { .. } => "query",
        Cmd::Stats { .. } => "stats",
        Cmd::Vacuum { .. } => "vacuum",
    };
    let obj = doc.as_object_mut().unwrap();
    obj.insert("step".into(), json!(step));
    obj.insert("label".into(), json!(args.label));
    obj.insert("wall_seconds".into(), json!(t.elapsed().as_secs_f64()));
    obj.insert("peak_rss_mib".into(), json!(peak_rss_mib()));
    obj.insert("peak_anon_mib".into(), json!(PEAK_ANON_KIB.load(Ordering::Relaxed) as f64 / 1024.0));
    obj.insert("machine".into(), machine());
    obj.insert("unix_time".into(), json!(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())));
    if let Some(dir) = args.out.parent() {
        std::fs::create_dir_all(dir).expect("results directory");
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&args.out).expect("results file");
    writeln!(f, "{}", serde_json::to_string(&doc).unwrap()).expect("write results");
    eprintln!(
        "{step} {}: {:.1}s, peak RSS {:.0} MiB (anonymous {:.0} MiB)",
        args.label,
        t.elapsed().as_secs_f64(),
        peak_rss_mib(),
        PEAK_ANON_KIB.load(Ordering::Relaxed) as f64 / 1024.0
    );
}

fn opts(args: &Args, segment_docs: usize) -> DbOptions {
    DbOptions { build: BuildOptions::default().set("vector.threads", args.threads), segment_max_docs: segment_docs, ..DbOptions::default() }
}

fn peak_rss_mib() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    u.ru_maxrss as f64 / 1024.0
}

fn machine() -> Json {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_default();
    let mem_kib: u64 = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("MemTotal")).and_then(|l| l.split_whitespace().nth(1).and_then(|x| x.parse().ok())))
        .unwrap_or(0);
    json!({
        "cpu": cpu,
        "cores": std::thread::available_parallelism().map_or(0, |n| n.get()),
        "mem_gib": mem_kib as f64 / (1 << 20) as f64,
        "host": "cloud container (Claude Code on the web)",
    })
}

fn db_bytes(path: &Path) -> u64 {
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    [path.to_path_buf(), wal].iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum()
}

// ---- data ------------------------------------------------------------------

#[derive(Deserialize)]
struct Meta {
    dim: usize,
    rows: usize,
    shards: Vec<ShardMeta>,
}

#[derive(Deserialize, Clone)]
struct ShardMeta {
    shard: usize,
    rows: usize,
    start: usize,
}

#[derive(Deserialize)]
struct Passage {
    docid: String,
    title: String,
    segment: String,
}

fn read_meta(data: &Path) -> Res<Meta> {
    Ok(serde_json::from_reader(File::open(data.join("meta.json"))?)?)
}

/// Rows `start..end` of a prepared directory as documents of `beir_schema`.
struct Rows {
    data: PathBuf,
    dim: usize,
    shards: Vec<ShardMeta>,
    next: usize,
    end: usize,
    vecs: BufReader<File>,
    attrs: BufReader<File>,
    text: Option<(usize, Child, BufReader<std::process::ChildStdout>)>,
    line: String,
}

impl Rows {
    fn open(data: &Path, start: usize, end: usize) -> Res<Rows> {
        let meta = read_meta(data)?;
        if end > meta.rows || start > end {
            return Err(format!("rows {start}..{end} out of 0..{}", meta.rows).into());
        }
        let mut vecs = BufReader::with_capacity(1 << 22, File::open(data.join("vecs.f32"))?);
        vecs.seek(SeekFrom::Start((start * meta.dim * 4) as u64))?;
        let mut attrs = BufReader::with_capacity(1 << 20, File::open(data.join("attrs.bin"))?);
        attrs.seek(SeekFrom::Start((start * 8) as u64))?;
        Ok(Rows { data: data.to_path_buf(), dim: meta.dim, shards: meta.shards, next: start, end, vecs, attrs, text: None, line: String::new() })
    }

    fn next_text(&mut self) -> Res<Passage> {
        let shard = self.shards.iter().find(|s| (s.start..s.start + s.rows).contains(&self.next)).cloned().ok_or("row past the last shard")?;
        if self.text.as_ref().is_none_or(|t| t.0 != shard.shard) {
            if let Some((_, mut child, _)) = self.text.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let mut child = Command::new("zcat").arg(self.data.join(format!("text_{:02}.json.gz", shard.shard))).stdout(Stdio::piped()).spawn()?;
            let mut r = BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
            let mut skip = String::new();
            for _ in shard.start..self.next {
                skip.clear();
                r.read_line(&mut skip)?;
            }
            self.text = Some((shard.shard, child, r));
        }
        let (_, _, r) = self.text.as_mut().unwrap();
        self.line.clear();
        if r.read_line(&mut self.line)? == 0 {
            return Err("text ended early".into());
        }
        Ok(serde_json::from_str(&self.line)?)
    }

    fn row(&mut self) -> Res<Document> {
        let p = self.next_text()?;
        let mut v = vec![0u8; self.dim * 4];
        self.vecs.read_exact(&mut v)?;
        let v: Arc<[f32]> = v.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect::<Vec<_>>().into();
        let mut a = [0u8; 8];
        self.attrs.read_exact(&mut a)?;
        let year = u16::from_le_bytes([a[0], a[1]]) as i64;
        let rating = (f32::from_le_bytes([a[4], a[5], a[6], a[7]]) as f64 * 100.0).round() / 100.0;
        self.next += 1;
        let body = if p.title.is_empty() { p.segment.clone() } else { format!("{}\n{}", p.title, p.segment) };
        Ok(vec![
            Value::Str(p.docid),
            Value::Str(p.title),
            Value::Str(p.segment),
            Value::Str(body),
            Value::Int(year),
            Value::Str(format!("c{:02}", a[2])),
            Value::Float(rating),
            Value::Str(format!("t{:02}", a[3])),
            Value::Vector(v),
        ])
    }
}

impl Iterator for Rows {
    type Item = Document;
    fn next(&mut self) -> Option<Document> {
        if self.next >= self.end {
            return None;
        }
        Some(self.row().unwrap_or_else(|e| panic!("row {}: {e}", self.next)))
    }
}

impl Drop for Rows {
    fn drop(&mut self) {
        if let Some((_, mut child, _)) = self.text.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---- load, merge, stats ------------------------------------------------------

fn schema(dim: usize) -> strata::Schema {
    beir_schema(dim, VectorIndexKind::Hnsw { m: 16, ef_construction: 200 })
}

fn load(args: &Args, data: &Path, path: &Path, start: usize, end: usize, segment_docs: usize) -> Res<Json> {
    let meta = read_meta(data)?;
    let db = if path.exists() { Db::open(path, opts(args, segment_docs))? } else { Db::create(path, schema(meta.dim), opts(args, segment_docs))? };
    let before = db.snapshot().n_live_docs();
    let rows = Rows::open(data, start, end)?;
    take_build_times();
    let t = Instant::now();
    db.load(rows)?;
    let seconds = t.elapsed().as_secs_f64();
    let builds = take_build_times();
    let n = end - start;
    eprintln!("load {start}..{end}: {seconds:.1}s, {:.0} docs/s", n as f64 / seconds);
    Ok(json!({
        "rows": [start, end],
        "docs_before": before,
        "docs_loaded": n,
        "segment_docs": segment_docs,
        "threads": args.threads,
        "seconds": seconds,
        "docs_per_second": n as f64 / seconds,
        "build_seconds": builds,
        "after": stats(&db, path),
    }))
}

fn merge(args: &Args, path: &Path, policy: MergePolicy) -> Res<Json> {
    let db = Db::open(path, opts(args, 100_000))?;
    let before = stats(&db, path);
    take_build_times();
    let st = db.storage().merge(&policy)?;
    let builds = take_build_times();
    let build_total: f64 = builds.values().sum();
    eprintln!("merge: {} merges, {} docs rewritten, {:.1}s ({build_total:.1}s building)", st.merges, st.docs_merged, st.seconds);
    Ok(json!({
        "policy": {"merge_factor": policy.merge_factor, "floor_docs": policy.floor_docs, "max_merged_docs": policy.max_merged_docs},
        "threads": args.threads,
        "merge": st,
        "build_seconds": builds,
        "rebuild_docs_seconds": st.seconds - build_total,
        "before": before,
        "after": stats(&db, path),
    }))
}

fn vacuum(args: &Args, path: &Path) -> Res<Json> {
    let db = Db::open(path, opts(args, 100_000))?;
    let before = stats(&db, path);
    let t = Instant::now();
    let shrank = db.storage().vacuum()?;
    let seconds = t.elapsed().as_secs_f64();
    eprintln!("vacuum: {:.2} GiB returned in {seconds:.1}s", shrank as f64 / (1u64 << 30) as f64);
    Ok(json!({"seconds": seconds, "bytes_returned": shrank, "before": before, "after": stats(&db, path)}))
}

fn stats(db: &Db, path: &Path) -> Json {
    let snap = db.snapshot();
    let fs = db.storage().file_stats();
    let mut comp: BTreeMap<String, u64> = BTreeMap::new();
    for v in snap.segments() {
        for (name, len) in v.file.section_sizes() {
            let mut parts = name.split('.');
            let (kind, field, part) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let key = match kind {
                "text" => format!("text {field} ({part})"),
                "vec" => format!("vector {field} ({part})"),
                "col" | "range" | "kw" => "structured".to_string(),
                "store" => "doc store".to_string(),
                other => other.to_string(),
            };
            *comp.entry(key).or_insert(0) += len as u64;
        }
    }
    let n = snap.n_live_docs();
    json!({
        "docs": n,
        "segments": snap.segments().len(),
        "segment_docs": snap.segments().iter().map(|v| v.n_live).collect::<Vec<_>>(),
        "file_bytes": fs.file_bytes,
        "disk_bytes": db_bytes(path),
        "live_pages": fs.live_pages,
        "free_pages": fs.free_pages,
        "pending_pages": fs.pending_pages,
        "bytes_per_doc": fs.live_pages as f64 * 4096.0 / n.max(1) as f64,
        "components": comp,
    })
}

// ---- queries -----------------------------------------------------------------

struct Q {
    text: String,
    vec: Arc<[f32]>,
    topic: String,
}

#[derive(Deserialize)]
struct RawQuery {
    text: String,
}

#[derive(Deserialize)]
struct Gt {
    rows: usize,
    ids: Vec<Vec<String>>,
}

fn read_f32(path: &Path, rows: usize, dim: usize) -> Res<Vec<Arc<[f32]>>> {
    let mut b = Vec::new();
    File::open(path)?.read_to_end(&mut b)?;
    if b.len() < rows * dim * 4 {
        return Err(format!("{} holds fewer than {rows} rows", path.display()).into());
    }
    Ok(b.chunks_exact(dim * 4).take(rows).map(|r| r.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect::<Vec<_>>().into()).collect())
}

fn read_queries(data: &Path, dir: &Path, n: usize) -> Res<Vec<Q>> {
    let meta = read_meta(data)?;
    let texts: Vec<String> = BufReader::new(File::open(dir.join("queries.jsonl"))?)
        .lines()
        .take(n)
        .map(|l| Ok(serde_json::from_str::<RawQuery>(&l?)?.text))
        .collect::<Res<_>>()?;
    let vecs = read_f32(&dir.join("query_vecs.f32"), texts.len(), meta.dim)?;
    let cents = read_f32(&data.join("centroids.f32"), 32, meta.dim)?;
    Ok(texts
        .into_iter()
        .zip(vecs)
        .map(|(text, vec)| {
            let best = (0..cents.len())
                .max_by(|&a, &b| dot(&vec, &cents[a]).total_cmp(&dot(&vec, &cents[b])))
                .unwrap_or(0);
            Q { text, vec, topic: format!("t{best:02}") }
        })
        .collect())
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Query `i`'s filter: three shapes in turn, broad, selective and
/// uncorrelated, selective and correlated with the query (its own topic).
fn filter(q: &Q, i: usize) -> String {
    match i % 3 {
        0 => "year BETWEEN 2015 AND 2024".into(),
        1 => "category = 'c10'".into(),
        _ => format!("topic = '{}' AND year >= 2000", q.topic),
    }
}

/// The SQL of each kind for query `i`; `:t` is the text, `:q` the vector.
fn sql(kind: &str, q: &Q, i: usize) -> String {
    let f = filter(q, i);
    let s = "SELECT id, score() FROM docs";
    match kind {
        "bm25" => format!("{s} ORDER BY bm25(body, :t) DESC LIMIT 10"),
        "bm25_filtered" => format!("{s} WHERE {f} ORDER BY bm25(body, :t) DESC LIMIT 10"),
        "vector" => format!("{s} ORDER BY similarity(emb, :q) DESC LIMIT 10"),
        "vector_ef128" => format!("{s} ORDER BY similarity(emb, :q, ef => 128) DESC LIMIT 10"),
        "vector_ef256" => format!("{s} ORDER BY similarity(emb, :q, ef => 256) DESC LIMIT 10"),
        "vector_ef256_rerank200" => format!("{s} ORDER BY similarity(emb, :q, ef => 256, rerank => 200) DESC LIMIT 10"),
        "vector_flat" => format!("{s} ORDER BY similarity(emb, :q, strategy => 'flat') DESC LIMIT 10"),
        "vector_flat_rerank200" => format!("{s} ORDER BY similarity(emb, :q, strategy => 'flat', rerank => 200) DESC LIMIT 10"),
        "vector_filtered" => format!("{s} WHERE {f} ORDER BY similarity(emb, :q) DESC LIMIT 10"),
        "vector_filtered_exact" => format!("{s} WHERE {f} ORDER BY similarity(emb, :q, strategy => 'prefilter', rerank => 100) DESC LIMIT 10"),
        "hybrid_rrf" => format!("{s} ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT 10"),
        "hybrid_weighted" => format!("{s} ORDER BY 0.3 * minmax(bm25(body, :t)) + 0.7 * minmax(similarity(emb, :q)) DESC LIMIT 10"),
        "bm25_top100" => format!("{s} ORDER BY bm25(body, :t) DESC LIMIT 100"),
        "vector_top100" => format!("{s} ORDER BY similarity(emb, :q) DESC LIMIT 100"),
        other => panic!("unknown kind {other}"),
    }
}

const KINDS: &[&str] =
    &["bm25", "bm25_filtered", "vector", "vector_ef128", "vector_ef256", "vector_filtered", "vector_filtered_exact", "hybrid_rrf", "hybrid_weighted"];
const SLOW: &[&str] = &["bm25_filtered", "vector_filtered", "vector_filtered_exact", "hybrid_rrf", "hybrid_weighted"];
const PARALLEL: &[&str] = &["bm25", "bm25_filtered", "vector", "vector_filtered", "hybrid_rrf"];
const THROUGHPUT: &[&str] = &["bm25", "vector", "hybrid_rrf"];

fn params(q: &Q) -> Params {
    Params::new().set("q", Value::Vector(q.vec.clone())).set("t", Value::Str(q.text.clone()))
}

type Hits = Vec<(String, f64)>;

fn run(db: &Db, snap: &Snapshot, sql: &str, p: &Params, opts: &ExecOptions) -> Res<Hits> {
    let rs = db.query_at(snap, sql, p, opts)?;
    Ok(rs.rows.iter().map(|r| (r[0].as_str().unwrap_or("").to_string(), r[1].as_f64().unwrap_or(0.0))).collect())
}

fn top(mut hits: Hits, k: usize) -> Hits {
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    hits.truncate(k);
    hits
}

/// Segment ordinals split into `p` groups of about equal documents.
fn groups(snap: &Snapshot, p: usize) -> Vec<Arc<[usize]>> {
    let mut segs: Vec<(usize, u32)> = snap.segments().iter().enumerate().map(|(i, v)| (i, v.n_live)).collect();
    segs.sort_by_key(|&(i, n)| (std::cmp::Reverse(n), i));
    let mut g: Vec<(u64, Vec<usize>)> = vec![(0, Vec::new()); p.min(segs.len()).max(1)];
    for (i, n) in segs {
        let least = g.iter_mut().min_by_key(|x| x.0).unwrap();
        least.0 += n as u64;
        least.1.push(i);
    }
    g.into_iter().map(|(_, v)| v.into()).collect()
}

/// One worker thread per segment group, each running its group's part of a
/// query, so the segment-parallel runs pay a channel hand-off per part rather
/// than a thread spawn.
struct Pool {
    workers: Vec<Sender<Job>>,
}

struct Job {
    sql: Arc<String>,
    params: Arc<Params>,
    tag: usize,
    reply: Sender<(usize, std::result::Result<Hits, String>)>,
}

impl Pool {
    fn new(db: &Arc<Db>, snap: &Snapshot, groups: &[Arc<[usize]>]) -> Pool {
        let workers = groups
            .iter()
            .map(|g| {
                let (tx, rx) = channel::<Job>();
                let (db, snap) = (db.clone(), snap.clone());
                let o = ExecOptions { segments: Some(g.clone()), ..ExecOptions::default() };
                std::thread::spawn(move || {
                    for j in rx {
                        let r = run(&db, &snap, &j.sql, &j.params, &o).map_err(|e| e.to_string());
                        let _ = j.reply.send((j.tag, r));
                    }
                });
                tx
            })
            .collect();
        Pool { workers }
    }

    /// Each query of `sqls` (with its k) on every group at once, the parts of
    /// each merged by score.
    fn fetch(&self, sqls: &[(String, usize)], params: Params) -> Res<Vec<Hits>> {
        let params = Arc::new(params);
        let (tx, rx) = channel();
        for (tag, (sql, _)) in sqls.iter().enumerate() {
            let sql = Arc::new(sql.clone());
            for w in &self.workers {
                w.send(Job { sql: sql.clone(), params: params.clone(), tag, reply: tx.clone() })?;
            }
        }
        drop(tx);
        let mut parts: Vec<Hits> = vec![Vec::new(); sqls.len()];
        for (tag, r) in rx {
            parts[tag].extend(r?);
        }
        Ok(parts.into_iter().zip(sqls).map(|(h, (_, k))| top(h, *k)).collect())
    }
}

/// One query with its segments split over the pool's groups, top lists
/// merged by score. `hybrid_rrf` fetches the BM25 and vector top-100 that
/// way and fuses them here (k = 60, as `rrf` does).
fn run_parallel(pool: &Pool, kind: &str, q: &Q, i: usize) -> Res<Hits> {
    if kind != "hybrid_rrf" {
        return Ok(pool.fetch(&[(sql(kind, q, i), 10)], params(q))?.remove(0));
    }
    let lists = pool.fetch(&[(sql("bm25_top100", q, i), 100), (sql("vector_top100", q, i), 100)], params(q))?;
    let mut fused: HashMap<String, f64> = HashMap::new();
    for list in lists {
        for (rank, (id, _)) in list.into_iter().enumerate() {
            *fused.entry(id).or_insert(0.0) += 1.0 / (60.0 + rank as f64 + 1.0);
        }
    }
    Ok(top(fused.into_iter().collect(), 10))
}

fn lat(ms: &[f64]) -> Json {
    json!({"n": ms.len(), "mean_ms": mean(ms), "p50_ms": percentile(ms, 50.0), "p90_ms": percentile(ms, 90.0), "p99_ms": percentile(ms, 99.0), "max_ms": ms.iter().cloned().fold(0.0, f64::max)})
}

fn overlap(a: &Hits, b: &[String]) -> f64 {
    let want: HashSet<&str> = b.iter().take(10).map(|s| s.as_str()).collect();
    if want.is_empty() {
        return 1.0;
    }
    a.iter().take(10).filter(|(id, _)| want.contains(id.as_str())).count() as f64 / want.len() as f64
}

#[allow(clippy::too_many_arguments)]
fn query(path: &Path, data: &Path, qdir: &Path, gt: Option<&Path>, n: usize, n_slow: usize, parallel: usize, clients: usize, kinds: &[String]) -> Res<Json> {
    let db = Arc::new(Db::open(path, DbOptions::default())?);
    let snap = db.snapshot();
    let qs = read_queries(data, qdir, n)?;
    let gt: Option<Gt> = match gt {
        Some(p) => Some(serde_json::from_reader(BufReader::new(File::open(p)?))?),
        None => None,
    };
    if let Some(g) = &gt {
        if g.rows as u64 != snap.n_live_docs() {
            return Err(format!("ground truth is for {} rows, the table holds {}", g.rows, snap.n_live_docs()).into());
        }
    }
    let kinds: Vec<&str> = if kinds.is_empty() { KINDS.to_vec() } else { kinds.iter().map(|s| s.as_str()).collect() };
    let opts = ExecOptions::default();
    let mut out = BTreeMap::new();
    let mut serial: HashMap<&str, Vec<Hits>> = HashMap::new();
    for &kind in &kinds {
        let m = if SLOW.contains(&kind) { n_slow.min(qs.len()) } else { qs.len() };
        // Warm-up pass (its results are the ones checked), then the timed pass.
        let mut res = Vec::with_capacity(m);
        for (i, q) in qs.iter().take(m).enumerate() {
            res.push(run(&db, &snap, &sql(kind, q, i), &params(q), &opts)?);
        }
        let mut ms = Vec::with_capacity(m);
        for (i, q) in qs.iter().take(m).enumerate() {
            let (s, p) = (sql(kind, q, i), params(q));
            let t = Instant::now();
            run(&db, &snap, &s, &p, &opts)?;
            ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let mut doc = json!({"latency": lat(&ms)});
        if kind.starts_with("vector") && !kind.contains("filtered") {
            if let Some(g) = &gt {
                let r: Vec<f64> = res.iter().zip(&g.ids).map(|(h, t)| overlap(h, t)).collect();
                doc["recall_at_10"] = json!(mean(&r));
            }
        }
        let empty = res.iter().filter(|r| r.is_empty()).count();
        doc["empty_results"] = json!(empty);
        eprintln!("   {kind:<22} {m} queries: p50 {:.2} ms, p99 {:.2} ms{}", percentile(&ms, 50.0), percentile(&ms, 99.0), doc.get("recall_at_10").map_or(String::new(), |r| format!(", recall@10 {:.3}", r.as_f64().unwrap())));
        out.insert(kind.to_string(), doc);
        serial.insert(kind, res);
    }
    // Filtered vector recall against the prefilter scan of the same filter.
    if let (Some(a), Some(b)) = (serial.get("vector_filtered"), serial.get("vector_filtered_exact")) {
        let r: Vec<f64> = a.iter().zip(b).map(|(x, y)| overlap(x, &y.iter().map(|h| h.0.clone()).collect::<Vec<_>>())).collect();
        out.get_mut("vector_filtered").unwrap()["recall_at_10_vs_prefilter"] = json!(mean(&r));
        eprintln!("   vector_filtered recall@10 vs prefilter scan {:.3}", mean(&r));
    }

    let mut par = BTreeMap::new();
    if parallel > 0 {
        let g = groups(&snap, parallel);
        let pool = Pool::new(&db, &snap, &g);
        for &kind in PARALLEL.iter().filter(|k| kinds.contains(k)) {
            let m = n_slow.min(qs.len());
            let mut ms = Vec::with_capacity(m);
            let mut agree = Vec::with_capacity(m);
            for (i, q) in qs.iter().take(m).enumerate() {
                run_parallel(&pool, kind, q, i)?;
            }
            for (i, q) in qs.iter().take(m).enumerate() {
                let t = Instant::now();
                let got = run_parallel(&pool, kind, q, i)?;
                ms.push(t.elapsed().as_secs_f64() * 1e3);
                agree.push(overlap(&got, &serial[kind][i].iter().map(|h| h.0.clone()).collect::<Vec<_>>()));
            }
            eprintln!("   parallel {kind:<13} {} groups: p50 {:.2} ms, p99 {:.2} ms, agrees with serial {:.3}", g.len(), percentile(&ms, 50.0), percentile(&ms, 99.0), mean(&agree));
            par.insert(kind.to_string(), json!({"latency": lat(&ms), "agreement_with_serial": mean(&agree)}));
        }
        par.insert("_groups".into(), json!(g.iter().map(|x| x.to_vec()).collect::<Vec<_>>()));
    }

    let mut thr = BTreeMap::new();
    if clients > 0 {
        for &kind in THROUGHPUT.iter().filter(|k| kinds.contains(k)) {
            let m = n_slow.min(qs.len());
            let next = AtomicUsize::new(0);
            let t = Instant::now();
            let per: Vec<Vec<f64>> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..clients)
                    .map(|_| {
                        let (next, qs, db, snap, opts) = (&next, &qs, &db, &snap, &opts);
                        s.spawn(move || {
                            let mut ms = Vec::new();
                            loop {
                                let i = next.fetch_add(1, Ordering::Relaxed);
                                if i >= 2 * m {
                                    break;
                                }
                                let q = &qs[i % m];
                                let t = Instant::now();
                                run(db, snap, &sql(kind, q, i % m), &params(q), opts).expect("query");
                                ms.push(t.elapsed().as_secs_f64() * 1e3);
                            }
                            ms
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let secs = t.elapsed().as_secs_f64();
            let all: Vec<f64> = per.into_iter().flatten().collect();
            eprintln!("   {clients} clients {kind:<13} {:.0} queries/s, p50 {:.2} ms", all.len() as f64 / secs, percentile(&all, 50.0));
            thr.insert(kind.to_string(), json!({"clients": clients, "queries_per_second": all.len() as f64 / secs, "latency": lat(&all)}));
        }
    }
    Ok(json!({
        "db": path.display().to_string(),
        "docs": snap.n_live_docs(),
        "segments": snap.segments().len(),
        "segment_docs": snap.segments().iter().map(|v| v.n_live).collect::<Vec<_>>(),
        "queries": qs.len(),
        "n_slow": n_slow,
        "vector_params": {"ef": opts.vector_params.ef, "rerank": opts.vector_params.rerank},
        "kinds": out,
        "parallel": {"threads": parallel, "kinds": par},
        "throughput": thr,
    }))
}
