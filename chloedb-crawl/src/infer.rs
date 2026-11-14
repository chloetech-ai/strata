//! The table's schema, from the pages: each field's type read from its
//! values (a price is a number, a date a time, a short label a keyword),
//! the fields most pages have as columns, everything else in a `JSON`
//! column, and fields that turn common later added as columns then.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::extract::Page;

/// A column's type, as Strata's SQL names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Ty {
    Bool,
    Int,
    Float,
    Timestamp,
    Keyword,
    Text,
    #[serde(rename = "keyword[]")]
    KeywordList,
    #[serde(rename = "int[]")]
    IntList,
}

impl Ty {
    pub fn sql(self) -> &'static str {
        match self {
            Ty::Bool => "BOOL",
            Ty::Int => "INT",
            Ty::Float => "FLOAT",
            Ty::Timestamp => "TIMESTAMP",
            Ty::Keyword => "KEYWORD",
            Ty::Text => "TEXT",
            Ty::KeywordList => "KEYWORD[]",
            Ty::IntList => "INT[]",
        }
    }

    pub fn from_strata(ty: &str, multi: bool) -> Option<Ty> {
        Some(match (ty, multi) {
            ("bool", _) => Ty::Bool,
            ("int", false) => Ty::Int,
            ("int", true) => Ty::IntList,
            ("float", _) => Ty::Float,
            ("timestamp", _) => Ty::Timestamp,
            ("keyword", false) => Ty::Keyword,
            ("keyword", true) => Ty::KeywordList,
            ("text", _) => Ty::Text,
            _ => return None,
        })
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The longest a `KEYWORD` value is kept.
const KEYWORD_MAX: usize = 512;

// ---------------------------------------------------------------- values

/// A number written as text: `1,299.00`, `£51.77`, `45%`, `1.234,5 €`.
/// `(value, whole)`, where a price or a fraction is not whole. Text that
/// only looks like a number (`02134`, a long code) is not one.
pub fn parse_number(s: &str) -> Option<(f64, bool)> {
    let mut t = s.trim();
    let mut money = false;
    const SYMBOLS: &[char] = &['$', '£', '€', '¥', '₹', '₩', '₽', '¢', '₺', '₱', '₪'];
    const CODES: &[&str] = &["USD", "EUR", "GBP", "JPY", "CAD", "AUD", "CHF", "CNY", "INR", "BRL", "MXN", "CLP", "SEK", "NOK", "DKK", "NZD", "US$", "R$", "C$", "A$"];
    for _ in 0..2 {
        let before = t;
        if let Some(r) = t.strip_prefix(SYMBOLS) {
            t = r.trim_start();
            money = true;
        }
        if let Some(r) = t.strip_suffix(SYMBOLS) {
            t = r.trim_end();
            money = true;
        }
        for c in CODES {
            if let Some(r) = t.strip_prefix(c) {
                t = r.trim_start();
                money = true;
            }
            if let Some(r) = t.strip_suffix(c) {
                t = r.trim_end();
                money = true;
            }
        }
        if before == t {
            break;
        }
    }
    let pct = t.ends_with('%');
    let t = t.trim_end_matches('%').trim();
    if t.is_empty() || t.len() > 24 {
        return None;
    }
    let (neg, body) = match t.strip_prefix('-').or_else(|| t.strip_prefix('−')) {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if !body.starts_with(|c: char| c.is_ascii_digit()) || !body.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.') {
        return None;
    }
    let commas = body.matches(',').count();
    let dots = body.matches('.').count();
    let grouped = |sep: char, b: &str| -> bool {
        let mut parts = b.split(sep);
        let first = parts.next().unwrap_or("");
        !first.is_empty() && first.len() <= 3 && parts.all(|p| p.len() == 3)
    };
    let plain: String = match (commas, dots) {
        (0, 0) => body.to_string(),
        (0, 1) => body.to_string(),
        // 1,234 or 1,234,567 (grouped), else 12,5 (a decimal comma)
        (_, 0) if grouped(',', body) && !(commas == 1 && money && body.rsplit(',').next().is_some_and(|d| d.len() == 2)) => body.replace(',', ""),
        (1, 0) => body.replace(',', "."),
        // 1,234.56
        (_, 1) if body.find(',') < body.find('.') && grouped(',', body.split('.').next().unwrap_or("")) => body.replace(',', ""),
        // 1.234,56 or 1.234.567
        (1, _) if body.find('.') < body.find(',') && grouped('.', body.split(',').next().unwrap_or("")) => body.replace('.', "").replace(',', "."),
        (0, _) if grouped('.', body) => body.replace('.', ""),
        _ => return None,
    };
    let int_part = plain.split('.').next().unwrap_or("");
    // A leading zero is a code (a zip code, an id), not a number.
    if int_part.len() > 1 && int_part.starts_with('0') {
        return None;
    }
    if int_part.len() > 15 {
        return None;
    }
    let v: f64 = plain.parse().ok()?;
    let v = if neg { -v } else { v };
    let whole = !plain.contains('.') && !money && !pct;
    Some((v, whole))
}

/// A time written as text, as ISO 8601 in UTC: RFC 3339, a date, a date
/// and time without an offset (UTC), RFC 2822, `Nov 7, 2026`.
pub fn parse_time(s: &str) -> Option<String> {
    let t = s.trim();
    if t.len() < 8 || t.len() > 40 || !t.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let ok = |d: DateTime<Utc>| -> Option<String> {
        let y = d.format("%Y").to_string().parse::<i32>().ok()?;
        (1900..=2200).contains(&y).then(|| d.to_rfc3339_opts(SecondsFormat::Secs, true))
    };
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        return ok(d.with_timezone(&Utc));
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M%z", "%Y-%m-%d %H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S %z"] {
        if let Ok(d) = DateTime::parse_from_str(t, f) {
            return ok(d.with_timezone(&Utc));
        }
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(t, f) {
            return ok(d.and_utc());
        }
    }
    if let Ok(d) = DateTime::parse_from_rfc2822(t) {
        return ok(d.with_timezone(&Utc));
    }
    for f in ["%Y-%m-%d", "%Y/%m/%d", "%B %d, %Y", "%b %d, %Y", "%d %B %Y", "%d %b %Y", "%B %e, %Y", "%b %e, %Y", "%A, %B %d, %Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(t, f) {
            return ok(d.and_hms_opt(0, 0, 0)?.and_utc());
        }
    }
    None
}

