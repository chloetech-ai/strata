//! Graph computations of a query (DESIGN.md §7): seeds are resolved to
//! rows, the traversal runs over the snapshot (edges resolved through the
//! primary-key indexes), and the result becomes, per segment, a materialised
//! document set with exact cardinality, plus hop distances and scores.
//!
//! A walk over an edge table finds a row's edges by its key in each of the
//! edge table's segments, at the same version ([`crate::graph::edge_table`]),
//! and reads each edge's weight as it is now, in-place writes included.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use super::{err, Executor, Hit, ProbeNode, Seg, Ticker};
use crate::query::memory::Charge;
use crate::error::Result;
use crate::graph::traverse::{self, node, split, Adjacency, Node, NodeMap};
use crate::graph::edge_table::weight_of;
use crate::graph::{EdgeTableReader, GraphReader};
use crate::query::logical::{Direction, GraphKind, GraphQuery, Pred, Score, Seeds};
use crate::query::plan::{FilterMode, VectorStrategy};
use crate::schema::FieldType;
use crate::segment::{DocFilter, DocId, DocSet};
use crate::storage::{key_hash, Snapshot, Updates};
use crate::vector::{SearchParams, VectorReader};

/// The result of one graph computation.
pub(super) struct GraphOut {
    /// Rows in the result: hop distance (`u32::MAX` for PageRank) and score.
    pub nodes: NodeMap<(u32, f64)>,
    /// The rows by segment ordinal, as document sets.
    pub per_seg: HashMap<usize, Arc<DocSet>>,
}

/// Segment `ord`'s view for traversals.
struct SegRef<'x> {
    live: Option<Arc<dyn DocFilter>>,
    graph: Option<&'x GraphReader>,
    vectors: Option<&'x VectorReader>,
    /// Accepted label codes (`None`: any label).
    labels: Option<Vec<u16>>,
    /// The `through` predicate, compiled for this segment.
    through: Option<ProbeNode>,
}

/// One segment of an edge table.
struct EdgeSeg {
    reader: EdgeTableReader,
    live: Option<Arc<dyn DocFilter>>,
    weights: Weights,
    /// The row each key number resolved to: 0 not yet, 1 none, else the
    /// node plus 2. Empty until the walk has resolved a thirty-second of the
    /// segment's keys through the key memo, so a short walk allocates none.
    ends: Vec<u64>,
    resolved: usize,
}

/// A segment's weights as they are now: written in place or as built.
struct Weights {
    /// The weight field.
    field: usize,
    /// The values written in place, if a weight was.
    updates: Option<Arc<Updates>>,
    /// The weights copied out of `updates`, once the walk has read enough
    /// of them that one pass over its layers costs less than the reads.
    flat: Option<Vec<f32>>,
    reads: usize,
    /// The reads through `updates` before copying them out: a sixteenth of
    /// their entries, each read being a lookup in every layer.
    budget: usize,
}

impl Weights {
    fn new(field: Option<usize>, updates: Option<&Arc<Updates>>) -> Weights {
        let (field, updates) = match (field, updates) {
            (Some(f), Some(u)) if u.has(f) => (f, Some(u.clone())),
            _ => (0, None),
        };
        let budget = updates.as_ref().map_or(0, |u| u.stored() / 16 + 64);
        Weights { field, updates, flat: None, reads: 0, budget }
    }

    /// Row `d`'s weight; `copied` counts the bytes of a copy it makes.
    #[inline]
    fn get(&mut self, r: &EdgeTableReader, d: DocId, copied: &mut u64) -> f32 {
        if let Some(f) = &self.flat {
            return f[d as usize];
        }
        let Some(u) = &self.updates else { return r.weight(d) };
        self.reads += 1;
        if self.reads <= self.budget {
            return u.get(self.field, d).map_or_else(|| r.weight(d), weight_of);
        }
        let mut f = r.weights();
        u.each_oldest_first(self.field, |d, v| {
            if let Some(w) = f.get_mut(d as usize) {
                *w = weight_of(v);
            }
        });
        *copied += 4 * f.len() as u64;
        let w = f[d as usize];
        self.flat = Some(f);
        w
    }
}

/// An edge table, as a walk over the table its keys name reads it.
struct EdgeTable {
    segs: Vec<EdgeSeg>,
}

