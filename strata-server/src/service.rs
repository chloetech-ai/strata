//! The operations both front ends serve, over a local database or, as a
//! coordinator, over shard servers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata::db::{Db, SqlResult};
use strata::embed::Embedder;
use strata::query::exec::{ExecOptions, ResultSet};
use strata::query::stats::TermStats;
use strata::query::{bind, compound, Params};
use strata::schema::Schema;
use strata::storage::{Snapshot, WriteTxn};
use strata::text::Synonyms;
use strata::value::Value;
use tokio::sync::Semaphore;

use crate::coord::Coordinator;
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
    pub lease_ttl: Duration,
    leases: Mutex<HashMap<u64, (Snapshot, Instant)>>,
    next_lease: AtomicU64,
    /// Queries allowed to run at once; the rest wait until their deadline.
    admission: Semaphore,
    cores: usize,
    running: AtomicUsize,
    /// The queue of a local database's writer thread ([`writer`]).
    writes: Option<mpsc::Sender<Queued>>,
    /// The synonym list `synonyms => 1` searches use, and its file.
    synonyms: RwLock<Option<Arc<Synonyms>>>,
    synonyms_file: Option<PathBuf>,
}

impl Service {
    pub fn new(backend: Backend, schema: Schema, agent: AgentConfig, max_concurrent: usize, timeout: Duration) -> Service {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let writes = match &backend {
            Backend::Local(db) => {
                let (tx, rx) = mpsc::channel();
                let db = db.clone();
                std::thread::Builder::new().name("strata-writer".into()).spawn(move || writer(&db, rx)).expect("spawn the writer thread");
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
            lease_ttl: Duration::from_secs(60),
            leases: Mutex::new(HashMap::new()),
            next_lease: AtomicU64::new(1),
            admission: Semaphore::new(max_concurrent.max(1)),
            cores,
            running: AtomicUsize::new(0),
            writes,
            synonyms: RwLock::new(None),
            synonyms_file: None,
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

    /// Hands a write to the writer thread and waits for its commit.
    async fn write(&self, job: WriteJob, deadline: Instant) -> Result<u64, ApiError> {
        let tx = self.writes.as_ref().ok_or_else(|| ApiError::internal("no writer for a coordinator"))?;
        let (reply, rx) = tokio::sync::oneshot::channel();
        tx.send(Queued { job, deadline, reply }).map_err(|_| ApiError::internal("the writer thread stopped"))?;
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
        let db = self.local().ok_or_else(|| ApiError::bad("a coordinator holds no snapshots"))?;
        let snap = db.snapshot();
        let txn = snap.txn();
        let id = self.next_lease.fetch_add(1, Ordering::Relaxed);
        let mut l = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        l.retain(|_, (_, t)| *t > now);
        l.insert(id, (snap, now + self.lease_ttl));
        Ok((id, txn))
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
        let timeout = if req.timeout_ms > 0 { Duration::from_millis(req.timeout_ms) } else { self.timeout };
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
        let Ok(permit) = tokio::time::timeout(timeout, self.admission.acquire()).await else {
            self.metrics.rejected();
            return Err(ApiError { status: 503, message: "too many queries running; try again".into() });
        };
        let res = match &self.backend {
            Backend::Local(db) => match self.snapshot(db, req.snapshot) {
                Ok(snap) => {
                    let running = self.running.fetch_add(1, Ordering::Relaxed) + 1;
                    // Each running query gets its share of the cores for its
                    // segments (as engram's recall does).
                    let opts = ExecOptions {
                        stats: req.stats.map(Arc::new),
                        shard: req.shard,
                        threads: (self.cores / running).max(1),
                        deadline: Some(t0 + timeout),
                        synonyms: self.synonyms().or_else(|| db.exec.synonyms.clone()),
                        ..db.exec.clone()
                    };
                    let db = db.clone();
                    let r = tokio::task::spawn_blocking(move || db.execute(&snap, &select, &opts)).await;
                    self.running.fetch_sub(1, Ordering::Relaxed);
                    match r {
                        Ok(r) => r.map(|rs| (rs, Vec::new())).map_err(ApiError::from),
                        Err(e) => Err(ApiError::internal(e)),
                    }
                }
                Err(e) => Err(e),
            },
            Backend::Cluster(c) => c.query(&req, &select, t0 + timeout).await.map(|m| (m.rs, m.missing)),
        };
        drop(permit);
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
        let Ok(permit) = tokio::time::timeout(timeout, self.admission.acquire()).await else {
            self.metrics.rejected();
            return Err(ApiError { status: 503, message: "too many queries running; try again".into() });
        };
        let schema = self.schema.clone();
        let deadline = t0 + timeout;
        let r = match &self.backend {
            Backend::Local(db) => {
                let snap = self.snapshot(db, req.snapshot)?;
                let running = self.running.fetch_add(1, Ordering::Relaxed) + 1;
                let opts = ExecOptions {
                    threads: (self.cores / running).max(1),
                    deadline: Some(deadline),
                    synonyms: self.synonyms().or_else(|| db.exec.synonyms.clone()),
                    ..db.exec.clone()
                };
                let db = db.clone();
                let r = tokio::task::spawn_blocking(move || {
                    compound::run(&req.sql, &schema, &req.params, &mut |text, p| {
                        let select = bind(text, snap.schema(), p)?;
                        db.execute(&snap, &select, &opts)
                    })
                })
                .await;
                self.running.fetch_sub(1, Ordering::Relaxed);
                r
            }
            Backend::Cluster(c) => {
                let c = c.clone();
                let rt = tokio::runtime::Handle::current();
                tokio::task::spawn_blocking(move || {
                    compound::run(&req.sql, &schema, &req.params, &mut |text, p| {
                        let select = bind(text, &schema, p)?;
                        let step = QueryReq { sql: text.to_string(), params: p.clone(), ..Default::default() };
                        rt.block_on(c.query(&step, &select, deadline)).map_err(|e| strata::Error::Query(e.message))
                    })
                })
                .await
            }
        };
        drop(permit);
        let res = match r {
            Ok(r) => r.map_err(ApiError::from),
            Err(e) => Err(ApiError::internal(e)),
        };
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        let res = match res {
            Err(_) if Instant::now() > deadline => Err(ApiError { status: 504, message: format!("the query ran past its {} ms deadline", timeout.as_millis()) }),
            r => r,
        };
        self.metrics.query(kind, if res.is_ok() { "ok" } else if res.as_ref().is_err_and(|e| e.status == 504) { "timeout" } else { "error" }, ms);
        res.map(|rs| Answer { rs, took_ms: ms, kind })
    }

    /// The statistics `sql` scores with, and a lease on the snapshot they
    /// came from (0 from a coordinator, which sums its shards').
    pub async fn term_stats(self: &Arc<Self>, sql: &str, params: &Params) -> Result<(TermStats, u64), ApiError> {
        let select = bind(sql, &self.schema, params)?;
        match &self.backend {
            Backend::Local(db) => {
                let (lease, _) = self.lease()?;
                let snap = self.snapshot(db, lease)?;
                let db = db.clone();
                let opts = ExecOptions { synonyms: self.synonyms().or_else(|| db.exec.synonyms.clone()), ..db.exec.clone() };
                let stats = tokio::task::spawn_blocking(move || db.term_stats_with(&snap, &select, &opts)).await.map_err(ApiError::internal)??;
                Ok((stats, lease))
            }
            Backend::Cluster(c) => Ok((c.term_stats(sql, params, Instant::now() + self.timeout).await?.0, 0)),
        }
    }

    /// Any statement: rows for a SELECT, the documents written otherwise.
    pub async fn sql(self: &Arc<Self>, sql: &str, params: &Params, timeout_ms: u64) -> Result<SqlAnswer, ApiError> {
        if strata::query::bind_write(sql, &self.schema, params)?.is_none() {
            let a = self.query(QueryReq { sql: sql.into(), params: params.clone(), timeout_ms, ..Default::default() }).await?;
            return Ok(SqlAnswer::Rows(a));
        }
        let t0 = Instant::now();
        let deadline = t0 + if timeout_ms > 0 { Duration::from_millis(timeout_ms) } else { self.timeout };
        let n = match &self.backend {
            Backend::Local(_) => self.write(WriteJob::Sql(sql.to_string(), params.clone()), deadline).await?,
            Backend::Cluster(c) => c.sql(sql, params).await?,
        };
        self.metrics.write(n);
        Ok(SqlAnswer::Written { n, took_ms: t0.elapsed().as_secs_f64() * 1e3 })
    }

    /// Applies write operations (see the proto's WriteRequest) in one
    /// transaction per server, which other requests waiting at the same
    /// time may share (see [`writer`]).
    pub async fn write_ops(self: &Arc<Self>, mut ops: Vec<Json>) -> Result<u64, ApiError> {
        let n = match &self.backend {
            Backend::Local(db) => {
                let deadline = Instant::now() + self.timeout;
                if self.models.iter().any(|m| m.model.is_ok()) {
                    let (svc, db) = (self.clone(), db.clone());
                    ops = tokio::task::spawn_blocking(move || svc.embed_ops(&db, &mut ops).map(|_| ops)).await.map_err(ApiError::internal)??;
                }
                self.write(WriteJob::Ops(ops), deadline).await?
            }
            Backend::Cluster(c) => c.write_ops(ops).await?,
        };
        self.metrics.write(n);
        Ok(n)
    }

    pub async fn health(self: &Arc<Self>) -> Json {
        match &self.backend {
            Backend::Local(db) => {
                let s = db.snapshot();
                json!({"status": "ok", "role": "shard", "docs": s.n_live_docs(), "segments": s.segments().len(), "txn": s.txn()})
            }
            Backend::Cluster(c) => c.health().await,
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
        self.metrics.render(h["docs"].as_u64().unwrap_or(0), h["segments"].as_u64().unwrap_or(0))
    }
}

pub enum SqlAnswer {
    Rows(Answer),
    Written { n: u64, took_ms: f64 },
}

/// The shard of `key` among `n`: FNV-1a of its text (an integer key as
/// decimal digits), modulo `n`. Loaders must split documents the same way.
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
    reply: tokio::sync::oneshot::Sender<Result<u64, ApiError>>,
}

/// Operations one shared transaction takes at most.
const GROUP_OPS: usize = 4096;

/// The writer of a local database: one thread applying write requests in
/// the order they arrive. Requests of operations that wait together share
/// one transaction, so one commit and one disk flush serve them all (group
/// commit); if one of them fails, each runs again alone, so that only it
/// fails. SQL writes find their rows by reading the database, which does
/// not show a transaction's own changes, so they commit alone.
fn writer(db: &Db, rx: mpsc::Receiver<Queued>) {
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
        // A request still queued at its deadline is refused unapplied.
        let now = Instant::now();
        let (late, group): (Vec<Queued>, Vec<Queued>) = group.into_iter().partition(|q| q.deadline <= now);
        for q in late {
            let _ = q.reply.send(Err(ApiError { status: 503, message: "the deadline passed while waiting for the writer".into() }));
        }
        let Some(deadline) = group.iter().map(|q| q.deadline).max() else { continue };
        if let [Queued { job: WriteJob::Sql(sql, params), .. }] = &group[..] {
            let r = wait_for_writer(deadline, || match db.sql(sql, params)? {
                SqlResult::Written(n) => Ok(n),
                SqlResult::Rows(_) => Err(ApiError::bad("not a write statement")),
            });
            let _ = group.into_iter().next().expect("one").reply.send(r);
            continue;
        }
        let requests: Vec<&[Json]> = group.iter().map(|q| q.ops()).collect();
        match wait_for_writer(deadline, || apply_requests(db, &requests)) {
            Ok(counts) => {
                for (q, n) in group.into_iter().zip(counts) {
                    let _ = q.reply.send(Ok(n));
                }
            }
            Err(e) if group.len() == 1 => {
                let _ = group.into_iter().next().expect("one").reply.send(Err(e));
            }
            Err(_) => {
                for q in group {
                    let r = wait_for_writer(q.deadline, || apply_ops(db, q.ops()));
                    let _ = q.reply.send(r);
                }
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
    // Values this transaction set, for `add` to build on.
    let mut seen: HashMap<(String, String), f64> = HashMap::new();
    let mut counts = Vec::with_capacity(requests.len());
    for ops in requests {
        counts.push(apply_into(db, &schema, &mut t, &mut seen, ops)?);
    }
    t.commit()?;
    Ok(counts)
}

fn apply_into(db: &Db, schema: &Schema, t: &mut WriteTxn<'_>, seen: &mut HashMap<(String, String), f64>, ops: &[Json]) -> Result<u64, ApiError> {
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
            "set" | "add" => {
                let key = op_key(&schema, op)?;
                let f = field()?;
                let fi = schema.field_index(f).ok_or_else(|| ApiError::bad(format!("no field {f}")))?;
                let raw = op.get("value").unwrap_or(&Json::Null);
                let value = if kind == "add" {
                    let x = raw.as_f64().ok_or_else(|| ApiError::bad("add needs a numeric value"))?;
                    let slot = (format!("{key:?}"), f.to_string());
                    let base = match seen.get(&slot) {
                        Some(b) => *b,
                        None => current(db, &schema, &key, fi)?,
                    };
                    seen.insert(slot, base + x);
                    match schema.fields[fi].ty {
                        strata::FieldType::Int => Value::Int((base + x) as i64),
                        _ => Value::Float(base + x),
                    }
                } else {
                    let v = js::field_value(&schema, fi, raw)?;
                    if let Some(x) = v.as_f64() {
                        seen.insert((format!("{key:?}"), f.to_string()), x);
                    }
                    v
                };
                t.set(&key, f, value)?
            }
            other => return Err(ApiError::bad(format!("unknown write op {other:?}"))),
        };
        n += wrote as u64;
    }
    Ok(n)
}

/// Numeric field `f` of the live document `key` (0 if it has none).
fn current(db: &Db, schema: &Schema, key: &Value, f: usize) -> Result<f64, ApiError> {
    let pk = schema.primary_key.expect("checked by op_key");
    let sql = format!("SELECT {} FROM {} WHERE {} = :k", schema.fields[f].name, schema.table, schema.fields[pk].name);
    let r = db.query(&sql, &Params::new().set("k", key.clone()))?;
    Ok(r.rows.first().and_then(|r| r[0].as_f64()).unwrap_or(0.0))
}
