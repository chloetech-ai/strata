//! A language model's answers as a query's values:
//! `llm(model, instructions, input, ...)`, the model's answer to the
//! instructions about the inputs, on each row a query returns.
//!
//! ```sql
//! CREATE FUNCTION summarize(t TEXT) RETURNS TEXT
//! AS llm('anthropic/claude-haiku-5-5', 'Summarize this review in one sentence.', :t);
//!
//! SELECT id, summarize(body) FROM reviews WHERE MATCH(body, :q) ORDER BY bm25(body, :q) DESC LIMIT 10;
//! ```
//!
//! A call costs a request to the model and about a second, so it runs on
//! the rows a query returns and on no other: a query that calls one runs
//! in two steps ([`split`]). The first is the query without the calls,
//! which finds the rows (after its `WHERE`, `ORDER BY` and `LIMIT`) and
//! computes what the calls read of each, as columns of its own; the second
//! ([`Split::finish`]) asks the model about each of those rows, a few at a
//! time, and computes the query's columns of the answers. A call may be
//! part of a column (`coalesce(llm(...), title)`, `CASE WHEN length(body)
//! < 200 THEN body ELSE llm(...) END`): the rest of the column is computed
//! over the answer as the operators compute values over rows in memory.
//!
//! In `ORDER BY`, a call ranks the rows the query would return without
//! that ORDER BY and its `LIMIT`, which are [`Limits::rows`] at most: the
//! top rows of a first ranking, as a rerank phase ranks them
//! (`WITH top AS (... ORDER BY bm25(body, :q) DESC LIMIT 50) SELECT ...
//! FROM top ORDER BY llm(...) DESC LIMIT 10`). Rows that tie keep the
//! order the first step gave them, and `score()` is the first key's value.
//!
//! A call is refused anywhere else (`WHERE`, `GROUP BY`, a join, a `WITH`,
//! a subquery), where it would run for rows a query reads rather than
//! returns. A plan never holds one: on a coordinator over shards the first
//! step is an ordinary query of the shards and the second runs on the
//! coordinator, over the merged rows, so no shard asks a model about a row
//! the merge cuts.
//!
//! * **Arguments.** `model` is a string: `anthropic/<model>` (its key in
//!   `ANTHROPIC_API_KEY`), `openai/<chat model>`, or a model directory
//!   whose `endpoint.json` names a chat server (`"api": "chat"` or
//!   `"anthropic"`), as for [text made of
//!   text](crate::schema::Derive::prompt). `instructions` is the system
//!   prompt, and each input after it is a part of the user's message,
//!   apart by a blank line; a `NULL` input is left out, and a call with no
//!   input is `NULL`, with no request.
//! * **Answers** are text, trimmed. `choices => 'a,b,c'` asks for one of
//!   them and gives it (any case, the answer's words searched for one if it
//!   says more), or `NULL`; `returns => 'number'` gives the first number of
//!   the answer as a FLOAT, and `returns => 'bool'` yes or no as a BOOL,
//!   `NULL` without one. `max_tokens => n` bounds the answer (1024 for
//!   text, 64 otherwise). The model answers with its own sampling
//!   defaults: no temperature is sent, which current Claude models refuse.
//! * **Cost.** One request per distinct (model, prompt, input), the
//!   endpoint's `concurrency` at once (8); answers are kept in memory
//!   ([`CACHE_ENTRIES`]), so the same question costs nothing the second
//!   time. A query calls the model for [`Limits::rows`] rows at most, and
//!   the process for [`Limits::per_minute`] requests a minute
//!   ([`set_limits`]); past either the query fails before asking.
//! * **Inputs are data.** A row's text is the user's message, the
//!   instructions the system prompt, and the model has no tools: a row can
//!   change its own answer, and nothing else.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use sha2::{Digest, Sha256};
use sqlparser::ast::{self as sql, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, SelectItem, Visit, Visitor};

use super::compound;
use super::exec::ResultSet;
use super::function::FunctionSet;
use super::ir;
use super::Params;
use crate::error::{Error, Result};
use crate::value::Value;

/// The function's name.
pub const NAME: &str = "llm";

/// Answers kept in memory, by a hash of the question.
pub const CACHE_ENTRIES: usize = 10_000;

/// Tokens a text answer takes at most, unless `max_tokens` says.
pub const TEXT_TOKENS: u64 = 1024;

/// Tokens a choice, number or yes/no takes at most, unless `max_tokens`
/// says.
pub const SHORT_TOKENS: u64 = 64;

/// What a process lets queries ask of models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Rows a query calls a model for, at most.
    pub rows: usize,
    /// Requests the process makes a minute, at most (answers kept in memory
    /// are free); `None` for no limit.
    pub per_minute: Option<u64>,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { rows: 100, per_minute: None }
    }
}

fn limits_cell() -> &'static Mutex<Limits> {
    static L: OnceLock<Mutex<Limits>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Limits::default()))
}

/// Sets what queries of this process may ask of models.
pub fn set_limits(l: Limits) {
    *limits_cell().lock().unwrap_or_else(|e| e.into_inner()) = l;
}

/// What queries of this process may ask of models.
pub fn limits() -> Limits {
    *limits_cell().lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts of this process's calls since it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Requests made to models.
    pub requests: u64,
    /// Answers found in memory.
    pub cached: u64,
    /// Requests that failed, after their retries.
    pub failures: u64,
}

static REQUESTS: AtomicU64 = AtomicU64::new(0);
static CACHED: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);

/// This process's calls so far.
pub fn usage() -> Usage {
    Usage { requests: REQUESTS.load(Ordering::Relaxed), cached: CACHED.load(Ordering::Relaxed), failures: FAILURES.load(Ordering::Relaxed) }
}

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

const USAGE: &str = "llm(model, instructions, input, ...): model a string such as 'anthropic/claude-haiku-5-5', then the instructions and the inputs, and options choices => 'a,b,c', returns => 'text' | 'number' | 'bool', max_tokens => n";

/// Why a classifier's call over a row's vector stands where it does.
const CLASSIFY_WHERE: &str = "classify() over a row's vector is a column of the rows a query returns (SELECT) or ranks them (ORDER BY), not part of WHERE, GROUP BY, HAVING, a join, a WITH or a subquery; to filter or group rows by their class, keep it in a column (a KEYWORD field OPTIONS (derive = 'title', model = 'classifier:topic') labels each row written), and a call with a parameter's vector, topic(:q), stands anywhere";

const WHERE: &str = "llm() is a column of the rows a query returns (SELECT) or ranks them (ORDER BY), not part of WHERE, GROUP BY, HAVING, a join, a WITH or a subquery: each call is a request to the model, made for the rows the query returns and no others";

/// What an answer is read as.
#[derive(Clone, Debug, PartialEq)]
enum Returns {
    Text,
    Number,
    Bool,
    Choice(Vec<String>),
}

