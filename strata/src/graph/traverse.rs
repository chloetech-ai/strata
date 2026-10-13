//! Traversals over any adjacency: breadth-first reachability, shortest
//! paths and personalized PageRank. Nodes are opaque 64-bit ids (the
//! executor packs segment ordinal and docID into one).

use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};

use crate::error::Result;

pub type Node = u64;

/// Packs a segment ordinal and a docID.
#[inline]
pub fn node(seg: usize, doc: u32) -> Node {
    ((seg as u64) << 32) | doc as u64
}

#[inline]
pub fn split(n: Node) -> (usize, u32) {
    ((n >> 32) as usize, n as u32)
}

/// A multiplicative hasher for node ids (they are already well spread in
/// their low bits; the multiply mixes the segment ordinal in).
#[derive(Default)]
pub struct NodeHasher(u64);

impl Hasher for NodeHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = (x ^ (x >> 29)).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (x >> 32);
    }
}

pub type NodeMap<V> = HashMap<Node, V, BuildHasherDefault<NodeHasher>>;

pub fn node_map<V>() -> NodeMap<V> {
    NodeMap::default()
}

/// The edges a traversal follows.
pub trait Adjacency {
    /// Appends the neighbours of `n` with edge weights: along the edges'
    /// direction, or against it when `reverse`.
    fn neighbors(&mut self, n: Node, reverse: bool, out: &mut Vec<(Node, f32)>) -> Result<()>;
    /// Whether a traversal may continue through `n` (the `through`
    /// predicate; seeds are always expanded).
    fn passable(&mut self, _n: Node) -> bool {
        true
    }
}

/// Hop distances from a set of seeds.
#[derive(Debug, Default)]
pub struct Reach {
    /// Every node met, with its distance (seeds at 0).
    pub dist: NodeMap<u32>,
    /// Stopped early at the node limit.
    pub truncated: bool,
    /// Edges followed.
    pub edges: u64,
}