/// What type a value reads as, and the value as that type.
pub fn classify(v: &Value) -> Option<(Ty, Value)> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some((Ty::Bool, Value::Bool(*b))),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Some((Ty::Int, Value::from(i))),
            None => n.as_f64().map(|f| (Ty::Float, Value::from(f))),
        },
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if s == "true" || s == "false" {
                return Some((Ty::Bool, Value::Bool(s == "true")));
            }
            if let Some((f, whole)) = parse_number(s) {
                return Some(if whole && f.fract() == 0.0 && f.abs() < 9.0e15 { (Ty::Int, Value::from(f as i64)) } else { (Ty::Float, Value::from(f)) });
            }
            if let Some(t) = parse_time(s) {
                return Some((Ty::Timestamp, Value::String(t)));
            }
            let words = s.split_whitespace().count();
            if s.len() <= 64 && words <= 3 {
                Some((Ty::Keyword, Value::String(s.to_string())))
            } else {
                Some((Ty::Text, Value::String(s.to_string())))
            }
        }
        Value::Array(a) => {
            if a.is_empty() || a.len() > 100 {
                return None;
            }
            if a.iter().all(|x| x.as_i64().is_some()) {
                return Some((Ty::IntList, v.clone()));
            }
            if a.iter().all(|x| x.as_str().is_some_and(|s| !s.trim().is_empty() && s.len() <= 64)) {
                return Some((Ty::KeywordList, v.clone()));
            }
            None
        }
        Value::Object(_) => None,
    }
}