struct GraphAdj<'x> {
    snap: &'x Snapshot,
    segs: Vec<Option<SegRef<'x>>>,
    /// The edge table walked instead of a field.
    edges: Option<EdgeTable>,
    dir: Direction,
    has_through: bool,
    /// VECTOR fields: neighbours are the `k` most similar rows.
    knn: Option<(usize, f64, SearchParams)>,
    /// Target keys resolved during this query, by key hash.
    memo: NodeMap<Option<Node>>,
    int_keys: bool,
    /// The query's deadline, checked as the traversal expands rows.
    tick: Ticker<'x>,
    /// What the traversal holds, counted against the query's memory every
    /// 64 KiB.
    charge: Charge,
    held: u64,
}

impl<'x> GraphAdj<'x> {
    /// The neighbours of row `d` of segment `s` along the rows of edge table
    /// `et`: the targets of the rows whose source is its key, the sources of
    /// those whose target is (by direction), with positive weights.
    fn edge_table_neighbors(&mut self, et: &mut EdgeTable, s: usize, d: DocId, reverse: bool, out: &mut Vec<(Node, f32)>) -> Result<()> {
        let snap = self.snap;
        let (Some(_), Some(pk)) = (&self.segs[s], snap.pk(s)) else { return Ok(()) };
        let key = pk.key_ref(d);
        let h = key_hash(key);
        let (fwd, back) = match (self.dir, reverse) {
            (Direction::Both, _) => (true, true),
            (Direction::Out, false) | (Direction::In, true) => (true, false),
            _ => (false, true),
        };
        let mut copied = 0;
        for seg in et.segs.iter_mut() {
            let Some(k) = seg.reader.find(key, h) else { continue };
            for leaving in [true, false] {
                let rows = match leaving {
                    true if fwd => seg.reader.leaving(k),
                    false if back => seg.reader.reaching(k),
                    _ => continue,
                };
                for &row in rows {
                    // A row of the segment (a damaged section lists others).
                    let Some(other) = (if leaving { seg.reader.target(row) } else { seg.reader.source(row) }) else { continue };
                    if seg.live.as_ref().is_some_and(|l| !l.contains(row)) {
                        continue;
                    }
                    let w = seg.weights.get(&seg.reader, row, &mut copied);
                    // Zero, negative and NaN weights are not followed.
                    if !(w > 0.0) {
                        continue;
                    }
                    let n2 = match seg.ends.get_mut(other as usize) {
                        Some(end) => {
                            if *end == 0 {
                                *end = seg.reader.key(other).and_then(|k| self.resolve(k)).map_or(1, |n2| n2 + 2);
                            }
                            (*end > 1).then(|| *end - 2)
                        }
                        None => {
                            seg.resolved += 1;
                            if seg.ends.is_empty() && seg.resolved > seg.reader.n_keys() / 32 {
                                seg.ends = vec![0; seg.reader.n_keys()];
                                copied += 8 * seg.ends.len() as u64;
                            }
                            seg.reader.key(other).and_then(|k| self.resolve(k))
                        }
                    };
                    if let Some(n2) = n2 {
                        out.push((n2, w));
                    }
                }
            }
        }
        self.hold(copied)
    }

    /// The live row with primary key `key`.
    fn resolve(&mut self, key: crate::storage::KeyRef) -> Option<Node> {
        let h = key_hash(key);
        if let Some(&hit) = self.memo.get(&h) {
            // Integer hashes are a bijection; a string key is checked
            // against the row found (only found rows are remembered).
            match hit {
                Some(n) if self.int_keys => return Some(n),
                None if self.int_keys => return None,
                Some(n) => {
                    let (s, d) = split(n);
                    if self.snap.pk(s).is_some_and(|pk| pk.key_ref(d) == key) {
                        return Some(n);
                    }
                }
                None => {}
            }
        }
        let found = self.snap.lookup_key(key).map(|(s, d)| node(s, d));
        if found.is_some() || self.int_keys {
            self.memo.insert(h, found);
        }
        found
    }
}

impl SegRef<'_> {
    fn label_ok(&self, g: &GraphReader, e: usize) -> bool {
        self.labels.as_ref().is_none_or(|ls| ls.contains(&g.label(e)))
    }
    fn live(&self, d: DocId) -> bool {
        self.live.as_ref().is_none_or(|l| l.contains(d))
    }
}

