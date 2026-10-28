//! The operations both front ends serve, over a local database or, as a
//! coordinator, over shard servers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata::db::{Db, SqlResult};
use strata::embed::Embedder;
use strata::ingest::{Format, Rows};
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::{bind, compound, Params};
use strata::schema::Schema;
use strata::storage::{CheckpointOptions, CheckpointStats, Layout, Snapshot, WriteTxn};
use strata::text::Synonyms;
use strata::value::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Snapshot leases held at once at most; a request for one more is
/// refused (429) until some expire.
pub const MAX_LEASES: usize = 10_000;

use crate::coord::Coordinator;
use crate::idem;
use crate::metrics::{self, Metrics};
use crate::{json as js, ApiError};

pub enum Backend {
    Local(Arc<Db>),
    Cluster(Arc<Coordinator>),
}

/// A SELECT to run.
#[derive(Clone, Default)]
pub struct QueryReq {
    pub sql: String,
    pub params: Params,
    /// 0: the server's default.
    pub timeout_ms: u64,
    /// Return results a coordinator can merge ([`ExecOptions::shard`]).
    pub shard: bool,
    /// Statistics summed over shards, for global BM25 scores.
    pub stats: Option<TermStats>,
    /// A snapshot lease; 0 for the latest version.
    pub snapshot: u64,
    /// From a coordinator: answer without shards that cannot answer
    /// (`None`: its default, [`crate::coord::CoordOptions::partial`]).
    pub partial: Option<bool>,
}

pub struct Answer {
    pub rs: ResultSet,
    pub took_ms: f64,
    pub kind: &'static str,
    /// Shards a coordinator answered without (see [`QueryReq::partial`]).
    pub missing: Vec<String>,
}

/// An embedded VECTOR field ([`strata::Embed`]) and its model, which
/// computes the vectors writes leave out and embeds `{"embed": "text"}`
/// parameters.
pub struct FieldModel {
    pub field: String,
    pub spec: strata::Embed,
    /// The model, or why it did not open (then writes must supply the
    /// vectors, and text cannot be searched against the field).
    pub model: Result<Arc<dyn Embedder>, String>,
}

/// The agent's model settings ([`crate::agent`]).
#[derive(Clone)]
pub struct AgentConfig {
    pub api_key: Option<String>,
    pub model: String,
    pub base_url: String,
}

pub struct Service {
    pub backend: Backend,
    pub schema: Schema,
    pub metrics: Metrics,
    pub agent: AgentConfig,
    /// The embedded VECTOR fields' models ([`Service::open_models`]).
    pub models: Vec<FieldModel>,
    /// Default query deadline.
    pub timeout: Duration,
    /// The longest deadline a request may ask for (`timeout_ms`).
    pub max_timeout: Duration,
    pub lease_ttl: Duration,
    leases: Mutex<HashMap<u64, (Snapshot, Instant)>>,
    next_lease: AtomicU64,
    /// Queries allowed to run at once; the rest wait until their deadline.
    admission: Arc<Semaphore>,
    cores: usize,
    /// Queries running, counted until their work ends ([`Running`]).
    running: Arc<AtomicUsize>,
    /// The queue of a local database's writer thread ([`writer`]).
    writes: Option<mpsc::Sender<Queued>>,
    writer_state: Arc<WriterState>,
    /// When a failed write was last logged, and how many failed since.
    write_log: Mutex<(Option<Instant>, u64)>,
    /// Idempotency keys of the writes it applied ([`crate::idem`]).
    idempotency: Arc<idem::Keys>,
    /// The synonym list `synonyms => 1` searches use, and its file.
    synonyms: RwLock<Option<Arc<Synonyms>>>,
    synonyms_file: Option<PathBuf>,
    /// Where `POST /v1/admin/checkpoint` writes checkpoints.
    backup_dir: Option<PathBuf>,
}

impl Service {
    pub fn new(backend: Backend, schema: Schema, agent: AgentConfig, max_concurrent: usize, timeout: Duration) -> Service {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let idempotency = Arc::new(idem::Keys::default());
        let writer_state = Arc::new(WriterState { alive: AtomicBool::new(true), panics: AtomicU64::new(0) });
        let writes = match &backend {
            Backend::Local(db) => {
                let (tx, rx) = mpsc::channel();
                let (db, state) = (db.clone(), writer_state.clone());
                std::thread::Builder::new().name("strata-writer".into()).spawn(move || writer(&db, rx, &state)).expect("spawn the writer thread");
                Some(tx)
            }
            Backend::Cluster(_) => None,
        };
        Service {
            backend,
            schema,
            metrics: Metrics::default(),
            agent,
            models: Vec::new(),
            timeout,
            max_timeout: timeout.max(Duration::from_secs(60)),
            lease_ttl: Duration::from_secs(60),
            leases: Mutex::new(HashMap::new()),
            next_lease: AtomicU64::new(1),
            admission: Arc::new(Semaphore::new(max_concurrent.max(1))),
            cores,
            running: Arc::new(AtomicUsize::new(0)),
            writes,
            writer_state,
            write_log: Mutex::new((None, 0)),
            idempotency,
            synonyms: RwLock::new(None),
            synonyms_file: None,
            backup_dir: None,
        }
    }

    /// Opens the model of each embedded VECTOR field from `dir`
    /// ([`strata::embed::open`]); a local database computes the vectors
    /// its writes leave out with them.
    pub fn open_models(&mut self, dir: &std::path::Path) {
        self.models = self
            .schema
            .fields
            .iter()
            .filter_map(|f| {
                let spec = f.embed.clone()?;
                let model = strata::embed::open(&spec.model, dir).and_then(|m| match &self.backend {
                    Backend::Local(db) => db.set_embedder(&f.name, m.clone()).map(|_| m),
                    Backend::Cluster(_) => Ok(m),
                });
                Some(FieldModel { field: f.name.clone(), spec, model: model.map_err(|e| e.to_string()) })
            })
            .collect();
    }

