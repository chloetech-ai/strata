//! A key and a token of its own for each app beside the cluster
//! (`spec.crawler`, `spec.sheets`, `spec.chloectx`, `spec.apps`, `spec.netops`), so that
//! none holds the cluster's admin token.
//!
//! For each app the operator keeps a Secret, `<cluster>-<app>-credentials`,
//! with two entries:
//!
//! - `key`: an API key of the cluster limited to the app's tables
//!   ([`crate::keys::Grant`]; `spec.<app>.tables`, else
//!   [`default_tables`]), which the app reads and writes the cluster with.
//!   The operator makes it through the coordinators with the admin token
//!   (`POST /v1/admin/keys`), named `<app>-app`, and makes it again (the
//!   old one revoked) when the app's tables change. It does so only once
//!   the coordinators say they make such keys (`/v1/whoami`'s
//!   `auth.app_keys`): a server from before limits would take a limited
//!   key's request as one for a key of every table.
//! - `token`: what the app's own API requires and the coordinators send
//!   it, random, made with the key. The coordinators read it from the
//!   Secret mounted as a folder (`CHLOEDB_APP_TOKEN_DIR`), a file per app,
//!   as they send it, so a Secret made after they started needs no
//!   restart.
//!
//! Until the Secret holds both, the app runs as it did before there were
//! such keys: with the admin token for both. The crawler that grows the
//! cluster (`crawler.grow`, the default) keeps the admin token as its key,
//! since growing is the server's admin (shards, rebalancing), not a
//! table's; its token is still its own.
//!
//! An app taken out of the spec loses its key (revoked) and its Secret.
//!
//! The operator revokes only keys it made: those whose owner is
//! [`OWNER`], as it makes them. A key someone else made, of an app's key
//! name or of the id its Secret names, is left as it is (audit N-E7).

use std::collections::BTreeMap;

use serde_json::{json, Value as Json};

use crate::keys::Grant;

/// The Secret's entries.
pub const KEY: &str = "key";
pub const TOKEN: &str = "token";
/// Annotations of the Secret: the key's id (to revoke it), and the hash
/// of the tables it was made for.
pub const KEY_ID: &str = "strata.chloetech.ai/key-id";
pub const TABLES: &str = "strata.chloetech.ai/tables";
/// Annotation of a Secret whose app has no key yet because making it
/// failed (audit X-E2): the app is given no key, rather than the admin
/// token, until a reconcile makes one.
pub const PENDING: &str = "strata.chloetech.ai/key-pending";
/// Where the coordinators find the apps' tokens, a file per app.
pub const TOKEN_DIR: &str = "/etc/strata/app-tokens";

/// The owner of every key the operator makes, which marks the keys it
/// revokes: no other (audit N-E7).
pub const OWNER: &str = "operator";

/// Whether key `k`, as `GET /v1/admin/keys` lists it, is live and one the
/// operator made.
fn own_live(k: &Json) -> bool {
    k["owner"] == OWNER && k["revoked_ms"].is_null()
}

/// Revokes key `id` when it is live and one the operator made; a key of
/// another owner is left, and logged. A key that is gone is no error.
pub async fn revoke_own(admin: &super::Admin, app: &str, id: &str) -> Result<(), String> {
    let listed = admin.call(reqwest::Method::GET, "/v1/admin/keys", None).await?;
    let Some(k) = listed["keys"].as_array().into_iter().flatten().find(|k| k["id"] == id) else { return Ok(()) };
    if !own_live(k) {
        if k["revoked_ms"].is_null() {
            tracing::warn!(app, key = %id, owner = %k["owner"], "an app's Secret names a key the operator did not make: it is not revoked");
        }
        return Ok(());
    }
    admin.call(reqwest::Method::DELETE, &format!("/v1/admin/keys/{id}"), None).await.map(drop).or_else(|e| if e.contains("404") { Ok(()) } else { Err(e) })
}

/// The tables an app's key may touch when its spec names none: its own.
pub fn default_tables(app: &str) -> &'static [&'static str] {
    match app {
        // Workbooks in sheets_books and sheets_cells; the cluster's tables
        // open in it as sheets (read).
        super::sheets::APP => &["sheets_*=admin", "*=read"],
        // Its spaces, and the memory `chloectx sync` pushes.
        super::chloectx::APP => &["ctx_*=admin", "chloectx_memory=read"],
        // The network, its telemetry, its incidents and its agents' runs.
        super::netops::APP => &["netops_*=admin"],
        // The engine's own tables and its apps' rows.
        super::apps::APP => &["sa_*=admin", "app_*=admin"],
        // A crawl fills the table it names (by default its first seed's
        // host): any table, but nothing of the server's.
        _ => &["*=admin"],
    }
}

/// The tables of app `app`'s key: `tables`, else its own.
pub fn tables_of(app: &str, tables: &[String]) -> Vec<String> {
    match tables.is_empty() {
        true => default_tables(app).iter().map(|t| t.to_string()).collect(),
        false => tables.to_vec(),
    }
}