impl Adjacency for GraphAdj<'_> {
    fn hold(&mut self, bytes: u64) -> Result<()> {
        self.held += bytes;
        if self.held >= 1 << 16 {
            self.charge.add(std::mem::take(&mut self.held))?;
        }
        Ok(())
    }

    fn neighbors(&mut self, n: Node, reverse: bool, out: &mut Vec<(Node, f32)>) -> Result<()> {
        self.tick.tick()?;
        let (s, d) = split(n);
        if let Some(mut et) = self.edges.take() {
            let r = self.edge_table_neighbors(&mut et, s, d, reverse, out);
            self.edges = Some(et);
            return r;
        }
        if let Some((k, min_sim, params)) = &self.knn {
            let Some(v) = self.segs[s].as_ref().and_then(|r| r.vectors).filter(|v| v.has_vector(d)).map(|v| v.vector(d).to_vec()) else {
                return Ok(());
            };
            let mut hits: Vec<(f32, Node)> = Vec::new();
            for (i, r) in self.segs.iter().enumerate() {
                let Some(r) = r else { continue };
                let Some(vr) = r.vectors else { continue };
                let mut sp = params.clone();
                sp.strategy = super::to_vector_strategy(VectorStrategy::InIndex, vr.index_kind(), r.live.is_some());
                let found = vr.search(&v, k + 1, &sp, r.live.as_deref());
                hits.extend(found.hits.into_iter().map(|h| (h.score, node(i, h.doc))));
            }
            hits.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            out.extend(hits.into_iter().filter(|&(sim, m)| m != n && sim as f64 >= *min_sim).take(*k).map(|(sim, m)| (m, sim.max(1e-6))));
            return Ok(());
        }
        let dir = match (self.dir, reverse) {
            (Direction::Out, true) => Direction::In,
            (Direction::In, true) => Direction::Out,
            (d, _) => d,
        };
        if self.segs[s].is_none() {
            return Ok(());
        }
        if matches!(dir, Direction::Out | Direction::Both) {
            let me = self.segs[s].as_ref().unwrap();
            if let Some(g) = me.graph {
                out.reserve(g.out_edges(d).len());
                for e in g.out_edges(d) {
                    let me = self.segs[s].as_ref().unwrap();
                    if !me.label_ok(g, e) {
                        continue;
                    }
                    // A target built into this segment and still live is the
                    // target: a key has one live row.
                    let hit = match g.local_target(e) {
                        Some(t) if me.live(t) => Some(node(s, t)),
                        _ => self.resolve(g.target(e)),
                    };
                    if let Some(n2) = hit {
                        out.push((n2, g.weight(e)));
                    }
                }
            }
        }
        if matches!(dir, Direction::In | Direction::Both) {
            let Some(pk) = self.snap.pk(s) else { return Ok(()) };
            let key = pk.key_ref(d);
            let h = key_hash(key);
            // Find every segment's group first, so a hub's in-edges are
            // collected into one allocation.
            let groups: Vec<(usize, std::ops::Range<usize>)> = self
                .segs
                .iter()
                .enumerate()
                .filter_map(|(s2, r)| Some((s2, r.as_ref()?.graph?.in_group(key, h))))
                .filter(|(_, g)| !g.is_empty())
                .collect();
            out.reserve(groups.iter().map(|(_, g)| g.len()).sum());
            for (s2, group) in groups {
                let r = self.segs[s2].as_ref().unwrap();
                let g = r.graph.unwrap();
                if !g.has_labels_or_weights() && r.live.is_none() {
                    // The common case reads the grouped sources in sequence.
                    out.extend(group.map(|i| (node(s2, g.in_source(i)), 1.0)));
                    continue;
                }
                for i in group {
                    let (src, e) = (g.in_source(i), g.in_edge(i));
                    if r.label_ok(g, e) && r.live(src) {
                        out.push((node(s2, src), g.weight(e)));
                    }
                }
            }
        }
        Ok(())
    }

    fn passable(&mut self, n: Node) -> bool {
        if !self.has_through {
            return true;
        }
        let (s, d) = split(n);
        self.segs[s].as_ref().and_then(|r| r.through.as_ref()).is_some_and(|t| t.contains(d))
    }
}

