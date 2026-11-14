//! How a connector's records become rows of its target table: paths into
//! a record, templates, a few conversions, and the target's own types.
//!
//! A record is the JSON a connector read (an issue, a series' point, an
//! alert, an email). Its `map` names each column of the row and how to
//! fill it:
//!
//! - `"$.fields.summary"`: a path into the record (`$` is the record,
//!   `.name` or `["name"]` a key, `[0]` an element, `[*]` every element);
//! - `"{key}-{fields.status.name}"`: a template, each `{path}` replaced by
//!   the path's value as text (a path in a template has no `$`);
//! - any other JSON value: itself;
//! - an object, for more: `{"path": ...}`, `{"template": ...}`, `{"const":
//!   ...}` or `{"now": true}` for the value, then, in this order, `"split":
//!   ","` (text into a list), `"values": {"-5": "new", ...}` (a value to
//!   another, by its text; `"*"` for any other), `"as"` (`text`, `int`,
//!   `float`, `bool`, `time`, `list`, `first`, `json`, `lower`, `upper`,
//!   `hash`, `text_of`) and `"default"` (when it came out null).
//!
//! A column the map does not name is left out of the row; with no map at
//! all, the record is the row. Either way the row is then fitted to the
//! target's columns ([`fit`]): a column the table does not have is dropped
//! (and counted, so a run says which), a time becomes ISO 8601 text for a
//! `TIMESTAMP`, a number in text becomes a number for an `INT` or `FLOAT`,
//! one value becomes a list for a `KEYWORD[]` or `INT[]`, and an object or
//! a list becomes text for a `TEXT` or `KEYWORD`.

use chloedb::schema::{FieldType, Logical, Schema};
use serde_json::{json, Map, Value as Json};
use sha2::{Digest, Sha256};

/// The value at `path` in `record`: `Json::Null` where there is none, a
/// list where the path has a `[*]`.
pub fn path(record: &Json, path: &str) -> Json {
    let p = path.trim();
    let p = p.strip_prefix('$').unwrap_or(p);
    let steps = match steps(p) {
        Some(s) => s,
        None => return Json::Null,
    };
    walk(record, &steps)
}

#[derive(Debug, PartialEq)]
enum Step {
    Key(String),
    Index(usize),
    All,
}

fn steps(p: &str) -> Option<Vec<Step>> {
    let b = p.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'.' => {
                i += 1;
                let start = i;
                while i < b.len() && b[i] != b'.' && b[i] != b'[' {
                    i += 1;
                }
                if i > start {
                    out.push(Step::Key(p[start..i].to_string()));
                }
            }
            b'[' => {
                let end = p[i..].find(']')? + i;
                let inner = p[i + 1..end].trim();
                if inner == "*" {
                    out.push(Step::All);
                } else if let Ok(n) = inner.parse::<usize>() {
                    out.push(Step::Index(n));
                } else {
                    let q = inner.trim_matches(|c| c == '"' || c == '\'');
                    out.push(Step::Key(q.to_string()));
                }
                i = end + 1;
            }
            _ => {
                // A first key without its dot: `fields.summary`.
                let start = i;
                while i < b.len() && b[i] != b'.' && b[i] != b'[' {
                    i += 1;
                }
                out.push(Step::Key(p[start..i].to_string()));
            }
        }
    }
    Some(out)
}

fn walk(v: &Json, steps: &[Step]) -> Json {
    let Some((first, rest)) = steps.split_first() else { return v.clone() };
    match first {
        Step::Key(k) => v.get(k.as_str()).map_or(Json::Null, |x| walk(x, rest)),
        Step::Index(n) => v.get(*n).map_or(Json::Null, |x| walk(x, rest)),
        Step::All => match v {
            Json::Array(a) => Json::Array(a.iter().map(|x| walk(x, rest)).filter(|x| !x.is_null()).flat_map(|x| if rest.iter().any(|s| *s == Step::All) { x.as_array().cloned().unwrap_or_else(|| vec![x]) } else { vec![x] }).collect()),
            Json::Object(o) => Json::Array(o.values().map(|x| walk(x, rest)).filter(|x| !x.is_null()).collect()),
            _ => Json::Null,
        },
    }
}

