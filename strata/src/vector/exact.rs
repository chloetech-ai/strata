//! Exact float32 search: blocked all-pairs dot products, used by k-means
//! assignment, brute-force ground truth and tests.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use crate::segment::{DocFilter, DocId};
use crate::vector::kernels::dot_f32_4x4;
use crate::vector::Hit;

/// Calls `f(i, j, a[i]·b[j])` for every pair. `a` is processed in tiles of
/// `tile` rows that stay in cache while `b` streams past in groups of four,
/// so each `b` row is read from memory once per tile.
pub fn block_dots(a: &[&[f32]], b: &[&[f32]], tile: usize, mut f: impl FnMut(usize, usize, f32)) {
    let quad = |v: &[&[f32]]| -> [usize; 4] { [0, 1.min(v.len() - 1), 2.min(v.len() - 1), 3.min(v.len() - 1)] };
    for (t, at) in a.chunks(tile.max(4)).enumerate() {
        let a0 = t * tile.max(4);
        for (g, bg) in b.chunks(4).enumerate() {
            let bi = quad(bg);
            let bq = bi.map(|j| bg[j]);
            for (h, ab) in at.chunks(4).enumerate() {
                let ai = quad(ab);
                let m = dot_f32_4x4(ai.map(|i| ab[i]), bq);
                for (i, row) in m.iter().enumerate().take(ab.len()) {
                    for (j, &d) in row.iter().enumerate().take(bg.len()) {
                        f(a0 + h * 4 + i, g * 4 + j, d);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Entry(f32, DocId);

impl Eq for Entry {}

impl PartialOrd for Entry {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Entry {
    /// Better = larger score, then smaller docID.
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0).then(o.1.cmp(&self.1))
    }
}

/// Exact top-`k` of every query by float32 dot product, where `rows[d]` is
/// document `d`'s vector (all of the queries' length), optionally restricted
/// to a filter's members, on `threads` threads. Hits are best first, ties to
/// the smaller docID.
pub fn exact_top_k(rows: &[&[f32]], queries: &[&[f32]], k: usize, filter: Option<&dyn DocFilter>, threads: usize) -> Vec<Vec<Hit>> {
    let (ids, docs): (Vec<DocId>, Vec<&[f32]>) =
        rows.iter().enumerate().filter(|(d, _)| filter.is_none_or(|f| f.contains(*d as DocId))).map(|(d, r)| (d as DocId, *r)).unzip();
    let mut out: Vec<Vec<Hit>> = vec![Vec::new(); queries.len()];
    if k == 0 || queries.is_empty() {
        return out;
    }
    let per = queries.len().div_ceil(threads.max(1));
    std::thread::scope(|s| {
        for (qs, res) in queries.chunks(per).zip(out.chunks_mut(per)) {
            let (ids, docs) = (&ids, &docs);
            s.spawn(move || {
                let mut heaps: Vec<BinaryHeap<Reverse<Entry>>> = vec![BinaryHeap::with_capacity(k + 1); qs.len()];
                let mut floor = vec![f32::NEG_INFINITY; qs.len()];
                // A 256-query tile (1 MiB at 1024 dimensions) fits in L2.
                block_dots(qs, docs, 256, |i, j, d| {
                    if d < floor[i] {
                        return;
                    }
                    let h = &mut heaps[i];
                    let e = Entry(d, ids[j]);
                    if h.len() < k {
                        h.push(Reverse(e));
                    } else if e > h.peek().unwrap().0 {
                        h.pop();
                        h.push(Reverse(e));
                    }
                    if h.len() == k {
                        floor[i] = h.peek().unwrap().0 .0;
                    }
                });
                for (r, h) in res.iter_mut().zip(heaps) {
                    let mut v: Vec<Entry> = h.into_iter().map(|e| e.0).collect();
                    v.sort_unstable_by(|a, b| b.cmp(a));
                    *r = v.into_iter().map(|e| Hit { doc: e.1, score: e.0 }).collect();
                }
            });
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::DocSet;
    use crate::vector::kernels::dot_f32_scalar;
    use rand::{Rng, SeedableRng};

    #[test]
    fn exact_top_k_matches_naive() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(11);
        let dim = 21;
        let rows: Vec<Vec<f32>> = (0..203).map(|_| (0..dim).map(|_| rng.gen_range(-1.0..1.0)).collect()).collect();
        let queries: Vec<Vec<f32>> = (0..7).map(|_| (0..dim).map(|_| rng.gen_range(-1.0..1.0)).collect()).collect();
        let r: Vec<&[f32]> = rows.iter().map(|v| v.as_slice()).collect();
        let q: Vec<&[f32]> = queries.iter().map(|v| v.as_slice()).collect();
        let filter = DocSet::from_sorted((0..203).filter(|d| d % 3 != 0).collect(), 203);
        for (f, threads) in [(None, 1), (Some(&filter as &dyn DocFilter), 3)] {
            let got = exact_top_k(&r, &q, 9, f, threads);
            for (qi, qv) in queries.iter().enumerate() {
                let mut want: Vec<(f32, u32)> =
                    (0..203u32).filter(|&d| f.is_none_or(|f| f.contains(d))).map(|d| (dot_f32_scalar(qv, &rows[d as usize]), d)).collect();
                want.sort_by(|a, b| b.0.total_cmp(&a.0));
                let want: Vec<u32> = want.iter().take(9).map(|x| x.1).collect();
                let have: Vec<u32> = got[qi].iter().map(|h| h.doc).collect();
                assert_eq!(have, want);
            }
        }
    }
}
