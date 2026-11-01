//! Paths of JSON fields that the executor reads itself.
//!
//! The planner of compound queries reads a path outside `WHERE` (`GROUP BY
//! attrs.color`, `AVG(attrs.price)`, `ORDER BY attrs.price`) with
//! operators: a leaf returns each row's object, and `json_extract` above it
//! parses them one by one. The executor reads those paths from the field's
//! terms instead ([`crate::json::PathValues`]), so [`paths`] moves the
//! operators over such a leaf into it when the executor runs them as they
//! would:
//!
//! * `Aggregate` of fields and paths over `Project` of fields over a
//!   `Search` becomes `Aggregate` over the `Search`, under a `Project` of
//!   its outputs; with the `Filter`, `Sort`, `Limit` and `Project` above it,
//!   when the executor takes those too;
//! * `Project` over `[Limit]` over `Sort` by a path (then possibly the
//!   primary key) over `Project` of fields over a `Search` becomes the
//!   `Search` ordered by the path, one leaf when the columns are fields and
//!   paths and the path is among them, else under the query's `Project`,
//!   the leaf giving the path as a column after the rows' fields (which a
//!   coordinator merges the rows of shards by).
//!
//! A node is moved into a leaf only when the leaf raises to the executor's
//! SELECT ([`super::Plan::to_select`]); a query whose plan has none of
//! these shapes plans as before. Joins are left as they are: a leaf on the
//! right of one is restricted to the left's keys, which a leaf that groups
//! or ranks its rows does not allow.

use super::{raise, Agg, Column, Expr, Func, Node, Search, SortKey};
use crate::schema::{FieldType, Schema};
use crate::value::Value;

/// Moves the operators over leaves that read paths into the leaves
/// ([module docs](self)), in `n` and below it.
pub(crate) fn paths(n: &mut Node, catalog: &[&Schema]) {
    if matches!(n, Node::Join { .. } | Node::KeyOrder { .. }) {
        return;
    }
    for c in n.inputs_mut() {
        paths(c, catalog);
    }
    let fused = aggregate(n, catalog).or_else(|| groups(n, catalog)).or_else(|| ordered(n, catalog));
    if let Some(f) = fused {
        *n = f;
    }
}

/// The table `s` searches.
fn table<'s>(catalog: &[&'s Schema], s: &Search) -> Option<&'s Schema> {
    catalog.iter().copied().find(|t| t.table.eq_ignore_ascii_case(&s.table))
}

/// Whether the executor takes `n` as a leaf of `schema`'s table.
fn raises(n: &Node, schema: &Schema) -> bool {
    raise::select_of(n, schema).is_ok()
}

/// What `e`, an expression of the rows of a `Project` of `cols`, reads of
/// the rows of the Search below: a field the Project passes on, or a path
/// of an indexed JSON field it passes on (`json_extract(field, path [,
/// type])`), and whether it is a path. None for any other expression.
fn column(e: &Expr, cols: &[Column], schema: &Schema) -> Option<(Expr, bool)> {
    let field = |i: u32| match cols.get(i as usize)?.expr {
        Expr::Field(f) => Some(f),
        _ => None,
    };
    match e {
        Expr::Output(i) => Some((Expr::Field(field(*i)?), false)),
        Expr::Call { func: Func::JsonExtract, args } => {
            let (Some(Expr::Output(i)), rest) = (args.first(), &args[1.min(args.len())..]) else { return None };
            if rest.is_empty() || rest.len() > 2 || !rest.iter().all(|a| matches!(a, Expr::Lit(Value::Str(_)))) {
                return None;
            }
            let f = field(*i)?;
            let fld = schema.fields.get(f as usize)?;
            if fld.ty != FieldType::Json || !fld.indexed {
                return None;
            }
            let args = std::iter::once(Expr::Field(f)).chain(rest.iter().cloned()).collect();
            Some((Expr::Call { func: Func::JsonExtract, args }, true))
        }
        _ => None,
    }
}

/// Whether the executor groups and aggregates by field `f` as the
/// operators do: an indexed keyword or number.
fn groupable(schema: &Schema, f: &Expr) -> bool {
    let Expr::Field(f) = f else { return true };
    schema.fields.get(*f as usize).is_some_and(|fld| fld.indexed && matches!(fld.ty, FieldType::Keyword | FieldType::Int | FieldType::Float))
}

/// The columns of a `Project` that passes on an Aggregate's `width`
/// outputs as they are.
fn outputs(width: usize) -> Vec<Column> {
    (0..width).map(|i| Column { name: format!("#{i}"), expr: Expr::Output(i as u32) }).collect()
}

