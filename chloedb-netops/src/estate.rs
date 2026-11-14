//! The estate: everything the backbone carries, as one graph in ChloeDB.
//!
//! The simulated carrier of `world.rs` is 64 devices. A carrier's network
//! is that and, hanging off it, tens of thousands of things: metro hubs,
//! their routers and power plants, street cabinets, cell sites, business
//! parks, and the ONTs, radios and customer routers behind them. This
//! module makes that network (`Estate::build`, about `--estate` objects),
//! writes it as rows of `netops_estate_nodes` and `netops_estate_edges` (an
//! edge table, upstream to downstream, so `REACHABLE(..., dir => 'out')`
//! is what depends on a thing and `dir => 'in'` what it depends on), with
//! two months of faults and changes in `netops_estate_events`, and then
//! keeps a few faults going.
//!
//! Every object has a place on a plane, inside its site, its hub, its
//! metro and its region, and a level of detail (`tier`) from which it is
//! worth drawing. A map is then two range queries, and what is broken
//! inside a region is a `GROUP BY` of the rows that are not `ok`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Map, Value};

use crate::api::{bad, Answer, Q, Shared};
use crate::sim::Engine;
use crate::strata::{Result, Row, RowExt};
use crate::telemetry::{iso, round};
use crate::world::{Rng, World};

pub const NODES: &str = "netops_estate_nodes";
pub const EDGES: &str = "netops_estate_edges";
pub const EVENTS: &str = "netops_estate_events";
/// Days of faults and changes written when the estate is made.
pub const HISTORY_DAYS: i64 = 60;
const NONE: u32 = u32::MAX;

/// The people (and the pipeline) whose names are on changes.
pub const OPERATORS: &[&str] = &["m.rossi", "a.khan", "j.muller", "s.dubois", "l.garcia", "t.nowak", "e.lindqvist", "automation"];

pub struct Node {
    pub id: String,
    /// region, metro, hub, site (the containers); a core device's role;
    /// hub-router, power, olt, cell, switch; ont, radio, cpe.
    pub kind: &'static str,
    /// container, core, aggregation, power, access, premises.
    pub layer: &'static str,
    /// The level of detail it is drawn from: 0 regions, 1 metros, 2 hubs,
    /// 3 devices and sites, 4 what is behind an access node.
    pub tier: u8,
    pub name: String,
    /// Its region, metro, hub and site (a container is its own).
    pub anc: [u32; 4],
    pub x: f64,
    pub y: f64,
    /// Half the shorter side of a place (what a level of detail is chosen by).
    pub r: f64,
    /// A place's width and height: places are boxes, one inside the other.
    pub w: f64,
    pub h: f64,
    pub vendor: &'static str,
    pub model: &'static str,
    pub version: String,
    pub customer: String,
    /// Round trip from the metro's core when nothing is wrong, ms.
    pub base_ms: f64,
    pub children: u32,
}

/// A fault that is going on.
#[derive(Clone)]
pub struct Live {
    pub id: String,
    pub root: u32,
    pub hit: Vec<u32>,
    pub started: i64,
    pub until: i64,
    pub text: String,
    pub cause: &'static str,
}

pub struct Estate {
    pub nodes: Vec<Node>,
    /// Upstream, downstream, kind.
    pub edges: Vec<(u32, u32, &'static str)>,
    index: HashMap<String, u32>,
    down: Vec<Vec<u32>>,
    pub up: Vec<Vec<u32>>,
    /// Between metros: how many backbone links.
    pub trunks: Vec<(String, String, usize)>,
    /// The radius of a hub and of a site, for choosing the level of detail.
    pub hub_r: f64,
    pub site_r: f64,
    classes: [Vec<u32>; 4],
    pub live: Mutex<Vec<Live>>,
}

struct Template {
    cause: &'static str,
    state: &'static str,
    severity: &'static str,
    text: &'static str,
    /// The change that came before it, when one did.
    change: Option<&'static str>,
}

macro_rules! t {
    ($cause:expr, $state:expr, $severity:expr, $text:expr, $change:expr) => {
        Template { cause: $cause, state: $state, severity: $severity, text: $text, change: $change }
    };
}

fn templates(kind: &str) -> &'static [Template] {
    match kind {
        "olt" => &[
            t!("fibre", "down", "critical", "PON port {p} lost signal on every ONT at once: feeder fibre cut suspected", None),
            t!("hardware", "degraded", "major", "Line card {p} failed its self-test and was taken out of service", None),
            t!("software", "degraded", "major", "Control plane restarted after the upgrade to {v}: ONTs are ranging again", Some("Software upgrade to {v}")),
            t!("power", "degraded", "major", "Mains failure at the cabinet: running on battery, {n} minutes left", None),
            t!("config", "degraded", "major", "ONTs fail to authenticate after a change to the VLAN profile", Some("Configuration push: new VLAN profile for residential service")),
        ],
        "cell" => &[
            t!("power", "down", "critical", "Cell site lost mains power and the battery is exhausted", None),
            t!("weather", "degraded", "minor", "Microwave backhaul fading in heavy rain: modulation dropped to QPSK and latency is up", None),
            t!("config", "degraded", "major", "Radios lost synchronisation after a change to the timing configuration", Some("Configuration push: PTP timing profile changed")),
            t!("hardware", "down", "critical", "Fan tray failure: the router shut down on high temperature", None),
            t!("software", "degraded", "major", "Packet loss on the backhaul after the upgrade to {v}: latency is three times its usual", Some("Software upgrade to {v}")),
        ],
        "switch" => &[
            t!("config", "degraded", "major", "Spanning-tree loop after a VLAN change: customer ports were disabled", Some("Configuration push: VLAN {p} added for a new customer")),
            t!("hardware", "down", "critical", "Uplink optic failed: receive power fell below the threshold", None),
            t!("software", "degraded", "major", "Memory leak in the forwarding process: packets dropped on customer ports and latency is up", Some("Software upgrade to {v}")),
            t!("fibre", "down", "critical", "Both uplinks lost signal: a duct was damaged by construction work nearby", None),
        ],
        "hub-router" => &[
            t!("software", "degraded", "critical", "Routing process crashed after the upgrade to {v}: sessions to the access nodes reset", Some("Software upgrade to {v}")),
            t!("hardware", "degraded", "critical", "Line card {p} failed: half of the access uplinks are down", None),
            t!("config", "degraded", "major", "Access policy pushed with a wrong prefix list: customer routes withdrawn", Some("Configuration push: prefix list for access aggregation updated")),
            t!("congestion", "degraded", "minor", "Uplink at 97% of capacity: queues are dropping and latency is up", None),
        ],
        "power" => &[
            t!("power", "degraded", "critical", "Rectifier failure at the hub: both routers are on battery", None),
            t!("power", "degraded", "major", "Mains supply lost at the hub: the generator started and carries the load", None),
        ],
        "ont" => &[
            t!("power", "down", "minor", "ONT sent a dying gasp: power lost at the premises", None),
            t!("fibre", "degraded", "minor", "ONT receive power is low: a dirty connector or a bent drop fibre", None),
        ],
        "radio" => &[
            t!("hardware", "degraded", "minor", "Radio unit raised a VSWR alarm: antenna feeder fault", None),
            t!("hardware", "down", "major", "Radio sector is down: the unit does not answer", None),
            t!("software", "degraded", "minor", "Radio unit restarts every few minutes after the upgrade to {v}", Some("Software upgrade to {v}")),
        ],
        _ => &[
            t!("unknown", "down", "minor", "Customer router is unreachable: no answer to management for {n} minutes", None),
            t!("config", "degraded", "minor", "WAN interface flaps after a change to the customer's QoS profile and latency is up", Some("Configuration push: QoS profile changed at the customer's request")),
            t!("hardware", "degraded", "minor", "Customer router reports a failed power supply and runs on the second one", None),
        ],
    }
}

/// Changes that are simply the work of the week.
const ROUTINE: &[(&str, &str)] = &[
    ("provisioning", "New customer service provisioned on port {p}"),
    ("maintenance", "Battery string replaced during the planned site visit"),
    ("software", "Firmware upgrade to {v} as part of the quarterly rollout"),
    ("fibre", "Planned fibre splice on the feeder cable: two new splitters added"),
    ("config", "QoS profile updated for the business service tier"),
    ("config", "Configuration push: SNMP and telemetry collectors moved to the new addresses"),
    ("hardware", "Faulty optic replaced on port {p}"),
    ("maintenance", "Cabinet cooling unit serviced and filters replaced"),
];

fn gear(kind: &str, n: usize) -> (&'static str, &'static str, &'static [&'static str]) {
    let list: &[(&str, &str, &[&str])] = match kind {
        "hub-router" => &[("Nokia", "7750 SR-1", &["23.10.R2", "23.10.R2", "22.10.R6", "24.3.R1"]), ("Juniper", "ACX7509", &["23.2R2", "22.4R3-S2", "23.4R1"])],
        "power" => &[("Eltek", "Flatpack2", &["4.1"]), ("Vertiv", "NetSure 7100", &["2.8"])],
        "olt" => &[("Nokia", "7360 ISAM FX", &["6.7.03", "6.7.03", "6.5.02", "6.8.01"]), ("Calix", "E7-2", &["23.4.0", "22.4.510", "24.1.0"]), ("Adtran", "SDX 6320", &["24.5", "23.11"])],
        "cell" => &[("Ericsson", "Router 6672", &["23.Q4", "23.Q2", "24.Q1"]), ("Nokia", "7250 IXR-e", &["23.10.R2", "22.10.R6"])],
        "switch" => &[("Cisco", "NCS 540", &["7.9.2", "7.7.21", "7.10.1"]), ("Juniper", "ACX7100", &["23.2R2", "22.4R3-S2"])],
        "ont" => &[("Nokia", "G-010G-Q", &["3FE49337", "3FE48077"]), ("Calix", "GigaSpire u6", &["23.4.0", "22.4.0"]), ("Zyxel", "PM7300", &["V5.13", "V5.11"])],
        "radio" => &[("Ericsson", "AIR 6449", &["23.Q4", "23.Q2", "24.Q1"]), ("Nokia", "AirScale AAHF", &["SBTS23R4", "SBTS23R2"])],
        _ => &[("Cisco", "ISR 1100", &["17.9.4", "17.6.5", "17.12.2"]), ("Juniper", "SRX300", &["21.4R3", "22.4R3-S2"]), ("MikroTik", "CCR2004", &["7.12", "7.10.2"])],
    };
    list[n % list.len()]
}

