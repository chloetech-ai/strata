//! Operators: what runs a plan ([`crate::query::ir`]) beyond the shapes the
//! executor answers. Each leaf (`Project [Limit] [Sort] [Filter]
//! [Aggregate] Search`, [`Node::is_leaf`]) goes to a [`Runner`] (the
//! executor, or a coordinator over its shards) once the subqueries in it
//! have run; joins, `WITH`, `DISTINCT`, and the nodes over their rows run
//! here, in memory, pulling their inputs' rows.
//!
//! A subquery ([`Expr::Values`], [`Expr::Keys`], [`Expr::Exists`],
//! [`Expr::Scalar`]) runs before the node it sits in, and its result takes
//! its place as a literal, so every index still serves a leaf's filter. A
//! join runs its left side, then its right side restricted to the join
//! values the left holds (`b.id IN (...)`, or the rows linking to them for
//! an EDGES field), and joins them by hashing the right side. A LIMIT over
//! joins whose first table's scan comes in the order asked for scans a
//! growing number of that table's rows until enough rows join.
//!
//! The query's deadline and cancel flag are checked every 1,024 rows, and
//! the rows the operators hold count against its memory budget.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::error::{Error, Result};
use crate::query::agg::{self, Acc, Part};
use crate::query::compound::MAX_ROWS;
use crate::query::exec::{ExecOptions, ExecStats, Leaf, ResultSet};
use crate::query::ir::eval::{self, Ref};
use crate::query::ir::{Agg, CmpOp, Expr, Graph, GraphKind, JoinKey, JoinKind, KeyValue, Node, Plan, SetOp, SortKey};
use crate::query::logical::Direction;
use crate::query::memory::Charge;
use crate::query::steps::{self, Clock, Step};
use crate::query::Projection;
use crate::schema::{FieldType, Schema};
use crate::value::Value;

/// Runs one leaf of a plan.
pub type Runner<'r> = dyn FnMut(&Plan) -> Result<ResultSet> + 'r;

/// Runs a leaf plan for [`run_on_engine`]: its rows, as Arrow columns
/// ([`Leaf`]) when there are more than the number it is given, which is
/// `usize::MAX` for the operators.
pub type LeafRunner<'r> = dyn FnMut(&Plan, usize) -> Result<Leaf> + 'r;

/// Runs `plan` (one that reads, [`Node::uses_operators`]) on the engine
/// `opts.engine` chooses ([`crate::query::exec::Engine`]): DataFusion
/// above its leaves where it lowers the plan (feature `datafusion`), and
/// otherwise the operators ([`run_in`]), which take the leaves DataFusion
/// already ran rather than run them again; the plan's first line then
/// says why DataFusion did not answer. A database runs its compound
/// queries here over its own tables, and a coordinator over its shards'
/// answers to each leaf.
pub fn run_on_engine(plan: &Plan, catalog: &[&Schema], runner: &mut LeafRunner, opts: &ExecOptions) -> Result<ResultSet> {
    #[allow(unused_mut)]
    let mut fallback: Option<String> = None;
    #[cfg(feature = "datafusion")]
    let mut ran = crate::query::df::Ran::default();
    #[cfg(feature = "datafusion")]
    // A leaf with subqueries has no operators for DataFusion to run.
    if opts.engine != crate::query::exec::Engine::Operators && !plan.root.is_leaf() {
        use crate::query::df::Outcome;
        match crate::query::df::run_in(plan, catalog, runner, opts)? {
            Outcome::Done(rs) => return Ok(rs),
            Outcome::Fallback(why, _) if opts.engine == crate::query::exec::Engine::DataFusionOnly => return Err(err(format!("not run on DataFusion: {why}"))),
            Outcome::Fallback(why, r) => {
                fallback = Some(why);
                ran = r;
            }
            Outcome::Small(r) => ran = r,
        }
    }
    // The leaves DataFusion ran are not run again, and the profile keeps
    // their steps.
    #[cfg(feature = "datafusion")]
    let ran_steps = std::mem::take(&mut ran.steps);
    let mut rows = |leaf: &Plan| {
        #[cfg(feature = "datafusion")]
        if let Some(rs) = ran.take(leaf) {
            return Ok(rs);
        }
        match runner(leaf, usize::MAX)? {
            Leaf::Rows(rs) => Ok(rs),
            #[cfg(feature = "datafusion")]
            Leaf::Arrow(_) => Err(err("a leaf gave Arrow columns where rows were asked for")),
        }
    };
    #[allow(unused_mut)]
    let mut rs = run_in(plan, catalog, &mut rows, opts)?;
    #[cfg(feature = "datafusion")]
    if !ran_steps.is_empty() {
        let mut steps = ran_steps;
        rs.stats.steps_dropped += crate::query::steps::nest(&mut steps, &rs.stats.steps, 0) as u64;
        rs.stats.steps = steps;
    }
    if let Some(why) = fallback {
        rs.stats.plan.insert(0, format!("operators (not datafusion: {why})"));
    }
    Ok(rs)
}

/// Rows an ORDER BY with a correlated subquery ranks at most: each row may
/// run the subquery once.
pub const CORRELATED_ROWS: usize = 10_000;

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// Runs a plan that reads `schema`'s table: its leaves with `runner`, the
/// rest here, under `opts`' deadline, cancel flag and memory budget.
pub fn run(plan: &Plan, schema: &Schema, runner: &mut Runner, opts: &ExecOptions) -> Result<ResultSet> {
    run_in(plan, &[schema], runner, opts)
}

/// [`run`] of a plan whose leaves read any tables of `catalog`.
pub fn run_in(plan: &Plan, catalog: &[&Schema], runner: &mut Runner, opts: &ExecOptions) -> Result<ResultSet> {
    let mut ops = Ops { catalog, runner, opts, charge: Charge::new(opts.memory.clone()), steps: 0, ctes: Vec::new(), driver: None, per: None, plan: Vec::new(), encoded: Vec::new(), clock: Clock::new(opts.profile), prof: Vec::new(), dropped: 0, filter_docs: 0, scored: 0 };
    let rows = ops.node(&plan.root, "query")?;
    let keys = rows.keys.filter(|k| k.iter().all(|k| !matches!(k, Value::Null)));
    Ok(ResultSet {
        columns: rows.columns,
        rows: rows.rows,
        keys: keys.unwrap_or_default(),
        scores: rows.scores.unwrap_or_default(),
        parts: None,
        stats: ExecStats { plan: ops.plan, filter_docs: ops.filter_docs, scored: ops.scored, steps: ops.prof, steps_dropped: ops.dropped, ..ExecStats::default() },
    })
}

/// Rows a node gives.
#[derive(Clone, Debug, Default)]
struct Rows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    /// Each row's primary key, when the rows are rows of the table.
    keys: Option<Vec<Value>>,
    /// Each row's score, from the scan that ranked it.
    scores: Option<Vec<f64>>,
}

impl Rows {
    /// The rows at `idx`, in that order.
    fn pick(self, idx: &[usize]) -> Rows {
        Rows {
            rows: idx.iter().map(|&i| self.rows[i].clone()).collect(),
            keys: self.keys.map(|k| idx.iter().map(|&i| k[i].clone()).collect()),
            scores: self.scores.map(|s| idx.iter().map(|&i| s[i]).collect()),
            columns: self.columns,
        }
    }
}

struct Ops<'a, 'r> {
    /// The tables leaves read.
    catalog: &'a [&'a Schema],
    runner: &'a mut Runner<'r>,
    opts: &'a ExecOptions,
    /// What the query holds.
    charge: Charge,
    /// Rows handled since the deadline was last checked.
    steps: u32,
    /// The `WITH` results in scope, the innermost last.
    ctes: Vec<(String, Rc<Rows>)>,
    /// The scan a LIMIT over joins grows: the leaf, the rows it may read,
    /// and how many it read.
    driver: Option<(*const Node, u64, usize)>,
    /// The cap a `MAX k PER` right above the driven scan puts on its rows,
    /// which the scan may keep itself ([`super::ir::Search::per`]).
    per: Option<(*const Node, super::ir::PerCap)>,
    plan: Vec<String>,
    /// The vectors of the `encode(..)`s run so far, with the `WITH`
    /// results in scope then: a column and the ORDER BY that names it
    /// run it once.
    encoded: Vec<(Expr, usize, Option<std::sync::Arc<[f32]>>)>,
    /// The operators' steps, each leaf's executor steps inside its own
    /// ([`steps`]), and those the cap dropped.
    clock: Clock,
    prof: Vec<Step>,
    dropped: u64,
    /// The leaves' rows passing their filters, and scored, summed.
    filter_docs: u64,
    scored: u64,
}

