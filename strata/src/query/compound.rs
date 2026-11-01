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
use crate::query::logical::{OrderBy, Phase, Select};
use crate::query::rank::Rank;
use crate::query::{AggFn, Params, Projection};
use crate::schema::{FieldType, Schema};
use crate::storage::RankProfile;
use crate::value::Value;

/// Rows one part of a compound query reads at most (a side of a join, a
/// subquery, a `WITH` result).
pub const MAX_ROWS: usize = 1_000_000;

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// Functions that only the executor evaluates: they name one side of a join.
const NATIVE: [&str; 14] = ["match", "bm25", "similarity", "distance", "maxsim", "rrf", "ppr", "decay", "reachable", "path", "hops", "score", "now", "near"];
/// Score functions, which order a side's scan.
const SCORES: [&str; 8] = ["bm25", "similarity", "distance", "maxsim", "rrf", "ppr", "decay", "hops"];
const AGGREGATES: [&str; 5] = ["count", "sum", "avg", "min", "max"];
/// Functions whose value a ranking computes (a score, a graph's, a model's,
/// a set function's, a row's sequence number), not the row evaluator.
const RANKING: [&str; 18] = [
    "bm25", "similarity", "distance", "maxsim", "rrf", "ppr", "decay", "hops", "score", "seq_no", "ranked", "features", "minmax", "rank", "gbdt", "cross_encoder", "rank_profile", "match",
];

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
        Ok(mut stmts) => match stmts.as_mut_slice() {
            [sql::Statement::Query(q)] => json_paths(q, &[schema]).is_ok() && needs(q, schema),
            _ => false,
        },
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
    if s.from.len() != 1 || !s.from[0].joins.is_empty() || s.distinct.is_some() || per_capped(q) || computes_columns(q, s) || reads_paths(q, s) {
        return true;
    }
    match &s.from[0].relation {
        TableFactor::Table { name, .. } if is_table(name, schema) => {}
        _ => return true,
    }
    if orders_by_text(q, s, schema) {
        return true;
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
            _ => found |= of_rows(x),
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

/// Whether a SELECT reads a JSON field's path ([`json_paths`]) outside its
/// WHERE (a column, an order, a group, `HAVING`): the row evaluator reads
/// it from the rows' objects, where WHERE's conditions on paths are the
/// scan's, read from the field's terms.
fn reads_paths(q: &sql::Query, s: &sql::Select) -> bool {
    let mut found = false;
    let mut check = |e: &Expr| walk(e, &mut |x| found |= matches!(x, Expr::Function(f) if fname(f) == "json_extract"));
    for e in distinct_on(s) {
        check(e);
    }
    for item in &s.projection {
        if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
            check(e);
        }
    }
    s.having.iter().for_each(&mut check);
    if let sql::GroupByExpr::Expressions(es, _) = &s.group_by {
        es.iter().for_each(&mut check);
    }
    if let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &q.order_by {
        es.iter().for_each(|o| check(&o.expr));
    }
    found
}

/// `q` with each path of a JSON field of the tables it reads (`attrs.color`,
/// `a.attrs.dims.w`, `attrs['any key']`; [`super::sql::path_ref`]) written
/// as the call it stands for, `json_extract(attrs, 'color')`
/// ([`ir::Func::JsonExtract`]): the binder reads a condition on one from
/// the field's terms, and the row evaluator reads one from the rows'
/// objects. Queries of tables without a JSON field are left as they are.
pub(crate) fn json_paths(q: &mut sql::Query, catalog: &[&Schema]) -> Result<()> {
    let tables = super::tables::read_by(q);
    let schemas: Vec<&Schema> = catalog.iter().copied().filter(|s| tables.iter().any(|t| t.eq_ignore_ascii_case(&s.table)) && s.fields.iter().any(|f| f.ty == FieldType::Json)).collect();
    if schemas.is_empty() {
        return Ok(());
    }
    let mut failed = None;
    let mut rewrite = |e: &mut Expr| {
        if matches!(e, Expr::Function(_)) || failed.is_some() {
            return;
        }
        for s in &schemas {
            match super::sql::path_column(s, e) {
                Ok(Some((_, path, column))) => {
                    let mut call = match parse(&format!("SELECT json_extract(x, {})", sql::Value::SingleQuotedString(path)))
                        .ok()
                        .and_then(|q| match *q.body {
                            sql::SetExpr::Select(s) => s.projection.into_iter().next(),
                            _ => None,
                        }) {
                        Some(SelectItem::UnnamedExpr(c)) => c,
                        _ => unreachable!("a call"),
                    };
                    if let Expr::Function(f) = &mut call {
                        if let Some(a) = func_args_mut(f).into_iter().next() {
                            *a = column;
                        }
                    }
                    *e = call;
                    return;
                }
                Ok(None) => {}
                Err(x) => {
                    failed = Some(x);
                    return;
                }
            }
        }
    };
    query_mut(q, &mut rewrite);
    joins_mut(q, &mut rewrite);
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Calls `f` on the `ON` conditions of `q`'s joins, and of its `WITH`
/// queries', derived tables' and subqueries' ([`query_mut`] reaches the
/// rest).
fn joins_mut(q: &mut sql::Query, f: &mut dyn FnMut(&mut Expr)) {
    if let Some(w) = &mut q.with {
        for cte in &mut w.cte_tables {
            joins_mut(&mut cte.query, f);
        }
    }
    let sql::SetExpr::Select(s) = q.body.as_mut() else { return };
    for t in &mut s.from {
        if let TableFactor::Derived { subquery, .. } = &mut t.relation {
            joins_mut(subquery, f);
        }
        for j in &mut t.joins {
            if let TableFactor::Derived { subquery, .. } = &mut j.relation {
                joins_mut(subquery, f);
            }
            match &mut j.join_operator {
                sql::JoinOperator::Join(sql::JoinConstraint::On(on))
                | sql::JoinOperator::Inner(sql::JoinConstraint::On(on))
                | sql::JoinOperator::Left(sql::JoinConstraint::On(on))
                | sql::JoinOperator::LeftOuter(sql::JoinConstraint::On(on)) => walk_mut(on, f),
                _ => {}
            }
        }
    }
}

/// Whether `e` (outside subqueries) computes a value of rows that a scan
/// does not: a semantic ID's prefix, `sid_at(..)`, or a dot product of
/// vectors, `dot(..)`, which operators compute over the rows the scan
/// returns.
fn of_rows(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| found |= matches!(x, Expr::Function(f) if matches!(fname(f).as_str(), "sid_at" | "dot")));
    found
}

