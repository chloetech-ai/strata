//! The incident room as a hub. Every incident gets a board of panels the
//! moment it is opened: a common set, and the set for its suspected cause.
//! The people on the call then shape it: ask for a panel in a sentence (or
//! paste a SELECT), remove what is noise, move what matters to the top.
//!
//! A panel is a row of `netops_panels` holding one read-only query, so the
//! board is shared by everyone in the room, survives a restart, and is the
//! evidence of the postmortem afterwards. Panels people keep adding for one
//! kind of incident are offered to the next incident of that kind.
//!
//! Nothing here writes a model's code: a sentence is read by a small,
//! deterministic reader that knows the metrics, the tables and the names of
//! the network, and says what it understood.

use std::collections::HashSet;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::agents::quoted;
use crate::api::{bad, Answer, ApiError, Shared, Q};
use crate::sim::Engine;
use crate::strata::{Row, RowExt};
use crate::telemetry::{iso, parse_iso};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/v1/incidents/:id/panels", get(panels).post(panel_add))
        .route("/v1/incidents/:id/panels/:panel/:action", post(panel_act))
}

/// What a panel is made from: a title, how to draw it, and its query.
/// A `line` query returns `t`, `series` and `v`; a `table` returns anything.
struct Spec {
    key: String,
    title: String,
    kind: &'static str,
    unit: &'static str,
    sql: String,
    /// Minutes back from now, or 0 for the incident's own window.
    window_min: i64,
}

/// What the panels of one incident are about.
struct Scope {
    devices: Vec<String>,
    links: Vec<String>,
    sites: Vec<String>,
}

fn scope_of(e: &Engine, inc: &Row) -> Scope {
    let devices = inc.list("devices");
    let mut near: Vec<String> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    if inc.s("root_kind") == "link" {
        links.push(inc.s("root_entity"));
    }
    for d in &devices {
        if let Some(di) = e.w.devices.iter().position(|x| &x.id == d) {
            for li in e.w.links_of(di) {
                let l = &e.w.links[li];
                // Links between two devices of the incident first, then backbone ones.
                let both = devices.contains(&e.w.devices[l.a].id) && devices.contains(&e.w.devices[l.b].id);
                if both && !links.contains(&l.id) {
                    links.push(l.id.clone());
                } else if l.kind == "backbone" && !near.contains(&l.id) {
                    near.push(l.id.clone());
                }
            }
        }
    }
    // The links between the devices of the incident; their backbone
    // neighbours only when that leaves too little to look at.
    if links.len() < 2 {
        links.extend(near.into_iter().filter(|l| !links.contains(l)).collect::<Vec<_>>());
    }
    links.truncate(6);
    Scope { devices: devices.into_iter().take(12).collect(), links, sites: inc.list("sites") }
}

const LINK_METRICS: &[(&str, &str, &str, &str, &[&str])] = &[
    ("loss", "Packet loss", "MAX", "pct", &["loss", "drops", "dropping"]),
    ("util", "Load", "MAX", "%", &["load", "util", "full", "congest", "capacity"]),
    ("in_bps", "Traffic in", "SUM", "bps", &["traffic", "bps", "throughput", "volume"]),
    ("errors", "Input errors", "SUM", "", &["error", "crc"]),
    ("discards", "Discards", "SUM", "", &["discard"]),
    ("latency_ms", "Latency", "MAX", "ms", &["latency", "delay", "rtt"]),
    ("rx_dbm", "Receive power", "MIN", "dBm", &["optic", "light", "dbm", "receive power", "rx"]),
    ("up", "Link state (1 is up)", "MIN", "", &["link state", "flap", "went down", "link up", "link down"]),
];

