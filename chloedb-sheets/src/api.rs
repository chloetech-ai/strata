//! The spreadsheets' HTTP API. The command center reaches it through
//! chloedb-server (`--app sheets=URL`), which also loads the view from here
//! (`/app.json`, `/ui/view.js`); programs may call it directly with its
//! token.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::book::Ext;
use crate::books::{Books, Fail, QuerySpec};
use crate::caller::Caller;
use crate::ops::{Op, Rect};
use crate::{io, store};

pub const VIEW: &str = include_str!("../ui/view.js");

/// A grid of cells, for the navigation.
const ICON: &str = r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 9h18M3 14h18M9 4v16M15 4v16"/>"#;

/// Files an import takes at most.
pub const MAX_UPLOAD: usize = 256 << 20;

pub struct Service {
    pub books: Arc<Books>,
    pub token: Option<String>,
    /// Whether this process started the cluster it keeps books in (`--local`).
    pub local: bool,
}

type Shared = Arc<Service>;

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<(u16, String)> for ApiError {
    fn from((s, m): (u16, String)) -> ApiError {
        ApiError(StatusCode::from_u16(s).unwrap_or(StatusCode::FORBIDDEN), m)
    }
}

impl From<Fail> for ApiError {
    fn from(f: Fail) -> ApiError {
        ApiError(StatusCode::from_u16(f.status).unwrap_or(StatusCode::BAD_GATEWAY), f.message)
    }
}

fn bad(m: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, m.into())
}

type Answer = Result<Json<Value>, ApiError>;

pub fn router(svc: Shared) -> Router {
    Router::new()
        .route("/app.json", get(manifest))
        .route("/ui/view.js", get(view))
        .route("/v1/info", get(info))
        .route("/v1/books", get(list).post(create))
        .route("/v1/import", post(import))
        .route("/v1/sqlite/tables", post(sqlite_tables))
        .route("/v1/from-table", post(from_table))
        .route("/v1/search", get(search))
        .route("/v1/books/:id", get(one).delete(forget))
        .route("/v1/books/:id/ops", post(apply))
        .route("/v1/books/:id/cells", get(cells))
        .route("/v1/books/:id/layout", get(layout))
        .route("/v1/books/:id/changes", get(changes))
        .route("/v1/books/:id/copy", post(copy))
        .route("/v1/books/:id/values", get(values))
        .route("/v1/books/:id/find", get(find))
        .route("/v1/books/:id/export", get(export))
        .route("/v1/books/:id/query", post(query))
        .route("/v1/books/:id/refresh", post(refresh))
        .route("/v1/books/:id/queries/:qid", axum::routing::delete(forget_query))
        .route("/v1/books/:id/to-table", post(to_table))
        .route("/v1/books/:id/share", post(share))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
        .layer(axum::middleware::from_fn_with_state(svc.clone(), guard))
        .with_state(svc)
}

/// Requests come from chloedb-server (for the command center) or from a
/// program, never from a page in a browser: a request with an `Origin` is
/// refused (another site's page could otherwise change workbooks), and
/// with `--token`, a request without it.
async fn guard(State(svc): State<Shared>, req: Request, next: Next) -> Response {
    if req.headers().contains_key(header::ORIGIN) {
        return ApiError(StatusCode::FORBIDDEN, "chloedb-sheets takes no requests from browsers: open it in ChloeDB's command center (chloedb-server --app sheets=URL)".into()).into_response();
    }
    if let Some(t) = &svc.token {
        let given = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
        if !constant_eq(given.as_bytes(), t.as_bytes()) {
            return ApiError(StatusCode::UNAUTHORIZED, "chloedb-sheets needs its token (Authorization: Bearer ...)".into()).into_response();
        }
    }
    next.run(req).await
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn manifest() -> Json<Value> {
    Json(json!({
        "name": "sheets",
        "title": "Sheets",
        "description": "Spreadsheets kept in ChloeDB: formulas, charts and pivots, xlsx, CSV and SQLite in and out, and the cluster's tables as sheets",
        "icon_svg": ICON,
        "view": "ui/view.js",
        "version": env!("CARGO_PKG_VERSION"),
        // It refuses what x-chloedb-scope and x-chloedb-tables do not allow
        // ([`crate::caller`]): the server forwards it keys limited to tables.
        "checks_callers": true,
    }))
}

async fn view() -> Response {
    let body = match std::env::var_os("CHLOEDB_SHEETS_UI_DIR") {
        Some(d) => std::fs::read_to_string(std::path::Path::new(&d).join("view.js")).unwrap_or_else(|_| VIEW.into()),
        None => VIEW.into(),
    };
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

async fn info(State(svc): State<Shared>) -> Json<Value> {
    let health = svc.books.strata.health().await;
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "engine": crate::book::ENGINE,
        "strata": {"url": svc.books.strata.url, "role": health.as_ref().ok().and_then(|h| h["role"].as_str().map(str::to_string)), "error": health.err().map(|e| e.to_string())},
        "local": svc.local,
        "open": svc.books.open_count(),
        "tables": {"books": store::BOOKS, "cells": store::CELLS},
        "limits": {"upload_bytes": MAX_UPLOAD, "import_cells": io::MAX_IMPORT_CELLS, "query_rows": crate::books::MAX_QUERY_ROWS},
    }))
}

