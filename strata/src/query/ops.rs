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
use crate::query::exec::{ExecOptions, ExecStats, ResultSet};
use crate::query::ir::eval::{self, Ref};
use crate::query::ir::{Agg, Expr, Graph, GraphKind, JoinKey, JoinKind, Node, Plan, SortKey};
use crate::query::logical::Direction;
use crate::query::memory::Charge;
use crate::query::Projection;
use crate::schema::{FieldType, Schema};
use crate::value::Value;

/// Runs one leaf of a plan.
pub type Runner<'r> = dyn FnMut(&Plan) -> Result<ResultSet> + 'r;

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
    let mut ops = Ops { catalog, runner, opts, charge: Charge::new(opts.memory.clone()), steps: 0, ctes: Vec::new(), driver: None, plan: Vec::new() };
    let rows = ops.node(&plan.root, "query")?;
    let keys = rows.keys.filter(|k| k.iter().all(|k| !matches!(k, Value::Null)));
    Ok(ResultSet {
        columns: rows.columns,
        rows: rows.rows,
        keys: keys.unwrap_or_default(),
        scores: rows.scores.unwrap_or_default(),
        parts: None,
        stats: ExecStats { plan: ops.plan, filter_docs: 0, scored: 0, reranked: 0 },
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
    plan: Vec<String>,
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

    fn node(&mut self, n: &Node, what: &str) -> Result<Rows> {
        if n.is_leaf() {
            return self.leaf(n, what, Vec::new());
        }
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
            Node::Search(_) => Err(err("a Search runs under a Project, which names its columns")),
            Node::Insert { .. } | Node::Update { .. } | Node::Delete { .. } => Err(err("a write runs on its own, not inside a query's plan")),
        }
    }

    /// Runs a leaf with the executor, once its subqueries ran, with
    /// `restrict` added to its Search's filter.
    fn leaf(&mut self, n: &Node, what: &str, restrict: Vec<(Expr, usize)>) -> Result<Rows> {
        self.opts.check()?;
        let schema = self.table(n)?;
        let mut leaf = n.clone();
        leaf.leaf_exprs_mut(&mut |e| e.visit_mut(&mut |x| self.resolve(x, Some(schema))))?;
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
        let rs = (self.runner)(&Plan::new(leaf))?;
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
            // A value of the list each, as the binder makes an IN list.
            Expr::In { expr, list } if list.iter().any(|x| matches!(x, Expr::Values(_) | Expr::Keys(_))) => {
                let f = field(expr);
                let mut out = Vec::with_capacity(list.len());
                for x in std::mem::take(list) {
                    match x {
                        x @ (Expr::Values(_) | Expr::Keys(_)) => out.extend(self.values(&x, schema, f)?.into_iter().map(Expr::Lit)),
                        x => out.push(x),
                    }
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
            Expr::Values(_) | Expr::Keys(_) => *e = Expr::Lit(Value::List(self.values(e, None, None)?)),
            Expr::Exists(n) => *e = Expr::Lit(Value::Bool(!self.node(n, "subquery")?.rows.is_empty())),
            Expr::Scalar(n) => *e = Expr::Lit(self.scalar(n, None, None)?),
            _ => {}
        }
        Ok(())
    }

    /// The values of [`Expr::Values`] (each once, a list's each, no NULL)
    /// or [`Expr::Keys`], as the values of field `f` of `schema`'s table.
    fn values(&mut self, e: &Expr, schema: Option<&Schema>, f: Option<usize>) -> Result<Vec<Value>> {
        let (Expr::Values(n) | Expr::Keys(n)) = e else { unreachable!("a subquery") };
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
                        Some(Value::Null) | None => Vec::new(),
                        Some(v) => vec![v],
                    };
                    out.extend(vs.into_iter().filter(|v| seen.insert(part(v))));
                }
                out
            }
        };
        self.plan.push(format!("subquery: {} {}", vals.len(), if matches!(e, Expr::Keys(_)) { "keys" } else { "values" }));
        equal_to(schema, f, vals)
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

    /// LIMIT; over joins whose first table's scan is a leaf, that scan reads
    /// a growing number of rows (twice the rows wanted, at least 32, then
    /// four times as many) until enough rows join or it runs out: rows keep
    /// its order through joins, filters and projections.
    fn limit(&mut self, input: &Node, offset: usize, limit: Option<usize>, what: &str) -> Result<Rows> {
        let cut = |rows: Rows| {
            let idx: Vec<usize> = (offset..rows.rows.len()).take(limit.unwrap_or(usize::MAX)).collect();
            rows.pick(&idx)
        };
        let (Some(l), Some(scan)) = (limit, driver_of(input)) else { return Ok(cut(self.node(input, what)?)) };
        let want = l.saturating_add(offset);
        let mut lim = want.saturating_mul(2).clamp(32, MAX_ROWS);
        let saved = self.driver.take();
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
        let new_accs = || aggs.iter().map(|a| Acc::new(&Projection::Agg { func: a.func, field: 0 })).collect::<Vec<_>>();
        let mut at: HashMap<Vec<Part>, usize> = HashMap::new();
        let mut groups: Vec<(Vec<Value>, Vec<Acc>)> = Vec::new();
        for r in &rows.rows {
            self.tick()?;
            let vals: Vec<Value> = keys.iter().map(|k| value(k, r)).collect::<Result<_>>()?;
            let key: Vec<Part> = vals.iter().map(|v| if let Value::List(_) = v { Part::Str(format!("{v:?}")) } else { Part::of(v) }).collect();
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
            Node::Filter { input, .. } | Node::Sort { input, .. } | Node::Limit { input, .. } | Node::Distinct { input } | Node::KeyOrder { input, .. } => self.source(input, i),
            _ => None,
        }
    }

    fn columns(&self, n: &Node) -> Vec<String> {
        match n {
            Node::Project { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
            Node::Join { left, right, .. } => [self.columns(left), self.columns(right)].concat(),
            Node::Ref { name } => self.ctes.iter().rev().find(|(n, _)| n.eq_ignore_ascii_case(name)).map_or_else(Vec::new, |(_, r)| r.columns.clone()),
            Node::Aggregate { keys, aggs, .. } => (0..keys.len() + aggs.len()).map(|i| format!("#{i}")).collect(),
            Node::Filter { input, .. } | Node::Sort { input, .. } | Node::Limit { input, .. } | Node::Distinct { input } | Node::KeyOrder { input, .. } | Node::With { input, .. } => self.columns(input),
            _ => Vec::new(),
        }
    }
}

