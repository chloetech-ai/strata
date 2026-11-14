//! Rollups: a table of counts, sums, least and greatest values per bucket
//! of time and per group, kept up to date from another table as rows
//! arrive ([`crate::db::Db::refresh_rollup`]).
//!
//! ```sql
//! CREATE ROLLUP logs_5m ON logs OPTIONS(lateness = '10 minutes', retain = '400 days') AS
//!   SELECT date_bin('5 minutes', ts) AS bucket, service, severity,
//!          COUNT(*), SUM(latency), MAX(latency)
//!   WHERE severity <> 'DEBUG';
//! SELECT bucket, SUM(count) FROM logs_5m WHERE bucket >= NOW() - INTERVAL '1 day' GROUP BY bucket;
//! REFRESH ROLLUP logs_5m;          -- fold now, without waiting for the background job
//! REFRESH ROLLUP logs_5m FULL;     -- also check every older bucket for rows that came late
//! REFRESH ROLLUP logs_5m REBUILD;  -- fold every bucket again, for changes that kept the counts
//! DROP ROLLUP IF EXISTS logs_5m;
//! ```
//!
//! The rollup is an ordinary table, named by the statement, with one row
//! per bucket and group: the bucket (a TIMESTAMP column, `bucket` unless
//! the SELECT names it), the group's columns, typed as in the table, and
//! a column per aggregate, `count`, `sum_<field>`, `min_<field>` and
//! `max_<field>` unless named with `AS`. `COUNT(f)` counts the rows
//! whose `f` is not NULL, as `count_<field>`, and `AVG(f)` keeps `sum_f`
//! and `count_f`, so that its mean, `SUM(sum_f) / SUM(count_f)`, skips
//! NULLs as `AVG` does. A `count` column (`COUNT(*)`) is always kept: it
//! is how a bucket is found out of date. Queries read it by name and add its rows up: a bucket of
//! an hour holds the twelve rows of five minutes as one with
//! `SUM(count)`, `SUM(sum_latency)`, `MIN(min_latency)` and so on.
//!
//! The bucket is `date_bin('<width>', field)` (any whole number of
//! seconds, minutes, hours, days or weeks, from 1970-01-01) or
//! `date_trunc('second' | 'minute' | 'hour' | 'day', field)` of a
//! TIMESTAMP field of the table. Groups are KEYWORD, INT, BOOL or
//! TIMESTAMP fields; an optional `WHERE` keeps some rows only.
//!
//! The definition is kept as a stored function, `rollup__<name>()`, whose
//! body is the statement, so it travels with the database's functions and
//! needs nothing new on disk. See [`crate::db::Db::refresh_rollup`] for
//! how buckets are kept up to date.
//!
//! Rollups are experimental: their options, columns and refresh may
//! change in a later release.

use sqlparser::ast::{self as sql, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, SelectItem, SetExpr};
use sqlparser::dialect::GenericDialect;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};

use crate::error::{Error, Result};
use crate::schema::{FieldType, Logical, Retain, Schema};

/// The prefix of a rollup's stored function.
pub const PREFIX: &str = "rollup__";

/// How late a row may come, by default, and still be in its bucket at the
/// next refresh, without a full one: ten minutes.
pub const DEFAULT_LATENESS: i64 = 600_000_000;

const CREATE_FORM: &str = "CREATE ROLLUP [IF NOT EXISTS] name ON table [OPTIONS(lateness = '10 minutes', retain = '400 days')] AS SELECT date_bin('5 minutes', ts) [AS bucket], group_field, ..., COUNT(*), SUM(field), MIN(field), MAX(field), AVG(field) [WHERE condition]";
const DROP_FORM: &str = "DROP ROLLUP [IF EXISTS] name";
const REFRESH_FORM: &str = "REFRESH ROLLUP name [FULL | REBUILD]";

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// The function that keeps rollup `name`.
pub fn function_name(name: &str) -> String {
    format!("{PREFIX}{}", name.to_ascii_lowercase())
}

/// A statement about a rollup.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    Create { rollup: Rollup, if_not_exists: bool },
    Drop { name: String, if_exists: bool },
    Refresh { name: String, mode: Refresh },
}

/// Which buckets a refresh compares and folds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Refresh {
    /// Those from the clock less the lateness, and those the rollup has
    /// not folded yet: `REFRESH ROLLUP name`.
    #[default]
    Recent,
    /// Every bucket either side holds: `REFRESH ROLLUP name FULL`.
    Full,
    /// Every bucket either side holds, each folded again whatever its
    /// count, for changes that keep a bucket's count (an UPDATE):
    /// `REFRESH ROLLUP name REBUILD`.
    Rebuild,
}

/// What one aggregate of a rollup keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Count,
    Sum,
    Min,
    Max,
}

