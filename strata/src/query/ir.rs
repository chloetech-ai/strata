//! The query plan: one typed, versioned and serialisable form of a
//! statement (DESIGN.md §6.2). The SQL binder makes it, and other
//! front-ends (a Python builder, a coordinator handing a query to its
//! shards) are to make it too, so that nothing writes SQL text to pass a
//! query on.
//!
//! A plan is a tree of [`Node`]s over [`Expr`]essions. Its leaf is a
//! [`Search`]: the rows of one table that a filter holds for, ranked, which
//! is what the executor answers ([`crate::query::exec`]). Above it,
//! [`Node::Aggregate`] groups rows, [`Node::Filter`] keeps groups,
//! [`Node::Sort`] orders them, [`Node::Limit`] cuts the rows and
//! [`Node::Project`] names the columns of the answer. Writes are nodes too:
//! [`Node::Insert`], and [`Node::Update`] and [`Node::Delete`] over the rows
//! a Search finds.
//!
//! * **Fields** are named by id, the field's position in its table's
//!   schema: tables have no ALTER, so a field keeps its position.
//! * **Values** are typed literals (`{"int": 5}`, `{"float": 5.0}`,
//!   `{"text": "5"}`, `{"timestamp": ...}`), so the INT 5, the FLOAT 5 and
//!   the text '5' stay apart, and a BOOL or TIMESTAMP field's values are
//!   booleans and times rather than the integers stored.
//! * **Versions.** A plan carries [`VERSION`] and the features it uses. A
//!   reader refuses a newer version, or a feature it does not know, by
//!   name ([`Plan::from_json`]), as a database refuses a format 3 feature
//!   ([`crate::storage::manifest`]); a later node or expression is a new
//!   feature, not a new version.
//!
//! [`Plan::of_select`] and [`Plan::of_write`] make the plan of a bound
//! statement; [`Plan::to_select`] and [`Plan::to_write`] give back the
//! statement the executor runs, for the shapes it runs:
//! `Project [Limit] Search` and `Project [Limit] [Sort] [Filter] Aggregate
//! Search`. Plans travel as JSON and read as a tree in `EXPLAIN`
//! ([`Plan::explain`]). Every SQL statement runs as its plan, and
//! [`crate::db::Db::run`] runs a plan made elsewhere.
//!
//! A compound query (joins, `WITH`, subqueries, `SELECT DISTINCT`) is a
//! plan of operators over those shapes, its leaves ([`crate::query::compound`]
//! makes it): [`Node::Join`], [`Node::With`] and [`Node::Ref`],
//! [`Node::Distinct`], [`Node::KeyOrder`], the nodes above over rows in
//! memory, and subqueries as expressions ([`Expr::Values`], [`Expr::Keys`],
//! [`Expr::Exists`], [`Expr::Scalar`]). [`crate::query::ops`] runs it: each
//! leaf on the executor, the rest in memory. Such a plan needs the feature
//! [`OPERATORS`].

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::query::logical::{AggFn, Decay, Direction, Expand, Opts};
use crate::value::Value;

pub mod eval;
mod explain;
pub(crate) mod lower;
mod raise;

/// The plan version this version of Strata writes and reads.
pub const VERSION: u32 = 1;

/// The plan features this version of Strata reads.
pub const FEATURES: [&str; 5] = [OPERATORS, SEQ_NO, RANK_EXPRESSIONS, SEMANTIC_ID, MAXSIM];

/// The feature of a plan with operators over rows in memory: a join,
/// `WITH`, a subquery, `DISTINCT`, or a node over another's rows than a
/// Search's ([module docs](self)).
pub const OPERATORS: &str = "operators";

/// The feature of a plan with a column of rows' sequence numbers
/// ([`Expr::SeqNo`]).
pub const SEQ_NO: &str = "seq_no";

/// The feature of a plan whose Search ranks by an expression of scores,
/// row values and numbers ([`Expr::Arith`], [`Expr::Call`], [`Expr::Case`],
/// [`Expr::Norm`] and literals around score leaves), rather than by a score
/// or one of the fused shapes ([`Expr::Rrf`], [`Expr::Weighted`],
/// [`Expr::Product`]).
pub const RANK_EXPRESSIONS: &str = "rank_expressions";

/// The feature of a plan that scores with several query vectors
/// ([`Expr::MaxSim`]), takes a dot product of vectors ([`Func::Dot`]), or
/// seeds a walk with weights ([`Graph::weights`]).
pub const MAXSIM: &str = "maxsim";
/// The feature of a plan that cuts semantic IDs ([`Func::SidAt`]).
pub const SEMANTIC_ID: &str = "semantic_id";

/// A statement as a tree of operators ([module docs](self)).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    /// Features a reader must have to run the plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    pub root: Node,
}

