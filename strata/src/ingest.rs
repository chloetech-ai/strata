//! Rows from JSON, CSV and files, by a table's schema: what `/v1/write`,
//! `strata-server import`, `POST /v1/import` and the Python package read.
//!
//! A row is a JSON object of field values or a CSV record under a header
//! naming the fields. A field the row leaves out (a key it lacks, a column
//! the header lacks) takes its default ([`crate::schema::Field::default`])
//! or is null; a null value or an empty cell is null. Formats: JSON Lines (one
//! object a line), a JSON array of objects, CSV and TSV. In CSV an empty
//! cell is null, a vector or a list is a JSON array (a list may also be
//! values separated by `|`), and a SPARSE value a JSON object. A BOOL is
//! `true`, `false`, 1 or 0 (in CSV also `t` and `f`), a TIMESTAMP ISO
//! 8601 text ([`crate::timestamp`]). A MEDIA value is an object of
//! [`crate::media::KEYS`] or a URI (in CSV the object's JSON text or the
//! URI), which [`Db::fill_in`] resolves before it is written
//! ([`crate::media`]).

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use serde_json::Value as Json;

use crate::db::Db;
use crate::error::{Error, Result};
use crate::schema::{Field, FieldType, Logical, Schema};
use crate::value::{Document, Value};

/// The value of field `f` from JSON. An array is a vector for a VECTOR
/// field and a list for a multi-valued one; EDGES take a JSON array (or
/// its text) and SPARSE an object of term weights (or its text).
pub fn value(schema: &Schema, f: usize, j: &Json) -> Result<Value> {
    field_value(&schema.fields[f], schema.primary_key == Some(f), j)
}

/// [`value`] of `field`, which is its table's primary key when `key`.
pub fn field_value(field: &Field, key: bool, j: &Json) -> Result<Value> {
    let bad = || Error::Schema(format!("bad value for {}: {}", field.name, short(j)));
    if j.is_null() {
        return Ok(Value::Null);
    }
    if let Some(l) = field.logical {
        // A BOOL is true, false, 1, 0 or such text; a TIMESTAMP is text.
        let v = match (l, j) {
            (Logical::Bool, Json::Bool(b)) => Value::Bool(*b),
            (Logical::Bool, Json::Number(n)) => Value::Int(n.as_i64().ok_or_else(bad)?),
            (_, Json::String(s)) => Value::Str(s.clone()),
            _ => return Err(Error::Schema(format!("bad value for {}: {} ({})", field.name, short(j), l.expects()))),
        };
        return field.store(v);
    }
    if field.media.is_some() {
        return crate::media::input_text(j).map(Value::Str).map_err(|e| Error::Schema(format!("bad value for {}: {e}", field.name)));
    }
    if field.is_multi_vector() {
        // An array of vectors (arrays of numbers), or JSON text of one.
        let v = match j {
            Json::String(s) => Value::Str(s.clone()),
            Json::Array(rows) => Value::List(
                rows.iter()
                    .map(|r| match r {
                        Json::Array(xs) => xs.iter().map(|x| x.as_f64().map(|v| v as f32)).collect::<Option<Vec<f32>>>().map(|v| Value::Vector(v.into())),
                        Json::Number(n) => n.as_f64().map(Value::Float),
                        _ => None,
                    })
                    .collect::<Option<Vec<Value>>>()
                    .ok_or_else(bad)?,
            ),
            _ => return Err(bad()),
        };
        return crate::vector::multi::normalize(field, v);
    }
    if field.multi {
        let items = match j {
            Json::Array(xs) => xs.clone(),
            x => vec![x.clone()],
        };
        let one = |x: &Json| match (&field.ty, x) {
            (FieldType::Keyword, Json::String(s)) => Ok(Value::Str(s.clone())),
            (FieldType::Keyword, Json::Number(n)) => Ok(Value::Str(n.to_string())),
            (FieldType::Int, Json::Number(n)) => n.as_i64().map(Value::Int).ok_or_else(bad),
            (FieldType::Int, Json::String(s)) => s.trim().parse().map(Value::Int).map_err(|_| bad()),
            _ => Err(bad()),
        };
        return Ok(Value::List(items.iter().map(one).collect::<Result<_>>()?));
    }
    Ok(match (&field.ty, j) {
        (FieldType::Vector { .. }, Json::Array(xs)) => Value::Vector(xs.iter().map(|x| x.as_f64().map(|v| v as f32).ok_or_else(bad)).collect::<Result<Vec<_>>>()?.into()),
        (FieldType::Int, Json::Number(n)) => Value::Int(n.as_i64().ok_or_else(bad)?),
        (FieldType::Int, Json::Bool(b)) => Value::Int(*b as i64),
        (FieldType::Float, Json::Number(n)) => Value::Float(n.as_f64().ok_or_else(bad)?),
        (FieldType::Keyword, Json::Number(n)) if key => Value::Str(n.to_string()),
        (FieldType::Text { .. } | FieldType::Keyword | FieldType::Edges, Json::String(s)) => Value::Str(s.clone()),
        (FieldType::Edges, Json::Array(_)) => Value::Str(j.to_string()),
        (FieldType::Sparse, Json::Object(_) | Json::String(_)) => {
            let v = match j {
                Json::String(s) => Value::Str(s.clone()),
                o => Value::Str(o.to_string()),
            };
            crate::text::sparse::parse(&v).map_err(|e| Error::Schema(format!("bad value for {}: {e}", field.name)))?;
            v
        }
        _ => return Err(bad()),
    })
}

