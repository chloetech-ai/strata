//! Query execution (DESIGN.md §7): runs a bound [`Select`] against a storage
//! snapshot, segment by segment, following the planner's choices, and
//! merges the per-segment results.

mod graph;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::forward::{self, DocStore, KeyRange, KeywordColumn, NumericColumn, RangeIndex, RangeStrategy};
use crate::graph::GraphReader;
use crate::query::fusion::{self, Norm};
use crate::query::agg;
use crate::query::memory::{Charge, MemoryBudget};
use crate::query::logical::{Decay, Expand, OrderBy, Opts, Pred, Projection, Score, Select, Tiebreak};
use crate::query::plan::{CostModel, Estimator, FilterMode, Planner, VectorStrategy};
use crate::query::stats::{FieldStats, TermStats};
use crate::query::logical::Seeds;
use crate::schema::{FieldType, Schema};
use crate::segment::{DocFilter, DocId, DocSet, SegmentFile};
use crate::storage::{KeyRef, SegmentView, Snapshot, Updates};
use crate::text::{self, Bm25Params, BoolQuery, CollectionStats, FilterStrategy, Synonyms, TextFieldReader, TopKAlgo};
use crate::value::Value;
use crate::vector::{self, SearchParams, VectorReader};

/// Rows returned by ranked queries without a LIMIT.
pub const DEFAULT_K: usize = 100;

/// Terms a typo-tolerant word adds to a ranked query at most (the nearest,
/// then the most frequent), as Elasticsearch's `max_expansions`.
pub const MAX_FUZZY_TERMS: usize = 50;

/// Readers of one segment's indexes, by schema field. Each opens on first
/// use (opening only parses headers: every structure is a zero-copy view),
/// so a query touches the headers of the fields it uses and no others, which
/// matters on a cold cache. Cached per segment by [`crate::db::Db`].
pub struct SegmentReaders {
    pub n_docs: u32,
    seg: SegmentFile,
    schema: Schema,
    text: Vec<OnceLock<Opened<TextFieldReader>>>,
    numeric: Vec<OnceLock<Opened<Numeric>>>,
    keyword: Vec<OnceLock<Opened<KeywordColumn>>>,
    vector: Vec<OnceLock<Opened<VectorReader>>>,
    graph: Vec<OnceLock<Opened<GraphReader>>>,
    store: OnceLock<Opened<DocStore>>,
}

/// A reader, absent (the field has no such index), or the error opening it.
type Opened<T> = std::result::Result<Option<T>, String>;

enum Numeric {
    Plain(NumericColumn),
    Indexed(RangeIndex),
}

impl Numeric {
    fn col(&self) -> &NumericColumn {
        match self {
            Numeric::Plain(c) => c,
            Numeric::Indexed(r) => r.column(),
        }
    }
    fn index(&self) -> Option<&RangeIndex> {
        match self {
            Numeric::Indexed(r) => Some(r),
            Numeric::Plain(_) => None,
        }
    }
}

fn cells<T>(n: usize) -> Vec<OnceLock<T>> {
    (0..n).map(|_| OnceLock::new()).collect()
}

fn opened<'r, T>(cell: &'r OnceLock<Opened<T>>, open: impl FnOnce() -> Result<Option<T>>) -> Result<Option<&'r T>> {
    match cell.get_or_init(|| open().map_err(|e| e.to_string())) {
        Ok(r) => Ok(r.as_ref()),
        Err(e) => Err(crate::error::corrupt(e)),
    }
}

impl SegmentReaders {
    pub fn open(seg: &SegmentFile, schema: &Schema) -> Result<Self> {
        let n = schema.fields.len();
        Ok(SegmentReaders {
            n_docs: seg.n_docs() as u32,
            seg: seg.clone(),
            schema: schema.clone(),
            text: cells(n),
            numeric: cells(n),
            keyword: cells(n),
            vector: cells(n),
            graph: cells(n),
            store: OnceLock::new(),
        })
    }

    /// The graph index of an indexed EDGES field.
    pub fn graph(&self, field: usize) -> Result<Option<&GraphReader>> {
        let f = &self.schema.fields[field];
        opened(&self.graph[field], || {
            Ok(match f.ty {
                FieldType::Edges if f.indexed => GraphReader::open(&self.seg, &f.name)?,
                _ => None,
            })
        })
    }

    pub fn text(&self, field: usize) -> Result<Option<&TextFieldReader>> {
        let f = &self.schema.fields[field];
        opened(&self.text[field], || {
            Ok(match f.ty {
                FieldType::Text { .. } | FieldType::Sparse if f.indexed => Some(TextFieldReader::open(&self.seg, &f.name)?),
                _ => None,
            })
        })
    }

    fn numeric(&self, field: usize) -> Result<Option<&Numeric>> {
        let f = &self.schema.fields[field];
        opened(&self.numeric[field], || {
            Ok(match f.ty {
                FieldType::Int | FieldType::Float if f.indexed && !f.multi => Some(match RangeIndex::open_if_present(&self.seg, &f.name)? {
                    Some(idx) => Numeric::Indexed(idx),
                    None => Numeric::Plain(NumericColumn::open(&self.seg, &f.name)?),
                }),
                _ => None,
            })
        })
    }

    pub fn keyword(&self, field: usize) -> Result<Option<&KeywordColumn>> {
        let f = &self.schema.fields[field];
        opened(&self.keyword[field], || {
            // Multi-valued INT fields are keyword columns of int_key strings.
            if !(f.indexed && (matches!(f.ty, FieldType::Keyword) || matches!(f.ty, FieldType::Int) && f.multi)) {
                return Ok(None);
            }
            let mut col = KeywordColumn::open(&self.seg, &f.name)?;
            if col.needs_values() {
                // Multi-valued fields are stored.
                let store = self.store()?.ok_or_else(|| crate::error::corrupt(format!("keyword column {}: no stored values to repair it from", f.name)))?;
                col.repair(&store.field_values(field))?;
            }
            Ok(Some(col))
        })
    }

    pub fn vector(&self, field: usize) -> Result<Option<&VectorReader>> {
        let f = &self.schema.fields[field];
        opened(&self.vector[field], || {
            Ok(match f.ty {
                FieldType::Vector { .. } => Some(VectorReader::open(&self.seg, &f.name)?),
                _ => None,
            })
        })
    }

    pub fn store(&self) -> Result<Option<&DocStore>> {
        opened(&self.store, || {
            let any = self.schema.fields.iter().any(|f| f.stored && !matches!(f.ty, FieldType::Vector { .. }));
            Ok(if any { Some(DocStore::open(&self.seg, &self.schema)?) } else { None })
        })
    }
}

/// Execution knobs. `None` fields let the planner decide; experiments force
/// them to compare plans.
#[derive(Clone, Debug)]
pub struct ExecOptions {
    pub costs: CostModel,
    pub filter_mode: Option<FilterMode>,
    pub range: RangeStrategy,
    pub vector: Option<VectorStrategy>,
    pub vector_params: SearchParams,
    pub bm25: Bm25Params,
    pub bm25_algo: TopKAlgo,
    /// Candidates per input for fusion (`rrf` uses its own `depth`).
    pub fusion_depth: usize,
    /// Collection statistics to score BM25 with instead of this database's
    /// own, so that a shard of a larger collection scores like the whole
    /// of it ([`crate::query::stats`]). Terms missing from it use local
    /// counts.
    pub stats: Option<Arc<TermStats>>,
    /// Answer as one shard of a larger collection, for a coordinator to
    /// merge ([`crate::cluster::merge`]): OFFSET is folded into LIMIT,
    /// facets and counts return every group, and fused scores return every
    /// candidate with each input's raw score ([`FusionParts`]).
    pub shard: bool,
    /// Threads a query may use to search its segments in parallel (1: the
    /// calling thread only): the calling thread and up to `threads - 1` of
    /// the process's shared pool ([`Pool::global`](super::pool::Pool::global)),
    /// as they come free.
    pub threads: usize,
    /// Fail with `Error::Timeout` once this has passed (checked between
    /// segments).
    pub deadline: Option<Instant>,
    /// Fail with `Error::Timeout` once this is set, checked where the
    /// deadline is: the caller stopped waiting for the answer.
    pub cancel: Option<Arc<AtomicBool>>,
    /// The synonym list `synonyms => 1` expands searches with.
    pub synonyms: Option<Arc<Synonyms>>,
    /// Search only these segments (ordinals into the snapshot's segments),
    /// with BM25 statistics still summed over every segment, so the top-k
    /// lists of disjoint subsets merge by score into the whole query's. For
    /// experiments that run a query's segments in parallel from outside.
    pub segments: Option<Arc<[usize]>>,
    /// The most rows a query may reach: one whose LIMIT plus OFFSET is
    /// larger, or that has no LIMIT and matches more rows, fails, as
    /// Elasticsearch's `max_result_window` does (`None`: no limit).
    pub max_rows: Option<usize>,
    /// The most groups a GROUP BY or a facet may make before the query
    /// fails, as Elasticsearch's `search.max_buckets` (`None`: no limit).
    pub max_groups: Option<usize>,
    /// The budget the query's groups, candidates and rows are counted
    /// against; past it the query fails with `Error::Memory`
    /// ([`crate::query::memory`];
    /// `None`: not counted).
    pub memory: Option<Arc<MemoryBudget>>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            costs: CostModel::default(),
            filter_mode: None,
            range: RangeStrategy::Auto,
            vector: None,
            vector_params: SearchParams::default(),
            bm25: Bm25Params::default(),
            bm25_algo: TopKAlgo::MaxScore,
            fusion_depth: 100,
            stats: None,
            shard: false,
            threads: 1,
            deadline: None,
            cancel: None,
            synonyms: None,
            segments: None,
            max_rows: None,
            max_groups: None,
            memory: None,
        }
    }
}

impl ExecOptions {
    /// `Error::Timeout` once [`Self::deadline`] has passed or [`Self::cancel`]
    /// is set.
    pub fn check(&self) -> Result<()> {
        if self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(Error::Timeout("the query was cancelled".into()));
        }
        match self.deadline {
            Some(d) if Instant::now() >= d => Err(Error::Timeout("the query ran past its deadline".into())),
            _ => Ok(()),
        }
    }
}

/// Counts the steps of a long loop and checks its query's deadline and
/// cancel flag ([`ExecOptions::check`]) every `every` of them, so that a
/// query stops within a few thousand steps instead of at its next segment.
pub struct Ticker<'o> {
    opts: &'o ExecOptions,
    every: u32,
    n: u32,
}

impl<'o> Ticker<'o> {
    /// Steps between checks in a loop over documents (a check costs about
    /// as much as reading the clock).
    pub const DOCS: u32 = 1024;

    pub fn new(opts: &'o ExecOptions, every: u32) -> Ticker<'o> {
        Ticker { opts, every: every.max(1), n: 0 }
    }

    /// One step: [`ExecOptions::check`] every `every` steps.
    #[inline]
    pub fn tick(&mut self) -> Result<()> {
        self.n += 1;
        if self.n < self.every {
            return Ok(());
        }
        self.n = 0;
        self.opts.check()
    }
}

/// What execution did, for EXPLAIN-style reporting and experiments.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExecStats {
    /// One line per segment and operator.
    pub plan: Vec<String>,
    /// Documents passing the filter (where it was materialised).
    pub filter_docs: u64,
    /// Candidates scored by ranking operators.
    pub scored: u64,
}

#[derive(Clone, Debug)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// The primary key of each row (`Null` in a table without one); empty
    /// for facet and count queries.
    pub keys: Vec<Value>,
    /// The score each row was ordered by (0 for unordered rows); empty for
    /// facet and count queries.
    pub scores: Vec<f64>,
    /// With [`ExecOptions::shard`] and a fused score: every input's score
    /// of every row, for the coordinator to fuse.
    pub parts: Option<FusionParts>,
    pub stats: ExecStats,
}

