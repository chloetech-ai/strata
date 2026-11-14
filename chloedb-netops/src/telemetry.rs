//! What the network reports each minute: a point per link, device and
//! service, flow summaries and log lines. All of it is computed from the
//! world, the minute and the faults active then, with a generator keyed by
//! the thing's name and the minute, so that a minute written twice (a
//! backfill after a restart) holds the same rows.
//!
//! Nothing here decides that something is wrong: alerts come from the
//! rules' SQL over these rows, incidents from correlating the alerts, and
//! causes from the agents. The faults only bend the numbers and add lines.

use std::collections::{HashMap, HashSet};

use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

use crate::world::{Rng, World};

pub fn iso(t: i64) -> String {
    Utc.timestamp_opt(t, 0).single().unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn parse_iso(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s).map(|d| d.timestamp()).unwrap_or(0)
}

/// A fault the simulator injected: its kind, its target and where it is in
/// its life. `truth_*` is the answer key, kept apart from what agents read.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Fault {
    pub id: String,
    pub scenario: String,
    pub started: i64,
    /// When someone (or the simulated engineer) took the traffic off it.
    pub mitigated: Option<i64>,
    /// When the underlying problem was repaired.
    pub ended: Option<i64>,
    pub link: Option<String>,
    pub device: Option<String>,
    pub site: Option<String>,
    pub change: Option<String>,
    pub customer: Option<String>,
    pub truth_kind: String,
    pub truth_entity: String,
    pub category: String,
    pub incident: Option<String>,
    /// `live` for one that plays out now, `seeded` for history.
    pub origin: String,
    /// When the simulated engineer mitigates if nobody does, and when the
    /// repair lands after that.
    pub auto_mitigate_after: i64,
    pub repair_after: i64,
}

impl Fault {
    pub fn active_at(&self, t: i64) -> bool {
        t >= self.started && self.ended.is_none_or(|e| t < e)
    }
    pub fn mitigated_at(&self, t: i64) -> bool {
        self.mitigated.is_some_and(|m| t >= m)
    }
    pub fn phase(&self, t: i64) -> &'static str {
        if self.ended.is_some_and(|e| t >= e) {
            "cleared"
        } else if self.mitigated_at(t) {
            "mitigated"
        } else {
            "active"
        }
    }
}

pub const SCENARIOS: &[(&str, &str, &str)] = &[
    ("fiber_cut", "Fibre cut on a backbone circuit", "A backbone circuit loses light; its traffic lands on the other plane, which runs hot"),
    ("bad_change", "A change breaks BGP on a border router", "A routing-policy change is committed on a border router and its sessions drop"),
    ("optic_degrade", "An optic slowly fails", "Receive power on a link decays, errors climb, and the link starts to flap"),
    ("ddos", "Volumetric DDoS on a customer prefix", "An amplification attack floods a site's borders and its firewalls"),
    ("cpu_bug", "Routing daemon pegs a core router's CPU", "A software defect named in a vendor advisory makes the routing process spin"),
    ("power", "A site loses one power feed", "Feed A at a small site fails; the devices on it go dark together"),
];

#[derive(Default, Clone)]
pub struct LinkFx {
    pub down: bool,
    pub extra_load: f64,
    pub scale: f64,
    pub loss: f64,
    pub errors: f64,
    pub rx: Option<f64>,
    pub latency: f64,
}

#[derive(Default, Clone)]
pub struct DevFx {
    pub down: bool,
    pub cpu: f64,
    pub mem: f64,
    pub bgp_mult: f64,
    pub sessions_mult: f64,
}

#[derive(Default, Clone)]
pub struct SvcFx {
    pub err: f64,
    pub lat_mult: f64,
}

#[derive(Default)]
pub struct Effects {
    pub link: HashMap<usize, LinkFx>,
    pub dev: HashMap<usize, DevFx>,
    pub svc: HashMap<usize, SvcFx>,
    /// Peers whose session is down.
    pub bgp_down: HashSet<usize>,
    /// (site, customer prefix, strength 0..1, scrubbed).
    pub ddos: Vec<(String, String, f64, bool)>,
}

impl Effects {
    fn l(&mut self, i: usize) -> &mut LinkFx {
        self.link.entry(i).or_insert_with(|| LinkFx { scale: 1.0, ..Default::default() })
    }
    fn d(&mut self, i: usize) -> &mut DevFx {
        self.dev.entry(i).or_insert_with(|| DevFx { bgp_mult: 1.0, sessions_mult: 1.0, ..Default::default() })
    }
    fn s(&mut self, i: usize) -> &mut SvcFx {
        let e = self.svc.entry(i).or_insert_with(|| SvcFx { lat_mult: 1.0, ..Default::default() });
        e
    }
    fn hurt(&mut self, w: &World, sites: &[&str], err: f64, lat: f64) {
        for (i, s) in w.services.iter().enumerate() {
            let share = sites.iter().filter(|x| s.sites.contains(x)).count() as f64 / s.sites.len().max(1) as f64;
            if share > 0.0 {
                let e = self.s(i);
                e.err += err * share;
                e.lat_mult *= 1.0 + (lat - 1.0) * share;
            }
        }
    }
    pub fn link_down(&self, i: usize) -> bool {
        self.link.get(&i).is_some_and(|l| l.down)
    }
    pub fn dev_down(&self, i: usize) -> bool {
        self.dev.get(&i).is_some_and(|d| d.down)
    }
}

/// The devices that sit on a site's power feed A.
pub fn feed_a(w: &World, site: &str) -> Vec<usize> {
    w.devices.iter().enumerate().filter(|(_, d)| d.site == site && (d.id.starts_with("cr1-") || d.id.starts_with("ag1-"))).map(|(i, _)| i).collect()
}

