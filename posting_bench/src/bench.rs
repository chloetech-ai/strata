//! Benchmark harness.
//!
//! For every codec: build all lists, measure space (bits per posting),
//! full-index decoding speed (ns per posting) and the mean latency of every
//! applicable AND / OR algorithm over a query workload. Every query result
//! of every algorithm is checked against a reference (length and an
//! order-sensitive hash) before it is timed; timings are the best of
//! `reps` passes over the whole workload.

use crate::algorithms::{cursor, multiway, pairwise, union};
use crate::codecs::{CursorList, PostingList, SetOps};
use crate::data::Collection;
use std::hint::black_box;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    And,
    Or,
}

impl Op {
    pub fn label(self) -> &'static str {
        match self {
            Op::And => "AND",
            Op::Or => "OR",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Measurement {
    pub op: Op,
    pub algorithm: String,
    pub ms_per_query: f64,
}

#[derive(Clone, Debug)]
pub struct CodecReport {
    pub codec: String,
    pub bits_per_posting: f64,
    /// Bits per posting over the lists that appear in queries.
    pub query_bits_per_posting: f64,
    pub build_secs: f64,
    /// Full decoding speed over every list of the collection.
    pub decode_ns_per_posting: f64,
    /// Full decoding speed over the lists that appear in queries.
    pub query_decode_ns_per_posting: f64,
    /// Memory derived at load time beyond the serialized size (bytes).
    pub aux_memory_bytes: u64,
    pub measurements: Vec<Measurement>,
}

impl CodecReport {
    /// Fastest algorithm for `op` that materialises docIDs (cardinality-only
    /// variants are excluded).
    pub fn best(&self, op: Op) -> Option<&Measurement> {
        self.measurements
            .iter()
            .filter(|m| m.op == op && !m.algorithm.contains("count only"))
            .min_by(|a, b| a.ms_per_query.total_cmp(&b.ms_per_query))
    }
}

#[derive(Clone, Debug)]
pub struct BenchConfig {
    pub reps: usize,
    pub and: bool,
    pub or: bool,
}

/// Length and order-sensitive hash of a result.
fn digest(out: &[u32]) -> (u64, u64) {
    let h = out.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &d| {
        (h ^ d as u64).wrapping_mul(0x0000_0100_0000_01b3)
    });
    (out.len() as u64, h)
}

/// A collection plus queries and their reference results.
pub struct Workload<'a> {
    pub coll: &'a Collection,
    pub queries: &'a [Vec<u32>],
    and_ref: Vec<(u64, u64)>,
    or_ref: Vec<(u64, u64)>,
    /// Distinct term ids used by the queries.
    pub query_terms: Vec<u32>,
}

impl<'a> Workload<'a> {
    pub fn new(coll: &'a Collection, queries: &'a [Vec<u32>]) -> Self {
        let (mut and_ref, mut or_ref) = (Vec::new(), Vec::new());
        let mut out = Vec::new();
        let mut scratch = Vec::new();
        for q in queries {
            let lists: Vec<&[u32]> = q.iter().map(|&t| coll.lists[t as usize].as_slice()).collect();
            out.clear();
            multiway::svs(&lists, pairwise::galloping, &mut out, &mut scratch);
            and_ref.push(digest(&out));
            out.clear();
            union::heap(&lists, &mut out);
            or_ref.push(digest(&out));
        }
        let mut query_terms: Vec<u32> = queries.iter().flatten().copied().collect();
        query_terms.sort_unstable();
        query_terms.dedup();
        Self { coll, queries, and_ref, or_ref, query_terms }
    }

    pub fn mean_result_size(&self, op: Op) -> f64 {
        let r = if op == Op::And { &self.and_ref } else { &self.or_ref };
        r.iter().map(|x| x.0 as f64).sum::<f64>() / r.len().max(1) as f64
    }

    /// Mean number of postings in the lists of a query.
    pub fn mean_query_postings(&self) -> f64 {
        let total: usize = self.queries.iter().flatten().map(|&t| self.coll.lists[t as usize].len()).sum();
        total as f64 / self.queries.len().max(1) as f64
    }

    pub fn mean_terms(&self) -> f64 {
        self.queries.iter().map(|q| q.len()).sum::<usize>() as f64 / self.queries.len().max(1) as f64
    }
}

