//! New versions of ChloeDB: a cluster follows an image tag (`spec.upgrade`,
//! by default its image's own, such as
//! `ghcr.io/chloetech-ai/chloedb-server:latest`), and when the
//! tag points to another image than the one its servers run, the operator
//! suggests it (`suggest`) or rolls it out (`on`), as the image's digest
//! (`repository@sha256:...`), so that every server runs the same bits even
//! when one restarts after the tag moved on.
//!
//! The operator asks the image's registry (the OCI distribution API, with
//! an anonymous token when it asks for one) every `checkMinutes`: the
//! digest the tag names, and the commit it was built from (the image's
//! `org.opencontainers.image.revision` label) and when. What it found is
//! `status.upgrade`. The rollout itself is any other change of the image
//! ([`super::reconcile`]): tried on every node first, one server at a time,
//! verified, and rolled back when it fails a check; an image rolled back so
//! is not suggested or rolled out again (`rejected`), though a person may
//! still ask for it.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

pub use super::autoscale::Mode;

/// `spec.upgrade`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Upgrade {
    /// Off: no registry is asked. Suggest: a new image waits in the
    /// status for a person. On: the operator rolls it out once the
    /// cluster is ready.
    pub mode: Mode,
    /// The image tag to follow (default: `spec.image`'s repository, at its
    /// tag, or at `latest` when it names a digest).
    pub track: Option<String>,
    /// Minutes between two looks at the registry.
    pub check_minutes: u64,
}

impl Default for Upgrade {
    fn default() -> Upgrade {
        Upgrade { mode: Mode::Off, track: None, check_minutes: 10 }
    }
}

impl Upgrade {
    pub fn check(&self, image: &str) -> Result<(), String> {
        if self.mode != Mode::Off && self.check_minutes == 0 {
            return Err("upgrade.checkMinutes is 1 or more".into());
        }
        let track = self.tracked(image);
        let r = Ref::parse(&track).map_err(|e| format!("upgrade.track: {e}"))?;
        if r.tag.is_none() {
            return Err(format!("upgrade.track names a tag to follow (repository:tag), not a digest: {track}"));
        }
        Ok(())
    }

    /// The tag followed for a cluster whose image is `image`.
    pub fn tracked(&self, image: &str) -> String {
        if let Some(t) = self.track.as_deref().filter(|t| !t.is_empty()) {
            return t.to_string();
        }
        match Ref::parse(image) {
            Ok(r) => format!("{}:{}", r.repository(), r.tag.as_deref().filter(|_| r.digest.is_none()).unwrap_or("latest")),
            Err(_) => image.to_string(),
        }
    }

    pub fn said(&self) -> String {
        match self.mode {
            Mode::Off => "upgrades off".into(),
            Mode::Suggest => format!("a new image is suggested, every {} min", self.check_minutes),
            Mode::On => format!("a new image is rolled out, every {} min", self.check_minutes),
        }
    }
}

/// An image reference: `[registry/]path[:tag][@digest]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Ref {
    /// The registry's host (and port), as the reference names it, or
    /// `docker.io`.
    pub registry: String,
    pub path: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl Ref {
    pub fn parse(s: &str) -> Result<Ref, String> {
        let s = s.trim();
        if s.is_empty() || s.contains(char::is_whitespace) {
            return Err(format!("{s:?} is not an image reference"));
        }
        let (rest, digest) = match s.split_once('@') {
            Some((r, d)) if d.starts_with("sha256:") && d.len() == 71 => (r, Some(d.to_string())),
            Some(_) => return Err(format!("{s}: a digest is sha256: and 64 hex digits")),
            None => (s, None),
        };
        let (registry, path) = match rest.split_once('/') {
            Some((h, p)) if h.contains('.') || h.contains(':') || h == "localhost" => (h.to_string(), p.to_string()),
            _ => ("docker.io".to_string(), rest.to_string()),
        };
        // A tag follows the last colon of the path.
        let (path, tag) = match path.rsplit_once(':') {
            Some((p, t)) if !t.contains('/') => (p.to_string(), Some(t.to_string())),
            _ => (path, None),
        };
        let path = if registry == "docker.io" && !path.contains('/') { format!("library/{path}") } else { path };
        if path.is_empty() {
            return Err(format!("{s}: no repository"));
        }
        let tag = if tag.is_none() && digest.is_none() { Some("latest".to_string()) } else { tag };
        Ok(Ref { registry, path, tag, digest })
    }

    /// `registry/path`.
    pub fn repository(&self) -> String {
        format!("{}/{}", self.registry, self.path)
    }

    /// The reference by digest: `registry/path@digest`.
    pub fn pinned(&self, digest: &str) -> String {
        format!("{}@{digest}", self.repository())
    }

    /// The base URL of the registry's API.
    fn api(&self) -> String {
        let host = if self.registry == "docker.io" { "registry-1.docker.io" } else { &self.registry };
        // A registry on this machine (a test's) speaks plain HTTP.
        let plain = host.starts_with("localhost") || host.starts_with("127.");
        format!("{}://{host}/v2/{}", if plain { "http" } else { "https" }, self.path)
    }
}

