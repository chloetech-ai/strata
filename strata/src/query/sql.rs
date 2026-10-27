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
//! Writes ([`bind_write`]): `INSERT INTO t (cols) VALUES (...), ...`,
//! `INSERT OR REPLACE` (an upsert), `UPDATE t SET f = v, g = g + 1 [WHERE
//! ...]` and `DELETE FROM t [WHERE ...]`. Lists are `ARRAY[...]`; vectors are
//! parameters, `ARRAY[...]` or a JSON array string.
//!
//! Graph queries over EDGES fields (DESIGN.md §6.1): the predicates
//! `REACHABLE(field, seeds, ...)` and `PATH(field, from, to, ...)`, the score
//! `ppr(field, seeds, ...)` and the column `hops()`. Seeds are a key, a
//! parameter, a tuple of keys or a subquery over the same table.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use sqlparser::ast::{self as sql, BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::error::{Error, Result};
use crate::query::logical::{AggFn, Decay, DecayShape, Direction, Expand, GraphKind, GraphQuery, Having, OrderBy, Opts, Pred, Projection, Score, Seeds, Select, Tiebreak};
use crate::schema::{FieldType, Schema};
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
    /// Documents in schema order (unnamed columns NULL); `replace` upserts.
    Insert { docs: Vec<Vec<Value>>, replace: bool },
    /// The documents `select` returns (their primary keys) get `sets`.
    Update { select: Select, sets: Vec<(usize, Assign)> },
    /// The documents `select` returns are deleted.
    Delete { select: Select },
}

/// The right-hand side of `SET field = ...`.
#[derive(Clone, Debug, PartialEq)]
pub enum Assign {
    Value(Value),
    /// `field = field + x` (or `- x`).
    Add(f64),
}