const DEVICE_METRICS: &[(&str, &str, &str, &str, &[&str])] = &[
    ("cpu", "Control-plane CPU", "MAX", "pct", &["cpu", "processor"]),
    ("mem", "Memory", "MAX", "pct", &["mem"]),
    ("temp_c", "Temperature", "MAX", "°C", &["temp", "heat", "hot"]),
    ("bgp_updates", "BGP updates a minute", "SUM", "", &["bgp update", "churn", "updates"]),
    ("bgp_up", "BGP sessions established", "MIN", "", &["bgp", "session", "peering", "neighbour", "neighbor"]),
    ("up", "Reachable (1 is yes)", "MIN", "", &["reachab", "device up", "device down", "power feed"]),
];

fn link_line(col: &str, links: &[String]) -> String {
    let (_, _, agg, _, _) = LINK_METRICS.iter().find(|m| m.0 == col).copied().unwrap_or(LINK_METRICS[0]);
    format!("SELECT date_trunc('minute', ts) AS t, link AS series, {agg}({col}) AS v FROM netops_link_metrics\nWHERE ts >= :since AND ts <= :until AND link IN ({})\nGROUP BY date_trunc('minute', ts), link ORDER BY t LIMIT 5000", quoted(links))
}

fn device_line(col: &str, devices: &[String]) -> String {
    let (_, _, agg, _, _) = DEVICE_METRICS.iter().find(|m| m.0 == col).copied().unwrap_or(DEVICE_METRICS[0]);
    format!("SELECT date_trunc('minute', ts) AS t, device AS series, {agg}({col}) AS v FROM netops_device_metrics\nWHERE ts >= :since AND ts <= :until AND device IN ({})\nGROUP BY date_trunc('minute', ts), device ORDER BY t LIMIT 5000", quoted(devices))
}

fn link_spec(col: &str, links: &[String], what: &str) -> Spec {
    let m = LINK_METRICS.iter().find(|m| m.0 == col).copied().unwrap_or(LINK_METRICS[0]);
    Spec { key: format!("link:{col}"), title: format!("{} on {what}", m.1), kind: "line", unit: m.3, sql: link_line(col, links), window_min: 0 }
}

fn device_spec(col: &str, devices: &[String], what: &str) -> Spec {
    let m = DEVICE_METRICS.iter().find(|m| m.0 == col).copied().unwrap_or(DEVICE_METRICS[0]);
    Spec { key: format!("device:{col}"), title: format!("{} on {what}", m.1), kind: "line", unit: m.3, sql: device_line(col, devices), window_min: 0 }
}

fn table_spec(key: &str, title: String, sql: String) -> Spec {
    Spec { key: key.to_string(), title, kind: "table", unit: "", sql, window_min: 0 }
}

fn logs_spec(devices: &[String], needle: Option<&str>) -> Spec {
    match needle {
        Some(n) => table_spec("logs:like", format!("Log lines containing \"{n}\""), format!("SELECT ts, device, severity, message FROM netops_syslog\nWHERE ts >= :since AND ts <= :until AND message LIKE '%{}%'\nORDER BY ts DESC LIMIT 40", n.replace(['\'', '%', '\\'], ""))),
        None => table_spec("logs:kinds", "Error log lines, by kind".into(), format!("SELECT template, COUNT(*) AS lines, MIN(ts) AS first_seen, MAX(ts) AS last_seen FROM netops_syslog\nWHERE ts >= :since AND ts <= :until AND sev <= 3 AND device IN ({})\nGROUP BY template ORDER BY lines DESC LIMIT 12", quoted(devices))),
    }
}

fn customers_spec(devices: &[String]) -> Spec {
    table_spec("customers", "Customers attached here, largest first".into(), format!("SELECT name, tier, device, circuit, mrr FROM netops_customers\nWHERE device IN ({})\nORDER BY mrr DESC LIMIT 15", quoted(devices)))
}

fn changes_spec(nums: &str) -> Spec {
    table_spec("changes", "Changes on these devices, last three days".into(), format!("SELECT id, title, status, executed, owner FROM netops_changes\nWHERE executed >= :since3d AND device_nums && [{nums}]\nORDER BY executed DESC LIMIT 12"))
}