/// What an operator's step is called, and what it works on.
fn label(n: &Node) -> (&'static str, String) {
    match n {
        Node::Join { kind, keys, .. } => ("join", format!("{} on {} key{}", if *kind == JoinKind::Left { "left" } else { "inner" }, keys.len(), if keys.len() == 1 { "" } else { "s" })),
        Node::With { ctes, .. } => ("with", ctes.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")),
        Node::Limit { offset, limit, .. } => ("limit", match limit {
            Some(l) => format!("{l} after {offset}"),
            None => format!("after {offset}"),
        }),
        Node::Sort { keys, .. } => ("sort", format!("{} key{}", keys.len(), if keys.len() == 1 { "" } else { "s" })),
        Node::Filter { .. } => ("filter", String::new()),
        Node::Project { columns, .. } => ("project", format!("{} column{}", columns.len(), if columns.len() == 1 { "" } else { "s" })),
        Node::Distinct { .. } => ("distinct", String::new()),
        Node::DistinctOn { per, .. } => ("distinct on", format!("{per} per value")),
        Node::KeyOrder { .. } => ("key order", String::new()),
        Node::SetOp { op, all, .. } => ("set operation", format!("{}{}", op.name(), if *all { " ALL" } else { "" })),
        Node::Window { funcs, .. } => ("window", funcs.iter().map(|w| w.func.name()).collect::<Vec<_>>().join(", ")),
        Node::Aggregate { keys, aggs, .. } => ("aggregate", format!("{} key{}, {} aggregate{}", keys.len(), if keys.len() == 1 { "" } else { "s" }, aggs.len(), if aggs.len() == 1 { "" } else { "s" })),
        _ => ("node", String::new()),
    }
}

impl<'a> Ops<'a, '_> {
    /// The table a leaf reads.
    fn table(&self, leaf: &Node) -> Result<&'a Schema> {
        let name = leaf.search().map(|s| s.table.as_str()).unwrap_or_default();
        self.catalog.iter().find(|s| s.table.eq_ignore_ascii_case(name)).copied().ok_or_else(|| err(format!("no such table: {name}")))
    }

    /// [`ExecOptions::check`] every 1,024 rows.
    fn tick(&mut self) -> Result<()> {
        self.steps += 1;
        if self.steps < 1024 {
            return Ok(());
        }
        self.steps = 0;
        self.opts.check()
    }

    /// The rows of `n`, its run a step ([`steps`]).
    fn node(&mut self, n: &Node, what: &str) -> Result<Rows> {
        if n.is_leaf() {
            return self.leaf(n, what, Vec::new());
        }
        if matches!(n, Node::Ref { .. }) {
            return self.node_untimed(n, what);
        }
        let m = self.clock.mark(&self.prof);
        let rows = self.node_untimed(n, what)?;
        let (op, detail) = label(n);
        if self.clock.wrap(&mut self.prof, m, op, detail, Some(rows.rows.len() as u64)).is_none() {
            self.dropped += 1;
        }
        Ok(rows)
    }

    fn node_untimed(&mut self, n: &Node, what: &str) -> Result<Rows> {
        match n {
            Node::Join { .. } => self.join(n),
            Node::With { ctes, input } => {
                let mark = self.ctes.len();
                let r = (|| {
                    for c in ctes {
                        let rows = self.node(&c.input, &format!("WITH {}", c.name))?;
                        if rows.rows.len() > MAX_ROWS {
                            return Err(err(format!("WITH {} holds more than {MAX_ROWS} rows", c.name)));
                        }
                        self.plan.push(format!("WITH {}: {} rows", c.name, rows.rows.len()));
                        self.ctes.push((c.name.clone(), Rc::new(rows)));
                    }
                    self.node(input, what)
                })();
                self.ctes.truncate(mark);
                r
            }
            Node::Ref { name } => match self.ctes.iter().rev().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
                Some((_, rows)) => Ok((**rows).clone()),
                None => Err(err(format!("no WITH result named {name}"))),
            },
            Node::Limit { input, offset, limit } => self.limit(input, *offset as usize, limit.map(|l| l as usize), what),
            Node::Sort { input, keys } => {
                let rows = self.node(input, what)?;
                let keys: Vec<SortKey> = keys.iter().map(|k| Ok(SortKey { expr: self.resolved(&k.expr)?, ..k.clone() })).collect::<Result<_>>()?;
                let mut vals = Vec::with_capacity(rows.rows.len());
                for r in &rows.rows {
                    self.tick()?;
                    vals.push(keys.iter().map(|k| value(&k.expr, r)).collect::<Result<Vec<_>>>()?);
                }
                let mut idx: Vec<usize> = (0..rows.rows.len()).collect();
                idx.sort_by(|&a, &b| keys.iter().enumerate().map(|(i, k)| compare(&vals[a][i], &vals[b][i], k)).find(|o| o.is_ne()).unwrap_or(Ordering::Equal));
                Ok(rows.pick(&idx))
            }
            Node::Filter { input, cond } => {
                let rows = self.node(input, what)?;
                let cond = self.resolved(cond)?;
                let mut keep = Vec::new();
                for (i, r) in rows.rows.iter().enumerate() {
                    self.tick()?;
                    if eval::holds(&cond, &reader(r))? {
                        keep.push(i);
                    }
                }
                Ok(rows.pick(&keep))
            }
            Node::Project { input, columns } => {
                let rows = self.node(input, what)?;
                let exprs: Vec<Expr> = columns.iter().map(|c| self.resolved(&c.expr)).collect::<Result<_>>()?;
                let mut out = Vec::with_capacity(rows.rows.len());
                for r in &rows.rows {
                    self.tick()?;
                    let row: Vec<Value> = exprs.iter().map(|e| value(e, r)).collect::<Result<_>>()?;
                    self.charge.row(&row)?;
                    out.push(row);
                }
                Ok(Rows { columns: columns.iter().map(|c| c.name.clone()).collect(), rows: out, keys: rows.keys, scores: rows.scores })
            }
            Node::Distinct { input } => {
                let rows = self.node(input, what)?;
                let mut seen = HashSet::new();
                let keep: Vec<usize> = (0..rows.rows.len()).filter(|&i| seen.insert(rows.rows[i].iter().map(|v| format!("{v:?}")).collect::<Vec<_>>())).collect();
                Ok(rows.pick(&keep))
            }
            Node::DistinctOn { input, keys, per } => {
                let rows = self.node(input, what)?;
                let keys: Vec<Expr> = keys.iter().map(|k| self.resolved(k)).collect::<Result<_>>()?;
                let mut seen: HashMap<Vec<String>, u64> = HashMap::new();
                let mut keep = Vec::new();
                for (i, r) in rows.rows.iter().enumerate() {
                    self.tick()?;
                    let group = keys.iter().map(|k| value(k, r).map(|v| format!("{v:?}"))).collect::<Result<Vec<_>>>()?;
                    let n = seen.entry(group).or_default();
                    if *n < *per {
                        *n += 1;
                        keep.push(i);
                    }
                }
                Ok(rows.pick(&keep))
            }
            Node::KeyOrder { input, by } => {
                let rows = self.node(input, what)?;
                let by = self.node(by, what)?;
                let idx = match (&rows.keys, &by.keys) {
                    (Some(got), Some(order)) => {
                        let at: HashMap<Part, usize> = order.iter().enumerate().map(|(i, k)| (part(k), i)).collect();
                        let mut idx: Vec<usize> = (0..rows.rows.len()).collect();
                        idx.sort_by_key(|&i| at.get(&part(&got[i])).copied().unwrap_or(usize::MAX));
                        idx
                    }
                    _ => return Ok(rows),
                };
                Ok(rows.pick(&idx))
            }
            Node::Aggregate { input, keys, aggs } => self.aggregate(input, keys, aggs, what),
            Node::SetOp { op, all, left, right } => {
                let l = self.node(left, what)?;
                let r = self.node(right, what)?;
                if l.columns.len() != r.columns.len() {
                    return Err(err(format!("{}: one side has {} columns and the other {}", op.name(), l.columns.len(), r.columns.len())));
                }
                let rows = match op {
                    SetOp::Union => {
                        let mut rows = l.rows;
                        rows.extend(r.rows);
                        if !*all {
                            let mut seen = HashSet::new();
                            rows.retain(|row| seen.insert(set_key(row)));
                        }
                        rows
                    }
                    SetOp::Intersect | SetOp::Except => {
                        let mut right: HashMap<Vec<String>, u64> = HashMap::new();
                        for row in &r.rows {
                            self.tick()?;
                            *right.entry(set_key(row)).or_default() += 1;
                        }
                        let keys: Vec<Vec<String>> = l.rows.iter().map(|row| set_key(row)).collect();
                        let mut left: HashMap<&[String], u64> = HashMap::new();
                        if *op == SetOp::Except && *all {
                            for k in &keys {
                                *left.entry(k.as_slice()).or_default() += 1;
                            }
                        }
                        // Each row's place among the rows of `left` equal to it.
                        let mut nth: HashMap<&[String], u64> = HashMap::new();
                        let mut keep = Vec::with_capacity(keys.len());
                        for k in &keys {
                            self.tick()?;
                            let n = nth.entry(k.as_slice()).or_default();
                            *n += 1;
                            let theirs = right.get(k).copied().unwrap_or(0);
                            let kept = match (op, all) {
                                (SetOp::Intersect, true) => *n <= theirs,
                                (SetOp::Intersect, false) => *n == 1 && theirs > 0,
                                (_, true) => *n + theirs <= left[k.as_slice()],
                                (_, false) => *n == 1 && theirs == 0,
                            };
                            keep.push(kept);
                        }
                        l.rows.into_iter().zip(keep).filter_map(|(row, k)| k.then_some(row)).collect()
                    }
                };
                Ok(Rows { columns: l.columns, rows, keys: None, scores: None })
            }
            Node::Window { input, funcs } => {
                let mut rows = self.node(input, what)?;
                let mut funcs = funcs.clone();
                for x in funcs.iter_mut().flat_map(super::ir::WindowFn::exprs_mut) {
                    *x = self.resolved(x)?;
                }
                let opts = self.opts;
                let mut steps = self.steps;
                let mut tick = || {
                    steps += 1;
                    if steps < 1024 {
                        return Ok(());
                    }
                    steps = 0;
                    opts.check()
                };
                let cols = super::window::evaluate(&rows.rows, &funcs, &mut tick)?;
                self.steps = steps;
                let w = rows.columns.len();
                for (i, r) in rows.rows.iter_mut().enumerate() {
                    let added: Vec<Value> = cols.iter().map(|c| c[i].clone()).collect();
                    self.charge.row(&added)?;
                    r.extend(added);
                }
                rows.columns.extend((0..funcs.len()).map(|i| format!("#{}", w + i)));
                Ok(rows)
            }
            Node::Search(_) => Err(err("a Search runs under a Project, which names its columns")),
            Node::Insert { .. } | Node::Update { .. } | Node::Delete { .. } => Err(err("a write runs on its own, not inside a query's plan")),
        }
    }

    /// Runs a leaf with the executor, once its subqueries ran, with
    /// `restrict` added to its Search's filter.
    fn leaf(&mut self, n: &Node, what: &str, restrict: Vec<(Expr, usize)>) -> Result<Rows> {
        let m = self.clock.mark(&self.prof);
        let rows = self.leaf_untimed(n, what, restrict)?;
        let table = n.search().map(|s| s.table.as_str()).unwrap_or_default();
        if self.clock.wrap(&mut self.prof, m, "leaf", format!("{what}: {table}"), Some(rows.rows.len() as u64)).is_none() {
            self.dropped += 1;
        }
        Ok(rows)
    }

    fn leaf_untimed(&mut self, n: &Node, what: &str, restrict: Vec<(Expr, usize)>) -> Result<Rows> {
        self.opts.check()?;
        let schema = self.table(n)?;
        let mut leaf = n.clone();
        leaf.leaf_exprs_mut(&mut |e| e.visit_mut(&mut |x| self.resolve(x, Some(schema))))?;
        // A subquery's NULL (no row, or a NULL among IN's values) compares
        // as SQL reads it, not as an error.
        unknown(&mut leaf, &self.catalog);
        let mut restricted = 0;
        if !restrict.is_empty() {
            let s = leaf.search_mut().expect("a leaf");
            let mut conds: Vec<Expr> = restrict.into_iter().map(|(c, n)| {
                restricted = restricted.max(n);
                c
            }).collect();
            match std::mem::replace(&mut s.filter, Expr::Lit(Value::Bool(true))) {
                Expr::Lit(Value::Bool(true)) => {}
                Expr::And(xs) => conds.extend(xs),
                f => conds.push(f),
            }
            s.filter = if conds.len() == 1 { conds.pop().expect("a condition") } else { Expr::And(conds) };
        }
        // The scan a LIMIT over joins grows.
        let driver = self.driver.filter(|(p, ..)| std::ptr::eq(*p, n)).map(|(_, cap, _)| cap);
        if let Some(cap) = driver {
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
        let mut correlated = false;
        leaf.leaf_exprs(&mut |e| correlated |= e.has_correlated());
        let rs = match correlated {
            true => self.correlated(leaf, schema)?,
            false => (self.runner)(&Plan::new(leaf))?,
        };
        if let Some((_, _, read)) = self.driver.as_mut().filter(|(p, ..)| std::ptr::eq(*p, n)) {
            *read = rs.rows.len();
        }
        // The leaf as the plan has it: a subquery's values, or a join's,
        // would make the line as long as they are.
        let text: Vec<String> = Plan::new(n.clone()).explain(self.catalog).iter().map(|l| l.trim().to_string()).collect();
        let mut line = format!("{what}: {} rows: {}", rs.rows.len(), text.join(" / "));
        if restricted > 0 {
            line += &format!(" (restricted to {restricted} join values)");
        }
        self.plan.push(line);
        self.plan.extend(rs.stats.plan.iter().map(|l| format!("  {l}")));
        self.filter_docs += rs.stats.filter_docs;
        self.scored += rs.stats.scored;
        self.dropped += steps::nest(&mut self.prof, &rs.stats.steps, 0) as u64 + rs.stats.steps_dropped;
        for r in &rs.rows {
            self.charge.row(r)?;
        }
        let n = rs.rows.len();
        Ok(Rows {
            keys: (rs.keys.len() == n).then_some(rs.keys),
            scores: (rs.scores.len() == n).then_some(rs.scores),
            columns: rs.columns,
            rows: rs.rows,
        })
    }

    /// Runs a leaf whose ranking holds correlated subqueries
    /// ([`Expr::is_correlated`]), each once for each distinct value of the
    /// outer fields it names among the rows it is computed for:
    ///
    /// 1. **Rows.** The rows the first phase that holds one reranks (the
    ///    phases before it run as the leaf has them); every row that passes
    ///    the filter, at most [`CORRELATED_ROWS`], when the ORDER BY holds
    ///    one; or, when only columns do, the leaf's own rows.
    /// 2. **Values.** Each subquery's value for each distinct value of
    ///    the fields it names among those rows, a number or NULL.
    /// 3. **Ranking.** The leaf again over those rows, each subquery as
    ///    its value for each row by key ([`Expr::ByKey`]), from that phase
    ///    on (the rows ranked by where step 1 left them), or from the
    ///    start; with columns only, step 1's rows take the values.
    ///
    /// A coordinator's shards answer steps 1 and 3 as any leaf, and the
    /// subqueries run here, so they may read any table.
    fn correlated(&mut self, leaf: Node, schema: &Schema) -> Result<ResultSet> {
        let pk = schema.primary_key.ok_or_else(|| err("a correlated subquery takes a value for each row of a table with a primary key"))?;
        let s = leaf.search().expect("a leaf").clone();
        let Node::Project { columns, input } = &leaf else { unreachable!("a leaf") };
        let mut elsewhere = s.filter.has_correlated();
        input.leaf_exprs(&mut |e| elsewhere |= e.has_correlated());
        let in_order = s.order.iter().any(|k| k.expr.has_correlated());
        let phase = s.phases.iter().position(|p| p.expr.has_correlated());
        let in_columns: Vec<bool> = columns.iter().map(|c| c.expr.has_correlated()).collect();
        if elsewhere && !(in_order || phase.is_some()) || s.filter.has_correlated() {
            return Err(err("a subquery that names the outer row computes a number of each row it ranks: it works in ORDER BY, rerank phases and the columns of ranked rows, not in WHERE"));
        }
        // The subqueries, each once, and the outer fields each names.
        let mut subs: Vec<(Expr, Vec<u32>)> = Vec::new();
        leaf.leaf_exprs(&mut |e| {
            e.visit(&mut |x| {
                if x.is_correlated() && !subs.iter().any(|(y, _)| y == x) {
                    let Expr::Scalar(n) = x else { unreachable!("a subquery") };
                    let mut fields = Vec::new();
                    n.visit_exprs(&mut |o| {
                        if let Expr::Outer(f) = o {
                            if !fields.contains(f) {
                                fields.push(*f);
                            }
                        }
                    });
                    subs.push((x.clone(), fields));
                }
            })
        });
        let mut outer: Vec<u32> = subs.iter().flat_map(|(_, fs)| fs.iter().copied()).collect();
        if !in_order && phase.is_none() {
            // A column computed from a subquery's value reads its row's
            // fields too.
            for c in columns.iter().filter(|c| c.expr.has_correlated()) {
                c.expr.visit(&mut |x| {
                    if let Expr::Field(f) = x {
                        outer.push(*f);
                    }
                });
            }
        }
        outer.sort_unstable();
        outer.dedup();
        let outer_columns = |c: &mut Vec<super::ir::Column>| c.extend(outer.iter().map(|f| super::ir::Column { name: format!("__outer{f}"), expr: Expr::Field(*f) }));

        // 1. The rows.
        let search_of = |order: Vec<SortKey>, phases: Vec<super::ir::Phase>| Node::Search(super::ir::Search { order, phases, profile: None, ..s.clone() });
        let (first, cut) = match (in_order, phase) {
            (true, _) => (search_of(Vec::new(), Vec::new()), CORRELATED_ROWS as u64 + 1),
            (false, Some(j)) => (search_of(s.order.clone(), s.phases[..j].to_vec()), s.phases[j].count),
            (false, None) => (Node::Ref { name: String::new() }, 0),
        };
        let (rows_of, only_columns) = match (in_order, phase) {
            (false, None) => {
                // The leaf itself, each such column the key until it has its values.
                let mut l = leaf.clone();
                let Node::Project { columns, .. } = &mut l else { unreachable!("a leaf") };
                for (c, &sub) in columns.iter_mut().zip(&in_columns) {
                    if sub {
                        c.expr = Expr::Field(pk as u32);
                    }
                }
                outer_columns(columns);
                let rs = (self.runner)(&Plan::new(l))?;
                (rs, true)
            }
            _ => {
                let mut cols = Vec::new();
                outer_columns(&mut cols);
                let l = Node::Project { input: Box::new(Node::Limit { input: Box::new(first), offset: 0, limit: Some(cut) }), columns: cols };
                ((self.runner)(&Plan::new(l))?, false)
            }
        };
        let n = rows_of.rows.len();
        if in_order && n > CORRELATED_ROWS {
            return Err(err(format!(
                "an ORDER BY with a subquery that names the outer row ranks at most {CORRELATED_ROWS} rows, and this one reaches more: rank its top rows in a rerank phase (WITH top AS (... LIMIT n) SELECT ... FROM top ORDER BY ...)"
            )));
        }
        if rows_of.keys.len() != n {
            return Err(err("a correlated subquery takes a value for each row by its key, and the rows came without keys"));
        }
        let at = rows_of.columns.len() - outer.len();

        // 2. Each subquery's values.
        let mark = self.ctes.len();
        let (mut distinct, mut runs) = (0, 0);
        let values = subs
            .iter()
            .map(|(sub, fields)| {
                let tuples: Vec<Vec<Value>> = rows_of.rows.iter().map(|row| fields.iter().map(|f| row[at + outer.binary_search(f).expect("an outer field")].clone()).collect()).collect();
                self.sub_values(sub, fields, &tuples, &mut distinct, &mut runs)
            })
            .collect::<Result<Vec<_>>>();
        self.ctes.truncate(mark);
        let values: Vec<Vec<KeyValue>> = values?.into_iter().map(|vs| vs.into_iter().zip(&rows_of.keys).map(|(value, key)| KeyValue { key: key.clone(), value }).collect()).collect();
        self.plan.push(format!("correlated: {} subqueries over {n} rows, {distinct} values, {runs} runs", subs.len()));
        let by_key = |e: &mut Expr| {
            e.visit_mut(&mut |x| {
                if let Some(i) = subs.iter().position(|(y, _)| y == x) {
                    *x = Expr::ByKey(values[i].clone());
                }
                Ok(())
            })
        };

        // 3. The ranking, or step 1's rows with their values.
        if only_columns {
            let mut rs = rows_of;
            for (i, c) in columns.iter().enumerate().filter(|(i, _)| in_columns[*i]) {
                for (r, row) in rs.rows.iter_mut().enumerate() {
                    let mut e = c.expr.clone();
                    e.visit_mut(&mut |x| {
                        if let Some(j) = subs.iter().position(|(y, _)| y == x) {
                            *x = Expr::Lit(values[j][r].value.map_or(Value::Null, Value::Float));
                        }
                        Ok(())
                    })?;
                    let fields = |f: u32| outer.binary_search(&f).map(|o| row[at + o].clone());
                    row[i] = eval::eval(&e, &|x| match x {
                        Ref::Field(f) => fields(f).map_err(|_| err("a field the column reads")),
                        _ => Err(err("a column computed from a subquery that names the outer row reads the row's fields and the subqueries' values: rank by scores in ORDER BY")),
                    })?;
                }
            }
            for row in rs.rows.iter_mut() {
                row.truncate(at);
            }
            rs.columns.truncate(at);
            return Ok(rs);
        }
        let keys = Expr::In { expr: Box::new(Expr::Field(pk as u32)), list: rows_of.keys.iter().cloned().map(Expr::Lit).collect() };
        let mut l = leaf;
        l.leaf_exprs_mut(&mut |e| by_key(e))?;
        let search = l.search_mut().expect("a leaf");
        search.filter = match std::mem::replace(&mut search.filter, Expr::Lit(Value::Bool(true))) {
            Expr::Lit(Value::Bool(true)) => keys,
            Expr::And(mut xs) => {
                xs.push(keys);
                Expr::And(xs)
            }
            f => Expr::And(vec![f, keys]),
        };
        search.profile = None;
        if let (false, Some(j)) = (in_order, phase) {
            // From that phase on, the rows in the order step 1 gave them.
            let order = rows_of.keys.iter().enumerate().map(|(i, k)| KeyValue { key: k.clone(), value: Some((n - i) as f64) }).collect();
            search.order = vec![SortKey { expr: Expr::ByKey(order), desc: true, nulls_first: false }];
            search.phases.drain(..j);
        }
        (self.runner)(&Plan::new(l))
    }

    /// The values of correlated subquery `sub` for each of `tuples`, the
    /// values of the outer `fields` it names: one grouped run when the
    /// outer values meet only equalities ([`decorrelated`]), else a run for
    /// each distinct tuple. Counts the distinct tuples and the runs.
    fn sub_values(&mut self, sub: &Expr, fields: &[u32], tuples: &[Vec<Value>], distinct: &mut usize, runs: &mut usize) -> Result<Vec<Option<f64>>> {
        let Expr::Scalar(node) = sub else { unreachable!("a subquery") };
        // What does not read the outer row, once.
        let mut node = (**node).clone();
        self.hoist(&mut node)?;
        let id = |t: &mut dyn Iterator<Item = &Value>| t.map(canonical).collect::<Vec<_>>().join("\u{0}");
        let ids: Vec<String> = tuples.iter().map(|t| id(&mut t.iter())).collect();
        let mut seen = HashSet::new();
        let unique: Vec<&[Value]> = tuples.iter().zip(&ids).filter(|(_, i)| seen.insert(i.as_str())).map(|(t, _)| t.as_slice()).collect();
        *distinct += unique.len();
        let mut memo: HashMap<String, Option<f64>> = HashMap::new();
        // With a grouped run, the value of each tuple of no group: the
        // subquery's over no rows.
        let mut grouped: Option<(Vec<usize>, Option<Option<f64>>)> = None;
        let one = decorrelated(&node, fields, &unique, self.catalog);
        if let Some((q, outs)) = one {
            self.opts.check()?;
            *runs += 1;
            let rows = self.node(&q, "subquery")?;
            let k = outs.len();
            if rows.rows.iter().all(|r| r[..k].iter().all(|v| matches!(v, Value::Str(_) | Value::Int(_) | Value::Float(_)))) {
                for r in &rows.rows {
                    memo.insert(id(&mut r[..k].iter()), number(&r[k])?);
                }
                grouped = Some((outs, None));
            }
        }
        let mut out = Vec::with_capacity(tuples.len());
        for (t, tid) in tuples.iter().zip(&ids) {
            let key = match &grouped {
                Some((outs, _)) => id(&mut outs.iter().map(|&i| &t[i])),
                None => tid.clone(),
            };
            if let Some(v) = memo.get(&key) {
                out.push(*v);
                continue;
            }
            if let Some((_, Some(v))) = &grouped {
                out.push(*v);
                continue;
            }
            self.opts.check()?;
            let mut q = node.clone();
            q.visit_exprs_mut(&mut |x| {
                if let Expr::Outer(f) = x {
                    *x = Expr::Lit(t[fields.iter().position(|g| g == f).expect("a field it names")].clone());
                }
                Ok(())
            })?;
            outer_lists(&mut q)?;
            unknown(&mut q, &self.catalog);
            *runs += 1;
            let v = number(&self.scalar(&q, None, None)?)?;
            match &mut grouped {
                Some((_, empty)) => *empty = Some(v),
                None => {
                    memo.insert(key, v);
                }
            }
            out.push(v);
        }
        Ok(out)
    }

    /// Runs each input of `n` that does not read the outer row (an
    /// [`Expr::Outer`]) and is more than a `WITH` result's rows, and puts
    /// its rows in its place: a correlated subquery then reads them for
    /// each of the outer row's values without computing them again. The
    /// rows are `WITH` results in scope until the caller truncates them.
    fn hoist(&mut self, n: &mut Node) -> Result<()> {
        fn outer(n: &Node) -> bool {
            let mut found = false;
            n.visit_exprs(&mut |e| found |= matches!(e, Expr::Outer(_)));
            found
        }
        for i in n.inputs_mut() {
            if outer(i) {
                self.hoist(i)?;
            } else if !matches!(i, Node::Ref { .. }) {
                let rows = self.node(i, "subquery")?;
                let name = format!("#{}", self.ctes.len());
                self.ctes.push((name.clone(), Rc::new(rows)));
                *i = Node::Ref { name };
            }
        }
        Ok(())
    }

    /// `e`, over rows in memory, with its subqueries run.
    fn resolved(&mut self, e: &Expr) -> Result<Expr> {
        let mut e = e.clone();
        e.visit_mut(&mut |x| self.resolve(x, None))?;
        Ok(e)
    }

    /// Runs the subqueries `e` holds itself and puts their results in their
    /// place: values a field of `schema`'s table (a leaf's) is compared
    /// with in the field's type.
    fn resolve(&mut self, e: &mut Expr, schema: Option<&Schema>) -> Result<()> {
        let field = |x: &Expr| match x {
            Expr::Field(f) => Some(*f as usize),
            _ => None,
        };
        match e {
            // A value of the list each, as the binder makes an IN list, and
            // NULL once if the subquery returned one: `x NOT IN (SELECT ..)`
            // holds for no row then.
            Expr::In { expr, list } if list.iter().any(|x| matches!(x, Expr::Values(_) | Expr::Keys(_))) => {
                let f = field(expr);
                let mut out = Vec::with_capacity(list.len());
                let mut null = false;
                for x in std::mem::take(list) {
                    match x {
                        x @ (Expr::Values(_) | Expr::Keys(_)) => {
                            let (vals, nulls) = self.values_and_null(&x, schema, f)?;
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
                        **side = Expr::Lit(self.scalar(n, schema, f)?);
                    }
                }
            }
            Expr::Between { expr, lo, hi } => {
                let f = field(expr);
                for b in lo.iter_mut().chain(hi.iter_mut()) {
                    if let Expr::Scalar(n) = b.as_ref() {
                        **b = Expr::Lit(self.scalar(n, schema, f)?);
                    }
                }
            }
            Expr::Graph(g) | Expr::Ppr(g) => {
                let pk = schema.and_then(|s| s.primary_key);
                let Graph { seeds, kind, weights, .. } = g.as_mut();
                // PageRank weighted by the seeds' scores restarts at each
                // seed in proportion to the scores of the rows that name it.
                if matches!(kind, GraphKind::Ppr { weighted: true, .. }) && matches!(seeds, Expr::Values(_) | Expr::Keys(_)) {
                    let (keys, ws) = self.weighted_values(seeds, schema, pk)?;
                    *seeds = Expr::Lit(Value::List(keys));
                    *weights = ws;
                }
                for s in std::iter::once(seeds).chain(match kind {
                    GraphKind::Path { to, .. } => Some(to),
                    _ => None,
                }) {
                    if matches!(s, Expr::Values(_) | Expr::Keys(_)) {
                        *s = Expr::Lit(Value::List(self.values(s, schema, pk)?));
                    }
                }
            }
            Expr::MaxSim { vectors, .. } if matches!(vectors.as_ref(), Expr::Values(_)) => **vectors = Expr::Lit(Value::List(self.vectors(vectors)?)),
            // No events, no vector: such a ranking scores no row.
            Expr::MaxSim { field, vectors, .. } if matches!(vectors.as_ref(), Expr::Encode { .. }) => {
                trained_for(vectors, schema, *field)?;
                **vectors = Expr::Lit(Value::List(self.encoded(vectors)?.map(Value::Vector).into_iter().collect()))
            }
            Expr::Encode { .. } => *e = Expr::Lit(self.encoded(e)?.map_or(Value::Null, Value::Vector)),
            Expr::Values(_) | Expr::Keys(_) => *e = Expr::Lit(Value::List(self.values(e, None, None)?)),
            Expr::Exists(n) => *e = Expr::Lit(Value::Bool(!self.node(n, "subquery")?.rows.is_empty())),
            // Runs for each row, once the rows are known.
            Expr::Scalar(_) if e.is_correlated() => {}
            Expr::Scalar(n) => *e = Expr::Lit(self.scalar(n, None, None)?),
            _ => {}
        }
        Ok(())
    }

    /// The values of [`Expr::Values`] (each once, a list's each, no NULL)
    /// or [`Expr::Keys`], as the values of field `f` of `schema`'s table.
    fn values(&mut self, e: &Expr, schema: Option<&Schema>, f: Option<usize>) -> Result<Vec<Value>> {
        Ok(self.values_and_null(e, schema, f)?.0)
    }

    /// [`Ops::values`], and whether a row's value was NULL.
    fn values_and_null(&mut self, e: &Expr, schema: Option<&Schema>, f: Option<usize>) -> Result<(Vec<Value>, bool)> {
        let (Expr::Values(n) | Expr::Keys(n)) = e else { unreachable!("a subquery") };
        let mut null = false;
        let rows = self.node(n, "subquery")?;
        let vals = match e {
            Expr::Keys(_) => rows.keys.ok_or_else(|| err("the subquery's rows are not rows of the table, which have keys"))?,
            _ => {
                if rows.rows.first().is_some_and(|r| r.len() != 1) {
                    return Err(err(format!("a subquery in IN returns one column, not {}", rows.columns.len())));
                }
                let mut seen = HashSet::new();
                let mut out = Vec::new();
                for r in rows.rows {
                    let vs = match r.into_iter().next() {
                        Some(Value::List(xs)) => xs,
                        Some(Value::Null) => {
                            null = true;
                            Vec::new()
                        }
                        None => Vec::new(),
                        Some(v) => vec![v],
                    };
                    out.extend(vs.into_iter().filter(|v| seen.insert(part(v))));
                }
                out
            }
        };
        self.plan.push(format!("subquery: {} {}{}", vals.len(), if matches!(e, Expr::Keys(_)) { "keys" } else { "values" }, if null { " and NULL" } else { "" }));
        Ok((equal_to(schema, f, vals)?, null))
    }

    /// The vectors of [`Expr::Values`] for `maxsim`: each row's, in order,
    /// a list's each, none for NULL.
    fn vectors(&mut self, e: &Expr) -> Result<Vec<Value>> {
        let Expr::Values(n) = e else { unreachable!("a subquery") };
        let rows = self.node(n, "subquery")?;
        if rows.rows.first().is_some_and(|r| r.len() != 1) {
            return Err(err(format!("maxsim's subquery returns one column of vectors, not {}", rows.columns.len())));
        }
        let mut out = Vec::new();
        for v in rows.rows.into_iter().filter_map(|r| r.into_iter().next()) {
            match v {
                Value::Null => {}
                v @ Value::Vector(_) => out.push(v),
                Value::List(xs) if xs.iter().all(|x| matches!(x, Value::Vector(_))) => out.extend(xs),
                v => return Err(err(format!("maxsim's subquery returns vectors, not {v:?}"))),
            }
        }
        self.plan.push(format!("subquery: {} query vectors", out.len()));
        Ok(out)
    }

    /// The vector of [`Expr::Encode`]'s history by its model, `None` when
    /// none of its events' items has a vector.
    fn encoded(&mut self, e: &Expr) -> Result<Option<std::sync::Arc<[f32]>>> {
        if let Some((_, _, v)) = self.encoded.iter().find(|(x, n, _)| x == e && *n == self.ctes.len()) {
            return Ok(v.clone());
        }
        let v = self.encode(e)?;
        self.encoded.push((e.clone(), self.ctes.len(), v.clone()));
        Ok(v)
    }

    fn encode(&mut self, e: &Expr) -> Result<Option<std::sync::Arc<[f32]>>> {
        let Expr::Encode { model, rows, now, open, .. } = e else { unreachable!("an encode") };
        let m = open.0.clone().ok_or_else(|| err(format!("sequence model {model} is not open: Db::query and Db::run open the models of a query's plan (Db::open_ranking_models)")))?;
        let items = self.catalog.iter().find(|s| s.table.eq_ignore_ascii_case(&m.table)).copied().ok_or_else(|| err(format!("sequence model {model} reads the vectors of table {}, which this database has not", m.table)))?;
        let (pk, field) = (items.primary_key.ok_or_else(|| err(format!("sequence model {model}: table {} has no primary key", m.table)))?, items.field_index(&m.field));
        match field.map(|f| &items.fields[f].ty) {
            _ if field.is_some_and(|f| items.fields[f].multi) => return Err(err(format!("sequence model {model} reads one vector an item, and {}.{} holds several", m.table, m.field))),
            Some(FieldType::Vector { dim, .. }) if *dim == m.dim => {}
            Some(FieldType::Vector { dim, .. }) => return Err(err(format!("sequence model {model} was trained on {}.{} of {} dimensions; the field has {dim}", m.table, m.field, m.dim))),
            _ => return Err(err(format!("sequence model {model} reads VECTOR field {}.{}, which is not one", m.table, m.field))),
        }
        let field = field.expect("a field");
        let history = self.node(rows, "encode's history")?;
        if history.rows.first().is_some_and(|r| r.is_empty() || r.len() > 3) {
            return Err(err(format!("encode's history returns the item's key, then its action and time (one to three columns), not {}", history.columns.len())));
        }
        // Item, action, time; or of two columns, the item and an action (a
        // KEYWORD) or a time (a TIMESTAMP). A number second of two is
        // refused: it could be either.
        let at = |r: &[Value]| -> Result<(Option<usize>, Option<usize>)> {
            Ok(match r.len() {
                2 => match &r[1] {
                    Value::Timestamp(_) => (None, Some(1)),
                    Value::Int(_) | Value::Float(_) => {
                        return Err(err(format!(
                            "encode's history of two columns has an item, then an action (text) or a time (a TIMESTAMP), not the number {:?}: give a time in microseconds as the third of (item, action, time), e.g. SELECT item, NULL, ts",
                            r[1]
                        )))
                    }
                    _ => (Some(1), None),
                },
                3 => (Some(1), Some(2)),
                _ => (None, None),
            })
        };
        let time_of = |r: &[Value]| -> Result<Option<f64>> {
            Ok(match at(r)?.1.map(|i| &r[i]) {
                // Microseconds, as TIMESTAMPs and the rank log's export.
                Some(Value::Timestamp(t) | Value::Int(t)) => Some(*t as f64 / 1e6),
                Some(Value::Float(t)) if t.is_finite() => Some(*t / 1e6),
                Some(Value::Null) | None => None,
                Some(v) => return Err(err(format!("encode's history has times (a TIMESTAMP, or microseconds since the Unix epoch) in its last column, not {v:?}"))),
            })
        };
        // Newest first, as the model reads the newest: times that rise
        // down the rows are a history in the wrong order.
        let mut newest: Option<(usize, f64)> = None;
        for (i, r) in history.rows.iter().enumerate() {
            let Some(t) = time_of(r)? else { continue };
            if let Some((j, before)) = newest.filter(|&(_, before)| t > before) {
                return Err(err(format!(
                    "encode's history is read newest first, and its row {} is newer than its row {} ({t} s against {before} s since the epoch): order it by time, descending (ORDER BY ts DESC)",
                    i + 1,
                    j + 1
                )));
            }
            newest = Some((i, t));
        }
        // The items' vectors; then, as training reads a history, the
        // events whose item has one, the newest the model reads.
        let keys: Vec<Value> = history.rows.iter().map(|r| r[0].clone()).filter(|k| !matches!(k, Value::Null)).collect();
        let mut vectors: HashMap<Part, std::sync::Arc<[f32]>> = HashMap::new();
        if !keys.is_empty() {
            let leaf = Node::Project {
                input: Box::new(Node::Search(crate::query::ir::Search {
                    table: items.table.clone(),
                    filter: Expr::In { expr: Box::new(Expr::Field(pk as u32)), list: keys.into_iter().map(Expr::Lit).collect() },
                    order: Vec::new(),
                    phases: Vec::new(),
                    profile: None,
                    guard: None,
                    per: None,
                })),
                columns: vec![crate::query::ir::Column { name: "key".into(), expr: Expr::Field(pk as u32) }, crate::query::ir::Column { name: "vector".into(), expr: Expr::Field(field as u32) }],
            };
            for r in self.node(&leaf, "encode's items")?.rows {
                if let [k, Value::Vector(v)] = &r[..] {
                    // The table's vectors must be those the model was
                    // trained with, where it keeps them.
                    if let Some(want) = m.item_vector(k) {
                        if want.len() != v.len() || want.iter().zip(v.iter()).any(|(a, b)| (a - b).abs() > 1e-5 * a.abs().max(1.0)) {
                            return Err(err(format!(
                                "the vectors of {}.{} are not those sequence model {model} was trained with (hash {}; item {} differs): write the model's vectors into the table (strata.train.write_vectors), or train it on the table's",
                                m.table,
                                m.field,
                                m.hash,
                                show_key(k)
                            )));
                        }
                    }
                    vectors.insert(part(k), v.clone());
                }
            }
        }
        let events: Vec<&Vec<Value>> = history.rows.iter().filter(|r| vectors.contains_key(&part(&r[0]))).take(m.max_len).collect();
        let text: Vec<Option<String>> = events
            .iter()
            .map(|r| -> Result<Option<String>> {
                Ok(match at(r)?.0.map(|i| &r[i]) {
                    Some(Value::Str(a)) => Some(a.clone()),
                    Some(Value::Null) | None => None,
                    Some(Value::Int(n)) => Some(n.to_string()),
                    Some(v) => Some(format!("{v:?}")),
                })
            })
            .collect::<Result<_>>()?;
        let mut steps = Vec::with_capacity(events.len());
        for (r, action) in events.iter().zip(&text).rev() {
            let item = &vectors[&part(&r[0])];
            steps.push(crate::query::sequence::Event { item, action: action.as_deref(), time: time_of(r)? });
        }
        let now = now.unwrap_or_else(|| crate::timestamp::now() as f64 / 1e6);
        self.plan.push(format!("encode: {model} over {} events, {} with vectors", history.rows.len(), steps.len()));
        Ok(m.encode(&steps, Some(now)).map(Into::into))
    }

    /// [`Ops::values`] of a weighted walk's seeds, each with the sum of the
    /// scores of the rows that name it (1 for a row without a score), in
    /// the order they first appear.
    fn weighted_values(&mut self, e: &Expr, schema: Option<&Schema>, f: Option<usize>) -> Result<(Vec<Value>, Vec<f64>)> {
        let (Expr::Values(n) | Expr::Keys(n)) = e else { unreachable!("a subquery") };
        let rows = self.node(n, "subquery")?;
        let scores = rows.scores.clone().unwrap_or_else(|| vec![1.0; rows.rows.len()]);
        let named: Vec<(Vec<Value>, f64)> = match e {
            Expr::Keys(_) => {
                let keys = rows.keys.ok_or_else(|| err("the subquery's rows are not rows of the table, which have keys"))?;
                keys.into_iter().zip(scores).map(|(k, w)| (vec![k], w)).collect()
            }
            _ => {
                if rows.rows.first().is_some_and(|r| r.len() != 1) {
                    return Err(err(format!("a subquery of seeds returns one column, not {}", rows.columns.len())));
                }
                rows.rows
                    .into_iter()
                    .zip(scores)
                    .map(|(r, w)| {
                        let vs = match r.into_iter().next() {
                            Some(Value::List(xs)) => xs,
                            Some(Value::Null) | None => Vec::new(),
                            Some(v) => vec![v],
                        };
                        (vs, w)
                    })
                    .collect()
            }
        };
        let mut at: HashMap<Part, usize> = HashMap::new();
        let (mut keys, mut weights) = (Vec::new(), Vec::new());
        for (vs, w) in named {
            for v in vs {
                let Some(v) = equal_value(schema, f, v)? else { continue };
                match at.get(&part(&v)) {
                    Some(&i) => weights[i] += w,
                    None => {
                        at.insert(part(&v), keys.len());
                        keys.push(v);
                        weights.push(w);
                    }
                }
            }
        }
        // Restart weights are positive: a score of 0 or below counts as
        // the least, as for a subquery of the scanned table.
        for w in &mut weights {
            *w = if w.is_finite() { w.max(1e-9) } else { 1e-9 };
        }
        self.plan.push(format!("subquery: {} weighted seeds", keys.len()));
        Ok((keys, weights))
    }

    /// The value of [`Expr::Scalar`]: NULL without a row.
    fn scalar(&mut self, n: &Node, schema: Option<&Schema>, f: Option<usize>) -> Result<Value> {
        let rows = self.node(n, "subquery")?;
        if rows.rows.len() > 1 {
            return Err(err(format!("a scalar subquery returned {} rows; add LIMIT 1 or an aggregate", rows.rows.len())));
        }
        match rows.rows.into_iter().next().and_then(|r| r.into_iter().next()) {
            None | Some(Value::Null) => Ok(Value::Null),
            Some(v) => coerce(schema, f, v),
        }
    }

    /// LIMIT; over joins whose first table's scan is a leaf, or over the
    /// first rows of groups (`DISTINCT ON`, `MAX k PER`) of a scan, that scan reads a
    /// growing number of rows (twice the rows wanted, four times for
    /// groups, at least 32, then four times as many) until enough rows join
    /// or groups have a row, or it runs out: rows keep its order through
    /// joins, filters, projections and the first rows of groups.
    fn limit(&mut self, input: &Node, offset: usize, limit: Option<usize>, what: &str) -> Result<Rows> {
        let cut = |rows: Rows| {
            let idx: Vec<usize> = (offset..rows.rows.len()).take(limit.unwrap_or(usize::MAX)).collect();
            rows.pick(&idx)
        };
        let (Some(l), Some(scan)) = (limit, driver_of(input)) else { return Ok(cut(self.node(input, what)?)) };
        let want = l.saturating_add(offset);
        let first = if matches!(input, Node::DistinctOn { .. }) { 4 } else { 2 };
        // A scan that keeps the cap itself returns the first rows of each
        // value, so the rows wanted are all it needs to read; one that does
        // not (a shard over the typed protocol) returns them uncapped, and
        // the slices grow from there.
        let per = per_cap(input, scan);
        let mut lim = match per {
            Some(_) => want.clamp(1, MAX_ROWS),
            None => want.saturating_mul(first).clamp(32, MAX_ROWS),
        };
        let saved = self.driver.take();
        let saved_per = std::mem::replace(&mut self.per, per.map(|c| (scan as *const Node, c)));
        let r = loop {
            self.driver = Some((scan as *const Node, lim as u64, 0));
            let rows = match self.node(input, what) {
                Ok(r) => r,
                Err(e) => break Err(e),
            };
            let read = self.driver.map_or(0, |(_, _, read)| read);
            if rows.rows.len() >= want || read < lim || lim >= MAX_ROWS {
                break Ok(cut(rows));
            }
            lim = (lim * 4).min(MAX_ROWS);
        };
        self.driver = saved;
        self.per = saved_per;
        r
    }

    fn join(&mut self, n: &Node) -> Result<Rows> {
        let Node::Join { left, right, kind, keys, on } = n else { unreachable!("a join") };
        let l = self.node(left, "scan")?;
        if l.rows.len() > MAX_ROWS {
            return Err(err(format!("a table of the join would read more than {MAX_ROWS} rows; add conditions on it")));
        }
        let lefts_at: Vec<Expr> = keys.iter().map(|k| self.resolved(&k.left)).collect::<Result<_>>()?;
        // Whether each key's links, on either side, name INT keys.
        let ints: Vec<(bool, bool)> = keys.iter().map(|k| Ok((k.left_links && self.int_links(left, &k.left)?, k.right_links && self.int_links(right, &k.right)?))).collect::<Result<_>>()?;
        // The join values the left rows hold, for each key.
        let mut lefts: Vec<Vec<Value>> = Vec::new();
        for ((k, at), &(int, _)) in keys.iter().zip(&lefts_at).zip(&ints) {
            let mut seen = HashSet::new();
            let mut vals = Vec::new();
            for r in &l.rows {
                for v in join_values(&value(at, r)?, k.left_links, int)? {
                    if seen.insert(part(&v)) {
                        vals.push(v);
                    }
                }
            }
            lefts.push(vals);
        }
        let right_at: Vec<usize> = keys.iter().map(|k| output(&k.right)).collect::<Result<_>>()?;
        let r = if lefts.iter().any(|v| v.is_empty()) {
            Rows { columns: self.columns(right), ..Rows::default() }
        } else if right.is_leaf() {
            let restrict = keys.iter().zip(&right_at).zip(lefts).map(|((k, &j), vals)| Ok((self.restriction(right, j, k, vals.clone())?, vals.len()))).collect::<Result<_>>()?;
            self.leaf(right, "join", restrict)?
        } else {
            self.node(right, "join")?
        };
        if r.rows.len() > MAX_ROWS {
            return Err(err(format!("a table of the join would read more than {MAX_ROWS} rows; add conditions on it")));
        }
        // Hash the right side on its key columns.
        let mut table: HashMap<Vec<Part>, Vec<usize>> = HashMap::new();
        for (ri, row) in r.rows.iter().enumerate() {
            self.tick()?;
            let mut combos: Vec<Vec<Part>> = vec![Vec::new()];
            for ((k, &j), &(_, int)) in keys.iter().zip(&right_at).zip(&ints) {
                let v = row.get(j).ok_or_else(|| err(format!("no column {j} in the join's right rows of {} columns", row.len())))?;
                let vals: Vec<Part> = join_values(v, k.right_links, int)?.iter().map(part).collect();
                combos = combos.into_iter().flat_map(|c| vals.iter().map(move |v| [c.clone(), vec![v.clone()]].concat())).collect();
            }
            for c in combos {
                self.charge.add(64 + c.iter().map(Part::bytes).sum::<u64>())?;
                table.entry(c).or_default().push(ri);
            }
        }
        let on = on.as_ref().map(|c| self.resolved(c)).transpose()?;
        let width = r.columns.len().max(r.rows.first().map_or(0, Vec::len));
        let mut out = Rows { columns: l.columns.iter().chain(&r.columns).cloned().collect(), rows: Vec::new(), keys: None, scores: Some(Vec::new()) };
        let scores = out.scores.as_mut().expect("scores");
        for (li, lrow) in l.rows.iter().enumerate() {
            self.tick()?;
            let mut combos: Vec<Vec<Part>> = vec![Vec::new()];
            for ((k, at), &(int, _)) in keys.iter().zip(&lefts_at).zip(&ints) {
                let vals: Vec<Part> = join_values(&value(at, lrow)?, k.left_links, int)?.iter().map(part).collect();
                combos = combos.into_iter().flat_map(|c| vals.iter().map(move |v| [c.clone(), vec![v.clone()]].concat())).collect();
            }
            let mut matched: Vec<usize> = combos.iter().filter_map(|c| table.get(c)).flatten().copied().collect();
            matched.sort_unstable();
            matched.dedup();
            let score = l.scores.as_ref().map_or(0.0, |s| s[li]);
            let mut any = false;
            for ri in matched {
                self.tick()?;
                let row: Vec<Value> = lrow.iter().chain(&r.rows[ri]).cloned().collect();
                if on.as_ref().map_or(Ok(true), |c| eval::holds(c, &reader(&row)))? {
                    self.charge.row(&row)?;
                    out.rows.push(row);
                    scores.push(score);
                    any = true;
                }
            }
            if !any && *kind == JoinKind::Left {
                let row: Vec<Value> = lrow.iter().cloned().chain(std::iter::repeat_n(Value::Null, width)).collect();
                self.charge.row(&row)?;
                out.rows.push(row);
                scores.push(score);
            }
            if out.rows.len() > MAX_ROWS {
                return Err(err(format!("the join makes more than {MAX_ROWS} rows; add conditions or a LIMIT")));
            }
        }
        self.plan.push(format!("join ({}): {} rows", if *kind == JoinKind::Left { "left" } else { "inner" }, out.rows.len()));
        Ok(out)
    }

    /// The condition restricting a joined leaf to the left's join values
    /// `vals` of key `k` (on its column `j`): the field holds one of them,
    /// or for an EDGES field links to one.
    fn restriction(&self, leaf: &Node, j: usize, k: &JoinKey, vals: Vec<Value>) -> Result<Expr> {
        let schema = self.table(leaf)?;
        let Node::Project { columns, .. } = leaf else { unreachable!("a leaf") };
        let Some(c) = columns.get(j) else { return Err(err(format!("no column {j} in the join's right rows of {} columns", columns.len()))) };
        let Expr::Field(f) = &c.expr else { return Err(err("a join key of a scanned table is one of its fields")) };
        if k.right_links {
            let vals = vals.into_iter().map(|v| coerce(Some(schema), schema.primary_key, v)).collect::<Result<_>>()?;
            // `min_hops => 0`: a value's own row may link to another value.
            return Ok(Expr::Graph(Box::new(Graph {
                field: *f,
                edges: None,
                seeds: Expr::Lit(Value::List(vals)),
                weights: Vec::new(),
                kind: GraphKind::Reachable { min_hops: 0, max_hops: 1 },
                dir: Direction::In,
                labels: Vec::new(),
                through: None,
                max_nodes: (MAX_ROWS + 1) as u64,
                k: 10,
                min_sim: None,
            })));
        }
        let vals = equal_to(Some(schema), Some(*f as usize), vals)?;
        Ok(Expr::In { expr: Box::new(Expr::Field(*f)), list: vals.into_iter().map(Expr::Lit).collect::<Vec<_>>() })
    }

    fn aggregate(&mut self, input: &Node, keys: &[Expr], aggs: &[Agg], what: &str) -> Result<Rows> {
        let rows = self.node(input, what)?;
        let keys: Vec<Expr> = keys.iter().map(|k| self.resolved(k)).collect::<Result<_>>()?;
        let args: Vec<Option<Expr>> = aggs.iter().map(|a| a.arg.as_ref().map(|x| self.resolved(x)).transpose()).collect::<Result<_>>()?;
        let new_accs = || aggs.iter().map(|a| Acc::new(&Projection::Agg { func: a.func, of: crate::query::logical::Col::Field(0) })).collect::<Vec<_>>();
        let mut at: HashMap<Vec<Part>, usize> = HashMap::new();
        let mut groups: Vec<(Vec<Value>, Vec<Acc>)> = Vec::new();
        for r in &rows.rows {
            self.tick()?;
            // Each key's values: a multi-valued key puts the row in a group
            // per distinct value, and in the group NULL with none, as a
            // table's GROUP BY does.
            let each: Vec<Vec<Value>> = keys
                .iter()
                .map(|k| {
                    Ok(match value(k, r)? {
                        Value::List(xs) => {
                            let mut seen = HashSet::new();
                            let xs: Vec<Value> = xs.into_iter().filter(|x| seen.insert(Part::group(x))).collect();
                            if xs.is_empty() { vec![Value::Null] } else { xs }
                        }
                        v => vec![v],
                    })
                })
                .collect::<Result<_>>()?;
            // Every combination of the keys' values, the last key fastest.
            let mut pick = vec![0usize; each.len()];
            loop {
                let vals: Vec<Value> = each.iter().zip(&pick).map(|(vs, &i)| vs[i].clone()).collect();
                let key: Vec<Part> = vals.iter().map(Part::group).collect();
                let g = match at.get(&key) {
                    Some(&g) => g,
                    None => {
                        self.charge.row(&vals)?;
                        at.insert(key, groups.len());
                        groups.push((vals, new_accs()));
                        groups.len() - 1
                    }
                };
                for (acc, arg) in groups[g].1.iter_mut().zip(&args) {
                    match arg {
                        None => acc.add_row(),
                        Some(a) => match value(a, r)? {
                            Value::List(xs) => xs.iter().for_each(|x| {
                                acc.add(x);
                            }),
                            v => {
                                acc.add(&v);
                            }
                        },
                    }
                }
                let Some(k) = (0..pick.len()).rev().find(|&k| pick[k] + 1 < each[k].len()) else { break };
                pick[k] += 1;
                pick[k + 1..].iter_mut().for_each(|p| *p = 0);
            }
        }
        // Aggregates of no rows, without groups: one row.
        if groups.is_empty() && keys.is_empty() {
            groups.push((Vec::new(), new_accs()));
        }
        let out = groups.into_iter().map(|(vals, accs)| vals.into_iter().chain(accs.iter().map(Acc::value)).collect()).collect();
        Ok(Rows { columns: (0..keys.len() + aggs.len()).map(|i| format!("#{i}")).collect(), rows: out, keys: None, scores: None })
    }

    /// The columns a node gives, without running it.
    /// Whether the links of column `at` of `n`'s rows (an EDGES field) name
    /// INT keys: links name keys of their own table.
    fn int_links(&self, n: &Node, at: &Expr) -> Result<bool> {
        let leaf = match at {
            Expr::Output(i) => self.source(n, *i as usize),
            _ => None,
        };
        let t = self.table(leaf.ok_or_else(|| err("a join along links needs the EDGES field of a scanned table"))?)?;
        Ok(t.primary_key.is_some_and(|p| t.fields[p].ty == FieldType::Int))
    }

    /// The leaf whose field is column `i` of `n`'s rows, through joins,
    /// projections of columns and the nodes that keep rows as they are.
    fn source<'n>(&self, n: &'n Node, i: usize) -> Option<&'n Node> {
        if n.is_leaf() {
            return Some(n);
        }
        match n {
            Node::Join { left, right, .. } => {
                let w = self.columns(left).len();
                if i < w {
                    self.source(left, i)
                } else {
                    self.source(right, i - w)
                }
            }
            Node::Project { input, columns } => match columns.get(i)?.expr {
                Expr::Output(j) => self.source(input, j as usize),
                _ => None,
            },
            Node::Filter { input, .. } | Node::Sort { input, .. } | Node::Limit { input, .. } | Node::Distinct { input } | Node::DistinctOn { input, .. } | Node::KeyOrder { input, .. } => self.source(input, i),
            Node::Window { input, .. } if i < self.columns(input).len() => self.source(input, i),
            _ => None,
        }
    }

    fn columns(&self, n: &Node) -> Vec<String> {
        match n {
            Node::Project { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
            Node::Join { left, right, .. } => [self.columns(left), self.columns(right)].concat(),
            Node::Ref { name } => self.ctes.iter().rev().find(|(n, _)| n.eq_ignore_ascii_case(name)).map_or_else(Vec::new, |(_, r)| r.columns.clone()),
            Node::Aggregate { keys, aggs, .. } => (0..keys.len() + aggs.len()).map(|i| format!("#{i}")).collect(),
            Node::SetOp { left, .. } => self.columns(left),
            Node::Window { input, funcs } => {
                let mut c = self.columns(input);
                let w = c.len();
                c.extend((0..funcs.len()).map(|i| format!("#{}", w + i)));
                c
            }
            Node::Filter { input, .. }
            | Node::Sort { input, .. }
            | Node::Limit { input, .. }
            | Node::Distinct { input }
            | Node::DistinctOn { input, .. }
            | Node::KeyOrder { input, .. }
            | Node::With { input, .. } => self.columns(input),
            _ => Vec::new(),
        }
    }
}

