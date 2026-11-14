//! The agents. Each is a playbook whose every step is a query over the
//! tables, kept with its SQL, its rows and its time, so that a person can
//! read what the agent read. No language model is called: what an agent
//! concludes follows from what its queries returned, and the brief it packs
//! (with a receipt of what fitted the token budget and what did not) is what
//! a model would be given to write the prose.
//!
//! None of them reads `netops_faults`, the simulator's answer key; the
//! engine marks the root-cause agent's answer against it afterwards.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use serde_json::{json, Value};

use crate::seed;
use crate::sim::{now, Engine};
use crate::strata::{Result, Row, RowExt};
use crate::telemetry::{iso, parse_iso, round};

/// (id, name, what it does, when it runs).
pub const AGENTS: &[(&str, &str, &str, &str)] = &[
    ("triage", "Triage", "Sizes a new incident: the alerts, the devices around them, the services and customers that ride on those, and who is on call", "When an incident opens"),
    ("root-cause", "Root cause", "Ranks the alerting devices over the topology, reads the logs, changes, optics and flows around them, tests six causes, finds similar incidents and the runbook that closed them, and proposes actions", "When an incident opens, and again when more alerts arrive"),
    ("change-risk", "Change risk", "Scores a planned change: its blast radius, other changes on the same or neighbouring devices in its window, open incidents nearby, and whether the other plane can carry the load", "On demand, from a change"),
    ("capacity", "Capacity", "Finds the links whose 95th percentile runs hot, and the pairs that cannot carry each other's traffic if one fails", "On demand"),
    ("compliance", "Advisories and drift", "Joins vendor advisories with the inventory's software versions, and searches every configuration for what should not be there", "On demand"),
    ("estate", "Estate outage", "Reads what a broken object of the estate reports, walks what depends on it and which customers are behind it, and looks for a change on it or on what it hangs from", "When an incident in the estate opens"),
    ("topology", "Topology from logs", "Reads LLDP and routing-adjacency lines out of syslog, builds the links they imply, and compares them with the inventory", "Every five minutes"),
];

fn clean(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || "_.:/@-".contains(*c)).collect()
}

/// `'a', 'b'` for an `IN (…)` or a seed tuple.
pub fn quoted(ids: &[String]) -> String {
    ids.iter().map(|d| format!("'{}'", clean(d))).collect::<Vec<_>>().join(", ")
}

fn hhmm(t: i64) -> String {
    iso(t)[11..16].to_string()
}

pub struct Run<'a> {
    e: &'a Engine,
    pub id: String,
    agent: &'static str,
    incident: String,
    subject: String,
    t0: Instant,
    at: i64,
    steps: Vec<Value>,
    queries: i64,
    rows: i64,
    pub tokens: i64,
    graph: &'static str,
}

impl<'a> Run<'a> {
    pub fn new(e: &'a Engine, agent: &'static str, incident: &str, subject: &str, at: i64) -> Run<'a> {
        Run { e, id: format!("run-{}-{}-{}", agent, at / 60, e.next_id()), agent, incident: incident.into(), subject: subject.into(), t0: Instant::now(), at, steps: Vec::new(), queries: 0, rows: 0, tokens: 0, graph: "" }
    }

    fn step(&mut self, tool: &str, title: &str, sql: &str, params: &Value, rows: usize, ms: f64, detail: &str) {
        let n = self.steps.len() + 1;
        let mut text = sql.to_string();
        if params.as_object().is_some_and(|o| !o.is_empty()) {
            text.push_str(&format!("\n-- {}", params));
        }
        self.steps.push(json!({"id": format!("{}:{n:02}", self.id), "run": self.id, "n": n, "ts": iso(now()), "tool": tool, "title": title, "detail": detail, "sql_text": text, "row_count": rows, "ms": round(ms, 2)}));
    }

    /// A query, kept as a step. One that fails is kept too, and returns no rows.
    pub async fn q(&mut self, title: &str, sql: &str, params: Value) -> Vec<Row> {
        let t0 = Instant::now();
        let r = self.e.db.query(sql, params.clone()).await;
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.queries += 1;
        match r {
            Ok(rows) => {
                self.rows += rows.len() as i64;
                self.step(if sql.contains("PACK ") { "pack" } else { "sql" }, title, sql, &params, rows.len(), ms, "");
                rows
            }
            Err(e) => {
                self.step("sql", title, sql, &params, 0, ms, &format!("Refused: {}", e.message));
                Vec::new()
            }
        }
    }

    /// What the last step showed.
    pub fn finding(&mut self, text: impl Into<String>) {
        if let Some(s) = self.steps.last_mut() {
            let text = text.into();
            let had = s["detail"].as_str().unwrap_or("").to_string();
            s["detail"] = json!(if had.is_empty() { text } else { format!("{had} {text}") });
        }
    }

    /// A step that is a conclusion, not a query.
    pub fn think(&mut self, title: &str, detail: impl Into<String>) {
        self.step("reason", title, "", &json!({}), 0, 0.0, &detail.into());
    }

    pub(crate) async fn graph(&mut self, title: &str, sql: String, fallback: impl FnOnce() -> Vec<Row>) -> Vec<Row> {
        if self.e.st().graph_sql != Some(false) {
            let t0 = Instant::now();
            let r = self.e.db.query(&sql, json!({})).await;
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.queries += 1;
            match r {
                Ok(rows) => {
                    self.e.st().graph_sql = Some(true);
                    self.graph = "sql";
                    self.rows += rows.len() as i64;
                    self.step("graph", title, &sql, &json!({}), rows.len(), ms, "");
                    return rows;
                }
                Err(err) if err.status == 400 => self.e.st().graph_sql = Some(false),
                Err(_) => {}
            }
        }
        let t0 = Instant::now();
        let rows = fallback();
        self.graph = "app";
        self.rows += rows.len() as i64;
        let text = format!("{sql}\n-- This server is a coordinator, which does not walk graphs yet: the app walked its own copy of the graph.");
        self.step("graph", title, &text, &json!({}), rows.len(), t0.elapsed().as_secs_f64() * 1000.0, "");
        rows
    }

    /// The devices within `hops` of the seeds: `REACHABLE` over the links table.
    pub async fn reach(&mut self, title: &str, seeds: &[String], hops: usize) -> Vec<(String, i64)> {
        if seeds.is_empty() {
            return Vec::new();
        }
        let s = if seeds.len() == 1 { quoted(seeds) } else { format!("({})", quoted(seeds)) };
        let sql = format!("SELECT id, hops() AS h FROM netops_devices\nWHERE REACHABLE(netops_links, {s}, hops => {hops}, dir => 'both')\nORDER BY hops(), id LIMIT 500");
        let g = &self.e.graph;
        let rows = self.graph(title, sql, || g.reach(seeds, hops).into_iter().map(|(id, h)| json!({"id": id, "h": h}).as_object().cloned().unwrap_or_default()).collect()).await;
        rows.iter().map(|r| (r.s("id"), r.i("h"))).collect()
    }

    /// The devices ranked by personalized PageRank from the seeds: `ppr`.
    pub async fn rank(&mut self, title: &str, seeds: &[String], k: usize) -> Vec<(String, f64)> {
        if seeds.is_empty() {
            return Vec::new();
        }
        let s = if seeds.len() == 1 { quoted(seeds) } else { format!("({})", quoted(seeds)) };
        let sql = format!("SELECT id, ppr(netops_links, {s}) AS score FROM netops_devices\nORDER BY ppr(netops_links, {s}) DESC LIMIT {k}");
        let g = &self.e.graph;
        let rows = self.graph(title, sql, || g.ppr(seeds).into_iter().take(k).map(|(id, v)| json!({"id": id, "score": v}).as_object().cloned().unwrap_or_default()).collect()).await;
        rows.iter().map(|r| (r.s("id"), r.f("score"))).collect()
    }

    pub async fn finish(self, status: &str, summary: &str, confidence: f64) -> Result<Value> {
        let run = json!({"id": self.id, "agent": self.agent, "incident": self.incident, "subject": self.subject, "started": iso(self.at), "finished": iso(now()), "status": status,
            "summary": summary, "confidence": round(confidence, 2), "steps": self.steps.len(), "queries": self.queries, "rows_read": self.rows, "tokens": self.tokens,
            "ms": round(self.t0.elapsed().as_secs_f64() * 1000.0, 1), "graph_engine": self.graph, "verdict": ""});
        self.e.db.upsert("netops_agent_steps", self.steps.clone()).await?;
        self.e.db.upsert("netops_agent_runs", vec![run.clone()]).await?;
        let mut out = run;
        out["step_list"] = json!(self.steps);
        Ok(out)
    }
}

