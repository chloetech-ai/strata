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

use std::borrow::Cow;
use std::sync::OnceLock;

use rust_stemmers::{Algorithm, Stemmer};

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

/// The terms of a ranked (`bm25`) query: analysed like documents, minus
/// [`QUERY_STOPWORDS`], each distinct term once.
pub fn analyze_query(text: &str) -> Vec<String> {
    analyze_query_with(text, true)
}

/// [`analyze_query`], optionally keeping the query stopwords (documents'
/// own stopwords are never indexed, so those are dropped either way).
pub fn analyze_query_with(text: &str, drop_stopwords: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for_each_word(text, |w| {
        if drop_stopwords && QUERY_STOPWORDS.binary_search(&w).is_ok() {
            return;
        }
        if let Some(t) = term_of(w) {
            if !out.iter().any(|x| *x == t) {
                out.push(t.into_owned());
            }
        }
    });
    out
}

/// The analysed terms of `text`, in order.
pub fn analyze(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for_each_word(text, |w| {
        if let Some(t) = term_of(w) {
            out.push(t.into_owned());
        }
    });
    out
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

/// The term for a lowercased, folded word: `None` for stopwords, else its
/// stem.
pub fn term_of(word: &str) -> Option<Cow<'_, str>> {
    if is_stopword(word) {
        return None;
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
    let mut buf = String::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if is_word_char(c) {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            emit(&text[s..i], &mut buf, &mut f);
        }
    }
    if let Some(s) = start {
        emit(&text[s..], &mut buf, &mut f);
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
    fn stopwords_sorted() {
        assert!(STOPWORDS.windows(2).all(|w| w[0] < w[1]));
        assert!(QUERY_STOPWORDS.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(analyze_query("What is the origin of the COVID-19 origin?"), ["origin", "covid", "19"]);
        assert_eq!(analyze_query_with("How can I learn?", false), ["how", "can", "i", "learn"]);
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
