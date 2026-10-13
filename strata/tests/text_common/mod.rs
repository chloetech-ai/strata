//! Shared helpers of the text index tests: corpora, building a segment,
//! and naive reference implementations (inverted index, boolean evaluator,
//! BM25).
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::segment::{SegmentFile, SegmentWriter};
use strata::text::codec::{decode_norm, encode_norm};
use strata::text::{analyze_with_positions, BoolQuery, PostingsFormat, TextFieldBuilder};

/// Documents over a Zipf vocabulary of `w<i>z` words, with clustered topics
/// (runs of consecutive documents sharing words, for run and bitvector
/// partitions), empty documents and a few very high term frequencies.
pub fn synthetic(seed: u64, n: usize) -> Vec<String> {
    let mut rng = StdRng::seed_from_u64(seed);
    let vocab = 3000usize;
    let word = |i: usize| format!("w{i}z");
    let mut docs = Vec::with_capacity(n);
    for d in 0..n {
        let len = match rng.gen_range(0..20) {
            0 => 0,
            1 => rng.gen_range(200..400),
            _ => rng.gen_range(1..60),
        };
        let mut words: Vec<String> = Vec::with_capacity(len + 4);
        for _ in 0..len {
            // Zipf-ish: rank ~ 1 / u.
            let u: f64 = rng.gen_range(0.0..1.0);
            let r = ((vocab as f64).powf(u) as usize).min(vocab) - 1;
            words.push(word(r));
        }
        // Topic words in runs of documents.
        if (d / 300) % 3 == 0 {
            words.push("topic".into());
            words.push(format!("sub{}", (d / 50) % 7));
        }
        if d % 2 == 0 {
            words.push("even".into());
        }
        if d % 997 == 5 {
            words.extend(std::iter::repeat_n("heavy".to_string(), 1000));
        }
        // Stopwords and punctuation are analysed away.
        if rng.gen_bool(0.3) {
            words.insert(0, "The".into());
        }
        docs.push(words.join(if rng.gen_bool(0.5) { " " } else { ", " }));
    }
    docs
}

/// SciFact bodies (title + text), if the collection is prepared.
pub fn scifact(limit: usize) -> Option<Vec<String>> {
    if !strata::data::data_dir().join("scifact/meta.json").exists() {
        eprintln!("SciFact not prepared; skipping");
        return None;
    }
    let c = strata::data::load("scifact", Some(limit)).unwrap();
    Some(c.docs.iter().map(|d| d[strata::data::col::BODY].as_str().unwrap().to_string()).collect())
}

/// A segment holding one text field `body`.
pub fn build(texts: &[String], format: PostingsFormat, loom_min_df: u32, positions: bool) -> SegmentFile {
    let mut b = TextFieldBuilder::new("body", positions, format, loom_min_df);
    for t in texts {
        b.add_text(t);
    }
    let mut w = SegmentWriter::new(texts.len());
    b.write(&mut w).unwrap();
    SegmentFile::from_vec(w.finish()).unwrap()
}

/// The configurations every test runs: BP128, and Loom with a low threshold
/// so that lists of every length are Loom lists.
pub fn configs() -> Vec<(PostingsFormat, u32)> {
    vec![(PostingsFormat::Bp128, 0), (PostingsFormat::Loom, 64), (PostingsFormat::Loom, 1)]
}

/// A posting: document and positions.
pub type Posting = (u32, Vec<u32>);

/// The naive inverted index.
pub struct Naive {
    pub n: u32,
    pub index: BTreeMap<String, Vec<Posting>>,
    /// Exact document lengths.
    pub lens: Vec<u32>,
}

impl Naive {
    pub fn new(texts: &[String]) -> Self {
        let mut index: BTreeMap<String, Vec<Posting>> = BTreeMap::new();
        let mut lens = Vec::new();
        for (d, t) in texts.iter().enumerate() {
            let toks = analyze_with_positions(t);
            lens.push(toks.len() as u32);
            let mut by_term: BTreeMap<String, Vec<u32>> = BTreeMap::new();
            for (term, p) in toks {
                by_term.entry(term).or_default().push(p);
            }
            for (term, ps) in by_term {
                index.entry(term).or_default().push((d as u32, ps));
            }
        }
        Naive { n: texts.len() as u32, index, lens }
    }