/// An operator: rows in (from `input`, or a table), rows out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum Node {
    /// The rows of one table (all its fields, and the score they are
    /// ranked by) that its filter holds for, best first.
    Search(Search),
    /// One row per group of the input's rows with equal `keys` (a row per
    /// value of a multi-valued key), or one row in all without keys: the
    /// keys, then the aggregates. Expressions above refer to those columns
    /// with [`Expr::Output`].
    Aggregate { input: Box<Node>, keys: Vec<Expr>, aggs: Vec<Agg> },
    /// The input's rows for which `cond` is true.
    Filter { input: Box<Node>, cond: Expr },
    /// The input's rows by `keys`, the first key first.
    Sort { input: Box<Node>, keys: Vec<SortKey> },
    /// The input's rows from `offset`, at most `limit` of them.
    Limit {
        input: Box<Node>,
        #[serde(default, skip_serializing_if = "is_zero")]
        offset: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    /// The answer's columns, by name.
    Project { input: Box<Node>, columns: Vec<Column> },
    /// Rows written to `table`, each a value per field in field order.
    Insert {
        table: String,
        rows: Vec<Vec<Expr>>,
        #[serde(default, skip_serializing_if = "OnConflict::is_fail")]
        on_conflict: OnConflict,
    },
    /// The rows of `input` (a Search of `table`, with its fields) get
    /// `sets`.
    Update { table: String, input: Box<Node>, sets: Vec<Set> },
    /// The rows of `input` (a Search of `table`) are deleted.
    Delete { table: String, input: Box<Node> },
    /// Each row of `left` beside each row of `right` whose `keys` match it
    /// and for which `on` holds: the left's columns, then the right's. A
    /// left join keeps a left row that matches none, with NULLs on the
    /// right. Rows keep the left's order.
    Join {
        left: Box<Node>,
        right: Box<Node>,
        #[serde(default, skip_serializing_if = "JoinKind::is_inner")]
        kind: JoinKind,
        keys: Vec<JoinKey>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on: Option<Expr>,
    },
    /// `input`, which reads the rows of named plans with [`Node::Ref`]:
    /// each runs once, in order, and may read those before it.
    With { ctes: Vec<Cte>, input: Box<Node> },
    /// The rows of a plan a [`Node::With`] names.
    Ref { name: String },
    /// The input's rows, each once.
    Distinct { input: Box<Node> },
    /// The input's rows (rows of one table) in the order of the rows of
    /// `by` with the same primary key.
    KeyOrder { input: Box<Node>, by: Box<Node> },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinKind {
    #[default]
    Inner,
    Left,
}

impl JoinKind {
    fn is_inner(&self) -> bool {
        *self == JoinKind::Inner
    }
}

/// A key of a [`Node::Join`]: a value of the left row equals a value of
/// the right row. Numbers equal by value whatever their type; a list
/// matches by any of its values, and an EDGES field (`links`) by the
/// targets of its links.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JoinKey {
    /// Over the left rows.
    pub left: Expr,
    /// A column of the right rows ([`Expr::Output`]).
    pub right: Expr,
    #[serde(default, skip_serializing_if = "is_false")]
    pub left_links: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub right_links: bool,
}

/// A named plan of a [`Node::With`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cte {
    pub name: String,
    pub input: Node,
}

/// The rows of one table that `filter` holds for, ordered by `order`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Search {
    pub table: String,
    #[serde(default = "Expr::always", skip_serializing_if = "Expr::is_always")]
    pub filter: Expr,
    /// A score to rank by (or a column), then possibly the primary key for
    /// ties.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<SortKey>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SortKey {
    pub expr: Expr,
    #[serde(default, skip_serializing_if = "is_false")]
    pub desc: bool,
    /// Rows without a value first (`NULLS FIRST`); they come last
    /// otherwise, either way.
    #[serde(default, skip_serializing_if = "is_false")]
    pub nulls_first: bool,
}

/// A column of the answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub expr: Expr,
}

/// An aggregate of a group's rows: `COUNT(*)` without an argument.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Agg {
    pub func: AggFn,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg: Option<Expr>,
}

/// `SET field = value`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Set {
    pub field: u32,
    pub value: Expr,
}

/// What an insert does with a row whose primary key is live.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// The statement fails.
    #[default]
    Fail,
    /// The new row replaces the live one.
    Replace,
    /// The live row stays and the new one is skipped.
    Skip,
    /// The live row gets these fields set; [`Expr::Excluded`] names the
    /// fields of the row the insert proposed.
    Update(Vec<Set>),
}

impl OnConflict {
    fn is_fail(&self) -> bool {
        *self == OnConflict::Fail
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    /// Always a FLOAT; NULL for a zero divisor.
    Div,
    /// The remainder; NULL for a zero divisor.
    Mod,
}

/// A scalar function of values. The functions from [`Func::Ln`] on are
/// numeric, came with [`RANK_EXPRESSIONS`], and are NULL where the result
/// is not a finite number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Func {
    Lower,
    Upper,
    /// Characters of a text.
    Length,
    Abs,
    /// To the nearest integer, or to `args[1]` decimals.
    Round,
    /// The first argument that is not NULL (nor a list without values).
    Coalesce,
    /// The natural logarithm.
    Ln,
    /// The base-10 logarithm of `args[0]`, or with two arguments the
    /// logarithm of `args[1]` to base `args[0]` (as PostgreSQL's `log`).
    Log,
    Exp,
    Sqrt,
    /// `args[0]` to the power `args[1]`.
    Pow,
    /// The smallest argument that is not NULL; NULL if all are.
    Least,
    /// The largest argument that is not NULL; NULL if all are.
    Greatest,
    /// `1 / (1 + e^-x)`.
    Sigmoid,
    /// `args[0]` within `args[1]` and `args[2]`.
    Clamp,
    /// The dot product of two vectors of one length (feature [`MAXSIM`]);
    /// not a ranking expression's function: `similarity` is.
    Dot,
    /// The prefix of `args[1]` levels of a semantic ID `args[0]`
    /// ([`crate::sid::sid_at`]); needs [`SEMANTIC_ID`].
    SidAt,
}