/// One call of [`NAME`] in a query, its arguments expressions over the
/// first step's columns ([`Split`]).
#[derive(Clone, Debug)]
struct Call {
    /// A classifier's call ([`super::classify`]): computed here, from its
    /// one argument, the vector. `model` is then [`super::classify::NAME`].
    classifier: Option<(Arc<super::classify::Classifier>, super::classify::Returns)>,
    model: String,
    /// The instructions, then the inputs.
    args: Vec<Expr>,
    returns: Returns,
    max_tokens: u64,
}

impl Call {
    /// The system prompt: the instructions, and what to answer with.
    fn system(&self, instructions: &str) -> String {
        let tail = match &self.returns {
            Returns::Text => return instructions.to_string(),
            Returns::Number => "Answer with a number, and nothing else.".to_string(),
            Returns::Bool => "Answer yes or no, and nothing else.".to_string(),
            Returns::Choice(cs) => format!("Answer with one of these, and nothing else: {}.", cs.join(", ")),
        };
        match instructions.trim().is_empty() {
            true => tail,
            false => format!("{}\n\n{tail}", instructions.trim_end()),
        }
    }

    /// The answer `text` as a value of what the call returns.
    fn read(&self, text: &str) -> Value {
        let t = text.trim();
        match &self.returns {
            Returns::Text => Value::Str(t.to_string()),
            Returns::Number => first_number(t).map_or(Value::Null, Value::Float),
            Returns::Bool => match words(t).first().map(String::as_str) {
                Some("yes" | "true") => Value::Bool(true),
                Some("no" | "false") => Value::Bool(false),
                _ => Value::Null,
            },
            Returns::Choice(cs) => {
                let bare = t.trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace()).to_lowercase();
                if let Some(c) = cs.iter().find(|c| c.to_lowercase() == bare) {
                    return Value::Str(c.clone());
                }
                // An answer that says more: the one choice it names.
                let ws = words(t);
                let named: Vec<&String> = cs.iter().filter(|c| {
                    let cw = words(c);
                    !cw.is_empty() && ws.windows(cw.len()).any(|w| w == cw.as_slice())
                }).collect();
                match named.as_slice() {
                    [c] => Value::Str((*c).clone()),
                    _ => Value::Null,
                }
            }
        }
    }
}

/// The lower-case words of `t`.
fn words(t: &str) -> Vec<String> {
    t.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-').filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// The first number written in `t`: `-2`, `3.5`, `7/10`'s 7.
fn first_number(t: &str) -> Option<f64> {
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let neg = b[i] == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit);
        if b[i].is_ascii_digit() || neg {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_digit() || (b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))) {
                i += 1;
            }
            return t[start..i].parse().ok();
        }
        i += 1;
    }
    None
}

/// A column of the answer.
#[derive(Clone, Debug)]
enum Item {
    /// The first step's columns of a SELECT item as it is: one, or a
    /// wildcard's several.
    Pass { wildcard: bool },
    /// A column computed of the first step's columns and the calls'
    /// answers.
    Made { name: String, expr: Expr },
    /// `score()` of a query its calls order: the first key's value.
    Score { name: String },
}

/// A query that calls a model, in its two steps ([module docs](self)).
#[derive(Clone, Debug)]
pub struct Split {
    /// The first step: the query that finds the rows, without the calls,
    /// each value a call reads a column of its own after the query's
    /// columns. When the calls order the rows, without its ORDER BY,
    /// OFFSET and LIMIT, and [`Limits::rows`] + 1 rows at most.
    pub first: sql::Query,
    /// The first step's columns after the query's: what the calls read.
    hidden: usize,
    calls: Vec<Call>,
    items: Vec<Item>,
    /// The keys the calls order the rows by, over the first step's columns
    /// and the answers; empty when the first step orders them.
    order: Vec<(Expr, bool, Option<bool>)>,
    offset: usize,
    limit: Option<usize>,
    rows: usize,
    /// Whether the first step's LIMIT is [`Split::rows`] + 1, set here.
    past: bool,
    /// Calls of a classifier computed before the first step
    /// ([`super::classify::fold`]).
    folded: usize,
}

impl Split {
    /// Holds the rows the calls read to `max` at most (a server's
    /// `--max-rows`, the most a query answers): the first step then
    /// fetches `max` + 1 rows, one more to say there are more.
    pub fn cap_rows(&mut self, max: usize) {
        if max >= self.rows {
            return;
        }
        self.rows = max;
        if self.past {
            self.first.limit_clause = Some(sql::LimitClause::LimitOffset { limit: Some(Expr::Value(sql::Value::Number((max + 1).to_string(), false).into())), offset: None, limit_by: Vec::new() });
        }
    }
}

/// Whether `f` calls [`NAME`].
fn is_llm(f: &sql::Function) -> bool {
    f.name.to_string().eq_ignore_ascii_case(NAME)
}

/// How many calls of [`NAME`] the text of `x` holds.
fn calls_in<V: Visit>(x: &V) -> usize {
    calls_named(x, &[NAME, super::classify::NAME])
}

/// How many calls of the functions `names` (an unquoted name, any case)
/// `x` holds: by its tree, not its text, which a classifier's call makes
/// hundreds of kilobytes long.
fn calls_named<V: Visit>(x: &V, names: &[&str]) -> usize {
    struct Count<'a> {
        names: &'a [&'a str],
        n: usize,
    }
    impl Visitor for Count<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, e: &Expr) -> std::ops::ControlFlow<()> {
            if let Expr::Function(f) = e {
                if let [sql::ObjectNamePart::Identifier(id)] = f.name.0.as_slice() {
                    if id.quote_style.is_none() && self.names.iter().any(|n| id.value.eq_ignore_ascii_case(n)) {
                        self.n += 1;
                    }
                }
            }
            std::ops::ControlFlow::Continue(())
        }
    }
    let mut c = Count { names, n: 0 };
    let _ = x.visit(&mut c);
    c.n
}

/// Whether a query of text `sql` may call [`NAME`], itself or through a
/// stored function of `functions`: a test that costs nothing, before
/// [`split`] looks.
pub fn may_call(sql: &str, functions: &FunctionSet) -> bool {
    let named = |n: &str| sql.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n.as_bytes()));
    functions.calls_llm() || named(NAME) || named(super::classify::NAME)
}

/// Whether `q` calls [`NAME`] anywhere.
pub(crate) fn calls_in_query(q: &sql::Query) -> bool {
    calls_in(q) > 0
}

/// The name of a first step's column of its own.
fn hidden_name(i: usize) -> String {
    format!("__llm{i}")
}

/// The name of a call's answer in the second step.
fn answer_name(i: usize) -> String {
    format!("__llm_answer{i}")
}

/// Whether `e` is a value of no row: a literal or a parameter.
fn constant(e: &Expr) -> bool {
    match e {
        Expr::Value(_) => true,
        Expr::UnaryOp { expr, .. } | Expr::Nested(expr) => constant(expr),
        Expr::BinaryOp { left, right, .. } => constant(left) && constant(right),
        _ => false,
    }
}