/// A value as a column of type `ty` takes it, or `None` when it is not one.
pub fn coerce(v: &Value, ty: Ty) -> Option<Value> {
    if v.is_null() {
        return None;
    }
    let text = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    };
    match ty {
        Ty::Bool => match v {
            Value::Bool(b) => Some(Value::Bool(*b)),
            Value::String(s) if s.trim() == "true" || s.trim() == "false" => Some(Value::Bool(s.trim() == "true")),
            _ => None,
        },
        Ty::Int => match v {
            Value::Number(n) => n.as_i64().map(Value::from).or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15).map(|f| Value::from(f as i64))),
            Value::String(s) => parse_number(s).filter(|(f, _)| f.fract() == 0.0 && f.abs() < 9.0e15).map(|(f, _)| Value::from(f as i64)),
            _ => None,
        },
        Ty::Float => match v {
            Value::Number(n) => n.as_f64().map(Value::from),
            Value::String(s) => parse_number(s).map(|(f, _)| Value::from(f)),
            _ => None,
        },
        Ty::Timestamp => match v {
            Value::String(s) => parse_time(s).map(Value::String),
            _ => None,
        },
        Ty::Keyword => text(v).map(|s| Value::String(cut(s, KEYWORD_MAX))),
        Ty::Text => match v {
            Value::Array(a) => {
                let parts: Vec<String> = a.iter().filter_map(text).collect();
                (!parts.is_empty()).then(|| Value::String(parts.join("\n")))
            }
            v => text(v).map(Value::String),
        },
        Ty::KeywordList => match v {
            Value::Array(a) => {
                let vs: Vec<Value> = a.iter().filter_map(text).map(|s| Value::String(cut(s, KEYWORD_MAX))).collect();
                (!vs.is_empty()).then_some(Value::Array(vs))
            }
            v => text(v).map(|s| Value::Array(vec![Value::String(cut(s, KEYWORD_MAX))])),
        },
        Ty::IntList => match v {
            Value::Array(a) => {
                let vs: Vec<Value> = a.iter().filter_map(|x| coerce(x, Ty::Int)).collect();
                (!vs.is_empty()).then_some(Value::Array(vs))
            }
            v => coerce(v, Ty::Int).map(|x| Value::Array(vec![x])),
        },
    }
}

fn cut(mut s: String, n: usize) -> String {
    if s.len() > n {
        let mut i = n;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        s.truncate(i);
    }
    s
}

// ---------------------------------------------------------------- a page's fields

/// The fields every row has, from the fetch and the page.
pub const BASE: &[(&str, Ty)] = &[
    ("url", Ty::Keyword),
    ("host", Ty::Keyword),
    ("depth", Ty::Int),
    ("status", Ty::Int),
    ("fetched", Ty::Timestamp),
    ("bytes", Ty::Int),
    ("fetch_ms", Ty::Int),
    ("title", Ty::Text),
    ("body", Ty::Text),
    ("words", Ty::Int),
    ("links", Ty::Int),
];

/// The fields a page may have: columns when the sample's pages have them,
/// else kept under `meta` in `data` (and added as columns once common).
pub const META: &[(&str, Ty)] = &[
    ("description", Ty::Text),
    ("headings", Ty::Text),
    ("lang", Ty::Keyword),
    ("canonical", Ty::Keyword),
    ("published", Ty::Timestamp),
    ("modified", Ty::Timestamp),
    ("author", Ty::Keyword),
    ("image", Ty::Keyword),
    ("site_name", Ty::Keyword),
    ("kind", Ty::Keyword),
    ("keywords", Ty::KeywordList),
];

/// Marked-up fields that say what a page's own fields already do.
const SAME_AS_META: &[&str] = &[
    "og.title", "og.description", "og.image", "og.type", "og.site_name", "og.url", "og.locale", "og.article_published_time", "og.article_modified_time", "ld.type", "ld.headline",
    "ld.description", "ld.image", "ld.url", "ld.datePublished", "ld.dateModified", "md.datepublished", "md.url",
];

/// A fetched page: what the fetch says, and the page's fields.
#[derive(Clone, Debug, Serialize)]
pub struct Fetched {
    pub url: String,
    pub host: String,
    pub depth: u32,
    pub status: u16,
    pub fetched: String,
    pub bytes: usize,
    pub fetch_ms: u64,
    #[serde(flatten)]
    pub page: Page,
}

impl Fetched {
    /// The page's own optional fields, by name.
    pub fn meta(&self) -> Vec<(&'static str, Value)> {
        let p = &self.page;
        let s = |v: &Option<String>| v.clone().map(Value::String);
        let mut out = Vec::new();
        let headings = (!p.headings.is_empty()).then(|| Value::String(p.headings.join("\n")));
        let keywords = (!p.keywords.is_empty()).then(|| Value::Array(p.keywords.iter().cloned().map(Value::String).collect()));
        for (name, v) in [
            ("description", s(&p.description)),
            ("headings", headings),
            ("lang", s(&p.lang)),
            ("canonical", s(&p.canonical)),
            ("published", s(&p.published)),
            ("modified", s(&p.modified)),
            ("author", s(&p.author)),
            ("image", s(&p.image)),
            ("site_name", s(&p.site_name)),
            ("kind", s(&p.kind)),
            ("keywords", keywords),
        ] {
            if let Some(v) = v {
                out.push((name, v));
            }
        }
        out
    }

