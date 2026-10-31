//! API keys with scopes, and the audit log of what they did.
//!
//! The server's `--token` is its root credential: every route and every
//! gRPC call, the internal ones a coordinator and a primary make
//! (`Replicate`, `Restore`, `Verify`, `Slots`) included. An API key has a
//! scope, and clients get keys rather than the token:
//!
//! | scope | may |
//! |---|---|
//! | `read` | query (SELECT, explain, embed, term statistics), read schemas, health, metrics and the series |
//! | `write` | that, and write rows (`/v1/write`, `/v1/import`, `/v1/stream`, `INSERT`, `UPDATE`, `DELETE`, logged queries) |
//! | `admin` | that, and change the server: tables and rank profiles, synonyms, checkpoints, vacuum, promotion, keys, alerts, log levels; see the running queries, logs and the audit log, and cancel queries |
//!
//! No key, admin ones included, may call the internal gRPC methods: only
//! the token, which the servers of a cluster share, may.
//!
//! A key is `sk_<id>_<secret>`; the server keeps its id, name, scope and
//! the SHA-256 of its secret (never the secret) in its keys file, and
//! compares in constant time. A server with no token and no key is open:
//! every request is the root's. Its first key, which must be an admin
//! key, closes it.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};

use crate::auth::{same_secret, Secret};
use crate::telemetry::now_ms;
use crate::ApiError;

/// What a credential may do, each scope all that the one before may.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Read,
    Write,
    Admin,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s.to_ascii_lowercase().as_str() {
            "read" => Some(Scope::Read),
            "write" => Some(Scope::Write),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Write => "write",
            Scope::Admin => "admin",
        }
    }
}

/// Who made a request, as its credential says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// "token" (the root), "open" (no credential needed), or a key's name.
    pub name: String,
    /// The key's id, for a key.
    pub key: Option<String>,
    pub scope: Scope,
    /// The root credential (the token, or an open server): may call the
    /// internal gRPC methods.
    pub root: bool,
}

impl Principal {
    pub fn open() -> Principal {
        Principal { name: "open".into(), key: None, scope: Scope::Admin, root: true }
    }

    fn token() -> Principal {
        Principal { name: "token".into(), key: None, scope: Scope::Admin, root: true }
    }

    /// Refuses (403) what needs `need` and this credential may not do.
    pub fn may(&self, need: Scope, what: &str) -> Result<(), ApiError> {
        match self.scope >= need {
            true => Ok(()),
            false => Err(ApiError { status: 403, message: format!("{what} needs a key with the {} scope; {:?} has {}", need.name(), self.name, self.scope.name()) }),
        }
    }

    pub fn json(&self) -> Json {
        json!({"name": self.name, "key": self.key, "scope": self.scope.name(), "root": self.root})
    }
}

/// A key as the keys file keeps it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Key {
    pub id: String,
    pub name: String,
    pub scope: Scope,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<u64>,
    /// The SHA-256 of the secret, in hex.
    pub sha256: String,
    #[serde(default)]
    pub last_used_ms: u64,
}

impl Key {
    /// What the API shows of it.
    pub fn json(&self) -> Json {
        json!({"id": self.id, "name": self.name, "scope": self.scope.name(), "created_ms": self.created_ms, "expires_ms": self.expires_ms, "last_used_ms": self.last_used_ms, "prefix": format!("sk_{}_", self.id)})
    }
}

