//! Turning documents into a segment.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

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

/// Seconds spent in each part of segment builds since the last
/// [`take_build_times`], summed over all builds and threads.
static BUILD_TIMES: Mutex<BTreeMap<String, f64>> = Mutex::new(BTreeMap::new());

fn add_build_time(part: &str, since: Instant) {
    let mut t = BUILD_TIMES.lock().unwrap_or_else(|e| e.into_inner());
    *t.entry(part.to_string()).or_insert(0.0) += since.elapsed().as_secs_f64();
}

/// Seconds spent in each part of segment builds since the last call, by
/// part: `<field> (<type>)` for an index (`emb (vector)` includes the SQ8
/// codes and the ANN graph), `doc store`, `keys`. Resets the counters.
pub fn take_build_times() -> BTreeMap<String, f64> {
    std::mem::take(&mut *BUILD_TIMES.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Builds the segment image for `docs` (local docIDs follow slice order).
pub fn build_segment(schema: &Schema, docs: &[Document], opts: &BuildOptions) -> Result<Vec<u8>> {
    for d in docs {
        crate::value::validate(schema, d)?;
    }
    let mut out = SegmentWriter::new(docs.len());
    let mut builders: Vec<(usize, Box<dyn FieldIndexBuilder>)> = Vec::new();
    let kind = |ty: &FieldType| match ty {
        FieldType::Text { .. } => "text",
        FieldType::Keyword => "keyword",
        FieldType::Int => "int",
        FieldType::Float => "float",
        FieldType::Vector { .. } => "vector",
        FieldType::Sparse => "sparse",
        FieldType::Edges => "edges",
    };
    for (i, f) in schema.fields.iter().enumerate() {
        let b: Option<Box<dyn FieldIndexBuilder>> = match &f.ty {
            FieldType::Text { .. } if f.indexed => Some(crate::text::field_builder(f, opts)?),
            FieldType::Text { .. } => None,
            FieldType::Sparse if f.indexed => Some(crate::text::sparse::field_builder(f, opts)?),
            FieldType::Sparse => None,
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
    for (i, mut b) in builders {
        let t = Instant::now();
        for (d, doc) in docs.iter().enumerate() {
            b.add(d as DocId, &doc[i])?;
        }
        b.finish(&mut out)?;
        add_build_time(&format!("{} ({})", schema.fields[i].name, kind(&schema.fields[i].ty)), t);
    }
    let t = Instant::now();
    crate::forward::build_doc_store(schema, docs, opts, &mut out)?;
    add_build_time("doc store", t);
    let t = Instant::now();
    crate::storage::build_pk_index(schema, docs, &mut out)?;
    add_build_time("keys", t);
    Ok(out.finish())
}
