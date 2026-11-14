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
//!   `c` (port 7700: the API and the UI), running as the service account
//!   `c-coordinator`, which may read and change this one `StrataCluster`
//!   (a Role and a RoleBinding of the same name): the command center
//!   shows the cluster's spec and status and scales it ([`Link`]).
//!
//! Each is owned by the `StrataCluster`, so deleting it deletes them (the
//! volumes are kept).
//!
//! The pods start the programs by their old names (`strata-server`,
//! `strata-crawl`, `strata-sheets`) and pass their settings as `STRATA_*`,
//! since ChloeDB was called Strata: every image answers to those, and the
//! images from before the rename, which a cluster may still run or roll
//! back to, answer to nothing else. They take the new names once the old
//! ones stop working.

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
        let mut b = crate::http_client().timeout(Duration::from_secs(30));
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

    /// A request to `path` of the API server with the token, whose
    /// answer may take as long as `timeout`: a call the hub forwards
    /// through a Service's proxy ([`crate::hub`]), which may stream.
    pub fn request(&self, method: reqwest::Method, path: &str, timeout: Duration) -> reqwest::RequestBuilder {
        let mut r = self.http.request(method, format!("{}{path}", self.api)).timeout(timeout);
        if let Some(t) = self.token() {
            r = r.bearer_auth(t);
        }
        r
    }

    /// A JSON call to `path`: the answer's status and body, whatever the
    /// status (a 409 of a compare-and-swap is an answer, not an error).
    pub async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Json>) -> Result<(u16, Json), String> {
        let mut r = self.http.request(method.clone(), format!("{}{path}", self.api));
        if let Some(t) = self.token() {
            r = r.bearer_auth(t);
        }
        if let Some(b) = body {
            r = r.header("content-type", "application/json").body(b.to_string());
        }
        let a = r.send().await.map_err(|e| format!("{method} {path}: {e}"))?;
        let status = a.status().as_u16();
        Ok((status, a.json().await.unwrap_or(Json::Null)))
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

    /// What a server tells of where it runs (`/v1/cluster`'s copies): its
    /// image, pod and node, and the cluster's name and environment. Taken
    /// at a server's next restart: the settings' hash leaves them out.
    fn place_env(&self) -> Vec<Json> {
        let field = |name: &str, path: &str| json!({"name": name, "valueFrom": {"fieldRef": {"fieldPath": path}}});
        let mut env = vec![
            json!({"name": "STRATA_IMAGE", "value": self.spec.image}),
            field("STRATA_POD", "metadata.name"),
            field("STRATA_NODE", "spec.nodeName"),
            json!({"name": "STRATA_CLUSTER_NAME", "value": format!("{}/{}", self.ns, self.name)}),
        ];
        if let Some(e) = self.spec.environment.as_deref().filter(|e| !e.is_empty()) {
            env.push(json!({"name": "STRATA_ENVIRONMENT", "value": e}));
        }
        env
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

    /// The variables `spec.secretEnv` takes from Secrets, for every server
    /// and coordinator.
    fn from_secrets(&self) -> Vec<Json> {
        let e = self.spec.secret_env.iter();
        e.map(|e| json!({"name": e.name, "valueFrom": {"secretKeyRef": {"name": e.secret, "key": e.key}}})).collect()
    }

    /// The coordinators' tokens: the token, and the cluster token when
    /// there is one, which they send to the servers.
    fn coordinator_token_env(&self) -> Vec<Json> {
        let s = &self.spec;
        let token = s.token_secret.iter().map(|t| secret_env("STRATA_TOKEN", t));
        token.chain(s.cluster_token_secret.iter().map(|c| secret_env("STRATA_CLUSTER_TOKEN", c))).collect()
    }

    /// The objects every cluster has: the schema, and the two Services.
    /// The coordinators' Role: this one StrataCluster, read and scaled
    /// from the command center, and (`lease`) the Lease of the cluster's
    /// map, which one coordinator at a time holds while it changes the map
    /// (finding E-2, [`super::lease`]). Kubernetes lets the operator make
    /// a Role only of what its own ClusterRole holds: one from before the
    /// Lease (operator.yaml as it was, or the cloud view's install) holds
    /// no Leases, and [`common`] makes the Role without them then.
    pub fn coordinator_role(&self, lease: bool) -> Json {
        let mut rules = vec![json!({"apiGroups": [GROUP], "resources": [PLURAL], "resourceNames": [self.name], "verbs": ["get", "patch"]})];
        if lease {
            // A create cannot name the object.
            rules.push(json!({"apiGroups": ["coordination.k8s.io"], "resources": ["leases"], "resourceNames": [format!("{}-map", self.name)], "verbs": ["get", "update"]}));
            rules.push(json!({"apiGroups": ["coordination.k8s.io"], "resources": ["leases"], "verbs": ["create"]}));
        }
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role", "metadata": self.meta(&self.coordinator_account(), self.labels()), "rules": rules})
    }

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
        // The coordinators' account: this one StrataCluster, read and scaled
        // from the command center.
        let account = self.coordinator_account();
        let sa = json!({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": self.meta(&account, self.labels())});
        let role = self.coordinator_role(true);
        let binding = json!({
            "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": self.meta(&account, self.labels()),
            "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": account},
            "subjects": [{"kind": "ServiceAccount", "name": account, "namespace": ns}],
        });
        let rbac = |plural: &str| format!("{}/{account}", ns_path(ns, "rbac.authorization.k8s.io/v1", plural));
        vec![
            (format!("{}/{}-schema", ns_path(ns, "v1", "configmaps"), self.name), schema),
            (format!("{}/{}", ns_path(ns, "v1", "services"), self.servers_service()), servers),
            (format!("{}/{}", ns_path(ns, "v1", "services"), self.name), front),
            (format!("{}/{account}", ns_path(ns, "v1", "serviceaccounts")), sa),
            (rbac("roles"), role),
            (rbac("rolebindings"), binding),
        ]
    }

    fn coordinator_account(&self) -> String {
        format!("{}-coordinator", self.name)
    }

    /// The StatefulSet of shard set `id`, `replicas` copies.
    pub fn statefulset(&self, id: u32, replicas: usize, demo: Option<(usize, usize)>) -> Json {
        let s = &self.spec;
        let labels = self.set_labels(id);
        let mut env = vec![
            json!({"name": "STRATA_FAILOVER_AFTER_MS", "value": s.failover_after_ms.to_string()}),
            json!({"name": "STRATA_WRITE_ACKS", "value": s.write_acks}),
            json!({"name": "STRATA_ARGS", "value": shell_words(&s.args)}),
            // Told to stop, a copy serves on a while before it stops
            // taking connections (`--drain-ms`; older servers ignore it).
            json!({"name": "STRATA_DRAIN_MS", "value": DRAIN_MS.to_string()}),
        ];
        env.extend(self.place_env());
        if let (Some(n), Some((i, of))) = (s.demo, demo) {
            env.push(json!({"name": "STRATA_DEMO", "value": n.to_string()}));
            env.push(json!({"name": "STRATA_DEMO_SHARD", "value": format!("{i}/{of}")}));
        }
        env.extend(self.server_token_env());
        env.extend(self.from_secrets());
        let mut container = json!({
            "name": "strata",
            "image": s.image,
            "command": ["sh", "-c", SERVER_SCRIPT],
            "env": env,
            "ports": [{"name": "http", "containerPort": 7700}, {"name": "grpc", "containerPort": 7701}],
            "readinessProbe": {"httpGet": {"path": "/readyz", "port": 7700}, "periodSeconds": 2, "failureThreshold": 3},
            "livenessProbe": {"httpGet": {"path": "/livez", "port": 7700}, "periodSeconds": 10, "failureThreshold": 6, "initialDelaySeconds": 10},
            // A server that exits says why in its pod's status.
            "terminationMessagePolicy": "FallbackToLogsOnError",
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
                // The operator restarts the copies, one at a time, when their
                // settings change ([`super::reconcile`]).
                "updateStrategy": {"type": "OnDelete"},
                "template": {
                    "metadata": {"labels": labels, "annotations": {CONFIG: s.server_config()}},
                    "spec": {
                        "terminationGracePeriodSeconds": 30,
                        // The servers run as user 10001: the volume is its
                        // group's, and the files a server wrote as root
                        // (images and operators before 0.2.1) are given to
                        // that group once.
                        "securityContext": run_as(true),
                        "affinity": self.copies_apart(&labels),
                        "containers": [container],
                        "volumes": [{"name": "schema", "configMap": {"name": format!("{}-schema", self.name)}}],
                    },
                },
                "volumeClaimTemplates": [{"metadata": {"name": "data", "labels": labels}, "spec": claim}],
            },
        })
    }

    /// Copies of a shard on different nodes, when there are: preferred,
    /// so that a cluster of fewer nodes than copies still runs them all.
    /// A rollout of a shard of fewer than three copies adds a third
    /// ([`super::RolloutSpec::extra_copies`]) and removes it after: a copy
    /// that restarts beside it weighed the extra copy's node as much as its
    /// peer's, so it landed beside its peer as often as not, and once the
    /// extra copy went both copies of each shard ran on one node (LIV-6).
    /// The shard's own copies (`apps.kubernetes.io/pod-index` below
    /// `spec.replicas`, the label Kubernetes 1.28 and later give a
    /// StatefulSet's pods) weigh twice as much as any copy of the set, which
    /// alone counts where the label is not given.
    fn copies_apart(&self, labels: &Json) -> Json {
        let own: Vec<String> = (0..self.spec.replicas).map(|k| k.to_string()).collect();
        let mut indexed = json!({"matchLabels": labels, "matchExpressions": [{"key": "apps.kubernetes.io/pod-index", "operator": "In", "values": own}]});
        if self.spec.replicas == 0 {
            indexed = json!({"matchLabels": labels});
        }
        json!({"podAntiAffinity": {"preferredDuringSchedulingIgnoredDuringExecution": [
            {"weight": 100, "podAffinityTerm": {"topologyKey": "kubernetes.io/hostname", "labelSelector": indexed}},
            {"weight": 50, "podAffinityTerm": {"topologyKey": "kubernetes.io/hostname", "labelSelector": {"matchLabels": labels}}},
        ]}})
    }

    /// The coordinators, over `shards` (each shard's servers): a
    /// Deployment, or with `spec.coordinatorStorage` a StatefulSet whose
    /// coordinators each keep their history on a volume of their own
    /// ([`super::COORDINATOR_FILES`]).
    pub fn coordinator_workload(&self, shards: &[Vec<String>], replicas: usize) -> Json {
        let s = &self.spec;
        let mut labels = self.labels();
        labels["app.kubernetes.io/component"] = json!("coordinator");
        let list = shards.iter().map(|s| s.join("|")).collect::<Vec<_>>().join(",");
        let mut args = vec!["--shards".to_string(), list, "--listen".into(), "0.0.0.0:7700".into(), "--grpc".into(), "off".into()];
        if s.token_secret.is_none() {
            args.push("--no-token".into());
        }
        if s.coordinator_storage.is_some() {
            // Not a flag coordinatorArgs gives itself: one given twice is refused.
            let given = |flag: &str| s.coordinator_args.iter().any(|a| a == flag || a.starts_with(&format!("{flag}=")));
            for (flag, path) in super::COORDINATOR_FILES.into_iter().filter(|(f, _)| !given(f)) {
                args.extend([flag.to_string(), path.to_string()]);
            }
        }
        args.extend(s.coordinator_args.iter().cloned());
        let mut env = self.coordinator_token_env();
        // The apps beside the cluster (the crawler, the spreadsheets), as
        // apps the command center shows; the admin token is the one they
        // require.
        let apps: Vec<String> = s.apps().into_iter().map(|a| format!("{a}={}", self.app_url(a))).collect();
        let mut volumes = Vec::new();
        let mut mounts = Vec::new();
        if !apps.is_empty() {
            env.push(json!({"name": "STRATA_APPS", "value": apps.join(",")}));
            // The admin token goes to an app that has no token of its own
            // yet; each app's own, once the operator made it, is a file of
            // the folder its credentials are mounted as, read as it is sent
            // ([`super::credentials`]).
            if let Some(t) = &s.token_secret {
                env.push(secret_env("STRATA_APP_TOKEN", t));
            }
            env.push(json!({"name": "STRATA_APP_TOKEN_DIR", "value": super::credentials::TOKEN_DIR}));
            let sources: Vec<Json> = s.apps().into_iter().map(|a| json!({"secret": {"name": super::credentials::secret_name(&self.name, a), "optional": true, "items": [{"key": super::credentials::TOKEN, "path": a}]}})).collect();
            volumes.push(json!({"name": "app-tokens", "projected": {"sources": sources}}));
            mounts.push(json!({"name": "app-tokens", "mountPath": super::credentials::TOKEN_DIR, "readOnly": true}));
        }
        env.push(json!({"name": "STRATA_KUBE_CLUSTER", "value": format!("{}/{}", self.ns, self.name)}));
        env.push(json!({"name": "STRATA_DRAIN_MS", "value": DRAIN_MS.to_string()}));
        env.extend(self.place_env());
        env.extend(self.from_secrets());
        let mut container = json!({
            "name": "coordinator", "image": s.image, "args": args, "env": env,
            "ports": [{"name": "http", "containerPort": 7700}],
            "readinessProbe": {"httpGet": {"path": "/readyz", "port": 7700}, "periodSeconds": 1, "failureThreshold": 2},
            "livenessProbe": {"httpGet": {"path": "/livez", "port": 7700}, "periodSeconds": 10, "failureThreshold": 6},
            "terminationMessagePolicy": "FallbackToLogsOnError",
        });
        if let Some(p) = &s.image_pull_policy {
            container["imagePullPolicy"] = json!(p);
        }
        if let Some(r) = &s.coordinator_resources {
            container["resources"] = r.clone();
        }
        if s.coordinator_storage.is_some() {
            mounts.push(json!({"name": "data", "mountPath": "/data"}));
        }
        if !mounts.is_empty() {
            container["volumeMounts"] = json!(mounts);
        }
        let mut pod = json!({"serviceAccountName": self.coordinator_account(), "terminationGracePeriodSeconds": 30, "securityContext": run_as(s.coordinator_storage.is_some()), "containers": [container]});
        if !volumes.is_empty() {
            pod["volumes"] = json!(volumes);
        }
        if let Some(st) = &s.coordinator_storage {
            // Coordinators on different nodes, when there are: a node lost
            // takes one.
            pod["affinity"] = json!({"podAntiAffinity": {"preferredDuringSchedulingIgnoredDuringExecution": [
                {"weight": 100, "podAffinityTerm": {"topologyKey": "kubernetes.io/hostname", "labelSelector": {"matchLabels": labels}}},
            ]}});
            let mut claim = json!({"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": st.size}}});
            if let Some(c) = &st.storage_class_name {
                claim["storageClassName"] = json!(c);
            }
            return json!({
                "apiVersion": "apps/v1", "kind": "StatefulSet", "metadata": self.meta(&format!("{}-coordinator", self.name), labels.clone()),
                "spec": {
                    "serviceName": self.name,
                    "replicas": replicas,
                    "podManagementPolicy": "Parallel",
                    "selector": {"matchLabels": labels},
                    // A volume is one coordinator's, so one restarts before
                    // the next starts: with two or more, the others serve.
                    "updateStrategy": {"type": "RollingUpdate"},
                    "minReadySeconds": 2,
                    // A coordinator's history is kept when the cluster or
                    // the coordinator goes.
                    "persistentVolumeClaimRetentionPolicy": {"whenDeleted": "Retain", "whenScaled": "Retain"},
                    "template": {
                        "metadata": {"labels": labels, "annotations": {CONFIG: s.coordinator_config()}},
                        "spec": pod,
                    },
                    "volumeClaimTemplates": [{"metadata": {"name": "data", "labels": labels}, "spec": claim}],
                },
            });
        }
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": self.meta(&format!("{}-coordinator", self.name), labels.clone()),
            "spec": {
                "replicas": replicas,
                "selector": {"matchLabels": labels},
                // A new coordinator is ready before an old one stops, so
                // that the Service always has one: none is down while they
                // restart, even when there is one.
                "strategy": {"type": "RollingUpdate", "rollingUpdate": {"maxUnavailable": 0, "maxSurge": 1}},
                "minReadySeconds": 2,
                "template": {
                    "metadata": {"labels": labels, "annotations": {CONFIG: s.coordinator_config()}},
                    "spec": pod,
                },
            },
        })
    }

    /// The pre-flight of `image`: a pod on each of `nodes` that runs
    /// `strata-server --help` in it and then waits, so that the image is
    /// pulled there and is known to run there, before any server restarts
    /// with it ([`super::Platform::preflight`]).
    pub fn preflight_deployment(&self, image: &str, nodes: &[String]) -> Json {
        let mut labels = self.labels();
        labels["app.kubernetes.io/component"] = json!("preflight");
        let mut container = json!({
            "name": "preflight", "image": image,
            "command": ["sh", "-c", "strata-server --help > /dev/null && echo 'strata-server runs here' && exec sleep 86400"],
            "resources": {"requests": {"cpu": "1m", "memory": "8Mi"}, "limits": {"memory": "128Mi"}},
            "terminationMessagePolicy": "FallbackToLogsOnError",
        });
        if let Some(p) = &self.spec.image_pull_policy {
            container["imagePullPolicy"] = json!(p);
        }
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": self.meta(&self.preflight_name(), labels.clone()),
            "spec": {
                "replicas": nodes.len(),
                "selector": {"matchLabels": labels},
                "template": {
                    "metadata": {"labels": labels},
                    "spec": {
                        "automountServiceAccountToken": false,
                        "terminationGracePeriodSeconds": 0,
                        // As the user the servers will run it as.
                        "securityContext": run_as(false),
                        // One on each node that runs copies.
                        "affinity": {
                            "nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {"nodeSelectorTerms": [{"matchExpressions": [{"key": "kubernetes.io/hostname", "operator": "In", "values": nodes}]}]}},
                            "podAntiAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": [{"topologyKey": "kubernetes.io/hostname", "labelSelector": {"matchLabels": labels}}]},
                        },
                        "containers": [container],
                    },
                },
            },
        })
    }

    fn preflight_name(&self) -> String {
        format!("{}-preflight", self.name)
    }

    fn crawler_name(&self) -> String {
        format!("{}-crawler", self.name)
    }

    /// Where the coordinators reach an app beside the cluster: its Service.
    fn app_url(&self, app: &str) -> String {
        let port = match app {
            super::sheets::APP => super::sheets::PORT,
            super::chloectx::APP => super::chloectx::PORT,
            super::apps::APP => super::apps::PORT,
            super::netops::APP => super::netops::PORT,
            _ => super::crawler::PORT,
        };
        format!("http://{}-{app}:{port}", self.name)
    }

    fn crawler_labels(&self) -> Json {
        let mut labels = self.labels();
        labels["app.kubernetes.io/component"] = json!("crawler");
        labels
    }

    /// The crawler's Deployment ([`super::crawler`]): one pod, or none
    /// while paused, never two at once (Recreate: each would hold crawls
    /// of its own), that writes through the coordinators' Service with
    /// the admin token, which its API requires too.
    pub fn crawler_deployment(&self, c: &super::CrawlerSpec, creds: Creds) -> Json {
        let s = &self.spec;
        let labels = self.crawler_labels();
        let mut env = vec![json!({"name": "RUST_LOG", "value": "info"})];
        env.extend(self.app_env(super::crawler::APP, creds, "STRATA_KEY", "STRATA_CRAWL_TOKEN"));
        let port = super::crawler::PORT;
        let mut container = json!({
            "name": "crawler", "image": s.image, "command": ["strata-crawl"], "args": c.args(&format!("http://{}:7700", self.name)), "env": env,
            "ports": [{"name": "http", "containerPort": port}],
            "readinessProbe": {"tcpSocket": {"port": port}, "periodSeconds": 5},
            "resources": c.resources.clone().unwrap_or_else(|| json!({"requests": {"cpu": "50m", "memory": "64Mi"}, "limits": {"memory": "1Gi"}})),
            "terminationMessagePolicy": "FallbackToLogsOnError",
        });
        if let Some(p) = &s.image_pull_policy {
            container["imagePullPolicy"] = json!(p);
        }
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": self.meta(&self.crawler_name(), labels.clone()),
            "spec": {
                "replicas": if c.paused { 0 } else { 1 },
                "selector": {"matchLabels": labels},
                "strategy": {"type": "Recreate"},
                "template": {
                    "metadata": {"labels": labels},
                    "spec": {"automountServiceAccountToken": false, "terminationGracePeriodSeconds": 10, "containers": [container]},
                },
            },
        })
    }

    /// The crawler's Service, which the coordinators forward
    /// `/v1/apps/crawler/...` to.
    pub fn crawler_service(&self) -> Json {
        let port = super::crawler::PORT;
        json!({
            "apiVersion": "v1", "kind": "Service", "metadata": self.meta(&self.crawler_name(), self.labels()),
            "spec": {"selector": self.crawler_labels(), "ports": [{"name": "http", "port": port, "targetPort": port}]},
        })
    }

    fn crawler_paths(&self) -> (String, String) {
        self.app_paths(&self.crawler_name())
    }

    /// The paths of an app's Service and Deployment.
    fn app_paths(&self, name: &str) -> (String, String) {
        (format!("{}/{name}", ns_path(&self.ns, "v1", "services")), format!("{}/{name}", ns_path(&self.ns, "apps/v1", "deployments")))
    }

    /// An app beside the cluster (the crawler, the spreadsheets): its
    /// Service and Deployment as `want`s, or, with none, both gone. Each is
    /// applied only when it changes, by the hash of what it is.
    async fn beside(&self, (service, deployment): (String, String), want: Option<(Json, Json)>) -> Result<(), String> {
        let Some((svc, dep)) = want else {
            for path in [deployment, service] {
                if self.kube.get(&path).await?.is_some() {
                    self.kube.delete(&path).await?;
                }
            }
            return Ok(());
        };
        for (path, mut want) in [(service, svc), (deployment, dep)] {
            let hash = super::short_hash(&want);
            want["metadata"]["annotations"] = json!({CONFIG: hash});
            if self.kube.get(&path).await?.is_some_and(|h| h["metadata"]["annotations"][CONFIG] == hash.as_str()) {
                continue;
            }
            self.kube.apply(&path, &want).await?;
        }
        Ok(())
    }

    fn sheets_name(&self) -> String {
        format!("{}-{}", self.name, super::sheets::APP)
    }

    fn sheets_labels(&self) -> Json {
        let mut labels = self.labels();
        labels["app.kubernetes.io/component"] = json!("sheets");
        labels
    }

    /// The spreadsheets' Deployment ([`super::sheets`]): one pod, or none
    /// while paused, never two at once (Recreate: each holds the workbooks
    /// it has open in memory, and saves them), that reads and writes
    /// through the coordinators' Service with the admin token, which its
    /// API requires too.
    pub fn sheets_deployment(&self, c: &super::SheetsSpec, creds: Creds) -> Json {
        let s = &self.spec;
        let labels = self.sheets_labels();
        let mut env = vec![json!({"name": "RUST_LOG", "value": "info"})];
        env.extend(self.app_env(super::sheets::APP, creds, "STRATA_KEY", "STRATA_SHEETS_TOKEN"));
        let port = super::sheets::PORT;
        let mut container = json!({
            "name": "sheets", "image": s.image, "command": ["strata-sheets"], "args": c.args(&format!("http://{}:7700", self.name)), "env": env,
            "ports": [{"name": "http", "containerPort": port}],
            "readinessProbe": {"tcpSocket": {"port": port}, "periodSeconds": 5},
            "resources": c.resources.clone().unwrap_or_else(|| json!({"requests": {"cpu": "50m", "memory": "96Mi"}, "limits": {"memory": "1Gi"}})),
            "terminationMessagePolicy": "FallbackToLogsOnError",
        });
        if let Some(p) = &s.image_pull_policy {
            container["imagePullPolicy"] = json!(p);
        }
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": self.meta(&self.sheets_name(), labels.clone()),
            "spec": {
                "replicas": if c.paused { 0 } else { 1 },
                "selector": {"matchLabels": labels},
                "strategy": {"type": "Recreate"},
                "template": {
                    "metadata": {"labels": labels},
                    // time to save the workbooks it holds (SIGTERM)
                    "spec": {"automountServiceAccountToken": false, "terminationGracePeriodSeconds": 30, "containers": [container]},
                },
            },
        })
    }

    /// The spreadsheets' Service, which the coordinators forward
    /// `/v1/apps/sheets/...` to.
    pub fn sheets_service(&self) -> Json {
        let port = super::sheets::PORT;
        json!({
            "apiVersion": "v1", "kind": "Service", "metadata": self.meta(&self.sheets_name(), self.labels()),
            "spec": {"selector": self.sheets_labels(), "ports": [{"name": "http", "port": port, "targetPort": port}]},
        })
    }

    fn netops_name(&self) -> String {
        format!("{}-{}", self.name, super::netops::APP)
    }

    fn netops_labels(&self) -> Json {
        let mut labels = self.labels();
        labels["app.kubernetes.io/component"] = json!("netops");
        labels
    }

    /// The network operations app's Deployment ([`super::netops`]): one
    /// pod, or none while paused, never two at once (Recreate: its
    /// simulator writes each minute of the network once), that reads and
    /// writes the tables `netops_*` through the coordinators' Service with
    /// its own key, or the admin token, which its API requires too.
    pub fn netops_deployment(&self, c: &super::NetopsSpec, creds: Creds) -> Json {
        let s = &self.spec;
        let labels = self.netops_labels();
        let mut env = vec![json!({"name": "RUST_LOG", "value": "info"})];
        env.extend(self.app_env(super::netops::APP, creds, "CHLOEDB_KEY", "CHLOEDB_NETOPS_TOKEN"));
        let port = super::netops::PORT;
        let mut container = json!({
            "name": "netops", "image": s.image, "command": ["chloedb-netops"], "args": c.args(&format!("http://{}:7700", self.name)), "env": env,
            "ports": [{"name": "http", "containerPort": port}],
            "readinessProbe": {"tcpSocket": {"port": port}, "periodSeconds": 5},
            "resources": c.resources.clone().unwrap_or_else(|| json!({"requests": {"cpu": "50m", "memory": "96Mi"}, "limits": {"memory": "1Gi"}})),
            "terminationMessagePolicy": "FallbackToLogsOnError",
        });
        if let Some(p) = &s.image_pull_policy {
            container["imagePullPolicy"] = json!(p);
        }
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": self.meta(&self.netops_name(), labels.clone()),
            "spec": {
                "replicas": if c.paused { 0 } else { 1 },
                "selector": {"matchLabels": labels},
                "strategy": {"type": "Recreate"},
                "template": {
                    "metadata": {"labels": labels},
                    "spec": {"automountServiceAccountToken": false, "terminationGracePeriodSeconds": 30, "containers": [container]},
                },
            },
        })
    }

    /// The network operations app's Service, which the coordinators
    /// forward `/v1/apps/netops/...` to.
    pub fn netops_service(&self) -> Json {
        let port = super::netops::PORT;
        json!({
            "apiVersion": "v1", "kind": "Service", "metadata": self.meta(&self.netops_name(), self.labels()),
            "spec": {"selector": self.netops_labels(), "ports": [{"name": "http", "port": port, "targetPort": port}]},
        })
    }

    fn chloectx_name(&self) -> String {
        format!("{}-{}", self.name, super::chloectx::APP)
    }

    fn chloectx_labels(&self) -> Json {
        let mut labels = self.labels();
        labels["app.kubernetes.io/component"] = json!("chloectx");
        labels
    }

    /// ChloeCtx's Deployment ([`super::chloectx`]): one pod, or none while
    /// paused, never two at once (Recreate: it numbers the records it
    /// writes to the spaces), that reads and writes through the
    /// coordinators' Service with the admin token, which its API requires
    /// too, and calls the model with the cluster's `secretEnv`. With the
    /// `static` embedder an init container first downloads the model into
    /// the pod's scratch volume.
    pub fn chloectx_deployment(&self, c: &super::ChloectxSpec, creds: Creds) -> Json {
        let s = &self.spec;
        let labels = self.chloectx_labels();
        let models = super::chloectx::MODELS;
        let mut env = self.app_env(super::chloectx::APP, creds, "CHLOEDB_KEY", "CHLOECTX_APP_TOKEN");
        env.push(json!({"name": "CHLOECTX_MODELS", "value": models}));
        env.extend(self.from_secrets());
        let port = super::chloectx::PORT;
        let resources = c.resources.clone().unwrap_or_else(|| json!({"requests": {"cpu": "50m", "memory": "128Mi"}, "limits": {"memory": "1Gi"}}));
        let mounts = json!([{"name": "models", "mountPath": models}]);
        let mut args = c.args(&format!("http://{}:7700", self.name));
        if s.token_secret.is_none() {
            // A cluster without a token serves anyone who reaches it, and so
            // does its ChloeCtx: chloectx app refuses that unless told.
            args.push("--insecure".into());
        }
        let mut container = json!({
            "name": "chloectx", "image": s.image, "command": ["chloectx"], "args": args, "env": env,
            "ports": [{"name": "http", "containerPort": port}],
            "readinessProbe": {"tcpSocket": {"port": port}, "periodSeconds": 5},
            "volumeMounts": mounts,
            "resources": resources,
            "terminationMessagePolicy": "FallbackToLogsOnError",
        });
        let mut model = json!({
            "name": "model", "image": s.image, "command": ["chloectx", "fetch-model"],
            "env": [{"name": "CHLOECTX_MODELS", "value": models}],
            "volumeMounts": mounts,
            "resources": resources,
            "terminationMessagePolicy": "FallbackToLogsOnError",
        });
        if let Some(p) = &s.image_pull_policy {
            container["imagePullPolicy"] = json!(p);
            model["imagePullPolicy"] = json!(p);
        }
        let mut pod = json!({
            "automountServiceAccountToken": false,
            "enableServiceLinks": false,
            "terminationGracePeriodSeconds": 10,
            "volumes": [{"name": "models", "emptyDir": {"sizeLimit": "512Mi"}}],
            "containers": [container],
        });
        if c.needs_model() {
            pod["initContainers"] = json!([model]);
        }
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": self.meta(&self.chloectx_name(), labels.clone()),
            "spec": {
                "replicas": if c.paused { 0 } else { 1 },
                "selector": {"matchLabels": labels},
                "strategy": {"type": "Recreate"},
                "template": {"metadata": {"labels": labels}, "spec": pod},
            },
        })
    }

    /// ChloeCtx's Service, which the coordinators forward
    /// `/v1/apps/chloectx/...` to.
    pub fn chloectx_service(&self) -> Json {
        let port = super::chloectx::PORT;
        json!({
            "apiVersion": "v1", "kind": "Service", "metadata": self.meta(&self.chloectx_name(), self.labels()),
            "spec": {"selector": self.chloectx_labels(), "ports": [{"name": "http", "port": port, "targetPort": port}]},
        })
    }

    fn apps_name(&self) -> String {
        format!("{}-{}", self.name, super::apps::APP)
    }

    fn apps_labels(&self) -> Json {
        let mut labels = self.labels();
        labels["app.kubernetes.io/component"] = json!("apps");
        labels
    }

    fn apps_claim_path(&self) -> String {
        format!("{}/{}", ns_path(&self.ns, "v1", "persistentvolumeclaims"), self.apps_name())
    }

    /// The app engine's Deployment ([`super::apps`]): one pod, or none
    /// while paused, never two at once (Recreate: its volume is one
    /// node's, and it counts what the model cost there), that keeps its
    /// apps in the cluster through the coordinators' Service with the
    /// admin token, which is also what the coordinators send it, and calls
    /// the model with the cluster's `secretEnv`.
    pub fn apps_deployment(&self, a: &super::AppsSpec, creds: Creds) -> Json {
        self.apps_deployment_at(a, a.publish.as_ref().and_then(|p| p.domain(None)).as_deref(), creds)
    }

    /// The same, with published apps answering under `domain` (none yet
    /// for `sslip` before its load balancer has an address); with
    /// `spec.apps.publish`, Caddy beside the engine gets their certificates.
    pub fn apps_deployment_at(&self, a: &super::AppsSpec, domain: Option<&str>, creds: Creds) -> Json {
        let s = &self.spec;
        let labels = self.apps_labels();
        let mut env = self.app_env(super::apps::APP, creds, "STRATA_TOKEN", "STRATA_APP_TOKEN");
        if let Some(u) = &a.public_url {
            env.push(json!({"name": "STRATA_APPS_PUBLIC_URL", "value": u}));
        }
        if let Some(d) = domain.filter(|_| a.publish.is_some()) {
            env.push(json!({"name": "STRATA_APPS_DOMAIN", "value": d}));
        }
        env.push(json!({"name": "STRATA_APPS_SPEND_CAP_USD", "value": a.spend_cap()}));
        env.extend(a.env.iter().map(|e| json!({"name": e.name, "value": e.value})));
        env.extend(self.from_secrets());
        let port = super::apps::PORT;
        let mut container = json!({
            "name": "apps", "image": a.image(), "args": a.args(&format!("http://{}:7700", self.name)), "env": env,
            "ports": [{"name": "http", "containerPort": port}],
            "readinessProbe": {"httpGet": {"path": "/healthz", "port": port}, "periodSeconds": 5},
            "volumeMounts": [{"name": "data", "mountPath": "/data"}],
            "resources": a.resources.clone().unwrap_or_else(|| json!({"requests": {"cpu": "250m", "memory": "600Mi"}, "limits": {"memory": "1536Mi"}})),
            "terminationMessagePolicy": "FallbackToLogsOnError",
        });
        if let Some(p) = &s.image_pull_policy {
            container["imagePullPolicy"] = json!(p);
        }
        let mut containers = vec![container];
        if let Some(p) = &a.publish {
            // Its certificates are kept on the engine's volume, so a new pod
            // does not ask Let's Encrypt for them again.
            containers.push(json!({
                "name": "caddy", "image": super::apps::CADDY_IMAGE,
                "command": ["sh", "-c", "printf '%s' \"$CADDYFILE\" > /tmp/Caddyfile && exec caddy run --config /tmp/Caddyfile --adapter caddyfile"],
                "env": [{"name": "CADDYFILE", "value": p.caddyfile()}, {"name": "XDG_DATA_HOME", "value": "/data"}, {"name": "XDG_CONFIG_HOME", "value": "/tmp"}],
                "ports": [{"name": "public-http", "containerPort": super::apps::CADDY_HTTP}, {"name": "public-https", "containerPort": super::apps::CADDY_HTTPS}],
                "volumeMounts": [{"name": "data", "mountPath": "/data", "subPath": "caddy"}],
                "resources": {"requests": {"cpu": "50m", "memory": "64Mi"}, "limits": {"memory": "256Mi"}},
                "terminationMessagePolicy": "FallbackToLogsOnError",
            }));
        }
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": self.meta(&self.apps_name(), labels.clone()),
            "spec": {
                "replicas": if a.paused { 0 } else { 1 },
                "selector": {"matchLabels": labels},
                "strategy": {"type": "Recreate"},
                "template": {
                    "metadata": {"labels": labels},
                    "spec": {
                        "automountServiceAccountToken": false,
                        "enableServiceLinks": false,
                        // The image's user (node), who owns the volume.
                        "securityContext": {"runAsNonRoot": true, "runAsUser": 1000, "runAsGroup": 1000, "fsGroup": 1000},
                        "terminationGracePeriodSeconds": 30,
                        "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": self.apps_name()}}],
                        "containers": containers,
                    },
                },
            },
        })
    }

    /// The app engine's Service, which the coordinators forward
    /// `/v1/apps/apps/...` to, and browsers reach the studio through (by a
    /// node's port, a load balancer, or an Ingress of one's own).
    pub fn apps_service(&self, a: &super::AppsSpec) -> Json {
        let port = super::apps::PORT;
        let mut p = json!({"name": "http", "port": port, "targetPort": port});
        if let Some(n) = a.node_port {
            p["nodePort"] = json!(n);
        }
        json!({
            "apiVersion": "v1", "kind": "Service", "metadata": self.meta(&self.apps_name(), self.labels()),
            "spec": {"type": a.service_type, "selector": self.apps_labels(), "ports": [p]},
        })
    }

    fn apps_public_name(&self) -> String {
        format!("{}-public", self.apps_name())
    }

    fn apps_public_path(&self) -> String {
        format!("{}/{}", ns_path(&self.ns, "v1", "services"), self.apps_public_name())
    }

    /// Published apps' load balancer (`spec.apps.publish`): 80 and 443 to
    /// Caddy beside the engine.
    pub fn apps_public_service(&self) -> Json {
        json!({
            "apiVersion": "v1", "kind": "Service", "metadata": self.meta(&self.apps_public_name(), self.labels()),
            "spec": {"type": "LoadBalancer", "selector": self.apps_labels(), "externalTrafficPolicy": "Local", "ports": [
                {"name": "http", "port": 80, "targetPort": super::apps::CADDY_HTTP},
                {"name": "https", "port": 443, "targetPort": super::apps::CADDY_HTTPS},
            ]},
        })
    }

    /// The load balancer, applied when it changes (or gone without
    /// `spec.apps.publish`), and its address once it has one.
    async fn apps_public(&self, a: Option<&super::AppsSpec>) -> Result<Option<String>, String> {
        let path = self.apps_public_path();
        let have = self.kube.get(&path).await?;
        if a.and_then(|a| a.publish.as_ref()).is_none() {
            if have.is_some() {
                self.kube.delete(&path).await?;
            }
            return Ok(None);
        }
        let mut want = self.apps_public_service();
        let hash = super::short_hash(&want);
        want["metadata"]["annotations"] = json!({CONFIG: hash});
        let have = match have {
            Some(h) if h["metadata"]["annotations"][CONFIG] == hash.as_str() => h,
            _ => self.kube.apply(&path, &want).await?,
        };
        Ok(have["status"]["loadBalancer"]["ingress"][0]["ip"].as_str().map(String::from))
    }

    /// The app engine's volume: made once, owned by the cluster.
    pub fn apps_claim(&self, a: &super::AppsSpec) -> Json {
        let mut spec = json!({"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": a.storage.size}}});
        if let Some(c) = &a.storage.storage_class_name {
            spec["storageClassName"] = json!(c);
        }
        json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": self.meta(&self.apps_name(), self.apps_labels()), "spec": spec})
    }

    /// The volume, before the pod that mounts it: made when there is none,
    /// grown when it is asked to be larger, else left as it is (a claim's
    /// class and modes cannot change).
    async fn apps_volume(&self, a: &super::AppsSpec) -> Result<(), String> {
        let path = self.apps_claim_path();
        let Some(have) = self.kube.get(&path).await? else {
            return self.kube.apply(&path, &self.apps_claim(a)).await.map(|_| ());
        };
        let has = have["spec"]["resources"]["requests"]["storage"].as_str().and_then(super::quantity).unwrap_or(0);
        if super::quantity(&a.storage.size).is_some_and(|want| want > has) {
            self.kube.merge(&path, &json!({"spec": {"resources": {"requests": {"storage": a.storage.size}}}})).await?;
        }
        Ok(())
    }

    fn follow_path(&self) -> String {
        format!("{}/{}-follow", ns_path(&self.ns, "apps/v1", "deployments"), self.name)
    }

    /// The follow process's Deployment ([`super::follow`]): one pod, or
    /// none while paused, never two at once (Recreate), that writes
    /// through the coordinators' Service with the cluster's token.
    pub fn follow_deployment(&self, f: &super::FollowSpec) -> Json {
        let s = &self.spec;
        let mut labels = self.labels();
        labels["app.kubernetes.io/component"] = json!("follow");
        let mut env = vec![json!({"name": "RUST_LOG", "value": "info"})];
        if let Some(k) = &f.key_secret {
            env.push(json!({"name": "STRATA_FOLLOW_KEY", "valueFrom": {"secretKeyRef": {"name": k, "key": "key"}}}));
        }
        // The cluster token where there is one, else the token: what the
        // coordinators take from their own cluster's servers.
        if let Some(t) = s.cluster_token_secret.as_ref().or(s.token_secret.as_ref()) {
            env.push(secret_env("STRATA_CLUSTER_TOKEN", t));
        }
        let mut container = json!({
            "name": "follow", "image": s.image, "args": f.args(&format!("http://{}:7700", self.name)), "env": env,
            "resources": f.resources.clone().unwrap_or_else(|| json!({"requests": {"cpu": "100m", "memory": "128Mi"}, "limits": {"memory": "1Gi"}})),
            "terminationMessagePolicy": "FallbackToLogsOnError",
        });
        if let Some(p) = &s.image_pull_policy {
            container["imagePullPolicy"] = json!(p);
        }
        let mut pod = json!({"automountServiceAccountToken": false, "terminationGracePeriodSeconds": 10, "containers": [container]});
        if let Some(ca) = &f.ca_secret {
            pod["containers"][0]["volumeMounts"] = json!([{"name": "source-ca", "mountPath": super::follow::CA_DIR, "readOnly": true}]);
            pod["volumes"] = json!([{"name": "source-ca", "secret": {"secretName": ca, "items": [{"key": "ca.crt", "path": "ca.crt"}]}}]);
        }
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment", "metadata": self.meta(&format!("{}-follow", self.name), labels.clone()),
            "spec": {
                "replicas": if f.paused { 0 } else { 1 },
                "selector": {"matchLabels": labels},
                // Never two follow processes at once: the old one stops
                // before the new one starts (two would apply nothing
                // twice, but write twice as much).
                "strategy": {"type": "Recreate"},
                "template": {"metadata": {"labels": labels}, "spec": pod},
            },
        })
    }

    fn preflight_path(&self) -> String {
        format!("{}/{}", ns_path(&self.ns, "apps/v1", "deployments"), self.preflight_name())
    }

    /// The cluster's pods: the copies' and the coordinators'.
    async fn pods(&self) -> Result<Vec<Json>, String> {
        let sel = format!("app.kubernetes.io/instance={}", self.name);
        let pods = self.kube.list(&ns_path(&self.ns, "v1", "pods"), &sel).await?;
        Ok(pods.into_iter().filter(|p| matches!(p["metadata"]["labels"]["app.kubernetes.io/component"].as_str(), Some("shard" | "coordinator"))).collect())
    }

    /// The digests the servers' pods run (their containers' `imageID`),
    /// each with how many run it.
    pub async fn running_images(&self) -> Result<Vec<super::upgrade::Running>, String> {
        let mut out: Vec<super::upgrade::Running> = Vec::new();
        for pod in self.pods().await? {
            let Some(id) = pod["status"]["containerStatuses"][0]["imageID"].as_str() else { continue };
            let Some(d) = id.rsplit_once('@').map(|(_, d)| d.to_string()).filter(|d| d.starts_with("sha256:")) else { continue };
            match out.iter_mut().find(|r| r.digest == d) {
                Some(r) => r.servers += 1,
                None => out.push(super::upgrade::Running { digest: d, servers: 1 }),
            }
        }
        out.sort_by(|a, b| b.servers.cmp(&a.servers));
        Ok(out)
    }

    fn sets_path(&self) -> String {
        ns_path(&self.ns, "apps/v1", "statefulsets")
    }

    /// The coordinators' object as the spec has them: a StatefulSet with
    /// `coordinatorStorage`, else a Deployment.
    fn coordinator_path(&self) -> String {
        self.coordinator_path_of(self.spec.coordinator_storage.is_some())
    }

    /// The coordinators' object of the other kind: the one they ran as
    /// before `coordinatorStorage` was given or taken away, which stops
    /// once the new ones serve.
    fn old_coordinator_path(&self) -> String {
        self.coordinator_path_of(self.spec.coordinator_storage.is_none())
    }

    fn coordinator_path_of(&self, stateful: bool) -> String {
        format!("{}/{}-coordinator", ns_path(&self.ns, "apps/v1", if stateful { "statefulsets" } else { "deployments" }), self.name)
    }

    /// The coordinators' object: of the kind the spec has them, else of
    /// the other (they run so until they restart with the new settings).
    async fn coordinator_object(&self) -> Result<Option<Json>, String> {
        match self.kube.get(&self.coordinator_path()).await? {
            Some(d) => Ok(Some(d)),
            None => self.kube.get(&self.old_coordinator_path()).await,
        }
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
/// The user the servers, the coordinators and the pre-flight run as: the
/// image's `chloedb` (10001), not root, given here rather than by the image
/// (deploy/Dockerfile), whose user stays root so that an operator from
/// before this one, which gives its pods no user, still runs its servers as
/// root on the volumes a root image wrote. `volume`: the pod's volume
/// belongs to that group, and files written as root are given to it the
/// first time (`OnRootMismatch`: only while the volume's root is not yet
/// the group's). The images from before 0.2.1 run as that user too: their
/// programs are anyone's to run, and their data is the volume.
fn run_as(volume: bool) -> Json {
    let mut s = json!({"runAsUser": 10001, "runAsGroup": 10001, "runAsNonRoot": true});
    if volume {
        s["fsGroup"] = json!(10001);
        s["fsGroupChangePolicy"] = json!("OnRootMismatch");
    }
    s
}

/// How far the coordinators' Deployment or StatefulSet is in restarting
/// them with its template.
fn rolled_of(d: &Json) -> super::Rolled {
    let st = &d["status"];
    let n = |j: &Json| j.as_u64().unwrap_or(0);
    let (want, updated, ready, all) = (n(&d["spec"]["replicas"]), n(&st["updatedReplicas"]), n(&st["readyReplicas"]), n(&st["replicas"]));
    let seen = n(&st["observedGeneration"]) >= n(&d["metadata"]["generation"]);
    // A StatefulSet counts no unavailable ones: its revisions say whether
    // every pod runs the template.
    let current = d["kind"] != "StatefulSet" || st["currentRevision"] == st["updateRevision"];
    let done = seen && updated == want && ready >= want && all == want && n(&st["unavailableReplicas"]) == 0 && current;
    let old = all.saturating_sub(updated);
    let text = format!("{updated} of {want} restarted, {ready} ready{}", if old > 0 { format!(", {old} older still serving") } else { String::new() });
    super::Rolled { done, text }
}

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
/// The annotation of a pod template, and so of its pods, that holds the
/// settings they start with ([`Spec::server_config`]).
const CONFIG: &str = "strata.chloetech.ai/config";

/// How long a server serves on once its pod is told to stop
/// (`STRATA_DRAIN_MS`): long enough for its Service's endpoints, and the
/// coordinators, to stop sending to it, well within the grace period.
pub const DRAIN_MS: u64 = 5_000;

/// Why a pod's server cannot run, from its status, if it cannot: an image
/// that cannot be pulled, a container that cannot be made, a server that
/// exits again and again, or that has exited twice already.
/// Whether a pod is ready: not one being deleted, which stays ready while
/// its server drains (until its readiness probe next fails), and is not
/// back until its new pod is.
pub fn pod_ready(pod: &Json) -> bool {
    pod["metadata"]["deletionTimestamp"].is_null() && pod["status"]["conditions"].as_array().is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
}

pub fn pod_trouble(pod: &Json) -> Option<String> {
    let c = &pod["status"]["containerStatuses"][0];
    let last = &c["lastState"]["terminated"];
    let exited = || {
        let code = last["exitCode"].as_i64()?;
        let said = last["message"].as_str().or_else(|| last["reason"].as_str()).unwrap_or_default();
        let tail: Vec<&str> = said.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = tail[tail.len().saturating_sub(3)..].join(" / ");
        Some(format!("it exited with code {code}{}", if tail.is_empty() { String::new() } else { format!(": {tail}") }))
    };
    let waiting = &c["state"]["waiting"];
    let reason = waiting["reason"].as_str().unwrap_or_default();
    let message = waiting["message"].as_str().unwrap_or_default();
    let restarts = c["restartCount"].as_u64().unwrap_or(0);
    match reason {
        "ImagePullBackOff" | "InvalidImageName" | "ErrImageNeverPull" | "CreateContainerConfigError" | "CreateContainerError" | "RunContainerError" => {
            Some(format!("{reason}{}", if message.is_empty() { String::new() } else { format!(": {message}") }))
        }
        "CrashLoopBackOff" => Some(format!("CrashLoopBackOff ({restarts} restarts){}", exited().map(|e| format!(", {e}")).unwrap_or_default())),
        _ if restarts >= 2 => Some(format!("its server restarted {restarts} times{}", exited().map(|e| format!(", {e}")).unwrap_or_default())),
        _ => None,
    }
}

/// The demo shard (`i/n`) a set's live template loads, if it does.
fn demo_of(set: &Json) -> Option<(usize, usize)> {
    let env = set["spec"]["template"]["spec"]["containers"][0]["env"].as_array()?;
    let v = env.iter().find(|e| e["name"] == "STRATA_DEMO_SHARD")?["value"].as_str()?;
    let (i, n) = v.split_once('/')?;
    Some((i.parse().ok()?, n.parse().ok()?))
}

fn secret_env(name: &str, secret: &str) -> Json {
    json!({"name": name, "valueFrom": {"secretKeyRef": {"name": secret, "key": "token"}}})
}

/// What an app beside the cluster holds of its own
/// ([`super::credentials`]): a key limited to its tables, and a token. What
/// it does not, it takes from the admin token, as before such keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Creds {
    pub key: bool,
    pub token: bool,
    /// The id of the key the Secret holds ([`super::credentials::KEY_ID`]):
    /// on the pod template, so that a new key rolls the app's pod, which
    /// reads its key from the environment once (audit N-D4).
    pub key_id: Option<String>,
    /// No key yet, because making one failed ([`super::credentials::PENDING`]):
    /// the app is given no key rather than the admin token (audit X-E2).
    pub pending: bool,
}

