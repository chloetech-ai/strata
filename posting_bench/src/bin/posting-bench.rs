//! `posting-bench`: command-line testbed for compressed posting lists and
//! intersection / union algorithms. Run `posting-bench --help`.

use clap::{Args, Parser, Subcommand, ValueEnum};
use posting_bench::bench::{self, BenchConfig, CodecReport, KernelRow, Op, Workload};
use posting_bench::data::{self, Collection, Distribution, QueryConfig, QueryMode, SyntheticConfig};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "posting-bench", version, about = "Benchmark compressed posting lists and set-operation algorithms")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Benchmark codecs and AND/OR algorithms on a collection and query log.
    Run(RunArgs),
    /// Benchmark pairwise intersection kernels across list-size ratios.
    Kernels(KernelArgs),
    /// Write a collection (and generated queries) in ds2i/PISA format.
    Export(ExportArgs),
    /// Print structural statistics of a collection and its queries.
    Stats(StatsArgs),
    /// List the available codecs.
    Codecs,
}

#[derive(Args)]
struct StatsArgs {
    #[command(flatten)]
    source: SourceArgs,
    #[command(flatten)]
    query: QueryArgs,
}

#[derive(Clone, Copy, ValueEnum)]
enum Source {
    /// Generated collection (see --distribution and friends).
    Synthetic,
    /// ds2i/PISA binary `.docs` file given by --input.
    Ds2i,
    /// Text file with one document per line, given by --input.
    Text,
}

#[derive(Args)]
struct SourceArgs {
    #[arg(long, value_enum, default_value_t = Source::Synthetic)]
    source: Source,
    /// Input file for --source ds2i / text.
    #[arg(long)]
    input: Option<PathBuf>,
    /// Synthetic docID distribution.
    #[arg(long, value_enum, default_value_t = Distribution::Clustered)]
    distribution: Distribution,
    /// Synthetic: number of documents.
    #[arg(long, default_value_t = 8_000_000)]
    universe: u32,
    /// Synthetic: number of posting lists (terms).
    #[arg(long, default_value_t = 20_000)]
    terms: usize,
    /// Synthetic: length of the longest list.
    #[arg(long, default_value_t = 2_000_000)]
    max_len: usize,
    /// Synthetic: Zipf exponent of list lengths.
    #[arg(long, default_value_t = 1.0)]
    zipf: f64,
    /// Synthetic (clustered): number of topic regions.
    #[arg(long, default_value_t = 1000)]
    topics: usize,
    /// Synthetic (clustered): fraction of postings inside preferred topics.
    #[arg(long, default_value_t = 0.85)]
    topic_mass: f64,
    /// Synthetic (clustered): mean run length of consecutive docIDs.
    #[arg(long, default_value_t = 4.0)]
    run_len: f64,
    /// Random seed for data and query generation.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// ds2i: lists shorter than this are dropped (kept empty).
    #[arg(long, default_value_t = 0)]
    min_len: usize,
    /// ds2i: read at most this many lists.
    #[arg(long)]
    max_lists: Option<usize>,
    /// text: read at most this many documents.
    #[arg(long)]
    max_docs: Option<usize>,
}

#[derive(Args)]
struct QueryArgs {
    /// Query file (one query per line: term ids, or words for text input).
    #[arg(long)]
    queries: Option<PathBuf>,
    /// Number of generated queries.
    #[arg(long, default_value_t = 500)]
    num_queries: usize,
    #[arg(long, default_value_t = 2)]
    min_terms: usize,
    #[arg(long, default_value_t = 4)]
    max_terms: usize,
    /// Generated queries only use lists with at least this many postings.
    #[arg(long, default_value_t = 1000)]
    min_list_len: usize,
    /// Generated queries only use lists with at most this many postings.
    #[arg(long)]
    max_list_len: Option<usize>,
    #[arg(long, value_enum, default_value_t = QueryMode::Cooccur)]
    query_mode: QueryMode,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    source: SourceArgs,
    #[command(flatten)]
    query: QueryArgs,
    /// Comma-separated, case-insensitive substrings selecting codecs.
    #[arg(long)]
    codecs: Option<String>,
    /// Operations to benchmark: and, or, or both.
    #[arg(long, default_value = "and,or")]
    ops: String,
    /// Timed passes per measurement (the best is kept).
    #[arg(long, default_value_t = 3)]
    reps: usize,
    /// Write all results as JSON.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Write per-algorithm results as CSV.
    #[arg(long)]
    csv: Option<PathBuf>,
}

