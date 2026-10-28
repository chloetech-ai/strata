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

/// Parses `sql_text` into its statements (generic SQL dialect).
pub fn parse(sql_text: &str) -> Result<Vec<Statement>> {
    let dialect = GenericDialect {};
    let tokens = Tokenizer::new(&dialect, sql_text).tokenize().map_err(|e| Error::Query(e.to_string()))?;
    check_depth(&tokens)?;
    Parser::new(&dialect).with_tokens(tokens).parse_statements().map_err(|e| Error::Query(e.to_string()))
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
