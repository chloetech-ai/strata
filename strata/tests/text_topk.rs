//! BM25 ranking: Exhaustive, MaxScore and Block-Max WAND against a naive
//! BM25, with and without filters (driven and probed), exact re-scoring,
//! and statistics summed over segments.

mod text_common;

use rand::{rngs::StdRng, seq::SliceRandom, Rng, SeedableRng};
use strata::segment::{DocFilter, DocId, DocSet};
use strata::text::{analyze, Bm25Params, CollectionStats, FilterStrategy, TextFieldReader, TopKAlgo};
use text_common::*;

const ALGOS: [TopKAlgo; 3] = [TopKAlgo::Exhaustive, TopKAlgo::MaxScore, TopKAlgo::BlockMaxWand];

/// A probe-only filter.
struct Every(u32);

impl DocFilter for Every {
    fn contains(&self, d: DocId) -> bool {
        d.is_multiple_of(self.0)
    }
    fn cardinality(&self) -> usize {
        1 << 30
    }
}

fn queries(texts: &[String], naive: &Naive, n: usize, seed: u64) -> Vec<Vec<String>> {
    let mut rng = StdRng::seed_from_u64(seed);
    let vocab: Vec<&String> = naive.index.keys().collect();
    (0..n)
        .map(|_| {
            let mut q: Vec<String> = match rng.gen_range(0..3) {
                // Terms of a document (they co-occur).
                0 => {
                    let toks = analyze(&texts[rng.gen_range(0..texts.len())]);
                    let m = rng.gen_range(1..=6);
                    toks.choose_multiple(&mut rng, m).cloned().collect()
                }
                _ => (0..rng.gen_range(1..=6)).map(|_| vocab.choose(&mut rng).unwrap().to_string()).collect(),
            };
            if rng.gen_bool(0.2) && !q.is_empty() {
                q.push(q[0].clone()); // a repeated term counts twice
            }
            if rng.gen_bool(0.1) {
                q.push("absentterm".into());
            }
            q
        })
        .collect()
}

fn check(texts: &[String], nq: usize, seed: u64) {
    let naive = Naive::new(texts);
    let qs = queries(texts, &naive, nq, seed);
    let n = texts.len() as u32;
    let mut rng = StdRng::seed_from_u64(seed + 7);
    let filters: Vec<DocSet> = [0.002, 0.05, 0.5]
        .iter()
        .map(|&p| DocSet::from_sorted((0..n).filter(|_| rng.gen_bool(p)).collect(), n))
        .collect();
    let total: u64 = naive.lens.iter().map(|&l| l as u64).sum();
    for (format, min_df) in configs() {
        let seg = build(texts, format, min_df, false);
        let r = TextFieldReader::open(&seg, "body").unwrap();
        for (qi, q) in qs.iter().enumerate() {
            let params = if qi % 5 == 4 { Bm25Params { k1: 1.2, b: 0.75 } } else { Bm25Params::default() };
            let stats = r.stats(q);
            let all = naive.bm25(q, n as u64, total, params.k1, params.b);
            let k = [1, 10, 100][qi % 3];
            for algo in ALGOS {
                let what = format!("{format:?}/{min_df} {algo:?} k={k} {q:?}");
                check_topk(&r.top_k(q, &stats, params, k, None, algo), &all, k, &what);
                for f in &filters {
                    let want: Vec<(u32, f64)> = all.iter().copied().filter(|&(d, _)| f.contains(d)).collect();
                    for strategy in [FilterStrategy::Auto, FilterStrategy::Drive, FilterStrategy::Probe] {
                        let got = r.top_k_with(q, &stats, params, k, Some(f), algo, strategy);
                        check_topk(&got, &want, k, &format!("{what} filter {} {strategy:?}", f.len()));
                    }
                }
                let every = Every(4);
                let want: Vec<(u32, f64)> = all.iter().copied().filter(|&(d, _)| every.contains(d)).collect();
                check_topk(&r.top_k(q, &stats, params, k, Some(&every), algo), &want, k, &format!("{what} probe-only filter"));
            }
            // Exact re-scoring, unsorted with duplicates and non-matching docs.
            let mut docs: Vec<u32> = all.iter().take(20).map(|x| x.0).collect();
            docs.extend((0..10).map(|_| rng.gen_range(0..n.max(1))));
            docs.shuffle(&mut rng);
            if n > 0 {
                let scores = r.score(q, &stats, params, &docs);
                for (&d, &s) in docs.iter().zip(&scores) {
                    let want = all.iter().find(|x| x.0 == d).map_or(0.0, |x| x.1);
                    assert!((s as f64 - want).abs() <= 1e-4 * want.max(1.0), "score of {d}: {s} vs {want}");
                }
            }
        }
    }
}