/// What is wrong with `spec.<app>.tables`, if anything.
pub fn check_tables(app: &str, tables: &[String]) -> Result<(), String> {
    for t in tables {
        Grant::parse(t).map_err(|e| format!("{app}.tables: {}", e.message))?;
    }
    Ok(())
}

/// `spec.<app>.tables` in the resource's schema.
pub fn schema(app: &str) -> Json {
    json!({
        "type": "array", "items": {"type": "string"},
        "description": format!("The tables the app's own key may touch, as table=scope (a name, a prefix and *, or *; read, write or admin). Default: {}.", default_tables(app).join(", ")),
    })
}

/// The Secret of app `app` of cluster `cluster`.
pub fn secret_name(cluster: &str, app: &str) -> String {
    format!("{cluster}-{app}-credentials")
}

/// The name of app `app`'s key in the cluster.
pub fn key_name(app: &str) -> String {
    format!("{app}-app")
}

/// A short hash of the tables a key was made for.
pub fn tables_hash(tables: &[String]) -> String {
    super::short_hash(&json!(tables))
}

/// What a Secret holds, decoded.
pub fn entries(secret: &Json) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(m) = secret["data"].as_object() {
        for (k, v) in m {
            if let Some(b) = v.as_str().and_then(super::kube::base64_decode) {
                out.insert(k.clone(), String::from_utf8_lossy(&b).trim().to_string());
            }
        }
    }
    out
}

/// Whether a Secret holds a key and a token.
pub fn ready(secret: Option<&Json>) -> bool {
    secret.is_some_and(|s| {
        let e = entries(s);
        e.get(KEY).is_some_and(|k| !k.is_empty()) && e.get(TOKEN).is_some_and(|t| !t.is_empty())
    })
}

/// Whether an app runs with a key of its own when its Secret is ready: all
/// but the crawler that grows the cluster.
pub fn keyed(spec: &super::Spec, app: &str) -> bool {
    match app {
        super::crawler::APP => spec.crawler.as_ref().is_some_and(|c| !c.grow),
        _ => true,
    }
}

/// The tables of app `app` in `spec`.
pub fn spec_tables(spec: &super::Spec, app: &str) -> Vec<String> {
    let given: &[String] = match app {
        super::crawler::APP => spec.crawler.as_ref().map_or(&[], |c| &c.tables),
        super::sheets::APP => spec.sheets.as_ref().map_or(&[], |c| &c.tables),
        super::chloectx::APP => spec.chloectx.as_ref().map_or(&[], |c| &c.tables),
        super::apps::APP => spec.apps.as_ref().map_or(&[], |c| &c.tables),
        super::netops::APP => spec.netops.as_ref().map_or(&[], |c| &c.tables),
        _ => &[],
    };
    tables_of(app, given)
}

/// Why [`make_key`] made no key.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyError {
    /// The coordinators are from before keys limited to tables: the app
    /// runs with the admin token, as it did before such keys.
    BeforeLimits(String),
    /// Anything else (a timeout, a 5xx, coordinators rolling): the app
    /// keeps the key it has, and the next reconcile tries again.
    Failed(String),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::BeforeLimits(e) | KeyError::Failed(e) => f.write_str(e),
        }
    }
}

