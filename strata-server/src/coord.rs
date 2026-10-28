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
//! Writes go to the shard of their key ([`shard_of`]), on every replica;
//! UPDATE and DELETE statements go to every shard. A write that spans
//! shards is one transaction per replica, not one across them, and a
//! replica that misses a write (it was down) is not caught up: rebuild it.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::future::join_all;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::{json, Value as Json};
use strata::query::exec::ResultSet;
use strata::query::stats::TermStats;
use strata::query::{bind_write, Params, Select, Write};
use strata::schema::Schema;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use tonic::Code;

use crate::auth::Secret;
use crate::grpc::{params_to_pb, result_from_pb};
use crate::metrics::Family;
use crate::pb::{self, strata_client::StrataClient};
use crate::service::{op_key, shard_of, QueryReq};
use crate::{json as js, ApiError};

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
}

impl Default for CoordOptions {
    fn default() -> CoordOptions {
        CoordOptions { hedge: Duration::from_millis(100), cool_off: Duration::from_secs(5), shard_timeout: None, partial: false, token: None, ca: None }
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
                rs.push(Replica { url: url.to_string(), client, avoid_until: AtomicU64::new(0) });
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
                async move { tokio::time::timeout(Duration::from_secs(5), c.schema(pb::Empty {})).await }
            }))
            .await;
            let mut errors = Vec::new();
            for (r, a) in rs.iter().zip(answers) {
                match a {
                    Ok(Ok(s)) => schemas.push(s.into_inner().schema_json),
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
        ]
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
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
    async fn shard_stats(&self, sql: &str, params: &Params, which: Vec<usize>, start: usize, deadline: Instant) -> Vec<(usize, Result<(TermStats, usize, u64), ApiError>)> {
        let req = pb::QueryRequest { sql: sql.into(), params: params_to_pb(params), timeout_ms: ms_left(deadline), ..Default::default() };
        let calls = which.into_iter().map(|i| {
            let req = req.clone();
            async move {
                let a = self
                    .call(i, start, None, deadline, |_, mut c| {
                        let req = req.clone();
                        async move { c.term_stats(req).await }
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

    /// Summed statistics, from every shard.
    pub async fn term_stats(&self, sql: &str, params: &Params, deadline: Instant) -> Result<(TermStats, Vec<u64>), ApiError> {
        let start = self.turn.fetch_add(1, Ordering::Relaxed);
        let mut sum = TermStats::default();
        let mut leases = Vec::new();
        for (_, a) in self.shard_stats(sql, params, (0..self.replicas.len()).collect(), start, deadline).await {
            let (s, _, lease) = a?;
            sum.add(&s);
            leases.push(lease);
        }
        Ok((sum, leases))
    }

    pub async fn query(&self, req: &QueryReq, select: &Select, deadline: Instant) -> Result<Merged, ApiError> {
        let partial = req.partial.unwrap_or(self.opts.partial);
        let n = self.replicas.len();
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
        // A shard that cannot answer is left out of a partial answer (and of
        // the next second's), but a request it rejects fails the query.
        let missing = |i: usize, e: ApiError, failed: &mut Vec<Option<ApiError>>| {
            if !partial || e.status == 400 {
                return Err(e);
            }
            self.skip_until[i].store(self.now_ms() + 1000, Ordering::Relaxed);
            failed[i] = Some(e);
            Ok(())
        };
        let stats = match &req.stats {
            Some(s) => Some(s.clone()),
            None if ranked_text && n > 1 => {
                let mut sum = TermStats::default();
                let which = (0..n).filter(|&i| failed[i].is_none()).collect();
                for (i, a) in self.shard_stats(&req.sql, &req.params, which, start, deadline).await {
                    match a {
                        Ok((s, r, lease)) => {
                            sum.add(&s);
                            leased[i] = Some((r, lease));
                        }
                        Err(e) => missing(i, e, &mut failed)?,
                    }
                }
                Some(sum)
            }
            None => None,
        };
        let stats_json = stats.map(|s| serde_json::to_vec(&s).unwrap_or_default()).unwrap_or_default();
        let leased = &leased;
        let calls = (0..n).filter(|&i| failed[i].is_none()).map(|i| {
            let stats_json = stats_json.clone();
            let first = leased[i].map(|(r, _)| r);
            async move {
                let rs = self.call(i, start, first, deadline, |r, mut c| {
                    let q = pb::QueryRequest {
                        sql: req.sql.clone(),
                        params: params_to_pb(&req.params),
                        timeout_ms: ms_left(deadline),
                        shard: true,
                        stats_json: stats_json.clone(),
                        // The lease holds on the replica that granted it; another
                        // replica scores from its latest version.
                        snapshot: leased[i].filter(|(lr, _)| *lr == r).map_or(0, |(_, l)| l),
                    };
                    async move { c.query(q).await }
                });
                (i, rs.await)
            }
        });
        let mut shards = Vec::new();
        for (i, a) in join_all(calls).await {
            match a {
                Ok((_, a)) => shards.push(result_from_pb(a)?),
                Err(e) => missing(i, e, &mut failed)?,
            }
        }
        if shards.is_empty() {
            let e = failed.into_iter().flatten().next();
            return Err(e.unwrap_or_else(|| ApiError::internal("no shard answered")));
        }
        let missing = failed.iter().enumerate().filter(|(_, f)| f.is_some()).map(|(i, _)| self.shards[i].clone()).collect();
        Ok(Merged { rs: strata::cluster::merge(select, shards)?, missing })
    }

    /// Writes to every replica of shard `i`; the count is the first's.
    async fn write(&self, i: usize, r: pb::WriteRequest) -> Result<u64, ApiError> {
        let calls = self.replicas[i].iter().map(|rep| {
            let (mut c, r) = (rep.client.clone(), r.clone());
            async move { c.write(r).await.map_err(|s| self.named(&rep.url, s)) }
        });
        let mut written = None;
        for a in join_all(calls).await {
            let n = a?.into_inner().written;
            written.get_or_insert(n);
        }
        Ok(written.unwrap_or(0))
    }

    /// An INSERT goes to the shards of its documents' keys; UPDATE and
    /// DELETE go to every shard.
    pub async fn sql(&self, sql: &str, params: &Params) -> Result<u64, ApiError> {
        match bind_write(sql, &self.schema, params)? {
            Some(Write::Insert { docs, replace }) => {
                let op = if replace { "upsert" } else { "insert" };
                let ops = docs.iter().map(|d| json!({"op": op, "doc": js::document_json(&self.schema, d)})).collect();
                self.write_ops(ops, None).await
            }
            Some(_) => {
                let r = pb::WriteRequest { sql: sql.into(), params: params_to_pb(params), ..Default::default() };
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
            let s = shard_of(&op_key(&self.schema, &op)?, self.replicas.len());
            by_shard.entry(s).or_default().push(op);
        }
        let calls = by_shard.into_iter().map(|(i, ops)| {
            let idempotency_key = key.as_ref().map(|k| format!("{k}/{i}")).unwrap_or_default();
            let r = pb::WriteRequest { ops_json: serde_json::to_vec(&ops).unwrap_or_default(), idempotency_key, ..Default::default() };
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
            "retries": self.retries.load(Ordering::Relaxed), "hedges": self.hedges.load(Ordering::Relaxed),
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
