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
//!   and FLOAT values is not lowered.
//! * **Multi-valued columns** are lists, and mean what they mean to the
//!   operators: a condition holds when one of the values does (and is
//!   never UNKNOWN), a row is in a group per distinct value (the group
//!   NULL without any), an aggregate adds each value, and a join matches
//!   on any value, once per pair of rows. A sort, window, or set operation
//!   other than `UNION ALL` by one is not lowered.
//!
//! Whatever is not lowered here (a subquery in a leaf, links of an EDGES
//! field, a function without an equivalent) gives [`Outcome::Fallback`]
//! with the reason, and the caller runs the plan on the operators; so does
//! any error of DataFusion's own, so a plan answers here exactly when
//! DataFusion and the operators agree on what it means.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock};

use datafusion::arrow::array::{
    Array, ArrayRef, AsArray, BooleanBuilder, Float64Array, Float64Builder, Int64Builder, UInt64Array,
};
use datafusion::arrow::compute::kernels::numeric;
use datafusion::arrow::datatypes::{DataType, Field, Int64Type, Schema as ArrowSchema, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::{Column as DfColumn, DFSchema, DataFusionError, ScalarValue};
use datafusion::datasource::{provider_as_source, MemTable};
use datafusion::execution::context::SessionContext;
use datafusion::execution::memory_pool::{MemoryLimit, MemoryPool, MemoryReservation};
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::functions_aggregate::expr_fn as agg_fn;
use datafusion::functions_window::expr_fn as win_fn;
use datafusion::functions_window::expr_fn::row_number;
use datafusion::logical_expr::expr::Like;
use datafusion::logical_expr::{
    binary_expr, cast, in_list, lit, not, when, ColumnarValue, Expr as DExpr, ExprFunctionExt, ExprSchemable, JoinType, LogicalPlan, LogicalPlanBuilder, Operator, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature, SortExpr, Volatility,
};
use datafusion::physical_plan::{displayable, ExecutionPlan};
use datafusion::prelude::SessionConfig;

use crate::error::{Error, Result};
use crate::query::arrow::{self, Columns, Mixed};
use crate::query::exec::{Engine, ExecOptions, ExecStats, Leaf, ResultSet};
use crate::query::ir::{Agg, ArithOp, Bound, CmpOp, Expr, FrameUnits, Func, JoinKind, Node, Plan, SetOp, SortKey, When, WindowFn, WindowFunc};
use crate::query::memory::Charge;
use crate::query::ops;
use crate::query::steps::{self, Clock, Step};
use crate::query::AggFn;
use crate::schema::Schema;
use crate::value::Value;

/// Runs a leaf plan for DataFusion: its rows, as Arrow columns
/// ([`Leaf::Arrow`]) when there are more than the number it is given.
pub type Runner<'r> = ops::LeafRunner<'r>;

/// What [`run_in`] made of a plan.
#[derive(Debug)]
pub enum Outcome {
    /// DataFusion answered it.
    Done(ResultSet),
    /// Not lowered (or DataFusion failed on it): why. The operators run
    /// it, with what ran here.
    Fallback(String, Ran),
    /// [`Engine::Auto`]: its leaves read at most [`SMALL`] rows before
    /// DataFusion would run, so the operators, faster on so few, answer
    /// it, with what ran here.
    Small(Ran),
}

/// What a plan ran here before the operators took it over: its leaves'
/// rows while they were few (at most [`SMALL`] in all), each with the leaf
/// plan the operators run for it, so that they do not run it again; and
/// the steps of the query's profile so far, the leaves' among them (the
/// rows kept have none, so that no step counts twice).
#[derive(Debug, Default)]
pub struct Ran {
    pub leaves: Vec<(Plan, ResultSet)>,
    pub steps: Vec<Step>,
}

impl Ran {
    /// The rows of `leaf` if it ran here, once.
    pub fn take(&mut self, leaf: &Plan) -> Option<ResultSet> {
        let i = self.leaves.iter().position(|(p, _)| p == leaf)?;
        Some(self.leaves.swap_remove(i).1)
    }
}

/// The most rows the leaves of a plan read for [`Engine::Auto`] to leave
/// it to the operators. DataFusion costs about 1 ms to plan and run a
/// plan, whatever its size, and the operators about 1-2 µs a row more than
/// it: on two tables of 1,000 rows a join with a group took 4 ms on the
/// operators and 6 ms on DataFusion, on two of 10,000 rows 23 ms and
/// 10 ms (`examples/df_join`, 4-core cloud VM).
pub const SMALL: usize = 4096;

/// Why a plan does not run here: not lowered, small enough for the
/// operators ([`Outcome::Small`]), or a Strata error (a leaf's, the
/// deadline's, the memory budget's), which the query returns.
enum Fail {
    No(String),
    Small,
    Err(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::Err(e)
    }
}

impl From<Mixed> for Fail {
    fn from(m: Mixed) -> Fail {
        Fail::No(m.0)
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
    let charge = Charge::new(opts.memory.clone());
    let pool = Arc::new(Pool { charge: charge.clone(), limited: opts.memory.is_some(), used: AtomicUsize::new(0), refused: Mutex::new(None) });
    let env = RuntimeEnvBuilder::new().with_memory_pool(pool.clone()).build_arc().map_err(|e| Error::Query(format!("datafusion: {e}")))?;
    let ctx = SessionContext::new_with_config_rt(config, env);
    let mut l = Lower { catalog, runner, opts, ctx, pool, next: 0, ctes: Vec::new(), driver: None, per: None, lines: Vec::new(), charge, filter_docs: 0, scored: 0, clock: Clock::new(opts.profile), prof: Vec::new(), dropped: 0, tables: HashMap::new(), leaf_rows: 0, ran: Vec::new() };
    match l.answer(&plan.root) {
        Ok(rs) => Ok(Outcome::Done(rs)),
        Err(Fail::No(why)) => Ok(Outcome::Fallback(why, l.ran())),
        Err(Fail::Small) => Ok(Outcome::Small(l.ran())),
        Err(Fail::Err(e)) => Err(e),
    }
}

/// DataFusion's memory, counted against the query's [`Charge`] while its
/// joins, groups and sorts hold it, and given back as they free it. Past
/// the budget a reservation is refused: a sort or a group that can spill
/// to disk then spills, and anything else fails the query with Strata's
/// [`Error::Memory`].
struct Pool {
    charge: Charge,
    limited: bool,
    used: AtomicUsize,
    /// The bytes of the last reservation refused.
    refused: Mutex<Option<u64>>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pool {{ used: {} }}", self.used.load(AtomicOrdering::Relaxed))
    }
}

impl std::fmt::Display for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "strata query memory ({} bytes held)", self.used.load(AtomicOrdering::Relaxed))
    }
}

impl MemoryPool for Pool {
    fn name(&self) -> &str {
        "strata"
    }

    fn grow(&self, _: &MemoryReservation, additional: usize) {
        // Infallible: counted, and the next fallible reservation is refused.
        self.charge.add_anyway(additional as u64);
        self.used.fetch_add(additional, AtomicOrdering::Relaxed);
    }

    fn shrink(&self, _: &MemoryReservation, shrink: usize) {
        self.charge.sub(shrink as u64);
        self.used.fetch_sub(shrink, AtomicOrdering::Relaxed);
    }

    fn try_grow(&self, _: &MemoryReservation, additional: usize) -> datafusion::common::Result<()> {
        if self.charge.try_add(additional as u64) {
            self.used.fetch_add(additional, AtomicOrdering::Relaxed);
            return Ok(());
        }
        *self.refused.lock().unwrap_or_else(|e| e.into_inner()) = Some(additional as u64);
        Err(DataFusionError::ResourcesExhausted(format!("{additional} bytes more than the query memory budget leaves")))
    }

    fn reserved(&self) -> usize {
        self.used.load(AtomicOrdering::Relaxed)
    }

