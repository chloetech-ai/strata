//! Text analysis, shared by indexing and queries.
//!
//! 1. **Split** into words: maximal runs of Unicode alphanumerics (combining
//!    accents U+0300..U+036F stay inside the word they follow).
//! 2. **Lowercase** (Unicode) and **fold** Latin accents and ligatures to
//!    ASCII (`é` -> `e`, `ß` -> `ss`, `œ` -> `oe`; Latin-1 Supplement and
//!    Latin Extended-A), dropping combining accents.
//! 3. **Stopwords**: Lucene's classic English list ([`STOPWORDS`]) is
//!    removed. Removed words take no position: positions number the kept
//!    terms, so the phrase `"quick the fox"` (analysed `[quick, fox]`)
//!    matches `quick fox` and `quick the fox`.
//! 4. **Stem** with Snowball English (Porter 2).
//!
//! That is the `english` [`Analyzer`], every TEXT field's unless its schema
//! names another: `simple` stops after step 2.

use std::borrow::Cow;
use std::sync::OnceLock;

use rust_stemmers::{Algorithm, Stemmer};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How a TEXT field's text becomes terms, for its documents and the
/// searches of it alike. A field's schema records its analyzer's name and
/// version, so a version of Strata whose analysis differs never reads a
/// field it did not index; a later change to an analyzer is a new version
/// (or a new name), and the old one stays as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Analyzer {
    /// `english` version 1: words lowercased and folded, Lucene's English
    /// stopwords removed, stemmed with Snowball English. A ranked query
    /// also drops [`QUERY_STOPWORDS`].
    #[default]
    English,
    /// `simple` version 1: words lowercased and folded, nothing removed or
    /// stemmed, for names, codes and text in other languages.
    Simple,
}

impl Analyzer {
    /// Every analyzer this version of Strata has.
    pub const ALL: [Analyzer; 2] = [Analyzer::English, Analyzer::Simple];

    pub fn name(self) -> &'static str {
        match self {
            Analyzer::English => "english",
            Analyzer::Simple => "simple",
        }
    }

    pub fn version(self) -> u32 {
        1
    }

    /// The analyzer of that name and version, or an error naming them.
    pub fn of(name: &str, version: u32) -> Result<Analyzer, String> {
        Analyzer::ALL.into_iter().find(|a| a.name() == name && a.version() == version).ok_or_else(|| {
            let known: Vec<String> = Analyzer::ALL.iter().map(|a| format!("{} {}", a.name(), a.version())).collect();
            format!("analyzer {name} version {version} is not one this version of Strata has (it has {})", known.join(", "))
        })
    }

    /// The format 3 feature a field with this analyzer needs
    /// (`analyzer:simple:1`), or none for `english`, which every version
    /// has.
    pub fn feature(self) -> Option<String> {
        (self != Analyzer::English).then(|| format!("analyzer:{}:{}", self.name(), self.version()))
    }

    /// The term for a lowercased, folded word, or `None` if it is dropped.
    pub fn term_of(self, word: &str) -> Option<Cow<'_, str>> {
        match self {
            Analyzer::English => term_of(word),
            Analyzer::Simple => Some(Cow::Borrowed(word)),
        }
    }

    /// The analysed terms of `text`, in order.
    pub fn analyze(self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for_each_word(text, |w| {
            if let Some(t) = self.term_of(w) {
                out.push(t.into_owned());
            }
        });
        out
    }

    /// The terms of a ranked query in order, repeats included, dropping
    /// [`QUERY_STOPWORDS`] if `drop_stopwords` (by default only `english`
    /// does: [`Analyzer::drops_query_stopwords`]).
    pub fn query_term_seq(self, text: &str, drop_stopwords: bool) -> Vec<String> {
        let mut out = Vec::new();
        for_each_word_in(text, |w, contraction| {
            if drop_stopwords && is_query_stopword(w, contraction) {
                return;
            }
            if let Some(t) = self.term_of(w) {
                out.push(t.into_owned());
            }
        });
        out
    }

    /// Whether a ranked query drops [`QUERY_STOPWORDS`] unless told
    /// otherwise: English function words, which only `english` expects.
    pub fn drops_query_stopwords(self) -> bool {
        self == Analyzer::English
    }
}

/// As a schema records it: `{"name": "simple", "version": 1}`.
impl Serialize for Analyzer {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Named {
            name: &'static str,
            version: u32,
        }
        Named { name: self.name(), version: self.version() }.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Analyzer {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Named {
            name: String,
            version: u32,
        }
        let n = Named::deserialize(d)?;
        Analyzer::of(&n.name, n.version).map_err(serde::de::Error::custom)
    }
}

/// Lucene's classic English stopword set (`EnglishAnalyzer`).
pub const STOPWORDS: [&str; 33] = [
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it", "no", "not", "of", "on",
    "or", "such", "that", "the", "their", "then", "there", "these", "they", "this", "to", "was", "will", "with",
];

