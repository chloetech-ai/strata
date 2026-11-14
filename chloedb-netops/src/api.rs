//! The app's HTTP API. The command center reaches it through chloedb-server
//! (`--app netops=URL`), which also loads the view from here (`/app.json`,
//! `/ui/view.js`). Every answer carries `queries`: the SQL it ran against
//! ChloeDB, with the rows and the time of each, which the page shows behind
//! its "SQL" button.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::agents::{self, quoted};
use crate::sim::{now, Engine};
use crate::strata::{Error, Row, RowExt, Trace};
use crate::telemetry::{iso, parse_iso, round, SCENARIOS};
use crate::{schema, seed};

pub const VIEW: &str = include_str!("../ui/view.js");

/// A network: three nodes and the links between them, for the navigation.
const ICON: &str = r#"<circle cx="5" cy="6" r="2.2"/><circle cx="19" cy="6" r="2.2"/><circle cx="12" cy="18" r="2.2"/><path d="M7.2 6h9.6M6.2 7.9l4.6 8.2M17.8 7.9l-4.6 8.2"/>"#;

pub struct Service {
    pub engine: Arc<Engine>,
    pub token: Option<String>,
    pub local: bool,
}

pub(crate) type Shared = Arc<Service>;

pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> ApiError {
        let status = match e.status {
            400 | 404 | 409 => StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_REQUEST),
            _ => StatusCode::BAD_GATEWAY,
        };
        ApiError(status, if e.status == 0 || e.status >= 500 { format!("ChloeDB: {}", e.message) } else { e.message })
    }
}

pub(crate) fn bad(m: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, m.into())
}

pub(crate) type Answer = Result<Json<Value>, ApiError>;

/// The queries of one request.
pub(crate) struct Q {
    pub(crate) e: Arc<Engine>,
    pub(crate) trace: Trace,
}

impl Q {
    pub(crate) fn new(svc: &Shared) -> Q {
        Q { e: svc.engine.clone(), trace: Trace::new() }
    }
    pub(crate) async fn rows(&self, what: &str, sql: &str, params: Value) -> Result<Vec<Row>, ApiError> {
        Ok(self.e.db.traced(&self.trace, what, sql, params).await?)
    }
    /// As `rows`, but a query the server refuses gives no rows (and says so
    /// in the trace) rather than failing the whole page.
    pub(crate) async fn soft(&self, what: &str, sql: &str, params: Value) -> Vec<Row> {
        self.e.db.traced(&self.trace, what, sql, params).await.unwrap_or_default()
    }
    /// A graph statement's rows, or `None` when the server does not walk
    /// graphs (a coordinator): then the statement is not sent at all, once
    /// the app knows, and the caller walks its own copy of the graph.
    pub(crate) async fn graph(&self, what: &str, sql: &str, params: Value) -> Result<Option<Vec<Row>>, ApiError> {
        if !self.e.walks_graphs() {
            return Ok(None);
        }
        let r = self.e.db.traced(&self.trace, what, sql, params).await;
        self.e.graph_answer(&r);
        match r {
            Ok(rows) => Ok(Some(rows)),
            Err(err) if crate::sim::refuses_graphs(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }
    pub(crate) fn done(&self, mut v: Value) -> Answer {
        v["queries"] = json!(self.trace.take());
        Ok(Json(v))
    }
    pub(crate) fn last(&self) -> i64 {
        self.e.st().last_minute
    }
}

const INCIDENT: &str = "id, title, severity, status, started, detected, mitigated, resolved, commander, summary, root_kind, root_entity, root_cause, confidence, category, devices, sites, services, customers, revenue_at_risk, alert_count, change_id, runbook, postmortem, origin, mttr_min, ttd_s";

pub fn router(svc: Shared) -> Router {
    Router::new()
        .route("/app.json", get(manifest))
        .route("/ui/view.js", get(view))
        .route("/v1/status", get(status))
        .route("/v1/overview", get(overview))
        .route("/v1/topology", get(topology))
        .route("/v1/topology/path", get(path))
        .route("/v1/topology/evidence", get(evidence))
        .route("/v1/topology/discover", post(discover))
        .route("/v1/incidents", get(incidents))
        .route("/v1/incidents/:id", get(incident))
        .route("/v1/incidents/:id/replay", get(replay))
        .route("/v1/incidents/:id/key", get(answer_key))
        .route("/v1/incidents/:id/:action", post(incident_act))
        .route("/v1/alerts", get(alerts))
        .route("/v1/alerts/webhook", post(webhook))
        .route("/v1/rules", get(rules).post(rule_save))
        .route("/v1/rules/test", post(rule_test))
        .route("/v1/rules/:id/:action", post(rule_act))
        .route("/v1/changes", get(changes))
        .route("/v1/changes/:id", get(change))
        .route("/v1/changes/:id/risk", post(change_risk))
        .route("/v1/devices", get(devices))
        .route("/v1/devices/:id", get(device))
        .route("/v1/devices/:id/blame", get(blame))
        .route("/v1/links/:id", get(link))
        .route("/v1/services", get(services))
        .route("/v1/services/:id", get(service))
        .route("/v1/logs", get(logs))
        .route("/v1/flows", get(flows))
        .route("/v1/search", get(search))
        .route("/v1/agents", get(agent_list))
        .route("/v1/agents/runs/:id", get(agent_run))
        .route("/v1/agents/:name/run", post(agent_start))
        .route("/v1/actions", get(actions))
        .route("/v1/actions/:id/:decision", post(action_decide))
        .route("/v1/data", get(data))
        .merge(crate::daily::routes())
        .merge(crate::leadership::routes())
        .merge(crate::hub::routes())
        .merge(crate::debug::routes())
        .merge(crate::estate::routes())
        .merge(crate::ask::routes())
        .merge(crate::assist::routes())
        .route("/v1/sim/inject", post(inject))
        .route("/v1/sim/end", post(end_fault))
        .route("/v1/sim/settings", post(settings))
        .layer(axum::middleware::from_fn_with_state(svc.clone(), guard))
        .with_state(svc)
}

/// Requests come from chloedb-server (for the command center) or from a
/// program, never from a page in a browser: one with an `Origin` is
/// refused, and with `--token`, one without it.
async fn guard(State(svc): State<Shared>, req: Request, next: Next) -> Response {
    if req.headers().contains_key(header::ORIGIN) {
        return ApiError(StatusCode::FORBIDDEN, "this app takes no requests from browsers: open it in ChloeDB's command center (chloedb-server --app netops=URL)".into()).into_response();
    }
    if let Some(t) = &svc.token {
        let given = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
        if !constant_eq(given.as_bytes(), t.as_bytes()) {
            return ApiError(StatusCode::UNAUTHORIZED, "this app needs its token (Authorization: Bearer ...)".into()).into_response();
        }
    }
    next.run(req).await
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn manifest() -> Json<Value> {
    Json(json!({
        "name": "netops",
        "title": "Network operations",
        "description": "A network's topology, telemetry, alerts, incidents and changes in ChloeDB tables, with agents that show every query they ran",
        "icon_svg": ICON,
        "view": "ui/view.js",
        "version": env!("CARGO_PKG_VERSION"),
        // On the demo train: tested by the Demos workflow, not by CI's
        // required check, and marked as a demo in the command center.
        "maturity": "demo",
    }))
}

/// The view, with an ETag of its text: a client that has it asks again with
/// If-None-Match and is answered 304 with no body.
async fn view(headers: HeaderMap) -> Response {
    let body: std::borrow::Cow<'static, str> = match std::env::var_os("CHLOEDB_NETOPS_UI_DIR") {
        Some(d) => std::fs::read_to_string(std::path::Path::new(&d).join("view.js")).map(Into::into).unwrap_or(VIEW.into()),
        None => VIEW.into(),
    };
    let tag = etag(&body);
    let fresh = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim().trim_start_matches("W/") == tag));
    let head = [(header::CONTENT_TYPE, "text/javascript; charset=utf-8".to_string()), (header::CACHE_CONTROL, "no-cache".to_string()), (header::ETAG, tag)];
    if fresh {
        return (StatusCode::NOT_MODIFIED, head).into_response();
    }
    (head, body.into_owned()).into_response()
}

/// A strong ETag for a text: FNV-1a over its bytes, so it is the same in
/// every process that serves the same view.
pub fn etag(text: &str) -> String {
    let h = text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3));
    format!("\"{h:016x}\"")
}

fn sim_state(e: &Engine) -> Value {
    let st = e.st();
    let live = st.faults.iter().find(|f| f.origin == "live" && f.ended.is_none());
    json!({
        "simulate": e.opts.simulate, "fault_every": st.fault_every, "autopilot": st.autopilot, "next_fault_at": if st.fault_every > 0 && live.is_none() { json!(iso(st.next_fault_at)) } else { Value::Null },
        "fault_active": live.is_some(), "fault_started": live.map(|f| iso(f.started)), "fault_phase": live.map(|f| f.phase(st.last_minute)), "fault_incident": live.and_then(|f| f.incident.clone()),
        "scenarios": SCENARIOS.iter().map(|s| json!({"id": s.0, "name": s.1, "what": s.2})).collect::<Vec<_>>(),
    })
}

