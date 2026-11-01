//! Window functions over rows in memory ([`crate::query::ir::Node::Window`]),
//! as Strata's operators run them ([`crate::query::ops`]): each function's
//! value for each row, over the rows of the row's partition in the
//! function's order.
//!
//! A partition is the rows with equal values of its keys (NULL equal to
//! NULL, numbers by value whatever their type). Its rows are sorted by the
//! order's keys, stably, so rows that tie keep the input's order, and rows
//! tie (are peers) when every key compares equal. Frames follow SQL:
//! `ROWS` counts rows from the row, `RANGE` takes the rows whose one
//! numeric key lies within the offsets of the row's (a row without a value
//! has its peers, the other rows without one), and `CURRENT ROW` in
//! `RANGE` is the row's first or last peer. An aggregate over a frame that
//! starts at the partition's first row, or ends at its last, adds each row
//! once; other frames are added up row by row.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::query::agg::{self, Acc};
use crate::query::ir::eval::{self, Ref};
use crate::query::ir::{Bound, Expr, Frame, FrameUnits, SortKey, WindowFn, WindowFunc};
use crate::query::Projection;
use crate::value::Value;

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

fn value(e: &Expr, row: &[Value]) -> Result<Value> {
    eval::eval(e, &|r| match r {
        Ref::Output(i) => row.get(i as usize).cloned().ok_or_else(|| err(format!("no column {i} in rows of {} columns", row.len()))),
        r => Err(err(format!("{r:?} is not a column of rows in memory"))),
    })
}

/// `a` against `b` as sort key `k` orders them: NULLs last unless
/// `nulls_first`, whatever the direction.
pub(crate) fn compare(a: &Value, b: &Value, k: &SortKey) -> Ordering {
    match (matches!(a, Value::Null), matches!(b, Value::Null)) {
        (true, true) => Ordering::Equal,
        (true, false) if k.nulls_first => Ordering::Less,
        (true, false) => Ordering::Greater,
        (false, true) if k.nulls_first => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let c = agg::cmp(a, b);
            if k.desc { c.reverse() } else { c }
        }
    }
}

/// The values of each of `funcs` for each of `rows`: a column per
/// function, a value per row, in the rows' order. `tick` runs every row
/// a function reads, for the query's deadline.
pub(crate) fn evaluate(rows: &[Vec<Value>], funcs: &[WindowFn], tick: &mut dyn FnMut() -> Result<()>) -> Result<Vec<Vec<Value>>> {
    funcs.iter().map(|w| one(rows, w, tick)).collect()
}

fn one(rows: &[Vec<Value>], w: &WindowFn, tick: &mut dyn FnMut() -> Result<()>) -> Result<Vec<Value>> {
    // The partitions, each its rows' positions in the input's order.
    let mut at: HashMap<Vec<agg::Part>, usize> = HashMap::new();
    let mut parts: Vec<Vec<usize>> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        tick()?;
        let key = w.partition.iter().map(|p| value(p, r).map(|v| crate::query::ops::part(&v))).collect::<Result<Vec<_>>>()?;
        let p = *at.entry(key).or_insert_with(|| {
            parts.push(Vec::new());
            parts.len() - 1
        });
        parts[p].push(i);
    }
    let mut out = vec![Value::Null; rows.len()];
    for part in parts {
        let keys: Vec<Vec<Value>> = part.iter().map(|&i| w.order.iter().map(|k| value(&k.expr, &rows[i])).collect()).collect::<Result<_>>()?;
        let mut idx: Vec<usize> = (0..part.len()).collect();
        let cmp = |a: usize, b: usize| w.order.iter().enumerate().map(|(j, k)| compare(&keys[a][j], &keys[b][j], k)).find(|o| o.is_ne()).unwrap_or(Ordering::Equal);
        idx.sort_by(|&a, &b| cmp(a, b));
        // The rows of the partition in order: their input positions and
        // their order's values.
        let sorted: Vec<usize> = idx.iter().map(|&j| part[j]).collect();
        let keys: Vec<&[Value]> = idx.iter().map(|&j| keys[j].as_slice()).collect();
        let peer = |a: usize, b: usize| w.order.iter().enumerate().all(|(j, k)| compare(&keys[a][j], &keys[b][j], k).is_eq());
        // Each row's first and last peer (+1).
        let n = sorted.len();
        let (mut first, mut last) = (vec![0; n], vec![0; n]);
        let mut s = 0;
        while s < n {
            let mut e = s + 1;
            while e < n && peer(s, e) {
                e += 1;
            }
            first[s..e].iter_mut().for_each(|x| *x = s);
            last[s..e].iter_mut().for_each(|x| *x = e);
            s = e;
        }
        let vals = values(rows, &sorted, w, &keys, &first, &last, tick)?;
        for (&i, v) in sorted.iter().zip(vals) {
            out[i] = v;
        }
    }
    Ok(out)
}

