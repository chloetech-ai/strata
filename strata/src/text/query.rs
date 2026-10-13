//! Boolean full-text queries and the `MATCH` grammar.
//!
//! ```text
//! query   := and ("OR" and)*
//! and     := unary (["AND"] unary)*        juxtaposition is AND
//! unary   := ("NOT" | "-") unary | primary  "a NOT b" is "a AND NOT b"
//! primary := "(" query ")" | "\"phrase\"" | word | word*
//! ```
//!
//! Operators are uppercase (`AND`, `OR`, `NOT`), as in SQLite FTS5;
//! lowercase `and`/`or`/`not` are words (and stopwords). `-` negates when it
//! starts a token. Words and phrases are analysed like documents: a word
//! that analyses to several terms (`e-mail`) is a phrase, one that analyses
//! to none (a stopword) is dropped. A prefix (`vacc*`) is lowercased and
//! folded but not stemmed, and matches the indexed (stemmed) terms that
//! start with it.

use super::analyzer::{analyze, for_each_word};
use crate::error::{Error, Result};

/// A boolean query over analysed terms.
#[derive(Clone, Debug, PartialEq)]
pub enum BoolQuery {
    Term(String),
    /// Consecutive terms.
    Phrase(Vec<String>),
    /// Every indexed term with this prefix.
    Prefix(String),
    /// All of (an empty `And` matches every document).
    And(Vec<BoolQuery>),
    /// Any of (an empty `Or` matches nothing).
    Or(Vec<BoolQuery>),
    Not(Box<BoolQuery>),
}

impl BoolQuery {
    /// The terms the query needs postings for (prefixes and their
    /// expansions excluded), with repetitions.
    pub fn terms(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_terms(&mut out);
        out
    }

    fn collect_terms<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            BoolQuery::Term(t) => out.push(t),
            BoolQuery::Phrase(ts) => out.extend(ts.iter().map(|t| t.as_str())),
            BoolQuery::Prefix(_) => {}
            BoolQuery::And(cs) | BoolQuery::Or(cs) => cs.iter().for_each(|c| c.collect_terms(out)),
            BoolQuery::Not(c) => c.collect_terms(out),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Phrase(String),
    Word(String),
}