/// A document from a JSON object of field values (missing fields take
/// their defaults or are null, generated fields are filled in). A field
/// the schema lacks is an error.
pub fn document(schema: &Schema, j: &Json) -> Result<Document> {
    document_at(schema, j, crate::timestamp::now())
}

/// [`document`] with `now` as the time a TIMESTAMP defaulting to `"now"`
/// takes, so that the rows of one request or file share it.
pub fn document_at(schema: &Schema, j: &Json, now: i64) -> Result<Document> {
    document_with(schema, j, now, &mut None)
}

/// [`document_at`], adding the names of fields the schema lacks to
/// `unknown` instead of failing when it is given.
fn document_with(schema: &Schema, j: &Json, now: i64, unknown: &mut Option<&mut BTreeSet<String>>) -> Result<Document> {
    let Json::Object(m) = j else { return Err(Error::Schema(format!("a row is a JSON object, not {}", short(j)))) };
    let mut doc = start(schema, now)?;
    for (k, v) in m {
        match schema.field_index(k) {
            Some(f) => doc[f] = value(schema, f, v)?,
            None => match unknown {
                Some(u) => {
                    u.insert(k.clone());
                }
                None => return Err(Error::Schema(format!("no field {k}"))),
            },
        }
    }
    schema.fill_generated(&mut doc);
    required(schema, &doc)?;
    Ok(doc)
}

/// Fails if `doc` has no value for a `NOT NULL` field, so that a file's
/// row is refused with its line rather than when the rows load. Embedded
/// vectors are computed later, so they are left to the write.
fn required(schema: &Schema, doc: &Document) -> Result<()> {
    match schema.fields.iter().zip(doc).find(|(f, v)| f.not_null && f.embed.is_none() && matches!(v, Value::Null)) {
        Some((f, _)) => Err(Error::Schema(format!("{} is NOT NULL: give it a value", f.name))),
        None => Ok(()),
    }
}

/// A row before its values: the fields' defaults ([`Schema::defaults`]),
/// which a field the row leaves out keeps.
fn start(schema: &Schema, now: i64) -> Result<Document> {
    if schema.has_defaults() {
        schema.defaults(now)
    } else {
        Ok(vec![Value::Null; schema.fields.len()])
    }
}

/// The value of field `f` from a CSV cell.
pub fn cell(schema: &Schema, f: usize, s: &str) -> Result<Value> {
    let field = &schema.fields[f];
    let t = s.trim();
    if t.is_empty() {
        return Ok(Value::Null);
    }
    let bad = || Error::Schema(format!("bad value for {}: {:?}", field.name, short_str(s)));
    let json = |s: &str| serde_json::from_str::<Json>(s).map_err(|_| bad());
    if field.multi {
        let j = if t.starts_with('[') { json(t)? } else { Json::Array(t.split('|').map(|x| Json::String(x.trim().to_string())).collect()) };
        return value(schema, f, &j);
    }
    if field.logical.is_some() {
        return field.store(Value::Str(t.to_string()));
    }
    if field.media.is_some() {
        return value(schema, f, &Json::String(t.to_string()));
    }
    Ok(match &field.ty {
        FieldType::Int => Value::Int(t.parse().map_err(|_| bad())?),
        FieldType::Float => Value::Float(t.parse().map_err(|_| bad())?),
        FieldType::Vector { .. } => value(schema, f, &json(t)?)?,
        FieldType::Sparse => value(schema, f, &json(t)?)?,
        FieldType::Edges if t.starts_with('[') => value(schema, f, &json(t)?)?,
        FieldType::Edges => Value::Str(Json::Array(t.split('|').map(|x| Json::String(x.trim().to_string())).collect()).to_string()),
        FieldType::Text { .. } | FieldType::Keyword => Value::Str(s.to_string()),
    })
}

