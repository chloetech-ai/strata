//! HNSW: hierarchical navigable small-world graph (Malkov & Yashunin).
//!
//! **Construction** inserts documents in docID order with exact float32
//! similarities, the neighbour-selection heuristic, `M` links per node on the
//! upper layers and `2M` on layer 0. Several threads insert at once; each
//! node's link list has its own lock, as in hnswlib. Levels come from a
//! seeded hash of the docID, so the layer structure does not depend on the
//! thread count (the links do, slightly, through insertion races).
//!
//! **Layout** (sections `vec.<field>.hnsw` and `vec.<field>.hnsw_upper`):
//! layer 0 is `n × 2M` u32 neighbour ids, padded with `u32::MAX`, so a node's
//! list is at a fixed offset (128 bytes for M = 16, two cache lines). Each
//! upper layer `l` is a sorted array of the nodes at level ≥ l plus their
//! `n_l × M` links; a node's slot is found by binary search (upper layers
//! hold ~1/M of the nodes each and a search touches a handful of them).
//!
//! **Search** runs on SQ8 codes (1 KiB per 1024-d vector instead of 4 KiB) and
//! the caller re-ranks the best candidates with float32. A filter is applied
//! *in* the traversal, in one of two ways:
//!
//! * traverse (default): every reached node is scored and expanded as a
//!   stepping stone, only members enter the result set, and the search ends
//!   when the closest unexpanded candidate is worse than the `ef`-th best
//!   member. The graph stays connected for any filter; the work grows like
//!   `1/selectivity`.
//! * two-hop (ACORN-1): only members are scored and expanded; a node's
//!   non-member neighbours are looked through to their own neighbours. The
//!   work no longer grows with `1/selectivity`, but on a graph built with the
//!   ordinary `M` the members' two-hop graph falls apart below ~10%
//!   selectivity, or when the members lie away from the query, and recall
//!   collapses (ACORN builds a denser graph for this; see `notes/vector.md`).

use std::cmp::{Ordering as CmpOrdering, Reverse};
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use posting_bench::bytes::{ByteReader, ByteWriter, OwnedBytes, Slice};

use crate::error::{corrupt, Result};
use crate::segment::DocId;
use crate::vector::build::Rows;
use crate::vector::kernels::{self, dot_f32};
use crate::vector::sq8::QueryCode;
use crate::vector::topk::{Members, TopK};
use crate::vector::{io, Codes, SearchStats};

const NONE: u32 = u32::MAX;
const MAX_LEVEL: usize = 16;

/// Construction parameters.
#[derive(Clone, Debug)]
pub(crate) struct HnswConfig {
    pub m: usize,
    pub ef_construction: usize,
    pub threads: usize,
    pub seed: u64,
}

/// A built graph in its on-disk shape.
pub(crate) struct Graph {
    pub m: usize,
    pub entry: u32,
    pub max_level: usize,
    pub layer0: Vec<u32>,
    /// Per upper layer 1..=max_level: (sorted node ids, ids.len() × m links).
    pub upper: Vec<(Vec<u32>, Vec<u32>)>,
}

impl Graph {
    /// The `hnsw_upper` section.
    pub fn upper_bytes(&self) -> Vec<u8> {
        let mut w = ByteWriter::new();
        w.u64(self.upper.len() as u64);
        for (ids, adj) in &self.upper {
            w.array(ids);
            w.array(adj);
        }
        w.finish()
    }

