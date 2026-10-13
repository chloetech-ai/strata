//! Turning documents into a segment.

use std::collections::BTreeMap;

use crate::error::Result;
use crate::schema::{FieldType, Schema};
use crate::segment::{DocId, FieldIndexBuilder, SegmentWriter};
use crate::value::Document;

/// Build-time knobs, mostly for experiments that compare encodings
/// (e.g. `text.postings = loom | bp128`). Components document their keys.
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    pub params: BTreeMap<String, String>,
}

impl BuildOptions {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(|s| s.as_str())
    }
    pub fn get_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }
    pub fn set(mut self, key: &str, value: impl ToString) -> Self {
        self.params.insert(key.to_string(), value.to_string());
        self
    }
}

/// Builds the segment image for `docs` (local docIDs follow slice order).
pub fn build_segment(schema: &Schema, docs: &[Document], opts: &BuildOptions) -> Result<Vec<u8>> {
    for d in docs {
        crate::value::validate(schema, d)?;
    }
    let mut out = SegmentWriter::new(docs.len());
    let mut builders: Vec<(usize, Box<dyn FieldIndexBuilder>)> = Vec::new();
    for (i, f) in schema.fields.iter().enumerate() {
        let b: Option<Box<dyn FieldIndexBuilder>> = match &f.ty {
            FieldType::Text { .. } if f.indexed => Some(crate::text::field_builder(f, opts)?),
            FieldType::Text { .. } => None,
            FieldType::Keyword | FieldType::Int | FieldType::Float if f.indexed => {
                Some(crate::forward::field_builder(f, opts)?)
            }
            FieldType::Keyword | FieldType::Int | FieldType::Float => None,
            // Vector fields always store their vectors; `indexed` adds the ANN index.
            FieldType::Vector { .. } => Some(crate::vector::field_builder(f, opts)?),
            FieldType::Edges if f.indexed => {
                let keys = schema.primary_key.map_or_else(Vec::new, |pk| docs.iter().map(|d| d[pk].clone()).collect());
                Some(crate::graph::field_builder(f, schema, keys)?)
            }
            FieldType::Edges => None,
        };
        if let Some(b) = b {
            builders.push((i, b));
        }
    }
    for (d, doc) in docs.iter().enumerate() {
        for (i, b) in builders.iter_mut() {
            b.add(d as DocId, &doc[*i])?;
        }
    }
    for (_, b) in builders {
        b.finish(&mut out)?;
    }
    crate::forward::build_doc_store(schema, docs, opts, &mut out)?;
    crate::storage::build_pk_index(schema, docs, &mut out)?;
    Ok(out.finish())
}
