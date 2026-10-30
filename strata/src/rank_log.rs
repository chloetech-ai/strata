//! The rank log: what each logged query served, written as it was served,
//! and its export for training.
//!
//! A logged query ([`crate::db::Db::query_logged`]; strata-server's `"log"`)
//! runs with its ranking's inputs added to its columns (`score()` and
//! `features()`), answers with the columns it asked for, and writes one row
//! per row it served to a log table ([`create_sql`]): the request, the time,
//! a context (a user, a session), the row's key, its position, whether it
//! was shown, its score, and its features, a JSON object of the values its
//! ranking read, named by their SQL text. Those are the values the ranking
//! computed, so a model trained on them sees what serving saw, however the
//! tables change afterwards: new rows that move BM25's statistics, counters
//! that grow, rows deleted.
//!
//! [`export`] reads the log back, one record per logged row, with what
//! happened around it as of its time, from an events table: the context's
//! events before it (its history, for a sequence model) and a label from
//! its row's events after it, within a window. A row given a label of its
//! own (`UPDATE rank_log SET label = 3 WHERE ...`: a judge's grade, an
//! agent citing it) keeps it.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde_json::{json, Map, Value as Json};
use sqlparser::ast as sql;

use crate::error::{Error, Result};
use crate::query::exec::ResultSet;
use crate::query::Params;
use crate::schema::{FieldType, Logical, Retain, Schema};
use crate::value::Value;

/// The log table a logged query writes to unless it names another.
pub const TABLE: &str = "rank_log";

/// Contexts whose rows [`export`] reads per query.
const BATCH: usize = 256;

fn bad(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// The statement that makes log table `table` if there is none: the key
/// `id` (`<request>#<position>`), then the request, its time and context,
/// and per row its key, position, whether it was shown, its score, its
/// features (JSON text, stored and not indexed), the client's name for
/// the ranking (`variant`, an A/B arm) and a label written later. Rows are
/// kept sorted by context, which [`export`] reads by. A table made by hand
/// may add columns (`retain`, `shard_by = 'context'` with `context NOT
/// NULL`) as long as it has these ([`check`]).
pub fn create_sql(table: &str) -> Result<String> {
    if table.is_empty() || !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || table.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(bad(format!("a log table's name is letters, digits and _, not {table:?}")));
    }
    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {table} (id KEYWORD PRIMARY KEY, request KEYWORD NOT NULL, ts TIMESTAMP NOT NULL, context KEYWORD, \
         row_key KEYWORD NOT NULL, position INT NOT NULL, shown BOOL NOT NULL, score FLOAT, features KEYWORD OPTIONS(indexed = false), \
         variant KEYWORD, label FLOAT) OPTIONS(cluster_by = 'context')"
    ))
}

/// The types of a log table's columns.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Keyword,
    Int,
    Float,
    Bool,
    Timestamp,
}

const COLUMNS: [(&str, Kind); 11] = [
    ("id", Kind::Keyword),
    ("request", Kind::Keyword),
    ("ts", Kind::Timestamp),
    ("context", Kind::Keyword),
    ("row_key", Kind::Keyword),
    ("position", Kind::Int),
    ("shown", Kind::Bool),
    ("score", Kind::Float),
    ("features", Kind::Keyword),
    ("variant", Kind::Keyword),
    ("label", Kind::Float),
];

/// Refuses a log table without the columns [`create_sql`] makes, of their
/// types, stored, with `id` the key.
pub fn check(schema: &Schema) -> Result<()> {
    let t = &schema.table;
    if schema.primary_key.map(|k| schema.fields[k].name.as_str()) != Some("id") {
        return Err(bad(format!("{t} is not a log table: its primary key is not id ({})", create_sql(t)?)));
    }
    for (name, kind) in COLUMNS {
        let Some(f) = schema.field(name) else { return Err(bad(format!("{t} is not a log table: it has no column {name} ({})", create_sql(t)?))) };
        let fits = match kind {
            Kind::Keyword => f.ty == FieldType::Keyword && !f.multi && f.media.is_none(),
            Kind::Int => f.ty == FieldType::Int && !f.multi && f.logical.is_none(),
            Kind::Float => f.ty == FieldType::Float,
            Kind::Bool => f.logical == Some(Logical::Bool),
            Kind::Timestamp => f.logical == Some(Logical::Timestamp),
        };
        if !fits || !f.stored {
            return Err(bad(format!("{t} is not a log table: column {name} is not as {} makes it", create_sql(t)?)));
        }
    }
    Ok(())
}

