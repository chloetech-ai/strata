//! Full-text index experiments: build time and section sizes of both
//! postings formats, BM25 top-k latency by algorithm and filter, boolean AND
//! / OR latency, cold-cache latency and pages read, nDCG@10, and the
//! measurements behind the design constants (Loom threshold, filter
//! strategy crossover, norm quantisation, impact tightness).
//!
//! ```sh
//! cargo run --release --bin strata-text-bench -- --collections scifact,fiqa \
//!     --max-queries 2000 --reps 3 --cold-queries 100 --sweep-min-df 128,512,1024,4096
//! ```
//!
//! Writes `<out>/<collection>.json` (default `results/text/`). Only the text
//! of each collection is loaded (no vectors), so Quora needs about 1.5 GB.

use std::collections::HashMap;
use std::fs::File;
use std::hint::black_box;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use memmap2::{Advice, Mmap};
use posting_bench::bytes::OwnedBytes;
use rand::{rngs::StdRng, seq::index, Rng, SeedableRng};
use serde::Deserialize;
use serde_json::{json, Value};
use strata::iostat;
use strata::metrics::{mean, ndcg_at_k, percentile};
use strata::segment::{DocId, DocSet, SegmentFile, SegmentWriter};
use strata::text::analyzer::{for_each_word, is_stopword};
use strata::text::codec::NORM_LENGTHS;
use strata::text::bm25::drive_cost;
use strata::text::postings::{LIST_IMPACT, LOOM_MIN_DF};
use strata::text::reader::PROBE_COST;
use strata::text::{
    analyze, Bm25Params, BoolQuery, CollectionStats, FilterStrategy, PostingsFormat, TextFieldBuilder, TextFieldReader, TopKAlgo, END,
};

#[derive(Parser)]
#[command(about = "Full-text index experiments")]
struct Args {
    /// BEIR collections under $STRATA_DATA (default /home/user/data/beir).
    #[arg(long, value_delimiter = ',', default_value = "scifact,fiqa,trec-covid,quora")]
    collections: Vec<String>,
    /// Keep only the first N documents of each collection.
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, value_delimiter = ',', default_value = "loom,bp128")]
    formats: Vec<String>,
    /// Shortest list stored as Loom in the Loom format.
    #[arg(long, default_value_t = LOOM_MIN_DF)]
    loom_min_df: u32,
    /// Queries timed per collection (a random sample; nDCG uses all).
    #[arg(long, default_value_t = 2000)]
    max_queries: usize,
    /// Timed runs per query after one warm-up; the median is kept.
    #[arg(long, default_value_t = 3)]
    reps: usize,
    #[arg(long, default_value_t = 10)]
    k: usize,
    /// Random-filter selectivities for BM25 latency.
    #[arg(long, value_delimiter = ',', default_value = "0.01,0.1,0.5")]
    filters: Vec<f64>,
    /// Queries of each kind run with a cold cache (0 disables).
    #[arg(long, default_value_t = 100)]
    cold_queries: usize,
    /// Directory for the cold-cache segment files (default: system temp).
    #[arg(long)]
    tmp: Option<PathBuf>,
    /// Selectivities for the filter-strategy crossover.
    #[arg(long, value_delimiter = ',', default_value = "0.0001,0.0003,0.001,0.003,0.01,0.03,0.1,0.3")]
    crossover: Vec<f64>,
    /// Queries timed per crossover point.
    #[arg(long, default_value_t = 300)]
    crossover_queries: usize,
    #[arg(long)]
    no_crossover: bool,
    /// Loom thresholds to compare (sizes and latency); none by default.
    #[arg(long, value_delimiter = ',')]
    sweep_min_df: Vec<u32>,
    #[arg(long, default_value = "results/text")]
    out: PathBuf,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Top-k algorithms timed warm (exhaustive, maxscore, bmw).
    #[arg(long, value_delimiter = ',', default_value = "exhaustive,maxscore,bmw")]
    algos: Vec<String>,
}

/// The text of a BEIR collection.
struct Coll {
    ids: Vec<String>,
    bodies: Vec<String>,
    queries: Vec<(String, String)>,
    qrels: HashMap<String, HashMap<String, u32>>,
}

#[derive(Deserialize)]
struct RawDoc {
    id: String,
    title: String,
    text: String,
}

#[derive(Deserialize)]
struct RawQuery {
    id: String,
    text: String,
}

