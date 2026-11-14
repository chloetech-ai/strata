//! The records the app starts with: alert rules, runbooks, advisories,
//! configurations, and a month of history (changes, tickets, past incidents
//! and their timelines) for searches and for "have we seen this before?".
//! All of it is written for this demo; none of it describes a real network.

use serde_json::{json, Value};

use crate::telemetry::{iso, Fault};
use crate::world::{Rng, World, SITES};

pub const PEOPLE: &[(&str, &str)] = &[
    ("M. Okafor", "Backbone NOC"), ("L. Chen", "Backbone NOC"), ("J. Vermeer", "IP Edge"), ("S. Haddad", "IP Edge"),
    ("A. Novak", "Optical"), ("P. Ricci", "Security"), ("D. Lindqvist", "Platform"), ("R. Banerjee", "Backbone NOC"),
];

/// (id, name, description, sql, cmp, threshold, unit, severity, entity kind, window in seconds, runbook).
#[allow(clippy::type_complexity)]
pub const RULES: &[(&str, &str, &str, &str, &str, f64, &str, &str, &str, i64, &str)] = &[
    ("link-down", "Link down", "A link reported down in the last two minutes",
     "SELECT link AS entity, MIN(up) AS v\nFROM netops_link_metrics\nWHERE ts >= :since\nGROUP BY link\nHAVING MIN(up) < 1", "<", 1.0, "up", "critical", "link", 120, "rb-fibre-cut"),
    ("link-saturated", "Link saturated", "Average utilisation above 92% for three minutes",
     "SELECT link AS entity, AVG(util) * 100 AS v\nFROM netops_link_metrics\nWHERE ts >= :since AND up = 1\nGROUP BY link\nHAVING AVG(util) > 0.92", ">", 92.0, "%", "major", "link", 180, "rb-congestion"),
    ("link-loss", "Packet loss on a link", "Average loss above 0.5% on a link that is up",
     "SELECT link AS entity, AVG(loss) AS v\nFROM netops_link_metrics\nWHERE ts >= :since AND up = 1\nGROUP BY link\nHAVING AVG(loss) > 0.5", ">", 0.5, "% loss", "major", "link", 180, "rb-packet-loss"),
    ("link-errors", "Input errors on a link", "More than 60 CRC or framing errors in three minutes",
     "SELECT link AS entity, SUM(errors) AS v\nFROM netops_link_metrics\nWHERE ts >= :since\nGROUP BY link\nHAVING SUM(errors) > 60", ">", 60.0, "errors", "minor", "link", 180, "rb-optic"),
    ("optic-rx-low", "Optical receive power low", "Receive power below -10 dBm on a link that is up",
     "SELECT link AS entity, AVG(rx_dbm) AS v\nFROM netops_link_metrics\nWHERE ts >= :since AND up = 1\nGROUP BY link\nHAVING AVG(rx_dbm) < -10", "<", -10.0, "dBm", "major", "link", 180, "rb-optic"),
    ("device-unreachable", "Device unreachable", "A device stopped answering polls",
     "SELECT device AS entity, MIN(up) AS v\nFROM netops_device_metrics\nWHERE ts >= :since\nGROUP BY device\nHAVING MIN(up) < 1", "<", 1.0, "up", "critical", "device", 120, "rb-power"),
    ("device-cpu", "Control-plane CPU high", "Average CPU above 85% for three minutes",
     "SELECT device AS entity, AVG(cpu) AS v\nFROM netops_device_metrics\nWHERE ts >= :since AND up = 1\nGROUP BY device\nHAVING AVG(cpu) > 85", ">", 85.0, "% CPU", "major", "device", 180, "rb-rpd-cpu"),
    ("bgp-churn", "BGP update churn", "More than 3,000 BGP updates in three minutes",
     "SELECT device AS entity, SUM(bgp_updates) AS v\nFROM netops_device_metrics\nWHERE ts >= :since\nGROUP BY device\nHAVING SUM(bgp_updates) > 3000", ">", 3000.0, "updates", "minor", "device", 180, "rb-bgp-rollback"),
    ("bgp-session-down", "BGP session down", "A session with a transit provider or peer is not established",
     "SELECT device AS entity, COUNT(*) AS v\nFROM netops_bgp\nWHERE state != 'established'\nGROUP BY device\nHAVING COUNT(*) >= 1", ">=", 1.0, "sessions", "critical", "device", 0, "rb-bgp-rollback"),
    ("fw-sessions", "Firewall session table filling", "Average sessions above 1.5 million",
     "SELECT device AS entity, AVG(sessions) AS v\nFROM netops_device_metrics\nWHERE ts >= :since AND up = 1\nGROUP BY device\nHAVING AVG(sessions) > 1500000", ">", 1_500_000.0, "sessions", "major", "device", 180, "rb-ddos"),
    ("service-errors", "Service error rate", "More than 1% of a service's requests failed in three minutes",
     "SELECT service AS entity, SUM(errors) * 100.0 / SUM(requests) AS v\nFROM netops_service_metrics\nWHERE ts >= :since\nGROUP BY service\nHAVING SUM(errors) * 100.0 / SUM(requests) > 1.0", ">", 1.0, "% errors", "critical", "service", 180, "rb-packet-loss"),
    ("service-latency", "Service latency over its objective", "p95 latency above the service's objective (a join with the services table)",
     "SELECT m.service AS entity, AVG(m.latency_p95) / MAX(s.latency_slo_ms) * 100 AS v\nFROM netops_service_metrics m JOIN netops_services s ON m.service = s.id\nWHERE m.ts >= :since\nGROUP BY m.service\nHAVING AVG(m.latency_p95) / MAX(s.latency_slo_ms) > 1.0", ">", 100.0, "% of objective", "major", "service", 180, "rb-packet-loss"),
    ("udp-flood", "UDP amplification flood", "More than 3 Gb/s of UDP from amplification ports into one site",
     "SELECT dst_site AS entity, SUM(bytes) * 8 / 120.0 / 1000000000 AS v\nFROM netops_flows\nWHERE ts >= :since AND proto = 'udp' AND dst_site != 'scrubbing' AND dst_port IN (53, 123, 389, 1900, 11211)\nGROUP BY dst_site\nHAVING SUM(bytes) * 8 / 120.0 / 1000000000 > 3", ">", 3.0, "Gb/s", "critical", "site", 120, "rb-ddos"),
    // The estate behind the backbone: what an outage there leaves without
    // service is counted as the objects that hang from it (its edges), and
    // a big one (20 or more behind it) is serious enough to open an incident.
    ("estate-outage", "Outage in the estate", "An access node, a hub router or a hub's power plant is down and other objects depend on it; major from 20 objects behind it",
     "SELECT n.id AS entity, COUNT(*) AS v, MAX(n.reason) AS detail,\n  CASE WHEN COUNT(*) >= 20 THEN 'major' ELSE 'minor' END AS severity\nFROM netops_estate_nodes n JOIN netops_estate_edges e ON e.src = n.id\nWHERE n.state = 'down' AND n.layer IN ('access', 'aggregation', 'power')\nGROUP BY n.id", ">=", 1.0, "behind it", "major", "estate", 0, "rb-estate-outage"),
    ("estate-degraded", "Degraded in the estate", "An access node, a hub router or a hub's power plant is degraded and other objects depend on it",
     "SELECT n.id AS entity, COUNT(*) AS v, MAX(n.reason) AS detail\nFROM netops_estate_nodes n JOIN netops_estate_edges e ON e.src = n.id\nWHERE n.state = 'degraded' AND n.layer IN ('access', 'aggregation', 'power')\nGROUP BY n.id", ">=", 1.0, "behind it", "minor", "estate", 0, "rb-estate-outage"),
    ("error-log-burst", "Burst of error logs", "Four or more log lines at error severity or worse from one device in three minutes",
     "SELECT device AS entity, COUNT(*) AS v\nFROM netops_syslog\nWHERE ts >= :since AND sev <= 3\nGROUP BY device\nHAVING COUNT(*) >= 4", ">=", 4.0, "lines", "minor", "device", 180, "rb-packet-loss"),
];