impl<'a> Executor<'a> {
    /// Runs every graph computation of the query, in order.
    pub(super) fn run_graphs(&mut self, gs: &[crate::query::logical::GraphQuery]) -> Result<()> {
        self.graphs.clear();
        for (i, g) in gs.iter().enumerate() {
            let out = self.run_graph(i, g)?;
            self.graphs.push(out);
        }
        Ok(())
    }

    fn run_graph(&mut self, id: usize, g: &GraphQuery) -> Result<GraphOut> {
        let t0 = Instant::now();
        let seeds = self.resolve_seeds(&g.seeds)?;
        let seed_nodes: Vec<Node> = seeds.iter().map(|s| s.0).collect();
        let targets: Vec<Node> = match &g.kind {
            GraphKind::Path { to, .. } => self.resolve_seeds(to)?.into_iter().map(|s| s.0).collect(),
            _ => Vec::new(),
        };
        let from = self.segs.clone();
        let edges = g.edges.as_deref().map(|t| self.edge_table(t)).transpose()?;
        let mut adj = self.adjacency(g, &from, edges)?;
        let mut nodes: NodeMap<(u32, f64)> = traverse::node_map();
        let (what, edges, extra) = match &g.kind {
            GraphKind::Reach { min_hops, max_hops } => {
                let r = traverse::bfs(&mut adj, &seed_nodes, *max_hops, g.max_nodes)?;
                // A cut answer would depend on the order of the edges.
                if r.truncated {
                    return Err(err(format!("REACHABLE meets more than max_nodes => {} rows within hops => {max_hops}: raise max_nodes or lower hops", g.max_nodes)));
                }
                for (n, d) in r.dist {
                    if d >= *min_hops && d <= *max_hops {
                        nodes.insert(n, (d, -(d as f64)));
                    }
                }
                ("reachable", r.edges, String::new())
            }
            GraphKind::Path { max_hops, .. } => {
                let traverse::Path { nodes: path, edges, truncated } = traverse::shortest_path(&mut adj, &seed_nodes, &targets, *max_hops, g.max_nodes)?;
                if truncated {
                    return Err(err(format!("PATH meets more than max_nodes => {} rows before finding a path: raise max_nodes", g.max_nodes)));
                }
                let len = path.as_ref().map_or(0, |p| p.len());
                for (i, n) in path.into_iter().flatten().enumerate() {
                    nodes.insert(n, (i as u32, -(i as f64)));
                }
                ("path", edges, format!(", length {}", len.saturating_sub(1)))
            }
            GraphKind::Ppr { alpha, eps, max_pushes, weighted } => {
                let weights: Vec<(Node, f64)> = seeds.iter().map(|&(n, sc)| (n, if *weighted { sc.max(1e-9) } else { 1.0 })).collect();
                let r = traverse::ppr(&mut adj, &weights, false, *alpha, *eps, *max_pushes)?;
                for (n, sc) in r.scores {
                    if sc > 0.0 {
                        nodes.insert(n, (u32::MAX, sc));
                    }
                }
                ("ppr", r.edges, format!(", {} pushes", r.pushes))
            }
        };
        // PageRank is a score, never a filter: it needs no document sets.
        let mut by_seg: HashMap<usize, Vec<DocId>> = HashMap::new();
        for &n in nodes.keys().filter(|_| !matches!(g.kind, GraphKind::Ppr { .. })) {
            let (s, d) = split(n);
            by_seg.entry(s).or_default().push(d);
        }
        let mut per_seg = HashMap::new();
        for (s, mut docs) in by_seg {
            docs.sort_unstable();
            let n = self.snap.segments()[s].file.n_docs() as u32;
            per_seg.insert(s, Arc::new(DocSet::from_sorted(docs, n)));
        }
        self.stats.plan.push(format!(
            "graph {id}: {what} over {}, {} seeds, {} rows, {edges} edges followed{extra}, {:.2} ms",
            g.edges.as_deref().unwrap_or(&self.schema.fields[g.field].name),
            seed_nodes.len(),
            nodes.len(),
            t0.elapsed().as_secs_f64() * 1e3
        ));
        Ok(GraphOut { nodes, per_seg })
    }

