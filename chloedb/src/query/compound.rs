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

use super::function::FunctionSet;
use super::budget::Budget;
use std::cell::RefCell;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use std::collections::HashSet;

use sqlparser::ast::{self as sql, BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, SelectItem, TableFactor, UnaryOperator};

use crate::error::{Error, Result};
use crate::query::exec::{ExecOptions, ResultSet};
use crate::query::hint;
use crate::query::ir::eval;
use crate::query::ir::{self, ArithOp, CmpOp, Func, When};
use crate::query::ops;
use crate::query::sql::{CastTo, Sub, Subs};
use crate::query::logical::{Fraction, OrderBy, Phase, Select};
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
pub(crate) const AGGREGATES: [&str; 9] = ["count", "sum", "avg", "min", "max", "median", "percentile_cont", "percentile_disc", "approx_percentile"];
/// Statistical aggregates, which [`stats_as_sums`] writes as sums and
/// counts: every path that aggregates (a shard's partial groups among
/// them) computes those.
pub(crate) const STATS: [&str; 9] = ["stddev", "stddev_samp", "stddev_pop", "variance", "var_samp", "var_pop", "covar_samp", "covar_pop", "corr"];
/// The percentiles among the aggregates: `MEDIAN(x)`, and `PERCENTILE_CONT(x,
/// p)` as `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY x)` parses
/// ([`super::parse`]), and `PERCENTILE_DISC` alike, and
/// `approx_percentile(x, p)`.
pub(crate) const PERCENTILES: [&str; 4] = ["median", "percentile_cont", "percentile_disc", "approx_percentile"];
/// Functions whose value a ranking computes (a score, a graph's, a model's,
/// a set function's, a row's sequence number), not the row evaluator.
const RANKING: [&str; 18] = [
    "bm25", "similarity", "distance", "maxsim", "rrf", "ppr", "decay", "hops", "score", "seq_no", "ranked", "features", "minmax", "rank", "gbdt", "cross_encoder", "rank_profile", "match",
];

fn fname(f: &sql::Function) -> String {
    f.name.to_string().to_ascii_lowercase()
}

/// Whether `f` is an aggregate's call (`SUM(x)`), not a window function's
/// (`SUM(x) OVER (..)`).
fn is_aggregate(f: &sql::Function) -> bool {
    AGGREGATES.contains(&fname(f).as_str()) && f.over.is_none()
}

/// Whether `e` (outside subqueries) calls a window function.
fn has_window(e: &Expr) -> bool {
    let mut found = false;
    walk(e, &mut |x| found |= matches!(x, Expr::Function(f) if f.over.is_some()));
    found
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
    if s.from.len() != 1 || !s.from[0].joins.is_empty() || s.distinct.is_some() || per_capped(q) || computes_columns(q, s) || reads_paths(q, s) || windowed(q, s) {
        return true;
    }
    match &s.from[0].relation {
        TableFactor::Table { name, .. } if is_table(name, schema) => {}
        _ => return true,
    }
    if orders_by_text(q, s, schema) || orders_by_row_values(q, s) || filters_rows(s, schema) || orders_by_several(q, s, schema) || aggregates_rows(q, s, schema) {
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
    // A path the answer has as a column is named as the query wrote it
    // (`attrs.color`), not by the call it becomes.
    if let sql::SetExpr::Select(s) = q.body.as_mut() {
        for item in s.projection.iter_mut() {
            let SelectItem::UnnamedExpr(e) = item else { continue };
            if schemas.iter().any(|s| matches!(super::sql::path_column(s, e), Ok(Some(_)))) {
                let alias = sql::Ident::with_quote('"', e.to_string());
                *item = SelectItem::ExprWithAlias { expr: e.clone(), alias };
            }
        }
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
                | sql::JoinOperator::LeftOuter(sql::JoinConstraint::On(on))
                | sql::JoinOperator::FullOuter(sql::JoinConstraint::On(on)) => walk_mut(on, f),
                sql::JoinOperator::AsOf { match_condition, constraint } => {
                    walk_mut(match_condition, f);
                    if let sql::JoinConstraint::On(on) = constraint {
                        walk_mut(on, f);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Names the columns of each `WITH` result and derived table in `q` as SQL
/// names them: a column of a table in its FROM, written without an alias
/// as `s.item`, is the column `item` (as PostgreSQL and SQLite name it),
/// so the query reading it says `ls.item`. One whose name another of its
/// columns has too keeps `s.item`, and so does a column of the answer
/// itself, where the qualifier tells a join's sides apart.
pub(crate) fn bare_column_names(q: &mut sql::Query) {
    if let Some(w) = &mut q.with {
        for cte in &mut w.cte_tables {
            if cte.alias.columns.is_empty() {
                name_bare(&mut cte.query);
            }
            bare_column_names(&mut cte.query);
        }
    }
    let mut inner = |q: &mut sql::Query| bare_column_names(q);
    match q.body.as_mut() {
        sql::SetExpr::Select(s) => derived_tables(s, &mut inner),
        sql::SetExpr::Query(sq) => bare_column_names(sq),
        sql::SetExpr::SetOperation { left, right, .. } => {
            for side in [left, right] {
                if let sql::SetExpr::Select(s) = side.as_mut() {
                    derived_tables(s, &mut inner);
                }
            }
        }
        _ => {}
    }
    query_mut(q, &mut |e| {
        if let Expr::Subquery(sq) | Expr::Exists { subquery: sq, .. } | Expr::InSubquery { subquery: sq, .. } = e {
            bare_column_names(sq);
        }
    });
}

/// Each derived table of `s`'s FROM, its columns named by
/// [`bare_column_names`] (unless its alias names them), then `f` of it.
fn derived_tables(s: &mut sql::Select, f: &mut dyn FnMut(&mut sql::Query)) {
    for t in &mut s.from {
        for r in std::iter::once(&mut t.relation).chain(t.joins.iter_mut().map(|j| &mut j.relation)) {
            if let TableFactor::Derived { subquery, alias, .. } = r {
                if alias.as_ref().is_none_or(|a| a.columns.is_empty()) {
                    name_bare(subquery);
                }
                f(subquery);
            }
        }
    }
}

/// [`bare_column_names`] of one query's columns: those of its SELECT, or
/// of a set operation's first SELECT, which names its columns.
fn name_bare(q: &mut sql::Query) {
    let mut body = q.body.as_mut();
    loop {
        match body {
            sql::SetExpr::SetOperation { left, .. } => body = left.as_mut(),
            sql::SetExpr::Select(s) => break name_bare_select(s),
            // A query in parentheses names its own.
            _ => break,
        }
    }
}

fn name_bare_select(s: &mut sql::Select) {
    let joined = s.from.len() > 1 || s.from.iter().any(|t| !t.joins.is_empty());
    // Over one table, `*` gives its columns by their own names, which a
    // column named here might be too.
    if !joined && s.projection.iter().any(|i| matches!(i, SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..))) {
        return;
    }
    let mut tables: Vec<String> = Vec::new();
    for t in &s.from {
        for r in std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)) {
            match r {
                TableFactor::Table { name, alias, .. } => {
                    tables.push(super::sql::table_name(name).to_ascii_lowercase());
                    tables.extend(alias.iter().map(|a| a.name.value.to_ascii_lowercase()));
                }
                TableFactor::Derived { alias: Some(a), .. } => tables.push(a.name.value.to_ascii_lowercase()),
                _ => {}
            }
        }
    }
    // The column a select item would be named by its column's own name.
    let bare = |item: &SelectItem| match item {
        SelectItem::UnnamedExpr(Expr::CompoundIdentifier(ids)) if ids.len() == 2 && tables.contains(&ids[0].value.to_ascii_lowercase()) => Some(ids[1].clone()),
        _ => None,
    };
    let name = |item: &SelectItem| match item {
        SelectItem::UnnamedExpr(e) => bare(item).map_or_else(|| super::sql::name_of(e), |id| id.value),
        SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
        _ => String::new(),
    };
    let names: Vec<String> = s.projection.iter().map(|i| name(i).to_ascii_lowercase()).collect();
    for item in s.projection.iter_mut() {
        let Some(id) = bare(item) else { continue };
        if names.iter().filter(|n| **n == id.value.to_ascii_lowercase()).count() > 1 {
            continue;
        }
        let SelectItem::UnnamedExpr(e) = item else { continue };
        *item = SelectItem::ExprWithAlias { expr: e.clone(), alias: id };
    }
}

/// Writes each `OVER w` and `OVER (w ORDER BY ..)` of `q`, and of the
/// queries inside it, out as the window it names (`WINDOW w AS (..)`), as
/// PostgreSQL does: `OVER (w ..)` takes `w`'s partition and adds an order
/// or a frame, and `w` names a window without a frame when it adds one.
pub(crate) fn inline_windows(q: &mut sql::Query) -> Result<()> {
    // The queries inside first, each with its own WINDOW clause.
    if let Some(w) = &mut q.with {
        for cte in &mut w.cte_tables {
            inline_windows(&mut cte.query)?;
        }
    }
    let mut failed = None;
    if let sql::SetExpr::Select(s) = q.body.as_mut() {
        for t in &mut s.from {
            let relations = std::iter::once(&mut t.relation).chain(t.joins.iter_mut().map(|j| &mut j.relation));
            for r in relations {
                if let TableFactor::Derived { subquery, .. } = r {
                    inline_windows(subquery)?;
                }
            }
        }
    }
    query_mut(q, &mut |e| match e {
        Expr::Subquery(sq) | Expr::Exists { subquery: sq, .. } | Expr::InSubquery { subquery: sq, .. } if failed.is_none() => {
            if let Err(x) = inline_windows(sq) {
                failed = Some(x);
            }
        }
        _ => {}
    });
    if let Some(e) = failed {
        return Err(e);
    }
    let sql::SetExpr::Select(s) = q.body.as_mut() else { return Ok(()) };
    if s.named_window.is_empty() {
        // Windows written out already; a name with none to name is an
        // error below.
        let mut named = false;
        query_mut(q, &mut |e| named |= matches!(e, Expr::Function(f) if matches!(&f.over, Some(sql::WindowType::NamedWindow(_))) || matches!(&f.over, Some(sql::WindowType::WindowSpec(w)) if w.window_name.is_some())));
        if !named {
            return Ok(());
        }
    }
    let sql::SetExpr::Select(s) = q.body.as_mut() else { return Ok(()) };
    // A column the query names is named as the query wrote it (`RANK()
    // OVER w`), not by the window it becomes.
    for item in s.projection.iter_mut() {
        let SelectItem::UnnamedExpr(e) = item else { continue };
        if has_window(e) {
            let alias = sql::Ident::with_quote('"', super::sql::name_of(e));
            *item = SelectItem::ExprWithAlias { expr: e.clone(), alias };
        }
    }
    // The named windows, each written out over the ones before.
    let mut named: Vec<(String, sql::WindowSpec)> = Vec::new();
    for sql::NamedWindowDefinition(name, def) in std::mem::take(&mut s.named_window) {
        let spec = match def {
            sql::NamedWindowExpr::NamedWindow(other) => spec_of(&sql::WindowType::NamedWindow(other), &named)?,
            sql::NamedWindowExpr::WindowSpec(w) => spec_of(&sql::WindowType::WindowSpec(w), &named)?,
        };
        if named.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name.value)) {
            return Err(err(format!("WINDOW {name} is named twice")));
        }
        named.push((name.value, spec));
    }
    query_mut(q, &mut |e| {
        let Expr::Function(f) = e else { return };
        let Some(over) = &mut f.over else { return };
        if failed.is_some() {
            return;
        }
        match spec_of(over, &named) {
            Ok(spec) => *over = sql::WindowType::WindowSpec(spec),
            Err(x) => failed = Some(x),
        }
    });
    failed.map_or(Ok(()), Err)
}

/// The window `over` is, written out over the `named` windows.
fn spec_of(over: &sql::WindowType, named: &[(String, sql::WindowSpec)]) -> Result<sql::WindowSpec> {
    let find = |n: &sql::Ident| named.iter().find(|(x, _)| x.eq_ignore_ascii_case(&n.value)).map(|(_, w)| w.clone()).ok_or_else(|| err(format!("no window named {n}: name it in a WINDOW clause, WINDOW {n} AS (PARTITION BY .. ORDER BY ..)")));
    match over {
        sql::WindowType::NamedWindow(n) => find(n),
        sql::WindowType::WindowSpec(w) => {
            let Some(n) = &w.window_name else { return Ok(w.clone()) };
            let base = find(n)?;
            if !w.partition_by.is_empty() {
                return Err(err(format!("OVER ({n} PARTITION BY ..): a window over {n} keeps its partition; partition in WINDOW {n} AS (..)")));
            }
            if !base.order_by.is_empty() && !w.order_by.is_empty() {
                return Err(err(format!("OVER ({n} ORDER BY ..): {n} has an order already")));
            }
            if base.window_frame.is_some() {
                return Err(err(format!("OVER ({n} ..): {n} has a frame, which a window over it cannot change; write OVER {n}")));
            }
            Ok(sql::WindowSpec {
                window_name: None,
                partition_by: base.partition_by,
                order_by: if w.order_by.is_empty() { base.order_by } else { w.order_by.clone() },
                window_frame: w.window_frame.clone(),
            })
        }
    }
}