fn load(name: &str, limit: Option<usize>) -> Coll {
    let dir = strata::data::data_dir().join(name);
    let mut ids = Vec::new();
    let mut bodies = Vec::new();
    for line in BufReader::new(File::open(dir.join("docs.jsonl")).expect("docs.jsonl")).lines() {
        if limit.is_some_and(|l| ids.len() >= l) {
            break;
        }
        let r: RawDoc = serde_json::from_str(&line.unwrap()).unwrap();
        // As strata::data: the indexed body is title + text.
        bodies.push(if r.title.is_empty() { r.text } else { format!("{}\n{}", r.title, r.text) });
        ids.push(r.id);
    }
    let queries = BufReader::new(File::open(dir.join("queries.jsonl")).unwrap())
        .lines()
        .map(|l| {
            let r: RawQuery = serde_json::from_str(&l.unwrap()).unwrap();
            (r.id, r.text)
        })
        .collect();
    let mut qrels: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for line in BufReader::new(File::open(dir.join("qrels.tsv")).unwrap()).lines() {
        let line = line.unwrap();
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() >= 3 {
            qrels.entry(f[0].into()).or_default().insert(f[1].into(), f[2].trim().parse().unwrap_or(0));
        }
    }
    Coll { ids, bodies, queries, qrels }
}

/// Latency summary in microseconds.
fn lat(v: &[f64]) -> Value {
    json!({ "mean_us": mean(v), "p50_us": percentile(v, 50.0), "p99_us": percentile(v, 99.0), "queries": v.len() })
}

/// Median over `reps` timed runs of `f(i)` (after one warm-up), per query.
fn time_queries(n: usize, reps: usize, mut f: impl FnMut(usize)) -> Vec<f64> {
    (0..n)
        .map(|i| {
            f(i);
            let mut ts: Vec<f64> = (0..reps.max(1))
                .map(|_| {
                    let t = Instant::now();
                    f(i);
                    t.elapsed().as_secs_f64() * 1e6
                })
                .collect();
            ts.sort_by(|a, b| a.total_cmp(b));
            ts[ts.len() / 2]
        })
        .collect()
}

fn random_filter(rng: &mut StdRng, n: u32, p: f64) -> DocSet {
    DocSet::from_sorted((0..n).filter(|_| rng.gen_bool(p)).collect(), n)
}

const ALGOS: [TopKAlgo; 3] = [TopKAlgo::Exhaustive, TopKAlgo::MaxScore, TopKAlgo::BlockMaxWand];

fn parse_algo(s: &str) -> TopKAlgo {
    *ALGOS.iter().find(|&&a| algo_name(a) == s).unwrap_or_else(|| panic!("unknown algorithm {s}"))
}

fn algo_name(a: TopKAlgo) -> &'static str {
    match a {
        TopKAlgo::Exhaustive => "exhaustive",
        TopKAlgo::MaxScore => "maxscore",
        TopKAlgo::BlockMaxWand => "bmw",
    }
}

/// The workload of one collection.
struct Workload {
    /// (query index, analysed terms) of every query with terms.
    bm25: Vec<(usize, Vec<String>)>,
    /// Indices into `bm25` of the timed sample.
    sample: Vec<usize>,
    /// 2-3 distinct indexed terms of a sampled query, for AND / OR.
    boolean: Vec<Vec<String>>,
}

fn workload(c: &Coll, r: &TextFieldReader, max: usize, seed: u64) -> Workload {
    let bm25: Vec<(usize, Vec<String>)> =
        c.queries.iter().enumerate().map(|(i, (_, t))| (i, analyze(t))).filter(|(_, t)| !t.is_empty()).collect();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut sample: Vec<usize> = index::sample(&mut rng, bm25.len(), max.min(bm25.len())).into_vec();
    sample.sort_unstable();
    let mut boolean = Vec::new();
    for &i in &sample {
        let mut terms: Vec<String> = bm25[i].1.iter().filter(|t| r.doc_freq(t) > 0).cloned().collect();
        terms.sort();
        terms.dedup();
        if terms.len() >= 2 {
            let m = rng.gen_range(2..=3.min(terms.len()));
            let pick = index::sample(&mut rng, terms.len(), m);
            boolean.push(pick.iter().map(|j| terms[j].clone()).collect());
        }
    }
    Workload { bm25, sample, boolean }
}