pub fn rule_rows() -> Vec<Value> {
    RULES
        .iter()
        .map(|&(id, name, description, sql, cmp, threshold, unit, severity, kind, window, runbook)| {
            json!({"id": id, "name": name, "description": description, "sql_text": sql, "cmp": cmp, "threshold": threshold, "unit": unit, "severity": severity,
                "entity_kind": kind, "window_s": window, "clear_after": 2, "enabled": true, "runbook": runbook, "firing": 0, "builtin": true})
        })
        .collect()
}

/// (id, title, category, tags, body).
pub const RUNBOOKS: &[(&str, &str, &str, &[&str], &str)] = &[
    ("rb-fibre-cut", "Backbone circuit down: loss of signal", "fiber_cut", &["optical", "backbone", "carrier"],
     "Symptoms: link down on both ends at the same second, optics report Rx loss of signal (-40 dBm), OSPF adjacency and RSVP LSPs drop, the other plane runs hot.\n1. Confirm LOS on both ends (show interfaces diagnostics optics). If only one end sees LOS, suspect the local patch, not the span.\n2. Check the other plane's utilisation. Above 90%: move best-effort traffic to a third path by raising the IGP metric on the surviving link's neighbours, or apply the TE template PLANE-FAILOVER.\n3. Open a priority-1 ticket with the circuit's provider, quoting the circuit id and the time of the LOS alarm. Ask for an OTDR trace.\n4. Tell the customers with waves or L3VPN sites on the span.\n5. When light returns, wait for 10 minutes without errors before restoring metrics."),
    ("rb-bgp-rollback", "BGP sessions drop after a policy change: roll back", "bad_change", &["bgp", "change", "edge"],
     "Symptoms: several eBGP sessions on one border router go Idle within a minute of a configuration commit; policy errors in the log (undefined prefix-set or community-set); received prefixes fall to zero.\n1. Find the last commit (show configuration commit list) and the change ticket it names.\n2. Compare it with the previous version. Look for a route-policy that references an object the commit did not create.\n3. Roll back to the commit before it (rollback configuration to <id>). Do not try to fix forward during the incident.\n4. Watch sessions re-establish and prefixes return; transit sessions first.\n5. Mark the change as failed and book a review: the pre-check should have caught the missing object."),
    ("rb-optic", "Receive power falling on an optic: drain and replace", "optic_degrade", &["optical", "hardware"],
     "Symptoms: Rx power warnings that get worse over 10 to 30 minutes, CRC and FCS errors climbing, then interface flaps. Traffic still passes, with loss.\n1. Plot rx_dbm for the last hour. A steady slope means a dying optic or a dirty connector; a step means a bend or a patch change.\n2. Drain the link before it flaps: set the IGP metric to 65535 on both ends and wait for utilisation to fall under 5%.\n3. Check the far end's Tx power. Normal Tx with low Rx on our side points at our optic or the patch.\n4. Dispatch field services to clean the connector and swap the optic (QSFP28-100G-LR4 spares are in every hub).\n5. After the swap, 15 minutes with no errors, then restore the metric."),
    ("rb-ddos", "Volumetric UDP flood: divert to scrubbing", "ddos", &["security", "ddos", "flowspec"],
     "Symptoms: UDP from source ports 53, 123, 389, 1900 or 11211 toward one customer prefix, from hundreds of networks; borders at one site saturate, LPTS policers drop control traffic, firewall session tables fill.\n1. Identify the target prefix and the ports from the flow summaries (top destinations by bytes, proto udp).\n2. Announce a flowspec rule for the prefix and those source ports with action redirect to the scrubbing VRF. Do not blackhole unless the customer asks.\n3. Confirm clean traffic returns from the scrubbers and the borders' utilisation falls.\n4. Tell the customer; offer always-on DDoS Shield if they are on-demand.\n5. Remove the rule 30 minutes after the attack stops."),
    ("rb-rpd-cpu", "Routing daemon pegs the CPU: restart routing, plan the upgrade", "cpu_bug", &["software", "advisory", "core"],
     "Symptoms: control-plane CPU above 90% on one router, scheduler slips in the log, BGP hold timers expiring on its neighbours, update churn across the site.\n1. Check the advisories against the router's software version; the rib-walk defect is fixed in 22.4R3-S2.\n2. If a second routing engine is in sync, switch over; otherwise restart routing gracefully in a quiet minute.\n3. Confirm CPU falls under 40% and neighbours stop timing out.\n4. Raise a change to upgrade every router on the affected release; list them from the inventory."),
    ("rb-power", "Several devices at one site go dark together: check power", "power", &["facilities", "power"],
     "Symptoms: two or more devices at one site stop answering at the same second; the devices that remain log a power supply input failure on feed A.\n1. If the survivors report PEM input failure, it is a feed, not the network. Call the site's facilities desk.\n2. Check that the surviving plane carries the site (cr2 and ag2); watch its utilisation.\n3. Ask facilities for the PDU breaker state; do not power-cycle remotely.\n4. When the feed returns, devices boot in about four minutes; adjacencies follow."),
    ("rb-congestion", "A link runs hot without a fault: move traffic, plan capacity", "capacity", &["capacity", "te"],
     "Symptoms: utilisation above 90% at the daily peak, queue drops in best-effort, no link down elsewhere.\n1. Check whether a parallel link is down or drained; if so, treat that fault first.\n2. Look at the top sources in the flow summaries; a single CDN can be moved to another peering point.\n3. Apply the TE template to spread load across both planes.\n4. Open a capacity ticket if the 4-week trend crosses 80% at peak."),
    ("rb-packet-loss", "Packet loss or latency on a service: find the hop", "generic", &["loss", "latency", "slo"],
     "Symptoms: a service's error rate or p95 latency breaches its objective with no device down.\n1. List the links on the service's path with loss above 0.1% or errors.\n2. Compare with changes executed in the last hour on those devices.\n3. Check the firewalls' session tables if the service is hosted.\n4. If one link stands out, drain it and follow the optic or congestion runbook."),
    ("rb-fw-sessions", "Firewall session table filling", "ddos", &["security", "firewall"],
     "Symptoms: session table above 80%, new connections dropped.\n1. Find the top destination by sessions; one address means an attack or a misbehaving client.\n2. Enable flood protection on the untrust zone for that destination.\n3. If volumetric, follow the DDoS runbook and divert upstream."),
    ("rb-estate-outage", "An access node or a hub is down: who is behind it, and who goes", "estate", &["estate", "access", "field"],
     "Symptoms: an OLT, a cell site, a business-park switch, a hub router or a hub's power plant reports down or degraded, and what hangs from it (ONTs, radios, customer routers, access nodes) is impacted.\n1. Open the object on the map: its reason says what it reported (loss of signal on a PON port, mains lost, an uplink optic, a change before it).\n2. Count what is behind it and which customers; a business park's customers are named on their routers.\n3. Look for a change on it or on what it hangs from in the hour before; roll it back if there is one.\n4. Power or fibre: dispatch field services to the site; tell the facilities desk for a hub's power.\n5. When it is back, check that what was behind it came back too before closing."),
    ("rb-customer-port", "Customer access port flapping", "generic", &["access", "customer"],
     "Symptoms: one access port on an aggregation router goes up and down.\n1. Check the customer's ticket history; CPE reboots are the usual cause.\n2. Look at light levels on the access optic.\n3. If flapping exceeds 10 times an hour, dampen the port and call the customer."),
];

