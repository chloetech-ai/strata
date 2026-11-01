//! The paths listing of a JSON field ([`crate::json`]): which paths its
//! objects hold, in how many rows, with values of which types, and which
//! values, read from the field's terms without reading a row.
//!
//! ```sql
//! SELECT * FROM json_paths('products', 'attrs');
//! SELECT * FROM json_paths('products', 'attrs', 'dims');           -- dims and below
//! SELECT * FROM json_paths('products', 'attrs', filter => 'price > 5');
//! ```
//!
//! One row per path, in path order: `path` (as a query names it), `rows`
//! (the rows holding a value or an object there), `share` (of the rows
//! read: the table's, or those `filter` matches), `strings`, `numbers` and
//! `bools` (the rows holding a value of that type there; a string longer
//! than [`crate::json::MAX_EXACT`] bytes is in `rows` only, as comparisons
//! do not see it), `min` and `max` (of the numbers), `distinct` (values,
//! of every type) and `values`: each value with its rows, most rows first,
//! as JSON text (`[["red", 12], [5, 3]]`), when there are at most
//! [`MAX_VALUES`]. At most [`MAX_PATHS`] paths, the first in path order.
//!
//! A coordinator asks each shard for its listing in SQL and sums them
//! ([`Paths::merge`]): rows and types add up, and values and their count
//! too while each shard lists its values; where one holds more than
//! [`MAX_VALUES`] at a path, `distinct` is NULL there.

use std::collections::BTreeMap;

use sqlparser::ast::{self as sql, FunctionArg, FunctionArgExpr, SelectItem, SetExpr, TableFactor};

use crate::error::{Error, Result};
use crate::json::{self, Kind};
use crate::query::exec::{ExecStats, ResultSet};
use crate::value::Value;

/// Paths a listing holds at most: the first in path order.
pub const MAX_PATHS: usize = 10_000;
/// Values a path lists at most; past this, only their number.
pub const MAX_VALUES: usize = 100;

/// The answer's columns.
pub const COLUMNS: [&str; 10] = ["path", "rows", "share", "strings", "numbers", "bools", "min", "max", "distinct", "values"];

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// One path of a listing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathStats {
    /// As a query names it (keys joined by dots, escaped).
    pub path: String,
    /// Rows holding a value or an object at the path.
    pub rows: u64,
    /// Rows holding a string (of at most [`json::MAX_EXACT`] bytes), a
    /// number, a boolean there.
    pub strings: u64,
    pub numbers: u64,
    pub bools: u64,
    /// The least and greatest number there.
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Distinct values there, of every type; `None` where a shard held
    /// more than [`MAX_VALUES`] (and they were not counted together).
    pub distinct: Option<u64>,
    /// Each value with its rows, most rows first, when there are at most
    /// [`MAX_VALUES`].
    pub values: Option<Vec<(Value, u64)>>,
}

impl PathStats {
    /// Rows holding a value of type `k` there.
    pub fn of_kind(&self, k: Kind) -> u64 {
        match k {
            Kind::Str => self.strings,
            Kind::Number => self.numbers,
            Kind::Bool => self.bools,
        }
    }

    pub(crate) fn kind_mut(&mut self, k: Kind) -> &mut u64 {
        match k {
            Kind::Str => &mut self.strings,
            Kind::Number => &mut self.numbers,
            Kind::Bool => &mut self.bools,
        }
    }
}

/// A JSON field's paths, among the rows read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Paths {
    /// The rows read: the table's, or those the filter matched.
    pub rows: u64,
    pub paths: Vec<PathStats>,
    /// Whether paths past the last were left out ([`MAX_PATHS`]).
    pub truncated: bool,
}

/// What a value's key is among a path's values: its term without a path,
/// which orders and tells apart values of every type.
fn value_key(v: &Value) -> String {
    json::term("", v).unwrap_or_default()
}

/// A value as JSON: a number that is an integer as one.
pub fn value_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Float(x) if x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 => serde_json::json!(*x as i64),
        Value::Float(x) => serde_json::json!(x),
        Value::Int(x) => serde_json::json!(x),
        Value::Str(s) => serde_json::json!(s),
        Value::Bool(b) => serde_json::json!(b),
        _ => serde_json::Value::Null,
    }
}

