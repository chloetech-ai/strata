//! The HTTP server: the API, the stream of changes, a WebSocket per
//! terminal, and the UI's files. It listens on loopback, answers only to
//! loopback host names, wants the token on every `/api` route but
//! `/api/health`, and refuses a change from a page of another origin.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use crate::supervisor::{ApiError, Change, SpawnRequest, Supervisor};
use crate::trail;

#[derive(Clone)]
pub struct App {
    pub sup: Arc<Supervisor>,
    pub ui_dir: Option<PathBuf>,
    /// The levels' state (goals, questions, narration, delivery).
    pub lv: Arc<crate::levels::Levels>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(json!({ "error": self.message, "error_code": self.code }))).into_response()
    }
}

pub(crate) type ApiResult = Result<Response, ApiError>;

fn ok() -> ApiResult {
    Ok(Json(json!({ "ok": true })).into_response())
}

/// The host name of a `Host` header, without its port.
fn host_name(host: &str) -> &str {
    if host.starts_with('[') {
        match host.find(']') {
            Some(i) => &host[..=i],
            None => host,
        }
    } else {
        host.split(':').next().unwrap_or(host)
    }
}

pub(crate) fn same_bytes(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn query_token(uri: &axum::http::Uri) -> Option<String> {
    uri.query()?.split('&').find_map(|kv| kv.strip_prefix("token=").map(|t| t.to_string()))
}

/// The routes that take the token as `?token=`: the ones a browser opens
/// with no way to set a header (EventSource, WebSocket).
fn takes_query_token(path: &str) -> bool {
    path == "/api/stream" || (path.starts_with("/api/agents/") && path.ends_with("/pty"))
}

async fn guard(State(app): State<App>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if !matches!(host_name(host), "127.0.0.1" | "localhost" | "[::1]") {
        return ApiError::new(403, "forbidden", format!("this server answers to 127.0.0.1, localhost and [::1], not {host:?}")).into_response();
    }
    let path = req.uri().path().to_string();
    let changes = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) || path.ends_with("/pty");
    if changes {
        if let Some(origin) = headers.get(header::ORIGIN) {
            let origin = origin.to_str().unwrap_or("");
            if origin != format!("http://{host}") {
                return ApiError::new(403, "forbidden", format!("a request from origin {origin:?} may not change anything here")).into_response();
            }
        }
    }
    if path.starts_with("/api/") && path != "/api/health" {
        let token = &app.sup.cfg.token;
        let bearer = headers.get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer ")).map(str::trim);
        let given = match bearer {
            Some(b) => Some(b.to_string()),
            None if takes_query_token(&path) => query_token(req.uri()),
            None => None,
        };
        // The user's token, or an agent's (src/guarded.rs: it cannot decide).
        if !given.is_some_and(|g| same_bytes(&g, token) || crate::guarded::is_agent_token(&g)) {
            return ApiError::new(401, "unauthenticated", "this route needs the token: Authorization: Bearer <token>").into_response();
        }
    }
    next.run(req).await
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(ui_index))
        .route("/index.html", get(ui_index))
        .route("/app.js", get(ui_js))
        .route("/style.css", get(ui_css))
        .route("/levels/:file", get(ui_level))
        .route("/api/health", get(health))
        .route("/api/agents", get(list).post(spawn))
        .route("/api/agents/:id", get(agent))
        .route("/api/agents/:id/events", get(events))
        .route("/api/agents/:id/input", post(input))
        .route("/api/agents/:id/resize", post(resize))
        .route("/api/agents/:id/stop", post(stop))
        .route("/api/agents/:id/pty", get(pty))
        .route("/api/agents/:id/recording", get(recording))
        .route("/api/agents/:id/diff", get(diff))
        .route("/api/why", get(why))
        .route("/api/decisions", get(decisions))
        .route("/api/ask", post(ask))
        .route("/api/attention", get(attention))
        .route("/api/wait", get(wait))
        .route("/api/hook", post(hook))
        .route("/api/stream", get(stream))
        // The levels: Map, Preview, Flow.
        .merge(crate::map::routes())
        .merge(crate::preview::routes())
        .merge(crate::flow::routes())
        .merge(crate::levels::routes())
        // Fourth build, Ship: blast radius, capsule, pull request.
        .merge(crate::blast::routes())
        .merge(crate::capsule::routes())
        .merge(crate::ctxwire::routes())
        // Health: alerts, incidents and the test radar.
        .merge(crate::alerts::routes())
        .merge(crate::testruns::routes())
        // Fourth build: time travel, guarded actions, model routing.
        .merge(crate::timetravel::routes())
        .merge(crate::guarded::routes())
        .merge(crate::routing::routes())
        .merge(crate::search::routes(&app))
        .merge(crate::library::routes())
        // Preview lab: persona tests, runtime errors and POST /v1/logs.
        .merge(crate::swarm::routes())
        .merge(crate::runtime::routes())
        .merge(crate::trace::routes())
        // Fifth build: Hotspots.
        .merge(crate::hotspots::routes())
        .merge(crate::complete::routes())
        // Sixth build: repositories without an agent, and the user guide.
        .merge(crate::repos::routes())
        .route("/guide.md", get(guide_md))
        // Sixth build: "I'm stepping away" and the presence heartbeat (src/digest.rs).
        .merge(crate::digest::routes())
        // The goals rewrite (R2): Review and ship; Debug's turns, records and threshold.
        .merge(crate::review::routes())
        .merge(crate::turns::routes())
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such route")
}