    pub fn layer0_bytes(&self) -> Vec<u8> {
        self.layer0.iter().flat_map(|x| x.to_le_bytes()).collect()
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Scored(f32, u32);

impl Eq for Scored {}

impl PartialOrd for Scored {
    fn partial_cmp(&self, o: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(o))
    }
}

impl Ord for Scored {
    fn cmp(&self, o: &Self) -> CmpOrdering {
        self.0.total_cmp(&o.0).then(o.1.cmp(&self.1))
    }
}

/// Level of node `i`: floor(-ln(U) / ln(M)) with U from a seeded hash.
fn level_of(i: u32, seed: u64, m: usize) -> usize {
    let mut z = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let u = ((z >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
    ((-u.ln() / (m as f64).ln()).floor() as usize).min(MAX_LEVEL)
}

struct Builder<'a> {
    rows: &'a Rows<'a>,
    m: usize,
    m0: usize,
    ef: usize,
    levels: Vec<u8>,
    /// links[v][l]: node v's neighbours on layer l (empty for absent docs).
    links: Vec<Vec<Mutex<Vec<u32>>>>,
    entry: Mutex<(u32, usize)>,
    /// Held by an insert whose level exceeds the current top level.
    grow: Mutex<()>,
}

struct Scratch {
    visited: Vec<u32>,
    epoch: u32,
}

impl Scratch {
    fn new(n: usize) -> Self {
        Scratch { visited: vec![0; n], epoch: 0 }
    }
    fn next_epoch(&mut self) -> u32 {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.visited.fill(0);
            self.epoch = 1;
        }
        self.epoch
    }
}

impl Builder<'_> {
    fn row(&self, v: u32) -> &[f32] {
        self.rows.row(v as usize)
    }

    /// Vectors are 4 KiB at 1024 dimensions and scattered: callers start
    /// fetching the next one while scoring the current one.
    fn prefetch(&self, v: u32) {
        let r = self.row(v);
        kernels::prefetch(r.as_ptr() as *const u8, r.len() * 4);
    }

    fn neighbours(&self, v: u32, l: usize) -> Vec<u32> {
        self.links[v as usize][l].lock().unwrap().clone()
    }

    fn greedy(&self, q: &[f32], mut cur: Scored, l: usize) -> Scored {
        loop {
            let mut changed = false;
            for nb in self.neighbours(cur.1, l) {
                let s = dot_f32(q, self.row(nb));
                if s > cur.0 {
                    cur = Scored(s, nb);
                    changed = true;
                }
            }
            if !changed {
                return cur;
            }
        }
    }

    /// The `ef` nodes closest to `q` on layer `l` reachable from `eps`, best first.
    fn search_layer(&self, q: &[f32], eps: &[Scored], ef: usize, l: usize, sc: &mut Scratch) -> Vec<Scored> {
        let epoch = sc.next_epoch();
        let mut cand: BinaryHeap<Scored> = BinaryHeap::new();
        let mut res: BinaryHeap<Reverse<Scored>> = BinaryHeap::new();
        for &e in eps {
            sc.visited[e.1 as usize] = epoch;
            cand.push(e);
            res.push(Reverse(e));
        }
        while let Some(c) = cand.pop() {
            if res.len() >= ef && c < res.peek().unwrap().0 {
                break;
            }
            let mut fresh = self.neighbours(c.1, l);
            fresh.retain(|&nb| std::mem::replace(&mut sc.visited[nb as usize], epoch) != epoch);
            for (i, &nb) in fresh.iter().enumerate() {
                if let Some(&next) = fresh.get(i + 1) {
                    self.prefetch(next);
                }
                let s = Scored(dot_f32(q, self.row(nb)), nb);
                if res.len() < ef || s > res.peek().unwrap().0 {
                    cand.push(s);
                    res.push(Reverse(s));
                    if res.len() > ef {
                        res.pop();
                    }
                }
            }
        }
        let mut v: Vec<Scored> = res.into_iter().map(|r| r.0).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    }