/// Whether a SELECT's columns or ORDER BY compute [`of_rows`] values.
fn rows_of(q: &sql::Query, s: &sql::Select) -> bool {
    select_exprs(s).into_iter().any(of_rows) || q.order_by.as_ref().is_some_and(|ob| matches!(&ob.kind, sql::OrderByKind::Expressions(es) if es.iter().any(|e| of_rows(&e.expr))))
}

/// Whether `e` (outside subqueries) calls a [`RANKING`] function or holds a
/// subquery.
fn ranks(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| {
        found |= match x {
            Expr::Function(f) => RANKING.contains(&fname(f).as_str()),
            Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => true,
            _ => false,
        }
    });
    found
}

/// Whether a SELECT over one table computes a column from each row's
/// values (`n * n`, `lower(k)`, `k LIKE 'a%'`, `SUM(n * 2)`): the row
/// evaluator computes it, typed and exactly, over the rows the table's
/// query finds, as over joined or derived rows ([`Planner::join`]). The
/// ranking evaluator computes in f64 and knows no text functions, so it
/// keeps only the columns of a SELECT that also selects a ranking's value
/// (`bm25(..) * 2`, `hops()`) or orders by one the scan cannot
/// ([`RANKING`] but not [`SCORES`]: `ranked(..)`, `seq_no()`, a model).
fn computes_columns(q: &sql::Query, s: &sql::Select) -> bool {
    let items: Vec<&Expr> = s
        .projection
        .iter()
        .filter_map(|i| match i {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
            _ => None,
        })
        .collect();
    // A column, `score()`, or an aggregate of a column (or `*`).
    let plain = |e: &Expr| match e {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => true,
        Expr::Function(f) if fname(f) == "score" && func_args(f).is_empty() => true,
        Expr::Function(f) if AGGREGATES.contains(&fname(f).as_str()) => {
            func_args(f).iter().all(|a| matches!(a, Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_) | Expr::Wildcard(_)))
        }
        _ => false,
    };
    let computed = items.iter().any(|e| !plain(e));
    let order_ok = match &q.order_by {
        Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) => es.iter().all(|o| is_score(&o.expr) || !ranks(&o.expr)),
        _ => true,
    };
    computed && order_ok && items.iter().all(|e| plain(e) || !ranks(e))
}

/// Whether `q`, a query of `schema`'s table that does not group, orders by
/// a stored KEYWORD or TEXT column of one value first: the row evaluator
/// sorts text, where the scan orders by numbers and scores.
fn orders_by_text(q: &sql::Query, s: &sql::Select, schema: &Schema) -> bool {
    let grouped = !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty()) || has_aggregate(s);
    let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &q.order_by else { return false };
    let text = |e: &Expr| {
        let name = match e {
            Expr::Identifier(id) => &id.value,
            Expr::CompoundIdentifier(ids) => &ids.last().expect("a name").value,
            _ => return false,
        };
        schema.field(name).is_some_and(|f| f.stored && !f.multi && matches!(f.ty, FieldType::Keyword | FieldType::Text { .. }))
    };
    // The first key: a text tie-break (the primary key) the scan orders by.
    !grouped && es.first().is_some_and(|o| text(&o.expr))
}

/// The expressions of a SELECT outside its FROM and ORDER BY.
fn select_exprs(s: &sql::Select) -> Vec<&Expr> {
    let mut out: Vec<&Expr> = distinct_on(s).iter().collect();
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

pub(crate) fn func_args(f: &sql::Function) -> Vec<&Expr> {
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

/// A table's rank profile by name, the table and the name in any case
/// ([`crate::storage::Database::rank_profiles`]).
pub type Profiles<'a> = &'a dyn Fn(&str, &str) -> Option<RankProfile>;

/// The plan of a SELECT over the tables of `catalog` (the first table
/// first), as [`crate::db::Db::query`] binds it: a query of one table that
/// needs no operators ([`is_compound`]) is one search of the table it
/// names, and any other a plan of operators ([`plan_in`]). A query that
/// ranks by a rank profile (`ORDER BY rank_profile(..)`) plans as written
/// out with the one `profiles` finds ([`super::profile::expand`]), and its
/// plan names it. A server binds its requests against its database's
/// tables with it.
pub fn plan_read(sql_text: &str, catalog: &[&Schema], params: &Params, profiles: Profiles) -> Result<ir::Plan> {
    plan_read_query(&*super::sql::parse_query(sql_text)?, catalog, params, profiles)
}

/// [`plan_read`] of a parsed query.
pub(crate) fn plan_read_query(q: &sql::Query, catalog: &[&Schema], params: &Params, profiles: Profiles) -> Result<ir::Plan> {
    let mut rewritten;
    let q = match catalog.iter().any(|s| s.fields.iter().any(|f| f.ty == FieldType::Json)) {
        true => {
            rewritten = q.clone();
            json_paths(&mut rewritten, catalog)?;
            &rewritten
        }
        false => q,
    };
    if let Some((q, r)) = super::profile::expand(q, profiles)? {
        let table = super::tables::read_by(&q).first().cloned().unwrap_or_default();
        return Ok(super::profile::mark(plan_read_query(&q, catalog, params, &|_, _| None)?, &table, &r));
    }
    let schema = match super::tables::read_by(q).first() {
        None => *catalog.first().ok_or_else(|| err("the database has no tables"))?,
        Some(t) => catalog.iter().copied().find(|s| s.table.eq_ignore_ascii_case(t)).ok_or_else(|| err(format!("no such table: {t}")))?,
    };
    if needs(q, schema) {
        return plan_query(q, catalog, params);
    }
    Ok(ir::Plan::of_select(schema, &super::sql::bind_query_in(q, schema, catalog, params)?))
}

/// [`plan_in`] of a parsed query.
pub(crate) fn plan_query(q: &sql::Query, catalog: &[&Schema], params: &Params) -> Result<ir::Plan> {
    let mut q = q.clone();
    json_paths(&mut q, catalog)?;
    let p = Planner { catalog, params, ctes: RefCell::new(Vec::new()), outer: RefCell::new(None) };
    Ok(ir::Plan::new(p.query(&q)?.node))
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
    /// Whether each column is a multi-valued field's, whose lists compare
    /// as the field's values do (`tags != 'red'` holds without tags).
    multi: Vec<bool>,
}

/// A `WITH` result in scope.
#[derive(Clone, Debug)]
struct Cte {
    name: String,
    columns: Vec<String>,
    keyed: Option<String>,
    multi: Vec<bool>,
    /// The ORDER BY its rows were ranked by, which `score()` over them
    /// reads when the query reading them has none.
    order: Option<sql::OrderBy>,
}

struct Planner<'a> {
    /// The tables a query may read.
    catalog: &'a [&'a Schema],
    params: &'a Params,
    /// The `WITH` results in scope, the innermost last.
    ctes: RefCell<Vec<Cte>>,
    /// The row of the Search being bound, which a subquery in its ranking
    /// may name ([`Planner::correlate`]).
    outer: RefCell<Option<Outer>>,
}

