//! Where is it broken? The network's state by layer of the stack and by
//! site, read from the last minutes of the tables, and a trace between two
//! points hop by hop.
//!
//! A fault low in the stack shows on every layer above it (a dark optic is
//! a down link, a lost adjacency, a failing service, a customer on the
//! phone), so the page answers two questions an alert list does not: at
//! which layer does the trouble begin, and where. The lowest layer that is
//! broken is where to start.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use crate::api::{bad, Answer, ApiError, Shared, Q};
use crate::strata::{Row, RowExt};
use crate::telemetry::{iso, round};

pub fn routes() -> Router<Shared> {
    Router::new().route("/v1/where", get(where_)).route("/v1/trace", get(trace)).route("/v1/investigate", get(investigate))
}

/// The layers, lowest first.
const LAYERS: &[(&str, &str, &str)] = &[
    ("power", "Power and reachability", "devices that answer"),
    ("optics", "Optics", "receive power on each link"),
    ("links", "Links", "state, packet loss and errors"),
    ("capacity", "Capacity", "load against what the link carries"),
    ("routing", "Routing", "BGP sessions and update churn"),
    ("devices", "Device health", "control-plane CPU and temperature"),
    ("services", "Services", "success and latency against the objective"),
    ("customers", "Customers", "attached where something below is broken"),
];

const BACKBONE: &str = "backbone";
const STATES: [&str; 3] = ["ok", "warn", "bad"];

fn rank(state: &str) -> u8 {
    match state {
        "bad" => 2,
        "warn" => 1,
        _ => 0,
    }
}

struct Readings {
    links: HashMap<String, Row>,
    devices: HashMap<String, Row>,
}

/// The latest minutes of every link and device: one query each.
async fn readings(q: &Q) -> std::result::Result<Readings, ApiError> {
    let p = json!({"since": iso(q.last() - 180)});
    let links = q.rows("Every link, last three minutes", "SELECT link, MAX(loss) AS loss, MAX(util) AS util, SUM(errors) AS errors, MIN(rx_dbm) AS rx, MIN(up) AS up, MAX(latency_ms) AS latency FROM netops_link_metrics\nWHERE ts >= :since\nGROUP BY link LIMIT 5000", p.clone()).await?;
    let devices = q.rows("Every device, last three minutes", "SELECT device, MAX(cpu) AS cpu, MAX(temp_c) AS temp, MIN(up) AS up, MIN(bgp_up) AS bgp_up, SUM(bgp_updates) AS updates FROM netops_device_metrics\nWHERE ts >= :since\nGROUP BY device LIMIT 5000", p).await?;
    Ok(Readings { links: links.into_iter().map(|r| (r.s("link"), r)).collect(), devices: devices.into_iter().map(|r| (r.s("device"), r)).collect() })
}

/// What a link's readings say, layer by layer: (layer, state, reading).
fn judge_link(r: &Row) -> Vec<(&'static str, &'static str, String)> {
    let mut out = Vec::new();
    let down = r.get("up").is_some_and(|v| !v.is_null()) && r.i("up") == 0;
    let rx = r.f("rx");
    if r.get("rx").is_some_and(|v| !v.is_null()) && rx < -9.0 {
        out.push(("optics", if rx < -12.0 { "bad" } else { "warn" }, if rx <= -30.0 { "no light (loss of signal)".to_string() } else { format!("receive power {rx:.1} dBm") }));
    }
    let (loss, errors) = (r.f("loss"), r.i("errors"));
    if down {
        out.push(("links", "bad", "down".into()));
    } else if loss > 0.1 || errors > 50 {
        // Loss is kept in percent; the rule pages above half a percent.
        out.push(("links", if loss > 0.5 || errors > 500 { "bad" } else { "warn" }, if loss > 0.1 { format!("{loss:.2}% loss, {errors} errors") } else { format!("{errors} errors in three minutes") }));
    }
    let util = r.f("util");
    if !down && util > 0.8 {
        out.push(("capacity", if util > 0.92 { "bad" } else { "warn" }, format!("{:.0}% full", util * 100.0)));
    }
    out
}

