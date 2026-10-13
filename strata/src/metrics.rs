//! Retrieval metrics.

use std::collections::{HashMap, HashSet};

/// nDCG@k with graded gains (`gain = grade`, discount `log2(rank + 1)`), as
/// in BEIR / pytrec_eval. Documents missing from `judged` have gain 0.
pub fn ndcg_at_k(ranked: &[String], judged: &HashMap<String, u32>, k: usize) -> f64 {
    let dcg: f64 = ranked
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, d)| judged.get(d).copied().unwrap_or(0) as f64 / ((i + 2) as f64).log2())
        .sum();
    let mut ideal: Vec<u32> = judged.values().copied().filter(|&g| g > 0).collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal.iter().take(k).enumerate().map(|(i, &g)| g as f64 / ((i + 2) as f64).log2()).sum();
    if idcg == 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

/// Fraction of `truth` (the exact top-k) found in `found`.
pub fn recall<T: std::hash::Hash + Eq>(found: &[T], truth: &[T]) -> f64 {
    if truth.is_empty() {
        return 1.0;
    }
    let t: HashSet<&T> = truth.iter().collect();
    found.iter().filter(|x| t.contains(x)).count() as f64 / truth.len() as f64
}

pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

/// The `p`-th percentile (0..=100) by nearest rank.
pub fn percentile(xs: &[f64], p: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
    v[i.min(v.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndcg_perfect_and_empty() {
        let judged: HashMap<String, u32> = [("a".to_string(), 2), ("b".to_string(), 1)].into();
        let perfect = vec!["a".to_string(), "b".to_string()];
        assert!((ndcg_at_k(&perfect, &judged, 10) - 1.0).abs() < 1e-12);
        let swapped = vec!["b".to_string(), "a".to_string()];
        assert!(ndcg_at_k(&swapped, &judged, 10) < 1.0);
        assert_eq!(ndcg_at_k(&[], &judged, 10), 0.0);
        assert_eq!(recall(&[1, 2, 3], &[3, 4]), 0.5);
    }
}
