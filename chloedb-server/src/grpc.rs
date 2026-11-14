//! The service over gRPC (`proto/strata.proto`).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chloedb::query::exec::ResultSet;
use chloedb::query::ir::{self, Plan};
use chloedb::query::Params;
use chloedb::value::Value;
use futures::StreamExt;
use tonic::{Request, Response, Status};
use tracing::Instrument;

use crate::pb::{self, value::Kind};
use crate::cores::slots::Moved;
use crate::service::{After, QueryReq, Service, Stream};
use crate::keys::{self, KeySet, Principal, Scope};
use crate::{obs, plan_pb, ApiError};

pub fn to_pb(v: &Value) -> pb::Value {
    let kind = match v {
        Value::Null => Kind::Null(true),
        Value::Int(x) => Kind::Int(*x),
        Value::Float(x) => Kind::Float(*x),
        Value::Str(s) => Kind::Str(s.clone()),
        Value::Vector(v) => Kind::Vector(pb::Floats { v: v.to_vec() }),
        Value::List(xs) => Kind::List(pb::Values { v: xs.iter().map(to_pb).collect() }),
        Value::Bool(b) => Kind::Boolean(*b),
        Value::Timestamp(t) => Kind::Timestamp(*t),
    };
    pb::Value { kind: Some(kind) }
}

pub fn from_pb(v: pb::Value) -> Value {
    match v.kind {
        None | Some(Kind::Null(_)) => Value::Null,
        Some(Kind::Int(x)) => Value::Int(x),
        Some(Kind::Float(x)) => Value::Float(x),
        Some(Kind::Str(s)) => Value::Str(s),
        Some(Kind::Vector(f)) => Value::Vector(f.v.into()),
        Some(Kind::List(xs)) => Value::List(xs.v.into_iter().map(from_pb).collect()),
        Some(Kind::Boolean(b)) => Value::Bool(b),
        Some(Kind::Timestamp(t)) => Value::Timestamp(t),
    }
}

pub fn params_to_pb(p: &Params) -> HashMap<String, pb::Value> {
    p.0.iter().map(|(k, v)| (k.clone(), to_pb(v))).collect()
}

pub fn params_from_pb(p: HashMap<String, pb::Value>) -> Params {
    Params(p.into_iter().map(|(k, v)| (k, from_pb(v))).collect())
}

fn json_bytes<T: serde::Serialize>(x: &T) -> Vec<u8> {
    serde_json::to_vec(x).unwrap_or_default()
}

fn from_json<T: serde::de::DeserializeOwned + Default>(b: &[u8], what: &str) -> Result<T, ApiError> {
    if b.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(b).map_err(|e| ApiError::bad(format!("bad {what}: {e}")))
}

pub fn result_to_pb(rs: &ResultSet, took_ms: f64) -> pb::QueryResponse {
    pb::QueryResponse {
        columns: rs.columns.clone(),
        rows: rs.rows.iter().map(|r| pb::Row { v: r.iter().map(to_pb).collect() }).collect(),
        keys: rs.keys.iter().map(to_pb).collect(),
        scores: rs.scores.clone(),
        parts_json: rs.parts.as_ref().map(json_bytes).unwrap_or_default(),
        stats_json: json_bytes(&rs.stats),
        took_ms,
        snapshot: 0,
        arrow: Vec::new(),
        stale: false,
        cursor: String::new(),
    }
}

pub fn result_from_pb(r: pb::QueryResponse) -> Result<ResultSet, ApiError> {
    Ok(ResultSet {
        columns: r.columns,
        rows: r.rows.into_iter().map(|r| r.v.into_iter().map(from_pb).collect()).collect(),
        keys: r.keys.into_iter().map(from_pb).collect(),
        scores: r.scores,
        parts: if r.parts_json.is_empty() { None } else { Some(from_json(&r.parts_json, "fusion parts")?) },
        stats: from_json(&r.stats_json, "statistics")?,
    })
}

/// The protocol versions this server speaks (`proto/strata.proto`): 1
/// takes SQL or a JSON plan, 2 a typed plan, 3 holds a query's snapshot
/// for the fetch that follows, 4 places keys in slots, 5 reads the writes
/// a read must see (`min_seq_no`), which a replica waits for, 6 keeps
/// stream sources' offsets ([`SOURCES_PROTOCOL`]), 7 answers the next
/// codes of semantic-ID prefixes ([`SIDNEXT_PROTOCOL`]), 8 counts the
/// rows of a guard's levels ([`GUARD_PROTOCOL`]), 9 keeps the cluster
/// map, every shard's servers, and takes `Configure` ([`CLUSTER_PROTOCOL`]),
/// 10 answers a compound query's leaf as Arrow columns
/// ([`ARROW_PROTOCOL`]), 11 reads rows by key without SQL (`Get`,
/// [`GET_PROTOCOL`]), 12 names the version BM25 statistics came from,
/// so that a coordinator can keep them ([`STATS_VERSION_PROTOCOL`]), 13
/// answers a shard's feed (`Feed`, [`FEED_PROTOCOL`]), 14 starts a feed
/// with a copy of the shard's rows ([`FEED_COPY_PROTOCOL`]), 15 copies
/// only rows changed since a transaction and compares keys
/// ([`FEED_KEYS_PROTOCOL`]), 16 keeps the stream offsets of slots that
/// move ([`MOVE_OFFSETS_PROTOCOL`]), 17 holds clients' writes on a mark
/// and keeps promotions ([`HOLD_PROTOCOL`]), 18 scrolls a shard's rows
/// for a coordinator ([`SCROLL_PROTOCOL`]), 19 checks a write without
/// applying it ([`VALIDATE_PROTOCOL`]), and 20 compares the transaction a
/// `Replicate` follows by its digest ([`SAME_TERM_PROTOCOL`]), and 21
/// answers what a write's operations found ([`RECEIPT_PROTOCOL`]).
pub const MIN_PROTOCOL: u32 = 1;
pub const PROTOCOL: u32 = 21;

