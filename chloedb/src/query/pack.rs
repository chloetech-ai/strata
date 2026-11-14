//! `PACK n TOKENS`: a query's answer as the context a language model is
//! given, filled to a budget of tokens rather than cut to k rows.
//!
//! ```sql
//! SELECT id, kind, text FROM memory
//! WHERE superseded = false
//! ORDER BY rrf(bm25(text, :task), similarity(emb, :q)) DESC
//! PACK 4000 TOKENS DEDUPE 0.85 FOLLOW links 1 HOP WITH RECEIPT
//! ```
//!
//! The rows are read in the query's order, in growing reads (the first 64,
//! then the first 320, 1,344 ..., up to the query's `LIMIT`, 1,000 without
//! one), each from the query's `OFFSET`, and each candidate is decided once,
//! in order:
//!
//! - **take** it whole when its text fits what is left of the budget;
//! - **excerpt** it when it is longer than a row may be (`EXCERPT n`, a
//!   quarter of the budget by default) or than what is left: the stretch of
//!   its longest text column that holds the most terms of the query's
//!   `bm25` and `MATCH` texts, as the column's analyzer makes terms, cut to
//!   fit (between words, or inside a run of text without spaces);
//! - **skip: near-duplicate of k** when `DEDUPE x` is given and the row
//!   says what a row taken before says: most of the shorter one's words are
//!   in the other (the overlap of their word sets is at least x), or, with
//!   `DEDUPE x ON emb`, the cosine of their vectors is at least x;
//! - **skip: over budget** when not even an excerpt fits;
//! - **skip: no text** when the row has no text to count.
//!
//! With `FOLLOW edges n HOP`, each row taken brings the rows its `EDGES`
//! field reaches within n hops (one by default) right after it, if they pass
//! the query's `WHERE`, decided the same way (**linked from k**). They are
//! read by key, a hop a query, as a walk through a coordinator reads them. Reading
//! stops once what is left of the budget is too small for any row, or once
//! 16 rows in a row were turned away for the budget (what is left is too
//! small for the rows this query finds), so a query whose first rows fill
//! the budget reads one slice.
//!
//! The answer is the rows taken, in that order, with two more columns:
//! `pack_tokens`, the tokens the row spends, and `pack_decision`. `WITH
//! RECEIPT` adds the rows skipped, their text columns `NULL` (so that the
//! rows' text still fits the budget), and the columns `pack_score` (the
//! query's score of the row) and `pack_used` (the tokens spent up to and
//! with the row). [`Packed::receipt`] says the same, with the snapshot the
//! rows were read from.
//!
//! Each read starts at the query's `OFFSET` and reads more rows than the
//! one before, rather than the rows after it: a fused ranking (`rrf`, a
//! weighted sum) ranks its candidates to a depth that grows with the
//! `LIMIT`, so a deeper read is a better ranking of the same rows, not the
//! next page of the last, and a read that comes back short is the end of
//! the ranking. Rows seen before are skipped by their keys (in a table
//! without a primary key, the reads are pages of the query instead).
//!
//! Tokens are counted by [`count_tokens`], an estimate: a tokenizer's count
//! depends on the model. Measured against two (`chloedb/results/pack/`), it
//! counts English prose and code 13 to 27% high, the safe side.
//!
//! The clause is taken off the statement's text before it is parsed, and the
//! rest runs as any query does, once per read (and once per row followed),
//! so PACK works wherever a query does: on a file, a server or a
//! coordinator over shards. The reads are given as parsed queries
//! ([`Next`]), not as text; where they must travel as text, [`Next::sql`]
//! prints them and refuses one that does not read back the same. Nothing
//! about it is stored.
//!
//! `PACK` is experimental: what it reads and how it chooses may change.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use sqlparser::ast as sql;
use sqlparser::dialect::GenericDialect;
use sqlparser::tokenizer::{Location, Token, Tokenizer};

use crate::error::{Error, Result};
use crate::query::exec::{ExecStats, ResultSet};
use crate::query::sql::Params;
use crate::schema::Schema;
use crate::text::Analyzer;
use crate::value::Value;

/// The rows read without a `LIMIT`.
pub const DEFAULT_CANDIDATES: usize = 1000;
/// The rows of the first slice.
const FIRST_SLICE: usize = 64;
/// The rows a row followed brings at most.
const FOLLOWED: usize = 16;
/// The fewest tokens an excerpt is made of: below this, a row that does not
/// fit is skipped.
const MIN_EXCERPT: usize = 32;
/// The largest budget.
const MAX_BUDGET: usize = 10_000_000;
/// The rows turned away for the budget in a row after which no more are
/// read: what is left is too small for the rows this query finds (a title
/// and a description need room for both and an excerpt), and a deeper read
/// would turn its rows away too, at the cost of ranking up to 1,000 of them
/// (PRF-11). The rows already read are still decided.
const TURNED_AWAY: usize = 16;
/// The most tokens one piece of an excerpt holds: a run of text without
/// spaces longer than this is cut between its characters.
const MAX_PIECE: usize = 8;
/// The parameter that holds the keys a FOLLOW query reads.
const FOLLOW_KEYS: &str = "__pack_follow_keys";

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// An estimate of the tokens a language model's tokenizer makes of `text`.
///
/// A run of Latin letters is one token per six letters or part of them
/// (most English words are one; `idempotency` is two), a run of letters
/// of another alphabet (Cyrillic, Greek, Arabic, Hebrew, Devanagari and the
/// other scripts of India) one per three, a run of digits one per three, a
/// letter of a script written without spaces (CJK, Thai) one each, and any
/// other character but whitespace one each. Whitespace joins the token
/// after it, as BPE tokenizers make " word" one token.
///
/// Measured against cl100k_base and o200k_base (`chloedb/results/pack/`):
/// English prose and Rust code count 13 to 27% more than either says, the
/// safe side for a budget; Chinese, Japanese, Korean, Arabic, Hindi and
/// Russian count 9 to 27% more than o200k_base says, but cl100k_base makes
/// up to 2.4 times more of them (Hindi) than this says.
pub fn count_tokens(text: &str) -> usize {
    let mut n = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c.is_ascii_digit() {
            let mut len = 1usize;
            while chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                chars.next();
                len += 1;
            }
            n += len.div_ceil(3);
        } else if c.is_alphabetic() && !spaceless(c) {
            let latin = is_latin(c);
            let mut len = 1usize;
            while chars.peek().is_some_and(|d| d.is_alphabetic() && !spaceless(*d) && is_latin(*d) == latin) {
                chars.next();
                len += 1;
            }
            n += len.div_ceil(if latin { 6 } else { 3 });
        } else {
            n += 1;
        }
    }
    n
}

/// Whether letter `c` is Latin (with its accented and extended forms),
/// which BPE tokenizers learned the most of and make the longest tokens of.
fn is_latin(c: char) -> bool {
    matches!(c as u32, 0..=0x024f | 0x1e00..=0x1eff | 0x2c60..=0x2c7f | 0xa720..=0xa7ff | 0xff21..=0xff5a)
}

/// Whether `c` is of a script written without spaces between words, whose
/// letters BPE tokenizers make a token or more each.
fn spaceless(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff | 0x0e00..=0x0e7f)
}

