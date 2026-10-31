//! API keys with scopes, and the audit log of what they did.
//!
//! The server's `--token` is an operator's credential: every route and
//! every gRPC call. The servers of a cluster call each other with the
//! cluster's token (`--cluster-token`), which clients never hold: the
//! calls between servers (`Replicate`, `Restore`, `Verify`, `Slots`,
//! `Configure`, the slot moves, `Keys`) take only it, and the `--token`
//! may not make them. A server without a cluster token takes its
//! `--token` for both, as servers did before cluster tokens. An API key
//! has a scope, and clients get keys rather than a token:
//!
//! | scope | may |
//! |---|---|
//! | `read` | query (SELECT, explain, embed, term statistics), read schemas, health, metrics and the series |
//! | `write` | that, and write rows (`/v1/write`, `/v1/import`, `/v1/stream`, `INSERT`, `UPDATE`, `DELETE`, logged queries) |
//! | `admin` | that, and change the server: tables and rank profiles, synonyms, checkpoints, vacuum, promotion, keys, alerts, log levels; see the running queries, logs and the audit log, and cancel queries |
//!
//! No key, admin ones included, may call the internal gRPC methods: only
//! the root credential ([`Principal::root`]) may.
//!
//! A route or method says the least scope it needs; what a request then
//! does is checked again by the service ([`crate::service`]), the same
//! for HTTP and gRPC: a statement by its kind (a query read, `INSERT`,
//! `UPDATE` and `DELETE` write, `CREATE` and `DROP` of tables and rank
//! profiles admin), a logged query write (and admin when it creates its
//! log's table), a write plan write, an import that creates tables admin.
//! The audit log keeps the admin's statements and every refusal, over
//! both.
//!
//! A key is `sk_<id>_<secret>`; the server keeps its id, name, scope and
//! the SHA-256 of its secret (never the secret) in its keys file, and
//! compares in constant time. A server with no token, no cluster token
//! and no key is open: every request is the root's. Its first key, which
//! must be an admin key, closes it. Such a server keeps an admin key that
//! never expires: it refuses to make or revoke a key that would leave it
//! none, since once its last admin key expired no one could manage it.
//!
//! **Keys across a cluster.** The servers of a cluster hold the same keys
//! ([`crate::keysync`]): a coordinator merges its keys with every server
//! of every shard, and a primary with its replicas, every few seconds and
//! as soon as its keys change. A merge keeps every key of either side, by
//! id, and every revocation, which wins: a key revoked anywhere is revoked
//! wherever it is merged ([`KeySet`]). So a key made on any server
//! reaches the others, a revocation spreads, a coordinator that starts
//! again with its keys in memory takes them back from its shards, and a
//! replica promoted to primary holds its primary's keys. A server takes
//! keys from others only when callers must prove themselves to it (it has
//! a token or a cluster token): keys would close an open server to the
//! servers that gave them. For the same reason a server of a cluster with
//! neither refuses to make a key ([`Keys::create`]).

use std::collections::{BTreeMap, VecDeque};
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
    /// The root credential: may call the internal gRPC methods. The
    /// cluster token; the token of a server without one; anyone on an
    /// open server.
    pub root: bool,
}

impl Principal {
    pub fn open() -> Principal {
        Principal { name: "open".into(), key: None, scope: Scope::Admin, root: true }
    }

    /// The `--token`'s holder: the root on a server without a cluster
    /// token (`root`), else an admin.
    fn token(root: bool) -> Principal {
        Principal { name: "token".into(), key: None, scope: Scope::Admin, root }
    }

    /// Another server of the cluster, by the cluster's token.
    pub fn cluster() -> Principal {
        Principal { name: "cluster".into(), key: None, scope: Scope::Admin, root: true }
    }

    /// Whether this is anyone, on a server with no credential to check.
    pub fn is_open(&self) -> bool {
        self.key.is_none() && self.name == "open"
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

/// A revoked key's id, kept so that the revocation wins wherever keys are
/// merged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revoked {
    pub id: String,
    pub revoked_ms: u64,
}

/// Keys and the ids of revoked ones: what a keys file holds and the
/// `Keys` call carries, as JSON.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeySet {
    pub keys: Vec<Key>,
    #[serde(default)]
    pub revoked: Vec<Revoked>,
}

impl KeySet {
    /// A keys file or a call's JSON: a set, or the list of keys that keys
    /// files held before revocations were kept.
    pub fn from_json(b: &[u8]) -> Result<KeySet, String> {
        match serde_json::from_slice::<Json>(b).map_err(|e| e.to_string())? {
            Json::Array(keys) => Ok(KeySet { keys: serde_json::from_value(Json::Array(keys)).map_err(|e| e.to_string())?, revoked: Vec::new() }),
            j => serde_json::from_value(j).map_err(|e| e.to_string()),
        }
    }

