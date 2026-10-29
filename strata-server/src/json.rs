//! Strata values and results as JSON.
//!
//! Values: `null`, numbers, strings, booleans, arrays and objects. A BOOL
//! field's values are `true` and `false`, a TIMESTAMP field's ISO 8601
//! text in UTC (`"2026-10-01T12:30:00Z"`). An array is a
//! vector when the field is a VECTOR field, a list when it is multi-valued.
//! An object of numbers is a SPARSE field's term weights (held as its JSON
//! text, which SPARSE fields also accept). Parameters have no field, so an
//! array of numbers is a vector, any other array a list, and an object of
//! numbers sparse weights.

use serde_json::{json, Map, Value as Json};
use strata::query::exec::ResultSet;
use strata::query::Params;
use strata::schema::Schema;
use strata::value::{Document, Value};

use crate::ApiError;

pub fn to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Int(x) => json!(x),
        Value::Float(x) if x.is_finite() => json!(x),
        Value::Float(_) => Json::Null,
        Value::Str(s) => json!(s),
        Value::Vector(v) => Json::Array(v.iter().map(|x| json!(x)).collect()),
        Value::List(xs) => Json::Array(xs.iter().map(to_json).collect()),
        Value::Bool(b) => json!(b),
        Value::Timestamp(t) => json!(strata::timestamp::format(*t)),
    }
}

/// A parameter value.
pub fn param(j: &Json) -> Result<Value, ApiError> {
    Ok(match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(x) => Value::Int(x),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(s) => Value::Str(s.clone()),
        // Whole numbers stay exact integers (`IN (:ids)` with 64-bit ids);
        // where a vector is expected, a list of numbers is one.
        Json::Array(xs) if !xs.is_empty() && xs.iter().all(|x| x.is_i64()) => Value::List(xs.iter().map(|x| Value::Int(x.as_i64().unwrap_or(0))).collect()),
        Json::Array(xs) if !xs.is_empty() && xs.iter().all(|x| x.is_number()) => {
            Value::Vector(xs.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect::<Vec<_>>().into())
        }
        Json::Array(xs) => Value::List(xs.iter().map(param).collect::<Result<_, _>>()?),
        Json::Object(m) if m.values().all(|w| w.is_number()) => Value::Str(j.to_string()),
        Json::Object(_) => return Err(ApiError::bad("parameters are numbers, strings, nulls, arrays or objects of term weights")),
    })
}

pub fn params(j: Option<&Json>) -> Result<Params, ApiError> {
    let mut p = Params::new();
    match j {
        None | Some(Json::Null) => {}
        Some(Json::Object(m)) => {
            for (k, v) in m {
                p = p.set(k, param(v)?);
            }
        }
        Some(_) => return Err(ApiError::bad("params must be an object")),
    }
    Ok(p)
}

/// The value of field `f` of `schema` from JSON ([`strata::ingest::value`]).
pub fn field_value(schema: &Schema, f: usize, j: &Json) -> Result<Value, ApiError> {
    Ok(strata::ingest::value(schema, f, j)?)
}

/// A document from a JSON object of field values (missing fields take
/// their defaults or are null; a TIMESTAMP defaulting to `"now"` takes
/// `now`, the time of the request).
pub fn document(schema: &Schema, j: &Json, now: i64) -> Result<Document, ApiError> {
    Ok(strata::ingest::document_at(schema, j, now)?)
}

/// A document as a JSON object.
pub fn document_json(schema: &Schema, doc: &[Value]) -> Json {
    let mut m = Map::new();
    for (f, v) in schema.fields.iter().zip(doc) {
        m.insert(f.name.clone(), to_json(&f.output(v.clone())));
    }
    Json::Object(m)
}

pub fn result(rs: &ResultSet, took_ms: f64) -> Json {
    json!({
        "columns": rs.columns,
        "rows": rs.rows.iter().map(|r| r.iter().map(to_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "keys": rs.keys.iter().map(to_json).collect::<Vec<_>>(),
        "scores": rs.scores.iter().map(|s| if s.is_finite() { json!(s) } else { Json::Null }).collect::<Vec<_>>(),
        "parts": rs.parts,
        "stats": rs.stats,
        "took_ms": took_ms,
    })
}