impl Kind {
    fn sql(self) -> &'static str {
        match self {
            Kind::Count => "COUNT",
            Kind::Sum => "SUM",
            Kind::Min => "MIN",
            Kind::Max => "MAX",
        }
    }
}

/// A column of aggregates: `kind` of the table's `field` (none for
/// `COUNT(*)`), kept as column `column` of the rollup.
#[derive(Clone, Debug, PartialEq)]
pub struct Agg {
    pub kind: Kind,
    pub field: Option<String>,
    pub column: String,
}

/// A rollup's definition.
#[derive(Clone, Debug, PartialEq)]
pub struct Rollup {
    /// The rollup's table.
    pub name: String,
    /// The table it is of.
    pub base: String,
    /// The TIMESTAMP field of `base` it buckets by.
    pub time: String,
    /// The width of a bucket, in microseconds.
    pub width: i64,
    /// The bucket's column in the rollup.
    pub bucket: String,
    /// The fields of `base` it groups by, also its columns.
    pub keys: Vec<String>,
    pub aggs: Vec<Agg>,
    /// The text of the SELECT's `WHERE`.
    pub filter: Option<String>,
    /// How late a row may come and still be counted at the next refresh,
    /// in microseconds.
    pub lateness: i64,
    /// The rollup's own retention, in seconds.
    pub retain: Option<i64>,
    /// The `CREATE ROLLUP` statement, as the function keeps it.
    pub statement: String,
}

/// The column of the rollup that holds its key: the bucket and the group.
pub const KEY: &str = "rollup_key";

impl Rollup {
    /// The column of counts of rows (`COUNT(*)`).
    pub fn count_column(&self) -> &str {
        &self.aggs.iter().find(|a| a.kind == Kind::Count && a.field.is_none()).expect("a rollup always counts").column
    }

    /// Whether it keeps a count of a field's values (`COUNT(f)`, or the
    /// one `AVG(f)` keeps), which a rollup made before 0.3.4 has no
    /// column for.
    pub fn counts_fields(&self) -> bool {
        self.aggs.iter().any(|a| a.kind == Kind::Count && a.field.is_some())
    }

    /// Itself as its table `roll` keeps it: a rollup made before 0.3.4
    /// has no column for the count of the field its `AVG` reads, and goes
    /// on without it (its mean is then of every row, as it was).
    pub fn fitted(&self, roll: &Schema) -> Rollup {
        let mut r = self.clone();
        r.aggs.retain(|a| !(a.kind == Kind::Count && a.field.is_some()) || roll.field(&a.column).is_some());
        r
    }

    /// The fields of its table it reads: the time, the groups and the
    /// aggregates' fields.
    pub fn fields(&self) -> Vec<&str> {
        let mut out = vec![self.time.as_str()];
        out.extend(self.keys.iter().map(String::as_str));
        out.extend(self.aggs.iter().filter_map(|a| a.field.as_deref()));
        out
    }

    /// Checks it against `base`, its table, and returns the SQL types of
    /// its columns after the key: the bucket, the groups, the aggregates.
    pub fn check(&self, base: &Schema) -> Result<Vec<(String, &'static str)>> {
        let ty = |name: &str| -> Result<(&'static str, bool)> {
            let f = base.field(name).ok_or_else(|| err(format!("rollup {}: {} has no field {name}", self.name, self.base)))?;
            let numeric = |t: &'static str| (t, true);
            Ok(match (&f.ty, f.logical, f.multi) {
                (_, _, true) => return Err(err(format!("rollup {}: {}.{name} holds a list; a rollup groups by and adds up single values", self.name, self.base))),
                (FieldType::Keyword, _, _) => ("KEYWORD", false),
                (FieldType::Int, Some(Logical::Timestamp), _) => numeric("TIMESTAMP"),
                (FieldType::Int, Some(Logical::Bool), _) => ("BOOL", false),
                (FieldType::Int, None, _) => numeric("INT"),
                (FieldType::Float, _, _) => numeric("FLOAT"),
                _ => return Err(err(format!("rollup {}: {}.{name} is not a KEYWORD, INT, FLOAT, BOOL or TIMESTAMP field", self.name, self.base))),
            })
        };
        if ty(&self.time)?.0 != "TIMESTAMP" {
            return Err(err(format!("rollup {}: {}.{} is not a TIMESTAMP field; buckets are of time", self.name, self.base, self.time)));
        }
        let mut cols = vec![(self.bucket.clone(), "TIMESTAMP")];
        for k in &self.keys {
            let (t, _) = ty(k)?;
            if t == "FLOAT" {
                return Err(err(format!("rollup {}: {}.{k} is a FLOAT; group by KEYWORD, INT, BOOL or TIMESTAMP fields", self.name, self.base)));
            }
            cols.push((k.clone(), t));
        }
        for a in &self.aggs {
            let t = match (&a.field, a.kind) {
                (None, _) => "INT",
                (Some(f), Kind::Count) => {
                    ty(f)?;
                    "INT"
                }
                (Some(f), kind) => match ty(f)? {
                    (t, true) if kind != Kind::Sum || t != "TIMESTAMP" => t,
                    _ => return Err(err(format!("rollup {}: {}({f}) needs a number{}", self.name, a.kind.sql(), if kind == Kind::Sum { "" } else { " or a TIMESTAMP" }))),
                },
            };
            cols.push((a.column.clone(), t));
        }
        let mut seen = std::collections::HashSet::new();
        for (c, _) in &cols {
            if c.eq_ignore_ascii_case(KEY) || !seen.insert(c.to_ascii_lowercase()) {
                return Err(err(format!("rollup {}: two columns would be named {c}: name one with AS", self.name)));
            }
        }
        Ok(cols)
    }

