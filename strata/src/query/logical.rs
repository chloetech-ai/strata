//! The bound form of a query: what to return, which rows qualify, and how to
//! rank them. The planner turns this into per-segment physical operators.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::query::ir::{ArithOp, Func, NormKind};
use crate::query::model::Opened;
use crate::value::Value;

/// A row predicate over schema fields (indices into `Schema::fields`).
#[derive(Clone, Debug, PartialEq)]
pub enum Pred {
    True,
    False,
    /// `MATCH(field, 'boolean query')` on a TEXT field (grammar: text
    /// module), with its words expanded by synonyms or typos as asked.
    Match { field: usize, query: String, expand: Expand },
    /// FLOAT range with inclusive bounds (`±inf` for open ends). Strict
    /// comparisons are normalised to inclusive ones at bind time.
    Range { field: usize, lo: f64, hi: f64 },
    /// INT range with inclusive bounds, exact (`i64::MIN` and `i64::MAX`
    /// for open ends): a double cannot tell 2^53 from 2^53 + 1.
    IntRange { field: usize, lo: i64, hi: i64 },
    /// Keyword (or numeric) equality with any of `values`.
    In { field: usize, values: Vec<Value> },
    /// Rows with a value of the field (`IS NOT NULL`; a multi-valued field
    /// needs at least one).
    Present { field: usize },
    And(Vec<Pred>),
    Or(Vec<Pred>),
    /// The rows the inner predicate does not hold: a plain complement.
    /// The binder hands the executor predicates in which NULL is already
    /// accounted for ([`Pred::three_valued`]).
    Not(Box<Pred>),
    /// Membership in the result of `Select::graphs[id]` (`REACHABLE`,
    /// `PATH`), computed before the rest of the query.
    Graph { id: usize },
    /// A condition on a subquery's rows (`IN (SELECT ...)`, `EXISTS`, a
    /// comparison with a scalar subquery), as the plan expression the
    /// subquery's plan sits in. A compound query's binder makes it
    /// ([`crate::query::compound`]); its plan runs the subquery first and
    /// hands the executor the condition on its values, never this.
    Sub(Box<crate::query::ir::Expr>),
}

/// How a full-text search widens its words (`MATCH(.., synonyms => 1,
/// fuzzy => 'auto')`, the same options on `bm25`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Expand {
    /// Each word also matches its synonyms from the synonym list
    /// ([`crate::text::synonyms`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synonyms: bool,
    /// Each word also matches indexed terms within this many edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuzzy: Option<Fuzzy>,
}

/// Edits a typo-tolerant word may be away from an indexed term.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fuzzy {
    /// By the word's length: none up to 2 letters, 1 up to 5, else 2.
    Auto,
    Edits(u8),
}

impl Fuzzy {
    pub fn edits(self, len: usize) -> u8 {
        match self {
            Fuzzy::Edits(n) => n,
            Fuzzy::Auto if len <= 2 => 0,
            Fuzzy::Auto if len <= 5 => 1,
            Fuzzy::Auto => 2,
        }
    }
}

impl Expand {
    /// The expansion options of a search function: `synonyms => 0 | 1`,
    /// `fuzzy => 0 | 1 | 2 | 'auto'`.
    pub fn from_opts(opts: &Opts) -> std::result::Result<Expand, String> {
        let synonyms = match opts.get("synonyms") {
            None => false,
            Some(v) => v.as_f64().ok_or("synonyms => 0 or 1")? != 0.0,
        };
        let fuzzy = match opts.get("fuzzy") {
            None => None,
            Some(Value::Str(s)) if s.eq_ignore_ascii_case("auto") => Some(Fuzzy::Auto),
            Some(v) => match v.as_f64() {
                Some(x) if x == 0.0 => None,
                Some(x) if x == 1.0 || x == 2.0 => Some(Fuzzy::Edits(x as u8)),
                _ => return Err("fuzzy => 0, 1, 2 or 'auto'".into()),
            },
        };
        Ok(Expand { synonyms, fuzzy })
    }

    pub fn is_none(&self) -> bool {
        !self.synonyms && self.fuzzy.is_none()
    }
}

