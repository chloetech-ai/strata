//! Stored functions: functions kept in the database, a row per version in
//! the system table `_functions` ([`crate::storage::FUNCTIONS`]), and
//! called from any query as a built-in is.
//!
//! ```sql
//! CREATE FUNCTION boosted(s FLOAT, importance FLOAT, w FLOAT DEFAULT 0.2)
//!   RETURNS FLOAT AS :s * (1 + :w * :importance);
//! SELECT id FROM memories WHERE MATCH(body, :q)
//!   ORDER BY boosted(bm25(body, :q), importance) DESC LIMIT 10;
//!
//! CREATE FUNCTION recall(q TEXT, k INT DEFAULT 10) RETURNS TABLE AS
//!   SELECT id, body FROM memories WHERE MATCH(body, :q)
//!   ORDER BY bm25(body, :q) DESC LIMIT :k;
//! SELECT * FROM recall('tokio runtime', k => 5);
//!
//! CREATE OR REPLACE FUNCTION boosted FROM VERSION 1;  -- a rollback
//! DROP FUNCTION [IF EXISTS] boosted [VERSION n];
//! ```
//!
//! A function's body is SQL: an expression of its parameters for one that
//! returns a value, a `SELECT` for one that `RETURNS TABLE`. Parameters
//! are `:name` in the body, as in rank profiles, so a body runs as written
//! as a plain query with the same parameters. A scalar body reads only its
//! parameters, never a column.
//!
//! **Calls are written out.** Binding replaces each call with its body,
//! the arguments in place of the parameters ([`expand`]): a scalar call
//! becomes an expression, a table function a subquery in `FROM` (or the
//! whole query, when the query is nothing but `SELECT * FROM f(..)`). The
//! plan is the one of the query written out, so indexes, ranking and the
//! shards' pushdown apply, and neither the plan nor the shard protocol
//! knows functions exist.
//!
//! **Versions never change.** `CREATE` writes version 1; `CREATE OR
//! REPLACE` the next one (or nothing, when the definition is the current
//! one's); `DROP FUNCTION` a tombstone version; `CREATE OR REPLACE
//! FUNCTION f FROM VERSION n` a new version with version n's definition.
//! Version numbers keep rising after a drop, so `f@2` always means one
//! body. A query calls the current version, the highest that is not a
//! tombstone, or pins one with a quoted name: `"f@2"(x)`. `DROP FUNCTION
//! f VERSION n` deletes an old version. `AT VERSION n` writes a given
//! version, as a coordinator does on its shards so that every shard
//! numbers alike ([`Statement::sql`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlparser::ast::{self as sql, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, SelectItem, TableFactor};
use sqlparser::dialect::GenericDialect;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};

use crate::error::{Error, Result};
use crate::value::Value;

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// How `CREATE FUNCTION` is written.
pub const CREATE_FORM: &str = "CREATE [OR REPLACE] FUNCTION name(parameter TYPE [DEFAULT value], ...) RETURNS TYPE | RETURNS TABLE [LANGUAGE sql] [OPTIONS (description = '...')] AS body";
/// How `DROP FUNCTION` is written.
pub const DROP_FORM: &str = "DROP FUNCTION [IF EXISTS] name [VERSION n]";

/// How deeply calls may nest in one another's bodies.
pub const MAX_DEPTH: usize = 16;

/// The parameter and return types a function may name.
const TYPES: [&str; 8] = ["INT", "FLOAT", "TEXT", "KEYWORD", "BOOL", "TIMESTAMP", "JSON", "VECTOR"];

/// Names the binder gives a meaning of its own, which no stored function
/// takes.
pub(crate) const RESERVED: &[&str] = &[
    "abs", "approx_percentile", "array_agg", "at_least", "avg", "bm25", "ceil", "ceiling", "char_length", "character_length", "clamp", "classify", "coalesce", "concat", "corr", "count", "count_true", "covar_pop", "covar_samp", "cross_encoder", "cume_dist", "date", "date_add",
    "date_bin", "date_format", "date_part", "date_sub", "date_trunc", "datediff", "datepart", "datetrunc", "day", "dayofweek", "decay", "dense_rank", "distance", "dot", "embed", "encode", "exp", "extract", "features", "first_value",
    "floor", "gbdt", "greatest", "group_concat", "hash", "hops", "hour", "ifnull", "instr", "isnull", "json_extract", "lag", "last_value", "lcase", "lead", "least", "left", "len", "length", "llm", "ln", "log", "lower",
    "ltrim", "match", "max", "maxsim", "min", "minmax", "month", "now", "nth_value", "ntile", "nvl", "path", "percent_rank", "percentile_cont", "percentile_disc", "position", "pow", "power", "ppr", "prefix", "rand",
    "random", "rank", "rank_profile", "ranked", "reachable", "replace", "reverse", "right", "round", "row_number", "rrf", "rtrim", "score", "seq_no", "sid_at", "sigmoid", "similarity", "sparse", "split_part",
    "sqrt", "stddev", "stddev_pop", "stddev_samp", "strftime", "string_agg", "strpos", "substr", "substring", "sum", "time_bucket", "tmm", "to_char", "trim", "trunc", "truncate", "ucase", "upper", "var_pop", "var_samp", "variance", "week", "weekday", "year", "zscore",
];

/// A parameter: its name (lower case), type, and default as a SQL literal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// What a function returns: a value, or rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Scalar,
    Table,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Scalar => "scalar",
            Kind::Table => "table",
        }
    }
}

/// A function as `CREATE FUNCTION` defines it, without its version.
#[derive(Clone, Debug, PartialEq)]
pub struct Definition {
    /// Lower case.
    pub name: String,
    pub kind: Kind,
    pub params: Vec<Param>,
    /// A type of [`TYPES`] (`VECTOR(n)`, `KEYWORD[]` and `INT[]` too), or
    /// `TABLE`.
    pub returns: String,
    /// The body as the parser prints it: an expression, or a `SELECT`.
    pub body: String,
    pub description: Option<String>,
}

/// The JSON of a definition's `signature` column.
#[derive(Serialize, Deserialize)]
struct Signature {
    params: Vec<Param>,
    returns: String,
}

impl Definition {
    /// SHA-256 of the definition (language, kind, parameters, return
    /// type, body and description), as hex: two versions with one hash
    /// define the same function.
    pub fn hash(&self) -> String {
        let j = serde_json::json!({"language": "sql", "kind": self.kind.name(), "params": self.params, "returns": self.returns, "body": self.body, "description": self.description});
        hex(&Sha256::digest(j.to_string().as_bytes()))
    }

    fn signature_json(&self) -> String {
        serde_json::to_string(&Signature { params: self.params.clone(), returns: self.returns.clone() }).expect("a signature serialises")
    }

    /// `name(p TYPE [DEFAULT v], ...)`.
    pub fn signature(&self) -> String {
        let ps: Vec<String> = self.params.iter().map(|p| format!("{} {}{}", p.name, p.ty, p.default.as_ref().map_or(String::new(), |d| format!(" DEFAULT {d}")))).collect();
        format!("{}({})", self.name, ps.join(", "))
    }

