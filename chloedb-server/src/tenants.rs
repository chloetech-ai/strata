//! Tenants (`--tenants URL`): databases kept whole in an object store
//! ([`chloedb::tenant`]), which this server opens when a request names
//! one. **Experimental**: a preview of the design in
//! `docs/design/0.7.0-file-per-agent.md`, whose layout may change without
//! a migration until that design is accepted.
//!
//! The server holds no tenant durably. Its copy of one, under
//! `--tenant-cache`, is a cache: any server pointed at the same store
//! answers for the same tenants, and of two that write one tenant at
//! once, the store refuses the later (HTTP 409, to be tried again).
//!
//! | route | does |
//! |---|---|
//! | `GET /v1/admin/tenants` | the store's tenants, and which are open here |
//! | `POST /v1/admin/tenants` | creates one: `{"id": "agent-7", "sql": ["CREATE TABLE ..."]}`, or from a template (`"template": "blackboard"`) |
//! | `GET /v1/admin/tenants/:id` | where it is: its epoch, base, logs and last transaction |
//! | `POST /v1/admin/tenants/:id/sql` | one statement: `{"sql": "...", "params": {}, "latest": false}` |
//! | `POST /v1/admin/tenants/:id/base` | makes a new base now (before leaving a tenant alone) |
//! | `DELETE /v1/admin/tenants/:id` | deletes it |
//! | `POST /v1/admin/tenants/:id/board/:tool`, `POST /v1/admin/tenants/:id/mcp?agent=NAME` | a blackboard's tools ([`crate::board`]) |
//!
//! All need an admin key: the routes are under `/v1/admin/` until
//! tenants have keys of their own, as the design proposes. A statement
//! that reads runs on the server's copy, which holds what this server
//! committed and what others had when it last looked; `"latest": true`
//! reads `HEAD` first (one round trip more) and sees every write
//! committed before the request.
//!
//! A tenant is opened by the first request that names it and closed
//! once no request has named it for `--tenant-idle-s` (or sooner, the
//! least recently used first, when more than `--tenant-open-max` are
//! open). Before it closes one it was writing, the server makes a new
//! base of it, so that whichever server opens it next reads two objects.
//! The copy in the cache is kept, and taken up again if the tenant comes
//! back to this server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use chloedb::db::{Db, SqlResult};
use chloedb::query::{bind_command, Command, Params};
use chloedb::storage::ObjectStore;
use chloedb::tenant::{Tenant, TenantError, TenantOptions};
use serde_json::{json, Value as Json};

use crate::ApiError;

/// When tenants are closed ([`Tenants::close_idle`]).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// A tenant no request has named for this long is closed.
    pub idle: Duration,
    /// More open than this, and the least recently used are closed.
    pub open_max: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { idle: Duration::from_secs(300), open_max: 1000 }
    }
}

/// A tenant asked for here.
#[derive(Default)]
struct Slot {
    /// `None` until it is open, and once it is closed. The lock is held
    /// while it opens, so that one tenant is opened once (two handles
    /// must not share a cache directory).
    tenant: Mutex<Option<Arc<Tenant>>>,
    /// When a request last named it.
    used: Mutex<Option<Instant>>,
}

/// The tenants of one store, and those open here.
pub struct Tenants {
    store: Arc<dyn ObjectStore>,
    cache: PathBuf,
    limits: Limits,
    open: Mutex<HashMap<String, Arc<Slot>>>,
}

impl std::fmt::Debug for Tenants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tenants").field("store", &self.store.describe()).field("cache", &self.cache).finish()
    }
}

impl From<TenantError> for ApiError {
    fn from(e: TenantError) -> ApiError {
        let status = match e {
            TenantError::Db(e) => return e.into(),
            TenantError::Missing => 404,
            // To be tried again: nothing was written.
            TenantError::Exists | TenantError::NotWriter => 409,
            TenantError::InDoubt(_) => 503,
        };
        ApiError { code: None, status, message: e.to_string() }
    }
}

