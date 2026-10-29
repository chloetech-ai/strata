//! Compound queries over the table: `WITH`, `IN (SELECT ...)`, `EXISTS`,
//! scalar subqueries, `FROM (SELECT ...)`, `SELECT DISTINCT`, and joins of
//! the table with itself or with a `WITH` result.
//!
//! They run as a sequence of the single-table queries the executor answers
//! ([`is_compound`] tells them apart). A subquery runs first and its result
//! reaches the outer query as a list parameter (`IN (:_s0)`), so every
//! index still serves the outer filter. A join scans each side with the
//! conditions that name only that side, the right side restricted to the
//! join values the left side holds (`b.id IN (...)`, or for an EDGES field
//! the rows linking to them), and joins, filters, groups, orders and cuts
//! the rows in memory.
//!
//! ```sql
//! -- each recent graph paper with the papers it cites
//! SELECT a.id, a.title, b.id AS cited, b.title AS cited_title
//! FROM papers a JOIN papers b ON a.cites = b.id
//! WHERE MATCH(a.body, 'graph neural') AND a.year >= 2019 ORDER BY a.year DESC LIMIT 50;
//!
//! WITH top AS (SELECT id FROM papers ORDER BY bm25(body, 'graph neural network') DESC LIMIT 20)
//! SELECT category, COUNT(*) FROM papers WHERE REACHABLE(cites, (SELECT id FROM top), dir => 'in') GROUP BY category;
//! ```
//!
//! A join condition compares columns of two sides with `=`; comparing an
//! EDGES field with a key follows the edges (`a.cites = b.id`: b is a row
//! a links to). Text searches, scores and graph predicates name one side.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use sqlparser::ast::{self as sql, BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, SelectItem, TableFactor, UnaryOperator};

use crate::error::{Error, Result};
use crate::query::agg::{self, Acc, Part};
use crate::query::exec::{ExecOptions, ExecStats, ResultSet};
use crate::query::memory::Charge;
use crate::query::{AggFn, Params, Projection};
use crate::schema::{FieldType, Schema};
use crate::value::Value;

/// Rows one step of a compound query reads at most (a side of a join, a
/// subquery, a `WITH` result).
pub const MAX_ROWS: usize = 1_000_000;

/// Runs one single-table SELECT (the executor, or a coordinator).
pub type Runner<'r> = dyn FnMut(&str, &Params) -> Result<ResultSet> + 'r;

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// Functions that only the executor evaluates: they name one side of a join.
const NATIVE: [&str; 13] = ["match", "bm25", "similarity", "distance", "rrf", "ppr", "decay", "reachable", "path", "hops", "score", "now", "near"];
/// Score functions, which order a side's scan.
const SCORES: [&str; 7] = ["bm25", "similarity", "distance", "rrf", "ppr", "decay", "hops"];
const AGGREGATES: [&str; 5] = ["count", "sum", "avg", "min", "max"];

fn fname(f: &sql::Function) -> String {
    f.name.to_string().to_ascii_lowercase()
}

fn parse(sql_text: &str) -> Result<Box<sql::Query>> {
    let stmts = super::parse::parse(sql_text)?;
    match stmts.into_iter().collect::<Vec<_>>().as_mut_slice() {
        [sql::Statement::Query(q)] => Ok(q.clone()),
        _ => Err(err("expected one SELECT")),
    }
}

/// Whether `sql` needs this layer; anything else binds as one SELECT.
pub fn is_compound(sql_text: &str, schema: &Schema) -> bool {
    match super::parse::parse(sql_text) {
        Ok(stmts) => matches!(stmts.as_slice(), [sql::Statement::Query(q)] if needs(q, schema)),
        Err(_) => false,
    }
}

fn is_table(name: &sql::ObjectName, schema: &Schema) -> bool {
    name.to_string().eq_ignore_ascii_case(&schema.table)
}

/// [`is_compound`] of a parsed query.
pub(crate) fn needs(q: &sql::Query, schema: &Schema) -> bool {
    if q.with.is_some() {
        return true;
    }
    let sql::SetExpr::Select(s) = q.body.as_ref() else { return true };
    if s.from.len() != 1 || !s.from[0].joins.is_empty() || s.distinct.is_some() {
        return true;
    }
    match &s.from[0].relation {
        TableFactor::Table { name, .. } if is_table(name, schema) => {}
        _ => return true,
    }
    let mut found = false;
    let mut check = |e: &Expr| {
        // A graph's seed subquery over the table is the executor's own.
        let mut seeds: HashSet<*const Expr> = HashSet::new();
        walk(e, &mut |x| match x {
            Expr::Function(f) if matches!(fname(f).as_str(), "reachable" | "path" | "ppr") => {
                seeds.extend(func_args(f).into_iter().skip(1).filter(|a| matches!(a, Expr::Subquery(_))).map(|a| a as *const Expr));
            }
            Expr::Subquery(q) if seeds.contains(&(x as *const Expr)) => found |= needs(q, schema) || !matches!(q.body.as_ref(), sql::SetExpr::Select(_)),
            Expr::InSubquery { .. } | Expr::Exists { .. } | Expr::Subquery(_) => found = true,
            _ => {}
        })
    };
    for e in select_exprs(s) {
        check(e);
    }
    if let Some(ob) = &q.order_by {
        if let sql::OrderByKind::Expressions(es) = &ob.kind {
            for e in es {
                check(&e.expr);
            }
        }
    }
    found
}

/// The expressions of a SELECT outside its FROM and ORDER BY.
fn select_exprs(s: &sql::Select) -> Vec<&Expr> {
    let mut out: Vec<&Expr> = Vec::new();
    for item in &s.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => out.push(e),
            _ => {}
        }
    }
    out.extend(s.selection.iter());
    out.extend(s.having.iter());
    if let sql::GroupByExpr::Expressions(es, _) = &s.group_by {
        out.extend(es.iter());
    }
    out
}

/// Calls `f` on `e` and then its subexpressions, outside subqueries.
pub(crate) fn walk(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    match e {
        Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } | Expr::IsNull(x) | Expr::IsNotNull(x) | Expr::Cast { expr: x, .. } => walk(x, f),
        Expr::BinaryOp { left, right, .. } => {
            walk(left, f);
            walk(right, f);
        }
        Expr::Between { expr, low, high, .. } => {
            walk(expr, f);
            walk(low, f);
            walk(high, f);
        }
        Expr::InList { expr, list, .. } => {
            walk(expr, f);
            list.iter().for_each(|x| walk(x, f));
        }
        Expr::InSubquery { expr, .. } => walk(expr, f),
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            walk(expr, f);
            walk(pattern, f);
        }
        Expr::Function(func) => {
            for a in func_args(func) {
                walk(a, f);
            }
        }
        Expr::Case { operand, conditions, else_result, .. } => {
            operand.iter().for_each(|x| walk(x, f));
            for c in conditions {
                walk(&c.condition, f);
                walk(&c.result, f);
            }
            else_result.iter().for_each(|x| walk(x, f));
        }
        _ => {}
    }
}

fn func_args(f: &sql::Function) -> Vec<&Expr> {
    let FunctionArguments::List(list) = &f.args else { return Vec::new() };
    list.args
        .iter()
        .filter_map(|a| match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) | FunctionArg::Named { arg: FunctionArgExpr::Expr(e), .. } => Some(e),
            _ => None,
        })
        .collect()
}

fn func_args_mut(f: &mut sql::Function) -> Vec<&mut Expr> {
    let FunctionArguments::List(list) = &mut f.args else { return Vec::new() };
    list.args
        .iter_mut()
        .filter_map(|a| match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) | FunctionArg::Named { arg: FunctionArgExpr::Expr(e), .. } => Some(e),
            _ => None,
        })
        .collect()
}

fn is_star(f: &sql::Function) -> bool {
    matches!(&f.args, FunctionArguments::List(l) if l.args.iter().any(|a| matches!(a, FunctionArg::Unnamed(FunctionArgExpr::Wildcard)))) || func_args(f).is_empty()
}

/// Runs a compound SELECT; `runner` runs each single-table step.
pub fn run(sql_text: &str, schema: &Schema, params: &Params, runner: &mut Runner) -> Result<ResultSet> {
    run_checked(sql_text, schema, params, runner, &ExecOptions::default())
}

/// [`run`] under `opts`' deadline and cancel flag, checked every 1,024 rows
/// it joins, and its memory budget, which the steps' rows, the joins' hash
/// tables and the joined rows are counted against while the query runs.
/// The steps themselves run under whatever options `runner` gives them.
pub fn run_checked(sql_text: &str, schema: &Schema, params: &Params, runner: &mut Runner, opts: &ExecOptions) -> Result<ResultSet> {
    run_query(&*parse(sql_text)?, schema, params, runner, opts)
}

/// [`run_checked`] of a parsed query.
pub(crate) fn run_query(q: &sql::Query, schema: &Schema, params: &Params, runner: &mut Runner, opts: &ExecOptions) -> Result<ResultSet> {
    let charge = Charge::new(opts.memory.clone());
    let mut cx = Ctx { schema, params: params.clone(), ctes: HashMap::new(), runner, next: 0, plan: Vec::new(), opts, steps: 0, charge };
    let rel = cx.query(q)?;
    let n = rel.rows.len();
    Ok(ResultSet {
        columns: rel.columns,
        rows: rel.rows,
        keys: rel.keys.unwrap_or_default(),
        scores: if rel.scores.len() == n { rel.scores } else { Vec::new() },
        parts: None,
        stats: ExecStats { plan: cx.plan, filter_docs: 0, scored: 0 },
    })
}