fn and_query(ts: &[String]) -> BoolQuery {
    BoolQuery::And(ts.iter().map(|t| BoolQuery::Term(t.clone())).collect())
}

fn or_query(ts: &[String]) -> BoolQuery {
    BoolQuery::Or(ts.iter().map(|t| BoolQuery::Term(t.clone())).collect())
}

/// Mean nDCG@k over the judged queries (BEIR: a document with the query's
/// own id is not counted). Queries without terms score 0.
fn ndcg(c: &Coll, w: &Workload, k: usize, mut rank: impl FnMut(&[String]) -> Vec<(DocId, f32)>) -> f64 {
    let with_terms: HashMap<usize, &Vec<String>> = w.bm25.iter().map(|(i, t)| (*i, t)).collect();
    let mut scores = Vec::new();
    for (qi, (qid, _)) in c.queries.iter().enumerate() {
        let Some(judged) = c.qrels.get(qid) else { continue };
        let ranked: Vec<String> = match with_terms.get(&qi) {
            Some(terms) => rank(terms).into_iter().map(|(d, _)| c.ids[d as usize].clone()).filter(|id| id != qid).take(k).collect(),
            None => Vec::new(),
        };
        scores.push(ndcg_at_k(&ranked, judged, k));
    }
    mean(&scores)
}

/// Exhaustive BM25 with exact document lengths (the index stores one-byte
/// norms), for the quantisation experiment.
fn exact_len_top_k(r: &TextFieldReader, terms: &[String], lens: &[u32], p: Bm25Params, k: usize, acc: &mut Vec<f32>) -> Vec<(DocId, f32)> {
    let stats = r.stats(terms);
    let avgdl = stats.avgdl();
    acc.resize(r.n_docs() as usize, 0.0);
    let mut touched: Vec<DocId> = Vec::new();
    let mut uniq: Vec<(&String, u32)> = Vec::new();
    for t in terms {
        match uniq.iter_mut().find(|(u, _)| *u == t) {
            Some(e) => e.1 += 1,
            None => uniq.push((t, 1)),
        }
    }
    for (t, m) in uniq {
        let Some(mut c) = r.postings(t) else { continue };
        let w = m as f32 * stats.idf(c.len() as u64) * (p.k1 + 1.0);
        while c.doc() != END {
            let d = c.doc();
            let tf = c.freq() as f32;
            let kk = p.k1 * (1.0 - p.b + p.b * lens[d as usize] as f32 / avgdl);
            if acc[d as usize] == 0.0 {
                touched.push(d);
            }
            acc[d as usize] += w * tf / (tf + kk);
            c.next();
        }
    }
    let mut v: Vec<(DocId, f32)> = touched.iter().map(|&d| (d, std::mem::take(&mut acc[d as usize]))).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    v.truncate(k);
    v
}

/// Number of analysed terms of a text, without stemming.
fn exact_len(text: &str) -> u32 {
    let mut n = 0;
    for_each_word(text, |w| n += !is_stopword(w) as u32);
    n
}

