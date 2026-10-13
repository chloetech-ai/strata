//! BM25 ranking: exhaustive, MaxScore and Block-Max WAND top-k, filtered
//! top-k, and exact scoring of given documents.
//!
//! `score(d) = Σ_t m_t · idf_t · tf (k1 + 1) / (tf + k1 (1 - b + b · len(d) / avgdl))`
//! with `idf_t = ln(1 + (N - df_t + 0.5) / (df_t + 0.5))` (Lucene's), `m_t`
//! the multiplicity of `t` in the query, `len(d)` the document length as
//! stored in its one-byte norm, and `N`, `avgdl`, `df_t` from
//! [`CollectionStats`] summed over a snapshot's segments, so that a
//! document scores the same in any segment.
//!
//! Upper bounds: a list's from its impact frontier (exact), a block's from
//! its impact (largest tf, smallest norm). Both hold for any parameters.
//!
//! All three algorithms visit documents in docID order and keep the first of
//! equal-scored documents, so they return the same top-k up to float
//! rounding (sums are taken in different orders).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use super::codec::NORM_LENGTHS;
use super::cursor::{PostingCursor, END};
use super::postings::{Impact, LIST_IMPACT};
use super::reader::TextFieldReader;
use crate::segment::{DocFilter, DocId};

/// BM25 parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bm25Params {
    pub k1: f32,
    pub b: f32,
}

impl Default for Bm25Params {
    fn default() -> Self {
        Bm25Params { k1: 1.2, b: 0.75 }
    }
}

/// Collection-wide statistics for scoring, summed over a snapshot's
/// segments by the caller.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CollectionStats {
    pub n_docs: u64,
    /// Sum of document lengths.
    pub total_len: u64,
    /// Document frequency of each query term (a term missing here uses
    /// the segment's own document frequency).
    pub df: HashMap<String, u64>,
}

impl CollectionStats {
    /// Adds another segment's statistics.
    pub fn add(&mut self, other: &CollectionStats) {
        self.n_docs += other.n_docs;
        self.total_len += other.total_len;
        for (t, &d) in &other.df {
            *self.df.entry(t.clone()).or_default() += d;
        }
    }

    /// Average document length.
    pub fn avgdl(&self) -> f32 {
        if self.n_docs == 0 {
            1.0
        } else {
            (self.total_len as f64 / self.n_docs as f64).max(1e-9) as f32
        }
    }

    /// `ln(1 + (N - df + 0.5) / (df + 0.5))`.
    pub fn idf(&self, df: u64) -> f32 {
        let n = self.n_docs as f64;
        let df = df as f64;
        (1.0 + ((n - df).max(0.0) + 0.5) / (df + 0.5)).ln() as f32
    }
}

/// Top-k algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopKAlgo {
    /// Score every document containing a query term.
    Exhaustive,
    /// MaxScore (Turtle & Flood): lists whose bounds cannot lift a document
    /// above the threshold only confirm candidates of the others.
    MaxScore,
    /// Block-Max WAND (Ding & Suel): WAND pivoting on list bounds, refined
    /// with block bounds that also skip whole blocks.
    BlockMaxWand,
}

/// How a filter is applied by [`TextFieldReader::top_k_with`] and
/// [`TextFieldReader::eval_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterStrategy {
    /// By the filter's cardinality against the query's cost (see
    /// [`drive_cost`] and [`super::reader::PROBE_COST`]).
    Auto,
    /// Iterate the filter's members and check or score each by seeking the
    /// lists (needs a filter that lists its members, else `Probe`).
    Drive,
    /// Run the query and skip candidates not in the filter.
    Probe,
}

/// `Auto` drives a top-k from the filter when `members * terms *
/// drive_cost(algo)` is below the total length of the query's lists, i.e.
/// when scoring a member by seeking one list costs `drive_cost` postings
/// of the filtered algorithm (fitted per query by `strata-text-bench` over
/// selectivities from 0.01% to 30%; see `notes/text.md`). A filter weakens
/// Block-Max WAND's pruning more than MaxScore's, so it drives longer.
pub fn drive_cost(algo: TopKAlgo) -> f64 {
    match algo {
        TopKAlgo::Exhaustive => 0.9,
        TopKAlgo::MaxScore => 1.8,
        TopKAlgo::BlockMaxWand => 0.9,
    }
}

/// Upper bounds are scaled up by this much so that float rounding (sums in
/// different orders) never prunes a document that would qualify.
const BOUND_SLACK: f32 = 1.0 + 1e-5;

/// One query term: its cursor, weight `m · idf · (k1 + 1)` and list bound.
struct TermScorer<'a> {
    cur: PostingCursor<'a>,
    weight: f32,
    max: f32,
    /// The block bound last computed, valid for targets below `block_end`
    /// (one past the block's last docID; 0 before the first call).
    block_end: u64,
    block_max: f32,
}

