//! Aggregates: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX` and `COUNT(DISTINCT)`,
//! over a whole result or per group, with `HAVING` and ordering of groups.
//!
//! The executor accumulates rows segment by segment ([`Groups`]); shards of
//! a split collection return partial accumulators (an average as its sum
//! and count) that [`crate::cluster::merge`] combines before [`finish`].

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use crate::error::{Error, Result};
use crate::query::ir::eval::{self, Ref};
use crate::query::ir::{lower, Expr};
use crate::query::memory::Charge;
use crate::query::logical::{AggFn, Col, Having, OrderBy, Projection, Select, Tiebreak};
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
    /// A boolean group value ([`Part::group`]).
    Bool(bool),
}

impl Part {
    /// A BOOL or TIMESTAMP value as the integer it is stored as.
    pub fn of(v: &Value) -> Part {
        match v {
            Value::Int(x) | Value::Timestamp(x) => Part::Int(*x),
            Value::Bool(b) => Part::Int(*b as i64),
            Value::Float(x) => Part::Float(x.to_bits()),
            Value::Str(s) => Part::Str(s.clone()),
            _ => Part::Null,
        }
    }

    /// A value as a group of GROUP BY: a boolean stays one (a JSON path's
    /// `true` is not the group of the number 1), else as [`Part::of`].
    pub fn group(v: &Value) -> Part {
        match v {
            Value::Bool(b) => Part::Bool(*b),
            v => Part::of(v),
        }
    }

    pub fn value(&self) -> Value {
        match self {
            Part::Null => Value::Null,
            Part::Int(x) => Value::Int(*x),
            Part::Float(b) => Value::Float(f64::from_bits(*b)),
            Part::Str(s) => Value::Str(s.clone()),
            Part::Bool(b) => Value::Bool(*b),
        }
    }