    /// Keeps a candidate (best first) only if it is closer to the base than
    /// to every neighbour already kept, which spreads links across directions.
    fn select(&self, cands: &[Scored], m: usize) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::with_capacity(m);
        for (i, c) in cands.iter().enumerate() {
            if out.len() >= m {
                break;
            }
            if let Some(next) = cands.get(i + 1) {
                self.prefetch(next.1);
            }
            let cv = self.row(c.1);
            if out.iter().all(|&r| dot_f32(cv, self.row(r)) <= c.0) {
                out.push(c.1);
            }
        }
        out
    }

    fn connect(&self, e: u32, new: u32, l: usize) {
        let cap = if l == 0 { self.m0 } else { self.m };
        let mut g = self.links[e as usize][l].lock().unwrap();
        if g.len() < cap {
            g.push(new);
            return;
        }
        let ev = self.row(e);
        let ids: Vec<u32> = g.iter().copied().chain([new]).collect();
        let mut cands: Vec<Scored> = Vec::with_capacity(ids.len());
        for (i, &x) in ids.iter().enumerate() {
            if let Some(&next) = ids.get(i + 1) {
                self.prefetch(next);
            }
            cands.push(Scored(dot_f32(ev, self.row(x)), x));
        }
        cands.sort_unstable_by(|a, b| b.cmp(a));
        *g = self.select(&cands, cap);
    }

    fn insert(&self, i: u32, sc: &mut Scratch) {
        let q = self.row(i);
        let level = self.levels[i as usize] as usize;
        let (mut ep, mut top) = *self.entry.lock().unwrap();
        let _grow = if level > top {
            let g = self.grow.lock().unwrap();
            (ep, top) = *self.entry.lock().unwrap();
            Some(g)
        } else {
            None
        };
        let mut cur = Scored(dot_f32(q, self.row(ep)), ep);
        for l in (level + 1..=top).rev() {
            cur = self.greedy(q, cur, l);
        }
        let mut eps = vec![cur];
        for l in (0..=level.min(top)).rev() {
            let w = self.search_layer(q, &eps, self.ef, l, sc);
            let sel = self.select(&w, self.m);
            *self.links[i as usize][l].lock().unwrap() = sel.clone();
            for e in sel {
                self.connect(e, i, l);
            }
            eps = w;
        }
        if level > top {
            *self.entry.lock().unwrap() = (i, level);
        }
    }
}

/// Builds the graph over the present rows.
pub(crate) fn build(rows: &Rows, present: &(dyn Fn(u32) -> bool + Sync), cfg: &HnswConfig) -> Graph {
    let n = rows.n;
    let (m, m0) = (cfg.m.max(2), 2 * cfg.m.max(2));
    let levels: Vec<u8> = (0..n as u32).map(|i| if present(i) { level_of(i, cfg.seed, m) as u8 } else { 0 }).collect();
    let links = (0..n as u32)
        .map(|i| if present(i) { (0..=levels[i as usize]).map(|l| Mutex::new(Vec::with_capacity(if l == 0 { m0 + 1 } else { m + 1 }))).collect() } else { Vec::new() })
        .collect();
    let first = (0..n as u32).find(|&i| present(i));
    let Some(first) = first else {
        return Graph { m, entry: NONE, max_level: 0, layer0: vec![NONE; n * m0], upper: Vec::new() };
    };
    let b = Builder {
        rows,
        m,
        m0,
        ef: cfg.ef_construction.max(m),
        levels,
        links,
        entry: Mutex::new((first, 0)),
        grow: Mutex::new(()),
    };
    *b.entry.lock().unwrap() = (first, b.levels[first as usize] as usize);
    let next = AtomicUsize::new(first as usize + 1);
    std::thread::scope(|s| {
        for _ in 0..cfg.threads.max(1) {
            s.spawn(|| {
                let mut sc = Scratch::new(n);
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    if present(i as u32) {
                        b.insert(i as u32, &mut sc);
                    }
                }
            });
        }
    });
    let (entry, max_level) = *b.entry.lock().unwrap();
    let mut layer0 = vec![NONE; n * m0];
    for (v, l) in b.links.iter().enumerate() {
        if let Some(l0) = l.first() {
            let g = l0.lock().unwrap();
            layer0[v * m0..v * m0 + g.len()].copy_from_slice(&g);
        }
    }
    let mut upper = Vec::with_capacity(max_level);
    for l in 1..=max_level {
        let ids: Vec<u32> = (0..n as u32).filter(|&v| present(v) && b.levels[v as usize] as usize >= l).collect();
        let mut adj = vec![NONE; ids.len() * m];
        for (j, &v) in ids.iter().enumerate() {
            let g = b.links[v as usize][l].lock().unwrap();
            adj[j * m..j * m + g.len()].copy_from_slice(&g);
        }
        upper.push((ids, adj));
    }
    Graph { m, entry, max_level, layer0, upper }
}