/// The workbooks the caller may open: `?whose=mine`, `shared` (others')
/// or `all` (the default).
async fn list(State(svc): State<Shared>, headers: header::HeaderMap, Query(q): Query<HashMap<String, String>>) -> Answer {
    svc.books.ready().await?;
    let who = Caller::of(&headers);
    let whose = q.get("whose").map(String::as_str).unwrap_or("all");
    let books: Vec<Value> = store::list(&svc.books.strata).await.map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e))?.into_iter().filter(|(_, e)| who.lists(e, whose)).map(|(b, _)| b).collect();
    Ok(Json(json!({"books": books, "whose": whose, "me": who.name})))
}

/// A workbook the caller may open (403 else), and its extension.
async fn seen(svc: &Service, who: &Caller, id: &str) -> Result<(), ApiError> {
    if who.direct() {
        return Ok(());
    }
    let g = svc.books.open(id).await?;
    who.may_see(&g.as_ref().expect("open").ext)?;
    Ok(())
}

/// A workbook the caller may change (403 else).
async fn changed(svc: &Service, who: &Caller, id: &str) -> Result<(), ApiError> {
    if who.direct() {
        return Ok(());
    }
    let g = svc.books.open(id).await?;
    let ext = &g.as_ref().expect("open").ext;
    who.may_see(ext)?;
    who.may_change(ext)?;
    Ok(())
}

/// The caller's own workbook: its owner.
fn owned(who: &Caller) -> Ext {
    Ext { owner: who.owner(), ..Ext::default() }
}

#[derive(Deserialize)]
struct Share {
    shared: bool,
}

/// Shares a workbook (keys limited to tables open it too), or not: its
/// owner's or an admin's to say.
async fn share(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Json(s): Json<Share>) -> Answer {
    let who = Caller::of(&headers);
    changed(&svc, &who, &id).await?;
    let mut g = svc.books.open(&id).await?;
    let b = g.as_mut().expect("open");
    b.ext.shared = s.shared;
    let cells = svc.books.save(b, false).await;
    let _ = cells;
    Ok(Json(json!({"book": b.summary()})))
}

#[derive(Deserialize)]
struct NewBook {
    #[serde(default)]
    name: Option<String>,
}

async fn create(State(svc): State<Shared>, headers: header::HeaderMap, Json(b): Json<NewBook>) -> Answer {
    let who = Caller::of(&headers);
    who.may_make()?;
    let name = b.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Untitled".into());
    let e = crate::book::new_engine(&name);
    Ok(Json(svc.books.create(e, "new", owned(&who)).await?))
}

#[derive(Deserialize)]
struct ImportQuery {
    #[serde(default)]
    name: Option<String>,
    /// xlsx, csv, tsv or sqlite (by default, what the bytes are).
    #[serde(default)]
    format: Option<String>,
    /// Of a SQLite file, the tables to bring (comma-separated).
    #[serde(default)]
    tables: Option<String>,
    /// Of a CSV file, whether its first row names the columns.
    #[serde(default)]
    header: Option<bool>,
}