    /// The statement that defines it: `CREATE FUNCTION ... AS body`.
    pub fn sql(&self, replace: bool, at: Option<i64>) -> String {
        let mut s = format!("CREATE {}FUNCTION {} RETURNS {} LANGUAGE sql", if replace { "OR REPLACE " } else { "" }, self.signature(), self.returns);
        if let Some(d) = &self.description {
            s.push_str(&format!(" OPTIONS (description = {})", quote(d)));
        }
        if let Some(n) = at {
            s.push_str(&format!(" AT VERSION {n}"));
        }
        s.push_str(" AS ");
        s.push_str(&self.body);
        s
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The body of a tombstone's hash.
const DROPPED: &str = "dropped";

/// One version of a stored function: a row of `_functions`.
#[derive(Clone, Debug)]
pub struct Function {
    pub def: Definition,
    pub version: i64,
    pub hash: String,
    /// A tombstone: `DROP FUNCTION` wrote it.
    pub dropped: bool,
    /// When it was written, in microseconds since the epoch.
    pub created: Option<i64>,
    /// Who wrote it (a server's key name).
    pub created_by: Option<String>,
    /// The body, parsed once.
    body: Body,
}

#[derive(Clone, Debug)]
enum Body {
    None,
    Expr(Box<Expr>),
    Query(Box<sql::Query>),
}

impl PartialEq for Function {
    fn eq(&self, o: &Function) -> bool {
        self.def == o.def && self.version == o.version && self.hash == o.hash && self.dropped == o.dropped
    }
}

impl Function {
    /// Version `version` of `def`, its body parsed.
    pub fn new(def: Definition, version: i64, created: Option<i64>, created_by: Option<String>) -> Result<Function> {
        let body = parse_body(def.kind, &def.body)?;
        Ok(Function { hash: def.hash(), def, version, dropped: false, created, created_by, body })
    }

    /// The tombstone `DROP FUNCTION` writes as version `version` of `of`.
    pub fn tombstone(of: &Definition, version: i64, created: Option<i64>, created_by: Option<String>) -> Function {
        let def = Definition { body: String::new(), description: None, ..of.clone() };
        Function { def, version, hash: hex(&Sha256::digest(DROPPED.as_bytes())), dropped: true, created, created_by, body: Body::None }
    }

    /// Its key in `_functions`: `name@version`.
    pub fn id(&self) -> String {
        format!("{}@{}", self.def.name, self.version)
    }

    /// Its row of `_functions`, in the table's column order
    /// ([`crate::storage::functions_schema`]).
    pub fn to_row(&self) -> Vec<Value> {
        let opt = |s: &Option<String>| s.clone().map_or(Value::Null, Value::Str);
        vec![
            Value::Str(self.id()),
            Value::Str(self.def.name.clone()),
            Value::Int(self.version),
            Value::Str("sql".into()),
            Value::Str(self.def.kind.name().into()),
            Value::Str(self.def.signature_json()),
            if self.dropped { Value::Null } else { Value::Str(self.def.body.clone()) },
            Value::Str(self.hash.clone()),
            opt(&self.def.description),
            self.created.map_or(Value::Null, Value::Timestamp),
            opt(&self.created_by),
            Value::Bool(self.dropped),
        ]
    }

    /// A row of `_functions`, by column name.
    pub fn from_row(row: &BTreeMap<String, Value>) -> Result<Function> {
        let text = |c: &str| match row.get(c) {
            Some(Value::Str(s)) => Some(s.clone()),
            _ => None,
        };
        let bad = |what: &str| Error::Corrupt(format!("a row of _functions has no valid {what}: {row:?}"));
        let name = text("name").ok_or_else(|| bad("name"))?;
        let version = row.get("version").and_then(Value::as_i64).ok_or_else(|| bad("version"))?;
        let kind = match text("kind").as_deref() {
            Some("scalar") => Kind::Scalar,
            Some("table") => Kind::Table,
            _ => return Err(bad("kind")),
        };
        let sig: Signature = serde_json::from_str(&text("signature").ok_or_else(|| bad("signature"))?).map_err(|_| bad("signature"))?;
        let dropped = matches!(row.get("dropped"), Some(Value::Bool(true)) | Some(Value::Int(1)));
        let def = Definition { name, kind, params: sig.params, returns: sig.returns, body: text("body").unwrap_or_default(), description: text("description") };
        let created = match row.get("created") {
            Some(Value::Timestamp(t)) | Some(Value::Int(t)) => Some(*t),
            _ => None,
        };
        let hash = text("hash").ok_or_else(|| bad("hash"))?;
        let body = match dropped {
            true => Body::None,
            false => parse_body(def.kind, &def.body)?,
        };
        Ok(Function { def, version, hash, dropped, created, created_by: text("created_by"), body })
    }
}

/// The functions of a database: every version of each, by name.
#[derive(Clone, Debug, Default)]
pub struct FunctionSet {
    by_name: BTreeMap<String, Vec<Arc<Function>>>,
    /// Whether a version's body may call a language model
    /// ([`super::llm`]) or a classifier ([`super::classify`]), which run
    /// after the query that finds the rows.
    llm: bool,
}

impl FunctionSet {
    /// The set of these versions.
    pub fn of(functions: impl IntoIterator<Item = Function>) -> FunctionSet {
        let mut s = FunctionSet::default();
        for f in functions {
            s.by_name.entry(f.def.name.clone()).or_default().push(Arc::new(f));
        }
        for vs in s.by_name.values_mut() {
            vs.sort_by_key(|f| f.version);
        }
        s.llm = s.by_name.values().flatten().any(|f| {
            let b = f.def.body.to_ascii_lowercase();
            b.contains(super::llm::NAME) || b.contains(super::classify::NAME)
        });
        s
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Whether a function's body may call a language model
    /// ([`super::llm`]): whether a query that calls one is worth writing
    /// out to look for calls.
    pub fn calls_llm(&self) -> bool {
        self.llm
    }

    /// The current version of `name` (any case): the highest, unless it is
    /// a tombstone.
    pub fn current(&self, name: &str) -> Option<&Arc<Function>> {
        self.by_name.get(&name.to_ascii_lowercase())?.last().filter(|f| !f.dropped)
    }

    /// Version `version` of `name`, tombstones included.
    pub fn get(&self, name: &str, version: i64) -> Option<&Arc<Function>> {
        self.by_name.get(&name.to_ascii_lowercase())?.iter().find(|f| f.version == version)
    }

    /// The highest version of `name`, tombstones included; 0 for none.
    pub fn max_version(&self, name: &str) -> i64 {
        self.by_name.get(&name.to_ascii_lowercase()).and_then(|v| v.last()).map_or(0, |f| f.version)
    }

    /// Every version, by name, then version.
    pub fn all(&self) -> impl Iterator<Item = &Arc<Function>> {
        self.by_name.values().flatten()
    }

    /// The current version of each function, by name.
    pub fn currents(&self) -> impl Iterator<Item = &Arc<Function>> {
        self.by_name.values().filter_map(|v| v.last().filter(|f| !f.dropped))
    }

    /// Whether some function of the set is named `name` (any case).
    fn names(&self, name: &str) -> bool {
        self.by_name.contains_key(&name.to_ascii_lowercase())
    }
}

/// A statement that creates or drops a function.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    Create { def: Definition, replace: bool, at: Option<i64> },
    /// `CREATE OR REPLACE FUNCTION name FROM VERSION n`: a new version
    /// with version `from`'s definition.
    CreateFrom { name: String, from: i64, at: Option<i64> },
    /// `DROP FUNCTION name`: a tombstone (at version `at`, if given); with
    /// `version`, `DROP FUNCTION name VERSION n` deletes that old version.
    Drop { name: String, if_exists: bool, version: Option<i64>, at: Option<i64> },
}

impl Statement {
    /// The function it names.
    pub fn name(&self) -> &str {
        match self {
            Statement::Create { def, .. } => &def.name,
            Statement::CreateFrom { name, .. } | Statement::Drop { name, .. } => name,
        }
    }

    /// The version it writes, if it says (`AT VERSION n`).
    pub fn at(&self) -> Option<i64> {
        match self {
            Statement::Create { at, .. } | Statement::CreateFrom { at, .. } | Statement::Drop { at, .. } => *at,
        }
    }

    /// The same statement writing version `at`.
    pub fn at_version(&self, n: i64) -> Statement {
        let mut s = self.clone();
        match &mut s {
            Statement::Create { at, .. } | Statement::CreateFrom { at, .. } | Statement::Drop { at, .. } => *at = Some(n),
        }
        s
    }

    /// The statement as SQL that [`parse_statement`] reads back as itself.
    pub fn sql(&self) -> String {
        let at = |a: &Option<i64>| a.map_or(String::new(), |n| format!(" AT VERSION {n}"));
        match self {
            Statement::Create { def, replace, at } => def.sql(*replace, *at),
            Statement::CreateFrom { name, from, at: a } => format!("CREATE OR REPLACE FUNCTION {name} FROM VERSION {from}{}", at(a)),
            Statement::Drop { name, if_exists, version, at: a } => {
                format!("DROP FUNCTION {}{name}{}{}", if *if_exists { "IF EXISTS " } else { "" }, version.map_or(String::new(), |v| format!(" VERSION {v}")), at(a))
            }
        }
    }
}

/// The statement `text` holds when it creates or drops a function; `None`
/// for any other.
pub fn parse_statement(text: &str) -> Result<Option<Statement>> {
    // Every statement comes here first: a query is not tokenized twice.
    if !matches!(first_word(text).to_ascii_uppercase().as_str(), "CREATE" | "DROP") || !text.as_bytes().windows(8).any(|w| w.eq_ignore_ascii_case(b"FUNCTION")) {
        return Ok(None);
    }
    let all = match Tokenizer::new(&GenericDialect {}, text).tokenize_with_location() {
        Ok(t) => t,
        // Not a statement of ours, or one whose body does not tokenize.
        Err(e) => {
            let head: String = text.trim_start().chars().take(64).collect::<String>().to_ascii_uppercase();
            let ours = head.split_whitespace().take(4).collect::<Vec<_>>();
            if matches!(ours.as_slice(), ["CREATE", "FUNCTION", ..] | ["CREATE", "OR", "REPLACE", "FUNCTION", ..] | ["DROP", "FUNCTION", ..]) {
                return Err(err(format!("SQL syntax error: {e}")));
            }
            return Ok(None);
        }
    };
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
    if word(i) != "FUNCTION" {
        return Ok(None);
    }
    i += 1;
    let form = if create { CREATE_FORM } else { DROP_FORM };
    let bad = |m: &str| err(format!("{m}; the form is {form}"));
    let number = |i: usize| -> Result<i64> {
        match toks.get(i).map(|t| &t.token) {
            Some(Token::Number(n, _)) => n.parse::<i64>().ok().filter(|&v| v > 0).ok_or_else(|| bad("a version is a whole number from 1")),
            _ => Err(bad("expected a version number")),
        }
    };
    if !create {
        let if_exists = word(i) == "IF" && word(i + 1) == "EXISTS";
        if if_exists {
            i += 2;
        }
        let name = name_at(&toks, i).ok_or_else(|| bad("expected the function's name"))?;
        i += 1;
        let (mut version, mut at) = (None, None);
        while i < toks.len() {
            match word(i).as_str() {
                "VERSION" if version.is_none() => {
                    version = Some(number(i + 1)?);
                    i += 2;
                }
                "AT" if at.is_none() && word(i + 1) == "VERSION" => {
                    at = Some(number(i + 2)?);
                    i += 3;
                }
                _ => return Err(bad(&format!("unexpected {}", toks[i].token))),
            }
        }
        if version.is_some() && at.is_some() {
            return Err(bad("VERSION n deletes an old version and AT VERSION n writes a tombstone: one or the other"));
        }
        return Ok(Some(Statement::Drop { name, if_exists, version, at }));
    }
    let name = name_at(&toks, i).ok_or_else(|| bad("expected the function's name"))?;
    check_name(&name)?;
    i += 1;
    if word(i) == "FROM" {
        if !replace {
            return Err(err(format!("CREATE OR REPLACE FUNCTION {name} FROM VERSION n makes a new version of {name} from version n")));
        }
        if word(i + 1) != "VERSION" {
            return Err(bad("expected FROM VERSION n"));
        }
        let from = number(i + 2)?;
        i += 3;
        let mut at = None;
        if word(i) == "AT" && word(i + 1) == "VERSION" {
            at = Some(number(i + 2)?);
            i += 3;
        }
        if i < toks.len() {
            return Err(bad(&format!("unexpected {}", toks[i].token)));
        }
        return Ok(Some(Statement::CreateFrom { name, from, at }));
    }
    if toks.get(i).map(|t| &t.token) != Some(&Token::LParen) {
        return Err(bad(&format!("expected ( after {name}")));
    }
    i += 1;
    let mut params: Vec<Param> = Vec::new();
    if toks.get(i).map(|t| &t.token) == Some(&Token::RParen) {
        i += 1;
    } else {
        loop {
            let p = name_at(&toks, i).ok_or_else(|| bad("expected a parameter's name"))?;
            if params.iter().any(|q| q.name == p) {
                return Err(err(format!("{name} has two parameters named {p}")));
            }
            i += 1;
            let (ty, next) = type_at(&toks, i).map_err(|m| bad(&format!("{p}: {m}")))?;
            i = next;
            let mut default = None;
            if word(i) == "DEFAULT" {
                let (lit, next) = literal_at(&toks, i + 1).ok_or_else(|| bad(&format!("{p}'s DEFAULT is a value: a number, a quoted string, TRUE, FALSE or NULL")))?;
                default = Some(lit);
                i = next;
            } else if params.iter().any(|q| q.default.is_some()) {
                return Err(err(format!("{p} follows a parameter with a DEFAULT, so it needs one too")));
            }
            params.push(Param { name: p, ty, default });
            match toks.get(i).map(|t| &t.token) {
                Some(Token::Comma) => i += 1,
                Some(Token::RParen) => {
                    i += 1;
                    break;
                }
                _ => return Err(bad("expected , or ) after a parameter")),
            }
        }
    }
    if word(i) != "RETURNS" {
        return Err(bad("expected RETURNS after the parameters"));
    }
    let (kind, returns) = match word(i + 1).as_str() {
        "TABLE" => {
            i += 2;
            (Kind::Table, "TABLE".to_string())
        }
        _ => {
            let (ty, next) = type_at(&toks, i + 1).map_err(|m| bad(&format!("RETURNS: {m}")))?;
            i = next;
            (Kind::Scalar, ty)
        }
    };
    let (mut description, mut at, mut language) = (None, None, None);
    let body_at = loop {
        match word(i).as_str() {
            "AS" => break toks.get(i + 1).map(|t| t.span.start),
            "LANGUAGE" if language.is_none() => {
                let l = match toks.get(i + 1).map(|t| &t.token) {
                    Some(Token::Word(w)) => w.value.to_ascii_lowercase(),
                    Some(Token::SingleQuotedString(s)) => s.to_ascii_lowercase(),
                    _ => return Err(bad("expected a language after LANGUAGE")),
                };
                match l.as_str() {
                    "sql" => {}
                    "wasm" | "webassembly" | "javascript" | "js" => return Err(err(format!("LANGUAGE {l} is not supported yet: a function's body is SQL (LANGUAGE sql)"))),
                    _ => return Err(err(format!("there is no language {l}: a function's body is SQL (LANGUAGE sql)"))),
                }
                language = Some(l);
                i += 2;
            }
            "OPTIONS" if description.is_none() => {
                let (opts, next) = options_at(&toks, i + 1).map_err(|m| bad(&m))?;
                for (k, v) in opts {
                    match k.as_str() {
                        "description" => description = Some(v),
                        _ => return Err(err(format!("{k} is not an option of a function: description is"))),
                    }
                }
                i = next;
            }
            "AT" if at.is_none() && word(i + 1) == "VERSION" => {
                at = Some(number(i + 2)?);
                i += 3;
            }
            _ => match toks.get(i) {
                None => return Err(bad("expected AS and the body")),
                Some(t) => return Err(bad(&format!("unexpected {}", t.token))),
            },
        }
    };
    let Some(start) = body_at else { return Err(bad("expected the body after AS")) };
    let mut body = text[byte_at(text, start)..].trim().trim_end_matches(';').trim();
    if body.len() >= 4 && body.starts_with("$$") && body.ends_with("$$") {
        body = body[2..body.len() - 2].trim();
    }
    if body.is_empty() {
        return Err(bad("expected the body after AS"));
    }
    let def = definition(name, kind, params, returns, body, description)?;
    Ok(Some(Statement::Create { def, replace, at }))
}

/// The first word of `text`, after spaces and comments.
fn first_word(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        if let Some(rest) = text.strip_prefix("--") {
            text = rest.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(rest) = text.strip_prefix("/*") {
            text = rest.split_once("*/").map_or("", |(_, r)| r);
        } else {
            break;
        }
    }
    let end = text.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(text.len());
    &text[..end]
}

/// A definition whose body is checked and printed as the parser prints it.
fn definition(name: String, kind: Kind, params: Vec<Param>, returns: String, body: &str, description: Option<String>) -> Result<Definition> {
    let parsed = parse_body(kind, body)?;
    // Its calls of a language model are checked as a query checks them.
    match &parsed {
        Body::Expr(e) => super::llm::check(e)?,
        Body::Query(q) if super::llm::calls_in_query(q) => return Err(err(format!("the body of {name} calls llm(): a function that RETURNS TABLE gives rows a query reads, and a model is asked about the rows a query returns; call it in a function that returns a value, in a SELECT's columns"))),
        _ => {}
    }
    let declared: BTreeSet<&str> = params.iter().map(|p| p.name.as_str()).collect();
    let mut used = BTreeSet::new();
    let mut columns = Vec::new();
    let printed = match &parsed {
        Body::Expr(e) => {
            visit_expr(e, &mut |x| match x {
                Expr::Value(v) => {
                    if let sql::Value::Placeholder(p) = &v.value {
                        used.insert(placeholder(p));
                    }
                }
                Expr::Identifier(_) | Expr::CompoundIdentifier(_) => columns.push(x.to_string()),
                _ => {}
            });
            e.to_string()
        }
        Body::Query(q) => {
            visit_query(q, &mut |x| {
                if let Expr::Value(v) = x {
                    if let sql::Value::Placeholder(p) = &v.value {
                        used.insert(placeholder(p));
                    }
                }
            });
            q.to_string()
        }
        Body::None => unreachable!("a body was given"),
    };
    if let Some(c) = columns.first() {
        return Err(err(format!("{name}'s body reads {c}, which is no parameter: the body of a function that returns a value reads only its parameters, as :name (a column is passed in as an argument)")));
    }
    if let Some(u) = used.iter().find(|u| !declared.contains(u.as_str())) {
        let all: Vec<String> = params.iter().map(|p| format!(":{}", p.name)).collect();
        return Err(err(format!("{name}'s body reads :{u}, which is not one of its parameters ({})", if all.is_empty() { "it has none".into() } else { all.join(", ") })));
    }
    Ok(Definition { name, kind, params, returns, body: printed, description })
}

fn placeholder(p: &str) -> String {
    p.trim_start_matches([':', '$', '@', '?']).to_ascii_lowercase()
}

/// A body parsed: an expression for a scalar function, a `SELECT` for a
/// table function.
fn parse_body(kind: Kind, body: &str) -> Result<Body> {
    match kind {
        Kind::Scalar => {
            if body.split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case("SELECT") || w.eq_ignore_ascii_case("WITH")) {
                return Err(err("the body of a function that returns a value is one expression of its parameters: a SELECT is the body of one that RETURNS TABLE"));
            }
            let q = super::sql::parse_query(&format!("SELECT {body}")).map_err(|e| err(format!("the body {body} is not an expression: {e}")))?;
            let shape = || err(format!("the body of a function that returns a value is one expression of its parameters, not {body}"));
            let sql::SetExpr::Select(s) = q.body.as_ref() else { return Err(shape()) };
            if q.with.is_some() || q.order_by.is_some() || q.limit_clause.is_some() || !s.from.is_empty() || s.selection.is_some() || s.projection.len() != 1 {
                return Err(shape());
            }
            match &s.projection[0] {
                SelectItem::UnnamedExpr(e) => Ok(Body::Expr(Box::new(e.clone()))),
                _ => Err(shape()),
            }
        }
        Kind::Table => {
            let q = super::sql::parse_query(body).map_err(|e| err(format!("the body of a function that RETURNS TABLE is a SELECT: {e}")))?;
            Ok(Body::Query(q))
        }
    }
}

/// A function's name at `i`: a plain identifier, lower case.
fn name_at(toks: &[&TokenWithSpan], i: usize) -> Option<String> {
    match toks.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) => Some(w.value.to_ascii_lowercase()),
        _ => None,
    }
}

