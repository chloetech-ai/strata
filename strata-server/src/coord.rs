//! A coordinator over shard servers: one collection split by primary key.
//!
//! A query runs in two rounds when it scores with BM25. The coordinator
//! asks every shard for the statistics of the query's terms, each shard
//! answering from a snapshot it holds for the next round (a lease), sums
//! them, and runs the query on every shard in shard mode with the sum and
//! the shard's lease. Each shard then scores as the unsplit collection
//! would, from the version it counted, and [`strata::cluster::merge`]
//! combines the answers. Other queries take one round.
//!
//! A query for a page of rows with stored columns is answered by query
//! then fetch ([`strata::cluster::fetches`], from protocol 3), when the
//! shards would send many more rows than the page in one round (a deep
//! page, many shards or a fused score: [`FETCH_FROM`]): every shard ranks
//! its rows and answers their keys, scores and sort values only, holding
//! the snapshot it read; the coordinator merges them, and the shards
//! holding the page's rows fetch their columns from that snapshot and end
//! its lease ([`CoordOptions::fetch`]). The other shards' leases are ended
//! alongside. Columns are read and sent for the page, not for each shard's
//! top `LIMIT + OFFSET` rows.
//!
//! A shard may be served by several replicas, servers holding the same
//! documents (`--shards a0|a1,b0|b1`). A read goes to one replica of each
//! shard, in turn; when a replica fails, the next one is asked, and when
//! one has not answered after [`CoordOptions::hedge`], the next one is
//! asked too and the first answer wins, while hedges stay within
//! [`HEDGE_SHARE`] of calls. A replica that refuses a query because too
//! many are running is not relieved by asking its sibling, which would
//! only be loaded more: the query fails (or leaves the shard out of a
//! partial answer). A replica that failed, or was outrun, is asked last
//! for [`CoordOptions::cool_off`]. With
//! [`CoordOptions::partial`], a query whose shard has no replica answering
//! is answered from the other shards and says which are missing, and the
//! queries of the next second leave that shard out without asking it.
//!
//! Writes go to the shard of their key, on every replica: the shard
//! holding its slot ([`crate::slots`]) in the coordinator's slot map, which
//! it gives every shard when it connects and whose epoch it sends with
//! every request, so that a shard refuses a coordinator routing by an
//! older map, and keys another shard holds. UPDATE and DELETE statements
//! go to every shard. A write that spans
//! shards is one transaction per replica, not one across them, and a
//! replica that misses a write (it was down) is not caught up: rebuild it.
//!
//! The coordinator asks each replica the protocol versions it speaks
//! (`Hello`, [`crate::grpc::PROTOCOL`]) when it connects, again every
//! minute and after the replica fails, as it may have restarted on another
//! version, and speaks the oldest any replica speaks. From protocol 2 a
//! read reaches the shards as a typed plan ([`crate::plan_pb`]); a server
//! from before `Hello` takes SQL, or a JSON plan, as it did.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::future::join_all;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::{json, Value as Json};
use strata::query::agg::Part;
use strata::query::exec::ResultSet;
use strata::query::ir::Plan;
use strata::query::stats::TermStats;
use strata::query::{bind_write, Conflict, Params, Select, Write};
use strata::schema::Schema;
use strata::value::Value;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use tonic::Code;

use crate::auth::Secret;
use crate::grpc::{params_to_pb, result_from_pb, MIN_PROTOCOL, PROTOCOL};
use crate::metrics::Family;
use crate::pb::{self, strata_client::StrataClient};
use crate::service::{op_key, QueryReq};
use crate::slots::{hash_shard_of, SlotMap};
use crate::{json as js, plan_pb, ApiError};

/// How the coordinator uses replicas and handles shards that do not answer.
#[derive(Clone, Debug)]
pub struct CoordOptions {
    /// Ask another replica of a shard when one has not answered after this.
    pub hedge: Duration,
    /// Ask a replica that failed or was outrun last, for this long.
    pub cool_off: Duration,
    /// Give up on a shard after this long (`None`: at the query's deadline).
    pub shard_timeout: Option<Duration>,
    /// Answer without shards that have no replica answering, and say so,
    /// instead of failing the query.
    pub partial: bool,
    /// The shards' token, sent with every call.
    pub token: Option<Secret>,
    /// A CA certificate (PEM) that `https://` shards' certificates are
    /// checked against, besides the public roots.
    pub ca: Option<Vec<u8>>,
    /// Answer pages of rows by query then fetch when the shards speak
    /// protocol 3; otherwise in one round, every shard sending the columns
    /// of its top rows.
    pub fetch: bool,
    /// The slot map to route writes by and give the shards (`None`: the
    /// shards holding equal runs of slots, [`SlotMap::even`], epoch 1).
    pub slots: Option<SlotMap>,
}

impl Default for CoordOptions {
    fn default() -> CoordOptions {
        CoordOptions { hedge: Duration::from_millis(100), cool_off: Duration::from_secs(5), shard_timeout: None, partial: false, token: None, ca: None, fetch: true, slots: None }
    }
}

/// Adds the shards' token to every call, and the request's id.
#[derive(Clone)]
pub struct Credentials(Option<tonic::metadata::AsciiMetadataValue>);

impl tonic::service::Interceptor for Credentials {
    /// Also passes on the id of the request the call serves
    /// ([`crate::obs`]).
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(v) = &self.0 {
            req.metadata_mut().insert("authorization", v.clone());
        }
        if let Some(id) = crate::obs::current().and_then(|id| id.parse().ok()) {
            req.metadata_mut().insert(crate::obs::HEADER, id);
        }
        Ok(req)
    }
}

type Client = StrataClient<InterceptedService<Channel, Credentials>>;

