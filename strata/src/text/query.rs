//! Boolean full-text queries and the `MATCH` grammar.
//!
//! ```text
//! query   := and ("OR" and)*
//! and     := unary (["AND"] unary)*        juxtaposition is AND
//! unary   := ("NOT" | "-") unary | primary  "a NOT b" is "a AND NOT b"
//! primary := "(" query ")" | ("\"phrase\"" | word | word* | word~ | word~N) ["^" weight]
//! ```
//!
//! A weight (`vaccine^2`, `"mrna vaccine"^0.5`) changes nothing about
//! which rows match: it is there so that the same text read by `bm25`
//! weighs the word ([`ranked_items`], the operators of a ranked query).
//!
//! Operators are uppercase (`AND`, `OR`, `NOT`), as in SQLite FTS5;
//! lowercase `and`/`or`/`not` are words (and stopwords). `-` negates when it
//! starts a token. Words and phrases are analysed like documents: a word
//! that analyses to several terms (`e-mail`) is a phrase, one that analyses
//! to none (a stopword) is dropped. A prefix (`vacc*`) is lowercased and
//! folded but not stemmed, and matches the indexed (stemmed) terms that
//! start with it. A typo-tolerant word (`nueral~`, `nueral~1`) also matches
//! the indexed terms within N edits of its term (`~` alone: 1 edit for terms
//! of 3 to 5 letters, 2 for longer ones; see [`super::fuzzy`]). The
//! field's [`Analyzer`] analyses them ([`parse_match_with`]).

use super::analyzer::{for_each_word, Analyzer};
use crate::error::{Error, Result};

/// A boolean query over analysed terms.
#[derive(Clone, Debug, PartialEq)]
pub enum BoolQuery {
    Term(String),
    /// Consecutive terms.
    Phrase(Vec<String>),
    /// Every indexed term with this prefix.
    Prefix(String),
    /// Every indexed term within `edits` edits of `term` (itself included).
    Fuzzy { term: String, edits: u8 },
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
            BoolQuery::Prefix(_) | BoolQuery::Fuzzy { .. } => {}
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

/// The tokens of `q`, and whether a word or phrase of it has a weight.
fn lex(q: &str) -> Result<(Vec<Tok>, bool)> {
    let mut out = Vec::new();
    let mut weighted = false;
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
                // A weight right after the closing quote.
                let rest = &q[end + 1..];
                let len = rest.find(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '"')).unwrap_or(rest.len());
                if rest.starts_with('^') && weight(&rest[1..len]).is_some() {
                    weighted = true;
                    while it.peek().is_some_and(|&(j, _)| j < end + 1 + len) {
                        it.next();
                    }
                }
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
                out.push(match split_weight(&q[start..end]) {
                    // A weighted word is a word, whatever it spells.
                    (w, Some(_)) => {
                        weighted = true;
                        Tok::Word(w.to_string())
                    }
                    ("AND", None) => Tok::And,
                    ("OR", None) => Tok::Or,
                    ("NOT", None) => Tok::Not,
                    (w, None) => Tok::Word(w.to_string()),
                });
            }
        }
    }
    Ok((out, weighted))
}

/// Whether a `MATCH` query weighs a word or phrase (`word^2`), which a
/// Strata from before weights reads as other words.
pub fn match_has_weights(q: &str) -> bool {
    lex(q).is_ok_and(|(_, w)| w)
}

/// A weight: digits with an optional fraction (`2`, `0.5`, `.5`), finite.
fn weight(s: &str) -> Option<f32> {
    let digits = s.chars().filter(char::is_ascii_digit).count();
    let ok = digits > 0 && s.chars().all(|c| c.is_ascii_digit() || c == '.') && s.matches('.').count() <= 1;
    ok.then(|| s.parse::<f32>().ok()).flatten().filter(|w| w.is_finite())
}

/// A word and its weight, if it ends in `^` and one (`vaccine^2`).
fn split_weight(w: &str) -> (&str, Option<f32>) {
    match w.rsplit_once('^') {
        Some((base, n)) if !base.is_empty() => match weight(n) {
            Some(x) => (base, Some(x)),
            None => (w, None),
        },
        _ => (w, None),
    }
}

/// Whether a ranked query's item must occur in a row (`+word`), must not
/// (`-word`), or may (a plain word).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occur {
    Should,
    Must,
    MustNot,
}

/// A word or quoted phrase of a ranked query ([`ranked_items`]), as
/// written: its text without sign, quotes or weight, still to analyse.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item<'a> {
    pub text: &'a str,
    /// Written in quotes.
    pub phrase: bool,
    pub occur: Occur,
    /// The weight written after it (`^2`), if any.
    pub weight: Option<f32>,
}

