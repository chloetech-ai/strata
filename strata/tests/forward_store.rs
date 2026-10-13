//! Doc store round trips, the zero-copy contract of the forward readers,
//! and the forward sections built through `build_segment`.

mod forward_common;

use forward_common::build;
use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::forward::{DocStore, KeywordColumn, NumericColumn, RangeIndex};
use strata::segment::build::{build_segment, BuildOptions};
use strata::segment::SegmentFile;
use strata::{data, Document, Field, FieldType, Schema, Value, VectorIndexKind};

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: false }).stored_only(),
            Field::new("n", FieldType::Int),
            Field::new("x", FieldType::Float).stored_only(),
            Field::new("hidden", FieldType::Text { positions: false }).stored_only().not_stored(),
            Field::new("v", FieldType::Vector { dim: 2, index: VectorIndexKind::Flat }).stored_only(),
        ],
        Some("id"),
    )
    .unwrap()
}

fn random_docs(rng: &mut StdRng, n: usize) -> Vec<Document> {
    (0..n)
        .map(|d| {
            let len = if rng.gen_bool(0.01) { rng.gen_range(0..60_000) } else { rng.gen_range(0..200) };
            let title: String = (0..len).map(|_| ['a', 'b', ' ', 'é', '\n', 'z'][rng.gen_range(0..6)]).collect();
            let int = Value::Int(rng.gen::<i64>() >> rng.gen_range(0..64));
            let float = Value::Float((rng.gen::<f64>() - 0.5) * 1e9);
            let mut opt = |v: Value| if rng.gen_bool(0.1) { Value::Null } else { v };
            vec![
                Value::Str(format!("doc-{d}")),
                opt(Value::Str(title)),
                opt(int),
                opt(float),
                Value::Str("not stored".into()),
                Value::Vector(vec![1.0, 2.0].into()),
            ]
        })
        .collect()
}

/// What the store returns: stored fields, `Null` for the rest.
fn expected(schema: &Schema, doc: &Document) -> Document {
    schema
        .fields
        .iter()
        .zip(doc)
        .map(|(f, v)| if f.stored && !matches!(f.ty, FieldType::Vector { .. }) { v.clone() } else { Value::Null })
        .collect()
}

#[test]
fn random_documents_round_trip() {
    let mut rng = StdRng::seed_from_u64(1);
    let schema = schema();
    for (n, block) in [(0usize, 16384usize), (1, 16384), (3000, 16384), (3000, 1), (3000, 1 << 20)] {
        let docs = random_docs(&mut rng, n);
        let seg = build(&schema, &docs, &BuildOptions::default().set("forward.store_block", block));
        let store = DocStore::open(&seg, &schema).unwrap();
        assert!(store.is_view());
        assert_eq!(store.len(), n);
        for (d, doc) in docs.iter().enumerate() {
            assert_eq!(store.get(d as u32), expected(&schema, doc), "doc {d}");
        }
        for _ in 0..200.min(n) {
            let d = rng.gen_range(0..n);
            let fields: Vec<usize> = (0..rng.gen_range(0..5)).map(|_| rng.gen_range(0..schema.fields.len())).collect();
            let want: Vec<Value> = fields.iter().map(|&f| expected(&schema, &docs[d])[f].clone()).collect();
            assert_eq!(store.get_fields(d as u32, &fields), want);
        }
        let (compressed, raw) = store.byte_sizes();
        assert!(n == 0 || compressed < raw, "{compressed} vs {raw}");
    }
}

#[test]
fn store_rejects_other_schema() {
    let schema = schema();
    let docs = random_docs(&mut StdRng::seed_from_u64(2), 10);
    let seg = build(&schema, &docs, &BuildOptions::default());
    let mut other = schema.clone();
    other.fields[2].ty = FieldType::Float;
    assert!(DocStore::open(&seg, &other).is_err());
}

#[test]
fn scifact_round_trip_and_zero_copy() {
    if !data::data_dir().join("scifact/meta.json").exists() {
        eprintln!("SciFact not prepared; skipping");
        return;
    }
    let c = data::load("scifact", None).unwrap();
    let schema = data::beir_schema(c.dim, VectorIndexKind::Flat);
    let seg = build(&schema, &c.docs, &BuildOptions::default());
    // The image again through a fresh buffer: every reader holds views.
    let seg = SegmentFile::from_vec(seg.image().to_vec()).unwrap();
    let store = DocStore::open(&seg, &schema).unwrap();
    for (d, doc) in c.docs.iter().enumerate() {
        assert_eq!(store.get(d as u32), expected(&schema, doc));
    }
    let (compressed, raw) = store.byte_sizes();
    eprintln!("scifact doc store: {raw} bytes raw, {compressed} compressed, {} blocks", store.n_blocks());
    for f in ["year", "rating"] {
        let col = NumericColumn::open(&seg, f).unwrap();
        assert!(col.is_view());
        let idx = RangeIndex::open(&seg, f).unwrap();
        assert!((0..idx.len()).all(|b| idx.bucket(b).is_view()));
        let fi = schema.field_index(f).unwrap();
        for (d, doc) in c.docs.iter().enumerate() {
            assert_eq!(col.get(d as u32), doc[fi]);
        }
    }
    for f in ["category", "topic", "id"] {
        let col = KeywordColumn::open(&seg, f).unwrap();
        assert!(col.is_view());
        let fi = schema.field_index(f).unwrap();
        for (d, doc) in c.docs.iter().enumerate() {
            assert_eq!(col.value_of_doc(d as u32), doc[fi].as_str());
        }
    }
}

#[test]
fn build_segment_writes_forward_sections() {
    let schema = Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("year", FieldType::Int),
            Field::new("rating", FieldType::Float),
            Field::new("note", FieldType::Keyword).stored_only(),
        ],
        Some("id"),
    )
    .unwrap();
    let docs: Vec<Document> = (0..500)
        .map(|d| vec![Value::Str(format!("{d}")), Value::Int(1990 + d % 35), Value::Float((d % 500) as f64 / 100.0), Value::Str("n".into())])
        .collect();
    let seg = SegmentFile::from_vec(build_segment(&schema, &docs, &BuildOptions::default()).unwrap()).unwrap();
    let names: Vec<String> = seg.section_sizes().into_iter().map(|(n, _)| n).collect();
    for s in ["col.year", "col.year.hist", "range.year", "col.rating", "kw.id.dict", "kw.id.codes", "kw.id.post", "store.index", "store.data"] {
        assert!(names.iter().any(|n| n == s), "missing {s} in {names:?}");
    }
    assert!(!names.iter().any(|n| n.starts_with("kw.note")));
    let want = (0..500).filter(|d| (10..20).contains(&(d % 35))).count();
    assert_eq!(NumericColumn::open(&seg, "year").unwrap().scan_count(2000.0, 2009.0), want);
    assert_eq!(DocStore::open(&seg, &schema).unwrap().get(7)[3], Value::Str("n".into()));
}