impl Func {
    /// The function's name in SQL.
    pub fn name(self) -> &'static str {
        match self {
            Func::Lower => "lower",
            Func::Upper => "upper",
            Func::Length => "length",
            Func::Abs => "abs",
            Func::Round => "round",
            Func::Coalesce => "coalesce",
            Func::Ln => "ln",
            Func::Log => "log",
            Func::Exp => "exp",
            Func::Sqrt => "sqrt",
            Func::Pow => "pow",
            Func::Least => "least",
            Func::Greatest => "greatest",
            Func::Sigmoid => "sigmoid",
            Func::Clamp => "clamp",
            Func::Dot => "dot",
            Func::SidAt => "sid_at",
        }
    }

    /// The numeric function named `name` (as SQL writes it), which a
    /// ranking expression can use.
    pub fn numeric(name: &str) -> Option<Func> {
        Some(match name {
            "abs" => Func::Abs,
            "round" => Func::Round,
            "coalesce" => Func::Coalesce,
            "ln" => Func::Ln,
            "log" => Func::Log,
            "exp" => Func::Exp,
            "sqrt" => Func::Sqrt,
            "pow" | "power" => Func::Pow,
            "least" => Func::Least,
            "greatest" => Func::Greatest,
            "sigmoid" => Func::Sigmoid,
            "clamp" => Func::Clamp,
            _ => return None,
        })
    }

    /// Whether the function takes `n` arguments, or else how many it takes.
    pub fn arity(self, n: usize) -> std::result::Result<(), &'static str> {
        let ok = match self {
            Func::Lower | Func::Upper | Func::Length | Func::Abs | Func::Ln | Func::Exp | Func::Sqrt | Func::Sigmoid => n == 1,
            Func::Round | Func::Log => n == 1 || n == 2,
            Func::Pow | Func::Dot | Func::SidAt => n == 2,
            Func::Clamp => n == 3,
            Func::Coalesce | Func::Least | Func::Greatest => n >= 1,
        };
        match (ok, self) {
            (true, _) => Ok(()),
            (_, Func::Round | Func::Log) => Err("one or two arguments"),
            (_, Func::Pow | Func::Dot) => Err("two arguments"),
            (_, Func::SidAt) => Err("two arguments: a semantic ID and a number of levels"),
            (_, Func::Clamp) => Err("three arguments: a value, its least and its greatest"),
            (_, Func::Coalesce | Func::Least | Func::Greatest) => Err("at least one argument"),
            _ => Err("one argument"),
        }
    }

    /// The numeric function of `args`, NaN standing for NULL: NaN where the
    /// result has no finite value (`ln(0)`, `sqrt(-1)`, an overflow).
    /// [`Func::Lower`], [`Func::Upper`], [`Func::Length`] and
    /// [`Func::SidAt`] take text, and are NaN here.
    pub fn eval_f64(self, args: &[f64]) -> f64 {
        // NULL in, NULL out, except where a function skips it: `pow(1,
        // NULL)` and `clamp(NULL, 0, 1)` are not 1 and 0, as f64's pow and
        // max would make them.
        if !matches!(self, Func::Coalesce | Func::Least | Func::Greatest) && args.iter().any(|a| a.is_nan()) {
            return f64::NAN;
        }
        let x = args.first().copied().unwrap_or(f64::NAN);
        let r = match self {
            Func::Lower | Func::Upper | Func::Length | Func::Dot | Func::SidAt => f64::NAN,
            Func::Abs => x.abs(),
            Func::Round => {
                let d = args.get(1).map_or(0.0, |d| d.clamp(-30.0, 30.0).trunc());
                let m = 10f64.powi(d as i32);
                (x * m).round() / m
            }
            Func::Coalesce => args.iter().copied().find(|v| !v.is_nan()).unwrap_or(f64::NAN),
            Func::Ln if x > 0.0 => x.ln(),
            Func::Log if args.len() == 1 && x > 0.0 => x.log10(),
            Func::Log => match args.get(1) {
                Some(&y) if x > 0.0 && x != 1.0 && y > 0.0 => y.ln() / x.ln(),
                _ => f64::NAN,
            },
            Func::Ln => f64::NAN,
            Func::Exp => x.exp(),
            Func::Sqrt if x >= 0.0 => x.sqrt(),
            Func::Sqrt => f64::NAN,
            Func::Pow => x.powf(args.get(1).copied().unwrap_or(f64::NAN)),
            Func::Least => args.iter().copied().filter(|v| !v.is_nan()).reduce(f64::min).unwrap_or(f64::NAN),
            Func::Greatest => args.iter().copied().filter(|v| !v.is_nan()).reduce(f64::max).unwrap_or(f64::NAN),
            Func::Sigmoid => 1.0 / (1.0 + (-x).exp()),
            Func::Clamp => match (args.get(1), args.get(2)) {
                (Some(&lo), Some(&hi)) => x.max(lo).min(hi),
                _ => f64::NAN,
            },
        };
        if r.is_finite() {
            r
        } else {
            f64::NAN
        }
    }
}

/// A set function of [`Expr::Norm`]: a value seen against every candidate
/// a ranking scores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormKind {
    /// `minmax(x)`: `(x - min) / (max - min)` over the candidates, 1 where
    /// they all have one value, and 0 where `x` is NULL.
    MinMax,
    /// `rank(x)`: 1 for the largest `x`, plus the number of candidates with
    /// a larger value; rows where `x` is NULL rank after every other.
    Rank,
}

/// A branch of [`Expr::Case`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct When {
    pub when: Expr,
    pub then: Expr,
}

