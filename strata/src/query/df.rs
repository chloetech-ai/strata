//! DataFusion ([`datafusion`]) as the engine of a plan's operators: what
//! [`crate::query::ops`] runs in memory, row by row, otherwise (feature
//! `datafusion`, [`Engine::DataFusion`](crate::query::exec::Engine)).
//!
//! Each leaf (`Project [Limit] [Sort] [Filter] [Aggregate] Search`) still
//! runs on Strata's executor through the [`Runner`], so search, ranking
//! and every index stay Strata's; its rows become Arrow batches in a
//! memory table, and the nodes above it (joins, `WITH`, `DISTINCT`,
//! groups, sorts, limits) become one DataFusion logical plan, which
//! DataFusion optimises and runs on column batches, on
//! [`ExecOptions::threads`] partitions.
//!
//! Strata's semantics hold where DataFusion's differ:
//!
//! * **Order.** Rows keep the order Strata gives them (a leaf's ranking, a
//!   join's left side, a stable sort, a group's first row): every relation
//!   carries hidden columns that order it, and the answer is sorted by
//!   them, whatever order DataFusion's partitions produce.
//! * **Arithmetic.** Integers add, subtract and multiply exactly (a
//!   result outside 64 bits is not wrapped: the plan falls back to the
//!   operators, which give a FLOAT), division gives a FLOAT and a zero
//!   divisor NULL, and a sum of integers is exact (a decimal sum, an INT
//!   where it fits).
//! * **Types.** A column has one Arrow type; a leaf column that mixes INT
//!   and FLOAT values, or a multi-valued column used as a key, is not
//!   lowered.
//!
//! Whatever is not lowered here (a subquery in a leaf, links of an EDGES
//! field, a function without an equivalent) gives [`Outcome::Fallback`]
//! with the reason, and the caller runs the plan on the operators; so does
//! any error of DataFusion's own, so a plan answers here exactly when
//! DataFusion and the operators agree on what it means.

use std::sync::{Arc, OnceLock};

use datafusion::arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, BooleanBuilder, Float32Builder, Float64Array, Float64Builder, Int64Array, Int64Builder, ListBuilder, StringBuilder, TimestampMicrosecondArray,
    TimestampMicrosecondBuilder, UInt64Array,
};
use datafusion::arrow::compute::kernels::numeric;
use datafusion::arrow::datatypes::{DataType, Field, Int64Type, Schema as ArrowSchema, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::{Column as DfColumn, DFSchema, DataFusionError, ScalarValue};
use datafusion::datasource::{provider_as_source, MemTable};
use datafusion::execution::context::SessionContext;
use datafusion::functions_aggregate::expr_fn as agg_fn;
use datafusion::functions_window::expr_fn::row_number;
use datafusion::logical_expr::expr::Like;
use datafusion::logical_expr::{
    binary_expr, cast, in_list, lit, not, when, ColumnarValue, Expr as DExpr, ExprFunctionExt, ExprSchemable, JoinType, LogicalPlan, LogicalPlanBuilder, Operator, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, SortExpr, Volatility,
};
use datafusion::prelude::SessionConfig;

use crate::error::{Error, Result};
use crate::query::exec::{ExecOptions, ExecStats, ResultSet};
use crate::query::ir::{Agg, ArithOp, CmpOp, Expr, Func, JoinKind, Node, Plan, SortKey};
use crate::query::memory::Charge;
use crate::query::ops::{self, Runner};
use crate::query::AggFn;
use crate::schema::Schema;
use crate::value::Value;

/// What [`run_in`] made of a plan.
#[derive(Debug)]
pub enum Outcome {
    /// DataFusion answered it.
    Done(ResultSet),
    /// Not lowered (or DataFusion failed on it): why. The operators run it.
    Fallback(String),
}

/// Why a plan does not run here: not lowered, or a Strata error (a
/// leaf's, the deadline's, the memory budget's), which the query returns.
enum Fail {
    No(String),
    Err(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::Err(e)
    }
}

impl From<DataFusionError> for Fail {
    fn from(e: DataFusionError) -> Fail {
        Fail::No(format!("datafusion: {e}"))
    }
}

type R<T> = std::result::Result<T, Fail>;

fn no<T>(why: impl Into<String>) -> R<T> {
    Err(Fail::No(why.into()))
}

/// The runtime DataFusion's partitions run on, shared by every query.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        let n = std::thread::available_parallelism().map_or(4, |n| n.get());
        tokio::runtime::Builder::new_multi_thread().worker_threads(n).thread_name("strata-datafusion").enable_all().build().expect("a tokio runtime for DataFusion")
    })
}

/// Runs `plan` (one that reads, [`Node::uses_operators`]) with DataFusion
/// above its leaves, which `runner` runs; [`Outcome::Fallback`] when it
/// is not lowered here.
pub fn run_in(plan: &Plan, catalog: &[&Schema], runner: &mut Runner, opts: &ExecOptions) -> Result<Outcome> {
    let config = SessionConfig::new().with_target_partitions(opts.threads.max(1)).with_batch_size(8192);
    let mut l = Lower { catalog, runner, opts, ctx: SessionContext::new_with_config(config), next: 0, ctes: Vec::new(), driver: None, per: None, lines: Vec::new(), charge: Charge::new(opts.memory.clone()) };
    match l.answer(&plan.root) {
        Ok(rs) => Ok(Outcome::Done(rs)),
        Err(Fail::No(why)) => Ok(Outcome::Fallback(why)),
        Err(Fail::Err(e)) => Err(e),
    }
}

/// A column that orders a relation: its rows are in the order of these,
/// the first first.
#[derive(Clone, Debug)]
struct Ord {
    col: String,
    desc: bool,
    nulls_first: bool,
}

/// The score of a relation's rows: none (groups), a column, or 0 for
/// each (a join whose left rows have none).
#[derive(Clone, Debug)]
enum Score {
    None,
    Col(String),
    Zero,
}

/// A lowered node: its plan, its columns (named uniquely across the whole
/// plan, so joins never clash), the names the answer gives them, and the
/// hidden columns of the rows' primary keys, scores and order.
#[derive(Clone, Debug)]
struct Rel {
    plan: LogicalPlan,
    cols: Vec<String>,
    names: Vec<String>,
    key: Option<String>,
    score: Score,
    ord: Vec<Ord>,
}

impl Rel {
    /// The hidden columns, each once.
    fn hidden(&self) -> Vec<String> {
        let mut h: Vec<String> = self.key.iter().cloned().collect();
        if let Score::Col(s) = &self.score {
            h.push(s.clone());
        }
        h.extend(self.ord.iter().map(|o| o.col.clone()));
        let mut seen = std::collections::HashSet::new();
        h.retain(|c| seen.insert(c.clone()));
        h
    }

    /// Every column, visible and hidden, each once.
    fn all(&self) -> Vec<DExpr> {
        let mut seen = std::collections::HashSet::new();
        self.cols.iter().cloned().chain(self.hidden()).filter(|c| seen.insert(c.clone())).map(|c| col(&c)).collect()
    }

    fn sort_exprs(&self) -> Vec<SortExpr> {
        self.ord.iter().map(|o| col(&o.col).sort(!o.desc, o.nulls_first)).collect()
    }
}

fn col(name: &str) -> DExpr {
    DExpr::Column(DfColumn::new_unqualified(name))
}

struct Lower<'a, 'r> {
    catalog: &'a [&'a Schema],
    runner: &'a mut Runner<'r>,
    opts: &'a ExecOptions,
    ctx: SessionContext,
    /// For names: columns, tables.
    next: usize,
    /// The `WITH` results in scope, each held in memory, the innermost
    /// last.
    ctes: Vec<(String, Rel)>,
    /// The scan a LIMIT over joins grows: the leaf, the rows it may read,
    /// and how many it read.
    driver: Option<(*const Node, u64, usize)>,
    /// The cap a `MAX k PER` right above the driven scan puts on its rows,
    /// which the scan keeps itself ([`crate::query::ir::Search::per`]).
    per: Option<(*const Node, crate::query::ir::PerCap)>,
    lines: Vec<String>,
    charge: Charge,
}