/// The protocol whose writes carry stream sources' offsets
/// ([`crate::service::op_source`]), and that answers `Sources`.
pub const SOURCES_PROTOCOL: u32 = 6;

/// The protocol of `SidNext`, the next codes of semantic-ID prefixes.
pub const SIDNEXT_PROTOCOL: u32 = 7;

/// The protocol of `GuardCounts`, the rows of a guard's levels.
pub const GUARD_PROTOCOL: u32 = 8;

/// The protocol of the cluster map ([`crate::slots::SlotMap::replicas`]):
/// `Slots` asked without a map answers the shard's, and `Configure` sets a
/// server's part in replication.
pub const CLUSTER_PROTOCOL: u32 = 9;

/// The protocol whose shards answer a compound query's leaf as Arrow
/// columns (`QueryRequest.arrow`, `QueryResponse.arrow`).
pub const ARROW_PROTOCOL: u32 = 10;

/// The protocol of `Get`, rows by primary key without SQL.
pub const GET_PROTOCOL: u32 = 11;

/// The protocol whose `TermStats` answers the version of the statistics
/// (`TermStatsResponse.version`) and whose queries run only on that
/// version when they name it (`QueryRequest.stats_version`, else
/// `QueryResponse.stale`): what a coordinator's cache of term statistics
/// needs ([`crate::coord::StatsCache`]).
pub const STATS_VERSION_PROTOCOL: u32 = 12;

/// The protocol of `Feed`, a shard's changes for another cluster to follow
/// ([`crate::service::Service::feed`]).
pub const FEED_PROTOCOL: u32 = 13;

/// The protocol whose `Feed` starts with a copy of the shard's rows
/// (`FeedRequest.copy`): a follower's first start.
pub const FEED_COPY_PROTOCOL: u32 = 14;

/// The protocol whose copy feed takes `since` and `digests`, and that
/// answers `FeedKeys`: what a follower that fell behind its source's log
/// catches up by.
pub const FEED_KEYS_PROTOCOL: u32 = 15;

/// The protocol whose slot moves carry the stream sources' offsets of the
/// slots on the shard they leave ([`crate::slots::Move::offsets`],
/// `SourcesRequest.moving`), which the shard they go to keeps for them
/// ([`crate::service::arrivals`]).
pub const MOVE_OFFSETS_PROTOCOL: u32 = 16;

/// The protocol whose shards hold their clients' writes on a mark
/// (`_strata/hold`, [`crate::service::feed::HOLD`]), say so in their feeds,
/// and keep a promotion from a cluster they followed: a switch-over to a
/// follower, and back. Nothing new on the wire but the marks, which an
/// older shard refuses as its own.
pub const HOLD_PROTOCOL: u32 = 17;

/// The protocol whose shards scroll a query's rows for a coordinator
/// (`QueryRequest.scroll`, `page_size`, `cursor`, `keep_alive_ms`,
/// `QueryResponse.cursor`, `ReleaseRequest.cursors`): what a scroll over
/// shards reads past the window by ([`crate::coord::ShardScroll`]).
pub const SCROLL_PROTOCOL: u32 = 18;

/// The protocol whose shards check a write's operations without applying
/// them (`WriteRequest.validate`, [`crate::service::Service::validate_ops`]),
/// which a coordinator asks of every shard of a write that spans several
/// before it sends the write ([`crate::coord::Coordinator::write_ops`]). A
/// shard of an older protocol is not asked: its part may still be
/// refused after others applied theirs, as before.
pub const VALIDATE_PROTOCOL: u32 = 19;

/// The protocol whose `Replicate` carries a digest of the transaction it
/// follows (`ReplicateRequest.prev_digest`, [`crate::cores::shard::digest`]),
/// which a replica holding a transaction of that term and number compares
/// with its own, refusing the call when they differ
/// (`ReplicateResponse.differs`, [`crate::cores::shard::Replica::follows`]):
/// a copy that holds another transaction than its primary's under one term
/// and number is found on the call that would build on it, and sent a copy
/// of the primary. Replication takes no protocol version, as before: a
/// replica of an older protocol ignores the digest, and a primary of an
/// older one sends none (0), so that either applies by term and number
/// alone, as before. Nothing gates on the number; it names the change.
pub const SAME_TERM_PROTOCOL: u32 = 20;

/// The protocol whose write answers say what the operations found
/// (`WriteResponse.receipt` and its counts, [`crate::service::Receipt`]),
/// and whose shards take an operation's `"if_missing"` and `"strict"`: a
/// `set`, `add` or `delete` refused when no row has its key, an operation
/// refused when it would replace a row or write a key twice. An older
/// shard ignores both fields, and answers no counts, so a coordinator
/// sends a strict write only to shards of this protocol.
pub const RECEIPT_PROTOCOL: u32 = 21;

/// The most bytes of a feed one `FeedChunk` carries.
const FEED_CHUNK: usize = 1 << 20;

/// The first protocol all of whose coordinators fuse a rank-safe boost
/// (`bm25(body, :t) * decay(year, 2015, 3)`) from the rows each shard's
/// bound found alone ([`chloedb::cluster::merge`]): rank-safe boosts came
/// while protocol 9 was the newest, so a coordinator of 10 or later has
/// them, and a shard answers it without the boost's inputs' top rows,
/// which only an older coordinator fuses from
/// ([`chloedb::query::exec::ExecOptions::boost_inputs`]). Not a protocol of
/// its own: nothing new goes on the wire.
pub const BOOSTS_PROTOCOL: u32 = ARROW_PROTOCOL;

