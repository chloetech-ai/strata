//! The plan of a Strata cluster on DOKS (DigitalOcean Kubernetes): where
//! its copies go on the nodes, what it costs, what to watch for, the
//! request that makes the cluster, and the StrataCluster the operator
//! keeps on it.
//!
//! Nothing here calls DigitalOcean: it works from a region's [`Catalog`].

use serde::Deserialize;
use serde_json::{json, Value as Json};

use super::{Catalog, HOURS_A_MONTH, MAX_PODS};
use crate::aws::catalog::Machine;
use crate::cloud::{check_name, cluster_yaml, fit, operator_yaml, Node, IMAGE};

/// The storage class DOKS makes: block storage volumes that grow in place
/// and are deleted with their claims.
pub const STORAGE_CLASS: &str = "do-block-storage";

/// The pods every node runs besides Strata's: Cilium, the CSI driver's
/// node pod, the metrics agent and the Konnectivity agent.
const NODE_PODS: u32 = 4;

/// The cluster's own pods: two CoreDNS, Cilium's operator.
const SYSTEM_PODS: u32 = 3;

/// What is asked for: `POST /v1/admin/digitalocean/plan`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Plan {
    /// The cluster's name: the DOKS cluster is `strata-<name>`.
    pub name: String,
    pub region: String,
    /// The nodes' size slug.
    pub machine: String,
    pub nodes: u32,
    /// Each copy's volume, GiB.
    pub volume_gb: u32,
    /// A highly available control plane ($40 a month; the standard one is
    /// free).
    pub ha: bool,
    /// The Kubernetes version (empty: the newest DOKS offers).
    pub version: String,
    /// The StrataCluster's spec, in part, as for EKS.
    pub cluster: Json,
}

impl Default for Plan {
    fn default() -> Plan {
        Plan { name: "search".into(), region: "nyc3".into(), machine: "m-4vcpu-32gb".into(), nodes: 3, volume_gb: 10, ha: false, version: String::new(), cluster: json!({}) }
    }
}

impl Plan {
    pub fn cluster_name(&self) -> String {
        format!("strata-{}", self.name)
    }
}

/// The memory a DOKS node's pods may have, GiB: DigitalOcean's table of
/// allocatable memory by node memory
/// (<https://docs.digitalocean.com/products/kubernetes/details/limits/>),
/// between its rows by line, less half a GiB for the node's own pods.
pub fn allocatable_gib(memory_gib: f64) -> f64 {
    const TABLE: [(f64, f64); 15] = [
        (2.0, 1.0),
        (3.0, 1.66),
        (4.0, 2.5),
        (8.0, 6.0),
        (16.0, 13.0),
        (32.0, 28.0),
        (48.0, 43.0),
        (64.0, 58.0),
        (96.0, 88.5),
        (128.0, 119.5),
        (160.0, 151.0),
        (192.0, 182.0),
        (256.0, 243.98),
        (384.0, 369.42),
        (512.0, 494.86),
    ];
    let a = if memory_gib <= TABLE[0].0 {
        memory_gib * TABLE[0].1 / TABLE[0].0
    } else if let Some(w) = TABLE.windows(2).find(|w| memory_gib <= w[1].0) {
        let ((m0, a0), (m1, a1)) = (w[0], w[1]);
        a0 + (memory_gib - m0) * (a1 - a0) / (m1 - m0)
    } else {
        let (m, a) = TABLE[TABLE.len() - 1];
        memory_gib * a / m
    };
    (a - 0.5).max(0.0)
}

/// Whether `m` is a v5 size: billed by the second without a monthly cap,
/// with its public IPv4 address billed apart.
fn v5(m: &Machine) -> bool {
    m.processor.as_deref().is_some_and(|d| d.starts_with("v5"))
}