impl<'a> Lower<'a, '_> {
    fn fresh(&mut self, what: &str) -> String {
        self.next += 1;
        format!("__{what}{}", self.next)
    }

    fn answer(&mut self, root: &Node) -> R<ResultSet> {
        let rel = self.node(root, "query")?;
        let (mut rs, _) = self.rows(rel)?;
        rs.stats.plan = std::mem::take(&mut self.lines);
        Ok(rs)
    }

    /// The rows of `rel`, in its order, and whether they are rows of a
    /// table (with keys).
    fn rows(&mut self, rel: Rel) -> R<(ResultSet, bool)> {
        let mut out: Vec<DExpr> = rel.cols.iter().map(|c| col(c)).collect();
        out.extend(rel.key.iter().map(|k| col(k)));
        if let Score::Col(s) = &rel.score {
            out.push(col(s));
        }
        let plan = LogicalPlanBuilder::from(rel.plan.clone()).sort(rel.sort_exprs())?.project(out)?.build()?;
        let batches = self.collect(plan)?;
        let n: usize = batches.iter().map(RecordBatch::num_rows).sum();
        let width = rel.cols.len();
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(n);
        let mut keys: Vec<Value> = Vec::new();
        let mut scores: Vec<f64> = Vec::new();
        for b in &batches {
            let columns: Vec<Vec<Value>> = b.columns().iter().map(|a| values_of(a)).collect::<R<_>>()?;
            for i in 0..b.num_rows() {
                rows.push(columns[..width].iter().map(|c| c[i].clone()).collect());
            }
            let mut at = width;
            if rel.key.is_some() {
                keys.extend(columns[at].iter().cloned());
                at += 1;
            }
            if matches!(rel.score, Score::Col(_)) {
                scores.extend(columns[at].iter().map(|v| v.as_f64().unwrap_or(0.0)));
            }
        }
        if matches!(rel.score, Score::Zero) {
            scores = vec![0.0; n];
        }
        if keys.iter().any(|k| matches!(k, Value::Null)) {
            keys.clear();
        }
        for r in &rows {
            self.charge.row(r)?;
        }
        self.lines.push(format!("datafusion: {n} rows"));
        let keyed = rel.key.is_some();
        Ok((ResultSet { columns: rel.names, rows, keys, scores, parts: None, stats: ExecStats::default() }, keyed))
    }

    /// [`Lower::expr`] of an expression of a node, its subqueries run.
    fn top(&mut self, e: &Expr, cols: &[String], schema: &DFSchema) -> R<DExpr> {
        let e = self.resolved(e, None)?;
        self.expr(&e, cols, schema)
    }

    /// [`Lower::cond`] of a node's condition, its subqueries run.
    fn top_cond(&mut self, e: &Expr, cols: &[String], schema: &DFSchema) -> R<DExpr> {
        let e = self.resolved(e, None)?;
        self.cond(&e, cols, schema)
    }

    /// `e` with its subqueries run and their results in their place, as
    /// the operators put them: values a field of `schema`'s table (a
    /// leaf's) is compared with, in the field's type.
    fn resolved(&mut self, e: &Expr, schema: Option<&Schema>) -> R<Expr> {
        if !e.has_operators() {
            return Ok(e.clone());
        }
        if e.has_correlated() {
            return no("a correlated subquery");
        }
        let mut e = e.clone();
        let mut fail = None;
        let _ = e.visit_mut(&mut |x| match self.resolve(x, schema) {
            Ok(()) => Ok(()),
            Err(f) => {
                fail = Some(f);
                Err(Error::Query(String::new()))
            }
        });
        match fail {
            Some(f) => Err(f),
            None => Ok(e),
        }
    }

    fn resolve(&mut self, e: &mut Expr, schema: Option<&Schema>) -> R<()> {
        let field = |x: &Expr| match x {
            Expr::Field(f) => Some(*f as usize),
            _ => None,
        };
        match e {
            // A value of the list each, and NULL once if the subquery
            // returned one: `x NOT IN (SELECT ..)` holds for no row then.
            Expr::In { expr, list } if list.iter().any(|x| matches!(x, Expr::Values(_) | Expr::Keys(_))) => {
                let f = field(expr);
                let mut out = Vec::with_capacity(list.len());
                let mut null = false;
                for x in std::mem::take(list) {
                    match x {
                        x @ (Expr::Values(_) | Expr::Keys(_)) => {
                            let (vals, nulls) = self.sub_values(&x, schema, f)?;
                            out.extend(vals.into_iter().map(Expr::Lit));
                            null |= nulls;
                        }
                        x => out.push(x),
                    }
                }
                if null {
                    out.push(Expr::Lit(Value::Null));
                }
                *list = out;
            }
            Expr::Cmp { left, right, .. } => {
                let (fl, fr) = (field(left), field(right));
                for (side, f) in [(left, fr), (right, fl)] {
                    if let Expr::Scalar(n) = side.as_ref() {
                        let n = (**n).clone();
                        **side = Expr::Lit(self.sub_scalar(&n, schema, f)?);
                    }
                }
            }
            Expr::Between { expr, lo, hi } => {
                let f = field(expr);
                for b in lo.iter_mut().chain(hi.iter_mut()) {
                    if let Expr::Scalar(n) = b.as_ref() {
                        let n = (**n).clone();
                        **b = Expr::Lit(self.sub_scalar(&n, schema, f)?);
                    }
                }
            }
            Expr::Graph(g) | Expr::Ppr(g) => {
                let pk = schema.and_then(|s| s.primary_key);
                let crate::query::ir::Graph { seeds, kind, .. } = g.as_mut();
                if matches!(kind, crate::query::ir::GraphKind::Ppr { weighted: true, .. }) && matches!(seeds, Expr::Values(_) | Expr::Keys(_)) {
                    return no("PageRank weighted by a subquery's scores");
                }
                let to = match kind {
                    crate::query::ir::GraphKind::Path { to, .. } => Some(to),
                    _ => None,
                };
                for s in std::iter::once(seeds).chain(to) {
                    if matches!(s, Expr::Values(_) | Expr::Keys(_)) {
                        let (vals, _) = self.sub_values(s, schema, pk)?;
                        *s = Expr::Lit(Value::List(vals));
                    }
                }
            }
            Expr::MaxSim { .. } | Expr::Encode { .. } => return no("maxsim or encode() of a subquery"),
            Expr::Values(_) | Expr::Keys(_) => {
                let (vals, _) = self.sub_values(e, None, None)?;
                *e = Expr::Lit(Value::List(vals));
            }
            Expr::Exists(n) => {
                let n = (**n).clone();
                let rel = self.node(&n, "subquery")?;
                let (rs, _) = self.rows(rel)?;
                *e = Expr::Lit(Value::Bool(!rs.rows.is_empty()));
            }
            Expr::Scalar(n) => {
                let n = (**n).clone();
                *e = Expr::Lit(self.sub_scalar(&n, None, None)?);
            }
            _ => {}
        }
        Ok(())
    }

    /// The values of [`Expr::Values`] (each once, a list's each, no NULL)
    /// or [`Expr::Keys`], as the values of field `f` of `schema`'s table,
    /// and whether a row's value was NULL.
    fn sub_values(&mut self, e: &Expr, schema: Option<&Schema>, f: Option<usize>) -> R<(Vec<Value>, bool)> {
        let (Expr::Values(n) | Expr::Keys(n)) = e else { unreachable!("a subquery") };
        let rel = self.node(n, "subquery")?;
        let (rs, keyed) = self.rows(rel)?;
        let mut null = false;
        let vals = match e {
            Expr::Keys(_) if !keyed => return Err(Fail::Err(Error::Query("the subquery's rows are not rows of the table, which have keys".into()))),
            Expr::Keys(_) => rs.keys,
            _ => {
                if rs.rows.first().is_some_and(|r| r.len() != 1) {
                    return Err(Fail::Err(Error::Query(format!("a subquery in IN returns one column, not {}", rs.columns.len()))));
                }
                let mut seen = std::collections::HashSet::new();
                let mut out = Vec::new();
                for r in rs.rows {
                    let vs = match r.into_iter().next() {
                        Some(Value::List(xs)) => xs,
                        Some(Value::Null) => {
                            null = true;
                            Vec::new()
                        }
                        None => Vec::new(),
                        Some(v) => vec![v],
                    };
                    out.extend(vs.into_iter().filter(|v| seen.insert(ops::part(v))));
                }
                out
            }
        };
        self.lines.push(format!("subquery: {} {}{}", vals.len(), if matches!(e, Expr::Keys(_)) { "keys" } else { "values" }, if null { " and NULL" } else { "" }));
        Ok((ops::equal_to(schema, f, vals)?, null))
    }

    /// The value of [`Expr::Scalar`]: NULL without a row.
    fn sub_scalar(&mut self, n: &Node, schema: Option<&Schema>, f: Option<usize>) -> R<Value> {
        let rel = self.node(n, "subquery")?;
        let (rs, _) = self.rows(rel)?;
        if rs.rows.len() > 1 {
            return Err(Fail::Err(Error::Query(format!("a scalar subquery returned {} rows; add LIMIT 1 or an aggregate", rs.rows.len()))));
        }
        Ok(match rs.rows.into_iter().next().and_then(|r| r.into_iter().next()) {
            None | Some(Value::Null) => Value::Null,
            Some(v) => ops::coerce(schema, f, v)?,
        })
    }

    /// Runs a plan to its batches, under the query's deadline and cancel
    /// flag (checked before and after: DataFusion's own run is not
    /// interrupted yet).
    fn collect(&mut self, plan: LogicalPlan) -> R<Vec<RecordBatch>> {
        self.opts.check()?;
        let ctx = self.ctx.clone();
        let batches = runtime().block_on(async move { ctx.execute_logical_plan(plan).await?.collect().await })?;
        self.opts.check()?;
        Ok(batches)
    }

    /// Lowers `n`; `what` names its leaves in the plan's lines, as the
    /// operators do (`query`, `scan`, `join`, `WITH t`, `subquery`).
    fn node(&mut self, n: &Node, what: &str) -> R<Rel> {
        if n.is_leaf() {
            return self.leaf(n, what, Vec::new());
        }
        match n {
            Node::Project { input, columns } => {
                let r = self.node(input, what)?;
                let mut exprs = Vec::with_capacity(columns.len() + 3);
                let mut cols = Vec::with_capacity(columns.len());
                for c in columns {
                    let e = self.top(&c.expr, &r.cols, r.plan.schema())?;
                    let name = self.fresh("c");
                    exprs.push(e.alias(&name));
                    cols.push(name);
                }
                exprs.extend(r.hidden().iter().map(|h| col(h)));
                let plan = LogicalPlanBuilder::from(r.plan).project(exprs)?.build()?;
                Ok(Rel { plan, cols, names: columns.iter().map(|c| c.name.clone()).collect(), ..r })
            }
            Node::Filter { input, cond } => {
                let r = self.node(input, what)?;
                let c = self.top_cond(cond, &r.cols, r.plan.schema())?;
                let plan = LogicalPlanBuilder::from(r.plan.clone()).filter(c)?.build()?;
                Ok(Rel { plan, ..r })
            }
            Node::Sort { input, keys } => {
                let r = self.node(input, what)?;
                self.sort(r, keys)
            }
            Node::Limit { input, offset, limit } => {
                let (skip, fetch) = (*offset as usize, limit.map(|l| l as usize));
                let r = match (fetch, ops::driver_of(input)) {
                    (Some(l), Some(scan)) => self.driven(input, scan, l.saturating_add(skip), what)?,
                    _ => self.node(input, what)?,
                };
                let plan = LogicalPlanBuilder::from(r.plan.clone()).sort_with_limit(r.sort_exprs(), fetch.map(|f| f.saturating_add(skip)))?.limit(skip, fetch)?.build()?;
                Ok(Rel { plan, ..r })
            }
            Node::Distinct { input } => {
                let r = self.node(input, what)?;
                let keys: Vec<DExpr> = r.cols.iter().map(|c| col(c)).collect();
                self.first_per(r, keys, 1)
            }
            Node::DistinctOn { input, keys, per } => {
                let r = self.node(input, what)?;
                let keys: Vec<DExpr> = keys.iter().map(|k| self.top(k, &r.cols, r.plan.schema())).collect::<R<_>>()?;
                self.first_per(r, keys, *per)
            }
            Node::Aggregate { input, keys, aggs } => {
                let r = self.node(input, what)?;
                self.aggregate(r, keys, aggs)
            }
            Node::Join { left, right, kind, keys, on } => self.join(left, right, *kind, keys, on.as_ref()),
            Node::With { ctes, input } => {
                let mark = self.ctes.len();
                let r = (|| {
                    for c in ctes {
                        let rel = self.node(&c.input, &format!("WITH {}", c.name))?;
                        let rel = self.materialize(rel)?;
                        self.ctes.push((c.name.clone(), rel));
                    }
                    self.node(input, what)
                })();
                self.ctes.truncate(mark);
                r
            }
            Node::Ref { name } => {
                let Some((_, rel)) = self.ctes.iter().rev().find(|(n, _)| n.eq_ignore_ascii_case(name)).cloned() else {
                    return Err(Fail::Err(Error::Query(format!("no WITH result named {name}"))));
                };
                self.renamed(rel)
            }
            Node::KeyOrder { input, by } => {
                let r = self.node(input, what)?;
                let b = self.node(by, what)?;
                self.key_order(r, b)
            }
            Node::Search(_) => no("a Search outside a leaf"),
            Node::Insert { .. } | Node::Update { .. } | Node::Delete { .. } => no("a write"),
        }
    }

    /// A leaf on the executor, `restrict` added to its Search's filter, its
    /// rows in memory.
    /// The leaf `n` as the executor runs it: its subqueries' values in
    /// place, their NULLs compared as SQL reads them.
    fn prepared(&mut self, n: &Node) -> R<Node> {
        let schema = self.table(n)?;
        let mut leaf = n.clone();
        let mut fail = None;
        let _ = leaf.leaf_exprs_mut(&mut |e| match self.resolved(e, Some(schema)) {
            Ok(x) => {
                *e = x;
                Ok(())
            }
            Err(f) => {
                fail = Some(f);
                Err(Error::Query(String::new()))
            }
        });
        if let Some(f) = fail {
            return Err(f);
        }
        // A subquery's NULL compares as SQL reads it, not as an error.
        ops::unknown(&mut leaf, self.catalog);
        Ok(leaf)
    }

    /// How many rows the leaf `n` reads: a count of its Search (`None`
    /// if the executor does not count it).
    fn count(&mut self, n: &Node) -> R<Option<u64>> {
        let leaf = self.prepared(n)?;
        let Some(s) = leaf.search() else { return Ok(None) };
        let mut search = s.clone();
        search.order.clear();
        search.phases.clear();
        search.per = None;
        let count = Node::Project {
            input: Box::new(Node::Aggregate { input: Box::new(Node::Search(search)), keys: Vec::new(), aggs: vec![Agg { func: AggFn::Count, arg: None }] }),
            columns: vec![crate::query::ir::Column { name: "n".into(), expr: Expr::Output(0) }],
        };
        self.opts.check()?;
        let n = match (self.runner)(&Plan::new(count)) {
            Ok(rs) => rs.rows.first().and_then(|r| r.first()).and_then(Value::as_i64).map(|n| n.max(0) as u64),
            Err(_) => None,
        };
        self.opts.check()?;
        Ok(n)
    }

    fn leaf(&mut self, n: &Node, what: &str, restrict: Vec<Expr>) -> R<Rel> {
        let mut leaf = self.prepared(n)?;
        let restricted = restrict.iter().map(|c| if let Expr::In { list, .. } = c { list.len() } else { 0 }).max().unwrap_or(0);
        if !restrict.is_empty() {
            let s = leaf.search_mut().expect("a leaf");
            let mut conds = restrict;
            match std::mem::replace(&mut s.filter, Expr::Lit(Value::Bool(true))) {
                Expr::Lit(Value::Bool(true)) => {}
                Expr::And(xs) => conds.extend(xs),
                f => conds.push(f),
            }
            s.filter = if conds.len() == 1 { conds.pop().expect("a condition") } else { Expr::And(conds) };
        }
        // The scan a LIMIT over joins grows.
        let driven = self.driver.filter(|(p, ..)| std::ptr::eq(*p, n)).map(|(_, cap, _)| cap);
        if let Some(cap) = driven {
            let Node::Project { input, .. } = &mut leaf else { unreachable!("a leaf") };
            match input.as_mut() {
                Node::Limit { limit, .. } => *limit = Some(limit.map_or(cap, |l| l.min(cap))),
                inner => {
                    let below = std::mem::replace(inner, Node::Ref { name: String::new() });
                    *inner = Node::Limit { input: Box::new(below), offset: 0, limit: Some(cap) };
                }
            }
        }
        if let Some(cap) = self.per.filter(|(p, _)| std::ptr::eq(*p, n)).map(|(_, c)| c) {
            if let Some(s) = leaf.search_mut() {
                s.per = Some(cap);
            }
        }
        self.opts.check()?;
        let rs = (self.runner)(&Plan::new(leaf))?;
        if let Some((_, _, read)) = self.driver.as_mut().filter(|(p, ..)| std::ptr::eq(*p, n)) {
            *read = rs.rows.len();
        }
        // The leaf as the plan has it, as the operators' lines give it.
        let text: Vec<String> = Plan::new(n.clone()).explain(self.catalog).iter().map(|l| l.trim().to_string()).collect();
        let mut line = format!("{what}: {} rows: {}", rs.rows.len(), text.join(" / "));
        if restricted > 0 {
            line += &format!(" (restricted to {restricted} join values)");
        }
        self.lines.push(line);
        self.lines.extend(rs.stats.plan.iter().map(|l| format!("  {l}")));
        for r in &rs.rows {
            self.charge.row(r)?;
        }
        let n = rs.rows.len();
        let keys = (rs.keys.len() == n).then_some(rs.keys);
        let scores = (rs.scores.len() == n).then_some(rs.scores);
        self.memory(rs.columns, rs.rows, keys, scores)
    }

    /// `input`, under a LIMIT that wants `want` of its rows, whose rows keep
    /// the order of the leaf `scan` ([`ops::driver_of`]): as the operators
    /// do, the scan reads twice the rows wanted (four times under
    /// `DISTINCT ON`, at least 32), then four times as many, until enough
    /// rows come through or it runs out.
    fn driven(&mut self, input: &Node, scan: &Node, want: usize, what: &str) -> R<Rel> {
        let first = if matches!(input, Node::DistinctOn { .. }) { 4 } else { 2 };
        // A scan that keeps a `MAX k PER` cap itself returns the first rows
        // of each value, so the rows wanted are all it needs to read.
        let per = ops::per_cap(input, scan);
        let mut cap = match per {
            Some(_) => want.max(1),
            None => want.saturating_mul(first).max(32),
        };
        let saved = self.driver.take();
        let saved_per = std::mem::replace(&mut self.per, per.map(|c| (scan as *const Node, c)));
        let r = loop {
            self.driver = Some((scan as *const Node, cap as u64, 0));
            let r = match self.node(input, what).and_then(|r| self.materialized(r)) {
                Ok(r) => r,
                Err(e) => break Err(e),
            };
            let read = self.driver.map_or(0, |(_, _, read)| read);
            if r.1 >= want || read < cap {
                break Ok(r.0);
            }
            cap = cap.saturating_mul(4);
        };
        self.driver = saved;
        self.per = saved_per;
        r
    }

    /// Rows as a memory table: a column per value of a row, then the keys,
    /// the scores, and their position.
    fn memory(&mut self, names: Vec<String>, rows: Vec<Vec<Value>>, keys: Option<Vec<Value>>, scores: Option<Vec<f64>>) -> R<Rel> {
        let width = names.len().max(rows.first().map_or(0, Vec::len));
        let mut fields = Vec::new();
        let mut arrays: Vec<ArrayRef> = Vec::new();
        let mut cols = Vec::with_capacity(width);
        for a in columns_of(&rows, width)? {
            let name = self.fresh("c");
            fields.push(Field::new(&name, a.data_type().clone(), true));
            arrays.push(a);
            cols.push(name);
        }
        let key = match keys {
            Some(k) => {
                let name = self.fresh("key");
                let a = column_of(&k)?;
                fields.push(Field::new(&name, a.data_type().clone(), true));
                arrays.push(a);
                Some(name)
            }
            None => None,
        };
        let score = match scores {
            Some(s) => {
                let name = self.fresh("score");
                fields.push(Field::new(&name, DataType::Float64, true));
                arrays.push(Arc::new(Float64Array::from(s)));
                Score::Col(name)
            }
            None => Score::None,
        };
        let ord = self.fresh("ord");
        fields.push(Field::new(&ord, DataType::UInt64, false));
        arrays.push(Arc::new(UInt64Array::from_iter_values(0..rows.len() as u64)));
        let schema = Arc::new(ArrowSchema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), arrays).map_err(DataFusionError::from)?;
        let mut names = names;
        names.resize(width, String::new());
        let ords = vec![Ord { col: ord, desc: false, nulls_first: false }];
        self.scan(schema, batch, Rel { plan: LogicalPlan::default(), cols, names, key, score, ord: ords })
    }

    /// A memory table of `batch`, sorted by `rel`'s order, as `rel`'s plan.
    fn scan(&mut self, schema: Arc<ArrowSchema>, batch: RecordBatch, rel: Rel) -> R<Rel> {
        let parts = slices(&batch, 8192);
        let sorted = rel.ord.iter().all(|o| !o.desc && !o.nulls_first) && rel.ord.len() == 1;
        let mut mem = MemTable::try_new(schema, vec![parts])?;
        if sorted {
            mem = mem.with_sort_order(vec![rel.sort_exprs()]);
        }
        let name = self.fresh("t");
        let plan = LogicalPlanBuilder::scan(name, provider_as_source(Arc::new(mem)), None)?.build()?;
        Ok(Rel { plan, ..rel })
    }

    /// Runs `rel` and keeps its rows in memory, in its order (`WITH`, and
    /// a join's left side, whose values restrict its right side).
    fn materialize(&mut self, rel: Rel) -> R<Rel> {
        self.materialized(rel).map(|(r, _)| r)
    }

    /// [`Lower::materialize`], and how many rows it holds.
    fn materialized(&mut self, rel: Rel) -> R<(Rel, usize)> {
        let plan = LogicalPlanBuilder::from(rel.plan.clone()).sort(rel.sort_exprs())?.build()?;
        let planned = Arc::new(plan.schema().as_arrow().clone());
        let batches = self.collect(plan)?;
        let schema = batches.first().map_or(planned, |b| b.schema());
        let batch = datafusion::arrow::compute::concat_batches(&schema, &batches).map_err(DataFusionError::from)?;
        // One position replaces the order's columns.
        let n = batch.num_rows();
        let ord = self.fresh("ord");
        let mut fields: Vec<Field> = schema.fields().iter().map(|f| f.as_ref().clone()).collect();
        let mut arrays: Vec<ArrayRef> = batch.columns().to_vec();
        fields.push(Field::new(&ord, DataType::UInt64, false));
        arrays.push(Arc::new(UInt64Array::from_iter_values(0..n as u64)));
        let schema = Arc::new(ArrowSchema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), arrays).map_err(DataFusionError::from)?;
        let r = self.scan(schema, batch, Rel { ord: vec![Ord { col: ord, desc: false, nulls_first: false }], ..rel })?;
        Ok((r, n))
    }

    /// `rel` with every column renamed (a `WITH` result read again).
    fn renamed(&mut self, rel: Rel) -> R<Rel> {
        let mut exprs = Vec::new();
        let mut rename = |l: &mut Self, c: &str| {
            let n = l.fresh("c");
            exprs.push(col(c).alias(&n));
            n
        };
        let cols = rel.cols.iter().map(|c| rename(self, c)).collect();
        let key = rel.key.as_ref().map(|k| rename(self, k));
        let score = match &rel.score {
            Score::Col(s) => Score::Col(rename(self, s)),
            s => s.clone(),
        };
        let ord = rel.ord.iter().map(|o| Ord { col: rename(self, &o.col), ..o.clone() }).collect();
        let plan = LogicalPlanBuilder::from(rel.plan).project(exprs)?.build()?;
        Ok(Rel { plan, cols, names: rel.names, key, score, ord })
    }

    fn sort(&mut self, r: Rel, keys: &[SortKey]) -> R<Rel> {
        let mut extra = Vec::new();
        let mut ord = Vec::new();
        for k in keys {
            let e = self.top(&k.expr, &r.cols, r.plan.schema())?;
            if matches!(e.get_type(r.plan.schema())?, DataType::List(_)) {
                return no("a sort by a multi-valued column");
            }
            let name = match &e {
                DExpr::Column(c) => c.name.clone(),
                _ => {
                    let n = self.fresh("o");
                    extra.push(e.alias(&n));
                    n
                }
            };
            ord.push(Ord { col: name, desc: k.desc, nulls_first: k.nulls_first });
        }
        let plan = match extra.is_empty() {
            true => r.plan.clone(),
            false => {
                let mut all = r.all();
                all.extend(extra);
                LogicalPlanBuilder::from(r.plan.clone()).project(all)?.build()?
            }
        };
        ord.extend(r.ord.iter().cloned());
        Ok(Rel { plan, ord, ..r })
    }

    /// One column for the order of `r`'s rows: its own, or each row's
    /// position in it.
    fn position(&mut self, r: Rel) -> R<(Rel, String)> {
        if let [o] = &r.ord[..] {
            if !o.desc && !o.nulls_first {
                let c = o.col.clone();
                return Ok((r, c));
            }
        }
        let name = self.fresh("pos");
        let w = row_number().order_by(r.sort_exprs()).build()?.alias(&name);
        let plan = LogicalPlanBuilder::from(r.plan.clone()).window(vec![w])?.build()?;
        let ord = vec![Ord { col: name.clone(), desc: false, nulls_first: false }];
        Ok((Rel { plan, ord, ..r }, name))
    }

    /// The first `per` rows of `r` with equal `keys`, in `r`'s order.
    fn first_per(&mut self, r: Rel, keys: Vec<DExpr>, per: u64) -> R<Rel> {
        let rn = self.fresh("rn");
        let w = row_number().partition_by(keys).order_by(r.sort_exprs()).build()?.alias(&rn);
        let keep = r.all();
        let plan = LogicalPlanBuilder::from(r.plan.clone()).window(vec![w])?.filter(col(&rn).lt_eq(lit(per)))?.project(keep)?.build()?;
        Ok(Rel { plan, ..r })
    }

    fn aggregate(&mut self, r: Rel, keys: &[Expr], aggs: &[Agg]) -> R<Rel> {
        let (r, pos) = self.position(r)?;
        let schema = r.plan.schema().clone();
        let mut group = Vec::new();
        let mut cols = Vec::new();
        for k in keys {
            let e = self.top(k, &r.cols, &schema)?;
            if matches!(e.get_type(&schema)?, DataType::List(_)) {
                return no("a group by a multi-valued column");
            }
            let n = self.fresh("c");
            group.push(e.alias(&n));
            cols.push(n);
        }
        // Each aggregate, and how its value comes from the aggregates'
        // columns.
        let mut parts = Vec::new();
        let mut finals = Vec::new();
        for a in aggs {
            let arg = a.arg.as_ref().map(|x| self.top(x, &r.cols, &schema)).transpose()?;
            let ty = arg.as_ref().map(|x| x.get_type(&schema)).transpose()?;
            if matches!(ty, Some(DataType::List(_))) {
                return no("an aggregate of a multi-valued column");
            }
            let mut part = |l: &mut Self, e: DExpr| {
                let n = l.fresh("a");
                parts.push(e.alias(&n));
                n
            };
            let out = match (a.func, arg, ty) {
                (AggFn::Count, None, _) => col(&part(self, agg_fn::count(lit(1i64)))),
                (AggFn::Count, Some(x), _) => col(&part(self, agg_fn::count(x))),
                (AggFn::CountDistinct, Some(x), _) => col(&part(self, agg_fn::count_distinct(x))),
                (AggFn::Min, Some(x), _) => col(&part(self, agg_fn::min(x))),
                (AggFn::Max, Some(x), _) => col(&part(self, agg_fn::max(x))),
                // Integers sum exactly: a decimal sum, read back as an INT
                // where it fits and a FLOAT where not, as the operators'.
                (AggFn::Sum, Some(x), Some(t)) if is_int(&t) => col(&part(self, agg_fn::sum(cast(x, DataType::Decimal128(38, 0))))),
                (AggFn::Sum, Some(x), Some(t)) if is_float(&t) => col(&part(self, agg_fn::sum(x))),
                (AggFn::Sum, Some(_), Some(DataType::Null)) => lit(ScalarValue::Float64(None)),
                (AggFn::Avg, Some(x), Some(t)) if is_int(&t) => {
                    let s = part(self, agg_fn::sum(cast(x.clone(), DataType::Decimal128(38, 0))));
                    let c = part(self, agg_fn::count(x));
                    when(col(&c).eq(lit(0i64)), lit(ScalarValue::Float64(None))).otherwise(cast(col(&s), DataType::Float64) / cast(col(&c), DataType::Float64))?
                }
                (AggFn::Avg, Some(x), Some(t)) if is_float(&t) => col(&part(self, agg_fn::avg(x))),
                (AggFn::Avg, Some(_), Some(DataType::Null)) => lit(ScalarValue::Float64(None)),
                (f, _, t) => return no(format!("aggregate {f:?} of {t:?}")),
            };
            finals.push(out);
        }
        let first = self.fresh("first");
        parts.push(agg_fn::min(col(&pos)).alias(&first));
        let mut out_cols = cols.clone();
        let mut project: Vec<DExpr> = cols.iter().map(|c| col(c)).collect();
        for f in finals {
            let n = self.fresh("c");
            project.push(f.alias(&n));
            out_cols.push(n);
        }
        project.push(col(&first));
        let plan = LogicalPlanBuilder::from(r.plan).aggregate(group, parts)?.project(project)?.build()?;
        let names = (0..out_cols.len()).map(|i| format!("#{i}")).collect();
        Ok(Rel { plan, cols: out_cols, names, key: None, score: Score::None, ord: vec![Ord { col: first, desc: false, nulls_first: false }] })
    }

    fn join(&mut self, left: &Node, right: &Node, kind: JoinKind, keys: &[crate::query::ir::JoinKey], on: Option<&Expr>) -> R<Rel> {
        if keys.iter().any(|k| k.left_links || k.right_links) {
            return no("a join along links");
        }
        let mut l = self.node(left, "scan")?;
        let r = if right.is_leaf() {
            // As the operators do: the right side read only for the join
            // values the left holds.
            let (m, left_rows) = self.materialized(l)?;
            l = m;
            // Many values cost the restricted scan more than reading the
            // rows they would cut: the right side is read whole and hashed.
            let whole = left_rows > RESTRICT_ALWAYS
                && plain(right)
                && match self.count(right)? {
                    Some(rows) if (left_rows as u64).saturating_mul(RESTRICT_RATIO) > rows => {
                        let values = self.distinct_keys(&l, keys)?;
                        values > RESTRICT_ALWAYS && (values as u64).saturating_mul(RESTRICT_RATIO) > rows
                    }
                    _ => false,
                };
            if whole {
                let r = self.leaf(right, "join", Vec::new())?;
                return self.joined(l, r, kind, keys, on);
            }
            let Node::Project { columns, .. } = right else { unreachable!("a leaf") };
            let schema = self.table(right)?;
            let mut restrict = Vec::new();
            let mut none = false;
            for k in keys {
                let e = self.top(&k.left, &l.cols, l.plan.schema())?;
                if matches!(e.get_type(l.plan.schema())?, DataType::List(_)) {
                    return no("a join on a multi-valued column");
                }
                let v = self.fresh("v");
                let plan = LogicalPlanBuilder::from(l.plan.clone()).project(vec![e.alias(&v)])?.filter(col(&v).is_not_null())?.distinct()?.build()?;
                let mut vals = Vec::new();
                for b in self.collect(plan)? {
                    vals.extend(values_of(b.column(0))?);
                }
                let j = ops::output(&k.right)?;
                let Some(c) = columns.get(j) else { return no(format!("no column {j} in the join's right rows")) };
                let Expr::Field(f) = &c.expr else { return no("a join key of a scanned table that is not one of its fields") };
                let vals = ops::equal_to(Some(schema), Some(*f as usize), vals)?;
                none |= vals.is_empty();
                restrict.push(Expr::In { expr: Box::new(Expr::Field(*f)), list: vals.into_iter().map(Expr::Lit).collect() });
            }
            match none {
                true => {
                    let names = columns.iter().map(|c| c.name.clone()).collect();
                    self.memory(names, Vec::new(), None, None)?
                }
                false => self.leaf(right, "join", restrict)?,
            }
        } else {
            self.node(right, "join")?
        };
        self.joined(l, r, kind, keys, on)
    }

    /// How many distinct values, NULL aside, the join keys take in the
    /// rows of `l` (summed over the keys).
    fn distinct_keys(&mut self, l: &Rel, keys: &[crate::query::ir::JoinKey]) -> R<usize> {
        let mut total = 0;
        for k in keys {
            let e = self.top(&k.left, &l.cols, l.plan.schema())?;
            let (v, c) = (self.fresh("v"), self.fresh("n"));
            let plan = LogicalPlanBuilder::from(l.plan.clone())
                .project(vec![e.alias(&v)])?
                .aggregate(Vec::<DExpr>::new(), vec![agg_fn::count_distinct(col(&v)).alias(&c)])?
                .build()?;
            for b in self.collect(plan)? {
                total += values_of(b.column(0))?.first().and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
            }
        }
        Ok(total)
    }

    /// The join of `l` and `r` on `keys` (and `on`).
    fn joined(&mut self, l: Rel, r: Rel, kind: JoinKind, keys: &[crate::query::ir::JoinKey], on: Option<&Expr>) -> R<Rel> {
        let both: Vec<String> = l.cols.iter().chain(&r.cols).cloned().collect();
        let mut conds = Vec::new();
        for k in keys {
            let a = self.top(&k.left, &l.cols, l.plan.schema())?;
            let b = self.top(&k.right, &r.cols, r.plan.schema())?;
            let (ta, tb) = (a.get_type(l.plan.schema())?, b.get_type(r.plan.schema())?);
            if matches!(ta, DataType::List(_)) || matches!(tb, DataType::List(_)) {
                return no("a join on a multi-valued column");
            }
            comparable(&ta, &tb)?;
            conds.push(a.eq(b));
        }
        let jt = match kind {
            JoinKind::Inner => JoinType::Inner,
            JoinKind::Left => JoinType::Left,
        };
        if let Some(c) = on {
            // The condition reads the joined row's columns.
            let probe = LogicalPlanBuilder::from(l.plan.clone()).join_on(r.plan.clone(), JoinType::Inner, conds.clone())?.build()?;
            conds.push(self.top_cond(c, &both, probe.schema())?);
        }
        let plan = LogicalPlanBuilder::from(l.plan.clone()).join_on(r.plan.clone(), jt, conds)?.build()?;
        let score = match &l.score {
            Score::Col(s) => Score::Col(s.clone()),
            _ => Score::Zero,
        };
        let mut ord = l.ord.clone();
        ord.extend(r.ord.iter().cloned());
        let names = l.names.iter().chain(&r.names).cloned().collect();
        Ok(Rel { plan, cols: both, names, key: None, score, ord })
    }

    /// `r`'s rows in the order of `b`'s rows with the same primary key
    /// (the last of them where several have it), those without one last.
    fn key_order(&mut self, r: Rel, b: Rel) -> R<Rel> {
        let (Some(rk), Some(bk)) = (r.key.clone(), b.key.clone()) else { return Ok(r) };
        let (b, pos) = self.position(b)?;
        let (k, at) = (self.fresh("k"), self.fresh("at"));
        let by = LogicalPlanBuilder::from(b.plan).aggregate(vec![col(&bk).alias(&k)], vec![agg_fn::max(col(&pos)).alias(&at)])?.build()?;
        let plan = LogicalPlanBuilder::from(r.plan.clone()).join_on(by, JoinType::Left, vec![col(&rk).eq(col(&k))])?.build()?;
        let mut ord = vec![Ord { col: at, desc: false, nulls_first: false }];
        ord.extend(r.ord.iter().cloned());
        Ok(Rel { plan, ord, ..r })
    }

    fn table(&self, leaf: &Node) -> R<&'a Schema> {
        let name = leaf.search().map(|s| s.table.as_str()).unwrap_or_default();
        let catalog: &'a [&'a Schema] = self.catalog;
        match catalog.iter().find(|s| s.table.eq_ignore_ascii_case(name)) {
            Some(s) => Ok(*s),
            None => Err(Fail::Err(Error::Query(format!("no such table: {name}")))),
        }
    }

    /// A condition: an expression that is TRUE, FALSE or NULL.
    fn cond(&mut self, e: &Expr, cols: &[String], schema: &DFSchema) -> R<DExpr> {
        let x = self.expr(e, cols, schema)?;
        match x.get_type(schema)? {
            DataType::Boolean | DataType::Null => Ok(x),
            t => no(format!("a condition of type {t}")),
        }
    }

    /// An expression over rows whose column `i` is `cols[i]`.
    fn expr(&mut self, e: &Expr, cols: &[String], schema: &DFSchema) -> R<DExpr> {
        let ty = |x: &DExpr| x.get_type(schema).map_err(Fail::from);
        Ok(match e {
            Expr::Output(i) => match cols.get(*i as usize) {
                Some(c) => col(c),
                None => return Err(Fail::Err(Error::Query(format!("no column {i} in rows of {} columns", cols.len())))),
            },
            Expr::Lit(v) => lit(scalar(v)?),
            Expr::Not(x) => not(self.cond(x, cols, schema)?),
            Expr::And(xs) | Expr::Or(xs) => {
                let and = matches!(e, Expr::And(_));
                let mut out: Option<DExpr> = None;
                for x in xs {
                    let c = self.cond(x, cols, schema)?;
                    out = Some(match out {
                        None => c,
                        Some(o) if and => o.and(c),
                        Some(o) => o.or(c),
                    });
                }
                out.unwrap_or(lit(and))
            }
            Expr::IsNull(x) => {
                let x = self.expr(x, cols, schema)?;
                if matches!(ty(&x)?, DataType::List(_)) {
                    return no("IS NULL of a multi-valued column");
                }
                x.is_null()
            }
            Expr::Cmp { op, left, right } => {
                let (a, b) = (self.expr(left, cols, schema)?, self.expr(right, cols, schema)?);
                comparable(&ty(&a)?, &ty(&b)?)?;
                let op = match op {
                    CmpOp::Eq => Operator::Eq,
                    CmpOp::Ne => Operator::NotEq,
                    CmpOp::Lt => Operator::Lt,
                    CmpOp::Le => Operator::LtEq,
                    CmpOp::Gt => Operator::Gt,
                    CmpOp::Ge => Operator::GtEq,
                };
                binary_expr(a, op, b)
            }
            Expr::Between { expr, lo, hi } => {
                let x = self.expr(expr, cols, schema)?;
                let tx = ty(&x)?;
                let mut out: Option<DExpr> = None;
                for (bound, op) in [(lo, Operator::GtEq), (hi, Operator::LtEq)] {
                    if let Some(b) = bound {
                        let b = self.expr(b, cols, schema)?;
                        comparable(&tx, &ty(&b)?)?;
                        let c = binary_expr(x.clone(), op, b);
                        out = Some(match out {
                            None => c,
                            Some(o) => o.and(c),
                        });
                    }
                }
                out.unwrap_or_else(|| x.is_not_null())
            }
            Expr::In { expr, list } => {
                let x = self.expr(expr, cols, schema)?;
                let tx = ty(&x)?;
                if matches!(tx, DataType::List(_)) {
                    return no("IN of a multi-valued column");
                }
                let mut items = Vec::with_capacity(list.len());
                for i in list {
                    let i = self.expr(i, cols, schema)?;
                    comparable(&tx, &ty(&i)?)?;
                    items.push(i);
                }
                if items.is_empty() {
                    return Ok(lit(false));
                }
                in_list(x, items, false)
            }
            Expr::Arith { op, left, right } => {
                let (a, b) = (self.expr(left, cols, schema)?, self.expr(right, cols, schema)?);
                self.arith(*op, a, b, schema)?
            }
            Expr::Neg(x) => {
                let x = self.expr(x, cols, schema)?;
                match ty(&x)? {
                    t if is_int(&t) => int_arith(ArithOp::Sub, lit(0i64), cast(x, DataType::Int64)),
                    t if is_float(&t) => DExpr::Negative(Box::new(x)),
                    DataType::Null => lit(ScalarValue::Null),
                    t => return no(format!("negation of {t}")),
                }
            }
            Expr::Concat(xs) => {
                let mut args = Vec::new();
                for x in xs {
                    let x = self.expr(x, cols, schema)?;
                    match ty(&x)? {
                        t if is_text(&t) || t == DataType::Null => args.push(x),
                        t => return no(format!("|| of {t}")),
                    }
                }
                datafusion::functions::string::expr_fn::concat(args)
            }
            Expr::Like { expr, pattern, case_insensitive } => {
                let (x, p) = (self.expr(expr, cols, schema)?, self.expr(pattern, cols, schema)?);
                if !is_text(&ty(&x)?) || !matches!(&**pattern, Expr::Lit(Value::Str(s)) if !s.contains('\\')) {
                    return no("LIKE of other than text with a pattern written out");
                }
                DExpr::Like(Like::new(false, Box::new(x), Box::new(p), None, *case_insensitive))
            }
            Expr::Prefix { expr, prefix, edits: 0 } => {
                let (x, p) = (self.expr(expr, cols, schema)?, self.expr(prefix, cols, schema)?);
                if !is_text(&ty(&x)?) || !is_text(&ty(&p)?) {
                    return no("PREFIX of other than text");
                }
                datafusion::functions::string::expr_fn::starts_with(x, p)
            }
            Expr::Case { operand, whens, otherwise } => {
                let base = operand.as_ref().map(|o| self.expr(o, cols, schema)).transpose()?;
                let mut out_ty: Option<DataType> = None;
                let mut same = |t: DataType| -> R<()> {
                    if t == DataType::Null {
                        return Ok(());
                    }
                    match &out_ty {
                        Some(o) if *o != t => no(format!("CASE of {o} and {t}")),
                        _ => {
                            out_ty = Some(t);
                            Ok(())
                        }
                    }
                };
                let mut whens_d = Vec::new();
                let mut thens = Vec::new();
                for w in whens {
                    let c = match &base {
                        Some(b) => {
                            let v = self.expr(&w.when, cols, schema)?;
                            comparable(&ty(b)?, &ty(&v)?)?;
                            b.clone().eq(v)
                        }
                        None => self.cond(&w.when, cols, schema)?,
                    };
                    let t = self.expr(&w.then, cols, schema)?;
                    same(ty(&t)?)?;
                    whens_d.push(c);
                    thens.push(t);
                }
                let other = otherwise.as_ref().map(|o| self.expr(o, cols, schema)).transpose()?;
                if let Some(o) = &other {
                    same(ty(o)?)?;
                }
                let mut it = whens_d.into_iter().zip(thens);
                let Some((w0, t0)) = it.next() else { return Ok(other.unwrap_or(lit(ScalarValue::Null))) };
                let mut b = when(w0, t0);
                for (w, t) in it {
                    b = b.when(w, t);
                }
                match other {
                    Some(o) => b.otherwise(o)?,
                    None => b.end()?,
                }
            }
            Expr::Call { func, args } => {
                let xs: Vec<DExpr> = args.iter().map(|a| self.expr(a, cols, schema)).collect::<R<_>>()?;
                let ts: Vec<DataType> = xs.iter().map(&ty).collect::<R<_>>()?;
                self.call(*func, xs, &ts, args)?
            }
            e => return no(format!("{} above a leaf", what(e))),
        })
    }

    fn arith(&mut self, op: ArithOp, a: DExpr, b: DExpr, schema: &DFSchema) -> R<DExpr> {
        let (ta, tb) = (a.get_type(schema)?, b.get_type(schema)?);
        let num = |t: &DataType| is_int(t) || is_float(t) || *t == DataType::Null;
        if !num(&ta) || !num(&tb) {
            return no(format!("{ta} {op:?} {tb}"));
        }
        let ints = (is_int(&ta) || ta == DataType::Null) && (is_int(&tb) || tb == DataType::Null);
        let f64 = |x: DExpr| cast(x, DataType::Float64);
        let zero = |x: DExpr| x.eq(lit(0i64));
        Ok(match op {
            // A FLOAT, and NULL for a zero divisor.
            ArithOp::Div => when(zero(b.clone()), lit(ScalarValue::Float64(None))).otherwise(f64(a) / f64(b))?,
            ArithOp::Mod if ints => when(zero(b.clone()), lit(ScalarValue::Int64(None))).otherwise(cast(a, DataType::Int64) % cast(b, DataType::Int64))?,
            ArithOp::Mod => when(zero(b.clone()), lit(ScalarValue::Float64(None))).otherwise(f64(a) % f64(b))?,
            _ if ints => int_arith(op, cast(a, DataType::Int64), cast(b, DataType::Int64)),
            ArithOp::Add => f64(a) + f64(b),
            ArithOp::Sub => f64(a) - f64(b),
            ArithOp::Mul => f64(a) * f64(b),
        })
    }

    fn call(&mut self, func: Func, xs: Vec<DExpr>, ts: &[DataType], args: &[Expr]) -> R<DExpr> {
        use datafusion::functions::expr_fn as f;
        let text = ts.iter().all(|t| is_text(t) || *t == DataType::Null);
        Ok(match func {
            Func::Lower if text => f::lower(xs[0].clone()),
            Func::Upper if text => f::upper(xs[0].clone()),
            Func::Length if text => cast(f::character_length(xs[0].clone()), DataType::Int64),
            Func::Coalesce => {
                let mut kind: Option<&DataType> = None;
                for t in ts.iter().filter(|t| **t != DataType::Null) {
                    if matches!(t, DataType::List(_)) || kind.is_some_and(|k| k != t) {
                        return no("coalesce of values of different types");
                    }
                    kind = Some(t);
                }
                f::coalesce(xs)
            }
            Func::Abs | Func::Round | Func::Ln | Func::Log | Func::Exp | Func::Sqrt | Func::Pow | Func::Least | Func::Greatest | Func::Sigmoid | Func::Clamp => {
                if !ts.iter().all(|t| is_int(t) || is_float(t) || *t == DataType::Null) {
                    return no(format!("{} of {ts:?}", func.name()));
                }
                // The evaluator's own function, so the values agree to the bit.
                let ret = match func {
                    Func::Abs if is_int(&ts[0]) => DataType::Int64,
                    Func::Round => match args.get(1) {
                        None => DataType::Int64,
                        Some(Expr::Lit(Value::Int(d))) if *d <= 0 => DataType::Int64,
                        Some(Expr::Lit(Value::Int(_))) => DataType::Float64,
                        Some(_) => return no("round to decimals not written out"),
                    },
                    _ => DataType::Float64,
                };
                ScalarUDF::new_from_impl(StrataCall::new(func, ret)).call(xs)
            }
            f => return no(format!("{}({ts:?}) above a leaf", f.name())),
        })
    }
}