/// The row a correlated subquery names: what it may call it (a table's
/// name, its alias, the `WITH` results whose rows it is), and its fields.
#[derive(Clone, Debug)]
struct Outer {
    names: Vec<String>,
    fields: Vec<String>,
}

/// The function a column of the outer row becomes in a correlated
/// subquery's syntax tree: `__outer(n)`, field `n` ([`ir::Expr::Outer`]).
pub(crate) const OUTER: &str = "__outer";

/// The field `__outer(n)` names ([`OUTER`]).
pub(crate) fn outer_field(e: &Expr) -> Option<u32> {
    let Expr::Function(f) = e else { return None };
    if fname(f) != OUTER {
        return None;
    }
    match func_args(f).as_slice() {
        [Expr::Value(v)] => v.value.to_string().parse().ok(),
        _ => None,
    }
}

/// `__outer(i)` ([`OUTER`]).
fn outer_call(i: usize) -> Expr {
    let q = super::sql::parse_query(&format!("SELECT {OUTER}({i})")).expect("a SELECT");
    let sql::SetExpr::Select(s) = *q.body else { unreachable!("a SELECT") };
    match s.projection.into_iter().next() {
        Some(SelectItem::UnnamedExpr(e)) => e,
        _ => unreachable!("one column"),
    }
}