/// An image the registry has: its digest, and the commit and time it was
/// built from, when its labels say.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Image {
    /// `registry/path@sha256:...`.
    pub image: String,
    pub digest: String,
    pub revision: Option<String>,
    pub created: Option<String>,
}

const MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

/// A registry's client: the OCI distribution API, with the anonymous
/// token a registry asks for (`WWW-Authenticate: Bearer realm=...`).
pub struct Registry {
    http: reqwest::Client,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::new()
    }
}

impl Registry {
    pub fn new() -> Registry {
        Registry { http: crate::http_client().timeout(Duration::from_secs(20)).build().expect("an HTTP client") }
    }

    /// GETs `url`, with a token when the registry asks for one.
    async fn get(&self, url: &str, accept: &str, token: &mut Option<String>) -> Result<reqwest::Response, String> {
        for _ in 0..2 {
            let mut req = self.http.get(url).header(reqwest::header::ACCEPT, accept);
            if let Some(t) = token.as_ref() {
                req = req.bearer_auth(t);
            }
            let a = req.send().await.map_err(|e| format!("GET {url}: {e}"))?;
            if a.status() == reqwest::StatusCode::UNAUTHORIZED && token.is_none() {
                let challenge = a.headers().get(reqwest::header::WWW_AUTHENTICATE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                *token = Some(self.token(&challenge).await?);
                continue;
            }
            if !a.status().is_success() {
                let status = a.status();
                let body = a.text().await.unwrap_or_default();
                return Err(format!("GET {url}: {status}: {}", body.chars().take(300).collect::<String>()));
            }
            return Ok(a);
        }
        Err(format!("GET {url}: the registry refused its own token"))
    }

    /// The anonymous token a `Bearer realm="...",service="...",scope="..."`
    /// challenge leads to.
    async fn token(&self, challenge: &str) -> Result<String, String> {
        let params = bearer(challenge).ok_or_else(|| format!("the registry asks for a sign-in this operator does not have ({challenge:?}); make the image public"))?;
        let realm = params.iter().find(|(k, _)| k == "realm").map(|(_, v)| v.clone()).ok_or("the registry's challenge names no realm")?;
        let query: Vec<(String, String)> = params.into_iter().filter(|(k, _)| k == "service" || k == "scope").collect();
        let a = self.http.get(&realm).query(&query).send().await.map_err(|e| format!("GET {realm}: {e}"))?;
        if !a.status().is_success() {
            return Err(format!("GET {realm}: {}: the image may be private", a.status()));
        }
        let j: Json = a.json().await.map_err(|e| format!("{realm}: {e}"))?;
        j["token"].as_str().or_else(|| j["access_token"].as_str()).map(str::to_string).ok_or_else(|| format!("{realm} answered no token"))
    }

    /// The image `reference` names now (a tag, or a digest), with its
    /// labels' revision and creation time.
    pub async fn resolve(&self, reference: &str) -> Result<Image, String> {
        let r = Ref::parse(reference)?;
        let api = r.api();
        let at = r.digest.clone().or_else(|| r.tag.clone()).unwrap_or_else(|| "latest".into());
        let mut token = None;
        let a = self.get(&format!("{api}/manifests/{at}"), MANIFESTS, &mut token).await?;
        let header = a.headers().get("docker-content-digest").and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = a.bytes().await.map_err(|e| format!("{reference}: {e}"))?;
        let digest = header.unwrap_or_else(|| {
            use sha2::{Digest, Sha256};
            format!("sha256:{}", Sha256::digest(&body).iter().map(|b| format!("{b:02x}")).collect::<String>())
        });
        let mut manifest: Json = serde_json::from_slice(&body).map_err(|e| format!("{reference}: the manifest: {e}"))?;
        // An index of images, one an architecture: the labels are the same
        // on each, so read one (amd64's, when there is one).
        if let Some(ms) = manifest["manifests"].as_array() {
            let one = ms.iter().find(|m| m["platform"]["architecture"] == "amd64" && m["platform"]["os"] == "linux").or_else(|| ms.first());
            if let Some(d) = one.and_then(|m| m["digest"].as_str()) {
                let a = self.get(&format!("{api}/manifests/{d}"), MANIFESTS, &mut token).await?;
                manifest = a.json().await.map_err(|e| format!("{reference}: the manifest of {d}: {e}"))?;
            }
        }
        let (mut revision, mut created) = (None, None);
        if let Some(c) = manifest["config"]["digest"].as_str() {
            let a = self.get(&format!("{api}/blobs/{c}"), "application/json, */*", &mut token).await?;
            let config: Json = a.json().await.map_err(|e| format!("{reference}: the image's config: {e}"))?;
            revision = config["config"]["Labels"]["org.opencontainers.image.revision"].as_str().map(str::to_string);
            created = config["created"].as_str().map(str::to_string);
        }
        Ok(Image { image: r.pinned(&digest), digest, revision, created })
    }
}

// ----- core: the images a version picker offers

/// The repository a server offers versions of when nothing names another
/// (`--image-repository`). Every version is published under the old name
/// too for a release, ghcr.io/chloetech-ai/strata-server, which a cluster
/// whose image has that name keeps following.
pub const REPOSITORY: &str = "ghcr.io/chloetech-ai/chloedb-server";

impl Registry {
    /// The tags of `repository` (`registry/path`), every page of them.
    pub async fn tags(&self, repository: &str) -> Result<Vec<String>, String> {
        let r = Ref::parse(repository)?;
        let base = r.api();
        let mut token = None;
        let mut url = format!("{base}/tags/list?n=1000");
        let mut tags = Vec::new();
        for _ in 0..10 {
            let a = self.get(&url, "application/json", &mut token).await?;
            let next = a.headers().get(reqwest::header::LINK).and_then(|v| v.to_str().ok()).and_then(next_page);
            let j: Json = a.json().await.map_err(|e| format!("{repository}: the tags: {e}"))?;
            tags.extend(j["tags"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(str::to_string)));
            match next {
                Some(n) if n.starts_with('/') => {
                    let host = base.split("/v2/").next().unwrap_or_default();
                    url = format!("{host}{n}");
                }
                Some(n) => url = n,
                None => break,
            }
        }
        Ok(tags)
    }
}

/// The next page of a `Link: <...>; rel="next"` header.
fn next_page(link: &str) -> Option<String> {
    link.split(',').find(|l| l.contains("rel=\"next\"")).and_then(|l| Some(l.split_once('<')?.1.split_once('>')?.0.to_string()))
}

/// Whether `tag` is a version a person picks: `latest`, a version number
/// (`1.2.3`, `v1.2`, `1.2.3-rc.1`) or a commit (40 hex digits), not an
/// image of one architecture (`...-amd64`) or another name.
pub fn offered(tag: &str) -> bool {
    if tag == "latest" {
        return true;
    }
    if tag.len() == 40 && tag.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return true;
    }
    let v = tag.strip_prefix('v').unwrap_or(tag);
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    (1..=3).contains(&parts.len())
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && pre.is_none_or(|p| !p.is_empty() && !["amd64", "arm64", "arm", "386", "ppc64le", "s390x"].contains(&p) && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'))
}

/// One image of the picker: its tags (a commit's and `latest`, say), and
/// what the registry says of it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Offered {
    pub tags: Vec<String>,
    pub image: Image,
}

