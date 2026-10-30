//! The HTTP JSON API, Prometheus metrics and the query UI.
//!
//! | route | body | answer |
//! |---|---|---|
//! | `GET /` | | the UI |
//! | `GET /livez`, `/readyz` | | `{status}`: 503 when a restart is needed |
//! | `GET /v1/health`, `/v1/stats`, `/v1/config` | | JSON (`/v1/health`: 503 when "down" or "failing") |
//! | `GET /v1/schema?table=name` | | a table's schema (the first table's without `table`) |
//! | `GET /v1/tables` | | `{tables: [...]}`: every table's schema and rank profiles, the first first |
//! | `GET /metrics` | | Prometheus text |
//! | `POST /v1/query` | `{sql, params?, timeout_ms?, shard?, stats?, snapshot?, min_seq_no?, min_seq_nos?, log?}` | rows; with `log` (`true` or `{table?, request?, context?, variant?, candidates?, ts?}`) also `request` and `logged` ([`strata::rank_log`]) |
//! | `POST /v1/sql` | `{sql, params?, timeout_ms?, min_seq_no?, min_seq_nos?, log?}` | rows, or `{written, seq_no?, seq_nos?}` |
//! | `POST /v1/rank_log/export` | `{table?, since?, until?, events?, history?, window?, grades?}` | NDJSON: one record per logged row ([`strata::rank_log::export`]) |
//! | `POST /v1/write` | `{ops: [...]}` (see `proto/strata.proto`; each op writes the first table, or its `table`) | `{written, seq_no?, seq_nos?}` |
//! | `POST /v1/plan` | `{plan, timeout_ms?, min_seq_no?, min_seq_nos?}` (a plan as `/v1/explain` gives it) | rows, or `{written, seq_no?, seq_nos?}` |
//! | `POST /v1/import?format=jsonl&table=name` | rows as JSON Lines, JSON, CSV or TSV (of the first table without `table`) | `{written, skipped_fields}` |
//! | `POST /v1/import?format=sqlite&tables=a,b&plan=true` | a SQLite file (`application/vnd.sqlite3`; recognised by its first bytes too) | `{tables, skipped, written}`, or with `plan` the plan ([`strata::sqlite`]) |
//! | `GET /v1/export?tables=a,b&fts=false&indexes=false&schemas=false` | | the database as a SQLite file |
//! | `POST /v1/schema/check` | a schema, in either JSON form | `{schema, short}`, or an error |
//! | `POST /v1/explain` | `{sql, params?, hits?}` | the bound query, its plan IR and the executor's plan; with `hits`, the top rows with their `score()` and `features()` |
//! | `POST /v1/embed` | `{text, field?, table?}` | `{table, field, vector}`: the text's vector by an embedded field's model |
//! | `GET /v1/synonyms`, `PUT /v1/synonyms` | `{text}` (Solr format) | the synonym list |
//! | `POST /v1/term_stats` | `{sql, params?, timeout_ms?, min_seq_no?, min_seq_nos?}` | `{stats, snapshot}` |
//! | `POST /v1/snapshot` | | `{snapshot, txn, ttl_s}` |
//! | `POST /v1/agent` | `{question, model?, effort?, api_key?, max_steps?}` | an answer and the steps taken |
//!
//! Every answer carries the request's id in `x-request-id`: the one the
//! client sent, if it is 1 to 64 letters, digits, '-', '_', '.' or ':', or
//! a new one ([`crate::obs`]).
//!
//! With a token set, every route but `/`, `/livez`, `/readyz` and
//! `/v1/health` needs
//! `Authorization: Bearer <token>`. Every route checks the `Host` header
//! and, on a request that can change something, the `Origin` header
//! ([`crate::auth`]).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use futures::StreamExt;
use axum::extract::{DefaultBodyLimit, MatchedPath, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json as AxJson, Router};
use serde_json::{json, Value as Json};
use tracing::Instrument;

use strata::ingest::Format;
use strata::storage::Layout;

use crate::auth::{self, Access, Secret};
use crate::replication::Acks;
use crate::service::{has_source, After, QueryReq, Service, SqlAnswer, Stream, Written, STREAM_OPS};
use crate::{agent, json as js, obs, ApiError};

const UI: &str = include_str!("../ui/index.html");

/// Why a request failed on the server's side, for [`observe`] to log.
#[derive(Clone)]
struct Failure(Arc<str>);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let failure = (self.status >= 500).then(|| Failure(self.message.as_str().into()));
        let mut r = (code, AxJson(json!({"error": self.message}))).into_response();
        // Refused for the server's load: when to try again.
        if self.status == 429 {
            r.headers_mut().insert(header::RETRY_AFTER, header::HeaderValue::from(crate::service::RETRY_AFTER.as_secs()));
        }
        if let Some(f) = failure {
            r.extensions_mut().insert(f);
        }
        r
    }
}

type Res = Result<AxJson<Json>, ApiError>;

#[derive(Clone)]
struct App {
    svc: Arc<Service>,
    token: Option<Secret>,
    hosts: Option<Arc<[String]>>,
}

