//! Intersection of `k` sorted, duplicate-free `u32` slices.
//!
//! * **SvS** (set-versus-set): sort by length, intersect the two smallest,
//!   then intersect the running result with each next list. The simplest
//!   algorithm and, per Barbay et al. (JEA 2009) and Culpepper & Moffat
//!   (TOIS 2010), the fastest in practice with galloping.
//! * **Small Adaptive** (Demaine, López-Ortiz & Munro, ALENEX 2001): take
//!   the next element of the currently smallest list and gallop for it in
//!   the others in increasing size order, re-sorting by remaining size.
//! * **Sequential** (Barbay & Kenyon, SODA 2002): a round-robin
//!   "eliminator" — gallop for the candidate in each list in turn; a miss
//!   promotes the successor found to new candidate. Its cost is
//!   `O(δ·Σ log(n_i/δ))` for alternation `δ`, which is optimal.

use super::pairwise::PairwiseFn;
use crate::codecs::uncompressed::gallop_geq;

/// SvS with the given pairwise kernel. `scratch` avoids reallocations.
pub fn svs(lists: &[&[u32]], kernel: PairwiseFn, out: &mut Vec<u32>, scratch: &mut Vec<u32>) {
    match lists.len() {
        0 => return,
        1 => return out.extend_from_slice(lists[0]),
        _ => {}
    }
    let mut order: Vec<&[u32]> = lists.to_vec();
    order.sort_by_key(|l| l.len());
    let mut acc = std::mem::take(scratch);
    acc.clear();
    kernel(order[0], order[1], &mut acc);
    let mut tmp = Vec::with_capacity(acc.len());
    for l in &order[2..] {
        if acc.is_empty() {
            break;
        }
        tmp.clear();
        kernel(&acc, l, &mut tmp);
        std::mem::swap(&mut acc, &mut tmp);
    }
    out.extend_from_slice(&acc);
    *scratch = acc;
}

/// Demaine et al.'s Small Adaptive.
pub fn small_adaptive(lists: &[&[u32]], out: &mut Vec<u32>) {
    let k = lists.len();
    if k == 0 {
        return;
    }
    let mut pos = vec![0usize; k];
    let mut order: Vec<usize> = (0..k).collect();
    loop {
        // Order lists by remaining length (insertion sort: k is small).
        for x in 1..k {
            let mut y = x;
            while y > 0 {
                let (p, q) = (order[y - 1], order[y]);
                if lists[p].len() - pos[p] <= lists[q].len() - pos[q] {
                    break;
                }
                order.swap(y - 1, y);
                y -= 1;
            }
        }
        let s = order[0];
        if pos[s] >= lists[s].len() {
            return;
        }
        let e = lists[s][pos[s]];
        let mut all = true;
        for &i in &order[1..] {
            pos[i] = gallop_geq(lists[i], pos[i], e);
            if pos[i] == lists[i].len() {
                return;
            }
            if lists[i][pos[i]] != e {
                all = false;
                break;
            }
        }
        if all {
            out.push(e);
            for &i in &order[1..] {
                pos[i] += 1;
            }
        }
        pos[s] += 1;
    }
}

/// Barbay & Kenyon's Sequential (round-robin eliminator).
pub fn sequential(lists: &[&[u32]], out: &mut Vec<u32>) {
    let k = lists.len();
    if k == 0 || lists.iter().any(|l| l.is_empty()) {
        return;
    }
    if k == 1 {
        return out.extend_from_slice(lists[0]);
    }
    let mut pos = vec![0usize; k];
    let mut e = lists[0][0];
    let mut matched = 1; // lists known to contain `e`
    let mut i = 1;
    loop {
        pos[i] = gallop_geq(lists[i], pos[i], e);
        if pos[i] == lists[i].len() {
            return;
        }
        let v = lists[i][pos[i]];
        if v == e {
            matched += 1;
            if matched == k {
                out.push(e);
                pos[i] += 1;
                if pos[i] == lists[i].len() {
                    return;
                }
                e = lists[i][pos[i]];
                matched = 1;
            }
        } else {
            e = v;
            matched = 1;
        }
        i = (i + 1) % k;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::pairwise::{self, tests::random_set};
    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn multiway_algorithms_agree() {
        let mut rng = StdRng::seed_from_u64(21);
        for _ in 0..300 {
            let k = rng.gen_range(1..=5);
            let u = *[50u32, 1000, 100_000].iter().nth(rng.gen_range(0..3)).unwrap();
            let lists: Vec<Vec<u32>> = (0..k)
                .map(|_| {
                    let n = rng.gen_range(0..(u as usize).min(20_000));
                    random_set(&mut rng, n, u)
                })
                .collect();
            let refs: Vec<&[u32]> = lists.iter().map(|l| l.as_slice()).collect();
            let mut expected = lists[0].clone();
            for l in &lists[1..] {
                expected.retain(|x| l.binary_search(x).is_ok());
            }
            let mut scratch = Vec::new();
            for kernel in [pairwise::galloping as PairwiseFn, pairwise::adaptive, pairwise::merge] {
                let mut out = Vec::new();
                svs(&refs, kernel, &mut out, &mut scratch);
                assert_eq!(out, expected, "svs");
            }
            let mut out = Vec::new();
            small_adaptive(&refs, &mut out);
            assert_eq!(out, expected, "small adaptive");
            out.clear();
            sequential(&refs, &mut out);
            assert_eq!(out, expected, "sequential");
        }
    }
}
