//! `strata-forward-bench`: sizes and latencies of the forward index
//! (columns, range and keyword postings, facets, doc store) on a BEIR
//! collection. Writes `<out>/<collection>.json` and prints a summary.
//!
//! Documents are read from `docs.jsonl` without their vectors (the
//! forward index never sees them, and Quora's vectors alone are 2 GB). The
//! table is the BEIR table of `strata::data` plus two synthetic fields:
//! `seq`, an INT correlated with docID order (like an insertion timestamp),
//! on which zone maps can prune, and `tier`, a KEYWORD with four values
//! (40/30/20/10%), the few-values case of facet counting.
//!
//! ```text
//! strata-forward-bench --collection trec-covid [--limit N] [--reps 7]
//!     [--only sizes,range,probe,keyword,facet,store] [--tmp DIR]
//! ```

use std::fs::File;
use std::hint::black_box;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use posting_bench::bytes::OwnedBytes;
use rand::{rngs::StdRng, Rng, SeedableRng};
use serde_json::{json, Value as Json};
use strata::forward::{self, count_range, eval_range, DocStore, FacetStrategy, KeywordColumn, NumericColumn, RangeIndex, RangeStrategy};
use strata::segment::build::BuildOptions;
use strata::segment::{DocFilter, DocSet, SegmentFile, SegmentWriter};
use strata::{data, iostat, Document, Field, FieldType, Schema, Value, VectorIndexKind};

#[derive(Parser)]
#[command(about = "Forward index sizes and latencies on a BEIR collection")]
struct Args {
    #[arg(long, default_value = "trec-covid")]
    collection: String,
    /// Keep the first N documents.
    #[arg(long)]
    limit: Option<usize>,
    /// Timed repetitions per measurement (median and min reported).
    #[arg(long, default_value_t = 7)]
    reps: usize,
    /// Experiments to run.
    #[arg(long, value_delimiter = ',', default_value = "sizes,range,probe,keyword,facet,store")]
    only: Vec<String>,
    /// Output directory for the JSON results.
    #[arg(long, default_value = "results/forward")]
    out: PathBuf,
    /// Directory for the mapped segment file of the cold-cache runs
    /// (deleted afterwards); defaults to the output directory.
    #[arg(long)]
    tmp: Option<PathBuf>,
    /// Build options, `key=value` (e.g. `forward.range_buckets=128`).
    #[arg(long = "opt", value_parser = parse_kv)]
    opts: Vec<(String, String)>,
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or_else(|| format!("expected key=value, got {s}"))
}

/// Field indices of the bench table.
mod f {
    pub use strata::data::col::*;
    pub const SEQ: usize = 9;
}

fn schema() -> Schema {
    let mut s = data::beir_schema(1024, VectorIndexKind::Flat);
    s.fields.push(Field::new("seq", FieldType::Int).not_stored());
    s.fields.push(Field::new("tier", FieldType::Keyword).not_stored());
    s
}

/// The documents of `docs.jsonl` in the bench table (no body, no vector).
fn load_docs(name: &str, limit: Option<usize>, rng: &mut StdRng) -> Vec<Document> {
    let path = data::data_dir().join(name).join("docs.jsonl");
    let reader = BufReader::new(File::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())));
    let mut docs = Vec::new();
    for line in reader.lines().take(limit.unwrap_or(usize::MAX)) {
        let j: Json = serde_json::from_str(&line.unwrap()).unwrap();
        let s = |k: &str| Value::Str(j[k].as_str().unwrap().to_string());
        let d = docs.len() as i64;
        docs.push(vec![
            s("id"),
            s("title"),
            s("text"),
            Value::Null,
            Value::Int(j["year"].as_i64().unwrap()),
            s("category"),
            Value::Float(j["rating"].as_f64().unwrap()),
            s("topic"),
            Value::Null,
            // Increasing with docID, with local disorder over ~100 docs.
            Value::Int(1_600_000_000 + 10 * d + rng.gen_range(0..1000)),
            Value::Str(["a", "b", "c", "d"][match rng.gen::<f64>() {
                x if x < 0.4 => 0,
                x if x < 0.7 => 1,
                x if x < 0.9 => 2,
                _ => 3,
            }]
            .to_string()),
        ]);
    }
    docs
}