/// Which way edges are followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Primary keys, each with its weight: the scores of the subquery rows
    /// that named it, summed, which `ppr(..., weighted => 1)` restarts at.
    Weighted(Vec<(Value, f64)>),
    /// The rows of a subquery over the same table (with their scores, which
    /// `ppr(..., weighted => 1)` uses as restart weights).
    Query(Box<Select>),
    /// The keys or values of a compound subquery's plan
    /// ([`crate::query::ir::Expr::Keys`], [`crate::query::ir::Expr::Values`]),
    /// which runs first: like [`Pred::Sub`], never the executor's.
    Plan(Box<crate::query::ir::Expr>),
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

/// A graph computation over one EDGES field, over an edge table, or over a
/// VECTOR field whose `k` most similar rows count as a row's neighbours.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphQuery {
    /// The EDGES or VECTOR field; with `edges`, the primary key.
    pub field: usize,
    /// The edge table walked instead of a field
    /// ([`crate::schema::Schema::with_edges`]).
    pub edges: Option<String>,
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
    /// The rows for which `self`, read with SQL's three-valued logic, is
    /// TRUE, as a predicate of plain sets. A comparison with NULL is
    /// UNKNOWN, and so is its negation, so `rating != 2` and `NOT rating >
    /// 2` leave out rows without a rating. NOTs are pushed down to the
    /// leaves (De Morgan's laws hold for three values too), and a negated
    /// comparison also asks for its field's value. MATCH, graph membership
    /// and `IS NULL` are never UNKNOWN, and neither is a comparison on a
    /// multi-valued field (`multi`), which holds when some value matches:
    /// its negation holds when none does, also for an empty list, as
    /// `NOT (x = ANY(list))` does. Apply it once, to a predicate as written.
    pub fn three_valued(self, multi: &dyn Fn(usize) -> bool) -> Pred {
        self.nnf(false, multi)
    }

    fn nnf(self, negated: bool, multi: &dyn Fn(usize) -> bool) -> Pred {
        match self {
            Pred::Not(p) => p.nnf(!negated, multi),
            Pred::And(ps) => {
                let ps = ps.into_iter().map(|p| p.nnf(negated, multi)).collect();
                if negated { Pred::Or(ps) } else { Pred::And(ps) }
            }
            Pred::Or(ps) => {
                let ps = ps.into_iter().map(|p| p.nnf(negated, multi)).collect();
                if negated { Pred::And(ps) } else { Pred::Or(ps) }
            }
            p if !negated => p,
            Pred::True => Pred::False,
            Pred::False => Pred::True,
            p @ (Pred::Range { field, .. } | Pred::IntRange { field, .. } | Pred::In { field, .. }) if !multi(field) => {
                Pred::And(vec![Pred::Present { field }, Pred::Not(Box::new(p))])
            }
            p => Pred::Not(Box::new(p)),
        }
    }

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
    /// Dot product of a SPARSE field's term weights with `terms`' (also
    /// `similarity(...)` in SQL).
    Sparse { field: usize, terms: Vec<(String, f32)>, opts: Opts },
    /// `maxsim(field, vectors)`: a row's best similarity to any of several
    /// query vectors.
    MaxSim { field: usize, vectors: Vectors, opts: Opts },
    /// Reciprocal rank fusion: `Σ 1 / (k + rank_i)` over the inputs' rankings
    /// of their top `depth` rows.
    Rrf { inputs: Vec<Score>, k: f64, depth: usize },
    /// A weighted sum of min-max normalised scores (`0.3 *
    /// minmax(bm25(..)) + 0.7 * minmax(similarity(..))`): a missing value
    /// counts as the worst of its term.
    Weighted(Vec<(f64, Score)>),
    /// A numeric column's value.
    Column { field: usize },
    /// Personalized PageRank of `Select::graphs[graph]`.
    Ppr { graph: usize },
    /// Hop distance in `Select::graphs[graph]`, nearest first (as
    /// `-hops`) or farthest first.
    Hops { graph: usize, far_first: bool },
    /// A numeric column's closeness to an origin, from 1 at the origin down
    /// towards 0 ([`Decay`]); 0 for rows without a value.
    Decay { field: usize, decay: Decay, opts: Opts },
    /// The product of scores (`bm25(..) * decay(..)`): candidates are the
    /// top `depth` rows of each relevance factor (BM25, similarity, graph;
    /// of every factor when there is none), scored exactly by every factor.
    /// A factor below 0, or missing, counts as 0.
    Product(Vec<Score>),
    /// The sequence number of the write that last changed the row
    /// (`seq_no()`), in a ranking expression.
    SeqNo,
    /// 1 for a row the predicate holds for, as `WHERE` would keep it, and
    /// 0 for others: the test of a [`Score::Case`] branch.
    Cond(Box<Pred>),
    // A ranking expression (`bm25(body, :q) * (1 + 0.2 * importance)`)
    // is these over the scores above ([`crate::query::rank`]). NULL in
    // arithmetic makes the expression NULL, and the row ranks last.
    /// A number: a literal or a parameter.
    Num(f64),
    Arith { op: ArithOp, left: Box<Score>, right: Box<Score> },
    Neg(Box<Score>),
    /// A numeric function ([`Func::numeric`]).
    Call { func: Func, args: Vec<Score> },
    /// A set function of a ranking expression over the rows it ranks
    /// (`minmax(x)`, `rank(x)`).
    Norm { kind: NormKind, input: Box<Score> },
    /// A ranking model's score of a row from its features
    /// ([`crate::query::model`]): the model named `model`, of the file
    /// whose hash is `hash`, once opened ([`super::ir::Expr::Gbdt`]).
    Gbdt { model: String, hash: String, args: Vec<Score>, open: Opened },
    /// `CASE WHEN c THEN x ... ELSE y END`, each `c` a [`Score::Cond`]; NULL
    /// without an `ELSE`.
    Case { whens: Vec<(Score, Score)>, otherwise: Option<Box<Score>> },
}

