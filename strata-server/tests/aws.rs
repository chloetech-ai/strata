//! The AWS view's routes against a stand-in for AWS: credentials checked
//! with STS and kept readable by their owner only, the secret in no answer
//! and no audit entry, machines with their prices and quotas, a plan and
//! CloudFormation's check of its template, and only an administrator
//! reaching any of it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::DbOptions;
use strata_server::auth::{Access, Secret};
use strata_server::aws::Aws;
use strata_server::keys::Keys;
use strata_server::service::{AgentConfig, Backend, Service};
use strata_server::{demo, server};
use tokio::net::TcpListener;

const KEY_ID: &str = "AKIAEXAMPLEKEYID0001";
const SECRET: &str = "wJalrXUtnFEMI-the-secret-never-shown-EXAMPLE";
const BAD_KEY_ID: &str = "AKIAEXAMPLEREFUSED01";
const ARN: &str = "arn:aws:iam::123456789012:user/tester";

const TYPES: &str = include_str!("fixtures/aws/describe-instance-types.xml");
const SPOT: &str = include_str!("fixtures/aws/describe-spot-price-history.xml");
const REGIONS: &str = include_str!("fixtures/aws/describe-regions.xml");
const PRICES: &str = include_str!("fixtures/aws/get-products.json");

/// What the stand-in saw: `service Action` with the signature's scope,
/// and the last template it was asked to check.
#[derive(Default)]
struct Seen {
    calls: Vec<String>,
    template: Option<Json>,
}

