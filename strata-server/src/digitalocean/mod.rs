//! DigitalOcean for the command center's cloud view: the account's
//! personal access token, kept on the server and never answered back, the
//! sizes a DOKS node may be in a region with their prices, the account's
//! droplet limit, and the plan of a DOKS cluster for Strata ([`plan`]).
//!
//! Every call reads (`GET` with the token as a bearer) through the
//! server's HTTP client. The routes are under `/v1/admin/digitalocean`
//! (admin only, and audited where they change something).
//!
//! The token is kept in a file of its own, readable by the server's user
//! only (`<db>.digitalocean.json` beside a database; in memory on a
//! coordinator without one). Without a stored one, the environment's
//! (`DIGITALOCEAN_TOKEN` or `DIGITALOCEAN_ACCESS_TOKEN`) is used.

pub mod plan;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::aws::catalog::Machine;
use crate::aws::{mask, write_private};
use crate::telemetry::now_ms;

const API: &str = "https://api.digitalocean.com";

/// A block storage volume, US dollars a GiB-month
/// (<https://docs.digitalocean.com/products/volumes/details/pricing/>).
pub const VOLUME_GB_MONTH: f64 = 0.10;

/// A highly available control plane, US dollars a month
/// (<https://docs.digitalocean.com/products/kubernetes/details/pricing/>);
/// the standard one is free.
pub const HA_MONTH: f64 = 40.0;

/// The hours DigitalOcean bills a month at most: an hourly price is the
/// monthly one over these.
pub const HOURS_A_MONTH: f64 = 672.0;

/// The pods a DOKS node may run
/// (<https://docs.digitalocean.com/products/kubernetes/details/limits/>).
pub const MAX_PODS: u32 = 110;

/// Who the token is: `GET /v2/account`, without the e-mail address.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub uuid: String,
    pub team: String,
    pub status: String,
    pub droplet_limit: u32,
    pub volume_limit: u32,
}

/// The token kept, and what is known of it.
#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    token: String,
    account: Account,
    saved_ms: u64,
    /// Who saved it (a key's name, "token" or "open").
    by: String,
}

/// A region's sizes, priced, as fetched at `at_ms`, with what DOKS offers.
#[derive(Clone, Debug, Serialize)]
pub struct Catalog {
    pub region: String,
    pub at_ms: u64,
    /// The sizes a DOKS node may be here (no GPUs, nothing legacy).
    pub machines: Vec<Machine>,
    /// DOKS's Kubernetes versions, newest first.
    pub versions: Vec<String>,
    /// The account's droplet limit, and the droplets it runs now.
    pub droplet_limit: Option<u32>,
    pub droplets: Option<u32>,
    /// US dollars: a GiB-month of block storage, a month of a highly
    /// available control plane.
    pub volume_gb_month: f64,
    pub ha_month: f64,
    /// What could not be fetched, said once each.
    pub missing: Vec<String>,
}

/// A region DOKS runs in.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Region {
    pub slug: String,
    pub name: String,
}

/// How long a region's catalogue is kept before it is fetched again.
const CATALOG_TTL_MS: u64 = 6 * 3600 * 1000;

pub struct DigitalOcean {
    stored: Mutex<Option<Stored>>,
    file: Option<PathBuf>,
    /// Where every call goes in place of `api.digitalocean.com` (tests).
    endpoint: Option<String>,
    http: reqwest::Client,
    catalogs: Mutex<HashMap<String, Arc<Catalog>>>,
    /// Calls made, `GET /v2/...` each (tests).
    calls: Mutex<Vec<String>>,
}

impl Default for DigitalOcean {
    fn default() -> DigitalOcean {
        DigitalOcean::memory()
    }
}

/// What a size is for, from its slug and description; `None` for those a
/// cluster does not run on (GPUs, legacy sizes).
pub fn kind(slug: &str, description: &str) -> Option<&'static str> {
    if description.starts_with("Legacy") || slug.starts_with("gpu-") {
        return None;
    }
    let family = slug.split('-').next().unwrap_or_default();
    let series: String = family.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    Some(match series.as_str() {
        // Shared CPUs: Basic and v5 Shared.
        "s" => "shared",
        "g" | "gd" => "general",
        "c" => "compute",
        "m" => "memory",
        "so" => "storage",
        _ => return None,
    })
}