/// The plan: `Err` when it cannot be made.
pub fn plan(p: &Plan, cat: &Catalog) -> Result<Json, String> {
    check_name(&p.name)?;
    if p.region != cat.region {
        return Err(format!("the sizes are those of {}, not {}", cat.region, p.region));
    }
    let m = cat.machines.iter().find(|m| m.name == p.machine).ok_or_else(|| format!("{} is not a size a DOKS node may be in {} (no GPUs, nothing legacy)", p.machine, p.region))?;
    if !(1..=100).contains(&p.nodes) {
        return Err("nodes is 1 to 100".into());
    }
    if !(1..=16_384).contains(&p.volume_gb) {
        return Err("volume_gb is 1 to 16,384 (a volume's size)".into());
    }
    let version = if p.version.is_empty() { cat.versions.first().cloned().ok_or("DOKS offers no Kubernetes version")? } else { p.version.clone() };
    if !cat.versions.contains(&version) {
        return Err(format!("DOKS offers Kubernetes {}, not {version}", cat.versions.join(", ")));
    }
    let alloc = allocatable_gib(m.memory_gib);
    let f = fit(&p.cluster, p.nodes, &Node { vcpu: m.vcpu, alloc_gib: alloc }, p.volume_gb, STORAGE_CLASS)?;

    let mut warnings = f.warnings(p.nodes);
    if let (Some(limit), Some(running)) = (cat.droplet_limit, cat.droplets) {
        if running + p.nodes > limit {
            warnings.push(format!(
                "the account may run {limit} droplets and runs {running}: {} more nodes would not start. Ask DigitalOcean for a higher droplet limit (Settings, Team, Droplet limit)",
                p.nodes
            ));
        }
    } else {
        warnings.push("the account's droplet limit is not known".into());
    }
    let pods = f.per_node + NODE_PODS + (SYSTEM_PODS + f.coordinators + 1).div_ceil(p.nodes);
    if pods > MAX_PODS {
        warnings.push(format!("a node would run {pods} pods, and DOKS lets a node run {MAX_PODS}: some copies would not start"));
    }
    if m.burstable {
        warnings.push(format!("{} has shared CPUs: under steady queries a neighbour's load can slow it, so timed benchmarks should run on dedicated CPUs (General Purpose, CPU- or Memory-Optimized)", m.name));
    }
    if v5(m) {
        warnings.push(format!("{} is a v5 size: billed by the second without a monthly cap, and its public IPv4 address is billed apart (not counted)", m.name));
    }
    if m.usd_hour.is_none() {
        warnings.push(format!("{} has no price here", m.name));
    }

    let cost = cost(p, m, cat, f.shards * f.replicas);
    let image = f.cluster["image"].as_str().unwrap_or(IMAGE).to_string();
    let name = p.cluster_name();
    let r = &p.region;
    let request = json!({
        "name": name,
        "region": r,
        "version": version,
        "ha": p.ha,
        "auto_upgrade": false,
        "surge_upgrade": true,
        "tags": ["strata"],
        "node_pools": [{"name": "strata", "size": m.name, "count": p.nodes, "tags": ["strata"], "labels": {"strata.chloetech.ai/cluster": name}}],
    });
    let commands = [
        format!("doctl kubernetes cluster create {name} --region {r} --version {version} --ha={} --auto-upgrade=false --node-pool \"name=strata;size={};count={};tag=strata\" --wait", p.ha, m.name, p.nodes),
        format!("kubectl apply -f {name}-operator.yaml"),
        "kubectl wait --for condition=established --timeout=180s crd/strataclusters.strata.chloetech.ai".to_string(),
        format!("kubectl apply -f {name}-cluster.yaml"),
        format!("kubectl -n strata get strata {} -w", p.name),
    ];
    // The volumes are the CSI driver's: deleted with their claims, or with
    // the cluster's `--dangerous`.
    let teardown = [format!("kubectl delete -f {name}-cluster.yaml"), "kubectl -n strata delete pvc --all --wait".to_string(), format!("doctl kubernetes cluster delete {name} --dangerous --force")];
    Ok(json!({
        "provider": "digitalocean",
        "stack": name,
        "cluster_name": name,
        "region": r,
        "version": version,
        "machine": m,
        "layout": f.layout_json(pods, MAX_PODS),
        "quota": {"droplets": cat.droplet_limit, "running": cat.droplets, "needs": p.nodes},
        "cost": cost,
        "warnings": warnings,
        "manifests": {"operator": operator_yaml(&image)?, "cluster": cluster_yaml("a Strata cluster on DOKS, from strata-server's cloud view", &p.name, &f.cluster, None)},
        "commands": commands,
        "teardown": teardown,
        "cluster": f.cluster,
        "request": request,
    }))
}

