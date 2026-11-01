//! What a cluster's plan is on any cloud ([`crate::aws`] for EKS,
//! [`crate::digitalocean`] for DOKS): its name, where its copies go on the
//! nodes, each copy's share of a node, the StrataCluster's spec filled in
//! for that share, what to watch for, and the manifests that run it.

use serde_json::{json, Value as Json};

use crate::operator::Spec;

pub mod clusters;
pub mod install;

/// The image the cluster runs when the plan names none.
pub const IMAGE: &str = "ghcr.io/chloetech-ai/strata-server:latest";

/// The operator's manifests (its namespace, account, role and
/// Deployment), whose image the plan's replaces.
const OPERATOR_YAML: &str = include_str!("../../deploy/k8s/operator/operator.yaml");

/// A cluster's name: the cloud's cluster is `strata-<name>`.
pub fn check_name(n: &str) -> Result<(), String> {
    let ok = !n.is_empty() && n.len() <= 40 && n.starts_with(|c: char| c.is_ascii_lowercase()) && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !n.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(format!("{n:?} is not a cluster name: 1 to 40 lower-case letters, digits and hyphens, starting with a letter"))
    }
}

/// Where the copies go: copy `c` of shard `s` on node `(s + c) % nodes`,
/// so a shard's copies are on different nodes while there are enough.
pub fn layout(shards: u32, replicas: u32, nodes: u32) -> Vec<Vec<(u32, u32)>> {
    let mut on = vec![Vec::new(); nodes.max(1) as usize];
    for s in 0..shards {
        for c in 0..replicas {
            on[((s + c) % nodes.max(1)) as usize].push((s, c));
        }
    }
    on
}

/// What a node offers the cluster's pods.
pub struct Node {
    pub vcpu: u32,
    /// The memory its pods may have, GiB.
    pub alloc_gib: f64,
    /// The CPU it keeps for itself and the pods every node runs (the
    /// cloud's networking and agents), which Strata's pods cannot request.
    pub system_cpu: f64,
}

/// The CPU requests of the pods besides the copies that may land on the
/// busiest node: a coordinator (250m), the operator (50m), and one of
/// the cluster's two CoreDNS (100m).
const COORDINATOR_CPU: f64 = 0.25;
const OPERATOR_CPU: f64 = 0.05;
const COREDNS_CPU: f64 = 0.1;

/// A cluster fitted to its nodes.
pub struct Fit {
    /// The StrataCluster's spec, filled in.
    pub cluster: Json,
    pub shards: u32,
    pub replicas: u32,
    pub coordinators: u32,
    /// The copies on each node.
    pub on: Vec<Vec<(u32, u32)>>,
    /// The copies on the busiest node.
    pub per_node: u32,
    pub alloc_gib: f64,
    pub copy_gib: f64,
    pub copy_cpu: f64,
    pub coordinator_gib: f64,
}