/// How a query is logged.
#[derive(Clone, Debug, PartialEq)]
pub struct Log {
    /// The log table, made by the first query logged to it.
    pub table: String,
    /// The request's id, which a client keeps to label its rows later; a
    /// new random one when `None`. Logging a request again rewrites its
    /// rows.
    pub request: Option<String>,
    /// Whom or what the request served: a user, a session, a query class.
    /// The export reads the context's events.
    pub context: Option<String>,
    /// The client's name for the ranking that served (an A/B arm).
    pub variant: Option<String>,
    /// Log this many rows from the query's OFFSET, more than its LIMIT
    /// returns: the rows past the LIMIT are logged as not shown.
    pub candidates: Option<usize>,
    /// When the request was served, in microseconds since the epoch; now
    /// when `None`. Replaying past traffic gives the times it had.
    pub ts: Option<i64>,
}

impl Default for Log {
    fn default() -> Self {
        Log { table: TABLE.into(), request: None, context: None, variant: None, candidates: None, ts: None }
    }
}

impl Log {
    /// A log from JSON (strata-server's `"log"`): `true`, or an object of
    /// `table`, `request`, `context`, `variant`, `candidates` and `ts` (ISO
    /// 8601 text, or microseconds).
    pub fn from_json(j: &Json) -> Result<Log> {
        let mut log = Log::default();
        let m = match j {
            Json::Bool(true) => return Ok(log),
            Json::Object(m) => m,
            _ => return Err(bad("log is true or an object of table, request, context, variant, candidates and ts")),
        };
        let text = |k: &str| -> Result<Option<String>> {
            match m.get(k) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::String(s)) => Ok(Some(s.clone())),
                Some(Json::Number(n)) if k != "table" => Ok(Some(n.to_string())),
                Some(v) => Err(bad(format!("log {k} is text, not {v}"))),
            }
        };
        for k in m.keys() {
            if !["table", "request", "context", "variant", "candidates", "ts"].contains(&k.as_str()) {
                return Err(bad(format!("log has no option {k:?}: table, request, context, variant, candidates, ts")));
            }
        }
        if let Some(t) = text("table")? {
            log.table = t;
        }
        log.request = text("request")?;
        log.context = text("context")?;
        log.variant = text("variant")?;
        log.candidates = match m.get("candidates") {
            None | Some(Json::Null) => None,
            Some(v) => Some(v.as_u64().filter(|&n| n > 0).ok_or_else(|| bad(format!("log candidates is a positive number, not {v}")))? as usize),
        };
        log.ts = m.get("ts").map(time_of).transpose()?.flatten();
        Ok(log)
    }
}

/// A time given as JSON: ISO 8601 text or microseconds since the epoch.
fn time_of(j: &Json) -> Result<Option<i64>> {
    match j {
        Json::Null => Ok(None),
        Json::String(s) => crate::timestamp::parse(s).map(Some).ok_or_else(|| bad(format!("{s:?} is not an ISO 8601 time"))),
        Json::Number(n) => n.as_i64().map(Some).ok_or_else(|| bad(format!("{n} is not a time in microseconds"))),
        v => Err(bad(format!("a time is ISO 8601 text or microseconds, not {v}"))),
    }
}

/// A logged query as it runs ([`prepare`]).
#[derive(Clone, Debug)]
pub struct Prepared {
    /// The query with `score()` and `features()` added to its columns,
    /// and its LIMIT raised to the candidates logged.
    pub sql: String,
    offset: usize,
    /// The rows the query's own LIMIT returns.
    limit: Option<usize>,
}

