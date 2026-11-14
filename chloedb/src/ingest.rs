//! Rows from JSON, CSV and files, by a table's schema: what `/v1/write`,
//! `chloedb-server import`, `POST /v1/import` and the Python package read.
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
            (FieldType::Int, Json::Number(n)) => whole_number(field, n).map(Value::Int),
            (FieldType::Int, Json::String(s)) => s.trim().parse().map(Value::Int).map_err(|_| bad()),
            _ => Err(bad()),
        };
        return Ok(Value::List(items.iter().map(one).collect::<Result<_>>()?));
    }
    Ok(match (&field.ty, j) {
        (FieldType::Vector { .. }, Json::Array(xs)) => Value::Vector(xs.iter().map(|x| x.as_f64().map(|v| v as f32).ok_or_else(bad)).collect::<Result<Vec<_>>>()?.into()),
        (FieldType::Int, Json::Number(n)) => Value::Int(whole_number(field, n)?),
        (FieldType::Int, Json::Bool(b)) => Value::Int(*b as i64),
        (FieldType::Float, Json::Number(n)) => Value::Float(n.as_f64().ok_or_else(bad)?),
        (FieldType::Keyword, Json::Number(n)) if key => Value::Str(n.to_string()),
        (FieldType::Text { .. } | FieldType::Keyword | FieldType::Edges, Json::String(s)) => Value::Str(s.clone()),
        (FieldType::Edges, Json::Array(_)) => Value::Str(j.to_string()),
        // An object, or JSON text of one.
        (FieldType::Json, Json::String(s)) => crate::json::canonical(Value::Str(s.clone()), &field.name)?,
        (FieldType::Json, j) => crate::json::canonical_json(j, &field.name)?,
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

/// [`document_at`] of a row whose vectors came apart from the rest as
/// float32 (an app's arrays, read without a number per element): each
/// `(field, values)` of `vectors` is a field [`takes_vector`] says yes to,
/// set as `j`'s values are.
pub fn document_with_vectors(schema: &Schema, j: &Json, vectors: Vec<(usize, Vec<f32>)>, now: i64) -> Result<Document> {
    let mut doc = fields_of(schema, j, now, &mut None)?;
    for (f, xs) in vectors {
        debug_assert!(takes_vector(&schema.fields[f]));
        doc[f] = Value::Vector(xs.into());
    }
    finish(schema, doc)
}

/// Whether `field`'s value from JSON is a [`Value::Vector`] of an array's
/// numbers as they are: a VECTOR field of one vector a row.
pub fn takes_vector(field: &Field) -> bool {
    matches!(field.ty, FieldType::Vector { .. }) && !field.multi && field.logical.is_none() && field.media.is_none()
}

/// [`document_at`], adding the names of fields the schema lacks to
/// `unknown` instead of failing when it is given.
fn document_with(schema: &Schema, j: &Json, now: i64, unknown: &mut Option<&mut BTreeSet<String>>) -> Result<Document> {
    let doc = fields_of(schema, j, now, unknown)?;
    finish(schema, doc)
}

/// The defaults with `j`'s values set: a document before its generated
/// fields.
fn fields_of(schema: &Schema, j: &Json, now: i64, unknown: &mut Option<&mut BTreeSet<String>>) -> Result<Document> {
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
    Ok(doc)
}

/// A JSON number for INT field `field`: an integer, or a float with no
/// fraction (`2.0`, as pandas writes a column of whole numbers with gaps).
fn whole_number(field: &Field, n: &serde_json::Number) -> Result<i64> {
    match n.as_i64() {
        Some(i) => Ok(i),
        None => whole(field, n.as_f64().unwrap_or(f64::NAN)),
    }
}

/// `x` as a whole number for INT field `field`, or why it is not one.
pub(crate) fn whole(field: &Field, x: f64) -> Result<i64> {
    // 2^63, the first float past i64::MAX. -2^63 is refused too: a float
    // that equals it may be a rounded literal past i64::MIN
    // (-9223372036854775809 reads as -2^63).
    const END: f64 = 9_223_372_036_854_775_808.0;
    match x.fract() == 0.0 && x > -END && x < END {
        true => Ok(x as i64),
        false => Err(Error::Schema(format!("{} is an INT column: {x} is not a whole number from {} to {} (a FLOAT column holds fractions)", field.name, i64::MIN, i64::MAX))),
    }
}

/// `doc` with its generated fields, checked for missing values.
fn finish(schema: &Schema, mut doc: Document) -> Result<Document> {
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
        FieldType::Int => Value::Int(match t.parse::<i64>() {
            Ok(n) => n,
            // `2.0`, as pandas writes a column of whole numbers with gaps.
            Err(_) => match t.parse::<f64>() {
                Ok(x) => whole(field, x)?,
                Err(_) => return Err(bad()),
            },
        }),
        FieldType::Float => Value::Float(t.parse().map_err(|_| bad())?),
        FieldType::Vector { .. } => value(schema, f, &json(t)?)?,
        FieldType::Sparse | FieldType::Json => value(schema, f, &json(t)?)?,
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
    /// What an empty CSV cell means.
    empty: EmptyCell,
    /// Columns or keys the schema has no field for (skipped).
    pub unknown: BTreeSet<String>,
    /// CSV or TSV rows with more or fewer values than the header names
    /// (their missing cells empty, their extra ones left out), unless
    /// [`Rows::strict`] refuses them.
    pub ragged: u64,
    strict: bool,
}

/// What an empty cell of a CSV or TSV file means (all spaces, or nothing,
/// quoted or not: the CSV reader does not tell them apart).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmptyCell {
    /// The column's `DEFAULT` when it has one, else NULL: a `NOT NULL
    /// DEFAULT 0` column takes 0 from a gap in the file.
    #[default]
    Default,
    /// NULL always, whatever the column's default.
    Null,
    /// The empty string in a TEXT or KEYWORD column (of one value), and as
    /// `Default` in the others.
    Text,
}

impl EmptyCell {
    /// `default`, `null` or `text` (`empty` too).
    pub fn parse(s: &str) -> Option<EmptyCell> {
        match s.to_ascii_lowercase().as_str() {
            "default" => Some(EmptyCell::Default),
            "null" => Some(EmptyCell::Null),
            "text" | "empty" | "string" => Some(EmptyCell::Text),
            _ => None,
        }
    }
}

enum Inner {
    Lines(std::io::Lines<BufReader<Box<dyn Read + Send>>>, u64),
    Array(std::vec::IntoIter<Json>, u64),
    Csv(csv::StringRecordsIntoIter<Box<dyn Read + Send>>, Vec<Option<usize>>, Newlines),
}

/// Where the newlines of a CSV file are, for the line a record starts on:
/// the CSV reader counts a line ended by `\r\n` from its `\n`, which it
/// reads as the start of the next record, so with CRLF line endings every
/// record but the first would be one line early. The newlines the reader
/// has read past and no record has yet started after are kept; the rest
/// are counted.
#[derive(Clone, Default)]
struct Newlines(std::sync::Arc<std::sync::Mutex<NewlineState>>);

#[derive(Default)]
struct NewlineState {
    /// Bytes read so far.
    read: u64,
    /// Newlines before `kept`'s first.
    counted: u64,
    /// Offsets of the newlines not yet counted.
    kept: std::collections::VecDeque<u64>,
}

impl Newlines {
    /// The line (from 1) of the record that starts at byte `start`: one
    /// more than the newlines before its first byte that is not a line's
    /// end.
    fn line(&self, start: u64) -> u64 {
        let mut st = self.0.lock().unwrap_or_else(|e| e.into_inner());
        while st.kept.front().is_some_and(|&b| b <= start) {
            st.kept.pop_front();
            st.counted += 1;
        }
        st.counted + 1
    }
}

/// A reader that notes the newlines in [`Newlines`].
struct Counting<R> {
    inner: R,
    newlines: Newlines,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let mut st = self.newlines.0.lock().unwrap_or_else(|e| e.into_inner());
        let at = st.read;
        for (i, b) in buf[..n].iter().enumerate() {
            if *b == b'\n' {
                st.kept.push_back(at + i as u64);
            }
        }
        st.read += n as u64;
        Ok(n)
    }
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
                let newlines = Newlines::default();
                let reader: Box<dyn Read + Send> = Box::new(Counting { inner: reader, newlines: newlines.clone() });
                let mut r = csv::ReaderBuilder::new().delimiter(if format == Format::Tsv { b'\t' } else { b',' }).flexible(true).from_reader(reader);
                let header = r.headers().map_err(|e| Error::Schema(format!("CSV header: {e}")))?.clone();
                let columns: Vec<Option<usize>> = header
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
                // A column twice would be filled by its last cell, silently.
                for (i, f) in columns.iter().enumerate() {
                    if let Some(j) = f.and_then(|f| columns[..i].iter().position(|g| *g == Some(f))) {
                        return Err(Error::Schema(format!("CSV header: column {} names field {} again, as column {} does", i + 1, schema.fields[f.unwrap_or_default()].name, j + 1)));
                    }
                }
                if !columns.iter().any(Option::is_some) {
                    let sep = if format == Format::Tsv { "tab-separated" } else { "comma-separated" };
                    return Err(Error::Schema(format!("CSV header: it names no field of {} (is the file {sep}, its first line the header?)", schema.table)));
                }
                Inner::Csv(r.into_records(), columns, newlines)
            }
        };
        Ok(Rows { schema, inner, now: crate::timestamp::now(), empty: EmptyCell::Default, unknown, ragged: 0, strict: false })
    }

    /// These rows refusing a CSV or TSV row with more or fewer values than
    /// the header names, by its line, rather than counting it in
    /// [`Rows::ragged`].
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// These rows with `empty` the meaning of an empty CSV cell
    /// ([`EmptyCell::Default`] unless set).
    pub fn empty_cells(mut self, empty: EmptyCell) -> Self {
        self.empty = empty;
        self
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
            Inner::Csv(records, columns, newlines) => {
                let rec = match records.next()? {
                    Ok(r) => r,
                    Err(e) => return Some(Err(Error::Schema(format!("CSV: {e}")))),
                };
                let line = rec.position().map_or(0, |p| newlines.line(p.byte()));
                if rec.len() != columns.len() {
                    if self.strict {
                        return Some(Err(at_row("line", line, Error::Schema(format!("{} values, and the header names {} columns", rec.len(), columns.len())))));
                    }
                    self.ragged += 1;
                }
                let mut doc = match start(schema, now) {
                    Ok(d) => d,
                    Err(e) => return Some(Err(e)),
                };
                for (cellv, f) in rec.iter().zip(columns.iter()) {
                    if let Some(f) = *f {
                        if cellv.trim().is_empty() {
                            let field = &schema.fields[f];
                            match self.empty {
                                EmptyCell::Text if !field.multi && matches!(field.ty, FieldType::Text { .. } | FieldType::Keyword) && field.logical.is_none() && field.media.is_none() => {
                                    doc[f] = Value::Str(String::new())
                                }
                                EmptyCell::Default | EmptyCell::Text if field.default.is_some() => {}
                                _ => doc[f] = Value::Null,
                            }
                            continue;
                        }
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
    /// CSV or TSV rows with more or fewer values than the header names.
    pub ragged: u64,
}

/// Rows a transaction of [`import`] upserts.
pub const IMPORT_BATCH: usize = 10_000;

/// Writes `rows` into `db`. Into an empty table they are bulk-loaded in one
/// commit (keys must then be distinct), and a bad row writes nothing;
/// otherwise, and in a database that numbers its writes (whose bulk loads
/// would reach no replica), they are upserted (inserted, in a table
/// without a key), [`IMPORT_BATCH`] rows a transaction, and a
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
    let keyed = view.schema().primary_key.is_some();
    if view.n_live_docs() == 0 && db.storage().seq_no().is_none() {
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
                // A table without a key has nothing to replace a row by.
                if keyed {
                    tt.upsert(d)?;
                } else {
                    tt.insert(d)?;
                }
            }
            t.commit()?;
            out.rows += n;
        }
    }
    out.unknown = rows.unknown.iter().cloned().collect();
    out.ragged = rows.ragged;
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

/// A `CREATE TABLE IF NOT EXISTS` statement for rows of a file that no
/// table holds yet, from the names in its header (or the keys of its
/// objects) and the values of its first thousand rows: a column of whole
/// numbers is INT, of numbers FLOAT, of `true`/`false` BOOL, of ISO dates
/// TIMESTAMP, of objects or arrays JSON, of text of two and a half words or
/// more on average (or any value longer than 64 bytes) TEXT (searched with MATCH), and of other text KEYWORD. The
/// primary key is `key` when given (`""` for none), else a column named
/// `id` whose values are all there and all different.
pub fn create_for(table: &str, data: &[u8], format: Format, key: Option<&str>) -> Result<String> {
    const SAMPLE: usize = 1000;
    // Each column's name and its first values, as JSON.
    let mut columns: Vec<(String, Vec<Json>)> = Vec::new();
    let col = |columns: &mut Vec<(String, Vec<Json>)>, name: &str, v: Json| {
        let i = match columns.iter().position(|(n, _)| n == name) {
            Some(i) => i,
            None => {
                columns.push((name.to_string(), Vec::new()));
                columns.len() - 1
            }
        };
        columns[i].1.push(v);
    };
    let mut rows = 0usize;
    match format {
        Format::Csv | Format::Tsv => {
            let mut r = csv::ReaderBuilder::new().delimiter(if format == Format::Tsv { b'\t' } else { b',' }).flexible(true).from_reader(data);
            let header: Vec<String> = r.headers().map_err(|e| Error::Schema(format!("CSV header: {e}")))?.iter().map(|h| h.trim().trim_start_matches('\u{feff}').to_string()).collect();
            for h in &header {
                if h.is_empty() {
                    return Err(Error::Schema("the header has an empty column name: name every column in the first line".into()));
                }
                col(&mut columns, h, Json::Null);
                columns.last_mut().unwrap().1.clear();
            }
            for rec in r.records().take(SAMPLE) {
                let rec = rec.map_err(|e| Error::Schema(format!("CSV: {e}")))?;
                rows += 1;
                for (h, cell) in header.iter().zip(rec.iter()) {
                    let cell = cell.trim();
                    col(&mut columns, h, if cell.is_empty() { Json::Null } else { Json::String(cell.to_string()) });
                }
            }
        }
        Format::Jsonl | Format::Json => {
            let objs: Vec<Json> = if format == Format::Json {
                match serde_json::from_slice::<Json>(data).map_err(|e| Error::Schema(format!("JSON: {e}")))? {
                    Json::Array(xs) => xs.into_iter().take(SAMPLE).collect(),
                    o => vec![o],
                }
            } else {
                data.split(|b| *b == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace)).take(SAMPLE).map(|l| serde_json::from_slice(l).map_err(|e| Error::Schema(format!("JSON line: {e}")))).collect::<Result<_>>()?
            };
            for o in objs {
                let Json::Object(o) = o else { return Err(Error::Schema("rows are JSON objects".into())) };
                rows += 1;
                for (k, v) in o {
                    col(&mut columns, &k, v);
                }
            }
        }
    }
    if columns.is_empty() {
        return Err(Error::Schema("the rows name no columns".into()));
    }
    let present = |vs: &[Json]| vs.iter().filter(|v| !v.is_null()).cloned().collect::<Vec<_>>();
    let type_of = |vs: &[Json]| -> &'static str {
        let vs = present(vs);
        let text = |v: &Json| v.as_str().map(str::to_string);
        if vs.is_empty() {
            return "KEYWORD";
        }
        if vs.iter().all(|v| v.is_boolean() || text(v).is_some_and(|s| s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false"))) {
            return "BOOL";
        }
        if vs.iter().all(|v| v.is_i64() || v.is_u64() || text(v).is_some_and(|s| s.parse::<i64>().is_ok())) {
            return "INT";
        }
        if vs.iter().all(|v| v.is_number() || text(v).is_some_and(|s| s.parse::<f64>().is_ok_and(f64::is_finite))) {
            return "FLOAT";
        }
        if vs.iter().all(|v| text(v).is_some_and(|s| crate::timestamp::parse(&s).is_some())) {
            return "TIMESTAMP";
        }
        if vs.iter().any(|v| v.is_object() || v.is_array()) {
            return "JSON";
        }
        let words: usize = vs.iter().map(|v| text(v).unwrap_or_default().split_whitespace().count()).sum();
        let long = vs.iter().any(|v| text(v).is_some_and(|s| s.len() > 64));
        if long || words as f64 / vs.len() as f64 >= 2.5 { "TEXT" } else { "KEYWORD" }
    };
    let key = match key {
        Some("") => None,
        Some(k) => Some(columns.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(n, _)| n.clone()).ok_or_else(|| Error::Schema(format!("the key {k:?} is not a column of the rows")))?),
        None => columns.iter().find(|(n, vs)| {
            let p = present(vs);
            let mut seen = std::collections::HashSet::new();
            n.eq_ignore_ascii_case("id") && p.len() == rows && p.iter().all(|v| seen.insert(v.to_string()))
        }).map(|(n, _)| n.clone()),
    };
    let name = |n: &str| if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !n.starts_with(|c: char| c.is_ascii_digit()) { n.to_string() } else { crate::query::sql::quoted(n) };
    let fields: Vec<String> = columns
        .iter()
        .map(|(n, vs)| {
            let mut ty = type_of(vs);
            if key.as_deref() == Some(n.as_str()) {
                ty = if ty == "INT" { "INT" } else { "KEYWORD" };
                return format!("{} {ty} PRIMARY KEY", name(n));
            }
            format!("{} {ty}", name(n))
        })
        .collect();
    Ok(format!("CREATE TABLE IF NOT EXISTS {} ({})", name(table), fields.join(", ")))
}

