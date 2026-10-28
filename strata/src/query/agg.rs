//! Aggregates: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` and `COUNT(DISTINCT)`,
//! over a whole result or per group, with `HAVING` and ordering of groups.
//!
//! The executor accumulates rows segment by segment ([`Groups`]); shards of
//! a split collection return partial accumulators (an average as its sum
//! and count) that [`crate::cluster::merge`] combines before [`finish`].

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use crate::error::{Error, Result};
use crate::query::logical::{AggFn, Having, OrderBy, Projection, Select, Tiebreak};
use crate::value::Value;

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// A hashable group value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Part {
    Null,
    Int(i64),
    /// Bits of an `f64` (no NaNs reach a group).
    Float(u64),
    Str(String),
}

impl Part {
    pub fn of(v: &Value) -> Part {
        match v {
            Value::Int(x) => Part::Int(*x),
            Value::Float(x) => Part::Float(x.to_bits()),
            Value::Str(s) => Part::Str(s.clone()),
            _ => Part::Null,
        }
    }

    pub fn value(&self) -> Value {
        match self {
            Part::Null => Value::Null,
            Part::Int(x) => Value::Int(*x),
            Part::Float(b) => Value::Float(f64::from_bits(*b)),
            Part::Str(s) => Value::Str(s.clone()),
        }
    }
}

/// One aggregate's running state.
#[derive(Clone, Debug)]
pub enum Acc {
    /// `COUNT(*)` (rows) or `COUNT(f)` (rows with a value).
    Count(i64),
    /// The sum of the FLOAT values, of the INT values (exactly: an i128
    /// cannot overflow), whether every value was an integer, and whether
    /// any was seen.
    Sum { sum: f64, isum: i128, int: bool, any: bool },
    Avg { sum: f64, n: i64 },
    Min(Option<Value>),
    Max(Option<Value>),
    Distinct(BTreeSet<Part>),
}

impl Acc {
    pub fn new(p: &Projection) -> Acc {
        match p {
            Projection::Agg { func, .. } => match func {
                AggFn::Count => Acc::Count(0),
                AggFn::Sum => Acc::Sum { sum: 0.0, isum: 0, int: true, any: false },
                AggFn::Avg => Acc::Avg { sum: 0.0, n: 0 },
                AggFn::Min => Acc::Min(None),
                AggFn::Max => Acc::Max(None),
                AggFn::CountDistinct => Acc::Distinct(BTreeSet::new()),
            },
            _ => Acc::Count(0),
        }
    }

    /// Adds one row (`COUNT(*)`).
    pub fn add_row(&mut self) {
        if let Acc::Count(n) = self {
            *n += 1;
        }
    }

    /// Adds one value of the aggregated field.
    pub fn add(&mut self, v: &Value) {
        if matches!(v, Value::Null) {
            return;
        }
        match self {
            Acc::Count(n) => *n += 1,
            Acc::Sum { sum, isum, int, any } => match v {
                Value::Int(x) => {
                    *isum += *x as i128;
                    *any = true;
                }
                v => {
                    if let Some(x) = v.as_f64() {
                        *sum += x;
                        *int = false;
                        *any = true;
                    }
                }
            },
            Acc::Avg { sum, n } => {
                if let Some(x) = v.as_f64() {
                    *sum += x;
                    *n += 1;
                }
            }
            Acc::Min(m) => {
                if m.as_ref().is_none_or(|m| cmp(v, m) == Ordering::Less) {
                    *m = Some(v.clone());
                }
            }
            Acc::Max(m) => {
                if m.as_ref().is_none_or(|m| cmp(v, m) == Ordering::Greater) {
                    *m = Some(v.clone());
                }
            }
            Acc::Distinct(s) => {
                s.insert(Part::of(v));
            }
        }
    }

    /// The final value.
    pub fn value(&self) -> Value {
        match self {
            Acc::Count(n) => Value::Int(*n),
            Acc::Sum { any: false, .. } => Value::Null,
            // A sum of integers beyond the 64-bit range is a FLOAT rather
            // than a wrapped integer.
            Acc::Sum { isum, int: true, .. } => i64::try_from(*isum).map_or(Value::Float(*isum as f64), Value::Int),
            Acc::Sum { sum, isum, .. } => Value::Float(sum + *isum as f64),
            Acc::Avg { n: 0, .. } => Value::Null,
            Acc::Avg { sum, n } => Value::Float(sum / *n as f64),
            Acc::Min(m) | Acc::Max(m) => m.clone().unwrap_or(Value::Null),
            Acc::Distinct(s) => Value::Int(s.len() as i64),
        }
    }

    /// The state as one value, for a shard's answer: an average is
    /// `[sum, n]`, a distinct count the list of values, a sum `[sum, int,
    /// any, integer sum]` (the last as decimal text: it can exceed 64 bits).
    pub fn partial(&self) -> Value {
        match self {
            Acc::Sum { sum, isum, int, any } => Value::List(vec![Value::Float(*sum), Value::Int(*int as i64), Value::Int(*any as i64), Value::Str(isum.to_string())]),
            Acc::Avg { sum, n } => Value::List(vec![Value::Float(*sum), Value::Int(*n)]),
            Acc::Distinct(s) => Value::List(s.iter().map(Part::value).collect()),
            a => a.value(),
        }
    }