/// Whether `sql` starts as a statement that reads does. Only a guess
/// ([`Kind::of`] asks the engine), for a statement the engine does not
/// bind.
fn starts_as_read(sql: &str) -> bool {
    let first = sql.trim_start().split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or_default().to_ascii_uppercase();
    matches!(first.as_str(), "SELECT" | "WITH" | "EXPLAIN" | "SHOW" | "DESCRIBE" | "VALUES")
}

/// What a statement does to a tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// It reads: a query or an EXPLAIN. Run by [`read_only`].
    Read,
    /// Anything else: run as a write, and committed to the store.
    Write,
}

impl Kind {
    /// What `sql` is, as the engine binds it on `db`, not by its words: a
    /// query that begins with a parenthesis or a comment reads (and does
    /// not make this server the tenant's writer), and nothing that writes
    /// is taken for a read by how it begins. A statement that does not
    /// bind here (it is wrong, or it names a table this copy has not
    /// seen yet) goes where it would report its error best: where reads
    /// run when it starts as one, else to the write, which looks at the
    /// store first.
    fn of(db: &Db, sql: &str, params: &Params) -> Kind {
        let catalog = db.catalog();
        let tables: Vec<&chloedb::schema::Schema> = catalog.iter().map(|t| &**t).collect();
        match bind_command(sql, &tables, params) {
            Ok(Command::Query | Command::Explain { .. } | Command::ShowTables) => Kind::Read,
            Ok(_) => Kind::Write,
            Err(_) if starts_as_read(sql) => Kind::Read,
            Err(_) => Kind::Write,
        }
    }
}

/// Runs a statement that reads, through what cannot write: a query runs
/// on a snapshot ([`Db::query`]), and an EXPLAIN shows a plan and runs
/// nothing. So a read never leaves in this server's copy what the store
/// does not have, whatever the statement turns out to say.
fn read_only(db: &Db, sql: &str, params: &Params) -> chloedb::Result<SqlResult> {
    let catalog = db.catalog();
    let tables: Vec<&chloedb::schema::Schema> = catalog.iter().map(|t| &**t).collect();
    match bind_command(sql, &tables, params)? {
        Command::Query => db.query(sql, params).map(SqlResult::Rows),
        Command::Explain { .. } | Command::ShowTables => db.sql(sql, params),
        _ => Err(chloedb::Error::Query("this statement writes: send it again".into())),
    }
}

impl Tenants {
    /// The tenants of the store `url` names (an `s3://bucket/prefix/` URL
    /// or a directory), cached under `cache`.
    pub fn open(url: &str, cache: PathBuf, limits: Limits) -> Result<Tenants, String> {
        let store = chloedb::storage::open_store(url).map_err(|e| format!("--tenants {url}: {e}"))?;
        // A store that does not refuse a put whose condition fails would
        // lose writes: it is probed, not believed.
        chloedb::tenant::check_store(&*store).map_err(|e| format!("--tenants: {e}"))?;
        std::fs::create_dir_all(&cache).map_err(|e| format!("--tenant-cache {}: {e}", cache.display()))?;
        Ok(Tenants { store, cache, limits, open: Mutex::default() })
    }

    fn options(&self, id: &str) -> TenantOptions {
        TenantOptions::new(self.cache.join(id))
    }

    fn slot(&self, id: &str) -> Arc<Slot> {
        let slot = self.open.lock().unwrap_or_else(|e| e.into_inner()).entry(id.to_string()).or_default().clone();
        *slot.used.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        slot
    }