/// The value of `key` in a form body.
fn form(body: &str, key: &str) -> Option<String> {
    let u = reqwest::Url::parse(&format!("http://form/?{body}")).ok()?;
    u.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

/// A price list page of one product (`usage` its usage type).
fn product(usage: &str, usd: &str) -> String {
    json!({"FormatVersion": "aws_v1", "PriceList": [json!({
        "product": {"attributes": {"usagetype": usage}},
        "terms": {"OnDemand": {"T": {"priceDimensions": {"D": {"unit": "Hrs", "pricePerUnit": {"USD": usd}}}}}},
    }).to_string()]})
    .to_string()
}

/// AWS, as far as the view uses it: every call is checked to be signed
/// (SigV4, with the service and region of its endpoint), and a key id other
/// than [`KEY_ID`] is refused as AWS refuses it.
async fn aws(State(seen): State<Arc<Mutex<Seen>>>, headers: HeaderMap, body: Bytes) -> axum::response::Response {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let auth = h("authorization");
    // `AWS4-HMAC-SHA256 Credential=KEY/DAY/REGION/SERVICE/aws4_request, SignedHeaders=..., Signature=<64 hex>`
    let scope: Vec<&str> = auth.strip_prefix("AWS4-HMAC-SHA256 Credential=").and_then(|r| r.split(',').next()).map(|c| c.split('/').collect()).unwrap_or_default();
    let sig_ok = auth.rsplit("Signature=").next().is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(scope.len() == 5 && scope[4] == "aws4_request" && sig_ok && auth.contains("SignedHeaders=") && !h("x-amz-date").is_empty(), "unsigned: {auth:?}");
    assert!(!auth.contains(SECRET), "the secret went out in a header");
    let (key, region, service) = (scope[0], scope[2], scope[3]);
    let text = String::from_utf8_lossy(&body).to_string();
    let target = h("x-amz-target");
    let action = if target.is_empty() {
        form(&text, "Action").unwrap_or_default()
    } else {
        target.rsplit('.').next().unwrap_or_default().to_string()
    };
    seen.lock().unwrap().calls.push(format!("{service} {action} {region}"));
    if key != KEY_ID {
        let e = json!({"Error": {"Code": "InvalidClientTokenId", "Message": "The security token included in the request is invalid."}});
        return (StatusCode::FORBIDDEN, e.to_string()).into_response();
    }
    let xml = |s: &'static str| ([("content-type", "text/xml")], s).into_response();
    match (service, action.as_str()) {
        ("sts", "GetCallerIdentity") => {
            assert_eq!(h("accept"), "application/json");
            json!({"GetCallerIdentityResponse": {"GetCallerIdentityResult": {"Account": "123456789012", "Arn": ARN, "UserId": "AIDAEXAMPLE"}}}).to_string().into_response()
        }
        ("ec2", "DescribeRegions") => xml(REGIONS),
        ("ec2", "DescribeInstanceTypes") => xml(TYPES),
        ("ec2", "DescribeSpotPriceHistory") => xml(SPOT),
        ("pricing", "GetProducts") => {
            // The price list answers in us-east-1 whatever the region.
            assert_eq!(region, "us-east-1");
            let q: Json = serde_json::from_str(&text).unwrap();
            let filter = |f: &str| q["Filters"].as_array().unwrap().iter().find(|x| x["Field"] == f).map(|x| x["Value"].as_str().unwrap().to_string());
            match (q["ServiceCode"].as_str().unwrap(), filter("productFamily").as_deref()) {
                ("AmazonEC2", Some("Compute Instance")) => PRICES.to_string(),
                ("AmazonEC2", Some("Storage")) => product("EBS:VolumeUsage.gp3", "0.08"),
                ("AmazonEKS", _) => product("USE1-AmazonEKS-Hours:perCluster", "0.10"),
                ("AmazonVPC", _) => product("USE1-PublicIPv4:InUseAddress", "0.005"),
                other => panic!("no prices for {other:?}"),
            }
            .into_response()
        }
        ("servicequotas", "GetServiceQuota") => {
            let q: Json = serde_json::from_str(&text).unwrap();
            let v = if q["QuotaCode"] == "L-1216C47A" { 64.0 } else { 32.0 };
            json!({"Quota": {"QuotaCode": q["QuotaCode"], "Value": v}}).to_string().into_response()
        }
        ("cloudformation", "ValidateTemplate") => {
            let body = form(&text, "TemplateBody").unwrap();
            seen.lock().unwrap().template = Some(serde_json::from_str(&body).unwrap());
            json!({"ValidateTemplateResponse": {"ValidateTemplateResult": {"Capabilities": ["CAPABILITY_IAM"], "CapabilitiesReason": "The following resource(s) require capabilities: [AWS::IAM::Role]", "Parameters": []}}})
                .to_string()
                .into_response()
        }
        other => panic!("AWS was not asked for {other:?}"),
    }
}

async fn listen() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    (l, url)
}