/// `sql_text` as a logged query runs it: one SELECT ranked by an ORDER BY
/// (rerank phases included), with its ranking's values added to its
/// columns ([`crate::query::sql::with_features`]).
pub fn prepare(sql_text: &str, params: &Params, log: &Log) -> Result<Prepared> {
    let mut q = crate::query::sql::ranked_query(sql_text).ok_or_else(|| bad("a logged query is one SELECT ranked by an ORDER BY"))?;
    let (offset, limit) = crate::query::compound::limits(&q.limit_clause, params)?;
    if let Some(c) = log.candidates {
        let l = limit.ok_or_else(|| bad("candidates are rows past a LIMIT: the query needs one"))?;
        if c > l {
            let offset = match q.limit_clause.take() {
                Some(sql::LimitClause::LimitOffset { offset, .. }) => offset,
                Some(sql::LimitClause::OffsetCommaLimit { offset, .. }) => Some(sql::Offset { value: offset, rows: sql::OffsetRows::None }),
                None => None,
            };
            let n = sql::Expr::value(sql::Value::Number(c.to_string(), false));
            q.limit_clause = Some(sql::LimitClause::LimitOffset { limit: Some(n), offset, limit_by: Vec::new() });
        }
    }
    crate::query::sql::add_features(&mut q)?;
    Ok(Prepared { sql: q.to_string(), offset, limit })
}

/// One row of the log ([`create_sql`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    pub request: String,
    pub ts: i64,
    pub context: Option<String>,
    pub row_key: String,
    pub position: usize,
    pub shown: bool,
    pub score: Option<f64>,
    /// A JSON object of the values the ranking read.
    pub features: String,
    pub variant: Option<String>,
}

impl Entry {
    /// The row as a JSON document of the log table.
    pub fn to_json(&self) -> Json {
        json!({
            "id": self.id, "request": self.request, "ts": crate::timestamp::format(self.ts), "context": self.context,
            "row_key": self.row_key, "position": self.position, "shown": self.shown, "score": self.score,
            "features": self.features, "variant": self.variant,
        })
    }
}

/// What a logged query served ([`serve`]).
#[derive(Clone, Debug)]
pub struct Served {
    /// The answer, with the columns and rows the query asked for.
    pub rs: ResultSet,
    pub request: String,
    /// The rows to log.
    pub entries: Vec<Entry>,
}

/// The answer to a logged query ([`crate::db::Db::query_logged`]).
#[derive(Clone, Debug)]
pub struct Logged {
    pub rs: ResultSet,
    /// The request's id ([`Log::request`]).
    pub request: String,
    /// The log write's sequence number, in a database that numbers its
    /// writes.
    pub seq_no: Option<u64>,
}

/// Splits the answer of a [`prepare`]d query into the answer the query
/// asked for and the rows to log.
pub fn serve(mut rs: ResultSet, p: &Prepared, log: &Log) -> Result<Served> {
    let request = match &log.request {
        Some(r) if r.is_empty() => return Err(bad("a log's request id is not empty")),
        Some(r) => r.clone(),
        None => format!("{:032x}", rand::random::<u128>()),
    };
    let ts = log.ts.unwrap_or_else(crate::timestamp::now);
    // The added columns: score(), then features() (none of which is
    // score()).
    let k = rs.columns.iter().rposition(|c| c == "score()").ok_or_else(|| bad("the logged query lost its score() column"))?;
    let shown = p.limit.unwrap_or(usize::MAX);
    let mut entries = Vec::with_capacity(rs.rows.len());
    for (i, row) in rs.rows.iter().enumerate() {
        let row_key = match rs.keys.get(i) {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Int(x)) => x.to_string(),
            _ => return Err(bad("a logged query's rows have primary keys: query a table that has one")),
        };
        let mut features = Map::new();
        for (name, v) in rs.columns[k + 1..].iter().zip(&row[k + 1..]) {
            features.insert(name.clone(), value_json(v));
        }
        let score = match row[k] {
            Value::Float(x) if x.is_finite() => Some(x),
            Value::Int(x) => Some(x as f64),
            _ => None,
        };
        let position = p.offset + i;
        entries.push(Entry {
            id: format!("{request}#{position}"),
            request: request.clone(),
            ts,
            context: log.context.clone(),
            row_key,
            position,
            shown: i < shown,
            score,
            features: Json::Object(features).to_string(),
            variant: log.variant.clone(),
        });
    }
    let n = rs.rows.len().min(shown);
    rs.rows.truncate(n);
    rs.rows.iter_mut().for_each(|r| r.truncate(k));
    rs.columns.truncate(k);
    rs.keys.truncate(n);
    rs.scores.truncate(n);
    Ok(Served { rs, request, entries })
}