    /// Every field the row's `data` holds, as `path -> value`: the
    /// marked-up ones (`ld.offers.price`), the template's (`page.*`), and
    /// the page's own optional ones (`meta.author`).
    pub fn paths(&self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for (k, v) in &self.page.structured {
            flatten(k, v, &mut out, 0);
        }
        for (k, v) in self.meta() {
            out.push((format!("meta.{k}"), v));
        }
        out
    }
}

fn flatten(prefix: &str, v: &Value, out: &mut Vec<(String, Value)>, depth: usize) {
    match v {
        Value::Object(o) if depth < 5 => {
            for (k, v) in o {
                flatten(&format!("{prefix}.{k}"), v, out, depth + 1);
            }
        }
        Value::Object(_) => {}
        Value::Array(a) if a.iter().any(|x| x.is_object() || x.is_array()) => {}
        v => out.push((prefix.to_string(), v.clone())),
    }
}

// ---------------------------------------------------------------- the schema

/// Where a column's values come from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "path", rename_all = "lowercase")]
pub enum Source {
    /// The fetch and the page: url, title, body, ...
    Base,
    /// A crawl's CSS selector, by its name.
    Rule(String),
    /// A path of `data`: `ld.offers.price`, `specs.upc`, `meta.author`.
    Path(String),
    /// A column the table had that the crawler does not fill.
    Other,
}

#[derive(Clone, Debug, Serialize)]
pub struct Column {
    pub name: String,
    pub ty: Ty,
    pub source: Source,
    /// Of the pages the column was decided on, the share that had it.
    pub fill: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<Value>,
    /// The page count when it was added, for a column added during the
    /// crawl (`None`: the table was made with it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<u64>,
}

/// The table: its columns, the `JSON` column for the rest, and the
/// embedded vector field.
#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub table: String,
    pub columns: Vec<Column>,
    /// The model of `emb`, embedded from the title and body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embed: Option<String>,
    /// Whether the table has a `data JSON` column.
    pub data: bool,
}

/// How the pages' values of one field read.
#[derive(Clone, Debug, Default, Serialize)]
pub struct FieldStats {
    pub pages: u64,
    counts: [u64; 8],
    max_len: usize,
    words: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<Value>,
}

impl FieldStats {
    pub fn add(&mut self, v: &Value) {
        let Some((ty, norm)) = classify(v) else { return };
        self.pages += 1;
        self.counts[ty.index()] += 1;
        if let Value::String(s) = &norm {
            self.max_len = self.max_len.max(s.len());
            self.words += s.split_whitespace().count() as u64;
        }
        if self.example.is_none() {
            self.example = Some(match norm {
                Value::String(s) => Value::String(cut(s, 120)),
                v => v,
            });
        }
    }

    /// The type that fits at least 90% of the values: the narrowest of
    /// bool, int, float (ints too), time, lists, else keyword for short
    /// labels and text for the rest of the text. `None` for values of
    /// mixed kinds.
    pub fn ty(&self) -> Option<Ty> {
        let n = self.pages as f64;
        if n == 0.0 {
            return None;
        }
        let c = |t: Ty| self.counts[t.index()] as f64;
        let fits = |x: f64| x >= 0.9 * n;
        if fits(c(Ty::Bool)) {
            return Some(Ty::Bool);
        }
        if fits(c(Ty::Int)) {
            return Some(Ty::Int);
        }
        if fits(c(Ty::Int) + c(Ty::Float)) {
            return Some(Ty::Float);
        }
        if fits(c(Ty::Timestamp)) {
            return Some(Ty::Timestamp);
        }
        if fits(c(Ty::IntList)) {
            return Some(Ty::IntList);
        }
        if fits(c(Ty::KeywordList) + c(Ty::Keyword)) && c(Ty::KeywordList) > 0.0 {
            return Some(Ty::KeywordList);
        }
        let texts = c(Ty::Keyword) + c(Ty::Text) + c(Ty::Int) + c(Ty::Float) + c(Ty::Timestamp) + c(Ty::Bool);
        if fits(texts) {
            let strings = (c(Ty::Keyword) + c(Ty::Text)).max(1.0);
            let avg_words = self.words as f64 / strings;
            return Some(if self.max_len <= 64 && avg_words <= 3.0 && c(Ty::Text) == 0.0 { Ty::Keyword } else { Ty::Text });
        }
        None
    }
}

/// Field statistics over many pages.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub pages: u64,
    pub fields: HashMap<String, FieldStats>,
}

