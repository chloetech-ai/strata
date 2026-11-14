//! What the view asks a workbook to do: type into cells, format, insert and
//! delete rows and columns, paste, fill, sort, filter, name ranges, add
//! sheets, undo. Each is one of IronCalc's actions (one step to undo), and
//! `sheet` is a sheet's index, as the view lists them.

use crate::book::Shown;
use std::cmp::Ordering;
use std::collections::HashMap;

use ironcalc::base::cell::CellValue;
use ironcalc::base::expressions::types::Area;
use ironcalc::base::types::Color;
use ironcalc::base::{BorderArea, ClipboardData};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::book::{Book, Chart, Engine, Key, SheetFilter};

/// A rectangle: its top left cell and its size.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Rect {
    pub row: i32,
    pub col: i32,
    #[serde(default = "one")]
    pub width: i32,
    #[serde(default = "one")]
    pub height: i32,
}

fn one() -> i32 {
    1
}

impl Rect {
    pub fn area(&self, sheet: u32) -> Area {
        Area { sheet, row: self.row.max(1), column: self.col.max(1), width: self.width.max(1), height: self.height.max(1) }
    }
    fn last_row(&self) -> i32 {
        self.row + self.height.max(1) - 1
    }
    fn last_col(&self) -> i32 {
        self.col + self.width.max(1) - 1
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Types `input` into a cell: a value, or a formula (`=...`).
    Set { sheet: u32, row: i32, col: i32, input: String },
    /// Types rows of inputs from a cell on.
    SetMany { sheet: u32, row: i32, col: i32, values: Vec<Vec<String>> },
    /// An array formula over an area (Ctrl+Shift+Enter).
    ArrayFormula { sheet: u32, area: Rect, input: String },
    /// Clears contents, formatting, or all.
    Clear { sheet: u32, area: Rect, #[serde(default)] what: Option<String> },
    /// One style property over an area: font.b, font.i, font.u, font.strike,
    /// font.color, font.size, font.size_delta, fill.color, num_fmt,
    /// alignment.horizontal, alignment.vertical, alignment.wrap_text, alignment.
    Style { sheet: u32, area: Rect, path: String, value: String },
    /// Borders over an area: `{"item": {"style": "thin", "color": "#000000"}, "type": "all"}`.
    Border { sheet: u32, area: Rect, border: Value },
    InsertRows { sheet: u32, row: i32, #[serde(default = "one")] count: i32 },
    DeleteRows { sheet: u32, row: i32, #[serde(default = "one")] count: i32 },
    InsertCols { sheet: u32, col: i32, #[serde(default = "one")] count: i32 },
    DeleteCols { sheet: u32, col: i32, #[serde(default = "one")] count: i32 },
    MoveRows { sheet: u32, row: i32, #[serde(default = "one")] count: i32, delta: i32 },
    MoveCols { sheet: u32, col: i32, #[serde(default = "one")] count: i32, delta: i32 },
    ColWidth { sheet: u32, col: i32, #[serde(default = "one")] count: i32, width: f64 },
    RowHeight { sheet: u32, row: i32, #[serde(default = "one")] count: i32, height: f64 },
    HideRows { sheet: u32, row: i32, #[serde(default = "one")] count: i32, hidden: bool },
    HideCols { sheet: u32, col: i32, #[serde(default = "one")] count: i32, hidden: bool },
    Freeze { sheet: u32, #[serde(default)] rows: Option<i32>, #[serde(default)] cols: Option<i32> },
    GridLines { sheet: u32, show: bool },
    /// Merges an area: `how` is merge (the default), center, across or down.
    Merge { sheet: u32, area: Rect, #[serde(default)] how: Option<String> },
    Unmerge { sheet: u32, area: Rect },
    AddSheet { #[serde(default)] name: Option<String> },
    DeleteSheet { sheet: u32 },
    RenameSheet { sheet: u32, name: String },
    MoveSheet { sheet: u32, to: u32 },
    DuplicateSheet { sheet: u32 },
    SheetColor { sheet: u32, color: String },
    HideSheet { sheet: u32, hidden: bool },
    /// Extends an area's pattern down (`to_row`) or right (`to_col`).
    Autofill { sheet: u32, area: Rect, #[serde(default)] to_row: Option<i32>, #[serde(default)] to_col: Option<i32> },
    /// A clipboard `copy` gave, pasted at `to` (its top left cell); `cut` moves it.
    Paste { sheet: u32, to: Rect, clipboard: Value, #[serde(default)] cut: bool },
    /// Text from another program (tab-separated lines) pasted at `to`.
    PasteText { sheet: u32, to: Rect, text: String },
    /// Sorts an area's rows by the values of column `by` (a column of the
    /// sheet); `header` keeps its first row in place.
    Sort { sheet: u32, area: Rect, by: i32, #[serde(default)] desc: bool, #[serde(default)] header: bool },
    /// Shows only the rows of an area that meet every criterion (the value
    /// of a column, as shown, is one of those listed); with none, shows
    /// every row again. The filter is kept with the book (its buttons stay
    /// on the header row) until one with `off` shows every row and drops it.
    Filter { sheet: u32, area: Rect, #[serde(default)] criteria: Vec<Crit>, #[serde(default)] header: bool, #[serde(default)] off: bool },
    /// Replaces text in the inputs of a sheet's cells (every sheet without one).
    Replace { #[serde(default)] sheet: Option<u32>, find: String, replace: String, #[serde(default)] case: bool, #[serde(default)] whole: bool },
    DefineName { name: String, #[serde(default)] scope: Option<u32>, formula: String },
    UpdateName { name: String, #[serde(default)] scope: Option<u32>, new_name: String, #[serde(default)] new_scope: Option<u32>, formula: String },
    DeleteName { name: String, #[serde(default)] scope: Option<u32> },
    /// A conditional format (IronCalc's rule, as `{"type": "CellIs", ...}`).
    AddFormat { sheet: u32, range: String, rule: Value },
    DeleteFormat { sheet: u32, index: u32 },
    Undo,
    Redo,
    RenameBook { name: String },
    /// A chart over a range (`id` to change one).
    Chart { #[serde(default)] id: Option<String>, sheet: u32, range: String, kind: String, #[serde(default)] title: String, row: i32, col: i32, #[serde(default)] width: Option<f64>, #[serde(default)] height: Option<f64>, #[serde(default)] stacked: bool },
    DeleteChart { id: String },
    /// A summary of an area (a pivot): its rows grouped by the values of
    /// columns `by`, and aggregates (sum, count, avg, min, max, countd) of
    /// columns, on a new sheet.
    Summarize { sheet: u32, area: Rect, by: Vec<i32>, values: Vec<Agg>, #[serde(default = "yes")] header: bool },
}

fn yes() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Crit {
    pub col: i32,
    pub values: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Agg {
    pub col: i32,
    pub agg: String,
}

/// What an operation gives back, beside the cells it changed.
#[derive(Default)]
pub struct Done {
    pub data: Option<Value>,
    /// The layout of the sheets may have changed: viewers draw them again.
    pub layout: bool,
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

impl Op {
    /// Whether this op can change where cells are or how a sheet is laid
    /// out, so that the view reloads rather than patching cells.
    pub fn moves_cells(&self) -> bool {
        !matches!(self, Op::Set { .. } | Op::SetMany { .. } | Op::Clear { .. } | Op::Style { .. } | Op::Border { .. } | Op::ArrayFormula { .. } | Op::PasteText { .. } | Op::Paste { .. } | Op::Autofill { .. } | Op::Replace { .. } | Op::RenameBook { .. } | Op::Chart { .. } | Op::DeleteChart { .. })
    }
}

/// Applies `op` to the book's model.
pub fn apply(b: &mut Book, op: Op) -> Result<Done, String> {
    let layout = op.moves_cells();
    let e = &mut b.engine;
    let mut out = Done { layout, ..Done::default() };
    match op {
        Op::Set { sheet, row, col, input } => e.set_user_input(sheet, row, col, &input)?,
        Op::SetMany { sheet, row, col, values } => paste_values(e, sheet, row, col, &values)?,
        Op::ArrayFormula { sheet, area, input } => e.set_user_array_formula(sheet, area.row, area.col, area.width, area.height, &input)?,
        Op::Clear { sheet, area, what } => match what.as_deref().unwrap_or("contents") {
            "contents" => e.range_clear_contents(&area.area(sheet))?,
            "formatting" | "formats" => e.range_clear_formatting(&area.area(sheet))?,
            "all" => e.range_clear_all(&area.area(sheet))?,
            w => return Err(format!("clear {w:?}: contents, formatting or all")),
        },
        Op::Style { sheet, area, path, value } => e.update_range_style(&area.area(sheet), &path, &value)?,
        Op::Border { sheet, area, border } => {
            let ba: BorderArea = serde_json::from_value(border).map_err(|x| format!("a border: {x}"))?;
            e.set_area_with_border(&area.area(sheet), &ba)?
        }
        Op::InsertRows { sheet, row, count } => e.insert_rows(sheet, row, count)?,
        Op::DeleteRows { sheet, row, count } => e.delete_rows(sheet, row, count)?,
        Op::InsertCols { sheet, col, count } => e.insert_columns(sheet, col, count)?,
        Op::DeleteCols { sheet, col, count } => e.delete_columns(sheet, col, count)?,
        Op::MoveRows { sheet, row, count, delta } => e.move_rows_action(sheet, row, count, delta)?,
        Op::MoveCols { sheet, col, count, delta } => e.move_columns_action(sheet, col, count, delta)?,
        Op::ColWidth { sheet, col, count, width } => e.set_columns_width(sheet, col, col + count.max(1) - 1, width.clamp(4.0, 2000.0))?,
        Op::RowHeight { sheet, row, count, height } => e.set_rows_height(sheet, row, row + count.max(1) - 1, height.clamp(4.0, 1000.0))?,
        Op::HideRows { sheet, row, count, hidden } => e.set_rows_hidden(sheet, row, row + count.max(1) - 1, hidden)?,
        Op::HideCols { sheet, col, count, hidden } => e.set_columns_hidden(sheet, col, col + count.max(1) - 1, hidden)?,
        Op::Freeze { sheet, rows, cols } => {
            if let Some(r) = rows {
                e.set_frozen_rows_count(sheet, r.max(0))?;
            }
            if let Some(c) = cols {
                e.set_frozen_columns_count(sheet, c.max(0))?;
            }
        }
        Op::GridLines { sheet, show } => e.set_show_grid_lines(sheet, show)?,
        Op::Merge { sheet, area, how } => match how.as_deref().unwrap_or("merge") {
            "merge" => e.merge_cells(&area.area(sheet))?,
            "center" => e.merge_cells_center(&area.area(sheet))?,
            "across" => e.merge_cells_across(&area.area(sheet))?,
            "down" => e.merge_cells_down(&area.area(sheet))?,
            h => return Err(format!("merge {h:?}: merge, center, across or down")),
        },
        Op::Unmerge { sheet, area } => e.unmerge_cells(&area.area(sheet))?,
        Op::AddSheet { name } => {
            e.new_sheet()?;
            let n = e.get_worksheets_properties().len() as u32 - 1;
            if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
                e.rename_sheet(n, name.trim())?;
            }
            out.data = Some(json!({"sheet": n}));
        }
        Op::DeleteSheet { sheet } => {
            let id = b.sheet_id(sheet);
            b.engine.delete_sheet(sheet)?;
            if let Some(id) = id {
                b.ext.charts.retain(|c| c.sheet != id);
                b.ext.queries.retain(|q| q.sheet != id);
                b.ext.filters.retain(|f| f.sheet != id);
            }
        }
        Op::RenameSheet { sheet, name } => e.rename_sheet(sheet, name.trim())?,
        Op::MoveSheet { sheet, to } => e.move_sheet(sheet, to)?,
        Op::DuplicateSheet { sheet } => {
            e.duplicate_sheet(sheet)?;
            out.data = Some(json!({"sheet": e.get_worksheets_properties().len() as u32 - 1}));
        }
        Op::SheetColor { sheet, color } => e.set_sheet_color(sheet, &if color.is_empty() { Color::None } else { Color::Rgb(color) })?,
        Op::HideSheet { sheet, hidden } => {
            if hidden {
                e.hide_sheet(sheet)?
            } else {
                e.unhide_sheet(sheet)?
            }
        }
        Op::Autofill { sheet, area, to_row, to_col } => {
            let a = area.area(sheet);
            match (to_row, to_col) {
                (Some(r), _) => e.auto_fill_rows(&a, r)?,
                (None, Some(c)) => e.auto_fill_columns(&a, c)?,
                _ => return Err("autofill: give to_row or to_col".into()),
            }
        }
        Op::Paste { sheet, to, clipboard, cut } => {
            let src_sheet = clipboard["sheet"].as_u64().unwrap_or(sheet as u64) as u32;
            let range = clipboard["range"].as_array().map(|r| r.iter().filter_map(|x| x.as_i64()).map(|x| x as i32).collect::<Vec<_>>()).unwrap_or_default();
            if range.len() != 4 {
                return Err("paste: a clipboard from copy".into());
            }
            let data: ClipboardData = serde_json::from_value(clipboard["data"].clone()).map_err(|x| format!("paste: the clipboard's cells: {x}"))?;
            select(e, sheet, to)?;
            e.paste_from_clipboard(src_sheet, (range[0], range[1], range[2], range[3]), &data, cut)?;
            out.layout = cut;
        }
        Op::PasteText { sheet, to, text } => {
            let text = text.replace("\r\n", "\n");
            let values: Vec<Vec<String>> = text.trim_end_matches('\n').split('\n').map(|l| l.split('\t').map(str::to_string).collect()).collect();
            paste_values(e, sheet, to.row, to.col, &values)?
        }
        Op::Sort { sheet, area, by, desc, header } => sort(e, sheet, area, by, desc, header)?,
        Op::Filter { sheet, area, criteria, header, off } => {
            filter(e, sheet, area, if off { &[] } else { &criteria }, header)?;
            let sid = b.sheet_id(sheet).ok_or("no such sheet")?;
            b.ext.filters.retain(|f| f.sheet != sid);
            if !off {
                b.ext.filters.push(SheetFilter { sheet: sid, row: area.row, col: area.col, width: area.width.max(1), height: area.height.max(1), criteria });
            }
        }
        Op::Replace { sheet, find, replace, case, whole } => {
            let n = replace_text(e, sheet, &find, &replace, case, whole)?;
            out.data = Some(json!({"replaced": n}));
        }
        Op::DefineName { name, scope, formula } => e.new_defined_name(&name, scope, &formula)?,
        Op::UpdateName { name, scope, new_name, new_scope, formula } => e.update_defined_name(&name, scope, &new_name, new_scope, &formula)?,
        Op::DeleteName { name, scope } => e.delete_defined_name(&name, scope)?,
        Op::AddFormat { sheet, range, rule } => {
            let rule = serde_json::from_value(rule).map_err(|x| format!("a conditional format: {x}"))?;
            e.add_conditional_formatting(sheet, &range, rule)?
        }
        Op::DeleteFormat { sheet, index } => e.delete_conditional_formatting(sheet, index)?,
        Op::Undo => e.undo()?,
        Op::Redo => e.redo()?,
        Op::RenameBook { name } => {
            let name = name.trim();
            if name.is_empty() {
                return Err("a workbook needs a name".into());
            }
            e.set_name(name)
        }
        Op::Chart { id, sheet, range, kind, title, row, col, width, height, stacked } => {
            let sid = b.sheet_id(sheet).ok_or("no such sheet")?;
            if !["bar", "column", "line", "area", "pie", "scatter"].contains(&kind.as_str()) {
                return Err(format!("chart kind {kind:?}: bar, column, line, area, pie or scatter"));
            }
            let id = match id {
                Some(id) => id,
                None => {
                    b.ext.next += 1;
                    format!("c{}", b.ext.next)
                }
            };
            let c = Chart { id: id.clone(), sheet: sid, range, kind, title, row, col, width: width.unwrap_or(480.0), height: height.unwrap_or(300.0), stacked };
            match b.ext.charts.iter_mut().find(|x| x.id == id) {
                Some(x) => *x = c,
                None => b.ext.charts.push(c),
            }
            out.data = Some(json!({"chart": id}));
        }
        Op::DeleteChart { id } => b.ext.charts.retain(|c| c.id != id),
        Op::Summarize { sheet, area, by, values, header } => {
            let n = summarize(b, sheet, area, &by, &values, header)?;
            out.data = Some(json!({"sheet": n}));
        }
    }
    Ok(out)
}

/// Types rows of inputs from a cell on, as one step to undo (IronCalc's
/// paste of tab-separated text: each value read as if typed).
pub fn paste_values(e: &mut Engine, sheet: u32, row: i32, col: i32, values: &[Vec<String>]) -> Result<(), String> {
    let width = values.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return Ok(());
    }
    let mut w = csv::WriterBuilder::new().delimiter(b'\t').quote_style(csv::QuoteStyle::Necessary).from_writer(Vec::new());
    for line in values {
        let mut rec: Vec<&str> = line.iter().map(String::as_str).collect();
        rec.resize(width, "");
        w.write_record(&rec).map_err(err)?;
    }
    let tsv = String::from_utf8(w.into_inner().map_err(err)?).map_err(err)?;
    let area = Area { sheet, row, column: col, width: width as i32, height: values.len() as i32 };
    // IronCalc selects what it pasted, from the selected cell.
    select(e, sheet, Rect { row, col, width: 1, height: 1 })?;
    e.paste_csv_string(&area, &tsv)
}

fn select(e: &mut Engine, sheet: u32, to: Rect) -> Result<(), String> {
    e.set_selected_sheet(sheet)?;
    e.set_selected_cell(to.row, to.col)?;
    e.set_selected_range(to.row, to.col, to.last_row(), to.last_col())
}

/// The clipboard of an area: its cells as IronCalc copies them (formulas,
/// styles), and the text other programs take (tab-separated).
pub fn copy(e: &mut Engine, sheet: u32, area: Rect) -> Result<Value, String> {
    select(e, sheet, area)?;
    let c = e.copy_to_clipboard()?;
    serde_json::to_value(c).map_err(err)
}

/// A value to sort by: numbers, then text, then booleans, then errors,
/// empty cells always last (as Excel sorts).
#[derive(Debug, Clone)]
enum SortKey {
    Num(f64),
    Text(String),
    Bool(bool),
    Err(String),
    Empty,
}

impl SortKey {
    fn rank(&self) -> u8 {
        match self {
            SortKey::Num(_) => 0,
            SortKey::Text(_) => 1,
            SortKey::Bool(_) => 2,
            SortKey::Err(_) => 3,
            SortKey::Empty => 4,
        }
    }
    fn cmp(&self, o: &SortKey) -> Ordering {
        match (self, o) {
            (SortKey::Num(a), SortKey::Num(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (SortKey::Text(a), SortKey::Text(b)) => a.to_lowercase().cmp(&b.to_lowercase()),
            (SortKey::Bool(a), SortKey::Bool(b)) => a.cmp(b),
            (SortKey::Err(a), SortKey::Err(b)) => a.cmp(b),
            _ => self.rank().cmp(&o.rank()),
        }
    }
}

fn sort_key(e: &Engine, sheet: u32, row: i32, col: i32) -> SortKey {
    let m = e.get_model();
    match m.get_cell_value_by_index(sheet, row, col) {
        Ok(CellValue::Number(n)) => SortKey::Num(n),
        Ok(CellValue::String(s)) if s.is_empty() => SortKey::Empty,
        Ok(CellValue::String(s)) => {
            if s.starts_with('#') && matches!(m.get_cell_type(sheet, row, col), Ok(ironcalc::base::types::CellType::ErrorValue)) {
                SortKey::Err(s)
            } else {
                SortKey::Text(s)
            }
        }
        Ok(CellValue::Boolean(b)) => SortKey::Bool(b),
        _ => SortKey::Empty,
    }
}

/// A cell of a clipboard as IronCalc reads one, its style written once
/// for each style it uses.
#[derive(serde::Serialize)]
struct ClipCell<'a> {
    text: String,
    is_spill: bool,
    style: &'a serde_json::value::RawValue,
    link: Option<Value>,
}

/// Sorts an area's rows as one paste: each row's cells, with their
/// formulas moved to where the row goes (as Excel moves them), are pasted
/// in place, so that one undo puts them back.
fn sort(e: &mut Engine, sheet: u32, area: Rect, by: i32, desc: bool, header: bool) -> Result<(), String> {
    use ironcalc::base::types::Cell;
    let (r0, r1) = (area.row + header as i32, area.last_row());
    let (c0, c1) = (area.col, area.last_col());
    if by < c0 || by > c1 {
        return Err("sort: the column is not in the area".into());
    }
    let m = e.get_model();
    let ws = m.workbook.worksheet(sheet)?;
    if ws.sheet_data.is_empty() {
        return Ok(());
    }
    let dim = ws.dimension();
    let (last, c1) = (r1.min(dim.max_row), c1.min(dim.max_column).max(c0));
    if last <= r0 {
        return Ok(());
    }
    let mut order: Vec<(i32, SortKey)> = (r0..=last).map(|r| (r, sort_key(e, sheet, r, by))).collect();
    order.sort_by(|a, b| {
        let o = match (&a.1, &b.1) {
            (SortKey::Empty, SortKey::Empty) => Ordering::Equal,
            (SortKey::Empty, _) => return Ordering::Greater,
            (_, SortKey::Empty) => return Ordering::Less,
            (x, y) => x.cmp(y),
        };
        if desc {
            o.reverse()
        } else {
            o
        }
    });
    if order.iter().enumerate().all(|(i, (src, _))| *src == r0 + i as i32) {
        return Ok(());
    }
    let mut styles: HashMap<i32, Box<serde_json::value::RawValue>> = HashMap::new();
    for r in r0..=last {
        for c in c0..=c1 {
            let si = m.get_cell_style_index(sheet, r, c).unwrap_or(0);
            if let std::collections::hash_map::Entry::Vacant(v) = styles.entry(si) {
                let st = m.get_style_for_cell(sheet, r, c)?;
                v.insert(serde_json::value::to_raw_value(&st).map_err(err)?);
            }
        }
    }
    let mut data: HashMap<i32, HashMap<i32, ClipCell>> = HashMap::with_capacity(order.len());
    for (i, (src, _)) in order.iter().enumerate() {
        let dst = r0 + i as i32;
        let mut row = HashMap::with_capacity((c1 - c0 + 1) as usize);
        for c in c0..=c1 {
            let cell = ws.sheet_data.cell(*src, c);
            let is_spill = matches!(cell.as_deref(), Some(Cell::SpillCell { .. }));
            let mut text = e.get_cell_content(sheet, *src, c)?;
            if text.starts_with('=') && *src != dst {
                if let Ok(t) = m.extend_to(sheet, *src, c, dst, c) {
                    text = t;
                }
            }
            let si = m.get_cell_style_index(sheet, *src, c).unwrap_or(0);
            let link = m.get_cell_link(sheet, *src, c).ok().flatten().and_then(|l| serde_json::to_value(l).ok());
            row.insert(c, ClipCell { text, is_spill, style: &styles[&si], link });
        }
        data.insert(dst, row);
    }
    let bytes = serde_json::to_vec(&data).map_err(err)?;
    drop(data);
    let data: ClipboardData = serde_json::from_slice(&bytes).map_err(err)?;
    select(e, sheet, Rect { row: r0, col: c0, width: 1, height: 1 })?;
    e.paste_from_clipboard(sheet, (r0, c0, last, c1), &data, false)
}

/// Hides the rows of an area that miss a criterion, and shows the others.
fn filter(e: &mut Engine, sheet: u32, area: Rect, criteria: &[Crit], header: bool) -> Result<(), String> {
    let (r0, r1) = (area.row + header as i32, area.last_row());
    if r1 < r0 {
        return Ok(());
    }
    if criteria.is_empty() {
        return e.set_rows_hidden(sheet, r0, r1, false);
    }
    let keep: Vec<(i32, std::collections::HashSet<&str>)> = criteria.iter().map(|c| (c.col, c.values.iter().map(String::as_str).collect())).collect();
    let mut hide = Vec::with_capacity((r1 - r0 + 1) as usize);
    for row in r0..=r1 {
        let mut h = false;
        for (col, vals) in &keep {
            let shown = e.shown(sheet, row, *col)?;
            if !vals.contains(shown.as_str()) {
                h = true;
                break;
            }
        }
        hide.push(h);
    }
    let mut start = 0;
    while start < hide.len() {
        let mut end = start;
        while end + 1 < hide.len() && hide[end + 1] == hide[start] {
            end += 1;
        }
        e.set_rows_hidden(sheet, r0 + start as i32, r0 + end as i32, hide[start])?;
        start = end + 1;
    }
    Ok(())
}

/// Replaces `find` in the inputs (text and formulas) of the cells.
fn replace_text(e: &mut Engine, sheet: Option<u32>, find: &str, replace: &str, case: bool, whole: bool) -> Result<usize, String> {
    if find.is_empty() {
        return Err("replace: give the text to find".into());
    }
    let sheets: Vec<u32> = match sheet {
        Some(s) => vec![s],
        None => (0..e.get_worksheets_properties().len() as u32).collect(),
    };
    let mut todo = Vec::new();
    for s in sheets {
        let ws = e.get_model().workbook.worksheet(s)?;
        let cells: Vec<(i32, i32)> = ws.sheet_data.cells().map(|(r, c, _)| (r, c)).collect();
        for (r, c) in cells {
            let input = e.get_cell_content(s, r, c)?;
            if input.is_empty() {
                continue;
            }
            if let Some(new) = replaced(&input, find, replace, case, whole) {
                todo.push((s, r, c, new));
            }
        }
    }
    e.pause_evaluation();
    let mut res = Ok(());
    for (s, r, c, new) in &todo {
        if let Err(x) = e.set_user_input(*s, *r, *c, new) {
            res = Err(x);
            break;
        }
    }
    e.resume_evaluation();
    e.evaluate();
    res.map(|_| todo.len())
}

/// `input` with `find` replaced, if it holds it.
pub fn replaced(input: &str, find: &str, replace: &str, case: bool, whole: bool) -> Option<String> {
    if whole {
        let eq = if case { input == find } else { input.to_lowercase() == find.to_lowercase() };
        return eq.then(|| replace.to_string());
    }
    if case {
        return input.contains(find).then(|| input.replace(find, replace));
    }
    let lower = input.to_lowercase();
    let f = find.to_lowercase();
    if !lower.contains(&f) {
        return None;
    }
    let mut out = String::new();
    let mut i = 0;
    while let Some(p) = lower[i..].find(&f) {
        out.push_str(&input[i..i + p]);
        out.push_str(replace);
        i += p + f.len();
    }
    out.push_str(&input[i..]);
    Some(out)
}

/// A summary of an area on a new sheet: one row a group of the values of
/// columns `by`, with the aggregates of `values`. Returns the new sheet.
fn summarize(b: &mut Book, sheet: u32, area: Rect, by: &[i32], values: &[Agg], header: bool) -> Result<u32, String> {
    if by.is_empty() && values.is_empty() {
        return Err("summarize: group by a column, or aggregate one".into());
    }
    // `by` and `values` name a sheet's columns (A is 1), not the area's:
    // one outside it grouped by empty values (DAP-24).
    let (c0, c1) = (area.col.max(1), area.last_col());
    for c in by.iter().copied().chain(values.iter().map(|a| a.col)) {
        if c < c0 || c > c1 {
            let range = format!("{}:{}", crate::book::cell_ref(area.row.max(1), c0), crate::book::cell_ref(area.last_row(), c1));
            let name = if c >= 1 { crate::book::col_name(c) } else { c.to_string() };
            return Err(format!("summarize: column {name} is outside {range} (by and values name the sheet's columns, A being 1, within the area)"));
        }
    }
    let (r0, r1) = (area.row, area.last_row());
    let m = b.engine.get_model();
    let ws = m.workbook.worksheet(sheet)?;
    let last = r1.min(ws.dimension().max_row);
    let first = r0 + header as i32;
    let name_of = |c: i32| -> String {
        if header {
            let n = m.shown(sheet, r0, c).unwrap_or_default();
            if !n.is_empty() {
                return n;
            }
        }
        crate::book::col_name(c)
    };
    struct Acc {
        sum: f64,
        n: usize,
        count: usize,
        min: f64,
        max: f64,
        distinct: std::collections::HashSet<String>,
    }
    let mut groups: Vec<(Vec<String>, Vec<Acc>)> = Vec::new();
    let mut index: HashMap<Vec<String>, usize> = HashMap::new();
    for r in first..=last {
        let key: Vec<String> = by.iter().map(|c| m.shown(sheet, r, *c).unwrap_or_default()).collect();
        let all_empty = key.iter().all(String::is_empty) && values.iter().all(|a| m.shown(sheet, r, a.col).unwrap_or_default().is_empty());
        if all_empty {
            continue;
        }
        let gi = *index.entry(key.clone()).or_insert_with(|| {
            groups.push((key.clone(), values.iter().map(|_| Acc { sum: 0.0, n: 0, count: 0, min: f64::INFINITY, max: f64::NEG_INFINITY, distinct: Default::default() }).collect()));
            groups.len() - 1
        });
        for (ai, a) in values.iter().enumerate() {
            let acc = &mut groups[gi].1[ai];
            let shown = m.shown(sheet, r, a.col).unwrap_or_default();
            if !shown.is_empty() {
                acc.count += 1;
                acc.distinct.insert(shown);
            }
            if let Ok(CellValue::Number(v)) = m.get_cell_value_by_index(sheet, r, a.col) {
                acc.sum += v;
                acc.n += 1;
                acc.min = acc.min.min(v);
                acc.max = acc.max.max(v);
            }
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    let mut head: Vec<String> = by.iter().map(|c| name_of(*c)).collect();
    for a in values {
        head.push(format!("{} of {}", a.agg, name_of(a.col)));
    }
    let src_name = ws.name.clone();
    b.engine.new_sheet()?;
    let n = b.engine.get_worksheets_properties().len() as u32 - 1;
    let wanted = format!("Summary of {src_name}");
    let mut name = wanted.clone();
    let mut i = 2;
    while b.engine.rename_sheet(n, &name).is_err() && i < 50 {
        name = format!("{wanted} {i}");
        i += 1;
    }
    let e = &mut b.engine;
    let key_input = |k: &str| -> String { if k.trim().parse::<f64>().is_ok() { k.to_string() } else { crate::book::as_text(k) } };
    let mut grid: Vec<Vec<String>> = vec![head.iter().map(|h| crate::book::as_text(h)).collect()];
    for (key, accs) in &groups {
        let mut line: Vec<String> = key.iter().map(|k| key_input(k)).collect();
        for (a, acc) in values.iter().zip(accs) {
            let v = match a.agg.as_str() {
                "sum" => Some(acc.sum),
                "count" => Some(acc.count as f64),
                "countd" => Some(acc.distinct.len() as f64),
                "avg" | "average" => (acc.n > 0).then(|| acc.sum / acc.n as f64),
                "min" => (acc.n > 0).then_some(acc.min),
                "max" => (acc.n > 0).then_some(acc.max),
                x => return Err(format!("aggregate {x:?}: sum, count, countd, avg, min or max")),
            };
            line.push(v.map(format_num).unwrap_or_default());
        }
        grid.push(line);
    }
    // A total row.
    let total_row = groups.len() as i32 + 2;
    if !groups.is_empty() && !values.is_empty() {
        let mut line = vec![String::new(); by.len().max(1)];
        line[0] = "Total".into();
        for (ai, a) in values.iter().enumerate() {
            let col = (by.len() + ai) as i32 + 1;
            let letter = crate::book::col_name(col);
            let f = match a.agg.as_str() {
                "sum" | "count" => format!("=SUM({letter}2:{letter}{})", total_row - 1),
                "min" => format!("=MIN({letter}2:{letter}{})", total_row - 1),
                "max" => format!("=MAX({letter}2:{letter}{})", total_row - 1),
                _ => String::new(),
            };
            if by.is_empty() && ai == 0 {
                continue;
            }
            line.push(f);
        }
        grid.push(line);
    }
    paste_values(e, n, 1, 1, &grid)?;
    let w = head.len() as i32;
    let _ = e.update_range_style(&Area { sheet: n, row: 1, column: 1, width: w, height: 1 }, "font.b", "true");
    let _ = e.update_range_style(&Area { sheet: n, row: total_row, column: 1, width: w, height: 1 }, "font.b", "true");
    let _ = e.set_frozen_rows_count(n, 1);
    let _ = e.set_columns_width(n, 1, w, 140.0);
    Ok(n)
}

pub fn format_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// The cells an op touched for sure, besides those a recalculation changed.
pub fn keys_of(b: &Book, s: &crate::book::Save) -> Vec<Key> {
    let mut v: Vec<Key> = s.cells.iter().map(|(k, _, _, _)| *k).collect();
    v.extend(s.deleted.iter().copied());
    let _ = b;
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::{new_engine, Book, Ext};

    fn book() -> Book {
        Book::new("t".into(), "new", new_engine("T"), Ext::default(), 0)
    }

    fn shown(b: &Book, s: u32, r: i32, c: i32) -> String {
        b.engine.shown(s, r, c).unwrap()
    }

    fn run(b: &mut Book, j: Value) -> Done {
        apply(b, serde_json::from_value(j).unwrap()).unwrap()
    }

    #[test]
    fn sorts_rows_with_their_formulas_and_undoes_in_one_step() {
        let mut b = book();
        run(&mut b, json!({"op": "set_many", "sheet": 0, "row": 1, "col": 1, "values": [["Name", "Qty", "Double"], ["pear", "3", "=B2*2"], ["apple", "10", "=B3*2"], ["fig", "", "=B4*2"], ["kiwi", "1", "=B5*2"]]}));
        run(&mut b, json!({"op": "sort", "sheet": 0, "area": {"row": 1, "col": 1, "width": 3, "height": 5}, "by": 2, "header": true}));
        let col = |b: &Book, c| (2..=5).map(|r| shown(b, 0, r, c)).collect::<Vec<_>>();
        assert_eq!(col(&b, 1), ["kiwi", "pear", "apple", "fig"]);
        assert_eq!(col(&b, 3), ["2", "6", "20", "0"]);
        assert_eq!(b.engine.get_cell_content(0, 2, 3).unwrap(), "=B2*2");
        assert_eq!(shown(&b, 0, 1, 1), "Name");
        run(&mut b, json!({"op": "sort", "sheet": 0, "area": {"row": 1, "col": 1, "width": 3, "height": 5}, "by": 1, "header": true, "desc": true}));
        assert_eq!(col(&b, 1), ["pear", "kiwi", "fig", "apple"]);
        run(&mut b, json!({"op": "undo"}));
        assert_eq!(col(&b, 1), ["kiwi", "pear", "apple", "fig"]);
    }

    #[test]
    fn filters_rows_and_shows_them_again() {
        let mut b = book();
        run(&mut b, json!({"op": "set_many", "sheet": 0, "row": 1, "col": 1, "values": [["City"], ["Lima"], ["Oslo"], ["Lima"], ["Rome"]]}));
        run(&mut b, json!({"op": "filter", "sheet": 0, "area": {"row": 1, "col": 1, "height": 5}, "criteria": [{"col": 1, "values": ["Lima"]}], "header": true}));
        let hidden = |b: &Book| (1..=5).map(|r| b.engine.get_model().is_row_hidden(0, r).unwrap()).collect::<Vec<_>>();
        assert_eq!(hidden(&b), [false, false, true, false, true]);
        assert_eq!(b.ext.filters.len(), 1, "the filter is kept with the book");
        assert_eq!(b.ext.filters[0].criteria[0].values, ["Lima"]);
        run(&mut b, json!({"op": "filter", "sheet": 0, "area": {"row": 1, "col": 1, "height": 5}, "header": true}));
        assert_eq!(hidden(&b), [false; 5]);
        assert_eq!(b.ext.filters.len(), 1, "a filter with no criteria is still on");
        run(&mut b, json!({"op": "filter", "sheet": 0, "area": {"row": 1, "col": 1, "height": 5}, "header": true, "off": true}));
        assert!(b.ext.filters.is_empty());
    }

    #[test]
    fn summarizes_into_a_new_sheet() {
        let mut b = book();
        run(&mut b, json!({"op": "set_many", "sheet": 0, "row": 1, "col": 1, "values": [["Region", "Sales"], ["North", "10"], ["South", "5"], ["North", "7.5"]]}));
        let d = run(&mut b, json!({"op": "summarize", "sheet": 0, "area": {"row": 1, "col": 1, "width": 2, "height": 4}, "by": [1], "values": [{"col": 2, "agg": "sum"}, {"col": 2, "agg": "count"}]}));
        let n = d.data.unwrap()["sheet"].as_u64().unwrap() as u32;
        assert_eq!(n, 1);
        assert_eq!(shown(&b, n, 1, 2), "sum of Sales");
        assert_eq!((shown(&b, n, 2, 1), shown(&b, n, 2, 2), shown(&b, n, 2, 3)), ("North".into(), "17.5".into(), "2".into()));
        assert_eq!(shown(&b, n, 3, 2), "5");
        assert_eq!(shown(&b, n, 4, 2), "22.5");
    }

    #[test]
    fn a_summary_refuses_a_column_outside_its_area() {
        let mut b = book();
        run(&mut b, json!({"op": "set_many", "sheet": 0, "row": 1, "col": 1, "values": [["Region", "Sales", "Other"], ["North", "10", "x"]]}));
        let sheets = b.engine.get_worksheets_properties().len();
        let area = json!({"row": 1, "col": 1, "width": 2, "height": 2});
        for (by, values) in [(json!([3]), json!([])), (json!([1]), json!([{"col": 3, "agg": "sum"}])), (json!([0]), json!([]))] {
            let op = serde_json::from_value(json!({"op": "summarize", "sheet": 0, "area": area, "by": by, "values": values})).unwrap();
            let e = apply(&mut b, op).err().expect("refused");
            assert!(e.contains("is outside A1:B2"), "{e}");
        }
        assert!(apply(&mut b, serde_json::from_value(json!({"op": "summarize", "sheet": 0, "area": area, "by": [3], "values": []})).unwrap()).err().expect("refused").contains("column C is outside A1:B2"));
        assert_eq!(b.engine.get_worksheets_properties().len(), sheets, "no sheet is added");
    }

    #[test]
    fn replaces_in_inputs() {
        assert_eq!(replaced("Hello hello", "hello", "bye", false, false).as_deref(), Some("bye bye"));
        assert_eq!(replaced("Hello hello", "hello", "bye", true, false).as_deref(), Some("Hello bye"));
        assert_eq!(replaced("Hello", "hello", "x", false, true).as_deref(), Some("x"));
        assert_eq!(replaced("Hello!", "hello", "x", false, true), None);
        let mut b = book();
        run(&mut b, json!({"op": "set_many", "sheet": 0, "row": 1, "col": 1, "values": [["red apple"], ["=\"red\"&\"!\""]]}));
        let d = run(&mut b, json!({"op": "replace", "find": "red", "replace": "green"}));
        assert_eq!(d.data.unwrap()["replaced"], 2);
        assert_eq!(shown(&b, 0, 1, 1), "green apple");
        assert_eq!(shown(&b, 0, 2, 1), "green!");
    }

    #[test]
    fn copies_and_pastes_with_formulas_moved() {
        let mut b = book();
        run(&mut b, json!({"op": "set_many", "sheet": 0, "row": 1, "col": 1, "values": [["1", "=A1*10"]]}));
        let clip = copy(&mut b.engine, 0, Rect { row: 1, col: 1, width: 2, height: 1 }).unwrap();
        run(&mut b, json!({"op": "paste", "sheet": 0, "to": {"row": 3, "col": 1}, "clipboard": clip}));
        assert_eq!(b.engine.get_cell_content(0, 3, 2).unwrap(), "=A3*10");
        assert_eq!(shown(&b, 0, 3, 2), "10");
        run(&mut b, json!({"op": "paste_text", "sheet": 0, "to": {"row": 5, "col": 1}, "text": "a\tb\n2\t=A6+1\n"}));
        assert_eq!(shown(&b, 0, 6, 2), "3");
    }

    #[test]
    fn rows_inserted_move_formulas_and_charts_go_with_their_sheet() {
        let mut b = book();
        run(&mut b, json!({"op": "set_many", "sheet": 0, "row": 1, "col": 1, "values": [["2"], ["3"], ["=SUM(A1:A2)"]]}));
        let d = run(&mut b, json!({"op": "insert_rows", "sheet": 0, "row": 2, "count": 2}));
        assert!(d.layout);
        assert_eq!(b.engine.get_cell_content(0, 5, 1).unwrap(), "=SUM(A1:A4)");
        run(&mut b, json!({"op": "add_sheet", "name": "Data"}));
        run(&mut b, json!({"op": "chart", "sheet": 1, "range": "A1:B3", "kind": "bar", "row": 1, "col": 4}));
        assert_eq!(b.ext.charts.len(), 1);
        run(&mut b, json!({"op": "delete_sheet", "sheet": 1}));
        assert!(b.ext.charts.is_empty());
    }
}


#[cfg(test)]
mod timing {
    use super::*;
    use crate::book::{new_engine, Book, Ext};

    /// How long a sort of 50,000 rows takes (`cargo test --profile ci sort_50k -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn sort_50k() {
        let mut b = Book::new("t".into(), "new", new_engine("T"), Ext::default(), 0);
        let rows: Vec<Vec<String>> = (0..50_000).map(|i| (0..7).map(|c| if c == 1 { format!("c{}", i % 997) } else { format!("{}", (i * 7919 + c * 13) % 10007) }).collect()).collect();
        let t = std::time::Instant::now();
        paste_values(&mut b.engine, 0, 2, 1, &rows).unwrap();
        eprintln!("fill {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let s = b.changes();
        eprintln!("changes {:?} ({} cells)", t.elapsed(), s.cells.len());
        b.saved(s);
        let t = std::time::Instant::now();
        sort(&mut b.engine, 0, Rect { row: 1, col: 1, width: 7, height: 50_001 }, 6, true, true).unwrap();
        eprintln!("sort {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let s = b.changes();
        eprintln!("changes {:?} ({} cells)", t.elapsed(), s.cells.len());
    }
}