// ---- triage

pub async fn triage(e: &Engine, incident: &str, m: i64) -> Result<()> {
    let Some(row) = e.st().open.get(incident).map(|o| o.row.clone()) else { return Ok(()) };
    let mut run = Run::new(e, "triage", incident, incident, m);
    let alerts = run.q("Read the incident's alerts", "SELECT rule, rule_name, entity, entity_kind, severity, reading, unit, started\nFROM netops_alerts WHERE incident = :id AND status = 'firing'\nORDER BY started LIMIT 200", json!({"id": incident})).await;
    let critical = alerts.iter().filter(|a| a.s("severity") == "critical").count();
    let degraded: Vec<String> = alerts.iter().filter(|a| a.s("entity_kind") == "service").map(|a| a.s("entity")).collect();
    run.finding(format!("{} alerts, {critical} of them critical{}.", alerts.len(), if degraded.is_empty() { String::new() } else { format!("; services already failing: {}", degraded.join(", ")) }));
    let devices = row.list("devices");
    let near = run.reach("Walk one hop out from the alerting devices", &devices, 1).await;
    run.finding(format!("{} devices alert; {} more are one hop away.", devices.len(), near.len()));
    let nums: Vec<String> = devices.iter().filter_map(|d| e.w.device(d).map(|x| x.num.to_string())).collect();
    let services = if nums.is_empty() {
        Vec::new()
    } else {
        run.q("Services that ride on these devices", &format!("SELECT id, name, tier, revenue_per_min, customers FROM netops_services\nWHERE device_nums && [{}]\nORDER BY tier LIMIT 20", nums.join(", ")), json!({})).await
    };
    let tier1 = services.iter().filter(|s| s.i("tier") == 1).count();
    run.finding(format!("{} services depend on them, {tier1} of tier 1.", services.len()));
    let sites = row.list("sites");
    let customers = if sites.is_empty() {
        Vec::new()
    } else {
        run.q("Customers attached at these sites", &format!("SELECT tier, COUNT(*) AS n, SUM(mrr) AS mrr FROM netops_customers\nWHERE site IN ({})\nGROUP BY tier", quoted(&sites)), json!({})).await
    };
    let n_customers: i64 = customers.iter().map(|c| c.i("n")).sum();
    let mrr: f64 = customers.iter().map(|c| c.f("mrr")).sum();
    run.finding(format!("{n_customers} customers attach at {}, worth {:.0} a month.", sites.join(", ").to_uppercase(), mrr));
    let team = if alerts.iter().any(|a| a.s("rule") == "udp-flood") {
        "Security"
    } else if alerts.iter().any(|a| a.s("rule").starts_with("bgp")) {
        "IP Edge"
    } else if alerts.iter().any(|a| a.s("rule").starts_with("optic") || a.s("rule") == "link-errors") {
        "Optical"
    } else {
        "Backbone NOC"
    };
    let oncall = run.q("Who is on call", "SELECT team, person, contact FROM netops_oncall\nWHERE role = 'primary' AND starts <= :now AND ends > :now LIMIT 20", json!({"now": iso(m)})).await;
    let person = oncall.iter().find(|o| o.s("team") == team).or(oncall.first()).map(|o| o.s("person")).unwrap_or_else(|| "the NOC".into());
    run.finding(format!("{person} is primary for {team}."));
    let severity = if !degraded.is_empty() || critical >= 2 { "sev1" } else if critical == 1 || alerts.len() >= 3 { "sev2" } else { "sev3" };
    let at_risk: f64 = services.iter().filter(|s| degraded.contains(&s.s("id"))).map(|s| s.f("revenue_per_min")).sum();
    let summary = format!("{severity}: {} alerts on {} devices at {}; {} services ride on them and {} customers attach there. Paged {person} ({team}).", alerts.len(), devices.len(), sites.join(", ").to_uppercase(), services.len(), n_customers);
    run.think("Size it", summary.clone());
    {
        let mut st = e.st();
        if let Some(o) = st.open.get_mut(incident) {
            let mut all: Vec<String> = degraded.clone();
            all.extend(services.iter().filter(|s| s.i("tier") <= 2).map(|s| s.s("id")));
            let mut seen = HashSet::new();
            all.retain(|s| seen.insert(s.clone()));
            all.truncate(8);
            o.row.insert("severity".into(), json!(severity));
            o.row.insert("commander".into(), json!(person));
            o.row.insert("customers".into(), json!(n_customers));
            o.row.insert("revenue_at_risk".into(), json!(at_risk));
            o.row.insert("services".into(), json!(all));
        }
    }
    e.save_incident(incident).await?;
    e.timeline(incident, m, "agent", "Triage agent", &summary, &run.id).await?;
    run.finish("done", &summary, 1.0).await?;
    Ok(())
}

// ---- root cause

struct Cause {
    category: &'static str,
    score: f64,
    kind: &'static str,
    entity: String,
    title: String,
    text: String,
    why: Vec<String>,
}

fn between<'x>(s: &'x str, a: &str, b: &str) -> Option<&'x str> {
    let i = s.find(a)? + a.len();
    let rest = &s[i..];
    Some(match rest.find(b) {
        Some(j) => &rest[..j],
        None => rest,
    })
}