/// An expression over a row. Conditions follow SQL's three-valued logic:
/// a comparison with NULL is neither true nor false, and so is its
/// negation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expr {
    /// A field of the row, by id.
    Field(u32),
    /// The score the Search ranked the row by (`score()`).
    Score,
    /// The row's hops from the seeds of the Search's first `REACHABLE` or
    /// `PATH` (`hops()`).
    Hops,
    /// The sequence number of the write that last changed the row
    /// (`seq_no()`), a column of a Search in a database that numbers its
    /// writes. Feature [`SEQ_NO`].
    SeqNo,
    /// Column `n` of the input's rows: an Aggregate's keys, then its
    /// aggregates; a Project's columns; a Join's left columns, then its
    /// right ones; a [`Node::Ref`]'s.
    Output(u32),
    /// A field of the row an insert proposed, in [`OnConflict::Update`]
    /// (`excluded.f`).
    Excluded(u32),
    Lit(#[serde(with = "lit")] Value),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    /// The expression has no value (a multi-valued field: holds none).
    IsNull(Box<Expr>),
    Cmp { op: CmpOp, left: Box<Expr>, right: Box<Expr> },
    /// `lo <= expr <= hi`; a missing bound is open.
    Between {
        expr: Box<Expr>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lo: Option<Box<Expr>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hi: Option<Box<Expr>>,
    },
    /// `expr` equals one of `list` (a multi-valued field: one of its
    /// values does).
    In { expr: Box<Expr>, list: Vec<Expr> },
    /// `left + right`, `-`, `*`, `/` or `%`: integers exact, and a FLOAT
    /// where an integer result would overflow.
    Arith { op: ArithOp, left: Box<Expr>, right: Box<Expr> },
    /// `-expr`.
    Neg(Box<Expr>),
    /// The values as text, one after another (`||`); NULL adds nothing.
    Concat(Vec<Expr>),
    /// `expr LIKE pattern` (`%` any run of characters, `_` one), or
    /// `ILIKE` ignoring case; NULL unless both are text.
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        #[serde(default, skip_serializing_if = "is_false")]
        case_insensitive: bool,
    },
    /// `CASE [operand] WHEN .. THEN .. [ELSE ..] END`: the first branch
    /// whose `when` is TRUE (or equals the operand), else `otherwise`, else
    /// NULL.
    Case {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operand: Option<Box<Expr>>,
        whens: Vec<When>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        otherwise: Option<Box<Expr>>,
    },
    /// A scalar function.
    Call { func: Func, args: Vec<Expr> },
    /// `MATCH(field, query)`: a TEXT field's full-text query.
    Match {
        field: u32,
        query: String,
        #[serde(default, skip_serializing_if = "Expand::is_none")]
        expand: Expand,
    },
    /// The rows a traversal reaches: `REACHABLE` or `PATH`.
    Graph(Box<Graph>),
    /// BM25 of a TEXT field for `text`.
    Bm25 {
        field: u32,
        text: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty", with = "lits")]
        opts: Opts,
    },
    /// Similarity of a VECTOR field with `vector`.
    Similarity {
        field: u32,
        vector: Arc<[f32]>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty", with = "lits")]
        opts: Opts,
    },
    /// The best similarity of a VECTOR field with any of `vectors`: a list
    /// literal of vectors, or [`Expr::Values`] of a subquery, which runs
    /// first (feature [`MAXSIM`]).
    #[serde(rename = "maxsim")]
    MaxSim {
        field: u32,
        vectors: Box<Expr>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty", with = "lits")]
        opts: Opts,
    },
    /// Similarity of a SPARSE field with `terms`.
    Sparse {
        field: u32,
        terms: Vec<(String, f32)>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty", with = "lits")]
        opts: Opts,
    },
    /// A numeric field's closeness to an origin.
    Decay {
        field: u32,
        decay: Decay,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty", with = "lits")]
        opts: Opts,
    },
    /// Reciprocal rank fusion of the rankings of `inputs`.
    Rrf { inputs: Vec<Expr>, k: f64, depth: u64 },
    /// `Σ weight · expr` of scores.
    Weighted(Vec<(f64, Expr)>),
    /// The product of scores.
    Product(Vec<Expr>),
    /// A set function of a ranking expression, over the rows it ranks
    /// (feature [`RANK_EXPRESSIONS`]).
    Norm { kind: NormKind, input: Box<Expr> },
    /// Personalized PageRank from a traversal's seeds.
    Ppr(Box<Graph>),
    /// The rows of a plan (graph seeds from a query of the table, which
    /// the executor runs).
    Subquery(Box<Node>),
    /// The values of the one column of a plan's rows, each value of a list
    /// and NULL none, in an [`Expr::In`] list or as graph seeds.
    Values(Box<Node>),
    /// The primary keys of a plan's rows (rows of one table), in an
    /// [`Expr::In`] list or as graph seeds.
    Keys(Box<Node>),
    /// Whether a plan has a row.
    Exists(Box<Node>),
    /// The value of a plan's one row and one column; NULL without a row.
    Scalar(Box<Node>),
}

/// A traversal over an EDGES field, or over a VECTOR field whose `k` most
/// similar rows count as a row's neighbours.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub field: u32,
    /// Primary keys (a list literal), or a [`Expr::Subquery`].
    pub seeds: Expr,
    /// The weight of each of the keys `seeds` lists, which PageRank
    /// restarts at in proportion (`weighted => 1`); none, all 1 (feature
    /// [`MAXSIM`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weights: Vec<f64>,
    pub kind: GraphKind,
    pub dir: Direction,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Traversals pass only through rows this holds for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub through: Option<Expr>,
    pub max_nodes: u64,
    pub k: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_sim: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphKind {
    Reachable { min_hops: u32, max_hops: u32 },
    Path { to: Expr, max_hops: u32 },
    Ppr { alpha: f64, eps: f64, max_pushes: u64, weighted: bool },
}

impl Expr {
    fn always() -> Expr {
        Expr::Lit(Value::Bool(true))
    }

    fn is_always(&self) -> bool {
        *self == Expr::always()
    }
}

