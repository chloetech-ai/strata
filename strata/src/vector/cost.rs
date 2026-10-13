//! Cost model for vector search strategies.
//!
//! A strategy's latency is predicted in two steps:
//!
//! 1. [`Work`]: the operations one query performs (codes scored, float32
//!    re-ranks, filter checks, ...), predicted from the segment size, `k`,
//!    the filter's cardinality, the index shape and the search parameters.
//!    The same quantities are counted during search ([`SearchStats`]), so
//!    each prediction can be checked against what happened.
//! 2. [`CostModel`]: nanoseconds per operation, fitted by
//!    `strata-vector-bench` (non-negative least squares of measured latency
//!    on measured counts, relative error) on the development machine.
//!
//! Filtered predictions assume the filter is independent of the query
//! (uncorrelated). Filters correlated with the query make in-index
//! strategies cheaper than predicted, so the model errs towards the exact
//! pre-filter scan.

use serde::{Deserialize, Serialize};

use crate::vector::ivf::filtered_probes;
use crate::vector::{SearchParams, SearchStats, Strategy};

/// Operation counts of one query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Work {
    /// SQ8 codes scored in contiguous runs (flat scan, unfiltered IVF).
    pub scan: f64,
    /// SQ8 codes scored at scattered positions (pre-filter, filtered IVF,
    /// HNSW).
    pub gather: f64,
    /// Float32 re-rank dot products.
    pub rerank: f64,
    /// IVF centroids scored (float32).
    pub centroids: f64,
    /// IVF clusters probed.
    pub clusters: f64,
    /// Filter membership tests.
    pub checks: f64,
    /// HNSW nodes expanded.
    pub nodes: f64,
}

impl Work {
    /// The work a finished search reports.
    pub fn from_stats(s: &SearchStats) -> Work {
        Work {
            scan: s.sq8_contiguous as f64,
            gather: (s.sq8_distances - s.sq8_contiguous) as f64,
            rerank: s.f32_distances as f64,
            centroids: s.centroids as f64,
            clusters: s.clusters_probed as f64,
            checks: s.filter_checks as f64,
            nodes: s.nodes_visited as f64,
        }
    }

    /// The counts as a regression row, in [`CostModel::coefficients`] order.
    pub fn features(&self) -> [f64; 8] {
        [1.0, self.scan, self.gather, self.rerank, self.centroids, self.clusters, self.checks, self.nodes]
    }
}

/// Shape of a segment's vector index, as the cost model needs it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexShape {
    /// Documents with a vector.
    pub n: usize,
    /// `"flat"`, `"ivf"` or `"hnsw"`.
    pub kind: String,
    pub nlist: usize,
    pub hnsw_m: usize,
}

/// Nanoseconds per operation, and the HNSW work curve.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostModel {
    pub fixed_ns: f64,
    pub scan_ns: f64,
    pub gather_ns: f64,
    pub rerank_ns: f64,
    pub centroid_ns: f64,
    pub cluster_ns: f64,
    pub check_ns: f64,
    pub node_ns: f64,
    /// HNSW (M = 16) SQ8 distances per query ≈ `a·ef + b`; scaled by M/16.
    pub hnsw_dist_a: f64,
    pub hnsw_dist_b: f64,
    /// HNSW expansions per query ≈ `a·ef + b`.
    pub hnsw_nodes_a: f64,
    pub hnsw_nodes_b: f64,
    /// Post-filter fetches `k · margin / selectivity` unfiltered candidates.
    pub post_margin: f64,
}

impl Default for CostModel {
    /// Fitted by `strata-vector-bench` on TREC-COVID (171k × 1024 d) on the
    /// development machine (see `notes/vector.md`).
    fn default() -> Self {
        CostModel {
            fixed_ns: 0.0,
            scan_ns: 56.0,
            gather_ns: 34.0,
            rerank_ns: 620.0,
            centroid_ns: 39.0,
            cluster_ns: 0.0,
            check_ns: 2.8,
            node_ns: 590.0,
            hnsw_dist_a: 9.0,
            hnsw_dist_b: 252.0,
            hnsw_nodes_a: 0.99,
            hnsw_nodes_b: 2.5,
            post_margin: 2.0,
        }
    }
}

impl CostModel {
    /// Per-operation costs in [`Work::features`] order.
    pub fn coefficients(&self) -> [f64; 8] {
        [self.fixed_ns, self.scan_ns, self.gather_ns, self.rerank_ns, self.centroid_ns, self.cluster_ns, self.check_ns, self.node_ns]
    }

    /// Replaces the per-operation costs (e.g. with a fit).
    pub fn set_coefficients(&mut self, c: [f64; 8]) {
        [self.fixed_ns, self.scan_ns, self.gather_ns, self.rerank_ns, self.centroid_ns, self.cluster_ns, self.check_ns, self.node_ns] = c;
    }

    /// Predicted latency of `w` in microseconds.
    pub fn time_us(&self, w: &Work) -> f64 {
        w.features().iter().zip(self.coefficients()).map(|(x, c)| x * c).sum::<f64>() / 1000.0
    }

    fn hnsw(&self, ef: f64, m: usize) -> (f64, f64) {
        let scale = m as f64 / 16.0;
        ((self.hnsw_dist_a * ef + self.hnsw_dist_b) * scale, self.hnsw_nodes_a * ef + self.hnsw_nodes_b)
    }