/// How loose the stored bounds are under the default parameters: for the
/// query terms' lists, the block impact bound over the block's true best
/// score, and the list corner (max tf, min norm) over the frontier bound
/// (which is exact).
fn impact_tightness(r: &TextFieldReader, w: &Workload) -> Value {
    let p = Bm25Params::default();
    let stats = CollectionStats { n_docs: r.n_docs() as u64, total_len: r.total_len(), df: HashMap::new() };
    let avgdl = stats.avgdl();
    let kn: Vec<f32> = (0..256).map(|i| p.k1 * (1.0 - p.b + p.b * NORM_LENGTHS[i] as f32 / avgdl)).collect();
    let s = |tf: u32, n: u8| tf as f32 / (tf as f32 + kn[n as usize]);
    let mut terms: Vec<&String> = w.sample.iter().flat_map(|&i| &w.bm25[i].1).collect();
    terms.sort();
    terms.dedup();
    let (mut block_ratios, mut list_ratios) = (Vec::new(), Vec::new());
    for t in terms {
        let Some(mut c) = r.postings(t) else { continue };
        let exact_list = c.frontier().iter().map(|&(tf, n)| s(tf, n)).fold(0.0, f32::max);
        let (mut max_tf, mut min_n) = (0, 255u8);
        let (mut best, mut first) = (0.0f32, c.doc());
        let mut rank = 0usize;
        while c.doc() != END {
            let d = c.doc();
            let (tf, n) = (c.freq(), r.norms()[d as usize]);
            max_tf = max_tf.max(tf);
            min_n = min_n.min(n);
            best = best.max(s(tf, n));
            rank += 1;
            c.next();
            if rank.is_multiple_of(128) || c.doc() == END {
                // The block of `first` ends here; compare with its stored impact.
                let mut probe = r.postings(t).unwrap();
                let (_, imp) = probe.block_bound(first);
                if imp != LIST_IMPACT {
                    block_ratios.push((s(imp >> 8, (imp & 0xff) as u8) / best) as f64);
                }
                best = 0.0;
                first = c.doc();
            }
        }
        if c.len() as usize > 128 {
            list_ratios.push((s(max_tf, min_n) / exact_list) as f64);
        }
    }
    json!({
        "block_bound_over_block_max": { "mean": mean(&block_ratios), "p50": percentile(&block_ratios, 50.0), "p90": percentile(&block_ratios, 90.0), "blocks": block_ratios.len() },
        "list_corner_over_frontier": { "mean": mean(&list_ratios), "p50": percentile(&list_ratios, 50.0), "p90": percentile(&list_ratios, 90.0), "lists": list_ratios.len() },
    })
}

fn build(b: &TextFieldBuilder, n: usize, format: PostingsFormat, min_df: u32) -> (Vec<u8>, f64) {
    let t = Instant::now();
    let mut w = SegmentWriter::new(n);
    b.write_as(&mut w, format, min_df).unwrap();
    let image = w.finish();
    (image, t.elapsed().as_secs_f64())
}

fn sizes_json(r: &TextFieldReader, seg: &SegmentFile) -> Value {
    let m = r.meta();
    let s = r.sizes();
    let np = m.n_postings.max(1) as f64;
    let sections: serde_json::Map<String, Value> = seg.section_sizes().into_iter().map(|(k, v)| (k, json!(v))).collect();
    // Bytes by list length: where per-list overhead goes.
    const BUCKETS: [(u32, u32); 6] = [(1, 1), (2, 15), (16, 127), (128, 1023), (1024, 16383), (16384, u32::MAX)];
    let mut by_df = vec![[0usize; 5]; BUCKETS.len()];
    r.for_each_term(|_, info| {
        let b = BUCKETS.iter().position(|&(lo, hi)| info.df >= lo && info.df <= hi).unwrap();
        let ls = r.list_sizes(&info);
        let e = &mut by_df[b];
        e[0] += 1;
        e[1] += info.df as usize;
        e[2] += ls.docs;
        e[3] += ls.freqs;
        e[4] += ls.meta;
    });
    let by_df: Vec<Value> = BUCKETS
        .iter()
        .zip(&by_df)
        .map(|(&(lo, hi), e)| {
            let p = e[1].max(1) as f64;
            json!({ "df_min": lo, "df_max": hi, "terms": e[0], "postings": e[1],
                    "docs_bits_per_posting": e[2] as f64 * 8.0 / p, "freqs_bits_per_posting": e[3] as f64 * 8.0 / p,
                    "meta_bits_per_posting": e[4] as f64 * 8.0 / p })
        })
        .collect();
    json!({
        "by_list_length": by_df,
        "sections": sections,
        "bits_per_posting": {
            "docs": s.docs as f64 * 8.0 / np,
            "freqs": s.freqs as f64 * 8.0 / np,
            "list_meta": s.list_meta as f64 * 8.0 / np,
            "postings_section": (s.docs + s.freqs + s.list_meta) as f64 * 8.0 / np,
        },
        "bits_per_position": s.positions as f64 * 8.0 / m.total_len.max(1) as f64,
        "dict_bytes": s.dict,
        "dict_bytes_per_term": s.dict as f64 / m.n_terms.max(1) as f64,
        "norms_bytes": s.norms,
        "segment_bytes": seg.image().len(),
    })
}