/// Rows a step produced.
#[derive(Clone, Debug, Default)]
struct Rel {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    /// Each row's primary key, when the rows are rows of the table.
    keys: Option<Vec<Value>>,
    scores: Vec<f64>,
}

impl Rel {
    fn of(rs: ResultSet) -> Rel {
        let rows_of_table = rs.keys.len() == rs.rows.len() && !rs.rows.is_empty() && rs.keys.iter().all(|k| !matches!(k, Value::Null));
        let empty_rows = rs.rows.is_empty() && !rs.columns.is_empty();
        Rel {
            columns: rs.columns,
            rows: rs.rows,
            keys: if rows_of_table || empty_rows { Some(rs.keys) } else { None },
            scores: rs.scores,
        }
    }

    /// The values of the first column.
    fn first_column(&self) -> Result<Vec<Value>> {
        if self.columns.len() != 1 {
            return Err(err(format!("a subquery in IN returns one column, not {}", self.columns.len())));
        }
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for r in &self.rows {
            let vals: Vec<Value> = match &r[0] {
                Value::List(xs) => xs.clone(),
                Value::Null => Vec::new(),
                v => vec![v.clone()],
            };
            for v in vals {
                if seen.insert(Part::of(&v)) {
                    out.push(v);
                }
            }
        }
        Ok(out)
    }
}

struct Ctx<'a, 'r> {
    schema: &'a Schema,
    params: Params,
    ctes: HashMap<String, Rel>,
    runner: &'a mut Runner<'r>,
    next: usize,
    plan: Vec<String>,
    /// The query's deadline, cancel flag and memory budget
    /// ([`run_checked`]), steps of the joins since the deadline was last
    /// checked, and what the query holds.
    opts: &'a ExecOptions,
    steps: u32,
    charge: Charge,
}

/// One table of a join.
struct Src {
    alias: String,
    kind: SrcKind,
    /// How it joins the tables before it (none for the first).
    join: Option<(JoinKind, Expr)>,
}

enum SrcKind {
    /// The table, scanned with the conditions that name only this side.
    Table,
    /// Rows of the table a `WITH` result holds, scanned likewise.
    TableRows(Vec<Value>),
    /// Rows held in memory (a `WITH` result of aggregates, a derived table).
    Rows(Rel),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum JoinKind {
    Inner,
    Left,
}

/// Rows of several tables side by side: `cols[i]` is (alias, column).
struct Joined {
    cols: Vec<(String, String)>,
    rows: Vec<Vec<Value>>,
    /// The first table's key and score of each row.
    keys: Vec<Value>,
    scores: Vec<f64>,
}

impl<'a, 'r> Ctx<'a, 'r> {
    /// One step of a join: [`ExecOptions::check`] every 1,024.
    fn tick(&mut self) -> Result<()> {
        self.steps += 1;
        if self.steps < 1024 {
            return Ok(());
        }
        self.steps = 0;
        self.opts.check()
    }

    /// A new list or value parameter.
    fn param(&mut self, v: Value) -> Expr {
        let name = format!("_s{}", self.next);
        self.next += 1;
        self.params = std::mem::take(&mut self.params).set(&name, v);
        Expr::Value(sql::Value::Placeholder(format!(":{name}")).into())
    }

    fn pk_name(&self) -> Result<String> {
        let pk = self.schema.primary_key.ok_or_else(|| err("this needs a primary key"))?;
        Ok(self.schema.fields[pk].name.clone())
    }

    fn run_sql(&mut self, text: &str, what: &str) -> Result<Rel> {
        let rs = (self.runner)(text, &self.params).map_err(|e| match e {
            Error::Query(m) => err(format!("{m} (in {what}: {text})")),
            e => e,
        })?;
        self.plan.push(format!("{what}: {} rows: {text}", rs.rows.len()));
        for l in &rs.stats.plan {
            self.plan.push(format!("  {l}"));
        }
        for r in &rs.rows {
            self.charge.row(r)?;
        }
        Ok(Rel::of(rs))
    }

    fn query(&mut self, q: &sql::Query) -> Result<Rel> {
        if let Some(w) = &q.with {
            if w.recursive {
                return Err(err("WITH RECURSIVE is not supported: REACHABLE and PATH traverse links"));
            }
            for cte in &w.cte_tables {
                let mut rel = self.query(&cte.query)?;
                if !cte.alias.columns.is_empty() {
                    if cte.alias.columns.len() != rel.columns.len() {
                        return Err(err(format!("WITH {}: {} column names for {} columns", cte.alias.name, cte.alias.columns.len(), rel.columns.len())));
                    }
                    rel.columns = cte.alias.columns.iter().map(|c| c.name.value.clone()).collect();
                }
                if rel.rows.len() > MAX_ROWS {
                    return Err(err(format!("WITH {} holds more than {MAX_ROWS} rows", cte.alias.name)));
                }
                // No rows: rows of the table only if its columns are the table's.
                let own = |c: &String| self.schema.field(c).is_some() || c == "score()" || c == "hops()";
                if rel.rows.is_empty() && !rel.columns.iter().all(own) {
                    rel.keys = None;
                }
                self.plan.push(format!("WITH {}: {} rows", cte.alias.name, rel.rows.len()));
                self.ctes.insert(cte.alias.name.value.to_ascii_lowercase(), rel);
            }
        }
        let mut q = q.clone();
        q.with = None;
        let sql::SetExpr::Select(s) = q.body.as_ref() else { return Err(err("UNION, INTERSECT and EXCEPT are not supported")) };
        let s = s.as_ref().clone();
        if s.from.len() != 1 {
            return Err(err("FROM takes one table; join others with JOIN ... ON"));
        }
        if !s.from[0].joins.is_empty() {
            return self.join(&q, &s);
        }
        match &s.from[0].relation {
            TableFactor::Table { name, .. } if self.ctes.contains_key(&name.to_string().to_ascii_lowercase()) => {
                let rel = self.ctes[&name.to_string().to_ascii_lowercase()].clone();
                match rel.keys {
                    Some(keys) => self.over_rows(&q, &s, keys),
                    None => self.join(&q, &s),
                }
            }
            TableFactor::Table { name, .. } if is_table(name, self.schema) => self.single(&q, &s),
            TableFactor::Table { name, .. } => Err(err(format!("no such table: {name}"))),
            TableFactor::Derived { .. } => self.join(&q, &s),
            t => Err(err(format!("unsupported FROM: {t}"))),
        }
    }

    /// A SELECT over the table: its subqueries run first.
    fn single(&mut self, q: &sql::Query, s: &sql::Select) -> Result<Rel> {
        let mut q = q.clone();
        let mut s = s.clone();
        self.resolve_select(&mut s)?;
        if let Some(order) = q.order_by.as_mut() {
            if let sql::OrderByKind::Expressions(es) = &mut order.kind {
                for e in es {
                    self.resolve(&mut e.expr)?;
                }
            }
        }
        if s.distinct.is_some() {
            distinct_to_group(&mut s)?;
        }
        q.body = Box::new(sql::SetExpr::Select(Box::new(s)));
        let text = q.to_string();
        self.run_sql(&text, "query")
    }

    /// A SELECT over the rows of the table that a `WITH` result holds:
    /// the table, restricted to their keys, in their order.
    fn over_rows(&mut self, q: &sql::Query, s: &sql::Select, keys: Vec<Value>) -> Result<Rel> {
        let pk = self.pk_name()?;
        let mut q = q.clone();
        let mut s = s.clone();
        let TableFactor::Table { name, .. } = &mut s.from[0].relation else { unreachable!() };
        *name = sql::ObjectName::from(vec![sql::Ident::new(self.schema.table.clone())]);
        let keyp = self.param(Value::List(keys.clone()));
        let in_keys = Expr::InList { expr: Box::new(Expr::Identifier(sql::Ident::new(pk))), list: vec![keyp], negated: false };
        s.selection = Some(match s.selection.take() {
            Some(w) => Expr::BinaryOp { left: Box::new(in_keys), op: BinaryOperator::And, right: Box::new(Expr::Nested(Box::new(w))) },
            None => in_keys,
        });
        let grouped = !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty()) || has_aggregate(&s);
        // Without an ORDER BY, rows keep the WITH result's order, so the
        // cut happens after reordering.
        let reorder = q.order_by.is_none() && !grouped;
        let cut = if reorder { q.limit_clause.take() } else { None };
        q.body = Box::new(sql::SetExpr::Select(Box::new(s.clone())));
        let mut rel = self.single(&q, &s)?;
        if reorder {
            if let Some(got) = rel.keys.clone() {
                let at: HashMap<Part, usize> = keys.iter().enumerate().map(|(i, k)| (Part::of(k), i)).collect();
                let mut idx: Vec<usize> = (0..rel.rows.len()).collect();
                idx.sort_by_key(|&i| at.get(&Part::of(&got[i])).copied().unwrap_or(usize::MAX));
                let (offset, limit) = limits(&cut, &self.params)?;
                let idx: Vec<usize> = idx.into_iter().skip(offset).take(limit.unwrap_or(usize::MAX)).collect();
                rel.rows = idx.iter().map(|&i| rel.rows[i].clone()).collect();
                rel.keys = Some(idx.iter().map(|&i| got[i].clone()).collect());
                rel.scores = if rel.scores.len() == got.len() { idx.iter().map(|&i| rel.scores[i]).collect() } else { Vec::new() };
            }
        }
        Ok(rel)
    }

