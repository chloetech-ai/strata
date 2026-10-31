//! The operator on Kubernetes: a small client of the API server (JSON over
//! HTTPS, server-side apply), the objects a `StrataCluster` is made of,
//! and the loop that reconciles every cluster.
//!
//! For cluster `c` in namespace `ns`:
//!
//! - `c-schema`: a ConfigMap holding the first table's schema;
//! - `c-servers`: a headless Service naming every copy
//!   (`c-s<set>-<ordinal>.c-servers.ns.svc`, port 7701 for gRPC, 7700 for
//!   HTTP), published before the copies are ready, since they call each
//!   other while they start;
//! - `c-s<set>`: a StatefulSet per shard, each pod a copy with a volume
//!   (`data`) of its own, kept when the set is deleted with the cluster
//!   and deleted when the set shrinks or its shard leaves the cluster;
//! - `c-coordinator`: the coordinators, a Deployment, behind the Service
//!   `c` (port 7700: the API and the UI).
//!
//! Each is owned by the `StrataCluster`, so deleting it deletes them (the
//! volumes are kept).

use std::time::Duration;

use serde_json::{json, Value as Json};

use super::{Admin, Outcome, Platform, Set, Spec, GROUP, KIND, PLURAL, VERSION};

const SA: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
const MANAGER: &str = "strata-operator";

/// How to reach the API server.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The API server's URL (default: in the cluster, from
    /// `KUBERNETES_SERVICE_HOST`).
    pub api: Option<String>,
    /// A bearer token (default: the pod's service account token).
    pub token: Option<String>,
    /// The API server's CA, PEM (default: the service account's).
    pub ca: Option<Vec<u8>>,
    /// The namespace of the clusters (none: every namespace).
    pub namespace: Option<String>,
    /// How often every cluster is looked at.
    pub every: Duration,
    /// Install or update the CRD when starting.
    pub install_crd: bool,
}

/// A client of the API server.
#[derive(Clone)]
pub struct Kube {
    api: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl Kube {
    pub fn new(o: &Options) -> Result<Kube, String> {
        let api = match &o.api {
            Some(a) => a.trim_end_matches('/').to_string(),
            None => {
                let host = std::env::var("KUBERNETES_SERVICE_HOST").map_err(|_| "not in a Kubernetes pod (no KUBERNETES_SERVICE_HOST): pass --kube-api")?;
                let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
                let host = if host.contains(':') { format!("[{host}]") } else { host };
                format!("https://{host}:{port}")
            }
        };
        let ca = match &o.ca {
            Some(c) => Some(c.clone()),
            None if o.api.is_none() => Some(std::fs::read(format!("{SA}/ca.crt")).map_err(|e| format!("{SA}/ca.crt: {e}"))?),
            None => None,
        };
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(30));
        if let Some(pem) = ca {
            for c in reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| format!("the API server's CA: {e}"))? {
                b = b.add_root_certificate(c);
            }
        }
        Ok(Kube { api, token: o.token.clone(), http: b.build().map_err(|e| e.to_string())? })
    }

    /// The token, read again each time from the service account (it is
    /// rotated) unless one was given.
    fn token(&self) -> Option<String> {
        self.token.clone().or_else(|| std::fs::read_to_string(format!("{SA}/token")).ok().map(|t| t.trim().to_string()))
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<(&str, &Json)>) -> Result<Option<Json>, String> {
        let mut r = self.http.request(method.clone(), format!("{}{path}", self.api));
        if let Some(t) = self.token() {
            r = r.bearer_auth(t);
        }
        if let Some((kind, b)) = body {
            r = r.header("content-type", kind).body(b.to_string());
        }
        let a = r.send().await.map_err(|e| format!("{method} {path}: {e}"))?;
        let status = a.status();
        let j: Json = a.json().await.unwrap_or(Json::Null);
        match status.as_u16() {
            200..=299 => Ok(Some(j)),
            404 => Ok(None),
            _ => Err(format!("{method} {path}: {status}: {}", j["message"].as_str().unwrap_or(&j.to_string()))),
        }
    }

    pub async fn get(&self, path: &str) -> Result<Option<Json>, String> {
        self.send(reqwest::Method::GET, path, None).await
    }

    /// Creates or updates `obj` at `path` (server-side apply, as this
    /// operator's field manager).
    pub async fn apply(&self, path: &str, obj: &Json) -> Result<Json, String> {
        let q = format!("{path}?fieldManager={MANAGER}&force=true");
        self.send(reqwest::Method::PATCH, &q, Some(("application/apply-patch+yaml", obj))).await?.ok_or_else(|| format!("apply {path}: not found"))
    }

    pub async fn merge(&self, path: &str, patch: &Json) -> Result<Option<Json>, String> {
        self.send(reqwest::Method::PATCH, path, Some(("application/merge-patch+json", patch))).await
    }

    pub async fn delete(&self, path: &str) -> Result<(), String> {
        let opts = json!({"kind": "DeleteOptions", "apiVersion": "v1", "propagationPolicy": "Background"});
        self.send(reqwest::Method::DELETE, path, Some(("application/json", &opts))).await.map(|_| ())
    }

    pub async fn list(&self, path: &str, selector: &str) -> Result<Vec<Json>, String> {
        let q = format!("{path}?labelSelector={}", selector.replace('=', "%3D").replace(',', "%2C").replace('/', "%2F"));
        Ok(self.get(&q).await?.and_then(|j| j["items"].as_array().cloned()).unwrap_or_default())
    }
}