async fn status(State(svc): State<Shared>) -> Json<Value> {
    let e = &svc.engine;
    let sim = sim_state(e);
    let st = e.st();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"), "ready": st.ready, "phase": st.phase, "progress": round(st.progress, 3), "error": st.error, "last_minute": iso(st.last_minute), "now": iso(now()),
        "semantic": st.semantic, "graph": match st.graph_sql { Some(true) => "sql", Some(false) => "app", None => "" }, "model": schema::MODEL,
        "eval_ms": round(st.eval_ms, 1), "tick_ms": round(st.tick_ms, 1), "open_incidents": st.open.len(), "firing": st.alerts.len(), "waiting": st.badges.map(|b| b.0), "notices": st.badges.map(|b| b.1), "estate": e.estate.as_ref().map(|x| json!({"objects": x.objects(), "broken": x.live.lock().unwrap_or_else(|p| p.into_inner()).len()})), "sim": sim, "local": svc.local,
        "network": {"name": "Meridian", "sites": e.w.sites.len(), "devices": e.w.devices.len(), "links": e.w.links.iter().filter(|l| l.documented).count(), "services": e.w.services.len(), "customers": e.w.customers.len()},
    }))
}

fn hours(p: &HashMap<String, String>, default: i64, e: &Engine) -> i64 {
    p.get("hours").and_then(|h| h.parse::<i64>().ok()).unwrap_or(default).clamp(1, e.opts.history_hours.max(1) + 72)
}

// ---- overview

async fn overview(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let h = hours(&p, 3, &q.e);
    let since = iso(m - h * 3600);
    let open = q.rows("Open incidents", &format!("SELECT {INCIDENT} FROM netops_incidents\nWHERE status != 'resolved'\nORDER BY started DESC LIMIT 20"), json!({})).await?;
    let recent = q.rows("Recently resolved", "SELECT id, title, severity, started, resolved, category, mttr_min, root_entity, origin FROM netops_incidents\nWHERE status = 'resolved'\nORDER BY started DESC LIMIT 8", json!({})).await?;
    let by_sev = q.rows("Firing alerts by severity", "SELECT severity, COUNT(*) AS n FROM netops_alerts\nWHERE status = 'firing'\nGROUP BY severity", json!({})).await?;
    let links = q.rows("Links by status", "SELECT status, COUNT(*) AS n FROM netops_links GROUP BY status", json!({})).await?;
    let devs = q.rows("Devices by status", "SELECT status, COUNT(*) AS n FROM netops_devices GROUP BY status", json!({})).await?;
    let bgp = q.rows("BGP sessions by state", "SELECT state, COUNT(*) AS n FROM netops_bgp GROUP BY state", json!({})).await?;
    let traffic = q.rows("Traffic, loss and errors a minute", "SELECT date_trunc('minute', ts) AS t, SUM(in_bps) AS bps, AVG(util) AS util, AVG(loss) AS loss, SUM(errors) AS errors FROM netops_link_metrics\nWHERE ts >= :since\nGROUP BY date_trunc('minute', ts) ORDER BY t LIMIT 5000", json!({"since": since})).await?;
    let svc_rows = q.rows("Services and their objectives", "SELECT id, name, tier, owner, slo_target, latency_slo_ms, revenue_per_min, customers FROM netops_services ORDER BY tier LIMIT 100", json!({})).await?;
    let points = q.rows("Service requests, errors and latency a minute (last hour)", "SELECT ts, service, requests, errors, latency_p95 FROM netops_service_metrics\nWHERE ts >= :since ORDER BY ts LIMIT 5000", json!({"since": iso(m - 3600)})).await?;
    let hot = q.rows("Hottest links this minute", "SELECT link, util, loss, errors, up FROM netops_link_metrics\nWHERE ts >= :m\nORDER BY util DESC LIMIT 8", json!({"m": iso(m)})).await?;
    let mttr = q.rows("Resolved incidents by kind", "SELECT category, COUNT(*) AS n, AVG(mttr_min) AS mttr, AVG(ttd_s) AS ttd FROM netops_incidents\nWHERE status = 'resolved'\nGROUP BY category", json!({})).await?;
    let upcoming = q.rows("Next changes", "SELECT id, title, risk, kind, window_start, window_end, devices, status, owner FROM netops_changes\nWHERE window_start >= :now AND status IN ('approved', 'scheduled')\nORDER BY window_start LIMIT 6", json!({"now": iso(m)})).await?;
    let oncall = q.rows("On call now", "SELECT team, person, role, contact FROM netops_oncall\nWHERE starts <= :now AND ends > :now LIMIT 20", json!({"now": iso(m)})).await?;
    let runs = q.rows("Latest agent runs", "SELECT id, agent, incident, subject, started, summary, verdict, ms, queries, rows_read FROM netops_agent_runs\nORDER BY started DESC LIMIT 8", json!({})).await?;
    let verdicts = q.rows("Root-cause answers against the simulator's key", "SELECT verdict, COUNT(*) AS n FROM netops_agent_runs\nWHERE agent = 'root-cause'\nGROUP BY verdict", json!({})).await?;
    let pending = q.rows("Actions waiting for approval", "SELECT id, incident, kind, title, risk, proposed FROM netops_actions\nWHERE status = 'proposed'\nORDER BY proposed DESC LIMIT 10", json!({})).await?;
    // Each service's last hour, from the points above.
    let mut per: BTreeMap<String, Vec<&Row>> = BTreeMap::new();
    for r in &points {
        per.entry(r.s("service")).or_default().push(r);
    }
    let services: Vec<Value> = svc_rows
        .iter()
        .map(|s| {
            let pts = per.get(&s.s("id")).cloned().unwrap_or_default();
            let (req, err): (f64, f64) = pts.iter().fold((0.0, 0.0), |a, r| (a.0 + r.f("requests"), a.1 + r.f("errors")));
            let last = pts.iter().rev().take(3).fold((0.0, 0.0, 0.0), |a, r| (a.0 + r.f("requests"), a.1 + r.f("errors"), a.2 + r.f("latency_p95") / 3.0));
            let now_rate = if last.0 > 0.0 { last.1 / last.0 } else { 0.0 };
            let state = if now_rate > 0.01 || last.2 > s.f("latency_slo_ms") { "failing" } else if now_rate > (1.0 - s.f("slo_target")) * 2.0 { "degraded" } else { "ok" };
            json!({"id": s.s("id"), "name": s.s("name"), "tier": s.i("tier"), "owner": s.s("owner"), "slo_target": s.f("slo_target"), "latency_slo_ms": s.f("latency_slo_ms"), "customers": s.i("customers"),
                "availability": if req > 0.0 { round(1.0 - err / req, 6) } else { 1.0 }, "error_rate": round(now_rate, 5), "p95": round(last.2, 1), "state": state,
                "errors": pts.iter().map(|r| round(if r.f("requests") > 0.0 { r.f("errors") / r.f("requests") * 100.0 } else { 0.0 }, 3)).collect::<Vec<_>>()})
        })
        .collect();
    let sim = sim_state(&q.e);
    q.done(json!({"at": iso(m), "hours": h, "open": open, "recent": recent, "alerts": by_sev, "links": links, "devices": devs, "bgp": bgp, "traffic": traffic, "services": services, "hot": hot,
        "by_kind": mttr, "upcoming": upcoming, "oncall": oncall, "runs": runs, "verdicts": verdicts, "pending": pending, "sim": sim}))
}

// ---- topology

async fn topology(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let sites = q.rows("Sites", "SELECT id, name, city, country, region, kind, lat, lon, devices FROM netops_sites LIMIT 200", json!({})).await?;
    let devices = q.rows("Devices", "SELECT id, num, site, role, vendor, model, os, os_version, status FROM netops_devices LIMIT 5000", json!({})).await?;
    let links = q.rows("The inventory's links (an edge table)", "SELECT id, src, dst, kind, plane, capacity_mbps, status, circuit, provider, a_if, z_if FROM netops_links LIMIT 10000", json!({})).await?;
    let metrics = q.rows("Each link this minute", "SELECT link, util, loss, errors, up, rx_dbm, latency_ms FROM netops_link_metrics\nWHERE ts >= :m LIMIT 10000", json!({"m": iso(m)})).await?;
    let firing = q.rows("Firing alerts", "SELECT entity_kind, entity, severity, rule_name, incident, devices FROM netops_alerts\nWHERE status = 'firing' LIMIT 2000", json!({})).await?;
    let seen = q.rows("Links read from the logs", "SELECT id, src, dst, a_if, z_if, evidence, protocols, first_seen, last_seen, confidence, state FROM netops_seen_links LIMIT 10000", json!({})).await?;
    let last = q.soft("The last comparison with the inventory", "SELECT val, updated FROM netops_state WHERE id = 'agent:topology'", json!({})).await;
    let compare: Value = last.first().and_then(|r| serde_json::from_str(&r.s("val")).ok()).unwrap_or(Value::Null);
    q.done(json!({"at": iso(m), "sites": sites, "devices": devices, "links": links, "metrics": metrics, "alerts": firing, "seen": seen, "compare": compare}))
}

async fn path(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let (from, to) = (p.get("from").cloned().unwrap_or_default(), p.get("to").cloned().unwrap_or_default());
    if q.e.w.device(&from).is_none() || q.e.w.device(&to).is_none() {
        return Err(bad("from and to must be devices"));
    }
    let sql = "SELECT id, site, role, hops() AS hop FROM netops_devices\nWHERE PATH(netops_links, :a, :b, dir => 'both', max_hops => 12)\nORDER BY hops()";
    let (hops, engine): (Vec<Value>, &str) = match q.graph("Shortest path over the links table", sql, json!({"a": from, "b": to})).await.ok().flatten() {
        Some(rows) => (rows.into_iter().map(Value::Object).collect(), "sql"),
        None => (q.e.graph.path(&from, &to).into_iter().enumerate().map(|(i, d)| json!({"id": d, "hop": i})).collect(), "app"),
    };
    q.done(json!({"from": from, "to": to, "path": hops, "engine": engine}))
}