impl Stats {
    pub fn add(&mut self, f: &Fetched, rules: &[crate::extract::Rule]) {
        self.pages += 1;
        for (path, v) in f.paths() {
            if self.fields.len() < 2000 || self.fields.contains_key(&path) {
                self.fields.entry(path).or_default().add(&v);
            }
        }
        for r in rules {
            if let Some(v) = f.page.rules.get(&r.name) {
                self.fields.entry(format!("rule.{}", r.name)).or_default().add(v);
            }
        }
    }

    /// The share of the pages read so far that have `path` (0 for none).
    pub fn share(&self, path: &str) -> f64 {
        self.fields.get(path).map_or(0.0, |s| s.pages as f64 / self.pages.max(1) as f64)
    }

    /// The fields by how many pages have them, the most first.
    pub fn top(&self, n: usize) -> Vec<(String, f64, Option<Ty>, Option<Value>)> {
        let mut v: Vec<_> = self.fields.iter().map(|(k, s)| (k.clone(), s.pages as f64 / self.pages.max(1) as f64, s.ty(), s.example.clone())).collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}

/// SQL words a column is not named.
const RESERVED: &[&str] = &[
    "all", "and", "as", "asc", "between", "by", "case", "check", "column", "create", "default", "delete", "desc", "distinct", "drop", "else", "end", "exists", "false", "from", "group", "having",
    "if", "in", "index", "insert", "into", "is", "join", "key", "like", "limit", "match", "not", "null", "offset", "on", "or", "order", "primary", "references", "select", "set", "table", "then",
    "to", "true", "union", "update", "values", "when", "where", "with", "add", "alter", "rename", "options", "over", "partition", "range", "rows", "window", "filter", "cross", "inner",
    "outer", "left", "right", "full", "natural", "using", "except", "intersect", "returning", "cast", "interval", "array", "emb", "data",
];

/// A column's name for a `data` path: the path without its source
/// (`ld.offers.price` is `offers_price`), as a SQL name.
pub fn column_name(path: &str) -> String {
    let rest = match path.split_once('.') {
        Some((src, rest)) if ["ld", "og", "md", "specs", "page", "meta", "rule"].contains(&src) => rest,
        _ => path,
    };
    sql_name(&rest.replace('.', "_"))
}

/// Whether `path` is the field column `c` holds, from another source: a
/// column made of a path, both paths named alike without their sources
/// (`ld.location.name`, `md.location.name`).
fn same_field(c: &Column, path: &str) -> bool {
    match &c.source {
        Source::Path(p) => !p.starts_with("meta.") && p != path && column_name(p) == column_name(path),
        _ => false,
    }
}

pub fn sql_name(s: &str) -> String {
    let mut n = crate::extract::slug(s);
    if n.is_empty() {
        n = "field".into();
    }
    if n.starts_with(|c: char| c.is_ascii_digit()) {
        n.insert(0, 'f');
    }
    if RESERVED.contains(&n.as_str()) {
        n.push('_');
    }
    n
}

impl Plan {
    /// The table's plan from a sample of pages: the base columns; the
    /// page's own optional fields any sample page has; each rule; and each
    /// marked-up or learned field that at least `promote_at` of the pages
    /// have, typed by its values.
    pub fn from_sample(table: &str, stats: &Stats, rules: &[crate::extract::Rule], promote_at: f64, embed: Option<String>) -> Plan {
        let mut columns: Vec<Column> = BASE.iter().map(|(n, t)| Column { name: n.to_string(), ty: *t, source: Source::Base, fill: 1.0, example: None, added_at: None }).collect();
        let n = stats.pages.max(1) as f64;
        for (name, ty) in META {
            if let Some(s) = stats.fields.get(&format!("meta.{name}")) {
                columns.push(Column { name: name.to_string(), ty: *ty, source: Source::Path(format!("meta.{name}")), fill: s.pages as f64 / n, example: s.example.clone(), added_at: None });
            }
        }
        for r in rules {
            let s = stats.fields.get(&format!("rule.{}", r.name));
            let ty = s.and_then(FieldStats::ty).unwrap_or(if r.multi { Ty::KeywordList } else { Ty::Text });
            let ty = if r.multi && !matches!(ty, Ty::KeywordList | Ty::IntList) { Ty::KeywordList } else { ty };
            let mut name = sql_name(&r.name);
            while columns.iter().any(|c| c.name == name) {
                name.push('_');
            }
            columns.push(Column { name, ty, source: Source::Rule(r.name.clone()), fill: s.map_or(0.0, |s| s.pages as f64 / n), example: s.and_then(|s| s.example.clone()), added_at: None });
        }
        let mut plan = Plan { table: table.to_string(), columns, embed, data: true };
        for (path, share, ty, _) in stats.top(usize::MAX) {
            if share < promote_at {
                break;
            }
            if let Some(ty) = ty {
                plan.add_path(&path, ty, &stats.fields[&path], n, None);
            }
        }
        plan
    }