/// English stopwords for ranked queries (NLTK's list without the apostrophe
/// forms, which the tokenizer splits), sorted. Natural-language queries
/// ("what is the origin of ...") carry many function words that the index
/// keeps but that only add noise to a BM25 ranking.
pub const QUERY_STOPWORDS: [&str; 153] = [
    "a", "about", "above", "after", "again", "against", "ain", "all", "am", "an", "and", "any", "are", "aren", "as",
    "at", "be", "because", "been", "before", "being", "below", "between", "both", "but", "by", "can", "couldn", "d",
    "did", "didn", "do", "does", "doesn", "doing", "don", "down", "during", "each", "few", "for", "from", "further",
    "had", "hadn", "has", "hasn", "have", "haven", "having", "he", "her", "here", "hers", "herself", "him", "himself",
    "his", "how", "i", "if", "in", "into", "is", "isn", "it", "its", "itself", "just", "ll", "m", "ma", "me",
    "mightn", "more", "most", "mustn", "my", "myself", "needn", "no", "nor", "not", "now", "o", "of", "off", "on",
    "once", "only", "or", "other", "our", "ours", "ourselves", "out", "over", "own", "re", "s", "same", "shan", "she",
    "should", "shouldn", "so", "some", "such", "t", "than", "that", "the", "their", "theirs", "them", "themselves",
    "then", "there", "these", "they", "this", "those", "through", "to", "too", "under", "until", "up", "ve", "very",
    "was", "wasn", "we", "were", "weren", "what", "when", "where", "which", "while", "who", "whom", "why", "will",
    "with", "won", "wouldn", "y", "you", "your", "yours", "yourself", "yourselves",
];

/// Letters of [`QUERY_STOPWORDS`] that are words of their own in a query
/// ("vitamin d", "T cell", "type O"): dropped only as the end of a
/// contraction ("don't", "it's", "I'm", "she'd").
const LETTER_STOPWORDS: [&str; 6] = ["d", "m", "o", "s", "t", "y"];

/// Whether a ranked query drops `word`; `contraction`: it follows an
/// apostrophe that follows a word.
fn is_query_stopword(word: &str, contraction: bool) -> bool {
    QUERY_STOPWORDS.binary_search(&word).is_ok() && (contraction || !LETTER_STOPWORDS.contains(&word))
}

/// The terms of a ranked (`bm25`) query: analysed like documents, minus
/// [`QUERY_STOPWORDS`], each distinct term once.
pub fn analyze_query(text: &str) -> Vec<String> {
    analyze_query_with(text, true)
}

/// [`analyze_query`], optionally keeping the query stopwords (documents'
/// own stopwords are never indexed, so those are dropped either way).
pub fn analyze_query_with(text: &str, drop_stopwords: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in query_term_seq(text, drop_stopwords) {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// The terms of a ranked query in order, repeats included (a repeated
/// term weighs more), as [`analyze_query_with`] analyses them.
pub fn query_term_seq(text: &str, drop_stopwords: bool) -> Vec<String> {
    Analyzer::English.query_term_seq(text, drop_stopwords)
}

/// The analysed terms of `text`, in order (`english`).
pub fn analyze(text: &str) -> Vec<String> {
    Analyzer::English.analyze(text)
}

/// The analysed terms of `text` with their positions (0, 1, 2, ... over the
/// kept terms).
pub fn analyze_with_positions(text: &str) -> Vec<(String, u32)> {
    analyze(text).into_iter().zip(0..).collect()
}

/// Is `word` (lowercased and folded) a stopword?
pub fn is_stopword(word: &str) -> bool {
    STOPWORDS.binary_search(&word).is_ok()
}

/// Words longer than this many bytes are not stemmed: no English word is,
/// and the stemmer takes time in the square of a word's length (a write
/// of one 900 KB word took 52 s).
pub const MAX_STEM_BYTES: usize = 64;

/// The term for a lowercased, folded word: `None` for stopwords, else its
/// stem (the word itself past [`MAX_STEM_BYTES`]).
pub fn term_of(word: &str) -> Option<Cow<'_, str>> {
    if is_stopword(word) {
        return None;
    }
    if word.len() > MAX_STEM_BYTES {
        return Some(Cow::Borrowed(word));
    }
    static STEMMER: OnceLock<Stemmer> = OnceLock::new();
    Some(STEMMER.get_or_init(|| Stemmer::create(Algorithm::English)).stem(word))
}

/// Lowercases and folds one word (no stopword removal, no stemming): the
/// form prefix queries match against.
pub fn normalize_word(word: &str) -> String {
    let mut out = String::new();
    fold_into(word, &mut out);
    out
}

/// Calls `f` with every word of `text`, lowercased and folded, in order.
pub fn for_each_word(text: &str, mut f: impl FnMut(&str)) {
    for_each_word_in(text, |w, _| f(w));
}

/// [`for_each_word`], also telling whether the word ends a contraction: it
/// follows an apostrophe (`'` or `’`) that follows a word, as `t` in
/// "don't".
fn for_each_word_in(text: &str, mut f: impl FnMut(&str, bool)) {
    let mut buf = String::new();
    let mut start: Option<usize> = None;
    let contraction = |s: usize| {
        let mut before = text[..s].chars().rev();
        matches!(before.next(), Some('\'' | '\u{2019}')) && before.next().is_some_and(is_word_char)
    };
    for (i, c) in text.char_indices() {
        if is_word_char(c) {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            let after = contraction(s);
            emit(&text[s..i], &mut buf, &mut |w| f(w, after));
        }
    }
    if let Some(s) = start {
        let after = contraction(s);
        emit(&text[s..], &mut buf, &mut |w| f(w, after));
    }
}

#[inline]
fn is_word_char(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphanumeric()
    } else {
        c.is_alphanumeric() || ('\u{300}'..='\u{36f}').contains(&c)
    }
}

