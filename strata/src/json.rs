//! JSON fields ([`crate::schema::FieldType::Json`]): a JSON object per row
//! whose keys the schema does not name, for attributes that few rows hold
//! and that nobody declared (a product's `voltage`, an event's
//! properties).
//!
//! A row stores its object as compact JSON text, as a SPARSE or EDGES field
//! stores its value, so the WAL, the replicas' records and the schema see
//! one string whatever keys arrive. A segment indexes the object's leaves
//! as typed terms in a multi-valued keyword column of the field
//! (`kw.<field>.dict`, `.codes`, `.post`, `.multi`; see
//! [`crate::forward::keyword`]), so a row pays for the values it holds and
//! nothing for the keys it lacks.
//!
//! **Paths.** A nested object's keys join with dots: `{"dims": {"w": 4}}`
//! holds path `dims.w`. A dot or a backslash inside a key is escaped with a
//! backslash (`{"a.b": 1}` is path `a\.b`). An array holds its elements at
//! its own path, as a multi-valued field does (`{"tags": ["a", "b"]}`
//! holds `a` and `b` at `tags`; `{"items": [{"sku": "x"}, {"sku": "y"}]}`
//! holds `x` and `y` at `items.sku`). JSON `null` is no value.
//!
//! **Terms.** Each leaf is the term `path \0 type value`: type `s` and the
//! UTF-8 text for a string, `n` and [`number_key`] for a number (integers
//! and doubles are one type, as in JSON), `b` and `0` or `1` for a
//! boolean. Each path a row holds a value or an object at adds the term
//! `path \0`, which lists the rows that have it (an empty array holds no
//! value, so the path IS NULL there, as an empty list is in a multi
//! field). Byte order then puts each path's terms in one run, each type in
//! a run within it, and numbers in numeric order, so `=` and `IN` read a
//! term each, `<` and `BETWEEN` a run of number terms, `LIKE 'p%'` a run of
//! string terms and `IS NOT NULL` the path's own term.
//!
//! **Types.** A value's type is part of its term, so `"price": 9.99` and
//! `"price": "n/a"` live side by side and no row is refused for a type. A
//! comparison sees the values of its literal's type only, as if the path
//! were a column of that type: `attrs.price > 5` reads numbers, and a row
//! whose price is `"n/a"` is NULL to it, so `NOT (attrs.price > 5)` passes
//! over that row too. Numbers compare as doubles in the index (an integer
//! above 2^53 loses its last digits there; the stored text keeps them).
//!
//! **Limits** per row, past which a write is refused with the limit named:
//! [`MAX_DEPTH`] levels of nesting, paths of [`MAX_PATH`] bytes, and
//! [`MAX_LEAVES`] values and objects. A string longer than [`MAX_EXACT`] bytes is
//! stored and returned, and `IS NOT NULL` sees it, but comparisons do not:
//! it has no term of its value.

use serde_json::Value as Json;

use crate::error::{Error, Result};
use crate::value::Value;

/// Levels of objects and arrays a value may nest.
pub const MAX_DEPTH: usize = 16;
/// Bytes of a path, escapes included.
pub const MAX_PATH: usize = 256;
/// Values (strings, numbers, booleans) and objects a row's object may hold
/// below it.
pub const MAX_LEAVES: usize = 1000;
/// A string longer than this many bytes is not indexed by its value.
pub const MAX_EXACT: usize = 256;

/// Ends a term's path.
pub const SEP: char = '\0';

/// A type of value at a path, as a term spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Str,
    Number,
    Bool,
}

impl Kind {
    fn tag(self) -> char {
        match self {
            Kind::Str => 's',
            Kind::Number => 'n',
            Kind::Bool => 'b',
        }
    }

    fn of_tag(c: u8) -> Option<Kind> {
        Some(match c {
            b's' => Kind::Str,
            b'n' => Kind::Number,
            b'b' => Kind::Bool,
            _ => return None,
        })
    }