/// How the faults active at `t` bend the network.
pub fn effects(w: &World, faults: &[Fault], t: i64) -> Effects {
    let mut fx = Effects::default();
    for f in faults.iter().filter(|f| f.active_at(t)) {
        let age = (t - f.started) as f64 / 60.0;
        let mitigated = f.mitigated_at(t);
        let link = f.link.as_deref().and_then(|l| w.link_by_id(l));
        let dev = f.device.as_deref().and_then(|d| w.devices.iter().position(|x| x.id == d));
        match f.scenario.as_str() {
            "fiber_cut" => {
                let Some(li) = link else { continue };
                let l = &w.links[li];
                let e = fx.l(li);
                e.down = true;
                e.rx = Some(-40.0);
                if let Some(s) = l.sibling {
                    let e = fx.l(s);
                    e.extra_load += if mitigated { l.load * 0.38 } else { l.load * 0.98 };
                    e.loss += if mitigated { 0.0 } else { 0.9 };
                    e.latency += if mitigated { 0.6 } else { 3.5 };
                }
                let sites = [w.devices[l.a].site, w.devices[l.b].site];
                fx.hurt(w, &sites, if mitigated { 0.0008 } else { 0.021 }, if mitigated { 1.12 } else { 1.9 });
            }
            "optic_degrade" => {
                let Some(li) = link else { continue };
                let l = &w.links[li];
                let p = (age / 24.0).min(1.0);
                let e = fx.l(li);
                e.rx = Some(-2.8 - 15.5 * p.powf(1.4));
                if mitigated {
                    // Drained: almost no traffic, so the errors stop mattering.
                    e.scale = 0.03;
                    e.errors = 4.0;
                } else {
                    e.errors = 3.0 * (p * 7.5).exp();
                    e.loss = 3.2 * p * p;
                    e.latency = 1.5 * p;
                    // Flaps once the light is nearly gone.
                    e.down = p > 0.72 && Rng::keyed(&l.id, (t / 60) as u64).chance(0.33);
                }
                if let Some(s) = l.sibling {
                    fx.l(s).extra_load += if mitigated { l.load * 0.6 } else { l.load * 0.25 * p };
                }
                let sites = [w.devices[l.a].site, w.devices[l.b].site];
                fx.hurt(w, &sites, if mitigated { 0.0004 } else { 0.012 * p * p }, if mitigated { 1.05 } else { 1.0 + 0.5 * p });
            }
            "bad_change" => {
                let Some(di) = dev else { continue };
                if mitigated {
                    continue;
                }
                for (pi, p) in w.peers.iter().enumerate() {
                    if p.device == di {
                        fx.bgp_down.insert(pi);
                    }
                }
                let e = fx.d(di);
                e.bgp_mult = 45.0;
                e.cpu += 22.0;
                for li in w.links_of(di) {
                    fx.l(li).scale = 0.25;
                }
                // The other border of the site takes the traffic.
                let site = w.devices[di].site;
                if let Some(other) = w.devices.iter().position(|d| d.site == site && d.role == "border" && d.id != w.devices[di].id) {
                    for li in w.links_of(other) {
                        fx.l(li).extra_load += 0.33;
                    }
                    fx.d(other).bgp_mult = 9.0;
                }
                fx.hurt(w, &[site], 0.034, 1.45);
            }
            "ddos" => {
                let site = f.site.clone().unwrap_or_else(|| "ams".into());
                let ramp = (age / 4.0).min(1.0);
                fx.ddos.push((site.clone(), f.customer.clone().unwrap_or_else(|| "203.0.113.0/24".into()), ramp, mitigated));
                if mitigated {
                    fx.hurt(w, &[w.site(&site).id], 0.0006, 1.06);
                    continue;
                }
                for (di, d) in w.devices.iter().enumerate().filter(|(_, d)| d.site == site) {
                    match d.role {
                        "border" => {
                            fx.d(di).cpu += 38.0 * ramp;
                            for li in w.links_of(di) {
                                let e = fx.l(li);
                                e.extra_load += 0.5 * ramp;
                                e.loss += 1.4 * ramp;
                            }
                        }
                        "firewall" => {
                            let e = fx.d(di);
                            e.sessions_mult = 1.0 + 7.0 * ramp;
                            e.cpu += 41.0 * ramp;
                        }
                        "agg" => {
                            for li in w.links_of(di) {
                                fx.l(li).extra_load += 0.22 * ramp;
                            }
                        }
                        _ => {}
                    }
                }
                fx.hurt(w, &[w.site(&site).id], 0.027 * ramp, 1.0 + 0.8 * ramp);
            }
            "cpu_bug" => {
                let Some(di) = dev else { continue };
                if mitigated {
                    continue;
                }
                let p = (age / 6.0).min(1.0);
                let e = fx.d(di);
                e.cpu += 62.0 * p;
                e.mem += 21.0 * (age / 30.0).min(1.0);
                e.bgp_mult = 1.0 + 28.0 * p;
                for li in w.links_of(di) {
                    let e = fx.l(li);
                    e.loss += 0.55 * p;
                    e.latency += 2.4 * p;
                }
                fx.hurt(w, &[w.devices[di].site], 0.011 * p, 1.0 + 0.55 * p);
            }
            "power" => {
                let site = f.site.clone().unwrap_or_default();
                if mitigated {
                    continue;
                }
                for di in feed_a(w, &site) {
                    fx.d(di).down = true;
                    for li in w.links_of(di) {
                        let l = &w.links[li];
                        fx.l(li).down = true;
                        if let Some(s) = l.sibling {
                            fx.l(s).extra_load += l.load * 0.9;
                        }
                    }
                }
                fx.hurt(w, &[w.site(&site).id], 0.024, 1.6);
            }
            _ => {}
        }
    }
    fx
}