    fn resolve_select(&mut self, s: &mut sql::Select) -> Result<()> {
        for item in s.projection.iter_mut() {
            if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
                self.resolve(e)?;
            }
        }
        if let Some(w) = s.selection.as_mut() {
            self.resolve(w)?;
        }
        if let Some(h) = s.having.as_mut() {
            self.resolve(h)?;
        }
        Ok(())
    }

    /// Runs the subqueries of `e` and puts their results in its place: `IN
    /// (SELECT ...)` becomes `IN (:list)`, `EXISTS` true or false, a scalar
    /// subquery its value. A graph's seed subquery over the table stays for
    /// the executor; any other becomes the list of its rows' keys.
    fn resolve(&mut self, e: &mut Expr) -> Result<()> {
        match e {
            Expr::InSubquery { expr, subquery, negated } => {
                self.resolve(expr)?;
                let rel = self.query(subquery)?;
                let values = rel.first_column()?;
                self.plan.push(format!("IN (SELECT ...): {} values", values.len()));
                let p = self.param(Value::List(values));
                *e = Expr::InList { expr: expr.clone(), list: vec![p], negated: *negated };
            }
            Expr::Exists { subquery, negated } => {
                let rel = self.query(subquery)?;
                let b = rel.rows.is_empty() == *negated;
                *e = Expr::Value(sql::Value::Boolean(b).into());
            }
            Expr::Subquery(q) => {
                let rel = self.query(q)?;
                if rel.columns.len() != 1 {
                    return Err(err(format!("a scalar subquery returns one column, not {}", rel.columns.len())));
                }
                if rel.rows.len() > 1 {
                    return Err(err(format!("a scalar subquery returned {} rows; add LIMIT 1 or an aggregate", rel.rows.len())));
                }
                let v = rel.rows.first().map_or(Value::Null, |r| r[0].clone());
                *e = self.param(v);
            }
            Expr::Function(f) if matches!(fname(f).as_str(), "reachable" | "path" | "ppr") => {
                for (i, a) in func_args_mut(f).into_iter().enumerate() {
                    match a {
                        Expr::Subquery(q) if i > 0 => {
                            if !needs(q, self.schema) {
                                continue;
                            }
                            let rel = self.query(q)?;
                            let keys = match rel.keys {
                                Some(k) => k,
                                None => rel.first_column()?,
                            };
                            self.plan.push(format!("seeds: {} keys", keys.len()));
                            *a = self.param(Value::List(keys));
                        }
                        a => self.resolve(a)?,
                    }
                }
            }
            Expr::Function(f) => {
                for a in func_args_mut(f) {
                    self.resolve(a)?;
                }
            }
            Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } | Expr::IsNull(x) | Expr::IsNotNull(x) | Expr::Cast { expr: x, .. } => self.resolve(x)?,
            Expr::BinaryOp { left, right, .. } => {
                self.resolve(left)?;
                self.resolve(right)?;
            }
            Expr::Between { expr, low, high, .. } => {
                self.resolve(expr)?;
                self.resolve(low)?;
                self.resolve(high)?;
            }
            Expr::InList { expr, list, .. } => {
                self.resolve(expr)?;
                for x in list {
                    self.resolve(x)?;
                }
            }
            Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
                self.resolve(expr)?;
                self.resolve(pattern)?;
            }
            Expr::Case { operand, conditions, else_result, .. } => {
                if let Some(o) = operand {
                    self.resolve(o)?;
                }
                for c in conditions {
                    self.resolve(&mut c.condition)?;
                    self.resolve(&mut c.result)?;
                }
                if let Some(x) = else_result {
                    self.resolve(x)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn source(&mut self, t: &TableFactor) -> Result<(String, SrcKind)> {
        let alias_of = |a: &Option<sql::TableAlias>, d: &str| a.as_ref().map_or(d.to_string(), |a| a.name.value.clone());
        match t {
            TableFactor::Table { name, alias, .. } => {
                let n = name.to_string();
                if let Some(rel) = self.ctes.get(&n.to_ascii_lowercase()) {
                    let kind = match &rel.keys {
                        Some(k) => SrcKind::TableRows(k.clone()),
                        None => SrcKind::Rows(rel.clone()),
                    };
                    return Ok((alias_of(alias, &n), kind));
                }
                if !is_table(name, self.schema) {
                    return Err(err(format!("no such table: {n}")));
                }
                Ok((alias_of(alias, &n), SrcKind::Table))
            }
            TableFactor::Derived { subquery, alias, .. } => {
                let rel = self.query(subquery)?;
                let alias = alias.as_ref().map(|a| a.name.value.clone()).ok_or_else(|| err("a subquery in FROM needs an alias: FROM (SELECT ...) AS t"))?;
                Ok((alias, SrcKind::Rows(Rel { keys: None, ..rel })))
            }
            t => Err(err(format!("unsupported FROM: {t}"))),
        }
    }

    /// Joins (and any SELECT over rows held in memory).
    fn join(&mut self, q: &sql::Query, s: &sql::Select) -> Result<Rel> {
        let mut s = s.clone();
        self.resolve_select(&mut s)?;
        let mut order: Vec<sql::OrderByExpr> = match &q.order_by {
            None => Vec::new(),
            Some(ob) => match &ob.kind {
                sql::OrderByKind::Expressions(es) => es.clone(),
                _ => return Err(err("ORDER BY ALL is not supported")),
            },
        };
        for o in order.iter_mut() {
            self.resolve(&mut o.expr)?;
        }
        let (offset, limit) = limits(&q.limit_clause, &self.params)?;

        // The tables.
        let twj = &s.from[0];
        let mut srcs = Vec::new();
        let (alias, kind) = self.source(&twj.relation)?;
        srcs.push(Src { alias, kind, join: None });
        for j in &twj.joins {
            let (kind, c) = match &j.join_operator {
                sql::JoinOperator::Join(c) | sql::JoinOperator::Inner(c) => (JoinKind::Inner, c),
                sql::JoinOperator::Left(c) | sql::JoinOperator::LeftOuter(c) => (JoinKind::Left, c),
                o => return Err(err(format!("unsupported join {o:?}: use JOIN or LEFT JOIN ... ON")))?,
            };
            let sql::JoinConstraint::On(on) = c else { return Err(err("JOIN needs ON, e.g. JOIN papers b ON a.cites = b.id")) };
            let (alias, src) = self.source(&j.relation)?;
            srcs.push(Src { alias, kind: src, join: Some((kind, on.clone())) });
        }
        let mut seen = HashSet::new();
        for src in &srcs {
            if !seen.insert(src.alias.to_ascii_lowercase()) {
                return Err(err(format!("table alias {} is used twice: name each side, e.g. FROM papers a JOIN papers b", src.alias)));
            }
        }
        let cols_of: Vec<Vec<String>> = srcs.iter().map(|s| self.columns_of(s)).collect();
        let aliases: Vec<String> = srcs.iter().map(|s| s.alias.to_ascii_lowercase()).collect();
        let vectors: Vec<String> = self.schema.fields.iter().filter(|f| matches!(f.ty, FieldType::Vector { .. })).map(|f| f.name.clone()).collect();
        let sides = Sides { aliases: &aliases, cols: &cols_of, vectors: &vectors };

        // WHERE: each condition naming one scanned side goes to its scan.
        let mut pushed: Vec<Vec<Expr>> = vec![Vec::new(); srcs.len()];
        let mut residual: Vec<Expr> = Vec::new();
        for c in s.selection.iter().flat_map(split_and) {
            let at = sides.of(&c)?;
            let left_join_right = |i: usize| srcs[i].join.as_ref().is_some_and(|(k, _)| *k == JoinKind::Left);
            match at.as_slice() {
                [i] if !matches!(srcs[*i].kind, SrcKind::Rows(_)) && !left_join_right(*i) => pushed[*i].push(c),
                _ => residual.push(c),
            }
        }
        // ON: equalities with the tables before become join keys; conditions
        // naming only the new table go to its scan.
        let mut keys: Vec<Vec<(Expr, String)>> = vec![Vec::new(); srcs.len()];
        let mut on_rest: Vec<Vec<Expr>> = vec![Vec::new(); srcs.len()];
        for i in 1..srcs.len() {
            let (_, on) = srcs[i].join.clone().unwrap();
            for c in split_and(&on) {
                let at = sides.of(&c)?;
                if let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = &c {
                    let (l, r) = (sides.of(left)?, sides.of(right)?);
                    let col = |e: &Expr| sides.column(e).ok().flatten();
                    if r == [i] && l.iter().all(|&x| x < i) && !l.is_empty() && col(right).is_some() {
                        keys[i].push(((**left).clone(), col(right).unwrap().1));
                        continue;
                    }
                    if l == [i] && r.iter().all(|&x| x < i) && !r.is_empty() && col(left).is_some() {
                        keys[i].push(((**right).clone(), col(left).unwrap().1));
                        continue;
                    }
                }
                if at == [i] && !matches!(srcs[i].kind, SrcKind::Rows(_)) {
                    pushed[i].push(c);
                } else if at.iter().any(|&x| x > i) {
                    return Err(err(format!("ON of {} names a table joined later", srcs[i].alias)));
                } else {
                    on_rest[i].push(c);
                }
            }
            if keys[i].is_empty() {
                return Err(err(format!("JOIN {} needs an equality with the tables before it in ON, e.g. a.cites = {}.id or a.category = {}.category", srcs[i].alias, srcs[i].alias, srcs[i].alias)));
            }
        }

        // ORDER BY a score: the scan of its table ranks by it.
        let mut scan_order: Vec<Option<(Expr, bool)>> = vec![None; srcs.len()];
        let mut mem_order: Vec<(Expr, bool)> = Vec::new();
        for o in &order {
            let desc = o.options.asc == Some(false);
            if is_score(&o.expr) {
                let at = sides.of(&o.expr)?;
                let [i] = at.as_slice() else { return Err(err(format!("ORDER BY {}: a score ranks one table's rows", o.expr))) };
                if matches!(srcs[*i].kind, SrcKind::Rows(_)) || scan_order[*i].is_some() {
                    return Err(err(format!("ORDER BY {}: one score per scanned table", o.expr)));
                }
                scan_order[*i] = Some((o.expr.clone(), desc));
                mem_order.push((qualified(&srcs[*i].alias, "__score"), true));
            } else {
                mem_order.push((o.expr.clone(), desc));
            }
        }
        let grouped = !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty()) || has_aggregate(&s) || s.having.is_some();
        // The first table drives: when the order is its own, scans of a
        // growing number of its rows stop once enough rows join.
        let driver_order = !grouped
            && s.distinct.is_none()
            && match order.first() {
                None => true,
                Some(_) if mem_order.len() > 1 => false,
                // The executor orders by a score or a numeric field.
                Some(o) => {
                    scan_order[0].is_some()
                        || (!matches!(srcs[0].kind, SrcKind::Rows(_))
                            && sides.column(&o.expr)?.is_some_and(|(i, c)| i == 0 && self.schema.field(&c).is_some_and(|f| matches!(f.ty, FieldType::Int | FieldType::Float) && !f.multi)))
                }
            };
        if driver_order && order.len() == 1 && scan_order[0].is_none() {
            let (e, desc) = mem_order[0].clone();
            scan_order[0] = Some((e, desc));
        }
        let want = limit.map(|l| l.saturating_add(offset));
        let mut lim = match (driver_order, want) {
            (true, Some(w)) => Some(w.saturating_mul(2).clamp(32, MAX_ROWS)),
            _ => None,
        };
        let needed = sides.needed(&s, &order, &srcs)?;
        loop {
            let (joined, driver_rows) = self.scan_and_join(&srcs, &needed, &pushed, &keys, &on_rest, &scan_order, lim)?;
            let joined = filter_rows(joined, &residual, &self.params)?;
            let enough = want.is_some_and(|w| joined.rows.len() >= w);
            match lim {
                Some(l) if !enough && driver_rows >= l && l < MAX_ROWS => lim = Some((l * 4).min(MAX_ROWS)),
                _ => return evaluate(&s, joined, &mem_order, offset, limit, &self.params, srcs.len() > 1),
            }
        }
    }

    /// Columns a side offers.
    fn columns_of(&self, src: &Src) -> Vec<String> {
        match &src.kind {
            SrcKind::Rows(rel) => rel.columns.clone(),
            _ => self.schema.fields.iter().map(|f| f.name.clone()).chain(["__score".to_string()]).collect(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_and_join(
        &mut self,
        srcs: &[Src],
        needed: &[Vec<String>],
        pushed: &[Vec<Expr>],
        keys: &[Vec<(Expr, String)>],
        on_rest: &[Vec<Expr>],
        scan_order: &[Option<(Expr, bool)>],
        lim: Option<usize>,
    ) -> Result<(Joined, usize)> {
        let first = self.scan(&srcs[0], &needed[0], &pushed[0], &[], &scan_order[0], lim, "scan")?;
        let driver_rows = first.rows.len();
        let mut joined = Joined {
            cols: needed[0].iter().map(|c| (srcs[0].alias.to_ascii_lowercase(), c.clone())).collect(),
            keys: first.keys.clone().unwrap_or_else(|| vec![Value::Null; first.rows.len()]),
            scores: if first.scores.len() == first.rows.len() { first.scores.clone() } else { vec![0.0; first.rows.len()] },
            rows: first.rows,
        };
        for i in 1..srcs.len() {
            // The join values the rows so far hold, for each key.
            let mut lefts: Vec<Vec<Value>> = Vec::new();
            for (le, _) in &keys[i] {
                let mut seen = HashSet::new();
                let mut vals = Vec::new();
                for r in &joined.rows {
                    for v in self.join_values(&eval_row(le, &joined.cols, r, &self.params)?, &joined.cols, le)? {
                        if seen.insert(Part::of(&v)) {
                            vals.push(v);
                        }
                    }
                }
                lefts.push(vals);
            }
            let (kind, _) = srcs[i].join.clone().unwrap();
            let right = if lefts.iter().any(|v| v.is_empty()) {
                Rel { columns: needed[i].clone(), ..Rel::default() }
            } else {
                let restrict: Vec<(String, Vec<Value>)> = keys[i].iter().map(|(_, c)| c.clone()).zip(lefts).collect();
                self.scan(&srcs[i], &needed[i], &pushed[i], &restrict, &scan_order[i], None, &format!("join {}", srcs[i].alias))?
            };
            // Hash the right side on its key columns.
            let rcols: Vec<(String, String)> = needed[i].iter().map(|c| (srcs[i].alias.to_ascii_lowercase(), c.clone())).collect();
            let key_idx: Vec<usize> = keys[i].iter().map(|(_, c)| needed[i].iter().position(|x| x.eq_ignore_ascii_case(c)).expect("key column scanned")).collect();
            let mut table: HashMap<Vec<Part>, Vec<usize>> = HashMap::new();
            for (ri, r) in right.rows.iter().enumerate() {
                self.tick()?;
                let mut combos: Vec<Vec<Part>> = vec![Vec::new()];
                for &k in &key_idx {
                    let vals = self.key_parts(&r[k], &needed[i][k])?;
                    combos = combos.into_iter().flat_map(|c| vals.iter().map(move |v| [c.clone(), vec![v.clone()]].concat())).collect();
                }
                for c in combos {
                    self.charge.add(64 + c.iter().map(Part::bytes).sum::<u64>())?;
                    table.entry(c).or_default().push(ri);
                }
            }
            let mut cols = joined.cols.clone();
            cols.extend(rcols);
            let width = needed[i].len();
            let mut out = Joined { cols, rows: Vec::new(), keys: Vec::new(), scores: Vec::new() };
            for (li, l) in joined.rows.iter().enumerate() {
                self.tick()?;
                let mut combos: Vec<Vec<Part>> = vec![Vec::new()];
                for (le, _) in &keys[i] {
                    let v = eval_row(le, &joined.cols, l, &self.params)?;
                    let vals: Vec<Part> = self.join_values(&v, &joined.cols, le)?.iter().map(Part::of).collect();
                    combos = combos.into_iter().flat_map(|c| vals.iter().map(move |v| [c.clone(), vec![v.clone()]].concat())).collect();
                }
                let mut matched: Vec<usize> = combos.iter().filter_map(|c| table.get(c)).flatten().copied().collect();
                matched.sort_unstable();
                matched.dedup();
                let mut any = false;
                for ri in matched {
                    self.tick()?;
                    let row: Vec<Value> = l.iter().chain(&right.rows[ri]).cloned().collect();
                    if pass_all(&on_rest[i], &out.cols, &row, &self.params)? {
                        self.charge.row(&row)?;
                        out.rows.push(row);
                        out.keys.push(joined.keys[li].clone());
                        out.scores.push(joined.scores[li]);
                        any = true;
                    }
                }
                if !any && kind == JoinKind::Left {
                    let row: Vec<Value> = l.iter().cloned().chain(std::iter::repeat_n(Value::Null, width)).collect();
                    self.charge.row(&row)?;
                    out.rows.push(row);
                    out.keys.push(joined.keys[li].clone());
                    out.scores.push(joined.scores[li]);
                }
                if out.rows.len() > MAX_ROWS {
                    return Err(err(format!("the join makes more than {MAX_ROWS} rows; add conditions or a LIMIT")));
                }
            }
            self.plan.push(format!("join {} ({}): {} rows", srcs[i].alias, if kind == JoinKind::Left { "left" } else { "inner" }, out.rows.len()));
            joined = out;
        }
        Ok((joined, driver_rows))
    }

    /// Join values of a value: each of a list's, each target of an EDGES
    /// field's links.
    fn join_values(&self, v: &Value, cols: &[(String, String)], e: &Expr) -> Result<Vec<Value>> {
        let edges = match e {
            Expr::Identifier(id) => self.is_edges(&id.value) && cols.iter().filter(|(_, c)| c.eq_ignore_ascii_case(&id.value)).count() == 1,
            Expr::CompoundIdentifier(ids) => self.is_edges(&ids.last().unwrap().value),
            _ => false,
        };
        if edges {
            let int = self.schema.primary_key.is_some_and(|p| self.schema.fields[p].ty == FieldType::Int);
            return Ok(crate::graph::parse_edges(v, int)?.into_iter().map(|e| e.to).collect());
        }
        Ok(match v {
            Value::Null => Vec::new(),
            Value::List(xs) => xs.clone(),
            v => vec![v.clone()],
        })
    }

    fn key_parts(&self, v: &Value, column: &str) -> Result<Vec<Part>> {
        if self.is_edges(column) {
            let int = self.schema.primary_key.is_some_and(|p| self.schema.fields[p].ty == FieldType::Int);
            return Ok(crate::graph::parse_edges(v, int)?.iter().map(|e| Part::of(&e.to)).collect());
        }
        Ok(match v {
            Value::Null => Vec::new(),
            Value::List(xs) => xs.iter().map(Part::of).collect(),
            // Numbers join by value whatever their type.
            Value::Float(x) if x.fract() == 0.0 && x.abs() < 9e15 => vec![Part::Int(*x as i64)],
            v => vec![Part::of(v)],
        })
    }

    fn is_edges(&self, column: &str) -> bool {
        self.schema.field(column).is_some_and(|f| f.ty == FieldType::Edges)
    }

    /// Scans one side: its own conditions, its key values (`restrict`) and
    /// its ranking, returning the `columns` it needs.
    #[allow(clippy::too_many_arguments)]
    fn scan(&mut self, src: &Src, columns: &[String], conds: &[Expr], restrict: &[(String, Vec<Value>)], order: &Option<(Expr, bool)>, lim: Option<usize>, what: &str) -> Result<Rel> {
        if let SrcKind::Rows(rel) = &src.kind {
            // Held in memory: pick the columns.
            let idx: Vec<usize> = columns.iter().map(|c| rel.columns.iter().position(|x| x.eq_ignore_ascii_case(c)).ok_or_else(|| err(format!("{} has no column {c}", src.alias)))).collect::<Result<_>>()?;
            let mut rows: Vec<Vec<Value>> = rel.rows.iter().map(|r| idx.iter().map(|&i| r[i].clone()).collect()).collect();
            if !restrict.is_empty() {
                let sets: Vec<(usize, HashSet<Part>)> = restrict.iter().map(|(c, vs)| (columns.iter().position(|x| x.eq_ignore_ascii_case(c)).unwrap(), vs.iter().map(Part::of).collect())).collect();
                rows.retain(|r| sets.iter().all(|(i, set)| self.key_parts(&r[*i], &columns[*i]).is_ok_and(|ps| ps.iter().any(|p| set.contains(p)))));
            }
            self.plan.push(format!("{what} {}: {} rows held in memory", src.alias, rows.len()));
            return Ok(Rel { columns: columns.to_vec(), rows, keys: None, scores: Vec::new() });
        }
        let alias = src.alias.to_ascii_lowercase();
        let mut where_: Vec<String> = conds.iter().map(|c| unqualify(c, &alias).to_string()).collect();
        if let SrcKind::TableRows(keys) = &src.kind {
            let p = self.param(Value::List(keys.clone()));
            where_.push(format!("{} IN ({p})", self.pk_name()?));
        }
        for (col, vals) in restrict {
            let p = self.param(Value::List(vals.clone()));
            if self.is_edges(col) {
                // Rows whose links point at the values.
                // `min_hops => 0`: a value's own row may link to another value.
                where_.push(format!("REACHABLE({}, {p}, dir => 'in', min_hops => 0, max_nodes => {})", ident(col), MAX_ROWS + 1));
            } else {
                let f = self.schema.field(col).ok_or_else(|| err(format!("no such column: {col}")))?;
                if !matches!(f.ty, FieldType::Keyword | FieldType::Int | FieldType::Float) {
                    return Err(err(format!("joins compare KEYWORD, INT or FLOAT fields, the key or EDGES links, not {col}")));
                }
                where_.push(format!("{} IN ({p})", ident(col)));
            }
        }
        let cols: Vec<String> = columns.iter().map(|c| if c == "__score" { "score() AS __score".to_string() } else { ident(c) }).collect();
        let mut text = format!("SELECT {} FROM {}", cols.join(", "), ident(&self.schema.table));
        if !where_.is_empty() {
            text += &format!(" WHERE {}", where_.iter().map(|w| format!("({w})")).collect::<Vec<_>>().join(" AND "));
        }
        let ranked = order.as_ref().is_some_and(|(e, _)| is_score(e));
        if let Some((e, desc)) = order {
            text += &format!(" ORDER BY {}{}", unqualify(e, &alias), if *desc { " DESC" } else { "" });
        }
        match lim {
            Some(l) => text += &format!(" LIMIT {l}"),
            // A ranked scan without a LIMIT would stop at the executor's default.
            None if ranked => text += &format!(" LIMIT {}", MAX_ROWS + 1),
            None => {}
        }
        let rel = self.run_sql(&text, &format!("{what} {}", src.alias))?;
        if rel.rows.len() > MAX_ROWS {
            return Err(err(format!("{} would read more than {MAX_ROWS} rows; add conditions on it", src.alias)));
        }
        Ok(rel)
    }
}

/// Which tables an expression names, and column lookup.
struct Sides<'s> {
    aliases: &'s [String],
    cols: &'s [Vec<String>],
    /// VECTOR fields, which `*` leaves out.
    vectors: &'s [String],
}

impl Sides<'_> {
    /// The table and column of a column reference (`None` if not one).
    fn column(&self, e: &Expr) -> Result<Option<(usize, String)>> {
        match e {
            Expr::CompoundIdentifier(ids) if ids.len() >= 2 => {
                let a = ids[ids.len() - 2].value.to_ascii_lowercase();
                let c = &ids.last().unwrap().value;
                let i = self.aliases.iter().position(|x| *x == a).ok_or_else(|| err(format!("no table {a} in FROM")))?;
                if !self.cols[i].iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    return Err(err(format!("{a} has no column {c}")));
                }
                Ok(Some((i, c.clone())))
            }
            Expr::Identifier(id) => {
                let hits: Vec<usize> = (0..self.cols.len()).filter(|&i| self.cols[i].iter().any(|x| x.eq_ignore_ascii_case(&id.value))).collect();
                match hits.as_slice() {
                    [i] => Ok(Some((*i, id.value.clone()))),
                    [] => Ok(None),
                    _ => Err(err(format!("column {} is in several tables: qualify it ({}.{})", id.value, self.aliases[hits[0]], id.value))),
                }
            }
            _ => Ok(None),
        }
    }

    /// The tables `e` names, in order.
    fn of(&self, e: &Expr) -> Result<Vec<usize>> {
        let mut out = Vec::new();
        let mut failed = None;
        walk(e, &mut |x| {
            let hit = match x {
                // The first argument of a search or graph function is its field.
                Expr::Function(f) if NATIVE.contains(&fname(f).as_str()) => func_args(f).first().map(|a| self.column(a)),
                Expr::Identifier(_) | Expr::CompoundIdentifier(_) => Some(self.column(x)),
                _ => None,
            };
            match hit {
                Some(Ok(Some((i, _)))) if !out.contains(&i) => out.push(i),
                Some(Err(e)) => failed = Some(e),
                _ => {}
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        out.sort_unstable();
        Ok(out)
    }

    /// Whether `*` shows column `c` of table `i`.
    fn in_star(&self, i: usize, c: &str, srcs: &[Src]) -> bool {
        c != "__score" && (matches!(srcs[i].kind, SrcKind::Rows(_)) || !self.vectors.iter().any(|v| v == c))
    }

    /// The columns each table must return.
    fn needed(&self, s: &sql::Select, order: &[sql::OrderByExpr], srcs: &[Src]) -> Result<Vec<Vec<String>>> {
        let mut need: Vec<Vec<String>> = vec![Vec::new(); self.cols.len()];
        let mut add = |i: usize, c: &str| {
            if !need[i].iter().any(|x| x.eq_ignore_ascii_case(c)) {
                need[i].push(c.to_string());
            }
        };
        for item in &s.projection {
            match item {
                SelectItem::Wildcard(_) => {
                    for (i, cs) in self.cols.iter().enumerate() {
                        for c in cs.iter().filter(|c| self.in_star(i, c, srcs)) {
                            add(i, c);
                        }
                    }
                }
                SelectItem::QualifiedWildcard(sql::SelectItemQualifiedWildcardKind::ObjectName(n), _) => {
                    let a = n.to_string().to_ascii_lowercase();
                    let i = self.aliases.iter().position(|x| *x == a).ok_or_else(|| err(format!("no table {a} in FROM")))?;
                    for c in self.cols[i].iter().filter(|c| self.in_star(i, c, srcs)) {
                        add(i, c);
                    }
                }
                _ => {}
            }
        }
        let mut exprs: Vec<&Expr> = select_exprs(s);
        exprs.extend(order.iter().map(|o| &o.expr));
        for src in srcs {
            if let Some((_, on)) = &src.join {
                exprs.push(on);
            }
        }
        let mut failed = None;
        for e in exprs {
            walk(e, &mut |x| {
                // Columns inside search and graph functions stay in their scan.
                if let Expr::Function(f) = x {
                    if NATIVE.contains(&fname(f).as_str()) {
                        return;
                    }
                }
                match self.column(x) {
                    Ok(Some((i, c))) => add(i, &c),
                    Ok(None) => {}
                    Err(e) => failed = Some(e),
                }
            });
        }
        if let Some(e) = failed {
            return Err(e);
        }
        // A scan ranked by a score returns it, and `score()` reads the first one.
        for o in order {
            if is_score(&o.expr) {
                if let [i] = self.of(&o.expr)?.as_slice() {
                    add(*i, "__score");
                }
            }
        }
        Ok(need)
    }
}

/// `e` with `alias.` dropped from its column references.
fn unqualify(e: &Expr, alias: &str) -> Expr {
    let mut e = e.clone();
    fn go(e: &mut Expr, alias: &str) {
        match e {
            Expr::CompoundIdentifier(ids) if ids.len() == 2 && ids[0].value.eq_ignore_ascii_case(alias) => *e = Expr::Identifier(ids[1].clone()),
            Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } | Expr::IsNull(x) | Expr::IsNotNull(x) | Expr::Cast { expr: x, .. } => go(x, alias),
            Expr::BinaryOp { left, right, .. } => {
                go(left, alias);
                go(right, alias);
            }
            Expr::Between { expr, low, high, .. } => {
                go(expr, alias);
                go(low, alias);
                go(high, alias);
            }
            Expr::InList { expr, list, .. } => {
                go(expr, alias);
                list.iter_mut().for_each(|x| go(x, alias));
            }
            Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
                go(expr, alias);
                go(pattern, alias);
            }
            Expr::Function(f) => {
                for a in func_args_mut(f) {
                    go(a, alias);
                }
            }
            _ => {}
        }
    }
    go(&mut e, alias);
    e
}

fn qualified(alias: &str, column: &str) -> Expr {
    Expr::CompoundIdentifier(vec![sql::Ident::new(alias), sql::Ident::new(column)])
}

fn ident(n: &str) -> String {
    if n.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        n.to_string()
    } else {
        format!("\"{}\"", n.replace('"', "\"\""))
    }
}