fn judge_device(r: &Row, sessions_before: Option<i64>) -> Vec<(&'static str, &'static str, String)> {
    let mut out = Vec::new();
    let down = r.get("up").is_some_and(|v| !v.is_null()) && r.i("up") == 0;
    if down {
        out.push(("power", "bad", "not answering".into()));
        return out;
    }
    if let Some(before) = sessions_before {
        let now = r.i("bgp_up");
        if now < before {
            out.push(("routing", if now == 0 || before - now >= 2 { "bad" } else { "warn" }, format!("{now} of {before} BGP sessions established")));
        }
    }
    let (cpu, temp) = (r.f("cpu"), r.f("temp"));
    if cpu > 75.0 || temp > 55.0 {
        out.push(("devices", if cpu > 90.0 || temp > 65.0 { "bad" } else { "warn" }, if cpu > 75.0 { format!("CPU {cpu:.0}%") } else { format!("{temp:.0} °C") }));
    }
    out
}

async fn where_(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let e = q.e.clone();
    let w = &e.w;
    let m = q.last();
    let r = readings(&q).await?;
    let before: HashMap<String, i64> = q.rows("BGP sessions each device had half an hour ago", "SELECT device, MAX(bgp_up) AS n FROM netops_device_metrics\nWHERE ts >= :a AND ts <= :b\nGROUP BY device LIMIT 5000", json!({"a": iso(m - 3600), "b": iso(m - 1500)})).await?.into_iter().map(|x| (x.s("device"), x.i("n"))).collect();
    let services = q.rows("Each service, last five minutes", "SELECT service, SUM(errors) AS errors, SUM(requests) AS requests, MAX(latency_p95) AS p95 FROM netops_service_metrics\nWHERE ts >= :since\nGROUP BY service LIMIT 500", json!({"since": iso(m - 300)})).await?;
    let alerts = q.rows("What fires now", "SELECT entity, rule_name, severity, incident, acked_by FROM netops_alerts\nWHERE status = 'firing' LIMIT 2000", json!({})).await?;
    let changes = q.rows("Changes carried out in the last six hours", "SELECT id, title, devices, executed FROM netops_changes\nWHERE executed >= :since AND executed <= :now\nORDER BY executed DESC LIMIT 100", json!({"since": iso(m - 6 * 3600), "now": iso(m)})).await?;
    // The estate behind the backbone: what its rules fire on now, the worst first.
    let mut estate: Vec<(u8, i64, Value)> = e.st().alerts.values()
        .filter(|a| a.s("entity_kind") == "estate" && a.s("status") == "firing")
        .map(|a| {
            let rank = match a.s("severity").as_str() { "critical" => 0, "major" => 1, _ => 2 };
            (rank, -a.f("reading") as i64, json!({"entity": a.s("entity"), "rule": a.s("rule_name"), "severity": a.s("severity"), "summary": a.s("summary"), "incident": a.s("incident")}))
        })
        .collect();
    estate.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let estate: Vec<Value> = estate.into_iter().take(50).map(|x| x.2).collect();
    let alert_of = |entity: &str| alerts.iter().find(|a| a.s("entity") == entity);
    let change_of = |devs: &[&str]| changes.iter().find(|c| c.list("devices").iter().any(|d| devs.contains(&d.as_str()))).map(|c| json!({"id": c.s("id"), "title": c.s("title"), "executed": c.s("executed")}));

    let mut off: Vec<Value> = Vec::new();
    let mut totals: HashMap<(&str, String), usize> = HashMap::new();
    let mut broken_devices: HashMap<String, &'static str> = HashMap::new();
    for l in w.links.iter().filter(|l| l.documented) {
        let (a, b) = (&w.devices[l.a], &w.devices[l.b]);
        let col = if a.site != b.site { BACKBONE.to_string() } else { a.site.to_string() };
        for layer in ["optics", "links", "capacity"] {
            *totals.entry((layer, col.clone())).or_default() += 1;
        }
        let Some(row) = r.links.get(&l.id) else { continue };
        for (layer, state, reading) in judge_link(row) {
            if layer == "links" && state == "bad" {
                for d in [&a.id, &b.id] {
                    broken_devices.entry(d.clone()).or_insert("warn");
                }
            }
            let al = alert_of(&l.id);
            off.push(json!({"layer": layer, "col": col, "kind": "link", "id": l.id, "state": state, "reading": reading,
                "where": if a.site != b.site { format!("{} to {}{}", a.site.to_uppercase(), b.site.to_uppercase(), if l.plane.is_empty() { String::new() } else { format!(", plane {}", l.plane) }) } else { a.site.to_uppercase() },
                "circuit": l.circuit, "provider": l.provider, "incident": al.map(|a| a.s("incident")).unwrap_or_default(), "alert": al.map(|a| a.s("rule_name")).unwrap_or_default(), "change": change_of(&[a.id.as_str(), b.id.as_str()])}));
        }
    }
    for d in &w.devices {
        for layer in ["power", "routing", "devices"] {
            *totals.entry((layer, d.site.to_string())).or_default() += 1;
        }
        let Some(row) = r.devices.get(&d.id) else { continue };
        for (layer, state, reading) in judge_device(row, before.get(&d.id).copied()) {
            if layer == "power" {
                broken_devices.insert(d.id.clone(), "bad");
            }
            let al = alert_of(&d.id);
            off.push(json!({"layer": layer, "col": d.site, "kind": "device", "id": d.id, "state": state, "reading": reading, "where": format!("{} {}", d.site.to_uppercase(), d.role),
                "incident": al.map(|a| a.s("incident")).unwrap_or_default(), "alert": al.map(|a| a.s("rule_name")).unwrap_or_default(), "change": change_of(&[d.id.as_str()])}));
        }
    }
    for s in &w.services {
        for site in &s.sites {
            *totals.entry(("services", site.to_string())).or_default() += 1;
        }
        let Some(row) = services.iter().find(|x| x.s("service") == s.id) else { continue };
        let (errors, requests) = (row.f("errors"), row.f("requests").max(1.0));
        let rate = errors / requests;
        let slow = s.latency_slo_ms > 0.0 && row.f("p95") > s.latency_slo_ms;
        if rate > (1.0 - s.slo) || slow {
            let state = if rate > 5.0 * (1.0 - s.slo) { "bad" } else { "warn" };
            let reading = if rate > (1.0 - s.slo) { format!("{:.2}% failing (objective {:.2}%)", rate * 100.0, s.slo * 100.0) } else { format!("p95 {:.0} ms (objective {:.0} ms)", row.f("p95"), s.latency_slo_ms) };
            let al = alert_of(s.id);
            // A service is drawn at each site it runs in where something below is broken, or at all of them.
            let hit: Vec<&&str> = s.sites.iter().filter(|site| off.iter().any(|o| o["col"] == **site && o["state"] == "bad")).collect();
            let sites: Vec<&str> = if hit.is_empty() { s.sites.clone() } else { hit.into_iter().copied().collect() };
            for site in sites {
                off.push(json!({"layer": "services", "col": site, "kind": "service", "id": s.id, "state": state, "reading": reading, "where": s.name,
                    "incident": al.map(|a| a.s("incident")).unwrap_or_default(), "alert": al.map(|a| a.s("rule_name")).unwrap_or_default(), "change": Value::Null}));
            }
        }
    }
    for c in &w.customers {
        *totals.entry(("customers", c.site.to_string())).or_default() += 1;
        let dev = &w.devices[c.device].id;
        // A customer is touched when a service they buy is failing where they attach, or their own router is in trouble.
        let failing: Vec<(String, String)> = off.iter().filter(|o| o["layer"] == "services" && o["col"] == c.site && c.services.iter().any(|s| o["id"] == *s)).map(|o| (o["where"].as_str().unwrap_or("").to_string(), o["state"].as_str().unwrap_or("warn").to_string())).collect();
        if let (None, Some((svc_name, state))) = (broken_devices.get(dev), failing.first()) {
            off.push(json!({"layer": "customers", "col": c.site, "kind": "customer", "id": c.id, "state": state, "reading": format!("buys {svc_name}, which is failing here"), "where": c.name, "incident": "", "alert": "", "change": Value::Null}));
        }
        if let Some(state) = broken_devices.get(dev) {
            off.push(json!({"layer": "customers", "col": c.site, "kind": "customer", "id": c.id, "state": state, "reading": if *state == "bad" { format!("attached to {dev}, which is not answering") } else { format!("attached to {dev}, which lost a link") },
                "where": c.name, "incident": "", "alert": "", "change": Value::Null}));
        }
    }
    // BGP sessions the table says are not established.
    for b in q.rows("BGP sessions that are not established", "SELECT id, device, peer_name, kind, state FROM netops_bgp\nWHERE state != 'established' LIMIT 500", json!({})).await? {
        let Some(d) = w.device(&b.s("device")) else { continue };
        if off.iter().any(|o| o["layer"] == "routing" && o["id"] == d.id.as_str()) || broken_devices.get(&d.id) == Some(&"bad") {
            continue;
        }
        off.push(json!({"layer": "routing", "col": d.site, "kind": "device", "id": d.id, "state": "bad", "reading": format!("session with {} ({}) is {}", b.s("peer_name"), b.s("kind"), b.s("state")), "where": format!("{} {}", d.site.to_uppercase(), d.role),
            "incident": alert_of(&d.id).map(|a| a.s("incident")).unwrap_or_default(), "alert": "", "change": change_of(&[d.id.as_str()])}));
    }

    // The matrix: for each layer and place, the worst state and how many.
    let mut cells: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let mut cols: Vec<Value> = w.sites.iter().map(|s| json!({"id": s.id, "name": s.id.to_uppercase(), "city": s.city})).collect();
    cols.push(json!({"id": BACKBONE, "name": "Backbone", "city": "between sites"}));
    for (layer, _, _) in LAYERS {
        for c in &cols {
            let col = c["id"].as_str().unwrap_or("");
            let here: Vec<&Value> = off.iter().filter(|o| o["layer"] == *layer && o["col"] == col).collect();
            let worst = here.iter().map(|o| rank(o["state"].as_str().unwrap_or(""))).max().unwrap_or(0);
            let total = totals.get(&(*layer, col.to_string())).copied().unwrap_or(0);
            cells.entry(layer.to_string()).or_default().insert(col.to_string(), json!({"state": if total == 0 { "none" } else { STATES[worst as usize] }, "n": here.len(), "total": total}));
        }
    }
    // Where to start: the lowest layer with something bad (else something
    // worn), and within it the place with the most.
    let mut start = Value::Null;
    'find: for want in ["bad", "warn"] {
        for (layer, name, _) in LAYERS.iter().filter(|l| l.0 != "customers") {
            let here: Vec<&Value> = off.iter().filter(|o| o["layer"] == *layer && o["state"] == want).collect();
            if here.is_empty() {
                continue;
            }
            let mut by: HashMap<&str, usize> = HashMap::new();
            for o in &here {
                *by.entry(o["col"].as_str().unwrap_or("")).or_default() += 1;
            }
            let col = by.iter().max_by_key(|(c, n)| (**n, std::cmp::Reverse(**c))).map(|(c, _)| *c).unwrap_or("");
            let first = here.iter().find(|o| o["col"] == col).copied().cloned().unwrap_or(Value::Null);
            let above = LAYERS.iter().skip_while(|l| l.0 != *layer).skip(1).filter(|l| off.iter().any(|o| o["layer"] == l.0)).map(|l| l.1.to_lowercase()).collect::<Vec<_>>();
            start = json!({"layer": layer, "layer_name": name, "col": col, "state": want, "count": here.len(), "first": first, "above": above});
            break 'find;
        }
    }
    off.sort_by_key(|o| (LAYERS.iter().position(|l| o["layer"] == l.0).unwrap_or(9), std::cmp::Reverse(rank(o["state"].as_str().unwrap_or("")))));
    q.done(json!({"at": iso(m), "layers": LAYERS.iter().map(|l| json!({"id": l.0, "name": l.1, "what": l.2})).collect::<Vec<_>>(), "cols": cols, "cells": cells, "offenders": off, "start": start,
        "changes": changes.iter().map(|c| json!({"id": c.s("id"), "title": c.s("title"), "executed": c.s("executed"), "devices": c.list("devices")})).collect::<Vec<_>>(), "firing": alerts.len(), "estate": estate}))
}