/// The daily curve at a site: low before dawn, a peak in the evening.
pub fn diurnal(t: i64, tz: f64) -> f64 {
    let h = ((t as f64 / 3600.0) + tz).rem_euclid(24.0);
    let evening = (-(h - 20.5).powi(2) / 18.0).exp();
    let day = (-(h - 13.0).powi(2) / 40.0).exp();
    0.34 + 0.52 * evening + 0.30 * day
}

fn row_id(kind: &str, name: &str, minute: i64) -> String {
    format!("{kind}:{name}:{minute}")
}

/// A point per live link for the minute starting at `t`.
pub fn link_points(w: &World, fx: &Effects, t: i64) -> Vec<Value> {
    let minute = t / 60;
    let mut out = Vec::with_capacity(w.links.len());
    for (i, l) in w.links.iter().enumerate().filter(|(_, l)| l.live) {
        let mut r = Rng::keyed(&l.id, minute as u64);
        let e = fx.link.get(&i).cloned().unwrap_or(LinkFx { scale: 1.0, ..Default::default() });
        let (sa, sb) = (w.site(w.devices[l.a].site), w.site(w.devices[l.b].site));
        let curve = (diurnal(t, sa.tz) + diurnal(t, sb.tz)) / 2.0;
        // A slow wander, so two days are not the same picture.
        let wander = 1.0 + 0.06 * ((t as f64 / 5400.0) + l.num as f64).sin();
        let down = e.down || fx.dev_down(l.a) || fx.dev_down(l.b);
        let mut util = (l.load * curve * wander * e.scale + e.extra_load * curve.max(0.8)) * (1.0 + 0.035 * r.normal());
        let mut loss = e.loss * (0.7 + 0.6 * r.unit());
        if util > 0.985 {
            // Past capacity the queue drops what does not fit.
            loss += (util - 0.985) * 60.0;
            util = 0.985 + 0.012 * r.unit();
        }
        if down {
            util = 0.0;
            loss = 100.0;
        }
        let bps = util * l.capacity_mbps as f64 * 1e6;
        let skew = 0.82 + 0.3 * r.unit();
        let errors = if down { 0.0 } else { e.errors * (0.6 + 0.8 * r.unit()) + if r.chance(0.01) { 1.0 } else { 0.0 } };
        let discards = if util > 0.93 { ((util - 0.93) * 9000.0 * (0.5 + r.unit())) as i64 } else { 0 };
        let rx = if down && e.rx.is_none() { -40.0 } else { e.rx.unwrap_or(-2.6 - (l.num % 5) as f64 * 0.35) + 0.08 * r.normal() };
        out.push(json!({
            "id": row_id("lm", &l.id, minute), "ts": iso(t), "link": l.id, "util": round(util, 4), "in_bps": (bps * skew).round(), "out_bps": (bps * (2.0 - skew) * 0.9).round(),
            "errors": errors.round() as i64, "discards": discards, "loss": round(loss.min(100.0), 3),
            "latency_ms": if down { Value::Null } else { json!(round(l.latency_ms * (1.0 + 0.012 * r.normal().abs()) + e.latency + (util - 0.8).max(0.0) * 14.0, 2)) },
            "rx_dbm": round(rx, 2), "up": i64::from(!down),
        }));
    }
    out
}

pub fn device_points(w: &World, fx: &Effects, t: i64) -> Vec<Value> {
    let minute = t / 60;
    w.devices
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let mut r = Rng::keyed(&d.id, minute as u64);
            let e = fx.dev.get(&i).cloned().unwrap_or(DevFx { bgp_mult: 1.0, sessions_mult: 1.0, ..Default::default() });
            let curve = diurnal(t, w.site(d.site).tz);
            let base = match d.role { "core" => 19.0, "border" => 27.0, "agg" => 16.0, "firewall" => 31.0, _ => 11.0 };
            let cpu = (base * (0.7 + 0.5 * curve) + e.cpu + 2.2 * r.normal()).clamp(1.0, 99.6);
            let mem = (38.0 + (d.num % 11) as f64 * 2.1 + e.mem + 0.4 * r.normal()).clamp(5.0, 99.0);
            let updates = match d.role { "border" => 90.0, "core" => 40.0, _ => 0.0 } * e.bgp_mult * (0.5 + r.unit());
            let bgp_up = w.peers.iter().enumerate().filter(|(pi, p)| p.device == i && !fx.bgp_down.contains(pi)).count();
            let sessions = if d.role == "firewall" { 410_000.0 * curve * e.sessions_mult * (0.95 + 0.1 * r.unit()) } else { 0.0 };
            if e.down {
                return json!({"id": row_id("dm", &d.id, minute), "ts": iso(t), "device": d.id, "up": 0});
            }
            json!({"id": row_id("dm", &d.id, minute), "ts": iso(t), "device": d.id, "cpu": round(cpu, 1), "mem": round(mem, 1),
                "temp_c": round(41.0 + cpu * 0.16 + (d.num % 7) as f64 + 0.3 * r.normal(), 1), "bgp_updates": updates.round() as i64, "bgp_up": bgp_up, "sessions": sessions.round() as i64, "up": 1})
        })
        .collect()
}