fn configs_spec(devices: &[String]) -> Spec {
    table_spec("configs", "Configuration versions written, last three days".into(), format!("SELECT device, version, ts, author, change_id, lines FROM netops_configs\nWHERE ts >= :since3d AND device IN ({})\nORDER BY ts DESC LIMIT 12", quoted(devices)))
}

fn flows_spec(sites: &[String]) -> Spec {
    table_spec("flows", "Who sends the most toward these sites".into(), format!("SELECT src_name, src_asn, app, dst_port, SUM(bytes) AS bytes, SUM(packets) AS packets FROM netops_flows\nWHERE ts >= :since AND ts <= :until AND dst_site IN ({})\nGROUP BY src_name, src_asn, app, dst_port ORDER BY bytes DESC LIMIT 12", quoted(sites)))
}

fn alerts_spec(incident: &str) -> Spec {
    table_spec("alerts", "Alerts in this incident".into(), format!("SELECT rule_name, entity, severity, status, reading, unit, started, ended FROM netops_alerts\nWHERE incident = '{}'\nORDER BY started LIMIT 40", incident.replace('\'', "")))
}

fn nums(e: &Engine, devices: &[String]) -> String {
    let v: Vec<String> = devices.iter().filter_map(|d| e.w.devices.iter().find(|x| &x.id == d)).map(|d| d.num.to_string()).collect();
    if v.is_empty() { "0".into() } else { v.join(", ") }
}

/// The board an incident starts with: what every incident needs, then what
/// its suspected cause needs.
fn template(e: &Engine, inc: &Row) -> Vec<Spec> {
    // An outage in the estate has no backbone device or link: its board is
    // the hub it is in, read from the estate's tables.
    if inc.s("root_kind") == "estate" || inc.s("category") == "estate" || e.is_estate(&inc.s("id")) {
        use crate::estate::{EVENTS, NODES};
        let hubs = quoted(&inc.list("sites"));
        return vec![
            table_spec("estate:hub", "What is not ok in the hub, by kind".into(), format!("SELECT kind, state, COUNT(*) AS objects FROM {NODES}\nWHERE hub IN ({hubs}) AND state != 'ok'\nGROUP BY kind, state ORDER BY objects DESC LIMIT 30")),
            table_spec("estate:events", "Faults, clearings and changes in the hub, last three days".into(), format!("SELECT e.ts AS ts, e.node AS node, e.event AS event, e.cause AS cause, e.author AS author, e.text AS text FROM {EVENTS} e JOIN {NODES} n ON n.id = e.node\nWHERE n.hub IN ({hubs}) AND e.ts >= :since3d\nORDER BY ts DESC LIMIT 20")),
            table_spec("customers", "Customers behind what is not ok in the hub".into(), format!("SELECT customer, COUNT(*) AS objects FROM {NODES}\nWHERE hub IN ({hubs}) AND state != 'ok' AND customer != ''\nGROUP BY customer ORDER BY objects DESC LIMIT 15")),
        ];
    }
    let s = scope_of(e, inc);
    let dn = nums(e, &s.devices);
    let mut out = Vec::new();
    if !s.links.is_empty() {
        out.push(link_spec("loss", &s.links, "its links"));
    }
    let cause: Vec<Spec> = match inc.s("category").as_str() {
        "fiber_cut" => vec![link_spec("up", &s.links, "its links"), link_spec("util", &s.links, "its links"), link_spec("latency_ms", &s.links, "its links")],
        "bad_change" => vec![device_spec("bgp_up", &s.devices, "its devices"), configs_spec(&s.devices), changes_spec(&dn)],
        "optic_degrade" => vec![link_spec("rx_dbm", &s.links, "its links"), link_spec("errors", &s.links, "its links")],
        "ddos" => vec![link_spec("in_bps", &s.links, "its links"), flows_spec(&s.sites), link_spec("util", &s.links, "its links")],
        "cpu_bug" => vec![device_spec("cpu", &s.devices, "its devices"), device_spec("bgp_updates", &s.devices, "its devices")],
        "power" => vec![device_spec("up", &s.devices, "its devices"), device_spec("temp_c", &s.devices, "its devices")],
        _ => vec![link_spec("util", &s.links, "its links"), device_spec("cpu", &s.devices, "its devices"), changes_spec(&dn)],
    };
    out.extend(cause.into_iter().filter(|p| !(p.key.starts_with("link:") && s.links.is_empty())));
    out.push(logs_spec(&s.devices, None));
    out.push(table_spec("customers", "Customers at these sites, largest first".into(), format!("SELECT name, tier, site, device, circuit, mrr FROM netops_customers\nWHERE site IN ({})\nORDER BY mrr DESC LIMIT 15", quoted(&s.sites))));
    out
}