    /// The `CREATE TABLE` of the rollup's table.
    pub fn create_table_sql(&self, base: &Schema) -> Result<String> {
        use super::sql::ident;
        let cols = self.check(base)?;
        // Queries read the columns, never the stored rows: keeping no copy
        // of each row halves the rollup.
        let mut parts = vec![format!("{KEY} INT PRIMARY KEY OPTIONS(stored = false)")];
        parts.extend(cols.iter().map(|(c, t)| format!("{} {t} OPTIONS(stored = false)", ident(c))));
        let mut opts = vec![format!("cluster_by = '{}'", self.bucket.replace('\'', "''"))];
        if let Some(s) = self.retain {
            opts.push(format!("retain = '{s} seconds'"));
            opts.push(format!("retain_by = '{}'", self.bucket.replace('\'', "''")));
        }
        Ok(format!("CREATE TABLE {} ({}) OPTIONS({})", ident(&self.name), parts.join(", "), opts.join(", ")))
    }

    /// The query that folds one bucket, `[:rollup_from, :rollup_to)`: a row
    /// per group, the groups' values, then the aggregates.
    pub fn fold_sql(&self) -> String {
        use super::sql::ident;
        let mut items: Vec<String> = self.keys.iter().map(|k| ident(k)).collect();
        items.extend(self.aggs.iter().map(|a| format!("{}({})", a.kind.sql(), a.field.as_deref().map_or("*".to_string(), ident))));
        let group = match self.keys.is_empty() {
            true => String::new(),
            false => format!(" GROUP BY {}", self.keys.iter().map(|k| ident(k)).collect::<Vec<_>>().join(", ")),
        };
        format!("SELECT {} FROM {} WHERE {}{group}", items.join(", "), ident(&self.base), self.window_condition())
    }

    /// `date_bin` of the rollup's time to its buckets.
    fn bin(&self) -> String {
        format!("date_bin('{} microseconds', {})", self.width, super::sql::ident(&self.time))
    }

    /// The query that folds the buckets of `[:rollup_from, :rollup_to)` at
    /// once: a row per bucket and group, the bucket, the groups' values,
    /// then the aggregates.
    pub fn fold_range_sql(&self) -> String {
        use super::sql::ident;
        let mut items = vec![format!("{} AS rollup_bin", self.bin())];
        items.extend(self.keys.iter().map(|k| ident(k)));
        items.extend(self.aggs.iter().map(|a| format!("{}({})", a.kind.sql(), a.field.as_deref().map_or("*".to_string(), ident))));
        let mut group = vec!["rollup_bin".to_string()];
        group.extend(self.keys.iter().map(|k| ident(k)));
        format!("SELECT {} FROM {} WHERE {} GROUP BY {}", items.join(", "), ident(&self.base), self.window_condition(), group.join(", "))
    }

    /// The rows each bucket of `[:rollup_from, :rollup_to)` counts, a row
    /// per bucket that has any.
    pub fn bucket_counts_sql(&self) -> String {
        format!("SELECT {} AS rollup_bin, COUNT(*) FROM {} WHERE {} GROUP BY rollup_bin", self.bin(), super::sql::ident(&self.base), self.window_condition())
    }

    /// The rows of `[:rollup_from, :rollup_to)` the rollup counts.
    pub fn count_sql(&self) -> String {
        format!("SELECT COUNT(*) FROM {} WHERE {}", super::sql::ident(&self.base), self.window_condition())
    }

    fn window_condition(&self) -> String {
        let t = super::sql::ident(&self.time);
        let mut c = format!("{t} >= :rollup_from AND {t} < :rollup_to");
        if let Some(f) = &self.filter {
            c.push_str(&format!(" AND ({f})"));
        }
        c
    }