async fn evidence(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let (a, b) = (p.get("a").cloned().unwrap_or_default(), p.get("b").cloned().unwrap_or_default());
    if q.e.w.device(&a).is_none() || q.e.w.device(&b).is_none() {
        return Err(bad("a and b must be devices"));
    }
    let lines = q.rows("The log lines that name this pair", "SELECT ts, device, program, template, message FROM netops_syslog\nWHERE program = 'lldpd' AND ((device = :a AND message LIKE :nb) OR (device = :b AND message LIKE :na))\nORDER BY ts DESC LIMIT 30", json!({"a": a, "b": b, "na": format!("%neighbor {a} %"), "nb": format!("%neighbor {b} %")})).await?;
    let inv = q.rows("The inventory's record of it", "SELECT id, kind, circuit, provider, status, a_if, z_if FROM netops_links\nWHERE (src = :a AND dst = :b) OR (src = :b AND dst = :a) LIMIT 2", json!({"a": a, "b": b})).await?;
    q.done(json!({"a": a, "b": b, "lines": lines, "inventory": inv}))
}

async fn discover(State(svc): State<Shared>) -> Answer {
    let e = &svc.engine;
    let _guard = e.lock.lock().await;
    let m = e.st().last_minute;
    Ok(Json(agents::discover(e, m, true).await?))
}

// ---- incidents

async fn incidents(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let text = p.get("q").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let semantic = q.e.st().semantic;
    let cols = "id, title, severity, status, started, detected, mitigated, resolved, commander, root_kind, root_entity, confidence, category, sites, services, customers, alert_count, runbook, origin, mttr_min, ttd_s";
    let rows = match &text {
        Some(t) => {
            let s = if semantic { "rrf(bm25(summary, :q), bm25(root_cause, :q), similarity(emb, :q))" } else { "bm25(summary, :q)" };
            q.rows("Incidents like this (words and meaning, fused)", &format!("SELECT {cols}, score() AS score FROM netops_incidents\nORDER BY {s} DESC LIMIT 20"), json!({"q": t})).await?
        }
        None => q.rows("Incidents, newest first", &format!("SELECT {cols} FROM netops_incidents\nORDER BY started DESC LIMIT 200"), json!({})).await?,
    };
    let by_kind = q.rows("Resolved incidents by kind", "SELECT category, COUNT(*) AS n, AVG(mttr_min) AS mttr, AVG(ttd_s) AS ttd FROM netops_incidents\nWHERE status = 'resolved'\nGROUP BY category", json!({})).await?;
    q.done(json!({"incidents": rows, "by_kind": by_kind, "q": text}))
}

async fn incident(State(svc): State<Shared>, Path(id): Path<String>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let rows = q.rows("The incident", &format!("SELECT {INCIDENT} FROM netops_incidents WHERE id = :id"), json!({"id": id})).await?;
    let Some(inc) = rows.into_iter().next() else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no incident {id}"))) };
    let alerts = q.rows("Its alerts", "SELECT id, rule, rule_name, severity, status, entity_kind, entity, site, summary, reading, threshold, unit, started, ended FROM netops_alerts\nWHERE incident = :id ORDER BY started LIMIT 300", json!({"id": id})).await?;
    let timeline = q.rows("Its timeline", "SELECT ts, kind, actor, note, ref_id FROM netops_timeline\nWHERE incident = :id ORDER BY ts LIMIT 300", json!({"id": id})).await?;
    let runs = q.rows("Agent runs on it", "SELECT * FROM netops_agent_runs WHERE incident = :id ORDER BY started DESC LIMIT 20", json!({"id": id})).await?;
    let acts = q.rows("Actions proposed for it", "SELECT * FROM netops_actions WHERE incident = :id ORDER BY proposed LIMIT 50", json!({"id": id})).await?;
    let rca = runs.iter().find(|r| r.s("agent") == "root-cause" || r.s("agent") == "estate").map(|r| r.s("id"));
    let (steps, receipts) = match &rca {
        Some(run) => (
            q.rows("What the root-cause agent did, step by step", "SELECT n, tool, title, detail, sql_text, row_count, ms FROM netops_agent_steps\nWHERE run = :run ORDER BY n LIMIT 100", json!({"run": run})).await?,
            q.rows("What it was shown, and what did not fit (the receipt)", "SELECT n, source, row_id, title, score, tokens, used, decision, excerpt FROM netops_receipts\nWHERE run = :run ORDER BY n LIMIT 400", json!({"run": run})).await?,
        ),
        None => (Vec::new(), Vec::new()),
    };
    let runbook = if inc.s("runbook").is_empty() { Vec::new() } else { q.rows("The runbook", "SELECT id, title, body, category, uses, wins, owner FROM netops_runbooks WHERE id = :id", json!({"id": inc.s("runbook")})).await? };
    let semantic = q.e.st().semantic;
    let text = format!("{} {}", inc.s("title"), inc.s("summary"));
    let s = if semantic { "rrf(bm25(summary, :q), bm25(root_cause, :q), similarity(emb, :q))" } else { "bm25(summary, :q)" };
    let similar = q.soft("Similar past incidents (words and meaning, fused)", &format!("SELECT id, title, category, runbook, mttr_min, started, score() AS score FROM netops_incidents\nWHERE status = 'resolved' AND id != :id\nORDER BY {s} DESC LIMIT 5"), json!({"q": text, "id": id})).await;
    let services = inc.list("services");
    let blast = if services.is_empty() { Vec::new() } else { q.rows("Services in its blast radius", &format!("SELECT id, name, tier, revenue_per_min, slo_target, customers FROM netops_services\nWHERE id IN ({}) ORDER BY tier LIMIT 20", quoted(&services)), json!({})).await? };
    let sites = inc.list("sites");
    let customers = if sites.is_empty() { Vec::new() } else { q.rows("Largest customers at its sites", &format!("SELECT id, name, tier, mrr, site, device FROM netops_customers\nWHERE site IN ({}) ORDER BY mrr DESC LIMIT 10", quoted(&sites)), json!({})).await? };
    let started = parse_iso(&inc.s("started"));
    let end = inc.get("resolved").and_then(|v| v.as_str()).map(parse_iso).unwrap_or(m);
    let (a, b) = (iso(started - 30 * 60), iso((end + 10 * 60).min(m)));
    let mut link_ids: Vec<String> = alerts.iter().filter(|x| x.s("entity_kind") == "link").map(|x| x.s("entity")).collect();
    if inc.s("root_kind") == "link" {
        link_ids.insert(0, inc.s("root_entity"));
    }
    let mut seen = HashSet::new();
    link_ids.retain(|l| seen.insert(l.clone()));
    link_ids.truncate(5);
    let link_series = if link_ids.is_empty() { Vec::new() } else { q.rows("The alerting links, a point a minute", &format!("SELECT ts, link, util, loss, errors, rx_dbm, up FROM netops_link_metrics\nWHERE ts >= :a AND ts <= :b AND link IN ({}) ORDER BY ts LIMIT 5000", quoted(&link_ids)), json!({"a": a, "b": b})).await? };
    let mut dev_ids: Vec<String> = alerts.iter().filter(|x| x.s("entity_kind") == "device").map(|x| x.s("entity")).collect();
    let mut seen = HashSet::new();
    dev_ids.retain(|l| seen.insert(l.clone()));
    dev_ids.truncate(5);
    let dev_series = if dev_ids.is_empty() { Vec::new() } else { q.rows("The alerting devices, a point a minute", &format!("SELECT ts, device, cpu, bgp_updates, sessions, up FROM netops_device_metrics\nWHERE ts >= :a AND ts <= :b AND device IN ({}) ORDER BY ts LIMIT 5000", quoted(&dev_ids)), json!({"a": a, "b": b})).await? };
    let mut svc_ids: Vec<String> = alerts.iter().filter(|x| x.s("entity_kind") == "service").map(|x| x.s("entity")).collect();
    svc_ids.sort();
    svc_ids.dedup();
    svc_ids.truncate(5);
    let svc_series = if svc_ids.is_empty() { Vec::new() } else { q.rows("The failing services, a point a minute", &format!("SELECT ts, service, requests, errors, latency_p95 FROM netops_service_metrics\nWHERE ts >= :a AND ts <= :b AND service IN ({}) ORDER BY ts LIMIT 5000", quoted(&svc_ids)), json!({"a": a, "b": b})).await? };
    let devices = inc.list("devices");
    let mut around: Vec<String> = devices.clone();
    around.extend(q.e.graph.reach(&devices, 1).into_iter().map(|x| x.0));
    let logs = if around.is_empty() { Vec::new() } else { q.rows("Warnings and errors around it", &format!("SELECT ts, device, severity, sev, template, message FROM netops_syslog\nWHERE ts >= :a AND ts <= :b AND sev <= 4 AND device IN ({})\nORDER BY ts LIMIT 150", quoted(&around)), json!({"a": iso(started - 5 * 60), "b": b})).await? };
    let change = if inc.s("change_id").is_empty() { Vec::new() } else { q.rows("The change it names", "SELECT id, title, description, kind, risk, status, executed, owner, approver, diff, rollback_plan, outcome FROM netops_changes WHERE id = :id", json!({"id": inc.s("change_id")})).await? };
    let judged = q.soft("Runbooks graded for it", "SELECT runbook, grade, judge, ts FROM netops_judgments WHERE incident = :id LIMIT 10", json!({"id": id})).await;
    q.done(json!({"incident": inc, "alerts": alerts, "timeline": timeline, "runs": runs, "steps": steps, "receipts": receipts, "actions": acts, "runbook": runbook.first(), "similar": similar,
        "blast": blast, "customers": customers, "link_series": link_series, "device_series": dev_series, "service_series": svc_series, "logs": logs, "change": change.first(), "around": around, "judgments": judged,
        "window": {"from": a, "to": b}}))
}