    /// About the bytes it holds, in place and on the heap.
    pub fn bytes(&self) -> u64 {
        let heap = match self {
            Part::Str(s) => s.len() as u64,
            _ => 0,
        };
        std::mem::size_of::<Part>() as u64 + heap
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
    /// The FLOAT values' sum, the INT values' exact sum, and the values
    /// counted: an average of integers is their exact sum divided once.
    Avg { sum: f64, isum: i128, n: i64 },
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
                AggFn::Avg => Acc::Avg { sum: 0.0, isum: 0, n: 0 },
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

    /// Adds one value of the aggregated field; returns about the bytes it
    /// now holds more (a value `COUNT(DISTINCT)` had not seen).
    pub fn add(&mut self, v: &Value) -> u64 {
        if matches!(v, Value::Null) {
            return 0;
        }
        match self {
            Acc::Count(n) => *n += 1,
            Acc::Sum { sum, isum, int, any } => match v {
                Value::Int(_) | Value::Bool(_) => {
                    *isum += v.as_i64().unwrap_or(0) as i128;
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
            Acc::Avg { sum, isum, n } => match v {
                Value::Int(_) | Value::Bool(_) => {
                    *isum += v.as_i64().unwrap_or(0) as i128;
                    *n += 1;
                }
                v => {
                    if let Some(x) = v.as_f64() {
                        *sum += x;
                        *n += 1;
                    }
                }
            },
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
                let p = Part::of(v);
                let bytes = 32 + p.bytes();
                if s.insert(p) {
                    return bytes;
                }
            }
        }
        0
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
            Acc::Avg { sum, isum, n } => Value::Float((sum + *isum as f64) / *n as f64),
            Acc::Min(m) | Acc::Max(m) => m.clone().unwrap_or(Value::Null),
            Acc::Distinct(s) => Value::Int(s.len() as i64),
        }
    }

    /// The state as one value, for a shard's answer: an average is
    /// `[sum, n, integer sum]`, a distinct count the list of values, a sum
    /// `[sum, int, any, integer sum]` (integer sums as decimal text: they
    /// can exceed 64 bits).
    pub fn partial(&self) -> Value {
        match self {
            Acc::Sum { sum, isum, int, any } => Value::List(vec![Value::Float(*sum), Value::Int(*int as i64), Value::Int(*any as i64), Value::Str(isum.to_string())]),
            Acc::Avg { sum, isum, n } => Value::List(vec![Value::Float(*sum), Value::Int(*n), Value::Str(isum.to_string())]),
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
            // An older shard's average is `[sum, n]`, its integers in `sum`.
            (Acc::Avg { sum, isum, n }, Value::List(xs)) if xs.len() == 2 || xs.len() == 3 => {
                *sum += xs[0].as_f64().ok_or_else(bad)?;
                *n += xs[1].as_i64().ok_or_else(bad)?;
                if let Some(x) = xs.get(2) {
                    *isum += x.as_str().and_then(|s| s.parse::<i128>().ok()).ok_or_else(bad)?;
                }
            }
            (a @ (Acc::Min(_) | Acc::Max(_)), v) => {
                a.add(v);
            }
            (Acc::Distinct(s), Value::List(xs)) => s.extend(xs.iter().map(Part::of)),
            _ => return Err(bad()),
        }
        Ok(())
    }
}

/// Orders values of one column: numbers by value (booleans as 1 and 0,
/// timestamps by time, also against text naming a time), strings by bytes,
/// NULL first.
pub fn cmp(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Int(x) | Value::Timestamp(x), Value::Int(y) | Value::Timestamp(y)) => x.cmp(y),
        // A time written as text compares as the time it names.
        (Value::Timestamp(x), Value::Str(s)) if crate::timestamp::parse(s).is_some() => x.cmp(&crate::timestamp::parse(s).unwrap()),
        (Value::Str(s), Value::Timestamp(y)) if crate::timestamp::parse(s).is_some() => crate::timestamp::parse(s).unwrap().cmp(y),
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
pub(crate) fn cmp_int_float(x: i64, y: f64) -> Ordering {
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
pub fn is_general(q: &Select, facet_field: impl Fn(Col) -> bool) -> bool {
    let facet = q.group_by.len() == 1
        && facet_field(q.group_by[0])
        && q.having.is_none()
        && q.projection.iter().all(|(_, p)| matches!(p, Projection::Count) || *p == q.group_by[0].projection())
        && matches!(q.order, None | Some(OrderBy::Count { .. }) | Some(OrderBy::GroupValue { .. }));
    let plain_count = q.group_by.is_empty() && q.having.is_none() && q.projection.iter().all(|(_, p)| matches!(p, Projection::Count));
    !facet && !plain_count && (!q.group_by.is_empty() || q.projection.iter().any(|(_, p)| matches!(p, Projection::Agg { .. })) || q.having.is_some())
}

/// Groups and their accumulators (one per [`needed`] aggregate).
#[derive(Default)]
pub struct Groups {
    pub map: HashMap<Vec<Part>, Vec<Acc>>,
    /// The most groups there may be ([`crate::query::exec::ExecOptions::max_groups`]).
    pub max: Option<usize>,
    /// What new groups are counted against.
    pub charge: Charge,
}

/// The error of a query that makes more than `max` groups.
pub fn too_many_groups(max: usize) -> Error {
    err(format!("the query makes more than {max} groups, the most a query may; add conditions or group by fewer values"))
}

impl Groups {
    /// No groups yet, and at most `max` of them.
    pub fn limited(max: Option<usize>) -> Groups {
        Groups { map: HashMap::new(), max, charge: Charge::default() }
    }

    /// The accumulators of group `key`, a new group's if there is room for
    /// one.
    pub fn entry(&mut self, key: Vec<Part>, needed: &[Projection]) -> Result<&mut Vec<Acc>> {
        let n = self.map.len();
        match self.map.entry(key) {
            std::collections::hash_map::Entry::Occupied(e) => Ok(e.into_mut()),
            std::collections::hash_map::Entry::Vacant(e) => {
                if let Some(max) = self.max.filter(|&m| n >= m) {
                    return Err(too_many_groups(max));
                }
                let key: u64 = e.key().iter().map(Part::bytes).sum();
                self.charge.add(64 + key + (needed.len() * std::mem::size_of::<Acc>()) as u64)?;
                Ok(e.insert(needed.iter().map(Acc::new).collect()))
            }
        }
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
            let accs = self.entry(r[..group_cols].iter().map(Part::group).collect(), needed)?;
            for (a, v) in accs.iter_mut().zip(&r[group_cols..]) {
                a.merge(v)?;
            }
        }
        Ok(())
    }
}

/// Applies `HAVING`, the order, `OFFSET` and `LIMIT`, and projects each
/// group. A query without `GROUP BY` has one group, even over no rows.
/// `HAVING` keeps the groups for which it is TRUE, in the one evaluator of
/// plan expressions ([`crate::query::ir::eval`]): an aggregate of no
/// values is NULL, and a comparison with it is neither true nor false.
pub fn finish(q: &Select, mut groups: Groups, needed: &[Projection]) -> Result<Vec<Vec<Value>>> {
    if q.group_by.is_empty() && groups.map.is_empty() {
        groups.map.insert(Vec::new(), needed.iter().map(Acc::new).collect());
    }
    let value_of = |key: &[Part], accs: &[Acc], p: &Projection| -> Value {
        match p {
            Projection::Field(_) | Projection::Path(_) => q.group_by.iter().position(|g| g.projection() == *p).map_or(Value::Null, |i| key[i].value()),
            _ => needed.iter().position(|n| n == p).map_or(Value::Null, |i| accs[i].value()),
        }
    };
    // The values HAVING names, as columns of the group's row.
    let mut columns: Vec<Projection> = Vec::new();
    let having = q.having.as_ref().map(|h| {
        lower::having(h, &mut |p| {
            let i = columns.iter().position(|c| c == p).unwrap_or_else(|| {
                columns.push(p.clone());
                columns.len() - 1
            });
            Expr::Output(i as u32)
        })
    });
    let mut rows: Vec<(Vec<Part>, Vec<Acc>)> = Vec::with_capacity(groups.map.len());
    for (k, a) in groups.map {
        let keep = match &having {
            None => true,
            Some(h) => eval::holds(h, &|r| match r {
                Ref::Output(i) => Ok(value_of(&k, &a, &columns[i as usize])),
                r => Err(err(format!("HAVING reads {r:?}, which a group does not have"))),
            })?,
        };
        if keep {
            rows.push((k, a));
        }
    }
    let value_desc = matches!(q.tiebreak, Some(Tiebreak::GroupValue { desc: true }));
    // Group values in the order values have (a part's order puts a
    // negative double after a positive one, and a JSON path's values may be
    // of several types).
    let by_key = |a: &[Part], b: &[Part]| {
        let o = a.iter().zip(b).map(|(x, y)| cmp(&x.value(), &y.value())).find(|o| o.is_ne()).unwrap_or(Ordering::Equal);
        if value_desc { o.reverse() } else { o }
    };
    let primary: Option<(Projection, bool)> = match &q.order {
        Some(OrderBy::Count { desc }) => Some((Projection::Count, *desc)),
        Some(OrderBy::GroupValue { desc }) => q.group_by.first().map(|g| (g.projection(), *desc)),
        Some(OrderBy::Agg { what, desc }) => Some((what.clone(), *desc)),
        // Without an ORDER BY, the largest groups first.
        None if !q.group_by.is_empty() => Some((Projection::Count, true)),
        _ => None,
    };
    rows.sort_by(|(ka, aa), (kb, ab)| {
        let o = match &primary {
            // NULL (the group of rows without a value, an aggregate of no
            // values) last in either direction, as rows are.
            Some((p, desc)) => match (value_of(ka, aa, p), value_of(kb, ab, p)) {
                (Value::Null, Value::Null) => Ordering::Equal,
                (Value::Null, _) => Ordering::Greater,
                (_, Value::Null) => Ordering::Less,
                (x, y) if *desc => cmp(&x, &y).reverse(),
                (x, y) => cmp(&x, &y),
            },
            None => Ordering::Equal,
        };
        o.then_with(|| by_key(ka, kb))
    });
    Ok(rows
        .into_iter()
        .skip(q.offset)
        .take(q.limit.unwrap_or(usize::MAX))
        .map(|(k, a)| q.projection.iter().map(|(_, p)| value_of(&k, &a, p)).collect())
        .collect())
}

impl Having {
    fn visit(&self, f: &mut impl FnMut(&Projection)) {
        match self {
            Having::Range { what, .. } | Having::Eq { what, .. } => f(what),
            Having::And(hs) | Having::Or(hs) => hs.iter().for_each(|h| h.visit(f)),
            Having::Not(h) => h.visit(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulators_and_partials() {
        let sum = Projection::Agg { func: AggFn::Sum, of: Col::Field(0) };
        let avg = Projection::Agg { func: AggFn::Avg, of: Col::Field(0) };
        let dis = Projection::Agg { func: AggFn::CountDistinct, of: Col::Field(0) };
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

    /// Review BUG-10: AVG of INT summed in f64, so integers beyond 2^53
    /// lost their low bits (`SUM(n) = 3` with `AVG(n) = 0.0`).
    #[test]
    fn avg_of_integers_sums_them_exactly() {
        let avg = Projection::Agg { func: AggFn::Avg, of: Col::Field(0) };
        let big = (1i64 << 53) + 1;
        let values = [Value::Int(big), Value::Int(big), Value::Int(-2 * big + 2), Value::Int(1)];
        let mut a = Acc::new(&avg);
        values.iter().for_each(|v| _ = a.add(v));
        assert_eq!(a.value(), Value::Float(0.75));
        // Over shards: the integer sum travels exactly, and an older
        // shard's `[sum, n]` still merges.
        let (mut s1, mut s2) = (Acc::new(&avg), Acc::new(&avg));
        values[..2].iter().for_each(|v| _ = s1.add(v));
        values[2..].iter().for_each(|v| _ = s2.add(v));
        let mut m = Acc::new(&avg);
        m.merge(&s1.partial()).unwrap();
        m.merge(&s2.partial()).unwrap();
        assert_eq!(m.value(), Value::Float(0.75));
        m.merge(&Value::List(vec![Value::Float(1.0), Value::Int(4)])).unwrap();
        assert_eq!(m.value(), Value::Float(4.0 / 8.0));
        // FLOAT values and integers together.
        let mut f = Acc::new(&avg);
        [Value::Float(0.5), Value::Int(2), Value::Null].iter().for_each(|v| _ = f.add(v));
        assert_eq!(f.value(), Value::Float(1.25));
    }
}