    /// The type's name, as messages and the paths listing give it.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Str => "string",
            Kind::Number => "number",
            Kind::Bool => "bool",
        }
    }

    /// The type of a value a query compares with (`None` for others).
    pub fn of(v: &Value) -> Option<Kind> {
        Some(match v {
            Value::Str(_) => Kind::Str,
            Value::Int(_) | Value::Float(_) => Kind::Number,
            Value::Bool(_) => Kind::Bool,
            _ => return None,
        })
    }

    /// The type a name gives (`'string'`, `'number'`, `'bool'`), as
    /// `json_extract`'s third argument names it.
    pub fn parse(s: &str) -> Option<Kind> {
        [Kind::Str, Kind::Number, Kind::Bool].into_iter().find(|k| k.name().eq_ignore_ascii_case(s) || (*k == Kind::Bool && s.eq_ignore_ascii_case("boolean")))
    }
}

fn bad(m: impl Into<String>) -> Error {
    Error::Schema(m.into())
}

// ------------------------------------------------------------------ paths

/// A key as a path spells it: dots and backslashes escaped.
pub fn escape(key: &str) -> String {
    if !key.contains(['.', '\\']) {
        return key.to_string();
    }
    let mut out = String::with_capacity(key.len() + 2);
    for c in key.chars() {
        if c == '.' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The keys of a path, its escapes undone: `dims.w` is `["dims", "w"]`.
pub fn keys(path: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.last_mut().expect("a key").extend(chars.next()),
            '.' => out.push(String::new()),
            c => out.last_mut().expect("a key").push(c),
        }
    }
    out
}

/// A path as a query names it, checked and in the form terms spell it:
/// keys joined by dots (`dims.w`, or `$.dims.w` as JSONPath writes it), a
/// dot or backslash within a key escaped by a backslash.
pub fn parse_path(text: &str) -> Result<String> {
    let p = text.strip_prefix("$.").unwrap_or(text);
    let bad = |m: &str| Error::Query(format!("JSON path {text:?}: {m}"));
    if p.is_empty() {
        return Err(bad("empty: name a key (attrs.color, json_extract(attrs, 'dims.w'))"));
    }
    let keys = keys(p);
    if keys.iter().any(String::is_empty) {
        return Err(bad("a key is empty"));
    }
    if keys.iter().any(|k| k.contains(SEP)) {
        return Err(bad("a key holds the character \\0"));
    }
    if p.ends_with('\\') && !p.ends_with("\\\\") {
        return Err(bad("ends in a lone backslash"));
    }
    let path = path_of(&keys);
    if path.len() > MAX_PATH {
        return Err(bad(&format!("longer than {MAX_PATH} bytes")));
    }
    Ok(path)
}

/// The path of `keys`, each escaped ([`escape`]).
pub fn path_of<S: AsRef<str>>(keys: &[S]) -> String {
    keys.iter().map(|k| escape(k.as_ref())).collect::<Vec<_>>().join(".")
}

// ------------------------------------------------------------------ terms

/// A number as 16 hex digits whose byte order is numeric order: the bits
/// of the double, all flipped for a negative number and the sign bit for
/// another (`-0` is `0`).
pub fn number_key(x: f64) -> String {
    let x = if x == 0.0 { 0.0 } else { x };
    let b = x.to_bits();
    let k = if b >> 63 == 1 { !b } else { b | 1 << 63 };
    format!("{k:016x}")
}

/// The number of a [`number_key`].
pub fn number_of_key(s: &str) -> Option<f64> {
    let k = u64::from_str_radix(s, 16).ok().filter(|_| s.len() == 16)?;
    let b = if k >> 63 == 1 { k & !(1 << 63) } else { !k };
    Some(f64::from_bits(b))
}

/// The term every row holding `path` has.
pub fn has_term(path: &str) -> String {
    format!("{path}{SEP}")
}

/// The term of a value of type `kind` at `path`, its text already spelled
/// (a number's [`number_key`], a boolean's `0` or `1`).
fn spelled(path: &str, kind: Kind, text: &str) -> String {
    let mut t = String::with_capacity(path.len() + 2 + text.len());
    t.push_str(path);
    t.push(SEP);
    t.push(kind.tag());
    t.push_str(text);
    t
}

/// The term a query's value matches at `path`: a string's, a number's
/// (integer or double) or a boolean's; `None` for another value, or for a
/// string longer than [`MAX_EXACT`], which is not indexed by its value.
pub fn term(path: &str, v: &Value) -> Option<String> {
    Some(match v {
        Value::Str(s) if s.len() <= MAX_EXACT => spelled(path, Kind::Str, s),
        Value::Int(x) => spelled(path, Kind::Number, &number_key(*x as f64)),
        Value::Float(x) if x.is_finite() => spelled(path, Kind::Number, &number_key(*x)),
        Value::Bool(b) => spelled(path, Kind::Bool, if *b { "1" } else { "0" }),
        _ => return None,
    })
}