async fn replay(State(svc): State<Shared>, Path(id): Path<String>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let rows = q.rows("The incident", "SELECT id, started, detected, mitigated, resolved, devices FROM netops_incidents WHERE id = :id", json!({"id": id})).await?;
    let Some(inc) = rows.into_iter().next() else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no incident {id}"))) };
    let started = parse_iso(&inc.s("started"));
    let end = inc.get("resolved").and_then(|v| v.as_str()).map(parse_iso).unwrap_or(m).min(started + 90 * 60);
    let (a, b) = (iso(started - 6 * 60), iso((end + 4 * 60).min(m)));
    let devices = inc.list("devices");
    let ball: Vec<String> = q.e.graph.ball(&devices, 1).into_iter().collect();
    let set: HashSet<&String> = ball.iter().collect();
    let w = &q.e.w;
    let link_ids: Vec<String> = w.links.iter().filter(|l| l.documented && l.live && set.contains(&w.devices[l.a].id) && set.contains(&w.devices[l.b].id)).map(|l| l.id.clone()).collect();
    let links = if link_ids.is_empty() { Vec::new() } else { q.rows("Every link around it, minute by minute", &format!("SELECT ts, link, util, up, loss FROM netops_link_metrics\nWHERE ts >= :a AND ts <= :b AND link IN ({}) ORDER BY ts LIMIT 10000", quoted(&link_ids)), json!({"a": a, "b": b})).await? };
    let devs = if ball.is_empty() { Vec::new() } else { q.rows("Every device around it, minute by minute", &format!("SELECT ts, device, cpu, up FROM netops_device_metrics\nWHERE ts >= :a AND ts <= :b AND device IN ({}) ORDER BY ts LIMIT 10000", quoted(&ball)), json!({"a": a, "b": b})).await? };
    let alerts = q.rows("When each alert began and ended", "SELECT rule_name, severity, entity_kind, entity, started, ended FROM netops_alerts WHERE incident = :id ORDER BY started LIMIT 300", json!({"id": id})).await?;
    let timeline = q.rows("Its timeline", "SELECT ts, kind, actor, note FROM netops_timeline WHERE incident = :id ORDER BY ts LIMIT 300", json!({"id": id})).await?;
    q.done(json!({"incident": inc, "from": a, "to": b, "devices": ball, "links": links, "device_points": devs, "alerts": alerts, "timeline": timeline}))
}

async fn answer_key(State(svc): State<Shared>, Path(id): Path<String>) -> Answer {
    let q = Q::new(&svc);
    let rows = q.rows("The simulator's answer key for this incident", "SELECT scenario, truth_kind, truth_entity, phase, started, origin FROM netops_faults WHERE incident = :id LIMIT 1", json!({"id": id})).await?;
    q.done(json!({"key": rows.first()}))
}

/// Who did something, as it is recorded: the key the command center
/// forwarded the request for (`x-chloedb-principal`, which the caller
/// cannot choose), else, for a call with no key behind it (the server's
/// own token, an open server, a direct call with this app's token), the
/// name the body gives, else `default`.
pub fn actor(h: &HeaderMap, said: Option<&str>, default: &str) -> String {
    let principal = h.get("x-chloedb-principal").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty());
    let said = said.map(str::trim).filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("you")).map(|s| s.chars().take(80).collect::<String>());
    match principal {
        Some(p) if !matches!(p, "token" | "open" | "cluster") => p.to_string(),
        Some(p) => said.unwrap_or_else(|| if p == "open" { "someone (an open server)".into() } else { "admin (the server's token)".into() }),
        None => said.unwrap_or_else(|| default.to_string()),
    }
}

async fn incident_act(State(svc): State<Shared>, Path((id, action)): Path<(String, String)>, headers: HeaderMap, body: Option<Json<Value>>) -> Answer {
    let e = &svc.engine;
    let b = body.map(|b| b.0).unwrap_or(Value::Null);
    let who = actor(&headers, b["who"].as_str(), "Operator");
    let _guard = e.lock.lock().await;
    let m = e.st().last_minute;
    let open = e.st().open.contains_key(&id);
    match action.as_str() {
        "investigate" => {
            if !open {
                return Err(ApiError(StatusCode::CONFLICT, "the incident is resolved: the agent investigates open ones".into()));
            }
            e.investigate(&id, m).await?;
        }
        "take" => {
            if let Some(o) = e.st().open.get_mut(&id) {
                o.row.insert("commander".into(), json!(who));
            }
            e.save_incident(&id).await?;
            e.timeline(&id, now(), "status", &who, "Took the incident", "").await?;
        }
        "close" => {
            // Close with the runbook that worked: a judgment the ranking learns from.
            let runbook = b["runbook"].as_str().unwrap_or("").to_string();
            let grade = b["grade"].as_i64().unwrap_or(3).clamp(0, 3);
            let rows = e.db.query("SELECT title FROM netops_incidents WHERE id = :id", json!({"id": id})).await?;
            let Some(r) = rows.first() else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no incident {id}"))) };
            if !runbook.is_empty() {
                e.judge(&id, &runbook, grade, &who, &r.s("title"), now()).await?;
                e.db.sql(&format!("UPDATE netops_runbooks SET uses = uses + 1 WHERE id = '{}'", runbook.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>())).await.ok();
            }
            if open {
                if !runbook.is_empty() {
                    if let Some(o) = e.st().open.get_mut(&id) {
                        o.row.insert("runbook".into(), json!(runbook));
                    }
                }
                e.resolve(&id, m, &who, "closed by hand").await?;
            } else if !runbook.is_empty() {
                e.timeline(&id, now(), "status", &who, &format!("Recorded that runbook {runbook} closed it (grade {grade})"), "").await?;
            }
        }
        _ => return Err(ApiError(StatusCode::NOT_FOUND, format!("no action {action}: investigate, take or close"))),
    }
    Ok(Json(json!({"id": id, "done": action})))
}

// ---- alerts and rules

async fn alerts(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let status = p.get("status").map(String::as_str).unwrap_or("firing");
    let cols = "id, rule, rule_name, severity, status, entity_kind, entity, site, summary, reading, threshold, unit, started, ended, last_seen, incident, acked_by, acked_at, assignee, silenced";
    let rows = if status == "all" {
        q.rows("Alerts, newest first", &format!("SELECT {cols} FROM netops_alerts ORDER BY started DESC LIMIT 300"), json!({})).await?
    } else if status == "firing" {
        q.rows("Alerts firing or silenced, newest first", &format!("SELECT {cols} FROM netops_alerts\nWHERE status IN ('firing', 'silenced') ORDER BY started DESC LIMIT 300"), json!({})).await?
    } else {
        q.rows("Alerts, newest first", &format!("SELECT {cols} FROM netops_alerts\nWHERE status = :s ORDER BY started DESC LIMIT 300"), json!({"s": status})).await?
    };
    let by_rule = q.rows("Alerts raised in the last day, by rule", "SELECT rule_name, COUNT(*) AS n FROM netops_alerts\nWHERE started >= :since\nGROUP BY rule_name ORDER BY n DESC LIMIT 30", json!({"since": iso(q.last() - 86400)})).await?;
    let per_hour = q.rows("Alerts raised an hour", "SELECT date_trunc('hour', started) AS t, COUNT(*) AS n FROM netops_alerts\nWHERE started >= :since\nGROUP BY date_trunc('hour', started) ORDER BY t LIMIT 200", json!({"since": iso(q.last() - 86400)})).await?;
    q.done(json!({"alerts": rows, "by_rule": by_rule, "per_hour": per_hour, "status": status}))
}

async fn rules(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let rows = q.rows("Alert rules", "SELECT * FROM netops_alert_rules LIMIT 500", json!({})).await?;
    let (eval_ms, last) = {
        let st = q.e.st();
        (st.eval_ms, st.last_minute)
    };
    q.done(json!({"rules": rows, "eval_ms": round(eval_ms, 1), "last": iso(last)}))
}

fn select_only(sql: &str) -> Result<(), ApiError> {
    let s = sql.trim().trim_end_matches(';').to_lowercase();
    if !(s.starts_with("select") || s.starts_with("with")) || s.contains(';') {
        return Err(bad("a rule is one SELECT"));
    }
    Ok(())
}

