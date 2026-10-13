//! Helpers shared by the forward-index tests (each test crate uses a
//! subset).
#![allow(dead_code)]

use strata::forward;
use strata::segment::build::BuildOptions;
use strata::segment::{DocSet, SegmentFile, SegmentWriter};
use strata::{Document, FieldType, Schema};

/// A segment with only the forward-index sections of `docs` (columns,
/// postings, doc store), so the tests do not depend on other components.
pub fn build(schema: &Schema, docs: &[Document], opts: &BuildOptions) -> SegmentFile {
    for d in docs {
        strata::value::validate(schema, d).unwrap();
    }
    let mut out = SegmentWriter::new(docs.len());
    for (i, f) in schema.fields.iter().enumerate() {
        if f.indexed && matches!(f.ty, FieldType::Keyword | FieldType::Int | FieldType::Float) {
            let mut b = forward::field_builder(f, opts).unwrap();
            for (d, doc) in docs.iter().enumerate() {
                b.add(d as u32, &doc[i]).unwrap();
            }
            b.finish(&mut out).unwrap();
        }
    }
    forward::build_doc_store(schema, docs, opts, &mut out).unwrap();
    SegmentFile::from_vec(out.finish()).unwrap()
}

/// A random docID set over `0..n` with about `density * n` members.
pub fn random_docset(rng: &mut impl rand::Rng, n: u32, density: f64) -> DocSet {
    let docs: Vec<u32> = (0..n).filter(|_| rng.gen_bool(density)).collect();
    DocSet::from_sorted(docs, n)
}