/// The terms of values of type `kind` at `path`: `[lo, hi)` in byte order.
pub fn kind_run(path: &str, kind: Kind) -> (String, String) {
    let next = (kind.tag() as u8 + 1) as char;
    (format!("{path}{SEP}{}", kind.tag()), format!("{path}{SEP}{next}"))
}

/// The run of terms `[lo, hi)` of values of `lo`'s and `hi`'s type at
/// `path` between the bounds, each with whether it is included; a missing
/// bound is open. Both bounds are of one type ([`Kind::of`]).
pub fn range_run(path: &str, lo: Option<(&Value, bool)>, hi: Option<(&Value, bool)>) -> Result<(String, String)> {
    let kinds: Vec<Kind> = lo.iter().chain(hi.iter()).map(|(v, _)| Kind::of(v).ok_or_else(|| Error::Query(format!("a JSON path compares with strings, numbers and booleans, not {v:?}")))).collect::<Result<_>>()?;
    let kind = match kinds.as_slice() {
        [] => return Err(Error::Query("a range needs a bound".into())),
        [k] => *k,
        [a, b] if a == b => *a,
        _ => return Err(Error::Query(format!("a range of a JSON path has bounds of one type, not a {} and a {}", kinds[0].name(), kinds[1].name()))),
    };
    let (start, end) = kind_run(path, kind);
    // A bound need not be a term: a long string, an infinity.
    let bound = |v: &Value| match v {
        Value::Str(s) => Ok(spelled(path, Kind::Str, s)),
        Value::Float(x) if x.is_nan() => Err(Error::Query("a range of a JSON path has no NaN bound".into())),
        Value::Float(x) => Ok(spelled(path, Kind::Number, &number_key(*x))),
        v => Ok(term(path, v).expect("an integer or a boolean")),
    };
    // Terms are compared as bytes; `t\0` is the first term after `t`.
    let lo = match lo {
        None => start,
        Some((v, true)) => bound(v)?,
        Some((v, false)) => format!("{}{SEP}", bound(v)?),
    };
    let hi = match hi {
        None => end,
        Some((v, true)) => format!("{}{SEP}", bound(v)?),
        Some((v, false)) => bound(v)?,
    };
    Ok((lo, hi))
}

/// A term split into its path and value (`None` for the value of the
/// term [`has_term`] gives).
pub fn decode(term: &str) -> Option<(&str, Option<Value>)> {
    let (path, rest) = term.split_once(SEP)?;
    let Some(&tag) = rest.as_bytes().first() else { return Some((path, None)) };
    let text = &rest[1..];
    Some((
        path,
        Some(match Kind::of_tag(tag)? {
            Kind::Str => Value::Str(text.to_string()),
            Kind::Number => Value::Float(number_of_key(text)?),
            Kind::Bool => Value::Bool(text == "1"),
        }),
    ))
}

// ----------------------------------------------------------------- values

/// A row's value of a JSON field as the table stores it: an object, given
/// as JSON text or as JSON, checked against the limits and written
/// compactly. JSON `null` (or SQL NULL) is no value.
pub fn canonical(v: Value, field: &str) -> Result<Value> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Str(s) => {
            let j: Json = serde_json::from_str(&s).map_err(|e| bad(format!("field {field}: not JSON ({e}): {}", short(&s))))?;
            canonical_json(&j, field)
        }
        v => Err(bad(format!("field {field} holds a JSON object, not {v:?}"))),
    }
}

/// [`canonical`] of parsed JSON.
pub fn canonical_json(j: &Json, field: &str) -> Result<Value> {
    match j {
        Json::Null => Ok(Value::Null),
        Json::Object(_) => {
            check(j).map_err(|e| bad(format!("field {field}: {e}")))?;
            Ok(Value::Str(j.to_string()))
        }
        j => Err(bad(format!("field {field} holds a JSON object, not {}", short(&j.to_string())))),
    }
}

fn short(s: &str) -> String {
    match s.char_indices().nth(60) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_string(),
    }
}