/// `v` in the type of field `f` of `schema`'s table, as the binder reads a
/// value compared with it; a BOOL or TIMESTAMP field's as it is, which the
/// plan's raising stores, and a fraction compared with an INT field too,
/// which the raising rounds (`n > 2.5` is `n >= 3`).
pub(crate) fn coerce(schema: Option<&Schema>, f: Option<usize>, v: Value) -> Result<Value> {
    match f.and_then(|f| schema?.fields.get(f)) {
        Some(field) if field.ty == FieldType::Int && matches!(v, Value::Float(_)) => Ok(v),
        Some(field) if field.logical.is_none() => super::sql::coerced(field, v),
        _ => Ok(v),
    }
}

/// Values field `f` is to equal ([`coerce`]d): an INT field holds no
/// fraction, so a whole FLOAT is its INT and another FLOAT equals no row.
pub(crate) fn equal_to(schema: Option<&Schema>, f: Option<usize>, vals: Vec<Value>) -> Result<Vec<Value>> {
    vals.into_iter().filter_map(|v| equal_value(schema, f, v).transpose()).collect()
}

/// [`equal_to`] of one value: `None` for one no value of the field equals.
fn equal_value(schema: Option<&Schema>, f: Option<usize>, v: Value) -> Result<Option<Value>> {
    let int = f.and_then(|f| schema?.fields.get(f)).is_some_and(|field| field.ty == FieldType::Int && field.logical.is_none());
    match v {
        Value::Float(x) if int && x.fract() == 0.0 && x.abs() < 9e18 => coerce(schema, f, Value::Int(x as i64)).map(Some),
        Value::Float(_) if int => Ok(None),
        v => coerce(schema, f, v).map(Some),
    }
}