#[derive(Args)]
struct KernelArgs {
    /// Size of the smaller list.
    #[arg(long, default_value_t = 10_000)]
    small: usize,
    /// Size ratios |large| / |small|.
    #[arg(long, value_delimiter = ',', default_value = "1,2,4,8,16,32,64,128,256,1024,4096")]
    ratios: Vec<usize>,
    /// Density of the larger list in its universe.
    #[arg(long, default_value_t = 0.1)]
    density: f64,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[arg(long)]
    json: Option<PathBuf>,
}

#[derive(Args)]
struct ExportArgs {
    #[command(flatten)]
    source: SourceArgs,
    #[command(flatten)]
    query: QueryArgs,
    /// Output `.docs` path.
    #[arg(long)]
    out: PathBuf,
    /// Also write generated queries (term ids) to this path.
    #[arg(long)]
    queries_out: Option<PathBuf>,
}

fn load_collection(a: &SourceArgs) -> Collection {
    let t = Instant::now();
    let input = || a.input.as_deref().expect("--input is required for this --source");
    let coll = match a.source {
        Source::Synthetic => data::synthetic(&SyntheticConfig {
            universe: a.universe,
            terms: a.terms,
            max_len: a.max_len,
            min_len: 1,
            zipf: a.zipf,
            distribution: a.distribution,
            topics: a.topics,
            topic_mass: a.topic_mass,
            run_len: a.run_len,
            seed: a.seed,
        }),
        Source::Ds2i => data::load_ds2i(input(), a.min_len, a.max_lists).expect("reading ds2i collection"),
        Source::Text => data::load_text(input(), a.max_docs).expect("reading text collection"),
    };
    eprintln!(
        "collection: {} — {} docs, {} lists, {} postings ({:.1}s)",
        coll.name,
        coll.universe,
        coll.lists.len(),
        coll.num_postings(),
        t.elapsed().as_secs_f64()
    );
    coll
}

fn load_queries(q: &QueryArgs, coll: &Collection, seed: u64) -> Vec<Vec<u32>> {
    match &q.queries {
        Some(p) => data::load_queries(p, coll).expect("reading queries"),
        None => data::generate_queries(
            coll,
            &QueryConfig {
                num: q.num_queries,
                min_terms: q.min_terms,
                max_terms: q.max_terms,
                min_list_len: q.min_list_len,
                max_list_len: q.max_list_len,
                mode: q.query_mode,
                seed: seed.wrapping_add(1000),
            },
        ),
    }
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| std::env::consts::ARCH.to_string())
}

fn simd_features() -> Vec<&'static str> {
    let mut v = Vec::new();
    macro_rules! feat {
        ($($f:literal),*) => { $( if cfg!(target_feature = $f) { v.push($f); } )* };
    }
    feat!("sse4.2", "avx2", "bmi2", "avx512f", "avx512bw", "neon");
    v
}

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn print_run(w: &Workload, reports: &[CodecReport]) {
    println!("\n## Collection and workload\n");
    println!("| | |\n|---|---|");
    println!("| collection | {} |", w.coll.name);
    println!("| documents | {} |", w.coll.universe);
    println!("| lists / postings | {} / {} |", w.coll.lists.len(), w.coll.num_postings());
    println!("| queries | {} (mean {:.2} terms, {:.0} postings) |", w.queries.len(), w.mean_terms(), w.mean_query_postings());
    println!("| mean AND / OR result | {:.0} / {:.0} docs |", w.mean_result_size(Op::And), w.mean_result_size(Op::Or));
    println!("| CPU | {} ({}) |", cpu_model(), simd_features().join(", "));

    println!("\n## Summary\n");
    println!("| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |");
    println!("|---|---:|---:|---:|---:|---:|---:|---|---|");
    for r in reports {
        let best = |op| {
            r.best(op)
                .map(|m| format!("{:.3} ({})", m.ms_per_query, m.algorithm))
                .unwrap_or_else(|| "—".into())
        };
        println!(
            "| {} | {:.2} | {:.2} | {:.1} | {:.2} | {:.2} | {:.2} | {} | {} |",
            r.codec,
            r.bits_per_posting,
            r.query_bits_per_posting,
            r.aux_memory_bytes as f64 / 1e6,
            r.build_secs,
            r.decode_ns_per_posting,
            r.query_decode_ns_per_posting,
            best(Op::And),
            best(Op::Or)
        );
    }

    println!("\n## Pareto frontier (bits/posting on query lists vs best ms/query)\n");
    for op in [Op::And, Op::Or] {
        let pts: Vec<(&str, f64, f64)> = reports
            .iter()
            .filter_map(|r| r.best(op).map(|m| (r.codec.as_str(), r.query_bits_per_posting, m.ms_per_query)))
            .collect();
        let mut front: Vec<&(&str, f64, f64)> = pts
            .iter()
            .filter(|p| !pts.iter().any(|q| q.1 <= p.1 && q.2 <= p.2 && (q.1 < p.1 || q.2 < p.2)))
            .collect();
        front.sort_by(|a, b| a.1.total_cmp(&b.1));
        let s: Vec<String> = front.iter().map(|p| format!("{} ({:.2} bits, {:.3} ms)", p.0, p.1, p.2)).collect();
        println!("* {}: {}", op.label(), s.join(" → "));
    }

    println!("\n## All measurements (ms per query)\n");
    println!("| codec | op | algorithm | ms/query |");
    println!("|---|---|---|---:|");
    for r in reports {
        for m in &r.measurements {
            println!("| {} | {} | {} | {:.3} |", r.codec, m.op.label(), m.algorithm, m.ms_per_query);
        }
    }
}