/// Checks `name` as a stored function's name, as `CREATE FUNCTION` does.
pub(crate) fn check_function_name(name: &str) -> Result<()> {
    check_name(name)
}

fn check_name(name: &str) -> Result<()> {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !plain {
        return Err(err(format!("{name} is not a function name: letters, digits and underscores, not starting with a digit")));
    }
    if name.starts_with("__") {
        return Err(err(format!("{name}: names starting with __ are ChloeDB's own")));
    }
    if RESERVED.contains(&name) {
        return Err(err(format!("{name} is a built-in function: a stored function takes another name")));
    }
    Ok(())
}

/// A type at `i` and the position after it: `INT`, `VECTOR(8)`,
/// `KEYWORD[]`, with the usual other spellings (`INTEGER`, `DOUBLE`,
/// `VARCHAR`, `BOOLEAN`).
fn type_at(toks: &[&TokenWithSpan], mut i: usize) -> std::result::Result<(String, usize), String> {
    let w = match toks.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) => w.value.to_ascii_uppercase(),
        _ => return Err(format!("expected a type: {}", TYPES.join(", "))),
    };
    let ty = match w.as_str() {
        "INT" | "INTEGER" | "BIGINT" | "SMALLINT" => "INT",
        "FLOAT" | "DOUBLE" | "REAL" | "NUMERIC" | "DECIMAL" => "FLOAT",
        "TEXT" | "VARCHAR" | "STRING" => "TEXT",
        "KEYWORD" => "KEYWORD",
        "BOOL" | "BOOLEAN" => "BOOL",
        "TIMESTAMP" => "TIMESTAMP",
        "JSON" => "JSON",
        "VECTOR" => "VECTOR",
        other => return Err(format!("{other} is not a type: {}", TYPES.join(", "))),
    };
    let mut out = ty.to_string();
    i += 1;
    if ty == "VECTOR" && toks.get(i).map(|t| &t.token) == Some(&Token::LParen) {
        match (toks.get(i + 1).map(|t| &t.token), toks.get(i + 2).map(|t| &t.token)) {
            (Some(Token::Number(n, _)), Some(Token::RParen)) => {
                out = format!("VECTOR({n})");
                i += 3;
            }
            _ => return Err("VECTOR(dimensions)".into()),
        }
    } else if ty == "TEXT" && toks.get(i).map(|t| &t.token) == Some(&Token::LParen) {
        // VARCHAR(n): the length says nothing to Strata.
        match toks.get(i + 2).map(|t| &t.token) {
            Some(Token::RParen) => i += 3,
            _ => return Err("expected ) after the length".into()),
        }
    }
    if toks.get(i).map(|t| &t.token) == Some(&Token::LBracket) && toks.get(i + 1).map(|t| &t.token) == Some(&Token::RBracket) {
        if ty != "KEYWORD" && ty != "INT" {
            return Err("only KEYWORD[] and INT[] are lists".into());
        }
        out.push_str("[]");
        i += 2;
    }
    Ok((out, i))
}