/// The paths of the objects of a namespace.
fn ns_path(ns: &str, api: &str, plural: &str) -> String {
    match api {
        "v1" => format!("/api/v1/namespaces/{ns}/{plural}"),
        g => format!("/apis/{g}/namespaces/{ns}/{plural}"),
    }
}

/// One `StrataCluster`, on Kubernetes.
pub struct Cluster {
    pub kube: Kube,
    pub ns: String,
    pub name: String,
    pub uid: String,
    pub spec: Spec,
}

impl Cluster {
    fn labels(&self) -> Json {
        json!({"app.kubernetes.io/name": "strata", "app.kubernetes.io/instance": self.name, "app.kubernetes.io/managed-by": MANAGER})
    }

    fn set_labels(&self, id: u32) -> Json {
        let mut l = self.labels();
        l["app.kubernetes.io/component"] = json!("shard");
        l[format!("{GROUP}/set")] = json!(id.to_string());
        l
    }

    fn meta(&self, name: &str, labels: Json) -> Json {
        json!({
            "name": name, "namespace": self.ns, "labels": labels,
            "ownerReferences": [{"apiVersion": format!("{GROUP}/{VERSION}"), "kind": KIND, "name": self.name, "uid": self.uid, "controller": true, "blockOwnerDeletion": true}],
        })
    }

    fn servers_service(&self) -> String {
        format!("{}-servers", self.name)
    }

    fn set_name(&self, id: u32) -> String {
        format!("{}-s{id}", self.name)
    }

    /// The servers' token: the cluster token when there is one (the
    /// admin token then stays with the coordinators), else the token.
    fn server_token_env(&self) -> Vec<Json> {
        let s = &self.spec;
        match (&s.cluster_token_secret, &s.token_secret) {
            (Some(c), _) => vec![secret_env("STRATA_CLUSTER_TOKEN", c)],
            (None, Some(t)) => vec![secret_env("STRATA_TOKEN", t)],
            (None, None) => Vec::new(),
        }
    }

    /// The coordinators' tokens: the token, and the cluster token when
    /// there is one, which they send to the servers.
    fn coordinator_token_env(&self) -> Vec<Json> {
        let s = &self.spec;
        let token = s.token_secret.iter().map(|t| secret_env("STRATA_TOKEN", t));
        token.chain(s.cluster_token_secret.iter().map(|c| secret_env("STRATA_CLUSTER_TOKEN", c))).collect()
    }