    fn is_revoked(&self, id: &str) -> bool {
        self.revoked.iter().any(|r| r.id == id)
    }

    /// This set with `other` merged in, and whether its keys or
    /// revocations changed: every revocation of either, and every key of
    /// either, by id, but the revoked ones. Of two keys with one id and
    /// other secrets (two servers drew the same 64 random bits), the one
    /// with the smaller digest stays, so that every server keeps the same.
    /// The time a key was last used is the later of the two.
    pub fn merged(&self, other: &KeySet) -> (KeySet, bool) {
        let mut next = self.clone();
        let mut changed = false;
        for r in &other.revoked {
            if !next.is_revoked(&r.id) {
                next.revoked.push(r.clone());
                changed = true;
            }
        }
        let before = next.keys.len();
        let revoked = std::mem::take(&mut next.revoked);
        next.keys.retain(|k| !revoked.iter().any(|r| r.id == k.id));
        next.revoked = revoked;
        changed |= next.keys.len() != before;
        for k in &other.keys {
            if next.is_revoked(&k.id) {
                continue;
            }
            match next.keys.iter_mut().find(|x| x.id == k.id) {
                Some(x) if x.sha256 == k.sha256 => x.last_used_ms = x.last_used_ms.max(k.last_used_ms),
                Some(x) => {
                    if k.sha256 < x.sha256 {
                        *x = k.clone();
                        changed = true;
                    }
                }
                None => {
                    next.keys.push(k.clone());
                    changed = true;
                }
            }
        }
        (next, changed)
    }
}

/// How the last merge of keys with a server went ([`crate::keysync`]).
#[derive(Clone, Debug, Serialize)]
pub struct Peer {
    pub at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The server's keys, and the file that keeps them (none: in memory, lost
/// when the server stops, unless other servers of its cluster hold them).
pub struct Keys {
    set: Mutex<KeySet>,
    file: Option<PathBuf>,
    /// Woken when the keys change, for [`crate::keysync`] to pass them on.
    pub changed: tokio::sync::Notify,
    /// The servers keys are merged with, and how the last merge went.
    peers: Mutex<BTreeMap<String, Peer>>,
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
        Keys::of(KeySet::default(), None)
    }

    fn of(set: KeySet, file: Option<PathBuf>) -> Keys {
        Keys { set: Mutex::new(set), file, changed: tokio::sync::Notify::new(), peers: Mutex::new(BTreeMap::new()) }
    }