/// The records at `records` in `body`: its list, or the body itself as
/// one record when the path finds an object.
pub fn records(body: &Json, records: Option<&str>) -> Vec<Json> {
    let found = match records {
        Some(p) if !p.trim().is_empty() && p.trim() != "$" => path(body, p),
        _ => body.clone(),
    };
    match found {
        Json::Array(a) => a,
        Json::Null => Vec::new(),
        v => vec![v],
    }
}

/// Why `records` finds nothing in `body`, when it is a path that matched
/// nothing there (no key of that name: not a list that is empty): the
/// path and what the body has instead, for a message. None when it
/// matched, or names no path, or the body is empty.
pub fn unmatched(body: &Json, records: Option<&str>) -> Option<String> {
    let p = records.map(str::trim).filter(|p| !p.is_empty() && *p != "$")?;
    if body.is_null() || !path(body, p).is_null() {
        return None;
    }
    let has = match body {
        Json::Object(o) if o.is_empty() => "the body is an empty object".to_string(),
        Json::Object(o) => {
            let mut keys: Vec<&str> = o.keys().map(String::as_str).take(12).collect();
            if o.len() > keys.len() {
                keys.push("...");
            }
            format!("the top-level keys are {}", keys.join(", "))
        }
        Json::Array(a) => format!("the body is a list of {} (\"records\": \"$\" reads it)", a.len()),
        v => format!("the body is {}", v.to_string().chars().take(60).collect::<String>()),
    };
    Some(format!("the records path {p} matched nothing; {has}"))
}

/// A value as text: a string as it is, a number or a bool as JSON writes
/// it, null as nothing, anything else as compact JSON.
pub fn text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        Json::Number(_) | Json::Bool(_) => v.to_string(),
        Json::Array(a) if a.iter().all(|x| !x.is_object() && !x.is_array()) => a.iter().map(text).collect::<Vec<_>>().join(","),
        _ => v.to_string(),
    }
}