/// Fills in `cluster` (a StrataCluster's spec, in part) for `nodes` of
/// `node`: the shards (2), copies of each (3), coordinators (1), the image,
/// 10,000 demo rows without a schema, each copy's volume of `volume_gb`
/// in `storage_class`, and each copy's requests, a little under its share
/// of the busiest node. `Err` when the spec is not one the operator takes.
pub fn fit(cluster: &Json, nodes: u32, node: &Node, volume_gb: u32, storage_class: &str) -> Result<Fit, String> {
    let mut cluster = match cluster {
        Json::Object(_) => cluster.clone(),
        Json::Null => json!({}),
        _ => return Err("cluster is a StrataCluster's spec (an object)".into()),
    };
    let field = |c: &Json, k: &str, d: u64| c[k].as_u64().unwrap_or(d) as u32;
    let (shards, replicas, coordinators) = (field(&cluster, "shards", 2), field(&cluster, "replicas", 3), field(&cluster, "coordinators", 1));
    let on = layout(shards, replicas, nodes);
    let per_node = on.iter().map(Vec::len).max().unwrap_or(0) as u32;
    let alloc = node.alloc_gib;
    // The coordinators and the operator get a fixed share; the copies
    // split the rest of the busiest node.
    let (coordinator_gib, operator_gib) = (0.5_f64.min(alloc / 8.0), 0.25_f64);
    let spare = alloc - (f64::from(coordinators) * coordinator_gib + operator_gib) / f64::from(nodes.max(1));
    let copy_gib = if per_node > 0 { spare / f64::from(per_node) } else { spare };
    // The coordinators, the operator and CoreDNS go where the scheduler
    // finds room, so the busiest node is counted with its share of the
    // coordinators (rounded up) and both of the others.
    let beside = f64::from(coordinators.div_ceil(nodes.max(1))) * COORDINATOR_CPU + OPERATOR_CPU + COREDNS_CPU;
    let copy_cpu = (f64::from(node.vcpu) - node.system_cpu - beside).max(0.0) / f64::from(per_node.max(1));
    // Requests a little under each copy's share, so they fit.
    let mib = |g: f64| format!("{}Mi", (g * 1024.0 * 0.9).floor().max(64.0) as u64);
    let milli = |c: f64| format!("{}m", (c * 1000.0 * 0.9).floor().max(50.0) as u64);
    let o = cluster.as_object_mut().expect("an object");
    o.entry("shards").or_insert(json!(shards));
    o.entry("replicas").or_insert(json!(replicas));
    o.entry("coordinators").or_insert(json!(coordinators));
    o.entry("image").or_insert(json!(IMAGE));
    if !o.contains_key("schema") {
        o.entry("demo").or_insert(json!(10_000));
    }
    let storage = o.entry("storage").or_insert(json!({}));
    if let Some(s) = storage.as_object_mut() {
        s.entry("size").or_insert(json!(format!("{volume_gb}Gi")));
        s.entry("storageClassName").or_insert(json!(storage_class));
    }
    o.entry("resources").or_insert(json!({"requests": {"cpu": milli(copy_cpu), "memory": mib(copy_gib)}}));
    o.entry("coordinatorResources").or_insert(json!({"requests": {"cpu": "250m", "memory": mib(coordinator_gib)}}));
    let spec: Spec = serde_json::from_value(cluster.clone()).map_err(|e| format!("cluster: {e}"))?;
    spec.check().map_err(|e| format!("cluster: {e}"))?;
    Ok(Fit { cluster, shards, replicas, coordinators, on, per_node, alloc_gib: alloc, copy_gib, copy_cpu, coordinator_gib })
}

impl Fit {
    /// What to watch for on any cloud: a shard's copies sharing a node, a
    /// single copy, too little memory a copy.
    pub fn warnings(&self, nodes: u32) -> Vec<String> {
        let mut w = Vec::new();
        if self.replicas > nodes {
            w.push(format!("a shard has {} copies and there are {nodes} nodes: some copies of a shard share a node, and losing it loses them", self.replicas));
        } else if self.replicas == 1 {
            w.push("one copy a shard: losing a node loses its shards until the node comes back".into());
        }
        if self.copy_cpu < 0.5 {
            w.push(format!("each copy may request {:.2} CPU after what the nodes keep for themselves: a larger machine or more nodes", self.copy_cpu * 0.9));
        }
        if self.copy_gib < 1.0 {
            w.push(format!("each copy gets {:.1} GiB of memory: search keeps its segments and vectors in the page cache, so a larger machine or more nodes", self.copy_gib));
        }
        w
    }

    /// The plan's `layout`: the copies on each node, and each one's share.
    pub fn layout_json(&self, pods: u32, max_pods: u32) -> Json {
        json!({
            "nodes": self.on.iter().enumerate().map(|(i, c)| json!({"node": i, "copies": c.iter().map(|(s, c)| json!({"shard": s, "copy": c})).collect::<Vec<_>>()})).collect::<Vec<_>>(),
            "copies_per_node": self.per_node,
            "pods_per_node": pods,
            "max_pods": max_pods,
            "allocatable_gib": self.alloc_gib,
            "copy_memory_gib": self.copy_gib,
            "copy_cpu": self.copy_cpu,
        })
    }
}

/// The operator's manifests, running `image`.
pub fn operator_yaml(image: &str) -> Result<String, String> {
    const DEV: &str = "image: strata-server:dev";
    if !OPERATOR_YAML.contains(DEV) {
        return Err("the operator's manifests name no image to replace".into());
    }
    Ok(OPERATOR_YAML.replace(DEV, &format!("image: {image}")))
}