/// `d`, an app's Deployment, its pod template marked with the id of the
/// key it is given: a key made again (its tables changed, or it was lost)
/// changes the template, and the Deployment rolls the pod onto it.
pub fn rolled(mut d: Json, creds: &Creds) -> Json {
    if let (true, Some(id)) = (creds.key, &creds.key_id) {
        d["spec"]["template"]["metadata"]["annotations"][super::credentials::KEY_ID] = json!(id);
    }
    d
}

impl Cluster {
    /// The image the app engine runs ([`super::upgrade::apps_image`]),
    /// from what the cluster's `status.upgrade` found of its tag.
    async fn apps_image(&self, u: &super::upgrade::Upgrade, image: &str) -> String {
        if u.mode != super::upgrade::Mode::On {
            return image.to_string();
        }
        let path = format!("/apis/{GROUP}/{VERSION}/namespaces/{}/{PLURAL}/{}", self.ns, self.name);
        let state: super::upgrade::State = match self.kube.get(&path).await {
            Ok(Some(c)) => serde_json::from_value(c["status"]["upgrade"].clone()).unwrap_or_default(),
            _ => return image.to_string(),
        };
        super::upgrade::apps_image(u, &state, image)
    }

    /// The digest the app engine's Deployment runs (as its template names
    /// it) and whether it is ready on it: rolled out to every pod, and
    /// every pod available. None when it is paused, names no digest, or
    /// cannot be read.
    async fn apps_engine(&self) -> Option<(String, bool)> {
        let path = format!("{}/{}", ns_path(&self.ns, "apps/v1", "deployments"), self.apps_name());
        let d = self.kube.get(&path).await.ok()??;
        engine_state(&d)
    }