/// The server's keys, and the file that keeps them (none: in memory, lost
/// when the server stops).
pub struct Keys {
    keys: Mutex<Vec<Key>>,
    file: Option<PathBuf>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha256(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

/// `n` random bytes from the operating system.
fn random(n: usize) -> Result<Vec<u8>, ApiError> {
    use std::io::Read;
    let mut b = vec![0; n];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(|e| ApiError::internal(format!("no random bytes for a key: {e}")))?;
    Ok(b)
}

impl Keys {
    /// The keys file of a database: `<db>.keys.json`.
    pub fn file_of(db: &std::path::Path) -> PathBuf {
        let mut p = db.as_os_str().to_owned();
        p.push(".keys.json");
        p.into()
    }

    /// No keys, kept in memory.
    pub fn memory() -> Keys {
        Keys { keys: Mutex::new(Vec::new()), file: None }
    }

    /// The keys of `file`, which a new key is written to (created when the
    /// first key is).
    pub fn open(file: PathBuf) -> Result<Keys, String> {
        let keys = match std::fs::read(&file) {
            Ok(b) => serde_json::from_slice::<Vec<Key>>(&b).map_err(|e| format!("{}: {e}", file.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("{}: {e}", file.display())),
        };
        Ok(Keys { keys: Mutex::new(keys), file: Some(file) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Key>> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn file(&self) -> Option<&std::path::Path> {
        self.file.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    pub fn list(&self) -> Vec<Json> {
        self.lock().iter().map(Key::json).collect()
    }

    /// Writes the keys to the file, through a new file renamed over it,
    /// readable by its owner only.
    fn save(&self, keys: &[Key]) -> Result<(), ApiError> {
        let Some(path) = &self.file else { return Ok(()) };
        let tmp = path.with_extension("tmp");
        let body = serde_json::to_vec_pretty(keys).map_err(ApiError::internal)?;
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        };
        write().map_err(|e| ApiError::internal(format!("write {}: {e}", path.display())))
    }

    /// A new key; its secret, which is not kept, is in the answer only.
    /// The first key of an open server must be an admin key.
    pub fn create(&self, name: &str, scope: Scope, expires_ms: Option<u64>, open: bool) -> Result<(Key, String), ApiError> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(ApiError::bad("a key's name has 1 to 64 characters"));
        }
        let mut keys = self.lock();
        if open && keys.is_empty() && scope != Scope::Admin {
            return Err(ApiError::bad("this server has no token and no key: its first key turns on authentication, so it must be an admin key"));
        }
        if keys.iter().any(|k| k.name == name) {
            return Err(ApiError { status: 409, message: format!("a key named {name:?} exists") });
        }
        let id = hex(&random(4)?);
        let secret = hex(&random(24)?);
        let full = format!("sk_{id}_{secret}");
        let key = Key { id, name: name.to_string(), scope, created_ms: now_ms(), expires_ms, sha256: sha256(&secret), last_used_ms: 0 };
        let mut next = keys.clone();
        next.push(key.clone());
        self.save(&next)?;
        *keys = next;
        Ok((key, full))
    }

    /// Revokes key `id`; false if there is none. On a server without a
    /// token (`token` false), its last admin key is not revoked: that
    /// would open the server, or leave no one to manage it.
    pub fn revoke(&self, id: &str, token: bool) -> Result<bool, ApiError> {
        let mut keys = self.lock();
        let next: Vec<Key> = keys.iter().filter(|k| k.id != id).cloned().collect();
        if next.len() == keys.len() {
            return Ok(false);
        }
        if !token && !next.iter().any(|k| k.scope == Scope::Admin) {
            return Err(ApiError { status: 409, message: "this is the last admin key of a server without a token: revoking it would leave no one to manage the server (or open it). Make another admin key first".into() });
        }
        self.save(&next)?;
        *keys = next;
        Ok(true)
    }

    /// The key a bearer credential names, if it is one of these, not
    /// expired.
    fn find(&self, given: &str) -> Option<Principal> {
        let rest = given.strip_prefix("sk_")?;
        let (id, secret) = rest.split_once('_')?;
        let digest = sha256(secret);
        let mut keys = self.lock();
        let k = keys.iter_mut().find(|k| k.id == id)?;
        if !same_secret(digest.as_bytes(), k.sha256.as_bytes()) || k.expires_ms.is_some_and(|e| now_ms() >= e) {
            return None;
        }
        // Kept in memory; written with the next change of keys.
        k.last_used_ms = now_ms();
        Some(Principal { name: k.name.clone(), key: Some(k.id.clone()), scope: k.scope, root: false })
    }

    /// Who an `Authorization` header value makes a request: the token's
    /// holder, a key's, or (with no token and no key) anyone. `None`: a
    /// wrong or missing credential.
    pub fn resolve(&self, token: Option<&Secret>, authorization: Option<&str>) -> Option<Principal> {
        if token.is_none() && self.is_empty() {
            return Some(Principal::open());
        }
        if token.is_some_and(|t| t.admits(authorization)) {
            return Some(Principal::token());
        }
        let given = authorization.and_then(|v| v.strip_prefix("Bearer "))?;
        self.find(given)
    }
}

/// What the audit log keeps of a request.
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub ts_ms: u64,
    pub who: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub scope: Option<&'static str>,
    pub method: String,
    pub route: String,
    pub status: u16,
    pub ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The requests that changed something or were refused: the latest in
/// memory, and every one appended to a file of JSON lines if given.
pub struct Audit {
    latest: Mutex<VecDeque<Arc<Entry>>>,
    file: Option<Mutex<std::fs::File>>,
    path: Option<PathBuf>,
}

/// Entries kept in memory.
pub const AUDIT: usize = 2000;

impl Audit {
    pub fn new(path: Option<PathBuf>) -> Result<Audit, String> {
        let file = match &path {
            Some(p) => Some(Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(p).map_err(|e| format!("--audit-log {}: {e}", p.display()))?)),
            None => None,
        };
        Ok(Audit { latest: Mutex::new(VecDeque::with_capacity(AUDIT)), file, path })
    }

    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    pub fn add(&self, e: Entry) {
        if let Some(f) = &self.file {
            use std::io::Write;
            let mut f = f.lock().unwrap_or_else(|e| e.into_inner());
            if let Ok(line) = serde_json::to_string(&e) {
                if let Err(err) = writeln!(f, "{line}") {
                    tracing::error!(error = %err, "could not append to the audit log");
                }
            }
        }
        let mut l = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        if l.len() == AUDIT {
            l.pop_front();
        }
        l.push_back(Arc::new(e));
    }

    /// The latest entries, the newest first.
    pub fn latest(&self, limit: usize) -> Vec<Arc<Entry>> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().take(limit).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_resolve_to_their_scope_and_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path().join("keys.json")).unwrap();
        // Open: anyone; the first key of an open server is an admin key.
        assert_eq!(keys.resolve(None, None), Some(Principal::open()));
        assert!(keys.create("reader", Scope::Read, None, true).is_err());
        let (admin, admin_secret) = keys.create("ops", Scope::Admin, None, true).unwrap();
        let (reader, secret) = keys.create("reader", Scope::Read, None, true).unwrap();
        assert_eq!(keys.create("reader", Scope::Read, None, true).unwrap_err().status, 409);
        // Closed now.
        assert_eq!(keys.resolve(None, None), None);
        let p = keys.resolve(None, Some(&format!("Bearer {secret}"))).unwrap();
        assert_eq!((p.name.as_str(), p.scope, p.root), ("reader", Scope::Read, false));
        assert!(p.may(Scope::Read, "a query").is_ok());
        assert_eq!(p.may(Scope::Write, "a write").unwrap_err().status, 403);
        assert_eq!(keys.resolve(None, Some(&format!("Bearer {admin_secret}"))).unwrap().scope, Scope::Admin);
        // A wrong secret for a real id, and the token.
        assert_eq!(keys.resolve(None, Some(&format!("Bearer sk_{}_00", reader.id))), None);
        let token = Secret::new("t0k");
        assert!(keys.resolve(Some(&token), Some("Bearer t0k")).unwrap().root);
        // Kept in the file, without the secret.
        let text = std::fs::read_to_string(dir.path().join("keys.json")).unwrap();
        assert!(!text.contains(&secret[12..]) && text.contains(&reader.sha256));
        let again = Keys::open(dir.path().join("keys.json")).unwrap();
        assert_eq!(again.resolve(None, Some(&format!("Bearer {secret}"))).unwrap().name, "reader");
        assert!(again.revoke(&reader.id, false).unwrap());
        assert!(!again.revoke(&reader.id, false).unwrap());
        // The last admin key stays, but for a server with a token.
        assert_eq!(again.revoke(&admin.id, false).unwrap_err().status, 409);
        assert_eq!(again.resolve(None, Some(&format!("Bearer {secret}"))), None);
        assert_eq!(again.list().len(), 1);
        assert_eq!(again.list()[0]["id"], admin.id);
        // Expired.
        let (_, old) = again.create("old", Scope::Read, Some(now_ms() - 1), false).unwrap();
        assert_eq!(again.resolve(None, Some(&format!("Bearer {old}"))), None);
    }

    #[test]
    fn the_audit_log_appends_to_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = Audit::new(Some(dir.path().join("audit.jsonl"))).unwrap();
        let e = |status| Entry { ts_ms: 1, who: "ops".into(), key: None, scope: Some("admin"), method: "POST".into(), route: "/v1/admin/vacuum".into(), status, ms: 1.0, request_id: None, detail: None };
        a.add(e(200));
        a.add(e(403));
        assert_eq!(a.latest(10)[0].status, 403);
        let text = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 2);
    }
}