fn run_json(w: &Workload, reports: &[CodecReport], args: &RunArgs) -> String {
    let mut s = String::from("{\n");
    let _ = writeln!(s, "  \"collection\": {},", json_str(&w.coll.name));
    let _ = writeln!(s, "  \"documents\": {},", w.coll.universe);
    let _ = writeln!(s, "  \"lists\": {},", w.coll.lists.len());
    let _ = writeln!(s, "  \"postings\": {},", w.coll.num_postings());
    let _ = writeln!(s, "  \"queries\": {},", w.queries.len());
    let _ = writeln!(s, "  \"mean_terms\": {:.4},", w.mean_terms());
    let _ = writeln!(s, "  \"mean_query_postings\": {:.1},", w.mean_query_postings());
    let _ = writeln!(s, "  \"mean_and_results\": {:.1},", w.mean_result_size(Op::And));
    let _ = writeln!(s, "  \"mean_or_results\": {:.1},", w.mean_result_size(Op::Or));
    let _ = writeln!(s, "  \"reps\": {},", args.reps);
    let _ = writeln!(s, "  \"cpu\": {},", json_str(&cpu_model()));
    let feats: Vec<String> = simd_features().iter().map(|f| json_str(f)).collect();
    let _ = writeln!(s, "  \"simd\": [{}],", feats.join(", "));
    s.push_str("  \"codecs\": [\n");
    for (i, r) in reports.iter().enumerate() {
        let _ = writeln!(s, "    {{\"codec\": {}, \"bits_per_posting\": {:.4}, \"query_bits_per_posting\": {:.4}, \"build_secs\": {:.4}, \"decode_ns_per_posting\": {:.4}, \"query_decode_ns_per_posting\": {:.4}, \"aux_memory_bytes\": {}, \"measurements\": [", json_str(&r.codec), r.bits_per_posting, r.query_bits_per_posting, r.build_secs, r.decode_ns_per_posting, r.query_decode_ns_per_posting, r.aux_memory_bytes);
        for (j, m) in r.measurements.iter().enumerate() {
            let _ = write!(
                s,
                "      {{\"op\": \"{}\", \"algorithm\": {}, \"ms_per_query\": {:.6}}}",
                m.op.label(),
                json_str(&m.algorithm),
                m.ms_per_query
            );
            s.push_str(if j + 1 < r.measurements.len() { ",\n" } else { "\n" });
        }
        s.push_str(if i + 1 < reports.len() { "    ]},\n" } else { "    ]}\n" });
    }
    s.push_str("  ]\n}\n");
    s
}