/// `v` in the type of field `f` of `schema`'s table, as the binder reads a
/// value compared with it; a BOOL or TIMESTAMP field's as it is, which the
/// plan's raising stores, and a fraction compared with an INT field too,
/// which the raising rounds (`n > 2.5` is `n >= 3`).
fn coerce(schema: Option<&Schema>, f: Option<usize>, v: Value) -> Result<Value> {
    match f.and_then(|f| schema?.fields.get(f)) {
        Some(field) if field.ty == FieldType::Int && matches!(v, Value::Float(_)) => Ok(v),
        Some(field) if field.logical.is_none() => super::sql::coerced(field, v),
        _ => Ok(v),
    }
}

/// Values field `f` is to equal ([`coerce`]d): an INT field holds no
/// fraction, so a whole FLOAT is its INT and another FLOAT equals no row.
fn equal_to(schema: Option<&Schema>, f: Option<usize>, vals: Vec<Value>) -> Result<Vec<Value>> {
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

/// The first table's scan of the joins under a LIMIT, through the nodes
/// that keep its rows' order (Project, Filter): a leaf.
fn driver_of(n: &Node) -> Option<&Node> {
    match n {
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

fn output(e: &Expr) -> Result<usize> {
    match e {
        Expr::Output(i) => Ok(*i as usize),
        e => Err(err(format!("a join key's right side is a column of the right rows, not {e:?}"))),
    }
}

/// A value as a join or group key: numbers equal by value whatever their
/// type.
fn part(v: &Value) -> Part {
    match v {
        Value::Float(x) if x.fract() == 0.0 && x.abs() < 9e15 => Part::Int(*x as i64),
        v => Part::of(v),
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