pub fn runbook_rows(now: i64) -> Vec<Value> {
    RUNBOOKS
        .iter()
        .enumerate()
        .map(|(i, &(id, title, category, tags, body))| json!({"id": id, "title": title, "body": body, "tags": tags, "category": category, "uses": 3 + (i * 7) % 19, "wins": 2 + (i * 5) % 14,
            "updated": iso(now - 86400 * (9 + i as i64 * 11)), "owner": PEOPLE[i % PEOPLE.len()].0}))
        .collect()
}

pub fn advisory_rows(now: i64) -> Vec<Value> {
    let a = |id: &str, vendor: &str, os: &str, affected: &[&str], fixed: &str, severity: &str, cvss: f64, title: &str, summary: &str, days: i64| {
        json!({"id": id, "vendor": vendor, "os": os, "affected": affected, "fixed_in": fixed, "severity": severity, "cvss": cvss, "title": title, "summary": summary, "published": iso(now - days * 86400)})
    };
    vec![
        a("JSA-2026-7741", "Juniper", "Junos", &["22.2R3-S1"], "22.4R3-S2", "high", 7.5, "rpd: high CPU and scheduler slips during a RIB walk with many BGP add-path routes",
          "On releases before 22.4R3-S2, a burst of BGP updates with add-path can leave the routing process spinning in a RIB walk. CPU stays above 90%, hold timers expire on neighbours. Workaround: restart routing. Fixed in 22.4R3-S2.", 41),
        a("JSA-2026-7802", "Juniper", "Junos", &["22.2R3-S1", "23.2R2"], "23.2R2-S1", "medium", 5.3, "chassisd: optics alarm thresholds not applied after an in-service upgrade",
          "Optics diagnostics may report stale thresholds after an upgrade until the optic is reseated. Monitoring that relies on the device's own alarm may miss a low receive power.", 18),
        a("cisco-sa-iosxr-bgp-flowspec-9K2", "Cisco", "IOS XR", &["7.7.21"], "7.9.2", "high", 8.6, "IOS XR: BGP process restarts on a crafted flowspec NLRI",
          "A flowspec update with an overlong component can restart the BGP process on 7.7.x. Sessions drop and re-establish. Fixed in 7.9.2.", 63),
        a("cisco-sa-iosxr-ssh-pk-4D1", "Cisco", "IOS XR", &["7.7.21", "7.9.2"], "7.10.1", "medium", 5.9, "IOS XR: SSH public-key authentication bypass with a specific key type",
          "Devices that accept sk-ecdsa keys may accept a signature that should fail. Restrict key types or upgrade.", 9),
        a("SA-2026-0113", "Arista", "EOS", &["4.29.4M"], "4.31.2F", "medium", 6.5, "EOS: memory leak in the LLDP agent with frequent neighbour changes",
          "On 4.29 trains the Lldp agent leaks memory when neighbours flap. After weeks the agent restarts and neighbours age out briefly.", 27),
        a("SA-2026-0127", "Arista", "EOS", &["4.30.5M", "4.29.4M"], "4.31.2F", "low", 3.7, "EOS: BFD sessions may take 3x the configured interval to detect a failure on LAG members", "Slow detection only; no crash.", 55),
        a("PAN-SA-2026-0031", "Palo Alto", "PAN-OS", &["10.2.7"], "11.0.3-h1", "critical", 9.8, "PAN-OS: command injection in the management interface",
          "An unauthenticated attacker with network access to the management interface can run commands as root. Restrict management access and upgrade.", 6),
    ]
}

/// A device's configuration at a version. Later versions add the lines
/// their change made, so that blame can say which change brought a line.
pub fn config_text(w: &World, di: usize, edits: &[(&str, Vec<String>)]) -> String {
    let d = &w.devices[di];
    let mut out: Vec<String> = Vec::new();
    let ios = d.vendor == "Cisco" || d.vendor == "Arista";
    if ios {
        out.push(format!("hostname {}", d.id));
        out.push("!".into());
        out.push("ntp server 10.250.0.1".into());
        out.push("ntp server 10.250.0.2".into());
        out.push("logging host 10.250.4.20 transport udp port 514".into());
        out.push("snmp-server community n0c-r0 RO ACL-MGMT".into());
        out.push("!".into());
        out.push("interface Loopback0".into());
        out.push(format!(" ipv4 address {} 255.255.255.255", d.loopback));
        out.push("!".into());
        for li in w.links_of(di) {
            let l = &w.links[li];
            let (my_if, other) = if l.a == di { (&l.a_if, l.b) } else { (&l.z_if, l.a) };
            out.push(format!("interface {my_if}"));
            out.push(format!(" description to {} [{}]{}", w.devices[other].id, l.kind, if l.circuit.is_empty() { String::new() } else { format!(" circuit {}", l.circuit) }));
            out.push(" mtu 9192".into());
            out.push(format!(" ipv4 address 10.255.{}.{} 255.255.255.254", l.num, if l.a == di { 0 } else { 1 }));
            out.push("!".into());
        }
        if d.role == "border" {
            out.push("prefix-set PS-CUSTOMERS-V4".into());
            out.push("  100.64.0.0/10 le 24".into());
            out.push("end-set".into());
            out.push("route-policy RP-TRANSIT-IN".into());
            out.push("  if destination in PS-CUSTOMERS-V4 then drop endif".into());
            out.push("  set local-preference 100".into());
            out.push("  pass".into());
            out.push("end-policy".into());
            out.push("router bgp 64512".into());
            for p in w.peers.iter().filter(|p| p.device == di) {
                out.push(format!(" neighbor {}", p.peer_ip));
                out.push(format!("  remote-as {}", p.asn));
                out.push(format!("  description {} ({})", p.name, p.kind));
                out.push("  address-family ipv4 unicast route-policy RP-TRANSIT-IN in".into());
            }
            out.push("!".into());
        }
        out.push("ipv4 access-list EDGE-IN".into());
        out.push(" 10 deny tcp any any eq 23".into());
        out.push(" 20 deny udp any any eq 1900".into());
        out.push(" 30 permit ipv4 any any".into());
    } else {
        out.push(format!("set system host-name {}", d.id));
        out.push("set system ntp server 10.250.0.1".into());
        out.push("set system ntp server 10.250.0.2".into());
        out.push("set system syslog host 10.250.4.20 any notice".into());
        out.push("set snmp community n0c-r0 authorization read-only".into());
        out.push(format!("set interfaces lo0 unit 0 family inet address {}/32", d.loopback));
        for li in w.links_of(di) {
            let l = &w.links[li];
            let (my_if, other) = if l.a == di { (&l.a_if, l.b) } else { (&l.z_if, l.a) };
            out.push(format!("set interfaces {my_if} description \"to {} [{}]{}\"", w.devices[other].id, l.kind, if l.circuit.is_empty() { String::new() } else { format!(" circuit {}", l.circuit) }));
            out.push(format!("set interfaces {my_if} mtu 9192"));
            out.push(format!("set interfaces {my_if} unit 0 family inet address 10.255.{}.{}/31", l.num, if l.a == di { 0 } else { 1 }));
            out.push(format!("set protocols ospf area 0.0.0.0 interface {my_if}.0 metric {}", if l.kind == "backbone" { (l.latency_ms * 10.0) as i64 } else { 10 }));
            if d.role == "core" {
                out.push(format!("set protocols mpls interface {my_if}.0"));
                out.push(format!("set protocols rsvp interface {my_if}.0"));
            }
        }
        out.push("set protocols bgp group IBGP-RR type internal".into());
        out.push(format!("set protocols bgp group IBGP-RR local-address {}", d.loopback));
        out.push("set routing-options autonomous-system 64512".into());
    }
    for (_, lines) in edits {
        out.extend(lines.iter().cloned());
    }
    out.join("\n")
}