/// One server holding a copy of a shard.
struct Replica {
    url: String,
    client: Client,
    /// Asked last until this time (ms after [`Coordinator::epoch`]).
    avoid_until: AtomicU64,
    /// The protocol it speaks with the coordinator (0: not known), and
    /// when it said so (ms after [`Coordinator::epoch`]).
    protocol: AtomicU32,
    hello_at: AtomicU64,
}

impl Replica {
    fn new(url: &str, client: Client) -> Replica {
        Replica { url: url.to_string(), client, avoid_until: AtomicU64::new(0), protocol: AtomicU32::new(0), hello_at: AtomicU64::new(0) }
    }
}

/// Query then fetch costs a second round, so it is taken when one round
/// would send at least this many times the page's rows
/// ([`Coordinator::fetch_pays`]). Over localhost, with 3 shards of 20,000
/// demo products and a 10-row page on a 4-core cloud container, one round
/// was faster by up to 0.5 ms up to 6 times the page's rows, the two were
/// even at 9, and query then fetch was faster from 15 (2026-10-29).
pub const FETCH_FROM: usize = 8;

/// How long a replica's answer to `Hello` is trusted before it is asked
/// again, so that the coordinator follows a rolling upgrade or a rollback.
const HELLO_EVERY_MS: u64 = 60_000;

/// The protocol to speak with a server that answered `Hello`: the newest
/// both speak.
fn agree(h: &pb::HelloResponse) -> Result<u32, String> {
    let p = h.max_protocol.min(PROTOCOL);
    if p < h.min_protocol.max(MIN_PROTOCOL) {
        return Err(format!("{} speaks protocols {} to {}, and this coordinator {MIN_PROTOCOL} to {PROTOCOL}", h.server, h.min_protocol, h.max_protocol));
    }
    Ok(p)
}

/// Asks a server the protocol to speak with it ([`agree`]) and the epoch
/// of the slot map it holds. One from before `Hello` (it answers
/// Unimplemented) speaks protocol 1.
async fn hello(c: &mut Client) -> Result<(u32, u64), tonic::Status> {
    match c.hello(pb::HelloRequest { min_protocol: MIN_PROTOCOL, max_protocol: PROTOCOL }).await {
        Ok(h) => {
            let h = h.into_inner();
            Ok((agree(&h).map_err(tonic::Status::failed_precondition)?, h.slot_epoch))
        }
        Err(s) if s.code() == Code::Unimplemented => Ok((1, 0)),
        Err(s) => Err(s),
    }
}

/// Gives a server `map`, with it as shard `shard` ([`crate::service::Service::set_slots`]).
async fn give_slots(c: &mut Client, map: &SlotMap, shard: usize) -> Result<u64, tonic::Status> {
    let map_json = serde_json::to_vec(&map.to_json()).unwrap_or_default();
    Ok(c.slots(pb::SlotsRequest { map_json, shard: shard as u32 }).await?.into_inner().epoch)
}

/// A read in the forms the shards take: SQL and its parameters, or a JSON
/// plan, in protocol 1; a typed plan from protocol 2, when the read is to
/// speak it.
struct Read {
    sql: String,
    params: std::collections::HashMap<String, pb::Value>,
    plan_json: Vec<u8>,
    plan: Option<pb::Plan>,
    /// The slot map's epoch, sent from protocol 4.
    epoch: u64,
}

impl Read {
    /// `sql` with `params`, or `given` (a compound query's leaf, which
    /// `sql` then only names), and `typed()`, the plan protocol 2 sends,
    /// made only when `protocol` is 2 or newer.
    fn new(sql: &str, params: &Params, given: Option<&Plan>, protocol: u32, epoch: u64, typed: impl FnOnce() -> strata::Result<Plan>) -> Result<Read, ApiError> {
        let plan = match (protocol >= 2, given) {
            (false, _) => None,
            (true, Some(p)) => Some(plan_pb::to_pb_plan(p)),
            (true, None) => Some(plan_pb::to_pb_plan(&typed()?)),
        };
        Ok(Read { sql: sql.to_string(), params: params_to_pb(params), plan_json: plan_json(given)?, plan, epoch })
    }

    /// The request in `protocol`.
    fn request(&self, protocol: u32) -> pb::QueryRequest {
        match (&self.plan, protocol >= 2) {
            (Some(plan), true) => {
                let epoch = if protocol >= 4 { self.epoch } else { 0 };
                pb::QueryRequest { sql: self.sql.clone(), protocol, plan: Some(plan.clone()), epoch, ..Default::default() }
            }
            _ => pb::QueryRequest { sql: self.sql.clone(), params: self.params.clone(), plan_json: self.plan_json.clone(), protocol: 1, ..Default::default() },
        }
    }
}

pub struct Coordinator {
    /// Each shard's replicas, as given: URLs joined by `|`.
    pub shards: Vec<String>,
    replicas: Vec<Vec<Replica>>,
    pub schema: Schema,
    pub opts: CoordOptions,
    epoch: Instant,
    /// Where each read starts among a shard's replicas, to spread the load.
    turn: AtomicUsize,
    /// Per shard: until when partial answers leave it out (ms after
    /// `epoch`), after no replica of it answered.
    skip_until: Vec<AtomicU64>,
    /// Replicas asked again after another failed, and hedged requests.
    retries: AtomicU64,
    hedges: AtomicU64,
    hedge_budget: HedgeBudget,
    /// Per shard: calls answered, failed and timed out ([`Self::call`]).
    outcomes: Vec<[AtomicU64; 3]>,
    /// The slot map writes are routed by, which every shard holds; `None`
    /// when a shard spoke a protocol before 4 at connect, and writes go by
    /// the hash before slots ([`hash_shard_of`]).
    slots: Option<SlotMap>,
    /// Queries answered by query then fetch, and rows of their pages that
    /// no fetch found.
    fetches: AtomicU64,
    fetch_lost: AtomicU64,
}