    /// Adds a column for a `data` path, unless one holds it already or
    /// the page's own fields do. The column, if added.
    pub fn add_path(&mut self, path: &str, ty: Ty, s: &FieldStats, pages: f64, at: Option<u64>) -> Option<&Column> {
        if path.starts_with("rule.") || SAME_AS_META.contains(&path) || self.columns.iter().any(|c| c.source == Source::Path(path.to_string())) || self.columns.len() >= 120 {
            return None;
        }
        // The same field from another source (`ld.startDate` beside
        // `md.startdate`) is that column's, which [`Plan::row`] fills from
        // either: not a second column (`ld_startdate`) of which one is
        // always NULL.
        if !path.starts_with("meta.") && self.columns.iter().any(|c| same_field(c, path)) {
            return None;
        }
        let meta = path.strip_prefix("meta.").and_then(|m| META.iter().find(|(n, _)| *n == m));
        let (mut name, ty) = match meta {
            Some((n, t)) => (n.to_string(), *t),
            None => (column_name(path), ty),
        };
        if self.columns.iter().any(|c| c.name == name) {
            name = sql_name(&path.replace('.', "_"));
        }
        while self.columns.iter().any(|c| c.name == name) {
            name.push('_');
        }
        self.columns.push(Column { name, ty, source: Source::Path(path.to_string()), fill: s.pages as f64 / pages.max(1.0), example: s.example.clone(), added_at: at });
        self.columns.last()
    }

    /// The plan of a table that exists: its columns as `/v1/tables` gives
    /// them, each matched to what fills it (the base fields by name, a
    /// rule by its column's name, a `data` path by the name it would get).
    pub fn from_existing(table: &str, short_fields: &[Value], rules: &[crate::extract::Rule], embed: Option<String>, known_paths: &[String]) -> Plan {
        let mut columns = Vec::new();
        let mut data = false;
        let mut emb = None;
        for f in short_fields {
            let name = f["name"].as_str().unwrap_or_default().to_string();
            let ty_s = f["type"].as_str().unwrap_or_default();
            if name == "data" && ty_s == "json" {
                data = true;
                continue;
            }
            if ty_s == "vector" {
                emb = embed.clone().or(Some(String::new()));
                continue;
            }
            let Some(ty) = Ty::from_strata(ty_s, f["multi"].as_bool().unwrap_or(false)) else { continue };
            let source = if BASE.iter().any(|(n, _)| *n == name) {
                Source::Base
            } else if META.iter().any(|(n, _)| *n == name) {
                Source::Path(format!("meta.{name}"))
            } else if let Some(r) = rules.iter().find(|r| sql_name(&r.name) == name) {
                Source::Rule(r.name.clone())
            } else if let Some(p) = known_paths.iter().find(|p| sql_name(&p.replace('.', "_")) == name).or_else(|| known_paths.iter().find(|p| column_name(p) == name)) {
                // Its full name (`ld_startdate`), else the short one's path
                // most pages have (`known_paths` come the most first).
                Source::Path(p.clone())
            } else {
                Source::Other
            };
            columns.push(Column { name, ty, source, fill: 0.0, example: None, added_at: None });
        }
        Plan { table: table.to_string(), columns, embed: emb.filter(|e| !e.is_empty()), data }
    }

    /// `CREATE TABLE IF NOT EXISTS` of the plan.
    pub fn create_sql(&self) -> String {
        let mut cols: Vec<String> = Vec::new();
        for c in &self.columns {
            let pk = if c.name == "url" { " PRIMARY KEY" } else { "" };
            cols.push(format!("  {} {}{pk}", c.name, c.ty.sql()));
        }
        if self.data {
            cols.push("  data JSON".into());
        }
        if let Some(m) = &self.embed {
            cols.push(format!("  emb VECTOR OPTIONS(embed = 'title, body', model = '{}')", m.replace('\'', "''")));
        }
        format!("CREATE TABLE IF NOT EXISTS {} (\n{}\n)", self.table, cols.join(",\n"))
    }

