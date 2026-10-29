//! SQL front end: parses Strata's dialect with `sqlparser` and binds it to a
//! table schema, producing a [`Select`].
//!
//! Supported: `SELECT <cols | * | score() | COUNT(*)> FROM <table>
//! [WHERE <pred>] [GROUP BY <keyword>] [ORDER BY <score> [ASC|DESC]]
//! [LIMIT n [OFFSET m]]`, where predicates combine `MATCH(field, q)`,
//! comparisons, `BETWEEN`, `IN` with AND/OR/NOT, and scores are `bm25`,
//! `similarity` (on a VECTOR or a SPARSE field), `distance`, `decay`, `rrf`,
//! weighted sums and products of those, or a column.
//! Parameters (`:name`) are bound from [`Params`].
//!
//! Writes ([`bind_command`], [`bind_write`]): `INSERT INTO t (cols) VALUES
//! (...), ...` with `OR REPLACE`, `OR IGNORE` or `ON CONFLICT (key) DO
//! NOTHING | DO UPDATE SET ...`, `UPDATE t SET f = v, g = g + 1 [WHERE ...]
//! [LIMIT n]` and `DELETE FROM t [WHERE ...] [ORDER BY ...] [LIMIT n]`, of
//! any table of the database. Lists are `ARRAY[...]`; vectors are
//! parameters, `ARRAY[...]` or a JSON array string. `CREATE TABLE` and
//! `DROP TABLE` are bound in [`crate::query::ddl`]. A clause the binder does
//! not handle is an error, never ignored.
//!
//! Graph queries over EDGES fields (DESIGN.md §6.1): the predicates
//! `REACHABLE(field, seeds, ...)` and `PATH(field, from, to, ...)`, the score
//! `ppr(field, seeds, ...)` and the column `hops()`. Seeds are a key, a
//! parameter, a tuple of keys or a subquery over the same table.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use sqlparser::ast::{self as sql, BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments};

use crate::error::{Error, Result};
use crate::query::ddl;
use crate::query::logical::{AggFn, Decay, DecayShape, Direction, Expand, GraphKind, GraphQuery, Having, OrderBy, Opts, Pred, Projection, Score, Seeds, Select, Tiebreak};
use crate::schema::{FieldType, Logical, Schema};
use crate::storage::DropOptions;
use crate::value::Value;

/// Values for `:name` placeholders.
#[derive(Clone, Debug, Default)]
pub struct Params(pub HashMap<String, Value>);

impl Params {
    pub fn new() -> Self {
        Params::default()
    }
    pub fn set(mut self, name: &str, v: Value) -> Self {
        self.0.insert(name.trim_start_matches([':', '$', '@']).to_string(), v);
        self
    }
    fn get(&self, placeholder: &str) -> Result<&Value> {
        let name = placeholder.trim_start_matches([':', '$', '@', '?']);
        self.0.get(name).ok_or_else(|| err(format!("unbound parameter {placeholder}")))
    }
}

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// A bound INSERT, UPDATE or DELETE.
#[derive(Clone, Debug, PartialEq)]
pub enum Write {
    /// Documents in schema order (unnamed columns NULL), and what to do
    /// with one whose primary key is live.
    Insert { docs: Vec<Vec<Value>>, conflict: Conflict },
    /// The documents `select` returns (their primary keys) get `sets`.
    Update { select: Select, sets: Vec<(usize, Assign)> },
    /// The documents `select` returns are deleted.
    Delete { select: Select },
}

/// What an INSERT does with a document whose primary key is already live
/// (or written earlier by the same statement).
#[derive(Clone, Debug, PartialEq)]
pub enum Conflict {
    /// The statement fails: `INSERT`.
    Fail,
    /// The new document replaces the live one: `INSERT OR REPLACE`,
    /// `REPLACE INTO`.
    Replace,
    /// The live document stays and the new one is skipped: `INSERT OR
    /// IGNORE`, `ON CONFLICT DO NOTHING`.
    Skip,
    /// The live document gets these fields set, as by an UPDATE: `ON
    /// CONFLICT (key) DO UPDATE SET ...`.
    Update(Vec<(usize, Assign)>),
}

/// The right-hand side of `SET field = ...`.
#[derive(Clone, Debug, PartialEq)]
pub enum Assign {
    Value(Value),
    /// `field = field + x` (or `- x`): an INT or a FLOAT, integers exact.
    Add(Value),
    /// `field = excluded.g`, in `ON CONFLICT DO UPDATE`: field `g` of the
    /// document the INSERT proposed.
    Excluded(usize),
}

/// A bound statement ([`bind_command`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// A SELECT, bound when it runs ([`crate::db::Db::query`]).
    Query,
    /// An INSERT, UPDATE or DELETE of the table named `table`.
    Write { table: String, write: Write },
    /// `CREATE TABLE [IF NOT EXISTS] ...`.
    CreateTable { schema: Schema, if_not_exists: bool },
    /// `DROP TABLE [IF EXISTS] a, b [CASCADE | RESTRICT]`.
    DropTable { tables: Vec<String>, opts: DropOptions },
}

/// Parses one statement.
pub(crate) fn parse_one(sql_text: &str) -> Result<sql::Statement> {
    let mut stmts = super::parse::parse(sql_text)?;
    if stmts.len() != 1 {
        return Err(err("expected exactly one statement"));
    }
    Ok(stmts.pop().unwrap())
}

/// Parses one SELECT statement.
pub(crate) fn parse_query(sql_text: &str) -> Result<Box<sql::Query>> {
    match parse_one(sql_text)? {
        sql::Statement::Query(q) => Ok(q),
        _ => Err(err("only SELECT is supported")),
    }
}

/// Parses and binds one INSERT, UPDATE or DELETE of `schema`'s table;
/// `None` for a SELECT.
pub fn bind_write(sql_text: &str, schema: &Schema, params: &Params) -> Result<Option<Write>> {
    match bind_command(sql_text, &[schema], params)? {
        Command::Query => Ok(None),
        Command::Write { write, .. } => Ok(Some(write)),
        _ => Err(err("only SELECT, INSERT, UPDATE and DELETE are supported")),
    }
}

/// Parses and binds one statement of a database whose tables are `tables`:
/// a SELECT (bound when it runs), an INSERT, UPDATE or DELETE of one of
/// them, `CREATE TABLE` or `DROP TABLE`.
pub fn bind_command(sql_text: &str, tables: &[&Schema], params: &Params) -> Result<Command> {
    let table = |name: &sql::ObjectName| -> Result<&Schema> {
        let n = name.to_string();
        tables.iter().copied().find(|t| t.table.eq_ignore_ascii_case(&n)).ok_or_else(|| err(format!("no such table: {n}")))
    };
    let write = |schema: &Schema, write: Write| Command::Write { table: schema.table.clone(), write };
    Ok(match &parse_one(sql_text)? {
        sql::Statement::Query(_) => Command::Query,
        sql::Statement::Insert(ins) => {
            let sql::TableObject::TableName(n) = &ins.table else { return Err(err("INSERT needs a table name")) };
            let schema = table(n)?;
            write(schema, insert(ins, schema, params)?)
        }
        sql::Statement::Update { table: t, assignments, from, selection, returning, or, limit } => {
            if !t.joins.is_empty() || from.is_some() {
                return Err(err("UPDATE ... FROM and joins are not supported: UPDATE one table, with a WHERE over its own columns"));
            }
            if returning.is_some() {
                return Err(err("UPDATE ... RETURNING is not supported"));
            }
            if let Some(o) = or {
                return Err(err(format!("UPDATE {o} is not supported")));
            }
            let schema = table(plain_table(&t.relation, "UPDATE")?)?;
            let sets = Binder::new(schema, params).assignments(assignments, false)?;
            write(schema, Write::Update { select: selecting(schema, params, selection, &[], limit)?, sets })
        }
        sql::Statement::Delete(d) => {
            let sql::Delete { tables: named, from, using, selection, returning, order_by, limit } = d;
            if !named.is_empty() || using.is_some() {
                return Err(err("DELETE ... USING and multi-table DELETE are not supported: DELETE FROM one table, with a WHERE over its own columns"));
            }
            if returning.is_some() {
                return Err(err("DELETE ... RETURNING is not supported"));
            }
            let (sql::FromTable::WithFromKeyword(from) | sql::FromTable::WithoutKeyword(from)) = from;
            let [t] = from.as_slice() else { return Err(err("DELETE from exactly one table")) };
            if !t.joins.is_empty() {
                return Err(err("DELETE from exactly one table"));
            }
            let schema = table(plain_table(&t.relation, "DELETE")?)?;
            write(schema, Write::Delete { select: selecting(schema, params, selection, order_by, limit)? })
        }
        sql::Statement::CreateTable(ct) => {
            let (schema, if_not_exists) = ddl::create_table(ct, tables)?;
            Command::CreateTable { schema, if_not_exists }
        }
        stmt @ sql::Statement::Drop { .. } => {
            let (tables, opts) = ddl::drop_table(stmt)?;
            Command::DropTable { tables, opts }
        }
        _ => return Err(err("only SELECT, INSERT, UPDATE, DELETE, CREATE TABLE and DROP TABLE are supported")),
    })
}

/// The name of a plain table in FROM (an alias allowed), for `what`.
fn plain_table<'t>(t: &'t sql::TableFactor, what: &str) -> Result<&'t sql::ObjectName> {
    match t {
        sql::TableFactor::Table { name, alias: _, args: None, with_hints, version: None, with_ordinality: false, partitions, json_path: None, sample: None, index_hints }
            if with_hints.is_empty() && partitions.is_empty() && index_hints.is_empty() =>
        {
            Ok(name)
        }
        _ => Err(err(format!("{what} needs a table name, not {t}"))),
    }
}