fn lex(q: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let mut it = q.char_indices().peekable();
    while let Some(&(i, c)) = it.peek() {
        match c {
            c if c.is_whitespace() => {
                it.next();
            }
            '(' => {
                it.next();
                out.push(Tok::LParen);
            }
            ')' => {
                it.next();
                out.push(Tok::RParen);
            }
            '"' => {
                it.next();
                let start = i + 1;
                let mut end = None;
                for (j, c) in it.by_ref() {
                    if c == '"' {
                        end = Some(j);
                        break;
                    }
                }
                let end = end.ok_or_else(|| Error::Query(format!("unterminated phrase in MATCH query {q:?}")))?;
                out.push(Tok::Phrase(q[start..end].to_string()));
            }
            '-' => {
                it.next();
                out.push(Tok::Not);
            }
            _ => {
                let start = i;
                let mut end = q.len();
                while let Some(&(j, c)) = it.peek() {
                    if c.is_whitespace() || matches!(c, '(' | ')' | '"') {
                        end = j;
                        break;
                    }
                    it.next();
                }
                out.push(match &q[start..end] {
                    "AND" => Tok::And,
                    "OR" => Tok::Or,
                    "NOT" => Tok::Not,
                    w => Tok::Word(w.to_string()),
                });
            }
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

/// Parses a `MATCH` query (see the module documentation). A query whose
/// words are all stopwords matches nothing (`Or([])`).
pub fn parse_match(q: &str) -> Result<BoolQuery> {
    let mut p = Parser { toks: lex(q)?, pos: 0 };
    let e = p.or()?;
    if p.pos < p.toks.len() {
        return Err(Error::Query(format!("unexpected {:?} in MATCH query {q:?}", p.toks[p.pos])));
    }
    Ok(e.unwrap_or(BoolQuery::Or(Vec::new())))
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    /// `None` when every operand analysed to nothing.
    fn or(&mut self) -> Result<Option<BoolQuery>> {
        let mut parts = vec![self.and()?];
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            parts.push(self.and()?);
        }
        Ok(combine(parts, BoolQuery::Or))
    }

    fn and(&mut self) -> Result<Option<BoolQuery>> {
        let mut parts = vec![self.unary()?];
        loop {
            match self.peek() {
                None | Some(Tok::Or) | Some(Tok::RParen) => break,
                Some(Tok::And) => self.pos += 1,
                _ => {}
            }
            parts.push(self.unary()?);
        }
        Ok(combine(parts, BoolQuery::And))
    }

    fn unary(&mut self) -> Result<Option<BoolQuery>> {
        if self.peek() == Some(&Tok::Not) {
            self.pos += 1;
            return Ok(self.unary()?.map(|q| BoolQuery::Not(Box::new(q))));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Option<BoolQuery>> {
        let tok = self.toks.get(self.pos).cloned().ok_or_else(|| Error::Query("MATCH query ends where a term was expected".into()))?;
        self.pos += 1;
        Ok(match tok {
            Tok::LParen => {
                if self.peek() == Some(&Tok::RParen) {
                    self.pos += 1;
                    return Ok(None);
                }
                let e = self.or()?;
                if self.peek() != Some(&Tok::RParen) {
                    return Err(Error::Query("unbalanced parentheses in MATCH query".into()));
                }
                self.pos += 1;
                e
            }
            Tok::Phrase(s) => terms_query(analyze(&s)),
            Tok::Word(w) => match w.strip_suffix('*') {
                Some(stem) => prefix_query(stem),
                None => terms_query(analyze(&w)),
            },
            t => return Err(Error::Query(format!("unexpected {t:?} in MATCH query"))),
        })
    }
}

/// Joins the operands that analysed to something.
fn combine(parts: Vec<Option<BoolQuery>>, f: fn(Vec<BoolQuery>) -> BoolQuery) -> Option<BoolQuery> {
    let mut parts: Vec<BoolQuery> = parts.into_iter().flatten().collect();
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(f(parts)),
    }
}

fn terms_query(mut terms: Vec<String>) -> Option<BoolQuery> {
    match terms.len() {
        0 => None,
        1 => terms.pop().map(BoolQuery::Term),
        _ => Some(BoolQuery::Phrase(terms)),
    }
}

/// `w*`: the last word is a prefix; earlier words (as in `e-mai*`) are
/// required terms.
fn prefix_query(w: &str) -> Option<BoolQuery> {
    let mut words = Vec::new();
    for_each_word(w, |x| words.push(x.to_string()));
    let last = words.pop()?;
    let mut parts: Vec<Option<BoolQuery>> = words.iter().map(|x| terms_query(analyze(x))).collect();
    parts.push(Some(BoolQuery::Prefix(last)));
    combine(parts, BoolQuery::And)
}

#[cfg(test)]
mod tests {
    use super::BoolQuery::*;
    use super::*;

    fn t(s: &str) -> BoolQuery {
        Term(s.to_string())
    }

    #[test]
    fn grammar() {
        assert_eq!(parse_match("vaccines").unwrap(), t("vaccin"));
        assert_eq!(parse_match("mrna vaccine").unwrap(), And(vec![t("mrna"), t("vaccin")]));
        assert_eq!(parse_match("cat OR dog").unwrap(), Or(vec![t("cat"), t("dog")]));
        assert_eq!(
            parse_match("vaccine AND (mrna OR \"messenger rna\") NOT animal").unwrap(),
            And(vec![
                t("vaccin"),
                Or(vec![t("mrna"), Phrase(vec!["messeng".into(), "rna".into()])]),
                Not(Box::new(t("anim"))),
            ])
        );
        assert_eq!(parse_match("cat -dog").unwrap(), And(vec![t("cat"), Not(Box::new(t("dog")))]));
        assert_eq!(parse_match("Vacc*").unwrap(), Prefix("vacc".into()));
        assert_eq!(parse_match("e-mail").unwrap(), Phrase(vec!["e".into(), "mail".into()]));
        assert_eq!(parse_match("covid OR sars cov").unwrap(), Or(vec![t("covid"), And(vec![t("sar"), t("cov")])]));
        // Stopwords vanish, also as operands.
        assert_eq!(parse_match("the cat AND the").unwrap(), t("cat"));
        assert_eq!(parse_match("the OR a").unwrap(), Or(vec![]));
        assert_eq!(parse_match("NOT the").unwrap(), Or(vec![]));
        assert_eq!(parse_match("()").unwrap(), Or(vec![]));
        // Lowercase operators are words.
        assert_eq!(parse_match("cats or dogs").unwrap(), And(vec![t("cat"), t("dog")]));
        for bad in ["(cat", "cat)", "\"cat", "cat OR", "AND cat", "NOT"] {
            assert!(parse_match(bad).is_err(), "{bad}");
        }
    }
}
