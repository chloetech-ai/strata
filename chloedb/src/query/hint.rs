//! What an error says beyond what went wrong: the names a misspelt name
//! may have meant, the columns and tables there are, values written as
//! SQL writes them, and where in a statement the parser stopped.

use crate::schema::{Field, FieldType, Schema};
use crate::value::Value;

/// The names among `names` within a few edits of `word` (any case): one
/// edit for a word of up to four letters, a third of its letters for
/// longer ones, nearest first; or the names it starts, or that start it.
pub fn nearest<'a>(word: &str, names: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let lw = word.to_lowercase();
    let w: Vec<char> = lw.chars().collect();
    if w.is_empty() {
        return Vec::new();
    }
    let k = if w.len() <= 4 { 1 } else { (w.len() / 3).clamp(2, 3) } as u8;
    let mut found: Vec<(u8, &str)> = Vec::new();
    for n in names {
        let l = n.to_lowercase();
        if l == lw || found.iter().any(|(_, x)| x.eq_ignore_ascii_case(n)) {
            continue;
        }
        // A name the word starts, or that starts the word, past a typo's reach.
        let starts = w.len() >= 3 && (l.starts_with(&lw) || (l.chars().count() >= 3 && lw.starts_with(&l)));
        if let Some(d) = crate::text::fuzzy::within(&w, &l, k).or(starts.then_some(k + 1)) {
            found.push((d, n));
        }
    }
    found.sort_by_key(|(d, _)| *d);
    found.into_iter().take(3).map(|(_, n)| n).collect()
}

/// " (did you mean a or b?)", or nothing.
pub fn did_you_mean<'a>(word: &str, names: impl IntoIterator<Item = &'a str>) -> String {
    match nearest(word, names).as_slice() {
        [] => String::new(),
        [a] => format!(" (did you mean {a}?)"),
        [a, b] => format!(" (did you mean {a} or {b}?)"),
        [a, rest @ ..] => format!(" (did you mean {a}, {}?)", rest.join(" or ")),
    }
}

/// A list of names for a message, the first `max` and how many more.
pub fn listing<S: AsRef<str>>(names: &[S], max: usize) -> String {
    let shown: Vec<&str> = names.iter().take(max).map(|s| s.as_ref()).collect();
    match names.len().saturating_sub(max) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

/// "no such column: x in t (did you mean y?); t's columns are a, b, c".
/// It starts with "no such column", which clients look for.
pub fn no_column(schema: &Schema, name: &str) -> String {
    let names: Vec<&str> = schema.fields.iter().map(|f| f.name.as_str()).collect();
    format!("no such column: {name} in {}{}; its columns are {}", schema.table, did_you_mean(name, names.iter().copied()), listing(&names, 30))
}

/// "no such table: x (did you mean y?); the tables are a, b". It starts
/// with "no such table", which a coordinator looks for to ask its shards
/// for their tables again.
pub fn no_table<'a>(name: &str, tables: impl IntoIterator<Item = &'a str>) -> String {
    let tables: Vec<&str> = tables.into_iter().collect();
    if tables.is_empty() {
        return format!("no such table: {name}");
    }
    let lower = name.to_ascii_lowercase();
    let schemas = if ["information_schema", "pg_", "sqlite_", "sys."].iter().any(|p| lower.starts_with(p)) {
        "; ChloeDB has no system tables: GET /v1/tables lists the tables and their columns"
    } else {
        ""
    };
    format!("no such table: {name}{}; the database's tables are {}{schemas}", did_you_mean(name, tables.iter().copied()), listing(&tables, 30))
}

/// Why a row whose key `key` a table already holds is refused, and the
/// writes that take it. It starts with "duplicate primary key".
pub fn duplicate_key(schema: &Schema, key: &Value) -> String {
    let k = schema.primary_key.map_or("key", |i| schema.fields[i].name.as_str());
    format!(
        "duplicate primary key: {} already has a row with {k} {} (INSERT OR REPLACE replaces it, ON CONFLICT ({k}) DO NOTHING keeps it, UPDATE changes it)",
        schema.table,
        sql_value(key)
    )
}

/// Why a write that gives key `key` to two of its rows (one INSERT's
/// VALUES, one file, one transaction) is refused, before any row has it.
/// It starts with "duplicate primary key" too.
pub fn repeated_key(schema: &Schema, key: &Value) -> String {
    let k = schema.primary_key.map_or("key", |i| schema.fields[i].name.as_str());
    format!(
        "duplicate primary key: {k} {} is given to two rows of this write (one statement's VALUES, one file or one transaction), and {} holds one row a key (INSERT OR REPLACE keeps the last of them)",
        sql_value(key),
        schema.table
    )
}

/// A field's type as `CREATE TABLE` writes it: `KEYWORD[]`, `VECTOR(64)`.
pub fn type_of(f: &Field) -> String {
    let base = match &f.ty {
        FieldType::Vector { dim, .. } => format!("VECTOR({dim})"),
        _ => f.type_name().to_string(),
    };
    if f.multi { format!("{base}[]") } else { base }
}

/// The TEXT fields of a table that `MATCH` and `bm25` search, each with
/// the fields it is made from: "body (title and description), note".
pub fn searchable(schema: &Schema) -> Vec<String> {
    schema
        .fields
        .iter()
        .filter(|f| f.indexed && (matches!(f.ty, FieldType::Text { .. }) || f.ty == FieldType::Json && f.text))
        .map(|f| match f.generated_from.as_slice() {
            [] => f.name.clone(),
            [a] => format!("{} (made from {a})", f.name),
            xs => format!("{} (made from {} and {})", f.name, xs[..xs.len() - 1].join(", "), xs[xs.len() - 1]),
        })
        .collect()
}

