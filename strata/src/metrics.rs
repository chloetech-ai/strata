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

/// Recall@k against judgements: the share of the documents judged
/// relevant (grade above 0) among the first `k` of `ranked`; 0 when none
/// is relevant.
pub fn recall_at_k(ranked: &[String], judged: &HashMap<String, u32>, k: usize) -> f64 {
    let relevant = judged.values().filter(|&&g| g > 0).count();
    if relevant == 0 {
        return 0.0;
    }
    ranked.iter().take(k).filter(|d| judged.get(*d).is_some_and(|&g| g > 0)).count() as f64 / relevant as f64
}

/// Two-sided p-value of a paired randomization test of per-query scores
/// `a` against `b` (the sign-flip test Smucker, Allan and Carterette
/// recommend for IR, CIKM 2007): the share of `rounds` random sign flips of
/// the per-query differences whose mean is at least as far from 0 as the
/// observed mean. The same for the same `seed`.
pub fn paired_randomization(a: &[f64], b: &[f64], rounds: usize, seed: u64) -> f64 {
    assert_eq!(a.len(), b.len(), "paired scores");
    let d: Vec<f64> = a.iter().zip(b).map(|(x, y)| x - y).collect();
    let observed = d.iter().sum::<f64>().abs();
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut hits = 0usize;
    for _ in 0..rounds {
        let mut sum = 0.0;
        let mut bits = 0u64;
        for (i, &v) in d.iter().enumerate() {
            if i % 64 == 0 {
                // xorshift64
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                bits = x;
            }
            sum += if bits >> (i % 64) & 1 == 1 { v } else { -v };
        }
        // Up to rounding: identical scores give p = 1.
        if sum.abs() >= observed - 1e-9 {
            hits += 1;
        }
    }
    (hits + 1) as f64 / (rounds + 1) as f64
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
    v.sort_by(|a, b| a.total_cmp(b));
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

    #[test]
    fn recall_at_k_counts_relevant_documents() {
        let judged: HashMap<String, u32> = [("a".to_string(), 1), ("b".to_string(), 2), ("c".to_string(), 0)].into();
        let ranked: Vec<String> = ["c", "a", "x", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(recall_at_k(&ranked, &judged, 2), 0.5);
        assert_eq!(recall_at_k(&ranked, &judged, 10), 1.0);
        assert_eq!(recall_at_k(&ranked, &HashMap::new(), 10), 0.0);
    }

    #[test]
    fn paired_randomization_tells_a_shift_from_noise() {
        let base: Vec<f64> = (0..200).map(|i| (i * 37 % 100) as f64 / 100.0).collect();
        assert_eq!(paired_randomization(&base, &base, 1000, 1), 1.0);
        let better: Vec<f64> = base.iter().map(|x| x + 0.05).collect();
        assert!(paired_randomization(&better, &base, 1000, 1) < 0.01);
        // Differences of alternating sign: no shift.
        let noisy: Vec<f64> = base.iter().enumerate().map(|(i, x)| if i % 2 == 0 { x + 0.05 } else { x - 0.05 }).collect();
        assert!(paired_randomization(&noisy, &base, 1000, 1) > 0.5);
        assert_eq!(paired_randomization(&better, &base, 1000, 7), paired_randomization(&better, &base, 1000, 7));
    }
}
