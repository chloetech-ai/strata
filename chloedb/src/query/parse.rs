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
use sqlparser::tokenizer::{Span, Token, TokenWithSpan, Tokenizer};

use crate::error::{Error, Result};

/// How deeply an expression may nest: parentheses plus the operators a
/// chain such as `a OR b OR ...` or `x + y + ...` strings together.
/// SQLite's default limit is 1,000 too.
pub const MAX_EXPR_DEPTH: usize = 1000;

/// The stack ChloeDB's own query threads (the pool's, DataFusion's) and
/// chloedb-server's start with: four times the 2 MiB that the statements
/// [`MAX_EXPR_DEPTH`] lets through are tested to plan and run in
/// (`tests/deep_statements.rs`), so that a thread with a deeper call
/// above the query still has room. Untouched, the pages cost no memory.
pub const QUERY_STACK: usize = 8 << 20;

/// The levels of [`MAX_EXPR_DEPTH`] that each `UNION`, `INTERSECT` or
/// `EXCEPT` counts for: planning and running a chain of them recurses
/// once per query joined, with frames larger than an operator's.
pub const SET_OP_LEVELS: usize = 8;

/// The call `MAX k PER field` parses as, the last item of its query's
/// ORDER BY: `__max_per(k, field)` ([`crate::query::compound`]).
pub const PER_CAP: &str = "__max_per";

/// Parses `sql_text` into its statements (generic SQL dialect).
pub fn parse(sql_text: &str) -> Result<Vec<Statement>> {
    let dialect = GenericDialect {};
    let located = Tokenizer::new(&dialect, sql_text).tokenize_with_location().map_err(|e| Error::Query(format!("SQL syntax error: {e}")))?;
    let tokens = within_groups(per_caps(located))?;
    let plain: Vec<Token> = tokens.iter().map(|t| t.token.clone()).collect();
    check_depth(&plain)?;
    let mut stmts = Parser::new(&dialect).with_tokens_with_locations(tokens).parse_statements().map_err(|e| Error::Query(syntax_error(&e.to_string(), sql_text)))?;
    for s in &mut stmts {
        match s {
            Statement::Query(q) | Statement::Insert(sqlparser::ast::Insert { source: Some(q), .. }) => {
                super::compound::filters_as_case(q)?;
                super::rewrite::query(q);
            }
            _ => {}
        }
    }
    Ok(stmts)
}

/// Parses `text` as one SQL expression (a condition given apart from its
/// statement), with the bound on depth [`parse`] has: anything after the
/// expression, such as a `LIMIT` or a second statement, is refused rather
/// than read as part of a statement around it.
pub fn parse_expr(text: &str) -> Result<sqlparser::ast::Expr> {
    let dialect = GenericDialect {};
    let located = Tokenizer::new(&dialect, text).tokenize_with_location().map_err(|e| Error::Query(format!("SQL syntax error: {e}")))?;
    let tokens = within_groups(per_caps(located))?;
    let plain: Vec<Token> = tokens.iter().map(|t| t.token.clone()).collect();
    check_depth(&plain)?;
    let mut p = Parser::new(&dialect).with_tokens_with_locations(tokens);
    let e = p.parse_expr().map_err(|e| Error::Query(syntax_error(&e.to_string(), text)))?;
    if p.peek_token().token != Token::EOF {
        return Err(Error::Query(format!("{text:?} is not one condition: it goes on after {e}")));
    }
    Ok(e)
}

/// Keywords a statement is written with, for "did you mean" a misspelt one.
const KEYWORDS: [&str; 44] = [
    "SELECT", "FROM", "WHERE", "GROUP", "ORDER", "BY", "HAVING", "LIMIT", "OFFSET", "JOIN", "LEFT", "INNER", "ON", "AS", "AND", "OR", "NOT", "IN", "IS", "NULL", "LIKE", "ILIKE", "BETWEEN",
    "DISTINCT", "WITH", "UNION", "INTERSECT", "EXCEPT", "CASE", "WHEN", "THEN", "ELSE", "END", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "TABLE", "DROP", "ALTER", "EXPLAIN",
];