const FIRMS_A: &[&str] = &["Northwind", "Harbour", "Meridian", "Alder", "Kestrel", "Bluewater", "Ironbridge", "Saffron", "Lumen", "Cobalt", "Halcyon", "Redwood", "Castell", "Orchard", "Vantage", "Tidewater"];
const FIRMS_B: &[&str] = &["Logistics", "Clinics", "Bank", "Foods", "Studios", "Pharma", "Hotels", "Retail", "Law", "Motors", "Schools", "Insurance", "Print", "Energy", "Labs", "Freight"];

impl Estate {
    /// The backbone of `w` and, behind each of its sites, about `size`
    /// objects in all.
    pub fn build(w: &World, size: usize) -> Estate {
        let mut e = Estate { nodes: Vec::new(), edges: Vec::new(), index: HashMap::new(), down: Vec::new(), up: Vec::new(), trunks: Vec::new(), hub_r: f64::MAX, site_r: f64::MAX, classes: Default::default(), live: Mutex::new(Vec::new()) };
        let mut rng = Rng::new(77_041);
        let sites = w.sites.len().max(1);
        // What a hub holds: 12 sites of an access node and what is behind it.
        const SITES_PER_HUB: usize = 12;
        let per_metro = size / sites;
        let behind = (per_metro / SITES_PER_HUB).saturating_sub(3).clamp(3, 40);
        let per_hub = 4 + SITES_PER_HUB * (2 + behind * 5 / 6);
        let hubs = (per_metro as f64 / per_hub as f64).round().max(1.0) as usize;

        // Regions in a row, west to east, each a disc that holds its metros.
        let mut regions: Vec<(&str, Vec<usize>)> = Vec::new();
        for (i, s) in w.sites.iter().enumerate() {
            match regions.iter_mut().find(|r| r.0 == s.region) {
                Some(r) => r.1.push(i),
                None => regions.push((s.region, vec![i])),
            }
        }
        let lon = |r: &(&str, Vec<usize>)| r.1.iter().map(|&i| w.sites[i].lon).sum::<f64>() / r.1.len() as f64;
        regions.sort_by(|a, b| lon(a).total_cmp(&lon(b)));
        // Places are boxes, one inside the other, each with a strip along
        // its top for its name. Metros are all of a size; a region is as
        // wide as its metros side by side; regions fill two rows.
        const METRO: f64 = 100.0;
        const GAP: f64 = 8.0;
        const HEAD: f64 = 12.0;
        let width = |k: usize| k as f64 * METRO + (k as f64 + 1.0) * GAP;
        let region_h = METRO + HEAD + 2.0 * GAP;
        let half: f64 = regions.iter().map(|r| width(r.1.len()) + 3.0 * GAP).sum::<f64>() / 2.0;
        let mut place: Vec<(f64, f64)> = Vec::new();
        let (mut left, mut row) = (0.0, 0usize);
        for r in &regions {
            let w = width(r.1.len());
            if row == 0 && left > 0.0 && left + w / 2.0 > half {
                row = 1;
                left = 0.0;
            }
            place.push((left + w / 2.0, row as f64 * (region_h + 3.0 * GAP)));
            left += w + 3.0 * GAP;
        }
        let mut core_at: HashMap<usize, u32> = HashMap::new();
        for (ri, (rname, members)) in regions.iter().enumerate() {
            let (rx, ry) = place[ri];
            let region = e.add(Node::container(format!("r-{}", rname.to_lowercase()), "region", 0, rname.to_string(), [NONE; 4], rx, ry, width(members.len()), region_h));
            let k = members.len();
            let rm = METRO;
            for (mi, &si) in members.iter().enumerate() {
                let site = &w.sites[si];
                let (mx, my) = (rx - width(k) / 2.0 + GAP + METRO / 2.0 + mi as f64 * (METRO + GAP), ry + HEAD / 2.0);
                let metro = e.add(Node::container(format!("m-{}", site.id), "metro", 1, site.name.to_string(), [region, NONE, NONE, NONE], mx, my, rm, rm));
                // The site's own devices, in a ring at the centre.
                let devs: Vec<usize> = w.devices.iter().enumerate().filter(|(_, d)| d.site == site.id).map(|(i, _)| i).collect();
                for (j, &di) in devs.iter().enumerate() {
                    let d = &w.devices[di];
                    let (cx, cy) = (mx - rm / 2.0 + rm * (j as f64 + 0.5) / devs.len() as f64, my - rm / 2.0 + 0.16 * rm);
                    let i = e.add(Node { id: d.id.clone(), kind: d.role, layer: "core", tier: 3, name: d.id.clone(), anc: [region, metro, NONE, NONE], x: cx, y: cy, r: 0.0, w: 0.0, h: 0.0, vendor: d.vendor, model: d.model, version: d.os_version.clone(), customer: String::new(), base_ms: round(rng.range(0.2, 0.9), 2), children: 0 });
                    core_at.insert(di, i);
                }
                let mut aggs: Vec<u32> = devs.iter().filter(|&&i| w.devices[i].role == "agg").map(|i| core_at[i]).collect();
                if aggs.is_empty() {
                    aggs = devs.iter().map(|i| core_at[i]).collect();
                }
                let n_hubs = ((hubs as f64) * (0.5 + 0.7 * site.weight)).round().max(1.0) as usize;
                // Hubs in a grid under the core's row.
                let (area_w, area_h, top) = (0.94 * rm, 0.74 * rm, my - rm / 2.0 + 0.23 * rm);
                let cols = ((n_hubs as f64 * area_w / area_h).sqrt().ceil() as usize).max(1);
                let rows = n_hubs.div_ceil(cols);
                let (cw, ch) = (area_w / cols as f64, area_h / rows as f64);
                let (hw, hh) = (cw * 0.9, ch * 0.9);
                e.hub_r = e.hub_r.min(hw.min(hh) / 2.0);
                for hi in 0..n_hubs {
                    let (hx, hy) = (mx - area_w / 2.0 + cw * ((hi % cols) as f64 + 0.5), top + ch * ((hi / cols) as f64 + 0.5));
                    let tag = format!("h{:02}-{}", hi + 1, site.id);
                    let hub = e.add(Node::container(tag.clone(), "hub", 2, format!("{} hub {}", site.name, hi + 1), [region, metro, NONE, NONE], hx, hy, hw, hh));
                    let anc = [region, metro, hub, NONE];
                    let mut routers = Vec::new();
                    for n in 0..2 {
                        let (vendor, model, versions) = gear("hub-router", hi);
                        let i = e.add(Node { id: format!("hr{}-{tag}", n + 1), kind: "hub-router", layer: "aggregation", tier: 3, name: format!("hr{}-{tag}", n + 1), anc, x: hx + (n as f64 - 0.5) * 0.44 * hw, y: hy - hh / 2.0 + 0.2 * hh, r: 0.0, w: 0.0, h: 0.0, vendor, model, version: versions[(hi * 3 + n) % versions.len()].to_string(), customer: String::new(), base_ms: round(rng.range(0.8, 2.6), 2), children: 0 });
                        for &up in &aggs {
                            e.edges.push((up, i, "uplink"));
                        }
                        routers.push(i);
                    }
                    let (vendor, model, versions) = gear("power", hi);
                    let pw = e.add(Node { id: format!("pw-{tag}"), kind: "power", layer: "power", tier: 3, name: format!("Power plant, {} hub {}", site.name, hi + 1), anc, x: hx, y: hy - hh / 2.0 + 0.2 * hh, r: 0.0, w: 0.0, h: 0.0, vendor, model, version: versions[0].to_string(), customer: String::new(), base_ms: 0.0, children: 0 });
                    for &r in &routers {
                        e.edges.push((pw, r, "power"));
                    }
                    // Its sites, four across, under the routers' row.
                    let (sw_cell, sh_cell, s_top) = (0.94 * hw / 4.0, 0.68 * hh / (SITES_PER_HUB as f64 / 4.0).ceil(), hy - hh / 2.0 + 0.29 * hh);
                    let (sw, sh) = (sw_cell * 0.9, sh_cell * 0.9);
                    e.site_r = e.site_r.min(sw.min(sh) / 2.0);
                    for s in 0..SITES_PER_HUB {
                        let (sx, sy) = (hx - 0.47 * hw + sw_cell * ((s % 4) as f64 + 0.5), s_top + sh_cell * ((s / 4) as f64 + 0.5));
                        let pick = rng.below(4);
                        let (kind, behind_kind, label, count) = match pick {
                            0 => ("cell", "radio", "Cell site", 3 + rng.below(4)),
                            1 => ("switch", "cpe", "Business park", (behind * 2 / 3).max(3) + rng.below(behind / 3 + 1)),
                            _ => ("olt", "ont", "Cabinet", (behind * 3 / 4).max(3) + rng.below(behind / 2 + 1)),
                        };
                        let stag = format!("s{:02}-{tag}", s + 1);
                        let asite = e.add(Node::container(stag.clone(), "site", 3, format!("{label} {}, {} hub {}", s + 1, site.name, hi + 1), anc, sx, sy, sw, sh));
                        let anc = [region, metro, hub, asite];
                        let (vendor, model, versions) = gear(kind, hi + s);
                        let node = e.add(Node { id: format!("{kind}-{stag}"), kind, layer: "access", tier: 3, name: format!("{kind}-{stag}"), anc, x: sx, y: if kind == "cell" { sy } else { sy - sh / 2.0 + 0.17 * sh }, r: 0.0, w: 0.0, h: 0.0, vendor, model, version: versions[(hi + s * 5) % versions.len()].to_string(), customer: String::new(), base_ms: round(rng.range(2.5, 7.5), 2), children: 0 });
                        e.edges.push((routers[s % 2], node, "access"));
                        if s % 3 == 0 {
                            e.edges.push((routers[(s + 1) % 2], node, "access"));
                        }
                        for b in 0..count {
                            // What is behind the access node, in a grid under it.
                            let (gw, gh) = (0.9 * sw, 0.6 * sh);
                            let gc = ((count as f64 * gw / gh).sqrt().ceil() as usize).max(1);
                            let gr = count.div_ceil(gc);
                            let (mut px, mut py) = (sx - gw / 2.0 + gw * ((b % gc) as f64 + 0.5) / gc as f64, sy - sh / 2.0 + 0.34 * sh + gh * ((b / gc) as f64 + 0.5) / gr as f64);
                            if behind_kind == "radio" {
                                // A cell site is a circle: its sectors stand around the tower.
                                let a = std::f64::consts::TAU * (b as f64 / count as f64) - std::f64::consts::FRAC_PI_2;
                                (px, py) = (sx + 0.33 * sw.min(sh) * a.cos(), sy + 0.33 * sw.min(sh) * a.sin());
                            }
                            let (vendor, model, versions) = gear(behind_kind, b + s);
                            let customer = if behind_kind == "cpe" { format!("{} {}", FIRMS_A[rng.below(FIRMS_A.len())], FIRMS_B[rng.below(FIRMS_B.len())]) } else { String::new() };
                            let id = format!("{behind_kind}-{:03}-{stag}", b + 1);
                            let name = match behind_kind {
                                "cpe" => format!("{customer}, customer router"),
                                "radio" => format!("Sector {}", b + 1),
                                _ => format!("ONT {}", b + 1),
                            };
                            let base = if behind_kind == "radio" { rng.range(9.0, 24.0) } else { rng.range(5.0, 16.0) };
                            let i = e.add(Node { id, kind: behind_kind, layer: "premises", tier: 4, name, anc, x: px, y: py, r: 0.0, w: 0.0, h: 0.0, vendor, model, version: versions[(b * 7 + s) % versions.len()].to_string(), customer, base_ms: round(base, 2), children: 0 });
                            e.edges.push((node, i, "drop"));
                        }
                    }
                }
            }
        }
        // The backbone itself, and how many of its links join each pair of
        // metros (what is drawn before single links are).
        let mut trunks: HashMap<(String, String), usize> = HashMap::new();
        for l in w.links.iter().filter(|l| l.documented) {
            let (Some(&a), Some(&b)) = (core_at.get(&l.a), core_at.get(&l.b)) else { continue };
            e.edges.push((a, b, if l.kind == "backbone" { "backbone" } else { "core" }));
            let (ma, mb) = (e.nodes[e.nodes[a as usize].anc[1] as usize].id.clone(), e.nodes[e.nodes[b as usize].anc[1] as usize].id.clone());
            if ma != mb {
                *trunks.entry(if ma < mb { (ma, mb) } else { (mb, ma) }).or_default() += 1;
            }
        }
        e.trunks = trunks.into_iter().map(|((a, b), n)| (a, b, n)).collect();
        e.trunks.sort();
        e.down = vec![Vec::new(); e.nodes.len()];
        e.up = vec![Vec::new(); e.nodes.len()];
        for &(a, b, _) in &e.edges {
            e.down[a as usize].push(b);
            e.up[b as usize].push(a);
        }
        for (i, n) in e.nodes.iter().enumerate() {
            let class = match n.layer {
                "premises" => 0,
                "access" => 1,
                "aggregation" => 2,
                "power" => 3,
                _ => continue,
            };
            e.classes[class].push(i as u32);
        }
        e
    }