/// What `Get` answers ([`crate::service::KeyRows`]).
pub fn get_response(got: crate::service::KeyRows) -> pb::GetResponse {
    let rows = got
        .rows
        .into_iter()
        .map(|r| match r {
            Some((v, seq_no)) => pb::KeyRow { found: true, v: v.iter().map(to_pb).collect(), seq_no },
            None => pb::KeyRow::default(),
        })
        .collect();
    pb::GetResponse { columns: got.columns, rows }
}

/// The rows of a `Get` answer: per key, its row's values and number.
pub fn rows_from_pb(r: pb::GetResponse) -> Vec<Option<(Vec<Value>, u64)>> {
    r.rows.into_iter().map(|r| r.found.then(|| (r.v.into_iter().map(from_pb).collect(), r.seq_no))).collect()
}

/// A write's answer ([`crate::service::Written`]).
pub fn write_response(w: crate::service::Written) -> pb::WriteResponse {
    let r = w.receipt.unwrap_or_default();
    pb::WriteResponse {
        written: w.rows,
        seq_no: w.seq_no,
        seq_nos: w.seq_nos,
        seq_term: w.seq_term,
        seq_terms: w.seq_terms,
        skipped: w.skipped,
        receipt: w.receipt.is_some(),
        inserted: r.inserted,
        replaced: r.replaced,
        updated: r.updated,
        deleted: r.deleted,
        collapsed: r.collapsed,
        not_found: r.not_found,
        ..Default::default()
    }
}

/// What [`pb::strata_server::Strata::hello`] answers, but what is the
/// server's own: its slot map's epoch, role, term and last number.
pub fn hello() -> pb::HelloResponse {
    pb::HelloResponse {
        min_protocol: MIN_PROTOCOL,
        max_protocol: PROTOCOL,
        plan_version: ir::VERSION,
        plan_features: ir::FEATURES.iter().map(|f| f.to_string()).collect(),
        server: format!("chloedb-server {}", env!("CARGO_PKG_VERSION")),
        ..Default::default()
    }
}

pub fn query_req(r: pb::QueryRequest) -> Result<QueryReq, ApiError> {
    // The answer cache knows a coordinator's query by its SQL, the plan
    // and the statistics it sent ([`crate::answers`]).
    let cache_key = (!crate::answers::volatile(&r.sql)).then(|| {
        let plan = r.plan.as_ref().map(prost::Message::encode_to_vec).unwrap_or_default();
        crate::answers::KeyBuilder::default()
            .part(b"grpc")
            .part(&r.protocol.to_le_bytes())
            .part(r.sql.as_bytes())
            .part(&plan)
            .part(&r.plan_json)
            .part(&r.stats_json)
            .finish()
    });
    let plan = request_plan(r.protocol, r.plan, &r.plan_json)?;
    Ok(QueryReq {
        sql: r.sql,
        params: params_from_pb(r.params),
        timeout_ms: r.timeout_ms as u64,
        shard: r.shard,
        stats: if r.stats_json.is_empty() { None } else { Some(from_json(&r.stats_json, "statistics")?) },
        snapshot: r.snapshot,
        partial: None,
        plan: plan.map(Arc::new),
        hold: r.hold,
        release: r.release,
        after: After::termed(r.min_seq_no, r.min_seq_nos, r.min_seq_term, r.min_seq_terms)?,
        // A coordinator's query that asked for its steps' CPU time; a shard
        // also measures it when set to for every query.
        profile: r.profile,
        quiet: false,
        // A coordinator's leaf: Arrow columns for any rows.
        arrow: r.arrow.then_some(0),
        // Set by the query itself, for a client's.
        reads: false,
        stats_version: r.stats_version,
        boosts_alone: r.shard && r.protocol >= BOOSTS_PROTOCOL,
        // A scroll's start names its owner ([`Grpc::query`]).
        scroll: None,
        skip_steps: r.skip_steps,
        cache_key,
        // The caller, whose model budget llm() takes from ([`Grpc::query`]).
        who: None,
        // A client's, held to the server's limits.
        own_read: false,
    })
}

/// A request's plan, if it has one: typed (`plan`, protocol 2) or JSON
/// (`plan_json`, protocol 1). A protocol newer than this server's is
/// refused, as the caller should have asked [`hello`] first.
pub fn request_plan(protocol: u32, plan: Option<pb::Plan>, json: &[u8]) -> Result<Option<Plan>, ApiError> {
    if protocol > PROTOCOL {
        return Err(ApiError::bad(format!("protocol {protocol} is newer than this server's ({MIN_PROTOCOL} to {PROTOCOL})")));
    }
    match plan {
        Some(p) => Ok(Some(plan_pb::from_pb_plan(p)?)),
        None => plan_of(json),
    }
}

/// A request's plan (`plan_json`), if it has one.
pub fn plan_of(json: &[u8]) -> Result<Option<Plan>, ApiError> {
    if json.is_empty() {
        return Ok(None);
    }
    let j: serde_json::Value = from_json(json, "plan")?;
    Ok(Some(Plan::from_json(&j)?))
}

pub struct Grpc(pub Arc<Service>);

/// A status code's name, as the metrics label it.
pub fn code_name(c: tonic::Code) -> &'static str {
    use tonic::Code::*;
    match c {
        Ok => "Ok",
        Cancelled => "Cancelled",
        Unknown => "Unknown",
        InvalidArgument => "InvalidArgument",
        DeadlineExceeded => "DeadlineExceeded",
        NotFound => "NotFound",
        AlreadyExists => "AlreadyExists",
        PermissionDenied => "PermissionDenied",
        ResourceExhausted => "ResourceExhausted",
        FailedPrecondition => "FailedPrecondition",
        Aborted => "Aborted",
        OutOfRange => "OutOfRange",
        Unimplemented => "Unimplemented",
        Internal => "Internal",
        Unavailable => "Unavailable",
        DataLoss => "DataLoss",
        Unauthenticated => "Unauthenticated",
    }
}