/// A join restricts its right side (a leaf) to the left side's join values
/// when there are at most this many of them, as the operators do.
const RESTRICT_ALWAYS: usize = 1024;

/// Beyond [`RESTRICT_ALWAYS`] values, a join restricts its right side only
/// when that side reads at least this many times as many rows as there
/// are values: a value costs the restricted scan about what reading tens
/// of rows does (a primary key's lookup more), so a join on a large key
/// set reads the right side whole and hashes it. Measured: a million keys
/// against a million-row table, 1.1 s restricted, 0.15 s read whole.
const RESTRICT_RATIO: u64 = 32;

/// Whether the leaf `n` returns the same rows restricted or not, but for
/// the rows the restriction drops: `Project [Sort] [Filter] Search`, with
/// no LIMIT or aggregate whose rows would change with it.
fn plain(n: &Node) -> bool {
    let Node::Project { input, .. } = n else { return false };
    let mut n: &Node = input;
    loop {
        match n {
            Node::Sort { input, .. } | Node::Filter { input, .. } => n = input,
            Node::Search(s) => return s.per.is_none(),
            _ => return false,
        }
    }
}

/// `a op b` of two INT columns: exact, failing (so the plan falls back to
/// the operators, which give a FLOAT) where the result leaves 64 bits.
fn int_arith(op: ArithOp, a: DExpr, b: DExpr) -> DExpr {
    ScalarUDF::new_from_impl(IntArith::new(op)).call(vec![a, b])
}