// The UI's files.

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const STYLE: &str = include_str!("../ui/style.css");

fn ui_file(app: &App, name: &str, built_in: &'static str, mime: &'static str) -> Response {
    let body = match &app.ui_dir {
        Some(dir) => match std::fs::read_to_string(dir.join(name)) {
            Ok(s) => s,
            Err(e) => return ApiError::not_found(format!("cannot read {}: {e}", dir.join(name).display())).into_response(),
        },
        None => built_in.to_string(),
    };
    ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-store"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], body).into_response()
}

async fn ui_index(State(app): State<App>) -> Response {
    ui_file(&app, "index.html", INDEX, "text/html; charset=utf-8")
}

async fn ui_js(State(app): State<App>) -> Response {
    ui_file(&app, "app.js", APP_JS, "text/javascript; charset=utf-8")
}

async fn ui_css(State(app): State<App>) -> Response {
    ui_file(&app, "style.css", STYLE, "text/css; charset=utf-8")
}

/// The level screens (`ui/levels/*.js`), compiled in.
const LEVELS: &[(&str, &str)] = &[
    ("common.js", include_str!("../ui/levels/common.js")),
    ("mission.js", include_str!("../ui/levels/mission.js")),
    ("story.js", include_str!("../ui/levels/story.js")),
    ("decide.js", include_str!("../ui/levels/decide.js")),
    ("map.js", include_str!("../ui/levels/map.js")),
    ("preview.js", include_str!("../ui/levels/preview.js")),
    ("preview-lab.js", include_str!("../ui/levels/preview-lab.js")),
    ("flow.js", include_str!("../ui/levels/flow.js")),
    ("away.js", include_str!("../ui/levels/away.js")),
    ("ship.js", include_str!("../ui/levels/ship.js")),
    ("blast-summary.js", include_str!("../ui/levels/blast-summary.js")),
    ("setup.js", include_str!("../ui/levels/setup.js")),
    ("health.js", include_str!("../ui/levels/health.js")),
    ("test-chip.js", include_str!("../ui/levels/test-chip.js")),
    ("timebar.js", include_str!("../ui/levels/timebar.js")),
    ("change-requests.js", include_str!("../ui/levels/change-requests.js")),
    ("route-hint.js", include_str!("../ui/levels/route-hint.js")),
    ("library.js", include_str!("../ui/levels/library.js")),
    ("quicksearch.js", include_str!("../ui/levels/quicksearch.js")),
    ("trace.js", include_str!("../ui/levels/trace.js")),
    ("hotspots.js", include_str!("../ui/levels/hotspots.js")),
    ("complete.js", include_str!("../ui/levels/complete.js")),
    ("repos.js", include_str!("../ui/levels/repos.js")),
    ("rules.js", include_str!("../ui/levels/rules.js")),
    ("guide.js", include_str!("../ui/levels/guide.js")),
    ("lifecycle.js", include_str!("../ui/levels/lifecycle.js")),
];