/// Returns the run's id and the cause it named (kind, entity), if any.
pub async fn root_cause(e: &Engine, incident: &str, m: i64) -> Result<Option<(String, String, String)>> {
    let Some(row) = e.st().open.get(incident).map(|o| o.row.clone()) else { return Ok(None) };
    let w = &e.w;
    let semantic = e.st().semantic;
    let mut run = Run::new(e, "root-cause", incident, incident, m);
    let started = parse_iso(&row.s("started"));
    let since = started - 20 * 60;
    let devices = row.list("devices");
    let sites = row.list("sites");

    // 1. What fired.
    let alerts = run.q("Read the incident's alerts", "SELECT rule, rule_name, entity, entity_kind, severity, reading, unit, started\nFROM netops_alerts WHERE incident = :id\nORDER BY started LIMIT 300", json!({"id": incident})).await;
    let mut by_rule: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for a in &alerts {
        by_rule.entry(a.s("rule")).or_default().push(a.s("entity"));
    }
    run.finding(by_rule.iter().map(|(r, v)| format!("{r} ×{}", v.len())).collect::<Vec<_>>().join(", "));
    let of = |rule: &str| by_rule.get(rule).cloned().unwrap_or_default();

    // 2. Where the alerts centre.
    let rank = run.rank("Rank devices by personalized PageRank from the alerting ones", &devices, 8).await;
    run.finding(format!("The alerts centre on {}.", rank.iter().take(3).map(|r| format!("{} ({:.3})", r.0, r.1)).collect::<Vec<_>>().join(", ")));
    let mut around: Vec<String> = devices.clone();
    around.extend(e.graph.reach(&devices, 1).into_iter().map(|x| x.0));
    around.truncate(80);

    // 3. Which kinds of log line are new.
    let base = since - 6 * 3600;
    let templates = if around.is_empty() {
        Vec::new()
    } else {
        run.q("Count log lines by template around those devices", &format!("SELECT template, COUNT(*) AS n, MIN(ts) AS first FROM netops_syslog\nWHERE ts >= :since AND sev <= 4 AND device IN ({})\nGROUP BY template ORDER BY n DESC LIMIT 20", quoted(&around)), json!({"since": iso(since)})).await
    };
    run.finding(templates.iter().take(6).map(|t| format!("{} ×{}", t.s("template"), t.i("n"))).collect::<Vec<_>>().join(", "));
    let before = run.q("The same count for the six hours before, to see what is new", "SELECT template, COUNT(*) AS n FROM netops_syslog\nWHERE ts >= :base AND ts < :since AND sev <= 4\nGROUP BY template LIMIT 300", json!({"base": iso(base), "since": iso(since)})).await;
    let known: HashSet<String> = before.iter().map(|r| r.s("template")).collect();
    let fresh: Vec<(String, i64)> = templates.iter().filter(|t| !known.contains(&t.s("template"))).map(|t| (t.s("template"), t.i("n"))).collect();
    run.finding(if fresh.is_empty() { "Nothing new: every template was seen before.".to_string() } else { format!("New since {}: {}.", hhmm(since), fresh.iter().map(|(t, n)| format!("{t} ×{n}")).collect::<Vec<_>>().join(", ")) });

    // 4. Known signatures, by substring (the FM-index answers these).
    // Only lines from the devices around the alerts: a signature elsewhere is another incident's evidence.
    let near = if around.is_empty() { String::new() } else { format!(" AND device IN ({})", quoted(&around)) };
    let lines = run.q("Search the logs for known signatures (substring index)", &format!("SELECT ts, device, message FROM netops_syslog\nWHERE ts >= :since{near} AND (message LIKE '%loss of signal alarm set%' OR message LIKE '%is not defined%' OR message LIKE '%input failure%'\n   OR message LIKE '%scheduler slip%' OR message LIKE '%POLICER_VIOLATION%' OR message LIKE '%Rx power low warning%')\nORDER BY ts LIMIT 60"), json!({"since": iso(since)})).await;
    let has = |needle: &str| -> Vec<&Row> { lines.iter().filter(|l| l.s("message").contains(needle)).collect() };
    let (los, undefined, pem, slip, policer, rxlow) = (has("loss of signal alarm set"), has("is not defined"), has("input failure"), has("scheduler slip"), has("POLICER_VIOLATION"), has("Rx power low warning"));
    run.finding(format!("Loss of signal ×{}, undefined policy object ×{}, power input failure ×{}, scheduler slip ×{}, policer violation ×{}, low receive power ×{}.", los.len(), undefined.len(), pem.len(), slip.len(), policer.len(), rxlow.len()));

    // 5. Changes on these devices in the hour before.
    let nums: Vec<String> = around.iter().filter_map(|d| w.device(d).map(|x| x.num.to_string())).collect();
    let changes = if nums.is_empty() {
        Vec::new()
    } else {
        run.q("Changes executed on these devices in the hour before (INT[] overlap)", &format!("SELECT id, title, executed, owner, devices FROM netops_changes\nWHERE executed >= :since AND executed <= :now AND device_nums && [{}]\nORDER BY executed DESC LIMIT 10", nums.join(", ")), json!({"since": iso(started - 3600), "now": iso(m)})).await
    };
    run.finding(if changes.is_empty() { "None.".to_string() } else { changes.iter().map(|c| format!("{} at {} by {}", c.s("id"), hhmm(parse_iso(&c.s("executed"))), c.s("owner"))).collect::<Vec<_>>().join("; ") });

    // 6. Optics on the links that alert.
    let mut alert_links: Vec<String> = alerts.iter().filter(|a| a.s("entity_kind") == "link").map(|a| a.s("entity")).collect();
    alert_links.sort();
    alert_links.dedup();
    alert_links.truncate(40);
    let optics = if alert_links.is_empty() {
        Vec::new()
    } else {
        run.q("Receive power and errors on the alerting links over 35 minutes", &format!("SELECT link, MIN(rx_dbm) AS low, MAX(rx_dbm) AS high, SUM(errors) AS errors, MIN(up) AS up FROM netops_link_metrics\nWHERE ts >= :since AND link IN ({})\nGROUP BY link LIMIT 60", quoted(&alert_links)), json!({"since": iso(started - 35 * 60)})).await
    };
    let fading: Vec<&Row> = optics.iter().filter(|o| o.f("low") < -6.0 && o.f("low") > -30.0 && o.f("high") - o.f("low") > 2.5).collect();
    if !alert_links.is_empty() {
        run.finding(if fading.is_empty() { "No link's light is fading.".to_string() } else { fading.iter().map(|o| format!("{} fell from {:.1} to {:.1} dBm with {} errors", o.s("link"), o.f("high"), o.f("low"), o.i("errors"))).collect::<Vec<_>>().join("; ") });
    }

    // 7. Floods.
    let flows = run.q("UDP from amplification ports in the last five minutes, by target", "SELECT dst_net, dst_site, SUM(bytes) AS bytes, COUNT(DISTINCT src_asn) AS networks FROM netops_flows\nWHERE ts >= :since AND proto = 'udp' AND dst_site != 'scrubbing' AND dst_port IN (53, 123, 389, 1900, 11211)\nGROUP BY dst_net, dst_site ORDER BY bytes DESC LIMIT 5", json!({"since": iso(m - 299)})).await;
    let flood = flows.iter().map(|f| (f.s("dst_net"), f.s("dst_site"), f.f("bytes") * 8.0 / 300.0 / 1e9, f.i("networks")))
        // A flood that this incident's own alert names: an attack that ended minutes ago is still in the window.
        .find(|f| f.2 > 2.0 && sites.contains(&f.1) && of("udp-flood").contains(&f.1));
    run.finding(match &flood { Some(f) => format!("{:.1} Gb/s toward {} at {} from {} networks.", f.2, f.0, f.1.to_uppercase(), f.3), None => "No flood.".to_string() });

    // The six causes, each scored by what the queries above returned.
    let mut causes: Vec<Cause> = Vec::new();
    // A fibre cut: a link down with loss of signal at its ends.
    for l in of("link-down") {
        let Some(li) = w.link_by_id(&l) else { continue };
        let link = &w.links[li];
        let ends = [&w.devices[link.a].id, &w.devices[link.b].id];
        let seen = ends.iter().filter(|d| los.iter().any(|x| x.s("device") == ***d && x.s("message").contains(if **d == ends[0] { link.a_if.as_str() } else { link.z_if.as_str() }))).count();
        if seen == 0 {
            continue;
        }
        let (a, z) = (w.devices[link.a].site.to_uppercase(), w.devices[link.b].site.to_uppercase());
        let what = if link.circuit.is_empty() { format!("link {l}") } else { format!("circuit {} ({})", link.circuit, link.provider) };
        causes.push(Cause {
            category: "fiber_cut", score: if seen == 2 { 0.93 } else { 0.7 }, kind: "link", entity: l.clone(),
            title: if link.plane.is_empty() { format!("Link {l} down: loss of signal") } else { format!("Backbone circuit {a}-{z} plane {} down: loss of signal", link.plane) },
            text: format!("Loss of signal on {what}: both ends of {l} lost light at {}, which points at the span between {a} and {z} (a fibre cut or a failed amplifier), not at either router.", los.first().map(|x| hhmm(parse_iso(&x.s("ts")))).unwrap_or_default()),
            why: vec![format!("link-down alert on {l}"), format!("loss-of-signal lines from {seen} of its 2 ends"), "no change on either router in the hour before".into()],
        });
    }
    // Power: several devices at one site dark together.
    let mut dark: HashMap<String, Vec<String>> = HashMap::new();
    for d in of("device-unreachable") {
        if let Some(x) = w.device(&d) {
            dark.entry(x.site.to_string()).or_default().push(d);
        }
    }
    for (site, ds) in &dark {
        if ds.len() < 2 {
            continue;
        }
        let survivors = pem.iter().filter(|x| w.device(&x.s("device")).is_some_and(|d| d.site == site)).count();
        causes.push(Cause {
            category: "power", score: if survivors > 0 { 0.95 } else { 0.6 }, kind: "site", entity: site.clone(),
            title: format!("Power feed A lost at {}", site.to_uppercase()),
            text: format!("Loss of a power feed at {}: {} went dark in the same minute and the {survivors} devices that remain report a power supply input failure on feed A. A facilities fault, not a network one.", site.to_uppercase(), ds.join(", ")),
            why: vec![format!("{} devices at one site unreachable together", ds.len()), format!("{survivors} power input failure lines from the survivors")],
        });
    }
    // A change: BGP down on a device a change just touched.
    let mut bgp_devices = of("bgp-session-down");
    bgp_devices.extend(undefined.iter().map(|x| x.s("device")));
    bgp_devices.sort();
    bgp_devices.dedup();
    // On a device that is alerting now: an error line from an earlier change is not this incident's.
    bgp_devices.retain(|d| alerts.iter().any(|a| a.s("entity_kind") == "device" && a.s("entity") == *d));
    for d in bgp_devices {
        let Some(c) = changes.iter().find(|c| c.list("devices").contains(&d)) else { continue };
        let policy = undefined.iter().find(|x| x.s("device") == d);
        let sessions = of("bgp-session-down").contains(&d);
        let id = c.s("id");
        causes.push(Cause {
            category: "bad_change", score: match (policy.is_some(), sessions) { (true, true) => 0.95, (true, false) => 0.75, _ => 0.66 }, kind: "change", entity: id.clone(),
            title: format!("BGP sessions down on {d} after change {id}"),
            text: format!("Change {id} on {d}, committed at {} by {}: {}", hhmm(parse_iso(&c.s("executed"))), c.s("owner"), match policy {
                Some(p) => format!("the device logged \"{}\", so the policy drops every route it receives.", between(&p.s("message"), " : ", ";").unwrap_or("a policy error").trim()),
                None => "its BGP sessions dropped within minutes of the commit.".to_string(),
            }),
            why: vec![format!("BGP sessions not established on {d}"), format!("{id} executed on {d} {} minutes before the first alert", (started - parse_iso(&c.s("executed"))).max(0) / 60), if policy.is_some() { "a policy error in its log naming an undefined object".into() } else { "no policy error found in the log".into() }],
        });
    }
    // A flood.
    if let Some((net, site, gbps, networks)) = &flood {
        causes.push(Cause {
            category: "ddos", score: if policer.is_empty() { 0.82 } else { 0.92 }, kind: "site", entity: site.clone(),
            title: format!("UDP amplification attack on {net} via {}", site.to_uppercase()),
            text: format!("Volumetric DDoS: {gbps:.1} Gb/s of UDP from DNS, NTP, SSDP and memcached reflectors in {networks} networks toward customer prefix {net}, entering at the {} borders.", site.to_uppercase()),
            why: vec![format!("{gbps:.1} Gb/s from amplification ports toward one /24"), format!("{} policer violation lines on the borders", policer.len()), format!("firewall session alerts: {}", of("fw-sessions").len())],
        });
    }
    // A software defect: one router's CPU pegged, with scheduler slips.
    let cpu_alert = of("device-cpu");
    // The slips show in the log minutes before the CPU rule's window fills, so either is a lead.
    let mut suspects = of("device-cpu");
    // But only on a device that is itself alerting, and only fresh ones: a
    // neighbour's slips from an earlier incident are not this one's cause.
    for x in &slip {
        let d = x.s("device");
        let alerting = alerts.iter().any(|a| a.s("entity_kind") == "device" && a.s("entity") == d);
        if alerting && parse_iso(&x.s("ts")) >= started - 12 * 60 && !suspects.contains(&d) {
            suspects.push(d);
        }
    }
    for d in suspects {
        let slips = slip.iter().filter(|x| x.s("device") == d).count();
        let Some(dev) = w.device(&d) else { continue };
        let adv = run.q("Advisories that name this router's software version", "SELECT id, title, fixed_in, severity FROM netops_advisories\nWHERE os = :os AND affected = :v LIMIT 5", json!({"os": dev.os, "v": dev.os_version})).await;
        let hit = adv.iter().find(|a| a.s("title").contains("CPU")).or(adv.first());
        run.finding(match hit { Some(a) => format!("{} on {} {}: {} (fixed in {}).", a.s("id"), dev.os, dev.os_version, a.s("title"), a.s("fixed_in")), None => format!("None for {} {}.", dev.os, dev.os_version) });
        // A busy CPU alone is a symptom (a flood does it too); the slips are the defect's mark.
        if slips == 0 {
            continue;
        }
        causes.push(Cause {
            category: "cpu_bug", score: match (slips > 0, hit.is_some()) { (true, true) if cpu_alert.contains(&d) => 0.93, (true, true) => 0.8, (true, false) => 0.72, _ => 0.5 }, kind: "device", entity: d.clone(),
            title: format!("Routing process pegs CPU on {d}"),
            text: format!("Software defect on {d} ({} {}){}: the routing process is spinning, with {slips} scheduler slips logged, and its neighbours' BGP hold timers expire.", dev.os, dev.os_version, hit.map(|a| format!(", advisory {}", a.s("id"))).unwrap_or_default()),
            why: vec![if cpu_alert.contains(&d) { format!("control-plane CPU above 85% on {d} alone") } else { format!("the routing process on {d} reports it cannot keep its schedule") }, format!("{slips} scheduler-slip lines"), hit.map(|a| format!("advisory {} names its version; fixed in {}", a.s("id"), a.s("fixed_in"))).unwrap_or_else(|| "no advisory names its version".into())],
        });
    }
    // A failing optic: light fading over minutes, errors climbing.
    for o in &fading {
        let l = o.s("link");
        let Some(li) = w.link_by_id(&l) else { continue };
        let link = &w.links[li];
        // Loss of signal on this link's own ports is a cut, not a fading optic.
        let lost = |dev: usize, port: &str| los.iter().any(|x| x.s("device") == w.devices[dev].id && x.s("message").contains(port));
        if lost(link.a, &link.a_if) || lost(link.b, &link.z_if) {
            continue;
        }
        let warn = rxlow.iter().find(|x| x.s("device") == w.devices[link.b].id || x.s("device") == w.devices[link.a].id);
        let end = warn.map(|x| x.s("device")).unwrap_or_else(|| w.devices[link.b].id.clone());
        let port = if end == w.devices[link.a].id { &link.a_if } else { &link.z_if };
        causes.push(Cause {
            category: "optic_degrade", score: 0.62 + if o.i("errors") > 60 { 0.2 } else { 0.0 } + if warn.is_some() { 0.1 } else { 0.0 }, kind: "link", entity: l.clone(),
            title: format!("Degrading optic on {end} toward {}", if end == w.devices[link.a].id { &w.devices[link.b].id } else { &w.devices[link.a].id }),
            text: format!("Failing optic (or a contaminated connector) on {end} {port}: receive power on {l} fell from {:.1} to {:.1} dBm over the last half hour with {} input errors, and no loss-of-signal alarm, so the span itself is intact.", o.f("high"), o.f("low"), o.i("errors")),
            why: vec![format!("receive power down {:.1} dB in 35 minutes", o.f("high") - o.f("low")), format!("{} CRC and framing errors", o.i("errors")), "no loss of signal on either end".into()],
        });
    }
    causes.sort_by(|a, b| b.score.total_cmp(&a.score));

    // 8. Have we seen this before? Words and meaning, fused.
    let mut symptoms: Vec<String> = alerts.iter().map(|a| a.s("rule_name")).collect();
    symptoms.sort();
    symptoms.dedup();
    let mut text = symptoms.join(", ");
    for (t, _) in fresh.iter().take(5) {
        text.push_str(&format!(", {}", t.to_lowercase().replace('_', " ")));
    }
    // Search by the leading hypothesis when there is one: it says more than the symptoms do.
    match causes.iter().max_by(|a, b| a.score.total_cmp(&b.score)) {
        Some(c) => text = format!("{}. {} {}", c.title, c.text.chars().take(300).collect::<String>(), text),
        None => {
            if let Some(l) = lines.first() {
                text.push_str(&format!(". {}", l.s("message").chars().take(140).collect::<String>()));
            }
        }
    }
    let score = |field: &str| match (semantic, field) {
        (true, "summary") => "rrf(bm25(summary, :q), bm25(root_cause, :q), similarity(emb, :q))".to_string(),
        (true, _) => format!("rrf(bm25({field}, :q), similarity(emb, :q))"),
        (false, _) => format!("bm25({field}, :q)"),
    };
    let similar = run.q("Similar past incidents (words and meaning, fused)", &format!("SELECT id, title, category, runbook, mttr_min, score() AS score FROM netops_incidents\nWHERE status = 'resolved'\nORDER BY {s} DESC LIMIT 5", s = score("summary")), json!({"q": text})).await;
    run.finding(if similar.is_empty() { "None.".to_string() } else { similar.iter().take(3).map(|s| format!("{} {} ({})", s.s("id"), s.s("title"), s.s("category"))).collect::<Vec<_>>().join("; ") });
    // History is a prior: a cause that matches what similar incidents turned out to be gains a little.
    for c in causes.iter_mut() {
        let votes = similar.iter().take(3).filter(|s| s.s("category") == c.category).count();
        c.score = (c.score + 0.02 * votes as f64).min(0.98);
    }
    causes.sort_by(|a, b| b.score.total_cmp(&a.score));

    // 9. The runbook: by text, and by which one closed the similar incidents.
    let runbooks = run.q("Runbooks that match these symptoms", &format!("SELECT id, title, category, score() AS score FROM netops_runbooks\nORDER BY {s} DESC LIMIT 4", s = score("body")), json!({"q": text})).await;
    run.finding(runbooks.iter().take(3).map(|r| format!("{} {}", r.s("id"), r.s("title"))).collect::<Vec<_>>().join("; "));
    let ids: Vec<String> = similar.iter().map(|s| s.s("id")).collect();
    let votes = if ids.is_empty() {
        Vec::new()
    } else {
        run.q("Which runbooks closed those incidents (judgments)", &format!("SELECT runbook, COUNT(*) AS n FROM netops_judgments\nWHERE grade >= 2 AND incident IN ({})\nGROUP BY runbook ORDER BY n DESC LIMIT 5", quoted(&ids)), json!({})).await
    };
    let best = causes.first();
    let second = causes.get(1).map(|c| c.score).unwrap_or(0.0);
    let by_cause = best.and_then(|c| seed::RUNBOOKS.iter().find(|r| r.2 == c.category).map(|r| r.0.to_string()));
    let voted = votes.first().map(|v| v.s("runbook"));
    let by_text = runbooks.first().map(|r| r.s("id"));
    let runbook = by_cause.clone().or(voted.clone()).or(by_text.clone()).unwrap_or_default();
    run.finding(format!("By text: {}. Closed the similar incidents: {}. Chosen: {runbook}.", by_text.clone().unwrap_or_else(|| "none".into()), voted.clone().map(|v| format!("{v} ×{}", votes[0].i("n"))).unwrap_or_else(|| "none".into())));

    // 10. The brief a model would be given, packed to a budget with a receipt.
    let mut receipts: Vec<Value> = Vec::new();
    let mut used = 0;
    let packs: Vec<(&str, String, Value)> = vec![
        ("netops_syslog", format!("SELECT id, ts, device, message FROM netops_syslog\nWHERE ts >= :since AND sev <= 5{}\nORDER BY bm25(message, :q) DESC LIMIT 80\nPACK 900 TOKENS WITH RECEIPT", if around.is_empty() { String::new() } else { format!(" AND device IN ({})", quoted(&around)) }), json!({"since": iso(since), "q": text})),
        ("netops_runbooks", "SELECT id, title, body FROM netops_runbooks\nORDER BY bm25(body, :q) DESC LIMIT 5\nPACK 700 TOKENS WITH RECEIPT".to_string(), json!({"q": text})),
        ("netops_incidents", "SELECT id, title, root_cause FROM netops_incidents\nWHERE status = 'resolved'\nORDER BY bm25(summary, :q) DESC LIMIT 6\nPACK 400 TOKENS WITH RECEIPT".to_string(), json!({"q": text})),
    ];
    for (source, sql, params) in packs {
        let rows = run.q(&format!("Pack {} into the brief", source.trim_start_matches("netops_")), &sql, params).await;
        let taken = rows.iter().filter(|r| r.s("pack_decision") == "take").count();
        let tokens = rows.iter().map(|r| r.i("pack_used")).max().unwrap_or(0);
        used += tokens;
        run.finding(format!("{taken} of {} rows fitted, {tokens} tokens.", rows.len()));
        for r in rows {
            let n = receipts.len();
            let title = if source == "netops_syslog" { format!("{} {}", hhmm(parse_iso(&r.s("ts"))), r.s("device")) } else { r.s("title") };
            let excerpt: String = [r.s("message"), r.s("body"), r.s("root_cause")].concat().chars().take(240).collect();
            receipts.push(json!({"id": format!("{}:{n:03}", run.id), "run": run.id, "incident": incident, "n": n, "source": source, "row_id": r.s("id"), "title": title, "score": round(r.f("pack_score"), 4),
                "tokens": r.i("pack_tokens"), "used": r.i("pack_used"), "decision": r.s("pack_decision"), "excerpt": excerpt}));
        }
    }
    run.tokens = used;
    e.db.upsert("netops_receipts", receipts).await?;

    // Conclude.
    let out = match best {
        Some(c) if c.score >= 0.5 => {
            let confidence = (c.score - 0.3 * second).clamp(0.2, 0.97);
            run.think("Conclude", format!("{} Because: {}.{}", c.text, c.why.join("; "), if second > 0.0 { format!(" Next best explanation scored {:.2}.", second) } else { String::new() }));
            let degraded: Vec<String> = of("service-errors").into_iter().chain(of("service-latency")).collect::<HashSet<_>>().into_iter().collect();
            let summary = format!("{} alerts on {} devices at {}.{} {}", alerts.len(), devices.len(), sites.join(", ").to_uppercase(),
                if degraded.is_empty() { String::new() } else { format!(" Failing: {}.", degraded.join(", ")) }, c.text);
            let change = if c.kind == "change" { c.entity.clone() } else { String::new() };
            {
                let mut st = e.st();
                if let Some(o) = st.open.get_mut(incident) {
                    for (k, v) in [("title", json!(c.title)), ("summary", json!(summary)), ("root_kind", json!(c.kind)), ("root_entity", json!(c.entity)), ("root_cause", json!(c.text)), ("confidence", json!(round(confidence, 2))),
                        ("category", json!(c.category)), ("runbook", json!(runbook)), ("change_id", json!(change))] {
                        o.row.insert(k.into(), v);
                    }
                    if o.row.s("status") == "investigating" {
                        o.row.insert("status".into(), json!("identified"));
                    }
                }
            }
            e.timeline(incident, m, "agent", "Root-cause agent", &format!("Likely cause ({:.0}% confident): {}", confidence * 100.0, c.text), &run.id).await?;
            propose(e, &run.id, incident, c, &row, &runbook, m).await?;
            let summary = format!("{} ({:.0}%)", c.title, confidence * 100.0);
            let done = run.finish("done", &summary, confidence).await?;
            Some((done["id"].as_str().unwrap_or("").to_string(), c.kind.to_string(), c.entity.clone()))
        }
        _ => {
            run.think("Conclude", "None of the six causes this playbook tests is supported by the evidence yet. It will look again when more alerts arrive.");
            e.timeline(incident, m, "agent", "Root-cause agent", "No single cause found yet; will look again as evidence arrives", &run.id).await?;
            let done = run.finish("done", "No single cause found yet", 0.0).await?;
            Some((done["id"].as_str().unwrap_or("").to_string(), String::new(), String::new()))
        }
    };
    Ok(out)
}