/// The inputs of a fused score (RRF or weighted sum), row by row, so that a
/// coordinator can fuse candidates from several shards once
/// ([`crate::cluster::merge`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FusionParts {
    /// `scores[row][input]`: the input's exact score of the row (`None`
    /// where it has none, e.g. a missing column value).
    pub scores: Vec<Vec<Option<f64>>>,
    /// `ranked[row][input]`: whether the row is among the input's top
    /// `depth` rows in this shard.
    pub ranked: Vec<Vec<bool>>,
}

#[derive(Clone, Copy, Debug)]
struct Hit {
    seg: usize,
    doc: DocId,
    score: f64,
}

#[derive(Clone)]
struct Seg<'a> {
    ord: usize,
    view: &'a SegmentView,
    r: Arc<SegmentReaders>,
}

/// A segment's filter, with deleted documents excluded.
enum SegFilter {
    /// No predicate and no deletes.
    All,
    Set(Arc<DocSet>),
    Probe(ProbeFilter),
}

impl SegFilter {
    fn as_dyn(&self) -> Option<&dyn DocFilter> {
        match self {
            SegFilter::All => None,
            SegFilter::Set(s) => Some(s.as_ref()),
            SegFilter::Probe(p) => Some(p),
        }
    }
}

/// A predicate checked per document: column and keyword-code probes, with
/// text matches materialised.
struct ProbeFilter {
    root: ProbeNode,
    live: Option<Arc<dyn DocFilter>>,
    card: usize,
}

enum ProbeNode {
    Const(bool),
    Leaf(Box<dyn DocFilter>),
    And(Vec<ProbeNode>),
    Or(Vec<ProbeNode>),
    Not(Box<ProbeNode>),
}

impl ProbeNode {
    fn contains(&self, d: DocId) -> bool {
        match self {
            ProbeNode::Const(b) => *b,
            ProbeNode::Leaf(f) => f.contains(d),
            ProbeNode::And(cs) => cs.iter().all(|c| c.contains(d)),
            ProbeNode::Or(cs) => cs.iter().any(|c| c.contains(d)),
            ProbeNode::Not(c) => !c.contains(d),
        }
    }
}

impl DocFilter for ProbeFilter {
    fn contains(&self, d: DocId) -> bool {
        self.root.contains(d) && self.live.as_ref().is_none_or(|l| l.contains(d))
    }
    fn cardinality(&self) -> usize {
        self.card
    }
}

/// What per-segment work records, taken into the executor afterwards (so
/// that segments can run on several threads).
#[derive(Default)]
struct Local {
    plan: Vec<String>,
    filter_docs: u64,
    scored: u64,
    /// Filters materialised, by segment ordinal.
    sets: Vec<(usize, Arc<DocSet>)>,
}