impl Offered {
    /// The tag that names it best: a version number, else a commit, else
    /// `latest`.
    pub fn tag(&self) -> Option<&str> {
        let rank = |t: &str| if t == "latest" { 2 } else if t.len() == 40 && !t.contains('.') { 1 } else { 0 };
        self.tags.iter().min_by_key(|t| rank(t)).map(String::as_str)
    }
}

/// The images of `tags` resolved, one each (tags of one digest together),
/// newest first: by `created`, then by tag; the ones without a time last.
pub fn newest_first(resolved: Vec<(String, Image)>) -> Vec<Offered> {
    let mut by: Vec<Offered> = Vec::new();
    for (tag, image) in resolved {
        match by.iter_mut().find(|o| o.image.digest == image.digest) {
            Some(o) => o.tags.push(tag),
            None => by.push(Offered { tags: vec![tag], image }),
        }
    }
    for o in &mut by {
        o.tags.sort();
    }
    by.sort_by(|a, b| b.image.created.cmp(&a.image.created).then_with(|| a.tag().cmp(&b.tag())));
    by
}

/// Images resolved, each kept 10 minutes (a commit's tag names one image
/// for good: kept a day).
pub struct Versions {
    resolved: tokio::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Image)>>,
    tags: tokio::sync::Mutex<Option<(std::time::Instant, String, Vec<String>)>>,
}

/// How long a tag's image is kept, and a commit tag's.
const KEEP: Duration = Duration::from_secs(600);
const KEEP_COMMIT: Duration = Duration::from_secs(86_400);

impl Versions {
    pub fn get() -> &'static Versions {
        static V: std::sync::OnceLock<Versions> = std::sync::OnceLock::new();
        V.get_or_init(|| Versions { resolved: Default::default(), tags: Default::default() })
    }

    /// The images of `repository` the picker offers ([`offered`]), newest
    /// first, resolved 8 at a time within 20 s: a tag not resolved in time
    /// is left out (and named in the error).
    pub async fn list(&self, registry: &Registry, repository: &str) -> Result<(Vec<Offered>, Option<String>), String> {
        let tags = {
            let mut held = self.tags.lock().await;
            match held.as_ref() {
                Some((at, repo, t)) if repo == repository && at.elapsed() < KEEP => t.clone(),
                _ => {
                    let t: Vec<String> = registry.tags(repository).await?.into_iter().filter(|t| offered(t)).collect();
                    *held = Some((std::time::Instant::now(), repository.to_string(), t.clone()));
                    t
                }
            }
        };
        use futures::StreamExt;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let results: Vec<(String, Result<Image, String>)> = futures::stream::iter(tags.into_iter().map(|tag| async move {
            let key = format!("{repository}:{tag}");
            let keep = if tag.len() == 40 { KEEP_COMMIT } else { KEEP };
            if let Some((at, i)) = self.resolved.lock().await.get(&key) {
                if at.elapsed() < keep {
                    return (tag, Ok(i.clone()));
                }
            }
            let r = match tokio::time::timeout_at(deadline, registry.resolve(&key)).await {
                Ok(r) => r,
                Err(_) => Err("not resolved within 20 s".into()),
            };
            if let Ok(i) = &r {
                self.resolved.lock().await.insert(key, (std::time::Instant::now(), i.clone()));
            }
            (tag, r)
        }))
        .buffer_unordered(8)
        .collect()
        .await;
        let mut failed = Vec::new();
        let mut ok = Vec::new();
        for (tag, r) in results {
            match r {
                Ok(i) => ok.push((tag, i)),
                Err(e) => failed.push(format!("{tag}: {e}")),
            }
        }
        let error = (!failed.is_empty()).then(|| format!("{} of the tags could not be resolved: {}", failed.len(), failed.into_iter().take(3).collect::<Vec<_>>().join("; ")));
        Ok((newest_first(ok), error))
    }
}