/// Builds the second step's expression of `e`: each call of [`NAME`] its
/// answer, and each part without one a column of the first step (or kept,
/// if a value of no row).
struct Builder<'p> {
    hidden: Vec<Expr>,
    calls: Vec<Call>,
    params: &'p Params,
}

impl Builder<'_> {
    fn column(&mut self, e: &Expr) -> Expr {
        if constant(e) {
            return e.clone();
        }
        // The same expression is one column.
        let i = self.hidden.iter().position(|h| h == e).unwrap_or_else(|| {
            self.hidden.push(e.clone());
            self.hidden.len() - 1
        });
        Expr::Identifier(Ident::new(hidden_name(i)))
    }

    fn build(&mut self, e: &Expr) -> Result<Expr> {
        if calls_in(e) == 0 {
            return Ok(self.column(e));
        }
        let mut out = e.clone();
        match &mut out {
            Expr::Function(f) if is_llm(f) => return self.call(f),
            Expr::Function(f) if super::classify::is_call(f) => return self.classify(f),
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => return Err(err(WHERE)),
            // coalesce(a, b, ...) as the CASE it is, so that a call after a
            // value that is not NULL asks nothing.
            Expr::Function(f) if f.name.to_string().eq_ignore_ascii_case("coalesce") && f.over.is_none() && compound::func_args(f).len() >= 2 => {
                let args: Vec<Expr> = compound::func_args(f).into_iter().cloned().collect();
                let (last, rest) = args.split_last().expect("two or more");
                let case = Expr::Case {
                    case_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
                    end_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
                    operand: None,
                    conditions: rest.iter().map(|a| sql::CaseWhen { condition: Expr::IsNotNull(Box::new(a.clone())), result: a.clone() }).collect(),
                    else_result: Some(Box::new(last.clone())),
                };
                return self.build(&case);
            }
            Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } | Expr::IsNull(x) | Expr::IsNotNull(x) | Expr::Cast { expr: x, .. } => **x = self.build(x)?,
            Expr::BinaryOp { left, right, .. } => {
                **left = self.build(left)?;
                **right = self.build(right)?;
            }
            Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
                **expr = self.build(expr)?;
                **pattern = self.build(pattern)?;
            }
            Expr::Between { expr, low, high, .. } => {
                **expr = self.build(expr)?;
                **low = self.build(low)?;
                **high = self.build(high)?;
            }
            Expr::InList { expr, list, .. } => {
                **expr = self.build(expr)?;
                for x in list.iter_mut() {
                    *x = self.build(x)?;
                }
            }
            Expr::Case { operand, conditions, else_result, .. } => {
                if let Some(x) = operand {
                    **x = self.build(x)?;
                }
                for c in conditions.iter_mut() {
                    c.condition = self.build(&c.condition)?;
                    c.result = self.build(&c.result)?;
                }
                if let Some(x) = else_result {
                    **x = self.build(x)?;
                }
            }
            Expr::Function(f) if f.over.is_none() && f.filter.is_none() => {
                let FunctionArguments::List(l) = &mut f.args else { return Err(err(format!("{e}: {WHERE}"))) };
                for a in l.args.iter_mut() {
                    match a {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) | FunctionArg::Named { arg: FunctionArgExpr::Expr(x), .. } | FunctionArg::ExprNamed { arg: FunctionArgExpr::Expr(x), .. } => *x = self.build(x)?,
                        _ => {}
                    }
                }
            }
            _ => return Err(err(format!("{e}: llm() inside this expression is not supported; a column may call it within arithmetic, ||, CASE, comparisons and functions of values"))),
        }
        Ok(out)
    }

    /// A call of [`NAME`]: its answer's name, the call kept.
    fn call(&mut self, f: &sql::Function) -> Result<Expr> {
        let call = parse_call(f, self.params)?;
        let mut args = Vec::with_capacity(call.args.len());
        for a in &call.args {
            if calls_in(a) > 0 {
                return Err(err(format!("{f}: an input of llm() is not another call of it; call one function of the other's column in a query over its rows")));
            }
            args.push(self.column(a));
        }
        self.calls.push(Call { args, ..call });
        Ok(Expr::Identifier(Ident::new(answer_name(self.calls.len() - 1))))
    }

    /// A classifier's call: its value's name, its vector a column.
    fn classify(&mut self, f: &sql::Function) -> Result<Expr> {
        let (x, c, returns) = super::classify::parse_call(f, self.params)?;
        if calls_in(&x) > 0 {
            return Err(err(format!("{f}: the vector of classify() is not another call")));
        }
        let arg = self.column(&x);
        self.calls.push(Call { classifier: Some((c, returns)), model: super::classify::NAME.into(), args: vec![arg], returns: Returns::Text, max_tokens: 0 });
        Ok(Expr::Identifier(Ident::new(answer_name(self.calls.len() - 1))))
    }
}

/// A string literal's text.
fn string_of(e: &Expr) -> Option<String> {
    match e {
        Expr::Value(v) => match &v.value {
            sql::Value::SingleQuotedString(s) | sql::Value::DoubleQuotedString(s) => Some(s.clone()),
            _ => None,
        },
        Expr::Nested(x) => string_of(x),
        _ => None,
    }
}

