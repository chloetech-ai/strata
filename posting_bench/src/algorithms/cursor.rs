//! Set operations over compressed lists through skip-capable cursors — how
//! search engines evaluate Boolean queries without decompressing whole
//! lists.
//!
//! * [`and_leapfrog`] — rarest list leads; every other cursor jumps with
//!   `next_geq` to the candidate; a mismatch makes the larger docID the new
//!   candidate (PISA `and_query`, Lucene `ConjunctionDISI`). Cost is
//!   dominated by `NextGEQ`, so it rewards codecs with cheap skips.
//! * [`and_round_robin`] — Leapfrog Triejoin's unary case (Veldhuizen,
//!   ICDT 2014): cursors cycle, each seeking to the current maximum.
//! * [`or_heap`] — `k`-way merge with a heap of cursors (Lucene's
//!   `DisjunctionDISIApproximation`): `O(N log k)` `next` calls.
//! * [`or_min_scan`] — linear scan for the minimum docID each step (PISA
//!   `or_query`): `O(N·k)`, but branch-friendly for small `k`.
//! * [`or_bitset_window`] — Lucene's `BooleanScorer` (Cutting 1997): drain
//!   every cursor into a 4096-docID bitset window, then scan the window.

use crate::codecs::Cursor;
use crate::END;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Rarest-first leapfrog conjunction.
pub fn and_leapfrog<C: Cursor>(cursors: &mut [C], out: &mut Vec<u32>) {
    let k = cursors.len();
    if k == 0 {
        return;
    }
    cursors.sort_by_key(|c| c.size());
    let mut candidate = cursors[0].docid();
    let mut i = 1;
    while candidate != END {
        while i < k {
            let d = cursors[i].next_geq(candidate);
            if d != candidate {
                candidate = d;
                i = 0;
                break;
            }
            i += 1;
        }
        if i == k {
            out.push(candidate);
            candidate = cursors[0].next();
            i = 1;
        } else if i == 0 && candidate != END {
            // Re-align the lead cursor, then re-check everyone else.
            let d = cursors[0].next_geq(candidate);
            candidate = d;
            i = 1;
        }
    }
}

/// Round-robin leapfrog (unary Leapfrog Triejoin).
pub fn and_round_robin<C: Cursor>(cursors: &mut [C], out: &mut Vec<u32>) {
    let k = cursors.len();
    if k == 0 || cursors.iter().any(|c| c.docid() == END) {
        return;
    }
    cursors.sort_by_key(|c| c.docid());
    let mut max = cursors[k - 1].docid();
    let mut p = 0;
    loop {
        let x = cursors[p].docid();
        let next = if x == max {
            out.push(x);
            cursors[p].next()
        } else {
            cursors[p].next_geq(max)
        };
        if next == END {
            return;
        }
        max = next;
        p = (p + 1) % k;
    }
}

/// Heap-based disjunction.
pub fn or_heap<C: Cursor>(cursors: &mut [C], out: &mut Vec<u32>) {
    let mut heap: BinaryHeap<Reverse<(u32, usize)>> = cursors
        .iter()
        .enumerate()
        .filter(|(_, c)| c.docid() != END)
        .map(|(i, c)| Reverse((c.docid(), i)))
        .collect();
    let mut last = END;
    while let Some(mut top) = heap.peek_mut() {
        let Reverse((d, i)) = *top;
        if d != last {
            out.push(d);
            last = d;
        }
        let next = cursors[i].next();
        if next == END {
            std::collections::binary_heap::PeekMut::pop(top);
        } else {
            *top = Reverse((next, i)); // sift-down happens when `top` drops
        }
    }
}

/// Disjunction by scanning for the minimum docID at every step.
pub fn or_min_scan<C: Cursor>(cursors: &mut [C], out: &mut Vec<u32>) {
    let mut cur = cursors.iter().map(|c| c.docid()).min().unwrap_or(END);
    while cur != END {
        out.push(cur);
        let mut next = END;
        for c in cursors.iter_mut() {
            let mut d = c.docid();
            if d == cur {
                d = c.next();
            }
            next = next.min(d);
        }
        cur = next;
    }
}

/// Window size of [`or_bitset_window`] in docIDs (Lucene uses 4096).
pub const WINDOW: u32 = 4096;

/// Windowed bitset disjunction.
pub fn or_bitset_window<C: Cursor>(cursors: &mut [C], out: &mut Vec<u32>) {
    let mut bits = [0u64; (WINDOW / 64) as usize];
    let mut min = cursors.iter().map(|c| c.docid()).min().unwrap_or(END);
    while min != END {
        let start = min & !(WINDOW - 1);
        let end = start as u64 + WINDOW as u64;
        let mut next_min = END;
        for c in cursors.iter_mut() {
            let mut d = c.docid();
            // `END` itself falls inside the last window of the u32 range.
            while (d as u64) < end && d != END {
                let off = d - start;
                bits[(off / 64) as usize] |= 1u64 << (off % 64);
                d = c.next();
            }
            next_min = next_min.min(d);
        }
        for (wi, w) in bits.iter_mut().enumerate() {
            let mut x = *w;
            while x != 0 {
                out.push(start + wi as u32 * 64 + x.trailing_zeros());
                x &= x - 1;
            }
            *w = 0;
        }
        min = next_min;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithms::pairwise::tests::random_set;
    use crate::codecs::uncompressed::SliceCursor;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use std::collections::BTreeSet;

    #[test]
    fn cursor_algorithms_agree() {
        let mut rng = StdRng::seed_from_u64(41);
        for _ in 0..300 {
            let k = rng.gen_range(1..=5);
            let u = [100u32, 10_000, 1 << 22][rng.gen_range(0..3)];
            let mut lists: Vec<Vec<u32>> = (0..k)
                .map(|_| {
                    let n = rng.gen_range(0..(u as usize).min(30_000));
                    random_set(&mut rng, n, u)
                })
                .collect();
            if rng.gen_bool(0.1) {
                lists[0] = (u32::MAX - 5000..u32::MAX).collect();
            }
            let mut and_expected = lists[0].clone();
            for l in &lists[1..] {
                and_expected.retain(|x| l.binary_search(x).is_ok());
            }
            let or_expected: Vec<u32> =
                lists.iter().flatten().copied().collect::<BTreeSet<_>>().into_iter().collect();
            let fresh = || lists.iter().map(|l| SliceCursor::new(l)).collect::<Vec<_>>();

            let mut out = Vec::new();
            and_leapfrog(&mut fresh(), &mut out);
            assert_eq!(out, and_expected, "leapfrog");
            out.clear();
            and_round_robin(&mut fresh(), &mut out);
            assert_eq!(out, and_expected, "round robin");
            out.clear();
            or_heap(&mut fresh(), &mut out);
            assert_eq!(out, or_expected, "heap");
            out.clear();
            or_min_scan(&mut fresh(), &mut out);
            assert_eq!(out, or_expected, "min scan");
            out.clear();
            or_bitset_window(&mut fresh(), &mut out);
            assert_eq!(out, or_expected, "bitset window");
        }
    }
}