const CHANGE_KINDS: &[(&str, &str, &str, &str)] = &[
    ("Upgrade {os} on {dev}", "software", "medium", "Install the current release on one routing engine, switch over, install on the other."),
    ("Add 100G uplink between {dev} and its core", "hardware", "low", "New optic and patch; bring up LLDP and OSPF, then add to the bundle."),
    ("Update EDGE-IN access list on {dev}", "config", "low", "Add deny entries for newly abused UDP ports."),
    ("Tune RSVP auto-bandwidth on {dev}", "config", "medium", "Adjust the adjust-interval and thresholds for the transatlantic LSPs."),
    ("Replace optic on {dev}", "hardware", "low", "Swap a QSFP28 that shows a falling receive level."),
    ("Rotate SNMP community on {dev}", "config", "low", "New read-only community, pushed with the monitoring cut-over."),
    ("Raise OSPF metric to drain {dev} for line-card work", "maintenance", "medium", "Drain, replace the card in slot 3, restore."),
    ("New peering session with a content network on {dev}", "config", "medium", "Add the neighbour, apply RP-PEER-IN and RP-PEER-OUT, check prefixes."),
    ("Carrier maintenance on a backbone span terminating on {dev}", "circuit", "high", "The provider splices fibre; the span is down for up to four hours. Traffic rides the other plane."),
    ("Route-policy clean-up on {dev}", "config", "high", "Remove unused prefix-sets and communities from the transit policies."),
];

pub struct History {
    pub changes: Vec<Value>,
    pub tickets: Vec<Value>,
    pub incidents: Vec<Value>,
    pub timeline: Vec<Value>,
    pub faults: Vec<Fault>,
    pub configs: Vec<Value>,
    pub oncall: Vec<Value>,
    pub judgments: Vec<Value>,
}

fn nums(w: &World, ids: &[String]) -> Vec<i64> {
    ids.iter().filter_map(|d| w.device(d).map(|x| x.num)).collect()
}

/// What an incident of each kind is called and how it reads, given its
/// target. Used for history here and for live incidents by the agents.
pub struct Story {
    pub title: String,
    pub summary: String,
    pub root_cause: String,
    pub root_kind: &'static str,
    pub root_entity: String,
    pub runbook: &'static str,
    pub severity: &'static str,
}

pub fn story(w: &World, f: &Fault) -> Story {
    let link = f.link.as_deref().and_then(|l| w.link_by_id(l)).map(|i| &w.links[i]);
    let dev = f.device.clone().unwrap_or_default();
    let site = f.site.clone().unwrap_or_default();
    match f.scenario.as_str() {
        "fiber_cut" => {
            let l = link.expect("a link");
            let (a, z) = (w.devices[l.a].site.to_uppercase(), w.devices[l.b].site.to_uppercase());
            Story {
                title: format!("Backbone circuit {a}-{z} plane {} down: loss of signal", l.plane),
                summary: format!("Both ends of {} lost light at the same second. OSPF and RSVP dropped on the span and its traffic moved to plane {}, which ran above 95% with queue drops until traffic was spread to a third path.", l.id, if l.plane == "A" { "B" } else { "A" }),
                root_cause: format!("Loss of signal on circuit {} ({}): a fibre cut or a failed amplifier on the provider's span between {a} and {z}.", l.circuit, l.provider),
                root_kind: "link", root_entity: l.id.clone(), runbook: "rb-fibre-cut", severity: "sev1",
            }
        }
        "optic_degrade" => {
            let l = link.expect("a link");
            Story {
                title: format!("Degrading optic on {} toward {}", w.devices[l.b].id, w.devices[l.a].id),
                summary: format!("Receive power on {} {} fell steadily for twenty minutes; CRC errors and loss followed, then the link began to flap. Draining the link stopped the loss.", w.devices[l.b].id, l.z_if),
                root_cause: format!("Failing QSFP28 optic (or a contaminated connector) on {} {}: receive power fell from -2.8 dBm to below -14 dBm while the far end's transmit power stayed normal.", w.devices[l.b].id, l.z_if),
                root_kind: "link", root_entity: l.id.clone(), runbook: "rb-optic", severity: "sev2",
            }
        }
        "bad_change" => Story {
            title: format!("BGP sessions down on {dev} after change {}", f.change.clone().unwrap_or_default()),
            summary: format!("Every eBGP session on {dev} went Idle within seconds of a configuration commit. The transit policy referenced a prefix-set the commit did not define, so it dropped every route. The site's other border carried the traffic."),
            root_cause: format!("Change {} on {dev}: route-policy RP-TRANSIT-IN references prefix-set PS-BOGONS-V4, which the change did not create, so the policy drops all received routes.", f.change.clone().unwrap_or_default()),
            root_kind: "change", root_entity: f.change.clone().unwrap_or_default(), runbook: "rb-bgp-rollback", severity: "sev1",
        },
        "ddos" => Story {
            title: format!("UDP amplification attack on {} via {}", f.customer.clone().unwrap_or_default(), site.to_uppercase()),
            summary: format!("A flood of UDP from DNS, NTP, SSDP and memcached reflectors toward {} saturated the borders at {} and filled the firewalls' session tables.", f.customer.clone().unwrap_or_default(), site.to_uppercase()),
            root_cause: format!("Volumetric DDoS (UDP amplification from source ports 53, 123, 1900, 11211) aimed at customer prefix {}, entering at the {} borders.", f.customer.clone().unwrap_or_default(), site.to_uppercase()),
            root_kind: "site", root_entity: site.clone(), runbook: "rb-ddos", severity: "sev1",
        },
        "cpu_bug" => {
            let v = w.device(&dev).map(|d| d.os_version.clone()).unwrap_or_default();
            Story {
                title: format!("Routing process pegs CPU on {dev}"),
                summary: format!("CPU on {dev} rose above 90% with scheduler slips; its neighbours' BGP hold timers expired and updates churned across the site."),
                root_cause: format!("Software defect in Junos {v} on {dev} (advisory JSA-2026-7741): the routing process spins in a RIB walk after a burst of add-path updates."),
                root_kind: "device", root_entity: dev.clone(), runbook: "rb-rpd-cpu", severity: "sev2",
            }
        }
        _ => Story {
            title: format!("Power feed A lost at {}", site.to_uppercase()),
            summary: format!("The devices on feed A at {} went dark in the same second; the devices on feed B reported a power supply input failure. Plane B carried the site.", site.to_uppercase()),
            root_cause: format!("Loss of power feed A (PDU-A-{}) at the {} site: a facilities fault, not a network one.", site.to_uppercase(), site.to_uppercase()),
            root_kind: "site", root_entity: site.clone(), runbook: "rb-power", severity: "sev1",
        },
    }
}

