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
//! The same machinery ranks learned sparse fields ([`crate::text::sparse`])
//! by the dot product `Σ_t q_t · w_t`: there a posting's frequency is the
//! document's quantised weight for the term, and a term scores linearly
//! (`weight · tf`) instead of saturating.
//!
//! All three algorithms visit documents in docID order and keep the first of
//! equal-scored documents, so they return the same top-k up to float
//! rounding (sums are taken in different orders).
//!
//! The same loops can rank by an expression of the text score and each
//! document's own values ([`Boost`], `bm25(..) * (1 + 0.2 * importance)`):
//! the expression's bound turns the threshold into a text score that a
//! document must exceed ([`Final::cut`]), so that lists and blocks are
//! skipped as for the text score alone, and the top k stays exact.

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

/// Entries a top-k heap reserves up front; it grows past this only with
/// what it holds, so a huge `k` costs nothing until rows arrive.
const HEAP_RESERVE: usize = 4096;

// A document's score is the sum of its terms' scores, added in f64: exact
// for terms of any plausible spread, so a score does not depend on the
// order in which a traversal met the terms (one segment and two shards,
// or two algorithms, give a document the same f32 score, and tie alike).

/// One query term: its cursor, weight `m · idf · (k1 + 1)` and list bound.
/// A posting scores `weight · tf / (sat · tf + k[norm])`: BM25 with `sat`
/// 1, a linear `weight · tf` with `sat` 0 and every `k` 1 (sparse fields).
struct TermScorer<'a> {
    cur: PostingCursor<'a>,
    weight: f32,
    sat: f32,
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
        self.weight * tf / (self.sat * tf + k[norm as usize])
    }

    #[inline]
    fn bound(&self, k: &[f32; 256], imp: Impact) -> f32 {
        if imp == LIST_IMPACT {
            return self.max;
        }
        let tf = (imp >> 8) as f32;
        self.weight * tf / (self.sat * tf + k[(imp & 0xff) as usize]) * BOUND_SLACK
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

/// A score a top-k keeps: a text score (f32), or an expression of one
/// (f64).
trait Key: Copy + PartialOrd {
    const NEG_INFINITY: Self;
    fn total_cmp(&self, o: &Self) -> Ordering;
    fn next_down(self) -> Self;
}

impl Key for f32 {
    const NEG_INFINITY: f32 = f32::NEG_INFINITY;
    fn total_cmp(&self, o: &f32) -> Ordering {
        f32::total_cmp(self, o)
    }
    fn next_down(self) -> f32 {
        f32::next_down(self)
    }
}

impl Key for f64 {
    const NEG_INFINITY: f64 = f64::NEG_INFINITY;
    fn total_cmp(&self, o: &f64) -> Ordering {
        f64::total_cmp(self, o)
    }
    fn next_down(self) -> f64 {
        f64::next_down(self)
    }
}

/// The best `k` (score, doc) pairs; ties keep the earlier document, or,
/// with `ties`, every document tied with the `k`-th stays as well.
struct TopK<'g, S = f32> {
    k: usize,
    heap: BinaryHeap<Entry<S>>,
    /// Pairs left out of the heap with the score of its worst, when kept.
    ties: Option<Vec<Entry<S>>>,
    /// With a cap on each group's rows, the rows kept instead of the heap
    /// ([`TopK::capped`]).
    cap: Option<Cap<'g, S>>,
}

/// Rows a capped top-k compacts at the least ([`Cap::limit`]).
const CAP_MIN: usize = 1024;

/// The rows of a top-k that keeps at most `per` rows of each group: every
/// row that can be among the best `k` once each group keeps its best `per`,
/// ties at either cut broken later (by key, which a segment does not
/// order by). Rows come in as scored; [`Cap::compact`] drops the rest.
struct Cap<'g, S> {
    per: usize,
    /// A row's group; None for rows without a value, which are one group.
    group: &'g dyn Fn(DocId) -> Option<u64>,
    rows: Vec<(S, DocId, Option<u64>)>,
    /// `rows` is compacted when it reaches this many.
    limit: usize,
    /// The score of the `k`-th row counted (each group's best `per`) at the
    /// last compaction: rows below it cannot be among the best `k`.
    theta: S,
    /// The groups with `per` rows at the last compaction, and the score of
    /// each one's `per`-th: rows of the group below it are out.
    full: HashMap<Option<u64>, S>,
}

