//! Clusters made and deleted from the cloud view, against stand-ins for
//! AWS (CloudFormation and EKS), DigitalOcean and each new cluster's
//! Kubernetes API: the name typed again to confirm, the account's limit
//! checked first, the cloud's cluster followed until it is up, Strata's
//! operator and cluster applied through the new API (with EKS's own kind of
//! token), the copies followed until ready, then the volumes deleted before
//! the cloud's cluster, and every step audited. Nothing here reaches a real
//! cloud.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::auth::{Access, Secret};
use strata_server::aws::Aws;
use strata_server::cloud::clusters::Clusters;
use strata_server::digitalocean::DigitalOcean;
use strata_server::keys::Keys;
use strata_server::service::{AgentConfig, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

const KEY_ID: &str = "AKIAEXAMPLEKEYID0001";
const SECRET: &str = "wJalrXUtnFEMI-the-secret-never-shown-EXAMPLE";
const DO_TOKEN: &str = "dop_v1_the-token-never-shown-0123456789abcdef";
const KUBE_TOKEN: &str = "doks-cluster-token-0123";

const TYPES: &str = include_str!("fixtures/aws/describe-instance-types.xml");
const SPOT: &str = include_str!("fixtures/aws/describe-spot-price-history.xml");
const PRICES: &str = include_str!("fixtures/aws/get-products.json");
const SIZES: &str = include_str!("fixtures/digitalocean/sizes.json");
const OPTIONS: &str = include_str!("fixtures/digitalocean/kubernetes-options.json");

async fn listen() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    (l, url)
}

