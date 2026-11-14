//! Files in and out: xlsx (IronCalc reads and writes Excel's format), CSV,
//! and SQLite databases, each of whose tables becomes a sheet.

use crate::book::Shown;
use std::io::{Cursor, Read};

use ironcalc::base::types::Workbook;
use ironcalc::base::{Model, UserModel};
use serde_json::{json, Value};

use crate::book::{engine_of, Engine, LANG};

/// Cells an import fills at most (a sheet holds the first rows of a table
/// that has more, and says so).
pub const MAX_IMPORT_CELLS: usize = 2_000_000;

/// Bytes an xlsx file may expand to: its parts, decompressed, together. A
/// file of a few megabytes can expand to gigabytes of XML, all of which
/// IronCalc would hold at once.
pub const MAX_XLSX_EXPANDED: u64 = 512 << 20;

/// How long reading a SQLite file may take: listing its tables, or
/// bringing them into sheets. A view of the file's can be a query that
/// never ends.
pub const SQLITE_LIST_SECS: u64 = 60;
pub const SQLITE_IMPORT_SECS: u64 = 180;

/// The kind of a file, by its first bytes, or by its name.
pub fn sniff(bytes: &[u8], name: &str) -> &'static str {
    if bytes.starts_with(b"SQLite format 3\0") {
        return "sqlite";
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return "xlsx";
    }
    let lower = name.to_lowercase();
    if lower.ends_with(".tsv") || lower.ends_with(".tab") {
        return "tsv";
    }
    "csv"
}

/// Refuses an xlsx file whose parts expand past `limit` bytes, counted by
/// decompressing them (the sizes a zip declares can lie).
pub fn check_xlsx_size(bytes: &[u8], limit: u64) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not an xlsx file: {e}"))?;
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut part = zip.by_index(i).map_err(|e| format!("not an xlsx file: {e}"))?;
        let room = limit.saturating_sub(total);
        let n = std::io::copy(&mut (&mut part).take(room + 1), &mut std::io::sink()).map_err(|e| format!("not an xlsx file: {e}"))?;
        total += n;
        if total > limit {
            return Err(format!("the xlsx file expands to more than {} MB; imports take at most that", limit >> 20));
        }
    }
    Ok(())
}

/// A workbook from the bytes of an xlsx file.
pub fn from_xlsx(bytes: &[u8], name: &str) -> Result<Engine, String> {
    check_xlsx_size(bytes, MAX_XLSX_EXPANDED)?;
    let wb: Workbook = ironcalc::import::load_from_xlsx_bytes(bytes, name, "en", "UTC").map_err(|e| format!("not an xlsx file IronCalc reads: {e}"))?;
    let cells: usize = wb.worksheets.iter().map(|w| w.sheet_data.cells().count()).sum();
    if cells > MAX_IMPORT_CELLS {
        return Err(format!("the xlsx file holds {cells} cells; imports take at most {MAX_IMPORT_CELLS}"));
    }
    let mut e = engine_of(wb)?;
    e.set_name(name);
    // Excel keeps each formula's last value in the file, but most other
    // writers (openpyxl, pandas, exporters) keep none: computed here, or the
    // formulas show 0 or #VALUE! until the next edit.
    e.evaluate();
    Ok(e)
}

/// The workbook as an xlsx file.
pub fn to_xlsx(model: &Model) -> Result<Vec<u8>, String> {
    let w = ironcalc::export::save_xlsx_to_writer(model, Cursor::new(Vec::new())).map_err(|e| format!("writing xlsx: {e}"))?;
    Ok(w.into_inner())
}

fn empty_model() -> Model<'static> {
    Model::new_empty("Book", "en", "UTC", LANG).expect("an empty workbook")
}

