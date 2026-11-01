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
    Array, ArrayRef, AsArray, BooleanArray, BooleanBuilder, Float32Builder, Float64Array, Float64Builder, Int64Array, Int64Builder, ListBuilder, NullArray, StringBuilder,
    TimestampMicrosecondArray, TimestampMicrosecondBuilder,
};
use datafusion::arrow::compute::{cast, concat, concat_batches};
use datafusion::arrow::datatypes::{DataType, Field, Float64Type, Schema as ArrowSchema, TimeUnit};
use datafusion::arrow::error::ArrowError;
use datafusion::arrow::ipc::reader::StreamReader;
use datafusion::arrow::ipc::writer::StreamWriter;
use datafusion::arrow::record_batch::{RecordBatch, RecordBatchOptions};

use crate::error::{Error, Result as StrataResult};
use crate::query::exec::{ExecStats, ResultSet};
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

    /// The rows as one Arrow IPC stream, as a shard sends a coordinator a
    /// leaf of its compound query: the projection's columns, the rows'
    /// keys, then their scores (FLOAT, or NULLs for none), named by their
    /// places. The columns' names, which may repeat, travel beside it.
    pub fn to_ipc(&self) -> StrataResult<Vec<u8>> {
        let scores: ArrayRef = match self.scores.len() == self.rows {
            true => Arc::new(Float64Array::from(self.scores.clone())),
            false => Arc::new(NullArray::new(self.rows)),
        };
        let arrays: Vec<ArrayRef> = self.arrays.iter().chain([&self.keys, &scores]).cloned().collect();
        let fields: Vec<Field> = arrays.iter().enumerate().map(|(i, a)| Field::new(i.to_string(), a.data_type().clone(), true)).collect();
        let schema = Arc::new(ArrowSchema::new(fields));
        let write = || -> Result<Vec<u8>, ArrowError> {
            let batch = RecordBatch::try_new_with_options(schema.clone(), arrays, &RecordBatchOptions::new().with_row_count(Some(self.rows)))?;
            let mut out = Vec::with_capacity(self.bytes() as usize + 1024);
            let mut w = StreamWriter::try_new(&mut out, &schema)?;
            w.write(&batch)?;
            w.finish()?;
            drop(w);
            Ok(out)
        };
        write().map_err(ipc_error)
    }

    /// The rows of [`Columns::to_ipc`]'s stream `bytes`, their columns
    /// named `names`, with the statistics `stats`.
    pub fn from_ipc(bytes: &[u8], names: Vec<String>, stats: ExecStats) -> StrataResult<Columns> {
        let read = || -> Result<RecordBatch, ArrowError> {
            let r = StreamReader::try_new(std::io::Cursor::new(bytes), None)?;
            let schema = r.schema();
            let batches = r.collect::<Result<Vec<_>, _>>()?;
            concat_batches(&schema, &batches)
        };
        let batch = read().map_err(ipc_error)?;
        if batch.num_columns() != names.len() + 2 {
            return Err(Error::Query(format!("an Arrow answer of {} columns for {} named", batch.num_columns(), names.len())));
        }
        let mut arrays = batch.columns().to_vec();
        let scores = arrays.pop().expect("the scores");
        let keys = arrays.pop().expect("the keys");
        let scores = match scores.data_type() {
            DataType::Float64 => scores.as_primitive::<Float64Type>().values().to_vec(),
            _ => Vec::new(),
        };
        Ok(Columns { names, arrays, keys, scores, rows: batch.num_rows(), stats })
    }

    /// The rows as a [`ResultSet`]'s, their values as Strata gives them
    /// ([`values_of`]).
    pub fn into_result(self) -> StrataResult<ResultSet> {
        let mixed = |m: Mixed| Error::Query(format!("an Arrow answer: {}", m.0));
        let cols = self.arrays.iter().map(values_of).collect::<Result<Vec<_>, _>>().map_err(mixed)?;
        let mut rows: Vec<Vec<Value>> = (0..self.rows).map(|_| Vec::with_capacity(cols.len())).collect();
        for col in cols {
            for (row, v) in rows.iter_mut().zip(col) {
                row.push(v);
            }
        }
        let keys = values_of(&self.keys).map_err(mixed)?;
        Ok(ResultSet { columns: self.names, rows, keys, scores: self.scores, parts: None, stats: self.stats })
    }
}

fn ipc_error(e: ArrowError) -> Error {
    Error::Query(format!("Arrow IPC: {e}"))
}

/// Whether a column of type `t` holds no values that type it: NULLs, or
/// lists without values, which take the type of another part's column
/// ([`concat_columns`]).
fn untyped(a: &ArrayRef) -> bool {
    match a.data_type() {
        DataType::Null => true,
        DataType::List(_) => a.as_list::<i32>().values().is_empty(),
        _ => a.null_count() == a.len(),
    }
}