pub fn router(svc: Arc<Service>, access: &Access) -> Router {
    // Rows come in writes and imports; every other body is small.
    let rows = DefaultBodyLimit::max(svc.max_body);
    let app = App { svc, token: access.token.clone(), hosts: access.hosts.clone().map(Into::into) };
    let api = Router::new()
        .route("/v1/stats", get(stats))
        .route("/v1/schema", get(schema))
        .route("/v1/tables", get(tables))
        .route("/v1/config", get(config))
        .route("/metrics", get(metrics))
        .route("/v1/query", post(query))
        .route("/v1/sql", post(sql))
        .route("/v1/write", post(write).layer(rows.clone()))
        .route("/v1/plan", post(run_plan).layer(rows.clone()))
        .route("/v1/import", post(import).layer(rows))
        .route("/v1/export", get(export))
        // A stream's body has no end set.
        .route("/v1/stream", post(stream).layer(DefaultBodyLimit::disable()))
        .route("/v1/sources", get(sources))
        .route("/v1/sources/*name", get(source))
        .route("/v1/schema/check", post(schema_check))
        .route("/v1/explain", post(explain))
        .route("/v1/embed", post(embed))
        .route("/v1/synonyms", get(synonyms).merge(put(put_synonyms)))
        .route("/v1/term_stats", post(term_stats))
        .route("/v1/sid/next", post(sid_next))
        .route("/v1/rank_log/export", post(export_rank_log))
        .route("/v1/snapshot", post(snapshot))
        .route("/v1/admin/checkpoint", post(admin_checkpoint))
        .route("/v1/admin/check", post(admin_check))
        .route("/v1/admin/vacuum", post(admin_vacuum))
        .route("/v1/admin/verify", post(admin_verify))
        .route("/v1/admin/promote", post(admin_promote))
        .route("/v1/agent", post(agent_run))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .route("/", get(|| async { Html(UI) }))
        .route("/v1/health", get(health))
        .route("/livez", get(live))
        .route("/readyz", get(live))
        .merge(api)
        .layer(middleware::from_fn_with_state(app.clone(), browsers))
        .layer(DefaultBodyLimit::max(crate::service::SMALL_BODY))
        .layer(middleware::from_fn_with_state(app.clone(), observe))
        .with_state(app)
}

/// Gives the request its id (answered in `x-request-id`), serves it in a
/// span that names it, counts it by route and status, and logs it: at
/// debug level, or as an error when it failed on the server's side.
async fn observe(State(app): State<App>, req: Request, next: Next) -> Response {
    let id = obs::request_id(header_str(req.headers(), header::HeaderName::from_static(obs::HEADER)));
    let route = req.extensions().get::<MatchedPath>().map_or("other", |p| p.as_str()).to_string();
    let span = tracing::info_span!("request", id = %id, method = %req.method(), route = %route);
    let t0 = Instant::now();
    let mut res = obs::scope(Some(id.clone()), next.run(req)).instrument(span.clone()).await;
    let secs = t0.elapsed().as_secs_f64();
    let status = res.status().as_u16();
    app.svc.metrics.http(&route, status, secs);
    if let Ok(v) = header::HeaderValue::from_str(&id) {
        res.headers_mut().insert(obs::HEADER, v);
    }
    let _in = span.enter();
    match res.extensions().get::<Failure>() {
        Some(Failure(why)) => obs::server_error(status, why),
        None => tracing::debug!(status, secs, "answered"),
    }
    res
}

fn header_str<'a>(headers: &'a HeaderMap, name: header::HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

async fn auth(State(app): State<App>, headers: HeaderMap, req: Request, next: Next) -> Response {
    if let Some(token) = &app.token {
        if !token.admits(header_str(&headers, header::AUTHORIZATION)) {
            return ApiError { status: 401, message: "missing or wrong token".into() }.into_response();
        }
    }
    next.run(req).await
}

/// Keeps out pages on other sites: one that reached this server through
/// DNS rebinding (a `Host` it does not answer to), and one that posts to
/// it (an `Origin` other than its own), which a browser lets any page do
/// without a token.
async fn browsers(State(app): State<App>, headers: HeaderMap, req: Request, next: Next) -> Response {
    let safe = req.method().is_safe();
    let host = header_str(&headers, header::HOST).map(str::to_string).or_else(|| req.uri().authority().map(|a| a.to_string()));
    if let Some(why) = auth::browser_check(app.hosts.as_deref(), host.as_deref(), header_str(&headers, header::ORIGIN), safe) {
        return ApiError { status: 403, message: why }.into_response();
    }
    next.run(req).await
}

fn body_str<'a>(b: &'a Json, k: &str) -> Result<&'a str, ApiError> {
    b.get(k).and_then(|v| v.as_str()).ok_or_else(|| ApiError::bad(format!("the body needs a string {k:?}")))
}

fn body_u64(b: &Json, k: &str) -> u64 {
    b.get(k).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// The writes a read must see ([`After`]): the body's `min_seq_no`, the
/// sequence number a server's write answered, or `min_seq_nos`, the
/// numbers a coordinator's answered, one per shard.
fn after(b: &Json) -> Result<After, ApiError> {
    let seq = |v: &Json, what: &str| v.as_u64().ok_or_else(|| ApiError::bad(format!("{what} is a sequence number: a whole number from 0")));
    let min = match b.get("min_seq_no") {
        None | Some(Json::Null) => 0,
        Some(v) => seq(v, "min_seq_no")?,
    };
    let mins = match b.get("min_seq_nos") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(a)) => a.iter().map(|v| seq(v, "each of min_seq_nos")).collect::<Result<_, _>>()?,
        Some(_) => return Err(ApiError::bad("min_seq_nos is a list of sequence numbers, one per shard, as a coordinator's write answers seq_nos")),
    };
    After::of(min, mins)
}