    /// Seed rows with their scores (1 for keys).
    fn resolve_seeds(&mut self, seeds: &Seeds) -> Result<Vec<(Node, f64)>> {
        Ok(match seeds {
            Seeds::Keys(keys) => {
                let mut out: Vec<(Node, f64)> = keys.iter().filter_map(|k| self.snap.lookup(k)).map(|(s, d)| (node(s, d), 1.0)).collect();
                out.dedup_by_key(|x| x.0);
                out
            }
            // A key listed twice restarts with both weights.
            Seeds::Weighted(keys) => {
                let mut out: Vec<(Node, f64)> = Vec::with_capacity(keys.len());
                let mut at: HashMap<Node, usize> = HashMap::new();
                for (k, w) in keys {
                    let Some((s, d)) = self.snap.lookup(k) else { continue };
                    let n = node(s, d);
                    match at.get(&n) {
                        Some(&i) => out[i].1 += w,
                        None => {
                            at.insert(n, out.len());
                            out.push((n, *w));
                        }
                    }
                }
                out
            }
            Seeds::Query(q) => {
                let mut sub = Executor::new(self.snap, &self.readers_all, self.opts);
                let hits: Vec<Hit> = sub.select_hits(q)?;
                self.stats.plan.extend(sub.stats.plan.into_iter().map(|l| format!("  seeds: {l}")));
                hits.into_iter().map(|h| (node(h.seg, h.doc), h.score)).collect()
            }
            Seeds::Plan(_) => return Err(super::unresolved()),
        })
    }

    /// Edge table `name` at this snapshot's version, checked against this
    /// table: its source and target hold this table's keys.
    fn edge_table(&self, name: &str) -> Result<EdgeTable> {
        let snap = self.snap.table(name).ok_or_else(|| err(format!("no such table: {name}")))?;
        let schema = snap.schema();
        let Some((src, _, weight)) = schema.edge_fields() else {
            return Err(err(format!("table {} is not an edge table: a walk follows an EDGES field or a table with OPTIONS(edges = 'src, dst, w')", schema.table)));
        };
        let pk = self.schema.primary_key.ok_or_else(|| err(format!("a walk over {} needs {} to have a primary key", schema.table, self.schema.table)))?;
        if schema.fields[src].ty != self.schema.fields[pk].ty {
            return Err(err(format!("{} links keys of another type than {}'s", schema.table, self.schema.table)));
        }
        let mut segs = Vec::with_capacity(snap.segments().len());
        for v in snap.segments() {
            if let Some(reader) = EdgeTableReader::open(&v.file)? {
                segs.push(EdgeSeg { reader, live: v.live.clone(), weights: Weights::new(weight, v.updates.as_ref()), ends: Vec::new(), resolved: 0 });
            }
        }
        Ok(EdgeTable { segs })
    }