impl TermScorer<'_> {
    #[inline]
    fn score(&mut self, k: &[f32; 256], norm: u8) -> f32 {
        let tf = self.cur.freq() as f32;
        self.weight * tf / (tf + k[norm as usize])
    }

    #[inline]
    fn bound(&self, k: &[f32; 256], imp: Impact) -> f32 {
        if imp == LIST_IMPACT {
            return self.max;
        }
        let tf = (imp >> 8) as f32;
        self.weight * tf / (tf + k[(imp & 0xff) as usize]) * BOUND_SLACK
    }

    /// Bound of the block holding the first posting `>= target` (targets
    /// must not decrease); recomputed only when the block changes.
    #[inline]
    fn block_max(&mut self, k: &[f32; 256], target: DocId) -> f32 {
        if target as u64 >= self.block_end {
            let (last, imp) = self.cur.block_bound(target);
            self.block_end = last as u64 + 1;
            self.block_max = self.bound(k, imp);
        }
        self.block_max
    }
}

/// The best `k` (score, doc) pairs; ties keep the earlier document.
struct TopK {
    k: usize,
    heap: BinaryHeap<Entry>,
}

/// Heap order: the greatest entry is the worst (lowest score, then highest
/// docID), so `peek` is the threshold.
#[derive(PartialEq)]
struct Entry(f32, DocId);

impl Eq for Entry {}

impl Ord for Entry {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.total_cmp(&self.0).then(self.1.cmp(&o.1))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl TopK {
    fn new(k: usize) -> Self {
        TopK { k, heap: BinaryHeap::with_capacity(k + 1) }
    }

    /// Scores must exceed this to enter.
    #[inline]
    fn threshold(&self) -> f32 {
        if self.heap.len() < self.k {
            f32::NEG_INFINITY
        } else {
            self.heap.peek().map_or(f32::NEG_INFINITY, |e| e.0)
        }
    }

    #[inline]
    fn insert(&mut self, doc: DocId, score: f32) {
        if self.heap.len() < self.k {
            self.heap.push(Entry(score, doc));
        } else if score > self.threshold() {
            self.heap.pop();
            self.heap.push(Entry(score, doc));
        }
    }

    fn into_sorted(self) -> Vec<(DocId, f32)> {
        self.heap.into_sorted_vec().into_iter().map(|e| (e.1, e.0)).collect()
    }
}

/// Per-query scoring state.
struct Query<'a> {
    scorers: Vec<TermScorer<'a>>,
    /// `k1 (1 - b + b · len / avgdl)` for every norm.
    k: [f32; 256],
    norms: &'a [u8],
}

impl TextFieldReader {
    /// This segment's statistics for `terms` (sum them over segments with
    /// [`CollectionStats::add`]).
    pub fn stats(&self, terms: &[String]) -> CollectionStats {
        CollectionStats {
            n_docs: self.n_docs() as u64,
            total_len: self.total_len(),
            df: terms.iter().map(|t| (t.clone(), self.doc_freq(t) as u64)).collect(),
        }
    }

