//! Parsing SQL text into statements, with a bound on expression depth.
//!
//! The parser builds `a OR b OR c ...` as a tree as deep as the chain is
//! long, and binding, printing and even dropping that tree recurse once
//! per level. Without a bound, one long chain overflows the thread's stack
//! and aborts the process (a tokio worker has 2 MB). Like SQLite's
//! `SQLITE_MAX_EXPR_DEPTH`, statements whose expressions could nest more
//! than [`MAX_EXPR_DEPTH`] levels are refused before a tree is built.

use sqlparser::ast::Statement;
use sqlparser::dialect::GenericDialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::error::{Error, Result};

/// How deeply an expression may nest: parentheses plus the operators a
/// chain such as `a OR b OR ...` or `x + y + ...` strings together.
/// SQLite's default limit is 1,000 too.
pub const MAX_EXPR_DEPTH: usize = 1000;

/// The call `MAX k PER field` parses as, the last item of its query's
/// ORDER BY: `__max_per(k, field)` ([`crate::query::compound`]).
pub const PER_CAP: &str = "__max_per";

/// Parses `sql_text` into its statements (generic SQL dialect).
pub fn parse(sql_text: &str) -> Result<Vec<Statement>> {
    let dialect = GenericDialect {};
    let tokens = per_caps(Tokenizer::new(&dialect, sql_text).tokenize().map_err(|e| Error::Query(e.to_string()))?);
    check_depth(&tokens)?;
    Parser::new(&dialect).with_tokens(tokens).parse_statements().map_err(|e| Error::Query(e.to_string()))
}

/// `MAX k PER field` (after an ORDER BY, or without one) as the call
/// [`PER_CAP`] ending the ORDER BY: `ORDER BY a DESC MAX 2 PER b` parses
/// as `ORDER BY a DESC, __max_per(2, b)`. `MAX` followed by a number is
/// nothing else in SQL. The field is a name, or names joined by dots.
fn per_caps(tokens: Vec<Token>) -> Vec<Token> {
    let word = |t: &Token, w: &str| matches!(t, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    // Whether the query of each open parenthesis has had an ORDER BY.
    let mut ordered = vec![false];
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut last: Option<Token> = None;
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        match t {
            Token::LParen => ordered.push(false),
            Token::RParen if ordered.len() > 1 => {
                ordered.pop();
            }
            Token::Word(w) if w.keyword == Keyword::SELECT => *ordered.last_mut().expect("the statement level") = false,
            Token::Word(w) if w.keyword == Keyword::BY && last.as_ref().is_some_and(|l| word(l, "ORDER")) => *ordered.last_mut().expect("the statement level") = true,
            _ => {}
        }
        if word(t, "MAX") {
            let next: Vec<usize> = (i + 1..tokens.len()).filter(|&j| !matches!(tokens[j], Token::Whitespace(_))).take(3).collect();
            if let [k, per, name] = next[..] {
                if matches!(tokens[k], Token::Number(..)) && word(&tokens[per], "PER") && matches!(tokens[name], Token::Word(_)) {
                    let mut end = name + 1;
                    while end + 1 < tokens.len() && tokens[end] == Token::Period && matches!(tokens[end + 1], Token::Word(_)) {
                        end += 2;
                    }
                    let level = ordered.last_mut().expect("the statement level");
                    match *level {
                        true => out.push(Token::Comma),
                        false => out.extend([Token::make_keyword("ORDER"), Token::Whitespace(sqlparser::tokenizer::Whitespace::Space), Token::make_keyword("BY")]),
                    }
                    *level = true;
                    out.extend([Token::Whitespace(sqlparser::tokenizer::Whitespace::Space), Token::make_word(PER_CAP, None), Token::LParen, tokens[k].clone(), Token::Comma]);
                    out.extend(tokens[name..end].iter().cloned());
                    out.push(Token::RParen);
                    last = Some(Token::RParen);
                    i = end;
                    continue;
                }
            }
        }
        if !matches!(t, Token::Whitespace(_)) {
            last = Some(t.clone());
        }
        out.push(t.clone());
        i += 1;
    }
    out
}

/// `sql` printed from a parsed statement, its [`PER_CAP`] calls written
/// back as `MAX k PER field`.
pub fn unparse_per_caps(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(at) = rest.find(&format!("{PER_CAP}(")) {
        let args_at = at + PER_CAP.len() + 1;
        let (Some(close), Some(comma)) = (rest[args_at..].find(')'), rest[args_at..].find(", ")) else { break };
        let before = &rest[..at];
        let before = before.strip_suffix(", ").or_else(|| before.strip_suffix("ORDER BY ")).unwrap_or(before);
        out.push_str(before.trim_end());
        out.push_str(&format!(" MAX {} PER {}", &rest[args_at..args_at + comma], &rest[args_at + comma + 2..args_at + close]));
        rest = &rest[args_at + close + 1..];
    }
    out.push_str(rest);
    out
}

