//! Rank profiles: a ranking kept with a table under a name, which queries
//! rank by.
//!
//! ```sql
//! CREATE OR REPLACE RANK PROFILE recall ON memories AS
//!   WITH top AS (SELECT * FROM memories ORDER BY bm25(body, :q) DESC LIMIT 100)
//!   SELECT * FROM top ORDER BY bm25(body, :q) * (1 + :imp * importance) DESC
//!   WITH (imp => 0.2);
//! SELECT id, score() FROM memories WHERE kind = 'fact'
//!   ORDER BY rank_profile('recall', q => :q) LIMIT 10;
//! DROP RANK PROFILE IF EXISTS recall ON memories;
//! ```
//!
//! A profile's ranking is a query of its table that ranks every row:
//! `SELECT * FROM table ORDER BY expression`, or the `WITH` form of rerank
//! phases ([`crate::query::compound`]), each later phase `DESC`, with no
//! `WHERE` and no `LIMIT` after its last ranking: the query that ranks by
//! it filters and cuts. `WITH (name => value, ...)` after the ranking gives
//! defaults to its parameters: numbers, strings, `TRUE` and `FALSE`.
//! `CREATE` refuses a name the table has already; `CREATE OR REPLACE`
//! replaces it. The database keeps profiles with their table
//! ([`crate::storage::Database::set_rank_profile`]), logged and replicated
//! as writes are; dropping the table drops them.
//!
//! `ORDER BY rank_profile('name', parameter => value, ...)` is the whole
//! ORDER BY of a query of one table, without `ASC`: the query ranks as the
//! profile does, its `WHERE` filtering the rows the profile's first
//! ranking reads, its columns, `LIMIT` and `OFFSET` those of the last.
//! Each parameter of the profile is the call's argument (a value, or a
//! parameter of the query: `q => :q`), else the profile's default, else
//! the query's parameter of the same name. The query plans as if written
//! out in full ([`expand`]), and its plan names the profile and its hash
//! ([`crate::query::ir::ProfileRef`]), so that a database whose profile of
//! that name is another refuses the plan ([`check`]).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as J;
use sqlparser::ast::{self as sql, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, SelectItem, TableFactor};
use sqlparser::dialect::GenericDialect;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};

use crate::error::{Error, Result};
use crate::query::compound::{one_table, printable, query_mut};
use crate::query::ir::{Node, Plan, ProfileRef};
use crate::storage::RankProfile;

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

const CREATE_FORM: &str = "CREATE [OR REPLACE] RANK PROFILE name ON table AS query [WITH (parameter => value, ...)]";
const DROP_FORM: &str = "DROP RANK PROFILE [IF EXISTS] name [ON table]";

/// A statement that creates or drops a rank profile.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    Create { table: String, profile: RankProfile, replace: bool },
    /// Without `table`, of whichever table has a profile of that name.
    Drop { table: Option<String>, name: String, if_exists: bool },
}

