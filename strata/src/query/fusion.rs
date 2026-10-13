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
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.2.cmp(&b.2)));
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
}

/// Weighted fusion of exact scores: `scores[i][j]` is input `i`'s score of
/// candidate `j`. Returns `(candidate index, fused score)`, best first.
pub fn weighted(weights: &[f64], scores: &[Vec<f32>], norm: Norm) -> Vec<(usize, f64)> {
    assert_eq!(weights.len(), scores.len());
    let n = scores.first().map_or(0, |s| s.len());
    let mut fused = vec![0.0f64; n];
    for (w, s) in weights.iter().zip(scores) {
        assert_eq!(s.len(), n);
        let (lo, hi) = s.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &x| (lo.min(x as f64), hi.max(x as f64)));
        for (f, &x) in fused.iter_mut().zip(s) {
            let v = match norm {
                Norm::None => x as f64,
                Norm::MinMax if hi > lo => (x as f64 - lo) / (hi - lo),
                Norm::MinMax => 1.0,
            };
            *f += w * v;
        }
    }
    let mut out: Vec<(usize, f64)> = fused.into_iter().enumerate().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    out
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
}