    fn query<'a>(&'a self, terms: &[String], stats: &CollectionStats, params: Bm25Params) -> Query<'a> {
        let avgdl = stats.avgdl();
        let mut k = [0f32; 256];
        for (i, k) in k.iter_mut().enumerate() {
            *k = params.k1 * (1.0 - params.b + params.b * NORM_LENGTHS[i] as f32 / avgdl);
        }
        // Distinct terms in first-occurrence order, with multiplicities.
        let mut uniq: Vec<(&str, u32)> = Vec::new();
        for t in terms {
            match uniq.iter_mut().find(|(u, _)| *u == t.as_str()) {
                Some(e) => e.1 += 1,
                None => uniq.push((t, 1)),
            }
        }
        let mut scorers = Vec::new();
        for (t, m) in uniq {
            let Some(info) = self.term_info(t) else { continue };
            let df = stats.df.get(t).copied().unwrap_or(info.df as u64);
            let weight = m as f32 * stats.idf(df) * (params.k1 + 1.0);
            let cur = self.cursor(&info);
            let max = cur.frontier().iter().map(|&(tf, n)| weight * tf as f32 / (tf as f32 + k[n as usize])).fold(0.0, f32::max)
                * BOUND_SLACK;
            scorers.push(TermScorer { cur, weight, max, block_end: 0, block_max: 0.0 });
        }
        Query { scorers, k, norms: &self.norms }
    }

    /// Disjunctive BM25 top-`k` of the analysed `terms` (repeated terms
    /// count with multiplicity), skipping documents outside `filter`.
    /// Returns (doc, score) by decreasing score.
    pub fn top_k(
        &self,
        terms: &[String],
        stats: &CollectionStats,
        params: Bm25Params,
        k: usize,
        filter: Option<&dyn DocFilter>,
        algo: TopKAlgo,
    ) -> Vec<(DocId, f32)> {
        self.top_k_with(terms, stats, params, k, filter, algo, FilterStrategy::Auto)
    }

    /// [`top_k`](Self::top_k) with an explicit filter strategy (`Drive`
    /// needs a filter that lists its members, else it probes).
    #[allow(clippy::too_many_arguments)]
    pub fn top_k_with(
        &self,
        terms: &[String],
        stats: &CollectionStats,
        params: Bm25Params,
        k: usize,
        filter: Option<&dyn DocFilter>,
        algo: TopKAlgo,
        strategy: FilterStrategy,
    ) -> Vec<(DocId, f32)> {
        let mut q = self.query(terms, stats, params);
        let mut top = TopK::new(k);
        if k == 0 || q.scorers.is_empty() {
            return Vec::new();
        }
        if let Some(f) = filter {
            let drive = match strategy {
                FilterStrategy::Drive => true,
                FilterStrategy::Probe => false,
                FilterStrategy::Auto => {
                    let postings: f64 = q.scorers.iter().map(|s| s.cur.len() as f64).sum();
                    f.cardinality() as f64 * q.scorers.len() as f64 * drive_cost(algo) < postings
                }
            };
            if drive {
                if let Some(members) = f.members() {
                    q.drive(members, &mut top);
                    return top.into_sorted();
                }
            }
        }
        match algo {
            TopKAlgo::Exhaustive => q.exhaustive(&mut top, filter),
            TopKAlgo::MaxScore => q.max_score(&mut top, filter),
            TopKAlgo::BlockMaxWand => q.block_max_wand(&mut top, filter),
        }
        top.into_sorted()
    }

    /// Exact BM25 of each of `docs` (0 for documents without the terms), in
    /// the order given; sorted input reads each list once.
    pub fn score(&self, terms: &[String], stats: &CollectionStats, params: Bm25Params, docs: &[DocId]) -> Vec<f32> {
        let mut out = vec![0f32; docs.len()];
        let mut idx: Vec<usize> = (0..docs.len()).collect();
        idx.sort_by_key(|&i| docs[i]);
        let mut q = self.query(terms, stats, params);
        for i in idx {
            let d = docs[i];
            if (d as usize) < q.norms.len() {
                out[i] = q.score_doc(d).0;
            }
        }
        out
    }
}