/// A literal at `i` as SQL text, and the position after it.
fn literal_at(toks: &[&TokenWithSpan], i: usize) -> Option<(String, usize)> {
    match toks.get(i).map(|t| &t.token)? {
        Token::Minus => match toks.get(i + 1).map(|t| &t.token)? {
            Token::Number(n, _) => Some((format!("-{n}"), i + 2)),
            _ => None,
        },
        Token::Number(n, _) => Some((n.clone(), i + 1)),
        Token::SingleQuotedString(s) => Some((quote(s), i + 1)),
        Token::Word(w) if w.quote_style.is_none() && ["TRUE", "FALSE", "NULL"].contains(&w.value.to_ascii_uppercase().as_str()) => Some((w.value.to_ascii_uppercase(), i + 1)),
        _ => None,
    }
}

/// `(key = 'value', ...)` at `i`, and the position after it.
fn options_at(toks: &[&TokenWithSpan], mut i: usize) -> std::result::Result<(Vec<(String, String)>, usize), String> {
    if toks.get(i).map(|t| &t.token) != Some(&Token::LParen) {
        return Err("expected ( after OPTIONS".into());
    }
    i += 1;
    let mut out = Vec::new();
    loop {
        let k = name_at(toks, i).ok_or("expected an option's name")?;
        if toks.get(i + 1).map(|t| &t.token) != Some(&Token::Eq) {
            return Err(format!("expected = after {k}"));
        }
        let v = match toks.get(i + 2).map(|t| &t.token) {
            Some(Token::SingleQuotedString(s)) => s.clone(),
            _ => return Err(format!("{k} is a quoted string")),
        };
        out.push((k, v));
        i += 3;
        match toks.get(i).map(|t| &t.token) {
            Some(Token::Comma) => i += 1,
            Some(Token::RParen) => return Ok((out, i + 1)),
            _ => return Err("expected , or ) in OPTIONS".into()),
        }
    }
}

/// The byte offset of `at` (line and column, from 1) in `text`.
fn byte_at(text: &str, at: Location) -> usize {
    let mut line_start = 0;
    for _ in 1..at.line {
        line_start += text[line_start..].find('\n').map_or(text.len() - line_start, |n| n + 1);
    }
    line_start + text[line_start..].chars().take((at.column as usize).saturating_sub(1)).map(char::len_utf8).sum::<usize>()
}

/// Parameters to plan a table function's body with, to check it before
/// it is kept: each parameter's default, or a value of its type; `None`
/// when one is a VECTOR of no stated size.
pub fn sample_params(def: &Definition) -> Option<super::Params> {
    let mut params = super::Params::new();
    for p in &def.params {
        let v = match p.default.as_deref() {
            Some(d) => literal_of(d).unwrap_or(Value::Null),
            None => sample_of(&p.ty)?,
        };
        params.0.insert(p.name.clone(), v);
    }
    Some(params)
}

/// A value of type `ty` (a stored function's parameter's), to plan its
/// body with; `None` for a VECTOR of no stated size.
fn sample_of(ty: &str) -> Option<Value> {
    let base = ty.trim_end_matches("[]");
    let v = match base {
        "INT" => Value::Int(1),
        "FLOAT" => Value::Float(1.0),
        "BOOL" => Value::Bool(true),
        "TIMESTAMP" => Value::Timestamp(0),
        "JSON" => Value::Str("{}".into()),
        _ if base.starts_with("VECTOR(") => Value::Vector(vec![0.0f32; base[7..base.len() - 1].parse().ok()?].into()),
        "VECTOR" => return None,
        _ => Value::Str("x".into()),
    };
    Some(match ty.ends_with("[]") {
        true => Value::List(vec![v]),
        false => v,
    })
}

/// The value of a literal as SQL text: `0.5`, `-3`, `'text'`, `TRUE`.
fn literal_of(text: &str) -> Option<Value> {
    use sql::{SetExpr, UnaryOperator, Value as V};
    let q = super::sql::parse_query(&format!("SELECT {text}")).ok()?;
    let SetExpr::Select(s) = q.body.as_ref() else { return None };
    let SelectItem::UnnamedExpr(e) = s.projection.first()? else { return None };
    let (neg, v) = match e {
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => match expr.as_ref() {
            Expr::Value(v) => (true, &v.value),
            _ => return None,
        },
        Expr::Value(v) => (false, &v.value),
        _ => return None,
    };
    Some(match v {
        V::Number(n, _) => match n.parse::<i64>() {
            Ok(i) => Value::Int(if neg { -i } else { i }),
            Err(_) => Value::Float(n.parse::<f64>().ok().map(|f| if neg { -f } else { f })?),
        },
        V::SingleQuotedString(t) => Value::Str(t.clone()),
        V::Boolean(b) => Value::Bool(*b),
        _ => Value::Null,
    })
}

/// What a statement changes in `_functions`.
#[derive(Debug)]
pub enum Change {
    /// Writes this version.
    Put(Function),
    /// Deletes the version with this key.
    Delete(String),
    /// Nothing: the version is there already, or `IF EXISTS` found none.
    Nothing,
}

/// What `stmt` changes in a database whose functions are `set`, written
/// at `now` (microseconds since the epoch) by `by`.
pub fn change(set: &FunctionSet, stmt: &Statement, now: Option<i64>, by: Option<&str>) -> Result<Change> {
    let by = by.map(str::to_string);
    let name = stmt.name();
    let max = set.max_version(name);
    // AT VERSION n: n is written once, then the same statement again does
    // nothing.
    let at = |n: i64, hash: &str, dropped: bool| -> Result<Option<Change>> {
        match set.get(name, n) {
            Some(f) if f.hash == hash && f.dropped == dropped => Ok(Some(Change::Nothing)),
            Some(_) => Err(err(format!("version {n} of {name} is another definition"))),
            None if n <= max => Err(err(format!("{name} has versions up to {max}, so version {n} is not the next one"))),
            None => Ok(None),
        }
    };
    match stmt {
        Statement::Create { def, replace, at: a } => {
            let hash = def.hash();
            if let Some(n) = a {
                return match at(*n, &hash, false)? {
                    Some(c) => Ok(c),
                    None => Ok(Change::Put(Function::new(def.clone(), *n, now, by)?)),
                };
            }
            if let Some(cur) = set.current(name) {
                if !replace {
                    return Err(err(format!("function {name} exists (version {}): CREATE OR REPLACE FUNCTION makes a new version", cur.version)));
                }
                if cur.hash == hash {
                    return Ok(Change::Nothing);
                }
                if cur.def.kind != def.kind {
                    return Err(err(format!("{name} {}; a new version returns the same kind: DROP FUNCTION {name} first", if cur.def.kind == Kind::Table { "RETURNS TABLE" } else { "returns a value" })));
                }
            }
            Ok(Change::Put(Function::new(def.clone(), max + 1, now, by)?))
        }
        Statement::CreateFrom { name, from, at: a } => {
            let base = set.get(name, *from).filter(|f| !f.dropped).ok_or_else(|| err(format!("{name} has no version {from} to make a new version from")))?;
            let def = base.def.clone();
            let stmt = Statement::Create { def, replace: true, at: *a };
            change(set, &stmt, now, by.as_deref())
        }
        Statement::Drop { name, if_exists, version: Some(v), .. } => match set.get(name, *v) {
            None if *if_exists => Ok(Change::Nothing),
            None => Err(err(format!("{name} has no version {v}"))),
            Some(f) if set.current(name).is_some_and(|c| c.version == f.version) => Err(err(format!("version {v} is {name}'s current version: DROP FUNCTION {name} drops the function"))),
            Some(f) => Ok(Change::Delete(f.id())),
        },
        Statement::Drop { name, if_exists, version: None, at: a } => {
            let Some(cur) = set.current(name) else {
                if let Some(n) = a {
                    if set.get(name, *n).is_some_and(|f| f.dropped) {
                        return Ok(Change::Nothing);
                    }
                }
                return match if_exists {
                    true => Ok(Change::Nothing),
                    false => Err(err(format!("no function named {name}"))),
                };
            };
            let n = a.unwrap_or(max + 1);
            let t = Function::tombstone(&cur.def, n, now, by);
            if let Some(c) = at(n, &t.hash, true)? {
                return Ok(c);
            }
            Ok(Change::Put(t))
        }
    }
}