/// A device for whatever names one end of a trace: a device, a customer
/// (its id, name, circuit or prefix), or a site (its first core router).
fn end_of(q: &Q, text: &str) -> Option<(String, String)> {
    let w = &q.e.w;
    let t = text.trim().to_lowercase();
    if let Some(d) = w.device(&t) {
        return Some((d.id.clone(), d.id.clone()));
    }
    if let Some(c) = w.customers.iter().find(|c| c.id == t || c.name.to_lowercase() == t || c.circuit.to_lowercase() == t || c.prefix == t) {
        return Some((w.devices[c.device].id.clone(), format!("{} ({})", c.name, w.devices[c.device].id)));
    }
    if let Some(s) = w.sites.iter().find(|s| s.id == t || s.city.to_lowercase() == t) {
        let d = w.devices.iter().find(|d| d.site == s.id && d.role == "core").or_else(|| w.devices.iter().find(|d| d.site == s.id))?;
        return Some((d.id.clone(), format!("{} ({})", s.city, d.id)));
    }
    None
}

async fn trace(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let e = q.e.clone();
    let w = &e.w;
    let (Some((from, from_name)), Some((to, to_name))) = (end_of(&q, p.get("from").map(String::as_str).unwrap_or("")), end_of(&q, p.get("to").map(String::as_str).unwrap_or(""))) else {
        return Err(bad("from and to are each a device, a customer (name, id, circuit) or a site"));
    };
    let sql = "SELECT id, hops() AS hop FROM netops_devices\nWHERE PATH(netops_links, :a, :b, dir => 'both', max_hops => 12)\nORDER BY hops()";
    let (devs, engine): (Vec<String>, &str) = match e.db.traced(&q.trace, "Shortest path over the links table", sql, json!({"a": from, "b": to})).await {
        Ok(rows) if !rows.is_empty() => (rows.iter().map(|r| r.s("id")).collect(), "sql"),
        _ => (e.graph.path(&from, &to), "app"),
    };
    if devs.is_empty() {
        return Err(bad("the inventory has no path between them"));
    }
    let r = readings(&q).await?;
    let mut hops = Vec::new();
    let mut worst: (u8, usize) = (0, 0);
    let mut latency = 0.0;
    for (i, id) in devs.iter().enumerate() {
        let d = w.device(id);
        let dev: Vec<_> = r.devices.get(id).map(|row| judge_device(row, None)).unwrap_or_default();
        let mut state = dev.iter().map(|x| rank(x.1)).max().unwrap_or(0);
        let mut findings: Vec<String> = dev.iter().map(|x| x.2.clone()).collect();
        // The link this hop was reached over: every parallel link between the two, the best of them carrying.
        let mut link = Value::Null;
        if i > 0 {
            let prev = &devs[i - 1];
            let between: Vec<_> = w.links.iter().filter(|l| l.documented && ((w.devices[l.a].id == *prev && w.devices[l.b].id == *id) || (w.devices[l.b].id == *prev && w.devices[l.a].id == *id))).collect();
            let judged: Vec<(u8, Vec<String>, &crate::world::Link)> = between.iter().map(|l| { let j = r.links.get(&l.id).map(judge_link).unwrap_or_default(); (j.iter().map(|x| rank(x.1)).max().unwrap_or(0), j.into_iter().map(|x| x.2).collect(), *l) }).collect();
            if let Some((s, f, l)) = judged.iter().min_by_key(|x| x.0) {
                let row = r.links.get(&l.id);
                latency += row.map(|x| x.f("latency")).filter(|v| *v > 0.0).unwrap_or(l.latency_ms);
                link = json!({"id": l.id, "state": STATES[*s as usize], "findings": f, "loss": row.map(|x| x.f("loss")), "util": row.map(|x| round(x.f("util"), 3)), "latency_ms": row.map(|x| round(x.f("latency"), 1)),
                    "errors": row.map(|x| x.i("errors")), "circuit": l.circuit, "provider": l.provider, "parallel": between.len(), "kind": l.kind});
                if *s > worst.0 {
                    worst = (*s, i);
                }
                if *s > 0 {
                    findings.extend(f.iter().map(|x| format!("{}: {x}", l.id)));
                }
                state = state.max(*s);
            }
        }
        if dev.iter().map(|x| rank(x.1)).max().unwrap_or(0) > worst.0 {
            worst = (state, i);
        }
        let row = r.devices.get(id);
        hops.push(json!({"n": i, "device": id, "site": d.map(|d| d.site).unwrap_or(""), "role": d.map(|d| d.role).unwrap_or(""), "state": STATES[state as usize], "findings": findings,
            "cpu": row.map(|x| round(x.f("cpu"), 0)), "link": link}));
    }
    let verdict = match worst.0 {
        0 => format!("Clean: {} hops, nothing wrong on any of them in the last three minutes", devs.len() - 1),
        s => format!("{} at hop {}: {}", if s == 2 { "Broken" } else { "Worn" }, worst.1, hops[worst.1]["findings"].as_array().and_then(|f| f.first()).and_then(|x| x.as_str()).unwrap_or("see the hop")),
    };
    q.done(json!({"from": from_name, "to": to_name, "hops": hops, "engine": engine, "verdict": verdict, "state": STATES[worst.0 as usize], "worst": worst.1, "latency_ms": round(latency, 1)}))
}

