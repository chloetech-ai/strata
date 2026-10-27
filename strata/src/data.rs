//! BEIR collections prepared by `scripts/prepare_beir.py`.
//!
//! Every collection is loaded into the same table:
//!
//! | field | type | role |
//! |---|---|---|
//! | `id` | KEYWORD, primary key | BEIR document id |
//! | `title`, `text` | TEXT, stored only | returned by `SELECT` |
//! | `body` | TEXT, indexed, positions, generated from title + text | searched |
//! | `year` | INT | synthetic, uniform 1990..=2024 |
//! | `category` | KEYWORD | synthetic, Zipf(1) over 64 values `c00`..`c63` |
//! | `rating` | FLOAT | synthetic, uniform [0, 5) |
//! | `topic` | KEYWORD | k-means cluster of the embedding, `t00`..`t31` |
//! | `emb` | VECTOR(1024) | Cohere embed-english-v3, unit length |

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::error::{corrupt, Result};
use crate::schema::{Field, FieldType, Schema, VectorIndexKind};
use crate::value::{Document, Value};

/// Directory holding the prepared collections: `$STRATA_DATA`, else
/// `/home/user/data/beir`.
pub fn data_dir() -> PathBuf {
    std::env::var_os("STRATA_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/home/user/data/beir"))
}

/// Field order of the BEIR table (indices into `Document`).
pub mod col {
    pub const ID: usize = 0;
    pub const TITLE: usize = 1;
    pub const TEXT: usize = 2;
    pub const BODY: usize = 3;
    pub const YEAR: usize = 4;
    pub const CATEGORY: usize = 5;
    pub const RATING: usize = 6;
    pub const TOPIC: usize = 7;
    pub const EMB: usize = 8;
}

pub fn beir_schema(dim: usize, index: VectorIndexKind) -> Schema {
    Schema::new(
        "docs",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: false }).stored_only(),
            Field::new("text", FieldType::Text { positions: false }).stored_only(),
            Field::new("body", FieldType::Text { positions: true }).not_stored().generated(&["title", "text"]),
            Field::new("year", FieldType::Int),
            Field::new("category", FieldType::Keyword),
            Field::new("rating", FieldType::Float),
            Field::new("topic", FieldType::Keyword),
            Field::new("emb", FieldType::Vector { dim, index }).not_stored(),
        ],
        Some("id"),
    )
    .expect("valid schema")
}

#[derive(Clone, Debug)]
pub struct Query {
    pub id: String,
    pub text: String,
    pub vector: Arc<[f32]>,
}

/// query id -> document id -> relevance grade.
pub type Qrels = HashMap<String, HashMap<String, u32>>;

pub struct Collection {
    pub name: String,
    pub dim: usize,
    pub docs: Vec<Document>,
    pub queries: Vec<Query>,
    pub qrels: Qrels,
}

#[derive(Deserialize)]
struct RawDoc {
    id: String,
    title: String,
    text: String,
    year: i64,
    category: String,
    rating: f64,
    topic: String,
}

#[derive(Deserialize)]
struct RawQuery {
    id: String,
    text: String,
}

#[derive(Deserialize)]
struct Meta {
    docs: usize,
    queries: usize,
    dim: usize,
}

/// Reads the first `n` vectors, one row at a time (the whole file can be
/// larger than what a prefix needs, and Quora's is 2 GB).
fn read_vectors(path: &Path, n: usize, dim: usize) -> Result<Vec<Arc<[f32]>>> {
    let mut f = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut row = vec![0u8; dim * 4];
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        f.read_exact(&mut row).map_err(|_| corrupt(format!("{} holds fewer than {n} vectors", path.display())))?;
        out.push(row.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect::<Vec<f32>>().into());
    }
    Ok(out)
}