/// A call of [`NAME`], its arguments as written ([`Call::args`] the
/// instructions and the inputs). `params` gives the options' values when
/// they are parameters.
fn parse_call(f: &sql::Function, params: &Params) -> Result<Call> {
    if f.over.is_some() || f.filter.is_some() || !f.within_group.is_empty() {
        return Err(err(format!("{f}: OVER, FILTER and WITHIN GROUP are for aggregates and window functions; {USAGE}")));
    }
    let FunctionArguments::List(l) = &f.args else { return Err(err(USAGE)) };
    if l.duplicate_treatment.is_some() || !l.clauses.is_empty() {
        return Err(err(USAGE));
    }
    let mut positional: Vec<Expr> = Vec::new();
    let mut options: Vec<(String, Expr)> = Vec::new();
    for a in &l.args {
        match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) if options.is_empty() => positional.push(e.clone()),
            FunctionArg::Unnamed(_) => return Err(err(format!("{f}: an input follows an option; {USAGE}"))),
            FunctionArg::Named { name, arg: FunctionArgExpr::Expr(e), .. } => options.push((name.value.to_ascii_lowercase(), e.clone())),
            FunctionArg::ExprNamed { name: Expr::Identifier(name), arg: FunctionArgExpr::Expr(e), .. } => options.push((name.value.to_ascii_lowercase(), e.clone())),
            _ => return Err(err(USAGE)),
        }
    }
    let [model, rest @ ..] = positional.as_slice() else { return Err(err(USAGE)) };
    let model = string_of(model).ok_or_else(|| err(format!("{f}: the model is a string, such as 'anthropic/claude-haiku-5-5', not {model}; {USAGE}")))?;
    if model.trim().is_empty() {
        return Err(err(format!("{f}: the model is named; {USAGE}")));
    }
    // A name, never a path: the model is read from the models directory.
    if !super::model::is_model_path(&model) {
        return Err(err(format!("{f}: the model is a provider's model (anthropic/<model>) or a directory of the models directory, its name letters, digits, _, - and . (two such names joined by / at most), not {model:?}")));
    }
    if rest.len() < 2 {
        return Err(err(format!("{f}: the instructions and at least one input follow the model; {USAGE}")));
    }
    let option = |e: &Expr| -> Result<Value> { compound::value_of(e, params) };
    let (mut returns, mut choices, mut max_tokens) = (None, None, None);
    for (name, e) in &options {
        let v = option(e)?;
        match name.as_str() {
            "choices" => {
                let Value::Str(s) = &v else { return Err(err(format!("{f}: choices is a string of them apart by commas, 'positive,negative,mixed'"))) };
                let cs: Vec<String> = s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
                if cs.len() < 2 {
                    return Err(err(format!("{f}: choices names two or more, apart by commas")));
                }
                choices = Some(cs);
            }
            "returns" => {
                returns = Some(match &v {
                    Value::Str(s) if s.eq_ignore_ascii_case("text") => Returns::Text,
                    Value::Str(s) if ["number", "float", "int"].iter().any(|n| s.eq_ignore_ascii_case(n)) => Returns::Number,
                    Value::Str(s) if s.eq_ignore_ascii_case("bool") => Returns::Bool,
                    v => return Err(err(format!("{f}: returns is 'text', 'number' or 'bool', not {}", crate::query::hint::sql_value(v)))),
                })
            }
            "max_tokens" => match v {
                Value::Int(n) if (1..=8192).contains(&n) => max_tokens = Some(n as u64),
                v => return Err(err(format!("{f}: max_tokens is a whole number from 1 to 8192, not {}", crate::query::hint::sql_value(&v)))),
            },
            n => return Err(err(format!("{f}: llm() has no option {n}; {USAGE}"))),
        }
    }
    let returns = match (choices, returns) {
        (Some(_), Some(r)) if r != Returns::Text => return Err(err(format!("{f}: choices gives one of them, which is text: leave returns out"))),
        (Some(cs), _) => Returns::Choice(cs),
        (None, r) => r.unwrap_or(Returns::Text),
    };
    let max_tokens = max_tokens.unwrap_or(if returns == Returns::Text { TEXT_TOKENS } else { SHORT_TOKENS });
    Ok(Call { classifier: None, model, args: rest.to_vec(), returns, max_tokens })
}

/// Checks the calls of [`NAME`] in `e` (a stored function's body) as a
/// query would, so a function is refused when it is made rather than
/// when it is called: their models are strings, their options known, and
/// no call reads another.
pub fn check(e: &Expr) -> Result<()> {
    if calls_in(e) == 0 {
        return Ok(());
    }
    let params = Params::new();
    let mut b = Builder { hidden: Vec::new(), calls: Vec::new(), params: &params };
    b.build(e).map(drop)
}

/// The two steps of a query that calls a model ([module docs](self)), its
/// stored functions written out; `None` for one that calls none.
pub fn split(q: &sql::Query, functions: &FunctionSet, params: &Params) -> Result<Option<Split>> {
    let expanded;
    let q = match functions.calls_llm() {
        true => match super::function::expand(q, functions)? {
            Some((x, _)) => {
                expanded = x;
                // The columns keep the names the query gave them.
                &rename_as(q, expanded)
            }
            None => q,
        },
        false => q,
    };
    // A classifier's calls of a value of no row, computed first.
    let mut folded_q;
    let mut folded = 0;
    let q = match calls_named(q, &[super::classify::NAME]) > 0 {
        true => {
            folded_q = q.clone();
            folded = super::classify::fold(&mut folded_q, params)?;
            &folded_q
        }
        false => q,
    };
    let total = calls_in(q);
    if total == 0 {
        return Ok((folded > 0).then(|| Split::folded(q.clone(), folded)));
    }
    let llms = calls_named(q, &[NAME]);
    let where_ = if llms == 0 { CLASSIFY_WHERE } else { WHERE };
    let sql::SetExpr::Select(s) = q.body.as_ref() else { return Err(err(where_)) };
    if q.fetch.is_some() || !q.locks.is_empty() {
        return Err(err(where_));
    }
    let order: Vec<sql::OrderByExpr> = match &q.order_by {
        None => Vec::new(),
        Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), interpolate: None }) => es.clone(),
        Some(_) => return Err(err(where_)),
    };
    // The calls a query may make: in its columns and its ORDER BY.
    let mut allowed = s.projection.iter().map(|i| match i {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => calls_in(e),
        _ => 0,
    }).sum::<usize>();
    // An ORDER BY of a column by name or position orders by its value.
    let items: Vec<(Option<String>, Option<&Expr>)> = s
        .projection
        .iter()
        .map(|i| match i {
            SelectItem::UnnamedExpr(e) => (None, Some(e)),
            SelectItem::ExprWithAlias { expr, alias } => (Some(alias.value.clone()), Some(expr)),
            _ => (None, None),
        })
        .collect();
    let mut keys: Vec<(Expr, bool, Option<bool>)> = Vec::with_capacity(order.len());
    for o in &order {
        let named = match &o.expr {
            Expr::Identifier(id) => items.iter().find(|(a, _)| a.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(&id.value))).and_then(|(_, e)| e.cloned()),
            Expr::Value(v) => match &v.value {
                sql::Value::Number(n, _) => n.parse::<usize>().ok().and_then(|i| items.get(i.wrapping_sub(1))).and_then(|(_, e)| e.cloned()),
                _ => None,
            },
            _ => None,
        };
        if named.is_none() {
            allowed += calls_in(&o.expr);
        }
        keys.push((named.unwrap_or_else(|| o.expr.clone()), o.options.asc == Some(false), o.options.nulls_first));
    }
    if allowed != total {
        return Err(err(where_));
    }
    let late_order = keys.iter().any(|(k, _, _)| calls_in(k) > 0);
    let mut limits = limits();
    if llms == 0 {
        limits.rows = super::classify::MAX_CALL_ROWS;
    }
    let mut b = Builder { hidden: Vec::new(), calls: Vec::new(), params };
    let mut first = q.clone();
    let sql::SetExpr::Select(fs) = first.body.as_mut() else { unreachable!("a SELECT") };
    let mut projection = Vec::new();
    let mut out = Vec::new();
    let mut wildcards = 0;
    for item in &s.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } if calls_in(e) > 0 || (late_order && is_score(e)) => {
                let name = match item {
                    SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
                    _ => super::sql::name_of(e),
                };
                match calls_in(e) > 0 {
                    true => out.push(Item::Made { name, expr: b.build(e)? }),
                    false => out.push(Item::Score { name }),
                }
            }
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {
                wildcards += 1;
                projection.push(item.clone());
                out.push(Item::Pass { wildcard: true });
            }
            _ => {
                projection.push(item.clone());
                out.push(Item::Pass { wildcard: false });
            }
        }
    }
    if wildcards > 1 {
        return Err(err("a query that calls llm() selects one * at most"));
    }
    let mut order_keys = Vec::new();
    if late_order {
        for (k, desc, nulls_first) in &keys {
            order_keys.push((b.build(k)?, *desc, *nulls_first));
        }
    }
    for (i, h) in b.hidden.iter().enumerate() {
        projection.push(SelectItem::ExprWithAlias { expr: h.clone(), alias: Ident::new(hidden_name(i)) });
    }
    if projection.is_empty() {
        return Err(err("a query that calls llm() reads something of its rows"));
    }
    fs.projection = projection;
    let (offset, limit) = compound::limits(&q.limit_clause, params)?;
    // Rows past the limit are not fetched: one more says there are more.
    let past = || Some(sql::LimitClause::LimitOffset { limit: Some(Expr::Value(sql::Value::Number((limits.rows + 1).to_string(), false).into())), offset: None, limit_by: Vec::new() });
    let mut set_past = false;
    if late_order {
        first.order_by = None;
        first.limit_clause = past();
        set_past = true;
    } else if limit.is_none() && offset == 0 {
        first.limit_clause = past();
        set_past = true;
    }
    Ok(Some(Split { first, hidden: b.hidden.len(), calls: b.calls, items: out, order: order_keys, offset, limit, rows: limits.rows, past: set_past, folded }))
}