/// A query's merged answer, and the shards it lacks (with
/// [`CoordOptions::partial`]).
pub struct Merged {
    pub rs: ResultSet,
    pub missing: Vec<String>,
}

/// The share of calls that may be hedged: each call earns this much of a
/// hedge, up to [`HEDGE_BURST`] saved, so that a slow cluster is not sent
/// twice its load.
pub const HEDGE_SHARE: f64 = 0.05;
/// Hedges that may be saved up for a burst.
pub const HEDGE_BURST: f64 = 10.0;

/// The hedges a coordinator may send now, in thousandths of one.
struct HedgeBudget(AtomicU64);

impl HedgeBudget {
    const ONE: u64 = 1000;
    const SHARE: u64 = (HEDGE_SHARE * 1000.0) as u64;
    const CAP: u64 = (HEDGE_BURST * 1000.0) as u64;

    fn new() -> HedgeBudget {
        HedgeBudget(AtomicU64::new(Self::CAP))
    }

    /// Earns a call's share of a hedge.
    fn earn(&self) {
        self.update(|b| (b < Self::CAP).then(|| (b + Self::SHARE).min(Self::CAP)));
    }

    /// Spends a hedge, if one is saved.
    fn spend(&self) -> bool {
        self.update(|b| b.checked_sub(Self::ONE))
    }

    /// Replaces the budget with `f` of it, unless `f` declines; whether it
    /// was replaced. (`fetch_update` is deprecated on newer toolchains and
    /// its replacement missing on older ones.)
    fn update(&self, f: impl Fn(u64) -> Option<u64>) -> bool {
        let mut b = self.0.load(Ordering::Relaxed);
        while let Some(next) = f(b) {
            match self.0.compare_exchange_weak(b, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(now) => b = now,
            }
        }
        false
    }
}

fn ms_left(deadline: Instant) -> u32 {
    deadline.saturating_duration_since(Instant::now()).as_millis().clamp(1, u32::MAX as u128) as u32
}

/// A plan as a request carries it (`plan_json`; empty without one).
fn plan_json(plan: Option<&Plan>) -> Result<Vec<u8>, ApiError> {
    match plan {
        Some(p) => Ok(serde_json::to_vec(&p.to_json()?).map_err(|e| ApiError::internal(format!("plan: {e}")))?),
        None => Ok(Vec::new()),
    }
}

/// Whether another replica may answer where this one failed: not when the
/// request itself is wrong, nor when the replica is overloaded
/// (`ResourceExhausted`), which would only load the next one more.
fn try_elsewhere(s: &tonic::Status) -> bool {
    !matches!(
        s.code(),
        Code::InvalidArgument
            | Code::NotFound
            | Code::AlreadyExists
            | Code::PermissionDenied
            | Code::Unauthenticated
            | Code::FailedPrecondition
            | Code::OutOfRange
            | Code::Unimplemented
            | Code::ResourceExhausted
    )
}

impl Coordinator {
    /// Connects to `shards` (gRPC URLs such as `http://10.0.0.5:7001`,
    /// replicas of one shard joined by `|`) and checks that they serve the
    /// same table. Every shard needs a replica answering.
    pub async fn connect(shards: Vec<String>) -> Result<Coordinator, ApiError> {
        Coordinator::connect_with(shards, CoordOptions::default()).await
    }