/// A sheet's name from a file's or a table's: at most 31 characters, none
/// of `[]:*?/\`, and none another sheet has.
pub fn sheet_name(want: &str, taken: &[String]) -> String {
    let clean: String = want.chars().map(|c| if "[]:*?/\\".contains(c) { '_' } else { c }).collect();
    let clean = clean.trim().trim_matches('\'');
    let base: String = if clean.is_empty() { "Sheet".into() } else { clean.chars().take(31).collect() };
    let mut name = base.clone();
    let mut i = 2;
    while taken.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
        let suffix = format!(" ({i})");
        name = format!("{}{suffix}", base.chars().take(31 - suffix.chars().count()).collect::<String>());
        i += 1;
    }
    name
}

/// A value read from a file, typed as it is (not parsed again).
pub enum Val {
    Empty,
    Num(f64),
    Text(String),
    Bool(bool),
}

/// Fills sheet `s` of `m` from `rows`, its first row (the names) bold and
/// frozen. Returns the cells filled.
fn fill(m: &mut Model, s: u32, header: Option<&[String]>, rows: impl Iterator<Item = Vec<Val>>, budget: &mut usize) -> Result<(usize, bool), String> {
    let mut r = 1;
    let mut n = 0;
    if let Some(h) = header {
        for (j, name) in h.iter().enumerate() {
            let c = j as i32 + 1;
            m.update_cell_with_text(s, 1, c, name)?;
            let mut st = m.get_style_for_cell(s, 1, c)?;
            st.font.b = true;
            m.set_cell_style(s, 1, c, &st)?;
            let w = (name.chars().count() as f64 * 8.0 + 24.0).clamp(70.0, 220.0);
            m.set_column_width(s, c, w)?;
        }
        m.set_frozen_rows(s, 1)?;
        r = 2;
    }
    let mut cut = false;
    for row in rows {
        if *budget < row.len() {
            cut = true;
            break;
        }
        for (j, v) in row.iter().enumerate() {
            let c = j as i32 + 1;
            match v {
                Val::Empty => continue,
                Val::Num(x) if x.is_finite() => m.update_cell_with_number(s, r, c, *x)?,
                // a cell holds no infinity or NaN: kept as the text it reads as
                Val::Num(x) => m.update_cell_with_text(s, r, c, if x.is_nan() { "NaN" } else if *x > 0.0 { "inf" } else { "-inf" })?,
                Val::Text(t) if t.is_empty() => continue,
                Val::Text(t) => m.update_cell_with_text(s, r, c, t)?,
                Val::Bool(b) => m.update_cell_with_bool(s, r, c, *b)?,
            }
            n += 1;
            *budget -= 1;
        }
        r += 1;
        if r > 1_048_576 {
            cut = true;
            break;
        }
    }
    Ok((n, cut))
}

/// A value of a CSV field: a number when it reads as one, else text.
fn csv_val(s: &str) -> Val {
    let t = s.trim();
    if t.is_empty() {
        return Val::Empty;
    }
    // Numbers as a spreadsheet would read them, but not codes with leading
    // zeros, nor what a number would lose: "NaN", "inf" and "Infinity"
    // (which Rust's parser takes), or an id of more than 15 digits (a card
    // or account number), which a double cannot hold.
    let leading_zero = t.len() > 1 && t.starts_with('0') && !t.starts_with("0.");
    let numeric = t.bytes().any(|b| b.is_ascii_digit()) && t.bytes().all(|b| b.is_ascii_digit() || b"+-.,eE".contains(&b));
    if numeric && !leading_zero && !long_integer(t) {
        if let Ok(x) = t.replace(',', "").parse::<f64>() {
            if x.is_finite() && (!t.contains(',') || t.split('.').next().is_some_and(|i| i.split(',').skip(1).all(|g| g.len() == 3))) {
                return Val::Num(x);
            }
        }
    }
    match t.to_ascii_lowercase().as_str() {
        "true" => Val::Bool(true),
        "false" => Val::Bool(false),
        _ => Val::Text(s.to_string()),
    }
}

/// A whole number written with more than 15 significant digits, which a
/// double does not hold exactly (Excel too keeps such ids as text only).
fn long_integer(t: &str) -> bool {
    let d = t.trim_start_matches(['+', '-']).replace(',', "");
    d.bytes().all(|b| b.is_ascii_digit()) && d.trim_start_matches('0').len() > 15
}