pub fn service_points(w: &World, fx: &Effects, t: i64) -> Vec<Value> {
    let minute = t / 60;
    w.services
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut r = Rng::keyed(s.id, minute as u64);
            let e = fx.svc.get(&i).cloned().unwrap_or(SvcFx { lat_mult: 1.0, ..Default::default() });
            let curve = s.sites.iter().map(|x| diurnal(t, w.site(x).tz)).sum::<f64>() / s.sites.len() as f64;
            let requests = (s.rps * 60.0 * curve * (1.0 + 0.03 * r.normal())).max(1.0);
            // The background failures stay inside a quarter to a half of what
            // the objective allows (99.99% allows 0.01%), so a calm network
            // keeps to every objective and a fault is what spends the budget.
            let u = r.unit();
            let noise = (0.00018 + 0.00012 * u).min((1.0 - s.slo) * (0.25 + 0.25 * u));
            let rate = (noise + e.err * (0.75 + 0.5 * r.unit())).min(0.9);
            let p50 = s.base_latency_ms * e.lat_mult * (1.0 + 0.04 * r.normal().abs());
            json!({"id": row_id("sm", s.id, minute), "ts": iso(t), "service": s.id, "requests": requests.round() as i64, "errors": (requests * rate).round() as i64,
                "latency_p50": round(p50, 2), "latency_p95": round(p50 * (1.9 + 0.5 * r.unit()) * (1.0 + (e.lat_mult - 1.0) * 0.6), 2)})
        })
        .collect()
}

const SOURCES: &[(i64, &str, &[(&str, &str, i64, f64)])] = &[
    (15169, "Google", &[("quic", "udp", 443, 0.20), ("https", "tcp", 443, 0.08)]),
    (2906, "Netflix", &[("video", "tcp", 443, 0.19)]),
    (20940, "Akamai", &[("https", "tcp", 443, 0.11)]),
    (32934, "Meta", &[("quic", "udp", 443, 0.09)]),
    (16509, "Amazon", &[("https", "tcp", 443, 0.08)]),
    (13335, "Cloudflare", &[("https", "tcp", 443, 0.07), ("dns", "udp", 53, 0.004)]),
    (8075, "Microsoft", &[("https", "tcp", 443, 0.06)]),
    (714, "Apple", &[("https", "tcp", 443, 0.04)]),
];

const BOTNET: &[(i64, &str, &str, i64)] = &[
    (4134, "Chinanet", "ntp", 123), (9121, "Turk Telekom", "dns", 53), (45899, "VNPT", "ssdp", 1900), (7922, "Comcast", "dns", 53),
    (3320, "Deutsche Telekom", "ntp", 123), (12389, "Rostelecom", "memcached", 11211), (9829, "BSNL", "dns", 53), (28573, "Claro", "ssdp", 1900),
    (4766, "Korea Telecom", "ntp", 123), (8151, "Telmex", "dns", 53), (17974, "Telkomnet", "cldap", 389), (6830, "Liberty Global", "ntp", 123),
];

/// Flow summaries for the minute: the largest sources into each hub, and
/// an attack's flood when there is one.
pub fn flow_rows(fx: &Effects, t: i64) -> Vec<Value> {
    let minute = t / 60;
    let mut out = Vec::new();
    for site in crate::world::SITES.iter().filter(|s| s.kind != "pop") {
        let curve = diurnal(t, site.tz);
        let exporter = format!("br1-{}", site.id);
        for &(asn, name, apps) in SOURCES {
            for &(app, proto, port, share) in apps {
                let mut r = Rng::keyed(&format!("{}{asn}{app}", site.id), minute as u64);
                if r.chance(0.35) {
                    continue;
                }
                let bytes = 46e9 * site.weight * share * curve * (0.75 + 0.5 * r.unit()) * 60.0 / 8.0;
                out.push(json!({"id": format!("fl:{}:{asn}:{app}:{minute}", site.id), "ts": iso(t), "exporter": exporter, "src_site": "internet", "dst_site": site.id,
                    "src_asn": asn, "src_name": name, "dst_net": format!("100.{}.0.0/14", 64 + (site.lat as i64 % 32)), "app": app, "proto": proto, "dst_port": port,
                    "bytes": bytes.round() as i64, "packets": (bytes / 1100.0).round() as i64, "flows": (bytes / 2.4e6).round() as i64}));
            }
        }
    }
    for (site, prefix, strength, scrubbed) in &fx.ddos {
        for &(asn, name, app, port) in BOTNET {
            let mut r = Rng::keyed(&format!("ddos{asn}"), minute as u64);
            let bytes = 31e9 * strength * (0.5 + r.unit()) * 60.0 / 8.0 / BOTNET.len() as f64 * 3.0;
            out.push(json!({"id": format!("fl:{site}:atk{asn}:{minute}"), "ts": iso(t), "exporter": format!("br{}-{site}", 1 + (asn % 2)), "src_site": "internet",
                "dst_site": if *scrubbed { "scrubbing" } else { site.as_str() }, "src_asn": asn, "src_name": name, "dst_net": prefix, "app": format!("{app}-amplification"),
                "proto": "udp", "dst_port": port, "bytes": bytes.round() as i64, "packets": (bytes / 460.0).round() as i64, "flows": (bytes / 9.0e4).round() as i64}));
        }
    }
    out
}

pub struct Log {
    pub t: i64,
    pub device: usize,
    pub sev: i64,
    pub program: &'static str,
    pub template: &'static str,
    pub message: String,
}

const SEV: &[&str] = &["emergency", "alert", "critical", "error", "warning", "notice", "info", "debug"];

pub fn log_rows(w: &World, logs: &[Log]) -> Vec<Value> {
    logs.iter()
        .enumerate()
        .map(|(n, l)| {
            let d = &w.devices[l.device];
            json!({"id": format!("sl:{}:{}:{n}:{}", l.t, d.id, l.template), "ts": iso(l.t), "device": d.id, "site": d.site, "severity": SEV[l.sev.clamp(0, 7) as usize], "sev": l.sev,
                "program": l.program, "template": l.template, "message": l.message})
        })
        .collect()
}

