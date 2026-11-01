//! Strata installed on a cluster the cloud view made, through the
//! cluster's own API: the StrataCluster resource definition, the
//! operator's objects and the cluster's, each applied with server-side
//! apply (so applying again changes nothing); then the StrataCluster's
//! status read back, and on deletion the cluster and its volumes' claims
//! deleted before the cloud deletes the nodes.

use serde_json::{json, Value as Json};

use super::NAMESPACE;
use crate::operator::kube::{Kube, Options};

/// The operator's objects, running `image`: what
/// `deploy/k8s/operator/operator.yaml` holds (a test keeps the two the
/// same), as JSON for the API.
pub fn operator_objects(image: &str) -> Vec<Json> {
    let rule = |groups: &[&str], resources: &[&str], verbs: &[&str]| json!({"apiGroups": groups, "resources": resources, "verbs": verbs});
    let labels = json!({"app.kubernetes.io/name": "strata-operator"});
    vec![
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "strata-system"}}),
        json!({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": "strata-operator", "namespace": "strata-system"}}),
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": {"name": "strata-operator"}, "rules": [
            rule(&["apiextensions.k8s.io"], &["customresourcedefinitions"], &["get", "create", "patch"]),
            rule(&["strata.chloetech.ai"], &["strataclusters"], &["get", "list", "watch", "patch"]),
            rule(&["strata.chloetech.ai"], &["strataclusters/status"], &["get", "patch", "update"]),
            rule(&["apps"], &["statefulsets", "deployments"], &["get", "list", "create", "patch", "delete"]),
            rule(&[""], &["services", "configmaps", "serviceaccounts"], &["get", "create", "patch"]),
            rule(&["rbac.authorization.k8s.io"], &["roles", "rolebindings"], &["get", "create", "patch"]),
            rule(&[""], &["pods"], &["get", "list", "delete"]),
            rule(&[""], &["persistentvolumeclaims"], &["list", "delete", "patch"]),
            rule(&[""], &["secrets"], &["get"]),
        ]}),
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding", "metadata": {"name": "strata-operator"},
            "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "strata-operator"},
            "subjects": [{"kind": "ServiceAccount", "name": "strata-operator", "namespace": "strata-system"}]}),
        json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "strata-operator", "namespace": "strata-system", "labels": labels},
            "spec": {"replicas": 1, "strategy": {"type": "Recreate"}, "selector": {"matchLabels": labels},
                "template": {"metadata": {"labels": labels}, "spec": {"serviceAccountName": "strata-operator", "containers": [{
                    "name": "operator", "image": image, "imagePullPolicy": "IfNotPresent", "args": ["operator"],
                    "env": [{"name": "RUST_LOG", "value": "info"}],
                    "resources": {"requests": {"cpu": "50m", "memory": "32Mi"}, "limits": {"memory": "256Mi"}},
                }]}}}}),
    ]
}

/// Where `o` is in the API, for the kinds the cloud view applies.
pub fn path_of(o: &Json) -> Result<String, String> {
    let s = |k: &Json| k.as_str().map(str::to_string);
    let (Some(api), Some(kind), Some(name)) = (s(&o["apiVersion"]), s(&o["kind"]), s(&o["metadata"]["name"])) else {
        return Err(format!("not an object: {o}"));
    };
    let (plural, namespaced) = match kind.as_str() {
        "Namespace" => ("namespaces", false),
        "ServiceAccount" => ("serviceaccounts", true),
        "ClusterRole" => ("clusterroles", false),
        "ClusterRoleBinding" => ("clusterrolebindings", false),
        "Deployment" => ("deployments", true),
        "CustomResourceDefinition" => ("customresourcedefinitions", false),
        "StorageClass" => ("storageclasses", false),
        "StrataCluster" => ("strataclusters", true),
        k => return Err(format!("the cloud view applies no {k}")),
    };
    let base = if api == "v1" { "/api/v1".to_string() } else { format!("/apis/{api}") };
    match (namespaced, s(&o["metadata"]["namespace"])) {
        (false, _) => Ok(format!("{base}/{plural}/{name}")),
        (true, Some(ns)) => Ok(format!("{base}/namespaces/{ns}/{plural}/{name}")),
        (true, None) => Err(format!("a {kind} needs a namespace: {name}")),
    }
}

/// A client of a new cluster's API at `endpoint`, trusting `ca` (PEM; none
/// for a plain `http://` endpoint in tests), with a bearer `token`.
pub fn client(endpoint: &str, ca: Option<Vec<u8>>, token: &str) -> Result<Kube, String> {
    Kube::new(&Options { api: Some(endpoint.to_string()), token: Some(token.to_string()), ca, ..Options::default() })
}

/// Everything a cluster needs, in the order it is applied: the resource
/// definition first, then the operator, then the cluster's own objects.
pub fn objects(image: &str, cluster: &[Json]) -> Vec<Json> {
    let mut all = vec![crate::operator::crd()];
    all.extend(operator_objects(image));
    all.extend(cluster.iter().cloned());
    all
}