/// What `GET /v1/admin/kube/versions` answers: the images of
/// `repository`, the newest `limit`, each marked `current` (the image
/// `current` names, by digest), `newer` (built after it) and `tracked`
/// (the one the tag `track` names).
pub fn versions_json(repository: &str, track: &str, current: &Json, offered: &[Offered], error: Option<String>, limit: usize) -> Json {
    let digest = current["digest"].as_str();
    let now = offered.iter().find(|o| Some(o.image.digest.as_str()) == digest);
    let current_created = now.and_then(|o| o.image.created.clone()).or_else(|| current["created"].as_str().map(String::from));
    let mut cur = current.clone();
    if let (Some(o), true) = (now, cur.is_object()) {
        cur["revision"] = json!(o.image.revision);
        cur["created"] = json!(o.image.created);
        cur["tag"] = json!(o.tag());
    }
    let images: Vec<Json> = offered
        .iter()
        .take(limit)
        .map(|o| {
            let newer = match (&o.image.created, &current_created) {
                (Some(a), Some(b)) => a > b,
                _ => false,
            };
            json!({
                "tag": o.tag(), "tags": o.tags, "image": o.image.image, "digest": o.image.digest, "short": short(&o.image.digest),
                "revision": o.image.revision, "created": o.image.created,
                "current": Some(o.image.digest.as_str()) == digest, "newer": newer, "tracked": o.tags.iter().any(|t| t == track),
            })
        })
        .collect();
    let mut j = json!({"repository": repository, "track": track, "current": cur, "images": images, "total": offered.len()});
    if let Some(e) = error {
        j["error"] = json!(e);
    }
    j
}
// ----- end core

/// The parameters of a `Bearer` challenge.
fn bearer(challenge: &str) -> Option<Vec<(String, String)>> {
    let rest = challenge.trim().strip_prefix("Bearer ").or_else(|| challenge.trim().strip_prefix("bearer "))?;
    let mut out = Vec::new();
    let mut s = rest.trim();
    while !s.is_empty() {
        let (k, v) = s.split_once('=')?;
        let k = k.trim().trim_start_matches(',').trim().to_string();
        let v = v.trim_start();
        let (value, after) = match v.strip_prefix('"') {
            Some(q) => {
                let end = q.find('"')?;
                (q[..end].to_string(), &q[end + 1..])
            }
            None => {
                let end = v.find(',').unwrap_or(v.len());
                (v[..end].trim().to_string(), &v[end..])
            }
        };
        out.push((k, value));
        s = after.trim_start().trim_start_matches(',').trim_start();
    }
    Some(out)
}

/// What the operator found of new versions (`status.upgrade`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub mode: Mode,
    /// The tag followed.
    pub track: String,
    /// When the registry was last asked, and what it said.
    pub checked_ms: Option<u64>,
    pub error: Option<String>,
    /// The image the tag names.
    pub latest: Option<Image>,
    /// The image the cluster asks its servers to run, when it names one
    /// by digest, with what the registry says of it.
    pub current: Option<Image>,
    /// Each image the servers run (by the digest their pods pulled), and
    /// how many run it.
    pub running: Vec<Running>,
    /// The tag names another image than the cluster runs, not rejected.
    pub available: bool,
    /// Images rolled back after they failed a check: not suggested or
    /// rolled out again.
    pub rejected: Vec<Rejected>,
    /// The latest upgrades made or suggested, the newest last.
    pub log: Vec<super::autoscale::Entry>,
    /// The image the app engine's tag names (`spec.apps.image`, when it
    /// names no digest), asked of the registry as the servers' is, and
    /// that tag (`appsTrack`). With `on`, the engine runs it by its digest
    /// ([`apps_image`]), so a new image of the tag starts a new engine pod,
    /// as a new server image rolls the servers.
    pub apps: Option<Image>,
    pub apps_track: String,
    /// The engine's last image (a digest) that came up ready with `on`: what
    /// it goes back to when a newer one does not (X-E6).
    pub apps_serving: Option<String>,
    /// The engine's image it was moved to and has not come up ready on yet.
    pub apps_trying: Option<Trying>,
    /// The engine's images that did not come up ready in time: not run
    /// again by the upgrader (a person may still name one in
    /// `spec.apps.image`).
    pub apps_rejected: Vec<Rejected>,
}

/// An image being tried, and since when.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Trying {
    pub digest: String,
    pub since_ms: u64,
}