impl<S: Key> Cap<'_, S> {
    #[inline]
    fn insert(&mut self, doc: DocId, score: S, k: usize) {
        if score.total_cmp(&self.theta).is_lt() {
            return;
        }
        let g = (self.group)(doc);
        if self.full.get(&g).is_some_and(|w| score.total_cmp(w).is_lt()) {
            return;
        }
        self.rows.push((score, doc, g));
        if self.rows.len() >= self.limit {
            self.compact(k);
            self.limit = self.rows.len().saturating_mul(2).max(k.saturating_mul(2)).max(CAP_MIN);
        }
    }

    /// Keeps, best first, each group's best `per` rows and the rows tied
    /// with its `per`-th, down to the `k`-th row counted (each group's
    /// first `per`) and the rows tied with it.
    fn compact(&mut self, k: usize) {
        self.rows.sort_unstable_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let per = self.per;
        let mut seen: HashMap<Option<u64>, (usize, S)> = HashMap::new();
        let (mut counted, mut theta) = (0usize, S::NEG_INFINITY);
        self.rows.retain(|&(s, _, g)| {
            if counted >= k && s.total_cmp(&theta).is_lt() {
                return false;
            }
            let e = seen.entry(g).or_insert((0, s));
            if e.0 < per {
                e.0 += 1;
                e.1 = s;
                counted += 1;
                if counted == k {
                    theta = s;
                }
                true
            } else {
                s.total_cmp(&e.1).is_eq()
            }
        });
        if counted >= k {
            self.theta = theta;
        }
        self.full = seen.into_iter().filter(|(_, (n, _))| *n >= per).map(|(g, (_, s))| (g, s)).collect();
    }
}

/// Heap order: the greatest entry is the worst (lowest score, then highest
/// docID), so `peek` is the threshold.
#[derive(PartialEq)]
struct Entry<S>(S, DocId);

impl<S: Key> Eq for Entry<S> {}

impl<S: Key> Ord for Entry<S> {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.total_cmp(&self.0).then(self.1.cmp(&o.1))
    }
}

impl<S: Key> PartialOrd for Entry<S> {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl<'g, S: Key> TopK<'g, S> {
    fn new(k: usize) -> Self {
        TopK { k, heap: BinaryHeap::with_capacity(k.saturating_add(1).min(HEAP_RESERVE)), ties: None, cap: None }
    }

    fn with_ties(k: usize) -> Self {
        TopK { ties: Some(Vec::new()), ..TopK::new(k) }
    }

    /// The rows that can be among the best `k` once each group (`group` of
    /// a document) keeps only its best `per` ([`Cap`]).
    fn capped(k: usize, per: usize, group: &'g dyn Fn(DocId) -> Option<u64>) -> Self {
        let cap = Cap { per: per.max(1), group, rows: Vec::new(), limit: k.saturating_mul(2).max(CAP_MIN), theta: S::NEG_INFINITY, full: HashMap::new() };
        TopK { k, heap: BinaryHeap::new(), ties: None, cap: Some(cap) }
    }

    /// Scores must exceed this to enter (to be kept, with ties).
    #[inline]
    fn threshold(&self) -> S {
        if let Some(c) = &self.cap {
            // Rows tied with the cut are kept.
            return c.theta.next_down();
        }
        if self.heap.len() < self.k {
            return S::NEG_INFINITY;
        }
        let worst = self.heap.peek().map_or(S::NEG_INFINITY, |e| e.0);
        // A score equal to the worst is kept with ties, so only lower
        // bounds may be skipped.
        if self.ties.is_some() { worst.next_down() } else { worst }
    }

    #[inline]
    fn insert(&mut self, doc: DocId, score: S) {
        if let Some(c) = &mut self.cap {
            c.insert(doc, score, self.k);
            return;
        }
        if self.heap.len() < self.k {
            self.heap.push(Entry(score, doc));
            return;
        }
        let Some(worst) = self.heap.peek().map(|e| e.0) else { return };
        if score > worst {
            let out = self.heap.pop().expect("a full heap");
            self.heap.push(Entry(score, doc));
            if let Some(ties) = &mut self.ties {
                if self.heap.peek().is_some_and(|e| e.0 == out.0) {
                    ties.push(out);
                } else {
                    ties.clear();
                }
            }
        } else if score == worst {
            if let Some(ties) = &mut self.ties {
                ties.push(Entry(score, doc));
            }
        }
    }