/// A value as JSON, as answers give it: a TIMESTAMP as ISO 8601 text, a
/// number that is not finite as `null`.
fn value_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Int(x) => json!(x),
        Value::Float(x) if x.is_finite() => json!(x),
        Value::Float(_) => Json::Null,
        Value::Str(s) => json!(s),
        Value::Vector(v) => Json::Array(v.iter().map(|x| json!(x)).collect()),
        Value::List(xs) => Json::Array(xs.iter().map(value_json).collect()),
        Value::Bool(b) => json!(b),
        Value::Timestamp(t) => json!(crate::timestamp::format(*t)),
    }
}

/// The events an export reads: a table with a column naming the context
/// (`user`), the row the event was about (`item`, a key of the table the
/// logged queries ranked), when it happened (`time`, a TIMESTAMP or INT
/// of microseconds) and optionally what it was (`action`).
#[derive(Clone, Debug, PartialEq)]
pub struct Events {
    pub table: String,
    pub user: String,
    pub item: String,
    pub time: String,
    pub action: Option<String>,
}

/// What [`export`] reads and adds.
#[derive(Clone, Debug, PartialEq)]
pub struct Export {
    /// The log table.
    pub table: String,
    /// Logged rows served at or after this time (microseconds).
    pub since: Option<i64>,
    /// Logged rows served before this time.
    pub until: Option<i64>,
    pub events: Option<Events>,
    /// The context's last `history` events before each row's time.
    pub history: usize,
    /// A row's label is the best grade of its item's events by the context
    /// in `(ts, ts + window]`, in microseconds; 0 when there is none.
    pub window: Option<i64>,
    /// The grade of each action; an event whose action is not here is not
    /// a label. Empty: every event grades 1.
    pub grades: Vec<(String, f64)>,
}

impl Default for Export {
    fn default() -> Self {
        Export { table: TABLE.into(), since: None, until: None, events: None, history: 0, window: None, grades: Vec::new() }
    }
}

impl Export {
    /// An export from JSON: `table`, `since`, `until` (times as [`Log`]
    /// takes them), `events` (an object of `table`, `user`, `item`, `time`
    /// and `action`), `history` (a number of events), `window` (a period,
    /// `'30 minutes'`, or seconds) and `grades` (an object of action to
    /// grade).
    pub fn from_json(j: &Json) -> Result<Export> {
        let mut x = Export::default();
        let Json::Object(m) = j else { return Err(bad("an export is an object of table, since, until, events, history, window and grades")) };
        for (k, v) in m {
            match k.as_str() {
                "table" => x.table = v.as_str().ok_or_else(|| bad("export table is text"))?.into(),
                "since" => x.since = time_of(v)?,
                "until" => x.until = time_of(v)?,
                "history" => x.history = v.as_u64().ok_or_else(|| bad(format!("export history is a number of events, not {v}")))? as usize,
                "window" => {
                    x.window = match v {
                        Json::Null => None,
                        Json::String(s) => Some(Retain::parse_period(s).map_err(|_| bad(format!("window {s:?} is a whole number and a unit (seconds, minutes, hours, days or weeks), as '30 minutes'")))? * 1_000_000),
                        v => Some(v.as_f64().filter(|s| *s > 0.0).map(|s| (s * 1e6) as i64).ok_or_else(|| bad(format!("export window is a period or seconds, not {v}")))?),
                    }
                }
                "grades" => {
                    let g = v.as_object().ok_or_else(|| bad("export grades is an object of action to grade"))?;
                    x.grades = g.iter().map(|(a, w)| w.as_f64().map(|w| (a.clone(), w)).ok_or_else(|| bad(format!("the grade of {a} is a number")))).collect::<Result<_>>()?;
                }
                "events" => {
                    let e = v.as_object().ok_or_else(|| bad("export events is an object of table, user, item, time and action"))?;
                    let get = |k: &str| e.get(k).and_then(|v| v.as_str()).map(String::from);
                    let need = |k: &str| get(k).ok_or_else(|| bad(format!("export events needs {k}")));
                    for k in e.keys() {
                        if !["table", "user", "item", "time", "action"].contains(&k.as_str()) {
                            return Err(bad(format!("export events has no option {k:?}: table, user, item, time, action")));
                        }
                    }
                    x.events = Some(Events { table: need("table")?, user: need("user")?, item: need("item")?, time: need("time")?, action: get("action") });
                }
                k => return Err(bad(format!("export has no option {k:?}: table, since, until, events, history, window, grades"))),
            }
        }
        Ok(x)
    }
}