/// The user guide (GUIDE.md), compiled in; `#/guide` renders it.
const GUIDE: &str = include_str!("../GUIDE.md");

async fn guide_md(State(app): State<App>) -> Response {
    match &app.ui_dir {
        // With the UI read from disk, the guide beside it is too.
        Some(dir) => match std::fs::read_to_string(dir.join("../GUIDE.md")) {
            Ok(s) => ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], s).into_response(),
            Err(_) => ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], GUIDE).into_response(),
        },
        None => ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], GUIDE).into_response(),
    }
}

async fn ui_level(State(app): State<App>, Path(file): Path<String>) -> Response {
    match LEVELS.iter().find(|(n, _)| *n == file) {
        Some((name, body)) => ui_file(&app, &format!("levels/{name}"), body, "text/javascript; charset=utf-8"),
        None => ApiError::not_found("no such level").into_response(),
    }
}

// The API.

async fn health(State(app): State<App>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "agents_running": app.sup.running_count(),
        "max_agents": app.sup.cfg.max_agents,
    }))
}

#[derive(Deserialize)]
struct ListQ {
    all: Option<String>,
}

fn truthy(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| matches!(s, "1" | "true" | "yes" | ""))
}

async fn list(State(app): State<App>, Query(q): Query<ListQ>) -> Json<Value> {
    Json(json!({ "agents": app.sup.list(truthy(&q.all)) }))
}

pub(crate) fn parse_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| ApiError::bad(format!("the body is not the JSON this route takes: {e}")))
}

pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError::internal(e.to_string()))?
}

async fn spawn(State(app): State<App>, body: Bytes) -> ApiResult {
    let req: SpawnRequest = parse_body(&body)?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    // With its goal and outcome (SPEC.md, "Goals and outcomes").
    let agent = blocking(move || crate::goals::spawn(&sup, &lv, req)).await?;
    Ok((StatusCode::CREATED, Json(json!({ "agent": agent }))).into_response())
}

async fn agent(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!({ "agent": app.sup.get(&id)? })).into_response())
}

#[derive(Deserialize)]
struct EventsQ {
    after: Option<i64>,
    limit: Option<usize>,
    data: Option<String>,
}

async fn events(State(app): State<App>, Path(id): Path<String>, Query(q): Query<EventsQ>) -> ApiResult {
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let sup = app.sup.clone();
    let evs = blocking(move || sup.events(&id, q.after.unwrap_or(0), limit, truthy(&q.data))).await?;
    Ok(Json(json!({ "events": evs })).into_response())
}

#[derive(Deserialize)]
struct InputB {
    text: Option<String>,
    enter: Option<bool>,
}

async fn input(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let b: InputB = parse_body(&body)?;
    let text = b.text.unwrap_or_default();
    let enter = b.enter.unwrap_or(true);
    if text.is_empty() && !enter {
        return Err(ApiError::bad("text is empty and enter is false: nothing to type"));
    }
    // The agent that typed it (send_to_agent), for the decision trail.
    let by = headers.get("x-workbench-agent").and_then(|h| h.to_str().ok()).map(str::to_string);
    let sup = app.sup.clone();
    blocking(move || sup.input(&id, &text, enter, by.as_deref())).await?;
    ok()
}

#[derive(Deserialize)]
struct ResizeB {
    cols: u16,
    rows: u16,
}

async fn resize(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    let b: ResizeB = parse_body(&body)?;
    app.sup.resize(&id, b.cols, b.rows)?;
    ok()
}

#[derive(Deserialize, Default)]
struct StopB {
    /// Its servers too (SPEC.md, "Sixth build: E"); unset: as chosen once.
    servers: Option<bool>,
}

async fn stop(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    let b: StopB = if body.is_empty() { StopB::default() } else { parse_body(&body)? };
    let sup = app.sup.clone();
    blocking(move || sup.stop_with(&id, b.servers)).await?;
    ok()
}

async fn attention(State(app): State<App>) -> Json<Value> {
    Json(json!({ "agents": app.sup.attention() }))
}