/// Builds the forward sections only; returns the image and the build time
/// per section family.
fn build(schema: &Schema, docs: &[Document], opts: &BuildOptions) -> (Vec<u8>, Json) {
    let mut out = SegmentWriter::new(docs.len());
    let mut times = serde_json::Map::new();
    for (i, fld) in schema.fields.iter().enumerate() {
        if fld.indexed && matches!(fld.ty, FieldType::Keyword | FieldType::Int | FieldType::Float) {
            let t = Instant::now();
            let mut b = forward::field_builder(fld, opts).unwrap();
            for (d, doc) in docs.iter().enumerate() {
                b.add(d as u32, &doc[i]).unwrap();
            }
            b.finish(&mut out).unwrap();
            times.insert(fld.name.clone(), json!(t.elapsed().as_secs_f64()));
        }
    }
    let t = Instant::now();
    forward::build_doc_store(schema, docs, opts, &mut out).unwrap();
    times.insert("store".into(), json!(t.elapsed().as_secs_f64()));
    (out.finish(), Json::Object(times))
}

/// Median and minimum of `reps` timings of batches of calls, in ns per
/// call; a batch lasts about 10 ms (sized from a first call).
fn time<R>(reps: usize, mut f: impl FnMut() -> R) -> (f64, f64) {
    let t = Instant::now();
    black_box(f());
    let once = t.elapsed().as_nanos() as f64;
    let iters = ((1e7 / once.max(20.0)) as usize).clamp(1, 100_000);
    time_n(reps, iters, f)
}

/// Median and minimum of `reps` timings of `iters` calls, in ns per call.
fn time_n<R>(reps: usize, iters: usize, mut f: impl FnMut() -> R) -> (f64, f64) {
    black_box(f());
    let mut v: Vec<f64> = (0..reps.max(1))
        .map(|_| {
            let t = Instant::now();
            for _ in 0..iters {
                black_box(f());
            }
            t.elapsed().as_nanos() as f64 / iters as f64
        })
        .collect();
    v.sort_by(f64::total_cmp);
    (v[v.len() / 2], v[0])
}

fn stat((median, min): (f64, f64)) -> Json {
    json!({"median_ns": median, "min_ns": min})
}

fn main() {
    let args = Args::parse();
    let mut rng = StdRng::seed_from_u64(args.seed);
    let schema = schema();
    let t = Instant::now();
    let docs = load_docs(&args.collection, args.limit, &mut rng);
    let n = docs.len();
    eprintln!("{}: {n} documents loaded in {:.1}s", args.collection, t.elapsed().as_secs_f64());
    let mut opts = BuildOptions::default();
    for (k, v) in &args.opts {
        opts = opts.set(k, v);
    }
    let t = Instant::now();
    let (image, build_secs) = build(&schema, &docs, &opts);
    eprintln!("built {} MB in {:.1}s", image.len() >> 20, t.elapsed().as_secs_f64());
    let seg = SegmentFile::from_vec(image).unwrap();
    let mut report = serde_json::Map::new();
    report.insert("collection".into(), json!(args.collection));
    report.insert("documents".into(), json!(n));
    report.insert("reps".into(), json!(args.reps));
    report.insert("build_options".into(), json!(opts.params));
    report.insert("build_secs".into(), build_secs);
    let run = |name: &str| args.only.iter().any(|o| o == name);

    if run("sizes") {
        report.insert("sizes".into(), sizes(&seg, &schema));
    }
    if run("range") {
        report.insert("range".into(), ranges(&seg, &docs, args.reps, &mut rng));
    }
    if run("probe") {
        report.insert("probe".into(), probes(&seg, args.reps, &mut rng));
    }
    if run("keyword") {
        report.insert("keyword".into(), keywords(&seg, args.reps));
    }
    if run("facet") {
        report.insert("facet".into(), facets(&seg, args.reps, &mut rng));
    }
    drop(docs);
    if run("store") {
        let tmp = args.tmp.clone().unwrap_or_else(|| args.out.clone());
        report.insert("store".into(), store(&seg, &schema, &tmp, args.reps, &mut rng));
    }
    let rss = peak_rss_mb();
    eprintln!("peak RSS {rss:.0} MB");
    report.insert("peak_rss_mb".into(), json!(rss));
    std::fs::create_dir_all(&args.out).unwrap();
    let path = args.out.join(format!("{}.json", args.collection));
    let mut f = File::create(&path).unwrap();
    f.write_all(serde_json::to_string_pretty(&Json::Object(report)).unwrap().as_bytes()).unwrap();
    eprintln!("wrote {}", path.display());
}