    /// The objects every cluster has: the schema, and the two Services.
    pub fn common(&self) -> Vec<(String, Json)> {
        let ns = &self.ns;
        let schema = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": self.meta(&format!("{}-schema", self.name), self.labels()), "data": {"schema.json": self.spec.schema_json()}});
        let mut shard_sel = self.labels();
        shard_sel["app.kubernetes.io/component"] = json!("shard");
        let servers = json!({
            "apiVersion": "v1", "kind": "Service", "metadata": self.meta(&self.servers_service(), self.labels()),
            "spec": {"clusterIP": "None", "publishNotReadyAddresses": true, "selector": shard_sel,
                     "ports": [{"name": "http", "port": 7700}, {"name": "grpc", "port": 7701}]},
        });
        let mut coord_sel = self.labels();
        coord_sel["app.kubernetes.io/component"] = json!("coordinator");
        let front = json!({
            "apiVersion": "v1", "kind": "Service", "metadata": self.meta(&self.name, self.labels()),
            "spec": {"selector": coord_sel, "ports": [{"name": "http", "port": 7700, "targetPort": 7700}]},
        });
        vec![
            (format!("{}/{}-schema", ns_path(ns, "v1", "configmaps"), self.name), schema),
            (format!("{}/{}", ns_path(ns, "v1", "services"), self.servers_service()), servers),
            (format!("{}/{}", ns_path(ns, "v1", "services"), self.name), front),
        ]
    }