async fn rule_test(State(svc): State<Shared>, Json(b): Json<Value>) -> Answer {
    let q = Q::new(&svc);
    let sql = b["sql"].as_str().unwrap_or("").trim().trim_end_matches(';').to_string();
    select_only(&sql)?;
    let window = b["window_s"].as_i64().unwrap_or(180).clamp(60, 86400);
    let params = if sql.contains(":since") { json!({"since": iso(q.last() - window + 1)}) } else { json!({}) };
    match q.e.db.traced(&q.trace, "The rule, run now", &sql, params).await {
        Ok(rows) => {
            let ok = rows.is_empty() || (rows[0].contains_key("entity") && rows[0].contains_key("v"));
            let n = rows.len();
            // What would fire: the rows whose reading passes the comparison.
            let cmp = b["cmp"].as_str().unwrap_or(">");
            let threshold = b["threshold"].as_f64().unwrap_or(0.0);
            let firing = rows.iter().filter(|r| !r.s("entity").is_empty() && crate::sim::fires(cmp, threshold, r)).count();
            q.done(json!({"rows": rows.into_iter().take(50).collect::<Vec<_>>(), "count": n, "firing": firing, "cmp": cmp, "threshold": threshold, "ok": ok, "note": if ok { "" } else { "A rule returns two columns: entity (what is wrong) and v (the reading)." }}))
        }
        Err(e) => q.done(json!({"error": e.message, "ok": false})),
    }
}

async fn rule_save(State(svc): State<Shared>, Json(b): Json<Value>) -> Answer {
    let e = &svc.engine;
    let name = b["name"].as_str().unwrap_or("").trim().to_string();
    let sql = b["sql"].as_str().unwrap_or("").trim().trim_end_matches(';').to_string();
    if name.is_empty() {
        return Err(bad("a rule needs a name"));
    }
    select_only(&sql)?;
    let id: String = b["id"].as_str().map(str::to_string).unwrap_or_else(|| name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect());
    let id = id.trim_matches('-').to_string();
    if id.is_empty() || id == "external" || seed::RULES.iter().any(|r| r.0 == id) {
        return Err(bad("that name is taken by a built-in rule"));
    }
    let kind = b["entity_kind"].as_str().unwrap_or("device");
    if !["link", "device", "service", "site"].contains(&kind) {
        return Err(bad("entity_kind is link, device, service or site"));
    }
    let cmp = b["cmp"].as_str().unwrap_or(">").trim();
    if !crate::sim::COMPARATORS.contains(&cmp) {
        return Err(bad(format!("cmp is how v is compared with the threshold: one of {}", crate::sim::COMPARATORS.join(" "))));
    }
    let Some(threshold) = b.get("threshold").map_or(Some(0.0), |t| t.as_f64().or_else(|| t.as_str().and_then(|s| s.trim().parse().ok()))) else { return Err(bad("threshold is a number")) };
    let severity = b["severity"].as_str().unwrap_or("minor");
    if !["critical", "major", "minor"].contains(&severity) {
        return Err(bad("severity is critical, major or minor"));
    }
    let _guard = e.lock.lock().await;
    e.db.upsert("netops_alert_rules", vec![json!({"id": id, "name": name, "description": b["description"].as_str().unwrap_or(""), "sql_text": sql, "cmp": cmp, "threshold": threshold,
        "unit": b["unit"].as_str().unwrap_or(""), "severity": severity, "entity_kind": kind, "window_s": b["window_s"].as_i64().unwrap_or(180).clamp(60, 86400), "clear_after": 2, "enabled": true,
        "runbook": b["runbook"].as_str().unwrap_or(""), "firing": 0, "builtin": false})]).await?;
    Ok(Json(json!({"id": id})))
}

async fn rule_act(State(svc): State<Shared>, Path((id, action)): Path<(String, String)>) -> Answer {
    let e = &svc.engine;
    let _guard = e.lock.lock().await;
    let rows = e.db.query("SELECT * FROM netops_alert_rules WHERE id = :id", json!({"id": id})).await?;
    let Some(mut r) = rows.into_iter().next() else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no rule {id}"))) };
    match action.as_str() {
        "toggle" => {
            let on = r.get("enabled") != Some(&Value::Bool(true));
            r.insert("enabled".into(), json!(on));
            if !on {
                r.insert("firing".into(), json!(0));
            }
            e.db.upsert("netops_alert_rules", vec![Value::Object(r)]).await?;
            Ok(Json(json!({"id": id, "enabled": on})))
        }
        "delete" => {
            if r.get("builtin") == Some(&Value::Bool(true)) {
                return Err(bad("a built-in rule can be switched off, not deleted"));
            }
            e.db.sql(&format!("DELETE FROM netops_alert_rules WHERE id = '{}'", id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>())).await?;
            Ok(Json(json!({"id": id, "deleted": true})))
        }
        _ => Err(ApiError(StatusCode::NOT_FOUND, format!("no action {action}: toggle or delete"))),
    }
}

/// Alertmanager's webhook: its alerts land in the same table as the rules'.
async fn webhook(State(svc): State<Shared>, Json(b): Json<Value>) -> Answer {
    let e = &svc.engine;
    let _guard = e.lock.lock().await;
    let mut rows = Vec::new();
    for a in b["alerts"].as_array().into_iter().flatten().take(500) {
        let labels = &a["labels"];
        let name = labels["alertname"].as_str().unwrap_or("external alert");
        let instance = labels["instance"].as_str().or(labels["device"].as_str()).unwrap_or("").split(':').next().unwrap_or("").to_string();
        let (kind, entity) = if e.w.device(&instance).is_some() { ("device", instance.clone()) } else { ("external", if instance.is_empty() { name.to_string() } else { instance.clone() }) };
        let key = format!("external:{name}:{entity}");
        let firing = a["status"].as_str().unwrap_or("firing") == "firing";
        let severity = match labels["severity"].as_str().unwrap_or("") { "critical" | "page" => "critical", "warning" | "major" => "major", _ => "minor" };
        let (devices, sites) = e.scope(kind, &entity);
        let started = a["startsAt"].as_str().map(parse_iso).filter(|t| *t > 0).unwrap_or_else(now);
        let mut row = json!({"id": format!("{key}:{}", started / 60), "rule": "external", "rule_name": name, "severity": severity, "status": if firing { "firing" } else { "resolved" }, "entity_kind": kind, "entity": entity,
            "site": sites.first().cloned().unwrap_or_default(), "devices": devices, "summary": a["annotations"]["summary"].as_str().or(a["annotations"]["description"].as_str()).unwrap_or(name), "reading": 0.0, "threshold": 0.0, "unit": "",
            "started": iso(started), "last_seen": iso(now()), "incident": "", "acked_by": ""});
        let mut st = e.st();
        if firing {
            if let Some(old) = st.alerts.get(&key) {
                row = Value::Object(old.clone());
                row["last_seen"] = json!(iso(now()));
            } else if let Value::Object(map) = &row {
                st.alerts.insert(key, map.clone());
            }
        } else if let Some(old) = st.alerts.remove(&key) {
            row = Value::Object(old);
            row["status"] = json!("resolved");
            row["ended"] = json!(iso(now()));
        }
        rows.push(row);
    }
    let n = rows.len();
    e.db.upsert("netops_alerts", rows).await?;
    Ok(Json(json!({"received": n})))
}

// ---- changes

async fn changes(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let rows = q.rows("Changes of the last week and the next ten days", "SELECT id, title, kind, risk, status, window_start, window_end, executed, owner, approver, devices, device_nums, sites, services, outcome FROM netops_changes\nWHERE window_start >= :from\nORDER BY window_start LIMIT 500", json!({"from": iso(m - 8 * 86400)})).await?;
    let outcomes = q.rows("How changes have gone, by kind", "SELECT kind, status, COUNT(*) AS n FROM netops_changes\nWHERE status IN ('completed', 'rolled_back')\nGROUP BY kind, status LIMIT 100", json!({})).await?;
    // Planned changes whose windows overlap on the same device or on
    // neighbours: flagged here from the rows above; the risk agent runs the
    // exact query (device_nums && [...]) for one change.
    let planned: Vec<&Row> = rows.iter().filter(|r| ["approved", "scheduled"].contains(&r.s("status").as_str())).collect();
    let mut clash: HashMap<String, Vec<Value>> = HashMap::new();
    for (i, a) in planned.iter().enumerate() {
        let ball = q.e.graph.ball(&a.list("devices"), 1);
        for b in planned.iter().skip(i + 1) {
            if a.s("window_start") < b.s("window_end") && b.s("window_start") < a.s("window_end") {
                let bd = b.list("devices");
                let same = bd.iter().any(|d| a.list("devices").contains(d));
                if same || bd.iter().any(|d| ball.contains(d)) {
                    clash.entry(a.s("id")).or_default().push(json!({"with": b.s("id"), "same_device": same}));
                    clash.entry(b.s("id")).or_default().push(json!({"with": a.s("id"), "same_device": same}));
                }
            }
        }
    }
    q.done(json!({"at": iso(m), "changes": rows, "collisions": clash, "outcomes": outcomes}))
}