fn push(out: &mut Vec<Log>, t: i64, device: usize, sev: i64, program: &'static str, template: &'static str, message: String) {
    out.push(Log { t, device, sev, program, template, message });
}

/// The neighbour lines a link's two ends log when it comes up: LLDP naming
/// the far device and port, and the routing adjacency. These are what the
/// topology is read back from.
pub fn adjacency_logs(w: &World, li: usize, t: i64, out: &mut Vec<Log>) {
    let l = &w.links[li];
    for (me, my_if, other, their_if) in [(l.a, &l.a_if, l.b, &l.z_if), (l.b, &l.z_if, l.a, &l.a_if)] {
        let (d, o) = (&w.devices[me], &w.devices[other]);
        push(out, t, me, 5, "lldpd", "LLDP_NEIGHBOR_UP", format!("LLDP_NEIGHBOR_UP: interface {my_if} neighbor {} chassis {} port {their_if} ttl 120", o.id, o.loopback));
        if d.role == "core" || d.role == "border" || d.role == "agg" {
            if o.role == "core" || o.role == "border" || o.role == "agg" {
                push(out, t + 2, me, 5, "rpd", "OSPF_ADJ_FULL", format!("RPD_OSPF_NBRUP: OSPF neighbor {} (realm ospf-v2 {my_if}.0 area 0.0.0.0) state changed from Loading to Full", o.loopback));
            }
        }
    }
}

fn down_logs(w: &World, li: usize, t: i64, reason: &str, out: &mut Vec<Log>) {
    let l = &w.links[li];
    for (me, my_if, other) in [(l.a, &l.a_if, l.b), (l.b, &l.z_if, l.a)] {
        let o = &w.devices[other];
        push(out, t, me, 3, "mib2d", "LINK_DOWN", format!("SNMP_TRAP_LINK_DOWN: ifIndex {}, ifAdminStatus up(1), ifOperStatus down(2), ifName {my_if}", 500 + l.num));
        push(out, t + 1, me, 4, "lldpd", "LLDP_NEIGHBOR_DOWN", format!("LLDP_NEIGHBOR_DOWN: interface {my_if} neighbor {} aged out", o.id));
        push(out, t + 1, me, 3, "rpd", "OSPF_ADJ_DOWN", format!("RPD_OSPF_NBRDOWN: OSPF neighbor {} (realm ospf-v2 {my_if}.0 area 0.0.0.0) state changed from Full to Down due to {reason}", o.loopback));
    }
}