    /// The StatefulSet of shard set `id`, `replicas` copies.
    pub fn statefulset(&self, id: u32, replicas: usize, demo: Option<(usize, usize)>) -> Json {
        let s = &self.spec;
        let labels = self.set_labels(id);
        let mut env = vec![
            json!({"name": "STRATA_FAILOVER_AFTER_MS", "value": s.failover_after_ms.to_string()}),
            json!({"name": "STRATA_WRITE_ACKS", "value": s.write_acks}),
            json!({"name": "STRATA_ARGS", "value": shell_words(&s.args)}),
        ];
        if let (Some(n), Some((i, of))) = (s.demo, demo) {
            env.push(json!({"name": "STRATA_DEMO", "value": n.to_string()}));
            env.push(json!({"name": "STRATA_DEMO_SHARD", "value": format!("{i}/{of}")}));
        }
        env.extend(self.server_token_env());
        let mut container = json!({
            "name": "strata",
            "image": s.image,
            "command": ["sh", "-c", SERVER_SCRIPT],
            "env": env,
            "ports": [{"name": "http", "containerPort": 7700}, {"name": "grpc", "containerPort": 7701}],
            "readinessProbe": {"httpGet": {"path": "/readyz", "port": 7700}, "periodSeconds": 2, "failureThreshold": 3},
            "livenessProbe": {"httpGet": {"path": "/livez", "port": 7700}, "periodSeconds": 10, "failureThreshold": 6, "initialDelaySeconds": 10},
            "volumeMounts": [{"name": "data", "mountPath": "/data"}, {"name": "schema", "mountPath": "/etc/strata"}],
        });
        if let Some(p) = &s.image_pull_policy {
            container["imagePullPolicy"] = json!(p);
        }
        if let Some(r) = &s.resources {
            container["resources"] = r.clone();
        }
        let mut claim = json!({"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": s.storage.size}}});
        if let Some(c) = &s.storage.storage_class_name {
            claim["storageClassName"] = json!(c);
        }
        json!({
            "apiVersion": "apps/v1", "kind": "StatefulSet", "metadata": self.meta(&self.set_name(id), labels.clone()),
            "spec": {
                "serviceName": self.servers_service(),
                "replicas": replicas,
                "podManagementPolicy": "Parallel",
                "selector": {"matchLabels": labels},
                "persistentVolumeClaimRetentionPolicy": {"whenDeleted": "Retain", "whenScaled": "Delete"},
                "template": {
                    "metadata": {"labels": labels},
                    "spec": {
                        "terminationGracePeriodSeconds": 30,
                        // Copies of a shard on different nodes, when there are.
                        "affinity": {"podAntiAffinity": {"preferredDuringSchedulingIgnoredDuringExecution": [
                            {"weight": 100, "podAffinityTerm": {"topologyKey": "kubernetes.io/hostname", "labelSelector": {"matchLabels": labels}}},
                        ]}},
                        "containers": [container],
                        "volumes": [{"name": "schema", "configMap": {"name": format!("{}-schema", self.name)}}],
                    },
                },
                "volumeClaimTemplates": [{"metadata": {"name": "data", "labels": labels}, "spec": claim}],
            },
        })
    }

    /// The coordinators' Deployment, over `shards` (each shard's servers).
    pub fn coordinator_deployment(&self, shards: &[Vec<String>], replicas: usize) -> Json {
        let s = &self.spec;
        let mut labels = self.labels();
        labels["app.kubernetes.io/component"] = json!("coordinator");
        let list = shards.iter().map(|s| s.join("|")).collect::<Vec<_>>().join(",");
        let mut args = vec!["--shards".to_string(), list, "--listen".into(), "0.0.0.0:7700".into(), "--grpc".into(), "off".into()];
        if s.token_secret.is_none() {
            args.push("--no-token".into());
        }
        let mut container = json!({
            "name": "coordinator", "image": s.image, "args": args, "env": self.coordinator_token_env(),
            "ports": [{"name": "http", "containerPort": 7700}],
            "readinessProbe": {"httpGet": {"path": "/readyz", "port": 7700}, "periodSeconds": 2},
            "livenessProbe": {"httpGet": {"path": "/livez", "port": 7700}, "periodSeconds": 10, "failureThreshold": 6},
        });
        if let Some(p) = &s.image_pull_policy {
            container["imagePullPolicy"] = json!(p);
        }
        if let Some(r) = &s.coordinator_resources {
            container["resources"] = r.clone();
        }
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": self.meta(&format!("{}-coordinator", self.name), labels.clone()),
            "spec": {"replicas": replicas, "selector": {"matchLabels": labels}, "template": {"metadata": {"labels": labels}, "spec": {"containers": [container]}}},
        })
    }

    fn sets_path(&self) -> String {
        ns_path(&self.ns, "apps/v1", "statefulsets")
    }

    fn coordinator_path(&self) -> String {
        format!("{}/{}-coordinator", ns_path(&self.ns, "apps/v1", "deployments"), self.name)
    }

    /// The coordinators' URL, through their Service.
    pub fn admin_url(&self) -> String {
        format!("http://{}.{}.svc:7700", self.name, self.ns)
    }

    /// The token the servers require, from the Secret.
    pub async fn admin_token(&self) -> Result<Option<String>, String> {
        let Some(s) = &self.spec.token_secret else { return Ok(None) };
        let secret = self.kube.get(&format!("{}/{s}", ns_path(&self.ns, "v1", "secrets"))).await?.ok_or_else(|| format!("no Secret {s} in {}", self.ns))?;
        let b64 = secret["data"]["token"].as_str().ok_or_else(|| format!("Secret {s} has no key token"))?;
        let bytes = base64_decode(b64).ok_or_else(|| format!("Secret {s}: token is not base64"))?;
        Ok(Some(String::from_utf8_lossy(&bytes).trim().to_string()))
    }
}