    fn add(&mut self, n: Node) -> u32 {
        let i = self.nodes.len() as u32;
        if n.layer != "container" {
            for a in n.anc {
                if a != NONE {
                    self.nodes[a as usize].children += 1;
                }
            }
        }
        self.index.insert(n.id.clone(), i);
        self.nodes.push(n);
        i
    }

    pub fn find(&self, id: &str) -> Option<u32> {
        self.index.get(id).copied()
    }

    /// The objects that are not containers.
    pub fn objects(&self) -> usize {
        self.nodes.iter().filter(|n| n.layer != "container").count()
    }

    fn anc_ids(&self, i: u32) -> [&str; 4] {
        let n = &self.nodes[i as usize];
        let mut out = [""; 4];
        for (k, a) in n.anc.iter().enumerate() {
            if *a != NONE {
                out[k] = &self.nodes[*a as usize].id;
            }
        }
        if n.layer == "container" {
            out[n.tier.min(3) as usize] = &n.id;
        }
        out
    }

    pub fn node_rows(&self, range: std::ops::Range<usize>, rng: &mut Rng) -> Vec<Value> {
        (range.start as u32..range.end as u32)
            .map(|i| {
                let n = &self.nodes[i as usize];
                let a = self.anc_ids(i);
                // A few are slow on an ordinary day.
                let ms = if n.base_ms > 0.0 && rng.chance(0.015) { round(n.base_ms * rng.range(2.5, 6.0), 2) } else { n.base_ms };
                json!({"id": n.id, "num": i, "kind": n.kind, "layer": n.layer, "tier": n.tier, "name": n.name, "region": a[0], "metro": a[1], "hub": a[2], "asite": a[3],
                    "x": round(n.x, 4), "y": round(n.y, 4), "r": round(n.r, 4), "w": round(n.w, 4), "h": round(n.h, 4), "vendor": n.vendor, "model": n.model, "os_version": n.version, "customer": n.customer,
                    "children": n.children, "state": "ok", "reason": "", "latency_ms": ms, "base_ms": n.base_ms, "loss": 0.0})
            })
            .collect()
    }

