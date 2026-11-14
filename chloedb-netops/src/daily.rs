//! The work between incidents, which is most of a shift: the handover and
//! the list of what waits, looking a thing up by whatever a ticket calls it,
//! answering "is it the network?" for a customer, acknowledging and
//! silencing alerts, carrying a change out with the network's state before
//! and after it, and carriers' maintenance notices checked against the
//! other plane, the change calendar and the capacity that is left.
//!
//! As everywhere in the app, each answer is a handful of SQL queries over
//! the `netops_*` tables, and carries them.

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::agents::quoted;
use crate::api::{bad, Answer, ApiError, Shared, Q};
use crate::estate::NODES;
use crate::seed;
use crate::sim::Engine;
use crate::strata::{Result, Row, RowExt};
use crate::telemetry::{iso, parse_iso, round};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/v1/shift", get(shift))
        .route("/v1/shift/notes", post(note_add))
        .route("/v1/shift/notes/:id/done", post(note_done))
        .route("/v1/lookup", get(lookup))
        .route("/v1/activity", get(activity))
        .route("/v1/customers", get(customers))
        .route("/v1/customers/:id", get(customer))
        .route("/v1/alerts/:id/:action", post(alert_act))
        .route("/v1/silences", get(silences).post(silence_add))
        .route("/v1/silences/:id/end", post(silence_end))
        .route("/v1/changes/:id/:action", post(change_act))
        .route("/v1/maintenance", get(maintenance))
        .route("/v1/maintenance/parse", post(maintenance_parse))
        .route("/v1/maintenance/:id/:action", post(maintenance_act))
}

fn who(headers: &HeaderMap, body: &Option<Json<Value>>) -> String {
    crate::api::actor(headers, body.as_ref().and_then(|b| b["who"].as_str()), "NOC engineer")
}

/// An id that a restart does not hand out again: the minute and a counter.
fn uid(e: &Engine, m: i64) -> String {
    format!("{}{:02}", (m / 60) % 100_000, e.next_id() % 100)
}

fn hhmm(t: i64) -> String {
    iso(t)[11..16].to_string()
}

fn nums_of(e: &Engine, devices: &[String]) -> String {
    let n: Vec<String> = devices.iter().filter_map(|d| e.w.device(d)).map(|d| d.num.to_string()).collect();
    if n.is_empty() { "-1".into() } else { n.join(", ") }
}

// ---- silences

/// The silence that covers `alert` at `m`, or nothing.
pub fn silence_for(silences: &BTreeMap<String, Row>, alert: &Row, m: i64) -> String {
    let (entity, site, rule) = (alert.s("entity"), alert.s("site"), alert.s("rule"));
    let devices = alert.list("devices");
    silences
        .values()
        .find(|s| {
            if parse_iso(&s.s("starts")) > m || parse_iso(&s.s("ends")) <= m {
                return false;
            }
            let v = s.s("matcher");
            match s.s("kind").as_str() {
                "device" => entity == v || devices.contains(&v),
                "site" => site == v,
                "rule" => rule == v,
                _ => entity == v,
            }
        })
        .map(|s| s.s("id"))
        .unwrap_or_default()
}

impl Engine {
    /// A silence on one thing, from `starts` to `ends`.
    #[allow(clippy::too_many_arguments)]
    pub async fn silence(&self, kind: &str, matcher: &str, reason: &str, by: &str, starts: i64, ends: i64, source: &str) -> Result<Value> {
        let m = self.st().last_minute;
        let row = json!({"id": format!("SIL-{}", uid(self, m)), "kind": kind, "matcher": matcher, "reason": reason, "created_by": by, "created": iso(m),
            "starts": iso(starts), "ends": iso(ends), "source": source, "status": "active"});
        self.db.upsert("netops_silences", vec![row.clone()]).await?;
        if let Value::Object(map) = &row {
            self.st().silences.insert(map.s("id"), map.clone());
        }
        self.requiet(m).await?;
        Ok(row)
    }

    /// Ends silences: one by id, or all that a change or a notice made.
    pub async fn end_silences(&self, id: Option<&str>, source: Option<&str>) -> Result<usize> {
        let m = self.st().last_minute;
        let gone: Vec<Row> = {
            let mut st = self.st();
            let ids: Vec<String> = st.silences.values().filter(|s| id.is_some_and(|i| s.s("id") == i) || source.is_some_and(|x| s.s("source") == x)).map(|s| s.s("id")).collect();
            ids.iter().filter_map(|i| st.silences.remove(i)).collect()
        };
        let n = gone.len();
        let rows: Vec<Value> = gone
            .into_iter()
            .map(|mut s| {
                s.insert("status".into(), json!("ended"));
                if parse_iso(&s.s("ends")) > m {
                    s.insert("ends".into(), json!(iso(m)));
                }
                Value::Object(s)
            })
            .collect();
        self.db.upsert("netops_silences", rows).await?;
        self.requiet(m).await?;
        Ok(n)
    }

    /// Alerts outside incidents take the state the silences give them now,
    /// without waiting for the next evaluation.
    async fn requiet(&self, m: i64) -> Result<()> {
        let rows: Vec<Value> = {
            let mut st = self.st();
            let silences = st.silences.clone();
            st.alerts
                .values_mut()
                .filter(|a| a.s("incident").is_empty())
                .filter_map(|a| {
                    let by = silence_for(&silences, a, m);
                    let want = if by.is_empty() { "firing" } else { "silenced" };
                    if a.s("status") == want && a.s("silenced") == by {
                        return None;
                    }
                    a.insert("status".into(), json!(want));
                    a.insert("silenced".into(), json!(by));
                    Some(Value::Object(a.clone()))
                })
                .collect()
        };
        self.db.upsert("netops_alerts", rows).await?;
        Ok(())
    }

    /// Each minute: silences that ran out end. Every ten: planned changes
    /// whose window passed were carried out by their owners (the simulated
    /// rest of the team), and the calendar and the carriers' notices get
    /// new entries when few lie ahead, so that a network left running for
    /// weeks still has a week to plan.
    pub async fn housekeeping(&self, m: i64) -> Result<()> {
        let over: Vec<Row> = {
            let mut st = self.st();
            let ids: Vec<String> = st.silences.values().filter(|s| parse_iso(&s.s("ends")) <= m).map(|s| s.s("id")).collect();
            ids.iter().filter_map(|i| st.silences.remove(i)).collect()
        };
        if !over.is_empty() {
            let rows = over.into_iter().map(|mut s| { s.insert("status".into(), json!("ended")); Value::Object(s) }).collect();
            self.db.upsert("netops_silences", rows).await?;
        }
        // Notices a mail connector wrote as it found them (the text and a
        // status only) are read here, so they get a circuit and a window.
        let unread = self.db.query("SELECT * FROM netops_maintenance WHERE status = 'new' LIMIT 200", json!({})).await?;
        let mut read = Vec::new();
        for n in unread.iter().filter(|n| n.s("link").is_empty() && !n.s("raw").is_empty()) {
            let Ok(Value::Object(mut found)) = read_notice(self, &n.s("raw")) else { continue };
            found.remove("notes");
            let mut row = n.clone();
            for (k, v) in found {
                let kept = row.get(&k).and_then(|x| x.as_str()).is_some_and(|x| !x.is_empty());
                if !matches!(k.as_str(), "id" | "received" | "raw") && !kept {
                    row.insert(k, v);
                }
            }
            read.push(Value::Object(row));
        }
        if !read.is_empty() {
            self.db.upsert("netops_maintenance", read).await?;
        }
        if m % 600 != 0 {
            return Ok(());
        }
        self.db.sql(&format!("UPDATE netops_maintenance SET status = 'completed' WHERE status != 'completed' AND window_end < '{}'", iso(m))).await?;
        if !self.opts.simulate {
            // A real network's changes are closed by the people who made
            // them, and nothing is invented for it.
            return Ok(());
        }
        let stale = self.db.query("SELECT id, window_start FROM netops_changes WHERE status IN ('approved', 'scheduled') AND window_end < :now LIMIT 50", json!({"now": iso(m)})).await?;
        for c in stale {
            self.db.sql(&format!("UPDATE netops_changes SET status = 'completed', outcome = 'as planned', executed = '{}' WHERE id = '{}'", iso(parse_iso(&c.s("window_start")) + 600), c.s("id").replace('\'', ""))).await?;
        }
        let ahead = self.db.query("SELECT COUNT(*) AS n FROM netops_changes WHERE window_start > :now", json!({"now": iso(m)})).await?;
        if ahead.first().map_or(0, |r| r.i("n")) < 8 {
            self.db.upsert("netops_changes", seed::more_changes(&self.w, m)).await?;
        }
        let notices = self.db.query("SELECT COUNT(*) AS n FROM netops_maintenance WHERE window_start > :now", json!({"now": iso(m)})).await?;
        if notices.first().map_or(0, |r| r.i("n")) < 2 {
            self.db.upsert("netops_maintenance", seed::more_maintenance(&self.w, m)).await?;
        }
        Ok(())
    }
}

async fn silences(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let rows = q.rows("Silences, newest first", "SELECT * FROM netops_silences ORDER BY created DESC LIMIT 100", json!({})).await?;
    let quiet = q.rows("Alerts they keep quiet now", "SELECT silenced, COUNT(*) AS n FROM netops_alerts\nWHERE status = 'silenced'\nGROUP BY silenced LIMIT 200", json!({})).await?;
    q.done(json!({"silences": rows, "quiet": quiet, "at": iso(q.last())}))
}

async fn silence_add(State(svc): State<Shared>, headers: HeaderMap, Json(b): Json<Value>) -> Answer {
    let e = &svc.engine;
    let kind = b["kind"].as_str().unwrap_or("device");
    if !["device", "link", "site", "rule", "service"].contains(&kind) {
        return Err(bad("kind is device, link, site, rule or service"));
    }
    let Some(matcher) = b["matcher"].as_str().map(str::trim).filter(|s| !s.is_empty()) else { return Err(bad("matcher is what to silence")) };
    let reason = b["reason"].as_str().unwrap_or("").trim();
    if reason.is_empty() {
        return Err(bad("a silence needs a reason: the next shift will ask"));
    }
    let hours = b["hours"].as_f64().unwrap_or(2.0).clamp(0.25, 72.0);
    let m = e.st().last_minute;
    let row = e.silence(kind, matcher, reason, &crate::api::actor(&headers, b["who"].as_str(), "NOC engineer"), m, m + (hours * 3600.0) as i64, "manual").await?;
    Ok(Json(row))
}

async fn silence_end(State(svc): State<Shared>, Path(id): Path<String>) -> Answer {
    let n = svc.engine.end_silences(Some(&id), None).await?;
    if n == 0 {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no active silence {id}")));
    }
    Ok(Json(json!({"id": id, "status": "ended"})))
}

async fn alert_act(State(svc): State<Shared>, Path((id, action)): Path<(String, String)>, headers: HeaderMap, body: Option<Json<Value>>) -> Answer {
    let e = &svc.engine;
    let by = who(&headers, &body);
    let m = e.st().last_minute;
    let rows = e.db.query("SELECT * FROM netops_alerts WHERE id = :id", json!({"id": id})).await?;
    let Some(mut a) = rows.into_iter().next() else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no alert {id}"))) };
    match action.as_str() {
        "ack" => {
            a.insert("acked_by".into(), json!(by));
            a.insert("acked_at".into(), json!(iso(m)));
            a.insert("assignee".into(), json!(body.as_ref().and_then(|b| b["assignee"].as_str()).unwrap_or(&by)));
        }
        "unack" => {
            a.insert("acked_by".into(), json!(""));
            a.insert("acked_at".into(), Value::Null);
            a.insert("assignee".into(), json!(""));
        }
        _ => return Err(bad("the action is ack or unack")),
    }
    {
        let mut st = e.st();
        let key = format!("{}:{}", a.s("rule"), a.s("entity"));
        if let Some(live) = st.alerts.get_mut(&key).filter(|l| l.s("id") == id) {
            for k in ["acked_by", "acked_at", "assignee"] {
                live.insert(k.into(), a.get(k).cloned().unwrap_or(Value::Null));
            }
        }
    }
    e.db.upsert("netops_alerts", vec![Value::Object(a.clone())]).await?;
    Ok(Json(Value::Object(a)))
}