/// Checks a value against the limits: an object, nested at most
/// [`MAX_DEPTH`] levels, with paths of at most [`MAX_PATH`] bytes, at most
/// [`MAX_LEAVES`] values and objects, and no empty key or key holding `\0`.
pub fn check(j: &Json) -> std::result::Result<(), String> {
    if !j.is_object() {
        return Err("a JSON field holds an object".into());
    }
    let mut leaves = 0;
    walk(j, &mut String::new(), 0, &mut |_, _| {
        leaves += 1;
        if leaves > MAX_LEAVES {
            return Err(format!("more than {MAX_LEAVES} values and objects in one row"));
        }
        Ok(())
    })
}

/// Calls `f` with each path `j` holds: once per value, with it, and once
/// per object below the top, with `None`; depth first.
fn walk(j: &Json, path: &mut String, depth: usize, f: &mut dyn FnMut(&str, Option<&Json>) -> std::result::Result<(), String>) -> std::result::Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("nested more than {MAX_DEPTH} levels"));
    }
    match j {
        Json::Null => Ok(()),
        Json::Object(m) => {
            if !path.is_empty() {
                f(path, None)?;
            }
            for (k, v) in m {
                if k.is_empty() {
                    return Err(format!("an empty key{}", if path.is_empty() { String::new() } else { format!(" in {path}") }));
                }
                if k.contains(SEP) {
                    return Err(format!("key {k:?} holds the character \\0"));
                }
                let len = path.len();
                if len > 0 {
                    path.push('.');
                }
                path.push_str(&escape(k));
                if path.len() > MAX_PATH {
                    return Err(format!("path {}... is longer than {MAX_PATH} bytes", &path[..path.floor_char_boundary(40)]));
                }
                walk(v, path, depth + 1, f)?;
                path.truncate(len);
            }
            Ok(())
        }
        // An empty array holds no values, like an empty list in a multi
        // field: the path IS NULL there.
        Json::Array(xs) => xs.iter().try_for_each(|x| walk(x, path, depth + 1, f)),
        leaf => f(path, Some(leaf)),
    }
}

