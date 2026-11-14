//! What the command center (the UI) shows of a running server, beside
//! `/metrics`: the queries running now, which an operator may cancel
//! ([`LiveQueries`]); the latest queries and the slow ones, each with its
//! trace ([`QueryLog`]); statistics per statement shape, as
//! `pg_stat_statements` keeps them ([`Statements`]); and a time series of
//! the server's rates and gauges, sampled every few seconds and kept for
//! an hour ([`Series`]), so that a page opened now has a history to draw.
//!
//! A trace ([`Trace`]) is the timed steps of one request: waiting for a
//! query slot, executing, and on a coordinator every call to a shard's
//! replica (hedges and retries included), so that a slow query shows
//! where its time went.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value as Json};
use chloedb::query::exec::ExecStats;
use chloedb::query::steps::Step;
use tokio::sync::Notify;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- traces

tokio::task_local! {
    static TRACE: Arc<Trace>;
}

/// The timed steps of one request, from its start.
pub struct Trace {
    t0: Instant,
    spans: Mutex<Vec<Span>>,
}

/// One step: what it was, on which shard and replica, when it started and
/// how long it took (microseconds from the request's start), and how it
/// ended ("ok", "error", "outrun" for a hedge that lost).
#[derive(Clone, Debug, serde::Serialize)]
pub struct Span {
    pub name: Cow<'static, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replica: Option<String>,
    pub start_us: u64,
    pub dur_us: u64,
    pub outcome: &'static str,
    /// The call was a hedge or a retry: not the first asked of its shard.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub again: bool,
}

impl Trace {
    pub fn new() -> Arc<Trace> {
        Arc::new(Trace { t0: Instant::now(), spans: Mutex::new(Vec::new()) })
    }

    /// When the request began: what its spans, and its query's steps
    /// ([`chloedb::query::steps`]), count from.
    pub fn t0(&self) -> Instant {
        self.t0
    }

    /// The steps so far, at most [`MAX_SPANS`].
    pub fn spans(&self) -> Vec<Span> {
        lock(&self.spans).clone()
    }
}

/// The most steps a trace keeps: a compound query over many shards could
/// make thousands.
pub const MAX_SPANS: usize = 512;

/// Runs `f` with `trace` as its request's trace.
pub fn traced<F: Future>(trace: Arc<Trace>, f: F) -> impl Future<Output = F::Output> {
    TRACE.scope(trace, f)
}

/// The trace of the request this task serves, if any.
pub fn trace() -> Option<Arc<Trace>> {
    TRACE.try_with(|t| t.clone()).ok()
}

/// Runs `f` in `trace` if there is one (a task that a request's work
/// moved to: a blocking thread's `block_on`).
pub fn within<F: Future>(trace: Option<Arc<Trace>>, f: F) -> impl Future<Output = F::Output> {
    match trace {
        Some(t) => futures::future::Either::Left(TRACE.scope(t, f)),
        None => futures::future::Either::Right(f),
    }
}

/// A step that started at `t0` and ends now, in the current trace.
pub fn record(name: impl Into<Cow<'static, str>>, t0: Instant, outcome: &'static str) {
    record_at(name.into(), None, None, t0, outcome, false);
}

/// A call to replica `replica` of shard `shard`.
pub fn record_call(name: Cow<'static, str>, shard: usize, replica: String, t0: Instant, outcome: &'static str, again: bool) {
    record_at(name, Some(shard), Some(replica), t0, outcome, again);
}

fn record_at(name: Cow<'static, str>, shard: Option<usize>, replica: Option<String>, t0: Instant, outcome: &'static str, again: bool) {
    let _ = TRACE.try_with(|t| {
        let mut spans = lock(&t.spans);
        if spans.len() < MAX_SPANS {
            let start_us = t0.saturating_duration_since(t.t0).as_micros() as u64;
            spans.push(Span { name, shard, replica, start_us, dur_us: t0.elapsed().as_micros() as u64, outcome, again });
        }
    });
}

/// Records a shard call when dropped: as `outcome` once set, else as
/// "outrun" (another replica answered first, or the request ended).
pub struct CallSpan {
    pub name: Cow<'static, str>,
    pub shard: usize,
    pub replica: String,
    pub t0: Instant,
    pub again: bool,
    pub outcome: Option<&'static str>,
}

impl Drop for CallSpan {
    fn drop(&mut self) {
        let (name, replica) = (std::mem::take(&mut self.name), std::mem::take(&mut self.replica));
        record_call(name, self.shard, replica, self.t0, self.outcome.unwrap_or("outrun"), self.again);
    }
}

/// The RPC a response type answers: `pb::QueryResponse` is "Query".
pub fn rpc_name<T>() -> Cow<'static, str> {
    let full = std::any::type_name::<T>();
    let last = full.rsplit("::").next().unwrap_or(full);
    let last = last.split('<').next().unwrap_or(last);
    Cow::Borrowed(last.strip_suffix("Response").unwrap_or(last))
}

// --------------------------------------------------------------- callers

tokio::task_local! {
    static CALLER: Arc<Caller>;
}

/// The `x-strata-client` of the command center's own requests.
pub use crate::keys::COMMAND_CENTER;

/// Who sent the request a query runs for: the key's name (`root token`
/// for the server's token) and id, the client's address, its app
/// (`x-strata-app`, else the product its user agent names), and
/// `command-center` for the UI's own requests.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Caller {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

impl Caller {
    /// The caller of a request by `who`, as the request's own
    /// ([`crate::keys::caller`]: its address, app and client) says.
    pub fn of(who: &crate::keys::Principal, request: Option<crate::keys::Caller>) -> Caller {
        let key = match who.name.as_str() {
            "open" if who.key.is_none() => None,
            _ => Some(crate::keys::audit_name(who)),
        };
        let r = request.unwrap_or_default();
        Caller { key, key_id: who.key.clone(), addr: r.addr, app: r.app, client: r.client.filter(|c| c.eq_ignore_ascii_case(COMMAND_CENTER)).map(|_| COMMAND_CENTER.to_string()) }
    }

    /// The command center's own request.
    pub fn is_command_center(&self) -> bool {
        self.client.as_deref() == Some(COMMAND_CENTER)
    }
}

/// Runs `f` as a request of `caller`.
pub fn calling<F: Future>(caller: Arc<Caller>, f: F) -> impl Future<Output = F::Output> {
    CALLER.scope(caller, f)
}

/// Who sent the request this task serves, if it serves one.
pub fn caller() -> Option<Arc<Caller>> {
    CALLER.try_with(|c| c.clone()).ok()
}

/// Keep only the types of queries' parameters (`--redact-params`).
static REDACT_PARAMS: AtomicBool = AtomicBool::new(false);

pub fn set_redact_params(on: bool) {
    REDACT_PARAMS.store(on, Ordering::Relaxed);
}

pub fn redacts_params() -> bool {
    REDACT_PARAMS.load(Ordering::Relaxed)
}

/// The most bytes of JSON a query's parameters are kept in.
pub const PARAMS_BYTES: usize = 1024;

/// A query's parameters as kept with it: their values as JSON (only their
/// types with `--redact-params`), long texts and lists cut, in at most
/// [`PARAMS_BYTES`]; `None` without parameters.
pub fn params_json(p: &chloedb::query::Params) -> Option<Arc<Json>> {
    if p.0.is_empty() {
        return None;
    }
    let redact = redacts_params();
    let mut names: Vec<&String> = p.0.keys().collect();
    names.sort();
    let mut out = serde_json::Map::new();
    let mut bytes = 2;
    for (i, name) in names.iter().enumerate() {
        let v = crate::json::to_json(&p.0[*name]);
        let v = match (redact, v) {
            (true, v) => json!(type_of(&v)),
            (false, Json::String(s)) if s.len() > 200 => json!(format!("{}...", crate::obs::truncate(&s, 200))),
            (false, Json::Array(a)) if a.len() > 16 => json!(format!("[{} values]", a.len())),
            (false, v) => v,
        };
        let add = name.len() + v.to_string().len() + 4;
        if bytes + add > PARAMS_BYTES - 32 {
            out.insert("...".into(), json!(format!("{} more", names.len() - i)));
            break;
        }
        bytes += add;
        out.insert((*name).clone(), v);
    }
    Some(Arc::new(Json::Object(out)))
}

fn type_of(v: &Json) -> String {
    match v {
        Json::Null => "null".into(),
        Json::Bool(_) => "bool".into(),
        Json::Number(n) if n.is_i64() || n.is_u64() => "int".into(),
        Json::Number(_) => "float".into(),
        Json::String(_) => "text".into(),
        Json::Array(a) if a.iter().all(Json::is_number) && !a.is_empty() => format!("vector({})", a.len()),
        Json::Array(a) => format!("list({})", a.len()),
        Json::Object(_) => "object".into(),
    }
}

// ---------------------------------------------------------- live queries

/// A query running now.
pub struct LiveQuery {
    pub id: u64,
    pub request: Option<Arc<str>>,
    pub sql: String,
    /// Its statement's shape ([`fingerprint`]).
    pub fingerprint: String,
    pub kind: &'static str,
    /// Who sent it, its parameters and deadline.
    pub meta: Meta,
    /// Asked by a coordinator, as a shard.
    pub shard: bool,
    pub started_ms: u64,
    t0: Instant,
    stop: AtomicBool,
    wake: Notify,
    trace: Option<Arc<Trace>>,
}

impl LiveQuery {
    /// Ends when an operator cancels the query.
    pub async fn cancelled(&self) {
        loop {
            let n = self.wake.notified();
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            n.await;
        }
    }

    fn json(&self) -> Json {
        let mut j = json!({
            "id": self.id, "sql": self.sql, "fingerprint": self.fingerprint, "kind": self.kind, "shard": self.shard,
            "started_ms": self.started_ms, "elapsed_ms": self.t0.elapsed().as_secs_f64() * 1e3,
            "cancelled": self.stop.load(Ordering::Relaxed), "deadline_ms": self.meta.deadline_ms,
        });
        if let Some(c) = &self.meta.caller {
            for (k, v) in json!(c).as_object().into_iter().flatten() {
                j[k] = v.clone();
            }
        }
        if let Some(p) = &self.meta.params {
            j["params"] = (**p).clone();
        }
        if let Some(r) = &self.request {
            j["request_id"] = json!(r);
        }
        if let Some(t) = &self.trace {
            j["trace"] = json!(t.spans());
        }
        j
    }
}

/// The queries running now, by id.
#[derive(Default)]
pub struct LiveQueries {
    next: AtomicU64,
    running: Mutex<BTreeMap<u64, Arc<LiveQuery>>>,
}

/// A query's place in [`LiveQueries`], given up when dropped.
pub struct Live {
    reg: Arc<LiveQueries>,
    pub query: Arc<LiveQuery>,
}