impl Query<'_> {
    /// Score of `d` (0 if no list contains it), seeking every list to it
    /// (`d` must not decrease between calls); and whether any list
    /// contains it, and any list has postings left.
    #[inline]
    fn score_doc(&mut self, d: DocId) -> (f32, bool, bool) {
        let norm = self.norms[d as usize];
        let (mut s, mut any, mut live) = (0.0, false, false);
        for sc in &mut self.scorers {
            let x = sc.cur.seek(d);
            if x == d {
                s += sc.score(&self.k, norm);
                any = true;
            }
            live |= x != END;
        }
        (s, any, live)
    }

    fn drive(&mut self, members: impl Iterator<Item = DocId>, top: &mut TopK) {
        for d in members {
            let (s, any, live) = self.score_doc(d);
            if any {
                top.insert(d, s);
            }
            if !live {
                break;
            }
        }
    }

    fn exhaustive(&mut self, top: &mut TopK, filter: Option<&dyn DocFilter>) {
        let sc = &mut self.scorers;
        // Current docIDs, dense, so that finding the next candidate does not
        // touch the (large) cursors.
        let mut docs: Vec<DocId> = sc.iter().map(|s| s.cur.doc()).collect();
        loop {
            let d = docs.iter().copied().min().unwrap_or(END);
            if d == END {
                break;
            }
            if filter.is_none_or(|f| f.contains(d)) {
                let norm = self.norms[d as usize];
                let mut s = 0.0;
                for (i, x) in docs.iter_mut().enumerate() {
                    if *x == d {
                        s += sc[i].score(&self.k, norm);
                        *x = sc[i].cur.next();
                    }
                }
                top.insert(d, s);
            } else {
                for (i, x) in docs.iter_mut().enumerate() {
                    if *x == d {
                        *x = sc[i].cur.next();
                    }
                }
            }
        }
    }

    fn max_score(&mut self, top: &mut TopK, filter: Option<&dyn DocFilter>) {
        let sc = &mut self.scorers;
        sc.sort_by(|a, b| a.max.total_cmp(&b.max));
        let n = sc.len();
        // ub[i]: the most lists 0..=i can add.
        let ub: Vec<f32> = sc
            .iter()
            .scan(0.0f32, |acc, s| {
                *acc += s.max;
                Some(*acc)
            })
            .collect();
        let mut docs: Vec<DocId> = sc.iter().map(|s| s.cur.doc()).collect();
        // Lists [0, ne) are non-essential.
        let mut ne = 0;
        loop {
            let theta = top.threshold();
            while ne < n && ub[ne] <= theta {
                ne += 1;
            }
            if ne == n {
                break;
            }
            let d = docs[ne..].iter().copied().min().unwrap();
            if d == END {
                break;
            }
            if filter.is_some_and(|f| !f.contains(d)) {
                for i in ne..n {
                    if docs[i] == d {
                        docs[i] = sc[i].cur.next();
                    }
                }
                continue;
            }
            let norm = self.norms[d as usize];
            let mut s = 0.0;
            for i in ne..n {
                if docs[i] == d {
                    s += sc[i].score(&self.k, norm);
                    docs[i] = sc[i].cur.next();
                }
            }
            let mut alive = true;
            for i in (0..ne).rev() {
                if s + ub[i] <= theta {
                    alive = false;
                    break;
                }
                if docs[i] < d {
                    docs[i] = sc[i].cur.seek(d);
                }
                if docs[i] == d {
                    s += sc[i].score(&self.k, norm);
                }
            }
            if alive {
                top.insert(d, s);
            }
        }
    }

    fn block_max_wand(&mut self, top: &mut TopK, filter: Option<&dyn DocFilter>) {
        let sc = &mut self.scorers;
        let n = sc.len();
        let mut docs: Vec<DocId> = sc.iter().map(|s| s.cur.doc()).collect();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| docs[i]);
        loop {
            let theta = top.threshold();
            // Pivot: the first list at which the bounds so far exceed theta.
            let mut upper = 0.0f32;
            let mut pivot = n;
            for (p, &i) in order.iter().enumerate() {
                if docs[i] == END {
                    break;
                }
                upper += sc[i].max;
                if upper > theta {
                    pivot = p;
                    break;
                }
            }
            if pivot == n {
                break;
            }
            let pivot_doc = docs[order[pivot]];
            while pivot + 1 < n && docs[order[pivot + 1]] == pivot_doc {
                pivot += 1;
            }
            // Refine with the bounds of the blocks holding pivot_doc.
            let mut block_upper = 0.0f32;
            for &i in &order[..=pivot] {
                block_upper += sc[i].block_max(&self.k, pivot_doc);
            }
            if block_upper > theta {
                if docs[order[0]] == pivot_doc {
                    // Every list up to the pivot is on pivot_doc. Score it,
                    // replacing block bounds by scores while it can qualify.
                    if filter.is_none_or(|f| f.contains(pivot_doc)) {
                        let norm = self.norms[pivot_doc as usize];
                        let (mut s, mut bound) = (0.0, block_upper);
                        for &i in &order[..=pivot] {
                            let x = sc[i].score(&self.k, norm);
                            s += x;
                            bound -= sc[i].block_max - x;
                            if bound <= theta {
                                break;
                            }
                        }
                        top.insert(pivot_doc, s);
                    }
                    for &i in &order[..=pivot] {
                        docs[i] = sc[i].cur.next();
                    }
                    insertion_sort(&mut order, |i| docs[i]);
                } else {
                    // Move the last list before the pivot's run up to it.
                    let mut p = pivot;
                    while docs[order[p]] == pivot_doc {
                        p -= 1;
                    }
                    docs[order[p]] = sc[order[p]].cur.seek(pivot_doc);
                    bubble(&mut order, p, |i| docs[i]);
                }
            } else {
                // No document up to the end of the smallest of these blocks
                // can qualify: skip past it (or to the next list's docID).
                let mut next = END as u64;
                for &i in &order[..=pivot] {
                    next = next.min(sc[i].block_end);
                }
                let mut next = next as DocId;
                if pivot + 1 < n {
                    next = next.min(docs[order[pivot + 1]]);
                }
                if next <= pivot_doc {
                    next = pivot_doc + 1;
                }
                // Advance the list with the largest bound among them.
                let p = (0..=pivot).max_by(|&a, &b| sc[order[a]].max.total_cmp(&sc[order[b]].max)).unwrap();
                docs[order[p]] = sc[order[p]].cur.seek(next);
                bubble(&mut order, p, |i| docs[i]);
            }
        }
    }
}

/// Sorts the (nearly sorted) list order by docID.
#[inline]
fn insertion_sort(order: &mut [usize], doc: impl Fn(usize) -> DocId) {
    for i in 1..order.len() {
        let mut j = i;
        while j > 0 && doc(order[j - 1]) > doc(order[j]) {
            order.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// Restores docID order after the list at `order[p]` moved forward.
#[inline]
fn bubble(order: &mut [usize], mut p: usize, doc: impl Fn(usize) -> DocId) {
    while p + 1 < order.len() && doc(order[p]) > doc(order[p + 1]) {
        order.swap(p, p + 1);
        p += 1;
    }
}