/// What an expression is, for a reason it is not lowered.
fn what(e: &Expr) -> String {
    let d = format!("{e:?}");
    d.split(['(', ' ', '{']).next().unwrap_or("").to_string()
}

fn is_int(t: &DataType) -> bool {
    matches!(t, DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 | DataType::UInt8 | DataType::UInt16 | DataType::UInt32)
}

fn is_float(t: &DataType) -> bool {
    matches!(t, DataType::Float32 | DataType::Float64)
}

fn is_text(t: &DataType) -> bool {
    matches!(t, DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View)
}

/// Whether two types compare as Strata compares their values: numbers by
/// value, text by bytes, booleans and times as themselves; NULL with any.
fn comparable(a: &DataType, b: &DataType) -> R<()> {
    let num = |t: &DataType| is_int(t) || is_float(t) || matches!(t, DataType::Decimal128(..));
    let ok = *a == DataType::Null
        || *b == DataType::Null
        || (num(a) && num(b))
        || (is_text(a) && is_text(b))
        || (*a == DataType::Boolean && *b == DataType::Boolean)
        || (matches!(a, DataType::Timestamp(..)) && matches!(b, DataType::Timestamp(..)));
    match ok {
        true => Ok(()),
        false => no(format!("a comparison of {a} with {b}")),
    }
}