impl Drop for Live {
    fn drop(&mut self) {
        lock(&self.reg.running).remove(&self.query.id);
    }
}

/// The answer a query cancelled by an operator gets (status 499).
pub fn cancelled_error(id: u64) -> crate::ApiError {
    crate::ApiError { code: None, status: 499, message: format!("query {id} was cancelled by an operator (DELETE /v1/queries/{id})") }
}

/// How a query can end: answered (`ok`, or `partial` without some
/// shards), failed on the server's side (`error`, or `timeout`: the
/// server's deadline passed), the client's doing (`client_error`: a 4xx
/// such as bad SQL; `client_deadline`: its own `timeout_ms`, below the
/// server's default, passed), `refused` (429: no slot, no memory, or a
/// Contain rule) or `cancelled` by an operator.
pub const OUTCOMES: [&str; 8] = ["ok", "partial", "error", "timeout", "client_error", "client_deadline", "refused", "cancelled"];

/// The outcome of a query that failed with HTTP status `status`; a
/// deadline that passed was the client's own when `client_deadline`.
pub fn outcome_of(status: u16, client_deadline: bool) -> &'static str {
    match status {
        504 if client_deadline => "client_deadline",
        504 => "timeout",
        429 => "refused",
        499 => "cancelled",
        400..=498 => "client_error",
        _ => "error",
    }
}

/// Whether a query that ended so failed, for whatever reason.
pub fn failed(outcome: &str) -> bool {
    !matches!(outcome, "ok" | "partial")
}

/// Whether a query that ended so failed on the server's side.
pub fn server_failed(outcome: &str) -> bool {
    matches!(outcome, "error" | "timeout")
}

impl LiveQueries {
    /// Lists `sql` as running until the guard is dropped.
    pub fn start(self: &Arc<Self>, sql: &str, kind: &'static str, shard: bool, meta: &Meta) -> Live {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let query = Arc::new(LiveQuery {
            id,
            request: crate::obs::current(),
            sql: crate::obs::truncate(sql, 4000).into_owned(),
            fingerprint: fingerprint(sql),
            kind,
            meta: meta.clone(),
            shard,
            started_ms: now_ms(),
            t0: Instant::now(),
            stop: AtomicBool::new(false),
            wake: Notify::new(),
            trace: trace(),
        });
        lock(&self.running).insert(id, query.clone());
        Live { reg: self.clone(), query }
    }

    /// Runs `work` as query `live`, which ends with [`cancelled_error`] if
    /// an operator cancels it first (dropping `work`, which stops the
    /// query's execution at its next check).
    pub async fn run<T, F>(live: &Live, work: F) -> Result<T, crate::ApiError>
    where
        F: Future<Output = Result<T, crate::ApiError>>,
    {
        tokio::select! {
            r = work => r,
            _ = live.query.cancelled() => Err(cancelled_error(live.query.id)),
        }
    }

    /// The running queries, the longest running first.
    pub fn list(&self) -> Vec<Json> {
        let mut qs: Vec<Arc<LiveQuery>> = lock(&self.running).values().cloned().collect();
        qs.sort_by_key(|q| q.t0);
        qs.iter().map(|q| q.json()).collect()
    }

    pub fn len(&self) -> usize {
        lock(&self.running).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The running queries by statement shape and key, the most time spent
    /// first: each group's `count`, `oldest_ms` (the longest running, ms),
    /// `total_ms` (their time so far) and `ids`. A coordinator's calls as
    /// a shard are left out.
    pub fn groups(&self) -> Vec<Json> {
        let qs: Vec<Arc<LiveQuery>> = lock(&self.running).values().cloned().collect();
        let mut by: BTreeMap<(String, Option<String>), Vec<&Arc<LiveQuery>>> = BTreeMap::new();
        for q in qs.iter().filter(|q| !q.shard) {
            by.entry((q.fingerprint.clone(), q.meta.caller.as_ref().and_then(|c| c.key.clone()))).or_default().push(q);
        }
        let mut out: Vec<(f64, Json)> = by
            .into_iter()
            .map(|((fp, key), qs)| {
                let ms: Vec<f64> = qs.iter().map(|q| q.t0.elapsed().as_secs_f64() * 1e3).collect();
                let total: f64 = ms.iter().sum();
                let oldest = qs.iter().map(|q| q.started_ms).min().unwrap_or(0);
                let mut ids: Vec<u64> = qs.iter().map(|q| q.id).collect();
                ids.sort_unstable();
                let j = json!({
                    "fingerprint": fp, "sql": qs[0].sql, "key": key, "count": qs.len(), "oldest_ms": ms.iter().cloned().fold(0.0, f64::max),
                    "oldest_started_ms": oldest, "total_ms": total, "ids": ids,
                    "client": qs[0].meta.caller.as_ref().and_then(|c| c.client.clone()),
                });
                (total, j)
            })
            .collect();
        out.sort_by(|a, b| b.0.total_cmp(&a.0));
        out.into_iter().map(|(_, j)| j).collect()
    }

    /// Cancels the running queries of statement `fingerprint`, of `key`
    /// (its name or id) when given; a coordinator's calls as a shard are
    /// its coordinator's to cancel. Returns those cancelled, as listed.
    pub fn cancel_matching(&self, fingerprint: &str, key: Option<&str>) -> Vec<Json> {
        let qs: Vec<Arc<LiveQuery>> = lock(&self.running).values().filter(|q| !q.shard && q.fingerprint == fingerprint).cloned().collect();
        let mut out = Vec::new();
        for q in qs {
            let c = q.meta.caller.as_deref();
            if key.is_some_and(|k| c.is_none_or(|c| c.key.as_deref() != Some(k) && c.key_id.as_deref() != Some(k))) {
                continue;
            }
            if !q.stop.swap(true, Ordering::Relaxed) {
                q.wake.notify_waiters();
                out.push(q.json());
            }
        }
        out
    }

    /// Cancels query `id`; false if it is not running.
    pub fn cancel(&self, id: u64) -> bool {
        let Some(q) = lock(&self.running).get(&id).cloned() else { return false };
        q.stop.store(true, Ordering::Relaxed);
        q.wake.notify_waiters();
        true
    }
}

// ------------------------------------------------- latest and slow queries

/// What a query is kept with besides its text and how it ended.
#[derive(Clone, Debug, Default)]
pub struct Meta {
    /// Who sent it ([`caller`]).
    pub caller: Option<Arc<Caller>>,
    /// Its parameters ([`params_json`]).
    pub params: Option<Arc<Json>>,
    /// Its deadline, in milliseconds.
    pub deadline_ms: u64,
    /// The deadline is the client's own (its `timeout_ms` below the
    /// server's default), so one that passes is `client_deadline`.
    pub client_deadline: bool,
}

/// A query that ended.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Finished {
    /// Its number among the queries this server ran, for
    /// `GET /v1/queries/finished/:id`.
    pub id: u64,
    pub ts_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Arc<str>>,
    pub sql: String,
    pub fingerprint: String,
    pub kind: &'static str,
    pub outcome: &'static str,
    pub ms: f64,
    pub rows: usize,
    pub shard: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Its deadline, in milliseconds (0: not known).
    pub deadline_ms: u64,
    /// Who sent it: `key`, `key_id`, `addr`, `app`, `client`.
    #[serde(flatten)]
    pub caller: Caller,
    /// Its parameters ([`params_json`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Arc<Json>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trace: Vec<Span>,
    /// Its profile: the timed steps of its execution, from the request's
    /// start as the trace's spans are ([`chloedb::query::steps`]), with
    /// CPU time when `profiled`; what ran, line by line; and its counters.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub profiled: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<String>,
    pub filter_docs: u64,
    pub scored: u64,
}

impl Finished {
    /// As the lists of queries show it: without its steps and plan, which
    /// `GET /v1/queries/finished/:id` gives, but their count and CPU time.
    pub fn summary(&self) -> Json {
        let mut j = serde_json::to_value(self).unwrap_or(Json::Null);
        if let Some(o) = j.as_object_mut() {
            o.remove("steps");
            o.remove("plan");
            o.insert("steps_n".into(), json!(self.steps.len()));
            if let Some(cpu) = cpu_ms(&self.steps) {
                o.insert("cpu_ms".into(), json!(cpu));
            }
            if let Some(b) = alloc_bytes(&self.steps) {
                o.insert("alloc_bytes".into(), json!(b));
            }
        }
        j
    }
}

/// The CPU time of a query's steps, in milliseconds: each measured step's,
/// the steps inside it counted in it (a coordinator measures none of its
/// own steps, its shards their own); `None` unless any was measured.
pub fn cpu_ms(steps: &[Step]) -> Option<f64> {
    outermost(steps, |s| s.cpu_us).map(|us| us / 1e3)
}

/// The bytes a query's steps allocated, counted as [`cpu_ms`] counts CPU
/// time; `None` unless any step counted them.
pub fn alloc_bytes(steps: &[Step]) -> Option<u64> {
    outermost(steps, |s| s.alloc_bytes.map(|b| b as f64)).map(|b| b as u64)
}

/// The sum of `f` over the outermost steps that have it.
fn outermost(steps: &[Step], f: impl Fn(&Step) -> Option<f64>) -> Option<f64> {
    let (mut total, mut inside) = (None, None);
    for s in steps {
        if inside.is_some_and(|d| s.depth > d) {
            continue;
        }
        inside = None;
        if let Some(c) = f(s) {
            *total.get_or_insert(0.0) += c;
            inside = Some(s.depth);
        }
    }
    total
}

/// Profiling settings an operator changes while the server runs
/// (`PUT /v1/profile/settings`).
#[derive(Default)]
pub struct Profiling {
    /// The slow threshold in microseconds, plus one (0: as the server
    /// started, `u64::MAX`: none).
    slow: AtomicU64,
    /// Measure every query's CPU time, as `"profile": true` does for one.
    pub cpu_all: AtomicBool,
}

impl Profiling {
    /// The slow threshold: as set at run time, else `started`.
    pub fn slow(&self, started: Option<Duration>) -> Option<Duration> {
        match self.slow.load(Ordering::Relaxed) {
            0 => started,
            u64::MAX => None,
            us => Some(Duration::from_micros(us - 1)),
        }
    }

    pub fn set_slow(&self, slow: Option<Duration>) {
        self.slow.store(slow.map_or(u64::MAX, |d| (d.as_micros() as u64).min(u64::MAX - 2) + 1), Ordering::Relaxed);
    }
}

/// The latest queries, the slow ones, and statistics per statement.
pub struct QueryLog {
    next: AtomicU64,
    recent: Mutex<VecDeque<Arc<Finished>>>,
    slow: Mutex<VecDeque<Arc<Finished>>>,
    pub statements: Statements,
    /// Which clients sent each key ([`KeyClients`]).
    pub clients: KeyClients,
    /// Also hands every query that ended, and whether it was slow, here:
    /// the server's log store ([`crate::logstore::LogStore::keep_query`]).
    pub keep: std::sync::OnceLock<Keep>,
}