/// Refuses an [`Expr::Encode`] against field `f` of `schema`'s table when
/// its model gives vectors of another table's or field's.
fn trained_for(e: &Expr, schema: Option<&Schema>, f: u32) -> Result<()> {
    if let (Expr::Encode { model, open: crate::query::model::Opened(Some(m)), .. }, Some(s)) = (e, schema) {
        let name = s.fields.get(f as usize).map_or("", |x| &x.name);
        if !s.table.eq_ignore_ascii_case(&m.table) || !name.eq_ignore_ascii_case(&m.field) {
            return Err(err(format!("sequence model {model} gives vectors of {}.{}, not of {}.{name}", m.table, m.field, s.table)));
        }
    }
    Ok(())
}

/// The cap of a `MAX k PER` (or a `DISTINCT ON` of one column) whose
/// input is the scan `scan` itself, by one of its fields: the scan may keep
/// it ([`super::ir::Search::per`]). None when a filter, a join or an
/// expression stands between them, since the scan's rows are then not the
/// rows the cap counts.
pub(crate) fn per_cap(n: &Node, scan: &Node) -> Option<super::ir::PerCap> {
    match n {
        Node::DistinctOn { input, keys, per } if std::ptr::eq(input.as_ref(), scan) => {
            let [Expr::Output(i)] = &keys[..] else { return None };
            let Node::Project { columns, .. } = scan else { return None };
            match columns.get(*i as usize)?.expr {
                Expr::Field(field) => Some(super::ir::PerCap { field, k: *per }),
                _ => None,
            }
        }
        Node::DistinctOn { input, .. } | Node::Filter { input, .. } => per_cap(input, scan),
        Node::Project { input, .. } if !n.is_leaf() => per_cap(input, scan),
        _ => None,
    }
}