/// Warm latency of every query kind on one reader.
fn warm(r: &TextFieldReader, w: &Workload, filters: &[(f64, DocSet)], a: &Args) -> Value {
    let mut bm25 = Vec::new();
    for algo in a.algos.iter().map(|s| parse_algo(s)) {
        for (sel, f) in std::iter::once((None, None)).chain(filters.iter().map(|(s, f)| (Some(*s), Some(f)))) {
            let ts = time_queries(w.sample.len(), a.reps, |i| {
                let terms = &w.bm25[w.sample[i]].1;
                let stats = r.stats(terms);
                black_box(r.top_k(terms, &stats, Bm25Params::default(), a.k, f.map(|f| f as _), algo));
            });
            let mut e = lat(&ts);
            e["algo"] = json!(algo_name(algo));
            e["filter"] = json!(sel);
            if sel.is_none() {
                // By the number of distinct query terms.
                let mut by_len = Vec::new();
                for (lo, hi) in [(1, 2), (3, 4), (5, 7), (8, usize::MAX)] {
                    let v: Vec<f64> = (0..ts.len())
                        .filter(|&i| {
                            let mut t = w.bm25[w.sample[i]].1.clone();
                            t.sort();
                            t.dedup();
                            t.len() >= lo && t.len() <= hi
                        })
                        .map(|i| ts[i])
                        .collect();
                    by_len.push(json!({ "terms_min": lo, "terms_max": hi, "queries": v.len(), "mean_us": mean(&v) }));
                }
                e["by_terms"] = json!(by_len);
            }
            bm25.push(e);
        }
    }
    let mut boolean = Vec::new();
    for (op, mk) in [("and", and_query as fn(&[String]) -> BoolQuery), ("or", or_query)] {
        let qs: Vec<BoolQuery> = w.boolean.iter().map(|t| mk(t)).collect();
        let mut results = 0usize;
        let ts = time_queries(qs.len(), a.reps, |i| {
            results += r.eval(&qs[i], None).len();
        });
        let mut e = lat(&ts);
        e["op"] = json!(op);
        e["mean_results"] = json!(results as f64 / ((a.reps + 1) * qs.len().max(1)) as f64);
        boolean.push(e);
    }
    json!({ "bm25": bm25, "boolean": boolean })
}

/// Cold-cache latency and pages read: the segment is a file mapped with
/// `MADV_RANDOM` (no read-around, so pages read are pages touched); before
/// every query the mapping's page-table entries and the file's page cache
/// are dropped.
fn cold(image: &[u8], path: &Path, w: &Workload, a: &Args) -> Value {
    std::fs::write(path, image).unwrap();
    File::open(path).unwrap().sync_all().unwrap();
    let file = File::open(path).unwrap();
    // SAFETY: the file is private to this run and not modified while mapped.
    let mmap = unsafe { Mmap::map(&file).unwrap() };
    mmap.advise(Advice::Random).unwrap();
    let (ptr, len) = (mmap.as_ptr(), mmap.len());
    let seg = SegmentFile::open(OwnedBytes::new(mmap)).unwrap();
    let r = TextFieldReader::open(&seg, "body").unwrap();
    let n = a.cold_queries.min(w.sample.len());
    // Page ranges of the sections, to split the pages read by section.
    let sections: Vec<(String, std::ops::Range<usize>)> = seg
        .section_sizes()
        .into_iter()
        .map(|(name, _)| {
            let r = seg.section_range(&name).unwrap();
            let part = name.rsplit('.').next().unwrap().to_string();
            (part, r.start / iostat::PAGE..r.end.div_ceil(iostat::PAGE))
        })
        .collect();
    let mut out = Vec::new();
    let mut run = |name: &str, nq: usize, f: &mut dyn FnMut(usize)| {
        let (mut ts, mut pages) = (Vec::new(), Vec::new());
        let mut by_section: HashMap<String, f64> = HashMap::new();
        for i in 0..nq {
            // SAFETY: a read-only shared mapping of a file nobody writes.
            unsafe { iostat::drop_mapping(ptr, len).unwrap() };
            if let Err(e) = iostat::evict(path) {
                eprintln!("warning: eviction failed: {e}");
            }
            let before = iostat::resident_pages(path).unwrap();
            let t = Instant::now();
            f(i);
            ts.push(t.elapsed().as_secs_f64() * 1e6);
            pages.push(iostat::resident_pages(path).unwrap().saturating_sub(before) as f64);
            let resident = resident_map(ptr, len);
            for (part, range) in &sections {
                *by_section.entry(part.clone()).or_default() += resident[range.clone()].iter().filter(|&&b| b & 1 == 1).count() as f64;
            }
        }
        let mut e = lat(&ts);
        e["query"] = json!(name);
        e["pages_mean"] = json!(mean(&pages));
        e["pages_mean_by_section"] = json!(by_section.iter().map(|(k, v)| (k.clone(), v / nq.max(1) as f64)).collect::<HashMap<_, _>>());
        e["pages_p50"] = json!(percentile(&pages, 50.0));
        e["pages_p99"] = json!(percentile(&pages, 99.0));
        out.push(e);
    };
    for algo in [TopKAlgo::MaxScore, TopKAlgo::BlockMaxWand] {
        run(&format!("bm25 {}", algo_name(algo)), n, &mut |i| {
            let terms = &w.bm25[w.sample[i]].1;
            let stats = r.stats(terms);
            black_box(r.top_k(terms, &stats, Bm25Params::default(), a.k, None, algo));
        });
    }
    let nb = a.cold_queries.min(w.boolean.len());
    run("and", nb, &mut |i| {
        black_box(r.eval(&and_query(&w.boolean[i]), None));
    });
    run("or", nb, &mut |i| {
        black_box(r.eval(&or_query(&w.boolean[i]), None));
    });
    drop(r);
    drop(seg);
    std::fs::remove_file(path).unwrap();
    json!(out)
}