/// Whether a SELECT calls window functions (in its columns, ORDER BY or
/// QUALIFY) or names windows: operators compute them over its rows.
fn windowed(q: &sql::Query, s: &sql::Select) -> bool {
    !s.named_window.is_empty()
        || s.qualify.is_some()
        || select_exprs(s).into_iter().any(has_window)
        || q.order_by.as_ref().is_some_and(|ob| matches!(&ob.kind, sql::OrderByKind::Expressions(es) if es.iter().any(|e| has_window(&e.expr))))
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

/// Whether `c`, a condition of a table's scan, is one its indexes cannot
/// answer but the row evaluator can, over the rows the scan returns: a
/// comparison of an expression of columns (`lower(brand) = 'acme'`,
/// `price * 2 > 10`), of two columns (`price > cost`), a `LIKE` that is
/// no value or prefix (`'%tent%'`) or of stored text, any `ILIKE`, a
/// comparison or `IS NULL` of a stored field the index does not hold
/// (`title = 'x'`, `note IS NULL`), and `AND`, `OR` and `NOT` of these
/// with other conditions. A condition with a search, a score, a graph
/// walk, a subquery or a JSON path stays the scan's, as does one naming
/// a field the scan cannot return (generated text, vectors): there the
/// binder says what is wrong with it.
pub(crate) fn of_rows_filter(c: &Expr, schema: &Schema) -> bool {
    let mut blocked = false;
    walk(c, &mut |x| {
        blocked |= match x {
            // A column of a correlated subquery's outer row: compared only
            // with the table's own columns, which the scan's filter does.
            Expr::Function(_) if outer_field(x).is_some() => true,
            Expr::Function(f) => {
                let n = fname(f);
                NATIVE.contains(&n.as_str()) || RANKING.contains(&n.as_str()) || AGGREGATES.contains(&n.as_str()) || f.over.is_some() || matches!(n.as_str(), "prefix" | "at_least" | "count_true" | "json_extract" | "sid_at" | "encode")
            }
            Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => true,
            // A field the scan does not return, or a path into a JSON field.
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => match column_field(x, schema) {
                Some(f) => !f.stored || matches!(f.ty, FieldType::Vector { .. } | FieldType::Sparse | FieldType::Edges | FieldType::Json) || f.media.is_some(),
                // An alias, a path, or no column at all: the binder's.
                None => true,
            },
            _ => false,
        };
    });
    !blocked && needs_rows(c, schema)
}

/// The field a column reference (`brand`, `p.brand`) names in `schema`.
fn column_field<'a>(e: &Expr, schema: &'a Schema) -> Option<&'a crate::schema::Field> {
    match e {
        Expr::Identifier(id) => schema.field(&id.value),
        Expr::CompoundIdentifier(ids) if ids.len() == 2 => schema.field(&ids[1].value),
        _ => None,
    }
}

/// [`of_rows_filter`] once `c` holds nothing but values, columns the scan
/// returns and the row evaluator's functions: whether some part of it is
/// beyond the scan's binder.
fn needs_rows(c: &Expr, schema: &Schema) -> bool {
    let plain = |e: &Expr| column_field(e, schema);
    // A side with a column that is not just the column.
    let computed = |e: &Expr| {
        let mut cols = false;
        walk(e, &mut |x| cols |= column_field(x, schema).is_some());
        cols && plain(e).is_none()
    };
    // A field the scan's own filters cannot test: stored text, or one it
    // does not index.
    let unindexed = |e: &Expr| plain(e).is_some_and(|f| !f.indexed || matches!(f.ty, FieldType::Text { .. }));
    match c {
        Expr::Nested(x) | Expr::UnaryOp { op: UnaryOperator::Not, expr: x } => needs_rows(x, schema),
        Expr::BinaryOp { left, op: BinaryOperator::And | BinaryOperator::Or, right } => needs_rows(left, schema) || needs_rows(right, schema),
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq | BinaryOperator::Lt | BinaryOperator::LtEq | BinaryOperator::Gt | BinaryOperator::GtEq) =>
        {
            computed(left) || computed(right) || (plain(left).is_some() && plain(right).is_some()) || unindexed(left) || unindexed(right)
        }
        Expr::Between { expr, low, high, .. } => computed(expr) || unindexed(expr) || [low, high].iter().any(|b| computed(b) || plain(b).is_some()),
        Expr::InList { expr, list, .. } => computed(expr) || unindexed(expr) || list.iter().any(|v| computed(v) || plain(v).is_some()),
        Expr::IsNull(x) | Expr::IsNotNull(x) => computed(x) || plain(x).is_some_and(|f| !f.indexed || !matches!(f.ty, FieldType::Int | FieldType::Float | FieldType::Keyword)),
        Expr::IsTrue(x) | Expr::IsFalse(x) | Expr::IsNotTrue(x) | Expr::IsNotFalse(x) => computed(x),
        // LIKE and ILIKE test text: of another field, and with ESCAPE, the
        // binder says what is wrong.
        Expr::Like { escape_char: Some(_), .. } | Expr::ILike { escape_char: Some(_), .. } => false,
        Expr::Like { expr, .. } | Expr::ILike { expr, .. } if plain(expr).is_some_and(|f| !matches!(f.ty, FieldType::Keyword | FieldType::Text { .. })) => false,
        // A field with a substring index: the scan answers any pattern.
        Expr::Like { any: false, expr, pattern, .. } | Expr::ILike { any: false, expr, pattern, .. }
            if plain(expr).is_some_and(|f| f.substring) && matches!(pattern.as_ref(), Expr::Value(_)) =>
        {
            false
        }
        Expr::ILike { any: false, .. } => true,
        Expr::Like { any: false, expr, pattern, escape_char, .. } => {
            let prefix_or_value = match pattern.as_ref() {
                Expr::Value(v) => match &v.value {
                    sql::Value::SingleQuotedString(p) | sql::Value::DoubleQuotedString(p) => escape_char.is_some() || !p.trim_end_matches('%').contains(['%', '_']),
                    _ => true,
                },
                _ => true,
            };
            computed(expr) || !plain(expr).is_some_and(|f| f.ty == FieldType::Keyword && f.indexed) || !prefix_or_value
        }
        Expr::Case { .. } => true,
        _ => false,
    }
}

/// Whether a SELECT over one table filters by an [`of_rows_filter`]
/// condition: then the row evaluator filters the scan's rows.
/// Whether a SELECT aggregates a stored column that the executor's
/// aggregates do not read, which take INT, FLOAT, KEYWORD and TIMESTAMP
/// columns: `COUNT(body)`, `COUNT(DISTINCT title)` and `MIN(title)` of a
/// TEXT column, or of a column kept without an index. The row evaluator
/// aggregates its stored values.
fn aggregates_rows(q: &sql::Query, s: &sql::Select, schema: &Schema) -> bool {
    let mut found = false;
    let mut check = |e: &Expr| {
        walk(e, &mut |x| {
            let Expr::Function(f) = x else { return };
            if !is_aggregate(f) || PERCENTILES.contains(&fname(f).as_str()) {
                return;
            }
            let name = match func_args(f).as_slice() {
                [Expr::Identifier(id)] => id.value.clone(),
                [Expr::CompoundIdentifier(ids)] => ids.last().expect("a name").value.clone(),
                _ => return,
            };
            found |= schema.field(&name).is_some_and(|fd| fd.stored && !fd.multi && (matches!(fd.ty, FieldType::Text { .. }) || (!fd.indexed && matches!(fd.ty, FieldType::Keyword | FieldType::Int | FieldType::Float))));
        })
    };
    for item in &s.projection {
        if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
            check(e);
        }
    }
    s.having.iter().for_each(&mut check);
    if let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &q.order_by {
        es.iter().for_each(|o| check(&o.expr));
    }
    found
}

fn filters_rows(s: &sql::Select, schema: &Schema) -> bool {
    s.selection.as_ref().is_some_and(|w| split_and(w).iter().any(|c| of_rows_filter(c, schema)))
}

/// Whether a SELECT over one table orders by more keys than its scan
/// takes: one key, then the primary key (or, grouped, the first group
/// field) to break ties. The rows are then sorted in memory.
fn orders_by_several(q: &sql::Query, s: &sql::Select, schema: &Schema) -> bool {
    let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &q.order_by else { return false };
    let es: Vec<&sql::OrderByExpr> = es.iter().filter(|o| !is_per_cap(&o.expr)).collect();
    match es.as_slice() {
        [] | [_] => false,
        [_, second] => {
            // A position names a selected column.
            let named = |e: &Expr| -> Option<String> {
                let e = match super::sql::literal_usize(e) {
                    Some(n) => match s.projection.get(n.wrapping_sub(1)) {
                        Some(SelectItem::UnnamedExpr(e)) | Some(SelectItem::ExprWithAlias { expr: e, .. }) => e,
                        _ => return None,
                    },
                    None => e,
                };
                match e {
                    Expr::Identifier(id) => Some(id.value.clone()),
                    Expr::CompoundIdentifier(ids) => ids.last().map(|i| i.value.clone()),
                    _ => None,
                }
            };
            let Some(name) = named(&second.expr) else { return true };
            let grouped = !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty());
            if grouped {
                let first = match &s.group_by {
                    sql::GroupByExpr::Expressions(gs, _) => gs.first().and_then(named),
                    _ => None,
                };
                !first.is_some_and(|g| g.eq_ignore_ascii_case(&name))
            } else if has_aggregate(s) {
                false
            } else {
                !schema.primary_key.is_some_and(|k| schema.fields[k].name.eq_ignore_ascii_case(&name))
            }
        }
        _ => true,
    }
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
        Expr::Function(f) if is_aggregate(f) => {
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

/// Whether `q`, a query of one table that does not group, orders by a
/// value of its rows that the scan's ranking does not compute, which
/// takes numbers and scores: a text function, a hash (`ORDER BY hash(id,
/// 7)`, a seeded shuffle) or a part of a time, and no score. The row
/// evaluator sorts by it, over the rows the table's query finds.
fn orders_by_row_values(q: &sql::Query, s: &sql::Select) -> bool {
    let grouped = !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty()) || has_aggregate(s);
    let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &q.order_by else { return false };
    let mut found = false;
    for o in es {
        if ranks(&o.expr) {
            return false;
        }
        walk(&o.expr, &mut |x| {
            found |= match x {
                Expr::Function(f) => matches!(fname(f).as_str(), "lower" | "upper" | "length" | "hash" | "date_trunc" | "date_part" | "strftime" | "date_bin" | "time_bucket"),
                Expr::Extract { .. } => true,
                _ => false,
            }
        });
    }
    !grouped && found
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
    out.extend(s.qualify.iter());
    if let sql::GroupByExpr::Expressions(es, _) = &s.group_by {
        out.extend(es.iter());
    }
    out
}

