//! The workbooks open in memory. A workbook is read from Strata the first
//! time it is asked for, edited under its own lock (one change at a time,
//! in the order they came), and saved after every change: the cells that
//! changed are written, as one write, before the answer goes back. Books
//! unused for a while are let go (they are in Strata); a book Strata could
//! not take is kept and saved again until it can.

use crate::book::Shown;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::OwnedMutexGuard;

use crate::book::{as_text, new_engine, Book, Engine, Ext, Key, Query};
use crate::ops::{self, format_num, Op};
use crate::store;
use crate::strata::{ident, Strata};

type Slot = Arc<tokio::sync::Mutex<Option<Book>>>;
pub type Guard = OwnedMutexGuard<Option<Book>>;

/// Rows a query or a copied table brings into a sheet at most.
pub const MAX_QUERY_ROWS: usize = 100_000;
/// Cells a change sends back to its viewer at most (beyond, it reloads).
const MAX_CELLS_BACK: usize = 20_000;
/// A book unused for this long is let go.
const IDLE: Duration = Duration::from_secs(30 * 60);
/// Cells the open books hold together at most: past it the least recently
/// used are let go (they are in Strata), as well as the idle ones. A cell
/// open takes about a kilobyte (IronCalc's model, and what was saved), so
/// a few large imports kept the process at gigabytes for half an hour
/// (DAP-9). `CHLOEDB_SHEETS_OPEN_CELLS` sets it.
const OPEN_CELLS: usize = 1_500_000;

fn open_cells_budget() -> usize {
    std::env::var("CHLOEDB_SHEETS_OPEN_CELLS").ok().and_then(|v| v.parse().ok()).unwrap_or(OPEN_CELLS)
}

/// Gives the memory freed back to the system (glibc keeps it otherwise).
pub fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}

pub struct Books {
    pub strata: Strata,
    slots: Mutex<HashMap<String, Slot>>,
    ready: tokio::sync::OnceCell<()>,
}

#[derive(Debug)]
pub struct Fail {
    pub status: u16,
    pub message: String,
}

impl Fail {
    pub fn bad(m: impl Into<String>) -> Fail {
        Fail { status: 400, message: m.into() }
    }
    fn of(e: String) -> Fail {
        let status = if e.starts_with("no workbook") { 404 } else if e.starts_with("HTTP 4") { 400 } else { 502 };
        Fail { status, message: e }
    }
}