impl Node {
    fn is_write(&self) -> bool {
        matches!(self, Node::Insert { .. } | Node::Update { .. } | Node::Delete { .. })
    }

    /// Whether the node has the shape the executor runs: `Project [Limit]
    /// [Sort] [Filter] [Aggregate] Search` ([`Plan::to_select`] says
    /// whether it is one it takes). Its subqueries ([`Expr::Values`] and
    /// the like) run first.
    pub fn is_leaf(&self) -> bool {
        let Node::Project { input, .. } = self else { return false };
        let mut n: &Node = input;
        loop {
            match n {
                Node::Limit { input, .. } | Node::Sort { input, .. } | Node::Filter { input, .. } | Node::Aggregate { input, .. } => n = input,
                Node::Search(_) => return true,
                _ => return false,
            }
        }
    }

    /// Whether running the node takes operators: it is not a leaf
    /// ([`Node::is_leaf`]), or holds a subquery that runs first.
    pub fn uses_operators(&self) -> bool {
        if !self.is_leaf() {
            return true;
        }
        let mut found = false;
        self.leaf_exprs(&mut |e| found |= e.has_operators());
        found
    }

    /// Whether the plan needs [`MAXSIM`]: an [`Expr::MaxSim`], a
    /// [`Func::Dot`] or weighted seeds anywhere in it, subqueries too.
    pub fn uses_maxsim(&self) -> bool {
        let mut found = false;
        self.exprs(&mut |e| {
            found |= match e {
                Expr::MaxSim { .. } | Expr::Call { func: Func::Dot, .. } => true,
                Expr::Graph(g) | Expr::Ppr(g) => !g.weights.is_empty(),
                _ => false,
            }
        });
        found
    }

    /// Whether a Search in the plan ranks by an expression that needs
    /// [`RANK_EXPRESSIONS`].
    pub fn ranks_by_expression(&self) -> bool {
        fn within(e: &Expr) -> bool {
            let mut found = false;
            e.visit(&mut |x| {
                if let Expr::Subquery(n) | Expr::Values(n) | Expr::Keys(n) | Expr::Exists(n) | Expr::Scalar(n) = x {
                    found |= n.ranks_by_expression();
                }
            });
            found
        }
        match self {
            Node::Search(s) => s.order.first().is_some_and(|k| k.expr.is_rank_expression()) || within(&s.filter),
            Node::Filter { input, cond } => input.ranks_by_expression() || within(cond),
            Node::Aggregate { input, .. }
            | Node::Sort { input, .. }
            | Node::Limit { input, .. }
            | Node::Project { input, .. }
            | Node::Update { input, .. }
            | Node::Delete { input, .. }
            | Node::Distinct { input } => input.ranks_by_expression(),
            Node::Join { left: a, right: b, .. } | Node::KeyOrder { input: a, by: b } => a.ranks_by_expression() || b.ranks_by_expression(),
            Node::With { ctes, input } => input.ranks_by_expression() || ctes.iter().any(|c| c.input.ranks_by_expression()),
            Node::Insert { .. } | Node::Ref { .. } => false,
        }
    }

    /// Calls `f` on every expression of the plan and each within it, in
    /// the plans of subqueries too.
    pub(crate) fn exprs(&self, f: &mut dyn FnMut(&Expr)) {
        let mut each = |e: &Expr| {
            e.visit(&mut |x| {
                f(x);
                if let Expr::Subquery(n) | Expr::Values(n) | Expr::Keys(n) | Expr::Exists(n) | Expr::Scalar(n) = x {
                    n.exprs(f);
                }
            })
        };
        match self {
            Node::Search(s) => {
                each(&s.filter);
                s.order.iter().for_each(|k| each(&k.expr));
            }
            Node::Aggregate { input, keys, aggs } => {
                keys.iter().chain(aggs.iter().filter_map(|a| a.arg.as_ref())).for_each(&mut each);
                input.exprs(f);
            }
            Node::Filter { input, cond } => {
                each(cond);
                input.exprs(f);
            }
            Node::Sort { input, keys } => {
                keys.iter().for_each(|k| each(&k.expr));
                input.exprs(f);
            }
            Node::Project { input, columns } => {
                columns.iter().for_each(|c| each(&c.expr));
                input.exprs(f);
            }
            Node::Insert { rows, .. } => rows.iter().flatten().for_each(each),
            Node::Update { input, sets, .. } => {
                sets.iter().for_each(|s| each(&s.value));
                input.exprs(f);
            }
            Node::Join { left, right, keys, on, .. } => {
                keys.iter().for_each(|k| {
                    each(&k.left);
                    each(&k.right);
                });
                on.iter().for_each(&mut each);
                left.exprs(f);
                right.exprs(f);
            }
            Node::With { ctes, input } => {
                ctes.iter().for_each(|c| c.input.exprs(f));
                input.exprs(f);
            }
            Node::KeyOrder { input, by } => {
                input.exprs(f);
                by.exprs(f);
            }
            Node::Limit { input, .. } | Node::Delete { input, .. } | Node::Distinct { input } => input.exprs(f),
            Node::Ref { .. } => {}
        }
    }

    /// Calls `f` on each expression of a leaf's nodes.
    pub(crate) fn leaf_exprs(&self, f: &mut dyn FnMut(&Expr)) {
        match self {
            Node::Project { input, columns } => {
                columns.iter().for_each(|c| f(&c.expr));
                input.leaf_exprs(f);
            }
            Node::Limit { input, .. } => input.leaf_exprs(f),
            Node::Sort { input, keys } => {
                keys.iter().for_each(|k| f(&k.expr));
                input.leaf_exprs(f);
            }
            Node::Filter { input, cond } => {
                f(cond);
                input.leaf_exprs(f);
            }
            Node::Aggregate { input, keys, aggs } => {
                keys.iter().chain(aggs.iter().filter_map(|a| a.arg.as_ref())).for_each(&mut *f);
                input.leaf_exprs(f);
            }
            Node::Search(s) => {
                f(&s.filter);
                s.order.iter().for_each(|k| f(&k.expr));
            }
            _ => {}
        }
    }