/// The lines the faults add in the minute starting at `t`.
pub fn fault_logs(w: &World, faults: &[Fault], t: i64, out: &mut Vec<Log>) {
    let minute = t / 60;
    for f in faults {
        let first = f.started / 60 == minute;
        let active = f.active_at(t);
        let cleared_now = f.ended.is_some_and(|e| e / 60 == minute);
        let mitigated_now = f.mitigated.is_some_and(|m| m / 60 == minute);
        if !(active || cleared_now) {
            continue;
        }
        let age = (t - f.started) / 60;
        let mut r = Rng::keyed(&f.id, minute as u64);
        let link = f.link.as_deref().and_then(|l| w.link_by_id(l));
        let dev = f.device.as_deref().and_then(|d| w.devices.iter().position(|x| x.id == d));
        match f.scenario.as_str() {
            "fiber_cut" => {
                let Some(li) = link else { continue };
                let l = &w.links[li];
                if first {
                    for (me, my_if) in [(l.a, &l.a_if), (l.b, &l.z_if)] {
                        push(out, f.started, me, 2, "chassisd", "OPTICS_LOS", format!("CHASSISD_OPTICS_ALARM: {my_if}: Rx loss of signal alarm set, rx power -40.00 dBm (circuit {})", l.circuit));
                        push(out, f.started + 3, me, 3, "rpd", "RSVP_LSP_DOWN", format!("RPD_MPLS_LSP_DOWN: MPLS LSP {}-to-{} down on primary path, reason: link down {my_if}", w.devices[l.a].site.to_uppercase(), w.devices[l.b].site.to_uppercase()));
                    }
                    down_logs(w, li, f.started + 1, "interface went down", out);
                    if let Some(s) = l.sibling {
                        let sl = &w.links[s];
                        push(out, f.started + 9, sl.a, 4, "rpd", "RSVP_REROUTE", format!("RPD_MPLS_PATH_UP: MPLS LSP rerouted onto secondary path via {} (plane {})", sl.a_if, sl.plane));
                    }
                } else if active && !f.mitigated_at(t) && age % 3 == 1 {
                    if let Some(s) = l.sibling {
                        let sl = &w.links[s];
                        push(out, t + 20, sl.a, 4, "cosd", "QUEUE_DROPS", format!("COSD_OUT_OF_PROFILE: {}: queue 3 (best-effort) tail drops {} pkts in 60s, utilisation above 97%", sl.a_if, 3000 + r.below(9000)));
                    }
                }
                if cleared_now {
                    for (me, my_if) in [(l.a, &l.a_if), (l.b, &l.z_if)] {
                        push(out, t + 5, me, 5, "chassisd", "OPTICS_LOS_CLEAR", format!("CHASSISD_OPTICS_ALARM: {my_if}: Rx loss of signal alarm cleared, rx power -2.71 dBm"));
                        push(out, t + 6, me, 5, "mib2d", "LINK_UP", format!("SNMP_TRAP_LINK_UP: ifIndex {}, ifAdminStatus up(1), ifOperStatus up(1), ifName {my_if}", 500 + l.num));
                    }
                    adjacency_logs(w, li, t + 8, out);
                }
            }
            "optic_degrade" => {
                let Some(li) = link else { continue };
                let l = &w.links[li];
                let p = (age as f64 / 24.0).min(1.0);
                if active && !f.mitigated_at(t) {
                    if age >= 4 && age % 2 == 0 {
                        push(out, t + 7, l.b, 4, "chassisd", "OPTICS_RX_LOW", format!("CHASSISD_OPTICS_WARNING: {}: Rx power low warning set, rx power {:.2} dBm (threshold -12.00 dBm)", l.z_if, -2.8 - 15.5 * p.powf(1.4)));
                    }
                    if age >= 7 {
                        push(out, t + 30, l.b, 3, "dcd", "FCS_ERRORS", format!("DCD_FRAMING_ERRORS: {}: input errors: {} CRC/FCS in last 60s", l.z_if, (3.0 * (p * 7.5).exp()) as i64));
                    }
                    if p > 0.72 && r.chance(0.33) {
                        down_logs(w, li, t + 12, "interface flap", out);
                        adjacency_logs(w, li, t + 41, out);
                    }
                }
                if mitigated_now {
                    push(out, t + 2, l.a, 5, "mgd", "UI_COMMIT", format!("UI_COMMIT_COMPLETED: commit complete: set protocols ospf area 0 interface {}.0 metric 65535 (drain)", l.a_if));
                }
                if cleared_now {
                    push(out, t + 4, l.b, 5, "chassisd", "OPTICS_INSERTED", format!("CHASSISD_SFP_INSERTED: {}: optic inserted, QSFP28-100G-LR4, rx power -2.65 dBm", l.z_if));
                    adjacency_logs(w, li, t + 30, out);
                }
            }
            "bad_change" => {
                let Some(di) = dev else { continue };
                let chg = f.change.clone().unwrap_or_default();
                if first {
                    push(out, f.started - 20, di, 5, "config", "CONFIG_COMMIT", format!("%MGBL-CONFIG-6-DB_COMMIT : Configuration committed by user 'jvermeer'. Use 'show configuration commit changes 10000{}' to view the changes. ({chg})", 400 + r.below(90)));
                    push(out, f.started + 2, di, 3, "policy_repo", "POLICY_ERR", "%ROUTING-POLICY_REPO-3-ERR_PREFIX_SET : route-policy RP-TRANSIT-IN references prefix-set PS-BOGONS-V4 which is not defined; policy evaluates to drop".to_string());
                    for p in w.peers.iter().filter(|p| p.device == di) {
                        push(out, f.started + 4 + r.below(8) as i64, di, 3, "bgp", "BGP_ADJ_DOWN", format!("%ROUTING-BGP-5-ADJCHANGE : neighbor {} Down - BGP Notification sent, policy change (VRF: default) (AS: {})", p.peer_ip, p.asn));
                    }
                } else if active && !f.mitigated_at(t) && age % 2 == 1 {
                    let peers: Vec<_> = w.peers.iter().filter(|p| p.device == di).collect();
                    if !peers.is_empty() {
                        let p = peers[r.below(peers.len())];
                        push(out, t + 15, di, 4, "bgp", "BGP_MAXPFX", format!("%ROUTING-BGP-4-NBR_NO_ROUTES : neighbor {} (AS: {}, {}) session Idle: 0 prefixes accepted by RP-TRANSIT-IN", p.peer_ip, p.asn, p.name));
                    }
                }
                if mitigated_now {
                    push(out, t + 1, di, 5, "config", "CONFIG_ROLLBACK", format!("%MGBL-CONFIG-6-DB_COMMIT : Configuration rolled back to commit before {chg} by user 'noc-automation'"));
                    for p in w.peers.iter().filter(|p| p.device == di) {
                        push(out, t + 20 + r.below(25) as i64, di, 5, "bgp", "BGP_ADJ_UP", format!("%ROUTING-BGP-5-ADJCHANGE : neighbor {} Up (VRF: default) (AS: {})", p.peer_ip, p.asn));
                    }
                }
            }
            "ddos" => {
                let site = f.site.clone().unwrap_or_default();
                let prefix = f.customer.clone().unwrap_or_default();
                if active && !f.mitigated_at(t) {
                    for b in w.devices.iter().enumerate().filter(|(_, d)| d.site == site && d.role == "border").map(|(i, _)| i) {
                        if age % 2 == 0 {
                            push(out, t + 10, b, 4, "ipv4_acl", "ACL_DENY", format!("%SEC-6-IPACCESSLOGP : list EDGE-IN denied udp {}.{}.{}.{}({}) -> {}({}), {} packets", 20 + r.below(200), r.below(255), r.below(255), 1 + r.below(250), [53, 123, 1900, 11211][r.below(4)], prefix.replace("0/24", &format!("{}", 2 + r.below(250))), 1024 + r.below(60000), 800 + r.below(9000)));
                        }
                        if age >= 1 && age % 3 == 1 {
                            push(out, t + 25, b, 3, "lpts", "DDOS_POLICER", "%OS-LPTS-3-POLICER_VIOLATION : flow UDP-default exceeded its allowed rate: 2100000 pps offered, 25000 pps allowed, dropping excess".to_string());
                        }
                    }
                    for fw in w.devices.iter().enumerate().filter(|(_, d)| d.site == site && d.role == "firewall").map(|(i, _)| i) {
                        if age >= 2 && age % 3 == 0 {
                            push(out, t + 33, fw, 3, "dataplane", "SESSION_TABLE", format!("SYSTEM: session table utilisation {}% (threshold 80%), flood protection activated on zone untrust for {prefix}", 81 + r.below(17)));
                        }
                    }
                }
                if mitigated_now {
                    if let Some(b) = w.devices.iter().position(|d| d.site == site && d.role == "border") {
                        push(out, t + 2, b, 5, "bgp", "BGP_FLOWSPEC", format!("%ROUTING-BGP-5-FLOWSPEC : installed flowspec rule: dst {prefix} proto udp src-port {{53,123,1900,11211}} action redirect to scrubbing VRF"));
                    }
                }
            }
            "cpu_bug" => {
                let Some(di) = dev else { continue };
                if active && !f.mitigated_at(t) {
                    if age >= 1 {
                        push(out, t + 14, di, 3, "rpd", "RPD_SCHED_SLIP", format!("RPD_SCHED_SLIP: {} sec scheduler slip, user: {} sec {} usec, system: 0 sec, 0 usec", 4 + r.below(9), 3 + r.below(8), r.below(999_999)));
                    }
                    if age >= 2 && age % 2 == 0 {
                        push(out, t + 31, di, 4, "kernel", "CPU_HOG", format!("kernel: rpd[{}]: process using {}% CPU for 60s; BGP update threads blocked in rib walk", 6000 + (f.started % 900), 94 + r.below(6)));
                    }
                    if age >= 4 && r.chance(0.4) {
                        let links = w.links_of(di);
                        if !links.is_empty() {
                            let li = links[r.below(links.len())];
                            let l = &w.links[li];
                            let other = if l.a == di { l.b } else { l.a };
                            push(out, t + 44, other, 4, "rpd", "BGP_HOLD_EXPIRED", format!("bgp_hold_timeout: NOTIFICATION sent to {} (Internal AS 64512): code 4 (Hold Timer Expired Error), Reason: holdtime expired", w.devices[di].loopback));
                        }
                    }
                }
                if mitigated_now {
                    push(out, t + 3, di, 5, "init", "PROCESS_RESTART", "init: routing (PID 6231) terminate signal sent by operator; routing (PID 9977) started; graceful restart in progress".to_string());
                }
            }
            "power" => {
                let site = f.site.clone().unwrap_or_default();
                let dark = feed_a(w, &site);
                if first {
                    // The dark devices say nothing; their neighbours and the
                    // devices on feed B do.
                    for &di in &dark {
                        for li in w.links_of(di) {
                            let l = &w.links[li];
                            let (other, their_if) = if l.a == di { (l.b, &l.z_if) } else { (l.a, &l.a_if) };
                            if dark.contains(&other) {
                                continue;
                            }
                            push(out, f.started + 2, other, 3, "mib2d", "LINK_DOWN", format!("SNMP_TRAP_LINK_DOWN: ifIndex {}, ifAdminStatus up(1), ifOperStatus down(2), ifName {their_if}", 500 + l.num));
                            push(out, f.started + 3, other, 3, "rpd", "OSPF_ADJ_DOWN", format!("RPD_OSPF_NBRDOWN: OSPF neighbor {} (realm ospf-v2 {their_if}.0 area 0.0.0.0) state changed from Full to Down due to interface went down", w.devices[di].loopback));
                        }
                    }
                    for (di, _) in w.devices.iter().enumerate().filter(|(i, d)| d.site == site && !dark.contains(i)) {
                        push(out, f.started + 1, di, 2, "chassisd", "PEM_INPUT_FAIL", format!("CHASSISD_PEM_INPUT_BAD: Power supply PEM 0 (feed A, PDU-A-{}) input failure; running on PEM 1 (feed B)", site.to_uppercase()));
                    }
                }
                if mitigated_now {
                    for (di, _) in w.devices.iter().enumerate().filter(|(i, d)| d.site == site && !dark.contains(i)) {
                        push(out, t + 1, di, 5, "chassisd", "PEM_INPUT_OK", "CHASSISD_PEM_INPUT_OK: Power supply PEM 0 (feed A) input restored".to_string());
                    }
                    for &di in &dark {
                        for li in w.links_of(di) {
                            adjacency_logs(w, li, t + 70, out);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// The ordinary lines of a minute: logins, denied packets at the edge, a
/// customer port flapping, a neighbour announcing itself again.
pub fn baseline_logs(w: &World, fx: &Effects, t: i64, out: &mut Vec<Log>) {
    let minute = t / 60;
    let mut r = Rng::keyed("baseline", minute as u64);
    let n = 5 + r.below(6);
    for _ in 0..n {
        let di = r.below(w.devices.len());
        if fx.dev_down(di) {
            continue;
        }
        let d = &w.devices[di];
        let at = t + r.below(60) as i64;
        match r.below(9) {
            0 => push(out, at, di, 6, "sshd", "SSH_LOGIN", format!("sshd[{}]: Accepted publickey for {} from 10.250.0.{} port {} ssh2", 2000 + r.below(60000), ["netconf", "oxidized", "ansible", "jvermeer", "mokafor", "lchen"][r.below(6)], 10 + r.below(20), 40000 + r.below(20000))),
            1 if d.role == "border" => push(out, at, di, 6, "ipv4_acl", "ACL_DENY", format!("%SEC-6-IPACCESSLOGP : list EDGE-IN denied tcp {}.{}.{}.{}({}) -> 198.51.100.{}({}), {} packets", 20 + r.below(200), r.below(255), r.below(255), 1 + r.below(250), 1024 + r.below(60000), 1 + r.below(250), [22, 23, 3389, 445, 179][r.below(5)], 1 + r.below(20))),
            2 if d.role == "agg" => {
                let port = 20 + r.below(24);
                let c = &w.customers[r.below(w.customers.len())];
                push(out, at, di, 5, "Ebra", "CUSTOMER_PORT_FLAP", format!("%LINEPROTO-5-UPDOWN: Line protocol on Interface Ethernet{port}/1 ({} access), changed state to {}", c.id, if r.chance(0.5) { "down" } else { "up" }));
            }
            3 => push(out, at, di, 6, "ntpd", "NTP_SYNC", format!("ntpd[{}]: synchronized to 10.250.0.{}, stratum 2, offset {:.3} ms", 900 + r.below(100), 1 + r.below(2), r.range(-0.4, 0.4))),
            4 => push(out, at, di, 5, "snmpd", "SNMP_AUTH_FAIL", format!("SNMPD_AUTH_FAILURE: nsa_log_community: unauthorized SNMP community from 10.250.3.{} to {}", 2 + r.below(40), d.mgmt_ip)),
            5 if d.role == "firewall" => push(out, at, di, 6, "dataplane", "FW_DENY", format!("TRAFFIC drop rule=default-deny src={}.{}.{}.{} dst=10.{}.20.{} app=incomplete proto=tcp dport={}", 40 + r.below(180), r.below(255), r.below(255), 1 + r.below(250), 10 + r.below(10), 1 + r.below(250), [8080, 6379, 9200, 5432][r.below(4)])),
            6 => {
                let links = w.links_of(di);
                if let Some(&li) = links.get(r.below(links.len().max(1))) {
                    let l = &w.links[li];
                    if l.live && !fx.link_down(li) {
                        let (my_if, other, their_if) = if l.a == di { (&l.a_if, l.b, &l.z_if) } else { (&l.z_if, l.a, &l.a_if) };
                        push(out, at, di, 6, "lldpd", "LLDP_NEIGHBOR_REFRESH", format!("LLDP_NEIGHBOR_REFRESH: interface {my_if} neighbor {} chassis {} port {their_if} ttl 120", w.devices[other].id, w.devices[other].loopback));
                    }
                }
            }
            7 if d.role == "core" => push(out, at, di, 6, "rpd", "BGP_KEEPALIVE_STATS", format!("bgp_io_stats: peer group IBGP-RR: {} updates received, {} sent in last 300s", 100 + r.below(900), 100 + r.below(900))),
            _ => push(out, at, di, 6, "cron", "CONFIG_BACKUP", format!("cron[{}]: (oxidized) configuration archived, {} lines, no changes", 300 + r.below(600), 800 + (d.num * 37) % 900)),
        }
    }
}

pub fn round(v: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (v * m).round() / m
}

/// Every row the network reports for the minute starting at `t`:
/// (table, rows).
pub fn minute(w: &World, faults: &[Fault], t: i64) -> Vec<(&'static str, Vec<Value>)> {
    let fx = effects(w, faults, t);
    let mut logs = Vec::new();
    baseline_logs(w, &fx, t, &mut logs);
    fault_logs(w, faults, t, &mut logs);
    vec![
        ("netops_link_metrics", link_points(w, &fx, t)),
        ("netops_device_metrics", device_points(w, &fx, t)),
        ("netops_service_metrics", service_points(w, &fx, t)),
        ("netops_flows", flow_rows(&fx, t)),
        ("netops_syslog", log_rows(w, &logs)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fault(scenario: &str, w: &World) -> Fault {
        let link = w.links.iter().find(|l| l.kind == "backbone" && l.live).map(|l| l.id.clone());
        Fault { id: "f1".into(), scenario: scenario.into(), started: 1_800_000_000, mitigated: None, ended: None, link, device: Some("br1-lon".into()), site: Some("waw".into()),
            change: Some("CHG-1".into()), customer: Some("203.0.113.0/24".into()), truth_kind: "link".into(), truth_entity: "x".into(), category: "x".into(), incident: None, origin: "live".into(), auto_mitigate_after: 600, repair_after: 600 }
    }

    #[test]
    fn a_minute_is_the_same_twice() {
        let w = World::build();
        let f = [fault("fiber_cut", &w)];
        let a = serde_json::to_string(&minute(&w, &f, 1_800_000_120).into_iter().map(|x| x.1).collect::<Vec<_>>()).unwrap();
        let b = serde_json::to_string(&minute(&w, &f, 1_800_000_120).into_iter().map(|x| x.1).collect::<Vec<_>>()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_fibre_cut_takes_the_link_down_and_fills_its_sibling() {
        let w = World::build();
        let f = fault("fiber_cut", &w);
        let li = w.link_by_id(f.link.as_ref().unwrap()).unwrap();
        let sib = w.links[li].sibling.unwrap();
        let t = f.started + 300;
        let calm = link_points(&w, &effects(&w, &[], t), t);
        let hurt = link_points(&w, &effects(&w, &[f.clone()], t), t);
        let of = |rows: &[Value], id: &str| rows.iter().find(|r| r["link"] == id).cloned().unwrap();
        assert_eq!(of(&hurt, &w.links[li].id)["up"], 0);
        assert!(of(&hurt, &w.links[sib].id)["util"].as_f64().unwrap() > of(&calm, &w.links[sib].id)["util"].as_f64().unwrap() * 1.5);
        let mut logs = Vec::new();
        fault_logs(&w, &[f], 1_800_000_000, &mut logs);
        assert!(logs.iter().any(|l| l.template == "OPTICS_LOS"));
        assert!(logs.iter().any(|l| l.template == "OSPF_ADJ_DOWN"));
    }

    #[test]
    fn every_scenario_changes_something() {
        let w = World::build();
        for (s, _, _) in SCENARIOS {
            let f = fault(s, &w);
            let fx = effects(&w, &[f], 1_800_000_000 + 900);
            assert!(!fx.link.is_empty() || !fx.dev.is_empty() || !fx.ddos.is_empty(), "{s}");
        }
    }
}