/// What a `PACK` clause asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    /// `PACK n TOKENS`: the budget, 0 until a parameter's value is bound
    /// ([`Spec::budget_param`]).
    pub budget: usize,
    /// `PACK :n TOKENS`: the parameter that holds the budget.
    pub budget_param: Option<String>,
    /// `OF a, b`: the columns whose text is counted (all text columns but
    /// the key's without it).
    pub of: Vec<String>,
    /// `EXCERPT n`: the most tokens one row may spend.
    pub excerpt: Option<usize>,
    /// `DEDUPE x [ON column]`.
    pub dedupe: Option<(f64, Option<String>)>,
    /// `FOLLOW field [n HOP]`.
    pub follow: Option<(String, usize)>,
    /// `WITH RECEIPT`.
    pub receipt: bool,
}

impl Spec {
    /// The most tokens one row may spend.
    pub fn row_cap(&self) -> usize {
        self.excerpt.unwrap_or((self.budget / 4).max(MIN_EXCERPT)).min(self.budget)
    }
}

/// Whether `sql_text` may hold a `PACK` clause: a cheap test before
/// [`split`] tokenizes it.
pub fn may_pack(sql_text: &str) -> bool {
    sql_text.as_bytes().windows(4).any(|w| w.eq_ignore_ascii_case(b"pack"))
}