fn scalar(v: &Value) -> R<ScalarValue> {
    Ok(match v {
        Value::Null => ScalarValue::Null,
        Value::Int(x) => ScalarValue::Int64(Some(*x)),
        Value::Float(x) => ScalarValue::Float64(Some(*x)),
        Value::Str(s) => ScalarValue::Utf8(Some(s.clone())),
        Value::Bool(b) => ScalarValue::Boolean(Some(*b)),
        Value::Timestamp(t) => ScalarValue::TimestampMicrosecond(Some(*t), None),
        v => return no(format!("a literal {v:?}")),
    })
}

/// `batch` in slices of at most `n` rows (zero-copy).
fn slices(batch: &RecordBatch, n: usize) -> Vec<RecordBatch> {
    let rows = batch.num_rows();
    if rows == 0 {
        return vec![batch.clone()];
    }
    (0..rows).step_by(n).map(|at| batch.slice(at, n.min(rows - at))).collect()
}

/// What Arrow type holds a column's values.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Null,
    Int,
    Float,
    Str,
    Bool,
    Time,
    Vector,
    IntList,
    StrList,
    /// A list without values: of either.
    List,
}

fn kind_of(v: &Value) -> R<Kind> {
    Ok(match v {
        Value::Null => Kind::Null,
        Value::Int(_) => Kind::Int,
        Value::Float(_) => Kind::Float,
        Value::Str(_) => Kind::Str,
        Value::Bool(_) => Kind::Bool,
        Value::Timestamp(_) => Kind::Time,
        Value::Vector(_) => Kind::Vector,
        Value::List(xs) => {
            let mut k = Kind::List;
            for x in xs {
                let kx = match x {
                    Value::Int(_) => Kind::IntList,
                    Value::Str(_) => Kind::StrList,
                    v => return no(format!("a list holding {v:?}")),
                };
                if k != Kind::List && k != kx {
                    return no("a list of INT and text values");
                }
                k = kx;
            }
            k
        }
    })
}

