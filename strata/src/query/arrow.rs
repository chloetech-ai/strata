//! Strata's values as Arrow arrays, for DataFusion ([`crate::query::df`]):
//! columns built a value at a time, either from rows or straight from the
//! executor as it reads each row's fields ([`Columns`]), so that a search
//! leaf's rows reach DataFusion without being held as rows first.
//!
//! A column takes the Arrow type of its first value that is not NULL;
//! values of another type later make it [`Mixed`], which DataFusion does
//! not run (the operators do).

use std::sync::Arc;

use datafusion::arrow::array::{
    Array, ArrayRef, BooleanArray, BooleanBuilder, Float32Builder, Float64Array, Float64Builder, Int64Array, Int64Builder, ListBuilder, NullArray, StringBuilder, TimestampMicrosecondArray,
    TimestampMicrosecondBuilder,
};

use crate::query::exec::ExecStats;
use crate::value::Value;

/// A search leaf's rows as Arrow arrays, as the executor gives them
/// ([`crate::query::exec::Executor::execute_arrow`]): what a
/// [`crate::query::exec::ResultSet`] holds, a column at a time.
pub struct Columns {
    /// The columns' names, as the leaf's projection gives them.
    pub names: Vec<String>,
    /// One array a column, each of `rows` values.
    pub arrays: Vec<ArrayRef>,
    /// Each row's primary key (NULL in a table without one).
    pub keys: ArrayRef,
    /// The score each row was ordered by.
    pub scores: Vec<f64>,
    pub rows: usize,
    pub stats: ExecStats,
}

impl std::fmt::Debug for Columns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Columns").field("names", &self.names).field("rows", &self.rows).finish_non_exhaustive()
    }
}

impl Columns {
    /// About the bytes the arrays hold.
    pub fn bytes(&self) -> u64 {
        let keys = match self.arrays.iter().any(|a| Arc::ptr_eq(a, &self.keys)) {
            true => 0,
            false => self.keys.get_array_memory_size(),
        };
        (self.arrays.iter().map(|a| a.get_array_memory_size()).sum::<usize>() + keys + 8 * self.scores.len()) as u64
    }
}

/// A column holding values of more than one type (or a list of both
/// numbers and text): why.
#[derive(Debug)]
pub struct Mixed(pub String);

fn mixed<T>(why: impl Into<String>) -> Result<T, Mixed> {
    Err(Mixed(why.into()))
}

/// What Arrow type holds a column's values.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Null,
    Int,
    Float,
    Str,
    Bool,
    Time,
    Vector,
    IntList,
    StrList,
    /// A list without values: of either.
    List,
}

fn kind_of(v: &Value) -> Result<Kind, Mixed> {
    Ok(match v {
        Value::Null => Kind::Null,
        Value::Int(_) => Kind::Int,
        Value::Float(_) => Kind::Float,
        Value::Str(_) => Kind::Str,
        Value::Bool(_) => Kind::Bool,
        Value::Timestamp(_) => Kind::Time,
        Value::Vector(_) => Kind::Vector,
        Value::List(xs) => {
            let mut k = Kind::List;
            for x in xs {
                let kx = match x {
                    Value::Int(_) => Kind::IntList,
                    Value::Str(_) => Kind::StrList,
                    v => return mixed(format!("a list holding {v:?}")),
                };
                if k != Kind::List && k != kx {
                    return mixed("a list of INT and text values");
                }
                k = kx;
            }
            k
        }
    })
}

/// One column being built a value at a time: typed by its first value
/// that is not NULL. Lists and vectors are kept as values and made an
/// array at the end ([`array_of`]).
pub enum Builder {
    Nulls(usize),
    Int(Int64Builder),
    Float(Float64Builder),
    Str(StringBuilder),
    Bool(BooleanBuilder),
    Time(TimestampMicrosecondBuilder),
    Other(Vec<Value>),
}

impl Builder {
    /// An empty column.
    pub fn new() -> Builder {
        Builder::Nulls(0)
    }

    /// The array of the `n` values pushed.
    pub fn finish(self, n: usize) -> Result<ArrayRef, Mixed> {
        Ok(match self {
            Builder::Nulls(_) => Arc::new(NullArray::new(n)),
            Builder::Int(mut b) => Arc::new(b.finish()),
            Builder::Float(mut b) => Arc::new(b.finish()),
            Builder::Str(mut b) => Arc::new(b.finish()),
            Builder::Bool(mut b) => Arc::new(b.finish()),
            Builder::Time(mut b) => Arc::new(b.finish()),
            Builder::Other(values) => array_of(values.iter())?,
        })
    }

    /// The column's type, from its first value that is not NULL, for `n`
    /// values in all.
    fn start(&mut self, v: &Value, n: usize) {
        let Builder::Nulls(k) = *self else { return };
        *self = match v {
            Value::Int(_) => Builder::Int(Int64Builder::with_capacity(n)),
            Value::Float(_) => Builder::Float(Float64Builder::with_capacity(n)),
            Value::Str(_) => Builder::Str(StringBuilder::with_capacity(n, n * 8)),
            Value::Bool(_) => Builder::Bool(BooleanBuilder::with_capacity(n)),
            Value::Timestamp(_) => Builder::Time(TimestampMicrosecondBuilder::with_capacity(n)),
            _ => Builder::Other(Vec::with_capacity(n)),
        };
        for _ in 0..k {
            self.null();
        }
    }