/// The devices a fault makes alert (for seeded incidents' scope).
pub fn scope(w: &World, f: &Fault) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(l) = f.link.as_deref().and_then(|l| w.link_by_id(l)).map(|i| &w.links[i]) {
        out.push(w.devices[l.a].id.clone());
        out.push(w.devices[l.b].id.clone());
        if let Some(s) = l.sibling {
            out.push(w.devices[w.links[s].a].id.clone());
            out.push(w.devices[w.links[s].b].id.clone());
        }
    }
    if let Some(d) = &f.device {
        out.push(d.clone());
    }
    if let Some(s) = &f.site {
        if f.scenario == "power" {
            out.extend(crate::telemetry::feed_a(w, s).into_iter().map(|i| w.devices[i].id.clone()));
        } else {
            out.extend(w.devices.iter().filter(|d| d.site == *s && (d.role == "border" || d.role == "firewall")).map(|d| d.id.clone()));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A target for a scenario, picked by `rng`.
pub fn new_fault(w: &World, scenario: &str, started: i64, rng: &mut Rng, origin: &str) -> Fault {
    let backbone: Vec<&crate::world::Link> = w.links.iter().filter(|l| l.kind == "backbone" && l.live).collect();
    let mut f = Fault {
        id: format!("flt-{}-{}", started, &scenario[..3]), scenario: scenario.into(), started, mitigated: None, ended: None, link: None, device: None, site: None, change: None, customer: None,
        truth_kind: String::new(), truth_entity: String::new(), category: scenario.into(), incident: None, origin: origin.into(),
        auto_mitigate_after: 9 * 60 + rng.below(240) as i64, repair_after: 8 * 60 + rng.below(360) as i64,
    };
    match scenario {
        "fiber_cut" => f.link = Some(rng.pick(&backbone).id.clone()),
        "optic_degrade" => {
            let pool: Vec<&crate::world::Link> = w.links.iter().filter(|l| l.live && l.documented && (l.kind == "backbone" || l.kind == "core")).collect();
            f.link = Some(rng.pick(&pool).id.clone());
            f.auto_mitigate_after = 21 * 60 + rng.below(180) as i64;
        }
        "bad_change" => {
            let borders: Vec<&crate::world::Device> = w.devices.iter().filter(|d| d.role == "border" && d.id.starts_with("br1") && w.devices.iter().any(|o| o.site == d.site && o.id.starts_with("br2"))).collect();
            f.device = Some(rng.pick(&borders).id.clone());
            f.change = Some(format!("CHG-{}", 4000 + (started / 60) % 5000));
        }
        "ddos" => {
            let s = *rng.pick(&["ams", "fra", "nyc", "lon"]);
            f.site = Some(s.into());
            f.customer = Some(format!("203.0.{}.0/24", 113 + rng.below(8)));
        }
        "cpu_bug" => {
            let old: Vec<&crate::world::Device> = w.devices.iter().filter(|d| d.role == "core" && d.os_version == "22.2R3-S1").collect();
            f.device = Some(rng.pick(&old).id.clone());
        }
        _ => {
            f.scenario = "power".into();
            f.category = "power".into();
            f.site = Some((*rng.pick(&["waw", "sto", "mad", "mil", "par", "ash"])).into());
            f.auto_mitigate_after = 12 * 60 + rng.below(200) as i64;
            f.repair_after = 60;
        }
    }
    let s = story(w, &f);
    f.truth_kind = s.root_kind.into();
    f.truth_entity = s.root_entity;
    f
}

pub fn history(w: &World, now: i64) -> History {
    let mut rng = Rng::new(77);
    let mut h = History { changes: vec![], tickets: vec![], incidents: vec![], timeline: vec![], faults: vec![], configs: vec![], oncall: vec![], judgments: vec![] };
    let day = 86400;

    // Changes: three weeks done, ten days planned. A few planned ones share
    // devices and overlap in time, which the calendar shows as collisions.
    let mut edits: std::collections::HashMap<usize, Vec<(String, i64, String, Vec<String>)>> = std::collections::HashMap::new();
    let routers: Vec<usize> = w.devices.iter().enumerate().filter(|(_, d)| d.role != "leaf").map(|(i, _)| i).collect();
    for n in 0..58 {
        let past = n < 38;
        let start = if past { now - (21 * day) + n as i64 * (21 * day / 38) + rng.below(7200) as i64 } else { now + 3 * 3600 + ((n as i64 - 38) / 2) * (10 * day / 10) };
        let start = start - start % 1800;
        let (title, kind, risk, description) = CHANGE_KINDS[(n * 3 + n / 7) % CHANGE_KINDS.len()];
        let di = if !past && (n % 5 == 0 || n == 44) { w.dev("cr1-fra") } else if !past && n % 5 == 1 { w.dev("cr2-fra") } else { routers[rng.below(routers.len())] };
        let d = &w.devices[di];
        let mut devices = vec![d.id.clone()];
        if kind == "circuit" || kind == "maintenance" {
            if let Some(&li) = w.links_of(di).iter().find(|&&li| w.links[li].kind == "backbone") {
                let l = &w.links[li];
                devices.push(w.devices[if l.a == di { l.b } else { l.a }].id.clone());
            }
        }
        let id = format!("CHG-{}", 3100 + n * 13);
        let (owner, _) = PEOPLE[rng.below(PEOPLE.len())];
        let approver = PEOPLE[(n + 3) % PEOPLE.len()].0;
        let failed = past && n % 13 == 6;
        let services: Vec<&str> = w.services.iter().filter(|s| s.devices.contains(&di)).map(|s| s.id).take(4).collect();
        let line = match kind {
            "config" if d.vendor == "Cisco" || d.vendor == "Arista" => vec![format!(" {} deny udp any any eq {}", 21 + n, [11211, 389, 19, 3702][n % 4])],
            "config" => vec![format!("set firewall family inet filter EDGE-IN term t{n} from port {}", [11211, 389, 19, 3702][n % 4])],
            "software" => vec![],
            _ if d.vendor == "Cisco" || d.vendor == "Arista" => vec![format!("! {id}: {}", title.replace("{dev}", &d.id).replace("{os}", d.os))],
            _ => vec![format!("set groups NOTES apply-macro \"{id} {}\"", kind)],
        };
        if past && !line.is_empty() {
            edits.entry(di).or_default().push((id.clone(), start, owner.to_string(), line.clone()));
        }
        h.changes.push(json!({
            "id": id, "title": title.replace("{dev}", &d.id).replace("{os}", d.os), "description": description, "kind": kind, "risk": risk,
            "status": if failed { "rolled_back" } else if past { "completed" } else if n % 4 == 0 { "scheduled" } else { "approved" },
            "window_start": iso(start), "window_end": iso(start + if kind == "circuit" { 4 * 3600 } else { 2 * 3600 }), "executed": if past { json!(iso(start + 600)) } else { Value::Null },
            "owner": owner, "approver": approver, "devices": devices, "device_nums": nums(w, &devices), "sites": [d.site], "services": services,
            "ticket": format!("TKT-{}", 52000 + n * 17), "rollback_plan": "Roll back to the previous commit; confirm adjacencies and sessions return within five minutes.",
            "diff": line.iter().map(|l| format!("+ {l}")).collect::<Vec<_>>().join("\n"), "outcome": if failed { "rolled back after alarms" } else if past { "as planned" } else { "" },
        }));
    }
    // Configurations: version 1 a month ago, then one version per change.
    for (di, d) in w.devices.iter().enumerate() {
        let mut applied: Vec<(&str, Vec<String>)> = Vec::new();
        let body = config_text(w, di, &[]);
        h.configs.push(json!({"id": format!("{}@1", d.id), "device": d.id, "version": 1, "ts": iso(now - 30 * day), "author": "oxidized", "change_id": "baseline", "lines": body.lines().count(), "body": body}));
        let mut list = edits.remove(&di).unwrap_or_default();
        list.sort_by_key(|e| e.1);
        for (v, (chg, at, who, lines)) in list.iter().enumerate() {
            applied.push((chg.as_str(), lines.clone()));
            let body = config_text(w, di, &applied);
            h.configs.push(json!({"id": format!("{}@{}", d.id, v + 2), "device": d.id, "version": v + 2, "ts": iso(at + 900), "author": who, "change_id": chg, "lines": body.lines().count(), "body": body}));
        }
    }

    // Past incidents: two of each kind over the last month, and one inside
    // the telemetry that is seeded, so its bump is on the charts.
    let offsets: &[(i64, &str)] = &[
        (27 * day + 4000, "fiber_cut"), (24 * day + 30000, "ddos"), (21 * day + 9000, "bad_change"), (18 * day + 50000, "optic_degrade"), (16 * day + 2000, "cpu_bug"),
        (13 * day + 61000, "power"), (11 * day + 20000, "fiber_cut"), (9 * day + 43000, "bad_change"), (6 * day + 70000, "ddos"), (4 * day + 15000, "optic_degrade"),
        (2 * day + 33000, "cpu_bug"), (4 * 3600 + 1200, "optic_degrade"),
    ];
    for (n, &(ago, scenario)) in offsets.iter().enumerate() {
        let started = now - ago;
        let started = started - started % 60;
        let mut f = new_fault(w, scenario, started, &mut rng, "seeded");
        let detect = 120 + rng.below(120) as i64;
        let mitig = f.auto_mitigate_after;
        f.mitigated = Some(started + mitig);
        f.ended = Some(started + mitig + f.repair_after);
        let id = format!("INC-{}", 2380 + n * 3);
        f.incident = Some(id.clone());
        let s = story(w, &f);
        let devices = scope(w, &f);
        let sites: Vec<&str> = SITES.iter().filter(|x| devices.iter().any(|d| d.ends_with(x.id))).map(|x| x.id).collect();
        let services: Vec<&str> = w.services.iter().filter(|sv| sv.sites.iter().any(|x| sites.contains(x))).map(|sv| sv.id).take(5).collect();
        let customers = w.customers.iter().filter(|c| sites.contains(&c.site)).count();
        let commander = PEOPLE[n % PEOPLE.len()].0;
        let resolved = started + mitig + f.repair_after + 600;
        let postmortem = format!("What happened: {}\n\nCause: {}\n\nDetection: the first alert fired {} s after the fault began; alerts were grouped into one incident by topology.\n\nResolution: followed runbook {} ({}). Service was restored {} minutes after the start; the underlying fault was repaired {} minutes later.\n\nFollow-ups: {}",
            s.summary, s.root_cause, detect, s.runbook, RUNBOOKS.iter().find(|r| r.0 == s.runbook).map(|r| r.1).unwrap_or(""), mitig / 60, f.repair_after / 60,
            match scenario { "fiber_cut" => "ask the provider for diverse routing on this span; add a third path to the TE template.", "bad_change" => "add a pre-check that every object a policy references exists; require a dry-run diff in the change ticket.",
                "optic_degrade" => "alert on the slope of receive power, not only its level; keep two spare optics per hub.", "ddos" => "offer always-on scrubbing to the customer; pre-stage flowspec templates.",
                "cpu_bug" => "upgrade every router on the affected release (see the advisory).", _ => "ask the site for dual-feed PDUs on the A side." });
        h.incidents.push(json!({
            "id": id, "title": s.title, "severity": s.severity, "status": "resolved", "started": iso(started), "detected": iso(started + detect), "mitigated": iso(started + mitig), "resolved": iso(resolved),
            "commander": commander, "summary": s.summary, "root_kind": s.root_kind, "root_entity": s.root_entity, "root_cause": s.root_cause, "confidence": 0.8 + 0.15 * rng.unit(), "category": scenario,
            "devices": devices, "device_nums": nums(w, &devices), "sites": sites, "services": services, "customers": customers,
            "revenue_at_risk": services.iter().filter_map(|x| w.services.iter().find(|sv| sv.id == *x)).map(|sv| sv.revenue_per_min).sum::<f64>() * (mitig as f64 / 60.0) * 0.2,
            "alert_count": 4 + rng.below(9), "change_id": f.change.clone().unwrap_or_default(), "runbook": s.runbook, "postmortem": postmortem, "origin": "seeded", "fault_id": f.id,
            "mttr_min": (mitig as f64 / 60.0 * 10.0).round() / 10.0, "ttd_s": detect,
        }));
        let mut ev = |at: i64, kind: &str, actor: &str, note: String| {
            let k = h.timeline.len();
            h.timeline.push(json!({"id": format!("{id}:{k}"), "incident": id, "ts": iso(at), "kind": kind, "actor": actor, "note": note}));
        };
        ev(started + detect, "alert", "alert rules", format!("First alert fired; {} more within two minutes", 3 + n % 6));
        ev(started + detect + 20, "agent", "triage agent", format!("Grouped the alerts into one incident by topology: {} devices within two hops", devices.len()));
        ev(started + detect + 45, "agent", "root-cause agent", format!("Named the likely cause: {}", s.root_cause));
        ev(started + detect + 200, "status", commander, "Took the incident; status Identified".into());
        ev(started + mitig, "action", commander, format!("Mitigated following {}", s.runbook));
        ev(started + mitig + f.repair_after, "status", "simulator", "The underlying fault was repaired".into());
        ev(resolved, "status", commander, "Resolved; postmortem written".into());
        h.judgments.push(json!({"id": format!("{id}:{}", s.runbook), "incident": id, "runbook": s.runbook, "grade": 3, "ts": iso(resolved), "judge": commander, "search": s.title}));
        h.tickets.push(json!({"id": format!("TKT-{}", 61000 + n * 29), "title": format!("[{id}] {}", s.title), "body": format!("{}\n\nCustomer impact: {} customers at {}.", s.summary, customers, sites.join(", ").to_uppercase()),
            "status": "closed", "priority": if s.severity == "sev1" { "P1" } else { "P2" }, "assignee": commander, "queue": "NOC", "incident": id, "device": devices.first().cloned().unwrap_or_default(),
            "opened": iso(started + detect + 60), "updated": iso(resolved), "resolution": format!("{} Closed with runbook {}.", s.root_cause, s.runbook), "runbook": s.runbook}));
        if scenario == "bad_change" {
            let dev = f.device.clone().unwrap_or_default();
            h.changes.push(json!({"id": f.change.clone().unwrap_or_default(), "title": format!("Route-policy update on {dev}: drop bogons from transit"), "description": "Add a bogon filter to RP-TRANSIT-IN.", "kind": "config", "risk": "medium",
                "status": "rolled_back", "window_start": iso(started - 600), "window_end": iso(started + 3000), "executed": iso(started - 20), "owner": "J. Vermeer", "approver": "S. Haddad",
                "devices": [dev.clone()], "device_nums": nums(w, &[dev.clone()]), "sites": [f.site.clone().unwrap_or_else(|| dev[4..].to_string())], "services": ["ip-transit"],
                "ticket": format!("TKT-{}", 59000 + n * 31), "rollback_plan": "rollback configuration last 1", "diff": "  route-policy RP-TRANSIT-IN\n+   if destination in PS-BOGONS-V4 then drop endif\n    set local-preference 100", "outcome": "rolled back after alarms"}));
        }
        h.faults.push(f);
    }

    // Tickets: the everyday queue.
    let kinds: &[(&str, &str, &str, &str)] = &[
        ("Customer {c}: intermittent packet loss toward {site}", "Customer reports 0.5 to 2% loss in the evening. MTR shows loss starting at our aggregation router. No alarms on our side.", "Found a marginal access optic; replaced. Loss gone.", "rb-optic"),
        ("Customer {c}: BGP session flapping", "The customer's session resets every few hours with hold timer expired. Their CPE is single-homed.", "CPE was rebooting on a watchdog; customer replaced it.", "rb-customer-port"),
        ("Request: add prefixes to {c}'s prefix-list", "Customer has a new /23 and asks us to accept it. LOA attached, RPKI ROA valid.", "Prefix-list updated in the next change window.", ""),
        ("High latency {site} to NYC in the evening", "Latency rises by 20 ms for two hours each evening on the transatlantic path.", "Plane A ran above 90% at peak; traffic engineering spread it across both planes.", "rb-congestion"),
        ("Customer {c}: asks whether last night's maintenance touched their wave", "They saw a 40 ms hit at 02:14 UTC.", "Yes: the provider's planned work caused a protection switch. Shared the maintenance notice.", ""),
        ("Monitoring: SNMP timeouts from a device at {site}", "Polls time out for a few minutes several times a day.", "Control-plane policer too tight for the new poller; raised it.", ""),
        ("Customer {c}: suspected DDoS, traffic graphs spiking", "Inbound traffic five times normal for ten minutes, mostly UDP.", "Short NTP amplification burst; offered DDoS Shield.", "rb-ddos"),
        ("Field: dirty connector found during audit at {site}", "Rx level 4 dB lower than its neighbours on the same span.", "Cleaned; level back to -2.9 dBm.", "rb-optic"),
    ];
    for n in 0..64 {
        let (title, body, resolution, runbook) = kinds[n % kinds.len()];
        let c = &w.customers[(n * 7) % w.customers.len()];
        let opened = now - (n as i64 * 9 * 3600 + rng.below(20000) as i64);
        let open = n < 9;
        h.tickets.push(json!({"id": format!("TKT-{}", 63000 + n * 7), "title": title.replace("{c}", &c.name).replace("{site}", &c.site.to_uppercase()), "body": body,
            "status": if open { if n % 3 == 0 { "open" } else { "in progress" } } else { "closed" }, "priority": (["P3", "P2", "P3", "P4"][n % 4]), "assignee": PEOPLE[n % PEOPLE.len()].0,
            "queue": if n % 5 == 0 { "Field" } else { "NOC" }, "device": w.devices[c.device].id, "customer": c.id, "opened": iso(opened), "updated": iso(opened + 3600 * (1 + n as i64 % 30)),
            "resolution": if open { "" } else { resolution }, "runbook": if open { "" } else { runbook }}));
    }

    // On call: a primary and a secondary per team, in 12-hour shifts.
    let teams = ["Backbone NOC", "IP Edge", "Optical", "Security", "Platform"];
    let shift0 = now - now % 43200 - 2 * 43200;
    for s in 0..8 {
        for (ti, team) in teams.iter().enumerate() {
            for (ri, role) in ["primary", "secondary"].iter().enumerate() {
                let p = PEOPLE[(s as usize + ti * 3 + ri * 5) % PEOPLE.len()].0;
                h.oncall.push(json!({"id": format!("oc:{team}:{s}:{role}"), "team": team, "person": p, "role": role, "starts": iso(shift0 + s * 43200), "ends": iso(shift0 + (s + 1) * 43200),
                    "contact": format!("+44 20 7946 0{:03}", 100 + (ti * 13 + ri * 7 + s as usize) % 800)}));
            }
        }
    }
    h
}

pub fn source_rows(now: i64) -> Vec<Value> {
    let s = |id: &str, name: &str, kind: &str, table: &str, via: &str, retention: &str, note: &str, snippet: &str| {
        json!({"id": id, "name": name, "kind": kind, "table_name": table, "via": via, "status": "simulated", "rate": 0.0, "lag_s": 0.0, "last_seen": iso(now), "retention": retention, "note": note, "snippet": snippet})
    };
    vec![
        s("syslog", "Device syslog", "logs", "netops_syslog", "OpenTelemetry Collector, syslog receiver, OTLP out", "7 days",
          "In production this is a Collector with the syslog receiver writing OTLP to /v1/logs. Here the simulator writes the rows.",
          "receivers:\n  syslog:\n    udp: { listen_address: 0.0.0.0:514 }\n    protocol: rfc5424\nexporters:\n  otlphttp:\n    endpoint: https://CHLOEDB/v1\n    headers: { authorization: \"Bearer ${CHLOEDB_KEY}\" }\nservice:\n  pipelines:\n    logs: { receivers: [syslog], exporters: [otlphttp] }"),
        s("link-metrics", "Interface counters", "metrics", "netops_link_metrics", "A minute rollup from Prometheus (snmp_exporter)", "3 days",
          "ChloeDB keeps the minute rollups an incident needs; the full series stay in Prometheus.",
          "# every minute, per link\nquery: avg_over_time(ifHCInOctets_rate[1m]) / ifHighSpeed\nwrite: POST /v1/write {ops: [{op: upsert, table: netops_link_metrics, doc: {...}}]}"),
        s("device-metrics", "Device health", "metrics", "netops_device_metrics", "A minute rollup from Prometheus", "3 days", "CPU, memory, temperature, BGP updates, firewall sessions.", ""),
        s("service-metrics", "Service probes", "metrics", "netops_service_metrics", "A minute rollup from Prometheus (blackbox_exporter)", "7 days", "Requests, errors and latency per service.", ""),
        s("flows", "Flow summaries", "flows", "netops_flows", "goflow2 to Kafka, read with /v1/stream", "3 days",
          "Top talkers a minute, not raw flows: raw NetFlow belongs in a column store.",
          "POST /v1/stream {source: \"kafka:flows-1m\", partition: 0, offset: N, rows: [...]}   # offsets committed with the rows"),
        s("inventory", "Inventory (NetBox)", "records", "netops_devices, netops_links, netops_circuits", "follow-postgres on NetBox's database", "kept",
          "Devices, cables and circuits follow NetBox live by logical replication; the links table is an edge table.",
          "chloedb-server follow-postgres --from postgres://netbox@db/netbox --tables dcim_device,dcim_cable,circuits_circuit"),
        s("changes", "Changes and tickets", "records", "netops_changes, netops_tickets", "A pull from the ITSM tool every five minutes", "kept", "Change windows, scopes and outcomes; ticket history for similar-incident search.", ""),
        s("configs", "Device configurations", "records", "netops_configs", "Oxidized, pushed on every commit", "kept", "Every version, so a line can be traced to the change that brought it.", ""),
        s("alerts-in", "Alerts", "events", "netops_alerts", "Rules evaluated here as SQL; Alertmanager webhooks land in the same table", "kept", "Each rule is a query over the tables above.", "POST /v1/apps/netops/v1/alerts/webhook   # Alertmanager's JSON"),
        s("advisories", "Vendor advisories", "records", "netops_advisories", "chloedb-crawl over vendor advisory pages", "kept", "Joined with the inventory's software versions.", ""),
    ]
}

/// A time as a carrier's notice writes it.
pub fn notice_time(t: i64) -> String {
    format!("{} UTC", iso(t)[..16].replace('T', " "))
}

/// A carrier's maintenance notice as the email it arrives as.
pub fn notice_text(provider: &str, reference: &str, circuit: &str, start: i64, end: i64, expected: &str, reason: &str, emergency: bool) -> (String, String) {
    let subject = format!("{} Maintenance Notification {reference} - {provider}", if emergency { "EMERGENCY" } else { "Planned" });
    let raw = format!(
        "From: maintenance@{}.example\nSubject: {subject}\n\nDear Customer,\n\n{provider} will carry out {} work on its network that affects a service you take from us.\n\nReference: {reference}\nService ID: {circuit}\nStart: {}\nEnd: {}\nExpected impact: {expected}\nReason: {reason}\n\nYour service may be affected at any point in the window. We apologise for the inconvenience.\n\n{provider} Change Management",
        provider.to_lowercase().replace(' ', ""), if emergency { "emergency" } else { "planned" }, notice_time(start), notice_time(end));
    (subject, raw)
}

/// Carriers' notices: two that take both planes of one path in the same
/// night, one that overlaps a planned change on the other plane's router,
/// an emergency on the busiest span, a quiet one and one that is done.
pub fn maintenance_rows(w: &World, now: i64) -> Vec<Value> {
    let hour = now - now % 3600;
    let by_id = |id: &str| w.link_by_id(id);
    let at = |dev: &str, skip: usize| w.links.iter().enumerate().filter(|(_, l)| l.kind == "backbone" && l.live && l.documented && (w.devices[l.a].id == dev || w.devices[l.b].id == dev)).map(|(i, _)| i).nth(skip);
    let lon_nyc = by_id("cr1-lon--cr1-nyc");
    let plan: Vec<(Option<usize>, i64, i64, &str, &str, &str, &str, &str, i64, bool)> = vec![
        (lon_nyc, 20 * 3600, 4 * 3600, "outage", "Outage of up to 4 hours", "Subsea cable repair: replacement of a repeater on the transatlantic segment.", "new", "", 5 * 3600, false),
        (lon_nyc.and_then(|i| w.links[i].sibling), 22 * 3600, 3 * 3600, "at risk", "Service at risk; two interruptions of up to 15 minutes", "Software upgrade of optical line amplifiers on the terrestrial backhaul.", "new", "", 2 * 3600, false),
        (at("cr2-fra", 0), 26 * 3600, 4 * 3600, "outage", "Outage of up to 3 hours", "Fibre re-route for road works; the span is spliced onto a new duct.", "acknowledged", "L. Chen", 2 * 86400, false),
        (at("cr1-mad", 1).or(at("cr1-mad", 0)), 5 * 86400, 6 * 3600, "outage", "Outage of up to 6 hours", "Planned replacement of a DWDM line card.", "acknowledged", "A. Novak", 4 * 86400, false),
        (at("cr1-ash", 0), -30 * 3600, 4 * 3600, "outage", "Outage of up to 2 hours", "Fibre splice after third-party damage.", "completed", "M. Okafor", 6 * 86400, false),
        (by_id("cr1-ams--cr1-fra").or(at("cr1-ams", 0)), 6 * 3600, 2 * 3600, "outage", "Outage of up to 90 minutes", "Emergency repair: a degraded amplifier must be replaced before it fails.", "new", "", 2400, true),
    ];
    plan.into_iter()
        .enumerate()
        .filter_map(|(n, (li, start, len, impact, expected, reason, status, acked, received, emergency))| {
            let l = &w.links[li?];
            let reference = format!("{}-{}", if emergency { "EMR" } else { "PWIC" }, 48213 + n * 371);
            let (subject, raw) = notice_text(l.provider, &reference, &l.circuit, hour + start, hour + start + len, expected, reason, emergency);
            Some(json!({"id": format!("MNT-{}", 2301 + n), "provider": l.provider, "reference": reference, "circuit": l.circuit, "link": l.id, "a_site": w.devices[l.a].site, "z_site": w.devices[l.b].site,
                "window_start": iso(hour + start), "window_end": iso(hour + start + len), "impact": impact, "expected": expected, "status": status, "received": iso(now - received),
                "subject": subject, "raw": raw, "acked_by": acked, "change_id": ""}))
        })
        .collect()
}

/// Notes the last shift left.
pub fn handover_rows(w: &World, now: i64) -> Vec<Value> {
    let c = &w.customers[0];
    let notes = [
        ("A. Novak", 9 * 3600, "br2-ams: the optic toward cr2-ams was cleaned yesterday and its receive level is still about 1 dB under its neighbours. If input errors fire there again, swap the optic (spare in AMS cage 4) rather than cleaning it a second time.".to_string()),
        ("J. Vermeer", 7 * 3600, "Colt has the LON-PAR span on a longer protection route until Friday: expect about 3 ms more latency on plane B. It is not a fault, and their ticket is open.".to_string()),
        ("R. Banerjee", 5 * 3600, format!("{} has an audit this week. Call their NOC before any work on {}, even inside an approved window.", c.name, w.devices[c.device].id)),
    ];
    notes.iter().enumerate().map(|(n, (who, ago, body))| json!({"id": format!("note-{}", n + 1), "ts": iso(now - ago), "kind": "note", "author": who, "body": body, "done": false})).collect()
}

/// More planned changes, five to nine days ahead: what the rest of the team
/// files while the network runs.
pub fn more_changes(w: &World, now: i64) -> Vec<Value> {
    let mut rng = Rng::new(now as u64 / 600);
    let routers: Vec<usize> = w.devices.iter().enumerate().filter(|(_, d)| d.role != "leaf").map(|(i, _)| i).collect();
    (0..4)
        .map(|k| {
            let n = (now / 600) as usize + k;
            let start = now + (5 + k as i64) * 86400 + rng.below(8) as i64 * 3600;
            let start = start - start % 1800;
            let (title, kind, risk, description) = CHANGE_KINDS[(n * 3 + n / 7) % CHANGE_KINDS.len()];
            let d = &w.devices[routers[rng.below(routers.len())]];
            let devices = vec![d.id.clone()];
            json!({"id": format!("CHG-{}", 10_000 + (now / 600) % 80_000 * 4 + k as i64), "title": title.replace("{dev}", &d.id).replace("{os}", d.os), "description": description, "kind": kind, "risk": risk,
                "status": if k % 2 == 0 { "approved" } else { "scheduled" }, "window_start": iso(start), "window_end": iso(start + 2 * 3600), "owner": PEOPLE[rng.below(PEOPLE.len())].0, "approver": PEOPLE[(n + 3) % PEOPLE.len()].0,
                "devices": devices, "device_nums": nums(w, &devices), "sites": [d.site], "services": Vec::<String>::new(), "ticket": format!("TKT-{}", 70_000 + n % 9000),
                "rollback_plan": "Roll back to the previous commit; confirm adjacencies and sessions return within five minutes.", "diff": "", "outcome": ""})
        })
        .collect()
}

/// One more carrier's notice, three days ahead on a backbone circuit.
pub fn more_maintenance(w: &World, now: i64) -> Vec<Value> {
    let mut rng = Rng::new(now as u64 / 600 + 9);
    let backbone: Vec<&crate::world::Link> = w.links.iter().filter(|l| l.kind == "backbone" && l.live && l.documented).collect();
    let l = backbone[rng.below(backbone.len())];
    let start = now - now % 3600 + 3 * 86400 + rng.below(6) as i64 * 3600;
    let reference = format!("PWIC-{}", 50_000 + (now / 600) % 40_000);
    let expected = "Outage of up to 3 hours";
    let (subject, raw) = notice_text(l.provider, &reference, &l.circuit, start, start + 4 * 3600, expected, "Planned fibre maintenance on the span.", false);
    vec![json!({"id": format!("MNT-{}", 4000 + (now / 600) % 90_000), "provider": l.provider, "reference": reference, "circuit": l.circuit, "link": l.id, "a_site": w.devices[l.a].site, "z_site": w.devices[l.b].site,
        "window_start": iso(start), "window_end": iso(start + 4 * 3600), "impact": "outage", "expected": expected, "status": "new", "received": iso(now), "subject": subject, "raw": raw, "acked_by": "", "change_id": ""})]
}
