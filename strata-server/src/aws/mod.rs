//! AWS for the command center's cloud view: the account's credentials, kept
//! on the server and never answered back, the machines of a region with
//! their prices and the account's quota, and the plan of an EKS cluster
//! for Strata ([`stack`]).
//!
//! Every call is signed here ([`sig`]) and sent with the server's HTTP
//! client: no AWS SDK. The routes are under `/v1/admin/aws` (admin only,
//! and audited where they change something).
//!
//! The credentials are kept in a file of their own, readable by the
//! server's user only (`<db>.aws.json` beside a database, `--aws-file`;
//! in memory on a coordinator without one). Without stored credentials,
//! those of the environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
//! `AWS_SESSION_TOKEN`, `AWS_REGION`) are used.

pub mod catalog;
pub mod sig;
pub mod stack;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::telemetry::now_ms;
use catalog::Machine;

/// An access key: what signs the calls.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Creds {
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    /// The region the view starts in.
    pub region: String,
}

impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Creds({}, {})", mask(&self.access_key_id), self.region)
    }
}

/// An access key id with its middle hidden: `AKIA…WAAU`.
pub fn mask(id: &str) -> String {
    if id.len() <= 8 {
        return "…".into();
    }
    format!("{}…{}", &id[..4], &id[id.len() - 4..])
}

/// Who the credentials are: STS's `GetCallerIdentity`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Identity {
    pub account: String,
    pub arn: String,
    pub user_id: String,
}

impl Identity {
    /// The ARN an EKS access entry takes: an assumed role's session is
    /// the role (`arn:aws:sts::A:assumed-role/R/S` is `arn:aws:iam::A:role/R`).
    pub fn principal(&self) -> String {
        let Some(rest) = self.arn.strip_prefix("arn:aws:sts::").or_else(|| self.arn.strip_prefix("arn:aws-us-gov:sts::")) else { return self.arn.clone() };
        let partition = if self.arn.starts_with("arn:aws-us-gov") { "aws-us-gov" } else { "aws" };
        match rest.split_once(":assumed-role/") {
            Some((account, role)) => format!("arn:{partition}:iam::{account}:role/{}", role.split('/').next().unwrap_or(role)),
            None => self.arn.clone(),
        }
    }
}

/// The credentials kept, and what is known of them.
#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    creds: Creds,
    identity: Identity,
    saved_ms: u64,
    /// Who saved them (a key's name, "token" or "open").
    by: String,
}

/// A region's machines, priced, as fetched at `at_ms`.
#[derive(Clone, Debug, Serialize)]
pub struct Catalog {
    pub region: String,
    pub at_ms: u64,
    pub machines: Vec<Machine>,
    /// The account's limit of vCPUs on standard on-demand instances (A, C,
    /// D, H, I, M, R, T, Z) and on spot ones, in this region.
    pub quota_vcpus: Option<f64>,
    pub spot_quota_vcpus: Option<f64>,
    /// US dollars: an EKS cluster an hour, a GB-month of gp3, a public
    /// IPv4 address an hour.
    pub eks_hour: Option<f64>,
    pub gp3_gb_month: Option<f64>,
    pub ipv4_hour: Option<f64>,
    /// What could not be fetched (a price, a quota), said once each.
    pub missing: Vec<String>,
}

/// A CloudFormation stack's state, from `DescribeStacks`.
#[derive(Clone, Debug, PartialEq)]
pub struct StackState {
    pub id: String,
    /// `CREATE_IN_PROGRESS`, `CREATE_COMPLETE`, `ROLLBACK_COMPLETE`, ...
    pub status: String,
    pub reason: Option<String>,
}

/// How long a region's catalogue is kept before it is fetched again.
const CATALOG_TTL_MS: u64 = 6 * 3600 * 1000;

pub struct Aws {
    stored: Mutex<Option<Stored>>,
    file: Option<PathBuf>,
    /// Where every call goes in place of AWS's endpoints (tests).
    endpoint: Option<String>,
    http: reqwest::Client,
    catalogs: Mutex<HashMap<String, Arc<Catalog>>>,
    /// Calls made, by service and action (tests, and the view's footer).
    calls: Mutex<Vec<String>>,
}