/// [`split`] of the query of text `sql`: `None` when it calls no model.
pub fn split_sql(sql: &str, functions: &FunctionSet, params: &Params) -> Result<Option<Split>> {
    if !may_call(sql, functions) {
        return Ok(None);
    }
    // A text that does not parse fails where it would without a call.
    let Ok(p) = super::sql::parse_query_cached(sql) else { return Ok(None) };
    split(&p.query, functions, params)
}

/// `expanded`, its SELECT items named as `q`'s were before its stored
/// functions were written out (`summarize(body)`, not the body).
fn rename_as(q: &sql::Query, mut expanded: sql::Query) -> sql::Query {
    if let (sql::SetExpr::Select(a), sql::SetExpr::Select(b)) = (q.body.as_ref(), expanded.body.as_mut()) {
        if a.projection.len() == b.projection.len() {
            for (x, y) in a.projection.iter().zip(b.projection.iter_mut()) {
                if let (SelectItem::UnnamedExpr(before), SelectItem::UnnamedExpr(after)) = (x, &*y) {
                    if before != after && calls_in(after) > 0 {
                        *y = SelectItem::ExprWithAlias { expr: after.clone(), alias: Ident::with_quote('"', super::sql::name_of(before)) };
                    }
                }
            }
        }
    }
    expanded
}

/// Whether `e` is `score()`.
fn is_score(e: &Expr) -> bool {
    let Expr::Function(f) = e else { return false };
    f.name.to_string().eq_ignore_ascii_case("score") && (matches!(&f.args, FunctionArguments::None) || matches!(&f.args, FunctionArguments::List(l) if l.args.is_empty()))
}

/// Answers kept in memory, by question.
struct Cache {
    answers: HashMap<[u8; 32], String>,
    order: VecDeque<[u8; 32]>,
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Cache { answers: HashMap::new(), order: VecDeque::new() }))
}

/// A caller's own count of the questions a query asks of models
/// ([`Split::finish_within`]): given how many are about to be asked, an
/// error refuses them.
pub type Budget<'a> = &'a (dyn Fn(u64) -> Result<()> + Sync);

/// Requests made in the last minute, for [`Limits::per_minute`].
fn spent() -> &'static Mutex<VecDeque<(Instant, u64)>> {
    static S: OnceLock<Mutex<VecDeque<(Instant, u64)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Takes `n` requests of the minute's budget, or says when there will be
/// room for them.
fn take_budget(n: u64, per_minute: Option<u64>) -> Result<()> {
    let Some(max) = per_minute else { return Ok(()) };
    let mut s = spent().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    while s.front().is_some_and(|(t, _)| now.duration_since(*t).as_secs() >= 60) {
        s.pop_front();
    }
    let used: u64 = s.iter().map(|(_, k)| k).sum();
    if used + n > max {
        let wait = s.front().map_or(60, |(t, _)| 60 - now.duration_since(*t).as_secs().min(59));
        return Err(Error::Unavailable(format!("llm(): this query asks {n} questions of models, and {} of this process's {max} a minute are left; try again in {wait} s, or ask about fewer rows", max.saturating_sub(used))));
    }
    s.push_back((now, n));
    Ok(())
}

/// One question to a model.
#[derive(Clone, Debug)]
struct Question {
    model: String,
    system: String,
    user: String,
    max_tokens: u64,
}

impl Question {
    fn key(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        for part in [self.model.as_str(), self.system.as_str(), self.user.as_str(), &self.max_tokens.to_string()] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        h.finalize().into()
    }
}

/// The answers to `questions`, in order: from memory, or asked of their
/// models, each model's `concurrency` at once; and how many were asked.
fn answer(questions: &[Question], models: &Path, budget: Option<Budget<'_>>) -> Result<(Vec<String>, u64)> {
    let keys: Vec<[u8; 32]> = questions.iter().map(Question::key).collect();
    let mut out: Vec<Option<String>> = {
        let c = cache().lock().unwrap_or_else(|e| e.into_inner());
        keys.iter().map(|k| c.answers.get(k).cloned()).collect()
    };
    // Each distinct question once.
    let mut ask: Vec<usize> = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if out[i].is_none() && !ask.iter().any(|&j| keys[j] == *k) {
            ask.push(i);
        }
    }
    CACHED.fetch_add((questions.len() - ask.len()) as u64, Ordering::Relaxed);
    if !ask.is_empty() {
        take_budget(ask.len() as u64, limits().per_minute)?;
        if let Some(b) = budget {
            b(ask.len() as u64)?;
        }
        let asked = ask_models(&ask.iter().map(|&i| &questions[i]).collect::<Vec<_>>(), models)?;
        let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
        for (&i, a) in ask.iter().zip(asked) {
            if c.answers.insert(keys[i], a.clone()).is_none() {
                c.order.push_back(keys[i]);
            }
            while c.order.len() > CACHE_ENTRIES {
                if let Some(k) = c.order.pop_front() {
                    c.answers.remove(&k);
                }
            }
            out[i] = Some(a);
        }
        drop(c);
        for (i, k) in keys.iter().enumerate() {
            if out[i].is_none() {
                let j = ask.iter().copied().find(|&j| keys[j] == *k).expect("asked");
                out[i] = out[j].clone();
            }
        }
    }
    Ok((out.into_iter().map(|a| a.unwrap_or_default()).collect(), ask.len() as u64))
}