/// The rows of `parts` one after another, as a coordinator puts its
/// shards' answers to a leaf together: each part's columns, of the same
/// names, of one type each but for columns that [`untyped`] says hold no
/// values that type them, which are made the others' type. `None` when the
/// types differ otherwise (the values of one column are of several types,
/// which DataFusion does not run). The parts' statistics are left to the
/// caller, who gives `stats`.
pub fn concat_columns(parts: Vec<Columns>, stats: ExecStats) -> StrataResult<Option<Columns>> {
    let Some(first) = parts.first() else { return Err(Error::Query("no answers to put together".into())) };
    let (names, width) = (first.names.clone(), first.arrays.len());
    if parts.iter().any(|p| p.arrays.len() != width) {
        return Err(Error::Query("answers of different columns to put together".into()));
    }
    // Column `width` is the keys.
    let mut columns = Vec::with_capacity(width + 1);
    for c in 0..=width {
        let arrays: Vec<&ArrayRef> = parts.iter().map(|p| if c == width { &p.keys } else { &p.arrays[c] }).collect();
        let typed = arrays.iter().find(|a| !untyped(a)).unwrap_or(&arrays[0]).data_type().clone();
        let mut same = Vec::with_capacity(arrays.len());
        for a in arrays {
            match a.data_type() == &typed {
                true => same.push(a.clone()),
                false if untyped(a) => match cast(a, &typed) {
                    Ok(a) => same.push(a),
                    Err(_) => return Ok(None),
                },
                false => return Ok(None),
            }
        }
        let refs: Vec<&dyn Array> = same.iter().map(|a| a.as_ref()).collect();
        columns.push(concat(&refs).map_err(ipc_error)?);
    }
    let keys = columns.pop().expect("the keys");
    let rows = parts.iter().map(|p| p.rows).sum();
    let scores = match parts.iter().all(|p| p.scores.len() == p.rows) {
        true => parts.into_iter().flat_map(|p| p.scores).collect(),
        false => Vec::new(),
    };
    Ok(Some(Columns { names, arrays: columns, keys, scores, rows, stats }))
}

impl Columns {
    /// Rows `offset..offset + limit` (to the end without a limit), as the
    /// arrays' slices.
    pub fn slice(mut self, offset: usize, limit: Option<usize>) -> Columns {
        let at = offset.min(self.rows);
        let n = limit.map_or(self.rows - at, |l| l.min(self.rows - at));
        if (at, n) == (0, self.rows) {
            return self;
        }
        self.arrays = self.arrays.iter().map(|a| a.slice(at, n)).collect();
        self.keys = self.keys.slice(at, n);
        if self.scores.len() == self.rows {
            self.scores = self.scores[at..at + n].to_vec();
        }
        self.rows = n;
        self
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

/// The values of an Arrow array, as Strata gives them: of the types
/// [`columns_of`] makes, and of those DataFusion's answers hold.
pub fn values_of(a: &ArrayRef) -> Result<Vec<Value>, Mixed> {
    use datafusion::arrow::datatypes as t;
    let n = a.len();
    let mut out = Vec::with_capacity(n);
    macro_rules! each {
        ($arr:expr, $f:expr) => {{
            let arr = $arr;
            for i in 0..n {
                out.push(if arr.is_null(i) { Value::Null } else { $f(arr.value(i)) });
            }
        }};
    }
    match a.data_type() {
        DataType::Null => out.resize(n, Value::Null),
        DataType::Boolean => each!(a.as_boolean(), Value::Bool),
        DataType::Int8 => each!(a.as_primitive::<t::Int8Type>(), |x| Value::Int(x as i64)),
        DataType::Int16 => each!(a.as_primitive::<t::Int16Type>(), |x| Value::Int(x as i64)),
        DataType::Int32 => each!(a.as_primitive::<t::Int32Type>(), |x| Value::Int(x as i64)),
        DataType::Int64 => each!(a.as_primitive::<t::Int64Type>(), Value::Int),
        DataType::UInt8 => each!(a.as_primitive::<t::UInt8Type>(), |x| Value::Int(x as i64)),
        DataType::UInt16 => each!(a.as_primitive::<t::UInt16Type>(), |x| Value::Int(x as i64)),
        DataType::UInt32 => each!(a.as_primitive::<t::UInt32Type>(), |x| Value::Int(x as i64)),
        DataType::UInt64 => each!(a.as_primitive::<t::UInt64Type>(), |x: u64| i64::try_from(x).map_or(Value::Float(x as f64), Value::Int)),
        DataType::Float32 => each!(a.as_primitive::<t::Float32Type>(), |x| Value::Float(x as f64)),
        DataType::Float64 => each!(a.as_primitive::<t::Float64Type>(), Value::Float),
        // An exact sum: an INT where it fits, else a FLOAT.
        DataType::Decimal128(_, 0) => each!(a.as_primitive::<t::Decimal128Type>(), |x: i128| i64::try_from(x).map_or(Value::Float(x as f64), Value::Int)),
        DataType::Decimal128(_, s) => {
            let s = *s as i32;
            each!(a.as_primitive::<t::Decimal128Type>(), |x: i128| Value::Float(x as f64 / 10f64.powi(s)))
        }
        DataType::Utf8 => each!(a.as_string::<i32>(), |s: &str| Value::Str(s.to_string())),
        DataType::LargeUtf8 => each!(a.as_string::<i64>(), |s: &str| Value::Str(s.to_string())),
        DataType::Utf8View => each!(a.as_string_view(), |s: &str| Value::Str(s.to_string())),
        DataType::Timestamp(TimeUnit::Microsecond, _) => each!(a.as_primitive::<t::TimestampMicrosecondType>(), Value::Timestamp),
        DataType::List(f) => {
            let list = a.as_list::<i32>();
            let vector = matches!(f.data_type(), DataType::Float32);
            for i in 0..n {
                if list.is_null(i) {
                    out.push(Value::Null);
                    continue;
                }
                let inner = list.value(i);
                out.push(match vector {
                    true => Value::Vector(inner.as_primitive::<t::Float32Type>().values().iter().copied().collect()),
                    false => Value::List(values_of(&inner)?),
                });
            }
        }
        t => return mixed(format!("an answer column of type {t}")),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(rows: Vec<Vec<Value>>, keys: Vec<Value>) -> Columns {
        let n = rows.len();
        let width = rows.first().map_or(2, Vec::len);
        let arrays = columns_of(rows, width).unwrap();
        let keys = column_of(keys).unwrap();
        Columns { names: vec!["a".into(), "a".into()], arrays, keys, scores: (0..n).map(|i| i as f64 / 2.0).collect(), rows: n, stats: ExecStats::default() }
    }

    fn rows(c: Columns) -> Vec<Vec<Value>> {
        c.into_result().unwrap().rows
    }

    #[test]
    fn columns_travel_as_arrow_ipc() {
        let list = Value::List(vec![Value::Str("x".into()), Value::Str("y".into())]);
        let c = part(
            vec![vec![Value::Int(1), list.clone()], vec![Value::Null, Value::List(Vec::new())], vec![Value::Int(-3), Value::Null]],
            vec![Value::Str("k1".into()), Value::Str("k2".into()), Value::Str("k3".into())],
        );
        let back = Columns::from_ipc(&c.to_ipc().unwrap(), c.names.clone(), ExecStats::default()).unwrap();
        assert_eq!((back.names.clone(), back.rows, back.scores.clone()), (c.names.clone(), 3, vec![0.0, 0.5, 1.0]));
        assert_eq!(values_of(&back.keys).unwrap(), values_of(&c.keys).unwrap());
        let r = back.into_result().unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int(1), list], vec![Value::Null, Value::List(Vec::new())], vec![Value::Int(-3), Value::Null]]);
        assert_eq!(r.keys.len(), 3);
        // No rows, and no scores.
        let empty = Columns { names: Vec::new(), arrays: Vec::new(), keys: Arc::new(NullArray::new(0)), scores: Vec::new(), rows: 0, stats: ExecStats::default() };
        let back = Columns::from_ipc(&empty.to_ipc().unwrap(), Vec::new(), ExecStats::default()).unwrap();
        assert_eq!((back.rows, back.scores.len()), (0, 0));
    }