/// Binds an INSERT into `schema`'s table.
fn insert(ins: &sql::Insert, schema: &Schema, params: &Params) -> Result<Write> {
    let sql::Insert {
        or,
        ignore,
        into: _,
        table: _,
        table_alias,
        columns,
        overwrite,
        source,
        assignments,
        partitioned,
        after_columns,
        has_table_keyword,
        on,
        returning,
        replace_into,
        priority,
        insert_alias,
        settings,
        format_clause,
    } = ins;
    if returning.is_some() {
        return Err(err("INSERT ... RETURNING is not supported"));
    }
    if !assignments.is_empty() {
        return Err(err("INSERT ... SET is not supported: INSERT INTO t (columns) VALUES (...)"));
    }
    if table_alias.is_some() || *overwrite || partitioned.is_some() || !after_columns.is_empty() || *has_table_keyword || priority.is_some() || insert_alias.is_some() || settings.is_some() || format_clause.is_some() {
        return Err(err(format!("this form of INSERT is not supported: {ins}")));
    }
    let b = Binder::new(schema, params);
    let mut conflict = match (or, *replace_into, *ignore) {
        (Some(sql::SqliteOnConflict::Replace), ..) | (None, true, false) => Conflict::Replace,
        (Some(sql::SqliteOnConflict::Ignore), ..) | (None, false, true) => Conflict::Skip,
        (None, false, false) => Conflict::Fail,
        (Some(o), ..) => return Err(err(format!("INSERT {o} is not supported: INSERT fails on a key that is taken, OR REPLACE replaces that row and OR IGNORE keeps it"))),
        (None, true, true) => return Err(err("REPLACE IGNORE is not supported")),
    };
    match on {
        None => {}
        Some(_) if conflict != Conflict::Fail => return Err(err("INSERT OR ... and ON CONFLICT cannot be combined")),
        Some(sql::OnInsert::DuplicateKeyUpdate(_)) => return Err(err("ON DUPLICATE KEY UPDATE is not supported: use ON CONFLICT (key) DO UPDATE SET ...")),
        Some(sql::OnInsert::OnConflict(oc)) => {
            let pk = schema.primary_key.ok_or_else(|| err(format!("ON CONFLICT needs a primary key, and {} has none", schema.table)))?;
            match &oc.conflict_target {
                None => {}
                Some(sql::ConflictTarget::Columns(cols)) if cols.len() == 1 && schema.field_index(&cols[0].value) == Some(pk) => {}
                Some(_) => return Err(err(format!("ON CONFLICT names the primary key: ON CONFLICT ({})", schema.fields[pk].name))),
            }
            conflict = match &oc.action {
                sql::OnConflictAction::DoNothing => Conflict::Skip,
                sql::OnConflictAction::DoUpdate(u) => {
                    if u.selection.is_some() {
                        return Err(err("ON CONFLICT ... DO UPDATE ... WHERE is not supported"));
                    }
                    Conflict::Update(b.assignments(&u.assignments, true)?)
                }
            };
        }
        Some(o) => return Err(err(format!("INSERT ...{o} is not supported"))),
    }
    let cols: Vec<usize> = if columns.is_empty() {
        (0..schema.fields.len()).collect()
    } else {
        columns.iter().map(|c| schema.field_index(&c.value).ok_or_else(|| err(format!("no such column: {}", c.value)))).collect::<Result<_>>()?
    };
    let values = match source.as_deref() {
        Some(q) if no_clauses(q).is_none() => match q.body.as_ref() {
            sql::SetExpr::Values(v) => v,
            _ => return Err(err("INSERT needs VALUES")),
        },
        Some(q) => return Err(err(format!("INSERT ... VALUES takes no {}", no_clauses(q).unwrap_or_default()))),
        None => return Err(err("INSERT needs VALUES")),
    };
    let mut docs = Vec::with_capacity(values.rows.len());
    for row in &values.rows {
        if row.len() != cols.len() {
            return Err(err(format!("{} values for {} columns", row.len(), cols.len())));
        }
        let mut doc = vec![Value::Null; schema.fields.len()];
        for (&f, e) in cols.iter().zip(row) {
            doc[f] = b.write_value(f, e)?;
        }
        docs.push(doc);
    }
    Ok(Write::Insert { docs, conflict })
}

/// The first clause of `q` other than its body: `WITH`, `ORDER BY`,
/// `LIMIT` and the clauses no statement here takes.
fn no_clauses(q: &sql::Query) -> Option<String> {
    let sql::Query { with, body: _, order_by, limit_clause, fetch, locks, for_clause, settings, format_clause, pipe_operators } = q;
    if with.is_some() {
        Some("WITH".into())
    } else if order_by.is_some() {
        Some("ORDER BY".into())
    } else if limit_clause.is_some() {
        Some("LIMIT".into())
    } else {
        unsupported_query_clause(fetch, locks, for_clause, settings, format_clause, pipe_operators)
    }
}

/// The clauses of a query that no statement here takes, by name.
fn unsupported_query_clause(
    fetch: &Option<sql::Fetch>,
    locks: &[sql::LockClause],
    for_clause: &Option<sql::ForClause>,
    settings: &Option<Vec<sql::Setting>>,
    format_clause: &Option<sql::FormatClause>,
    pipe_operators: &[sql::PipeOperator],
) -> Option<String> {
    if fetch.is_some() {
        Some("FETCH (use LIMIT)".into())
    } else if !locks.is_empty() {
        Some("FOR UPDATE or FOR SHARE (reads never lock: they see a snapshot)".into())
    } else if let Some(f) = for_clause {
        Some(f.to_string())
    } else if settings.is_some() {
        Some("SETTINGS".into())
    } else if let Some(f) = format_clause {
        Some(f.to_string())
    } else if !pipe_operators.is_empty() {
        Some("pipe operators".into())
    } else {
        None
    }
}

/// The rows a WHERE clause selects, as a query of every field (an UPDATE
/// may rewrite them; a DELETE needs their keys), cut by `ORDER BY` and
/// `LIMIT` as a SELECT's rows are.
fn selecting(schema: &Schema, params: &Params, selection: &Option<Expr>, order_by: &[sql::OrderByExpr], limit: &Option<Expr>) -> Result<Select> {
    let cols: Vec<String> = schema.fields.iter().map(|f| ident(&f.name)).collect();
    let mut text = format!("SELECT {} FROM {}", cols.join(", "), ident(&schema.table));
    if let Some(e) = selection {
        text += &format!(" WHERE {e}");
    }
    if !order_by.is_empty() {
        text += &format!(" ORDER BY {}", order_by.iter().map(|o| o.to_string()).collect::<Vec<_>>().join(", "));
    }
    if let Some(l) = limit {
        text += &format!(" LIMIT {l}");
    }
    bind(&text, schema, params)
}

/// A name as SQL spells it: quoted unless it is a plain identifier.
pub(crate) fn ident(n: &str) -> String {
    if n.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        n.to_string()
    } else {
        format!("\"{}\"", n.replace('"', "\"\""))
    }
}

/// Parses and binds one SELECT statement.
pub fn bind(sql_text: &str, schema: &Schema, params: &Params) -> Result<Select> {
    bind_query(&*parse_query(sql_text)?, schema, params)
}

/// [`bind`] of a parsed query.
pub(crate) fn bind_query(q: &sql::Query, schema: &Schema, params: &Params) -> Result<Select> {
    Binder::new(schema, params).query(q)
}

struct Binder<'a> {
    schema: &'a Schema,
    params: &'a Params,
    /// Graph computations bound so far (`Select::graphs`).
    graphs: RefCell<Vec<GraphQuery>>,
}

impl<'a> Binder<'a> {
    fn new(schema: &'a Schema, params: &'a Params) -> Self {
        Binder { schema, params, graphs: RefCell::new(Vec::new()) }
    }