/// The parser's message (`sql parser error: Expected: end of statement,
/// found: FORM at Line: 1, Column: 10`) as one that says where, what came
/// instead and, for a misspelt keyword, which it may be: `SQL syntax
/// error at line 1, column 10 (near "FORM products"): expected the end of
/// the statement, found FORM (did you mean FROM?)`.
fn syntax_error(message: &str, sql_text: &str) -> String {
    let m = message.trim_start_matches("sql parser error: ");
    // The place, if the parser gave one.
    let (m, at) = match m.rfind(" at Line: ") {
        Some(i) => {
            let place = &m[i + " at Line: ".len()..];
            let mut parts = place.split(", Column: ");
            let line = parts.next().and_then(|l| l.trim().parse::<usize>().ok());
            let col = parts.next().and_then(|c| c.trim().parse::<usize>().ok());
            (&m[..i], line.zip(col))
        }
        None => (m, None),
    };
    let (expected, found) = match m.strip_prefix("Expected: ").and_then(|r| r.rsplit_once(", found: ")) {
        Some((e, f)) => (Some(e), Some(f)),
        None => (None, None),
    };
    // A misspelt clause keyword before the error reads as an alias of what
    // precedes it (`SELECT id FORM t`: FORM names id), and the parser stops
    // a word later: the place to fix is the misspelt word.
    if let (Some((line, col)), Some(f)) = (at, found) {
        if let Some((w, wl, wc, kw)) = alias_typo(sql_text, line, col) {
            let mut out = format!("SQL syntax error at line {wl}, column {wc}");
            if let Some(near) = near(sql_text, wl as usize, wc as usize) {
                out.push_str(&format!(" (near \"{near}\")"));
            }
            out.push_str(&format!(": {w} reads as a name here, the alias of what comes before it, and then {f} cannot follow (did you mean {kw}?)"));
            return out;
        }
    }
    // `body MATCH 'words'`, as SQLite's FTS writes it: ChloeDB's MATCH is
    // a function of the field and the words.
    if let (Some((line, col)), Some(rest)) = (at, m.strip_prefix("No infix parser for token ")) {
        if let Some(word) = rest.split("value: \"").nth(1).and_then(|r| r.split('"').next()) {
            let mut out = format!("SQL syntax error at line {line}, column {col}");
            if let Some(near) = near(sql_text, line, col) {
                out.push_str(&format!(" (near \"{near}\")"));
            }
            if word.eq_ignore_ascii_case("MATCH") {
                let (field, words) = infix_match(sql_text, line, col).unwrap_or_else(|| ("field".into(), "'words'".into()));
                out.push_str(&format!(": MATCH is written as a function of the field and the words, MATCH({field}, {words}), not between them; ORDER BY bm25({field}, {words}) DESC ranks by it"));
            } else {
                out.push_str(&format!(": {word} cannot follow what comes before it here; between two values comes an operator (=, <, LIKE, IN, AND, OR)"));
            }
            return out;
        }
    }
    let mut out = String::from("SQL syntax error");
    if let Some((line, col)) = at {
        out.push_str(&format!(" at line {line}, column {col}"));
        if let Some(near) = near(sql_text, line, col) {
            out.push_str(&format!(" (near \"{near}\")"));
        }
    } else if found == Some("EOF") {
        out.push_str(" at the end of the statement");
    }
    out.push_str(": ");
    match (expected, found) {
        (Some(e), Some("EOF")) => out.push_str(&format!("the statement ends where {} should follow", readable(e))),
        (Some(e), Some(f)) => {
            out.push_str(&format!("expected {}, found {f}", readable(e)));
            let word = f.trim_matches(|c| c == '"' || c == '`');
            if word.chars().all(|c| c.is_ascii_alphabetic()) && !KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word)) {
                out.push_str(&super::hint::did_you_mean(&word.to_ascii_uppercase(), KEYWORDS.iter().copied()));
            }
        }
        _ => out.push_str(m),
    }
    out
}

/// Clause keywords a misspelling of which reads as an alias.
const CLAUSES: [&str; 12] = ["FROM", "WHERE", "GROUP", "ORDER", "HAVING", "LIMIT", "OFFSET", "JOIN", "UNION", "VALUES", "INTO", "SET"];

