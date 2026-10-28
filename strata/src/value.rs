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
    /// The values of a multi-valued KEYWORD or INT field
    /// ([`crate::schema::Field::multi`]).
    List(Vec<Value>),
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
    /// The values of a list, or the value itself as a list of one (`Null`
    /// as none).
    pub fn items(&self) -> &[Value] {
        match self {
            Value::List(v) => v,
            Value::Null => &[],
            v => std::slice::from_ref(v),
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
        if f.multi {
            let ok = v.items().iter().all(|x| matches!((&f.ty, x), (FieldType::Keyword, Value::Str(_)) | (FieldType::Int, Value::Int(_))));
            if !ok {
                return Err(Error::Schema(format!("bad value for multi-valued field {}: {:?}", f.name, short(v))));
            }
            continue;
        }
        let ok = match (&f.ty, v) {
            (_, Value::Null) => schema.primary_key.map_or(true, |pk| schema.fields[pk].name != f.name),
            (FieldType::Text { .. } | FieldType::Keyword, Value::Str(_)) => true,
            (FieldType::Int, Value::Int(_)) => true,
            // NaN and infinities have no place in an order or a sum (as in
            // Elasticsearch and Lucene, which refuse them too).
            (FieldType::Float, Value::Float(x)) => x.is_finite(),
            (FieldType::Float, Value::Int(_)) => true,
            (FieldType::Vector { dim, .. }, Value::Vector(x)) => x.len() == *dim && x.iter().all(|c| c.is_finite()),
            (FieldType::Edges, Value::Str(_)) => {
                crate::graph::parse_edges(v, crate::graph::int_keys(schema))?;
                true
            }
            (FieldType::Sparse, Value::Str(_)) => {
                crate::text::sparse::parse(v)?;
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
    match s.char_indices().nth(60) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s,
    }
}