    fn query(&self, q: &sql::Query) -> Result<Select> {
        let sql::Query { with, body, order_by: _, limit_clause, fetch, locks, for_clause, settings, format_clause, pipe_operators } = q;
        if with.is_some() {
            return Err(err("WITH runs as a compound query (Db::query), not as one SELECT"));
        }
        if let Some(c) = unsupported_query_clause(fetch, locks, for_clause, settings, format_clause, pipe_operators) {
            return Err(err(format!("{c} is not supported")));
        }
        let sql::SetExpr::Select(s) = body.as_ref() else { return Err(err("only simple SELECT is supported")) };
        check_select(s)?;
        if s.from.len() != 1 || !s.from[0].joins.is_empty() {
            return Err(err("exactly one table, no joins"));
        }
        let t = plain_table(&s.from[0].relation, "SELECT")?.to_string();
        if !t.eq_ignore_ascii_case(&self.schema.table) {
            return Err(err(format!("no such table: {t}")));
        }
        if let Some(sql::LimitClause::LimitOffset { limit_by, .. }) = limit_clause {
            if !limit_by.is_empty() {
                return Err(err("LIMIT ... BY is not supported"));
            }
        }
        if let Some(ob) = &q.order_by {
            if ob.interpolate.is_some() || matches!(&ob.kind, sql::OrderByKind::Expressions(es) if es.iter().any(|e| e.with_fill.is_some())) {
                return Err(err("ORDER BY ... WITH FILL and INTERPOLATE are not supported"));
            }
        }
        let filter = match &s.selection {
            Some(e) => self.pred(e)?.three_valued(&|f| self.schema.fields[f].multi).simplify(),
            None => Pred::True,
        };
        let group_by: Vec<usize> = match &s.group_by {
            sql::GroupByExpr::Expressions(es, _) => es.iter().map(|e| self.group_field(e, &s.projection)).collect::<Result<_>>()?,
            _ => return Err(err("GROUP BY ALL is not supported")),
        };
        let mut projection = Vec::new();
        for item in &s.projection {
            let (e, alias) = match item {
                sql::SelectItem::UnnamedExpr(e) => (e, None),
                sql::SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
                sql::SelectItem::Wildcard(_) => {
                    for (i, f) in self.schema.fields.iter().enumerate() {
                        if f.stored && !matches!(f.ty, FieldType::Vector { .. }) {
                            projection.push((f.name.clone(), Projection::Field(i)));
                        }
                    }
                    continue;
                }
                _ => return Err(err("unsupported select item")),
            };
            let p = self.projection(e)?;
            projection.push((alias.unwrap_or_else(|| e.to_string()), p));
        }
        let aggregated = !group_by.is_empty() || projection.iter().any(|(_, p)| matches!(p, Projection::Count | Projection::Agg { .. }));
        if aggregated {
            for (name, p) in &projection {
                match p {
                    Projection::Field(f) if !group_by.contains(f) => {
                        return Err(err(format!("{name} must be in GROUP BY or inside an aggregate (COUNT, SUM, AVG, MIN, MAX)")))
                    }
                    Projection::Score | Projection::Hops(_) => return Err(err("score() and hops() cannot be aggregated")),
                    _ => {}
                }
            }
        }
        let having = match &s.having {
            None => None,
            Some(_) if !aggregated => return Err(err("HAVING needs GROUP BY or aggregates")),
            Some(h) => Some(self.having(h, &projection, &group_by)?),
        };
        let (order, tiebreak) = match &q.order_by {
            None => (None, None),
            Some(ob) => match &ob.kind {
                sql::OrderByKind::Expressions(es) if (1..=2).contains(&es.len()) => {
                    let desc = es[0].options.asc == Some(false);
                    let mut order = if aggregated {
                        self.group_order(&es[0].expr, desc, &projection, &group_by)?
                    } else {
                        self.order(&es[0].expr, desc, &projection)?
                    };
                    // Rows without the column's value come last, either way
                    // (as in Elasticsearch); `NULLS FIRST` puts them first.
                    if es[0].options.nulls_first == Some(true) {
                        match &mut order {
                            OrderBy::Score { score: Score::Column { .. }, nulls_first, .. } => *nulls_first = true,
                            _ => return Err(err("NULLS FIRST applies to ORDER BY a column")),
                        }
                    }
                    let tiebreak = match es.get(1) {
                        None => None,
                        Some(e) => Some(self.tiebreak(&e.expr, e.options.asc == Some(false), &projection, &group_by)?),
                    };
                    (Some(order), tiebreak)
                }
                _ => return Err(err("ORDER BY supports a key and an optional tie-break")),
            },
        };
        let (limit, offset) = match &q.limit_clause {
            None => (None, 0),
            Some(sql::LimitClause::LimitOffset { limit, offset, .. }) => (
                limit.as_ref().map(|e| self.usize_lit(e)).transpose()?,
                offset.as_ref().map(|o| self.usize_lit(&o.value)).transpose()?.unwrap_or(0),
            ),
            Some(sql::LimitClause::OffsetCommaLimit { offset, limit }) => {
                (Some(self.usize_lit(limit)?), self.usize_lit(offset)?)
            }
        };
        let graphs = self.graphs.take();
        let types = projection.iter().map(|(_, p)| self.typed_source(p).and_then(|f| self.schema.fields[f].logical)).collect();
        Ok(Select { projection, filter, order, group_by, having, tiebreak, limit, offset, graphs, types })
    }

    /// The graph predicate `hops()` refers to: the first REACHABLE or PATH.
    fn hops_graph(&self) -> Result<usize> {
        self.graphs
            .borrow()
            .iter()
            .position(|g| !matches!(g.kind, GraphKind::Ppr { .. }))
            .ok_or_else(|| err("hops() needs a REACHABLE or PATH predicate in WHERE"))
    }

