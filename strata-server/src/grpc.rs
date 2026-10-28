//! The service over gRPC (`proto/strata.proto`).

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use strata::query::exec::ResultSet;
use strata::query::Params;
use strata::value::Value;
use tonic::{Request, Response, Status};
use tracing::Instrument;

use crate::pb::{self, value::Kind};
use crate::service::{QueryReq, Service};
use crate::{obs, ApiError};

pub fn to_pb(v: &Value) -> pb::Value {
    let kind = match v {
        Value::Null => Kind::Null(true),
        Value::Int(x) => Kind::Int(*x),
        Value::Float(x) => Kind::Float(*x),
        Value::Str(s) => Kind::Str(s.clone()),
        Value::Vector(v) => Kind::Vector(pb::Floats { v: v.to_vec() }),
        Value::List(xs) => Kind::List(pb::Values { v: xs.iter().map(to_pb).collect() }),
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

pub fn query_req(r: pb::QueryRequest) -> Result<QueryReq, ApiError> {
    Ok(QueryReq {
        sql: r.sql,
        params: params_from_pb(r.params),
        timeout_ms: r.timeout_ms as u64,
        shard: r.shard,
        stats: if r.stats_json.is_empty() { None } else { Some(from_json(&r.stats_json, "statistics")?) },
        snapshot: r.snapshot,
        partial: None,
    })
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

impl Grpc {
    /// Serves one call: gives it its id (the caller's `x-request-id`, or a
    /// new one, sent back in the answer's metadata), runs it in a span that
    /// names it, counts it and logs it ([`crate::obs`]).
    async fn serve<T, R, Fut>(&self, method: &'static str, req: Request<T>, f: impl FnOnce(Arc<Service>, T) -> Fut) -> Result<Response<R>, Status>
    where
        Fut: Future<Output = Result<R, ApiError>>,
    {
        let id = obs::request_id(req.metadata().get(obs::HEADER).and_then(|v| v.to_str().ok()));
        let span = tracing::info_span!("request", id = %id, method);
        let t0 = Instant::now();
        let r = obs::scope(Some(id.clone()), f(self.0.clone(), req.into_inner())).instrument(span.clone()).await;
        let secs = t0.elapsed().as_secs_f64();
        let _in = span.enter();
        let (code, r) = match r {
            Ok(x) => {
                tracing::debug!(secs, "answered");
                ("Ok", Ok(Response::new(x)))
            }
            Err(e) => {
                if e.status >= 500 {
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
        self.serve("Query", req, |svc, r| async move {
            let a = svc.query(query_req(r)?).await?;
            Ok(result_to_pb(&a.rs, a.took_ms))
        })
        .await
    }

    async fn term_stats(&self, req: Request<pb::QueryRequest>) -> Result<Response<pb::TermStatsResponse>, Status> {
        self.serve("TermStats", req, |svc, r| async move {
            let (stats, snapshot) = svc.term_stats(&r.sql, &params_from_pb(r.params), r.timeout_ms as u64).await?;
            Ok(pb::TermStatsResponse { stats_json: json_bytes(&stats), snapshot })
        })
        .await
    }

    async fn write(&self, req: Request<pb::WriteRequest>) -> Result<Response<pb::WriteResponse>, Status> {
        self.serve("Write", req, |svc, r| async move {
            let written = if !r.ops_json.is_empty() {
                let ops: Vec<serde_json::Value> = from_json(&r.ops_json, "write operations")?;
                let key = Some(r.idempotency_key).filter(|k| !k.is_empty());
                svc.write_ops(ops, key).await?
            } else {
                match svc.sql(&r.sql, &params_from_pb(r.params), 0).await? {
                    crate::service::SqlAnswer::Written { n, .. } => n,
                    crate::service::SqlAnswer::Rows(_) => return Err(ApiError::bad("Write takes INSERT, UPDATE or DELETE; use Query")),
                }
            };
            Ok(pb::WriteResponse { written })
        })
        .await
    }

    async fn schema(&self, req: Request<pb::Empty>) -> Result<Response<pb::SchemaResponse>, Status> {
        self.serve("Schema", req, |svc, _| async move { Ok(pb::SchemaResponse { schema_json: serde_json::to_string(&svc.schema).unwrap_or_default() }) }).await
    }

    async fn health(&self, req: Request<pb::Empty>) -> Result<Response<pb::HealthResponse>, Status> {
        self.serve("Health", req, |svc, _| async move {
            let h = svc.health().await;
            let n = |k: &str| h[k].as_u64().unwrap_or(0);
            Ok(pb::HealthResponse {
                status: h["status"].as_str().unwrap_or("unknown").to_string(),
                docs: n("docs"),
                segments: n("segments"),
                txn: n("txn"),
            })
        })
        .await
    }
}