/// Zero-copy view of a stored graph.
pub(crate) struct HnswView {
    m: usize,
    m0: usize,
    entry: u32,
    layer0: Slice<u32>,
    upper: Vec<(Slice<u32>, Slice<u32>)>,
}

impl HnswView {
    pub fn open(m: usize, entry: u32, layer0: OwnedBytes, upper: OwnedBytes, n: usize) -> Result<Self> {
        let m0 = 2 * m;
        let layer0: Slice<u32> = Slice::view(layer0).map_err(corrupt)?;
        if layer0.len() != n * m0 {
            return Err(corrupt("hnsw layer 0 has the wrong size"));
        }
        let mut r = ByteReader::new(upper);
        let levels = r.u64().map_err(corrupt)? as usize;
        let mut up = Vec::with_capacity(levels);
        for _ in 0..levels {
            let ids: Slice<u32> = r.array().map_err(corrupt)?;
            let adj: Slice<u32> = r.array().map_err(corrupt)?;
            if adj.len() != ids.len() * m {
                return Err(corrupt("hnsw upper layer has the wrong size"));
            }
            up.push((ids, adj));
        }
        if entry != NONE && entry as usize >= n {
            return Err(corrupt("hnsw entry point out of range"));
        }
        Ok(HnswView { m, m0, entry, layer0, upper: up })
    }

    pub fn is_shared(&self) -> bool {
        matches!(self.layer0, Slice::Shared { .. }) && self.upper.iter().all(|(a, b)| matches!(a, Slice::Shared { .. }) && matches!(b, Slice::Shared { .. }))
    }

    fn neighbours(&self, v: u32, l: usize) -> &[u32] {
        let s: &[u32] = if l == 0 {
            &self.layer0[v as usize * self.m0..(v as usize + 1) * self.m0]
        } else {
            let (ids, adj) = &self.upper[l - 1];
            match ids.binary_search(&v) {
                Ok(j) => &adj[j * self.m..(j + 1) * self.m],
                Err(_) => &[],
            }
        };
        &s[..s.iter().position(|&x| x == NONE).unwrap_or(s.len())]
    }