/// The terms a row's stored object is indexed by, sorted and distinct;
/// none for NULL.
pub fn terms(v: &Value) -> Result<Vec<String>> {
    let s = match v {
        Value::Null => return Ok(Vec::new()),
        Value::Str(s) => s,
        v => return Err(bad(format!("a JSON field holds JSON text, not {v:?}"))),
    };
    let j: Json = serde_json::from_str(s).map_err(|e| bad(format!("a stored JSON value does not parse: {e}")))?;
    let mut out = Vec::new();
    let mut leaves = 0;
    walk(&j, &mut String::new(), 0, &mut |path, leaf| {
        leaves += 1;
        if leaves > MAX_LEAVES {
            return Err(format!("more than {MAX_LEAVES} values and objects in one row"));
        }
        out.push(has_term(path));
        out.extend(match leaf {
            None => None,
            Some(Json::String(s)) if s.len() > MAX_EXACT => None,
            Some(Json::String(s)) => Some(spelled(path, Kind::Str, s)),
            Some(Json::Number(n)) => n.as_f64().map(|x| spelled(path, Kind::Number, &number_key(x))),
            Some(Json::Bool(b)) => Some(spelled(path, Kind::Bool, if *b { "1" } else { "0" })),
            Some(_) => None,
        });
        Ok(())
    })
    .map_err(bad)?;
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The text a JSON field with text ([`crate::schema::Field::text`])
/// indexes for a row: its object's strings, long ones too, in the order of
/// their paths (keys sorted), one a line; `None` for a row with none.
/// Numbers, booleans and keys are not text: a query reads them by path.
pub fn text_of(v: &Value) -> Result<Option<String>> {
    let s = match v {
        Value::Null => return Ok(None),
        Value::Str(s) => s,
        v => return Err(bad(format!("a JSON field holds JSON text, not {v:?}"))),
    };
    let j: Json = serde_json::from_str(s).map_err(|e| bad(format!("a stored JSON value does not parse: {e}")))?;
    let mut out = String::new();
    walk(&j, &mut String::new(), 0, &mut |_, leaf| {
        if let Some(Json::String(s)) = leaf {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(s);
        }
        Ok(())
    })
    .map_err(bad)?;
    Ok((!out.is_empty()).then_some(out))
}

/// The value at `path` of a row's stored object, as a query returns it:
/// a string, an integer (a JSON number that is one) or a double, a
/// boolean; an object as its JSON text; an array of plain values as a list
/// of them, and one that holds objects or arrays as its JSON text. A path
/// through an array gathers each element's value, as a list when there
/// are several. NULL where the object has no value there. With `kind`,
/// only values of that type ([`Kind`]), and strings no longer than
/// [`MAX_EXACT`]: what a comparison with a value of that type reads, as the
/// index does.
pub fn extract(v: &Value, path: &str, kind: Option<Kind>) -> Result<Value> {
    let s = match v {
        Value::Null => return Ok(Value::Null),
        Value::Str(s) => s,
        v => return Err(Error::Query(format!("json_extract reads a JSON field, not {v:?}"))),
    };
    let j: Json = serde_json::from_str(s).map_err(|e| Error::Query(format!("json_extract: not JSON ({e})")))?;
    let keys = keys(path);
    let mut found: Vec<&Json> = Vec::new();
    gather(&j, &keys, &mut found);
    let mut out: Vec<Value> = Vec::new();
    let one = |x: &Json| -> Option<Value> {
        let v = plain(x)?;
        match kind {
            None => Some(v),
            // A comparison sees no string it could not see in the index.
            Some(Kind::Str) if matches!(&v, Value::Str(s) if s.len() > MAX_EXACT) => None,
            Some(k) if Kind::of(&v) == Some(k) => Some(v),
            Some(_) => None,
        }
    };
    for x in found {
        match x {
            Json::Null => {}
            Json::Array(xs) if kind.is_some() || xs.iter().all(|x| !x.is_object() && !x.is_array()) => {
                let mut flat = Vec::new();
                flatten(x, &mut flat);
                out.extend(flat.into_iter().filter_map(one));
            }
            // As the index has it: an array of empty arrays holds nothing.
            Json::Array(_) | Json::Object(_) if kind.is_none() => {
                if present(x) {
                    out.push(Value::Str(x.to_string()))
                }
            }
            Json::Array(_) | Json::Object(_) => {}
            x => out.extend(one(x)),
        }
    }
    Ok(match out.len() {
        0 if kind.is_none() && matches!(j.pointer(&pointer(&keys)), Some(Json::Array(xs)) if xs.is_empty()) => Value::List(Vec::new()),
        0 => Value::Null,
        1 => out.pop().expect("one"),
        _ => Value::List(out),
    })
}

/// Whether `j` holds a value or an object, at any depth of arrays.
fn present(j: &Json) -> bool {
    match j {
        Json::Null => false,
        Json::Array(xs) => xs.iter().any(present),
        _ => true,
    }
}

/// A JSON Pointer to `keys` (for the one case [`extract`] needs it: an
/// empty array where no array was passed through).
fn pointer(keys: &[String]) -> String {
    keys.iter().map(|k| format!("/{}", k.replace('~', "~0").replace('/', "~1"))).collect()
}

/// The values at `keys` below `j`, through arrays.
fn gather<'j>(j: &'j Json, keys: &[String], out: &mut Vec<&'j Json>) {
    let Some((k, rest)) = keys.split_first() else {
        out.push(j);
        return;
    };
    match j {
        Json::Object(m) => {
            if let Some(v) = m.get(k) {
                gather(v, rest, out);
            }
        }
        Json::Array(xs) => xs.iter().for_each(|x| gather(x, keys, out)),
        _ => {}
    }
}

/// The plain values of an array, nested arrays flattened.
fn flatten<'j>(j: &'j Json, out: &mut Vec<&'j Json>) {
    match j {
        Json::Array(xs) => xs.iter().for_each(|x| flatten(x, out)),
        x => out.push(x),
    }
}

/// A plain JSON value as a query's value.
fn plain(x: &Json) -> Option<Value> {
    Some(match x {
        Json::String(s) => Value::Str(s.clone()),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64()?),
        },
        Json::Bool(b) => Value::Bool(*b),
        _ => return None,
    })
}

// ---------------------------------------------------------------- builder

/// Builds a JSON field's keyword column of terms ([`terms`]), and with
/// [`crate::schema::Field::text`] the text index of its strings
/// ([`text_of`]), under the field's key as a TEXT field's
/// (`text.<key>.*`, beside the column's `kw.<key>.*`).
pub(crate) struct JsonBuilder {
    inner: Box<dyn crate::segment::FieldIndexBuilder>,
    text: Option<Box<crate::text::TextFieldBuilder>>,
}

