//! The HTTP JSON API, Prometheus metrics and the query UI.
//!
//! | route | body | answer |
//! |---|---|---|
//! | `GET /` | | the UI |
//! | `GET /v1/health`, `/v1/stats`, `/v1/schema`, `/v1/config` | | JSON |
//! | `GET /metrics` | | Prometheus text |
//! | `POST /v1/query` | `{sql, params?, timeout_ms?, shard?, stats?, snapshot?}` | rows |
//! | `POST /v1/sql` | `{sql, params?, timeout_ms?}` | rows, or `{written}` |
//! | `POST /v1/write` | `{ops: [...]}` (see `proto/strata.proto`) | `{written}` |
//! | `POST /v1/explain` | `{sql, params?}` | the bound query and its plan |
//! | `POST /v1/term_stats` | `{sql, params?}` | `{stats, snapshot}` |
//! | `POST /v1/snapshot` | | `{snapshot, txn, ttl_s}` |
//! | `POST /v1/agent` | `{question, model?, effort?, api_key?, max_steps?}` | an answer and the steps taken |
//!
//! With a token set, every route but `/` and `/v1/health` needs
//! `Authorization: Bearer <token>`.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json as AxJson, Router};
use serde_json::{json, Value as Json};

use crate::service::{QueryReq, Service, SqlAnswer};
use crate::{agent, json as js, ApiError};

const UI: &str = include_str!("../ui/index.html");

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, AxJson(json!({"error": self.message}))).into_response()
    }
}

type Res = Result<AxJson<Json>, ApiError>;

#[derive(Clone)]
struct App {
    svc: Arc<Service>,
    token: Option<Arc<str>>,
}

pub fn router(svc: Arc<Service>, token: Option<String>) -> Router {
    let app = App { svc, token: token.map(Into::into) };
    let api = Router::new()
        .route("/v1/stats", get(stats))
        .route("/v1/schema", get(schema))
        .route("/v1/config", get(config))
        .route("/metrics", get(metrics))
        .route("/v1/query", post(query))
        .route("/v1/sql", post(sql))
        .route("/v1/write", post(write))
        .route("/v1/explain", post(explain))
        .route("/v1/term_stats", post(term_stats))
        .route("/v1/snapshot", post(snapshot))
        .route("/v1/agent", post(agent_run))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .route("/", get(|| async { Html(UI) }))
        .route("/v1/health", get(health))
        .merge(api)
        .layer(DefaultBodyLimit::max(256 << 20))
        .with_state(app)
}

async fn auth(State(app): State<App>, headers: HeaderMap, req: Request, next: Next) -> Response {
    if let Some(token) = &app.token {
        let given = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        if given != Some(&**token) {
            return ApiError { status: 401, message: "missing or wrong token".into() }.into_response();
        }
    }
    next.run(req).await
}

fn body_str<'a>(b: &'a Json, k: &str) -> Result<&'a str, ApiError> {
    b.get(k).and_then(|v| v.as_str()).ok_or_else(|| ApiError::bad(format!("the body needs a string {k:?}")))
}

fn body_u64(b: &Json, k: &str) -> u64 {
    b.get(k).and_then(|v| v.as_u64()).unwrap_or(0)
}

async fn health(State(app): State<App>) -> AxJson<Json> {
    AxJson(app.svc.health().await)
}

async fn stats(State(app): State<App>) -> AxJson<Json> {
    AxJson(app.svc.stats().await)
}

async fn schema(State(app): State<App>) -> AxJson<Json> {
    let s = &app.svc.schema;
    AxJson(json!({
        "schema": s,
        "primary_key": s.primary_key.map(|k| s.fields[k].name.clone()),
        // Each embedded VECTOR field: its sources, model, and whether this
        // server can embed text for it.
        "embeddings": app.svc.models.iter().map(|m| json!({
            "field": m.field, "from": m.spec.from, "model": m.spec.model,
            "loaded": m.model.is_ok(), "error": m.model.as_ref().err(),
        })).collect::<Vec<_>>(),
    }))
}

async fn config(State(app): State<App>) -> AxJson<Json> {
    let a = &app.svc.agent;
    AxJson(json!({"agent": {"model": a.model, "server_key": a.api_key.is_some(), "models": agent::MODELS}}))
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
    };
    let a = app.svc.query(req).await?;
    Ok(AxJson(js::result(&a.rs, a.took_ms)))
}

async fn sql(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let params = app.svc.params(b.get("params")).await?;
    match app.svc.sql(body_str(&b, "sql")?, &params, body_u64(&b, "timeout_ms")).await? {
        SqlAnswer::Rows(a) => Ok(AxJson(js::result(&a.rs, a.took_ms))),
        SqlAnswer::Written { n, took_ms } => Ok(AxJson(json!({"written": n, "took_ms": took_ms}))),
    }
}

async fn write(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let ops = match b.get("ops") {
        Some(Json::Array(ops)) => ops.clone(),
        _ => return Err(ApiError::bad("the body needs an array \"ops\"")),
    };
    let t0 = std::time::Instant::now();
    let n = app.svc.write_ops(ops).await?;
    Ok(AxJson(json!({"written": n, "took_ms": t0.elapsed().as_secs_f64() * 1e3})))
}

async fn explain(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let sql = body_str(&b, "sql")?;
    let params = app.svc.params(b.get("params")).await?;
    let select = strata::query::bind(sql, &app.svc.schema, &params)?;
    let a = app.svc.query(QueryReq { sql: sql.into(), params, timeout_ms: body_u64(&b, "timeout_ms"), ..Default::default() }).await?;
    Ok(AxJson(json!({
        "kind": a.kind,
        "logical": format!("{select:#?}"),
        "plan": a.rs.stats.plan,
        "filter_docs": a.rs.stats.filter_docs,
        "scored": a.rs.stats.scored,
        "rows": a.rs.rows.len(),
        "took_ms": a.took_ms,
    })))
}

async fn term_stats(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let params = app.svc.params(b.get("params")).await?;
    let (stats, snapshot) = app.svc.term_stats(body_str(&b, "sql")?, &params).await?;
    Ok(AxJson(json!({"stats": stats, "snapshot": snapshot})))
}

async fn snapshot(State(app): State<App>) -> Res {
    let (id, txn) = app.svc.lease()?;
    Ok(AxJson(json!({"snapshot": id, "txn": txn, "ttl_s": app.svc.lease_ttl.as_secs()})))
}

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