/// Loads collection `name` from [`data_dir`], keeping the first `limit`
/// documents if given (queries and judgments are kept whole).
pub fn load(name: &str, limit: Option<usize>) -> Result<Collection> {
    let dir = data_dir().join(name);
    let meta: Meta = serde_json::from_reader(File::open(dir.join("meta.json"))?).map_err(corrupt)?;
    let n = limit.map_or(meta.docs, |l| l.min(meta.docs));
    let vecs = read_vectors(&dir.join("doc_vecs.f32"), n, meta.dim)?;
    let mut docs = Vec::with_capacity(n);
    let reader = BufReader::new(File::open(dir.join("docs.jsonl"))?);
    for (line, v) in reader.lines().zip(vecs) {
        let r: RawDoc = serde_json::from_str(&line?).map_err(corrupt)?;
        let body = if r.title.is_empty() { r.text.clone() } else { format!("{}\n{}", r.title, r.text) };
        docs.push(vec![
            Value::Str(r.id),
            Value::Str(r.title),
            Value::Str(r.text),
            Value::Str(body),
            Value::Int(r.year),
            Value::Str(r.category),
            Value::Float(r.rating),
            Value::Str(r.topic),
            Value::Vector(v),
        ]);
    }
    let qvecs = read_vectors(&dir.join("query_vecs.f32"), meta.queries, meta.dim)?;
    let mut queries = Vec::with_capacity(meta.queries);
    let reader = BufReader::new(File::open(dir.join("queries.jsonl"))?);
    for (line, v) in reader.lines().zip(qvecs) {
        let r: RawQuery = serde_json::from_str(&line?).map_err(corrupt)?;
        queries.push(Query { id: r.id, text: r.text, vector: v });
    }
    let mut qrels: Qrels = HashMap::new();
    let reader = BufReader::new(File::open(dir.join("qrels.tsv"))?);
    for line in reader.lines() {
        let line = line?;
        let mut it = line.split('\t');
        let (q, d, g) = (it.next(), it.next(), it.next());
        if let (Some(q), Some(d), Some(g)) = (q, d, g) {
            qrels.entry(q.to_string()).or_default().insert(d.to_string(), g.trim().parse().unwrap_or(0));
        }
    }
    Ok(Collection { name: name.to_string(), dim: meta.dim, docs, queries, qrels })
}

/// A synthetic collection in the BEIR table's layout, for tests, demos and
/// smoke benchmarks where no prepared collection is at hand: `n` documents
/// whose words follow Zipf's law over a 5,000-word vocabulary, mixed with
/// words of one of 32 topics, and unit vectors of `dim` dimensions near
/// their topic's centre. The same seed gives the same collection. The
/// `body` field is left `Null` for [`crate::db::Db::load`] to generate.
pub fn synthetic(n: usize, dim: usize, seed: u64) -> Vec<Document> {
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use rand_distr::{Distribution, Normal, Zipf};

    const TOPICS: usize = 32;
    let mut rng = StdRng::seed_from_u64(seed);
    let syllables = ["ka", "lo", "mi", "ne", "ru", "ta", "vo", "si", "pe", "da", "go", "fu", "zi", "ba", "che", "wa"];
    let vocab: Vec<String> = (0..5000)
        .map(|i| {
            let mut w = String::new();
            let mut x = i + 17;
            while x > 0 || w.len() < 4 {
                w.push_str(syllables[x % syllables.len()]);
                x /= syllables.len();
            }
            w
        })
        .collect();
    let zipf_words = Zipf::new(vocab.len() as u64, 1.07).expect("valid zipf");
    let zipf_cat = Zipf::new(64, 1.0).expect("valid zipf");
    let normal = Normal::new(0.0f32, 1.0).expect("valid normal");
    let centres: Vec<Vec<f32>> = (0..TOPICS).map(|_| unit((0..dim).map(|_| normal.sample(&mut rng)).collect())).collect();
    let topic_words: Vec<Vec<&str>> = (0..TOPICS).map(|t| (0..40).map(|j| vocab[(t * 131 + j * 37 + 500) % vocab.len()].as_str()).collect()).collect();
    let mut docs = Vec::with_capacity(n);
    for i in 0..n {
        let topic = rng.gen_range(0..TOPICS);
        let words = |len: usize, rng: &mut StdRng| -> String {
            (0..len)
                .map(|_| {
                    if rng.gen_bool(0.25) {
                        topic_words[topic][rng.gen_range(0..topic_words[topic].len())]
                    } else {
                        vocab[zipf_words.sample(rng) as usize - 1].as_str()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        let title = words(rng.gen_range(3..9), &mut rng);
        let text = words(rng.gen_range(20..120), &mut rng);
        let emb: Vec<f32> = centres[topic].iter().map(|c| c + 0.6 * normal.sample(&mut rng) / (dim as f32).sqrt()).collect();
        docs.push(vec![
            Value::Str(format!("d{i:07}")),
            Value::Str(title),
            Value::Str(text),
            Value::Null,
            Value::Int(rng.gen_range(1990..=2024)),
            Value::Str(format!("c{:02}", zipf_cat.sample(&mut rng) as u64 - 1)),
            Value::Float((rng.gen_range(0.0..5.0f64) * 100.0).round() / 100.0),
            Value::Str(format!("t{topic:02}")),
            Value::Vector(Arc::from(unit(emb))),
        ]);
    }
    docs
}

fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    v.iter_mut().for_each(|x| *x /= n);
    v
}