/// The function a call names, if it is a stored one: its current version,
/// or the one a quoted `"name@v"` pins.
fn resolve<'s>(set: &'s FunctionSet, name: &sql::ObjectName) -> Result<Option<&'s Arc<Function>>> {
    let [part] = name.0.as_slice() else { return Ok(None) };
    let Some(id) = part.as_ident() else { return Ok(None) };
    if id.quote_style.is_some() {
        if let Some((n, v)) = id.value.rsplit_once('@') {
            let Ok(v) = v.parse::<i64>() else { return Ok(None) };
            if !set.names(n) {
                return Ok(None);
            }
            return match set.get(n, v) {
                Some(f) if !f.dropped => Ok(Some(f)),
                Some(_) => Err(err(format!("version {v} of {n} is the one that dropped it"))),
                None => Err(err(format!("{n} has no version {v}"))),
            };
        }
    }
    Ok(set.current(&id.value))
}

/// `q` with every call of a function of `set` written out ([module
/// docs](self)), and the versions it called (`name@version`), in order of
/// first call; `None` when it calls none.
pub fn expand(q: &sql::Query, set: &FunctionSet) -> Result<Option<(sql::Query, Vec<String>)>> {
    if set.is_empty() {
        return Ok(None);
    }
    let mut x = Expansion { set, used: Vec::new(), nodes: 0 };
    // `SELECT * FROM f(..)` alone is f's body.
    if let Some(body) = whole_call(q, set, &mut x.used)? {
        let mut out = body;
        x.query(&mut out, 1, 0)?;
        return Ok(Some((out, x.used)));
    }
    let mut out = q.clone();
    x.query(&mut out, 0, 0)?;
    Ok((!x.used.is_empty()).then_some((out, x.used)))
}

/// The most expressions a statement's calls may write out in all
/// ([`expand`]), as many as the elements a statement's text may hold: a
/// body that uses a parameter twice and calls a function that does too
/// doubles at each of up to [`MAX_DEPTH`] calls.
pub const MAX_WRITTEN_OUT: usize = 250_000;

/// The depth of `e` as a tree, counted up to `cap` (past it, `cap + 1`),
/// and its expressions, added to `nodes`.
fn depth_of(e: &mut Expr, cap: usize, nodes: &mut usize) -> usize {
    *nodes += 1;
    if cap == 0 {
        return 1;
    }
    let (mut first, mut most) = (true, 0);
    // The first expression visit_expr_mut is called with is `e`; each
    // one after it is one of its children, measured here.
    visit_expr_mut(e, &mut |c| {
        if std::mem::take(&mut first) {
            return true;
        }
        most = most.max(depth_of(c, cap - 1, nodes));
        false
    });
    1 + most
}

/// One [`expand`]: the functions it calls, the versions it called, and
/// the expressions it wrote out so far.
struct Expansion<'s> {
    set: &'s FunctionSet,
    used: Vec<String>,
    nodes: usize,
}

impl Expansion<'_> {
    /// A query error when `out`, written out at `pos` levels deep in the
    /// statement, would nest past [`super::parse::MAX_EXPR_DEPTH`] or take
    /// the statement past [`MAX_WRITTEN_OUT`] expressions: the bounds the
    /// parser holds a statement's text to, held to it written out, before
    /// anything walks it deeper (#501 bounds the text only, so that a
    /// call of a few bytes could plan 16,000 levels and overflow the
    /// stack).
    fn check(&mut self, name: &str, out: &mut Expr, pos: usize) -> Result<()> {
        // Each call written out: the deadline and plan_ms too (X-B3).
        super::compound::check_planning()?;
        let room = super::parse::MAX_EXPR_DEPTH.saturating_sub(pos);
        if depth_of(out, room, &mut self.nodes) > room {
            return Err(err(format!(
                "{name}(...) written out nests more than {} levels deep, the most a statement may (its body, the bodies it calls and its arguments in them): write the function with fewer terms, or call it with simpler arguments",
                super::parse::MAX_EXPR_DEPTH
            )));
        }
        if self.nodes > MAX_WRITTEN_OUT {
            return Err(err(format!(
                "the calls of {name} written out hold more than {MAX_WRITTEN_OUT} expressions, the most a statement may: does a body use a parameter many times and call another function that does too?"
            )));
        }
        Ok(())
    }

    /// Writes out the calls in `q`, at call nesting `depth`, its
    /// expressions `pos` levels deep in the statement.
    fn query(&mut self, q: &mut sql::Query, depth: usize, pos: usize) -> Result<()> {
        let mut failed: Option<Error> = None;
        // Table functions in FROM, then calls in expressions.
        let set = self.set;
        relations_mut(q, &mut |t| {
            if failed.is_some() {
                return;
            }
            let TableFactor::Table { name, alias, args, .. } = t else { return };
            let f = match resolve(set, name) {
                Ok(Some(f)) => f.clone(),
                Ok(None) => return,
                Err(e) => {
                    failed = Some(e);
                    return;
                }
            };
            if f.def.kind != Kind::Table {
                failed = Some(err(format!("{} returns a value, not rows: call it in an expression, not in FROM", f.def.name)));
                return;
            }
            let Some(a) = args else {
                failed = Some(err(format!("{} is a table function: call it, {}(...)", f.def.name, f.def.name)));
                return;
            };
            if depth >= MAX_DEPTH {
                failed = Some(err(format!("calls nest more than {MAX_DEPTH} deep at {}: does a function call itself?", f.def.name)));
                return;
            }
            match table_body(&f, &a.args, &mut self.used) {
                Ok(mut body) => {
                    // The body is a query of its own, a level deeper.
                    let mut measured = Ok(());
                    query_exprs_mut(&mut body, &mut |e| {
                        if measured.is_ok() {
                            measured = self.check(&f.def.name, e, pos + 1);
                        }
                    });
                    if let Err(e) = measured.and_then(|()| self.query(&mut body, depth + 1, pos + 1)) {
                        failed = Some(e);
                        return;
                    }
                    let alias = alias.clone().unwrap_or_else(|| sql::TableAlias { name: sql::Ident::new(f.def.name.clone()), columns: Vec::new() });
                    *t = TableFactor::Derived { lateral: false, subquery: Box::new(body), alias: Some(alias) };
                }
                Err(e) => failed = Some(e),
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }
        query_exprs_mut(q, &mut |e| {
            if failed.is_none() {
                if let Err(x) = self.expr(e, depth, pos) {
                    failed = Some(x);
                }
            }
        });
        failed.map_or(Ok(()), Err)
    }

    /// Writes out the scalar calls in `e`, which sits `pos` levels deep in
    /// the statement.
    fn expr(&mut self, e: &mut Expr, depth: usize, pos: usize) -> Result<()> {
        if let Some(out) = self.call(e, depth, pos)? {
            *e = out;
            return Ok(());
        }
        let (mut first, mut failed) = (true, None);
        // `e`'s children, one level deeper ([`depth_of`]).
        visit_expr_mut(e, &mut |c| {
            if std::mem::take(&mut first) {
                return true;
            }
            if failed.is_none() {
                if let Err(x) = self.expr(c, depth, pos + 1) {
                    failed = Some(x);
                }
            }
            false
        });
        failed.map_or(Ok(()), Err)
    }

    /// `x` written out when it calls a scalar function of the set, at
    /// `pos` levels deep in the statement.
    fn call(&mut self, x: &mut Expr, depth: usize, pos: usize) -> Result<Option<Expr>> {
        let Expr::Function(call) = x else { return Ok(None) };
        let Some(f) = resolve(self.set, &call.name)?.cloned() else { return Ok(None) };
        let name = &f.def.name;
        if f.def.kind == Kind::Table {
            return Err(err(format!("{name} returns rows: call it in FROM, SELECT * FROM {name}(...)")));
        }
        if call.over.is_some() || call.filter.is_some() || !call.within_group.is_empty() {
            return Err(err(format!("{name} is a stored function: OVER, FILTER and WITHIN GROUP are for aggregates and window functions")));
        }
        if depth >= MAX_DEPTH {
            return Err(err(format!("calls nest more than {MAX_DEPTH} deep at {name}: does a function call itself?")));
        }
        let args: Vec<FunctionArg> = match &call.args {
            FunctionArguments::None => Vec::new(),
            FunctionArguments::List(l) if l.duplicate_treatment.is_none() && l.clauses.is_empty() => l.args.clone(),
            other => return Err(err(format!("{name}{other}: the arguments of a stored function are values or expressions"))),
        };
        // The arguments first, then the body with them in place.
        let mut bound = bind_args(&f, &args)?;
        for a in bound.values_mut() {
            self.expr(a, depth, pos + 1)?;
        }
        let Body::Expr(body) = &f.body else { return Err(Error::Corrupt(format!("{} has no expression", f.id()))) };
        let mut out = Expr::Nested(Box::new((**body).clone()));
        substitute(&mut out, &bound);
        note(&mut self.used, &f);
        // Measured before the calls it makes are walked into, so that no
        // walk goes deeper than a statement may.
        self.check(name, &mut out, pos)?;
        // Calls the body makes, one level down; the arguments in it are
        // written out already.
        self.expr(&mut out, depth + 1, pos)?;
        Ok(Some(out))
    }
}

/// The body of a table function `q` is nothing but a call of: `SELECT *
/// FROM f(..)`, no alias read, no WHERE, order, limit or join.
fn whole_call(q: &sql::Query, set: &FunctionSet, used: &mut Vec<String>) -> Result<Option<sql::Query>> {
    let sql::SetExpr::Select(s) = q.body.as_ref() else { return Ok(None) };
    let plain = q.with.is_none() && q.order_by.is_none() && q.limit_clause.is_none() && q.fetch.is_none() && s.distinct.is_none() && s.selection.is_none() && s.having.is_none();
    let grouped = !matches!(&s.group_by, sql::GroupByExpr::Expressions(es, _) if es.is_empty());
    if !plain || grouped || s.from.len() != 1 || !s.from[0].joins.is_empty() || !matches!(s.projection.as_slice(), [SelectItem::Wildcard(_)]) {
        return Ok(None);
    }
    let TableFactor::Table { name, args: Some(args), .. } = &s.from[0].relation else { return Ok(None) };
    let Some(f) = resolve(set, name)? else { return Ok(None) };
    if f.def.kind != Kind::Table {
        return Ok(None);
    }
    Ok(Some(table_body(f, &args.args, used)?))
}

fn note(used: &mut Vec<String>, f: &Function) {
    let id = f.id();
    if !used.contains(&id) {
        used.push(id);
    }
}

/// The arguments of a call of `f`, by parameter: positional ones first,
/// then `name => value`, then the defaults.
fn bind_args(f: &Function, args: &[FunctionArg]) -> Result<BTreeMap<String, Expr>> {
    let name = &f.def.name;
    let mut out: BTreeMap<String, Expr> = BTreeMap::new();
    let mut named = false;
    for (i, a) in args.iter().enumerate() {
        let (p, e) = match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                if named {
                    return Err(err(format!("{name}(...): an argument by position follows one by name")));
                }
                let p = f.def.params.get(i).ok_or_else(|| err(format!("{name} takes {} argument{}, and is given more", f.def.params.len(), if f.def.params.len() == 1 { "" } else { "s" })))?;
                (p, e)
            }
            FunctionArg::Named { name: n, arg: FunctionArgExpr::Expr(e), .. } => {
                named = true;
                let k = n.value.to_ascii_lowercase();
                let p = f.def.params.iter().find(|p| p.name == k).ok_or_else(|| err(format!("{name} has no parameter {k}; its parameters are {}", f.def.signature())))?;
                (p, e)
            }
            FunctionArg::ExprNamed { name: Expr::Identifier(n), arg: FunctionArgExpr::Expr(e), .. } => {
                named = true;
                let k = n.value.to_ascii_lowercase();
                let p = f.def.params.iter().find(|p| p.name == k).ok_or_else(|| err(format!("{name} has no parameter {k}; its parameters are {}", f.def.signature())))?;
                (p, e)
            }
            a => return Err(err(format!("{name}({a}): an argument is a value, a parameter (:name) or an expression"))),
        };
        if out.contains_key(&p.name) {
            return Err(err(format!("{name}(...) is given {} twice", p.name)));
        }
        check_type(name, p, e)?;
        out.insert(p.name.clone(), e.clone());
    }
    for p in &f.def.params {
        if !out.contains_key(&p.name) {
            let d = p.default.as_ref().ok_or_else(|| err(format!("{name}(...) needs {} ({}); the function is {}", p.name, p.ty, f.def.signature())))?;
            let e = parse_body(Kind::Scalar, d).ok().and_then(|b| match b {
                Body::Expr(e) => Some(*e),
                _ => None,
            });
            out.insert(p.name.clone(), e.ok_or_else(|| Error::Corrupt(format!("{name}'s default for {} is no value: {d}", p.name)))?);
        }
    }
    Ok(out)
}