    /// `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` of one column.
    pub fn add_sql(&self, c: &Column) -> String {
        format!("ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {}", self.table, c.name, c.ty.sql())
    }

    /// The row of a page: each column's value as its type, and `data`
    /// holding every marked-up, learned and optional field the page has.
    pub fn row(&self, f: &Fetched) -> Map<String, Value> {
        let mut row = Map::new();
        let paths: BTreeMap<String, Value> = f.paths().into_iter().collect();
        for c in &self.columns {
            let v = match &c.source {
                Source::Base => match c.name.as_str() {
                    "url" => Some(Value::String(f.url.clone())),
                    "host" => Some(Value::String(f.host.clone())),
                    "depth" => Some(Value::from(f.depth)),
                    "status" => Some(Value::from(f.status)),
                    "fetched" => Some(Value::String(f.fetched.clone())),
                    "bytes" => Some(Value::from(f.bytes as u64)),
                    "fetch_ms" => Some(Value::from(f.fetch_ms)),
                    "title" => f.page.title.clone().map(Value::String),
                    "body" => (!f.page.body.is_empty()).then(|| Value::String(f.page.body.clone())),
                    "words" => Some(Value::from(f.page.words as u64)),
                    "links" => Some(Value::from(f.page.links_out as u64)),
                    _ => None,
                },
                Source::Rule(r) => f.page.rules.get(r).cloned(),
                // Its path, else the same field from another source (the
                // page's JSON-LD where the column was made of microdata).
                Source::Path(p) => match paths.get(p).and_then(|v| coerce(v, c.ty)) {
                    Some(v) => Some(v),
                    None if !p.starts_with("meta.") => paths.iter().filter(|(q, _)| *q != p && !q.starts_with("meta.") && same_field(c, q)).find_map(|(_, v)| coerce(v, c.ty)),
                    None => None,
                },
                Source::Other => None,
            };
            if let Some(v) = v.and_then(|v| coerce(&v, c.ty)) {
                row.insert(c.name.clone(), v);
            }
        }
        if self.data {
            let mut data = f.page.structured.clone();
            let meta: Map<String, Value> = f.meta().into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            if !meta.is_empty() {
                data.insert("meta".into(), Value::Object(meta));
            }
            if !data.is_empty() {
                row.insert("data".into(), Value::Object(data));
            }
        }
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers() {
        assert_eq!(parse_number("£51.77"), Some((51.77, false)));
        assert_eq!(parse_number("$1,299.00"), Some((1299.0, false)));
        assert_eq!(parse_number("1.234,56 €"), Some((1234.56, false)));
        assert_eq!(parse_number("12,5"), Some((12.5, false)));
        assert_eq!(parse_number("1,234"), Some((1234.0, true)));
        assert_eq!(parse_number("22"), Some((22.0, true)));
        assert_eq!(parse_number("-3"), Some((-3.0, true)));
        assert_eq!(parse_number("45%"), Some((45.0, false)));
        assert_eq!(parse_number("USD 10"), Some((10.0, false)));
        assert_eq!(parse_number("02134"), None, "a zip code");
        assert_eq!(parse_number("a897fe39"), None);
        assert_eq!(parse_number("978-3-16"), None);
        assert_eq!(parse_number("1234567890123456789"), None);
        assert_eq!(parse_number("In stock (22 available)"), None);
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("2026-11-07").as_deref(), Some("2026-11-07T00:00:00Z"));
        assert_eq!(parse_time("2026-11-07T10:30:00+02:00").as_deref(), Some("2026-11-07T08:30:00Z"));
        assert_eq!(parse_time("2026-11-07 10:30:00").as_deref(), Some("2026-11-07T10:30:00Z"));
        assert_eq!(parse_time("Sat, 07 Nov 2026 10:30:00 GMT").as_deref(), Some("2026-11-07T10:30:00Z"));
        assert_eq!(parse_time("Nov 7, 2026").as_deref(), Some("2026-11-07T00:00:00Z"));
        assert_eq!(parse_time("7 November 2026").as_deref(), Some("2026-11-07T00:00:00Z"));
        assert_eq!(parse_time("hello world"), None);
        assert_eq!(parse_time("0001-01-01"), None);
    }