impl Score {
    /// Calls `f` on the expression and each within it.
    pub fn visit(&self, f: &mut dyn FnMut(&Score)) {
        f(self);
        match self {
            Score::Rrf { inputs: xs, .. } | Score::Product(xs) | Score::Call { args: xs, .. } | Score::Gbdt { args: xs, .. } => xs.iter().for_each(|x| x.visit(f)),
            Score::Weighted(ts) => ts.iter().for_each(|(_, x)| x.visit(f)),
            Score::Arith { left, right, .. } => {
                left.visit(f);
                right.visit(f);
            }
            Score::Neg(x) | Score::Norm { input: x, .. } => x.visit(f),
            Score::Case { whens, otherwise } => {
                for (c, x) in whens {
                    c.visit(f);
                    x.visit(f);
                }
                otherwise.iter().for_each(|x| x.visit(f));
            }
            _ => {}
        }
    }

    /// Every leaf (BM25, similarity, a column...) of the expression.
    pub fn leaves(&self) -> Vec<&Score> {
        match self {
            Score::Rrf { inputs, .. } | Score::Product(inputs) | Score::Call { args: inputs, .. } | Score::Gbdt { args: inputs, .. } => inputs.iter().flat_map(|s| s.leaves()).collect(),
            Score::Weighted(ts) => ts.iter().flat_map(|(_, s)| s.leaves()).collect(),
            Score::Arith { left, right, .. } => [left.leaves(), right.leaves()].concat(),
            Score::Neg(x) | Score::Norm { input: x, .. } => x.leaves(),
            Score::Case { whens, otherwise } => whens.iter().flat_map(|(c, x)| [c.leaves(), x.leaves()].concat()).chain(otherwise.iter().flat_map(|x| x.leaves())).collect(),
            Score::Num(_) => Vec::new(),
            s => vec![s],
        }
    }

    /// Whether the score is a leaf: a score of one row by itself, which
    /// the executor reads directly.
    pub fn is_leaf(&self) -> bool {
        matches!(
            self,
            Score::Bm25 { .. } | Score::Similarity { .. } | Score::Sparse { .. } | Score::MaxSim { .. } | Score::Column { .. } | Score::Ppr { .. } | Score::Hops { .. } | Score::Decay { .. } | Score::SeqNo | Score::Cond(_)
        )
    }

    pub fn opts(&self) -> Option<&Opts> {
        match self {
            Score::Bm25 { opts, .. } | Score::Similarity { opts, .. } | Score::Sparse { opts, .. } | Score::MaxSim { opts, .. } | Score::Decay { opts, .. } => Some(opts),
            _ => None,
        }
    }