pub struct Executor<'a> {
    schema: &'a Schema,
    snap: &'a Snapshot,
    segs: Arc<[Seg<'a>]>,
    /// Every non-empty segment, for collection statistics; empty unless
    /// `ExecOptions::segments` restricts `segs`.
    stat_segs: Arc<[Seg<'a>]>,
    opts: &'a ExecOptions,
    stats: ExecStats,
    /// Materialised filters by segment, shared by the operators of a query.
    sets: HashMap<usize, Arc<DocSet>>,
    /// Results of the query's graph computations (`Select::graphs`).
    graphs: Vec<graph::GraphOut>,
    /// Every segment's readers, for subqueries.
    readers_all: Vec<Arc<SegmentReaders>>,
    /// Shard mode: the fused query's candidates, each input's scores of
    /// them, and whether each is in each input's top `depth`.
    parts: Option<(Vec<(usize, DocId)>, Vec<Vec<f32>>, Vec<Vec<bool>>)>,
    /// Expanded searches, worked out once for every segment.
    expanded: Mutex<Expanded>,
    /// What the query holds of [`ExecOptions::memory`].
    charge: Charge,
}

/// Searches with their synonyms and typos resolved to indexed terms.
#[derive(Default)]
struct Expanded {
    /// `MATCH` queries by field, text and options.
    matches: HashMap<(usize, String, Expand), Arc<BoolQuery>>,
    /// Indexed terms near a term: (term, documents, edits), by field, term
    /// and edits.
    near: HashMap<(usize, String, u8), Arc<Vec<(String, u64, u8)>>>,
}

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// Keeps the rows of `v` that `better` puts among its first `k`, and every
/// row tied with the `k`-th (all of them when `k` is 0 or `v` has no more).
fn keep_best<T: Copy>(v: &mut Vec<T>, k: usize, better: impl Fn(&T, &T) -> std::cmp::Ordering) {
    if k > 0 && k < v.len() {
        v.select_nth_unstable_by(k - 1, &better);
        let cut = v[k - 1];
        v.retain(|c| better(c, &cut) != std::cmp::Ordering::Greater);
    }
}

impl<'a> Executor<'a> {
    /// `readers[i]` must belong to `snap.segments()[i]`.
    pub fn new(snap: &'a Snapshot, readers: &[Arc<SegmentReaders>], opts: &'a ExecOptions) -> Self {
        let all: Vec<Seg<'a>> = snap
            .segments()
            .iter()
            .zip(readers)
            .enumerate()
            .filter(|(_, (v, _))| v.file.n_docs() > 0)
            .map(|(ord, (view, r))| Seg { ord, view, r: r.clone() })
            .collect();
        let (segs, stat_segs): (Arc<[Seg<'a>]>, Arc<[Seg<'a>]>) = match &opts.segments {
            None => (all.into(), Arc::new([])),
            Some(only) => (all.iter().filter(|s| only.contains(&s.ord)).cloned().collect(), all.into()),
        };
        Executor {
            schema: snap.schema(),
            snap,
            segs,
            stat_segs,
            opts,
            stats: ExecStats::default(),
            sets: HashMap::new(),
            graphs: Vec::new(),
            readers_all: readers.to_vec(),
            parts: None,
            expanded: Mutex::new(Expanded::default()),
            charge: Charge::new(opts.memory.clone()),
        }
    }

    /// Rows live in the snapshot: an upper bound of any result, to which
    /// LIMIT and fusion depths are cut before they size anything.
    fn n_live(&self) -> usize {
        self.stat_segs().iter().map(|s| s.view.n_live as usize).sum()
    }

    /// A fusion depth or other requested row count, cut to the live rows.
    fn rows_at_most(&self, n: usize) -> usize {
        n.min(self.n_live())
    }

    /// Every segment of the snapshot, for collection statistics and term
    /// expansions, even when `ExecOptions::segments` searches only some.
    fn stat_segs(&self) -> &[Seg<'a>] {
        if self.opts.segments.is_some() {
            &self.stat_segs
        } else {
            &self.segs
        }
    }

    /// This database's BM25 statistics for every term `q` scores with
    /// (subqueries of graph seeds included), for a coordinator to sum over
    /// shards and send back as [`ExecOptions::stats`].
    pub fn term_stats(self, q: &Select) -> Result<TermStats> {
        let mut leaves = Vec::new();
        bm25_leaves(q, &mut leaves);
        let mut out = TermStats::default();
        for (field, text, opts) in leaves {
            let terms: Vec<String> = self.bm25_terms(field, text, opts)?.into_iter().map(|(t, _)| t).collect();
            let c = self.local_stats(field, &terms)?;
            out.merge_field(&self.schema.fields[field].name, FieldStats::from_collection(&c));
        }
        Ok(out)
    }

    pub fn execute(mut self, q: &Select) -> Result<ResultSet> {
        let columns: Vec<String> = q.projection.iter().map(|(n, _)| n.clone()).collect();
        if self.opts.shard && has_graphs(q) {
            return Err(err("graph queries cannot be split across shards: edges cross them"));
        }
        self.run_graphs(&q.graphs)?;
        let empty = |columns, rows, stats| ResultSet { columns, rows, keys: Vec::new(), scores: Vec::new(), parts: None, stats };
        if !q.group_by.is_empty() || agg::is_general(q, |f| self.facet_field(f)) {
            let needed = agg::needed(q);
            let groups = if agg::is_general(q, |f| self.facet_field(f)) { self.aggregate(q, &needed)? } else { self.facets(q, q.group_by[0], &needed)? };
            // Shards return every group with partial aggregates, for the
            // coordinator to combine.
            let rows = if self.opts.shard { groups.partial_rows() } else { agg::finish(q, groups, &needed) };
            return Ok(empty(columns, rows, self.stats));
        }
        let has_count = q.projection.iter().any(|(_, p)| *p == Projection::Count);
        if has_count {
            if q.projection.iter().any(|(_, p)| *p != Projection::Count) {
                return Err(err("COUNT(*) with other columns needs GROUP BY"));
            }
            let n = self.count(&q.filter)? as i64;
            let one = self.opts.shard || (q.offset == 0 && q.limit != Some(0));
            let rows = if one { vec![vec![Value::Int(n); columns.len()]] } else { Vec::new() };
            return Ok(empty(columns, rows, self.stats));
        }
        let hits = self.ordered_hits(q, self.opts.max_rows)?;
        let skip = if self.opts.shard { 0 } else { q.offset };
        let hits: Vec<Hit> = hits.into_iter().skip(skip).collect();
        let rows = self.project(q, &hits)?;
        let keys = hits.iter().map(|h| self.key(h)).collect();
        let scores = hits.iter().map(|h| h.score).collect();
        let parts = self.parts.take().map(|(cands, scores, ranked)| {
            let at: HashMap<(usize, DocId), usize> = cands.iter().enumerate().map(|(i, c)| (*c, i)).collect();
            let mut out = FusionParts::default();
            for h in &hits {
                let i = at[&(h.seg, h.doc)];
                out.scores.push(scores.iter().map(|s| Some(s[i] as f64).filter(|x| x.is_finite())).collect());
                out.ranked.push(ranked.iter().map(|r| r[i]).collect());
            }
            out
        });
        Ok(ResultSet { columns, rows, keys, scores, parts, stats: self.stats })
    }

    /// The rows of a query without facets or counts, in order, before
    /// OFFSET (graph seeds from a subquery use these).
    fn select_hits(&mut self, q: &Select) -> Result<Vec<Hit>> {
        if !q.group_by.is_empty() || q.projection.iter().any(|(_, p)| matches!(p, Projection::Count | Projection::Agg { .. })) {
            return Err(err("a seed subquery returns rows, not counts or aggregates"));
        }
        self.run_graphs(&q.graphs)?;
        let hits = self.ordered_hits(q, None)?;
        Ok(hits.into_iter().skip(q.offset).collect())
    }

    /// The rows of `q` in order, OFFSET included; at most `max` of them
    /// ([`ExecOptions::max_rows`]).
    fn ordered_hits(&mut self, q: &Select, max: Option<usize>) -> Result<Vec<Hit>> {
        let reach = q.limit.unwrap_or(0).saturating_add(q.offset);
        if let Some(max) = max.filter(|&m| reach > m) {
            return Err(err(format!("LIMIT and OFFSET reach row {reach}; a query returns at most {max} rows")));
        }
        // No query returns more rows than are live, so a larger LIMIT asks
        // for nothing more (and sizes nothing by itself). Without a LIMIT,
        // one row past `max` shows that the query matches too many.
        let want = q.limit.map(|l| l.saturating_add(q.offset)).or(max.map(|m| m + 1)).map(|w| w.min(self.n_live()));
        let hits = self.ordered_hits_to(q, want)?;
        if let Some(max) = max.filter(|&m| q.limit.is_none() && hits.len() > m) {
            return Err(err(format!("the query matches more than {max} rows, the most a query returns; add a LIMIT")));
        }
        Ok(hits)
    }

    fn ordered_hits_to(&mut self, q: &Select, want: Option<usize>) -> Result<Vec<Hit>> {
        Ok(match &q.order {
            None => self.unordered(&q.filter, want)?,
            Some(OrderBy::Score { score: Score::Column { field }, desc, nulls_first }) => {
                self.by_column(*field, *desc, Some(*nulls_first), &q.filter, want, q.tiebreak.as_ref())?
            }
            // Without a LIMIT, DEFAULT_K rows after the OFFSET, as a
            // coordinator cuts them.
            Some(OrderBy::Score { score, desc: true, .. }) => {
                let k = if q.limit.is_some() { want.unwrap_or(0) } else { DEFAULT_K.saturating_add(q.offset).min(self.n_live()) };
                self.ranked(score, &q.filter, k, q.tiebreak.as_ref())?
            }
            Some(OrderBy::Score { .. }) => return Err(err("relevance scores can only be ordered DESC")),
            Some(_) => return Err(err("ORDER BY COUNT(*) needs GROUP BY")),
        })
    }

    // ---- readers ---------------------------------------------------------

    fn text<'s>(&self, s: &'s Seg<'a>, field: usize) -> Result<&'s TextFieldReader> {
        s.r.text(field)?.ok_or_else(|| err(format!("{} has no text index", self.schema.fields[field].name)))
    }
    fn numeric<'s>(&self, s: &'s Seg<'a>, field: usize) -> Result<&'s Numeric> {
        s.r.numeric(field)?.ok_or_else(|| err(format!("{} has no column", self.schema.fields[field].name)))
    }
    fn keyword<'s>(&self, s: &'s Seg<'a>, field: usize) -> Result<&'s KeywordColumn> {
        s.r.keyword(field)?.ok_or_else(|| err(format!("{} has no keyword index", self.schema.fields[field].name)))
    }

    /// Segment `s`'s values of `field` written in place, if any.
    fn updates(&self, s: &Seg<'a>, field: usize) -> Option<&'a Updates> {
        s.view.updates.as_deref().filter(|u| u.has(field))
    }

    /// Document `d`'s value of numeric `field`: written in place, else
    /// the column's.
    fn num_value(&self, s: &Seg<'a>, field: usize, col: &NumericColumn, d: DocId) -> Option<f64> {
        match self.updates(s, field).and_then(|u| u.get(field, d)) {
            Some(v) => v.as_f64(),
            None => col.get_f64(d),
        }
    }

    /// Document `d`'s value of INT `field`, exactly.
    fn int_value(&self, s: &Seg<'a>, field: usize, col: &NumericColumn, d: DocId) -> Option<i64> {
        match self.updates(s, field).and_then(|u| u.get(field, d)) {
            Some(v) => v.as_i64(),
            None => col.get_i64(d),
        }
    }

    /// `set`, the documents whose column value of `field` passes `keep`,
    /// corrected for the values written in place.
    fn patch(&self, s: &Seg<'a>, field: usize, set: DocSet, keep: impl Fn(&Value) -> bool) -> DocSet {
        let Some(u) = self.updates(s, field) else { return set };
        let n = s.r.n_docs;
        let (mut add, mut drop) = (Vec::new(), Vec::new());
        for (d, v) in u.field(field) {
            if keep(v) {
                add.push(d);
            } else {
                drop.push(d);
            }
        }
        set.union(&DocSet::from_sorted(add, n)).intersect(&DocSet::from_sorted(drop, n).complement())
    }
    fn vector<'s>(&self, s: &'s Seg<'a>, field: usize) -> Result<&'s VectorReader> {
        s.r.vector(field)?.ok_or_else(|| err(format!("{} has no vectors", self.schema.fields[field].name)))
    }

    fn planner<'p>(&'p self, est: &'p SegEst<'p, 'a>) -> Planner<'p, SegEst<'p, 'a>> {
        Planner { est, costs: &self.opts.costs }
    }

    // ---- filters ---------------------------------------------------------

    /// The segment's filter in the given mode, deleted documents excluded.
    fn seg_filter(&mut self, s: &Seg<'a>, p: &Pred, mode: FilterMode) -> Result<SegFilter> {
        let mut l = Local::default();
        let r = self.seg_filter_l(s, p, mode, &mut l);
        self.absorb(l);
        r
    }

    fn seg_filter_l(&self, s: &Seg<'a>, p: &Pred, mode: FilterMode, l: &mut Local) -> Result<SegFilter> {
        if *p == Pred::True || mode == FilterMode::None {
            return Ok(match &s.view.live {
                None => SegFilter::All,
                Some(_) if mode == FilterMode::Materialize => SegFilter::Set(self.filter_set_l(s, p, l)?),
                Some(lv) => SegFilter::Probe(ProbeFilter { root: ProbeNode::Const(true), live: Some(lv.clone()), card: lv.cardinality() }),
            });
        }
        match mode {
            FilterMode::Probe => {
                let root = self.compile_probe(s, p)?;
                let card = {
                    let est = SegEst { ex: self, s, params: None, local_sel: None };
                    self.planner(&est).estimate(p).card as usize
                };
                Ok(SegFilter::Probe(ProbeFilter { root, live: s.view.live.clone(), card }))
            }
            _ => Ok(SegFilter::Set(self.filter_set_l(s, p, l)?)),
        }
    }

    /// The materialised filter of a segment, live documents only (cached).
    fn filter_set(&mut self, s: &Seg<'a>, p: &Pred) -> Result<Arc<DocSet>> {
        let mut l = Local::default();
        let r = self.filter_set_l(s, p, &mut l);
        self.absorb(l);
        r
    }

    fn filter_set_l(&self, s: &Seg<'a>, p: &Pred, l: &mut Local) -> Result<Arc<DocSet>> {
        if let Some(set) = self.sets.get(&s.ord).or_else(|| l.sets.iter().find(|(o, _)| *o == s.ord).map(|(_, x)| x)) {
            return Ok(set.clone());
        }
        let mut set = self.materialize(s, p)?;
        if let Some(live) = &s.view.live {
            set = DocSet::from_sorted(set.iter().filter(|&d| live.contains(d)).collect(), s.r.n_docs);
        }
        l.filter_docs += set.len() as u64;
        l.plan.push(format!("seg {}: filter materialised, {} docs", s.ord, set.len()));
        let set = Arc::new(set);
        l.sets.push((s.ord, set.clone()));
        Ok(set)
    }

    /// Takes in what per-segment work recorded.
    fn absorb(&mut self, l: Local) {
        self.stats.plan.extend(l.plan);
        self.stats.filter_docs += l.filter_docs;
        self.stats.scored += l.scored;
        for (ord, set) in l.sets {
            self.sets.entry(ord).or_insert(set);
        }
    }

    /// Runs `f` on every segment, on this thread and up to
    /// [`ExecOptions::threads`] `- 1` pool threads (largest segments first),
    /// and returns the results in segment order with what each recorded
    /// taken in.
    fn per_segment<T: Send>(&mut self, f: impl Fn(&Self, &Seg<'a>, &mut Local) -> Result<T> + Sync) -> Result<Vec<T>>
    where
        Self: Sync,
    {
        let segs = self.segs.clone();
        let threads = self.opts.threads.clamp(1, segs.len().max(1));
        let mut outs: Vec<Option<(Result<T>, Local)>> = (0..segs.len()).map(|_| None).collect();
        if threads == 1 {
            for (i, s) in segs.iter().enumerate() {
                let mut l = Local::default();
                let r = self.check_deadline().and_then(|_| f(self, s, &mut l));
                outs[i] = Some((r, l));
            }
        } else {
            let mut order: Vec<usize> = (0..segs.len()).collect();
            order.sort_by_key(|&i| std::cmp::Reverse(segs[i].view.n_live));
            let next = std::sync::atomic::AtomicUsize::new(0);
            let slots: Vec<std::sync::Mutex<Option<(Result<T>, Local)>>> = (0..segs.len()).map(|_| std::sync::Mutex::new(None)).collect();
            let this = &*self;
            // This thread and up to `threads - 1` of the shared pool's,
            // each taking the next segment until none are left.
            super::pool::Pool::global().run(threads - 1, &|| loop {
                let j = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(&i) = order.get(j) else { break };
                let mut l = Local::default();
                let r = this.check_deadline().and_then(|_| f(this, &segs[i], &mut l));
                *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some((r, l));
            });
            for (o, m) in outs.iter_mut().zip(slots) {
                *o = m.into_inner().unwrap_or_else(|e| e.into_inner());
            }
            self.stats.plan.push(format!("{} segments on up to {threads} threads", segs.len()));
        }
        let mut res = Vec::with_capacity(outs.len());
        for o in outs {
            let (r, l) = o.expect("every segment ran");
            self.absorb(l);
            res.push(r?);
        }
        Ok(res)
    }

    fn materialize(&self, s: &Seg<'a>, p: &Pred) -> Result<DocSet> {
        let n = s.r.n_docs;
        Ok(match p {
            Pred::True => DocSet::all(n),
            Pred::False => DocSet::empty(n),
            Pred::Match { field, query, expand } => {
                let q = self.match_query(*field, query, expand)?;
                self.text(s, *field)?.eval(&q, None)
            }
            Pred::Range { field, lo, hi } if self.schema.fields[*field].multi => {
                let kw = self.keyword(s, *field)?;
                kw.docs_codes(&kw.int_codes(*lo, *hi).collect::<Vec<u32>>())
            }
            Pred::Range { field, lo, hi } => {
                let num = self.numeric(s, *field)?;
                let set = forward::eval_range(num.col(), num.index(), *lo, *hi, self.opts.range);
                self.patch(s, *field, set, |v| v.as_f64().is_some_and(|x| *lo <= x && x <= *hi))
            }
            Pred::IntRange { field, lo, hi } if self.schema.fields[*field].multi => {
                let kw = self.keyword(s, *field)?;
                kw.docs_codes(&kw.int_codes_exact(*lo, *hi).collect::<Vec<u32>>())
            }
            Pred::IntRange { field, lo, hi } => {
                let num = self.numeric(s, *field)?;
                let set = forward::eval_keys(num.col(), num.index(), num.col().int_key_range(*lo, *hi), self.opts.range);
                self.patch(s, *field, set, |v| int_in(v, *lo, *hi))
            }
            Pred::Present { field } => match self.schema.fields[*field].ty {
                FieldType::Int | FieldType::Float if !self.schema.fields[*field].multi => {
                    let set = self.numeric(s, *field)?.col().scan_keys(Some(KeyRange { lo: i64::MIN, hi: i64::MAX }));
                    self.patch(s, *field, set, |v| !matches!(v, Value::Null))
                }
                _ => self.keyword(s, *field)?.present(),
            },
            Pred::In { field, values } => self.materialize_in(s, *field, values)?,
            Pred::Graph { id } => self.graph_set(*id, s.ord, n),
            Pred::Not(c) => self.materialize(s, c)?.complement(),
            Pred::Or(cs) => {
                let mut acc = DocSet::empty(n);
                for c in cs {
                    acc = acc.union(&self.materialize(s, c)?);
                }
                acc
            }
            Pred::And(cs) => {
                // Most selective first; once the running result is small,
                // probing a child per survivor beats materialising it.
                let est = SegEst { ex: self, s, params: None, local_sel: None };
                let planner = self.planner(&est);
                let mut ests: Vec<(f64, &Pred)> = cs.iter().map(|c| (planner.estimate(c).card, c)).collect();
                ests.sort_by(|a, b| a.0.total_cmp(&b.0));
                let mut acc: Option<DocSet> = None;
                for (_, c) in ests {
                    acc = Some(match acc {
                        None => self.materialize(s, c)?,
                        Some(a) if a.is_empty() => return Ok(a),
                        Some(a) => {
                            let e = planner.estimate(c);
                            if e.probeable && a.len() as f64 * e.probe_cost < e.mat_cost {
                                let probe = self.compile_probe(s, c)?;
                                DocSet::from_sorted(a.iter().filter(|&d| probe.contains(d)).collect(), n)
                            } else {
                                a.intersect(&self.materialize(s, c)?)
                            }
                        }
                    });
                }
                acc.unwrap_or_else(|| DocSet::all(n))
            }
        })
    }

    fn materialize_in(&self, s: &Seg<'a>, field: usize, values: &[Value]) -> Result<DocSet> {
        let f = &self.schema.fields[field];
        match f.ty {
            _ if f.ty == FieldType::Keyword || f.multi => {
                let keys: Vec<_> = values.iter().filter_map(forward::key_of).collect();
                let vs: Vec<&str> = keys.iter().map(|k| k.as_ref()).collect();
                Ok(self.keyword(s, field)?.docs_in(&vs))
            }
            _ => {
                let num = self.numeric(s, field)?;
                let mut acc = DocSet::empty(s.r.n_docs);
                for v in values {
                    acc = acc.union(&forward::eval_keys(num.col(), num.index(), value_keys(num.col(), v), self.opts.range));
                }
                Ok(self.patch(s, field, acc, |v| values.iter().any(|x| num_eq(v, x))))
            }
        }
    }

    fn compile_probe(&self, s: &Seg<'a>, p: &Pred) -> Result<ProbeNode> {
        Ok(match p {
            Pred::True => ProbeNode::Const(true),
            Pred::False => ProbeNode::Const(false),
            Pred::Match { .. } => ProbeNode::Leaf(Box::new(self.materialize(s, p)?)),
            // Multi-valued fields are probed through their postings, fields
            // with values written in place through the corrected set.
            Pred::Range { field, .. } | Pred::IntRange { field, .. } | Pred::In { field, .. }
                if self.schema.fields[*field].multi || self.updates(s, *field).is_some() =>
            {
                ProbeNode::Leaf(Box::new(self.materialize(s, p)?))
            }
            Pred::Present { .. } => ProbeNode::Leaf(Box::new(self.materialize(s, p)?)),
            Pred::Range { field, lo, hi } => ProbeNode::Leaf(Box::new(self.numeric(s, *field)?.col().filter(*lo, *hi))),
            Pred::IntRange { field, lo, hi } => {
                let col = self.numeric(s, *field)?.col();
                ProbeNode::Leaf(Box::new(col.filter_keys(col.int_key_range(*lo, *hi))))
            }
            Pred::In { field, values } => match self.schema.fields[*field].ty {
                FieldType::Keyword => {
                    let vs: Vec<&str> = values.iter().filter_map(|v| v.as_str()).collect();
                    ProbeNode::Leaf(Box::new(self.keyword(s, *field)?.filter_in(&vs)))
                }
                _ => {
                    let col = self.numeric(s, *field)?.col();
                    ProbeNode::Or(values.iter().map(|v| ProbeNode::Leaf(Box::new(col.filter_keys(value_keys(col, v))))).collect())
                }
            },
            Pred::Graph { id } => ProbeNode::Leaf(Box::new(self.graph_set(*id, s.ord, s.r.n_docs))),
            Pred::Not(c) => ProbeNode::Not(Box::new(self.compile_probe(s, c)?)),
            Pred::Or(cs) => ProbeNode::Or(cs.iter().map(|c| self.compile_probe(s, c)).collect::<Result<_>>()?),
            Pred::And(cs) => {
                let est = SegEst { ex: self, s, params: None, local_sel: None };
                let planner = self.planner(&est);
                let mut ordered: Vec<(f64, &Pred)> = cs.iter().map(|c| (planner.estimate(c).card, c)).collect();
                ordered.sort_by(|a, b| a.0.total_cmp(&b.0));
                ProbeNode::And(ordered.into_iter().map(|(_, c)| self.compile_probe(s, c)).collect::<Result<_>>()?)
            }
        })
    }

    // ---- unranked --------------------------------------------------------

    fn count(&mut self, p: &Pred) -> Result<u64> {
        let mut total = 0u64;
        let segs = self.segs.clone();
        for s in segs.iter() {
            self.check_deadline()?;
            total += match (p, &s.view.live) {
                (Pred::True, None) => s.r.n_docs as u64,
                (Pred::Range { field, lo, hi }, None) if !self.schema.fields[*field].multi && self.updates(s, *field).is_none() => {
                    let num = self.numeric(s, *field)?;
                    self.stats.plan.push(format!("seg {}: count from the range index", s.ord));
                    forward::count_range(num.col(), num.index(), *lo, *hi, self.opts.range) as u64
                }
                (Pred::IntRange { field, lo, hi }, None) if !self.schema.fields[*field].multi && self.updates(s, *field).is_none() => {
                    let num = self.numeric(s, *field)?;
                    self.stats.plan.push(format!("seg {}: count from the range index", s.ord));
                    forward::count_keys(num.col(), num.index(), num.col().int_key_range(*lo, *hi), self.opts.range) as u64
                }
                _ => self.filter_set(s, p)?.len() as u64,
            };
        }
        Ok(total)
    }

    fn unordered(&mut self, p: &Pred, want: Option<usize>) -> Result<Vec<Hit>> {
        let mut out = Vec::new();
        let segs = self.segs.clone();
        let mut tick = self.ticker();
        for s in segs.iter() {
            self.check_deadline()?;
            if want.is_some_and(|w| out.len() >= w) {
                break;
            }
            let set = self.filter_set(s, p)?;
            for d in set.iter() {
                tick.tick()?;
                if want.is_some_and(|w| out.len() >= w) {
                    break;
                }
                self.charge.room(&mut out)?;
                out.push(Hit { seg: s.ord, doc: d, score: 0.0 });
            }
        }
        Ok(out)
    }

    /// Top rows by a numeric column; all rows tied with the last one are
    /// kept until the tie-break orders them. Rows without a value are left
    /// out (`nulls: None`, a fusion input) or score `∓inf` so that they
    /// sort last, or first with `nulls: Some(true)` (`NULLS FIRST`).
    fn by_column(&mut self, field: usize, desc: bool, nulls: Option<bool>, p: &Pred, want: Option<usize>, tb: Option<&Tiebreak>) -> Result<Vec<Hit>> {
        let null_score = nulls.map(|first| if first == desc { f64::INFINITY } else { f64::NEG_INFINITY });
        // Each row with its exact INT value: a double cannot order
        // integers beyond 2^53 (0 for FLOAT columns and missing values).
        let int = self.schema.fields[field].ty == FieldType::Int;
        let better = |a: &(Hit, i64), b: &(Hit, i64)| {
            let o = a.0.score.total_cmp(&b.0.score).then(a.1.cmp(&b.1));
            if desc { o.reverse() } else { o }
        };
        let mut cands: Vec<(Hit, i64)> = Vec::new();
        // With a LIMIT, the candidates are cut to the best and their ties
        // as they pile up, so they hold about twice the LIMIT, not every
        // match.
        let mut cut_at = want.map_or(usize::MAX, |k| (2 * k).max(4096));
        let segs = self.segs.clone();
        let mut tick = self.ticker();
        for s in segs.iter() {
            self.check_deadline()?;
            let col = self.numeric(s, field)?.col();
            let set = self.filter_set(s, p)?;
            for d in set.iter() {
                tick.tick()?;
                if let Some(v) = self.num_value(s, field, col, d).or(null_score) {
                    let exact = if int { self.int_value(s, field, col, d).unwrap_or(0) } else { 0 };
                    self.charge.room(&mut cands)?;
                    cands.push((Hit { seg: s.ord, doc: d, score: v }, exact));
                    self.stats.scored += 1;
                    if cands.len() >= cut_at {
                        keep_best(&mut cands, want.unwrap_or(0), better);
                        cut_at = cut_at.max(2 * cands.len());
                    }
                }
            }
        }
        if let Some(k) = want {
            keep_best(&mut cands, k, better);
        }
        // Rows tied on the value go by key (`ORDER BY rating DESC, id`), or
        // in index order.
        let key_desc = match tb {
            Some(Tiebreak::Key { desc }) => Some(*desc),
            _ => None,
        };
        let mut keyed: Vec<(Hit, i64, Value)> = cands.into_iter().map(|(h, x)| (h, x, if key_desc.is_some() { self.key(&h) } else { Value::Null })).collect();
        keyed.sort_by(|a, b| {
            better(&(a.0, a.1), &(b.0, b.1)).then_with(|| match key_desc {
                Some(kd) => {
                    let c = cmp_values(&a.2, &b.2);
                    if kd { c.reverse() } else { c }
                }
                None => (a.0.seg, a.0.doc).cmp(&(b.0.seg, b.0.doc)),
            })
        });
        if let Some(k) = want {
            keyed.truncate(k);
        }
        Ok(keyed.into_iter().map(|(h, _, _)| h).collect())
    }

    /// The `k` rows of the filter with the largest decay of `field` (rows
    /// without a value score 0), rows tied on it by key.
    fn decay_top(&mut self, field: usize, f: &Decay, p: &Pred, k: usize, key_desc: bool) -> Result<Vec<Hit>> {
        let by_score = |a: &Hit, b: &Hit| b.score.total_cmp(&a.score);
        let mut cands = Vec::new();
        let mut cut_at = if k > 0 { (2 * k).max(4096) } else { usize::MAX };
        let segs = self.segs.clone();
        let mut tick = self.ticker();
        for s in segs.iter() {
            self.check_deadline()?;
            let col = self.numeric(s, field)?.col();
            let set = self.filter_set(s, p)?;
            for d in set.iter() {
                tick.tick()?;
                let score = self.num_value(s, field, col, d).map_or(0.0, |v| f.eval(v));
                self.charge.room(&mut cands)?;
                cands.push(Hit { seg: s.ord, doc: d, score });
                self.stats.scored += 1;
                if cands.len() >= cut_at {
                    keep_best(&mut cands, k, by_score);
                    cut_at = cut_at.max(2 * cands.len());
                }
            }
        }
        self.stats.plan.push(format!("decay {:?} of {} around {}, scale {}", f.shape, self.schema.fields[field].name, f.origin, f.scale));
        keep_best(&mut cands, k, by_score);
        self.sort_by_score_then_key(&mut cands, true, key_desc);
        cands.truncate(k);
        Ok(cands)
    }

    // ---- ranked ----------------------------------------------------------

    /// The top `k` rows by `score`. A fused score takes at least `k` rows
    /// of each input (its depth is raised to the page asked for).
    fn ranked(&mut self, score: &Score, p: &Pred, k: usize, tb: Option<&Tiebreak>) -> Result<Vec<Hit>> {
        if self.opts.shard && matches!(score, Score::Rrf { .. } | Score::Weighted(_) | Score::Product(_)) {
            return self.fusion_candidates(score, p, k);
        }
        let mut hits = match score {
            Score::Rrf { inputs, k: rk, depth } => {
                let depth = &self.rows_at_most((*depth).max(k));
                let mut lists = Vec::new();
                for s in inputs {
                    lists.push(self.input_top(s, p, *depth, false)?.into_iter().map(|h| (h.seg, h.doc)).collect::<Vec<_>>());
                }
                self.stats.plan.push(format!("rrf: {} inputs, depth {depth}, k {rk}", inputs.len()));
                let mut hits: Vec<Hit> = fusion::rrf(&lists, *rk).into_iter().map(|((seg, doc), score)| Hit { seg, doc, score }).collect();
                // Exact ties (same ranks in different lists) go to the smaller key.
                self.sort_by_score_then_key(&mut hits, true, false);
                hits
            }
            Score::Weighted(terms) => self.weighted(terms, p, k)?,
            Score::Product(factors) => self.product(factors, p, k)?,
            // Cut and ordered like a fusion input, so that rows tied on
            // score come in key order on one database and over shards.
            leaf => self.input_top(leaf, p, k, matches!(tb, Some(Tiebreak::Key { desc: true })))?,
        };
        if let Some(Tiebreak::Key { desc }) = tb {
            self.sort_by_score_then_key(&mut hits, true, *desc);
        }
        hits.truncate(k);
        Ok(hits)
    }

    /// The top `depth` rows of a single score (a query's ranking, or one
    /// input of a fused score), rows tied on score ordered by key as
    /// [`crate::cluster::merge`] orders them over shards. Rows tied with the
    /// `depth`-th are read to the end of the tie, so that one database and
    /// its shards cut at the same rows and rank them alike.
    fn input_top(&mut self, score: &Score, p: &Pred, depth: usize, key_desc: bool) -> Result<Vec<Hit>> {
        let mut hits = match score {
            // Scans of every row: they keep the whole tie at the cut and
            // order it by key themselves.
            Score::Column { field } => return self.by_column(*field, true, None, p, Some(depth), Some(&Tiebreak::Key { desc: key_desc })),
            Score::Decay { field, decay, .. } => return self.decay_top(*field, decay, p, depth, key_desc),
            // These keep every row tied with the depth-th.
            Score::Ppr { .. } | Score::Hops { .. } => self.graph_top(score, p, depth)?,
            Score::Bm25 { field, text, opts } => self.bm25_top(*field, text, opts, p, depth, true)?,
            _ => {
                let mut want = depth + 1;
                loop {
                    let hits = self.leaf_top(score, p, want)?;
                    if depth == 0 || hits.len() < want || hits[want - 1].score != hits[depth - 1].score {
                        break hits;
                    }
                    want = want.saturating_mul(2);
                }
            }
        };
        self.order_ties_by_key(&mut hits, key_desc);
        hits.truncate(depth);
        Ok(hits)
    }

    /// Orders each run of rows with equal scores by key, as
    /// [`cmp_values`] orders keys; rows that tie with no other keep their
    /// place and have no key read.
    fn order_ties_by_key(&self, hits: &mut [Hit], key_desc: bool) {
        let mut i = 0;
        while i < hits.len() {
            let j = i + hits[i..].iter().take_while(|h| h.score == hits[i].score).count();
            if j - i > 1 {
                // Borrowed keys: a tie can hold thousands of rows.
                let mut keyed: Vec<(Option<KeyRef<'_>>, Hit)> = hits[i..j].iter().map(|h| (self.snap.pk(h.seg).map(|p| p.key_ref(h.doc)), *h)).collect();
                keyed.sort_by(|(a, _), (b, _)| {
                    let c = match (a, b) {
                        (Some(KeyRef::Str(x)), Some(KeyRef::Str(y))) => x.cmp(y),
                        (Some(KeyRef::Int(x)), Some(KeyRef::Int(y))) => x.cmp(y),
                        _ => std::cmp::Ordering::Equal,
                    };
                    if key_desc { c.reverse() } else { c }
                });
                for (h, (_, k)) in hits[i..j].iter_mut().zip(keyed) {
                    *h = k;
                }
            }
            i = j;
        }
    }

    fn leaf_top(&mut self, score: &Score, p: &Pred, k: usize) -> Result<Vec<Hit>> {
        match score {
            Score::Bm25 { field, text, opts } => self.bm25_top(*field, text, opts, p, k, false),
            Score::Similarity { field, vector, opts } => self.vector_top(*field, vector, opts, p, k),
            Score::Sparse { field, terms, opts } => self.sparse_top(*field, terms, opts, p, k),
            Score::Column { field } => self.by_column(*field, true, None, p, Some(k), None),
            Score::Decay { field, decay, .. } => self.decay_top(*field, decay, p, k, false),
            Score::Ppr { .. } | Score::Hops { .. } => self.graph_top(score, p, k),
            _ => Err(err("fusion inputs must be bm25, similarity, ppr, hops, decay or a column")),
        }
    }

    /// Top-`k` of a SPARSE field by the dot product with `terms`: the
    /// text index's top-k over the weights (no collection statistics, so
    /// shards and segments score alike).
    fn sparse_top(&mut self, field: usize, terms: &[(String, f32)], opts: &Opts, p: &Pred, k: usize) -> Result<Vec<Hit>> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let (_, algo) = self.bm25_params(opts)?;
        let per = self.per_segment(|ex, s, l| {
            let t = ex.text(s, field)?;
            let candidates = terms.iter().map(|(w, _)| t.doc_freq(w) as f64).sum::<f64>() * 0.3;
            let mode = match ex.opts.filter_mode {
                Some(m) => m,
                None => {
                    let est = SegEst { ex, s, params: None, local_sel: None };
                    ex.planner(&est).filter_mode(p, candidates)
                }
            };
            let f = ex.seg_filter_l(s, p, mode, l)?;
            let hits = t.top_k_dot(terms, k, f.as_dyn(), algo, FilterStrategy::Auto);
            l.plan.push(format!("seg {}: sparse {algo:?}, {} terms, filter {mode:?}", s.ord, terms.len()));
            Ok(hits.into_iter().map(|(doc, sc)| Hit { seg: s.ord, doc, score: sc as f64 }).collect::<Vec<_>>())
        })?;
        Ok(merge_top(per.concat(), k))
    }

    /// BM25 statistics over every segment of the snapshot.
    fn local_stats(&self, field: usize, terms: &[String]) -> Result<CollectionStats> {
        let mut st = CollectionStats { n_docs: 0, total_len: 0, df: HashMap::new() };
        for s in self.stat_segs() {
            let t = self.text(s, field)?;
            st.n_docs += t.n_docs_with_terms() as u64;
            st.total_len += t.total_len();
            for w in terms {
                *st.df.entry(w.clone()).or_insert(0) += t.doc_freq(w) as u64;
            }
        }
        Ok(st)
    }

    /// The statistics BM25 scores with: the snapshot's own, or the
    /// collection's from [`ExecOptions::stats`] where it has them.
    fn collection_stats(&self, field: usize, terms: &[String]) -> Result<CollectionStats> {
        let mut st = self.local_stats(field, terms)?;
        if let Some(g) = self.opts.stats.as_ref().and_then(|s| s.fields.get(&self.schema.fields[field].name)) {
            st.n_docs = g.n_docs;
            st.total_len = g.total_len;
            for w in terms {
                if let Some(&df) = g.df.get(w) {
                    st.df.insert(w.clone(), df);
                }
            }
        }
        Ok(st)
    }

    fn synonyms(&self) -> Result<&Synonyms> {
        self.opts.synonyms.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
            err("synonyms => 1, but no synonym list is loaded (strata-server --synonyms FILE, or PUT /v1/synonyms)")
        })
    }

    /// The indexed terms of `field` within `edits` edits of `term` in any
    /// segment, with their document counts and distances.
    fn near_terms(&self, field: usize, term: &str, edits: u8) -> Result<Arc<Vec<(String, u64, u8)>>> {
        let key = (field, term.to_string(), edits);
        if let Some(v) = self.expanded.lock().unwrap_or_else(|e| e.into_inner()).near.get(&key) {
            return Ok(v.clone());
        }
        let mut found: HashMap<String, (u64, u8)> = HashMap::new();
        for s in self.stat_segs() {
            for (t, info, d) in self.text(s, field)?.fuzzy_terms(term, edits) {
                found.entry(t).or_insert((0, d)).0 += info.df as u64;
            }
        }
        let mut v: Vec<(String, u64, u8)> = found.into_iter().map(|(t, (df, d))| (t, df, d)).collect();
        v.sort_by(|a, b| a.2.cmp(&b.2).then(b.1.cmp(&a.1)).then(a.0.cmp(&b.0)));
        let v = Arc::new(v);
        self.expanded.lock().unwrap_or_else(|e| e.into_inner()).near.insert(key, v.clone());
        Ok(v)
    }

    /// A `MATCH` query with its synonyms and typos expanded to the indexed
    /// terms of every segment (so each segment evaluates the same terms).
    fn match_query(&self, field: usize, query: &str, expand: &Expand) -> Result<Arc<BoolQuery>> {
        let key = (field, query.to_string(), *expand);
        if let Some(q) = self.expanded.lock().unwrap_or_else(|e| e.into_inner()).matches.get(&key) {
            return Ok(q.clone());
        }
        let mut q = text::parse_match(query)?;
        if !expand.is_none() {
            let syn = if expand.synonyms { Some(self.synonyms()?) } else { None };
            let edits = |t: &str| expand.fuzzy.map_or(0, |f| f.edits(t.chars().count()));
            q = text::query::expand(q, syn, &edits);
        }
        let mut failed = None;
        q = text::query::resolve_fuzzy(q, &mut |t, e| match self.near_terms(field, t, e) {
            Ok(v) => v.iter().map(|x| x.0.clone()).collect(),
            Err(e) => {
                failed = Some(e);
                Vec::new()
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        let q = Arc::new(q);
        self.expanded.lock().unwrap_or_else(|e| e.into_inner()).matches.insert(key, q.clone());
        Ok(q)
    }

    /// The weighted terms of `bm25(field, text, ...)`: the query's own,
    /// each weighing as often as it occurs, then its synonyms' (1) and its
    /// typo-tolerant words' nearest indexed terms (`1 − edits / length`).
    fn bm25_terms(&self, field: usize, text: &str, opts: &Opts) -> Result<Vec<(String, f32)>> {
        Ok(self.bm25_query(field, text, opts)?.terms)
    }

    /// [`Self::bm25_terms`], and the groups of terms that stand for one
    /// query term: it, its typo neighbours and its one-word synonyms. A
    /// group shares its statistics ([`blend_groups`]), as Lucene's synonym
    /// and fuzzy queries do, so that a rare misspelling does not outrank
    /// the word it misspells.
    fn bm25_query(&self, field: usize, text: &str, opts: &Opts) -> Result<Bm25Query> {
        let seq = query_term_seq(text, opts);
        let expand = Expand::from_opts(opts).map_err(err)?;
        let mut out = Bm25Query::default();
        for t in &seq {
            match out.terms.iter_mut().find(|(x, _)| x == t) {
                Some((_, w)) => *w += 1.0,
                None => {
                    out.groups.push(vec![out.terms.len()]);
                    out.terms.push((t.clone(), 1.0));
                }
            }
        }
        let n_base = out.terms.len();
        // Adds `t` unless the query has it; to base term `g`'s group if given.
        let add = |t: &str, w: f32, g: Option<usize>, out: &mut Bm25Query| {
            if !out.terms.iter().any(|(x, _)| x == t) {
                if let Some(g) = g {
                    out.groups[g].push(out.terms.len());
                }
                out.terms.push((t.to_string(), w));
            }
        };
        let base_of = |t: &str, out: &Bm25Query| out.terms[..n_base].iter().position(|(x, _)| x == t);
        if expand.synonyms {
            let syn = self.synonyms()?;
            for i in 0..seq.len() {
                for len in 1..=syn.max_len().min(seq.len() - i) {
                    for alt in syn.get(&seq[i..i + len]) {
                        // A one-word synonym of one word stands for it; the
                        // words of longer ones score on their own.
                        let g = if len == 1 && alt.len() == 1 { base_of(&seq[i], &out) } else { None };
                        for t in alt {
                            add(t, 1.0, g, &mut out);
                        }
                    }
                }
            }
        }
        if let Some(fz) = expand.fuzzy {
            for b in 0..n_base {
                let t = out.terms[b].0.clone();
                let len = t.chars().count();
                let e = fz.edits(len);
                if e == 0 {
                    continue;
                }
                for (near, _, d) in self.near_terms(field, &t, e)?.iter().filter(|x| x.2 > 0).take(MAX_FUZZY_TERMS) {
                    add(near, text::fuzzy::weight(*d, len), Some(b), &mut out);
                }
            }
        }
        out.n_base = n_base;
        Ok(out)
    }

    /// The terms of a `bm25` query and the collection statistics it scores
    /// with, each group's terms sharing the largest document frequency
    /// among them.
    fn bm25_scoring(&self, field: usize, text: &str, opts: &Opts) -> Result<(Bm25Query, CollectionStats)> {
        let q = self.bm25_query(field, text, opts)?;
        let names: Vec<String> = q.terms.iter().map(|(t, _)| t.clone()).collect();
        let mut stats = self.collection_stats(field, &names)?;
        blend_groups(&q, &mut stats);
        Ok((q, stats))
    }

    /// Whether facets can count `field`'s values (a keyword column).
    fn facet_field(&self, field: usize) -> bool {
        let f = &self.schema.fields[field];
        f.ty == FieldType::Keyword || (f.ty == FieldType::Int && f.multi)
    }

    /// Document `d`'s values of `field` for aggregates: one for a
    /// single-valued field (none when missing), each of a multi-valued
    /// field's (from `multi`, the documents' value codes).
    fn agg_values(&self, s: &Seg<'a>, field: usize, d: DocId, multi: &HashMap<usize, HashMap<DocId, Vec<u32>>>, out: &mut Vec<Value>) -> Result<()> {
        out.clear();
        let f = &self.schema.fields[field];
        if let Some(codes) = multi.get(&field) {
            let kw = self.keyword(s, field)?;
            for &c in codes.get(&d).map_or(&[][..], |v| v.as_slice()) {
                let v = kw.value(c);
                out.push(if f.ty == FieldType::Int { forward::int_of_key(v).map_or(Value::Null, Value::Int) } else { Value::Str(v.to_string()) });
            }
            return Ok(());
        }
        let v = match f.ty {
            FieldType::Keyword => self.keyword(s, field)?.value_of_doc(d).map_or(Value::Null, |v| Value::Str(v.to_string())),
            _ => match self.updates(s, field).and_then(|u| u.get(field, d)) {
                Some(v) => v.clone(),
                None => self.numeric(s, field)?.col().get(d),
            },
        };
        if !matches!(v, Value::Null) {
            out.push(v);
        }
        Ok(())
    }

    /// Groups and aggregates, document by document (anything beyond a
    /// facet's counts).
    fn aggregate(&mut self, q: &Select, needed: &[Projection]) -> Result<agg::Groups> {
        let mut fields: Vec<usize> = q.group_by.clone();
        for p in needed {
            if let Projection::Agg { field, .. } = p {
                if !fields.contains(field) {
                    fields.push(*field);
                }
            }
        }
        let mut groups = agg::Groups::limited(self.opts.max_groups);
        groups.charge = self.charge.clone();
        let segs = self.segs.clone();
        let (mut vals, mut gv) = (Vec::new(), Vec::new());
        let mut tick = self.ticker();
        for s in segs.iter() {
            self.check_deadline()?;
            let set = self.filter_set(s, &q.filter)?;
            self.stats.plan.push(format!("seg {}: aggregate {} rows into groups of {}", s.ord, set.len(), q.group_by.len()));
            // Multi-valued fields: each document's value codes, from the postings.
            let mut multi: HashMap<usize, HashMap<DocId, Vec<u32>>> = HashMap::new();
            for &f in &fields {
                if self.schema.fields[f].multi {
                    let kw = self.keyword(s, f)?;
                    let mut m: HashMap<DocId, Vec<u32>> = HashMap::new();
                    for c in 0..kw.n_values() as u32 {
                        tick.tick()?;
                        for d in kw.docs_code(c).intersect(&set).iter() {
                            m.entry(d).or_default().push(c);
                        }
                    }
                    multi.insert(f, m);
                }
            }
            for d in set.iter() {
                tick.tick()?;
                // A document belongs to one group per combination of its
                // group values.
                let mut keys: Vec<Vec<agg::Part>> = vec![Vec::new()];
                for &g in &q.group_by {
                    self.agg_values(s, g, d, &multi, &mut gv)?;
                    if gv.is_empty() {
                        gv.push(Value::Null);
                    }
                    keys = keys.into_iter().flat_map(|k| gv.iter().map(move |v| [k.clone(), vec![agg::Part::of(v)]].concat())).collect();
                }
                for key in keys {
                    let accs = groups.entry(key, needed)?;
                    for (a, p) in accs.iter_mut().zip(needed) {
                        match p {
                            Projection::Agg { field, .. } => {
                                self.agg_values(s, *field, d, &multi, &mut vals)?;
                                for v in &vals {
                                    let more = a.add(v);
                                    if more > 0 {
                                        self.charge.add(more)?;
                                    }
                                }
                            }
                            _ => a.add_row(),
                        }
                    }
                }
            }
        }
        Ok(groups)
    }

    /// `Error::Timeout` once [`ExecOptions::deadline`] has passed or the
    /// query was cancelled.
    fn check_deadline(&self) -> Result<()> {
        self.opts.check()
    }

    /// A [`Ticker`] of this query, for a loop over documents.
    fn ticker(&self) -> Ticker<'a> {
        Ticker::new(self.opts, Ticker::DOCS)
    }

    /// BM25's `k1` and `b` (`k1 >= 0` and `0 <= b <= 1`, under which a
    /// score grows with a term's frequency, as the precomputed bounds of
    /// MaxScore and Block-Max WAND assume) and the top-k algorithm.
    fn bm25_params(&self, opts: &Opts) -> Result<(Bm25Params, TopKAlgo)> {
        let mut params = self.opts.bm25;
        if let Some(v) = opts.get("k1").and_then(|v| v.as_f64()) {
            if !(v >= 0.0 && v.is_finite()) {
                return Err(err(format!("bm25's k1 must be 0 or more, not {v}")));
            }
            params.k1 = v as f32;
        }
        if let Some(v) = opts.get("b").and_then(|v| v.as_f64()) {
            if !(0.0..=1.0).contains(&v) {
                return Err(err(format!("bm25's b must be between 0 and 1, not {v}")));
            }
            params.b = v as f32;
        }
        let algo = match opts.get("algo").and_then(|v| v.as_str()) {
            Some("exhaustive") => TopKAlgo::Exhaustive,
            Some("maxscore") => TopKAlgo::MaxScore,
            Some("bmw") | Some("wand") => TopKAlgo::BlockMaxWand,
            _ => self.opts.bm25_algo,
        };
        Ok((params, algo))
    }

    /// The top `k` rows by BM25; with `ties`, also every row tied with the
    /// `k`-th.
    fn bm25_top(&mut self, field: usize, query: &str, opts: &Opts, p: &Pred, k: usize, ties: bool) -> Result<Vec<Hit>> {
        let (q, stats) = self.bm25_scoring(field, query, opts)?;
        let terms = &q.terms;
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let names: Vec<String> = terms.iter().map(|(t, _)| t.clone()).collect();
        if terms.len() > q.n_base {
            let added: Vec<&str> = names[q.n_base..].iter().map(|t| t.as_str()).collect();
            self.stats.plan.push(format!("bm25 expanded with {} terms: {}", added.len(), added.join(", ")));
        }
        let (params, algo) = self.bm25_params(opts)?;
        let per = self.per_segment(|ex, s, l| {
            let t = ex.text(s, field)?;
            // Block-Max WAND fully scores a fraction of the postings it meets.
            let candidates = names.iter().map(|w| t.doc_freq(w) as f64).sum::<f64>() * 0.3;
            let mode = match ex.opts.filter_mode {
                Some(m) => m,
                None => {
                    let est = SegEst { ex, s, params: None, local_sel: None };
                    ex.planner(&est).filter_mode(p, candidates)
                }
            };
            let f = ex.seg_filter_l(s, p, mode, l)?;
            let hits = t.top_k_weighted(terms, &stats, params, k, f.as_dyn(), algo, ties);
            l.plan.push(format!("seg {}: bm25 {algo:?}, filter {mode:?}", s.ord));
            Ok(hits.into_iter().map(|(doc, sc)| Hit { seg: s.ord, doc, score: sc as f64 }).collect::<Vec<_>>())
        })?;
        let mut hits = merge_top(per.concat(), usize::MAX);
        let keep = if ties && k > 0 && hits.len() > k { k + hits[k..].iter().take_while(|h| h.score == hits[k - 1].score).count() } else { k };
        hits.truncate(keep);
        Ok(hits)
    }

    fn search_params(&self, opts: &Opts) -> SearchParams {
        let mut sp = self.opts.vector_params.clone();
        let num = |k: &str| opts.get(k).and_then(|v| v.as_f64()).map(|x| x as usize);
        if let Some(x) = num("nprobe") {
            sp.nprobe = x;
        }
        if let Some(x) = num("ef") {
            sp.ef = x;
        }
        if let Some(x) = num("rerank") {
            sp.rerank = x;
        }
        sp
    }

    fn vector_top(&mut self, field: usize, q: &[f32], opts: &Opts, p: &Pred, k: usize) -> Result<Vec<Hit>> {
        let base = self.search_params(opts);
        let forced = match opts.get("strategy").and_then(|v| v.as_str()) {
            Some("prefilter") => Some(VectorStrategy::PreFilter),
            Some("postfilter") => Some(VectorStrategy::PostFilter),
            Some("index") | Some("ivf") | Some("hnsw") => Some(VectorStrategy::InIndex),
            Some("flat") => Some(VectorStrategy::Flat),
            Some("auto") => None,
            Some(other) => return Err(err(format!("unknown vector strategy {other}"))),
            None => self.opts.vector,
        };
        let per = self.per_segment(|ex, s, l| {
            let vr = ex.vector(s, field)?;
            let has_index = vr.index_kind() != "flat";
            let (strategy, mode) = {
                let est = SegEst { ex, s, params: Some((field, base.clone())), local_sel: None };
                let planner = ex.planner(&est);
                match forced {
                    Some(VectorStrategy::PreFilter) | Some(VectorStrategy::Flat) => (forced.unwrap(), FilterMode::Materialize),
                    Some(st) => {
                        let e = planner.estimate(p);
                        let mode = if *p == Pred::True { FilterMode::None } else if e.probeable { FilterMode::Probe } else { FilterMode::Materialize };
                        (st, ex.opts.filter_mode.unwrap_or(mode))
                    }
                    None => {
                        let global = planner.estimate(p).card / est.n_docs().max(1.0);
                        let (mut st, mut mode, _) = planner.vector_plan(field, p, k, has_index);
                        // An index strategy chosen on the global selectivity is
                        // re-checked against the selectivity near the query.
                        if st != VectorStrategy::PreFilter && has_index && *p != Pred::True {
                            if let Some(ls) = ex.local_selectivity(s, vr, q, p, &base, global, l)? {
                                let est = SegEst { ex, s, params: Some((field, base.clone())), local_sel: Some(ls) };
                                (st, mode, _) = ex.planner(&est).vector_plan(field, p, k, has_index);
                                l.plan.push(format!("seg {}: selectivity {global:.4} global, {ls:.4} near the query", s.ord));
                            }
                        }
                        (st, ex.opts.filter_mode.unwrap_or(mode))
                    }
                }
            };
            let f = ex.seg_filter_l(s, p, if *p == Pred::True { FilterMode::None } else { mode }, l)?;
            let mut sp = base.clone();
            sp.strategy = to_vector_strategy(strategy, vr.index_kind(), f.as_dyn().is_some());
            let out = vr.search(q, k, &sp, f.as_dyn());
            l.scored += out.stats.candidates as u64;
            l.plan.push(format!("seg {}: vector {:?} ({:?} ran), filter {mode:?}", s.ord, strategy, out.stats.strategy));
            Ok(out.hits.into_iter().map(|h| Hit { seg: s.ord, doc: h.doc, score: h.score as f64 }).collect::<Vec<_>>())
        })?;
        Ok(merge_top(per.concat(), k))
    }

    /// The fraction of the query's nearest neighbours that pass the filter,
    /// from a cheap unfiltered index search (32 results, 160 when the first
    /// sample holds at most one match). A filter correlated
    /// with the query, or anti-correlated, has a local selectivity far from
    /// its global one, and the local one decides how far an index search
    /// must go to collect k matches. Only measured when the global
    /// selectivity leaves the choice open.
    #[allow(clippy::too_many_arguments)]
    fn local_selectivity(&self, s: &Seg<'a>, vr: &VectorReader, q: &[f32], p: &Pred, base: &SearchParams, global: f64, l: &mut Local) -> Result<Option<f64>> {
        if !(0.0005..0.6).contains(&global) {
            return Ok(None);
        }
        let probeable = {
            let est = SegEst { ex: self, s, params: None, local_sel: None };
            self.planner(&est).estimate(p).probeable
        };
        let f = self.seg_filter_l(s, p, if probeable { FilterMode::Probe } else { FilterMode::Materialize }, l)?;
        let sample = |m: usize| {
            let mut sp = base.clone();
            sp.strategy = to_vector_strategy(VectorStrategy::InIndex, vr.index_kind(), false);
            sp.ef = m;
            sp.rerank = 0;
            sp.nprobe = sp.nprobe.min(if m > 32 { 8 } else { 4 });
            let out = vr.search(q, m, &sp, None);
            let pass = out.hits.iter().filter(|h| f.as_dyn().is_none_or(|f| f.contains(h.doc))).count();
            (pass, out.hits.len())
        };
        // 32 neighbours resolve selectivities down to a few per cent. When
        // they hold almost no matches, a 160-neighbour sample tells a filter
        // that is merely selective from one whose members lie far from the
        // query, where an index search would wander for tens of ms.
        let (mut pass, mut m) = sample(32);
        if pass <= 1 && global > 0.02 {
            (pass, m) = sample(160);
        }
        Ok(Some((pass as f64 + 0.5) / (m as f64 + 1.0)))
    }

    /// Weighted fusion: the union of each input's top candidates, re-scored
    /// exactly by every input, normalised (min-max by default) and summed.
    /// With `exact => true` on any input, every document passing the filter
    /// is a candidate.
    fn weighted(&mut self, terms: &[(f64, Score)], p: &Pred, k: usize) -> Result<Vec<Hit>> {
        let opt = |key: &str| terms.iter().find_map(|(_, s)| s.opts().and_then(|o| o.get(key)).cloned());
        let exact = opt("exact").and_then(|v| v.as_i64()).unwrap_or(0) != 0;
        let depth = self.rows_at_most(opt("depth").and_then(|v| v.as_f64()).map(|d| d as usize).unwrap_or(self.opts.fusion_depth).max(k));
        let norm = match opt("norm").as_ref().and_then(|v| v.as_str()) {
            Some("none") => Norm::None,
            _ => Norm::MinMax,
        };
        let mut cands: Vec<(usize, DocId)> = Vec::new();
        if exact {
            let segs = self.segs.clone();
            for s in segs.iter() {
                self.check_deadline()?;
                let set = self.filter_set(s, p)?;
                cands.extend(set.iter().map(|d| (s.ord, d)));
            }
        } else {
            for (_, s) in terms {
                cands.extend(self.input_top(s, p, depth, false)?.into_iter().map(|h| (h.seg, h.doc)));
            }
            cands.sort_unstable();
            cands.dedup();
        }
        self.stats.scored += cands.len() as u64;
        self.stats.plan.push(format!("weighted: {} candidates ({}), {norm:?}", cands.len(), if exact { "exact" } else { "top-depth union" }));
        let mut scores = Vec::with_capacity(terms.len());
        for (_, s) in terms {
            scores.push(self.exact_scores(s, &cands)?);
        }
        let weights: Vec<f64> = terms.iter().map(|(w, _)| *w).collect();
        let mut hits: Vec<Hit> = fusion::weighted(&weights, &scores, norm).into_iter().map(|(i, score)| Hit { seg: cands[i].0, doc: cands[i].1, score }).collect();
        // Rows tied on the fused score go to the smaller key, as over shards.
        self.order_ties_by_key(&mut hits, false);
        hits.truncate(k);
        Ok(hits)
    }

    /// `a * b * ...`: the top `depth` rows of each relevance factor (of
    /// every factor if none is), scored exactly by every factor.
    fn product(&mut self, factors: &[Score], p: &Pred, k: usize) -> Result<Vec<Hit>> {
        let depth = self.rows_at_most(factors.iter().find_map(|s| s.opts().and_then(|o| o.get("depth")).and_then(|v| v.as_f64())).map_or(self.opts.fusion_depth, |d| d as usize).max(k));
        let any_relevance = factors.iter().any(Score::is_relevance);
        let mut cands: Vec<(usize, DocId)> = Vec::new();
        for f in factors.iter().filter(|f| f.is_relevance() || !any_relevance) {
            cands.extend(self.input_top(f, p, depth, false)?.into_iter().map(|h| (h.seg, h.doc)));
        }
        cands.sort_unstable();
        cands.dedup();
        self.stats.scored += cands.len() as u64;
        self.stats.plan.push(format!("product: {} candidates from {} factors, depth {depth}", cands.len(), factors.len()));
        let mut scores = Vec::with_capacity(factors.len());
        for f in factors {
            scores.push(self.exact_scores(f, &cands)?);
        }
        let ones = vec![1.0; factors.len()];
        let mut hits: Vec<Hit> = fusion::weighted(&ones, &scores, Norm::Product).into_iter().map(|(i, score)| Hit { seg: cands[i].0, doc: cands[i].1, score }).collect();
        self.sort_by_score_then_key(&mut hits, true, false);
        hits.truncate(k);
        Ok(hits)
    }

    /// Shard mode: every candidate of a fused score with each input's exact
    /// score, ordered by the fused score over this shard's candidates. The
    /// coordinator takes each input's top `depth` over all shards and fuses
    /// those, which is what one database holding every shard would do.
    fn fusion_candidates(&mut self, score: &Score, p: &Pred, k: usize) -> Result<Vec<Hit>> {
        let (inputs, depth, exact): (Vec<&Score>, usize, bool) = match score {
            Score::Rrf { inputs, depth, .. } => (inputs.iter().collect(), *depth, false),
            Score::Weighted(terms) => {
                let opt = |key: &str| terms.iter().find_map(|(_, s)| s.opts().and_then(|o| o.get(key)).cloned());
                let exact = opt("exact").and_then(|v| v.as_i64()).unwrap_or(0) != 0;
                let depth = opt("depth").and_then(|v| v.as_f64()).map(|d| d as usize).unwrap_or(self.opts.fusion_depth);
                (terms.iter().map(|(_, s)| s).collect(), depth, exact)
            }
            Score::Product(factors) => {
                let depth = factors.iter().find_map(|s| s.opts().and_then(|o| o.get("depth")).and_then(|v| v.as_f64())).map_or(self.opts.fusion_depth, |d| d as usize);
                (factors.iter().collect(), depth, false)
            }
            _ => unreachable!("fused scores only"),
        };
        let depth = self.rows_at_most(depth.max(k));
        // A product's candidates come from its relevance factors only.
        let generates = |s: &Score| match score {
            Score::Product(factors) => s.is_relevance() || !factors.iter().any(Score::is_relevance),
            _ => true,
        };
        let mut lists = Vec::with_capacity(inputs.len());
        for s in &inputs {
            lists.push(if generates(s) { self.input_top(s, p, depth, false)?.into_iter().map(|h| (h.seg, h.doc)).collect::<Vec<_>>() } else { Vec::new() });
        }
        let mut cands: Vec<(usize, DocId)> = Vec::new();
        if exact {
            let segs = self.segs.clone();
            for s in segs.iter() {
                self.check_deadline()?;
                let set = self.filter_set(s, p)?;
                cands.extend(set.iter().map(|d| (s.ord, d)));
            }
        } else {
            cands = lists.concat();
            cands.sort_unstable();
            cands.dedup();
        }
        let mut scores = Vec::with_capacity(inputs.len());
        for s in &inputs {
            scores.push(self.exact_scores(s, &cands)?);
        }
        let ranked: Vec<Vec<bool>> = lists
            .iter()
            .map(|l| {
                let set: HashSet<&(usize, DocId)> = l.iter().collect();
                cands.iter().map(|c| set.contains(c)).collect()
            })
            .collect();
        let mut hits: Vec<Hit> = match score {
            Score::Rrf { k, .. } => fusion::rrf(&lists, *k).into_iter().map(|((seg, doc), score)| Hit { seg, doc, score }).collect(),
            Score::Weighted(terms) => {
                let norm = match terms.iter().find_map(|(_, s)| s.opts().and_then(|o| o.get("norm")).cloned()).as_ref().and_then(|v| v.as_str()) {
                    Some("none") => Norm::None,
                    _ => Norm::MinMax,
                };
                let weights: Vec<f64> = terms.iter().map(|(w, _)| *w).collect();
                fusion::weighted(&weights, &scores, norm).into_iter().map(|(i, score)| Hit { seg: cands[i].0, doc: cands[i].1, score }).collect()
            }
            Score::Product(_) => {
                let ones = vec![1.0; inputs.len()];
                fusion::weighted(&ones, &scores, Norm::Product).into_iter().map(|(i, score)| Hit { seg: cands[i].0, doc: cands[i].1, score }).collect()
            }
            _ => unreachable!(),
        };
        self.sort_by_score_then_key(&mut hits, true, false);
        self.stats.scored += cands.len() as u64;
        self.stats.plan.push(format!("shard fusion: {} candidates from {} inputs, depth {depth}", cands.len(), inputs.len()));
        self.parts = Some((cands, scores, ranked));
        Ok(hits)
    }

    /// Exact scores of `cands` (sorted by segment, then docID) for one input.
    fn exact_scores(&self, score: &Score, cands: &[(usize, DocId)]) -> Result<Vec<f32>> {
        let mut out = vec![0f32; cands.len()];
        let by_seg = group_by_seg(cands);
        match score {
            Score::Bm25 { field, text: q, opts } => {
                let (q, stats) = self.bm25_scoring(*field, q, opts)?;
                let terms = q.terms;
                let (params, _) = self.bm25_params(opts)?;
                for (seg, range) in by_seg {
                    self.check_deadline()?;
                    let s = &self.seg_by_ord(seg);
                    let docs: Vec<DocId> = cands[range.clone()].iter().map(|c| c.1).collect();
                    let sc = self.text(s, *field)?.score_weighted(&terms, &stats, params, &docs);
                    out[range].copy_from_slice(&sc);
                }
            }
            Score::Sparse { field, terms, .. } => {
                for (seg, range) in by_seg {
                    self.check_deadline()?;
                    let docs: Vec<DocId> = cands[range.clone()].iter().map(|c| c.1).collect();
                    let sc = self.text(&self.seg_by_ord(seg), *field)?.score_dot(terms, &docs);
                    out[range].copy_from_slice(&sc);
                }
            }
            Score::Similarity { field, vector, .. } => {
                for (seg, range) in by_seg {
                    self.check_deadline()?;
                    let s = &self.seg_by_ord(seg);
                    let vr = self.vector(s, *field)?;
                    for i in range {
                        out[i] = vr.similarity(cands[i].1, vector);
                    }
                }
            }
            Score::Column { field } => {
                for (seg, range) in by_seg {
                    self.check_deadline()?;
                    let s = &self.seg_by_ord(seg);
                    let col = self.numeric(s, *field)?.col();
                    for i in range {
                        out[i] = self.num_value(s, *field, col, cands[i].1).unwrap_or(f64::NEG_INFINITY) as f32;
                    }
                }
            }
            Score::Decay { field, decay, .. } => {
                for (seg, range) in by_seg {
                    self.check_deadline()?;
                    let s = &self.seg_by_ord(seg);
                    let col = self.numeric(s, *field)?.col();
                    for i in range {
                        out[i] = self.num_value(s, *field, col, cands[i].1).map_or(0.0, |v| decay.eval(v)) as f32;
                    }
                }
            }
            Score::Ppr { .. } | Score::Hops { .. } => {
                for (i, c) in cands.iter().enumerate() {
                    out[i] = self.graph_score(score, c.0, c.1) as f32;
                }
            }
            _ => return Err(err("weighted inputs must be bm25, similarity, ppr, hops, decay or a column")),
        }
        Ok(out)
    }

    fn seg_by_ord(&self, ord: usize) -> Seg<'a> {
        self.segs.iter().find(|s| s.ord == ord).expect("candidate from a known segment").clone()
    }

    // ---- facets ----------------------------------------------------------

    /// The groups of one keyword field and their counts, from the field's
    /// facet counters (no per-document work).
    fn facets(&mut self, q: &Select, g: usize, needed: &[Projection]) -> Result<agg::Groups> {
        let mut counts: HashMap<String, u64> = HashMap::new();
        let segs = self.segs.clone();
        for s in segs.iter() {
            self.check_deadline()?;
            let set = self.filter_set(s, &q.filter)?;
            let kw = self.keyword(s, g)?;
            let strategy = kw.choose_facet_strategy(&set);
            self.stats.plan.push(format!("seg {}: facet {strategy:?} over {} docs", s.ord, set.len()));
            if kw.is_multi() {
                self.stats.plan.push(format!("seg {}: multi-valued, each value of a document counts", s.ord));
            }
            for (code, c) in kw.facet_code_counts(&set, strategy).into_iter().enumerate() {
                if c > 0 {
                    let n = counts.len();
                    let v = kw.value(code as u32);
                    *counts.entry(v.to_string()).or_insert(0) += c;
                    if counts.len() > n {
                        self.charge.add(48 + v.len() as u64)?;
                    }
                }
            }
            if let Some(max) = self.opts.max_groups.filter(|&m| counts.len() > m) {
                return Err(agg::too_many_groups(max));
            }
        }
        // A multi-valued INT field's values are int_key strings.
        let ints = self.schema.fields[g].ty == FieldType::Int;
        let mut groups = agg::Groups::default();
        for (v, c) in counts {
            let key = if ints { forward::int_of_key(&v).map_or(agg::Part::Null, agg::Part::Int) } else { agg::Part::Str(v) };
            for a in groups.entry(vec![key], needed)? {
                *a = agg::Acc::Count(c as i64);
            }
        }
        Ok(groups)
    }

    // ---- output ----------------------------------------------------------

    fn key(&self, h: &Hit) -> Value {
        self.snap.key(h.seg, h.doc).unwrap_or(Value::Null)
    }

    fn sort_by_score_then_key(&self, hits: &mut [Hit], score_desc: bool, key_desc: bool) {
        let mut keyed: Vec<(Hit, Value)> = hits.iter().map(|h| (*h, self.key(h))).collect();
        keyed.sort_by(|(a, ka), (b, kb)| {
            let s = if score_desc { b.score.total_cmp(&a.score) } else { a.score.total_cmp(&b.score) };
            s.then_with(|| {
                let c = cmp_values(ka, kb);
                if key_desc { c.reverse() } else { c }
            })
        });
        for (h, (k, _)) in hits.iter_mut().zip(keyed) {
            *h = k;
        }
    }

    fn project(&self, q: &Select, hits: &[Hit]) -> Result<Vec<Vec<Value>>> {
        let stored: Vec<usize> = q
            .projection
            .iter()
            .filter_map(|(_, p)| match p {
                // Vectors live in their index, not the doc store.
                Projection::Field(i)
                    if self.schema.fields[*i].stored
                        && Some(*i) != self.schema.primary_key
                        && !matches!(self.schema.fields[*i].ty, FieldType::Vector { .. }) =>
                {
                    Some(*i)
                }
                _ => None,
            })
            .collect();
        let mut rows = Vec::with_capacity(hits.len());
        for h in hits {
            let s = &self.seg_by_ord(h.seg);
            let fetched = match (s.r.store()?, stored.is_empty()) {
                (Some(store), false) => store.get_fields(h.doc, &stored),
                _ => Vec::new(),
            };
            let mut row = Vec::with_capacity(q.projection.len());
            for (_, p) in &q.projection {
                row.push(match p {
                    // A row without the ordering column's value scores ±inf.
                    Projection::Score if h.score.is_finite() => Value::Float(h.score),
                    Projection::Score => Value::Null,
                    Projection::Count | Projection::Agg { .. } => Value::Null,
                    Projection::Hops(g) => self.hops_of(*g, h.seg, h.doc).map_or(Value::Null, |x| Value::Int(x as i64)),
                    Projection::Field(i) if Some(*i) == self.schema.primary_key => self.key(h),
                    Projection::Field(i) if self.updates(s, *i).is_some_and(|u| u.get(*i, h.doc).is_some()) => {
                        self.updates(s, *i).and_then(|u| u.get(*i, h.doc)).cloned().unwrap_or(Value::Null)
                    }
                    Projection::Field(i) => match stored.iter().position(|x| x == i) {
                        Some(j) => fetched.get(j).cloned().unwrap_or(Value::Null),
                        None => self.unstored_value(s, *i, h.doc),
                    },
                });
            }
            self.charge.row(&row)?;
            rows.push(row);
        }
        Ok(rows)
    }

    fn unstored_value(&self, s: &Seg<'a>, field: usize, d: DocId) -> Value {
        match self.schema.fields[field].ty {
            FieldType::Vector { .. } => s.r.vector(field).ok().flatten().map_or(Value::Null, |v| Value::Vector(v.vector(d).into())),
            FieldType::Int | FieldType::Float => s.r.numeric(field).ok().flatten().map_or(Value::Null, |n| n.col().get(d)),
            FieldType::Keyword => s.r.keyword(field).ok().flatten().and_then(|k| k.value_of_doc(d)).map_or(Value::Null, |v| Value::Str(v.to_string())),
            FieldType::Text { .. } | FieldType::Sparse => Value::Null,
            FieldType::Edges => s.r.graph(field).ok().flatten().map_or(Value::Null, |g| g.edges_value(d)),
        }
    }
}

fn merge_top(mut hits: Vec<Hit>, k: usize) -> Vec<Hit> {
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then((a.seg, a.doc).cmp(&(b.seg, b.doc))));
    hits.truncate(k);
    hits
}

/// The BM25 inputs of a query and of the subqueries that seed its graphs.
fn bm25_leaves<'q>(q: &'q Select, out: &mut Vec<(usize, &'q str, &'q Opts)>) {
    if let Some(OrderBy::Score { score, .. }) = &q.order {
        for l in score.leaves() {
            if let Score::Bm25 { field, text, opts } = l {
                out.push((*field, text, opts));
            }
        }
    }
    for g in &q.graphs {
        let seeds = match &g.kind {
            crate::query::logical::GraphKind::Path { to, .. } => vec![&g.seeds, to],
            _ => vec![&g.seeds],
        };
        for s in seeds {
            if let Seeds::Query(sub) = s {
                bm25_leaves(sub, out);
            }
        }
    }
}

fn has_graphs(q: &Select) -> bool {
    !q.graphs.is_empty()
}

/// The terms of a `bm25` query in order, repeats included; `stopwords =>
/// 0` keeps query stopwords.
fn query_term_seq(q: &str, opts: &Opts) -> Vec<String> {
    let drop = opts.get("stopwords").and_then(|v| v.as_f64()).map_or(true, |x| x != 0.0);
    text::query_term_seq(q, drop)
}

/// The weighted terms of a `bm25` query ([`Executor::bm25_query`]).
#[derive(Default)]
struct Bm25Query {
    terms: Vec<(String, f32)>,
    /// Indices into `terms` that stand for one query term.
    groups: Vec<Vec<usize>>,
    /// The query's own terms, before its expansions.
    n_base: usize,
}

/// Gives each group's terms the largest document frequency among them, so
/// that a group scores like the term it stands for rather than like its
/// rarest variant.
fn blend_groups(q: &Bm25Query, stats: &mut CollectionStats) {
    for g in q.groups.iter().filter(|g| g.len() > 1) {
        let df = g.iter().map(|&i| stats.df.get(&q.terms[i].0).copied().unwrap_or(0)).max().unwrap_or(0);
        for &i in g {
            stats.df.insert(q.terms[i].0.clone(), df);
        }
    }
}

fn group_by_seg(cands: &[(usize, DocId)]) -> Vec<(usize, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..=cands.len() {
        if i == cands.len() || cands[i].0 != cands[start].0 {
            out.push((cands[start].0, start..i));
            start = i;
        }
    }
    out
}

/// Orders primary keys and other values: strings and integers by value,
/// anything else as numbers.
pub fn cmp_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            (x, y) => x.is_some().cmp(&y.is_some()),
        },
    }
}