/// Where a [`QueryLog`] hands the queries that end.
pub type Keep = Box<dyn Fn(&Arc<Finished>, bool) + Send + Sync>;

/// Which clients sent each key, and how much, over the last
/// [`KeyClients::KEEP_MS`]: every query a client sent with a key counts,
/// not a sample, so a key's rotation can tell which clients still have to
/// move (AC-4). Bounded: [`KeyClients::PER_KEY`] clients a key (the least
/// recent go), [`KeyClients::KEYS`] keys.
#[derive(Default)]
pub struct KeyClients {
    by_key: Mutex<HashMap<String, Vec<KeyClient>>>,
}

/// One client of a key ([`KeyClients`]).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct KeyClient {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    pub calls: u64,
    pub failed: u64,
    pub first_ms: u64,
    pub last_ms: u64,
    /// The coordinators it went through, once merged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coordinators: Vec<String>,
}

impl KeyClient {
    fn same(&self, o: &KeyClient) -> bool {
        (&self.app, &self.client, &self.addr) == (&o.app, &o.client, &o.addr)
    }
}

impl KeyClients {
    pub const KEEP_MS: u64 = 7 * 24 * 3_600_000;
    pub const PER_KEY: usize = 32;
    pub const KEYS: usize = 2_000;

    /// A query `c` sent (`failed` or not) at `t`.
    pub fn add(&self, c: &Caller, failed: bool, t: u64) {
        let Some(key) = c.key_id.as_deref() else { return };
        let mut by = lock(&self.by_key);
        if !by.contains_key(key) && by.len() >= Self::KEYS {
            return;
        }
        let list = by.entry(key.to_string()).or_default();
        let one = KeyClient { app: c.app.clone(), client: c.client.clone(), addr: c.addr.clone(), calls: 1, failed: u64::from(failed), first_ms: t, last_ms: t, coordinators: Vec::new() };
        match list.iter_mut().find(|x| x.same(&one)) {
            Some(x) => {
                x.calls += 1;
                x.failed += u64::from(failed);
                x.last_ms = x.last_ms.max(t);
            }
            None => {
                if list.len() >= Self::PER_KEY {
                    if let Some(i) = list.iter().enumerate().min_by_key(|(_, x)| x.last_ms).map(|(i, _)| i) {
                        list.swap_remove(i);
                    }
                }
                list.push(one);
            }
        }
    }

    /// Each key's clients seen since `since_ms`, the most used first.
    pub fn json(&self, since_ms: u64) -> Json {
        let t = now_ms();
        let mut by = lock(&self.by_key);
        by.retain(|_, l| {
            l.retain(|x| t.saturating_sub(x.last_ms) <= Self::KEEP_MS);
            !l.is_empty()
        });
        let keys: serde_json::Map<String, Json> = by
            .iter()
            .map(|(k, l)| {
                let mut l: Vec<&KeyClient> = l.iter().filter(|x| x.last_ms >= since_ms).collect();
                l.sort_by(|a, b| b.calls.cmp(&a.calls).then(b.last_ms.cmp(&a.last_ms)));
                (k.clone(), json!(l))
            })
            .filter(|(_, l)| l.as_array().is_some_and(|a| !a.is_empty()))
            .collect();
        json!({"keys": keys})
    }

    /// Several coordinators' [`Self::json`] answers as one (`parts`: each
    /// one's name and answer): a client of a key, by its app, client and
    /// address, with its calls summed and the coordinators it went through.
    pub fn merge(parts: &[(String, Json)]) -> Json {
        let mut by: BTreeMap<String, Vec<KeyClient>> = BTreeMap::new();
        for (name, j) in parts {
            for (k, l) in j["keys"].as_object().into_iter().flatten() {
                let list = by.entry(k.clone()).or_default();
                for mut c in l.as_array().into_iter().flatten().filter_map(|c| serde_json::from_value::<KeyClient>(c.clone()).ok()) {
                    match list.iter_mut().find(|x| x.same(&c)) {
                        Some(x) => {
                            x.calls += c.calls;
                            x.failed += c.failed;
                            x.first_ms = x.first_ms.min(c.first_ms);
                            x.last_ms = x.last_ms.max(c.last_ms);
                            if !x.coordinators.contains(name) {
                                x.coordinators.push(name.clone());
                            }
                        }
                        None => {
                            c.coordinators = vec![name.clone()];
                            list.push(c);
                        }
                    }
                }
            }
        }
        for l in by.values_mut() {
            l.sort_by(|a, b| b.calls.cmp(&a.calls).then(b.last_ms.cmp(&a.last_ms)));
        }
        json!({"keys": by})
    }
}

/// Queries [`QueryLog`] keeps of the latest, and of the slow.
pub const RECENT: usize = 500;
pub const SLOW: usize = 200;

/// Plan lines a kept query keeps.
pub const PLAN_LINES: usize = 400;

impl Default for QueryLog {
    fn default() -> QueryLog {
        QueryLog { next: AtomicU64::new(0), recent: Mutex::new(VecDeque::with_capacity(RECENT)), slow: Mutex::new(VecDeque::new()), statements: Statements::default(), clients: KeyClients::default(), keep: std::sync::OnceLock::new() }
    }
}

impl QueryLog {
    /// Keeps a query that ended (in the slow ones too if `slow`), with
    /// what it ran (`stats`, its steps from the request's start) and
    /// whether its CPU time was measured (`profiled`).
    #[allow(clippy::too_many_arguments)]
    pub fn add(&self, sql: &str, kind: &'static str, outcome: &'static str, secs: f64, rows: usize, shard: bool, error: Option<&str>, slow: bool, stats: Option<&ExecStats>, profiled: bool, meta: &Meta) {
        let fingerprint = fingerprint(sql);
        let f = Arc::new(Finished {
            id: self.next.fetch_add(1, Ordering::Relaxed) + 1,
            ts_ms: now_ms(),
            request_id: crate::obs::current(),
            sql: crate::obs::truncate(sql, 2000).into_owned(),
            fingerprint,
            kind,
            outcome,
            ms: secs * 1e3,
            rows,
            shard,
            error: error.map(|e| crate::obs::truncate(e, 500).into_owned()),
            deadline_ms: meta.deadline_ms,
            caller: meta.caller.as_deref().cloned().unwrap_or_default(),
            params: meta.params.clone(),
            trace: trace().map(|t| t.spans()).unwrap_or_default(),
            steps: stats.map(|s| s.steps.clone()).unwrap_or_default(),
            profiled,
            plan: stats.map(|s| s.plan.iter().take(PLAN_LINES).cloned().collect()).unwrap_or_default(),
            filter_docs: stats.map_or(0, |s| s.filter_docs),
            scored: stats.map_or(0, |s| s.scored),
        });
        self.statements.add(&f);
        if !f.shard {
            self.clients.add(&f.caller, f.outcome != "ok", f.ts_ms);
        }
        if let Some(keep) = self.keep.get() {
            keep(&f, slow);
        }
        push(&self.recent, f.clone(), RECENT);
        if slow {
            push(&self.slow, f, SLOW);
        }
    }

    /// The latest queries, the newest first: those after `since_ms`, at
    /// most `limit`.
    pub fn recent(&self, since_ms: u64, limit: usize) -> Vec<Arc<Finished>> {
        lock(&self.recent).iter().rev().filter(|f| f.ts_ms > since_ms).take(limit).cloned().collect()
    }

    pub fn slow(&self, limit: usize) -> Vec<Arc<Finished>> {
        lock(&self.slow).iter().rev().take(limit).cloned().collect()
    }

    /// The query numbered `id` ([`Finished::id`]), if it is among the
    /// latest or the slow ones still.
    pub fn finished(&self, id: u64) -> Option<Arc<Finished>> {
        let find = |q: &Mutex<VecDeque<Arc<Finished>>>| lock(q).iter().rev().find(|f| f.id == id).cloned();
        find(&self.recent).or_else(|| find(&self.slow))
    }

    /// The query with request id `id`, as a coordinator asked or a client.
    pub fn by_request(&self, id: &str) -> Vec<Arc<Finished>> {
        lock(&self.recent).iter().rev().filter(|f| f.request_id.as_deref() == Some(id)).cloned().collect()
    }
}

fn push<T>(q: &Mutex<VecDeque<T>>, x: T, cap: usize) {
    let mut q = lock(q);
    if q.len() == cap {
        q.pop_front();
    }
    q.push_back(x);
}