/// The names a SELECT's FROM gives its tables (each table's name and
/// alias, lowercase).
fn from_names(q: &sql::Query) -> Vec<String> {
    let sql::SetExpr::Select(s) = q.body.as_ref() else { return Vec::new() };
    let mut out = Vec::new();
    for t in &s.from {
        for r in std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)) {
            match r {
                TableFactor::Table { name, alias, .. } => {
                    out.push(super::sql::table_name(name).to_ascii_lowercase());
                    out.extend(alias.iter().map(|a| a.name.value.to_ascii_lowercase()));
                }
                TableFactor::Derived { alias, .. } => out.extend(alias.iter().map(|a| a.name.value.to_ascii_lowercase())),
                _ => {}
            }
        }
    }
    out
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
        if let Some(p) = self.phased(q)? {
            return Ok(p);
        }
        let mut defs = Vec::new();
        if let Some(w) = &q.with {
            if w.recursive {
                return Err(err("WITH RECURSIVE is not supported: REACHABLE and PATH traverse links"));
            }
            for (i, cte) in w.cte_tables.iter().enumerate() {
                // Rerank phases over the `WITH` results from this one on,
                // which may read those before (a user's history).
                if i > 0 {
                    let mut rest = q.clone();
                    rest.with.as_mut().expect("WITH").cte_tables.drain(..i);
                    if let Some(p) = self.phased(&rest)? {
                        return Ok(Planned { node: ir::Node::With { ctes: defs, input: Box::new(p.node) }, ..p });
                    }
                }
                let mut p = self.query(&cte.query)?;
                if !cte.alias.columns.is_empty() {
                    if cte.alias.columns.len() != p.columns.len() {
                        return Err(err(format!("WITH {}: {} column names for {} columns", cte.alias.name, cte.alias.columns.len(), p.columns.len())));
                    }
                    let columns: Vec<String> = cte.alias.columns.iter().map(|c| c.name.value.clone()).collect();
                    let exprs = columns.iter().enumerate().map(|(i, name)| ir::Column { name: name.clone(), expr: ir::Expr::Output(i as u32) }).collect();
                    p = Planned { node: ir::Node::Project { input: Box::new(p.node), columns: exprs }, columns, keyed: None, multi: p.multi };
                }
                let name = cte.alias.name.value.clone();
                let order = cte.query.order_by.clone().filter(|_| p.keyed.is_some());
                self.ctes.borrow_mut().push(Cte { name: name.clone(), columns: p.columns, keyed: p.keyed, multi: p.multi, order });
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

    /// A `WITH` that ranks a table's rows and cuts them, ranked again by
    /// the next `WITH` or the query itself, as one Search with rerank
    /// phases ([`ir::Phase`]): each phase ranks the rows of the one before
    /// by an expression, best first.
    ///
    /// ```sql
    /// WITH top AS (SELECT * FROM docs WHERE year > 2020 ORDER BY bm25(body, :q) DESC LIMIT 100)
    /// SELECT id, score() FROM top ORDER BY bm25(body, :q) * (1 + 0.1 * rating) DESC LIMIT 10
    /// ```
    ///
    /// Every row a phase reads goes on, ranked by the phase (one it gives
    /// no value last, rows tied in the order they had), and an outer
    /// ranking may read any of the table's columns, as the rows are the
    /// table's. Each `WITH` after the first, and the query, reads the one
    /// before alone, with no `WHERE`, grouping or subquery, and ranks by one
    /// expression `DESC`; each `WITH` keeps the table's columns (`*`) and
    /// has a `LIMIT`. `None` for any other query, or one that does not
    /// bind so, which plans as the table restricted to the `WITH` result's
    /// keys ([`Planner::over_rows`]).
    fn phased(&self, q: &sql::Query) -> Result<Option<Planned>> {
        let Some(w) = q.with.as_ref().filter(|w| !w.recursive && w.cte_tables.iter().all(|c| c.alias.columns.is_empty())) else { return Ok(None) };
        let mut body = q.clone();
        body.with = None;
        // The first `WITH`, its table and alias.
        let first = &w.cte_tables[0].query;
        let Some((s0, name)) = one_table(first) else { return Ok(None) };
        let Some(schema) = self.table(&name).filter(|_| self.cte(&name).is_none()) else { return Ok(None) };
        let simple = |s: &sql::Select| s.distinct.is_none() && s.having.is_none() && matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty());
        if first.with.is_some() || !simple(s0) {
            return Ok(None);
        }
        // Each later step: what it projects, ranks by and cuts, its
        // columns named without the `WITH` it reads.
        let mut steps = Vec::new();
        let reads: Vec<&sql::Query> = w.cte_tables[1..].iter().map(|c| c.query.as_ref()).chain(std::iter::once(&body)).collect();
        for (i, r) in reads.iter().enumerate() {
            let read = &w.cte_tables[i].alias.name.value;
            let Some((s, from)) = one_table(r) else { return Ok(None) };
            let alias = match &s.from[0].relation {
                TableFactor::Table { alias, .. } => alias.as_ref().map(|a| a.name.value.clone()),
                _ => None,
            };
            let mut subquery = false;
            let mut exprs = select_exprs(s);
            if let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &r.order_by {
                exprs.extend(es.iter().map(|o| &o.expr));
            }
            for e in exprs {
                // A scalar subquery is a number of each row ranked.
                walk(e, &mut |x| subquery |= matches!(x, Expr::InSubquery { .. } | Expr::Exists { .. }));
            }
            let order = match &r.order_by {
                Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) if es.len() == 1 => &es[0],
                _ => return Ok(None),
            };
            let ranks = order.options.asc == Some(false)
                && order.options.nulls_first.is_none()
                && !matches!(&order.expr, Expr::Value(_))
                && !matches!(&order.expr, Expr::Function(f) if matches!(fname(f).as_str(), "hops" | "distance"));
            if !from.eq_ignore_ascii_case(read) || r.with.is_some() || !simple(s) || s.selection.is_some() || subquery || !ranks {
                return Ok(None);
            }
            let plain = |e: &Expr| {
                let e = unqualify(e, read);
                alias.as_deref().map_or(e.clone(), |a| unqualify(&e, a))
            };
            let projection = s
                .projection
                .iter()
                .map(|item| match item {
                    SelectItem::UnnamedExpr(e) => SelectItem::UnnamedExpr(plain(e)),
                    SelectItem::ExprWithAlias { expr, alias } => SelectItem::ExprWithAlias { expr: plain(expr), alias: alias.clone() },
                    other => other.clone(),
                })
                .collect();
            let mut o = order.clone();
            o.expr = plain(&order.expr);
            steps.push((projection, o, r.limit_clause.clone()));
        }
        // Each phase's ranking, first to last, for `features()`.
        let orders: Vec<Expr> = match &first.order_by {
            Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) if !es.is_empty() => {
                std::iter::once(es[0].expr.clone()).chain(steps.iter().map(|(_, o, _)| o.expr.clone())).collect()
            }
            _ => return Ok(None),
        };
        // Each step binds as the first `WITH` with its own columns, ranking
        // and cut, so that its graphs and parameters line up; the last with
        // the rankings before it.
        let subs = |sq: &sql::Query, kind: Sub| self.subquery(sq, kind, Some(schema));
        // A correlated subquery calls the row by the table's name, its
        // alias, or a `WITH` result's.
        let mut names = from_names(first);
        for (i, r) in reads.iter().enumerate() {
            names.push(w.cte_tables[i].alias.name.value.to_ascii_lowercase());
            names.extend(from_names(r));
        }
        let fields = schema.fields.iter().map(|f| f.name.clone()).collect();
        let outer = self.outer.replace(Some(Outer { names, fields }));
        let r = self.phases_of(first, schema, steps, orders, &subs);
        *self.outer.borrow_mut() = outer;
        r
    }

    /// The Search of [`Planner::phased`]: the first `WITH`, then each step
    /// as a phase.
    #[allow(clippy::type_complexity)]
    fn phases_of(&self, first: &sql::Query, schema: &'a Schema, steps: Vec<(Vec<SelectItem>, sql::OrderByExpr, Option<sql::LimitClause>)>, orders: Vec<Expr>, subs: &dyn Fn(&sql::Query, Sub) -> Result<Option<ir::Expr>>) -> Result<Option<Planned>> {
        let bind = |projection: Option<Vec<SelectItem>>, order: Option<sql::OrderByExpr>, cut: Option<sql::LimitClause>, earlier: &[Expr]| -> Option<Select> {
            let mut q = first.clone();
            let sql::SetExpr::Select(s) = q.body.as_mut() else { return None };
            if let Some(p) = projection {
                s.projection = p;
            }
            if let (Some(o), Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. })) = (order, &mut q.order_by) {
                *es = vec![o];
            }
            q.limit_clause = cut;
            super::sql::bind_phase_with(&q, schema, self.catalog, self.params, subs as &Subs, earlier).ok()
        };
        let keyed = |s: &Select| {
            s.group_by.is_empty()
                && s.projection.iter().all(|(name, p)| match p {
                    Projection::Field(f) => schema.fields[*f].name.eq_ignore_ascii_case(name),
                    Projection::Score | Projection::Hops(_) => true,
                    _ => false,
                })
        };
        let Some(base) = bind(None, None, first.limit_clause.clone(), &[]) else { return Ok(None) };
        let mut count = match limits(&first.limit_clause, self.params) {
            Ok((0, Some(n))) if n > 0 && base.order.is_some() && base.tiebreak.is_none() && keyed(&base) => n,
            _ => return Ok(None),
        };
        let mut phases: Vec<Phase> = Vec::new();
        let last = steps.len() - 1;
        let mut out = None;
        for (i, (projection, order, cut)) in steps.into_iter().enumerate() {
            let earlier = if i == last { &orders[..=i] } else { &[] };
            let Some(s) = bind(Some(projection), Some(order), cut.clone(), earlier) else { return Ok(None) };
            let Some(OrderBy::Score { score, desc: true, nulls_first: false }) = s.order.clone() else { return Ok(None) };
            if s.tiebreak.is_some() || s.graphs != base.graphs || s.projection.iter().any(|(_, p)| *p == Projection::SeqNo) {
                return Ok(None);
            }
            let global = phases.last().is_some_and(|p| p.global) || Rank::is_global(&score);
            phases.push(Phase { score, count, global });
            if i < last {
                count = match limits(&cut, self.params) {
                    Ok((0, Some(n))) if n > 0 && keyed(&s) => n,
                    _ => return Ok(None),
                };
            } else {
                out = Some(s);
            }
        }
        let Some(s) = out else { return Ok(None) };
        let select = Select { order: base.order, phases, ..s };
        let columns = select.projection.iter().map(|(n, _)| n.clone()).collect();
        let keyed = keyed(&select).then(|| schema.table.clone());
        let multi = multi_of(schema, &select);
        Ok(Some(Planned { node: ir::Plan::of_select(schema, &select).root, columns, keyed, multi }))
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
                    Some(schema) if !rows_of(q, s) => self.over_rows(q, s, &cte, schema),
                    _ => self.join(q, s),
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
        match &s.distinct {
            // The first rows of each group: the scan's rows, kept by operators.
            Some(sql::Distinct::On(_)) => return self.join(&q, &s),
            _ if per_capped(&q) => return self.join(&q, &s),
            // Columns computed from the rows' values, and an order by
            // text, by the row evaluator.
            _ if computes_columns(&q, &s) || orders_by_text(&q, &s, schema) || reads_paths(&q, &s) => return self.join(&q, &s),
            Some(sql::Distinct::Distinct) => distinct_to_group(&mut s)?,
            None => {}
        }
        if rows_of(&q, &s) {
            return self.join(&q, &s);
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
        let outer = self.outer.replace(Some(Outer { names: from_names(q), fields: schema.fields.iter().map(|f| f.name.clone()).collect() }));
        let select = super::sql::bind_query_with(q, schema, self.catalog, self.params, &subs as &Subs);
        *self.outer.borrow_mut() = outer;
        let select = select.map_err(at)?;
        if select.projection.iter().any(|(_, p)| *p == Projection::SeqNo) {
            return Err(at(err("seq_no() is a column of a SELECT on one table, not of a compound query")));
        }
        let aggregated = !select.group_by.is_empty() || select.projection.iter().any(|(_, p)| matches!(p, Projection::Count | Projection::Agg { .. }));
        let keyed = !aggregated
            && select.projection.iter().all(|(name, p)| match p {
                Projection::Field(f) => schema.fields[*f].name.eq_ignore_ascii_case(name),
                Projection::Score | Projection::Hops(_) => true,
                _ => false,
            });
        let columns = select.projection.iter().map(|(n, _)| n.clone()).collect();
        let multi = multi_of(schema, &select);
        Ok(Planned { node: ir::Plan::of_select(schema, &select).root, columns, keyed: keyed.then(|| schema.table.clone()), multi })
    }

    /// The expression a subquery stands for: its one column's values (`IN`),
    /// whether it has a row (`EXISTS`), its one value (a comparison), or for
    /// seeds the keys of its rows when they are rows of the scanned table
    /// `scanned`, else its one column's values (`None` for a subquery of the
    /// scanned table that the executor runs itself).
    fn subquery(&self, q: &sql::Query, kind: Sub, scanned: Option<&Schema>) -> Result<Option<ir::Expr>> {
        if kind == Sub::Seeds && scanned.is_some_and(|s| !needs(q, s)) {
            return Ok(None);
        }
        let correlated = match kind {
            Sub::Score => self.correlate(q)?,
            _ if self.correlate(q)?.is_some() => {
                return Err(err("a subquery that names the outer row computes a number of each row it ranks: it works in ORDER BY, rerank phases and the columns of ranked rows, not in WHERE, IN or EXISTS"));
            }
            _ => None,
        };
        let q = correlated.as_ref().unwrap_or(q);
        // The subquery's own Searches name their own rows.
        let outer = self.outer.borrow_mut().take();
        let p = self.query(q);
        *self.outer.borrow_mut() = outer;
        let p = p?;
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
            Sub::Vectors => {
                one("a subquery of query vectors")?;
                ir::Expr::Values(node)
            }
            Sub::Rows if p.columns.is_empty() || p.columns.len() > 3 => {
                return Err(err(format!("encode's history returns the item's key, then its action and time (one to three columns), not {}", p.columns.len())));
            }
            Sub::Rows => ir::Expr::Values(node),
            Sub::Exists => ir::Expr::Exists(node),
            Sub::Scalar => {
                one("a scalar subquery")?;
                ir::Expr::Scalar(node)
            }
            Sub::Score => {
                one("a subquery in a ranking expression")?;
                ir::Expr::Scalar(node)
            }
            // Rows of the scanned table seed by their keys. Rows of another
            // table seed by the values of their one column: `(SELECT item
            // FROM events ...)` starts from the items, not from the events.
            Sub::Seeds if p.keyed.as_deref().is_some_and(|t| scanned.is_none_or(|s| s.table.eq_ignore_ascii_case(t))) => ir::Expr::Keys(node),
            Sub::Seeds => {
                one("a subquery of seeds")?;
                ir::Expr::Values(node)
            }
        }))
    }

    /// `q` with each column of the outer row it names (`first.cat`, where
    /// `first` names the row of the Search being ranked and not a table of
    /// `q`'s own FROM) as [`OUTER`], or `None` when it names none: a
    /// correlated subquery, which runs once for each distinct value of
    /// those columns among the rows ranked.
    fn correlate(&self, q: &sql::Query) -> Result<Option<sql::Query>> {
        let Some(outer) = self.outer.borrow().clone() else { return Ok(None) };
        let own = from_names(q);
        let names: Vec<&String> = outer.names.iter().filter(|n| !own.contains(n)).collect();
        let mut q = q.clone();
        let mut named = false;
        let mut failed = None;
        query_mut(&mut q, &mut |e| {
            let Expr::CompoundIdentifier(ids) = e else { return };
            let [a, c] = ids.as_slice() else { return };
            if !names.iter().any(|n| a.value.eq_ignore_ascii_case(n)) {
                return;
            }
            match outer.fields.iter().position(|f| f.eq_ignore_ascii_case(&c.value)) {
                Some(i) => {
                    *e = outer_call(i);
                    named = true;
                }
                None => failed = Some(err(format!("{a}.{c}: {a} has no column {c}"))),
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        Ok(named.then_some(q))
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
        let TableFactor::Table { name, alias, .. } = &mut s.from[0].relation else { unreachable!("a table") };
        // A correlated subquery still calls the rows by the `WITH` result's name.
        if alias.is_none() {
            *alias = Some(sql::TableAlias { name: sql::Ident::new(cte.name.clone()), columns: Vec::new() });
        }
        *name = sql::ObjectName::from(vec![sql::Ident::new(schema.table.clone())]);
        let grouped = !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty()) || has_aggregate(&s) || s.distinct.is_some();
        // `score()` without an ORDER BY is the score the WITH result ranked
        // its rows by: ranking them again by it keeps their order.
        let mut score = false;
        for i in &s.projection {
            if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = i {
                walk(e, &mut |x| score |= matches!(x, Expr::Function(f) if fname(f) == "score"));
            }
        }
        if q.order_by.is_none() && !grouped && score {
            q.order_by = cte.order.clone();
        }
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
                        None => SrcKind::Rows(Planned { node: ir::Node::Ref { name: cte.name }, columns: cte.columns, keyed: None, multi: cte.multi }),
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
        let mut order: Vec<sql::OrderByExpr> = match &q.order_by {
            None => Vec::new(),
            Some(ob) => match &ob.kind {
                sql::OrderByKind::Expressions(es) => es.clone(),
                _ => return Err(err("ORDER BY ALL is not supported")),
            },
        };
        let (offset, limit) = limits(&q.limit_clause, self.params)?;
        // MAX k PER field: DISTINCT ON (field), k rows a value.
        let cap = take_per_cap(&mut order)?;
        let capped;
        let s = match &cap {
            None => s,
            Some(_) if s.distinct.is_some() => return Err(err("MAX k PER field keeps rows, as DISTINCT ON does (MAX 1 PER): not with DISTINCT")),
            Some((_, field)) => {
                capped = sql::Select { distinct: Some(sql::Distinct::On(vec![field.clone()])), ..s.clone() };
                &capped
            }
        };
        let per = cap.as_ref().map_or(1, |(k, _)| *k);
        // `SELECT DISTINCT a, b` is `GROUP BY a, b`, as over one table, so
        // a multi-valued column gives a row per value there and here.
        let grouping;
        let plain = |s: &sql::Select| {
            matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty())
                && !has_aggregate(s)
                && s.projection.iter().all(|i| matches!(i, SelectItem::UnnamedExpr(_) | SelectItem::ExprWithAlias { .. }))
        };
        let s = if matches!(s.distinct, Some(sql::Distinct::Distinct)) && plain(s) {
            let mut g = s.clone();
            distinct_to_group(&mut g)?;
            grouping = g;
            &grouping
        } else {
            s
        };

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
                [i] if !in_memory(*i) && !left_join_right(*i) && !of_rows(&c) => pushed[*i].push(c),
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
            && !matches!(s.distinct, Some(sql::Distinct::Distinct))
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
        if let Some((i, c)) = cap.as_ref().map(|(_, field)| sides.column(field)).transpose()?.flatten() {
            if table(i).and_then(|t| t.field(&c)).is_some_and(|f| f.multi) {
                return Err(err(format!("MAX k PER {c}: {c} is multi-valued, and a row with several values is in several groups; cap a field of one value")));
            }
            if table(i).and_then(|t| t.field(&c)).is_some_and(|f| matches!(f.ty, FieldType::Text { .. })) {
                return Err(err(format!("MAX k PER {c}: {c} is TEXT, searched by its words, whose rows rarely share a value; cap a KEYWORD or a number")));
            }
        }

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
            let side_multi: Vec<bool> = needed[i]
                .iter()
                .map(|c| match &src.kind {
                    SrcKind::Rows(p) => p.columns.iter().position(|x| x.eq_ignore_ascii_case(c)).is_some_and(|j| p.multi.get(j) == Some(&true)),
                    _ => table(i).and_then(|t| t.field(c)).is_some_and(|f| f.multi),
                })
                .collect();
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
        self.evaluate(s, node, &cols, &multi, &mem_order, offset, limit, srcs.len() > 1, driver_order, per)
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

/// A SELECT of one table or `WITH` result, with no join, and its name.
pub(crate) fn one_table(q: &sql::Query) -> Option<(&sql::Select, String)> {
    let sql::SetExpr::Select(s) = q.body.as_ref() else { return None };
    match s.from.as_slice() {
        [t] if t.joins.is_empty() => match &t.relation {
            TableFactor::Table { name, .. } => Some((s, super::sql::table_name(name))),
            _ => None,
        },
        _ => None,
    }
}

fn qualified(alias: &str, column: &str) -> Expr {
    Expr::CompoundIdentifier(vec![sql::Ident::new(alias), sql::Ident::new(column)])
}

/// `q` as SQL that parses back to it, as an error names it: a column
/// named like a keyword, or other than a plain identifier, quoted
/// (`"order"`, `"doc id"`), and the operand of a sign in parentheses when
/// it starts with a sign itself (`- -1` would print as `--1`, which
/// starts a comment).
pub(crate) fn printable(q: &sql::Query) -> String {
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
    super::parse::unparse_per_caps(&q.to_string())
}

fn quote(id: &mut sql::Ident) {
    let plain = id.value.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && id.value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if id.quote_style.is_none() && (!plain || sqlparser::keywords::ALL_KEYWORDS.contains(&id.value.to_ascii_uppercase().as_str())) {
        id.quote_style = Some('"');
    }
}

/// Calls `f` on every expression of `q`'s SELECT, its subqueries' too,
/// each before its subexpressions.
pub(crate) fn query_mut(q: &mut sql::Query, f: &mut dyn FnMut(&mut Expr)) {
    if let Some(w) = &mut q.with {
        for cte in &mut w.cte_tables {
            query_mut(&mut cte.query, f);
        }
    }
    if let sql::SetExpr::Select(s) = q.body.as_mut() {
        if let Some(sql::Distinct::On(es)) = &mut s.distinct {
            es.iter_mut().for_each(|e| walk_mut(e, f));
        }
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

/// Whether `e` is `MAX k PER field` ([`super::parse::PER_CAP`]).
fn is_per_cap(e: &Expr) -> bool {
    matches!(e, Expr::Function(f) if fname(f) == super::parse::PER_CAP)
}

/// Whether `q` ends its ORDER BY with `MAX k PER field`.
fn per_capped(q: &sql::Query) -> bool {
    matches!(&q.order_by, Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) if es.iter().any(|o| is_per_cap(&o.expr)))
}

/// `MAX k PER field` out of an ORDER BY, as (k, field): the rows a
/// value keeps, and the field.
fn take_per_cap(order: &mut Vec<sql::OrderByExpr>) -> Result<Option<(u64, Expr)>> {
    let Some(i) = order.iter().position(|o| is_per_cap(&o.expr)) else { return Ok(None) };
    let o = order.remove(i);
    if i != order.len() {
        return Err(err("MAX k PER field ends ORDER BY"));
    }
    let Expr::Function(f) = &o.expr else { unreachable!("a call") };
    let (k, field) = match func_args(f).as_slice() {
        [Expr::Value(v), field] => (v.value.to_string().parse::<u64>().ok().filter(|&k| k > 0), (*field).clone()),
        _ => (None, o.expr.clone()),
    };
    let k = k.ok_or_else(|| err("MAX k PER field takes a whole number of rows, 1 or more"))?;
    if order.is_empty() {
        return Err(err("MAX k PER field needs an ORDER BY: it keeps each value's k first rows in its order"));
    }
    Ok(Some((k, field)))
}

/// The expressions of `DISTINCT ON (...)`: none for another SELECT.
fn distinct_on(s: &sql::Select) -> &[Expr] {
    match &s.distinct {
        Some(sql::Distinct::On(es)) => es,
        _ => &[],
    }
}

/// `SELECT DISTINCT a, b` as `GROUP BY a, b`.
pub(crate) fn distinct_to_group(s: &mut sql::Select) -> Result<()> {
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

/// A query's OFFSET and LIMIT, its parameters bound.
pub(crate) fn limits(l: &Option<sql::LimitClause>, params: &Params) -> Result<(usize, Option<usize>)> {
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
    // An operand of a comparison, IN, LIKE or PREFIX.
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
    // A JSON path compared with a value reads the values of the value's
    // type only, as the field's terms do (`attrs.price > 5` sees numbers).
    let kind_of = |x: &Expr| -> Option<crate::json::Kind> {
        match go(x) {
            Ok(ir::Expr::Lit(v)) => crate::json::Kind::of(&v),
            _ => None,
        }
    };
    let typed = |x: &Expr, kind: Option<crate::json::Kind>| -> Result<Box<ir::Expr>> {
        match (x, kind) {
            (Expr::Function(f), Some(k)) if fname(f) == "json_extract" && func_args(f).len() == 2 => {
                let mut args: Vec<ir::Expr> = func_args(f).into_iter().map(go).collect::<Result<_>>()?;
                args.push(ir::Expr::Lit(Value::Str(k.name().to_string())));
                Ok(Box::new(ir::Expr::Call { func: Func::JsonExtract, args }))
            }
            (x, _) => listy(x),
        }
    };
    let sub = |q: &sql::Query, kind: Sub| match subs {
        Some(subs) => subs(q, kind),
        None => Err(err(format!("a subquery is not supported here: {e}"))),
    };
    Ok(match e {
        Expr::Nested(x) | Expr::UnaryOp { op: UnaryOperator::Plus, expr: x } => go(x)?,
        Expr::Value(v) => ir::Expr::Lit(literal(&v.value, params)?),
        Expr::UnaryOp { op: UnaryOperator::Not, expr } => ir::Expr::Not(bx(expr)?),
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => match super::sql::negative_integer(expr) {
            Some(n) => ir::Expr::Lit(Value::Int(n)),
            None => ir::Expr::Neg(bx(expr)?),
        },
        Expr::IsNull(x) => ir::Expr::IsNull(bx(x)?),
        Expr::IsNotNull(x) => ir::Expr::Not(Box::new(ir::Expr::IsNull(bx(x)?))),
        // `attrs.in_stock IS TRUE`: its booleans, as over the table.
        Expr::IsTrue(x) | Expr::IsFalse(x) if matches!(x.as_ref(), Expr::Function(f) if fname(f) == "json_extract") => ir::Expr::Cmp {
            op: CmpOp::Eq,
            left: typed(x, Some(crate::json::Kind::Bool))?,
            right: Box::new(ir::Expr::Lit(Value::Bool(matches!(e, Expr::IsTrue(_))))),
        },
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
                let (l, r) = (typed(left, kind_of(right))?, typed(right, kind_of(left))?);
                // A comparison of a multi-valued column holds when some value
                // matches, and `!=` when none does, as over one table: `tags
                // != 'red'` holds without tags, and not for ['red', 'blue'].
                let of_lists = [&*l, &*r].iter().any(|x| matches!(x, ir::Expr::Case { whens, .. } if matches!(whens.first(), Some(When { then: ir::Expr::Lit(Value::List(xs)), .. }) if xs.is_empty())));
                if op == CmpOp::Ne && of_lists {
                    return Ok(ir::Expr::Not(Box::new(ir::Expr::Cmp { op: CmpOp::Eq, left: l, right: r })));
                }
                return Ok(ir::Expr::Cmp { op, left: l, right: r });
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
        Expr::Between { expr, negated, low, high } => not_if(*negated, ir::Expr::Between { expr: typed(expr, kind_of(low).or(kind_of(high)))?, lo: Some(bx(low)?), hi: Some(bx(high)?) }),
        Expr::InList { expr, list, negated } => {
            // Values of one type at a time: `attrs.k IN ('a', 1)` reads the
            // path's strings for 'a' and its numbers for 1.
            let kinds: Vec<Option<crate::json::Kind>> = list.iter().map(kind_of).collect();
            let mut distinct: Vec<Option<crate::json::Kind>> = kinds.clone();
            distinct.sort();
            distinct.dedup();
            let path = matches!(expr.as_ref(), Expr::Function(f) if fname(f) == "json_extract");
            let x = match distinct.as_slice() {
                [k] => ir::Expr::In { expr: typed(expr, *k)?, list: list.iter().map(go).collect::<Result<_>>()? },
                ks if path && ks.iter().all(Option::is_some) => ir::Expr::Or(
                    ks.iter()
                        .map(|k| Ok(ir::Expr::In { expr: typed(expr, *k)?, list: list.iter().zip(&kinds).filter(|(_, x)| *x == k).map(|(v, _)| go(v)).collect::<Result<_>>()? }))
                        .collect::<Result<_>>()?,
                ),
                _ => ir::Expr::In { expr: listy(expr)?, list: list.iter().map(go).collect::<Result<_>>()? },
            };
            not_if(*negated, x)
        }
        Expr::InSubquery { expr, subquery, negated } => not_if(*negated, ir::Expr::In { expr: listy(expr)?, list: vec![sub(subquery, Sub::In)?] }),
        Expr::Exists { subquery, negated } => not_if(*negated, sub(subquery, Sub::Exists)?),
        Expr::Subquery(q) => sub(q, Sub::Scalar)?,
        Expr::Like { negated, any, expr, pattern, escape_char } | Expr::ILike { negated, any, expr, pattern, escape_char } => {
            if *any || escape_char.is_some() {
                return Err(err(format!("unsupported expression: {e}")));
            }
            not_if(*negated, ir::Expr::Like { expr: typed(expr, Some(crate::json::Kind::Str))?, pattern: bx(pattern)?, case_insensitive: matches!(e, Expr::ILike { .. }) })
        }
        Expr::Case { operand, conditions, else_result, .. } => ir::Expr::Case {
            operand: operand.as_deref().map(bx).transpose()?,
            whens: conditions.iter().map(|c| Ok(When { when: go(&c.condition)?, then: go(&c.result)? })).collect::<Result<_>>()?,
            otherwise: else_result.as_deref().map(bx).transpose()?,
        },
        Expr::Function(_) if outer_field(e).is_some() => ir::Expr::Outer(outer_field(e).expect("a field")),
        Expr::Function(f) if fname(f) == "prefix" => {
            let [x, p] = func_args(f)[..] else { return Err(err("PREFIX(text, prefix) takes two arguments")) };
            ir::Expr::Prefix { expr: typed(x, Some(crate::json::Kind::Str))?, prefix: bx(p)?, edits: 0 }
        }
        Expr::Function(f) if fname(f) == "at_least" => {
            let usage = "AT_LEAST(n, condition, ...): n a count (2) or a percentage ('75%') of the conditions";
            let args = func_args(f);
            let [n, conds @ ..] = args.as_slice() else { return Err(err(usage)) };
            if conds.is_empty() {
                return Err(err(usage));
            }
            if let FunctionArguments::List(l) = &f.args {
                if let Some(FunctionArg::Named { name, .. }) = l.args.iter().find(|a| matches!(a, FunctionArg::Named { .. })) {
                    return Err(err(format!("unknown option {name} for AT_LEAST")));
                }
            }
            let ir::Expr::Lit(n) = go(n)? else { return Err(err(usage)) };
            let need = crate::query::exec::need_of(&n, "AT_LEAST's n is")?.of(conds.len());
            ir::Expr::AtLeast { need: need as u32, conds: conds.iter().map(|c| go(c)).collect::<Result<_>>()? }
        }
        Expr::Function(f) if fname(f) == "encode" => {
            let lit = |x: &Expr| match x {
                Expr::Value(v) => literal(&v.value, params),
                x => Err(err(format!("encode's model and now are values, not {x}"))),
            };
            super::sql::encode_call(f, &lit, &|q| sub(q, Sub::Rows))?
        }
        Expr::Function(f) => {
            let func = match fname(f).as_str() {
                "lower" => Func::Lower,
                "upper" => Func::Upper,
                "length" => Func::Length,
                "abs" => Func::Abs,
                "round" => Func::Round,
                "coalesce" => Func::Coalesce,
                "dot" if func_args(f).len() != 2 => return Err(err("dot(a, b) takes two vectors")),
                "dot" => Func::Dot,
                "sid_at" => Func::SidAt,
                "count_true" => Func::CountTrue,
                "json_extract" => Func::JsonExtract,
                n if Func::numeric(n).is_some() => Func::numeric(n).expect("a numeric function"),
                n if NATIVE.contains(&n) => {
                    return Err(err(format!("{}(...) works on one table's scan, in its WHERE or ORDER BY (in a join, on that table's column: a.body), not on values of rows", n.to_uppercase())))
                }
                n if AGGREGATES.contains(&n) => return Err(err(format!("{}(...) is an aggregate: it needs GROUP BY or a SELECT of aggregates, not a value of one row", n.to_uppercase()))),
                n => return Err(err(format!("unknown function {n}"))),
            };
            let args: Vec<ir::Expr> = func_args(f).into_iter().map(go).collect::<Result<_>>()?;
            func.arity(args.len()).map_err(|a| err(format!("{}() takes {a}", func.name())))?;
            ir::Expr::Call { func, args }
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
    /// `per`: the rows a group of `DISTINCT ON` keeps (`MAX k PER`).
    fn evaluate(&self, s: &sql::Select, input: ir::Node, cols: &[(String, String)], multi: &[bool], order: &[OrderKey], offset: usize, limit: Option<usize>, joined: bool, driver: bool, per: u64) -> Result<Planned> {
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
        // A number is a position (`ORDER BY 2`), and one past the columns
        // an error, not a constant that orders nothing.
        let by_alias = |e: &Expr, what: &str| -> Result<Option<usize>> {
            Ok(match e {
                Expr::Identifier(id) => items.iter().position(|(n, _)| n.eq_ignore_ascii_case(&id.value)),
                Expr::Value(v) if matches!(v.value, sql::Value::Number(..)) => match e.to_string().parse::<usize>() {
                    Ok(n) if (1..=items.len()).contains(&n) => Some(n - 1),
                    _ => return Err(err(format!("{what} {e}: no such column (the SELECT list has {} columns)", items.len()))),
                },
                _ => None,
            })
        };
        let sort_keys = |items_at: &[ir::Expr], slot: &dyn Fn(&Expr) -> Option<Result<ir::Expr>>, multi: &[bool]| -> Result<Vec<ir::SortKey>> {
            order
                .iter()
                .map(|(e, desc, nulls_first)| {
                    let expr = match by_alias(e, "ORDER BY")? {
                        Some(i) => items_at[i].clone(),
                        None => compile_with(e, params, slot, multi, Some(&subs))?,
                    };
                    Ok(ir::SortKey { expr, desc: *desc, nulls_first: *nulls_first })
                })
                .collect()
        };
        // DISTINCT ON: the first row of each group (`per` rows with MAX k
        // PER), in the order above.
        let first_of = |node: ir::Node, items_at: &[ir::Expr], slot: &dyn Fn(&Expr) -> Option<Result<ir::Expr>>, multi: &[bool]| -> Result<ir::Node> {
            let on = distinct_on(s);
            if on.is_empty() {
                return Ok(node);
            }
            let keys = on
                .iter()
                .map(|e| match by_alias(e, "DISTINCT ON")? {
                    Some(i) => Ok(items_at[i].clone()),
                    None => compile_with(e, params, slot, multi, Some(&subs)),
                })
                .collect::<Result<_>>()?;
            Ok(ir::Node::DistinctOn { input: Box::new(node), keys, per })
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
            node = first_of(node, &items_at, &group_slot, &[])?;
            items_at
        } else {
            let items_at: Vec<ir::Expr> = items.iter().map(|(_, e)| compile_row(e, cols, multi, params, &subs)).collect::<Result<_>>()?;
            if !order.is_empty() && !driver {
                let keys = sort_keys(&items_at, &|e| at(slot(e, cols)), multi)?;
                node = ir::Node::Sort { input: Box::new(node), keys };
            }
            node = first_of(node, &items_at, &|e| at(slot(e, cols)), multi)?;
            items_at
        };
        let columns: Vec<String> = items.into_iter().map(|i| i.0).collect();
        // A multi-valued column selected as it is (not a group's value).
        let out_multi: Vec<bool> = items_at.iter().map(|x| !grouped && matches!(x, ir::Expr::Output(i) if multi.get(*i as usize) == Some(&true))).collect();
        let project = |node: ir::Node| ir::Node::Project {
            input: Box::new(node),
            columns: columns.iter().zip(items_at).map(|(name, expr)| ir::Column { name: name.clone(), expr }).collect(),
        };
        let cut = |node: ir::Node| match (offset, limit) {
            (0, None) => node,
            (o, l) => ir::Node::Limit { input: Box::new(node), offset: o as u64, limit: l.map(|l| l as u64) },
        };
        let node = match s.distinct {
            Some(sql::Distinct::Distinct) => cut(ir::Node::Distinct { input: Box::new(project(node)) }),
            _ => project(cut(node)),
        };
        Ok(Planned { node, columns, keyed: None, multi: out_multi })
    }
}

/// [`Planned::multi`] of a leaf's columns.
fn multi_of(schema: &Schema, select: &Select) -> Vec<bool> {
    select.projection.iter().map(|(_, p)| matches!(p, Projection::Field(f) if schema.fields[*f].multi)).collect()
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