/// The word just before (`line`, `col`) when it is a name of three or more
/// letters near a clause keyword, not a keyword itself: the word, its line
/// and column, and the keyword it may be.
fn alias_typo(sql_text: &str, line: usize, col: usize) -> Option<(String, u64, u64, &'static str)> {
    let tokens = Tokenizer::new(&GenericDialect {}, sql_text).tokenize_with_location().ok()?;
    let at = tokens.iter().position(|t| t.span.start.line == line as u64 && t.span.start.column == col as u64)?;
    let prev = tokens[..at].iter().rev().find(|t| !matches!(t.token, Token::Whitespace(_)))?;
    let Token::Word(w) = &prev.token else { return None };
    if w.quote_style.is_some() || w.value.len() < 3 || !w.value.chars().all(|c| c.is_ascii_alphabetic()) || KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(&w.value)) {
        return None;
    }
    let kw = *super::hint::nearest(&w.value.to_ascii_uppercase(), CLAUSES.iter().copied()).first()?;
    Some((w.value.clone(), prev.span.start.line, prev.span.start.column, kw))
}

/// The field before and the text after the `MATCH` at (`line`, `col`) of
/// `body MATCH 'words'`, as the function call writes them.
fn infix_match(sql_text: &str, line: usize, col: usize) -> Option<(String, String)> {
    let tokens = Tokenizer::new(&GenericDialect {}, sql_text).tokenize_with_location().ok()?;
    let at = tokens.iter().position(|t| t.span.start.line == line as u64 && t.span.start.column == col as u64)?;
    let solid = |t: &&TokenWithSpan| !matches!(t.token, Token::Whitespace(_));
    let prev = tokens[..at].iter().rev().find(solid)?;
    let mut after = tokens[at + 1..].iter().filter(solid);
    let next = after.next()?;
    let field = match &prev.token {
        Token::Word(w) => w.to_string(),
        _ => return None,
    };
    let words = match &next.token {
        Token::SingleQuotedString(_) | Token::Placeholder(_) => next.token.to_string(),
        // `:q`, a parameter, as the tokenizer reads it: a colon, then a name.
        Token::Colon => match &after.next()?.token {
            Token::Word(w) => format!(":{}", w.value),
            _ => return None,
        },
        _ => return None,
    };
    Some((field, words))
}

/// The parser's names of what it expected, as a sentence says them.
fn readable(expected: &str) -> String {
    match expected {
        "end of statement" => "the end of the statement (or ; before another)".into(),
        "an SQL statement" => "a statement such as SELECT, INSERT, UPDATE, DELETE or CREATE TABLE".into(),
        "an expression" => "an expression (a column, a value or a condition)".into(),
        "identifier" => "a name".into(),
        e => e.to_string(),
    }
}

/// The text from (`line`, `col`), both from 1, to the end of the next word.
fn near(sql_text: &str, line: usize, col: usize) -> Option<String> {
    let l = sql_text.lines().nth(line.checked_sub(1)?)?;
    let rest: String = l.chars().skip(col.saturating_sub(1)).collect();
    let rest = rest.trim_end();
    if rest.is_empty() {
        return None;
    }
    // Two words, at most 30 characters.
    let mut end = 0;
    for (n, w) in rest.split_inclusive(char::is_whitespace).enumerate() {
        if n >= 2 || end + w.len() > 30 {
            break;
        }
        end += w.len();
    }
    let cut = if end == 0 { rest.chars().take(30).collect::<String>() } else { rest[..end].trim_end().to_string() };
    Some(cut)
}