/// One column being built a row at a time: typed by its first value that
/// is not NULL. Lists and vectors are left to [`array_of`].
enum Col {
    Nulls(usize),
    Int(Int64Builder),
    Float(Float64Builder),
    Str(StringBuilder),
    Bool(BooleanBuilder),
    Time(TimestampMicrosecondBuilder),
    Other,
}

impl Col {
    /// The array of `n` rows; `other` builds a column of lists or vectors.
    fn finish(self, n: usize, other: impl FnOnce() -> R<ArrayRef>) -> R<ArrayRef> {
        Ok(match self {
            Col::Nulls(_) => Arc::new(datafusion::arrow::array::NullArray::new(n)),
            Col::Int(mut b) => Arc::new(b.finish()),
            Col::Float(mut b) => Arc::new(b.finish()),
            Col::Str(mut b) => Arc::new(b.finish()),
            Col::Bool(mut b) => Arc::new(b.finish()),
            Col::Time(mut b) => Arc::new(b.finish()),
            Col::Other => other()?,
        })
    }

    fn push(&mut self, v: &Value, n: usize) -> R<()> {
        match (&mut *self, v) {
            (Col::Other, _) => {}
            (Col::Nulls(k), Value::Null) => *k += 1,
            (Col::Nulls(k), v) => {
                let k = *k;
                *self = match v {
                    Value::Int(_) => Col::Int(Int64Builder::with_capacity(n)),
                    Value::Float(_) => Col::Float(Float64Builder::with_capacity(n)),
                    Value::Str(_) => Col::Str(StringBuilder::with_capacity(n, n * 8)),
                    Value::Bool(_) => Col::Bool(BooleanBuilder::with_capacity(n)),
                    Value::Timestamp(_) => Col::Time(TimestampMicrosecondBuilder::with_capacity(n)),
                    _ => Col::Other,
                };
                for _ in 0..k {
                    self.push(&Value::Null, n)?;
                }
                self.push(v, n)?;
            }
            (Col::Int(b), Value::Int(x)) => b.append_value(*x),
            (Col::Float(b), Value::Float(x)) => b.append_value(*x),
            (Col::Str(b), Value::Str(x)) => b.append_value(x),
            (Col::Bool(b), Value::Bool(x)) => b.append_value(*x),
            (Col::Time(b), Value::Timestamp(x)) => b.append_value(*x),
            (Col::Int(b), Value::Null) => b.append_null(),
            (Col::Float(b), Value::Null) => b.append_null(),
            (Col::Str(b), Value::Null) => b.append_null(),
            (Col::Bool(b), Value::Null) => b.append_null(),
            (Col::Time(b), Value::Null) => b.append_null(),
            (_, v) => return no(format!("a column of values of several types, {v:?} among them")),
        }
        Ok(())
    }
}

