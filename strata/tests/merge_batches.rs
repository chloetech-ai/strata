//! A segment built from batches of documents is the same bytes as one built
//! from all of them at once, and a merge decodes its inputs' documents a
//! batch at a time, not a whole segment.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::db::{Db, StoredSource};
use strata::graph::{edges_value, Edge};
use strata::query::Params;
use strata::segment::build::{build_segment, BuildOptions, SegmentBuild};
use strata::segment::{DocId, SegmentFile};
use strata::storage::{DbOptions, DocumentSource, MergePolicy};
use strata::text::sparse::to_value;
use strata::value::{Document, Value};
use strata::{Field, FieldType, Schema, VectorIndexKind};

/// A field of every type, TEXT with and without positions, a list, and
/// edges between rows.
fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: false }),
            Field::new("body", FieldType::Text { positions: true }),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("year", FieldType::Int),
            Field::new("rating", FieldType::Float),
            Field::new("emb", FieldType::Vector { dim: 8, index: VectorIndexKind::Hnsw { m: 8, ef_construction: 40 } }),
            Field::new("sp", FieldType::Sparse),
            Field::new("links", FieldType::Edges),
        ],
        Some("id"),
    )
    .unwrap()
}

fn docs(n: usize) -> Vec<Document> {
    let mut rng = StdRng::seed_from_u64(3);
    let word = |rng: &mut StdRng| format!("w{}", (rng.gen::<f64>().powi(3) * 5000.0) as u32);
    (0..n)
        .map(|i| {
            let words = |rng: &mut StdRng, k: usize| (0..k).map(|_| word(rng)).collect::<Vec<_>>().join(" ");
            let tags = (0..rng.gen_range(0..4)).map(|_| Value::Str(format!("t{}", rng.gen_range(0..50)))).collect();
            let emb: Vec<f32> = (0..8).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let sp: Vec<(String, f32)> = (0..rng.gen_range(1..6)).map(|_| (word(&mut rng), rng.gen_range(0.1..3.0))).collect();
            let links: Vec<Edge> = (0..rng.gen_range(0..3)).map(|_| Edge::to(Value::Str(format!("d{:06}", rng.gen_range(0..n + 100))))).collect();
            vec![
                Value::Str(format!("d{i:06}")),
                Value::Str(words(&mut rng, 5)),
                Value::Str(words(&mut rng, 40)),
                Value::List(tags),
                if i % 10 == 0 { Value::Null } else { Value::Int(1990 + rng.gen_range(0..30)) },
                Value::Float(rng.gen_range(0.0..5.0)),
                Value::Vector(emb.into()),
                to_value(&sp),
                edges_value(&links),
            ]
        })
        .collect()
}

#[test]
fn batches_build_the_same_segment() {
    let (schema, docs) = (schema(), docs(12_000));
    for threads in [1, 4] {
        let opts = BuildOptions::default().set("build.threads", threads);
        let whole = build_segment(&schema, &docs, &opts).unwrap();
        for sizes in [vec![12_000], vec![1, 11_999], vec![3000, 3000, 3000, 3000], vec![5000, 6999, 1], vec![4096, 4096, 3808, 0]] {
            let mut b = SegmentBuild::new(&schema, &opts, docs.len()).unwrap();
            let mut at = 0;
            for &n in &sizes[..sizes.len() - 1] {
                b.add(&docs[at..at + n]).unwrap();
                at += n;
            }
            let image = b.finish(&docs[at..]).unwrap().finish();
            assert!(image == whole, "{threads} threads, batches of {sizes:?}");
        }
    }
    // The documents must add up to the count the build was made for.
    let opts = BuildOptions::default();
    let mut b = SegmentBuild::new(&schema, &opts, 10).unwrap();
    assert!(b.add(&docs[..11]).is_err());
    assert!(SegmentBuild::new(&schema, &opts, 10).unwrap().finish(&docs[..9]).is_err());
}

/// The stored-field source, counting what merges ask it for.
#[derive(Default)]
struct Counting {
    calls: AtomicUsize,
    most: AtomicUsize,
}

impl DocumentSource for Counting {
    fn documents(&self, schema: &Schema, seg: &SegmentFile, docs: &[DocId]) -> strata::error::Result<Vec<Document>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.most.fetch_max(docs.len(), Ordering::Relaxed);
        StoredSource.documents(schema, seg, docs)
    }
}

#[test]
fn merges_decode_documents_in_batches() {
    let schema = Schema::new("t", vec![Field::new("id", FieldType::Int), Field::new("note", FieldType::Text { positions: false }).stored_only()], Some("id")).unwrap();
    let mut rng = StdRng::seed_from_u64(9);
    // 3 KB a row, which compresses little: 20,000 rows are 60 MB of
    // images, decoded in more than one batch.
    let notes: Vec<String> = (0..40_000).map(|_| (0..1536).map(|_| format!("{:02x}", rng.gen::<u8>())).collect()).collect();
    let source = Arc::new(Counting::default());
    let dir = tempfile::tempdir().unwrap();
    let opts = DbOptions { segment_max_docs: 20_000, sync: false, doc_source: Some(source.clone()), ..DbOptions::default() };
    let db = Db::create(&dir.path().join("m.strata"), schema, opts).unwrap();
    db.load(notes.iter().enumerate().map(|(i, s)| vec![Value::Int(i as i64), Value::Str(s.clone())])).unwrap();
    assert_eq!(db.snapshot().segments().len(), 2);
    let calls = source.calls.load(Ordering::Relaxed);
    db.storage().merge(&MergePolicy { merge_factor: 2, ..MergePolicy::default() }).unwrap();
    assert_eq!(db.snapshot().segments().len(), 1);
    let (calls, most) = (source.calls.load(Ordering::Relaxed) - calls, source.most.load(Ordering::Relaxed));
    assert!(calls >= 4 && most < 20_000, "{calls} calls, at most {most} documents at once");
    // The merged segment holds every row as it was.
    for at in (0..notes.len()).step_by(5000) {
        let r = db.query(&format!("SELECT id, note FROM t WHERE id >= {at} AND id < {} ORDER BY id LIMIT 5000", at + 5000), &Params::new()).unwrap();
        assert_eq!(r.rows.len(), 5000);
        for (row, note) in r.rows.iter().zip(&notes[at..]) {
            assert_eq!(row[1].as_str(), Some(note.as_str()));
        }
    }
}