async fn change(State(svc): State<Shared>, Path(id): Path<String>) -> Answer {
    let q = Q::new(&svc);
    let rows = q.rows("The change", "SELECT * FROM netops_changes WHERE id = :id", json!({"id": id})).await?;
    let Some(c) = rows.into_iter().next() else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no change {id}"))) };
    let configs = q.rows("Configuration versions it made", "SELECT device, version, ts, author, lines FROM netops_configs WHERE change_id = :id LIMIT 50", json!({"id": id})).await?;
    let runs = q.rows("The last risk assessment", "SELECT * FROM netops_agent_runs WHERE agent = 'change-risk' AND subject = :id ORDER BY started DESC LIMIT 1", json!({"id": id})).await?;
    let incidents = q.rows("Incidents that named it", "SELECT id, title, started, status FROM netops_incidents WHERE change_id = :id LIMIT 10", json!({"id": id})).await?;
    let checks = crate::daily::checks_for(&q, &c).await?;
    q.done(json!({"change": c, "configs": configs, "risk_run": runs.first(), "incidents": incidents, "checks": checks}))
}

async fn change_risk(State(svc): State<Shared>, Path(id): Path<String>) -> Answer {
    Ok(Json(agents::change_risk(&svc.engine, &id).await?))
}

// ---- devices, links, configuration blame

async fn devices(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let rows = q.rows("Devices", "SELECT id, num, site, role, vendor, model, os, os_version, mgmt_ip, loopback, status, serial, tags FROM netops_devices LIMIT 5000", json!({})).await?;
    let metrics = q.rows("Each device this minute", "SELECT device, cpu, mem, temp_c, bgp_updates, sessions, up FROM netops_device_metrics WHERE ts >= :m LIMIT 5000", json!({"m": iso(m)})).await?;
    let versions = q.rows("Software versions in the fleet", "SELECT os, os_version, COUNT(*) AS n FROM netops_devices GROUP BY os, os_version LIMIT 100", json!({})).await?;
    q.done(json!({"devices": rows, "metrics": metrics, "versions": versions}))
}

async fn device(State(svc): State<Shared>, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let h = hours(&p, 3, &q.e);
    let Some(d) = q.e.w.device(&id) else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no device {id}"))) };
    let row = q.rows("The device", "SELECT * FROM netops_devices WHERE id = :id", json!({"id": id})).await?;
    let links = q.rows("Its links", "SELECT id, src, dst, kind, plane, capacity_mbps, status, circuit, provider, a_if, z_if FROM netops_links\nWHERE src = :id OR dst = :id LIMIT 100", json!({"id": id})).await?;
    let link_ids: Vec<String> = links.iter().map(|l| l.s("id")).collect();
    let link_now = if link_ids.is_empty() { Vec::new() } else { q.rows("Its links this minute", &format!("SELECT link, util, loss, errors, rx_dbm, up FROM netops_link_metrics\nWHERE ts >= :m AND link IN ({}) LIMIT 200", quoted(&link_ids)), json!({"m": iso(m)})).await? };
    let series = q.rows("Its health, a point a minute", "SELECT ts, cpu, mem, temp_c, bgp_updates, sessions, up FROM netops_device_metrics\nWHERE device = :id AND ts >= :since ORDER BY ts LIMIT 10000", json!({"id": id, "since": iso(m - h * 3600)})).await?;
    let logs = q.rows("Its latest log lines", "SELECT ts, severity, sev, program, template, message FROM netops_syslog\nWHERE device = :id ORDER BY ts DESC LIMIT 60", json!({"id": id})).await?;
    let configs = q.rows("Its configuration versions", "SELECT version, ts, author, change_id, lines FROM netops_configs\nWHERE device = :id ORDER BY version DESC LIMIT 50", json!({"id": id})).await?;
    let firing = q.rows("Alerts that name it", "SELECT id, rule_name, severity, status, entity, summary, started, ended, incident FROM netops_alerts\nWHERE devices = :id ORDER BY started DESC LIMIT 30", json!({"id": id})).await?;
    let chg = q.rows("Changes that touch it", &format!("SELECT id, title, kind, risk, status, window_start, owner FROM netops_changes\nWHERE device_nums && [{}] ORDER BY window_start DESC LIMIT 12", d.num), json!({})).await?;
    let bgp = q.rows("Its BGP sessions", "SELECT id, peer_ip, peer_asn, peer_name, kind, state, prefixes_rx, since FROM netops_bgp WHERE device = :id LIMIT 50", json!({"id": id})).await?;
    let customers = q.rows("Customers attached to it", "SELECT id, name, tier, mrr, services FROM netops_customers WHERE device = :id ORDER BY mrr DESC LIMIT 30", json!({"id": id})).await?;
    let advisories = q.rows("Advisories that name its software version", "SELECT id, title, severity, cvss, fixed_in FROM netops_advisories\nWHERE os = :os AND affected = :v ORDER BY cvss DESC LIMIT 10", json!({"os": d.os, "v": d.os_version})).await?;
    let services = q.rows("Services that ride on it", &format!("SELECT id, name, tier FROM netops_services WHERE device_nums && [{}] ORDER BY tier LIMIT 20", d.num), json!({})).await?;
    q.done(json!({"device": row.first(), "links": links, "link_now": link_now, "series": series, "logs": logs, "configs": configs, "alerts": firing, "changes": chg, "bgp": bgp, "customers": customers,
        "advisories": advisories, "services": services, "hours": h}))
}

/// Each line of a device's current configuration with the version that
/// brought it, and that version's change and author.
async fn blame(State(svc): State<Shared>, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let versions = q.rows("Every version of its configuration", "SELECT version, ts, author, change_id, body FROM netops_configs\nWHERE device = :id ORDER BY version LIMIT 200", json!({"id": id})).await?;
    if versions.is_empty() {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no configuration for {id}")));
    }
    let at = p.get("version").and_then(|v| v.parse::<i64>().ok()).unwrap_or(i64::MAX);
    let upto: Vec<&Row> = versions.iter().filter(|v| v.i("version") <= at).collect();
    let Some(last) = upto.last() else { return Err(bad("no such version")) };
    let sets: Vec<(i64, HashSet<String>)> = upto.iter().map(|v| (v.i("version"), v.s("body").lines().map(str::to_string).collect())).collect();
    let prev: Option<HashSet<String>> = if upto.len() >= 2 { Some(sets[upto.len() - 2].1.clone()) } else { None };
    let meta: HashMap<i64, &Row> = upto.iter().map(|v| (v.i("version"), *v)).collect();
    let lines: Vec<Value> = last
        .s("body")
        .lines()
        .enumerate()
        .map(|(n, text)| {
            // The oldest version from which the line has been there without a gap.
            let mut born = last.i("version");
            for (v, set) in sets.iter().rev() {
                if set.contains(text) {
                    born = *v;
                } else {
                    break;
                }
            }
            let v = meta[&born];
            json!({"n": n + 1, "text": text, "version": born, "change": v.s("change_id"), "author": v.s("author"), "ts": v.s("ts"), "new": prev.as_ref().is_some_and(|p| !p.contains(text))})
        })
        .collect();
    let removed: Vec<String> = match &prev {
        Some(p) => {
            let body = last.s("body");
            let cur: HashSet<&str> = body.lines().collect();
            p.iter().filter(|l| !cur.contains(l.as_str())).cloned().collect()
        }
        None => Vec::new(),
    };
    q.done(json!({"device": id, "version": last.i("version"), "versions": versions.iter().map(|v| json!({"version": v.i("version"), "ts": v.s("ts"), "author": v.s("author"), "change": v.s("change_id")})).collect::<Vec<_>>(), "lines": lines, "removed": removed}))
}

async fn link(State(svc): State<Shared>, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let h = hours(&p, 3, &q.e);
    let row = q.rows("The link", "SELECT * FROM netops_links WHERE id = :id", json!({"id": id})).await?;
    let w = &q.e.w;
    let li = w.link_by_id(&id);
    if row.is_empty() && li.is_none() {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no link {id}")));
    }
    let other = li.and_then(|i| w.links[i].sibling).map(|s| w.links[s].id.clone());
    let mut ids = vec![id.clone()];
    ids.extend(other.clone());
    let series = q.rows("The link and its other plane, a point a minute", &format!("SELECT ts, link, util, in_bps, out_bps, loss, errors, discards, latency_ms, rx_dbm, up FROM netops_link_metrics\nWHERE link IN ({}) AND ts >= :since ORDER BY ts LIMIT 10000", quoted(&ids)), json!({"since": iso(m - h * 3600)})).await?;
    let circuit = q.rows("Its circuit", "SELECT id, provider, capacity_mbps, monthly_cost, sla, status, contract_months_left FROM netops_circuits WHERE link = :id LIMIT 1", json!({"id": id})).await?;
    let seen = q.rows("What the logs say of it", "SELECT evidence, protocols, first_seen, last_seen, confidence, state FROM netops_seen_links WHERE id = :id LIMIT 1", json!({"id": id})).await?;
    let firing = q.rows("Alerts on it", "SELECT rule_name, severity, status, summary, started, ended, incident FROM netops_alerts WHERE entity = :id ORDER BY started DESC LIMIT 20", json!({"id": id})).await?;
    q.done(json!({"link": row.first(), "other": other, "series": series, "circuit": circuit.first(), "seen": seen.first(), "alerts": firing, "hours": h}))
}

// ---- services