/// Peak resident set size of this process (Linux `VmHWM`).
fn peak_rss_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
        .map_or(0.0, |kb| kb / 1024.0)
}

// ----------------------------------------------------------------- sizes

fn sizes(seg: &SegmentFile, schema: &Schema) -> Json {
    let n = seg.n_docs() as f64;
    let bits = |b: usize| b as f64 * 8.0 / n;
    let sections: serde_json::Map<String, Json> =
        seg.section_sizes().into_iter().map(|(name, b)| (name, json!({"bytes": b, "bits_per_doc": bits(b)}))).collect();
    let mut cols = serde_json::Map::new();
    for name in ["year", "rating", "seq"] {
        let c = NumericColumn::open(seg, name).unwrap();
        let [zone, offs, data, presence, hist] = c.byte_sizes();
        let range = RangeIndex::open_if_present(seg, name).unwrap().map_or(0, |r| r.byte_len());
        cols.insert(
            name.into(),
            json!({
                "kind": format!("{:?}", c.kind()),
                "values_bits_per_doc": bits(data),
                "zone_maps_bits_per_doc": bits(zone),
                "block_offsets_bits_per_doc": bits(offs),
                "presence_bits_per_doc": bits(presence),
                "column_bits_per_doc": bits(zone + offs + data + presence),
                "histogram_bytes": hist,
                "histogram_buckets": c.histogram().len(),
                "range_lists_bits_per_doc": bits(range),
            }),
        );
        eprintln!(
            "  {name:8} {:?}: column {:.2} bits/doc (values {:.2}, zone maps {:.2}), range lists {:.2} bits/doc",
            c.kind(),
            bits(zone + offs + data + presence),
            bits(data),
            bits(zone),
            bits(range)
        );
    }
    for name in ["category", "topic", "tier", "id"] {
        let c = KeywordColumn::open(seg, name).unwrap();
        let s = c.byte_sizes();
        let codes: usize = s.codes.iter().sum();
        cols.insert(
            name.into(),
            json!({
                "values": c.n_values(),
                "dictionary_bytes": s.dict,
                "codes_bits_per_doc": bits(codes),
                "code_values_bits_per_doc": bits(s.codes[2]),
                "lists_bits_per_doc": bits(s.lists),
                "rare_bits_per_doc": bits(s.rare),
            }),
        );
        eprintln!(
            "  {name:8} {} values: dict {} B, codes {:.2} bits/doc, lists {:.2} + rare {:.2} bits/doc",
            c.n_values(),
            s.dict,
            bits(codes),
            bits(s.lists),
            bits(s.rare)
        );
    }
    let store = DocStore::open(seg, schema).unwrap();
    let (compressed, raw) = store.byte_sizes();
    eprintln!("  store: {:.1} MB raw, {:.1} MB compressed ({:.2}x), {} blocks", raw as f64 / 1e6, compressed as f64 / 1e6, raw as f64 / compressed as f64, store.n_blocks());
    json!({
        "sections": sections,
        "fields": cols,
        "store": {"raw_bytes": raw, "compressed_bytes": compressed, "ratio": raw as f64 / compressed as f64, "blocks": store.n_blocks()},
        "segment_bytes": seg.image().len(),
    })
}

// ----------------------------------------------------------------- range

/// Bounds `[lo, hi]` whose selectivity is close to `s`: a random window of
/// the sorted values.
fn window(sorted: &[f64], s: f64, rng: &mut StdRng) -> (f64, f64) {
    let n = sorted.len();
    let w = ((s * n as f64) as usize).clamp(1, n);
    let start = rng.gen_range(0..=n - w);
    (sorted[start], sorted[start + w - 1])
}