    /// Binds `REACHABLE(field, seeds, ...)`, `PATH(field, from, to, ...)` or
    /// `ppr(field, seeds, ...)` and returns its index in `Select::graphs`.
    fn graph(&self, f: &sql::Function, name: &str) -> Result<usize> {
        let (args, named) = self.args_raw(f)?;
        let want = if name == "path" { 3 } else { 2 };
        if args.len() != want {
            return Err(err(match name {
                "path" => "PATH(field, from, to, ...)",
                "ppr" => "ppr(field, seeds, ...)",
                _ => "REACHABLE(field, seeds, ...)",
            }));
        }
        let field = self.typed_field(args[0], |t| matches!(t, FieldType::Edges | FieldType::Vector { .. }), "EDGES or VECTOR")?;
        let vector = matches!(self.schema.fields[field].ty, FieldType::Vector { .. });
        if !vector && !self.schema.fields[field].indexed {
            return Err(err(format!("{} is not indexed", self.schema.fields[field].name)));
        }
        if vector && name == "ppr" {
            return Err(err("ppr() needs an EDGES field"));
        }
        let seeds = self.seeds(args[1])?;
        let mut opts = Opts::new();
        let mut through = None;
        for (k, e) in named {
            if k == "through" {
                let p = self.pred(e)?.three_valued(&|f| self.schema.fields[f].multi).simplify();
                if contains_graph(&p) {
                    return Err(err("a through predicate cannot hold a graph predicate"));
                }
                through = Some(p);
            } else {
                opts.insert(k, self.literal(e)?);
            }
        }
        let allowed: &[&str] = match name {
            "path" => &["max_hops", "dir", "label", "labels", "max_nodes", "k", "min_sim"],
            "ppr" => &["alpha", "eps", "max_pushes", "weighted", "dir", "label", "labels"],
            _ => &["hops", "min_hops", "dir", "label", "labels", "max_nodes", "k", "min_sim"],
        };
        if let Some(k) = opts.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(err(format!("unknown option {k} for {name}")));
        }
        if through.is_some() && name == "ppr" {
            return Err(err("ppr() has no through option"));
        }
        let num = |k: &str, d: f64| -> Result<f64> {
            match opts.get(k) {
                None => Ok(d),
                Some(v) => v.as_f64().ok_or_else(|| err(format!("{k} must be a number"))),
            }
        };
        let count = |k: &str, d: f64| -> Result<u32> {
            let x = num(k, d)?;
            if x < 0.0 || x.fract() != 0.0 || x > 1e6 {
                return Err(err(format!("{k} must be a non-negative integer")));
            }
            Ok(x as u32)
        };
        let dir = match opts.get("dir").map(|v| v.as_str().unwrap_or("")) {
            None => if name == "ppr" { Direction::Both } else { Direction::Out },
            Some("out") => Direction::Out,
            Some("in") => Direction::In,
            Some("both") => Direction::Both,
            Some(d) => return Err(err(format!("dir must be 'out', 'in' or 'both', not {d:?}"))),
        };
        let labels: Vec<String> = ["label", "labels"]
            .iter()
            .filter_map(|k| opts.get(*k))
            .flat_map(|v| v.as_str().unwrap_or("").split(',').map(|l| l.trim().to_string()).collect::<Vec<_>>())
            .filter(|l| !l.is_empty())
            .collect();
        let kind = match name {
            "path" => GraphKind::Path { to: self.seeds(args[2])?, max_hops: count("max_hops", 6.0)?.max(1) },
            "ppr" => {
                let alpha = num("alpha", 0.15)?;
                let eps = num("eps", 1e-4)?;
                if !(alpha > 0.0 && alpha < 1.0) || eps <= 0.0 {
                    return Err(err("ppr needs 0 < alpha < 1 and eps > 0"));
                }
                GraphKind::Ppr { alpha, eps, max_pushes: num("max_pushes", 1e6)? as u64, weighted: num("weighted", 0.0)? != 0.0 }
            }
            _ => {
                let max_hops = count("hops", 1.0)?;
                let min_hops = count("min_hops", 1.0_f64.min(max_hops as f64))?;
                if min_hops > max_hops {
                    return Err(err("min_hops is larger than hops"));
                }
                GraphKind::Reach { min_hops, max_hops }
            }
        };
        let g = GraphQuery {
            field,
            seeds,
            kind,
            dir,
            labels,
            through,
            max_nodes: num("max_nodes", 100_000.0)?.max(1.0) as usize,
            k: count("k", 10.0)?.max(1) as usize,
            min_sim: num("min_sim", f64::NEG_INFINITY)?,
        };
        let mut gs = self.graphs.borrow_mut();
        gs.push(g);
        Ok(gs.len() - 1)
    }

    /// Seeds: a key, a parameter, a tuple or array of keys, or a subquery.
    fn seeds(&self, e: &Expr) -> Result<Seeds> {
        match e {
            Expr::Subquery(q) => Ok(Seeds::Query(Box::new(Binder::new(self.schema, self.params).query(q)?))),
            Expr::Nested(inner) => self.seeds(inner),
            Expr::Tuple(items) => Ok(Seeds::Keys(items.iter().map(|i| self.key_value(i)).collect::<Result<_>>()?)),
            Expr::Array(a) => Ok(Seeds::Keys(a.elem.iter().map(|i| self.key_value(i)).collect::<Result<_>>()?)),
            // A list-valued parameter is a list of keys.
            _ => match self.literal(e).map(as_list) {
                Ok(Ok(xs)) => {
                    let pk = self.schema.primary_key.ok_or_else(|| err("graph queries need a primary key"))?;
                    Ok(Seeds::Keys(xs.into_iter().map(|x| self.coerce(pk, x)).collect::<Result<_>>()?))
                }
                _ => Ok(Seeds::Keys(vec![self.key_value(e)?])),
            },
        }
    }

    /// A literal as a primary-key value.
    fn key_value(&self, e: &Expr) -> Result<Value> {
        let pk = self.schema.primary_key.ok_or_else(|| err("graph queries need a primary key"))?;
        self.field_value(pk, e)
    }

    /// The second ORDER BY key: the primary key, or the (first) group value.
    fn tiebreak(&self, e: &Expr, desc: bool, proj: &[(String, Projection)], group: &[usize]) -> Result<Tiebreak> {
        let field = match literal_usize(e) {
            Some(n) => match proj.get(n.wrapping_sub(1)) {
                Some((_, Projection::Field(f))) => *f,
                _ => return Err(err(format!("ORDER BY ..., {n}: not a column"))),
            },
            None => self.field_of(e)?,
        };
        if group.first() == Some(&field) {
            Ok(Tiebreak::GroupValue { desc })
        } else if Some(field) == self.schema.primary_key && group.is_empty() {
            Ok(Tiebreak::Key { desc })
        } else {
            Err(err("the second ORDER BY key must be the primary key or the first group field"))
        }
    }

    /// A GROUP BY item: a field, or the position of a selected field.
    fn group_field(&self, e: &Expr, items: &[sql::SelectItem]) -> Result<usize> {
        let f = match literal_usize(e) {
            Some(n) => match items.get(n.wrapping_sub(1)) {
                Some(sql::SelectItem::UnnamedExpr(e)) | Some(sql::SelectItem::ExprWithAlias { expr: e, .. }) => self.field_of(e)?,
                _ => return Err(err(format!("GROUP BY {n}: not a selected column"))),
            },
            None => match self.field_of(e) {
                Ok(f) => f,
                // An alias of a selected field.
                Err(e2) => {
                    let alias = match e {
                        Expr::Identifier(id) => &id.value,
                        _ => return Err(e2),
                    };
                    let found = items.iter().find_map(|i| match i {
                        sql::SelectItem::ExprWithAlias { expr, alias: a } if a.value.eq_ignore_ascii_case(alias) => Some(expr),
                        _ => None,
                    });
                    self.field_of(found.ok_or(e2)?)?
                }
            },
        };
        let fld = &self.schema.fields[f];
        let ok = match fld.ty {
            FieldType::Keyword | FieldType::Int => true,
            FieldType::Float => !fld.multi,
            _ => false,
        };
        if !ok {
            return Err(err(format!("GROUP BY {}: group by KEYWORD, INT or FLOAT fields", fld.name)));
        }
        if !fld.indexed {
            return Err(err(format!("GROUP BY {}: the field is not indexed", fld.name)));
        }
        Ok(f)
    }

    fn projection(&self, e: &Expr) -> Result<Projection> {
        match e {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => Ok(Projection::Field(self.field_of(e)?)),
            Expr::Function(f) => match fname(f).as_str() {
                "score" => Ok(Projection::Score),
                "hops" => Ok(Projection::Hops(self.hops_graph()?)),
                "count" | "sum" | "avg" | "min" | "max" => self.aggregate(f),
                n => Err(err(format!("unsupported function in SELECT: {n}"))),
            },
            _ => Err(err(format!("unsupported select expression: {e}"))),
        }
    }

    /// `COUNT(*)`, `COUNT([DISTINCT] f)`, `SUM(f)`, `AVG(f)`, `MIN(f)`, `MAX(f)`.
    fn aggregate(&self, f: &sql::Function) -> Result<Projection> {
        let name = fname(f);
        let distinct = matches!(&f.args, FunctionArguments::List(l) if l.duplicate_treatment == Some(sql::DuplicateTreatment::Distinct));
        let (args, _) = self.args_raw(f)?;
        let star = args.is_empty() || (name == "count" && args.len() == 1 && literal_usize(args[0]).is_some());
        if star {
            return if name == "count" && !distinct { Ok(Projection::Count) } else { Err(err(format!("{}(field)", name.to_uppercase()))) };
        }
        let [arg] = args.as_slice() else { return Err(err(format!("{} takes one field", name.to_uppercase()))) };
        let field = self.field_of(arg)?;
        let fld = &self.schema.fields[field];
        let numeric = matches!(fld.ty, FieldType::Int | FieldType::Float);
        let func = match name.as_str() {
            "count" if distinct => AggFn::CountDistinct,
            "count" => AggFn::Count,
            "sum" => AggFn::Sum,
            "avg" => AggFn::Avg,
            "min" => AggFn::Min,
            _ => AggFn::Max,
        };
        match func {
            AggFn::Sum | AggFn::Avg if fld.logical == Some(Logical::Timestamp) => {
                return Err(err(format!("{}({}): times do not add up; a TIMESTAMP takes MIN, MAX and COUNT", name.to_uppercase(), fld.name)))
            }
            AggFn::Sum | AggFn::Avg if !numeric => return Err(err(format!("{}({}): not a numeric field", name.to_uppercase(), fld.name))),
            AggFn::Min | AggFn::Max | AggFn::CountDistinct if !(numeric || fld.ty == FieldType::Keyword) => {
                return Err(err(format!("{}({}): needs a KEYWORD, INT or FLOAT field", name.to_uppercase(), fld.name)))
            }
            AggFn::Count if !(numeric || fld.ty == FieldType::Keyword) => return Err(err(format!("COUNT({}): count a KEYWORD, INT or FLOAT field, or COUNT(*)", fld.name))),
            _ => {}
        }
        if distinct && func != AggFn::CountDistinct {
            return Err(err("DISTINCT works in COUNT only"));
        }
        if !fld.indexed {
            return Err(err(format!("{}: aggregates read the field's column, and it is not indexed", fld.name)));
        }
        Ok(Projection::Agg { func, field })
    }

    /// An aggregate or group value named in HAVING or ORDER BY: a function
    /// call, a selected column's alias or position, or a group field.
    fn group_value(&self, e: &Expr, proj: &[(String, Projection)], group: &[usize]) -> Result<Projection> {
        if let Some(n) = literal_usize(e) {
            return proj.get(n.wrapping_sub(1)).map(|(_, p)| p.clone()).ok_or_else(|| err(format!("{n}: no such column")));
        }
        match e {
            Expr::Nested(inner) => self.group_value(inner, proj, group),
            Expr::Function(f) if matches!(fname(f).as_str(), "count" | "sum" | "avg" | "min" | "max") => self.aggregate(f),
            Expr::Identifier(id) if proj.iter().any(|(n, p)| n.eq_ignore_ascii_case(&id.value) && !matches!(p, Projection::Field(_))) => {
                Ok(proj.iter().find(|(n, _)| n.eq_ignore_ascii_case(&id.value)).unwrap().1.clone())
            }
            _ => {
                let f = self.field_of(e)?;
                if group.contains(&f) {
                    Ok(Projection::Field(f))
                } else {
                    Err(err(format!("{e}: use a group field or an aggregate")))
                }
            }
        }
    }

    fn group_order(&self, e: &Expr, desc: bool, proj: &[(String, Projection)], group: &[usize]) -> Result<OrderBy> {
        Ok(match self.group_value(e, proj, group)? {
            Projection::Count => OrderBy::Count { desc },
            Projection::Field(f) if group.first() == Some(&f) => OrderBy::GroupValue { desc },
            what => OrderBy::Agg { what, desc },
        })
    }

    fn having(&self, e: &Expr, proj: &[(String, Projection)], group: &[usize]) -> Result<Having> {
        use BinaryOperator::*;
        Ok(match e {
            Expr::Nested(inner) => self.having(inner, proj, group)?,
            Expr::BinaryOp { left, op: And, right } => Having::And(vec![self.having(left, proj, group)?, self.having(right, proj, group)?]),
            Expr::BinaryOp { left, op: Or, right } => Having::Or(vec![self.having(left, proj, group)?, self.having(right, proj, group)?]),
            Expr::UnaryOp { op: sql::UnaryOperator::Not, expr } => Having::Not(Box::new(self.having(expr, proj, group)?)),
            Expr::Between { expr, negated, low, high } => {
                let what = self.group_value(expr, proj, group)?;
                let h = Having::Range { lo: self.having_bound(&what, low)?, hi: self.having_bound(&what, high)?, what };
                if *negated { Having::Not(Box::new(h)) } else { h }
            }
            Expr::BinaryOp { left, op, right } => {
                let (what, lit, op) = match self.group_value(left, proj, group) {
                    Ok(w) => (w, right, op.clone()),
                    Err(_) => {
                        let flipped = match op {
                            Lt => Gt,
                            LtEq => GtEq,
                            Gt => Lt,
                            GtEq => LtEq,
                            o => o.clone(),
                        };
                        (self.group_value(right, proj, group)?, left, flipped)
                    }
                };
                let v = match self.typed_source(&what) {
                    Some(f) => self.coerce(f, self.literal(lit)?)?,
                    None => self.literal(lit)?,
                };
                let Some(x) = v.as_f64() else {
                    return match op {
                        Eq => Ok(Having::Eq { what, value: v }),
                        NotEq => Ok(Having::Not(Box::new(Having::Eq { what, value: v }))),
                        _ => Err(err("HAVING compares strings with = and != only")),
                    };
                };
                let (inf, ninf) = (f64::INFINITY, f64::NEG_INFINITY);
                let range = |lo, hi| Having::Range { what: what.clone(), lo, hi };
                match op {
                    Eq => range(x, x),
                    NotEq => Having::Not(Box::new(range(x, x))),
                    Gt => range(next_up(x), inf),
                    GtEq => range(x, inf),
                    Lt => range(ninf, next_down(x)),
                    LtEq => range(ninf, x),
                    o => return Err(err(format!("unsupported operator {o} in HAVING"))),
                }
            }
            _ => return Err(err(format!("unsupported HAVING condition: {e}"))),
        })
    }

    /// The BOOL or TIMESTAMP field whose values a group value or aggregate
    /// holds: the field itself, or its MIN or MAX.
    fn typed_source(&self, p: &Projection) -> Option<usize> {
        match p {
            Projection::Field(f) | Projection::Agg { func: AggFn::Min | AggFn::Max, field: f } => Some(*f).filter(|&f| self.schema.fields[f].logical.is_some()),
            _ => None,
        }
    }

    /// A bound of `BETWEEN` in HAVING: a number, or a value of the type of
    /// a BOOL or TIMESTAMP field (as its stored integer).
    fn having_bound(&self, what: &Projection, e: &Expr) -> Result<f64> {
        match self.typed_source(what) {
            Some(f) => Ok(self.field_value(f, e)?.as_f64().unwrap_or(f64::NAN)),
            None => self.num_lit(e),
        }
    }

    fn order(&self, e: &Expr, desc: bool, proj: &[(String, Projection)]) -> Result<OrderBy> {
        // Positional `ORDER BY 2` addresses a selected column.
        if let Some(n) = literal_usize(e) {
            let (_, p) = proj.get(n.wrapping_sub(1)).ok_or_else(|| err(format!("ORDER BY {n}: no such column")))?;
            return match p {
                Projection::Field(f) => Ok(OrderBy::Score { score: Score::Column { field: *f }, desc, nulls_first: false }),
                Projection::Score => Err(err("ORDER BY score(): order by the score expression itself")),
                Projection::Hops(g) => Ok(OrderBy::Score { score: Score::Hops { graph: *g, far_first: desc }, desc: true, nulls_first: false }),
                Projection::Count | Projection::Agg { .. } => Err(err("ORDER BY an aggregate needs GROUP BY")),
            };
        }
        // `ORDER BY hops()` is nearest first; ranking is always best first.
        if let Expr::Function(f) = e {
            if fname(f) == "hops" {
                return Ok(OrderBy::Score { score: Score::Hops { graph: self.hops_graph()?, far_first: desc }, desc: true, nulls_first: false });
            }
        }
        // `ORDER BY distance(..) ASC` ranks like `similarity DESC`.
        if let Expr::Function(f) = e {
            if fname(f) == "distance" {
                return Ok(OrderBy::Score { score: self.score(e)?, desc: !desc, nulls_first: false });
            }
        }
        Ok(OrderBy::Score { score: self.score(e)?, desc, nulls_first: false })
    }

    fn score(&self, e: &Expr) -> Result<Score> {
        match e {
            Expr::Nested(inner) => self.score(inner),
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                let f = self.field_of(e)?;
                if self.schema.fields[f].multi {
                    return Err(err(format!("cannot order by multi-valued field {}", self.schema.fields[f].name)));
                }
                match self.schema.fields[f].ty {
                    FieldType::Int | FieldType::Float => Ok(Score::Column { field: f }),
                    _ => Err(err("ORDER BY a column needs a numeric field")),
                }
            }
            Expr::BinaryOp { left, op: BinaryOperator::Plus, right } => {
                Ok(Score::Weighted([self.weighted(left, 1.0)?, self.weighted(right, 1.0)?].concat()))
            }
            Expr::BinaryOp { left, op: BinaryOperator::Minus, right } => {
                Ok(Score::Weighted([self.weighted(left, 1.0)?, self.weighted(right, -1.0)?].concat()))
            }
            Expr::BinaryOp { op: BinaryOperator::Multiply, .. } => {
                let mut terms = self.weighted(e, 1.0)?;
                match terms.as_slice() {
                    [(w, Score::Product(_))] if *w == 1.0 => Ok(terms.pop().unwrap().1),
                    _ => Ok(Score::Weighted(terms)),
                }
            }
            Expr::Function(f) => {
                let (args, opts) = self.args(f)?;
                match fname(f).as_str() {
                    "bm25" => {
                        let [field, text] = args.as_slice() else { return Err(err("bm25(field, text)")) };
                        let field = self.typed_field(field, |t| matches!(t, FieldType::Text { .. }), "TEXT")?;
                        let text = self.literal(text)?.as_str().ok_or_else(|| err("bm25 text must be a string"))?.to_string();
                        Expand::from_opts(&opts).map_err(err)?;
                        Ok(Score::Bm25 { field, text, opts })
                    }
                    "similarity" | "distance" => {
                        let [field, v] = args.as_slice() else { return Err(err("similarity(field, vector)")) };
                        let field = self.typed_field(field, |t| matches!(t, FieldType::Vector { .. } | FieldType::Sparse), "VECTOR or SPARSE")?;
                        if self.schema.fields[field].ty == FieldType::Sparse {
                            if fname(f) == "distance" {
                                return Err(err("a SPARSE field takes similarity(...), not distance(...)"));
                            }
                            let terms = crate::text::sparse::parse(&self.literal(v)?).map_err(|e| err(format!("similarity({}, ..): {e}", self.schema.fields[field].name)))?;
                            return Ok(Score::Sparse { field, terms, opts });
                        }
                        let FieldType::Vector { dim, .. } = self.schema.fields[field].ty else { unreachable!() };
                        let vector = self.vector(v)?;
                        if vector.len() != dim {
                            return Err(err(format!("vector has {} dimensions, field has {dim}", vector.len())));
                        }
                        Ok(Score::Similarity { field, vector, opts })
                    }
                    "rrf" => {
                        if args.len() < 2 {
                            return Err(err("rrf needs at least two scores"));
                        }
                        let inputs = args.iter().map(|a| self.score(a)).collect::<Result<Vec<_>>>()?;
                        let num = |k: &str, d: f64| opts.get(k).and_then(|v| v.as_f64()).unwrap_or(d);
                        Ok(Score::Rrf { inputs, k: num("k", 60.0), depth: num("depth", 100.0) as usize })
                    }
                    "ppr" => Ok(Score::Ppr { graph: self.graph(f, "ppr")? }),
                    "decay" => self.decay(f),
                    n => Err(err(format!("unknown score function {n}"))),
                }
            }
            _ => Err(err(format!("unsupported ORDER BY expression: {e}"))),
        }
    }

    /// Terms of a weighted sum, each multiplied by `w`.
    fn weighted(&self, e: &Expr, w: f64) -> Result<Vec<(f64, Score)>> {
        match e {
            Expr::Nested(inner) => self.weighted(inner, w),
            Expr::BinaryOp { left, op: BinaryOperator::Plus, right } => {
                Ok([self.weighted(left, w)?, self.weighted(right, w)?].concat())
            }
            Expr::BinaryOp { left, op: BinaryOperator::Minus, right } => {
                Ok([self.weighted(left, w)?, self.weighted(right, -w)?].concat())
            }
            Expr::BinaryOp { left, op: BinaryOperator::Multiply, right } => {
                if let Some(c) = self.number(left) {
                    self.weighted(right, w * c)
                } else if let Some(c) = self.number(right) {
                    self.weighted(left, w * c)
                } else {
                    Ok(vec![(w, Score::Product(self.factors(e)?))])
                }
            }
            _ => Ok(vec![(w, self.score(e)?)]),
        }
    }

    /// The factors of a product of scores, `a * b * ...`.
    fn factors(&self, e: &Expr) -> Result<Vec<Score>> {
        match e {
            Expr::Nested(inner) => self.factors(inner),
            Expr::BinaryOp { left, op: BinaryOperator::Multiply, right } => {
                if self.number(left).is_some() || self.number(right).is_some() {
                    return Err(err("a product of scores takes no constant factors (they do not change the order)"));
                }
                Ok([self.factors(left)?, self.factors(right)?].concat())
            }
            _ => {
                let s = self.score(e)?;
                match s {
                    Score::Bm25 { .. } | Score::Similarity { .. } | Score::Sparse { .. } | Score::Column { .. } | Score::Decay { .. } | Score::Ppr { .. } | Score::Hops { .. } => Ok(vec![s]),
                    _ => Err(err("a product multiplies bm25, similarity, ppr, a column or decay, not sums or fusions")),
                }
            }
        }
    }

    /// `decay(field, origin, scale, offset => 0, decay => 0.5, shape =>
    /// 'exp' | 'gauss' | 'linear')`; the origin may be `now()` (seconds
    /// since the Unix epoch). On a TIMESTAMP field the origin is `now()` or
    /// a time, and the scale and offset are seconds.
    fn decay(&self, f: &sql::Function) -> Result<Score> {
        let (args, opts) = self.args(f)?;
        let usage = "decay(field, origin, scale, offset => .., decay => .., shape => 'exp' | 'gauss' | 'linear')";
        let [field, origin, scale] = args.as_slice() else { return Err(err(usage)) };
        let field = self.numeric_field(field)?;
        if self.schema.fields[field].multi {
            return Err(err(format!("cannot decay multi-valued field {}", self.schema.fields[field].name)));
        }
        // A TIMESTAMP's origin is a time, and its scale and offset are
        // seconds, as now() is for a number.
        let time = self.schema.fields[field].logical == Some(Logical::Timestamp);
        let unit = if time { 1e6 } else { 1.0 };
        let now = || crate::timestamp::now() as f64 / 1e6 * unit;
        let origin = match origin {
            Expr::Function(g) if fname(g) == "now" => now(),
            e => match self.literal(e)? {
                Value::Str(s) if s.eq_ignore_ascii_case("now") => now(),
                v if time => self.coerce(field, v)?.as_f64().unwrap_or(0.0),
                v => v.as_f64().ok_or_else(|| err("decay origin must be a number or now()"))?,
            },
        };
        let scale = self.num_lit(scale)? * unit;
        let num = |k: &str, d: f64| opts.get(k).map_or(Ok(d), |v| v.as_f64().ok_or_else(|| err(format!("decay {k} must be a number"))));
        let (offset, decay) = (num("offset", 0.0)? * unit, num("decay", 0.5)?);
        if scale <= 0.0 || offset < 0.0 || !(decay > 0.0 && decay < 1.0) {
            return Err(err("decay needs scale > 0, offset >= 0 and 0 < decay < 1"));
        }
        let shape = match opts.get("shape").map(|v| v.as_str().unwrap_or("").to_ascii_lowercase()).as_deref() {
            None | Some("exp") => DecayShape::Exp,
            Some("gauss") => DecayShape::Gauss,
            Some("linear") => DecayShape::Linear,
            Some(s) => return Err(err(format!("unknown decay shape {s:?}: exp, gauss or linear"))),
        };
        Ok(Score::Decay { field, decay: Decay { origin, scale, offset, decay, shape }, opts })
    }

    fn pred(&self, e: &Expr) -> Result<Pred> {
        Ok(match e {
            Expr::Nested(inner) => self.pred(inner)?,
            Expr::BinaryOp { left, op: BinaryOperator::And, right } => Pred::And(vec![self.pred(left)?, self.pred(right)?]),
            Expr::BinaryOp { left, op: BinaryOperator::Or, right } => Pred::Or(vec![self.pred(left)?, self.pred(right)?]),
            Expr::UnaryOp { op: sql::UnaryOperator::Not, expr } => Pred::Not(Box::new(self.pred(expr)?)),
            Expr::BinaryOp { left, op, right } => self.comparison(left, op, right)?,
            Expr::Between { expr, negated, low, high } => {
                let f = self.numeric_field(expr)?;
                let p = if self.schema.fields[f].ty == FieldType::Int {
                    int_range(f, ceil(&self.int_bound(f, low)?), floor(&self.int_bound(f, high)?))
                } else {
                    Pred::Range { field: f, lo: self.num_lit(low)?, hi: self.num_lit(high)? }
                };
                if *negated { Pred::Not(Box::new(p)) } else { p }
            }
            Expr::IsNull(x) | Expr::IsNotNull(x) => {
                let f = self.field_of(x)?;
                let field = &self.schema.fields[f];
                if !matches!(field.ty, FieldType::Int | FieldType::Float | FieldType::Keyword) || !field.indexed {
                    return Err(err(format!("IS NULL works on indexed INT, FLOAT and KEYWORD fields, not {}", field.name)));
                }
                let p = Pred::Present { field: f };
                if matches!(e, Expr::IsNull(_)) { Pred::Not(Box::new(p)) } else { p }
            }
            Expr::InList { expr, list, negated } => {
                let f = self.field_of(expr)?;
                if self.schema.fields[f].ty == FieldType::Edges {
                    return Err(err(format!("{} is an EDGES field: use REACHABLE({}, keys) for rows linked to keys", self.schema.fields[f].name, self.schema.fields[f].name)));
                }
                // A list-valued parameter stands for its values (`IN (:ids)`).
                let mut values = Vec::with_capacity(list.len());
                for v in list {
                    match as_list(self.literal(v)?) {
                        Ok(xs) => {
                            for x in xs {
                                values.push(self.coerce(f, x)?);
                            }
                        }
                        Err(x) => values.push(self.coerce(f, x)?),
                    }
                }
                let p = if values.is_empty() { Pred::False } else { Pred::In { field: f, values } };
                if *negated { Pred::Not(Box::new(p)) } else { p }
            }
            Expr::Function(func) if fname(func) == "match" => {
                let (args, opts) = self.args(func)?;
                let [field, q] = args.as_slice() else { return Err(err("MATCH(field, query, synonyms => 0 | 1, fuzzy => 0 | 1 | 2 | 'auto')")) };
                let field = self.typed_field(field, |t| matches!(t, FieldType::Text { .. }), "TEXT")?;
                if !self.schema.fields[field].indexed {
                    return Err(err(format!("{} is not indexed", self.schema.fields[field].name)));
                }
                if let Some(k) = opts.keys().find(|k| !matches!(k.as_str(), "synonyms" | "fuzzy")) {
                    return Err(err(format!("unknown option {k} for MATCH: synonyms, fuzzy")));
                }
                let expand = Expand::from_opts(&opts).map_err(err)?;
                let query = self.literal(q)?.as_str().ok_or_else(|| err("MATCH query must be a string"))?.to_string();
                crate::text::parse_match(&query)?;
                Pred::Match { field, query, expand }
            }
            Expr::Function(func) if matches!(fname(func).as_str(), "reachable" | "path") => Pred::Graph { id: self.graph(func, &fname(func))? },
            Expr::Value(v) if matches!(v.value, sql::Value::Boolean(_)) => {
                if matches!(v.value, sql::Value::Boolean(true)) { Pred::True } else { Pred::False }
            }
            e if self.bool_field(e).is_some() => int_range(self.bool_field(e).unwrap(), 1, 1),
            Expr::IsTrue(x) | Expr::IsFalse(x) | Expr::IsNotTrue(x) | Expr::IsNotFalse(x) => {
                let f = self.bool_field(x).ok_or_else(|| err(format!("{e}: IS TRUE and IS FALSE test a BOOL field")))?;
                let want = matches!(e, Expr::IsTrue(_) | Expr::IsNotFalse(_)) as i128;
                match e {
                    Expr::IsTrue(_) | Expr::IsFalse(_) => int_range(f, want, want),
                    // Unlike `NOT done`, `done IS NOT TRUE` holds for NULL.
                    _ => Pred::Or(vec![Pred::Not(Box::new(Pred::Present { field: f })), int_range(f, want, want)]),
                }
            }
            _ => return Err(err(format!("unsupported predicate: {e}"))),
        })
    }

    fn comparison(&self, left: &Expr, op: &BinaryOperator, right: &Expr) -> Result<Pred> {
        use BinaryOperator::*;
        // Normalise to `field op literal`.
        let (field_e, lit_e, op) = if self.field_of(left).is_ok() {
            (left, right, op.clone())
        } else {
            let flipped = match op {
                Lt => Gt,
                LtEq => GtEq,
                Gt => Lt,
                GtEq => LtEq,
                o => o.clone(),
            };
            (right, left, flipped)
        };
        let f = self.field_of(field_e)?;
        let ty = &self.schema.fields[f].ty;
        if matches!(self.literal(lit_e), Ok(Value::Null)) {
            return Err(err(format!("{e} compares with NULL, which is never true: use IS NULL or IS NOT NULL", e = Expr::BinaryOp { left: Box::new(left.clone()), op: op.clone(), right: Box::new(right.clone()) })));
        }
        if matches!(ty, FieldType::Keyword) {
            let v = self.field_value(f, lit_e)?;
            return match op {
                Eq => Ok(Pred::In { field: f, values: vec![v] }),
                NotEq => Ok(Pred::Not(Box::new(Pred::In { field: f, values: vec![v] }))),
                _ => Err(err("keyword fields support =, != and IN")),
            };
        }
        if matches!(ty, FieldType::Int) {
            // Integers compare exactly, in 128 bits so that the bounds of
            // `> i64::MAX` or `< 1e30` need no special cases.
            let v = self.int_bound(f, lit_e)?;
            let (min, max) = (i64::MIN as i128, i64::MAX as i128);
            return Ok(match op {
                Eq => int_range(f, ceil(&v), floor(&v)),
                NotEq => Pred::Not(Box::new(int_range(f, ceil(&v), floor(&v)))),
                Gt => int_range(f, floor(&v) + 1, max),
                GtEq => int_range(f, ceil(&v), max),
                Lt => int_range(f, min, ceil(&v) - 1),
                LtEq => int_range(f, min, floor(&v)),
                o => return Err(err(format!("unsupported operator {o}"))),
            });
        }
        if !matches!(ty, FieldType::Float) {
            return Err(err(format!("cannot compare field {}", self.schema.fields[f].name)));
        }
        let x = self.num_lit(lit_e)?;
        let (inf, ninf) = (f64::INFINITY, f64::NEG_INFINITY);
        let range = |lo, hi| Pred::Range { field: f, lo, hi };
        Ok(match op {
            Eq => range(x, x),
            NotEq => Pred::Not(Box::new(range(x, x))),
            Gt => range(next_up(x), inf),
            GtEq => range(x, inf),
            Lt => range(ninf, next_down(x)),
            LtEq => range(ninf, x),
            o => return Err(err(format!("unsupported operator {o}"))),
        })
    }

    /// Positional and named (`name => value`) arguments of a function call.
    fn args<'e>(&self, f: &'e sql::Function) -> Result<(Vec<&'e Expr>, Opts)> {
        let (pos, named) = self.args_raw(f)?;
        let mut opts = Opts::new();
        for (k, e) in named {
            opts.insert(k, self.literal(e)?);
        }
        Ok((pos, opts))
    }

    /// Positional arguments and unevaluated named ones.
    fn args_raw<'e>(&self, f: &'e sql::Function) -> Result<(Vec<&'e Expr>, Vec<(String, &'e Expr)>)> {
        let FunctionArguments::List(list) = &f.args else {
            return Ok((Vec::new(), Vec::new()));
        };
        let (mut pos, mut named) = (Vec::new(), Vec::new());
        for a in &list.args {
            match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => pos.push(e),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => {}
                FunctionArg::Named { name, arg: FunctionArgExpr::Expr(e), .. } => named.push((name.value.to_ascii_lowercase(), e)),
                _ => return Err(err(format!("unsupported argument {a}"))),
            }
        }
        Ok((pos, named))
    }

    fn field_of(&self, e: &Expr) -> Result<usize> {
        let name = match e {
            Expr::Identifier(id) => &id.value,
            Expr::CompoundIdentifier(ids) => &ids.last().unwrap().value,
            _ => return Err(err(format!("expected a column, got {e}"))),
        };
        self.schema.field_index(name).ok_or_else(|| err(format!("no such column: {name}")))
    }

    fn typed_field(&self, e: &Expr, ok: impl Fn(&FieldType) -> bool, what: &str) -> Result<usize> {
        let f = self.field_of(e)?;
        if ok(&self.schema.fields[f].ty) { Ok(f) } else { Err(err(format!("{} is not a {what} field", self.schema.fields[f].name))) }
    }

    fn numeric_field(&self, e: &Expr) -> Result<usize> {
        self.typed_field(e, |t| matches!(t, FieldType::Int | FieldType::Float), "numeric")
    }

    fn literal(&self, e: &Expr) -> Result<Value> {
        match e {
            Expr::Value(v) => match &v.value {
                sql::Value::Number(s, _) => {
                    Ok(s.parse::<i64>().map(Value::Int).or_else(|_| s.parse::<f64>().map(Value::Float)).map_err(|_| err("bad number"))?)
                }
                sql::Value::SingleQuotedString(s) | sql::Value::DoubleQuotedString(s) => Ok(Value::Str(s.clone())),
                sql::Value::Boolean(b) => Ok(Value::Bool(*b)),
                sql::Value::Null => Ok(Value::Null),
                sql::Value::Placeholder(p) => Ok(self.params.get(p)?.clone()),
                other => Err(err(format!("unsupported literal {other}"))),
            },
            Expr::UnaryOp { op: sql::UnaryOperator::Minus, expr } => match self.literal(expr)? {
                // -i64::MIN is the one integer without a negation.
                Value::Int(x) => Ok(x.checked_neg().map_or(Value::Float(-(x as f64)), Value::Int)),
                Value::Float(x) => Ok(Value::Float(-x)),
                _ => Err(err("cannot negate a non-number")),
            },
            // "x" reads as a string, as in SQLite and MySQL; an unquoted
            // name is a column, which is no value.
            Expr::Identifier(id) if id.quote_style == Some('"') => Ok(Value::Str(id.value.clone())),
            Expr::Identifier(id) if self.schema.field_index(&id.value).is_some() => {
                Err(err(format!("{} is a column, and a value is expected here (a string is quoted: '{}')", id.value, id.value)))
            }
            Expr::Identifier(id) => Err(err(format!("no such column: {} (a string is quoted: '{}')", id.value, id.value))),
            _ => Err(err(format!("expected a literal or parameter, got {e}"))),
        }
    }

    fn number(&self, e: &Expr) -> Option<f64> {
        if self.field_of(e).is_ok() {
            return None;
        }
        self.literal(e).ok()?.as_f64()
    }

    fn num_lit(&self, e: &Expr) -> Result<f64> {
        self.number(e).ok_or_else(|| err(format!("expected a number, got {e}")))
    }

    /// A finite numeric literal or parameter, an integer kept exact.
    fn num_literal(&self, e: &Expr) -> Result<Value> {
        if self.field_of(e).is_err() {
            match self.literal(e) {
                Ok(v @ Value::Int(_)) => return Ok(v),
                Ok(Value::Float(x)) if x.is_finite() => return Ok(Value::Float(x)),
                _ => {}
            }
        }
        Err(err(format!("expected a number, got {e}")))
    }

    fn usize_lit(&self, e: &Expr) -> Result<usize> {
        let x = self.num_lit(e)?;
        if x < 0.0 || x.fract() != 0.0 {
            return Err(err("LIMIT/OFFSET must be non-negative integers"));
        }
        Ok(x as usize)
    }

    /// A literal coerced to field `f`'s type.
    fn field_value(&self, f: usize, e: &Expr) -> Result<Value> {
        self.coerce(f, self.literal(e)?)
    }

    /// A value coerced to field `f`'s type, in the form its column holds
    /// ([`crate::schema::Field::store`]).
    fn coerce(&self, f: usize, v: Value) -> Result<Value> {
        let field = &self.schema.fields[f];
        if let Some(l) = field.logical {
            // In SQL a time is text or a timestamp parameter; a bare
            // number would be microseconds nobody meant.
            if l == Logical::Timestamp && matches!(v, Value::Int(_) | Value::Float(_)) {
                return Err(err(format!("{} is a TIMESTAMP: give it text such as '2026-10-01T12:30:00Z', not the number {v:?}", field.name)));
            }
            return field.store(v).map_err(|e| match e {
                Error::Schema(m) => err(m),
                e => e,
            });
        }
        match (&field.ty, v) {
            (FieldType::Keyword, Value::Str(s)) => Ok(Value::Str(s)),
            (FieldType::Keyword, Value::Int(x)) => Ok(Value::Str(x.to_string())),
            (FieldType::Int, Value::Int(x)) => Ok(Value::Int(x)),
            (FieldType::Int, Value::Bool(b)) => Ok(Value::Int(b as i64)),
            (FieldType::Float, v @ (Value::Int(_) | Value::Float(_) | Value::Bool(_))) => Ok(Value::Float(v.as_f64().unwrap())),
            (t, v) => Err(err(format!("value {v:?} does not fit a {t:?} field"))),
        }
    }

    /// A bound of an INT field's range: a number, or for a BOOL or
    /// TIMESTAMP field a value of its type, as the integer it is stored as.
    fn int_bound(&self, f: usize, e: &Expr) -> Result<Value> {
        if self.schema.fields[f].logical.is_some() {
            self.field_value(f, e)
        } else {
            self.num_literal(e)
        }
    }

    /// A BOOL field named alone (`WHERE done`, `NOT done`).
    fn bool_field(&self, e: &Expr) -> Option<usize> {
        self.field_of(e).ok().filter(|&f| self.schema.fields[f].logical == Some(Logical::Bool))
    }

    /// A value written to field `f`: a literal or parameter of its type,
    /// `ARRAY[...]` for lists and vectors.
    fn write_value(&self, f: usize, e: &Expr) -> Result<Value> {
        let field = &self.schema.fields[f];
        if let Expr::Array(a) = e {
            return match &field.ty {
                FieldType::Vector { .. } => {
                    let xs: Vec<f32> = a.elem.iter().map(|x| self.num_lit(x).map(|v| v as f32)).collect::<Result<_>>()?;
                    Ok(Value::Vector(xs.into()))
                }
                _ if field.multi => Ok(Value::List(a.elem.iter().map(|x| self.field_value(f, x)).collect::<Result<_>>()?)),
                _ => Err(err(format!("{} does not take a list", field.name))),
            };
        }
        let v = self.literal(e)?;
        Ok(match (&field.ty, v) {
            (_, Value::Null) => Value::Null,
            (FieldType::Vector { .. }, _) => Value::Vector(self.vector(e)?),
            (_, v @ Value::List(_)) if field.multi => v,
            (FieldType::Text { .. } | FieldType::Edges, v @ Value::Str(_)) => v,
            (FieldType::Sparse, v @ Value::Str(_)) => {
                crate::text::sparse::parse(&v)?;
                v
            }
            (FieldType::Keyword | FieldType::Int | FieldType::Float, _) if field.multi => Value::List(vec![self.field_value(f, e)?]),
            _ => self.field_value(f, e)?,
        })
    }

    /// `SET f = value, g = g + 1, ...`; with `excluded`, as in `ON CONFLICT
    /// DO UPDATE`, also `h = excluded.h` (the value the INSERT proposed).
    fn assignments(&self, assignments: &[sql::Assignment], excluded: bool) -> Result<Vec<(usize, Assign)>> {
        let mut sets: Vec<(usize, Assign)> = Vec::new();
        for a in assignments {
            let sql::AssignmentTarget::ColumnName(name) = &a.target else { return Err(err("SET one column at a time")) };
            let f = self.schema.field_index(&name.to_string()).ok_or_else(|| err(format!("no such column: {name}")))?;
            let fld = &self.schema.fields[f];
            if Some(f) == self.schema.primary_key {
                return Err(err("the primary key cannot be updated; delete and insert instead"));
            }
            if sets.iter().any(|(g, _)| *g == f) {
                return Err(err(format!("{} is set twice", fld.name)));
            }
            let assign = match &a.value {
                Expr::BinaryOp { left, op: op @ (BinaryOperator::Plus | BinaryOperator::Minus), right } if self.field_of(left).ok() == Some(f) => {
                    if !matches!(fld.ty, FieldType::Int | FieldType::Float) || fld.logical.is_some() {
                        return Err(err(format!("{} is not numeric", fld.name)));
                    }
                    if fld.multi {
                        return Err(err(format!("{} holds a list: SET it to a list", fld.name)));
                    }
                    Assign::Add(match (self.num_literal(right)?, op) {
                        (x, BinaryOperator::Plus) => x,
                        (Value::Int(x), _) => x.checked_neg().map_or(Value::Float(-(x as f64)), Value::Int),
                        (x, _) => Value::Float(-x.as_f64().unwrap_or(0.0)),
                    })
                }
                Expr::CompoundIdentifier(ids) if excluded && ids.len() == 2 && ids[0].value.eq_ignore_ascii_case("excluded") => {
                    let g = self.schema.field_index(&ids[1].value).ok_or_else(|| err(format!("no such column: {}", ids[1].value)))?;
                    let src = &self.schema.fields[g];
                    if src.ty != fld.ty || src.multi != fld.multi || src.logical != fld.logical {
                        return Err(err(format!("{} = excluded.{}: the columns have different types", fld.name, src.name)));
                    }
                    Assign::Excluded(g)
                }
                e => Assign::Value(self.write_value(f, e)?),
            };
            sets.push((f, assign));
        }
        Ok(sets)
    }

    fn vector(&self, e: &Expr) -> Result<Arc<[f32]>> {
        let v: Arc<[f32]> = match self.literal(e)? {
            Value::Vector(v) => v,
            Value::Str(s) => {
                let v: Vec<f32> = serde_json::from_str(&s).map_err(|_| err("vector literal must be a JSON array"))?;
                v.into()
            }
            // Clients send a list of whole numbers as a list of integers.
            Value::List(xs) if !xs.is_empty() && xs.iter().all(|x| matches!(x, Value::Int(_) | Value::Float(_))) => {
                xs.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect::<Vec<_>>().into()
            }
            v => return Err(err(format!("expected a vector, got {v:?}"))),
        };
        if v.iter().any(|x| !x.is_finite()) {
            return Err(err("a vector's components must be finite numbers"));
        }
        Ok(v)
    }
}