fn to_vector_strategy(s: VectorStrategy, index_kind: &str, filtered: bool) -> vector::Strategy {
    match s {
        VectorStrategy::PreFilter => vector::Strategy::PreFilter,
        VectorStrategy::Flat => vector::Strategy::Flat,
        VectorStrategy::PostFilter if filtered => vector::Strategy::PostFilter,
        VectorStrategy::PostFilter | VectorStrategy::InIndex => match index_kind {
            "ivf" => vector::Strategy::Ivf,
            "hnsw" => vector::Strategy::Hnsw,
            _ => vector::Strategy::Flat,
        },
    }
}

/// Planner statistics for one segment, answered by its indexes.
struct SegEst<'e, 'a> {
    ex: &'e Executor<'a>,
    s: &'e Seg<'a>,
    /// Vector field and search parameters, for vector cost estimates.
    params: Option<(usize, SearchParams)>,
    /// Selectivity of the filter near the query (see
    /// [`Executor::local_selectivity`]); index strategies are costed with it.
    local_sel: Option<f64>,
}

impl Estimator for SegEst<'_, '_> {
    fn n_docs(&self) -> f64 {
        self.s.view.n_live as f64
    }
    fn match_count(&self, field: usize, query: &str, expand: &Expand) -> f64 {
        match (self.s.r.text(field).ok().flatten(), self.ex.match_query(field, query, expand)) {
            (Some(t), Ok(q)) => t.estimate(&q),
            _ => self.n_docs(),
        }
    }
    fn match_postings(&self, field: usize, query: &str, expand: &Expand) -> f64 {
        match (self.s.r.text(field).ok().flatten(), self.ex.match_query(field, query, expand)) {
            (Some(t), Ok(q)) => t.eval_cost(&q),
            _ => self.n_docs(),
        }
    }
    fn range_count(&self, field: usize, lo: f64, hi: f64) -> f64 {
        if self.ex.schema.fields[field].multi {
            return self.s.r.keyword(field).ok().flatten().map_or(self.n_docs(), |kw| kw.int_codes(lo, hi).map(|c| kw.count_code(c) as f64).sum());
        }
        self.s.r.numeric(field).ok().flatten().map_or(self.n_docs(), |n| n.col().estimate(lo, hi))
    }
    fn range_cost(&self, field: usize, lo: f64, hi: f64) -> Option<f64> {
        let n = self.s.r.numeric(field).ok().flatten()?;
        forward::range_costs(n.col(), n.index(), lo, hi, false).map(|(scan, post)| scan.min(post))
    }
    fn present_count(&self, field: usize) -> f64 {
        if let Some(kw) = self.s.r.keyword(field).ok().flatten() {
            return kw.n_present() as f64;
        }
        self.s.r.numeric(field).ok().flatten().map_or(self.n_docs(), |n| n.col().n_present() as f64)
    }
    fn in_count(&self, field: usize, values: &[Value]) -> f64 {
        if let Some(kw) = self.s.r.keyword(field).ok().flatten() {
            return values.iter().filter_map(forward::key_of).map(|v| kw.count(&v) as f64).sum();
        }
        values.iter().filter_map(|v| v.as_f64()).map(|v| self.range_count(field, v, v)).sum()
    }
    fn graph_count(&self, id: usize) -> f64 {
        self.ex.graph_count(id, self.s.ord)
    }
    fn vector_cost(&self, field: usize, s: VectorStrategy, k: usize, card: Option<f64>) -> f64 {
        let Some(vr) = self.s.r.vector(field).ok().flatten() else { return f64::INFINITY };
        let params = self.params.as_ref().map(|(_, p)| p.clone()).unwrap_or_else(|| self.ex.opts.vector_params.clone());
        let st = to_vector_strategy(s, vr.index_kind(), card.is_some());
        // In-index search pays for the selectivity near the query, in both
        // directions. Post-filtering sizes its candidate pool from the global
        // selectivity, so correlation cannot make it cheaper, only
        // anti-correlation dearer (retries).
        let card = match (s, self.local_sel, card) {
            (VectorStrategy::InIndex, Some(l), _) => Some(l * self.n_docs()),
            (VectorStrategy::PostFilter, Some(l), Some(c)) => Some(c.min(l * self.n_docs())),
            _ => card,
        };
        vr.estimate_cost(st, k, card.map(|c| c.max(0.0) as usize), &params)
    }
}

/// Whether `v` is an integer in `[lo, hi]` (a FLOAT in a column written
/// before it became INT counts by its value).
fn int_in(v: &Value, lo: i64, hi: i64) -> bool {
    match v {
        Value::Int(x) => lo <= *x && *x <= hi,
        v => v.as_f64().is_some_and(|x| lo as f64 <= x && x <= hi as f64),
    }
}

/// Whether two numbers are equal, integers exactly.
fn num_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        _ => a.as_f64().zip(b.as_f64()).is_some_and(|(x, y)| x == y),
    }
}

/// The keys of column `col` equal to the number `v`.
fn value_keys(col: &NumericColumn, v: &Value) -> Option<KeyRange> {
    match v {
        Value::Int(x) => col.int_key_range(*x, *x),
        v => v.as_f64().and_then(|x| col.key_range(x, x)),
    }
}