/// The function's value for each row of one partition, in its order.
#[allow(clippy::too_many_arguments)]
fn values(rows: &[Vec<Value>], sorted: &[usize], w: &WindowFn, keys: &[&[Value]], first: &[usize], last: &[usize], tick: &mut dyn FnMut() -> Result<()>) -> Result<Vec<Value>> {
    let n = sorted.len();
    let arg = |e: &Expr, at: usize| value(e, &rows[sorted[at]]);
    Ok(match &w.func {
        WindowFunc::RowNumber => (0..n).map(|i| Value::Int(i as i64 + 1)).collect(),
        WindowFunc::Rank => first.iter().map(|&f| Value::Int(f as i64 + 1)).collect(),
        WindowFunc::DenseRank => {
            let mut out = Vec::with_capacity(n);
            let mut rank = 0;
            for i in 0..n {
                if first[i] == i {
                    rank += 1;
                }
                out.push(Value::Int(rank));
            }
            out
        }
        WindowFunc::PercentRank => first.iter().map(|&f| Value::Float(if n == 1 { 0.0 } else { f as f64 / (n - 1) as f64 })).collect(),
        WindowFunc::CumeDist => last.iter().map(|&l| Value::Float(l as f64 / n as f64)).collect(),
        WindowFunc::Ntile(0) => return Err(err("NTILE(n) takes 1 or more buckets")),
        WindowFunc::NthValue { n: 0, .. } => return Err(err("NTH_VALUE(x, n) counts rows from 1")),
        WindowFunc::Ntile(buckets) => (0..n).map(|i| Value::Int(ntile(i as u64, n as u64, *buckets) as i64)).collect(),
        WindowFunc::Lag { arg: a, offset, default } | WindowFunc::Lead { arg: a, offset, default } => {
            let lag = matches!(w.func, WindowFunc::Lag { .. });
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                tick()?;
                let at = match lag {
                    true => i.checked_sub(*offset as usize),
                    false => i.checked_add(*offset as usize).filter(|&j| j < n),
                };
                out.push(match (at, default) {
                    (Some(j), _) => arg(a, j)?,
                    (None, Some(d)) => arg(d, i)?,
                    (None, None) => Value::Null,
                });
            }
            out
        }
        f => {
            let frame = w.frame();
            let bounds = (0..n).map(|i| frame_of(&frame, i, n, keys, &w.order, first, last)).collect::<Result<Vec<_>>>()?;
            match f {
                WindowFunc::FirstValue(a) => bounds.iter().map(|&(lo, hi)| if lo < hi { arg(a, lo) } else { Ok(Value::Null) }).collect::<Result<_>>()?,
                WindowFunc::LastValue(a) => bounds.iter().map(|&(lo, hi)| if lo < hi { arg(a, hi - 1) } else { Ok(Value::Null) }).collect::<Result<_>>()?,
                WindowFunc::NthValue { arg: a, n: k } => {
                    bounds.iter().map(|&(lo, hi)| match lo.checked_add(*k as usize - 1).filter(|&j| j < hi) { Some(j) => arg(a, j), None => Ok(Value::Null) }).collect::<Result<_>>()?
                }
                WindowFunc::Agg(a) => {
                    let new = || Acc::new(&Projection::Agg { func: a.func, of: crate::query::logical::Col::Field(0) });
                    let mut add = |acc: &mut Acc, at: usize| -> Result<()> {
                        tick()?;
                        match &a.arg {
                            None => acc.add_row(),
                            Some(x) => match arg(x, at)? {
                                Value::List(xs) => xs.iter().for_each(|x| {
                                    acc.add(x);
                                }),
                                v => {
                                    acc.add(&v);
                                }
                            },
                        }
                        Ok(())
                    };
                    let mut out = vec![Value::Null; n];
                    if bounds.iter().all(|&(lo, _)| lo == 0) {
                        // Frames that grow from the first row.
                        let (mut acc, mut to) = (new(), 0);
                        for (i, &(_, hi)) in bounds.iter().enumerate() {
                            while to < hi {
                                add(&mut acc, to)?;
                                to += 1;
                            }
                            out[i] = acc.value();
                        }
                    } else if bounds.iter().all(|&(_, hi)| hi == n) {
                        // Frames that end at the last row: from the last.
                        let (mut acc, mut from) = (new(), n);
                        for (i, &(lo, _)) in bounds.iter().enumerate().rev() {
                            while from > lo {
                                from -= 1;
                                add(&mut acc, from)?;
                            }
                            out[i] = acc.value();
                        }
                    } else {
                        for (i, &(lo, hi)) in bounds.iter().enumerate() {
                            let mut acc = new();
                            for j in lo..hi {
                                add(&mut acc, j)?;
                            }
                            out[i] = acc.value();
                        }
                    }
                    out
                }
                _ => unreachable!("a framed function"),
            }
        }
    })
}