    #[test]
    fn types_of_fields() {
        let ty = |vals: &[Value]| {
            let mut s = FieldStats::default();
            for v in vals {
                s.add(v);
            }
            s.ty()
        };
        assert_eq!(ty(&[json!("£1.00"), json!("£2.50")]), Some(Ty::Float));
        assert_eq!(ty(&[json!("1"), json!("22"), json!(3)]), Some(Ty::Int));
        assert_eq!(ty(&[json!("1"), json!("2.5")]), Some(Ty::Float));
        assert_eq!(ty(&[json!("Poetry"), json!("Travel")]), Some(Ty::Keyword));
        assert_eq!(ty(&[json!("In stock (22 available)"), json!("In stock (3 available)")]), Some(Ty::Text));
        assert_eq!(ty(&[json!("2026-01-01"), json!("2026-02-01T10:00:00Z")]), Some(Ty::Timestamp));
        assert_eq!(ty(&[json!(["a", "b"]), json!("c")]), Some(Ty::KeywordList));
        assert_eq!(ty(&[json!(true), json!(false)]), Some(Ty::Bool));
        assert_eq!(ty(&[json!("a897fe39b1053632"), json!("bc897fe39b1053632")]), Some(Ty::Keyword));
        // One odd value in 10 still makes the column; it is NULL there.
        let mut v: Vec<Value> = (0..9).map(|i| json!(format!("{i}.5"))).collect();
        v.push(json!("n/a"));
        assert_eq!(ty(&v), Some(Ty::Float));
        assert_eq!(coerce(&json!("n/a"), Ty::Float), None);
        assert_eq!(coerce(&json!("£3.10"), Ty::Float), Some(json!(3.1)));
        assert_eq!(coerce(&json!(["x", 2]), Ty::KeywordList), Some(json!(["x", "2"])));
        assert_eq!(coerce(&json!("Nov 7, 2026"), Ty::Timestamp), Some(json!("2026-11-07T00:00:00Z")));
    }

    /// One field from two sources (microdata and JSON-LD) is one column,
    /// filled from either: not `startdate` always NULL beside
    /// `ld_startdate` holding the values (the formula1_com crawl).
    #[test]
    fn one_field_from_two_sources_is_one_column() {
        let mut stats = FieldStats::default();
        stats.add(&json!("2026-03-14T04:00:00Z"));
        let mut plan = Plan { table: "t".into(), columns: Vec::new(), embed: None, data: true };
        plan.add_path("md.startdate", Ty::Timestamp, &stats, 10.0, None).expect("a column");
        plan.add_path("md.location.name", Ty::Keyword, &stats, 10.0, None).expect("a column");
        assert!(plan.add_path("ld.startDate", Ty::Timestamp, &stats, 10.0, None).is_none(), "the same field");
        assert!(plan.add_path("ld.location.name", Ty::Keyword, &stats, 10.0, None).is_none());
        assert_eq!(plan.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["startdate", "location_name"]);
        // A page with only the JSON-LD fills them.
        let mut page = crate::extract::Page::default();
        page.structured.insert("ld".into(), json!({"startDate": "2026-03-14T04:00:00Z", "location": {"name": "Albert Park"}}));
        page.structured.insert("md".into(), json!({"startdate": "soon"}));
        let f = Fetched { url: "https://x/1".into(), host: "x".into(), depth: 0, status: 200, fetched: "2026-11-13T00:00:00Z".into(), bytes: 1, fetch_ms: 1, page };
        let row = plan.row(&f);
        assert_eq!(row.get("startdate"), Some(&json!("2026-03-14T04:00:00Z")), "the microdata's is no time: the JSON-LD's is");
        assert_eq!(row.get("location_name"), Some(&json!("Albert Park")));
        // A table made before: its columns matched by their full names first.
        let fields = vec![json!({"name": "url", "type": "keyword"}), json!({"name": "startdate", "type": "timestamp"}), json!({"name": "ld_startdate", "type": "timestamp"})];
        let known = vec!["ld.startDate".to_string(), "md.startdate".to_string()];
        let p = Plan::from_existing("t", &fields, &[], None, &known);
        let src = |n: &str| p.columns.iter().find(|c| c.name == n).map(|c| c.source.clone());
        assert_eq!(src("ld_startdate"), Some(Source::Path("ld.startDate".into())));
        assert_eq!(src("startdate"), Some(Source::Path("ld.startDate".into())), "the path most pages have");
    }

    #[test]
    fn names() {
        assert_eq!(column_name("ld.offers.price"), "offers_price");
        assert_eq!(column_name("specs.number_of_reviews"), "number_of_reviews");
        assert_eq!(column_name("md.select"), "select_");
        assert_eq!(column_name("specs.2nd_edition"), "f2nd_edition");
    }
}