    /// By decreasing score, the kept ties last (capped: by decreasing
    /// score, then document).
    fn into_sorted(self) -> Vec<(DocId, S)> {
        if let Some(mut c) = self.cap {
            c.compact(self.k);
            return c.rows.into_iter().map(|(s, d, _)| (d, s)).collect();
        }
        let mut v = self.heap.into_sorted_vec();
        v.extend(self.ties.unwrap_or_default());
        v.into_iter().map(|e| (e.1, e.0)).collect()
    }
}

/// What a top-k ranks documents by, given each one's text score: the text
/// score itself ([`Text`]) or an expression of it ([`Boosted`]).
trait Final {
    type S: Key;
    /// Whether a document whose bound fell to the threshold midway is
    /// still offered with the part of its text score summed so far (which
    /// cannot enter). An expression is offered only whole scores.
    const PARTIAL: bool;
    /// The score of document `d` whose text score is `text`.
    fn value(&mut self, d: DocId, text: f32) -> Self::S;
    /// A text score at or below which no document scores above `theta`:
    /// the threshold the loops compare text bounds with.
    fn cut(&mut self, theta: Self::S) -> f32;
}

/// Ranks by the text score.
struct Text;

impl Final for Text {
    type S = f32;
    const PARTIAL: bool = true;
    #[inline]
    fn value(&mut self, _: DocId, text: f32) -> f32 {
        text
    }
    #[inline]
    fn cut(&mut self, theta: f32) -> f32 {
        theta
    }
}

/// The value a top-k ranks a document by, from its text score (BM25 or a
/// sparse dot product) and the document's own values, with an upper bound
/// that lets the top-k skip what cannot reach its `k`-th value: a rank-safe
/// boost such as `bm25(body, :q) * (1 + 0.2 * importance)`
/// ([`crate::query::rank::Rank::driver`]).
pub trait Boost {
    /// Document `d`'s value when its text score is `text`; NaN for NULL,
    /// which ranks below every number.
    fn score(&mut self, d: DocId, text: f32) -> f64;
    /// An upper bound of the values of the segment's documents whose text
    /// score is at most `text` (`f64::MIN` where each is NULL).
    fn bound(&mut self, text: f32) -> f64;
}

/// A cut is lowered by this fraction of itself, so that the float rounding
/// of a text bound (a sum of list or block bounds) never prunes a document
/// above it.
const CUT_SLACK: f32 = 1e-4;

/// Ranks by a [`Boost`] of the text score.
struct Boosted<'b, B> {
    boost: &'b mut B,
    /// The largest text score of the segment: its lists' bounds summed.
    max: f32,
    /// The last threshold asked for and its cut.
    theta: f64,
    cut: f32,
    /// The largest text score found whose bound is at most an earlier
    /// threshold (thresholds only rise), or -1.
    ok: f32,
}

impl<'b, B: Boost> Boosted<'b, B> {
    fn new(boost: &'b mut B, max: f32) -> Self {
        Boosted { boost, max, theta: f64::NEG_INFINITY, cut: f32::NEG_INFINITY, ok: -1.0 }
    }

    /// Whether a bound narrows as the text score falls, so that a
    /// threshold can skip documents: else every matching document is
    /// scored. A bound may be flat over the higher text scores and fall
    /// only below them, as a model's is above its highest split on the
    /// text score.
    fn narrows(&mut self) -> bool {
        let top = self.boost.bound(self.max);
        top < f64::INFINITY && (self.boost.bound(self.max * 0.5) < top || self.boost.bound(0.0) < top)
    }