/// The actions that follow from a cause, for a person to approve.
async fn propose(e: &Engine, run: &str, incident: &str, c: &Cause, row: &Row, runbook: &str, m: i64) -> Result<()> {
    let w = &e.w;
    let have: HashSet<String> = e.db.query("SELECT id FROM netops_actions WHERE incident = :i LIMIT 100", json!({"i": incident})).await?.iter().map(|r| r.s("id")).collect();
    let mut out: Vec<Value> = Vec::new();
    let mut add = |kind: &str, target: &str, risk: &str, title: String, detail: String, command: String| {
        let id = format!("{incident}:{kind}:{}", clean(target));
        if !have.contains(&id) {
            out.push(json!({"id": id, "run": run, "incident": incident, "kind": kind, "title": title, "detail": detail, "command": command, "target": target, "risk": risk, "status": "proposed", "proposed": iso(now()), "decided_by": "", "result": ""}));
        }
    };
    let link = w.link_by_id(&c.entity).map(|i| &w.links[i]);
    match c.category {
        "fiber_cut" => {
            if let Some(l) = link {
                let sib = l.sibling.map(|s| w.links[s].id.clone()).unwrap_or_default();
                add("reroute", &l.id, "low", "Spread the span's traffic across a third path".to_string(),
                    format!("Plane {} is down and {sib} carries both planes' traffic. The TE template PLANE-FAILOVER moves best-effort traffic to a third path until the span is repaired.", l.plane),
                    format!("configure\nset groups TE-PLANE-FAILOVER protocols mpls label-switched-path {}-{}-BE secondary VIA-THIRD-PATH standby\nset apply-groups TE-PLANE-FAILOVER\ncommit confirmed 5 comment \"{incident}\"", w.devices[l.a].site.to_uppercase(), w.devices[l.b].site.to_uppercase()));
                if !l.circuit.is_empty() {
                    add("ticket", &l.circuit, "low", format!("Open a priority-1 ticket with {} for circuit {}", l.provider, l.circuit),
                        format!("Circuit {} between {} and {}: loss of signal on both ends since {}. Please dispatch and send an OTDR trace. Our reference: {incident}.", l.circuit, w.devices[l.a].site.to_uppercase(), w.devices[l.b].site.to_uppercase(), row.s("started")), String::new());
                }
            }
        }
        "optic_degrade" => {
            if let Some(l) = link {
                add("drain", &l.id, "low", format!("Drain {} before it flaps", l.id), "Raise the IGP metric on both ends so traffic leaves the link while it still passes; then the optic can be cleaned or swapped without loss.".to_string(),
                    format!("configure\nset protocols ospf area 0.0.0.0 interface {}.0 metric 65535\ncommit confirmed 5 comment \"{incident} drain\"", l.a_if));
                add("ticket", &format!("optic-{}", l.id), "low", format!("Dispatch field services to {} to clean the connector and swap the optic", w.devices[l.b].site.to_uppercase()),
                    format!("{} {}: receive power falling. Spare QSFP28-100G-LR4 is in the site's cabinet. Our reference: {incident}.", w.devices[l.b].id, l.z_if), String::new());
            }
        }
        "bad_change" => {
            let dev = row.list("devices").into_iter().find(|d| d.starts_with("br")).unwrap_or_default();
            add("rollback", &c.entity, "medium", format!("Roll back change {}", c.entity), format!("Return {dev} to the commit before {}. Do not fix forward during the incident.", c.entity),
                format!("configure exclusive\nrollback configuration last 1\ncommit comment \"{incident}: roll back {}\"\nshow bgp summary", c.entity));
        }
        "ddos" => {
            let net = between(&c.title, "attack on ", " via").unwrap_or("the prefix").to_string();
            add("flowspec", &c.entity, "medium", format!("Divert {net} to scrubbing with a flowspec rule"), "Redirect UDP from the amplification ports toward the attacked prefix into the scrubbing VRF. Clean traffic keeps flowing; nothing is blackholed.".to_string(),
                format!("flowspec\n address-family ipv4\n  service-policy type pbr SCRUB-{}\nclass-map type traffic match-all {incident}\n match destination-address ipv4 {net}\n match protocol udp\n match source-port 53 123 389 1900 11211\npolicy-map type pbr SCRUB\n class type traffic {incident}\n  redirect nexthop route-target 64512:666", c.entity.to_uppercase()));
        }
        "cpu_bug" => {
            add("restart_routing", &c.entity, "medium", format!("Restart the routing process on {} gracefully", c.entity), "Graceful restart keeps forwarding while the process comes back; neighbours hold their routes.".to_string(),
                format!("request system process restart routing gracefully   # on {}", c.entity));
            let v = w.device(&c.entity).map(|d| d.os_version.clone()).unwrap_or_default();
            let n = w.devices.iter().filter(|d| d.os_version == v && d.os == "Junos").count();
            add("ticket", &format!("upgrade-{v}"), "low", format!("Raise a change to upgrade the {n} routers still on Junos {v}"), format!("The defect behind {incident} is fixed in a later release. The inventory lists {n} routers on {v}."), String::new());
        }
        _ => {
            add("dispatch", &c.entity, "low", format!("Page the facilities desk at {}: power feed A is down", c.entity.to_uppercase()), "The network cannot fix this. Ask for the PDU breaker state; do not power-cycle remotely.".to_string(), String::new());
        }
    }
    if row.s("severity") == "sev1" {
        let services = row.list("services");
        add("comms", incident, "low", "Post a status update".to_string(),
            format!("Identified: {}\n\nWe are seeing degraded service at {}{}. Our engineers have identified the cause and are applying the fix{}. Next update in 30 minutes.", c.title, row.list("sites").join(", ").to_uppercase(),
                if services.is_empty() { String::new() } else { format!(" affecting {}", services.join(", ")) }, if runbook.is_empty() { String::new() } else { format!(" (runbook {runbook})") }), String::new());
    }
    if !out.is_empty() {
        let n = out.len();
        e.db.upsert("netops_actions", out).await?;
        e.timeline(incident, m, "agent", "Root-cause agent", &format!("Proposed {n} action{} for approval", if n == 1 { "" } else { "s" }), run).await?;
    }
    Ok(())
}