    /// A NULL.
    pub fn null(&mut self) {
        match self {
            Builder::Nulls(k) => *k += 1,
            Builder::Int(b) => b.append_null(),
            Builder::Float(b) => b.append_null(),
            Builder::Str(b) => b.append_null(),
            Builder::Bool(b) => b.append_null(),
            Builder::Time(b) => b.append_null(),
            Builder::Other(xs) => xs.push(Value::Null),
        }
    }

    /// The next value, of a column of `n`.
    pub fn push(&mut self, v: Value, n: usize) -> Result<(), Mixed> {
        if matches!(v, Value::Null) {
            self.null();
            return Ok(());
        }
        self.start(&v, n);
        match (&mut *self, v) {
            (Builder::Other(xs), v) => xs.push(v),
            (Builder::Int(b), Value::Int(x)) => b.append_value(x),
            (Builder::Float(b), Value::Float(x)) => b.append_value(x),
            (Builder::Str(b), Value::Str(x)) => b.append_value(x),
            (Builder::Bool(b), Value::Bool(x)) => b.append_value(x),
            (Builder::Time(b), Value::Timestamp(x)) => b.append_value(x),
            (_, v) => return mixed(format!("a column of values of several types, {v:?} among them")),
        }
        Ok(())
    }

    /// [`Builder::push`] of a value held elsewhere: copied only into the
    /// array (or, a list's or vector's, kept whole).
    pub fn push_ref(&mut self, v: &Value, n: usize) -> Result<(), Mixed> {
        match v {
            Value::Str(x) => self.push_str(x, n),
            v => self.push(v.clone(), n),
        }
    }

    /// The next value, text, of a column of `n`: as [`Builder::push`] of
    /// it, without making it a [`Value`] first.
    pub fn push_str(&mut self, x: &str, n: usize) -> Result<(), Mixed> {
        if let Builder::Nulls(_) = self {
            self.start(&Value::Str(String::new()), n);
        }
        match self {
            Builder::Str(b) => b.append_value(x),
            Builder::Other(xs) => xs.push(Value::Str(x.to_string())),
            _ => return mixed(format!("a column of values of several types, {x:?} among them")),
        }
        Ok(())
    }
}

impl Default for Builder {
    fn default() -> Self {
        Builder::new()
    }
}

/// Columns `0..width` of `rows` as Arrow arrays, in one pass over the
/// rows: a column of one type each, NULLs aside.
pub fn columns_of(rows: Vec<Vec<Value>>, width: usize) -> Result<Vec<ArrayRef>, Mixed> {
    let n = rows.len();
    let mut cols: Vec<Builder> = (0..width).map(|_| Builder::new()).collect();
    for r in rows {
        let k = r.len();
        for (c, v) in cols.iter_mut().zip(r) {
            c.push(v, n)?;
        }
        cols.iter_mut().skip(k).for_each(Builder::null);
    }
    cols.into_iter().map(|c| c.finish(n)).collect()
}

/// `values` as one Arrow array ([`columns_of`] of one column).
pub fn column_of(values: Vec<Value>) -> Result<ArrayRef, Mixed> {
    let n = values.len();
    let mut c = Builder::new();
    for v in values {
        c.push(v, n)?;
    }
    c.finish(n)
}

/// The values of one column as an Arrow array of their one type.
fn array_of<'v>(values: impl Iterator<Item = &'v Value> + Clone) -> Result<ArrayRef, Mixed> {
    let mut kind = Kind::Null;
    for v in values.clone() {
        let k = kind_of(v)?;
        kind = match (kind, k) {
            (a, Kind::Null) => a,
            (Kind::Null, b) => b,
            (Kind::List, b @ (Kind::IntList | Kind::StrList | Kind::List)) | (b @ (Kind::IntList | Kind::StrList), Kind::List) => b,
            (a, b) if a == b => a,
            (a, b) => return mixed(format!("a column of {a:?} and {b:?} values")),
        };
    }
    let n = values.clone().count();
    Ok(match kind {
        Kind::Null => Arc::new(NullArray::new(n)),
        Kind::Int => Arc::new(values.map(|v| v.as_i64()).collect::<Int64Array>()),
        Kind::Float => Arc::new(values.map(|v| if let Value::Float(x) = v { Some(*x) } else { None }).collect::<Float64Array>()),
        Kind::Str => {
            let mut b = StringBuilder::new();
            values.for_each(|v| b.append_option(v.as_str()));
            Arc::new(b.finish())
        }
        Kind::Bool => Arc::new(values.map(|v| if let Value::Bool(x) = v { Some(*x) } else { None }).collect::<BooleanArray>()),
        Kind::Time => Arc::new(values.map(|v| if let Value::Timestamp(x) = v { Some(*x) } else { None }).collect::<TimestampMicrosecondArray>()),
        Kind::Vector => {
            let mut b = ListBuilder::new(Float32Builder::new());
            for v in values {
                match v {
                    Value::Vector(x) => {
                        b.values().append_slice(x);
                        b.append(true);
                    }
                    _ => b.append(false),
                }
            }
            Arc::new(b.finish())
        }
        Kind::IntList => {
            let mut b = ListBuilder::new(Int64Builder::new());
            for v in values {
                match v {
                    Value::List(xs) => {
                        xs.iter().for_each(|x| b.values().append_option(x.as_i64()));
                        b.append(true);
                    }
                    _ => b.append(false),
                }
            }
            Arc::new(b.finish())
        }
        Kind::StrList | Kind::List => {
            let mut b = ListBuilder::new(StringBuilder::new());
            for v in values {
                match v {
                    Value::List(xs) => {
                        xs.iter().for_each(|x| b.values().append_option(x.as_str()));
                        b.append(true);
                    }
                    _ => b.append(false),
                }
            }
            Arc::new(b.finish())
        }
    })
}
