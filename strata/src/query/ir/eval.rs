//! The one evaluator of plan expressions over a row, in SQL's three-valued
//! logic: a comparison with NULL is UNKNOWN (`Value::Null`), `NOT` of
//! UNKNOWN is UNKNOWN, `AND` and `OR` follow Kleene's tables, and a
//! condition keeps a row only when it is TRUE ([`holds`]).
//!
//! It agrees with the indexes, which answer a Search's filter as sets
//! ([`crate::query::logical::Pred::three_valued`]): a comparison on a
//! multi-valued field holds when one of its values matches and is FALSE
//! otherwise, never UNKNOWN, and `IS NULL` of one holds when it has no
//! value. Full-text, graph and score expressions other than `decay` are
//! answered by indexes, not row by row.
//!
//! The SELECTs of joins, `WITH` results and subqueries evaluate their
//! conditions and values over rows in memory here too
//! ([`crate::query::compound`]), and so does `HAVING`
//! ([`crate::query::agg::finish`]).

use std::cmp::Ordering;

use super::{ArithOp, CmpOp, Expr, Func};
use crate::error::{Error, Result};
use crate::query::agg::cmp_int_float;
use crate::value::Value;

/// What an expression reads of its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ref {
    /// A field of the row ([`Expr::Field`]), as queries return it: BOOL
    /// and TIMESTAMP fields as booleans and times, a multi-valued field as
    /// a list.
    Field(u32),
    /// A column of the Aggregate below ([`Expr::Output`]).
    Output(u32),
    /// A field of the row an insert proposed ([`Expr::Excluded`]).
    Excluded(u32),
    Score,
    Hops,
    SeqNo,
}

/// A row's values, as an expression reads them.
pub type Row<'r> = dyn Fn(Ref) -> Result<Value> + 'r;

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// Whether condition `e` is TRUE over `row`: FALSE and UNKNOWN leave the
/// row out.
pub fn holds(e: &Expr, row: &Row) -> Result<bool> {
    Ok(truth(&eval(e, row)?)? == Some(true))
}