/// A postmortem draft from what the incident's row holds.
pub fn postmortem(row: &Row, m: i64) -> String {
    let started = parse_iso(&row.s("started"));
    let detected = parse_iso(&row.s("detected"));
    let mitigated = row.get("mitigated").and_then(|v| v.as_str()).map(parse_iso).unwrap_or(m);
    format!(
        "What happened: {}\n\nCause: {}\n\nDetection: the first alert fired at {} and alerts were grouped into one incident by topology at {}.\n\nResolution: {}service was restored {} minutes after the first alert, and every alert had cleared by {}.\n\nImpact: {} services and {} customers at {}.\n\nFollow-ups: review the runbook's steps against what was done; check that the alert rules caught this as early as they could.",
        if row.s("summary").is_empty() { row.s("title") } else { row.s("summary") },
        if row.s("root_cause").is_empty() { "not established".to_string() } else { row.s("root_cause") },
        hhmm(started), hhmm(detected),
        if row.s("runbook").is_empty() { String::new() } else { format!("followed runbook {}; ", row.s("runbook")) },
        (mitigated - started).max(0) / 60, hhmm(m), row.list("services").len(), row.i("customers"), row.list("sites").join(", ").to_uppercase())
}

// ---- topology from logs

/// Reads the links the logs imply and compares them with the inventory.
pub async fn discover(e: &Engine, m: i64, record: bool) -> Result<Value> {
    let w = &e.w;
    let mut run = Run::new(e, "topology", "", "syslog", m);
    let since = iso(m - 7 * 86400);
    // A link's ends announce their neighbour when it comes up, and now and
    // then after: the first kind over a week, the rest over a day.
    let mut lldp = run.q("LLDP: neighbours announced when links came up", "SELECT ts, device, template, message FROM netops_syslog\nWHERE template = 'LLDP_NEIGHBOR_UP' AND ts >= :since\nORDER BY ts DESC LIMIT 10000", json!({"since": since})).await;
    lldp.extend(run.q("LLDP: refreshes and neighbours aged out, last day", "SELECT ts, device, template, message FROM netops_syslog\nWHERE program = 'lldpd' AND template != 'LLDP_NEIGHBOR_UP' AND ts >= :since\nORDER BY ts DESC LIMIT 10000", json!({"since": iso(m - 86400)})).await);
    lldp.sort_by_key(|r| r.s("ts"));
    let ospf = run.q("Routing adjacencies that reached Full", "SELECT ts, device, message FROM netops_syslog\nWHERE template = 'OSPF_ADJ_FULL' AND ts >= :since\nORDER BY ts DESC LIMIT 10000", json!({"since": since})).await;
    #[derive(Default)]
    struct Seen {
        ifs: HashMap<String, String>,
        evidence: i64,
        protocols: HashSet<&'static str>,
        first: String,
        last: String,
        directions: HashSet<String>,
        aged: Option<String>,
    }
    let mut seen: BTreeMap<(String, String), Seen> = BTreeMap::new();
    let key = |a: &str, b: &str| if a <= b { (a.to_string(), b.to_string()) } else { (b.to_string(), a.to_string()) };
    for l in &lldp {
        let (dev, msg, ts) = (l.s("device"), l.s("message"), l.s("ts"));
        let Some(nbr) = between(&msg, " neighbor ", " ") else { continue };
        if w.device(nbr).is_none() {
            continue;
        }
        let s = seen.entry(key(&dev, nbr)).or_default();
        if l.s("template") == "LLDP_NEIGHBOR_DOWN" {
            s.aged = Some(ts);
            continue;
        }
        if let Some(port) = between(&msg, "interface ", " ") {
            s.ifs.insert(dev.clone(), port.to_string());
        }
        if let Some(port) = between(&msg, " port ", " ") {
            s.ifs.insert(nbr.to_string(), port.to_string());
        }
        s.evidence += 1;
        s.protocols.insert("lldp");
        s.directions.insert(dev.clone());
        if s.first.is_empty() || ts < s.first {
            s.first = ts.clone();
        }
        s.aged = None;
        s.last = ts;
    }
    let by_loopback: HashMap<&str, &str> = w.devices.iter().map(|d| (d.loopback.as_str(), d.id.as_str())).collect();
    for l in &ospf {
        let (dev, msg, ts) = (l.s("device"), l.s("message"), l.s("ts"));
        let Some(nbr) = between(&msg, "OSPF neighbor ", " ").and_then(|ip| by_loopback.get(ip)) else { continue };
        let s = seen.entry(key(&dev, nbr)).or_default();
        s.evidence += 1;
        s.protocols.insert("ospf");
        s.directions.insert(dev.clone());
        if s.first.is_empty() || ts < s.first {
            s.first = ts.clone();
        }
        if ts > s.last {
            s.last = ts;
        }
    }
    run.finding(format!("{} lines name {} distinct pairs of devices.", lldp.len() + ospf.len(), seen.len()));
    let inventory = run.q("The inventory's links", "SELECT id, src, dst, kind, circuit FROM netops_links LIMIT 10000", json!({})).await;
    let declared: HashMap<(String, String), &Row> = inventory.iter().map(|r| (key(&r.s("src"), &r.s("dst")), r)).collect();
    let mut rows: Vec<Value> = Vec::new();
    let (mut confirmed, mut undocumented, mut silent) = (0, Vec::new(), Vec::new());
    for (pair, s) in &seen {
        if s.evidence == 0 {
            continue;
        }
        let inv = declared.get(pair);
        let (src, dst) = match inv {
            Some(r) => (r.s("src"), r.s("dst")),
            None => pair.clone(),
        };
        let confidence = (0.5 + if s.directions.len() >= 2 { 0.2 } else { 0.0 } + if s.protocols.len() >= 2 { 0.1 } else { 0.0 } + 0.04 * s.evidence.min(5) as f64).min(0.99);
        let state = if inv.is_some() { "confirmed" } else { "undocumented" };
        if inv.is_some() {
            confirmed += 1;
        } else {
            undocumented.push(json!({"src": src, "dst": dst, "a_if": s.ifs.get(&src), "z_if": s.ifs.get(&dst), "evidence": s.evidence, "first_seen": s.first, "last_seen": s.last, "confidence": round(confidence, 2)}));
        }
        let mut protocols: Vec<&str> = s.protocols.iter().copied().collect();
        protocols.sort();
        rows.push(json!({"id": format!("{src}--{dst}"), "src": src, "dst": dst, "a_if": s.ifs.get(&src).cloned().unwrap_or_default(), "z_if": s.ifs.get(&dst).cloned().unwrap_or_default(), "evidence": s.evidence,
            "protocols": protocols, "first_seen": s.first, "last_seen": s.last, "confidence": round(confidence, 2), "state": state}));
    }
    for (pair, r) in &declared {
        if seen.get(pair).is_none_or(|s| s.evidence == 0) {
            silent.push(json!({"id": r.s("id"), "src": r.s("src"), "dst": r.s("dst"), "kind": r.s("kind"), "circuit": r.s("circuit")}));
        }
    }
    silent.sort_by_key(|v| v["id"].as_str().unwrap_or("").to_string());
    e.db.upsert("netops_seen_links", rows.clone()).await?;
    let summary = format!("{} links read from the logs: {confirmed} match the inventory, {} are not in it, and {} inventory links were never heard.", rows.len(), undocumented.len(), silent.len());
    run.think("Compare", summary.clone());
    let out = json!({"at": iso(m), "seen": rows.len(), "confirmed": confirmed, "undocumented": undocumented, "silent": silent, "lines": lldp.len() + ospf.len(), "summary": summary});
    e.db.upsert("netops_state", vec![json!({"id": "agent:topology", "val": out.to_string(), "updated": iso(m)})]).await?;
    if record {
        let mut done = run.finish("done", &summary, 1.0).await?;
        done["result"] = out;
        return Ok(done);
    }
    Ok(out)
}

