//! Writing Strata tables to a new SQLite file ([`super`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::types::Value as Sql;
use rusqlite::{params_from_iter, Connection};
use serde_json::{json, Value as Json};

use super::{distinct, literal, quote, META_TABLE};
use crate::db::Db;
use crate::error::{Error, Result};
use crate::schema::{Analyzer, Field, FieldType, Logical, Schema, VectorIndexKind};
use crate::storage::Snapshot;
use crate::value::Value;

/// What an export writes besides the tables and their rows.
#[derive(Clone, Debug)]
pub struct ExportOptions {
    /// The tables to export (any case); every table when empty.
    pub tables: Vec<String>,
    /// An FTS5 index over each table's TEXT fields, `<table>_fts`, kept in
    /// step with the table by triggers.
    pub fts: bool,
    /// A B-tree index on each single-valued, indexed KEYWORD, INT, FLOAT,
    /// BOOL and TIMESTAMP field other than the key.
    pub indexes: bool,
    /// The Strata schemas, in [`META_TABLE`].
    pub meta: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions { tables: Vec::new(), fts: true, indexes: true, meta: true }
    }
}

/// What an export wrote.
#[derive(Clone, Debug, Default)]
pub struct Exported {
    pub tables: Vec<TableExport>,
}

/// What an export wrote of one table.
#[derive(Clone, Debug)]
pub struct TableExport {
    pub table: String,
    pub rows: u64,
    /// The B-tree indexes created.
    pub indexes: Vec<String>,
    /// The FTS5 table over its TEXT fields.
    pub fts: Option<String>,
    /// What SQLite does not hold of it.
    pub notes: Vec<String>,
}

impl Exported {
    /// `{"tables": [{table, rows, indexes, fts, notes}]}`.
    pub fn to_json(&self) -> Json {
        let tables: Vec<Json> = self.tables.iter().map(|t| json!({"table": t.table, "rows": t.rows, "indexes": t.indexes, "fts": t.fts, "notes": t.notes})).collect();
        json!({"tables": tables})
    }
}