impl JsonBuilder {
    pub fn new(field: &crate::schema::Field, opts: &crate::segment::build::BuildOptions) -> Result<Self> {
        let text = if field.text { Some(Box::new(crate::text::text_builder(field, opts)?)) } else { None };
        Ok(JsonBuilder { inner: crate::forward::field_builder(field, opts)?, text })
    }
}

impl crate::segment::FieldIndexBuilder for JsonBuilder {
    fn add(&mut self, doc: crate::segment::DocId, value: &Value) -> Result<()> {
        if let Some(t) = &mut self.text {
            t.add(doc, &text_of(value)?.map_or(Value::Null, Value::Str))?;
        }
        let terms = terms(value)?;
        self.inner.add(doc, &Value::List(terms.into_iter().map(Value::Str).collect()))
    }

    fn finish(self: Box<Self>, out: &mut crate::segment::SegmentWriter) -> Result<()> {
        if let Some(t) = self.text {
            t.finish(out)?;
        }
        self.inner.finish(out)
    }
}

/// The builder of a JSON field's index.
pub fn field_builder(field: &crate::schema::Field, opts: &crate::segment::build::BuildOptions) -> Result<Box<dyn crate::segment::FieldIndexBuilder>> {
    Ok(Box::new(JsonBuilder::new(field, opts)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(x: &str) -> Value {
        Value::Str(x.into())
    }

    #[test]
    fn paths_escape_dots_and_backslashes() {
        assert_eq!(escape("a.b"), "a\\.b");
        assert_eq!(escape("a\\b"), "a\\\\b");
        assert_eq!(keys("dims.w"), vec!["dims", "w"]);
        assert_eq!(keys("a\\.b.c"), vec!["a.b", "c"]);
        assert_eq!(parse_path("$.dims.w").unwrap(), "dims.w");
        assert_eq!(path_of(&keys("a\\.b.c\\\\d")), "a\\.b.c\\\\d");
        assert!(parse_path("").is_err() && parse_path("a..b").is_err() && parse_path(&"k".repeat(300)).is_err());
    }

    #[test]
    fn number_keys_sort_as_numbers() {
        let xs = [f64::NEG_INFINITY, -1e300, -2.5, -1.0, -1e-300, 0.0, 1e-300, 1.0, 2.0, 2.5, 1e300, f64::INFINITY];
        let keys: Vec<String> = xs.iter().map(|&x| number_key(x)).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
        assert_eq!(number_key(-0.0), number_key(0.0));
        for x in xs {
            assert_eq!(number_of_key(&number_key(x)), Some(x));
        }
    }

    #[test]
    fn terms_of_an_object() {
        let v = s(&json!({"color": "black", "voltage": 5, "certs": ["CE", "UL"], "dims": {"w_mm": 40.5}, "ok": true, "none": null, "e": [], "o": {}, "a.b": 1}).to_string());
        let t = terms(&v).unwrap();
        let has = |p: &str| t.contains(&has_term(p));
        assert!(has("color") && has("certs") && has("dims.w_mm") && has("o") && has("a\\.b"));
        assert!(has("dims") && !has("none") && !has("e"), "{t:?}");
        assert!(t.contains(&term("color", &s("black")).unwrap()));
        assert!(t.contains(&term("certs", &s("UL")).unwrap()));
        assert!(t.contains(&term("voltage", &Value::Float(5.0)).unwrap()));
        assert_eq!(term("voltage", &Value::Int(5)), term("voltage", &Value::Float(5.0)));
        assert!(t.contains(&term("ok", &Value::Bool(true)).unwrap()));
        assert_eq!(decode(&term("dims.w_mm", &Value::Float(40.5)).unwrap()), Some(("dims.w_mm", Some(Value::Float(40.5)))));
        assert_eq!(decode(&has_term("color")), Some(("color", None)));
        let mut sorted = t.clone();
        sorted.sort();
        assert_eq!(t, sorted);
    }

    #[test]
    fn limits_are_named() {
        let deep = (0..20).fold(json!(1), |j, _| json!({ "k": j }));
        assert!(canonical(s(&deep.to_string()), "attrs").unwrap_err().to_string().contains("nested"));
        let wide: serde_json::Map<String, Json> = (0..1001).map(|i| (format!("k{i}"), json!(i))).collect();
        assert!(canonical(s(&Json::Object(wide).to_string()), "attrs").unwrap_err().to_string().contains("1000"));
        assert!(canonical(s("[1, 2]"), "attrs").is_err() && canonical(s("not json"), "attrs").is_err());
        assert!(canonical(s(&json!({"": 1}).to_string()), "attrs").is_err());
        assert_eq!(canonical(s("null"), "attrs").unwrap(), Value::Null);
        assert_eq!(canonical(s("{ \"a\" : 1 }"), "attrs").unwrap(), s("{\"a\":1}"));
        // A long string is kept, and indexed by its path only.
        let long = "x".repeat(MAX_EXACT + 1);
        let v = canonical(s(&json!({ "d": long }).to_string()), "attrs").unwrap();
        assert_eq!(terms(&v).unwrap(), vec![has_term("d")]);
    }

    #[test]
    fn ranges_are_runs_of_one_type() {
        let t = |x: f64| term("p", &Value::Float(x)).unwrap();
        let (lo, hi) = range_run("p", Some((&Value::Int(5), false)), None).unwrap();
        assert!(lo > t(5.0) && lo < t(5.000001) && hi > t(f64::MAX) && hi < has_term("p\u{1}"));
        assert!(term("p", &s("a")).unwrap() >= hi || term("p", &s("a")).unwrap() < lo);
        let (lo, hi) = range_run("p", Some((&Value::Int(1), true)), Some((&Value::Int(2), true))).unwrap();
        assert!(lo <= t(1.0) && t(1.0) < hi && t(2.0) < hi && t(2.0000001) >= hi);
        assert!(range_run("p", Some((&Value::Int(1), true)), Some((&s("z"), true))).is_err());
        let (lo, hi) = range_run("p", None, Some((&s("m"), false))).unwrap();
        assert!(lo <= term("p", &s("")).unwrap() && term("p", &s("l")).unwrap() < hi && term("p", &s("m")).unwrap() >= hi);
    }

    #[test]
    fn extract_values() {
        let v = s(&json!({"color": "black", "n": 5, "x": 2.5, "certs": ["CE", "UL"], "dims": {"w": 4}, "items": [{"sku": "a"}, {"sku": "b", "n": 1}], "mixed": [1, "a"], "e": [], "nested": [[1], [2]], "a.b": true}).to_string());
        let x = |p: &str| extract(&v, &parse_path(p).unwrap(), None).unwrap();
        assert_eq!(x("color"), s("black"));
        assert_eq!(x("n"), Value::Int(5));
        assert_eq!(x("x"), Value::Float(2.5));
        assert_eq!(x("certs"), Value::List(vec![s("CE"), s("UL")]));
        assert_eq!(x("dims"), s("{\"w\":4}"));
        assert_eq!(x("dims.w"), Value::Int(4));
        assert_eq!(x("items.sku"), Value::List(vec![s("a"), s("b")]));
        assert_eq!(x("items.n"), Value::Int(1));
        assert_eq!(x("items"), s("[{\"sku\":\"a\"},{\"n\":1,\"sku\":\"b\"}]"), "keys sorted");
        assert_eq!(x("nested"), s("[[1],[2]]"));
        assert_eq!(x("e"), Value::List(vec![]));
        let deep_empty = s(&json!({"e": [[], [null]]}).to_string());
        assert_eq!(extract(&deep_empty, "e", None).unwrap(), Value::Null, "nothing there, as the index has it");
        assert_eq!(terms(&deep_empty).unwrap(), Vec::<String>::new());
        assert_eq!(x("missing"), Value::Null);
        assert_eq!(x("a\\.b"), Value::Bool(true));
        assert_eq!(extract(&v, "mixed", Some(Kind::Number)).unwrap(), Value::Int(1));
        assert_eq!(extract(&v, "mixed", Some(Kind::Str)).unwrap(), s("a"));
        assert_eq!(extract(&v, "nested", Some(Kind::Number)).unwrap(), Value::List(vec![Value::Int(1), Value::Int(2)]));
        assert_eq!(extract(&v, "color", Some(Kind::Number)).unwrap(), Value::Null);
        assert_eq!(extract(&Value::Null, "color", None).unwrap(), Value::Null);
    }
}