/// The delimiter of a CSV file: the one of `,;\t|` its first lines split by most evenly.
fn delimiter(text: &str) -> u8 {
    let lines: Vec<&str> = text.lines().take(20).collect();
    let mut best = (b',', 0usize);
    for d in [b',', b'\t', b';', b'|'] {
        let counts: Vec<usize> = lines.iter().map(|l| l.bytes().filter(|b| *b == d).count()).collect();
        let Some(&first) = counts.first() else { continue };
        if first == 0 {
            continue;
        }
        let even = counts.iter().filter(|c| **c == first).count();
        if even * first > best.1 {
            best = (d, even * first);
        }
    }
    best.0
}

/// A workbook of one sheet from a CSV (or tab-separated) file. Its first
/// row is taken as the names when `header` says so (by default, when it
/// holds only text and the next one does not).
pub fn from_csv(bytes: &[u8], name: &str, header: Option<bool>) -> Result<(Engine, Value), String> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_start_matches('\u{feff}');
    let d = if sniff(bytes, name) == "tsv" { b'\t' } else { delimiter(text) };
    let mut rd = csv::ReaderBuilder::new().delimiter(d).has_headers(false).flexible(true).from_reader(text.as_bytes());
    let mut rows: Vec<Vec<String>> = Vec::new();
    for rec in rd.records() {
        let rec = rec.map_err(|e| format!("CSV: {e}"))?;
        rows.push(rec.iter().map(str::to_string).collect());
    }
    let header = header.unwrap_or_else(|| {
        rows.len() > 1 && rows[0].iter().all(|v| matches!(csv_val(v), Val::Text(_))) && rows[1].iter().any(|v| !matches!(csv_val(v), Val::Text(_) | Val::Empty))
    });
    let mut m = empty_model();
    let stem = name.rsplit_once('.').map(|(a, _)| a).unwrap_or(name);
    m.rename_sheet_by_index(0, &sheet_name(stem, &[]))?;
    let mut budget = MAX_IMPORT_CELLS;
    let (head, body) = if header && !rows.is_empty() { (Some(rows.remove(0)), rows) } else { (None, rows) };
    let n_rows = body.len();
    let (cells, cut) = fill(&mut m, 0, head.as_deref(), body.into_iter().map(|r| r.iter().map(|v| csv_val(v)).collect()), &mut budget)?;
    m.workbook.name = stem.to_string();
    m.evaluate();
    let report = json!({"format": "csv", "delimiter": (d as char).to_string(), "header": header, "rows": n_rows, "cells": cells, "truncated": cut});
    Ok((UserModel::from_model(m), report))
}

/// A sheet as CSV: the values as shown.
pub fn to_csv(model: &Model, sheet: u32) -> Result<String, String> {
    let ws = model.workbook.worksheet(sheet)?;
    let mut w = csv::Writer::from_writer(Vec::new());
    if !ws.sheet_data.is_empty() {
        let d = ws.dimension();
        for r in 1..=d.max_row {
            let rec: Vec<String> = (1..=d.max_column).map(|c| model.shown(sheet, r, c).unwrap_or_default()).collect();
            w.write_record(&rec).map_err(|e| e.to_string())?;
        }
    }
    String::from_utf8(w.into_inner().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// Interrupts what a connection runs once `secs` have passed, until it is
/// dropped.
pub struct Deadline(Option<std::sync::mpsc::Sender<()>>);

impl Deadline {
    pub fn new(conn: &rusqlite::Connection, secs: u64) -> Deadline {
        let handle = conn.get_interrupt_handle();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(std::time::Duration::from_secs(secs)) {
                handle.interrupt();
            }
        });
        Deadline(Some(tx))
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}

/// An error of a read the deadline cut short says so.
fn sqlite_err(e: rusqlite::Error, secs: u64) -> String {
    if e.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) {
        format!("reading the SQLite file took more than {secs} s and was stopped (a view of the file's may be a query that does not end)")
    } else {
        e.to_string()
    }
}