    /// The newest or oldest time it counts. An aggregate, `MAX(ts)`, reads
    /// every row's time; the first row in order reads the column's index.
    pub fn edge_sql(&self, newest: bool) -> String {
        let t = super::sql::ident(&self.time);
        let filter = self.filter.as_ref().map_or(String::new(), |f| format!(" AND ({f})"));
        format!("SELECT {t} FROM {} WHERE {t} IS NOT NULL{filter} ORDER BY {t}{} LIMIT 1", super::sql::ident(&self.base), if newest { " DESC" } else { "" })
    }

    /// The newest bucket the rollup holds that begins no later than
    /// `:rollup_now`: a bucket of a row dated in the future does not move
    /// a refresh's window past the present.
    pub fn folded_sql(&self) -> String {
        let b = super::sql::ident(&self.bucket);
        format!("SELECT {b} FROM {} WHERE {b} <= :rollup_now ORDER BY {b} DESC LIMIT 1", super::sql::ident(&self.name))
    }

    /// The first time it counts at or after `:rollup_from`, which a refresh
    /// skips to over a span without rows.
    pub fn next_sql(&self) -> String {
        let t = super::sql::ident(&self.time);
        let filter = self.filter.as_ref().map_or(String::new(), |f| format!(" AND ({f})"));
        format!("SELECT {t} FROM {} WHERE {t} >= :rollup_from{filter} ORDER BY {t} LIMIT 1", super::sql::ident(&self.base))
    }

    /// The first bucket the rollup holds at or after `:rollup_from`.
    pub fn next_folded_sql(&self) -> String {
        let b = super::sql::ident(&self.bucket);
        format!("SELECT {b} FROM {} WHERE {b} >= :rollup_from ORDER BY {b} LIMIT 1", super::sql::ident(&self.name))
    }

    /// The bucket that holds time `t`.
    pub fn bucket_of(&self, t: i64) -> i64 {
        t.div_euclid(self.width).saturating_mul(self.width)
    }
}

fn byte_at(text: &str, at: Location) -> usize {
    let mut line_start = 0;
    for _ in 1..at.line {
        line_start += text[line_start..].find('\n').map_or(text.len() - line_start, |n| n + 1);
    }
    line_start + text[line_start..].chars().take((at.column as usize).saturating_sub(1)).map(char::len_utf8).sum::<usize>()
}

/// The rollup statement text `text` is; `None` for any other.
pub fn parse_statement(text: &str) -> Result<Option<Statement>> {
    let head = text.trim_start();
    let words: Vec<String> = head.split_whitespace().take(5).map(str::to_ascii_uppercase).collect();
    let verb = words.first().map(String::as_str);
    if !matches!(verb, Some("CREATE" | "DROP" | "REFRESH")) || words.get(1).map(String::as_str) != Some("ROLLUP") {
        return Ok(None);
    }
    let all = Tokenizer::new(&GenericDialect {}, text).tokenize_with_location().map_err(|e| err(format!("SQL syntax error: {e}")))?;
    let mut toks: Vec<&TokenWithSpan> = all.iter().filter(|t| !matches!(t.token, Token::Whitespace(_))).collect();
    if toks.last().is_some_and(|t| t.token == Token::SemiColon) {
        toks.pop();
    }
    let word = |i: usize| match toks.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_ascii_uppercase(),
        _ => String::new(),
    };
    let name_at = |i: usize| match toks.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) => Some(w.value.clone()),
        _ => None,
    };
    let mut i = 2;
    match verb {
        Some("DROP") => {
            let bad = |m: &str| err(format!("{m}; the form is {DROP_FORM}"));
            let if_exists = word(i) == "IF" && word(i + 1) == "EXISTS";
            if if_exists {
                i += 2;
            }
            let name = name_at(i).ok_or_else(|| bad("expected the rollup's name"))?;
            if i + 1 != toks.len() {
                return Err(bad(&format!("unexpected {}", toks[i + 1].token)));
            }
            Ok(Some(Statement::Drop { name, if_exists }))
        }
        Some("REFRESH") => {
            let bad = |m: &str| err(format!("{m}; the form is {REFRESH_FORM}"));
            let name = name_at(i).ok_or_else(|| bad("expected the rollup's name"))?;
            let mode = match word(i + 1).as_str() {
                "FULL" => Refresh::Full,
                "REBUILD" => Refresh::Rebuild,
                _ => Refresh::Recent,
            };
            let end = i + 1 + (mode != Refresh::Recent) as usize;
            if end != toks.len() {
                return Err(bad(&format!("unexpected {}", toks[end].token)));
            }
            Ok(Some(Statement::Refresh { name, mode }))
        }
        _ => {
            let bad = |m: &str| err(format!("{m}; the form is {CREATE_FORM}"));
            let if_not_exists = word(i) == "IF" && word(i + 1) == "NOT" && word(i + 2) == "EXISTS";
            if if_not_exists {
                i += 3;
            }
            let name = name_at(i).ok_or_else(|| bad("expected the rollup's name"))?;
            if word(i + 1) != "ON" {
                return Err(bad("expected ON table"));
            }
            let base = name_at(i + 2).ok_or_else(|| bad("expected the table after ON"))?;
            i += 3;
            let (mut lateness, mut retain) = (DEFAULT_LATENESS, None);
            if word(i) == "OPTIONS" {
                if toks.get(i + 1).map(|t| &t.token) != Some(&Token::LParen) {
                    return Err(bad("expected OPTIONS (name = value, ...)"));
                }
                i += 2;
                loop {
                    let key = word(i).to_ascii_lowercase();
                    if key.is_empty() || toks.get(i + 1).map(|t| &t.token) != Some(&Token::Eq) {
                        return Err(bad("expected an option, name = 'value'"));
                    }
                    let Some(Token::SingleQuotedString(value)) = toks.get(i + 2).map(|t| &t.token) else {
                        return Err(bad(&format!("expected a period in quotes for {key}, as '10 minutes'")));
                    };
                    let seconds = Retain::parse_period(value).map_err(|e| err(format!("rollup {name}: {key}: {e}")))?;
                    match key.as_str() {
                        "lateness" => lateness = seconds * 1_000_000,
                        "retain" => retain = Some(seconds),
                        // The marks of a rollup with number keys and of one
                        // that counts a field's values ([`marked`]).
                        LAYOUT_MARK | COUNTS_MARK => {}
                        _ => return Err(bad(&format!("no option {key}"))),
                    }
                    i += 3;
                    match toks.get(i).map(|t| &t.token) {
                        Some(Token::Comma) => i += 1,
                        Some(Token::RParen) => {
                            i += 1;
                            break;
                        }
                        _ => return Err(bad("expected , or ) after an option")),
                    }
                }
            }
            if word(i) != "AS" || word(i + 1) != "SELECT" {
                return Err(bad("expected AS SELECT"));
            }
            let select = &text[byte_at(text, toks[i + 1].span.start)..];
            let rollup = of_select(&name, &base, select, lateness, retain, text.trim().trim_end_matches(';').trim())?;
            Ok(Some(Statement::Create { rollup, if_not_exists }))
        }
    }
}