// ---- carrying a change out: the network before and after

/// What the telemetry said of `devices` in the minute `t`: each device's
/// reachability, CPU and BGP sessions, and the links on it.
async fn state_at(q: &Q, devices: &[String], t: i64, what: &str) -> std::result::Result<Vec<Value>, ApiError> {
    let w = &q.e.w;
    let t = t - t.rem_euclid(60);
    let mut links: HashMap<String, Vec<String>> = HashMap::new();
    for d in devices {
        if let Some(di) = w.devices.iter().position(|x| &x.id == d) {
            links.insert(d.clone(), w.links_of(di).into_iter().filter(|&li| w.links[li].live && w.links[li].documented).map(|li| w.links[li].id.clone()).collect());
        }
    }
    let all: Vec<String> = links.values().flatten().cloned().collect::<HashSet<_>>().into_iter().collect();
    let p = json!({"a": iso(t), "b": iso(t + 60)});
    let dm = q.rows(&format!("The devices {what}"), &format!("SELECT device, cpu, bgp_up, up FROM netops_device_metrics\nWHERE ts >= :a AND ts < :b AND device IN ({})\nLIMIT 200", quoted(devices)), p.clone()).await?;
    let lm = if all.is_empty() { Vec::new() } else { q.rows(&format!("Their links {what}"), &format!("SELECT link, up, loss, util FROM netops_link_metrics\nWHERE ts >= :a AND ts < :b AND link IN ({})\nLIMIT 2000", quoted(&all)), p).await? };
    let by_link: HashMap<String, &Row> = lm.iter().map(|r| (r.s("link"), r)).collect();
    Ok(devices
        .iter()
        .map(|d| {
            let row = dm.iter().find(|r| &r.s("device") == d);
            let mine = links.get(d).cloned().unwrap_or_default();
            let seen: Vec<&&Row> = mine.iter().filter_map(|l| by_link.get(l)).collect();
            json!({"device": d, "seen": row.is_some(), "up": row.map_or(0, |r| r.i("up")), "cpu": row.map_or(0.0, |r| r.f("cpu")), "bgp_up": row.map_or(0, |r| r.i("bgp_up")),
                "links_up": seen.iter().filter(|r| r.i("up") == 1).count(), "links": mine.len(), "worst_loss": seen.iter().filter(|r| r.i("up") == 1).map(|r| r.f("loss")).fold(0.0, f64::max)})
        })
        .collect())
}

/// The checks a change is verified by: each device's state after against
/// its state before.
fn compare(before: &[Value], after: &[Value]) -> (Vec<Value>, bool) {
    let mut out = Vec::new();
    let mut all = true;
    for b in before {
        let Some(a) = after.iter().find(|a| a["device"] == b["device"]) else { continue };
        let f = |v: &Value, k: &str| v[k].as_f64().unwrap_or(0.0);
        let mut push = |check: &str, bv: Value, av: Value, ok: bool, unit: &str| {
            all &= ok;
            out.push(json!({"device": b["device"], "check": check, "before": bv, "after": av, "ok": ok, "unit": unit}));
        };
        push("Reachable", json!(if f(b, "up") > 0.0 { "yes" } else { "no" }), json!(if f(a, "up") > 0.0 { "yes" } else { "no" }), f(a, "up") >= f(b, "up"), "");
        if f(b, "bgp_up") > 0.0 || f(a, "bgp_up") > 0.0 {
            push("BGP sessions established", b["bgp_up"].clone(), a["bgp_up"].clone(), f(a, "bgp_up") >= f(b, "bgp_up"), "");
        }
        push("Links up", json!(format!("{} of {}", b["links_up"], b["links"])), json!(format!("{} of {}", a["links_up"], a["links"])), f(a, "links_up") >= f(b, "links_up"), "");
        push("Worst packet loss on its links", json!(round(f(b, "worst_loss"), 2)), json!(round(f(a, "worst_loss"), 2)), f(a, "worst_loss") <= f(b, "worst_loss") + 0.3, "%");
        push("Control-plane CPU", json!(round(f(b, "cpu"), 0)), json!(round(f(a, "cpu"), 0)), f(a, "up") == 0.0 || f(a, "cpu") <= (f(b, "cpu") * 1.6).max(f(b, "cpu") + 25.0), "%");
    }
    (out, all)
}

/// A change's before and after, read back from the telemetry: for one in
/// progress, the minute before it began against now; for one that was
/// carried out while the telemetry is kept, two minutes before against
/// five after.
pub(crate) async fn checks_for(q: &Q, c: &Row) -> std::result::Result<Value, ApiError> {
    let m = q.last();
    let devices = c.list("devices");
    let status = c.s("status");
    let (t0, t1) = if status == "in_progress" && !c.s("started").is_empty() {
        (parse_iso(&c.s("started")) - 60, m)
    } else if !c.s("started").is_empty() && !c.s("verified").is_empty() {
        (parse_iso(&c.s("started")) - 60, parse_iso(&c.s("verified")))
    } else if !c.s("executed").is_empty() {
        let x = parse_iso(&c.s("executed"));
        (x - 120, (x + 300).min(m))
    } else {
        return Ok(Value::Null);
    };
    if devices.is_empty() || t0 < m - 3 * 86400 || t1 <= t0 {
        return Ok(Value::Null);
    }
    let before = state_at(q, &devices, t0, "before").await?;
    if before.iter().all(|b| b["seen"] == false) {
        return Ok(Value::Null);
    }
    let after = state_at(q, &devices, t1, "after").await?;
    let (checks, ok) = compare(&before, &after);
    Ok(json!({"before_at": iso(t0 - t0.rem_euclid(60)), "after_at": iso(t1 - t1.rem_euclid(60)), "checks": checks, "ok": ok, "live": status == "in_progress"}))
}

async fn change_act(State(svc): State<Shared>, Path((id, action)): Path<(String, String)>, headers: HeaderMap, body: Option<Json<Value>>) -> Answer {
    let q = Q::new(&svc);
    let e = &svc.engine;
    let by = who(&headers, &body);
    let m = q.last();
    let rows = q.rows("The change", "SELECT * FROM netops_changes WHERE id = :id", json!({"id": id})).await?;
    let Some(mut c) = rows.into_iter().next() else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no change {id}"))) };
    let status = c.s("status");
    let source = format!("change:{id}");
    match action.as_str() {
        "start" => {
            if !["approved", "scheduled"].contains(&status.as_str()) {
                return Err(bad(format!("{id} is {status}: only a planned change can be started")));
            }
            let end = parse_iso(&c.s("window_end")).max(m + 3600).min(m + 6 * 3600);
            for d in c.list("devices") {
                e.silence("device", &d, &format!("{id} in progress: {}", c.s("title")), &by, m, end, &source).await?;
            }
            c.insert("status".into(), json!("in_progress"));
            c.insert("started".into(), json!(iso(m)));
            c.insert("started_by".into(), json!(by));
        }
        "verify" | "complete" => {
            if status != "in_progress" {
                return Err(bad(format!("{id} is {status}: start it first")));
            }
            c.insert("verified".into(), json!(iso(m)));
            if action == "complete" {
                let checks = checks_for(&q, &c).await?;
                let ok = checks["ok"].as_bool().unwrap_or(true);
                if !ok && body.as_ref().and_then(|b| b["force"].as_bool()) != Some(true) {
                    return Err(bad("the network is not as it was before the change: roll it back, or complete it with force once you know why"));
                }
                c.insert("status".into(), json!("completed"));
                c.insert("executed".into(), c.get("started").cloned().unwrap_or(Value::Null));
                c.insert("outcome".into(), json!(if ok { "as planned, verified against the state before" } else { "completed with differences from the state before" }));
                e.end_silences(None, Some(&source)).await?;
            }
        }
        "rollback" => {
            if status != "in_progress" {
                return Err(bad(format!("{id} is {status}: nothing to roll back")));
            }
            c.insert("status".into(), json!("rolled_back"));
            c.insert("verified".into(), json!(iso(m)));
            c.insert("executed".into(), c.get("started").cloned().unwrap_or(Value::Null));
            c.insert("outcome".into(), json!(format!("rolled back by {by} after the checks")));
            e.end_silences(None, Some(&source)).await?;
        }
        _ => return Err(bad("the action is start, verify, complete or rollback")),
    }
    c.remove("emb");
    e.db.upsert("netops_changes", vec![Value::Object(c.clone())]).await?;
    let checks = checks_for(&q, &c).await?;
    q.done(json!({"change": c, "checks": checks}))
}

// ---- carriers' maintenance

/// Time zones as carriers write them, by their offset from UTC in minutes.
/// A notice in a zone that is not here is refused rather than guessed at.
const ZONES: &[(&str, i64)] = &[
    ("UTC", 0), ("GMT", 0), ("Z", 0), ("WET", 0), ("BST", 60), ("WEST", 60), ("CET", 60), ("MET", 60), ("CEST", 120), ("MEST", 120), ("EET", 120), ("EEST", 180), ("MSK", 180),
    ("IST", 330), ("SGT", 480), ("HKT", 480), ("JST", 540), ("AEST", 600), ("AEDT", 660),
    ("EST", -300), ("EDT", -240), ("CST", -360), ("CDT", -300), ("MST", -420), ("MDT", -360), ("PST", -480), ("PDT", -420),
];

/// One time in a notice: where it was found and what it says.
#[derive(Debug, Clone, PartialEq)]
struct Stamp {
    at: usize,
    line: usize,
    /// Seconds since the epoch of the wall time, before any zone is applied.
    wall: i64,
    /// Whether it carried a date (or only a time of day).
    dated: bool,
    /// Its offset from UTC in minutes, when it named one.
    offset: Option<i64>,
    /// A zone it named that is not known.
    unknown: Option<String>,
}