/// The least a method's credential needs ([`crate::keys`]): the internal
/// methods the root (the cluster's token, or the token of a server
/// without one), writes a write key, the log store and a profile an admin
/// key (the log store shows every client's SQL; a profile as
/// `/v1/profile` over HTTP),
/// the rest any key. What a call then does (a `Write` of a
/// table's statement needs an admin key) the service checks, as for HTTP.
/// Every method is named, so that a new one must say what it needs.
fn method_scope(method: &str) -> Option<Scope> {
    match method {
        "Replicate" | "Restore" | "Verify" | "Vote" | "Slots" | "Configure" | "ExportSlots" | "FreezeSlots" | "CleanupSlots" | "Write (moved)" | "Keys" | "Feed" | "FeedKeys" => None,
        // LogsExport: OpenTelemetry logs in ([`crate::otlp::Logs`]).
        "Write" | "WriteStream" | "LogsExport" => Some(Scope::Write),
        "Logs" | "Profile" => Some(Scope::Admin),
        "Query" | "TermStats" | "GuardCounts" | "SidNext" | "Sources" | "Schema" | "Hello" | "Release" | "Health" | "Get" => Some(Scope::Read),
        // An unknown method is the root's.
        _ => None,
    }
}

/// Who a call is from, and whether that may call its method.
fn may_call<T>(req: &Request<T>, method: &str) -> (Principal, Result<(), ApiError>) {
    let who = req.extensions().get::<Principal>().cloned().unwrap_or_else(Principal::open);
    let r = match method_scope(method) {
        None if !who.root => Err(ApiError { code: None, status: 403, message: format!("{method} is a call between the servers of a cluster: it needs the cluster's token ({:?} may not make it)", who.name) }),
        None => Ok(()),
        Some(need) => who.may(need, method),
    };
    (who, r)
}

/// Keeps a call in the audit log ([`crate::keys::Audit`]), as HTTP keeps
/// a request: a refused one, or one the service noted ([`keys::note`]).
fn audit(svc: &Service, who: &Principal, method: &str, status: u16, t0: Instant, detail: Option<String>) {
    if status != 401 && status != 403 && detail.is_none() {
        return;
    }
    svc.audit.add(keys::Entry {
        ts_ms: crate::telemetry::now_ms(),
        who: who.name.clone(),
        key: who.key.clone(),
        scope: Some(who.scope.name()),
        method: "gRPC".into(),
        route: method.to_string(),
        status,
        ms: t0.elapsed().as_secs_f64() * 1e3,
        request_id: obs::current().map(|id| id.to_string()),
        kind: (if status == 401 || status == 403 { keys::REFUSAL } else { keys::CHANGE }).into(),
        detail,
        ..Default::default()
    });
}

impl Grpc {
    /// Serves one call: gives it its id (the caller's `x-request-id`, or a
    /// new one, sent back in the answer's metadata), runs it in a span that
    /// names it, counts it and logs it ([`crate::obs`]).
    pub(crate) async fn serve<T, R, Fut>(&self, method: &'static str, req: Request<T>, f: impl FnOnce(Arc<Service>, Principal, T) -> Fut) -> Result<Response<R>, Status>
    where
        Fut: Future<Output = Result<R, ApiError>>,
    {
        let id = obs::request_id(req.metadata().get(obs::HEADER).and_then(|v| v.to_str().ok()));
        let span = tracing::info_span!("request", id = %id, method);
        let t0 = Instant::now();
        let (who, mut may) = may_call(&req, method);
        // A follow process's writes, through a coordinator
        // ([`crate::service::FOLLOW`]).
        let follow: Option<Arc<str>> = req.metadata().get(crate::service::FOLLOW_HEADER).and_then(|v| v.to_str().ok()).map(Into::into);
        if follow.is_some() && !who.root && may.is_ok() {
            may = Err(ApiError { code: None, status: 403, message: format!("{} is a coordinator's, with the cluster's token", crate::service::FOLLOW_HEADER) });
        }
        if let Err(e) = may {
            self.0.metrics.grpc(method, "PermissionDenied", 0.0);
            obs::scope(Some(id.clone()), async { audit(&self.0, &who, method, e.status, t0, None) }).await;
            return Err(Status::from(e));
        }
        // Who the call's queries are from ([`crate::telemetry::Caller`]).
        let caller = {
            let md = |k: &str| req.metadata().get(k).and_then(|v| v.to_str().ok()).map(String::from);
            let app = md("x-strata-app").or_else(|| md("user-agent").as_deref().and_then(keys::product));
            let r = keys::Caller { addr: req.remote_addr().map(|a| a.ip().to_string()), client: md("x-strata-client"), app, reason: None, ..Default::default() };
            crate::telemetry::Caller::of(&who, Some(r))
        };
        // A part whose deadline is its client's own (the coordinator says so):
        // one that passes is the client's doing, counted and logged so.
        let clients = req.metadata().get(crate::service::CLIENT_DEADLINE).is_some_and(|v| v == "client");
        // The call itself, on the heap: a query's future is many kilobytes,
        // and each wrapper below would otherwise move it again (a shard's
        // query spent an eighth of its time copying it).
        let call = Box::pin(f(self.0.clone(), who.clone(), req.into_inner()));
        let call = keys::noting(crate::service::DEADLINE.scope(crate::service::Deadline::of_part(clients), crate::service::ROUTED.scope(std::cell::Cell::new(0), call)));
        let call = crate::telemetry::calling(Arc::new(caller), crate::service::FOLLOW.scope(follow, crate::service::OWN.scope(who.root, call)));
        let (r, note) = crate::telemetry::traced(crate::telemetry::Trace::new(), obs::scope(Some(id.clone()), call)).instrument(span.clone()).await;
        let status = r.as_ref().map_or_else(|e| e.status, |_| 200);
        obs::scope(Some(id.clone()), async { audit(&self.0, &who, method, status, t0, note) }).await;
        let secs = t0.elapsed().as_secs_f64();
        let _in = span.enter();
        let (code, r) = match r {
            Ok(x) => {
                // A coordinator's query of the logs, as the viewer's own
                // requests over HTTP, is not logged ([`crate::logstore`]).
                if method != "Logs" {
                    tracing::debug!(secs, "answered");
                }
                ("Ok", Ok(Response::new(x)))
            }
            Err(e) => {
                if e.status == 504 && clients {
                    static LIMIT: obs::RateLimit = obs::RateLimit::new(10);
                    if let Some(held) = LIMIT.admit() {
                        tracing::warn!(status = e.status, error = %e.message, held_back = held, "the client's own deadline passed");
                    }
                } else if e.status >= 500 {
                    obs::server_error(e.status, &e.message);
                }
                let s = Status::from(e);
                (code_name(s.code()), Err(s))
            }
        };
        self.0.metrics.grpc(method, code, secs);
        let v = id.parse::<tonic::metadata::AsciiMetadataValue>().ok();
        match (r, v) {
            (Ok(mut a), Some(v)) => {
                a.metadata_mut().insert(obs::HEADER, v);
                Ok(a)
            }
            (Err(mut s), Some(v)) => {
                s.metadata_mut().insert(obs::HEADER, v);
                Err(s)
            }
            (r, None) => r,
        }
    }
}

#[tonic::async_trait]
impl pb::strata_server::Strata for Grpc {
    async fn query(&self, req: Request<pb::QueryRequest>) -> Result<Response<pb::QueryResponse>, Status> {
        self.serve("Query", req, |svc, who, r| async move {
            // A scroll's next page ([`SCROLL_PROTOCOL`]), for who started it.
            let owner = crate::service::scroll_owner(&who);
            let keep_alive = (r.keep_alive_ms > 0).then(|| Duration::from_millis(r.keep_alive_ms));
            if !r.cursor.is_empty() {
                let a = svc.scroll_next(&r.cursor, &owner, keep_alive, r.timeout_ms as u64, Some(r.page_size as usize)).await?;
                return Ok(pb::QueryResponse { cursor: a.cursor.unwrap_or_default(), ..result_to_pb(&a.rs, a.took_ms) });
            }
            svc.check_epoch(r.epoch)?;
            let (scroll, page) = (r.scroll, r.page_size as usize);
            let mut q = query_req(r)?;
            // llm() takes from the caller's model budget.
            q.who = Some(who.clone());
            if scroll {
                q.scroll = Some(crate::service::ScrollAsk { page, keep_alive, owner });
            }
            let a = svc.query(q).await?;
            Ok(pb::QueryResponse { snapshot: a.lease, arrow: a.arrow, stale: a.stale, cursor: a.cursor.unwrap_or_default(), ..result_to_pb(&a.rs, a.took_ms) })
        })
        .await
    }