    /// The largest text score whose bound is at most `theta`, found by
    /// bisection from the last one (a bound need not be monotone, so only
    /// scores whose bound was computed are taken).
    fn search(&mut self, theta: f64) -> f32 {
        if self.boost.bound(self.max) <= theta {
            return self.max;
        }
        let mut lo = self.ok;
        if lo < 0.0 && self.boost.bound(0.0) <= theta {
            lo = 0.0;
        }
        if lo < 0.0 {
            return lo;
        }
        let mut hi = self.max;
        while hi - lo > hi * CUT_SLACK {
            let mid = lo + (hi - lo) * 0.5;
            if self.boost.bound(mid) <= theta {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

impl<B: Boost> Final for Boosted<'_, B> {
    type S = f64;
    const PARTIAL: bool = false;
    #[inline]
    fn value(&mut self, d: DocId, text: f32) -> f64 {
        let v = self.boost.score(d, text);
        if v.is_nan() { f64::NEG_INFINITY } else { v }
    }
    #[inline]
    fn cut(&mut self, theta: f64) -> f32 {
        if theta != self.theta {
            self.theta = theta;
            if theta > f64::NEG_INFINITY {
                self.ok = self.search(theta);
                self.cut = if self.ok < 0.0 { f32::NEG_INFINITY } else { self.ok - self.ok * CUT_SLACK };
            }
        }
        self.cut
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
            n_docs: self.n_docs_with_terms() as u64,
            total_len: self.total_len(),
            df: terms.iter().map(|t| (t.clone(), self.doc_freq(t) as u64)).collect(),
        }
    }

    fn query<'a>(&'a self, terms: &[String], stats: &CollectionStats, params: Bm25Params) -> Query<'a> {
        let w: Vec<(&str, f32)> = terms.iter().map(|t| (t.as_str(), 1.0)).collect();
        self.query_weighted(&w, stats, params)
    }