    /// The model of embedded field `field`, or of the first embedded field
    /// that has one.
    pub fn model(&self, field: Option<&str>) -> Result<(&str, &Arc<dyn Embedder>), ApiError> {
        let m = match field {
            Some(f) => self.models.iter().find(|m| m.field.eq_ignore_ascii_case(f)).ok_or_else(|| ApiError::bad(format!("{f} is not an embedded VECTOR field; pass a vector")))?,
            None => self.models.iter().find(|m| m.model.is_ok()).ok_or_else(|| ApiError::bad("this table has no field this server can embed text for; pass a vector"))?,
        };
        match &m.model {
            Ok(e) => Ok((&m.field, e)),
            Err(why) => Err(ApiError::bad(format!("{}'s model {} is not available here ({why}); pass a vector", m.field, m.spec.model))),
        }
    }

    /// Computes the embedded vectors that the documents of `ops` leave out
    /// before they queue for the writer, so that models run on many cores
    /// rather than in the writer thread.
    fn embed_ops(&self, db: &Db, ops: &mut [Json]) -> Result<(), ApiError> {
        let wanted: Vec<usize> = self.models.iter().filter(|m| m.model.is_ok()).filter_map(|m| self.schema.field_index(&m.field)).collect();
        if wanted.is_empty() {
            return Ok(());
        }
        let writes_doc = |op: &Json| matches!(op.get("op").and_then(|o| o.as_str()), Some("insert" | "upsert" | "upsert_if_newer"));
        let mut at = Vec::new();
        let mut docs = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            if writes_doc(op) && op.get("doc").is_some_and(|d| d.is_object()) {
                docs.push(js::document(&self.schema, &op["doc"])?);
                at.push(i);
            }
        }
        let before: Vec<Vec<bool>> = docs.iter().map(|d| wanted.iter().map(|&f| matches!(d[f], Value::Null)).collect()).collect();
        db.embed_missing(&mut docs);
        for ((i, d), missing) in at.into_iter().zip(&docs).zip(before) {
            for (&f, was_missing) in wanted.iter().zip(missing) {
                if let (true, Value::Vector(v)) = (was_missing, &d[f]) {
                    ops[i]["doc"][self.schema.fields[f].name.as_str()] = json!(v.as_ref());
                }
            }
        }
        Ok(())
    }

    /// Reads the synonym list at `path` if it exists; `PUT /v1/synonyms`
    /// writes it there.
    pub fn open_synonyms(&mut self, path: &Path) -> Result<(), ApiError> {
        if let Backend::Cluster(_) = self.backend {
            return Err(ApiError::bad("--synonyms goes to each shard server, which expands its own searches"));
        }
        self.synonyms_file = Some(path.to_path_buf());
        match std::fs::read_to_string(path) {
            Ok(text) => self.set_synonyms_text(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ApiError::bad(format!("read {}: {e}", path.display()))),
        }
    }

    /// The synonym list, if one is loaded.
    pub fn synonyms(&self) -> Option<Arc<Synonyms>> {
        self.synonyms.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_synonyms_text(&self, text: &str) -> Result<(), ApiError> {
        let s = Synonyms::parse(text)?;
        *self.synonyms.write().unwrap_or_else(|e| e.into_inner()) = (!s.is_empty() || !text.trim().is_empty()).then(|| Arc::new(s));
        Ok(())
    }

    /// Replaces the synonym list (and its file, if the server has one).
    pub fn put_synonyms(&self, text: &str) -> Result<usize, ApiError> {
        if let Backend::Cluster(_) = self.backend {
            return Err(ApiError::bad("a coordinator has no synonym list: each shard server takes --synonyms FILE"));
        }
        let parsed = Synonyms::parse(text)?;
        if let Some(path) = &self.synonyms_file {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path)).map_err(|e| ApiError::internal(format!("write {}: {e}", path.display())))?;
        }
        let rules = parsed.rules();
        *self.synonyms.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(parsed));
        Ok(rules)
    }

    /// Where the synonym list is saved, if anywhere.
    pub fn synonyms_file(&self) -> Option<&Path> {
        self.synonyms_file.as_deref()
    }

    /// Sets where `POST /v1/admin/checkpoint` writes checkpoints, creating
    /// the directory if need be.
    pub fn set_backup_dir(&mut self, dir: &Path) -> Result<(), ApiError> {
        if let Backend::Cluster(_) = self.backend {
            return Err(ApiError::bad("--backup-dir goes to each shard server, which checkpoints its own database"));
        }
        std::fs::create_dir_all(dir).map_err(|e| ApiError::bad(format!("create {}: {e}", dir.display())))?;
        self.backup_dir = Some(dir.to_path_buf());
        Ok(())
    }

    /// Writes a checkpoint of the database ([`strata::storage::Database::checkpoint`])
    /// named `name` in the backup directory, hard-linking the segments an
    /// earlier checkpoint there (`link_from`) already holds.
    pub async fn checkpoint(&self, name: &str, layout: Layout, link_from: Option<&str>) -> Result<CheckpointStats, ApiError> {
        let dir = self.backup_dir.as_ref().ok_or_else(|| ApiError::bad("checkpoints need the server started with --backup-dir"))?;
        let db = self.admin_db()?;
        let dest = dir.join(checkpoint_name(name)?);
        let link_from = link_from.map(|n| checkpoint_name(n).map(|n| dir.join(n))).transpose()?;
        let opts = CheckpointOptions { layout, link_from };
        tokio::task::spawn_blocking(move || db.storage().checkpoint(&dest, &opts)).await.map_err(ApiError::internal)?.map_err(Into::into)
    }

    /// Checks every byte of the database against its checksums
    /// ([`strata::storage::Database::check`]). Writes wait meanwhile.
    pub async fn check(&self) -> Result<(), ApiError> {
        let db = self.admin_db()?;
        let deadline = Instant::now() + self.max_timeout;
        tokio::task::spawn_blocking(move || wait_for_writer(deadline, || Ok(db.storage().check()?))).await.map_err(ApiError::internal)?
    }

    /// Returns free space to the file system ([`strata::storage::Database::vacuum`]);
    /// the bytes the file shrank by.
    pub async fn vacuum(&self) -> Result<u64, ApiError> {
        let db = self.admin_db()?;
        let deadline = Instant::now() + self.max_timeout;
        tokio::task::spawn_blocking(move || wait_for_writer(deadline, || Ok(db.storage().vacuum()?))).await.map_err(ApiError::internal)?
    }

    fn admin_db(&self) -> Result<Arc<Db>, ApiError> {
        self.local_db().cloned().ok_or_else(|| ApiError::bad("a coordinator has no database of its own: ask each shard server"))
    }

    /// Hands a write to the writer thread and waits for its commit. The
    /// writer records `key`'s outcome, even if this future is dropped.
    async fn write(&self, job: WriteJob, deadline: Instant, key: Option<String>) -> Result<u64, ApiError> {
        let tx = self.writes.as_ref().ok_or_else(|| ApiError::internal("no writer for a coordinator"))?;
        let (reply, rx) = tokio::sync::oneshot::channel();
        let keys = self.idempotency.clone();
        tx.send(Queued { job, deadline, reply: Some(reply), key, keys }).map_err(|_| ApiError::internal("the writer thread stopped"))?;
        rx.await.map_err(|_| ApiError::internal("the writer thread stopped"))?
    }

    /// The database, unless this is a coordinator.
    pub fn local_db(&self) -> Option<&Arc<Db>> {
        self.local()
    }

    fn local(&self) -> Option<&Arc<Db>> {
        match &self.backend {
            Backend::Local(db) => Some(db),
            Backend::Cluster(_) => None,
        }
    }

    /// Pins the current version for `lease_ttl`; returns the lease.
    pub fn lease(&self) -> Result<(u64, u64), ApiError> {
        self.lease_for(self.lease_ttl)
    }

    /// Pins the current version for `ttl`; returns the lease. At most
    /// [`MAX_LEASES`] are held at once.
    fn lease_for(&self, ttl: Duration) -> Result<(u64, u64), ApiError> {
        let db = self.local().ok_or_else(|| ApiError::bad("a coordinator holds no snapshots"))?;
        let snap = db.snapshot();
        let txn = snap.txn();
        let mut l = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        l.retain(|_, (_, t)| *t > now);
        if l.len() >= MAX_LEASES {
            return Err(ApiError { status: 429, message: format!("{MAX_LEASES} snapshot leases are held; try again when some expire") });
        }
        let id = self.next_lease.fetch_add(1, Ordering::Relaxed);
        l.insert(id, (snap, now + ttl));
        Ok((id, txn))
    }

    /// Releases expired leases, and the versions only they held; returns
    /// how many are held still. The maintenance loop calls it, so that a
    /// version is not kept until the next lease is made.
    pub fn sweep_leases(&self) -> usize {
        let mut l = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        l.retain(|_, (_, t)| *t > now);
        l.len()
    }

    /// A request's deadline: its `timeout_ms` up to [`Self::max_timeout`],
    /// or the default.
    pub fn timeout_of(&self, timeout_ms: u64) -> Duration {
        if timeout_ms > 0 {
            Duration::from_millis(timeout_ms).min(self.max_timeout)
        } else {
            self.timeout
        }
    }

    fn snapshot(&self, db: &Db, lease: u64) -> Result<Snapshot, ApiError> {
        if lease == 0 {
            return Ok(db.snapshot());
        }
        let l = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        match l.get(&lease) {
            Some((s, t)) if *t > Instant::now() => Ok(s.clone()),
            _ => Err(ApiError::bad(format!("snapshot lease {lease} is unknown or expired"))),
        }
    }

    /// Parameters from JSON. Besides plain values (see [`js::param`]), a
    /// parameter may be `{"vector_of": key, "field": "emb"}`, the vector of
    /// a stored document (field: the first VECTOR field if left out), or
    /// `{"embed": "text", "field": "emb"}`, the vector of the text by the
    /// model of that embedded field (the first one with a model if left
    /// out), so that the text and the rows are embedded alike.
    pub async fn params(self: &Arc<Self>, j: Option<&Json>) -> Result<Params, ApiError> {
        let mut p = Params::new();
        let Some(j) = j.filter(|j| !j.is_null()) else { return Ok(p) };
        let m = j.as_object().ok_or_else(|| ApiError::bad("params must be an object"))?;
        for (k, v) in m {
            let value = match v {
                Json::Object(o) if o.contains_key("vector_of") && o.keys().all(|k| k == "vector_of" || k == "field") => {
                    let field = match o.get("field").and_then(|f| f.as_str()) {
                        Some(f) => f.to_string(),
                        None => self.vector_field().ok_or_else(|| ApiError::bad("the table has no VECTOR field"))?,
                    };
                    self.vector_of(&o["vector_of"], &field).await?
                }
                Json::Object(o) if o.get("embed").is_some_and(|e| e.is_string()) && o.keys().all(|k| k == "embed" || k == "field") => {
                    let (_, e) = self.model(o.get("field").and_then(|f| f.as_str()))?;
                    let text = o["embed"].as_str().unwrap_or_default().to_string();
                    let e = e.clone();
                    let v = tokio::task::spawn_blocking(move || e.embed_query(&text)).await.map_err(ApiError::internal)?;
                    Value::Vector(v.into())
                }
                v => js::param(v)?,
            };
            p = p.set(k, value);
        }
        Ok(p)
    }

    pub fn vector_field(&self) -> Option<String> {
        self.schema.fields.iter().find(|f| matches!(f.ty, strata::FieldType::Vector { .. })).map(|f| f.name.clone())
    }

    /// The vector `field` of the document `key`.
    pub async fn vector_of(self: &Arc<Self>, key: &Json, field: &str) -> Result<Value, ApiError> {
        let pk = self.schema.primary_key.ok_or_else(|| ApiError::bad("vector_of needs a primary key"))?;
        let f = self.schema.field_index(field).ok_or_else(|| ApiError::bad(format!("no field {field}")))?;
        if !matches!(self.schema.fields[f].ty, strata::FieldType::Vector { .. }) {
            return Err(ApiError::bad(format!("{field} is not a VECTOR field")));
        }
        let sql = format!("SELECT {field} FROM {} WHERE {} = :k LIMIT 1", self.schema.table, self.schema.fields[pk].name);
        let params = Params::new().set("k", js::field_value(&self.schema, pk, key)?);
        let a = self.query(QueryReq { sql, params, ..Default::default() }).await?;
        match a.rs.rows.into_iter().next().and_then(|r| r.into_iter().next()) {
            Some(v @ Value::Vector(_)) => Ok(v),
            Some(_) => Err(ApiError::bad(format!("document {key} has no {field}"))),
            None => Err(ApiError::bad(format!("no document {key}"))),
        }
    }

    pub async fn query(self: &Arc<Self>, req: QueryReq) -> Result<Answer, ApiError> {
        let t0 = Instant::now();
        let timeout = self.timeout_of(req.timeout_ms);
        if compound::is_compound(&req.sql, &self.schema) {
            return self.compound(req, t0, timeout).await;
        }
        let select = match bind(&req.sql, &self.schema, &req.params) {
            Ok(s) => s,
            Err(e) => {
                self.metrics.query("invalid", "error", t0.elapsed().as_secs_f64() * 1e3);
                return Err(e.into());
            }
        };
        let kind = metrics::kind(&select);
        let Some(permit) = self.admit(timeout).await else {
            return Err(ApiError { status: 429, message: "too many queries running; try again".into() });
        };
        let res = match &self.backend {
            Backend::Local(db) => match self.snapshot(db, req.snapshot) {
                Ok(snap) => {
                    let (running, n) = Running::start(&self.running);
                    let cancel = Cancel::default();
                    // Each running query gets its share of the cores for its
                    // segments (as engram's recall does).
                    let opts = ExecOptions {
                        stats: req.stats.map(Arc::new),
                        shard: req.shard,
                        threads: (self.cores / n).max(1),
                        deadline: Some(t0 + timeout),
                        cancel: Some(cancel.0.clone()),
                        synonyms: self.synonyms().or_else(|| db.exec.synonyms.clone()),
                        ..db.exec.clone()
                    };
                    let db = db.clone();
                    // The work holds the slot and the count until it ends,
                    // and stops at its next check if this future is dropped.
                    let r = tokio::task::spawn_blocking(move || {
                        let _held = (permit, running);
                        db.execute(&snap, &select, &opts)
                    })
                    .await;
                    drop(cancel);
                    match r {
                        Ok(r) => r.map(|rs| (rs, Vec::new())).map_err(ApiError::from),
                        Err(e) => Err(ApiError::internal(e)),
                    }
                }
                Err(e) => Err(e),
            },
            Backend::Cluster(c) => {
                let r = c.query(&req, &select, t0 + timeout).await.map(|m| (m.rs, m.missing));
                drop(permit);
                r
            }
        };
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        let outcome = match &res {
            Ok((_, missing)) if !missing.is_empty() => "partial",
            Ok(_) => "ok",
            Err(e) if e.status == 504 => "timeout",
            Err(_) => "error",
        };
        self.metrics.query(kind, outcome, ms);
        res.map(|(rs, missing)| Answer { rs, took_ms: ms, kind, missing })
    }

    /// A compound query ([`compound`]): its single-table steps run on one
    /// snapshot here, or each through the coordinator.
    async fn compound(self: &Arc<Self>, req: QueryReq, t0: Instant, timeout: Duration) -> Result<Answer, ApiError> {
        let kind = "compound";
        let Some(permit) = self.admit(timeout).await else {
            return Err(ApiError { status: 429, message: "too many queries running; try again".into() });
        };
        let schema = self.schema.clone();
        let deadline = t0 + timeout;
        // Each step checks it: the caller may have stopped waiting.
        let cancel = Cancel::default();
        let r = match &self.backend {
            Backend::Local(db) => {
                let snap = self.snapshot(db, req.snapshot)?;
                let (running, n) = Running::start(&self.running);
                let opts = ExecOptions {
                    threads: (self.cores / n).max(1),
                    deadline: Some(deadline),
                    cancel: Some(cancel.0.clone()),
                    synonyms: self.synonyms().or_else(|| db.exec.synonyms.clone()),
                    ..db.exec.clone()
                };
                let db = db.clone();
                let r = tokio::task::spawn_blocking(move || {
                    let _held = (permit, running);
                    compound::run(&req.sql, &schema, &req.params, &mut |text, p| {
                        let select = bind(text, snap.schema(), p)?;
                        db.execute(&snap, &select, &opts)
                    })
                })
                .await;
                r.map(|r| r.map(|rs| (rs, Vec::new())))
            }
            Backend::Cluster(c) => {
                let c = c.clone();
                let rt = tokio::runtime::Handle::current();
                let stop = cancel.0.clone();
                tokio::task::spawn_blocking(move || {
                    let _held = permit;
                    // Shards any step was answered without (partial answers).
                    let mut missing: Vec<String> = Vec::new();
                    let rs = compound::run(&req.sql, &schema, &req.params, &mut |text, p| {
                        if stop.load(Ordering::Relaxed) {
                            return Err(strata::Error::Timeout("the query was cancelled".into()));
                        }
                        let select = bind(text, &schema, p)?;
                        let step = QueryReq { sql: text.to_string(), params: p.clone(), partial: req.partial, ..Default::default() };
                        let m = rt.block_on(c.query(&step, &select, deadline)).map_err(|e| strata::Error::Query(e.message))?;
                        for s in m.missing {
                            if !missing.contains(&s) {
                                missing.push(s);
                            }
                        }
                        Ok(m.rs)
                    });
                    rs.map(|rs| (rs, missing))
                })
                .await
            }
        };
        drop(cancel);
        let res = match r {
            Ok(r) => r.map_err(ApiError::from),
            Err(e) => Err(ApiError::internal(e)),
        };
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        let res = match res {
            Err(_) if Instant::now() > deadline => Err(ApiError { status: 504, message: format!("the query ran past its {} ms deadline", timeout.as_millis()) }),
            r => r,
        };
        let outcome = match &res {
            Ok((_, missing)) if !missing.is_empty() => "partial",
            Ok(_) => "ok",
            Err(e) if e.status == 504 => "timeout",
            Err(_) => "error",
        };
        self.metrics.query(kind, outcome, ms);
        res.map(|(rs, missing)| Answer { rs, took_ms: ms, kind, missing })
    }

    /// A query slot, waited for until `timeout`; `None` (counted as
    /// rejected) if none came free.
    async fn admit(&self, timeout: Duration) -> Option<OwnedSemaphorePermit> {
        match tokio::time::timeout(timeout, self.admission.clone().acquire_owned()).await {
            Ok(Ok(p)) => Some(p),
            _ => {
                self.metrics.rejected();
                None
            }
        }
    }

    /// Queries running now, including those whose callers stopped waiting
    /// and which have not reached a check yet.
    pub fn running(&self) -> usize {
        self.running.load(Ordering::Relaxed)
    }

    /// The statistics `sql` scores with, and a lease on the snapshot they
    /// came from (0 from a coordinator, which sums its shards'). The lease
    /// lasts for the query round that follows: the request's deadline
    /// (`timeout_ms`, or the default) and a second.
    pub async fn term_stats(self: &Arc<Self>, sql: &str, params: &Params, timeout_ms: u64) -> Result<(TermStats, u64), ApiError> {
        let select = bind(sql, &self.schema, params)?;
        match &self.backend {
            Backend::Local(db) => {
                let ttl = self.lease_ttl.min(self.timeout_of(timeout_ms) + Duration::from_secs(1));
                let (lease, _) = self.lease_for(ttl)?;
                let snap = self.snapshot(db, lease)?;
                let db = db.clone();
                let opts = ExecOptions { synonyms: self.synonyms().or_else(|| db.exec.synonyms.clone()), ..db.exec.clone() };
                let stats = tokio::task::spawn_blocking(move || db.term_stats_with(&snap, &select, &opts)).await.map_err(ApiError::internal)??;
                Ok((stats, lease))
            }
            Backend::Cluster(c) => Ok((c.term_stats(sql, params, Instant::now() + self.timeout_of(timeout_ms)).await?.0, 0)),
        }
    }

    /// Any statement: rows for a SELECT, the documents written otherwise.
    pub async fn sql(self: &Arc<Self>, sql: &str, params: &Params, timeout_ms: u64) -> Result<SqlAnswer, ApiError> {
        if strata::query::bind_write(sql, &self.schema, params)?.is_none() {
            let a = self.query(QueryReq { sql: sql.into(), params: params.clone(), timeout_ms, ..Default::default() }).await?;
            return Ok(SqlAnswer::Rows(a));
        }
        let t0 = Instant::now();
        let deadline = t0 + self.timeout_of(timeout_ms);
        let r = match &self.backend {
            Backend::Local(_) => self.write(WriteJob::Sql(sql.to_string(), params.clone()), deadline, None).await,
            Backend::Cluster(c) => c.sql(sql, params).await,
        };
        let n = self.wrote(r)?;
        Ok(SqlAnswer::Written { n, took_ms: t0.elapsed().as_secs_f64() * 1e3 })
    }

    /// Applies write operations (see the proto's WriteRequest) in one
    /// transaction per server, which other requests waiting at the same
    /// time may share (see [`writer`]). With an idempotency `key`, a retry
    /// of a request that committed returns its answer unapplied.
    pub async fn write_ops(self: &Arc<Self>, mut ops: Vec<Json>, key: Option<String>) -> Result<u64, ApiError> {
        let n = match &self.backend {
            Backend::Local(db) => {
                if let Some(k) = &key {
                    match self.idempotency.begin(k, idem::hash_ops(&ops)) {
                        idem::Begin::New => {}
                        idem::Begin::Done(n) => return Ok(n),
                        idem::Begin::Busy => return Err(ApiError { status: 409, message: "a write with this idempotency key is still running".into() }),
                        idem::Begin::Mismatch => return Err(ApiError { status: 422, message: "this idempotency key was used for different operations".into() }),
                    }
                }
                let r = async {
                    if self.models.iter().any(|m| m.model.is_ok()) {
                        let (svc, db) = (self.clone(), db.clone());
                        ops = tokio::task::spawn_blocking(move || svc.embed_ops(&db, &mut ops).map(|_| ops)).await.map_err(ApiError::internal)??;
                    }
                    // The deadline bounds the wait for the writer, not the embedding.
                    self.write(WriteJob::Ops(ops), Instant::now() + self.timeout, key.clone()).await
                }
                .await;
                if let (Err(_), Some(k)) = (&r, &key) {
                    // Not committed (the writer reports a commit as Ok).
                    self.idempotency.finish(k, None);
                }
                r
            }
            Backend::Cluster(c) => c.write_ops(ops, key).await,
        };
        self.wrote(n)
    }

    /// Counts a write's outcome, and logs a server-side failure (at most
    /// one line every 10 s, with how many failed since).
    fn wrote(&self, r: Result<u64, ApiError>) -> Result<u64, ApiError> {
        match &r {
            Ok(n) => self.metrics.write(*n),
            Err(e) => {
                self.metrics.write_error(e.status);
                if e.status >= 500 {
                    let mut log = self.write_log.lock().unwrap_or_else(|e| e.into_inner());
                    log.1 += 1;
                    if log.0.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
                        eprintln!("strata-server: a write failed ({} since the last report): {}", log.1, e.message);
                        *log = (Some(Instant::now()), 0);
                    }
                }
            }
        }
        r
    }

    /// Imports rows of `format` from `body` ([`strata::ingest`]): upserted
    /// through the writer (or routed to the shards) [`IMPORT_OPS`] rows a
    /// request, so the rows before a failed request stay written. Returns
    /// the rows written and the fields the schema lacks (skipped).
    pub async fn import(self: &Arc<Self>, body: axum::body::Bytes, format: Format) -> Result<(u64, Vec<String>), ApiError> {
        let schema = self.schema.clone();
        let (ops, unknown) = tokio::task::spawn_blocking(move || -> strata::Result<_> {
            let mut rows = Rows::new(&schema, std::io::Cursor::new(body), format)?;
            let ops: Vec<Json> = rows.by_ref().map(|d| d.map(|d| json!({"op": "upsert", "doc": js::document_json(&schema, &d)}))).collect::<strata::Result<_>>()?;
            Ok((ops, rows.unknown.into_iter().collect::<Vec<_>>()))
        })
        .await
        .map_err(ApiError::internal)??;
        let mut n = 0;
        for chunk in ops.chunks(IMPORT_OPS) {
            n += self.write_ops(chunk.to_vec(), None).await?;
        }
        Ok((n, unknown))
    }

    /// "ok"; for a database, "failing" when it cannot write until it
    /// restarts ([`Self::failing`]) and "degraded" while its flushes fail
    /// (writes still commit, to the WAL, until the memtable holds twice its
    /// limit); for a coordinator, its shards' state ([`Coordinator::health`]).
    pub async fn health(self: &Arc<Self>) -> Json {
        match &self.backend {
            Backend::Local(db) => {
                let s = db.snapshot();
                let mut h = json!({"status": "ok", "role": "shard", "docs": s.n_live_docs(), "segments": s.segments().len(), "txn": s.txn()});
                if let Some(why) = self.failing() {
                    h["status"] = json!("failing");
                    h["error"] = json!(why);
                } else if let Some(e) = db.storage().flush_error() {
                    h["status"] = json!("degraded");
                    h["error"] = json!(format!("flushes fail: {e}"));
                }
                h
            }
            Backend::Cluster(c) => c.health().await,
        }
    }

    /// Why this server cannot write until it restarts, if it cannot: a
    /// write failed partway (a full disk, an I/O error), after which the
    /// database refuses every write until it is reopened, or the writer
    /// thread stopped. Liveness (`/livez`) fails then, so that the server
    /// is restarted. A coordinator is never failing; its shards may be.
    pub fn failing(&self) -> Option<String> {
        match &self.backend {
            Backend::Local(db) if db.storage().is_poisoned() => Some("a write failed partway, so the database refuses writes until it is reopened: restart the server".into()),
            Backend::Local(_) if !self.writer_state.alive.load(Ordering::Relaxed) => Some("the writer thread stopped: restart the server".into()),
            _ => None,
        }
    }

    /// Storage statistics (local) and query counters.
    pub async fn stats(self: &Arc<Self>) -> Json {
        let storage = match &self.backend {
            Backend::Local(db) => serde_json::to_value(db.storage().file_stats()).unwrap_or(Json::Null),
            Backend::Cluster(_) => Json::Null,
        };
        json!({"health": self.health().await, "storage": storage, "metrics": self.metrics.summary()})
    }

    pub async fn prometheus(self: &Arc<Self>) -> String {
        let h = self.health().await;
        let mut more = vec![("strata_queries_running", "gauge", self.running() as u64)];
        if let Backend::Local(db) = &self.backend {
            more.extend([
                ("strata_snapshot_leases", "gauge", self.sweep_leases() as u64),
                ("strata_failing", "gauge", self.failing().is_some() as u64),
                ("strata_flush_failing", "gauge", db.storage().flush_error().is_some() as u64),
                ("strata_writer_panics_total", "counter", self.writer_state.panics.load(Ordering::Relaxed)),
            ]);
        }
        self.metrics.render(h["docs"].as_u64().unwrap_or(0), h["segments"].as_u64().unwrap_or(0), &more)
    }
}