/// Calls `f` on `e` and then its subexpressions, outside subqueries.
pub(crate) fn walk(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    match e {
        Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } | Expr::IsNull(x) | Expr::IsNotNull(x) | Expr::Cast { expr: x, .. } | Expr::Extract { expr: x, .. } => walk(x, f),
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
            if let Some(sql::WindowType::WindowSpec(w)) = &func.over {
                w.partition_by.iter().for_each(|x| walk(x, f));
                w.order_by.iter().for_each(|o| walk(&o.expr, f));
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

/// Writes every `agg(x) FILTER (WHERE c)` of `q` (and its subqueries, `WITH`
/// queries and set operations' arms) as the aggregate of the values the
/// condition keeps, `agg(CASE WHEN c THEN x END)`: an aggregate skips the
/// NULL the other rows give, so the two are one in SQL. `COUNT(*) FILTER
/// (WHERE c)` is `COUNT(CASE WHEN c THEN 1 END)`. A percentile's fraction
/// stays as written; `CORR(y, x)` takes the condition on both values.
/// FILTER on a call that is not an aggregate is refused, and a window's
/// aggregate refuses it where it is bound ([`window_fn`]).
pub(crate) fn filters_as_case(q: &mut sql::Query) -> Result<()> {
    let mut failed = None;
    let mut rewrite = |e: &mut Expr| {
        let Expr::Function(f) = e else { return };
        if f.filter.is_none() || f.over.is_some() {
            return;
        }
        let name = fname(f);
        if !AGGREGATES.contains(&name.as_str()) && !STATS.contains(&name.as_str()) {
            failed.get_or_insert_with(|| err(format!("{name}(..) FILTER (WHERE ..): FILTER applies to an aggregate ({}), and {name} is not one", AGGREGATES.join(", ").to_uppercase())));
            return;
        }
        let cond = *f.filter.take().expect("a FILTER");
        let case = |x: Expr| Expr::Case {
            case_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
            end_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
            operand: None,
            conditions: vec![sql::CaseWhen { condition: Expr::Nested(Box::new(cond.clone())), result: x }],
            else_result: None,
        };
        let FunctionArguments::List(list) = &mut f.args else {
            failed.get_or_insert_with(|| err(format!("{name}(..) FILTER (WHERE ..) names no value to aggregate")));
            return;
        };
        // Which arguments are values: a percentile's second is its fraction.
        let values = match name.as_str() {
            "percentile_cont" | "percentile_disc" => 1,
            _ => list.args.len(),
        };
        for a in list.args.iter_mut().take(values) {
            match a {
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => *a = FunctionArg::Unnamed(FunctionArgExpr::Expr(case(Expr::value(sql::Value::Number("1".into(), false))))),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => *x = case(x.clone()),
                _ => {
                    failed.get_or_insert_with(|| err(format!("{name}(..) FILTER (WHERE ..): the aggregate's value is an expression")));
                }
            }
        }
        if list.args.is_empty() {
            // `COUNT()`, as `COUNT(*)`.
            list.args.push(FunctionArg::Unnamed(FunctionArgExpr::Expr(case(Expr::value(sql::Value::Number("1".into(), false))))));
        }
    };
    keep_names(q, &|e| {
        let mut found = false;
        walk(e, &mut |x| found |= matches!(x, Expr::Function(f) if f.filter.is_some() && f.over.is_none()));
        found
    });
    filters_in(q, &mut rewrite);
    failed.map_or(Ok(()), Err)?;
    stats_as_sums(q)
}

/// Writes `STDDEV(x)`, `VARIANCE(x)` (and their `_SAMP` and `_POP` forms),
/// `COVAR_SAMP(y, x)`, `COVAR_POP(y, x)` and `CORR(y, x)` as the sums and
/// counts they are computed from, in one pass over the values (the sum of
/// squares less the square of the sum over n): `COUNT`, `SUM` and
/// arithmetic, which every path that aggregates computes, a shard's
/// partial groups among them. As in PostgreSQL, the sample forms (and
/// `STDDEV`, `VARIANCE`) are NULL for fewer than two values, the
/// population forms for none, and `CORR` when either variable does not
/// vary; `COVAR` and `CORR` take the rows where both values are there.
fn stats_as_sums(q: &mut sql::Query) -> Result<()> {
    let is_stat = |x: &Expr| matches!(x, Expr::Function(f) if f.over.is_none() && STATS.contains(&fname(f).as_str()));
    keep_names(q, &|e| {
        let mut found = false;
        walk(e, &mut |x| found |= is_stat(x));
        found
    });
    let mut failed = None;
    let mut rewrite = |e: &mut Expr| {
        if !is_stat(e) {
            return;
        }
        let Expr::Function(f) = &*e else { return };
        let name = fname(f);
        match stat_sql(f, &name).and_then(|text| super::parse::parse_expr(&text)) {
            Ok(x) => *e = Expr::Nested(Box::new(x)),
            Err(x) => {
                failed.get_or_insert(x);
            }
        }
    };
    filters_in(q, &mut rewrite);
    failed.map_or(Ok(()), Err)
}

/// The SQL of a statistical aggregate's call `f` ([`stats_as_sums`]).
fn stat_sql(f: &sql::Function, name: &str) -> Result<String> {
    let upper = name.to_uppercase();
    if matches!(&f.args, FunctionArguments::List(l) if l.duplicate_treatment == Some(sql::DuplicateTreatment::Distinct)) {
        return Err(err(format!("{upper}(DISTINCT ..) is not supported")));
    }
    if !f.within_group.is_empty() {
        return Err(err(format!("{upper}(..) takes no WITHIN GROUP")));
    }
    let args = func_args(f);
    let two = matches!(name, "covar_samp" | "covar_pop" | "corr");
    let (y, x) = match (two, args.as_slice()) {
        (false, [x]) => (None, format!("({x})")),
        (true, [y, x]) => (Some(format!("({y})")), format!("({x})")),
        (false, _) => return Err(err(format!("{upper} takes one value: {upper}(price)"))),
        (true, _) => return Err(err(format!("{upper} takes two values, y and x: {upper}(rating, price)"))),
    };
    // Sum of squared deviations: Σa·b − Σa·Σb / n.
    let dev = |a: &str, b: &str, n: &str| format!("(SUM({a} * {b} * 1.0) - SUM({a} * 1.0) * SUM({b} * 1.0) / {n})");
    let nonneg = |v: String| format!("(CASE WHEN {v} < 0 THEN 0.0 ELSE {v} END)");
    Ok(match y {
        None => {
            let n = format!("COUNT({x})");
            let ss = nonneg(dev(&x, &x, &n));
            let (var, least) = match name {
                "var_pop" | "stddev_pop" => (format!("{ss} / {n}"), 0),
                _ => (format!("{ss} / ({n} - 1)"), 1),
            };
            let var = format!("CASE WHEN {n} > {least} THEN {var} END");
            match name.starts_with("stddev") {
                true => format!("sqrt({var})"),
                false => var,
            }
        }
        Some(y) => {
            // The pairs with both values.
            let (xp, yp) = (format!("(CASE WHEN {y} IS NOT NULL THEN {x} END)"), format!("(CASE WHEN {x} IS NOT NULL THEN {y} END)"));
            let n = format!("COUNT({xp})");
            let sxy = dev(&xp, &yp, &n);
            match name {
                "covar_pop" => format!("CASE WHEN {n} > 0 THEN {sxy} / {n} END"),
                "covar_samp" => format!("CASE WHEN {n} > 1 THEN {sxy} / ({n} - 1) END"),
                _ => {
                    let (sxx, syy) = (dev(&xp, &xp, &n), dev(&yp, &yp, &n));
                    format!("CASE WHEN {n} > 1 AND {sxx} > 0 AND {syy} > 0 THEN {sxy} / sqrt({sxx} * {syy}) END")
                }
            }
        }
    })
}

/// Names each column of `q`'s SELECTs (its set operations' arms, `WITH`
/// queries and derived tables too) that `rewritten` says a rewrite will
/// change, by its SQL as written, the name it has without an alias.
fn keep_names(q: &mut sql::Query, rewritten: &dyn Fn(&Expr) -> bool) {
    fn body(b: &mut sql::SetExpr, rewritten: &dyn Fn(&Expr) -> bool) {
        match b {
            sql::SetExpr::SetOperation { left, right, .. } => {
                body(left, rewritten);
                body(right, rewritten);
            }
            sql::SetExpr::Query(q) => keep_names(q, rewritten),
            sql::SetExpr::Select(s) => {
                for item in &mut s.projection {
                    if let SelectItem::UnnamedExpr(e) = item {
                        if rewritten(e) {
                            let alias = sql::Ident::with_quote('"', super::sql::name_of(e));
                            *item = SelectItem::ExprWithAlias { expr: e.clone(), alias };
                        }
                    }
                }
                for t in &mut s.from {
                    if let TableFactor::Derived { subquery, .. } = &mut t.relation {
                        keep_names(subquery, rewritten);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(w) = &mut q.with {
        for cte in &mut w.cte_tables {
            keep_names(&mut cte.query, rewritten);
        }
    }
    body(&mut q.body, rewritten);
}

/// [`query_mut`] over `q` and every arm of its set operations.
fn filters_in(q: &mut sql::Query, f: &mut dyn FnMut(&mut Expr)) {
    fn body(b: &mut sql::SetExpr, f: &mut dyn FnMut(&mut Expr)) {
        match b {
            sql::SetExpr::SetOperation { left, right, .. } => {
                body(left, f);
                body(right, f);
            }
            sql::SetExpr::Query(q) => filters_in(q, f),
            sql::SetExpr::Select(_) => {}
            _ => {}
        }
    }
    if let Some(w) = &mut q.with {
        for cte in &mut w.cte_tables {
            filters_in(&mut cte.query, f);
        }
    }
    if matches!(q.body.as_ref(), sql::SetExpr::Select(_)) {
        // `query_mut` reads the WITH queries too: once is enough.
        let with = q.with.take();
        query_mut(q, f);
        q.with = with;
    } else {
        body(&mut q.body, f);
        if let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &mut q.order_by {
            es.iter_mut().for_each(|o| walk_mut(&mut o.expr, f));
        }
    }
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
    let q = parse(sql_text)?;
    check_budget(&q, params)?;
    plan_query(&q, catalog, params)
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
/// tables with it. It calls no stored function: [`plan_read_calling`]
/// does.
pub fn plan_read(sql_text: &str, catalog: &[&Schema], params: &Params, profiles: Profiles) -> Result<ir::Plan> {
    plan_read_calling(sql_text, catalog, params, profiles, &FunctionSet::default())
}

/// The limits of the plans made on one thread ([`planning_under`]).
#[derive(Clone, Default)]
struct Planning {
    deadline: Option<Instant>,
    cancel: Option<Arc<AtomicBool>>,
    /// The statement's budget ([`super::budget`]).
    budget: Budget,
    /// When the plan's `plan_ms` runs out, and the budget.
    plan_by: Option<(Instant, u64)>,
}

thread_local! {
    /// The deadline, cancel flag and budget of the plans made on this
    /// thread ([`planning_under`]).
    static PLANNING: RefCell<Planning> = RefCell::new(Planning::default());
}

/// `f`, whose plans ([`plan_read_calling`] and the rest) stop with
/// [`Error::Timeout`] once `opts`' deadline passes, its cancel flag is
/// set or its budget's `plan_ms` runs out from now, and are refused once
/// written out past its budget's `nodes`, `depth` or `list`
/// ([`super::budget`]): a statement whose planning takes long (thousands
/// of `WITH` results, say) gives up when its query would, rather than
/// holding a thread past the caller's wait.
pub fn planning_under<T>(opts: &ExecOptions, f: impl FnOnce() -> T) -> T {
    struct Restore(Planning);
    impl Drop for Restore {
        fn drop(&mut self) {
            let before = std::mem::take(&mut self.0);
            PLANNING.with(|p| *p.borrow_mut() = before);
        }
    }
    let now = Planning {
        deadline: opts.deadline,
        cancel: opts.cancel.clone(),
        budget: opts.budget,
        plan_by: opts.budget.plan_ms.map(|ms| (Instant::now() + std::time::Duration::from_millis(ms), ms)),
    };
    let _restore = Restore(PLANNING.with(|p| p.replace(now)));
    f()
}

/// The budget of the statement being planned on this thread
/// ([`planning_under`]).
pub(crate) fn planning_budget() -> Budget {
    PLANNING.with(|p| p.borrow().budget)
}

/// [`Error::Timeout`] once the plan being made ran past the deadline or
/// its budget's `plan_ms`, or was cancelled ([`planning_under`]).
pub(crate) fn check_planning() -> Result<()> {
    PLANNING.with(|p| {
        let p = p.borrow();
        if p.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err(Error::Timeout("the query was cancelled while it was planned".into()));
        }
        let now = Instant::now();
        if let Some((_, ms)) = p.plan_by.filter(|(by, _)| now >= *by) {
            return Err(Error::Timeout(format!("the statement took more than {ms} ms to plan, its budget (plan_ms): write it with fewer WITH results, set operations or calls, or raise the budget")));
        }
        match p.deadline {
            Some(d) if now >= d => Err(Error::Timeout("the query ran past its deadline while it was planned".into())),
            _ => Ok(()),
        }
    })
}

/// [`check_planning`] every 1,024 calls of `n` (a loop over values as
/// a statement binds).
pub(crate) fn check_planning_every(n: usize) -> Result<()> {
    match n % 1024 {
        1023 => check_planning(),
        _ => Ok(()),
    }
}

/// A query error when statement `sql` as written (a write, which calls
/// no stored function) is past the shape the budget of the statement
/// being planned allows; nothing is parsed when it bounds none.
pub(crate) fn check_statement(sql_text: &str, params: &Params) -> Result<()> {
    super::budget::check_text(&planning_budget(), sql_text, params)
}

/// A query error when `q`, written out, is past the shape the budget of
/// the statement being planned allows ([`super::budget::check_shape`]).
fn check_budget(q: &sql::Query, params: &Params) -> Result<()> {
    let b = planning_budget();
    if b.is_none() {
        return Ok(());
    }
    check_planning()?;
    super::budget::check_shape(&b, q, params)
}

/// [`plan_read`] of a query that may call the stored functions of
/// `functions`, which plan as written out ([`super::function::expand`]).
pub fn plan_read_calling(sql_text: &str, catalog: &[&Schema], params: &Params, profiles: Profiles, functions: &FunctionSet) -> Result<ir::Plan> {
    plan_read_parsed(&*super::sql::parse_query_cached(sql_text)?, catalog, params, profiles, functions)
}

/// [`plan_read_calling`] of a query [`super::sql::parse_query_cached`]
/// parsed.
pub(crate) fn plan_read_parsed(p: &super::sql::Parsed, catalog: &[&Schema], params: &Params, profiles: Profiles, functions: &FunctionSet) -> Result<ir::Plan> {
    plan_read_with(&p.query, catalog, params, profiles, functions, p.profiles)
}

/// [`plan_read_calling`] of a parsed query.
pub(crate) fn plan_read_query(q: &sql::Query, catalog: &[&Schema], params: &Params, profiles: Profiles, functions: &FunctionSet) -> Result<ir::Plan> {
    plan_read_with(q, catalog, params, profiles, functions, true)
}

/// [`plan_read_query`]; `may_rank_by_profile` false for a query whose text
/// names no `rank_profile`.
fn plan_read_with(q: &sql::Query, catalog: &[&Schema], params: &Params, profiles: Profiles, functions: &FunctionSet, may_rank_by_profile: bool) -> Result<ir::Plan> {
    // Stored functions are written out first: a table function's body may
    // rank by a rank profile.
    if let Some((q, _)) = super::function::expand(q, functions)? {
        return plan_read_with(&q, catalog, params, profiles, &FunctionSet::default(), true);
    }
    let mut rewritten;
    let q = match catalog.iter().any(|s| s.fields.iter().any(|f| f.ty == FieldType::Json)) {
        true => {
            rewritten = q.clone();
            json_paths(&mut rewritten, catalog)?;
            &rewritten
        }
        false => q,
    };
    if let Some((q, r)) = if may_rank_by_profile { super::profile::expand(q, profiles)? } else { None } {
        let table = super::tables::read_by(&q).first().cloned().unwrap_or_default();
        return Ok(super::profile::mark(plan_read_query(&q, catalog, params, &|_, _| None, functions)?, &table, &r));
    }
    // Written out, the statement is held to its budget.
    check_budget(q, params)?;
    let schema = match super::tables::read_by(q).first() {
        None => *catalog.first().ok_or_else(|| err("the database has no tables"))?,
        Some(t) => catalog.iter().copied().find(|s| s.table.eq_ignore_ascii_case(t)).ok_or_else(|| err(hint::no_table(t, catalog.iter().map(|s| s.table.as_str()))))?,
    };
    if needs(q, schema) {
        return plan_query(q, catalog, params);
    }
    Ok(ir::Plan::of_select(schema, &super::sql::bind_query_in(q, schema, catalog, params)?))
}

/// [`plan_in`] of a parsed query.
pub(crate) fn plan_query(q: &sql::Query, catalog: &[&Schema], params: &Params) -> Result<ir::Plan> {
    plan_query_fused(q, catalog, params, true)
}

/// [`plan_query`]; with `fuse`, the executor reads the paths of JSON
/// fields outside WHERE from the field's terms ([`ir::fuse_paths`]).
fn plan_query_fused(q: &sql::Query, catalog: &[&Schema], params: &Params, fuse: bool) -> Result<ir::Plan> {
    let mut q = q.clone();
    bare_column_names(&mut q);
    json_paths(&mut q, catalog)?;
    inline_windows(&mut q)?;
    let p = Planner { catalog, params, ctes: RefCell::new(Vec::new()), outer: RefCell::new(None) };
    let mut node = p.query(&q)?.node;
    if fuse {
        ir::fuse_paths(&mut node, catalog);
    }
    Ok(ir::Plan::new(node))
}

/// The plan of a compound SELECT ([`plan_read`]) that reads the paths of
/// JSON fields outside WHERE row by row, with operators over each row's
/// object, as plans did before the executor read them from the field's
/// terms: what tests check the executor's answers against.
#[doc(hidden)]
pub fn plan_read_by_rows(sql_text: &str, catalog: &[&Schema], params: &Params) -> Result<ir::Plan> {
    plan_query_fused(&*super::sql::parse_query(sql_text)?, catalog, params, false)
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

/// `q` without its `WITH`, copied without copying the `WITH`'s queries.
fn without_with(q: &sql::Query) -> sql::Query {
    let sql::Query { with: _, body, order_by, limit_clause, fetch, locks, for_clause, settings, format_clause, pipe_operators } = q;
    sql::Query {
        with: None,
        body: body.clone(),
        order_by: order_by.clone(),
        limit_clause: limit_clause.clone(),
        fetch: fetch.clone(),
        locks: locks.clone(),
        for_clause: for_clause.clone(),
        settings: settings.clone(),
        format_clause: format_clause.clone(),
        pipe_operators: pipe_operators.clone(),
    }
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

/// The columns of a rerank step's projection, for the `*` of the step
/// after it: the table's `*`, and each column of the table by its name.
/// `None` when it selects anything else (`score()`, `hops()`), whose value
/// the step after would not read.
fn columns_of_step(projection: &[SelectItem]) -> Option<Vec<SelectItem>> {
    projection
        .iter()
        .map(|item| match item {
            SelectItem::Wildcard(_) => Some(item.clone()),
            SelectItem::UnnamedExpr(Expr::Identifier(id)) => Some(SelectItem::UnnamedExpr(Expr::Identifier(id.clone()))),
            // A column under its own name, in some case (`id AS ID`).
            SelectItem::ExprWithAlias { expr: Expr::Identifier(id), alias } if id.value.eq_ignore_ascii_case(&alias.value) => Some(SelectItem::UnnamedExpr(Expr::Identifier(alias.clone()))),
            _ => None,
        })
        .collect()
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
        check_planning()?;
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
        // The query without its `WITH`, copied once (not once per `WITH`).
        let rest = without_with(q);
        if let Some(w) = &q.with {
            if w.recursive {
                return Err(err("WITH RECURSIVE is not supported: REACHABLE and PATH traverse links"));
            }
            for (i, cte) in w.cte_tables.iter().enumerate() {
                // Rerank phases over the `WITH` results from this one on,
                // which may read those before (a user's history).
                if i > 0 {
                    if let Some(p) = self.phased_in(&w.cte_tables[i..], &rest)? {
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
        let body = self.body(&rest)?;
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
        match q.with.as_ref().filter(|w| !w.recursive) {
            Some(w) => self.phased_in(&w.cte_tables, &without_with(q)),
            None => Ok(None),
        }
    }

    /// [`Planner::phased`] of the query `body` (with no `WITH` of its own)
    /// after the `WITH` results `ctes`.
    fn phased_in(&self, ctes: &[sql::Cte], body: &sql::Query) -> Result<Option<Planned>> {
        if ctes.is_empty() || ctes.iter().any(|c| !c.alias.columns.is_empty()) {
            return Ok(None);
        }
        // The first `WITH`, its table and alias.
        let first = &ctes[0].query;
        let Some((s0, name)) = one_table(first) else { return Ok(None) };
        let Some(schema) = self.table(&name).filter(|_| self.cte(&name).is_none()) else { return Ok(None) };
        let simple = |s: &sql::Select| s.distinct.is_none() && s.having.is_none() && matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty());
        if first.with.is_some() || !simple(s0) || windowed(first, s0) {
            return Ok(None);
        }
        // Each later step: what it projects, ranks by and cuts, its
        // columns named without the `WITH` it reads.
        let mut steps: Vec<(Vec<SelectItem>, sql::OrderByExpr, Option<sql::LimitClause>)> = Vec::new();
        let reads: Vec<&sql::Query> = ctes[1..].iter().map(|c| c.query.as_ref()).chain(std::iter::once(body)).collect();
        for (i, r) in reads.iter().enumerate() {
            let read = &ctes[i].alias.name.value;
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
            if !from.eq_ignore_ascii_case(read) || r.with.is_some() || !simple(s) || windowed(r, s) || s.selection.is_some() || subquery || !ranks {
                return Ok(None);
            }
            let plain = |e: &Expr| {
                let e = unqualify(e, read);
                alias.as_deref().map_or(e.clone(), |a| unqualify(&e, a))
            };
            // `*` (or `w.*`) is the columns of the step read, when each is a
            // column of the table (not a score, which the step's ranking gave).
            let before: &[SelectItem] = steps.last().map_or(&s0.projection, |(p, ..)| p);
            let mut projection = Vec::new();
            for item in &s.projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        let Some(columns) = columns_of_step(before) else { return Ok(None) };
                        projection.extend(columns);
                    }
                    SelectItem::QualifiedWildcard(sql::SelectItemQualifiedWildcardKind::ObjectName(n), _)
                        if [Some(read.as_str()), alias.as_deref()].into_iter().flatten().any(|a| a.eq_ignore_ascii_case(&super::sql::table_name(n))) =>
                    {
                        let Some(columns) = columns_of_step(before) else { return Ok(None) };
                        projection.extend(columns);
                    }
                    SelectItem::UnnamedExpr(e) => projection.push(SelectItem::UnnamedExpr(plain(e))),
                    SelectItem::ExprWithAlias { expr, alias } => projection.push(SelectItem::ExprWithAlias { expr: plain(expr), alias: alias.clone() }),
                    other => projection.push(other.clone()),
                }
            }
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
            names.push(ctes[i].alias.name.value.to_ascii_lowercase());
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
                    Projection::Score | Projection::Hops(_) => schema.field(name).is_none(),
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

    /// "no such table: x", with the tables of the database and `WITH`.
    fn no_table(&self, name: &str) -> String {
        let ctes: Vec<String> = self.ctes.borrow().iter().map(|c| c.name.clone()).collect();
        hint::no_table(name, self.catalog.iter().map(|t| t.table.as_str()).chain(ctes.iter().map(String::as_str)))
    }

    /// Why a SELECT with no table, or with several after FROM, is refused,
    /// and what to write instead.
    fn from_hint(&self, s: &sql::Select) -> String {
        let tables: Vec<&str> = self.catalog.iter().map(|t| t.table.as_str()).collect();
        let first = tables.first().copied().unwrap_or("t");
        if s.from.is_empty() {
            return format!(
                "a SELECT reads a table: add FROM and one of the database's tables ({}). A value of no table is a column of any of them: SELECT {} FROM {first} LIMIT 1",
                hint::listing(&tables, 10),
                s.projection.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
            );
        }
        // FROM a, b: the join's ON from an equality of the two in WHERE.
        let name = |t: &sql::TableWithJoins| match &t.relation {
            TableFactor::Table { name, alias, .. } => {
                let n = super::sql::table_name(name);
                let a = alias.as_ref().map_or_else(|| n.clone(), |a| a.name.value.clone());
                (if a.eq_ignore_ascii_case(&n) { n } else { format!("{n} {a}") }, a)
            }
            t => (t.to_string(), "t".to_string()),
        };
        let ((t0, a0), (t1, a1)) = (name(&s.from[0]), name(&s.from[1]));
        let on = s
            .selection
            .iter()
            .flat_map(split_and)
            .find(|c| match c {
                Expr::BinaryOp { left, op: BinaryOperator::Eq, right } => {
                    let side = |e: &Expr| match e {
                        Expr::CompoundIdentifier(ids) if ids.len() == 2 => Some(ids[0].value.to_ascii_lowercase()),
                        _ => None,
                    };
                    let (l, r) = (side(left), side(right));
                    let (x, y) = (a0.to_ascii_lowercase(), a1.to_ascii_lowercase());
                    l.is_some() && r.is_some() && l != r && [l.clone(), r.clone()].iter().all(|v| v.as_deref() == Some(x.as_str()) || v.as_deref() == Some(y.as_str()))
                }
                _ => false,
            })
            .map_or_else(|| format!("{a0}.column = {a1}.column"), |c| c.to_string());
        format!("FROM {} lists {} tables: join them with JOIN ... ON an equality of their columns, e.g. FROM {t0} JOIN {t1} ON {on}", s.from.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", "), s.from.len())
    }

    fn body(&self, q: &sql::Query) -> Result<Planned> {
        let s = match q.body.as_ref() {
            sql::SetExpr::Select(s) => s,
            sql::SetExpr::SetOperation { .. } | sql::SetExpr::Query(_) => return self.set_query(q),
            sql::SetExpr::Values(_) => return Err(err("VALUES lists an INSERT's rows; a query selects them FROM a table")),
            b => return Err(err(format!("this form of query is not supported: {b}"))),
        };
        // SELECT 1, SELECT now(): values of no table, one row of them, as
        // the one row a count of a table gives.
        if s.from.is_empty() && s.selection.is_none() && s.group_by == sql::GroupByExpr::Expressions(vec![], vec![]) && s.having.is_none() {
            if let Some(first) = self.catalog.first() {
                let one = format!("SELECT COUNT(*) FROM {}", sql::Ident::with_quote('"', first.table.as_str()));
                let Some(sql::Statement::Query(one)) = super::parse::parse(&one)?.into_iter().next() else { return Err(err("a query of one row")) };
                let mut q = q.clone();
                if let sql::SetExpr::Select(s) = q.body.as_mut() {
                    s.from = vec![sql::TableWithJoins {
                        relation: TableFactor::Derived { lateral: false, subquery: one, alias: Some(sql::TableAlias { name: sql::Ident::new("one"), columns: Vec::new() }) },
                        joins: Vec::new(),
                    }];
                }
                return self.body(&q);
            }
        }
        if s.from.len() != 1 {
            return Err(err(self.from_hint(s)));
        }
        if let Some((q, in_memory)) = self.with_columns(q, s) {
            // The `WITH` results read in memory, in scope as results of no
            // table while the query plans.
            let mark = self.ctes.borrow().len();
            self.ctes.borrow_mut().extend(in_memory.into_iter().map(|c| Cte { keyed: None, order: None, ..c }));
            let r = self.body(&q);
            self.ctes.borrow_mut().truncate(mark);
            return r;
        }
        if !s.from[0].joins.is_empty() {
            return self.join(q, s);
        }
        match &s.from[0].relation {
            TableFactor::Table { name, .. } if self.cte(&super::sql::table_name(name)).is_some() => {
                let cte = self.cte(&super::sql::table_name(name)).expect("in scope");
                match cte.keyed.as_deref().and_then(|t| self.table(t)) {
                    Some(schema) if !rows_of(q, s) && !windowed(q, s) => self.over_rows(q, s, &cte, schema),
                    _ => self.join(q, s),
                }
            }
            TableFactor::Table { name, .. } => match self.table(&super::sql::table_name(name)) {
                Some(schema) => self.single(q, s, schema),
                None => Err(err(self.no_table(&super::sql::table_name(name)))),
            },
            TableFactor::Derived { .. } => self.join(q, s),
            t => Err(err(format!("unsupported FROM: {t}"))),
        }
    }

    /// A query that takes queries' rows together (`UNION`, `INTERSECT`,
    /// `EXCEPT`, [`ir::Node::SetOp`]), or a query in parentheses. Its
    /// `ORDER BY` and `LIMIT` read its rows as a derived table's:
    /// `SELECT * FROM (..) ORDER BY .. LIMIT ..`, so they name its columns,
    /// which are its first query's.
    fn set_query(&self, q: &sql::Query) -> Result<Planned> {
        let sql::Query { with: _, body, order_by, limit_clause, fetch, locks, for_clause, settings, format_clause, pipe_operators } = q;
        if let Some(c) = super::sql::unsupported_query_clause(fetch, locks, for_clause, settings, format_clause, pipe_operators) {
            return Err(err(format!("{c} is not supported")));
        }
        if order_by.is_some() || limit_clause.is_some() {
            let mut outer = super::sql::parse_query("SELECT * FROM (SELECT 1) AS __set")?;
            let sql::SetExpr::Select(s) = outer.body.as_mut() else { unreachable!("a SELECT") };
            let TableFactor::Derived { subquery, .. } = &mut s.from[0].relation else { unreachable!("a derived table") };
            *subquery = Box::new(sql::Query { with: None, order_by: None, limit_clause: None, ..q.clone() });
            outer.order_by = order_by.clone();
            outer.limit_clause = limit_clause.clone();
            return self.body(&outer);
        }
        match body.as_ref() {
            sql::SetExpr::Query(_) => self.side(body),
            sql::SetExpr::SetOperation { .. } => {
                // `a UNION b UNION c ...` parses left-deep: its queries are
                // planned from the first on, along the chain rather than
                // down it, so that a long chain neither recurses once per
                // query nor copies the queries before each.
                let mut chain = Vec::new();
                let mut first = body.as_ref();
                while let sql::SetExpr::SetOperation { op, set_quantifier, left, right } = first {
                    chain.push((op, set_quantifier, right.as_ref()));
                    first = left;
                }
                let mut l = self.side(first)?;
                for (op, set_quantifier, right) in chain.into_iter().rev() {
                    let all = match set_quantifier {
                        sql::SetQuantifier::All => true,
                        sql::SetQuantifier::Distinct | sql::SetQuantifier::None => false,
                        by_name => return Err(err(format!("{op} {by_name} is not supported: select the columns in the same order on both sides"))),
                    };
                    let kind = match op {
                        sql::SetOperator::Union => ir::SetOp::Union,
                        sql::SetOperator::Intersect => ir::SetOp::Intersect,
                        sql::SetOperator::Except | sql::SetOperator::Minus => ir::SetOp::Except,
                    };
                    let r = self.side(right)?;
                    if l.columns.len() != r.columns.len() {
                        return Err(err(format!(
                            "{}: the query before it selects {} column{} and the one after {}; both sides of a {} select as many",
                            kind.name(),
                            l.columns.len(),
                            if l.columns.len() == 1 { "" } else { "s" },
                            r.columns.len(),
                            kind.name()
                        )));
                    }
                    let multi = l.multi.iter().zip(&r.multi).map(|(a, b)| *a || *b).collect();
                    l = Planned { node: ir::Node::SetOp { op: kind, all, left: Box::new(l.node), right: Box::new(r.node) }, columns: l.columns, keyed: None, multi };
                }
                Ok(l)
            }
            _ => unreachable!("a set operation or a query"),
        }
    }

    /// A side of a set operation, or a query in parentheses, as a query
    /// of its own: [`plan_query`] rewrote the paths and windows of the
    /// queries it reached, which are not these.
    fn side(&self, b: &sql::SetExpr) -> Result<Planned> {
        let mut q = match b {
            sql::SetExpr::Query(q) => q.clone(),
            b => {
                let mut q = super::sql::parse_query("SELECT 1")?;
                q.body = Box::new(b.clone());
                q
            }
        };
        json_paths(&mut q, self.catalog)?;
        inline_windows(&mut q)?;
        self.query(&q)
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
            // text or by values of rows, by the row evaluator.
            // Window functions, by operators over the rows.
            // Conditions the indexes cannot answer, and orders by several
            // keys, by the row evaluator over the scan's rows.
            _ if computes_columns(&q, &s)
                || orders_by_text(&q, &s, schema)
                || orders_by_row_values(&q, &s)
                || reads_paths(&q, &s)
                || windowed(&q, &s)
                || filters_rows(&s, schema)
                || orders_by_several(&q, &s, schema)
                || aggregates_rows(&q, &s, schema) =>
            {
                return self.join(&q, &s)
            }
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
        // A score or a hop count named as a column of the table would read
        // as that column over the rows.
        let keyed = !aggregated
            && select.projection.iter().all(|(name, p)| match p {
                Projection::Field(f) => schema.fields[*f].name.eq_ignore_ascii_case(name),
                Projection::Score | Projection::Hops(_) => schema.field(name).is_none(),
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

    /// A SELECT of the columns of `WITH` results that hold a table's rows,
    /// which read as the table restricted to those rows
    /// ([`Planner::over_rows`]): `*` (or `w.*`) is the result's columns, not
    /// the table's, and a result whose columns are not all the table's
    /// (`score() AS s`, `hops()`) is read in memory when the query reads
    /// them, by `*` or by name. The query with each `*` written out, and the
    /// results to read in memory; `None` when neither changes it.
    fn with_columns(&self, q: &sql::Query, s: &sql::Select) -> Option<(sql::Query, Vec<Cte>)> {
        let twj = &s.from[0];
        let relations: Vec<&TableFactor> = std::iter::once(&twj.relation).chain(twj.joins.iter().map(|j| &j.relation)).collect();
        // The FROM's `WITH` results of a table's rows: their alias (in
        // lowercase), the result, and whether its columns are the table's.
        let mut read: Vec<(String, Cte, bool)> = Vec::new();
        for r in &relations {
            let TableFactor::Table { name, alias, .. } = r else { continue };
            let n = super::sql::table_name(name);
            let Some(cte) = self.cte(&n) else { continue };
            let Some(schema) = cte.keyed.as_deref().and_then(|t| self.table(t)) else { continue };
            let fields = cte.columns.iter().all(|c| schema.field(c).is_some());
            read.push((alias.as_ref().map_or(n, |a| a.name.value.clone()).to_ascii_lowercase(), cte, fields));
        }
        if read.is_empty() {
            return None;
        }
        let star = |a: &str| {
            s.projection.iter().any(|i| match i {
                SelectItem::Wildcard(_) => true,
                SelectItem::QualifiedWildcard(sql::SelectItemQualifiedWildcardKind::ObjectName(n), _) => super::sql::table_name(n).eq_ignore_ascii_case(a),
                _ => false,
            })
        };
        // The columns the query names, with the alias that qualifies them;
        // in ORDER BY, not the names it gives its own columns.
        let mut named: Vec<(Option<String>, String)> = Vec::new();
        let mut name = |x: &Expr, aliases: &[&str]| match x {
            Expr::Identifier(id) if !aliases.iter().any(|n| n.eq_ignore_ascii_case(&id.value)) => named.push((None, id.value.clone())),
            Expr::CompoundIdentifier(ids) if ids.len() == 2 => named.push((Some(ids[0].value.to_ascii_lowercase()), ids[1].value.clone())),
            _ => {}
        };
        for e in select_exprs(s) {
            walk(e, &mut |x| name(x, &[]));
        }
        let given: Vec<&str> = s.projection.iter().filter_map(|i| if let SelectItem::ExprWithAlias { alias, .. } = i { Some(alias.value.as_str()) } else { None }).collect();
        if let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &q.order_by {
            for o in es {
                walk(&o.expr, &mut |x| name(x, &given));
            }
        }
        let mut in_memory: Vec<Cte> = Vec::new();
        for (a, cte, fields) in &read {
            let schema = cte.keyed.as_deref().and_then(|t| self.table(t)).expect("a table's rows");
            let own = |c: &str| schema.field(c).is_none() && cte.columns.iter().any(|x| x.eq_ignore_ascii_case(c));
            let reads_own = (!fields && star(a)) || named.iter().any(|(at, c)| at.as_ref().is_none_or(|x| x == a) && own(c));
            if reads_own && !in_memory.iter().any(|c| c.name.eq_ignore_ascii_case(&cte.name)) {
                in_memory.push(cte.clone());
            }
        }
        // The results read as the table's rows, whose `*` is written out.
        let as_rows = |a: &str| read.iter().find(|(x, cte, _)| x == a && !in_memory.iter().any(|c| c.name.eq_ignore_ascii_case(&cte.name))).map(|(_, cte, _)| cte);
        let joined = relations.len() > 1;
        // Each FROM item's alias, which `*` over a join writes out.
        let aliases: Option<Vec<String>> = relations
            .iter()
            .map(|r| match r {
                TableFactor::Table { name, alias, .. } => Some(alias.as_ref().map_or_else(|| super::sql::table_name(name), |a| a.name.value.clone())),
                TableFactor::Derived { alias: Some(a), .. } => Some(a.name.value.clone()),
                _ => None,
            })
            .collect();
        let columns = |a: &str, cte: &Cte| -> Vec<SelectItem> {
            cte.columns
                .iter()
                .map(|c| match joined {
                    // Named as `*` over a join names a table's columns.
                    true => SelectItem::ExprWithAlias { expr: qualified(a, c), alias: sql::Ident::with_quote('"', format!("{a}.{c}")) },
                    false => SelectItem::UnnamedExpr(Expr::Identifier(sql::Ident::new(c.clone()))),
                })
                .collect()
        };
        let mut projection = Vec::new();
        let mut written = false;
        for item in &s.projection {
            match item {
                SelectItem::Wildcard(options) if read.iter().any(|(a, ..)| as_rows(a).is_some()) => match (&aliases, joined) {
                    (_, false) => {
                        projection.extend(columns(&read[0].0, as_rows(&read[0].0).expect("read as rows")));
                        written = true;
                    }
                    (Some(aliases), true) => {
                        for alias in aliases {
                            let a = alias.to_ascii_lowercase();
                            match as_rows(&a) {
                                Some(cte) => projection.extend(columns(&a, cte)),
                                None => projection.push(SelectItem::QualifiedWildcard(sql::SelectItemQualifiedWildcardKind::ObjectName(sql::ObjectName::from(vec![sql::Ident::new(alias.clone())])), options.clone())),
                            }
                        }
                        written = true;
                    }
                    // A FROM item without an alias, which planning refuses.
                    (None, true) => projection.push(item.clone()),
                },
                SelectItem::QualifiedWildcard(sql::SelectItemQualifiedWildcardKind::ObjectName(n), _) if as_rows(&super::sql::table_name(n).to_ascii_lowercase()).is_some() => {
                    let a = super::sql::table_name(n).to_ascii_lowercase();
                    projection.extend(columns(&a, as_rows(&a).expect("read as rows")));
                    written = true;
                }
                i => projection.push(i.clone()),
            }
        }
        if !written && in_memory.is_empty() {
            return None;
        }
        let mut q = q.clone();
        if let sql::SetExpr::Select(s) = q.body.as_mut() {
            s.projection = projection;
        }
        Some((q, in_memory))
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
                let schema = self.table(&n).ok_or_else(|| err(self.no_table(&n)))?;
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
        // Windows are computed over the rows WHERE, GROUP BY and HAVING
        // give, so they are not part of those.
        let mut before: Vec<&Expr> = s.selection.iter().chain(s.having.iter()).collect();
        if let sql::GroupByExpr::Expressions(es, _) = &s.group_by {
            before.extend(es.iter());
        }
        for j in s.from.iter().flat_map(|t| &t.joins) {
            if let sql::JoinOperator::Join(sql::JoinConstraint::On(on)) | sql::JoinOperator::Inner(sql::JoinConstraint::On(on)) | sql::JoinOperator::Left(sql::JoinConstraint::On(on)) | sql::JoinOperator::LeftOuter(sql::JoinConstraint::On(on)) | sql::JoinOperator::FullOuter(sql::JoinConstraint::On(on)) = &j.join_operator {
                before.push(on);
            }
            if let sql::JoinOperator::AsOf { match_condition, constraint } = &j.join_operator {
                before.push(match_condition);
                if let sql::JoinConstraint::On(on) = constraint {
                    before.push(on);
                }
            }
        }
        for e in before {
            let mut call = None;
            walk(e, &mut |x| {
                if matches!(x, Expr::Function(f) if f.over.is_some()) && call.is_none() {
                    call = Some(x.to_string());
                }
            });
            if let Some(c) = call {
                return Err(err(format!("{c}: a window function is a column of SELECT, ORDER BY or QUALIFY, not part of WHERE, ON, GROUP BY or HAVING")));
            }
        }
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
                && !select_exprs(s).into_iter().any(has_window)
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
        // The match condition of each ASOF JOIN, by its table's place.
        let mut matches: Vec<Option<Expr>> = vec![None];
        for j in &twj.joins {
            let (kind, c) = match &j.join_operator {
                sql::JoinOperator::Join(c) | sql::JoinOperator::Inner(c) => (ir::JoinKind::Inner, c),
                sql::JoinOperator::Left(c) | sql::JoinOperator::LeftOuter(c) => (ir::JoinKind::Left, c),
                sql::JoinOperator::FullOuter(c) => (ir::JoinKind::Full, c),
                sql::JoinOperator::AsOf { constraint, .. } => (ir::JoinKind::AsOf, constraint),
                o => return Err(err(unsupported_join(o)))?,
            };
            let sql::JoinConstraint::On(on) = c else {
                return Err(err(if kind == ir::JoinKind::AsOf {
                    "ASOF JOIN needs ON, e.g. ASOF JOIN feats f MATCH_CONDITION (s.ts >= f.ts) ON s.usr = f.usr"
                } else {
                    "JOIN needs ON, e.g. JOIN papers b ON a.cites = b.id"
                }));
            };
            let on = match &j.join_operator {
                sql::JoinOperator::AsOf { match_condition, .. } => {
                    let m = match match_condition {
                        Expr::Nested(e) => (**e).clone(),
                        e => e.clone(),
                    };
                    // `f.ts <= s.ts` is `s.ts >= f.ts`.
                    let m = match m {
                        Expr::BinaryOp { left, op: BinaryOperator::LtEq, right } => Expr::BinaryOp { left: right, op: BinaryOperator::GtEq, right: left },
                        Expr::BinaryOp { left, op: BinaryOperator::Lt, right } => Expr::BinaryOp { left: right, op: BinaryOperator::Gt, right: left },
                        m => m,
                    };
                    matches.push(Some(m.clone()));
                    Expr::BinaryOp { left: Box::new(on.clone()), op: BinaryOperator::And, right: Box::new(m) }
                }
                _ => {
                    matches.push(None);
                    on.clone()
                }
            };
            let (alias, src) = self.source(&j.relation)?;
            srcs.push(Src { alias, kind: src, join: Some((kind, on)) });
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

        // A full join keeps the rows of both sides that match none, so no
        // condition of WHERE or ON filters the scans it joins.
        let full_at: Option<usize> = srcs.iter().rposition(|s| s.join.as_ref().is_some_and(|(k, _)| *k == ir::JoinKind::Full));
        // WHERE: each condition naming one scanned side goes to its scan.
        let mut pushed: Vec<Vec<Expr>> = vec![Vec::new(); srcs.len()];
        let mut residual: Vec<Expr> = Vec::new();
        for c in s.selection.iter().flat_map(split_and) {
            let at = sides.of(&c)?;
            let left_join_right = |i: usize| srcs[i].join.as_ref().is_some_and(|(k, _)| *k != ir::JoinKind::Inner) || full_at.is_some_and(|f| i <= f);
            match at.as_slice() {
                [i] if !in_memory(*i) && !left_join_right(*i) && !of_rows(&c) && !table(*i).is_some_and(|t| of_rows_filter(&c, t)) => pushed[*i].push(c),
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
                let full = srcs[i].join.as_ref().is_some_and(|(k, _)| *k == ir::JoinKind::Full);
                if at == [i] && !full && !in_memory(i) && !table(i).is_some_and(|t| of_rows_filter(&c, t)) {
                    pushed[i].push(c);
                } else if at.iter().any(|&x| x > i) {
                    return Err(err(format!("ON of {} names a table joined later", srcs[i].alias)));
                } else {
                    on_rest[i].push(c);
                }
            }
            if let Some(m) = &matches[i] {
                // The match condition compares a left expression with a
                // right one, and is the only condition of both sides.
                let ok = match m {
                    Expr::BinaryOp { left, op: BinaryOperator::GtEq | BinaryOperator::Gt, right } => {
                        let (l, r) = (sides.of(left)?, sides.of(right)?);
                        l.iter().all(|&x| x < i) && !l.is_empty() && r == [i]
                    }
                    _ => false,
                };
                if !ok {
                    return Err(err(format!(
                        "MATCH_CONDITION ({m}) of ASOF JOIN {a}: a time of the tables before, at or after a time of {a} (>= or >), e.g. s.ts >= {a}.ts, so each row takes the latest {a} row at or before it",
                        a = srcs[i].alias
                    )));
                }
                if on_rest[i].len() != 1 || &on_rest[i][0] != m {
                    return Err(err(format!("ASOF JOIN {}: ON holds equalities of the two sides and conditions of {} alone; the time goes in MATCH_CONDITION", srcs[i].alias, srcs[i].alias)));
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
        // A window reads all the rows, so none drive (and its order is
        // its own).
        let driver_order = !grouped
            && full_at.is_none()
            && !windowed(q, s)
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
        let mut p = self.evaluate(s, node, &cols, &multi, &mem_order, offset, limit, srcs.len() > 1, driver_order, per)?;
        group_first(&mut p.node, self.catalog);
        Ok(p)
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

/// Groups a join's left rows in their own scan, before the join, where the
/// groups are the right table's rows: `GROUP BY` the right table's primary
/// key (and other columns of its rows), aggregating the left table's
/// fields, as in `SELECT p.id, p.title, COUNT(*) FROM reviews r JOIN
/// products p ON r.product_id = p.id GROUP BY p.id, p.title`. A group of
/// the join is one right row and the left rows whose key is its key,
/// which is what the left table's group of that key aggregates; so the
/// executor groups the left table by its join key (as `GROUP BY
/// product_id` alone, from the key's column), the join reads a row per
/// group rather than per row, and each group of the join, now of one row,
/// takes its aggregates' values from it. The groups then come in the
/// order of the left's groups rather than of their first rows, so the
/// plan changes only when the sort above them is by the right table's
/// key, which no two groups share: the answer is the same, row for row.
fn group_first(node: &mut ir::Node, catalog: &[&Schema]) {
    for n in node.inputs_mut() {
        group_first(n, catalog);
    }
    let ir::Node::Sort { input, keys: sort } = node else { return };
    let at = match input.as_mut() {
        ir::Node::Filter { input, .. } => input.as_mut(),
        n => n,
    };
    if let Some(n) = grouped_first(at, sort, catalog) {
        *at = n;
    }
}

/// [`group_first`] of `n`, an Aggregate under a sort by `sort`: the
/// Aggregate over the join of the left table's groups, or `None` where
/// it does not apply.
fn grouped_first(n: &ir::Node, sort: &[ir::SortKey], catalog: &[&Schema]) -> Option<ir::Node> {
    let ir::Node::Aggregate { input, keys, aggs } = n else { return None };
    let ir::Node::Join { left, right, kind: ir::JoinKind::Inner, keys: on, on: None } = input.as_ref() else { return None };
    let [ir::JoinKey { left: ir::Expr::Output(li), right: ir::Expr::Output(rj), left_links: false, right_links: false }] = on.as_slice() else { return None };
    let (li, rj) = (*li as usize, *rj as usize);
    let table = |name: &str| catalog.iter().copied().find(|t| t.table.eq_ignore_ascii_case(name));
    // The left: a scan of a table, in no order of its own.
    let ir::Node::Project { input: scan, columns: lcols } = left.as_ref() else { return None };
    let ir::Node::Search(search) = scan.as_ref() else { return None };
    if !search.order.is_empty() || !search.phases.is_empty() || search.per.is_some() {
        return None;
    }
    let lt = table(&search.table)?;
    // The right: rows of a table, each once, its key among their columns.
    let ir::Node::Project { input: below, columns: rcols } = right.as_ref() else { return None };
    let mut x = below.as_ref();
    let rt = loop {
        match x {
            ir::Node::Limit { input, .. } | ir::Node::Filter { input, .. } => x = input,
            ir::Node::Search(s) => break table(&s.table)?,
            _ => return None,
        }
    };
    let pk = rt.primary_key?;
    if rcols.get(rj)?.expr != ir::Expr::Field(pk as u32) {
        return None;
    }
    // One value a left row, of the key's own type, so that it equals the
    // right's key exactly, and values that a group of the table reads.
    let ir::Expr::Field(lf) = lcols.get(li)?.expr else { return None };
    let plain = |f: &crate::schema::Field| f.indexed && !f.multi && f.logical.is_none() && f.media.is_none();
    let (kf, pf) = (lt.fields.get(lf as usize)?, rt.fields.get(pk)?);
    if !plain(kf) || pf.logical.is_some() || kf.ty != pf.ty || !matches!(kf.ty, FieldType::Keyword | FieldType::Int) {
        return None;
    }
    // The groups: of values of the right's rows (or the left's key, which
    // equals the right's), one of them the right's key, which the sort
    // above orders by.
    let (lw, rw) = (lcols.len(), rcols.len());
    let mut by_key = Vec::new();
    for (g, k) in keys.iter().enumerate() {
        if matches!(k, ir::Expr::Output(i) if *i as usize == lw + rj || *i as usize == li) {
            by_key.push(g as u32);
        }
        let mut ok = !k.has_operators();
        k.visit(&mut |e| {
            ok &= match e {
                ir::Expr::Output(i) => (lw..lw + rw).contains(&(*i as usize)) || *i as usize == li,
                ir::Expr::Lit(_)
                | ir::Expr::Not(_)
                | ir::Expr::And(_)
                | ir::Expr::Or(_)
                | ir::Expr::IsNull(_)
                | ir::Expr::Cmp { .. }
                | ir::Expr::Between { .. }
                | ir::Expr::In { .. }
                | ir::Expr::Arith { .. }
                | ir::Expr::Neg(_)
                | ir::Expr::Concat(_)
                | ir::Expr::Like { .. }
                | ir::Expr::Prefix { .. }
                | ir::Expr::Case { .. }
                | ir::Expr::Call { .. } => true,
                _ => false,
            }
        });
        if !ok {
            return None;
        }
    }
    if !sort.iter().any(|k| matches!(k.expr, ir::Expr::Output(g) if by_key.contains(&g))) {
        return None;
    }
    // The aggregates: of rows, or of single-valued fields of the left,
    // each once (`COUNT(*)` is both asked for and counts the group's rows).
    let mut first: Vec<ir::Agg> = Vec::with_capacity(aggs.len());
    let mut at = Vec::with_capacity(aggs.len());
    for a in aggs {
        let arg = match &a.arg {
            None if a.func == AggFn::Count => None,
            Some(ir::Expr::Output(i)) if (*i as usize) < lw => {
                let ir::Expr::Field(f) = lcols[*i as usize].expr else { return None };
                let fl = lt.fields.get(f as usize)?;
                let typed = match a.func {
                    AggFn::Count | AggFn::CountDistinct => matches!(fl.ty, FieldType::Keyword | FieldType::Int | FieldType::Float),
                    AggFn::Sum | AggFn::Avg | AggFn::Min | AggFn::Max | AggFn::PercentileCont(_) | AggFn::PercentileDisc(_) | AggFn::ApproxPercentile(_) => matches!(fl.ty, FieldType::Int | FieldType::Float),
                };
                if !plain(fl) || !typed {
                    return None;
                }
                Some(ir::Expr::Field(f))
            }
            _ => return None,
        };
        let agg = ir::Agg { func: a.func, arg };
        at.push(1 + first.iter().position(|x| *x == agg).unwrap_or_else(|| {
            first.push(agg);
            first.len() - 1
        }));
    }
    // The left table's groups: its key, then the aggregates.
    let width = 1 + first.len();
    let mut columns = vec![ir::Column { name: lcols[li].name.clone(), expr: ir::Expr::Output(0) }];
    columns.extend((1..width).map(|j| ir::Column { name: format!("#{j}"), expr: ir::Expr::Output(j as u32) }));
    let groups = ir::Node::Project {
        input: Box::new(ir::Node::Aggregate { input: Box::new(ir::Node::Search(search.clone())), keys: vec![ir::Expr::Field(lf)], aggs: first }),
        columns,
    };
    ir::Plan::new(groups.clone()).to_select(lt).ok()?;
    let join = ir::Node::Join {
        left: Box::new(groups),
        right: right.clone(),
        kind: ir::JoinKind::Inner,
        keys: vec![ir::JoinKey { left: ir::Expr::Output(0), right: ir::Expr::Output(rj as u32), left_links: false, right_links: false }],
        on: None,
    };
    // The same groups over the join's rows, one row each: its aggregates'
    // values are the row's.
    let keys = keys
        .iter()
        .map(|k| {
            let mut k = k.clone();
            k.visit_mut(&mut |e| {
                if let ir::Expr::Output(i) = e {
                    *i = if *i as usize == li { 0 } else { *i - lw as u32 + width as u32 };
                }
                Ok(())
            })
            .ok()?;
            Some(k)
        })
        .collect::<Option<Vec<_>>>()?;
    let aggs = at.into_iter().map(|j| ir::Agg { func: AggFn::Min, arg: Some(ir::Expr::Output(j as u32)) }).collect();
    Some(ir::Node::Aggregate { input: Box::new(join), keys, aggs })
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
        s.qualify.iter_mut().for_each(|e| walk_mut(e, f));
        if let sql::GroupByExpr::Expressions(es, _) = &mut s.group_by {
            es.iter_mut().for_each(|e| walk_mut(e, f));
        }
    }
    if let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &mut q.order_by {
        es.iter_mut().for_each(|o| walk_mut(&mut o.expr, f));
    }
}

/// [`walk`], changing the expressions, and into subqueries.
pub(crate) fn walk_mut(e: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
    f(e);
    match e {
        Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } | Expr::IsNull(x) | Expr::IsNotNull(x) | Expr::Cast { expr: x, .. } | Expr::Extract { expr: x, .. } => walk_mut(x, f),
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
            if let Some(sql::WindowType::WindowSpec(w)) = &mut func.over {
                w.partition_by.iter_mut().for_each(|x| walk_mut(x, f));
                w.order_by.iter_mut().for_each(|o| walk_mut(&mut o.expr, f));
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

/// Why a join other than `JOIN`, `LEFT JOIN` and `FULL JOIN` is refused, and what to
/// write instead.
fn unsupported_join(o: &sql::JoinOperator) -> String {
    use sql::JoinOperator as J;
    match o {
        J::Right(_) | J::RightOuter(_) => "RIGHT JOIN is not supported: swap the two tables and use LEFT JOIN (a RIGHT JOIN b ON ... is b LEFT JOIN a ON ...)".into(),
        J::CrossJoin(_) => "CROSS JOIN is not supported: join with JOIN ... ON an equality of the two tables' columns".into(),
        J::Semi(_) | J::LeftSemi(_) | J::RightSemi(_) => "SEMI JOIN is not supported: WHERE x IN (SELECT ...) or EXISTS (SELECT ...) keeps the rows that have a match".into(),
        J::Anti(_) | J::LeftAnti(_) | J::RightAnti(_) => "ANTI JOIN is not supported: WHERE x NOT IN (SELECT ...) or NOT EXISTS (SELECT ...) keeps the rows without a match".into(),
        J::CrossApply | J::OuterApply => "APPLY is not supported: join with JOIN ... ON".into(),
        _ => "this kind of join is not supported: use JOIN (inner) or LEFT JOIN, with ON an equality of the two tables' columns".into(),
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

/// Whether `q` ranks rows: one SELECT of one table, without groups,
/// aggregates, `DISTINCT` or windows, whose first ORDER BY key (an alias
/// or a position read as the column it names) is a column or calls a
/// ranking function. Such a query's top rows have values they were ranked
/// by ([`super::sql::with_features`]); any other ORDER BY sorts rows (or
/// groups) by a value the row evaluator computes, and has none.
pub(crate) fn ranks_rows(q: &sql::Query) -> bool {
    let sql::SetExpr::Select(s) = q.body.as_ref() else { return false };
    let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &q.order_by else { return false };
    let Some(first) = es.first() else { return false };
    let grouped = !matches!(&s.group_by, sql::GroupByExpr::Expressions(g, _) if g.is_empty());
    if s.from.len() != 1 || !s.from[0].joins.is_empty() || !matches!(s.from[0].relation, TableFactor::Table { .. }) || grouped || s.having.is_some() || s.distinct.is_some() || has_aggregate(s) {
        return false;
    }
    let items: Vec<&Expr> = s
        .projection
        .iter()
        .filter_map(|i| match i {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
            _ => None,
        })
        .collect();
    if items.iter().any(|e| has_window(e)) {
        return false;
    }
    // The key, through an alias or a position.
    let key = match &first.expr {
        Expr::Identifier(id) => s
            .projection
            .iter()
            .find_map(|i| match i {
                SelectItem::ExprWithAlias { expr, alias } if alias.value.eq_ignore_ascii_case(&id.value) => Some(expr),
                _ => None,
            })
            .unwrap_or(&first.expr),
        Expr::Value(v) => match v.value.to_string().parse::<usize>() {
            Ok(n) if (1..=s.projection.len()).contains(&n) => match &s.projection[n - 1] {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                _ => return false,
            },
            _ => return false,
        },
        e => e,
    };
    matches!(key, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) || ranks(key)
}

fn has_aggregate(s: &sql::Select) -> bool {
    let mut found = false;
    for item in &s.projection {
        if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
            walk(e, &mut |x| found |= matches!(x, Expr::Function(f) if is_aggregate(f)));
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

/// The value of `e`, an expression of no row (a literal or a
/// parameter).
pub(crate) fn value_of(e: &Expr, params: &Params) -> Result<Value> {
    value(&compile(e, params, &|_| None, &[])?)
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
        // A time moved by an interval: computed once when the time is a
        // value (`now() - INTERVAL '1 hour'`), and over the rows when it is
        // a column (`at + INTERVAL '2 days'`, in microseconds, which a
        // month has no fixed number of).
        e if super::sql::interval_shift(e).is_some() => {
            let (t, iv) = super::sql::interval_shift(e).expect("an interval")?;
            match go(t)? {
                ir::Expr::Lit(Value::Null) => ir::Expr::Lit(Value::Null),
                ir::Expr::Lit(v) => {
                    let base = super::sql::time_of(&v, &|| e.to_string())?;
                    ir::Expr::Lit(Value::Timestamp(iv.add_to(base).ok_or_else(|| err(format!("{e} is outside the times a TIMESTAMP holds")))?))
                }
                _ if iv.months != 0 => {
                    return Err(err(format!("{e}: a month or a year has no fixed length, so it moves a time written as a value (now(), '2026-01-31'), not a column's; write days")))
                }
                x => ir::Expr::Arith { op: ArithOp::Add, left: Box::new(x), right: Box::new(ir::Expr::Lit(Value::Int(iv.micros))) },
            }
        }
        e if super::sql::time_plus_number(e).is_some() => return Err(super::sql::time_plus_number(e).expect("an error")),
        Expr::Interval(_) => return Err(err(format!("{e} is a length of time: add it to a time or subtract it from one (now() - {e})"))),
        Expr::TypedString(sql::TypedString { data_type, value, .. }) => match &value.value {
            sql::Value::SingleQuotedString(s) | sql::Value::DoubleQuotedString(s) => ir::Expr::Lit(super::sql::cast_value(Value::Str(s.clone()), CastTo::of(data_type)?)?),
            _ => return Err(err(format!("unsupported expression: {e}"))),
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
            // Set operators of lists: `&&` holds when a value of one is a
            // value of the other; `@>` when each value of a list written out
            // (ARRAY[...], a list parameter) is one of the other side's.
            // Lists without values are empty sets, as over one table.
            if matches!(op, B::PGOverlap | B::AtArrow | B::ArrowAt) {
                // ARRAY[...] of values is a list value.
                let side = |x: &Expr| -> Result<Box<ir::Expr>> {
                    match x {
                        Expr::Array(a) => {
                            let vs = a.elem.iter().map(|v| match go(v)? {
                                ir::Expr::Lit(v) => Ok(v),
                                _ => Err(err(format!("{x}: an ARRAY of a set operator holds values"))),
                            });
                            Ok(Box::new(ir::Expr::Lit(Value::List(vs.collect::<Result<_>>()?))))
                        }
                        x => listy(x),
                    }
                };
                let (list, of, contains) = match op {
                    B::PGOverlap => return Ok(ir::Expr::In { expr: side(left)?, list: vec![*side(right)?] }),
                    B::AtArrow => (right, left, "@>"),
                    _ => (left, right, "<@"),
                };
                let values = match *side(list)? {
                    ir::Expr::Lit(Value::List(vs)) => vs,
                    ir::Expr::Lit(Value::Null) => return Err(err(format!("{e}: the list is NULL"))),
                    ir::Expr::Lit(v) => vec![v],
                    _ => return Err(err(format!("{e}: over rows in memory, {contains} tests a list column against a list of values written out (ARRAY[...] or a list parameter), with that list on the {} side; && compares any two", if contains == "@>" { "right" } else { "left" }))),
                };
                if values.iter().any(|v| *v == Value::Null) {
                    return Err(err(format!("{e}: the list holds a NULL, which no value equals")));
                }
                let of = side(of)?;
                return Ok(ir::Expr::And(values.into_iter().map(|v| ir::Expr::In { expr: of.clone(), list: vec![ir::Expr::Lit(v)] }).collect()));
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
        e if super::sql::is_now(e) => ir::Expr::Lit(Value::Timestamp(crate::timestamp::now())),
        e if super::sql::is_today(e) => ir::Expr::Lit(super::sql::day_of(Value::Timestamp(crate::timestamp::now()), &|| e.to_string())?),
        // DATE(t): the start of t's day, as date_trunc('day', t) and
        // CAST(t AS DATE) are.
        Expr::Function(f) if fname(f) == "date" && f.over.is_none() && func_args(f).len() == 1 => match go(func_args(f)[0])? {
            ir::Expr::Lit(v) => ir::Expr::Lit(super::sql::day_of(super::sql::cast_value(v, CastTo::Timestamp)?, &|| e.to_string())?),
            x => ir::Expr::Call { func: Func::DateTrunc, args: vec![ir::Expr::Lit(Value::Str("day".into())), x] },
        },
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
        Expr::Function(f) if f.over.is_some() => return Err(err(format!("{e}: a window function is a column of SELECT, ORDER BY or QUALIFY, not part of WHERE, ON, GROUP BY or HAVING"))),
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
                "date_trunc" => Func::DateTrunc,
                "date_part" => Func::DatePart,
                "strftime" => Func::Strftime,
                "date_bin" => Func::DateBin,
                "time_bucket" => Func::DateBin,
                "hash" => Func::Hash,
                n if Func::numeric(n).is_some() => Func::numeric(n).expect("a numeric function"),
                n if NATIVE.contains(&n) => {
                    return Err(err(format!("{}(...) works on one table's scan, in its WHERE or ORDER BY (in a join, on that table's column: a.body), not on values of rows", n.to_uppercase())))
                }
                n if AGGREGATES.contains(&n) => return Err(err(format!("{}(...) is an aggregate: it needs GROUP BY or a SELECT of aggregates, not a value of one row", n.to_uppercase()))),
                n => return Err(err(unknown_function(n))),
            };
            let mut args: Vec<ir::Expr> = Vec::new();
            for (i, a) in func_args(f).into_iter().enumerate() {
                args.push(match (func, i) {
                    // date_bin(INTERVAL '5 minutes', t) (and time_bucket, its other
                    // name): the width as the text the evaluator reads.
                    (Func::DateBin, 0) => match super::sql::interval(a) {
                        Some(iv) => {
                            let iv = iv?;
                            if iv.months != 0 || iv.micros <= 0 {
                                return Err(err(format!("{e}: a bucket's width is a positive length of time without months or years, such as INTERVAL '5 minutes'")));
                            }
                            ir::Expr::Lit(Value::Str(crate::timestamp::stride_text(iv.micros)))
                        }
                        None => go(a)?,
                    },
                    _ => go(a)?,
                });
            }
            func.arity(args.len()).map_err(|a| err(format!("{}() takes {a}", func.name())))?;
            ir::Expr::Call { func, args }
        }
        // CAST(x AS type) and x::type: a value cast now; a column of the
        // rows by the arithmetic and functions that make the type.
        Expr::Cast { expr, data_type, format: None, .. } => {
            let to = CastTo::of(data_type)?;
            let day = super::sql::is_date(data_type);
            match go(expr)? {
                ir::Expr::Lit(v) if day => ir::Expr::Lit(super::sql::day_of(super::sql::cast_value(v, to)?, &|| e.to_string())?),
                ir::Expr::Lit(v) => ir::Expr::Lit(super::sql::cast_value(v, to)?),
                // A column's day: date_trunc('day', at).
                x if day => ir::Expr::Call { func: Func::DateTrunc, args: vec![ir::Expr::Lit(Value::Str("day".into())), x] },
                x => match to {
                    CastTo::Float => ir::Expr::Arith { op: ArithOp::Mul, left: Box::new(x), right: Box::new(ir::Expr::Lit(Value::Float(1.0))) },
                    CastTo::Int => ir::Expr::Call { func: Func::Round, args: vec![x] },
                    CastTo::Text => ir::Expr::Case {
                        operand: None,
                        whens: vec![When { when: ir::Expr::IsNull(Box::new(x.clone())), then: ir::Expr::Lit(Value::Null) }],
                        otherwise: Some(Box::new(ir::Expr::Concat(vec![x]))),
                    },
                    CastTo::Bool | CastTo::Timestamp => {
                        return Err(err(format!(
                            "{e}: a column of the rows casts to INT, FLOAT, TEXT or DATE (its day, as date_trunc('day', at)); BOOL and TIMESTAMP take a value, such as CAST('2026-11-02' AS TIMESTAMP)"
                        )))
                    }
                },
            }
        }
        e @ (Expr::Identifier(_) | Expr::CompoundIdentifier(_)) => return Err(err(format!("no such column: {e}"))),
        Expr::Substring { .. } => return Err(err(unknown_function("substring"))),
        Expr::Trim { .. } => return Err(err(unknown_function("trim"))),
        Expr::Position { .. } => return Err(err(unknown_function("position"))),
        Expr::Overlay { .. } => return Err(err(unknown_function("replace"))),
        Expr::Ceil { .. } => return Err(err(unknown_function("ceil"))),
        Expr::Floor { .. } => return Err(err(unknown_function("floor"))),
        // EXTRACT(part FROM t) is date_part('part', t).
        Expr::Extract { field, expr, .. } => {
            let part = extract_part(field);
            ir::Expr::Call { func: Func::DatePart, args: vec![ir::Expr::Lit(Value::Str(part)), go(expr)?] }
        }
        e => return Err(err(format!("{e} is not something ChloeDB computes for a row: {}", ROW_EXPRESSIONS))),
    })
}

/// What the row evaluator computes, for an error about what it does not.
const ROW_EXPRESSIONS: &str = "it computes columns, values and parameters with + - * / %, ||, comparisons, AND/OR/NOT, IN, BETWEEN, LIKE, ILIKE, IS NULL, CASE, CAST, EXTRACT and the functions lower, upper, length, abs, round, coalesce, ln, log, exp, sqrt, pow, least, greatest, sigmoid, clamp, dot, date_trunc, date_part, strftime, time_bucket and hash";

/// The part `EXTRACT(part FROM t)` names, as `date_part` takes it.
pub(crate) fn extract_part(field: &sql::DateTimeField) -> String {
    match field {
        sql::DateTimeField::Custom(id) => id.value.to_ascii_lowercase(),
        sql::DateTimeField::Week(_) | sql::DateTimeField::IsoWeek => "week".into(),
        sql::DateTimeField::DayOfWeek => "dow".into(),
        sql::DateTimeField::DayOfYear => "doy".into(),
        f => f.to_string().to_ascii_lowercase(),
    }
}

/// Why a function is not one the row evaluator knows: the one meant, if
/// the name is near one, or what there is.
fn unknown_function(n: &str) -> String {
    const ROW_FUNCTIONS: [&str; 22] = [
        "lower", "upper", "length", "abs", "round", "coalesce", "ln", "log", "exp", "sqrt", "pow", "least", "greatest", "sigmoid", "clamp", "dot", "date_trunc", "date_part", "strftime", "date_bin", "time_bucket", "hash",
    ];
    let hint = match n {
        "substr" | "substring" | "left" | "right" | "trim" | "ltrim" | "rtrim" | "replace" | "concat" | "position" | "instr" | "strpos" | "split_part" | "reverse" => {
            "; to match part of a text, use LIKE '%part%' (ILIKE ignores case) or PREFIX(field, 'start'); to join texts, use ||"
        }
        "date" | "year" | "month" | "day" | "hour" | "week" | "to_char" | "datepart" | "datetrunc" | "date_format" | "datediff" | "date_add" | "date_sub" | "dayofweek" | "weekday" => {
            "; a time's parts are date_trunc('day', at) (its day, week, month, ...), date_bin('5 minutes', at) (its bucket of a fixed width), EXTRACT(year FROM at) or date_part('dow', at), and strftime('%Y-%m-%d', at); a time later or earlier is at + n or at - n, n in microseconds"
        }
        "random" | "rand" => "; ORDER BY random() is not supported: for a seeded shuffle, ORDER BY hash(id, 7), and for a sample of about a tenth, WHERE hash(id, 7) % 10 = 0",
        "ifnull" | "nvl" | "isnull" => "; coalesce(x, y) gives x, or y where x is NULL",
        "floor" | "ceil" | "ceiling" | "trunc" | "truncate" => "; round(x) rounds to a whole number, round(x, n) to n decimals",
        "char_length" | "character_length" | "len" => "; length(text) counts its characters",
        "llm" => "; llm() asks a language model about the rows a query returns, after the query finds them: Db::query and a server's queries run it, not a plan",
        "lcase" => "; lower(text)",
        "ucase" => "; upper(text)",
        _ => "",
    };
    format!(
        "unknown function {n}(){}: over rows ChloeDB computes {}, and over groups the aggregates {}{hint}",
        hint::did_you_mean(n, ROW_FUNCTIONS.iter().chain(AGGREGATES.iter()).chain(STATS.iter()).copied()),
        ROW_FUNCTIONS.join(", "),
        AGGREGATES.iter().chain(STATS.iter()).map(|a| a.to_uppercase()).collect::<Vec<_>>().join(", ")
    )
}

/// An aggregate call: its text (how other expressions name it), function and argument.
struct AggCall {
    text: String,
    func: Option<AggFn>,
    arg: Option<Expr>,
}

fn agg_calls(exprs: &[&Expr], params: &Params) -> Result<Vec<AggCall>> {
    let mut out: Vec<AggCall> = Vec::new();
    let mut failed = None;
    for e in exprs {
        walk(e, &mut |x| {
            let Expr::Function(f) = x else { return };
            let name = fname(f);
            if !is_aggregate(f) || out.iter().any(|a| a.text == x.to_string()) {
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
                    "max" => AggFn::Max,
                    _ => match percentile(f, &name, distinct, params) {
                        Ok(func) => func,
                        Err(e) => {
                            failed.get_or_insert(e);
                            return;
                        }
                    },
                };
                (Some(func), func_args(f).first().map(|a| (*a).clone()))
            };
            out.push(AggCall { text: x.to_string(), func, arg });
        });
    }
    failed.map_or(Ok(out), Err)
}

/// The percentile a call of one of [`PERCENTILES`] computes: `MEDIAN(x)`,
/// or `PERCENTILE_CONT(x, p)` and the others with a constant fraction p
/// from 0 to 1.
fn percentile(f: &sql::Function, name: &str, distinct: bool, params: &Params) -> Result<AggFn> {
    let upper = name.to_uppercase();
    if distinct {
        return Err(err("DISTINCT works in COUNT only"));
    }
    let args = func_args(f);
    let p = match (name, args.as_slice()) {
        ("median", [_]) => 0.5,
        ("median", _) => return Err(err("MEDIAN takes one value: MEDIAN(price)")),
        (_, [_, p]) => match compile(p, params, &|_| None, &[]).and_then(|e| value(&e)) {
            Ok(v) => v.as_f64().filter(|_| matches!(v, Value::Int(_) | Value::Float(_))).ok_or_else(|| err(format!("{upper}: the fraction is a number from 0 to 1, not {p}")))?,
            Err(_) => return Err(err(format!("{upper}: the fraction is a constant from 0 to 1, not {p}"))),
        },
        _ => return Err(err(format!("{upper}(p) WITHIN GROUP (ORDER BY x): p, from 0 to 1, of one value of each row"))),
    };
    let p = Fraction::new(p).ok_or_else(|| err(format!("{upper}: the fraction is from 0 to 1, not {p}")))?;
    Ok(match name {
        "percentile_disc" => AggFn::PercentileDisc(p),
        "approx_percentile" => AggFn::ApproxPercentile(p),
        _ => AggFn::PercentileCont(p),
    })
}

/// A window function's call: its text (how other expressions name it) and
/// the call.
struct WindowCall {
    text: String,
    call: sql::Function,
}

/// The window functions' calls in `exprs`, each once. A window function
/// reads rows, not other windows' values.
fn window_calls(exprs: &[&Expr]) -> Result<Vec<WindowCall>> {
    let mut out: Vec<WindowCall> = Vec::new();
    let mut failed = None;
    for e in exprs {
        walk(e, &mut |x| {
            let Expr::Function(f) = x else { return };
            let Some(sql::WindowType::WindowSpec(w)) = &f.over else { return };
            let inner = func_args(f).into_iter().any(has_window) || w.partition_by.iter().any(has_window) || w.order_by.iter().any(|o| has_window(&o.expr));
            if inner && failed.is_none() {
                failed = Some(err(format!("{x}: a window function's argument, partition or order holds another; compute that one in a subquery in FROM")));
            }
            let text = x.to_string();
            if !out.iter().any(|c| c.text == text) {
                out.push(WindowCall { text, call: f.clone() });
            }
        });
    }
    failed.map_or(Ok(out), Err)
}

/// A whole number of a window function's call (`NTILE(4)`, `LAG(x, 2)`,
/// `NTH_VALUE(x, 3)`) at least `min`: a constant.
fn count_arg(e: &ir::Expr, what: &str, min: i64) -> Result<u64> {
    match value(e) {
        Ok(Value::Int(n)) if n >= min => Ok(n as u64),
        _ => Err(err(format!("{what} takes a whole number of at least {min}, not {e:?}"))),
    }
}

/// The plan's window function of a call: `compile` compiles an expression
/// over the rows the window reads.
fn window_fn(f: &sql::Function, params: &Params, compile: &dyn Fn(&Expr) -> Result<ir::Expr>) -> Result<ir::WindowFn> {
    let name = fname(f);
    let Some(sql::WindowType::WindowSpec(w)) = &f.over else { return Err(err(format!("{name}: OVER names no window"))) };
    if f.filter.is_some() {
        return Err(err(format!("{name}(..) FILTER (WHERE ..) OVER: a window aggregate takes no FILTER; aggregate a CASE (SUM(CASE WHEN .. THEN x END))")));
    }
    if f.null_treatment == Some(sql::NullTreatment::IgnoreNulls) || matches!(&f.args, FunctionArguments::List(l) if l.clauses.iter().any(|c| matches!(c, sql::FunctionArgumentClause::IgnoreOrRespectNulls(sql::NullTreatment::IgnoreNulls)))) {
        return Err(err(format!("{name}(..) IGNORE NULLS is not supported")));
    }
    if !f.within_group.is_empty() || matches!(&f.args, FunctionArguments::List(l) if !l.clauses.iter().all(|c| matches!(c, sql::FunctionArgumentClause::IgnoreOrRespectNulls(_)))) {
        return Err(err(format!("{name}(..): a window function's arguments are expressions")));
    }
    if matches!(&f.args, FunctionArguments::List(l) if l.duplicate_treatment == Some(sql::DuplicateTreatment::Distinct)) {
        return Err(err(format!("{name}(DISTINCT ..) OVER is not supported")));
    }
    let args = func_args(f);
    let arity = |lo: usize, hi: usize, usage: &str| -> Result<()> {
        match (lo..=hi).contains(&args.len()) {
            true => Ok(()),
            false => Err(err(format!("{name} is {usage}"))),
        }
    };
    let func = match name.as_str() {
        "row_number" | "rank" | "dense_rank" | "percent_rank" | "cume_dist" => {
            arity(0, 0, &format!("{}() OVER (..), of no arguments", name.to_ascii_uppercase()))?;
            match name.as_str() {
                "row_number" => ir::WindowFunc::RowNumber,
                "rank" => ir::WindowFunc::Rank,
                "dense_rank" => ir::WindowFunc::DenseRank,
                "percent_rank" => ir::WindowFunc::PercentRank,
                _ => ir::WindowFunc::CumeDist,
            }
        }
        "ntile" => {
            arity(1, 1, "NTILE(buckets) OVER (..)")?;
            ir::WindowFunc::Ntile(count_arg(&compile(args[0])?, "NTILE", 1)?)
        }
        "lag" | "lead" => {
            arity(1, 3, &format!("{}(x [, offset [, default]]) OVER (..)", name.to_ascii_uppercase()))?;
            let arg = compile(args[0])?;
            let offset = match args.get(1) {
                None => 1,
                Some(o) => count_arg(&compile(o)?, &name.to_ascii_uppercase(), 0)?,
            };
            // A constant default as its value (`-1.5`, not `-(1.5)`).
            let default = args.get(2).map(|d| compile(d).map(|e| value(&e).map_or(e, ir::Expr::Lit))).transpose()?;
            match name.as_str() {
                "lag" => ir::WindowFunc::Lag { arg, offset, default },
                _ => ir::WindowFunc::Lead { arg, offset, default },
            }
        }
        "first_value" | "last_value" => {
            arity(1, 1, &format!("{}(x) OVER (..)", name.to_ascii_uppercase()))?;
            let arg = compile(args[0])?;
            if name == "first_value" { ir::WindowFunc::FirstValue(arg) } else { ir::WindowFunc::LastValue(arg) }
        }
        "nth_value" => {
            arity(2, 2, "NTH_VALUE(x, n) OVER (..)")?;
            ir::WindowFunc::NthValue { arg: compile(args[0])?, n: count_arg(&compile(args[1])?, "NTH_VALUE", 1)? }
        }
        "count" | "sum" | "avg" | "min" | "max" => {
            let func = match name.as_str() {
                "count" => AggFn::Count,
                "sum" => AggFn::Sum,
                "avg" => AggFn::Avg,
                "min" => AggFn::Min,
                _ => AggFn::Max,
            };
            let arg = match (name.as_str(), is_star(f)) {
                ("count", true) => None,
                (_, true) => return Err(err(format!("{}(x) OVER (..) takes a column or an expression", name.to_ascii_uppercase()))),
                _ => {
                    arity(1, 1, &format!("{}(x) OVER (..)", name.to_ascii_uppercase()))?;
                    Some(compile(args[0])?)
                }
            };
            ir::WindowFunc::Agg(ir::Agg { func, arg })
        }
        _ => {
            return Err(err(format!(
                "{name} is not a window function: ROW_NUMBER, RANK, DENSE_RANK, PERCENT_RANK, CUME_DIST, NTILE, LAG, LEAD, FIRST_VALUE, LAST_VALUE, NTH_VALUE, COUNT, SUM, AVG, MIN and MAX are"
            )))
        }
    };
    let partition = w.partition_by.iter().map(compile).collect::<Result<Vec<_>>>()?;
    let order = w
        .order_by
        .iter()
        .map(|o| Ok(ir::SortKey { expr: compile(&o.expr)?, desc: o.options.asc == Some(false), nulls_first: o.options.nulls_first == Some(true) }))
        .collect::<Result<Vec<_>>>()?;
    // A frame matters to the functions that read one; the others read the
    // partition in order whatever it says, as in SQL.
    let frame = match &w.window_frame {
        Some(fr) if func.framed() => Some(frame_of(fr, order.len(), params)?),
        _ => None,
    };
    Ok(ir::WindowFn { func, partition, order, frame })
}

/// A window's frame: ROWS or RANGE, between literal offsets.
fn frame_of(fr: &sql::WindowFrame, keys: usize, params: &Params) -> Result<ir::Frame> {
    let units = match fr.units {
        sql::WindowFrameUnits::Rows => ir::FrameUnits::Rows,
        sql::WindowFrameUnits::Range => ir::FrameUnits::Range,
        sql::WindowFrameUnits::Groups => return Err(err("GROUPS frames are not supported: count ROWS or a RANGE of the order's values")),
    };
    let offset = |x: &Expr| -> Result<ir::Expr> {
        let v = value(&compile(x, params, &|_| None, &[]).map_err(|_| err(format!("{x}: a frame's offset is a number, not an expression of the row")))?)?;
        match (units, &v) {
            (ir::FrameUnits::Rows, Value::Int(n)) if *n >= 0 => {}
            (ir::FrameUnits::Rows, _) => return Err(err(format!("ROWS {x} PRECEDING/FOLLOWING: a ROWS offset is a whole number of rows, 0 or more"))),
            (ir::FrameUnits::Range, Value::Int(n)) if *n >= 0 => {}
            (ir::FrameUnits::Range, Value::Float(n)) if *n >= 0.0 => {}
            (ir::FrameUnits::Range, _) => return Err(err(format!("RANGE {x} PRECEDING/FOLLOWING: a RANGE offset is a number, 0 or more"))),
        }
        if units == ir::FrameUnits::Range && keys != 1 {
            return Err(err(format!("RANGE {x} PRECEDING/FOLLOWING measures from the value of the window's one ORDER BY key, and it has {keys}")));
        }
        Ok(ir::Expr::Lit(v))
    };
    let bound = |b: &sql::WindowFrameBound| -> Result<ir::Bound> {
        Ok(match b {
            sql::WindowFrameBound::CurrentRow => ir::Bound::CurrentRow,
            sql::WindowFrameBound::Preceding(None) => ir::Bound::UnboundedPreceding,
            sql::WindowFrameBound::Preceding(Some(x)) => ir::Bound::Preceding(offset(x)?),
            sql::WindowFrameBound::Following(None) => ir::Bound::UnboundedFollowing,
            sql::WindowFrameBound::Following(Some(x)) => ir::Bound::Following(offset(x)?),
        })
    };
    let start = bound(&fr.start_bound)?;
    let end = fr.end_bound.as_ref().map(bound).transpose()?.unwrap_or(ir::Bound::CurrentRow);
    // As SQL: a frame starts no later than it ends, by its kinds of bound.
    let rank = |b: &ir::Bound| match b {
        ir::Bound::UnboundedPreceding => 0,
        ir::Bound::Preceding(_) => 1,
        ir::Bound::CurrentRow => 2,
        ir::Bound::Following(_) => 3,
        ir::Bound::UnboundedFollowing => 4,
    };
    if matches!(start, ir::Bound::UnboundedFollowing) || matches!(end, ir::Bound::UnboundedPreceding) || rank(&start) > rank(&end) {
        let shown = match &fr.end_bound {
            Some(e) => format!("{} BETWEEN {} AND {e}", fr.units, fr.start_bound),
            None => format!("{} {}", fr.units, fr.start_bound),
        };
        return Err(err(format!("{shown}: a frame starts no later than it ends, in the order UNBOUNDED PRECEDING, n PRECEDING, CURRENT ROW, n FOLLOWING, UNBOUNDED FOLLOWING")));
    }
    Ok(ir::Frame { units, start, end })
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
        // QUALIFY, which may name selected window functions by their alias.
        let qualify = s.qualify.as_ref().map(|q| having_with_aliases(q, &items));
        let mut item_exprs: Vec<&Expr> = items.iter().map(|i| &i.1).collect();
        item_exprs.extend(s.having.iter());
        item_exprs.extend(order.iter().map(|o| &o.0));
        item_exprs.extend(qualify.iter());
        let calls = agg_calls(&item_exprs, params)?;
        let windows = window_calls(&item_exprs)?;
        let grouped = !group_exprs.is_empty() || !calls.is_empty();
        // The window functions over the rows (or the groups) as `slot`
        // names their columns, then QUALIFY: the rows with a column per
        // function after their own `width`.
        let window = |node: ir::Node, width: usize, slot: &dyn Fn(&Expr) -> Option<Result<ir::Expr>>, multi: &[bool]| -> Result<ir::Node> {
            let mut node = node;
            if !windows.is_empty() {
                let compile = |e: &Expr| compile_with(e, params, slot, multi, Some(&subs));
                let funcs = windows.iter().map(|c| window_fn(&c.call, params, &compile)).collect::<Result<Vec<_>>>()?;
                node = ir::Node::Window { input: Box::new(node), funcs };
            }
            if let Some(q) = &qualify {
                let wslot = |e: &Expr| windows.iter().position(|c| c.text == e.to_string()).map(|j| Ok(ir::Expr::Output((width + j) as u32))).or_else(|| slot(e));
                let cond = compile_with(q, params, &wslot, multi, Some(&subs))?;
                node = ir::Node::Filter { input: Box::new(node), cond };
            }
            Ok(node)
        };
        let window_slot = |e: &Expr, width: usize| windows.iter().position(|c| c.text == e.to_string()).map(|j| Ok(ir::Expr::Output((width + j) as u32)));
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
            // The Aggregate's columns, its count of rows the last.
            let width = k + calls.len() + 1;
            node = window(node, width, &group_slot, &[])?;
            let group_slot = |e: &Expr| window_slot(e, width).or_else(|| group_slot(e));
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
            node = window(node, cols.len(), &|e| at(slot(e, cols)), multi)?;
            let row_slot = |e: &Expr| window_slot(e, cols.len()).or_else(|| at(slot(e, cols)));
            let items_at: Vec<ir::Expr> = items.iter().map(|(_, e)| compile_with(e, params, &row_slot, multi, Some(&subs))).collect::<Result<_>>()?;
            if !order.is_empty() && !driver {
                let keys = sort_keys(&items_at, &row_slot, multi)?;
                node = ir::Node::Sort { input: Box::new(node), keys };
            }
            node = first_of(node, &items_at, &row_slot, multi)?;
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