/// A file format rows are read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// JSON Lines: one JSON object a line.
    Jsonl,
    /// A JSON array of objects (or a single object).
    Json,
    Csv,
    Tsv,
}

impl Format {
    /// `jsonl` (or `ndjson`), `json`, `csv` or `tsv`.
    pub fn parse(s: &str) -> Option<Format> {
        match s.to_ascii_lowercase().trim_start_matches('.') {
            "jsonl" | "ndjson" | "json-lines" | "jsonlines" => Some(Format::Jsonl),
            "json" => Some(Format::Json),
            "csv" => Some(Format::Csv),
            "tsv" | "tab" => Some(Format::Tsv),
            _ => None,
        }
    }

    /// The format a file's extension names.
    pub fn of_path(path: &Path) -> Option<Format> {
        path.extension().and_then(|e| e.to_str()).and_then(Format::parse)
    }
}

/// Reads rows: every row as a document, or the first error, naming the
/// line or record. Fields the schema lacks are skipped and listed in
/// `unknown`. A TIMESTAMP defaulting to `"now"` takes the time the rows
/// were opened, the same in every row.
pub struct Rows<'a> {
    schema: &'a Schema,
    inner: Inner,
    now: i64,
    /// Columns or keys the schema has no field for (skipped).
    pub unknown: BTreeSet<String>,
}

enum Inner {
    Lines(std::io::Lines<BufReader<Box<dyn Read + Send>>>, u64),
    Array(std::vec::IntoIter<Json>, u64),
    Csv(csv::StringRecordsIntoIter<Box<dyn Read + Send>>, Vec<Option<usize>>),
}

impl<'a> Rows<'a> {
    pub fn new(schema: &'a Schema, reader: impl Read + Send + 'static, format: Format) -> Result<Rows<'a>> {
        let reader: Box<dyn Read + Send> = Box::new(reader);
        let mut unknown = BTreeSet::new();
        let inner = match format {
            Format::Jsonl => Inner::Lines(BufReader::with_capacity(1 << 20, reader).lines(), 0),
            Format::Json => {
                let j: Json = serde_json::from_reader(BufReader::with_capacity(1 << 20, reader)).map_err(|e| Error::Schema(format!("JSON: {e}")))?;
                let rows = match j {
                    Json::Array(xs) => xs,
                    o @ Json::Object(_) => vec![o],
                    other => return Err(Error::Schema(format!("a JSON file of rows is an array of objects, not {}", short(&other)))),
                };
                Inner::Array(rows.into_iter(), 0)
            }
            Format::Csv | Format::Tsv => {
                let mut r = csv::ReaderBuilder::new().delimiter(if format == Format::Tsv { b'\t' } else { b',' }).flexible(true).from_reader(reader);
                let header = r.headers().map_err(|e| Error::Schema(format!("CSV header: {e}")))?.clone();
                let columns = header
                    .iter()
                    .map(|h| {
                        let h = h.trim().trim_start_matches('\u{feff}');
                        let f = schema.field_index(h);
                        if f.is_none() {
                            unknown.insert(h.to_string());
                        }
                        f
                    })
                    .collect();
                Inner::Csv(r.into_records(), columns)
            }
        };
        Ok(Rows { schema, inner, now: crate::timestamp::now(), unknown })
    }

    /// Opens `path`, in `format` or the one its extension names.
    pub fn open(schema: &'a Schema, path: &Path, format: Option<Format>) -> Result<Rows<'a>> {
        let format = format.or_else(|| Format::of_path(path)).ok_or_else(|| Error::Schema(format!("{}: name the format (jsonl, json, csv or tsv)", path.display())))?;
        Rows::new(schema, std::fs::File::open(path)?, format)
    }
}