/// Refuses a literal argument of another type than its parameter's.
fn check_type(name: &str, p: &Param, e: &Expr) -> Result<()> {
    let v = match e {
        Expr::Value(v) => &v.value,
        Expr::UnaryOp { op: sql::UnaryOperator::Minus, expr } => match expr.as_ref() {
            Expr::Value(v) => &v.value,
            _ => return Ok(()),
        },
        _ => return Ok(()),
    };
    let base = p.ty.trim_end_matches("[]");
    let fits = match v {
        sql::Value::Null | sql::Value::Placeholder(_) => true,
        sql::Value::Number(n, _) => match base {
            "INT" => n.parse::<i64>().is_ok(),
            "FLOAT" | "TIMESTAMP" => true,
            _ => false,
        },
        sql::Value::SingleQuotedString(_) | sql::Value::DoubleQuotedString(_) => matches!(base, "TEXT" | "KEYWORD" | "TIMESTAMP" | "JSON"),
        sql::Value::Boolean(_) => base == "BOOL",
        _ => true,
    };
    match fits {
        true => Ok(()),
        false => Err(err(format!("{name}'s {} is {}, and is given {e}", p.name, p.ty))),
    }
}

/// `e` with each placeholder of `args` replaced by its argument: a value
/// or a placeholder as it is, any other expression in parentheses.
fn substitute(e: &mut Expr, args: &BTreeMap<String, Expr>) {
    visit_expr_mut(e, &mut |x| {
        if let Expr::Value(v) = x {
            if let sql::Value::Placeholder(p) = &v.value {
                if let Some(a) = args.get(&placeholder(p)) {
                    *x = match a {
                        Expr::Value(_) | Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Nested(_) | Expr::Function(_) => a.clone(),
                        _ => Expr::Nested(Box::new(a.clone())),
                    };
                    return false;
                }
            }
        }
        true
    });
}

/// The body of table function `f` for a call with `args`.
fn table_body(f: &Function, args: &[FunctionArg], used: &mut Vec<String>) -> Result<sql::Query> {
    let Body::Query(q) = &f.body else { return Err(Error::Corrupt(format!("{} has no query", f.id()))) };
    let bound = bind_args(f, args)?;
    for (k, a) in &bound {
        let mut col = None;
        visit_expr(a, &mut |x| {
            if matches!(x, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                col.get_or_insert_with(|| x.to_string());
            }
        });
        if let Some(c) = col {
            return Err(err(format!("{}({k} => {a}): the argument of a table function is a value or a parameter (:name), not a column ({c})", f.def.name)));
        }
    }
    let mut out = (**q).clone();
    query_exprs_mut(&mut out, &mut |e| substitute(e, &bound));
    note(used, f);
    Ok(out)
}

/// The functions a statement's text calls, as [`expand`] would write them
/// out (`name@version`), for `EXPLAIN`.
pub fn called(sql_text: &str, set: &FunctionSet) -> Vec<String> {
    let Ok(q) = super::sql::parse_query(sql_text) else { return Vec::new() };
    expand(&q, set).ok().flatten().map(|(_, used)| used).unwrap_or_default()
}

// Walks over the parts of a query and its expressions that Strata binds.

/// Calls `f` with each expression of `e`, outermost first; `f` returning
/// false skips what is inside (it replaced it).
pub(crate) fn visit_expr_mut(e: &mut Expr, f: &mut dyn FnMut(&mut Expr) -> bool) {
    if !f(e) {
        return;
    }
    match e {
        Expr::Nested(x)
        | Expr::UnaryOp { expr: x, .. }
        | Expr::IsNull(x)
        | Expr::IsNotNull(x)
        | Expr::IsTrue(x)
        | Expr::IsNotTrue(x)
        | Expr::IsFalse(x)
        | Expr::IsNotFalse(x)
        | Expr::IsUnknown(x)
        | Expr::IsNotUnknown(x)
        | Expr::Cast { expr: x, .. }
        | Expr::Extract { expr: x, .. }
        | Expr::Ceil { expr: x, .. }
        | Expr::Floor { expr: x, .. }
        | Expr::Collate { expr: x, .. } => visit_expr_mut(x, f),
        Expr::BinaryOp { left, right, .. } | Expr::IsDistinctFrom(left, right) | Expr::IsNotDistinctFrom(left, right) | Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
            visit_expr_mut(left, f);
            visit_expr_mut(right, f);
        }
        Expr::Between { expr, low, high, .. } => {
            visit_expr_mut(expr, f);
            visit_expr_mut(low, f);
            visit_expr_mut(high, f);
        }
        Expr::InList { expr, list, .. } => {
            visit_expr_mut(expr, f);
            list.iter_mut().for_each(|x| visit_expr_mut(x, f));
        }
        Expr::InSubquery { expr, subquery, .. } => {
            visit_expr_mut(expr, f);
            query_exprs_mut(subquery, &mut |x| visit_expr_mut(x, f));
        }
        Expr::Exists { subquery, .. } | Expr::Subquery(subquery) => query_exprs_mut(subquery, &mut |x| visit_expr_mut(x, f)),
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } | Expr::SimilarTo { expr, pattern, .. } | Expr::RLike { expr, pattern, .. } => {
            visit_expr_mut(expr, f);
            visit_expr_mut(pattern, f);
        }
        Expr::Function(func) => {
            if let FunctionArguments::List(l) = &mut func.args {
                for a in &mut l.args {
                    match a {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) | FunctionArg::Named { arg: FunctionArgExpr::Expr(x), .. } | FunctionArg::ExprNamed { arg: FunctionArgExpr::Expr(x), .. } => visit_expr_mut(x, f),
                        _ => {}
                    }
                }
            }
            if let FunctionArguments::Subquery(q) = &mut func.args {
                query_exprs_mut(q, &mut |x| visit_expr_mut(x, f));
            }
            if let Some(x) = &mut func.filter {
                visit_expr_mut(x, f);
            }
            if let Some(sql::WindowType::WindowSpec(w)) = &mut func.over {
                w.partition_by.iter_mut().for_each(|x| visit_expr_mut(x, f));
                w.order_by.iter_mut().for_each(|o| visit_expr_mut(&mut o.expr, f));
            }
        }
        Expr::Case { operand, conditions, else_result, .. } => {
            if let Some(x) = operand {
                visit_expr_mut(x, f);
            }
            for c in conditions {
                visit_expr_mut(&mut c.condition, f);
                visit_expr_mut(&mut c.result, f);
            }
            if let Some(x) = else_result {
                visit_expr_mut(x, f);
            }
        }
        Expr::Tuple(xs) => xs.iter_mut().for_each(|x| visit_expr_mut(x, f)),
        Expr::Array(a) => a.elem.iter_mut().for_each(|x| visit_expr_mut(x, f)),
        _ => {}
    }
}

/// [`visit_expr_mut`] that only reads.
fn visit_expr(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    let mut e = e.clone();
    visit_expr_mut(&mut e, &mut |x| {
        f(x);
        true
    });
}

/// [`query_exprs_mut`] that only reads, into calls' subqueries too.
fn visit_query(q: &sql::Query, f: &mut dyn FnMut(&Expr)) {
    let mut q = q.clone();
    query_exprs_mut(&mut q, &mut |e| {
        visit_expr_mut(e, &mut |x| {
            f(x);
            true
        })
    });
}