/// Reads a sentence into a panel. Says what it did not understand rather
/// than guessing.
fn read_ask(e: &Engine, inc: &Row, text: &str) -> std::result::Result<Spec, String> {
    let raw = text.trim();
    let t = raw.to_lowercase();
    if t.starts_with("select") || t.starts_with("with ") {
        if raw.contains(';') {
            return Err("One SELECT, without a semicolon.".into());
        }
        let words: HashSet<&str> = t.split(|c: char| !c.is_alphanumeric() && c != '_').collect();
        if ["insert", "update", "delete", "drop", "alter", "create", "truncate", "copy", "attach"].iter().any(|w| words.contains(w)) {
            return Err("A panel only reads: a SELECT over the netops_ tables.".into());
        }
        let line = t.contains(" as t") && t.contains(" as v");
        return Ok(Spec { key: String::new(), title: "A query".into(), kind: if line { "line" } else { "table" }, unit: "", sql: raw.to_string(), window_min: 0 });
    }
    // What it is about: names in the sentence, or the incident's own.
    let base = scope_of(e, inc);
    let mut devices: Vec<String> = e.w.devices.iter().filter(|d| t.contains(&d.id)).map(|d| d.id.clone()).collect();
    let mut links: Vec<String> = e.w.links.iter().filter(|l| t.contains(&l.id.to_lowercase()) || (!l.circuit.is_empty() && t.contains(&l.circuit.to_lowercase()))).map(|l| l.id.clone()).collect();
    let mut sites: Vec<String> = Vec::new();
    for s in &e.w.sites {
        if t.split(|c: char| !c.is_alphanumeric()).any(|w| w == s.id || w == s.id.to_lowercase() || w == s.name.to_lowercase()) {
            sites.push(s.id.to_string());
        }
    }
    let mut what = String::new();
    if sites.len() >= 2 && links.is_empty() {
        links = e.w.links.iter().filter(|l| l.kind == "backbone" && sites.iter().any(|s| e.w.devices[l.a].site == s) && sites.iter().any(|s| e.w.devices[l.b].site == s) && e.w.devices[l.a].site != e.w.devices[l.b].site).map(|l| l.id.clone()).collect();
        what = sites.iter().map(|s| s.to_uppercase()).collect::<Vec<_>>().join(" to ");
    } else if sites.len() == 1 && devices.is_empty() && links.is_empty() {
        devices = e.w.devices.iter().filter(|d| d.site == sites[0] && ["core", "border", "agg"].contains(&d.role)).map(|d| d.id.clone()).collect();
        links = e.w.links.iter().filter(|l| l.kind == "backbone" && (e.w.devices[l.a].site == sites[0] || e.w.devices[l.b].site == sites[0])).map(|l| l.id.clone()).collect();
        what = sites[0].to_uppercase();
    } else if !devices.is_empty() && links.is_empty() {
        for d in &devices {
            if let Some(di) = e.w.devices.iter().position(|x| &x.id == d) {
                links.extend(e.w.links_of(di).into_iter().map(|li| e.w.links[li].id.clone()));
            }
        }
        what = devices.join(", ");
    } else if !links.is_empty() {
        what = if links.len() == 1 { links[0].clone() } else { format!("{} links", links.len()) };
        if devices.is_empty() {
            devices = links.iter().filter_map(|id| e.w.links.iter().find(|l| &l.id == id)).flat_map(|l| [e.w.devices[l.a].id.clone(), e.w.devices[l.b].id.clone()]).collect();
        }
    }
    let named = !what.is_empty();
    if !named {
        devices = base.devices.clone();
        links = base.links.clone();
        what = "its devices and links".into();
    }
    if sites.is_empty() {
        sites = base.sites.clone();
    }
    links.sort();
    links.dedup();
    links.truncate(10);
    devices.sort();
    devices.dedup();
    devices.truncate(12);
    // The window: "last 2 hours", "last 30 min", or the incident's.
    let mut window_min = 0;
    let w: Vec<&str> = t.split_whitespace().collect();
    for i in 0..w.len().saturating_sub(1) {
        if let Ok(n) = w[i].parse::<i64>() {
            let u = w[i + 1];
            if u.starts_with('h') {
                window_min = n * 60;
            } else if u.starts_with("min") || u == "m" {
                window_min = n;
            } else if u.starts_with('d') {
                window_min = n * 1440;
            }
        }
    }
    window_min = window_min.clamp(0, 3 * 1440);
    let has = |words: &[&str]| words.iter().any(|x| t.contains(x));
    let quote = raw.split('"').nth(1).filter(|s| s.len() >= 3);
    let mut spec = if has(&["log", "syslog"]) || quote.is_some() {
        logs_spec(&devices, quote)
    } else if has(&["customer", "who is affected", "who is hit"]) {
        customers_spec(&devices)
    } else if has(&["config", "blame", "commit"]) {
        configs_spec(&devices)
    } else if has(&["change", "maintenance", "who touched"]) {
        changes_spec(&nums(e, &devices))
    } else if has(&["flow", "talker", "who sends", "source", "attack"]) {
        flows_spec(&sites)
    } else if has(&["alert"]) {
        alerts_spec(&inc.s("id"))
    } else if let Some(m) = DEVICE_METRICS.iter().find(|m| has(m.4)) {
        if devices.is_empty() {
            return Err("That is a reading of a device, and no device is named or involved.".into());
        }
        device_spec(m.0, &devices, if named { &what } else { "its devices" })
    } else if let Some(m) = LINK_METRICS.iter().find(|m| has(m.4)) {
        if links.is_empty() {
            return Err("That is a reading of a link, and no link is named or involved.".into());
        }
        link_spec(m.0, &links, if named { &what } else { "its links" })
    } else {
        return Err("Name a reading (loss, load, traffic, errors, latency, optical power, CPU, memory, temperature, BGP sessions), or ask for logs, customers, changes, configurations, flows or alerts. A device, a link, a circuit or two sites narrow it; \"last 2 hours\" sets the window. Or paste a SELECT.".into());
    };
    if spec.kind == "table" && named && !spec.title.contains(&what) && !spec.key.starts_with("logs:like") && spec.key != "alerts" {
        spec.title = format!("{} ({what})", spec.title.replace(" here", "").replace(" on these devices", ""));
    }
    if window_min > 0 {
        spec.window_min = window_min;
        spec.title = format!("{}, last {}", spec.title, if window_min % 60 == 0 { format!("{} h", window_min / 60) } else { format!("{window_min} min") });
    }
    spec.key = String::new();
    Ok(spec)
}