#[cfg(feature = "hosted")]
fn ask_models(questions: &[&Question], models: &Path) -> Result<Vec<String>> {
    use crate::hosted::Generator;
    let mut opened: HashMap<&str, Generator> = HashMap::new();
    for q in questions {
        if !opened.contains_key(q.model.as_str()) {
            let g = Generator::open_for_queries(&q.model, models).map_err(|e| err(format!("llm(): {e}")))?;
            opened.insert(q.model.as_str(), g);
        }
    }
    let mut out: Vec<Option<Result<String>>> = questions.iter().map(|_| None).collect();
    for (model, g) in &opened {
        let mine: Vec<usize> = (0..questions.len()).filter(|&i| questions[i].model == *model).collect();
        for round in mine.chunks(g.concurrency().max(1)) {
            let got: Vec<Result<String>> = std::thread::scope(|s| {
                let hs: Vec<_> = round.iter().map(|&i| s.spawn(move || g.ask(&questions[i].system, &questions[i].user, questions[i].max_tokens))).collect();
                hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(Error::Unavailable("a model's request panicked".into())))).collect()
            });
            for (&i, r) in round.iter().zip(got) {
                REQUESTS.fetch_add(1, Ordering::Relaxed);
                if r.is_err() {
                    FAILURES.fetch_add(1, Ordering::Relaxed);
                }
                out[i] = Some(r);
            }
        }
    }
    out.into_iter().map(|r| r.unwrap_or_else(|| Err(err("a question was not asked")))).collect()
}

#[cfg(not(feature = "hosted"))]
fn ask_models(_: &[&Question], _: &Path) -> Result<Vec<String>> {
    Err(err("llm(): this build of ChloeDB has no models behind an API (feature hosted)"))
}

/// A value as an input's text: `NULL` none.
fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Str(s) => Some(s.clone()),
        Value::List(vs) => {
            let parts: Vec<String> = vs.iter().filter_map(text_of).collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        v => Some(crate::query::hint::sql_value(v).trim_matches('\'').to_string()),
    }
}

impl Split {
    /// A query whose classifier calls were all of values of no row, `q`
    /// with them computed: its first step is the whole query.
    fn folded(q: sql::Query, folded: usize) -> Split {
        Split { first: q, hidden: 0, calls: Vec::new(), items: Vec::new(), order: Vec::new(), offset: 0, limit: None, rows: usize::MAX, past: false, folded }
    }

    /// Whether the query called a classifier on values of no row alone,
    /// computed: its first step is the whole query, and
    /// [`Split::finish`] gives its rows as they are.
    pub fn is_folded(&self) -> bool {
        self.calls.is_empty()
    }

    /// The plan's line of the calls computed before planning.
    pub fn folded_line(&self) -> String {
        format!("classify: {} call{} of a parameter computed before planning", self.folded, if self.folded == 1 { "" } else { "s" })
    }

    /// The first step as SQL text, for a server that sends it on (to its
    /// shards, or to its own query path): the text parses back to the
    /// very query [`Self::first`] is, or this fails, so that what runs is
    /// what was split and never a different reading of printed text.
    pub fn first_sql(&self) -> Result<String> {
        let text = self.first.to_string();
        match super::sql::parse_query(&text) {
            Ok(q) if *q == self.first => Ok(text),
            _ => Err(Error::Query(format!("llm(): the query without its calls does not print as SQL that reads back the same ({text}): write it another way"))),
        }
    }

    /// The models the calls name, each once.
    pub fn models(&self) -> Vec<&str> {
        let mut ms: Vec<&str> = Vec::new();
        for c in self.calls.iter().filter(|c| c.classifier.is_none()) {
            if !ms.contains(&c.model.as_str()) {
                ms.push(&c.model);
            }
        }
        ms
    }

    /// The answer of the query: `rs`, the first step's rows, with the
    /// calls' answers and the columns made of them, ordered and cut as the
    /// query says when its calls order the rows. `models` is the directory
    /// of the models' `endpoint.json` files.
    pub fn finish(&self, rs: ResultSet, params: &Params, models: &Path) -> Result<ResultSet> {
        self.finish_within(rs, params, models, None)
    }