fn ranges(seg: &SegmentFile, docs: &[Document], reps: usize, rng: &mut StdRng) -> Json {
    let mut out = Vec::new();
    for (name, fi) in [("year", f::YEAR), ("rating", f::RATING), ("seq", f::SEQ)] {
        let col = NumericColumn::open(seg, name).unwrap();
        let idx = RangeIndex::open(seg, name).unwrap();
        let mut sorted: Vec<f64> = docs.iter().filter_map(|d| d[fi].as_f64()).collect();
        sorted.sort_by(f64::total_cmp);
        for s in [0.001, 0.01, 0.1, 0.5] {
            for w in 0..5 {
                let (lo, hi) = window(&sorted, s, rng);
                let actual = col.scan_count(lo, hi);
                let plan = idx.plan(lo, hi).unwrap();
                let mut row = json!({
                    "field": name, "target": s, "window": w, "lo": lo, "hi": hi,
                    "selectivity": actual as f64 / col.len() as f64,
                    "estimate": col.estimate(lo, hi),
                    "covered_docs": plan.covered_docs, "boundary_docs": plan.boundary_docs,
                    "blocks_decoded": col.blocks_to_decode(lo, hi), "blocks": col.len().div_ceil(forward::BLOCK),
                    "auto_choice": format!("{:?}", forward::choose_range_strategy(&col, Some(&idx), lo, hi, false)),
                    "auto_count_choice": format!("{:?}", forward::choose_range_strategy(&col, Some(&idx), lo, hi, true)),
                    "model_docset_ns": forward::range_costs(&col, Some(&idx), lo, hi, false),
                    "model_count_ns": forward::range_costs(&col, Some(&idx), lo, hi, true),
                    "scan_fractions_estimate": col.key_range(lo, hi).map(|r| col.scan_fractions(r)),
                });
                for (label, st) in [("scan", RangeStrategy::Scan), ("postings", RangeStrategy::Postings), ("auto", RangeStrategy::Auto)] {
                    row[format!("{label}_docset")] = stat(time(reps, || eval_range(&col, Some(&idx), lo, hi, st).len()));
                    row[format!("{label}_count")] = stat(time(reps, || count_range(&col, Some(&idx), lo, hi, st)));
                }
                if w == 0 {
                    let g = |k: &str| row[k]["median_ns"].as_f64().unwrap() / 1e3;
                    eprintln!(
                        "  {name:6} {:6.2}%: docset scan {:8.1} us, postings {:8.1} us (auto {:8.1}); count scan {:8.1}, postings {:8.1} us",
                        100.0 * actual as f64 / col.len() as f64,
                        g("scan_docset"),
                        g("postings_docset"),
                        g("auto_docset"),
                        g("scan_count"),
                        g("postings_count")
                    );
                }
                out.push(row);
            }
        }
    }
    Json::Array(out)
}

// ----------------------------------------------------------------- probe

fn probes(seg: &SegmentFile, reps: usize, rng: &mut StdRng) -> Json {
    let n = seg.n_docs() as u32;
    let year = NumericColumn::open(seg, "year").unwrap();
    let rating = NumericColumn::open(seg, "rating").unwrap();
    let cat = KeywordColumn::open(seg, "category").unwrap();
    let topic = KeywordColumn::open(seg, "topic").unwrap();
    let tier = KeywordColumn::open(seg, "tier").unwrap();
    // Candidate streams in docID order (as from a posting list) and in
    // random order (as from a vector index).
    let m = 100_000.min(n as usize);
    let mut sorted: Vec<u32> = (0..m).map(|_| rng.gen_range(0..n)).collect();
    sorted.sort_unstable();
    let shuffled: Vec<u32> = (0..m).map(|_| rng.gen_range(0..n)).collect();
    let frequent = cat.value(most_frequent(&cat, 0)).to_string();
    let mid = cat.value(most_frequent(&cat, 15)).to_string();
    let filters: Vec<(String, Box<dyn DocFilter>)> = vec![
        ("column year in [2000, 2009]".into(), Box::new(year.filter(2000.0, 2009.0))),
        ("column rating in [1, 2]".into(), Box::new(rating.filter(1.0, 2.0))),
        (format!("pyramid category = {frequent}"), Box::new(cat.filter(&frequent))),
        (format!("pyramid category = {mid}"), Box::new(cat.filter(&mid))),
        ("pyramid topic = t00".into(), Box::new(topic.filter("t00"))),
        ("pyramid tier = a (Flat chunks)".into(), Box::new(tier.filter("a"))),
        ("codes tier = a".into(), Box::new(tier.filter_in(&["a"]))),
        ("codes category in 4 values".into(), Box::new(cat.filter_in(&top_values(&cat, 4).iter().map(|s| s.as_str()).collect::<Vec<_>>()))),
        ("docset bits year in [2000, 2009]".into(), Box::new(year.scan(2000.0, 2009.0))),
        (format!("docset sorted category = {mid}"), Box::new(cat.docs(&mid))),
    ];
    let mut out = Vec::new();
    for (name, flt) in &filters {
        let card = flt.cardinality();
        for (order, cands) in [("sorted", &sorted), ("random", &shuffled)] {
            let (med, min) = time_n(reps, 3, || cands.iter().filter(|&&d| flt.contains(d)).count());
            let per = (med / cands.len() as f64, min / cands.len() as f64);
            eprintln!("  probe {name:40} {order:6}: {:6.2} ns/candidate", per.0);
            out.push(json!({"filter": name, "cardinality": card, "order": order, "candidates": cands.len(), "ns_per_candidate": stat(per)}));
        }
    }
    Json::Array(out)
}

