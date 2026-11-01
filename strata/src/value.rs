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
    /// A BOOL field's value. Its column holds 1 or 0
    /// ([`crate::schema::Logical`]): queries return this, and writes take
    /// it or the integer.
    Bool(bool),
    /// A TIMESTAMP field's value, in microseconds since
    /// 1970-01-01T00:00:00Z ([`crate::timestamp`]). Its column holds the
    /// integer: queries return this, and writes take it, the integer or
    /// ISO 8601 text.
    Timestamp(i64),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    /// An integer, or a boolean as 1 or 0.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }
    /// A number, or a boolean as 1 or 0.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Float(v) => Some(*v),
            Value::Bool(b) => Some(*b as i64 as f64),
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

/// Puts `doc`'s values in the form the table stores
/// ([`crate::schema::Field::store`]: BOOL and TIMESTAMP values as
/// integers, a boolean in another INT or FLOAT field as 1 or 0, a JSON
/// field's object as compact text, [`crate::json::canonical`]) and
/// [`validate`]s it. Writes call this; documents read back hold the
/// stored form.
pub fn conform(schema: &Schema, doc: &mut Document) -> Result<()> {
    conform_as(schema, doc, Pooling::All)
}

/// Which multi-vector values of a document a write pools
/// ([`crate::vector::multi::pooled`]): its new values. A row read back and
/// written again keeps the vectors it has, which were pooled when they were
/// written: pooling by a factor would shrink them again.
#[derive(Clone, Copy, Debug)]
pub enum Pooling<'a> {
    /// Every value is new.
    All,
    /// None is (a primary's rows replayed on a replica).
    None,
    /// The fields at these positions are new (those an UPDATE assigns; the
    /// rest of the row was read back).
    Fields(&'a [usize]),
}

/// [`conform`], pooling the multi-vector values `pooling` names.
pub fn conform_as(schema: &Schema, doc: &mut Document, pooling: Pooling) -> Result<()> {
    let typed = |v: &Value| matches!(v, Value::Bool(_) | Value::Timestamp(_));
    for (i, (f, v)) in schema.fields.iter().zip(doc.iter_mut()).enumerate() {
        if f.is_multi_vector() {
            *v = crate::vector::multi::normalize(f, std::mem::replace(v, Value::Null))?;
            if match pooling {
                Pooling::All => true,
                Pooling::None => false,
                Pooling::Fields(new) => new.contains(&i),
            } {
                *v = crate::vector::multi::pooled(f, std::mem::replace(v, Value::Null));
            }
            continue;
        }
        if f.ty == FieldType::Json {
            *v = crate::json::canonical(std::mem::replace(v, Value::Null), &f.name)?;
            continue;
        }
        if f.logical.is_some() || typed(v) || matches!(v, Value::List(xs) if xs.iter().any(typed)) {
            *v = f.store(std::mem::replace(v, Value::Null))?;
        }
    }
    validate(schema, doc)
}

/// Checks that `doc` has one value of the right type per field, in the
/// stored form ([`conform`]).
pub fn validate(schema: &Schema, doc: &Document) -> Result<()> {
    if doc.len() != schema.fields.len() {
        return Err(Error::Schema(format!("document has {} values, table has {} fields", doc.len(), schema.fields.len())));
    }
    for (f, v) in schema.fields.iter().zip(doc) {
        if f.not_null && matches!(v, Value::Null) {
            return Err(Error::Schema(format!("{} is NOT NULL: give it a value", f.name)));
        }
        if f.multi {
            let ok = v.items().iter().all(|x| match (&f.ty, x) {
                (FieldType::Keyword, Value::Str(_)) | (FieldType::Int, Value::Int(_)) => true,
                (FieldType::Vector { dim, .. }, Value::Vector(x)) => x.len() == *dim && x.iter().all(|c| c.is_finite()),
                _ => false,
            });
            if !ok {
                return Err(Error::Schema(format!("bad value for multi-valued field {}: {:?}", f.name, short(v))));
            }
            continue;
        }
        let ok = match (&f.ty, v) {
            (_, Value::Null) => schema.primary_key.map_or(true, |pk| schema.fields[pk].name != f.name),
            (FieldType::Keyword, Value::Str(t)) if f.media.is_some() => {
                let m = crate::media::Media::parse(t).map_err(|e| Error::Schema(format!("field {}: {e} (a MEDIA value is stored with its uri, sha256, mime and bytes; a write's uri or data is resolved before the writer, by Db::fill_in or a server)", f.name)))?;
                f.media.is_some_and(|k| k.accepts(&m.mime))
            }
            (FieldType::Text { .. } | FieldType::Keyword, Value::Str(_)) => true,
            (FieldType::Int, Value::Int(x)) => f.logical != Some(crate::schema::Logical::Bool) || matches!(x, 0 | 1),
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
            (FieldType::Json, Value::Str(s)) => {
                let j: serde_json::Value = serde_json::from_str(s).map_err(|e| Error::Schema(format!("field {}: not JSON ({e})", f.name)))?;
                crate::json::check(&j).map_err(|e| Error::Schema(format!("field {}: {e}", f.name)))?;
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