async fn services(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let h = hours(&p, 6, &q.e);
    let rows = q.rows("Services and their objectives", "SELECT id, name, tier, owner, kind, description, slo_target, latency_slo_ms, revenue_per_min, customers, sites FROM netops_services ORDER BY tier LIMIT 100", json!({})).await?;
    let totals = q.rows("Requests, errors and latency over the window", "SELECT service, SUM(requests) AS requests, SUM(errors) AS errors, AVG(latency_p95) AS p95, MAX(latency_p95) AS worst FROM netops_service_metrics\nWHERE ts >= :since GROUP BY service LIMIT 100", json!({"since": iso(m - h * 3600)})).await?;
    let series = q.rows("Requests, errors and latency an hour", "SELECT service, date_trunc('hour', ts) AS t, SUM(requests) AS requests, SUM(errors) AS errors, AVG(latency_p95) AS p95 FROM netops_service_metrics\nWHERE ts >= :since GROUP BY service, date_trunc('hour', ts) LIMIT 5000", json!({"since": iso(m - h * 3600)})).await.unwrap_or_default();
    let points = q.rows("The last hour, a point a minute", "SELECT ts, service, requests, errors, latency_p95 FROM netops_service_metrics WHERE ts >= :since ORDER BY ts LIMIT 5000", json!({"since": iso(m - 3600)})).await?;
    let open = q.rows("Open incidents and the services they name", "SELECT id, title, services, severity FROM netops_incidents WHERE status != 'resolved' LIMIT 50", json!({})).await?;
    q.done(json!({"services": rows, "totals": totals, "hourly": series, "points": points, "open": open, "hours": h}))
}

async fn service(State(svc): State<Shared>, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let h = hours(&p, 6, &q.e);
    let row = q.rows("The service", "SELECT * FROM netops_services WHERE id = :id", json!({"id": id})).await?;
    if row.is_empty() {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no service {id}")));
    }
    let series = q.rows("Requests, errors and latency a minute", "SELECT ts, requests, errors, latency_p50, latency_p95 FROM netops_service_metrics\nWHERE service = :id AND ts >= :since ORDER BY ts LIMIT 10000", json!({"id": id, "since": iso(m - h * 3600)})).await?;
    let customers = q.rows("Customers who buy it", "SELECT id, name, tier, site, mrr FROM netops_customers WHERE services = :id ORDER BY mrr DESC LIMIT 50", json!({"id": id})).await?;
    let past = q.rows("Incidents that touched it", "SELECT id, title, started, status, category, mttr_min FROM netops_incidents WHERE services = :id ORDER BY started DESC LIMIT 12", json!({"id": id})).await?;
    q.done(json!({"service": row.first(), "series": series, "customers": customers, "incidents": past, "hours": h}))
}

// ---- logs, flows and search

async fn logs(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let minutes = p.get("minutes").and_then(|x| x.parse::<i64>().ok()).unwrap_or(60).clamp(5, 7 * 1440);
    let text = p.get("q").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let mode = p.get("mode").map(String::as_str).unwrap_or("substring");
    let mut wh = vec!["ts >= :since".to_string()];
    let mut params = json!({"since": iso(m - minutes * 60)});
    for (k, col) in [("device", "device"), ("site", "site"), ("template", "template")] {
        if let Some(v) = p.get(k).filter(|v| !v.is_empty()) {
            wh.push(format!("{col} = :{k}"));
            params[k] = json!(v);
        }
    }
    if let Some(sev) = p.get("sev").and_then(|x| x.parse::<i64>().ok()) {
        wh.push(format!("sev <= {}", sev.clamp(0, 7)));
    }
    if let Some(t) = &text {
        if mode == "words" {
            wh.push("match(message, :q)".into());
            params["q"] = json!(t);
        } else {
            wh.push("message LIKE :q".into());
            params["q"] = json!(format!("%{t}%"));
        }
    }
    let w = wh.join(" AND ");
    let rows = q.rows(if mode == "words" { "Log lines by words (BM25 index)" } else if text.is_some() { "Log lines by substring (FM-index, no scan)" } else { "Log lines, newest first" },
        &format!("SELECT ts, device, site, severity, sev, program, template, message FROM netops_syslog\nWHERE {w}\nORDER BY ts DESC LIMIT 200"), params.clone()).await?;
    let hist = q.rows("Lines a minute", &format!("SELECT date_trunc('minute', ts) AS t, COUNT(*) AS n FROM netops_syslog\nWHERE {w}\nGROUP BY date_trunc('minute', ts) ORDER BY t LIMIT 10000"), params.clone()).await?;
    let templates = q.rows("Lines by template", &format!("SELECT template, COUNT(*) AS n, MIN(sev) AS sev FROM netops_syslog\nWHERE {w}\nGROUP BY template ORDER BY n DESC LIMIT 20"), params.clone()).await?;
    let by_device = q.rows("Lines by device", &format!("SELECT device, COUNT(*) AS n FROM netops_syslog\nWHERE {w}\nGROUP BY device ORDER BY n DESC LIMIT 12"), params.clone()).await?;
    // Templates in the last 15 minutes that the six hours before never had.
    let recent = q.rows("Templates of the last 15 minutes", "SELECT template, COUNT(*) AS n, MIN(ts) AS first FROM netops_syslog\nWHERE ts >= :since GROUP BY template LIMIT 300", json!({"since": iso(m - 15 * 60)})).await?;
    let before = q.rows("Templates of the six hours before", "SELECT template, COUNT(*) AS n FROM netops_syslog\nWHERE ts >= :base AND ts < :since GROUP BY template LIMIT 500", json!({"base": iso(m - 15 * 60 - 6 * 3600), "since": iso(m - 15 * 60)})).await?;
    let known: HashSet<String> = before.iter().map(|r| r.s("template")).collect();
    let fresh: Vec<&Row> = recent.iter().filter(|r| !known.contains(&r.s("template"))).collect();
    q.done(json!({"lines": rows, "histogram": hist, "templates": templates, "devices": by_device, "new_templates": fresh, "minutes": minutes, "mode": mode, "q": text}))
}

async fn flows(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let minutes = p.get("minutes").and_then(|x| x.parse::<i64>().ok()).unwrap_or(60).clamp(5, 3 * 1440);
    let since = json!({"since": iso(m - minutes * 60)});
    let sources = q.rows("Top source networks", "SELECT src_name, src_asn, SUM(bytes) AS bytes, SUM(packets) AS packets FROM netops_flows\nWHERE ts >= :since GROUP BY src_name, src_asn ORDER BY bytes DESC LIMIT 12", since.clone()).await?;
    let apps = q.rows("Traffic by application", "SELECT app, SUM(bytes) AS bytes FROM netops_flows\nWHERE ts >= :since GROUP BY app ORDER BY bytes DESC LIMIT 12", since.clone()).await?;
    let sites = q.rows("Traffic by destination site", "SELECT dst_site, SUM(bytes) AS bytes FROM netops_flows\nWHERE ts >= :since GROUP BY dst_site ORDER BY bytes DESC LIMIT 12", since.clone()).await?;
    let targets = q.rows("Top destination prefixes for UDP", "SELECT dst_net, dst_site, SUM(bytes) AS bytes, COUNT(DISTINCT src_asn) AS networks FROM netops_flows\nWHERE ts >= :since AND proto = 'udp' GROUP BY dst_net, dst_site ORDER BY bytes DESC LIMIT 8", since.clone()).await?;
    let tcp = q.rows("TCP a minute", "SELECT date_trunc('minute', ts) AS t, SUM(bytes) AS bytes FROM netops_flows\nWHERE ts >= :since AND proto = 'tcp' GROUP BY date_trunc('minute', ts) ORDER BY t LIMIT 10000", since.clone()).await?;
    let udp = q.rows("UDP a minute", "SELECT date_trunc('minute', ts) AS t, SUM(bytes) AS bytes FROM netops_flows\nWHERE ts >= :since AND proto = 'udp' GROUP BY date_trunc('minute', ts) ORDER BY t LIMIT 10000", since).await?;
    q.done(json!({"sources": sources, "apps": apps, "sites": sites, "targets": targets, "tcp": tcp, "udp": udp, "minutes": minutes}))
}

async fn search(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let Some(t) = p.get("q").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else { return Err(bad("q is what to look for")) };
    let semantic = q.e.st().semantic;
    let s = |field: &str| if semantic { format!("rrf(bm25({field}, :q), similarity(emb, :q))") } else { format!("bm25({field}, :q)") };
    let how = if semantic { "words and meaning, fused" } else { "words" };
    let params = json!({"q": t});
    let runbooks = q.soft(&format!("Runbooks ({how})"), &format!("SELECT id, title, category, body, score() AS score FROM netops_runbooks\nORDER BY {r} DESC LIMIT 4", r = s("body")), params.clone()).await;
    let incidents = q.soft(&format!("Incidents ({how})"), &format!("SELECT id, title, category, started, root_cause, score() AS score FROM netops_incidents\nORDER BY {r} DESC LIMIT 5", r = s("summary")), params.clone()).await;
    let tickets = q.soft(&format!("Tickets ({how})"), &format!("SELECT id, title, status, opened, resolution, score() AS score FROM netops_tickets\nORDER BY {r} DESC LIMIT 5", r = s("body")), params.clone()).await;
    let needle = json!({"q": format!("%{t}%")});
    let configs = q.soft("Configurations (substring)", "SELECT device, version, ts, change_id FROM netops_configs\nWHERE body LIKE :q ORDER BY ts DESC LIMIT 12", needle.clone()).await;
    let lines = q.soft("Log lines (substring)", "SELECT ts, device, severity, message FROM netops_syslog\nWHERE message LIKE :q ORDER BY ts DESC LIMIT 12", needle.clone()).await;
    let devs = q.soft("Devices (address or serial, substring)", "SELECT id, site, role, mgmt_ip, serial FROM netops_devices\nWHERE mgmt_ip LIKE :q OR serial LIKE :q LIMIT 8", needle).await;
    let named: Vec<Value> = q.e.w.devices.iter().filter(|d| d.id.contains(&t.to_lowercase())).take(8).map(|d| json!({"id": d.id, "site": d.site, "role": d.role})).collect();
    q.done(json!({"q": t, "semantic": semantic, "runbooks": runbooks, "incidents": incidents, "tickets": tickets, "configs": configs, "lines": lines, "devices": devs, "named": named}))
}