pub enum SqlAnswer {
    Rows(Answer),
    Written { n: u64, took_ms: f64 },
}

/// The shard of `key` among `n`: FNV-1a of its text (an integer key as
/// decimal digits), modulo `n`. Loaders must split documents the same way.
/// Rows one write request of an import carries.
pub const IMPORT_OPS: usize = 1000;

pub fn shard_of(key: &Value, n: usize) -> usize {
    let text = match key {
        Value::Str(s) => s.clone(),
        Value::Int(x) => x.to_string(),
        v => format!("{v:?}"),
    };
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h % n.max(1) as u64) as usize
}

/// The primary key an operation names (`key`, or the document's).
pub fn op_key(schema: &Schema, op: &Json) -> Result<Value, ApiError> {
    let pk = schema.primary_key.ok_or_else(|| ApiError::bad("writes by key need a primary key"))?;
    match (op.get("key"), op.get("doc")) {
        (Some(k), _) => js::field_value(schema, pk, k),
        (None, Some(d)) => js::field_value(schema, pk, d.get(&schema.fields[pk].name).unwrap_or(&Json::Null)),
        _ => Err(ApiError::bad("an operation needs a key or a doc")),
    }
}

/// A write request for the writer thread.
enum WriteJob {
    /// Operations of `/v1/write` (or gRPC `Write`).
    Ops(Vec<Json>),
    /// An `INSERT`, `UPDATE` or `DELETE`.
    Sql(String, Params),
}