/// Columns `0..width` of `rows` as Arrow arrays, in one pass over the
/// rows: a column of one type each, NULLs aside.
fn columns_of(rows: &[Vec<Value>], width: usize) -> R<Vec<ArrayRef>> {
    let n = rows.len();
    let mut cols: Vec<Col> = (0..width).map(|_| Col::Nulls(0)).collect();
    for r in rows {
        for (i, c) in cols.iter_mut().enumerate() {
            c.push(r.get(i).unwrap_or(&Value::Null), n)?;
        }
    }
    cols.into_iter().enumerate().map(|(i, c)| c.finish(n, || array_of(rows.iter().map(|r| r.get(i).unwrap_or(&Value::Null))))).collect()
}

/// `values` as one Arrow array ([`columns_of`] of one column).
fn column_of(values: &[Value]) -> R<ArrayRef> {
    let n = values.len();
    let mut c = Col::Nulls(0);
    for v in values {
        c.push(v, n)?;
    }
    c.finish(n, || array_of(values.iter()))
}

/// The values of one column as an Arrow array of their one type.
fn array_of<'v>(values: impl Iterator<Item = &'v Value> + Clone) -> R<ArrayRef> {
    let mut kind = Kind::Null;
    for v in values.clone() {
        let k = kind_of(v)?;
        kind = match (kind, k) {
            (a, Kind::Null) => a,
            (Kind::Null, b) => b,
            (Kind::List, b @ (Kind::IntList | Kind::StrList | Kind::List)) | (b @ (Kind::IntList | Kind::StrList), Kind::List) => b,
            (a, b) if a == b => a,
            (a, b) => return no(format!("a column of {a:?} and {b:?} values")),
        };
    }
    let n = values.clone().count();
    Ok(match kind {
        Kind::Null => Arc::new(datafusion::arrow::array::NullArray::new(n)),
        Kind::Int => Arc::new(values.map(|v| v.as_i64()).collect::<Int64Array>()),
        Kind::Float => Arc::new(values.map(|v| if let Value::Float(x) = v { Some(*x) } else { None }).collect::<Float64Array>()),
        Kind::Str => {
            let mut b = StringBuilder::new();
            values.for_each(|v| b.append_option(v.as_str()));
            Arc::new(b.finish())
        }
        Kind::Bool => Arc::new(values.map(|v| if let Value::Bool(x) = v { Some(*x) } else { None }).collect::<BooleanArray>()),
        Kind::Time => Arc::new(values.map(|v| if let Value::Timestamp(x) = v { Some(*x) } else { None }).collect::<TimestampMicrosecondArray>()),
        Kind::Vector => {
            let mut b = ListBuilder::new(Float32Builder::new());
            for v in values {
                match v {
                    Value::Vector(x) => {
                        b.values().append_slice(x);
                        b.append(true);
                    }
                    _ => b.append(false),
                }
            }
            Arc::new(b.finish())
        }
        Kind::IntList => {
            let mut b = ListBuilder::new(Int64Builder::new());
            for v in values {
                match v {
                    Value::List(xs) => {
                        xs.iter().for_each(|x| b.values().append_option(x.as_i64()));
                        b.append(true);
                    }
                    _ => b.append(false),
                }
            }
            Arc::new(b.finish())
        }
        Kind::StrList | Kind::List => {
            let mut b = ListBuilder::new(StringBuilder::new());
            for v in values {
                match v {
                    Value::List(xs) => {
                        xs.iter().for_each(|x| b.values().append_option(x.as_str()));
                        b.append(true);
                    }
                    _ => b.append(false),
                }
            }
            Arc::new(b.finish())
        }
    })
}