/// The offset a zone written after a time stands for: `Z`, `+01:00`,
/// `+0100`, `UTC+1`, `(UTC)`, `CET`. `Ok(None)` when nothing follows;
/// `Err` with the name when it is a zone this reader does not know.
fn zone_after(rest: &str, iso_like: bool) -> (std::result::Result<Option<i64>, String>, usize) {
    let t = rest.trim_start_matches([' ', '(']);
    let skip = rest.len() - t.len();
    let sign_offset = |s: &str| -> Option<(i64, usize)> {
        let b = s.as_bytes();
        let sign = match b.first()? { b'+' => 1, b'-' => -1, _ => return None };
        let digits: String = s[1..].chars().take_while(|c| c.is_ascii_digit() || *c == ':').collect();
        let plain = digits.replace(':', "");
        let (h, m) = match plain.len() {
            1 | 2 => (plain.parse::<i64>().ok()?, 0),
            3 | 4 => (plain[..plain.len() - 2].parse::<i64>().ok()?, plain[plain.len() - 2..].parse::<i64>().ok()?),
            _ => return None,
        };
        (h <= 14 && m < 60).then_some((sign * (h * 60 + m), 1 + digits.len()))
    };
    if t.starts_with('Z') && !t[1..].starts_with(|c: char| c.is_ascii_alphabetic()) {
        return (Ok(Some(0)), skip + 1);
    }
    if let Some((off, n)) = sign_offset(t) {
        // "-05:00" right after an ISO time is an offset; after "01:00" it
        // is more often the end of a range ("01:00-05:00").
        if t.starts_with('+') || (iso_like && skip == 0) {
            return (Ok(Some(off)), skip + n);
        }
    }
    let word: String = t.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    const NOT_ZONES: &[&str] = &["TO", "AND", "UNTIL", "TILL", "THRU", "ON", "AT", "FOR", "THE", "OF", "IN", "BY", "OR", "LOCAL", "HRS", "HOURS", "AM", "PM"];
    if word.len() < 2 || word.len() > 5 || !word.chars().all(|c| c.is_ascii_uppercase()) || NOT_ZONES.contains(&word.as_str()) {
        return (Ok(None), 0);
    }
    let after = &t[word.len()..];
    let base = ZONES.iter().find(|z| z.0 == word).map(|z| z.1);
    match base {
        Some(b) => match sign_offset(after) {
            // UTC+1, GMT-5
            Some((off, n)) if b == 0 => (Ok(Some(off)), skip + word.len() + n),
            _ => (Ok(Some(b)), skip + word.len()),
        },
        None => (Err(word), skip),
    }
}

/// Every time in the text: `2026-11-14 01:00`, `2026-11-14T01:00:00+01:00`,
/// and a time of day alone (`05:00 UTC`) for an end on the start's day.
fn stamps(text: &str) -> Vec<Stamp> {
    let bytes = text.as_bytes();
    let line_of = |i: usize| text[..i].matches('\n').count();
    let dg = |i: usize| bytes.get(i).is_some_and(|b| b.is_ascii_digit());
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let dated = boundary && i + 16 <= bytes.len() && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15].iter().all(|&k| dg(i + k)) && bytes[i + 4] == b'-' && bytes[i + 7] == b'-' && (bytes[i + 10] == b' ' || bytes[i + 10] == b'T') && bytes[i + 13] == b':';
        let timed = !dated && boundary && i + 5 <= bytes.len() && dg(i) && dg(i + 1) && bytes[i + 2] == b':' && dg(i + 3) && dg(i + 4) && !dg(i + 5);
        if !dated && !timed {
            i += 1;
            continue;
        }
        let (date, hm, mut end) = if dated { (&text[i..i + 10], &text[i + 11..i + 16], i + 16) } else { ("1970-01-01", &text[i..i + 5], i + 5) };
        // Seconds, when given.
        if bytes.get(end) == Some(&b':') && dg(end + 1) && dg(end + 2) {
            end += 3;
        }
        let iso_like = dated && (bytes[i + 10] == b'T' || end > i + 16);
        let mut wall = parse_iso(&format!("{date}T{hm}:00Z"));
        // 1:00 PM, 12:30 am.
        let tail = text[end..].trim_start();
        let ampm = tail.get(..2).map(|x| x.to_ascii_lowercase()).filter(|x| (x == "am" || x == "pm") && !tail[2..].starts_with(|c: char| c.is_ascii_alphabetic()));
        if let Some(x) = ampm {
            let hour = hm[..2].parse::<i64>().unwrap_or(0);
            if x == "pm" && hour < 12 {
                wall += 12 * 3600;
            } else if x == "am" && hour == 12 {
                wall -= 12 * 3600;
            }
            end += text[end..].len() - tail.len() + 2;
        }
        let (zone, n) = zone_after(&text[end..], iso_like);
        let (offset, unknown) = match zone {
            Ok(o) => (o, None),
            Err(w) => (None, Some(w)),
        };
        if wall > 0 || !dated {
            out.push(Stamp { at: i, line: line_of(i), wall, dated, offset, unknown });
        }
        i = end + n;
    }
    out
}

/// What a line is about, by the label it starts with.
fn label(line: &str) -> &'static str {
    let l = line.trim().trim_start_matches(['*', '-', '>', ' ']).to_lowercase();
    let head: String = l.split([':', '\t']).next().unwrap_or("").trim().to_string();
    let starts = |w: &[&str]| w.iter().any(|x| head == *x || head.starts_with(&format!("{x} ")));
    if ["sent", "date", "date sent", "sent on", "received", "issued", "published", "created", "notified", "from", "to", "cc"].contains(&head.as_str()) {
        return "sent";
    }
    if starts(&["start", "starts", "begin", "begins", "start time", "start date", "window start", "maintenance start", "planned start", "work start"]) || head == "start (utc)" {
        return "start";
    }
    if starts(&["end", "ends", "finish", "finishes", "end time", "end date", "window end", "maintenance end", "planned end", "work end", "until", "completion"]) {
        return "end";
    }
    if starts(&["window", "maintenance window", "time", "when", "schedule", "scheduled"]) {
        return "window";
    }
    ""
}

/// A carrier's email, read: the circuit it names (one of ours, the first
/// it names), its window and what to expect. The window is the one on the
/// lines labelled Start and End (or a Window line), each time in the zone
/// it is written in; the date the email was sent is not part of it. A time
/// with no zone takes the zone the notice names elsewhere ("All times
/// UTC"), else UTC, and the answer says so.
fn read_notice(e: &Engine, text: &str) -> std::result::Result<Value, String> {
    let w = &e.w;
    let upper = text.to_uppercase();
    let Some((_, l)) = w.links.iter().filter(|l| !l.circuit.is_empty() && l.documented).filter_map(|l| upper.find(&l.circuit.to_uppercase()).map(|at| (at, l))).min_by_key(|x| x.0) else {
        return Err("no circuit of ours is named in this text: it should carry a service or circuit id such as the ones under Carriers".into());
    };
    let (start, end, notes) = notice_window(text)?;
    let line = |label: &str| text.lines().find_map(|ln| ln.trim().strip_prefix(label).map(|v| v.trim_start_matches([':', ' ']).trim().to_string())).unwrap_or_default();
    let expected = [line("Expected impact"), line("Impact")].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
    let low = text.to_lowercase();
    let impact = if low.contains("at risk") && !low.contains("outage") { "at risk" } else { "outage" };
    let subject = line("Subject");
    let reference = notice_reference(text);
    let m = e.st().last_minute;
    Ok(json!({"id": format!("MNT-{}", uid(e, e.st().last_minute)), "provider": l.provider, "reference": reference, "circuit": l.circuit, "link": l.id, "a_site": w.devices[l.a].site, "z_site": w.devices[l.b].site,
        "window_start": iso(start), "window_end": iso(end), "impact": impact, "expected": expected, "status": "new", "received": iso(m),
        "subject": if subject.is_empty() { format!("Maintenance on {}", l.circuit) } else { subject }, "raw": text, "acked_by": "", "change_id": "", "notes": notes}))
}

/// The carrier's own reference: a Reference, Ticket or Maintenance ID line,
/// or "Ref PW-88213" anywhere (a subject often carries it).
fn notice_reference(text: &str) -> String {
    for ln in text.lines() {
        let t = ln.trim();
        let low = t.to_lowercase();
        for label in ["reference", "our reference", "ticket", "maintenance id", "change id", "ref"] {
            if let Some(rest) = low.strip_prefix(label) {
                if rest.starts_with([':', ' ', '#', '.']) {
                    let v = t[label.len()..].trim_start_matches([':', ' ', '#', '.']).split_whitespace().next().unwrap_or("");
                    if v.chars().any(|c| c.is_ascii_digit()) {
                        return v.trim_end_matches([',', ';', ')', ']']).to_string();
                    }
                }
            }
        }
    }
    // "… Ref PW-88213 …", "[Ref: X-1]", "Reference #4411" in a sentence or a subject.
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, wd) in words.iter().enumerate() {
        let k = wd.trim_start_matches(['[', '(']).trim_end_matches([':', '#', '.']).to_lowercase();
        if matches!(k.as_str(), "ref" | "reference" | "ticket") {
            if let Some(v) = words.get(i + 1).map(|v| v.trim_start_matches([':', '#']).trim_end_matches([',', ';', ')', ']', '.'])) {
                if v.chars().any(|c| c.is_ascii_digit()) && v.len() >= 3 {
                    return v.to_string();
                }
            }
        }
    }
    String::new()
}

/// The window of a notice, in UTC seconds, and what the reader assumed.
fn notice_window(text: &str) -> std::result::Result<(i64, i64, Vec<String>), String> {
    let all = stamps(text);
    if let Some(s) = all.iter().find(|s| s.unknown.is_some()) {
        return Err(format!("the time zone {} is not one this reader knows: write the times in UTC or with an offset such as +01:00", s.unknown.clone().unwrap_or_default()));
    }
    let lines: Vec<&str> = text.lines().collect();
    let kind = |s: &Stamp| label(lines.get(s.line).copied().unwrap_or(""));
    let mut notes = Vec::new();
    // Times on the Start and End lines; else a Window line's two; else the
    // first two that are not when it was sent.
    let first = |k: &str| all.iter().find(|s| kind(s) == k);
    let (a, b) = match (first("start"), first("end")) {
        (Some(a), Some(b)) => (a.clone(), b.clone()),
        (a, b) => {
            let window: Vec<&Stamp> = all.iter().filter(|s| kind(s) == "window").collect();
            let rest: Vec<&Stamp> = all.iter().filter(|s| kind(s) != "sent").collect();
            match (a.or(window.first().copied()), b.or(window.get(1).copied())) {
                (Some(a), Some(b)) if a.at != b.at => (a.clone(), b.clone()),
                _ if rest.len() >= 2 => (rest[0].clone(), rest[1].clone()),
                _ => return Err("no window found: the text should give a start and an end, such as Start: 2026-11-14 01:00 UTC and End: 2026-11-14 05:00 UTC".into()),
            }
        }
    };
    if !a.dated {
        return Err("the start has no date: it should read like 2026-11-14 01:00 UTC".into());
    }
    // A time with no zone of its own: the zone the notice names for all its
    // times (the first known one written as a word), else UTC.
    let general = text.split(|c: char| !c.is_ascii_alphanumeric()).find_map(|w| ZONES.iter().find(|z| z.0 == w && z.0 != "Z").map(|z| (z.0, z.1)));
    let zone = |s: &Stamp, notes: &mut Vec<String>| -> i64 {
        match s.offset {
            Some(o) => o,
            None => match general {
                Some((name, o)) => {
                    let n = format!("a time without a zone was read as {name}, the zone the notice names");
                    if !notes.contains(&n) {
                        notes.push(n);
                    }
                    o
                }
                None => {
                    let n = "no time zone given: read as UTC".to_string();
                    if !notes.contains(&n) {
                        notes.push(n);
                    }
                    0
                }
            },
        }
    };
    let oa = zone(&a, &mut notes);
    let ob = if b.offset.is_none() && !b.dated { a.offset.unwrap_or(oa) } else { zone(&b, &mut notes) };
    let start = a.wall - oa * 60;
    let mut end = if b.dated { b.wall } else { a.wall - a.wall.rem_euclid(86400) + b.wall } - ob * 60;
    if !b.dated && end <= start {
        end += 86400;
        notes.push("the end has no date and is earlier than the start: read as the next day".into());
    }
    if end <= start {
        return Err(format!("the end ({} UTC) is not after the start ({} UTC)", iso(end).replace('T', " ").trim_end_matches(":00Z"), iso(start).replace('T', " ").trim_end_matches(":00Z")));
    }
    if end - start > 14 * 86400 {
        return Err("the window is longer than two weeks: check its start and end".into());
    }
    Ok((start, end, notes))
}