    fn memory_limit(&self) -> MemoryLimit {
        if self.limited {
            MemoryLimit::Unknown
        } else {
            MemoryLimit::Infinite
        }
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
    /// The memory DataFusion holds, counted against `charge`.
    pool: Arc<Pool>,
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
    /// The leaves' rows passing their filters, and scored, summed, as the
    /// operators sum them.
    filter_docs: u64,
    scored: u64,
    /// The steps of the query's profile ([`steps`]): each leaf's, its
    /// executor's inside, and each DataFusion plan's operators.
    clock: Clock,
    prof: Vec<Step>,
    dropped: u64,
    /// The batch of each memory table in its own order, by name, and the
    /// column of that order: the rows a scan of it gives.
    tables: HashMap<String, (String, RecordBatch)>,
    /// The rows the leaves read, and their rows while those are at most
    /// [`SMALL`] ([`Ran`]).
    leaf_rows: usize,
    ran: Vec<(Plan, ResultSet)>,
}

impl<'a> Lower<'a, '_> {
    fn ran(&mut self) -> Ran {
        Ran { leaves: std::mem::take(&mut self.ran), steps: std::mem::take(&mut self.prof) }
    }

    fn fresh(&mut self, what: &str) -> String {
        self.next += 1;
        format!("__{what}{}", self.next)
    }

    fn answer(&mut self, root: &Node) -> R<ResultSet> {
        let rel = self.node(root, "query")?;
        let (mut rs, _) = self.rows(rel)?;
        rs.stats.plan = std::mem::take(&mut self.lines);
        rs.stats.filter_docs = self.filter_docs;
        rs.stats.scored = self.scored;
        rs.stats.steps = std::mem::take(&mut self.prof);
        rs.stats.steps_dropped = self.dropped;
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
    /// Runs `plan` on DataFusion's threads, checking the query's deadline
    /// and cancel flag while it runs: past them, the plan is stopped and
    /// the query fails as the operators' would.
    fn collect(&mut self, plan: LogicalPlan) -> R<Vec<RecordBatch>> {
        if self.opts.engine == Engine::Auto && self.leaf_rows <= SMALL {
            return Err(Fail::Small);
        }
        self.opts.check()?;
        *self.pool.refused.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let ctx = self.ctx.clone();
        let m = self.clock.mark(&self.prof);
        let (tx, rx) = std::sync::mpsc::channel();
        let task = runtime().spawn(async move {
            let r = async move {
                let df = ctx.execute_logical_plan(plan).await?;
                let task = Arc::new(df.task_ctx());
                let physical = df.create_physical_plan().await?;
                let batches = datafusion::physical_plan::collect(physical.clone(), task).await?;
                Ok::<_, DataFusionError>((batches, physical))
            }
            .await;
            let _ = tx.send(r);
        });
        let r = loop {
            match rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok(r) => break r,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(e) = self.opts.check() {
                        task.abort();
                        return Err(Fail::Err(e));
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err(Fail::Err(Error::Query("datafusion: the plan's task stopped without an answer".into()))),
            }
        };
        let (batches, physical) = r.map_err(|e| match (e.find_root(), self.pool.refused.lock().unwrap_or_else(|e| e.into_inner()).take()) {
            // Past the memory budget: the query's answer, as the operators'.
            (DataFusionError::ResourcesExhausted(_), Some(wanted)) => Fail::Err(self.charge.refuse(self.charge.counted().saturating_add(wanted))),
            _ => Fail::from(e),
        })?;
        // The plan's operators, each a step inside this one.
        let wall = m.t0.elapsed().as_nanos() as f64 / 1e3;
        let ops = operator_steps(&physical, steps::us_at(m.t0), wall, self.clock.cpu);
        // Their CPU time, on DataFusion's threads, counts in this step's.
        if let Some(root) = ops.first() {
            self.clock.helped_by(root);
        }
        self.dropped += steps::nest(&mut self.prof, &ops, 0) as u64;
        let rows = batches.iter().map(|b| b.num_rows() as u64).sum();
        let top = ops.first().map(|s| s.op.clone()).unwrap_or_default();
        if self.clock.wrap(&mut self.prof, m, "datafusion", top, Some(rows)).is_none() {
            self.dropped += 1;
        }
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
            Node::SetOp { op, all, left, right } => {
                let l = self.node(left, what)?;
                let r = self.node(right, what)?;
                self.set_op(*op, *all, l, r)
            }
            Node::Window { input, funcs } => {
                let r = self.node(input, what)?;
                self.window(r, funcs)
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
        let n = match (self.runner)(&Plan::new(count), usize::MAX) {
            Ok(Leaf::Rows(rs)) => rs.rows.first().and_then(|r| r.first()).and_then(Value::as_i64).map(|n| n.max(0) as u64),
            Ok(Leaf::Arrow(_)) | Err(_) => None,
        };
        self.opts.check()?;
        Ok(n)
    }

    fn leaf(&mut self, node: &Node, what: &str, restrict: Vec<Expr>) -> R<Rel> {
        let mut leaf = self.prepared(node)?;
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
        let driven = self.driver.filter(|(p, ..)| std::ptr::eq(*p, node)).map(|(_, cap, _)| cap);
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
        if let Some(cap) = self.per.filter(|(p, _)| std::ptr::eq(*p, node)).map(|(_, c)| c) {
            if let Some(s) = leaf.search_mut() {
                s.per = Some(cap);
            }
        }
        self.opts.check()?;
        let table = leaf.search().map(|s| s.table.clone()).unwrap_or_default();
        let m = self.clock.mark(&self.prof);
        let leaf = Plan::new(leaf);
        // With `auto`, rows while the leaves read few, for the operators to
        // take over without running them again; Arrow columns past that.
        let over = match self.opts.engine {
            Engine::Auto => SMALL.saturating_sub(self.leaf_rows),
            _ => 0,
        };
        let (n, stats, rel) = match (self.runner)(&leaf, over)? {
            Leaf::Rows(rs) => {
                self.leaf_rows = self.leaf_rows.saturating_add(rs.rows.len());
                match self.leaf_rows <= SMALL {
                    true => {
                        let mut kept = rs.clone();
                        kept.stats.steps = Vec::new();
                        kept.stats.steps_dropped = 0;
                        self.ran.push((leaf, kept));
                    }
                    false => self.ran.clear(),
                }
                for r in &rs.rows {
                    self.charge.row(r)?;
                }
                let n = rs.rows.len();
                let keys = (rs.keys.len() == n).then_some(rs.keys);
                let scores = (rs.scores.len() == n).then_some(rs.scores);
                let width = rs.columns.len().max(rs.rows.first().map_or(0, Vec::len));
                let arrays = arrow::columns_of(rs.rows, width)?;
                let keys = keys.map(arrow::column_of).transpose()?;
                (n, rs.stats, (rs.columns, arrays, keys, scores))
            }
            Leaf::Arrow(c) => {
                // Not kept for the operators, which run the leaf again
                // if they take the plan over.
                self.leaf_rows = self.leaf_rows.saturating_add(c.rows);
                if self.leaf_rows > SMALL {
                    self.ran.clear();
                }
                self.charge.add(c.bytes())?;
                let Columns { names, arrays, keys, scores, rows, stats } = c;
                let scores = (scores.len() == rows).then_some(scores);
                (rows, stats, (names, arrays, Some(keys), scores))
            }
        };
        self.dropped += steps::nest(&mut self.prof, &stats.steps, 0) as u64 + stats.steps_dropped;
        if let Some((_, _, read)) = self.driver.as_mut().filter(|(p, ..)| std::ptr::eq(*p, node)) {
            *read = n;
        }
        // The leaf as the plan has it, as the operators' lines give it.
        let text: Vec<String> = Plan::new(node.clone()).explain(self.catalog).iter().map(|l| l.trim().to_string()).collect();
        let mut line = format!("{what}: {n} rows: {}", text.join(" / "));
        if restricted > 0 {
            line += &format!(" (restricted to {restricted} join values)");
        }
        self.lines.push(line);
        self.lines.extend(stats.plan.iter().map(|l| format!("  {l}")));
        self.filter_docs += stats.filter_docs;
        self.scored += stats.scored;
        let (names, arrays, keys, scores) = rel;
        let rel = self.memory(names, arrays, keys, scores, n)?;
        if self.clock.wrap(&mut self.prof, m, "leaf", format!("{what}: {table}"), Some(n as u64)).is_none() {
            self.dropped += 1;
        }
        Ok(rel)
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
            if r.1.num_rows() >= want || read < cap {
                break Ok(r.0);
            }
            cap = cap.saturating_mul(4);
        };
        self.driver = saved;
        self.per = saved_per;
        r
    }

    /// A leaf's `n` rows as a memory table: its columns, then the keys,
    /// the scores, and their position.
    fn memory(&mut self, names: Vec<String>, arrays: Vec<ArrayRef>, keys: Option<ArrayRef>, scores: Option<Vec<f64>>, n: usize) -> R<Rel> {
        let width = arrays.len();
        let mut fields = Vec::new();
        let mut cols = Vec::with_capacity(width);
        let mut all: Vec<ArrayRef> = Vec::with_capacity(width + 3);
        for a in arrays {
            let name = self.fresh("c");
            fields.push(Field::new(&name, a.data_type().clone(), true));
            all.push(a);
            cols.push(name);
        }
        let key = match keys {
            Some(a) => {
                let name = self.fresh("key");
                fields.push(Field::new(&name, a.data_type().clone(), true));
                all.push(a);
                Some(name)
            }
            None => None,
        };
        let score = match scores {
            Some(s) => {
                let name = self.fresh("score");
                fields.push(Field::new(&name, DataType::Float64, true));
                all.push(Arc::new(Float64Array::from(s)));
                Score::Col(name)
            }
            None => Score::None,
        };
        let ord = self.fresh("ord");
        fields.push(Field::new(&ord, DataType::UInt64, false));
        all.push(Arc::new(UInt64Array::from_iter_values(0..n as u64)));
        let schema = Arc::new(ArrowSchema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), all).map_err(DataFusionError::from)?;
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
        if sorted {
            self.tables.insert(name.clone(), (rel.ord[0].col.clone(), batch));
        }
        let plan = LogicalPlanBuilder::scan(name, provider_as_source(Arc::new(mem)), None)?.build()?;
        Ok(Rel { plan, ..rel })
    }