// ---- change risk

pub async fn change_risk(e: &Engine, change: &str) -> Result<Value> {
    let w = &e.w;
    let m = e.st().last_minute;
    let mut run = Run::new(e, "change-risk", "", change, m);
    let rows = run.q("Read the change", "SELECT * FROM netops_changes WHERE id = :id", json!({"id": change})).await;
    let Some(c) = rows.into_iter().next() else { return Err(crate::strata::Error { status: 404, message: format!("no change {change}") }) };
    let devices = c.list("devices");
    run.finding(format!("{} on {} in the window {} to {}.", c.s("kind"), devices.join(", "), c.s("window_start"), c.s("window_end")));
    let near = run.reach("Devices one hop from the change", &devices, 1).await;
    let mut ball: Vec<String> = devices.clone();
    ball.extend(near.iter().map(|x| x.0.clone()));
    let num = |ids: &[String]| ids.iter().filter_map(|d| w.device(d).map(|x| x.num.to_string())).collect::<Vec<_>>().join(", ");
    run.finding(format!("{} neighbours.", near.len()));
    let services = run.q("Services that ride on the changed devices", &format!("SELECT id, name, tier, revenue_per_min FROM netops_services\nWHERE device_nums && [{}]\nORDER BY tier LIMIT 20", num(&devices)), json!({})).await;
    let tier1 = services.iter().filter(|s| s.i("tier") == 1).count();
    run.finding(format!("{} services, {tier1} of tier 1.", services.len()));
    let customers = run.q("Customers attached to the changed devices", &format!("SELECT COUNT(*) AS n, SUM(mrr) AS mrr FROM netops_customers\nWHERE device IN ({})", quoted(&devices)), json!({})).await;
    let n_customers = customers.first().map(|r| r.i("n")).unwrap_or(0);
    run.finding(format!("{n_customers} attach directly."));
    let clashes = run.q("Other changes on these devices or their neighbours in the same window", &format!("SELECT id, title, window_start, window_end, owner, devices FROM netops_changes\nWHERE device_nums && [{}] AND window_start < :end AND window_end > :start AND status IN ('approved', 'scheduled') AND id != :id\nLIMIT 20", num(&ball)),
        json!({"start": c.s("window_start"), "end": c.s("window_end"), "id": change})).await;
    let direct = clashes.iter().filter(|x| x.list("devices").iter().any(|d| devices.contains(d))).count();
    run.finding(if clashes.is_empty() { "None.".to_string() } else { format!("{}: {} on the same device, {} on a neighbour.", clashes.iter().map(|x| x.s("id")).collect::<Vec<_>>().join(", "), direct, clashes.len() - direct) });
    let open = run.q("Open incidents on these devices or their neighbours", &format!("SELECT id, title, status FROM netops_incidents\nWHERE status != 'resolved' AND device_nums && [{}] LIMIT 10", num(&ball)), json!({})).await;
    run.finding(if open.is_empty() { "None.".to_string() } else { open.iter().map(|x| x.s("id")).collect::<Vec<_>>().join(", ") });
    let past = run.q("How changes of this kind have gone", "SELECT status, COUNT(*) AS n FROM netops_changes\nWHERE kind = :kind AND status IN ('completed', 'rolled_back')\nGROUP BY status", json!({"kind": c.s("kind")})).await;
    let done: i64 = past.iter().map(|p| p.i("n")).sum();
    let failed: i64 = past.iter().filter(|p| p.s("status") == "rolled_back").map(|p| p.i("n")).sum();
    run.finding(format!("{failed} of {done} were rolled back."));
    // If the device is taken out, can the other plane carry its links' traffic?
    let mut pairs: Vec<(String, String)> = Vec::new();
    for d in &devices {
        if let Some(di) = w.devices.iter().position(|x| x.id == *d) {
            for li in w.links_of(di) {
                if let Some(s) = w.links[li].sibling {
                    pairs.push((w.links[li].id.clone(), w.links[s].id.clone()));
                }
            }
        }
    }
    let mut n1: Vec<Value> = Vec::new();
    if !pairs.is_empty() {
        let ids: Vec<String> = pairs.iter().flat_map(|p| [p.0.clone(), p.1.clone()]).collect();
        let peaks = run.q("Peak utilisation of the links it would take out, and of their other plane", &format!("SELECT link, MAX(util) AS peak FROM netops_link_metrics\nWHERE ts >= :since AND link IN ({})\nGROUP BY link LIMIT 100", quoted(&ids)), json!({"since": iso(m - 6 * 3600)})).await;
        let peak: HashMap<String, f64> = peaks.iter().map(|p| (p.s("link"), p.f("peak"))).collect();
        for (l, s) in &pairs {
            let both = peak.get(l).copied().unwrap_or(0.0) + peak.get(s).copied().unwrap_or(0.0);
            n1.push(json!({"link": l, "other": s, "combined": round(both, 3)}));
        }
        n1.sort_by(|a, b| b["combined"].as_f64().unwrap_or(0.0).total_cmp(&a["combined"].as_f64().unwrap_or(0.0)));
        run.finding(match n1.first() { Some(x) => format!("At the last six hours' peaks, {} would carry {:.0}% with {} out.", x["other"].as_str().unwrap_or(""), x["combined"].as_f64().unwrap_or(0.0) * 100.0, x["link"].as_str().unwrap_or("")), None => String::new() });
    }
    let hot = n1.first().and_then(|x| x["combined"].as_f64()).unwrap_or(0.0);
    let mut reasons: Vec<(i64, String)> = Vec::new();
    reasons.push((match c.s("risk").as_str() { "high" => 45, "medium" => 28, _ => 12 }, format!("Declared risk: {}", c.s("risk"))));
    if direct > 0 {
        reasons.push((25, format!("{direct} other change{} on the same device in this window", if direct == 1 { "" } else { "s" })));
    }
    if clashes.len() > direct {
        reasons.push((14, format!("{} change{} on a neighbouring device in this window: both sides of a path may be down at once", clashes.len() - direct, if clashes.len() - direct == 1 { "" } else { "s" })));
    }
    if !open.is_empty() {
        reasons.push((30, format!("An incident is open nearby ({})", open[0].s("id"))));
    }
    if done > 0 && failed > 0 {
        reasons.push(((40.0 * failed as f64 / done as f64).round() as i64, format!("{failed} of {done} past changes of this kind were rolled back")));
    }
    if hot > 0.9 {
        reasons.push((20, format!("The other plane would run at {:.0}% at peak with this device out", hot * 100.0)));
    }
    if tier1 >= 3 {
        reasons.push((6, format!("{tier1} tier-1 services ride on it")));
    }
    let score: i64 = reasons.iter().map(|r| r.0).sum::<i64>().min(100);
    let verdict = if score < 35 { "go" } else if score < 65 { "go with care" } else { "hold" };
    let summary = format!("{verdict} (risk {score}/100): {}", reasons.iter().map(|r| r.1.clone()).collect::<Vec<_>>().join("; "));
    run.think("Score it", summary.clone());
    let mut out = run.finish("done", &summary, 1.0).await?;
    out["result"] = json!({"score": score, "verdict": verdict, "reasons": reasons.iter().map(|r| json!({"points": r.0, "text": r.1})).collect::<Vec<_>>(), "services": services, "customers": n_customers,
        "near": near.iter().map(|x| x.0.clone()).collect::<Vec<_>>(), "collisions": clashes, "incidents": open, "n1": n1.into_iter().take(6).collect::<Vec<_>>()});
    Ok(out)
}

