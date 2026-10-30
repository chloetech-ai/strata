//! Combining rankings: reciprocal rank fusion and weighted score fusion.
//!
//! Both work on candidates identified by a global key `(segment, docID)`.

use std::collections::HashMap;
use std::hash::Hash;

/// Reciprocal rank fusion: `score(d) = Σ_i 1 / (k + rank_i(d))` with ranks
/// from 1, over the given rankings (best first). Documents absent from a
/// ranking get nothing from it. Returns the fused ranking, best first; ties
/// are broken by the order of first appearance.
pub fn rrf<K: Copy + Eq + Hash>(rankings: &[Vec<K>], k: f64) -> Vec<(K, f64)> {
    let mut score: HashMap<K, (f64, usize)> = HashMap::new();
    let mut seen = 0usize;
    for r in rankings {
        for (i, &d) in r.iter().enumerate() {
            let e = score.entry(d).or_insert_with(|| {
                seen += 1;
                (0.0, seen)
            });
            e.0 += 1.0 / (k + (i + 1) as f64);
        }
    }
    let mut out: Vec<(K, f64, usize)> = score.into_iter().map(|(d, (s, o))| (d, s, o)).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
    out.into_iter().map(|(d, s, _)| (d, s)).collect()
}

/// How each input score is scaled before weighting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Norm {
    /// Raw scores.
    None,
    /// `(s − min) / (max − min)` over the candidates, per input (as in
    /// relative-score fusion); constant inputs map to 1.
    MinMax,
    /// Not a sum: the product of raw scores, each raised to its weight.
    Product,
}

/// Weighted fusion of exact scores: `scores[i][j]` is input `i`'s score of
/// candidate `j`. Returns `(candidate index, fused score)`, best first, as
/// [`combine`] fuses them.
pub fn weighted(weights: &[f64], scores: &[Vec<f32>], norm: Norm) -> Vec<(usize, f64)> {
    let scores: Vec<Vec<f64>> = scores.iter().map(|s| s.iter().map(|&x| f64::from(x)).collect()).collect();
    let mut out: Vec<(usize, f64)> = combine(weights, &scores, norm).into_iter().enumerate().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// The fused score of each candidate: `scores[i][j]` is input `i`'s score
/// of candidate `j`.
///
/// A score that is not finite (a missing column value arrives as −∞) is
/// missing, and a missing value is the input's worst: in a sum it counts as
/// the lowest score of the input over the candidates when its weight is
/// positive and as the highest when it is negative (after min-max, 0 or 1),
/// so a row without a value never gains from it; in a product it counts as
/// 0. Product factors below 0 count as 0 too, so that two negative factors
/// cannot multiply into a high score. Every fused score is finite.
pub fn combine<S: AsRef<[f64]>>(weights: &[f64], scores: &[S], norm: Norm) -> Vec<f64> {
    assert_eq!(weights.len(), scores.len());
    let n = scores.first().map_or(0, |s| s.as_ref().len());
    let mut fused = vec![if norm == Norm::Product { 1.0 } else { 0.0f64 }; n];
    for (&w, s) in weights.iter().zip(scores) {
        let s = s.as_ref();
        assert_eq!(s.len(), n);
        let (lo, hi) = s.iter().filter(|x| x.is_finite()).fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &x| (lo.min(x), hi.max(x)));
        // The worst value of this input, which missing values take.
        let worst = match norm {
            _ if lo > hi => 0.0,
            Norm::None if w < 0.0 => hi,
            Norm::None => lo,
            Norm::MinMax if w < 0.0 && hi > lo => 1.0,
            Norm::MinMax => 0.0,
            Norm::Product => 0.0,
        };
        for (f, &x) in fused.iter_mut().zip(s) {
            let v = match norm {
                _ if !x.is_finite() => worst,
                Norm::None => x,
                Norm::Product => x.max(0.0),
                Norm::MinMax if hi > lo => (x - lo) / (hi - lo),
                Norm::MinMax => 1.0,
            };
            if norm == Norm::Product {
                *f *= if w == 1.0 { v } else { v.powf(w) };
            } else {
                *f += w * v;
            }
        }
    }
    for f in &mut fused {
        // An overflow or 0^-w; the row ranks last.
        if !f.is_finite() {
            *f = f64::MIN;
        }
    }
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_rewards_agreement() {
        let a = vec![1, 2, 3];
        let b = vec![3, 1, 4];
        let f = rrf(&[a, b], 60.0);
        let ids: Vec<i32> = f.iter().map(|x| x.0).collect();
        assert_eq!(ids, vec![1, 3, 2, 4]);
        assert!((f[0].1 - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-12);
    }

    #[test]
    fn weighted_minmax_balances_scales() {
        // Input 0 ranges 0..30 (BM25-like), input 1 ranges 0.2..0.4 (cosine-like).
        let s0 = vec![30.0, 0.0, 15.0];
        let s1 = vec![0.2, 0.4, 0.35];
        let raw = weighted(&[0.5, 0.5], &[s0.clone(), s1.clone()], Norm::None);
        assert_eq!(raw[0].0, 0, "raw sums are dominated by the larger scale");
        let mm = weighted(&[0.5, 0.5], &[s0, s1], Norm::MinMax);
        assert_eq!(mm[0].0, 2, "after min-max, the candidate good on both wins");
        assert!((mm[0].1 - (0.5 * 0.5 + 0.5 * 0.75)).abs() < 1e-6);
    }

    #[test]
    fn missing_values_are_the_worst_of_their_input() {
        let ninf = f32::NEG_INFINITY;
        // Candidate 1 has no value of input 1 (a NULL column).
        let s0 = vec![1.0, 3.0, 2.0];
        let s1 = vec![10.0, ninf, 20.0];
        let mm = weighted(&[0.7, 0.3], &[s0.clone(), s1.clone()], Norm::MinMax);
        assert!(mm.iter().all(|(_, s)| s.is_finite()), "{mm:?}");
        // 0.7·1 + 0.3·0 for the row without a value; 0.7·0.5 + 0.3·1 for row 2.
        assert_eq!(mm.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 2, 0]);
        assert!((mm[0].1 - 0.7).abs() < 1e-9);
        // A negative weight: missing counts as the highest value, so the
        // row gains nothing over rows that have one.
        let neg = weighted(&[1.0, -0.1], &[s0.clone(), s1.clone()], Norm::None);
        let row1 = neg.iter().find(|x| x.0 == 1).unwrap().1;
        assert!((row1 - (3.0 - 0.1 * 20.0)).abs() < 1e-9, "{neg:?}");
        // Products: a missing factor is 0, negatives are 0, and nothing is NaN.
        let p = weighted(&[1.0, 1.0], &[vec![2.0, 3.0, -1.0], vec![ninf, 1.0, -5.0]], Norm::Product);
        assert_eq!(p, vec![(1, 3.0), (0, 0.0), (2, 0.0)]);
        // An input with no value at all.
        let none = weighted(&[0.5, 0.5], &[vec![1.0, 2.0], vec![ninf, ninf]], Norm::MinMax);
        assert_eq!(none.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 0]);
    }
}