    #[test]
    fn parts_put_together_take_one_type() {
        let k = |s: &str| Value::Str(s.into());
        // The second part's first column is all NULL, its second lists
        // without values: both take the first part's types.
        let a = part(vec![vec![Value::Int(1), Value::List(vec![Value::Int(7)])]], vec![k("a")]);
        let b = part(vec![vec![Value::Null, Value::List(Vec::new())], vec![Value::Null, Value::Null]], vec![k("b"), k("c")]);
        let all = concat_columns(vec![a, b], ExecStats::default()).unwrap().expect("one type a column");
        assert_eq!((all.rows, all.scores.clone()), (3, vec![0.0, 0.0, 0.5]));
        assert_eq!(all.arrays[0].data_type(), &DataType::Int64);
        let all = all.slice(1, Some(5));
        assert_eq!(values_of(&all.keys).unwrap(), vec![k("b"), k("c")]);
        assert_eq!(rows(all), vec![vec![Value::Null, Value::List(Vec::new())], vec![Value::Null, Value::Null]]);
        // INT in one part and text in another: not put together.
        let a = part(vec![vec![Value::Int(1), Value::Null]], vec![k("a")]);
        let b = part(vec![vec![k("one"), Value::Null]], vec![k("b")]);
        assert!(concat_columns(vec![a, b], ExecStats::default()).unwrap().is_none());
        // OFFSET and LIMIT.
        let a = part((0..5).map(|i| vec![Value::Int(i), Value::Null]).collect(), (0..5).map(|i| k(&i.to_string())).collect());
        assert_eq!(rows(a.slice(3, None)), vec![vec![Value::Int(3), Value::Null], vec![Value::Int(4), Value::Null]]);
        let a = part((0..5).map(|i| vec![Value::Int(i), Value::Null]).collect(), (0..5).map(|i| k(&i.to_string())).collect());
        assert_eq!(a.slice(9, Some(2)).rows, 0);
    }
}