/// The code of the `rank`-th most frequent value.
fn most_frequent(col: &KeywordColumn, rank: usize) -> u32 {
    let mut codes: Vec<u32> = (0..col.n_values() as u32).collect();
    codes.sort_by_key(|&c| std::cmp::Reverse(col.count_code(c)));
    codes[rank.min(codes.len() - 1)]
}

fn top_values(col: &KeywordColumn, k: usize) -> Vec<String> {
    (0..k).map(|r| col.value(most_frequent(col, r)).to_string()).collect()
}

// --------------------------------------------------------------- keyword

fn keywords(seg: &SegmentFile, reps: usize) -> Json {
    let mut out = Vec::new();
    for name in ["category", "topic", "tier"] {
        let col = KeywordColumn::open(seg, name).unwrap();
        let nv = col.n_values();
        for rank in [0, nv / 4, nv / 2, nv - 1] {
            let v = col.value(most_frequent(&col, rank)).to_string();
            let docs = time(reps, || col.docs(&v).len());
            let filter = time(reps, || col.filter(&v).cardinality());
            eprintln!("  {name} = {v:4} ({:7} docs): docs {:8.1} us, filter {:6.2} us", col.count(&v), docs.0 / 1e3, filter.0 / 1e3);
            out.push(json!({"field": name, "op": "eq", "value": v, "docs": col.count(&v), "docset": stat(docs), "filter_open": stat(filter)}));
        }
        for k in [2, 4, 8, 16].into_iter().filter(|&k| k <= nv) {
            let vals = top_values(&col, k.min(nv));
            let refs: Vec<&str> = vals.iter().map(|s| s.as_str()).collect();
            let total: usize = refs.iter().map(|v| col.count(v)).sum();
            let docs = time(reps, || col.docs_in(&refs).len());
            eprintln!("  {name} IN {k:2} most frequent ({total:7} docs): {:8.1} us", docs.0 / 1e3);
            out.push(json!({"field": name, "op": "in", "values": k, "docs": total, "docset": stat(docs)}));
            // The least frequent values: a sparse union.
            let vals: Vec<String> = (0..k.min(nv)).map(|r| col.value(most_frequent(&col, nv - 1 - r)).to_string()).collect();
            let refs: Vec<&str> = vals.iter().map(|s| s.as_str()).collect();
            let total: usize = refs.iter().map(|v| col.count(v)).sum();
            let docs = time(reps, || col.docs_in(&refs).len());
            eprintln!("  {name} IN {k:2} least frequent ({total:7} docs): {:8.1} us", docs.0 / 1e3);
            out.push(json!({"field": name, "op": "in_rare", "values": k, "docs": total, "docset": stat(docs)}));
        }
    }
    Json::Array(out)
}

// ----------------------------------------------------------------- facet

fn facets(seg: &SegmentFile, reps: usize, rng: &mut StdRng) -> Json {
    let n = seg.n_docs() as u32;
    let mut out = Vec::new();
    for name in ["category", "topic", "tier"] {
        let col = KeywordColumn::open(seg, name).unwrap();
        for density in [1e-4, 1e-3, 0.005, 0.01, 0.02, 0.03, 0.05, 0.1, 0.2, 0.4, 0.7, 1.0] {
            let docs = if density >= 1.0 { DocSet::all(n) } else { forward_docset(rng, n, density) };
            let codes = time(reps, || col.facet_code_counts(&docs, FacetStrategy::Codes).len());
            let post = time(reps, || col.facet_code_counts(&docs, FacetStrategy::Postings).len());
            let auto = time(reps, || col.facet_code_counts(&docs, FacetStrategy::Auto).len());
            let choice = col.choose_facet_strategy(&docs);
            eprintln!(
                "  facet {name:8} {:8} docs ({}): codes {:8.1} us, postings {:8.1} us, auto ({choice:?}) {:8.1} us",
                docs.len(),
                if matches!(docs, DocSet::Bits { .. }) { "bits" } else { "sorted" },
                codes.0 / 1e3,
                post.0 / 1e3,
                auto.0 / 1e3
            );
            out.push(json!({
                "field": name, "values": col.n_values(), "density": density, "docs": docs.len(),
                "representation": if matches!(docs, DocSet::Bits { .. }) { "bits" } else { "sorted" },
                "codes": stat(codes), "postings": stat(post), "auto": stat(auto), "auto_choice": format!("{choice:?}"),
            }));
        }
    }
    Json::Array(out)
}