/// A SQL query whose answer fills a block of a sheet: its names in the
/// first row, then its rows.
#[derive(Debug, Deserialize)]
pub struct QuerySpec {
    #[serde(default)]
    pub id: Option<String>,
    pub sheet: u32,
    pub row: i32,
    pub col: i32,
    pub sql: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

impl Books {
    pub fn new(strata: Strata) -> Arc<Books> {
        Arc::new(Books { strata, slots: Mutex::new(HashMap::new()), ready: tokio::sync::OnceCell::new() })
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Creates the tables, once.
    pub async fn ready(&self) -> Result<(), Fail> {
        self.ready.get_or_try_init(|| async { store::ensure_tables(&self.strata).await }).await.map(|_| ()).map_err(|e| Fail { status: 502, message: format!("ChloeDB: {e}") })
    }

    pub fn open_count(&self) -> usize {
        self.slots().len()
    }

    /// A workbook, under its lock: read from Strata if it is not open.
    pub async fn open(&self, id: &str) -> Result<Guard, Fail> {
        self.ready().await?;
        let slot = self.slots().entry(id.to_string()).or_default().clone();
        let mut g = slot.clone().lock_owned().await;
        if g.is_none() {
            match store::load(&self.strata, id).await {
                Ok(b) => *g = Some(b),
                Err(e) => {
                    drop(g);
                    let mut s = self.slots();
                    if s.get(id).is_some_and(|x| Arc::ptr_eq(x, &slot)) && slot.try_lock().is_ok_and(|x| x.is_none()) {
                        s.remove(id);
                    }
                    return Err(Fail::of(e));
                }
            }
        }
        if let Some(b) = g.as_mut() {
            b.last_used = Instant::now();
        }
        Ok(g)
    }

    /// Keeps a new workbook (saved first), and returns its summary.
    pub async fn create(&self, engine: Engine, source: &str, ext: Ext) -> Result<Value, Fail> {
        self.ready().await?;
        let mut b = Book::new(store::new_id("b"), source, engine, ext, store::now_ms());
        let s = tokio::task::block_in_place(|| b.changes());
        store::write(&self.strata, &b, &s).await.map_err(Fail::of)?;
        b.saved(s);
        let summary = b.summary();
        let slot: Slot = Arc::new(tokio::sync::Mutex::new(Some(b)));
        self.slots().insert(summary["id"].as_str().unwrap_or_default().to_string(), slot);
        self.let_go_over(open_cells_budget());
        trim();
        Ok(summary)
    }

    pub async fn delete(&self, id: &str) -> Result<(), Fail> {
        self.ready().await?;
        let slot = self.slots().get(id).cloned();
        let _g = match &slot {
            Some(s) => Some(s.clone().lock_owned().await),
            None => None,
        };
        store::delete(&self.strata, id).await.map_err(Fail::of)?;
        self.slots().remove(id);
        Ok(())
    }

    /// Saves what changed, and notes it for the other viewers. The cells
    /// that changed, unless the layout did (then `None`).
    pub async fn save(&self, b: &mut Book, layout: bool) -> Option<Vec<Key>> {
        let s = tokio::task::block_in_place(|| b.changes());
        if s.is_empty() {
            if layout {
                b.note(None);
            }
            return if layout { None } else { Some(Vec::new()) };
        }
        let mut keys: Vec<Key> = s.cells.iter().map(|c| c.0).collect();
        keys.extend(s.deleted.iter().copied());
        let cells = (!layout).then_some(keys);
        b.note(cells.clone());
        match store::write(&self.strata, b, &s).await {
            Ok(_) => b.saved(s),
            Err(e) => {
                tracing::warn!("workbook {}: not saved: {e}", b.id);
                b.unsaved = Some(e);
            }
        }
        cells
    }

    /// Applies operations in order, saves, and answers with what changed:
    /// the cells (or `layout: true`), each operation's result, and the
    /// book's summary.
    pub async fn apply(&self, id: &str, list: Vec<Op>) -> Result<Value, Fail> {
        let mut g = self.open(id).await?;
        let b = g.as_mut().expect("open");
        let mut results = Vec::new();
        let mut layout = false;
        let mut error = None;
        for op in list {
            match tokio::task::block_in_place(|| ops::apply(b, op)) {
                Ok(d) => {
                    layout |= d.layout;
                    results.push(d.data.unwrap_or(Value::Null));
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        let cells = self.save(b, layout).await;
        Ok(self.answer(b, cells, results, error))
    }

    fn answer(&self, b: &Book, cells: Option<Vec<Key>>, results: Vec<Value>, error: Option<String>) -> Value {
        let (layout, cells) = match cells {
            Some(k) if k.len() <= MAX_CELLS_BACK => (false, b.cells_at(&k)),
            Some(_) | None => (true, Value::Null),
        };
        json!({"version": b.version, "layout": layout, "cells": cells, "results": results, "error": error, "book": b.summary()})
    }

    /// What changed since version `since`: the cells, or `layout: true`
    /// when the view should draw the sheets again.
    pub async fn changes(&self, id: &str, since: u64) -> Result<Value, Fail> {
        let g = self.open(id).await?;
        let b = g.as_ref().expect("open");
        if since >= b.version {
            return Ok(json!({"version": b.version, "layout": false, "cells": null}));
        }
        let first = b.log.front().map(|c| c.version).unwrap_or(u64::MAX);
        let mut keys = Vec::new();
        let mut layout = first > since + 1;
        if !layout {
            for c in b.log.iter().filter(|c| c.version > since) {
                match &c.cells {
                    Some(k) => keys.extend(k.iter().copied()),
                    None => {
                        layout = true;
                        break;
                    }
                }
            }
        }
        keys.sort();
        keys.dedup();
        Ok(self.answer(b, (!layout).then_some(keys), Vec::new(), None))
    }

    /// Lets go of the least recently used books, saved and not in use,
    /// until the open ones hold at most `budget` cells. The books let go.
    pub fn let_go_over(&self, budget: usize) -> usize {
        let mut open: Vec<(String, Slot, usize, Instant)> = Vec::new();
        let mut total = 0;
        for (id, slot) in self.slots().iter() {
            let Ok(g) = slot.try_lock() else { continue };
            if let Some(b) = g.as_ref() {
                let n = b.saved.len();
                total += n;
                if b.unsaved.is_none() {
                    open.push((id.clone(), slot.clone(), n, b.last_used));
                }
            }
        }
        if total <= budget {
            return 0;
        }
        open.sort_by_key(|o| o.3);
        let mut gone = 0;
        for (id, slot, n, _) in open {
            if total <= budget {
                break;
            }
            let mut s = self.slots();
            // still that book, and still not in use
            if s.get(&id).is_some_and(|x| Arc::ptr_eq(x, &slot)) && slot.try_lock().is_ok() {
                s.remove(&id);
                total -= n;
                gone += 1;
            }
        }
        if gone > 0 {
            tracing::info!("let go of {gone} workbooks: the open ones held more than {budget} cells");
        }
        gone
    }

    /// Lets go of books unused for a while, and saves again those Strata
    /// could not take. Runs until the process ends.
    pub async fn keeper(self: Arc<Books>) {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let slots: Vec<(String, Slot)> = self.slots().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (id, slot) in slots {
                let Ok(mut g) = slot.clone().try_lock_owned() else { continue };
                let Some(b) = g.as_mut() else { continue };
                if b.unsaved.is_some() {
                    let s = tokio::task::block_in_place(|| b.changes());
                    match store::write(&self.strata, b, &s).await {
                        Ok(_) => {
                            tracing::info!("workbook {id}: saved");
                            b.saved(s)
                        }
                        Err(e) => b.unsaved = Some(e),
                    }
                } else if b.last_used.elapsed() > IDLE {
                    drop(g);
                    let mut s = self.slots();
                    if s.get(&id).is_some_and(|x| Arc::ptr_eq(x, &slot)) {
                        s.remove(&id);
                    }
                }
            }
            self.let_go_over(open_cells_budget());
            trim();
        }
    }

    /// Saves every book Strata has not taken yet (before the process ends).
    pub async fn flush(&self) {
        let slots: Vec<Slot> = self.slots().values().cloned().collect();
        for slot in slots {
            let mut g = slot.lock().await;
            if let Some(b) = g.as_mut().filter(|b| b.unsaved.is_some()) {
                let s = b.changes();
                if store::write(&self.strata, b, &s).await.is_ok() {
                    b.saved(s);
                }
            }
        }
    }

    // ------------------------------------------------------------ queries

    /// Runs a query into a block of a sheet (a new one, or the one `id` names
    /// again), and saves.
    pub async fn query(&self, book: &str, spec: QuerySpec, run: &Strata) -> Result<Value, Fail> {
        let mut g = self.open(book).await?;
        let b = g.as_mut().expect("open");
        let err = self.fill_query(b, spec, run).await.err();
        let cells = self.save(b, false).await;
        Ok(self.answer(b, cells, Vec::new(), err))
    }

    /// Runs every query of a book again.
    pub async fn refresh_all(&self, book: &str, run: &Strata) -> Result<Value, Fail> {
        let mut g = self.open(book).await?;
        let b = g.as_mut().expect("open");
        let qs = b.ext.queries.clone();
        let mut errors = Vec::new();
        for q in qs {
            let Some(sheet) = b.sheet_index(q.sheet) else { continue };
            let spec = QuerySpec { id: Some(q.id.clone()), sheet, row: q.row, col: q.col, sql: q.sql.clone(), limit: None };
            if let Err(e) = self.fill_query(b, spec, run).await {
                errors.push(json!({"id": q.id, "error": e}));
            }
        }
        let cells = self.save(b, false).await;
        let mut a = self.answer(b, cells, Vec::new(), None);
        a["errors"] = json!(errors);
        Ok(a)
    }

    /// `run`: the client the query runs with, narrowed to the caller's
    /// tables ([`Strata::as_caller`]).
    async fn fill_query(&self, b: &mut Book, spec: QuerySpec, run: &Strata) -> Result<(), String> {
        let sid = b.sheet_id(spec.sheet).ok_or("no such sheet")?;
        let sql = spec.sql.trim().trim_end_matches(';').to_string();
        if sql.is_empty() {
            return Err("give a SELECT".into());
        }
        let limit = spec.limit.unwrap_or(MAX_QUERY_ROWS).clamp(1, MAX_QUERY_ROWS);
        let t0 = Instant::now();
        let mut names: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let answer = run
            .scroll(&sql, json!({}), limit, |cols, page| {
                if names.is_empty() {
                    names = cols.to_vec();
                }
                for r in page {
                    if rows.len() >= limit {
                        break;
                    }
                    let line = r.as_array().map(|a| a.iter().map(input_of).collect()).unwrap_or_default();
                    rows.push(line);
                }
            })
            .await;
        let took_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let id = match spec.id.clone() {
            Some(id) => id,
            None => {
                b.ext.next += 1;
                format!("q{}", b.ext.next)
            }
        };
        let old = b.ext.queries.iter().position(|q| q.id == id);
        let mut q = match old {
            Some(i) => b.ext.queries[i].clone(),
            None => Query { id: id.clone(), sheet: sid, row: spec.row, col: spec.col, sql: sql.clone(), rows: 0, cols: 0, refreshed_ms: 0, took_ms: 0.0, error: None },
        };
        q.sql = sql;
        q.took_ms = took_ms;
        q.refreshed_ms = store::now_ms();
        let result = match answer {
            Ok(_) => {
                let e = &mut b.engine;
                // The block it filled before is cleared first.
                if q.rows > 0 && q.cols > 0 {
                    if let Some(si) = b_sheet_index(e, q.sheet) {
                        let _ = e.range_clear_contents(&ironcalc::base::expressions::types::Area { sheet: si, row: q.row, column: q.col, width: q.cols, height: q.rows });
                    }
                }
                q.sheet = sid;
                q.row = spec.row;
                q.col = spec.col;
                let mut grid: Vec<Vec<String>> = vec![names.iter().map(|n| as_text(n)).collect()];
                grid.extend(rows);
                q.rows = grid.len() as i32;
                q.cols = names.len() as i32;
                q.error = None;
                let r = tokio::task::block_in_place(|| ops::paste_values(e, spec.sheet, spec.row, spec.col, &grid));
                if r.is_ok() && !names.is_empty() {
                    let _ = e.update_range_style(&ironcalc::base::expressions::types::Area { sheet: spec.sheet, row: spec.row, column: spec.col, width: names.len() as i32, height: 1 }, "font.b", "true");
                }
                r
            }
            Err(e) => {
                q.error = Some(e.message.clone());
                Err(e.to_string())
            }
        };
        match old {
            Some(i) => b.ext.queries[i] = q,
            None => b.ext.queries.push(q),
        }
        result
    }

    pub fn forget_query(b: &mut Book, id: &str) {
        b.ext.queries.retain(|q| q.id != id);
    }

    // ------------------------------------------------------------- tables

    /// A new workbook of a Strata table (or of any `SELECT`): a sheet of its
    /// rows, the names in the first row, frozen.
    pub async fn from_table(&self, table: Option<&str>, sql: Option<&str>, name: Option<&str>, limit: Option<usize>, run: &Strata, ext: Ext) -> Result<Value, Fail> {
        let sql = match (sql.filter(|s| !s.trim().is_empty()), table) {
            (Some(s), _) => s.trim().trim_end_matches(';').to_string(),
            (None, Some(t)) => format!("SELECT * FROM {}", ident(t)),
            _ => return Err(Fail::bad("give a table or a SELECT")),
        };
        let limit = limit.unwrap_or(MAX_QUERY_ROWS).clamp(1, MAX_QUERY_ROWS);
        let mut names: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<Value>> = Vec::new();
        run.scroll(&sql, json!({}), limit, |cols, page| {
                if names.is_empty() {
                    names = cols.to_vec();
                }
                for r in page {
                    if let Value::Array(a) = r {
                        rows.push(a);
                    }
                }
            })
            .await
            .map_err(|e| Fail { status: if e.status == 0 || e.status >= 500 { 502 } else if e.status == 403 { 403 } else { 400 }, message: e.to_string() })?;
        rows.truncate(limit);
        let title = name.map(str::to_string).or_else(|| table.map(str::to_string)).unwrap_or_else(|| "Query".into());
        let n_rows = rows.len();
        let engine = tokio::task::block_in_place(|| table_engine(&title, &names, rows))?;
        let mut summary = self.create(engine, &format!("table:{}", table.unwrap_or("query")), ext).await?;
        summary["copied"] = json!({"rows": n_rows, "columns": names.len(), "sql": sql});
        Ok(summary)
    }

    /// Writes a sheet into a new Strata table: its first row names the
    /// columns, each column's type is what its values are (INT, FLOAT,
    /// BOOL or TEXT), and the rows are keyed by `key` (a column whose
    /// values are distinct) or by their number.
    pub async fn to_table(&self, book: &str, sheet: u32, table: &str, key: Option<&str>, run: &Strata) -> Result<Value, Fail> {
        let (cols, docs) = {
            let g = self.open(book).await?;
            let b = g.as_ref().expect("open");
            sheet_rows(b, sheet, key)?
        };
        if docs.is_empty() {
            return Err(Fail::bad("the sheet has no rows below its first"));
        }
        let t = table.trim();
        if t.is_empty() || !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || t.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(Fail::bad("a table's name is letters, digits and _, not starting with a digit"));
        }
        let ddl = format!(
            "CREATE TABLE {} ({})",
            ident(t),
            cols.iter().map(|(n, ty, pk)| format!("{} {}{}", ident(n), ty, if *pk { " PRIMARY KEY" } else { "" })).collect::<Vec<_>>().join(", ")
        );
        run.sql(&ddl).await.map_err(|e| Fail { status: if e.status == 409 || e.status == 403 { e.status } else if e.status >= 500 || e.status == 0 { 502 } else { 400 }, message: e.to_string() })?;
        let n = docs.len();
        let base = store::new_id("t");
        for (i, chunk) in docs.chunks(store::WRITE_OPS).enumerate() {
            let ops: Vec<Value> = chunk.iter().map(|d| json!({"op": "upsert", "table": t, "doc": d})).collect();
            run.write(ops, &format!("{base}-{i}"), store::PATIENCE).await.map_err(|e| Fail { status: if e.status == 403 { 403 } else { 502 }, message: e.to_string() })?;
        }
        Ok(json!({"table": t, "rows": n, "columns": cols.iter().map(|(n, ty, pk)| json!({"name": n, "type": ty, "key": pk})).collect::<Vec<_>>(), "sql": ddl}))
    }
}

fn b_sheet_index(e: &Engine, id: u32) -> Option<u32> {
    e.get_model().workbook.worksheets.iter().position(|w| w.sheet_id == id).map(|i| i as u32)
}

/// What to type for a value of a Strata answer.
pub fn input_of(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Number(n) => n.as_f64().map(format_num).unwrap_or_else(|| n.to_string()),
        Value::String(s) => as_text(s),
        other => as_text(&other.to_string()),
    }
}

/// A workbook of one sheet holding rows, their names in the first row.
pub fn table_engine(title: &str, names: &[String], rows: Vec<Vec<Value>>) -> Result<Engine, Fail> {
    let mut e = new_engine(title);
    let sheet = crate::io::sheet_name(title, &[]);
    e.rename_sheet(0, &sheet).map_err(Fail::bad)?;
    let mut grid: Vec<Vec<String>> = Vec::with_capacity(rows.len() + 1);
    grid.push(names.iter().map(|n| as_text(n)).collect());
    for r in &rows {
        grid.push(r.iter().map(input_of).collect());
    }
    e.pause_evaluation();
    ops::paste_values(&mut e, 0, 1, 1, &grid).map_err(Fail::bad)?;
    let w = names.len().max(1) as i32;
    let _ = e.update_range_style(&ironcalc::base::expressions::types::Area { sheet: 0, row: 1, column: 1, width: w, height: 1 }, "font.b", "true");
    let _ = e.set_frozen_rows_count(0, 1);
    for (j, n) in names.iter().enumerate() {
        let longest = rows.iter().take(200).map(|r| r.get(j).map(|v| input_of(v).chars().count()).unwrap_or(0)).max().unwrap_or(0).max(n.chars().count());
        let _ = e.set_columns_width(0, j as i32 + 1, j as i32 + 1, (longest as f64 * 7.5 + 20.0).clamp(60.0, 320.0));
    }
    e.resume_evaluation();
    e.evaluate();
    // A new book starts with nothing to undo.
    Ok(crate::book::engine_of(e.get_model().workbook.clone()).map_err(Fail::bad)?)
}

/// A sheet as rows for a table: the columns (name, type, key) and the docs.
fn sheet_rows(b: &Book, sheet: u32, key: Option<&str>) -> Result<(Vec<(String, &'static str, bool)>, Vec<Value>), Fail> {
    use ironcalc::base::cell::CellValue;
    let m = b.model();
    let ws = m.workbook.worksheet(sheet).map_err(Fail::bad)?;
    if ws.sheet_data.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let d = ws.dimension();
    let mut names: Vec<String> = Vec::new();
    for c in 1..=d.max_column {
        let n = m.shown(sheet, 1, c).unwrap_or_default();
        let mut n: String = n.trim().chars().map(|ch| if ch.is_ascii_alphanumeric() { ch.to_ascii_lowercase() } else { '_' }).collect();
        n = n.trim_matches('_').to_string();
        if n.is_empty() || n.starts_with(|ch: char| ch.is_ascii_digit()) {
            n = format!("col_{}", crate::book::col_name(c).to_lowercase());
        }
        while names.contains(&n) {
            n.push('_');
        }
        names.push(n);
    }
    // Each column's kind: 0 nothing yet, 1 int, 2 float, 3 bool, 4 text.
    let mut kind = vec![0u8; names.len()];
    let mut values: Vec<Vec<Value>> = Vec::new();
    for r in 2..=d.max_row {
        let mut line = Vec::with_capacity(names.len());
        let mut any = false;
        for (j, k) in kind.iter_mut().enumerate() {
            let c = j as i32 + 1;
            let v = match m.get_cell_value_by_index(sheet, r, c) {
                Ok(CellValue::Number(x)) => {
                    let shown = m.shown(sheet, r, c).unwrap_or_default();
                    let is_int = x.fract() == 0.0 && x.abs() < 9e15;
                    *k = match *k {
                        0 | 1 if is_int => 1,
                        0..=2 => 2,
                        _ => 4,
                    };
                    let _ = shown;
                    json!(x)
                }
                Ok(CellValue::Boolean(x)) => {
                    *k = if *k == 0 || *k == 3 { 3 } else { 4 };
                    json!(x)
                }
                Ok(CellValue::String(s)) if !s.is_empty() => {
                    *k = 4;
                    json!(s)
                }
                _ => Value::Null,
            };
            any |= !v.is_null();
            line.push(v);
        }
        if any {
            values.push(line);
        }
    }
    let key_col = match key.filter(|k| !k.trim().is_empty()) {
        Some(k) => {
            let k = k.trim().to_lowercase();
            let j = names.iter().position(|n| *n == k).ok_or_else(|| Fail::bad(format!("no column {k} (the columns: {})", names.join(", "))))?;
            let mut seen = std::collections::HashSet::new();
            for v in &values {
                if v[j].is_null() || !seen.insert(v[j].to_string()) {
                    return Err(Fail::bad(format!("column {k} is empty or repeats a value: it cannot be the key")));
                }
            }
            Some(j)
        }
        None => None,
    };
    let mut cols: Vec<(String, &'static str, bool)> = Vec::new();
    if key_col.is_none() {
        let mut k = "row".to_string();
        while names.contains(&k) {
            k.push('_');
        }
        cols.push((k, "INT", true));
    }
    for (j, n) in names.iter().enumerate() {
        let ty = if Some(j) == key_col {
            if kind[j] == 1 {
                "INT"
            } else {
                "KEYWORD"
            }
        } else {
            match kind[j] {
                1 => "INT",
                2 => "FLOAT",
                3 => "BOOL",
                _ => "TEXT",
            }
        };
        cols.push((n.clone(), ty, Some(j) == key_col));
    }
    let docs = values
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let mut o = serde_json::Map::new();
            if key_col.is_none() {
                o.insert(cols[0].0.clone(), json!(i + 1));
            }
            for (j, v) in line.into_iter().enumerate() {
                if v.is_null() {
                    continue;
                }
                let ty = cols.iter().find(|c| c.0 == names[j]).map(|c| c.1).unwrap_or("TEXT");
                let v = match (ty, &v) {
                    ("INT", Value::Number(x)) => json!(x.as_f64().unwrap_or(0.0) as i64),
                    ("KEYWORD", Value::Number(x)) => json!(format_num(x.as_f64().unwrap_or(0.0))),
                    ("TEXT", Value::Number(x)) => json!(format_num(x.as_f64().unwrap_or(0.0))),
                    ("TEXT", Value::Bool(x)) => json!(if *x { "TRUE" } else { "FALSE" }),
                    _ => v,
                };
                o.insert(names[j].clone(), v);
            }
            Value::Object(o)
        })
        .collect();
    Ok((cols, docs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Books open past the budget of cells are let go, the least recently
    /// used first; a book in use, or not yet saved, is kept (DAP-9).
    #[tokio::test]
    async fn the_least_recently_used_books_are_let_go_past_the_budget() {
        let books = Books::new(Strata::new("http://127.0.0.1:9", None));
        let t0 = Instant::now();
        for (i, (cells, unsaved)) in [(400, false), (300, true), (500, false), (200, false)].into_iter().enumerate() {
            let mut b = Book::new(format!("b{i}"), "new", new_engine("x"), Ext::default(), 0);
            b.saved = (0..cells).map(|r| (Key { sheet: 1, row: r + 1, col: 1 }, 0)).collect();
            b.last_used = t0 + Duration::from_secs(i as u64);
            b.unsaved = unsaved.then(|| "Strata did not answer".to_string());
            books.slots().insert(b.id.clone(), Arc::new(tokio::sync::Mutex::new(Some(b))));
        }
        assert_eq!(books.let_go_over(10_000), 0);
        // b2 is in use: kept, though it is old enough to go.
        let b2 = books.slots().get("b2").unwrap().clone();
        let held = b2.lock_owned().await;
        // Of the books not in use (900 cells), 300 at most: b0 (400) goes,
        // b1 is kept (unsaved), and b3 (200) goes.
        assert_eq!(books.let_go_over(300), 2);
        let mut left: Vec<String> = books.slots().keys().cloned().collect();
        left.sort();
        assert_eq!(left, ["b1", "b2"]);
        drop(held);
    }
}