fn split_and(e: &Expr) -> Vec<Expr> {
    match e {
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => [split_and(left), split_and(right)].concat(),
        Expr::Nested(x) if matches!(x.as_ref(), Expr::BinaryOp { op: BinaryOperator::And, .. }) => split_and(x),
        e => vec![e.clone()],
    }
}

fn is_score(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| {
        if let Expr::Function(f) = x {
            found |= SCORES.contains(&fname(f).as_str());
        }
    });
    found
}

fn has_aggregate(s: &sql::Select) -> bool {
    let mut found = false;
    for item in &s.projection {
        if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
            walk(e, &mut |x| found |= matches!(x, Expr::Function(f) if AGGREGATES.contains(&fname(f).as_str())));
        }
    }
    found
}

/// `SELECT DISTINCT a, b` as `GROUP BY a, b`.
fn distinct_to_group(s: &mut sql::Select) -> Result<()> {
    if !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty()) || has_aggregate(s) {
        return Err(err("SELECT DISTINCT with GROUP BY or aggregates: drop DISTINCT"));
    }
    let mut es = Vec::new();
    for item in &s.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => es.push(e.clone()),
            _ => return Err(err("SELECT DISTINCT * is not supported: name the columns")),
        }
    }
    s.group_by = sql::GroupByExpr::Expressions(es, Vec::new());
    s.distinct = None;
    Ok(())
}