    /// An app's two variables, its cluster `key` and its own API's
    /// `token`: from its credentials Secret, or the admin token.
    fn app_env(&self, app: &str, creds: Creds, key: &str, token: &str) -> Vec<Json> {
        let own = |name: &str, entry: &str| json!({"name": name, "valueFrom": {"secretKeyRef": {"name": super::credentials::secret_name(&self.name, app), "key": entry}}});
        let admin = self.spec.token_secret.as_deref();
        let mut env = Vec::new();
        match (creds.key, admin) {
            (true, _) => env.push(own(key, super::credentials::KEY)),
            (false, _) if creds.pending => {}
            (false, Some(t)) => env.push(secret_env(key, t)),
            (false, None) => {}
        }
        match (creds.token, admin) {
            (true, _) => env.push(own(token, super::credentials::TOKEN)),
            (false, Some(t)) => env.push(secret_env(token, t)),
            (false, None) => {}
        }
        env
    }

    /// What app `app` holds of its own now, by its credentials Secret.
    async fn creds(&self, spec: &Spec, app: &str) -> Result<Creds, String> {
        let s = self.kube.get(&self.credentials_path(app)).await?;
        let e = s.as_ref().map(super::credentials::entries).unwrap_or_default();
        let has = |k: &str| e.get(k).is_some_and(|v| !v.is_empty());
        let key_id = s.as_ref().and_then(|s| s["metadata"]["annotations"][super::credentials::KEY_ID].as_str()).map(str::to_string);
        let pending = super::credentials::keyed(spec, app) && s.as_ref().is_some_and(|s| s["metadata"]["annotations"][super::credentials::PENDING] == "true");
        Ok(Creds { key: has(super::credentials::KEY) && super::credentials::keyed(spec, app), token: has(super::credentials::TOKEN), key_id, pending })
    }

    fn credentials_path(&self, app: &str) -> String {
        format!("{}/{}", ns_path(&self.ns, "v1", "secrets"), super::credentials::secret_name(&self.name, app))
    }
}