/// The fields of `schema` that `ok` takes, by name.
pub fn fields_where(schema: &Schema, ok: impl Fn(&Field) -> bool) -> Vec<String> {
    schema.fields.iter().filter(|f| ok(f)).map(|f| f.name.clone()).collect()
}

/// A value as SQL writes it, cut short past 60 characters: 'text', 5,
/// 2.5, TRUE, NULL, ARRAY['a', 'b'], a vector as its length.
pub fn sql_value(v: &Value) -> String {
    let s = match v {
        Value::Null => "NULL".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Str(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Timestamp(t) => format!("'{}'", crate::timestamp::format(*t)),
        Value::Vector(v) => format!("a vector of {} numbers", v.len()),
        Value::List(xs) => format!("ARRAY[{}]", xs.iter().map(sql_value).collect::<Vec<_>>().join(", ")),
    };
    match s.char_indices().nth(60) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s,
    }
}

/// What kind of value `v` is, for "a FLOAT takes a number, not text".
pub fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Int(_) | Value::Float(_) => "a number",
        Value::Str(_) => "text",
        Value::Bool(_) => "a boolean",
        Value::Timestamp(_) => "a time",
        Value::Vector(_) => "a vector",
        Value::List(_) => "a list",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_names() {
        let names = ["price", "rating", "reviews", "brand", "category", "products"];
        assert_eq!(nearest("pricee", names), vec!["price"]);
        assert_eq!(nearest("Prise", names), vec!["price"]);
        assert_eq!(nearest("product", names), vec!["products"]);
        assert_eq!(nearest("xyz", names), Vec::<&str>::new());
        assert_eq!(did_you_mean("brnd", names), " (did you mean brand?)");
        assert_eq!(did_you_mean("zzzz", names), "");
    }

    #[test]
    fn values_as_sql() {
        assert_eq!(sql_value(&Value::Str("it's".into())), "'it''s'");
        assert_eq!(sql_value(&Value::List(vec![Value::Str("a".into()), Value::Int(2)])), "ARRAY['a', 2]");
        assert_eq!(sql_value(&Value::Null), "NULL");
        assert!(sql_value(&Value::Str("x".repeat(100))).ends_with("..."));
    }
}

/// What an answer's reader may want to know about it, from its statement:
/// why a `MATCH` of several words found no row (it needs every word in a
/// row), or why `score()` is 0 (a `MATCH` filters and does not rank).
/// `rows` is how many rows the answer has, and `zero_scores` whether it
/// has scores and each is 0.
pub fn of_answer(sql: &str, rows: usize, zero_scores: bool) -> Option<String> {
    use sqlparser::dialect::GenericDialect;
    use sqlparser::tokenizer::{Token, Tokenizer};
    if !(rows == 0 || zero_scores) || !sql.as_bytes().windows(5).any(|w| w.eq_ignore_ascii_case(b"match")) {
        return None;
    }
    let tokens: Vec<Token> = Tokenizer::new(&GenericDialect {}, sql).tokenize().ok()?.into_iter().filter(|t| !matches!(t, Token::Whitespace(_))).collect();
    let word = |t: &Token, w: &str| matches!(t, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    let ranked = tokens.iter().any(|t| ["bm25", "similarity", "rrf", "decay", "maxsim", "distance"].iter().any(|w| word(t, w)));
    if rows == 0 {
        // MATCH ( field , 'words' ) with plain words alone.
        for i in 0..tokens.len().saturating_sub(5) {
            if !(word(&tokens[i], "MATCH") && tokens[i + 1] == Token::LParen && tokens[i + 3] == Token::Comma) {
                continue;
            }
            let (Token::Word(field), Token::SingleQuotedString(q)) = (&tokens[i + 2], &tokens[i + 4]) else { continue };
            let words: Vec<&str> = q.split_whitespace().collect();
            let plain = words.len() >= 2
                && words.iter().all(|w| w.chars().all(|c| c.is_alphanumeric() || c == '\'') && !["AND", "OR", "NOT", "NEAR"].contains(w));
            if !plain || !matches!(tokens.get(i + 5), Some(Token::RParen | Token::Comma)) {
                continue;
            }
            // The call's options, to its closing parenthesis.
            let end = tokens[i + 5..].iter().position(|t| *t == Token::RParen).map_or(tokens.len(), |n| i + 5 + n);
            let fuzzy = tokens[i + 5..end].iter().any(|t| word(t, "fuzzy"));
            let typos = if fuzzy {
                String::from(" (fuzzy => 'auto' corrects a word only when the index has no such word and a near one is in more rows)")
            } else {
                format!(", and MATCH({field}, '{q}', fuzzy => 'auto') forgives typos")
            };
            return Some(format!(
                "MATCH({field}, '{q}') holds for a row that has every one of its {} words, and no row has them all: MATCH({field}, '{}') finds rows with any of them, MATCH({field}, '{q}', min => 2) rows with two, ORDER BY bm25({field}, '{q}') DESC ranks rows by them{typos}",
                words.len(),
                words.join(" OR "),
            ));
        }
        return None;
    }
    let scored = tokens.windows(3).any(|w| word(&w[0], "score") && w[1] == Token::LParen && w[2] == Token::RParen);
    (zero_scores && scored && !ranked).then(|| "score() is 0: MATCH filters rows and does not rank them. Rank with ORDER BY bm25(field, 'words') DESC (and SELECT bm25(field, 'words') or score() for each row's score)".to_string())
}
