//! Field values and documents.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::schema::{FieldType, Schema};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Str(String),
    /// Shared so that bulk loads can hand out vectors without copying.
    Vector(Arc<[f32]>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Float(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_vector(&self) -> Option<&[f32]> {
        match self {
            Value::Vector(v) => Some(v),
            _ => None,
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}

impl From<i64> for Value {
    fn from(x: i64) -> Value {
        Value::Int(x)
    }
}

/// One row: a value per schema field, in schema order.
pub type Document = Vec<Value>;

/// Checks that `doc` has one value of the right type per field.
pub fn validate(schema: &Schema, doc: &Document) -> Result<()> {
    if doc.len() != schema.fields.len() {
        return Err(Error::Schema(format!("document has {} values, table has {} fields", doc.len(), schema.fields.len())));
    }
    for (f, v) in schema.fields.iter().zip(doc) {
        let ok = match (&f.ty, v) {
            (_, Value::Null) => schema.primary_key.map_or(true, |pk| schema.fields[pk].name != f.name),
            (FieldType::Text { .. } | FieldType::Keyword, Value::Str(_)) => true,
            (FieldType::Int, Value::Int(_)) => true,
            (FieldType::Float, Value::Float(_) | Value::Int(_)) => true,
            (FieldType::Vector { dim, .. }, Value::Vector(x)) => x.len() == *dim,
            (FieldType::Edges, Value::Str(_)) => {
                crate::graph::parse_edges(v, crate::graph::int_keys(schema))?;
                true
            }
            _ => false,
        };
        if !ok {
            return Err(Error::Schema(format!("bad value for field {}: {:?}", f.name, short(v))));
        }
    }
    Ok(())
}

fn short(v: &Value) -> String {
    let s = format!("{v:?}");
    if s.len() > 60 {
        format!("{}...", &s[..60])
    } else {
        s
    }
}