fn forward_docset(rng: &mut StdRng, n: u32, density: f64) -> DocSet {
    DocSet::from_sorted((0..n).filter(|_| rng.gen_bool(density)).collect(), n)
}

// ----------------------------------------------------------------- store

/// Fetch latency of 10 random documents: warm (in memory) and cold (the
/// segment written to a file and mapped; its pages evicted and unmapped
/// before every query, pages read counted with `mincore`), with the
/// kernel's default read-ahead and with `MADV_RANDOM`.
fn store(seg: &SegmentFile, schema: &Schema, tmp: &Path, reps: usize, rng: &mut StdRng) -> Json {
    let n = seg.n_docs() as u32;
    let queries: Vec<Vec<u32>> = (0..50).map(|_| (0..10).map(|_| rng.gen_range(0..n)).collect()).collect();
    let fields = [f::ID, f::TITLE];
    let mem = DocStore::open(seg, schema).unwrap();
    let warm = time_n(reps, 1, || queries.iter().map(|q| q.iter().map(|&d| mem.get(d).len()).sum::<usize>()).sum::<usize>());
    let warm_proj = time_n(reps, 1, || queries.iter().map(|q| q.iter().map(|&d| mem.get_fields(d, &fields).len()).sum::<usize>()).sum::<usize>());
    let per_query = |t: (f64, f64)| (t.0 / queries.len() as f64, t.1 / queries.len() as f64);
    eprintln!("  store warm: {:.1} us per 10 documents ({:.1} us for id, title)", per_query(warm).0 / 1e3, per_query(warm_proj).0 / 1e3);

    std::fs::create_dir_all(tmp).unwrap();
    let path = tmp.join(format!("forward-bench-{}.seg", std::process::id()));
    std::fs::write(&path, seg.image().as_slice()).unwrap();
    let mut cold = serde_json::Map::new();
    for random in [false, true] {
        let file = File::open(&path).unwrap();
        // SAFETY: the file is private to this run and not modified.
        let map = unsafe { memmap2::Mmap::map(&file).unwrap() };
        let (ptr, len) = (map.as_ptr(), map.len());
        if random {
            // SAFETY: advice on our own mapping.
            unsafe { libc::madvise(ptr as *mut libc::c_void, len, libc::MADV_RANDOM) };
        }
        let mapped = SegmentFile::open(OwnedBytes::new(map)).unwrap();
        let store = DocStore::open(&mapped, schema).unwrap();
        let mut lat = Vec::new();
        let mut pages = Vec::new();
        for q in &queries {
            // Unmap first: pages mapped by this process cannot be evicted.
            // SAFETY: a read-only shared mapping of an unchanged file.
            unsafe { iostat::drop_mapping(ptr, len).unwrap() };
            iostat::evict(&path).unwrap();
            let before = iostat::resident_pages(&path).unwrap();
            let t = Instant::now();
            black_box(q.iter().map(|&d| store.get(d).len()).sum::<usize>());
            lat.push(t.elapsed().as_nanos() as f64);
            pages.push(iostat::resident_pages(&path).unwrap().saturating_sub(before));
        }
        lat.sort_by(f64::total_cmp);
        let mean_pages = pages.iter().sum::<usize>() as f64 / pages.len() as f64;
        let key = if random { "cold_madv_random" } else { "cold" };
        eprintln!("  store {key}: {:.1} us per 10 documents (median), {:.1} pages read", lat[lat.len() / 2] / 1e3, mean_pages);
        cold.insert(key.into(), json!({"median_ns": lat[lat.len() / 2], "min_ns": lat[0], "mean_pages": mean_pages}));
    }
    std::fs::remove_file(&path).unwrap();
    let (compressed, raw) = mem.byte_sizes();
    json!({
        "queries": queries.len(), "docs_per_query": 10,
        "blocks": mem.n_blocks(), "raw_bytes": raw, "compressed_bytes": compressed,
        "warm": stat(per_query(warm)), "warm_id_title": stat(per_query(warm_proj)),
        "cold": cold,
    })
}