async fn health(State(app): State<App>) -> Response {
    let h = app.svc.health().await;
    let code = if matches!(h["status"].as_str(), Some("down" | "failing")) { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    (code, AxJson(h)).into_response()
}

/// Liveness and readiness: 503 when the server cannot write until it is
/// restarted ([`Service::failing`]). Neither asks a coordinator's shards,
/// so a slow shard cannot take every coordinator out of service at once;
/// `/v1/health` shows them.
async fn live(State(app): State<App>) -> Response {
    match app.svc.failing() {
        Some(why) => (StatusCode::SERVICE_UNAVAILABLE, AxJson(json!({"status": "failing", "error": why}))).into_response(),
        None => AxJson(json!({"status": "ok"})).into_response(),
    }
}

async fn stats(State(app): State<App>) -> AxJson<Json> {
    AxJson(app.svc.stats().await)
}

/// A table's schema: `?table=`, else the first table's.
async fn schema(State(app): State<App>, Query(q): Query<std::collections::HashMap<String, String>>) -> Res {
    let s = match q.get("table") {
        Some(t) => app.svc.table(t).await?,
        None => Arc::new(app.svc.schema.clone()),
    };
    Ok(AxJson(table_json(&app.svc, &s)))
}

/// Every table's schema, the first first.
async fn tables(State(app): State<App>) -> AxJson<Json> {
    let tables: Vec<Json> = app.svc.catalog().iter().map(|s| table_json(&app.svc, s)).collect();
    AxJson(json!({"tables": tables}))
}

fn table_json(svc: &crate::service::Service, s: &strata::schema::Schema) -> Json {
    json!({
        "table": s.table,
        "schema": s,
        "short": s.to_short_json(),
        "primary_key": s.primary_key.map(|k| s.fields[k].name.clone()),
        "shard_by": s.shard_by,
        // Its rank profiles, by name.
        "profiles": svc.rank_profiles().into_iter().filter(|(t, _)| t.eq_ignore_ascii_case(&s.table)).map(|(_, p)| p).collect::<Vec<_>>(),
        // Each embedded VECTOR field: its sources, model, and whether this
        // server can embed text for it.
        "embeddings": svc.models().iter().filter(|m| m.table.eq_ignore_ascii_case(&s.table)).map(|m| json!({
            "field": m.field, "from": m.spec.from, "model": m.spec.model,
            "loaded": m.model.is_ok(), "error": m.model.as_ref().err(),
        })).collect::<Vec<_>>(),
        // Each derived TEXT field: its MEDIA field, model, and whether this
        // server makes its text (null on a coordinator: its shards do).
        "derived": s.fields.iter().filter_map(|f| f.derive.as_ref().map(|d| json!({
            "field": f.name, "from": d.from, "model": d.model,
            "loaded": svc.local_db().map(|db| db.deriver_in(&s.table, &f.name).is_some()),
        }))).collect::<Vec<_>>(),
    })
}

async fn config(State(app): State<App>) -> AxJson<Json> {
    let a = &app.svc.agent;
    let rules = app.svc.synonyms().map_or(0, |s| s.rules());
    AxJson(json!({"agent": {"model": a.model, "server_key": a.api_key.is_some(), "models": agent::MODELS}, "synonyms": {"rules": rules, "editable": app.svc.local_db().is_some()}}))
}

async fn metrics(State(app): State<App>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], app.svc.prometheus().await)
}

async fn query(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let stats = match b.get("stats") {
        None | Some(Json::Null) => None,
        Some(s) => Some(serde_json::from_value(s.clone()).map_err(|e| ApiError::bad(format!("bad stats: {e}")))?),
    };
    let req = QueryReq {
        sql: body_str(&b, "sql")?.to_string(),
        params: app.svc.params(b.get("params")).await?,
        timeout_ms: body_u64(&b, "timeout_ms"),
        shard: b.get("shard").and_then(|v| v.as_bool()).unwrap_or(false),
        stats,
        snapshot: body_u64(&b, "snapshot"),
        partial: b.get("partial").and_then(|v| v.as_bool()),
        after: after(&b)?,
        ..Default::default()
    };
    if let Some(log) = b.get("log") {
        if req.shard {
            return Err(ApiError::bad("a shard's query is not logged: log through the coordinator"));
        }
        return logged(&app, req, log).await;
    }
    let a = app.svc.query(req).await?;
    Ok(AxJson(answer(&a)))
}

/// A query run and logged ([`Service::query_logged`]): its answer, with
/// the request's id (`"request"`) and the log's write (`"logged"`: the
/// rows, and the write's sequence numbers as a write's answer gives them).
async fn logged(app: &App, req: QueryReq, log: &Json) -> Res {
    let log = strata::rank_log::Log::from_json(log)?;
    let (a, request, w) = app.svc.query_logged(req, log).await?;
    let mut j = answer(&a);
    j["request"] = json!(request);
    let mut l = json!({"rows": w.rows});
    if let Some(s) = w.seq_no {
        l["seq_no"] = json!(s);
    }
    if !w.seq_nos.is_empty() {
        l["seq_nos"] = json!(w.seq_nos);
    }
    j["logged"] = l;
    Ok(AxJson(j))
}

/// The records of a log table ([`strata::rank_log::export`]) as NDJSON,
/// read from one snapshot of a local database as they are sent. A failure
/// after the first record ends the answer with `{"error": ..., "status":
/// ..}`. A coordinator refuses it: each shard's server exports its own
/// rows, which hold every row of a context when the log and the events
/// are placed by it (`shard_by`).
async fn export_rank_log(State(app): State<App>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let x = strata::rank_log::Export::from_json(&b)?;
    let db = app.svc.local_db().cloned().ok_or_else(|| ApiError::bad("a coordinator does not export a rank log: export from each shard's server"))?;
    // Refused before the answer starts.
    strata::rank_log::check(&*app.svc.table(&x.table).await?)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Json>(1024);
    tokio::task::spawn_blocking(move || {
        let r = db.export_rank_log(&x, |r| tx.blocking_send(r).map_err(|_| strata::Error::Query("the export's client left".into())));
        if let Err(e) = r {
            let e = ApiError::from(e);
            let _ = tx.blocking_send(json!({"error": e.message, "status": e.status}));
        }
    });
    let lines = tokio_stream::wrappers::ReceiverStream::new(rx).map(|j| Ok::<_, std::convert::Infallible>(Bytes::from(format!("{j}\n"))));
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], axum::body::Body::from_stream(lines)).into_response())
}