/// A logged row as the export reads it.
struct Impression {
    id: String,
    request: String,
    ts: i64,
    context: Option<String>,
    row_key: String,
    position: i64,
    shown: bool,
    score: Option<f64>,
    features: Json,
    variant: Option<String>,
    label: Option<f64>,
}

/// An event of a context.
struct Event {
    time: i64,
    key: Value,
    item: String,
    action: Option<String>,
}

/// Reads the log table and emits one record per logged row (to `out`),
/// context by context, each context's rows in the order they were served:
/// the row's columns, its `features` as an object, its `label`, and with
/// `events` and `history`, the context's last events before the row's
/// time, oldest first (`history_items`, `history_times` in microseconds,
/// `history_actions`). Each query goes through `run`, which should read
/// one snapshot; `tables` are its tables. Returns the records emitted.
pub fn export(run: &mut dyn FnMut(&str, &Params) -> Result<ResultSet>, tables: &[&Schema], x: &Export, out: &mut dyn FnMut(Json) -> Result<()>) -> Result<u64> {
    let t = &x.table;
    let log = tables.iter().find(|s| s.table.eq_ignore_ascii_case(t)).ok_or_else(|| bad(format!("no log table {t}")))?;
    check(log)?;
    let events = match &x.events {
        None => None,
        Some(e) => {
            let s = tables.iter().find(|s| s.table.eq_ignore_ascii_case(&e.table)).ok_or_else(|| bad(format!("no events table {}", e.table)))?;
            let field = |n: &str| s.field(n).ok_or_else(|| bad(format!("{} has no column {n}", e.table)));
            let user = field(&e.user)?;
            let time = field(&e.time)?;
            field(&e.item)?;
            if let Some(a) = &e.action {
                field(a)?;
            }
            if time.ty != FieldType::Int || time.multi || !matches!(time.logical, None | Some(Logical::Timestamp)) {
                return Err(bad(format!("{}.{} is a TIMESTAMP or an INT of microseconds", e.table, e.time)));
            }
            Some((e, user.ty == FieldType::Int, time.logical == Some(Logical::Timestamp)))
        }
    };
    let mut range = String::new();
    let mut params = Params::new();
    if let Some(s) = x.since {
        range.push_str(" AND ts >= :since");
        params = params.set("since", Value::Timestamp(s));
    }
    if let Some(u) = x.until {
        range.push_str(" AND ts < :until");
        params = params.set("until", Value::Timestamp(u));
    }
    let columns = "id, request, ts, context, row_key, position, shown, score, features, variant, label";
    let where_range = range.strip_prefix(" AND ").map(|r| format!(" WHERE {r}")).unwrap_or_default();
    let groups = run(&format!("SELECT context, COUNT(*) FROM {t}{where_range} GROUP BY context"), &params)?;
    let mut contexts: Vec<String> = groups.rows.iter().filter_map(|r| match &r[0] {
        Value::Str(s) => Some(s.clone()),
        _ => None,
    }).collect();
    contexts.sort();
    let mut n = 0u64;
    // Rows without a context have no events.
    let rs = run(&format!("SELECT {columns} FROM {t} WHERE context IS NULL{range}"), &params)?;
    let mut none = impressions(&rs)?;
    sort_served(&mut none);
    for i in &none {
        out(record(i, &[], x))?;
        n += 1;
    }
    for batch in contexts.chunks(BATCH) {
        let list = Value::List(batch.iter().map(|c| Value::Str(c.clone())).collect());
        let rs = run(&format!("SELECT {columns} FROM {t} WHERE context IN (:contexts){range}"), &params.clone().set("contexts", list))?;
        let mut by_context: HashMap<String, Vec<Impression>> = HashMap::new();
        for i in impressions(&rs)? {
            by_context.entry(i.context.clone().unwrap_or_default()).or_default().push(i);
        }
        let mut evs: HashMap<String, Vec<Event>> = HashMap::new();
        if let Some((e, int_users, timestamp)) = &events {
            // Events up to the last row's time plus the window.
            let last = by_context.values().flatten().map(|i| i.ts).max().unwrap_or(i64::MIN);
            let end = last.saturating_add(x.window.unwrap_or(0));
            let users: Vec<Value> = match int_users {
                true => batch.iter().filter_map(|c| c.parse::<i64>().ok().map(Value::Int)).collect(),
                false => batch.iter().map(|c| Value::Str(c.clone())).collect(),
            };
            if !users.is_empty() {
                let action = e.action.as_deref().map(|a| format!(", {a}")).unwrap_or_default();
                let p = Params::new().set("users", Value::List(users)).set("end", if *timestamp { Value::Timestamp(end) } else { Value::Int(end) });
                let rs = run(&format!("SELECT {}, {}, {}{action} FROM {} WHERE {} IN (:users) AND {} <= :end", e.user, e.item, e.time, e.table, e.user, e.time), &p)?;
                for (j, r) in rs.rows.iter().enumerate() {
                    let (Some(user), Some(time)) = (text(&r[0]), time_value(&r[2])) else { continue };
                    let Some(item) = text(&r[1]) else { continue };
                    let action = e.action.as_ref().and_then(|_| text(&r[3]));
                    evs.entry(user).or_default().push(Event { time, key: rs.keys.get(j).cloned().unwrap_or(Value::Null), item, action });
                }
            }
        }
        for c in batch {
            let Some(mut imps) = by_context.remove(c) else { continue };
            sort_served(&mut imps);
            let mut ev = evs.remove(c).unwrap_or_default();
            ev.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| cmp_values(&a.key, &b.key)));
            for i in &imps {
                out(record(i, &ev, x))?;
                n += 1;
            }
        }
    }
    Ok(n)
}

