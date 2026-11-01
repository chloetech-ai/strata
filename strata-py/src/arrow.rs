//! Answers as Arrow (feature `datafusion`): a query's rows as one Arrow
//! record batch, which Python libraries take through the Arrow PyCapsule
//! interface (`__arrow_c_stream__`) without copying it: pyarrow, polars,
//! DuckDB, and pandas through pyarrow.
//!
//! A column has the Arrow type of its values, as Strata's DataFusion
//! engine types them ([`strata::query::arrow`]): INT as Int64, FLOAT as
//! Float64, text and keywords as Utf8, BOOL as Boolean, TIMESTAMP as
//! microseconds in UTC, a vector as a list of Float32 (a multi-vector
//! field's as a list of those), a list field as a list of Int64 or Utf8,
//! and a column of NULLs as Null. A column of both
//! integers and floats is Float64; values of other types mixed in one
//! column are an error, as Arrow holds one type a column.

use std::ffi::CStr;
use std::sync::Arc;

use arrow_array::ffi_stream::FFI_ArrowArrayStream;
use arrow_array::builder::{Float32Builder, ListBuilder};
use arrow_array::{Array, ArrayRef, Float64Array, RecordBatch, RecordBatchIterator, RecordBatchOptions, TimestampMicrosecondArray};
use arrow_schema::ffi::FFI_ArrowSchema;
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use pyo3::prelude::*;
use pyo3::types::PyCapsule;
use strata::query::arrow::column_of;
use strata::query::exec::Leaf;
use strata::Value;

use crate::StrataError;

const STREAM: &CStr = c"arrow_array_stream";
const SCHEMA: &CStr = c"arrow_schema";

/// A query's answer as Arrow columns.
#[pyclass(frozen, module = "strata._strata")]
pub struct Arrow {
    batch: RecordBatch,
    /// What the engine did, as JSON text.
    #[pyo3(get)]
    stats: String,
}

impl Arrow {
    /// A query's answer ([`strata::db::Db::query_leaf`]) as Arrow columns,
    /// each named as the query names it: the executor's arrays, or its
    /// rows made arrays.
    pub fn of(leaf: Leaf) -> strata::Result<Arrow> {
        let (names, arrays, rows, stats) = match leaf {
            Leaf::Arrow(c) => (c.names, c.arrays, c.rows, c.stats),
            Leaf::Rows(rs) => {
                let n = rs.rows.len();
                (rs.columns.clone(), arrays_of(&rs.columns, rs.rows)?, n, rs.stats)
            }
        };
        let stats = serde_json::to_string(&stats).unwrap_or_default();
        Ok(Arrow { batch: batch_of(&names, arrays.into_iter().map(utc).collect(), rows)?, stats })
    }
}

#[pymethods]
impl Arrow {
    /// The columns' names, in order.
    #[getter]
    fn columns(&self) -> Vec<String> {
        self.batch.schema().fields().iter().map(|f| f.name().clone()).collect()
    }

    #[getter]
    fn num_rows(&self) -> usize {
        self.batch.num_rows()
    }

    fn __len__(&self) -> usize {
        self.batch.num_rows()
    }

    /// The rows as an Arrow C stream of one record batch, in a capsule
    /// (the Arrow PyCapsule interface). The batch's buffers are shared,
    /// not copied: each call gives a new stream of the same batch. A
    /// requested schema is not followed.
    #[pyo3(signature = (requested_schema = None))]
    fn __arrow_c_stream__<'py>(&self, py: Python<'py>, requested_schema: Option<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyCapsule>> {
        let _ = requested_schema;
        let reader = RecordBatchIterator::new([Ok(self.batch.clone())], self.batch.schema());
        PyCapsule::new_with_value(py, FFI_ArrowArrayStream::new(Box::new(reader)), STREAM)
    }

    /// The columns' Arrow schema, in a capsule.
    fn __arrow_c_schema__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyCapsule>> {
        let schema = FFI_ArrowSchema::try_from(self.batch.schema().as_ref()).map_err(|e| StrataError::new_err(e.to_string()))?;
        PyCapsule::new_with_value(py, schema, SCHEMA)
    }

    fn __repr__(&self) -> String {
        let n = self.batch.num_rows();
        format!("<strata.ArrowResult {n} row{}: {}>", if n == 1 { "" } else { "s" }, self.columns().join(", "))
    }
}

/// Rows of `columns.len()` values as arrays, one a column (a row with
/// fewer is NULL in the rest).
fn arrays_of(columns: &[String], rows: Vec<Vec<Value>>) -> strata::Result<Vec<ArrayRef>> {
    let n = rows.len();
    let mut cols: Vec<Vec<Value>> = columns.iter().map(|_| Vec::with_capacity(n)).collect();
    for r in rows {
        let k = r.len();
        for (c, v) in cols.iter_mut().zip(r) {
            c.push(v);
        }
        cols.iter_mut().skip(k).for_each(|c| c.push(Value::Null));
    }
    cols.into_iter().zip(columns).map(|(values, name)| array_of(name, values)).collect()
}

/// `arrays`, of `n` rows each, as one record batch.
fn batch_of(names: &[String], arrays: Vec<ArrayRef>, n: usize) -> strata::Result<RecordBatch> {
    let fields: Vec<Field> = names.iter().zip(&arrays).map(|(name, a)| Field::new(name, a.data_type().clone(), true)).collect();
    let opts = RecordBatchOptions::new().with_row_count(Some(n));
    RecordBatch::try_new_with_options(Arc::new(ArrowSchema::new(fields)), arrays, &opts).map_err(|e| strata::Error::Query(format!("arrow: {e}")))
}

/// A column of times in UTC, which Strata's are; any other as it is.
fn utc(a: ArrayRef) -> ArrayRef {
    match (a.data_type(), a.as_any().downcast_ref::<TimestampMicrosecondArray>()) {
        (DataType::Timestamp(_, None), Some(t)) => Arc::new(t.clone().with_timezone("UTC")),
        _ => a,
    }
}

/// One column's values as an array of their one type.
fn array_of(name: &str, values: Vec<Value>) -> strata::Result<ArrayRef> {
    let numbers = |v: &Value| matches!(v, Value::Null | Value::Int(_) | Value::Float(_));
    if values.iter().any(|v| matches!(v, Value::Int(_))) && values.iter().any(|v| matches!(v, Value::Float(_))) && values.iter().all(numbers) {
        return Ok(Arc::new(values.iter().map(Value::as_f64).collect::<Float64Array>()));
    }
    // A multi-vector field's: a list of vectors a row.
    let vectors = |v: &Value| match v {
        Value::List(xs) => xs.iter().all(|x| matches!(x, Value::Vector(_))),
        v => matches!(v, Value::Null),
    };
    if values.iter().any(|v| matches!(v, Value::List(xs) if !xs.is_empty())) && values.iter().all(vectors) {
        let mut b = ListBuilder::new(ListBuilder::new(Float32Builder::new()));
        for v in &values {
            match v {
                Value::List(xs) => {
                    for x in xs {
                        b.values().values().append_slice(x.as_vector().unwrap_or_default());
                        b.values().append(true);
                    }
                    b.append(true);
                }
                _ => b.append(false),
            }
        }
        return Ok(Arc::new(b.finish()));
    }
    column_of(values).map_err(|m| strata::Error::Query(format!("column {name}: {}; an Arrow column holds values of one type (query() answers it)", m.0)))
}