/// The statement `text` holds when it creates or drops a rank profile;
/// `None` for any other.
pub fn parse_statement(text: &str) -> Result<Option<Statement>> {
    let all = Tokenizer::new(&GenericDialect {}, text).tokenize_with_location().map_err(|e| err(e.to_string()))?;
    let mut toks: Vec<&TokenWithSpan> = all.iter().filter(|t| !matches!(t.token, Token::Whitespace(_))).collect();
    if toks.last().is_some_and(|t| t.token == Token::SemiColon) {
        toks.pop();
    }
    let word = |i: usize| match toks.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_ascii_uppercase(),
        _ => String::new(),
    };
    let create = match word(0).as_str() {
        "CREATE" => true,
        "DROP" => false,
        _ => return Ok(None),
    };
    let replace = create && word(1) == "OR" && word(2) == "REPLACE";
    let mut i = if replace { 3 } else { 1 };
    if word(i) != "RANK" || word(i + 1) != "PROFILE" {
        return Ok(None);
    }
    i += 2;
    let form = if create { CREATE_FORM } else { DROP_FORM };
    let name_at = |i: usize| match toks.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) => Ok(w.value.clone()),
        _ => Err(err(format!("expected a name at {}; the form is {form}", toks.get(i).map_or("the end".to_string(), |t| t.token.to_string())))),
    };
    if !create {
        let if_exists = word(i) == "IF" && word(i + 1) == "EXISTS";
        if if_exists {
            i += 2;
        }
        let name = name_at(i)?;
        let table = match (word(i + 1).as_str(), toks.len()) {
            (_, n) if n == i + 1 => None,
            ("ON", n) if n == i + 3 => Some(name_at(i + 2)?),
            _ => return Err(err(format!("the form is {DROP_FORM}"))),
        };
        return Ok(Some(Statement::Drop { table, name, if_exists }));
    }
    let name = name_at(i)?;
    if word(i + 1) != "ON" || word(i + 3) != "AS" {
        return Err(err(format!("the form is {CREATE_FORM}")));
    }
    let table = name_at(i + 2)?;
    let start = i + 4;
    if start >= toks.len() {
        return Err(err(format!("the profile's ranking is missing; the form is {CREATE_FORM}")));
    }
    // A trailing `WITH (...)` at the top level gives the defaults; a
    // `WITH` that starts the ranking names its phases (`WITH a AS (`).
    let mut depth = 0i32;
    let mut defaults_at = None;
    for j in start..toks.len() {
        match &toks[j].token {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            Token::Word(w) if depth == 0 && w.quote_style.is_none() && w.value.eq_ignore_ascii_case("WITH") && toks.get(j + 1).is_some_and(|t| t.token == Token::LParen) => defaults_at = Some(j),
            _ => {}
        }
    }
    let end = match defaults_at {
        Some(j) => {
            let mut depth = 0;
            let close = (j + 1..toks.len()).find(|&k| {
                match toks[k].token {
                    Token::LParen => depth += 1,
                    Token::RParen => depth -= 1,
                    _ => {}
                }
                depth == 0
            });
            if close != Some(toks.len() - 1) {
                return Err(err(format!("WITH (...) after the ranking ends the statement; the form is {CREATE_FORM}")));
            }
            j
        }
        None => toks.len(),
    };
    let from = byte_at(text, toks[start].span.start);
    let to = if end < toks.len() { byte_at(text, toks[end].span.start) } else { toks.last().map_or(text.len(), |t| byte_at(text, t.span.end)) };
    let ranking = super::sql::parse_query(&text[from..to]).map_err(|e| err(format!("the profile's ranking: {e}")))?;
    let defaults = match defaults_at {
        Some(j) => defaults(&toks[j + 2..toks.len() - 1])?,
        None => BTreeMap::new(),
    };
    let profile = RankProfile { name, ranking: printable(&ranking), defaults };
    check(&profile, &table)?;
    Ok(Some(Statement::Create { table, profile, replace }))
}

/// The byte of `text` at a tokenizer's location (lines and columns of
/// characters, from 1).
fn byte_at(text: &str, at: Location) -> usize {
    let line: usize = text.split_inclusive('\n').take((at.line as usize).saturating_sub(1)).map(str::len).sum();
    text[line..].char_indices().nth((at.column as usize).saturating_sub(1)).map_or(text.len(), |(b, _)| line + b)
}