/// `template` with each `{path}` replaced by its value in `record`.
pub fn template(record: &Json, template: &str) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        match rest[open..].find('}') {
            Some(close) => {
                let p = &rest[open + 1..open + close];
                out.push_str(&text(&path(record, p)));
                rest = &rest[open + close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether a string spec is a path (`$...`) or a template (`...{x}...`).
fn string_spec(record: &Json, s: &str) -> Json {
    if s.starts_with('$') {
        path(record, s)
    } else if s.contains('{') && s.contains('}') {
        Json::String(template(record, s))
    } else {
        Json::String(s.to_string())
    }
}

/// The value `spec` makes of `record` ([`self`]).
pub fn eval(record: &Json, spec: &Json, now_us: i64) -> Result<Json, String> {
    let o = match spec {
        Json::String(s) => return Ok(string_spec(record, s)),
        Json::Object(o) => o,
        v => return Ok(v.clone()),
    };
    let mut v = if let Some(p) = o.get("path") {
        path(record, p.as_str().ok_or("a map's \"path\" is text")?)
    } else if let Some(t) = o.get("template") {
        Json::String(template(record, t.as_str().ok_or("a map's \"template\" is text")?))
    } else if let Some(c) = o.get("const") {
        c.clone()
    } else if o.get("now").and_then(Json::as_bool) == Some(true) {
        Json::String(chloedb::timestamp::format(now_us))
    } else {
        // `{"as": "hash"}` alone: of the whole record.
        record.clone()
    };
    if let Some(sep) = o.get("split").and_then(Json::as_str) {
        if let Json::String(s) = &v {
            v = Json::Array(s.split(sep).map(str::trim).filter(|x| !x.is_empty()).map(|x| json!(x)).collect());
        }
    }
    if let Some(Json::Object(values)) = o.get("values") {
        let one = |x: &Json| -> Json {
            let k = text(x);
            values.get(&k).or_else(|| values.get(&k.to_ascii_lowercase())).or_else(|| values.get("*")).cloned().unwrap_or_else(|| x.clone())
        };
        v = match v {
            Json::Array(a) => Json::Array(a.iter().map(one).collect()),
            Json::Null => Json::Null,
            x => one(&x),
        };
    }
    if let Some(as_) = o.get("as").and_then(Json::as_str) {
        v = convert(v, as_)?;
    }
    if v.is_null() || v.as_str() == Some("") {
        if let Some(d) = o.get("default") {
            v = d.clone();
        }
    }
    Ok(v)
}

/// `v` converted `as` ([`self`]).
pub fn convert(v: Json, as_: &str) -> Result<Json, String> {
    Ok(match as_ {
        "text" => match v {
            Json::Null => Json::Null,
            v => Json::String(text(&v)),
        },
        "lower" => match v {
            Json::String(s) => Json::String(s.to_lowercase()),
            v => v,
        },
        "upper" => match v {
            Json::String(s) => Json::String(s.to_uppercase()),
            v => v,
        },
        "int" => number(&v).map_or(Json::Null, |x| json!(x.round() as i64)),
        "float" => number(&v).map_or(Json::Null, |x| json!(x)),
        "bool" => match &v {
            Json::Bool(_) => v,
            Json::Null => Json::Null,
            x => match text(x).trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "yes" | "y" | "1" | "on" => json!(true),
                "false" | "f" | "no" | "n" | "0" | "off" | "" => json!(false),
                _ => Json::Null,
            },
        },
        "time" => time(&v).map_or(Json::Null, |us| Json::String(chloedb::timestamp::format(us))),
        "list" => match v {
            Json::Array(_) | Json::Null => v,
            x => Json::Array(vec![x]),
        },
        "first" => match v {
            Json::Array(a) => a.into_iter().next().unwrap_or(Json::Null),
            x => x,
        },
        "json" => match v {
            Json::String(s) => serde_json::from_str(&s).unwrap_or(Json::String(s)),
            x => x,
        },
        "hash" => Json::String(hash(&v)),
        "text_of" => Json::String(text_of(&v)),
        other => return Err(format!("{other:?} is not a conversion: text, int, float, bool, time, list, first, json, lower, upper, hash or text_of")),
    })
}

/// The first 16 hex digits of the SHA-256 of `v`'s compact JSON (of the
/// text itself for a string).
pub fn hash(v: &Json) -> String {
    let bytes = match v {
        Json::String(s) => s.as_bytes().to_vec(),
        v => v.to_string().into_bytes(),
    };
    let d = Sha256::digest(&bytes);
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Every string of `v`, deep, one per line: what a full-text column of a
/// record keeps.
pub fn text_of(v: &Json) -> String {
    fn go(v: &Json, out: &mut Vec<String>) {
        match v {
            Json::String(s) if !s.trim().is_empty() => out.push(s.clone()),
            Json::Array(a) => a.iter().for_each(|x| go(x, out)),
            Json::Object(o) => o.values().for_each(|x| go(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    go(v, &mut out);
    out.join("\n")
}

/// A number of `v`: itself, or its text read as one.
pub fn number(v: &Json) -> Option<f64> {
    match v {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse::<f64>().ok().filter(|x| x.is_finite()),
        Json::Bool(b) => Some(*b as i64 as f64),
        _ => None,
    }
}

/// A time of `v`, in microseconds since 1970: ISO 8601 text (with or
/// without an offset, `T` or a space, `+0000` or `+00:00`), ServiceNow's
/// `2026-11-13 03:00:00` (UTC), iCalendar's `20261113T030000Z`, an email's
/// `Thu, 13 Nov 2026 03:00:00 +0000`, Oxidized's `2026-11-13 03:00:00
/// UTC`; or a number of seconds, milliseconds or microseconds since 1970
/// (told apart by size).
pub fn time(v: &Json) -> Option<i64> {
    match v {
        Json::Number(n) => {
            let x = n.as_f64()?;
            Some(if x.abs() < 1e11 {
                (x * 1e6) as i64
            } else if x.abs() < 1e14 {
                (x * 1e3) as i64
            } else {
                x as i64
            })
        }
        Json::String(s) => time_text(s),
        _ => None,
    }
}

/// [`time`] of text.
pub fn time_text(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.bytes().all(|c| c.is_ascii_digit() || c == '.' as u8) && s.len() >= 9 {
        return time(&json!(s.parse::<f64>().ok()?));
    }
    let t = s.strip_suffix(" UTC").or_else(|| s.strip_suffix(" GMT")).unwrap_or(s);
    if let Some(us) = chloedb::timestamp::parse(t) {
        return Some(us);
    }
    // 2026-11-13T03:00:00.000+0000 with more than six decimals, or a
    // trailing zone name: cut what the parser does not read.
    if let Some(us) = ical(t) {
        return Some(us);
    }
    rfc2822(t)
}

/// iCalendar's `20261113T030000Z` (UTC) or `20261113` (a date).
fn ical(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r).filter(|x| x.bytes().all(|c| c.is_ascii_digit())).and_then(|x| x.parse().ok()) };
    if b.len() == 8 {
        return civil(digits(0..4)?, digits(4..6)?, digits(6..8)?, 0, 0, 0);
    }
    if b.len() >= 15 && b[8] == b'T' {
        let us = civil(digits(0..4)?, digits(4..6)?, digits(6..8)?, digits(9..11)?, digits(11..13)?, digits(13..15)?)?;
        return match &s[15..] {
            "" | "Z" | "z" => Some(us),
            _ => None,
        };
    }
    None
}

/// An email's date: `Thu, 13 Nov 2026 03:00:00 +0000` (the day's name and
/// the seconds optional; a zone `+hhmm`, `-hhmm`, `UT`, `GMT`, `Z` or a
/// US zone name).
pub fn rfc2822(s: &str) -> Option<i64> {
    let s = s.split('(').next()?.trim();
    let s = match s.find(',') {
        Some(i) => &s[i + 1..],
        None => s,
    };
    let mut it = s.split_whitespace();
    let day: i64 = it.next()?.parse().ok()?;
    let mon = it.next()?.to_ascii_lowercase();
    let month = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"].iter().position(|m| mon.starts_with(m))? as i64 + 1;
    let mut year: i64 = it.next()?.parse().ok()?;
    if year < 100 {
        year += if year < 50 { 2000 } else { 1900 };
    }
    let hms: Vec<i64> = it.next()?.split(':').map(|x| x.parse().ok()).collect::<Option<Vec<_>>>()?;
    let (h, m, sec) = (*hms.first()?, *hms.get(1)?, hms.get(2).copied().unwrap_or(0));
    let zone = it.next().unwrap_or("+0000");
    let off_min: i64 = match zone.to_ascii_uppercase().as_str() {
        "UT" | "UTC" | "GMT" | "Z" => 0,
        "EST" => -300,
        "EDT" => -240,
        "CST" => -360,
        "CDT" => -300,
        "MST" => -420,
        "MDT" => -360,
        "PST" => -480,
        "PDT" => -420,
        z if (z.starts_with('+') || z.starts_with('-')) && z.len() == 5 => {
            let n: i64 = z[1..].parse().ok()?;
            let v = (n / 100) * 60 + n % 100;
            if z.starts_with('-') {
                -v
            } else {
                v
            }
        }
        _ => 0,
    };
    Some(civil(year, month, day, h, m, sec)? - off_min * 60_000_000)
}

/// Microseconds since 1970 of a UTC date and time.
pub fn civil(y: i64, m: i64, d: i64, h: i64, min: i64, s: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || s > 60 {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + min) * 60_000_000 + s * 1_000_000)
}

/// The row `map` makes of `record`: the record itself with no map.
pub fn row(record: &Json, map: Option<&Map<String, Json>>, now_us: i64) -> Result<Map<String, Json>, String> {
    match map {
        None => Ok(match record {
            Json::Object(o) => o.clone(),
            v => {
                let mut m = Map::new();
                m.insert("value".into(), v.clone());
                m
            }
        }),
        Some(map) => {
            let mut out = Map::new();
            for (col, spec) in map {
                let v = eval(record, spec, now_us).map_err(|e| format!("column {col}: {e}"))?;
                if !v.is_null() {
                    out.insert(col.clone(), v);
                }
            }
            Ok(out)
        }
    }
}

/// What [`fit`] did to a row.
#[derive(Default, Debug)]
pub struct Fitted {
    /// Columns the table does not have, left out.
    pub dropped: Vec<String>,
}

/// `row` fitted to `schema`'s columns ([`self`]); an error for a row
/// without the table's primary key.
pub fn fit(mut row: Map<String, Json>, schema: &Schema, fitted: &mut Fitted) -> Result<Map<String, Json>, String> {
    let names: Vec<String> = row.keys().cloned().collect();
    for name in names {
        let Some(field) = schema.fields.iter().find(|f| f.name.eq_ignore_ascii_case(&name)) else {
            row.remove(&name);
            if !fitted.dropped.contains(&name) {
                fitted.dropped.push(name);
            }
            continue;
        };
        let v = row.remove(&name).unwrap_or(Json::Null);
        let v = fit_value(v, &field.ty, field.logical, field.multi);
        if !v.is_null() {
            row.insert(field.name.clone(), v);
        }
    }
    if let Some(pk) = schema.primary_key.and_then(|i| schema.fields.get(i)) {
        match row.get(&pk.name) {
            None | Some(Json::Null) => return Err(format!("the row has no {} (the primary key of {}): map one, as \"{}\": \"{{...}}\"", pk.name, schema.table, pk.name)),
            Some(Json::String(s)) if s.is_empty() => return Err(format!("the row's {} (the primary key of {}) is empty", pk.name, schema.table)),
            _ => {}
        }
    }
    Ok(row)
}

fn fit_value(v: Json, ty: &FieldType, logical: Option<Logical>, multi: bool) -> Json {
    if v.is_null() {
        return v;
    }
    if multi {
        let list = match v {
            Json::Array(a) => a,
            Json::String(s) if s.contains(',') => s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(|x| json!(x)).collect(),
            x => vec![x],
        };
        return Json::Array(list.into_iter().map(|x| fit_value(x, ty, logical, false)).filter(|x| !x.is_null()).collect());
    }
    match (ty, logical) {
        (_, Some(Logical::Timestamp)) => time(&v).map_or(Json::Null, |us| Json::String(chloedb::timestamp::format(us))),
        (_, Some(Logical::Bool)) => convert(v, "bool").unwrap_or(Json::Null),
        (FieldType::Int, None) => match v {
            Json::Number(ref n) if n.is_i64() || n.is_u64() => v,
            x => number(&x).map_or(Json::Null, |f| json!(f.round() as i64)),
        },
        (FieldType::Float, None) => number(&v).map_or(Json::Null, |f| json!(f)),
        (FieldType::Text { .. } | FieldType::Keyword, _) => match v {
            Json::String(_) => v,
            x => Json::String(text(&x)),
        },
        (FieldType::Json, _) => match v {
            Json::Object(_) => v,
            Json::String(s) => match serde_json::from_str::<Json>(&s) {
                Ok(o @ Json::Object(_)) => o,
                _ => json!({ "value": s }),
            },
            x => json!({ "value": x }),
        },
        _ => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_find_keys_elements_and_every_element() {
        let r = json!({"a": {"b": [{"c": 1}, {"c": 2}], "x y": 3}, "k": "v"});
        assert_eq!(path(&r, "$.a.b[1].c"), json!(2));
        assert_eq!(path(&r, "$.a.b[*].c"), json!([1, 2]));
        assert_eq!(path(&r, "$.a[\"x y\"]"), json!(3));
        assert_eq!(path(&r, "k"), json!("v"));
        assert_eq!(path(&r, "$"), r);
        assert_eq!(path(&r, "$.nope.deeper"), Json::Null);
        assert_eq!(records(&json!({"alerts": [1, 2]}), Some("$.alerts")), vec![json!(1), json!(2)]);
        assert_eq!(records(&json!({"one": 1}), None), vec![json!({"one": 1})]);
    }

    #[test]
    fn a_records_path_that_matches_nothing_says_what_the_body_has() {
        let body = json!({"data": {"items": [1]}, "next": null});
        assert_eq!(unmatched(&body, Some("$.items")).as_deref(), Some("the records path $.items matched nothing; the top-level keys are data, next"));
        assert_eq!(unmatched(&body, Some("$.data.items")), None);
        assert_eq!(unmatched(&json!({"items": []}), Some("$.items")), None, "an empty list is no records, not a wrong path");
        assert_eq!(unmatched(&body, None), None);
        assert_eq!(unmatched(&body, Some("$")), None);
        assert_eq!(unmatched(&Json::Null, Some("$.items")), None);
        assert!(unmatched(&json!([1, 2]), Some("$.items")).unwrap().contains("a list of 2"));
        assert!(unmatched(&json!({}), Some("$.alerts")).unwrap().contains("an empty object"));
    }

    #[test]
    fn specs_make_values() {
        let r = json!({"key": "NET-7", "fields": {"status": {"name": "In Progress"}, "labels": ["a", "b"]}, "state": "-2", "when": "2026-11-13 03:00:00"});
        assert_eq!(eval(&r, &json!("{key}:{fields.status.name}"), 0).unwrap(), json!("NET-7:In Progress"));
        assert_eq!(eval(&r, &json!("$.fields.labels"), 0).unwrap(), json!(["a", "b"]));
        assert_eq!(eval(&r, &json!("plain"), 0).unwrap(), json!("plain"));
        assert_eq!(eval(&r, &json!({"path": "$.state", "values": {"-2": "scheduled", "*": "other"}}), 0).unwrap(), json!("scheduled"));
        assert_eq!(eval(&r, &json!({"path": "$.fields.status.name", "as": "lower"}), 0).unwrap(), json!("in progress"));
        assert_eq!(eval(&r, &json!({"path": "$.when", "as": "time"}), 0).unwrap(), json!("2026-11-13T03:00:00Z"));
        assert_eq!(eval(&r, &json!({"path": "$.missing", "default": "x"}), 0).unwrap(), json!("x"));
        assert_eq!(eval(&r, &json!({"const": "a,b", "split": ","}), 0).unwrap(), json!(["a", "b"]));
        assert_eq!(eval(&r, &json!({"as": "hash"}), 0).unwrap().as_str().unwrap().len(), 16);
        assert!(eval(&r, &json!({"path": "$.key", "as": "nope"}), 0).is_err());
    }

    #[test]
    fn times_are_read_in_every_form_connectors_meet() {
        let t = civil(2026, 11, 13, 3, 0, 0).unwrap();
        for s in ["2026-11-13T03:00:00Z", "2026-11-13 03:00:00", "2026-11-13T03:00:00.000+0000", "2026-11-13T04:00:00+01:00", "20261113T030000Z", "Fri, 13 Nov 2026 03:00:00 +0000", "13 Nov 2026 04:00 +0100", "2026-11-13 03:00:00 UTC"] {
            assert_eq!(time_text(s), Some(t), "{s}");
        }
        assert_eq!(time(&json!(1_794_538_800)), Some(1_794_538_800_000_000));
        assert_eq!(time(&json!(1_794_538_800_000u64)), Some(1_794_538_800_000_000));
        assert_eq!(time(&json!("1794538800")), Some(1_794_538_800_000_000));
        assert_eq!(time_text("not a time"), None);
        assert_eq!(chloedb::timestamp::format(civil(1970, 1, 1, 0, 0, 0).unwrap()), "1970-01-01T00:00:00Z");
    }
}