    pub fn docs(&self, t: &str) -> BTreeSet<u32> {
        self.index.get(t).map(|ps| ps.iter().map(|p| p.0).collect()).unwrap_or_default()
    }

    pub fn eval(&self, q: &BoolQuery) -> BTreeSet<u32> {
        match q {
            BoolQuery::Term(t) => self.docs(t),
            BoolQuery::Prefix(p) => self.index.iter().filter(|(t, _)| t.starts_with(p.as_str())).flat_map(|(_, ps)| ps.iter().map(|x| x.0)).collect(),
            BoolQuery::Phrase(ts) => {
                let all = (0..self.n).collect::<BTreeSet<u32>>();
                let cand = ts.iter().fold(all, |acc, t| acc.intersection(&self.docs(t)).copied().collect());
                cand.into_iter()
                    .filter(|&d| {
                        let pos: Vec<&Vec<u32>> =
                            ts.iter().map(|t| &self.index[t].iter().find(|p| p.0 == d).unwrap().1).collect();
                        pos[0].iter().any(|&p| (1..ts.len()).all(|i| pos[i].contains(&(p + i as u32))))
                    })
                    .collect()
            }
            BoolQuery::And(cs) => cs.iter().fold((0..self.n).collect(), |acc, c| acc.intersection(&self.eval(c)).copied().collect()),
            BoolQuery::Or(cs) => cs.iter().fold(BTreeSet::new(), |acc, c| acc.union(&self.eval(c)).copied().collect()),
            BoolQuery::Not(c) => {
                let s = self.eval(c);
                (0..self.n).filter(|d| !s.contains(d)).collect()
            }
        }
    }

    /// BM25 of every document containing a term (lengths quantised as the
    /// index stores them), by decreasing score then docID.
    pub fn bm25(&self, terms: &[String], n_docs: u64, total_len: u64, k1: f32, b: f32) -> Vec<(u32, f64)> {
        let avgdl = total_len as f64 / n_docs as f64;
        let mut scores: BTreeMap<u32, f64> = BTreeMap::new();
        for t in terms {
            let Some(ps) = self.index.get(t) else { continue };
            let df = ps.len() as f64;
            let idf = (1.0 + (n_docs as f64 - df + 0.5) / (df + 0.5)).ln();
            for (d, pos) in ps {
                let tf = pos.len() as f64;
                let len = decode_norm(encode_norm(self.lens[*d as usize])) as f64;
                let k = k1 as f64 * (1.0 - b as f64 + b as f64 * len / avgdl);
                *scores.entry(*d).or_default() += idf * tf * (k1 as f64 + 1.0) / (tf + k);
            }
        }
        let mut v: Vec<(u32, f64)> = scores.into_iter().collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }
}

/// Checks a top-k answer against the full ranking: same length, scores
/// within `tol` (relative) of the reference at each rank and of each
/// returned document's own reference score; ties may resolve either way.
pub fn check_topk(got: &[(u32, f32)], all: &[(u32, f64)], k: usize, what: &str) {
    let want = &all[..k.min(all.len())];
    assert_eq!(got.len(), want.len(), "{what}: result count");
    let tol = |x: f64| 1e-4 * x.abs().max(1.0);
    let exact: BTreeMap<u32, f64> = all.iter().copied().collect();
    for (i, (&(d, s), &(_, ws))) in got.iter().zip(want).enumerate() {
        assert!((s as f64 - ws).abs() <= tol(ws), "{what}: rank {i} score {s} vs {ws}");
        let own = exact.get(&d).copied().unwrap_or(f64::NAN);
        assert!((s as f64 - own).abs() <= tol(own), "{what}: doc {d} scored {s}, reference {own}");
    }
    let mut seen = BTreeSet::new();
    assert!(got.iter().all(|&(d, _)| seen.insert(d)), "{what}: duplicate documents");
}