/// What each notice that is not over collides with: the other plane's own
/// maintenance, a change on a router that carries the other plane, and the
/// traffic the other plane would have to carry alone.
async fn conflicts(q: &Q, notices: &[Row]) -> std::result::Result<HashMap<String, Vec<Value>>, ApiError> {
    let e = &q.e;
    let w = &e.w;
    let m = q.last();
    let ahead: Vec<&Row> = notices.iter().filter(|n| n.s("status") != "completed" && parse_iso(&n.s("window_end")) > m).collect();
    let mut out: HashMap<String, Vec<Value>> = HashMap::new();
    if ahead.is_empty() {
        return Ok(out);
    }
    let last = ahead.iter().map(|n| n.s("window_end")).max().unwrap_or_default();
    let changes = q.rows("Planned changes in the notices' windows", "SELECT id, title, window_start, window_end, devices, status, owner FROM netops_changes\nWHERE status IN ('approved', 'scheduled', 'in_progress') AND window_start < :last AND window_end > :now\nLIMIT 200", json!({"last": last, "now": iso(m)})).await?;
    let mut ids: Vec<String> = Vec::new();
    for n in &ahead {
        if let Some(li) = w.link_by_id(&n.s("link")) {
            ids.push(w.links[li].id.clone());
            if let Some(s) = w.links[li].sibling {
                ids.push(w.links[s].id.clone());
            }
        }
    }
    let peaks = q.rows("Each plane's peak over the last day", &format!("SELECT link, MAX(util) AS peak FROM netops_link_metrics\nWHERE ts >= :since AND up = 1 AND link IN ({})\nGROUP BY link LIMIT 200", quoted(&ids)), json!({"since": iso(m - 86400)})).await?;
    let peak = |id: &str| peaks.iter().find(|r| r.s("link") == id).map_or(0.0, |r| r.f("peak"));
    let overlap = |a: &Row, s: &str, t: &str| a.s("window_start").as_str() < t && s < a.s("window_end").as_str();
    for n in &ahead {
        let Some(li) = w.link_by_id(&n.s("link")) else { continue };
        let l = &w.links[li];
        let mut found = Vec::new();
        let span = format!("{}-{}", n.s("a_site").to_uppercase(), n.s("z_site").to_uppercase());
        if let Some(si) = l.sibling {
            let s = &w.links[si];
            for o in ahead.iter().filter(|o| o.s("id") != n.s("id") && o.s("link") == s.id && overlap(o, &n.s("window_start"), &n.s("window_end"))) {
                found.push(json!({"kind": "both_planes", "severity": "critical", "with": o.s("id"),
                    "text": format!("{} ({}) works on the other plane of {span} in the same window. With both down the two sites have no direct path: ask one carrier to move.", o.s("id"), o.s("provider"))}));
            }
            let ends = [w.devices[s.a].id.clone(), w.devices[s.b].id.clone()];
            for c in changes.iter().filter(|c| overlap(c, &n.s("window_start"), &n.s("window_end"))) {
                let devs = c.list("devices");
                if let Some(d) = devs.iter().find(|d| ends.contains(d)) {
                    found.push(json!({"kind": "change", "severity": "major", "with": c.s("id"),
                        "text": format!("{} ({}) works on {d}, which carries the other plane of {span}, in the same window: move one of the two.", c.s("id"), c.s("title"))}));
                }
            }
            if n.s("impact") == "outage" {
                let load = (peak(&l.id) * l.capacity_mbps as f64 + peak(&s.id) * s.capacity_mbps as f64) / s.capacity_mbps as f64;
                if load > 1.0 {
                    found.push(json!({"kind": "capacity", "severity": if load > 1.1 { "major" } else { "minor" }, "with": s.id,
                        "text": format!("With this circuit down, {} carries both planes: about {:.0}% of its capacity at the last day's peak. Check the hour of the window, or shed traffic first.", s.id, load * 100.0)}));
                }
            }
        }
        if !found.is_empty() {
            out.insert(n.s("id"), found);
        }
    }
    Ok(out)
}

async fn maintenance(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let m = q.last();
    let notices = q.rows("Carriers' notices", "SELECT * FROM netops_maintenance ORDER BY window_start DESC LIMIT 100", json!({})).await?;
    let found = conflicts(&q, &notices).await?;
    let circuits = q.rows("Circuits, by carrier", "SELECT id, provider, link, a_site, z_site, capacity_mbps, monthly_cost, sla, status, contract_months_left FROM netops_circuits ORDER BY provider LIMIT 300", json!({})).await?;
    let down = q.rows("Minutes each circuit was down in the telemetry kept", "SELECT link, COUNT(*) AS minutes FROM netops_link_metrics\nWHERE up = 0\nGROUP BY link LIMIT 300", json!({})).await?;
    q.done(json!({"at": iso(m), "notices": notices, "conflicts": found, "circuits": circuits, "down": down}))
}

async fn maintenance_parse(State(svc): State<Shared>, Json(b): Json<Value>) -> Answer {
    let e = &svc.engine;
    let text = b["text"].as_str().unwrap_or("").trim();
    if text.len() < 20 {
        return Err(bad("text is the carrier's notice, as it arrived"));
    }
    let mut row = read_notice(e, text).map_err(bad)?;
    let notes = row.as_object_mut().and_then(|o| o.remove("notes")).unwrap_or(json!([]));
    let saved = b["save"].as_bool() != Some(false);
    if saved {
        e.db.upsert("netops_maintenance", vec![row.clone()]).await?;
    }
    row["notes"] = notes;
    row["saved"] = json!(saved);
    Ok(Json(row))
}

async fn maintenance_act(State(svc): State<Shared>, Path((id, action)): Path<(String, String)>, headers: HeaderMap, body: Option<Json<Value>>) -> Answer {
    let e = &svc.engine;
    let by = who(&headers, &body);
    let m = e.st().last_minute;
    let rows = e.db.query("SELECT * FROM netops_maintenance WHERE id = :id", json!({"id": id})).await?;
    let Some(mut n) = rows.into_iter().next() else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no notice {id}"))) };
    match action.as_str() {
        "ack" => {
            n.insert("status".into(), json!("acknowledged"));
            n.insert("acked_by".into(), json!(by));
        }
        // A change for the window, so that it is on the calendar and its
        // collisions show, and a silence on the circuit's link for it.
        "plan" => {
            if !n.s("change_id").is_empty() {
                return Err(bad(format!("{id} is already planned as {}", n.s("change_id"))));
            }
            let Some(li) = e.w.link_by_id(&n.s("link")) else { return Err(bad("the notice's circuit is not one of ours")) };
            let l = &e.w.links[li];
            let devices = vec![e.w.devices[l.a].id.clone(), e.w.devices[l.b].id.clone()];
            let cid = format!("CHG-{}", uid(e, m));
            let (start, end) = (parse_iso(&n.s("window_start")), parse_iso(&n.s("window_end")));
            let nums: Vec<i64> = devices.iter().filter_map(|d| e.w.device(d)).map(|d| d.num).collect();
            e.db.upsert("netops_changes", vec![json!({"id": cid, "title": format!("{} maintenance on {} ({})", n.s("provider"), n.s("circuit"), n.s("reference")), "description": n.s("expected"), "kind": "circuit", "risk": "medium",
                "status": "approved", "window_start": iso(start), "window_end": iso(end), "owner": by, "approver": by, "devices": devices, "device_nums": nums,
                "sites": [n.s("a_site"), n.s("z_site")], "services": Vec::<String>::new(), "ticket": n.s("reference"), "rollback_plan": "None of ours: the carrier restores the circuit. Confirm light and adjacencies on both ends afterwards.", "diff": "", "outcome": ""})]).await?;
            e.silence("link", &l.id, &format!("{} maintenance {} on {}", n.s("provider"), n.s("reference"), n.s("circuit")), &by, start, end, &format!("maintenance:{id}")).await?;
            n.insert("status".into(), json!("planned"));
            n.insert("acked_by".into(), json!(by));
            n.insert("change_id".into(), json!(cid));
        }
        _ => return Err(bad("the action is ack or plan")),
    }
    e.db.upsert("netops_maintenance", vec![Value::Object(n.clone())]).await?;
    Ok(Json(Value::Object(n)))
}

// ---- looking a thing up by whatever it is called

fn ip_in(prefix: &str, ip: &str) -> bool {
    let parse = |s: &str| -> Option<u32> {
        let p: Vec<u32> = s.split('.').filter_map(|x| x.parse().ok()).collect();
        (p.len() == 4 && p.iter().all(|&x| x < 256)).then(|| p[0] << 24 | p[1] << 16 | p[2] << 8 | p[3])
    };
    let Some((net, bits)) = prefix.split_once('/') else { return false };
    let (Some(n), Some(a), Ok(b)) = (parse(net), parse(ip.split('/').next().unwrap_or("")), bits.parse::<u32>()) else { return false };
    let mask = if b == 0 { 0 } else { u32::MAX << (32 - b.min(32)) };
    n & mask == a & mask
}