/// The namespace a cluster made from the cloud view runs in.
pub const NAMESPACE: &str = "strata";

/// The cluster's objects: its namespace, the storage class when the cloud
/// has none to use (`storage_class`), and the StrataCluster.
pub fn cluster_objects(name: &str, spec: &Json, storage_class: Option<Json>) -> Vec<Json> {
    let mut docs = vec![json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": NAMESPACE}})];
    docs.extend(storage_class);
    docs.push(json!({"apiVersion": "strata.chloetech.ai/v1alpha1", "kind": "StrataCluster", "metadata": {"name": name, "namespace": NAMESPACE}, "spec": spec}));
    docs
}

/// The cluster's manifests ([`cluster_objects`]): YAML documents written
/// as JSON (which YAML reads), so a schema goes in whole.
pub fn cluster_yaml(what: &str, objects: &[Json]) -> String {
    let mut out = format!("# {what}: kubectl apply -f this file\n");
    for d in objects {
        out.push_str("---\n");
        out.push_str(&serde_json::to_string_pretty(d).expect("JSON"));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_of_a_shard_go_on_different_nodes() {
        assert_eq!(layout(2, 3, 3), vec![vec![(0, 0), (1, 2)], vec![(0, 1), (1, 0)], vec![(0, 2), (1, 1)]]);
        let on = layout(4, 2, 4);
        assert!(on.iter().all(|n| n.len() == 2));
        for n in &on {
            assert_ne!(n[0].0, n[1].0, "two copies of a shard on one node: {on:?}");
        }
        assert_eq!(layout(1, 3, 1), vec![vec![(0, 0), (0, 1), (0, 2)]]);
    }

    #[test]
    fn a_cluster_is_fitted_to_its_nodes() {
        assert!(check_name("search-2").is_ok());
        for bad in ["", "Search", "2search", "search-", &"s".repeat(41)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        // 4 shards of 2 copies on 4 nodes of 8 vCPUs and 57.5 GiB: 2 copies a node.
        let f = fit(&json!({"shards": 4, "replicas": 2}), 4, &Node { vcpu: 8, alloc_gib: 57.5, system_cpu: 0.5 }, 50, "do-block-storage").unwrap();
        assert_eq!((f.shards, f.replicas, f.coordinators, f.per_node), (4, 2, 1, 2));
        // Each node keeps half a core, and a coordinator, the operator and
        // CoreDNS may run beside the copies.
        assert!((f.copy_gib - (57.5 - (0.5 + 0.25) / 4.0) / 2.0).abs() < 1e-9 && (f.copy_cpu - (8.0 - 0.5 - 0.4) / 2.0).abs() < 1e-9);
        assert_eq!(f.cluster["storage"], json!({"size": "50Gi", "storageClassName": "do-block-storage"}));
        assert_eq!((f.cluster["demo"].as_u64(), f.cluster["image"].as_str()), (Some(10_000), Some(IMAGE)));
        assert_eq!(f.warnings(4), Vec::<String>::new());
        // What was given is kept.
        let f = fit(&json!({"replicas": 1, "demo": 50, "storage": {"size": "5Gi"}}), 2, &Node { vcpu: 2, alloc_gib: 1.0, system_cpu: 0.25 }, 10, "gp3").unwrap();
        assert_eq!(f.cluster["storage"], json!({"size": "5Gi", "storageClassName": "gp3"}));
        assert_eq!(f.cluster["demo"], 50);
        let w = f.warnings(2).join("; ");
        assert!(w.contains("one copy a shard") && w.contains("GiB of memory"), "{w}");
        assert!(fit(&json!([]), 2, &Node { vcpu: 2, alloc_gib: 4.0, system_cpu: 0.25 }, 10, "gp3").is_err());
        let y = cluster_yaml("a test", &cluster_objects("search", &json!({"shards": 1}), None));
        assert!(y.starts_with("# a test: kubectl apply -f this file\n---\n") && y.matches("---\n").count() == 2, "{y}");
    }
}