    async fn logs(&self, req: Request<pb::QueryRequest>) -> Result<Response<pb::QueryResponse>, Status> {
        self.serve("Logs", req, |svc, _who, r| async move {
            let store = svc.log_store.clone().ok_or_else(crate::logstore::none_here)?;
            let (rs, took_ms) = store.query(&r.sql, &params_from_pb(r.params), r.shard, svc.timeout_of(r.timeout_ms as u64)).await?;
            Ok(result_to_pb(&rs, took_ms))
        })
        .await
    }

    async fn term_stats(&self, req: Request<pb::QueryRequest>) -> Result<Response<pb::TermStatsResponse>, Status> {
        self.serve("TermStats", req, |svc, _who, r| async move {
            svc.check_epoch(r.epoch)?;
            let plan = request_plan(r.protocol, r.plan, &r.plan_json)?;
            let after = After::termed(r.min_seq_no, r.min_seq_nos, r.min_seq_term, r.min_seq_terms)?;
            let (stats, snapshot, version) = svc.term_stats(&r.sql, plan.as_ref(), &params_from_pb(r.params), &after, r.timeout_ms as u64).await?;
            Ok(pb::TermStatsResponse { stats_json: json_bytes(&stats), snapshot, version })
        })
        .await
    }

    async fn guard_counts(&self, req: Request<pb::QueryRequest>) -> Result<Response<pb::GuardCountsResponse>, Status> {
        self.serve("GuardCounts", req, |svc, _who, r| async move {
            svc.check_epoch(r.epoch)?;
            let plan = request_plan(r.protocol, r.plan, &r.plan_json)?;
            let after = After::termed(r.min_seq_no, r.min_seq_nos, r.min_seq_term, r.min_seq_terms)?;
            let (need, counts) = svc.guard_counts(&r.sql, plan.as_ref(), &params_from_pb(r.params), &after, r.timeout_ms as u64).await?;
            Ok(pb::GuardCountsResponse { need: need as u32, counts })
        })
        .await
    }

    async fn sid_next(&self, req: Request<pb::SidNextRequest>) -> Result<Response<pb::SidNextResponse>, Status> {
        self.serve("SidNext", req, |svc, _who, r| async move {
            let q = r.query.unwrap_or_default();
            svc.check_epoch(q.epoch)?;
            let plan = request_plan(q.protocol, q.plan, &q.plan_json)?;
            let after = After::termed(q.min_seq_no, q.min_seq_nos, q.min_seq_term, q.min_seq_terms)?;
            let next = svc.sid_next(&q.sql, plan.as_ref(), &params_from_pb(q.params), &r.field, &r.prefixes, &after, q.timeout_ms as u64).await?;
            let next = next.into_iter().map(|cs| {
                let (codes, counts) = cs.into_iter().unzip();
                pb::SidChildren { codes, counts }
            });
            Ok(pb::SidNextResponse { next: next.collect() })
        })
        .await
    }

