//! The bound form of a query: what to return, which rows qualify, and how to
//! rank them. The planner turns this into per-segment physical operators.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::value::Value;

/// A row predicate over schema fields (indices into `Schema::fields`).
#[derive(Clone, Debug, PartialEq)]
pub enum Pred {
    True,
    False,
    /// `MATCH(field, 'boolean query')` on a TEXT field (grammar: text module).
    Match { field: usize, query: String },
    /// Numeric range with inclusive bounds (`±inf` for open ends). Strict
    /// comparisons are normalised to inclusive ones at bind time.
    Range { field: usize, lo: f64, hi: f64 },
    /// Keyword (or numeric) equality with any of `values`.
    In { field: usize, values: Vec<Value> },
    And(Vec<Pred>),
    Or(Vec<Pred>),
    Not(Box<Pred>),
    /// Membership in the result of `Select::graphs[id]` (`REACHABLE`,
    /// `PATH`), computed before the rest of the query.
    Graph { id: usize },
}

/// Which way edges are followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From a row to the targets of its edges.
    Out,
    /// From a row to the rows whose edges point at it.
    In,
    Both,
}

/// Where a traversal starts.
#[derive(Clone, Debug, PartialEq)]
pub enum Seeds {
    /// Primary keys.
    Keys(Vec<Value>),
    /// The rows of a subquery over the same table (with their scores, which
    /// `ppr(..., weighted => 1)` uses as restart weights).
    Query(Box<Select>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum GraphKind {
    /// Rows within `min_hops..=max_hops` of a seed.
    Reach { min_hops: u32, max_hops: u32 },
    /// The rows of one shortest path from a seed to one of `to`.
    Path { to: Seeds, max_hops: u32 },
    /// Personalized PageRank from the seeds (forward push).
    Ppr { alpha: f64, eps: f64, max_pushes: u64, weighted: bool },
}

/// A graph computation over one EDGES field, or over a VECTOR field whose
/// `k` most similar rows count as a row's neighbours.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphQuery {
    pub field: usize,
    pub seeds: Seeds,
    pub kind: GraphKind,
    pub dir: Direction,
    /// Only edges with one of these labels (empty: any).
    pub labels: Vec<String>,
    /// Traversals continue only through rows satisfying this (seeds and
    /// end points are exempt).
    pub through: Option<Pred>,
    /// Stop after meeting this many rows.
    pub max_nodes: usize,
    /// VECTOR fields: neighbours per row, and the least similarity.
    pub k: usize,
    pub min_sim: f64,
}

impl Pred {
    /// Flattens nested AND/OR and folds constants.
    pub fn simplify(self) -> Pred {
        match self {
            Pred::And(ps) => {
                let mut out = Vec::new();
                for p in ps.into_iter().map(Pred::simplify) {
                    match p {
                        Pred::True => {}
                        Pred::False => return Pred::False,
                        Pred::And(inner) => out.extend(inner),
                        p => out.push(p),
                    }
                }
                match out.len() {
                    0 => Pred::True,
                    1 => out.pop().unwrap(),
                    _ => Pred::And(out),
                }
            }
            Pred::Or(ps) => {
                let mut out = Vec::new();
                for p in ps.into_iter().map(Pred::simplify) {
                    match p {
                        Pred::False => {}
                        Pred::True => return Pred::True,
                        Pred::Or(inner) => out.extend(inner),
                        p => out.push(p),
                    }
                }
                match out.len() {
                    0 => Pred::False,
                    1 => out.pop().unwrap(),
                    _ => Pred::Or(out),
                }
            }
            Pred::Not(p) => match p.simplify() {
                Pred::True => Pred::False,
                Pred::False => Pred::True,
                Pred::Not(q) => *q,
                p => Pred::Not(Box::new(p)),
            },
            p => p,
        }
    }
}

/// Named options of a search function, e.g. `nprobe => 32`.
pub type Opts = BTreeMap<String, Value>;

/// A ranking function. Larger is better for every variant.
#[derive(Clone, Debug, PartialEq)]
pub enum Score {
    /// Disjunctive BM25 of the analysed terms of `text` in a TEXT field.
    Bm25 { field: usize, text: String, opts: Opts },
    /// Dot product with `vector` (cosine for unit vectors). `distance(...)`
    /// binds to this with the sort direction flipped.
    Similarity { field: usize, vector: Arc<[f32]>, opts: Opts },
    /// Reciprocal rank fusion: `Σ 1 / (k + rank_i)` over the inputs' rankings
    /// of their top `depth` rows.
    Rrf { inputs: Vec<Score>, k: f64, depth: usize },
    /// A weighted sum of scores (`0.3 * bm25(..) + 0.7 * similarity(..)`).
    Weighted(Vec<(f64, Score)>),
    /// A numeric column's value.
    Column { field: usize },
    /// Personalized PageRank of `Select::graphs[graph]`.
    Ppr { graph: usize },
    /// Hop distance in `Select::graphs[graph]`, nearest first (as
    /// `-hops`) or farthest first.
    Hops { graph: usize, far_first: bool },
}

impl Score {
    /// Every leaf (BM25, similarity or column) of the expression.
    pub fn leaves(&self) -> Vec<&Score> {
        match self {
            Score::Rrf { inputs, .. } => inputs.iter().flat_map(|s| s.leaves()).collect(),
            Score::Weighted(ts) => ts.iter().flat_map(|(_, s)| s.leaves()).collect(),
            s => vec![s],
        }
    }

    pub fn opts(&self) -> Option<&Opts> {
        match self {
            Score::Bm25 { opts, .. } | Score::Similarity { opts, .. } => Some(opts),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OrderBy {
    /// Rank by a score, best first (`desc = true`) or worst first.
    Score { score: Score, desc: bool },
    /// Facet queries: order groups by their count.
    Count { desc: bool },
    /// Facet queries: order groups by their value.
    GroupValue { desc: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Projection {
    Field(usize),
    /// The ORDER BY score (`score()`), or the fused score for hybrids.
    Score,
    /// `COUNT(*)`: of the whole result, or per group.
    Count,
    /// `hops()`: the row's hop distance in `Select::graphs[id]`.
    Hops(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    /// Output column names and what they hold.
    pub projection: Vec<(String, Projection)>,
    pub filter: Pred,
    pub order: Option<OrderBy>,
    /// A KEYWORD field to group by (facets), with `COUNT(*)` per group.
    pub group_by: Option<usize>,
    /// Secondary order for rows that tie on the primary one.
    pub tiebreak: Option<Tiebreak>,
    pub limit: Option<usize>,
    pub offset: usize,
    /// Graph computations that `Pred::Graph`, `Score::Ppr`, `Score::Hops`
    /// and `Projection::Hops` refer to by index.
    pub graphs: Vec<GraphQuery>,
}

/// A secondary ORDER BY key (`ORDER BY rating DESC, id`; `ORDER BY 2 DESC, 1`).
#[derive(Clone, Debug, PartialEq)]
pub enum Tiebreak {
    /// The primary key field.
    Key { desc: bool },
    /// The group value of a facet query.
    GroupValue { desc: bool },
}