/// `name => value, ...` (or `name = value`): numbers, strings, `TRUE` and
/// `FALSE`.
fn defaults(toks: &[&TokenWithSpan]) -> Result<BTreeMap<String, J>> {
    let mut out = BTreeMap::new();
    for item in toks.split(|t| t.token == Token::Comma) {
        let bad = || err(format!("WITH ({}): each default is parameter => value, a number, a string, TRUE or FALSE", toks.iter().map(|t| t.token.to_string()).collect::<Vec<_>>().join(" ")));
        let (name, value) = match item {
            [n, op, v @ ..] if matches!(op.token, Token::RArrow | Token::Eq) => match &n.token {
                Token::Word(w) => (w.value.to_ascii_lowercase(), v),
                _ => return Err(bad()),
            },
            _ => return Err(bad()),
        };
        let number = |s: &str, neg: bool| {
            let s = if neg { format!("-{s}") } else { s.to_string() };
            s.parse::<i64>().map(J::from).ok().or_else(|| s.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(J::Number))
        };
        let v = match value.iter().map(|t| &t.token).collect::<Vec<_>>().as_slice() {
            [Token::Number(s, _)] => number(s, false),
            [Token::Minus, Token::Number(s, _)] => number(s, true),
            [Token::SingleQuotedString(s)] => Some(J::String(s.clone())),
            [Token::Word(w)] if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("true") => Some(J::Bool(true)),
            [Token::Word(w)] if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("false") => Some(J::Bool(false)),
            _ => None,
        };
        let v = v.ok_or_else(bad)?;
        if out.insert(name.clone(), v).is_some() {
            return Err(err(format!("WITH (...) gives {name} twice")));
        }
    }
    Ok(out)
}

/// Checks that a profile of `table` ranks as a profile must (the [module
/// docs](self)), and that its defaults are of parameters it reads.
pub fn check(profile: &RankProfile, table: &str) -> Result<()> {
    let q = super::sql::parse_query(&profile.ranking)?;
    let shape = format!("a rank profile's ranking is SELECT * FROM {table} ORDER BY expression, or WITH phases over it (WITH a AS (SELECT * FROM {table} ORDER BY e DESC LIMIT n) SELECT * FROM a ORDER BY e2 DESC)");
    let bad = |what: &str| err(format!("rank profile {}: {what}; {shape}", profile.name));
    // Each step: its query, the table or phase it reads, whether it is the
    // last.
    let mut steps: Vec<(&sql::Query, String, bool)> = Vec::new();
    let mut reads = table.to_string();
    if let Some(w) = &q.with {
        if w.recursive {
            return Err(bad("WITH RECURSIVE"));
        }
        for c in &w.cte_tables {
            if !c.alias.columns.is_empty() {
                return Err(bad("a phase names its columns"));
            }
            steps.push((&c.query, reads, false));
            reads = c.alias.name.value.clone();
        }
    }
    steps.push((&q, reads, true));
    for (k, (step, reads, last)) in steps.into_iter().enumerate() {
        if step.with.is_some() && !last {
            return Err(bad("a phase has a WITH of its own"));
        }
        let Some((s, from)) = one_table(step) else { return Err(bad("a step reads other than one table")) };
        if !from.eq_ignore_ascii_case(&reads) {
            return Err(bad(&format!("a step reads {from}, not {reads}")));
        }
        if !matches!(s.projection.as_slice(), [SelectItem::Wildcard(_)]) {
            return Err(bad("a step selects other than *"));
        }
        if s.selection.is_some() {
            return Err(bad("it filters (WHERE); the query that ranks by it does"));
        }
        if s.distinct.is_some() || s.having.is_some() || !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty()) {
            return Err(bad("it groups"));
        }
        let o = match &step.order_by {
            Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) if es.len() == 1 => &es[0],
            _ => return Err(bad("each step ranks by one expression (ORDER BY)")),
        };
        if k > 0 && o.options.asc != Some(false) {
            return Err(bad("a rerank phase ranks best first (DESC)"));
        }
        let mut nested = false;
        super::compound::walk(&o.expr, &mut |x| nested |= matches!(x, Expr::Function(f) if is_call(f)));
        if nested {
            return Err(bad("it ranks by a rank profile"));
        }
        match (last, &step.limit_clause) {
            (true, Some(_)) => return Err(bad("its last ranking has a LIMIT or OFFSET; the query that ranks by it cuts")),
            (false, None) => return Err(bad("a phase before the last has no LIMIT")),
            _ => {}
        }
    }
    let reads = parameters(&q);
    if let Some(p) = profile.defaults.keys().find(|p| !reads.contains(*p)) {
        return Err(err(format!("rank profile {}: WITH ({p} => ...): the ranking reads no parameter :{p}", profile.name)));
    }
    Ok(())
}