/// The words and phrases of a ranked (`bm25`) query in its operator
/// syntax, in order:
///
/// - `"a phrase"`: the words between paired quotes are one item;
/// - `+item`: it must occur in a row, `-item`: it must not; a sign counts
///   only at the start of a word or phrase, before something other than
///   a space or another sign;
/// - `item^w`: weight `w` (digits with an optional fraction), right after
///   the word or closing quote.
///
/// It never fails: an unpaired quote, a lone sign and a `^` without a
/// number are read as part of a word, which analysis splits as it splits
/// any word.
pub fn ranked_items(text: &str) -> Vec<Item<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let mut occur = Occur::Should;
        let mut start = i;
        if c == '+' || c == '-' {
            if let Some(n) = text[i + 1..].chars().next() {
                if !n.is_whitespace() && n != '+' && n != '-' {
                    occur = if c == '+' { Occur::Must } else { Occur::MustNot };
                    start = i + 1;
                }
            }
        }
        if let Some(inner) = text[start..].strip_prefix('"') {
            if let Some(len) = inner.find('"') {
                let mut end = start + 1 + len + 1;
                let rest = &text[end..];
                let run = rest.find(|c: char| c.is_whitespace() || c == '"').unwrap_or(rest.len());
                let w = rest.strip_prefix('^').and_then(|_| weight(&rest[1..run]));
                if w.is_some() {
                    end += run;
                }
                out.push(Item { text: &inner[..len], phrase: true, occur, weight: w });
                i = end;
                continue;
            }
        }
        // A word runs to a space or a quote; its first character may be an
        // unpaired quote.
        let first = text[start..].chars().next().map_or(0, char::len_utf8);
        let end = text[start + first..].find(|c: char| c.is_whitespace() || c == '"').map_or(text.len(), |j| start + first + j);
        let (word, w) = split_weight(&text[start..end]);
        out.push(Item { text: word, phrase: false, occur, weight: w });
        i = end;
    }
    out
}

/// Whether a ranked query uses an operator ([`ranked_items`]): a sign or
/// a weight, which a Strata from before them reads as plain words. Quotes
/// alone do not count: a phrase's words score as the words do.
pub fn has_operators(text: &str) -> bool {
    ranked_items(text).iter().any(|i| i.occur != Occur::Should || i.weight.is_some())
}

struct Parser {
    toks: Vec<Tok>,
    analyzer: Analyzer,
    pos: usize,
    /// Parentheses and NOTs open around the current position.
    depth: usize,
}

/// How deeply a `MATCH` query may nest parentheses and `NOT`s. Parsing
/// and evaluation recurse once per level, so without a bound one query
/// could overflow the thread's stack and abort the process.
pub const MAX_DEPTH: usize = 100;

/// Parses a `MATCH` query (see the module documentation) of an `english`
/// field. A query whose words are all stopwords matches nothing (`Or([])`).
pub fn parse_match(q: &str) -> Result<BoolQuery> {
    parse_match_with(q, Analyzer::English)
}