/// Refuses the clauses of a SELECT that the binder does not handle, so that
/// none is silently ignored.
fn check_select(s: &sql::Select) -> Result<()> {
    let sql::Select {
        select_token: _,
        distinct,
        top,
        top_before_distinct: _,
        projection: _,
        exclude,
        into,
        from: _,
        lateral_views,
        prewhere,
        selection: _,
        group_by,
        cluster_by,
        distribute_by,
        sort_by,
        having: _,
        named_window,
        qualify,
        window_before_qualify: _,
        value_table_mode,
        connect_by,
        flavor,
    } = s;
    let modifiers = matches!(group_by, sql::GroupByExpr::Expressions(_, m) if !m.is_empty());
    let clause = if distinct.is_some() {
        "SELECT DISTINCT (it runs as a compound query, through Db::query)"
    } else if top.is_some() {
        "TOP (use LIMIT)"
    } else if into.is_some() {
        "SELECT INTO"
    } else if named_window.is_empty() && qualify.is_none() && !modifiers && exclude.is_none() && lateral_views.is_empty() && prewhere.is_none() && cluster_by.is_empty() && distribute_by.is_empty() && sort_by.is_empty() && value_table_mode.is_none() && connect_by.is_none() && *flavor == sql::SelectFlavor::Standard {
        return Ok(());
    } else if !named_window.is_empty() || qualify.is_some() {
        "WINDOW or QUALIFY"
    } else if modifiers {
        "GROUP BY ... WITH ROLLUP, CUBE or TOTALS"
    } else {
        return Err(err(format!("this form of SELECT is not supported: {s}")));
    };
    Err(err(format!("{clause} is not supported")))
}