    /// The rows of `rel` when it is a whole memory table read in its own
    /// order, as they are held: no plan to run.
    fn held(&self, rel: &Rel) -> Option<RecordBatch> {
        let LogicalPlan::TableScan(t) = &rel.plan else { return None };
        if !t.filters.is_empty() || t.fetch.is_some() || t.projection.is_some() {
            return None;
        }
        let [o] = rel.ord.as_slice() else { return None };
        let (ord, batch) = self.tables.get(t.table_name.table())?;
        (!o.desc && !o.nulls_first && *ord == o.col).then(|| batch.clone())
    }

    /// Runs `rel` and keeps its rows in memory, in its order (`WITH`, and
    /// a join's left side, whose values restrict its right side).
    fn materialize(&mut self, rel: Rel) -> R<Rel> {
        self.materialized(rel).map(|(r, _)| r)
    }

    /// [`Lower::materialize`], and the rows it holds.
    fn materialized(&mut self, rel: Rel) -> R<(Rel, RecordBatch)> {
        if let Some(batch) = self.held(&rel) {
            return Ok((rel, batch));
        }
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
        let r = self.scan(schema, batch.clone(), Rel { ord: vec![Ord { col: ord, desc: false, nulls_first: false }], ..rel })?;
        Ok((r, batch))
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
        self.numbered(r)
    }

    /// `r` with a column numbering its rows 1, 2, .. in its order, which
    /// is then its order: unlike [`Self::position`]'s, never NULL and
    /// never the same for two rows.
    fn numbered(&mut self, r: Rel) -> R<(Rel, String)> {
        let name = self.fresh("pos");
        let w = row_number().order_by(r.sort_exprs()).build()?.alias(&name);
        let plan = LogicalPlanBuilder::from(r.plan.clone()).window(vec![w])?.build()?;
        let ord = vec![Ord { col: name.clone(), desc: false, nulls_first: false }];
        Ok((Rel { plan, ord, ..r }, name))
    }

    /// `l`'s and `r`'s rows taken together, as the operators do
    /// ([`Node::SetOp`]): both sides' columns under one set of names, and
    /// the rows ordered by side, then by their place in it.
    fn set_op(&mut self, op: SetOp, all: bool, l: Rel, r: Rel) -> R<Rel> {
        if l.cols.len() != r.cols.len() {
            return Err(Fail::Err(Error::Query(format!("{}: one side has {} columns and the other {}", op.name(), l.cols.len(), r.cols.len()))));
        }
        let (side, pos) = (self.fresh("side"), self.fresh("pos"));
        let names: Vec<String> = l.cols.iter().map(|_| self.fresh("c")).collect();
        let mut sides = Vec::new();
        for (i, rel) in [l, r].into_iter().enumerate() {
            let p = self.fresh("pos");
            let w = row_number().order_by(rel.sort_exprs()).build()?.alias(&p);
            let plan = LogicalPlanBuilder::from(rel.plan.clone()).window(vec![w])?.build()?;
            sides.push((rel, plan, p, i as i64));
        }
        let (ls, rs) = (sides[0].1.schema().clone(), sides[1].1.schema().clone());
        let mut exprs: [Vec<DExpr>; 2] = [Vec::new(), Vec::new()];
        for (i, n) in names.iter().enumerate() {
            let (a, b) = (col(&sides[0].0.cols[i]), col(&sides[1].0.cols[i]));
            let (ta, tb) = (a.get_type(&ls)?, b.get_type(&rs)?);
            if matches!(ta, DataType::List(_)) && !(op == SetOp::Union && all) {
                return no(format!("{} of a multi-valued column", op.name()));
            }
            let (a, b) = match (&ta, &tb) {
                _ if ta == tb => (a, b),
                (DataType::Null, t) => (cast(a, t.clone()), b),
                (t, DataType::Null) => (a, cast(b, t.clone())),
                _ => return no(format!("{} of {ta} and {tb}", op.name())),
            };
            exprs[0].push(a.alias(n));
            exprs[1].push(b.alias(n));
        }
        let mut plans = Vec::new();
        for ((_, plan, p, i), mut e) in sides.into_iter().zip(exprs) {
            e.push(lit(i).alias(&side));
            e.push(cast(col(&p), DataType::Int64).alias(&pos));
            plans.push(LogicalPlanBuilder::from(plan).project(e)?.build()?);
        }
        let right = plans.pop().expect("two sides");
        let plan = LogicalPlanBuilder::from(plans.pop().expect("two sides")).union(right)?.build()?;
        let names_out = (0..names.len()).map(|i| format!("#{i}")).collect();
        let ord = vec![Ord { col: side.clone(), desc: false, nulls_first: false }, Ord { col: pos.clone(), desc: false, nulls_first: false }];
        let rel = Rel { plan, cols: names.clone(), names: names_out, key: None, score: Score::None, ord };
        let keys: Vec<DExpr> = names.iter().map(|c| col(c)).collect();
        if op == SetOp::Union {
            return match all {
                true => Ok(rel),
                false => self.first_per(rel, keys, 1),
            };
        }
        // Each row's place among the rows equal to it, how many of them
        // the right side has, and how many in all.
        let (nth, theirs, total) = (self.fresh("nth"), self.fresh("theirs"), self.fresh("total"));
        use datafusion::logical_expr::{WindowFrame, WindowFrameBound as WB, WindowFrameUnits as WU};
        let whole = WindowFrame::new_bounds(WU::Rows, WB::Preceding(ScalarValue::UInt64(None)), WB::Following(ScalarValue::UInt64(None)));
        let n = row_number().partition_by(keys.clone()).order_by(rel.sort_exprs()).build()?.alias(&nth);
        let t = over_rows(agg_fn::sum(col(&side))).partition_by(keys.clone()).window_frame(whole.clone()).build()?.alias(&theirs);
        let m = over_rows(agg_fn::count(lit(1i64))).partition_by(keys).window_frame(whole).build()?.alias(&total);
        let (n_, t_) = (cast(col(&nth), DataType::Int64), cast(col(&theirs), DataType::Int64));
        let ours = cast(col(&total), DataType::Int64) - t_.clone();
        let keep = match (op, all) {
            (SetOp::Intersect, true) => n_.lt_eq(t_),
            (SetOp::Intersect, false) => n_.eq(lit(1i64)).and(t_.gt(lit(0i64))),
            (_, true) => (n_ + t_).lt_eq(ours),
            (_, false) => n_.eq(lit(1i64)).and(t_.eq(lit(0i64))),
        };
        let all_cols = rel.all();
        let plan = LogicalPlanBuilder::from(rel.plan.clone())
            .window(vec![n])?
            .window(vec![t, m])?
            .filter(col(&side).eq(lit(0i64)).and(keep))?
            .project(all_cols)?
            .build()?;
        Ok(Rel { plan, ..rel })
    }