/// How a copy starts: on an empty volume, the first copy of a shard the
/// cluster starts with loads its demo products, and every other copy is
/// created empty with the schema (it joins its shard and is sent a copy).
const SERVER_SCRIPT: &str = r#"set -e
set -- --db /data/db.strata --seq-no --listen 0.0.0.0:7700 --grpc 0.0.0.0:7701 --failover-after-ms "$STRATA_FAILOVER_AFTER_MS" --write-acks "$STRATA_WRITE_ACKS"
[ -n "$STRATA_TOKEN$STRATA_CLUSTER_TOKEN" ] || set -- "$@" --no-token
if [ ! -e /data/db.strata ]; then
  if [ "${HOSTNAME##*-}" = 0 ] && [ -n "$STRATA_DEMO" ]; then
    set -- "$@" --demo "$STRATA_DEMO" --demo-shard "$STRATA_DEMO_SHARD"
  else
    set -- "$@" --schema /etc/strata/schema.json
  fi
fi
eval "exec strata-server \"\$@\" $STRATA_ARGS"
"#;

/// A variable read from key `token` of Secret `secret`.
fn secret_env(name: &str, secret: &str) -> Json {
    json!({"name": name, "valueFrom": {"secretKeyRef": {"name": secret, "key": "token"}}})
}