/// Writes `db`'s tables (or those `opts` names) to a new SQLite file at
/// `path`, the rows of one snapshot. The file is written under another
/// name and renamed into place when complete; an existing `path` is an
/// error.
pub fn export(db: &Db, path: &Path, opts: &ExportOptions) -> Result<Exported> {
    if path.exists() {
        return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{} exists", path.display()))));
    }
    let snap = db.snapshot();
    let all: Vec<&Schema> = snap.schemas().collect();
    let mut tables: Vec<&Schema> = Vec::new();
    if opts.tables.is_empty() {
        tables = all.clone();
    } else {
        for name in &opts.tables {
            let s = all.iter().find(|s| s.table.eq_ignore_ascii_case(name)).ok_or_else(|| Error::Schema(format!("no table {name}")))?;
            if !tables.iter().any(|t| t.table == s.table) {
                tables.push(s);
            }
        }
    }
    let name = path.file_name().ok_or_else(|| Error::Schema(format!("{}: not a file name", path.display())))?;
    let tmp: PathBuf = path.with_file_name(format!(".{}.strata-export-{}", name.to_string_lossy(), std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let written = write(db, &snap, &tables, &tmp, opts);
    let renamed = written.and_then(|out| std::fs::rename(&tmp, path).map(|()| out).map_err(Error::from));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed
}

fn write(db: &Db, snap: &Snapshot, tables: &[&Schema], path: &Path, opts: &ExportOptions) -> Result<Exported> {
    let conn = Connection::open(path)?;
    // A new file, renamed into place only when complete: no journal needed.
    // Strata does not enforce references, so a row may name a key that is
    // gone, or one of its own table written after it.
    conn.execute_batch("PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF; PRAGMA foreign_keys = OFF; BEGIN;")?;
    // Tables, indexes and triggers share SQLite's names.
    let mut used: HashSet<String> = tables.iter().map(|s| s.table.to_ascii_lowercase()).collect();
    used.insert(META_TABLE.to_string());
    let mut out = Exported::default();
    for s in tables {
        let mut t = TableExport { table: s.table.clone(), rows: 0, indexes: Vec::new(), fts: None, notes: notes(s) };
        conn.execute_batch(&create_table(s, tables, &mut t.notes))?;
        let rows = snap.table(&s.table).ok_or_else(|| Error::Schema(format!("no table {}", s.table)))?;
        t.rows = insert_rows(&conn, db, &rows, s)?;
        if opts.indexes {
            for (i, f) in s.fields.iter().enumerate() {
                let indexable = matches!(f.ty, FieldType::Keyword | FieldType::Int | FieldType::Float) && f.indexed && !f.multi;
                if s.primary_key == Some(i) || !indexable {
                    continue;
                }
                let index = distinct(format!("{}_{}_idx", s.table, f.name), &mut used);
                // A MEDIA field is indexed by its file's hash, as in Strata.
                let on = match f.media {
                    Some(_) => format!("json_extract({}, '$.sha256')", quote(&f.name)),
                    None => quote(&f.name),
                };
                conn.execute_batch(&format!("CREATE INDEX {} ON {} ({on});", quote(&index), quote(&s.table)))?;
                t.indexes.push(index);
            }
        }
        if opts.fts {
            t.fts = fts(&conn, s, &mut used, &mut t.notes)?;
        }
        out.tables.push(t);
    }
    if opts.meta {
        conn.execute_batch(&format!("CREATE TABLE {} (name TEXT PRIMARY KEY NOT NULL, position INTEGER NOT NULL, schema TEXT NOT NULL);", quote(META_TABLE)))?;
        let mut stmt = conn.prepare(&format!("INSERT INTO {} (name, position, schema) VALUES (?1, ?2, ?3)", quote(META_TABLE)))?;
        for (i, s) in tables.iter().enumerate() {
            stmt.execute(rusqlite::params![s.table, i as i64, s.to_short_json().to_string()])?;
        }
    }
    conn.execute_batch("COMMIT;")?;
    Ok(out)
}

/// What SQLite does not hold of `s`'s fields.
fn notes(s: &Schema) -> Vec<String> {
    let mut out = Vec::new();
    for f in &s.fields {
        match &f.ty {
            FieldType::Vector { dim, index } => {
                let index = match index {
                    VectorIndexKind::Flat => String::new(),
                    VectorIndexKind::Hnsw { .. } => " and its HNSW index".into(),
                    VectorIndexKind::Ivf { .. } => " and its IVF index".into(),
                };
                let model = f.embed.as_ref().map_or(String::new(), |e| format!("; the embedding model ({}) is in the kept schema only", e.model));
                out.push(format!("{}: vectors as F32_BLOB({dim}), without their codes{index}{model}", f.name));
            }
            FieldType::Edges => out.push(format!("{}: edges as JSON arrays of keys, without the graph index", f.name)),
            FieldType::Sparse => out.push(format!("{}: term weights as JSON objects, without their index", f.name)),
            _ if f.media.is_some() => out.push(format!("{}: each file's description as a JSON object (uri, sha256, mime and what else Strata read), indexed by sha256; the files stay where the URIs say", f.name)),
            _ if f.multi => out.push(format!("{}: values as JSON arrays", f.name)),
            _ => {}
        }
        if !f.generated_from.is_empty() {
            out.push(format!("{}: the text generated from {}, as a plain column", f.name, f.generated_from.join(", ")));
        } else if !f.stored && !matches!(f.ty, FieldType::Vector { .. }) {
            out.push(format!("{}: not stored, so it holds what the indexes give back (often NULL)", f.name));
        }
    }
    out
}

/// The SQLite type of a field's column.
fn declared(f: &Field) -> String {
    if f.multi || f.media.is_some() {
        return "JSON".into();
    }
    match (f.logical, &f.ty) {
        (Some(Logical::Bool), _) => "BOOLEAN".into(),
        (Some(Logical::Timestamp), _) => "TIMESTAMP".into(),
        (_, FieldType::Text { .. } | FieldType::Keyword) => "TEXT".into(),
        (_, FieldType::Int) => "INTEGER".into(),
        (_, FieldType::Float) => "REAL".into(),
        (_, FieldType::Vector { dim, .. }) => format!("F32_BLOB({dim})"),
        (_, FieldType::Edges | FieldType::Sparse) => "JSON".into(),
    }
}

fn create_table(s: &Schema, tables: &[&Schema], notes: &mut Vec<String>) -> String {
    let mut cols = Vec::new();
    for (i, f) in s.fields.iter().enumerate() {
        let mut c = format!("{} {}", quote(&f.name), declared(f));
        if s.primary_key == Some(i) {
            // An INTEGER PRIMARY KEY is the rowid, never NULL; a TEXT one
            // could be, but for NOT NULL.
            c += if f.ty == FieldType::Int { " PRIMARY KEY" } else { " PRIMARY KEY NOT NULL" };
        } else if f.not_null {
            c += " NOT NULL";
        }
        if let Some(d) = default_sql(f) {
            c += &format!(" DEFAULT {d}");
        }
        // A list or edges in one column cannot be a foreign key.
        if let Some(r) = f.references.as_deref().filter(|_| f.ty != FieldType::Edges && !f.multi) {
            match tables.iter().find(|t| t.table.eq_ignore_ascii_case(r)).and_then(|t| t.primary_key.map(|k| (t, k))) {
                Some((t, k)) => c += &format!(" REFERENCES {} ({})", quote(&t.table), quote(&t.fields[k].name)),
                None => notes.push(format!("{}: references {r}, which is not exported", f.name)),
            }
        }
        cols.push(c);
    }
    format!("CREATE TABLE {} (\n  {}\n);", quote(&s.table), cols.join(",\n  "))
}

/// A field's `DEFAULT` in SQL: `CURRENT_TIMESTAMP` for `"now"`, else its
/// value as a literal.
fn default_sql(f: &Field) -> Option<String> {
    let d = f.default.as_ref()?;
    if f.logical == Some(Logical::Timestamp) && d.as_str().is_some_and(|s| s.eq_ignore_ascii_case("now")) {
        return Some("CURRENT_TIMESTAMP".into());
    }
    match sql_value(f, &f.default_value(0).ok()?) {
        Sql::Integer(i) => Some(i.to_string()),
        Sql::Real(x) => Some(format!("{x:?}")),
        Sql::Text(s) => Some(literal(&s)),
        Sql::Null | Sql::Blob(_) => None,
    }
}

/// A stored value of field `f` as SQLite holds it.
fn sql_value(f: &Field, v: &Value) -> Sql {
    match v {
        Value::Null => Sql::Null,
        Value::Int(t) | Value::Timestamp(t) if f.logical == Some(Logical::Timestamp) => Sql::Text(sqlite_time(*t)),
        Value::Int(x) => Sql::Integer(*x),
        Value::Timestamp(t) => Sql::Text(sqlite_time(*t)),
        Value::Bool(b) => Sql::Integer(*b as i64),
        Value::Float(x) => Sql::Real(*x),
        Value::Str(s) => Sql::Text(s.clone()),
        Value::Vector(v) => Sql::Blob(v.iter().flat_map(|x| x.to_le_bytes()).collect()),
        Value::List(xs) => {
            let items: Vec<Json> = xs
                .iter()
                .map(|x| match x {
                    Value::Int(i) => json!(i),
                    Value::Str(s) => json!(s),
                    Value::Float(x) => json!(x),
                    Value::Bool(b) => json!(b),
                    other => json!(format!("{other:?}")),
                })
                .collect();
            Sql::Text(Json::Array(items).to_string())
        }
    }
}

/// A timestamp as SQLite's date functions write one, in UTC:
/// `2026-10-01 12:30:05`, with a fraction when it has one.
fn sqlite_time(micros: i64) -> String {
    let s = crate::timestamp::format(micros);
    s.strip_suffix('Z').unwrap_or(&s).replacen('T', " ", 1)
}

fn insert_rows(conn: &Connection, db: &Db, snap: &Snapshot, s: &Schema) -> Result<u64> {
    let cols: Vec<String> = s.fields.iter().map(|f| quote(&f.name)).collect();
    let marks = vec!["?"; cols.len()].join(", ");
    let mut stmt = conn.prepare(&format!("INSERT INTO {} ({}) VALUES ({marks})", quote(&s.table), cols.join(", ")))?;
    let mut n = 0;
    for d in db.documents(snap) {
        let d = d?;
        stmt.execute(params_from_iter(s.fields.iter().zip(&d).map(|(f, v)| sql_value(f, v))))?;
        n += 1;
    }
    Ok(n)
}

/// An FTS5 index over `s`'s TEXT fields, with triggers that keep it in
/// step with the table, if it has any.
fn fts(conn: &Connection, s: &Schema, used: &mut HashSet<String>, notes: &mut Vec<String>) -> Result<Option<String>> {
    let text: Vec<&Field> = s.fields.iter().filter(|f| matches!(f.ty, FieldType::Text { .. })).collect();
    if text.is_empty() {
        return Ok(None);
    }
    // The rowid: an INT key is it.
    let rowid = match s.primary_key {
        Some(k) if s.fields[k].ty == FieldType::Int => quote(&s.fields[k].name),
        _ => {
            if let Some(f) = s.fields.iter().find(|f| ["rowid", "oid", "_rowid_"].contains(&f.name.to_ascii_lowercase().as_str())) {
                notes.push(format!("no FTS5 index: field {} hides the table's rowid", f.name));
                return Ok(None);
            }
            "rowid".to_string()
        }
    };
    let english = text.iter().any(|f| f.text_analyzer() == Analyzer::English);
    if english && text.iter().any(|f| f.text_analyzer() != Analyzer::English) {
        notes.push("FTS5 has one tokenizer a table: porter (stemming) for every TEXT field".into());
    }
    let tokenize = if english { "porter unicode61 remove_diacritics 2" } else { "unicode61 remove_diacritics 2" };
    let name = distinct(format!("{}_fts", s.table), used);
    let (table, q) = (quote(&s.table), quote(&name));
    let cols: Vec<String> = text.iter().map(|f| quote(&f.name)).collect();
    let cols = cols.join(", ");
    let of = |row: &str| text.iter().map(|f| format!("{row}.{}", quote(&f.name))).collect::<Vec<_>>().join(", ");
    let (new, old) = (of("new"), of("old"));
    let content_rowid = if rowid == "rowid" { "rowid".to_string() } else { s.fields[s.primary_key.unwrap_or(0)].name.clone() };
    let trigger = |suffix: &str, used: &mut HashSet<String>| quote(&distinct(format!("{name}_{suffix}"), used));
    let (ai, ad, au) = (trigger("ai", used), trigger("ad", used), trigger("au", used));
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE {q} USING fts5({cols}, content={}, content_rowid={}, tokenize={});
         INSERT INTO {q} ({q}) VALUES ('rebuild');
         CREATE TRIGGER {ai} AFTER INSERT ON {table} BEGIN
           INSERT INTO {q} (rowid, {cols}) VALUES (new.{rowid}, {new});
         END;
         CREATE TRIGGER {ad} AFTER DELETE ON {table} BEGIN
           INSERT INTO {q} ({q}, rowid, {cols}) VALUES ('delete', old.{rowid}, {old});
         END;
         CREATE TRIGGER {au} AFTER UPDATE ON {table} BEGIN
           INSERT INTO {q} ({q}, rowid, {cols}) VALUES ('delete', old.{rowid}, {old});
           INSERT INTO {q} (rowid, {cols}) VALUES (new.{rowid}, {new});
         END;",
        literal(&s.table),
        literal(&content_rowid),
        literal(tokenize),
    ))?;
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_as_sqlite_writes_them() {
        let t = crate::timestamp::parse("2026-10-01T12:30:05.25Z").unwrap();
        assert_eq!(sqlite_time(t), "2026-10-01 12:30:05.250");
        assert_eq!(sqlite_time(0), "1970-01-01 00:00:00");
        assert_eq!(crate::timestamp::parse(&sqlite_time(t)), Some(t));
    }
}