    /// The traversal's view of `from`, the executor's segments (a copy of
    /// [`Executor::segs`] the caller keeps while it traverses), and of the
    /// edge table it walks instead of a field.
    fn adjacency<'s>(&self, g: &GraphQuery, from: &'s [Seg<'a>], edges: Option<EdgeTable>) -> Result<GraphAdj<'s>> {
        let field = &self.schema.fields[g.field];
        let vector = edges.is_none() && matches!(field.ty, FieldType::Vector { .. });
        let mut segs: Vec<Option<SegRef<'s>>> = (0..self.snap.segments().len()).map(|_| None).collect();
        for s in from {
            let graph = if vector || edges.is_some() { None } else { s.r.graph(g.field)? };
            let vectors = if vector { s.r.vector(g.field)? } else { None };
            let labels = match (graph, g.labels.is_empty()) {
                (Some(gr), false) => Some(g.labels.iter().filter_map(|l| gr.label_code(l)).collect()),
                _ => None,
            };
            let through = match &g.through {
                Some(p) => Some(self.compile_probe(s, p)?),
                None => None,
            };
            segs[s.ord] = Some(SegRef { live: s.view.live.clone(), graph, vectors, labels, through });
        }
        let knn = vector.then(|| (g.k, g.min_sim, self.opts.vector_params.clone()));
        let int_keys = crate::graph::int_keys(self.schema);
        // A row's neighbours by similarity take a vector search each.
        let tick = Ticker::new(self.opts, if vector { 16 } else { Ticker::DOCS });
        let charge = self.charge.clone();
        Ok(GraphAdj { snap: self.snap, segs, edges, dir: g.dir, has_through: g.through.is_some(), knn, memo: traverse::node_map(), int_keys, tick, charge, held: 0 })
    }

    /// Top rows by PageRank or hop distance, among those passing `p`.
    pub(super) fn graph_top(&mut self, score: &Score, p: &Pred, k: usize) -> Result<Vec<Hit>> {
        let (gid, value): (usize, Box<dyn Fn(&(u32, f64)) -> f64>) = match score {
            Score::Ppr { graph } => (*graph, Box::new(|x: &(u32, f64)| x.1)),
            Score::Hops { graph, far_first } => {
                let far = *far_first;
                (*graph, Box::new(move |x: &(u32, f64)| if far { x.0 as f64 } else { -(x.0 as f64) }))
            }
            _ => return Err(err("not a graph score")),
        };
        let out = self.graphs.get(gid).ok_or_else(|| err("graph score without its computation"))?;
        let mut hits = Vec::new();
        if *p == Pred::True {
            // Rows in a graph result are live: no filter to apply.
            hits.extend(out.nodes.iter().map(|(n, x)| {
                let (seg, doc) = split(*n);
                Hit { seg, doc, score: value(x) }
            }));
        }
        let mut by_seg: HashMap<usize, Vec<(DocId, f64)>> = HashMap::new();
        if *p != Pred::True {
            for (n, x) in &out.nodes {
                let (s, d) = split(*n);
                by_seg.entry(s).or_default().push((d, value(x)));
            }
        }
        let segs = self.segs.clone();
        for s in segs.iter() {
            let Some(cands) = by_seg.remove(&s.ord) else { continue };
            let probeable = {
                let est = super::SegEst { ex: self, s, params: None, local_sel: None };
                self.planner(&est).estimate(p).probeable
            };
            let f = self.seg_filter(s, p, if probeable { FilterMode::Probe } else { FilterMode::Materialize })?;
            let f = f.as_dyn();
            hits.extend(cands.into_iter().filter(|(d, _)| f.is_none_or(|f| f.contains(*d))).map(|(doc, score)| Hit { seg: s.ord, doc, score }));
        }
        self.stats.scored += hits.len() as u64;
        self.stats.plan.push(format!("graph {gid}: ranked {} rows by {}", hits.len(), if matches!(score, Score::Ppr { .. }) { "ppr" } else { "hops" }));
        // Hop counts tie by the hundred: every row tied with the k-th stays,
        // so that a tie-break (ORDER BY hops(), id) chooses among all of them.
        // The k-th is found in linear time; only the rows kept are sorted.
        let better = |a: &Hit, b: &Hit| b.score.total_cmp(&a.score).then((a.seg, a.doc).cmp(&(b.seg, b.doc)));
        if k > 0 && hits.len() > k {
            hits.select_nth_unstable_by(k - 1, better);
            let cut = hits[k - 1].score;
            hits.retain(|h| h.score >= cut);
        }
        hits.sort_by(better);
        Ok(hits)
    }

    /// A graph score of one row (0 for PageRank, very low for hops, when
    /// the row is not in the result).
    pub(super) fn graph_score(&self, score: &Score, seg: usize, doc: DocId) -> f64 {
        let n = node(seg, doc);
        match score {
            Score::Ppr { graph } => self.graphs.get(*graph).and_then(|g| g.nodes.get(&n)).map_or(0.0, |x| x.1),
            Score::Hops { graph, far_first } => match self.graphs.get(*graph).and_then(|g| g.nodes.get(&n)) {
                Some(x) if *far_first => x.0 as f64,
                Some(x) => -(x.0 as f64),
                None => -1e9,
            },
            _ => 0.0,
        }
    }

    /// `hops()` of a row.
    pub(super) fn hops_of(&self, graph: usize, seg: usize, doc: DocId) -> Option<u32> {
        self.graphs.get(graph)?.nodes.get(&node(seg, doc)).map(|x| x.0).filter(|&h| h != u32::MAX)
    }

    pub(super) fn graph_count(&self, id: usize, seg: usize) -> f64 {
        self.graphs.get(id).and_then(|g| g.per_seg.get(&seg)).map_or(0.0, |s| s.len() as f64)
    }

    pub(super) fn graph_set(&self, id: usize, seg: usize, n_docs: u32) -> DocSet {
        self.graphs.get(id).and_then(|g| g.per_seg.get(&seg)).map_or_else(|| DocSet::empty(n_docs), |s| (**s).clone())
    }
}