#[derive(Deserialize)]
struct WaitQ {
    agent: Option<String>,
    until: Option<String>,
    timeout_s: Option<f64>,
}

async fn wait(State(app): State<App>, Query(q): Query<WaitQ>) -> ApiResult {
    let id = q.agent.ok_or_else(|| ApiError::bad("agent is required"))?;
    let until = q.until.unwrap_or_else(|| "idle".into());
    if !matches!(until.as_str(), "idle" | "waiting" | "exited" | "any") {
        return Err(ApiError::bad("until is one of idle, waiting, exited, any"));
    }
    let timeout = Duration::from_secs_f64(q.timeout_s.unwrap_or(30.0).clamp(0.0, 600.0));
    let deadline = tokio::time::Instant::now() + timeout;
    let mut rx = app.sup.changes.subscribe();
    // Reached: the state, or (for `any`) a new event; an exited agent
    // reaches nothing else.
    let check = |app: &App| -> Result<Option<bool>, ApiError> {
        let (state, running) = app.sup.state(&id).ok_or_else(|| ApiError::not_found(format!("no agent {id}")))?;
        if until != "any" && state == until {
            return Ok(Some(true));
        }
        if state == "exited" || !running {
            return Ok(Some(until == "exited"));
        }
        Ok(None)
    };
    let reached = match check(&app)? {
        Some(r) if until != "any" || !r => r,
        _ => loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => break false,
                Ok(Ok(Change::Event { agent, .. })) if agent == id && until == "any" => break true,
                Ok(Ok(_)) | Ok(Err(RecvError::Lagged(_))) => {
                    if until != "any" {
                        if let Some(r) = check(&app)? {
                            break r;
                        }
                    }
                }
                Ok(Err(RecvError::Closed)) => break false,
            }
        },
    };
    Ok(Json(json!({ "agent": app.sup.get(&id)?, "reached": reached })).into_response())
}

async fn hook(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let id = headers.get("x-workbench-agent").and_then(|h| h.to_str().ok()).ok_or_else(|| ApiError::bad("the header x-workbench-agent is required"))?.to_string();
    let v: Value = parse_body(&body)?;
    let (sup, lv) = (app.sup.clone(), app.lv.clone());
    // What the levels do after it, and what the hook prints (SPEC.md,
    // "Delivery"): `output` is the hook's JSON output, or null.
    let output = blocking(move || {
        sup.hook(&id, v.clone())?;
        crate::testruns::after_hook(&sup, &id, &v);
        let out = crate::levels::after_hook(&sup, &lv, &id, &v);
        // Guarded actions (src/guarded.rs): a guarded Bash command is denied.
        // Hotspots (src/hotspots.rs): a first edit of one gets its history.
        Ok(crate::guarded::check(&sup, &id, &v).or(out).or_else(|| crate::hotspots::check(&sup, &id, &v)))
    })
    .await?;
    Ok(Json(json!({ "ok": true, "output": output })).into_response())
}

async fn stream(State(app): State<App>) -> Response {
    let rx = app.sup.changes.subscribe();
    let snapshot = json!({ "agents": app.sup.list(true) });
    let first = SseEvent::default().event("snapshot").data(snapshot.to_string());
    let sup = app.sup.clone();
    let rest = futures_util::stream::unfold((rx, sup), |(mut rx, sup)| async move {
        let ev = match rx.recv().await {
            Ok(Change::Agent { json, .. }) => SseEvent::default().event("agent").data(json.to_string()),
            Ok(Change::Event { json, .. }) => SseEvent::default().event("event").data(json.to_string()),
            // Behind: a new snapshot stands for what was missed.
            Err(RecvError::Lagged(_)) => SseEvent::default().event("snapshot").data(json!({ "agents": sup.list(true) }).to_string()),
            Err(RecvError::Closed) => return None,
        };
        Some((Ok::<_, std::convert::Infallible>(ev), (rx, sup)))
    });
    let s = futures_util::stream::once(async move { Ok(first) }).chain(rest);
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response()
}

async fn pty(State(app): State<App>, Path(id): Path<String>, ws: WebSocketUpgrade) -> ApiResult {
    app.sup.get(&id)?;
    let sup = app.sup.clone();
    Ok(ws.on_upgrade(move |socket| terminal(socket, sup, id)))
}