    /// Predicted work of `strategy` (never `Auto`) on `shape` for a top-`k`
    /// query with a filter of `card` members (`None`: unfiltered).
    pub fn work(&self, shape: &IndexShape, strategy: Strategy, k: usize, card: Option<usize>, p: &SearchParams) -> Work {
        let n = shape.n.max(1) as f64;
        let r = p.rerank.max(k) as f64;
        let c = card.map_or(n, |c| (c as f64).min(n));
        let s = (c / n).max(1.0 / n);
        let avg = n / shape.nlist.max(1) as f64;
        let filtered = card.is_some();
        let mut w = Work::default();
        match resolve(shape, strategy, filtered) {
            Strategy::Flat => {
                w.scan = n;
                w.rerank = r.min(n);
            }
            Strategy::PreFilter => {
                w.gather = c;
                w.rerank = r.min(c);
            }
            Strategy::Ivf if !filtered => {
                let probes = (p.nprobe.max(1) as f64).max((r / avg).ceil()).min(shape.nlist as f64);
                w.centroids = shape.nlist as f64;
                w.clusters = probes;
                w.scan = probes * avg;
                w.rerank = r;
            }
            Strategy::Ivf => {
                let min = filtered_probes(p.nprobe.max(1), s, shape.nlist) as f64;
                let probes = min.max((r / (s * avg)).ceil()).min(shape.nlist as f64);
                w.centroids = shape.nlist as f64;
                w.clusters = probes;
                w.checks = probes * avg;
                w.gather = probes * avg * s;
                w.rerank = r.min(c);
            }
            Strategy::Hnsw => {
                // Filtered, the traversal must reach ef members: about ef/s nodes.
                let (d, nodes) = self.hnsw(p.ef.max(r as usize) as f64, shape.hnsw_m);
                let grow = if filtered { 1.0 / s } else { 1.0 };
                w.gather = (d * grow).min(n);
                w.nodes = (nodes * grow).min(n);
                w.checks = if filtered { w.gather } else { 0.0 };
                w.rerank = r.min(c);
            }
            Strategy::PostFilter => {
                let kp = (k as f64 * self.post_margin / s).ceil().max(r).min(n);
                let inner = SearchParams { rerank: kp as usize, ef: p.ef.max(kp as usize), nprobe: post_filter_nprobe(p.nprobe, kp as usize, r as usize), ..*p };
                w = self.work(shape, resolve(shape, Strategy::PostFilter, false), k, None, &inner);
                w.checks += kp;
                w.rerank = r.min(c);
            }
            Strategy::Auto => unreachable!("resolve Auto before estimating"),
        }
        w
    }

    /// Predicted latency (µs) of `strategy` (not `Auto`).
    pub fn estimate(&self, shape: &IndexShape, strategy: Strategy, k: usize, card: Option<usize>, p: &SearchParams) -> f64 {
        self.time_us(&self.work(shape, strategy, k, card, p))
    }

    /// The strategies applicable to `shape` with or without a filter.
    pub fn candidates(shape: &IndexShape, filtered: bool) -> Vec<Strategy> {
        let native = match shape.kind.as_str() {
            "ivf" => Some(Strategy::Ivf),
            "hnsw" => Some(Strategy::Hnsw),
            _ => None,
        };
        let mut v = vec![if filtered { Strategy::PreFilter } else { Strategy::Flat }];
        if let Some(s) = native {
            v.push(s);
            if filtered {
                v.push(Strategy::PostFilter);
            }
        }
        v
    }

    /// The cheapest applicable strategy and its predicted latency (µs).
    pub fn choose(&self, shape: &IndexShape, k: usize, card: Option<usize>, p: &SearchParams) -> (Strategy, f64) {
        Self::candidates(shape, card.is_some())
            .into_iter()
            .map(|s| (s, self.estimate(shape, s, k, card, p)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .expect("at least one strategy")
    }
}

/// The strategy that actually runs when `s` is requested: `Flat` with a
/// filter is the pre-filter scan and vice versa, an index strategy without
/// that index falls back to the scan, and an unfiltered post-filter search is
/// the plain index search. `Auto` is left to [`CostModel::choose`].
pub fn resolve(shape: &IndexShape, s: Strategy, filtered: bool) -> Strategy {
    let scan = if filtered { Strategy::PreFilter } else { Strategy::Flat };
    let native = match shape.kind.as_str() {
        "ivf" => Strategy::Ivf,
        "hnsw" => Strategy::Hnsw,
        _ => scan,
    };
    match s {
        Strategy::Auto => Strategy::Auto,
        Strategy::Flat | Strategy::PreFilter => scan,
        Strategy::Ivf if shape.kind == "ivf" => Strategy::Ivf,
        Strategy::Hnsw if shape.kind == "hnsw" => Strategy::Hnsw,
        Strategy::Ivf | Strategy::Hnsw => scan,
        Strategy::PostFilter if filtered => Strategy::PostFilter,
        Strategy::PostFilter => native,
    }
}

/// Clusters probed by an IVF post-filter search that needs `kp` candidates
/// where an unfiltered search needs `r`: probes scale with the candidates.
pub fn post_filter_nprobe(nprobe: usize, kp: usize, r: usize) -> usize {
    (nprobe.max(1) as f64 * kp as f64 / r.max(1) as f64).ceil() as usize
}
