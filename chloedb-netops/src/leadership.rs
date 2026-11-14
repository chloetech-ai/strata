//! The same tables from further away: what a head of network is asked in a
//! review. Are we meeting what we sold, what did incidents cost, where is
//! the risk, are the carriers delivering, is change safe, is the team
//! coping. Every number is a query over rows an engineer can open, and the
//! answer says over how much telemetry it was measured.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use crate::agents::quoted;
use crate::api::{Answer, Shared, Q};
use crate::strata::{Row, RowExt};
use crate::telemetry::{iso, parse_iso, round};

pub fn routes() -> Router<Shared> {
    Router::new().route("/v1/leadership", get(leadership))
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") }
}

/// The objectives in the written week: the rate measured over the hours of
/// telemetry kept, and the incident minutes of the period against what each
/// objective allows, each with its own window.
fn objectives_line(total: usize, below: &[&str], over: &[&str], kept_hours: f64, days: i64) -> String {
    let measured = if below.is_empty() {
        format!("all {total} services are inside their objective")
    } else {
        format!("{} of {total} {} ({})", below.len(), if below.len() == 1 { "service is below its objective" } else { "services are below their objective" }, below.join(", "))
    };
    let period = if over.is_empty() {
        "no service has had more incident minutes than its objective allows".to_string()
    } else {
        format!("{} {} had more incident minutes than {} objective allows ({})", over.len(), if over.len() == 1 { "service has" } else { "services have" }, if over.len() == 1 { "its" } else { "their" }, over.join(", "))
    };
    format!("Objectives. Measured over the last {kept_hours} h of telemetry, {measured}. Over the {days} days of incidents, {period}.")
}