// ---- agents and actions

async fn agent_list(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let stats = q.rows("Runs by agent", "SELECT agent, COUNT(*) AS runs, AVG(ms) AS ms, SUM(queries) AS queries, SUM(rows_read) AS rows_read, MAX(started) AS last FROM netops_agent_runs GROUP BY agent LIMIT 50", json!({})).await?;
    let verdicts = q.rows("Root-cause answers against the simulator's key", "SELECT verdict, COUNT(*) AS n FROM netops_agent_runs\nWHERE agent = 'root-cause' GROUP BY verdict", json!({})).await?;
    let runs = q.rows("Latest runs", "SELECT id, agent, incident, subject, started, status, summary, confidence, steps, queries, rows_read, tokens, ms, graph_engine, verdict FROM netops_agent_runs\nORDER BY started DESC LIMIT 60", json!({})).await?;
    let saved = q.rows("The last result of the on-demand agents", "SELECT id, val, updated FROM netops_state WHERE id IN ('agent:capacity', 'agent:compliance', 'agent:topology') LIMIT 5", json!({})).await?;
    let pending = q.rows("Actions waiting for approval", "SELECT * FROM netops_actions WHERE status = 'proposed' ORDER BY proposed DESC LIMIT 30", json!({})).await?;
    let decided = q.rows("Actions decided", "SELECT id, incident, kind, title, status, decided, decided_by, result FROM netops_actions WHERE status != 'proposed' ORDER BY decided DESC LIMIT 20", json!({})).await?;
    let results: BTreeMap<String, Value> = saved.iter().map(|r| (r.s("id").trim_start_matches("agent:").to_string(), json!({"at": r.s("updated"), "value": serde_json::from_str::<Value>(&r.s("val")).unwrap_or(Value::Null)}))).collect();
    let (autopilot, graph) = {
        let st = q.e.st();
        (st.autopilot, st.graph_sql)
    };
    q.done(json!({"agents": agents::AGENTS.iter().map(|a| json!({"id": a.0, "name": a.1, "what": a.2, "when": a.3})).collect::<Vec<_>>(), "stats": stats, "verdicts": verdicts, "runs": runs, "results": results,
        "pending": pending, "decided": decided, "autopilot": autopilot, "graph": match graph { Some(true) => "sql", Some(false) => "app", None => "" }}))
}

async fn agent_run(State(svc): State<Shared>, Path(id): Path<String>) -> Answer {
    let q = Q::new(&svc);
    let run = q.rows("The run", "SELECT * FROM netops_agent_runs WHERE id = :id", json!({"id": id})).await?;
    if run.is_empty() {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no run {id}")));
    }
    let steps = q.rows("Its steps", "SELECT n, ts, tool, title, detail, sql_text, row_count, ms FROM netops_agent_steps WHERE run = :id ORDER BY n LIMIT 200", json!({"id": id})).await?;
    let receipts = q.rows("Its receipt", "SELECT n, source, row_id, title, score, tokens, used, decision, excerpt FROM netops_receipts WHERE run = :id ORDER BY n LIMIT 400", json!({"id": id})).await?;
    let acts = q.rows("Actions it proposed", "SELECT * FROM netops_actions WHERE run = :id LIMIT 20", json!({"id": id})).await?;
    q.done(json!({"run": run.first(), "steps": steps, "receipts": receipts, "actions": acts}))
}

async fn agent_start(State(svc): State<Shared>, Path(name): Path<String>, body: Option<Json<Value>>) -> Answer {
    let e = &svc.engine;
    let subject = body.as_ref().and_then(|b| b["subject"].as_str()).unwrap_or("").to_string();
    let out = match name.as_str() {
        "capacity" => agents::capacity(e).await?,
        "compliance" => agents::compliance(e).await?,
        "change-risk" => agents::change_risk(e, &subject).await?,
        "topology" => {
            let _guard = e.lock.lock().await;
            let m = e.st().last_minute;
            agents::discover(e, m, true).await?
        }
        "root-cause" => {
            let _guard = e.lock.lock().await;
            let m = e.st().last_minute;
            if !e.st().open.contains_key(&subject) {
                return Err(bad("subject is an open incident"));
            }
            e.investigate(&subject, m).await?;
            json!({"incident": subject})
        }
        _ => return Err(ApiError(StatusCode::NOT_FOUND, format!("no agent {name} to run on demand"))),
    };
    Ok(Json(out))
}

async fn actions(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let status = p.get("status").map(String::as_str).unwrap_or("proposed");
    let rows = q.rows("Actions", "SELECT * FROM netops_actions WHERE status = :s ORDER BY proposed DESC LIMIT 100", json!({"s": status})).await?;
    q.done(json!({"actions": rows}))
}

async fn action_decide(State(svc): State<Shared>, Path((id, decision)): Path<(String, String)>, headers: HeaderMap, body: Option<Json<Value>>) -> Answer {
    let who = actor(&headers, body.as_ref().and_then(|b| b["who"].as_str()), "Operator");
    let approve = match decision.as_str() {
        "approve" => true,
        "reject" => false,
        _ => return Err(ApiError(StatusCode::NOT_FOUND, "approve or reject".into())),
    };
    Ok(Json(svc.engine.apply_action(&id, &who, approve).await?))
}

// ---- the tables and where their rows come from

async fn data(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    // Each table's rows: one COUNT each, all at once, each in the trace.
    let mut counts = tokio::task::JoinSet::new();
    for (k, (name, _, _)) in schema::tables().into_iter().enumerate() {
        let (db, trace) = (q.e.db.clone(), q.trace.clone());
        counts.spawn(async move { (k, db.traced(&trace, &format!("The rows of {name}"), &format!("SELECT COUNT(*) AS n FROM {name}"), json!({})).await.ok().and_then(|r| r.first().map(|r| r.i("n")))) });
    }
    let mut n: HashMap<usize, i64> = HashMap::new();
    while let Some(Ok((k, c))) = counts.join_next().await {
        if let Some(c) = c {
            n.insert(k, c);
        }
    }
    let tables: Vec<Value> = schema::tables().into_iter().enumerate().map(|(k, (name, ddl, what))| json!({"table": name, "what": what, "rows": n.get(&k), "ddl": ddl, "telemetry": schema::TELEMETRY.contains(&name)})).collect();
    let sources = q.rows("Where each table's rows come from", "SELECT id, name, kind, table_name, via, status, rate, lag_s, last_seen, retention, note, snippet FROM netops_sources LIMIT 50", json!({})).await?;
    let health = q.e.db.health().await.ok();
    let (written, last_rows, semantic, graph, started) = {
        let st = q.e.st();
        (st.written.clone(), st.last_rows.clone(), st.semantic, st.graph_sql, st.started_at)
    };
    q.done(json!({"tables": tables, "sources": sources, "written": written, "last_minute_rows": last_rows, "semantic": semantic, "model": schema::MODEL,
        "graph": match graph { Some(true) => "sql", Some(false) => "app", None => "" }, "server": {"url": if svc.local { q.e.db.url.clone() } else { String::new() }, "role": health.as_ref().and_then(|h| h["role"].as_str().map(str::to_string))},
        "since": iso(started), "history_hours": q.e.opts.history_hours}))
}

// ---- the simulator

async fn inject(State(svc): State<Shared>, Json(b): Json<Value>) -> Answer {
    let scenario = b["scenario"].as_str().unwrap_or("");
    Ok(Json(svc.engine.inject(scenario, b["seed"].as_u64()).await?))
}

async fn end_fault(State(svc): State<Shared>) -> Answer {
    Ok(Json(svc.engine.end_fault().await?))
}

async fn settings(State(svc): State<Shared>, Json(b): Json<Value>) -> Answer {
    svc.engine.settings(b["autopilot"].as_bool(), b["fault_every"].as_i64()).await?;
    Ok(Json(sim_state(&svc.engine)))
}

#[cfg(test)]
mod actor_tests {
    use super::*;

    #[test]
    fn the_actor_is_the_key_the_server_forwarded_for() {
        let with = |p: &str| { let mut h = HeaderMap::new(); h.insert("x-chloedb-principal", p.parse().unwrap()); h };
        // A key's name is who it was, whatever the body says.
        assert_eq!(actor(&with("roberto"), Some("You"), "Operator"), "roberto");
        assert_eq!(actor(&with("roberto"), Some("someone else"), "Operator"), "roberto");
        // No key behind it: the body's name, never "You", else what it was.
        assert_eq!(actor(&with("token"), Some("m.rossi"), "Operator"), "m.rossi");
        assert_eq!(actor(&with("token"), Some("You"), "Operator"), "admin (the server's token)");
        assert_eq!(actor(&with("open"), None, "Operator"), "someone (an open server)");
        assert_eq!(actor(&HeaderMap::new(), Some("test"), "Operator"), "test");
        assert_eq!(actor(&HeaderMap::new(), None, "Operator"), "Operator");
    }
}