async fn lookup(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let e = q.e.clone();
    let w = &e.w;
    let m = q.last();
    let Some(t) = p.get("q").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else { return Err(bad("q is what to look up")) };
    let low = t.to_lowercase();
    let up = t.to_uppercase();
    let mut hits: Vec<Value> = Vec::new();
    let (firing, open): (Vec<Row>, Vec<Row>) = {
        let st = e.st();
        (st.alerts.values().cloned().collect(), st.open.values().map(|o| o.row.clone()).collect())
    };
    let health = |devices: &[String], entity: &str| -> Value {
        let inc = open.iter().find(|i| i.list("devices").iter().any(|d| devices.contains(d)));
        let n = firing.iter().filter(|a| a.s("entity") == entity || a.list("devices").iter().any(|d| devices.contains(d))).count();
        json!({"state": if inc.is_some() { "bad" } else if n > 0 { "warn" } else { "ok" }, "alerts": n, "incident": inc.map(|i| i.s("id"))})
    };
    let is_ip = low.split('/').next().is_some_and(|a| a.split('.').count() == 4 && a.split('.').all(|x| x.parse::<u8>().is_ok()));
    // Records by their id.
    for (prefix, table, kind, title) in [("INC-", "netops_incidents", "incident", "title"), ("CHG-", "netops_changes", "change", "title"), ("TKT-", "netops_tickets", "ticket", "title"), ("MNT-", "netops_maintenance", "maintenance", "subject")] {
        if up.starts_with(prefix) {
            for r in q.soft(&format!("The {kind} of that id"), &format!("SELECT id, {title} AS title, status FROM {table} WHERE id = :id"), json!({"id": up})).await {
                hits.push(json!({"kind": kind, "id": r.s("id"), "title": r.s("title"), "sub": r.s("status")}));
            }
        }
    }
    // A device, by name.
    for d in w.devices.iter().filter(|d| d.id == low || (low.len() >= 3 && d.id.contains(&low))).take(6) {
        hits.push(json!({"kind": "device", "id": d.id, "title": d.id, "sub": format!("{} {} at {}, {} {}", d.vendor, d.model, d.site.to_uppercase(), d.os, d.os_version), "health": health(std::slice::from_ref(&d.id), &d.id)}));
    }
    // A link by id, a circuit by the carrier's id.
    for l in w.links.iter().filter(|l| l.documented && (l.id == low || (!l.circuit.is_empty() && l.circuit.to_uppercase().contains(&up) && up.len() >= 4))).take(6) {
        let ends = vec![w.devices[l.a].id.clone(), w.devices[l.b].id.clone()];
        let circuit = if l.circuit.is_empty() { String::new() } else { format!("circuit {} ({}), ", l.circuit, l.provider) };
        hits.push(json!({"kind": if l.circuit.is_empty() || l.id == low { "link" } else { "circuit" }, "id": l.id, "title": if l.id == low || l.circuit.is_empty() { l.id.clone() } else { l.circuit.clone() },
            "sub": format!("{circuit}{} {} to {} {}, {} Gb/s, plane {}", ends[0], l.a_if, ends[1], l.z_if, l.capacity_mbps / 1000, l.plane), "health": health(&ends, &l.id)}));
    }
    // An interface: every device has an et-0/0/1, so each one it could be.
    if low.contains('/') && !is_ip {
        for l in w.links.iter().filter(|l| l.documented && (l.a_if.to_lowercase() == low || l.z_if.to_lowercase() == low)).take(8) {
            let (near, far) = if l.a_if.to_lowercase() == low { (l.a, l.b) } else { (l.b, l.a) };
            hits.push(json!({"kind": "interface", "id": l.id, "title": format!("{} on {}", t, w.devices[near].id), "sub": format!("faces {} ({}), {} Gb/s", w.devices[far].id, l.id, l.capacity_mbps / 1000),
                "health": health(&[w.devices[near].id.clone()], &l.id)}));
        }
        for c in w.customers.iter().filter(|c| c.port.to_lowercase() == low).take(6) {
            hits.push(json!({"kind": "customer", "id": c.id, "title": format!("{} on {}", t, w.devices[c.device].id), "sub": format!("{}'s port, VLAN {}", c.name, c.vlan), "health": health(&[w.devices[c.device].id.clone()], "")}));
        }
    }
    // An address: a device's own, a BGP neighbour's, or inside what is
    // routed to a customer.
    if is_ip {
        let addr = low.split('/').next().unwrap_or("").to_string();
        for d in q.soft("Devices with that address", "SELECT id, site, role, mgmt_ip, loopback FROM netops_devices\nWHERE mgmt_ip = :q OR loopback = :q LIMIT 5", json!({"q": addr})).await {
            let id = d.s("id");
            hits.push(json!({"kind": "device", "id": id, "title": format!("{addr} is {id}"), "sub": format!("{} at {} (management {}, loopback {})", d.s("role"), d.s("site").to_uppercase(), d.s("mgmt_ip"), d.s("loopback")), "health": health(std::slice::from_ref(&id), &id)}));
        }
        for b in q.soft("BGP neighbours with that address", "SELECT id, device, peer_ip, peer_asn, peer_name, kind, state, prefixes_rx FROM netops_bgp\nWHERE peer_ip = :q LIMIT 5", json!({"q": addr})).await {
            let dev = b.s("device");
            hits.push(json!({"kind": "bgp", "id": dev, "title": format!("{addr} is {} (AS{})", b.s("peer_name"), b.i("peer_asn")), "sub": format!("{} session on {dev}: {}, {} prefixes received", b.s("kind"), b.s("state"), b.i("prefixes_rx")),
                "health": json!({"state": if b.s("state") == "established" { "ok" } else { "bad" }, "alerts": 0})}));
        }
        for c in w.customers.iter().filter(|c| ip_in(&c.prefix, &addr)).take(3) {
            hits.push(json!({"kind": "customer", "id": c.id, "title": format!("{addr} belongs to {}", c.name), "sub": format!("{} is routed to them on {} port {}, VLAN {}", c.prefix, w.devices[c.device].id, c.port, c.vlan), "health": health(&[w.devices[c.device].id.clone()], "")}));
        }
        let lines = q.soft("Log lines that mention it in the last day (substring index)", "SELECT COUNT(*) AS n FROM netops_syslog\nWHERE message LIKE :q AND ts >= :since", json!({"q": format!("%{addr}%"), "since": iso(m - 86400)})).await;
        let n = lines.first().map_or(0, |r| r.i("n"));
        if n > 0 {
            hits.push(json!({"kind": "logs", "id": addr, "title": format!("{n} log lines mention {addr} in the last day"), "sub": "open them in Explore"}));
        }
    }
    // An AS number.
    if let Some(asn) = low.strip_prefix("as").unwrap_or(&low).parse::<i64>().ok().filter(|_| !is_ip && low.len() <= 8) {
        for b in q.soft("Sessions with that AS", "SELECT device, peer_ip, peer_name, kind, state FROM netops_bgp\nWHERE peer_asn = :asn LIMIT 8", json!({"asn": asn})).await {
            hits.push(json!({"kind": "bgp", "id": b.s("device"), "title": format!("AS{asn} is {}", b.s("peer_name")), "sub": format!("{} session on {} with {}: {}", b.s("kind"), b.s("device"), b.s("peer_ip"), b.s("state")),
                "health": json!({"state": if b.s("state") == "established" { "ok" } else { "bad" }, "alerts": 0})}));
        }
    }
    // A customer by name, id, circuit or prefix; a site; a service.
    for c in w.customers.iter().filter(|c| c.id == low || c.name.to_lowercase().contains(&low) && low.len() >= 3 || c.circuit.to_uppercase() == up || c.prefix == low).take(6) {
        hits.push(json!({"kind": "customer", "id": c.id, "title": c.name, "sub": format!("{} customer at {}, on {} port {}, {}, circuit {}", c.tier, c.site.to_uppercase(), w.devices[c.device].id, c.port, c.prefix, c.circuit), "health": health(&[w.devices[c.device].id.clone()], "")}));
    }
    for s in e.w.sites.iter().filter(|s| s.id == low || s.city.to_lowercase() == low) {
        let devs: Vec<String> = w.devices.iter().filter(|d| d.site == s.id).map(|d| d.id.clone()).collect();
        hits.push(json!({"kind": "site", "id": s.id, "title": format!("{} ({})", s.city, s.id.to_uppercase()), "sub": format!("{}, {} devices", s.kind, devs.len()), "health": health(&devs, s.id)}));
    }
    for s in w.services.iter().filter(|s| s.id == low || s.name.to_lowercase() == low) {
        hits.push(json!({"kind": "service", "id": s.id, "title": s.name, "sub": format!("tier {}, owned by {}", s.tier, s.owner)}));
    }
    // The estate behind the backbone: an object by its id (exactly, else by
    // the start of it), and a customer of a business park by name.
    if low.len() >= 3 && !is_ip && !hits.iter().any(|h| h["kind"] == "device" && h["id"] == low.as_str()) {
        let cols = "id, kind, layer, name, state, reason, hub, metro";
        let mut found = q.soft("An object of the estate by its id", &format!("SELECT {cols} FROM {NODES}\nWHERE id = :q LIMIT 1"), json!({"q": low})).await;
        if found.is_empty() && low.contains('-') {
            found = q.soft("Objects of the estate by the start of their id (PREFIX)", &format!("SELECT {cols} FROM {NODES}\nWHERE PREFIX(id, :q) AND layer != 'container'\nORDER BY tier, id LIMIT 6"), json!({"q": low})).await;
        }
        for r in found.iter().filter(|r| r.s("layer") != "core") {
            let id = r.s("id");
            let inc = open.iter().find(|i| i.s("root_entity") == id || firing.iter().any(|a| a.s("entity") == id && a.s("incident") == i.s("id")));
            let n = firing.iter().filter(|a| a.s("entity") == id).count();
            let st = r.s("state");
            let state = if inc.is_some() || st == "down" { "bad" } else if st != "ok" || n > 0 { "warn" } else { "ok" };
            let what = if r.s("layer") == "container" { format!("{} {}", r.s("kind"), r.s("name")) } else { format!("{} in {}, {}", r.s("kind"), r.s("hub"), r.s("metro").trim_start_matches("m-").to_uppercase()) };
            hits.push(json!({"kind": "object", "id": id, "title": if r.s("name") == id { id.clone() } else { format!("{id}: {}", r.s("name")) }, "sub": if st == "ok" { what } else { format!("{what} · {st}: {}", r.s("reason")) },
                "health": {"state": state, "alerts": n, "incident": inc.map(|i| i.s("id"))}, "exact": id == low}));
        }
        if found.is_empty() && !low.contains('-') {
            let names = q.soft("Customers of the estate by name (substring index)", &format!("SELECT customer, COUNT(*) AS n, SUM(CASE WHEN state != 'ok' THEN 1 ELSE 0 END) AS hurt FROM {NODES}\nWHERE customer ILIKE :q\nGROUP BY customer ORDER BY customer LIMIT 6"), json!({"q": format!("%{}%", t.replace(['%', '_', '\\'], ""))})).await;
            for c in names {
                let hurt = c.i("hurt");
                hits.push(json!({"kind": "estate-customer", "id": c.s("customer"), "title": c.s("customer"), "sub": format!("a customer behind the estate's business parks: {} site{}{}", c.i("n"), if c.i("n") == 1 { "" } else { "s" }, if hurt > 0 { format!(", {hurt} not ok now") } else { String::new() }),
                    "health": {"state": if hurt > 0 { "warn" } else { "ok" }, "alerts": hurt}}));
            }
        }
    }
    q.done(json!({"q": t, "hits": hits}))
}

// ---- what happened to one thing, in order

async fn activity(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let Some(id) = p.get("id").filter(|s| !s.is_empty()) else { return Err(bad("id is the device")) };
    let hours = p.get("hours").and_then(|h| h.parse::<i64>().ok()).unwrap_or(24).clamp(1, 168);
    let events = device_activity(&q, std::slice::from_ref(id), q.last() - hours * 3600).await?;
    q.done(json!({"id": id, "hours": hours, "events": events}))
}

