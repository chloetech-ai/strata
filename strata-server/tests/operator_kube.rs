//! The operator's Kubernetes side against a real API server (etcd and
//! kube-apiserver, as controller-runtime's envtest runs them: no nodes, so
//! no pods run, and the test stands in for the kubelet by marking pods
//! ready). It installs the resource definition, makes a cluster's objects,
//! which the API server validates, writes the cluster's status, starts
//! the coordinator once the shards' first copies are ready, scales and
//! deletes a set, and reads and scales the cluster as a coordinator does.
//!
//! Skipped unless `STRATA_ENVTEST_DIR` names a directory holding `etcd`
//! and `kube-apiserver` (for example from `setup-envtest use`, or etcd
//! from the registry.k8s.io/etcd image and kube-apiserver from
//! dl.k8s.io/release/<version>/bin/<os>/<arch>/kube-apiserver); needs
//! `openssl`.

#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata_server::operator::kube::{self, Cluster, Kube};
use strata_server::operator::{self, Phase, Platform};

const TOKEN: &str = "envtest-admin-token";

struct Envtest {
    procs: Vec<Child>,
    url: String,
    ca: Vec<u8>,
    _dir: tempfile::TempDir,
}

impl Drop for Envtest {
    fn drop(&mut self) {
        for p in self.procs.iter_mut().rev() {
            let _ = p.kill();
            let _ = p.wait();
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

impl Envtest {
    async fn start(bin: &Path) -> Envtest {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let log = |name: &str| std::fs::File::create(d.join(name)).unwrap();
        let (client, peer, secure) = (free_port(), free_port(), free_port());
        let etcd = Command::new(bin.join("etcd"))
            .arg("--data-dir")
            .arg(d.join("etcd"))
            .args(["--listen-client-urls", &format!("http://127.0.0.1:{client}"), "--advertise-client-urls", &format!("http://127.0.0.1:{client}")])
            .args(["--listen-peer-urls", &format!("http://127.0.0.1:{peer}"), "--initial-advertise-peer-urls", &format!("http://127.0.0.1:{peer}")])
            .args(["--initial-cluster", &format!("default=http://127.0.0.1:{peer}")])
            .stdout(Stdio::null())
            .stderr(log("etcd.log"))
            .spawn()
            .unwrap();
        let key = d.join("sa.key");
        assert!(Command::new("openssl").args(["genrsa", "-out"]).arg(&key).arg("2048").stderr(Stdio::null()).status().unwrap().success());
        std::fs::write(d.join("tokens.csv"), format!("{TOKEN},admin,admin,system:masters\n")).unwrap();
        let certs = d.join("certs");
        let api = Command::new(bin.join("kube-apiserver"))
            .args(["--etcd-servers", &format!("http://127.0.0.1:{client}"), "--bind-address", "127.0.0.1", "--advertise-address", "127.0.0.1"])
            .args(["--secure-port", &secure.to_string(), "--authorization-mode", "AlwaysAllow", "--service-cluster-ip-range", "10.0.0.0/24"])
            .arg("--cert-dir")
            .arg(&certs)
            .arg("--token-auth-file")
            .arg(d.join("tokens.csv"))
            .args(["--service-account-issuer", "https://kubernetes.default.svc"])
            .arg("--service-account-key-file")
            .arg(&key)
            .arg("--service-account-signing-key-file")
            .arg(&key)
            .args(["--disable-admission-plugins", "ServiceAccount"])
            .stdout(Stdio::null())
            .stderr(log("kube-apiserver.log"))
            .spawn()
            .unwrap();
        let url = format!("https://127.0.0.1:{secure}");
        let t0 = Instant::now();
        let mut ca = Vec::new();
        loop {
            assert!(t0.elapsed() < Duration::from_secs(60), "kube-apiserver did not start: {}", std::fs::read_to_string(d.join("kube-apiserver.log")).unwrap_or_default());
            if ca.is_empty() {
                ca = std::fs::read(certs.join("apiserver.crt")).unwrap_or_default();
            }
            if !ca.is_empty() {
                let k = Kube::new(&kube::Options { api: Some(url.clone()), token: Some(TOKEN.into()), ca: Some(ca.clone()), ..Default::default() }).unwrap();
                if k.get("/readyz").await.is_ok_and(|r| r.is_some()) || k.get("/api/v1/namespaces/default").await.is_ok_and(|r| r.is_some()) {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Envtest { procs: vec![etcd, api], url, ca, _dir: dir }
    }

    fn kube(&self) -> Kube {
        Kube::new(&kube::Options { api: Some(self.url.clone()), token: Some(TOKEN.into()), ca: Some(self.ca.clone()), ..Default::default() }).unwrap()
    }
}

const CLUSTERS: &str = "/apis/strata.chloetech.ai/v1alpha1/namespaces/db/strataclusters";

async fn get(k: &Kube, path: &str) -> Json {
    k.get(path).await.unwrap().unwrap_or(Json::Null)
}

/// A pod of set `set` the kubelet would have started from its template,
/// ready.
async fn ready_pod(k: &Kube, set: u32, ordinal: usize) {
    let name = format!("orders-s{set}-{ordinal}");
    let labels = json!({"app.kubernetes.io/instance": "orders", "app.kubernetes.io/component": "shard", "strata.chloetech.ai/set": set.to_string()});
    let template = get(k, &format!("/apis/apps/v1/namespaces/db/statefulsets/orders-s{set}")).await["spec"]["template"].clone();
    let annotations = template["metadata"]["annotations"].clone();
    let pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "namespace": "db", "labels": labels, "annotations": annotations}, "spec": {"containers": [{"name": "strata", "image": "strata-server:dev"}]}});
    k.apply(&format!("/api/v1/namespaces/db/pods/{name}"), &pod).await.unwrap();
    let status = json!({"status": {"conditions": [{"type": "Ready", "status": "True"}]}});
    k.merge(&format!("/api/v1/namespaces/db/pods/{name}/status"), &status).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_makes_a_clusters_objects_on_an_api_server() {
    let Some(bin) = std::env::var_os("STRATA_ENVTEST_DIR") else {
        eprintln!("skipped: set STRATA_ENVTEST_DIR to a directory holding etcd and kube-apiserver");
        return;
    };
    let env = Envtest::start(Path::new(&bin)).await;
    let k = env.kube();

    // The resource definition, as the operator installs it.
    let crd = operator::crd();
    k.apply(&format!("/apis/apiextensions.k8s.io/v1/customresourcedefinitions/{}", crd["metadata"]["name"].as_str().unwrap()), &crd).await.unwrap();
    k.apply("/api/v1/namespaces/db", &json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "db"}})).await.unwrap();
    let t0 = Instant::now();
    while !k.get(CLUSTERS).await.is_ok_and(|r| r.is_some()) {
        assert!(t0.elapsed() < Duration::from_secs(30), "the StrataCluster resource was not served");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let cr = json!({"apiVersion": "strata.chloetech.ai/v1alpha1", "kind": "StrataCluster", "metadata": {"name": "orders", "namespace": "db"},
                    "spec": {"shards": 2, "replicas": 3, "demo": 300, "storage": {"size": "1Gi"}}});
    k.apply(&format!("{CLUSTERS}/orders"), &cr).await.unwrap();
    // A field the definition does not have is refused.
    let e = k.apply(&format!("{CLUSTERS}/bad"), &json!({"apiVersion": "strata.chloetech.ai/v1alpha1", "kind": "StrataCluster", "metadata": {"name": "bad", "namespace": "db"}, "spec": {"shards": 0}})).await.unwrap_err();
    assert!(e.contains("shards"), "{e}");
    let now = || async { get(&k, &format!("{CLUSTERS}/orders")).await };

    // First the shards' sets, which the API server takes as they are.
    let out = kube::once(&k, &now().await).await;
    assert_eq!((out.phase, out.message.as_str()), (Phase::Creating, "creating 2 shards of 3 copies"));
    let uid = now().await["metadata"]["uid"].clone();
    for set in 0..2 {
        let s = get(&k, &format!("/apis/apps/v1/namespaces/db/statefulsets/orders-s{set}")).await;
        assert_eq!((&s["spec"]["replicas"], &s["metadata"]["ownerReferences"][0]["uid"]), (&json!(3), &uid), "{s}");
        assert_eq!(s["spec"]["persistentVolumeClaimRetentionPolicy"]["whenScaled"], "Delete");
    }
    let svc = get(&k, "/api/v1/namespaces/db/services/orders-servers").await;
    assert_eq!((&svc["spec"]["clusterIP"], &svc["spec"]["publishNotReadyAddresses"]), (&json!("None"), &json!(true)));
    assert_eq!(get(&k, "/api/v1/namespaces/db/services/orders").await["spec"]["ports"][0]["port"], 7700);
    // The coordinators' account, which may read and scale this cluster.
    let role = get(&k, "/apis/rbac.authorization.k8s.io/v1/namespaces/db/roles/orders-coordinator").await;
    assert_eq!(role["rules"][0]["resourceNames"], json!(["orders"]), "{role}");
    let binding = get(&k, "/apis/rbac.authorization.k8s.io/v1/namespaces/db/rolebindings/orders-coordinator").await;
    assert_eq!(binding["subjects"][0]["name"], "orders-coordinator");
    assert_eq!(get(&k, "/api/v1/namespaces/db/serviceaccounts/orders-coordinator").await["metadata"]["name"], "orders-coordinator");
    let schema = get(&k, "/api/v1/namespaces/db/configmaps/orders-schema").await;
    strata::schema::Schema::from_json(schema["data"]["schema.json"].as_str().unwrap()).unwrap();
    assert_eq!(now().await["status"]["phase"], "Creating");

    // Then, once the first copy of each is ready, the coordinator over them.
    let out = kube::once(&k, &now().await).await;
    assert_eq!(out.message, "waiting for the first copy of 2 of 2 shards to start");
    assert_eq!(now().await["status"]["message"], "waiting for the first copy of 2 of 2 shards to start");
    ready_pod(&k, 0, 0).await;
    let out = kube::once(&k, &now().await).await;
    assert_eq!(out.message, "waiting for the first copy of 1 of 2 shards to start");
    ready_pod(&k, 1, 0).await;
    ready_pod(&k, 1, 1).await;
    let out = kube::once(&k, &now().await).await;
    assert_eq!(out.message, "starting the coordinator, which lays the cluster out");
    let d = get(&k, "/apis/apps/v1/namespaces/db/deployments/orders-coordinator").await;
    let args = &d["spec"]["template"]["spec"]["containers"][0]["args"];
    assert_eq!(args[1], "http://orders-s0-0.orders-servers.db.svc:7701,http://orders-s1-0.orders-servers.db.svc:7701", "{d}");
    assert_eq!(d["spec"]["template"]["spec"]["serviceAccountName"], "orders-coordinator");
    // Its Service's name does not resolve here: the operator waits.
    let out = kube::once(&k, &now().await).await;
    assert!(out.phase == Phase::Creating && out.message.starts_with("waiting for the coordinator"), "{out:?}");

    // What the platform finds and changes.
    let spec = serde_json::from_value(now().await["spec"].clone()).unwrap();
    let c = Cluster { kube: k.clone(), ns: "db".into(), name: "orders".into(), uid: uid.as_str().unwrap().into(), spec };
    let mut sets = c.sets().await.unwrap();
    sets.sort_by_key(|s| s.id);
    assert_eq!(sets.iter().map(|s| (s.id, s.replicas, s.ready.clone())).collect::<Vec<_>>(), vec![(0, 3, vec![0]), (1, 3, vec![0, 1])]);
    assert_eq!(c.coordinator().await.unwrap().unwrap().0.len(), 2);
    assert_eq!(c.coordinator_config().await.unwrap(), Some(c.spec.coordinator_config()));

    // New settings: a set's template takes them at once, and each copy when
    // the operator restarts it.
    assert_eq!(c.stale(&c.spec).await.unwrap(), vec![]);
    let mut newer = c.spec.clone();
    newer.args = vec!["--slow-ms".into(), "50".into()];
    assert!(c.configure(0, &newer).await.unwrap());
    assert!(!c.configure(0, &newer).await.unwrap(), "configured once");
    let s0 = get(&k, "/apis/apps/v1/namespaces/db/statefulsets/orders-s0").await;
    assert_eq!(s0["spec"]["updateStrategy"]["type"], "OnDelete");
    assert_eq!(s0["spec"]["template"]["metadata"]["annotations"]["strata.chloetech.ai/config"], json!(newer.server_config()));
    let env0 = s0["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap().iter().find(|e| e["name"] == "STRATA_ARGS").cloned().unwrap();
    assert_eq!(env0["value"], "'--slow-ms' '50'");
    assert_eq!(s0["spec"]["volumeClaimTemplates"][0]["spec"]["resources"]["requests"]["storage"], "1Gi", "the claims are as they were");
    assert_eq!(c.stale(&newer).await.unwrap(), vec![(0, 0), (1, 0), (1, 1)]);
    c.restart(0, 0).await.unwrap();
    let p = k.get("/api/v1/namespaces/db/pods/orders-s0-0").await.unwrap();
    assert!(p.is_none() || !p.unwrap()["metadata"]["deletionTimestamp"].is_null(), "the pod is deleted, for its set to start it again");
    assert_eq!(c.stale(&newer).await.unwrap(), vec![(1, 0), (1, 1)]);
    // Volumes grow in place, once, when their storage class lets them.
    let class = |name: &str, grows: bool| json!({"apiVersion": "storage.k8s.io/v1", "kind": "StorageClass", "metadata": {"name": name}, "provisioner": "example.com/disks", "allowVolumeExpansion": grows});
    k.apply("/apis/storage.k8s.io/v1/storageclasses/fixed", &class("fixed", false)).await.unwrap();
    k.apply("/apis/storage.k8s.io/v1/storageclasses/grows", &class("grows", true)).await.unwrap();
    let claim = |name: &str, class: &str| json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": {"name": name, "namespace": "db", "labels": {"app.kubernetes.io/instance": "orders", "app.kubernetes.io/component": "shard", "strata.chloetech.ai/set": "0"}},
        "spec": {"accessModes": ["ReadWriteOnce"], "storageClassName": class, "resources": {"requests": {"storage": "1Gi"}}, "volumeName": format!("pv-{name}")}});
    let bind = |name: &'static str, class: &'static str| {
        let k = k.clone();
        async move {
            k.apply(&format!("/api/v1/namespaces/db/persistentvolumeclaims/{name}"), &claim(name, class)).await.unwrap();
            k.merge(&format!("/api/v1/namespaces/db/persistentvolumeclaims/{name}/status"), &json!({"status": {"phase": "Bound", "capacity": {"storage": "1Gi"}}})).await.unwrap();
        }
    };
    bind("data-orders-s0-0", "fixed").await;
    let e = c.grow_volumes("2Gi", true).await.unwrap_err();
    assert!(e.contains("must support resize"), "{e}");
    k.delete("/api/v1/namespaces/db/persistentvolumeclaims/data-orders-s0-0").await.unwrap();
    // (A claim keeps its protection finalizer here: take it off.)
    let _ = k.merge("/api/v1/namespaces/db/persistentvolumeclaims/data-orders-s0-0", &json!({"metadata": {"finalizers": null}})).await;
    let t0 = Instant::now();
    while k.get("/api/v1/namespaces/db/persistentvolumeclaims/data-orders-s0-0").await.unwrap().is_some() {
        assert!(t0.elapsed() < Duration::from_secs(10), "the claim was not deleted");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bind("data-orders-s0-0", "grows").await;
    bind("data-orders-s0-1", "grows").await;
    assert_eq!(c.grow_volumes("2Gi", false).await.unwrap(), 2, "two claims are smaller");
    assert_eq!(c.grow_volumes("2Gi", true).await.unwrap(), 2);
    assert_eq!(get(&k, "/api/v1/namespaces/db/persistentvolumeclaims/data-orders-s0-1").await["spec"]["resources"]["requests"]["storage"], "2Gi");
    assert_eq!(c.grow_volumes("2Gi", true).await.unwrap(), 0);
    assert_eq!(c.grow_volumes("1Gi", true).await.unwrap(), 0, "a volume never shrinks");
    c.scale_set(1, 2).await.unwrap();
    assert_eq!(get(&k, "/apis/apps/v1/namespaces/db/statefulsets/orders-s1").await["spec"]["replicas"], 2);
    let pvc = json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim",
                     "metadata": {"name": "data-orders-s1-0", "namespace": "db", "labels": {"app.kubernetes.io/instance": "orders", "strata.chloetech.ai/set": "1"}},
                     "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}}});
    k.apply("/api/v1/namespaces/db/persistentvolumeclaims/data-orders-s1-0", &pvc).await.unwrap();
    c.delete_set(1).await.unwrap();
    let s = k.get("/apis/apps/v1/namespaces/db/statefulsets/orders-s1").await.unwrap();
    assert!(s.is_none() || !s.unwrap()["metadata"]["deletionTimestamp"].is_null(), "the set is deleted");
    // (No controller manager removes a claim's protection finalizer here.)
    let p = k.get("/api/v1/namespaces/db/persistentvolumeclaims/data-orders-s1-0").await.unwrap();
    assert!(p.is_none() || !p.unwrap()["metadata"]["deletionTimestamp"].is_null(), "the set's volume is deleted");

    // What the coordinator shows of its cluster, and the sizes it changes.
    let link = kube::Link::with("db/orders", &kube::Options { api: Some(env.url.clone()), token: Some(TOKEN.into()), ca: Some(env.ca.clone()), ..Default::default() }).unwrap();
    let j = link.get().await.unwrap();
    assert_eq!((&j["spec"]["shards"], &j["status"]["phase"], &j["settled"]), (&json!(2), &json!("Creating"), &json!(true)), "{j}");
    let first = j["status"]["history"].clone();
    assert_eq!((first.as_array().map(Vec::len), &first[0]["version"], &first[0]["by"]), (Some(1), &json!(1), &Json::Null), "{first}");
    assert!(!j["status"]["log"].as_array().unwrap().is_empty(), "{j}");
    // Who changes it through a coordinator is noted with the version
    // the change makes, and its version keeps it.
    let next: operator::Spec = serde_json::from_value(json!({"shards": 3, "replicas": 2, "demo": 300, "storage": {"size": "1Gi"}})).unwrap();
    let j = link.change(&kube::Link::sizes(Some(3), Some(2), None).unwrap(), &[(kube::CHANGED_BY, format!("roberto {}", next.version()))]).await.unwrap();
    assert_eq!((&j["spec"]["shards"], &j["spec"]["replicas"], &j["spec"]["demo"], &j["settled"]), (&json!(3), &json!(2), &json!(300), &json!(false)), "{j}");
    kube::once(&k, &now().await).await;
    let j = link.get().await.unwrap();
    assert_eq!(j["settled"], true);
    let h = &j["status"]["history"];
    assert_eq!((h.as_array().map(Vec::len), &h[1]["version"], &h[1]["by"], &h[1]["spec"]["shards"]), (Some(2), &json!(2), &json!("roberto"), &json!(3)), "{h}");
    assert_eq!(j["status"]["rollout"]["version"], next.version(), "{j}");
    // A resume names the hold it lifts; with none, there is nothing to resume.
    assert!(link.resume().await.unwrap_err().contains("no gate holds the change"));
    let held = json!({"gate": "p99Ms", "value": 312.0, "limit": 200.0, "atMs": 1234, "text": "search p99 was 312 ms, over its 200 ms limit, for 30 s"});
    k.merge(&format!("{CLUSTERS}/orders/status"), &json!({"status": {"rollout": {"held": held, "gates": [{"name": "p99Ms", "limit": 200.0, "overSinceMs": 1000}]}}})).await.unwrap();
    k.merge(&format!("{CLUSTERS}/orders"), &json!({"spec": {"rollout": {"gates": {"p99Ms": 200}}}})).await.unwrap();
    kube::once(&k, &now().await).await;
    let kept = now().await["status"]["rollout"]["held"].clone();
    assert!(kept["atMs"] == 1234 && kept["text"] == held["text"], "a hold stays until it is resumed: {kept}");
    let j = link.resume().await.unwrap();
    assert_eq!(j["status"]["rollout"]["held"]["atMs"], 1234);
    assert_eq!(now().await["metadata"]["annotations"][kube::RESUME], "1234");
    kube::once(&k, &now().await).await;
    let r = &now().await["status"]["rollout"];
    assert!(r["held"].is_null() && r["gates"][0]["overSinceMs"].is_null(), "{r}");
    // A rollback: the patch back to version 1, its gates kept, held by none.
    let j = link.get().await.unwrap();
    let back = kube::Link::rollback(&j, 1).unwrap();
    let (_, to) = operator::patched(&j["spec"], &back).unwrap();
    assert_eq!((to.shards, to.replicas, to.rollout.gates.p99_ms), (2, 3, Some(200.0)));
    link.change(&back, &[(kube::ROLLBACK, to.version())]).await.unwrap();
    kube::once(&k, &now().await).await;
    let j = link.get().await.unwrap();
    let h = &j["status"]["history"];
    assert_eq!((h.as_array().map(Vec::len), &h[2]["version"], &h[2]["restores"]), (Some(3), &json!(3), &json!(1)), "{h}");
    assert_eq!((&j["spec"]["shards"], &j["status"]["rollout"]["ungated"]), (&json!(2), &json!(true)), "{j}");
    let gone = kube::Link::with("db/nothing", &kube::Options { api: Some(env.url.clone()), token: Some(TOKEN.into()), ca: Some(env.ca.clone()), ..Default::default() }).unwrap();
    assert!(gone.get().await.unwrap_err().contains("no StrataCluster db/nothing"));

    // A spec that the definition allows but the operator does not.
    k.merge(&format!("{CLUSTERS}/orders"), &json!({"spec": {"schema": {"table": "t", "fields": []}}})).await.unwrap();
    let out = kube::once(&k, &now().await).await;
    assert!(out.phase == Phase::Failed && out.message.contains("give a schema or demo, not both"), "{out:?}");
    assert_eq!(now().await["status"]["phase"], "Failed");
}