/// The bucket (from 1) of row `i` of `n` in `buckets` buckets as even as
/// can be, the first ones a row larger.
pub(crate) fn ntile(i: u64, n: u64, buckets: u64) -> u64 {
    let (q, r) = (n / buckets, n % buckets);
    match i < r * (q + 1) {
        true => i / (q + 1) + 1,
        false => (i - r * (q + 1)) / q + r + 1,
    }
}

/// The rows `[lo, hi)` of row `i`'s frame, in a partition of `n` rows.
fn frame_of(frame: &Frame, i: usize, n: usize, keys: &[&[Value]], order: &[SortKey], first: &[usize], last: &[usize]) -> Result<(usize, usize)> {
    let rows = |e: &Expr| -> Result<usize> {
        match e {
            Expr::Lit(Value::Int(k)) if *k >= 0 => Ok(*k as usize),
            e => Err(err(format!("a ROWS frame's offset is an INT of 0 or more, not {e:?}"))),
        }
    };
    let (lo, hi) = match frame.units {
        FrameUnits::Rows => {
            let lo = match &frame.start {
                Bound::UnboundedPreceding => 0,
                Bound::Preceding(k) => i.saturating_sub(rows(k)?),
                Bound::CurrentRow => i,
                Bound::Following(k) => i.saturating_add(rows(k)?).min(n),
                Bound::UnboundedFollowing => n,
            };
            let hi = match &frame.end {
                Bound::UnboundedPreceding => 0,
                Bound::Preceding(k) => (i + 1).saturating_sub(rows(k)?),
                Bound::CurrentRow => i + 1,
                Bound::Following(k) => i.saturating_add(rows(k)?).saturating_add(1).min(n),
                Bound::UnboundedFollowing => n,
            };
            (lo, hi)
        }
        FrameUnits::Range => {
            let lo = match &frame.start {
                Bound::UnboundedPreceding => 0,
                Bound::CurrentRow => first[i],
                Bound::UnboundedFollowing => n,
                b => range_edge(b, true, i, keys, order, first, last)?,
            };
            let hi = match &frame.end {
                Bound::UnboundedPreceding => 0,
                Bound::CurrentRow => last[i],
                Bound::UnboundedFollowing => n,
                b => range_edge(b, false, i, keys, order, first, last)?,
            };
            (lo, hi)
        }
    };
    Ok((lo, hi.max(lo)))
}