    pub async fn connect_with(shards: Vec<String>, opts: CoordOptions) -> Result<Coordinator, ApiError> {
        if shards.is_empty() {
            return Err(ApiError::bad("a coordinator needs at least one shard"));
        }
        let credentials = match &opts.token {
            Some(t) => Some(format!("Bearer {}", t.0).parse().map_err(|_| ApiError::bad("the shards' token must be printable ASCII"))?),
            None => None,
        };
        let mut tls = tonic::transport::ClientTlsConfig::new().with_webpki_roots();
        if let Some(ca) = &opts.ca {
            tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(ca));
        }
        let mut replicas = Vec::new();
        for group in &shards {
            let mut rs = Vec::new();
            for url in group.split('|').map(str::trim).filter(|u| !u.is_empty()) {
                let mut ep = Channel::from_shared(url.to_string()).map_err(|e| ApiError::bad(format!("bad shard URL {url}: {e}")))?;
                if url.starts_with("https://") {
                    ep = ep.tls_config(tls.clone()).map_err(|e| ApiError::bad(format!("TLS to {url}: {e}")))?;
                }
                let ch = ep.connect_timeout(Duration::from_secs(5)).tcp_nodelay(true).connect_lazy();
                let client = StrataClient::with_interceptor(ch, Credentials(credentials.clone())).max_decoding_message_size(256 << 20);
                rs.push(Replica::new(url, client));
            }
            if rs.is_empty() {
                return Err(ApiError::bad(format!("shard {group:?} has no URL")));
            }
            replicas.push(rs);
        }
        let mut schemas = Vec::new();
        for (i, rs) in replicas.iter().enumerate() {
            let answers = join_all(rs.iter().map(|r| {
                let mut c = r.client.clone();
                let ask = async move {
                    let s = c.schema(pb::Empty {}).await?.into_inner().schema_json;
                    Ok::<_, tonic::Status>((s, hello(&mut c).await?.0))
                };
                tokio::time::timeout(Duration::from_secs(5), ask)
            }))
            .await;
            let mut errors = Vec::new();
            for (r, a) in rs.iter().zip(answers) {
                match a {
                    Ok(Ok((s, p))) => {
                        schemas.push(s);
                        r.protocol.store(p, Ordering::Relaxed);
                    }
                    Ok(Err(e)) => errors.push(format!("{}: {}", r.url, e.message())),
                    Err(_) => errors.push(format!("{}: no answer within 5 s", r.url)),
                }
            }
            if errors.len() == rs.len() {
                return Err(ApiError { status: 502, message: format!("shard {i} has no replica answering: {}", errors.join("; ")) });
            }
        }
        if schemas.iter().any(|s| *s != schemas[0]) {
            return Err(ApiError::bad("the shards serve different schemas"));
        }
        let schema: Schema = serde_json::from_str(&schemas[0]).map_err(ApiError::internal)?;
        if schema.primary_key.is_none() {
            return Err(ApiError::bad("a sharded table needs a primary key to route writes"));
        }
        // Slots once every replica answering speaks protocol 4.
        let old = replicas.iter().flatten().find(|r| (1..4).contains(&r.protocol.load(Ordering::Relaxed)));
        let slots = match (old, &opts.slots) {
            (None, given) => Some(given.clone().unwrap_or_else(|| SlotMap::even(replicas.len(), 1))),
            (Some(r), Some(_)) => return Err(ApiError::bad(format!("a slot map needs every shard to speak protocol 4; {} speaks {}", r.url, r.protocol.load(Ordering::Relaxed)))),
            (Some(r), None) => {
                tracing::warn!(shard = %r.url, protocol = r.protocol.load(Ordering::Relaxed), "a shard is from before slots (protocol 4): writes go by the old hash until every shard is upgraded and the coordinator restarts");
                None
            }
        };
        if let Some(m) = &slots {
            if m.shards() != replicas.len() {
                return Err(ApiError::bad(format!("the slot map has {} shards, and --shards {}", m.shards(), replicas.len())));
            }
            let gives = replicas.iter().enumerate().flat_map(|(i, rs)| rs.iter().filter(|r| r.protocol.load(Ordering::Relaxed) != 0).map(move |r| (i, r)));
            let answers = join_all(gives.map(|(i, r)| {
                let mut c = r.client.clone();
                async move { (r, tokio::time::timeout(Duration::from_secs(60), give_slots(&mut c, m, i)).await) }
            }))
            .await;
            for (r, a) in answers {
                match a {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => return Err(ApiError { status: ApiError::from(e.clone()).status, message: format!("{} refused slot map epoch {}: {}", r.url, m.epoch, e.message()) }),
                    Err(_) => return Err(ApiError { status: 504, message: format!("{} took the slot map no answer within 60 s", r.url) }),
                }
            }
        }
        let skip_until = replicas.iter().map(|_| AtomicU64::new(0)).collect();
        let outcomes = replicas.iter().map(|_| Default::default()).collect();
        Ok(Coordinator {
            shards,
            replicas,
            schema,
            opts,
            epoch: Instant::now(),
            turn: AtomicUsize::new(0),
            skip_until,
            retries: AtomicU64::new(0),
            hedges: AtomicU64::new(0),
            hedge_budget: HedgeBudget::new(),
            outcomes,
            fetches: AtomicU64::new(0),
            fetch_lost: AtomicU64::new(0),
            slots,
        })
    }

    /// What `/metrics` shows of the shards: calls by outcome, retries and
    /// hedges, and replicas avoided now after failing or being outrun.
    pub fn metrics(&self) -> Vec<Family> {
        let outcomes = ["ok", "error", "timeout"];
        let calls = self.outcomes.iter().enumerate().flat_map(|(i, o)| outcomes.iter().zip(o).map(move |(name, n)| (format!("shard=\"{i}\",outcome=\"{name}\""), n.load(Ordering::Relaxed) as f64))).collect();
        let now = self.now_ms();
        let avoided = self.replicas.iter().flatten().filter(|r| r.avoid_until.load(Ordering::Relaxed) > now).count();
        vec![
            Family { name: "strata_shard_calls_total", ty: "counter", help: "Calls to a shard (its replicas, hedges and retries together), by outcome: ok, error, timeout.", values: calls },
            Family::one("strata_shard_retries_total", "counter", "Calls asked of another replica after one failed.", self.retries.load(Ordering::Relaxed) as f64),
            Family::one("strata_shard_hedges_total", "counter", "Calls asked of another replica too after the first had not answered within --hedge-ms.", self.hedges.load(Ordering::Relaxed) as f64),
            Family::one("strata_replicas_avoided", "gauge", "Replicas asked last for now, after they failed or were outrun.", avoided as f64),
            Family::one("strata_shard_protocol", "gauge", "The protocol reads speak to the shards: the oldest a replica speaks.", self.protocol() as f64),
            Family::one("strata_slot_epoch", "gauge", "The epoch of the slot map writes are routed by; 0 while they go by the hash before slots.", self.epoch() as f64),
            Family::one("strata_fetch_queries_total", "counter", "Queries answered by query then fetch.", self.fetches.load(Ordering::Relaxed) as f64),
            Family::one("strata_fetch_lost_rows_total", "counter", "Rows of query-then-fetch pages that no fetch found: their shard did not answer the fetch (partial answers), or another replica did, without them.", self.fetch_lost.load(Ordering::Relaxed) as f64),
        ]
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// The protocol reads speak to the shards: the oldest a replica
    /// speaks, of those that said (the newest while none has).
    pub fn protocol(&self) -> u32 {
        self.replicas.iter().flatten().map(|r| r.protocol.load(Ordering::Relaxed)).filter(|&p| p != 0).min().unwrap_or(PROTOCOL)
    }

    /// The epoch of the slot map writes are routed by; 0 without one.
    pub fn epoch(&self) -> u64 {
        self.slots.as_ref().map_or(0, |m| m.epoch)
    }

    /// The shard holding `key`.
    fn shard_of(&self, key: &strata::value::Value) -> usize {
        match &self.slots {
            Some(m) => m.shard_of(key),
            None => hash_shard_of(key, self.replicas.len()),
        }
    }

    /// The protocol to speak with replica `r` of shard `i`, `max` at most:
    /// asked of it first when not known, or not asked for a minute, and
    /// given the slot map when it holds another.
    async fn speak(&self, i: usize, r: usize, c: &mut Client, max: u32) -> Result<u32, tonic::Status> {
        let rep = &self.replicas[i][r];
        let p = rep.protocol.load(Ordering::Relaxed);
        if p != 0 && self.now_ms() < rep.hello_at.load(Ordering::Relaxed) + HELLO_EVERY_MS {
            return Ok(p.min(max));
        }
        let (p, epoch) = hello(c).await?;
        if let Some(m) = self.slots.as_ref().filter(|m| p >= 4 && epoch != m.epoch) {
            give_slots(c, m, i).await?;
        }
        rep.protocol.store(p, Ordering::Relaxed);
        rep.hello_at.store(self.now_ms(), Ordering::Relaxed);
        Ok(p.min(max))
    }

    fn avoid(&self, i: usize, r: usize) {
        self.replicas[i][r].avoid_until.store(self.now_ms() + self.opts.cool_off.as_millis() as u64, Ordering::Relaxed);
    }

    /// The order to ask shard `i`'s replicas in: `first` if given, then in
    /// turn from `start`, those being avoided last.
    fn order(&self, i: usize, start: usize, first: Option<usize>) -> Vec<usize> {
        let n = self.replicas[i].len();
        let now = self.now_ms();
        let mut order: Vec<usize> = (0..n).map(|k| (start + k) % n).collect();
        order.sort_by_key(|&r| (self.replicas[i][r].avoid_until.load(Ordering::Relaxed) > now, Some(r) != first));
        order
    }

    /// Calls shard `i` with `f(replica, client)`, asking its replicas in
    /// [`Self::order`]: the next one when one fails with an error another
    /// may not have, and also when one has not answered after the hedge
    /// delay and the hedge budget allows. Returns the replica that
    /// answered and its answer.
    async fn call<T, F, Fut>(&self, i: usize, start: usize, first: Option<usize>, deadline: Instant, f: F) -> Result<(usize, T), ApiError>
    where
        F: Fn(usize, Client) -> Fut,
        Fut: Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    {
        let order = self.order(i, start, first);
        let limit = self.opts.shard_timeout.map_or(deadline, |t| deadline.min(Instant::now() + t));
        let mut pending = FuturesUnordered::new();
        let mut asked = Vec::new();
        let launch = |r: usize| {
            let fut = f(r, self.replicas[i][r].client.clone());
            async move { (r, fut.await) }
        };
        pending.push(launch(order[0]));
        asked.push(order[0]);
        self.hedge_budget.earn();
        let mut hedge_at = Instant::now() + self.opts.hedge;
        // Off once a hedge was due and the budget had none.
        let mut hedging = true;
        loop {
            let more = asked.len() < order.len();
            tokio::select! {
                Some((r, res)) = pending.next() => match res {
                    Ok(answer) => {
                        // Replicas still working were outrun (and those that
                        // failed are avoided already).
                        for &other in asked.iter().filter(|&&o| o != r) {
                            self.avoid(i, other);
                        }
                        self.outcomes[i][0].fetch_add(1, Ordering::Relaxed);
                        return Ok((r, answer.into_inner()));
                    }
                    Err(s) if try_elsewhere(&s) => {
                        self.avoid(i, r);
                        // It may come back on another version: asked again.
                        self.replicas[i][r].protocol.store(0, Ordering::Relaxed);
                        if more {
                            self.retries.fetch_add(1, Ordering::Relaxed);
                            let next = order[asked.len()];
                            pending.push(launch(next));
                            asked.push(next);
                            hedge_at = Instant::now() + self.opts.hedge;
                        } else if pending.is_empty() {
                            let mut e = self.named(&self.replicas[i][r].url, s);
                            if order.len() > 1 {
                                e.message = format!("every replica of shard {i} failed; the last: {}", e.message);
                            }
                            self.outcomes[i][1].fetch_add(1, Ordering::Relaxed);
                            return Err(e);
                        }
                    }
                    Err(s) => {
                        self.outcomes[i][1].fetch_add(1, Ordering::Relaxed);
                        return Err(self.named(&self.replicas[i][r].url, s));
                    }
                },
                _ = tokio::time::sleep_until(hedge_at.into()), if more && hedging => {
                    if !self.hedge_budget.spend() {
                        hedging = false;
                        continue;
                    }
                    self.hedges.fetch_add(1, Ordering::Relaxed);
                    let next = order[asked.len()];
                    pending.push(launch(next));
                    asked.push(next);
                    hedge_at = Instant::now() + self.opts.hedge;
                }
                _ = tokio::time::sleep_until(limit.into()) => {
                    for &r in &asked {
                        self.avoid(i, r);
                    }
                    let urls: Vec<&str> = asked.iter().map(|&r| self.replicas[i][r].url.as_str()).collect();
                    self.outcomes[i][2].fetch_add(1, Ordering::Relaxed);
                    return Err(ApiError { status: 504, message: format!("shard {i} ({}): no answer in time", urls.join(", ")) });
                }
            }
        }
    }

    fn named(&self, url: &str, s: tonic::Status) -> ApiError {
        let mut e = ApiError::from(s);
        e.message = format!("shard {url}: {}", e.message);
        e
    }

    /// The statistics of shards `which`, and the replica and lease each
    /// came from. A lease lasts until `deadline` and a second.
    async fn shard_stats(&self, read: &Read, protocol: u32, which: Vec<usize>, start: usize, deadline: Instant) -> Vec<(usize, Result<(TermStats, usize, u64), ApiError>)> {
        let calls = which.into_iter().map(|i| {
            async move {
                let a = self
                    .call(i, start, None, deadline, |r, mut c| async move {
                        let p = self.speak(i, r, &mut c, protocol).await?;
                        c.term_stats(pb::QueryRequest { timeout_ms: ms_left(deadline), ..read.request(p) }).await
                    })
                    .await;
                let a = a.and_then(|(r, a)| {
                    let s: TermStats = serde_json::from_slice(&a.stats_json).map_err(ApiError::internal)?;
                    Ok((s, r, a.snapshot))
                });
                (i, a)
            }
        });
        join_all(calls).await
    }

    /// Summed statistics, from every shard, of `sql` or of `plan`.
    pub async fn term_stats(&self, sql: &str, plan: Option<&Plan>, params: &Params, deadline: Instant) -> Result<(TermStats, Vec<u64>), ApiError> {
        let start = self.turn.fetch_add(1, Ordering::Relaxed);
        let mut sum = TermStats::default();
        let mut leases = Vec::new();
        let protocol = self.protocol();
        let read = Read::new(sql, params, plan, protocol, self.epoch(), || Plan::bind(sql, &self.schema, params))?;
        for (_, a) in self.shard_stats(&read, protocol, (0..self.replicas.len()).collect(), start, deadline).await {
            let (s, _, lease) = a?;
            sum.add(&s);
            leases.push(lease);
        }
        Ok((sum, leases))
    }

    pub async fn query(&self, req: &QueryReq, select: &Select, deadline: Instant) -> Result<Merged, ApiError> {
        let partial = req.partial.unwrap_or(self.opts.partial);
        let protocol = self.protocol();
        let n = self.replicas.len();
        // Query then fetch: the query round answers the keys alone.
        let ids = match protocol >= 3 && self.opts.fetch && n > 1 && strata::cluster::fetches(select, &self.schema) && self.fetch_pays(select) {
            true => Some(strata::cluster::ids(select, &self.schema)?),
            false => None,
        };
        let ids_plan = ids.as_ref().map(|q| Plan::of_select(&self.schema, q));
        let none = Params::new();
        let (params, given) = match &ids_plan {
            Some(p) => (&none, Some(p)),
            None => (&req.params, req.plan.as_deref()),
        };
        let read = &Read::new(&req.sql, params, given, protocol, self.epoch(), || Ok(Plan::of_select(&self.schema, select)))?;
        let start = self.turn.fetch_add(1, Ordering::Relaxed);
        let ranked_text = matches!(crate::metrics::kind(select), "bm25" | "hybrid" | "boosted");
        // Per shard: the replica holding a lease for this query, and the lease.
        let mut leased: Vec<Option<(usize, u64)>> = vec![None; n];
        let mut failed: Vec<Option<ApiError>> = (0..n).map(|_| None).collect();
        if partial {
            let now = self.now_ms();
            for (i, f) in failed.iter_mut().enumerate() {
                if self.skip_until[i].load(Ordering::Relaxed) > now {
                    *f = Some(ApiError { status: 503, message: format!("shard {}: no replica answered in the last second", self.shards[i]) });
                }
            }
        }
        let stats = match &req.stats {
            Some(s) => Some(s.clone()),
            None if ranked_text && n > 1 => {
                let mut sum = TermStats::default();
                let which = (0..n).filter(|&i| failed[i].is_none()).collect();
                for (i, a) in self.shard_stats(read, protocol, which, start, deadline).await {
                    match a {
                        Ok((s, r, lease)) => {
                            sum.add(&s);
                            leased[i] = Some((r, lease));
                        }
                        Err(e) => self.leave_out(partial, i, e, &mut failed)?,
                    }
                }
                Some(sum)
            }
            None => None,
        };
        let stats_json = stats.map(|s| serde_json::to_vec(&s).unwrap_or_default()).unwrap_or_default();
        let leased = &leased;
        let hold = ids.is_some();
        let calls = (0..n).filter(|&i| failed[i].is_none()).map(|i| {
            let stats_json = stats_json.clone();
            let first = leased[i].map(|(r, _)| r);
            async move {
                let rs = self.call(i, start, first, deadline, |r, mut c| {
                    let stats_json = stats_json.clone();
                    // The lease holds on the replica that granted it; another
                    // replica scores from its latest version.
                    let snapshot = leased[i].filter(|(lr, _)| *lr == r).map_or(0, |(_, l)| l);
                    async move {
                        let p = self.speak(i, r, &mut c, protocol).await?;
                        // From protocol 3, the lease is held for the fetch
                        // round, or ended once answered.
                        let (hold, release) = (hold && p >= 3, !hold && snapshot != 0 && p >= 3);
                        c.query(pb::QueryRequest { timeout_ms: ms_left(deadline), shard: true, stats_json, snapshot, hold, release, ..read.request(p) }).await
                    }
                });
                (i, rs.await)
            }
        });
        let mut shards = Vec::new();
        // Per shard answering: the replica that did, and the lease it holds.
        let mut held = Vec::new();
        for (i, a) in join_all(calls).await {
            match a {
                Ok((r, a)) => {
                    held.push((i, r, a.snapshot));
                    shards.push(result_from_pb(a)?);
                }
                Err(e) => self.leave_out(partial, i, e, &mut failed)?,
            }
        }
        if shards.is_empty() {
            let e = failed.into_iter().flatten().next();
            return Err(e.unwrap_or_else(|| ApiError::internal("no shard answered")));
        }
        let rs = match &ids {
            None => strata::cluster::merge(select, shards)?,
            Some(ids) => {
                // Each key's shard, for the fetch round.
                let mut owner = HashMap::new();
                for (&(i, _, _), a) in held.iter().zip(&shards) {
                    owner.extend(a.keys.iter().map(|k| (Part::of(k), i)));
                }
                let ranked = strata::cluster::merge(ids, shards)?;
                let mut keys = vec![Vec::new(); n];
                for k in &ranked.keys {
                    let i = owner.get(&Part::of(k)).ok_or_else(|| ApiError::internal("a merged key no shard answered"))?;
                    keys[*i].push(k.clone());
                }
                self.fetch(&req.sql, select, ranked, &held, keys, (start, deadline, protocol, partial), &mut failed).await?
            }
        };
        let missing = failed.iter().enumerate().filter(|(_, f)| f.is_some()).map(|(i, _)| self.shards[i].clone()).collect();
        Ok(Merged { rs, missing })
    }

    /// Whether query then fetch pays for its second round: when one round
    /// would send at least [`FETCH_FROM`] times the page's rows, every shard
    /// its top `LIMIT + OFFSET`, as it does for a fused score, whose shards
    /// send each input's candidates to its depth.
    fn fetch_pays(&self, select: &Select) -> bool {
        let page = select.limit.unwrap_or(0);
        matches!(crate::metrics::kind(select), "hybrid" | "boosted") || self.replicas.len() * (page + select.offset) >= FETCH_FROM * page
    }

    /// Leaves shard `i` out of a partial answer (and the next second's)
    /// after it failed with `e`; a request it rejected fails the query, as
    /// does any failure without `partial`.
    fn leave_out(&self, partial: bool, i: usize, e: ApiError, failed: &mut [Option<ApiError>]) -> Result<(), ApiError> {
        if !partial || e.status == 400 {
            return Err(e);
        }
        self.skip_until[i].store(self.now_ms() + 1000, Ordering::Relaxed);
        failed[i] = Some(e);
        Ok(())
    }

    /// The fetch round of query then fetch: the columns of the page
    /// `ranked`, `keys[i]` from shard `i`, read from the snapshot its
    /// query round held (`held`: shard, replica, lease), whose lease the
    /// fetch ends. The leases of shards holding none of the page's rows
    /// are ended alongside, without waiting.
    async fn fetch(
        &self,
        sql: &str,
        select: &Select,
        ranked: ResultSet,
        held: &[(usize, usize, u64)],
        mut keys: Vec<Vec<Value>>,
        (start, deadline, protocol, partial): (usize, Instant, u32, bool),
        failed: &mut [Option<ApiError>],
    ) -> Result<ResultSet, ApiError> {
        self.fetches.fetch_add(1, Ordering::Relaxed);
        let mut reads = Vec::new();
        for &(i, r, lease) in held {
            if !keys[i].is_empty() {
                let q = strata::cluster::fetch(select, &self.schema, std::mem::take(&mut keys[i]))?;
                let plan = Plan::of_select(&self.schema, &q);
                reads.push((i, r, lease, Read::new(sql, &Params::new(), Some(&plan), protocol, self.epoch(), || Ok(plan.clone()))?));
            } else if lease != 0 {
                let mut c = self.replicas[i][r].client.clone();
                tokio::spawn(async move { tokio::time::timeout(Duration::from_secs(5), c.release(pb::ReleaseRequest { snapshots: vec![lease] })).await });
            }
        }
        let calls = reads.iter().map(|(i, held_r, lease, read)| {
            let (i, held_r, lease) = (*i, *held_r, *lease);
            async move {
                let a = self.call(i, start, Some(held_r), deadline, |r, mut c| {
                    // Another replica reads its latest version.
                    let snapshot = if r == held_r { lease } else { 0 };
                    async move {
                        let p = self.speak(i, r, &mut c, protocol).await?;
                        c.query(pb::QueryRequest { timeout_ms: ms_left(deadline), shard: true, snapshot, release: snapshot != 0, ..read.request(p) }).await
                    }
                });
                (i, a.await)
            }
        });
        let mut fetched = Vec::new();
        for (i, a) in join_all(calls).await {
            match a {
                Ok((_, a)) => fetched.push(result_from_pb(a)?),
                Err(e) => self.leave_out(partial, i, e, failed)?,
            }
        }
        let (mut rs, lost) = strata::cluster::fill(select, ranked, fetched)?;
        if lost > 0 {
            self.fetch_lost.fetch_add(lost as u64, Ordering::Relaxed);
            rs.stats.plan.push(format!("fetch: {lost} rows of the page left out: their shard did not answer the fetch, or another replica did, without them"));
        }
        Ok(rs)
    }

    /// Writes to every replica of shard `i`; the count is the first's.
    async fn write(&self, i: usize, r: pb::WriteRequest) -> Result<u64, ApiError> {
        let calls = self.replicas[i].iter().enumerate().map(|(k, rep)| {
            let (mut c, r) = (rep.client.clone(), r.clone());
            async move {
                // A replica not asked yet (it was down at connect), or one
                // that failed, is asked again and given the slot map first.
                self.speak(i, k, &mut c, PROTOCOL).await.map_err(|s| self.named(&rep.url, s))?;
                let a = c.write(r).await;
                if a.as_ref().is_err_and(try_elsewhere) {
                    rep.protocol.store(0, Ordering::Relaxed);
                }
                a.map_err(|s| self.named(&rep.url, s))
            }
        });
        let mut written = None;
        for a in join_all(calls).await {
            let n = a?.into_inner().written;
            written.get_or_insert(n);
        }
        Ok(written.unwrap_or(0))
    }

    /// An INSERT goes to the shards of its documents' keys; UPDATE and
    /// DELETE go to every shard, so they take no LIMIT here.
    pub async fn sql(&self, sql: &str, params: &Params) -> Result<u64, ApiError> {
        match bind_write(sql, &self.schema, params)? {
            Some(Write::Insert { docs, conflict }) => {
                let op = match conflict {
                    Conflict::Fail => "insert",
                    Conflict::Replace => "upsert",
                    // The shards' write operations have no insert-if-absent
                    // or update-on-conflict.
                    Conflict::Skip | Conflict::Update(_) => {
                        return Err(ApiError::bad("INSERT ... ON CONFLICT DO NOTHING and DO UPDATE are not supported through a coordinator yet (INSERT OR REPLACE is)"))
                    }
                };
                let ops = docs.iter().map(|d| json!({"op": op, "doc": js::document_json(&self.schema, d)})).collect();
                self.write_ops(ops, None).await
            }
            // Each shard would take its own LIMIT rows.
            Some(Write::Update { select, .. } | Write::Delete { select }) if select.limit.is_some() => {
                Err(ApiError::bad("UPDATE ... LIMIT and DELETE ... LIMIT are not supported through a coordinator yet"))
            }
            Some(_) => {
                let r = pb::WriteRequest { sql: sql.into(), params: params_to_pb(params), epoch: self.epoch(), ..Default::default() };
                let calls = (0..self.replicas.len()).map(|i| self.write(i, r.clone()));
                Ok(join_all(calls).await.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().sum())
            }
            None => Err(ApiError::bad("not a write statement")),
        }
    }

    /// Write operations, grouped by the shard of their key. An idempotency
    /// key goes to each shard as `key/shard`, so a retry after a replica
    /// failed is not applied twice by the replicas that took it.
    pub async fn write_ops(&self, ops: Vec<Json>, key: Option<String>) -> Result<u64, ApiError> {
        let mut by_shard: BTreeMap<usize, Vec<Json>> = BTreeMap::new();
        for op in ops {
            let s = self.shard_of(&op_key(&self.schema, &op)?);
            by_shard.entry(s).or_default().push(op);
        }
        let calls = by_shard.into_iter().map(|(i, ops)| {
            let idempotency_key = key.as_ref().map(|k| format!("{k}/{i}")).unwrap_or_default();
            let r = pb::WriteRequest { ops_json: serde_json::to_vec(&ops).unwrap_or_default(), idempotency_key, epoch: self.epoch(), ..Default::default() };
            self.write(i, r)
        });
        Ok(join_all(calls).await.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().sum())
    }

    /// "ok" when every replica answers, "degraded" when every shard still
    /// has one, "down" when a shard has none. Documents and segments are
    /// counted once per shard.
    pub async fn health(&self) -> Json {
        let calls = self.replicas.iter().map(|rs| {
            join_all(rs.iter().map(|r| {
                let mut c = r.client.clone();
                async move { tokio::time::timeout(Duration::from_secs(2), c.health(pb::Empty {})).await }
            }))
        });
        let now = self.now_ms();
        let mut shards = Vec::new();
        let (mut docs, mut segments, mut all_up, mut every_shard) = (0, 0, true, true);
        for (i, answers) in join_all(calls).await.into_iter().enumerate() {
            let mut replicas = Vec::new();
            // Counted from a replica that is "ok", else from any answering:
            // one that cannot write ("failing") still answers queries.
            let (mut counted, mut answering) = (None, None);
            for (r, a) in self.replicas[i].iter().zip(answers) {
                let mut j = match a {
                    Ok(Ok(h)) => {
                        let h = h.into_inner();
                        if h.status == "ok" {
                            counted.get_or_insert((h.docs, h.segments));
                        }
                        answering.get_or_insert((h.docs, h.segments));
                        json!({"url": r.url, "status": h.status, "docs": h.docs, "segments": h.segments, "txn": h.txn})
                    }
                    Ok(Err(e)) => json!({"url": r.url, "status": "down", "error": e.message()}),
                    Err(_) => json!({"url": r.url, "status": "down", "error": "no answer within 2 s"}),
                };
                all_up &= j["status"] == "ok";
                if r.avoid_until.load(Ordering::Relaxed) > now {
                    j["avoided"] = json!(true);
                }
                replicas.push(j);
            }
            let counted = counted.or(answering);
            every_shard &= counted.is_some();
            let (d, s) = counted.unwrap_or((0, 0));
            docs += d;
            segments += s;
            let status = if counted.is_none() { "down" } else { "ok" };
            let mut shard = json!({"url": self.shards[i], "status": status, "docs": d, "segments": s});
            if self.replicas[i].len() > 1 {
                shard["replicas"] = json!(replicas);
            } else if let Some(err) = replicas[0].get("error") {
                shard["error"] = err.clone();
            }
            if let Some(txn) = replicas[0].get("txn") {
                shard["txn"] = txn.clone();
            }
            shards.push(shard);
        }
        let status = if all_up { "ok" } else if every_shard { "degraded" } else { "down" };
        json!({
            "status": status, "role": "coordinator", "docs": docs, "segments": segments, "txn": 0, "shards": shards,
            "retries": self.retries.load(Ordering::Relaxed), "hedges": self.hedges.load(Ordering::Relaxed), "protocol": self.protocol(), "slot_epoch": self.epoch(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hedges_stay_within_their_share() {
        let b = HedgeBudget::new();
        let mut hedged = 0;
        for _ in 0..1000 {
            b.earn();
            hedged += b.spend() as usize;
        }
        // The saved burst, then one call in twenty.
        assert!((1000 / 20..=HEDGE_BURST as usize + 1000 / 20).contains(&hedged), "{hedged}");
        assert!(!b.spend());
    }

    #[test]
    fn an_overloaded_replica_is_not_retried_elsewhere() {
        assert!(!try_elsewhere(&tonic::Status::resource_exhausted("too many queries running")));
        assert!(try_elsewhere(&tonic::Status::unavailable("connection refused")));
        assert!(!try_elsewhere(&tonic::Status::invalid_argument("bad SQL")));
        // The server answers too many queries with 429, which crosses gRPC
        // as ResourceExhausted and back.
        let s = tonic::Status::from(ApiError { status: 429, message: "too many queries running".into() });
        assert_eq!(s.code(), Code::ResourceExhausted);
        assert_eq!(ApiError::from(s).status, 429);
    }
}