fn emit(word: &str, buf: &mut String, f: &mut impl FnMut(&str)) {
    buf.clear();
    fold_into(word, buf);
    if !buf.is_empty() {
        f(buf);
    }
}

fn fold_into(word: &str, out: &mut String) {
    if word.is_ascii() {
        out.extend(word.bytes().map(|b| b.to_ascii_lowercase() as char));
        return;
    }
    for c in word.chars() {
        for l in c.to_lowercase() {
            match fold(l) {
                Some(s) => out.push_str(s),
                None => out.push(l),
            }
        }
    }
}

/// ASCII replacement of a lowercase Latin letter with a diacritic or a
/// ligature (`""` for combining accents); `None` to keep the character.
fn fold(c: char) -> Option<&'static str> {
    if c.is_ascii() {
        return None;
    }
    Some(match c {
        '\u{300}'..='\u{36f}' => "",
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ð' | 'ď' | 'đ' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ĳ' => "ij",
        'ĵ' => "j",
        'ķ' | 'ĸ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' | 'ŋ' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ſ' => "s",
        'ß' => "ss",
        'ţ' | 'ť' | 'ŧ' => "t",
        'þ' => "th",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_words_are_kept_whole_and_fast() {
        let at = "ing".repeat(20) + "s";
        assert_eq!(at.len(), 61);
        assert_ne!(term_of(&at).unwrap(), at, "stemmed");
        let past = "ing".repeat(25);
        assert_eq!(term_of(&past).unwrap(), past);
        let t0 = std::time::Instant::now();
        let huge = "y".repeat(1 << 20);
        assert_eq!(analyze(&huge), [huge.clone()]);
        assert!(t0.elapsed() < std::time::Duration::from_secs(1), "{:?}", t0.elapsed());
    }

    #[test]
    fn stopwords_sorted() {
        assert!(STOPWORDS.windows(2).all(|w| w[0] < w[1]));
        assert!(QUERY_STOPWORDS.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(analyze_query("What is the origin of the COVID-19 origin?"), ["origin", "covid", "19"]);
        assert_eq!(analyze_query_with("How can I learn?", false), ["how", "can", "i", "learn"]);
    }

    // IR-4: "vitamin d deficiency" ranked on [vitamin, defici] and "T cell
    // exhaustion" on [cell, exhaust]. A letter is a query word unless it
    // ends a contraction; a repeated term is kept for its weight.
    #[test]
    fn query_letters_and_repeats() {
        assert_eq!(analyze_query("vitamin D deficiency"), ["vitamin", "d", "defici"]);
        assert_eq!(analyze_query("T cell exhaustion"), ["t", "cell", "exhaust"]);
        assert_eq!(analyze_query("Why don't cats purr? It\u{2019}s late, I'd say"), ["cat", "purr", "late", "say"]);
        assert_eq!(analyze_query("the letter \u{2018}d\u{2019} and 's'"), ["letter", "d", "s"]);
        assert_eq!(query_term_seq("cat dog cats", true), ["cat", "dog", "cat"]);
        assert_eq!(analyze_query("cat dog cats"), ["cat", "dog"]);
    }

    #[test]
    fn analyzer_cases() {
        assert_eq!(analyze("The Quick, brown foxes JUMPED over the lazy dogs!"), ["quick", "brown", "fox", "jump", "over", "lazi", "dog"]);
        // Accents, ligatures and combining marks fold; non-Latin scripts are kept.
        assert_eq!(analyze("Café naïve Œuvre straße"), ["cafe", "naiv", "oeuvr", "strass"]);
        assert_eq!(analyze("cafe\u{301} Ünïcödé"), ["cafe", "unicod"]);
        assert_eq!(analyze("Москва 東京"), ["москва", "東京"]);
        // Punctuation splits words; digits are words.
        assert_eq!(analyze("e-mail COVID-19, 2019-nCoV"), ["e", "mail", "covid", "19", "2019", "ncov"]);
        // Only stopwords.
        assert!(analyze("to be or not to be, that is the...").is_empty());
        assert!(analyze("").is_empty());
        assert_eq!(analyze_with_positions("the cats and the dogs"), [("cat".to_string(), 0), ("dog".to_string(), 1)]);
        assert_eq!(normalize_word("Running"), "running");
        assert_eq!(term_of("running").as_deref(), Some("run"));
        assert_eq!(term_of("the"), None);
    }
}