/// A query's answer; one a coordinator gave without some shards says
/// which (`"partial": {"missing_shards": [...]}`).
fn answer(a: &crate::service::Answer) -> Json {
    let mut j = js::result(&a.rs, a.took_ms);
    if !a.missing.is_empty() {
        j["partial"] = json!({"missing_shards": a.missing});
    }
    j
}

async fn sql(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let params = app.svc.params(b.get("params")).await?;
    if let Some(log) = b.get("log") {
        let req = QueryReq { sql: body_str(&b, "sql")?.to_string(), params, timeout_ms: body_u64(&b, "timeout_ms"), after: after(&b)?, ..Default::default() };
        return logged(&app, req, log).await;
    }
    match app.svc.sql(body_str(&b, "sql")?, &params, after(&b)?, body_u64(&b, "timeout_ms")).await? {
        SqlAnswer::Rows(a) => Ok(AxJson(answer(&a))),
        SqlAnswer::Written { w, took_ms } => Ok(AxJson(written(w, took_ms))),
    }
}

/// A write's answer: the rows written, and the transaction's sequence
/// number in a database that numbers its writes, or through a
/// coordinator each shard's ([`Written`]).
fn written(w: Written, took_ms: f64) -> Json {
    let mut j = json!({"written": w.rows, "took_ms": took_ms});
    if w.skipped > 0 {
        j["skipped"] = json!(w.skipped);
    }
    if let Some(s) = w.seq_no {
        j["seq_no"] = json!(s);
    }
    if !w.seq_nos.is_empty() {
        j["seq_nos"] = json!(w.seq_nos);
    }
    j
}

/// Runs a plan ([`strata::query::ir`]) as `/v1/sql` runs a statement.
async fn run_plan(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let plan = strata::query::ir::Plan::from_json(b.get("plan").ok_or_else(|| ApiError::bad("the body needs a \"plan\""))?)?;
    match app.svc.run_plan(plan, after(&b)?, body_u64(&b, "timeout_ms")).await? {
        SqlAnswer::Rows(a) => Ok(AxJson(answer(&a))),
        SqlAnswer::Written { w, took_ms } => Ok(AxJson(written(w, took_ms))),
    }
}

/// The body's `ops`, applied once per idempotency key (the body's
/// `idempotency_key` or an `Idempotency-Key` header).
async fn write(State(app): State<App>, headers: HeaderMap, AxJson(mut b): AxJson<Json>) -> Res {
    let ops = match b.get_mut("ops").map(Json::take) {
        Some(Json::Array(ops)) => ops,
        _ => return Err(ApiError::bad("the body needs an array \"ops\"")),
    };
    let key = match (b.get("idempotency_key"), headers.get("idempotency-key")) {
        (Some(Json::String(k)), _) => Some(k.clone()),
        (Some(_), _) => return Err(ApiError::bad("idempotency_key is a string")),
        (None, Some(h)) => Some(h.to_str().map_err(|_| ApiError::bad("the Idempotency-Key header is not text"))?.to_string()),
        (None, None) => None,
    };
    if key.as_ref().is_some_and(|k| k.is_empty() || k.len() > 256) {
        return Err(ApiError::bad("an idempotency key has 1 to 256 characters"));
    }
    let t0 = std::time::Instant::now();
    let w = app.svc.write_ops(ops, key).await?;
    Ok(AxJson(written(w, t0.elapsed().as_secs_f64() * 1e3)))
}

/// Rows in the body, in the format `?format=` names or the content type
/// implies. The content type must not be one a page on another site may
/// send without asking first (text/plain, a form, or none).
async fn import(State(app): State<App>, Query(q): Query<std::collections::HashMap<String, String>>, headers: HeaderMap, body: Bytes) -> Res {
    if auth::simple_content_type(header_str(&headers, header::CONTENT_TYPE)) {
        return Err(ApiError {
            status: 415,
            message: "send the rows with their content type (text/csv, text/tab-separated-values, application/x-ndjson, application/json, application/vnd.sqlite3 or application/octet-stream), not text/plain or a form".into(),
        });
    }
    let ct = header_str(&headers, header::CONTENT_TYPE).unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let sqlite = match q.get("format") {
        Some(f) => f.eq_ignore_ascii_case("sqlite"),
        None => matches!(ct.as_str(), "application/vnd.sqlite3" | "application/x-sqlite3") || body.starts_with(strata::sqlite::MAGIC),
    };
    if sqlite {
        return import_sqlite(&app, &q, body).await;
    }
    let format = match q.get("format") {
        Some(f) => Some(Format::parse(f).ok_or_else(|| ApiError::bad(format!("unknown format {f:?}: jsonl, json, csv, tsv or sqlite")))?),
        None => {
            let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
            let ct = ct.split(';').next().unwrap_or("").trim();
            match ct {
                "text/csv" => Some(Format::Csv),
                "text/tab-separated-values" => Some(Format::Tsv),
                "application/x-ndjson" | "application/jsonl" | "application/x-jsonlines" => Some(Format::Jsonl),
                "application/json" => Some(Format::Json),
                _ => None,
            }
        }
    };
    let format = format.ok_or_else(|| ApiError::bad("name the rows' format: ?format=jsonl, json, csv, tsv or sqlite"))?;
    let t0 = std::time::Instant::now();
    let (n, skipped) = app.svc.import(q.get("table").map(String::as_str), body, format).await?;
    Ok(AxJson(json!({"written": n, "skipped_fields": skipped, "took_ms": t0.elapsed().as_secs_f64() * 1e3})))
}