/// The text of a string literal.
fn string_of(e: &Expr) -> Option<&str> {
    match e {
        Expr::Value(v) => match &v.value {
            sql::Value::SingleQuotedString(s) => Some(s),
            _ => None,
        },
        _ => None,
    }
}

fn column_of(e: &Expr) -> Option<String> {
    match e {
        Expr::Identifier(i) => Some(i.value.clone()),
        Expr::CompoundIdentifier(p) if p.len() == 2 => Some(p[1].value.clone()),
        _ => None,
    }
}

fn args_of(f: &sql::Function) -> Option<Vec<&FunctionArgExpr>> {
    if f.over.is_some() || f.filter.is_some() || !f.within_group.is_empty() {
        return None;
    }
    match &f.args {
        FunctionArguments::List(l) if l.duplicate_treatment.is_none() && l.clauses.is_empty() => {
            l.args.iter().map(|a| if let FunctionArg::Unnamed(e) = a { Some(e) } else { None }).collect()
        }
        FunctionArguments::None => Some(Vec::new()),
        _ => None,
    }
}

fn of_select(name: &str, base: &str, select: &str, lateness: i64, retain: Option<i64>, statement: &str) -> Result<Rollup> {
    let bad = |m: String| err(format!("rollup {name}: {m}; the form is {CREATE_FORM}"));
    let sql::Statement::Query(q) = super::sql::parse_one(select)? else { return Err(bad("expected a SELECT".into())) };
    if q.with.is_some() || q.order_by.is_some() || q.limit_clause.is_some() {
        return Err(bad("the SELECT takes no WITH, ORDER BY or LIMIT".into()));
    }
    let SetExpr::Select(s) = &*q.body else { return Err(bad("expected a single SELECT".into())) };
    if s.distinct.is_some() || s.having.is_some() || s.from.len() > 1 {
        return Err(bad("the SELECT takes no DISTINCT, HAVING or join".into()));
    }
    if let Some(t) = s.from.first() {
        match &t.relation {
            sql::TableFactor::Table { name: n, .. } if t.joins.is_empty() && n.to_string().trim_matches('"').eq_ignore_ascii_case(base) => {}
            _ => return Err(bad(format!("the SELECT reads {base}, the table after ON"))),
        }
    }
    let mut rollup = Rollup {
        name: name.to_string(),
        base: base.to_string(),
        time: String::new(),
        width: 0,
        bucket: String::new(),
        keys: Vec::new(),
        aggs: Vec::new(),
        filter: s.selection.as_ref().map(|e| e.to_string()),
        lateness,
        retain,
        statement: statement.to_string(),
    };
    let add = |rollup: &mut Rollup, kind: Kind, field: Option<String>, alias: Option<String>| {
        let column = alias.unwrap_or_else(|| match (&field, kind) {
            (None, _) => "count".to_string(),
            (Some(f), k) => format!("{}_{f}", k.sql().to_ascii_lowercase()),
        });
        if !rollup.aggs.iter().any(|a| a.kind == kind && a.field == field) {
            rollup.aggs.push(Agg { kind, field, column });
        }
    };
    for item in &s.projection {
        let (e, alias) = match item {
            SelectItem::UnnamedExpr(e) => (e, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
            _ => return Err(bad(format!("{item} is not a bucket, a field or an aggregate"))),
        };
        if let Some(c) = column_of(e) {
            if alias.as_ref().is_some_and(|a| !a.eq_ignore_ascii_case(&c)) {
                return Err(bad(format!("a group keeps its field's name ({c})")));
            }
            rollup.keys.push(c);
            continue;
        }
        let Expr::Function(f) = e else { return Err(bad(format!("{e} is not a bucket, a field or an aggregate"))) };
        let fname = f.name.to_string().to_ascii_lowercase();
        let args = args_of(f).ok_or_else(|| bad(format!("{f}: plain arguments only")))?;
        let field_arg = |i: usize| match args.get(i) {
            Some(FunctionArgExpr::Expr(e)) => column_of(e),
            _ => None,
        };
        match fname.as_str() {
            "date_bin" | "date_trunc" => {
                if !rollup.time.is_empty() {
                    return Err(bad("one bucket only".into()));
                }
                let (Some(FunctionArgExpr::Expr(w)), Some(time), 2) = (args.first(), field_arg(1), args.len()) else {
                    return Err(bad(format!("{f}: expected {fname}('<width>', timestamp_field)")));
                };
                let w = string_of(w).ok_or_else(|| bad(format!("{f}: the width is text, as '5 minutes'")))?;
                let seconds = match fname.as_str() {
                    "date_bin" => Retain::parse_period(w).map_err(|e| bad(format!("{f}: {e}")))?,
                    _ => match w.to_ascii_lowercase().as_str() {
                        "second" => 1,
                        "minute" => 60,
                        "hour" => 3600,
                        "day" => 86_400,
                        u => return Err(bad(format!("{f}: date_trunc's buckets are of a second, minute, hour or day, not {u}; date_bin('7 days', ...) is a week from a Thursday"))),
                    },
                };
                if seconds <= 0 {
                    return Err(bad(format!("{f}: a bucket is at least a second")));
                }
                rollup.time = time;
                rollup.width = seconds * 1_000_000;
                rollup.bucket = alias.unwrap_or_else(|| "bucket".into());
            }
            "count" => match (args.as_slice(), field_arg(0)) {
                ([FunctionArgExpr::Wildcard], _) => add(&mut rollup, Kind::Count, None, alias),
                ([_], Some(field)) => add(&mut rollup, Kind::Count, Some(field), alias),
                _ => return Err(bad(format!("{f}: rollups count rows, COUNT(*), or a field's values, COUNT(field)"))),
            },
            "sum" | "min" | "max" | "avg" => {
                let (Some(field), 1) = (field_arg(0), args.len()) else { return Err(bad(format!("{f}: of one field"))) };
                match fname.as_str() {
                    "sum" => add(&mut rollup, Kind::Sum, Some(field), alias),
                    "min" => add(&mut rollup, Kind::Min, Some(field), alias),
                    "max" => add(&mut rollup, Kind::Max, Some(field), alias),
                    _ => {
                        if alias.is_some() {
                            return Err(bad(format!("{f}: AVG keeps a sum and a count of the values, sum_{field} and count_{field}, and takes no name")));
                        }
                        add(&mut rollup, Kind::Sum, Some(field.clone()), None);
                        add(&mut rollup, Kind::Count, Some(field), None);
                    }
                }
            }
            _ => return Err(bad(format!("{f}: a rollup keeps COUNT, SUM, MIN, MAX and AVG"))),
        }
    }
    if rollup.time.is_empty() {
        return Err(bad("no bucket: one item is date_bin('<width>', timestamp_field)".into()));
    }
    if !rollup.aggs.iter().any(|a| a.kind == Kind::Count && a.field.is_none()) {
        rollup.aggs.insert(0, Agg { kind: Kind::Count, field: None, column: "count".into() });
    }
    // GROUP BY, when written, names the groups.
    if let GroupByExpr::Expressions(g, _) = &s.group_by {
        for e in g {
            let named = column_of(e).is_some_and(|c| rollup.keys.iter().any(|k| k.eq_ignore_ascii_case(&c)) || c.eq_ignore_ascii_case(&rollup.bucket));
            let bucket = matches!(e, Expr::Function(_)) || matches!(e, Expr::Value(_));
            if !named && !bucket {
                return Err(bad(format!("GROUP BY {e} is not an item of the SELECT")));
            }
        }
    }
    Ok(rollup)
}

/// The definition kept in function `f`'s body.
pub fn of_function(f: &super::function::Function) -> Result<Option<Rollup>> {
    let Some(text) = super::table_synonyms::rules_of(f) else { return Ok(None) };
    match parse_statement(&text)? {
        Some(Statement::Create { rollup, .. }) => Ok(Some(rollup)),
        _ => Ok(None),
    }
}

/// The option a rollup made since 0.3.3 keeps in its definition: its rows
/// are keyed by number with columns only (`stored = false`), which 0.3.2
/// reads but cannot write (its refresher writes text keys). 0.3.2 refuses
/// a definition with an option it does not know, "no option
/// needs_chloedb_0_3_3", so it says why it does not refresh the rollup
/// rather than serve stale buckets (audit 4, X-I5). This build takes it
/// and reads the layout from the rollup's table.
pub const LAYOUT_MARK: &str = "needs_chloedb_0_3_3";

/// `statement`, a `CREATE ROLLUP`, with [`LAYOUT_MARK`] among its options:
/// what [`Db::create_rollup`](crate::db::Db::create_rollup) keeps.
pub fn marked(statement: &str) -> String {
    with_mark(statement, LAYOUT_MARK)
}

/// The option a rollup made since 0.3.4 keeps when it counts a field's
/// values (`COUNT(f)`, or `AVG(f)`, whose mean skips NULLs): 0.3.3 would
/// write its rows a column short, and refuses it by name instead, "no
/// option needs_chloedb_0_3_4" (audit 4, X-B6).
pub const COUNTS_MARK: &str = "needs_chloedb_0_3_4";

/// `statement` with option `mark` among its options, unless it has it.
pub fn with_mark(statement: &str, mark_name: &str) -> String {
    let Ok(all) = Tokenizer::new(&GenericDialect {}, statement).tokenize_with_location() else { return statement.to_string() };
    let toks: Vec<&TokenWithSpan> = all.iter().filter(|t| !matches!(t.token, Token::Whitespace(_))).collect();
    let word = |i: usize| match toks.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_ascii_uppercase(),
        _ => String::new(),
    };
    if toks.iter().any(|t| matches!(&t.token, Token::Word(w) if w.value.eq_ignore_ascii_case(mark_name))) {
        return statement.to_string();
    }
    // CREATE ROLLUP [IF NOT EXISTS] name ON table, then OPTIONS(...) or AS.
    let at = if word(2) == "IF" { 8 } else { 5 };
    let mark = format!("{mark_name} = '1 second'");
    match (word(at).as_str(), toks.get(at + 1)) {
        ("OPTIONS", Some(open)) if open.token == Token::LParen => {
            let b = byte_at(statement, open.span.end);
            format!("{}{mark}, {}", &statement[..b], &statement[b..])
        }
        _ => match toks.get(at) {
            Some(t) => {
                let b = byte_at(statement, t.span.start);
                format!("{}OPTIONS({mark}) {}", &statement[..b], &statement[b..])
            }
            None => statement.to_string(),
        },
    }
}