fn panel_row(inc: &Row, spec: &Spec, pos: i64, source: &str, who: &str, asked: &str, id: String, now: i64) -> Value {
    json!({"id": id, "incident": inc.s("id"), "category": inc.s("category"), "tkey": spec.key, "pos": pos, "title": spec.title, "kind": spec.kind, "unit": spec.unit,
        "sql_text": spec.sql, "window_min": spec.window_min, "source": source, "asked": asked, "added_by": who, "added": iso(now), "removed": false})
}

/// The window a panel's query reads: its own, or the incident's (from half
/// an hour before it began to a quarter after it ended, or now).
fn window(inc: &Row, p: &Row, now: i64) -> Value {
    let started = parse_iso(&inc.s("started"));
    let ended = if inc.s("resolved").is_empty() { now } else { (parse_iso(&inc.s("resolved")) + 900).min(now) };
    let (since, until) = if p.i("window_min") > 0 { (now - p.i("window_min") * 60, now) } else { (started - 1800, ended) };
    json!({"since": iso(since), "until": iso(until), "since3d": iso(started - 3 * 86400)})
}

async fn load(q: &Q, id: &str) -> std::result::Result<Row, ApiError> {
    let rows = q.rows("The incident", "SELECT id, title, status, started, resolved, category, root_kind, root_entity, devices, sites FROM netops_incidents WHERE id = :id", json!({"id": id})).await?;
    rows.into_iter().next().ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no incident {id}")))
}

