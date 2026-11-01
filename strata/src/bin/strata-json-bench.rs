//! `strata-json-bench`: K sparse attribute keys, which only a few rows
//! hold, kept as K declared columns (`A`) or as the keys of one `JSON`
//! field (`J`): the size of the database and of one segment by section,
//! the load, the open, filters, facets, sorts and aggregates on the keys,
//! the `json_paths` listing, and single-row `INSERT`s.
//!
//! Row `i` holds 4 to 11 distinct keys drawn from a Zipf(1.0) popularity
//! over the K keys, so a few keys are in most rows and most keys are in
//! few. Of each 20 keys, 8 hold one of 20 strings, 4 a random 10-digit hex
//! string, 5 an integer below 100,000 and 3 a number with two decimals;
//! a numeric key holds the string `"n/a"` 1% of the time, which a declared
//! column cannot hold and drops. The data is the same for every run (a
//! fixed seed). `J text` (the segment only) is the JSON field with
//! `OPTIONS(text = true)`.
//!
//! ```text
//! strata-json-bench [--rows 1000000] [--keys 1000] [--reps 7] [--inserts 5000] [--out FILE]
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::query::Params;
use strata::segment::build::{build_segment, BuildOptions};
use strata::segment::SegmentFile;
use strata::storage::DbOptions;
use strata::{Document, Field, FieldType, Schema, Value};

#[derive(Parser)]
#[command(about = "Sparse keys as declared columns against a JSON field")]
struct Args {
    #[arg(long, default_value_t = 1_000_000)]
    rows: usize,
    /// Attribute keys.
    #[arg(long, default_value_t = 1000)]
    keys: usize,
    /// Timed repetitions of each query (the median and the fastest are
    /// reported), after one untimed run.
    #[arg(long, default_value_t = 7)]
    reps: usize,
    /// Rows of the segment built alone, for the sizes by section.
    #[arg(long, default_value_t = 100_000)]
    segment_rows: usize,
    /// Single-row INSERTs timed, into a new database.
    #[arg(long, default_value_t = 5000)]
    inserts: usize,
    /// The JSON report; printed to stdout without it.
    #[arg(long)]
    out: Option<PathBuf>,
    /// The directory the databases are built in, and removed from after
    /// (the system's temporary directory by default).
    #[arg(long)]
    tmp: Option<PathBuf>,
}

fn machine() -> Json {
    let sh = |cmd: &str, args: &[&str]| std::process::Command::new(cmd).args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty());
    let cpu = sh("sysctl", &["-n", "machdep.cpu.brand_string"]).or_else(|| {
        std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
    });
    let mem = sh("sysctl", &["-n", "hw.memsize"]).and_then(|s| s.parse::<u64>().ok()).or_else(|| {
        std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| s.lines().find(|l| l.starts_with("MemTotal")).and_then(|l| l.split_whitespace().nth(1)).and_then(|k| k.parse::<u64>().ok()).map(|k| k * 1024))
    });
    let os = sh("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}")).or_else(|| sh("uname", &["-sr"]));
    json!({
        "cpu": cpu,
        "cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "memory_gb": mem.map(|b| (b as f64 / 1e9 * 10.0).round() / 10.0),
        "os": os,
        "arch": std::env::consts::ARCH,
    })
}

// ------------------------------------------------------------------ data

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Few,
    Hex,
    Int,
    Float,
}

fn kind(k: usize) -> Kind {
    match k % 20 {
        0..=7 => Kind::Few,
        8..=11 => Kind::Hex,
        12..=16 => Kind::Int,
        _ => Kind::Float,
    }
}

enum Leaf {
    S(String),
    I(i64),
    F(f64),
}

/// The rows' keys and values, the same sequence for every generator.
struct Gen {
    cdf: Vec<f64>,
    rng: Rng,
}

impl Gen {
    fn new(k: usize) -> Gen {
        let w: Vec<f64> = (0..k).map(|r| 1.0 / (r as f64 + 1.0)).collect();
        let sum: f64 = w.iter().sum();
        let mut acc = 0.0;
        let cdf = w
            .iter()
            .map(|x| {
                acc += x / sum;
                acc
            })
            .collect();
        Gen { cdf, rng: Rng(0x9E37_79B9_7F4A_7C15) }
    }

