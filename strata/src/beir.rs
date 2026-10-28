//! BEIR collections in their published layout (`corpus.jsonl`,
//! `queries.jsonl`, `qrels/<split>.tsv`), read as downloaded: for text-only
//! evaluations (`strata-bm25-eval`, the relevance gate in
//! `tests/relevance_gate.rs`), which need none of the embeddings and
//! synthetic fields of [`crate::data`]'s prepared collections.
//!
//! Splits: every collection has `test`; some have `dev` (NFCorpus, FiQA,
//! Quora) or `train` (SciFact). Choose settings on those and report `test`,
//! so that the reported numbers were not tuned on the queries they measure.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value as Json;

use crate::error::{corrupt, Result};

pub struct RawCollection {
    /// `(id, title and text)` in file order; the title is its own line.
    pub docs: Vec<(String, String)>,
    /// `(id, text)` of the queries judged in the split, in file order.
    pub queries: Vec<(String, String)>,
    /// Grades by query id, then document id.
    pub qrels: HashMap<String, HashMap<String, u32>>,
}

fn lines(path: &Path) -> Result<impl Iterator<Item = std::io::Result<String>>> {
    let f = File::open(path).map_err(|e| corrupt(format!("{}: {e}", path.display())))?;
    Ok(BufReader::new(f).lines())
}

fn str_of<'a>(j: &'a Json, key: &str) -> &'a str {
    j.get(key).and_then(Json::as_str).unwrap_or("")
}

fn json(path: &Path, line: &str) -> Result<Json> {
    serde_json::from_str(line).map_err(|e| corrupt(format!("{}: {e}", path.display())))
}

/// The collection in `dir` with the queries and judgements of `split`.
pub fn load(dir: &Path, split: &str) -> Result<RawCollection> {
    let mut docs = Vec::new();
    let path = dir.join("corpus.jsonl");
    for line in lines(&path)? {
        let j = json(&path, &line?)?;
        let (title, text) = (str_of(&j, "title"), str_of(&j, "text"));
        let body = if title.is_empty() { text.to_string() } else { format!("{title}\n{text}") };
        docs.push((str_of(&j, "_id").to_string(), body));
    }
    let mut qrels: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for line in lines(&dir.join("qrels").join(format!("{split}.tsv")))?.skip(1) {
        let line = line?;
        let mut cols = line.split('\t');
        let (Some(q), Some(d), Some(g)) = (cols.next(), cols.next(), cols.next()) else { continue };
        qrels.entry(q.to_string()).or_default().insert(d.to_string(), g.trim().parse().unwrap_or(0));
    }
    let mut queries = Vec::new();
    let path = dir.join("queries.jsonl");
    for line in lines(&path)? {
        let j = json(&path, &line?)?;
        let id = str_of(&j, "_id");
        if qrels.contains_key(id) {
            queries.push((id.to_string(), str_of(&j, "text").to_string()));
        }
    }
    Ok(RawCollection { docs, queries, qrels })
}