/// `Aggregate` of fields and paths over `Project` of fields over a
/// `Search`, as an `Aggregate` over the Search under a `Project` of its
/// outputs: a leaf.
fn aggregate(n: &Node, catalog: &[&Schema]) -> Option<Node> {
    let Node::Aggregate { input, keys, aggs } = n else { return None };
    let Node::Project { input: below, columns: cols } = &**input else { return None };
    let Node::Search(s) = &**below else { return None };
    if !s.order.is_empty() || !s.phases.is_empty() || s.per.is_some() {
        return None;
    }
    let schema = table(catalog, s)?;
    let mut any_path = false;
    let mut read = |e: &Expr| {
        let (x, path) = column(e, cols, schema)?;
        any_path |= path;
        groupable(schema, &x).then_some(x)
    };
    let keys2 = keys.iter().map(&mut read).collect::<Option<Vec<_>>>()?;
    let aggs2 = aggs
        .iter()
        .map(|a| {
            let arg = match &a.arg {
                None => None,
                Some(e) => Some(read(e)?),
            };
            Some(Agg { func: a.func, arg })
        })
        .collect::<Option<Vec<_>>>()?;
    if !any_path {
        return None;
    }
    let agg = Node::Aggregate { input: Box::new(Node::Search(s.clone())), keys: keys2, aggs: aggs2 };
    let leaf = Node::Project { input: Box::new(agg), columns: outputs(keys.len() + aggs.len()) };
    raises(&leaf, schema).then_some(leaf)
}

/// The leaf [`aggregate`] makes with the `Project`, `[Limit]`, `[Sort]`
/// and `[Filter]` over it: one leaf, when the executor takes them.
fn groups(n: &Node, catalog: &[&Schema]) -> Option<Node> {
    /// `n` without the `Project` of an Aggregate's outputs below its
    /// Limit, Sort and Filter.
    fn inner(n: &Node) -> Option<Node> {
        Some(match n {
            Node::Limit { input, offset, limit } => Node::Limit { input: Box::new(inner(input)?), offset: *offset, limit: *limit },
            Node::Sort { input, keys } => Node::Sort { input: Box::new(inner(input)?), keys: keys.clone() },
            Node::Filter { input, cond } => Node::Filter { input: Box::new(inner(input)?), cond: cond.clone() },
            Node::Project { input, columns } => {
                let Node::Aggregate { input: s, keys, aggs } = &**input else { return None };
                if !matches!(**s, Node::Search(_)) || *columns != outputs(keys.len() + aggs.len()) {
                    return None;
                }
                (**input).clone()
            }
            _ => return None,
        })
    }
    let Node::Project { input, columns } = n else { return None };
    let leaf = Node::Project { input: Box::new(inner(input)?), columns: columns.clone() };
    let schema = table(catalog, leaf.search()?)?;
    raises(&leaf, schema).then_some(leaf)
}

/// `Project` over `[Limit]` over `Sort` by a path (then possibly the
/// primary key) over `Project` of fields over a `Search`, as the Search
/// ordered by the path.
fn ordered(n: &Node, catalog: &[&Schema]) -> Option<Node> {
    let Node::Project { input, columns: top } = n else { return None };
    let (limit, sort) = match &**input {
        Node::Limit { input, offset, limit } => (Some((*offset, *limit)), &**input),
        x => (None, x),
    };
    let Node::Sort { input: below, keys } = sort else { return None };
    let Node::Project { input: inner, columns: cols } = &**below else { return None };
    let Node::Search(s) = &**inner else { return None };
    if !s.order.is_empty() || !s.phases.is_empty() || s.per.is_some() || keys.is_empty() || keys.len() > 2 {
        return None;
    }
    let schema = table(catalog, s)?;
    let (first, true) = column(&keys[0].expr, cols, schema)? else { return None };
    let mut order = vec![SortKey { expr: first.clone(), ..keys[0].clone() }];
    if let Some(k) = keys.get(1) {
        let (e, _) = column(&k.expr, cols, schema)?;
        if Some(&e) != schema.primary_key.map(|p| Expr::Field(p as u32)).as_ref() {
            return None;
        }
        order.push(SortKey { expr: e, nulls_first: false, ..k.clone() });
    }
    let search = Node::Search(Search { order, ..s.clone() });
    let rows = match limit {
        Some((offset, limit)) => Node::Limit { input: Box::new(search), offset, limit },
        None => search,
    };
    // One leaf, when the query's columns are fields and paths, the order's
    // among them.
    let own: Option<Vec<Column>> = top.iter().map(|c| column(&c.expr, cols, schema).map(|(expr, _)| Column { name: c.name.clone(), expr })).collect();
    if let Some(own) = own.filter(|o| o.iter().any(|c| c.expr == first)) {
        let leaf = Node::Project { input: Box::new(rows.clone()), columns: own };
        if raises(&leaf, schema) {
            return Some(leaf);
        }
    }
    let mut cols = cols.clone();
    cols.push(Column { name: ORDER_COLUMN.to_string(), expr: first });
    let leaf = Node::Project { input: Box::new(rows), columns: cols };
    raises(&leaf, schema).then(|| Node::Project { input: Box::new(leaf), columns: top.clone() })
}

/// The name of the column of the path a leaf orders its rows by, after
/// the columns the query reads ([`ordered`]).
pub(crate) const ORDER_COLUMN: &str = "\0order";
