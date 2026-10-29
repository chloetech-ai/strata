//! Segment builds on several threads (`build.threads`): fields build at
//! once and TEXT fields are analysed in runs of documents joined in order,
//! so the image is the same bytes as a build on one thread.

use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::graph::{edges_value, Edge};
use strata::schema::DEFAULT_HNSW;
use strata::segment::build::{build_segment, BuildOptions};
use strata::segment::SegmentFile;
use strata::text::sparse::to_value;
use strata::{Field, FieldType, Schema, Value, VectorIndexKind};

const WORDS: [&str; 12] = ["merge", "merged", "merging", "segment", "the", "of", "graph", "vectors", "running", "ran", "build", "builds"];

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("n", FieldType::Int),
            Field::new("x", FieldType::Float),
            Field::new("title", FieldType::Text { positions: false }),
            Field::new("body", FieldType::Text { positions: true }),
            Field::new("note", FieldType::Text { positions: true }).stored_only(),
            Field::new("emb", FieldType::Vector { dim: 16, index: DEFAULT_HNSW }),
            Field::new("flat", FieldType::Vector { dim: 8, index: VectorIndexKind::Flat }),
            Field::new("splade", FieldType::Sparse),
            Field::new("links", FieldType::Edges),
        ],
        Some("id"),
    )
    .unwrap()
}

/// Documents whose words, some first seen late, repeat across the runs a
/// parallel build analyses apart; some fields are missing.
fn docs(n: usize) -> Vec<Vec<Value>> {
    let mut rng = StdRng::seed_from_u64(7);
    (0..n)
        .map(|i| {
            let mut words = |k: usize| (0..k).map(|_| if rng.gen_bool(0.1) { format!("w{}", rng.gen_range(0..i + 1)) } else { WORDS[rng.gen_range(0..WORDS.len())].to_string() }).collect::<Vec<_>>().join(" ");
            let title = words(4);
            let body = words(30);
            let note = words(3);
            let tags: Vec<Value> = (0..i % 3).map(|k| Value::Str(format!("t{}", (i + k) % 7))).collect();
            let splade: Vec<(String, f32)> = (0..i % 5).map(|k| (format!("s{}", (i * 3 + k) % 50), 0.5 + k as f32)).collect();
            let links = edges_value(&(0..i % 3).map(|k| Edge::labeled("cites", format!("d{}", (i * 7 + k) % n))).collect::<Vec<_>>());
            let emb: Vec<f32> = (0..16).map(|_| rng.gen::<f32>() - 0.5).collect();
            let flat: Vec<f32> = (0..8).map(|_| rng.gen::<f32>()).collect();
            vec![
                Value::Str(format!("d{i}")),
                Value::List(tags),
                if i % 11 == 0 { Value::Null } else { Value::Int(i as i64 % 100) },
                Value::Float(i as f64 / 3.0),
                if i % 13 == 0 { Value::Null } else { Value::Str(title) },
                Value::Str(body),
                Value::Str(note),
                if i % 10 != 0 { Value::Null } else { Value::Vector(emb.into()) },
                Value::Vector(flat.into()),
                to_value(&splade),
                links,
            ]
        })
        .collect()
}

fn build(docs: &[Vec<Value>], threads: usize, opts: &BuildOptions) -> Vec<u8> {
    let opts = opts.clone().set("build.threads", threads).set("vector.threads", 1);
    build_segment(&schema(), docs, &opts).unwrap()
}

#[test]
fn a_build_on_several_threads_writes_the_same_bytes_as_one_on_one() {
    // 20,000 documents: four runs of 5,000 per TEXT field on four threads,
    // and posting lists written in ranges of terms. In BP128 a range may
    // start at any list, in Loom only at a Loom list.
    let docs = docs(20_000);
    let formats = [BuildOptions::default(), BuildOptions::default().set("text.postings", "bp128"), BuildOptions::default().set("text.loom_min_df", 2)];
    for opts in &formats {
        let one = build(&docs, 1, opts);
        SegmentFile::from_vec(one.clone()).unwrap();
        for threads in [2, 3, 4, 8] {
            assert!(build(&docs, threads, opts) == one, "{threads} threads built different bytes with {opts:?}");
        }
    }
    // Too few documents to split, and none at all.
    for n in [1500, 1, 0] {
        assert!(build(&docs[..n], 4, &formats[0]) == build(&docs[..n], 1, &formats[0]), "{n} documents");
    }
}

#[test]
fn a_bad_thread_count_is_refused() {
    let docs = docs(10);
    for bad in ["0", "-1", "many"] {
        let opts = BuildOptions::default().set("build.threads", bad);
        assert!(build_segment(&schema(), &docs, &opts).is_err(), "build.threads = {bad}");
    }
}
