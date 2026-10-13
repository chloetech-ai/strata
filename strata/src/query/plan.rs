//! Cost-based planning (DESIGN.md §7).
//!
//! The planner works per segment from statistics the indexes answer cheaply
//! ([`Estimator`]), and makes three decisions:
//!
//! 1. **Filter mode.** A predicate is *materialised* into a [`DocSet`] before
//!    ranking, or *probed* per candidate. With `m` candidates from the
//!    ranking driver, materialising wins when
//!    `C_mat(P) < m · c_probe(P)`.
//! 2. **Range strategy** per numeric leaf: column scan with zone maps vs the
//!    union of histogram-bucket postings.
//! 3. **Vector strategy**: pre-filter, IVF/HNSW with the filter applied in
//!    the index, or post-filter, by the vector index's own cost model plus
//!    the cost of the filter mode each strategy needs.
//!
//! [`DocSet`]: crate::segment::DocSet

use crate::query::logical::Pred;
use crate::value::Value;

/// Per-segment statistics, answered from index metadata without scanning.
pub trait Estimator {
    fn n_docs(&self) -> f64;
    /// Documents matching `MATCH(field, query)` (exact for single terms).
    fn match_count(&self, field: usize, query: &str) -> f64;
    /// Total postings read to materialise `MATCH(field, query)`.
    fn match_postings(&self, field: usize, query: &str) -> f64;
    /// Documents with `lo <= field <= hi` (histogram estimate).
    fn range_count(&self, field: usize, lo: f64, hi: f64) -> f64;
    /// Documents whose keyword field equals one of `values` (exact).
    fn in_count(&self, field: usize, values: &[Value]) -> f64;
    /// Nanoseconds to materialise a range, if the index can estimate it
    /// (otherwise the planner's own scan-vs-postings formula is used).
    fn range_cost(&self, _field: usize, _lo: f64, _hi: f64) -> Option<f64> {
        None
    }
    /// Estimated microseconds of a vector search strategy for `k` results,
    /// given the filter's cardinality (`None` = unfiltered).
    fn vector_cost(&self, field: usize, strategy: VectorStrategy, k: usize, filter_card: Option<f64>) -> f64;
    /// Documents of this segment in the result of graph computation `id`
    /// (computed before planning, so exact).
    fn graph_count(&self, _id: usize) -> f64 {
        self.n_docs()
    }
}