/// A statement's shape: its text with every literal (string, number, a
/// list of them) replaced by `?`, and its spaces collapsed, so that
/// queries differing only in their values count as one statement.
pub fn fingerprint(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len().min(512));
    let b = sql.as_bytes();
    let mut i = 0;
    let mut space = false;
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() && out.len() < 1000 {
        let c = b[i];
        if c.is_ascii_whitespace() {
            space = !out.is_empty();
            i += 1;
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        if c == b'\'' {
            // A string, '' inside it an escaped quote.
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            out.push('?');
        } else if c == b'"' {
            // A quoted identifier, kept.
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push_str(&sql[start..i]);
        } else if (c.is_ascii_digit() || (c == b'-' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) && !out.ends_with(|p: char| p.is_ascii_alphanumeric() || p == ')'))) && !out.ends_with(|p: char| p.is_ascii_alphanumeric() || p == '_') {
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || ((b[i] == b'-' || b[i] == b'+') && matches!(b[i - 1], b'e' | b'E'))) {
                i += 1;
            }
            out.push('?');
        } else if ident(c) {
            let start = i;
            while i < b.len() && ident(b[i]) {
                i += 1;
            }
            out.push_str(&sql[start..i]);
        } else {
            // Any other character, whole.
            let ch = sql[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    collapse_lists(&out)
}

/// `(?, ?, ?)` as `(?...)`, `[?, ?]` as `[?...]`: a list of values is one
/// statement whatever its length.
fn collapse_lists(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(['(', '[']) {
        out.push_str(&rest[..=i]);
        let close = if rest.as_bytes()[i] == b'(' { ')' } else { ']' };
        let after = &rest[i + 1..];
        let end = after.find(close);
        match end {
            Some(e) if e > 1 && after[..e].split(',').all(|p| p.trim() == "?") && after[..e].contains(',') => {
                out.push_str("?...");
                rest = &after[e..];
            }
            _ => rest = after,
        }
    }
    out.push_str(rest);
    out
}

/// Statistics per statement ([`fingerprint`]), a bucket a minute, kept
/// for [`STATEMENT_MINUTES`]: any window of the last day reads its
/// statements, and compares them with the window before
/// ([`Statements::window`]). Each statement also keeps its latest calls
/// and its slowest of the day, with who sent them and their parameters
/// ([`Call`]).
pub struct Statements {
    inner: Mutex<Stmts>,
    /// Since when it counts (the first query, or the last reset).
    since_ms: AtomicU64,
}

impl Default for Statements {
    fn default() -> Statements {
        Statements { inner: Mutex::new(Stmts::default()), since_ms: AtomicU64::new(now_ms()) }
    }
}

/// Statements known at most; past that, the one seen least recently
/// goes.
pub const STATEMENTS: usize = 2000;
/// Minutes of buckets kept: a day.
pub const STATEMENT_MINUTES: usize = 1440;
/// Statements a minute's bucket holds at most; the rest count as one.
const PER_MINUTE: usize = 2000;
/// What the statements past [`PER_MINUTE`] count as.
const OTHER: &str = "(other statements)";
/// Keys a statement's minute counts apart at most; the rest as one.
const KEYS: usize = 16;
/// Error messages a statement's minute counts apart at most.
const ERRORS: usize = 5;
/// Calls each statement keeps: its latest, and its slowest of the day.
const LATEST_CALLS: usize = 32;
const SLOWEST_CALLS: usize = 16;

/// Latency bounds of a statement's histogram, in milliseconds.
const STMT_BUCKETS: [f64; 15] = [0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0, 30_000.0];

#[derive(Default)]
struct Stmts {
    known: HashMap<String, Known>,
    /// The oldest first.
    minutes: VecDeque<Minute>,
}

struct Known {
    id: String,
    example: String,
    kind: &'static str,
    first_seen_ms: u64,
    last_seen_ms: u64,
    latest: VecDeque<Arc<Call>>,
    slowest: Vec<Arc<Call>>,
}

struct Minute {
    t: u64,
    by: HashMap<String, Agg>,
}

/// One call of a statement, as its page shows it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Call {
    /// Its number in the query log (`GET /v1/queries/finished/:id`).
    pub id: u64,
    pub ts_ms: u64,
    pub ms: f64,
    pub outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Arc<str>>,
    #[serde(flatten)]
    pub caller: Caller,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Arc<Json>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub rows: usize,
    /// On a coordinator: each shard's part, (shard, ms, outcome).
    #[serde(skip)]
    shards: Vec<(usize, f64, &'static str)>,
}

#[derive(Clone, Default)]
struct Agg {
    calls: u64,
    outcomes: [u64; OUTCOMES.len()],
    total_ms: f64,
    max_ms: f64,
    rows: u64,
    shard_calls: u64,
    /// Calls the command center made (`x-strata-client: command-center`).
    command_center: u64,
    hist: [u64; STMT_BUCKETS.len() + 1],
    keys: Vec<KeyAgg>,
    errors: Vec<(String, u64, u64)>,
}

#[derive(Clone, Default)]
struct KeyAgg {
    key: String,
    calls: u64,
    total_ms: f64,
    errors: u64,
}

impl Agg {
    fn add(&mut self, f: &Finished) {
        self.calls += 1;
        if let Some(i) = OUTCOMES.iter().position(|o| *o == f.outcome) {
            self.outcomes[i] += 1;
        }
        self.total_ms += f.ms;
        self.max_ms = self.max_ms.max(f.ms);
        self.rows += f.rows as u64;
        self.shard_calls += f.shard as u64;
        self.command_center += f.caller.is_command_center() as u64;
        let b = STMT_BUCKETS.iter().position(|&b| f.ms <= b).unwrap_or(STMT_BUCKETS.len());
        self.hist[b] += 1;
        let key = f.caller.key.clone().unwrap_or_else(|| "-".into());
        let failed = failed(f.outcome) as u64;
        let at = match self.keys.iter().position(|k| k.key == key) {
            Some(i) => i,
            None if self.keys.len() < KEYS => {
                self.keys.push(KeyAgg { key, ..Default::default() });
                self.keys.len() - 1
            }
            None => match self.keys.iter().position(|k| k.key == "(other keys)") {
                Some(i) => i,
                None => {
                    self.keys.push(KeyAgg { key: "(other keys)".into(), ..Default::default() });
                    self.keys.len() - 1
                }
            },
        };
        let k = &mut self.keys[at];
        (k.calls, k.total_ms, k.errors) = (k.calls + 1, k.total_ms + f.ms, k.errors + failed);
        if let Some(e) = &f.error {
            let m = crate::obs::truncate(e, 300).into_owned();
            match self.errors.iter().position(|x| x.0 == m) {
                Some(i) => (self.errors[i].1, self.errors[i].2) = (self.errors[i].1 + 1, f.ts_ms),
                None if self.errors.len() < ERRORS => self.errors.push((m, 1, f.ts_ms)),
                None => {}
            }
        }
    }

    fn merge(&mut self, o: &Agg) {
        self.calls += o.calls;
        for (a, b) in self.outcomes.iter_mut().zip(o.outcomes) {
            *a += b;
        }
        self.total_ms += o.total_ms;
        self.max_ms = self.max_ms.max(o.max_ms);
        self.rows += o.rows;
        self.shard_calls += o.shard_calls;
        self.command_center += o.command_center;
        for (a, b) in self.hist.iter_mut().zip(o.hist) {
            *a += b;
        }
        for k in &o.keys {
            match self.keys.iter_mut().find(|x| x.key == k.key) {
                Some(x) => (x.calls, x.total_ms, x.errors) = (x.calls + k.calls, x.total_ms + k.total_ms, x.errors + k.errors),
                None => self.keys.push(k.clone()),
            }
        }
        for e in &o.errors {
            match self.errors.iter_mut().find(|x| x.0 == e.0) {
                Some(x) => (x.1, x.2) = (x.1 + e.1, x.2.max(e.2)),
                None => self.errors.push(e.clone()),
            }
        }
    }

    fn failed(&self) -> u64 {
        OUTCOMES.iter().zip(self.outcomes).filter(|(o, _)| failed(o)).map(|(_, n)| n).sum()
    }

    /// Its counts as they go to another coordinator, which merges them
    /// ([`Statements::window_raw`]).
    fn raw(&self) -> Json {
        json!({
            "calls": self.calls, "outcomes": self.outcomes.to_vec(), "total_ms": self.total_ms, "max_ms": self.max_ms, "rows": self.rows,
            "shard_calls": self.shard_calls, "command_center": self.command_center, "hist": self.hist.to_vec(),
            "keys": self.keys.iter().map(|k| json!([k.key, k.calls, k.total_ms, k.errors])).collect::<Vec<_>>(),
            "errors": self.errors.iter().map(|e| json!([e.0, e.1, e.2])).collect::<Vec<_>>(),
        })
    }

    /// What [`Agg::raw`] wrote (what it cannot read counts as nothing).
    fn from_raw(j: &Json) -> Agg {
        let u = |k: &str| j[k].as_u64().unwrap_or(0);
        let f = |k: &str| j[k].as_f64().unwrap_or(0.0);
        let mut a = Agg { calls: u("calls"), total_ms: f("total_ms"), max_ms: f("max_ms"), rows: u("rows"), shard_calls: u("shard_calls"), command_center: u("command_center"), ..Default::default() };
        for (x, v) in a.outcomes.iter_mut().zip(j["outcomes"].as_array().into_iter().flatten()) {
            *x = v.as_u64().unwrap_or(0);
        }
        for (x, v) in a.hist.iter_mut().zip(j["hist"].as_array().into_iter().flatten()) {
            *x = v.as_u64().unwrap_or(0);
        }
        for k in j["keys"].as_array().into_iter().flatten() {
            a.keys.push(KeyAgg { key: k[0].as_str().unwrap_or("-").into(), calls: k[1].as_u64().unwrap_or(0), total_ms: k[2].as_f64().unwrap_or(0.0), errors: k[3].as_u64().unwrap_or(0) });
        }
        for e in j["errors"].as_array().into_iter().flatten() {
            a.errors.push((e[0].as_str().unwrap_or("").into(), e[1].as_u64().unwrap_or(0), e[2].as_u64().unwrap_or(0)));
        }
        a
    }

    fn server_failed(&self) -> u64 {
        OUTCOMES.iter().zip(self.outcomes).filter(|(o, _)| server_failed(o)).map(|(_, n)| n).sum()
    }

    fn p(&self, q: f64) -> Option<f64> {
        (self.calls > 0).then(|| quantile_ms(&self.hist, q, self.max_ms))
    }

    fn outcomes_json(&self) -> Json {
        Json::Object(OUTCOMES.iter().zip(self.outcomes).filter(|(_, n)| *n > 0).map(|(o, n)| (o.to_string(), json!(n))).collect())
    }

    fn keys_json(&self) -> Json {
        let mut k = self.keys.clone();
        k.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
        Json::Array(k.iter().map(|k| json!({"key": k.key, "calls": k.calls, "total_ms": k.total_ms, "errors": k.errors})).collect())
    }
}

/// A short id of a statement: 16 hex digits of its fingerprint's hash,
/// for its page's URL.
pub fn statement_id(fp: &str) -> String {
    // FNV-1a: the same on every server and in every version.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in fp.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// The shards a coordinator's query asked, from its trace: each shard's
/// slowest call that was not outrun, and how it ended.
fn shard_parts(trace: &[Span]) -> Vec<(usize, f64, &'static str)> {
    let mut out: Vec<(usize, f64, &'static str)> = Vec::new();
    for s in trace.iter().filter(|s| s.outcome != "outrun") {
        let Some(i) = s.shard else { continue };
        let ms = s.dur_us as f64 / 1e3;
        match out.iter_mut().find(|x| x.0 == i) {
            Some(x) if ms > x.1 => (x.1, x.2) = (ms, s.outcome),
            Some(_) => {}
            None => out.push((i, ms, s.outcome)),
        }
    }
    out
}

/// The time window `[from, to)` a request names (ms), by default the 15
/// minutes before now.
pub fn window_of(from: Option<u64>, to: Option<u64>) -> (u64, u64) {
    let to = to.filter(|&t| t > 0).unwrap_or_else(now_ms);
    let from = from.filter(|&f| f > 0 && f < to).unwrap_or(to.saturating_sub(15 * 60_000));
    (from, to)
}

impl Statements {
    /// Counts a query that ended.
    pub fn add(&self, f: &Arc<Finished>) {
        let minute = f.ts_ms / 60_000 * 60_000;
        let mut s = lock(&self.inner);
        let s = &mut *s;
        if !s.known.contains_key(&f.fingerprint) && s.known.len() >= STATEMENTS {
            if let Some(k) = s.known.iter().min_by_key(|(_, k)| k.last_seen_ms).map(|(k, _)| k.clone()) {
                s.known.remove(&k);
            }
        }
        let k = s.known.entry(f.fingerprint.clone()).or_insert_with(|| Known {
            id: statement_id(&f.fingerprint),
            example: f.sql.clone(),
            kind: f.kind,
            first_seen_ms: f.ts_ms,
            last_seen_ms: f.ts_ms,
            latest: VecDeque::new(),
            slowest: Vec::new(),
        });
        k.last_seen_ms = k.last_seen_ms.max(f.ts_ms);
        let call = Arc::new(Call {
            id: f.id,
            ts_ms: f.ts_ms,
            ms: f.ms,
            outcome: f.outcome,
            request_id: f.request_id.clone(),
            caller: f.caller.clone(),
            params: f.params.clone(),
            error: f.error.clone(),
            rows: f.rows,
            shards: shard_parts(&f.trace),
        });
        if k.latest.len() == LATEST_CALLS {
            k.latest.pop_front();
        }
        k.latest.push_back(call.clone());
        k.slowest.retain(|c| c.ts_ms + (STATEMENT_MINUTES as u64) * 60_000 > f.ts_ms);
        if k.slowest.len() < SLOWEST_CALLS {
            k.slowest.push(call);
        } else if let Some(i) = k.slowest.iter().enumerate().min_by(|a, b| a.1.ms.total_cmp(&b.1.ms)).map(|(i, _)| i).filter(|&i| k.slowest[i].ms < f.ms) {
            k.slowest[i] = call;
        }
        // The minutes stay in order: a query that ended in an earlier
        // minute than the last counted (by a few milliseconds, on another
        // thread) counts in its own.
        let at = match s.minutes.iter().rposition(|m| m.t <= minute) {
            Some(i) if s.minutes[i].t == minute => i,
            i => {
                let i = i.map_or(0, |i| i + 1);
                s.minutes.insert(i, Minute { t: minute, by: HashMap::new() });
                i
            }
        };
        let newest = s.minutes.back().map_or(minute, |m| m.t);
        let mut at = at;
        while s.minutes.len() > STATEMENT_MINUTES || s.minutes.front().is_some_and(|m| m.t + (STATEMENT_MINUTES as u64) * 60_000 <= newest) {
            s.minutes.pop_front();
            if at == 0 {
                return;
            }
            at -= 1;
        }
        let m = &mut s.minutes[at];
        let fp = if m.by.len() >= PER_MINUTE && !m.by.contains_key(&f.fingerprint) { OTHER.to_string() } else { f.fingerprint.clone() };
        m.by.entry(fp).or_default().add(f);
    }

    /// Since when it counts.
    pub fn since(&self) -> u64 {
        self.since_ms.load(Ordering::Relaxed)
    }

    /// Each key's calls in the minutes of `[from, to)`, how many failed
    /// (any way: its own errors and deadlines included) and the
    /// milliseconds they ran within `[from, to)`: of the minute that began
    /// before `from`, the share of its time after `from` (as if its calls
    /// were spread over it), so that a burst stops counting as the window
    /// leaves it rather than a minute later. The key `-` is calls without
    /// one (alerts agent).
    pub fn key_calls(&self, from: u64, to: u64) -> Vec<(String, u64, u64, f64)> {
        let s = lock(&self.inner);
        let mut out: Vec<(String, u64, u64, f64)> = Vec::new();
        let lo = from / 60_000 * 60_000;
        for m in s.minutes.iter().filter(|m| m.t >= lo && m.t < to) {
            let part = if m.t < from { (m.t + 60_000 - from) as f64 / 60_000.0 } else { 1.0 };
            for a in m.by.values() {
                for k in &a.keys {
                    match out.iter_mut().find(|x| x.0 == k.key) {
                        Some(x) => (x.1, x.2, x.3) = (x.1 + k.calls, x.2 + k.errors, x.3 + k.total_ms * part),
                        None => out.push((k.key.clone(), k.calls, k.errors, k.total_ms * part)),
                    }
                }
            }
        }
        out
    }

    /// Each statement's minutes in `[from, to)`, merged.
    fn sum(s: &Stmts, from: u64, to: u64, only: Option<&str>) -> HashMap<String, Agg> {
        let mut out: HashMap<String, Agg> = HashMap::new();
        let lo = from / 60_000 * 60_000;
        for m in s.minutes.iter().filter(|m| m.t >= lo && m.t < to) {
            for (fp, a) in &m.by {
                if only.is_some_and(|o| o != fp) {
                    continue;
                }
                out.entry(fp.clone()).or_default().merge(a);
            }
        }
        out
    }

    fn statement_json(&self, s: &Stmts, fp: &str, a: &Agg, prev: Option<&Agg>, (from, to): (u64, u64), total: f64) -> Json {
        let k = s.known.get(fp);
        let since = self.since();
        let first = k.map(|k| k.first_seen_ms);
        let latest = k.and_then(|k| k.latest.iter().rev().find(|c| c.ts_ms < to).cloned());
        json!({
            "id": k.map_or_else(|| statement_id(fp), |k| k.id.clone()),
            "fingerprint": fp,
            "sql": k.map_or(fp, |k| k.example.as_str()),
            "kind": k.map(|k| k.kind),
            "calls": a.calls,
            "outcomes": a.outcomes_json(),
            "errors": a.failed(),
            "server_errors": a.server_failed(),
            "total_ms": a.total_ms,
            "mean_ms": a.total_ms / a.calls.max(1) as f64,
            "p50_ms": a.p(0.5), "p95_ms": a.p(0.95), "p99_ms": a.p(0.99), "max_ms": a.max_ms,
            "rows": a.rows, "rows_per_call": a.rows as f64 / a.calls.max(1) as f64, "shard_calls": a.shard_calls,
            "share": if total > 0.0 { a.total_ms / total } else { 0.0 },
            "first_seen_ms": first,
            "last_seen_ms": k.map(|k| k.last_seen_ms.min(to)),
            "prev": prev.map(|p| json!({"calls": p.calls, "p99_ms": p.p(0.99), "errors": p.failed(), "total_ms": p.total_ms, "mean_ms": p.total_ms / p.calls.max(1) as f64})),
            "keys": a.keys_json(),
            // The command center's own calls of it, and all of them its own.
            "clients": {"command-center": a.command_center},
            "command_center": a.calls > 0 && a.command_center == a.calls,
            // New: first seen in the window, while statements were counted
            // before it.
            "new": first.is_some_and(|f| f >= from) && since + 60_000 < from,
            "sample": latest.map(|c| json!({"ts_ms": c.ts_ms, "params": c.params, "key": c.caller.key, "key_id": c.caller.key_id, "app": c.caller.app, "client": c.caller.client, "addr": c.caller.addr})),
        })
    }

    /// The statements of window `[from, to)`, by time spent, the most
    /// first, `limit` at most; each with the window before (`prev`, as
    /// long), its keys, and whether it is new.
    pub fn window(&self, from: u64, to: u64, limit: usize) -> Json {
        let s = lock(&self.inner);
        let len = to.saturating_sub(from).max(60_000);
        let now = Self::sum(&s, from, to, None);
        let before = Self::sum(&s, from.saturating_sub(len), from, None);
        let total: f64 = now.values().map(|a| a.total_ms).sum();
        let mut all: Vec<(&String, &Agg)> = now.iter().collect();
        all.sort_by(|a, b| b.1.total_ms.total_cmp(&a.1.total_ms));
        let rows: Vec<Json> = all.iter().take(limit).map(|(fp, a)| self.statement_json(&s, fp, a, before.get(*fp), (from, to), total)).collect();
        let held = s.minutes.front().map_or(self.since(), |m| m.t.max(self.since()));
        json!({
            "statements": rows, "count": now.len(), "from": from, "to": to, "prev": {"from": from.saturating_sub(len), "to": from},
            "since_ms": self.since(), "held_from_ms": held, "source": "memory", "buckets_ms": STMT_BUCKETS,
        })
    }

    /// The statements of `[from, to)` and of the window before, as counts
    /// another coordinator merges with its own ([`Statements::window_merged`]):
    /// `{"statements": {fingerprint: {now, before, id, sql, kind,
    /// first_seen_ms, last_seen_ms, sample}}}`; `only` one fingerprint.
    pub fn window_raw(&self, from: u64, to: u64, only: Option<&str>) -> Json {
        let s = lock(&self.inner);
        let len = to.saturating_sub(from).max(60_000);
        let now = Self::sum(&s, from, to, only);
        let before = Self::sum(&s, from.saturating_sub(len), from, only);
        let total: f64 = now.values().map(|a| a.total_ms).sum();
        let mut out = serde_json::Map::new();
        for (fp, a) in &now {
            let mut j = self.statement_json(&s, fp, a, None, (from, to), total);
            let o = j.as_object_mut().expect("an object");
            o.retain(|k, _| ["id", "sql", "kind", "first_seen_ms", "last_seen_ms", "sample"].contains(&k.as_str()));
            o.insert("now".into(), a.raw());
            o.insert("before".into(), before.get(fp).map_or(Json::Null, Agg::raw));
            out.insert(fp.clone(), j);
        }
        json!({"statements": out, "since_ms": self.since(), "held_from_ms": s.minutes.front().map_or(self.since(), |m| m.t.max(self.since()))})
    }

    /// [`Statements::window`] with other coordinators' counts added
    /// (`others`: each one's name and its [`Statements::window_raw`]), so
    /// that a statement sent through any coordinator is in it, each
    /// statement saying by how many calls each coordinator (`coordinators`).
    pub fn window_merged(&self, from: u64, to: u64, limit: usize, others: &[(String, Json)], this: &str) -> Json {
        let s = lock(&self.inner);
        let len = to.saturating_sub(from).max(60_000);
        let mut now = Self::sum(&s, from, to, None);
        let mut before = Self::sum(&s, from.saturating_sub(len), from, None);
        let mut by: HashMap<String, Vec<(String, u64)>> = now.iter().map(|(fp, a)| (fp.clone(), vec![(this.to_string(), a.calls)])).collect();
        let mut info: HashMap<String, Json> = HashMap::new();
        let mut held = s.minutes.front().map_or(self.since(), |m| m.t.max(self.since()));
        for (name, j) in others {
            held = held.max(j["held_from_ms"].as_u64().unwrap_or(0));
            for (fp, x) in j["statements"].as_object().into_iter().flatten() {
                let a = Agg::from_raw(&x["now"]);
                by.entry(fp.clone()).or_default().push((name.clone(), a.calls));
                now.entry(fp.clone()).or_default().merge(&a);
                if !x["before"].is_null() {
                    before.entry(fp.clone()).or_default().merge(&Agg::from_raw(&x["before"]));
                }
                // What this coordinator does not know of it, and the latest call of any.
                let i = info.entry(fp.clone()).or_insert_with(|| x.clone());
                for (k, pick_min) in [("first_seen_ms", true), ("last_seen_ms", false)] {
                    if let (Some(a), Some(b)) = (i[k].as_u64(), x[k].as_u64()) {
                        i[k] = json!(if pick_min { a.min(b) } else { a.max(b) });
                    }
                }
                if x["sample"]["ts_ms"].as_u64() > i["sample"]["ts_ms"].as_u64() {
                    i["sample"] = x["sample"].clone();
                }
            }
        }
        let total: f64 = now.values().map(|a| a.total_ms).sum();
        let mut all: Vec<(&String, &Agg)> = now.iter().collect();
        all.sort_by(|a, b| b.1.total_ms.total_cmp(&a.1.total_ms));
        let rows: Vec<Json> = all
            .iter()
            .take(limit)
            .map(|(fp, a)| {
                let mut j = self.statement_json(&s, fp, a, before.get(*fp), (from, to), total);
                if let Some(i) = info.get(*fp) {
                    let known = s.known.contains_key(*fp);
                    for k in ["id", "sql", "kind", "first_seen_ms", "last_seen_ms", "sample"] {
                        let take = !known || match k {
                            "first_seen_ms" => i[k].as_u64() < j[k].as_u64(),
                            "last_seen_ms" => i[k].as_u64() > j[k].as_u64(),
                            "sample" => i[k]["ts_ms"].as_u64() > j[k]["ts_ms"].as_u64(),
                            _ => false,
                        };
                        if take && !i[k].is_null() {
                            j[k] = i[k].clone();
                        }
                    }
                }
                j["coordinators"] = Json::Array(by.get(*fp).into_iter().flatten().map(|(n, c)| json!({"name": n, "calls": c})).collect());
                j
            })
            .collect();
        json!({
            "statements": rows, "count": now.len(), "from": from, "to": to, "prev": {"from": from.saturating_sub(len), "to": from},
            "since_ms": self.since(), "held_from_ms": held, "source": "memory", "buckets_ms": STMT_BUCKETS,
        })
    }

    /// One statement (by its id or its fingerprint) over `[from, to)`:
    /// its numbers, a point a minute, its keys, its shards (from its calls
    /// kept), its slowest calls and its errors.
    pub fn detail(&self, which: &str, from: u64, to: u64) -> Option<Json> {
        let s = lock(&self.inner);
        let fp = match s.known.iter().find(|(fp, k)| k.id == which || fp.as_str() == which) {
            Some((fp, _)) => fp.clone(),
            None => return None,
        };
        let len = to.saturating_sub(from).max(60_000);
        let mut all = Self::sum(&s, from, to, None);
        // Its share of the window's time, over every statement (as the
        // list gives it).
        let total: f64 = all.values().map(|a| a.total_ms).sum();
        let a = all.remove(&fp).unwrap_or_default();
        let before = Self::sum(&s, from.saturating_sub(len), from, Some(&fp)).remove(&fp);
        let statement = self.statement_json(&s, &fp, &a, before.as_ref(), (from, to), total);
        let lo = from / 60_000 * 60_000;
        let trend: Vec<Json> = s
            .minutes
            .iter()
            .filter(|m| m.t >= lo && m.t < to)
            .filter_map(|m| m.by.get(&fp).map(|x| (m.t, x)))
            .map(|(t, x)| json!({"t": t, "calls": x.calls, "p99_ms": x.p(0.99), "p50_ms": x.p(0.5), "errors": x.failed(), "mean_ms": x.total_ms / x.calls.max(1) as f64, "max_ms": x.max_ms}))
            .collect();
        let k = &s.known[&fp];
        let mut calls: Vec<Arc<Call>> = k.latest.iter().chain(k.slowest.iter()).filter(|c| c.ts_ms >= from && c.ts_ms < to).cloned().collect();
        calls.sort_by_key(|c| c.id);
        calls.dedup_by_key(|c| c.id);
        let mut by_shard: std::collections::BTreeMap<usize, (u64, u64, Vec<f64>)> = std::collections::BTreeMap::new();
        for c in &calls {
            for (shard, ms, outcome) in &c.shards {
                let e = by_shard.entry(*shard).or_default();
                e.0 += 1;
                e.1 += (*outcome != "ok") as u64;
                e.2.push(*ms);
            }
        }
        let by_shard: Vec<Json> = by_shard
            .into_iter()
            .map(|(shard, (n, errors, mut ms))| {
                ms.sort_by(f64::total_cmp);
                let p99 = ms[((ms.len() as f64 * 0.99).ceil() as usize).clamp(1, ms.len()) - 1];
                json!({"shard": shard, "calls": n, "errors": errors, "p99_ms": p99, "max_ms": ms.last(), "mean_ms": ms.iter().sum::<f64>() / n as f64})
            })
            .collect();
        let mut slowest = calls.clone();
        slowest.sort_by(|a, b| b.ms.total_cmp(&a.ms));
        slowest.truncate(10);
        let mut latest = calls;
        latest.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
        latest.truncate(20);
        let mut errors = a.errors.clone();
        errors.sort_by(|x, y| y.1.cmp(&x.1));
        Some(json!({
            "statement": statement, "from": from, "to": to, "trend": trend, "by_key": a.keys_json(),
            "by_shard": by_shard, "by_shard_from_calls": latest.len().max(slowest.len()),
            "slowest": slowest, "latest": latest,
            "errors": errors.iter().map(|(m, n, last)| json!({"message": m, "count": n, "last_ms": last})).collect::<Vec<_>>(),
            "source": "memory",
        }))
    }

    /// Starts counting again (this server's statements only).
    pub fn reset(&self) {
        *lock(&self.inner) = Stmts::default();
        self.since_ms.store(now_ms(), Ordering::Relaxed);
    }
}

/// The `q` quantile of a histogram over [`STMT_BUCKETS`], interpolated
/// within its bucket, and at most `max_ms`, the slowest counted.
fn quantile_ms(hist: &[u64], q: f64, max_ms: f64) -> f64 {
    quantile_within(&STMT_BUCKETS, hist, q, Some(max_ms)).unwrap_or(0.0)
}

/// The `q` quantile of counts per bucket (`counts[i]` at most `bounds[i]`,
/// the last count past every bound), interpolated linearly within its
/// bucket; `None` without counts. Past the last bound, the last bound.
pub fn quantile(bounds: &[f64], counts: &[u64], q: f64) -> Option<f64> {
    let n: u64 = counts.iter().sum();
    if n == 0 {
        return None;
    }
    let want = (q * n as f64).max(1.0);
    let mut cum = 0.0;
    for (i, &c) in counts.iter().enumerate() {
        let next = cum + c as f64;
        if next >= want && c > 0 {
            let lo = if i == 0 { 0.0 } else { bounds[i - 1] };
            let Some(&hi) = bounds.get(i) else { return bounds.last().copied() };
            return Some(lo + (hi - lo) * ((want - cum) / c as f64));
        }
        cum = next;
    }
    bounds.last().copied()
}

/// The `q` quantile of counts per bucket as [`quantile`] gives it, but
/// never above `max`, the largest value counted: one in the last bucket
/// (past every bound) is placed between the last bound and `max`.
pub fn quantile_within(bounds: &[f64], counts: &[u64], q: f64, max: Option<f64>) -> Option<f64> {
    let Some(max) = max else { return quantile(bounds, counts, q) };
    let n: u64 = counts.iter().sum();
    if n == 0 {
        return None;
    }
    let past = counts.get(bounds.len()).copied().unwrap_or(0);
    let want = (q * n as f64).max(1.0);
    let below = (n - past) as f64;
    let v = match (past > 0 && want > below, bounds.last()) {
        (true, Some(&last)) if max > last => last + (max - last) * ((want - below) / past as f64),
        _ => quantile(bounds, counts, q)?,
    };
    Some(v.min(max))
}

// ------------------------------------------------------------ the series

/// Cumulative counters and gauges at one moment ([`Service::sample`]):
/// the sampler turns two into a point of rates.
#[derive(Clone, Default, Debug)]
pub struct Sample {
    pub at: Option<Instant>,
    pub ts_ms: u64,
    /// Queries ended, and those that failed on the server's side (an
    /// error, or the server's deadline passed: `timeouts` of them), were
    /// the client's fault (a 4xx such as bad SQL), passed the client's own
    /// deadline, were refused (429), cancelled or answered partially.
    pub queries: u64,
    pub query_errors: u64,
    pub timeouts: u64,
    pub client_errors: u64,
    pub client_deadlines: u64,
    pub rejected: u64,
    pub cancelled: u64,
    pub partial: u64,
    /// The slowest query since the sample before, in seconds.
    pub query_max_s: Option<f64>,
    /// Queries ended by kind.
    pub by_kind: BTreeMap<&'static str, u64>,
    /// Query latencies, every kind together: counts per
    /// [`crate::metrics::BUCKETS`] bucket, the last past every bound.
    pub query_hist: Vec<u64>,
    pub http: u64,
    pub http_4xx: u64,
    pub http_5xx: u64,
    pub writes: u64,
    pub written_docs: u64,
    pub write_errors: u64,
    pub write_hist: Vec<u64>,
    pub slow: u64,
    /// The process's CPU time, in seconds.
    pub cpu_s: f64,
    /// Bytes allocated and allocations, since the process started (with
    /// the counting allocator, [`crate::heap`]).
    pub alloc_bytes: u64,
    pub allocs: u64,
    /// Gauges, by name.
    pub gauges: BTreeMap<&'static str, f64>,
    /// Values named at run time: the alert rules' queries
    /// ([`crate::alerts::refresh_sql`]), `sql_<rule>`.
    pub named: BTreeMap<String, f64>,
}

/// Points kept: an hour at the default interval.
pub const POINTS: usize = 1800;

/// How often the sampler samples by default.
pub const INTERVAL: Duration = Duration::from_secs(2);

/// The latest points.
pub struct Series {
    pub interval: Duration,
    points: Mutex<VecDeque<Arc<Json>>>,
    last: Mutex<Option<Sample>>,
}

impl Default for Series {
    fn default() -> Series {
        Series { interval: INTERVAL, points: Mutex::new(VecDeque::with_capacity(POINTS)), last: Mutex::new(None) }
    }
}

impl Series {
    /// Adds the point between the last sample and `s`, and keeps `s` for
    /// the next. The first sample makes no point.
    pub fn add(&self, s: Sample) -> Option<Arc<Json>> {
        let mut last = lock(&self.last);
        let point = last.as_ref().map(|prev| Arc::new(point(prev, &s)));
        *last = Some(s);
        drop(last);
        if let Some(p) = &point {
            push(&self.points, p.clone(), POINTS);
        }
        point
    }

    /// The points after `since_ms`, the oldest first.
    pub fn since(&self, since_ms: u64) -> Vec<Arc<Json>> {
        lock(&self.points).iter().filter(|p| p["t"].as_u64().unwrap_or(0) > since_ms).cloned().collect()
    }

    /// The latest point.
    pub fn last(&self) -> Option<Arc<Json>> {
        lock(&self.points).back().cloned()
    }
}

/// The load over `points` (a minute's, say): the mean `qps` and `cpu`
/// (cores busy), the slowest `p99_ms` and the latest `disk_used_ratio`;
/// `None` without points.
pub fn load_of(points: &[Arc<Json>]) -> Option<(f64, f64, Option<f64>, Option<f64>)> {
    if points.is_empty() {
        return None;
    }
    let n = points.len() as f64;
    let mean = |k: &str| points.iter().filter_map(|p| p[k].as_f64()).sum::<f64>() / n;
    let p99 = points.iter().filter_map(|p| p["p99_ms"].as_f64()).reduce(f64::max);
    let disk = points.iter().rev().find_map(|p| p["disk_used_ratio"].as_f64());
    Some((mean("qps"), mean("cpu"), p99, disk))
}

/// The rates between samples `a` and `b`, and `b`'s gauges. Counts of
/// the interval: `queries` ended, `query_errors` (failures on the
/// server's side: errors and the server's deadline), `client_errors` and
/// `client_deadlines`. `error_ratio` (server failures over queries) and
/// `availability` (one minus it) are left out when no query ended: no
/// query is no data, not none failing. No quantile exceeds `max_ms`, the
/// slowest query of the interval.
fn point(a: &Sample, b: &Sample) -> Json {
    let secs = match (a.at, b.at) {
        (Some(x), Some(y)) => y.saturating_duration_since(x).as_secs_f64(),
        _ => (b.ts_ms.saturating_sub(a.ts_ms)) as f64 / 1e3,
    }
    .max(1e-3);
    let rate = |x: u64, y: u64| y.saturating_sub(x) as f64 / secs;
    let delta = |x: &[u64], y: &[u64]| y.iter().enumerate().map(|(i, &v)| v.saturating_sub(x.get(i).copied().unwrap_or(0))).collect::<Vec<u64>>();
    let qh = delta(&a.query_hist, &b.query_hist);
    let wh = delta(&a.write_hist, &b.write_hist);
    let max_s = b.query_max_s.filter(|_| qh.iter().any(|&n| n > 0));
    let ms = |h: &[u64], q: f64, max: Option<f64>| quantile_within(&crate::metrics::BUCKETS, h, q, max).map(|s| s * 1e3);
    let by_kind: serde_json::Map<String, Json> =
        b.by_kind.iter().map(|(k, &n)| (k.to_string(), json!(rate(a.by_kind.get(k).copied().unwrap_or(0), n)))).filter(|(_, v)| v.as_f64().unwrap_or(0.0) > 0.0).collect();
    let ended = b.queries.saturating_sub(a.queries);
    let failed = b.query_errors.saturating_sub(a.query_errors);
    let mut p = json!({
        "t": b.ts_ms,
        "secs": secs,
        "qps": rate(a.queries, b.queries),
        "queries": ended,
        "query_errors": failed,
        "client_errors": b.client_errors.saturating_sub(a.client_errors),
        "client_deadlines": b.client_deadlines.saturating_sub(a.client_deadlines),
        "errors_s": rate(a.query_errors, b.query_errors),
        "timeouts_s": rate(a.timeouts, b.timeouts),
        "client_errors_s": rate(a.client_errors, b.client_errors),
        "client_deadlines_s": rate(a.client_deadlines, b.client_deadlines),
        "cancelled_s": rate(a.cancelled, b.cancelled),
        "rejected_s": rate(a.rejected, b.rejected),
        "partial_s": rate(a.partial, b.partial),
        "p50_ms": ms(&qh, 0.5, max_s), "p95_ms": ms(&qh, 0.95, max_s), "p99_ms": ms(&qh, 0.99, max_s),
        "http_s": rate(a.http, b.http), "http_4xx_s": rate(a.http_4xx, b.http_4xx), "http_5xx_s": rate(a.http_5xx, b.http_5xx),
        "writes_s": rate(a.writes, b.writes), "rows_s": rate(a.written_docs, b.written_docs), "write_errors_s": rate(a.write_errors, b.write_errors),
        "write_p99_ms": ms(&wh, 0.99, None),
        "slow_s": rate(a.slow, b.slow),
        "cpu": ((b.cpu_s - a.cpu_s) / secs).max(0.0),
        "alloc_bytes_s": rate(a.alloc_bytes, b.alloc_bytes), "allocs_s": rate(a.allocs, b.allocs),
        "by_kind": by_kind,
        "latency_hist": qh,
    });
    if ended > 0 {
        let ratio = failed as f64 / ended as f64;
        p["error_ratio"] = json!(ratio);
        p["availability"] = json!(1.0 - ratio);
    }
    if let Some(m) = max_s {
        p["max_ms"] = json!(m * 1e3);
    }
    for (k, v) in &b.gauges {
        p[*k] = json!(v);
    }
    for (k, v) in &b.named {
        p[k.as_str()] = json!(v);
    }
    p
}

// --------------------------------------------------------- the process

/// The process's resident memory in bytes, and its CPU time (user and
/// system) in seconds.
pub fn process_usage() -> (Option<u64>, f64) {
    // SAFETY: getrusage writes the struct it is given.
    let cpu = unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut u) == 0 {
            let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
            t(u.ru_utime) + t(u.ru_stime)
        } else {
            0.0
        }
    };
    (rss(), cpu)
}

#[cfg(target_os = "linux")]
fn rss() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some(pages * page.max(4096) as u64)
}