    /// [`Self::finish`], each batch of questions asked of models (answers
    /// from memory not among them) first taken from `budget`: its error
    /// ends the query, and nothing more is asked.
    pub fn finish_within(&self, rs: ResultSet, params: &Params, models: &Path, budget: Option<Budget<'_>>) -> Result<ResultSet> {
        if self.calls.is_empty() {
            let mut rs = rs;
            rs.stats.plan.insert(0, self.folded_line());
            return Ok(rs);
        }
        let t0 = Instant::now();
        let ResultSet { columns, rows, keys, scores, parts: _, mut stats } = rs;
        let width = columns.len();
        let pass = width.checked_sub(self.hidden).ok_or_else(|| err("llm(): the first step lost its columns"))?;
        if rows.len() > self.rows {
            return Err(err(match self.order.is_empty() {
                true => format!("llm(): this query returns {} rows or more, and a query calls a model for {} at most: LIMIT it", rows.len(), self.rows),
                false => format!("llm() in ORDER BY ranks {} rows at most, and this query has more: rank the top rows of a first ranking (WITH top AS (SELECT * FROM t WHERE ... ORDER BY bm25(body, :q) DESC LIMIT 50) SELECT ... FROM top ORDER BY llm(...) DESC LIMIT 10)", self.rows),
            }));
        }
        // Expressions over a row of the first step and the calls' answers.
        let slot = |e: &Expr| -> Option<Result<ir::Expr>> {
            let Expr::Identifier(id) = e else { return None };
            let at = if let Some(n) = id.value.strip_prefix("__llm_answer") {
                width + n.parse::<usize>().ok()?
            } else {
                pass + id.value.strip_prefix("__llm")?.parse::<usize>().ok()?
            };
            Some(Ok(ir::Expr::Output(at as u32)))
        };
        let compile = |e: &Expr| compound::compile(e, params, &slot, &[]);
        let args: Vec<Vec<ir::Expr>> = self.calls.iter().map(|c| c.args.iter().map(compile).collect::<Result<_>>()).collect::<Result<_>>()?;
        let items: Vec<Option<ir::Expr>> = self.items.iter().map(|i| match i {
            Item::Made { expr, .. } => compile(expr).map(Some),
            _ => Ok(None),
        }).collect::<Result<_>>()?;
        let order: Vec<ir::Expr> = self.order.iter().map(|(k, _, _)| compile(k)).collect::<Result<_>>()?;
        let eval = |e: &ir::Expr, row: &[Value]| ir::eval::eval(e, &|r| match r {
            ir::eval::Ref::Output(i) => Ok(row.get(i as usize).cloned().unwrap_or(Value::Null)),
            r => Err(err(format!("llm(): {r:?} is not a column of the rows"))),
        });
        // A row's and a call's question, if it has an input.
        let mut questions: Vec<Vec<Option<Question>>> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut mine = Vec::with_capacity(self.calls.len());
            for (c, args) in self.calls.iter().zip(&args) {
                if c.classifier.is_some() {
                    mine.push(None);
                    continue;
                }
                let vals: Vec<Value> = args.iter().map(|a| eval(a, row)).collect::<Result<_>>()?;
                let instructions = text_of(&vals[0]).unwrap_or_default();
                let inputs: Vec<String> = vals[1..].iter().filter_map(text_of).filter(|t| !t.trim().is_empty()).collect();
                mine.push((!inputs.is_empty()).then(|| Question { model: c.model.clone(), system: c.system(&instructions), user: inputs.join("\n\n"), max_tokens: c.max_tokens }));
            }
            questions.push(mine);
        }
        // The answers a row's columns and keys read, asked as they are
        // found: a CASE reads the branch it takes, so a call in a branch
        // not taken asks nothing, and one that reads another's answer is
        // asked once that answer is known. Each round asks what the last
        // found, until a round finds nothing new.
        let wild = pass.saturating_sub(self.items.iter().filter(|i| matches!(i, Item::Pass { wildcard: false })).count());
        let mut answers: Vec<Vec<Option<Value>>> = questions.iter().map(|qs| qs.iter().map(|q| if q.is_none() { Some(Value::Null) } else { None }).collect()).collect();
        // A classifier's calls, computed here: each row's vector is a column.
        let t_classify = Instant::now();
        let mut classified = 0u64;
        for (k, (c, args)) in self.calls.iter().zip(&args).enumerate() {
            let Some((model, returns)) = &c.classifier else { continue };
            for (r, row) in rows.iter().enumerate() {
                let x = eval(&args[0], row)?;
                answers[r][k] = Some(model.value(&x, *returns)?);
                classified += 1;
            }
        }
        let classify_ms = t_classify.elapsed().as_secs_f64() * 1e3;
        let (mut asked, mut from_memory) = (0u64, 0u64);
        let made: Vec<(Vec<Value>, Vec<Value>)> = loop {
            let needed = std::cell::RefCell::new(Vec::<(usize, usize)>::new());
            let mut round: Vec<Result<(Vec<Value>, Vec<Value>)>> = Vec::with_capacity(rows.len());
            for (r, row) in rows.iter().enumerate() {
                let value = |e: &ir::Expr| ir::eval::eval(e, &|x| match x {
                    ir::eval::Ref::Output(i) if (i as usize) < width => Ok(row[i as usize].clone()),
                    ir::eval::Ref::Output(i) => {
                        let k = i as usize - width;
                        Ok(match answers[r].get(k).cloned().flatten() {
                            Some(v) => v,
                            None => {
                                needed.borrow_mut().push((r, k));
                                Value::Null
                            }
                        })
                    }
                    x => Err(err(format!("llm(): {x:?} is not a column of the rows"))),
                });
                round.push((|| {
                    let keys: Vec<Value> = order.iter().map(value).collect::<Result<_>>()?;
                    let mut out = Vec::with_capacity(self.items.len() + wild);
                    let mut at = 0;
                    for (item, x) in self.items.iter().zip(&items) {
                        match item {
                            Item::Pass { wildcard } => {
                                let n = if *wildcard { wild } else { 1 };
                                out.extend(row[at..at + n].iter().cloned());
                                at += n;
                            }
                            Item::Made { .. } => out.push(value(x.as_ref().expect("compiled"))?),
                            Item::Score { .. } => out.push(keys.first().cloned().unwrap_or(Value::Null)),
                        }
                    }
                    Ok((out, keys))
                })());
            }
            let mut needed = needed.into_inner();
            needed.sort_unstable();
            needed.dedup();
            if needed.is_empty() {
                break round.into_iter().collect::<Result<_>>()?;
            }
            let qs: Vec<Question> = needed.iter().map(|&(r, k)| questions[r][k].clone().expect("a question")).collect();
            let (texts, n) = answer(&qs, models, budget)?;
            asked += n;
            from_memory += qs.len() as u64 - n;
            for (&(r, k), t) in needed.iter().zip(texts) {
                answers[r][k] = Some(self.calls[k].read(&t));
            }
        };
        let mut made: Vec<(Vec<Value>, Vec<Value>, Value, f64, usize)> = made
            .into_iter()
            .enumerate()
            .map(|(r, (out, key_values))| {
                let score = match key_values.first() {
                    Some(Value::Float(f)) => *f,
                    Some(Value::Int(i)) => *i as f64,
                    Some(_) => 0.0,
                    None => scores.get(r).copied().unwrap_or(0.0),
                };
                (out, key_values, keys.get(r).cloned().unwrap_or(Value::Null), score, r)
            })
            .collect();
        if !self.order.is_empty() {
            made.sort_by(|a, b| {
                for (i, (_, desc, nulls_first)) in self.order.iter().enumerate() {
                    let (x, y) = (&a.1[i], &b.1[i]);
                    let nf = nulls_first.unwrap_or(false);
                    let o = match (matches!(x, Value::Null), matches!(y, Value::Null)) {
                        (true, true) => std::cmp::Ordering::Equal,
                        (true, false) => if nf { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater },
                        (false, true) => if nf { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less },
                        (false, false) => {
                            let o = ir::eval::compare(x, y).ok().flatten().unwrap_or(std::cmp::Ordering::Equal);
                            if *desc { o.reverse() } else { o }
                        }
                    };
                    if o != std::cmp::Ordering::Equal {
                        return o;
                    }
                }
                a.4.cmp(&b.4)
            });
            made = made.into_iter().skip(self.offset).take(self.limit.unwrap_or(usize::MAX)).collect();
        }
        let mut names = Vec::with_capacity(self.items.len() + wild);
        let mut at = 0;
        for item in &self.items {
            match item {
                Item::Pass { wildcard } => {
                    let n = if *wildcard { wild } else { 1 };
                    names.extend(columns[at..at + n].iter().cloned());
                    at += n;
                }
                Item::Made { name, .. } | Item::Score { name } => names.push(name.clone()),
            }
        }
        if classified > 0 || self.folded > 0 {
            stats.plan.push(format!("classify: {classified} value{} of rows in {classify_ms:.1} ms, {} of a parameter before planning", if classified == 1 { "" } else { "s" }, self.folded));
        }
        if self.calls.iter().any(|c| c.classifier.is_none()) {
        stats.plan.push(format!(
            "llm: {} asked about {} row{}: {} request{}, {} answer{} from memory, in {:.0} ms",
            self.models().join(", "),
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
            asked,
            if asked == 1 { "" } else { "s" },
            from_memory,
            if from_memory == 1 { "" } else { "s" },
            t0.elapsed().as_secs_f64() * 1e3,
        ));
        }
        let (mut out_rows, mut out_keys, mut out_scores) = (Vec::with_capacity(made.len()), Vec::with_capacity(made.len()), Vec::with_capacity(made.len()));
        for (row, _, key, score, _) in made {
            out_rows.push(row);
            out_keys.push(key);
            out_scores.push(score);
        }
        if keys.is_empty() {
            out_keys.clear();
        }
        if scores.is_empty() && self.order.is_empty() {
            out_scores.clear();
        }
        Ok(ResultSet { columns: names, rows: out_rows, keys: out_keys, scores: out_scores, parts: None, stats })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(sql: &str) -> Result<Call> {
        let q = super::super::sql::parse_query(&format!("SELECT {sql}")).unwrap();
        let sql::SetExpr::Select(s) = q.body.as_ref() else { panic!() };
        let SelectItem::UnnamedExpr(Expr::Function(f)) = &s.projection[0] else { panic!() };
        parse_call(f, &Params::new())
    }