/// The statement without its `PACK` clause, and the clause; `None` when it
/// has none. The clause is `PACK n TOKENS` and its options, outside any
/// parenthesis, and runs to the statement's end or its `LIMIT` or `OFFSET`.
pub fn split(sql_text: &str) -> Result<Option<(String, Spec)>> {
    if !may_pack(sql_text) {
        return Ok(None);
    }
    let Ok(tokens) = Tokenizer::new(&GenericDialect {}, sql_text).tokenize_with_location() else { return Ok(None) };
    let word = |t: &Token, w: &str| matches!(t, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    let solid: Vec<usize> = (0..tokens.len()).filter(|&i| !matches!(tokens[i].token, Token::Whitespace(_))).collect();
    let mut depth = 0usize;
    let mut start = None;
    for (n, &i) in solid.iter().enumerate() {
        match &tokens[i].token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            t if depth == 0 && word(t, "PACK") => {
                let after: Vec<&Token> = solid[n + 1..].iter().take(3).map(|&j| &tokens[j].token).collect();
                // `PACK 4000 TOKENS`, `PACK ? TOKENS` or `PACK :n TOKENS`
                // (a colon, then a word).
                let budget = match after[..] {
                    [Token::Number(..) | Token::Placeholder(_), t, ..] => word(t, "TOKENS"),
                    [Token::Colon, Token::Word(_), t] => word(t, "TOKENS"),
                    _ => false,
                };
                if budget {
                    start = Some(n);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(start) = start else { return Ok(None) };
    // The clause's tokens, to LIMIT, OFFSET, `;` or the end.
    let mut end = start;
    while end < solid.len() {
        let t = &tokens[solid[end]].token;
        if end > start && (word(t, "LIMIT") || word(t, "OFFSET") || *t == Token::SemiColon) {
            break;
        }
        end += 1;
    }
    let mut clause: Vec<Token> = solid[start..end].iter().map(|&i| tokens[i].token.clone()).collect();
    if let [_, Token::Colon, Token::Word(w), ..] = &clause[..] {
        let p = Token::Placeholder(format!(":{}", w.value));
        clause.splice(1..3, [p]);
    }
    let clause: Vec<&Token> = clause.iter().collect();
    let spec = parse_clause(&clause)?;
    let from = offset_of(sql_text, tokens[solid[start]].span.start);
    let to = match solid.get(end) {
        Some(&i) => offset_of(sql_text, tokens[i].span.start),
        None => sql_text.len(),
    };
    let mut rest = String::with_capacity(sql_text.len());
    rest.push_str(sql_text[..from].trim_end());
    rest.push(' ');
    rest.push_str(&sql_text[to..]);
    Ok(Some((rest.trim().to_string(), spec)))
}

/// The byte offset of a token's location in `text` (lines and columns from
/// 1, columns counted in characters).
fn offset_of(text: &str, at: Location) -> usize {
    let mut line = 1;
    let mut col = 1;
    for (i, c) in text.char_indices() {
        if line == at.line && col == at.column {
            return i;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    text.len()
}

/// A placeholder's parameter name (`:n`, `$n`, `@n`, `?n`).
fn param_name(p: &str) -> &str {
    p.trim_start_matches([':', '$', '@', '?'])
}

/// A budget given as a parameter: a whole number, 1 to [`MAX_BUDGET`].
fn bound_budget(name: &str, params: &Params) -> Result<usize> {
    match params.0.get(name) {
        Some(Value::Int(n)) if *n >= 1 && *n as u64 <= MAX_BUDGET as u64 => Ok(*n as usize),
        Some(Value::Float(x)) if *x >= 1.0 && x.fract() == 0.0 && *x <= MAX_BUDGET as f64 => Ok(*x as usize),
        Some(v) => Err(err(format!("PACK :{name} TOKENS takes a whole number, 1 or more (at most {MAX_BUDGET}), not {}", super::hint::sql_value(v)))),
        None => Err(err(format!("PACK :{name} TOKENS: no parameter {name} was given"))),
    }
}

/// `PACK n TOKENS [OF a, b] [EXCERPT n] [DEDUPE x [ON c]] [FOLLOW f [n
/// HOP]] [WITH RECEIPT]`, the options in any order.
fn parse_clause(t: &[&Token]) -> Result<Spec> {
    const SHAPE: &str = "PACK n TOKENS [OF column, ...] [EXCERPT n] [DEDUPE x [ON column]] [FOLLOW field [n HOP]] [WITH RECEIPT]";
    let word = |i: usize, w: &str| matches!(t.get(i), Some(Token::Word(x)) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    let name = |i: usize| match t.get(i) {
        Some(Token::Word(x)) => Some(x.value.clone()),
        _ => None,
    };
    let number = |i: usize| match t.get(i) {
        Some(Token::Number(s, _)) => s.parse::<f64>().ok(),
        _ => None,
    };
    let whole = |i: usize, what: &str| -> Result<usize> {
        match number(i) {
            Some(x) if x >= 1.0 && x.fract() == 0.0 && x <= MAX_BUDGET as f64 => Ok(x as usize),
            _ => Err(err(format!("PACK: {what} takes a whole number, 1 or more (at most {MAX_BUDGET})"))),
        }
    };
    let (budget, budget_param) = match t.get(1) {
        Some(Token::Placeholder(p)) => (0, Some(param_name(p).to_string())),
        _ => (whole(1, "PACK n TOKENS")?, None),
    };
    let mut spec = Spec { budget, budget_param, of: Vec::new(), excerpt: None, dedupe: None, follow: None, receipt: false };
    let mut i = 3;
    while i < t.len() {
        if word(i, "OF") && spec.of.is_empty() {
            i += 1;
            loop {
                spec.of.push(name(i).ok_or_else(|| err("PACK ... OF takes the names of the columns whose text it counts"))?);
                i += 1;
                if t.get(i) != Some(&&Token::Comma) {
                    break;
                }
                i += 1;
            }
        } else if word(i, "EXCERPT") && spec.excerpt.is_none() {
            spec.excerpt = Some(whole(i + 1, "EXCERPT n")?.max(MIN_EXCERPT));
            i += 2;
        } else if word(i, "DEDUPE") && spec.dedupe.is_none() {
            let x = number(i + 1).filter(|x| *x > 0.0 && *x <= 1.0).ok_or_else(|| err("PACK ... DEDUPE x takes a share above 0 and at most 1 (0.85: rows of which 85% says what a row taken says)"))?;
            i += 2;
            let on = match word(i, "ON") {
                true => {
                    i += 2;
                    Some(name(i - 1).ok_or_else(|| err("PACK ... DEDUPE x ON takes a VECTOR column"))?)
                }
                false => None,
            };
            spec.dedupe = Some((x, on));
        } else if word(i, "FOLLOW") && spec.follow.is_none() {
            let field = name(i + 1).ok_or_else(|| err("PACK ... FOLLOW takes an EDGES field"))?;
            i += 2;
            let mut hops = 1;
            if number(i).is_some() && (word(i + 1, "HOP") || word(i + 1, "HOPS")) {
                hops = whole(i, "FOLLOW field n HOP")?;
                if hops > 3 {
                    return Err(err("PACK ... FOLLOW field n HOP follows at most 3 hops"));
                }
                i += 2;
            }
            spec.follow = Some((field, hops));
        } else if word(i, "WITH") && word(i + 1, "RECEIPT") && !spec.receipt {
            spec.receipt = true;
            i += 2;
        } else {
            let at = t.get(i).map(|x| x.to_string()).unwrap_or_default();
            return Err(err(format!("PACK: {at} is not an option here; the clause is {SHAPE}, and LIMIT or OFFSET follow it")));
        }
    }
    Ok(spec)
}

/// One candidate, as decided.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    /// The row's primary key (`Null` in a table without one).
    pub key: Value,
    /// The query's score of the row.
    pub score: f64,
    /// The tokens the row spends: of its excerpt when excerpted, 0 when
    /// skipped.
    pub tokens: usize,
    /// The tokens of the whole row.
    pub full_tokens: usize,
    /// The tokens spent up to and with this row.
    pub used: usize,
    /// `take`, `excerpt`, `linked from k`, `skip: near-duplicate of k`,
    /// `skip: over budget` or `skip: no text`.
    pub decision: String,
    pub taken: bool,
}

/// What [`Packer::finish`] answers: the rows, and how they were chosen.
#[derive(Clone, Debug)]
pub struct Packed {
    /// The rows taken (and with `WITH RECEIPT` the rows skipped, their text
    /// `NULL`), with the pack's columns.
    pub rs: ResultSet,
    pub receipt: Receipt,
}

/// How a pack was made.
#[derive(Clone, Debug, PartialEq)]
pub struct Receipt {
    /// The snapshot's transaction ([`crate::storage::Snapshot::txn`]) and
    /// write number ([`crate::storage::Snapshot::seq_no`]); `None` where
    /// they are not known (a coordinator's answer, whose shards each read
    /// their own).
    pub txn: Option<u64>,
    pub seq_no: Option<u64>,
    pub budget: usize,
    pub used: usize,
    /// Every candidate decided, in the order it was decided.
    pub rows: Vec<Decision>,
    /// The queries run to read the candidates.
    pub queries: usize,
}

/// A query the packer reads its candidates with ([`Packer::next_query`]):
/// parsed, with the parameters it runs with (the query's, and the keys a
/// FOLLOW reads).
#[derive(Clone, Debug)]
pub struct Next {
    pub query: sql::Query,
    pub params: Params,
}

impl Next {
    /// The query as text, for a runner that takes text (a server's): an
    /// error when the text does not read back as the same query, as
    /// sqlparser prints `- -1` as `--1`, a comment.
    pub fn sql(&self) -> Result<String> {
        let text = self.query.to_string();
        match super::sql::parse_query(&text) {
            Ok(q) if *q == self.query => Ok(text),
            _ => Err(err(format!("PACK: the query without its clause does not print as SQL that reads back the same ({text}): write it another way (a doubled minus, `- -1`, prints as a comment)"))),
        }
    }
}

/// What the query that is running reads.
enum Pending {
    /// `asked` rows, which read the candidates up to row `to` (from the
    /// first row, not from the query's OFFSET).
    Slice { asked: usize, to: usize },
    /// The rows of `targets`, linked from the row labelled `from`, `hop`
    /// hops away.
    Follow { from: String, targets: Vec<Value>, hop: usize },
}

/// Rows to follow to: the keys an `EDGES` value names.
struct Follow {
    from: String,
    targets: Vec<Value>,
    hop: usize,
}

/// A candidate waiting for its decision.
struct Candidate {
    row: Vec<Value>,
    key: Value,
    score: f64,
    /// The keys its `EDGES` field links to, with FOLLOW.
    edges: Vec<Value>,
}

/// What a taken row is compared with for `DEDUPE`.
struct Kept {
    label: String,
    words: HashSet<String>,
    vector: Option<Vec<f32>>,
}

/// Runs a `PACK` query: ask it for the next query to run
/// ([`Packer::next_query`]), give it that query's answer ([`Packer::feed`]),
/// until it has none, then [`Packer::finish`].
pub struct Packer {
    spec: Spec,
    /// The query without its `PACK` clause, `LIMIT` and `OFFSET`.
    query: sql::Query,
    params: Params,
    /// The columns added after the query's: the VECTOR column `DEDUPE x
    /// ON` reads, then the EDGES field FOLLOW reads.
    hidden: usize,
    vector_at: Option<usize>,
    edges_at: Option<usize>,
    /// The table the query reads, when it reads one.
    table: Option<Arc<Schema>>,
    /// The primary key's column, which FOLLOW reads rows by.
    key_column: Option<String>,
    /// The texts the query searches by, with the fields they search: the
    /// second argument of each `bm25` and `MATCH`.
    searches: Vec<(Option<String>, String)>,
    /// Their terms, as each analyzer of the answer's columns makes them.
    terms: HashMap<Analyzer, HashSet<String>>,
    /// Each column's analyzer: its field's, else the first searched
    /// field's, else `english`.
    analyzers: Vec<Analyzer>,
    /// The query's OFFSET, where every read starts.
    start: usize,
    /// The candidates read so far end at row `read_to`.
    read_to: usize,
    end: usize,
    slice: usize,
    /// Whether each read is the query's first rows again, deeper (its rows
    /// have keys: the ones seen are skipped), or the next page.
    reread: Option<bool>,
    exhausted: bool,
    /// The rows skipped over budget since the last row taken.
    turned_away: usize,
    pending: Option<Pending>,
    buffer: VecDeque<Candidate>,
    follows: VecDeque<Follow>,
    seen: HashSet<String>,
    columns: Option<Vec<String>>,
    /// The columns whose text is counted, and those that hold the row's
    /// key, which are not.
    text_cols: Vec<usize>,
    key_cols: Vec<usize>,
    kept: Vec<Kept>,
    used: usize,
    out_rows: Vec<Vec<Value>>,
    out_keys: Vec<Value>,
    out_scores: Vec<f64>,
    decisions: Vec<Decision>,
    queries: usize,
    stats: Option<ExecStats>,
}

impl Packer {
    /// The packer of `sql_text`, `None` when it has no `PACK` clause.
    /// `table_of` gives a table's schema, by which FOLLOW reads the rows
    /// linked to by their primary key and the text is analyzed as its
    /// fields are.
    pub fn of_sql(sql_text: &str, params: &Params, table_of: &dyn Fn(&str) -> Option<Arc<Schema>>) -> Result<Option<Packer>> {
        let Some((rest, spec)) = split(sql_text)? else { return Ok(None) };
        let q = *super::sql::parse_query(&rest)?;
        Packer::new(q, spec, params, table_of).map(Some)
    }

    /// The packer of query `q` (without its `PACK` clause) by `spec`.
    pub fn new(mut q: sql::Query, mut spec: Spec, params: &Params, table_of: &dyn Fn(&str) -> Option<Arc<Schema>>) -> Result<Packer> {
        if let Some(name) = &spec.budget_param {
            spec.budget = bound_budget(name, params)?;
        }
        let number = |e: &sql::Expr, what: &str| -> Result<usize> {
            let v = match e {
                sql::Expr::Value(v) => match &v.value {
                    sql::Value::Number(s, _) => s.parse::<i64>().ok().map(Value::Int),
                    sql::Value::Placeholder(p) => params.0.get(param_name(p)).cloned(),
                    _ => None,
                },
                _ => None,
            };
            match v {
                Some(Value::Int(n)) if n >= 0 => Ok(n as usize),
                _ => Err(err(format!("PACK: {what} takes a whole number of rows"))),
            }
        };
        let (limit, offset) = match q.limit_clause.take() {
            None => (None, 0),
            Some(sql::LimitClause::LimitOffset { limit, offset, limit_by }) if limit_by.is_empty() => {
                (limit.as_ref().map(|l| number(l, "LIMIT")).transpose()?, offset.as_ref().map(|o| number(&o.value, "OFFSET")).transpose()?.unwrap_or(0))
            }
            Some(sql::LimitClause::OffsetCommaLimit { offset, limit }) => (Some(number(&limit, "LIMIT")?), number(&offset, "OFFSET")?),
            Some(_) => return Err(err("PACK: LIMIT BY is not supported")),
        };
        if q.fetch.is_some() {
            return Err(err("PACK reads the rows up to LIMIT n: not with FETCH"));
        }
        let candidates = limit.unwrap_or(DEFAULT_CANDIDATES);
        let table = match q.body.as_ref() {
            sql::SetExpr::Select(s) => match &s.from[..] {
                [t] if t.joins.is_empty() => match &t.relation {
                    sql::TableFactor::Table { name, args: None, .. } => Some(name.to_string()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        let schema = table.as_deref().and_then(table_of);
        let (mut hidden, mut vector_at, mut edges_at, mut key_column) = (0, None, None, None);
        if let Some((_, Some(on))) = &spec.dedupe {
            let sql::SetExpr::Select(s) = q.body.as_mut() else { return Err(err("PACK ... DEDUPE x ON column reads the column from a SELECT of one table")) };
            s.projection.push(sql::SelectItem::ExprWithAlias { expr: sql::Expr::Identifier(sql::Ident::new(on)), alias: sql::Ident::new("__pack_vector") });
            vector_at = Some(hidden);
            hidden += 1;
        }
        if let Some((field, _)) = &spec.follow {
            let table = table.as_deref().ok_or_else(|| err("PACK ... FOLLOW follows the edges of a SELECT of one table"))?;
            let sql::SetExpr::Select(s) = q.body.as_mut() else { return Err(err("PACK ... FOLLOW follows the edges of a SELECT of one table")) };
            let key = schema.as_ref().and_then(|t| t.primary_key.map(|k| t.fields[k].name.clone()));
            key_column = Some(key.ok_or_else(|| err(format!("PACK ... FOLLOW reads the rows linked to by their keys: table {table} needs a PRIMARY KEY")))?);
            s.projection.push(sql::SelectItem::ExprWithAlias { expr: sql::Expr::Identifier(sql::Ident::new(field)), alias: sql::Ident::new("__pack_edges") });
            edges_at = Some(hidden);
            hidden += 1;
        }
        let searches = search_texts(&q, params);
        Ok(Packer {
            spec,
            query: q,
            params: params.clone(),
            hidden,
            vector_at,
            edges_at,
            table: schema,
            key_column,
            searches,
            terms: HashMap::new(),
            analyzers: Vec::new(),
            start: offset,
            read_to: offset,
            end: offset.saturating_add(candidates),
            slice: FIRST_SLICE,
            reread: None,
            exhausted: false,
            turned_away: 0,
            pending: None,
            buffer: VecDeque::new(),
            follows: VecDeque::new(),
            seen: HashSet::new(),
            columns: None,
            text_cols: Vec::new(),
            key_cols: Vec::new(),
            kept: Vec::new(),
            used: 0,
            out_rows: Vec::new(),
            out_keys: Vec::new(),
            out_scores: Vec::new(),
            decisions: Vec::new(),
            queries: 0,
            stats: None,
        })
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Whether what is left of the budget is too small for any row.
    fn full(&self) -> bool {
        self.spec.budget.saturating_sub(self.used) < MIN_EXCERPT.min(self.spec.budget)
    }

    /// The next query to run, `None` when the pack is made.
    pub fn next_query(&mut self) -> Result<Option<Next>> {
        if self.pending.is_some() {
            return Err(err("PACK: the answer of the last query was not given"));
        }
        loop {
            self.drive()?;
            if self.full() {
                return Ok(None);
            }
            let Some(Follow { from, targets, hop }) = self.follows.pop_front() else { break };
            let targets: Vec<Value> = targets.into_iter().filter(|k| !self.seen.contains(&key_label(k, 0))).take(FOLLOWED).collect();
            if targets.is_empty() {
                continue;
            }
            let key = self.key_column.clone().expect("FOLLOW has a key column");
            let mut q = self.query.clone();
            let sql::SetExpr::Select(s) = q.body.as_mut() else { unreachable!("checked in new") };
            // By key, as a walk through a coordinator reads a hop: the
            // query's WHERE still holds. The keys are a parameter, so that
            // the query's text is the same for every row followed.
            let by_key = format!("{} IN (:{FOLLOW_KEYS})", sql::Ident::with_quote('"', &key));
            let by_key = sqlparser::parser::Parser::new(&GenericDialect {}).try_with_sql(&by_key).and_then(|mut p| p.parse_expr()).map_err(|e| err(format!("PACK ... FOLLOW: {e}")))?;
            s.selection = Some(match s.selection.take() {
                Some(w) => sql::Expr::BinaryOp { left: Box::new(sql::Expr::Nested(Box::new(w))), op: sql::BinaryOperator::And, right: Box::new(by_key) },
                None => by_key,
            });
            // Not in the query's order: a ranking by bm25 would keep only
            // the linked rows that match its words, and the link is the
            // reason to take them. They come in the order of the edges.
            q.order_by = None;
            q.limit_clause = Some(limit_offset(FOLLOWED, 0));
            let mut params = self.params.clone();
            params.0.insert(FOLLOW_KEYS.to_string(), Value::List(targets.clone()));
            self.pending = Some(Pending::Follow { from, targets, hop });
            self.queries += 1;
            return Ok(Some(Next { query: q, params }));
        }
        let first = self.queries == 0;
        if !first && (self.exhausted || self.read_to >= self.end || self.turned_away >= TURNED_AWAY) {
            return Ok(None);
        }
        // `LIMIT 0` still reads once, for the answer's columns.
        let n = self.slice.min(self.end - self.read_to);
        let (from, asked) = match self.reread {
            Some(true) => (self.start, self.read_to - self.start + n),
            _ => (self.read_to, n),
        };
        let mut q = self.query.clone();
        q.limit_clause = Some(limit_offset(asked, from));
        self.pending = Some(Pending::Slice { asked, to: self.read_to + n });
        self.queries += 1;
        Ok(Some(Next { query: q, params: self.params.clone() }))
    }

    /// Gives the packer the answer of the query [`Packer::next_query`] gave.
    pub fn feed(&mut self, rs: ResultSet) -> Result<()> {
        let pending = self.pending.take().ok_or_else(|| err("PACK: an answer no query was asked for"))?;
        self.columns_of(&rs)?;
        if self.stats.is_none() {
            self.stats = Some(rs.stats.clone());
        }
        let n_rows = rs.rows.len();
        let mut cands: Vec<Candidate> = Vec::with_capacity(n_rows);
        let int_keys = rs.keys.iter().any(|k| matches!(k, Value::Int(_)));
        let keyed = n_rows > 0 && rs.keys.len() == n_rows && rs.keys.iter().all(|k| !matches!(k, Value::Null));
        for (i, mut row) in rs.rows.into_iter().enumerate() {
            let key = rs.keys.get(i).cloned().unwrap_or(Value::Null);
            let score = rs.scores.get(i).copied().unwrap_or(0.0);
            let shown = row.len().saturating_sub(self.hidden);
            let extra = row.split_off(shown);
            let mut edges = Vec::new();
            if let Some(at) = self.edges_at {
                let v = extra.get(at).cloned().unwrap_or(Value::Null);
                edges = crate::graph::parse_edges(&v, int_keys)?.into_iter().map(|e| e.to).collect();
            }
            if let Some(at) = self.vector_at {
                row.push(extra.get(at).cloned().unwrap_or(Value::Null));
            }
            cands.push(Candidate { row, key, score, edges });
        }
        match pending {
            Pending::Slice { asked, to } => {
                self.read_to = to;
                // A short read is the end of the ranking: a deeper read
                // ranks the candidates to a depth of its LIMIT at least.
                if n_rows < asked || asked == 0 {
                    self.exhausted = true;
                }
                if self.reread.is_none() && n_rows > 0 {
                    self.reread = Some(keyed);
                }
                self.slice = self.slice.saturating_mul(4);
                self.buffer.extend(cands);
            }
            Pending::Follow { from, targets, hop } => {
                let place = |k: &Value| targets.iter().position(|t| t == k).unwrap_or(usize::MAX);
                cands.sort_by_key(|c| place(&c.key));
                let hops = self.spec.follow.as_ref().map_or(1, |f| f.1);
                let mut next: Vec<Value> = Vec::new();
                for c in cands {
                    if self.full() {
                        break;
                    }
                    let edges = c.edges.clone();
                    // The next hop follows the rows taken, as the first
                    // does: not those skipped.
                    if self.decide(c, Some(&from))? && hop < hops {
                        for t in edges {
                            if !next.contains(&t) && !targets.contains(&t) {
                                next.push(t);
                            }
                        }
                    }
                }
                if !next.is_empty() {
                    self.follows.push_front(Follow { from, targets: next, hop: hop + 1 });
                }
            }
        }
        Ok(())
    }

    /// Decides the candidates read, up to a row taken that has edges to
    /// follow.
    fn drive(&mut self) -> Result<()> {
        while self.follows.is_empty() && !self.full() {
            let Some(c) = self.buffer.pop_front() else { break };
            self.decide(c, None)?;
        }
        Ok(())
    }

    /// The columns of the first answer, and which hold the text counted:
    /// those named by `OF`, else every column a row read holds text in,
    /// but one that holds the row's key.
    fn columns_of(&mut self, rs: &ResultSet) -> Result<()> {
        let shown = rs.columns.len().saturating_sub(self.hidden);
        if self.columns.is_none() {
            let columns: Vec<String> = rs.columns[..shown].to_vec();
            if !self.spec.of.is_empty() {
                self.text_cols = self
                    .spec
                    .of
                    .iter()
                    .map(|c| columns.iter().position(|x| x.eq_ignore_ascii_case(c)).ok_or_else(|| err(format!("PACK ... OF {c}: the query has no column {c} (its columns: {})", columns.join(", ")))))
                    .collect::<Result<_>>()?;
            }
            let searched = self.searches.iter().find_map(|(f, _)| f.as_deref().and_then(|f| self.table.as_ref()?.field(f)).map(|f| f.text_analyzer()));
            self.analyzers = columns.iter().map(|c| self.table.as_ref().and_then(|t| t.field(c)).map(|f| f.text_analyzer()).or(searched).unwrap_or_default()).collect();
            for &a in &self.analyzers {
                if !self.terms.contains_key(&a) {
                    let terms = self.searches.iter().flat_map(|(_, t)| a.query_term_seq(t, a.drops_query_stopwords())).collect();
                    self.terms.insert(a, terms);
                }
            }
            self.columns = Some(columns);
        }
        if self.spec.of.is_empty() {
            let key_name = self.table.as_ref().and_then(|t| t.primary_key.map(|k| t.fields[k].name.clone()));
            let names = self.columns.clone().unwrap_or_default();
            for j in 0..shown.min(names.len()) {
                if self.text_cols.contains(&j) || self.key_cols.contains(&j) || !rs.rows.iter().any(|r| matches!(r.get(j), Some(Value::Str(_)))) {
                    continue;
                }
                let holds_keys = rs.rows.iter().enumerate().all(|(i, r)| rs.keys.get(i).is_some_and(|k| !matches!(k, Value::Null) && r.get(j) == Some(k)));
                match holds_keys || key_name.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(&names[j])) {
                    true => self.key_cols.push(j),
                    false => self.text_cols.push(j),
                }
            }
            self.text_cols.sort_unstable();
        }
        Ok(())
    }

    /// Decides one candidate; whether it was taken.
    fn decide(&mut self, c: Candidate, linked_from: Option<&str>) -> Result<bool> {
        let label = key_label(&c.key, self.decisions.len());
        if !matches!(c.key, Value::Null) && !self.seen.insert(label.clone()) {
            return Ok(false);
        }
        let Candidate { mut row, key, score, edges } = c;
        let vector = match self.vector_at {
            None => None,
            Some(_) => match row.pop() {
                Some(Value::Vector(v)) => Some(v.to_vec()),
                Some(Value::Null) | None => None,
                Some(_) => return Err(err("PACK ... DEDUPE x ON column compares vectors: name a VECTOR column")),
            },
        };
        let text_of = |row: &[Value], j: usize| match row.get(j) {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => plain(v),
        };
        let counts: Vec<usize> = self.text_cols.iter().map(|&j| count_tokens(&text_of(&row, j))).collect();
        let full: usize = counts.iter().sum();
        let left = self.spec.budget - self.used;
        // A query may name no text column (its rows spend nothing).
        let (longest, most) = counts.iter().copied().enumerate().max_by_key(|&(_, n)| n).unwrap_or((0, 0));
        let rest = full - most;
        // A row's share of the budget leaves its longest text column an
        // excerpt of MIN_EXCERPT tokens at least, after its other text
        // columns: a row of a title and a description is not skipped while
        // the budget has room for both and an excerpt.
        let room = left.min(self.spec.row_cap().max(rest + MIN_EXCERPT));
        // Whole, or an excerpt of the longest text column.
        let (tokens, excerpted) = if full <= room {
            (full, false)
        } else {
            if rest + MIN_EXCERPT > room || room < MIN_EXCERPT {
                (0, false)
            } else {
                let j = self.text_cols[longest];
                let a = self.analyzers.get(j).copied().unwrap_or_default();
                let cut = excerpt_with(&text_of(&row, j), a, &self.terms[&a], room - rest);
                let n = count_tokens(&cut);
                // An excerpt never spends more than the room it was cut to.
                if cut.is_empty() || rest + n > room {
                    (0, false)
                } else {
                    row[j] = Value::Str(cut);
                    (rest + n, true)
                }
            }
        };
        let words: HashSet<String> = match self.spec.dedupe {
            Some((_, None)) => self.text_cols.iter().flat_map(|&j| word_set(&text_of(&row, j), self.analyzers.get(j).copied().unwrap_or_default())).collect(),
            _ => HashSet::new(),
        };
        let decision = if full == 0 {
            "skip: no text".to_string()
        } else if tokens == 0 {
            "skip: over budget".to_string()
        } else if let Some(dup) = self.duplicate_of(&words, vector.as_deref()) {
            format!("skip: near-duplicate of {dup}")
        } else {
            match (linked_from, excerpted) {
                (Some(from), false) => format!("linked from {from}"),
                (Some(from), true) => format!("excerpt, linked from {from}"),
                (None, false) => "take".to_string(),
                (None, true) => "excerpt".to_string(),
            }
        };
        let taken = !decision.starts_with("skip");
        match (taken, tokens == 0 && full > 0) {
            (true, _) => self.turned_away = 0,
            (false, true) => self.turned_away += 1,
            (false, false) => {}
        }
        let spent = if taken { tokens } else { 0 };
        self.used += spent;
        if taken {
            self.kept.push(Kept { label: label.clone(), words, vector });
            if self.spec.follow.is_some() && linked_from.is_none() {
                if matches!(key, Value::Null) {
                    return Err(err("PACK ... FOLLOW follows the edges of rows by their keys: the table needs a PRIMARY KEY"));
                }
                if !edges.is_empty() {
                    self.follows.push_back(Follow { from: label.clone(), targets: edges, hop: 1 });
                }
            }
        }
        self.decisions.push(Decision { key: key.clone(), score, tokens: spent, full_tokens: full, used: self.used, decision: decision.clone(), taken });
        if taken || self.spec.receipt {
            // A row skipped is answered without its text: the rows'
            // text is what the budget holds.
            if !taken {
                for &j in &self.text_cols {
                    if let Some(v) = row.get_mut(j) {
                        *v = Value::Null;
                    }
                }
            }
            row.push(Value::Int(spent as i64));
            row.push(Value::Str(decision));
            if self.spec.receipt {
                row.push(Value::Float(score));
                row.push(Value::Int(self.used as i64));
            }
            self.out_rows.push(row);
            self.out_keys.push(key);
            self.out_scores.push(score);
        }
        Ok(taken)
    }

    /// The label of a row taken that the row of these words and vector
    /// repeats, by `DEDUPE`.
    fn duplicate_of(&self, words: &HashSet<String>, vector: Option<&[f32]>) -> Option<String> {
        let (x, on) = self.spec.dedupe.as_ref()?;
        self.kept
            .iter()
            .find(|k| match (on, vector, k.vector.as_deref()) {
                (Some(_), Some(a), Some(b)) => cosine(a, b) >= *x,
                (Some(_), _, _) => false,
                (None, _, _) => overlap(words, &k.words) >= *x,
            })
            .map(|k| k.label.clone())
    }

    /// The pack: the rows taken and its receipt, read from the snapshot of
    /// `txn` and `seq_no` where they are known.
    pub fn finish(mut self, txn: Option<u64>, seq_no: Option<u64>) -> Result<Packed> {
        if self.pending.is_some() {
            return Err(err("PACK: the answer of the last query was not given"));
        }
        let mut columns = self.columns.take().unwrap_or_default();
        if self.text_cols.is_empty() && !self.decisions.is_empty() {
            return Err(err(format!("PACK counts the tokens of the query's text columns, and none of the rows read holds text (its columns: {}); select one, or name them with OF", columns.join(", "))));
        }
        // Rows read and not decided stay undecided: the budget was full.
        columns.push("pack_tokens".into());
        columns.push("pack_decision".into());
        if self.spec.receipt {
            columns.push("pack_score".into());
            columns.push("pack_used".into());
        }
        let mut stats = self.stats.take().unwrap_or_default();
        stats.plan.push(format!(
            "pack: {} of {} tokens in {} rows, {} candidates decided, read by {} quer{}",
            self.used,
            self.spec.budget,
            self.decisions.iter().filter(|d| d.taken).count(),
            self.decisions.len(),
            self.queries,
            if self.queries == 1 { "y" } else { "ies" }
        ));
        let receipt = Receipt { txn, seq_no, budget: self.spec.budget, used: self.used, rows: self.decisions, queries: self.queries };
        let rs = ResultSet { columns, rows: self.out_rows, keys: self.out_keys, scores: self.out_scores, parts: None, stats };
        Ok(Packed { rs, receipt })
    }
}

/// `LIMIT n OFFSET o`.
fn limit_offset(n: usize, offset: usize) -> sql::LimitClause {
    let num = |n: usize| sql::Expr::value(sql::Value::Number(n.to_string(), false));
    sql::LimitClause::LimitOffset { limit: Some(num(n)), offset: (offset > 0).then(|| sql::Offset { value: num(offset), rows: sql::OffsetRows::None }), limit_by: Vec::new() }
}

/// How a row is named in decisions: its key, else its place.
fn key_label(key: &Value, n: usize) -> String {
    match key {
        Value::Null => format!("#{}", n + 1),
        Value::Str(s) => s.clone(),
        v => plain(v),
    }
}

/// A value that is not text, as text.
fn plain(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Str(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Timestamp(t) => crate::timestamp::format(*t),
        Value::Vector(v) => format!("{} numbers", v.len()),
        Value::List(xs) => xs.iter().map(plain).collect::<Vec<_>>().join(", "),
    }
}

/// The terms of `text` that say something, as `analyzer` makes them (for
/// `english`, stemmed and without its stopwords; for `standard` and the
/// languages of scripts without spaces, their dictionary words too).
fn word_set(text: &str, analyzer: Analyzer) -> HashSet<String> {
    analyzer.analyze(text).into_iter().collect()
}

/// The share of the smaller set's words that are in the other.
fn overlap(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let (small, big) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if small.is_empty() {
        return 0.0;
    }
    small.iter().filter(|w| big.contains(*w)).count() as f64 / small.len() as f64
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let (mut ab, mut aa, mut bb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        ab += (*x as f64) * (*y as f64);
        aa += (*x as f64) * (*x as f64);
        bb += (*y as f64) * (*y as f64);
    }
    if aa == 0.0 || bb == 0.0 {
        return 0.0;
    }
    ab / (aa.sqrt() * bb.sqrt())
}

/// The texts the query searches by, with the field each searches: the
/// second argument of each `bm25` and `MATCH` in its `WHERE` and `ORDER
/// BY`, and the first.
fn search_texts(q: &sql::Query, params: &Params) -> Vec<(Option<String>, String)> {
    let mut texts = Vec::new();
    if let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &q.order_by {
        for e in es {
            texts_in(&e.expr, params, &mut texts);
        }
    }
    if let sql::SetExpr::Select(s) = q.body.as_ref() {
        if let Some(w) = &s.selection {
            texts_in(w, params, &mut texts);
        }
    }
    texts
}

fn texts_in(e: &sql::Expr, params: &Params, out: &mut Vec<(Option<String>, String)>) {
    match e {
        sql::Expr::Function(f) => {
            let name = f.name.to_string();
            let sql::FunctionArguments::List(list) = &f.args else { return };
            let args: Vec<&sql::Expr> = list
                .args
                .iter()
                .filter_map(|a| match a {
                    sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(x)) => Some(x),
                    _ => None,
                })
                .collect();
            if name.eq_ignore_ascii_case("bm25") || name.eq_ignore_ascii_case("match") {
                let field = match args.first() {
                    Some(sql::Expr::Identifier(i)) => Some(i.value.clone()),
                    Some(sql::Expr::CompoundIdentifier(is)) => is.last().map(|i| i.value.clone()),
                    _ => None,
                };
                if let Some(sql::Expr::Value(v)) = args.get(1) {
                    match &v.value {
                        sql::Value::SingleQuotedString(s) | sql::Value::DoubleQuotedString(s) => out.push((field, s.clone())),
                        sql::Value::Placeholder(p) => {
                            if let Some(Value::Str(s)) = params.0.get(param_name(p)) {
                                out.push((field, s.clone()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            for a in args {
                texts_in(a, params, out);
            }
        }
        sql::Expr::BinaryOp { left, right, .. } => {
            texts_in(left, params, out);
            texts_in(right, params, out);
        }
        sql::Expr::Nested(x) | sql::Expr::UnaryOp { expr: x, .. } => texts_in(x, params, out),
        _ => {}
    }
}

/// The terms of `terms` that `text` holds, as `analyzer` makes terms.
fn held_terms(text: &str, analyzer: Analyzer, terms: &HashSet<String>) -> Vec<String> {
    let mut held: Vec<String> = Vec::new();
    analyzer.for_each_token(text, |w| {
        if let Some(t) = analyzer.term_of(w) {
            if terms.contains(t.as_ref()) && !held.iter().any(|h| h == t.as_ref()) {
                held.push(t.into_owned());
            }
        }
    });
    held
}

/// The pieces an excerpt is made of, as byte ranges of `text` with their
/// tokens: its words (runs of characters between spaces), and a word of
/// more than [`MAX_PIECE`] tokens (a run of Chinese, a URL, base64) cut
/// into pieces of [`MAX_PIECE`] characters, each of which counts that
/// many tokens at most. The tokens of the text from one piece to another
/// are at most the sum of theirs.
fn pieces(text: &str) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                let w = &text[s..i];
                let n = count_tokens(w);
                if n <= MAX_PIECE {
                    out.push((s, i, n.max(1)));
                } else {
                    let mut from = s;
                    for (k, (at, _)) in w.char_indices().enumerate() {
                        if k > 0 && k % MAX_PIECE == 0 {
                            out.push((from, s + at, count_tokens(&text[from..s + at]).max(1)));
                            from = s + at;
                        }
                    }
                    out.push((from, i, count_tokens(&text[from..i]).max(1)));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// The stretch of `text` of at most `budget` tokens (with the marks that
/// say it was cut) that holds the most of `terms` (each analyzed as
/// `english` analyzes a query), the earliest of those; the start of the
/// text when it holds none. [`excerpt_with`] with the analyzer of a field.
pub fn excerpt(text: &str, terms: &[String], budget: usize) -> String {
    let a = Analyzer::English;
    let terms: HashSet<String> = terms.iter().flat_map(|t| a.query_term_seq(t, true)).collect();
    excerpt_with(text, a, &terms, budget)
}

/// The stretch of `text` of at most `budget` tokens, with the marks that
/// say it was cut, that holds the most pieces with one of `terms` (terms as
/// `analyzer` makes them), the earliest of those; the start of the text
/// when it holds none; empty when no piece fits. It is cut between words,
/// or inside a word too long to fit ([`pieces`]), and never counts more
/// than `budget` tokens.
pub fn excerpt_with(text: &str, analyzer: Analyzer, terms: &HashSet<String>, budget: usize) -> String {
    let pieces = pieces(text);
    if pieces.is_empty() {
        return String::new();
    }
    let total: usize = pieces.iter().map(|p| p.2).sum();
    if total <= budget {
        return text.to_string();
    }
    // The terms each piece holds: a word, or a cut piece with the piece
    // after it, so that a term across the cut is found.
    let held: Vec<Vec<String>> = match terms.is_empty() {
        true => vec![Vec::new(); pieces.len()],
        false => (0..pieces.len())
            .map(|i| {
                let (s, e, _) = pieces[i];
                let to = pieces.get(i + 1).filter(|n| n.0 == e).map_or(e, |n| n.1);
                held_terms(&text[s..to], analyzer, terms)
            })
            .collect(),
    };
    // A term in more than a quarter of the pieces says nothing of where to
    // cut (a particle such as `の`, a word the whole text is about).
    let mut df: HashMap<&str, usize> = HashMap::new();
    for t in held.iter().flatten() {
        *df.entry(t.as_str()).or_default() += 1;
    }
    let common = |t: &str| df[t] > 2 && df[t] * 4 > pieces.len();
    let hits: Vec<bool> = held.iter().map(|h| h.iter().any(|t| !common(t))).collect();
    // Two tokens for the marks (an ellipsis each side).
    let room = budget.saturating_sub(2);
    // The window of pieces of at most `room` tokens with the most hits.
    let (mut best, mut best_at, mut best_end) = (0usize, 0usize, 0usize);
    let (mut lo, mut tokens, mut n_hits) = (0usize, 0usize, 0usize);
    for hi in 0..pieces.len() {
        tokens += pieces[hi].2;
        n_hits += hits[hi] as usize;
        while tokens > room && lo <= hi {
            tokens -= pieces[lo].2;
            n_hits -= hits[lo] as usize;
            lo += 1;
        }
        if lo <= hi && n_hits > best {
            (best, best_at, best_end) = (n_hits, lo, hi + 1);
        }
    }
    if best == 0 {
        // No term: the text's start.
        best_at = 0;
        let mut t = 0;
        best_end = 0;
        while best_end < pieces.len() && t + pieces[best_end].2 <= room {
            t += pieces[best_end].2;
            best_end += 1;
        }
        if best_end == 0 {
            return String::new();
        }
    } else {
        // Centre the hits: from the first to the last hit of the window,
        // grow it a piece at a time on either side, after then before,
        // while the pieces fit.
        let first_hit = (best_at..best_end).find(|&i| hits[i]).unwrap_or(best_at);
        let last_hit = (best_at..best_end).rev().find(|&i| hits[i]).unwrap_or(best_end - 1);
        let (mut lo, mut hi) = (first_hit, last_hit + 1);
        let mut used: usize = pieces[lo..hi].iter().map(|p| p.2).sum();
        loop {
            let mut grew = false;
            if hi < pieces.len() && used + pieces[hi].2 <= room {
                used += pieces[hi].2;
                hi += 1;
                grew = true;
            }
            if lo > 0 && used + pieces[lo - 1].2 <= room {
                used += pieces[lo - 1].2;
                lo -= 1;
                grew = true;
            }
            if !grew {
                break;
            }
        }
        (best_at, best_end) = (lo, hi);
    }
    let mut out = String::new();
    if best_at > 0 {
        out.push_str("… ");
    }
    out.push_str(&text[pieces[best_at].0..pieces[best_end - 1].1]);
    if best_end < pieces.len() {
        out.push_str(" …");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_tokens_near_bpe() {
        assert_eq!(count_tokens(""), 0);
        assert_eq!(count_tokens("The quick brown fox jumps over the lazy dog."), 10);
        assert_eq!(count_tokens("idempotency"), 2);
        assert_eq!(count_tokens("2026"), 2);
        assert_eq!(count_tokens("fn main() {}"), 6);
        assert_eq!(count_tokens("日本語"), 3);
        // Other alphabets: a token per three letters.
        assert_eq!(count_tokens("привет"), 2);
        assert_eq!(count_tokens("café привет"), 3);
    }

    #[test]
    fn splits_the_clause_off() {
        let (rest, spec) = split("SELECT id, text FROM m ORDER BY bm25(text, :q) DESC PACK 4000 TOKENS DEDUPE 0.85 FOLLOW links 1 HOP WITH RECEIPT LIMIT 200").unwrap().unwrap();
        assert_eq!(rest, "SELECT id, text FROM m ORDER BY bm25(text, :q) DESC LIMIT 200");
        assert_eq!(spec, Spec { budget: 4000, budget_param: None, of: vec![], excerpt: None, dedupe: Some((0.85, None)), follow: Some(("links".into(), 1)), receipt: true });
        // The budget may be a parameter, bound when the query runs.
        let (rest, spec) = split("SELECT a FROM t PACK :n TOKENS LIMIT 9").unwrap().unwrap();
        assert_eq!((rest.as_str(), spec.budget, spec.budget_param.as_deref()), ("SELECT a FROM t LIMIT 9", 0, Some("n")));
        assert_eq!(bound_budget("n", &Params::new().set("n", Value::Int(300))).unwrap(), 300);
        assert!(bound_budget("n", &Params::new().set("n", Value::Int(0))).unwrap_err().to_string().contains("whole number"));
        assert!(bound_budget("n", &Params::new()).unwrap_err().to_string().contains("no parameter n"));
        let (rest, spec) = split("SELECT a FROM t pack 100 tokens of a, b excerpt 50 dedupe 0.9 on emb;").unwrap().unwrap();
        assert_eq!(rest, "SELECT a FROM t ;");
        assert_eq!(spec.of, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(spec.excerpt, Some(50));
        assert_eq!(spec.dedupe, Some((0.9, Some("emb".into()))));
        // A column named pack, and a PACK inside parentheses, are not the clause.
        assert!(split("SELECT pack FROM t WHERE pack > 3").unwrap().is_none());
        assert!(split("SELECT id FROM t WHERE id IN (SELECT id FROM t PACK 10 TOKENS)").unwrap().is_none());
        assert!(split("SELECT a FROM t PACK 0 TOKENS").is_err());
        assert!(split("SELECT a FROM t PACK 10 TOKENS DEDUPE 2").is_err());
        assert!(split("SELECT a FROM t PACK 10 TOKENS SOMETHING").unwrap_err().to_string().contains("not an option"));
    }

    #[test]
    fn excerpts_around_the_terms() {
        let filler = "lorem ipsum dolor sit amet ".repeat(40);
        let text = format!("{filler}the retry backoff hit its cap of five seconds {filler}");
        let cut = excerpt(&text, &["backoff".into(), "retry".into()], 20);
        assert!(cut.contains("retry backoff"), "{cut}");
        assert!(cut.starts_with("… ") && cut.ends_with(" …"), "{cut}");
        assert!(count_tokens(&cut) <= 20, "{} tokens: {cut}", count_tokens(&cut));
        // No term: the start.
        let cut = excerpt(&text, &[], 10);
        assert!(cut.starts_with("lorem") && count_tokens(&cut) <= 10);
        // Short text comes back whole.
        assert_eq!(excerpt("a short note", &[], 10), "a short note");
    }

    #[test]
    fn overlap_of_words() {
        let en = Analyzer::English;
        let a = word_set("use exponential backoff capped at 5 s", en);
        let b = word_set("Decision: use exponential backoff, capped at 5 s, with jitter", en);
        assert!(overlap(&a, &b) >= 0.85);
        assert!(overlap(&a, &word_set("payments retry storm on Tuesday", en)) < 0.2);
        // Text without spaces is compared by the words its analyzer finds,
        // not as one word.
        let ja = Analyzer::Standard;
        let a = word_set("東京都の天気は晴れです", ja);
        assert!(a.len() > 2, "{a:?}");
        assert!(overlap(&a, &word_set("今日の東京都の天気は晴れです", ja)) >= 0.8);
    }

    /// Every excerpt counts at most its budget, whatever the text: text
    /// without spaces, one long word, digits, marks (P1).
    #[test]
    fn excerpts_never_overrun_the_budget() {
        let texts = [
            "日本語のテキストには単語の間に空白がありません。".repeat(400),
            "x".repeat(50_000),
            format!("see {} for the payload", "QUJDRA".repeat(5_000)),
            "1234567890".repeat(3_000),
            "!@#$%^&*()".repeat(2_000),
            format!("{} retry {}", "ก".repeat(3_000), "ข".repeat(3_000)),
        ];
        for text in &texts {
            for budget in [3, 10, 32, 33, 100, 1000] {
                for terms in [vec![], vec!["retry".to_string()], vec!["空白".to_string()]] {
                    let cut = excerpt(text, &terms, budget);
                    let head: String = text.chars().take(20).collect();
                    assert!(count_tokens(&cut) <= budget, "budget {budget}: {} tokens from {head}", count_tokens(&cut));
                    assert!(budget < 10 || !cut.is_empty(), "budget {budget}: nothing from {head}");
                }
            }
        }
    }

    /// Excerpts find the query's terms as the field's analyzer makes them:
    /// in Japanese without spaces, and by stems rather than prefixes (P9,
    /// N-C3).
    #[test]
    fn excerpts_match_terms_by_the_analyzer() {
        let ja = Analyzer::Standard;
        let filler = "これは関係のない文章です。".repeat(200);
        let text = format!("{filler}東京都の天気は晴れです。{filler}");
        let terms: HashSet<String> = ja.query_term_seq("東京都の天気", false).into_iter().collect();
        let cut = excerpt_with(&text, ja, &terms, 40);
        assert!(cut.contains("天気"), "{cut}");
        assert!(count_tokens(&cut) <= 40);
        // `policy` is not `police`.
        let filler = "lorem ipsum dolor sit amet ".repeat(40);
        let text = format!("{filler}the police arrived {filler}the retry policy says wait {filler}");
        let cut = excerpt(&text, &["policy".into()], 12);
        assert!(cut.contains("retry policy"), "{cut}");
    }

    /// A query that does not read back as itself once printed is refused
    /// with a message, rather than run as other text (N-B8).
    #[test]
    fn a_query_that_does_not_print_back_is_refused() {
        let q = *crate::query::sql::parse_query("SELECT id, text FROM m WHERE rank > - -1 ORDER BY rank DESC").unwrap();
        let e = Next { query: q, params: Params::new() }.sql().unwrap_err().to_string();
        assert!(e.contains("reads back the same"), "{e}");
        let q = *crate::query::sql::parse_query("SELECT id, text FROM m WHERE rank > 1 ORDER BY rank DESC LIMIT 5").unwrap();
        assert_eq!(Next { query: q, params: Params::new() }.sql().unwrap(), "SELECT id, text FROM m WHERE rank > 1 ORDER BY rank DESC LIMIT 5");
    }
}