/// Page-cache residency of every page of a mapping (`mincore`).
fn resident_map(ptr: *const u8, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len.div_ceil(iostat::PAGE)];
    // SAFETY: `ptr..ptr + len` is a live, page-aligned mapping.
    let r = unsafe { libc::mincore(ptr as *mut libc::c_void, len, v.as_mut_ptr()) };
    assert_eq!(r, 0, "mincore failed");
    v
}

/// Per-query points `(x, drive us, probe us)` of one query family, where
/// `x = members * terms / cost` is what the `Auto` rules compare with
/// `1 / constant` (drive iff `x * constant < 1`).
type Points = Vec<(f64, f64, f64)>;

/// The constant minimising the family's total time over all points, and
/// the totals of the alternatives.
fn fit(points: &Points, current: f64) -> Value {
    let total = |thr: f64| points.iter().map(|&(x, d, p)| if x < thr { d } else { p }).sum::<f64>();
    let mut xs: Vec<f64> = points.iter().map(|p| p.0).filter(|x| x.is_finite()).collect();
    xs.sort_by(|a, b| a.total_cmp(b));
    xs.dedup();
    // Candidate thresholds: below all points, between neighbours, above all.
    let mut cands = vec![0.0];
    cands.extend(xs.windows(2).map(|w| (w[0] * w[1]).sqrt().max((w[0] + w[1]) / 2.0 * 1e-9)));
    cands.push(f64::INFINITY);
    let best = cands.iter().copied().min_by(|&a, &b| total(a).total_cmp(&total(b))).unwrap();
    json!({
        "fitted_constant": 1.0 / best,
        "threshold_x": best,
        "total_ms_fitted": total(best) / 1e3,
        "total_ms_current": total(1.0 / current) / 1e3,
        "total_ms_always_drive": total(f64::INFINITY) / 1e3,
        "total_ms_always_probe": total(0.0) / 1e3,
        "points": points.len(),
    })
}

