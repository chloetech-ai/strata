//! Compound queries: `WITH`, `IN (SELECT ...)`, `EXISTS`, scalar
//! subqueries, `FROM (SELECT ...)`, `SELECT DISTINCT`, and joins of tables
//! (a table with itself, with another table of the database, or with a
//! `WITH` result).
//!
//! A compound query ([`is_compound`]) plans into one plan tree ([`plan`]):
//! the single-table queries the executor answers are its leaves, each bound
//! from a syntax tree built here, and joins, `WITH`, subqueries and
//! `DISTINCT` are operators over them ([`crate::query::ir`]), which
//! [`crate::query::ops`] runs. A subquery runs first and its result reaches
//! the outer query as a list of values (`IN (...)`), so every index still
//! serves the outer filter. A join scans each side with the conditions that
//! name only that side, the right side restricted to the join values the
//! left side holds (`b.id IN (...)`, or for an EDGES field the rows linking
//! to them), and joins, filters, groups, orders and cuts the rows in memory.
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
//! a links to), within one table, whose keys the links name. Text
//! searches, scores and graph predicates name one side.
//!
//! The parts of a query may read any tables of the database
//! ([`plan_in`]): each leaf names its table, and a join or subquery across
//! tables runs as one within a table does.

use std::cell::RefCell;
use std::collections::HashSet;

use sqlparser::ast::{self as sql, BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, SelectItem, TableFactor, UnaryOperator};

use crate::error::{Error, Result};
use crate::query::exec::{ExecOptions, ResultSet};
use crate::query::ir::eval;
use crate::query::ir::{self, ArithOp, CmpOp, Func, When};
use crate::query::ops;
use crate::query::sql::{Sub, Subs};
use crate::query::{AggFn, Params, Projection};
use crate::schema::{FieldType, Schema};
use crate::value::Value;

/// Rows one part of a compound query reads at most (a side of a join, a
/// subquery, a `WITH` result).
pub const MAX_ROWS: usize = 1_000_000;

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
    super::sql::table_name(name).eq_ignore_ascii_case(&schema.table)
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

/// The plan of a compound SELECT ([`is_compound`]) over `schema`'s table:
/// its single-table queries are leaves, and its joins, `WITH`, subqueries
/// and `DISTINCT` operators over them.
pub fn plan(sql_text: &str, schema: &Schema, params: &Params) -> Result<ir::Plan> {
    plan_in(sql_text, &[schema], params)
}

/// [`plan`] of a query that may read any of the tables of `catalog`.
pub fn plan_in(sql_text: &str, catalog: &[&Schema], params: &Params) -> Result<ir::Plan> {
    plan_query(&*parse(sql_text)?, catalog, params)
}

/// [`plan_in`] of a parsed query.
pub(crate) fn plan_query(q: &sql::Query, catalog: &[&Schema], params: &Params) -> Result<ir::Plan> {
    let p = Planner { catalog, params, ctes: RefCell::new(Vec::new()) };
    Ok(ir::Plan::new(p.query(q)?.node))
}

/// Runs a compound SELECT: its [`plan`], each leaf run by `runner`.
pub fn run(sql_text: &str, schema: &Schema, params: &Params, runner: &mut ops::Runner) -> Result<ResultSet> {
    run_checked(sql_text, schema, params, runner, &ExecOptions::default())
}

/// [`run`] under `opts`' deadline, cancel flag and memory budget
/// ([`ops::run`]). The leaves run under whatever options `runner` gives
/// them.
pub fn run_checked(sql_text: &str, schema: &Schema, params: &Params, runner: &mut ops::Runner, opts: &ExecOptions) -> Result<ResultSet> {
    ops::run(&plan(sql_text, schema, params)?, schema, runner, opts)
}

/// A query's plan, its columns, and the table whose rows its rows are
/// (with their keys), which a `WITH` of them keeps: a SELECT of a table's
/// fields, not of aggregates.
#[derive(Clone, Debug)]
struct Planned {
    node: ir::Node,
    columns: Vec<String>,
    keyed: Option<String>,
}

/// A `WITH` result in scope.
#[derive(Clone, Debug)]
struct Cte {
    name: String,
    columns: Vec<String>,
    keyed: Option<String>,
}

struct Planner<'a> {
    /// The tables a query may read.
    catalog: &'a [&'a Schema],
    params: &'a Params,
    /// The `WITH` results in scope, the innermost last.
    ctes: RefCell<Vec<Cte>>,
}

/// One table of a join.
struct Src<'a> {
    alias: String,
    kind: SrcKind<'a>,
    /// How it joins the tables before it (none for the first).
    join: Option<(ir::JoinKind, Expr)>,
}

enum SrcKind<'a> {
    /// A table, scanned with the conditions that name only this side.
    Table(&'a Schema),
    /// Rows of a table a `WITH` result holds (by its name), scanned
    /// likewise.
    TableRows(String, &'a Schema),
    /// Rows held in memory (a `WITH` result of aggregates, a derived table).
    Rows(Planned),
}

impl<'a> SrcKind<'a> {
    /// The table it scans; none for rows in memory.
    fn schema(&self) -> Option<&'a Schema> {
        match self {
            SrcKind::Table(s) | SrcKind::TableRows(_, s) => Some(s),
            SrcKind::Rows(_) => None,
        }
    }
}

fn pk(schema: &Schema) -> Result<usize> {
    schema.primary_key.ok_or_else(|| err(format!("this needs a primary key, and {} has none", schema.table)))
}

fn is_edges(schema: Option<&Schema>, column: &str) -> bool {
    schema.and_then(|s| s.field(column)).is_some_and(|f| f.ty == FieldType::Edges)
}

/// An ORDER BY key: its expression, DESC, and NULLS FIRST.
type OrderKey = (Expr, bool, bool);

impl<'a> Planner<'a> {
    fn cte(&self, name: &str) -> Option<Cte> {
        self.ctes.borrow().iter().rev().find(|c| c.name.eq_ignore_ascii_case(name)).cloned()
    }