fn form(body: &str, key: &str) -> Option<String> {
    let u = reqwest::Url::parse(&format!("http://form/?{body}")).ok()?;
    u.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

// ------------------------------------------------------------- Kubernetes

/// A new cluster's API, as far as installing Strata uses it: server-side
/// apply of each object, the StrataCluster's status (first `Creating`,
/// then `Ready`, as the operator would write it), and on deletion the
/// cluster, its volumes' claims and the volumes going (one left at the
/// first look).
#[derive(Default)]
struct KubeApi {
    calls: Vec<String>,
    applied: Vec<(String, Json)>,
    tokens: Vec<String>,
    claims: Vec<String>,
    status_reads: u32,
    volume_looks: u32,
}

async fn kube(State(k): State<Arc<Mutex<KubeApi>>>, method: Method, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Bytes) -> axum::response::Response {
    let mut k = k.lock().unwrap();
    let path = uri.path().to_string();
    k.calls.push(format!("{method} {path}"));
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
    k.tokens.push(auth.trim_start_matches("Bearer ").to_string());
    const CLUSTER: &str = "/apis/strata.chloetech.ai/v1alpha1/namespaces/strata/strataclusters/search";
    const CLAIMS: &str = "/api/v1/namespaces/strata/persistentvolumeclaims";
    let ok = |j: Json| j.to_string().into_response();
    match (method.as_str(), path.as_str()) {
        ("PATCH", p) => {
            assert_eq!(headers.get("content-type").unwrap(), "application/apply-patch+yaml");
            assert!(uri.query().unwrap_or_default().contains("fieldManager="), "{uri}");
            let o: Json = serde_json::from_slice(&body).unwrap();
            if p == CLUSTER {
                k.claims = vec!["data-search-s0-0".into(), "data-search-s1-0".into()];
            }
            k.applied.push((p.to_string(), o.clone()));
            ok(o)
        }
        ("GET", CLUSTER) if k.applied.iter().any(|(p, _)| p == CLUSTER) => {
            k.status_reads += 1;
            let status = if k.status_reads == 1 { json!({"phase": "Creating"}) } else { json!({"phase": "Ready", "message": "", "shards": [{"servers": [{"up": true}, {"up": true}]}, {"servers": [{"up": true}, {"up": true}]}]}) };
            ok(json!({"metadata": {"name": "search"}, "status": status}))
        }
        ("DELETE", CLUSTER) => ok(json!({"status": "Success"})),
        ("GET", CLAIMS) => ok(json!({"items": k.claims.iter().map(|n| json!({"metadata": {"name": n}})).collect::<Vec<_>>()})),
        ("DELETE", p) if p.starts_with(CLAIMS) => {
            let n = p.rsplit('/').next().unwrap().to_string();
            k.claims.retain(|c| *c != n);
            ok(json!({}))
        }
        ("GET", "/api/v1/persistentvolumes") => {
            k.volume_looks += 1;
            let left = if k.volume_looks == 1 { vec![json!({"spec": {"claimRef": {"namespace": "strata"}}})] } else { vec![] };
            ok(json!({"items": left}))
        }
        _ => (StatusCode::NOT_FOUND, json!({"message": "not found"}).to_string()).into_response(),
    }
}

async fn kube_api() -> (Arc<Mutex<KubeApi>>, String) {
    let k = Arc::new(Mutex::new(KubeApi::default()));
    let (l, url) = listen().await;
    let app = axum::Router::new().fallback(kube).with_state(k.clone());
    tokio::spawn(async move { axum::serve(l, app).await });
    (k, url)
}

/// What every cluster gets applied, in order, before its own objects.
const OPERATOR: [&str; 6] = [
    "/apis/apiextensions.k8s.io/v1/customresourcedefinitions/strataclusters.strata.chloetech.ai",
    "/api/v1/namespaces/strata-system",
    "/api/v1/namespaces/strata-system/serviceaccounts/strata-operator",
    "/apis/rbac.authorization.k8s.io/v1/clusterroles/strata-operator",
    "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings/strata-operator",
    "/apis/apps/v1/namespaces/strata-system/deployments/strata-operator",
];

// --------------------------------------------------------------------- AWS

/// CloudFormation's stack, as the stand-in plays it.
#[derive(Default)]
struct Aws_ {
    calls: Vec<String>,
    kube: String,
    stack: Option<String>,
    /// DescribeStacks answers, oldest first, the last one repeated.
    statuses: Vec<&'static str>,
    create: Option<String>,
}

async fn aws(State(seen): State<Arc<Mutex<Aws_>>>, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Bytes) -> axum::response::Response {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let auth = h("authorization");
    let scope: Vec<&str> = auth.strip_prefix("AWS4-HMAC-SHA256 Credential=").and_then(|r| r.split(',').next()).map(|c| c.split('/').collect()).unwrap_or_default();
    assert!(scope.len() == 5 && scope[0] == KEY_ID && !auth.contains(SECRET), "unsigned: {auth:?}");
    let (region, service) = (scope[2], scope[3]);
    let text = String::from_utf8_lossy(&body).to_string();
    let target = h("x-amz-target");
    let action = if service == "eks" {
        assert_eq!(uri.path(), "/clusters/strata-search");
        "DescribeCluster".to_string()
    } else if target.is_empty() {
        form(&text, "Action").unwrap_or_default()
    } else {
        target.rsplit('.').next().unwrap_or_default().to_string()
    };
    let mut s = seen.lock().unwrap();
    s.calls.push(format!("{service} {action}"));
    let xml = |x: &'static str| ([("content-type", "text/xml")], x).into_response();
    let product = |usage: &str, usd: &str| json!({"FormatVersion": "aws_v1", "PriceList": [json!({"product": {"attributes": {"usagetype": usage}}, "terms": {"OnDemand": {"T": {"priceDimensions": {"D": {"unit": "Hrs", "pricePerUnit": {"USD": usd}}}}}}}).to_string()]}).to_string();
    let no_stack = |name: &str| (StatusCode::BAD_REQUEST, json!({"Error": {"Code": "ValidationError", "Message": format!("Stack with id {name} does not exist")}}).to_string()).into_response();
    match (service, action.as_str()) {
        ("sts", "GetCallerIdentity") => json!({"GetCallerIdentityResponse": {"GetCallerIdentityResult": {"Account": "123456789012", "Arn": "arn:aws:iam::123456789012:user/tester", "UserId": "AIDA"}}}).to_string().into_response(),
        ("ec2", "DescribeInstanceTypes") => xml(TYPES),
        ("ec2", "DescribeSpotPriceHistory") => xml(SPOT),
        ("pricing", "GetProducts") => {
            let q: Json = serde_json::from_str(&text).unwrap();
            let family = q["Filters"].as_array().unwrap().iter().find(|x| x["Field"] == "productFamily").map(|x| x["Value"].as_str().unwrap().to_string());
            match (q["ServiceCode"].as_str().unwrap(), family.as_deref()) {
                ("AmazonEC2", Some("Compute Instance")) => PRICES.to_string(),
                ("AmazonEC2", Some("Storage")) => product("EBS:VolumeUsage.gp3", "0.08"),
                ("AmazonEKS", _) => product("USE1-AmazonEKS-Hours:perCluster", "0.10"),
                _ => product("USE1-PublicIPv4:InUseAddress", "0.005"),
            }
            .into_response()
        }
        ("servicequotas", "GetServiceQuota") => {
            let q: Json = serde_json::from_str(&text).unwrap();
            json!({"Quota": {"QuotaCode": q["QuotaCode"], "Value": 64.0}}).to_string().into_response()
        }
        ("cloudformation", "CreateStack") => {
            assert_eq!(region, "us-east-1");
            assert_eq!(form(&text, "Capabilities.member.1").as_deref(), Some("CAPABILITY_IAM"));
            assert_eq!(form(&text, "Tags.member.1.Key").as_deref(), Some("strata.chloetech.ai/cluster"));
            let t: Json = serde_json::from_str(&form(&text, "TemplateBody").unwrap()).unwrap();
            assert!(t["Resources"]["Cluster"]["Type"] == "AWS::EKS::Cluster", "{t}");
            s.create = form(&text, "ClientRequestToken");
            let id = "arn:aws:cloudformation:us-east-1:123456789012:stack/strata-search/0b6f";
            s.stack = Some(id.into());
            s.statuses = vec!["CREATE_IN_PROGRESS", "CREATE_IN_PROGRESS", "CREATE_COMPLETE"];
            json!({"CreateStackResponse": {"CreateStackResult": {"StackId": id}}}).to_string().into_response()
        }
        ("cloudformation", "DescribeStacks") => {
            let name = form(&text, "StackName").unwrap();
            let Some(id) = s.stack.clone() else { return no_stack(&name) };
            let status = if s.statuses.len() > 1 { s.statuses.remove(0) } else { s.statuses[0] };
            if status == "GONE" {
                return no_stack(&name);
            }
            json!({"DescribeStacksResponse": {"DescribeStacksResult": {"Stacks": [{"StackId": id, "StackName": "strata-search", "StackStatus": status}]}}}).to_string().into_response()
        }
        ("cloudformation", "DescribeStackResources") => {
            let r: Vec<Json> = (0..19).map(|i| json!({"ResourceStatus": if i < 7 { "CREATE_COMPLETE" } else { "CREATE_IN_PROGRESS" }})).collect();
            json!({"DescribeStackResourcesResponse": {"DescribeStackResourcesResult": {"StackResources": r}}}).to_string().into_response()
        }
        ("cloudformation", "DeleteStack") => {
            s.statuses = vec!["DELETE_IN_PROGRESS", "GONE"];
            json!({"DeleteStackResponse": {}}).to_string().into_response()
        }
        ("eks", "DescribeCluster") => json!({"cluster": {"name": "strata-search", "endpoint": s.kube, "certificateAuthority": {"data": ""}, "status": "ACTIVE"}}).to_string().into_response(),
        other => panic!("AWS was not asked for {other:?}"),
    }
}

/// Sends `method path body` as the administrator.
async fn call(base: &str, method: &str, path: &str, body: Option<Json>) -> (u16, Json) {
    let c = reqwest::Client::new();
    let mut r = match method {
        "GET" => c.get(format!("{base}{path}")),
        "DELETE" => c.delete(format!("{base}{path}")),
        "PUT" => c.put(format!("{base}{path}")),
        _ => c.post(format!("{base}{path}")),
    }
    .bearer_auth("root-token");
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(json!(text)))
}