impl Default for Aws {
    fn default() -> Aws {
        Aws::memory()
    }
}

impl Aws {
    /// Credentials kept in memory only.
    pub fn memory() -> Aws {
        Aws {
            stored: Mutex::new(None),
            file: None,
            endpoint: None,
            http: reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap_or_default(),
            catalogs: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The credentials file of a database: `<db>.aws.json`.
    pub fn file_of(db: &Path) -> PathBuf {
        let mut p = db.as_os_str().to_owned();
        p.push(".aws.json");
        p.into()
    }

    /// Credentials kept in `file` (read now if it exists).
    pub fn open(file: PathBuf) -> Result<Aws, String> {
        let stored = match std::fs::read(&file) {
            Ok(b) => Some(serde_json::from_slice::<Stored>(&b).map_err(|e| format!("{}: {e}", file.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", file.display())),
        };
        Ok(Aws { stored: Mutex::new(stored), file: Some(file), ..Aws::memory() })
    }

    /// Every call to `url` (`http://127.0.0.1:PORT`) in place of AWS.
    pub fn with_endpoint(mut self, url: &str) -> Aws {
        self.endpoint = Some(url.trim_end_matches('/').to_string());
        self
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Stored>> {
        self.stored.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The credentials calls are signed with: the stored ones, else the
    /// environment's.
    pub fn creds(&self) -> Option<(Creds, &'static str)> {
        if let Some(s) = self.lock().as_ref() {
            return Some((s.creds.clone(), "stored"));
        }
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let (id, secret) = (var("AWS_ACCESS_KEY_ID")?, var("AWS_SECRET_ACCESS_KEY")?);
        let region = var("AWS_REGION").or_else(|| var("AWS_DEFAULT_REGION")).unwrap_or_else(|| "us-east-1".into());
        Some((Creds { access_key_id: id, secret_access_key: secret, session_token: var("AWS_SESSION_TOKEN"), region }, "environment"))
    }

    /// What the view shows of the credentials: never the secret, nor the
    /// session token.
    pub fn status(&self) -> Json {
        let file = self.file.as_ref().map(|p| p.display().to_string());
        if let Some(s) = self.lock().as_ref() {
            return json!({
                "credentials": {"source": "stored", "access_key_id": mask(&s.creds.access_key_id), "session_token": s.creds.session_token.is_some(),
                    "region": s.creds.region, "account": s.identity.account, "arn": s.identity.arn, "saved_ms": s.saved_ms, "by": s.by},
                "file": file,
            });
        }
        match self.creds() {
            Some((c, source)) => json!({"credentials": {"source": source, "access_key_id": mask(&c.access_key_id), "session_token": c.session_token.is_some(), "region": c.region}, "file": file}),
            None => json!({"credentials": null, "file": file}),
        }
    }

    /// Checks `c` with STS, then keeps them: in the file (mode 0600) when
    /// there is one. Answers who they are.
    pub async fn set(&self, c: Creds, by: &str) -> Result<Identity, String> {
        for (what, v) in [("access_key_id", &c.access_key_id), ("secret_access_key", &c.secret_access_key), ("region", &c.region)] {
            if v.trim().is_empty() {
                return Err(format!("{what} is required"));
            }
        }
        if !c.region.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
            return Err(format!("{:?} is not a region (like us-east-1)", c.region));
        }
        let identity = self.identity(&c).await.map_err(|e| format!("AWS refused these credentials: {e}"))?;
        let s = Stored { creds: c, identity: identity.clone(), saved_ms: now_ms(), by: by.into() };
        if let Some(path) = &self.file {
            write_private(path, &serde_json::to_vec_pretty(&s).map_err(|e| e.to_string())?).map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        *self.lock() = Some(s);
        Ok(identity)
    }

    /// Forgets the stored credentials (and deletes their file). Answers
    /// the key id they had, masked.
    pub fn forget(&self) -> Result<Option<String>, String> {
        let was = self.lock().take().map(|s| mask(&s.creds.access_key_id));
        if let Some(path) = &self.file {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("remove {}: {e}", path.display())),
            }
        }
        Ok(was)
    }

    fn need(&self) -> Result<Creds, String> {
        self.creds().map(|(c, _)| c).ok_or_else(|| "no AWS credentials: save an access key in the Cloud view (PUT /v1/admin/aws/credentials)".into())
    }

    /// The host of a service's endpoint in a region.
    fn host(service: &str, region: &str) -> String {
        match service {
            // The Price List API answers in us-east-1 for every region.
            "pricing" => "api.pricing.us-east-1.amazonaws.com".into(),
            s => format!("{s}.{region}.amazonaws.com"),
        }
    }

    /// One signed call. `body` with `content_type`, or a GET.
    async fn call(&self, c: &Creds, service: &str, region: &str, path: &str, body: Option<(&str, Vec<u8>)>, extra: &[(&str, &str)], what: &str) -> Result<String, String> {
        let sign_region = if service == "pricing" { "us-east-1" } else { region };
        let (base, host) = match &self.endpoint {
            Some(e) => (e.clone(), e.split("://").nth(1).unwrap_or(e).to_string()),
            None => {
                let h = Aws::host(service, region);
                (format!("https://{h}"), h)
            }
        };
        let (method, ctype, bytes) = match &body {
            Some((t, b)) => ("POST", Some(*t), b.as_slice()),
            None => ("GET", None, &[][..]),
        };
        let mut headers: Vec<(&str, &str)> = extra.to_vec();
        if let Some(t) = ctype {
            headers.push(("content-type", t));
        }
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let r = sig::Request { method, host: &host, path, query, headers: &headers, body: bytes };
        let signed = sig::sign(&r, &c.access_key_id, &c.secret_access_key, c.session_token.as_deref(), sign_region, service, &sig::amz_date(now_ms()));
        let url = if query.is_empty() { format!("{base}{path}") } else { format!("{base}{path}?{query}") };
        let mut req = if method == "POST" { self.http.post(url).body(bytes.to_vec()) } else { self.http.get(url) };
        for (k, v) in headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).chain(signed) {
            req = req.header(k, v);
        }
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{service} {what}"));
        let res = req.send().await.map_err(|e| format!("{service} {what}: {e}"))?;
        let status = res.status();
        let text = res.text().await.map_err(|e| format!("{service} {what}: {e}"))?;
        if !status.is_success() {
            return Err(format!("{service} {what}: {status}: {}", error_text(&text)));
        }
        Ok(text)
    }

    /// A query-protocol call (`Action=...&Version=...` in a form body).
    async fn query(&self, c: &Creds, service: &str, region: &str, version: &str, action: &str, params: &[(&str, String)], json_answer: bool) -> Result<String, String> {
        let mut p: Vec<(&str, String)> = vec![("Action", action.into()), ("Version", version.into())];
        p.extend(params.iter().cloned());
        let accept: &[(&str, &str)] = if json_answer { &[("accept", "application/json")] } else { &[] };
        self.call(c, service, region, "/", Some(("application/x-www-form-urlencoded; charset=utf-8", sig::query(&p).into_bytes())), accept, action).await
    }

    /// A JSON-protocol call (`X-Amz-Target`).
    async fn target(&self, c: &Creds, service: &str, region: &str, target: &str, version: &str, body: &Json) -> Result<Json, String> {
        let what = target.rsplit('.').next().unwrap_or(target);
        let t = self.call(c, service, region, "/", Some((version, body.to_string().into_bytes())), &[("x-amz-target", target)], what).await?;
        serde_json::from_str(&t).map_err(|e| format!("{service} {what}: {e}"))
    }

    /// Who `c` are.
    pub async fn identity(&self, c: &Creds) -> Result<Identity, String> {
        let t = self.query(c, "sts", &c.region, "2011-06-15", "GetCallerIdentity", &[], true).await?;
        let j: Json = serde_json::from_str(&t).map_err(|e| format!("sts GetCallerIdentity: {e}"))?;
        let r = &j["GetCallerIdentityResponse"]["GetCallerIdentityResult"];
        let s = |k: &str| r[k].as_str().unwrap_or_default().to_string();
        Ok(Identity { account: s("Account"), arn: s("Arn"), user_id: s("UserId") })
    }

    /// The ARN an EKS cluster made with these credentials makes its
    /// administrator ([`Identity::principal`]).
    pub async fn principal(&self) -> Result<String, String> {
        if let Some(s) = self.lock().as_ref() {
            return Ok(s.identity.principal());
        }
        let c = self.need()?;
        Ok(self.identity(&c).await?.principal())
    }

    /// The regions the account has enabled.
    pub async fn regions(&self) -> Result<Vec<String>, String> {
        let c = self.need()?;
        catalog::regions(&self.query(&c, "ec2", &c.region, "2016-11-15", "DescribeRegions", &[], false).await?)
    }

    /// A region's machines, priced, and the account's quotas there: from
    /// the cache when fetched in the last six hours (`fresh`: fetched again).
    pub async fn catalog(&self, region: &str, fresh: bool) -> Result<Arc<Catalog>, String> {
        if !region.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') || region.is_empty() {
            return Err(format!("{region:?} is not a region (like us-east-1)"));
        }
        if !fresh {
            if let Some(c) = self.catalogs.lock().unwrap_or_else(|e| e.into_inner()).get(region).filter(|c| now_ms().saturating_sub(c.at_ms) < CATALOG_TTL_MS) {
                return Ok(c.clone());
            }
        }
        let c = self.need()?;
        let (types, prices, spot, quota, spot_quota, extras) = tokio::join!(
            self.instance_types(&c, region),
            self.instance_prices(&c, region),
            self.spot_prices(&c, region),
            self.quota(&c, region, "L-1216C47A"),
            self.quota(&c, region, "L-34B43A08"),
            self.extra_prices(&c, region),
        );
        let mut machines = types?;
        let mut missing = Vec::new();
        let prices = prices.unwrap_or_else(|e| {
            missing.push(format!("on-demand prices: {e}"));
            HashMap::new()
        });
        let spot = spot.unwrap_or_else(|e| {
            missing.push(format!("spot prices: {e}"));
            HashMap::new()
        });
        for m in &mut machines {
            if let Some((usd, cpu)) = prices.get(&m.name) {
                m.usd_hour = (*usd > 0.0).then_some(*usd);
                m.processor = (!cpu.is_empty()).then(|| cpu.clone());
            }
            m.spot_hour = spot.get(&m.name).copied();
        }
        machines.sort_by(|a, b| a.family.cmp(&b.family).then(a.vcpu.cmp(&b.vcpu)).then(a.memory_gib.total_cmp(&b.memory_gib)));
        let mut q = |r: Result<f64, String>, what: &str| {
            r.map_err(|e| missing.push(format!("{what}: {e}"))).ok()
        };
        let quota_vcpus = q(quota, "the on-demand vCPU quota");
        let spot_quota_vcpus = q(spot_quota, "the spot vCPU quota");
        let (eks_hour, gp3_gb_month, ipv4_hour) = extras.unwrap_or_else(|e| {
            missing.push(format!("EKS, gp3 and IPv4 prices: {e}"));
            (None, None, None)
        });
        let cat = Arc::new(Catalog { region: region.into(), at_ms: now_ms(), machines, quota_vcpus, spot_quota_vcpus, eks_hour, gp3_gb_month, ipv4_hour, missing });
        self.catalogs.lock().unwrap_or_else(|e| e.into_inner()).insert(region.into(), cat.clone());
        Ok(cat)
    }

    async fn instance_types(&self, c: &Creds, region: &str) -> Result<Vec<Machine>, String> {
        let mut out = Vec::new();
        let mut next: Option<String> = None;
        for _ in 0..50 {
            let mut p = vec![("MaxResults", "100".to_string()), ("Filter.1.Name", "current-generation".into()), ("Filter.1.Value.1", "true".into())];
            if let Some(t) = &next {
                p.push(("NextToken", t.clone()));
            }
            let (m, n) = catalog::instance_types(&self.query(c, "ec2", region, "2016-11-15", "DescribeInstanceTypes", &p, false).await?)?;
            out.extend(m);
            next = n;
            if next.is_none() {
                break;
            }
        }
        Ok(out)
    }

    async fn instance_prices(&self, c: &Creds, region: &str) -> Result<HashMap<String, (f64, String)>, String> {
        let filters = [
            ("regionCode", region),
            ("productFamily", "Compute Instance"),
            ("operatingSystem", "Linux"),
            ("tenancy", "Shared"),
            ("preInstalledSw", "NA"),
            ("capacitystatus", "Used"),
            ("licenseModel", "No License required"),
            ("currentGeneration", "Yes"),
        ];
        let mut out = HashMap::new();
        let mut next: Option<String> = None;
        for _ in 0..100 {
            let mut b = price_query("AmazonEC2", &filters);
            if let Some(t) = &next {
                b["NextToken"] = json!(t);
            }
            let page = self.target(c, "pricing", "us-east-1", "AWSPriceListService.GetProducts", "application/x-amz-json-1.1", &b).await?;
            next = catalog::instance_prices(&page, &mut out)?;
            if next.is_none() {
                break;
            }
        }
        Ok(out)
    }

    async fn spot_prices(&self, c: &Creds, region: &str) -> Result<HashMap<String, f64>, String> {
        let now = sig::amz_date(now_ms());
        let start = format!("{}-{}-{}T{}:{}:{}Z", &now[..4], &now[4..6], &now[6..8], &now[9..11], &now[11..13], &now[13..15]);
        let mut out = HashMap::new();
        let mut next: Option<String> = None;
        for _ in 0..50 {
            let mut p = vec![("ProductDescription.1", "Linux/UNIX".to_string()), ("StartTime", start.clone()), ("MaxResults", "1000".into())];
            if let Some(t) = &next {
                p.push(("NextToken", t.clone()));
            }
            next = catalog::spot_prices(&self.query(c, "ec2", region, "2016-11-15", "DescribeSpotPriceHistory", &p, false).await?, &mut out)?;
            if next.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// A quota's value in a region (Service Quotas, EC2).
    async fn quota(&self, c: &Creds, region: &str, code: &str) -> Result<f64, String> {
        let j = self.target(c, "servicequotas", region, "ServiceQuotasV20190624.GetServiceQuota", "application/x-amz-json-1.1", &json!({"ServiceCode": "ec2", "QuotaCode": code})).await?;
        j["Quota"]["Value"].as_f64().ok_or_else(|| format!("no value for the quota {code}"))
    }

    /// An EKS cluster an hour, a GB-month of gp3, a public IPv4 address an
    /// hour, in `region`.
    async fn extra_prices(&self, c: &Creds, region: &str) -> Result<(Option<f64>, Option<f64>, Option<f64>), String> {
        let get = |service: &'static str, filters: Vec<(&'static str, String)>, usage: &'static str| async move {
            let f: Vec<(&str, &str)> = filters.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let page = self.target(c, "pricing", "us-east-1", "AWSPriceListService.GetProducts", "application/x-amz-json-1.1", &price_query(service, &f)).await?;
            Ok::<_, String>(price_with_usage(&page, usage))
        };
        let r = region.to_string();
        let (eks, gp3, ipv4) = tokio::join!(
            get("AmazonEKS", vec![("regionCode", r.clone()), ("operation", "CreateOperation".into())], "AmazonEKS-Hours:perCluster"),
            get("AmazonEC2", vec![("regionCode", r.clone()), ("productFamily", "Storage".into()), ("volumeApiName", "gp3".into())], "EBS:VolumeUsage.gp3"),
            get("AmazonVPC", vec![("regionCode", r), ("group", "VPCPublicIPv4Address".into())], "PublicIPv4:InUseAddress"),
        );
        Ok((eks?, gp3?, ipv4?))
    }

    /// CloudFormation's own check of a template (free; creates nothing).
    pub async fn validate(&self, region: &str, template: &Json) -> Result<Json, String> {
        let c = self.need()?;
        let t = self.query(&c, "cloudformation", region, "2010-05-15", "ValidateTemplate", &[("TemplateBody", template.to_string())], true).await?;
        let j: Json = serde_json::from_str(&t).map_err(|e| format!("cloudformation ValidateTemplate: {e}"))?;
        let r = &j["ValidateTemplateResponse"]["ValidateTemplateResult"];
        Ok(json!({"capabilities": r["Capabilities"], "reason": r["CapabilitiesReason"], "parameters": r["Parameters"]}))
    }

    /// Makes the stack `name` from `template` (CloudFormation `CreateStack`
    /// with `CAPABILITY_IAM`, rolled back if any resource fails). `request`
    /// makes the same call again a no-op. This bills the account.
    pub async fn create_stack(&self, region: &str, name: &str, template: &Json, request: &str, tags: &[(&str, &str)]) -> Result<String, String> {
        let c = self.need()?;
        let mut p: Vec<(String, String)> = vec![
            ("StackName".into(), name.into()),
            ("TemplateBody".into(), template.to_string()),
            ("Capabilities.member.1".into(), "CAPABILITY_IAM".into()),
            ("ClientRequestToken".into(), request.into()),
            ("OnFailure".into(), "ROLLBACK".into()),
        ];
        for (i, (k, v)) in tags.iter().enumerate() {
            p.push((format!("Tags.member.{}.Key", i + 1), k.to_string()));
            p.push((format!("Tags.member.{}.Value", i + 1), v.to_string()));
        }
        let p: Vec<(&str, String)> = p.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let t = self.query(&c, "cloudformation", region, "2010-05-15", "CreateStack", &p, true).await?;
        let j: Json = serde_json::from_str(&t).map_err(|e| format!("cloudformation CreateStack: {e}"))?;
        j["CreateStackResponse"]["CreateStackResult"]["StackId"].as_str().map(str::to_string).ok_or_else(|| format!("cloudformation CreateStack: no stack id in {t}"))
    }

    /// The stack `id` (or name) now; `None` when CloudFormation has none.
    pub async fn stack(&self, region: &str, id: &str) -> Result<Option<StackState>, String> {
        let c = self.need()?;
        let t = match self.query(&c, "cloudformation", region, "2010-05-15", "DescribeStacks", &[("StackName", id.into())], true).await {
            Ok(t) => t,
            Err(e) if e.contains("does not exist") => return Ok(None),
            Err(e) => return Err(e),
        };
        let j: Json = serde_json::from_str(&t).map_err(|e| format!("cloudformation DescribeStacks: {e}"))?;
        let s = &j["DescribeStacksResponse"]["DescribeStacksResult"]["Stacks"][0];
        Ok(s["StackStatus"].as_str().map(|status| StackState {
            id: s["StackId"].as_str().unwrap_or(id).to_string(),
            status: status.to_string(),
            reason: s["StackStatusReason"].as_str().filter(|r| !r.is_empty()).map(str::to_string),
        }))
    }

    /// Why the stack failed: the first of its resources that did, and why.
    pub async fn stack_failure(&self, region: &str, id: &str) -> Result<Option<String>, String> {
        let c = self.need()?;
        let t = self.query(&c, "cloudformation", region, "2010-05-15", "DescribeStackEvents", &[("StackName", id.into())], true).await?;
        let j: Json = serde_json::from_str(&t).map_err(|e| format!("cloudformation DescribeStackEvents: {e}"))?;
        let events = j["DescribeStackEventsResponse"]["DescribeStackEventsResult"]["StackEvents"].as_array().cloned().unwrap_or_default();
        // Newest first: the earliest failure is the cause, the others follow it.
        Ok(events.iter().rev().find(|e| {
            e["ResourceStatus"].as_str().is_some_and(|s| s.ends_with("_FAILED")) && e["ResourceStatusReason"].as_str().is_some_and(|r| !r.contains("cancelled"))
        }).map(|e| format!("{}: {}", e["LogicalResourceId"].as_str().unwrap_or("?"), e["ResourceStatusReason"].as_str().unwrap_or("?"))))
    }

    /// How many of the stack's resources are made, of how many it has.
    pub async fn stack_progress(&self, region: &str, id: &str) -> Result<(usize, usize), String> {
        let c = self.need()?;
        let t = self.query(&c, "cloudformation", region, "2010-05-15", "DescribeStackResources", &[("StackName", id.into())], true).await?;
        let j: Json = serde_json::from_str(&t).map_err(|e| format!("cloudformation DescribeStackResources: {e}"))?;
        let r = j["DescribeStackResourcesResponse"]["DescribeStackResourcesResult"]["StackResources"].as_array().cloned().unwrap_or_default();
        Ok((r.iter().filter(|r| r["ResourceStatus"] == "CREATE_COMPLETE").count(), r.len()))
    }

    /// Deletes the stack `id` (`DeleteStack`): its nodes, cluster and
    /// network. Deleting one already gone does nothing.
    pub async fn delete_stack(&self, region: &str, id: &str) -> Result<(), String> {
        let c = self.need()?;
        self.query(&c, "cloudformation", region, "2010-05-15", "DeleteStack", &[("StackName", id.into())], true).await.map(|_| ())
    }

    /// The EKS cluster `name`'s API: its endpoint and its CA (PEM).
    pub async fn eks_cluster(&self, region: &str, name: &str) -> Result<(String, Vec<u8>), String> {
        use base64::Engine;
        let c = self.need()?;
        let t = self.call(&c, "eks", region, &format!("/clusters/{}", sig::encode(name, true)), None, &[], "DescribeCluster").await?;
        let j: Json = serde_json::from_str(&t).map_err(|e| format!("eks DescribeCluster: {e}"))?;
        let endpoint = j["cluster"]["endpoint"].as_str().filter(|e| !e.is_empty()).ok_or_else(|| format!("eks DescribeCluster: {name} has no endpoint yet"))?;
        let ca = j["cluster"]["certificateAuthority"]["data"].as_str().unwrap_or_default();
        let ca = base64::engine::general_purpose::STANDARD.decode(ca.trim()).map_err(|e| format!("eks DescribeCluster: the CA of {name}: {e}"))?;
        Ok((endpoint.to_string(), ca))
    }

    /// A token for the EKS cluster `name`'s API, as `aws eks get-token`
    /// makes one: STS's `GetCallerIdentity` presigned for the cluster,
    /// which EKS takes for 15 minutes as who signed it.
    pub fn eks_token(&self, region: &str, name: &str) -> Result<String, String> {
        use base64::Engine;
        let c = self.need()?;
        let host = format!("sts.{region}.amazonaws.com");
        let params = [("Action", "GetCallerIdentity".to_string()), ("Version", "2011-06-15".to_string())];
        let q = sig::presign(&host, "/", &params, &[("x-k8s-aws-id", name)], &c.access_key_id, &c.secret_access_key, c.session_token.as_deref(), region, "sts", &sig::amz_date(now_ms()), 60);
        Ok(format!("k8s-aws-v1.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("https://{host}/?{q}"))))
    }

    /// The calls made so far, `service Action` each (tests).
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// A `GetProducts` body: `TERM_MATCH` on each filter.
fn price_query(service: &str, filters: &[(&str, &str)]) -> Json {
    json!({
        "ServiceCode": service,
        "FormatVersion": "aws_v1",
        "MaxResults": 100,
        "Filters": filters.iter().map(|(k, v)| json!({"Type": "TERM_MATCH", "Field": k, "Value": v})).collect::<Vec<_>>(),
    })
}

/// The price of the product whose usage type ends with `usage` (usage
/// types start with a region's code: `USE1-AmazonEKS-Hours:perCluster`).
fn price_with_usage(page: &Json, usage: &str) -> Option<f64> {
    page["PriceList"].as_array()?.iter().find_map(|p| {
        let p: Json = match p {
            Json::String(s) => serde_json::from_str(s).ok()?,
            other => other.clone(),
        };
        let (usd, a) = catalog::product_price(&p)?;
        let u = a["usagetype"].as_str()?;
        (u == usage || u.ends_with(&format!("-{usage}"))).then_some(usd)
    })
}

/// The message of an AWS error answer (XML or JSON), else its start.
fn error_text(body: &str) -> String {
    if let Ok(j) = serde_json::from_str::<Json>(body) {
        let e = if j["Error"].is_object() { &j["Error"] } else { &j };
        let code = e["Code"].as_str().or(e["__type"].as_str()).unwrap_or_default();
        let msg = e["Message"].as_str().or(e["message"].as_str()).unwrap_or_default();
        if !code.is_empty() || !msg.is_empty() {
            return format!("{}: {msg}", code.rsplit('#').next().unwrap_or(code));
        }
    }
    if let Ok(d) = roxmltree::Document::parse(body) {
        if let Some(e) = d.descendants().find(|n| n.has_tag_name("Error")) {
            let t = |name: &str| e.children().find(|c| c.has_tag_name(name)).and_then(|c| c.text()).unwrap_or_default().to_string();
            return format!("{}: {}", t("Code"), t("Message"));
        }
    }
    body.chars().take(300).collect()
}

/// Writes `bytes` to `path` readable by its owner only, through a
/// temporary file renamed over it.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_masked_and_sessions_are_their_roles() {
        assert_eq!(mask("AKIAIOSFODNN7EXAMPLE"), "AKIA…MPLE");
        assert_eq!(mask("short"), "…");
        let c = Creds { access_key_id: "AKIAIOSFODNN7EXAMPLE".into(), secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(), session_token: None, region: "us-east-1".into() };
        assert_eq!(format!("{c:?}"), "Creds(AKIA…MPLE, us-east-1)");
        let i = |arn: &str| Identity { arn: arn.into(), ..Identity::default() }.principal();
        assert_eq!(i("arn:aws:iam::123456789012:user/dev"), "arn:aws:iam::123456789012:user/dev");
        assert_eq!(i("arn:aws:sts::123456789012:assumed-role/Admin/roberto@example.com"), "arn:aws:iam::123456789012:role/Admin");
        assert_eq!(error_text(r#"{"__type":"com.amazon#ResourceNotFoundException","message":"no"}"#), "ResourceNotFoundException: no");
        assert_eq!(error_text(r#"{"Error":{"Code":"InvalidClientTokenId","Message":"The security token included in the request is invalid."}}"#), "InvalidClientTokenId: The security token included in the request is invalid.");
        assert_eq!(error_text("<ErrorResponse><Error><Code>ValidationError</Code><Message>bad</Message></Error></ErrorResponse>"), "ValidationError: bad");
        let page = json!({"PriceList": [json!({"product": {"attributes": {"usagetype": "USE1-AmazonEKS-Hours:perCluster"}}, "terms": {"OnDemand": {"a": {"priceDimensions": {"b": {"pricePerUnit": {"USD": "0.1000000000"}}}}}}}).to_string()]});
        assert_eq!(price_with_usage(&page, "AmazonEKS-Hours:perCluster"), Some(0.1));
        assert_eq!(price_with_usage(&page, "Other"), None);
    }

    #[test]
    fn stored_credentials_are_private_to_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = Aws::file_of(&dir.path().join("x.strata"));
        assert!(path.ends_with("x.strata.aws.json"));
        let s = Stored { creds: Creds { access_key_id: "AKIAIOSFODNN7EXAMPLE".into(), secret_access_key: "secret".into(), session_token: None, region: "eu-west-1".into() }, identity: Identity::default(), saved_ms: 1, by: "token".into() };
        write_private(&path, &serde_json::to_vec(&s).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let a = Aws::open(path.clone()).unwrap();
        let st = a.status();
        assert_eq!(st["credentials"]["access_key_id"], "AKIA…MPLE");
        assert!(!st.to_string().contains("secret"), "{st}");
        assert_eq!(a.forget().unwrap().as_deref(), Some("AKIA…MPLE"));
        assert!(!path.exists());
    }
}