/// A value as text: a key or a context.
fn text(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.clone()),
        Value::Int(x) => Some(x.to_string()),
        _ => None,
    }
}

fn time_value(v: &Value) -> Option<i64> {
    match v {
        Value::Timestamp(t) | Value::Int(t) => Some(*t),
        _ => None,
    }
}

fn cmp_values(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// The log's rows in the order they were served.
fn sort_served(imps: &mut [Impression]) {
    imps.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.request.cmp(&b.request)).then(a.position.cmp(&b.position)));
}

/// The rows of a query of the log's columns.
fn impressions(rs: &ResultSet) -> Result<Vec<Impression>> {
    rs.rows
        .iter()
        .map(|r| {
            let s = |i: usize| text(&r[i]);
            let num = |i: usize| match r[i] {
                Value::Float(x) => Some(x),
                Value::Int(x) => Some(x as f64),
                _ => None,
            };
            let features = match &r[8] {
                Value::Str(f) => serde_json::from_str(f).map_err(|e| bad(format!("a logged row's features are not JSON: {e}")))?,
                _ => Json::Null,
            };
            Ok(Impression {
                id: s(0).unwrap_or_default(),
                request: s(1).unwrap_or_default(),
                ts: time_value(&r[2]).unwrap_or(0),
                context: s(3),
                row_key: s(4).unwrap_or_default(),
                position: match r[5] {
                    Value::Int(p) => p,
                    _ => 0,
                },
                shown: matches!(r[6], Value::Bool(true) | Value::Int(1)),
                score: num(7),
                features,
                variant: s(9),
                label: num(10),
            })
        })
        .collect()
}