/// Filter strategies (drive from the filter vs probe while iterating)
/// across selectivities, for BM25 and boolean AND / OR, and the cost
/// constants that fit them best.
fn crossover(r: &TextFieldReader, w: &Workload, a: &Args, rng: &mut StdRng) -> Value {
    let n = r.n_docs();
    let nq = a.crossover_queries.min(w.sample.len());
    let nb = a.crossover_queries.min(w.boolean.len());
    let mut rows = Vec::new();
    let mut points: HashMap<&str, Points> = HashMap::new();
    let bool_qs: Vec<(&str, Vec<BoolQuery>)> =
        vec![("and", w.boolean[..nb].iter().map(|t| and_query(t)).collect()), ("or", w.boolean[..nb].iter().map(|t| or_query(t)).collect())];
    for &sel in &a.crossover {
        let f = random_filter(rng, n, sel);
        let m = f.len() as f64;
        let mut row = json!({ "selectivity": sel, "members": f.len() });
        for algo in [TopKAlgo::MaxScore, TopKAlgo::BlockMaxWand] {
            let mut by: Vec<Vec<f64>> = Vec::new();
            for strategy in [FilterStrategy::Drive, FilterStrategy::Probe, FilterStrategy::Auto] {
                let ts = time_queries(nq, a.reps, |i| {
                    let terms = &w.bm25[w.sample[i]].1;
                    let stats = r.stats(terms);
                    black_box(r.top_k_with(terms, &stats, Bm25Params::default(), a.k, Some(&f), algo, strategy));
                });
                row[format!("{}_{strategy:?}_mean_us", algo_name(algo)).to_lowercase()] = json!(mean(&ts));
                by.push(ts);
            }
            let pts = points.entry(algo_name(algo)).or_default();
            for i in 0..nq {
                let terms = &w.bm25[w.sample[i]].1;
                let postings: f64 = terms.iter().map(|t| r.doc_freq(t) as f64).sum();
                let distinct = terms.iter().collect::<std::collections::HashSet<_>>().len() as f64;
                pts.push((m * distinct / postings.max(1.0), by[0][i], by[1][i]));
            }
        }
        for (op, qs) in &bool_qs {
            let mut by: Vec<Vec<f64>> = Vec::new();
            for strategy in [FilterStrategy::Drive, FilterStrategy::Probe, FilterStrategy::Auto] {
                let ts = time_queries(qs.len(), a.reps, |i| {
                    black_box(r.eval_with(&qs[i], Some(&f), strategy));
                });
                row[format!("{op}_{strategy:?}_mean_us").to_lowercase()] = json!(mean(&ts));
                by.push(ts);
            }
            let pts = points.entry(op).or_default();
            for (i, q) in qs.iter().enumerate() {
                pts.push((m * q.terms().len() as f64 / r.eval_cost(q).max(1.0), by[0][i], by[1][i]));
            }
        }
        eprintln!("  crossover {row}");
        rows.push(row);
    }
    let fits = json!({
        "maxscore": fit(&points["maxscore"], drive_cost(TopKAlgo::MaxScore)),
        "bmw": fit(&points["bmw"], drive_cost(TopKAlgo::BlockMaxWand)),
        "and": fit(&points["and"], PROBE_COST),
        "or": fit(&points["or"], PROBE_COST),
    });
    eprintln!("  fitted {fits}");
    json!({ "rows": rows, "fits": fits })
}

/// Peak resident set size of this process so far (Linux).
fn peak_rss_mb() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok())
        .map_or(0.0, |kb| kb / 1024.0)
}