/// Unit costs in nanoseconds, calibrated from the component benchmarks.
#[derive(Clone, Debug)]
pub struct CostModel {
    /// Reading one posting while materialising a list (decode + union).
    pub posting: f64,
    /// Scanning one document's value in a numeric column (with zone maps,
    /// before pruning).
    pub column_scan: f64,
    /// Adding one member to a materialised set.
    pub emit: f64,
    /// Probing one candidate against a numeric column.
    pub probe_column: f64,
    /// Probing one candidate against a keyword value list.
    pub probe_keyword: f64,
    /// Probing one candidate against a text match (posting seeks).
    pub probe_match: f64,
    /// Fixed cost of evaluating any leaf.
    pub leaf: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        // Probe costs from the forward-index measurements (column range
        // 7–11 ns, code-set 6–9 ns per candidate); scan 0.35 ns per document.
        CostModel { posting: 1.0, column_scan: 0.35, emit: 1.0, probe_column: 9.0, probe_keyword: 8.0, probe_match: 40.0, leaf: 500.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeStrategy {
    Scan,
    Postings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterMode {
    /// No predicate: every live document qualifies.
    None,
    /// Evaluate the predicate into a document set first.
    Materialize,
    /// Check each candidate of the ranking driver.
    Probe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VectorStrategy {
    /// Exact scan of the filter's members (needs a materialised filter).
    PreFilter,
    /// The index (IVF or HNSW, whichever the field has) with the filter
    /// applied during the search.
    InIndex,
    /// Unfiltered index search with a larger k, then filtering.
    PostFilter,
    /// Exact scan of every document (no index).
    Flat,
}

/// Cardinality and costs of a predicate in one segment.
#[derive(Clone, Debug)]
pub struct PredEstimate {
    /// Expected matching documents.
    pub card: f64,
    /// Nanoseconds to materialise.
    pub mat_cost: f64,
    /// Expected nanoseconds to probe one candidate.
    pub probe_cost: f64,
    /// Whether every leaf can be probed per document (`MATCH` with phrases or
    /// prefixes is materialised only).
    pub probeable: bool,
}

pub struct Planner<'a, E: Estimator> {
    pub est: &'a E,
    pub costs: &'a CostModel,
}

impl<E: Estimator> Planner<'_, E> {
    /// Estimates a predicate. Conjunctions assume independence and probe the
    /// most selective child first; disjunctions use inclusion-exclusion.
    pub fn estimate(&self, p: &Pred) -> PredEstimate {
        let n = self.est.n_docs().max(1.0);
        let c = self.costs;
        match p {
            Pred::True => PredEstimate { card: n, mat_cost: n * c.emit / 64.0, probe_cost: 0.0, probeable: true },
            Pred::False => PredEstimate { card: 0.0, mat_cost: 0.0, probe_cost: 0.0, probeable: true },
            Pred::Match { field, query } => {
                let card = self.est.match_count(*field, query).min(n);
                let postings = self.est.match_postings(*field, query);
                let simple = !query.contains('"') && !query.contains('*');
                PredEstimate { card, mat_cost: c.leaf + postings * c.posting + card * c.emit, probe_cost: c.probe_match, probeable: simple }
            }
            Pred::Range { field, lo, hi } => {
                let card = self.est.range_count(*field, *lo, *hi).min(n);
                let cost = self.est.range_cost(*field, *lo, *hi).unwrap_or_else(|| self.range_strategy(n, card).1);
                PredEstimate { card, mat_cost: c.leaf + cost, probe_cost: c.probe_column, probeable: true }
            }
            Pred::In { field, values } => {
                let card = self.est.in_count(*field, values).min(n);
                PredEstimate {
                    card,
                    mat_cost: c.leaf * values.len() as f64 + card * (c.posting + c.emit),
                    probe_cost: c.probe_keyword * values.len().min(4) as f64,
                    probeable: true,
                }
            }
            Pred::Graph { id } => {
                // Already computed: materialising is a copy, probing a lookup.
                let card = self.est.graph_count(*id).min(n);
                PredEstimate { card, mat_cost: c.leaf + card * c.emit, probe_cost: c.probe_keyword, probeable: true }
            }
            Pred::Not(inner) => {
                let e = self.estimate(inner);
                PredEstimate { card: n - e.card, mat_cost: e.mat_cost + n * c.emit / 64.0, probe_cost: e.probe_cost, probeable: e.probeable }
            }
            Pred::And(ps) => {
                let mut es: Vec<PredEstimate> = ps.iter().map(|q| self.estimate(q)).collect();
                es.sort_by(|a, b| a.card.partial_cmp(&b.card).unwrap());
                let card = es.iter().fold(n, |acc, e| acc * e.card / n);
                // Probing short-circuits: child i runs on the survivors of 0..i.
                let mut pass = 1.0;
                let mut probe_cost = 0.0;
                for e in &es {
                    probe_cost += pass * e.probe_cost;
                    pass *= e.card / n;
                }
                let mat_cost = es.iter().map(|e| e.mat_cost).sum::<f64>() + es.iter().skip(1).map(|e| e.card.min(es[0].card)).sum::<f64>() * c.emit;
                PredEstimate { card, mat_cost, probe_cost, probeable: es.iter().all(|e| e.probeable) }
            }
            Pred::Or(ps) => {
                let es: Vec<PredEstimate> = ps.iter().map(|q| self.estimate(q)).collect();
                let miss = es.iter().fold(1.0, |acc, e| acc * (1.0 - e.card / n));
                let mut fail = 1.0;
                let mut probe_cost = 0.0;
                for e in &es {
                    probe_cost += fail * e.probe_cost;
                    fail *= 1.0 - e.card / n;
                }
                PredEstimate {
                    card: n * (1.0 - miss),
                    mat_cost: es.iter().map(|e| e.mat_cost + e.card * c.emit).sum(),
                    probe_cost,
                    probeable: es.iter().all(|e| e.probeable),
                }
            }
        }
    }

    /// Scan the column (cost ∝ n) or union the covering histogram buckets
    /// (cost ∝ matches, plus two boundary buckets checked against the column).
    pub fn range_strategy(&self, n: f64, card: f64) -> (RangeStrategy, f64) {
        let c = self.costs;
        let scan = n * c.column_scan + card * c.emit;
        let boundary = 2.0 * n / 64.0;
        let postings = card * (c.posting + c.emit) + boundary * (c.posting + c.probe_column);
        if postings < scan {
            (RangeStrategy::Postings, postings)
        } else {
            (RangeStrategy::Scan, scan)
        }
    }

    /// Filter mode for a ranking driver that tests `candidates` documents.
    pub fn filter_mode(&self, p: &Pred, candidates: f64) -> FilterMode {
        if *p == Pred::True {
            return FilterMode::None;
        }
        let e = self.estimate(p);
        if e.probeable && candidates * e.probe_cost < e.mat_cost {
            FilterMode::Probe
        } else {
            FilterMode::Materialize
        }
    }

    /// The cheapest vector strategy under filter `p`, with its total cost in
    /// microseconds and the filter mode it implies. `has_index` is false for
    /// fields without an ANN index.
    pub fn vector_plan(&self, field: usize, p: &Pred, k: usize, has_index: bool) -> (VectorStrategy, FilterMode, f64) {
        if *p == Pred::True {
            if !has_index {
                return (VectorStrategy::Flat, FilterMode::None, self.est.vector_cost(field, VectorStrategy::Flat, k, None));
            }
            return (VectorStrategy::InIndex, FilterMode::None, self.est.vector_cost(field, VectorStrategy::InIndex, k, None));
        }
        let e = self.estimate(p);
        let mat_us = e.mat_cost / 1000.0;
        let mut options = vec![(VectorStrategy::PreFilter, FilterMode::Materialize, mat_us + self.est.vector_cost(field, VectorStrategy::PreFilter, k, Some(e.card)))];
        if has_index {
            for s in [VectorStrategy::InIndex, VectorStrategy::PostFilter] {
                let search = self.est.vector_cost(field, s, k, Some(e.card));
                // In-index and post-filter only need membership tests; probing
                // avoids materialising when the filter is cheap per document.
                let tested = match s {
                    VectorStrategy::PostFilter => (k as f64) * self.est.n_docs() / e.card.max(1.0),
                    _ => self.est.n_docs().min(search * 50.0),
                };
                let probe_us = tested * e.probe_cost / 1000.0;
                let (mode, filt) = if e.probeable && probe_us < mat_us { (FilterMode::Probe, probe_us) } else { (FilterMode::Materialize, mat_us) };
                options.push((s, mode, search + filt));
            }
        }
        options.into_iter().min_by(|a, b| a.2.partial_cmp(&b.2).unwrap()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1M documents; field 0 is text, 1 numeric (uniform 0..1000), 2 keyword.
    struct Mock;
    impl Estimator for Mock {
        fn n_docs(&self) -> f64 {
            1e6
        }
        fn match_count(&self, _: usize, q: &str) -> f64 {
            if q == "rare" { 100.0 } else { 300_000.0 }
        }
        fn match_postings(&self, f: usize, q: &str) -> f64 {
            self.match_count(f, q)
        }
        fn range_count(&self, _: usize, lo: f64, hi: f64) -> f64 {
            ((hi.min(1000.0) - lo.max(0.0)).max(0.0) / 1000.0) * 1e6
        }
        fn in_count(&self, _: usize, values: &[Value]) -> f64 {
            values.len() as f64 * 10_000.0
        }
        fn vector_cost(&self, _: usize, s: VectorStrategy, _k: usize, card: Option<f64>) -> f64 {
            let sel = card.unwrap_or(1e6) / 1e6;
            match s {
                VectorStrategy::PreFilter => card.unwrap_or(1e6) * 0.05,
                VectorStrategy::InIndex => 400.0 / sel.max(0.01).sqrt(),
                VectorStrategy::PostFilter => 400.0 / sel.max(1e-4),
                VectorStrategy::Flat => 1e6 * 0.05,
            }
        }
    }

    fn planner() -> (Mock, CostModel) {
        (Mock, CostModel::default())
    }

    #[test]
    fn independence_and_inclusion_exclusion() {
        let (m, c) = planner();
        let p = Planner { est: &m, costs: &c };
        let a = Pred::Range { field: 1, lo: 0.0, hi: 99.0 }; // ~10%
        let b = Pred::In { field: 2, values: vec![Value::Str("x".into())] }; // 1%
        let and = p.estimate(&Pred::And(vec![a.clone(), b.clone()]));
        assert!((and.card - 1e6 * 0.099 * 0.01).abs() < 50.0);
        let or = p.estimate(&Pred::Or(vec![a, b]));
        assert!((or.card - 1e6 * (1.0 - 0.901 * 0.99)).abs() < 50.0);
    }

    #[test]
    fn selective_ranges_use_postings_and_wide_ones_scan() {
        let (m, c) = planner();
        let p = Planner { est: &m, costs: &c };
        assert_eq!(p.range_strategy(1e6, 1_000.0).0, RangeStrategy::Postings);
        assert_eq!(p.range_strategy(1e6, 500_000.0).0, RangeStrategy::Scan);
    }

    #[test]
    fn few_candidates_probe_many_materialise() {
        let (m, c) = planner();
        let p = Planner { est: &m, costs: &c };
        let f = Pred::Range { field: 1, lo: 0.0, hi: 499.0 };
        assert_eq!(p.filter_mode(&f, 100.0), FilterMode::Probe);
        assert_eq!(p.filter_mode(&f, 1e6), FilterMode::Materialize);
        assert_eq!(p.filter_mode(&Pred::Match { field: 0, query: "\"a b\"".into() }, 10.0), FilterMode::Materialize);
    }

    #[test]
    fn vector_strategy_follows_selectivity() {
        let (m, c) = planner();
        let p = Planner { est: &m, costs: &c };
        let sel = |hi: f64| Pred::Range { field: 1, lo: 0.0, hi };
        assert_eq!(p.vector_plan(3, &sel(0.5), 10, true).0, VectorStrategy::PreFilter); // 0.1%
        assert_eq!(p.vector_plan(3, &sel(999.0), 10, true).0, VectorStrategy::PostFilter); // 100%
        assert_eq!(p.vector_plan(3, &sel(299.0), 10, true).0, VectorStrategy::InIndex); // 30%
        assert_eq!(p.vector_plan(3, &Pred::True, 10, false).0, VectorStrategy::Flat);
    }
}