struct Queued {
    job: WriteJob,
    deadline: Instant,
    /// `None` once answered.
    reply: Option<tokio::sync::oneshot::Sender<Result<u64, ApiError>>>,
    /// The request's idempotency key, whose outcome the writer records.
    key: Option<String>,
    keys: Arc<idem::Keys>,
}

impl Queued {
    /// Records the outcome under the request's key and answers it.
    fn answer(mut self, r: Result<u64, ApiError>) {
        if let Some(k) = &self.key {
            self.keys.finish(k, r.as_ref().ok().copied());
        }
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(r);
        }
    }
}

impl Drop for Queued {
    /// A request dropped unanswered: the writer panicked applying it.
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            if let Some(k) = &self.key {
                self.keys.finish(k, None);
            }
            let _ = reply.send(Err(ApiError::internal("the writer failed applying this write (see the server's log)")));
        }
    }
}

/// The writer thread's state, for health checks and metrics.
pub struct WriterState {
    /// False once the thread has stopped.
    pub alive: AtomicBool,
    /// Groups of write requests whose application panicked.
    pub panics: AtomicU64,
}

/// Marks the writer stopped when its thread ends, however it ends.
struct Alive<'a>(&'a WriterState);

impl Drop for Alive<'_> {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::Relaxed);
    }
}