    /// The table named `name` (any case).
    fn table(&self, name: &str) -> Option<&'a Schema> {
        self.catalog.iter().find(|s| s.table.eq_ignore_ascii_case(name)).copied()
    }

    fn query(&self, q: &sql::Query) -> Result<Planned> {
        let mark = self.ctes.borrow().len();
        let r = self.scoped(q);
        self.ctes.borrow_mut().truncate(mark);
        r
    }

    /// [`Planner::query`], its `WITH` results in scope until it returns.
    fn scoped(&self, q: &sql::Query) -> Result<Planned> {
        let mut defs = Vec::new();
        if let Some(w) = &q.with {
            if w.recursive {
                return Err(err("WITH RECURSIVE is not supported: REACHABLE and PATH traverse links"));
            }
            for cte in &w.cte_tables {
                let mut p = self.query(&cte.query)?;
                if !cte.alias.columns.is_empty() {
                    if cte.alias.columns.len() != p.columns.len() {
                        return Err(err(format!("WITH {}: {} column names for {} columns", cte.alias.name, cte.alias.columns.len(), p.columns.len())));
                    }
                    let columns: Vec<String> = cte.alias.columns.iter().map(|c| c.name.value.clone()).collect();
                    let exprs = columns.iter().enumerate().map(|(i, name)| ir::Column { name: name.clone(), expr: ir::Expr::Output(i as u32) }).collect();
                    p = Planned { node: ir::Node::Project { input: Box::new(p.node), columns: exprs }, columns, keyed: None };
                }
                let name = cte.alias.name.value.clone();
                self.ctes.borrow_mut().push(Cte { name: name.clone(), columns: p.columns, keyed: p.keyed });
                defs.push(ir::Cte { name, input: p.node });
            }
        }
        let mut q = q.clone();
        q.with = None;
        let body = self.body(&q)?;
        Ok(match defs.is_empty() {
            true => body,
            false => Planned { node: ir::Node::With { ctes: defs, input: Box::new(body.node) }, ..body },
        })
    }

    fn body(&self, q: &sql::Query) -> Result<Planned> {
        let sql::SetExpr::Select(s) = q.body.as_ref() else { return Err(err("UNION, INTERSECT and EXCEPT are not supported")) };
        if s.from.len() != 1 {
            return Err(err("FROM takes one table; join others with JOIN ... ON"));
        }
        if !s.from[0].joins.is_empty() {
            return self.join(q, s);
        }
        match &s.from[0].relation {
            TableFactor::Table { name, .. } if self.cte(&super::sql::table_name(name)).is_some() => {
                let cte = self.cte(&super::sql::table_name(name)).expect("in scope");
                match cte.keyed.as_deref().and_then(|t| self.table(t)) {
                    Some(schema) => self.over_rows(q, s, &cte, schema),
                    None => self.join(q, s),
                }
            }
            TableFactor::Table { name, .. } => match self.table(&super::sql::table_name(name)) {
                Some(schema) => self.single(q, s, schema),
                None => Err(err(format!("no such table: {}", super::sql::table_name(name)))),
            },
            TableFactor::Derived { .. } => self.join(q, s),
            t => Err(err(format!("unsupported FROM: {t}"))),
        }
    }

    /// A SELECT over a table: a leaf, its subqueries planned within it.
    fn single(&self, q: &sql::Query, s: &sql::Select, schema: &Schema) -> Result<Planned> {
        let mut q = q.clone();
        let mut s = s.clone();
        if s.distinct.is_some() {
            distinct_to_group(&mut s)?;
        }
        q.body = Box::new(sql::SetExpr::Select(Box::new(s)));
        self.leaf(&q, "query", schema)
    }

    /// Binds a leaf's syntax tree on `schema`'s table; a subquery in a
    /// condition or as seeds plans into the condition
    /// ([`Planner::subquery`]).
    fn leaf(&self, q: &sql::Query, what: &str, schema: &Schema) -> Result<Planned> {
        // A subquery's error names the subquery, not each query around it.
        let at = |e: Error| match e {
            Error::Query(m) if !m.contains(" (in ") => err(format!("{m} (in {what}: {})", printable(q))),
            e => e,
        };
        let subs = |sq: &sql::Query, kind: Sub| self.subquery(sq, kind, Some(schema));
        let select = super::sql::bind_query_with(q, schema, self.params, &subs as &Subs).map_err(at)?;
        let aggregated = !select.group_by.is_empty() || select.projection.iter().any(|(_, p)| matches!(p, Projection::Count | Projection::Agg { .. }));
        let keyed = !aggregated
            && select.projection.iter().all(|(name, p)| match p {
                Projection::Field(f) => schema.fields[*f].name.eq_ignore_ascii_case(name),
                Projection::Score | Projection::Hops(_) => true,
                _ => false,
            });
        let columns = select.projection.iter().map(|(n, _)| n.clone()).collect();
        Ok(Planned { node: ir::Plan::of_select(schema, &select).root, columns, keyed: keyed.then(|| schema.table.clone()) })
    }

    /// The expression a subquery stands for: its one column's values (`IN`),
    /// whether it has a row (`EXISTS`), its one value (a comparison), or the
    /// keys of its rows (seeds; `None` for a subquery of the scanned table
    /// `scanned`, which the executor runs itself).
    fn subquery(&self, q: &sql::Query, kind: Sub, scanned: Option<&Schema>) -> Result<Option<ir::Expr>> {
        if kind == Sub::Seeds && scanned.is_some_and(|s| !needs(q, s)) {
            return Ok(None);
        }
        let p = self.query(q)?;
        let one = |what: &str| match p.columns.len() {
            1 => Ok(()),
            n => Err(err(format!("{what} returns one column, not {n}"))),
        };
        let node = Box::new(p.node.clone());
        Ok(Some(match kind {
            Sub::In => {
                one("a subquery in IN")?;
                ir::Expr::Values(node)
            }
            Sub::Exists => ir::Expr::Exists(node),
            Sub::Scalar => {
                one("a scalar subquery")?;
                ir::Expr::Scalar(node)
            }
            Sub::Seeds if p.keyed.is_some() => ir::Expr::Keys(node),
            Sub::Seeds => {
                one("a subquery of seeds")?;
                ir::Expr::Values(node)
            }
        }))
    }

    /// A subquery of an expression over rows in memory.
    fn in_memory(&self, q: &sql::Query, kind: Sub) -> Result<ir::Expr> {
        self.subquery(q, kind, None)?.ok_or_else(|| err("a traversal's seeds name one table's rows, in its scan"))
    }

    /// A SELECT over the rows of a table that a `WITH` result holds: the
    /// table, restricted to their keys, in their order.
    fn over_rows(&self, q: &sql::Query, s: &sql::Select, cte: &Cte, schema: &Schema) -> Result<Planned> {
        let pk = pk(schema)?;
        let mut q = q.clone();
        let mut s = s.clone();
        let TableFactor::Table { name, .. } = &mut s.from[0].relation else { unreachable!("a table") };
        *name = sql::ObjectName::from(vec![sql::Ident::new(schema.table.clone())]);
        let grouped = !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty()) || has_aggregate(&s) || s.distinct.is_some();
        // Without an ORDER BY, rows keep the WITH result's order, so the
        // cut happens after reordering.
        let reorder = q.order_by.is_none() && !grouped;
        let cut = if reorder { q.limit_clause.take() } else { None };
        let mut p = self.single(&q, &s, schema)?;
        let rows = ir::Node::Ref { name: cte.name.clone() };
        restrict(&mut p.node, ir::Expr::In { expr: Box::new(ir::Expr::Field(pk as u32)), list: vec![ir::Expr::Keys(Box::new(rows.clone()))] });
        if reorder {
            p.node = ir::Node::KeyOrder { input: Box::new(p.node), by: Box::new(rows) };
            let (offset, limit) = limits(&cut, self.params)?;
            if offset > 0 || limit.is_some() {
                p.node = ir::Node::Limit { input: Box::new(p.node), offset: offset as u64, limit: limit.map(|l| l as u64) };
            }
        }
        Ok(p)
    }

    fn source(&self, t: &TableFactor) -> Result<(String, SrcKind<'a>)> {
        let alias_of = |a: &Option<sql::TableAlias>, d: &str| a.as_ref().map_or(d.to_string(), |a| a.name.value.clone());
        match t {
            TableFactor::Table { name, alias, .. } => {
                let n = super::sql::table_name(name);
                if let Some(cte) = self.cte(&n) {
                    let kind = match cte.keyed.as_deref().and_then(|t| self.table(t)) {
                        Some(schema) => SrcKind::TableRows(cte.name, schema),
                        None => SrcKind::Rows(Planned { node: ir::Node::Ref { name: cte.name }, columns: cte.columns, keyed: None }),
                    };
                    return Ok((alias_of(alias, &n), kind));
                }
                let schema = self.table(&n).ok_or_else(|| err(format!("no such table: {n}")))?;
                Ok((alias_of(alias, &n), SrcKind::Table(schema)))
            }
            TableFactor::Derived { subquery, alias, .. } => {
                let p = self.query(subquery)?;
                let alias = alias.as_ref().map(|a| a.name.value.clone()).ok_or_else(|| err("a subquery in FROM needs an alias: FROM (SELECT ...) AS t"))?;
                Ok((alias, SrcKind::Rows(Planned { keyed: None, ..p })))
            }
            t => Err(err(format!("unsupported FROM: {t}"))),
        }
    }

    /// Joins (and any SELECT over rows held in memory).
    fn join(&self, q: &sql::Query, s: &sql::Select) -> Result<Planned> {
        let order: Vec<sql::OrderByExpr> = match &q.order_by {
            None => Vec::new(),
            Some(ob) => match &ob.kind {
                sql::OrderByKind::Expressions(es) => es.clone(),
                _ => return Err(err("ORDER BY ALL is not supported")),
            },
        };
        let (offset, limit) = limits(&q.limit_clause, self.params)?;

        // The tables.
        let twj = &s.from[0];
        let mut srcs = Vec::new();
        let (alias, kind) = self.source(&twj.relation)?;
        srcs.push(Src { alias, kind, join: None });
        for j in &twj.joins {
            let (kind, c) = match &j.join_operator {
                sql::JoinOperator::Join(c) | sql::JoinOperator::Inner(c) => (ir::JoinKind::Inner, c),
                sql::JoinOperator::Left(c) | sql::JoinOperator::LeftOuter(c) => (ir::JoinKind::Left, c),
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
        let cols_of: Vec<Vec<String>> = srcs.iter().map(|s| columns_of(s)).collect();
        let aliases: Vec<String> = srcs.iter().map(|s| s.alias.to_ascii_lowercase()).collect();
        let vectors: Vec<Vec<String>> =
            srcs.iter().map(|s| s.kind.schema().map_or_else(Vec::new, |t| t.fields.iter().filter(|f| matches!(f.ty, FieldType::Vector { .. })).map(|f| f.name.clone()).collect())).collect();
        let sides = Sides { aliases: &aliases, cols: &cols_of, vectors: &vectors };
        let in_memory = |i: usize| matches!(srcs[i].kind, SrcKind::Rows(_));
        let table = |i: usize| srcs[i].kind.schema();

        // WHERE: each condition naming one scanned side goes to its scan.
        let mut pushed: Vec<Vec<Expr>> = vec![Vec::new(); srcs.len()];
        let mut residual: Vec<Expr> = Vec::new();
        for c in s.selection.iter().flat_map(split_and) {
            let at = sides.of(&c)?;
            let left_join_right = |i: usize| srcs[i].join.as_ref().is_some_and(|(k, _)| *k == ir::JoinKind::Left);
            match at.as_slice() {
                [i] if !in_memory(*i) && !left_join_right(*i) => pushed[*i].push(c),
                _ => residual.push(c),
            }
        }
        // ON: equalities with the tables before become join keys; conditions
        // naming only the new table go to its scan.
        let mut keys: Vec<Vec<(Expr, String)>> = vec![Vec::new(); srcs.len()];
        let mut on_rest: Vec<Vec<Expr>> = vec![Vec::new(); srcs.len()];
        for i in 1..srcs.len() {
            let (_, on) = srcs[i].join.clone().expect("a joined table");
            for c in split_and(&on) {
                let at = sides.of(&c)?;
                if let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = &c {
                    let (l, r) = (sides.of(left)?, sides.of(right)?);
                    let col = |e: &Expr| sides.column(e).ok().flatten();
                    if r == [i] && l.iter().all(|&x| x < i) && !l.is_empty() && col(right).is_some() {
                        keys[i].push(((**left).clone(), col(right).expect("a column").1));
                        continue;
                    }
                    if l == [i] && r.iter().all(|&x| x < i) && !r.is_empty() && col(left).is_some() {
                        keys[i].push(((**right).clone(), col(left).expect("a column").1));
                        continue;
                    }
                }
                if at == [i] && !in_memory(i) {
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
            // A scanned side is restricted to the left's join values.
            for (le, c) in &keys[i] {
                if !in_memory(i) && !is_edges(table(i), c) && !table(i).and_then(|t| t.field(c)).is_some_and(|f| matches!(f.ty, FieldType::Keyword | FieldType::Int | FieldType::Float)) {
                    return Err(err(format!("joins compare KEYWORD, INT or FLOAT fields, the key or EDGES links, not {c}")));
                }
                // Keys of two kinds never equal (INT and FLOAT compare as numbers).
                if let (Some(rf), Some((j, lc))) = (table(i).and_then(|t| t.field(c)), sides.column(le)?) {
                    if let Some(lf) = table(j).and_then(|t| t.field(&lc)) {
                        let kind = |f: &crate::schema::Field| match (f.logical, &f.ty) {
                            (Some(l), _) => l.type_name(),
                            (None, FieldType::Int | FieldType::Float) => "number",
                            _ => f.type_name(),
                        };
                        if lf.ty != FieldType::Edges && rf.ty != FieldType::Edges && kind(lf) != kind(rf) {
                            return Err(err(format!(
                                "{le} = {}.{c}: a {} field never equals a {} field; join keys are of one type (INT and FLOAT compare as numbers)",
                                srcs[i].alias,
                                lf.type_name(),
                                rf.type_name()
                            )));
                        }
                    }
                }
                // Links name keys of their own table.
                let left = sides.column(le)?.map(|(j, lc)| (j, is_edges(table(j), &lc)));
                let (Some(t), Some((j, left_links))) = (table(i), left) else { continue };
                let Some(u) = table(j).filter(|u| !u.table.eq_ignore_ascii_case(&t.table)) else { continue };
                if left_links || is_edges(Some(t), c) {
                    let (links, keys) = if left_links { (u, t) } else { (t, u) };
                    return Err(err(format!(
                        "{le} = {}.{c}: an EDGES field of {} links rows of {} by their keys, not rows of {}",
                        srcs[i].alias, links.table, links.table, keys.table
                    )));
                }
            }
        }

        // ORDER BY a score: the scan of its table ranks by it.
        let mut scan_order: Vec<Option<OrderKey>> = vec![None; srcs.len()];
        let mut mem_order: Vec<OrderKey> = Vec::new();
        for o in &order {
            let desc = o.options.asc == Some(false);
            let nulls_first = o.options.nulls_first == Some(true);
            if is_score(&o.expr) {
                let at = sides.of(&o.expr)?;
                let [i] = at.as_slice() else { return Err(err(format!("ORDER BY {}: a score ranks one table's rows", o.expr))) };
                if in_memory(*i) || scan_order[*i].is_some() {
                    return Err(err(format!("ORDER BY {}: one score per scanned table", o.expr)));
                }
                scan_order[*i] = Some((o.expr.clone(), desc, false));
                mem_order.push((qualified(&srcs[*i].alias, "__score"), true, false));
            } else {
                mem_order.push((o.expr.clone(), desc, nulls_first));
            }
        }
        let grouped = !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty()) || has_aggregate(s) || s.having.is_some();
        // The first table drives: when the order is its own, its rows come
        // in that order and keep it through the joins, so a cut needs only
        // as many of them as join into enough rows ([`ops`] grows its scan).
        let driver_order = !grouped
            && s.distinct.is_none()
            && match order.first() {
                None => true,
                Some(_) if mem_order.len() > 1 => false,
                // The executor orders by a score or a numeric field.
                Some(o) => {
                    scan_order[0].is_some()
                        || sides.column(&o.expr)?.is_some_and(|(i, c)| i == 0 && table(0).and_then(|t| t.field(&c)).is_some_and(|f| matches!(f.ty, FieldType::Int | FieldType::Float) && !f.multi))
                }
            };
        if driver_order && order.len() == 1 && scan_order[0].is_none() {
            scan_order[0] = Some(mem_order[0].clone());
        }
        let needed = sides.needed(s, &order, &srcs)?;

        // The scans, joined left to right: rows of `cols` (alias, column),
        // multi-valued where `multi` says.
        let mut node: Option<ir::Node> = None;
        let mut cols: Vec<(String, String)> = Vec::new();
        let mut multi: Vec<bool> = Vec::new();
        let subs = |q: &sql::Query, kind: Sub| self.in_memory(q, kind);
        for (i, src) in srcs.iter().enumerate() {
            let side = self.scan(src, &needed[i], &pushed[i], &scan_order[i], if i == 0 { "scan" } else { "join" })?;
            let alias = src.alias.to_ascii_lowercase();
            let side_cols: Vec<(String, String)> = needed[i].iter().map(|c| (alias.clone(), c.clone())).collect();
            let side_multi: Vec<bool> = needed[i].iter().map(|c| table(i).and_then(|t| t.field(c)).is_some_and(|f| f.multi)).collect();
            let Some(left) = node.take() else {
                node = Some(side);
                cols = side_cols;
                multi = side_multi;
                continue;
            };
            let (kind, _) = src.join.clone().expect("a joined table");
            let join_keys = keys[i]
                .iter()
                .map(|(le, rc)| {
                    let right = needed[i].iter().position(|x| x.eq_ignore_ascii_case(rc)).expect("key column scanned");
                    Ok(ir::JoinKey {
                        left: compile_row(le, &cols, &multi, self.params, &subs)?,
                        right: ir::Expr::Output(right as u32),
                        left_links: sides.column(le)?.is_some_and(|(j, c)| is_edges(table(j), &c)),
                        right_links: is_edges(table(i), rc),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            cols.extend(side_cols);
            multi.extend(side_multi);
            let on = and(compile_all(&on_rest[i], &cols, &multi, self.params, &subs)?);
            node = Some(ir::Node::Join { left: Box::new(left), right: Box::new(side), kind, keys: join_keys, on });
        }
        let mut node = node.expect("a table");
        // Conditions over several tables, or rows in memory.
        if let Some(cond) = and(compile_all(&residual, &cols, &multi, self.params, &subs)?) {
            node = ir::Node::Filter { input: Box::new(node), cond };
        }
        self.evaluate(s, node, &cols, &multi, &mem_order, offset, limit, srcs.len() > 1, driver_order)
    }

    /// One side of a join: a leaf with its own conditions and ranking,
    /// returning the `columns` it needs, or rows in memory.
    fn scan(&self, src: &Src, columns: &[String], conds: &[Expr], order: &Option<OrderKey>, what: &str) -> Result<ir::Node> {
        let schema = match &src.kind {
            SrcKind::Table(t) | SrcKind::TableRows(_, t) => *t,
            SrcKind::Rows(p) => {
                // Held in memory: pick the columns.
                let picked = columns
                    .iter()
                    .map(|c| {
                        let i = p.columns.iter().position(|x| x.eq_ignore_ascii_case(c)).ok_or_else(|| err(format!("{} has no column {c}", src.alias)))?;
                        Ok(ir::Column { name: c.clone(), expr: ir::Expr::Output(i as u32) })
                    })
                    .collect::<Result<_>>()?;
                return Ok(ir::Node::Project { input: Box::new(p.node.clone()), columns: picked });
            }
        };
        let alias = src.alias.to_ascii_lowercase();
        let column = |c: &str| Expr::Identifier(sql::Ident::new(c));
        let mut q = super::sql::parse_query("SELECT score() AS __score FROM t")?;
        let sql::SetExpr::Select(s) = q.body.as_mut() else { unreachable!("a SELECT") };
        let score = s.projection.pop().expect("score() AS __score");
        s.projection = columns.iter().map(|c| if c == "__score" { score.clone() } else { SelectItem::UnnamedExpr(column(c)) }).collect();
        if let TableFactor::Table { name, .. } = &mut s.from[0].relation {
            *name = sql::ObjectName::from(vec![sql::Ident::new(schema.table.clone())]);
        }
        s.selection = conds.iter().map(|c| Expr::Nested(Box::new(unqualify(c, &alias)))).reduce(|a, b| Expr::BinaryOp { left: Box::new(a), op: BinaryOperator::And, right: Box::new(b) });
        let ranked = order.as_ref().is_some_and(|(e, ..)| is_score(e));
        if let Some((e, desc, nulls_first)) = order {
            let options = sql::OrderByOptions { asc: desc.then_some(false), nulls_first: nulls_first.then_some(true) };
            q.order_by = Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(vec![sql::OrderByExpr { expr: unqualify(e, &alias), options, with_fill: None }]), interpolate: None });
        }
        // A ranked scan without a LIMIT would stop at the executor's default.
        if ranked {
            q.limit_clause = Some(sql::LimitClause::LimitOffset { limit: Some(Expr::value(sql::Value::Number((MAX_ROWS + 1).to_string(), false))), offset: None, limit_by: Vec::new() });
        }
        let mut p = self.leaf(&q, &format!("{what} {}", src.alias), schema)?;
        if let SrcKind::TableRows(cte, _) = &src.kind {
            let rows = ir::Expr::Keys(Box::new(ir::Node::Ref { name: cte.clone() }));
            restrict(&mut p.node, ir::Expr::In { expr: Box::new(ir::Expr::Field(pk(schema)? as u32)), list: vec![rows] });
        }
        Ok(p.node)
    }
}

/// Columns a side offers.
fn columns_of(src: &Src) -> Vec<String> {
    match &src.kind {
        SrcKind::Rows(p) => p.columns.clone(),
        SrcKind::Table(t) | SrcKind::TableRows(_, t) => t.fields.iter().map(|f| f.name.clone()).chain(["__score".to_string()]).collect(),
    }
}

/// Adds condition `c` to a leaf's Search.
fn restrict(leaf: &mut ir::Node, c: ir::Expr) {
    let s = leaf.search_mut().expect("a leaf");
    s.filter = match std::mem::replace(&mut s.filter, ir::Expr::Lit(Value::Bool(true))) {
        ir::Expr::Lit(Value::Bool(true)) => c,
        ir::Expr::And(mut xs) => {
            xs.insert(0, c);
            ir::Expr::And(xs)
        }
        f => ir::Expr::And(vec![c, f]),
    };
}

/// The conditions as one, `None` without any.
fn and(mut cs: Vec<ir::Expr>) -> Option<ir::Expr> {
    match cs.len() {
        0 => None,
        1 => cs.pop(),
        _ => Some(ir::Expr::And(cs)),
    }
}

/// Which tables an expression names, and column lookup.
struct Sides<'s> {
    aliases: &'s [String],
    cols: &'s [Vec<String>],
    /// Each table's VECTOR fields, which `*` leaves out.
    vectors: &'s [Vec<String>],
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
        c != "__score" && (matches!(srcs[i].kind, SrcKind::Rows(_)) || !self.vectors[i].iter().any(|v| v == c))
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
                    let a = super::sql::table_name(n).to_ascii_lowercase();
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

/// `q` as SQL that parses back to it, as an error names it: a column
/// named like a keyword, or other than a plain identifier, quoted
/// (`"order"`, `"doc id"`), and the operand of a sign in parentheses when
/// it starts with a sign itself (`- -1` would print as `--1`, which
/// starts a comment).
fn printable(q: &sql::Query) -> String {
    let mut q = q.clone();
    query_mut(&mut q, &mut |e| match e {
        Expr::Identifier(id) => quote(id),
        Expr::CompoundIdentifier(ids) => ids.iter_mut().for_each(quote),
        Expr::UnaryOp { expr, .. } if expr.to_string().starts_with(['-', '+']) => {
            let x = std::mem::replace(expr.as_mut(), Expr::value(sql::Value::Null));
            **expr = Expr::Nested(Box::new(x));
        }
        _ => {}
    });
    q.to_string()
}

fn quote(id: &mut sql::Ident) {
    let plain = id.value.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && id.value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if id.quote_style.is_none() && (!plain || sqlparser::keywords::ALL_KEYWORDS.contains(&id.value.to_ascii_uppercase().as_str())) {
        id.quote_style = Some('"');
    }
}

/// Calls `f` on every expression of `q`'s SELECT, its subqueries' too,
/// each before its subexpressions.
fn query_mut(q: &mut sql::Query, f: &mut dyn FnMut(&mut Expr)) {
    if let Some(w) = &mut q.with {
        for cte in &mut w.cte_tables {
            query_mut(&mut cte.query, f);
        }
    }
    if let sql::SetExpr::Select(s) = q.body.as_mut() {
        for item in &mut s.projection {
            if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
                walk_mut(e, f);
            }
        }
        for t in &mut s.from {
            if let TableFactor::Derived { subquery, .. } = &mut t.relation {
                query_mut(subquery, f);
            }
        }
        s.selection.iter_mut().for_each(|e| walk_mut(e, f));
        s.having.iter_mut().for_each(|e| walk_mut(e, f));
        if let sql::GroupByExpr::Expressions(es, _) = &mut s.group_by {
            es.iter_mut().for_each(|e| walk_mut(e, f));
        }
    }
    if let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &mut q.order_by {
        es.iter_mut().for_each(|o| walk_mut(&mut o.expr, f));
    }
}

/// [`walk`], changing the expressions, and into subqueries.
fn walk_mut(e: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
    f(e);
    match e {
        Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } | Expr::IsNull(x) | Expr::IsNotNull(x) | Expr::Cast { expr: x, .. } => walk_mut(x, f),
        Expr::BinaryOp { left, right, .. } => {
            walk_mut(left, f);
            walk_mut(right, f);
        }
        Expr::Between { expr, low, high, .. } => {
            walk_mut(expr, f);
            walk_mut(low, f);
            walk_mut(high, f);
        }
        Expr::InList { expr, list, .. } => {
            walk_mut(expr, f);
            list.iter_mut().for_each(|x| walk_mut(x, f));
        }
        Expr::InSubquery { expr, subquery, .. } => {
            walk_mut(expr, f);
            query_mut(subquery, f);
        }
        Expr::Exists { subquery: q, .. } | Expr::Subquery(q) => query_mut(q, f),
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            walk_mut(expr, f);
            walk_mut(pattern, f);
        }
        Expr::Function(func) => {
            for a in func_args_mut(func) {
                walk_mut(a, f);
            }
        }
        Expr::Case { operand, conditions, else_result, .. } => {
            operand.iter_mut().for_each(|x| walk_mut(x, f));
            for c in conditions {
                walk_mut(&mut c.condition, f);
                walk_mut(&mut c.result, f);
            }
            else_result.iter_mut().for_each(|x| walk_mut(x, f));
        }
        _ => {}
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
        match value(&compile(e, params, &|_| None, &[])?)? {
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

// ---- expressions over rows in memory -------------------------------------
//
// Expressions over rows in memory compile into the plan IR's expressions,
// each column (grouped: each group value and aggregate) an output of the
// node below, and the operators evaluate them on the IR's one three-valued
// evaluator, as HAVING and the single-table queries' conditions do.

/// Plans a subquery of an expression over rows in memory.
type InMemory<'s> = dyn Fn(&sql::Query, Sub) -> Result<ir::Expr> + 's;

/// The slot of a column in rows of joined tables; `None` when `e` is not a
/// column.
fn slot(e: &Expr, cols: &[(String, String)]) -> Option<Result<u32>> {
    let (alias, name) = match e {
        Expr::CompoundIdentifier(ids) if ids.len() >= 2 => (Some(ids[ids.len() - 2].value.to_ascii_lowercase()), &ids.last().unwrap().value),
        Expr::Identifier(id) => (None, &id.value),
        _ => return None,
    };
    let hits: Vec<usize> = (0..cols.len()).filter(|&i| cols[i].1.eq_ignore_ascii_case(name) && alias.as_ref().is_none_or(|a| *a == cols[i].0)).collect();
    Some(match hits.as_slice() {
        [i] => Ok(*i as u32),
        [] => Err(err(format!("no such column: {e}"))),
        _ => Err(err(format!("column {e} is in several tables: qualify it"))),
    })
}

/// `e` over rows of joined tables (`cols` and `multi` as [`Planner::join`]
/// keeps them).
fn compile_row(e: &Expr, cols: &[(String, String)], multi: &[bool], params: &Params, subs: &InMemory) -> Result<ir::Expr> {
    compile_with(e, params, &|x| at(slot(x, cols)), multi, Some(subs))
}

/// A slot as the column of rows in memory it is.
fn at(slot: Option<Result<u32>>) -> Option<Result<ir::Expr>> {
    slot.map(|r| r.map(ir::Expr::Output))
}

fn compile_all(es: &[Expr], cols: &[(String, String)], multi: &[bool], params: &Params, subs: &InMemory) -> Result<Vec<ir::Expr>> {
    es.iter().map(|e| compile_row(e, cols, multi, params, subs)).collect()
}

/// The value of an expression of no row.
fn value(e: &ir::Expr) -> Result<Value> {
    eval::eval(e, &|r| Err(err(format!("{r:?} is not a value of no row"))))
}

fn literal(v: &sql::Value, params: &Params) -> Result<Value> {
    Ok(match v {
        sql::Value::Number(s, _) => s.parse::<i64>().map(Value::Int).or_else(|_| s.parse::<f64>().map(Value::Float)).map_err(|_| err(format!("bad number {s}")))?,
        sql::Value::SingleQuotedString(s) | sql::Value::DoubleQuotedString(s) => Value::Str(s.clone()),
        sql::Value::Boolean(b) => Value::Bool(*b),
        sql::Value::Null => Value::Null,
        sql::Value::Placeholder(p) => params.0.get(p.trim_start_matches([':', '$', '@', '?'])).cloned().ok_or_else(|| err(format!("unbound parameter {p}")))?,
        v => return Err(err(format!("unsupported literal {v}"))),
    })
}

/// Compiles `e` into the plan IR's expression: `slot` gives what a column
/// (and, grouped, an aggregate or group value) is, a slot of rows in
/// memory or (in `SET`) a field of the row, and parameters become
/// literals. A multi-valued slot (`multi[slot]`) with no values compares
/// as an empty list, as the executor compares it: `tags = 'a'` is FALSE
/// there and `NOT (tags = 'a')` TRUE, not UNKNOWN.
pub(crate) fn compile(e: &Expr, params: &Params, slot: &dyn Fn(&Expr) -> Option<Result<ir::Expr>>, multi: &[bool]) -> Result<ir::Expr> {
    compile_with(e, params, slot, multi, None)
}

/// [`compile`], with the subqueries `subs` plans (over rows in memory).
fn compile_with(e: &Expr, params: &Params, slot: &dyn Fn(&Expr) -> Option<Result<ir::Expr>>, multi: &[bool], subs: Option<&InMemory>) -> Result<ir::Expr> {
    if let Some(x) = slot(e) {
        return x;
    }
    let go = |x: &Expr| compile_with(x, params, slot, multi, subs);
    let bx = |x: &Expr| go(x).map(Box::new);
    // An operand of a comparison.
    let listy = |x: &Expr| -> Result<Box<ir::Expr>> {
        Ok(Box::new(match go(x)? {
            // NULL as the empty list (coalesce would skip the empty list).
            x @ ir::Expr::Output(i) if multi.get(i as usize) == Some(&true) => ir::Expr::Case {
                operand: None,
                whens: vec![When { when: ir::Expr::IsNull(Box::new(x.clone())), then: ir::Expr::Lit(Value::List(Vec::new())) }],
                otherwise: Some(Box::new(x)),
            },
            x => x,
        }))
    };
    let not_if = |negated: bool, x: ir::Expr| if negated { ir::Expr::Not(Box::new(x)) } else { x };
    let sub = |q: &sql::Query, kind: Sub| match subs {
        Some(subs) => subs(q, kind),
        None => Err(err(format!("a subquery is not supported here: {e}"))),
    };
    Ok(match e {
        Expr::Nested(x) | Expr::UnaryOp { op: UnaryOperator::Plus, expr: x } => go(x)?,
        Expr::Value(v) => ir::Expr::Lit(literal(&v.value, params)?),
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => ir::Expr::Not(bx(expr)?),
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => ir::Expr::Neg(bx(expr)?),
        Expr::IsNull(x) => ir::Expr::IsNull(bx(x)?),
        Expr::IsNotNull(x) => ir::Expr::Not(Box::new(ir::Expr::IsNull(bx(x)?))),
        Expr::BinaryOp { left, op, right } => {
            use BinaryOperator as B;
            let cmp = match op {
                B::Eq => Some(CmpOp::Eq),
                B::NotEq => Some(CmpOp::Ne),
                B::Lt => Some(CmpOp::Lt),
                B::LtEq => Some(CmpOp::Le),
                B::Gt => Some(CmpOp::Gt),
                B::GtEq => Some(CmpOp::Ge),
                _ => None,
            };
            if let Some(op) = cmp {
                return Ok(ir::Expr::Cmp { op, left: listy(left)?, right: listy(right)? });
            }
            let arith = match op {
                B::Plus => Some(ArithOp::Add),
                B::Minus => Some(ArithOp::Sub),
                B::Multiply => Some(ArithOp::Mul),
                B::Divide => Some(ArithOp::Div),
                B::Modulo => Some(ArithOp::Mod),
                _ => None,
            };
            let (l, r) = (go(left)?, go(right)?);
            match (op, arith) {
                (_, Some(op)) => ir::Expr::Arith { op, left: Box::new(l), right: Box::new(r) },
                (B::And, _) => ir::Expr::And(vec![l, r]),
                (B::Or, _) => ir::Expr::Or(vec![l, r]),
                (B::StringConcat, _) => ir::Expr::Concat(vec![l, r]),
                (o, _) => return Err(err(format!("unsupported operator {o}"))),
            }
        }
        Expr::Between { expr, negated, low, high } => not_if(*negated, ir::Expr::Between { expr: listy(expr)?, lo: Some(bx(low)?), hi: Some(bx(high)?) }),
        Expr::InList { expr, list, negated } => not_if(*negated, ir::Expr::In { expr: listy(expr)?, list: list.iter().map(go).collect::<Result<_>>()? }),
        Expr::InSubquery { expr, subquery, negated } => not_if(*negated, ir::Expr::In { expr: listy(expr)?, list: vec![sub(subquery, Sub::In)?] }),
        Expr::Exists { subquery, negated } => not_if(*negated, sub(subquery, Sub::Exists)?),
        Expr::Subquery(q) => sub(q, Sub::Scalar)?,
        Expr::Like { negated, any, expr, pattern, escape_char } | Expr::ILike { negated, any, expr, pattern, escape_char } => {
            if *any || escape_char.is_some() {
                return Err(err(format!("unsupported expression: {e}")));
            }
            not_if(*negated, ir::Expr::Like { expr: bx(expr)?, pattern: bx(pattern)?, case_insensitive: matches!(e, Expr::ILike { .. }) })
        }
        Expr::Case { operand, conditions, else_result, .. } => ir::Expr::Case {
            operand: operand.as_deref().map(bx).transpose()?,
            whens: conditions.iter().map(|c| Ok(When { when: go(&c.condition)?, then: go(&c.result)? })).collect::<Result<_>>()?,
            otherwise: else_result.as_deref().map(bx).transpose()?,
        },
        Expr::Function(f) => {
            let func = match fname(f).as_str() {
                "lower" => Func::Lower,
                "upper" => Func::Upper,
                "length" => Func::Length,
                "abs" => Func::Abs,
                "round" => Func::Round,
                "coalesce" => Func::Coalesce,
                n if NATIVE.contains(&n) => {
                    return Err(err(format!("{}(...) works on one table's scan, in its WHERE or ORDER BY (in a join, on that table's column: a.body), not on values of rows", n.to_uppercase())))
                }
                n if AGGREGATES.contains(&n) => return Err(err(format!("{}(...) is an aggregate: it needs GROUP BY or a SELECT of aggregates, not a value of one row", n.to_uppercase()))),
                n => return Err(err(format!("unknown function {n}"))),
            };
            ir::Expr::Call { func, args: func_args(f).into_iter().map(go).collect::<Result<_>>()? }
        }
        e @ (Expr::Identifier(_) | Expr::CompoundIdentifier(_)) => return Err(err(format!("no such column: {e}"))),
        e => return Err(err(format!("unsupported expression: {e}"))),
    })
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

impl Planner<'_> {
    /// WHERE (already applied), GROUP BY, HAVING, ORDER BY, OFFSET, LIMIT
    /// and the projection, as operators over rows in memory. With `driver`
    /// the rows come in the order asked for (the first table's scan's).
    #[allow(clippy::too_many_arguments)]
    fn evaluate(&self, s: &sql::Select, input: ir::Node, cols: &[(String, String)], multi: &[bool], order: &[OrderKey], offset: usize, limit: Option<usize>, joined: bool, driver: bool) -> Result<Planned> {
        let params = self.params;
        let subs = |q: &sql::Query, kind: Sub| self.in_memory(q, kind);
        // The output columns.
        let mut items: Vec<(String, Expr)> = Vec::new();
        for item in &s.projection {
            match item {
                SelectItem::UnnamedExpr(e) => items.push((super::sql::name_of(e), e.clone())),
                SelectItem::ExprWithAlias { expr, alias } => items.push((alias.value.clone(), expr.clone())),
                SelectItem::Wildcard(_) => {
                    for (a, c) in cols.iter().filter(|(_, c)| c != "__score") {
                        items.push((if joined { format!("{a}.{c}") } else { c.clone() }, qualified(a, c)));
                    }
                }
                SelectItem::QualifiedWildcard(sql::SelectItemQualifiedWildcardKind::ObjectName(n), _) => {
                    let a = super::sql::table_name(n).to_ascii_lowercase();
                    for (_, c) in cols.iter().filter(|(x, c)| *x == a && c != "__score") {
                        items.push((format!("{a}.{c}"), qualified(&a, c)));
                    }
                }
                i => return Err(err(format!("unsupported select item {i}"))),
            }
        }
        // `score()`: the first ranked table's score.
        let score_col = cols.iter().find(|(_, c)| c == "__score").map(|(a, _)| qualified(a, "__score"));
        for it in items.iter_mut() {
            if matches!(&it.1, Expr::Function(f) if fname(f) == "score") {
                it.1 = score_col.clone().ok_or_else(|| err("score() needs ORDER BY a score (bm25, similarity, ppr, ...)"))?;
            }
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
                    Expr::Identifier(id) if slot(e, cols).is_none_or(|r| r.is_err()) => {
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
        // ORDER BY an output column's name or number, or an expression over
        // the input row.
        let by_alias = |e: &Expr| -> Option<usize> {
            match e {
                Expr::Identifier(id) => items.iter().position(|(n, _)| n.eq_ignore_ascii_case(&id.value)),
                Expr::Value(v) if matches!(v.value, sql::Value::Number(..)) => e.to_string().parse::<usize>().ok().filter(|n| (1..=items.len()).contains(n)).map(|n| n - 1),
                _ => None,
            }
        };
        let sort_keys = |items_at: &[ir::Expr], slot: &dyn Fn(&Expr) -> Option<Result<ir::Expr>>, multi: &[bool]| -> Result<Vec<ir::SortKey>> {
            order
                .iter()
                .map(|(e, desc, nulls_first)| {
                    let expr = match by_alias(e) {
                        Some(i) => items_at[i].clone(),
                        None => compile_with(e, params, slot, multi, Some(&subs))?,
                    };
                    Ok(ir::SortKey { expr, desc: *desc, nulls_first: *nulls_first })
                })
                .collect()
        };
        let mut node = input;
        let items_at: Vec<ir::Expr> = if grouped {
            // The Aggregate's rows: its group values, its aggregates, and
            // the rows of each group, which order groups by default.
            let k = group_exprs.len();
            let keys = group_exprs.iter().map(|g| compile_row(g, cols, multi, params, &subs)).collect::<Result<Vec<_>>>()?;
            let mut aggs: Vec<ir::Agg> = calls
                .iter()
                .map(|c| Ok(ir::Agg { func: c.func.unwrap_or(AggFn::Count), arg: c.arg.as_ref().map(|a| compile_row(a, cols, multi, params, &subs)).transpose()? }))
                .collect::<Result<_>>()?;
            aggs.push(ir::Agg { func: AggFn::Count, arg: None });
            node = ir::Node::Aggregate { input: Box::new(node), keys, aggs };
            let group_slot = |e: &Expr| -> Option<Result<ir::Expr>> {
                if let Some(i) = calls.iter().position(|c| c.text == e.to_string()) {
                    return Some(Ok(ir::Expr::Output((k + i) as u32)));
                }
                if let Some(i) = group_exprs.iter().position(|g| g.to_string() == e.to_string() || same_column(g, e, cols)) {
                    return Some(Ok(ir::Expr::Output(i as u32)));
                }
                match e {
                    Expr::Identifier(_) | Expr::CompoundIdentifier(_) => Some(Err(err(format!("{e} must be in GROUP BY or inside an aggregate")))),
                    _ => None,
                }
            };
            if let Some(h) = &s.having {
                let cond = compile_with(&having_with_aliases(h, &items), params, &group_slot, &[], Some(&subs))?;
                node = ir::Node::Filter { input: Box::new(node), cond };
            }
            let items_at: Vec<ir::Expr> = items.iter().map(|(_, e)| compile_with(e, params, &group_slot, &[], Some(&subs))).collect::<Result<_>>()?;
            let keys = match order.is_empty() {
                false => sort_keys(&items_at, &group_slot, &[])?,
                // As the executor: the largest groups first, then by value.
                true => std::iter::once(ir::SortKey { expr: ir::Expr::Output((k + calls.len()) as u32), desc: true, nulls_first: false })
                    .chain(items_at.iter().map(|x| ir::SortKey { expr: x.clone(), desc: false, nulls_first: true }))
                    .collect(),
            };
            node = ir::Node::Sort { input: Box::new(node), keys };
            items_at
        } else {
            let items_at: Vec<ir::Expr> = items.iter().map(|(_, e)| compile_row(e, cols, multi, params, &subs)).collect::<Result<_>>()?;
            if !order.is_empty() && !driver {
                let keys = sort_keys(&items_at, &|e| at(slot(e, cols)), multi)?;
                node = ir::Node::Sort { input: Box::new(node), keys };
            }
            items_at
        };
        let columns: Vec<String> = items.into_iter().map(|i| i.0).collect();
        let project = |node: ir::Node| ir::Node::Project {
            input: Box::new(node),
            columns: columns.iter().zip(items_at).map(|(name, expr)| ir::Column { name: name.clone(), expr }).collect(),
        };
        let cut = |node: ir::Node| match (offset, limit) {
            (0, None) => node,
            (o, l) => ir::Node::Limit { input: Box::new(node), offset: o as u64, limit: l.map(|l| l as u64) },
        };
        let node = match s.distinct.is_some() {
            true => cut(ir::Node::Distinct { input: Box::new(project(node)) }),
            false => project(cut(node)),
        };
        Ok(Planned { node, columns, keyed: None })
    }
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
            value(&compile(&e, &params, &|_| None, &[]).unwrap()).unwrap()
        };
        for (src, want) in [
            ("NULL = 1", Value::Null),
            ("NOT (:n = 1)", Value::Null),
            ("NOT (:n = 1) OR 1 = 1", Value::Bool(true)),
            ("NULL AND 1 = 0", Value::Bool(false)),
            ("NULL AND 1 = 1", Value::Null),
            ("NULL OR 1 = 0", Value::Null),
            ("2 IN (1, NULL)", Value::Null),
            ("2 NOT IN (1, NULL)", Value::Null),
            ("1 IN (1, NULL)", Value::Bool(true)),
            ("NULL BETWEEN 1 AND 2", Value::Null),
            ("9007199254740993 > 9007199254740992.0", Value::Bool(true)),
            ("9007199254740992.0 < 9007199254740993", Value::Bool(true)),
            ("9007199254740993 = 9007199254740992.0", Value::Bool(false)),
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
}