/// A logged row's record, with its history and label from `evs`, its
/// context's events in time order. A row without a context has a history
/// of none and no label but its own.
fn record(i: &Impression, evs: &[Event], x: &Export) -> Json {
    let mut m = Map::new();
    m.insert("id".into(), json!(i.id));
    m.insert("request".into(), json!(i.request));
    m.insert("ts".into(), json!(i.ts));
    m.insert("context".into(), json!(i.context));
    m.insert("row_key".into(), json!(i.row_key));
    m.insert("position".into(), json!(i.position));
    m.insert("shown".into(), json!(i.shown));
    m.insert("score".into(), json!(i.score));
    m.insert("variant".into(), json!(i.variant));
    m.insert("features".into(), i.features.clone());
    // The events before the row's time, and those up to the window after.
    let before = evs.partition_point(|e| e.time < i.ts);
    let label = match (i.label, x.window) {
        (Some(l), _) => Some(l),
        (None, Some(w)) if x.events.is_some() && i.context.is_some() => {
            let after = evs.partition_point(|e| e.time <= i.ts);
            let grade = |e: &Event| match x.grades.is_empty() {
                true => Some(1.0),
                false => e.action.as_ref().and_then(|a| x.grades.iter().find(|(g, _)| g == a)).map(|(_, w)| *w),
            };
            let best = evs[after..].iter().take_while(|e| e.time <= i.ts.saturating_add(w)).filter(|e| e.item == i.row_key).filter_map(grade).fold(None, |b: Option<f64>, g| Some(b.map_or(g, |b| b.max(g))));
            Some(best.unwrap_or(0.0))
        }
        _ => None,
    };
    m.insert("label".into(), json!(label));
    if x.events.is_some() && x.history > 0 {
        let h = &evs[before.saturating_sub(x.history)..before];
        m.insert("history_items".into(), json!(h.iter().map(|e| &e.item).collect::<Vec<_>>()));
        m.insert("history_times".into(), json!(h.iter().map(|e| e.time).collect::<Vec<_>>()));
        if x.events.as_ref().is_some_and(|e| e.action.is_some()) {
            m.insert("history_actions".into(), json!(h.iter().map(|e| &e.action).collect::<Vec<_>>()));
        }
    }
    Json::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_made_table_is_a_log_table() {
        let stmts = crate::query::parse::parse(&create_sql("rank_log").unwrap()).unwrap();
        let sqlparser::ast::Statement::CreateTable(ct) = &stmts[0] else { panic!("not a CREATE TABLE") };
        let (schema, _) = crate::query::ddl::create_table(ct, &[]).unwrap();
        check(&schema).unwrap();
        assert!(create_sql("a b").is_err());
        assert!(create_sql("1x").is_err());
    }

    #[test]
    fn logs_from_json() {
        assert_eq!(Log::from_json(&json!(true)).unwrap(), Log::default());
        let l = Log::from_json(&json!({"table": "served", "request": "r1", "context": 42, "candidates": 50, "ts": "2026-10-30T12:00:00Z"})).unwrap();
        assert_eq!((l.table.as_str(), l.request.as_deref(), l.context.as_deref(), l.candidates), ("served", Some("r1"), Some("42"), Some(50)));
        assert_eq!(l.ts, crate::timestamp::parse("2026-10-30T12:00:00Z"));
        assert!(Log::from_json(&json!({"contxt": "u1"})).is_err());
        assert!(Log::from_json(&json!({"candidates": 0})).is_err());
        let x = Export::from_json(&json!({"window": "30 minutes", "history": 20, "grades": {"buy": 3, "click": 1}, "events": {"table": "events", "user": "usr", "item": "item", "time": "ts"}})).unwrap();
        assert_eq!((x.window, x.history, x.grades.len()), (Some(1_800_000_000), 20, 2));
        assert_eq!(Export::from_json(&json!({"window": 1.5})).unwrap().window, Some(1_500_000));
        assert!(Export::from_json(&json!({"events": {"table": "events"}})).is_err());
    }

    #[test]
    fn candidates_raise_the_limit_and_keep_the_offset() {
        let log = Log { candidates: Some(30), ..Log::default() };
        let p = prepare("SELECT id FROM t ORDER BY bm25(body, :q) DESC LIMIT 10 OFFSET 5", &Params::new(), &log).unwrap();
        assert!(p.sql.contains("LIMIT 30 OFFSET 5") && p.sql.contains("score(), features()"), "{}", p.sql);
        assert_eq!((p.offset, p.limit), (5, Some(10)));
        let p = prepare("SELECT id FROM t ORDER BY bm25(body, :q) DESC LIMIT :k", &Params::new().set("k", Value::Int(10)), &Log::default()).unwrap();
        assert_eq!(p.limit, Some(10));
        assert!(prepare("SELECT id FROM t WHERE x = 1", &Params::new(), &log).is_err());
        assert!(prepare("SELECT id FROM t ORDER BY x", &Params::new(), &log).is_err());
    }
}