/// Counts a query among those running until dropped. The blocking work
/// holds it, so a query whose caller stopped waiting still counts while
/// it runs.
struct Running(Arc<AtomicUsize>);

impl Running {
    /// Starts counting; returns the guard and how many run now.
    fn start(n: &Arc<AtomicUsize>) -> (Running, usize) {
        let now = n.fetch_add(1, Ordering::Relaxed) + 1;
        (Running(n.clone()), now)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A query's cancellation flag ([`ExecOptions::cancel`]), set when this
/// is dropped: when the query's future ends, finished or dropped by its
/// caller (a client that went away, a hedge that lost, a shard timeout).
#[derive(Default)]
struct Cancel(Arc<AtomicBool>);

impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Operations one shared transaction takes at most.
const GROUP_OPS: usize = 4096;

/// The writer of a local database: one thread applying write requests in
/// the order they arrive. Requests of operations that wait together share
/// one transaction, so one commit and one disk flush serve them all (group
/// commit); if one of them fails, each runs again alone, so that only it
/// fails. SQL writes find their rows by reading the database, which does
/// not show a transaction's own changes, so they commit alone. A group
/// that panics is answered with errors and the writer goes on.
fn writer(db: &Db, rx: mpsc::Receiver<Queued>, state: &WriterState) {
    let _alive = Alive(state);
    let mut next = None;
    loop {
        let Some(first) = next.take().or_else(|| rx.recv().ok()) else { return };
        let mut group = vec![first];
        if matches!(group[0].job, WriteJob::Ops(_)) {
            let mut n = group[0].ops().len();
            while n < GROUP_OPS {
                match rx.try_recv() {
                    Ok(q) if matches!(q.job, WriteJob::Ops(_)) => {
                        n += q.ops().len();
                        group.push(q);
                    }
                    Ok(q) => {
                        next = Some(q);
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        // The requests a panic leaves unanswered are answered as they drop.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply_group(db, group))).is_err() {
            state.panics.fetch_add(1, Ordering::Relaxed);
            eprintln!("strata-server: the writer panicked applying a group of writes; they were answered with errors");
        }
    }
}

/// Applies one group of write requests and answers each.
fn apply_group(db: &Db, group: Vec<Queued>) {
    // A request still queued at its deadline is refused unapplied.
    let now = Instant::now();
    let (late, group): (Vec<Queued>, Vec<Queued>) = group.into_iter().partition(|q| q.deadline <= now);
    for q in late {
        q.answer(Err(ApiError { status: 503, message: "the deadline passed while waiting for the writer".into() }));
    }
    let Some(deadline) = group.iter().map(|q| q.deadline).max() else { return };
    if let [Queued { job: WriteJob::Sql(sql, params), .. }] = &group[..] {
        let r = wait_for_writer(deadline, || match db.sql(sql, params)? {
            SqlResult::Written(n) => Ok(n),
            SqlResult::Rows(_) => Err(ApiError::bad("not a write statement")),
        });
        group.into_iter().next().expect("one").answer(r);
        return;
    }
    let requests: Vec<&[Json]> = group.iter().map(|q| q.ops()).collect();
    match wait_for_writer(deadline, || apply_requests(db, &requests)) {
        Ok(counts) => {
            for (q, n) in group.into_iter().zip(counts) {
                q.answer(Ok(n));
            }
        }
        Err(e) if group.len() == 1 => {
            group.into_iter().next().expect("one").answer(Err(e));
        }
        // A failed commit wrote nothing (Strata reports a durable commit
        // as Ok), so each request can run again alone.
        Err(_) => {
            for q in group {
                let r = wait_for_writer(q.deadline, || apply_ops(db, q.ops()));
                q.answer(r);
            }
        }
    }
}

impl Queued {
    fn ops(&self) -> &[Json] {
        match &self.job {
            WriteJob::Ops(ops) => ops,
            WriteJob::Sql(..) => &[],
        }
    }
}

/// A checkpoint's name in the backup directory: letters, digits, '.', '_'
/// and '-', not starting with '.', so that it stays in the directory.
fn checkpoint_name(name: &str) -> Result<&str, ApiError> {
    let ok = !name.is_empty() && name.len() <= 200 && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    ok.then_some(name).ok_or_else(|| ApiError::bad(format!("bad checkpoint name {name:?}: use letters, digits, '.', '_' and '-', not starting with '.'")))
}

/// Runs `write` until it gets the writer or `deadline` passes: Strata
/// answers `Busy` (503) rather than queueing while another write, or a
/// background merge's commit, holds it. A busy write has changed nothing.
fn wait_for_writer<T>(deadline: Instant, mut write: impl FnMut() -> Result<T, ApiError>) -> Result<T, ApiError> {
    loop {
        match write() {
            Err(e) if e.status == 503 && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
            r => return r,
        }
    }
}

/// Applies `ops` to `db` in one transaction; returns how many wrote.
pub fn apply_ops(db: &Db, ops: &[Json]) -> Result<u64, ApiError> {
    Ok(apply_requests(db, &[ops])?[0])
}

/// Applies several requests' operations in one transaction; returns the
/// rows each wrote.
fn apply_requests(db: &Db, requests: &[&[Json]]) -> Result<Vec<u64>, ApiError> {
    let schema = db.schema().clone();
    let mut t = db.storage().begin()?;
    let mut counts = Vec::with_capacity(requests.len());
    for ops in requests {
        counts.push(apply_into(&schema, &mut t, ops)?);
    }
    t.commit()?;
    Ok(counts)
}

fn apply_into(schema: &Schema, t: &mut WriteTxn<'_>, ops: &[Json]) -> Result<u64, ApiError> {
    let mut n = 0u64;
    for op in ops {
        let kind = op.get("op").and_then(|o| o.as_str()).unwrap_or("");
        let doc = || js::document(&schema, op.get("doc").unwrap_or(&Json::Null));
        let field = || op.get("field").and_then(|f| f.as_str()).ok_or_else(|| ApiError::bad(format!("{kind} needs a field")));
        let wrote = match kind {
            "insert" => {
                t.insert(doc()?)?;
                true
            }
            "upsert" => {
                t.upsert(doc()?)?;
                true
            }
            "upsert_if_newer" => {
                let v = op.get("version").and_then(|v| v.as_str()).ok_or_else(|| ApiError::bad("upsert_if_newer needs a version field"))?;
                t.upsert_if_newer(doc()?, v)?
            }
            "delete" => t.delete(&op_key(&schema, op)?)?,
            "set" => {
                let key = op_key(schema, op)?;
                let f = field()?;
                let fi = schema.field_index(f).ok_or_else(|| ApiError::bad(format!("no field {f}")))?;
                t.set(&key, f, js::field_value(schema, fi, op.get("value").unwrap_or(&Json::Null))?)?
            }
            // The transaction's own writes count, other requests' in the
            // same group commit included.
            "add" => {
                let key = op_key(schema, op)?;
                let raw = op.get("value").unwrap_or(&Json::Null);
                let delta = match (raw.as_i64(), raw.as_f64()) {
                    (Some(x), _) => Value::Int(x),
                    (None, Some(x)) => Value::Float(x),
                    _ => return Err(ApiError::bad("add needs a numeric value")),
                };
                t.add(&key, field()?, &delta)?
            }
            other => return Err(ApiError::bad(format!("unknown write op {other:?}"))),
        };
        n += wrote as u64;
    }
    Ok(n)
}


#[cfg(test)]
mod tests {
    use super::*;

    // OPS-6: a panic in the writer thread ended it, and every later write
    // failed with health still "ok". The writer now catches it; the
    // requests of the batch it was applying are answered as they drop.
    #[test]
    fn a_write_dropped_unanswered_gets_an_error() {
        let keys = Arc::new(idem::Keys::default());
        assert_eq!(keys.begin("k", 1), idem::Begin::New);
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let q = Queued { job: WriteJob::Ops(Vec::new()), deadline: Instant::now(), reply: Some(tx), key: Some("k".into()), keys: keys.clone() };
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = q;
            panic!("a bug while applying a write");
        }));
        assert!(r.is_err());
        assert_eq!(rx.try_recv().unwrap().unwrap_err().status, 500);
        // It did not commit, so a retry with its key applies it.
        assert_eq!(keys.begin("k", 1), idem::Begin::New);
    }
}