/// The `CREATE FUNCTION` that keeps `r`.
pub fn function_sql(r: &Rollup) -> String {
    format!("CREATE OR REPLACE FUNCTION {}() RETURNS TEXT AS '{}'", function_name(&r.name), r.statement.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(text: &str) -> Rollup {
        match parse_statement(text).unwrap() {
            Some(Statement::Create { rollup, .. }) => rollup,
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn a_rollup_reads_its_select() {
        let r = create("CREATE ROLLUP logs_5m ON logs OPTIONS(lateness = '2 minutes', retain = '400 days') AS SELECT date_bin('5 minutes', ts), service, severity, COUNT(*), SUM(latency), AVG(latency), MAX(latency) AS worst WHERE severity <> 'DEBUG' GROUP BY 1, service, severity;");
        assert_eq!((r.name.as_str(), r.base.as_str(), r.time.as_str(), r.width, r.bucket.as_str()), ("logs_5m", "logs", "ts", 300_000_000, "bucket"));
        assert_eq!(r.keys, ["service", "severity"]);
        let cols: Vec<&str> = r.aggs.iter().map(|a| a.column.as_str()).collect();
        assert_eq!(cols, ["count", "sum_latency", "count_latency", "worst"]);
        assert!(r.counts_fields());
        assert_eq!(r.filter.as_deref(), Some("severity <> 'DEBUG'"));
        assert_eq!((r.lateness, r.retain), (120_000_000, Some(400 * 86_400)));
        assert_eq!(r.fold_sql(), "SELECT service, severity, COUNT(*), SUM(latency), COUNT(latency), MAX(latency) FROM logs WHERE ts >= :rollup_from AND ts < :rollup_to AND (severity <> 'DEBUG') GROUP BY service, severity");
        // The function keeps the statement, and gives the same rollup back.
        assert_eq!(create(&r.statement), r);
    }

    #[test]
    fn a_rollup_made_now_is_marked_for_0_3_2() {
        for text in [
            "CREATE ROLLUP logs_5m ON logs AS SELECT date_bin('5 minutes', ts), service, COUNT(*)",
            "CREATE ROLLUP IF NOT EXISTS logs_5m ON logs OPTIONS(lateness = '2 minutes') AS SELECT date_bin('5 minutes', ts), service, COUNT(*)",
        ] {
            let m = marked(text);
            assert!(m.contains("needs_chloedb_0_3_3 = '1 second'"), "{m}");
            assert_eq!(marked(&m), m, "marked once");
            // It reads as the rollup it marks; 0.3.2 answers "no option
            // needs_chloedb_0_3_3" (its parser takes lateness and retain only).
            let (a, b) = (create(text), create(&m));
            assert_eq!((a.lateness, a.retain, a.keys, a.aggs), (b.lateness, b.retain, b.keys, b.aggs));
        }
        assert_eq!(marked("CREATE ROLLUP h ON logs OPTIONS(retain = '1 day') AS SELECT date_trunc('hour', ts), COUNT(*)"), "CREATE ROLLUP h ON logs OPTIONS(needs_chloedb_0_3_3 = '1 second', retain = '1 day') AS SELECT date_trunc('hour', ts), COUNT(*)");
    }

    #[test]
    fn a_rollup_always_counts() {
        let r = create("CREATE ROLLUP h ON logs AS SELECT date_trunc('hour', ts) AS hour, MIN(latency) FROM logs");
        assert_eq!((r.width, r.bucket.as_str(), r.lateness), (3_600_000_000, "hour", DEFAULT_LATENESS));
        assert_eq!(r.aggs[0], Agg { kind: Kind::Count, field: None, column: "count".into() });
        assert_eq!(r.fold_sql(), "SELECT COUNT(*), MIN(latency) FROM logs WHERE ts >= :rollup_from AND ts < :rollup_to");
    }

    #[test]
    fn other_statements_are_not_rollups() {
        assert_eq!(parse_statement("CREATE TABLE rollup (a INT)").unwrap(), None);
        assert_eq!(parse_statement("SELECT * FROM rollup").unwrap(), None);
        assert_eq!(parse_statement("DROP ROLLUP IF EXISTS x").unwrap(), Some(Statement::Drop { name: "x".into(), if_exists: true }));
        assert_eq!(parse_statement("REFRESH ROLLUP x FULL;").unwrap(), Some(Statement::Refresh { name: "x".into(), mode: Refresh::Full }));
        assert_eq!(parse_statement("REFRESH ROLLUP x REBUILD").unwrap(), Some(Statement::Refresh { name: "x".into(), mode: Refresh::Rebuild }));
        assert_eq!(parse_statement("REFRESH ROLLUP x").unwrap(), Some(Statement::Refresh { name: "x".into(), mode: Refresh::Recent }));
    }

    #[test]
    fn bad_rollups_say_why() {
        let bad = |text: &str, says: &str| match parse_statement(text) {
            Err(e) => assert!(e.to_string().contains(says), "{text}: {e}"),
            Ok(s) => panic!("{text}: {s:?}"),
        };
        bad("CREATE ROLLUP r ON logs AS SELECT service, COUNT(*)", "no bucket");
        bad("CREATE ROLLUP r ON logs AS SELECT date_trunc('month', ts), COUNT(*)", "not month");
        bad("CREATE ROLLUP r ON logs AS SELECT date_bin('5 minutes', ts), COUNT(DISTINCT host)", "plain arguments");
        bad("CREATE ROLLUP r ON logs AS SELECT date_bin('5 minutes', ts), quantile(latency, 0.5)", "COUNT, SUM, MIN, MAX and AVG");
        bad("CREATE ROLLUP r ON logs AS SELECT date_bin('5 minutes', ts), COUNT(*) FROM other", "the table after ON");
        bad("CREATE ROLLUP r ON logs OPTIONS(speed = '1 second') AS SELECT date_bin('5 minutes', ts)", "no option speed");
    }
}