fn cmd_run(args: RunArgs) {
    let coll = load_collection(&args.source);
    let queries = load_queries(&args.query, &coll, args.source.seed);
    let t = Instant::now();
    let w = Workload::new(&coll, &queries);
    eprintln!(
        "workload: {} queries, mean AND {:.0} / OR {:.0} results (reference in {:.1}s)",
        queries.len(),
        w.mean_result_size(Op::And),
        w.mean_result_size(Op::Or),
        t.elapsed().as_secs_f64()
    );
    let ops = args.ops.to_lowercase();
    let cfg = BenchConfig { reps: args.reps, and: ops.contains("and"), or: ops.contains("or") };
    let filters: Vec<String> = args
        .codecs
        .as_deref()
        .map(|s| s.split(',').map(|f| f.trim().to_lowercase()).filter(|f| !f.is_empty()).collect())
        .unwrap_or_default();
    let mut reports = Vec::new();
    for entry in bench::registry() {
        let lname = entry.name.to_lowercase();
        if !filters.is_empty() && !filters.iter().any(|f| lname.contains(f)) {
            continue;
        }
        let t = Instant::now();
        eprint!("{:<28} ", entry.name);
        let r = (entry.run)(&w, &cfg);
        eprintln!("{:.2} bits/posting, done in {:.1}s", r.bits_per_posting, t.elapsed().as_secs_f64());
        reports.push(r);
    }
    print_run(&w, &reports);
    if let Some(p) = &args.json {
        std::fs::write(p, run_json(&w, &reports, &args)).expect("writing JSON");
        eprintln!("wrote {}", p.display());
    }
    if let Some(p) = &args.csv {
        let mut s = String::from("codec,bits_per_posting,decode_ns_per_posting,op,algorithm,ms_per_query\n");
        for r in &reports {
            for m in &r.measurements {
                let _ = writeln!(
                    s,
                    "\"{}\",{:.4},{:.4},{},\"{}\",{:.6}",
                    r.codec, r.bits_per_posting, r.decode_ns_per_posting, m.op.label(), m.algorithm, m.ms_per_query
                );
            }
        }
        std::fs::write(p, s).expect("writing CSV");
        eprintln!("wrote {}", p.display());
    }
}

fn cmd_kernels(args: KernelArgs) {
    let rows: Vec<KernelRow> = bench::bench_kernels(args.small, &args.ratios, args.density, args.seed);
    let mut kernels: Vec<String> = Vec::new();
    for r in &rows {
        if !kernels.contains(&r.kernel) {
            kernels.push(r.kernel.clone());
        }
    }
    println!("\n## Pairwise intersection kernels (µs per intersection, |small| = {}, large-list density {})\n", args.small, args.density);
    println!("| ratio | {} |", kernels.join(" | "));
    println!("|---:|{}", "---:|".repeat(kernels.len()));
    for &ratio in &args.ratios {
        let cells: Vec<String> = kernels
            .iter()
            .map(|k| {
                rows.iter()
                    .find(|r| r.ratio == ratio && &r.kernel == k)
                    .map(|r| format!("{:.1}", r.micros_per_op))
                    .unwrap_or_default()
            })
            .collect();
        println!("| {} | {} |", ratio, cells.join(" | "));
    }
    println!("\nCPU: {} ({})", cpu_model(), simd_features().join(", "));
    if let Some(p) = &args.json {
        let mut s = String::from("{\n");
        let _ = writeln!(s, "  \"small\": {},\n  \"density\": {},\n  \"cpu\": {},\n  \"rows\": [", args.small, args.density, json_str(&cpu_model()));
        for (i, r) in rows.iter().enumerate() {
            let _ = write!(
                s,
                "    {{\"kernel\": {}, \"ratio\": {}, \"small\": {}, \"large\": {}, \"micros_per_op\": {:.4}, \"ns_per_small_element\": {:.4}}}",
                json_str(&r.kernel),
                r.ratio,
                r.small,
                r.large,
                r.micros_per_op,
                r.ns_per_small_element
            );
            s.push_str(if i + 1 < rows.len() { ",\n" } else { "\n" });
        }
        s.push_str("  ]\n}\n");
        std::fs::write(p, s).expect("writing JSON");
        eprintln!("wrote {}", p.display());
    }
}

fn cmd_export(args: ExportArgs) {
    let coll = load_collection(&args.source);
    coll.write_ds2i(&args.out).expect("writing collection");
    eprintln!("wrote {}", args.out.display());
    if let Some(p) = &args.queries_out {
        let queries = load_queries(&args.query, &coll, args.source.seed);
        let s: String = queries
            .iter()
            .map(|q| q.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(" ") + "\n")
            .collect();
        std::fs::write(p, s).expect("writing queries");
        eprintln!("wrote {}", p.display());
    }
}

fn main() {
    match Cli::parse().cmd {
        Cmd::Run(a) => cmd_run(a),
        Cmd::Kernels(a) => cmd_kernels(a),
        Cmd::Export(a) => cmd_export(a),
        Cmd::Stats(a) => {
            let coll = load_collection(&a.source);
            let queries = load_queries(&a.query, &coll, a.source.seed);
            println!("{}", posting_bench::stats::report(&coll, &queries));
        }
        Cmd::Codecs => {
            for e in bench::registry() {
                println!("{}", e.name);
            }
        }
    }
}