/// What the plan costs, an hour and a month, line by line: the nodes, the
/// control plane, the volumes.
pub fn cost(p: &Plan, m: &Machine, cat: &Catalog, copies: u32) -> Json {
    let n = f64::from(p.nodes);
    // v5 sizes have no monthly cap: a month is all its hours.
    let node_month_hours = if v5(m) { crate::aws::stack::HOURS_A_MONTH } else { HOURS_A_MONTH };
    let gb = f64::from(copies) * f64::from(p.volume_gb);
    let ha = if p.ha { cat.ha_month } else { 0.0 };
    let lines: [(String, Option<f64>, Option<f64>); 3] = [
        (format!("{} × {}", p.nodes, m.name), m.usd_hour.map(|h| h * n), m.usd_hour.map(|h| h * n * node_month_hours)),
        (if p.ha { "the control plane, highly available".to_string() } else { "the control plane (free)".to_string() }, Some(ha / HOURS_A_MONTH), Some(ha)),
        (format!("{gb} GiB of volumes ({copies} copies × {} GiB)", p.volume_gb), Some(gb * cat.volume_gb_month / HOURS_A_MONTH), Some(gb * cat.volume_gb_month)),
    ];
    let hour: f64 = lines.iter().filter_map(|l| l.1).sum();
    let month: f64 = lines.iter().filter_map(|l| l.2).sum();
    let unknown: Vec<&str> = lines.iter().filter(|l| l.1.is_none()).map(|l| l.0.as_str()).collect();
    json!({
        "hour": hour,
        "month": month,
        "lines": lines.iter().map(|(w, h, mo)| json!({"what": w, "hour": h, "month": mo})).collect::<Vec<_>>(),
        "unknown": unknown,
        "nodes": {"on_demand_hour": m.usd_hour.map(|h| h * n), "spot_hour": null},
        "not_counted": "transfer over the nodes' free allowance, load balancers, and snapshots",
    })
}

