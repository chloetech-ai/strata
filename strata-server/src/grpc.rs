//! The service over gRPC (`proto/strata.proto`).

use std::collections::HashMap;
use std::sync::Arc;

use strata::query::exec::ResultSet;
use strata::query::Params;
use strata::value::Value;
use tonic::{Request, Response, Status};

use crate::pb::{self, value::Kind};
use crate::service::{QueryReq, Service};
use crate::ApiError;

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

#[tonic::async_trait]
impl pb::strata_server::Strata for Grpc {
    async fn query(&self, req: Request<pb::QueryRequest>) -> Result<Response<pb::QueryResponse>, Status> {
        let a = self.0.query(query_req(req.into_inner())?).await?;
        Ok(Response::new(result_to_pb(&a.rs, a.took_ms)))
    }

    async fn term_stats(&self, req: Request<pb::QueryRequest>) -> Result<Response<pb::TermStatsResponse>, Status> {
        let r = req.into_inner();
        let (stats, snapshot) = self.0.term_stats(&r.sql, &params_from_pb(r.params)).await?;
        Ok(Response::new(pb::TermStatsResponse { stats_json: json_bytes(&stats), snapshot }))
    }

    async fn write(&self, req: Request<pb::WriteRequest>) -> Result<Response<pb::WriteResponse>, Status> {
        let r = req.into_inner();
        let written = if !r.ops_json.is_empty() {
            let ops: Vec<serde_json::Value> = from_json(&r.ops_json, "write operations")?;
            self.0.write_ops(ops).await?
        } else {
            match self.0.sql(&r.sql, &params_from_pb(r.params), 0).await? {
                crate::service::SqlAnswer::Written { n, .. } => n,
                crate::service::SqlAnswer::Rows(_) => return Err(Status::invalid_argument("Write takes INSERT, UPDATE or DELETE; use Query")),
            }
        };
        Ok(Response::new(pb::WriteResponse { written }))
    }

    async fn schema(&self, _: Request<pb::Empty>) -> Result<Response<pb::SchemaResponse>, Status> {
        Ok(Response::new(pb::SchemaResponse { schema_json: serde_json::to_string(&self.0.schema).unwrap_or_default() }))
    }

    async fn health(&self, _: Request<pb::Empty>) -> Result<Response<pb::HealthResponse>, Status> {
        let h = self.0.health().await;
        let n = |k: &str| h[k].as_u64().unwrap_or(0);
        Ok(Response::new(pb::HealthResponse {
            status: h["status"].as_str().unwrap_or("unknown").to_string(),
            docs: n("docs"),
            segments: n("segments"),
            txn: n("txn"),
        }))
    }
}