/// The first table's scan of the joins under a LIMIT, or the scan whose
/// rows' first of each group a `DISTINCT ON` keeps, through the nodes that
/// keep its rows' order (Project, Filter, DistinctOn): a leaf.
pub(crate) fn driver_of(n: &Node) -> Option<&Node> {
    match n {
        Node::DistinctOn { input, .. } => {
            let scan = match input.as_ref() {
                Node::Filter { input, .. } => input,
                _ => input,
            };
            if scan.is_leaf() {
                Some(scan)
            } else {
                driver_of(input)
            }
        }
        Node::Project { input, .. } | Node::Filter { input, .. } if !n.is_leaf() => driver_of(input),
        Node::Join { .. } => {
            let mut x = n;
            while let Node::Join { left, .. } = x {
                x = left;
            }
            x.is_leaf().then_some(x)
        }
        _ => None,
    }
}

/// A row in memory, as the evaluator reads it.
fn reader(row: &[Value]) -> impl Fn(Ref) -> Result<Value> + '_ {
    |r| match r {
        Ref::Output(i) => row.get(i as usize).cloned().ok_or_else(|| err(format!("no column {i} in rows of {} columns", row.len()))),
        r => Err(err(format!("{r:?} is not a column of rows in memory"))),
    }
}

fn value(e: &Expr, row: &[Value]) -> Result<Value> {
    eval::eval(e, &reader(row))
}