/// A query parameter that is true or false (`1`, `0`, `yes`, `no`), or
/// `default` when it is missing.
fn flag(q: &std::collections::HashMap<String, String>, k: &str, default: bool) -> Result<bool, ApiError> {
    match q.get(k).map(|v| v.to_ascii_lowercase()) {
        None => Ok(default),
        Some(v) => match v.as_str() {
            "" | "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            _ => Err(ApiError::bad(format!("{k} is true or false, not {v:?}"))),
        },
    }
}

/// `?tables=a,b`: the tables named, or none for every table.
fn tables_param(q: &std::collections::HashMap<String, String>) -> Vec<String> {
    q.get("tables").map(|t| t.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()).unwrap_or_default()
}

/// A SQLite file's tables ([`Service::import_sqlite`]): `?tables=a,b`
/// imports only those, `?blobs_base64=true` keeps BLOB columns as base64
/// text, and `?plan=true` answers what the import would do and writes
/// nothing.
async fn import_sqlite(app: &App, q: &std::collections::HashMap<String, String>, body: Bytes) -> Res {
    if q.contains_key("table") {
        return Err(ApiError::bad("table is for rows: a SQLite file's tables go to the tables of their names (?tables= picks them)"));
    }
    let opts = strata::sqlite::ImportOptions { tables: tables_param(q), schemas: Vec::new(), blobs_base64: flag(q, "blobs_base64", false)? };
    let plan = flag(q, "plan", false)?;
    let t0 = std::time::Instant::now();
    let mut j = app.svc.import_sqlite(body, opts, plan, 0).await?;
    if !plan {
        let written: u64 = j["tables"].as_array().map_or(0, |t| t.iter().filter_map(|t| t["rows"].as_u64()).sum());
        j["written"] = json!(written);
        j["took_ms"] = json!(t0.elapsed().as_secs_f64() * 1e3);
    }
    Ok(AxJson(j))
}

/// The database as a new SQLite file ([`Service::export_sqlite`]), sent
/// as it is read from disk: `?tables=a,b` for only those tables,
/// `?fts=false`, `?indexes=false` and `?schemas=false` to leave out the
/// FTS5 indexes, the B-tree indexes and the kept Strata schemas.
async fn export(State(app): State<App>, Query(q): Query<std::collections::HashMap<String, String>>) -> Result<Response, ApiError> {
    if let Some(f) = q.get("format").filter(|f| !f.eq_ignore_ascii_case("sqlite")) {
        return Err(ApiError::bad(format!("unknown format {f:?}: the export is a SQLite file (format=sqlite)")));
    }
    let opts = strata::sqlite::ExportOptions { tables: tables_param(&q), fts: flag(&q, "fts", true)?, indexes: flag(&q, "indexes", true)?, meta: flag(&q, "schemas", true)? };
    let (dir, path, done) = app.svc.export_sqlite(opts).await?;
    let file = tokio::fs::File::open(&path).await.map_err(ApiError::internal)?;
    let len = file.metadata().await.map_err(ApiError::internal)?.len();
    // The directory goes when the last chunk is sent.
    let chunks = futures::stream::unfold((file, Some(dir)), |(mut file, dir)| async move {
        use tokio::io::AsyncReadExt;
        dir.as_ref()?;
        let mut buf = vec![0u8; 1 << 16];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), (file, dir)))
            }
            Err(e) => Some((Err(e), (file, None))),
        }
    });
    let name = done.tables.first().map_or("strata".into(), |t| t.table.clone());
    let tables: Vec<&str> = done.tables.iter().map(|t| t.table.as_str()).collect();
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.sqlite3".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}.sqlite\"")),
            (header::HeaderName::from_static("x-strata-tables"), tables.join(",")),
        ],
        axum::body::Body::from_stream(chunks),
    )
        .into_response())
}

/// Write operations as NDJSON: one a line, or one event's several as
/// `{"ops": [...], "source": {...}}` (the source going to each operation
/// that names none), which a batch never splits. They are written in
/// batches as they arrive ([`Service::stream_write`]), and each batch is
/// answered by a line once it commits: `{"batch": n, "ops": .., "written":
/// .., "skipped": .., "lines": ..}`, `lines` counting the lines committed
/// so far. The answer ends with `{"done": true, ...}`, or with `{"error":
/// ..., "status": .., "lines": ..}` at the first failure, nothing after
/// the lines it counts applied.
async fn stream(State(app): State<App>, headers: HeaderMap, body: axum::body::Body) -> Result<Response, ApiError> {
    if auth::simple_content_type(header_str(&headers, header::CONTENT_TYPE)) {
        return Err(ApiError { status: 415, message: "send the operations as application/x-ndjson, not text/plain or a form".into() });
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Json>();
    tokio::spawn(ndjson(app.svc.clone(), body, tx).instrument(tracing::Span::current()));
    let lines = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(|j| Ok::<_, std::convert::Infallible>(Bytes::from(format!("{j}\n"))));
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], axum::body::Body::from_stream(lines)).into_response())
}

/// Lines a stream reads ahead of its writes ([`read_lines`]).
const STREAM_LINES: usize = 4096;