/// The value of `e` over `row`; a condition's is a `Bool`, or `Null` for
/// UNKNOWN.
pub fn eval(e: &Expr, row: &Row) -> Result<Value> {
    let cond = |e: &Expr| truth(&eval(e, row)?);
    Ok(match e {
        Expr::Field(f) => row(Ref::Field(*f))?,
        Expr::Output(i) => row(Ref::Output(*i))?,
        Expr::Excluded(f) => row(Ref::Excluded(*f))?,
        Expr::Score => row(Ref::Score)?,
        Expr::Hops => row(Ref::Hops)?,
        Expr::SeqNo => row(Ref::SeqNo)?,
        Expr::Lit(v) => v.clone(),
        Expr::Not(x) => known(cond(x)?.map(|b| !b)),
        Expr::And(xs) => {
            let mut unknown = false;
            for x in xs {
                match cond(x)? {
                    Some(false) => return Ok(Value::Bool(false)),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            known((!unknown).then_some(true))
        }
        Expr::Or(xs) => {
            let mut unknown = false;
            for x in xs {
                match cond(x)? {
                    Some(true) => return Ok(Value::Bool(true)),
                    None => unknown = true,
                    Some(false) => {}
                }
            }
            known((!unknown).then_some(false))
        }
        // TRUE once `need` are, FALSE once too few are left to get there,
        // and UNKNOWN when the UNKNOWN ones could go either way.
        Expr::AtLeast { need, conds } => {
            let (need, mut yes, mut maybe) = (*need as usize, 0, 0);
            for x in conds {
                match cond(x)? {
                    Some(true) => yes += 1,
                    None => maybe += 1,
                    Some(false) => {}
                }
            }
            known(if yes >= need {
                Some(true)
            } else if yes + maybe < need {
                Some(false)
            } else {
                None
            })
        }
        Expr::IsNull(x) => Value::Bool(match eval(x, row)? {
            Value::Null => true,
            Value::List(vs) => vs.is_empty(),
            _ => false,
        }),
        // `x != y` is `NOT (x = y)`: for a multi-valued field, that none of
        // its values is y.
        Expr::Cmp { op: CmpOp::Ne, left, right } => {
            let eq = Expr::Cmp { op: CmpOp::Eq, left: left.clone(), right: right.clone() };
            known(truth(&eval(&eq, row)?)?.map(|b| !b))
        }
        Expr::Cmp { op, left, right } => {
            let (a, b) = (eval(left, row)?, eval(right, row)?);
            any(&a, |x| any_of(&b, |y| Ok(compare(x, y)?.map(|o| test(*op, o)))))?
        }
        Expr::Between { expr, lo, hi } => {
            let bound = |b: &Option<Box<Expr>>| b.as_ref().map(|b| eval(b, row)).transpose();
            let (lo, hi) = (bound(lo)?, bound(hi)?);
            any(&eval(expr, row)?, |x| {
                let side = |b: &Option<Value>, outside: Ordering| -> Result<Option<bool>> {
                    match b {
                        None => Ok(Some(true)),
                        Some(b) => Ok(compare(x, b)?.map(|o| o != outside)),
                    }
                };
                Ok(and(side(&lo, Ordering::Less)?, side(&hi, Ordering::Greater)?))
            })?
        }
        Expr::In { expr, list } => {
            let mut items = Vec::with_capacity(list.len());
            for x in list {
                match eval(x, row)? {
                    Value::List(vs) => items.extend(vs),
                    v => items.push(v),
                }
            }
            any(&eval(expr, row)?, |x| {
                let mut unknown = false;
                for y in &items {
                    match compare(x, y)? {
                        Some(Ordering::Equal) => return Ok(Some(true)),
                        None => unknown = true,
                        Some(_) => {}
                    }
                }
                Ok((!unknown).then_some(false))
            })?
        }
        Expr::Arith { op, left, right } => arith(*op, &eval(left, row)?, &eval(right, row)?)?,
        Expr::Neg(x) => match eval(x, row)? {
            Value::Null => Value::Null,
            Value::Int(x) => x.checked_neg().map_or(Value::Float(-(x as f64)), Value::Int),
            Value::Float(x) => Value::Float(-x),
            v => return Err(err(format!("cannot negate {v:?}"))),
        },
        Expr::Concat(xs) => {
            let mut out = String::new();
            for x in xs {
                match eval(x, row)? {
                    Value::Null => {}
                    Value::Str(s) => out.push_str(&s),
                    Value::Int(x) => out.push_str(&x.to_string()),
                    Value::Float(x) => out.push_str(&x.to_string()),
                    Value::Bool(b) => out.push_str(&b.to_string()),
                    Value::Timestamp(t) => out.push_str(&crate::timestamp::format(t)),
                    v => out.push_str(&format!("{v:?}")),
                }
            }
            Value::Str(out)
        }
        Expr::Like { expr, pattern, case_insensitive } => match (eval(expr, row)?, eval(pattern, row)?) {
            (Value::Str(s), Value::Str(p)) => {
                let (s, p) = if *case_insensitive { (s.to_lowercase(), p.to_lowercase()) } else { (s, p) };
                Value::Bool(like(&s.chars().collect::<Vec<_>>(), &p.chars().collect::<Vec<_>>()))
            }
            _ => Value::Null,
        },
        Expr::Prefix { expr, prefix, edits } => match (eval(expr, row)?, eval(prefix, row)?) {
            (Value::Str(s), Value::Str(p)) if *edits == 0 => Value::Bool(s.starts_with(&p)),
            (Value::Str(s), Value::Str(p)) => Value::Bool(crate::text::fuzzy::prefix_within(&p.chars().collect::<Vec<_>>(), &s, *edits)),
            _ => Value::Null,
        },
        Expr::Case { operand, whens, otherwise } => {
            let base = operand.as_ref().map(|o| eval(o, row)).transpose()?;
            for w in whens {
                let hit = match &base {
                    Some(b) => compare(b, &eval(&w.when, row)?)? == Some(Ordering::Equal),
                    None => cond(&w.when)? == Some(true),
                };
                if hit {
                    return eval(&w.then, row);
                }
            }
            otherwise.as_ref().map(|x| eval(x, row)).transpose()?.unwrap_or(Value::Null)
        }
        Expr::Call { func, args } => call(*func, &args.iter().map(|a| eval(a, row)).collect::<Result<Vec<_>>>()?)?,
        // A row without a value scores 0, as the executor scores it.
        Expr::Decay { field, decay, .. } => match row(Ref::Field(*field))? {
            Value::Null => Value::Float(0.0),
            Value::Int(x) | Value::Timestamp(x) => Value::Float(decay.eval(x as f64)),
            Value::Float(x) => Value::Float(decay.eval(x)),
            v => return Err(err(format!("decay of {v:?}, which is not a number"))),
        },
        Expr::Match { .. }
        | Expr::Graph(_)
        | Expr::Bm25 { .. }
        | Expr::Similarity { .. }
        | Expr::MaxSim { .. }
        | Expr::Sparse { .. }
        | Expr::Rrf { .. }
        | Expr::Weighted(_)
        | Expr::Product(_)
        | Expr::Norm { .. }
        | Expr::Gbdt { .. }
        | Expr::Ppr(_)
        | Expr::Ranked { .. }
        | Expr::Subquery(_) => return Err(err("full-text, graph and score expressions are answered by indexes in a Search, not row by row")),
        Expr::Values(_) | Expr::Keys(_) | Expr::Exists(_) | Expr::Scalar(_) => return Err(err("a subquery is run before the rows it is evaluated over")),
        Expr::Outer(_) => return Err(err("a correlated subquery runs with the values of the row around it in place")),
        Expr::ByKey(_) => return Err(err("values by key rank a Search's rows, not rows in memory")),
    })
}

/// A condition's truth: `None` for UNKNOWN.
fn truth(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        v => Err(err(format!("{v:?} is not a condition: a condition is TRUE, FALSE or NULL"))),
    }
}

fn known(t: Option<bool>) -> Value {
    t.map_or(Value::Null, Value::Bool)
}

fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

/// `test` of a value, or of each value of a list: a list holds when one
/// of its values does and is FALSE otherwise (empty included), never
/// UNKNOWN.
fn any(v: &Value, test: impl Fn(&Value) -> Result<Option<bool>>) -> Result<Value> {
    match v {
        Value::List(vs) => {
            for x in vs {
                if test(x)? == Some(true) {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        v => Ok(known(test(v)?)),
    }
}

/// [`any`] on the right side of a comparison, as a truth value.
fn any_of(v: &Value, test: impl Fn(&Value) -> Result<Option<bool>>) -> Result<Option<bool>> {
    truth(&any(v, test)?)
}

fn test(op: CmpOp, o: Ordering) -> bool {
    match op {
        CmpOp::Eq => o == Ordering::Equal,
        CmpOp::Ne => o != Ordering::Equal,
        CmpOp::Lt => o == Ordering::Less,
        CmpOp::Le => o != Ordering::Greater,
        CmpOp::Gt => o == Ordering::Greater,
        CmpOp::Ge => o != Ordering::Less,
    }
}

/// An integer view of a value: an INT, a time in microseconds, a BOOL as 1
/// or 0 (as their columns store them).
fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(x) | Value::Timestamp(x) => Some(*x),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

/// The order of two values, `None` when either is NULL. Numbers compare by
/// value (integers exactly, also against doubles), text by bytes, a time
/// also against text naming one; anything else is an error.
pub fn compare(a: &Value, b: &Value) -> Result<Option<Ordering>> {
    let time = |s: &str| crate::timestamp::parse(s);
    Ok(Some(match (a, b) {
        (Value::Null, _) | (_, Value::Null) => return Ok(None),
        (Value::Str(x), Value::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Timestamp(x), Value::Str(s)) if time(s).is_some() => x.cmp(&time(s).unwrap()),
        (Value::Str(s), Value::Timestamp(y)) if time(s).is_some() => time(s).unwrap().cmp(y),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        _ => match (int(a), int(b), a, b) {
            (Some(x), Some(y), ..) => x.cmp(&y),
            (Some(x), None, _, Value::Float(y)) => cmp_int_float(x, *y),
            (None, Some(y), Value::Float(x), _) => cmp_int_float(y, *x).reverse(),
            _ => return Err(err(format!("cannot compare {a:?} with {b:?}"))),
        },
    }))
}

/// `a op b`: integers exactly, and a FLOAT where an integer result would
/// overflow (as SQLite), never a wrapped value; a double if either is one;
/// a time plus or minus microseconds. Division gives a FLOAT, and a zero
/// divisor NULL.
fn arith(op: ArithOp, a: &Value, b: &Value) -> Result<Value> {
    let name = match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::Mod => "%",
    };
    let float = |x: f64, y: f64| match op {
        ArithOp::Add => Value::Float(x + y),
        ArithOp::Sub => Value::Float(x - y),
        ArithOp::Mul => Value::Float(x * y),
        _ if y == 0.0 => Value::Null,
        ArithOp::Div => Value::Float(x / y),
        ArithOp::Mod => Value::Float(x % y),
    };
    let time = |x: i64, y: i64| {
        match op {
            ArithOp::Add => x.checked_add(y),
            _ => x.checked_sub(y),
        }
        .map(Value::Timestamp)
        .ok_or_else(|| err(format!("{a:?} {name} {b:?} is outside the times a TIMESTAMP holds")))
    };
    Ok(match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        (Value::Int(x), Value::Int(y)) => match op {
            ArithOp::Add => x.checked_add(*y).map(Value::Int),
            ArithOp::Sub => x.checked_sub(*y).map(Value::Int),
            ArithOp::Mul => x.checked_mul(*y).map(Value::Int),
            ArithOp::Div => Some(float(*x as f64, *y as f64)),
            ArithOp::Mod if *y == 0 => Some(Value::Null),
            // i64::MIN % -1 overflows; the remainder is 0.
            ArithOp::Mod => Some(Value::Int(x.checked_rem(*y).unwrap_or(0))),
        }
        .unwrap_or_else(|| float(*x as f64, *y as f64)),
        (Value::Timestamp(x), Value::Int(y)) if matches!(op, ArithOp::Add | ArithOp::Sub) => time(*x, *y)?,
        (Value::Int(x), Value::Timestamp(y)) if op == ArithOp::Add => time(*y, *x)?,
        (Value::Int(x), Value::Float(y)) => float(*x as f64, *y),
        (Value::Float(x), Value::Int(y)) => float(*x, *y as f64),
        (Value::Float(x), Value::Float(y)) => float(*x, *y),
        _ => return Err(err(format!("cannot compute {a:?} {name} {b:?}"))),
    })
}

/// A scalar function of evaluated arguments; NULL in, NULL out (but for
/// `coalesce`).
fn call(func: Func, args: &[Value]) -> Result<Value> {
    let one = args.first().cloned().unwrap_or(Value::Null);
    let text = |f: fn(&str) -> Value| match &one {
        Value::Str(s) => Ok(f(s)),
        Value::Null => Ok(Value::Null),
        v => Err(err(format!("{func:?} takes text, not {v:?}"))),
    };
    match func {
        Func::Lower => text(|s| Value::Str(s.to_lowercase())),
        Func::Upper => text(|s| Value::Str(s.to_uppercase())),
        Func::Length => text(|s| Value::Int(s.chars().count() as i64)),
        Func::SidAt => crate::sid::sid_at(&one, args.get(1).unwrap_or(&Value::Null)),
        // UNKNOWN is not TRUE.
        Func::CountTrue => {
            let mut n = 0;
            for v in args {
                n += (truth(v).map_err(|_| err(format!("count_true counts conditions, not {v:?}")))? == Some(true)) as i64;
            }
            Ok(Value::Int(n))
        }
        Func::Abs => Ok(match one {
            Value::Int(x) => x.checked_abs().map_or(Value::Float((x as f64).abs()), Value::Int),
            Value::Float(x) => Value::Float(x.abs()),
            Value::Null => Value::Null,
            v => return Err(err(format!("abs takes a number, not {v:?}"))),
        }),
        Func::Round => {
            let d = match args.get(1) {
                None => 0,
                Some(Value::Int(d)) => (*d).clamp(-30, 30) as i32,
                Some(v) => return Err(err(format!("round takes a whole number of decimals, not {v:?}"))),
            };
            let x = match one {
                Value::Null => return Ok(Value::Null),
                Value::Int(x) if d == 0 => return Ok(Value::Int(x)),
                v => v.as_f64().ok_or_else(|| err(format!("round takes a number, not {v:?}")))?,
            };
            let m = 10f64.powi(d);
            let r = (x * m).round() / m;
            Ok(if d <= 0 { Value::Int(r as i64) } else { Value::Float(r) })
        }
        // A list without values is NULL here, as to IS NULL.
        Func::Coalesce => Ok(args.iter().find(|v| !matches!(v, Value::Null) && !matches!(v, Value::List(xs) if xs.is_empty())).cloned().unwrap_or(Value::Null)),
        Func::Ln | Func::Log | Func::Exp | Func::Sqrt | Func::Pow | Func::Least | Func::Greatest | Func::Sigmoid | Func::Clamp => {
            let xs = args
                .iter()
                .map(|v| match v {
                    Value::Null => Ok(f64::NAN),
                    v => v.as_f64().ok_or_else(|| err(format!("{} takes numbers, not {v:?}", func.name()))),
                })
                .collect::<Result<Vec<f64>>>()?;
            let r = func.eval_f64(&xs);
            Ok(if r.is_nan() { Value::Null } else { Value::Float(r) })
        }
        // With the kernel similarity() uses, so the two agree to the bit.
        Func::Dot => match (vector(&one)?, vector(args.get(1).unwrap_or(&Value::Null))?) {
            (Some(a), Some(b)) if a.len() == b.len() => Ok(Some(crate::vector::kernels::dot_f32(&a, &b) as f64).filter(|d| d.is_finite()).map_or(Value::Null, Value::Float)),
            (Some(a), Some(b)) => Err(err(format!("dot of vectors of {} and {} dimensions", a.len(), b.len()))),
            _ => Ok(Value::Null),
        },
    }
}

/// A vector argument: a vector, or a list of numbers as a parameter
/// gives one; `None` for NULL.
fn vector(v: &Value) -> Result<Option<std::borrow::Cow<'_, [f32]>>> {
    Ok(match v {
        Value::Null => None,
        Value::Vector(x) => Some(std::borrow::Cow::Borrowed(x)),
        Value::List(xs) if xs.iter().all(|x| matches!(x, Value::Int(_) | Value::Float(_))) => Some(xs.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect()),
        v => return Err(err(format!("dot takes vectors, not {v:?}"))),
    })
}

/// SQL LIKE: `%` any run, `_` one character. Greedy matching that backs
/// up only to the last `%`: linear in the text for each pattern piece, and
/// without recursion, so a long pattern cannot overflow the stack.
pub(crate) fn like(s: &[char], p: &[char]) -> bool {
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while i < s.len() {
        if j < p.len() && (p[j] == '_' || (p[j] != '%' && p[j] == s[i])) {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '%' {
            star = Some((j, i));
            j += 1;
        } else if let Some((sj, si)) = star {
            j = sj + 1;
            i = si + 1;
            star = Some((sj, si + 1));
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '%' {
        j += 1;
    }
    j == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(v: Value) -> Box<Expr> {
        Box::new(Expr::Lit(v))
    }

    fn cmp(op: CmpOp, l: Value, r: Value) -> Expr {
        Expr::Cmp { op, left: lit(l), right: lit(r) }
    }

    fn val(e: &Expr) -> Value {
        eval(e, &|_| Ok(Value::Null)).unwrap()
    }

    const T: Value = Value::Bool(true);
    const F: Value = Value::Bool(false);
    const N: Value = Value::Null;

    #[test]
    fn three_valued_logic() {
        let (t, f, n) = (Expr::Lit(T), Expr::Lit(F), Expr::Lit(N));
        assert_eq!(val(&Expr::Not(Box::new(n.clone()))), N);
        for (a, b, and, or) in [(&t, &n, N, T), (&f, &n, F, N), (&n, &n, N, N), (&t, &f, F, T), (&t, &t, T, T)] {
            assert_eq!(val(&Expr::And(vec![a.clone(), b.clone()])), and, "{a:?} AND {b:?}");
            assert_eq!(val(&Expr::And(vec![b.clone(), a.clone()])), and, "{b:?} AND {a:?}");
            assert_eq!(val(&Expr::Or(vec![a.clone(), b.clone()])), or, "{a:?} OR {b:?}");
        }
        assert_eq!(val(&cmp(CmpOp::Ne, N, Value::Int(2))), N);
        assert_eq!(val(&Expr::IsNull(lit(N))), T);
        assert!(!holds(&n, &|_| Ok(N)).unwrap() && !holds(&Expr::Not(Box::new(n)), &|_| Ok(N)).unwrap());
    }

    #[test]
    fn values_compare_across_their_types() {
        let big = 9_007_199_254_740_993; // 2^53 + 1, which a double rounds down
        assert_eq!(val(&cmp(CmpOp::Gt, Value::Int(big), Value::Float(9_007_199_254_740_992.0))), T);
        assert_eq!(val(&cmp(CmpOp::Lt, Value::Float(2.5), Value::Int(3))), T);
        assert_eq!(val(&cmp(CmpOp::Eq, Value::Bool(true), Value::Int(1))), T);
        assert_eq!(val(&cmp(CmpOp::Ge, Value::Timestamp(1_790_812_800_000_000), Value::Str("2026-10-01".into()))), T);
        assert_eq!(val(&cmp(CmpOp::Lt, Value::Str("B".into()), Value::Str("a".into()))), T);
        assert!(eval(&cmp(CmpOp::Eq, Value::Str("1".into()), Value::Int(1)), &|_| Ok(N)).is_err());
    }

    #[test]
    fn lists_hold_when_one_value_does() {
        let tags = || Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]);
        let one = |v: &str| Expr::In { expr: lit(tags()), list: vec![Expr::Lit(Value::Str(v.into()))] };
        assert_eq!(val(&one("b")), T);
        assert_eq!(val(&one("c")), F);
        assert_eq!(val(&Expr::Not(Box::new(one("c")))), T);
        let empty = Expr::In { expr: lit(Value::List(vec![])), list: vec![Expr::Lit(Value::Str("a".into()))] };
        assert_eq!((val(&empty), val(&Expr::IsNull(lit(Value::List(vec![]))))), (F, T));
        assert_eq!(val(&cmp(CmpOp::Ne, tags(), Value::Str("a".into()))), F);
        assert_eq!(val(&cmp(CmpOp::Ne, tags(), Value::Str("c".into()))), T);
        // IN with a NULL among its values is UNKNOWN when nothing matches.
        let maybe = Expr::In { expr: lit(Value::Int(1)), list: vec![Expr::Lit(Value::Int(2)), Expr::Lit(N)] };
        assert_eq!(val(&maybe), N);
        let between = |x: Value| Expr::Between { expr: lit(x), lo: Some(lit(Value::Int(1))), hi: None };
        assert_eq!((val(&between(Value::Int(1))), val(&between(Value::Int(0))), val(&between(N))), (T, F, N));
    }

    #[test]
    fn arithmetic_is_exact_or_a_float() {
        let ar = |op, a, b| Expr::Arith { op, left: lit(a), right: lit(b) };
        let (min, two) = (Value::Int(i64::MIN), Value::Int(2));
        for (e, want) in [
            (ar(ArithOp::Add, two.clone(), Value::Int(3)), Value::Int(5)),
            (ar(ArithOp::Add, two.clone(), Value::Float(0.5)), Value::Float(2.5)),
            (ar(ArithOp::Add, two.clone(), N), N),
            (ar(ArithOp::Div, Value::Int(7), two.clone()), Value::Float(3.5)),
            (ar(ArithOp::Div, Value::Int(7), Value::Int(0)), N),
            (ar(ArithOp::Mod, Value::Int(7), Value::Int(0)), N),
            // An integer result that would overflow is a FLOAT, never a
            // panic or a wrapped value.
            (ar(ArithOp::Add, Value::Int(i64::MAX), Value::Int(1)), Value::Float(9_223_372_036_854_775_808.0)),
            (ar(ArithOp::Mul, min.clone(), two), Value::Float(-18_446_744_073_709_551_616.0)),
            (ar(ArithOp::Mod, min.clone(), Value::Int(-1)), Value::Int(0)),
            (Expr::Neg(lit(min.clone())), Value::Float(9_223_372_036_854_775_808.0)),
            (Expr::Call { func: Func::Abs, args: vec![Expr::Lit(min)] }, Value::Float(9_223_372_036_854_775_808.0)),
        ] {
            assert_eq!(val(&e), want, "{e:?}");
        }
        let late = ar(ArithOp::Add, Value::Timestamp(i64::MAX), Value::Int(1));
        assert!(eval(&late, &|_| Ok(N)).is_err());
    }

    #[test]
    fn text_case_and_functions() {
        let s = |x: &str| Value::Str(x.into());
        let like = |x: &str, p: &str, ci| Expr::Like { expr: lit(s(x)), pattern: lit(s(p)), case_insensitive: ci };
        assert_eq!((val(&like("Graph", "gr%", false)), val(&like("Graph", "gr%", true))), (F, T));
        assert_eq!(val(&Expr::Like { expr: lit(N), pattern: lit(s("%")), case_insensitive: false }), N);
        assert_eq!(val(&Expr::Concat(vec![Expr::Lit(s("a")), Expr::Lit(N), Expr::Lit(Value::Int(1))])), s("a1"));
        let case = |operand: Option<Value>| Expr::Case {
            operand: operand.map(lit),
            whens: vec![super::super::When { when: Expr::Lit(N), then: Expr::Lit(s("null")) }, super::super::When { when: Expr::Lit(T), then: Expr::Lit(s("true")) }],
            otherwise: Some(lit(s("else"))),
        };
        // NULL equals nothing, not even NULL, and is not TRUE.
        assert_eq!((val(&case(None)), val(&case(Some(N))), val(&case(Some(F)))), (s("true"), s("else"), s("else")));
        let f = |func, args: Vec<Value>| val(&Expr::Call { func, args: args.into_iter().map(Expr::Lit).collect() });
        assert_eq!(f(Func::Lower, vec![s("ÄB")]), s("äb"));
        assert_eq!(f(Func::Length, vec![s("ñu")]), Value::Int(2));
        assert_eq!(f(Func::Round, vec![Value::Float(2.567), Value::Int(2)]), Value::Float(2.57));
        assert_eq!(f(Func::Round, vec![Value::Float(2.5)]), Value::Int(3));
        assert_eq!(f(Func::Coalesce, vec![N, Value::Int(4), Value::Int(5)]), Value::Int(4));
        assert_eq!(f(Func::Coalesce, vec![Value::List(Vec::new()), Value::Int(4)]), Value::Int(4));
        assert_eq!(f(Func::Upper, vec![N]), N);
    }

    #[test]
    fn like_patterns() {
        let l = |s: &str, p: &str| like(&s.chars().collect::<Vec<_>>(), &p.chars().collect::<Vec<_>>());
        assert!(l("graph neural", "graph%"));
        assert!(l("graph", "gr_ph"));
        assert!(!l("graph", "gr_h"));
        assert!(l("", "%"));
        assert!(l("abcabd", "%ab_"));
        assert!(l("mississippi", "m%ss%pi"));
        assert!(!l("mississippi", "m%ss%px"));
        assert!(l("a%b", "a%%b"));
        assert!(!l("ab", "a"));
        assert!(!l("", "_"));
        // A long pattern used to recurse once per character (and `%` runs
        // to backtrack exponentially).
        let s = "a".repeat(50_000);
        assert!(!l(&s, &format!("{}b", "%a".repeat(25_000))));
        assert!(l(&s, &"_".repeat(50_000)));
    }
}