    /// [`Node::leaf_exprs`], changing them.
    pub(crate) fn leaf_exprs_mut(&mut self, f: &mut dyn FnMut(&mut Expr) -> Result<()>) -> Result<()> {
        match self {
            Node::Project { input, columns } => {
                for c in columns {
                    f(&mut c.expr)?;
                }
                input.leaf_exprs_mut(f)
            }
            Node::Limit { input, .. } => input.leaf_exprs_mut(f),
            Node::Sort { input, keys } => {
                for k in keys {
                    f(&mut k.expr)?;
                }
                input.leaf_exprs_mut(f)
            }
            Node::Filter { input, cond } => {
                f(cond)?;
                input.leaf_exprs_mut(f)
            }
            Node::Aggregate { input, keys, aggs } => {
                for k in keys.iter_mut().chain(aggs.iter_mut().filter_map(|a| a.arg.as_mut())) {
                    f(k)?;
                }
                input.leaf_exprs_mut(f)
            }
            Node::Search(s) => {
                f(&mut s.filter)?;
                for k in &mut s.order {
                    f(&mut k.expr)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The Search of a leaf.
    /// A leaf's Search.
    pub(crate) fn search(&self) -> Option<&Search> {
        match self {
            Node::Search(s) => Some(s),
            Node::Project { input, .. } | Node::Limit { input, .. } | Node::Sort { input, .. } | Node::Filter { input, .. } | Node::Aggregate { input, .. } => input.search(),
            _ => None,
        }
    }

    pub(crate) fn search_mut(&mut self) -> Option<&mut Search> {
        match self {
            Node::Search(s) => Some(s),
            Node::Project { input, .. } | Node::Limit { input, .. } | Node::Sort { input, .. } | Node::Filter { input, .. } | Node::Aggregate { input, .. } => input.search_mut(),
            _ => None,
        }
    }
}

impl Expr {
    /// Whether the expression holds a subquery that operators run
    /// ([`Expr::Values`], [`Expr::Keys`], [`Expr::Exists`],
    /// [`Expr::Scalar`]).
    /// Whether the expression, a Search's ranking, needs
    /// [`RANK_EXPRESSIONS`]: anything but a score, a field, `hops()` and
    /// the fused shapes of them that came before.
    pub fn is_rank_expression(&self) -> bool {
        match self {
            Expr::Field(_) | Expr::Hops | Expr::Bm25 { .. } | Expr::Similarity { .. } | Expr::Sparse { .. } | Expr::MaxSim { .. } | Expr::Decay { .. } | Expr::Ppr(_) => false,
            Expr::Rrf { inputs, .. } | Expr::Product(inputs) => inputs.iter().any(Expr::is_rank_expression),
            Expr::Weighted(terms) => terms.iter().any(|(_, x)| x.is_rank_expression()),
            _ => true,
        }
    }

    /// Whether the expression adds raw relevance scores (`bm25(..) +
    /// similarity(..)`), which meant min-max fusion before ranking
    /// expressions: EXPLAIN says so, for one release.
    pub fn adds_raw_scores(&self) -> bool {
        // The scores and fields an expression reads raw: not through a set
        // function or a fused shape.
        fn raw<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
            match e {
                Expr::Arith { left, right, .. } => {
                    raw(left, out);
                    raw(right, out);
                }
                Expr::Neg(x) => raw(x, out),
                Expr::Call { args, .. } => args.iter().for_each(|a| raw(a, out)),
                Expr::Case { whens, otherwise, .. } => whens.iter().map(|w| &w.then).chain(otherwise.as_deref()).for_each(|x| raw(x, out)),
                Expr::Field(_) | Expr::Hops | Expr::SeqNo | Expr::Bm25 { .. } | Expr::Similarity { .. } | Expr::Sparse { .. } | Expr::Decay { .. } | Expr::Ppr(_) => out.push(e),
                _ => {}
            }
        }
        let relevance = |e: &&Expr| matches!(e, Expr::Bm25 { .. } | Expr::Similarity { .. } | Expr::Sparse { .. } | Expr::Ppr(_) | Expr::Hops);
        let mut found = false;
        self.visit(&mut |e| {
            if let Expr::Arith { op: ArithOp::Add | ArithOp::Sub, left, right } = e {
                let (mut l, mut r) = (Vec::new(), Vec::new());
                raw(left, &mut l);
                raw(right, &mut r);
                found |= !l.is_empty() && !r.is_empty() && l != r && (l.iter().any(relevance) || r.iter().any(relevance));
            }
        });
        found
    }

    pub fn has_operators(&self) -> bool {
        let mut found = false;
        self.visit(&mut |e| found |= matches!(e, Expr::Values(_) | Expr::Keys(_) | Expr::Exists(_) | Expr::Scalar(_)));
        found
    }

    /// Calls `f` on this expression and each within it, not inside the
    /// plans of subqueries.
    pub fn visit(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        let graph = |g: &Graph, f: &mut dyn FnMut(&Expr)| {
            g.seeds.visit(f);
            if let GraphKind::Path { to, .. } = &g.kind {
                to.visit(f);
            }
            if let Some(t) = &g.through {
                t.visit(f);
            }
        };
        match self {
            Expr::Not(x) | Expr::IsNull(x) | Expr::Neg(x) | Expr::Norm { input: x, .. } => x.visit(f),
            Expr::And(xs) | Expr::Or(xs) | Expr::Concat(xs) | Expr::Product(xs) => xs.iter().for_each(|x| x.visit(f)),
            Expr::Cmp { left, right, .. } | Expr::Arith { left, right, .. } => {
                left.visit(f);
                right.visit(f);
            }
            Expr::Between { expr, lo, hi } => {
                expr.visit(f);
                lo.iter().chain(hi.iter()).for_each(|x| x.visit(f));
            }
            Expr::In { expr, list } => {
                expr.visit(f);
                list.iter().for_each(|x| x.visit(f));
            }
            Expr::Like { expr, pattern, .. } => {
                expr.visit(f);
                pattern.visit(f);
            }
            Expr::Case { operand, whens, otherwise } => {
                operand.iter().for_each(|x| x.visit(f));
                for w in whens {
                    w.when.visit(f);
                    w.then.visit(f);
                }
                otherwise.iter().for_each(|x| x.visit(f));
            }
            Expr::Call { args, .. } => args.iter().for_each(|x| x.visit(f)),
            Expr::MaxSim { vectors, .. } => vectors.visit(f),
            Expr::Rrf { inputs, .. } => inputs.iter().for_each(|x| x.visit(f)),
            Expr::Weighted(ts) => ts.iter().for_each(|(_, x)| x.visit(f)),
            Expr::Graph(g) | Expr::Ppr(g) => graph(g, f),
            _ => {}
        }
    }

    /// [`Expr::visit`], changing the expressions: `f` sees each before
    /// those within it.
    pub fn visit_mut(&mut self, f: &mut dyn FnMut(&mut Expr) -> Result<()>) -> Result<()> {
        f(self)?;
        let mut within: Vec<&mut Expr> = Vec::new();
        match self {
            Expr::Not(x) | Expr::IsNull(x) | Expr::Neg(x) | Expr::Norm { input: x, .. } | Expr::MaxSim { vectors: x, .. } => within.push(x),
            Expr::And(xs) | Expr::Or(xs) | Expr::Concat(xs) | Expr::Product(xs) | Expr::Call { args: xs, .. } | Expr::Rrf { inputs: xs, .. } => within.extend(xs.iter_mut()),
            Expr::Cmp { left, right, .. } | Expr::Arith { left, right, .. } | Expr::Like { expr: left, pattern: right, .. } => within.extend([&mut **left, &mut **right]),
            Expr::Between { expr, lo, hi } => {
                within.push(expr);
                within.extend(lo.iter_mut().chain(hi.iter_mut()).map(|x| &mut **x));
            }
            Expr::In { expr, list } => {
                within.push(expr);
                within.extend(list.iter_mut());
            }
            Expr::Case { operand, whens, otherwise } => {
                within.extend(operand.iter_mut().map(|x| &mut **x));
                for w in whens {
                    within.extend([&mut w.when, &mut w.then]);
                }
                within.extend(otherwise.iter_mut().map(|x| &mut **x));
            }
            Expr::Weighted(ts) => within.extend(ts.iter_mut().map(|(_, x)| x)),
            Expr::Graph(g) | Expr::Ppr(g) => {
                let Graph { seeds, kind, through, .. } = &mut **g;
                within.push(seeds);
                if let GraphKind::Path { to, .. } = kind {
                    within.push(to);
                }
                within.extend(through.iter_mut());
            }
            _ => {}
        }
        for x in within {
            x.visit_mut(f)?;
        }
        Ok(())
    }
}

fn is_zero(x: &u64) -> bool {
    *x == 0
}

fn is_false(x: &bool) -> bool {
    !*x
}

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

impl Plan {
    /// A plan of the current version, with the features it uses.
    pub fn new(root: Node) -> Plan {
        let mut features = Vec::new();
        if !root.is_write() && root.uses_operators() {
            features.push(OPERATORS.to_string());
        }
        let mut seq_no = false;
        if root.is_leaf() {
            root.leaf_exprs(&mut |e| e.visit(&mut |x| seq_no |= *x == Expr::SeqNo));
        }
        if seq_no {
            features.push(SEQ_NO.to_string());
        }
        if root.ranks_by_expression() {
            features.push(RANK_EXPRESSIONS.to_string());
        }
        let mut sid_at = false;
        root.exprs(&mut |e| sid_at |= matches!(e, Expr::Call { func: Func::SidAt, .. }));
        if sid_at {
            features.push(SEMANTIC_ID.to_string());
        }
        if root.uses_maxsim() {
            features.push(MAXSIM.to_string());
        }
        Plan { version: VERSION, features, root }
    }

    /// The table the plan reads or writes: its (first) Search's, or its
    /// write's.
    pub fn table(&self) -> &str {
        fn of(n: &Node) -> Option<&str> {
            match n {
                Node::Search(s) => Some(&s.table),
                Node::Insert { table, .. } | Node::Update { table, .. } | Node::Delete { table, .. } => Some(table),
                Node::Aggregate { input, .. }
                | Node::Filter { input, .. }
                | Node::Sort { input, .. }
                | Node::Limit { input, .. }
                | Node::Project { input, .. }
                | Node::Distinct { input }
                | Node::KeyOrder { input, .. } => of(input),
                Node::Join { left, right, .. } => of(left).or_else(|| of(right)),
                Node::With { ctes, input } => of(input).or_else(|| ctes.iter().find_map(|c| of(&c.input))),
                Node::Ref { .. } => None,
            }
        }
        of(&self.root).unwrap_or("")
    }

    /// Whether the plan writes (an Insert, Update or Delete) rather than
    /// reads.
    pub fn is_write(&self) -> bool {
        self.root.is_write()
    }

    pub fn to_json(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self).map_err(|e| err(format!("the plan cannot be written as JSON: {e}")))
    }

    /// [`Plan::to_json`] as text, each object's keys in the order of the
    /// types here (a node's kind first).
    pub fn to_json_text(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| err(format!("the plan cannot be written as JSON: {e}")))
    }

    /// Reads a plan, refusing first a version newer than [`VERSION`] and a
    /// feature this version lacks, by name, and then anything else it does
    /// not know.
    pub fn from_json(j: &serde_json::Value) -> Result<Plan> {
        let version = j.get("version").and_then(|v| v.as_u64()).ok_or_else(|| err("a plan needs a version"))?;
        let features = match j.get("features") {
            None => Vec::new(),
            Some(serde_json::Value::Array(fs)) => fs.iter().map(|f| f.as_str().ok_or_else(|| err("a plan's features are names"))).collect::<Result<_>>()?,
            Some(_) => return Err(err("a plan's features are a list of names")),
        };
        Plan::readable(version, &features)?;
        serde_json::from_value(j.clone()).map_err(|e| err(format!("not a plan of version {VERSION}: {e}")))
    }

    /// Whether this version of Strata reads a plan of `version` that needs
    /// `features`: an error naming what it lacks if not. A plan in another
    /// form than JSON (a shard's protobuf) is checked with it too.
    pub fn readable(version: u64, features: &[&str]) -> Result<()> {
        Plan::readable_by(version, features, &FEATURES)
    }

    /// [`Plan::readable`] by a reader of the plan features `known`: an
    /// older version of Strata knows fewer.
    pub fn readable_by(version: u64, features: &[&str], known: &[&str]) -> Result<()> {
        if version == 0 || version > VERSION as u64 {
            return Err(err(format!("plan version {version} is not one this version of Strata reads (it reads version {VERSION}): it may be newer")));
        }
        match features.iter().find(|f| !known.contains(f)) {
            Some(name) => Err(err(format!("the plan needs feature {name:?}, which this version of Strata does not have: it may be newer"))),
            None => Ok(()),
        }
    }
}

/// Typed literals in JSON: `null`, `{"bool": true}`, `{"int": 5}`,
/// `{"float": 0.5}`, `{"text": "a"}`, `{"timestamp": micros}`,
/// `{"vector": [...]}` and `{"list": [...]}`. A FLOAT must be finite.
mod lit {
    use std::sync::Arc;

