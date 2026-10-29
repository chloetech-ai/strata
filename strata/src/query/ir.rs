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
//! ([`Plan::explain`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::query::logical::{AggFn, Decay, Direction, Expand, Opts};
use crate::value::Value;

mod explain;
mod lower;
mod raise;

/// The plan version this version of Strata writes and reads.
pub const VERSION: u32 = 1;

/// The plan features this version of Strata reads: none yet, as every node
/// and expression here is version 1's.
pub const FEATURES: [&str; 0] = [];

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
    /// Column `n` of the rows of the Aggregate below: its keys, then its
    /// aggregates.
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
    /// `left + right` or `left - right`; integers exact.
    Arith { op: ArithOp, left: Box<Expr>, right: Box<Expr> },
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
    /// Personalized PageRank from a traversal's seeds.
    Ppr(Box<Graph>),
    /// The rows of a plan (graph seeds from a query).
    Subquery(Box<Node>),
}

/// A traversal over an EDGES field, or over a VECTOR field whose `k` most
/// similar rows count as a row's neighbours.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub field: u32,
    /// Primary keys (a list literal), or a [`Expr::Subquery`].
    pub seeds: Expr,
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
    /// A plan of the current version, without features.
    pub fn new(root: Node) -> Plan {
        Plan { version: VERSION, features: Vec::new(), root }
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
        if version == 0 || version > VERSION as u64 {
            return Err(err(format!("plan version {version} is not one this version of Strata reads (it reads version {VERSION}): it may be newer")));
        }
        match j.get("features") {
            None => {}
            Some(serde_json::Value::Array(fs)) => {
                for f in fs {
                    let name = f.as_str().ok_or_else(|| err("a plan's features are names"))?;
                    if !FEATURES.contains(&name) {
                        return Err(err(format!("the plan needs feature {name:?}, which this version of Strata does not have: it may be newer")));
                    }
                }
            }
            Some(_) => return Err(err("a plan's features are a list of names")),
        }
        serde_json::from_value(j.clone()).map_err(|e| err(format!("not a plan of version {VERSION}: {e}")))
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