/// What DigitalOcean is asked to check before a plan is made: whether its
/// size, region and version are offered, and the droplets fit. `account`
/// is the account's status.
pub fn checks(p: &Plan, cat: &Catalog, account: &str) -> Vec<Json> {
    let mut out = Vec::new();
    let mut check = |what: String, ok: bool| out.push(json!({"check": what, "ok": ok}));
    check(format!("the account is {account}"), account == "active");
    check(format!("DOKS runs in {} and takes {} nodes there", p.region, p.machine), cat.region == p.region && cat.machines.iter().any(|m| m.name == p.machine));
    let version = if p.version.is_empty() { cat.versions.first().cloned().unwrap_or_default() } else { p.version.clone() };
    check(format!("DOKS offers Kubernetes {version}"), cat.versions.contains(&version));
    match (cat.droplet_limit, cat.droplets) {
        (Some(limit), Some(running)) => check(format!("{} nodes fit the droplet limit: {limit} allowed, {running} running", p.nodes), running + p.nodes <= limit),
        _ => check("the droplet limit is known".into(), false),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digitalocean::{machines, options, HA_MONTH, VOLUME_GB_MONTH};

    fn catalog() -> Catalog {
        let (versions, _, k8s) = options(&serde_json::from_str(include_str!("../../tests/fixtures/digitalocean/kubernetes-options.json")).unwrap());
        let ms = machines(&serde_json::from_str(include_str!("../../tests/fixtures/digitalocean/sizes.json")).unwrap(), "nyc3", &k8s);
        Catalog { region: "nyc3".into(), at_ms: 0, machines: ms, versions, droplet_limit: Some(25), droplets: Some(2), volume_gb_month: VOLUME_GB_MONTH, ha_month: HA_MONTH, missing: vec![] }
    }

    #[test]
    fn allocatable_memory_follows_digitaloceans_table() {
        assert_eq!(allocatable_gib(8.0), 5.5);
        assert_eq!(allocatable_gib(64.0), 57.5);
        assert!((allocatable_gib(24.0) - (13.0 + 8.0 * 15.0 / 16.0 - 0.5)).abs() < 1e-9);
        assert!((allocatable_gib(1.0) - 0.0).abs() < 1e-9);
        assert!(allocatable_gib(1024.0) > 980.0);
    }

    #[test]
    fn a_plan_has_its_layout_cost_request_and_manifests() {
        let cat = catalog();
        let p = Plan { machine: "m-8vcpu-64gb".into(), nodes: 4, cluster: json!({"shards": 4, "replicas": 2}), ..Plan::default() };
        let j = plan(&p, &cat).unwrap();
        assert_eq!(j["cluster_name"], "strata-search");
        assert_eq!(j["version"], "1.37.0-do.2");
        assert_eq!(j["layout"]["copies_per_node"], 2);
        assert_eq!(j["layout"]["max_pods"], 110);
        assert_eq!(j["cluster"]["storage"]["storageClassName"], "do-block-storage");
        // 4 × $0.50 an hour, a free control plane, 8 volumes of 10 GiB.
        let hour = j["cost"]["hour"].as_f64().unwrap();
        assert!((hour - (2.0 + 80.0 * 0.10 / 672.0)).abs() < 1e-9, "{}", j["cost"]);
        assert!((j["cost"]["month"].as_f64().unwrap() - (4.0 * 336.0 + 8.0)).abs() < 1e-6, "{}", j["cost"]);
        assert_eq!(j["request"]["node_pools"][0], json!({"name": "strata", "size": "m-8vcpu-64gb", "count": 4, "tags": ["strata"], "labels": {"strata.chloetech.ai/cluster": "strata-search"}}));
        assert_eq!(j["request"]["ha"], false);
        assert!(j["commands"][0].as_str().unwrap().starts_with("doctl kubernetes cluster create strata-search --region nyc3 --version 1.37.0-do.2 --ha=false"), "{}", j["commands"][0]);
        assert!(j["teardown"][2].as_str().unwrap().contains("--dangerous"));
        let manifests = j["manifests"]["cluster"].as_str().unwrap();
        assert!(manifests.contains("\"kind\": \"StrataCluster\"") && !manifests.contains("StorageClass"), "{manifests}");
        assert_eq!(j["warnings"], json!([]), "{}", j["warnings"]);

        // HA costs $40 a month; shared CPUs, the droplet limit and a version
        // DOKS does not offer are said.
        let ha = plan(&Plan { ha: true, ..p.clone() }, &cat).unwrap();
        assert!((ha["cost"]["month"].as_f64().unwrap() - j["cost"]["month"].as_f64().unwrap() - 40.0).abs() < 1e-6);
        let shared = plan(&Plan { machine: "s-4vcpu-8gb".into(), nodes: 24, ..p.clone() }, &cat).unwrap();
        let w = shared["warnings"].to_string();
        assert!(w.contains("shared CPUs") && w.contains("may run 25 droplets and runs 2"), "{w}");
        assert!(plan(&Plan { version: "1.20.0-do.1".into(), ..p.clone() }, &cat).unwrap_err().contains("DOKS offers Kubernetes"));
        assert!(plan(&Plan { machine: "so-8vcpu-64gb".into(), ..p.clone() }, &cat).unwrap_err().contains("is not a size a DOKS node may be"));
        let c = checks(&Plan { nodes: 24, ..p.clone() }, &cat, "active");
        assert_eq!(c.iter().map(|c| c["ok"].as_bool().unwrap()).collect::<Vec<_>>(), [true, true, true, false], "{c:?}");
    }
}