    use serde::{de, ser, Deserialize, Deserializer, Serialize, Serializer};

    use crate::value::Value;

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub(super) enum Lit {
        Null,
        Bool(bool),
        Int(i64),
        Float(f64),
        Text(String),
        Timestamp(i64),
        Vector(Arc<[f32]>),
        List(Vec<Lit>),
    }

    impl Lit {
        pub(super) fn of(v: &Value) -> std::result::Result<Lit, String> {
            Ok(match v {
                Value::Null => Lit::Null,
                Value::Bool(b) => Lit::Bool(*b),
                Value::Int(x) => Lit::Int(*x),
                Value::Float(x) if x.is_finite() => Lit::Float(*x),
                Value::Float(x) => return Err(format!("a FLOAT literal must be finite, not {x}")),
                Value::Str(s) => Lit::Text(s.clone()),
                Value::Timestamp(t) => Lit::Timestamp(*t),
                Value::Vector(v) if v.iter().all(|x| x.is_finite()) => Lit::Vector(v.clone()),
                Value::Vector(_) => return Err("a vector's components must be finite".into()),
                Value::List(xs) => Lit::List(xs.iter().map(Lit::of).collect::<std::result::Result<_, _>>()?),
            })
        }

        pub(super) fn value(self) -> Value {
            match self {
                Lit::Null => Value::Null,
                Lit::Bool(b) => Value::Bool(b),
                Lit::Int(x) => Value::Int(x),
                Lit::Float(x) => Value::Float(x),
                Lit::Text(s) => Value::Str(s),
                Lit::Timestamp(t) => Value::Timestamp(t),
                Lit::Vector(v) => Value::Vector(v),
                Lit::List(xs) => Value::List(xs.into_iter().map(Lit::value).collect()),
            }
        }
    }