/// Writes a stream's lines ([`stream`]), answering on `tx`. The body is
/// read in a task of its own, ahead of the writes, so each batch takes the
/// lines that arrived while the one before it committed: one line at a
/// time while the client trickles, up to [`STREAM_OPS`] operations when
/// it sends faster than one commit takes. The client may send
/// acknowledged batches' successors before the answers arrive: answers
/// wait in memory, so that a client that reads them only once it has
/// sent everything does not stall the stream.
async fn ndjson(svc: Arc<Service>, body: axum::body::Body, tx: tokio::sync::mpsc::UnboundedSender<Json>) {
    let (ahead, mut lines) = tokio::sync::mpsc::channel(STREAM_LINES);
    tokio::spawn(read_lines(body, svc.max_body, ahead).in_current_span());
    let mut s = Stream::default();
    let mut batch: Vec<Json> = Vec::new();
    // Lines read, and lines committed.
    let (mut read, mut done) = (0u64, 0u64);
    while let Some(first) = lines.recv().await {
        let mut next = Some(first);
        while let Some(line) = next.take() {
            let ops = match line {
                Ok(ops) => s.admit(&ops).map(|_| ops).map_err(|e| ApiError { message: format!("line {}: {}", read + 1, e.message), ..e }),
                Err(e) => Err(e),
            };
            let ops = match ops {
                Ok(ops) => ops,
                Err(e) => {
                    // The lines before it are good.
                    if flush(&svc, &mut s, &mut batch, read, &mut done, &tx).await {
                        let _ = tx.send(json!({"error": e.message, "status": e.status, "lines": done}));
                    }
                    return;
                }
            };
            read += 1;
            batch.extend(ops);
            if batch.len() < STREAM_OPS {
                next = lines.try_recv().ok();
            }
        }
        if !flush(&svc, &mut s, &mut batch, read, &mut done, &tx).await {
            return;
        }
    }
    if flush(&svc, &mut s, &mut batch, read, &mut done, &tx).await {
        let _ = tx.send(json!({"done": true, "batches": s.batches, "lines": done, "written": s.written, "skipped": s.skipped}));
    }
}

/// Reads a stream's body into its lines' operations ([`line_ops`]) for
/// [`ndjson`] to write. Once [`STREAM_LINES`] lines wait it stops reading,
/// so that flow control slows a producer faster than the writes. A line
/// that is not operations is the last it sends.
async fn read_lines(body: axum::body::Body, max: usize, lines: tokio::sync::mpsc::Sender<Result<Vec<Json>, ApiError>>) {
    let at_line = |n: u64| move |e: ApiError| ApiError { message: format!("line {n}: {}", e.message), ..e };
    let mut chunks = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut n = 0u64;
    while let Some(next) = chunks.next().await {
        let bytes = match next {
            Ok(b) => b,
            Err(e) => {
                let _ = lines.send(Err(ApiError::bad(format!("reading the body: {e}")))).await;
                return;
            }
        };
        buf.extend_from_slice(&bytes);
        let mut at = 0;
        while let Some(nl) = buf[at..].iter().position(|&b| b == b'\n') {
            n += 1;
            let ops = line_ops(&buf[at..at + nl]).map_err(at_line(n));
            at += nl + 1;
            let bad = ops.is_err();
            if lines.send(ops).await.is_err() || bad {
                return;
            }
        }
        buf.drain(..at);
        if buf.len() > max {
            let _ = lines.send(Err(ApiError { status: 413, message: format!("line {}: longer than the server takes ({max} bytes)", n + 1) })).await;
            return;
        }
    }
    // A last line without its newline.
    if !buf.is_empty() {
        let _ = lines.send(line_ops(&buf).map_err(at_line(n + 1))).await;
    }
}

/// Writes the stream's batch, if any, and answers it; false when it
/// failed, which the answer says.
async fn flush(svc: &Arc<Service>, s: &mut Stream, batch: &mut Vec<Json>, read: u64, done: &mut u64, tx: &tokio::sync::mpsc::UnboundedSender<Json>) -> bool {
    if batch.is_empty() {
        *done = read;
        return true;
    }
    let ops = std::mem::take(batch);
    let n = ops.len();
    match svc.stream_write(s, ops).await {
        Ok(w) => {
            *done = read;
            let mut j = json!({"batch": s.batches, "ops": n, "written": w.rows, "skipped": w.skipped, "lines": read});
            if let Some(q) = w.seq_no {
                j["seq_no"] = json!(q);
            }
            if !w.seq_nos.is_empty() {
                j["seq_nos"] = json!(w.seq_nos);
            }
            let offsets = s.offsets();
            if !offsets.is_empty() {
                j["offsets"] = Json::Object(offsets.into_iter().map(|(n, o)| (n, json!(o))).collect());
            }
            let _ = tx.send(j);
            true
        }
        Err(e) => {
            let _ = tx.send(json!({"error": e.message, "status": e.status, "lines": *done}));
            false
        }
    }
}

/// A line of a stream: an operation, `{"ops": [...]}`, or blank.
fn line_ops(line: &[u8]) -> Result<Vec<Json>, ApiError> {
    let text = std::str::from_utf8(line).map_err(|_| ApiError::bad("not UTF-8 text"))?.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut j: Json = serde_json::from_str(text).map_err(|e| ApiError::bad(format!("not JSON: {e}")))?;
    match j.get_mut("ops").map(Json::take) {
        Some(Json::Array(mut ops)) => {
            if let Some(src) = j.get("source").filter(|s| !s.is_null()) {
                for op in ops.iter_mut().filter(|op| op.is_object() && !has_source(op)) {
                    op["source"] = src.clone();
                }
            }
            Ok(ops)
        }
        Some(_) => Err(ApiError::bad("\"ops\" is an array of operations")),
        None if j.is_object() => Ok(vec![j]),
        None => Err(ApiError::bad("a line is an operation (a JSON object), or one event's operations as {\"ops\": [...]}")),
    }
}