    /// Whether the score ranks by relevance to the query (BM25,
    /// similarity, sparse, graph), as opposed to a row's own values (a column, a
    /// decay), which only boost rows found by relevance.
    pub fn is_relevance(&self) -> bool {
        matches!(self, Score::Bm25 { .. } | Score::Similarity { .. } | Score::Sparse { .. } | Score::MaxSim { .. } | Score::Ppr { .. } | Score::Hops { .. })
    }

    /// Whether the score can be a term of the fused shapes
    /// ([`Score::Weighted`], [`Score::Product`]) that came before ranking
    /// expressions: `hops()`, the number of hops in an expression, ranks
    /// nearest first in them.
    pub fn is_fusable(&self) -> bool {
        self.is_leaf() && !matches!(self, Score::Hops { .. } | Score::SeqNo | Score::Cond(_))
    }

    /// The expression with its sums of `w * minmax(score)` as
    /// [`Score::Weighted`] and its products of scores as [`Score::Product`],
    /// whatever wrote it (SQL or a plan): they keep the rules from before
    /// ranking expressions, and plans write them as before, which older
    /// shards read.
    pub fn fused_shapes(self) -> Score {
        // The terms of a sum of `[±][c *] minmax(score) [* c]`, each with
        // its weight.
        fn terms(e: &Score, w: f64, out: &mut Vec<(f64, Score)>) -> bool {
            match e {
                Score::Arith { op: ArithOp::Add, left, right } => terms(left, w, out) && terms(right, w, out),
                Score::Arith { op: ArithOp::Sub, left, right } => terms(left, w, out) && terms(right, -w, out),
                Score::Neg(x) => terms(x, -w, out),
                Score::Arith { op: ArithOp::Mul, left, right } => match (&**left, &**right) {
                    (Score::Num(c), x) | (x, Score::Num(c)) => terms(x, w * c, out),
                    _ => false,
                },
                Score::Norm { kind: NormKind::MinMax, input } if input.is_fusable() => {
                    out.push((w, (**input).clone()));
                    true
                }
                _ => false,
            }
        }
        // The factors of a chain of `*`.
        fn factors(e: Score, out: &mut Vec<Score>) {
            match e {
                Score::Arith { op: ArithOp::Mul, left, right } => {
                    factors(*left, out);
                    factors(*right, out);
                }
                e => out.push(e),
            }
        }
        let shapes = |x: Box<Score>| Box::new(x.fused_shapes());
        match self {
            Score::Arith { op: ArithOp::Add | ArithOp::Sub, .. } => {
                let mut t = Vec::new();
                if terms(&self, 1.0, &mut t) && t.len() > 1 {
                    return Score::Weighted(t);
                }
                let Score::Arith { op, left, right } = self else { unreachable!() };
                Score::Arith { op, left: shapes(left), right: shapes(right) }
            }
            Score::Arith { op: ArithOp::Mul, .. } => {
                let mut fs = Vec::new();
                factors(self.clone(), &mut fs);
                let (scores, rest): (Vec<Score>, Vec<Score>) = fs.into_iter().partition(Score::is_fusable);
                if scores.len() > 1 && rest.iter().all(|f| matches!(f, Score::Num(_))) {
                    // Constant factors scale the product of scores.
                    let c: f64 = rest.iter().map(|f| if let Score::Num(c) = f { *c } else { 1.0 }).product();
                    let product = Score::Product(scores);
                    return match rest.is_empty() {
                        true => product,
                        false => Score::Arith { op: ArithOp::Mul, left: Box::new(Score::Num(c)), right: Box::new(product) },
                    };
                }
                let Score::Arith { op, left, right } = self else { unreachable!() };
                Score::Arith { op, left: shapes(left), right: shapes(right) }
            }
            Score::Arith { op, left, right } => Score::Arith { op, left: shapes(left), right: shapes(right) },
            Score::Neg(x) => Score::Neg(shapes(x)),
            Score::Call { func, args } => Score::Call { func, args: args.into_iter().map(Score::fused_shapes).collect() },
            Score::Norm { kind, input } => Score::Norm { kind, input: shapes(input) },
            Score::Gbdt { model, hash, args, open } => Score::Gbdt { model, hash, args: args.into_iter().map(Score::fused_shapes).collect(), open },
            Score::Case { whens, otherwise } => Score::Case { whens: whens.into_iter().map(|(c, x)| (c, x.fused_shapes())).collect(), otherwise: otherwise.map(shapes) },
            e => e,
        }
    }
}