#[test]
fn algorithms_agree_synthetic() {
    check(&synthetic(31, 5000), 120, 1);
}

#[test]
fn algorithms_agree_scifact() {
    let Some(texts) = scifact(2000) else { return };
    check(&texts, 80, 2);
}

/// Two segments scored with summed statistics rank like one segment.
#[test]
fn statistics_summed_over_segments() {
    let texts = synthetic(41, 3000);
    let (a, b) = texts.split_at(1700);
    let whole = build(&texts, strata::text::PostingsFormat::Loom, 32, false);
    let parts = [build(a, strata::text::PostingsFormat::Loom, 32, false), build(b, strata::text::PostingsFormat::Bp128, 0, false)];
    let rw = TextFieldReader::open(&whole, "body").unwrap();
    let rp: Vec<TextFieldReader> = parts.iter().map(|s| TextFieldReader::open(s, "body").unwrap()).collect();
    let naive = Naive::new(&texts);
    for q in queries(&texts, &naive, 50, 9) {
        let mut stats = CollectionStats::default();
        for r in &rp {
            stats.add(&r.stats(&q));
        }
        assert_eq!(stats, rw.stats(&q));
        let p = Bm25Params::default();
        let want = rw.top_k(&q, &stats, p, 10, None, TopKAlgo::Exhaustive);
        let mut merged: Vec<(u32, f32)> = rp[0].top_k(&q, &stats, p, 10, None, TopKAlgo::BlockMaxWand);
        merged.extend(rp[1].top_k(&q, &stats, p, 10, None, TopKAlgo::MaxScore).into_iter().map(|(d, s)| (d + a.len() as u32, s)));
        merged.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
        merged.truncate(10);
        let all: Vec<(u32, f64)> = want.iter().map(|&(d, s)| (d, s as f64)).collect();
        check_topk(&merged, &all, all.len(), &format!("{q:?}"));
    }
}

#[test]
fn edge_cases() {
    let texts: Vec<String> = vec!["alpha beta".into(), "".into(), "beta beta gamma".into(), "the".into()];
    let seg = build(&texts, strata::text::PostingsFormat::Loom, 1, true);
    let r = TextFieldReader::open(&seg, "body").unwrap();
    let q: Vec<String> = vec!["beta".into()];
    let stats = r.stats(&q);
    for algo in ALGOS {
        assert!(r.top_k(&q, &stats, Bm25Params::default(), 0, None, algo).is_empty());
        assert!(r.top_k(&[], &stats, Bm25Params::default(), 10, None, algo).is_empty());
        assert!(r.top_k(&["nothing".into()], &stats, Bm25Params::default(), 10, None, algo).is_empty());
        let top = r.top_k(&q, &stats, Bm25Params::default(), 10, None, algo);
        assert_eq!(top.iter().map(|x| x.0).collect::<Vec<_>>(), [2, 0], "{algo:?}");
        let empty = DocSet::empty(4);
        assert!(r.top_k(&q, &stats, Bm25Params::default(), 10, Some(&empty), algo).is_empty());
    }
    assert_eq!(r.score(&q, &stats, Bm25Params::default(), &[1, 3, 0])[..2], [0.0, 0.0]);
}