/// Calls `f` with each top-level expression of `q` (its CTEs, set
/// operations, derived tables and joins' conditions included).
pub(crate) fn query_exprs_mut(q: &mut sql::Query, f: &mut dyn FnMut(&mut Expr)) {
    if let Some(w) = &mut q.with {
        for cte in &mut w.cte_tables {
            query_exprs_mut(&mut cte.query, f);
        }
    }
    set_exprs_mut(&mut q.body, f);
    if let Some(sql::OrderBy { kind: sql::OrderByKind::Expressions(es), .. }) = &mut q.order_by {
        es.iter_mut().for_each(|o| f(&mut o.expr));
    }
    match &mut q.limit_clause {
        Some(sql::LimitClause::LimitOffset { limit, offset, limit_by }) => {
            limit.iter_mut().for_each(&mut *f);
            if let Some(o) = offset {
                f(&mut o.value);
            }
            limit_by.iter_mut().for_each(&mut *f);
        }
        Some(sql::LimitClause::OffsetCommaLimit { offset, limit }) => {
            f(offset);
            f(limit);
        }
        None => {}
    }
}

fn set_exprs_mut(s: &mut sql::SetExpr, f: &mut dyn FnMut(&mut Expr)) {
    match s {
        sql::SetExpr::Select(s) => {
            if let Some(sql::Distinct::On(es)) = &mut s.distinct {
                es.iter_mut().for_each(&mut *f);
            }
            for item in &mut s.projection {
                if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
                    f(e);
                }
            }
            for t in &mut s.from {
                factor_exprs_mut(&mut t.relation, f);
                for j in &mut t.joins {
                    factor_exprs_mut(&mut j.relation, f);
                    if let Some(sql::JoinConstraint::On(e)) = join_constraint_mut(&mut j.join_operator) {
                        f(e);
                    }
                }
            }
            s.selection.iter_mut().for_each(&mut *f);
            s.having.iter_mut().for_each(&mut *f);
            s.qualify.iter_mut().for_each(&mut *f);
            if let sql::GroupByExpr::Expressions(es, _) = &mut s.group_by {
                es.iter_mut().for_each(&mut *f);
            }
        }
        sql::SetExpr::Query(q) => query_exprs_mut(q, f),
        sql::SetExpr::SetOperation { left, right, .. } => {
            set_exprs_mut(left, f);
            set_exprs_mut(right, f);
        }
        _ => {}
    }
}

fn factor_exprs_mut(t: &mut TableFactor, f: &mut dyn FnMut(&mut Expr)) {
    match t {
        TableFactor::Derived { subquery, .. } => query_exprs_mut(subquery, f),
        TableFactor::NestedJoin { table_with_joins, .. } => {
            factor_exprs_mut(&mut table_with_joins.relation, f);
            for j in &mut table_with_joins.joins {
                factor_exprs_mut(&mut j.relation, f);
            }
        }
        TableFactor::Table { args: Some(a), .. } => {
            for x in &mut a.args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) | FunctionArg::Named { arg: FunctionArgExpr::Expr(e), .. } | FunctionArg::ExprNamed { arg: FunctionArgExpr::Expr(e), .. } = x {
                    f(e);
                }
            }
        }
        _ => {}
    }
}

fn join_constraint_mut(o: &mut sql::JoinOperator) -> Option<&mut sql::JoinConstraint> {
    use sql::JoinOperator as J;
    match o {
        J::Join(c) | J::Inner(c) | J::Left(c) | J::LeftOuter(c) | J::Right(c) | J::RightOuter(c) | J::FullOuter(c) | J::Semi(c) | J::LeftSemi(c) | J::RightSemi(c) | J::Anti(c) | J::LeftAnti(c) | J::RightAnti(c) => Some(c),
        _ => None,
    }
}

/// Calls `f` with each table factor of `q` that names a table (or calls a
/// table function), in its CTEs, set operations, derived tables, joins
/// and subqueries too.
pub(crate) fn relations_mut(q: &mut sql::Query, f: &mut dyn FnMut(&mut TableFactor)) {
    if let Some(w) = &mut q.with {
        for cte in &mut w.cte_tables {
            relations_mut(&mut cte.query, f);
        }
    }
    set_relations_mut(&mut q.body, f);
    // Subqueries in expressions.
    query_exprs_mut(q, &mut |e| expr_relations_mut(e, f));
}

/// [`relations_mut`] of the subqueries in `e`.
fn expr_relations_mut(e: &mut Expr, f: &mut dyn FnMut(&mut TableFactor)) {
    visit_expr_mut(e, &mut |x| match x {
        Expr::Exists { subquery, .. } | Expr::Subquery(subquery) => {
            relations_mut(subquery, f);
            false
        }
        Expr::InSubquery { expr, subquery, .. } => {
            relations_mut(subquery, f);
            expr_relations_mut(expr, f);
            false
        }
        _ => true,
    });
}

fn set_relations_mut(s: &mut sql::SetExpr, f: &mut dyn FnMut(&mut TableFactor)) {
    match s {
        sql::SetExpr::Select(s) => {
            for t in &mut s.from {
                factor_relations_mut(&mut t.relation, f);
                for j in &mut t.joins {
                    factor_relations_mut(&mut j.relation, f);
                }
            }
        }
        sql::SetExpr::Query(q) => relations_mut(q, f),
        sql::SetExpr::SetOperation { left, right, .. } => {
            set_relations_mut(left, f);
            set_relations_mut(right, f);
        }
        _ => {}
    }
}