/// The query vectors of [`Score::MaxSim`].
#[derive(Clone, Debug, PartialEq)]
pub enum Vectors {
    List(Vec<Arc<[f32]>>),
    /// The vectors of a compound subquery's plan ([`crate::query::ir::Expr::Values`]),
    /// which runs first: like [`Seeds::Plan`], never the executor's.
    Plan(Box<crate::query::ir::Expr>),
}

/// The query vectors `maxsim` takes by default, at most.
pub const MAX_QUERY_VECTORS: usize = 64;

/// How a decay function falls off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecayShape {
    /// `exp(λ·d)`: halves (by default) every `scale`.
    Exp,
    /// A Gaussian of the distance: flat near the origin, then steep.
    Gauss,
    /// Straight down to 0 at `scale / (1 − decay)`.
    Linear,
}

/// `decay(field, origin, scale, offset => 0, decay => 0.5, shape => 'exp')`,
/// as Elasticsearch's decay functions: the distance `d = max(0, |x −
/// origin| − offset)` maps to 1 at `d = 0` and to `decay` at `d = scale`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Decay {
    pub origin: f64,
    pub scale: f64,
    pub offset: f64,
    pub decay: f64,
    pub shape: DecayShape,
}

impl Decay {
    pub fn eval(&self, x: f64) -> f64 {
        let d = ((x - self.origin).abs() - self.offset).max(0.0);
        match self.shape {
            DecayShape::Exp => (self.decay.ln() / self.scale * d).exp(),
            DecayShape::Gauss => {
                let var = -self.scale * self.scale / (2.0 * self.decay.ln());
                (-d * d / (2.0 * var)).exp()
            }
            DecayShape::Linear => {
                let s = self.scale / (1.0 - self.decay);
                ((s - d) / s).max(0.0)
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OrderBy {
    /// Rank by a score, best first (`desc = true`) or worst first. Rows
    /// without a value of a column come last unless `nulls_first`
    /// (`NULLS FIRST`).
    Score { score: Score, desc: bool, nulls_first: bool },
    /// Facet queries: order groups by their count.
    Count { desc: bool },
    /// Facet queries: order groups by their value (the first group field's).
    GroupValue { desc: bool },
    /// Aggregate queries: order groups by an aggregate or a group field.
    Agg { what: Projection, desc: bool },
}

/// A rerank phase ([`Select::phases`]): the top `count` rows of the phase
/// before it (the first phase is the query's `ORDER BY`), ranked again by
/// `score`, best first. Rows tied on it keep the order they had, and only
/// those `count` rows go on.
#[derive(Clone, Debug, PartialEq)]
pub struct Phase {
    pub score: Score,
    pub count: usize,
    /// Scored once over every row the phase reranks, on the coordinator
    /// over shards: a set function (`minmax`, `rank`, `rrf`) reads them
    /// all. A phase that is not global is row-local, and each shard scores
    /// its own rows.
    pub global: bool,
}

/// An aggregate function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggFn {
    /// `COUNT(f)`: rows with a value.
    Count,
    Sum,
    Avg,
    Min,
    Max,
    CountDistinct,
}

/// A `HAVING` condition on a group's aggregates or group values.
#[derive(Clone, Debug, PartialEq)]
pub enum Having {
    /// An inclusive numeric range.
    Range { what: Projection, lo: f64, hi: f64 },
    Eq { what: Projection, value: Value },
    And(Vec<Having>),
    Or(Vec<Having>),
    Not(Box<Having>),
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
    /// `seq_no()`: the sequence number of the write that last changed the
    /// row, in a database that numbers its writes
    /// ([`crate::storage::Database::seq_of`]).
    SeqNo,
    /// `SUM(f)`, `AVG(f)`, `MIN(f)`, `MAX(f)`, `COUNT(f)`, `COUNT(DISTINCT
    /// f)`: of the whole result, or per group.
    Agg { func: AggFn, field: usize },
    /// A ranking expression that scores each row by itself (`bm25(title,
    /// :q)`, `ln(1 + rating)`; no set function, RRF or fusion): the value
    /// the query's ranking gives it ([`crate::query::rank`]), NULL where the
    /// row has none. `features()` stands for those of the query's ranking.
    Feature(Score),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    /// Output column names and what they hold.
    pub projection: Vec<(String, Projection)>,
    pub filter: Pred,
    pub order: Option<OrderBy>,
    /// Fields to group by (KEYWORD, INT or FLOAT, multi-valued ones
    /// counting a row once per value), with aggregates per group.
    pub group_by: Vec<usize>,
    /// Which groups to keep.
    pub having: Option<Having>,
    /// Secondary order for rows that tie on the primary one.
    pub tiebreak: Option<Tiebreak>,
    /// Rerank phases after `order`, in turn; LIMIT and OFFSET cut the
    /// last one's rows.
    pub phases: Vec<Phase>,
    pub limit: Option<usize>,
    pub offset: usize,
    /// Graph computations that `Pred::Graph`, `Score::Ppr`, `Score::Hops`
    /// and `Projection::Hops` refer to by index.
    pub graphs: Vec<GraphQuery>,
    /// Per output column, the type of a BOOL or TIMESTAMP field whose
    /// values it holds (the field, or its MIN or MAX), which the answer
    /// gives as such rather than as the stored integers
    /// ([`Select::typed`]).
    pub types: Vec<Option<crate::schema::Logical>>,
}

impl Select {
    /// A ranking model the query ranks with that is not open
    /// ([`crate::query::model::Models::open`]): the executor refuses the
    /// query rather than rank by NULL.
    pub fn unopened_model(&self) -> Option<String> {
        let order = self.order.iter().filter_map(|o| match o {
            OrderBy::Score { score, .. } => Some(score),
            _ => None,
        });
        let features = self.projection.iter().filter_map(|(_, p)| match p {
            Projection::Feature(s) => Some(s),
            _ => None,
        });
        let mut found = None;
        for s in order.chain(self.phases.iter().map(|p| &p.score)).chain(features) {
            s.visit(&mut |x| {
                if let Score::Gbdt { model, open: Opened(None), .. } = x {
                    found.get_or_insert_with(|| model.clone());
                }
            });
        }
        found
    }

    /// `rows` with the values of BOOL and TIMESTAMP columns as
    /// [`Value::Bool`](crate::value::Value::Bool) and
    /// [`Value::Timestamp`](crate::value::Value::Timestamp)
    /// ([`Select::types`]). Executors and the shard merge call it last.
    pub fn typed(&self, rows: &mut [Vec<crate::value::Value>]) {
        if self.types.iter().all(Option::is_none) {
            return;
        }
        for row in rows {
            for (v, t) in row.iter_mut().zip(&self.types) {
                if let Some(t) = t {
                    *v = t.output(std::mem::replace(v, crate::value::Value::Null));
                }
            }
        }
    }
}

/// A secondary ORDER BY key (`ORDER BY rating DESC, id`; `ORDER BY 2 DESC, 1`).
#[derive(Clone, Debug, PartialEq)]
pub enum Tiebreak {
    /// The primary key field.
    Key { desc: bool },
    /// The group value of a facet query.
    GroupValue { desc: bool },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negations_reach_the_leaves() {
        let r = |f| Pred::IntRange { field: f, lo: 1, hi: 2 };
        let not = |p| Pred::Not(Box::new(p));
        let present_not = |f| Pred::And(vec![Pred::Present { field: f }, not(r(f))]);
        assert_eq!(not(r(0)).three_valued(&|_| false), present_not(0));
        assert_eq!(not(not(r(0))).three_valued(&|_| false), r(0));
        assert_eq!(not(Pred::Or(vec![r(0), r(1)])).three_valued(&|_| false), Pred::And(vec![present_not(0), present_not(1)]));
        assert_eq!(not(Pred::And(vec![r(0), not(r(1))])).three_valued(&|_| false), Pred::Or(vec![present_not(0), r(1)]));
        // IS NULL and MATCH are two-valued: a plain complement.
        assert_eq!(not(Pred::Present { field: 3 }).three_valued(&|_| false), not(Pred::Present { field: 3 }));
        assert_eq!(not(not(Pred::Present { field: 3 })).three_valued(&|_| false), Pred::Present { field: 3 });
        assert_eq!(not(Pred::True).three_valued(&|_| false), Pred::False);
        // A multi-valued field: NOT holds when no value matches, for none too.
        assert_eq!(not(r(0)).three_valued(&|f| f == 0), not(r(0)));
    }
}