/// An upper bound of the expression depth `tokens` can parse into: within
/// each open parenthesis (or bracket), the operators since its last comma,
/// summed over the open ones, plus how many are open. Commas separate list
/// items, which sit side by side, so `IN (1, 2, ...)` and `VALUES (...),
/// (...)` stay shallow however long they are.
fn check_depth(tokens: &[Token]) -> Result<()> {
    let mut open: Vec<usize> = vec![0];
    let mut sum = 0usize;
    for t in tokens {
        match t {
            Token::LParen | Token::LBracket | Token::LBrace => open.push(0),
            Token::RParen | Token::RBracket | Token::RBrace => {
                if open.len() > 1 {
                    sum -= open.pop().unwrap_or(0);
                }
            }
            Token::Comma | Token::SemiColon => {
                let last = open.last_mut().expect("the statement level");
                sum -= *last;
                *last = 0;
            }
            t if chains(t) => {
                *open.last_mut().expect("the statement level") += 1;
                sum += 1;
            }
            _ => {}
        }
        if sum + open.len() > MAX_EXPR_DEPTH {
            return Err(Error::Query(format!("the statement nests expressions more than {MAX_EXPR_DEPTH} levels deep (long chains of AND, OR or arithmetic, or deep parentheses); use IN lists or split it")));
        }
    }
    Ok(())
}

/// Whether a token can join two expressions into a deeper one: every
/// operator symbol and the operator keywords. Names, literals and other
/// keywords cannot.
fn chains(t: &Token) -> bool {
    match t {
        Token::Word(w) => matches!(
            w.keyword,
            Keyword::AND
                | Keyword::OR
                | Keyword::XOR
                | Keyword::NOT
                | Keyword::IS
                | Keyword::IN
                | Keyword::BETWEEN
                | Keyword::LIKE
                | Keyword::ILIKE
                | Keyword::SIMILAR
                | Keyword::RLIKE
                | Keyword::REGEXP
                | Keyword::DIV
                | Keyword::MOD
                | Keyword::COLLATE
                | Keyword::AT
                | Keyword::OVERLAPS
                | Keyword::MEMBER
                | Keyword::ESCAPE
        ),
        Token::Whitespace(_)
        | Token::EOF
        | Token::Number(..)
        | Token::Char(_)
        | Token::SingleQuotedString(_)
        | Token::DoubleQuotedString(_)
        | Token::TripleSingleQuotedString(_)
        | Token::TripleDoubleQuotedString(_)
        | Token::DollarQuotedString(_)
        | Token::SingleQuotedByteStringLiteral(_)
        | Token::DoubleQuotedByteStringLiteral(_)
        | Token::TripleSingleQuotedByteStringLiteral(_)
        | Token::TripleDoubleQuotedByteStringLiteral(_)
        | Token::SingleQuotedRawStringLiteral(_)
        | Token::DoubleQuotedRawStringLiteral(_)
        | Token::TripleSingleQuotedRawStringLiteral(_)
        | Token::TripleDoubleQuotedRawStringLiteral(_)
        | Token::NationalStringLiteral(_)
        | Token::EscapedStringLiteral(_)
        | Token::UnicodeStringLiteral(_)
        | Token::HexStringLiteral(_)
        | Token::Placeholder(_) => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_k_per_parses_as_the_last_ordering() {
        let as_sql = |q: &str| parse(q).unwrap()[0].to_string();
        assert_eq!(as_sql("SELECT id FROM t ORDER BY bm25(body, 'x') DESC MAX 2 PER venue LIMIT 20"), "SELECT id FROM t ORDER BY bm25(body, 'x') DESC, __max_per(2, venue) LIMIT 20");
        assert_eq!(as_sql("SELECT id FROM t max 1 per a.b"), "SELECT id FROM t ORDER BY __max_per(1, a.b)");
        assert_eq!(
            as_sql("SELECT id FROM t WHERE id IN (SELECT id FROM t ORDER BY n MAX 3 PER v LIMIT 5) ORDER BY MAX(n) DESC"),
            "SELECT id FROM t WHERE id IN (SELECT id FROM t ORDER BY n, __max_per(3, v) LIMIT 5) ORDER BY MAX(n) DESC"
        );
        assert_eq!(as_sql("SELECT MAX(n) FROM t"), "SELECT MAX(n) FROM t");
        for q in ["SELECT id FROM t ORDER BY n DESC MAX 2 PER venue LIMIT 20", "SELECT id FROM t WHERE id IN (SELECT id FROM t ORDER BY n MAX 3 PER a.v) LIMIT 5"] {
            assert_eq!(unparse_per_caps(&as_sql(q)), q);
        }
    }

    #[test]
    fn long_lists_pass_and_long_chains_do_not() {
        let list = (0..50_000).map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
        assert!(parse(&format!("SELECT id FROM t WHERE id IN ({list})")).is_ok());
        let rows = (0..10_000).map(|i| format!("({i}, -1.5, 'a')")).collect::<Vec<_>>().join(", ");
        assert!(parse(&format!("INSERT INTO t VALUES {rows}")).is_ok());
        let ok = vec!["a = 1"; 300].join(" AND ");
        assert!(parse(&format!("SELECT id FROM t WHERE {ok}")).is_ok());
        for bad in [vec!["a = 1"; 2000].join(" OR "), vec!["x"; 2000].join(" + "), format!("{}a", "NOT ".repeat(2000))] {
            let r = parse(&format!("SELECT id FROM t WHERE {bad}"));
            assert!(matches!(&r, Err(Error::Query(m)) if m.contains("nests")), "{r:?}");
        }
    }
}