/// The sizes of `GET /v2/sizes` a DOKS node may be in `region`
/// (`k8s`: the slugs of `/v2/kubernetes/options`), as machines.
pub fn machines(sizes: &Json, region: &str, k8s: &[String]) -> Vec<Machine> {
    let mut out: Vec<Machine> = sizes["sizes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let slug = s["slug"].as_str()?;
            let description = s["description"].as_str().unwrap_or_default();
            let kind = kind(slug, description)?;
            let here = s["available"].as_bool() == Some(true) && s["regions"].as_array()?.iter().any(|r| r == region);
            if !here || !k8s.iter().any(|k| k == slug) {
                return None;
            }
            // `m-2vcpu-16gb-intel`: family `m-intel`.
            let vendor = ["intel", "amd"].into_iter().find(|v| slug.ends_with(&format!("-{v}")));
            let series = slug.split('-').next().unwrap_or_default();
            let family = match vendor {
                Some(v) => format!("{series}-{v}"),
                None => series.to_string(),
            };
            let transfer = s["transfer"].as_f64().unwrap_or(0.0);
            let mbps = s["networking_throughput"].as_u64().unwrap_or(0);
            let network = match (transfer > 0.0, mbps > 0) {
                (true, true) => format!("{:.0} Mbit/s, {transfer} TB a month free", mbps),
                (true, false) => format!("{transfer} TB a month free"),
                _ => String::new(),
            };
            Some(Machine {
                name: slug.into(),
                family,
                kind,
                arch: "x86_64".into(),
                vcpu: s["vcpus"].as_u64()? as u32,
                memory_gib: s["memory"].as_f64()? / 1024.0,
                burstable: kind == "shared",
                local_gb: s["disk"].as_u64().unwrap_or(0) as u32,
                network,
                ebs_mbps: 0,
                max_pods: MAX_PODS,
                processor: Some(description.to_string()),
                usd_hour: s["price_hourly"].as_f64().filter(|p| *p > 0.0),
                spot_hour: None,
            })
        })
        .collect();
    out.sort_by(|a, b| a.family.cmp(&b.family).then(a.vcpu.cmp(&b.vcpu)).then(a.memory_gib.total_cmp(&b.memory_gib)));
    out
}

/// The machines suggested for search: the dedicated general and
/// memory-optimized sizes of 2 to 8 vCPUs (regular CPUs, not the Premium
/// ones, which cost more for the same memory).
pub fn recommended(machines: &[Machine]) -> Vec<&Machine> {
    machines.iter().filter(|m| (m.family == "g" || m.family == "m") && (2..=8).contains(&m.vcpu) && m.usd_hour.is_some()).collect()
}

/// DOKS's versions (newest first), regions and node sizes:
/// `GET /v2/kubernetes/options`.
pub fn options(j: &Json) -> (Vec<String>, Vec<Region>, Vec<String>) {
    let list = |k: &str| j["options"][k].as_array().cloned().unwrap_or_default();
    let versions = list("versions").iter().filter_map(|v| v["slug"].as_str().map(String::from)).collect();
    let regions = list("regions").iter().filter_map(|r| Some(Region { slug: r["slug"].as_str()?.into(), name: r["name"].as_str().unwrap_or_default().into() })).collect();
    let sizes = list("sizes").iter().filter_map(|s| s["slug"].as_str().map(String::from)).collect();
    (versions, regions, sizes)
}

/// The message of an API error answer, else its start.
fn error_text(body: &str) -> String {
    if let Ok(j) = serde_json::from_str::<Json>(body) {
        if let Some(m) = j["message"].as_str() {
            return match j["id"].as_str() {
                Some(id) => format!("{id}: {m}"),
                None => m.to_string(),
            };
        }
    }
    body.chars().take(300).collect()
}