fn contains_graph(p: &Pred) -> bool {
    match p {
        Pred::Graph { .. } => true,
        Pred::And(ps) | Pred::Or(ps) => ps.iter().any(contains_graph),
        Pred::Not(q) => contains_graph(q),
        _ => false,
    }
}

fn fname(f: &sql::Function) -> String {
    f.name.to_string().to_ascii_lowercase()
}

fn literal_usize(e: &Expr) -> Option<usize> {
    match e {
        Expr::Value(v) => match &v.value {
            sql::Value::Number(s, _) => s.parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

/// The smallest integer at least `v` (a number), in 128 bits so that
/// doubles beyond the 64-bit range stay beyond it.
fn ceil(v: &Value) -> i128 {
    match v {
        Value::Int(x) => *x as i128,
        v => v.as_f64().unwrap_or(0.0).ceil().clamp(-1e20, 1e20) as i128,
    }
}

/// The largest integer at most `v` (a number).
fn floor(v: &Value) -> i128 {
    match v {
        Value::Int(x) => *x as i128,
        v => v.as_f64().unwrap_or(0.0).floor().clamp(-1e20, 1e20) as i128,
    }
}

/// INT field `f` within `[lo, hi]`, cut to the 64-bit range.
fn int_range(f: usize, lo: i128, hi: i128) -> Pred {
    let (lo, hi) = (lo.max(i64::MIN as i128), hi.min(i64::MAX as i128));
    if lo > hi {
        return Pred::False;
    }
    Pred::IntRange { field: f, lo: lo as i64, hi: hi as i64 }
}

fn next_up(x: f64) -> f64 {
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x == 0.0 {
        return f64::from_bits(1);
    }
    let b = x.to_bits();
    f64::from_bits(if x > 0.0 { b + 1 } else { b - 1 })
}

fn next_down(x: f64) -> f64 {
    -next_up(-x)
}

/// The values of a list-valued parameter. A JSON list of numbers arrives as
/// a vector of f32, so a vector of whole numbers that f32 holds exactly
/// counts as a list of integers.
fn as_list(v: Value) -> std::result::Result<Vec<Value>, Value> {
    match v {
        Value::List(xs) => Ok(xs),
        Value::Vector(xs) if xs.iter().all(|x| x.fract() == 0.0 && x.abs() < 16_777_216.0) => Ok(xs.iter().map(|&x| Value::Int(x as i64)).collect()),
        v => Err(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::beir_schema;
    use crate::schema::VectorIndexKind;

    fn schema() -> Schema {
        beir_schema(4, VectorIndexKind::Flat)
    }

    fn q() -> Params {
        Params::new().set("q", Value::Vector(vec![0.5f32; 4].into())).set("t", Value::Str("mrna vaccine".into()))
    }

    #[test]
    fn binds_filtered_bm25() {
        let s = bind(
            "SELECT id, title, score() FROM docs WHERE MATCH(body, 'vaccine -animal') AND year BETWEEN 2019 AND 2021 \
             AND category IN ('c01', 'c02') AND rating > 4 ORDER BY bm25(body, :t) DESC LIMIT 10",
            &schema(),
            &q(),
        )
        .unwrap();
        assert_eq!(s.limit, Some(10));
        assert_eq!(s.projection.len(), 3);
        let Pred::And(ps) = &s.filter else { panic!("{:?}", s.filter) };
        assert_eq!(ps.len(), 4);
        assert!(matches!(&ps[0], Pred::Match { field: 3, query, .. } if query == "vaccine -animal"));
        assert_eq!(ps[1], Pred::IntRange { field: 4, lo: 2019, hi: 2021 });
        assert!(matches!(&ps[2], Pred::In { field: 5, values } if values.len() == 2));
        assert!(matches!(ps[3], Pred::Range { field: 6, lo, hi } if lo > 4.0 && lo < 4.0001 && hi.is_infinite()));
        assert!(matches!(&s.order, Some(OrderBy::Score { score: Score::Bm25 { field: 3, text, .. }, desc: true, .. }) if text == "mrna vaccine"));
    }

    #[test]
    fn binds_vector_and_hybrid_scores() {
        let s = bind("SELECT id FROM docs WHERE year > 2019 ORDER BY similarity(emb, :q, nprobe => 32) DESC LIMIT 5", &schema(), &q())
            .unwrap();
        assert_eq!(s.filter, Pred::IntRange { field: 4, lo: 2020, hi: i64::MAX });
        let Some(OrderBy::Score { score: Score::Similarity { opts, .. }, desc: true, .. }) = &s.order else { panic!() };
        assert_eq!(opts.get("nprobe"), Some(&Value::Int(32)));

        let s = bind("SELECT id FROM docs ORDER BY distance(emb, :q) LIMIT 5", &schema(), &q()).unwrap();
        assert!(matches!(s.order, Some(OrderBy::Score { score: Score::Similarity { .. }, desc: true, .. })));

        let s = bind("SELECT id FROM docs ORDER BY rrf(bm25(body, :t), similarity(emb, :q), k => 20) DESC LIMIT 5", &schema(), &q())
            .unwrap();
        let Some(OrderBy::Score { score: Score::Rrf { inputs, k, depth }, .. }) = &s.order else { panic!() };
        assert_eq!((inputs.len(), *k, *depth), (2, 20.0, 100));

        let s = bind("SELECT id FROM docs ORDER BY 0.3 * bm25(body, :t) + 0.7 * similarity(emb, :q) DESC LIMIT 5", &schema(), &q())
            .unwrap();
        let Some(OrderBy::Score { score: Score::Weighted(ts), .. }) = &s.order else { panic!() };
        assert_eq!(ts.iter().map(|(w, _)| *w).collect::<Vec<_>>(), vec![0.3, 0.7]);
    }

    #[test]
    fn binds_facets_and_rejects_errors() {
        let s = bind("SELECT category, COUNT(*) FROM docs WHERE MATCH(body, 'x') GROUP BY category ORDER BY 2 DESC LIMIT 5", &schema(), &q())
            .unwrap();
        assert_eq!(s.group_by, vec![5]);
        assert_eq!(s.order, Some(OrderBy::Count { desc: true }));
        assert_eq!(s.projection[1].1, Projection::Count);
        let s = bind("SELECT category, COUNT(*) FROM docs GROUP BY category ORDER BY 2 DESC, 1 LIMIT 5", &schema(), &q()).unwrap();
        assert_eq!(s.tiebreak, Some(Tiebreak::GroupValue { desc: false }));
        let s = bind("SELECT id FROM docs WHERE rating BETWEEN 2.0 AND 2.0 ORDER BY rating DESC, id LIMIT 10", &schema(), &q()).unwrap();
        assert_eq!(s.tiebreak, Some(Tiebreak::Key { desc: false }));
        assert!(matches!(s.order, Some(OrderBy::Score { score: Score::Column { field: 6 }, desc: true, .. })));
        // A product of scores (a boost), not a weighted sum.
        let s = bind("SELECT id FROM docs ORDER BY bm25(body, :t) * similarity(emb, :q) DESC", &schema(), &q()).unwrap();
        assert!(matches!(&s.order, Some(OrderBy::Score { score: Score::Product(f), desc: true, .. }) if f.len() == 2), "{:?}", s.order);

        for bad in [
            "SELECT id FROM other",
            "SELECT id FROM docs WHERE MATCH(year, 'x')",
            "SELECT id FROM docs WHERE category > 'c01'",
            "SELECT id FROM docs ORDER BY similarity(emb, :missing)",
            "SELECT id FROM docs ORDER BY bm25(body, :t) * 2 * similarity(emb, :q)",
            "SELECT id FROM docs ORDER BY year, rating",
            "DELETE FROM docs",
        ] {
            assert!(bind(bad, &schema(), &q()).is_err(), "{bad}");
        }
    }

    #[test]
    fn star_and_strict_float_bounds() {
        let s = bind("SELECT * FROM docs WHERE rating < 2.5 OR NOT category = 'c00' LIMIT 3 OFFSET 1", &schema(), &q()).unwrap();
        let names: Vec<&str> = s.projection.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["id", "title", "text", "year", "category", "rating", "topic"]);
        assert_eq!((s.limit, s.offset), (Some(3), 1));
        let Pred::Or(ps) = &s.filter else { panic!() };
        assert!(matches!(ps[0], Pred::Range { lo, hi, .. } if lo.is_infinite() && hi < 2.5 && hi > 2.4999));
        // A negated comparison also needs the value (NULL != 'c00' is not true).
        assert!(matches!(&ps[1], Pred::And(qs) if matches!(qs[..], [Pred::Present { field: 5 }, Pred::Not(_)])), "{:?}", ps[1]);
    }
}