#[cfg(test)]
mod create_for_tests {
    use super::*;

    #[test]
    fn a_table_for_a_csv_file() {
        let csv = "id,title,category,price,stock,in_stock,added\ng-1,Trail Running Shoes,shoes,89.5,12,true,2026-01-02\ng-2,Rain Jacket,jackets,120,3,false,2026-02-03\ng-3,Wool Socks,socks,9.99,40,true,2026-03-04\n";
        let sql = create_for("gear", csv.as_bytes(), Format::Csv, None).unwrap();
        assert_eq!(sql, "CREATE TABLE IF NOT EXISTS gear (id KEYWORD PRIMARY KEY, title KEYWORD, category KEYWORD, price FLOAT, stock INT, in_stock BOOL, added TIMESTAMP)");
        let sql = create_for("gear", b"n,note\n1,a long note of words\n2,another note with words\n", Format::Csv, Some("n")).unwrap();
        assert_eq!(sql, "CREATE TABLE IF NOT EXISTS gear (n INT PRIMARY KEY, note TEXT)");
        let sql = create_for("t", br#"[{"id": 1, "tags": ["a"], "my col": "x"}, {"id": 1}]"#, Format::Json, None).unwrap();
        assert_eq!(sql, r#"CREATE TABLE IF NOT EXISTS t (id INT, "my col" KEYWORD, tags JSON)"#);
        assert!(create_for("t", b"a,,b\n1,2,3\n", Format::Csv, None).is_err());
    }
}