    /// The keys of `file`, which a new key is written to (created when the
    /// first key is).
    pub fn open(file: PathBuf) -> Result<Keys, String> {
        let set = match std::fs::read(&file) {
            Ok(b) => KeySet::from_json(&b).map_err(|e| format!("{}: {e}", file.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => KeySet::default(),
            Err(e) => return Err(format!("{}: {e}", file.display())),
        };
        Ok(Keys::of(set, Some(file)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, KeySet> {
        self.set.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn file(&self) -> Option<&std::path::Path> {
        self.file.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().keys.is_empty()
    }

    pub fn list(&self) -> Vec<Json> {
        self.lock().keys.iter().map(Key::json).collect()
    }

    /// The keys and revocations, to give another server.
    pub fn set(&self) -> KeySet {
        self.lock().clone()
    }

    /// Merges `other` into these keys ([`KeySet::merged`]), keeping the
    /// result in the file when it changed (and waking [`Self::changed`]).
    /// Whether it did.
    pub fn merge(&self, other: &KeySet) -> Result<bool, ApiError> {
        let mut set = self.lock();
        let (next, changed) = set.merged(other);
        if changed {
            self.save(&next)?;
        }
        *set = next;
        drop(set);
        if changed {
            self.changed.notify_one();
        }
        Ok(changed)
    }

    /// Records how the last round of merges went with each server
    /// ([`crate::keysync`]): the error, for one that failed.
    pub fn synced(&self, peers: Vec<(String, Option<String>)>) {
        let at_ms = now_ms();
        *self.peers.lock().unwrap_or_else(|e| e.into_inner()) = peers.into_iter().map(|(url, error)| (url, Peer { at_ms, error })).collect();
    }

    /// The servers keys were last merged with: `{url: {at_ms, error?}}`.
    pub fn peers(&self) -> Json {
        json!(*self.peers.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Writes the keys to the file, through a new file renamed over it,
    /// readable by its owner only.
    fn save(&self, set: &KeySet) -> Result<(), ApiError> {
        let Some(path) = &self.file else { return Ok(()) };
        let tmp = path.with_extension("tmp");
        let body = serde_json::to_vec_pretty(set).map_err(ApiError::internal)?;
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
    /// On a server without a token or a cluster token (`token` false), the
    /// first key must be an admin key, and the keys must keep one admin
    /// key that never expires ([`managed`]); such a server refuses a key
    /// while it serves in a cluster (`in_cluster`), since the key would
    /// close it to the servers that call it, which send no credential.
    pub fn create(&self, name: &str, scope: Scope, expires_ms: Option<u64>, token: bool, in_cluster: bool) -> Result<(Key, String), ApiError> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(ApiError::bad("a key's name has 1 to 64 characters"));
        }
        if !token && in_cluster {
            return Err(ApiError {
                status: 409,
                message: "this server serves in a cluster and has no token: a key would close it to the servers that call it. Give the cluster's servers a --cluster-token, or make the key on the coordinator".into(),
            });
        }
        let mut set = self.lock();
        if !token && set.keys.is_empty() && scope != Scope::Admin {
            return Err(ApiError::bad("this server has no token and no key: its first key turns on authentication, so it must be an admin key"));
        }
        if set.keys.iter().any(|k| k.name == name) {
            return Err(ApiError { status: 409, message: format!("a key named {name:?} exists") });
        }
        let id = hex(&random(8)?);
        let secret = hex(&random(24)?);
        let full = format!("sk_{id}_{secret}");
        let key = Key { id, name: name.to_string(), scope, created_ms: now_ms(), expires_ms, sha256: sha256(&secret), last_used_ms: 0 };
        let mut next = set.clone();
        next.keys.push(key.clone());
        if !token && !managed(&next.keys) {
            return Err(ApiError::bad(
                "this server has no token, so one of its admin keys must never expire: once the last one expired, no one could manage the server. Make an admin key without expires_days first",
            ));
        }
        self.save(&next)?;
        *set = next;
        drop(set);
        self.changed.notify_one();
        Ok((key, full))
    }

    /// Revokes key `id`; false if there is none. On a server without a
    /// token (`token` false), its last admin key that never expires is not
    /// revoked: that would open the server, or leave it to be locked out
    /// when the others expire.
    /// The revocation is kept, so that it wins wherever these keys are
    /// merged.
    pub fn revoke(&self, id: &str, token: bool) -> Result<bool, ApiError> {
        let mut set = self.lock();
        let mut next = set.clone();
        next.keys.retain(|k| k.id != id);
        if next.keys.len() == set.keys.len() {
            return Ok(false);
        }
        next.revoked.push(Revoked { id: id.to_string(), revoked_ms: now_ms() });
        if !token && !managed(&next.keys) {
            return Err(ApiError {
                status: 409,
                message: "this is the last admin key that never expires on a server without a token: revoking it would leave no one to manage the server once the others expire (or open it). Make another admin key without expires_days first".into(),
            });
        }
        self.save(&next)?;
        *set = next;
        drop(set);
        self.changed.notify_one();
        Ok(true)
    }

    /// Whether an admin key that never expires is among these keys: on a
    /// server without a token, the credential sure to manage it
    /// ([`managed`]).
    pub fn managed(&self) -> bool {
        managed(&self.lock().keys)
    }

    /// The key a bearer credential names, if it is one of these, not
    /// expired.
    fn find(&self, given: &str) -> Option<Principal> {
        let rest = given.strip_prefix("sk_")?;
        let (id, secret) = rest.split_once('_')?;
        let digest = sha256(secret);
        let mut set = self.lock();
        let k = set.keys.iter_mut().find(|k| k.id == id)?;
        if !same_secret(digest.as_bytes(), k.sha256.as_bytes()) || k.expires_ms.is_some_and(|e| now_ms() >= e) {
            return None;
        }
        // Kept in memory; written with the next change of keys.
        k.last_used_ms = now_ms();
        Some(Principal { name: k.name.clone(), key: Some(k.id.clone()), scope: k.scope, root: false })
    }

    /// Who an `Authorization` header value makes a request: another
    /// server of the cluster (by the `cluster` token), the `token`'s
    /// holder, a key's, or (with no token, no cluster token and no key)
    /// anyone. `None`: a wrong or missing credential.
    pub fn resolve(&self, token: Option<&Secret>, cluster: Option<&Secret>, authorization: Option<&str>) -> Option<Principal> {
        if token.is_none() && cluster.is_none() && self.is_empty() {
            return Some(Principal::open());
        }
        if cluster.is_some_and(|c| c.admits(authorization)) {
            return Some(Principal::cluster());
        }
        if token.is_some_and(|t| t.admits(authorization)) {
            return Some(Principal::token(cluster.is_none()));
        }
        let given = authorization.and_then(|v| v.strip_prefix("Bearer "))?;
        self.find(given)
    }
}

/// Whether `keys` hold an admin key that never expires.
fn managed(keys: &[Key]) -> bool {
    keys.iter().any(|k| k.scope == Scope::Admin && k.expires_ms.is_none())
}

tokio::task_local! {
    static NOTE: std::cell::RefCell<Option<String>>;
}

/// Runs `f`, a request, with room for what the audit log is to say of it
/// ([`note`]): its output, and that note.
pub async fn noting<F: std::future::Future>(f: F) -> (F::Output, Option<String>) {
    NOTE.scope(std::cell::RefCell::new(None), async move {
        let out = f.await;
        (out, NOTE.with(|n| n.borrow_mut().take()))
    })
    .await
}

/// Says what the audit log keeps of the request being served (a table's
/// statement, the tables an import creates), which audits it whatever its
/// route or method. Outside a request ([`noting`]), nothing.
pub fn note(detail: impl Into<String>) {
    let _ = NOTE.try_with(|n| *n.borrow_mut() = Some(detail.into()));
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
        assert_eq!(keys.resolve(None, None, None), Some(Principal::open()));
        assert!(keys.create("reader", Scope::Read, None, false, false).is_err());
        let (admin, admin_secret) = keys.create("ops", Scope::Admin, None, false, false).unwrap();
        let (reader, secret) = keys.create("reader", Scope::Read, None, false, false).unwrap();
        assert_eq!(keys.create("reader", Scope::Read, None, false, false).unwrap_err().status, 409);
        // Closed now.
        assert_eq!(keys.resolve(None, None, None), None);
        let p = keys.resolve(None, None, Some(&format!("Bearer {secret}"))).unwrap();
        assert_eq!((p.name.as_str(), p.scope, p.root), ("reader", Scope::Read, false));
        assert!(p.may(Scope::Read, "a query").is_ok());
        assert_eq!(p.may(Scope::Write, "a write").unwrap_err().status, 403);
        assert_eq!(keys.resolve(None, None, Some(&format!("Bearer {admin_secret}"))).unwrap().scope, Scope::Admin);
        // A wrong secret for a real id, and the token.
        assert_eq!(keys.resolve(None, None, Some(&format!("Bearer sk_{}_00", reader.id))), None);
        let token = Secret::new("t0k");
        assert!(keys.resolve(Some(&token), None, Some("Bearer t0k")).unwrap().root);
        // Kept in the file, without the secret.
        let text = std::fs::read_to_string(dir.path().join("keys.json")).unwrap();
        assert!(!text.contains(secret.rsplit_once('_').unwrap().1) && text.contains(&reader.sha256));
        let again = Keys::open(dir.path().join("keys.json")).unwrap();
        assert_eq!(again.resolve(None, None, Some(&format!("Bearer {secret}"))).unwrap().name, "reader");
        assert!(again.revoke(&reader.id, false).unwrap());
        assert!(!again.revoke(&reader.id, false).unwrap());
        // The last admin key stays, but for a server with a token.
        assert_eq!(again.revoke(&admin.id, false).unwrap_err().status, 409);
        assert_eq!(again.resolve(None, None, Some(&format!("Bearer {secret}"))), None);
        assert_eq!(again.list().len(), 1);
        assert_eq!(again.list()[0]["id"], admin.id);
        // Expired.
        let (_, old) = again.create("old", Scope::Read, Some(now_ms() - 1), false, false).unwrap();
        assert_eq!(again.resolve(None, None, Some(&format!("Bearer {old}"))), None);
    }

    /// A server without a token keeps an admin key that never expires: its
    /// last admin key expiring would lock everyone out (BUG-N5).
    #[test]
    fn a_server_without_a_token_keeps_an_admin_key_that_never_expires() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Keys::open(dir.path().join("keys.json")).unwrap();
        let soon = Some(now_ms() + 60_000);
        // Its first admin key may not expire...
        let e = keys.create("ops", Scope::Admin, soon, false, false).unwrap_err();
        assert!(e.status == 400 && e.message.contains("never expire"), "{}", e.message);
        assert!(keys.list().is_empty() && !dir.path().join("keys.json").exists());
        let (forever, _) = keys.create("ops", Scope::Admin, None, false, false).unwrap();
        assert!(keys.managed());
        // ...others may, once one does not.
        let (brief, _) = keys.create("ops-brief", Scope::Admin, soon, false, false).unwrap();
        keys.create("reader", Scope::Read, soon, false, false).unwrap();
        // The one that never expires is not revoked while it is the only
        // one; the expiring one is.
        let e = keys.revoke(&forever.id, false).unwrap_err();
        assert!(e.status == 409 && e.message.contains("never expires"), "{}", e.message);
        assert!(keys.revoke(&brief.id, false).unwrap());
        // With a token, the token manages the server: keys may all expire.
        let with = Keys::open(dir.path().join("with-token.json")).unwrap();
        let (only, _) = with.create("ops", Scope::Admin, soon, true, false).unwrap();
        assert!(with.revoke(&only.id, true).unwrap());
    }

    /// The cluster token is the root's, and the token then an admin's;
    /// without a cluster token, the token is the root's.
    #[test]
    fn a_cluster_token_is_the_root_and_the_token_an_admin() {
        let keys = Keys::memory();
        let (t, c) = (Secret::new("admin-t"), Secret::new("cluster-c"));
        let who = |token, cluster, auth: &str| keys.resolve(token, cluster, Some(auth));
        assert_eq!(who(Some(&t), Some(&c), "Bearer cluster-c"), Some(Principal::cluster()));
        let p = who(Some(&t), Some(&c), "Bearer admin-t").unwrap();
        assert!(p.scope == Scope::Admin && !p.root);
        assert!(who(Some(&t), None, "Bearer admin-t").unwrap().root);
        assert_eq!(who(None, Some(&c), "Bearer admin-t"), None);
        // A cluster token alone closes a server.
        assert_eq!(keys.resolve(None, Some(&c), None), None);
        assert!(Principal::open().is_open() && !Principal::cluster().is_open());
        // A server of a cluster with neither makes no key.
        let e = keys.create("ops", Scope::Admin, None, false, true).unwrap_err();
        assert!(e.status == 409 && e.message.contains("--cluster-token"), "{}", e.message);
        assert!(keys.create("ops", Scope::Admin, None, true, true).is_ok());
    }

    /// Merging keeps every key and every revocation, which wins; merging
    /// again changes nothing, and the order of merges does not matter.
    #[test]
    fn key_sets_merge_and_revocations_win() {
        let dir = tempfile::tempdir().unwrap();
        let a = Keys::open(dir.path().join("a.json")).unwrap();
        let b = Keys::memory();
        let (ka, sa) = a.create("from-a", Scope::Read, None, true, false).unwrap();
        let (kb, sb) = b.create("from-b", Scope::Write, None, true, false).unwrap();
        let (gone, sg) = b.create("gone", Scope::Read, None, true, false).unwrap();
        let admits = |k: &Keys, s: &str| k.resolve(None, None, Some(&format!("Bearer {s}"))).is_some();
        assert!(a.merge(&b.set()).unwrap());
        assert!(!a.merge(&b.set()).unwrap());
        assert!(b.merge(&a.set()).unwrap());
        for k in [&a, &b] {
            assert!(admits(k, &sa) && admits(k, &sb) && admits(k, &sg));
        }
        // Revoked on b; a, which still holds the key, does not bring it back.
        assert!(b.revoke(&gone.id, true).unwrap());
        assert!(!b.merge(&a.set()).unwrap());
        assert!(!admits(&b, &sg));
        assert!(a.merge(&b.set()).unwrap());
        assert!(!admits(&a, &sg) && admits(&a, &sa));
        assert_eq!(a.list().len(), 2);
        // Kept in a's file, the revocation too.
        let again = Keys::open(dir.path().join("a.json")).unwrap();
        assert_eq!(again.set().revoked, b.set().revoked);
        assert!(admits(&again, &sb) && !admits(&again, &sg));
        // A keys file from before revocations were kept: a list of keys.
        std::fs::write(dir.path().join("old.json"), serde_json::to_vec(&vec![ka]).unwrap()).unwrap();
        assert!(admits(&Keys::open(dir.path().join("old.json")).unwrap(), &sa));
        // One id with two secrets: every server keeps the same one.
        let other = Key { sha256: "0".repeat(64), ..kb.clone() };
        let x = KeySet { keys: vec![other], revoked: vec![] };
        let y = KeySet { keys: vec![kb], revoked: vec![] };
        assert_eq!(x.merged(&y).0.keys[0].sha256, "0".repeat(64));
        assert_eq!(y.merged(&x).0.keys[0].sha256, "0".repeat(64));
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
