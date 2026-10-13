//! Union of `k` sorted, duplicate-free `u32` slices.
//!
//! Union output is `Θ(N)` for `N = Σ n_i` in the worst case, so the
//! algorithms differ in constant factors and in how they scale with `k`:
//!
//! * **heap** — `k`-way merge with a binary heap: `O(N log k)`.
//! * **fold** — merge the lists two at a time, smallest first (the "naive
//!   fold" Roaring, Druid and Pinot use by default): `O(N·k)` worst case
//!   but sequential and cache friendly.
//! * **bitmap** — set bits in a bitmap spanning `[min, max]`, then scan
//!   it: `O(N + (max − min)/64)`; wins when the result is dense.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Two-way merge union.
pub fn merge2(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.reserve(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        out.push(x.min(y));
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
}

/// `k`-way merge with a binary heap.
pub fn heap(lists: &[&[u32]], out: &mut Vec<u32>) {
    let mut heap: BinaryHeap<Reverse<(u32, usize)>> = lists
        .iter()
        .enumerate()
        .filter_map(|(i, l)| l.first().map(|&d| Reverse((d, i))))
        .collect();
    let mut pos = vec![0usize; lists.len()];
    let mut last = None;
    while let Some(Reverse((d, i))) = heap.pop() {
        if last != Some(d) {
            out.push(d);
            last = Some(d);
        }
        pos[i] += 1;
        if let Some(&next) = lists[i].get(pos[i]) {
            heap.push(Reverse((next, i)));
        }
    }
}

/// Pairwise merges, smallest list first.
pub fn fold(lists: &[&[u32]], out: &mut Vec<u32>) {
    let mut order: Vec<&[u32]> = lists.to_vec();
    order.sort_by_key(|l| l.len());
    match order.len() {
        0 => return,
        1 => return out.extend_from_slice(order[0]),
        _ => {}
    }
    let mut acc = Vec::new();
    merge2(order[0], order[1], &mut acc);
    let mut tmp = Vec::new();
    for l in &order[2..] {
        tmp.clear();
        merge2(&acc, l, &mut tmp);
        std::mem::swap(&mut acc, &mut tmp);
    }
    out.extend_from_slice(&acc);
}

/// Append the positions of set bits in `words` (bit `i` = `base + i`).
#[inline]
pub fn extract_bits(words: &[u64], base: u32, out: &mut Vec<u32>) {
    for (wi, &w) in words.iter().enumerate() {
        let mut w = w;
        while w != 0 {
            out.push(base + (wi as u32) * 64 + w.trailing_zeros());
            w &= w - 1;
        }
    }
}

/// Bitmap accumulation over the range `[min, max]` of the inputs.
pub fn bitmap(lists: &[&[u32]], out: &mut Vec<u32>, scratch: &mut Vec<u64>) {
    let lo = lists.iter().filter_map(|l| l.first()).min();
    let hi = lists.iter().filter_map(|l| l.last()).max();
    let (Some(&lo), Some(&hi)) = (lo, hi) else { return };
    let base = lo & !63;
    let words = ((hi - base) / 64 + 1) as usize;
    scratch.clear();
    scratch.resize(words, 0);
    for l in lists {
        for &d in l.iter() {
            let off = d - base;
            scratch[(off / 64) as usize] |= 1u64 << (off % 64);
        }
    }
    extract_bits(scratch, base, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::pairwise::tests::random_set;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use std::collections::BTreeSet;

    #[test]
    fn union_algorithms_agree() {
        let mut rng = StdRng::seed_from_u64(31);
        let mut scratch = Vec::new();
        for _ in 0..200 {
            let k = rng.gen_range(1..=6);
            let u = [64u32, 5000, 1 << 24][rng.gen_range(0..3)];
            let lists: Vec<Vec<u32>> = (0..k)
                .map(|_| {
                    let n = rng.gen_range(0..(u as usize).min(5000));
                    random_set(&mut rng, n, u)
                })
                .collect();
            let refs: Vec<&[u32]> = lists.iter().map(|l| l.as_slice()).collect();
            let expected: Vec<u32> =
                lists.iter().flatten().copied().collect::<BTreeSet<_>>().into_iter().collect();
            let mut out = Vec::new();
            heap(&refs, &mut out);
            assert_eq!(out, expected, "heap");
            out.clear();
            fold(&refs, &mut out);
            assert_eq!(out, expected, "fold");
            out.clear();
            bitmap(&refs, &mut out, &mut scratch);
            assert_eq!(out, expected, "bitmap");
        }
        let top: Vec<u32> = (u32::MAX - 70..u32::MAX).collect();
        let mut out = Vec::new();
        bitmap(&[&top, &top[3..9]], &mut out, &mut scratch);
        assert_eq!(out, top);
    }
}