#[derive(Deserialize)]
struct ResizeMsg {
    resize: (u16, u16),
}

async fn terminal(socket: WebSocket, sup: Arc<Supervisor>, id: String) {
    let Ok((back, rx)) = sup.attach(&id) else { return };
    let (mut tx, mut inbox) = socket.split();
    for chunk in back.chunks(64 * 1024) {
        if tx.send(Message::Binary(chunk.to_vec())).await.is_err() {
            return;
        }
    }
    let Some(mut rx) = rx else {
        let _ = tx.send(Message::Close(None)).await;
        return;
    };
    let sup2 = sup.clone();
    let id2 = id.clone();
    let mut keys = tokio::spawn(async move {
        while let Some(Ok(msg)) = inbox.next().await {
            let bytes = match msg {
                Message::Binary(b) => b,
                Message::Text(t) => {
                    if t.starts_with('{') {
                        if let Ok(r) = serde_json::from_str::<ResizeMsg>(&t) {
                            let _ = sup2.resize(&id2, r.resize.0, r.resize.1);
                            continue;
                        }
                    }
                    t.into_bytes()
                }
                Message::Close(_) => break,
                _ => continue,
            };
            let (s, i) = (sup2.clone(), id2.clone());
            let _ = tokio::task::spawn_blocking(move || s.keys(&i, &bytes)).await;
        }
    });
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Ok(b) => {
                    if tx.send(Message::Binary(b.to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => {
                    let _ = tx.send(Message::Close(None)).await;
                    break;
                }
            },
            _ = &mut keys => break,
        }
    }
    keys.abort();
}

// The decision trail.

#[derive(Deserialize)]
struct RecordingQ {
    from_ms: Option<u64>,
    to_ms: Option<u64>,
}

async fn recording(State(app): State<App>, Path(id): Path<String>, Query(q): Query<RecordingQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(Json(blocking(move || trail::recording(&sup, &id, q.from_ms, q.to_ms)).await?).into_response())
}

async fn diff(State(app): State<App>, Path(id): Path<String>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(Json(blocking(move || trail::diff(&sup, &id)).await?).into_response())
}

async fn why(State(app): State<App>, Query(q): Query<trail::WhyQ>) -> ApiResult {
    let sup = app.sup.clone();
    Ok(Json(blocking(move || trail::why(&sup, q)).await?).into_response())
}

#[derive(Deserialize)]
struct DecisionsQ {
    repo: Option<String>,
    limit: Option<usize>,
}

async fn decisions(State(app): State<App>, Query(q): Query<DecisionsQ>) -> ApiResult {
    let sup = app.sup.clone();
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    Ok(Json(blocking(move || trail::decisions(&sup, q.repo.as_deref(), limit)).await?).into_response())
}

#[derive(Deserialize)]
struct AskB {
    repo: Option<String>,
    question: Option<String>,
}

async fn ask(State(app): State<App>, body: Bytes) -> ApiResult {
    let b: AskB = parse_body(&body)?;
    let sup = app.sup.clone();
    Ok(Json(blocking(move || trail::ask(&sup, b.repo.as_deref(), b.question.as_deref().unwrap_or(""))).await?).into_response())
}

/// Serves until SIGINT or SIGTERM, then stops the agents and exits.
pub async fn serve(listener: tokio::net::TcpListener, app: App) -> std::io::Result<()> {
    let sup = app.sup.clone();
    crate::alerts::start(app.sup.clone(), app.lv.clone());
    // Time travel's snapshots and routing's recorder (src/timetravel.rs, src/routing.rs).
    crate::timetravel::start(&sup);
    crate::routing::start(&sup);
    crate::hotspots::start(&sup);
    let router = router(app);
    let server = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>());
    tokio::select! {
        r = server => r?,
        _ = shutdown_signal() => {}
    }
    tokio::task::spawn_blocking(move || sup.shutdown()).await.ok();
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("a SIGTERM handler");
    let mut hup = signal(SignalKind::hangup()).expect("a SIGHUP handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = hup.recv() => {}
    }
}