pub(crate) fn output(e: &Expr) -> Result<usize> {
    match e {
        Expr::Output(i) => Ok(*i as usize),
        e => Err(err(format!("a join key's right side is a column of the right rows, not {e:?}"))),
    }
}

/// A value as a join or group key: numbers equal by value whatever their
/// type.
pub(crate) fn part(v: &Value) -> Part {
    match v {
        Value::Float(x) if x.fract() == 0.0 && x.abs() < 9e15 => Part::Int(*x as i64),
        v => Part::of(v),
    }
}

/// A row's values as a [`Node::SetOp`] compares them: a whole FLOAT as
/// the INT it equals, NULL as NULL.
fn set_key(row: &[Value]) -> Vec<String> {
    row.iter()
        .map(|v| match v {
            Value::Float(x) if x.fract() == 0.0 && x.abs() < 9e15 => format!("{:?}", Value::Int(*x as i64)),
            v => format!("{v:?}"),
        })
        .collect()
}

/// A row key as an error names it.
fn show_key(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("{s:?}"),
        Value::Int(n) => n.to_string(),
        v => format!("{v:?}"),
    }
}

/// Join values of a value: each of a list's, or with `links` each target
/// of an EDGES field's links; none for NULL.
fn join_values(v: &Value, links: bool, int_keys: bool) -> Result<Vec<Value>> {
    if links {
        return Ok(crate::graph::parse_edges(v, int_keys)?.into_iter().map(|e| e.to).collect());
    }
    Ok(match v {
        Value::Null => Vec::new(),
        Value::List(xs) => xs.clone(),
        v => vec![v.clone()],
    })
}