/// [`parse_match`] of a field whose text `analyzer` analyses.
pub fn parse_match_with(q: &str, analyzer: Analyzer) -> Result<BoolQuery> {
    let mut p = Parser { toks: lex(q)?.0, analyzer, pos: 0, depth: 0 };
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
            self.enter()?;
            let q = self.unary()?.map(|q| BoolQuery::Not(Box::new(q)));
            self.depth -= 1;
            return Ok(q);
        }
        self.primary()
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Error::Query(format!("MATCH query nests more than {MAX_DEPTH} levels of parentheses and NOT")));
        }
        Ok(())
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
                self.enter()?;
                let e = self.or()?;
                self.depth -= 1;
                if self.peek() != Some(&Tok::RParen) {
                    return Err(Error::Query("unbalanced parentheses in MATCH query".into()));
                }
                self.pos += 1;
                e
            }
            Tok::Phrase(s) => terms_query(self.analyzer.analyze(&s)),
            Tok::Word(w) => match (w.strip_suffix('*'), fuzzy_suffix(&w)) {
                (Some(stem), _) => prefix_query(stem, self.analyzer),
                (None, Some((word, edits))) => fuzzy_query(word, edits, self.analyzer)?,
                _ => terms_query(self.analyzer.analyze(&w)),
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

/// `word~` (`None` edits: by length) or `word~N`.
fn fuzzy_suffix(w: &str) -> Option<(&str, Option<u8>)> {
    let (word, n) = w.rsplit_once('~')?;
    if word.is_empty() {
        return None;
    }
    match n {
        "" => Some((word, None)),
        "0" | "1" | "2" => Some((word, Some(n.parse().unwrap()))),
        _ => None,
    }
}

fn fuzzy_query(word: &str, edits: Option<u8>, analyzer: Analyzer) -> Result<Option<BoolQuery>> {
    let terms = analyzer.analyze(word);
    let parts = terms
        .into_iter()
        .map(|t| {
            let e = edits.unwrap_or_else(|| auto_edits(t.chars().count()));
            Some(if e == 0 { BoolQuery::Term(t) } else { BoolQuery::Fuzzy { term: t, edits: e } })
        })
        .collect();
    Ok(combine(parts, BoolQuery::And))
}

/// Edits a word of `len` letters may have: 0 up to 2 letters, 1 up to 5, else 2.
pub fn auto_edits(len: usize) -> u8 {
    match len {
        0..=2 => 0,
        3..=5 => 1,
        _ => 2,
    }
}

/// Widens every word of `q`: with its synonyms (`synonyms`), and with the
/// indexed terms near it (`edits(term)` edits; 0 for none). Phrases with
/// synonyms get them too.
pub fn expand(q: BoolQuery, synonyms: Option<&super::synonyms::SynonymMap>, edits: &dyn Fn(&str) -> u8) -> BoolQuery {
    let with = |base: BoolQuery, key: &[String]| -> BoolQuery {
        let mut alts = vec![base];
        if let Some(s) = synonyms {
            for p in s.get(key) {
                alts.extend(terms_query(p.clone()));
            }
        }
        if let [t] = key {
            let e = edits(t);
            if e > 0 {
                alts.push(BoolQuery::Fuzzy { term: t.clone(), edits: e });
            }
        }
        if alts.len() == 1 { alts.pop().unwrap() } else { BoolQuery::Or(alts) }
    };
    match q {
        BoolQuery::Term(t) => {
            let key = [t.clone()];
            with(BoolQuery::Term(t), &key)
        }
        BoolQuery::Phrase(ts) => {
            let key = ts.clone();
            let p = BoolQuery::Phrase(ts);
            match synonyms {
                Some(s) if !s.get(&key).is_empty() => with(p, &key),
                _ => p,
            }
        }
        BoolQuery::And(cs) => BoolQuery::And(cs.into_iter().map(|c| expand(c, synonyms, edits)).collect()),
        BoolQuery::Or(cs) => BoolQuery::Or(cs.into_iter().map(|c| expand(c, synonyms, edits)).collect()),
        // Excluded words stay exact.
        q => q,
    }
}

/// Replaces every `Fuzzy` node by the terms `near(term, edits)` lists (an
/// `Or` of them, the term itself included).
pub fn resolve_fuzzy(q: BoolQuery, near: &mut dyn FnMut(&str, u8) -> Vec<String>) -> BoolQuery {
    match q {
        BoolQuery::Fuzzy { term, edits } => {
            let mut ts = near(&term, edits);
            if !ts.contains(&term) {
                ts.insert(0, term);
            }
            if ts.len() == 1 { BoolQuery::Term(ts.pop().unwrap()) } else { BoolQuery::Or(ts.into_iter().map(BoolQuery::Term).collect()) }
        }
        BoolQuery::And(cs) => BoolQuery::And(cs.into_iter().map(|c| resolve_fuzzy(c, near)).collect()),
        BoolQuery::Or(cs) => BoolQuery::Or(cs.into_iter().map(|c| resolve_fuzzy(c, near)).collect()),
        BoolQuery::Not(c) => BoolQuery::Not(Box::new(resolve_fuzzy(*c, near))),
        q => q,
    }
}

/// `w*`: the last word is a prefix; earlier words (as in `e-mai*`) are
/// required terms.
fn prefix_query(w: &str, analyzer: Analyzer) -> Option<BoolQuery> {
    let mut words = Vec::new();
    for_each_word(w, |x| words.push(x.to_string()));
    let last = words.pop()?;
    let mut parts: Vec<Option<BoolQuery>> = words.iter().map(|x| terms_query(analyzer.analyze(x))).collect();
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
        // Typo tolerance.
        assert_eq!(parse_match("nueral~").unwrap(), Fuzzy { term: "nueral".into(), edits: 2 });
        assert_eq!(parse_match("grpah~1 OR cat~").unwrap(), Or(vec![Fuzzy { term: "grpah".into(), edits: 1 }, Fuzzy { term: "cat".into(), edits: 1 }]));
        assert_eq!(parse_match("ox~").unwrap(), t("ox"));
        assert_eq!(parse_match("cat~dog").unwrap(), Phrase(vec!["cat".into(), "dog".into()]));
        for bad in ["(cat", "cat)", "\"cat", "cat OR", "AND cat", "NOT"] {
            assert!(parse_match(bad).is_err(), "{bad}");
        }
    }
}