/// `MAX k PER field` (after an ORDER BY, or without one) as the call
/// [`PER_CAP`] ending the ORDER BY: `ORDER BY a DESC MAX 2 PER b` parses
/// as `ORDER BY a DESC, __max_per(2, b)`. `MAX` followed by a number is
/// nothing else in SQL. The field is a name, or names joined by dots.
fn per_caps(located: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let word = |t: &Token, w: &str| matches!(t, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    let tokens: Vec<Token> = located.iter().map(|t| t.token.clone()).collect();
    // Whether the query of each open parenthesis has had an ORDER BY.
    let mut ordered = vec![false];
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(tokens.len());
    // The tokens written in place of `MAX k PER field` take its place.
    let put = |out: &mut Vec<TokenWithSpan>, span: Span, ts: Vec<Token>| out.extend(ts.into_iter().map(|token| TokenWithSpan { token, span }));
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
                    let span = located[i].span;
                    let level = ordered.last_mut().expect("the statement level");
                    match *level {
                        true => put(&mut out, span, vec![Token::Comma]),
                        false => put(&mut out, span, vec![Token::make_keyword("ORDER"), Token::Whitespace(sqlparser::tokenizer::Whitespace::Space), Token::make_keyword("BY")]),
                    }
                    *level = true;
                    put(&mut out, span, vec![Token::Whitespace(sqlparser::tokenizer::Whitespace::Space), Token::make_word(PER_CAP, None), Token::LParen, tokens[k].clone(), Token::Comma]);
                    out.extend(located[name..end].iter().cloned());
                    put(&mut out, located[end - 1].span, vec![Token::RParen]);
                    last = Some(Token::RParen);
                    i = end;
                    continue;
                }
            }
        }
        if !matches!(t, Token::Whitespace(_)) {
            last = Some(t.clone());
        }
        out.push(located[i].clone());
        i += 1;
    }
    out
}