/// Makes app `app`'s key through the coordinators (`admin`), limited to
/// `tables`, in place of any key of its name the operator made (and of
/// `old`, by id, when the operator made it): its id and secret. A key of
/// that name someone else made is left (N-E7). Refuses (and revokes what
/// it made) when the answer does not show the key limited, as a server
/// from before limits would answer.
pub async fn make_key(admin: &super::Admin, app: &str, tables: &[String], old: Option<&str>) -> Result<(String, String), KeyError> {
    let name = key_name(app);
    let listed = admin.call(reqwest::Method::GET, "/v1/admin/keys", None).await.map_err(KeyError::Failed)?;
    let stale: Vec<String> = listed["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|k| own_live(k) && (k["name"] == name.as_str() || old.is_some_and(|o| k["id"] == o)))
        .filter_map(|k| k["id"].as_str().map(String::from))
        .collect();
    // The new key first: should making it fail, the app keeps the one it
    // has. The old ones are revoked once it exists; the caller writes it
    // to the Secret, whose key id on the pod template rolls the pod
    // (`kube::rolled`, audit N-D4).
    let made = admin
        .post("/v1/admin/keys", json!({"name": name, "tables": tables, "owner": OWNER, "note": format!("the key of the app {app} beside this cluster, made by the operator")}))
        .await
        .map_err(KeyError::Failed)?;
    let id = made["key"]["id"].as_str().ok_or_else(|| KeyError::Failed(format!("the coordinators made a key with no id: {made}")))?.to_string();
    if made["key"]["tables"].as_object().is_none_or(|t| t.is_empty()) {
        let _ = admin.call(reqwest::Method::DELETE, &format!("/v1/admin/keys/{id}"), None).await;
        return Err(KeyError::BeforeLimits("the coordinators made a key of every table for an app: they are from before keys limited to tables, so the app keeps the admin token".into()));
    }
    let secret = made["secret"].as_str().ok_or_else(|| KeyError::Failed("the coordinators' answer has no secret".into()))?.to_string();
    for old in stale.iter().filter(|s| **s != id) {
        // A key left over is named the app's and listed: the next new key
        // revokes it.
        if let Err(e) = admin.call(reqwest::Method::DELETE, &format!("/v1/admin/keys/{old}"), None).await {
            tracing::warn!(app, key = %old, error = %e, "could not revoke the app's old key");
        }
    }
    Ok((id, secret))
}

/// Whether the coordinators make keys limited to tables, as `/v1/whoami`
/// says.
pub async fn supported(admin: &super::Admin) -> bool {
    admin.call(reqwest::Method::GET, "/v1/whoami", None).await.is_ok_and(|j| j["auth"]["app_keys"] == true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apps_get_their_own_tables() {
        assert_eq!(tables_of("sheets", &[]), ["sheets_*=admin", "*=read"]);
        assert_eq!(tables_of("netops", &[]), ["netops_*=admin"]);
        assert_eq!(tables_of("chloectx", &["ctx_*=write".into()]), ["ctx_*=write"]);
        for app in ["crawler", "sheets", "chloectx", "apps", "netops"] {
            assert!(check_tables(app, &tables_of(app, &[])).is_ok(), "{app}");
        }
        assert!(check_tables("sheets", &["sheets_*=root".into()]).unwrap_err().contains("sheets.tables"));
        assert!(check_tables("sheets", &["a.b=read".into()]).is_err());
        assert_ne!(tables_hash(&["a=read".into()]), tables_hash(&["a=write".into()]));
        assert_eq!(secret_name("shop", "sheets"), "shop-sheets-credentials");
    }

    /// N-E7: making an app's key again revokes the keys of its name that
    /// the operator made, not one of that name an admin made, nor one of
    /// another owner its Secret names; and revoking the key of an app
    /// taken out of the spec leaves another owner's key too.
    #[tokio::test]
    async fn only_the_operators_own_keys_are_revoked() {
        use std::sync::{Arc, Mutex};
        let deleted: Arc<Mutex<Vec<String>>> = Arc::default();
        let d = deleted.clone();
        let app = axum::Router::new().fallback(move |method: axum::http::Method, uri: axum::http::Uri| {
            let d = d.clone();
            async move {
                let path = uri.path().to_string();
                match (method.as_str(), path.as_str()) {
                    ("GET", "/v1/admin/keys") => axum::Json(json!({"keys": [
                        {"id": "k0", "name": "sheets-app", "owner": "operator", "revoked_ms": null},
                        {"id": "k1", "name": "sheets-app", "owner": "operator", "revoked_ms": 5},
                        {"id": "ka", "name": "sheets-app", "owner": "alice", "revoked_ms": null},
                        {"id": "kb", "name": "reports", "owner": "alice", "revoked_ms": null},
                        {"id": "kc", "name": "sheets-app", "revoked_ms": null},
                    ]})),
                    ("POST", "/v1/admin/keys") => axum::Json(json!({"key": {"id": "k2", "tables": {"sheets_*": "admin"}}, "secret": "sk_k2_x"})),
                    ("DELETE", p) => {
                        d.lock().unwrap().push(p.trim_start_matches("/v1/admin/keys/").to_string());
                        axum::Json(json!({}))
                    }
                    _ => axum::Json(json!({})),
                }
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        let admin = super::super::Admin::new(&url, Some("admin".into()));
        let (id, secret) = make_key(&admin, "sheets", &tables_of("sheets", &[]), Some("kb")).await.unwrap();
        assert_eq!((id.as_str(), secret.as_str()), ("k2", "sk_k2_x"));
        assert_eq!(*deleted.lock().unwrap(), ["k0"], "the operator's own live key of the app's name, and no other");
        deleted.lock().unwrap().clear();
        for id in ["ka", "kb", "kc", "k1", "gone"] {
            revoke_own(&admin, "sheets", id).await.unwrap();
        }
        assert!(deleted.lock().unwrap().is_empty(), "{:?}", deleted.lock().unwrap());
        revoke_own(&admin, "sheets", "k0").await.unwrap();
        assert_eq!(*deleted.lock().unwrap(), ["k0"]);
    }

    #[test]
    fn a_secret_is_ready_with_a_key_and_a_token() {
        let b64 = |s: &str| super::super::kube::base64_encode(s.as_bytes());
        assert!(!ready(None));
        assert!(!ready(Some(&json!({"data": {"token": b64("t")}}))));
        let s = json!({"data": {"token": b64("t"), "key": b64("sk_1_2")}});
        assert!(ready(Some(&s)));
        assert_eq!(entries(&s)["key"], "sk_1_2");
    }
}