/// Every stream source written from, with the offset of the last event
/// each wrote ([`Service::sources`]).
async fn sources(State(app): State<App>) -> Res {
    let sources: Vec<Json> = app.svc.sources().await?.into_iter().map(source_json).collect();
    Ok(AxJson(json!({"sources": sources})))
}

/// One source's offset, null for one not written from (start from the
/// beginning).
async fn source(State(app): State<App>, axum::extract::Path(name): axum::extract::Path<String>) -> Res {
    let found = app.svc.sources().await?.into_iter().find(|s| s.name == name);
    Ok(AxJson(found.map_or_else(|| json!({"name": name, "offset": null, "updated": null}), source_json)))
}

fn source_json(s: strata::db::Source) -> Json {
    json!({"name": s.name, "offset": s.offset, "updated": s.updated.map(strata::timestamp::format)})
}

/// A schema validated, in both JSON forms.
async fn schema_check(AxJson(b): AxJson<Json>) -> Res {
    let s = strata::schema::Schema::from_json_value(&b)?;
    Ok(AxJson(json!({"schema": s, "short": s.to_short_json()})))
}

/// The vector of a text, or of an image file (`"image"`, base64), as
/// `{"embed": ...}` gives a query's parameter: for a plan, whose vectors
/// are values.
async fn embed(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let what = match b.get("image") {
        Some(i) => json!({ "image": i.as_str().ok_or_else(|| ApiError::bad("image is an image file in base64"))? }),
        None => json!(body_str(&b, "text")?),
    };
    let name = |k: &str| match b.get(k) {
        None | Some(Json::Null) => Ok(None),
        Some(f) => f.as_str().map(Some).ok_or_else(|| ApiError::bad(format!("{k} is a name"))),
    };
    let (table, field, _) = app.svc.model(name("table")?, name("field")?)?;
    let p = app.svc.params(Some(&json!({ "v": { "embed": what, "field": field, "table": table } }))).await?;
    let Some(strata::value::Value::Vector(v)) = p.0.get("v") else { return Err(ApiError::internal("the text has no vector")) };
    Ok(AxJson(json!({ "table": table, "field": field, "vector": &v[..] })))
}

async fn explain(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let sql = body_str(&b, "sql")?;
    let params = app.svc.params(b.get("params")).await?;
    let catalog = app.svc.catalog();
    let tables: Vec<&strata::schema::Schema> = catalog.iter().map(|t| &**t).collect();
    // A compound query is operators over single-table queries; what each
    // of those ran is in "plan".
    let profiles = |t: &str, n: &str| app.svc.rank_profile(t, n);
    // The plan names the files of its ranking models.
    let plan = app.svc.open_ranking_models(std::sync::Arc::new(strata::query::compound::plan_read(sql, &tables, &params, &profiles)?), false)?;
    let logical = match plan.root.uses_operators() {
        true => "a compound query: operators over single-table queries (\"tree\"), whose runs are in \"plan\"".to_string(),
        false => {
            let schema = tables.iter().find(|t| t.table.eq_ignore_ascii_case(plan.table())).ok_or_else(|| ApiError::bad(format!("query error: no such table: {}", plan.table())))?;
            // A query by a rank profile binds as written out.
            format!("{:#?}", strata::query::bind(&strata::query::profile::written_out(sql, &profiles)?, schema, &params)?)
        }
    };
    let (ir, tree) = (plan.to_json()?, json!(plan.explain(&tables)));
    // `hits`: the top rows with the values they were ranked by.
    let hits = match body_u64(&b, "hits") {
        0 => None,
        n => {
            let sql = strata::query::sql::with_features(sql)?;
            let h = app.svc.query(QueryReq { sql, params: params.clone(), timeout_ms: body_u64(&b, "timeout_ms"), ..Default::default() }).await?;
            let j = js::result(&h.rs, h.took_ms);
            let names = j["columns"].as_array().cloned().unwrap_or_default();
            let rows = j["rows"].as_array().cloned().unwrap_or_default();
            Some(rows.iter().take(n as usize).map(|r| names.iter().zip(r.as_array().into_iter().flatten()).map(|(c, v)| (c.as_str().unwrap_or("").to_string(), v.clone())).collect::<serde_json::Map<_, _>>()).collect::<Vec<_>>())
        }
    };
    let a = app.svc.query(QueryReq { sql: sql.into(), params, timeout_ms: body_u64(&b, "timeout_ms"), ..Default::default() }).await?;
    let mut out = json!({
        "kind": a.kind,
        "logical": logical,
        "ir": ir,
        "tree": tree,
        "plan": a.rs.stats.plan,
        "filter_docs": a.rs.stats.filter_docs,
        "scored": a.rs.stats.scored,
        "rows": a.rs.rows.len(),
        "took_ms": a.took_ms,
    });
    if let Some(h) = hits {
        out["hits"] = json!(h);
    }
    Ok(AxJson(out))
}

async fn synonyms(State(app): State<App>) -> AxJson<Json> {
    let s = app.svc.synonyms();
    AxJson(json!({
        "text": s.as_ref().map_or("", |s| s.text()),
        "rules": s.as_ref().map_or(0, |s| s.rules()),
        "file": app.svc.synonyms_file().map(|p| p.display().to_string()),
    }))
}

async fn put_synonyms(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let rules = app.svc.put_synonyms(body_str(&b, "text")?)?;
    Ok(AxJson(json!({"rules": rules, "file": app.svc.synonyms_file().map(|p| p.display().to_string())})))
}