/// A file as a new workbook: the body is its bytes.
async fn import(State(svc): State<Shared>, headers: header::HeaderMap, Query(q): Query<ImportQuery>, body: Bytes) -> Answer {
    let who = Caller::of(&headers);
    who.may_make()?;
    if body.is_empty() {
        return Err(bad("send the file as the body"));
    }
    let name = q.name.clone().unwrap_or_else(|| "Imported".into());
    let format = q.format.clone().filter(|f| !f.is_empty() && f != "auto").unwrap_or_else(|| io::sniff(&body, &name).to_string());
    let stem = name.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or_else(|| name.clone());
    let tables: Option<Vec<String>> = q.tables.as_deref().map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
    let (engine, report) = tokio::task::block_in_place(|| match format.as_str() {
        "xlsx" => io::from_xlsx(&body, &stem).map(|e| (e, json!({"format": "xlsx"}))),
        "csv" | "tsv" | "txt" => io::from_csv(&body, &name, q.header),
        "sqlite" | "db" => io::from_sqlite(&body, &name, tables.as_deref()),
        f => Err(format!("format {f:?}: xlsx, csv, tsv or sqlite")),
    })
    .map_err(bad)?;
    let mut s = svc.books.create(engine, &format!("{format}:{name}"), owned(&who)).await?;
    s["import"] = report;
    Ok(Json(s))
}

/// The tables of a SQLite file (the body), to choose what to bring.
async fn sqlite_tables(body: Bytes) -> Answer {
    let t = tokio::task::block_in_place(|| {
        let (conn, _f) = io::open_sqlite(&body)?;
        io::sqlite_tables(&conn)
    })
    .map_err(bad)?;
    Ok(Json(json!({"tables": t})))
}

#[derive(Deserialize)]
struct FromTable {
    #[serde(default)]
    table: Option<String>,
    #[serde(default)]
    sql: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn from_table(State(svc): State<Shared>, headers: header::HeaderMap, Json(b): Json<FromTable>) -> Answer {
    let who = Caller::of(&headers);
    who.may_make()?;
    let run = svc.books.strata.as_caller(who.as_tables());
    Ok(Json(svc.books.from_table(b.table.as_deref(), b.sql.as_deref(), b.name.as_deref(), b.limit, &run, owned(&who)).await?))
}

async fn search(State(svc): State<Shared>, headers: header::HeaderMap, Query(q): Query<HashMap<String, String>>) -> Answer {
    svc.books.ready().await?;
    let limit = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(50);
    let mut j = store::search(&svc.books.strata, q.get("q").map(String::as_str).unwrap_or(""), limit).await.map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e))?;
    // Only the workbooks the caller may open (each answer's rows begin
    // with the workbook's id).
    let who = Caller::of(&headers);
    if !who.direct() {
        let open: std::collections::HashSet<String> = store::list(&svc.books.strata).await.map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e))?.into_iter().filter(|(_, e)| who.sees(e)).filter_map(|(b, _)| b["id"].as_str().map(str::to_string)).collect();
        for k in ["cells", "books"] {
            if let Some(rows) = j[k]["rows"].as_array_mut() {
                rows.retain(|r| r[0].as_str().is_some_and(|id| open.contains(id)));
            }
        }
    }
    Ok(Json(j))
}

async fn one(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>) -> Answer {
    seen(&svc, &Caller::of(&headers), &id).await?;
    let g = svc.books.open(&id).await?;
    Ok(Json(g.as_ref().expect("open").summary()))
}

async fn forget(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>) -> Answer {
    changed(&svc, &Caller::of(&headers), &id).await?;
    svc.books.delete(&id).await?;
    Ok(Json(json!({"deleted": id})))
}

#[derive(Deserialize)]
struct Ops {
    ops: Vec<Value>,
}

async fn apply(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Json(b): Json<Ops>) -> Answer {
    changed(&svc, &Caller::of(&headers), &id).await?;
    let mut list = Vec::with_capacity(b.ops.len());
    for (i, o) in b.ops.into_iter().enumerate() {
        let op: Op = serde_json::from_value(o).map_err(|e| bad(format!("operation {i}: {e}")))?;
        list.push(op);
    }
    Ok(Json(svc.books.apply(&id, list).await?))
}

fn num<T: std::str::FromStr>(q: &HashMap<String, String>, k: &str, d: T) -> T {
    q.get(k).and_then(|v| v.parse().ok()).unwrap_or(d)
}

