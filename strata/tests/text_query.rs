//! Boolean retrieval: random query trees, phrases and prefixes against a
//! naive evaluator, with and without filters; `MATCH` parsing end to end;
//! cardinality estimates.

mod text_common;

use std::collections::BTreeSet;

use rand::{rngs::StdRng, seq::SliceRandom, Rng, SeedableRng};
use strata::segment::{DocFilter, DocId, DocSet};
use strata::text::{analyze, parse_match, BoolQuery, TextFieldReader};
use text_common::*;

/// A probe-only filter (cannot list its members).
struct Every(u32);

impl DocFilter for Every {
    fn contains(&self, d: DocId) -> bool {
        d % self.0 == 1
    }
    fn cardinality(&self) -> usize {
        usize::MAX / 2
    }
}

struct Gen<'a> {
    rng: StdRng,
    terms: Vec<&'a String>,
    /// Real phrases (consecutive terms of documents).
    phrases: Vec<Vec<String>>,
}

impl Gen<'_> {
    fn leaf(&mut self) -> BoolQuery {
        match self.rng.gen_range(0..10) {
            0 => BoolQuery::Term("absentterm".into()),
            1 | 2 => {
                let p = self.phrases.choose(&mut self.rng).unwrap().clone();
                if self.rng.gen_bool(0.2) {
                    // A phrase in the wrong order mostly does not match.
                    BoolQuery::Phrase(p.into_iter().rev().collect())
                } else {
                    BoolQuery::Phrase(p)
                }
            }
            3 => BoolQuery::Prefix(["w1", "w2", "w29", "sub", "t", "ev", "q"].choose(&mut self.rng).unwrap().to_string()),
            _ => {
                // Frequent terms more often.
                let i = (self.rng.gen_range(0.0f64..1.0).powi(3) * self.terms.len() as f64) as usize;
                BoolQuery::Term(self.terms[i.min(self.terms.len() - 1)].clone())
            }
        }
    }

    fn tree(&mut self, depth: u32) -> BoolQuery {
        if depth == 0 || self.rng.gen_bool(0.3) {
            return self.leaf();
        }
        let k = self.rng.gen_range(0..=4);
        let cs: Vec<BoolQuery> = (0..k).map(|_| self.tree(depth - 1)).collect();
        match self.rng.gen_range(0..5) {
            0 | 1 => BoolQuery::And(cs),
            2 | 3 => BoolQuery::Or(cs),
            _ => BoolQuery::Not(Box::new(self.tree(depth - 1))),
        }
    }
}

fn check(texts: &[String], queries: usize, seed: u64) {
    let naive = Naive::new(texts);
    // Terms by decreasing df.
    let mut terms: Vec<&String> = naive.index.keys().collect();
    terms.sort_by_key(|t| std::cmp::Reverse(naive.index[*t].len()));
    let mut rng = StdRng::seed_from_u64(seed);
    let mut phrases = Vec::new();
    for _ in 0..200 {
        let toks = analyze(&texts[rng.gen_range(0..texts.len())]);
        if toks.len() >= 3 {
            let len = rng.gen_range(2..=3);
            let at = rng.gen_range(0..=toks.len() - len);
            phrases.push(toks[at..at + len].to_vec());
        }
    }
    let mut g = Gen { rng: StdRng::seed_from_u64(seed + 1), terms, phrases };
    let n = texts.len() as u32;
    let filters: Vec<DocSet> = [0.001, 0.02, 0.3, 0.9]
        .iter()
        .map(|&p| DocSet::from_sorted((0..n).filter(|_| rng.gen_bool(p)).collect(), n))
        .collect();
    let qs: Vec<BoolQuery> = (0..queries).map(|_| g.tree(3)).collect();
    for (format, min_df) in configs() {
        let seg = build(texts, format, min_df, true);
        let r = TextFieldReader::open(&seg, "body").unwrap();
        for q in &qs {
            let want = naive.eval(q);
            let got: BTreeSet<u32> = r.eval(q, None).iter().collect();
            assert!(got == want, "{format:?}/{min_df}: {q:?}: {} docs, want {}", got.len(), want.len());
            for f in &filters {
                let got: Vec<u32> = r.eval(q, Some(f)).iter().collect();
                let w: Vec<u32> = want.iter().copied().filter(|&d| f.contains(d)).collect();
                assert_eq!(got, w, "{format:?}/{min_df}: {q:?} with a filter of {}", f.len());
            }
            let every = Every(3);
            let got: Vec<u32> = r.eval(q, Some(&every)).iter().collect();
            assert!(got.iter().copied().eq(want.iter().copied().filter(|&d| every.contains(d))));
            let est = r.estimate(q);
            assert!((0.0..=n as f64 + 1e-6).contains(&est), "{q:?}: estimate {est}");
            if let BoolQuery::Term(_) = q {
                assert_eq!(est, want.len() as f64, "single-term estimates are exact");
            }
        }
    }
}

#[test]
fn random_queries_synthetic() {
    check(&synthetic(21, 4000), 400, 1);
}

#[test]
fn random_queries_scifact() {
    let Some(texts) = scifact(1000) else { return };
    check(&texts, 200, 2);
}

#[test]
fn match_queries_end_to_end() {
    let texts: Vec<String> = [
        "Vaccines against COVID-19 use messenger RNA.",
        "An mRNA vaccine was tested in animals.",
        "Animal models of influenza vaccination.",
        "Messenger RNA is translated by ribosomes; RNA messenger services differ.",
        "Café owners vaccinate their cats.",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for (format, min_df) in configs() {
        let seg = build(&texts, format, min_df, true);
        let r = TextFieldReader::open(&seg, "body").unwrap();
        let run = |q: &str| -> Vec<u32> { r.eval(&parse_match(q).unwrap(), None).iter().collect() };
        // "vaccination" and "vaccinate" stem to "vaccin" too.
        assert_eq!(run("vaccine"), [0, 1, 2, 4]);
        assert_eq!(run("vaccin*"), [0, 1, 2, 4]);
        assert_eq!(run("\"messenger rna\""), [0, 3]);
        assert_eq!(run("\"rna messenger\""), [3]);
        assert_eq!(run("vaccine AND (mrna OR \"messenger rna\") NOT animal"), [0]);
        assert_eq!(run("vaccine -animal"), [0, 4]);
        assert_eq!(run("covid-19"), [0]);
        assert_eq!(run("CAFE"), [4]);
        assert_eq!(run("the"), Vec::<u32>::new());
        assert_eq!(run("NOT vaccine"), [3]);
        assert_eq!(run("influenza OR ribosomes OR absent"), [2, 3]);
    }
    // Without positions a phrase is the conjunction of its terms.
    let seg = build(&texts, strata::text::PostingsFormat::Loom, 1, false);
    let r = TextFieldReader::open(&seg, "body").unwrap();
    assert_eq!(r.eval(&parse_match("\"rna messenger\"").unwrap(), None).to_sorted(), [0, 3]);
    assert!(!r.postings("rna").unwrap().has_positions());
}