/// `{"field": "sid", "prefixes": ["12", "12.7"], "where": "tag = :t",
/// "params": {...}, "table": ..., "min_seq_no": ..., "timeout_ms": ...}`:
/// the codes that can follow each prefix among the rows `where` matches
/// (all rows without it), with how many rows have each
/// ([`strata::db::Db::sid_next`]). `table` defaults to the first.
async fn sid_next(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let t0 = Instant::now();
    let table = match b.get("table") {
        None | Some(Json::Null) => app.svc.schema.table.clone(),
        Some(_) => body_str(&b, "table")?.to_string(),
    };
    let field = body_str(&b, "field")?;
    let prefixes: Vec<String> = match b.get("prefixes") {
        Some(Json::Array(ps)) => ps.iter().map(|p| p.as_str().map(String::from).ok_or_else(|| ApiError::bad("prefixes are strings, such as \"12.7\", or \"\" for the first numbers"))).collect::<Result<_, _>>()?,
        _ => return Err(ApiError::bad("the body needs \"prefixes\", a list of codes such as [\"12\", \"12.7\"] (\"\" for the first numbers)")),
    };
    let filter = match b.get("where") {
        None | Some(Json::Null) => String::new(),
        Some(_) => format!(" WHERE {}", body_str(&b, "where")?),
    };
    let sql = format!("SELECT COUNT(*) FROM {}{filter}", strata::query::sql::ident(&table));
    let params = app.svc.params(b.get("params")).await?;
    let next = app.svc.sid_next(&sql, None, &params, field, &prefixes, &after(&b)?, body_u64(&b, "timeout_ms")).await?;
    let next: Vec<Json> = prefixes
        .iter()
        .zip(next)
        .map(|(p, cs)| json!({"prefix": p, "next": cs.into_iter().map(|(code, count)| json!({"code": code, "count": count})).collect::<Vec<_>>()}))
        .collect();
    Ok(AxJson(json!({"next": next, "took_ms": t0.elapsed().as_secs_f64() * 1e3})))
}

async fn term_stats(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let params = app.svc.params(b.get("params")).await?;
    let (stats, snapshot) = app.svc.term_stats(body_str(&b, "sql")?, None, &params, &after(&b)?, body_u64(&b, "timeout_ms")).await?;
    Ok(AxJson(json!({"stats": stats, "snapshot": snapshot})))
}

async fn snapshot(State(app): State<App>) -> Res {
    let (id, txn) = app.svc.lease()?;
    Ok(AxJson(json!({"snapshot": id, "txn": txn, "ttl_s": app.svc.lease_ttl.as_secs()})))
}

/// `{"name": ..., "layout": "file" | "dir", "link_from": ...}`: writes a
/// checkpoint into the server's `--backup-dir`.
async fn admin_checkpoint(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let layout = match b.get("layout").and_then(|v| v.as_str()).unwrap_or("file") {
        "file" => Layout::SingleFile,
        "dir" | "directory" => Layout::Directory,
        other => return Err(ApiError::bad(format!("layout is \"file\" or \"dir\", not {other:?}"))),
    };
    let stats = app.svc.checkpoint(body_str(&b, "name")?, layout, b.get("link_from").and_then(|v| v.as_str())).await?;
    Ok(AxJson(json!({"checkpoint": stats})))
}

async fn admin_check(State(app): State<App>) -> Res {
    app.svc.check().await?;
    Ok(AxJson(json!({"ok": true})))
}

async fn admin_vacuum(State(app): State<App>) -> Res {
    Ok(AxJson(json!({"bytes_freed": app.svc.vacuum().await?})))
}

async fn admin_verify(State(app): State<App>, body: Option<AxJson<Json>>) -> Res {
    let timeout_ms = body.as_ref().and_then(|b| b.get("timeout_ms")).and_then(|v| v.as_u64()).unwrap_or(0);
    let timeout = if timeout_ms > 0 { Duration::from_millis(timeout_ms) } else { VERIFY_WAIT };
    Ok(AxJson(app.svc.verify_replicas(timeout).await?))
}

/// Makes this replica the primary of a new term: `{"replicas": [gRPC
/// URLs], "write_acks": "all", "force": false}`
/// ([`crate::service::Service::promote`]).
async fn admin_promote(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let urls = || ApiError::bad("replicas is a list of the gRPC URLs of the copies the new primary ships to");
    let replicas = match b.get("replicas") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(a)) => a.iter().map(|v| v.as_str().map(String::from).ok_or_else(urls)).collect::<Result<_, _>>()?,
        Some(_) => return Err(urls()),
    };
    let acks = match b.get("write_acks") {
        None | Some(Json::Null) => Acks::All,
        Some(v) => v.as_str().and_then(Acks::parse).ok_or_else(|| ApiError::bad("write_acks is all, majority or primary"))?,
    };
    let force = match b.get("force") {
        None | Some(Json::Null) => false,
        Some(v) => v.as_bool().ok_or_else(|| ApiError::bad("force is true or false"))?,
    };
    Ok(AxJson(app.svc.promote(replicas, acks, force).await?))
}

/// How long `POST /v1/admin/verify` waits for the replicas' digests by
/// default.
const VERIFY_WAIT: Duration = Duration::from_secs(60);

async fn agent_run(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let req = agent::AgentReq {
        question: body_str(&b, "question")?.to_string(),
        model: b.get("model").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        effort: b.get("effort").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        api_key: b.get("api_key").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        max_steps: b.get("max_steps").and_then(|v| v.as_u64()).map(|n| n.clamp(1, 20) as usize).unwrap_or(8),
    };
    app.svc.metrics.agent_run();
    Ok(AxJson(agent::run(&app.svc, req).await?))
}