async fn cells(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Answer {
    seen(&svc, &Caller::of(&headers), &id).await?;
    let g = svc.books.open(&id).await?;
    let b = g.as_ref().expect("open");
    let (r0, c0) = (num(&q, "r0", 1), num(&q, "c0", 1));
    // At most 200,000 cells an answer.
    let r1 = num(&q, "r1", r0 + 100).min(r0 + 20_000);
    let c1 = num(&q, "c1", c0 + 30).min(c0 + 1_000);
    let mut j = b.cells(num(&q, "sheet", 0), r0, r1, c0, c1).map_err(bad)?;
    j["version"] = json!(b.version);
    Ok(Json(j))
}

async fn layout(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Answer {
    seen(&svc, &Caller::of(&headers), &id).await?;
    let g = svc.books.open(&id).await?;
    let b = g.as_ref().expect("open");
    let mut j = b.layout(num(&q, "sheet", 0)).map_err(bad)?;
    j["version"] = json!(b.version);
    Ok(Json(j))
}

async fn changes(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Answer {
    seen(&svc, &Caller::of(&headers), &id).await?;
    Ok(Json(svc.books.changes(&id, num(&q, "since", 0)).await?))
}

#[derive(Deserialize)]
struct CopyReq {
    sheet: u32,
    area: Rect,
}

/// The clipboard of an area (to paste with its formulas and styles), and
/// its values as tab-separated text (for other programs).
async fn copy(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Json(c): Json<CopyReq>) -> Answer {
    seen(&svc, &Caller::of(&headers), &id).await?;
    let mut g = svc.books.open(&id).await?;
    let b = g.as_mut().expect("open");
    let clip = crate::ops::copy(&mut b.engine, c.sheet, c.area).map_err(bad)?;
    let vals = b.values(c.sheet, c.area.row, c.area.col, c.area.row + c.area.height.max(1) - 1, c.area.col + c.area.width.max(1) - 1);
    let text = vals.iter().map(|r| r.iter().map(|(s, _)| s.replace(['\t', '\n'], " ")).collect::<Vec<_>>().join("\t")).collect::<Vec<_>>().join("\n");
    Ok(Json(json!({"clipboard": clip, "text": text})))
}

/// The values of a range (A1:D20 or a defined name), as numbers where
/// they are: for charts.
async fn values(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Answer {
    seen(&svc, &Caller::of(&headers), &id).await?;
    let g = svc.books.open(&id).await?;
    let b = g.as_ref().expect("open");
    let sheet = num(&q, "sheet", 0);
    let range = q.get("range").map(String::as_str).unwrap_or("A1");
    let (r0, c0, r1, c1) = parse_range(range).ok_or_else(|| bad(format!("range {range:?}: as A1:D20")))?;
    let (r1, c1) = (r1.min(r0 + 5_000), c1.min(c0 + 200));
    let vals = b.values(sheet, r0, c0, r1, c1);
    let rows: Vec<Value> = vals.into_iter().map(|r| Value::Array(r.into_iter().map(|(s, n)| json!([s, n])).collect())).collect();
    Ok(Json(json!({"sheet": sheet, "range": [r0, c0, r1, c1], "rows": rows})))
}

/// The cells that hold a text: `?q=&sheet=&case=1&whole=1&formulas=1`.
async fn find(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Answer {
    seen(&svc, &Caller::of(&headers), &id).await?;
    let g = svc.books.open(&id).await?;
    let b = g.as_ref().expect("open");
    let text = q.get("q").map(String::as_str).unwrap_or("");
    if text.is_empty() {
        return Ok(Json(json!({"matches": []})));
    }
    let flag = |k: &str| q.get(k).is_some_and(|v| v == "1" || v == "true");
    let sheet = q.get("sheet").and_then(|s| s.parse().ok());
    let m = tokio::task::block_in_place(|| b.find(text, sheet, flag("case"), flag("whole"), flag("formulas"), 2_000));
    Ok(Json(json!({"matches": m})))
}

/// `A1:D20` (or one cell) as rows and columns.
pub fn parse_range(s: &str) -> Option<(i32, i32, i32, i32)> {
    let s = s.rsplit('!').next()?.replace('$', "");
    let mut parts = s.split(':');
    let a = parse_cell(parts.next()?)?;
    let b = match parts.next() {
        Some(p) => parse_cell(p)?,
        None => a,
    };
    Some((a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)))
}

fn parse_cell(s: &str) -> Option<(i32, i32)> {
    let s = s.trim().to_ascii_uppercase();
    let letters: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let digits = &s[letters.len()..];
    if letters.is_empty() || digits.is_empty() {
        return None;
    }
    let col = letters.bytes().fold(0i32, |a, b| a * 26 + (b - b'A' + 1) as i32);
    let row: i32 = digits.parse().ok()?;
    (row >= 1 && col >= 1).then_some((row, col))
}

/// The workbook as a file: xlsx, or a sheet as CSV.
async fn export(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Query(q): Query<HashMap<String, String>>) -> Result<Response, ApiError> {
    seen(&svc, &Caller::of(&headers), &id).await?;
    let g = svc.books.open(&id).await?;
    let b = g.as_ref().expect("open");
    let name: String = b.name().chars().map(|c| if c.is_ascii_alphanumeric() || " -_.".contains(c) { c } else { '_' }).collect();
    let format = q.get("format").map(String::as_str).unwrap_or("xlsx");
    let (bytes, ctype, ext) = match format {
        "xlsx" => (tokio::task::block_in_place(|| io::to_xlsx(b.model())).map_err(bad)?, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "xlsx"),
        "csv" => (io::to_csv(b.model(), num(&q, "sheet", 0)).map_err(bad)?.into_bytes(), "text/csv; charset=utf-8", "csv"),
        f => return Err(bad(format!("format {f:?}: xlsx or csv"))),
    };
    let disp = format!("attachment; filename=\"{}.{ext}\"", name.trim());
    let mut r = ([(header::CONTENT_TYPE, ctype.to_string()), (header::CONTENT_DISPOSITION, disp)], bytes).into_response();
    // IronCalc writes no charts into an xlsx: the answer says so, and the
    // view tells whoever downloads it.
    let charts = b.ext.charts.len();
    if format == "xlsx" && charts > 0 {
        let v = format!("{charts} chart{}", if charts == 1 { "" } else { "s" });
        if let Ok(v) = header::HeaderValue::from_str(&v) {
            r.headers_mut().insert("x-app-left-out", v);
        }
    }
    Ok(r)
}

async fn query(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Json(spec): Json<QuerySpec>) -> Answer {
    let who = Caller::of(&headers);
    changed(&svc, &who, &id).await?;
    Ok(Json(svc.books.query(&id, spec, &svc.books.strata.as_caller(who.as_tables())).await?))
}

async fn refresh(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>) -> Answer {
    let who = Caller::of(&headers);
    changed(&svc, &who, &id).await?;
    Ok(Json(svc.books.refresh_all(&id, &svc.books.strata.as_caller(who.as_tables())).await?))
}

async fn forget_query(State(svc): State<Shared>, headers: header::HeaderMap, Path((id, qid)): Path<(String, String)>) -> Answer {
    changed(&svc, &Caller::of(&headers), &id).await?;
    let mut g = svc.books.open(&id).await?;
    let b = g.as_mut().expect("open");
    Books::forget_query(b, &qid);
    let cells = svc.books.save(b, false).await;
    let _ = cells;
    Ok(Json(json!({"forgot": qid, "book": b.summary()})))
}

#[derive(Deserialize)]
struct ToTable {
    sheet: u32,
    table: String,
    #[serde(default)]
    key: Option<String>,
}

async fn to_table(State(svc): State<Shared>, headers: header::HeaderMap, Path(id): Path<String>, Json(t): Json<ToTable>) -> Answer {
    let who = Caller::of(&headers);
    seen(&svc, &who, &id).await?;
    Ok(Json(svc.books.to_table(&id, t.sheet, &t.table, t.key.as_deref(), &svc.books.strata.as_caller(who.as_tables())).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("A1:D20"), Some((1, 1, 20, 4)));
        assert_eq!(parse_range("Sheet1!$B$2"), Some((2, 2, 2, 2)));
        assert_eq!(parse_range("D20:A1"), Some((1, 1, 20, 4)));
        assert_eq!(parse_range("AA10:AB11"), Some((10, 27, 11, 28)));
        assert_eq!(parse_range("nope"), None);
    }

    /// The grid's scroller (`.sx-scroll`) is absolutely positioned over its
    /// pane; any other element given that class covers whatever is under it,
    /// as the home page's tables card did (DAP-1: nothing on the home page
    /// could be clicked). Only the grid may carry it.
    #[test]
    fn grid_scroller_class_is_the_grids_only() {
        let uses = VIEW.matches("class: \"sx-scroll\"").count() + VIEW.matches("class: \"sx-scroll ").count();
        assert_eq!(uses, 1, "the absolutely positioned .sx-scroll is used by something other than the grid");
        // and no second rule for the class, which would apply to the grid too
        let rules = VIEW.lines().filter(|l| l.trim_start().starts_with(".sx-scroll {")).count();
        assert_eq!(rules, 1, "a second .sx-scroll rule applies to the grid as well");
    }
}