    /// The first `per` rows of `r` with equal `keys`, in `r`'s order.
    fn first_per(&mut self, r: Rel, keys: Vec<DExpr>, per: u64) -> R<Rel> {
        let rn = self.fresh("rn");
        let w = row_number().partition_by(keys).order_by(r.sort_exprs()).build()?.alias(&rn);
        let keep = r.all();
        let plan = LogicalPlanBuilder::from(r.plan.clone()).window(vec![w])?.filter(col(&rn).lt_eq(lit(per)))?.project(keep)?.build()?;
        Ok(Rel { plan, ..r })
    }

    /// `r` with a row per value of each multi-valued key of `keys` (its
    /// distinct values, in their order; one row of NULL without any), as
    /// the operators group them: a row is in a group per value, the last
    /// key's values fastest. Each such key's values are a new column (`r`'s
    /// own columns are kept, for the aggregates), whose name the key's
    /// place gives, and its rows are ordered by their row, then by the
    /// value's place in it.
    fn unnest_keys(&mut self, r: Rel, keys: &[Expr]) -> R<(Rel, Vec<Option<String>>)> {
        let schema = r.plan.schema().clone();
        let mut lists = Vec::new();
        for k in keys {
            let e = self.top(k, &r.cols, &schema)?;
            lists.push(match e.get_type(&schema)? {
                DataType::List(f) if f.data_type() != &DataType::Float32 => Some((e, f.data_type().clone())),
                _ => None,
            });
        }
        let mut by = vec![None; keys.len()];
        if lists.iter().all(Option::is_none) {
            return Ok((r, by));
        }
        let (mut r, _) = self.position(r)?;
        let mut kept: Vec<String> = Vec::new();
        for (i, list) in lists.into_iter().enumerate() {
            let Some((e, elem)) = list else { continue };
            let (v, n) = (self.fresh("u"), self.fresh("i"));
            let distinct = ScalarUDF::new_from_impl(ListOf::new(ListOp::Distinct, DataType::List(Arc::new(Field::new("item", elem, true))))).call(vec![e]);
            let mut exprs = r.all();
            exprs.extend(kept.iter().map(|k| col(k)));
            let mut with_places = exprs.clone();
            exprs.push(distinct.alias(&v));
            with_places.push(col(&v));
            with_places.push(ScalarUDF::new_from_impl(ListOf::new(ListOp::Places, DataType::List(Arc::new(Field::new("item", DataType::Int64, true))))).call(vec![col(&v)]).alias(&n));
            use datafusion::common::{NullHandling, UnnestOptions};
            let options = UnnestOptions::new().with_null_handling(NullHandling::PreserveAndExpandEmpty);
            let plan = LogicalPlanBuilder::from(r.plan.clone())
                .project(exprs)?
                .project(with_places)?
                .unnest_columns_with_options(vec![DfColumn::new_unqualified(&v), DfColumn::new_unqualified(&n)], options)?
                .build()?;
            r.ord.push(Ord { col: n, desc: false, nulls_first: false });
            r.plan = plan;
            kept.push(v.clone());
            by[i] = Some(v);
        }
        Ok((r, by))
    }