/// How long the app engine has to come up ready on a new image of its tag
/// before it goes back to the last one that served: the rollout's
/// `verifySeconds`, and at least five minutes, since a pod of one that
/// pulls a new image first and stops the old one (`Recreate`, for its
/// volume) takes longer than a server.
pub fn engine_verify_ms(verify_seconds: u64) -> u64 {
    verify_seconds.max(300) * 1000
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Running {
    pub digest: String,
    pub servers: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Rejected {
    pub digest: String,
    pub at_ms: u64,
    pub why: String,
}

const LOG: usize = 30;

impl State {
    pub fn note(&mut self, at_ms: u64, text: impl Into<String>) {
        self.log.push(super::autoscale::Entry { at_ms, text: text.into() });
        let over = self.log.len().saturating_sub(LOG);
        self.log.drain(..over);
    }

    /// Notes that `digest` failed a check and was rolled back.
    pub fn reject(&mut self, digest: &str, at_ms: u64, why: &str) {
        if !self.rejected.iter().any(|r| r.digest == digest) {
            self.rejected.push(Rejected { digest: digest.into(), at_ms, why: why.into() });
            let over = self.rejected.len().saturating_sub(20);
            self.rejected.drain(..over);
        }
        self.available = false;
        self.note(at_ms, format!("{} failed and was rolled back: {why}", short(digest)));
    }

    /// What the app engine's Deployment runs (`deployed`, the digest its
    /// template names) and whether it is ready on it, with `on` (X-E6). A
    /// digest that comes up ready is the one to go back to; one that is
    /// not ready `verify_ms` after the engine was moved to it is rejected,
    /// and [`apps_image`] then names the last one that served. An engine
    /// that served on a digest and stops being ready later is not an
    /// upgrade's fault, so it is left alone.
    pub fn engine_seen(&mut self, deployed: &str, ready: bool, now: u64, verify_ms: u64) {
        if ready {
            if self.apps_trying.as_ref().is_some_and(|t| t.digest == deployed) {
                self.note(now, format!("App engine: {} came up ready", short(deployed)));
            }
            self.apps_trying = None;
            self.apps_serving = Some(deployed.to_string());
            return;
        }
        if self.apps_serving.as_deref() == Some(deployed) || self.apps_rejected.iter().any(|r| r.digest == deployed) {
            self.apps_trying = None;
            return;
        }
        let since = match &self.apps_trying {
            Some(t) if t.digest == deployed => t.since_ms,
            _ => {
                self.apps_trying = Some(Trying { digest: deployed.to_string(), since_ms: now });
                now
            }
        };
        if now.saturating_sub(since) < verify_ms {
            return;
        }
        let why = format!("not ready {} s after the engine was moved to it", verify_ms / 1000);
        self.apps_rejected.push(Rejected { digest: deployed.to_string(), at_ms: now, why: why.clone() });
        let over = self.apps_rejected.len().saturating_sub(20);
        self.apps_rejected.drain(..over);
        self.apps_trying = None;
        let back = match &self.apps_serving {
            Some(d) => format!("back to {}", short(d)),
            None => "no earlier image served, so it stays; pin spec.apps.image to one that works".into(),
        };
        self.note(now, format!("App engine: {} was {why}; {back}", short(deployed)));
    }

    /// Whether the registry is due to be asked at `now`.
    pub fn due(&self, u: &Upgrade, track: &str, now: u64) -> bool {
        u.mode != Mode::Off && (self.track != track || self.checked_ms.is_none_or(|t| now.saturating_sub(t) >= u.check_minutes.saturating_mul(60_000)))
    }
}

/// The digest an image reference pins, if it does.
pub fn digest_of(image: &str) -> Option<String> {
    image.split_once('@').map(|(_, d)| d.to_string()).filter(|d| d.starts_with("sha256:"))
}

/// A digest as people read it: its first 12 hex digits.
pub fn short(digest: &str) -> String {
    digest.trim_start_matches("sha256:").chars().take(12).collect()
}

/// An image in words: its revision's first 7 digits and its digest's
/// first 12.
pub fn said(i: &Image) -> String {
    match &i.revision {
        Some(r) => format!("{} (commit {})", short(&i.digest), r.chars().take(7).collect::<String>()),
        None => short(&i.digest),
    }
}

/// The image to roll out, if the tag names one the cluster does not run:
/// with `on`, once the cluster is ready (a change under way, or a version
/// that failed, waits); never one rejected. `image` is the spec's,
/// `running` the digests its servers run.
pub fn decide(u: &Upgrade, s: &State, image: &str, ready: bool) -> Option<String> {
    let latest = s.latest.as_ref()?;
    if !s.available || u.mode != Mode::On || !ready || image == latest.image {
        return None;
    }
    Some(latest.image.clone())
}

/// Whether the tag's image is new to the cluster: not the one its spec
/// pins, not the only one its servers run, and not rejected.
pub fn available(s: &State, image: &str) -> bool {
    let Some(latest) = &s.latest else { return false };
    let pinned = digest_of(image);
    let runs_it = match &pinned {
        Some(d) => *d == latest.digest,
        // A tag: what its servers pulled.
        None => !s.running.is_empty() && s.running.iter().all(|r| r.digest == latest.digest),
    };
    !runs_it && !s.rejected.iter().any(|r| r.digest == latest.digest)
}

/// The image the app engine runs (`spec.apps.image` is `image`): with
/// `on`, the digest its tag named when the registry was last asked
/// ([`State::apps`]), so that a new image of the tag restarts the engine
/// on it, unless that digest did not come up ready ([`State::engine_seen`]),
/// when it is the last one that served; else (`off`, `suggest`, an image
/// by digest, a tag not asked yet) `image` as it is.
pub fn apps_image(u: &Upgrade, s: &State, image: &str) -> String {
    let Some(i) = s.apps.as_ref().filter(|_| u.mode == Mode::On && s.apps_track == image && digest_of(image).is_none()) else { return image.to_string() };
    let digest = match &s.apps_serving {
        Some(good) if s.apps_rejected.iter().any(|r| r.digest == i.digest) => good,
        _ => &i.digest,
    };
    match Ref::parse(image) {
        Ok(r) => r.pinned(digest),
        Err(_) => image.to_string(),
    }
}

/// The JSON Schema of `spec.upgrade`, for the resource definition.
pub fn schema() -> Json {
    json!({
        "type": "object",
        "description": "Following an image tag for new versions of ChloeDB: a new image is suggested (suggest) or rolled out (on), by its digest.",
        "properties": {
            "mode": {"type": "string", "enum": ["off", "suggest", "on"], "description": "off (default): no registry is asked; suggest: a new image waits in status.upgrade for a person; on: the operator rolls it out once the cluster is ready, and the app engine (spec.apps, at a tag) runs its tag's newest image too."},
            "track": {"type": "string", "description": "The image tag to follow (default: the image's repository at its tag, or at latest)."},
            "checkMinutes": {"type": "integer", "minimum": 1, "description": "Minutes between two looks at the registry (default 10)."},
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_read_as_registries_name_them() {
        let r = Ref::parse("ghcr.io/chloetech-ai/chloedb-server:latest").unwrap();
        assert_eq!((r.registry.as_str(), r.path.as_str(), r.tag.as_deref(), r.digest.as_deref()), ("ghcr.io", "chloetech-ai/chloedb-server", Some("latest"), None));
        assert_eq!(r.api(), "https://ghcr.io/v2/chloetech-ai/chloedb-server");
        let d = format!("sha256:{}", "a".repeat(64));
        let r = Ref::parse(&format!("ghcr.io/x/y@{d}")).unwrap();
        assert_eq!((r.tag.as_deref(), r.digest.as_deref(), r.pinned("sha256:b").as_str()), (None, Some(d.as_str()), "ghcr.io/x/y@sha256:b"));
        let r = Ref::parse("chloedb-server").unwrap();
        assert_eq!((r.registry.as_str(), r.path.as_str(), r.tag.as_deref()), ("docker.io", "library/chloedb-server", Some("latest")));
        assert_eq!(r.api(), "https://registry-1.docker.io/v2/library/chloedb-server");
        let r = Ref::parse("localhost:5000/s:v2").unwrap();
        assert_eq!((r.registry.as_str(), r.path.as_str(), r.tag.as_deref(), r.api().as_str()), ("localhost:5000", "s", Some("v2"), "http://localhost:5000/v2/s"));
        assert!(Ref::parse("x@sha256:short").is_err());
        assert!(Ref::parse("").is_err());
    }

    #[test]
    fn the_tag_followed_is_the_images_unless_named() {
        let u = Upgrade::default();
        assert_eq!(u.tracked("ghcr.io/chloetech-ai/chloedb-server:latest"), "ghcr.io/chloetech-ai/chloedb-server:latest");
        assert_eq!(u.tracked("ghcr.io/chloetech-ai/chloedb-server:v1"), "ghcr.io/chloetech-ai/chloedb-server:v1");
        assert_eq!(u.tracked(&format!("ghcr.io/chloetech-ai/chloedb-server@sha256:{}", "c".repeat(64))), "ghcr.io/chloetech-ai/chloedb-server:latest");
        // A cluster on the image's old name keeps following the old name.
        assert_eq!(u.tracked(&format!("ghcr.io/chloetech-ai/strata-server@sha256:{}", "c".repeat(64))), "ghcr.io/chloetech-ai/strata-server:latest");
        let u = Upgrade { track: Some("ghcr.io/x/y:main".into()), ..u };
        assert_eq!(u.tracked("anything:1"), "ghcr.io/x/y:main");
        assert!(Upgrade { mode: Mode::On, track: Some(format!("ghcr.io/x/y@sha256:{}", "c".repeat(64))), ..Upgrade::default() }.check("i").unwrap_err().contains("names a tag"));
    }

    #[test]
    fn a_bearer_challenge_is_read() {
        let p = bearer(r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:chloetech-ai/strata-server:pull""#).unwrap();
        assert_eq!(next_page(r#"</v2/x/tags/list?last=b&n=2>; rel="next""#).as_deref(), Some("/v2/x/tags/list?last=b&n=2"));
        assert_eq!(p, vec![("realm".into(), "https://ghcr.io/token".into()), ("service".into(), "ghcr.io".into()), ("scope".into(), "repository:chloetech-ai/strata-server:pull".into())]);
        assert!(bearer("Basic realm=x").is_none());
    }

    #[test]
    fn a_new_image_is_rolled_out_once_ready_and_never_one_rejected() {
        let d = |c: char| format!("sha256:{}", c.to_string().repeat(64));
        let latest = Image { image: format!("ghcr.io/x/y@{}", d('b')), digest: d('b'), revision: Some("abcdef0123".into()), created: None };
        let mut s = State { latest: Some(latest.clone()), ..State::default() };
        let on = Upgrade { mode: Mode::On, ..Upgrade::default() };
        // Pinned to another digest: the tag's is new.
        let pinned = format!("ghcr.io/x/y@{}", d('a'));
        s.available = available(&s, &pinned);
        assert!(s.available);
        assert_eq!(decide(&on, &s, &pinned, true), Some(latest.image.clone()));
        assert_eq!(decide(&on, &s, &pinned, false), None, "a change under way first");
        assert_eq!(decide(&Upgrade { mode: Mode::Suggest, ..on.clone() }, &s, &pinned, true), None, "a person applies a suggestion");
        // A tag: what the servers pulled.
        s.running = vec![Running { digest: d('b'), servers: 9 }];
        assert!(!available(&s, "ghcr.io/x/y:latest"));
        s.running.push(Running { digest: d('a'), servers: 1 });
        assert!(available(&s, "ghcr.io/x/y:latest"));
        // Rolled back once, never again.
        s.reject(&d('b'), 5, "c-1 cannot run the new settings");
        assert!(!s.available && !available(&s, &pinned));
        assert_eq!(decide(&on, &s, &pinned, true), None);
        assert!(s.log.last().unwrap().text.starts_with("bbbbbbbbbbbb failed and was rolled back"));
        assert_eq!(said(&latest), "bbbbbbbbbbbb (commit abcdef0)");
    }

    #[test]
    fn the_app_engine_runs_its_tags_image_by_digest_with_upgrades_on() {
        let d = format!("sha256:{}", "c".repeat(64));
        let tag = "ghcr.io/chloetech-ai/chloedb-apps:latest";
        let s = State { apps: Some(Image { image: format!("ghcr.io/chloetech-ai/chloedb-apps@{d}"), digest: d.clone(), revision: None, created: None }), apps_track: tag.into(), ..State::default() };
        let on = Upgrade { mode: Mode::On, ..Upgrade::default() };
        assert_eq!(apps_image(&on, &s, tag), format!("ghcr.io/chloetech-ai/chloedb-apps@{d}"));
        // Suggest or off: the tag as it is; another tag, or a digest: as given.
        assert_eq!(apps_image(&Upgrade { mode: Mode::Suggest, ..on.clone() }, &s, tag), tag);
        assert_eq!(apps_image(&Upgrade::default(), &s, tag), tag);
        assert_eq!(apps_image(&on, &s, "ghcr.io/chloetech-ai/chloedb-apps:0.3.0"), "ghcr.io/chloetech-ai/chloedb-apps:0.3.0");
        let pinned = format!("ghcr.io/chloetech-ai/chloedb-apps@sha256:{}", "a".repeat(64));
        assert_eq!(apps_image(&on, &State { apps_track: pinned.clone(), ..s.clone() }, &pinned), pinned);
        assert_eq!(apps_image(&on, &State::default(), tag), tag);
        // Read back from a status written before the field existed.
        let old: State = serde_json::from_value(json!({"mode": "on", "track": "x:latest"})).unwrap();
        assert!(old.apps.is_none() && old.apps_track.is_empty());
    }

    /// X-E6: a new image of the engine's tag that does not come up ready
    /// is rejected after the verify time, and the engine goes back to the
    /// last image that served.
    #[test]
    fn the_app_engine_goes_back_to_the_image_that_served() {
        let (good, bad) = (format!("sha256:{}", "1".repeat(64)), format!("sha256:{}", "2".repeat(64)));
        let tag = "ghcr.io/chloetech-ai/chloedb-apps:latest";
        let named = |d: &str| Image { image: format!("ghcr.io/chloetech-ai/chloedb-apps@{d}"), digest: d.to_string(), revision: None, created: None };
        let on = Upgrade { mode: Mode::On, ..Upgrade::default() };
        let mut s = State { apps: Some(named(&good)), apps_track: tag.into(), ..State::default() };
        let verify = engine_verify_ms(60);
        assert_eq!(verify, 300_000, "at least five minutes");
        s.engine_seen(&good, true, 0, verify);
        assert_eq!(s.apps_serving.as_deref(), Some(good.as_str()));
        // The tag moves on: the engine is moved to the new digest.
        s.apps = Some(named(&bad));
        assert_eq!(apps_image(&on, &s, tag), format!("ghcr.io/chloetech-ai/chloedb-apps@{bad}"));
        s.engine_seen(&bad, false, 1_000, verify);
        s.engine_seen(&bad, false, 200_000, verify);
        assert!(s.apps_rejected.is_empty(), "still within the verify time");
        assert_eq!(s.apps_trying.as_ref().map(|t| t.since_ms), Some(1_000));
        s.engine_seen(&bad, false, 301_000, verify);
        assert_eq!(s.apps_rejected.iter().map(|r| r.digest.as_str()).collect::<Vec<_>>(), [bad.as_str()]);
        assert!(s.log.last().unwrap().text.contains("back to 111111111111"), "{:?}", s.log.last());
        // Back to the image that served, while the tag still names the bad one.
        assert_eq!(apps_image(&on, &s, tag), format!("ghcr.io/chloetech-ai/chloedb-apps@{good}"));
        // Not ready later on the good one is not an upgrade's fault.
        s.engine_seen(&good, false, 900_000, verify);
        s.engine_seen(&good, false, 2_000_000, verify);
        assert_eq!(s.apps_rejected.len(), 1);
        // A newer image of the tag is tried again.
        let newer = format!("sha256:{}", "3".repeat(64));
        s.apps = Some(named(&newer));
        assert_eq!(apps_image(&on, &s, tag), format!("ghcr.io/chloetech-ai/chloedb-apps@{newer}"));
        s.engine_seen(&newer, true, 2_100_000, verify);
        assert_eq!(s.apps_serving.as_deref(), Some(newer.as_str()));
        // With nothing that served before, a rejected image stays (there is
        // nothing to go back to) and the log says to pin one.
        let mut fresh = State { apps: Some(named(&bad)), apps_track: tag.into(), ..State::default() };
        fresh.engine_seen(&bad, false, 0, verify);
        fresh.engine_seen(&bad, false, verify, verify);
        assert_eq!(apps_image(&on, &fresh, tag), format!("ghcr.io/chloetech-ai/chloedb-apps@{bad}"));
        assert!(fresh.log.last().unwrap().text.contains("pin spec.apps.image"));
    }

    #[test]
    fn the_registry_is_asked_every_few_minutes() {
        let u = Upgrade { mode: Mode::Suggest, check_minutes: 10, ..Upgrade::default() };
        let mut s = State::default();
        assert!(s.due(&u, "t", 0));
        s.track = "t".into();
        s.checked_ms = Some(1_000);
        assert!(!s.due(&u, "t", 1_000 + 599_999));
        assert!(s.due(&u, "t", 1_000 + 600_000));
        assert!(s.due(&u, "other", 2_000), "another tag at once");
        assert!(!s.due(&Upgrade::default(), "other", 2_000), "off");
    }

    #[test]
    fn the_picker_offers_versions_commits_and_latest_but_not_one_architecture() {
        let c = "0123456789abcdef0123456789abcdef01234567";
        for t in ["latest", "1.2.3", "v1.2", "v2", "1.4.0-rc.1", c] {
            assert!(offered(t), "{t}");
        }
        for t in [format!("{c}-amd64"), format!("{c}-arm64"), "1.2.3-amd64".into(), "main".into(), "sha-0123".into(), "1..2".into(), "0123456789ABCDEF0123456789ABCDEF01234567".into(), "".into()] {
            assert!(!offered(&t), "{t}");
        }
    }

    #[test]
    fn images_are_listed_newest_first_one_per_digest() {
        let img = |d: char, created: Option<&str>| Image { image: format!("r@sha256:{}", d.to_string().repeat(64)), digest: format!("sha256:{}", d.to_string().repeat(64)), revision: Some(d.to_string().repeat(40)), created: created.map(String::from) };
        let c1 = "1".repeat(40);
        let c2 = "2".repeat(40);
        let got = newest_first(vec![
            (c1.clone(), img('a', Some("2026-10-01T00:00:00Z"))),
            ("latest".into(), img('b', Some("2026-10-30T00:00:00Z"))),
            ("unknown".into(), img('c', None)),
            (c2.clone(), img('b', Some("2026-10-30T00:00:00Z"))),
        ]);
        assert_eq!(got.iter().map(|o| o.tag().unwrap()).collect::<Vec<_>>(), [c2.as_str(), c1.as_str(), "unknown"]);
        assert_eq!(got[0].tags, vec![c2.clone(), "latest".to_string()]);
        let current = json!({"image": "r@sha256:".to_string() + &"a".repeat(64), "digest": format!("sha256:{}", "a".repeat(64))});
        let j = versions_json("ghcr.io/x/y", "latest", &current, &got, None, 2);
        let images = j["images"].as_array().unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!((images[0]["newer"].as_bool(), images[0]["tracked"].as_bool(), images[0]["current"].as_bool()), (Some(true), Some(true), Some(false)));
        assert_eq!((images[1]["newer"].as_bool(), images[1]["current"].as_bool()), (Some(false), Some(true)));
        assert_eq!(j["current"]["tag"], json!(c1));
        assert_eq!(j["total"], 3);
    }

    /// The real registry (network): the repository's images, resolved.
    #[tokio::test]
    #[ignore = "asks ghcr.io"]
    async fn the_public_repository_lists_its_images() {
        let (list, error) = Versions::get().list(&Registry::new(), REPOSITORY).await.unwrap();
        assert!(error.is_none(), "{error:?}");
        assert!(list.iter().any(|o| o.tags.iter().any(|t| t == "latest")));
        assert!(list.iter().all(|o| o.image.digest.starts_with("sha256:") && o.tags.iter().all(|t| offered(t))));
        assert!(list.windows(2).all(|w| w[0].image.created >= w[1].image.created));
    }

}