    /// BM25 with a weight per term (repeated terms add up): a term scores
    /// `weight · idf · (k1 + 1) · tf / (tf + k)`.
    fn query_weighted<'a>(&'a self, terms: &[(&str, f32)], stats: &CollectionStats, params: Bm25Params) -> Query<'a> {
        let avgdl = stats.avgdl();
        let mut k = [0f32; 256];
        for (i, k) in k.iter_mut().enumerate() {
            *k = params.k1 * (1.0 - params.b + params.b * NORM_LENGTHS[i] as f32 / avgdl);
        }
        // Distinct terms in first-occurrence order, with summed weights.
        let mut uniq: Vec<(&str, f32)> = Vec::new();
        for &(t, w) in terms {
            match uniq.iter_mut().find(|(u, _)| *u == t) {
                Some(e) => e.1 += w,
                None => uniq.push((t, w)),
            }
        }
        let mut scorers = Vec::new();
        for (t, m) in uniq {
            if m <= 0.0 {
                continue;
            }
            let Some(info) = self.term_info(t) else { continue };
            let df = stats.df.get(t).copied().unwrap_or(info.df as u64);
            let weight = m * stats.idf(df) * (params.k1 + 1.0);
            let cur = self.cursor(&info);
            let max = cur.frontier().iter().map(|&(tf, n)| weight * tf as f32 / (tf as f32 + k[n as usize])).fold(0.0, f32::max)
                * BOUND_SLACK;
            scorers.push(TermScorer { cur, weight, sat: 1.0, max, block_end: 0, block_max: 0.0 });
        }
        Query { scorers, k, norms: &self.norms }
    }

    /// The linear query of a sparse field: `terms` with their query
    /// weights (repeated terms add up; weights at or below 0 are dropped).
    fn query_dot<'a>(&'a self, terms: &[(String, f32)]) -> Query<'a> {
        let mut uniq: Vec<(&str, f32)> = Vec::new();
        for (t, w) in terms {
            match uniq.iter_mut().find(|(u, _)| *u == t.as_str()) {
                Some(e) => e.1 += w,
                None => uniq.push((t, *w)),
            }
        }
        let mut scorers = Vec::new();
        for (t, w) in uniq {
            if w <= 0.0 {
                continue;
            }
            let Some(info) = self.term_info(t) else { continue };
            let weight = w / super::sparse::SCALE;
            let cur = self.cursor(&info);
            let max = cur.frontier().iter().map(|&(tf, _)| weight * tf as f32).fold(0.0, f32::max) * BOUND_SLACK;
            scorers.push(TermScorer { cur, weight, sat: 0.0, max, block_end: 0, block_max: 0.0 });
        }
        Query { scorers, k: [1.0; 256], norms: &self.norms }
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
        self.run(self.query(terms, stats, params), TopK::new(k), &mut Text, filter, algo, strategy)
    }

    /// [`top_k`](Self::top_k), plus every document whose score equals the
    /// `k`-th's, in no particular order after the others: all candidates
    /// for a cut at `k` that breaks ties by something else (a key).
    pub fn top_k_ties(
        &self,
        terms: &[String],
        stats: &CollectionStats,
        params: Bm25Params,
        k: usize,
        filter: Option<&dyn DocFilter>,
        algo: TopKAlgo,
    ) -> Vec<(DocId, f32)> {
        self.run(self.query(terms, stats, params), TopK::with_ties(k), &mut Text, filter, algo, FilterStrategy::Auto)
    }

    /// Top-`k` (with `ties`, plus every document tied with the `k`-th) by
    /// BM25 with a weight per term ([`top_k`](Self::top_k) weighs each 1).
    #[allow(clippy::too_many_arguments)]
    pub fn top_k_weighted(
        &self,
        terms: &[(String, f32)],
        stats: &CollectionStats,
        params: Bm25Params,
        k: usize,
        filter: Option<&dyn DocFilter>,
        algo: TopKAlgo,
        ties: bool,
    ) -> Vec<(DocId, f32)> {
        let w: Vec<(&str, f32)> = terms.iter().map(|(t, w)| (t.as_str(), *w)).collect();
        let top = if ties { TopK::with_ties(k) } else { TopK::new(k) };
        self.run(self.query_weighted(&w, stats, params), top, &mut Text, filter, algo, FilterStrategy::Auto)
    }

    /// The documents that can be among the best `k` by weighted BM25
    /// ([`top_k_weighted`](Self::top_k_weighted)) once each group (`group`
    /// of a document, None for the documents without a value) keeps only
    /// its best `per`: each group's best `per`
    /// and the documents tied with its `per`-th, down to the score of the
    /// `k`-th so counted, documents tied with it kept; by decreasing score,
    /// then document. Whatever order breaks the ties, the best `k` capped
    /// rows of one segment or of many (the union of each one's) are among
    /// them.
    #[allow(clippy::too_many_arguments)]
    pub fn top_k_capped(
        &self,
        terms: &[(String, f32)],
        stats: &CollectionStats,
        params: Bm25Params,
        k: usize,
        per: usize,
        group: &dyn Fn(DocId) -> Option<u64>,
        filter: Option<&dyn DocFilter>,
        algo: TopKAlgo,
    ) -> Vec<(DocId, f32)> {
        let w: Vec<(&str, f32)> = terms.iter().map(|(t, w)| (t.as_str(), *w)).collect();
        self.run(self.query_weighted(&w, stats, params), TopK::capped(k, per, group), &mut Text, filter, algo, FilterStrategy::Auto)
    }

    /// The top `k` documents, and every document tied with the `k`-th, by
    /// `boost` of their weighted BM25 ([`top_k_weighted`](Self::top_k_weighted)),
    /// best first, and the algorithm that ran: `algo` where the boost's
    /// bound narrows with BM25, else every matching document is scored
    /// ([`TopKAlgo::Exhaustive`]). Exact: no document left out scores above
    /// the `k`-th.
    #[allow(clippy::too_many_arguments)]
    pub fn top_k_boosted<B: Boost>(
        &self,
        terms: &[(String, f32)],
        stats: &CollectionStats,
        params: Bm25Params,
        k: usize,
        filter: Option<&dyn DocFilter>,
        algo: TopKAlgo,
        boost: &mut B,
    ) -> (Vec<(DocId, f64)>, TopKAlgo) {
        let w: Vec<(&str, f32)> = terms.iter().map(|(t, w)| (t.as_str(), *w)).collect();
        self.run_boosted(self.query_weighted(&w, stats, params), k, filter, algo, boost)
    }

    /// [`top_k_boosted`](Self::top_k_boosted) of a sparse field's dot
    /// product with `terms` ([`top_k_dot`](Self::top_k_dot)).
    pub fn top_k_dot_boosted<B: Boost>(
        &self,
        terms: &[(String, f32)],
        k: usize,
        filter: Option<&dyn DocFilter>,
        algo: TopKAlgo,
        boost: &mut B,
    ) -> (Vec<(DocId, f64)>, TopKAlgo) {
        self.run_boosted(self.query_dot(terms), k, filter, algo, boost)
    }

    fn run_boosted<B: Boost>(&self, q: Query<'_>, k: usize, filter: Option<&dyn DocFilter>, algo: TopKAlgo, boost: &mut B) -> (Vec<(DocId, f64)>, TopKAlgo) {
        let max = q.scorers.iter().map(|s| s.max).sum::<f32>();
        let mut f = Boosted::new(boost, max);
        let algo = if f.narrows() { algo } else { TopKAlgo::Exhaustive };
        (self.run(q, TopK::with_ties(k), &mut f, filter, algo, FilterStrategy::Auto), algo)
    }

    /// Exact weighted BM25 of `docs` (see [`top_k_weighted`](Self::top_k_weighted)).
    pub fn score_weighted(&self, terms: &[(String, f32)], stats: &CollectionStats, params: Bm25Params, docs: &[DocId]) -> Vec<f32> {
        let w: Vec<(&str, f32)> = terms.iter().map(|(t, w)| (t.as_str(), *w)).collect();
        self.score_with(self.query_weighted(&w, stats, params), docs)
    }

    /// Top-`k` of a sparse field by the dot product of each document's
    /// term weights with `terms`' (see [`crate::text::sparse`]); filters as
    /// in [`top_k_with`](Self::top_k_with).
    pub fn top_k_dot(&self, terms: &[(String, f32)], k: usize, filter: Option<&dyn DocFilter>, algo: TopKAlgo, strategy: FilterStrategy) -> Vec<(DocId, f32)> {
        self.run(self.query_dot(terms), TopK::new(k), &mut Text, filter, algo, strategy)
    }

    /// Exact dot product of a sparse field's documents `docs` with `terms`.
    pub fn score_dot(&self, terms: &[(String, f32)], docs: &[DocId]) -> Vec<f32> {
        self.score_with(self.query_dot(terms), docs)
    }

    fn run<F: Final>(&self, mut q: Query<'_>, mut top: TopK<'_, F::S>, f: &mut F, filter: Option<&dyn DocFilter>, algo: TopKAlgo, strategy: FilterStrategy) -> Vec<(DocId, F::S)> {
        if top.k == 0 || q.scorers.is_empty() {
            return Vec::new();
        }
        if let Some(filt) = filter {
            let drive = match strategy {
                FilterStrategy::Drive => true,
                FilterStrategy::Probe => false,
                FilterStrategy::Auto => {
                    let postings: f64 = q.scorers.iter().map(|s| s.cur.len() as f64).sum();
                    filt.cardinality() as f64 * q.scorers.len() as f64 * drive_cost(algo) < postings
                }
            };
            if drive {
                if let Some(members) = filt.members() {
                    q.drive(members, &mut top, f);
                    return top.into_sorted();
                }
            }
        }
        match algo {
            TopKAlgo::Exhaustive => q.exhaustive(&mut top, f, filter),
            TopKAlgo::MaxScore => q.max_score(&mut top, f, filter),
            TopKAlgo::BlockMaxWand => q.block_max_wand(&mut top, f, filter),
        }
        top.into_sorted()
    }

    /// Exact BM25 of each of `docs` (0 for documents without the terms), in
    /// the order given; sorted input reads each list once.
    pub fn score(&self, terms: &[String], stats: &CollectionStats, params: Bm25Params, docs: &[DocId]) -> Vec<f32> {
        self.score_with(self.query(terms, stats, params), docs)
    }

    fn score_with(&self, mut q: Query<'_>, docs: &[DocId]) -> Vec<f32> {
        let mut out = vec![0f32; docs.len()];
        let mut idx: Vec<usize> = (0..docs.len()).collect();
        idx.sort_by_key(|&i| docs[i]);
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
        let (mut s, mut any, mut live) = (0.0f64, false, false);
        for sc in &mut self.scorers {
            let x = sc.cur.seek(d);
            if x == d {
                s += sc.score(&self.k, norm) as f64;
                any = true;
            }
            live |= x != END;
        }
        (s as f32, any, live)
    }

    fn drive<F: Final>(&mut self, members: impl Iterator<Item = DocId>, top: &mut TopK<'_, F::S>, f: &mut F) {
        for d in members {
            let (s, any, live) = self.score_doc(d);
            if any {
                top.insert(d, f.value(d, s));
            }
            if !live {
                break;
            }
        }
    }

    fn exhaustive<F: Final>(&mut self, top: &mut TopK<'_, F::S>, f: &mut F, filter: Option<&dyn DocFilter>) {
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
                let mut s = 0.0f64;
                for (i, x) in docs.iter_mut().enumerate() {
                    if *x == d {
                        s += sc[i].score(&self.k, norm) as f64;
                        *x = sc[i].cur.next();
                    }
                }
                top.insert(d, f.value(d, s as f32));
            } else {
                for (i, x) in docs.iter_mut().enumerate() {
                    if *x == d {
                        *x = sc[i].cur.next();
                    }
                }
            }
        }
    }

    fn max_score<F: Final>(&mut self, top: &mut TopK<'_, F::S>, f: &mut F, filter: Option<&dyn DocFilter>) {
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
            let theta = f.cut(top.threshold());
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
            let mut s = 0.0f64;
            for i in ne..n {
                if docs[i] == d {
                    s += sc[i].score(&self.k, norm) as f64;
                    docs[i] = sc[i].cur.next();
                }
            }
            let mut alive = true;
            for i in (0..ne).rev() {
                if s as f32 + ub[i] <= theta {
                    alive = false;
                    break;
                }
                if docs[i] < d {
                    docs[i] = sc[i].cur.seek(d);
                }
                if docs[i] == d {
                    s += sc[i].score(&self.k, norm) as f64;
                }
            }
            if alive {
                top.insert(d, f.value(d, s as f32));
            }
        }
    }

    fn block_max_wand<F: Final>(&mut self, top: &mut TopK<'_, F::S>, f: &mut F, filter: Option<&dyn DocFilter>) {
        let sc = &mut self.scorers;
        let n = sc.len();
        let mut docs: Vec<DocId> = sc.iter().map(|s| s.cur.doc()).collect();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| docs[i]);
        loop {
            let theta = f.cut(top.threshold());
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
                        let (mut s, mut bound, mut whole) = (0.0f64, block_upper, true);
                        for &i in &order[..=pivot] {
                            let x = sc[i].score(&self.k, norm);
                            s += x as f64;
                            bound -= sc[i].block_max - x;
                            if bound <= theta {
                                whole = false;
                                break;
                            }
                        }
                        if whole || F::PARTIAL {
                            top.insert(pivot_doc, f.value(pivot_doc, s as f32));
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    /// The first `k` rows with at most `per` of each group, in the order
    /// of decreasing score with ties broken by `rank` (any order a caller
    /// breaks them by).
    fn exact(rows: &[(f32, DocId, Option<u64>)], k: usize, per: usize, rank: &[u32]) -> Vec<DocId> {
        let mut v = rows.to_vec();
        v.sort_by(|a, b| b.0.total_cmp(&a.0).then(rank[a.1 as usize].cmp(&rank[b.1 as usize])));
        let mut seen: HashMap<Option<u64>, usize> = HashMap::new();
        v.iter()
            .filter(|r| {
                let n = seen.entry(r.2).or_default();
                *n += 1;
                *n <= per
            })
            .take(k)
            .map(|r| r.1)
            .collect()
    }

    #[test]
    fn a_capped_top_k_keeps_every_row_a_tie_order_can_pick() {
        let mut g = rand::rngs::StdRng::seed_from_u64(5);
        for case in 0..60 {
            let n = [50, 3000, 9000][case % 3];
            let groups = [1u64, 3, 40, 2000][case % 4];
            // Few distinct scores, so that many rows tie at either cut.
            let scores = [4u32, 30, 1000][case % 3];
            let rows: Vec<(f32, DocId, Option<u64>)> = (0..n as DocId)
                .map(|d| {
                    let skewed = (g.gen::<f64>().powi(3) * groups as f64) as u64;
                    let group = if g.gen_bool(0.1) { None } else { Some(skewed) };
                    (g.gen_range(0..scores) as f32 / 7.0, d, group)
                })
                .collect();
            let group_of: HashMap<DocId, Option<u64>> = rows.iter().map(|r| (r.1, r.2)).collect();
            let group = |d: DocId| group_of[&d];
            let (k, per) = (g.gen_range(1..60), g.gen_range(1..4));
            let mut top = TopK::capped(k, per, &group);
            let mut order: Vec<usize> = (0..rows.len()).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, g.gen_range(0..=i));
            }
            for &i in &order {
                if rows[i].0 > top.threshold() {
                    top.insert(rows[i].1, rows[i].0);
                }
            }
            let kept = top.into_sorted();
            assert!(kept.windows(2).all(|w| w[0].1 >= w[1].1), "case {case}: best first");
            let got: std::collections::HashSet<DocId> = kept.iter().map(|r| r.0).collect();
            for _ in 0..8 {
                let mut rank: Vec<u32> = (0..n as u32).collect();
                for i in (1..rank.len()).rev() {
                    rank.swap(i, g.gen_range(0..=i));
                }
                for d in exact(&rows, k, per, &rank) {
                    assert!(got.contains(&d), "case {case} (k {k}, per {per}): row {d} missing");
                }
            }
        }
    }
}