/// The order of two values of sort key `k`: NULL last unless
/// `nulls_first`, either way.
fn compare(a: &Value, b: &Value, k: &SortKey) -> Ordering {
    match (matches!(a, Value::Null), matches!(b, Value::Null)) {
        (true, true) => Ordering::Equal,
        (true, false) if k.nulls_first => Ordering::Less,
        (true, false) => Ordering::Greater,
        (false, true) if k.nulls_first => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let c = agg::cmp(a, b);
            if k.desc { c.reverse() } else { c }
        }
    }
}

/// The searches of `n` with each comparison with NULL (an outer row's
/// value, a subquery's) read as SQL reads it: UNKNOWN, which keeps no
/// row, and neither does its negation. `x IN (1, NULL)` is TRUE where x is
/// 1 and else UNKNOWN; a multi-valued field's `IN` is never UNKNOWN (one
/// of its values is in the list, or none is, as on rows in memory), so the
/// NULL matches none of them. Indexes answer a comparison with a value
/// only.
pub(crate) fn unknown(n: &mut Node, catalog: &[&Schema]) {
    fn fold(e: &mut Expr, positive: bool, schema: Option<&Schema>) {
        let null = |x: &Expr| matches!(x, Expr::Lit(Value::Null));
        let multi = |x: &Expr| matches!(x, Expr::Field(f) if schema.and_then(|s| s.fields.get(*f as usize)).is_some_and(|f| f.multi));
        match e {
            Expr::Not(x) => fold(x, !positive, schema),
            Expr::And(xs) | Expr::Or(xs) => xs.iter_mut().for_each(|x| fold(x, positive, schema)),
            Expr::Cmp { left, right, .. } if null(left) || null(right) => *e = Expr::Lit(Value::Bool(!positive)),
            Expr::Between { expr, lo, hi } if null(expr) || lo.iter().chain(hi.iter()).any(|b| null(b)) => *e = Expr::Lit(Value::Bool(!positive)),
            Expr::In { expr, .. } if null(expr) => *e = Expr::Lit(Value::Bool(!positive)),
            Expr::In { expr, list } if list.iter().any(null) && !positive && !multi(expr) => *e = Expr::Lit(Value::Bool(true)),
            Expr::In { list, .. } if list.iter().any(null) => {
                list.retain(|x| !null(x));
                if list.is_empty() {
                    *e = Expr::Lit(Value::Bool(false));
                }
            }
            _ => {}
        }
    }
    if let Node::Search(s) = n {
        let schema = catalog.iter().find(|t| t.table.eq_ignore_ascii_case(&s.table)).copied();
        fold(&mut s.filter, true, schema);
    }
    n.inputs_mut().into_iter().for_each(|n| unknown(n, catalog));
}

