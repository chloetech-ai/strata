//! Intersection of two sorted, duplicate-free `u32` slices.
//!
//! With `m = |a| <= n = |b|`, any comparison-based algorithm needs about
//! `log2 C(n+m, m) ≈ m·log2(n/m)` comparisons (Hwang & Lin 1972):
//!
//! | algorithm      | comparisons              | best when            |
//! |----------------|--------------------------|----------------------|
//! | merge          | `m + n`                  | `n/m` small (< ~30)  |
//! | binary search  | `m·log2 n`               | `n/m` huge           |
//! | galloping      | `≤ 2m·log2(n/m) + O(m)`  | skewed sizes         |
//! | Baeza-Yates    | `O(m·log2(n/m))` average | uniform random data  |
//!
//! All functions append the intersection to `out` in increasing order.

use crate::codecs::uncompressed::gallop_geq;

/// Signature shared by all pairwise intersection kernels.
pub type PairwiseFn = fn(&[u32], &[u32], &mut Vec<u32>);

/// Classic two-pointer merge (branchy).
pub fn merge(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        if x < y {
            i += 1;
        } else if y < x {
            j += 1;
        } else {
            out.push(x);
            i += 1;
            j += 1;
        }
    }
}

/// Branch-free merge: pointer increments are computed from comparison
/// results, avoiding mispredictions on random data (Inoue et al., VLDB 2014).
pub fn merge_branchless(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    let start = out.len();
    out.resize(start + a.len().min(b.len()) + 1, 0);
    let dst = &mut out[start..];
    let (mut i, mut j, mut k) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        dst[k] = x;
        k += (x == y) as usize;
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    out.truncate(start + k);
}

/// For each element of `small`, gallop (exponential + binary search) from
/// the previous position in `large`.
pub fn galloping(small: &[u32], large: &[u32], out: &mut Vec<u32>) {
    let mut j = 0;
    for &x in small {
        j = gallop_geq(large, j, x);
        if j == large.len() {
            break;
        }
        if large[j] == x {
            out.push(x);
            j += 1;
        }
    }
}

/// For each element of `small`, binary search the remainder of `large`.
pub fn binary_search(small: &[u32], large: &[u32], out: &mut Vec<u32>) {
    let mut j = 0;
    for &x in small {
        j += large[j..].partition_point(|&y| y < x);
        if j == large.len() {
            break;
        }
        if large[j] == x {
            out.push(x);
            j += 1;
        }
    }
}

/// Baeza-Yates' divide and conquer (CPM 2004): binary-search the median of
/// the smaller list in the larger one and recurse on both sides.
pub fn baeza_yates(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if small.is_empty() || large.is_empty() {
        return;
    }
    if small.len() <= 4 {
        // Small base case: binary search is cheaper than further recursion.
        return binary_search(small, large, out);
    }
    let m = small.len() / 2;
    let x = small[m];
    let p = large.partition_point(|&y| y < x);
    baeza_yates(&small[..m], &large[..p], out);
    let found = p < large.len() && large[p] == x;
    if found {
        out.push(x);
    }
    let rest = if found { p + 1 } else { p };
    baeza_yates(&small[m + 1..], &large[rest..], out);
}

/// Size ratio above which [`adaptive`] switches from merging to galloping.
/// Lemire et al. report crossovers of 50:1; CRoaring hard-codes 64.
pub const GALLOP_RATIO: usize = 64;

/// Merge-style SIMD kernel for similar sizes, galloping for skewed sizes.
pub fn adaptive(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if small.len() * GALLOP_RATIO < large.len() {
        galloping(small, large, out)
    } else {
        super::simd::intersect(small, large, out)
    }
}

/// All pairwise kernels with display names (used by the kernel benchmark).
pub fn kernels() -> Vec<(&'static str, PairwiseFn)> {
    let mut v: Vec<(&'static str, PairwiseFn)> = vec![
        ("merge", merge),
        ("merge (branchless)", merge_branchless),
        ("galloping", galloping),
        ("binary search", binary_search),
        ("Baeza-Yates", baeza_yates),
    ];
    v.extend(super::simd::kernels());
    v.push(("adaptive (SIMD / gallop @64x)", adaptive));
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use std::collections::BTreeSet;

    pub fn random_set(rng: &mut StdRng, n: usize, universe: u32) -> Vec<u32> {
        let mut s = BTreeSet::new();
        while s.len() < n.min(universe as usize) {
            s.insert(rng.gen_range(0..universe));
        }
        s.into_iter().collect()
    }

    /// Check a kernel against `merge` on many size combinations.
    pub fn check_kernel(name: &str, f: PairwiseFn) {
        let mut rng = StdRng::seed_from_u64(11);
        for &(na, nb, u) in &[
            (0, 0, 10),
            (0, 5, 10),
            (1, 1, 2),
            (3, 100, 200),
            (17, 33, 60),
            (64, 64, 100),
            (100, 10_000, 50_000),
            (1000, 1000, 1500),
            (1000, 1000, 1_000_000),
            (5, 100_000, 200_000),
            (4095, 4097, 9000),
        ] {
            for _ in 0..5 {
                let a = random_set(&mut rng, na, u);
                let b = random_set(&mut rng, nb, u);
                let mut expected = Vec::new();
                merge(&a, &b, &mut expected);
                for (x, y) in [(&a, &b), (&b, &a)] {
                    let mut out = vec![7]; // results must be appended
                    f(x, y, &mut out);
                    assert_eq!(&out[1..], &expected[..], "{name}: |a|={}, |b|={}", x.len(), y.len());
                }
            }
        }
        // Identical inputs and the largest docIDs.
        let a: Vec<u32> = (u32::MAX - 100..u32::MAX).collect();
        let mut out = Vec::new();
        f(&a, &a, &mut out);
        assert_eq!(out, a, "{name}: identical inputs");
    }

    #[test]
    fn all_kernels_agree() {
        for (name, f) in kernels() {
            check_kernel(name, f);
        }
    }
}