    fn row(&mut self) -> Vec<(usize, Leaf)> {
        let m = (4 + self.rng.below(8) as usize).min(self.cdf.len());
        let mut keys: Vec<usize> = Vec::with_capacity(m);
        while keys.len() < m {
            let u = self.rng.unit();
            let k = self.cdf.partition_point(|&c| c < u).min(self.cdf.len() - 1);
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        keys.sort_unstable();
        keys.into_iter()
            .map(|k| {
                let numeric = matches!(kind(k), Kind::Int | Kind::Float);
                let leaf = if numeric && self.rng.below(100) == 0 {
                    Leaf::S("n/a".into())
                } else {
                    match kind(k) {
                        Kind::Few => Leaf::S(format!("v{}", self.rng.below(20))),
                        Kind::Hex => Leaf::S(format!("{:010x}", self.rng.next() & 0xff_ffff_ffff)),
                        Kind::Int => Leaf::I(self.rng.below(100_000) as i64),
                        Kind::Float => Leaf::F(self.rng.below(100_000) as f64 / 100.0),
                    }
                };
                (k, leaf)
            })
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Variant {
    Columns,
    Json,
    JsonText,
}

impl Variant {
    fn name(self) -> &'static str {
        match self {
            Variant::Columns => "A",
            Variant::Json => "J",
            Variant::JsonText => "J text",
        }
    }
}

fn schema(v: Variant, k: usize) -> Schema {
    let mut fields = vec![Field::new("id", FieldType::Keyword)];
    match v {
        Variant::Columns => fields.extend((0..k).map(|i| {
            let ty = match kind(i) {
                Kind::Few | Kind::Hex => FieldType::Keyword,
                Kind::Int => FieldType::Int,
                Kind::Float => FieldType::Float,
            };
            Field::new(&format!("k{i}"), ty)
        })),
        Variant::Json => fields.push(Field::new("attrs", FieldType::Json)),
        Variant::JsonText => fields.push(Field::new("attrs", FieldType::Json).json_text()),
    }
    Schema::new("t", fields, Some("id")).unwrap()
}

fn object(row: &[(usize, Leaf)]) -> String {
    let mut obj = serde_json::Map::new();
    for (key, leaf) in row {
        let v = match leaf {
            Leaf::S(s) => json!(s),
            Leaf::I(x) => json!(x),
            Leaf::F(x) => json!(x),
        };
        obj.insert(format!("k{key}"), v);
    }
    Json::Object(obj).to_string()
}

/// Row `i` as variant `v` stores it, and how many of its values a
/// declared column could not hold.
fn doc(v: Variant, k: usize, id: String, row: &[(usize, Leaf)]) -> (Document, usize) {
    match v {
        Variant::Columns => {
            let mut d = vec![Value::Null; k + 1];
            let mut dropped = 0;
            d[0] = Value::Str(id);
            for (key, leaf) in row {
                d[key + 1] = match (kind(*key), leaf) {
                    (Kind::Few | Kind::Hex, Leaf::S(s)) => Value::Str(s.clone()),
                    (Kind::Int, Leaf::I(x)) => Value::Int(*x),
                    (Kind::Float, Leaf::F(x)) => Value::Float(*x),
                    _ => {
                        dropped += 1;
                        Value::Null
                    }
                };
            }
            (d, dropped)
        }
        _ => (vec![Value::Str(id), Value::Str(object(row))], 0),
    }
}

/// Row `i`'s `INSERT` statement.
fn insert(v: Variant, id: &str, row: &[(usize, Leaf)]) -> String {
    match v {
        Variant::Columns => {
            let mut cols = vec!["id".to_string()];
            let mut vals = vec![format!("'{id}'")];
            for (key, leaf) in row {
                let val = match (kind(*key), leaf) {
                    (Kind::Few | Kind::Hex, Leaf::S(s)) => format!("'{s}'"),
                    (Kind::Int, Leaf::I(x)) => x.to_string(),
                    (Kind::Float, Leaf::F(x)) => format!("{x:?}"),
                    _ => continue,
                };
                cols.push(format!("k{key}"));
                vals.push(val);
            }
            format!("INSERT INTO t ({}) VALUES ({})", cols.join(", "), vals.join(", "))
        }
        _ => format!("INSERT INTO t (id, attrs) VALUES ('{id}', '{}')", object(row)),
    }
}

// --------------------------------------------------------------- measures

/// The size of a segment and of its sections, grouped by the first part
/// of their names (`kw`, `num`, `docs`, ...).
fn sections(image: Vec<u8>) -> (usize, BTreeMap<String, usize>) {
    let total = image.len();
    let seg = SegmentFile::from_vec(image).unwrap();
    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    for (name, len) in seg.section_sizes() {
        *by.entry(name.split('.').next().unwrap_or("").to_string()).or_default() += len;
    }
    (total, by)
}

struct Run {
    rows: Vec<Vec<Value>>,
    median_ms: f64,
    best_ms: f64,
    /// The most heap the first run held beyond what was held before it,
    /// in MB.
    peak_mb: f64,
}

/// The system allocator, counting the bytes held and their peak.
struct Counting;

static HELD: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            let now = HELD.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        HELD.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let q = System.realloc(p, l, size);
        if !q.is_null() {
            if size > l.size() {
                let now = HELD.fetch_add(size - l.size(), Ordering::Relaxed) + size - l.size();
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                HELD.fetch_sub(l.size() - size, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn time(db: &Db, sql: &str, reps: usize) -> Result<Run, String> {
    let run = || db.query(sql, &Params::new()).map(|r| r.rows).map_err(|e| e.to_string());
    let before = HELD.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let rows = run()?;
    let peak_mb = PEAK.load(Ordering::Relaxed).saturating_sub(before) as f64 / 1e6;
    let mut ms: Vec<f64> = (0..reps)
        .map(|_| {
            let t = Instant::now();
            let again = run().unwrap();
            assert_eq!(again.len(), rows.len(), "{sql}");
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    Ok(Run { rows, median_ms: ms[ms.len() / 2], best_ms: ms[0], peak_mb })
}

fn round(x: f64, places: i32) -> f64 {
    let p = 10f64.powi(places);
    (x * p).round() / p
}

fn report(r: &Result<Run, String>) -> Json {
    match r {
        Ok(r) => {
            let mut first = r.rows.first().map(|row| format!("{row:?}")).unwrap_or_default();
            if first.len() > 160 {
                first.truncate(first.char_indices().take_while(|(i, _)| *i < 157).last().map(|(i, c)| i + c.len_utf8()).unwrap_or(0));
                first.push_str("...");
            }
            json!({"median_ms": round(r.median_ms, 3), "best_ms": round(r.best_ms, 3), "peak_mb": round(r.peak_mb, 1), "rows": r.rows.len(), "first": first})
        }
        Err(e) => json!({"error": e}),
    }
}

fn remove_db(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let _ = std::fs::remove_file(PathBuf::from(wal));
}

fn main() {
    let a = Args::parse();
    let k = a.keys;
    assert!(k >= 20, "--keys must be at least 20");
    let tmp = a.tmp.clone().unwrap_or_else(std::env::temp_dir).join(format!("strata-json-bench-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();

    // The data: how many rows hold each key, and what the rows are like.
    let mut g = Gen::new(k);
    let mut df = vec![0usize; k];
    let (mut pairs, mut json_bytes, mut conflicts) = (0usize, 0usize, 0usize);
    for _ in 0..a.rows {
        let r = g.row();
        pairs += r.len();
        json_bytes += object(&r).len();
        for (key, leaf) in &r {
            df[*key] += 1;
            conflicts += (matches!(kind(*key), Kind::Int | Kind::Float) && matches!(leaf, Leaf::S(_))) as usize;
        }
    }
    let n = a.rows as f64;
    let mut sorted = df.clone();
    sorted.sort_unstable_by(|x, y| y.cmp(x));
    let share = |rank: usize| round(100.0 * sorted[rank.min(k - 1)] as f64 / n, 3);
    let data = json!({
        "values_per_row": round(pairs as f64 / n, 2),
        "keys_held": df.iter().filter(|&&d| d > 0).count(),
        "keys_in_under_1pct_of_rows": df.iter().filter(|&&d| (d as f64) < 0.01 * n).count(),
        "keys_in_under_0.1pct_of_rows": df.iter().filter(|&&d| (d as f64) < 0.001 * n).count(),
        "pct_of_rows_holding_key_rank": {"1": share(0), "10": share(9), "100": share(99), "1000": share(999)},
        "json_bytes_per_row": round(json_bytes as f64 / n, 1),
        "type_conflicts": conflicts,
    });
    eprintln!("data: {data}");

    // The keys queried: the most held of each kind, one of 20 strings
    // held by about 1% of rows, and one held by about 0.1%.
    let near = |pred: &dyn Fn(usize) -> bool, target: f64| (0..k).filter(|&i| pred(i) && df[i] > 0).min_by(|&x, &y| (df[x] as f64 - target).abs().total_cmp(&(df[y] as f64 - target).abs())).unwrap();
    let few = |i: usize| kind(i) == Kind::Few;
    let hot = (0..k).find(|&i| few(i)).unwrap();
    let mid = near(&few, 0.01 * n);
    let rare = near(&few, 0.001 * n);
    let int = (0..k).find(|&i| kind(i) == Kind::Int).unwrap();
    let int_mid = near(&|i| kind(i) == Kind::Int, 0.01 * n);
    let float = (0..k).find(|&i| kind(i) == Kind::Float).unwrap();
    let hex = (0..k).find(|&i| kind(i) == Kind::Hex).unwrap();
    let keys = json!({
        "hot": {"key": format!("k{hot}"), "rows": df[hot]},
        "1pct": {"key": format!("k{mid}"), "rows": df[mid]},
        "0.1pct": {"key": format!("k{rare}"), "rows": df[rare]},
        "int": {"key": format!("k{int}"), "rows": df[int]},
        "int_1pct": {"key": format!("k{int_mid}"), "rows": df[int_mid]},
        "float": {"key": format!("k{float}"), "rows": df[float]},
        "hex": {"key": format!("k{hex}"), "rows": df[hex]},
    });
    eprintln!("keys: {keys}");

    // One segment built alone, by section.
    let mut segment = serde_json::Map::new();
    for v in [Variant::Columns, Variant::Json, Variant::JsonText] {
        let mut g = Gen::new(k);
        let docs: Vec<Document> = (0..a.segment_rows).map(|i| doc(v, k, format!("r{i}"), &g.row()).0).collect();
        let s = schema(v, k);
        let t = Instant::now();
        let image = build_segment(&s, &docs, &BuildOptions::default()).unwrap();
        let build_s = t.elapsed().as_secs_f64();
        drop(docs);
        let (total, by) = sections(image);
        let rows = a.segment_rows as f64;
        let by: serde_json::Map<String, Json> = by.into_iter().map(|(c, b)| (c, json!({"bytes": b, "bytes_per_row": round(b as f64 / rows, 2)}))).collect();
        eprintln!("[{}] segment of {} rows: {:.2} MB, built in {build_s:.2} s", v.name(), a.segment_rows, total as f64 / 1e6);
        segment.insert(v.name().into(), json!({"bytes": total, "bytes_per_row": round(total as f64 / rows, 2), "build_s": round(build_s, 3), "sections": by}));
    }

    // The query on each variant: `{p}` is the key's column or path.
    let col = |v: Variant, key: usize| if v == Variant::Columns { format!("k{key}") } else { format!("attrs.k{key}") };
    let queries: Vec<(&str, Box<dyn Fn(Variant) -> Option<String>>)> = vec![
        ("eq hot", Box::new(move |v| Some(format!("SELECT COUNT(*) FROM t WHERE {} = 'v3'", col(v, hot))))),
        ("eq 1%", Box::new(move |v| Some(format!("SELECT COUNT(*) FROM t WHERE {} = 'v3'", col(v, mid))))),
        ("eq 0.1%", Box::new(move |v| Some(format!("SELECT COUNT(*) FROM t WHERE {} = 'v3'", col(v, rare))))),
        ("in hot", Box::new(move |v| Some(format!("SELECT COUNT(*) FROM t WHERE {} IN ('v1', 'v2', 'v3', 'v4')", col(v, hot))))),
        ("prefix hex", Box::new(move |v| Some(format!("SELECT COUNT(*) FROM t WHERE {} LIKE 'ab%'", col(v, hex))))),
        ("range int", Box::new(move |v| Some(format!("SELECT COUNT(*) FROM t WHERE {} BETWEEN 10000 AND 19999", col(v, int))))),
        ("range float", Box::new(move |v| Some(format!("SELECT COUNT(*) FROM t WHERE {} > 900.5", col(v, float))))),
        ("exists 1%", Box::new(move |v| Some(format!("SELECT COUNT(*) FROM t WHERE {} IS NOT NULL", col(v, mid))))),
        ("exists 0.1%", Box::new(move |v| Some(format!("SELECT COUNT(*) FROM t WHERE {} IS NOT NULL", col(v, rare))))),
        ("and", Box::new(move |v| Some(format!("SELECT COUNT(*) FROM t WHERE {} = 'v3' AND {} < 50000", col(v, hot), col(v, int))))),
        ("ids limit 10", Box::new(move |v| Some(format!("SELECT id FROM t WHERE {} = 'v3' LIMIT 10", col(v, mid))))),
        ("rows limit 100", Box::new(move |v| Some(format!("SELECT * FROM t WHERE {} = 'v3' LIMIT 100", col(v, mid))))),
        ("group hot", Box::new(move |v| Some(format!("SELECT {p}, COUNT(*) FROM t WHERE {p} IS NOT NULL GROUP BY {p} ORDER BY COUNT(*) DESC, {p} LIMIT 5", p = col(v, hot))))),
        ("group 1%", Box::new(move |v| Some(format!("SELECT {p}, COUNT(*) FROM t WHERE {p} IS NOT NULL GROUP BY {p} ORDER BY COUNT(*) DESC, {p} LIMIT 5", p = col(v, mid))))),
        ("group hot of 1%", Box::new(move |v| Some(format!("SELECT {p}, COUNT(*) FROM t WHERE {} = 'v3' GROUP BY {p} ORDER BY COUNT(*) DESC, {p} LIMIT 5", col(v, mid), p = col(v, hot))))),
        ("avg int", Box::new(move |v| Some(format!("SELECT AVG({p}), MIN({p}), MAX({p}) FROM t WHERE {p} IS NOT NULL", p = col(v, int))))),
        ("avg int 1%", Box::new(move |v| Some(format!("SELECT AVG({p}), MIN({p}), MAX({p}) FROM t WHERE {p} IS NOT NULL", p = col(v, int_mid))))),
        ("order int limit 10", Box::new(move |v| Some(format!("SELECT id, {p} FROM t WHERE {p} IS NOT NULL ORDER BY {p}, id LIMIT 10", p = col(v, int))))),
        ("order int of eq hot", Box::new(move |v| Some(format!("SELECT id, {p} FROM t WHERE {} = 'v3' ORDER BY {p}, id LIMIT 10", col(v, hot), p = col(v, int))))),
        ("paths", Box::new(|v| (v != Variant::Columns).then(|| "SELECT * FROM json_paths('t', 'attrs')".to_string()))),
        ("paths of eq 1%", Box::new(move |v| (v != Variant::Columns).then(|| format!("SELECT * FROM json_paths('t', 'attrs', filter => 'attrs.k{mid} = ''v3''')")))),
    ];

    let mut db_report = serde_json::Map::new();
    let mut results: BTreeMap<&str, Vec<(Option<String>, Option<Result<Run, String>>)>> = BTreeMap::new();
    let mut inserts = serde_json::Map::new();
    for v in [Variant::Columns, Variant::Json] {
        let path = tmp.join(format!("{}.strata", v.name()));
        remove_db(&path);
        let opts = DbOptions { sync: false, ..DbOptions::default() };
        let t = Instant::now();
        let mut dropped = 0usize;
        {
            let db = Db::create(&path, schema(v, k), opts.clone()).unwrap();
            let mut g = Gen::new(k);
            let mut i = 0usize;
            while i < a.rows {
                let end = (i + 100_000).min(a.rows);
                let docs: Vec<Document> = (i..end)
                    .map(|j| {
                        let (d, x) = doc(v, k, format!("r{j}"), &g.row());
                        dropped += x;
                        d
                    })
                    .collect();
                db.load(docs).unwrap();
                i = end;
            }
            db.storage().flush().unwrap();
        }
        let load_s = t.elapsed().as_secs_f64();
        let bytes = std::fs::metadata(&path).unwrap().len();
        let t = Instant::now();
        let db = Db::open(&path, opts).unwrap();
        let open_ms = t.elapsed().as_secs_f64() * 1e3;
        let segments = db.snapshot().segments().len();
        eprintln!("[{}] {} rows: {:.1} MB in {segments} segments, loaded in {load_s:.1} s, opened in {open_ms:.1} ms", v.name(), a.rows, bytes as f64 / 1e6);
        db_report.insert(
            v.name().into(),
            json!({
                "bytes": bytes,
                "bytes_per_row": round(bytes as f64 / n, 1),
                "segments": segments,
                "load_s": round(load_s, 2),
                "rows_per_s": (n / load_s).round(),
                "open_ms": round(open_ms, 2),
                "values_dropped": dropped,
            }),
        );
        for (name, sql) in &queries {
            let sql = sql(v);
            let run = sql.as_ref().map(|s| time(&db, s, a.reps));
            if let Some(r) = &run {
                match r {
                    Ok(r) => eprintln!("  [{}] {name:22} {:9.3} ms  {} rows  peak +{:.1} MB", v.name(), r.median_ms, r.rows.len(), r.peak_mb),
                    Err(e) => eprintln!("  [{}] {name:22} ERROR {e}", v.name()),
                }
            }
            results.entry(name).or_default().push((sql, run));
        }
        drop(db);
        remove_db(&path);

        // Single-row INSERTs into a new database.
        let wpath = tmp.join(format!("{}-inserts.strata", v.name()));
        remove_db(&wpath);
        let db = Db::create(&wpath, schema(v, k), DbOptions { sync: false, ..DbOptions::default() }).unwrap();
        let mut g = Gen::new(k);
        let stmts: Vec<String> = (0..a.inserts).map(|i| insert(v, &format!("w{i}"), &g.row())).collect();
        let t = Instant::now();
        for s in &stmts {
            db.sql(s, &Params::new()).unwrap();
        }
        let secs = t.elapsed().as_secs_f64();
        let mut wal = wpath.as_os_str().to_owned();
        wal.push("-wal");
        let wal_bytes = std::fs::metadata(PathBuf::from(wal)).map(|m| m.len()).unwrap_or(0);
        eprintln!("  [{}] {} single-row INSERTs: {:.0} rows/s", v.name(), a.inserts, a.inserts as f64 / secs);
        inserts.insert(v.name().into(), json!({"rows": a.inserts, "rows_per_s": (a.inserts as f64 / secs).round(), "wal_bytes_per_row": round(wal_bytes as f64 / a.inserts as f64, 1)}));
        drop(db);
        remove_db(&wpath);
    }
    let _ = std::fs::remove_dir(&tmp);

    let queries: Vec<Json> = queries
        .iter()
        .map(|(name, _)| {
            let runs = &results[name];
            let rows = |i: usize| runs[i].1.as_ref().and_then(|r| r.as_ref().ok()).map(|r| format!("{:?}", r.rows));
            let mut q = serde_json::Map::new();
            q.insert("name".into(), json!(name));
            for (i, v) in [Variant::Columns, Variant::Json].into_iter().enumerate() {
                q.insert(format!("sql_{}", v.name()), json!(runs[i].0));
                q.insert(v.name().into(), runs[i].1.as_ref().map(report).unwrap_or(Json::Null));
            }
            if let (Some(x), Some(y)) = (rows(0), rows(1)) {
                q.insert("same_rows".into(), json!(x == y));
            }
            Json::Object(q)
        })
        .collect();
    let report = json!({
        "bench": "strata-json-bench",
        "machine": machine(),
        "rows": a.rows,
        "keys": k,
        "reps": a.reps,
        "data": data,
        "keys_queried": keys,
        "segment": {"rows": a.segment_rows, "variants": segment},
        "database": db_report,
        "queries": queries,
        "inserts": inserts,
    });
    let text = serde_json::to_string_pretty(&report).unwrap();
    match &a.out {
        Some(p) => std::fs::write(p, text + "\n").unwrap(),
        None => println!("{text}"),
    }
}