async fn panels(State(svc): State<Shared>, Path(id): Path<String>) -> Answer {
    let q = Q::new(&svc);
    let e = &svc.engine;
    let inc = load(&q, &id).await?;
    let now = q.last();
    // Looking writes nothing: a read key sees the board as it would be.
    let rows = board(&q, e, &inc, now, false).await?;
    let mut out = Vec::new();
    for p in rows.iter().filter(|p| p.get("removed").and_then(Value::as_bool) != Some(true)) {
        let t = std::time::Instant::now();
        let res = e.db.traced(&q.trace, &p.s("title"), &p.s("sql_text"), window(&inc, p, now)).await;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let (data, error) = match res {
            Ok(r) => (r, String::new()),
            Err(x) => (Vec::new(), x.to_string()),
        };
        out.push(json!({"id": p.s("id"), "title": p.s("title"), "kind": p.s("kind"), "unit": p.s("unit"), "sql": p.s("sql_text"), "source": p.s("source"), "asked": p.s("asked"),
            "added_by": p.s("added_by"), "added": p.s("added"), "window_min": p.i("window_min"), "rows": data, "ms": (ms * 10.0).round() / 10.0, "error": error}));
    }
    // What people added by hand in other incidents of this kind.
    let mut often = Vec::new();
    if !inc.s("category").is_empty() {
        let mine: HashSet<String> = rows.iter().map(|r| r.s("asked").to_lowercase()).collect();
        for r in q.soft("Panels people added to incidents of this kind", "SELECT asked, COUNT(*) AS n FROM netops_panels\nWHERE category = :c AND source = 'person' AND incident != :id\nGROUP BY asked ORDER BY n DESC LIMIT 8", json!({"c": inc.s("category"), "id": id})).await {
            if !r.s("asked").is_empty() && !mine.contains(&r.s("asked").to_lowercase()) {
                often.push(json!({"ask": r.s("asked"), "n": r.i("n")}));
            }
        }
    }
    let removed = rows.iter().filter(|p| p.get("removed").and_then(Value::as_bool) == Some(true)).count();
    q.done(json!({"incident": id, "category": inc.s("category"), "panels": out, "often": often, "removed": removed,
        "examples": ["loss on LON-NYC last 2 hours", "cpu on cr1-fra", "logs \"LINK-3-UPDOWN\"", "who sends the most", "customers on ag1-ams", "optical power", "changes last 3 days"]}))
}

/// The room's panels: those kept, and the template's for its kind and
/// cause that are not kept yet (a removed panel stays removed). `keep`
/// writes those, which a change to the board does first, so that every
/// panel it names is a row.
async fn board(q: &Q, e: &Engine, inc: &Row, now: i64, keep: bool) -> std::result::Result<Vec<Row>, ApiError> {
    let id = inc.s("id");
    let mut rows = q.rows("Its panels", "SELECT * FROM netops_panels WHERE incident = :id ORDER BY pos LIMIT 200", json!({"id": id})).await?;
    let have: HashSet<String> = rows.iter().map(|r| r.s("tkey")).collect();
    let mut pos = rows.iter().map(|r| r.i("pos")).max().unwrap_or(0);
    let mut fresh = Vec::new();
    for spec in template(e, inc) {
        if !have.contains(&spec.key) {
            pos += 10;
            let common = ["link:loss", "logs:kinds", "customers"].contains(&spec.key.as_str());
            fresh.push(panel_row(inc, &spec, pos, if common { "template" } else { "cause" }, if common { "the room" } else { "root-cause agent" }, "", format!("{id}:{}", spec.key), now));
        }
    }
    if !fresh.is_empty() {
        if keep {
            e.db.upsert("netops_panels", fresh.clone()).await?;
        }
        rows.extend(fresh.into_iter().filter_map(|v| v.as_object().cloned()));
        rows.sort_by_key(|r| r.i("pos"));
    }
    Ok(rows)
}