fn value_of_json(j: &serde_json::Value) -> Result<Value> {
    Ok(match j {
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => Value::Float(n.as_f64().ok_or_else(|| err("a paths listing's value is not a number"))?),
        _ => return Err(err(format!("a paths listing's value is a string, a number or a boolean, not {j}"))),
    })
}

/// Values most rows first, ties in the order of their terms.
fn sort_values(values: &mut [(Value, u64)]) {
    values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| value_key(&a.0).cmp(&value_key(&b.0))));
}

impl Paths {
    /// The listing as rows ([`COLUMNS`]); `stats.filter_docs` holds the
    /// rows read, which a coordinator adds up.
    pub fn to_result(&self) -> ResultSet {
        let share = |n: u64| if self.rows == 0 { Value::Null } else { Value::Float(n as f64 / self.rows as f64) };
        let opt_f = |x: Option<f64>| x.map_or(Value::Null, Value::Float);
        let rows = self
            .paths
            .iter()
            .map(|p| {
                vec![
                    Value::Str(p.path.clone()),
                    Value::Int(p.rows as i64),
                    share(p.rows),
                    Value::Int(p.strings as i64),
                    Value::Int(p.numbers as i64),
                    Value::Int(p.bools as i64),
                    opt_f(p.min),
                    opt_f(p.max),
                    p.distinct.map_or(Value::Null, |d| Value::Int(d as i64)),
                    p.values.as_ref().map_or(Value::Null, |vs| Value::Str(serde_json::Value::Array(vs.iter().map(|(v, n)| serde_json::json!([value_json(v), n])).collect()).to_string())),
                ]
            })
            .collect();
        let mut plan = vec![format!("json_paths: {} paths in {} rows", self.paths.len(), self.rows)];
        if self.truncated {
            plan.push(format!("json_paths: the first {MAX_PATHS} paths; name a path to list those below it"));
        }
        let stats = ExecStats { plan, filter_docs: self.rows, ..ExecStats::default() };
        ResultSet { columns: COLUMNS.iter().map(|c| c.to_string()).collect(), rows, keys: Vec::new(), scores: Vec::new(), parts: None, stats }
    }