/// The comparisons of `n` with a multi-valued column of the outer row,
/// its values substituted (a list), as a filter on a multi-valued field
/// reads them: `x = t.tags` holds when x is one of the values, `x !=
/// t.tags` when it is none of them, and `x < t.sizes` when it is below
/// one of them.
fn outer_lists(n: &mut Node) -> Result<()> {
    n.visit_exprs_mut(&mut |x| {
        let Expr::Cmp { op, left, right } = x else { return Ok(()) };
        // The list on the right: `x op list`.
        let (op, other, values) = match (&**left, &**right) {
            (o, Expr::Lit(Value::List(vs))) => (*op, o.clone(), vs.clone()),
            (Expr::Lit(Value::List(vs)), o) => {
                let flipped = match *op {
                    CmpOp::Lt => CmpOp::Gt,
                    CmpOp::Le => CmpOp::Ge,
                    CmpOp::Gt => CmpOp::Lt,
                    CmpOp::Ge => CmpOp::Le,
                    op => op,
                };
                (flipped, o.clone(), vs.clone())
            }
            _ => return Ok(()),
        };
        let one_of = Expr::In { expr: Box::new(other.clone()), list: values.iter().cloned().map(Expr::Lit).collect() };
        *x = match op {
            CmpOp::Eq => one_of,
            CmpOp::Ne => Expr::Not(Box::new(one_of)),
            op => Expr::Or(values.into_iter().map(|v| Expr::Cmp { op, left: Box::new(other.clone()), right: Box::new(Expr::Lit(v)) }).collect()),
        };
        Ok(())
    })
}

/// A subquery's value as a ranking reads it.
fn number(v: &Value) -> Result<Option<f64>> {
    Ok(match v {
        Value::Null => None,
        Value::Int(x) => Some(*x as f64),
        Value::Float(x) => Some(*x).filter(|x| x.is_finite()),
        Value::Bool(b) => Some(*b as u8 as f64),
        v => return Err(err(format!("a subquery in a ranking expression gives a number, not {v:?}"))),
    })
}

/// A value as a key of a correlated subquery's memo: numbers equal as
/// numbers (`2` and `2.0`), and a value of another kind never equal to one.
fn canonical(v: &Value) -> String {
    match v {
        Value::Int(x) => format!("n{}", *x as f64),
        Value::Float(x) => format!("n{x}"),
        Value::Str(s) => format!("s{s}"),
        // Bit for bit, which is cheaper to write than the numbers.
        Value::Vector(xs) => {
            use std::fmt::Write;
            let mut s = String::with_capacity(1 + 8 * xs.len());
            s.push('v');
            for x in xs.iter() {
                let _ = write!(s, "{:08x}", x.to_bits());
            }
            s
        }
        v => format!("{v:?}"),
    }
}

/// A correlated subquery `SELECT agg(..) FROM t WHERE c AND t.a = o.b`,
/// whose outer values meet only equalities ANDed into its filter, as one
/// query grouped by their other sides over the values the outer rows give
/// them (`t.a IN (...)`): a group's row is the subquery's value for outer
/// values equal to its keys, and outer values of no group take its value
/// over no rows. The query gives the keys, then the subquery's column;
/// with it, the index in `fields` of each key's outer field. A key of a
/// table's Search is a KEYWORD, INT or FLOAT column whose outer values are
/// of its kind; one over rows in memory, values compared as eval does.
fn decorrelated(n: &Node, fields: &[u32], tuples: &[&[Value]], catalog: &[&Schema]) -> Option<(Node, Vec<usize>)> {
    fn outer(e: &Expr) -> bool {
        let mut found = false;
        e.visit(&mut |x| found |= matches!(x, Expr::Outer(_)));
        found
    }
    fn node_outer(n: &Node) -> bool {
        let mut found = false;
        n.visit_exprs(&mut |x| found |= matches!(x, Expr::Outer(_)));
        found
    }
    let Node::Project { input, columns } = n else { return None };
    let below = match &**input {
        Node::Sort { input, .. } => input,
        n => n,
    };
    let Node::Aggregate { input: base, keys, aggs } = below else { return None };
    if !keys.is_empty() || columns.len() != 1 || outer(&columns[0].expr) || aggs.iter().any(|a| a.arg.as_ref().is_some_and(outer)) {
        return None;
    }
    let (filter, schema) = match &**base {
        Node::Search(s) => (&s.filter, Some(*catalog.iter().find(|t| t.table.eq_ignore_ascii_case(&s.table))?)),
        Node::Filter { input, cond } if !node_outer(input) => (cond, None),
        _ => return None,
    };
    let conjuncts: Vec<&Expr> = match filter {
        Expr::And(xs) => xs.iter().collect(),
        x => vec![x],
    };
    let (mut keys, mut outs, mut rest) = (Vec::new(), Vec::new(), Vec::new());
    for c in conjuncts {
        match c {
            Expr::Cmp { op: CmpOp::Eq, left, right } if matches!((&**left, &**right), (a, Expr::Outer(_)) | (Expr::Outer(_), a) if !outer(a)) => {
                let (a, o) = match (&**left, &**right) {
                    (a, Expr::Outer(o)) | (Expr::Outer(o), a) => (a, *o),
                    _ => unreachable!("an equality with the outer row"),
                };
                keys.push(a.clone());
                outs.push(fields.iter().position(|f| *f == o)?);
            }
            c if outer(c) => return None,
            c => rest.push(c.clone()),
        }
    }
    if keys.is_empty() {
        return None;
    }
    for (key, &o) in keys.iter().zip(&outs) {
        let mut seen = HashSet::new();
        let values: Vec<&Value> = tuples.iter().map(|t| &t[o]).filter(|v| **v != Value::Null && seen.insert(canonical(v))).collect();
        let fits = |v: &Value| match schema.map(|s| (key, s)) {
            Some((Expr::Field(f), s)) => {
                let fld = s.fields.get(*f as usize);
                match fld.map(|f| (&f.ty, f.logical.is_some())) {
                    Some((FieldType::Keyword, _)) => matches!(v, Value::Str(_)),
                    Some((FieldType::Int, false)) => matches!(v, Value::Int(_)),
                    Some((FieldType::Float, false)) => matches!(v, Value::Int(_) | Value::Float(_)),
                    _ => false,
                }
            }
            Some(_) => false,
            None => matches!(v, Value::Str(_) | Value::Int(_) | Value::Float(_)),
        };
        if values.is_empty() || !values.iter().all(|v| fits(v)) {
            return None;
        }
        rest.push(Expr::In { expr: Box::new(key.clone()), list: values.into_iter().cloned().map(Expr::Lit).collect() });
    }
    let filter = if rest.len() == 1 { rest.pop().expect("a condition") } else { Expr::And(rest) };
    let base = match &**base {
        Node::Search(s) => Node::Search(super::ir::Search { filter, ..s.clone() }),
        Node::Filter { input, .. } => Node::Filter { input: input.clone(), cond: filter },
        _ => unreachable!("a Search or a Filter"),
    };
    let k = keys.len() as u32;
    let mut value = columns[0].clone();
    value
        .expr
        .visit_mut(&mut |x| {
            if let Expr::Output(i) = x {
                *i += k;
            }
            Ok(())
        })
        .ok()?;
    let mut cols: Vec<super::ir::Column> = (0..k).map(|i| super::ir::Column { name: format!("#{i}"), expr: Expr::Output(i) }).collect();
    cols.push(value);
    Some((Node::Project { input: Box::new(Node::Aggregate { input: Box::new(base), keys, aggs: aggs.clone() }), columns: cols }, outs))
}