    pub fn edge_rows(&self, range: std::ops::Range<usize>) -> Vec<Value> {
        self.edges[range]
            .iter()
            .map(|&(a, b, kind)| {
                let (na, nb) = (&self.nodes[a as usize], &self.nodes[b as usize]);
                json!({"id": format!("{}>{}", na.id, nb.id), "src": na.id, "dst": nb.id, "kind": kind, "tier": na.tier.max(nb.tier),
                    "x1": round(na.x, 4), "y1": round(na.y, 4), "x2": round(nb.x, 4), "y2": round(nb.y, 4),
                    "minx": round(na.x.min(nb.x), 4), "maxx": round(na.x.max(nb.x), 4), "miny": round(na.y.min(nb.y), 4), "maxy": round(na.y.max(nb.y), 4)})
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn event(&self, id: String, ts: i64, node: u32, event: &str, severity: &str, cause: &str, text: &str, fault: &str, author: &str, minutes: f64) -> Value {
        let n = &self.nodes[node as usize];
        let a = self.anc_ids(node);
        json!({"id": id, "ts": iso(ts), "node": n.id, "kind": n.kind, "layer": n.layer, "region": a[0], "metro": a[1], "hub": a[2], "asite": a[3], "x": round(n.x, 4), "y": round(n.y, 4),
            "event": event, "severity": severity, "cause": cause, "text": text, "fault": fault, "author": author, "ticket": if event == "change" { format!("CHG-E{:05}", id.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 100_000) } else { String::new() }, "minutes": minutes})
    }

    fn fill(&self, text: &str, node: u32, rng: &mut Rng) -> String {
        let (_, _, versions) = gear(self.nodes[node as usize].kind, rng.below(3));
        text.replace("{p}", &(1 + rng.below(16)).to_string()).replace("{v}", versions[rng.below(versions.len())]).replace("{n}", &(10 + rng.below(80)).to_string())
    }

    /// An object for something to happen to. Over two months it happens to
    /// each about as often as there are of its kind (what carries more a
    /// little more often); the faults kept going now are spread over the
    /// layers, so that there is something to see at every zoom.
    fn pick(&self, rng: &mut Rng, now: bool) -> u32 {
        let class = if now {
            match rng.below(100) {
                0..=44 => 0,
                45..=81 => 1,
                82..=95 => 2,
                _ => 3,
            }
        } else {
            let weights = [self.classes[0].len(), self.classes[1].len() * 4, self.classes[2].len() * 3, self.classes[3].len() * 2];
            let mut at = rng.below(weights.iter().sum::<usize>().max(1));
            let mut class = 0;
            for (k, w) in weights.iter().enumerate() {
                if at < *w {
                    class = k;
                    break;
                }
                at -= w;
            }
            class
        };
        let list = if self.classes[class].is_empty() { &self.classes[1] } else { &self.classes[class] };
        list[rng.below(list.len())]
    }

    /// A fault on `root` at `at`: the change before it when its cause is
    /// one (on the object, or on what it hangs from), and the fault.
    fn fault_rows(&self, id: &str, root: u32, at: i64, rng: &mut Rng) -> (Vec<Value>, &'static Template, String) {
        let list = templates(self.nodes[root as usize].kind);
        let tpl = &list[rng.below(list.len())];
        let mut rows = Vec::new();
        if let Some(change) = tpl.change {
            let ups = &self.up[root as usize];
            let on = if self.nodes[root as usize].layer == "access" && !ups.is_empty() && rng.chance(0.35) { ups[0] } else { root };
            let text = self.fill(change, on, rng);
            rows.push(self.event(format!("{id}-c"), at - 60 * (4 + rng.below(25) as i64), on, "change", "info", tpl.cause, &text, id, OPERATORS[rng.below(OPERATORS.len())], 0.0));
        }
        let text = self.fill(tpl.text, root, rng);
        rows.push(self.event(format!("{id}-f"), at, root, "fault", tpl.severity, tpl.cause, &text, id, "", 0.0));
        (rows, tpl, text)
    }

    /// Two months of faults (each with its clearing, and the change that
    /// caused it when one did) and of ordinary changes: about three rows
    /// for each object. `range` is a part of them (of `0..objects()`), so
    /// that they are made and written a part at a time.
    pub fn history(&self, now: i64, range: std::ops::Range<usize>) -> Vec<Value> {
        let span = HISTORY_DAYS * 86_400;
        let mut rows = Vec::with_capacity(range.len() * 3);
        for k in range {
            let mut rng = Rng::keyed("estate-history", k as u64);
            let root = self.pick(&mut rng, false);
            let at = now - 1800 - (rng.unit() * (span - 3600) as f64) as i64;
            let id = format!("F{k:07}");
            let (mut r, tpl, text) = self.fault_rows(&id, root, at, &mut rng);
            rows.append(&mut r);
            let minutes = match tpl.state {
                "down" => 15.0 + rng.unit() * 220.0,
                _ => 6.0 + rng.unit() * 90.0,
            };
            if at + (minutes * 60.0) as i64 <= now - 300 {
                rows.push(self.event(format!("{id}-k"), at + (minutes * 60.0) as i64, root, "clear", "info", tpl.cause, &text, &id, "", round(minutes, 1)));
            }
            if k % 2 == 0 {
                let node = self.pick(&mut rng, false);
                let (cause, text) = ROUTINE[rng.below(ROUTINE.len())];
                let text = self.fill(text, node, &mut rng);
                let at = now - 600 - (rng.unit() * (span - 1200) as f64) as i64;
                rows.push(self.event(format!("C{k:07}"), at, node, "change", "info", cause, &text, "", OPERATORS[rng.below(OPERATORS.len())], 0.0));
            }
        }
        rows
    }

    /// What depends on `i` (never through the backbone), nearest first.
    pub fn below(&self, i: u32, cap: usize) -> Vec<u32> {
        self.walk(i, 8, cap, true, false)
    }

    pub fn walk(&self, from: u32, hops: usize, cap: usize, down: bool, up: bool) -> Vec<u32> {
        self.walk_hops(from, hops, cap, down, up).into_iter().map(|(i, _)| i).collect()
    }

    /// The rows within `hops` of `from` along the chosen directions, with
    /// their distance. As the queries do, a walk passes through no device
    /// of the backbone other than the one it starts from.
    pub fn walk_hops(&self, from: u32, hops: usize, cap: usize, down: bool, up: bool) -> Vec<(u32, usize)> {
        let mut seen: HashSet<u32> = HashSet::from([from]);
        let mut queue = VecDeque::from([(from, 0usize)]);
        let mut out = Vec::new();
        while let Some((i, d)) = queue.pop_front() {
            if d == hops || (i != from && self.nodes[i as usize].layer == "core") {
                continue;
            }
            let next = (if down { self.down[i as usize].as_slice() } else { &[] }).iter().chain(if up { self.up[i as usize].iter() } else { [].iter() });
            for &j in next {
                if seen.insert(j) {
                    out.push((j, d + 1));
                    if out.len() >= cap {
                        return out;
                    }
                    queue.push_back((j, d + 1));
                }
            }
        }
        out
    }
}

impl Node {
    #[allow(clippy::too_many_arguments)]
    fn container(id: String, kind: &'static str, tier: u8, name: String, anc: [u32; 4], x: f64, y: f64, w: f64, h: f64) -> Node {
        Node { id, kind, layer: "container", tier, name, anc, x, y, r: w.min(h) / 2.0, w, h, vendor: "", model: "", version: String::new(), customer: String::new(), base_ms: 0.0, children: 0 }
    }
}

fn quoted(ids: impl IntoIterator<Item = impl AsRef<str>>) -> String {
    let list: Vec<String> = ids.into_iter().map(|s| format!("'{}'", s.as_ref().replace('\'', ""))).collect();
    if list.is_empty() { "''".into() } else { list.join(", ") }
}

// ---- writing it, and keeping a few faults going

/// Makes the estate's rows when they are not there (or were made for
/// another size), and puts back to `ok` what a stopped app left broken.
pub async fn ensure(e: &Engine) -> Result<()> {
    let Some(est) = &e.estate else { return Ok(()) };
    let want = json!({"nodes": est.nodes.len(), "edges": est.edges.len(), "layout": "zones"}).to_string();
    let have = e.db.query("SELECT val FROM netops_state WHERE id = 'estate'", json!({})).await?;
    if have.first().map(|r| r.s("val")) == Some(want.clone()) {
        e.db.sql(&format!("UPDATE {NODES} SET state = 'ok', reason = '', latency_ms = base_ms, loss = 0 WHERE state != 'ok'")).await?;
        return minute(e, crate::sim::minute_now()).await;
    }
    let now = crate::sim::minute_now();
    if !have.is_empty() {
        for (name, ddl, _) in crate::schema::tables().into_iter().filter(|t| t.0.starts_with("netops_estate_")) {
            e.db.sql(&format!("DROP TABLE IF EXISTS {name}")).await?;
            e.db.sql(&ddl).await?;
        }
    }
    let objects = est.objects();
    let total = (est.nodes.len() + est.edges.len() + objects) as f64;
    let mut done = 0usize;
    let mut rng = Rng::new(31);
    const PART: usize = 5000;
    // Said as the map says it: the objects, without the places that hold them.
    let places = est.nodes.len() - objects;
    let objects_text = format!("{} objects and {} places", thousands(objects), thousands(places));
    for (what, n) in [("objects", est.nodes.len()), ("links", est.edges.len()), ("two months of faults and changes", objects)] {
        let mut from = 0;
        while from < n {
            let to = (from + PART).min(n);
            let text = match what {
                "objects" => objects_text.clone(),
                "links" => format!("{} links", thousands(n)),
                _ => format!("two months of faults and changes on {} objects", thousands(objects)),
            };
            e.phase(&format!("Writing the estate: {text}"), 0.97 * done as f64 / total);
            match what {
                "objects" => e.db.upsert(NODES, est.node_rows(from..to, &mut rng)).await?,
                "links" => e.db.upsert(EDGES, est.edge_rows(from..to)).await?,
                _ => e.db.upsert(EVENTS, est.history(now, from..to)).await?,
            };
            done += to - from;
            from = to;
        }
    }
    e.db.upsert("netops_state", vec![json!({"id": "estate", "val": want, "updated": iso(now)})]).await?;
    minute(e, now).await
}

/// Each minute: faults whose time is up clear, and a new one may start.
pub async fn minute(e: &Engine, m: i64) -> Result<()> {
    let Some(est) = &e.estate else { return Ok(()) };
    let mut rng = Rng::keyed("estate", m as u64);
    let target = (est.objects() / 2500).clamp(4, 40);
    let (over, n_live): (Vec<Live>, usize) = {
        let mut live = est.live.lock().unwrap_or_else(|p| p.into_inner());
        let (over, keep): (Vec<Live>, Vec<Live>) = live.drain(..).partition(|f| f.until <= m);
        *live = keep;
        (over, live.len())
    };
    let mut events = Vec::new();
    for f in &over {
        let ids: Vec<&str> = std::iter::once(f.root).chain(f.hit.iter().copied()).map(|i| est.nodes[i as usize].id.as_str()).collect();
        e.db.sql(&format!("UPDATE {NODES} SET state = 'ok', reason = '', latency_ms = base_ms, loss = 0 WHERE id IN ({})", quoted(ids))).await?;
        events.push(est.event(format!("{}-k", f.id), m, f.root, "clear", "info", f.cause, &f.text, &f.id, "", round((m - f.started) as f64 / 60.0, 1)));
    }
    let mut started = Vec::new();
    let mut n = n_live;
    while n < target && (n < target / 2 || rng.chance(0.3)) {
        let busy: HashSet<u32> = est.live.lock().unwrap_or_else(|p| p.into_inner()).iter().flat_map(|f| std::iter::once(f.root).chain(f.hit.iter().copied())).chain(started.iter().flat_map(|f: &Live| std::iter::once(f.root).chain(f.hit.iter().copied()))).collect();
        let root = est.pick(&mut rng, true);
        n += 1;
        if busy.contains(&root) {
            continue;
        }
        let id = format!("L{}-{}", m / 60, e.next_id());
        let (mut rows, tpl, text) = est.fault_rows(&id, root, m, &mut rng);
        events.append(&mut rows);
        let below: Vec<u32> = est.down[root as usize].iter().copied().filter(|i| !busy.contains(i) && est.nodes[*i as usize].layer != "core").collect();
        let hit: Vec<u32> = if tpl.state == "down" { below } else { below.into_iter().filter(|_| rng.chance(0.4)).collect() };
        let node = &est.nodes[root as usize];
        let (ms, loss) = if tpl.state == "down" { (0.0, 100.0) } else { (round(node.base_ms.max(1.0) * rng.range(3.0, 9.0), 2), round(rng.range(0.4, 6.0), 2)) };
        e.db.sql(&format!("UPDATE {NODES} SET state = '{}', reason = '{}', since = '{}', latency_ms = {ms}, loss = {loss} WHERE id = '{}'", tpl.state, text.replace('\'', ""), iso(m), node.id)).await?;
        if !hit.is_empty() {
            let (factor, loss) = if tpl.state == "down" { (0.0, 100.0) } else { (round(rng.range(2.0, 5.0), 1), round(rng.range(0.2, 3.0), 2)) };
            e.db.sql(&format!("UPDATE {NODES} SET state = 'impacted', reason = 'Behind {}: {}', since = '{}', latency_ms = base_ms * {factor}, loss = {loss} WHERE id IN ({})", node.id, text.replace('\'', ""), iso(m), quoted(hit.iter().map(|i| &est.nodes[*i as usize].id)))).await?;
        }
        let minutes = if tpl.state == "down" { 12 + rng.below(50) } else { 6 + rng.below(30) } as i64;
        started.push(Live { id, root, hit, started: m, until: m + minutes * 60, text, cause: tpl.cause });
    }
    if !events.is_empty() {
        e.db.upsert(EVENTS, events).await?;
    }
    est.live.lock().unwrap_or_else(|p| p.into_inner()).extend(started);
    Ok(())
}

// ---- outages in the estate, as the shift sees them

/// The hub an object of the estate sits in (its own id for a hub), which
/// is what its alerts are grouped by: from the estate this app made, or
/// read off the id (`olt-s03-h12-lon` is in `h12-lon`).
pub fn hub_of(est: Option<&Estate>, id: &str) -> Option<String> {
    if let Some(est) = est {
        if let Some(i) = est.find(id) {
            let n = &est.nodes[i as usize];
            let a = n.anc[2];
            if a != NONE {
                return Some(est.nodes[a as usize].id.clone());
            }
            if n.kind == "hub" {
                return Some(n.id.clone());
            }
            return None;
        }
    }
    let parts: Vec<&str> = id.split('-').collect();
    let k = parts.iter().position(|p| p.len() == 3 && p.starts_with('h') && p[1..].chars().all(|c| c.is_ascii_digit()))?;
    (k + 1 < parts.len()).then(|| parts[k..].join("-"))
}

const LAYER_RANK: &[&str] = &["power", "aggregation", "access", "premises"];

/// The estate's agent: an incident whose alerts are on objects behind the
/// backbone. The object says what is wrong with it (its `reason`); what is
/// left to find is how far it reaches (a walk along the edges, and the
/// customers on what it reaches) and whether a change came before it, on it
/// or on what it hangs from. Every step is a query, kept as the other
/// agents keep theirs.
pub async fn diagnose(e: &Engine, incident: &str, m: i64) -> Result<()> {
    use crate::agents::{quoted, Run};
    if !e.st().open.contains_key(incident) {
        return Ok(());
    }
    let mut run = Run::new(e, "estate", incident, incident, m);
    let alerts = run.q("Read the incident's alerts", "SELECT entity, severity, reading, summary, started FROM netops_alerts\nWHERE incident = :id AND status IN ('firing', 'silenced')\nORDER BY reading DESC LIMIT 50", json!({"id": incident})).await;
    let mut ids: Vec<String> = alerts.iter().map(|a| a.s("entity")).collect();
    ids.dedup();
    run.finding(format!("{} alert{} on {}.", alerts.len(), if alerts.len() == 1 { "" } else { "s" }, ids.join(", ")));
    if ids.is_empty() {
        run.think("Conclude", "Nothing is firing: the objects came back before the agent looked.");
        run.finish("done", "Nothing firing", 0.0).await?;
        return Ok(());
    }
    let objs = run.q("What the objects report now", &format!("SELECT id, kind, layer, name, state, reason, since, hub, metro, region, vendor, model, os_version FROM {NODES}\nWHERE id IN ({}) LIMIT 50", quoted(&ids)), json!({})).await;
    // The root is one that is down before one that is only degraded (a
    // slow router does not take down what it feeds); among those, the one
    // highest up: a hub's power before its routers, a router before what it
    // feeds; then the one with the most behind it.
    let rank = |r: &Row| LAYER_RANK.iter().position(|l| *l == r.s("layer")).unwrap_or(9);
    let reading = |r: &Row| alerts.iter().find(|a| a.s("entity") == r.s("id")).map_or(0.0, |a| a.f("reading"));
    let mut sorted: Vec<&Row> = objs.iter().collect();
    sorted.sort_by(|a, b| (a.s("state") != "down").cmp(&(b.s("state") != "down")).then(rank(a).cmp(&rank(b))).then(reading(b).total_cmp(&reading(a))));
    let Some(root) = sorted.first().map(|r| (*r).clone()) else {
        run.think("Conclude", "None of the alerting objects is in the estate's tables.");
        run.finish("done", "The objects were not found", 0.0).await?;
        return Ok(());
    };
    let rid = root.s("id");
    run.finding(format!("{rid} ({}, {}) is {}: {}", root.s("kind"), root.s("layer"), root.s("state"), root.s("reason")));

    // How far it reaches.
    let seeds = if ids.len() == 1 { quoted(&ids) } else { format!("({})", quoted(&ids)) };
    let sql = format!("SELECT kind, state, customer, COUNT(*) AS n FROM {NODES}\nWHERE REACHABLE({EDGES}, {seeds}, hops => 3, dir => 'out', through => layer != 'core')\nGROUP BY kind, state, customer LIMIT 2000");
    let walked = || -> Vec<Row> {
        let Some(est) = &e.estate else { return Vec::new() };
        let hit: HashSet<u32> = est.live.lock().unwrap_or_else(|p| p.into_inner()).iter().flat_map(|f| std::iter::once(f.root).chain(f.hit.iter().copied())).collect();
        let mut by: HashMap<(String, String, String), i64> = HashMap::new();
        let mut seen = HashSet::new();
        for id in &ids {
            let Some(i) = est.find(id) else { continue };
            for j in est.walk(i, 3, 20_000, true, false) {
                if seen.insert(j) {
                    let n = &est.nodes[j as usize];
                    *by.entry((n.kind.to_string(), if hit.contains(&j) { "impacted".into() } else { "ok".into() }, n.customer.clone())).or_default() += 1;
                }
            }
        }
        by.into_iter().map(|((kind, state, customer), n)| json!({"kind": kind, "state": state, "customer": customer, "n": n}).as_object().cloned().unwrap_or_default()).collect()
    };
    let behind = run.graph("What depends on them: a walk along the edges, by kind, state and customer", sql, walked).await;
    let total: i64 = behind.iter().map(|r| r.i("n")).sum();
    let hit: i64 = behind.iter().filter(|r| r.s("state") != "ok").map(|r| r.i("n")).sum();
    let mut firms: Vec<(String, i64)> = Vec::new();
    for r in behind.iter().filter(|r| !r.s("customer").is_empty()) {
        match firms.iter_mut().find(|f| f.0 == r.s("customer")) {
            Some(f) => f.1 += r.i("n"),
            None => firms.push((r.s("customer"), r.i("n"))),
        }
    }
    firms.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    run.finding(format!("{total} objects depend on them, {hit} of them not ok now; {} business customer{} behind them{}.", firms.len(), if firms.len() == 1 { "" } else { "s" },
        if firms.is_empty() { String::new() } else { format!(": {}", firms.iter().take(5).map(|f| f.0.clone()).collect::<Vec<_>>().join(", ")) }));

    // What came before it: a change on it or on what it hangs from.
    let mut near: Vec<String> = ids.clone();
    if let Some(est) = &e.estate {
        if let Some(i) = est.find(&rid) {
            near.extend(est.up[i as usize].iter().map(|j| est.nodes[*j as usize].id.clone()).filter(|x| !x.contains("--")));
        }
    }
    near.sort();
    near.dedup();
    let events = run.q("Changes and faults on them, and on what they hang from, in the last two hours", &format!("SELECT ts, node, event, cause, text, author, ticket FROM {EVENTS}\nWHERE node IN ({}) AND ts >= :since\nORDER BY ts DESC LIMIT 30", quoted(&near)), json!({"since": iso(m - 7200)})).await;
    let fault = events.iter().find(|ev| ev.s("event") == "fault" && ev.s("node") == rid);
    let change = events.iter().find(|ev| ev.s("event") == "change" && fault.is_none_or(|f| ev.s("ts") <= f.s("ts")));
    run.finding(match change {
        Some(c) => format!("{} on {} at {} by {}: {}.", c.s("ticket"), c.s("node"), &c.s("ts").get(11..16).unwrap_or(""), c.s("author"), c.s("text")),
        None => "No change on them or on what they hang from in the last two hours.".into(),
    });
    let cause = fault.map(|f| f.s("cause")).filter(|c| !c.is_empty()).unwrap_or_else(|| "unknown".into());

    // Who is on call.
    let oncall = run.q("Who is on call", "SELECT team, person FROM netops_oncall\nWHERE role = 'primary' AND starts <= :now AND ends > :now LIMIT 20", json!({"now": iso(m)})).await;
    let person = oncall.iter().find(|o| o.s("team") == "Backbone NOC").or(oncall.first()).map(|o| o.s("person")).unwrap_or_else(|| "the NOC".into());
    run.finding(format!("{person} is primary."));

    let down = root.s("state") == "down";
    let reason = root.s("reason");
    let title = format!("{} {rid}: {}", if down { "Outage at" } else { "Degraded:" }, reason.split(':').next().unwrap_or(&reason).trim());
    let mut text = format!("{rid} reports: {}.", reason.trim_end_matches('.'));
    if let Some(c) = change {
        text += &format!(" It follows {} on {} by {} ({}).", c.s("ticket"), c.s("node"), c.s("author"), c.s("text"));
    }
    let severity = if hit >= 100 || firms.len() >= 5 { "sev2" } else { "sev3" };
    let (who, them) = if ids.len() == 1 { (format!("{rid} alerts"), "it") } else { (format!("{} objects of the estate alert", ids.len()), "them") };
    let summary = format!("{who} in {}: {total} object{} depend on {them}, {hit} not ok now{}.", root.s("hub"), if total == 1 { "" } else { "s" },
        if firms.is_empty() { String::new() } else { format!(", with {} business customer{} behind {them} ({}{})", firms.len(), if firms.len() == 1 { "" } else { "s" }, firms.iter().take(3).map(|f| f.0.clone()).collect::<Vec<_>>().join(", "), if firms.len() > 3 { " and others" } else { "" }) });
    run.think("Conclude", format!("{text} {summary}"));
    {
        let mut st = e.st();
        if let Some(o) = st.open.get_mut(incident) {
            for (k, v) in [("title", json!(title)), ("summary", json!(summary)), ("root_kind", json!("estate")), ("root_entity", json!(rid)), ("root_cause", json!(text)), ("confidence", json!(0.9)),
                ("category", json!("estate")), ("runbook", json!("rb-estate-outage")), ("severity", json!(severity)), ("commander", json!(person)), ("customers", json!(firms.len()))] {
                o.row.insert(k.into(), v);
            }
            if o.row.s("status") == "investigating" {
                o.row.insert("status".into(), json!("identified"));
            }
        }
    }
    e.save_incident(incident).await?;
    e.timeline(incident, m, "agent", "Estate agent", &format!("{text} {summary} Paged {person}."), &run.id).await?;
    // What a person can do from here: send someone, with what was found.
    let action = format!("{incident}:ticket:{}", rid.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', ""));
    let have = e.db.query("SELECT id FROM netops_actions WHERE id = :id", json!({"id": action})).await?;
    if have.is_empty() {
        e.db.upsert("netops_actions", vec![json!({"id": action, "run": run.id, "incident": incident, "kind": "ticket", "title": format!("Dispatch field services to {rid}"),
            "detail": format!("{text} {total} objects depend on it{}. Our reference: {incident}.", if firms.is_empty() { String::new() } else { format!(", among them {}", firms.iter().take(5).map(|f| f.0.clone()).collect::<Vec<_>>().join(", ")) }),
            "command": "", "target": rid, "risk": "low", "status": "proposed", "proposed": iso(crate::sim::now()), "decided_by": "", "result": ""})]).await?;
    }
    run.finish("done", &format!("{title} ({cause})"), 0.9).await?;
    Ok(())
}

/// A count with its thousands separated: 170,337.
pub fn thousands(n: usize) -> String {
    let d = n.to_string();
    let mut out = String::new();
    for (k, c) in d.chars().enumerate() {
        if k > 0 && (d.len() - k) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ---- walking where the server does not (a coordinator)

/// The most keys one statement names in `id IN (...)`: about 10,000
/// characters of ids.
pub const MAX_KEYS: usize = 400;
/// Up to this many keys a walk is read by key, `MAX_KEYS` at a time, the
/// batches at once; a larger one is read by the places that hold it.
pub const BY_KEY: usize = 4 * MAX_KEYS;
/// The most rows a walk's places are read for, a page at a time.
pub const PLACE_ROWS: usize = 50_000;
/// The rows of one page: as many as a query returns.
const PAGE: usize = 10_000;
/// The most places a condition on them names.
const MAX_PLACES: usize = 60;

/// The rows a walk found, read without a graph statement.
pub struct Among {
    pub rows: Vec<Row>,
    /// How the rows were read, for the trace's reader.
    pub how: &'static str,
    /// Whether the read stopped at `PLACE_ROWS`, so that some are missing.
    pub cut: bool,
}

impl Estate {
    /// A condition on the places that hold every one of `set`: their hubs,
    /// else their metros, else their regions, the first of those lists that
    /// is short. `None` when none is (or the set is empty).
    pub fn places_of(&self, set: &[u32]) -> Option<String> {
        if set.is_empty() {
            return None;
        }
        for (level, col) in [(2, "hub"), (1, "metro"), (0, "region")] {
            let mut names: Vec<&str> = Vec::new();
            let mut covered = true;
            for &i in set {
                let id = self.anc_ids(i)[level];
                if id.is_empty() {
                    covered = false;
                    break;
                }
                if !names.contains(&id) {
                    names.push(id);
                    if names.len() > MAX_PLACES {
                        covered = false;
                        break;
                    }
                }
            }
            if covered {
                names.sort_unstable();
                return Some(format!("{col} IN ({})", names.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ")));
            }
        }
        None
    }

    /// An object's region, metro, hub and site, as its row has them.
    pub fn places(&self, i: u32) -> [&str; 4] {
        self.anc_ids(i)
    }
}

/// The rows of `cols` (which name `id`) among the objects of `set` that
/// meet `cond`, read without a graph statement and without naming more
/// than [`MAX_KEYS`] keys in one: by key in batches when the set is small,
/// otherwise one plain query over the places that hold the set, whose rows
/// the app keeps when they are in the set.
pub(crate) async fn among(q: &Q, what: &str, est: &Estate, set: &[u32], cols: &str, cond: &str, params: Value) -> std::result::Result<Among, crate::api::ApiError> {
    let and = if cond.trim().is_empty() { String::new() } else { format!("\n  AND {cond}") };
    if set.is_empty() {
        return Ok(Among { rows: Vec::new(), how: "by key", cut: false });
    }
    if set.len() <= BY_KEY {
        let mut jobs = tokio::task::JoinSet::new();
        let batches: Vec<&[u32]> = set.chunks(MAX_KEYS).collect();
        let n = batches.len();
        for (k, batch) in batches.into_iter().enumerate() {
            let list = batch.iter().map(|&i| format!("'{}'", est.nodes[i as usize].id)).collect::<Vec<_>>().join(", ");
            let sql = format!("SELECT {cols} FROM {NODES}\nWHERE id IN ({list}){and}\nLIMIT {}", batch.len());
            let title = if n == 1 { format!("{what} (walked by the app, read by key)") } else { format!("{what} (walked by the app, read by key, {} of {n})", k + 1) };
            let (db, trace, params) = (q.e.db.clone(), q.trace.clone(), params.clone());
            jobs.spawn(async move { (k, db.traced(&trace, &title, &sql, params).await) });
        }
        let mut parts: Vec<(usize, Vec<Row>)> = Vec::new();
        while let Some(done) = jobs.join_next().await {
            let (k, r) = done.map_err(|e| bad(format!("a read stopped: {e}")))?;
            parts.push((k, r?));
        }
        parts.sort_by_key(|p| p.0);
        return Ok(Among { rows: parts.into_iter().flat_map(|p| p.1).collect(), how: "by key", cut: false });
    }
    let place = est.places_of(set);
    let wh = match &place {
        Some(p) => format!("{p}{and}"),
        None if and.is_empty() => "layer != 'container'".to_string(),
        None => cond.to_string(),
    };
    // Pages by the rows' number, as a query returns at most 10,000 rows.
    let sql = format!("SELECT {cols}, num FROM {NODES}\nWHERE {wh}\n  AND num > :after\nORDER BY num LIMIT {PAGE}");
    let inside: HashSet<u32> = set.iter().copied().collect();
    let (mut rows, mut read, mut cut, mut after) = (Vec::new(), 0, false, -1);
    for page in 1.. {
        let mut p = params.clone();
        p["after"] = json!(after);
        let got = q.rows(&format!("{what} (walked by the app: the rows of the places that hold the walk, kept here when the walk reached them{})", if page > 1 { format!(", page {page}") } else { String::new() }), &sql, p).await?;
        let n = got.len();
        read += n;
        after = got.last().map_or(after, |r| r.i("num"));
        rows.extend(got.into_iter().filter(|r| est.find(&r.s("id")).is_some_and(|i| inside.contains(&i))));
        if n < PAGE {
            break;
        }
        if read >= PLACE_ROWS {
            cut = true;
            break;
        }
    }
    Ok(Among { rows, how: "by place", cut })
}

// ---- the map

pub fn routes() -> Router<Shared> {
    Router::new().route("/v1/map/base", get(base)).route("/v1/map/hot", get(hot)).route("/v1/map/view", get(view)).route("/v1/map/node", get(node))
}

const NODE_COLS: &str = "id, kind, layer, tier, name, x, y, r, w, h, state, children, region, metro, hub, asite";

/// What does not change: the regions, metros and hubs, the trunks between
/// metros, and the sizes that say how far in each level of detail starts.
async fn base(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let containers = q.rows("Regions, metros and hubs", &format!("SELECT {NODE_COLS} FROM {NODES}\nWHERE tier <= 2 LIMIT 10000"), json!({})).await?;
    let counts = q.rows("How much there is", &format!("SELECT layer, kind, COUNT(*) AS n FROM {NODES}\nGROUP BY layer, kind LIMIT 100"), json!({})).await?;
    let links = q.rows("Links", &format!("SELECT COUNT(*) AS n FROM {EDGES}"), json!({})).await?;
    let events = q.rows("Faults and changes kept", &format!("SELECT event, COUNT(*) AS n FROM {EVENTS}\nGROUP BY event LIMIT 10"), json!({})).await?;
    let (mut hub_r, mut site_r) = (0.0, 0.0);
    let mut trunks = Vec::new();
    if let Some(est) = &q.e.estate {
        hub_r = est.hub_r;
        site_r = est.site_r;
        trunks = est.trunks.iter().map(|(a, b, n)| json!({"a": a, "b": b, "n": n})).collect();
    }
    if hub_r == 0.0 {
        hub_r = containers.iter().filter(|c| c.s("kind") == "hub").map(|c| c.f("r")).fold(f64::MAX, f64::min);
        let sites = q.rows("The size of a site", &format!("SELECT MIN(r) AS r FROM {NODES}\nWHERE kind = 'site'"), json!({})).await?;
        site_r = sites.first().map_or(0.0, |r| r.f("r"));
    }
    let core = q.rows("The backbone's devices", &format!("SELECT id, kind, x, y, metro FROM {NODES}\nWHERE layer = 'core' LIMIT 2000"), json!({})).await?;
    let objects: i64 = counts.iter().filter(|c| c.s("layer") != "container").map(|c| c.i("n")).sum();
    q.done(json!({"containers": containers, "trunks": trunks, "core": core, "hub_r": hub_r, "site_r": site_r, "counts": counts, "objects": objects, "links": links.first().map_or(0, |r| r.i("n")), "events": events, "operators": OPERATORS, "history_days": HISTORY_DAYS}))
}

/// What is wrong now, and where: the objects that are down or degraded,
/// how many are behind them by place, and the open incidents of the
/// backbone, each counted up through its hub, metro and region.
async fn hot(State(svc): State<Shared>) -> Answer {
    let q = Q::new(&svc);
    let faults = q.rows("What is down or degraded", &format!("SELECT id, kind, layer, name, state, reason, since, x, y, region, metro, hub, asite FROM {NODES}\nWHERE state IN ('down', 'degraded')\nORDER BY since DESC LIMIT 500"), json!({})).await?;
    let behind = q.rows("What is behind them, by place", &format!("SELECT region, metro, hub, asite, COUNT(*) AS n FROM {NODES}\nWHERE state = 'impacted'\nGROUP BY region, metro, hub, asite LIMIT 5000"), json!({})).await?;
    let mut places: Map<String, Value> = Map::new();
    let mut count = |ids: [String; 4], key: &str, n: i64| {
        for id in ids.into_iter().filter(|s| !s.is_empty()) {
            let p = places.entry(id).or_insert_with(|| json!({"down": 0, "degraded": 0, "impacted": 0, "incidents": 0, "weight": 0}));
            p[key] = json!(p[key].as_i64().unwrap_or(0) + n);
        }
    };
    let place = |r: &Row| [r.s("region"), r.s("metro"), r.s("hub"), r.s("asite")];
    // How much a broken thing weighs on the places that hold it: a hub's
    // router or power more than a cabinet, a cabinet more than one home.
    for f in &faults {
        count(place(f), &f.s("state"), 1);
        let down = f.s("state") == "down";
        count(place(f), "weight", match f.s("layer").as_str() {
            "aggregation" | "power" | "core" => 20,
            "access" if down => 6,
            "access" => 3,
            _ => 0,
        });
    }
    for b in &behind {
        count(place(b), "impacted", b.i("n"));
    }
    let open: Vec<Row> = q.e.st().open.values().map(|o| o.row.clone()).collect();
    let mut incidents = Vec::new();
    let mut devices: Map<String, Value> = Map::new();
    for inc in &open {
        let devs = inc.list("devices");
        let mut metros: Vec<String> = devs.iter().filter_map(|d| q.e.w.device(d)).map(|d| format!("m-{}", d.site)).collect();
        metros.sort();
        metros.dedup();
        for m in &metros {
            let region = q.e.w.sites.iter().find(|s| format!("m-{}", s.id) == *m).map(|s| format!("r-{}", s.region.to_lowercase())).unwrap_or_default();
            count([region.clone(), m.clone(), String::new(), String::new()], "incidents", 1);
            count([region, m.clone(), String::new(), String::new()], "weight", 100);
        }
        for d in &devs {
            devices.insert(d.clone(), json!(inc.s("id")));
        }
        incidents.push(json!({"id": inc.s("id"), "title": inc.s("title"), "severity": inc.s("severity"), "status": inc.s("status"), "started": inc.s("started"), "devices": devs, "metros": metros}));
    }
    q.done(json!({"at": iso(q.last()), "faults": faults, "places": places, "incidents": incidents, "devices": devices}))
}

#[derive(serde::Deserialize)]
struct ViewQ {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    t: i64,
}

/// What a window on the plane shows at a level of detail: two range
/// queries, one for the objects and one for the links that cross it.
async fn view(State(svc): State<Shared>, Query(v): Query<ViewQ>) -> Answer {
    let q = Q::new(&svc);
    let t = v.t.clamp(3, 4);
    let p = json!({"t": t, "x0": v.x0, "x1": v.x1, "y0": v.y0, "y1": v.y1});
    let nodes = q.rows("The objects in this window, at this level of detail", &format!("SELECT {NODE_COLS}, latency_ms FROM {NODES}\nWHERE tier >= 3 AND tier <= :t AND x >= :x0 AND x <= :x1 AND y >= :y0 AND y <= :y1\nLIMIT 9000"), p.clone()).await?;
    let edges = q.rows("The links that cross it", &format!("SELECT src, dst, kind, x1, y1, x2, y2 FROM {EDGES}\nWHERE tier <= :t AND maxx >= :x0 AND minx <= :x1 AND maxy >= :y0 AND miny <= :y1\nLIMIT 9000"), p).await?;
    q.done(json!({"t": t, "nodes": nodes, "edges": edges, "cut": nodes.len() >= 9000 || edges.len() >= 9000}))
}

#[derive(serde::Deserialize)]
struct NodeQ {
    id: String,
}

const SEEN: &str = "id, kind, layer, name, state, reason, since, x, y, latency_ms";

/// How many of the faults that read most like one are counted in how such
/// faults ended.
const ENDED_LIKE: usize = 100;

/// Cleared faults grouped by cause, the most frequent first: how many, how
/// long on average and at the longest.
fn ended_by_cause(rows: &[Row]) -> Vec<Value> {
    let mut by: Vec<(String, usize, f64, f64)> = Vec::new();
    for r in rows {
        let (cause, m) = (r.s("cause"), r.f("minutes"));
        match by.iter_mut().find(|b| b.0 == cause) {
            Some(b) => {
                b.1 += 1;
                b.2 += m;
                b.3 = b.3.max(m);
            }
            None => by.push((cause, 1, m, m)),
        }
    }
    by.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    by.into_iter().take(8).map(|(cause, n, sum, longest)| json!({"cause": cause, "n": n, "minutes": round(sum / n as f64, 1), "longest": longest})).collect()
}

/// One object: what is wrong with it, what it hangs from and where that
/// chain is broken, what depends on it, what else near it is in trouble,
/// what was changed on it and around it, and how this ended before.
async fn node(State(svc): State<Shared>, Query(a): Query<NodeQ>) -> Answer {
    let q = Q::new(&svc);
    let e = q.e.clone();
    let id = a.id.replace('\'', "");
    let found = q.rows("The object", &format!("SELECT * FROM {NODES}\nWHERE id = :id"), json!({"id": id})).await?;
    let Some(n) = found.first().cloned() else { return Err(bad(format!("no object {id}"))) };
    if n.s("layer") == "container" {
        return Err(bad("a place, not an object: zoom into it"));
    }
    let core = n.s("layer") == "core";
    let mut engine = "sql";
    // What it hangs from. A walk passes through nothing of the backbone:
    // the chain ends at the first router of the core it reaches.
    let up_sql = if core {
        format!("SELECT {SEEN}, hops() AS hop FROM {NODES}\nWHERE REACHABLE({EDGES}, '{id}', hops => 1, dir => 'both') AND layer = 'core'\nORDER BY hops(), id LIMIT 40")
    } else {
        format!("SELECT {SEEN}, hops() AS hop FROM {NODES}\nWHERE REACHABLE({EDGES}, '{id}', hops => 6, dir => 'in', through => layer != 'core')\nORDER BY hops(), id LIMIT 40")
    };
    let in_memory = |ids: Vec<(u32, usize)>| -> (String, HashMap<String, usize>) {
        let est = e.estate.as_ref().expect("checked");
        let hop: HashMap<String, usize> = ids.iter().map(|&(i, d)| (est.nodes[i as usize].id.clone(), d)).collect();
        (quoted(hop.keys()), hop)
    };
    let at = e.estate.as_ref().and_then(|est| est.find(&id));
    let with_hops = |rows: Vec<Row>, hop: &HashMap<String, usize>| -> Vec<Row> {
        let mut rows: Vec<Row> = rows.into_iter().map(|mut r| { let h = hop.get(&r.s("id")).copied().unwrap_or(0); r.insert("hop".into(), json!(h)); r }).collect();
        rows.sort_by_key(|r| (r.i("hop"), r.s("id")));
        rows
    };
    // A coordinator does not walk graphs: once the app knows, it sends no
    // graph statement and walks its own copy of the estate, reading the
    // rows it reaches by key or by their places.
    let upstream: Vec<Row> = match q.graph(if core { "Its neighbours in the backbone (REACHABLE, one hop)" } else { "What it hangs from, nearest first (REACHABLE against the edges)" }, &up_sql, json!({})).await? {
        Some(rows) => rows,
        None => {
            let (Some(est), Some(i)) = (&e.estate, at) else { return Err(bad("this server does not walk graphs, and the app has no copy of the estate to walk")) };
            engine = "app";
            let ids: Vec<(u32, usize)> = if core { est.walk_hops(i, 1, 40, true, true).into_iter().filter(|(j, _)| est.nodes[*j as usize].layer == "core").collect() } else { est.walk_hops(i, 6, 40, false, true) };
            let (list, hop) = in_memory(ids);
            with_hops(q.rows("What it hangs from (walked by the app: a coordinator does not walk graphs yet)", &format!("SELECT {SEEN} FROM {NODES}\nWHERE id IN ({list}) LIMIT 40"), json!({})).await?, &hop)
        }
    };
    // What depends on it, counted by kind and state.
    let down_sql = format!("SELECT kind, state, COUNT(*) AS n FROM {NODES}\nWHERE REACHABLE({EDGES}, '{id}', hops => 6, dir => 'out', through => layer != 'core') AND layer != 'core'\nGROUP BY kind, state LIMIT 100");
    let downstream: Vec<Row> = if engine == "sql" {
        q.rows("What depends on it, by kind and state (REACHABLE along the edges, grouped)", &down_sql, json!({})).await?
    } else {
        // The walk gives each object's kind; the rows that are not ok, read
        // for the walk's places, give the states (NET-22: not all "").
        let est = e.estate.as_ref().expect("checked");
        let below: Vec<u32> = est.below(at.expect("checked"), 200_000).into_iter().filter(|j| est.nodes[*j as usize].layer != "core").collect();
        let hurt = among(&q, "What depends on it that is not ok", est, &below, "id, state", "state != 'ok'", json!({})).await?;
        let state: HashMap<String, String> = hurt.rows.iter().map(|r| (r.s("id"), r.s("state"))).collect();
        let mut by: HashMap<(&str, String), i64> = HashMap::new();
        for j in &below {
            let n = &est.nodes[*j as usize];
            *by.entry((n.kind, state.get(n.id.as_str()).cloned().unwrap_or_else(|| "ok".into()))).or_default() += 1;
        }
        let mut rows: Vec<Row> = by.into_iter().map(|((k, st), n)| json!({"kind": k, "state": st, "n": n}).as_object().cloned().unwrap_or_default()).collect();
        rows.sort_by(|a, b| b.i("n").cmp(&a.i("n")).then(a.s("kind").cmp(&b.s("kind"))).then(a.s("state").cmp(&b.s("state"))));
        rows
    };
    // What else is in trouble within three hops, either way.
    let near_sql = format!("SELECT {SEEN}, hops() AS hop FROM {NODES}\nWHERE REACHABLE({EDGES}, '{id}', hops => 3, dir => 'both', through => layer != 'core') AND state != 'ok'\nORDER BY hops(), id LIMIT 40");
    let near: Vec<Row> = if engine == "sql" {
        q.rows("Trouble within three hops, either way (REACHABLE and a condition)", &near_sql, json!({})).await?
    } else {
        let est = e.estate.as_ref().expect("checked");
        let walked = est.walk_hops(at.expect("checked"), 3, 20_000, true, true);
        let set: Vec<u32> = walked.iter().map(|w| w.0).collect();
        let hop: HashMap<String, usize> = walked.iter().map(|&(i, d)| (est.nodes[i as usize].id.clone(), d)).collect();
        let mut rows = with_hops(among(&q, "Trouble within three hops", est, &set, SEEN, "state != 'ok'", json!({})).await?.rows, &hop);
        rows.truncate(40);
        rows
    };
    // The neighbourhood whose history matters: itself, what it hangs from,
    // and what is one hop away.
    let around_sql = format!("SELECT id, hops() AS hop FROM {NODES}\nWHERE REACHABLE({EDGES}, '{id}', hops => 1, dir => 'both')\nORDER BY hops() LIMIT 80");
    let around: Vec<String> = if engine == "sql" {
        q.rows("One hop around it", &around_sql, json!({})).await?.iter().map(|r| r.s("id")).collect()
    } else {
        let est = e.estate.as_ref().expect("checked");
        est.walk_hops(at.expect("checked"), 1, 80, true, true).into_iter().map(|(j, _)| est.nodes[j as usize].id.clone()).collect()
    };
    let mut relation: HashMap<String, String> = around.iter().map(|i| (i.clone(), "one hop away".to_string())).collect();
    for u in &upstream {
        relation.insert(u.s("id"), if core { "a neighbour in the backbone".into() } else { format!("it hangs from this, {} {} up", u.i("hop"), if u.i("hop") == 1 { "hop" } else { "hops" }) });
    }
    relation.insert(id.clone(), "this object".into());
    let since = iso(q.last() - 30 * 86_400);
    let mut history = q.rows("Changes and faults on it and around it, thirty days", &format!("SELECT id, ts, node, kind, event, severity, cause, text, author, ticket, minutes, fault FROM {EVENTS}\nWHERE node IN ({}) AND ts >= :since AND event IN ('change', 'fault')\nORDER BY ts DESC LIMIT 60", quoted(relation.keys())), json!({"since": since})).await?;
    for h in history.iter_mut() {
        let rel = relation.get(&h.s("node")).cloned().unwrap_or_default();
        h.insert("relation".into(), json!(rel));
    }
    let mut changes = Vec::new();
    if core {
        if let Some(d) = e.w.device(&id) {
            changes = q.rows("Changes of the backbone that name it (INT[] overlap)", &format!("SELECT id, title, status, executed, owner, risk FROM netops_changes\nWHERE device_nums && [{}] AND executed >= :since\nORDER BY executed DESC LIMIT 10", d.num), json!({"since": since})).await?;
        }
    }
    // Where the chain is broken: the nearest thing it hangs from that is
    // down or degraded, or an open incident on one of them.
    let open: Vec<Row> = e.st().open.values().map(|o| o.row.clone()).collect();
    let incident_on = |dev: &str| open.iter().find(|i| i.list("devices").iter().any(|d| d == dev)).map(|i| json!({"id": i.s("id"), "title": i.s("title"), "severity": i.s("severity")}));
    let own = incident_on(&id);
    let mut cause = Value::Null;
    if matches!(n.s("state").as_str(), "down" | "degraded") {
        cause = json!({"id": id, "name": n.s("name"), "kind": n.s("kind"), "state": n.s("state"), "reason": n.s("reason"), "since": n.s("since"), "hop": 0, "x": n.f("x"), "y": n.f("y")});
    } else if !core {
        if let Some(u) = upstream.iter().find(|u| matches!(u.s("state").as_str(), "down" | "degraded")) {
            cause = Value::Object(u.clone());
        } else if let Some((u, inc)) = upstream.iter().find_map(|u| incident_on(&u.s("id")).map(|i| (u, i))) {
            let mut c = u.clone();
            c.insert("incident".into(), inc);
            cause = Value::Object(c);
        }
    }
    // The change nearest before the trouble began, on the cause or around it.
    let began = cause["since"].as_str().filter(|s| !s.is_empty()).map(crate::telemetry::parse_iso).unwrap_or(0);
    let suspect = if began > 0 {
        history.iter().filter(|h| h.s("event") == "change").map(|h| (h, crate::telemetry::parse_iso(&h.s("ts")))).filter(|(_, ts)| *ts <= began && began - *ts <= 6 * 3600).max_by_key(|(_, ts)| *ts).map(|(h, ts)| {
            let mut s = h.clone();
            s.insert("minutes_before".into(), json!((began - ts) / 60));
            Value::Object(s)
        })
    } else {
        None
    };
    // How this ended before, on objects of its kind: the words of the
    // trouble against two months of faults.
    let words = [cause["reason"].as_str().unwrap_or(""), &n.s("reason")].into_iter().find(|s| !s.is_empty()).map(|s| s.trim_start_matches("Behind ").to_string()).or_else(|| history.iter().find(|h| h.s("event") == "fault").map(|h| h.s("text")));
    let kind = cause["kind"].as_str().map(str::to_string).unwrap_or_else(|| n.s("kind"));
    let mut before = json!({"words": words, "kind": kind, "like": [], "ended": []});
    if let Some(w) = &words {
        let p = json!({"q": w, "kind": kind, "id": cause["id"].as_str().unwrap_or(&id)});
        before["like"] = json!(q.soft("Faults that read like this one, on the same kind of object (BM25)", &format!("SELECT ts, node, cause, text, metro, hub, x, y FROM {EVENTS}\nWHERE event = 'fault' AND kind = :kind AND node != :id\nORDER BY bm25(text, :q) DESC LIMIT 5"), p.clone()).await);
        // Only the faults that read like this one: the clearings whose text
        // holds at least half of its words, the closest hundred, by cause.
        let cleared = q.soft("How the faults that read like this one ended (BM25, at least half of its words)", &format!("SELECT cause, minutes FROM {EVENTS}\nWHERE event = 'clear' AND kind = :kind\nORDER BY bm25(text, :q, min => '50%') DESC LIMIT {ENDED_LIKE}"), p).await;
        before["matched"] = json!(cleared.len());
        before["ended"] = json!(ended_by_cause(&cleared));
    }
    let mut total = 0;
    let mut not_ok = 0;
    for d in &downstream {
        total += d.i("n");
        if !matches!(d.s("state").as_str(), "ok" | "") {
            not_ok += d.i("n");
        }
    }
    q.done(json!({"node": n, "engine": engine, "incident": own, "cause": cause, "suspect": suspect, "upstream": upstream, "downstream": {"rows": downstream, "total": total, "not_ok": not_ok},
        "near": near, "history": history, "changes": changes, "before": before}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_said_with_their_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(170_337), "170,337");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    /// A walk is read by the places that hold it: the hub of what hangs
    /// from one hub router, the metro of what a metro's backbone carries.
    #[test]
    fn a_walk_is_held_by_its_hubs_or_its_metro() {
        let est = Estate::build(&World::build(), 4000);
        let hr = est.find("hr1-h01-ams").expect("a hub router");
        let below = est.below(hr, 100_000);
        assert!(!below.is_empty());
        assert_eq!(est.places_of(&below).as_deref(), Some("hub IN ('h01-ams')"));
        let ag = est.find("ag1-ams").expect("an aggregation router");
        let all = est.walk(ag, 6, 100_000, true, false);
        let place = est.places_of(&all).expect("a place holds it");
        assert!(place.starts_with("hub IN (") || place == "metro IN ('m-ams')", "{place}");
        assert!(all.iter().all(|&i| est.places(i)[1] == "m-ams" || est.nodes[i as usize].layer == "core"));
        assert_eq!(est.places_of(&[]), None);
    }
}
