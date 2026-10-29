//! Synonyms for full-text searches, in Solr's synonym file format:
//!
//! ```text
//! # equivalent words: each one also finds the others
//! laptop, notebook, portable computer
//! # one-way: a search for tv also finds television, not the reverse
//! tv => television
//! ```
//!
//! Both sides are analysed like documents (`laptops` finds `notebook`), by
//! the analyzer of the field searched ([`Synonyms::of`]).
//! `MATCH(.., synonyms => 1)` and `bm25(.., synonyms => 1)` use the list a
//! database's [`crate::query::exec::ExecOptions::synonyms`] holds; strata-server
//! loads it from `--synonyms FILE` and edits it at `/v1/synonyms`.

use std::collections::HashMap;

use super::analyzer::Analyzer;
use crate::error::{Error, Result};

#[derive(Clone, Debug, Default)]
pub struct Synonyms {
    /// The rules as each analyzer analyses them, in [`Analyzer::ALL`]'s
    /// order.
    maps: Vec<SynonymMap>,
    /// The text it was parsed from.
    text: String,
    rules: usize,
}

/// The synonyms of one analyzer's terms.
#[derive(Clone, Debug, Default)]
pub struct SynonymMap {
    /// From an analysed word or phrase to the phrases it also finds.
    map: HashMap<Vec<String>, Vec<Vec<String>>>,
    /// The longest key, in terms.
    max_len: usize,
}

impl Synonyms {
    pub fn parse(text: &str) -> Result<Synonyms> {
        let mut s = Synonyms { text: text.to_string(), maps: vec![SynonymMap::default(); Analyzer::ALL.len()], ..Synonyms::default() };
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            // A rule that finds nothing new under every analyzer is a mistake.
            let mut useful = false;
            for (a, m) in Analyzer::ALL.into_iter().zip(&mut s.maps) {
                let side = |t: &str| -> Vec<Vec<String>> { t.split(',').map(|p| a.analyze(p)).filter(|p| !p.is_empty()).collect() };
                let (from, to) = match line.split_once("=>") {
                    Some((x, y)) => (side(x), side(y)),
                    None => {
                        let all = side(line);
                        (all.clone(), all)
                    }
                };
                if from.is_empty() || to.is_empty() || (from.len() == 1 && to.len() == 1 && from == to) {
                    continue;
                }
                useful = true;
                for f in &from {
                    let e = m.map.entry(f.clone()).or_default();
                    for t in &to {
                        if t != f && !e.contains(t) {
                            e.push(t.clone());
                        }
                    }
                    m.max_len = m.max_len.max(f.len());
                }
            }
            if !useful {
                return Err(Error::Query(format!("synonyms line {}: {line:?} needs two or more words or phrases", n + 1)));
            }
            s.rules += 1;
        }
        Ok(s)
    }

    /// The synonyms of a field that `analyzer` analyses.
    pub fn of(&self, analyzer: Analyzer) -> &SynonymMap {
        static EMPTY: std::sync::OnceLock<SynonymMap> = std::sync::OnceLock::new();
        let i = Analyzer::ALL.iter().position(|&a| a == analyzer).unwrap_or(0);
        self.maps.get(i).unwrap_or_else(|| EMPTY.get_or_init(SynonymMap::default))
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Lines with a rule.
    pub fn rules(&self) -> usize {
        self.rules
    }

    pub fn is_empty(&self) -> bool {
        self.rules == 0
    }
}

impl SynonymMap {
    /// What `terms` (an analysed word or phrase) also finds.
    pub fn get(&self, terms: &[String]) -> &[Vec<String>] {
        self.map.get(terms).map_or(&[], |v| v.as_slice())
    }

    /// The longest phrase with synonyms, in terms.
    pub fn max_len(&self) -> usize {
        self.max_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_kinds_of_rule() {
        let all = Synonyms::parse("# comment\nlaptop, notebook, portable computer\ntv => television\n\n").unwrap();
        assert_eq!(all.rules(), 2);
        let s = all.of(Analyzer::English);
        let laptop = vec!["laptop".to_string()];
        assert_eq!(s.get(&laptop), &[vec!["notebook".to_string()], vec!["portabl".to_string(), "comput".to_string()]]);
        assert_eq!(s.get(&["notebook".to_string()]).len(), 2);
        assert_eq!(s.get(&["tv".to_string()]), &[vec!["televis".to_string()]]);
        assert!(s.get(&["televis".to_string()]).is_empty());
        assert_eq!(s.max_len(), 2);
        assert!(Synonyms::parse("lonely").is_err());

        // Each analyzer analyses the rules its own way: `simple` keeps
        // words whole, so plurals that `english` makes one stay two.
        let s = all.of(Analyzer::Simple);
        assert_eq!(s.get(&["laptop".to_string()]), &[vec!["notebook".to_string()], vec!["portable".to_string(), "computer".to_string()]]);
        assert_eq!(s.get(&["tv".to_string()]), &[vec!["television".to_string()]]);
        let plurals = Synonyms::parse("laptops, laptop").unwrap();
        assert!(plurals.of(Analyzer::English).get(&["laptop".to_string()]).is_empty());
        assert_eq!(plurals.of(Analyzer::Simple).get(&["laptops".to_string()]), &[vec!["laptop".to_string()]]);
        // Stopwords to english, words to simple.
        assert!(Synonyms::parse("the, a").is_ok() && Synonyms::parse("!!, ??").is_err());
    }
}