// ---- one broken thing, from every side

/// Runs one group of queries and says which of ChloeDB's abilities answered
/// it, with what it cost.
struct Lens {
    out: Vec<Value>,
}

impl Lens {
    fn add(&mut self, id: &str, feature: &str, how: &str, t0: std::time::Instant, queries: usize, body: Value) {
        self.out.push(json!({"id": id, "feature": feature, "how": how, "ms": round(t0.elapsed().as_secs_f64() * 1000.0, 1), "queries": queries, "data": body}));
    }
}

/// Everything about one link, device or service that is misbehaving, asked
/// of the same database in the ways only it can be asked together: a graph
/// walk for what it takes down and what the symptoms have in common, a
/// substring search for what every device's log says about it, array
/// overlap and configuration history for what changed, a search by words
/// and meaning for when it happened before, and the minutes around it.
async fn investigate(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let e = q.e.clone();
    let w = &e.w;
    let m = q.last();
    let (kind, id) = (p.get("kind").map(String::as_str).unwrap_or(""), p.get("id").cloned().unwrap_or_default());
    // The devices it is about, the words its log lines carry, and what it looks like now.
    let (devices, needles, mut symptom, title): (Vec<String>, Vec<(String, String)>, String, String) = match kind {
        "link" => {
            let Some(li) = w.link_by_id(&id) else { return Err(bad("no such link")) };
            let l = &w.links[li];
            let (a, b) = (w.devices[l.a].id.clone(), w.devices[l.b].id.clone());
            (vec![a.clone(), b.clone()], vec![(a.clone(), l.a_if.clone()), (b.clone(), l.z_if.clone())], String::new(), format!("{} ({} {} to {} {})", l.id, a, l.a_if, b, l.z_if))
        }
        "device" => {
            let Some(d) = w.device(&id) else { return Err(bad("no such device")) };
            (vec![d.id.clone()], vec![(String::new(), d.id.clone())], String::new(), format!("{} ({} at {})", d.id, d.role, d.site.to_uppercase()))
        }
        "service" => {
            let Some(s) = w.services.iter().find(|s| s.id == id) else { return Err(bad("no such service")) };
            (s.devices.iter().map(|&d| w.devices[d].id.clone()).collect(), Vec::new(), format!("{} failing requests", s.name), s.name.to_string())
        }
        _ => return Err(bad("kind is link, device or service")),
    };
    let r = readings(&q).await?;
    let mut now: Vec<String> = Vec::new();
    if kind == "link" {
        now.extend(r.links.get(&id).map(judge_link).unwrap_or_default().into_iter().map(|x| x.2));
    }
    if kind == "device" {
        now.extend(r.devices.get(&id).map(|row| judge_device(row, None)).unwrap_or_default().into_iter().map(|x| x.2));
    }
    let firing = q.rows("What fires now, and on what", "SELECT entity, rule_name, severity, devices, summary, incident FROM netops_alerts\nWHERE status = 'firing' LIMIT 2000", json!({})).await?;
    let mine: Vec<&Row> = firing.iter().filter(|a| a.s("entity") == id || (kind != "service" && a.list("devices").iter().any(|d| devices.contains(d)))).collect();
    let inc = mine.iter().map(|a| a.s("incident")).find(|i| !i.is_empty()).unwrap_or_default();
    // What it reads itself comes first, then the rules that fire on it: the
    // rest of its incident's alerts are symptoms and would pull the search toward them.
    let _ = &inc;
    let mut rules: Vec<String> = firing.iter().filter(|a| a.s("entity") == id).map(|a| a.s("rule_name")).collect();
    rules.sort();
    rules.dedup();
    symptom = [symptom, now.join(", "), rules.join(", ")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("; ");
    if symptom.is_empty() {
        symptom = p.get("q").cloned().unwrap_or_else(|| format!("{id} degraded"));
    }
    let nums: Vec<String> = devices.iter().filter_map(|d| w.device(d)).map(|d| d.num.to_string()).collect();
    let nums = if nums.is_empty() { "0".to_string() } else { nums.join(", ") };
    let seedsql = |s: &[String]| if s.len() == 1 { crate::agents::quoted(s) } else { format!("({})", crate::agents::quoted(s)) };
    let mut lens = Lens { out: Vec::new() };

    // 1. The graph: what the symptoms have in common, and what this takes down.
    let t0 = std::time::Instant::now();
    let mut seeds: Vec<String> = firing.iter().flat_map(|a| a.list("devices")).filter(|d| w.device(d).is_some()).collect();
    seeds.extend(devices.iter().cloned());
    seeds.sort();
    seeds.dedup();
    seeds.truncate(40);
    let s = seedsql(&seeds);
    let sql = format!("SELECT id, ppr(netops_links, {s}) AS score FROM netops_devices\nORDER BY ppr(netops_links, {s}) DESC LIMIT 6");
    let (suspects, engine): (Vec<Value>, &str) = match e.db.traced(&q.trace, "The devices everything alerting is closest to (personalised PageRank over the links table)", &sql, json!({})).await {
        Ok(rows) => (rows.into_iter().map(Value::Object).collect(), "sql"),
        Err(_) => (e.graph.ppr(&seeds).into_iter().take(6).map(|(id, v)| json!({"id": id, "score": v})).collect(), "app"),
    };
    let d = seedsql(&devices);
    let sql = format!("SELECT id, hops() AS h FROM netops_devices\nWHERE REACHABLE(netops_links, {d}, hops => 2, dir => 'both')\nORDER BY hops(), id LIMIT 200");
    let near: Vec<(String, i64)> = match e.db.traced(&q.trace, "Devices within two hops (REACHABLE over the links table)", &sql, json!({})).await {
        Ok(rows) => rows.iter().map(|r| (r.s("id"), r.i("h"))).collect(),
        Err(_) => e.graph.reach(&devices, 2).into_iter().map(|(id, h)| (id, h as i64)).collect(),
    };
    let services = q.rows("Services that ride on it (INT[] overlap)", &format!("SELECT id, name, tier, revenue_per_min, customers FROM netops_services\nWHERE device_nums && [{nums}]\nORDER BY tier LIMIT 20"), json!({})).await?;
    let one_hop: Vec<String> = devices.iter().cloned().chain(near.iter().filter(|x| x.1 == 1).map(|x| x.0.clone())).collect();
    let customers = q.rows("Customers attached on it or one hop away", &format!("SELECT name, tier, device, mrr FROM netops_customers\nWHERE device IN ({})\nORDER BY mrr DESC LIMIT 8", crate::agents::quoted(&one_hop)), json!({})).await?;
    let total = suspects.first().map(|x| x["score"].as_f64().unwrap_or(0.0)).unwrap_or(0.0).max(1e-9);
    lens.add("graph", "Graph queries in SQL", if engine == "sql" { "ppr() and REACHABLE() over the links table, no graph database" } else { "walked by the app: a coordinator does not walk graphs yet" }, t0, 4, json!({
        "suspects": suspects.iter().map(|x| json!({"id": x["id"], "share": round(x["score"].as_f64().unwrap_or(0.0) / total, 2), "alerting": firing.iter().any(|a| a.list("devices").iter().any(|d| x["id"] == d.as_str())), "this": devices.iter().any(|d| x["id"] == d.as_str())})).collect::<Vec<_>>(),
        "seeds": seeds.len(), "near": {"one": near.iter().filter(|x| x.1 == 1).count(), "two": near.iter().filter(|x| x.1 == 2).count()}, "services": services, "customers": customers}));

    // 2. The substring index: what any device's log says about it.
    let t0 = std::time::Instant::now();
    let since = json!({"since": iso(m - 6 * 3600)});
    let clean = |s: &str| s.replace(['\'', '%', '\\'], "");
    let cond = if needles.is_empty() {
        format!("device IN ({}) AND sev <= 4", crate::agents::quoted(&devices))
    } else {
        needles.iter().map(|(dev, n)| if dev.is_empty() { format!("message LIKE '%{}%'", clean(n)) } else { format!("(device = '{}' AND message LIKE '%{}%')", clean(dev), clean(n)) }).collect::<Vec<_>>().join(" OR ")
    };
    let lines = q.rows("Log lines that name it, newest first (LIKE from the substring index)", &format!("SELECT ts, device, severity, sev, template, message FROM netops_syslog\nWHERE ts >= :since AND ({cond})\nORDER BY ts DESC LIMIT 14"), since.clone()).await?;
    let kinds = q.rows("The same lines, counted by kind and by who wrote them", &format!("SELECT template, device, COUNT(*) AS n, MIN(ts) AS first FROM netops_syslog\nWHERE ts >= :since AND ({cond})\nGROUP BY template, device ORDER BY n DESC LIMIT 12"), since).await?;
    lens.add("logs", "Substring search", "LIKE '%…%' answered from an FM-index over the log text: half an interface name finds its lines without a scan", t0, 2, json!({
        "needle": if needles.is_empty() { "errors on its devices".to_string() } else { needles.iter().map(|n| n.1.clone()).collect::<Vec<_>>().join(", ") }, "lines": lines, "kinds": kinds,
        "others": kinds.iter().filter(|k| !devices.contains(&k.s("device"))).map(|k| k.s("device")).collect::<std::collections::BTreeSet<_>>()}));

    // 3. What changed: arrays that overlap, and every configuration version kept.
    let t0 = std::time::Instant::now();
    let changes = q.rows("Changes whose devices overlap these (INT[] &&), last three days", &format!("SELECT id, title, status, executed, owner, risk FROM netops_changes\nWHERE executed >= :since AND executed <= :now AND device_nums && [{nums}]\nORDER BY executed DESC LIMIT 8"), json!({"since": iso(m - 3 * 86400), "now": iso(m)})).await?;
    let versions = q.rows("Configuration versions written on them, last three days", &format!("SELECT device, version, ts, author, change_id, lines FROM netops_configs\nWHERE ts >= :since AND device IN ({})\nORDER BY ts DESC LIMIT 8", crate::agents::quoted(&devices)), json!({"since": iso(m - 3 * 86400)})).await?;
    let mut mentions = Vec::new();
    if kind == "link" {
        for (dev, n) in &needles {
            mentions.extend(q.soft("Configuration versions that mention its interface (substring)", "SELECT device, version, ts, change_id FROM netops_configs\nWHERE device = :d AND body LIKE :n\nORDER BY ts DESC LIMIT 2", json!({"d": dev, "n": format!("%{}%", clean(n))})).await);
        }
    }
    lens.add("changed", "History as rows", "every change and every configuration version is a row: array overlap finds the changes, a substring finds the lines", t0, 2 + needles.len().min(2) * usize::from(kind == "link"), json!({"changes": changes, "versions": versions, "mentions": mentions}));

    // 4. Has it happened before: words and meaning in one ORDER BY.
    let t0 = std::time::Instant::now();
    let semantic = e.st().semantic;
    let score = |field: &str| if semantic { format!("rrf(bm25({field}, :q), similarity(emb, :q))") } else { format!("bm25({field}, :q)") };
    let text = format!("{symptom} {}", devices.join(" "));
    let before = q.soft("Past incidents like this (words and meaning, fused)", &format!("SELECT id, title, category, started, root_cause, mttr_min, runbook FROM netops_incidents\nWHERE status = 'resolved'\nORDER BY {} DESC LIMIT 4", score("summary")), json!({"q": text})).await;
    let runbooks = q.soft("Runbooks for it (words and meaning, fused)", &format!("SELECT id, title, category FROM netops_runbooks\nORDER BY {} DESC LIMIT 3", score("body")), json!({"q": symptom})).await;
    lens.add("before", "Hybrid search", if semantic { "BM25 and a static embedding fused with rrf() in the ORDER BY, the model inside the server" } else { "BM25 (no embedding model on this server)" }, t0, 2, json!({"asked": symptom, "incidents": before, "runbooks": runbooks}));

    // 5. The minutes around it.
    let t0 = std::time::Instant::now();
    let hour = json!({"since": iso(m - 3600), "id": id});
    let series = match kind {
        "link" => q.rows("Its last hour, a point a minute", "SELECT ts, loss, util, errors, rx_dbm, up FROM netops_link_metrics\nWHERE link = :id AND ts >= :since ORDER BY ts LIMIT 200", hour).await?,
        "device" => q.rows("Its last hour, a point a minute", "SELECT ts, cpu, temp_c, bgp_up, up FROM netops_device_metrics\nWHERE device = :id AND ts >= :since ORDER BY ts LIMIT 200", hour).await?,
        _ => q.rows("Its last hour, a point a minute", "SELECT ts, requests, errors, latency_p95 FROM netops_service_metrics\nWHERE service = :id AND ts >= :since ORDER BY ts LIMIT 200", hour).await?,
    };
    lens.add("time", "Time series beside everything else", "a point a minute kept for three days in the same database, so the reading sits next to the log line and the change", t0, 1, json!({"series": series}));

    q.done(json!({"kind": kind, "id": id, "title": title, "now": now, "symptom": symptom, "alerts": mine.iter().map(|a| json!({"rule": a.s("rule_name"), "severity": a.s("severity"), "incident": a.s("incident")})).collect::<Vec<_>>(),
        "incident": mine.iter().map(|a| a.s("incident")).find(|i| !i.is_empty()).unwrap_or_default(), "lenses": lens.out, "at": iso(m)}))
}
