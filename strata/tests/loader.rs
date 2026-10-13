//! Loads SciFact (if prepared) and builds a segment through the skeleton.

use strata::segment::build::{build_segment, BuildOptions};
use strata::segment::SegmentFile;
use strata::{data, VectorIndexKind};

#[test]
fn scifact_loads_and_builds() {
    if !data::data_dir().join("scifact/meta.json").exists() {
        eprintln!("SciFact not prepared; skipping");
        return;
    }
    let c = data::load("scifact", Some(500)).unwrap();
    assert_eq!(c.docs.len(), 500);
    assert_eq!(c.dim, 1024);
    assert_eq!(c.queries.len(), 300);
    let schema = data::beir_schema(c.dim, VectorIndexKind::Flat);
    let seg = SegmentFile::from_vec(build_segment(&schema, &c.docs, &BuildOptions::default()).unwrap()).unwrap();
    assert_eq!(seg.n_docs(), 500);
}