    async fn write(&self, req: Request<pb::WriteRequest>) -> Result<Response<pb::WriteResponse>, Status> {
        if req.get_ref().moved {
            // Rows of moving slots: a call between the servers of a cluster.
            let (who, may) = may_call(&req, "Write (moved)");
            if let Err(e) = may {
                self.0.metrics.grpc("Write", "PermissionDenied", 0.0);
                audit(&self.0, &who, "Write (moved)", e.status, Instant::now(), None);
                return Err(Status::from(e));
            }
        }
        self.serve("Write", req, |svc, who, r| async move {
            svc.check_epoch(r.epoch)?;
            let key = Some(r.idempotency_key).filter(|k| !k.is_empty());
            if r.validate {
                let ops: Vec<serde_json::Value> = from_json(&r.ops_json, "write operations")?;
                svc.validate_ops(ops, key, r.epoch).await?;
                return Ok(pb::WriteResponse::default());
            }
            let w = if !r.ops_json.is_empty() {
                let ops: Vec<serde_json::Value> = from_json(&r.ops_json, "write operations")?;
                match r.moved {
                    true => svc.write_ops_as(ops, key, Moved::Copy).await?,
                    false => svc.write_request(ops, key).await?,
                }
            } else if r.moved {
                return Err(ApiError::bad("rows of moving slots come as write operations (ops_json), not SQL"));
            } else {
                let slots = match r.slots_json.is_empty() {
                    true => None,
                    false => {
                        let ranges: Vec<(usize, usize)> = from_json(&r.slots_json, "slots")?;
                        Some(ranges.into_iter().flat_map(|(a, b)| a..b.min(crate::slots::SLOTS)).collect())
                    }
                };
                match svc.sql_in(&who, &r.sql, &params_from_pb(r.params), After::Any, 0, slots, key).await? {
                    crate::service::SqlAnswer::Written { w, .. } => w,
                    crate::service::SqlAnswer::Rows(_) => return Err(ApiError::bad("Write takes INSERT, UPDATE or DELETE; use Query")),
                }
            };
            Ok(write_response(svc.termed(w)))
        })
        .await
    }

    type WriteStreamStream = std::pin::Pin<Box<dyn futures::Stream<Item = Result<pb::WriteResponse, Status>> + Send>>;