/// `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY x)` (and `PERCENTILE_DISC`)
/// as the call `PERCENTILE_CONT(x, p)` it means, so that every reader of
/// calls finds the column among the arguments. An ascending order is the
/// only one: `DESC` is refused.
fn within_groups(located: Vec<TokenWithSpan>) -> Result<Vec<TokenWithSpan>> {
    let word = |t: &Token, w: &str| matches!(t, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    let n = located.len();
    let tok = |i: usize| &located[i].token;
    // The next token from `i` that is not whitespace.
    let next = |i: usize| (i..n).find(|&j| !matches!(tok(j), Token::Whitespace(_)));
    // The parenthesis closing the one at `i`.
    let close = |i: usize| {
        let mut depth = 0usize;
        for j in i..n {
            match tok(j) {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
        None
    };
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let call = word(tok(i), "PERCENTILE_CONT") || word(tok(i), "PERCENTILE_DISC");
        let open = if call { next(i + 1).filter(|&j| *tok(j) == Token::LParen) } else { None };
        // `name ( p ) WITHIN GROUP ( ORDER BY x )`
        let parts = open.and_then(|a| {
            let a_end = close(a)?;
            let within = next(a_end + 1).filter(|&j| word(tok(j), "WITHIN"))?;
            let group = next(within + 1).filter(|&j| word(tok(j), "GROUP"))?;
            let g = next(group + 1).filter(|&j| *tok(j) == Token::LParen)?;
            let order = next(g + 1).filter(|&j| word(tok(j), "ORDER"))?;
            let by = next(order + 1).filter(|&j| word(tok(j), "BY"))?;
            let g_end = close(g)?;
            Some((a, a_end, by, g_end))
        });
        let Some((a, a_end, by, g_end)) = parts else {
            out.push(located[i].clone());
            i += 1;
            continue;
        };
        // The column, without a trailing ASC or NULLS FIRST or LAST (the
        // values leave NULLs out); DESC is refused.
        let trim = |mut e: usize| {
            while e > by + 1 && matches!(tok(e - 1), Token::Whitespace(_)) {
                e -= 1;
            }
            e
        };
        let mut x_end = trim(g_end);
        if x_end > by + 2 && (word(tok(x_end - 1), "FIRST") || word(tok(x_end - 1), "LAST")) {
            let nulls = trim(x_end - 1);
            if nulls > by + 1 && word(tok(nulls - 1), "NULLS") {
                x_end = trim(nulls - 1);
            }
        }
        if x_end > by + 1 && word(tok(x_end - 1), "DESC") {
            let name = tok(i).to_string().to_uppercase();
            return Err(Error::Query(format!("{name}(p) WITHIN GROUP (ORDER BY x DESC): percentiles order ascending only; write {name}(1 - p) WITHIN GROUP (ORDER BY x)")));
        }
        if x_end > by + 1 && word(tok(x_end - 1), "ASC") {
            x_end = trim(x_end - 1);
        }
        out.extend(located[i..=a].iter().cloned());
        out.extend(located[by + 1..x_end].iter().cloned());
        out.push(TokenWithSpan { token: Token::Comma, span: located[a].span });
        out.extend(located[a + 1..=a_end].iter().cloned());
        i = g_end + 1;
    }
    Ok(out)
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

/// An upper bound of the depth `tokens` can parse into: within each open
/// parenthesis (or bracket), the operators since its last comma, summed
/// over the open ones, plus how many are open. Commas separate list items,
/// which sit side by side, so `IN (1, 2, ...)` and `VALUES (...), (...)`
/// stay shallow however long they are. `UNION`, `INTERSECT` and `EXCEPT`
/// nest the queries they join as operators nest expressions, and every
/// step of planning one costs more stack than an operator's, so each
/// counts [`SET_OP_LEVELS`], and the commas of the queries it joins do not
/// take them back; the operators of the query before it do not count on.
fn check_depth(tokens: &[Token]) -> Result<()> {
    depth(tokens, MAX_EXPR_DEPTH).map(drop).map_err(|()| {
        Error::Query(format!(
            "the statement nests more than {MAX_EXPR_DEPTH} levels deep (long chains of AND, OR or arithmetic, deep parentheses, or more than {} queries joined by UNION, INTERSECT or EXCEPT); use IN lists or split it",
            MAX_EXPR_DEPTH / SET_OP_LEVELS
        ))
    })
}

/// The bound of [`check_depth`] on how deep `tokens` nest, or `Err` once
/// it passes `limit`.
fn depth(tokens: &[Token], limit: usize) -> std::result::Result<usize, ()> {
    // Per open parenthesis: the operators since its last comma, and the
    // set operators within it.
    let mut open: Vec<(usize, usize)> = vec![(0, 0)];
    let mut sum = 0usize;
    let mut most = 0usize;
    for t in tokens {
        match t {
            Token::LParen | Token::LBracket | Token::LBrace => open.push((0, 0)),
            Token::RParen | Token::RBracket | Token::RBrace => {
                if open.len() > 1 {
                    let (ops, sets) = open.pop().unwrap_or_default();
                    sum -= ops + sets;
                }
            }
            Token::Comma => {
                let last = &mut open.last_mut().expect("the statement level").0;
                sum -= *last;
                *last = 0;
            }
            Token::SemiColon => {
                let (ops, sets) = open.last_mut().expect("the statement level");
                sum -= *ops + *sets;
                (*ops, *sets) = (0, 0);
            }
            // The query before it is whole: its operators do not nest
            // with the next one's.
            Token::Word(w) if matches!(w.keyword, Keyword::UNION | Keyword::INTERSECT | Keyword::EXCEPT | Keyword::MINUS) => {
                let (ops, sets) = open.last_mut().expect("the statement level");
                sum -= *ops;
                *ops = 0;
                *sets += SET_OP_LEVELS;
                sum += SET_OP_LEVELS;
            }
            t if chains(t) => {
                open.last_mut().expect("the statement level").0 += 1;
                sum += 1;
            }
            _ => {}
        }
        most = most.max(sum + open.len());
        if most > limit {
            return Err(());
        }
    }
    Ok(most)
}

/// How deep a statement may nest ([`check_depth`]'s bound) to be planned
/// and run on the thread that asks for it, which may be a tokio worker's
/// or a blocking thread's of 2 MiB: [`deep_stack`] gives a deeper one a
/// thread of [`QUERY_STACK`]. The statements of each shape at this depth
/// are tested on a 2 MiB thread (`tests/deep_statements.rs`).
pub const INLINE_DEPTH: usize = 100;

/// The longest statement that cannot nest past [`INLINE_DEPTH`]: each
/// level takes a byte or more of text.
const INLINE_BYTES: usize = INLINE_DEPTH - 1;

/// Whether `sql` may nest deeper than [`INLINE_DEPTH`], by [`check_depth`]'s
/// bound, or may call a stored function: a call is written out after the
/// text is read ([`crate::query::function::expand`]), and may nest as
/// deep as a statement may ([`MAX_EXPR_DEPTH`]) however short its text.
/// A statement with a parenthesis or longer than [`INLINE_BYTES`] is
/// tokenized once more to tell. One that does not tokenize is not deep
/// (its parse fails on the thread that asks).
pub fn nests_deep(sql: &str) -> bool {
    if sql.len() <= INLINE_BYTES && !sql.contains('(') {
        return false;
    }
    match Tokenizer::new(&GenericDialect {}, sql).tokenize() {
        Ok(tokens) => may_call_stored(&tokens) || (sql.len() > INLINE_BYTES && depth(&tokens, INLINE_DEPTH).is_err()),
        Err(_) => false,
    }
}

/// Words a parenthesis follows that name no function: a stored
/// function's name may be any other word but a built-in one's.
const NOT_CALLS: &[Keyword] = &[
    Keyword::ALL, Keyword::AND, Keyword::ANY, Keyword::ARRAY, Keyword::AS, Keyword::BETWEEN, Keyword::BY, Keyword::CASE, Keyword::CAST, Keyword::CHAR, Keyword::CHECK, Keyword::CREATE, Keyword::DECIMAL,
    Keyword::DEFAULT, Keyword::ELSE, Keyword::EXCEPT, Keyword::EXISTS, Keyword::EXTRACT, Keyword::FILTER, Keyword::FROM, Keyword::FUNCTION, Keyword::GROUP, Keyword::HAVING, Keyword::IN, Keyword::INTERSECT,
    Keyword::INTERVAL, Keyword::INTO, Keyword::IS, Keyword::JOIN, Keyword::KEY, Keyword::LATERAL, Keyword::LIMIT, Keyword::NOT, Keyword::NUMERIC, Keyword::OFFSET, Keyword::ON, Keyword::OPTIONS, Keyword::OR,
    Keyword::ORDER, Keyword::OVER, Keyword::PARTITION, Keyword::PRIMARY, Keyword::RETURNS, Keyword::ROW, Keyword::SELECT, Keyword::SET, Keyword::TABLE, Keyword::THEN, Keyword::TRY_CAST, Keyword::UNION,
    Keyword::UNIQUE, Keyword::UNNEST, Keyword::USING, Keyword::VALUES, Keyword::VARCHAR, Keyword::WHEN, Keyword::WHERE, Keyword::WITH, Keyword::WITHIN,
];

/// Words after which a name followed by a parenthesis is a table's, with
/// its columns: `INSERT INTO t (a, b)`, `CREATE TABLE t (...)`.
const NAMES_TABLE: &[Keyword] = &[Keyword::INTO, Keyword::TABLE, Keyword::EXISTS, Keyword::INDEX, Keyword::ON, Keyword::REFERENCES, Keyword::FUNCTION];

/// Whether `tokens` call a function that may be a stored one: a word,
/// not a built-in function's name nor one of [`NOT_CALLS`], followed by
/// a parenthesis, and not a table's name before its columns.
fn may_call_stored(tokens: &[Token]) -> bool {
    let mut words = tokens.iter().filter(|t| !matches!(t, Token::Whitespace(_))).peekable();
    let mut before: Option<&Token> = None;
    while let Some(t) = words.next() {
        let prev = before.replace(t);
        let Token::Word(w) = t else { continue };
        if w.quote_style.is_some() || !matches!(words.peek(), Some(Token::LParen)) || NOT_CALLS.contains(&w.keyword) {
            continue;
        }
        if matches!(prev, Some(Token::Word(p)) if NAMES_TABLE.contains(&p.keyword)) {
            continue;
        }
        let name = w.value.to_ascii_lowercase();
        if !name.starts_with("__") && !crate::query::function::RESERVED.contains(&name.as_str()) {
            return true;
        }
    }
    false
}

thread_local! {
    /// Whether this thread has [`QUERY_STACK`] or more (a thread of
    /// [`deep_stack`], or of the query pool).
    static DEEP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this thread has at least [`QUERY_STACK`] of stack
/// ([`mark_deep_stack`]).
pub fn on_deep_stack() -> bool {
    DEEP.with(std::cell::Cell::get)
}

/// Marks this thread as one with at least [`QUERY_STACK`] of stack, on
/// which [`deep_stack`] runs its work itself.
pub fn mark_deep_stack() {
    DEEP.with(|d| d.set(true));
}

/// `f`, on a thread of [`QUERY_STACK`] when `deep` and this thread is not
/// one (its caller waits), else here. Planning, binding, running and
/// dropping a statement recurse once per level it nests, so a statement
/// that nests deeper than [`INLINE_DEPTH`] ([`nests_deep`]) runs so, on
/// whatever thread it was asked from.
pub fn deep_stack<T: Send>(deep: bool, f: impl FnOnce() -> T + Send) -> T {
    if !deep || on_deep_stack() {
        return f();
    }
    std::thread::scope(|s| {
        let h = std::thread::Builder::new()
            .name("chloedb-deep".into())
            .stack_size(QUERY_STACK)
            .spawn_scoped(s, || {
                mark_deep_stack();
                f()
            })
            .expect("start a thread for a deep statement");
        h.join().unwrap_or_else(|p| std::panic::resume_unwind(p))
    })
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
    fn an_infix_match_says_how_chloedb_writes_it() {
        let e = parse("SELECT id FROM t WHERE body MATCH 'red mug' LIMIT 3").unwrap_err().to_string();
        assert!(e.contains("MATCH(body, 'red mug')") && e.contains("bm25(body, 'red mug')"), "{e}");
        let e = parse("SELECT id FROM t WHERE title MATCH :q").unwrap_err().to_string();
        assert!(e.contains("MATCH(title, :q)"), "{e}");
        let hint = |sql: &str, rows, zero| crate::query::hint::of_answer(sql, rows, zero);
        assert!(hint("SELECT id FROM t WHERE MATCH(body, 'red mug cup')", 0, false).is_some_and(|h| h.contains("'red OR mug OR cup'")));
        assert_eq!(hint("SELECT id FROM t WHERE MATCH(body, 'red OR mug')", 0, false), None);
        assert_eq!(hint("SELECT id FROM t WHERE MATCH(body, 'red')", 0, false), None);
        assert_eq!(hint("SELECT id FROM t WHERE MATCH(body, 'red mug')", 3, false), None);
        assert!(hint("SELECT id, score() FROM t WHERE MATCH(body, 'red')", 3, true).is_some_and(|h| h.contains("ORDER BY bm25")));
        assert_eq!(hint("SELECT id, score() FROM t WHERE MATCH(body, 'red') ORDER BY bm25(body, 'red') DESC", 3, true), None);
    }

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
    fn within_group_parses_as_the_column_and_fraction() {
        let one = |q: &str| parse(q).unwrap().remove(0).to_string();
        assert_eq!(one("SELECT percentile_cont(0.25) WITHIN GROUP (ORDER BY price) FROM t"), "SELECT percentile_cont(price, 0.25) FROM t");
        assert_eq!(one("SELECT PERCENTILE_DISC((0.5)) within group (order by t.price ASC) AS p FROM t GROUP BY item"), "SELECT PERCENTILE_DISC(t.price, (0.5)) AS p FROM t GROUP BY item");
        assert_eq!(one("SELECT percentile_cont(:p) WITHIN GROUP (ORDER BY price * 2 ASC NULLS FIRST) FROM t"), "SELECT percentile_cont(price * 2, :p) FROM t");
        // The two-argument call, and a column named so, stay as written.
        assert_eq!(one("SELECT percentile_disc(price, 0.5), percentile_cont FROM t"), "SELECT percentile_disc(price, 0.5), percentile_cont FROM t");
        let e = parse("SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY price DESC) FROM t").unwrap_err().to_string();
        assert!(e.contains("ascending only"), "{e}");
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
        // Each set operator counts SET_OP_LEVELS, its queries' commas and
        // operators apart; within parentheses as without.
        let most = (MAX_EXPR_DEPTH - 1) / SET_OP_LEVELS + 1;
        for (n, ok) in [(most, true), (most + 1, false)] {
            for q in ["SELECT a, b FROM t WHERE a = 1 AND b > 2", "SELECT 1"] {
                let chain = vec![q; n].join(" UNION ALL ");
                for sql in [chain.clone(), format!("SELECT id FROM t WHERE id IN ({chain})"), vec![q; n].join(" INTERSECT "), vec![q; n].join(" EXCEPT ")] {
                    let r = parse(&sql);
                    assert_eq!(r.is_ok(), ok, "{n} of {q}: {:?}", r.err());
                    if !ok {
                        assert!(matches!(&r, Err(Error::Query(m)) if m.contains("nests more than") && m.contains("UNION")), "{r:?}");
                    }
                }
            }
        }
        // Statements apart count apart.
        assert!(parse(&format!("{0}; {0}", vec!["SELECT 1"; most].join(" UNION ALL "))).is_ok());
    }
}