/// Sends `method path body` with `auth` as the bearer credential; the
/// status and the answer, whole, as text.
async fn call(base: &str, method: &str, path: &str, auth: Option<&str>, body: Option<Json>) -> (u16, String) {
    let c = reqwest::Client::new();
    let mut r = match method {
        "GET" => c.get(format!("{base}{path}")),
        "DELETE" => c.delete(format!("{base}{path}")),
        "PUT" => c.put(format!("{base}{path}")),
        _ => c.post(format!("{base}{path}")),
    };
    if let Some(a) = auth {
        r = r.bearer_auth(a);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let r = r.send().await.unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

fn parse(s: &str) -> Json {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("{e}: {s}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_aws_view_keeps_its_keys_private_and_plans_from_aws_prices() {
    // The environment's keys would stand in for missing ones.
    let env_keys = std::env::var("AWS_ACCESS_KEY_ID").is_ok_and(|v| !v.is_empty());

    let seen = Arc::new(Mutex::new(Seen::default()));
    let (l, aws_url) = listen().await;
    let app = axum::Router::new().fallback(aws).with_state(seen.clone());
    tokio::spawn(async move { axum::serve(l, app).await });

    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("a.strata"), demo::schema(10), DbOptions::default()).unwrap();
    let schema = (*db.schema()).clone();
    let agent = AgentConfig { api_key: None, model: String::new(), base_url: String::new() };
    let mut svc = Service::new(Backend::Local(Arc::new(db)), schema, agent, 4, Duration::from_secs(10));
    svc.keys = Arc::new(Keys::open(dir.path().join("a.strata.keys.json")).unwrap());
    let file = Aws::file_of(&dir.path().join("a.strata"));
    svc.aws = Arc::new(Aws::open(file.clone()).unwrap().with_endpoint(&aws_url));
    let svc = Arc::new(svc);
    let (l, base) = listen().await;
    let access = Access { token: Some(Secret::new("root-token")), ..Default::default() };
    tokio::spawn(server::serve_http(svc.clone(), l, access));
    let root = Some("root-token");
    // Every answer the view gets, to look for the secret in.
    let mut answers = Vec::new();

    // Only an administrator reaches the AWS view.
    let (status, j) = call(&base, "POST", "/v1/admin/keys", root, Some(json!({"name": "ingest", "scope": "write"}))).await;
    assert_eq!(status, 200, "{j}");
    let writer = parse(&j)["secret"].as_str().unwrap().to_string();
    let creds = json!({"access_key_id": KEY_ID, "secret_access_key": SECRET, "region": "us-east-1"});
    for (method, path, body) in [("GET", "/v1/admin/aws", None), ("PUT", "/v1/admin/aws/credentials", Some(creds.clone())), ("GET", "/v1/admin/aws/machines", None), ("POST", "/v1/admin/aws/plan", Some(json!({})))] {
        assert_eq!(call(&base, method, path, None, body.clone()).await.0, 401, "{method} {path}");
        assert_eq!(call(&base, method, path, Some(&writer), body).await.0, 403, "{method} {path}");
    }
    assert!(!file.exists() && seen.lock().unwrap().calls.is_empty());

    // Nothing saved: no credentials, and nothing to plan with.
    let (status, j) = call(&base, "GET", "/v1/admin/aws", root, None).await;
    assert_eq!(status, 200);
    if !env_keys {
        assert_eq!(parse(&j)["credentials"], Json::Null, "{j}");
        let (status, j) = call(&base, "GET", "/v1/admin/aws/machines", root, None).await;
        assert_eq!(status, 400, "{j}");
        assert!(j.contains("no AWS credentials"), "{j}");
    }

    // Keys AWS refuses are not kept.
    let bad = json!({"access_key_id": BAD_KEY_ID, "secret_access_key": SECRET, "region": "us-east-1"});
    let (status, j) = call(&base, "PUT", "/v1/admin/aws/credentials", root, Some(bad)).await;
    assert_eq!(status, 400, "{j}");
    assert!(j.contains("AWS refused these credentials") && j.contains("InvalidClientTokenId"), "{j}");
    answers.push(j);
    assert!(!file.exists());
    for (body, says) in [
        (json!({"access_key_id": KEY_ID, "secret_access_key": "", "region": "us-east-1"}), "secret_access_key is required"),
        (json!({"access_key_id": KEY_ID, "secret_access_key": SECRET, "region": "US East"}), "is not a region"),
        (json!({"access_key_id": KEY_ID}), "the body is"),
    ] {
        let (status, j) = call(&base, "PUT", "/v1/admin/aws/credentials", root, Some(body)).await;
        assert!(status == 400 && j.contains(says), "{status} {j}");
    }

    // Good keys: checked with STS, kept in a file only their owner reads,
    // shown masked.
    let (status, j) = call(&base, "PUT", "/v1/admin/aws/credentials", root, Some(creds)).await;
    assert_eq!(status, 200, "{j}");
    let saved = parse(&j);
    assert_eq!(saved["identity"]["arn"], ARN, "{j}");
    assert_eq!(saved["credentials"]["source"], "stored");
    assert_eq!(saved["credentials"]["account"], "123456789012");
    let masked = saved["credentials"]["access_key_id"].as_str().unwrap().to_string();
    assert!(masked != KEY_ID && masked.starts_with("AKIA") && masked.ends_with("0001"), "{masked}");
    answers.push(j);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }
    // A restart reads them back.
    let again = Aws::open(file.clone()).unwrap();
    assert_eq!(again.status()["credentials"]["arn"], ARN);
    assert_eq!(again.creds().unwrap().0.secret_access_key, SECRET);

    // The account's regions, and a region's machines with their prices
    // (on demand and spot), quotas, and what EKS, gp3 and an IPv4 address cost.
    let (status, j) = call(&base, "GET", "/v1/admin/aws/regions", root, None).await;
    assert_eq!(status, 200, "{j}");
    assert!(parse(&j)["regions"].as_array().unwrap().iter().any(|r| r == "us-east-1"), "{j}");
    let (status, j) = call(&base, "GET", "/v1/admin/aws/machines?region=us-east-1", root, None).await;
    assert_eq!(status, 200, "{j}");
    let cat = parse(&j);
    let m = cat["machines"].as_array().unwrap().iter().find(|m| m["type"] == "m7g.large").unwrap_or_else(|| panic!("{j}"));
    assert_eq!((m["usd_hour"].as_f64(), m["processor"].as_str(), m["arch"].as_str(), m["max_pods"].as_u64()), (Some(0.0816), Some("AWS Graviton3 Processor"), Some("arm64"), Some(29)), "{m}");
    assert!(m["spot_hour"].as_f64().is_some_and(|s| s > 0.0 && s <= 0.0523), "{m}");
    assert_eq!((cat["quota_vcpus"].as_f64(), cat["spot_quota_vcpus"].as_f64()), (Some(64.0), Some(32.0)), "{j}");
    assert_eq!((cat["eks_hour"].as_f64(), cat["gp3_gb_month"].as_f64(), cat["ipv4_hour"].as_f64()), (Some(0.10), Some(0.08), Some(0.005)), "{j}");
    assert_eq!(cat["missing"], json!([]), "{j}");
    assert_eq!(cat["recommended"], json!(["m7g.large"]), "{j}");
    answers.push(j);
    // Kept for six hours: asked again, AWS is not.
    let before = seen.lock().unwrap().calls.len();
    assert_eq!(call(&base, "GET", "/v1/admin/aws/machines?region=us-east-1", root, None).await.0, 200);
    assert_eq!(seen.lock().unwrap().calls.len(), before);
    let (status, j) = call(&base, "GET", "/v1/admin/aws/machines?region=../x", root, None).await;
    assert_eq!(status, 400, "{j}");

    // A plan: three m7g.large nodes, two shards of three copies.
    let plan = json!({"name": "search", "region": "us-east-1", "machine": "m7g.large", "nodes": 3, "spot": false, "volume_gb": 10, "cluster": {"shards": 2, "replicas": 3}});
    let (status, j) = call(&base, "POST", "/v1/admin/aws/plan", root, Some(plan.clone())).await;
    assert_eq!(status, 200, "{j}");
    let p = parse(&j);
    assert_eq!(p["stack"], "strata-search");
    assert_eq!(p["layout"]["copies_per_node"], 2, "{j}");
    assert_eq!(p["layout"]["nodes"].as_array().unwrap().len(), 3);
    // The nodes, EKS, six 10 GiB volumes and three 20 GiB roots of gp3, and
    // the nodes' addresses, an hour.
    let hour = p["cost"]["hour"].as_f64().unwrap();
    let want = 3.0 * 0.0816 + 0.10 + (6.0 * 10.0 + 3.0 * 20.0) * 0.08 / 730.0 + 3.0 * 0.005;
    assert!((hour - want).abs() < 1e-9, "{hour} vs {want}: {}", p["cost"]);
    assert_eq!(p["cost"]["unknown"], json!([]), "{j}");
    assert_eq!(p["quota"]["vcpus"].as_f64(), Some(64.0));
    let template = p["template"].to_string();
    assert!(template.contains(ARN) && template.contains("m7g.large") && template.contains("AWS::EKS::Cluster"), "{template}");
    // YAML documents written as JSON, which YAML reads.
    let manifests = p["manifests"]["cluster"].as_str().unwrap();
    assert!(manifests.contains("\"kind\": \"StrataCluster\"") && manifests.contains("\"provisioner\": \"ebs.csi.aws.com\""), "{manifests}");
    answers.push(j);
    for (bad, says) in [
        (json!({"name": "Not A Name", "region": "us-east-1", "machine": "m7g.large", "nodes": 3, "spot": false, "volume_gb": 10, "cluster": {}}), "is not a cluster name"),
        (json!({"name": "search", "region": "us-east-1", "machine": "p5.48xlarge", "nodes": 3, "spot": false, "volume_gb": 10, "cluster": {}}), "is not a machine"),
        (json!({"name": "search", "region": "us-east-1", "machine": "m7g.large", "nodes": 0, "spot": false, "volume_gb": 10, "cluster": {}}), "nodes is 1 to 100"),
    ] {
        let (status, j) = call(&base, "POST", "/v1/admin/aws/plan", root, Some(bad)).await;
        assert!(status == 400 && j.contains(says), "{status} {j}");
    }

    // CloudFormation checks the template it is sent: the plan's own.
    let (status, j) = call(&base, "POST", "/v1/admin/aws/validate", root, Some(plan)).await;
    assert_eq!(status, 200, "{j}");
    let v = parse(&j);
    assert_eq!(v["validated"]["ok"], true, "{j}");
    assert_eq!(v["validated"]["answer"]["capabilities"], json!(["CAPABILITY_IAM"]));
    assert_eq!(seen.lock().unwrap().template.as_ref(), Some(&v["template"]));
    answers.push(j);

    // Every call was signed for its own service and region; nothing was
    // created.
    let calls = seen.lock().unwrap().calls.clone();
    for want in ["sts GetCallerIdentity us-east-1", "ec2 DescribeRegions us-east-1", "ec2 DescribeInstanceTypes us-east-1", "ec2 DescribeSpotPriceHistory us-east-1", "pricing GetProducts us-east-1", "servicequotas GetServiceQuota us-east-1", "cloudformation ValidateTemplate us-east-1"] {
        assert!(calls.iter().any(|c| c == want), "{want} not in {calls:?}");
    }
    assert!(calls.iter().all(|c| ["Get", "Describe", "Validate"].iter().any(|v| c.split(' ').nth(1).unwrap().starts_with(v))), "{calls:?}");

    // Forgetting deletes the file.
    let (status, j) = call(&base, "DELETE", "/v1/admin/aws/credentials", root, None).await;
    assert_eq!(status, 200, "{j}");
    assert_eq!(parse(&j)["forgot"], json!(masked));
    assert!(!file.exists());
    answers.push(j);
    if !env_keys {
        assert_eq!(parse(&call(&base, "GET", "/v1/admin/aws", root, None).await.1)["credentials"], Json::Null);
    }

    // The audit log says who saved and forgot which key, by its masked id.
    let (status, j) = call(&base, "GET", "/v1/admin/audit", root, None).await;
    assert_eq!(status, 200);
    let details: Vec<String> = parse(&j)["entries"].as_array().unwrap().iter().filter_map(|e| e["detail"].as_str().map(String::from)).collect();
    assert!(details.iter().any(|d| d == &format!("saved the AWS credentials {masked} (account 123456789012, {ARN})")), "{details:?}");
    assert!(details.iter().any(|d| d == &format!("forgot the AWS credentials {masked}")), "{details:?}");
    answers.push(j);

    // The secret is in no answer and no audit entry.
    for a in &answers {
        assert!(!a.contains(SECRET) && !a.contains(KEY_ID), "{a}");
    }
}