    /// Applies each batch once the one before has committed, and answers
    /// it; the first that fails ends the stream with its error, so that no
    /// batch after it is applied ([`Service::stream_write`]).
    async fn write_stream(&self, req: Request<tonic::Streaming<pb::WriteRequest>>) -> Result<Response<Self::WriteStreamStream>, Status> {
        let id = obs::request_id(req.metadata().get(obs::HEADER).and_then(|v| v.to_str().ok()));
        let (who, may) = may_call(&req, "WriteStream");
        if let Err(e) = may {
            self.0.metrics.grpc("WriteStream", "PermissionDenied", 0.0);
            obs::scope(Some(id.clone()), async { audit(&self.0, &who, "WriteStream", e.status, Instant::now(), None) }).await;
            return Err(Status::from(e));
        }
        let mut input = req.into_inner();
        let svc = self.0.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let span = tracing::info_span!("request", id = %id, method = "WriteStream");
        let run = async move {
            let t0 = Instant::now();
            let mut s = Stream::default();
            let mut code = "Ok";
            while let Some(m) = input.next().await {
                let r = async {
                    let m = m.map_err(|e| ApiError::bad(format!("reading the stream: {}", e.message())))?;
                    svc.check_epoch(m.epoch)?;
                    if m.ops_json.is_empty() || !m.sql.is_empty() {
                        return Err(ApiError::bad("a WriteStream message carries write operations (ops_json), not SQL"));
                    }
                    if !m.idempotency_key.is_empty() {
                        return Err(ApiError::bad("WriteStream takes no idempotency keys: give the events sources, whose offsets make a replay skip what was written"));
                    }
                    let ops: Vec<serde_json::Value> = from_json(&m.ops_json, "write operations")?;
                    s.admit(&ops)?;
                    svc.stream_write(&mut s, ops).await
                }
                .await;
                match r {
                    Ok(w) => {
                        let r = pb::WriteResponse { offsets: s.offsets().into_iter().collect(), ..write_response(svc.termed(w)) };
                        if tx.send(Ok(r)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        if e.status >= 500 {
                            obs::server_error(e.status, &e.message);
                        }
                        let st = Status::from(e);
                        code = code_name(st.code());
                        let _ = tx.send(Err(st)).await;
                        break;
                    }
                }
            }
            tracing::debug!(batches = s.batches, written = s.written, skipped = s.skipped, "stream ended");
            svc.metrics.grpc("WriteStream", code, t0.elapsed().as_secs_f64());
        };
        tokio::spawn(run.instrument(span));
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))))
    }

    async fn sources(&self, req: Request<pb::SourcesRequest>) -> Result<Response<pb::SourcesResponse>, Status> {
        self.serve("Sources", req, |svc, _who, r| async move {
            let sources = match r.moving {
                true => svc.moving_offsets().await?,
                false => svc.sources().await?,
            };
            Ok(pb::SourcesResponse { sources: sources.into_iter().map(|s| pb::Source { name: s.name, offset: s.offset, updated_micros: s.updated.unwrap_or(0) }).collect() })
        })
        .await
    }

    async fn schema(&self, req: Request<pb::Empty>) -> Result<Response<pb::SchemaResponse>, Status> {
        self.serve("Schema", req, |svc, _who, _| async move {
            let tables = svc.catalog().iter().map(|t| serde_json::to_string(&**t).unwrap_or_default()).collect();
            let profiles = svc.rank_profiles().iter().map(|(t, p)| crate::coord::profile_json(t, p)).collect();
            Ok(pb::SchemaResponse { schema_json: serde_json::to_string(&*svc.first_table()).unwrap_or_default(), tables_json: tables, profiles_json: profiles })
        })
        .await
    }

    async fn hello(&self, req: Request<pb::HelloRequest>) -> Result<Response<pb::HelloResponse>, Status> {
        self.serve("Hello", req, |svc, _who, _| async move {
            let n = svc.local_db().and_then(|db| db.storage().numbering()).unwrap_or_default();
            Ok(pb::HelloResponse { slot_epoch: svc.slot_epoch(), role: svc.role() as i32, term: n.term, seq_no: n.seq, last_term: n.last().term, replaced_by_term: svc.replaced_by(), ..hello() })
        })
        .await
    }

    async fn replicate(&self, req: Request<pb::ReplicateRequest>) -> Result<Response<pb::ReplicateResponse>, Status> {
        self.serve("Replicate", req, |svc, _who, r| async move { svc.replicate(r).await }).await
    }

    async fn restore(&self, req: Request<tonic::Streaming<pb::RestoreChunk>>) -> Result<Response<pb::ReplicateResponse>, Status> {
        self.serve("Restore", req, |svc, _who, chunks| async move { svc.restore(chunks).await }).await
    }

    async fn verify(&self, req: Request<pb::VerifyRequest>) -> Result<Response<pb::VerifyResponse>, Status> {
        self.serve("Verify", req, |svc, _who, r| async move { svc.verify_for(r).await }).await
    }

    async fn vote(&self, req: Request<pb::VoteRequest>) -> Result<Response<pb::VoteResponse>, Status> {
        self.serve("Vote", req, |svc, _who, r| async move { svc.vote(r).await }).await
    }

    async fn slots(&self, req: Request<pb::SlotsRequest>) -> Result<Response<pb::SlotsResponse>, Status> {
        self.serve("Slots", req, |svc, _who, r| async move {
            if !r.map_json.is_empty() {
                let j: serde_json::Value = from_json(&r.map_json, "slot map")?;
                let map = crate::slots::SlotMap::from_json(&j).map_err(ApiError::bad)?;
                svc.set_slots(map, r.shard as usize).await?;
            }
            Ok(match svc.slots() {
                Some((m, shard)) => pb::SlotsResponse { epoch: m.epoch, map_json: serde_json::to_vec(&m.to_json()).unwrap_or_default(), shard: shard as u32 },
                None => pb::SlotsResponse::default(),
            })
        })
        .await
    }

    async fn configure(&self, req: Request<pb::ConfigureRequest>) -> Result<Response<pb::ConfigureResponse>, Status> {
        self.serve("Configure", req, |svc, who, r| async move {
            let acks = match r.write_acks.as_str() {
                "" => None,
                a => Some(crate::replication::Acks::parse(a).ok_or_else(|| ApiError::bad(format!("write_acks is all, majority or primary, not {a:?}")))?),
            };
            if r.promote && r.role() == pb::Role::Primary {
                let acks = acks.unwrap_or_default();
                svc.promote(&who, r.replicas, acks, r.force).await?;
                return Ok(svc.configured());
            }
            svc.configure_as(r.role(), r.replicas, acks, r.force, r.accept_data_loss).await
        })
        .await
    }

    type ExportSlotsStream = std::pin::Pin<Box<dyn futures::Stream<Item = Result<pb::ExportSlotsChunk, Status>> + Send>>;

    async fn export_slots(&self, req: Request<pb::ExportSlotsRequest>) -> Result<Response<Self::ExportSlotsStream>, Status> {
        let rx = self
            .serve("ExportSlots", req, |svc, _who, r| async move {
                svc.check_epoch(r.epoch)?;
                svc.export_slots(r.changes, if r.batch_rows == 0 { 1000 } else { r.batch_rows as usize }).await
            })
            .await?
            .into_inner();
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx).map(|c| c.map_err(Status::from)))))
    }

    type FeedStream = std::pin::Pin<Box<dyn futures::Stream<Item = Result<pb::FeedChunk, Status>> + Send>>;

    async fn feed(&self, req: Request<pb::FeedRequest>) -> Result<Response<Self::FeedStream>, Status> {
        // A feed goes on until its follower leaves: it ends when the
        // listener stops instead, so that the server stops within its
        // grace time (as the HTTP feed does).
        let stopping = Box::pin(crate::server::Stopping::of(req.extensions().get::<crate::server::Stopping>().cloned()));
        let rx = self
            .serve("Feed", req, |svc, _who, r| async move {
                let copy = r.copy.then_some(crate::service::feed::CopyOpts { since: r.since, whole: r.whole, digests: r.digests });
                svc.feed(r.from, (r.after_term > 0).then_some(r.after_term), copy).await
            })
            .await?
            .into_inner();
        let chunks = tokio_stream::wrappers::ReceiverStream::new(rx).flat_map(|lines| futures::stream::iter(lines.chunks(FEED_CHUNK).map(|c| Ok(pb::FeedChunk { lines: c.to_vec() })).collect::<Vec<_>>()));
        Ok(Response::new(Box::pin(chunks.take_until(stopping))))
    }

    async fn feed_keys(&self, req: Request<pb::FeedKeysRequest>) -> Result<Response<pb::FeedKeysResponse>, Status> {
        self.serve("FeedKeys", req, |svc, _who, r| async move {
            let slots = r.keys.then(|| r.slots.iter().map(|&s| s as usize).collect());
            let j = svc.feed_keys(r.tables, slots).await?;
            Ok(pb::FeedKeysResponse { json: serde_json::to_vec(&j).map_err(ApiError::internal)? })
        })
        .await
    }

    async fn freeze_slots(&self, req: Request<pb::FreezeRequest>) -> Result<Response<pb::FreezeResponse>, Status> {
        self.serve("FreezeSlots", req, |svc, _who, r| async move {
            svc.check_epoch(r.epoch)?;
            let ttl = match r.ttl_ms {
                0 => crate::service::FREEZE_TTL,
                ms => std::time::Duration::from_millis(ms.min(600_000) as u64),
            };
            Ok(pb::FreezeResponse { seq_no: svc.freeze_slots(ttl, r.thaw).await? })
        })
        .await
    }

    async fn cleanup_slots(&self, req: Request<pb::CleanupRequest>) -> Result<Response<pb::CleanupResponse>, Status> {
        self.serve("CleanupSlots", req, |svc, _who, r| async move {
            svc.check_epoch(r.epoch)?;
            Ok(pb::CleanupResponse { deleted: svc.cleanup_slots().await? })
        })
        .await
    }

    async fn keys(&self, req: Request<pb::KeysRequest>) -> Result<Response<pb::KeysResponse>, Status> {
        self.serve("Keys", req, |svc, who, r| async move {
            if who.is_open() {
                return Err(ApiError {
                    code: None, status: 409,
                    message: "this server has no token: keys would close it to the servers that gave them, which send it none. Give the cluster's servers a --cluster-token to share keys".into(),
                });
            }
            if !r.keys_json.is_empty() {
                let given = KeySet::from_json(&r.keys_json).map_err(|e| ApiError::bad(format!("keys_json: {e}")))?;
                let before = svc.keys.set();
                if svc.keys.merge(&given)? {
                    let after = svc.keys.set();
                    keys::note(format!("merged keys from the cluster: {} keys, {} revoked (were {} and {})", after.keys.len(), after.revoked.len(), before.keys.len(), before.revoked.len()));
                }
            }
            Ok(pb::KeysResponse { keys_json: serde_json::to_vec(&svc.keys.set()).map_err(ApiError::internal)? })
        })
        .await
    }

    async fn get(&self, req: Request<pb::GetRequest>) -> Result<Response<pb::GetResponse>, Status> {
        self.serve("Get", req, |svc, _who, r| async move {
            svc.check_epoch(r.epoch)?;
            let after = After::termed(r.min_seq_no, r.min_seq_nos, r.min_seq_term, r.min_seq_terms)?;
            let table = Some(r.table).filter(|t| !t.is_empty());
            let keys = r.keys.into_iter().map(from_pb).collect();
            // A coordinator's read names the protocol it speaks; it counts
            // the keys itself ([`crate::reads`]).
            let got = svc.get_rows(table.as_deref(), keys, None, &after, r.timeout_ms as u64, r.protocol == 0).await?;
            Ok(get_response(got))
        })
        .await
    }

    async fn release(&self, req: Request<pb::ReleaseRequest>) -> Result<Response<pb::Empty>, Status> {
        self.serve("Release", req, |svc, who, r| async move {
            svc.release(&r.snapshots);
            let owner = crate::service::scroll_owner(&who);
            for c in &r.cursors {
                svc.close_scroll(c, &owner);
            }
            Ok(pb::Empty {})
        })
        .await
    }

    async fn profile(&self, req: Request<pb::ProfileRequest>) -> Result<Response<pb::ProfileResponse>, Status> {
        self.serve("Profile", req, |svc, _, r| async move {
            let body: serde_json::Value = if r.body_json.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&r.body_json).map_err(|e| ApiError::bad(format!("bad profile body: {e}")))? };
            let asked = crate::profiling::Asked { kind: crate::profiling::Kind::of(&r.kind)?, body, folded: r.folded, in_use: r.in_use };
            // Audited as an admin's request over HTTP is.
            keys::note(match asked.kind {
                crate::profiling::Kind::LogFilter => format!("log filter {} from a coordinator, by {}", asked.body["filter"].as_str().unwrap_or("?"), asked.body["by"].as_str().unwrap_or("?")),
                crate::profiling::Kind::Verify => format!("verify the replicas from a coordinator, by {}", asked.body["by"].as_str().unwrap_or("?")),
                crate::profiling::Kind::Checkpoint => format!("checkpoint {} from a coordinator, by {}", asked.body["name"].as_str().unwrap_or("?"), asked.body["by"].as_str().unwrap_or("?")),
                crate::profiling::Kind::Scrub => format!("scrub from a coordinator, by {}", asked.body["by"].as_str().unwrap_or("?")),
                crate::profiling::Kind::Alerts => format!("alerts {} from a coordinator, by {}", asked.body["op"].as_str().unwrap_or("?"), asked.body["by"].as_str().unwrap_or("?")),
                crate::profiling::Kind::Settings => match (asked.body.get("slow_ms"), asked.body.get("cpu_all")) {
                    (None, None) => "profiling settings, read by a coordinator".to_string(),
                    (slow, cpu) => format!("profiling settings from a coordinator, by {}: {}", asked.body["by"].as_str().unwrap_or("?"), [slow.map(|v| format!("slow_ms {v}")), cpu.map(|v| format!("cpu_all {v}"))].into_iter().flatten().collect::<Vec<_>>().join(", ")),
                },
                k => match asked.body["by"].as_str() {
                    Some(by) => format!("a {} profile from a coordinator, by {by}", k.name()),
                    None => format!("a {} profile", k.name()),
                },
            });
            Ok(pb::ProfileResponse { body: crate::profiling::take(&svc, asked).await?.bytes() })
        })
        .await
    }

    async fn health(&self, req: Request<pb::Empty>) -> Result<Response<pb::HealthResponse>, Status> {
        // fleet: a coordinator that names itself ([`crate::peers`]).
        crate::peers::heard_from(&self.0, req.metadata(), req.extensions().get::<Principal>());
        self.serve("Health", req, |svc, _who, _| async move {
            let d = svc.detail().await;
            let h = &d["health"];
            let n = |k: &str| h[k].as_u64().unwrap_or(0);
            Ok(pb::HealthResponse {
                status: h["status"].as_str().unwrap_or("unknown").to_string(),
                docs: n("docs"),
                segments: n("segments"),
                txn: n("txn"),
                detail: d.to_string(),
            })
        })
        .await
    }
}