    fn aggregate(&mut self, r: Rel, keys: &[Expr], aggs: &[Agg]) -> R<Rel> {
        let (r, by) = self.unnest_keys(r, keys)?;
        let (r, pos) = self.position(r)?;
        let schema = r.plan.schema().clone();
        let mut group = Vec::new();
        let mut cols = Vec::new();
        for (k, by) in keys.iter().zip(by) {
            let e = match by {
                Some(v) => col(&v),
                None => self.top(k, &r.cols, &schema)?,
            };
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
            let mut part = |l: &mut Self, e: DExpr| {
                let n = l.fresh("a");
                parts.push(e.alias(&n));
                n
            };
            // A multi-valued argument adds each of its values: each row's
            // count, sum, least or greatest of them, aggregated.
            if let (Some(x), Some(DataType::List(f))) = (&arg, &ty) {
                let each = |op: ListOp, ret: DataType| ScalarUDF::new_from_impl(ListOf::new(op, ret)).call(vec![x.clone()]);
                let elem = f.data_type().clone();
                let int = is_int(&elem);
                let out = match a.func {
                    AggFn::Count => col(&part(self, agg_fn::sum(each(ListOp::Count, DataType::Int64)))),
                    AggFn::Sum if int => col(&part(self, agg_fn::sum(each(ListOp::Sum, DataType::Decimal128(38, 0))))),
                    AggFn::Min if int || is_text(&elem) => col(&part(self, agg_fn::min(each(ListOp::Min, elem.clone())))),
                    AggFn::Max if int || is_text(&elem) => col(&part(self, agg_fn::max(each(ListOp::Max, elem.clone())))),
                    AggFn::Avg if int => {
                        let s = part(self, agg_fn::sum(each(ListOp::Sum, DataType::Decimal128(38, 0))));
                        let c = part(self, agg_fn::sum(each(ListOp::Count, DataType::Int64)));
                        when(col(&c).eq(lit(0i64)), lit(ScalarValue::Float64(None))).otherwise(cast(col(&s), DataType::Float64) / cast(col(&c), DataType::Float64))?
                    }
                    f => return no(format!("aggregate {f:?} of a multi-valued column of {elem}")),
                };
                finals.push(out);
                continue;
            }
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

    /// `r`'s rows with a column per window function after their own, in
    /// `r`'s order. Rows that tie in a function's order take `r`'s order
    /// where the function's value depends on it (a row number, a lag, a
    /// `ROWS` frame's rows), as the operators order them; the functions
    /// whose value would depend on how DataFusion breaks ties (the last
    /// row of a `RANGE` frame's peers but by these patterns) fall back.
    fn window(&mut self, r: Rel, funcs: &[WindowFn]) -> R<Rel> {
        use datafusion::logical_expr::{WindowFrame, WindowFrameBound as WB, WindowFrameUnits as WU};
        let (r, pos) = self.position(r)?;
        let schema = r.plan.schema().clone();
        let unbounded = || ScalarValue::UInt64(None);
        let mut parts: Vec<DExpr> = Vec::new();
        let mut finals: Vec<DExpr> = Vec::new();
        for w in funcs {
            let partition: Vec<DExpr> = w.partition.iter().map(|k| self.top(k, &r.cols, &schema)).collect::<R<_>>()?;
            let mut order: Vec<SortExpr> = Vec::with_capacity(w.order.len() + 1);
            let mut order_types = Vec::new();
            for k in &w.order {
                let e = self.top(&k.expr, &r.cols, &schema)?;
                let t = e.get_type(&schema)?;
                if matches!(t, DataType::List(_)) {
                    return no("a window ordered by a multi-valued column");
                }
                order_types.push(t);
                order.push(e.sort(!k.desc, k.nulls_first));
            }
            // The order with the rows' own as the last key: no ties.
            let mut total = order.clone();
            total.push(col(&pos).sort(true, false));
            let arg = |l: &mut Self, e: &Expr| -> R<(DExpr, DataType)> {
                let x = l.top(e, &r.cols, &schema)?;
                let t = x.get_type(&schema)?;
                if matches!(t, DataType::List(_)) {
                    return no("a window function of a multi-valued column");
                }
                Ok((x, t))
            };
            let mut part = |l: &mut Self, e: DExpr| -> R<DExpr> {
                let n = l.fresh("w");
                parts.push(e.alias(&n));
                Ok(col(&n))
            };
            let whole = WindowFrame::new_bounds(WU::Rows, WB::Preceding(unbounded()), WB::Following(unbounded()));
            let out = match &w.func {
                WindowFunc::RowNumber => cast(part(self, row_number().partition_by(partition).order_by(total).build()?)?, DataType::Int64),
                // Without an order every row is its partition's peer.
                WindowFunc::Rank | WindowFunc::DenseRank if order.is_empty() => lit(1i64),
                WindowFunc::PercentRank if order.is_empty() => lit(0.0f64),
                WindowFunc::CumeDist if order.is_empty() => lit(1.0f64),
                WindowFunc::Rank => cast(part(self, win_fn::rank().partition_by(partition).order_by(order).build()?)?, DataType::Int64),
                WindowFunc::DenseRank => cast(part(self, win_fn::dense_rank().partition_by(partition).order_by(order).build()?)?, DataType::Int64),
                WindowFunc::PercentRank => cast(part(self, win_fn::percent_rank().partition_by(partition).order_by(order).build()?)?, DataType::Float64),
                WindowFunc::CumeDist => cast(part(self, win_fn::cume_dist().partition_by(partition).order_by(order).build()?)?, DataType::Float64),
                WindowFunc::Ntile(0) => return no("NTILE(0)"),
                WindowFunc::Ntile(b) => {
                    // As the operators: buckets as even as can be, the
                    // first ones a row larger.
                    let rn = part(self, row_number().partition_by(partition.clone()).order_by(total).build()?)?;
                    let m = part(self, over_rows(agg_fn::count(lit(1i64))).partition_by(partition).window_frame(whole).build()?)?;
                    let (i, m, b) = (cast(rn, DataType::Int64) - lit(1i64), cast(m, DataType::Int64), lit(*b as i64));
                    let (q, rest) = (m.clone() / b.clone(), m % b);
                    let big = rest.clone() * (q.clone() + lit(1i64));
                    let q1 = when(q.clone().eq(lit(0i64)), lit(1i64)).otherwise(q.clone())?;
                    when(i.clone().lt(big.clone()), i.clone() / (q + lit(1i64)) + lit(1i64)).otherwise((i - big) / q1 + rest + lit(1i64))?
                }
                WindowFunc::Lag { arg: a, offset, default } | WindowFunc::Lead { arg: a, offset, default } => {
                    let (x, t) = arg(self, a)?;
                    let d = match default {
                        None => None,
                        Some(Expr::Lit(v)) => match (scalar(v)?, &t) {
                            (ScalarValue::Null, _) => None,
                            (d, t) if d.data_type() == *t => Some(d),
                            _ => return no("a lag's or lead's default of another type than its value"),
                        },
                        Some(_) => return no("a lag's or lead's default that is not a literal"),
                    };
                    let offset = i64::try_from(*offset).map_err(|_| Fail::No("a lag's offset".into()))?;
                    let f = match w.func {
                        WindowFunc::Lag { .. } => win_fn::lag(x, Some(offset), d),
                        _ => win_fn::lead(x, Some(offset), d),
                    };
                    part(self, f.partition_by(partition).order_by(total).build()?)?
                }
                f @ (WindowFunc::FirstValue(_) | WindowFunc::LastValue(_) | WindowFunc::NthValue { .. } | WindowFunc::Agg(_)) => {
                    let frame = w.frame();
                    // A frame and the order it is taken in.
                    let bound = |b: &Bound, start: bool| -> R<WB> {
                        let off = |e: &Expr| -> R<ScalarValue> {
                            let Expr::Lit(v) = e else { return no("a frame's offset that is not a literal") };
                            Ok(match (frame.units, v, order_types.first()) {
                                (FrameUnits::Rows, Value::Int(k), _) if *k >= 0 => ScalarValue::UInt64(Some(*k as u64)),
                                (FrameUnits::Range, Value::Int(k), Some(t)) if *k >= 0 && is_int(t) => ScalarValue::Int64(Some(*k)),
                                (FrameUnits::Range, Value::Int(_) | Value::Float(_), Some(t)) if is_float(t) && v.as_f64().is_some_and(|x| x >= 0.0) => ScalarValue::Float64(v.as_f64()),
                                _ => return no(format!("a frame's offset {v:?}")),
                            })
                        };
                        Ok(match b {
                            Bound::UnboundedPreceding if start => WB::Preceding(unbounded()),
                            Bound::UnboundedFollowing if !start => WB::Following(unbounded()),
                            Bound::UnboundedPreceding | Bound::UnboundedFollowing => return no("a frame that starts at its partition's end or ends at its start"),
                            Bound::Preceding(e) => WB::Preceding(off(e)?),
                            Bound::CurrentRow => WB::CurrentRow,
                            Bound::Following(e) => WB::Following(off(e)?),
                        })
                    };
                    let units = match frame.units {
                        FrameUnits::Rows => WU::Rows,
                        FrameUnits::Range => WU::Range,
                    };
                    let df_frame = WindowFrame::new_bounds(units, bound(&frame.start, true)?, bound(&frame.end, false)?);
                    let rows = frame.units == FrameUnits::Rows;
                    match f {
                        WindowFunc::Agg(a) => {
                            let x = a.arg.as_ref().map(|e| arg(self, e)).transpose()?;
                            // A RANGE frame holds whole groups of peers,
                            // whatever their order; ROWS count rows.
                            let ord = if rows { total } else { order };
                            let mut over = |l: &mut Self, e: DExpr| -> R<DExpr> { part(l, over_rows(e).partition_by(partition.clone()).order_by(ord.clone()).window_frame(df_frame.clone()).build()?) };
                            match (a.func, x) {
                                (AggFn::Count, None) => over(self, agg_fn::count(lit(1i64)))?,
                                (AggFn::Count, Some((x, _))) => over(self, agg_fn::count(x))?,
                                (AggFn::Min, Some((x, _))) => over(self, agg_fn::min(x))?,
                                (AggFn::Max, Some((x, _))) => over(self, agg_fn::max(x))?,
                                (AggFn::Sum, Some((x, t))) if is_whole(&t) => over(self, agg_fn::sum(cast(x, DataType::Decimal128(38, 0))))?,
                                (AggFn::Sum, Some((x, t))) if is_float(&t) => over(self, agg_fn::sum(x))?,
                                (AggFn::Sum | AggFn::Avg, Some((_, DataType::Null))) => lit(ScalarValue::Float64(None)),
                                (AggFn::Avg, Some((x, t))) if is_whole(&t) => {
                                    let s = over(self, agg_fn::sum(cast(x.clone(), DataType::Decimal128(38, 0))))?;
                                    let c = over(self, agg_fn::count(x))?;
                                    when(c.clone().eq(lit(0i64)), lit(ScalarValue::Float64(None))).otherwise(cast(s, DataType::Float64) / cast(c, DataType::Float64))?
                                }
                                (AggFn::Avg, Some((x, t))) if is_float(&t) => over(self, agg_fn::avg(x))?,
                                (f, x) => return no(format!("a window aggregate {f:?} of {:?}", x.map(|x| x.1))),
                            }
                        }
                        f => {
                            let (a, _) = arg(self, match f {
                                WindowFunc::FirstValue(a) | WindowFunc::LastValue(a) | WindowFunc::NthValue { arg: a, .. } => a,
                                _ => unreachable!("a value function"),
                            })?;
                            let last = matches!(f, WindowFunc::LastValue(_));
                            // NTH_VALUE over RANGE frames from the
                            // partition's first row: the partition's n-th
                            // row in the total order, while the frame
                            // (to the row's last peer) holds n rows.
                            if let (WindowFunc::NthValue { n, .. }, false) = (f, rows) {
                                let n = i64::try_from(*n).map_err(|_| Fail::No("NTH_VALUE's n".into()))?;
                                let v = part(self, win_fn::nth_value(a, n).partition_by(partition.clone()).order_by(total).window_frame(whole.clone()).build()?)?;
                                let out = match (&frame.start, &frame.end) {
                                    (Bound::UnboundedPreceding, Bound::UnboundedFollowing) => v,
                                    (Bound::UnboundedPreceding, Bound::CurrentRow) => {
                                        let to_peers = WindowFrame::new_bounds(WU::Range, WB::Preceding(unbounded()), WB::CurrentRow);
                                        let c = part(self, over_rows(agg_fn::count(lit(1i64))).partition_by(partition).order_by(order).window_frame(to_peers).build()?)?;
                                        when(cast(c, DataType::Int64).gt_eq(lit(n)), v).end()?
                                    }
                                    _ => return no("NTH_VALUE over a RANGE frame that starts after the partition's first row"),
                                };
                                finals.push(out);
                                continue;
                            }
                            // ROWS frames take the rows in the total order;
                            // a RANGE frame's first row or last row, the
                            // partition's or the row's peers', is the
                            // first or last of those in the total order.
                            let (partition, ord, df_frame) = match (rows, last, &frame.start, &frame.end) {
                                (true, _, _, _) => (partition, total, df_frame),
                                (false, false, Bound::UnboundedPreceding, Bound::CurrentRow | Bound::UnboundedFollowing) | (false, true, Bound::UnboundedPreceding | Bound::CurrentRow, Bound::UnboundedFollowing) => {
                                    (partition, total, whole.clone())
                                }
                                (false, false, Bound::CurrentRow, Bound::CurrentRow | Bound::UnboundedFollowing) | (false, true, Bound::UnboundedPreceding | Bound::CurrentRow, Bound::CurrentRow) => {
                                    // Over the row's peers: the partition
                                    // and the order's keys.
                                    let mut peers = partition;
                                    peers.extend(order.iter().map(|o| o.expr.clone()));
                                    (peers, vec![col(&pos).sort(true, false)], whole.clone())
                                }
                                _ => return no("a value function over a RANGE frame of this shape"),
                            };
                            let e = match f {
                                WindowFunc::FirstValue(_) => win_fn::first_value(a),
                                WindowFunc::LastValue(_) => win_fn::last_value(a),
                                WindowFunc::NthValue { n, .. } => win_fn::nth_value(a, i64::try_from(*n).map_err(|_| Fail::No("NTH_VALUE's n".into()))?),
                                _ => unreachable!("a value function"),
                            };
                            part(self, e.partition_by(partition).order_by(ord).window_frame(df_frame).build()?)?
                        }
                    }
                }
            };
            finals.push(out);
        }
        // A Window node per partition and order: DataFusion's physical
        // plan sorts a node's rows once, by its first function's keys, for
        // every function of it.
        let mut groups: Vec<(Vec<DExpr>, Vec<SortExpr>, Vec<DExpr>)> = Vec::new();
        for e in parts {
            let (partition, order) = match &e {
                DExpr::Alias(a) => match a.expr.as_ref() {
                    DExpr::WindowFunction(w) => (w.params.partition_by.clone(), w.params.order_by.clone()),
                    e => return no(format!("not a window function: {e}")),
                },
                e => return no(format!("not a named window function: {e}")),
            };
            match groups.iter_mut().find(|g| g.0 == partition && g.1 == order) {
                Some(g) => g.2.push(e),
                None => groups.push((partition, order, vec![e])),
            }
        }
        let mut plan = r.plan.clone();
        for (_, _, es) in groups {
            plan = LogicalPlanBuilder::from(plan).window(es)?.build()?;
        }
        let mut cols = r.cols.clone();
        let mut names = r.names.clone();
        let mut project = r.all();
        for f in finals {
            let n = self.fresh("c");
            project.push(f.alias(&n));
            names.push(format!("#{}", cols.len()));
            cols.push(n);
        }
        let plan = LogicalPlanBuilder::from(plan).project(project)?.build()?;
        Ok(Rel { plan, cols, names, ..r })
    }

    fn join(&mut self, left: &Node, right: &Node, kind: JoinKind, keys: &[crate::query::ir::JoinKey], on: Option<&Expr>) -> R<Rel> {
        if keys.iter().any(|k| k.left_links || k.right_links) {
            return no("a join along links");
        }
        let mut l = self.node(left, "scan")?;
        let r = if right.is_leaf() {
            // As the operators do: the right side read only for the join
            // values the left holds.
            let (m, held) = self.materialized(l)?;
            l = m;
            let left_rows = held.num_rows();
            // Many values cost the restricted scan more than reading the
            // rows they would cut: the right side is read whole and hashed.
            let whole = left_rows > RESTRICT_ALWAYS
                && plain(right)
                && match self.count(right)? {
                    Some(rows) if (left_rows as u64).saturating_mul(RESTRICT_RATIO) > rows => {
                        let values = self.distinct_keys(&l, &held, keys)?;
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
                let vals = self.key_values(&l, &held, e)?;
                let j = ops::output(&k.right)?;
                let Some(c) = columns.get(j) else { return no(format!("no column {j} in the join's right rows")) };
                let Expr::Field(f) = &c.expr else { return no("a join key of a scanned table that is not one of its fields") };
                let vals = ops::equal_to(Some(schema), Some(*f as usize), vals)?;
                none |= vals.is_empty();
                restrict.push(Expr::In { expr: Box::new(Expr::Field(*f)), list: vals.into_iter().map(Expr::Lit).collect() });
            }
            match none {
                true => {
                    let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
                    let arrays = arrow::columns_of(Vec::new(), names.len())?;
                    self.memory(names, arrays, None, None, 0)?
                }
                false => self.leaf(right, "join", restrict)?,
            }
        } else {
            self.node(right, "join")?
        };
        self.joined(l, r, kind, keys, on)
    }

    /// How many distinct values, NULL aside, the join keys take in the
    /// rows of `l` (summed over the keys), which `held` holds.
    fn distinct_keys(&mut self, l: &Rel, held: &RecordBatch, keys: &[crate::query::ir::JoinKey]) -> R<usize> {
        let mut total = 0;
        for k in keys {
            let e = self.top(&k.left, &l.cols, l.plan.schema())?;
            total += self.key_values(l, held, e)?.len();
        }
        Ok(total)
    }

    /// The distinct values, NULL aside, of `e` over the rows of `l`, which
    /// `held` holds, in the order of the rows they first come in, as the
    /// operators list them: a column's are read from `held`, and a
    /// multi-valued one's are each of its values.
    fn key_values(&mut self, l: &Rel, held: &RecordBatch, e: DExpr) -> R<Vec<Value>> {
        let column = match &e {
            DExpr::Column(c) => held.column_by_name(&c.name).cloned(),
            _ => None,
        };
        let all = match column {
            Some(a) => values_of(&a)?,
            None => {
                let v = self.fresh("v");
                let plan = LogicalPlanBuilder::from(l.plan.clone()).project(vec![e.alias(&v)])?.build()?;
                let mut vals = Vec::new();
                for b in self.collect(plan)? {
                    vals.extend(values_of(b.column(0))?);
                }
                vals
            }
        };
        // A multi-valued column's values are each of its lists'.
        let each = all.into_iter().flat_map(|v| match v {
            Value::List(xs) => xs,
            v => vec![v],
        });
        let mut seen = std::collections::HashSet::new();
        Ok(each.filter(|v| !matches!(v, Value::Null) && seen.insert(ops::part(v))).collect())
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
                return self.joined_lists(l, r, kind, keys, on);
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

    /// [`Self::joined`] on keys of which some are multi-valued, as the
    /// operators join them: a multi-valued key matches on any of its
    /// values, a pair of rows joins once however many values they share,
    /// and a LEFT JOIN's row that matches none (or has no values) keeps
    /// one row of NULLs. Each side's rows are numbered, a multi-valued
    /// key unnested to a row per value, and the pairs joined on the
    /// values kept once, in the left's order, then the right's.
    fn joined_lists(&mut self, l: Rel, r: Rel, kind: JoinKind, keys: &[crate::query::ir::JoinKey], on: Option<&Expr>) -> R<Rel> {
        let both: Vec<String> = l.cols.iter().chain(&r.cols).cloned().collect();
        let names = l.names.iter().chain(&r.names).cloned().collect();
        let score = match &l.score {
            Score::Col(s) => Score::Col(s.clone()),
            _ => Score::Zero,
        };
        let (l, lp) = self.numbered(l)?;
        let (r, rp) = self.numbered(r)?;
        let (l, lk) = self.join_side(l, &lp, keys.iter().map(|k| &k.left), kind == JoinKind::Left)?;
        let (r, rk) = self.join_side(r, &rp, keys.iter().map(|k| &k.right), false)?;
        let mut conds = Vec::new();
        for (a, b) in lk.into_iter().zip(rk) {
            comparable(&a.get_type(l.plan.schema())?, &b.get_type(r.plan.schema())?)?;
            conds.push(a.eq(b));
        }
        let jt = match kind {
            JoinKind::Inner => JoinType::Inner,
            JoinKind::Left => JoinType::Left,
        };
        if let Some(c) = on {
            let probe = LogicalPlanBuilder::from(l.plan.clone()).join_on(r.plan.clone(), JoinType::Inner, conds.clone())?.build()?;
            conds.push(self.top_cond(c, &both, probe.schema())?);
        }
        let plan = LogicalPlanBuilder::from(l.plan.clone()).join_on(r.plan.clone(), jt, conds)?.build()?;
        let ord = vec![Ord { col: lp.clone(), desc: false, nulls_first: false }, Ord { col: rp.clone(), desc: false, nulls_first: false }];
        let mut rel = Rel { plan, cols: both, names, key: None, score, ord };
        if kind == JoinKind::Left {
            // A left row's copies that matched nothing go when another
            // copy matched.
            use datafusion::logical_expr::{WindowFrame, WindowFrameBound as WB, WindowFrameUnits as WU};
            let n = self.fresh("n");
            let whole = WindowFrame::new_bounds(WU::Rows, WB::Preceding(ScalarValue::UInt64(None)), WB::Following(ScalarValue::UInt64(None)));
            let w = over_rows(agg_fn::count(col(&rp))).partition_by(vec![col(&lp)]).window_frame(whole).build()?.alias(&n);
            let keep = rel.all();
            rel.plan = LogicalPlanBuilder::from(rel.plan.clone())
                .window(vec![w])?
                .filter(col(&rp).is_not_null().or(cast(col(&n), DataType::Int64).eq(lit(0i64))))?
                .project(keep)?
                .build()?;
        }
        self.first_per(rel, vec![col(&lp), col(&rp)], 1)
    }

    /// `r` and each of `keys` as an expression over its rows, a
    /// multi-valued key's unnested: a row per value, and for a row
    /// without any (with `keep`) one row whose value is NULL, or none.
    ///
    /// `pos` numbers `r`'s rows, and once they are unnested DataFusion is
    /// not told it orders them: it would take the copies of a row for
    /// rows in order, and a hash join probing them for rows ordered by
    /// `pos` and then by the other side's order, which they are not
    /// (DataFusion 55 then skips the sort the pairs need, and keeps some
    /// twice and drops others).
    fn join_side<'k>(&mut self, mut r: Rel, pos: &str, keys: impl Iterator<Item = &'k Expr>, keep: bool) -> R<(Rel, Vec<DExpr>)> {
        use datafusion::common::{NullHandling, UnnestOptions};
        let schema = r.plan.schema().clone();
        let mut out = Vec::new();
        let mut values: Vec<String> = Vec::new();
        for k in keys {
            let e = self.top(k, &r.cols, &schema)?;
            match e.get_type(&schema)? {
                DataType::List(f) if f.data_type() != &DataType::Float32 => {
                    let u = self.fresh("u");
                    let unordered = |c: DExpr| match &c {
                        DExpr::Column(x) if x.name == pos => ScalarUDF::new_from_impl(Unordered::new()).call(vec![c]).alias(pos),
                        _ => c,
                    };
                    let mut exprs: Vec<DExpr> = r.all().into_iter().map(unordered).collect();
                    exprs.extend(values.iter().map(|v| col(v)));
                    exprs.push(e.alias(&u));
                    let nulls = if keep { NullHandling::PreserveAndExpandEmpty } else { NullHandling::Drop };
                    r.plan = LogicalPlanBuilder::from(r.plan.clone())
                        .project(exprs)?
                        .unnest_columns_with_options(vec![DfColumn::new_unqualified(&u)], UnnestOptions::new().with_null_handling(nulls))?
                        .build()?;
                    values.push(u.clone());
                    out.push(col(&u));
                }
                _ => out.push(e),
            }
        }
        Ok((r, out))
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
    /// A condition on a multi-valued column (a comparison, `IN`, `IS NULL`,
    /// `BETWEEN`, `LIKE`), as Strata's evaluator decides it ([`StrataCond`]):
    /// a list holds when one of its values does, and is never UNKNOWN.
    /// Its operands are lowered, the condition reads them as its columns.
    /// None for a condition of no list.
    fn list_cond(&mut self, e: &Expr, cols: &[String], schema: &DFSchema) -> R<Option<DExpr>> {
        let operands: Vec<&Expr> = match e {
            Expr::Cmp { left, right, .. } => vec![left, right],
            Expr::In { expr, list } => std::iter::once(&**expr).chain(list).collect(),
            Expr::IsNull(x) => vec![x],
            Expr::Between { expr, lo, hi } => std::iter::once(&**expr).chain(lo.as_deref()).chain(hi.as_deref()).collect(),
            Expr::Like { expr, pattern, .. } => vec![expr, pattern],
            _ => return Ok(None),
        };
        // A list here is a column's (or the binder's guard around one), and
        // a column costs nothing to lower: the other operands are lowered
        // once a list is found, so that a condition nested in another is
        // not lowered twice for each level.
        let mut lists = false;
        for o in &operands {
            if let Expr::Output(_) = listy(o).unwrap_or(o) {
                let d = self.expr(listy(o).unwrap_or(o), cols, schema)?;
                lists |= matches!(d.get_type(schema)?, DataType::List(f) if f.data_type() != &DataType::Float32);
            }
        }
        if !lists {
            return Ok(None);
        }
        let mut args = Vec::new();
        let mut slots: Vec<Option<Expr>> = Vec::new();
        for o in &operands {
            // A literal stays in the condition.
            if let Expr::Lit(_) = o {
                slots.push(None);
                continue;
            }
            let inner = listy(o);
            args.push(self.expr(inner.unwrap_or(o), cols, schema)?);
            let at = Expr::Output(args.len() as u32 - 1);
            slots.push(Some(match inner {
                Some(_) => listy_case(at),
                None => at,
            }));
        }
        let mut slots = slots.into_iter();
        let mut next = |o: &Expr| slots.next().flatten().unwrap_or_else(|| o.clone());
        let read = match e {
            Expr::Cmp { op, left, right } => {
                let l = next(left);
                Expr::Cmp { op: *op, left: Box::new(l), right: Box::new(next(right)) }
            }
            Expr::In { expr, list } => {
                let x = next(expr);
                Expr::In { expr: Box::new(x), list: list.iter().map(&mut next).collect() }
            }
            Expr::IsNull(x) => Expr::IsNull(Box::new(next(x))),
            Expr::Between { expr, lo, hi } => {
                let x = next(expr);
                let lo = lo.as_deref().map(&mut next).map(Box::new);
                Expr::Between { expr: Box::new(x), lo, hi: hi.as_deref().map(&mut next).map(Box::new) }
            }
            Expr::Like { expr, pattern, case_insensitive } => {
                let x = next(expr);
                Expr::Like { expr: Box::new(x), pattern: Box::new(next(pattern)), case_insensitive: *case_insensitive }
            }
            _ => unreachable!("a condition of operands"),
        };
        Ok(Some(ScalarUDF::new_from_impl(StrataCond::new(read)).call(args)))
    }

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
        if let Some(c) = self.list_cond(e, cols, schema)? {
            return Ok(c);
        }
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

/// The steps of a DataFusion physical plan's operators, in pre-order:
/// `HashJoinExec` as `hash join`, its line of `EXPLAIN` as the detail, and
/// the rows it gave. DataFusion measures the time an operator spent itself
/// (`elapsed_compute`, which leaves out its inputs', summed over its
/// partitions); a step's time is that and its inputs', which are laid out
/// one after the other from its start (a hash join builds from its first
/// input, then probes with the second), as its CPU time when the query
/// measures CPU. The operators of a plan run at once, each pulling from
/// its inputs, so the times are the plan's work, not when each ran; scaled
/// to fit within the plan's `wall` time, starting at `start`.
fn operator_steps(plan: &Arc<dyn ExecutionPlan>, start: f64, wall: f64, cpu: bool) -> Vec<Step> {
    fn walk(plan: &Arc<dyn ExecutionPlan>, depth: u16, at: f64, out: &mut Vec<Step>) -> f64 {
        let metrics = plan.metrics().map(|m| m.aggregate_by_name());
        let own = metrics.as_ref().and_then(|m| m.elapsed_compute()).map_or(0.0, |ns| ns as f64 / 1e3);
        let line = displayable(plan.as_ref()).one_line().to_string();
        let detail = line.split_once(": ").map_or("", |(_, d)| d).trim();
        let i = out.len();
        out.push(Step {
            op: operator_name(plan.name()),
            detail: steps::short(detail, 200),
            depth,
            start_us: at,
            rows: metrics.as_ref().and_then(|m| m.output_rows()).map(|n| n as u64),
            ..Step::default()
        });
        let mut t = at;
        for c in plan.children() {
            t += walk(c, depth.saturating_add(1), t, out);
        }
        out[i].us = own + (t - at);
        out[i].us
    }
    let mut out = Vec::new();
    let total = walk(plan, 0, 0.0, &mut out);
    let scale = if total > wall && total > 0.0 { wall / total } else { 1.0 };
    for s in &mut out {
        s.cpu_us = cpu.then_some(steps::tenth(s.us));
        s.start_us = start + s.start_us * scale;
        s.us = steps::tenth(s.us * scale);
    }
    out
}

/// `HashJoinExec` as `hash join`, `SortPreservingMergeExec` as `sort
/// preserving merge`.
fn operator_name(name: &str) -> String {
    let name = name.strip_suffix("Exec").unwrap_or(name);
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    out
}

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

/// An aggregate's call as a window function, which takes a partition, an
/// order and a frame ([`ExprFunctionExt`] does not for an aggregate's).
fn over_rows(e: DExpr) -> DExpr {
    match e {
        DExpr::AggregateFunction(a) => DExpr::WindowFunction(Box::new(datafusion::logical_expr::expr::WindowFunction::new(a.func, a.params.args))),
        e => e,
    }
}

/// An INT, or a sum of INTs (a decimal of no fraction).
fn is_whole(t: &DataType) -> bool {
    is_int(t) || matches!(t, DataType::Decimal128(_, 0))
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

/// The column a multi-valued operand reads, when `e` is the binder's
/// `CASE WHEN x IS NULL THEN [] ELSE x END` of it (a NULL list compares as
/// the empty one).
fn listy(e: &Expr) -> Option<&Expr> {
    match e {
        Expr::Case { operand: None, whens, otherwise: Some(o) } => match &whens[..] {
            [When { when: Expr::IsNull(x), then: Expr::Lit(Value::List(v)) }] if v.is_empty() && **x == **o => Some(o),
            _ => None,
        },
        _ => None,
    }
}

/// [`listy`]'s `CASE` of `x`.
fn listy_case(x: Expr) -> Expr {
    Expr::Case { operand: None, whens: vec![When { when: Expr::IsNull(Box::new(x.clone())), then: Expr::Lit(Value::List(Vec::new())) }], otherwise: Some(Box::new(x)) }
}

/// Its argument as it is, of an order DataFusion does not know (see
/// [`Lower::join_side`]).
#[derive(Debug, PartialEq, Eq, Hash)]
struct Unordered {
    signature: Signature,
}

impl Unordered {
    fn new() -> Unordered {
        Unordered { signature: Signature::any(1, Volatility::Immutable) }
    }
}

impl ScalarUDFImpl for Unordered {
    fn name(&self) -> &str {
        "strata_unordered"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, args: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(args[0].clone())
    }

    fn invoke_with_args(&self, mut args: ScalarFunctionArgs) -> datafusion::error::Result<ColumnarValue> {
        Ok(args.args.remove(0))
    }
}

/// What [`ListOf`] computes of each row's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ListOp {
    /// Its distinct values, each where it first comes (as
    /// [`crate::query::agg::Part`]s tell them apart).
    Distinct,
    /// Its values' places, 0, 1, ..
    Places,
    /// How many values it has (0 for NULL).
    Count,
    /// The sum of its INT values, exactly (NULL for none).
    Sum,
    /// Its least value (NULL for none).
    Min,
    /// Its greatest value.
    Max,
}

/// A function of a multi-valued column's list, row by row, its values of
/// type `ret`.
#[derive(Debug, PartialEq, Eq, Hash)]
struct ListOf {
    op: ListOp,
    ret: DataType,
    signature: Signature,
}

impl ListOf {
    fn new(op: ListOp, ret: DataType) -> ListOf {
        ListOf { op, ret, signature: Signature::any(1, Volatility::Immutable) }
    }
}

impl ScalarUDFImpl for ListOf {
    fn name(&self) -> &str {
        match self.op {
            ListOp::Distinct => "strata_list_distinct",
            ListOp::Places => "strata_list_places",
            ListOp::Count => "strata_list_count",
            ListOp::Sum => "strata_list_sum",
            ListOp::Min => "strata_list_min",
            ListOp::Max => "strata_list_max",
        }
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(self.ret.clone())
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> datafusion::error::Result<ColumnarValue> {
        use crate::query::agg::Part;
        let rows = arg_values(&args.args, args.number_rows)?;
        let lists = rows.into_iter().map(|mut r| match r.pop() {
            Some(Value::List(xs)) => xs,
            _ => Vec::new(),
        });
        let values: Vec<Value> = match self.op {
            ListOp::Distinct => lists
                .map(|xs| {
                    let mut seen = std::collections::HashSet::new();
                    Value::List(xs.into_iter().filter(|x| seen.insert(Part::of(x))).collect())
                })
                .collect(),
            ListOp::Places => lists.map(|xs| Value::List((0..xs.len() as i64).map(Value::Int).collect())).collect(),
            ListOp::Count => lists.map(|xs| Value::Int(xs.iter().filter(|x| !matches!(x, Value::Null)).count() as i64)).collect(),
            ListOp::Sum => lists
                .map(|xs| {
                    let ints: Vec<i128> = xs.iter().filter_map(|x| x.as_i64()).map(i128::from).collect();
                    if ints.is_empty() { Value::Null } else { Value::Str(ints.iter().sum::<i128>().to_string()) }
                })
                .collect(),
            ListOp::Min | ListOp::Max => lists
                .map(|xs| {
                    let mut best: Option<Value> = None;
                    for x in xs.into_iter().filter(|x| !matches!(x, Value::Null)) {
                        let better = match &best {
                            None => true,
                            Some(b) => {
                                let o = crate::query::exec::cmp_values(&x, b);
                                if self.op == ListOp::Min { o.is_lt() } else { o.is_gt() }
                            }
                        };
                        if better {
                            best = Some(x);
                        }
                    }
                    best.unwrap_or(Value::Null)
                })
                .collect(),
        };
        let out: ArrayRef = match (&self.ret, self.op) {
            (DataType::Decimal128(p, sc), ListOp::Sum) => {
                let a: datafusion::arrow::array::Decimal128Array = values.iter().map(|v| match v {
                    Value::Str(s) => s.parse::<i128>().ok(),
                    _ => None,
                }).collect();
                Arc::new(a.with_precision_and_scale(*p, *sc).map_err(DataFusionError::from)?)
            }
            (t, _) => {
                let a = arrow::column_of(values).map_err(|e| DataFusionError::Execution(format!("strata: {}", e.0)))?;
                datafusion::arrow::compute::cast(&a, t)?
            }
        };
        Ok(ColumnarValue::Array(out))
    }
}

/// A condition as Strata's evaluator decides it, over its arguments as
/// the columns `Expr::Output(i)` it reads: TRUE, FALSE or NULL (UNKNOWN).
#[derive(Debug)]
struct StrataCond {
    /// The condition, written out: what tells two apart.
    key: String,
    cond: Expr,
    signature: Signature,
}

impl StrataCond {
    fn new(cond: Expr) -> StrataCond {
        StrataCond { key: format!("{cond:?}"), cond, signature: Signature::variadic_any(Volatility::Immutable) }
    }
}

impl PartialEq for StrataCond {
    fn eq(&self, o: &Self) -> bool {
        self.key == o.key
    }
}

impl Eq for StrataCond {}

impl std::hash::Hash for StrataCond {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.key.hash(h);
    }
}

impl ScalarUDFImpl for StrataCond {
    fn name(&self) -> &str {
        "strata_cond"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _: &[DataType]) -> datafusion::error::Result<DataType> {
        Ok(DataType::Boolean)
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> datafusion::error::Result<ColumnarValue> {
        let rows = arg_values(&args.args, args.number_rows)?;
        let mut b = BooleanBuilder::with_capacity(rows.len());
        for r in &rows {
            let read = |x: crate::query::ir::eval::Ref| match x {
                crate::query::ir::eval::Ref::Output(i) => Ok(r.get(i as usize).cloned().unwrap_or(Value::Null)),
                x => Err(Error::Query(format!("{x:?} is not a column of a condition's arguments"))),
            };
            match crate::query::ir::eval::eval(&self.cond, &read).map_err(|e| DataFusionError::Execution(format!("strata: {e}")))? {
                Value::Bool(x) => b.append_value(x),
                Value::Null => b.append_null(),
                v => return Err(DataFusionError::Execution(format!("strata: a condition gave {v:?}"))),
            }
        }
        Ok(ColumnarValue::Array(Arc::new(b.finish())))
    }
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