/// The tables of a SQLite database: name, columns and rows; and its views
/// (`"kind": "view"`), whose rows are not counted, since counting runs
/// the view's query.
pub fn sqlite_tables(conn: &rusqlite::Connection) -> Result<Vec<Value>, String> {
    let _deadline = Deadline::new(conn, SQLITE_LIST_SECS);
    let mut st = conn.prepare("SELECT name, type FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY type, name").map_err(|e| e.to_string())?;
    let names: Vec<(String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(|e| e.to_string())?.filter_map(Result::ok).collect();
    let mut out = Vec::new();
    for (t, kind) in names {
        let q = crate::strata::ident(&t);
        let cols: Vec<Value> = match conn.prepare(&format!("PRAGMA table_info({q})")) {
            Ok(mut s) => s.query_map([], |r| Ok(json!({"name": r.get::<_, String>(1)?, "type": r.get::<_, String>(2).unwrap_or_default()}))).map_err(|e| e.to_string())?.filter_map(Result::ok).collect(),
            Err(e) => return Err(e.to_string()),
        };
        if kind == "view" {
            out.push(json!({"name": t, "kind": "view", "columns": cols, "rows": null}));
            continue;
        }
        let rows: i64 = conn.query_row(&format!("SELECT count(*) FROM {q}"), [], |r| r.get(0)).unwrap_or(0);
        out.push(json!({"name": t, "kind": "table", "columns": cols, "rows": rows}));
    }
    Ok(out)
}

/// Opens SQLite bytes (written to a temporary file, read only).
pub fn open_sqlite(bytes: &[u8]) -> Result<(rusqlite::Connection, tempfile::NamedTempFile), String> {
    let f = tempfile::Builder::new().prefix("strata-sheets-").suffix(".sqlite").tempfile().map_err(|e| e.to_string())?;
    std::fs::write(f.path(), bytes).map_err(|e| e.to_string())?;
    let conn = rusqlite::Connection::open_with_flags(f.path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| format!("not a SQLite database: {e}"))?;
    // The file's schema is not ours: its views and triggers may call only
    // functions without side effects.
    conn.execute_batch("PRAGMA trusted_schema = OFF").map_err(|e| format!("not a SQLite database: {e}"))?;
    Ok((conn, f))
}

/// A workbook of a SQLite database: a sheet for each table (those of
/// `only`, if given, views among them only when named), its columns' names
/// in the first row.
pub fn from_sqlite(bytes: &[u8], name: &str, only: Option<&[String]>) -> Result<(Engine, Value), String> {
    let (conn, _file) = open_sqlite(bytes)?;
    let tables = sqlite_tables(&conn)?;
    let _deadline = Deadline::new(&conn, SQLITE_IMPORT_SECS);
    let mut m = empty_model();
    let mut budget = MAX_IMPORT_CELLS;
    let mut taken: Vec<String> = Vec::new();
    let mut report = Vec::new();
    for t in &tables {
        let tname = t["name"].as_str().unwrap_or_default().to_string();
        let named = only.is_some_and(|o| o.iter().any(|x| x == &tname));
        if (only.is_some() && !named) || (t["kind"] == "view" && !named) {
            continue;
        }
        let sname = sheet_name(&tname, &taken);
        let s = if taken.is_empty() {
            m.rename_sheet_by_index(0, &sname)?;
            0
        } else {
            m.add_sheet(&sname)?;
            taken.len() as u32
        };
        taken.push(sname.clone());
        let q = crate::strata::ident(&tname);
        let mut st = conn.prepare(&format!("SELECT * FROM {q}")).map_err(|e| sqlite_err(e, SQLITE_IMPORT_SECS))?;
        let head: Vec<String> = st.column_names().into_iter().map(str::to_string).collect();
        let width = head.len();
        let mut rows = st.query([]).map_err(|e| sqlite_err(e, SQLITE_IMPORT_SECS))?;
        let mut vals: Vec<Vec<Val>> = Vec::new();
        let mut more = false;
        let room = budget / width.max(1);
        while let Some(r) = rows.next().map_err(|e| sqlite_err(e, SQLITE_IMPORT_SECS))? {
            if vals.len() >= room {
                more = true;
                break;
            }
            let mut line = Vec::with_capacity(width);
            for i in 0..width {
                use rusqlite::types::ValueRef;
                line.push(match r.get_ref(i).map_err(|e| e.to_string())? {
                    ValueRef::Null => Val::Empty,
                    ValueRef::Integer(x) => Val::Num(x as f64),
                    ValueRef::Real(x) => Val::Num(x),
                    ValueRef::Text(t) => Val::Text(String::from_utf8_lossy(t).into_owned()),
                    ValueRef::Blob(b) => Val::Text(format!("({} bytes)", b.len())),
                });
            }
            vals.push(line);
        }
        let n = vals.len();
        let (cells, cut) = fill(&mut m, s, Some(&head), vals.into_iter(), &mut budget)?;
        report.push(json!({"table": tname, "sheet": sname, "rows": n, "of": t["rows"], "cells": cells, "truncated": cut || more}));
    }
    if taken.is_empty() {
        return Err(if tables.is_empty() {
            "the database has no tables".into()
        } else if only.is_none() {
            "the database has only views: name the ones to bring in (a view runs its query as it is read)".into()
        } else {
            "none of those tables is in the database".into()
        });
    }
    let stem = name.rsplit_once('.').map(|(a, _)| a).unwrap_or(name);
    m.workbook.name = stem.to_string();
    m.evaluate();
    Ok((UserModel::from_model(m), json!({"format": "sqlite", "tables": report})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_values_and_delimiters() {
        assert!(matches!(csv_val("12.5"), Val::Num(x) if x == 12.5));
        assert!(matches!(csv_val("1,234"), Val::Num(x) if x == 1234.0));
        assert!(matches!(csv_val("007"), Val::Text(_)));
        assert!(matches!(csv_val("1,2"), Val::Text(_)));
        assert!(matches!(csv_val("TRUE"), Val::Bool(true)));
        assert_eq!(delimiter("a;b;c\n1;2;3\n"), b';');
        assert_eq!(delimiter("a,b\n1,2\n"), b',');
        let (e, r) = from_csv(b"name,qty,price\npear,3,1.5\n\"fig, dried\",10,0.25\n", "fruit.csv", None).unwrap();
        assert_eq!(r["header"], true);
        assert_eq!(e.shown(0, 3, 1).unwrap(), "fig, dried");
        assert_eq!(e.shown(0, 2, 2).unwrap(), "3");
        assert_eq!(e.get_worksheets_properties()[0].name, "fruit");
        let csv = to_csv(e.get_model(), 0).unwrap();
        assert!(csv.starts_with("name,qty,price\npear,3,1.5\n\"fig, dried\""), "{csv}");
    }

    /// Text that a float would read but lose: "NaN", "inf", "Infinity"
    /// (which `f64::from_str` takes, and which the sheet then dropped) and
    /// ids of more than 15 digits stay as they were written (DAP-7).
    #[test]
    fn csv_keeps_what_a_number_would_lose() {
        for t in ["NaN", "nan", "inf", "-inf", "Infinity", "+infinity", "1e400", "12345678901234567890", "4111111111111111", "-4111111111111111", "0x1F"] {
            assert!(matches!(csv_val(t), Val::Text(ref s) if s == t), "{t} should stay text");
        }
        assert!(matches!(csv_val("123456789012345"), Val::Num(x) if x == 123456789012345.0));
        assert!(matches!(csv_val("1e5"), Val::Num(x) if x == 100000.0));
        assert!(matches!(csv_val("-2.5"), Val::Num(x) if x == -2.5));
        assert!(matches!(csv_val(".5"), Val::Num(x) if x == 0.5));
        let (e, r) = from_csv(b"label,value\nn,7\nnan,NaN\ninf,inf\nInfinity,Infinity\nbig id,12345678901234567890\ncard,4111111111111111\n", "edge.csv", None).unwrap();
        assert_eq!(r["header"], true);
        assert_eq!(r["cells"], 12, "a cell was dropped: {r}");
        for (row, want) in [(2, "7"), (3, "NaN"), (4, "inf"), (5, "Infinity"), (6, "12345678901234567890"), (7, "4111111111111111")] {
            assert_eq!(e.shown(0, row, 2).unwrap(), want, "row {row}");
        }
        let csv = to_csv(e.get_model(), 0).unwrap();
        assert!(csv.contains("card,4111111111111111\n"), "{csv}");
    }

    /// A number a file holds that a cell cannot (an infinite REAL of a
    /// SQLite table) is kept as text, not dropped.
    #[test]
    fn a_number_a_cell_cannot_hold_is_kept_as_text() {
        let mut m = empty_model();
        let mut budget = 100;
        let (n, cut) = fill(&mut m, 0, None, vec![vec![Val::Num(f64::INFINITY), Val::Num(f64::NAN), Val::Num(1.0)]].into_iter(), &mut budget).unwrap();
        assert_eq!((n, cut), (3, false));
        assert_eq!(m.shown(0, 1, 1).unwrap(), "inf");
        assert_eq!(m.shown(0, 1, 2).unwrap(), "NaN");
    }

    /// The xlsx file with its formulas' values taken out, as openpyxl
    /// writes one: `<f>A1*B1</f>` and no `<v>`.
    fn without_values(x: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut zin = zip::ZipArchive::new(Cursor::new(x)).unwrap();
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for i in 0..zin.len() {
            let mut part = zin.by_index(i).unwrap();
            let name = part.name().unwrap().to_string();
            let mut text = Vec::new();
            part.read_to_end(&mut text).unwrap();
            if name.starts_with("xl/worksheets/") {
                let mut s = String::from_utf8(text).unwrap();
                let mut n = 0;
                while let Some(at) = s.find("</f><v>") {
                    let end = at + s[at..].find("</v>").unwrap() + 4;
                    s.replace_range(at + 4..end, "");
                    n += 1;
                }
                assert!(n > 0 || !name.ends_with(".xml"), "{name}: no formula values to take out");
                text = s.into_bytes();
            }
            w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(&text).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// An xlsx file written by a program that leaves no computed values
    /// (openpyxl, pandas and most exporters) shows its formulas computed,
    /// not 0 until the next edit (DAP-6).
    #[test]
    fn xlsx_formulas_are_computed_on_import() {
        let mut m = empty_model();
        m.set_user_input(0, 1, 1, "4".into()).unwrap();
        m.set_user_input(0, 1, 2, "5".into()).unwrap();
        m.set_user_input(0, 1, 3, "=A1*B1".into()).unwrap();
        m.set_user_input(0, 2, 3, "=SUM(A1:C1)".into()).unwrap();
        m.add_sheet("Feuille2").unwrap();
        m.set_user_input(1, 1, 1, "=Sheet1!C1*2".into()).unwrap();
        m.evaluate();
        let x = without_values(&to_xlsx(&m).unwrap());
        let e = from_xlsx(&x, "calc").unwrap();
        assert_eq!(e.shown(0, 1, 3).unwrap(), "20");
        assert_eq!(e.shown(0, 2, 3).unwrap(), "29");
        assert_eq!(e.shown(1, 1, 1).unwrap(), "40");
    }

    #[test]
    fn sheet_names_are_valid_and_distinct() {
        let taken = vec!["Sales".to_string()];
        assert_eq!(sheet_name("sales", &taken), "sales (2)");
        assert_eq!(sheet_name("a/b:c", &[]), "a_b_c");
        assert_eq!(sheet_name(&"x".repeat(40), &[]).chars().count(), 31);
    }

    #[test]
    fn sqlite_tables_become_sheets() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("shop.db");
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE orders (id INTEGER PRIMARY KEY, item TEXT, qty INTEGER, price REAL, note BLOB); INSERT INTO orders VALUES (1, 'pear', 3, 1.5, x'00ff'), (2, '007', NULL, 2.25, NULL); CREATE TABLE people (name TEXT);").unwrap();
        drop(c);
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(sniff(&bytes, "x"), "sqlite");
        let (e, r) = from_sqlite(&bytes, "shop.db", None).unwrap();
        let props = e.get_worksheets_properties();
        assert_eq!(props.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["orders", "people"]);
        assert_eq!(e.shown(0, 1, 2).unwrap(), "item");
        assert_eq!(e.shown(0, 3, 2).unwrap(), "007");
        assert_eq!(e.shown(0, 2, 4).unwrap(), "1.5");
        assert_eq!(e.shown(0, 2, 5).unwrap(), "(2 bytes)");
        assert_eq!(r["tables"][0]["rows"], 2);
        let x = to_xlsx(e.get_model()).unwrap();
        let back = from_xlsx(&x, "shop").unwrap();
        assert_eq!(back.shown(0, 3, 2).unwrap(), "007");
        assert_eq!(back.shown(0, 2, 3).unwrap(), "3");
    }

    /// A SQLite file whose view never ends: `forever`, over a recursive CTE.
    fn endless_view() -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("trap.db");
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1); CREATE VIEW forever AS WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT count(*) AS c FROM n;").unwrap();
        drop(c);
        std::fs::read(&p).unwrap()
    }

    #[test]
    fn a_sqlite_files_views_are_listed_not_run() {
        let bytes = endless_view();
        let (conn, _f) = open_sqlite(&bytes).unwrap();
        let started = std::time::Instant::now();
        let list = sqlite_tables(&conn).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "listing counted the view's rows");
        assert_eq!(list.iter().map(|t| (t["name"].as_str().unwrap(), t["kind"].as_str().unwrap())).collect::<Vec<_>>(), [("t", "table"), ("forever", "view")]);
        assert!(list[1]["rows"].is_null());
        // Brought in without naming it, the view is left out.
        let (e, r) = from_sqlite(&bytes, "trap.db", None).unwrap();
        assert_eq!(e.get_worksheets_properties().len(), 1);
        assert_eq!(r["tables"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_deadline_stops_a_query_that_does_not_end() {
        let bytes = endless_view();
        let (conn, _f) = open_sqlite(&bytes).unwrap();
        let _d = Deadline::new(&conn, 1);
        let started = std::time::Instant::now();
        let e = conn.query_row("SELECT c FROM forever", [], |r| r.get::<_, i64>(0)).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert!(sqlite_err(e, 1).contains("took more than 1 s"));
        // A deadline dropped in time interrupts nothing.
        let d = Deadline::new(&conn, 1);
        drop(d);
        std::thread::sleep(std::time::Duration::from_millis(1500));
        assert_eq!(conn.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    }

    #[test]
    fn an_xlsx_that_expands_past_the_bound_is_refused() {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("xl/worksheets/sheet1.xml", zip::write::SimpleFileOptions::default()).unwrap();
        let zeros = vec![b' '; 1 << 20];
        for _ in 0..8 {
            w.write_all(&zeros).unwrap();
        }
        let bytes = w.finish().unwrap().into_inner();
        assert!(bytes.len() < 100_000, "8 MB of spaces compress to {} bytes", bytes.len());
        let e = check_xlsx_size(&bytes, 4 << 20).unwrap_err();
        assert!(e.contains("expands to more than 4 MB"), "{e}");
        assert!(check_xlsx_size(&bytes, 16 << 20).is_ok());
        // A real workbook passes the default bound.
        let x = to_xlsx(&empty_model()).unwrap();
        assert!(check_xlsx_size(&x, MAX_XLSX_EXPANDED).is_ok());
    }
}