    /// Closes the tenants no request has named for [`Limits::idle`],
    /// and the least recently used beyond [`Limits::open_max`]; how many.
    /// One a request is using is left for the next time. One this server
    /// was writing gets a new base first, when transactions follow its
    /// last: the next open, here or elsewhere, is then two reads.
    pub fn close_idle(&self) -> usize {
        let now = Instant::now();
        let mut open: Vec<(Duration, Arc<Slot>)> = {
            let slots = self.open.lock().unwrap_or_else(|e| e.into_inner());
            slots.values().filter(|s| s.tenant.try_lock().is_ok_and(|t| t.is_some())).map(|s| (s.used.lock().unwrap_or_else(|e| e.into_inner()).map_or(Duration::MAX, |u| now.duration_since(u)), s.clone())).collect()
        };
        // Longest unused first.
        open.sort_by(|a, b| b.0.cmp(&a.0));
        let over = open.len().saturating_sub(self.limits.open_max);
        let mut closed = 0;
        for (i, (unused, slot)) in open.iter().enumerate() {
            if *unused < self.limits.idle && i >= over {
                break;
            }
            let Ok(mut held) = slot.tenant.try_lock() else { continue };
            // In a request's hands: the slot's is not the only one.
            let Some(t) = held.take_if(|t| Arc::strong_count(t) == 1) else { continue };
            if t.is_writer().unwrap_or(false) && t.head().is_ok_and(|h| h.last_seq > h.base.seq) {
                if let Err(e) = t.make_base() {
                    tracing::warn!(tenant = t.id(), error = %e, "a tenant was closed without a new base");
                }
            }
            closed += 1;
        }
        drop(open);
        // Slots nobody holds and that hold nothing.
        self.open.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, s| Arc::strong_count(s) > 1 || s.tenant.try_lock().map_or(true, |t| t.is_some()));
        closed
    }

    /// Closes idle tenants from a thread of its own, until the last
    /// `Arc` of these tenants is dropped.
    pub fn spawn_closer(self: &Arc<Self>) {
        let tenants: Weak<Tenants> = Arc::downgrade(self);
        let every = (self.limits.idle / 4).clamp(Duration::from_millis(50), Duration::from_secs(15));
        let spawned = std::thread::Builder::new().name("chloedb-tenants".into()).spawn(move || loop {
            std::thread::sleep(every);
            let Some(t) = tenants.upgrade() else { return };
            t.close_idle();
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "idle tenants will not be closed: no thread for it");
        }
    }

    /// The ids of the tenants open here, in order.
    pub fn open_ids(&self) -> Vec<String> {
        let slots = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let mut ids: Vec<String> = slots.iter().filter(|(_, s)| s.tenant.try_lock().map_or(true, |t| t.is_some())).map(|(id, _)| id.clone()).collect();
        ids.sort();
        ids
    }

    /// The store's tenants, and those open here.
    pub fn list(&self) -> Result<Json, TenantError> {
        let tenants = chloedb::tenant::list(&*self.store)?;
        Ok(json!({"experimental": true, "store": self.store.describe(), "tenants": tenants, "open": self.open_ids(), "idle_s": self.limits.idle.as_secs_f64(), "open_max": self.limits.open_max}))
    }

    /// Tenant `id`, opened unless it is open.
    fn tenant(&self, id: &str) -> Result<Arc<Tenant>, TenantError> {
        if !chloedb::tenant::valid_id(id) {
            return Err(TenantError::Missing);
        }
        let slot = self.slot(id);
        let mut held = slot.tenant.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = &*held {
            return Ok(t.clone());
        }
        let t = Arc::new(Tenant::open(self.store.clone(), id, self.options(id))?);
        *held = Some(t.clone());
        Ok(t)
    }

    /// Closes this server's copy of `id` when `e` says the tenant is gone.
    fn forget_if_missing<T>(&self, id: &str, r: Result<T, TenantError>) -> Result<T, TenantError> {
        if matches!(r, Err(TenantError::Missing)) {
            self.open.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        }
        r
    }

    /// The templates a tenant can be made from.
    pub const TEMPLATES: [&'static str; 1] = [chloedb::blackboard::TEMPLATE];

    /// Tenant `id`, opened unless it is open, for a call of several steps
    /// ([`crate::board`]).
    pub fn get(&self, id: &str) -> Result<Arc<Tenant>, TenantError> {
        let r = self.tenant(id);
        self.forget_if_missing(id, r)
    }

    /// Creates tenant `id` with the tables of `template` (one of
    /// [`Tenants::TEMPLATES`]; any other name makes none) and runs
    /// `statements` on it, before it is first committed.
    pub fn create(&self, id: &str, statements: &[String], template: Option<&str>) -> Result<Json, TenantError> {
        let slot = self.slot(id);
        let mut held = slot.tenant.lock().unwrap_or_else(|e| e.into_inner());
        if held.is_some() {
            return Err(TenantError::Exists);
        }
        let t = Tenant::create(self.store.clone(), id, self.options(id), |db| {
            if template == Some(chloedb::blackboard::TEMPLATE) {
                chloedb::blackboard::init(db)?;
            }
            for s in statements {
                db.sql(s, &Params::new())?;
            }
            Ok(())
        })?;
        let described = describe(&t)?;
        *held = Some(Arc::new(t));
        Ok(described)
    }

    /// Where tenant `id` is.
    pub fn describe(&self, id: &str) -> Result<Json, TenantError> {
        let r = self.tenant(id).and_then(|t| {
            t.refresh()?;
            describe(&t)
        });
        self.forget_if_missing(id, r)
    }

    /// Runs one statement on tenant `id`: on this server's copy when it
    /// reads (after a look at the store when `latest`), committed to the
    /// store when it writes.
    pub fn sql(&self, id: &str, sql: &str, params: &Params, latest: bool) -> Result<Json, TenantError> {
        let t0 = Instant::now();
        let r = self.tenant(id).and_then(|t| {
            let read = |db: &Db| read_only(db, sql, params);
            let r = match (t.read(|db| Ok(Kind::of(db, sql, params)))?, latest) {
                (Kind::Read, true) => t.read_latest(read)?,
                (Kind::Read, false) => t.read(read)?,
                (Kind::Write, _) => t.write(|db| db.sql(sql, params))?,
            };
            let took_ms = t0.elapsed().as_secs_f64() * 1e3;
            Ok(match r {
                SqlResult::Rows(rs) => crate::json::result(&rs, took_ms),
                SqlResult::Written(n) => json!({"written": n, "seq": t.head()?.last_seq, "took_ms": took_ms}),
            })
        });
        self.forget_if_missing(id, r)
    }

    /// Makes a new base of tenant `id` now.
    pub fn make_base(&self, id: &str) -> Result<Json, TenantError> {
        let r = self.tenant(id).and_then(|t| {
            t.make_base()?;
            describe(&t)
        });
        self.forget_if_missing(id, r)
    }

    /// Deletes tenant `id`; whether there was one.
    pub fn delete(&self, id: &str) -> Result<bool, TenantError> {
        if !chloedb::tenant::valid_id(id) {
            return Ok(false);
        }
        let deleted = Tenant::delete(&*self.store, id)?;
        self.open.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        let _ = std::fs::remove_dir_all(self.cache.join(id));
        Ok(deleted)
    }
}

fn describe(t: &Tenant) -> Result<Json, TenantError> {
    let head = t.head()?;
    Ok(json!({
        "id": head.tenant,
        "experimental": true,
        "layout": head.layout,
        "incarnation": head.incarnation,
        "epoch": head.epoch,
        "writer_here": t.is_writer()?,
        "commit": head.commit,
        "last_seq": head.last_seq,
        "base": {"key": head.base.key, "seq": head.base.seq},
        "logs": head.logs.iter().map(|l| json!({"key": l.key, "first": l.first, "last": l.last, "bytes": l.bytes})).collect::<Vec<_>>(),
        "tail": {"transactions": head.tail.len(), "bytes": head.tail.iter().map(|e| e.bytes.len()).sum::<usize>()},
        "kept": head.kept.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_that_read() {
        let reads = starts_as_read;
        assert!(reads("select 1") && reads("  WITH a AS (SELECT 1) SELECT * FROM a") && reads("EXPLAIN SELECT 1") && reads("SELECT(1)"));
        assert!(!reads("INSERT INTO t VALUES (1)") && !reads("create table t (id INT)") && !reads("") && !reads("DELETE FROM t"));
    }
}