/// `args` quoted for a POSIX shell.
fn shell_words(args: &[String]) -> String {
    args.iter().map(|a| format!("'{}'", a.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ")
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        acc = (acc << 6) | val(c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

impl Platform for Cluster {
    async fn sets(&self) -> Result<Vec<Set>, String> {
        let sel = format!("app.kubernetes.io/instance={},app.kubernetes.io/component=shard", self.name);
        let (sets_path, pods_path) = (self.sets_path(), ns_path(&self.ns, "v1", "pods"));
        let (sts, pods) = tokio::join!(self.kube.list(&sets_path, &sel), self.kube.list(&pods_path, &sel));
        let (sts, pods) = (sts?, pods?);
        let label = format!("{GROUP}/set");
        let mut sets = Vec::new();
        for s in &sts {
            let Some(id) = s["metadata"]["labels"][&label].as_str().and_then(|v| v.parse::<u32>().ok()) else { continue };
            let prefix = format!("{}-", self.set_name(id));
            let ready = pods
                .iter()
                .filter(|p| p["status"]["conditions"].as_array().is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True")))
                .filter_map(|p| p["metadata"]["name"].as_str()?.strip_prefix(&prefix)?.parse::<usize>().ok())
                .collect();
            sets.push(Set { id, replicas: s["spec"]["replicas"].as_u64().unwrap_or(0) as usize, ready });
        }
        Ok(sets)
    }

    async fn create_set(&self, id: u32, replicas: usize, demo: Option<(usize, usize)>) -> Result<(), String> {
        self.kube.apply(&format!("{}/{}", self.sets_path(), self.set_name(id)), &self.statefulset(id, replicas, demo)).await.map(|_| ())
    }

    async fn scale_set(&self, id: u32, replicas: usize) -> Result<(), String> {
        self.kube.merge(&format!("{}/{}", self.sets_path(), self.set_name(id)), &json!({"spec": {"replicas": replicas}})).await.map(|_| ())
    }

    async fn delete_set(&self, id: u32) -> Result<(), String> {
        self.kube.delete(&format!("{}/{}", self.sets_path(), self.set_name(id))).await?;
        // Its volumes: its shard's slots moved to others.
        let sel = format!("app.kubernetes.io/instance={},{GROUP}/set={id}", self.name);
        for pvc in self.kube.list(&ns_path(&self.ns, "v1", "persistentvolumeclaims"), &sel).await? {
            if let Some(n) = pvc["metadata"]["name"].as_str() {
                self.kube.delete(&format!("{}/{n}", ns_path(&self.ns, "v1", "persistentvolumeclaims"))).await?;
            }
        }
        Ok(())
    }

    async fn coordinator(&self) -> Result<Option<(Vec<Vec<String>>, usize)>, String> {
        let Some(d) = self.kube.get(&self.coordinator_path()).await? else { return Ok(None) };
        let args: Vec<&str> = d["spec"]["template"]["spec"]["containers"][0]["args"].as_array().into_iter().flatten().filter_map(Json::as_str).collect();
        let list = args.iter().position(|a| *a == "--shards").and_then(|k| args.get(k + 1)).copied().unwrap_or_default();
        let shards = list.split(',').filter(|s| !s.is_empty()).map(|s| s.split('|').map(str::to_string).collect()).collect();
        Ok(Some((shards, d["spec"]["replicas"].as_u64().unwrap_or(1) as usize)))
    }

    async fn set_coordinator(&self, shards: &[Vec<String>], replicas: usize) -> Result<(), String> {
        self.kube.apply(&self.coordinator_path(), &self.coordinator_deployment(shards, replicas)).await.map(|_| ())
    }

    fn url(&self, id: u32, ordinal: usize) -> String {
        format!("http://{}-{ordinal}.{}.{}.svc:7701", self.set_name(id), self.servers_service(), self.ns)
    }

    fn set_of(&self, url: &str) -> Option<(u32, usize)> {
        let host = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
        let (pod, rest) = host.split_once('.')?;
        if !rest.starts_with(&format!("{}.{}.", self.servers_service(), self.ns)) {
            return None;
        }
        let (set, k) = pod.strip_prefix(&format!("{}-s", self.name))?.split_once('-')?;
        Some((set.parse().ok()?, k.parse().ok()?))
    }
}

/// Runs the operator: installs the CRD (with `install_crd`), then looks
/// at every `StrataCluster` every `every`, takes each one step toward its
/// spec ([`super::reconcile`]), and writes what it found in its status.
pub async fn run(o: Options) -> Result<(), String> {
    let kube = Kube::new(&o)?;
    if o.install_crd {
        let crd = super::crd();
        let name = crd["metadata"]["name"].as_str().unwrap_or_default().to_string();
        kube.apply(&format!("/apis/apiextensions.k8s.io/v1/customresourcedefinitions/{name}"), &crd).await?;
        tracing::info!(crd = %name, "installed the StrataCluster resource definition");
    }
    let path = match &o.namespace {
        Some(ns) => format!("/apis/{GROUP}/{VERSION}/namespaces/{ns}/{PLURAL}"),
        None => format!("/apis/{GROUP}/{VERSION}/{PLURAL}"),
    };
    tracing::info!(namespace = o.namespace.as_deref().unwrap_or("(all)"), every_ms = o.every.as_millis() as u64, "operator: reconciling StrataClusters");
    loop {
        match kube.get(&path).await {
            Ok(list) => {
                let items = list.and_then(|l| l["items"].as_array().cloned()).unwrap_or_default();
                futures::future::join_all(items.iter().map(|c| once(&kube, c))).await;
            }
            Err(e) => tracing::warn!(error = %e, "could not list the StrataClusters"),
        }
        tokio::time::sleep(o.every).await;
    }
}

/// One step for one cluster, and its status.
pub async fn once(kube: &Kube, c: &Json) -> Outcome {
    let meta = &c["metadata"];
    let (ns, name) = (meta["namespace"].as_str().unwrap_or("default").to_string(), meta["name"].as_str().unwrap_or_default().to_string());
    let spec: Result<Spec, String> = serde_json::from_value(c["spec"].clone()).map_err(|e| format!("spec: {e}"));
    let out = match spec {
        Err(e) => Outcome::new(super::Phase::Failed, e),
        Ok(spec) => {
            let cluster = Cluster { kube: kube.clone(), ns: ns.clone(), name: name.clone(), uid: meta["uid"].as_str().unwrap_or_default().to_string(), spec };
            match common(&cluster).await {
                Err(e) => Outcome::new(super::Phase::Failed, e),
                Ok(admin) => super::reconcile(&cluster.spec, &cluster, &admin).await,
            }
        }
    };
    let status = json!({"phase": out.phase, "message": out.message, "shards": out.shards, "observedGeneration": meta["generation"]});
    if c["status"] != status {
        if out.phase != super::Phase::Ready || c["status"]["phase"] != "Ready" {
            tracing::info!(cluster = %format!("{ns}/{name}"), phase = ?out.phase, message = %out.message, "reconciled");
        }
        let path = format!("/apis/{GROUP}/{VERSION}/namespaces/{ns}/{PLURAL}/{name}/status");
        if let Err(e) = kube.merge(&path, &json!({"status": status})).await {
            tracing::warn!(cluster = %format!("{ns}/{name}"), error = %e, "could not write the status");
        }
    }
    out
}

/// Applies the objects every cluster has, and the coordinators' client.
async fn common(c: &Cluster) -> Result<Admin, String> {
    c.spec.check()?;
    for (path, obj) in c.common() {
        c.kube.apply(&path, &obj).await?;
    }
    Ok(Admin::new(&c.admin_url(), c.admin_token().await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(spec: Json) -> Cluster {
        let kube = Kube::new(&Options { api: Some("http://127.0.0.1:1".into()), ..Default::default() }).unwrap();
        Cluster { kube, ns: "db".into(), name: "orders".into(), uid: "u-1".into(), spec: serde_json::from_value(spec).unwrap() }
    }

    #[test]
    fn copies_are_named_by_set_and_ordinal() {
        let c = cluster(json!({"demo": 10}));
        let u = c.url(3, 2);
        assert_eq!(u, "http://orders-s3-2.orders-servers.db.svc:7701");
        assert_eq!(c.set_of(&u), Some((3, 2)));
        assert_eq!(c.set_of("http://other-s3-2.other-servers.db.svc:7701"), None);
        assert_eq!(c.set_of("http://orders-s3-2.orders-servers.elsewhere.svc:7701"), None);
    }

    #[test]
    fn a_shard_set_is_a_statefulset_of_copies_with_volumes() {
        let c = cluster(json!({"demo": 1000, "shards": 2, "storage": {"size": "5Gi", "storageClassName": "fast"}, "tokenSecret": "t", "args": ["--max-rows", "5'0"]}));
        let s = c.statefulset(1, 3, Some((1, 2)));
        assert_eq!(s["metadata"]["name"], "orders-s1");
        assert_eq!(s["metadata"]["ownerReferences"][0]["uid"], "u-1");
        assert_eq!(s["spec"]["replicas"], 3);
        assert_eq!(s["spec"]["serviceName"], "orders-servers");
        assert_eq!(s["spec"]["persistentVolumeClaimRetentionPolicy"]["whenScaled"], "Delete");
        let claim = &s["spec"]["volumeClaimTemplates"][0];
        assert_eq!((&claim["spec"]["resources"]["requests"]["storage"], &claim["spec"]["storageClassName"]), (&json!("5Gi"), &json!("fast")));
        let env = &s["spec"]["template"]["spec"]["containers"][0]["env"];
        let get = |n: &str| env.as_array().unwrap().iter().find(|e| e["name"] == n).cloned();
        assert_eq!(get("STRATA_DEMO_SHARD").unwrap()["value"], "1/2");
        assert_eq!(get("STRATA_ARGS").unwrap()["value"], "'--max-rows' '5'\\''0'");
        assert_eq!(get("STRATA_TOKEN").unwrap()["valueFrom"]["secretKeyRef"]["name"], "t");
        // A set added later loads nothing.
        let later = c.statefulset(2, 3, None);
        assert!(later["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().iter().all(|e| e["name"] != "STRATA_DEMO"));
    }

    #[test]
    fn the_coordinators_are_given_every_shards_servers() {
        let c = cluster(json!({"demo": 10}));
        let d = c.coordinator_deployment(&[vec!["a".into(), "b".into()], vec!["c".into()]], 2);
        let args = &d["spec"]["template"]["spec"]["containers"][0]["args"];
        assert_eq!(args[1], "a|b,c");
        assert!(args.as_array().unwrap().contains(&json!("--no-token")));
        assert_eq!(d["spec"]["replicas"], 2);
        let (svc, front) = (&c.common()[1].1, &c.common()[2].1);
        assert_eq!((&svc["spec"]["clusterIP"], &svc["spec"]["publishNotReadyAddresses"]), (&json!("None"), &json!(true)));
        assert_eq!(front["spec"]["selector"]["app.kubernetes.io/component"], "coordinator");
    }

    #[test]
    fn with_a_cluster_token_the_admin_token_stays_with_the_coordinators() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner"}));
        let names = |env: &Json| -> Vec<(String, String)> {
            let e = env.as_array().unwrap().iter().filter(|e| e["valueFrom"].is_object());
            e.map(|e| (e["name"].as_str().unwrap().to_string(), e["valueFrom"]["secretKeyRef"]["name"].as_str().unwrap().to_string())).collect()
        };
        let s = c.statefulset(0, 3, Some((0, 1)));
        assert_eq!(names(&s["spec"]["template"]["spec"]["containers"][0]["env"]), [("STRATA_CLUSTER_TOKEN".to_string(), "inner".to_string())]);
        let d = c.coordinator_deployment(&[vec!["a".into()]], 1);
        let coordinator = &d["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(names(&coordinator["env"]), [("STRATA_TOKEN".to_string(), "admin".to_string()), ("STRATA_CLUSTER_TOKEN".to_string(), "inner".to_string())]);
        assert!(!coordinator["args"].as_array().unwrap().contains(&json!("--no-token")));
        // Without one, the token is everyone's, as before.
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin"}));
        let d = c.coordinator_deployment(&[vec!["a".into()]], 1);
        assert_eq!(names(&d["spec"]["template"]["spec"]["containers"][0]["env"]), [("STRATA_TOKEN".to_string(), "admin".to_string())]);
    }

    #[test]
    fn base64_as_secrets_hold_it() {
        assert_eq!(base64_decode("c2VjcmV0LXRva2Vu").unwrap(), b"secret-token");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(base64_decode("!!"), None);
    }

    /// The server script, as `sh` runs it, for the first copy of a demo
    /// shard and for any other.
    #[cfg(unix)]
    #[test]
    fn a_copy_starts_with_demo_rows_or_the_schema() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("strata-server");
        std::fs::write(&bin, "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let run = |host: &str, demo: bool, env: &[(&str, &str)]| {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", SERVER_SCRIPT]).env("PATH", format!("{}:/usr/bin:/bin", dir.path().display())).env("HOSTNAME", host);
            cmd.env("STRATA_FAILOVER_AFTER_MS", "3000").env("STRATA_WRITE_ACKS", "all").env("STRATA_ARGS", "'--max-rows' '5 0'");
            if demo {
                cmd.env("STRATA_DEMO", "100").env("STRATA_DEMO_SHARD", "0/2");
            }
            cmd.env_remove("STRATA_TOKEN").env_remove("STRATA_CLUSTER_TOKEN").envs(env.iter().copied());
            String::from_utf8(cmd.output().unwrap().stdout).unwrap()
        };
        let first = run("orders-s0-0", true, &[]);
        assert!(first.contains("--demo\n100\n--demo-shard\n0/2\n") && first.contains("--no-token\n") && first.ends_with("--max-rows\n5 0\n"), "{first}");
        let other = run("orders-s0-1", true, &[]);
        assert!(other.contains("--schema\n/etc/strata/schema.json\n") && !other.contains("--demo"), "{other}");
        // A copy with either token is not served open.
        for t in ["STRATA_TOKEN", "STRATA_CLUSTER_TOKEN"] {
            let closed = run("orders-s0-1", false, &[(t, "x")]);
            assert!(!closed.contains("--no-token") && closed.contains("--schema\n"), "{t}: {closed}");
        }
    }
}