/// The names of the parameters `q` reads (`:q` is `q`), lower case.
fn parameters(q: &sql::Query) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    query_mut(&mut q.clone(), &mut |e| {
        if let Expr::Value(v) = e {
            if let sql::Value::Placeholder(p) = &v.value {
                names.insert(p.trim_start_matches([':', '$', '@', '?']).to_ascii_lowercase());
            }
        }
    });
    names
}

/// `q` written out with the rank profile it ranks by, and the profile's
/// name and hash; `None` for a query that ranks by none. `profile_of`
/// finds a table's (any case) profile by name (any case).
pub fn expand(q: &sql::Query, profile_of: &dyn Fn(&str, &str) -> Option<RankProfile>) -> Result<Option<(sql::Query, ProfileRef)>> {
    let mut n = 0;
    query_mut(&mut q.clone(), &mut |e| n += matches!(e, Expr::Function(f) if is_call(f)) as usize);
    if n == 0 {
        return Ok(None);
    }
    let form = "rank_profile('name', parameter => value, ...) is the whole ORDER BY of a query of one table, without a WITH, join, GROUP BY or DISTINCT";
    let o = match &q.order_by {
        Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) if n == 1 && es.len() == 1 => &es[0],
        _ => return Err(err(form)),
    };
    let Expr::Function(call) = &o.expr else { return Err(err(form)) };
    if !is_call(call) {
        return Err(err(form));
    }
    if o.options.asc == Some(true) || o.options.nulls_first.is_some() {
        return Err(err("a rank profile ranks best first, its rows without a value last: ORDER BY rank_profile(...), without ASC or NULLS"));
    }
    let Some((s, table)) = one_table(q).filter(|(s, _)| q.with.is_none() && s.distinct.is_none() && s.having.is_none() && matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty())) else {
        return Err(err(form));
    };
    let FunctionArguments::List(list) = &call.args else { return Err(err(form)) };
    let mut args = list.args.iter();
    let name = match args.next() {
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(v)))) => match &v.value {
            sql::Value::SingleQuotedString(s) => s.clone(),
            _ => return Err(err(form)),
        },
        _ => return Err(err(form)),
    };
    let profile = profile_of(&table, &name).ok_or_else(|| err(format!("{table} has no rank profile named {name}")))?;
    let mut p = super::sql::parse_query(&profile.ranking)?;
    let reads = parameters(&p);
    let mut subs: BTreeMap<String, Expr> = profile.defaults.iter().map(|(k, v)| (k.clone(), literal(v))).collect();
    for a in args {
        let (k, e) = match a {
            FunctionArg::Named { name, arg: FunctionArgExpr::Expr(e), .. } => (name.value.to_ascii_lowercase(), e),
            _ => return Err(err(format!("rank_profile('{name}', ...): its parameters are named, parameter => value"))),
        };
        if !reads.contains(&k) {
            let all: Vec<String> = reads.iter().map(|r| format!(":{r}")).collect();
            return Err(err(format!("rank profile {} reads no parameter :{k}; it reads {}", profile.name, if all.is_empty() { "none".into() } else { all.join(", ") })));
        }
        let value = matches!(e, Expr::Value(_)) || matches!(e, Expr::UnaryOp { op: sql::UnaryOperator::Minus | sql::UnaryOperator::Plus, expr } if matches!(**expr, Expr::Value(_)));
        if !value {
            return Err(err(format!("rank_profile('{name}', {k} => {e}): an argument is a value or a parameter of the query")));
        }
        subs.insert(k, e.clone());
    }
    query_mut(&mut p, &mut |e| {
        if let Expr::Value(v) = e {
            if let sql::Value::Placeholder(ph) = &v.value {
                if let Some(x) = subs.get(&ph.trim_start_matches([':', '$', '@', '?']).to_ascii_lowercase()) {
                    *e = x.clone();
                }
            }
        }
    });
    let mut out = q.clone();
    out.order_by = p.order_by.take();
    if let Some(mut w) = p.with.take() {
        // The first phase reads the query's table as the query names it,
        // under its WHERE; the query reads the last phase's rows.
        let sql::SetExpr::Select(first) = w.cte_tables[0].query.body.as_mut() else { unreachable!("checked when created") };
        first.from = s.from.clone();
        first.selection = s.selection.clone();
        let last = w.cte_tables.last().expect("a WITH has a table").alias.name.clone();
        let sql::SetExpr::Select(o) = out.body.as_mut() else { unreachable!("one table") };
        if let TableFactor::Table { name, .. } = &mut o.from[0].relation {
            *name = sql::ObjectName::from(vec![last]);
        }
        o.selection = None;
        out.with = Some(w);
    }
    Ok(Some((out, ProfileRef { name: profile.name.clone(), hash: profile.hash() })))
}