impl DigitalOcean {
    /// A token kept in memory only.
    pub fn memory() -> DigitalOcean {
        DigitalOcean {
            stored: Mutex::new(None),
            file: None,
            endpoint: None,
            http: reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap_or_default(),
            catalogs: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The token file of a database: `<db>.digitalocean.json`.
    pub fn file_of(db: &Path) -> PathBuf {
        let mut p = db.as_os_str().to_owned();
        p.push(".digitalocean.json");
        p.into()
    }

    /// A token kept in `file` (read now if it exists).
    pub fn open(file: PathBuf) -> Result<DigitalOcean, String> {
        let stored = match std::fs::read(&file) {
            Ok(b) => Some(serde_json::from_slice::<Stored>(&b).map_err(|e| format!("{}: {e}", file.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", file.display())),
        };
        Ok(DigitalOcean { stored: Mutex::new(stored), file: Some(file), ..DigitalOcean::memory() })
    }

    /// Every call to `url` (`http://127.0.0.1:PORT`) in place of the API.
    pub fn with_endpoint(mut self, url: &str) -> DigitalOcean {
        self.endpoint = Some(url.trim_end_matches('/').to_string());
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Stored>> {
        self.stored.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The token calls are made with: the stored one, else the
    /// environment's.
    pub fn token(&self) -> Option<(String, &'static str)> {
        if let Some(s) = self.lock().as_ref() {
            return Some((s.token.clone(), "stored"));
        }
        ["DIGITALOCEAN_TOKEN", "DIGITALOCEAN_ACCESS_TOKEN"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).map(|t| (t, "environment"))
    }

    /// What the view shows of the token: never the token itself.
    pub fn status(&self) -> Json {
        let file = self.file.as_ref().map(|p| p.display().to_string());
        if let Some(s) = self.lock().as_ref() {
            return json!({"token": {"source": "stored", "token": mask(&s.token), "account": s.account, "saved_ms": s.saved_ms, "by": s.by}, "file": file});
        }
        match self.token() {
            Some((t, source)) => json!({"token": {"source": source, "token": mask(&t)}, "file": file}),
            None => json!({"token": null, "file": file}),
        }
    }

    /// Checks `token` (`GET /v2/account`), then keeps it: in the file (mode
    /// 0600) when there is one. Answers whose it is.
    pub async fn set(&self, token: String, by: &str) -> Result<Account, String> {
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err("token is required".into());
        }
        if !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("a token is letters, digits and punctuation".into());
        }
        let account = self.account(&token).await.map_err(|e| format!("DigitalOcean refused this token: {e}"))?;
        let s = Stored { token, account: account.clone(), saved_ms: now_ms(), by: by.into() };
        if let Some(path) = &self.file {
            write_private(path, &serde_json::to_vec_pretty(&s).map_err(|e| e.to_string())?).map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        *self.lock() = Some(s);
        Ok(account)
    }

    /// Forgets the stored token (and deletes its file). Answers it, masked.
    pub fn forget(&self) -> Result<Option<String>, String> {
        let was = self.lock().take().map(|s| mask(&s.token));
        if let Some(path) = &self.file {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("remove {}: {e}", path.display())),
            }
        }
        Ok(was)
    }

    fn need(&self) -> Result<String, String> {
        self.token().map(|(t, _)| t).ok_or_else(|| "no DigitalOcean token: save one in the Cloud view (PUT /v1/admin/digitalocean/token)".into())
    }

    /// One `GET` of the API.
    async fn get(&self, token: &str, path: &str) -> Result<Json, String> {
        let base = self.endpoint.as_deref().unwrap_or(API);
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push(format!("GET {}", path.split('?').next().unwrap_or(path)));
        let res = self.http.get(format!("{base}{path}")).bearer_auth(token).send().await.map_err(|e| format!("GET {path}: {e}"))?;
        let status = res.status();
        let text = res.text().await.map_err(|e| format!("GET {path}: {e}"))?;
        if !status.is_success() {
            return Err(format!("GET {path}: {status}: {}", error_text(&text)));
        }
        serde_json::from_str(&text).map_err(|e| format!("GET {path}: {e}"))
    }

    /// Whose `token` is.
    pub async fn account(&self, token: &str) -> Result<Account, String> {
        let a = self.get(token, "/v2/account").await?;
        let a = &a["account"];
        let s = |k: &str| a[k].as_str().unwrap_or_default().to_string();
        let n = |k: &str| a[k].as_u64().unwrap_or(0) as u32;
        Ok(Account { uuid: s("uuid"), team: a["team"]["name"].as_str().unwrap_or_default().into(), status: s("status"), droplet_limit: n("droplet_limit"), volume_limit: n("volume_limit") })
    }

    /// DOKS's options: versions, regions, node sizes.
    pub async fn options(&self) -> Result<(Vec<String>, Vec<Region>, Vec<String>), String> {
        let t = self.need()?;
        Ok(options(&self.get(&t, "/v2/kubernetes/options").await?))
    }

    /// The droplets the account runs now.
    async fn droplets(&self, token: &str) -> Result<u32, String> {
        let j = self.get(token, "/v2/droplets?per_page=1").await?;
        Ok(j["meta"]["total"].as_u64().unwrap_or(0) as u32)
    }

    /// Every size, through the pages.
    async fn sizes(&self, token: &str) -> Result<Json, String> {
        let mut all = Vec::new();
        for page in 1..=20 {
            let j = self.get(token, &format!("/v2/sizes?per_page=200&page={page}")).await?;
            let got = j["sizes"].as_array().cloned().unwrap_or_default();
            let n = got.len();
            all.extend(got);
            let total = j["meta"]["total"].as_u64().unwrap_or(0) as usize;
            if n == 0 || all.len() >= total || j["links"]["pages"]["next"].is_null() {
                break;
            }
        }
        Ok(json!({"sizes": all}))
    }

    /// A region's sizes, priced, DOKS's versions and the account's droplets:
    /// from the cache when fetched in the last six hours (`fresh`: fetched
    /// again).
    pub async fn catalog(&self, region: &str, fresh: bool) -> Result<Arc<Catalog>, String> {
        if region.is_empty() || !region.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()) {
            return Err(format!("{region:?} is not a region (like nyc3)"));
        }
        if !fresh {
            if let Some(c) = self.catalogs.lock().unwrap_or_else(|e| e.into_inner()).get(region).filter(|c| now_ms().saturating_sub(c.at_ms) < CATALOG_TTL_MS) {
                return Ok(c.clone());
            }
        }
        let t = self.need()?;
        let (sizes, opts, account, droplets) = tokio::join!(self.sizes(&t), self.get(&t, "/v2/kubernetes/options"), self.account(&t), self.droplets(&t));
        let (versions, regions, k8s) = options(&opts?);
        if !regions.iter().any(|r| r.slug == region) {
            return Err(format!("DOKS does not run in {region:?}: it runs in {}", regions.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>().join(", ")));
        }
        let mut missing = Vec::new();
        let droplet_limit = account.map(|a| a.droplet_limit).map_err(|e| missing.push(format!("the droplet limit: {e}"))).ok();
        let droplets = droplets.map_err(|e| missing.push(format!("the droplets running: {e}"))).ok();
        let cat = Arc::new(Catalog {
            region: region.into(),
            at_ms: now_ms(),
            machines: machines(&sizes?, region, &k8s),
            versions,
            droplet_limit,
            droplets,
            volume_gb_month: VOLUME_GB_MONTH,
            ha_month: HA_MONTH,
            missing,
        });
        self.catalogs.lock().unwrap_or_else(|e| e.into_inner()).insert(region.into(), cat.clone());
        Ok(cat)
    }

    /// The regions DOKS runs in.
    pub async fn regions(&self) -> Result<Vec<Region>, String> {
        Ok(self.options().await?.1)
    }

    /// The calls made so far (tests).
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: &str = include_str!("../../tests/fixtures/digitalocean/sizes.json");
    const OPTIONS: &str = include_str!("../../tests/fixtures/digitalocean/kubernetes-options.json");

    #[test]
    fn sizes_are_the_nodes_doks_takes_in_a_region() {
        let (versions, regions, k8s) = options(&serde_json::from_str(OPTIONS).unwrap());
        assert_eq!(versions[0], "1.37.0-do.2");
        assert!(regions.iter().any(|r| r.slug == "nyc3" && r.name == "New York 3"), "{regions:?}");
        let ms = machines(&serde_json::from_str(SIZES).unwrap(), "nyc3", &k8s);
        let names: Vec<&str> = ms.iter().map(|m| m.name.as_str()).collect();
        // No GPU, no legacy size, nothing DOKS does not take (so-), nothing
        // not sold in nyc3 (g5-, the Premium Intel ones).
        assert_eq!(names, ["c-16", "g-8vcpu-32gb", "m-2vcpu-16gb", "m-4vcpu-32gb", "m-8vcpu-64gb", "m-16vcpu-128gb", "s-4vcpu-8gb"], "{names:?}");
        let m = ms.iter().find(|m| m.name == "m-8vcpu-64gb").unwrap();
        assert_eq!((m.family.as_str(), m.kind, m.vcpu, m.memory_gib, m.local_gb, m.max_pods, m.usd_hour), ("m", "memory", 8, 64.0, 200, 110, Some(0.5)));
        assert_eq!(m.processor.as_deref(), Some("Memory-Optimized"));
        let s = ms.iter().find(|m| m.name == "s-4vcpu-8gb").unwrap();
        assert!(s.burstable && s.kind == "shared");
        let r: Vec<&str> = recommended(&ms).iter().map(|m| m.name.as_str()).collect();
        assert_eq!(r, ["g-8vcpu-32gb", "m-2vcpu-16gb", "m-4vcpu-32gb", "m-8vcpu-64gb"]);
        // Elsewhere, a Premium Intel size, in a family of its own, and a v5 one.
        let nyc2 = machines(&serde_json::from_str(SIZES).unwrap(), "nyc2", &k8s);
        let i = nyc2.iter().find(|m| m.name == "m-2vcpu-16gb-intel").unwrap();
        assert_eq!((i.family.as_str(), i.kind, i.network.as_str()), ("m-intel", "memory", "10000 Mbit/s, 4 TB a month free"));
        let ric1 = machines(&serde_json::from_str(SIZES).unwrap(), "ric1", &k8s);
        assert_eq!(ric1.iter().map(|m| (m.name.as_str(), m.family.as_str(), m.kind)).collect::<Vec<_>>(), [("g5-16vcpu-64gb-80gb", "g5", "general")]);
        assert_eq!([kind("gpu-h100x1-80gb", "H100 GPU - 1X"), kind("512mb", "Legacy Basic"), kind("so1_5-2vcpu-16gb", "Storage-Optimized 1.5x SSD"), kind("c2-2vcpu-4gb", "CPU-Optimized 2x SSD"), kind("s5-1vcpu-1gb-30gb", "v5 Shared")], [None, None, Some("storage"), Some("compute"), Some("shared")]);
        assert_eq!(error_text(r#"{"id":"unauthorized","message":"Unable to authenticate you"}"#), "unauthorized: Unable to authenticate you");
    }

    #[test]
    fn a_stored_token_is_private_to_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = DigitalOcean::file_of(&dir.path().join("x.strata"));
        assert!(path.ends_with("x.strata.digitalocean.json"));
        let s = Stored { token: "dop_v1_0123456789abcdef".into(), account: Account::default(), saved_ms: 1, by: "token".into() };
        write_private(&path, &serde_json::to_vec(&s).unwrap()).unwrap();
        let d = DigitalOcean::open(path.clone()).unwrap();
        let st = d.status();
        assert_eq!(st["token"]["token"], "dop_…cdef");
        assert!(!st.to_string().contains("0123456789"), "{st}");
        assert_eq!(d.forget().unwrap().as_deref(), Some("dop_…cdef"));
        assert!(!path.exists());
    }
}