/// Breadth-first search to `max_hops`, meeting at most `max_nodes` nodes.
pub fn bfs(adj: &mut dyn Adjacency, seeds: &[Node], max_hops: u32, max_nodes: usize) -> Result<Reach> {
    let mut out = Reach { dist: node_map(), ..Default::default() };
    let mut frontier: Vec<Node> = Vec::new();
    for &s in seeds {
        if out.dist.insert(s, 0).is_none() {
            frontier.push(s);
        }
    }
    let mut buf = Vec::new();
    for depth in 1..=max_hops {
        let mut next = Vec::new();
        for &u in &frontier {
            buf.clear();
            adj.neighbors(u, false, &mut buf)?;
            out.edges += buf.len() as u64;
            for &(v, _) in &buf {
                if out.dist.contains_key(&v) {
                    continue;
                }
                if out.dist.len() >= max_nodes {
                    out.truncated = true;
                    return Ok(out);
                }
                out.dist.insert(v, depth);
                if depth < max_hops && adj.passable(v) {
                    next.push(v);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(out)
}

/// A shortest path from any of `from` to any of `to` (unweighted), by
/// bidirectional breadth-first search: each round expands the smaller
/// frontier, forward along the edges or backward against them. Returns the
/// path's nodes in order, and the edges followed.
pub fn shortest_path(adj: &mut dyn Adjacency, from: &[Node], to: &[Node], max_hops: u32, max_nodes: usize) -> Result<(Option<Vec<Node>>, u64)> {
    let mut edges = 0u64;
    if let Some(&n) = from.iter().find(|n| to.contains(n)) {
        return Ok((Some(vec![n]), 0));
    }
    // Parent pointers on each side; u64::MAX marks a seed.
    let mut fwd: NodeMap<Node> = from.iter().map(|&n| (n, u64::MAX)).collect();
    let mut bwd: NodeMap<Node> = to.iter().map(|&n| (n, u64::MAX)).collect();
    let mut ff: Vec<Node> = fwd.keys().copied().collect();
    let mut bf: Vec<Node> = bwd.keys().copied().collect();
    let mut buf = Vec::new();
    let (mut hops_f, mut hops_b) = (0u32, 0u32);
    while hops_f + hops_b < max_hops && !ff.is_empty() && !bf.is_empty() {
        let forward = ff.len() <= bf.len();
        let (frontier, mine, other) = if forward { (&mut ff, &mut fwd, &bwd) } else { (&mut bf, &mut bwd, &fwd) };
        let mut next = Vec::new();
        let mut meet = None;
        'outer: for &u in frontier.iter() {
            buf.clear();
            adj.neighbors(u, !forward, &mut buf)?;
            edges += buf.len() as u64;
            for &(v, _) in &buf {
                if mine.contains_key(&v) {
                    continue;
                }
                mine.insert(v, u);
                if other.contains_key(&v) {
                    meet = Some(v);
                    break 'outer;
                }
                if adj.passable(v) {
                    next.push(v);
                }
                if mine.len() + other.len() > max_nodes {
                    return Ok((None, edges));
                }
            }
        }
        if forward { hops_f += 1 } else { hops_b += 1 }
        if let Some(m) = meet {
            let mut path = Vec::new();
            let mut x = m;
            while x != u64::MAX {
                path.push(x);
                x = fwd[&x];
            }
            path.reverse();
            let mut x = bwd[&m];
            while x != u64::MAX {
                path.push(x);
                x = bwd[&x];
            }
            return Ok((Some(path), edges));
        }
        *frontier = next;
    }
    Ok((None, edges))
}

/// Personalized PageRank by forward push (Andersen, Chung and Lang, 2006).
///
/// The walk restarts at the seeds with probability `alpha` per step, seed
/// `s` with probability `w_s / Σ w`. Each node holds an estimate `p` and a
/// residual `r`; pushing `u` moves `alpha · r(u)` into `p(u)` and spreads
/// `(1 − alpha) · r(u)` over its neighbours in proportion to edge weight.
/// Nodes are pushed while `r(u) ≥ eps · deg(u)`, which bounds the error of
/// every estimate by `eps · deg`. Mass reaching a node without edges stays
/// in its estimate (the walk restarts from there). At most `max_pushes`
/// pushes.
pub struct Ppr {
    pub scores: NodeMap<f64>,
    pub pushes: u64,
    pub edges: u64,
}

pub fn ppr(adj: &mut dyn Adjacency, seeds: &[(Node, f64)], reverse: bool, alpha: f64, eps: f64, max_pushes: u64) -> Result<Ppr> {
    let total: f64 = seeds.iter().map(|s| s.1.max(0.0)).sum();
    let mut p: NodeMap<f64> = node_map();
    let mut r: NodeMap<f64> = node_map();
    let mut nbrs: NodeMap<(Vec<(Node, f32)>, f64)> = node_map();
    let mut queue: VecDeque<Node> = VecDeque::new();
    let mut queued: NodeMap<()> = node_map();
    if total <= 0.0 {
        return Ok(Ppr { scores: p, pushes: 0, edges: 0 });
    }
    for &(s, w) in seeds {
        *r.entry(s).or_insert(0.0) += w.max(0.0) / total;
        if queued.insert(s, ()).is_none() {
            queue.push_back(s);
        }
    }
    let (mut pushes, mut edges) = (0u64, 0u64);
    while let Some(u) = queue.pop_front() {
        queued.remove(&u);
        if pushes >= max_pushes {
            break;
        }
        let ru = r.get(&u).copied().unwrap_or(0.0);
        // The threshold is eps · max(deg, 1) ≥ eps: below eps a node cannot
        // be pushed, whatever its degree, so its neighbours are not needed.
        if ru < eps {
            continue;
        }
        if !nbrs.contains_key(&u) {
            let mut list = Vec::with_capacity(16);
            adj.neighbors(u, reverse, &mut list)?;
            let deg = list.iter().map(|x| x.1 as f64).sum::<f64>();
            nbrs.insert(u, (list, deg));
        }
        let (list, deg) = &nbrs[&u];
        if ru < eps * deg.max(1.0) {
            continue;
        }
        pushes += 1;
        r.insert(u, 0.0);
        if *deg == 0.0 {
            *p.entry(u).or_insert(0.0) += ru;
            continue;
        }
        *p.entry(u).or_insert(0.0) += alpha * ru;
        let spread = (1.0 - alpha) * ru / deg;
        edges += list.len() as u64;
        for &(v, w) in list {
            let rv = r.entry(v).or_insert(0.0);
            *rv += spread * w as f64;
            // Queue a node once its residual could pass the threshold; the
            // exact test (with its degree) runs when it is popped.
            if *rv >= eps && queued.insert(v, ()).is_none() {
                queue.push_back(v);
            }
        }
    }
    Ok(Ppr { scores: p, pushes, edges })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directed graph on small integers.
    struct G(Vec<Vec<u64>>, Option<u64>);
    impl Adjacency for G {
        fn neighbors(&mut self, n: Node, reverse: bool, out: &mut Vec<(Node, f32)>) -> Result<()> {
            if reverse {
                out.extend((0..self.0.len() as u64).filter(|&u| self.0[u as usize].contains(&n)).map(|u| (u, 1.0)));
            } else {
                out.extend(self.0[n as usize].iter().map(|&v| (v, 1.0)));
            }
            Ok(())
        }
        fn passable(&mut self, n: Node) -> bool {
            self.1 != Some(n)
        }
    }

    fn chain() -> G {
        // 0 → 1 → 2 → 3 → 4, and 0 → 5 → 4
        G(vec![vec![1, 5], vec![2], vec![3], vec![4], vec![], vec![4]], None)
    }

    #[test]
    fn bfs_distances_limits_and_through() {
        let r = bfs(&mut chain(), &[0], 2, 100).unwrap();
        let mut d: Vec<(u64, u32)> = r.dist.into_iter().collect();
        d.sort();
        assert_eq!(d, vec![(0, 0), (1, 1), (2, 2), (4, 2), (5, 1)]);
        assert!(bfs(&mut chain(), &[0], 9, 3).unwrap().truncated);
        // Not through 5: 4 is reached only along the long chain.
        let mut g = chain();
        g.1 = Some(5);
        assert_eq!(bfs(&mut g, &[0], 9, 100).unwrap().dist[&4], 4);
    }

    #[test]
    fn shortest_paths_both_ways() {
        assert_eq!(shortest_path(&mut chain(), &[0], &[4], 10, 100).unwrap().0, Some(vec![0, 5, 4]));
        assert_eq!(shortest_path(&mut chain(), &[1], &[4], 10, 100).unwrap().0, Some(vec![1, 2, 3, 4]));
        assert_eq!(shortest_path(&mut chain(), &[4], &[0], 10, 100).unwrap().0, None);
        assert_eq!(shortest_path(&mut chain(), &[1], &[4], 2, 100).unwrap().0, None);
        assert_eq!(shortest_path(&mut chain(), &[3], &[3], 2, 100).unwrap().0, Some(vec![3]));
        let mut g = chain();
        g.1 = Some(5);
        assert_eq!(shortest_path(&mut g, &[0], &[4], 10, 100).unwrap().0, Some(vec![0, 1, 2, 3, 4]));
    }

    /// Power iteration on a small graph matches forward push.
    #[test]
    fn ppr_matches_power_iteration() {
        let g = vec![vec![1, 2], vec![2], vec![0, 3], vec![0], vec![1, 3]];
        let n = g.len();
        let alpha = 0.15;
        let mut x = vec![0.0f64; n];
        let e = |i: usize| if i == 0 { 1.0 } else { 0.0 };
        for _ in 0..500 {
            let mut y: Vec<f64> = (0..n).map(|i| alpha * e(i)).collect();
            for u in 0..n {
                for &v in &g[u] {
                    y[v as usize] += (1.0 - alpha) * x[u] / g[u].len() as f64;
                }
            }
            x = y;
        }
        let out = ppr(&mut G(g, None), &[(0, 1.0)], false, alpha, 1e-9, 10_000_000).unwrap();
        for (i, want) in x.iter().enumerate() {
            let got = out.scores.get(&(i as u64)).copied().unwrap_or(0.0);
            assert!((got - want).abs() < 1e-6, "node {i}: {got} vs {want}");
        }
    }
}