fn main() {
    let a = Args::parse();
    std::fs::create_dir_all(&a.out).unwrap();
    let tmp = a.tmp.clone().unwrap_or_else(std::env::temp_dir);
    for name in &a.collections {
        let t = Instant::now();
        let c = load(name, a.limit);
        let n = c.bodies.len();
        eprintln!("{name}: {n} documents, {} queries, loaded in {:.1}s", c.queries.len(), t.elapsed().as_secs_f64());
        let t = Instant::now();
        let mut b = TextFieldBuilder::new("body", true, PostingsFormat::Loom, a.loom_min_df);
        for body in &c.bodies {
            b.add_text(body);
        }
        let analyze_secs = t.elapsed().as_secs_f64();
        let mut rng = StdRng::seed_from_u64(a.seed);
        let filters: Vec<(f64, DocSet)> = a.filters.iter().map(|&p| (p, random_filter(&mut rng, n as u32, p))).collect();
        let mut formats = Vec::new();
        let mut collection = json!({ "collection": name, "documents": n, "queries": c.queries.len(), "analyze_secs": analyze_secs });
        let mut workload_out: Option<Workload> = None;
        for fname in &a.formats {
            let format = PostingsFormat::parse(fname).expect("format");
            let (image, write_secs) = build(&b, n, format, a.loom_min_df);
            let seg = SegmentFile::from_vec(image).unwrap();
            let r = TextFieldReader::open(&seg, "body").unwrap();
            let w = workload_out.take().unwrap_or_else(|| workload(&c, &r, a.max_queries, a.seed));
            let m = r.meta().clone();
            collection["terms"] = json!(m.n_terms);
            collection["postings"] = json!(m.n_postings);
            collection["positions"] = json!(m.total_len);
            collection["avg_doc_len"] = json!(m.total_len as f64 / n.max(1) as f64);
            collection["timed_queries"] = json!(w.sample.len());
            collection["boolean_queries"] = json!(w.boolean.len());
            let mut f = json!({ "format": fname, "loom_min_df": a.loom_min_df, "write_secs": write_secs, "build_secs": analyze_secs + write_secs });
            f["sizes"] = sizes_json(&r, &seg);
            eprintln!("  {fname}: written in {write_secs:.2}s, {}", f["sizes"]["bits_per_posting"]);
            f["warm"] = warm(&r, &w, &filters, &a);
            for e in f["warm"]["bm25"].as_array().unwrap() {
                eprintln!("    bm25 {} filter {}: mean {:.1} us p99 {:.1}", e["algo"], e["filter"], e["mean_us"].as_f64().unwrap(), e["p99_us"].as_f64().unwrap());
            }
            for e in f["warm"]["boolean"].as_array().unwrap() {
                eprintln!("    {}: mean {:.1} us p99 {:.1}", e["op"], e["mean_us"].as_f64().unwrap(), e["p99_us"].as_f64().unwrap());
            }
            if formats.is_empty() {
                // Format-independent measurements, once.
                let p = Bm25Params::default();
                let bm25 = |terms: &[String], p: Bm25Params| r.top_k(terms, &r.stats(terms), p, a.k + 1, None, TopKAlgo::BlockMaxWand);
                let lens: Vec<u32> = c.bodies.iter().map(|t| exact_len(t)).collect();
                let mut acc = Vec::new();
                let alt = Bm25Params { k1: 1.2, b: 0.75 };
                collection["ndcg@10"] = json!({
                    "bm25 k1=0.9 b=0.4": ndcg(&c, &w, a.k, |t| bm25(t, p)),
                    "bm25 k1=1.2 b=0.75": ndcg(&c, &w, a.k, |t| bm25(t, alt)),
                    "bm25 k1=0.9 b=0.4, exact lengths": ndcg(&c, &w, a.k, |t| exact_len_top_k(&r, t, &lens, p, a.k + 1, &mut acc)),
                    "bm25 k1=1.2 b=0.75, exact lengths": ndcg(&c, &w, a.k, |t| exact_len_top_k(&r, t, &lens, alt, a.k + 1, &mut acc)),
                    "judged_queries": c.queries.iter().filter(|(id, _)| c.qrels.contains_key(id)).count(),
                });
                eprintln!("  nDCG@10 {}", collection["ndcg@10"]);
                collection["impact_tightness"] = impact_tightness(&r, &w);
                eprintln!("  impact tightness {}", collection["impact_tightness"]);
                if !a.no_crossover {
                    collection["filter_crossover"] = crossover(&r, &w, &a, &mut rng);
                }
            }
            if a.cold_queries > 0 {
                let path = tmp.join(format!("strata-text-bench-{}-{name}-{fname}.seg", std::process::id()));
                f["cold"] = cold(seg.image(), &path, &w, &a);
                for e in f["cold"].as_array().unwrap() {
                    eprintln!("    cold {}: mean {:.0} us, {:.1} pages", e["query"], e["mean_us"].as_f64().unwrap(), e["pages_mean"].as_f64().unwrap());
                }
            }
            formats.push(f);
            workload_out = Some(w);
        }
        collection["formats"] = json!(formats);
        if !a.sweep_min_df.is_empty() {
            let mut sweep = Vec::new();
            for &t in &a.sweep_min_df {
                let (image, write_secs) = build(&b, n, PostingsFormat::Loom, t);
                let seg = SegmentFile::from_vec(image).unwrap();
                let r = TextFieldReader::open(&seg, "body").unwrap();
                let w = workload_out.as_ref().unwrap();
                let mut e = json!({ "loom_min_df": t, "write_secs": write_secs, "sizes": sizes_json(&r, &seg) });
                let warm = warm(&r, w, &[], &a);
                e["bm25"] = warm["bm25"].clone();
                e["boolean"] = warm["boolean"].clone();
                eprintln!("  loom_min_df {t}: {}", e["sizes"]["bits_per_posting"]);
                sweep.push(e);
            }
            collection["loom_min_df_sweep"] = json!(sweep);
        }
        collection["peak_rss_mb"] = json!(peak_rss_mb());
        let path = a.out.join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&collection).unwrap()).unwrap();
        eprintln!("wrote {} (peak RSS {:.0} MB)", path.display(), peak_rss_mb());
    }
}