#[cfg(target_os = "macos")]
fn rss() -> Option<u64> {
    // SAFETY: getrusage writes the struct it is given. On macOS
    // ru_maxrss is the peak, in bytes.
    unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        (libc::getrusage(libc::RUSAGE_SELF, &mut u) == 0).then_some(u.ru_maxrss as u64)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rss() -> Option<u64> {
    None
}

/// The most resident memory the process has had, in bytes.
pub fn peak_rss() -> Option<u64> {
    // SAFETY: getrusage writes the struct it is given.
    let max = unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        (libc::getrusage(libc::RUSAGE_SELF, &mut u) == 0).then_some(u.ru_maxrss as u64)?
    };
    // Linux counts ru_maxrss in KiB, macOS in bytes.
    Some(if cfg!(target_os = "macos") { max } else { max * 1024 })
}

/// The space of the file system holding `path`: total, free, and
/// available to this user, in bytes.
pub fn disk(path: &std::path::Path) -> Option<(u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let dir = if path.is_dir() { path.to_path_buf() } else { path.parent().filter(|p| !p.as_os_str().is_empty()).map_or_else(|| std::path::PathBuf::from("."), |p| p.to_path_buf()) };
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs reads a NUL-terminated path and writes the struct.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return None;
        }
        let f = s.f_frsize as u64;
        Some((s.f_blocks as u64 * f, s.f_bfree as u64 * f, s.f_bavail as u64 * f))
    }
}