    /// Adds a [`partial`](Self::partial) of the same aggregate.
    pub fn merge(&mut self, p: &Value) -> Result<()> {
        let bad = || err("a shard's partial aggregate does not match the query");
        match (self, p) {
            (Acc::Count(n), v) => *n += v.as_i64().ok_or_else(bad)?,
            (Acc::Sum { sum, isum, int, any }, Value::List(xs)) if xs.len() == 4 => {
                *sum += xs[0].as_f64().ok_or_else(bad)?;
                *int &= xs[1].as_i64() == Some(1);
                *any |= xs[2].as_i64() == Some(1);
                *isum += xs[3].as_str().and_then(|s| s.parse::<i128>().ok()).ok_or_else(bad)?;
            }
            (Acc::Avg { sum, n }, Value::List(xs)) if xs.len() == 2 => {
                *sum += xs[0].as_f64().ok_or_else(bad)?;
                *n += xs[1].as_i64().ok_or_else(bad)?;
            }
            (a @ (Acc::Min(_) | Acc::Max(_)), v) => a.add(v),
            (Acc::Distinct(s), Value::List(xs)) => s.extend(xs.iter().map(Part::of)),
            _ => return Err(bad()),
        }
        Ok(())
    }
}

/// Orders values of one column: numbers by value, strings by bytes, NULL first.
pub fn cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Int(x), Value::Float(y)) => cmp_int_float(*x, *y),
        (Value::Float(x), Value::Int(y)) => cmp_int_float(*y, *x).reverse(),
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => format!("{a:?}").cmp(&format!("{b:?}")),
        },
    }
}

/// The exact order of an integer and a double (converting the integer
/// would round 2^53 + 1 to 2^53). NaN sorts last, as in `total_cmp`.
fn cmp_int_float(x: i64, y: f64) -> Ordering {
    const TWO63: f64 = 9_223_372_036_854_775_808.0;
    if y.is_nan() || y >= TWO63 {
        return Ordering::Less;
    }
    if y < -TWO63 {
        return Ordering::Greater;
    }
    let f = y.floor();
    match x.cmp(&(f as i64)) {
        Ordering::Equal if y > f => Ordering::Less,
        o => o,
    }
}

/// Every aggregate the query computes: those it selects, filters on
/// (`HAVING`) and orders by, each once, in that order.
pub fn needed(q: &Select) -> Vec<Projection> {
    let mut out: Vec<Projection> = Vec::new();
    let mut add = |p: &Projection| {
        if matches!(p, Projection::Count | Projection::Agg { .. }) && !out.contains(p) {
            out.push(p.clone());
        }
    };
    for (_, p) in &q.projection {
        add(p);
    }
    if let Some(h) = &q.having {
        h.visit(&mut add);
    }
    if let Some(OrderBy::Agg { what, .. }) = &q.order {
        add(what);
    }
    // Groups come by count without an ORDER BY.
    if matches!(q.order, Some(OrderBy::Count { .. })) || (q.order.is_none() && !q.group_by.is_empty()) {
        add(&Projection::Count);
    }
    out
}

/// Whether the query takes the general aggregate path (anything beyond
/// counting the values of one keyword field, which facets answer).
pub fn is_general(q: &Select, facet_field: impl Fn(usize) -> bool) -> bool {
    let facet = q.group_by.len() == 1
        && facet_field(q.group_by[0])
        && q.having.is_none()
        && q.projection.iter().all(|(_, p)| matches!(p, Projection::Count) || *p == Projection::Field(q.group_by[0]))
        && matches!(q.order, None | Some(OrderBy::Count { .. }) | Some(OrderBy::GroupValue { .. }));
    let plain_count = q.group_by.is_empty() && q.having.is_none() && q.projection.iter().all(|(_, p)| matches!(p, Projection::Count));
    !facet && !plain_count && (!q.group_by.is_empty() || q.projection.iter().any(|(_, p)| matches!(p, Projection::Agg { .. })) || q.having.is_some())
}

/// Groups and their accumulators (one per [`needed`] aggregate).
#[derive(Default)]
pub struct Groups {
    pub map: HashMap<Vec<Part>, Vec<Acc>>,
}

impl Groups {
    pub fn entry(&mut self, key: Vec<Part>, needed: &[Projection]) -> &mut Vec<Acc> {
        self.map.entry(key).or_insert_with(|| needed.iter().map(Acc::new).collect())
    }

    /// Shard-mode rows: the group values, then each needed aggregate's partial.
    pub fn partial_rows(self) -> Vec<Vec<Value>> {
        self.map.into_iter().map(|(k, accs)| k.iter().map(Part::value).chain(accs.iter().map(Acc::partial)).collect()).collect()
    }