// ---- capacity

pub async fn capacity(e: &Engine) -> Result<Value> {
    let w = &e.w;
    let m = e.st().last_minute;
    let mut run = Run::new(e, "capacity", "", "links", m);
    let hours = e.opts.history_hours.max(1);
    let stats = run.q("95th percentile, peak and mean utilisation of every link", "SELECT link, PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY util) AS p95, MAX(util) AS peak, AVG(util) AS mean FROM netops_link_metrics\nWHERE ts >= :since AND up = 1\nGROUP BY link ORDER BY p95 DESC LIMIT 1000", json!({"since": iso(m - hours * 3600)})).await;
    let p95: HashMap<String, f64> = stats.iter().map(|s| (s.s("link"), s.f("p95"))).collect();
    run.finding(format!("{} links; the hottest is {} at {:.0}%.", stats.len(), stats.first().map(|s| s.s("link")).unwrap_or_default(), stats.first().map(|s| s.f("p95")).unwrap_or(0.0) * 100.0));
    let circuits = run.q("What each circuit costs", "SELECT id, link, provider, monthly_cost, contract_months_left FROM netops_circuits LIMIT 500", json!({})).await;
    let cost: HashMap<String, &Row> = circuits.iter().map(|c| (c.s("link"), c)).collect();
    let mut findings: Vec<Value> = Vec::new();
    for s in &stats {
        let id = s.s("link");
        let Some(li) = w.link_by_id(&id) else { continue };
        let l = &w.links[li];
        let other = l.sibling.map(|x| w.links[x].id.clone());
        let combined = other.as_ref().map(|o| s.f("p95") + p95.get(o).copied().unwrap_or(0.0));
        let hot = s.f("p95") > 0.8;
        let fragile = combined.is_some_and(|c| c > 0.95);
        if !hot && !fragile {
            continue;
        }
        let c = cost.get(&id);
        findings.push(json!({"link": id, "kind": l.kind, "capacity_gbps": l.capacity_mbps / 1000, "p95": round(s.f("p95"), 3), "peak": round(s.f("peak"), 3), "mean": round(s.f("mean"), 3),
            "other": other, "if_other_fails": combined.map(|c| round(c, 3)), "circuit": l.circuit, "provider": l.provider, "monthly_cost": c.map(|c| c.f("monthly_cost")), "months_left": c.map(|c| c.i("contract_months_left")),
            "finding": if fragile && !hot { "cannot carry its other plane at peak" } else if fragile { "hot, and cannot carry its other plane" } else { "hot at peak" }}));
    }
    findings.sort_by(|a, b| b["if_other_fails"].as_f64().unwrap_or(0.0).total_cmp(&a["if_other_fails"].as_f64().unwrap_or(0.0)));
    let fragile = findings.iter().filter(|f| f["if_other_fails"].as_f64().unwrap_or(0.0) > 0.95).count();
    let summary = format!("Over the last {hours} hours, {} links need attention; {fragile} of them could not carry their other plane's traffic at peak.", findings.len());
    run.think("Pair each link with its other plane", summary.clone());
    let mut out = run.finish("done", &summary, 1.0).await?;
    out["result"] = json!({"findings": findings, "hours": hours});
    save_result(e, "capacity", &out, m).await?;
    Ok(out)
}