/// Waits until the cluster `id` is in `phase`, and answers it.
async fn until(base: &str, id: &str, phase: &str) -> Json {
    let start = Instant::now();
    loop {
        let (status, j) = call(base, "GET", &format!("/v1/admin/cloud/clusters/{id}"), None).await;
        assert_eq!(status, 200, "{j}");
        if j["phase"] == phase {
            return j;
        }
        assert!(j["phase"] != "failed" || phase == "failed", "failed: {j}");
        assert!(start.elapsed() < Duration::from_secs(30), "never {phase}: {j}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn server(aws: Option<&str>, digitalocean: Option<&str>) -> (String, tempfile::TempDir, Arc<Service>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("a.strata"), demo::schema(10), DbOptions::default()).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent, 4, Duration::from_secs(10));
    svc.keys = Arc::new(Keys::open(dir.path().join("a.strata.keys.json")).unwrap());
    if let Some(u) = aws {
        svc.aws = Arc::new(Aws::memory().with_endpoint(u));
    }
    if let Some(u) = digitalocean {
        svc.digitalocean = Arc::new(DigitalOcean::memory().with_endpoint(u));
    }
    svc.clouds = Arc::new(Clusters::open(Clusters::file_of(&dir.path().join("a.strata"))).unwrap().with_every(Duration::from_millis(20)));
    let svc = Arc::new(svc);
    let (l, base) = listen().await;
    let access = Access { token: Some(Secret::new("root-token")), ..Default::default() };
    tokio::spawn(server::serve_http(svc.clone(), l, access));
    (base, dir, svc)
}

async fn audit(base: &str) -> Vec<String> {
    let (_, j) = call(base, "GET", "/v1/admin/audit", None).await;
    j["entries"].as_array().unwrap().iter().filter_map(|e| e["detail"].as_str().map(String::from)).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cluster_is_made_on_eks_installed_and_deleted_volumes_first() {
    let (kube_seen, kube_url) = kube_api().await;
    let seen = Arc::new(Mutex::new(Aws_ { kube: kube_url, ..Default::default() }));
    let (l, aws_url) = listen().await;
    let app = axum::Router::new().fallback(aws).with_state(seen.clone());
    tokio::spawn(async move { axum::serve(l, app).await });
    let (base, dir, _svc) = server(Some(&aws_url), None).await;

    let (status, j) = call(&base, "PUT", "/v1/admin/aws/credentials", Some(json!({"access_key_id": KEY_ID, "secret_access_key": SECRET, "region": "us-east-1"}))).await;
    assert_eq!(status, 200, "{j}");
    let plan = json!({"name": "search", "region": "us-east-1", "machine": "m7g.large", "nodes": 3, "spot": false, "volume_gb": 10, "cluster": {"shards": 2, "replicas": 2}});

    // The name typed again, the provider named, and the quota enough.
    let (status, j) = call(&base, "POST", "/v1/admin/cloud/clusters", Some(json!({"provider": "aws", "plan": plan}))).await;
    assert_eq!(status, 400, "{j}");
    assert!(j["error"].as_str().unwrap().contains("confirm is the cluster's name, strata-search"), "{j}");
    let (status, _) = call(&base, "POST", "/v1/admin/cloud/clusters", Some(json!({"provider": "gcp", "plan": plan, "confirm": "strata-search"}))).await;
    assert_eq!(status, 400);
    let mut big = plan.clone();
    big["nodes"] = json!(40);
    let (status, j) = call(&base, "POST", "/v1/admin/cloud/clusters", Some(json!({"provider": "aws", "plan": big, "confirm": "strata-search"}))).await;
    assert_eq!(status, 409, "{j}");
    assert!(j["error"].as_str().unwrap().contains("the nodes need 80 vCPUs") && j["error"].as_str().unwrap().contains("may run 64"), "{j}");
    assert!(!seen.lock().unwrap().calls.iter().any(|c| c.contains("CreateStack")), "a refused plan made something");

    let (status, j) = call(&base, "POST", "/v1/admin/cloud/clusters", Some(json!({"provider": "aws", "plan": plan, "confirm": "strata-search"}))).await;
    assert_eq!(status, 202, "{j}");
    let id = j["id"].as_str().unwrap().to_string();
    assert_eq!((j["phase"].as_str(), j["cluster"].as_str(), j["machine"].as_str(), j["nodes"].as_u64()), (Some("creating"), Some("strata-search"), Some("m7g.large"), Some(3)));
    assert!(j["cost_hour"].as_f64().unwrap() > 0.1, "{j}");
    // CloudFormation already has it: a second one is refused.
    let (status, _) = call(&base, "POST", "/v1/admin/cloud/clusters", Some(json!({"provider": "aws", "plan": plan, "confirm": "strata-search"}))).await;
    assert_eq!(status, 409);

    let j = until(&base, &id, "ready").await;
    assert_eq!(seen.lock().unwrap().create.as_deref(), Some(id.as_str()), "CreateStack's request token is the cluster's id");
    let steps: Vec<&str> = j["steps"].as_array().unwrap().iter().map(|s| s["text"].as_str().unwrap()).collect();
    assert!(steps.iter().any(|s| s.contains("CloudFormation is making")) && steps.iter().any(|s| s.contains("the stack is made")), "{steps:?}");
    assert!(steps.iter().any(|s| s.starts_with("installed the operator (ghcr.io/chloetech-ai/strata-server:latest)")), "{steps:?}");
    assert!(steps.last().unwrap().starts_with("ready: 4 of 4 copies answering"), "{steps:?}");
    assert_eq!(j["summary"], "Ready, 4 of 4 copies answering");
    assert_eq!(j["connect"], json!(["aws eks update-kubeconfig --region us-east-1 --name strata-search", "kubectl -n strata port-forward svc/search 7700:7700"]));

    {
        let k = kube_seen.lock().unwrap();
        let paths: Vec<&str> = k.applied.iter().map(|(p, _)| p.as_str()).collect();
        let mut want: Vec<&str> = OPERATOR.to_vec();
        want.extend(["/api/v1/namespaces/strata", "/apis/storage.k8s.io/v1/storageclasses/gp3", "/apis/strata.chloetech.ai/v1alpha1/namespaces/strata/strataclusters/search"]);
        assert_eq!(paths, want);
        let spec = &k.applied.last().unwrap().1["spec"];
        assert_eq!((spec["shards"].as_u64(), spec["replicas"].as_u64(), spec["storage"]["storageClassName"].as_str()), (Some(2), Some(2), Some("gp3")));
        // EKS's kind of token: STS's GetCallerIdentity presigned for the
        // cluster, never the secret.
        use base64::Engine;
        let t = k.tokens.last().unwrap();
        let url = String::from_utf8(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(t.strip_prefix("k8s-aws-v1.").unwrap()).unwrap()).unwrap();
        assert!(url.starts_with("https://sts.us-east-1.amazonaws.com/?Action=GetCallerIdentity&"), "{url}");
        assert!(url.contains(&format!("X-Amz-Credential={KEY_ID}%2F")) && url.contains("X-Amz-SignedHeaders=host%3Bx-k8s-aws-id") && url.contains("X-Amz-Expires=60"), "{url}");
        assert!(!url.contains(SECRET));
    }

    // Ready clusters bill: forgetting one is refused, deleting it needs
    // its name.
    let (status, _) = call(&base, "DELETE", &format!("/v1/admin/cloud/clusters/{id}"), None).await;
    assert_eq!(status, 409);
    let (status, _) = call(&base, "POST", &format!("/v1/admin/cloud/clusters/{id}/delete"), Some(json!({"confirm": "search"}))).await;
    assert_eq!(status, 400);
    let (status, j) = call(&base, "POST", &format!("/v1/admin/cloud/clusters/{id}/delete"), Some(json!({"confirm": "strata-search"}))).await;
    assert_eq!(status, 202, "{j}");
    let j = until(&base, &id, "deleted").await;
    let steps: Vec<&str> = j["steps"].as_array().unwrap().iter().map(|s| s["text"].as_str().unwrap()).collect();
    assert!(steps.iter().any(|s| *s == "waiting for 1 volumes to be deleted") && steps.iter().any(|s| *s == "deleted the StrataCluster and its volumes"), "{steps:?}");
    assert!(steps.last().unwrap().starts_with("deleted, after"), "{steps:?}");
    assert!(j["ended_ms"].as_u64().is_some() && j["connect"] == json!([]));
    // The volumes went before the stack.
    let k = kube_seen.lock().unwrap().calls.clone();
    assert!(k.iter().any(|c| c == "DELETE /apis/strata.chloetech.ai/v1alpha1/namespaces/strata/strataclusters/search"), "{k:?}");
    assert_eq!(k.iter().filter(|c| c.starts_with("DELETE /api/v1/namespaces/strata/persistentvolumeclaims/")).count(), 2, "{k:?}");
    assert!(kube_seen.lock().unwrap().claims.is_empty());
    let calls = seen.lock().unwrap().calls.clone();
    assert_eq!(calls.iter().filter(|c| *c == "cloudformation CreateStack").count(), 1, "{calls:?}");
    assert_eq!(calls.iter().filter(|c| *c == "cloudformation DeleteStack").count(), 1, "{calls:?}");

    // Kept in the file; forgotten once deleted.
    let file = Clusters::file_of(&dir.path().join("a.strata"));
    let kept: Json = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(kept[0]["phase"], "deleted");
    assert!(!String::from_utf8_lossy(&std::fs::read(&file).unwrap()).contains(SECRET));
    let (status, _) = call(&base, "DELETE", &format!("/v1/admin/cloud/clusters/{id}"), None).await;
    assert_eq!(status, 200);
    let (_, j) = call(&base, "GET", "/v1/admin/cloud/clusters", None).await;
    assert_eq!(j["clusters"], json!([]));
    let a = audit(&base).await;
    assert!(a.iter().any(|d| d.starts_with("asked AWS for the cluster strata-search in us-east-1: 3 × m7g.large, $")), "{a:?}");
    assert!(a.iter().any(|d| d == "deleting the cluster strata-search on AWS in us-east-1"), "{a:?}");
    assert!(a.iter().any(|d| d == "forgot the cluster strata-search (Deleted)"), "{a:?}");
}

// ------------------------------------------------------------ DigitalOcean

#[derive(Default)]
struct Do {
    calls: Vec<String>,
    kube: String,
    /// GET answers for the cluster, oldest first, the last one repeated.
    states: Vec<&'static str>,
    made: Option<Json>,
}

async fn digitalocean(State(seen): State<Arc<Mutex<Do>>>, method: Method, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Bytes) -> axum::response::Response {
    let path = uri.path().to_string();
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
    assert_eq!(auth, format!("Bearer {DO_TOKEN}"));
    let mut s = seen.lock().unwrap();
    s.calls.push(format!("{method} {path}"));
    const ID: &str = "6e4a1b9c-0000-4000-8000-000000000001";
    let cluster = |state: &str| json!({"kubernetes_cluster": {"id": ID, "name": "strata-search", "status": {"state": state, "message": ""},
        "node_pools": [{"nodes": [{"status": {"state": "running"}}, {"status": {"state": if state == "running" { "running" } else { "provisioning" }}}]}]}});
    match (method.as_str(), path.as_str()) {
        ("GET", "/v2/account") => json!({"account": {"droplet_limit": 25, "volume_limit": 100, "uuid": "acct", "status": "active", "team": {"name": "My Team"}}}).to_string().into_response(),
        ("GET", "/v2/sizes") => SIZES.to_string().into_response(),
        ("GET", "/v2/kubernetes/options") => OPTIONS.to_string().into_response(),
        ("GET", "/v2/droplets") => json!({"droplets": [], "meta": {"total": 3}}).to_string().into_response(),
        ("GET", "/v2/kubernetes/clusters") => {
            let list: Vec<Json> = s.made.iter().filter(|_| s.states.first() != Some(&"GONE")).map(|_| cluster("running")["kubernetes_cluster"].clone()).collect();
            json!({"kubernetes_clusters": list}).to_string().into_response()
        }
        ("POST", "/v2/kubernetes/clusters") => {
            let b: Json = serde_json::from_slice(&body).unwrap();
            assert_eq!((b["name"].as_str(), b["ha"].as_bool(), b["auto_upgrade"].as_bool()), (Some("strata-search"), Some(false), Some(false)), "{b}");
            assert_eq!(b["node_pools"][0]["count"], 2);
            s.made = Some(b);
            s.states = vec!["provisioning", "running"];
            (StatusCode::CREATED, cluster("provisioning").to_string()).into_response()
        }
        ("GET", p) if p == format!("/v2/kubernetes/clusters/{ID}") => {
            let st = if s.states.len() > 1 { s.states.remove(0) } else { s.states[0] };
            if st == "GONE" {
                return (StatusCode::NOT_FOUND, json!({"id": "not_found", "message": "The resource you were accessing could not be found."}).to_string()).into_response();
            }
            cluster(st).to_string().into_response()
        }
        ("GET", p) if p == format!("/v2/kubernetes/clusters/{ID}/credentials") => {
            use base64::Engine;
            json!({"server": s.kube, "certificate_authority_data": base64::engine::general_purpose::STANDARD.encode(""), "token": KUBE_TOKEN, "expires_at": "2026-11-08T00:00:00Z"}).to_string().into_response()
        }
        ("DELETE", p) if p == format!("/v2/kubernetes/clusters/{ID}/destroy_with_associated_resources/dangerous") => {
            s.states = vec!["deleting", "GONE"];
            StatusCode::NO_CONTENT.into_response()
        }
        other => panic!("DigitalOcean was not asked for {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cluster_is_made_on_doks_installed_and_deleted() {
    let (kube_seen, kube_url) = kube_api().await;
    let seen = Arc::new(Mutex::new(Do { kube: kube_url, ..Default::default() }));
    let (l, do_url) = listen().await;
    let app = axum::Router::new().fallback(digitalocean).with_state(seen.clone());
    tokio::spawn(async move { axum::serve(l, app).await });
    let (base, dir, _svc) = server(None, Some(&do_url)).await;
    let (status, j) = call(&base, "PUT", "/v1/admin/digitalocean/token", Some(json!({"token": DO_TOKEN}))).await;
    assert_eq!(status, 200, "{j}");

    let plan = json!({"name": "search", "region": "nyc3", "machine": "m-4vcpu-32gb", "nodes": 2, "volume_gb": 10, "ha": false, "cluster": {"shards": 2, "replicas": 2}});
    let mut big = plan.clone();
    big["nodes"] = json!(30);
    let (status, j) = call(&base, "POST", "/v1/admin/cloud/clusters", Some(json!({"provider": "digitalocean", "plan": big, "confirm": "strata-search"}))).await;
    assert_eq!(status, 409, "{j}");
    assert!(j["error"].as_str().unwrap().contains("the nodes need 30 droplets and the account runs 3 of its 25"), "{j}");
    let (status, j) = call(&base, "POST", "/v1/admin/cloud/clusters", Some(json!({"provider": "digitalocean", "plan": plan, "confirm": "strata-search"}))).await;
    assert_eq!(status, 202, "{j}");
    let id = j["id"].as_str().unwrap().to_string();

    let j = until(&base, &id, "ready").await;
    assert_eq!(j["cloud_id"], "6e4a1b9c-0000-4000-8000-000000000001");
    assert_eq!(j["connect"][0], "doctl kubernetes cluster kubeconfig save 6e4a1b9c-0000-4000-8000-000000000001");
    {
        let k = kube_seen.lock().unwrap();
        let paths: Vec<&str> = k.applied.iter().map(|(p, _)| p.as_str()).collect();
        let mut want: Vec<&str> = OPERATOR.to_vec();
        want.extend(["/api/v1/namespaces/strata", "/apis/strata.chloetech.ai/v1alpha1/namespaces/strata/strataclusters/search"]);
        assert_eq!(paths, want);
        assert_eq!(k.applied.last().unwrap().1["spec"]["storage"]["storageClassName"], "do-block-storage");
        assert!(k.tokens.iter().all(|t| t == KUBE_TOKEN), "the cluster's own token, never the account's");
    }

    // A restart reads the cluster back from the file.
    let again = Clusters::open(Clusters::file_of(&dir.path().join("a.strata"))).unwrap();
    assert_eq!(again.get(&id).unwrap().cloud_id.as_deref(), Some("6e4a1b9c-0000-4000-8000-000000000001"));

    let (status, _) = call(&base, "POST", &format!("/v1/admin/cloud/clusters/{id}/delete"), Some(json!({"confirm": "strata-search"}))).await;
    assert_eq!(status, 202);
    let j = until(&base, &id, "deleted").await;
    assert!(j["steps"].as_array().unwrap().iter().any(|s| s["text"] == "DigitalOcean is deleting the cluster, its nodes and what it made"), "{j}");
    let calls = seen.lock().unwrap().calls.clone();
    assert_eq!(calls.iter().filter(|c| c.starts_with("POST ")).count(), 1, "{calls:?}");
    assert_eq!(calls.iter().filter(|c| c.starts_with("DELETE ")).count(), 1, "{calls:?}");
    let pos = |what: &str| calls.iter().position(|c| c.starts_with(what)).unwrap();
    assert!(kube_seen.lock().unwrap().calls.iter().any(|c| c.starts_with("DELETE /api/v1/namespaces/strata/persistentvolumeclaims/")));
    assert!(pos("DELETE ") > pos("POST "));
    let a = audit(&base).await;
    assert!(a.iter().any(|d| d.starts_with("asked DigitalOcean for the cluster strata-search in nyc3: 2 × m-4vcpu-32gb, $")), "{a:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stack_that_fails_says_why_and_is_deleted_not_forgotten() {
    let seen = Arc::new(Mutex::new(Aws_ { kube: "http://127.0.0.1:9".into(), ..Default::default() }));
    let (l, aws_url) = listen().await;
    let fail = |State(seen): State<Arc<Mutex<Aws_>>>, uri: OriginalUri, headers: HeaderMap, body: Bytes| async move {
        let text = String::from_utf8_lossy(&body).to_string();
        let action = form(&text, "Action").unwrap_or_default();
        if action == "DescribeStackEvents" {
            seen.lock().unwrap().calls.push("cloudformation DescribeStackEvents".into());
            let e = json!([
                {"LogicalResourceId": "Nodes", "ResourceStatus": "CREATE_FAILED", "ResourceStatusReason": "Resource creation cancelled"},
                {"LogicalResourceId": "Nodes", "ResourceStatus": "CREATE_FAILED", "ResourceStatusReason": "You have requested more vCPU capacity than your current vCPU limit of 0 allows"},
                {"LogicalResourceId": "Cluster", "ResourceStatus": "CREATE_COMPLETE"},
            ]);
            return json!({"DescribeStackEventsResponse": {"DescribeStackEventsResult": {"StackEvents": e}}}).to_string().into_response();
        }
        if action == "CreateStack" {
            let r = aws(State(seen.clone()), uri, headers, body).await;
            seen.lock().unwrap().statuses = vec!["CREATE_IN_PROGRESS", "ROLLBACK_IN_PROGRESS", "ROLLBACK_COMPLETE"];
            return r;
        }
        // The first deletion fails (a resource still in use), the second goes.
        let first_delete = action == "DeleteStack" && !seen.lock().unwrap().calls.iter().any(|c| c == "cloudformation DeleteStack");
        if first_delete {
            let r = aws(State(seen.clone()), uri, headers, body).await;
            seen.lock().unwrap().statuses = vec!["DELETE_IN_PROGRESS", "DELETE_FAILED"];
            return r;
        }
        aws(State(seen), uri, headers, body).await
    };
    let app = axum::Router::new().fallback(fail).with_state(seen.clone());
    tokio::spawn(async move { axum::serve(l, app).await });
    let (base, _dir, _svc) = server(Some(&aws_url), None).await;
    let (status, _) = call(&base, "PUT", "/v1/admin/aws/credentials", Some(json!({"access_key_id": KEY_ID, "secret_access_key": SECRET, "region": "us-east-1"}))).await;
    assert_eq!(status, 200);
    let plan = json!({"name": "search", "region": "us-east-1", "machine": "m7g.large", "nodes": 3, "spot": false, "volume_gb": 10, "cluster": {}});
    let (status, j) = call(&base, "POST", "/v1/admin/cloud/clusters", Some(json!({"provider": "aws", "plan": plan, "confirm": "strata-search"}))).await;
    assert_eq!(status, 202, "{j}");
    let id = j["id"].as_str().unwrap().to_string();
    let j = until(&base, &id, "failed").await;
    assert_eq!(j["error"], "CloudFormation could not make the stack (ROLLBACK_IN_PROGRESS): Nodes: You have requested more vCPU capacity than your current vCPU limit of 0 allows");
    // The stack is still there (rolled back): forgetting is refused.
    let (status, _) = call(&base, "DELETE", &format!("/v1/admin/cloud/clusters/{id}"), None).await;
    assert_eq!(status, 409);
    let (status, _) = call(&base, "POST", &format!("/v1/admin/cloud/clusters/{id}/delete"), Some(json!({"confirm": "strata-search"}))).await;
    assert_eq!(status, 202);
    // CloudFormation could not delete it: it says so, may still bill, and is
    // asked again when deleted again.
    let j = until(&base, &id, "failed").await;
    assert!(j["error"].as_str().unwrap().starts_with("CloudFormation could not delete the stack"), "{j}");
    let (status, _) = call(&base, "DELETE", &format!("/v1/admin/cloud/clusters/{id}"), None).await;
    assert_eq!(status, 409);
    let (status, _) = call(&base, "POST", &format!("/v1/admin/cloud/clusters/{id}/delete"), Some(json!({"confirm": "strata-search"}))).await;
    assert_eq!(status, 202);
    until(&base, &id, "deleted").await;
    let calls = seen.lock().unwrap().calls.clone();
    assert_eq!(calls.iter().filter(|c| *c == "cloudformation DeleteStack").count(), 2, "{calls:?}");
    // Nothing was installed, so the cluster's API was never asked.
    assert!(!calls.iter().any(|c| c == "eks DescribeCluster"), "{calls:?}");
}