async fn panel_add(State(svc): State<Shared>, Path(id): Path<String>, headers: HeaderMap, Json(b): Json<Value>) -> Answer {
    let q = Q::new(&svc);
    let e = &svc.engine;
    let inc = load(&q, &id).await?;
    let ask = b["ask"].as_str().unwrap_or("").trim();
    if ask.is_empty() {
        return Err(bad("ask is what the panel should show: a sentence or a SELECT"));
    }
    let who = &crate::api::actor(&headers, b["who"].as_str(), "NOC engineer");
    let mut spec = read_ask(e, &inc, ask).map_err(|m| bad(&m))?;
    if let Some(t) = b["title"].as_str().filter(|t| !t.trim().is_empty()) {
        spec.title = t.trim().to_string();
    }
    let now = q.last();
    board(&q, e, &inc, now, true).await?;
    let n = e.next_id();
    let row = panel_row(&inc, &spec, 0, "person", who, ask, format!("{id}:p{now}-{n}"), now);
    // The query has to run before the panel joins the board.
    let probe = row.as_object().cloned().unwrap_or_default();
    let got = e.db.traced(&q.trace, &spec.title, &spec.sql, window(&inc, &probe, now)).await.map_err(|x| bad(&format!("The query did not run: {x}")))?;
    // New panels go to the top: it is what someone is looking at now.
    let first = q.rows("The first panel's place", "SELECT MIN(pos) AS p FROM netops_panels WHERE incident = :id", json!({"id": id})).await?.first().map(|r| r.i("p")).unwrap_or(10);
    let mut row = row;
    row["pos"] = json!(first - 10);
    e.db.upsert("netops_panels", vec![row.clone()]).await?;
    let _ = e.timeline(&id, crate::sim::now(), "panel", who, &format!("Added the panel \"{}\" to the room", spec.title), row["id"].as_str().unwrap_or("")).await;
    q.done(json!({"panel": row, "rows": got.len(), "understood": spec.title}))
}

async fn panel_act(State(svc): State<Shared>, Path((id, panel, action)): Path<(String, String, String)>, headers: HeaderMap, body: Option<Json<Value>>) -> Answer {
    let e = &svc.engine;
    let q = Q::new(&svc);
    let who = crate::api::actor(&headers, body.as_ref().and_then(|b| b["who"].as_str()), "NOC engineer");
    let inc = load(&q, &id).await?;
    board(&q, e, &inc, q.last(), true).await?;
    let rows = q.rows("The panel", "SELECT * FROM netops_panels WHERE id = :p AND incident = :id", json!({"p": panel, "id": id})).await?;
    let Some(p) = rows.into_iter().next() else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no panel {panel}"))) };
    let mut row = Value::Object(p.clone());
    match action.as_str() {
        "remove" => {
            row["removed"] = json!(true);
            let _ = e.timeline(&id, crate::sim::now(), "panel", &who, &format!("Removed the panel \"{}\" from the room", p.s("title")), &panel).await;
        }
        "restore" => row["removed"] = json!(false),
        "top" => {
            let first = q.rows("The first panel's place", "SELECT MIN(pos) AS p FROM netops_panels WHERE incident = :id", json!({"id": id})).await?.first().map(|r| r.i("p")).unwrap_or(10);
            row["pos"] = json!(first - 10);
        }
        _ => return Err(bad("the action is remove, restore or top")),
    }
    e.db.upsert("netops_panels", vec![row.clone()]).await?;
    Ok(Json(json!({"panel": row})))
}
