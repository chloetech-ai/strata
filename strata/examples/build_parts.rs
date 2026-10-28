//! Time one segment build, and each of its parts, over a BEIR corpus: the
//! documents' title and text as an indexed TEXT field with positions, plus
//! keyword, number and (with `DIM` > 0) random vector fields.
//!
//! ```text
//! build_parts CORPUS.jsonl DOCS DIM [KEY=VALUE ...]
//! ```
//!
//! `KEY=VALUE` pairs are build options, such as `build.threads=1` or
//! `vector.threads=4`. Part times are summed over the threads that ran them.
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::io::BufRead;
use std::time::Instant;
use strata::schema::DEFAULT_HNSW;
use strata::segment::build::{build_segment, take_build_times, BuildOptions};
use strata::{Field, FieldType, Schema, Value};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let n: usize = a[2].parse().unwrap();
    let dim: usize = a[3].parse().unwrap();
    let mut opts = BuildOptions::default();
    for kv in &a[4..] {
        let (k, v) = kv.split_once('=').expect("KEY=VALUE");
        opts = opts.set(k, v);
    }
    let f = std::io::BufReader::new(std::fs::File::open(&a[1]).unwrap());
    let texts: Vec<String> = f
        .lines()
        .take(n)
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(&l.unwrap()).unwrap();
            format!("{} {}", v["title"].as_str().unwrap_or(""), v["text"].as_str().unwrap_or(""))
        })
        .collect();
    let mut fields = vec![
        Field::new("id", FieldType::Keyword),
        Field::new("kind", FieldType::Keyword),
        Field::new("turn", FieldType::Int),
        Field::new("ts", FieldType::Int),
        Field::new("importance", FieldType::Float),
        Field::new("body", FieldType::Text { positions: true }),
    ];
    if dim > 0 {
        fields.push(Field::new("emb", FieldType::Vector { dim, index: DEFAULT_HNSW }));
    }
    let schema = Schema::new("t", fields, Some("id")).unwrap();
    let mut rng = StdRng::seed_from_u64(1);
    let kinds = ["note", "rule", "decision", "turn", "summary"];
    let docs: Vec<Vec<Value>> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut d = vec![
                Value::Str(format!("k{i:09}")),
                Value::Str(kinds[i % 5].into()),
                Value::Int(i as i64 / 7),
                Value::Int(1_700_000_000 + i as i64),
                Value::Float(rng.gen()),
                Value::Str(t.clone()),
            ];
            if dim > 0 {
                d.push(Value::Vector((0..dim).map(|_| rng.gen::<f32>() - 0.5).collect::<Vec<f32>>().into()));
            }
            d
        })
        .collect();
    let bytes: usize = texts.iter().map(|t| t.len()).sum();
    take_build_times();
    let t = Instant::now();
    let img = build_segment(&schema, &docs, &opts).unwrap();
    let wall = t.elapsed().as_secs_f64();
    println!("{} docs, {:.1} MB text, image {:.1} MB, wall {:.2} s", docs.len(), bytes as f64 / 1e6, img.len() as f64 / 1e6, wall);
    for (k, v) in take_build_times() {
        println!("  {k}: {v:.2} s");
    }
}