    /// Adds shard-mode rows of `group_cols` group values followed by partials.
    pub fn merge_rows(&mut self, rows: &[Vec<Value>], group_cols: usize, needed: &[Projection]) -> Result<()> {
        for r in rows {
            if r.len() != group_cols + needed.len() {
                return Err(err("a shard's aggregate row has the wrong width"));
            }
            let accs = self.entry(r[..group_cols].iter().map(Part::of).collect(), needed);
            for (a, v) in accs.iter_mut().zip(&r[group_cols..]) {
                a.merge(v)?;
            }
        }
        Ok(())
    }
}

/// Applies `HAVING`, the order, `OFFSET` and `LIMIT`, and projects each
/// group. A query without `GROUP BY` has one group, even over no rows.
pub fn finish(q: &Select, mut groups: Groups, needed: &[Projection]) -> Vec<Vec<Value>> {
    if q.group_by.is_empty() && groups.map.is_empty() {
        groups.entry(Vec::new(), needed);
    }
    let value_of = |key: &[Part], accs: &[Acc], p: &Projection| -> Value {
        match p {
            Projection::Field(f) => q.group_by.iter().position(|g| g == f).map_or(Value::Null, |i| key[i].value()),
            _ => needed.iter().position(|n| n == p).map_or(Value::Null, |i| accs[i].value()),
        }
    };
    let mut rows: Vec<(Vec<Part>, Vec<Acc>)> =
        groups.map.into_iter().filter(|(k, a)| q.having.as_ref().is_none_or(|h| h.eval(&|p| value_of(k, a, p)))).collect();
    let value_desc = matches!(q.tiebreak, Some(Tiebreak::GroupValue { desc: true }));
    let by_key = |a: &[Part], b: &[Part]| if value_desc { b.cmp(a) } else { a.cmp(b) };
    let primary: Option<(Projection, bool)> = match &q.order {
        Some(OrderBy::Count { desc }) => Some((Projection::Count, *desc)),
        Some(OrderBy::GroupValue { desc }) => q.group_by.first().map(|g| (Projection::Field(*g), *desc)),
        Some(OrderBy::Agg { what, desc }) => Some((what.clone(), *desc)),
        // Without an ORDER BY, the largest groups first.
        None if !q.group_by.is_empty() => Some((Projection::Count, true)),
        _ => None,
    };
    rows.sort_by(|(ka, aa), (kb, ab)| {
        let o = match &primary {
            Some((p, desc)) => {
                let o = cmp(&value_of(ka, aa, p), &value_of(kb, ab, p));
                if *desc { o.reverse() } else { o }
            }
            None => Ordering::Equal,
        };
        o.then_with(|| by_key(ka, kb))
    });
    rows.into_iter()
        .skip(q.offset)
        .take(q.limit.unwrap_or(usize::MAX))
        .map(|(k, a)| q.projection.iter().map(|(_, p)| value_of(&k, &a, p)).collect())
        .collect()
}

impl Having {
    fn visit(&self, f: &mut impl FnMut(&Projection)) {
        match self {
            Having::Range { what, .. } | Having::Eq { what, .. } => f(what),
            Having::And(hs) | Having::Or(hs) => hs.iter().for_each(|h| h.visit(f)),
            Having::Not(h) => h.visit(f),
        }
    }

    pub fn eval(&self, value: &dyn Fn(&Projection) -> Value) -> bool {
        match self {
            Having::Range { what, lo, hi } => value(what).as_f64().is_some_and(|x| *lo <= x && x <= *hi),
            Having::Eq { what, value: v } => cmp(&value(what), v) == Ordering::Equal,
            Having::And(hs) => hs.iter().all(|h| h.eval(value)),
            Having::Or(hs) => hs.iter().any(|h| h.eval(value)),
            Having::Not(h) => !h.eval(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulators_and_partials() {
        let sum = Projection::Agg { func: AggFn::Sum, field: 0 };
        let avg = Projection::Agg { func: AggFn::Avg, field: 0 };
        let dis = Projection::Agg { func: AggFn::CountDistinct, field: 0 };
        let (mut a, mut b, mut c) = (Acc::new(&sum), Acc::new(&avg), Acc::new(&dis));
        for v in [Value::Int(2), Value::Int(4), Value::Null, Value::Int(4)] {
            a.add(&v);
            b.add(&v);
            c.add(&v);
        }
        assert_eq!(a.value(), Value::Int(10));
        assert_eq!(b.value(), Value::Float(10.0 / 3.0));
        assert_eq!(c.value(), Value::Int(2));
        // Merging partials gives the aggregate of the union.
        let mut b2 = Acc::new(&avg);
        b2.add(&Value::Float(1.0));
        b2.merge(&b.partial()).unwrap();
        assert_eq!(b2.value(), Value::Float(11.0 / 4.0));
        let mut c2 = Acc::new(&dis);
        c2.add(&Value::Int(7));
        c2.merge(&c.partial()).unwrap();
        assert_eq!(c2.value(), Value::Int(3));
        assert_eq!(Acc::new(&avg).value(), Value::Null);
    }
}