// ---- advisories and drift

pub async fn compliance(e: &Engine) -> Result<Value> {
    let m = e.st().last_minute;
    let mut run = Run::new(e, "compliance", "", "fleet", m);
    let advisories = run.q("Vendor advisories", "SELECT id, vendor, os, affected, fixed_in, severity, cvss, title FROM netops_advisories ORDER BY cvss DESC LIMIT 100", json!({})).await;
    run.finding(format!("{} advisories.", advisories.len()));
    let mut exposed: Vec<Value> = Vec::new();
    for a in &advisories {
        let versions = a.list("affected");
        if versions.is_empty() {
            continue;
        }
        let devices = run.q(&format!("Devices on a version {} names", a.s("id")), &format!("SELECT id, site, role, os_version FROM netops_devices\nWHERE os = :os AND os_version IN ({})\nORDER BY id LIMIT 200", quoted(&versions)), json!({"os": a.s("os")})).await;
        run.finding(format!("{} devices.", devices.len()));
        if !devices.is_empty() {
            exposed.push(json!({"advisory": a.s("id"), "title": a.s("title"), "severity": a.s("severity"), "cvss": a.f("cvss"), "os": a.s("os"), "fixed_in": a.s("fixed_in"), "devices": devices.iter().map(|d| json!({"id": d.s("id"), "site": d.s("site"), "version": d.s("os_version")})).collect::<Vec<_>>()}));
        }
    }
    // Drift: configurations that still name what is gone, or what was never defined.
    let mut drift: Vec<Value> = Vec::new();
    let ceased = run.q("Circuits that were ceased", "SELECT id, link, provider FROM netops_circuits WHERE status = 'ceased' LIMIT 50", json!({})).await;
    let latest = |rows: &[Row]| -> Vec<String> {
        let mut best: BTreeMap<String, i64> = BTreeMap::new();
        for r in rows {
            let v = best.entry(r.s("device")).or_default();
            *v = (*v).max(r.i("version"));
        }
        best.into_keys().collect()
    };
    for c in &ceased {
        let hits = run.q(&format!("Configurations that still name circuit {}", c.s("id")), "SELECT device, version FROM netops_configs WHERE body LIKE :needle LIMIT 500", json!({"needle": format!("%{}%", c.s("id"))})).await;
        let devices = latest(&hits);
        run.finding(format!("{} devices.", devices.len()));
        if !devices.is_empty() {
            drift.push(json!({"check": "Configuration names a ceased circuit", "detail": format!("{} ({}) was ceased, but an interface description still names it", c.s("id"), c.s("provider")), "devices": devices}));
        }
    }
    let dangling = run.q("Policies that reference a prefix-set no configuration defines", "SELECT device, version, ts FROM netops_configs WHERE body LIKE '%in PS-BOGONS-V4%' LIMIT 500", json!({})).await;
    let defined = run.q("Configurations that define it", "SELECT device, version FROM netops_configs WHERE body LIKE '%prefix-set PS-BOGONS-V4%' LIMIT 500", json!({})).await;
    let have: HashSet<String> = defined.iter().map(|r| r.s("device")).collect();
    let current = run.q("The current version of each configuration", "SELECT device, MAX(version) AS version FROM netops_configs GROUP BY device LIMIT 1000", json!({})).await;
    let cur: HashMap<String, i64> = current.iter().map(|r| (r.s("device"), r.i("version"))).collect();
    let live: Vec<String> = dangling.iter().filter(|r| cur.get(&r.s("device")) == Some(&r.i("version")) && !have.contains(&r.s("device"))).map(|r| r.s("device")).collect();
    run.finding(format!("{} versions reference it; {} of them are a device's current configuration.", dangling.len(), live.len()));
    if !live.is_empty() {
        drift.push(json!({"check": "Route policy references an undefined prefix-set", "detail": "RP-TRANSIT-IN tests PS-BOGONS-V4, which is not defined: the policy drops every route", "devices": live}));
    }
    let n: usize = exposed.iter().map(|x| x["devices"].as_array().map(|a| a.len()).unwrap_or(0)).sum();
    let summary = format!("{} advisories apply to {n} device-versions in the inventory; {} configuration checks found drift.", exposed.len(), drift.len());
    run.think("Summarise", summary.clone());
    let mut out = run.finish("done", &summary, 1.0).await?;
    out["result"] = json!({"exposed": exposed, "drift": drift});
    save_result(e, "compliance", &out, m).await?;
    Ok(out)
}

async fn save_result(e: &Engine, agent: &str, out: &Value, m: i64) -> Result<()> {
    let mut slim = out.clone();
    if let Some(o) = slim.as_object_mut() {
        o.remove("step_list");
    }
    e.db.upsert("netops_state", vec![json!({"id": format!("agent:{agent}"), "val": slim.to_string(), "updated": iso(m)})]).await?;
    Ok(())
}