    /// A listing from its rows ([`Paths::to_result`]), as a shard answers
    /// a coordinator. A listing of [`MAX_PATHS`] paths may have been cut.
    pub fn from_result(rs: &ResultSet) -> Result<Paths> {
        if rs.columns.iter().map(String::as_str).ne(COLUMNS) {
            return Err(err(format!("a paths listing has the columns {}, not {}", COLUMNS.join(", "), rs.columns.join(", "))));
        }
        let int = |v: &Value| match v {
            Value::Int(x) if *x >= 0 => Ok(*x as u64),
            v => Err(err(format!("a paths listing's count is a number, not {v:?}"))),
        };
        let float = |v: &Value| match v {
            Value::Null => Ok(None),
            Value::Float(x) => Ok(Some(*x)),
            Value::Int(x) => Ok(Some(*x as f64)),
            v => Err(err(format!("a paths listing's number is a number, not {v:?}"))),
        };
        let paths = rs
            .rows
            .iter()
            .map(|r| {
                let Value::Str(path) = &r[0] else { return Err(err("a paths listing's path is text")) };
                let values = match &r[9] {
                    Value::Null => None,
                    Value::Str(s) => {
                        let j: serde_json::Value = serde_json::from_str(s).map_err(|e| err(format!("a paths listing's values: {e}")))?;
                        let pairs = j.as_array().ok_or_else(|| err("a paths listing's values are a list"))?;
                        Some(
                            pairs
                                .iter()
                                .map(|p| match p.as_array().map(Vec::as_slice) {
                                    Some([v, n]) => Ok((value_of_json(v)?, n.as_u64().ok_or_else(|| err("a value's rows are a number"))?)),
                                    _ => Err(err("a paths listing's value is a pair of a value and its rows")),
                                })
                                .collect::<Result<Vec<_>>>()?,
                        )
                    }
                    v => return Err(err(format!("a paths listing's values are JSON text, not {v:?}"))),
                };
                Ok(PathStats {
                    path: path.clone(),
                    rows: int(&r[1])?,
                    strings: int(&r[3])?,
                    numbers: int(&r[4])?,
                    bools: int(&r[5])?,
                    min: float(&r[6])?,
                    max: float(&r[7])?,
                    distinct: match &r[8] {
                        Value::Null => None,
                        v => Some(int(v)?),
                    },
                    values,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Paths { rows: rs.stats.filter_docs, truncated: paths.len() >= MAX_PATHS, paths })
    }

    /// The listing of the rows of all of `parts` (each over other rows: a
    /// shard's). Counts add up; values too while every part lists them. A
    /// part cut at [`MAX_PATHS`] cuts the whole at its last path, past
    /// which the others' counts would miss its own.
    pub fn merge(parts: Vec<Paths>) -> Paths {
        let cut: Option<String> = parts.iter().filter(|p| p.truncated).filter_map(|p| p.paths.last().map(|s| s.path.clone())).min();
        let mut truncated = parts.iter().any(|p| p.truncated);
        let rows = parts.iter().map(|p| p.rows).sum();
        // Per path: the merged stats, and the values by key while every
        // part lists them.
        let mut by: BTreeMap<String, (PathStats, Option<BTreeMap<String, (Value, u64)>>)> = BTreeMap::new();
        for part in parts {
            for p in part.paths {
                if cut.as_ref().is_some_and(|c| p.path > *c) {
                    continue;
                }
                let (m, values) = by.entry(p.path.clone()).or_insert_with(|| (PathStats { path: p.path.clone(), ..PathStats::default() }, Some(BTreeMap::new())));
                m.rows += p.rows;
                for k in [Kind::Str, Kind::Number, Kind::Bool] {
                    *m.kind_mut(k) += p.of_kind(k);
                }
                m.min = [m.min, p.min].into_iter().flatten().reduce(f64::min);
                m.max = [m.max, p.max].into_iter().flatten().reduce(f64::max);
                match (values.as_mut(), p.values) {
                    (Some(vs), Some(pvs)) => {
                        for (v, c) in pvs {
                            vs.entry(value_key(&v)).or_insert((v, 0)).1 += c;
                        }
                    }
                    _ => *values = None,
                }
            }
        }
        let mut paths: Vec<PathStats> = by
            .into_values()
            .map(|(mut m, values)| {
                match values {
                    Some(vs) => {
                        m.distinct = Some(vs.len() as u64);
                        m.values = (vs.len() <= MAX_VALUES).then(|| {
                            let mut v: Vec<(Value, u64)> = vs.into_values().collect();
                            sort_values(&mut v);
                            v
                        });
                    }
                    None => {
                        m.distinct = None;
                        m.values = None;
                    }
                }
                m
            })
            .collect();
        if paths.len() > MAX_PATHS {
            paths.truncate(MAX_PATHS);
            truncated = true;
        }
        Paths { rows, paths, truncated }
    }

    /// The listing as JSON, as `GET /v1/tables/:table/paths` answers.
    pub fn to_json(&self) -> serde_json::Value {
        let paths: Vec<serde_json::Value> = self
            .paths
            .iter()
            .map(|p| {
                let mut o = serde_json::json!({
                    "path": p.path,
                    "keys": json::keys(&p.path),
                    "rows": p.rows,
                    "types": {"string": p.strings, "number": p.numbers, "bool": p.bools},
                    "distinct": p.distinct,
                });
                if let (Some(lo), Some(hi)) = (p.min, p.max) {
                    o["min"] = value_json(&Value::Float(lo));
                    o["max"] = value_json(&Value::Float(hi));
                }
                if let Some(vs) = &p.values {
                    o["values"] = vs.iter().map(|(v, n)| serde_json::json!({"value": value_json(v), "rows": n})).collect();
                }
                o
            })
            .collect();
        serde_json::json!({"rows": self.rows, "paths": paths, "truncated": self.truncated})
    }
}

/// Builds a listing from a sorted stream of a field's terms with their
/// rows ([`Builder::term`]), then the rows of each path's types
/// ([`Builder::kinds`]).
#[derive(Default)]
pub(crate) struct Builder {
    pub paths: Vec<PathStats>,
    pub truncated: bool,
}

impl Builder {
    /// Takes term `t`, held by `rows` rows (more than 0), after every term
    /// before it. False once the listing is full.
    pub fn term(&mut self, t: &str, rows: u64) -> bool {
        let Some((path, value)) = json::decode(t) else { return true };
        if self.paths.last().is_none_or(|p| p.path != path) {
            if self.paths.len() == MAX_PATHS {
                self.truncated = true;
                return false;
            }
            if let Some(last) = self.paths.last_mut() {
                finish(last);
            }
            self.paths.push(PathStats { path: path.to_string(), distinct: Some(0), values: Some(Vec::new()), ..PathStats::default() });
        }
        let p = self.paths.last_mut().expect("a path");
        match value {
            None => p.rows += rows,
            Some(v) => {
                *p.distinct.as_mut().expect("counted") += 1;
                if let Value::Float(x) = v {
                    p.min = Some(p.min.map_or(x, |m| m.min(x)));
                    p.max = Some(p.max.map_or(x, |m| m.max(x)));
                }
                match &mut p.values {
                    Some(vs) if vs.len() < MAX_VALUES => vs.push((v, rows)),
                    _ => p.values = None,
                }
            }
        }
        true
    }

    pub fn finish(mut self, rows: u64) -> Paths {
        if let Some(last) = self.paths.last_mut() {
            finish(last);
        }
        Paths { rows, paths: self.paths, truncated: self.truncated }
    }
}

fn finish(p: &mut PathStats) {
    if let Some(vs) = &mut p.values {
        sort_values(vs);
    }
}

/// A `json_paths(...)` listing a query asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub table: String,
    pub field: String,
    /// List this path and those below it ([`json::parse_path`]).
    pub prefix: Option<String>,
    /// A SQL condition on the table's rows: list the paths of the rows it
    /// matches.
    pub filter: Option<String>,
}

const USAGE: &str = "SELECT * FROM json_paths('table', 'field' [, 'path'] [, filter => 'condition'])";

impl Call {
    /// The statement that asks for this listing (what a coordinator sends
    /// each shard).
    pub fn sql(&self) -> String {
        let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let mut args = vec![q(&self.table), q(&self.field)];
        args.extend(self.prefix.as_deref().map(q));
        args.extend(self.filter.as_deref().map(|f| format!("filter => {}", q(f))));
        format!("SELECT * FROM json_paths({})", args.join(", "))
    }

    /// The listing `sql` asks for, if it is a `SELECT * FROM
    /// json_paths(...)`; `None` for any other statement.
    pub fn of_sql(sql_text: &str) -> Result<Option<Call>> {
        if !sql_text.to_ascii_lowercase().contains("json_paths") {
            return Ok(None);
        }
        let Ok(sql::Statement::Query(q)) = super::sql::parse_one(sql_text) else { return Ok(None) };
        let SetExpr::Select(s) = q.body.as_ref() else { return Ok(None) };
        let [from] = s.from.as_slice() else { return Ok(None) };
        let TableFactor::Table { name, args: Some(args), .. } = &from.relation else { return Ok(None) };
        if !super::sql::table_name(name).eq_ignore_ascii_case("json_paths") {
            return Ok(None);
        }
        let plain = from.joins.is_empty()
            && q.with.is_none()
            && q.order_by.is_none()
            && q.limit_clause.is_none()
            && s.selection.is_none()
            && s.distinct.is_none()
            && s.having.is_none()
            && matches!(&s.group_by, sql::GroupByExpr::Expressions(g, _) if g.is_empty())
            && matches!(s.projection.as_slice(), [SelectItem::Wildcard(_)]);
        if !plain {
            return Err(err(format!("json_paths is read whole: {USAGE}")));
        }
        let text = |e: &sql::Expr| match e {
            sql::Expr::Value(v) => match &v.value {
                sql::Value::SingleQuotedString(s) => Ok(s.clone()),
                _ => Err(err(format!("json_paths takes names as strings, not {e}: {USAGE}"))),
            },
            sql::Expr::Identifier(id) => Ok(id.value.clone()),
            _ => Err(err(format!("json_paths takes names as strings, not {e}: {USAGE}"))),
        };
        let mut pos = Vec::new();
        let mut filter = None;
        let mut prefix = None;
        for a in &args.args {
            match a {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => pos.push(text(e)?),
                FunctionArg::Named { name, arg: FunctionArgExpr::Expr(e), .. } => match name.value.to_ascii_lowercase().as_str() {
                    "filter" => filter = Some(text(e)?),
                    "path" | "prefix" => prefix = Some(text(e)?),
                    n => return Err(err(format!("json_paths has no option {n}: {USAGE}"))),
                },
                a => return Err(err(format!("unsupported argument {a}: {USAGE}"))),
            }
        }
        let (table, field) = match pos.as_slice() {
            [t, f] => (t.clone(), f.clone()),
            [t, f, p] if prefix.is_none() => {
                prefix = Some(p.clone());
                (t.clone(), f.clone())
            }
            _ => return Err(err(format!("json_paths takes a table, a JSON field and a path: {USAGE}"))),
        };
        let prefix = prefix.filter(|p| !p.is_empty() && p != "$").map(|p| json::parse_path(&p)).transpose()?;
        Ok(Some(Call { table, field, prefix, filter: filter.filter(|f| !f.trim().is_empty()) }))
    }

    /// The field the listing reads, checked: a JSON field of `schema`,
    /// indexed.
    pub fn field_of(&self, schema: &crate::schema::Schema) -> Result<usize> {
        let f = schema.field_index(&self.field).ok_or_else(|| err(format!("no field {} in {}", self.field, schema.table)))?;
        let field = &schema.fields[f];
        if field.ty != crate::schema::FieldType::Json {
            return Err(err(format!("{} is not a JSON field: json_paths lists a JSON field's paths", field.name)));
        }
        if !field.indexed {
            return Err(err(format!("{} is not indexed: json_paths reads its index", field.name)));
        }
        Ok(f)
    }

    /// The runs of terms `[lo, hi)` the listing reads, in order: the
    /// prefix's own path, then those below it; every term without one.
    pub(crate) fn runs(&self) -> Vec<(String, Option<String>)> {
        match &self.prefix {
            None => vec![(String::new(), None)],
            Some(p) => vec![(json::has_term(p), Some(format!("{p}\u{1}"))), (format!("{p}."), Some(format!("{p}/")))],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(path: &str, rows: u64, values: Option<Vec<(Value, u64)>>) -> PathStats {
        PathStats { path: path.into(), rows, strings: rows, distinct: values.as_ref().map(|v| v.len() as u64), values, ..PathStats::default() }
    }

    #[test]
    fn calls_parse_and_print_back() {
        let c = Call::of_sql("SELECT * FROM json_paths('items', 'attrs')").unwrap().unwrap();
        assert_eq!(c, Call { table: "items".into(), field: "attrs".into(), prefix: None, filter: None });
        let c = Call::of_sql("select * from JSON_PATHS(items, attrs, '$.dims', filter => 'name = ''x''')").unwrap().unwrap();
        assert_eq!(c, Call { table: "items".into(), field: "attrs".into(), prefix: Some("dims".into()), filter: Some("name = 'x'".into()) });
        assert_eq!(Call::of_sql(&c.sql()).unwrap().unwrap(), c);
        assert_eq!(Call::of_sql("SELECT * FROM items").unwrap(), None);
        assert_eq!(Call::of_sql("SELECT json_paths FROM items").unwrap(), None);
        for bad in ["SELECT path FROM json_paths('a', 'b')", "SELECT * FROM json_paths('a')", "SELECT * FROM json_paths('a', 'b') WHERE rows > 1", "SELECT * FROM json_paths('a', 'b', nope => 'x')"] {
            assert!(Call::of_sql(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn builder_counts_paths_values_and_ranges() {
        let mut b = Builder::default();
        let terms = [
            (json::has_term("a"), 3),
            (json::term("a", &Value::Bool(true)).unwrap(), 1),
            (json::term("a", &Value::Float(-2.5)).unwrap(), 1),
            (json::term("a", &Value::Int(7)).unwrap(), 2),
            (json::term("a", &Value::Str("x".into())).unwrap(), 2),
            (json::has_term("a.b"), 1),
        ];
        let mut sorted = terms.to_vec();
        sorted.sort();
        assert_eq!(sorted, terms.to_vec(), "terms come in this order");
        for (t, n) in &terms {
            assert!(b.term(t, *n));
        }
        let p = b.finish(4);
        assert_eq!(p.rows, 4);
        assert_eq!(p.paths.len(), 2);
        let a = &p.paths[0];
        assert_eq!((a.rows, a.min, a.max, a.distinct), (3, Some(-2.5), Some(7.0), Some(4)));
        assert_eq!(a.values.as_ref().unwrap()[..2], [(Value::Float(7.0), 2), (Value::Str("x".into()), 2)]);
        assert_eq!(p.paths[1], PathStats { path: "a.b".into(), rows: 1, distinct: Some(0), values: Some(Vec::new()), ..PathStats::default() });
        let back = Paths::from_result(&p.to_result()).unwrap();
        // Kinds are counted apart ([`Builder::kinds`]); the rest round trips.
        assert_eq!(back, p);
    }

    #[test]
    fn many_values_are_counted_and_many_paths_cut() {
        let mut b = Builder::default();
        assert!(b.term(&json::has_term("a"), 200));
        for i in 0..=MAX_VALUES {
            assert!(b.term(&json::term("a", &Value::Int(i as i64)).unwrap(), 1));
        }
        let p = b.finish(200);
        assert_eq!((p.paths[0].distinct, p.paths[0].values.is_none(), p.paths[0].max), (Some(MAX_VALUES as u64 + 1), true, Some(MAX_VALUES as f64)));
        // Through a coordinator, a shard that does not list them makes the
        // count unknown.
        let other = Paths { rows: 1, paths: vec![stats("a", 1, Some(vec![(Value::Str("x".into()), 1)]))], truncated: false };
        let m = Paths::merge(vec![p, other]);
        assert_eq!((m.paths[0].rows, m.paths[0].distinct, m.paths[0].values.is_none()), (201, None, true));
        let mut b = Builder::default();
        let full = (0..=MAX_PATHS).map(|i| format!("k{i:05}")).take_while(|p| b.term(&json::has_term(p), 1)).count();
        let l = b.finish(1);
        assert_eq!((full, l.paths.len(), l.truncated), (MAX_PATHS, MAX_PATHS, true));
        assert!(Paths::from_result(&l.to_result()).unwrap().truncated);
    }

    #[test]
    fn merges_add_up_and_cut_where_a_part_was_cut() {
        let one = Paths { rows: 10, paths: vec![stats("a", 2, Some(vec![(Value::Str("x".into()), 2)])), stats("c", 1, None)], truncated: false };
        let two = Paths { rows: 5, paths: vec![stats("a", 3, Some(vec![(Value::Str("x".into()), 1), (Value::Str("y".into()), 2)])), stats("b", 1, Some(vec![]))], truncated: false };
        let m = Paths::merge(vec![one.clone(), two.clone()]);
        assert_eq!(m.rows, 15);
        assert_eq!(m.paths.iter().map(|p| (p.path.as_str(), p.rows, p.distinct)).collect::<Vec<_>>(), [("a", 5, Some(2)), ("b", 1, Some(0)), ("c", 1, None)]);
        assert_eq!(m.paths[0].values.as_ref().unwrap(), &[(Value::Str("x".into()), 3), (Value::Str("y".into()), 2)]);
        // A part cut after "a" leaves out what follows it.
        let cut = Paths { truncated: true, paths: vec![stats("a", 1, Some(vec![]))], rows: 1 };
        let m = Paths::merge(vec![cut, two]);
        assert!(m.truncated);
        assert_eq!(m.paths.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(), ["a"]);
    }
}