    #[test]
    fn calls_read_their_options() {
        let c = call("llm('anthropic/claude-haiku-5-5', 'Mood?', body, choices => 'positive, negative,mixed')").unwrap();
        assert_eq!(c.returns, Returns::Choice(vec!["positive".into(), "negative".into(), "mixed".into()]));
        assert_eq!((c.model.as_str(), c.args.len(), c.max_tokens), ("anthropic/claude-haiku-5-5", 2, SHORT_TOKENS));
        assert_eq!(c.system("Mood?"), "Mood?\n\nAnswer with one of these, and nothing else: positive, negative, mixed.");
        let c = call("llm('m', 'Rate it', title, body, returns => 'number', max_tokens => 8)").unwrap();
        assert_eq!((c.returns, c.max_tokens, c.args.len()), (Returns::Number, 8, 3));
        assert_eq!(call("llm('m', 'Sum up', body)").unwrap().max_tokens, TEXT_TOKENS);
        for (bad, says) in [
            ("llm(body, 'x', body)", "the model is a string"),
            ("llm('m', 'x')", "at least one input"),
            ("llm('m', 'x', body, choices => 'one')", "two or more"),
            ("llm('m', 'x', body, returns => 'json')", "returns is"),
            ("llm('m', 'x', body, temperature => 1)", "no option temperature"),
            ("llm('m', 'x', body, choices => 'a,b', returns => 'number')", "leave returns out"),
            ("llm('m', 'x', body, max_tokens => 0)", "max_tokens"),
        ] {
            let e = call(bad).unwrap_err().to_string();
            assert!(e.contains(says), "{bad}: {e}");
        }
    }

    #[test]
    fn answers_read_as_what_the_call_returns() {
        let c = |r: Returns| Call { classifier: None, model: "m".into(), args: Vec::new(), returns: r, max_tokens: 1 };
        let choice = c(Returns::Choice(vec!["positive".into(), "negative".into(), "very mixed".into()]));
        for (a, want) in [("Positive.", Some("positive")), ("  NEGATIVE ", Some("negative")), ("It is very mixed, I think", Some("very mixed")), ("positive or negative", None), ("unclear", None)] {
            assert_eq!(choice.read(a), want.map_or(Value::Null, |w| Value::Str(w.into())), "{a}");
        }
        let n = c(Returns::Number);
        for (a, want) in [("7", Some(7.0)), ("Score: 3.5/10", Some(3.5)), ("-2 points", Some(-2.0)), ("none", None), ("v2", Some(2.0))] {
            assert_eq!(n.read(a), want.map_or(Value::Null, Value::Float), "{a}");
        }
        let b = c(Returns::Bool);
        for (a, want) in [("Yes.", Some(true)), ("no, it is not", Some(false)), ("TRUE", Some(true)), ("maybe", None)] {
            assert_eq!(b.read(a), want.map_or(Value::Null, Value::Bool), "{a}");
        }
        assert_eq!(c(Returns::Text).read("  A summary.\n"), Value::Str("A summary.".into()));
    }

    #[test]
    fn calls_are_counted_by_their_tokens() {
        let q = |s: &str| super::super::sql::parse_query(s).unwrap();
        assert_eq!(calls_in(&q("SELECT llm('m', 'llm(', body), LLM ('m','x',t) FROM t WHERE x = 'llm(1)'")), 2);
        assert_eq!(calls_in(&q("SELECT \"llm\"(x), llm_score FROM t")), 0);
    }

    #[test]
    fn a_split_finds_the_rows_and_reads_what_the_calls_need() {
        let q = |s: &str| super::super::sql::parse_query(s).unwrap();
        let none = split(&q("SELECT id FROM t LIMIT 5"), &FunctionSet::default(), &Params::new()).unwrap();
        assert!(none.is_none());
        let s = split(
            &q("SELECT id, CASE WHEN length(body) < 200 THEN body ELSE llm('m', 'Sum up', body) END AS s, * FROM t WHERE MATCH(body, 'x') ORDER BY bm25(body, 'x') DESC LIMIT 5"),
            &FunctionSet::default(),
            &Params::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.first.to_string(), "SELECT id, *, length(body) < 200 AS __llm0, body AS __llm1 FROM t WHERE MATCH(body, 'x') ORDER BY bm25(body, 'x') DESC LIMIT 5");
        // A server sends the first step on as text: it reads back the same.
        assert_eq!(s.first_sql().unwrap(), s.first.to_string());
        assert!(s.order.is_empty());
        let s = split(&q("WITH top AS (SELECT * FROM t ORDER BY bm25(body, :q) DESC LIMIT 20) SELECT id, score() FROM top ORDER BY llm('m', 'Grade it', :q, body, returns => 'number') DESC LIMIT 3"), &FunctionSet::default(), &Params::new()).unwrap().unwrap();
        assert_eq!(s.first.to_string(), "WITH top AS (SELECT * FROM t ORDER BY bm25(body, :q) DESC LIMIT 20) SELECT id, body AS __llm0 FROM top LIMIT 101");
        assert_eq!((s.order.len(), s.limit, s.offset), (1, Some(3), 0));
        assert!(s.first_sql().is_ok());
        // A first step that would print as other SQL than it is (here a
        // sum built without its parentheses) is refused, not run as read.
        let mut odd = s.clone();
        let sum = |l: Expr, r: Expr| Expr::BinaryOp { left: Box::new(l), op: sql::BinaryOperator::Plus, right: Box::new(r) };
        let n = |v: &str| Expr::Value(sql::Value::Number(v.into(), false).into());
        let product = Expr::BinaryOp { left: Box::new(sum(n("1"), n("2"))), op: sql::BinaryOperator::Multiply, right: Box::new(n("3")) };
        if let sql::SetExpr::Select(sel) = odd.first.body.as_mut() {
            sel.projection.push(SelectItem::UnnamedExpr(product));
        }
        assert!(odd.first.to_string().contains("1 + 2 * 3"), "{}", odd.first);
        assert!(odd.first_sql().unwrap_err().to_string().contains("reads back the same"));
        for bad in [
            "SELECT id FROM t WHERE llm('m', 'x', body) = 'yes'",
            "WITH a AS (SELECT llm('m', 'x', body) AS s FROM t) SELECT * FROM a",
            "SELECT id FROM t GROUP BY llm('m', 'x', body)",
            "SELECT id, (SELECT llm('m', 'x', body) FROM t LIMIT 1) FROM t",
            "SELECT id FROM t UNION SELECT llm('m', 'x', body) FROM t",
        ] {
            let e = split(&q(bad), &FunctionSet::default(), &Params::new()).unwrap_err().to_string();
            assert!(e.contains("not part of WHERE"), "{bad}: {e}");
        }
        let e = split(&q("SELECT llm('m', 'x', llm('m', 'y', body)) FROM t"), &FunctionSet::default(), &Params::new()).unwrap_err().to_string();
        assert!(e.contains("not another call"), "{e}");
    }
}