/// Applies `objects` in order, and names each (`Kind/name`). Stops at the
/// first the API refuses; applying again redoes nothing that was done.
pub async fn apply(k: &Kube, objects: &[Json]) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    for o in objects {
        let path = path_of(o)?;
        k.apply(&path, o).await?;
        done.push(format!("{}/{}", o["kind"].as_str().unwrap_or("?"), o["metadata"]["name"].as_str().unwrap_or("?")));
    }
    Ok(done)
}

/// The StrataCluster `name`'s status, `None` while it has none (or is not
/// there yet).
pub async fn status(k: &Kube, name: &str) -> Result<Option<Json>, String> {
    let c = k.get(&format!("/apis/strata.chloetech.ai/v1alpha1/namespaces/{NAMESPACE}/strataclusters/{name}")).await?;
    Ok(c.map(|c| c["status"].clone()).filter(|s| !s.is_null()))
}

/// Deletes the StrataCluster `name` and the claims of the volumes in its
/// namespace, whose disks the cloud would otherwise keep (and bill). The
/// persistent volumes of the namespace still there: the cloud's driver
/// deletes their disks, so deleting the nodes waits for none to be left.
pub async fn teardown(k: &Kube, name: &str) -> Result<usize, String> {
    k.delete(&format!("/apis/strata.chloetech.ai/v1alpha1/namespaces/{NAMESPACE}/strataclusters/{name}")).await?;
    let claims = format!("/api/v1/namespaces/{NAMESPACE}/persistentvolumeclaims");
    for c in k.get(&claims).await?.and_then(|j| j["items"].as_array().cloned()).unwrap_or_default() {
        if let Some(n) = c["metadata"]["name"].as_str() {
            k.delete(&format!("{claims}/{n}")).await?;
        }
    }
    let volumes = k.get("/api/v1/persistentvolumes").await?.and_then(|j| j["items"].as_array().cloned()).unwrap_or_default();
    Ok(volumes.iter().filter(|v| v["spec"]["claimRef"]["namespace"] == NAMESPACE).count())
}

/// A StrataCluster's status in a line: its phase, and how many of its
/// shards' copies answer.
pub fn summary(s: &Json) -> String {
    let phase = s["phase"].as_str().unwrap_or("–");
    let shards = s["shards"].as_array().map(Vec::as_slice).unwrap_or_default();
    let (mut ready, mut copies) = (0, 0);
    for sh in shards {
        for r in sh["servers"].as_array().map(Vec::as_slice).unwrap_or_default() {
            copies += 1;
            if r["up"] == true {
                ready += 1;
            }
        }
    }
    let message = s["message"].as_str().filter(|m| !m.is_empty()).map(|m| format!(": {m}")).unwrap_or_default();
    if copies > 0 {
        format!("{phase}, {ready} of {copies} copies answering{message}")
    } else {
        format!("{phase}{message}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operator_is_what_its_manifests_say() {
        let yaml = include_str!("../../deploy/k8s/operator/operator.yaml");
        let docs: Vec<Json> = serde_yaml::Deserializer::from_str(yaml).map(|d| serde::Deserialize::deserialize(d).expect("a YAML document")).collect();
        assert_eq!(docs, operator_objects("strata-server:dev"));
    }

    #[test]
    fn objects_have_their_paths() {
        let all = objects("ghcr.io/x/strata-server:1", &super::super::cluster_objects("search", &json!({"shards": 1}), None));
        let paths: Vec<String> = all.iter().map(|o| path_of(o).unwrap()).collect();
        assert_eq!(paths, [
            "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/strataclusters.strata.chloetech.ai",
            "/api/v1/namespaces/strata-system",
            "/api/v1/namespaces/strata-system/serviceaccounts/strata-operator",
            "/apis/rbac.authorization.k8s.io/v1/clusterroles/strata-operator",
            "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings/strata-operator",
            "/apis/apps/v1/namespaces/strata-system/deployments/strata-operator",
            "/api/v1/namespaces/strata",
            "/apis/strata.chloetech.ai/v1alpha1/namespaces/strata/strataclusters/search",
        ]);
        assert_eq!(all[5]["spec"]["template"]["spec"]["containers"][0]["image"], "ghcr.io/x/strata-server:1");
        assert!(path_of(&json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "x", "namespace": "y"}})).is_err());
        assert!(path_of(&json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "x"}})).is_err());
        let sc = json!({"apiVersion": "storage.k8s.io/v1", "kind": "StorageClass", "metadata": {"name": "gp3"}});
        assert_eq!(path_of(&sc).unwrap(), "/apis/storage.k8s.io/v1/storageclasses/gp3");
    }

    #[test]
    fn a_status_is_summed_up() {
        let s = json!({"phase": "Scaling", "message": "adding shard 1", "shards": [{"servers": [{"up": true}, {"up": false}]}, {"servers": [{"up": true}]}]});
        assert_eq!(summary(&s), "Scaling, 2 of 3 copies answering: adding shard 1");
        assert_eq!(summary(&json!({"phase": "Creating"})), "Creating");
    }
}