/// How full a file system is, as `df` counts it (CCW-14): what is used
/// over what is used and what this user may still write, so that the
/// blocks kept for root count neither as used nor as free. `None` for a
/// file system with no blocks.
pub fn disk_used_ratio(total: u64, free: u64, avail: u64) -> Option<f64> {
    let used = total.saturating_sub(free);
    (used + avail > 0).then(|| used as f64 / (used + avail) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CCW-14: root's reserved blocks are not counted as used: 40 GB used
    /// and 53 GB available on a 100 GB disk with 7 GB reserved is 43%
    /// used, as df says, not 47%.
    #[test]
    fn disk_used_as_df_counts_it() {
        let gb = 1u64 << 30;
        let r = disk_used_ratio(100 * gb, 60 * gb, 53 * gb).unwrap();
        assert!((r - 40.0 / 93.0).abs() < 1e-9, "{r}");
        assert_eq!(disk_used_ratio(100, 100, 100), Some(0.0));
        assert_eq!(disk_used_ratio(100, 5, 0), Some(1.0));
        assert_eq!(disk_used_ratio(0, 0, 0), None);
    }

    /// Every query with a key counts its client; coordinators' counts add
    /// up, each client naming the coordinators it went through (AC-4).
    #[test]
    fn a_key_s_clients_are_counted_and_merged() {
        let c = |app: &str, addr: &str| Caller { key_id: Some("k1".into()), app: Some(app.into()), addr: Some(addr.into()), ..Caller::default() };
        let (a, b) = (KeyClients::default(), KeyClients::default());
        let t = now_ms();
        for _ in 0..3 {
            a.add(&c("nightly", "10.0.0.7"), false, t);
        }
        a.add(&Caller::default(), false, t);
        b.add(&c("nightly", "10.0.0.7"), true, t + 5);
        b.add(&c("broken-app", "10.0.0.9"), true, t + 9);
        let ja = a.json(0);
        assert_eq!(ja["keys"]["k1"][0]["calls"], 3);
        assert!(ja["keys"].get("-").is_none(), "no key: not counted");
        let m = KeyClients::merge(&[("coord-a".into(), ja), ("coord-b".into(), b.json(0))]);
        let k1 = m["keys"]["k1"].as_array().unwrap();
        assert_eq!((k1[0]["app"].as_str(), k1[0]["calls"].as_u64(), k1[0]["failed"].as_u64()), (Some("nightly"), Some(4), Some(1)));
        assert_eq!(k1[0]["coordinators"], json!(["coord-a", "coord-b"]));
        assert_eq!((k1[1]["app"].as_str(), k1[1]["coordinators"].clone()), (Some("broken-app"), json!(["coord-b"])));
        assert!(b.json(t + 10)["keys"].get("k1").is_none(), "none since then");
    }

    #[test]
    fn statements_differing_in_values_share_a_fingerprint() {
        let a = fingerprint("SELECT id FROM t WHERE MATCH(body, 'laptop') AND price < 100 LIMIT 10");
        let b = fingerprint("SELECT  id FROM t\nWHERE MATCH(body, 'red ''phone''') AND price < 2.5e3 LIMIT 5");
        assert_eq!(a, b);
        assert_eq!(a, "SELECT id FROM t WHERE MATCH(body, ?) AND price < ? LIMIT ?");
        assert_eq!(fingerprint("SELECT x1 FROM t2 WHERE id IN (1, 2, 3)"), "SELECT x1 FROM t2 WHERE id IN (?...)");
        assert_eq!(fingerprint("SELECT a FROM t WHERE id IN (1, 2)"), fingerprint("SELECT a FROM t WHERE id IN (7, 8, 9, 10)"));
        assert_eq!(fingerprint("SELECT \"a b\" FROM t WHERE x = -3"), "SELECT \"a b\" FROM t WHERE x = ?");
        assert_eq!(fingerprint("SELECT a-1 FROM t"), "SELECT a-? FROM t");
        assert_eq!(fingerprint("SELECT 'é' AS ü FROM t"), "SELECT ? AS ü FROM t");
    }

    #[test]
    fn params_are_kept_capped_or_by_type() {
        use chloedb::value::Value;
        let p = chloedb::query::Params::new().set("q", Value::Str("x".repeat(500))).set("n", Value::Int(3)).set("v", Value::Vector(vec![0.5f32; 384].into()));
        let j = params_json(&p).unwrap();
        assert_eq!(j["n"], 3);
        assert!(j["q"].as_str().unwrap().len() < 220, "{j}");
        assert_eq!(j["v"], "[384 values]");
        assert!(j.to_string().len() <= PARAMS_BYTES);
        set_redact_params(true);
        let r = params_json(&p).unwrap();
        set_redact_params(false);
        assert_eq!(*r, json!({"n": "int", "q": "text", "v": "vector(384)"}));
        assert!(params_json(&chloedb::query::Params::new()).is_none());
        let who = crate::keys::Principal { name: "token".into(), key: None, scope: crate::keys::Scope::Admin, root: true, tables: None, role: None, agent: false, on_behalf_of: None, allow: None };
        let r = crate::keys::Caller { app: Some("shop-ios".into()), client: Some("command-center".into()), ..Default::default() };
        let c = Caller::of(&who, Some(r));
        assert_eq!((c.key.as_deref(), c.app.as_deref(), c.client.as_deref()), (Some("root token"), Some("shop-ios"), Some("command-center")));
        assert_eq!(Caller::of(&crate::keys::Principal::open(), None).key, None);
    }

    #[test]
    fn quantiles_interpolate_within_a_bucket() {
        let bounds = [1.0, 2.0, 4.0];
        assert_eq!(quantile(&bounds, &[0, 0, 0, 0], 0.5), None);
        assert_eq!(quantile(&bounds, &[10, 0, 0, 0], 0.5), Some(0.5));
        assert_eq!(quantile(&bounds, &[0, 10, 0, 0], 1.0), Some(2.0));
        assert_eq!(quantile(&bounds, &[5, 0, 5, 0], 0.75), Some(3.0));
        assert_eq!(quantile(&bounds, &[0, 0, 0, 3], 0.99), Some(4.0));
        // Never above the slowest counted, past the last bound too.
        assert_eq!(quantile_within(&bounds, &[0, 10, 0, 0], 1.0, Some(1.5)), Some(1.5));
        assert_eq!(quantile_within(&bounds, &[0, 0, 0, 4], 1.0, Some(9.0)), Some(9.0));
        assert_eq!(quantile_within(&bounds, &[0, 0, 0, 4], 0.5, Some(8.0)), Some(6.0));
        assert_eq!(quantile_within(&bounds, &[0, 0, 0, 0], 0.5, Some(8.0)), None);
    }

    fn finished(fp: &str, ts_ms: u64, ms: f64, outcome: &'static str, key: &str) -> Arc<Finished> {
        static ID: AtomicU64 = AtomicU64::new(0);
        let caller = Caller { key: Some(key.into()), ..Default::default() };
        Arc::new(Finished { id: ID.fetch_add(1, Ordering::Relaxed), ts_ms, sql: fp.to_lowercase(), fingerprint: fp.into(), kind: "bm25", outcome, ms, rows: 2, caller, ..Default::default() })
    }

    /// A key's time in a window counts, of the minute that began before
    /// it, only the part inside it; its calls count whole.
    #[test]
    fn a_keys_time_counts_the_part_of_its_first_minute_inside_the_window() {
        let s = Statements::default();
        let m = now_ms() / 60_000 * 60_000 - 120_000;
        s.add(&finished("A", m + 10_000, 6_000.0, "ok", "report"));
        s.add(&finished("A", m + 70_000, 600.0, "error", "report"));
        s.add(&finished("B", m + 70_000, 60.0, "ok", "app"));
        // 15 s of the first minute are inside: a quarter of its 6 s.
        let mut k = s.key_calls(m + 45_000, m + 120_000);
        k.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(k, [("app".to_string(), 1, 0, 60.0), ("report".to_string(), 2, 1, 1_500.0 + 600.0)]);
        // The whole of both minutes.
        let k = s.key_calls(m, m + 120_000);
        assert!(k.iter().any(|x| x.0 == "report" && x.3 == 6_600.0), "{k:?}");
    }

    #[test]
    fn statements_are_ranked_by_time_spent() {
        let s = Statements::default();
        let t = now_ms();
        for _ in 0..3 {
            s.add(&finished("A", t, 1.0, "ok", "app"));
        }
        s.add(&finished("B", t, 500.0, "error", "app"));
        s.add(&finished("B", t, 2.0, "client_error", "batch"));
        let (from, to) = window_of(None, None);
        let j = s.window(from, to + 1, 10);
        assert_eq!(j["statements"][0]["fingerprint"], "B", "{j}");
        assert_eq!(j["statements"][0]["errors"], 2);
        assert_eq!(j["statements"][0]["server_errors"], 1);
        assert_eq!(j["statements"][0]["outcomes"], json!({"error": 1, "client_error": 1}));
        assert_eq!(j["statements"][0]["keys"][0]["key"], "app");
        assert_eq!(j["statements"][1]["calls"], 3);
        assert_eq!(j["statements"][1]["rows_per_call"], 2.0);
        assert!(j["statements"][1]["p99_ms"].as_f64().unwrap() <= 1.0);
        let id = j["statements"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(id, statement_id("B"));
        let d = s.detail(&id, from, to + 1).unwrap();
        assert_eq!(d["statement"]["calls"], 2);
        assert_eq!(d["slowest"][0]["ms"], 500.0, "{d}");
        assert_eq!(d["by_key"].as_array().unwrap().len(), 2);
        assert!(s.detail("nothing", from, to).is_none());
        s.reset();
        assert_eq!(s.window(from, to + 1, 10)["count"], 0);
    }

    /// A window compares with the one before it, as long; a statement
    /// first seen in it, while statements were counted before, is new.
    #[test]
    fn a_window_has_the_one_before() {
        let s = Statements::default();
        let t = now_ms() / 60_000 * 60_000;
        s.since_ms.store(t - 3 * 3_600_000, Ordering::Relaxed);
        s.add(&finished("OLD", t - 30 * 60_000, 10.0, "ok", "app"));
        s.add(&finished("OLD", t - 5 * 60_000, 40.0, "ok", "app"));
        s.add(&finished("NEW", t - 4 * 60_000, 1.0, "ok", "app"));
        let j = s.window(t - 15 * 60_000, t, 10);
        assert_eq!(j["prev"], json!({"from": t - 30 * 60_000, "to": t - 15 * 60_000}));
        let old = j["statements"].as_array().unwrap().iter().find(|x| x["fingerprint"] == "OLD").unwrap();
        assert_eq!(old["prev"]["calls"], 1, "{old}");
        assert_eq!(old["new"], false);
        assert_eq!(old["first_seen_ms"], t - 30 * 60_000);
        let new = j["statements"].as_array().unwrap().iter().find(|x| x["fingerprint"] == "NEW").unwrap();
        assert!(new["prev"].is_null());
        assert_eq!(new["new"], true);
        let d = s.detail("OLD", t - 60 * 60_000, t).unwrap();
        assert_eq!(d["trend"].as_array().unwrap().len(), 2, "{d}");
    }

    #[test]
    fn a_point_is_the_rates_between_samples() {
        let t = Instant::now();
        let mut a = Sample { at: Some(t), queries: 10, query_hist: vec![0; 15], ..Default::default() };
        a.by_kind.insert("bm25", 10);
        let mut b = Sample { at: Some(t + Duration::from_secs(2)), ts_ms: 5, queries: 30, query_errors: 2, query_hist: vec![0; 15], ..Default::default() };
        b.by_kind.insert("bm25", 30);
        b.query_hist[3] = 20; // all at most 10 ms
        b.gauges.insert("docs", 7.0);
        let series = Series::default();
        assert!(series.add(a).is_none());
        let p = series.add(b).unwrap();
        assert_eq!(p["qps"], 10.0);
        assert_eq!(p["error_ratio"], 0.1);
        assert_eq!(p["by_kind"]["bm25"], 10.0);
        assert_eq!(p["docs"], 7.0);
        let p50 = p["p50_ms"].as_f64().unwrap();
        assert!(p50 > 5.0 && p50 <= 10.0, "{p50}");
        assert_eq!(series.since(0).len(), 1);
        assert!(series.since(5).is_empty());
        assert_eq!(p["availability"], 0.9);
        // No query ended: no ratio, rather than none failing.
        let c = Sample { at: Some(t + Duration::from_secs(4)), ts_ms: 7, queries: 30, query_errors: 2, query_hist: b_hist(), ..Default::default() };
        let q = series.add(c).unwrap();
        assert_eq!(q["queries"], 0);
        assert!(q.get("error_ratio").is_none() && q.get("availability").is_none(), "{q}");
        assert!(q["p99_ms"].is_null());
    }

    fn b_hist() -> Vec<u64> {
        let mut h = vec![0; 15];
        h[3] = 20;
        h
    }

    #[tokio::test]
    async fn a_cancelled_query_ends_with_its_error() {
        let reg = Arc::new(LiveQueries::default());
        let live = reg.start("SELECT 1", "filter", false, &Meta::default());
        assert_eq!(reg.len(), 1);
        let id = live.query.id;
        let r2 = reg.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(r2.cancel(id));
        });
        let r: Result<(), _> = LiveQueries::run(&live, std::future::pending()).await;
        assert_eq!(r.unwrap_err().status, 499);
        drop(live);
        assert!(reg.is_empty());
        assert!(!reg.cancel(id));
    }

    #[tokio::test]
    async fn spans_land_in_the_request_s_trace() {
        let t = Trace::new();
        traced(t.clone(), async {
            let t0 = Instant::now();
            record("execute", t0, "ok");
            let _c = CallSpan { name: rpc_name::<crate::pb::QueryResponse>(), shard: 1, replica: "http://r".into(), t0, again: true, outcome: None };
        })
        .await;
        record("outside", Instant::now(), "ok");
        let spans = t.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "execute");
        assert_eq!(spans[1].name, "Query");
        assert_eq!(spans[1].outcome, "outrun");
        assert_eq!(spans[1].shard, Some(1));
    }

    #[test]
    fn the_process_and_its_disk_are_measured() {
        let (rss, cpu) = process_usage();
        assert!(cpu >= 0.0);
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(rss.unwrap() > 0);
        }
        let (total, free, avail) = disk(std::path::Path::new(".")).unwrap();
        assert!(total >= free && free >= avail.min(free));
    }
}