/// Parses and binds one INSERT, UPDATE or DELETE; `None` for a SELECT.
pub fn bind_write(sql_text: &str, schema: &Schema, params: &Params) -> Result<Option<Write>> {
    let stmts = Parser::parse_sql(&GenericDialect {}, sql_text).map_err(|e| err(e.to_string()))?;
    let [stmt] = stmts.as_slice() else { return Err(err("expected exactly one statement")) };
    let b = Binder::new(schema, params);
    let table = |name: &str| {
        if name.eq_ignore_ascii_case(&schema.table) {
            Ok(())
        } else {
            Err(err(format!("no such table: {name}")))
        }
    };
    // The documents a WHERE clause selects, as a query for their keys and
    // every field (an UPDATE may rewrite them).
    let selecting = |selection: &Option<Expr>| -> Result<Select> {
        let cols: Vec<&str> = schema.fields.iter().map(|f| f.name.as_str()).collect();
        let text = match selection {
            Some(e) => format!("SELECT {} FROM {} WHERE {e}", cols.join(", "), schema.table),
            None => format!("SELECT {} FROM {}", cols.join(", "), schema.table),
        };
        bind(&text, schema, params)
    };
    Ok(Some(match stmt {
        sql::Statement::Query(_) => return Ok(None),
        sql::Statement::Insert(ins) => {
            match &ins.table {
                sql::TableObject::TableName(n) => table(&n.to_string())?,
                _ => return Err(err("INSERT needs a table name")),
            }
            let replace = ins.replace_into || matches!(ins.or, Some(sql::SqliteOnConflict::Replace)) || ins.on.is_some();
            let cols: Vec<usize> = if ins.columns.is_empty() {
                (0..schema.fields.len()).collect()
            } else {
                ins.columns.iter().map(|c| schema.field_index(&c.value).ok_or_else(|| err(format!("no such column: {}", c.value)))).collect::<Result<_>>()?
            };
            let Some(sql::SetExpr::Values(values)) = ins.source.as_ref().map(|q| q.body.as_ref()) else {
                return Err(err("INSERT needs VALUES"));
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
            Write::Insert { docs, replace }
        }
        sql::Statement::Update { table: t, assignments, selection, .. } => {
            table(&t.relation.to_string())?;
            let mut sets = Vec::new();
            for a in assignments {
                let sql::AssignmentTarget::ColumnName(name) = &a.target else { return Err(err("SET one column at a time")) };
                let f = schema.field_index(&name.to_string()).ok_or_else(|| err(format!("no such column: {name}")))?;
                if Some(f) == schema.primary_key {
                    return Err(err("the primary key cannot be updated; delete and insert instead"));
                }
                let assign = match &a.value {
                    Expr::BinaryOp { left, op: op @ (BinaryOperator::Plus | BinaryOperator::Minus), right } if b.field_of(left).ok() == Some(f) => {
                        let x = b.num_lit(right)?;
                        Assign::Add(if *op == BinaryOperator::Minus { -x } else { x })
                    }
                    e => Assign::Value(b.write_value(f, e)?),
                };
                if matches!(assign, Assign::Add(_)) && !matches!(schema.fields[f].ty, FieldType::Int | FieldType::Float) {
                    return Err(err(format!("{name} is not numeric")));
                }
                sets.push((f, assign));
            }
            Write::Update { select: selecting(selection)?, sets }
        }
        sql::Statement::Delete(d) => {
            let (sql::FromTable::WithFromKeyword(from) | sql::FromTable::WithoutKeyword(from)) = &d.from;
            let [t] = from.as_slice() else { return Err(err("DELETE from exactly one table")) };
            table(&t.relation.to_string())?;
            Write::Delete { select: selecting(&d.selection)? }
        }
        _ => return Err(err("only SELECT, INSERT, UPDATE and DELETE are supported")),
    }))
}

/// Parses and binds one SELECT statement.
pub fn bind(sql_text: &str, schema: &Schema, params: &Params) -> Result<Select> {
    let stmts = Parser::parse_sql(&GenericDialect {}, sql_text).map_err(|e| err(e.to_string()))?;
    let [stmt] = stmts.as_slice() else { return Err(err("expected exactly one statement")) };
    let sql::Statement::Query(q) = stmt else { return Err(err("only SELECT is supported")) };
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
        let sql::SetExpr::Select(s) = q.body.as_ref() else { return Err(err("only simple SELECT is supported")) };
        if s.from.len() != 1 || !s.from[0].joins.is_empty() {
            return Err(err("exactly one table, no joins"));
        }
        if let sql::TableFactor::Table { name, .. } = &s.from[0].relation {
            let t = name.to_string();
            if !t.eq_ignore_ascii_case(&self.schema.table) {
                return Err(err(format!("no such table: {t}")));
            }
        }
        let filter = match &s.selection {
            Some(e) => self.pred(e)?.simplify(),
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
                    let order = if aggregated {
                        self.group_order(&es[0].expr, desc, &projection, &group_by)?
                    } else {
                        self.order(&es[0].expr, desc, &projection)?
                    };
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
        Ok(Select { projection, filter, order, group_by, having, tiebreak, limit, offset, graphs })
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
                let p = self.pred(e)?.simplify();
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
            _ => match self.literal(e) {
                Ok(Value::List(xs)) => {
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
                let h = Having::Range { what: self.group_value(expr, proj, group)?, lo: self.num_lit(low)?, hi: self.num_lit(high)? };
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
                let v = self.literal(lit)?;
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

    fn order(&self, e: &Expr, desc: bool, proj: &[(String, Projection)]) -> Result<OrderBy> {
        // Positional `ORDER BY 2` addresses a selected column.
        if let Some(n) = literal_usize(e) {
            let (_, p) = proj.get(n.wrapping_sub(1)).ok_or_else(|| err(format!("ORDER BY {n}: no such column")))?;
            return match p {
                Projection::Field(f) => Ok(OrderBy::Score { score: Score::Column { field: *f }, desc }),
                Projection::Score => Err(err("ORDER BY score(): order by the score expression itself")),
                Projection::Hops(g) => Ok(OrderBy::Score { score: Score::Hops { graph: *g, far_first: desc }, desc: true }),
                Projection::Count | Projection::Agg { .. } => Err(err("ORDER BY an aggregate needs GROUP BY")),
            };
        }
        // `ORDER BY hops()` is nearest first; ranking is always best first.
        if let Expr::Function(f) = e {
            if fname(f) == "hops" {
                return Ok(OrderBy::Score { score: Score::Hops { graph: self.hops_graph()?, far_first: desc }, desc: true });
            }
        }
        // `ORDER BY distance(..) ASC` ranks like `similarity DESC`.
        if let Expr::Function(f) = e {
            if fname(f) == "distance" {
                return Ok(OrderBy::Score { score: self.score(e)?, desc: !desc });
            }
        }
        Ok(OrderBy::Score { score: self.score(e)?, desc })
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
    /// since the Unix epoch).
    fn decay(&self, f: &sql::Function) -> Result<Score> {
        let (args, opts) = self.args(f)?;
        let usage = "decay(field, origin, scale, offset => .., decay => .., shape => 'exp' | 'gauss' | 'linear')";
        let [field, origin, scale] = args.as_slice() else { return Err(err(usage)) };
        let field = self.numeric_field(field)?;
        if self.schema.fields[field].multi {
            return Err(err(format!("cannot decay multi-valued field {}", self.schema.fields[field].name)));
        }
        let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let origin = match origin {
            Expr::Function(g) if fname(g) == "now" => now(),
            e => match self.literal(e)? {
                Value::Str(s) if s.eq_ignore_ascii_case("now") => now(),
                v => v.as_f64().ok_or_else(|| err("decay origin must be a number or now()"))?,
            },
        };
        let scale = self.num_lit(scale)?;
        let num = |k: &str, d: f64| opts.get(k).map_or(Ok(d), |v| v.as_f64().ok_or_else(|| err(format!("decay {k} must be a number"))));
        let (offset, decay) = (num("offset", 0.0)?, num("decay", 0.5)?);
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
                let (lo, hi) = (self.num_lit(low)?, self.num_lit(high)?);
                let p = Pred::Range { field: f, lo, hi };
                if *negated { Pred::Not(Box::new(p)) } else { p }
            }
            Expr::InList { expr, list, negated } => {
                let f = self.field_of(expr)?;
                if self.schema.fields[f].ty == FieldType::Edges {
                    return Err(err(format!("{} is an EDGES field: use REACHABLE({}, keys) for rows linked to keys", self.schema.fields[f].name, self.schema.fields[f].name)));
                }
                // A list-valued parameter stands for its values (`IN (:ids)`).
                let mut values = Vec::with_capacity(list.len());
                for v in list {
                    match self.literal(v)? {
                        Value::List(xs) => {
                            for x in xs {
                                values.push(self.coerce(f, x)?);
                            }
                        }
                        x => values.push(self.coerce(f, x)?),
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
        if matches!(ty, FieldType::Keyword) {
            let v = self.field_value(f, lit_e)?;
            return match op {
                Eq => Ok(Pred::In { field: f, values: vec![v] }),
                NotEq => Ok(Pred::Not(Box::new(Pred::In { field: f, values: vec![v] }))),
                _ => Err(err("keyword fields support =, != and IN")),
            };
        }
        if !matches!(ty, FieldType::Int | FieldType::Float) {
            return Err(err(format!("cannot compare field {}", self.schema.fields[f].name)));
        }
        let x = self.num_lit(lit_e)?;
        let int = matches!(ty, FieldType::Int);
        let above = |x: f64| if int { x.floor() + 1.0 } else { next_up(x) };
        let below = |x: f64| if int { x.ceil() - 1.0 } else { next_down(x) };
        let (inf, ninf) = (f64::INFINITY, f64::NEG_INFINITY);
        let range = |lo, hi| Pred::Range { field: f, lo, hi };
        Ok(match op {
            Eq => range(x, x),
            NotEq => Pred::Not(Box::new(range(x, x))),
            Gt => range(above(x), inf),
            GtEq => range(if int { x.ceil() } else { x }, inf),
            Lt => range(ninf, below(x)),
            LtEq => range(ninf, if int { x.floor() } else { x }),
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
                sql::Value::Boolean(b) => Ok(Value::Int(*b as i64)),
                sql::Value::Null => Ok(Value::Null),
                sql::Value::Placeholder(p) => Ok(self.params.get(p)?.clone()),
                other => Err(err(format!("unsupported literal {other}"))),
            },
            Expr::UnaryOp { op: sql::UnaryOperator::Minus, expr } => match self.literal(expr)? {
                Value::Int(x) => Ok(Value::Int(-x)),
                Value::Float(x) => Ok(Value::Float(-x)),
                _ => Err(err("cannot negate a non-number")),
            },
            Expr::Identifier(id) if !id.value.is_empty() => Ok(Value::Str(id.value.clone())),
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

    /// A value coerced to field `f`'s type.
    fn coerce(&self, f: usize, v: Value) -> Result<Value> {
        match (&self.schema.fields[f].ty, v) {
            (FieldType::Keyword, Value::Str(s)) => Ok(Value::Str(s)),
            (FieldType::Keyword, Value::Int(x)) => Ok(Value::Str(x.to_string())),
            (FieldType::Int, Value::Int(x)) => Ok(Value::Int(x)),
            (FieldType::Float, v @ (Value::Int(_) | Value::Float(_))) => Ok(Value::Float(v.as_f64().unwrap())),
            (t, v) => Err(err(format!("value {v:?} does not fit a {t:?} field"))),
        }
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

    fn vector(&self, e: &Expr) -> Result<Arc<[f32]>> {
        match self.literal(e)? {
            Value::Vector(v) => Ok(v),
            Value::Str(s) => {
                let v: Vec<f32> = serde_json::from_str(&s).map_err(|_| err("vector literal must be a JSON array"))?;
                Ok(v.into())
            }
            v => Err(err(format!("expected a vector, got {v:?}"))),
        }
    }
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
        assert_eq!(ps[1], Pred::Range { field: 4, lo: 2019.0, hi: 2021.0 });
        assert!(matches!(&ps[2], Pred::In { field: 5, values } if values.len() == 2));
        assert!(matches!(ps[3], Pred::Range { field: 6, lo, hi } if lo > 4.0 && lo < 4.0001 && hi.is_infinite()));
        assert!(matches!(&s.order, Some(OrderBy::Score { score: Score::Bm25 { field: 3, text, .. }, desc: true }) if text == "mrna vaccine"));
    }

    #[test]
    fn binds_vector_and_hybrid_scores() {
        let s = bind("SELECT id FROM docs WHERE year > 2019 ORDER BY similarity(emb, :q, nprobe => 32) DESC LIMIT 5", &schema(), &q())
            .unwrap();
        assert_eq!(s.filter, Pred::Range { field: 4, lo: 2020.0, hi: f64::INFINITY });
        let Some(OrderBy::Score { score: Score::Similarity { opts, .. }, desc: true }) = &s.order else { panic!() };
        assert_eq!(opts.get("nprobe"), Some(&Value::Int(32)));

        let s = bind("SELECT id FROM docs ORDER BY distance(emb, :q) LIMIT 5", &schema(), &q()).unwrap();
        assert!(matches!(s.order, Some(OrderBy::Score { score: Score::Similarity { .. }, desc: true })));

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
        assert_eq!(s.group_by, Some(5));
        assert_eq!(s.order, Some(OrderBy::Count { desc: true }));
        assert_eq!(s.projection[1].1, Projection::Count);
        let s = bind("SELECT category, COUNT(*) FROM docs GROUP BY category ORDER BY 2 DESC, 1 LIMIT 5", &schema(), &q()).unwrap();
        assert_eq!(s.tiebreak, Some(Tiebreak::GroupValue { desc: false }));
        let s = bind("SELECT id FROM docs WHERE rating BETWEEN 2.0 AND 2.0 ORDER BY rating DESC, id LIMIT 10", &schema(), &q()).unwrap();
        assert_eq!(s.tiebreak, Some(Tiebreak::Key { desc: false }));
        assert!(matches!(s.order, Some(OrderBy::Score { score: Score::Column { field: 6 }, desc: true })));
        // A product of scores (a boost), not a weighted sum.
        let s = bind("SELECT id FROM docs ORDER BY bm25(body, :t) * similarity(emb, :q) DESC", &schema(), &q()).unwrap();
        assert!(matches!(&s.order, Some(OrderBy::Score { score: Score::Product(f), desc: true }) if f.len() == 2), "{:?}", s.order);

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
        assert!(matches!(&ps[1], Pred::Not(_)));
    }
}