    pub fn serialize<S: Serializer>(v: &Value, s: S) -> std::result::Result<S::Ok, S::Error> {
        Lit::of(v).map_err(ser::Error::custom)?.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Value, D::Error> {
        let v = Lit::deserialize(d)?.value();
        Lit::of(&v).map_err(de::Error::custom)?;
        Ok(v)
    }
}

/// Named options of typed literals (`{"k": {"int": 60}}`).
mod lits {
    use std::collections::BTreeMap;

    use serde::{de, ser, Deserialize, Deserializer, Serialize, Serializer};

    use super::lit::Lit;
    use crate::query::logical::Opts;

    pub fn serialize<S: Serializer>(opts: &Opts, s: S) -> std::result::Result<S::Ok, S::Error> {
        let m: BTreeMap<&str, Lit> = opts.iter().map(|(k, v)| Ok((k.as_str(), Lit::of(v)?))).collect::<std::result::Result<_, String>>().map_err(ser::Error::custom)?;
        m.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Opts, D::Error> {
        let m = BTreeMap::<String, Lit>::deserialize(d)?;
        let opts: Opts = m.into_iter().map(|(k, v)| (k, v.value())).collect();
        for v in opts.values() {
            Lit::of(v).map_err(de::Error::custom)?;
        }
        Ok(opts)
    }
}