fn is_call(f: &sql::Function) -> bool {
    f.name.to_string().eq_ignore_ascii_case("rank_profile")
}

/// A default as an SQL literal.
fn literal(v: &J) -> Expr {
    Expr::value(match v {
        J::Number(n) => sql::Value::Number(n.to_string(), false),
        J::String(s) => sql::Value::SingleQuotedString(s.clone()),
        J::Bool(b) => sql::Value::Boolean(*b),
        _ => sql::Value::Null,
    })
}

/// The text of a query as [`expand`] writes it out, or as given if it
/// ranks by no profile.
pub fn written_out(sql_text: &str, profile_of: &dyn Fn(&str, &str) -> Option<RankProfile>) -> Result<String> {
    Ok(match expand(&*super::sql::parse_query(sql_text)?, profile_of)? {
        Some((q, _)) => q.to_string(),
        None => sql_text.to_string(),
    })
}

/// Names the profile on the Searches of `table` in the plan of a query
/// that [`expand`] wrote out (outside subqueries, which rank otherwise).
pub fn mark(plan: Plan, table: &str, r: &ProfileRef) -> Plan {
    fn on(n: &mut Node, table: &str, r: &ProfileRef) {
        match n {
            Node::Search(s) if s.table.eq_ignore_ascii_case(table) => s.profile = Some(r.clone()),
            Node::Filter { input, .. }
            | Node::Aggregate { input, .. }
            | Node::Sort { input, .. }
            | Node::Limit { input, .. }
            | Node::Project { input, .. }
            | Node::Distinct { input }
            | Node::KeyOrder { input, .. } => on(input, table, r),
            Node::With { input, .. } => on(input, table, r),
            _ => {}
        }
    }
    let mut root = plan.root;
    on(&mut root, table, r);
    Plan::new(root)
}

const STALE: &str = "is not the one the plan was made with";
const GONE: &str = "which the plan ranks as";

/// Refuses a plan made with a rank profile that a table here does not
/// have as the plan names it: `hash_of(table, name)` is the hash of a
/// table's profile of that name, if it has one.
pub fn check_plan(plan: &Plan, hash_of: &dyn Fn(&str, &str) -> Option<String>) -> Result<()> {
    let mut bad = None;
    plan.root.each_search(&mut |s| {
        let Some(r) = s.profile.as_ref().filter(|_| bad.is_none()) else { return };
        bad = match hash_of(&s.table, &r.name) {
            Some(h) if h == r.hash => None,
            Some(h) => Some(format!("rank profile {} of {} {STALE} (hash {h} here, {} in the plan): it was replaced since, or the copies differ", r.name, s.table, r.hash)),
            None => Some(format!("{} has no rank profile {}, {GONE}", s.table, r.name)),
        };
    });
    bad.map_or(Ok(()), |m| Err(err(m)))
}

/// Whether an error says what [`check_plan`] says of a plan whose rank
/// profile has changed or gone: a coordinator that planned with what its
/// shards held before plans again.
pub fn is_stale(message: &str) -> bool {
    message.contains(STALE) || message.contains(GONE)
}