fn limits(l: &Option<sql::LimitClause>, params: &Params) -> Result<(usize, Option<usize>)> {
    let n = |e: &Expr| -> Result<usize> {
        match eval_row(e, &[], &[], params)? {
            Value::Int(x) if x >= 0 => Ok(x as usize),
            v => Err(err(format!("LIMIT and OFFSET take non-negative integers, not {v:?}"))),
        }
    };
    Ok(match l {
        None => (0, None),
        Some(sql::LimitClause::LimitOffset { limit, offset, .. }) => (offset.as_ref().map(|o| n(&o.value)).transpose()?.unwrap_or(0), limit.as_ref().map(n).transpose()?),
        Some(sql::LimitClause::OffsetCommaLimit { offset, limit }) => (n(offset)?, Some(n(limit)?)),
    })
}

// ---- evaluation in memory ------------------------------------------------

/// A condition's truth value: `None` for NULL (UNKNOWN).
fn logic(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        v => Some(truthy(v)),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Int(x) => *x != 0,
        Value::Bool(b) => *b,
        Value::Timestamp(_) => true,
        Value::Float(x) => *x != 0.0,
        Value::Str(s) => !s.is_empty(),
        _ => false,
    }
}

fn pass_all(conds: &[Expr], cols: &[(String, String)], row: &[Value], params: &Params) -> Result<bool> {
    for c in conds {
        if !truthy(&eval_row(c, cols, row, params)?) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn filter_rows(mut j: Joined, conds: &[Expr], params: &Params) -> Result<Joined> {
    if conds.is_empty() {
        return Ok(j);
    }
    let mut keep = Vec::with_capacity(j.rows.len());
    for r in &j.rows {
        keep.push(pass_all(conds, &j.cols, r, params)?);
    }
    let mut it = keep.iter();
    j.rows.retain(|_| *it.next().unwrap());
    let mut it = keep.iter();
    j.keys.retain(|_| *it.next().unwrap());
    let mut it = keep.iter();
    j.scores.retain(|_| *it.next().unwrap());
    Ok(j)
}

/// Looks a column up in a row of joined tables.
fn lookup(e: &Expr, cols: &[(String, String)], row: &[Value]) -> Option<Result<Value>> {
    let (alias, name) = match e {
        Expr::CompoundIdentifier(ids) if ids.len() >= 2 => (Some(ids[ids.len() - 2].value.to_ascii_lowercase()), &ids.last().unwrap().value),
        Expr::Identifier(id) => (None, &id.value),
        _ => return None,
    };
    let hits: Vec<usize> = (0..cols.len()).filter(|&i| cols[i].1.eq_ignore_ascii_case(name) && alias.as_ref().is_none_or(|a| *a == cols[i].0)).collect();
    Some(match hits.as_slice() {
        [i] => Ok(row[*i].clone()),
        [] => Err(err(format!("no such column: {e}"))),
        _ => Err(err(format!("column {e} is in several tables: qualify it"))),
    })
}

fn eval_row(e: &Expr, cols: &[(String, String)], row: &[Value], params: &Params) -> Result<Value> {
    eval(e, params, &|x| lookup(x, cols, row))
}

/// Evaluates an expression; `look` answers column references (and, in a
/// grouped query, group values and aggregates) first.
fn eval(e: &Expr, params: &Params, look: &dyn Fn(&Expr) -> Option<Result<Value>>) -> Result<Value> {
    if let Some(v) = look(e) {
        return v;
    }
    let ev = |x: &Expr| eval(x, params, look);
    let num = |v: &Value| v.as_f64();
    Ok(match e {
        Expr::Nested(x) => ev(x)?,
        Expr::Value(v) => match &v.value {
            sql::Value::Number(s, _) => s.parse::<i64>().map(Value::Int).or_else(|_| s.parse::<f64>().map(Value::Float)).map_err(|_| err(format!("bad number {s}")))?,
            sql::Value::SingleQuotedString(s) | sql::Value::DoubleQuotedString(s) => Value::Str(s.clone()),
            sql::Value::Boolean(b) => Value::Bool(*b),
            sql::Value::Null => Value::Null,
            sql::Value::Placeholder(p) => params.0.get(p.trim_start_matches([':', '$', '@', '?'])).cloned().ok_or_else(|| err(format!("unbound parameter {p}")))?,
            v => return Err(err(format!("unsupported literal {v}"))),
        },
        // Conditions follow SQL's three-valued logic: a comparison with
        // NULL is NULL (UNKNOWN), NOT NULL is NULL, and a WHERE keeps only
        // rows whose condition is true.
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => match ev(expr)? {
            Value::Null => Value::Null,
            v => Value::Int(!truthy(&v) as i64),
        },
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => match ev(expr)? {
            Value::Int(x) => x.checked_neg().map_or(Value::Float(-(x as f64)), Value::Int),
            Value::Float(x) => Value::Float(-x),
            Value::Null => Value::Null,
            v => return Err(err(format!("cannot negate {v:?}"))),
        },
        Expr::UnaryOp { op: UnaryOperator::Plus, expr } => ev(expr)?,
        Expr::IsNull(x) => Value::Int(matches!(ev(x)?, Value::Null) as i64),
        Expr::IsNotNull(x) => Value::Int(!matches!(ev(x)?, Value::Null) as i64),
        Expr::BinaryOp { left, op, right } => {
            use BinaryOperator::*;
            match op {
                And | Or => {
                    let (a, b) = (logic(&ev(left)?), logic(&ev(right)?));
                    let v = if matches!(op, And) {
                        match (a, b) {
                            (Some(false), _) | (_, Some(false)) => Some(false),
                            (Some(true), Some(true)) => Some(true),
                            _ => None,
                        }
                    } else {
                        match (a, b) {
                            (Some(true), _) | (_, Some(true)) => Some(true),
                            (Some(false), Some(false)) => Some(false),
                            _ => None,
                        }
                    };
                    v.map_or(Value::Null, |t| Value::Int(t as i64))
                }
                Eq | NotEq | Lt | LtEq | Gt | GtEq => {
                    let (a, b) = (ev(left)?, ev(right)?);
                    if matches!(a, Value::Null) || matches!(b, Value::Null) {
                        return Ok(Value::Null);
                    }
                    let c = agg::cmp(&a, &b);
                    Value::Int(match op {
                        Eq => c == Ordering::Equal,
                        NotEq => c != Ordering::Equal,
                        Lt => c == Ordering::Less,
                        LtEq => c != Ordering::Greater,
                        Gt => c == Ordering::Greater,
                        _ => c != Ordering::Less,
                    } as i64)
                }
                StringConcat => {
                    let s = |v: Value| match v {
                        Value::Str(s) => s,
                        Value::Null => String::new(),
                        Value::Int(x) => x.to_string(),
                        Value::Float(x) => x.to_string(),
                        Value::Bool(b) => b.to_string(),
                        Value::Timestamp(t) => crate::timestamp::format(t),
                        v => format!("{v:?}"),
                    };
                    Value::Str(s(ev(left)?) + &s(ev(right)?))
                }
                Plus | Minus | Multiply | Divide | Modulo => {
                    let (a, b) = (ev(left)?, ev(right)?);
                    if let (Value::Int(x), Value::Int(y), false) = (&a, &b, matches!(op, Divide)) {
                        // An integer result that overflows becomes a FLOAT,
                        // as in SQLite.
                        let (fx, fy) = (*x as f64, *y as f64);
                        return Ok(match op {
                            Plus => x.checked_add(*y).map_or(Value::Float(fx + fy), Value::Int),
                            Minus => x.checked_sub(*y).map_or(Value::Float(fx - fy), Value::Int),
                            Multiply => x.checked_mul(*y).map_or(Value::Float(fx * fy), Value::Int),
                            _ if *y == 0 => Value::Null,
                            // i64::MIN % -1 overflows; the remainder is 0.
                            _ => Value::Int(x.checked_rem(*y).unwrap_or(0)),
                        });
                    }
                    match (num(&a), num(&b)) {
                        (Some(x), Some(y)) => Value::Float(match op {
                            Plus => x + y,
                            Minus => x - y,
                            Multiply => x * y,
                            Divide if y == 0.0 => return Ok(Value::Null),
                            Divide => x / y,
                            _ => x % y,
                        }),
                        _ => Value::Null,
                    }
                }
                o => return Err(err(format!("unsupported operator {o}"))),
            }
        }
        Expr::Between { expr, negated, low, high } => {
            let (v, lo, hi) = (ev(expr)?, ev(low)?, ev(high)?);
            if [&v, &lo, &hi].iter().any(|x| matches!(x, Value::Null)) {
                return Ok(Value::Null);
            }
            let inside = agg::cmp(&v, &lo) != Ordering::Less && agg::cmp(&v, &hi) != Ordering::Greater;
            Value::Int((inside != *negated) as i64)
        }
        Expr::InList { expr, list, negated } => {
            let v = ev(expr)?;
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            // `x IN (1, NULL)` is NULL, not false, when x is not 1.
            let (mut found, mut null) = (false, false);
            for x in list {
                let xs = match ev(x)? {
                    Value::List(xs) => xs,
                    y => vec![y],
                };
                for y in &xs {
                    null |= matches!(y, Value::Null);
                    found |= !matches!(y, Value::Null) && agg::cmp(&v, y) == Ordering::Equal;
                }
            }
            if !found && null {
                return Ok(Value::Null);
            }
            Value::Int((found != *negated) as i64)
        }
        Expr::Like { negated, expr, pattern, .. } | Expr::ILike { negated, expr, pattern, .. } => {
            let ci = matches!(e, Expr::ILike { .. });
            let (Value::Str(s), Value::Str(p)) = (ev(expr)?, ev(pattern)?) else { return Ok(Value::Null) };
            let (s, p) = if ci { (s.to_lowercase(), p.to_lowercase()) } else { (s, p) };
            Value::Int((like(&s.chars().collect::<Vec<_>>(), &p.chars().collect::<Vec<_>>()) != *negated) as i64)
        }
        Expr::Case { operand, conditions, else_result, .. } => {
            let base = operand.as_ref().map(|o| ev(o)).transpose()?;
            for c in conditions {
                let hit = match &base {
                    Some(b) => agg::cmp(b, &ev(&c.condition)?) == Ordering::Equal,
                    None => truthy(&ev(&c.condition)?),
                };
                if hit {
                    return ev(&c.result);
                }
            }
            else_result.as_ref().map(|x| ev(x)).transpose()?.unwrap_or(Value::Null)
        }
        Expr::Function(f) => {
            let name = fname(f);
            let args: Vec<Value> = func_args(f).into_iter().map(ev).collect::<Result<_>>()?;
            let one = || args.first().cloned().unwrap_or(Value::Null);
            match name.as_str() {
                "lower" => one().as_str().map_or(Value::Null, |s| Value::Str(s.to_lowercase())),
                "upper" => one().as_str().map_or(Value::Null, |s| Value::Str(s.to_uppercase())),
                "length" => one().as_str().map_or(Value::Null, |s| Value::Int(s.chars().count() as i64)),
                "abs" => match one() {
                    Value::Int(x) => x.checked_abs().map_or(Value::Float((x as f64).abs()), Value::Int),
                    v => num(&v).map_or(Value::Null, |x| Value::Float(x.abs())),
                },
                "round" => {
                    let d = args.get(1).and_then(num).unwrap_or(0.0) as i32;
                    num(&one()).map_or(Value::Null, |x| {
                        let m = 10f64.powi(d);
                        let r = (x * m).round() / m;
                        if d <= 0 { Value::Int(r as i64) } else { Value::Float(r) }
                    })
                }
                "coalesce" => args.into_iter().find(|v| !matches!(v, Value::Null)).unwrap_or(Value::Null),
                n if NATIVE.contains(&n) => return Err(err(format!("{}(...) works on one table's scan: give it that table's column (a.body) and use it in WHERE or ORDER BY, not across tables", n.to_uppercase()))),
                n if AGGREGATES.contains(&n) => return Err(err(format!("{}(...) needs GROUP BY or a SELECT of aggregates", n.to_uppercase()))),
                n => return Err(err(format!("unknown function {n}"))),
            }
        }
        e @ (Expr::Identifier(_) | Expr::CompoundIdentifier(_)) => return Err(err(format!("no such column: {e}"))),
        e => return Err(err(format!("unsupported expression: {e}"))),
    })
}

/// SQL LIKE: `%` any run, `_` one character. Greedy matching that backs
/// up only to the last `%`: linear in the text for each pattern piece, and
/// without recursion, so a long pattern cannot overflow the stack.
fn like(s: &[char], p: &[char]) -> bool {
    let (mut i, mut j) = (0, 0);
    // The last `%` seen, and where in `s` its run currently ends.
    let mut star: Option<(usize, usize)> = None;
    while i < s.len() {
        if j < p.len() && (p[j] == '_' || (p[j] != '%' && p[j] == s[i])) {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '%' {
            star = Some((j, i));
            j += 1;
        } else if let Some((sj, si)) = star {
            // Let the last `%` swallow one more character and retry.
            j = sj + 1;
            i = si + 1;
            star = Some((sj, si + 1));
        } else {
            return false;
        }
    }
    p[j..].iter().all(|&c| c == '%')
}

/// An aggregate call: its text (how other expressions name it), function and argument.
struct AggCall {
    text: String,
    func: Option<AggFn>,
    arg: Option<Expr>,
}

fn agg_calls(exprs: &[&Expr]) -> Result<Vec<AggCall>> {
    let mut out: Vec<AggCall> = Vec::new();
    for e in exprs {
        walk(e, &mut |x| {
            let Expr::Function(f) = x else { return };
            let name = fname(f);
            if !AGGREGATES.contains(&name.as_str()) || out.iter().any(|a| a.text == x.to_string()) {
                return;
            }
            let distinct = matches!(&f.args, FunctionArguments::List(l) if l.duplicate_treatment == Some(sql::DuplicateTreatment::Distinct));
            let (func, arg) = if name == "count" && is_star(f) {
                (None, None)
            } else {
                let func = match name.as_str() {
                    "count" if distinct => AggFn::CountDistinct,
                    "count" => AggFn::Count,
                    "sum" => AggFn::Sum,
                    "avg" => AggFn::Avg,
                    "min" => AggFn::Min,
                    _ => AggFn::Max,
                };
                (Some(func), func_args(f).first().map(|a| (*a).clone()))
            };
            out.push(AggCall { text: x.to_string(), func, arg });
        });
    }
    Ok(out)
}

/// WHERE (already applied), GROUP BY, HAVING, ORDER BY, OFFSET, LIMIT and
/// the projection, over rows in memory.
fn evaluate(s: &sql::Select, j: Joined, order: &[(Expr, bool)], offset: usize, limit: Option<usize>, params: &Params, joined: bool) -> Result<Rel> {
    // The output columns.
    let mut items: Vec<(String, Expr)> = Vec::new();
    for item in &s.projection {
        match item {
            SelectItem::UnnamedExpr(e) => items.push((e.to_string(), e.clone())),
            SelectItem::ExprWithAlias { expr, alias } => items.push((alias.value.clone(), expr.clone())),
            SelectItem::Wildcard(_) => {
                for (a, c) in j.cols.iter().filter(|(_, c)| c != "__score") {
                    items.push((if joined { format!("{a}.{c}") } else { c.clone() }, qualified(a, c)));
                }
            }
            SelectItem::QualifiedWildcard(sql::SelectItemQualifiedWildcardKind::ObjectName(n), _) => {
                let a = n.to_string().to_ascii_lowercase();
                for (_, c) in j.cols.iter().filter(|(x, c)| *x == a && c != "__score") {
                    items.push((format!("{a}.{c}"), qualified(&a, c)));
                }
            }
            i => return Err(err(format!("unsupported select item {i}"))),
        }
    }
    // `score()`: the first ranked table's score.
    let score_col = j.cols.iter().find(|(_, c)| c == "__score").map(|(a, _)| qualified(a, "__score"));
    let fix_score = |e: &Expr| -> Result<Expr> {
        match e {
            Expr::Function(f) if fname(f) == "score" => score_col.clone().ok_or_else(|| err("score() needs ORDER BY a score (bm25, similarity, ppr, ...)")),
            e => Ok(e.clone()),
        }
    };
    for it in items.iter_mut() {
        it.1 = fix_score(&it.1)?;
    }
    let group_exprs: Vec<Expr> = match &s.group_by {
        sql::GroupByExpr::Expressions(es, _) => es
            .iter()
            .map(|e| match e {
                // GROUP BY 1, or a selected column's alias.
                Expr::Value(v) if matches!(v.value, sql::Value::Number(..)) => {
                    let n: usize = e.to_string().parse().unwrap_or(0);
                    items.get(n.wrapping_sub(1)).map(|i| i.1.clone()).ok_or_else(|| err(format!("GROUP BY {n}: no such column")))
                }
                Expr::Identifier(id) if lookup(e, &j.cols, &vec![Value::Null; j.cols.len()]).is_none_or(|r| r.is_err()) => {
                    items.iter().find(|(n, _)| n.eq_ignore_ascii_case(&id.value)).map(|i| i.1.clone()).ok_or_else(|| err(format!("no such column: {e}")))
                }
                e => Ok(e.clone()),
            })
            .collect::<Result<_>>()?,
        _ => return Err(err("GROUP BY ALL is not supported")),
    };
    let mut item_exprs: Vec<&Expr> = items.iter().map(|i| &i.1).collect();
    item_exprs.extend(s.having.iter());
    item_exprs.extend(order.iter().map(|o| &o.0));
    let calls = agg_calls(&item_exprs)?;
    let grouped = !group_exprs.is_empty() || !calls.is_empty();
    let mut out: Vec<(Vec<Value>, Vec<Value>, Value, f64)> = Vec::new(); // row, sort keys, key, score
    let by_alias = |e: &Expr, row: &[Value]| -> Option<Value> {
        match e {
            Expr::Identifier(id) => items.iter().position(|(n, _)| n.eq_ignore_ascii_case(&id.value)).map(|i| row[i].clone()),
            Expr::Value(v) if matches!(v.value, sql::Value::Number(..)) => e.to_string().parse::<usize>().ok().and_then(|n| row.get(n.wrapping_sub(1)).cloned()),
            _ => None,
        }
    };
    if grouped {
        let mut groups: HashMap<Vec<Part>, (Vec<Value>, Vec<Acc>, usize)> = HashMap::new();
        let mut order_seen: Vec<Vec<Part>> = Vec::new();
        let projections: Vec<Projection> = calls.iter().map(|c| Projection::Agg { func: c.func.unwrap_or(AggFn::Count), field: 0 }).collect();
        for r in &j.rows {
            let vals: Vec<Value> = group_exprs.iter().map(|g| eval_row(g, &j.cols, r, params)).collect::<Result<_>>()?;
            let key: Vec<Part> = vals.iter().map(|v| if let Value::List(_) = v { Part::Str(format!("{v:?}")) } else { Part::of(v) }).collect();
            let entry = groups.entry(key.clone()).or_insert_with(|| {
                order_seen.push(key);
                (vals, projections.iter().map(Acc::new).collect(), 0)
            });
            entry.2 += 1;
            for (acc, c) in entry.1.iter_mut().zip(&calls) {
                match &c.arg {
                    None => acc.add_row(),
                    Some(a) => match eval_row(a, &j.cols, r, params)? {
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
        if groups.is_empty() && group_exprs.is_empty() {
            let k: Vec<Part> = Vec::new();
            order_seen.push(k.clone());
            groups.insert(k, (Vec::new(), projections.iter().map(Acc::new).collect(), 0));
        }
        for key in order_seen {
            let (vals, accs, rows) = &groups[&key];
            let look = |e: &Expr| -> Option<Result<Value>> {
                if let Some(i) = calls.iter().position(|c| c.text == e.to_string()) {
                    return Some(Ok(accs[i].value()));
                }
                if let Some(i) = group_exprs.iter().position(|g| g.to_string() == e.to_string() || same_column(g, e, &j.cols)) {
                    return Some(Ok(vals[i].clone()));
                }
                match e {
                    Expr::Identifier(_) | Expr::CompoundIdentifier(_) => Some(Err(err(format!("{e} must be in GROUP BY or inside an aggregate")))),
                    _ => None,
                }
            };
            if let Some(h) = &s.having {
                let h = having_with_aliases(h, &items);
                if !truthy(&eval(&h, params, &look)?) {
                    continue;
                }
            }
            let row: Vec<Value> = items.iter().map(|(_, e)| eval(e, params, &look)).collect::<Result<_>>()?;
            let sort: Vec<Value> = order.iter().map(|(e, _)| by_alias(e, &row).map_or_else(|| eval(e, params, &look), Ok)).collect::<Result<_>>()?;
            // The group's rows stand in the score until the order is set.
            out.push((row, sort, Value::Null, *rows as f64));
        }
    } else {
        for (i, r) in j.rows.iter().enumerate() {
            let row: Vec<Value> = items.iter().map(|(_, e)| eval_row(e, &j.cols, r, params)).collect::<Result<_>>()?;
            let sort: Vec<Value> = order.iter().map(|(e, _)| by_alias(e, &row).map_or_else(|| eval_row(e, &j.cols, r, params), Ok)).collect::<Result<_>>()?;
            out.push((row, sort, j.keys[i].clone(), j.scores[i]));
        }
    }
    if s.distinct.is_some() {
        let mut seen = HashSet::new();
        out.retain(|(row, ..)| seen.insert(row.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>()));
    }
    if !order.is_empty() {
        out.sort_by(|a, b| {
            for (i, (_, desc)) in order.iter().enumerate() {
                let c = agg::cmp(&a.1[i], &b.1[i]);
                let c = if *desc { c.reverse() } else { c };
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
    } else if grouped {
        // As the executor: the largest groups first, then by value.
        out.sort_by(|a, b| b.3.total_cmp(&a.3).then_with(|| a.0.iter().zip(&b.0).map(|(x, y)| agg::cmp(x, y)).find(|c| c.is_ne()).unwrap_or(Ordering::Equal)));
    }
    let out: Vec<_> = out.into_iter().skip(offset).take(limit.unwrap_or(usize::MAX)).collect();
    let n = out.len();
    let mut rel = Rel { columns: items.into_iter().map(|i| i.0).collect(), rows: Vec::with_capacity(n), keys: None, scores: Vec::with_capacity(n) };
    let mut keys = Vec::with_capacity(n);
    for (row, _, k, sc) in out {
        rel.rows.push(row);
        keys.push(k);
        rel.scores.push(sc);
    }
    if grouped {
        rel.scores.clear();
    }
    // Rows of one table keep their keys, so a WITH of them filters the table.
    if !joined && !grouped && keys.iter().all(|k| !matches!(k, Value::Null)) {
        rel.keys = Some(keys);
    }
    Ok(rel)
}

/// Whether `a` and `b` name the same column.
fn same_column(a: &Expr, b: &Expr, cols: &[(String, String)]) -> bool {
    let at = |e: &Expr| -> Option<usize> {
        let (alias, name) = match e {
            Expr::CompoundIdentifier(ids) if ids.len() >= 2 => (Some(ids[ids.len() - 2].value.to_ascii_lowercase()), &ids.last().unwrap().value),
            Expr::Identifier(id) => (None, &id.value),
            _ => return None,
        };
        let hits: Vec<usize> = (0..cols.len()).filter(|&i| cols[i].1.eq_ignore_ascii_case(name) && alias.as_ref().is_none_or(|x| *x == cols[i].0)).collect();
        if let [i] = hits.as_slice() { Some(*i) } else { None }
    };
    matches!((at(a), at(b)), (Some(x), Some(y)) if x == y)
}

/// HAVING may name selected aggregates by their alias.
fn having_with_aliases(h: &Expr, items: &[(String, Expr)]) -> Expr {
    let mut h = h.clone();
    fn go(e: &mut Expr, items: &[(String, Expr)]) {
        match e {
            Expr::Identifier(id) => {
                if let Some((_, x)) = items.iter().find(|(n, x)| n.eq_ignore_ascii_case(&id.value) && matches!(x, Expr::Function(_))) {
                    *e = x.clone();
                }
            }
            Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } => go(x, items),
            Expr::BinaryOp { left, right, .. } => {
                go(left, items);
                go(right, items);
            }
            Expr::Between { expr, low, high, .. } => {
                go(expr, items);
                go(low, items);
                go(high, items);
            }
            _ => {}
        }
    }
    go(&mut h, items);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_are_three_valued_and_integers_exact() {
        let params = Params::new().set("min", Value::Int(i64::MIN)).set("n", Value::Null);
        let ev = |src: &str| {
            let e = sqlparser::parser::Parser::new(&sqlparser::dialect::GenericDialect {}).try_with_sql(src).unwrap().parse_expr().unwrap();
            eval(&e, &params, &|_| None).unwrap()
        };
        for (src, want) in [
            ("NULL = 1", Value::Null),
            ("NOT (:n = 1)", Value::Null),
            ("NOT (:n = 1) OR 1 = 1", Value::Int(1)),
            ("NULL AND 1 = 0", Value::Int(0)),
            ("NULL AND 1 = 1", Value::Null),
            ("NULL OR 1 = 0", Value::Null),
            ("2 IN (1, NULL)", Value::Null),
            ("2 NOT IN (1, NULL)", Value::Null),
            ("1 IN (1, NULL)", Value::Int(1)),
            ("NULL BETWEEN 1 AND 2", Value::Null),
            ("9007199254740993 > 9007199254740992.0", Value::Int(1)),
            ("9007199254740992.0 < 9007199254740993", Value::Int(1)),
            ("9007199254740993 = 9007199254740992.0", Value::Int(0)),
            // Overflowing integer arithmetic gives a FLOAT, never a panic
            // or a wrapped value.
            ("9223372036854775807 + 1", Value::Float(9_223_372_036_854_775_808.0)),
            ("-:min", Value::Float(9_223_372_036_854_775_808.0)),
            ("abs(:min)", Value::Float(9_223_372_036_854_775_808.0)),
            (":min % -1", Value::Int(0)),
            (":min * 2", Value::Float(-18_446_744_073_709_551_616.0)),
        ] {
            assert_eq!(ev(src), want, "{src}");
        }
    }

    #[test]
    fn like_patterns() {
        let l = |s: &str, p: &str| like(&s.chars().collect::<Vec<_>>(), &p.chars().collect::<Vec<_>>());
        assert!(l("graph neural", "graph%"));
        assert!(l("graph", "gr_ph"));
        assert!(!l("graph", "gr_h"));
        assert!(l("", "%"));
        assert!(l("abcabd", "%ab_"));
        assert!(l("mississippi", "m%ss%pi"));
        assert!(!l("mississippi", "m%ss%px"));
        assert!(l("a%b", "a%%b"));
        assert!(!l("ab", "a"));
        assert!(!l("", "_"));
        // A long pattern used to recurse once per character (and `%` runs
        // to backtrack exponentially).
        let s = "a".repeat(50_000);
        assert!(!l(&s, &format!("{}b", "%a".repeat(25_000))));
        assert!(l(&s, &"_".repeat(50_000)));
    }
}