/// Verify `f` on every query, then time `reps` passes and keep the best.
fn time_queries(
    w: &Workload,
    cfg: &BenchConfig,
    op: Op,
    codec: &str,
    algorithm: &str,
    mut f: impl FnMut(&[u32], &mut Vec<u32>),
) -> Measurement {
    let expected = if op == Op::And { &w.and_ref } else { &w.or_ref };
    let mut out = Vec::new();
    for (i, q) in w.queries.iter().enumerate() {
        out.clear();
        f(q, &mut out);
        let got = digest(&out);
        assert!(
            got == expected[i],
            "{codec} / {algorithm}: wrong {} result for query #{i} {q:?}: {} docs, expected {}",
            op.label(),
            got.0,
            expected[i].0
        );
    }
    let mut best = f64::INFINITY;
    for _ in 0..cfg.reps.max(1) {
        let t = Instant::now();
        for q in w.queries {
            out.clear();
            f(q, &mut out);
            black_box(&out);
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    Measurement {
        op,
        algorithm: algorithm.to_string(),
        ms_per_query: best * 1e3 / w.queries.len().max(1) as f64,
    }
}

/// Like [`time_queries`] for operations that only return a cardinality.
fn time_queries_count(
    w: &Workload,
    cfg: &BenchConfig,
    op: Op,
    codec: &str,
    algorithm: &str,
    mut f: impl FnMut(&[u32]) -> u64,
) -> Measurement {
    let expected = if op == Op::And { &w.and_ref } else { &w.or_ref };
    for (i, q) in w.queries.iter().enumerate() {
        let got = f(q);
        assert!(got == expected[i].0, "{codec} / {algorithm}: wrong {} count for query #{i}", op.label());
    }
    let mut best = f64::INFINITY;
    for _ in 0..cfg.reps.max(1) {
        let t = Instant::now();
        for q in w.queries {
            black_box(f(q));
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    Measurement {
        op,
        algorithm: algorithm.to_string(),
        ms_per_query: best * 1e3 / w.queries.len().max(1) as f64,
    }
}

/// Signature of a codec's algorithm suite.
pub type AlgoSuite<C> = fn(&[C], &Workload, &BenchConfig, &mut Vec<Measurement>);

/// Build a codec over the collection and run its algorithm suite.
pub fn report<C: PostingList>(w: &Workload, cfg: &BenchConfig, algos: AlgoSuite<C>) -> CodecReport {
    let t = Instant::now();
    let lists: Vec<C> = w.coll.lists.iter().map(|l| C::build(l, w.coll.universe)).collect();
    let build_secs = t.elapsed().as_secs_f64();

    let postings = w.coll.num_postings().max(1) as f64;
    let bits: u64 = lists.iter().map(|l| l.size_bits()).sum();
    let (qbits, qpostings) = w.query_terms.iter().fold((0u64, 0u64), |(b, p), &t| {
        (b + lists[t as usize].size_bits(), p + lists[t as usize].len() as u64)
    });

    let mut buf = Vec::with_capacity(w.coll.lists.iter().map(|l| l.len()).max().unwrap_or(0));
    let mut best = f64::INFINITY;
    for _ in 0..cfg.reps.max(1) {
        let t = Instant::now();
        for l in &lists {
            buf.clear();
            l.decode_into(&mut buf);
            black_box(&buf);
        }
        best = best.min(t.elapsed().as_secs_f64());
    }

    let mut qbest = f64::INFINITY;
    for _ in 0..cfg.reps.max(1) {
        let t = Instant::now();
        for &term in &w.query_terms {
            buf.clear();
            lists[term as usize].decode_into(&mut buf);
            black_box(&buf);
        }
        qbest = qbest.min(t.elapsed().as_secs_f64());
    }

    let mut measurements = Vec::new();
    algos(&lists, w, cfg, &mut measurements);
    CodecReport {
        codec: C::NAME.to_string(),
        bits_per_posting: bits as f64 / postings,
        query_bits_per_posting: qbits as f64 / qpostings.max(1) as f64,
        build_secs,
        decode_ns_per_posting: best * 1e9 / postings,
        query_decode_ns_per_posting: qbest * 1e9 / qpostings.max(1) as f64,
        aux_memory_bytes: lists.iter().map(|l| l.aux_memory_bytes()).sum::<u64>() + C::shared_aux_memory_bytes(),
        measurements,
    }
}

/// Cursor-based algorithms plus decode-then-intersect.
pub fn cursor_algos<C: CursorList>(lists: &[C], w: &Workload, cfg: &BenchConfig, m: &mut Vec<Measurement>) {
    let name = C::NAME;
    let cursors = |q: &[u32]| q.iter().map(|&t| lists[t as usize].cursor()).collect::<Vec<_>>();
    if cfg.and {
        m.push(time_queries(w, cfg, Op::And, name, "leapfrog (NextGEQ)", |q, out| {
            cursor::and_leapfrog(&mut cursors(q), out)
        }));
        m.push(time_queries(w, cfg, Op::And, name, "round-robin (LFTJ)", |q, out| {
            cursor::and_round_robin(&mut cursors(q), out)
        }));
        let mut bufs: Vec<Vec<u32>> = Vec::new();
        let mut scratch = Vec::new();
        m.push(time_queries(w, cfg, Op::And, name, "decode + SvS", |q, out| {
            bufs.resize_with(bufs.len().max(q.len()), Vec::new);
            for (b, &t) in bufs.iter_mut().zip(q) {
                b.clear();
                lists[t as usize].decode_into(b);
            }
            let refs: Vec<&[u32]> = bufs[..q.len()].iter().map(|b| b.as_slice()).collect();
            multiway::svs(&refs, pairwise::adaptive, out, &mut scratch);
        }));
    }
    if cfg.or {
        m.push(time_queries(w, cfg, Op::Or, name, "heap", |q, out| cursor::or_heap(&mut cursors(q), out)));
        m.push(time_queries(w, cfg, Op::Or, name, "min-scan", |q, out| {
            cursor::or_min_scan(&mut cursors(q), out)
        }));
        m.push(time_queries(w, cfg, Op::Or, name, "bitset window (4096)", |q, out| {
            cursor::or_bitset_window(&mut cursors(q), out)
        }));
    }
}

/// The codec's own set-operation algorithms.
pub fn setops_algos<C: SetOps>(lists: &[C], w: &Workload, cfg: &BenchConfig, m: &mut Vec<Measurement>) {
    let name = C::NAME;
    let refs = |q: &[u32]| q.iter().map(|&t| &lists[t as usize]).collect::<Vec<_>>();
    if cfg.and {
        m.push(time_queries(w, cfg, Op::And, name, "native", |q, out| C::intersect(&refs(q), out)));
    }
    if cfg.or {
        m.push(time_queries(w, cfg, Op::Or, name, "native", |q, out| C::union(&refs(q), out)));
    }
}

/// Algorithms over uncompressed slices (no decoding cost at all).
pub fn slice_algos(
    lists: &[crate::codecs::uncompressed::Uncompressed],
    w: &Workload,
    cfg: &BenchConfig,
    m: &mut Vec<Measurement>,
) {
    let name = "Uncompressed";
    let slices = |q: &[u32]| q.iter().map(|&t| lists[t as usize].as_slice()).collect::<Vec<_>>();
    let mut scratch = Vec::new();
    if cfg.and {
        for (label, kernel) in pairwise::kernels() {
            let algo = format!("SvS + {label}");
            m.push(time_queries(w, cfg, Op::And, name, &algo, |q, out| {
                multiway::svs(&slices(q), kernel, out, &mut scratch)
            }));
        }
        m.push(time_queries(w, cfg, Op::And, name, "Small Adaptive", |q, out| {
            multiway::small_adaptive(&slices(q), out)
        }));
        m.push(time_queries(w, cfg, Op::And, name, "Sequential (Barbay-Kenyon)", |q, out| {
            multiway::sequential(&slices(q), out)
        }));
    }
    if cfg.or {
        m.push(time_queries(w, cfg, Op::Or, name, "slices: heap", |q, out| union::heap(&slices(q), out)));
        m.push(time_queries(w, cfg, Op::Or, name, "slices: fold (pairwise merge)", |q, out| {
            union::fold(&slices(q), out)
        }));
        let mut words = Vec::new();
        m.push(time_queries(w, cfg, Op::Or, name, "slices: bitmap", |q, out| {
            union::bitmap(&slices(q), out, &mut words)
        }));
    }
}

/// `roaring` crate: also time cardinality-only operations (no docID
/// materialisation), as CRoaring benchmarks do.
pub fn roaring_rs_count_algos(
    lists: &[crate::codecs::external::RoaringRs],
    w: &Workload,
    cfg: &BenchConfig,
    m: &mut Vec<Measurement>,
) {
    use crate::codecs::external::RoaringRs;
    let name = RoaringRs::NAME;
    let refs = |q: &[u32]| q.iter().map(|&t| &lists[t as usize]).collect::<Vec<_>>();
    if cfg.and {
        m.push(time_queries_count(w, cfg, Op::And, name, "native, count only", |q| {
            RoaringRs::and_cardinality(&refs(q))
        }));
    }
    if cfg.or {
        m.push(time_queries_count(w, cfg, Op::Or, name, "native, count only", |q| {
            RoaringRs::or_cardinality(&refs(q))
        }));
    }
}

/// Our Roaring: cardinality-only operations returning Roaring results.
pub fn roaring_count_algos(
    lists: &[crate::codecs::roaring::Roaring],
    w: &Workload,
    cfg: &BenchConfig,
    m: &mut Vec<Measurement>,
) {
    use crate::codecs::roaring::Roaring;
    let name = Roaring::NAME;
    let refs = |q: &[u32]| q.iter().map(|&t| &lists[t as usize]).collect::<Vec<_>>();
    if cfg.and {
        m.push(time_queries_count(w, cfg, Op::And, name, "native, count only", |q| {
            Roaring::and_native(&refs(q)).cardinality()
        }));
    }
    if cfg.or {
        m.push(time_queries_count(w, cfg, Op::Or, name, "native, count only", |q| {
            Roaring::or_native(&refs(q)).cardinality()
        }));
    }
}

/// Loom: cardinality-only AND / OR (no docID materialisation).
pub fn loom_count_algos(lists: &[crate::codecs::loom::Loom], w: &Workload, cfg: &BenchConfig, m: &mut Vec<Measurement>) {
    use crate::codecs::loom::Loom;
    let name = Loom::NAME;
    let refs = |q: &[u32]| q.iter().map(|&t| &lists[t as usize]).collect::<Vec<_>>();
    if cfg.and {
        m.push(time_queries_count(w, cfg, Op::And, name, "native, count only", |q| Loom::and_count(&refs(q))));
    }
    if cfg.or {
        m.push(time_queries_count(w, cfg, Op::Or, name, "native, count only", |q| Loom::or_count(&refs(q))));
    }
}

/// Pyramid: cardinality-only intersection and union (popcounts, no docIDs).
pub fn pyramid_count_algos(
    lists: &[crate::codecs::pyramid::Pyramid],
    w: &Workload,
    cfg: &BenchConfig,
    m: &mut Vec<Measurement>,
) {
    use crate::codecs::pyramid::Pyramid;
    let name = Pyramid::NAME;
    let refs = |q: &[u32]| q.iter().map(|&t| &lists[t as usize]).collect::<Vec<_>>();
    if cfg.and {
        m.push(time_queries_count(w, cfg, Op::And, name, "native, count only", |q| Pyramid::and_count(&refs(q))));
    }
    if cfg.or {
        m.push(time_queries_count(w, cfg, Op::Or, name, "native, count only", |q| Pyramid::or_count(&refs(q))));
    }
}

/// A codec in the benchmark registry.
pub struct CodecEntry {
    pub name: &'static str,
    pub run: fn(&Workload, &BenchConfig) -> CodecReport,
}

/// Every codec the testbed knows, in report order.
pub fn registry() -> Vec<CodecEntry> {
    use crate::codecs::*;
    vec![
        CodecEntry {
            name: uncompressed::Uncompressed::NAME,
            run: |w, c| {
                report::<uncompressed::Uncompressed>(w, c, |l, w, c, m| {
                    slice_algos(l, w, c, m);
                    cursor_algos(l, w, c, m);
                })
            },
        },
        CodecEntry { name: vbyte::VByte::NAME, run: |w, c| report::<vbyte::VByte>(w, c, cursor_algos) },
        CodecEntry { name: bp128::Bp128::NAME, run: |w, c| report::<bp128::Bp128>(w, c, cursor_algos) },
        CodecEntry {
            name: external::TantivyBp128::NAME,
            run: |w, c| report::<external::TantivyBp128>(w, c, cursor_algos),
        },
        CodecEntry { name: pfor::OptPFor::NAME, run: |w, c| report::<pfor::OptPFor>(w, c, cursor_algos) },
        CodecEntry {
            name: elias_fano::EliasFano::NAME,
            run: |w, c| report::<elias_fano::EliasFano>(w, c, cursor_algos),
        },
        CodecEntry { name: pef::PartitionedEF::NAME, run: |w, c| report::<pef::PartitionedEF>(w, c, cursor_algos) },
        CodecEntry {
            name: loom::Loom::NAME,
            run: |w, c| {
                report::<loom::Loom>(w, c, |l, w, c, m| {
                    setops_algos(l, w, c, m);
                    loom_count_algos(l, w, c, m);
                    cursor_algos(l, w, c, m);
                })
            },
        },
        CodecEntry {
            name: interpolative::Interpolative::NAME,
            run: |w, c| report::<interpolative::Interpolative>(w, c, cursor_algos),
        },
        CodecEntry {
            name: entropic::Entropic::NAME,
            run: |w, c| {
                report::<entropic::Entropic>(w, c, |l, w, c, m| {
                    setops_algos(l, w, c, m);
                    cursor_algos(l, w, c, m);
                })
            },
        },
        CodecEntry {
            name: hybrid::Hybrid::NAME,
            run: |w, c| {
                report::<hybrid::Hybrid>(w, c, |l, w, c, m| {
                    setops_algos(l, w, c, m);
                    cursor_algos(l, w, c, m);
                })
            },
        },
        CodecEntry {
            name: roaring::Roaring::NAME,
            run: |w, c| {
                report::<roaring::Roaring>(w, c, |l, w, c, m| {
                    setops_algos(l, w, c, m);
                    roaring_count_algos(l, w, c, m);
                    cursor_algos(l, w, c, m);
                })
            },
        },
        CodecEntry {
            name: pyramid::Pyramid::NAME,
            run: |w, c| {
                report::<pyramid::Pyramid>(w, c, |l, w, c, m| {
                    setops_algos(l, w, c, m);
                    pyramid_count_algos(l, w, c, m);
                    cursor_algos(l, w, c, m);
                })
            },
        },
        CodecEntry {
            name: trie::Trie::NAME,
            run: |w, c| {
                report::<trie::Trie>(w, c, |l, w, c, m| {
                    setops_algos(l, w, c, m);
                    cursor_algos(l, w, c, m);
                })
            },
        },
        CodecEntry {
            name: trie::RunTrie::NAME,
            run: |w, c| {
                report::<trie::RunTrie>(w, c, |l, w, c, m| {
                    setops_algos(l, w, c, m);
                    cursor_algos(l, w, c, m);
                })
            },
        },
        CodecEntry {
            name: external::RoaringRs::NAME,
            run: |w, c| {
                report::<external::RoaringRs>(w, c, |l, w, c, m| {
                    setops_algos(l, w, c, m);
                    roaring_rs_count_algos(l, w, c, m);
                    cursor_algos(l, w, c, m);
                })
            },
        },
    ]
}

/// One pairwise-kernel timing.
#[derive(Clone, Debug)]
pub struct KernelRow {
    pub kernel: String,
    pub ratio: usize,
    pub small: usize,
    pub large: usize,
    pub micros_per_op: f64,
    pub ns_per_small_element: f64,
}

/// Time every pairwise kernel on random pairs with size ratios `ratios`
/// (`|large| = ratio · small`; the large list has density `density`).
pub fn bench_kernels(small: usize, ratios: &[usize], density: f64, seed: u64) -> Vec<KernelRow> {
    use rand::{rngs::StdRng, SeedableRng};
    let mut rng = StdRng::seed_from_u64(seed);
    let mut rows = Vec::new();
    for &ratio in ratios {
        let large = small * ratio;
        let universe = ((large as f64 / density) as u64).min(u32::MAX as u64 - 1) as u32;
        let pairs: Vec<(Vec<u32>, Vec<u32>)> = (0..4)
            .map(|_| {
                (
                    crate::data::uniform_sample(&mut rng, small, 0, universe),
                    crate::data::uniform_sample(&mut rng, large, 0, universe),
                )
            })
            .collect();
        let expected: Vec<Vec<u32>> = pairs
            .iter()
            .map(|(a, b)| {
                let mut o = Vec::new();
                pairwise::merge(a, b, &mut o);
                o
            })
            .collect();
        for (label, kernel) in pairwise::kernels() {
            let mut out = Vec::new();
            for ((a, b), e) in pairs.iter().zip(&expected) {
                out.clear();
                kernel(a, b, &mut out);
                assert!(&out == e, "kernel {label} wrong at ratio {ratio}");
            }
            // Repeat until at least 50 ms have elapsed; keep the best round.
            let mut best = f64::INFINITY;
            let mut iters = 1usize;
            let mut spent = 0.0;
            while spent < 0.05 {
                let t = Instant::now();
                for _ in 0..iters {
                    for (a, b) in &pairs {
                        out.clear();
                        kernel(black_box(a), black_box(b), &mut out);
                        black_box(&out);
                    }
                }
                let s = t.elapsed().as_secs_f64();
                spent += s;
                best = best.min(s / (iters * pairs.len()) as f64);
                iters *= 2;
            }
            rows.push(KernelRow {
                kernel: label.to_string(),
                ratio,
                small,
                large,
                micros_per_op: best * 1e6,
                ns_per_small_element: best * 1e9 / small as f64,
            });
        }
    }
    rows
}