/// Alerts, incidents, changes, configuration versions and bursts of error
/// logs on `devices` since `since`, newest first.
async fn device_activity(q: &Q, devices: &[String], since: i64) -> std::result::Result<Vec<Value>, ApiError> {
    let e = &q.e;
    let m = q.last();
    let nums = nums_of(e, devices);
    let ins = quoted(devices);
    let p = json!({"since": iso(since)});
    let mut ev: Vec<(String, Value)> = Vec::new();
    let links: HashSet<String> = devices.iter().filter_map(|d| e.w.devices.iter().position(|x| &x.id == d)).flat_map(|di| e.w.links_of(di)).map(|li| e.w.links[li].id.clone()).collect();
    for a in q.rows("Alerts raised since", "SELECT id, rule_name, severity, status, entity, devices, summary, started, ended, incident FROM netops_alerts\nWHERE started >= :since\nORDER BY started DESC LIMIT 1000", p.clone()).await? {
        if devices.contains(&a.s("entity")) || links.contains(&a.s("entity")) || a.list("devices").iter().any(|d| devices.contains(d)) {
            let ended = if a.s("ended").is_empty() { "still firing".to_string() } else { format!("cleared {}", hhmm(parse_iso(&a.s("ended")))) };
            ev.push((a.s("started"), json!({"ts": a.s("started"), "kind": "alert", "severity": a.s("severity"), "title": a.s("summary"), "sub": format!("{}{}", ended, if a.s("incident").is_empty() { String::new() } else { format!(", in {}", a.s("incident")) }), "ref": a.s("incident")})));
        }
    }
    for i in q.rows("Incidents on these devices (INT[] overlap)", &format!("SELECT id, title, severity, status, started, resolved, category FROM netops_incidents\nWHERE started >= :since AND device_nums && [{nums}]\nORDER BY started DESC LIMIT 50"), p.clone()).await? {
        ev.push((i.s("started"), json!({"ts": i.s("started"), "kind": "incident", "severity": i.s("severity"), "title": format!("{}: {}", i.s("id"), i.s("title")), "sub": i.s("status"), "ref": i.s("id")})));
    }
    for c in q.rows("Changes carried out on them", &format!("SELECT id, title, status, executed, owner, outcome FROM netops_changes\nWHERE executed >= :since AND executed <= :now AND device_nums && [{nums}]\nORDER BY executed DESC LIMIT 50"), json!({"since": iso(since), "now": iso(m)})).await? {
        ev.push((c.s("executed"), json!({"ts": c.s("executed"), "kind": "change", "title": format!("{}: {}", c.s("id"), c.s("title")), "sub": format!("{} by {}{}", c.s("status").replace('_', " "), c.s("owner"), if c.s("outcome").is_empty() { String::new() } else { format!(", {}", c.s("outcome")) }), "ref": c.s("id")})));
    }
    for c in q.rows("Configuration versions written", &format!("SELECT device, version, ts, author, change_id, lines FROM netops_configs\nWHERE ts >= :since AND device IN ({ins})\nORDER BY ts DESC LIMIT 50"), p.clone()).await? {
        ev.push((c.s("ts"), json!({"ts": c.s("ts"), "kind": "config", "title": format!("{} configuration version {}", c.s("device"), c.i("version")), "sub": format!("by {}{}", c.s("author"), if c.s("change_id").is_empty() { String::new() } else { format!(" for {}", c.s("change_id")) }), "ref": c.s("change_id")})));
    }
    for l in q.rows("Error logs, by the hour and the kind of line", &format!("SELECT date_trunc('hour', ts) AS t, device, template, COUNT(*) AS n FROM netops_syslog\nWHERE ts >= :since AND sev <= 3 AND device IN ({ins})\nGROUP BY date_trunc('hour', ts), device, template ORDER BY t DESC LIMIT 60"), p).await? {
        ev.push((l.s("t"), json!({"ts": l.s("t"), "kind": "logs", "title": format!("{} error lines on {}: {}", l.i("n"), l.s("device"), l.s("template")), "sub": "in that hour", "ref": l.s("device")})));
    }
    ev.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(ev.into_iter().map(|x| x.1).take(120).collect())
}

// ---- customers: is it the network?

/// The devices and links between a customer's port and the backbone: the
/// router they attach on, the cores of that site, the links between them
/// and the backbone links out of the site.
fn access_path(e: &Engine, device: usize) -> (Vec<String>, Vec<String>) {
    let w = &e.w;
    let site = w.devices[device].site;
    let mut devs = vec![device];
    let mut links = Vec::new();
    for li in w.links_of(device) {
        let l = &w.links[li];
        if !l.live || !l.documented {
            continue;
        }
        let other = if l.a == device { l.b } else { l.a };
        if w.devices[other].site == site && w.devices[other].role == "core" {
            links.push(li);
            if !devs.contains(&other) {
                devs.push(other);
            }
        }
    }
    for &core in devs.clone().iter().skip(1) {
        for li in w.links_of(core) {
            let l = &w.links[li];
            if l.live && l.documented && l.kind == "backbone" && !links.contains(&li) {
                links.push(li);
            }
        }
    }
    (devs.iter().map(|&d| w.devices[d].id.clone()).collect(), links.iter().map(|&l| w.links[l].id.clone()).collect())
}

async fn customers(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let e = q.e.clone();
    let rows = q.rows("Customers", "SELECT id, num, name, tier, site, device, services, mrr, prefix, port, vlan, circuit FROM netops_customers ORDER BY mrr DESC LIMIT 500", json!({})).await?;
    let tickets = q.rows("Their open tickets", "SELECT customer, COUNT(*) AS n FROM netops_tickets\nWHERE status != 'closed'\nGROUP BY customer LIMIT 500", json!({})).await?;
    let (firing, open): (Vec<Row>, Vec<Row>) = {
        let st = e.st();
        (st.alerts.values().cloned().collect(), st.open.values().map(|o| o.row.clone()).collect())
    };
    let out: Vec<Value> = rows
        .into_iter()
        .map(|mut c| {
            let (devs, links) = e.w.devices.iter().position(|d| d.id == c.s("device")).map(|di| access_path(&e, di)).unwrap_or_default();
            let theirs = c.list("services");
            let inc = open.iter().find(|i| i.list("devices").iter().any(|d| devs.contains(d)) || (i.list("sites").contains(&c.s("site")) && i.list("services").iter().any(|s| theirs.contains(s))));
            let n = firing.iter().filter(|a| a.s("status") != "silenced" && (devs.contains(&a.s("entity")) || links.contains(&a.s("entity")))).count();
            c.insert("state".into(), json!(if inc.is_some() { "bad" } else if n > 0 { "warn" } else { "ok" }));
            c.insert("incident".into(), json!(inc.map(|i| i.s("id"))));
            c.insert("alerts".into(), json!(n));
            c.insert("tickets".into(), json!(tickets.iter().find(|t| t.s("customer") == c.s("id")).map_or(0, |t| t.i("n"))));
            Value::Object(c)
        })
        .collect();
    q.done(json!({"customers": out}))
}

async fn customer(State(svc): State<Shared>, Path(id): Path<String>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let e = q.e.clone();
    let m = q.last();
    let hours = p.get("hours").and_then(|h| h.parse::<i64>().ok()).unwrap_or(24).clamp(1, 72);
    let since = m - hours * 3600;
    let rows = q.rows("The customer", "SELECT * FROM netops_customers WHERE id = :id", json!({"id": id})).await?;
    let Some(c) = rows.into_iter().next() else { return Err(ApiError(StatusCode::NOT_FOUND, format!("no customer {id}"))) };
    let Some(di) = e.w.devices.iter().position(|d| d.id == c.s("device")) else { return Err(bad("the customer's device is not in the inventory")) };
    let (devs, links) = access_path(&e, di);
    let p1 = json!({"since": iso(since)});
    let link_stats = q.rows("Their path's links over the window: worst loss, peak load, errors, any minute down", &format!("SELECT link, MAX(loss) AS worst_loss, MAX(util) AS peak, SUM(errors) AS errors, MIN(up) AS up, AVG(latency_ms) AS latency FROM netops_link_metrics\nWHERE ts >= :since AND link IN ({})\nGROUP BY link LIMIT 200", quoted(&links)), p1.clone()).await?;
    let bad_minutes = q.rows("The minutes the path lost packets or a link", &format!("SELECT MIN(ts) AS first, MAX(ts) AS last, COUNT(*) AS n FROM netops_link_metrics\nWHERE ts >= :since AND link IN ({}) AND (loss > 0.2 OR up = 0)", quoted(&links)), p1.clone()).await?;
    let dev_stats = q.rows("Their path's devices over the window", &format!("SELECT device, MAX(cpu) AS cpu, MIN(up) AS up FROM netops_device_metrics\nWHERE ts >= :since AND device IN ({})\nGROUP BY device LIMIT 50", quoted(&devs)), p1.clone()).await?;
    let loss_series = q.rows("Worst loss on the path, a minute", &format!("SELECT date_trunc('minute', ts) AS t, MAX(loss) AS loss, MAX(util) AS util FROM netops_link_metrics\nWHERE ts >= :since AND up = 1 AND link IN ({})\nGROUP BY date_trunc('minute', ts) ORDER BY t LIMIT 5000", quoted(&links)), p1.clone()).await?;
    let services = c.list("services");
    let svc_stats = if services.is_empty() { Vec::new() } else {
        q.rows("The services they buy, against their objectives", &format!("SELECT m.service, MAX(s.name) AS name, SUM(m.errors) AS errors, SUM(m.requests) AS requests, MAX(m.latency_p95) AS p95, MAX(s.slo_target) AS slo, MAX(s.latency_slo_ms) AS latency_slo\nFROM netops_service_metrics m JOIN netops_services s ON m.service = s.id\nWHERE m.ts >= :since AND m.service IN ({})\nGROUP BY m.service LIMIT 20", quoted(&services)), p1.clone()).await?
    };
    let nums = nums_of(&e, &devs);
    let incidents = q.rows("Incidents on their path (INT[] overlap)", &format!("SELECT id, title, severity, status, started, resolved, category, root_cause, mttr_min FROM netops_incidents\nWHERE (started >= :since OR status != 'resolved') AND device_nums && [{nums}]\nORDER BY started DESC LIMIT 20"), p1.clone()).await?;
    let changes = q.rows("Changes on their path: carried out in the window, or planned for the next two days", &format!("SELECT id, title, status, risk, window_start, window_end, executed, owner FROM netops_changes\nWHERE device_nums && [{nums}] AND window_end >= :since AND window_start <= :until\nORDER BY window_start LIMIT 20"), json!({"since": iso(since), "until": iso(m + 2 * 86400)})).await?;
    let notices = q.rows("Carriers' maintenance on their path's circuits", &format!("SELECT id, provider, circuit, link, window_start, window_end, impact, status FROM netops_maintenance\nWHERE link IN ({}) AND window_end >= :since\nORDER BY window_start LIMIT 10", quoted(&links)), p1.clone()).await?;
    let tickets = q.rows("Their tickets", "SELECT id, title, status, priority, assignee, opened, resolution FROM netops_tickets\nWHERE customer = :id ORDER BY opened DESC LIMIT 8", json!({"id": id})).await?;
    let logs = q.rows("Warnings and errors on the router they attach to", "SELECT ts, severity, program, message FROM netops_syslog\nWHERE device = :d AND ts >= :since AND sev <= 4\nORDER BY ts DESC LIMIT 12", json!({"d": c.s("device"), "since": iso(since)})).await?;
    let firing: Vec<Row> = e.st().alerts.values().filter(|a| a.s("status") != "silenced" && (devs.contains(&a.s("entity")) || links.contains(&a.s("entity")))).cloned().collect();

    // The answer, from the rows above.
    let open_inc = incidents.iter().find(|i| i.s("status") != "resolved");
    let past_inc: Vec<&Row> = incidents.iter().filter(|i| i.s("status") == "resolved").collect();
    let bad_n = bad_minutes.first().map_or(0, |r| r.i("n"));
    let worst = link_stats.iter().map(|r| r.f("worst_loss")).fold(0.0, f64::max);
    let down_dev: Vec<String> = dev_stats.iter().filter(|r| r.i("up") == 0).map(|r| r.s("device")).collect();
    let done_changes: Vec<&Row> = changes.iter().filter(|c| !c.s("executed").is_empty() && parse_iso(&c.s("executed")) >= since).collect();
    let failing: Vec<&Row> = svc_stats.iter().filter(|s| s.f("requests") > 0.0 && 1.0 - s.f("errors") / s.f("requests") < s.f("slo")).collect();
    let name = c.s("name");
    let window = if hours == 24 { "the last day".to_string() } else { format!("the last {hours} hours") };
    let mut reasons: Vec<String> = Vec::new();
    // An open incident off their path that hits a service they buy at their site.
    let related: Option<Row> = e.st().open.values().map(|o| o.row.clone()).find(|i| i.list("sites").contains(&c.s("site")) && i.list("services").iter().any(|s| services.contains(s)) && open_inc.is_none_or(|o| o.s("id") != i.s("id")));
    let (verdict, headline) = if let Some(i) = open_inc {
        reasons.push(format!("{} is open on their path: {}", i.s("id"), i.s("title")));
        if !i.s("root_cause").is_empty() {
            reasons.push(i.s("root_cause"));
        }
        ("now", format!("Yes, right now: {} is on {name}'s path", i.s("id")))
    } else if let Some(i) = &related {
        let theirs: Vec<String> = i.list("services").into_iter().filter(|s| services.contains(s)).collect();
        reasons.push(format!("{} is open at {}: {}", i.s("id"), c.s("site").to_uppercase(), i.s("title")));
        reasons.push(format!("It is not on the links between their port and the backbone, but it affects {}, which they buy", theirs.join(" and ")));
        ("now", format!("Likely, right now: {} affects a service {name} buys at {}", i.s("id"), c.s("site").to_uppercase()))
    } else if !firing.is_empty() {
        for a in firing.iter().take(3) {
            reasons.push(a.s("summary"));
        }
        ("now", format!("Probably: {} alert{} firing on {name}'s path", firing.len(), if firing.len() == 1 { " is" } else { "s are" }))
    } else if bad_n > 0 || !past_inc.is_empty() || !down_dev.is_empty() {
        let r = bad_minutes.first();
        let (first, last) = (r.map(|r| r.s("first")).unwrap_or_default(), r.map(|r| r.s("last")).unwrap_or_default());
        if bad_n > 0 {
            reasons.push(format!("{bad_n} link-minutes with packet loss or a link down between {} and {} UTC; the worst loss was {worst:.1}%", hhmm(parse_iso(&first)), hhmm(parse_iso(&last) + 60)));
        }
        for i in past_inc.iter().take(2) {
            reasons.push(format!("{} ({}), resolved in {:.0} minutes: {}", i.s("id"), i.s("title"), i.f("mttr_min"), i.s("root_cause")));
        }
        for d in &down_dev {
            reasons.push(format!("{d} was unreachable for part of the window"));
        }
        ("earlier", format!("It was, earlier: {name}'s path was degraded in {window}, and is clean now"))
    } else if !done_changes.is_empty() {
        for ch in done_changes.iter().take(2) {
            reasons.push(format!("{} ({}) was carried out at {} UTC by {}, with no loss or alert after it", ch.s("id"), ch.s("title"), hhmm(parse_iso(&ch.s("executed"))), ch.s("owner")));
        }
        ("change", format!("Probably not, but a change ran on {name}'s path in {window}"))
    } else {
        ("clear", format!("No sign of it: {name}'s path has been clean for {window}"))
    };
    for s in &failing {
        reasons.push(format!("{} ran at {:.3}% success against an objective of {:.2}%, for every customer of it", s.s("name"), (1.0 - s.f("errors") / s.f("requests")) * 100.0, s.f("slo") * 100.0));
    }
    let checked = vec![
        format!("{} links from their port to the backbone: worst loss {worst:.2}%, peak load {:.0}%", links.len(), link_stats.iter().map(|r| r.f("peak")).fold(0.0, f64::max) * 100.0),
        format!("{} devices on the path: {}", devs.len(), if down_dev.is_empty() { "all reachable throughout".to_string() } else { format!("{} unreachable at some point", down_dev.join(", ")) }),
        format!("{} incident{} and {} alert{} firing on the path", incidents.len(), if incidents.len() == 1 { "" } else { "s" }, firing.len(), if firing.len() == 1 { "" } else { "s" }),
        format!("{} change{} carried out on the path, {} planned for the next two days", done_changes.len(), if done_changes.len() == 1 { "" } else { "s" }, changes.len() - done_changes.len()),
        if svc_stats.len() == 1 { format!("The service they buy is {} its objective", if failing.is_empty() { "within" } else { "below" }) } else { format!("{} of the {} services they buy within objective", svc_stats.len() - failing.len(), svc_stats.len()) },
    ];
    let reply = match verdict {
        "now" => format!("Hello,\n\nWe can confirm a fault on our network that affects your service at {} ({}, {}). Our engineers are working on it now{}. We will update you within 30 minutes and again when it is resolved.\n\nMeridian NOC",
            c.s("site").to_uppercase(), c.s("circuit"), c.s("prefix"), open_inc.map(|i| format!(" under {}", i.s("id"))).or(related.as_ref().map(|i| format!(" under {}", i.s("id")))).unwrap_or_default()),
        "earlier" => format!("Hello,\n\nWe checked the path of your service at {} ({}, {}) over {window}. {} The path is clean now. We are sorry for the disruption; a report of the cause will follow.\n\nMeridian NOC",
            c.s("site").to_uppercase(), c.s("circuit"), c.s("prefix"), reasons.first().map(|r| format!("We did see a problem on our side: {r}.")).unwrap_or_default()),
        _ => format!("Hello,\n\nWe checked the path of your service at {} ({}, {}) over {window}: the {} links and {} devices between your port and our backbone showed no packet loss above {worst:.2}%, no errors of note and no outage, and no alert fired on them. We do not see a fault on our network. If the problem continues, a traceroute and the time it happened (UTC) will let us look further.\n\nMeridian NOC",
            c.s("site").to_uppercase(), c.s("circuit"), c.s("prefix"), links.len(), devs.len()),
    };
    q.done(json!({"customer": c, "hours": hours, "at": iso(m), "verdict": verdict, "headline": headline, "reasons": reasons, "checked": checked, "reply": reply,
        "path": {"devices": devs, "links": links}, "links": link_stats, "device_stats": dev_stats, "series": loss_series, "services": svc_stats, "incidents": incidents,
        "changes": changes, "maintenance": notices, "tickets": tickets, "logs": logs, "alerts": firing}))
}