async fn leadership(State(svc): State<Shared>, Query(p): Query<HashMap<String, String>>) -> Answer {
    let q = Q::new(&svc);
    let e = q.e.clone();
    let w = &e.w;
    let m = q.last();
    let days = p.get("days").and_then(|d| d.parse::<i64>().ok()).unwrap_or(30).clamp(7, 90);
    let since = m - days * 86400;
    let week = m - 7 * 86400;

    // ---- what we sold
    let svc_rows = q.rows("Services and their objectives", "SELECT id, name, tier, owner, slo_target, latency_slo_ms, revenue_per_min, customers FROM netops_services ORDER BY tier, name LIMIT 100", json!({})).await?;
    let measured = q.rows("Each service over the telemetry kept: requests, errors, the slowest minute", "SELECT service, SUM(requests) AS requests, SUM(errors) AS errors, MAX(latency_p95) AS worst_p95, AVG(latency_p95) AS p95, MIN(ts) AS first, COUNT(*) AS minutes FROM netops_service_metrics\nGROUP BY service LIMIT 100", json!({})).await?;
    let incidents = q.rows("Incidents of the period", "SELECT id, title, severity, status, category, started, resolved, mttr_min, ttd_s, customers, revenue_at_risk, services, sites, change_id, root_kind, root_entity, commander, origin FROM netops_incidents\nWHERE started >= :since\nORDER BY started LIMIT 1000", json!({"since": iso(since)})).await?;
    let first = measured.iter().map(|r| r.s("first")).filter(|s| !s.is_empty()).min().unwrap_or_default();
    let window_h = if first.is_empty() { 0.0 } else { round((m - parse_iso(&first)) as f64 / 3600.0, 1) };
    let services: Vec<Value> = svc_rows
        .iter()
        .map(|s| {
            let id = s.s("id");
            let r = measured.iter().find(|r| r.s("service") == id);
            let (req, err) = r.map_or((0.0, 0.0), |r| (r.f("requests"), r.f("errors")));
            let success = if req > 0.0 { 1.0 - err / req } else { 1.0 };
            let allowed = (1.0 - s.f("slo_target")) * req;
            let mine: Vec<&Row> = incidents.iter().filter(|i| i.list("services").contains(&id)).collect();
            let minutes: f64 = mine.iter().map(|i| i.f("mttr_min")).sum();
            // Over the period, from the incidents that touched it: the
            // minutes it was degraded against the minutes its objective allows.
            let allowed_min = (1.0 - s.f("slo_target")) * (days * 1440) as f64;
            json!({"id": id, "name": s.s("name"), "tier": s.i("tier"), "owner": s.s("owner"), "slo_target": s.f("slo_target"), "customers": s.i("customers"), "revenue_per_min": s.f("revenue_per_min"),
                "success": round(success, 6), "met": success >= s.f("slo_target"), "budget_used": if allowed > 0.0 { round(err / allowed, 3) } else { 0.0 },
                "p95": r.map_or(0.0, |r| round(r.f("p95"), 1)), "latency_slo_ms": s.f("latency_slo_ms"),
                "incidents": mine.len(), "degraded_min": round(minutes, 0), "allowed_min": round(allowed_min, 0), "period_used": if allowed_min > 0.0 { round(minutes / allowed_min, 2) } else { 0.0 },
                "revenue_exposed": round(minutes * s.f("revenue_per_min"), 0)})
        })
        .collect();

    // ---- what incidents cost
    let n = incidents.len();
    let mean = |f: &dyn Fn(&Row) -> f64, rows: &[&Row]| if rows.is_empty() { 0.0 } else { rows.iter().map(|r| f(r)).sum::<f64>() / rows.len() as f64 };
    let all: Vec<&Row> = incidents.iter().collect();
    let resolved: Vec<&Row> = incidents.iter().filter(|i| i.s("status") == "resolved").collect();
    let customer_minutes: f64 = resolved.iter().map(|i| i.f("customers") * i.f("mttr_min")).sum();
    let exposed: f64 = incidents.iter().map(|i| i.f("revenue_at_risk")).sum();
    let mut weeks: Vec<Value> = Vec::new();
    let mut k = since;
    while k < m {
        let rows: Vec<&Row> = resolved.iter().filter(|i| { let t = parse_iso(&i.s("started")); t >= k && t < k + 7 * 86400 }).cloned().collect();
        weeks.push(json!({"from": iso(k), "incidents": rows.len(), "sev1": rows.iter().filter(|i| i.s("severity") == "sev1").count(), "mttr": round(mean(&|r| r.f("mttr_min"), &rows), 1), "ttd": round(mean(&|r| r.f("ttd_s"), &rows), 0)}));
        k += 7 * 86400;
    }
    let mut by_cat: BTreeMap<String, Vec<&Row>> = BTreeMap::new();
    let mut by_root: BTreeMap<String, Vec<&Row>> = BTreeMap::new();
    for i in &incidents {
        by_cat.entry(i.s("category")).or_default().push(i);
        if !i.s("root_entity").is_empty() {
            by_root.entry(i.s("root_entity")).or_default().push(i);
        }
    }
    let mut causes: Vec<Value> = by_cat.iter().map(|(c, rows)| json!({"category": c, "incidents": rows.len(), "mttr": round(mean(&|r| r.f("mttr_min"), rows), 1), "minutes": round(rows.iter().map(|r| r.f("mttr_min")).sum::<f64>(), 0),
        "customer_minutes": round(rows.iter().map(|r| r.f("customers") * r.f("mttr_min")).sum::<f64>(), 0)})).collect();
    causes.sort_by(|a, b| b["minutes"].as_f64().partial_cmp(&a["minutes"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    let repeats: Vec<Value> = by_root.iter().filter(|(_, r)| r.len() >= 2).map(|(k, r)| json!({"entity": k, "kind": r[0].s("root_kind"), "incidents": r.iter().map(|i| i.s("id")).collect::<Vec<_>>(), "category": r[0].s("category")})).collect();
    let by_change: Vec<Value> = incidents.iter().filter(|i| !i.s("change_id").is_empty()).map(|i| json!({"incident": i.s("id"), "change": i.s("change_id"), "title": i.s("title"), "minutes": i.f("mttr_min")})).collect();
    let out_of_hours = incidents.iter().filter(|i| { let h = (parse_iso(&i.s("started")).rem_euclid(86400)) / 3600; !(7..19).contains(&h) }).count();
    let mut pages: BTreeMap<String, usize> = BTreeMap::new();
    for i in incidents.iter().filter(|i| !i.s("commander").is_empty()) {
        *pages.entry(i.s("commander")).or_default() += 1;
    }

    // ---- where the risk is
    let backbone: Vec<String> = w.links.iter().filter(|l| l.kind == "backbone" && l.live && l.documented).map(|l| l.id.clone()).collect();
    let peaks = q.rows("Each backbone link's peak over the telemetry kept", &format!("SELECT link, MAX(util) AS peak, AVG(util) AS mean FROM netops_link_metrics\nWHERE up = 1 AND link IN ({})\nGROUP BY link LIMIT 500", quoted(&backbone)), json!({})).await?;
    let peak = |id: &str| peaks.iter().find(|r| r.s("link") == id).map_or(0.0, |r| r.f("peak"));
    let mut failover: Vec<Value> = Vec::new();
    for (i, l) in w.links.iter().enumerate().filter(|(_, l)| l.kind == "backbone" && l.live && l.documented && l.plane == "A") {
        let Some(si) = l.sibling else { continue };
        let s = &w.links[si];
        let load = (peak(&l.id) * l.capacity_mbps as f64 + peak(&s.id) * s.capacity_mbps as f64) / (l.capacity_mbps.min(s.capacity_mbps) as f64);
        let _ = i;
        failover.push(json!({"span": format!("{}-{}", w.devices[l.a].site.to_uppercase(), w.devices[l.b].site.to_uppercase()), "a": l.id, "b": s.id, "load": round(load, 3), "capacity_gbps": l.capacity_mbps / 1000,
            "survives": load <= 1.0, "services": w.services.iter().filter(|x| x.devices.contains(&l.a) || x.devices.contains(&l.b)).count()}));
    }
    failover.sort_by(|a, b| b["load"].as_f64().partial_cmp(&a["load"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    // An advisory at a time: ChloeDB does not compare a column with = ANY over a joined array column.
    let advisories = {
        {
            let mut out = Vec::new();
            for a in q.rows("Vendor advisories", "SELECT id, os, affected, fixed_in, severity, cvss, title FROM netops_advisories ORDER BY cvss DESC LIMIT 50", json!({})).await? {
                let versions = a.list("affected");
                if versions.is_empty() { continue; }
                let d = q.rows(&format!("Devices on a version {} names", a.s("id")), &format!("SELECT COUNT(*) AS n FROM netops_devices\nWHERE os = :os AND os_version IN ({})", quoted(&versions)), json!({"os": a.s("os")})).await?;
                let n = d.first().map_or(0, |r| r.i("n"));
                if n > 0 {
                    let mut r = Row::new();
                    for (k, v) in [("advisory", json!(a.s("id"))), ("title", json!(a.s("title"))), ("severity", json!(a.s("severity"))), ("cvss", json!(a.f("cvss"))), ("fixed_in", json!(a.s("fixed_in"))), ("devices", json!(n))] {
                        r.insert(k.into(), v);
                    }
                    out.push(r);
                }
            }
            out
        }
    };
    let circuits = q.rows("Circuits: carrier, cost, contract", "SELECT id, provider, link, a_site, z_site, capacity_mbps, monthly_cost, sla, status, contract_months_left FROM netops_circuits LIMIT 500", json!({})).await?;
    let drift = q.rows("The inventory against what the logs show", "SELECT state, COUNT(*) AS n FROM netops_seen_links GROUP BY state LIMIT 10", json!({})).await?;
    let expiring: Vec<&Row> = circuits.iter().filter(|c| c.s("status") == "active" && c.i("contract_months_left") <= 6).collect();
    let ceased: Vec<&Row> = circuits.iter().filter(|c| c.s("status") != "active").collect();

    // ---- the carriers
    let down = q.rows("Minutes each link was down, over the telemetry kept", "SELECT link, COUNT(*) AS minutes FROM netops_link_metrics\nWHERE up = 0\nGROUP BY link LIMIT 500", json!({})).await?;
    let span = q.rows("How much link telemetry is kept", "SELECT MIN(ts) AS first, MAX(ts) AS last FROM netops_link_metrics", json!({})).await?;
    let kept_min = span.first().map_or(0.0, |r| ((parse_iso(&r.s("last")) - parse_iso(&r.s("first"))) as f64 / 60.0).max(1.0));
    let notices = q.rows("Carriers' maintenance ahead", "SELECT provider, COUNT(*) AS n FROM netops_maintenance\nWHERE window_end >= :now AND status != 'completed'\nGROUP BY provider LIMIT 50", json!({"now": iso(m)})).await?;
    let mut carriers: BTreeMap<String, (usize, f64, f64, f64, usize, Vec<String>)> = BTreeMap::new();
    for c in circuits.iter().filter(|c| c.s("status") == "active") {
        let x = carriers.entry(c.s("provider")).or_default();
        x.0 += 1;
        x.1 += c.f("monthly_cost");
        let mins = down.iter().find(|d| d.s("link") == c.s("link")).map_or(0.0, |d| d.f("minutes"));
        x.2 += mins;
        x.3 = x.3.max(c.s("sla").parse::<f64>().unwrap_or(99.9));
        let mine: Vec<String> = incidents.iter().filter(|i| i.s("root_entity") == c.s("link")).map(|i| i.s("id")).collect();
        x.4 += mine.len();
        x.5.extend(mine);
    }
    let carriers: Vec<Value> = carriers
        .into_iter()
        .map(|(name, (n, cost, mins, sla, inc, ids))| {
            let avail = 100.0 * (1.0 - mins / (kept_min * n as f64));
            json!({"provider": name, "circuits": n, "monthly_cost": round(cost, 0), "down_min": mins, "availability": round(avail, 4), "sla": sla, "met": avail >= sla, "incidents": inc, "incident_ids": ids,
                "notices": notices.iter().find(|x| x.s("provider") == name).map_or(0, |x| x.i("n"))})
        })
        .collect();

    // ---- change
    let outcomes = q.rows("How changes went in the period, by kind", "SELECT kind, status, COUNT(*) AS n FROM netops_changes\nWHERE status IN ('completed', 'rolled_back') AND executed >= :since\nGROUP BY kind, status LIMIT 100", json!({"since": iso(since)})).await?;
    let ahead = q.rows("Changes planned, by risk", "SELECT risk, COUNT(*) AS n FROM netops_changes\nWHERE status IN ('approved', 'scheduled') AND window_start >= :now\nGROUP BY risk LIMIT 10", json!({"now": iso(m)})).await?;
    let mut kinds: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for o in &outcomes {
        let x = kinds.entry(o.s("kind")).or_default();
        if o.s("status") == "completed" { x.0 += o.i("n") } else { x.1 += o.i("n") }
    }
    let (done, back): (i64, i64) = kinds.values().fold((0, 0), |a, x| (a.0 + x.0, a.1 + x.1));
    let change_kinds: Vec<Value> = kinds.iter().map(|(k, (ok, bad))| json!({"kind": k, "completed": ok, "rolled_back": bad, "success": if ok + bad > 0 { round(*ok as f64 / (ok + bad) as f64, 3) } else { 1.0 }})).collect();

    // ---- the team, and what the agents took off it
    let alerts_day = q.rows("Alerts raised a day", "SELECT date_trunc('day', started) AS t, COUNT(*) AS n FROM netops_alerts\nWHERE started >= :since\nGROUP BY date_trunc('day', started) ORDER BY t LIMIT 100", json!({"since": iso(since)})).await?;
    let alerts_kind = q.rows("Alerts by whether they joined an incident", "SELECT incident = '' AS loose, COUNT(*) AS n FROM netops_alerts\nWHERE started >= :since\nGROUP BY incident = '' LIMIT 5", json!({"since": iso(since)})).await.unwrap_or_default();
    let (mut loose, mut grouped) = (0, 0);
    if alerts_kind.is_empty() {
        for r in q.rows("Alerts by incident", "SELECT incident, COUNT(*) AS n FROM netops_alerts\nWHERE started >= :since\nGROUP BY incident LIMIT 2000", json!({"since": iso(since)})).await? {
            if r.s("incident").is_empty() { loose += r.i("n") } else { grouped += r.i("n") }
        }
    } else {
        for r in &alerts_kind {
            if r.get("loose") == Some(&Value::Bool(true)) || r.i("loose") == 1 { loose += r.i("n") } else { grouped += r.i("n") }
        }
    }
    let live_incidents = incidents.iter().filter(|i| i.s("origin") == "live").count();
    let agents = q.rows("What the agents did", "SELECT agent, COUNT(*) AS runs, AVG(ms) AS ms, SUM(queries) AS queries, SUM(rows_read) AS rows_read FROM netops_agent_runs\nGROUP BY agent LIMIT 20", json!({})).await?;
    let verdicts = q.rows("Root-cause answers against the simulator's key", "SELECT verdict, COUNT(*) AS n FROM netops_agent_runs\nWHERE agent = 'root-cause'\nGROUP BY verdict LIMIT 10", json!({})).await?;
    let actions = q.rows("Actions the agents proposed, by what became of them", "SELECT status, COUNT(*) AS n FROM netops_actions GROUP BY status LIMIT 10", json!({})).await?;
    let (right, judged) = verdicts.iter().fold((0, 0), |a, v| if v.s("verdict").is_empty() { a } else { (a.0 + if v.s("verdict") == "correct" { v.i("n") } else { 0 }, a.1 + v.i("n")) });

    // ---- the week, in words
    let wk: Vec<&Row> = incidents.iter().filter(|i| parse_iso(&i.s("started")) >= week).collect();
    let worst = wk.iter().max_by(|a, b| (a.f("customers") * a.f("mttr_min")).partial_cmp(&(b.f("customers") * b.f("mttr_min"))).unwrap_or(std::cmp::Ordering::Equal));
    // The two windows are said apart: a rate is measured over the telemetry
    // kept (hours), incident minutes over the period (days).
    let below: Vec<&str> = services.iter().filter(|s| s["met"] == false).map(|s| s["name"].as_str().unwrap_or("")).collect();
    let over: Vec<&str> = services.iter().filter(|s| s["period_used"].as_f64().unwrap_or(0.0) > 1.0).map(|s| s["name"].as_str().unwrap_or("")).collect();
    let cannot: Vec<&Value> = failover.iter().filter(|f| f["survives"] == false).collect();
    let exposed_devices: i64 = advisories.iter().map(|a| a.i("devices")).sum();
    let mut report: Vec<String> = Vec::new();
    report.push(format!("Network operations, week to {}", &iso(m)[..10]));
    report.push(String::new());
    report.push(format!("Incidents. {} this week ({} sev1), against {:.1} a week over the last {days} days. Mean time to detect {:.0} s, to resolve {:.0} min.{}",
        wk.len(), wk.iter().filter(|i| i.s("severity") == "sev1").count(), n as f64 / (days as f64 / 7.0), mean(&|r| r.f("ttd_s"), &wk), mean(&|r| r.f("mttr_min"), &wk.iter().filter(|i| i.s("status") == "resolved").cloned().collect::<Vec<_>>()),
        worst.map(|i| format!(" The largest was {} ({}): {} customers for {:.0} minutes.", i.s("id"), i.s("title"), i.i("customers"), i.f("mttr_min"))).unwrap_or_default()));
    report.push(objectives_line(services.len(), &below, &over, round(kept_min / 60.0, 1), days));
    report.push(format!("Change. {} carried out in the period, {} rolled back ({:.0}% as planned); {} incident{} began with a change.", done + back, back, if done + back > 0 { 100.0 * done as f64 / (done + back) as f64 } else { 100.0 }, by_change.len(), if by_change.len() == 1 { "" } else { "s" }));
    report.push(format!("Risk. {} of {} backbone spans would not fit on one plane at their measured peak{}. {} on software with an open vendor advisory. {} within six months.",
        cannot.len(), failover.len(), cannot.first().map(|f| format!(" (the tightest is {} at {:.0}%)", f["span"].as_str().unwrap_or(""), f["load"].as_f64().unwrap_or(0.0) * 100.0)).unwrap_or_default(), plural(exposed_devices as usize, "device"), format!("{} {}", plural(expiring.len(), "carrier contract"), if expiring.len() == 1 { "ends" } else { "end" })));
    report.push(format!("Repeat causes. {}", if repeats.is_empty() { "None: no device or circuit was the cause of two incidents.".to_string() } else { repeats.iter().take(3).map(|r| format!("{} ({} incidents, {})", r["entity"].as_str().unwrap_or(""), r["incidents"].as_array().map_or(0, Vec::len), r["category"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("; ") + "." }));
    report.push(format!("Team. {} alerts in the period, {} of them grouped into {} by the topology; {} of {} incidents began outside 07:00 to 19:00 UTC. The root-cause agent named the cause the simulator's key confirms in {right} of {judged} checked.",
        loose + grouped, grouped, plural(live_incidents, "live incident"), out_of_hours, n));

    q.done(json!({"at": iso(m), "days": days, "window_hours": window_h, "kept_hours": round(kept_min / 60.0, 1),
        "services": services,
        "incidents": {"count": n, "sev1": all.iter().filter(|i| i.s("severity") == "sev1").count(), "ttd": round(mean(&|r| r.f("ttd_s"), &all), 0), "mttr": round(mean(&|r| r.f("mttr_min"), &resolved), 1),
            "customer_minutes": round(customer_minutes, 0), "revenue_exposed": round(exposed, 0), "weeks": weeks, "causes": causes, "repeats": repeats, "by_change": by_change, "out_of_hours": out_of_hours, "pages": pages},
        "risk": {"failover": failover, "advisories": advisories, "expiring": expiring, "ceased": ceased, "drift": drift},
        "carriers": carriers,
        "change": {"completed": done, "rolled_back": back, "kinds": change_kinds, "ahead": ahead},
        "team": {"alerts_day": alerts_day, "loose": loose, "grouped": grouped, "agents": agents, "right": right, "judged": judged, "actions": actions},
        "report": report.join("\n")}))
}

#[cfg(test)]
mod tests {
    use super::objectives_line;

    #[test]
    fn the_objectives_say_which_window() {
        let l = objectives_line(12, &[], &["MPLS L3VPN"], 6.0, 30);
        assert_eq!(l, "Objectives. Measured over the last 6 h of telemetry, all 12 services are inside their objective. Over the 30 days of incidents, 1 service has had more incident minutes than its objective allows (MPLS L3VPN).");
        let l = objectives_line(12, &["Recursive DNS", "Wavelength Transport"], &[], 5.5, 7);
        assert!(l.starts_with("Objectives. Measured over the last 5.5 h of telemetry, 2 of 12 services are below their objective (Recursive DNS, Wavelength Transport)."), "{l}");
        assert!(l.ends_with("Over the 7 days of incidents, no service has had more incident minutes than its objective allows."), "{l}");
    }
}