fn factor_relations_mut(t: &mut TableFactor, f: &mut dyn FnMut(&mut TableFactor)) {
    match t {
        TableFactor::Derived { subquery, .. } => relations_mut(subquery, f),
        TableFactor::NestedJoin { table_with_joins, .. } => {
            factor_relations_mut(&mut table_with_joins.relation, f);
            for j in &mut table_with_joins.joins {
                factor_relations_mut(&mut j.relation, f);
            }
        }
        TableFactor::Table { .. } => f(t),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(text: &str) -> Definition {
        match parse_statement(text).unwrap().unwrap() {
            Statement::Create { def, .. } => def,
            s => panic!("{s:?}"),
        }
    }

    fn set_of(texts: &[&str]) -> FunctionSet {
        let mut set = FunctionSet::default();
        for t in texts {
            let stmt = parse_statement(t).unwrap().unwrap();
            if let Change::Put(f) = change(&set, &stmt, None, None).unwrap() {
                let mut all: Vec<Function> = set.all().map(|f| (**f).clone()).collect();
                all.push(f);
                set = FunctionSet::of(all);
            }
        }
        set
    }

    fn expanded(set: &FunctionSet, q: &str) -> String {
        let q = super::super::sql::parse_query(q).unwrap();
        expand(&q, set).unwrap().map_or_else(|| q.to_string(), |(q, _)| q.to_string())
    }

    #[test]
    fn parses_and_prints_back() {
        for t in [
            "CREATE FUNCTION boosted(s FLOAT, importance FLOAT, w FLOAT DEFAULT 0.2) RETURNS FLOAT AS :s * (1 + :w * :importance)",
            "CREATE OR REPLACE FUNCTION tier(price FLOAT) RETURNS KEYWORD LANGUAGE sql OPTIONS (description = 'price band') AS CASE WHEN :price < 20 THEN 'budget' ELSE 'premium' END;",
            "create function recall(q text, k int default 10) returns table as select id from memories where match(body, :q) limit :k",
            "CREATE FUNCTION one() RETURNS INT AT VERSION 3 AS 1",
            "CREATE FUNCTION v(x VECTOR(8), tags KEYWORD[]) RETURNS FLOAT AS $$ 1.5 $$",
        ] {
            let s = parse_statement(t).unwrap().unwrap();
            assert_eq!(parse_statement(&s.sql()).unwrap().unwrap(), s, "{t}");
        }
        let d = create("CREATE FUNCTION boosted(s FLOAT, w FLOAT DEFAULT 0.2) RETURNS FLOAT AS :s*(1+:w)");
        assert_eq!(d.body, ":s * (1 + :w)");
        assert_eq!(d.params[1].default.as_deref(), Some("0.2"));
        assert_eq!(create("CREATE FUNCTION f(s TEXT DEFAULT 'it''s') RETURNS TEXT AS :s").params[0].default.as_deref(), Some("'it''s'"));
        for t in ["DROP FUNCTION f", "DROP FUNCTION IF EXISTS f VERSION 2", "DROP FUNCTION f AT VERSION 4", "CREATE OR REPLACE FUNCTION f FROM VERSION 1", "CREATE OR REPLACE FUNCTION f FROM VERSION 1 AT VERSION 5"] {
            let s = parse_statement(t).unwrap().unwrap();
            assert_eq!(s.sql(), t);
        }
        assert_eq!(parse_statement("CREATE TABLE t (a INT)").unwrap(), None);
        assert_eq!(parse_statement("CREATE TABLE function_calls (a INT)").unwrap(), None);
        assert!(parse_statement("-- a note\n/* and */ create function f() returns int as 1").unwrap().is_some());
        assert_eq!(parse_statement("SELECT 1").unwrap(), None);
        assert_eq!(parse_statement("DROP TABLE t").unwrap(), None);
    }

    #[test]
    fn refuses_bad_definitions() {
        for (t, why) in [
            ("CREATE FUNCTION lower(s TEXT) RETURNS TEXT AS :s", "built-in"),
            ("CREATE FUNCTION f(s TEXT) RETURNS TEXT AS name", "reads name, which is no parameter"),
            ("CREATE FUNCTION f(s TEXT) RETURNS TEXT AS :t", "not one of its parameters"),
            ("CREATE FUNCTION f(s BLOB) RETURNS TEXT AS :s", "BLOB is not a type"),
            ("CREATE FUNCTION f(s TEXT) RETURNS TEXT LANGUAGE wasm AS :s", "not supported yet"),
            ("CREATE FUNCTION f(a INT DEFAULT 1, b INT) RETURNS INT AS :a", "needs one too"),
            ("CREATE FUNCTION f(a INT, a INT) RETURNS INT AS :a", "two parameters"),
            ("CREATE FUNCTION f(a INT) RETURNS INT", "AS and the body"),
            ("CREATE FUNCTION f(a INT) RETURNS INT AS SELECT 1 FROM t", "one expression"),
            ("CREATE FUNCTION f(a INT) RETURNS TABLE AS :a + 1", "is a SELECT"),
            ("CREATE FUNCTION f FROM VERSION 1", "OR REPLACE"),
            ("CREATE FUNCTION f(a INT) RETURNS INT OPTIONS (cost = '1') AS :a", "not an option"),
            ("DROP FUNCTION f VERSION 1 AT VERSION 2", "one or the other"),
        ] {
            let e = parse_statement(t).err().unwrap_or_else(|| panic!("{t} parsed")).to_string();
            assert!(e.contains(why), "{t}: {e}");
        }
    }

    #[test]
    fn versions_rise_and_never_change() {
        let set = set_of(&["CREATE FUNCTION f(x INT) RETURNS INT AS :x + 1"]);
        assert_eq!(set.current("F").unwrap().version, 1);
        let again = parse_statement("CREATE FUNCTION f(x INT) RETURNS INT AS :x + 2").unwrap().unwrap();
        assert!(change(&set, &again, None, None).unwrap_err().to_string().contains("CREATE OR REPLACE"));
        // The same definition again is no new version.
        let same = parse_statement("CREATE OR REPLACE FUNCTION f(x INT) RETURNS INT AS :x + 1").unwrap().unwrap();
        assert!(matches!(change(&set, &same, None, None).unwrap(), Change::Nothing));
        let set = set_of(&["CREATE FUNCTION f(x INT) RETURNS INT AS :x + 1", "CREATE OR REPLACE FUNCTION f(x INT) RETURNS INT AS :x + 2", "DROP FUNCTION f", "CREATE FUNCTION f(x INT) RETURNS INT AS :x + 3"]);
        assert_eq!(set.current("f").unwrap().version, 4);
        assert!(set.get("f", 3).unwrap().dropped);
        // A rollback is a new version with an old one's body.
        let back = parse_statement("CREATE OR REPLACE FUNCTION f FROM VERSION 1").unwrap().unwrap();
        let Change::Put(f) = change(&set, &back, None, None).unwrap() else { panic!() };
        assert_eq!((f.version, f.def.body.as_str()), (5, ":x + 1"));
        assert_eq!(f.hash, set.get("f", 1).unwrap().hash);
        // The current version is not deleted; an old one is.
        let del = parse_statement("DROP FUNCTION f VERSION 4").unwrap().unwrap();
        assert!(change(&set, &del, None, None).unwrap_err().to_string().contains("current version"));
        let del = parse_statement("DROP FUNCTION f VERSION 2").unwrap().unwrap();
        assert!(matches!(change(&set, &del, None, None).unwrap(), Change::Delete(id) if id == "f@2"));
        // AT VERSION writes the version once; again it does nothing.
        let at = parse_statement("CREATE OR REPLACE FUNCTION f(x INT) RETURNS INT AT VERSION 6 AS :x + 9").unwrap().unwrap();
        let Change::Put(f6) = change(&set, &at, None, None).unwrap() else { panic!() };
        let mut all: Vec<Function> = set.all().map(|f| (**f).clone()).collect();
        all.push(f6);
        let set = FunctionSet::of(all);
        assert!(matches!(change(&set, &at, None, None).unwrap(), Change::Nothing));
        let other = parse_statement("CREATE OR REPLACE FUNCTION f(x INT) RETURNS INT AT VERSION 6 AS :x").unwrap().unwrap();
        assert!(change(&set, &other, None, None).unwrap_err().to_string().contains("another definition"));
        let old = parse_statement("CREATE OR REPLACE FUNCTION f(x INT) RETURNS INT AT VERSION 5 AS :x").unwrap().unwrap();
        assert!(change(&set, &old, None, None).unwrap_err().to_string().contains("not the next one"));
    }

    #[test]
    fn rows_read_back() {
        let set = set_of(&["CREATE FUNCTION f(x INT, y FLOAT DEFAULT 2.5) RETURNS FLOAT OPTIONS (description = 'add') AS :x + :y", "DROP FUNCTION f"]);
        let schema = crate::storage::functions_schema();
        for f in set.all() {
            let row: BTreeMap<String, Value> = schema.fields.iter().map(|c| c.name.clone()).zip(f.to_row()).collect();
            assert_eq!(&Function::from_row(&row).unwrap(), &**f);
        }
    }

    #[test]
    fn scalar_calls_are_written_out() {
        let set = set_of(&[
            "CREATE FUNCTION boosted(s FLOAT, importance FLOAT, w FLOAT DEFAULT 0.2) RETURNS FLOAT AS :s * (1 + :w * :importance)",
            "CREATE FUNCTION twice(x FLOAT) RETURNS FLOAT AS boosted(:x, 1) * 2",
        ]);
        assert_eq!(
            expanded(&set, "SELECT id FROM m WHERE MATCH(body, :q) ORDER BY boosted(bm25(body, :q), importance) DESC LIMIT 10"),
            "SELECT id FROM m WHERE MATCH(body, :q) ORDER BY (bm25(body, :q) * (1 + 0.2 * importance)) DESC LIMIT 10"
        );
        assert_eq!(expanded(&set, "SELECT boosted(a, b, w => 0.5) FROM m"), "SELECT (a * (1 + 0.5 * b)) FROM m");
        assert_eq!(expanded(&set, "SELECT twice(a + 1) FROM m"), "SELECT (((a + 1) * (1 + 0.2 * 1)) * 2) FROM m");
        // A pinned version, and calls in joins, subqueries and set operations.
        let set2 = set_of(&["CREATE FUNCTION g(x INT) RETURNS INT AS :x + 1", "CREATE OR REPLACE FUNCTION g(x INT) RETURNS INT AS :x + 2"]);
        assert_eq!(expanded(&set2, "SELECT \"g@1\"(a), g(a) FROM m"), "SELECT (a + 1), (a + 2) FROM m");
        assert_eq!(
            expanded(&set2, "SELECT a FROM m JOIN n ON g(m.a) = n.b WHERE a IN (SELECT g(b) FROM n) UNION SELECT g(c) FROM o"),
            "SELECT a FROM m JOIN n ON (m.a + 2) = n.b WHERE a IN (SELECT (b + 2) FROM n) UNION SELECT (c + 2) FROM o"
        );
        assert_eq!(expanded(&set2, "SELECT lower(a) FROM m"), "SELECT lower(a) FROM m");
        for (q, why) in [
            ("SELECT \"g@9\"(a) FROM m", "no version 9"),
            ("SELECT g() FROM m", "needs x"),
            ("SELECT g(1, 2) FROM m", "given more"),
            ("SELECT g(y => 1) FROM m", "no parameter y"),
            ("SELECT g('one') FROM m", "is INT, and is given 'one'"),
            ("SELECT * FROM g(1)", "returns a value, not rows"),
        ] {
            let e = expand(&super::super::sql::parse_query(q).unwrap(), &set2).unwrap_err().to_string();
            assert!(e.contains(why), "{q}: {e}");
        }
    }

    #[test]
    fn table_calls_are_written_out() {
        let set = set_of(&["CREATE FUNCTION recall(q TEXT, k INT DEFAULT 10) RETURNS TABLE AS SELECT id, body FROM memories WHERE MATCH(body, :q) ORDER BY bm25(body, :q) DESC LIMIT :k"]);
        // Alone, the call is the body.
        assert_eq!(expanded(&set, "SELECT * FROM recall('tokio', k => 5)"), "SELECT id, body FROM memories WHERE MATCH(body, 'tokio') ORDER BY bm25(body, 'tokio') DESC LIMIT 5");
        assert_eq!(expanded(&set, "SELECT * FROM recall(:q)"), "SELECT id, body FROM memories WHERE MATCH(body, :q) ORDER BY bm25(body, :q) DESC LIMIT 10");
        assert_eq!(
            expanded(&set, "SELECT r.id, m.created FROM recall(:q) AS r JOIN memories m ON m.id = r.id"),
            "SELECT r.id, m.created FROM (SELECT id, body FROM memories WHERE MATCH(body, :q) ORDER BY bm25(body, :q) DESC LIMIT 10) AS r JOIN memories AS m ON m.id = r.id"
        );
        assert_eq!(expanded(&set, "SELECT count(*) FROM recall('x')"), "SELECT count(*) FROM (SELECT id, body FROM memories WHERE MATCH(body, 'x') ORDER BY bm25(body, 'x') DESC LIMIT 10) AS recall");
        let e = expand(&super::super::sql::parse_query("SELECT * FROM t WHERE recall('x') > 1").unwrap(), &set).unwrap_err().to_string();
        assert!(e.contains("returns rows"), "{e}");
        let e = expand(&super::super::sql::parse_query("SELECT * FROM t JOIN recall(t.a) r ON r.id = t.id").unwrap(), &set).unwrap_err().to_string();
        assert!(e.contains("not a column"), "{e}");
    }

    #[test]
    fn a_function_that_calls_itself_stops() {
        // Written as two versions: the second calls the first's name.
        let set = set_of(&["CREATE FUNCTION f(x INT) RETURNS INT AS :x", "CREATE OR REPLACE FUNCTION f(x INT) RETURNS INT AS f(:x) + 1"]);
        let e = expand(&super::super::sql::parse_query("SELECT f(1) FROM t").unwrap(), &set).unwrap_err().to_string();
        assert!(e.contains("nest more than 16"), "{e}");
    }
}