// ---- the shift: what happened, and what waits

async fn note_add(State(svc): State<Shared>, headers: HeaderMap, Json(b): Json<Value>) -> Answer {
    let e = &svc.engine;
    let body = b["body"].as_str().unwrap_or("").trim();
    if body.is_empty() {
        return Err(bad("body is the note"));
    }
    let row = json!({"id": format!("note-{}", uid(e, crate::sim::now())), "ts": iso(crate::sim::now()), "kind": "note", "author": crate::api::actor(&headers, b["who"].as_str(), "NOC engineer"), "body": body, "done": false});
    e.db.upsert("netops_handovers", vec![row.clone()]).await?;
    Ok(Json(row))
}

async fn note_done(State(svc): State<Shared>, Path(id): Path<String>) -> Answer {
    svc.engine.db.sql(&format!("UPDATE netops_handovers SET done = true WHERE id = '{}'", id.replace('\'', ""))).await?;
    Ok(Json(json!({"id": id, "done": true})))
}

async fn shift(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let e = q.e.clone();
    let m = q.last();
    // Shifts are the on-call table's: twelve hours, changing at 00:00 and
    // 12:00 UTC. The briefing covers the last twelve hours.
    let began = m - m.rem_euclid(43200);
    let since = m - 43200;
    let p = json!({"since": iso(since), "now": iso(m)});
    let oncall = q.rows("On call now", "SELECT team, person, role, contact FROM netops_oncall\nWHERE starts <= :now AND ends > :now LIMIT 20", json!({"now": iso(m)})).await?;
    let before = q.rows("On call the shift before", "SELECT team, person, role FROM netops_oncall\nWHERE starts <= :t AND ends > :t AND role = 'primary' LIMIT 20", json!({"t": iso(began - 60)})).await?;
    let incidents = q.rows("Incidents of the last twelve hours, and any still open", "SELECT id, title, severity, status, started, resolved, category, root_cause, root_entity, commander, mttr_min, customers FROM netops_incidents\nWHERE started >= :since OR status != 'resolved'\nORDER BY started DESC LIMIT 30", p.clone()).await?;
    let pending = q.rows("Actions waiting for approval", "SELECT id, incident, kind, title, risk, proposed FROM netops_actions\nWHERE status = 'proposed'\nORDER BY proposed DESC LIMIT 10", json!({})).await?;
    let alerts = q.rows("Alerts firing or silenced now", "SELECT id, rule_name, severity, status, entity_kind, entity, summary, started, incident, acked_by, assignee, silenced FROM netops_alerts\nWHERE status IN ('firing', 'silenced')\nORDER BY started DESC LIMIT 300", json!({})).await?;
    let flaps = q.rows("What kept alerting: alerts raised since, by rule and entity", "SELECT rule_name, entity, COUNT(*) AS n, MAX(started) AS last FROM netops_alerts\nWHERE started >= :since\nGROUP BY rule_name, entity ORDER BY n DESC LIMIT 12", p.clone()).await?;
    let done = q.rows("Changes carried out since", "SELECT id, title, status, executed, owner, outcome, devices FROM netops_changes\nWHERE executed >= :since AND executed <= :now\nORDER BY executed DESC LIMIT 20", p.clone()).await?;
    let next = q.rows("Changes in progress, or planned for the next twelve hours", "SELECT id, title, kind, risk, status, window_start, window_end, owner, devices, started, started_by FROM netops_changes\nWHERE status = 'in_progress' OR (status IN ('approved', 'scheduled') AND window_start <= :until AND window_end >= :now)\nORDER BY window_start LIMIT 20", json!({"until": iso(m + 43200), "now": iso(m)})).await?;
    let notices = q.rows("Carriers' notices that are not over", "SELECT * FROM netops_maintenance\nWHERE status != 'completed' AND window_end >= :now\nORDER BY window_start LIMIT 30", json!({"now": iso(m)})).await?;
    let found = conflicts(&q, &notices).await?;
    let tickets = q.rows("Open tickets", "SELECT id, title, status, priority, assignee, queue, customer, device, opened FROM netops_tickets\nWHERE status != 'closed'\nORDER BY priority, opened LIMIT 30", json!({})).await?;
    let notes = q.rows("Notes left for the shift", "SELECT id, ts, author, body, done FROM netops_handovers\nWHERE kind = 'note' AND ts >= :since\nORDER BY ts DESC LIMIT 20", json!({"since": iso(m - 2 * 86400)})).await?;
    let hot = q.rows("The fullest links of the last twelve hours", "SELECT link, MAX(util) AS peak FROM netops_link_metrics\nWHERE ts >= :since AND up = 1\nGROUP BY link ORDER BY peak DESC LIMIT 3", p.clone()).await?;
    let active: Vec<Row> = e.st().silences.values().cloned().collect();

    // The worklist: what a person should do, most urgent first.
    let mut work: Vec<(i64, String, Value)> = Vec::new();
    let mut add = |urgency: i64, at: String, kind: &str, title: String, sub: String, go: Value, act: Value| {
        work.push((urgency, at.clone(), json!({"urgency": urgency, "kind": kind, "title": title, "sub": sub, "at": at, "go": go, "act": act})));
    };
    for a in &pending {
        add(1, a.s("proposed"), "approval", format!("Approve or reject: {}", a.s("title")), format!("{} risk, proposed by the {} agent for {}", a.s("risk"), if e.is_estate(&a.s("incident")) { "estate's" } else { "root-cause" }, a.s("incident")), json!({"page": "incidents", "id": a.s("incident")}), json!({"action": a.s("id")}));
    }
    for i in incidents.iter().filter(|i| i.s("status") != "resolved") {
        let waits = pending.iter().any(|a| a.s("incident") == i.s("id"));
        if !waits {
            add(1, i.s("started"), "incident", format!("{} is {}: {}", i.s("id"), i.s("status"), i.s("title")), format!("{}; commander {}", if i.s("root_cause").is_empty() { "cause not named yet".to_string() } else { i.s("root_cause") }, if i.s("commander").is_empty() { "nobody yet".to_string() } else { i.s("commander") }), json!({"page": "incidents", "id": i.s("id")}), Value::Null);
        }
    }
    for c in next.iter().filter(|c| c.s("status") == "in_progress") {
        add(1, c.s("started"), "change", format!("{} is in progress: verify it and close it", c.s("id")), format!("{}; started {} by {}", c.s("title"), hhmm(parse_iso(&c.s("started"))), c.s("started_by")), json!({"page": "changes", "id": c.s("id")}), Value::Null);
    }
    for n in &notices {
        let id = n.s("id");
        if let Some(cs) = found.get(&id) {
            let worst = cs.iter().find(|c| c["severity"] == "critical").or(cs.first()).cloned().unwrap_or(Value::Null);
            add(if worst["severity"] == "critical" { 1 } else { 2 }, n.s("window_start"), "maintenance", format!("{} {} on {} collides with something", n.s("provider"), id, n.s("circuit")), worst["text"].as_str().unwrap_or("").to_string(), json!({"page": "carriers", "id": id}), Value::Null);
        } else if n.s("status") == "new" {
            add(2, n.s("window_start"), "maintenance", format!("New notice from {}: {} on {}", n.s("provider"), n.s("impact"), n.s("circuit")), format!("window {} to {} UTC; nobody has acknowledged it", n.s("window_start").replace('T', " ").trim_end_matches(":00Z"), hhmm(parse_iso(&n.s("window_end")))), json!({"page": "carriers", "id": id}), Value::Null);
        }
    }
    for c in next.iter().filter(|c| c.s("status") != "in_progress" && parse_iso(&c.s("window_start")) <= m + 4 * 3600) {
        let begins = parse_iso(&c.s("window_start"));
        add(2, c.s("window_start"), "change", format!("{} {}: {}", c.s("id"), if begins <= m { "is in its window".to_string() } else { format!("starts at {} UTC", hhmm(begins)) }, c.s("title")), format!("{} risk, owner {}, on {}", c.s("risk"), c.s("owner"), c.list("devices").join(", ")), json!({"page": "changes", "id": c.s("id")}), Value::Null);
    }
    let loose: Vec<&Row> = alerts.iter().filter(|a| a.s("status") == "firing" && a.s("incident").is_empty() && a.s("acked_by").is_empty()).collect();
    for a in loose.iter().take(6) {
        add(if a.s("severity") == "minor" { 3 } else { 2 }, a.s("started"), "alert", format!("Unacknowledged: {}", a.s("summary")), format!("{} since {} UTC, in no incident", a.s("severity"), hhmm(parse_iso(&a.s("started")))), json!({"page": "alerts"}), json!({"alert": a.s("id")}));
    }
    for t in tickets.iter().filter(|t| t.s("priority") <= "P2".to_string()).take(4) {
        add(3, t.s("opened"), "ticket", format!("{} {}: {}", t.s("priority"), t.s("id"), t.s("title")), format!("{}, with {}", t.s("status"), t.s("assignee")), json!({"page": "customers", "id": t.s("customer")}), Value::Null);
    }
    for s in active.iter().filter(|s| parse_iso(&s.s("ends")) <= m + 3600 && s.s("source") == "manual") {
        add(3, s.s("ends"), "silence", format!("The silence on {} ends at {} UTC", s.s("matcher"), hhmm(parse_iso(&s.s("ends")))), s.s("reason"), json!({"page": "alerts"}), Value::Null);
    }
    for n in notes.iter().filter(|n| n.get("done") != Some(&Value::Bool(true))) {
        add(3, n.s("ts"), "note", format!("{} left a note", n.s("author")), n.s("body"), Value::Null, json!({"note": n.s("id")}));
    }
    work.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let work: Vec<Value> = work.into_iter().map(|x| x.2).collect();

    // The briefing, in sentences.
    let recent: Vec<&Row> = incidents.iter().filter(|i| parse_iso(&i.s("started")) >= since).collect();
    let resolved = recent.iter().filter(|i| i.s("status") == "resolved").count();
    let open_now = incidents.iter().filter(|i| i.s("status") != "resolved").count();
    let flapping: Vec<&Row> = flaps.iter().filter(|f| f.i("n") >= 3).collect();
    let silenced = alerts.iter().filter(|a| a.s("status") == "silenced").count();
    let firing = alerts.len() - silenced;
    let mut lines: Vec<String> = Vec::new();
    lines.push(match (recent.len(), open_now) {
        (0, 0) => "No incidents in the last twelve hours, and none open.".to_string(),
        (n, 0) => format!("{n} incident{} in the last twelve hours, all resolved{}.", if n == 1 { "" } else { "s" }, recent.iter().map(|i| i.f("mttr_min")).fold(None, |a: Option<f64>, x| Some(a.map_or(x, |y| y.max(x)))).map(|x| format!(" (the longest took {x:.0} minutes)")).unwrap_or_default()),
        (n, o) => format!("{n} incident{} in the last twelve hours, {resolved} resolved; {o} open now.", if n == 1 { "" } else { "s" }),
    });
    for i in recent.iter().take(3) {
        lines.push(format!("{} at {} UTC, {}: {}", i.s("id"), hhmm(parse_iso(&i.s("started"))), i.s("status"), if i.s("root_cause").is_empty() { i.s("title") } else { i.s("root_cause") }));
    }
    lines.push(format!("{firing} alert{} firing, {} of them in no incident and not acknowledged; {silenced} silenced.", if firing == 1 { "" } else { "s" }, loose.len()));
    if let Some(f) = flapping.first() {
        lines.push(format!("{} on {} alerted {} times: look for the cause rather than clearing it again.", f.s("rule_name"), f.s("entity"), f.i("n")));
    }
    let failed = done.iter().filter(|c| c.s("status") == "rolled_back").count();
    lines.push(format!("{} change{} carried out{}; {} in progress or due in the next twelve hours.", done.len(), if done.len() == 1 { "" } else { "s" }, if failed > 0 { format!(", {failed} rolled back") } else { String::new() }, next.len()));
    let soon = notices.iter().filter(|n| parse_iso(&n.s("window_start")) <= m + 86400).count();
    lines.push(format!("{soon} carrier maintenance window{} in the next day, {} of all that lie ahead collide with something.", if soon == 1 { "" } else { "s" }, found.len()));
    if let Some(h) = hot.first() {
        lines.push(format!("The fullest link was {} at {:.0}% of its capacity.", h.s("link"), h.f("peak") * 100.0));
    }
    q.done(json!({"at": iso(m), "began": iso(began), "since": iso(since), "oncall": oncall, "before": before, "briefing": lines, "work": work,
        "incidents": incidents, "pending": pending, "flaps": flapping, "done": done, "next": next, "notices": notices, "conflicts": found, "tickets": tickets,
        "notes": notes, "silences": active, "counts": {"firing": firing, "silenced": silenced, "loose": loose.len(), "open": open_now}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(text: &str) -> (String, String, Vec<String>) {
        let (a, b, notes) = notice_window(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        (iso(a), iso(b), notes)
    }

    #[test]
    fn a_notice_is_read_on_its_start_and_end_lines_not_on_when_it_was_sent() {
        let text = "From: noc@carrier.example\nSent: 2026-11-13 09:12 UTC\nSubject: Planned work Ref PW-88213 on LON-AMS\n\nDear customer,\n\nService ID: EVX-1001\nStart: 2026-11-20 01:00 UTC\nEnd: 2026-11-20 05:00 UTC\nExpected impact: outage of up to 30 minutes\n";
        let (a, b, notes) = window(text);
        assert_eq!((a.as_str(), b.as_str()), ("2026-11-20T01:00:00Z", "2026-11-20T05:00:00Z"));
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(notice_reference(text), "PW-88213", "the reference in the subject");
    }

    #[test]
    fn offsets_and_zones_are_applied() {
        // An ISO offset, and the same time in a zone's name.
        let (a, b, _) = window("Planned maintenance\nStart 2026-11-17T23:00:00+01:00\nEnd 2026-11-18T03:00:00+01:00\n");
        assert_eq!((a.as_str(), b.as_str()), ("2026-11-17T22:00:00Z", "2026-11-18T02:00:00Z"));
        let (a, b, _) = window("Start: 2026-11-17 23:00 CET\nEnd: 2026-11-18 03:00 CET\n");
        assert_eq!((a.as_str(), b.as_str()), ("2026-11-17T22:00:00Z", "2026-11-18T02:00:00Z"));
        // Summer time, and a zone given once for every time.
        let (a, _, _) = window("Begin: 2026-07-01 02:00 CEST\nFinish: 2026-07-01 04:00 CEST");
        assert_eq!(a, "2026-07-01T00:00:00Z");
        let (a, b, notes) = window("All times are in EST.\nStart: 2026-11-21 22:00\nEnd: 2026-11-22 02:00\n");
        assert_eq!((a.as_str(), b.as_str()), ("2026-11-22T03:00:00Z", "2026-11-22T07:00:00Z"));
        assert!(notes.iter().any(|n| n.contains("EST")), "{notes:?}");
        // A negative offset right after an ISO time.
        let (a, _, _) = window("Start: 2026-11-21T22:00:00-05:00\nEnd: 2026-11-22T02:00:00-05:00");
        assert_eq!(a, "2026-11-22T03:00:00Z");
        // UTC+1 and GMT.
        let (a, _, _) = window("Start: 2026-11-21 10:00 UTC+1\nEnd: 2026-11-21 11:00 UTC+1");
        assert_eq!(a, "2026-11-21T09:00:00Z");
    }

    #[test]
    fn a_window_on_one_line_and_an_end_without_a_date() {
        let (a, b, notes) = window("Circuit: EVX-1\nWindow: 2031-01-05 01:00 to 2031-01-05 03:00 (UTC)\n");
        assert_eq!((a.as_str(), b.as_str()), ("2031-01-05T01:00:00Z", "2031-01-05T03:00:00Z"));
        assert!(notes.len() == 1 && notes[0].contains("UTC"), "the zone at the end of the line holds for its first time too: {notes:?}");
        let (a, b, _) = window("Date: 2026-11-10 08:00 UTC\nStart: 2026-11-19 23:00 UTC\nEnd: 03:00 UTC\n");
        assert_eq!((a.as_str(), b.as_str()), ("2026-11-19T23:00:00Z", "2026-11-20T03:00:00Z"), "an end with no date after midnight is the next day");
        let (a, b, _) = window("Maintenance window: 2026-11-19 01:00-05:00 UTC\n");
        assert_eq!((a.as_str(), b.as_str()), ("2026-11-19T01:00:00Z", "2026-11-19T05:00:00Z"), "a range is not an offset");
        let (a, _, notes) = window("Start: 2026-11-19 01:00\nEnd: 2026-11-19 02:00\n");
        assert_eq!(a, "2026-11-19T01:00:00Z");
        assert_eq!(notes, ["no time zone given: read as UTC"]);
        let (a, b, _) = window("Start: 2026-11-19 11:00 PM UTC\nEnd: 2026-11-20 01:30 AM UTC\n");
        assert_eq!((a.as_str(), b.as_str()), ("2026-11-19T23:00:00Z", "2026-11-20T01:30:00Z"));
    }

    #[test]
    fn what_cannot_be_read_is_refused() {
        assert!(notice_window("Start: 2026-11-19 01:00 XYZT\nEnd: 2026-11-19 02:00 XYZT").unwrap_err().contains("XYZT"), "an unknown zone");
        assert!(notice_window("Start: 2026-11-19 05:00 UTC\nEnd: 2026-11-19 01:00 UTC").unwrap_err().contains("not after"));
        assert!(notice_window("Sent: 2026-11-13 09:12 UTC\nWe will work on your circuit next week.").is_err(), "the date it was sent is no window");
        assert_eq!(notice_reference("Reference: CHG-4411\n"), "CHG-4411");
        assert_eq!(notice_reference("Our ticket 99812 refers"), "99812");
        assert_eq!(notice_reference("No number here"), "");
    }
}