/// Where a `RANGE` frame with an offset starts (`start`) or ends (+1) for
/// row `i`: past the rows whose key lies outside the offset from the row's.
/// A row without a value has its peers.
fn range_edge(b: &Bound, start: bool, i: usize, keys: &[&[Value]], order: &[SortKey], first: &[usize], last: &[usize]) -> Result<usize> {
    let [k] = order else { return Err(err("a RANGE frame with an offset orders by one key")) };
    let (off, preceding) = match b {
        Bound::Preceding(e) => (e, true),
        Bound::Following(e) => (e, false),
        _ => unreachable!("an offset"),
    };
    let off = match off {
        Expr::Lit(v @ (Value::Int(_) | Value::Float(_))) if v.as_f64().is_some_and(|x| x >= 0.0) => v,
        e => return Err(err(format!("a RANGE frame's offset is a number of 0 or more, not {e:?}"))),
    };
    let v = &keys[i][0];
    if matches!(v, Value::Null) {
        return Ok(if start { first[i] } else { last[i] });
    }
    // The bound's value: before the row's in the order for PRECEDING.
    let down = preceding != k.desc;
    let bound = shifted(v, off, down)?;
    // The rows with a value, in order: those within the bound.
    let n = keys.len();
    let (mut a, mut z) = (0, n);
    while a < n && matches!(keys[a][0], Value::Null) {
        a += 1;
    }
    while z > a && matches!(keys[z - 1][0], Value::Null) {
        z -= 1;
    }
    let before = |x: &Value| {
        // Whether x comes before the bound in the order.
        let c = agg::cmp(x, &bound);
        if k.desc { c.is_gt() } else { c.is_lt() }
    };
    let after = |x: &Value| {
        let c = agg::cmp(x, &bound);
        if k.desc { c.is_lt() } else { c.is_gt() }
    };
    Ok(match start {
        true => a + keys[a..z].partition_point(|r| before(&r[0])),
        false => a + keys[a..z].partition_point(|r| !after(&r[0])),
    })
}

/// `v` moved by `off`, down or up: exactly for integers.
fn shifted(v: &Value, off: &Value, down: bool) -> Result<Value> {
    Ok(match (v, off) {
        (Value::Int(x), Value::Int(o)) => {
            let y = if down { *x as i128 - *o as i128 } else { *x as i128 + *o as i128 };
            match i64::try_from(y) {
                Ok(y) => Value::Int(y),
                Err(_) => Value::Float(y as f64),
            }
        }
        (v, off) => match (v.as_f64(), off.as_f64()) {
            (Some(x), Some(o)) => Value::Float(if down { x - o } else { x + o }),
            _ => return Err(err(format!("a RANGE frame with an offset orders by a number, not {v:?}"))),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_as_even_as_can_be_the_first_ones_larger() {
        let sizes = |n: u64, b: u64| {
            let mut s = vec![0; b.min(n) as usize];
            (0..n).for_each(|i| s[ntile(i, n, b) as usize - 1] += 1);
            s
        };
        assert_eq!(sizes(10, 3), [4, 3, 3]);
        assert_eq!(sizes(7, 5), [2, 2, 1, 1, 1]);
        assert_eq!(sizes(3, 5), [1, 1, 1]);
        assert_eq!(sizes(6, 6), [1; 6]);
        assert_eq!(sizes(1, 1), [1]);
    }
}
