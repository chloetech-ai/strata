//! Strata values and results as JSON.
//!
//! Values: `null`, numbers, strings, and arrays. An array is a vector when
//! the field is a VECTOR field, a list when it is multi-valued. Parameters
//! have no field, so an array of numbers is a vector and any other array a
//! list.

use serde_json::{json, Map, Value as Json};
use strata::query::exec::ResultSet;
use strata::query::Params;
use strata::schema::{FieldType, Schema};
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
    }
}

/// A parameter value.
pub fn param(j: &Json) -> Result<Value, ApiError> {
    Ok(match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Int(*b as i64),
        Json::Number(n) => match n.as_i64() {
            Some(x) => Value::Int(x),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(s) => Value::Str(s.clone()),
        Json::Array(xs) if !xs.is_empty() && xs.iter().all(|x| x.is_number()) => {
            Value::Vector(xs.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect::<Vec<_>>().into())
        }
        Json::Array(xs) => Value::List(xs.iter().map(param).collect::<Result<_, _>>()?),
        Json::Object(_) => return Err(ApiError::bad("parameters are numbers, strings, nulls or arrays")),
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

/// The value of field `f` of `schema` from JSON.
pub fn field_value(schema: &Schema, f: usize, j: &Json) -> Result<Value, ApiError> {
    let field = &schema.fields[f];
    let bad = || ApiError::bad(format!("bad value for {}: {j}", field.name));
    if j.is_null() {
        return Ok(Value::Null);
    }
    if field.multi {
        let items = match j {
            Json::Array(xs) => xs.clone(),
            x => vec![x.clone()],
        };
        let one = |x: &Json| match (&field.ty, x) {
            (FieldType::Keyword, Json::String(s)) => Ok(Value::Str(s.clone())),
            (FieldType::Int, Json::Number(n)) => n.as_i64().map(Value::Int).ok_or_else(bad),
            _ => Err(bad()),
        };
        return Ok(Value::List(items.iter().map(one).collect::<Result<_, _>>()?));
    }
    Ok(match (&field.ty, j) {
        (FieldType::Vector { .. }, Json::Array(xs)) => {
            Value::Vector(xs.iter().map(|x| x.as_f64().map(|v| v as f32).ok_or_else(bad)).collect::<Result<Vec<_>, _>>()?.into())
        }
        (FieldType::Int, Json::Number(n)) => Value::Int(n.as_i64().ok_or_else(bad)?),
        (FieldType::Float, Json::Number(n)) => Value::Float(n.as_f64().ok_or_else(bad)?),
        (FieldType::Keyword, Json::Number(n)) if schema.primary_key == Some(f) => Value::Str(n.to_string()),
        (FieldType::Text { .. } | FieldType::Keyword | FieldType::Edges, Json::String(s)) => Value::Str(s.clone()),
        _ => return Err(bad()),
    })
}

/// A document from a JSON object of field values (missing fields null).
pub fn document(schema: &Schema, j: &Json) -> Result<Document, ApiError> {
    let Json::Object(m) = j else { return Err(ApiError::bad("a document is a JSON object")) };
    let mut doc = vec![Value::Null; schema.fields.len()];
    for (k, v) in m {
        let f = schema.field_index(k).ok_or_else(|| ApiError::bad(format!("no field {k}")))?;
        doc[f] = field_value(schema, f, v)?;
    }
    schema.fill_generated(&mut doc);
    Ok(doc)
}

/// A document as a JSON object.
pub fn document_json(schema: &Schema, doc: &[Value]) -> Json {
    let mut m = Map::new();
    for (f, v) in schema.fields.iter().zip(doc) {
        m.insert(f.name.clone(), to_json(v));
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
