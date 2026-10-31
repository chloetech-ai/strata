//! Learned sparse fields: a weight per term, as SPLADE, BGE-M3's sparse
//! output or OpenSearch's neural sparse models produce, ranked by the dot
//! product with a query's term weights.
//!
//! A SPARSE field's value is a JSON object of terms and non-negative
//! weights, `{"tok": 1.25, "##en": 0.4}`, held as a string (like an EDGES
//! field's JSON array). Terms are taken as given, not analysed. The field
//! is stored as a text index ([`super`]) whose posting frequencies are the
//! weights quantised to steps of `1 / SCALE` (weights below half a step are
//! dropped), so the text index's top-k algorithms rank it, with bounds from
//! the same block impacts: `score = Σ_t q_t · w_t` over the terms the
//! document and the query share.
//!
//! In SQL: `ORDER BY similarity(field, :q)`, `:q` being a JSON object of the
//! query's term weights (see [`parse`]).

use std::collections::BTreeMap;

use super::builder::TextFieldBuilder;
use crate::error::{Error, Result};
use crate::schema::{Field, FieldType};
use crate::segment::build::BuildOptions;
use crate::segment::{DocId, FieldIndexBuilder, SegmentWriter};
use crate::value::Value;

/// Quantisation steps per unit of weight: a stored weight is
/// `round(w · SCALE) / SCALE`.
pub const SCALE: f32 = 1024.0;

/// The largest weight a document can store (the posting frequency limit).
pub const MAX_WEIGHT: f32 = ((1u32 << 24) - 1) as f32 / SCALE;

/// The terms and weights of a sparse value: a JSON object in a string
/// (`Null` is empty). Weights must be finite and non-negative; a term given
/// twice keeps its last weight (JSON objects do not repeat keys).
pub fn parse(v: &Value) -> Result<Vec<(String, f32)>> {
    let s = match v {
        Value::Null => return Ok(Vec::new()),
        Value::Str(s) => s,
        v => return Err(Error::Schema(format!("a sparse value is a JSON object of term weights, not {v:?}"))),
    };
    let map: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(s).map_err(|e| Error::Schema(format!("a sparse value is a JSON object of term weights: {e}")))?;
    map.into_iter()
        .map(|(t, w)| match w.as_f64() {
            Some(x) if x.is_finite() && x >= 0.0 && x as f32 <= MAX_WEIGHT => Ok((t, x as f32)),
            _ => Err(Error::Schema(format!("sparse weight of {t:?} must be a number from 0 to {MAX_WEIGHT}, not {w}"))),
        })
        .collect()
}

/// A sparse value from terms and weights.
pub fn to_value(terms: &[(String, f32)]) -> Value {
    let map: serde_json::Map<String, serde_json::Value> = terms.iter().map(|(t, w)| (t.clone(), serde_json::json!(w))).collect();
    Value::Str(serde_json::Value::Object(map).to_string())
}

/// The stored (quantised) weight of `w`.
pub fn quantise(w: f32) -> u32 {
    (w * SCALE).round() as u32
}

/// Builder for an indexed SPARSE field.
pub fn field_builder(field: &Field, opts: &BuildOptions) -> Result<Box<dyn FieldIndexBuilder>> {
    if field.ty != FieldType::Sparse {
        return Err(Error::Schema(format!("field {} is not SPARSE", field.name)));
    }
    let (format, loom_min_df) = super::builder::options(opts)?;
    Ok(Box::new(SparseBuilder { field: field.name.clone(), n: 0, text: TextFieldBuilder::new(&field.key(), false, format, loom_min_df) }))
}

struct SparseBuilder {
    field: String,
    n: usize,
    text: TextFieldBuilder,
}

impl FieldIndexBuilder for SparseBuilder {
    fn add(&mut self, doc: DocId, value: &Value) -> Result<()> {
        if doc as usize != self.n {
            return Err(Error::Schema(format!("sparse field {}: document {doc} added out of order", self.field)));
        }
        let terms: Vec<(String, u32)> = parse(value)?.into_iter().map(|(t, w)| (t, quantise(w))).filter(|(_, q)| *q > 0).collect();
        self.text.add_weighted(&terms);
        self.n += 1;
        Ok(())
    }

    fn finish(mut self: Box<Self>, out: &mut SegmentWriter) -> Result<()> {
        self.text.seal();
        self.text.write(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_and_quantise() {
        let v = Value::Str(r#"{"b": 0.5, "a": 2, "z": 0}"#.into());
        assert_eq!(parse(&v).unwrap(), vec![("a".into(), 2.0), ("b".into(), 0.5), ("z".into(), 0.0)]);
        assert_eq!(parse(&Value::Null).unwrap(), vec![]);
        assert_eq!(parse(&to_value(&[("x".into(), 1.5)])).unwrap(), vec![("x".into(), 1.5)]);
        for bad in [r#"{"a": -1}"#, r#"{"a": "high"}"#, r#"["a"]"#, "a:1", r#"{"a": 1e9}"#] {
            assert!(parse(&Value::Str(bad.into())).is_err(), "{bad}");
        }
        assert!(parse(&Value::Int(3)).is_err());
        assert_eq!(quantise(1.0), 1024);
        assert_eq!(quantise(0.4 / SCALE), 0);
        assert_eq!(quantise(0.6 / SCALE), 1);
    }
}