/// `args` quoted for a POSIX shell.
fn shell_words(args: &[String]) -> String {
    args.iter().map(|a| format!("'{}'", a.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ")
}

/// `b` in base64, as a Secret's data holds it.
pub fn base64_encode(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = c.iter().enumerate().fold(0u32, |n, (i, x)| n | (*x as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
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
                .filter(|p| pod_ready(p))
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
        let Some(d) = self.coordinator_object().await? else { return Ok(None) };
        // Coordinators of both kinds (the spec's were made while the others
        // served, by a restart or a change of their list): once the spec's
        // all serve, the others stop, whichever step made them.
        if d["kind"] == if self.spec.coordinator_storage.is_some() { "StatefulSet" } else { "Deployment" } && rolled_of(&d).done && self.kube.get(&self.old_coordinator_path()).await?.is_some() {
            self.kube.delete(&self.old_coordinator_path()).await?;
        }
        let args: Vec<&str> = d["spec"]["template"]["spec"]["containers"][0]["args"].as_array().into_iter().flatten().filter_map(Json::as_str).collect();
        let list = args.iter().position(|a| *a == "--shards").and_then(|k| args.get(k + 1)).copied().unwrap_or_default();
        let shards = list.split(',').filter(|s| !s.is_empty()).map(|s| s.split('|').map(str::to_string).collect()).collect();
        Ok(Some((shards, d["spec"]["replicas"].as_u64().unwrap_or(1) as usize)))
    }

    async fn set_coordinator(&self, shards: &[Vec<String>], replicas: usize) -> Result<(), String> {
        // Those of the other kind serve on until these do: the restart
        // waits for these, then stops them ([`Self::coordinators_rolled`]).
        self.kube.apply(&self.coordinator_path(), &self.coordinator_workload(shards, replicas)).await.map(|_| ())
    }

    async fn follow(&self, spec: &Spec) -> Result<(), String> {
        let path = self.follow_path();
        let have = self.kube.get(&path).await?;
        let Some(f) = &spec.follow else {
            if have.is_some() {
                self.kube.delete(&path).await?;
            }
            return Ok(());
        };
        // Applied only when it changes, by the hash of what it is.
        let with = Cluster { kube: self.kube.clone(), ns: self.ns.clone(), name: self.name.clone(), uid: self.uid.clone(), spec: spec.clone() };
        let mut d = with.follow_deployment(f);
        let hash = super::short_hash(&d);
        d["metadata"]["annotations"] = json!({CONFIG: hash});
        if have.is_some_and(|h| h["metadata"]["annotations"][CONFIG] == hash.as_str()) {
            return Ok(());
        }
        self.kube.apply(&path, &d).await.map(|_| ())
    }

    async fn crawler(&self, spec: &Spec) -> Result<(), String> {
        let with = Cluster { kube: self.kube.clone(), ns: self.ns.clone(), name: self.name.clone(), uid: self.uid.clone(), spec: spec.clone() };
        let creds = self.creds(spec, super::crawler::APP).await?;
        self.beside(self.crawler_paths(), spec.crawler.as_ref().map(|c| (with.crawler_service(), rolled(with.crawler_deployment(c, creds.clone()), &creds)))).await
    }

    async fn sheets(&self, spec: &Spec) -> Result<(), String> {
        let with = Cluster { kube: self.kube.clone(), ns: self.ns.clone(), name: self.name.clone(), uid: self.uid.clone(), spec: spec.clone() };
        let creds = self.creds(spec, super::sheets::APP).await?;
        self.beside(self.app_paths(&self.sheets_name()), spec.sheets.as_ref().map(|c| (with.sheets_service(), rolled(with.sheets_deployment(c, creds.clone()), &creds)))).await
    }

    async fn netops(&self, spec: &Spec) -> Result<(), String> {
        let with = Cluster { kube: self.kube.clone(), ns: self.ns.clone(), name: self.name.clone(), uid: self.uid.clone(), spec: spec.clone() };
        let creds = self.creds(spec, super::netops::APP).await?;
        self.beside(self.app_paths(&self.netops_name()), spec.netops.as_ref().map(|c| (with.netops_service(), rolled(with.netops_deployment(c, creds.clone()), &creds)))).await
    }

    async fn chloectx(&self, spec: &Spec) -> Result<(), String> {
        let with = Cluster { kube: self.kube.clone(), ns: self.ns.clone(), name: self.name.clone(), uid: self.uid.clone(), spec: spec.clone() };
        let creds = self.creds(spec, super::chloectx::APP).await?;
        self.beside(self.app_paths(&self.chloectx_name()), spec.chloectx.as_ref().map(|c| (with.chloectx_service(), rolled(with.chloectx_deployment(c, creds.clone()), &creds)))).await
    }

    async fn apps(&self, spec: &Spec) -> Result<(), String> {
        let with = Cluster { kube: self.kube.clone(), ns: self.ns.clone(), name: self.name.clone(), uid: self.uid.clone(), spec: spec.clone() };
        if let Some(a) = &spec.apps {
            with.apps_volume(a).await?;
        }
        // With upgrades on, the engine runs its tag's newest image by digest.
        let mut spec = spec.clone();
        if let Some(a) = spec.apps.as_mut() {
            let image = self.apps_image(&spec.upgrade, a.image()).await;
            a.image = Some(image);
        }
        let spec = &spec;
        let creds = self.creds(spec, super::apps::APP).await?;
        // Published apps' load balancer first: an sslip domain is its address.
        let address = with.apps_public(spec.apps.as_ref()).await?;
        let domain = spec.apps.as_ref().and_then(|a| a.publish.as_ref()).and_then(|p| p.domain(address.as_deref()));
        self.beside(self.app_paths(&self.apps_name()), spec.apps.as_ref().map(|a| (with.apps_service(a), rolled(with.apps_deployment_at(a, domain.as_deref(), creds.clone()), &creds)))).await
    }

    async fn app_credentials(&self, spec: &Spec, admin: &super::Admin) -> Result<(), String> {
        use super::credentials as cr;
        let apps = spec.apps();
        // An app taken out of the spec: its key revoked, its Secret gone.
        for app in [super::crawler::APP, super::sheets::APP, super::chloectx::APP, super::apps::APP, super::netops::APP] {
            if apps.contains(&app) {
                continue;
            }
            let path = self.credentials_path(app);
            let Some(have) = self.kube.get(&path).await? else { continue };
            if let Some(id) = have["metadata"]["annotations"][cr::KEY_ID].as_str() {
                admin.call(reqwest::Method::DELETE, &format!("/v1/admin/keys/{id}"), None).await.or_else(|e| if e.contains("404") { Ok(Json::Null) } else { Err(e) })?;
            }
            match self.kube.delete(&path).await {
                Err(e) if forbidden(&e) => tracing::warn!(cluster = %self.name, app, error = %e, "{OLD_ROLE}: the Secret of an app taken out of the spec is left (its key is revoked)"),
                r => r?,
            }
        }
        if apps.is_empty() || !cr::supported(admin).await {
            return Ok(());
        }
        let mut failed = Vec::new();
        for app in apps {
            let path = self.credentials_path(app);
            let have = self.kube.get(&path).await?;
            let e = have.as_ref().map(cr::entries).unwrap_or_default();
            let note = |k: &str| have.as_ref().and_then(|h| h["metadata"]["annotations"][k].as_str()).map(str::to_string);
            let (old_id, old_hash) = (note(cr::KEY_ID), note(cr::TABLES));
            let tables = cr::spec_tables(spec, app);
            let hash = cr::tables_hash(&tables);
            let token = match e.get(cr::TOKEN).filter(|t| !t.is_empty()) {
                Some(t) => t.clone(),
                None => crate::keys::random_hex(32).map_err(|e| e.message)?,
            };
            if have.is_none() {
                // The Secret first, with its token alone: an operator whose
                // ClusterRole is from before apps' credentials (an
                // installed operator upgrades neither itself nor it) may
                // not make Secrets, and then makes no key either, which it
                // would make again at every reconcile. Its apps keep the
                // admin token, as they had it.
                let mut data = serde_json::Map::new();
                data.insert(cr::TOKEN.into(), json!(base64_encode(token.as_bytes())));
                let meta = self.meta(&cr::secret_name(&self.name, app), self.labels());
                match self.kube.apply(&path, &json!({"apiVersion": "v1", "kind": "Secret", "metadata": meta, "type": "Opaque", "data": data})).await {
                    Err(e) if forbidden(&e) => {
                        tracing::warn!(cluster = %self.name, error = %e, "{OLD_ROLE}: the apps keep the admin token");
                        return Ok(());
                    }
                    r => drop(r?),
                }
            }
            let mut key = e.get(cr::KEY).filter(|k| !k.is_empty()).cloned();
            let mut id = old_id.clone();
            let keyed = cr::keyed(spec, app);
            let mut pending = false;
            if keyed && (key.is_none() || old_hash.as_deref() != Some(hash.as_str())) {
                match cr::make_key(admin, app, &tables, old_id.as_deref()).await {
                    Ok((i, k)) => (id, key) = (Some(i), Some(k)),
                    // Coordinators from before keys limited to tables: the
                    // admin token, as before such keys.
                    Err(cr::KeyError::BeforeLimits(e)) => {
                        failed.push(format!("{app}: {e}"));
                        (id, key) = (None, None);
                    }
                    // A timeout, a 5xx, coordinators rolling: make_key makes
                    // the new key before it revokes the old one, so the old
                    // one still works. The Secret is left as it is, its old
                    // key and tables' hash with it (the pod does not roll),
                    // and the next reconcile tries again (audit X-E2).
                    Err(cr::KeyError::Failed(e)) => {
                        failed.push(format!("{app}: {e} (it keeps the key it has; tried again at the next reconcile)"));
                        if key.is_some() {
                            continue;
                        }
                        // No key yet: none until one is made, never the
                        // admin token.
                        (id, pending) = (None, true);
                    }
                }
            }
            if !keyed {
                // The crawler that grows the cluster: the admin token is
                // its key, and the one it had is revoked.
                if let Some(i) = &old_id {
                    admin.call(reqwest::Method::DELETE, &format!("/v1/admin/keys/{i}"), None).await.or_else(|e| if e.contains("404") { Ok(Json::Null) } else { Err(e) })?;
                }
                (id, key) = (None, None);
            }
            let was_pending = have.as_ref().is_some_and(|h| h["metadata"]["annotations"][cr::PENDING] == "true");
            let unchanged = have.is_some() && e.get(cr::TOKEN) == Some(&token) && e.get(cr::KEY) == key.as_ref() && old_id == id && (old_hash.as_deref() == Some(hash.as_str()) || key.is_none()) && was_pending == pending;
            if unchanged {
                continue;
            }
            let mut data = serde_json::Map::new();
            data.insert(cr::TOKEN.into(), json!(base64_encode(token.as_bytes())));
            if let Some(k) = &key {
                data.insert(cr::KEY.into(), json!(base64_encode(k.as_bytes())));
            }
            let mut meta = self.meta(&cr::secret_name(&self.name, app), self.labels());
            let mut notes = serde_json::Map::new();
            if let Some(i) = &id {
                notes.insert(cr::KEY_ID.into(), json!(i));
                notes.insert(cr::TABLES.into(), json!(hash));
            }
            if pending {
                notes.insert(cr::PENDING.into(), json!("true"));
            }
            meta["annotations"] = Json::Object(notes);
            self.kube.apply(&path, &json!({"apiVersion": "v1", "kind": "Secret", "metadata": meta, "type": "Opaque", "data": data})).await?;
            tracing::info!(app, keyed = key.is_some(), "the app's credentials changed");
        }
        match failed.is_empty() {
            true => Ok(()),
            false => Err(format!("an app's own key was not made: {}", failed.join("; "))),
        }
    }

    async fn coordinator_config(&self) -> Result<Option<String>, String> {
        let Some(d) = self.coordinator_object().await? else { return Ok(None) };
        Ok(Some(d["spec"]["template"]["metadata"]["annotations"][CONFIG].as_str().unwrap_or_default().to_string()))
    }

    async fn configure(&self, id: u32, spec: &Spec) -> Result<bool, String> {
        let path = format!("{}/{}", self.sets_path(), self.set_name(id));
        let Some(set) = self.kube.get(&path).await? else { return Ok(false) };
        let want = spec.server_config();
        if set["spec"]["template"]["metadata"]["annotations"][CONFIG] == want.as_str() && set["spec"]["updateStrategy"]["type"] == "OnDelete" {
            return Ok(false);
        }
        // The template only: a set's volume claims cannot change, and its
        // copies restart when the operator says.
        let replicas = set["spec"]["replicas"].as_u64().unwrap_or(0) as usize;
        let with = Cluster { kube: self.kube.clone(), ns: self.ns.clone(), name: self.name.clone(), uid: self.uid.clone(), spec: spec.clone() };
        let fresh = with.statefulset(id, replicas, demo_of(&set));
        let patch = json!({"spec": {"updateStrategy": {"type": "OnDelete", "rollingUpdate": null}, "template": fresh["spec"]["template"]}});
        self.kube.merge(&path, &patch).await?;
        Ok(true)
    }

    async fn stale(&self, spec: &Spec) -> Result<Vec<(u32, usize)>, String> {
        let sel = format!("app.kubernetes.io/instance={},app.kubernetes.io/component=shard", self.name);
        let want = spec.server_config();
        let label = format!("{GROUP}/set");
        let mut out = Vec::new();
        for pod in self.kube.list(&ns_path(&self.ns, "v1", "pods"), &sel).await? {
            if pod["metadata"]["annotations"][CONFIG] == want.as_str() || !pod["metadata"]["deletionTimestamp"].is_null() {
                continue;
            }
            let Some(id) = pod["metadata"]["labels"][&label].as_str().and_then(|v| v.parse::<u32>().ok()) else { continue };
            let prefix = format!("{}-", self.set_name(id));
            if let Some(k) = pod["metadata"]["name"].as_str().and_then(|n| n.strip_prefix(&prefix)).and_then(|k| k.parse::<usize>().ok()) {
                out.push((id, k));
            }
        }
        out.sort();
        Ok(out)
    }

    async fn restart(&self, id: u32, ordinal: usize) -> Result<(), String> {
        self.kube.delete(&format!("{}/{}-{ordinal}", ns_path(&self.ns, "v1", "pods"), self.set_name(id))).await
    }

    async fn grow_volumes(&self, size: &str, apply: bool) -> Result<usize, String> {
        let want = super::quantity(size).ok_or_else(|| format!("the volume size {size:?} is not one Kubernetes reads"))?;
        let sel = format!("app.kubernetes.io/instance={},app.kubernetes.io/component=shard", self.name);
        let path = ns_path(&self.ns, "v1", "persistentvolumeclaims");
        let mut grown = 0;
        for pvc in self.kube.list(&path, &sel).await? {
            let has = pvc["spec"]["resources"]["requests"]["storage"].as_str().and_then(super::quantity).unwrap_or(0);
            let Some(name) = pvc["metadata"]["name"].as_str() else { continue };
            if has < want {
                if apply {
                    self.kube.merge(&format!("{path}/{name}"), &json!({"spec": {"resources": {"requests": {"storage": size}}}})).await?;
                }
                grown += 1;
            }
        }
        Ok(grown)
    }

    fn url(&self, id: u32, ordinal: usize) -> String {
        format!("http://{}-{ordinal}.{}.{}.svc:7701", self.set_name(id), self.servers_service(), self.ns)
    }

    async fn coordinators_rolled(&self) -> Result<super::Rolled, String> {
        let Some(d) = self.kube.get(&self.coordinator_path()).await? else { return Ok(super::Rolled { done: true, text: "no coordinators".into() }) };
        let mut rolled = rolled_of(&d);
        // The coordinators of the other kind (before coordinatorStorage was
        // given or taken away) stop once these all serve.
        if self.kube.get(&self.old_coordinator_path()).await?.is_some() {
            if rolled.done {
                self.kube.delete(&self.old_coordinator_path()).await?;
                rolled.text = format!("{}; the coordinators of before stop", rolled.text);
            } else {
                rolled.text = format!("{}; the coordinators of before serve until then", rolled.text);
            }
            rolled.done = false;
        }
        Ok(rolled)
    }

    async fn trouble(&self, spec: &Spec) -> Result<Vec<super::Trouble>, String> {
        let (server, coordinator) = (spec.server_config(), spec.coordinator_config());
        let label = format!("{GROUP}/set");
        let mut out = Vec::new();
        for pod in self.pods().await? {
            if !pod["metadata"]["deletionTimestamp"].is_null() {
                continue;
            }
            let Some(text) = pod_trouble(&pod) else { continue };
            let name = pod["metadata"]["name"].as_str().unwrap_or_default().to_string();
            let config = pod["metadata"]["annotations"][CONFIG].as_str().unwrap_or_default();
            let copy = pod["metadata"]["labels"][&label].as_str().and_then(|v| v.parse::<u32>().ok()).and_then(|id| {
                let k = name.strip_prefix(&format!("{}-", self.set_name(id)))?.parse::<usize>().ok()?;
                Some((id, k))
            });
            let fresh = if copy.is_some() { config == server } else { config == coordinator };
            out.push(super::Trouble { copy, who: format!("pod {name}"), fresh, text });
        }
        Ok(out)
    }

    async fn preflight(&self, image: &str) -> Result<super::Check, String> {
        use super::Check;
        let pods = self.pods().await?;
        let runs = |p: &Json| p["spec"]["containers"][0]["image"] == image;
        if !pods.is_empty() && pods.iter().all(runs) {
            return Ok(Check::Done("every server runs it already".into()));
        }
        let mut nodes: Vec<String> = pods.iter().filter(|p| p["metadata"]["labels"]["app.kubernetes.io/component"] == "shard").filter_map(|p| p["spec"]["nodeName"].as_str().map(str::to_string)).collect();
        nodes.sort();
        nodes.dedup();
        if nodes.is_empty() {
            return Ok(Check::Done("no copy runs on a node yet".into()));
        }
        let made = self.kube.get(&self.preflight_path()).await?;
        let template = made.as_ref().map(|d| (&d["spec"]["template"]["spec"]["containers"][0]["image"], d["spec"]["replicas"].as_u64().unwrap_or(0)));
        if template.is_none_or(|(i, n)| *i != image || n != nodes.len() as u64) {
            self.kube.apply(&self.preflight_path(), &self.preflight_deployment(image, &nodes)).await?;
            return Ok(Check::Waiting(format!("starting it on {} node{}", nodes.len(), if nodes.len() == 1 { "" } else { "s" })));
        }
        let sel = format!("app.kubernetes.io/instance={},app.kubernetes.io/component=preflight", self.name);
        let tried: Vec<Json> = self.kube.list(&ns_path(&self.ns, "v1", "pods"), &sel).await?.into_iter().filter(|p| p["spec"]["containers"][0]["image"] == image && p["metadata"]["deletionTimestamp"].is_null()).collect();
        let (mut ran, mut no_room, mut pulling) = (Vec::new(), Vec::new(), 0);
        for p in &tried {
            let node = p["spec"]["nodeName"].as_str().unwrap_or("a node").to_string();
            if let Some(t) = pod_trouble(p) {
                return Ok(Check::Failed(format!("on {node}: {t}")));
            }
            let c = &p["status"]["containerStatuses"][0];
            let last = &c["lastState"]["terminated"];
            if last["exitCode"].as_i64().is_some_and(|c| c != 0) {
                return Ok(Check::Failed(format!("on {node}: strata-server --help exited with code {}: {}", last["exitCode"], last["message"].as_str().unwrap_or_default().trim())));
            }
            let unschedulable = p["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == "PodScheduled" && c["status"] == "False" && c["reason"] == "Unschedulable"));
            if c["state"]["running"].is_object() {
                ran.push(node);
            } else if unschedulable {
                no_room.push(p["status"]["conditions"].as_array().and_then(|cs| cs.iter().find(|c| c["type"] == "PodScheduled")).and_then(|c| c["message"].as_str()).unwrap_or("no room").to_string());
            } else {
                pulling += 1;
            }
        }
        if ran.len() + no_room.len() < nodes.len() || pulling > 0 {
            return Ok(Check::Waiting(format!("running on {} of {} nodes, pulling on {pulling}", ran.len(), nodes.len())));
        }
        let mut found = format!("it runs on {} of {} nodes", ran.len(), nodes.len());
        if let Some(why) = no_room.first() {
            found += &format!(" ({} had no room to try it: {why}; their copies pull it as they restart)", no_room.len());
        }
        Ok(Check::Done(found))
    }

    async fn no_room(&self, id: u32, k: usize) -> Result<Option<String>, String> {
        let Some(pod) = self.kube.get(&format!("{}/{}-{k}", ns_path(&self.ns, "v1", "pods"), self.set_name(id))).await? else { return Ok(None) };
        let scheduled = pod["status"]["conditions"].as_array().and_then(|cs| cs.iter().find(|c| c["type"] == "PodScheduled").cloned());
        Ok(scheduled.filter(|c| c["status"] == "False" && c["reason"] == "Unschedulable").map(|c| c["message"].as_str().unwrap_or("no node has room").to_string()))
    }

    async fn end_preflight(&self) -> Result<(), String> {
        self.kube.delete(&self.preflight_path()).await
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

/// A coordinator's link to the `StrataCluster` that runs it (the
/// operator names it in `STRATA_KUBE_CLUSTER`, `namespace/name`), through
/// the API server as the pod's service account: what the command center
/// shows of it, and the sizes it changes.
#[derive(Clone)]
pub struct Link {
    kube: Kube,
    pub namespace: String,
    pub name: String,
}

impl Link {
    /// The link to `namespace/name`, in a pod.
    pub fn new(cluster: &str) -> Result<Link, String> {
        Link::with(cluster, &Options::default())
    }

    /// The link to `namespace/name` through the API server `o` names.
    pub fn with(cluster: &str, o: &Options) -> Result<Link, String> {
        let (ns, name) = cluster.split_once('/').filter(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains('/')).ok_or_else(|| format!("a StrataCluster is namespace/name, not {cluster:?}"))?;
        Ok(Link { kube: Kube::new(o)?, namespace: ns.to_string(), name: name.to_string() })
    }

    fn path(&self) -> String {
        format!("/apis/{GROUP}/{VERSION}/namespaces/{}/{PLURAL}/{}", self.namespace, self.name)
    }

    /// The Lease of the cluster's map, taken as `me` ([`super::lease`]).
    pub fn map_lease(&self, me: &str) -> super::lease::MapLease {
        super::lease::MapLease::new(self.kube.clone(), &self.namespace, &self.name, me)
    }

    /// The resource: its spec, its status and whether the operator has
    /// seen its latest spec.
    pub async fn get(&self) -> Result<Json, String> {
        let c = self.kube.get(&self.path()).await?.ok_or_else(|| format!("no StrataCluster {}/{}", self.namespace, self.name))?;
        let (generation, seen) = (&c["metadata"]["generation"], &c["status"]["observedGeneration"]);
        Ok(json!({
            "managed": true, "namespace": self.namespace, "name": self.name, "spec": c["spec"], "status": c["status"],
            "generation": generation, "settled": !generation.is_null() && generation == seen,
        }))
    }

    /// The spec patch that sets `shards`, `replicas` and `coordinators`
    /// (those given), checked.
    pub fn sizes(shards: Option<usize>, replicas: Option<usize>, coordinators: Option<usize>) -> Result<Json, String> {
        let mut spec = serde_json::Map::new();
        for (k, v, max) in [("shards", shards, crate::slots::SLOTS), ("replicas", replicas, 64), ("coordinators", coordinators, 64)] {
            if let Some(n) = v {
                if n == 0 || n > max {
                    return Err(format!("{k} is 1 to {max}, not {n}"));
                }
                spec.insert(k.into(), json!(n));
            }
        }
        if spec.is_empty() {
            return Err("give shards, replicas or coordinators".into());
        }
        Ok(Json::Object(spec))
    }

    /// Merges `spec` (a patch of the spec: [`Link::sizes`], or any of its
    /// fields) into the cluster's, with `notes` among its annotations: the
    /// operator then takes the cluster there, one step at a time.
    pub async fn change(&self, spec: &Json, notes: &[(&str, String)]) -> Result<Json, String> {
        let notes: serde_json::Map<String, Json> = notes.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        let mut patch = json!({"spec": spec});
        if !notes.is_empty() {
            patch["metadata"] = json!({"annotations": notes});
        }
        self.kube.merge(&self.path(), &patch).await?.ok_or_else(|| format!("no StrataCluster {}/{}", self.namespace, self.name))?;
        self.get().await
    }

    /// The spec patch that takes the cluster back to `version` of its
    /// `status.history`, its `rollout`, `autoscale` and `upgrade` kept as
    /// they are.
    pub fn rollback(now: &Json, version: u64) -> Result<Json, String> {
        let history = now["status"]["history"].as_array().map(Vec::as_slice).unwrap_or_default();
        let then = history.iter().find(|v| v["version"] == version).ok_or_else(|| {
            let kept: Vec<String> = history.iter().filter_map(|v| v["version"].as_u64()).map(|v| v.to_string()).collect();
            format!("no version {version} in the cluster's history (it keeps {})", if kept.is_empty() { "none yet".into() } else { kept.join(", ") })
        })?;
        Ok(super::back_to(&now["spec"], &then["spec"]))
    }

    /// Lifts the hold a gate put on the change: the operator goes on, and
    /// its gates count again from now.
    pub async fn resume(&self) -> Result<Json, String> {
        let now = self.get().await?;
        let r = &now["status"]["rollout"];
        let at = r["held"]["atMs"].as_u64().or_else(|| r["failed"]["atMs"].as_u64()).ok_or("no gate or failed check holds the change: nothing to resume (a pause is lifted with rollout.paused: false)")?;
        self.kube.merge(&self.path(), &json!({"metadata": {"annotations": {RESUME: at.to_string()}}})).await?;
        self.get().await
    }
}

impl Link {
    /// Dismisses the autoscaler's suggestion ([`DISMISS`]): it is not
    /// made, and the rules wait their cooldown before they suggest again.
    pub async fn dismiss(&self) -> Result<Json, String> {
        let now = self.get().await?;
        let at = now["status"]["autoscale"]["suggestion"]["atMs"].as_u64().ok_or("the autoscaler suggests nothing now: nothing to dismiss")?;
        self.kube.merge(&self.path(), &json!({"metadata": {"annotations": {DISMISS: at.to_string()}}})).await?;
        self.get().await
    }
}

/// The annotation that resumes a change a gate held (the hold's `atMs`).
pub const RESUME: &str = "strata.chloetech.ai/resume";
/// The annotation that marks a version as a rollback, which no gate
/// holds ([`super::Spec::version`]).
pub const ROLLBACK: &str = "strata.chloetech.ai/rollback";
/// Who made the latest change through a coordinator, and its version
/// (`name version`).
pub const CHANGED_BY: &str = "strata.chloetech.ai/changed-by";
/// The annotation that dismisses the autoscaler's suggestion (its `atMs`).
pub const DISMISS: &str = "strata.chloetech.ai/autoscale-dismiss";
/// Who the autoscaler is in [`CHANGED_BY`].
pub const AUTOSCALER: &str = "autoscaler";
/// Who the upgrader is in [`CHANGED_BY`].
pub const UPGRADER: &str = "upgrader";
/// The operator itself, rolling a failed version back.
pub const OPERATOR: &str = "operator";

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

/// One step for one cluster, and its status: the outcome, what the next
/// step takes from it (`rollout`), the versions of the spec (`history`)
/// and the operator's latest messages (`log`).
pub async fn once(kube: &Kube, c: &Json) -> Outcome {
    let meta = &c["metadata"];
    let (ns, name) = (meta["namespace"].as_str().unwrap_or("default").to_string(), meta["name"].as_str().unwrap_or_default().to_string());
    let spec: Result<Spec, String> = serde_json::from_value(c["spec"].clone()).map_err(|e| format!("spec: {e}"));
    let before: super::Rollout = serde_json::from_value(c["status"]["rollout"].clone()).unwrap_or_default();
    let note = |k: &str| meta["annotations"][k].as_str();
    let out = match &spec {
        Err(e) => super::Outcome { rollout: before, ..super::Outcome::new(super::Phase::Failed, e.clone()) },
        Ok(spec) => {
            let before = super::noted(before, note(RESUME), note(ROLLBACK), spec);
            let cluster = Cluster { kube: kube.clone(), ns: ns.clone(), name: name.clone(), uid: meta["uid"].as_str().unwrap_or_default().to_string(), spec: spec.clone() };
            match common(&cluster).await {
                Err(e) => super::Outcome { rollout: before, ..super::Outcome::new(super::Phase::Failed, e) },
                Ok(admin) => super::reconcile(&cluster.spec, &cluster, &admin, &before).await,
            }
        }
    };
    let mut out = out;
    let now = super::now_ms();
    let mut history = match &spec {
        Ok(spec) => {
            // Who changed it, if the change came through a coordinator.
            let by = note(CHANGED_BY).and_then(|b| b.rsplit_once(' ')).filter(|(_, v)| *v == spec.version()).map(|(who, _)| who);
            super::versions(&c["status"]["history"], spec, meta["generation"].as_u64(), by, now, out.phase == super::Phase::Ready)
        }
        Err(_) => c["status"]["history"].clone(),
    };
    // A version that failed a check goes back to the last one that ran
    // everywhere, and its image is not rolled out again by the upgrader.
    let mut rejected = None;
    if let (Ok(spec), Some(f)) = (&spec, out.rollout.failed.clone()) {
        if let Some(last) = history.as_array_mut().and_then(|h| h.last_mut()).filter(|v| v["hash"] == spec.version().as_str()) {
            last["failed"] = json!({"atMs": f.at_ms, "step": f.step, "text": f.text});
        }
        match rolled_back(kube, c, spec, &history, &f, out.rollout.ungated).await {
            Ok(said) => {
                // The image of a version rolled back, if it was another.
                let target = super::rollback_target(&history, spec, out.rollout.ungated).and_then(|t| t["spec"]["image"].as_str().map(str::to_string));
                if let Some(d) = super::upgrade::digest_of(&spec.image).filter(|_| said.starts_with("rolling back") && target.as_deref() != Some(spec.image.as_str())) {
                    rejected = Some((d, f.text.clone()));
                }
                out.message = said;
            }
            Err(e) => out.message = format!("the version failed: {}; could not roll it back: {e}", f.text),
        }
    }
    let autoscale = match &spec {
        Ok(spec) => autoscaled(kube, c, spec, &out, &history, now).await,
        Err(_) => c["status"]["autoscale"].clone(),
    };
    let upgrade = match &spec {
        Ok(spec) => {
            let cluster = Cluster { kube: kube.clone(), ns: ns.clone(), name: name.clone(), uid: meta["uid"].as_str().unwrap_or_default().to_string(), spec: spec.clone() };
            upgraded(&cluster, c, &out, rejected, now).await
        }
        Err(_) => c["status"]["upgrade"].clone(),
    };
    let status = json!({
        "phase": out.phase, "message": out.message, "shards": out.shards, "observedGeneration": meta["generation"],
        "rollout": out.rollout, "history": history, "log": super::logged(&c["status"]["log"], out.phase, &out.message, now),
        "autoscale": autoscale, "upgrade": upgrade,
    });
    // The autoscaler's readings alone rewrite the status once a minute.
    let fresh = c["status"]["autoscale"]["atMs"].as_u64().is_some_and(|t| now.saturating_sub(t) < 60_000);
    let quiet = |s: &Json| {
        let mut s = s.clone();
        if let Some(a) = s["autoscale"].as_object_mut().filter(|_| fresh) {
            for k in ["busy", "full", "atMs"] {
                a.remove(k);
            }
        }
        s
    };
    if !same(&quiet(&c["status"]), &quiet(&status)) {
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

/// What becomes of a version that failed (`f`): with `onFailure:
/// rollback`, the spec goes back to the last version that ran everywhere
/// ([`super::rollback_target`]), marked as a rollback ([`ROLLBACK`]), which
/// no gate holds, and as the operator's change ([`CHANGED_BY`]); a version
/// that is a rollback itself, or one with none before it, is held. Said in
/// words.
async fn rolled_back(kube: &Kube, c: &Json, spec: &Spec, history: &Json, f: &super::Failed, rollback: bool) -> Result<String, String> {
    if spec.rollout.on_failure == super::OnFailure::Hold {
        return Ok(format!("the version failed: {}; held until it is resumed (POST /v1/admin/kube/resume) or the spec changes", f.text));
    }
    let Some(target) = super::rollback_target(history, spec, rollback) else {
        let why = if rollback { "it is a rollback itself" } else { "no version before it ran everywhere" };
        return Ok(format!("the version failed: {}; held, not rolled back ({why}): fix the spec, or resume to try again", f.text));
    };
    let patch = super::back_to(&c["spec"], &target["spec"]);
    let (_, new) = super::patched(&c["spec"], &patch)?;
    let (ns, name) = (c["metadata"]["namespace"].as_str().unwrap_or("default"), c["metadata"]["name"].as_str().unwrap_or_default());
    let body = json!({"spec": patch, "metadata": {"annotations": {ROLLBACK: new.version(), CHANGED_BY: format!("{OPERATOR} {}", new.version())}}});
    kube.merge(&format!("/apis/{GROUP}/{VERSION}/namespaces/{ns}/{PLURAL}/{name}"), &body).await?;
    let n = target["version"].as_u64().unwrap_or(0);
    tracing::warn!(cluster = %format!("{ns}/{name}"), to = n, why = %f.text, "rolling a failed version back");
    Ok(format!("rolling back to version {n}: the version failed ({}): {}", f.step, f.text))
}

/// What an app engine Deployment runs and whether it is ready on it
/// ([`Cluster::apps_engine`]).
fn engine_state(d: &Json) -> Option<(String, bool)> {
    let want = d["spec"]["replicas"].as_u64().unwrap_or(1);
    if want == 0 {
        return None;
    }
    let image = d["spec"]["template"]["spec"]["containers"].as_array()?.iter().find(|c| c["name"] == "apps")?["image"].as_str()?;
    let digest = super::upgrade::digest_of(image)?;
    let st = &d["status"];
    let seen = st["observedGeneration"].as_u64().unwrap_or(0) >= d["metadata"]["generation"].as_u64().unwrap_or(0);
    let ready = seen && st["updatedReplicas"].as_u64().unwrap_or(0) >= want && st["availableReplicas"].as_u64().unwrap_or(0) >= want && st["replicas"].as_u64().unwrap_or(0) <= want;
    Some((digest, ready))
}

/// The upgrader's step for a cluster ([`super::upgrade`]): every
/// `checkMinutes` it asks the registry what the followed tag names, and
/// with `mode: on` rolls a new image out (by its digest) once the cluster
/// is ready, noted as the upgrader's change ([`CHANGED_BY`]). `rejected`
/// is the image of a version just rolled back. Its state
/// (`status.upgrade`) answered.
pub async fn upgraded(cluster: &Cluster, c: &Json, out: &Outcome, rejected: Option<(String, String)>, now: u64) -> Json {
    use super::upgrade::{available, decide, digest_of, said, Mode, Registry, State};
    let spec = &cluster.spec;
    let u = &spec.upgrade;
    let mut s: State = serde_json::from_value(c["status"]["upgrade"].clone()).unwrap_or_default();
    if let Some((d, why)) = &rejected {
        s.reject(d, now, why);
    }
    if u.mode == Mode::Off && s.rejected.is_empty() && s.latest.is_none() {
        return Json::Null;
    }
    s.mode = u.mode;
    let track = u.tracked(&spec.image);
    if s.due(u, &track, now) {
        let registry = Registry::new();
        s.track = track.clone();
        s.checked_ms = Some(now);
        match registry.resolve(&track).await {
            Ok(i) => {
                if s.latest.as_ref().is_none_or(|l| l.digest != i.digest) {
                    s.note(now, format!("{track} names {}", said(&i)));
                }
                s.latest = Some(i);
                s.error = None;
            }
            Err(e) => s.error = Some(e),
        }
        s.current = match digest_of(&spec.image) {
            Some(d) if s.current.as_ref().is_some_and(|c| c.digest == d) => s.current.clone(),
            Some(_) => registry.resolve(&spec.image).await.ok(),
            None => None,
        };
        // The app engine's tag, which it runs by digest with `on`
        // ([`super::upgrade::apps_image`]): a new image of it restarts the
        // engine, as a new server image rolls the servers.
        match spec.apps.as_ref().map(|a| a.image().to_string()).filter(|i| digest_of(i).is_none()) {
            Some(tag) => match registry.resolve(&tag).await {
                Ok(i) => {
                    if s.apps_track != tag || s.apps.as_ref().is_none_or(|l| l.digest != i.digest) {
                        let runs = if u.mode == Mode::On { "; the app engine runs it" } else { "" };
                        // Said as the app engine's (LIV-15): under the server's
                        // versions, "latest names <commit>" read as the servers'.
                        s.note(now, format!("App engine image: {tag} names {}{runs}", said(&i)));
                    }
                    s.apps = Some(i);
                    s.apps_track = tag;
                }
                Err(e) => tracing::warn!(cluster = %cluster.name, image = %tag, error = %e, "could not ask the registry for the app engine's image"),
            },
            None => {
                s.apps = None;
                s.apps_track = String::new();
            }
        }
    }
    if let Ok(r) = cluster.running_images().await {
        s.running = r;
    }
    // The app engine on a new image of its tag: kept once ready, rejected
    // when it does not come up in time (X-E6).
    if u.mode == Mode::On && s.apps.is_some() {
        if let Some((deployed, ready)) = cluster.apps_engine().await {
            s.engine_seen(&deployed, ready, now, super::upgrade::engine_verify_ms(spec.rollout.verify_seconds));
        }
    }
    s.available = available(&s, &spec.image);
    let ready = out.phase == super::Phase::Ready && out.rollout.failed.is_none();
    if let Some(image) = decide(u, &s, &spec.image, ready) {
        let made = async {
            let patch = json!({"image": image});
            let (_, new) = super::patched(&c["spec"], &patch)?;
            new.check()?;
            let body = json!({"spec": patch, "metadata": {"annotations": {CHANGED_BY: format!("{UPGRADER} {}", new.version())}}});
            cluster.kube.merge(&format!("/apis/{GROUP}/{VERSION}/namespaces/{}/{PLURAL}/{}", cluster.ns, cluster.name), &body).await.map(drop)
        };
        let what = s.latest.as_ref().map(said).unwrap_or_default();
        match made.await {
            Ok(()) => {
                tracing::info!(cluster = %cluster.name, image = %image, "upgrading");
                s.note(now, format!("rolling out {what}"));
                s.available = false;
            }
            Err(e) => s.note(now, format!("could not roll out {what}: {e}")),
        }
    }
    serde_json::to_value(&s).unwrap_or(Json::Null)
}

/// The autoscaler's step for a cluster ([`super::autoscale::step`]): its
/// rules on the numbers `out` read, from its state (`status.autoscale`),
/// which it answers. With `mode: on` it changes the spec here, noted as a
/// person's change through a coordinator is ([`CHANGED_BY`], by
/// [`AUTOSCALER`]); with `suggest` the change waits in the state until a
/// person applies it or dismisses it ([`DISMISS`]).
pub async fn autoscaled(kube: &Kube, c: &Json, spec: &Spec, out: &Outcome, history: &Json, now: u64) -> Json {
    use super::autoscale::{step, Mode, Reading, Sizes, State};
    let a = &spec.autoscale;
    if a.mode == Mode::Off && a.grow.is_none() && a.shrink.is_none() && a.volumes.is_none() {
        return Json::Null;
    }
    let before: State = serde_json::from_value(c["status"]["autoscale"].clone()).unwrap_or_default();
    let latest = history.as_array().and_then(|h| h.last()).and_then(|v| v["atMs"].as_u64());
    let numbers = out.numbers.clone().unwrap_or_default();
    let r = Reading { at_ms: now, busy: numbers.busy, full: numbers.disk_used_ratio };
    let sizes = Sizes { replicas: spec.replicas, shards: spec.shards, volume: spec.storage.size.clone() };
    let (mut s, change) = step(a, before, latest, c["metadata"]["annotations"][DISMISS].as_str(), &r, &sizes, out.phase == super::Phase::Ready);
    if let Some(ch) = change {
        let made = async {
            let (_, new) = super::patched(&c["spec"], &ch.patch)?;
            new.check()?;
            let (ns, name) = (c["metadata"]["namespace"].as_str().unwrap_or("default"), c["metadata"]["name"].as_str().unwrap_or_default());
            let patch = json!({"spec": ch.patch, "metadata": {"annotations": {CHANGED_BY: format!("{AUTOSCALER} {}", new.version())}}});
            kube.merge(&format!("/apis/{GROUP}/{VERSION}/namespaces/{ns}/{PLURAL}/{name}"), &patch).await.map(drop)
        };
        match made.await {
            Ok(()) => {
                tracing::info!(cluster = %c["metadata"]["name"].as_str().unwrap_or_default(), change = %ch.text, "autoscaled");
                s.last_change_ms = Some(now);
                s.note(now, format!("made: {}", ch.text));
            }
            Err(e) => s.note(now, format!("could not make the change ({e}): {}", ch.text)),
        }
    }
    serde_json::to_value(&s).unwrap_or(Json::Null)
}

/// Whether the status the API server holds is `b`: numbers by value (it
/// gives 200.0 back as 200) and a null as a field left out (a merge patch
/// removes a field it sets to null).
fn same(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        (Json::Array(x), Json::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b)),
        (Json::Object(x), Json::Object(y)) => x.keys().chain(y.keys()).all(|k| same(x.get(k).unwrap_or(&Json::Null), y.get(k).unwrap_or(&Json::Null))),
        _ => a == b,
    }
}

/// Whether the API server refused a request as not allowed (403).
fn forbidden(e: &str) -> bool {
    e.contains(" 403 ")
}

/// What an operator is told when its ClusterRole may not make Secrets.
const OLD_ROLE: &str = "the operator may not make, change or delete Secrets in this namespace, so apps get no keys and tokens of their own: bind the ClusterRole strata-operator-secrets here (kubectl -n NS create rolebinding strata-operator-secrets --clusterrole=strata-operator-secrets --serviceaccount=strata-system:strata-operator; operator.yaml binds it in strata), or apply deploy/k8s/operator/operator.yaml again if it is from before 0.3";

/// Applies the objects every cluster has, and the coordinators' client.
async fn common(c: &Cluster) -> Result<Admin, String> {
    c.spec.check()?;
    for (path, obj) in c.common() {
        match c.kube.apply(&path, &obj).await {
            // The coordinators' Role, refused as granting what this
            // operator's ClusterRole does not hold (one from before the
            // map's Lease): made without the Lease, so the cluster is still
            // kept, and its coordinators change the map without one.
            Err(e) if obj["kind"] == "Role" && e.contains(" 403 ") => {
                tracing::warn!(cluster = %c.name, error = %e, "the operator's ClusterRole holds no Leases (it is from an older operator.yaml): the coordinators' Role is made without the cluster map's Lease; apply deploy/k8s/operator/operator.yaml again to give it");
                c.kube.apply(&path, &c.coordinator_role(false)).await?;
            }
            r => drop(r?),
        }
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


    /// N-D4: an app's pod template names the id of the key its Secret
    /// holds, so that a new key (its tables changed) rolls the pod, which
    /// reads the key from its environment once; the admin token's pod is
    /// not marked.
    #[test]
    fn a_new_app_key_rolls_its_pod() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "sheets": {}}));
        let with = |id: Option<&str>| {
            let creds = Creds { key: true, token: true, key_id: id.map(String::from), pending: false };
            rolled(c.sheets_deployment(c.spec.sheets.as_ref().unwrap(), creds.clone()), &creds)
        };
        let (a, b) = (with(Some("k1")), with(Some("k2")));
        assert_eq!(a["spec"]["template"]["metadata"]["annotations"][super::super::credentials::KEY_ID], "k1");
        assert_ne!(super::super::short_hash(&a), super::super::short_hash(&b), "a new key changes the Deployment");
        assert_eq!(super::super::short_hash(&a), super::super::short_hash(&with(Some("k1"))));
        let admin = rolled(c.sheets_deployment(c.spec.sheets.as_ref().unwrap(), Creds::default()), &Creds { key_id: Some("k1".into()), ..Default::default() });
        assert!(admin["spec"]["template"]["metadata"]["annotations"].is_null(), "{admin}");
    }

    /// Each app's own key and token, from its credentials Secret, once
    /// the operator made them; the admin token until then; the crawler
    /// that grows keeps the admin token as its key.
    #[test]
    fn apps_take_their_own_credentials() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "crawler": {"grow": false}, "sheets": {}, "chloectx": {}, "apps": {}}));
        let env = |d: &Json| d["spec"]["template"]["spec"]["containers"][0]["env"].clone();
        let from = |d: &Json, name: &str| env(d).as_array().unwrap().iter().find(|e| e["name"] == name).map(|e| e["valueFrom"]["secretKeyRef"].clone());
        let own = Creds { key: true, token: true, key_id: None, pending: false };
        let d = c.sheets_deployment(c.spec.sheets.as_ref().unwrap(), own.clone());
        assert_eq!(from(&d, "STRATA_KEY"), Some(json!({"name": "orders-sheets-credentials", "key": "key"})));
        assert_eq!(from(&d, "STRATA_SHEETS_TOKEN"), Some(json!({"name": "orders-sheets-credentials", "key": "token"})));
        assert!(!env(&d).to_string().contains("\"admin\""), "{}", env(&d));
        let d = c.sheets_deployment(c.spec.sheets.as_ref().unwrap(), Creds::default());
        assert_eq!(from(&d, "STRATA_KEY"), Some(json!({"name": "admin", "key": "token"})));
        let d = c.crawler_deployment(c.spec.crawler.as_ref().unwrap(), Creds { key: false, token: true, key_id: None, pending: false });
        assert_eq!(from(&d, "STRATA_KEY"), Some(json!({"name": "admin", "key": "token"})));
        assert_eq!(from(&d, "STRATA_CRAWL_TOKEN"), Some(json!({"name": "orders-crawler-credentials", "key": "token"})));
        let d = c.chloectx_deployment(c.spec.chloectx.as_ref().unwrap(), own.clone());
        assert_eq!(from(&d, "CHLOEDB_KEY"), Some(json!({"name": "orders-chloectx-credentials", "key": "key"})));
        assert_eq!(from(&d, "CHLOECTX_APP_TOKEN"), Some(json!({"name": "orders-chloectx-credentials", "key": "token"})));
        let d = c.apps_deployment(c.spec.apps.as_ref().unwrap(), own.clone());
        assert_eq!(from(&d, "STRATA_TOKEN"), Some(json!({"name": "orders-apps-credentials", "key": "key"})));
        assert_eq!(from(&d, "STRATA_APP_TOKEN"), Some(json!({"name": "orders-apps-credentials", "key": "token"})));
        // The coordinators read each app's token from a folder of them.
        let co = c.coordinator_workload(&[vec!["http://a:7701".into()]], 1);
        let pod = &co["spec"]["template"]["spec"];
        let sources = pod["volumes"][0]["projected"]["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 4);
        assert_eq!(sources[1]["secret"], json!({"name": "orders-sheets-credentials", "optional": true, "items": [{"key": "token", "path": "sheets"}]}));
        assert_eq!(pod["containers"][0]["volumeMounts"][0]["mountPath"], super::super::credentials::TOKEN_DIR);
        assert!(from(&co, "STRATA_APP_TOKEN").is_some() && env(&co).to_string().contains("STRATA_APP_TOKEN_DIR"));
        // A cluster without apps mounts nothing.
        let none = cluster(json!({"demo": 10, "tokenSecret": "admin"}));
        assert!(none.coordinator_workload(&[vec!["http://a:7701".into()]], 1)["spec"]["template"]["spec"]["volumes"].is_null());
        // base64 both ways.
        for t in ["", "a", "ab", "abc", "sk_0a1b_00ff"] {
            assert_eq!(base64_decode(&base64_encode(t.as_bytes())).unwrap(), t.as_bytes());
        }
        assert_eq!(base64_encode(b"ab"), "YWI=");
    }

    #[test]
    fn a_follower_runs_one_follow_process_with_the_sources_key_and_its_own_token() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner", "imagePullPolicy": "Always",
            "follow": {"source": "https://east.example:7700", "name": "east", "keySecret": "east-feed", "caSecret": "east-ca", "askedMs": 1700000000000u64}}));
        let f = c.spec.follow.clone().unwrap();
        let d = c.follow_deployment(&f);
        assert_eq!(d["metadata"]["name"], "orders-follow");
        assert_eq!(d["metadata"]["ownerReferences"][0]["uid"], "u-1");
        assert_eq!((&d["spec"]["replicas"], &d["spec"]["strategy"]["type"]), (&json!(1), &json!("Recreate")));
        let pod = &d["spec"]["template"]["spec"];
        assert_eq!(pod["automountServiceAccountToken"], false);
        let k = &pod["containers"][0];
        assert_eq!(k["imagePullPolicy"], "Always");
        let args: Vec<&str> = k["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
        assert_eq!(args, ["follow", "--source", "https://east.example:7700", "--target", "http://orders:7700", "--name", "east", "--once", "1700000000000", "--source-ca", "/etc/strata-follow/ca.crt"]);
        let env = k["env"].as_array().unwrap();
        let get = |n: &str| env.iter().find(|e| e["name"] == n).cloned().unwrap_or_default();
        assert_eq!(get("STRATA_FOLLOW_KEY")["valueFrom"]["secretKeyRef"], json!({"name": "east-feed", "key": "key"}));
        assert_eq!(get("STRATA_CLUSTER_TOKEN")["valueFrom"]["secretKeyRef"], json!({"name": "inner", "key": "token"}), "the cluster's token, not the admin's");
        assert_eq!(pod["volumes"][0]["secret"]["secretName"], "east-ca");
        assert_eq!(k["volumeMounts"][0]["mountPath"], "/etc/strata-follow");
        // Paused: no pod. Without a cluster token, the token.
        let paused = cluster(json!({"tokenSecret": "admin", "follow": {"source": "http://east:7700", "name": "east", "paused": true}}));
        let d = paused.follow_deployment(paused.spec.follow.as_ref().unwrap());
        assert_eq!(d["spec"]["replicas"], 0);
        let env = d["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().clone();
        assert!(env.iter().any(|e| e["name"] == "STRATA_CLUSTER_TOKEN" && e["valueFrom"]["secretKeyRef"]["name"] == "admin"));
        assert!(!env.iter().any(|e| e["name"] == "STRATA_FOLLOW_KEY"));
        // Following is not a version of the cluster: no server restarts.
        let none = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner", "imagePullPolicy": "Always"}));
        assert_eq!(none.spec.version(), c.spec.version());
        assert_eq!(none.spec.server_config(), c.spec.server_config());
    }

    #[test]
    fn a_crawler_runs_beside_the_cluster_and_the_coordinators_list_it() {
        let none = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner"}));
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner", "crawler": {"splitRows": 5000}}));
        let d = c.crawler_deployment(c.spec.crawler.as_ref().unwrap(), Creds::default());
        assert_eq!(d["metadata"]["name"], "orders-crawler");
        assert_eq!(d["metadata"]["ownerReferences"][0]["uid"], "u-1");
        assert_eq!((&d["spec"]["replicas"], &d["spec"]["strategy"]["type"]), (&json!(1), &json!("Recreate")));
        let pod = &d["spec"]["template"]["spec"];
        assert_eq!(pod["automountServiceAccountToken"], false);
        let k = &pod["containers"][0];
        assert_eq!(k["command"], json!(["strata-crawl"]));
        assert_eq!(k["image"], c.spec.image.as_str(), "the cluster's image, which carries strata-crawl");
        assert_eq!(k["args"], json!(["serve", "--listen", "0.0.0.0:7800", "--strata", "http://orders:7700", "--grow", "kube", "--split-rows", "5000"]));
        let env = k["env"].as_array().unwrap();
        let secret = |n: &str| env.iter().find(|e| e["name"] == n).map(|e| e["valueFrom"]["secretKeyRef"].clone()).unwrap_or_default();
        // The admin token writes and grows the cluster, and is what its API takes.
        assert_eq!(secret("STRATA_KEY"), json!({"name": "admin", "key": "token"}));
        assert_eq!(secret("STRATA_CRAWL_TOKEN"), json!({"name": "admin", "key": "token"}));
        assert!(!env.iter().any(|e| e["name"] == "STRATA_CLUSTER_TOKEN"), "never the cluster's token");
        let s = c.crawler_service();
        assert_eq!(s["metadata"]["name"], "orders-crawler");
        assert_eq!(s["spec"]["selector"], d["spec"]["selector"]["matchLabels"]);
        assert_eq!(s["spec"]["ports"][0]["port"], 7800);
        // The coordinators list it, with the token it takes; the servers
        // do not restart for it, the coordinators do.
        let coord = c.coordinator_workload(&[vec!["http://a:7701".into()]], 1);
        let env = coord["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().clone();
        assert!(env.iter().any(|e| e["name"] == "STRATA_APPS" && e["value"] == "crawler=http://orders-crawler:7800"));
        assert!(env.iter().any(|e| e["name"] == "STRATA_APP_TOKEN" && e["valueFrom"]["secretKeyRef"]["name"] == "admin"));
        let plain = none.coordinator_workload(&[vec!["http://a:7701".into()]], 1);
        assert!(!plain["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().iter().any(|e| e["name"] == "STRATA_APPS"));
        assert_eq!(c.spec.server_config(), none.spec.server_config());
        assert_ne!(c.spec.coordinator_config(), none.spec.coordinator_config());
        // A cluster without one keeps the hashes it had before spec.crawler.
        let before = json!({"image": none.spec.image, "imagePullPolicy": null, "resources": null, "args": [], "tokenSecret": "admin", "clusterTokenSecret": "inner"});
        assert_eq!(none.spec.coordinator_config(), super::super::short_hash(&before));
        assert!(!serde_json::to_value(&none.spec).unwrap().as_object().unwrap().contains_key("crawler"));
        // Paused: no pod.
        let paused = cluster(json!({"tokenSecret": "admin", "crawler": {"paused": true}}));
        assert_eq!(paused.crawler_deployment(paused.spec.crawler.as_ref().unwrap(), Creds::default())["spec"]["replicas"], 0);
    }

    #[test]
    fn network_operations_run_beside_the_cluster_with_their_own_key() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner", "sheets": {}, "netops": {"args": ["--fault-every", "0"]}}));
        let n = c.spec.netops.as_ref().unwrap();
        let d = c.netops_deployment(n, Creds::default());
        assert_eq!(d["metadata"]["name"], "orders-netops");
        assert_eq!(d["spec"]["replicas"], 1);
        assert_eq!(d["spec"]["strategy"]["type"], "Recreate");
        let k = &d["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(k["command"], json!(["chloedb-netops"]));
        assert_eq!(k["image"], c.spec.image.as_str(), "the cluster's image, which carries chloedb-netops");
        assert_eq!(k["args"], json!(["serve", "--listen", "0.0.0.0:7840", "--strata", "http://orders:7700", "--fault-every", "0"]));
        let secret = |d: &Json, name: &str| d["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().iter().find(|e| e["name"] == name).map(|e| e["valueFrom"]["secretKeyRef"].clone());
        // The admin token until the app has its own key and token.
        assert_eq!(secret(&d, "CHLOEDB_KEY"), Some(json!({"name": "admin", "key": "token"})));
        assert_eq!(secret(&d, "CHLOEDB_NETOPS_TOKEN"), Some(json!({"name": "admin", "key": "token"})));
        let own = c.netops_deployment(n, Creds { key: true, token: true, key_id: None, pending: false });
        assert_eq!(secret(&own, "CHLOEDB_KEY"), Some(json!({"name": "orders-netops-credentials", "key": "key"})));
        assert_eq!(secret(&own, "CHLOEDB_NETOPS_TOKEN"), Some(json!({"name": "orders-netops-credentials", "key": "token"})));
        let s = c.netops_service();
        assert_eq!(s["metadata"]["name"], "orders-netops");
        assert_eq!(s["spec"]["ports"][0]["port"], 7840);
        assert_eq!(s["spec"]["selector"]["app.kubernetes.io/component"], "netops");
        assert_eq!(c.spec.apps(), ["sheets", "netops"]);
        assert_eq!(c.app_url("netops"), "http://orders-netops:7840");
        assert_eq!(super::super::credentials::spec_tables(&c.spec, "netops"), ["netops_*=admin"]);
        let paused = cluster(json!({"tokenSecret": "admin", "netops": {"paused": true}}));
        assert_eq!(paused.netops_deployment(paused.spec.netops.as_ref().unwrap(), Creds::default())["spec"]["replicas"], 0);
    }

    #[test]
    fn spreadsheets_run_beside_the_cluster_and_the_coordinators_list_them_with_the_crawler() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner", "sheets": {}}));
        let d = c.sheets_deployment(c.spec.sheets.as_ref().unwrap(), Creds::default());
        assert_eq!(d["metadata"]["name"], "orders-sheets");
        assert_eq!(d["metadata"]["ownerReferences"][0]["uid"], "u-1");
        assert_eq!((&d["spec"]["replicas"], &d["spec"]["strategy"]["type"]), (&json!(1), &json!("Recreate")));
        let k = &d["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(k["command"], json!(["strata-sheets"]));
        assert_eq!(k["image"], c.spec.image.as_str(), "the cluster's image, which carries strata-sheets");
        assert_eq!(k["args"], json!(["serve", "--listen", "0.0.0.0:7820", "--strata", "http://orders:7700"]));
        let env = k["env"].as_array().unwrap();
        let secret = |n: &str| env.iter().find(|e| e["name"] == n).map(|e| e["valueFrom"]["secretKeyRef"].clone()).unwrap_or_default();
        assert_eq!(secret("STRATA_KEY"), json!({"name": "admin", "key": "token"}));
        assert_eq!(secret("STRATA_SHEETS_TOKEN"), json!({"name": "admin", "key": "token"}));
        assert!(!env.iter().any(|e| e["name"] == "STRATA_CLUSTER_TOKEN"), "never the cluster's token");
        let s = c.sheets_service();
        assert_eq!(s["metadata"]["name"], "orders-sheets");
        assert_eq!(s["spec"]["selector"], d["spec"]["selector"]["matchLabels"]);
        assert_eq!(s["spec"]["ports"][0]["port"], 7820);
        let env_of = |c: &Cluster| c.coordinator_workload(&[vec!["http://a:7701".into()]], 1)["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().clone();
        let apps = |c: &Cluster| env_of(c).iter().find(|e| e["name"] == "STRATA_APPS").map(|e| e["value"].as_str().unwrap().to_string());
        assert_eq!(apps(&c).as_deref(), Some("sheets=http://orders-sheets:7820"));
        let both = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner", "crawler": {}, "sheets": {}}));
        assert_eq!(apps(&both).as_deref(), Some("crawler=http://orders-crawler:7800,sheets=http://orders-sheets:7820"));
        assert_eq!(env_of(&both).iter().filter(|e| e["name"] == "STRATA_APP_TOKEN").count(), 1);
        // The servers do not restart for it; the coordinators do. A
        // cluster with the crawler alone has the hash it had and the apps'
        // token folder.
        let crawler = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner", "crawler": {}}));
        assert_eq!(both.spec.server_config(), crawler.spec.server_config());
        assert_ne!(both.spec.coordinator_config(), crawler.spec.coordinator_config());
        let mut j = json!({"image": crawler.spec.image, "imagePullPolicy": null, "resources": null, "args": [], "tokenSecret": "admin", "clusterTokenSecret": "inner"});
        j["apps"] = json!(["crawler"]);
        j["appTokens"] = json!(super::super::credentials::TOKEN_DIR);
        assert_eq!(crawler.spec.coordinator_config(), super::super::short_hash(&j));
        let paused = cluster(json!({"tokenSecret": "admin", "sheets": {"paused": true}}));
        assert_eq!(paused.sheets_deployment(paused.spec.sheets.as_ref().unwrap(), Creds::default())["spec"]["replicas"], 0);
    }

    #[test]
    fn chloectx_runs_beside_the_cluster_with_its_model_and_the_clusters_model_key() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner", "imagePullPolicy": "Always",
            "secretEnv": [{"name": "ANTHROPIC_API_KEY", "secret": "anthropic"}], "sheets": {}, "chloectx": {}}));
        let d = c.chloectx_deployment(c.spec.chloectx.as_ref().unwrap(), Creds::default());
        assert_eq!(d["metadata"]["name"], "orders-chloectx");
        assert_eq!(d["metadata"]["ownerReferences"][0]["uid"], "u-1");
        assert_eq!((&d["spec"]["replicas"], &d["spec"]["strategy"]["type"]), (&json!(1), &json!("Recreate")));
        let pod = &d["spec"]["template"]["spec"];
        assert_eq!((&pod["automountServiceAccountToken"], &pod["enableServiceLinks"]), (&json!(false), &json!(false)));
        assert!(pod["volumes"][0]["emptyDir"].is_object(), "the model is the pod's own, downloaded at each start");
        let k = &pod["containers"][0];
        assert_eq!(k["command"], json!(["chloectx"]));
        assert_eq!(k["image"], c.spec.image.as_str(), "the cluster's image, which carries chloectx");
        assert_eq!(k["imagePullPolicy"], "Always");
        assert_eq!(k["args"], json!(["app", "--server", "http://orders:7700", "--host", "0.0.0.0", "--port", "7760", "--embedder", "static"]));
        assert_eq!(k["volumeMounts"][0]["mountPath"], "/models");
        let env = k["env"].as_array().unwrap();
        let secret = |n: &str| env.iter().find(|e| e["name"] == n).map(|e| e["valueFrom"]["secretKeyRef"].clone()).unwrap_or_default();
        assert_eq!(secret("CHLOEDB_KEY"), json!({"name": "admin", "key": "token"}));
        assert_eq!(secret("CHLOECTX_APP_TOKEN"), json!({"name": "admin", "key": "token"}));
        assert_eq!(secret("ANTHROPIC_API_KEY"), json!({"name": "anthropic", "key": "key"}));
        assert!(!env.iter().any(|e| e["name"] == "STRATA_CLUSTER_TOKEN"), "never the cluster's token");
        assert!(env.iter().any(|e| e["name"] == "CHLOECTX_MODELS" && e["value"] == "/models"));
        let init = &pod["initContainers"][0];
        assert_eq!(init["command"], json!(["chloectx", "fetch-model"]));
        assert_eq!((&init["image"], &init["volumeMounts"], &init["imagePullPolicy"]), (&k["image"], &k["volumeMounts"], &json!("Always")));
        assert!(init["env"].as_array().unwrap().iter().all(|e| e["valueFrom"].is_null()), "no secret in the download");
        let s = c.chloectx_service();
        assert_eq!(s["metadata"]["name"], "orders-chloectx");
        assert_eq!(s["spec"]["selector"], d["spec"]["selector"]["matchLabels"]);
        assert_eq!(s["spec"]["ports"][0]["port"], 7760);
        let env = c.coordinator_workload(&[vec!["http://a:7701".into()]], 1)["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().clone();
        let apps = env.iter().find(|e| e["name"] == "STRATA_APPS").and_then(|e| e["value"].as_str()).unwrap_or_default().to_string();
        assert_eq!(apps, "sheets=http://orders-sheets:7820,chloectx=http://orders-chloectx:7760");
        // The servers do not restart for it.
        let sheets = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner", "imagePullPolicy": "Always", "secretEnv": [{"name": "ANTHROPIC_API_KEY", "secret": "anthropic"}], "sheets": {}}));
        assert_eq!(c.spec.server_config(), sheets.spec.server_config());
        assert_ne!(c.spec.coordinator_config(), sheets.spec.coordinator_config());
        // A cluster without a token: its ChloeCtx is open too, and says so.
        let open = cluster(json!({"chloectx": {}}));
        let d = open.chloectx_deployment(open.spec.chloectx.as_ref().unwrap(), Creds::default());
        let args = d["spec"]["template"]["spec"]["containers"][0]["args"].as_array().unwrap().clone();
        assert_eq!(args.last(), Some(&json!("--insecure")), "{args:?}");
        // No model to download with the hash embedder; paused: no pod.
        let hash = cluster(json!({"tokenSecret": "admin", "chloectx": {"embedder": "hash:256"}}));
        let d = hash.chloectx_deployment(hash.spec.chloectx.as_ref().unwrap(), Creds::default());
        assert!(d["spec"]["template"]["spec"]["initContainers"].is_null());
        assert_eq!(d["spec"]["template"]["spec"]["containers"][0]["args"][8], "hash:256");
        let paused = cluster(json!({"tokenSecret": "admin", "chloectx": {"paused": true}}));
        assert_eq!(paused.chloectx_deployment(paused.spec.chloectx.as_ref().unwrap(), Creds::default())["spec"]["replicas"], 0);
    }

    #[test]
    fn the_app_engines_deployment_is_ready_once_its_new_pod_is_available() {
        let d = format!("sha256:{}", "e".repeat(64));
        let dep = |gen: u64, observed: u64, updated: u64, available: u64, replicas: u64| {
            json!({"metadata": {"generation": gen}, "spec": {"replicas": 1, "template": {"spec": {"containers": [{"name": "caddy", "image": "caddy:2"}, {"name": "apps", "image": format!("ghcr.io/x/apps@{d}")}]}}},
                   "status": {"observedGeneration": observed, "updatedReplicas": updated, "availableReplicas": available, "replicas": replicas}})
        };
        assert_eq!(engine_state(&dep(3, 3, 1, 1, 1)), Some((d.clone(), true)));
        assert_eq!(engine_state(&dep(3, 2, 1, 1, 1)), Some((d.clone(), false)), "the controller has not seen the change");
        assert_eq!(engine_state(&dep(3, 3, 1, 0, 1)), Some((d.clone(), false)), "the new pod is not ready");
        assert_eq!(engine_state(&dep(3, 3, 0, 1, 1)), Some((d.clone(), false)), "an old pod is the one available");
        let mut paused = dep(3, 3, 0, 0, 0);
        paused["spec"]["replicas"] = json!(0);
        assert_eq!(engine_state(&paused), None);
        let mut tag = dep(3, 3, 1, 1, 1);
        tag["spec"]["template"]["spec"]["containers"][1]["image"] = json!("ghcr.io/x/apps:latest");
        assert_eq!(engine_state(&tag), None, "a tag: not the upgrader's");
    }

    #[test]
    fn the_app_engine_runs_beside_the_cluster_with_its_volume_and_the_clusters_model_key() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner", "imagePullPolicy": "Always",
            "secretEnv": [{"name": "ANTHROPIC_API_KEY", "secret": "anthropic"}],
            "apps": {"publicUrl": "https://studio.example", "serviceType": "NodePort", "nodePort": 30731, "spendCapUsd": 40, "env": [{"name": "STRATA_APPS_MODEL", "value": "m"}]}}));
        let a = c.spec.apps.as_ref().unwrap();
        let d = c.apps_deployment(a, Creds::default());
        assert_eq!(d["metadata"]["name"], "orders-apps");
        assert_eq!(d["metadata"]["ownerReferences"][0]["uid"], "u-1");
        assert_eq!((&d["spec"]["replicas"], &d["spec"]["strategy"]["type"]), (&json!(1), &json!("Recreate")));
        let pod = &d["spec"]["template"]["spec"];
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
        assert_eq!(pod["volumes"][0]["persistentVolumeClaim"]["claimName"], "orders-apps");
        let k = &pod["containers"][0];
        assert_eq!(k["image"], super::super::apps::IMAGE, "an image of its own, not the cluster's");
        assert_eq!(k["imagePullPolicy"], "Always");
        assert_eq!(k["args"], json!(["--strata", "http://orders:7700", "--host", "0.0.0.0", "--port", "7730", "--data", "/data"]));
        assert_eq!(k["volumeMounts"][0]["mountPath"], "/data");
        let env = k["env"].as_array().unwrap();
        let secret = |n: &str| env.iter().find(|e| e["name"] == n).map(|e| e["valueFrom"]["secretKeyRef"].clone()).unwrap_or_default();
        let value = |n: &str| env.iter().find(|e| e["name"] == n).map(|e| e["value"].clone()).unwrap_or_default();
        // The admin token keeps its apps in the cluster and is what the
        // coordinators send it; the model key is the cluster's.
        assert_eq!(secret("STRATA_TOKEN"), json!({"name": "admin", "key": "token"}));
        assert_eq!(secret("STRATA_APP_TOKEN"), json!({"name": "admin", "key": "token"}));
        assert_eq!(secret("ANTHROPIC_API_KEY"), json!({"name": "anthropic", "key": "key"}));
        assert!(!env.iter().any(|e| e["name"] == "STRATA_CLUSTER_TOKEN"), "never the cluster's token");
        assert_eq!((value("STRATA_APPS_PUBLIC_URL"), value("STRATA_APPS_SPEND_CAP_USD"), value("STRATA_APPS_MODEL")), (json!("https://studio.example"), json!("40"), json!("m")));
        let s = c.apps_service(a);
        assert_eq!(s["metadata"]["name"], "orders-apps");
        assert_eq!(s["spec"]["selector"], d["spec"]["selector"]["matchLabels"]);
        assert_eq!((&s["spec"]["type"], &s["spec"]["ports"][0]["port"], &s["spec"]["ports"][0]["nodePort"]), (&json!("NodePort"), &json!(7730), &json!(30731)));
        let v = c.apps_claim(a);
        assert_eq!((&v["metadata"]["name"], &v["spec"]["resources"]["requests"]["storage"]), (&json!("orders-apps"), &json!("1Gi")));
        assert_eq!(v["metadata"]["ownerReferences"][0]["uid"], "u-1", "it goes with the cluster");
        let env_of = |c: &Cluster| c.coordinator_workload(&[vec!["http://a:7701".into()]], 1)["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().clone();
        let apps = env_of(&c).iter().find(|e| e["name"] == "STRATA_APPS").map(|e| e["value"].clone());
        assert_eq!(apps, Some(json!("apps=http://orders-apps:7730")));
        let plain = cluster(json!({"demo": 10, "tokenSecret": "admin", "apps": {"image": "registry.example/apps:1", "paused": true, "storage": {"size": "2Gi", "storageClassName": "fast"}}}));
        let a = plain.spec.apps.as_ref().unwrap();
        let d = plain.apps_deployment(a, Creds::default());
        assert_eq!(d["spec"]["replicas"], 0);
        let k = &d["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(k["image"], "registry.example/apps:1");
        assert!(k.get("imagePullPolicy").is_none());
        assert!(!k["env"].as_array().unwrap().iter().any(|e| e["name"] == "STRATA_APPS_PUBLIC_URL"));
        assert_eq!(plain.apps_service(a)["spec"]["type"], "ClusterIP");
        assert!(plain.apps_service(a)["spec"]["ports"][0].get("nodePort").is_none());
        assert_eq!(plain.apps_claim(a)["spec"]["storageClassName"], "fast");
        assert_eq!(d["spec"]["template"]["spec"]["containers"].as_array().unwrap().len(), 1, "no Caddy without publish");
    }

    #[test]
    fn published_apps_get_a_load_balancer_and_caddy_for_their_certificates() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "apps": {"publish": {"domain": "sslip", "email": "ops@example.com"}}}));
        let a = c.spec.apps.as_ref().unwrap();
        let value = |d: &Json, n: &str| d["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().iter().find(|e| e["name"] == n).map(|e| e["value"].clone());
        // sslip has no domain until the load balancer has an address.
        let d = c.apps_deployment(a, Creds::default());
        assert_eq!(value(&d, "STRATA_APPS_DOMAIN"), None);
        let d = c.apps_deployment_at(a, Some("203-0-113-5.sslip.io"), Creds::default());
        assert_eq!(value(&d, "STRATA_APPS_DOMAIN"), Some(json!("203-0-113-5.sslip.io")));
        let pod = &d["spec"]["template"]["spec"];
        let caddy = &pod["containers"][1];
        assert_eq!((&caddy["name"], &caddy["image"]), (&json!("caddy"), &json!(super::super::apps::CADDY_IMAGE)));
        assert!(caddy["env"][0]["value"].as_str().unwrap().contains("ask http://127.0.0.1:7730/api/tls/ask"));
        assert_eq!((&caddy["volumeMounts"][0]["claimName"], &caddy["volumeMounts"][0]["subPath"]), (&Json::Null, &json!("caddy")), "its certificates on the engine's volume");
        assert_eq!(pod["securityContext"]["runAsNonRoot"], true, "Caddy runs as the engine's user, on ports over 1024");
        let s = c.apps_public_service();
        assert_eq!((&s["metadata"]["name"], &s["spec"]["type"]), (&json!("orders-apps-public"), &json!("LoadBalancer")));
        assert_eq!(s["spec"]["selector"], d["spec"]["selector"]["matchLabels"]);
        assert_eq!(s["spec"]["ports"], json!([{"name": "http", "port": 80, "targetPort": 8080}, {"name": "https", "port": 443, "targetPort": 8443}]));
        // A domain of one's own is known at once.
        let own = cluster(json!({"demo": 10, "tokenSecret": "admin", "apps": {"publish": {"domain": "apps.example.com"}}}));
        assert_eq!(value(&own.apps_deployment(own.spec.apps.as_ref().unwrap(), Creds::default()), "STRATA_APPS_DOMAIN"), Some(json!("apps.example.com")));
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
        // The servers run as user 10001, which the pod says (the image's
        // user is root, for older operators): its volume is that group's,
        // and files a server wrote as root before are given to it.
        let sc = &s["spec"]["template"]["spec"]["securityContext"];
        assert_eq!((&sc["fsGroup"], &sc["fsGroupChangePolicy"]), (&json!(10001), &json!("OnRootMismatch")));
        assert_eq!((&sc["runAsUser"], &sc["runAsGroup"], &sc["runAsNonRoot"]), (&json!(10001), &json!(10001), &json!(true)));
        let claim = &s["spec"]["volumeClaimTemplates"][0];
        assert_eq!((&claim["spec"]["resources"]["requests"]["storage"], &claim["spec"]["storageClassName"]), (&json!("5Gi"), &json!("fast")));
        let env = &s["spec"]["template"]["spec"]["containers"][0]["env"];
        let get = |n: &str| env.as_array().unwrap().iter().find(|e| e["name"] == n).cloned();
        assert_eq!(get("STRATA_DEMO_SHARD").unwrap()["value"], "1/2");
        assert_eq!(get("STRATA_ARGS").unwrap()["value"], "'--max-rows' '5'\\''0'");
        assert_eq!(get("STRATA_TOKEN").unwrap()["valueFrom"]["secretKeyRef"]["name"], "t");
        // New settings reach a copy when the operator restarts it: its pod
        // carries the hash of the settings it started with.
        assert_eq!(s["spec"]["updateStrategy"], json!({"type": "OnDelete"}));
        assert_eq!(s["spec"]["template"]["metadata"]["annotations"][CONFIG], json!(c.spec.server_config()));
        // A set added later loads nothing.
        let later = c.statefulset(2, 3, None);
        assert!(later["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().iter().all(|e| e["name"] != "STRATA_DEMO"));
    }

    #[test]
    fn the_coordinators_are_given_every_shards_servers() {
        let c = cluster(json!({"demo": 10, "coordinatorArgs": ["--hedge-ms", "40"]}));
        let d = c.coordinator_workload(&[vec!["a".into(), "b".into()], vec!["c".into()]], 2);
        let args = &d["spec"]["template"]["spec"]["containers"][0]["args"];
        assert_eq!(args[1], "a|b,c");
        assert!(args.as_array().unwrap().contains(&json!("--no-token")));
        assert_eq!(args.as_array().unwrap()[args.as_array().unwrap().len() - 2..], [json!("--hedge-ms"), json!("40")]);
        assert_eq!(d["spec"]["template"]["metadata"]["annotations"][CONFIG], json!(c.spec.coordinator_config()));
        assert_eq!(d["spec"]["replicas"], 2);
        assert_eq!(d["spec"]["template"]["spec"]["securityContext"], json!({"runAsUser": 10001, "runAsGroup": 10001, "runAsNonRoot": true}));
        let pre = c.preflight_deployment("chloedb-server:new", &["n1".into()]);
        assert_eq!(pre["spec"]["template"]["spec"]["securityContext"]["runAsUser"], 10001, "the pre-flight runs the image as the servers will");
        let (svc, front) = (&c.common()[1].1, &c.common()[2].1);
        assert_eq!((&svc["spec"]["clusterIP"], &svc["spec"]["publishNotReadyAddresses"]), (&json!("None"), &json!(true)));
        assert_eq!(front["spec"]["selector"]["app.kubernetes.io/component"], "coordinator");
        // Their account may read and change this cluster, and only it.
        let spec = &d["spec"]["template"]["spec"];
        assert_eq!(spec["serviceAccountName"], "orders-coordinator");
        assert!(spec["containers"][0]["env"].as_array().unwrap().contains(&json!({"name": "STRATA_KUBE_CLUSTER", "value": "db/orders"})));
        let objects = c.common();
        let role = &objects.iter().find(|(p, _)| p.ends_with("/roles/orders-coordinator")).unwrap().1;
        assert_eq!(
            role["rules"],
            json!([
                {"apiGroups": [GROUP], "resources": [PLURAL], "resourceNames": ["orders"], "verbs": ["get", "patch"]},
                // The Lease of its map (finding E-2), and only it.
                {"apiGroups": ["coordination.k8s.io"], "resources": ["leases"], "resourceNames": ["orders-map"], "verbs": ["get", "update"]},
                {"apiGroups": ["coordination.k8s.io"], "resources": ["leases"], "verbs": ["create"]},
            ])
        );
        let binding = &objects.iter().find(|(p, _)| p.ends_with("/rolebindings/orders-coordinator")).unwrap().1;
        assert_eq!(binding["subjects"][0], json!({"kind": "ServiceAccount", "name": "orders-coordinator", "namespace": "db"}));
    }

    /// LIV-2: the coordinators kept their audit log, events, alerts,
    /// statement statistics and table growth in memory only, and every
    /// rollout forgot them. With `coordinatorStorage` each keeps them on a
    /// volume of its own; without it they run as before, to the hash.
    #[test]
    fn coordinators_with_storage_keep_their_history_on_a_volume_each() {
        let plain = cluster(json!({"demo": 10}));
        let d = plain.coordinator_workload(&[vec!["a".into()]], 2);
        assert_eq!(d["kind"], "Deployment");
        assert!(d["spec"]["template"]["spec"]["containers"][0]["args"].as_array().unwrap().iter().all(|a| a != "--log-store" && a != "--audit-log"), "{d}");
        assert_eq!(plain.coordinator_path(), "/apis/apps/v1/namespaces/db/deployments/orders-coordinator");
        let c = cluster(json!({"demo": 10, "coordinators": 2, "coordinatorStorage": {"size": "3Gi", "storageClassName": "fast"}, "coordinatorArgs": ["--alerts", "/data/mine.json"]}));
        c.spec.check().unwrap();
        assert_ne!(c.spec.coordinator_config(), plain.spec.coordinator_config(), "giving it restarts the coordinators");
        let s = c.coordinator_workload(&[vec!["a".into()]], 2);
        assert_eq!((s["kind"].as_str(), s["metadata"]["name"].as_str()), (Some("StatefulSet"), Some("orders-coordinator")));
        let args: Vec<&str> = s["spec"]["template"]["spec"]["containers"][0]["args"].as_array().unwrap().iter().filter_map(Json::as_str).collect();
        let after = |flag: &str| args.iter().position(|a| *a == flag).map(|k| args[k + 1]);
        assert_eq!((after("--log-store"), after("--audit-log"), after("--alerts")), (Some("/data/logs"), Some("/data/audit.jsonl"), Some("/data/mine.json")), "{args:?}");
        assert_eq!(args.iter().filter(|a| **a == "--alerts").count(), 1, "a flag coordinatorArgs gives is not given twice");
        let pod = &s["spec"]["template"]["spec"];
        assert!(pod["containers"][0]["volumeMounts"].as_array().unwrap().contains(&json!({"name": "data", "mountPath": "/data"})));
        assert_eq!(pod["securityContext"]["fsGroup"], 10001);
        assert_eq!(s["spec"]["volumeClaimTemplates"][0]["spec"], json!({"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "3Gi"}}, "storageClassName": "fast"}));
        assert_eq!((s["spec"]["serviceName"].as_str(), s["spec"]["replicas"].as_u64()), (Some("orders"), Some(2)));
        assert_eq!(s["spec"]["persistentVolumeClaimRetentionPolicy"], json!({"whenDeleted": "Retain", "whenScaled": "Retain"}));
        assert_eq!(s["spec"]["template"]["metadata"]["labels"]["app.kubernetes.io/component"], "coordinator", "the cluster's Service selects them");
        assert_eq!((c.coordinator_path(), c.old_coordinator_path()), ("/apis/apps/v1/namespaces/db/statefulsets/orders-coordinator".to_string(), "/apis/apps/v1/namespaces/db/deployments/orders-coordinator".to_string()));
        let bad: Spec = serde_json::from_value(json!({"demo": 1, "coordinatorStorage": {"size": "lots"}})).unwrap();
        assert!(bad.check().unwrap_err().contains("coordinatorStorage.size"));
        let dflt: Spec = serde_json::from_value(json!({"demo": 1, "coordinatorStorage": {}})).unwrap();
        assert_eq!(dflt.coordinator_storage.unwrap().size, "2Gi");
        // A StatefulSet has rolled when every pod runs its revision.
        let st = |cur: &str| json!({"kind": "StatefulSet", "metadata": {"generation": 3}, "spec": {"replicas": 2}, "status": {"observedGeneration": 3, "replicas": 2, "readyReplicas": 2, "updatedReplicas": 2, "currentRevision": cur, "updateRevision": "b"}});
        assert!(!rolled_of(&st("a")).done);
        assert!(rolled_of(&st("b")).done);
    }

    /// LIV-6: a rollout's extra copy weighed as much as a shard's own, so
    /// a copy restarting beside it landed on its peer's node as often as
    /// not. The shard's own copies weigh twice.
    #[test]
    fn a_shards_own_copies_keep_apart_more_than_its_extra_copy() {
        let c = cluster(json!({"demo": 10, "replicas": 2}));
        let set = c.statefulset(0, 3, None);
        let terms = set["spec"]["template"]["spec"]["affinity"]["podAntiAffinity"]["preferredDuringSchedulingIgnoredDuringExecution"].as_array().unwrap().clone();
        assert_eq!(terms.len(), 2, "{terms:?}");
        assert!(set["spec"]["template"]["spec"]["affinity"]["podAntiAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"].is_null(), "a cluster of one node still runs every copy");
        let own = &terms[0];
        assert_eq!(own["weight"], 100);
        assert_eq!(own["podAffinityTerm"]["labelSelector"]["matchExpressions"], json!([{"key": "apps.kubernetes.io/pod-index", "operator": "In", "values": ["0", "1"]}]), "copies 0 and 1, not the extra copy 2");
        assert_eq!(own["podAffinityTerm"]["labelSelector"]["matchLabels"]["strata.chloetech.ai/set"], "0");
        assert_eq!((terms[1]["weight"].as_u64(), terms[1]["podAffinityTerm"]["labelSelector"]["matchExpressions"].is_null()), (Some(50), true));
    }

    #[test]
    fn a_link_names_one_cluster() {
        let o = Options { api: Some("http://127.0.0.1:1".into()), ..Default::default() };
        let l = Link::with("db/orders", &o).unwrap();
        assert_eq!(l.path(), "/apis/strata.chloetech.ai/v1alpha1/namespaces/db/strataclusters/orders");
        for bad in ["orders", "/orders", "db/", "a/b/c"] {
            assert!(Link::with(bad, &o).is_err(), "{bad}");
        }
        assert_eq!(Link::sizes(Some(3), None, Some(2)).unwrap(), json!({"shards": 3, "coordinators": 2}));
        assert!(Link::sizes(None, Some(0), None).unwrap_err().contains("replicas is 1 to 64"));
        assert!(Link::sizes(Some(5000), None, None).is_err());
        assert!(Link::sizes(None, None, None).is_err());
    }

    #[test]
    fn with_a_cluster_token_the_admin_token_stays_with_the_coordinators() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "clusterTokenSecret": "inner"}));
        let names = |env: &Json| -> Vec<(String, String)> {
            let e = env.as_array().unwrap().iter().filter(|e| e["valueFrom"]["secretKeyRef"].is_object());
            e.map(|e| (e["name"].as_str().unwrap().to_string(), e["valueFrom"]["secretKeyRef"]["name"].as_str().unwrap().to_string())).collect()
        };
        let s = c.statefulset(0, 3, Some((0, 1)));
        assert_eq!(names(&s["spec"]["template"]["spec"]["containers"][0]["env"]), [("STRATA_CLUSTER_TOKEN".to_string(), "inner".to_string())]);
        let d = c.coordinator_workload(&[vec!["a".into()]], 1);
        let coordinator = &d["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(names(&coordinator["env"]), [("STRATA_TOKEN".to_string(), "admin".to_string()), ("STRATA_CLUSTER_TOKEN".to_string(), "inner".to_string())]);
        assert!(!coordinator["args"].as_array().unwrap().contains(&json!("--no-token")));
        // Without one, the token is everyone's, as before.
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin"}));
        let d = c.coordinator_workload(&[vec!["a".into()]], 1);
        assert_eq!(names(&d["spec"]["template"]["spec"]["containers"][0]["env"]), [("STRATA_TOKEN".to_string(), "admin".to_string())]);
    }

    #[test]
    fn secret_env_reaches_every_server_and_coordinator() {
        let c = cluster(json!({"demo": 10, "tokenSecret": "admin", "secretEnv": [{"name": "ANTHROPIC_API_KEY", "secret": "anthropic"}, {"name": "OPENAI_API_KEY", "secret": "models", "key": "openai"}]}));
        let from = |env: &Json, name: &str| env.as_array().unwrap().iter().find(|e| e["name"] == name).map(|e| e["valueFrom"]["secretKeyRef"].clone());
        let s = c.statefulset(0, 2, Some((0, 1)));
        let d = c.coordinator_workload(&[vec!["a".into()]], 1);
        for env in [&s["spec"]["template"]["spec"]["containers"][0]["env"], &d["spec"]["template"]["spec"]["containers"][0]["env"]] {
            assert_eq!(from(env, "ANTHROPIC_API_KEY"), Some(json!({"name": "anthropic", "key": "key"})));
            assert_eq!(from(env, "OPENAI_API_KEY"), Some(json!({"name": "models", "key": "openai"})));
        }
        // Not the follow process's: it calls no model.
        let f: super::super::FollowSpec = serde_json::from_value(json!({"source": "https://east:7700"})).unwrap();
        assert!(from(&c.follow_deployment(&f)["spec"]["template"]["spec"]["containers"][0]["env"], "ANTHROPIC_API_KEY").is_none());
        // Without it, the servers' and coordinators' variables are as before.
        let plain = cluster(json!({"demo": 10, "tokenSecret": "admin"}));
        assert!(from(&plain.statefulset(0, 2, Some((0, 1)))["spec"]["template"]["spec"]["containers"][0]["env"], "ANTHROPIC_API_KEY").is_none());
    }

    #[test]
    fn a_status_is_the_same_as_the_api_server_gives_it_back() {
        assert!(same(&json!({"a": 200, "b": [1, {"c": 2}]}), &json!({"a": 200.0, "b": [1.0, {"c": 2, "d": null}], "e": null})));
        assert!(!same(&json!({"a": 200}), &json!({"a": 201})));
        assert!(!same(&json!({"a": [1]}), &json!({"a": [1, 2]})));
        assert!(!same(&json!({}), &json!({"a": 0})));
    }

    #[test]
    fn base64_as_secrets_hold_it() {
        assert_eq!(base64_decode("c2VjcmV0LXRva2Vu").unwrap(), b"secret-token");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(base64_decode("!!"), None);
    }

    #[test]
    fn a_pod_being_deleted_is_not_ready() {
        let ready = json!({"metadata": {}, "status": {"conditions": [{"type": "Ready", "status": "True"}]}});
        let mut draining = ready.clone();
        draining["metadata"]["deletionTimestamp"] = json!("2026-11-02T08:30:00Z");
        let starting = json!({"metadata": {}, "status": {"conditions": [{"type": "Ready", "status": "False"}]}});
        assert!(pod_ready(&ready) && !pod_ready(&draining) && !pod_ready(&starting));
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

    /// Kubernetes refuses a Role that grants what its maker does not hold
    /// (RBAC's escalation check): every rule of the coordinators' Role is
    /// one the operator's ClusterRole (operator.yaml, and the cloud view's
    /// copy of it) holds, or no cluster gets past its Role.
    #[test]
    fn the_operator_holds_what_it_grants_the_coordinators() {
        let c = cluster(json!({"demo": 10}));
        let operator = crate::cloud::install::operator_objects("chloedb-server:dev").into_iter().find(|o| o["kind"] == "ClusterRole").unwrap();
        let strs = |j: &Json| j.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect::<Vec<_>>();
        for lease in [true, false] {
            for rule in c.coordinator_role(lease)["rules"].as_array().unwrap() {
                for (g, r, v) in strs(&rule["apiGroups"]).iter().flat_map(|g| strs(&rule["resources"]).into_iter().map(move |r| (g.clone(), r))).flat_map(|(g, r)| strs(&rule["verbs"]).into_iter().map(move |v| (g.clone(), r.clone(), v))) {
                    let held = operator["rules"].as_array().unwrap().iter().any(|o| {
                        let all = |k: &str, x: &str| strs(&o[k]).iter().any(|y| y == x || y == "*");
                        all("apiGroups", &g) && all("resources", &r) && all("verbs", &v) && o["resourceNames"].is_null()
                    });
                    assert!(held, "the coordinators' Role grants {v} {g}/{r}, which the operator's ClusterRole does not hold");
                }
            }
        }
        let leases = |role: &Json| role["rules"].as_array().unwrap().iter().any(|r| r["resources"] == json!(["leases"]));
        assert!(leases(&c.coordinator_role(true)) && !leases(&c.coordinator_role(false)));
    }

    /// An operator whose ClusterRole is from before the map's Lease (the
    /// live clusters' until operator.yaml is applied again) is refused the
    /// coordinators' Role with the Lease: it makes the Role without it and
    /// keeps the cluster, rather than failing every reconcile there.
    #[tokio::test]
    async fn a_role_refused_for_its_lease_is_made_without_it() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<(String, Json)>>> = Arc::default();
        let s = seen.clone();
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
            let s = s.clone();
            async move {
                let j: Json = serde_json::from_slice(&body).unwrap_or(Json::Null);
                s.lock().unwrap().push((uri.path().to_string(), j.clone()));
                if j["kind"] == "Role" && j.to_string().contains("coordination.k8s.io") {
                    let m = json!({"message": "roles.rbac.authorization.k8s.io \"orders-coordinator\" is forbidden: user is attempting to grant RBAC permissions not currently held"});
                    return (axum::http::StatusCode::FORBIDDEN, axum::Json(m));
                }
                (axum::http::StatusCode::OK, axum::Json(j))
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        let mut c = cluster(json!({"demo": 10}));
        c.kube = Kube::new(&Options { api: Some(url), token: Some("t".into()), ..Default::default() }).unwrap();
        common(&c).await.unwrap();
        let seen = seen.lock().unwrap();
        let roles: Vec<&Json> = seen.iter().filter(|(p, _)| p.ends_with("/roles/orders-coordinator")).map(|(_, j)| j).collect();
        assert_eq!(roles.len(), 2, "{seen:?}");
        assert_eq!(*roles[1], c.coordinator_role(false));
        assert!(seen.iter().any(|(p, _)| p.ends_with("/rolebindings/orders-coordinator")), "the binding after it is made too");
    }

    /// An operator whose ClusterRole may not make Secrets (one from before
    /// apps' credentials: an installed operator upgrades neither itself
    /// nor it) makes no key for its apps, which keep the admin token, and
    /// the reconcile goes on.
    #[tokio::test]
    async fn an_operator_that_may_not_make_secrets_makes_no_keys() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let s = seen.clone();
        let app = axum::Router::new().fallback(move |method: axum::http::Method, uri: axum::http::Uri| {
            let s = s.clone();
            async move {
                let path = uri.path().to_string();
                s.lock().unwrap().push((method.to_string(), path.clone()));
                match (method.as_str(), path.as_str()) {
                    ("GET", "/v1/whoami") => (axum::http::StatusCode::OK, axum::Json(json!({"auth": {"app_keys": true}}))),
                    ("GET", "/v1/admin/keys") => (axum::http::StatusCode::OK, axum::Json(json!({"keys": []}))),
                    ("POST", "/v1/admin/keys") => (axum::http::StatusCode::OK, axum::Json(json!({"key": {"id": "k1", "tables": {"sheets_*": "admin"}}, "secret": "sk_k1_x"}))),
                    ("GET", p) if p.contains("/secrets/") => (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"message": "not found"}))),
                    (_, p) if p.contains("/secrets/") => (axum::http::StatusCode::FORBIDDEN, axum::Json(json!({"message": "secrets is forbidden: User \"system:serviceaccount:strata-system:strata-operator\" cannot patch resource \"secrets\""}))),
                    _ => (axum::http::StatusCode::OK, axum::Json(json!({}))),
                }
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        let mut c = cluster(json!({"tokenSecret": "admin", "sheets": {}, "chloectx": {}}));
        c.kube = Kube::new(&Options { api: Some(url.clone()), token: Some("t".into()), ..Default::default() }).unwrap();
        let admin = Admin::new(&url, Some("admin".into()));
        let spec = c.spec.clone();
        c.app_credentials(&spec, &admin).await.unwrap();
        // Its apps keep the admin token, for their key and their token.
        let creds = c.creds(&spec, super::super::sheets::APP).await.unwrap();
        assert!(!creds.key && !creds.token);
        let seen = seen.lock().unwrap();
        assert!(!seen.iter().any(|(m, p)| m == "POST" && p == "/v1/admin/keys"), "no key is made: {seen:?}");
        assert!(seen.iter().any(|(m, p)| m == "PATCH" && p.contains("/secrets/")), "{seen:?}");
    }

    /// A fake Kubernetes API that keeps the Secrets it is sent, and
    /// coordinators whose key creation answers `keys` (status, body).
    async fn keeping_secrets(keys: (u16, Json), have: Option<Json>) -> (String, std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Json>>>, std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        use std::sync::{Arc, Mutex};
        let secrets: Arc<Mutex<std::collections::BTreeMap<String, Json>>> = Arc::default();
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        if let Some(h) = have {
            secrets.lock().unwrap().insert("/api/v1/namespaces/db/secrets/orders-sheets-credentials".into(), h);
        }
        let (k, s2) = (secrets.clone(), seen.clone());
        let app = axum::Router::new().fallback(move |method: axum::http::Method, uri: axum::http::Uri, body: axum::body::Bytes| {
            let (k, s2, keys) = (k.clone(), s2.clone(), keys.clone());
            async move {
                let path = uri.path().to_string();
                s2.lock().unwrap().push((method.to_string(), path.clone()));
                let ok = |j: Json| (axum::http::StatusCode::OK, axum::Json(j));
                match (method.as_str(), path.as_str()) {
                    ("GET", "/v1/whoami") => ok(json!({"auth": {"app_keys": true}})),
                    ("GET", "/v1/admin/keys") => ok(json!({"keys": [{"id": "k-old", "name": "sheets-app", "revoked_ms": null}]})),
                    ("POST", "/v1/admin/keys") => (axum::http::StatusCode::from_u16(keys.0).unwrap(), axum::Json(keys.1)),
                    ("GET", p) if p.contains("/secrets/") => match k.lock().unwrap().get(p) {
                        Some(s) => ok(s.clone()),
                        None => (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"message": "not found"}))),
                    },
                    (_, p) if p.contains("/secrets/") => {
                        let j: Json = serde_json::from_slice(&body).unwrap_or(Json::Null);
                        k.lock().unwrap().insert(p.to_string(), j.clone());
                        ok(j)
                    }
                    _ => ok(json!({})),
                }
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        (url, secrets, seen)
    }

    /// Audit X-E2: when making an app's new key fails (its tables changed
    /// while the coordinators roll, a 5xx, a timeout), the Secret keeps
    /// its old key, key id and tables' hash, so the pod neither rolls nor
    /// falls back to the admin token; the next reconcile tries again.
    #[tokio::test]
    async fn a_failed_key_keeps_the_old_one() {
        let old = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "orders-sheets-credentials", "annotations": {super::super::credentials::KEY_ID: "k-old", super::super::credentials::TABLES: "an-older-hash"}},
            "data": {"key": base64_encode(b"sk_old"), "token": base64_encode(b"t1")}});
        let (url, secrets, seen) = keeping_secrets((503, json!({"error": "the coordinators are rolling"})), Some(old.clone())).await;
        let mut c = cluster(json!({"tokenSecret": "admin", "sheets": {}}));
        c.kube = Kube::new(&Options { api: Some(url.clone()), token: Some("t".into()), ..Default::default() }).unwrap();
        let admin = Admin::new(&url, Some("admin".into()));
        let spec = c.spec.clone();
        let e = c.app_credentials(&spec, &admin).await.unwrap_err();
        assert!(e.contains("keeps the key it has"), "{e}");
        let seen = seen.lock().unwrap().clone();
        assert!(seen.iter().any(|(m, p)| m == "POST" && p == "/v1/admin/keys"), "a key was asked for: {seen:?}");
        assert!(!seen.iter().any(|(m, p)| m != "GET" && p.contains("/secrets/")), "the Secret is not written: {seen:?}");
        assert!(!seen.iter().any(|(m, _)| m == "DELETE"), "nothing is revoked: {seen:?}");
        assert_eq!(secrets.lock().unwrap().values().next().unwrap(), &old);
        // The app keeps its own key, and its pod's template is unchanged.
        let creds = c.creds(&spec, super::super::sheets::APP).await.unwrap();
        assert_eq!((creds.key, creds.key_id.as_deref(), creds.pending), (true, Some("k-old"), false));
        let env = c.app_env(super::super::sheets::APP, creds, "STRATA_KEY", "STRATA_SHEETS_TOKEN");
        assert!(!env.iter().any(|v| v["valueFrom"]["secretKeyRef"]["name"] == "admin"), "{env:?}");
    }

    /// Audit X-E2: an app with no key yet whose key could not be made is
    /// given none, rather than the admin token; coordinators from before
    /// keys limited to tables still give it the admin token.
    #[tokio::test]
    async fn an_app_without_a_key_gets_none_rather_than_the_admin_token() {
        let (url, kept, _) = keeping_secrets((503, json!({"error": "the coordinators are rolling"})), None).await;
        let mut c = cluster(json!({"tokenSecret": "admin", "sheets": {}}));
        c.kube = Kube::new(&Options { api: Some(url.clone()), token: Some("t".into()), ..Default::default() }).unwrap();
        let admin = Admin::new(&url, Some("admin".into()));
        let spec = c.spec.clone();
        assert!(c.app_credentials(&spec, &admin).await.is_err());
        let creds = c.creds(&spec, super::super::sheets::APP).await.unwrap();
        assert_eq!((creds.key, creds.token, creds.pending), (false, true, true));
        let env = c.app_env(super::super::sheets::APP, creds, "STRATA_KEY", "STRATA_SHEETS_TOKEN");
        assert!(!env.iter().any(|v| v["name"] == "STRATA_KEY"), "no key, not the admin token: {env:?}");
        assert!(!env.iter().any(|v| v["valueFrom"]["secretKeyRef"]["name"] == "admin"), "{env:?}");

        // Coordinators from before limited keys: the admin token, as before.
        let pending = kept.lock().unwrap().values().next().cloned();
        assert_eq!(pending.as_ref().unwrap()["metadata"]["annotations"][super::super::credentials::PENDING], "true");
        let every = json!({"key": {"id": "k1", "tables": {}}, "secret": "sk_k1_x"});
        let (url, _, _) = keeping_secrets((200, every), pending.clone()).await;
        c.kube = Kube::new(&Options { api: Some(url.clone()), token: Some("t".into()), ..Default::default() }).unwrap();
        let admin = Admin::new(&url, Some("admin".into()));
        let e = c.app_credentials(&spec, &admin).await.unwrap_err();
        assert!(e.contains("from before keys limited"), "{e}");
        let creds = c.creds(&spec, super::super::sheets::APP).await.unwrap();
        assert_eq!((creds.key, creds.pending), (false, false));
        let env = c.app_env(super::super::sheets::APP, creds, "STRATA_KEY", "STRATA_SHEETS_TOKEN");
        assert!(env.iter().any(|v| v["name"] == "STRATA_KEY" && v["valueFrom"]["secretKeyRef"]["name"] == "admin"), "{env:?}");

        // Once a key is made, the pending mark goes.
        let made = json!({"key": {"id": "k2", "tables": {"sheets_*": "admin"}}, "secret": "sk_k2_x"});
        let (url, _, _) = keeping_secrets((200, made), pending).await;
        c.kube = Kube::new(&Options { api: Some(url.clone()), token: Some("t".into()), ..Default::default() }).unwrap();
        let admin = Admin::new(&url, Some("admin".into()));
        c.app_credentials(&spec, &admin).await.unwrap();
        let creds = c.creds(&spec, super::super::sheets::APP).await.unwrap();
        assert_eq!((creds.key, creds.key_id.as_deref(), creds.pending), (true, Some("k2"), false));
    }
}