/// The values of an Arrow array, as Strata gives them.
fn values_of(a: &ArrayRef) -> R<Vec<Value>> {
    use datafusion::arrow::datatypes as t;
    let n = a.len();
    let mut out = Vec::with_capacity(n);
    macro_rules! each {
        ($arr:expr, $f:expr) => {{
            let arr = $arr;
            for i in 0..n {
                out.push(if arr.is_null(i) { Value::Null } else { $f(arr.value(i)) });
            }
        }};
    }
    match a.data_type() {
        DataType::Null => out.resize(n, Value::Null),
        DataType::Boolean => each!(a.as_boolean(), Value::Bool),
        DataType::Int8 => each!(a.as_primitive::<t::Int8Type>(), |x| Value::Int(x as i64)),
        DataType::Int16 => each!(a.as_primitive::<t::Int16Type>(), |x| Value::Int(x as i64)),
        DataType::Int32 => each!(a.as_primitive::<t::Int32Type>(), |x| Value::Int(x as i64)),
        DataType::Int64 => each!(a.as_primitive::<t::Int64Type>(), Value::Int),
        DataType::UInt8 => each!(a.as_primitive::<t::UInt8Type>(), |x| Value::Int(x as i64)),
        DataType::UInt16 => each!(a.as_primitive::<t::UInt16Type>(), |x| Value::Int(x as i64)),
        DataType::UInt32 => each!(a.as_primitive::<t::UInt32Type>(), |x| Value::Int(x as i64)),
        DataType::UInt64 => each!(a.as_primitive::<t::UInt64Type>(), |x: u64| i64::try_from(x).map_or(Value::Float(x as f64), Value::Int)),
        DataType::Float32 => each!(a.as_primitive::<t::Float32Type>(), |x| Value::Float(x as f64)),
        DataType::Float64 => each!(a.as_primitive::<t::Float64Type>(), Value::Float),
        // An exact sum: an INT where it fits, else a FLOAT.
        DataType::Decimal128(_, 0) => each!(a.as_primitive::<t::Decimal128Type>(), |x: i128| i64::try_from(x).map_or(Value::Float(x as f64), Value::Int)),
        DataType::Decimal128(_, s) => {
            let s = *s as i32;
            each!(a.as_primitive::<t::Decimal128Type>(), |x: i128| Value::Float(x as f64 / 10f64.powi(s)))
        }
        DataType::Utf8 => each!(a.as_string::<i32>(), |s: &str| Value::Str(s.to_string())),
        DataType::LargeUtf8 => each!(a.as_string::<i64>(), |s: &str| Value::Str(s.to_string())),
        DataType::Utf8View => each!(a.as_string_view(), |s: &str| Value::Str(s.to_string())),
        DataType::Timestamp(TimeUnit::Microsecond, _) => each!(a.as_primitive::<t::TimestampMicrosecondType>(), Value::Timestamp),
        DataType::List(f) => {
            let list = a.as_list::<i32>();
            let vector = matches!(f.data_type(), DataType::Float32);
            for i in 0..n {
                if list.is_null(i) {
                    out.push(Value::Null);
                    continue;
                }
                let inner = list.value(i);
                out.push(match vector {
                    true => Value::Vector(inner.as_primitive::<t::Float32Type>().values().iter().copied().collect()),
                    false => Value::List(values_of(&inner)?),
                });
            }
        }
        t => return no(format!("an answer column of type {t}")),
    }
    Ok(out)
}

/// The values of the arguments, row by row.
fn arg_values(args: &[ColumnarValue], rows: usize) -> datafusion::error::Result<Vec<Vec<Value>>> {
    let arrays = ColumnarValue::values_to_arrays(args)?;
    let cols: Vec<Vec<Value>> = arrays.iter().map(|a| values_of(a).map_err(|_| DataFusionError::Execution(format!("strata: no values of {}", a.data_type())))).collect::<datafusion::error::Result<_>>()?;
    Ok((0..rows).map(|i| cols.iter().map(|c| c.get(i).cloned().unwrap_or(Value::Null)).collect()).collect())
}

/// A scalar function as Strata's evaluator computes it
/// ([`crate::query::ir::eval`]), its values of type `ret`.
#[derive(Debug, PartialEq, Eq)]
struct StrataCall {
    name: String,
    func: Func,
    ret: DataType,
    signature: Signature,
}

impl StrataCall {
    fn new(func: Func, ret: DataType) -> StrataCall {
        StrataCall { name: format!("strata_{}", func.name()), func, ret, signature: Signature::variadic_any(Volatility::Immutable) }
    }
}

impl std::hash::Hash for StrataCall {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.name.hash(h);
        self.ret.hash(h);
    }
}

impl ScalarUDFImpl for StrataCall {

    fn name(&self) -> &str {
        &self.name
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(self.ret.clone())
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> datafusion::error::Result<ColumnarValue> {
        let rows = arg_values(&args.args, args.number_rows)?;
        let fail = |e: Error| DataFusionError::Execution(format!("strata: {e}"));
        let out: ArrayRef = match self.ret {
            DataType::Int64 => {
                let mut b = Int64Builder::with_capacity(rows.len());
                for r in &rows {
                    match crate::query::ir::eval::call(self.func, r).map_err(fail)? {
                        Value::Null => b.append_null(),
                        Value::Int(x) => b.append_value(x),
                        v => return Err(DataFusionError::Execution(format!("strata: {} gave {v:?} for an INT column", self.func.name()))),
                    }
                }
                Arc::new(b.finish())
            }
            _ => {
                let mut b = Float64Builder::with_capacity(rows.len());
                for r in &rows {
                    match crate::query::ir::eval::call(self.func, r).map_err(fail)? {
                        Value::Null => b.append_null(),
                        Value::Float(x) => b.append_value(x),
                        v => return Err(DataFusionError::Execution(format!("strata: {} gave {v:?} for a FLOAT column", self.func.name()))),
                    }
                }
                Arc::new(b.finish())
            }
        };
        Ok(ColumnarValue::Array(out))
    }
}

/// Exact `+`, `-` or `*` of two INT columns: Arrow's checked kernels,
/// which fail where a result leaves 64 bits.
#[derive(Debug, PartialEq, Eq)]
struct IntArith {
    name: &'static str,
    op: ArithOp,
    signature: Signature,
}

impl IntArith {
    fn new(op: ArithOp) -> IntArith {
        let name = match op {
            ArithOp::Add => "strata_add",
            ArithOp::Sub => "strata_sub",
            _ => "strata_mul",
        };
        IntArith { name, op, signature: Signature::exact(vec![DataType::Int64, DataType::Int64], Volatility::Immutable) }
    }
}

impl std::hash::Hash for IntArith {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.name.hash(h);
    }
}

impl ScalarUDFImpl for IntArith {

    fn name(&self) -> &str {
        self.name
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(DataType::Int64)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> datafusion::error::Result<ColumnarValue> {
        let [a, b] = &args.args[..] else { return Err(DataFusionError::Execution("strata: arithmetic of two values".into())) };
        let (a, b) = (a.to_array(args.number_rows)?, b.to_array(args.number_rows)?);
        let out = match self.op {
            ArithOp::Add => numeric::add(&a, &b)?,
            ArithOp::Sub => numeric::sub(&a, &b)?,
            _ => numeric::mul(&a, &b)?,
        };
        let _ = out.as_primitive::<Int64Type>();
        Ok(ColumnarValue::Array(out))
    }
}
