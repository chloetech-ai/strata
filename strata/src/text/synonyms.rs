//! Synonyms for full-text searches, in Solr's synonym file format:
//!
//! ```text
//! # equivalent words: each one also finds the others
//! laptop, notebook, portable computer
//! # one-way: a search for tv also finds television, not the reverse
//! tv => television
//! ```
//!
//! Both sides are analysed like documents (`laptops` finds `notebook`).
//! `MATCH(.., synonyms => 1)` and `bm25(.., synonyms => 1)` use the list a
//! database's [`crate::query::exec::ExecOptions::synonyms`] holds; strata-server
//! loads it from `--synonyms FILE` and edits it at `/v1/synonyms`.

use std::collections::HashMap;

use super::analyzer::analyze;
use crate::error::{Error, Result};

#[derive(Clone, Debug, Default)]
pub struct Synonyms {
    /// From an analysed word or phrase to the phrases it also finds.
    map: HashMap<Vec<String>, Vec<Vec<String>>>,
    /// The longest key, in terms.
    max_len: usize,
    /// The text it was parsed from.
    text: String,
    rules: usize,
}

impl Synonyms {
    pub fn parse(text: &str) -> Result<Synonyms> {
        let mut s = Synonyms { text: text.to_string(), ..Synonyms::default() };
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let side = |t: &str| -> Vec<Vec<String>> { t.split(',').map(analyze).filter(|p| !p.is_empty()).collect() };
            let (from, to) = match line.split_once("=>") {
                Some((a, b)) => (side(a), side(b)),
                None => {
                    let all = side(line);
                    (all.clone(), all)
                }
            };
            if from.is_empty() || to.is_empty() || (from.len() == 1 && to.len() == 1 && from == to) {
                return Err(Error::Query(format!("synonyms line {}: {line:?} needs two or more words or phrases", n + 1)));
            }
            for f in &from {
                let e = s.map.entry(f.clone()).or_default();
                for t in &to {
                    if t != f && !e.contains(t) {
                        e.push(t.clone());
                    }
                }
                s.max_len = s.max_len.max(f.len());
            }
            s.rules += 1;
        }
        Ok(s)
    }

    /// What `terms` (an analysed word or phrase) also finds.
    pub fn get(&self, terms: &[String]) -> &[Vec<String>] {
        self.map.get(terms).map_or(&[], |v| v.as_slice())
    }

    /// The longest phrase with synonyms, in terms.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Lines with a rule.
    pub fn rules(&self) -> usize {
        self.rules
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_kinds_of_rule() {
        let s = Synonyms::parse("# comment\nlaptop, notebook, portable computer\ntv => television\n\n").unwrap();
        assert_eq!(s.rules(), 2);
        let laptop = vec!["laptop".to_string()];
        assert_eq!(s.get(&laptop), &[vec!["notebook".to_string()], vec!["portabl".to_string(), "comput".to_string()]]);
        assert_eq!(s.get(&["notebook".to_string()]).len(), 2);
        assert_eq!(s.get(&["tv".to_string()]), &[vec!["televis".to_string()]]);
        assert!(s.get(&["televis".to_string()]).is_empty());
        assert_eq!(s.max_len(), 2);
        assert!(Synonyms::parse("lonely").is_err());
    }
}