    /// The `ef` best members by SQ8 score, best first.
    ///
    /// With a filter, `two_hop` picks the traversal: without it every reached
    /// node is scored and expanded and only members are collected; with it
    /// (ACORN-1 style) only members are scored and expanded, and a node's
    /// non-member neighbours are looked through to their own neighbours, up
    /// to `2M` members per expansion. With `hints`, the rows about to be
    /// scored are announced to the kernel first.
    #[allow(clippy::too_many_arguments)]
    pub fn search(&self, codes: &Codes, q: &QueryCode, ef: usize, filter: Option<Members>, two_hop: bool, hints: bool, stats: &mut SearchStats) -> Vec<(i32, DocId)> {
        if self.entry == NONE || ef == 0 {
            return Vec::new();
        }
        let mut cur = self.entry;
        let mut cur_s = q.dot(codes.row(cur));
        stats.sq8_distances += 1;
        let announce = |ids: &[u32]| {
            if hints {
                io::will_need(&mut ids.iter().map(|&d| (codes.row(d).as_ptr() as usize, codes.stride())).collect::<Vec<_>>());
            }
        };
        for l in (1..=self.upper.len()).rev() {
            loop {
                let mut changed = false;
                announce(self.neighbours(cur, l));
                for &nb in self.neighbours(cur, l) {
                    let s = q.dot(codes.row(nb));
                    stats.sq8_distances += 1;
                    if s > cur_s || (s == cur_s && nb < cur) {
                        (cur, cur_s) = (nb, s);
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
        }

        let n = codes.n();
        let mut visited = vec![0u64; n.div_ceil(64)];
        let seen = |v: &mut Vec<u64>, d: u32| {
            let (w, b) = (d as usize / 64, d % 64);
            let was = v[w] >> b & 1 == 1;
            v[w] |= 1 << b;
            was
        };
        let peek = |v: &Vec<u64>, d: u32| v[d as usize / 64] >> (d % 64) & 1 == 1;
        let two_hop = two_hop && filter.is_some();
        let is_member = |d: u32, stats: &mut SearchStats| match &filter {
            None => true,
            Some(f) => {
                stats.filter_checks += 1;
                f.contains(d)
            }
        };
        // Candidates: max-heap by (score, smaller docID); results: TopK of members.
        let key = |s: i32, d: u32| ((s as i64) << 32) | (u32::MAX - d) as i64;
        let mut cand: BinaryHeap<i64> = BinaryHeap::new();
        let mut res = TopK::new(ef);
        seen(&mut visited, cur);
        cand.push(key(cur_s, cur));
        if is_member(cur, stats) {
            res.push(cur_s, cur);
        }
        let mut fresh: Vec<u32> = Vec::with_capacity(self.m0);
        let mut hop2: Vec<u32> = Vec::with_capacity(self.m0);
        while let Some(c) = cand.pop() {
            let (cs, cd) = ((c >> 32) as i32, u32::MAX - c as u32);
            if res.is_full() && cs < res.floor() {
                break;
            }
            stats.nodes_visited += 1;
            fresh.clear();
            if two_hop {
                hop2.clear();
                for &nb in self.neighbours(cd, 0) {
                    if !peek(&visited, nb) {
                        if is_member(nb, stats) {
                            seen(&mut visited, nb);
                            fresh.push(nb);
                        } else {
                            hop2.push(nb);
                        }
                    }
                }
                // A non-member is marked only once looked through, so one cut
                // short by the 2M cap stays a bridge for later expansions.
                'look: for &v in &hop2 {
                    if fresh.len() >= self.m0 {
                        break;
                    }
                    seen(&mut visited, v);
                    for &nb in self.neighbours(v, 0) {
                        if fresh.len() >= self.m0 {
                            break 'look;
                        }
                        if !peek(&visited, nb) && is_member(nb, stats) {
                            seen(&mut visited, nb);
                            fresh.push(nb);
                        }
                    }
                }
            } else {
                for &nb in self.neighbours(cd, 0) {
                    if !seen(&mut visited, nb) {
                        fresh.push(nb);
                    }
                }
            }
            for &nb in &fresh {
                kernels::prefetch(codes.row(nb).as_ptr(), codes.stride());
            }
            announce(&fresh);
            let mut i = 0;
            while i < fresh.len() {
                let mut scores = [0i32; 4];
                let take = (fresh.len() - i).min(4);
                if take == 4 {
                    scores = kernels::dot_u8_i8_x4(
                        [codes.row(fresh[i]), codes.row(fresh[i + 1]), codes.row(fresh[i + 2]), codes.row(fresh[i + 3])],
                        &q.q8,
                    );
                } else {
                    for j in 0..take {
                        scores[j] = q.dot(codes.row(fresh[i + j]));
                    }
                }
                stats.sq8_distances += take;
                for j in 0..take {
                    let (s, nb) = (scores[j], fresh[i + j]);
                    if !res.is_full() || s > res.floor() {
                        cand.push(key(s, nb));
                        if two_hop || is_member(nb, stats) {
                            res.push(s, nb);
                        }
                    }
                }
                i += take;
            }
        }
        res.into_sorted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_geometric() {
        let n = 200_000u32;
        let mut count = [0usize; 4];
        for i in 0..n {
            let l = level_of(i, 42, 16);
            for c in count.iter_mut().take(l.min(3) + 1) {
                *c += 1;
            }
        }
        // P(level >= l) = 16^-l.
        assert_eq!(count[0], n as usize);
        let p1 = count[1] as f64 / n as f64;
        assert!((p1 - 1.0 / 16.0).abs() < 0.005, "p1 {p1}");
        let p2 = count[2] as f64 / n as f64;
        assert!((p2 - 1.0 / 256.0).abs() < 0.001, "p2 {p2}");
    }
}