impl Iterator for Rows<'_> {
    type Item = Result<Document>;

    fn next(&mut self) -> Option<Result<Document>> {
        let (schema, now) = (self.schema, self.now);
        let unknown = &mut Some(&mut self.unknown);
        match &mut self.inner {
            Inner::Lines(lines, n) => loop {
                *n += 1;
                let line = match lines.next()? {
                    Ok(l) => l,
                    Err(e) => return Some(Err(e.into())),
                };
                if line.trim().is_empty() {
                    continue;
                }
                let at = *n;
                return Some(serde_json::from_str::<Json>(&line).map_err(|e| Error::Schema(e.to_string())).and_then(|j| document_with(schema, &j, now, unknown)).map_err(|e| at_row("line", at, e)));
            },
            Inner::Array(rows, n) => {
                *n += 1;
                let at = *n;
                rows.next().map(|j| document_with(schema, &j, now, unknown).map_err(|e| at_row("row", at, e)))
            }
            Inner::Csv(records, columns) => {
                let rec = match records.next()? {
                    Ok(r) => r,
                    Err(e) => return Some(Err(Error::Schema(format!("CSV: {e}")))),
                };
                let line = rec.position().map_or(0, |p| p.line());
                let mut doc = match start(schema, now) {
                    Ok(d) => d,
                    Err(e) => return Some(Err(e)),
                };
                for (cellv, f) in rec.iter().zip(columns.iter()) {
                    if let Some(f) = *f {
                        match cell(schema, f, cellv) {
                            Ok(v) => doc[f] = v,
                            Err(e) => return Some(Err(at_row("line", line, e))),
                        }
                    }
                }
                schema.fill_generated(&mut doc);
                Some(required(schema, &doc).map(|()| doc).map_err(|e| at_row("line", line, e)))
            }
        }
    }
}

/// What [`import`] did.
#[derive(Clone, Debug, Default)]
pub struct Imported {
    /// Rows written.
    pub rows: u64,
    /// Whether they were bulk-loaded (the table was empty) rather than upserted.
    pub bulk: bool,
    /// Columns or keys the schema has no field for (skipped).
    pub unknown: Vec<String>,
}

/// Rows a transaction of [`import`] upserts.
pub const IMPORT_BATCH: usize = 10_000;

/// Writes `rows` into `db`. Into an empty table they are bulk-loaded in one
/// commit (keys must then be distinct), and a bad row writes nothing;
/// otherwise they are upserted, [`IMPORT_BATCH`] rows a transaction, and a
/// bad row stops the import after the transactions before it. Embedded
/// vectors the rows leave out are computed when the database has the
/// field's model ([`Db::set_embedder`]).
pub fn import(db: &Db, rows: Rows<'_>) -> Result<Imported> {
    let first = db.schema().table.clone();
    import_into(db, &first, rows)
}

/// [`import`] into the table named `table`, whose schema `rows` reads.
pub fn import_into(db: &Db, table: &str, mut rows: Rows<'_>) -> Result<Imported> {
    let view = db.snapshot().table(table).ok_or_else(|| Error::Query(format!("no such table: {table}")))?;
    let mut out = Imported::default();
    if view.n_live_docs() == 0 {
        let mut n = 0u64;
        db.try_load_into(table, rows.by_ref().inspect(|r| n += r.is_ok() as u64))?;
        (out.rows, out.bulk) = (n, true);
    } else {
        loop {
            let mut batch = Vec::with_capacity(IMPORT_BATCH);
            for r in rows.by_ref().take(IMPORT_BATCH) {
                batch.push(r?);
            }
            if batch.is_empty() {
                break;
            }
            db.fill_in(table, &mut batch)?;
            let n = batch.len() as u64;
            let mut t = db.storage().begin()?;
            let mut tt = t.table(table)?;
            for d in batch {
                tt.upsert(d)?;
            }
            t.commit()?;
            out.rows += n;
        }
    }
    out.unknown = rows.unknown.iter().cloned().collect();
    Ok(out)
}

/// [`import`] of a file, in `format` or the one its extension names.
pub fn import_file(db: &Db, path: &Path, format: Option<Format>) -> Result<Imported> {
    let first = db.schema().table.clone();
    import_file_into(db, &first, path, format)
}

/// [`import_file`] into the table named `table`.
pub fn import_file_into(db: &Db, table: &str, path: &Path, format: Option<Format>) -> Result<Imported> {
    let schema = db.storage().table(table).ok_or_else(|| Error::Query(format!("no such table: {table}")))?;
    import_into(db, &schema.table, Rows::open(&schema, path, format)?)
}

fn at_row(what: &str, n: u64, e: Error) -> Error {
    match e {
        Error::Schema(m) => Error::Schema(format!("{what} {n}: {m}")),
        other => other,
    }
}

fn short(j: &Json) -> String {
    short_str(&j.to_string())
}

fn short_str(s: &str) -> String {
    if s.chars().count() <= 80 {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(80).collect::<String>())
    }
}
