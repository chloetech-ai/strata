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
//! Writes go to the shard of their key ([`shard_of`]); UPDATE and DELETE
//! statements go to every shard. A write that spans shards is one
//! transaction per shard, not one across them.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use futures::future::join_all;
use serde_json::{json, Value as Json};
use strata::query::stats::TermStats;
use strata::query::{bind_write, Params, Select, Write};
use strata::schema::Schema;
use tonic::transport::Channel;

use crate::grpc::{params_to_pb, result_from_pb};
use crate::pb::{self, strata_client::StrataClient};
use crate::service::{op_key, shard_of, QueryReq};
use crate::{json as js, ApiError};

pub struct Coordinator {
    pub shards: Vec<String>,
    clients: Vec<StrataClient<Channel>>,
    pub schema: Schema,
}

/// Runs `f` within `deadline`.
async fn within<T>(deadline: Instant, f: impl std::future::Future<Output = Result<T, ApiError>>) -> Result<T, ApiError> {
    let left = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(left, f).await {
        Ok(r) => r,
        Err(_) => Err(ApiError { status: 504, message: "the query ran past its deadline".into() }),
    }
}

fn ms_left(deadline: Instant) -> u32 {
    deadline.saturating_duration_since(Instant::now()).as_millis().clamp(1, u32::MAX as u128) as u32
}

impl Coordinator {
    /// Connects to `shards` (gRPC URLs such as `http://10.0.0.5:7001`) and
    /// checks that they serve the same table.
    pub async fn connect(shards: Vec<String>) -> Result<Coordinator, ApiError> {
        if shards.is_empty() {
            return Err(ApiError::bad("a coordinator needs at least one shard"));
        }
        let mut clients = Vec::new();
        for url in &shards {
            let ch = Channel::from_shared(url.clone())
                .map_err(|e| ApiError::bad(format!("bad shard URL {url}: {e}")))?
                .connect_timeout(Duration::from_secs(5))
                .tcp_nodelay(true)
                .connect_lazy();
            clients.push(StrataClient::new(ch).max_decoding_message_size(256 << 20));
        }
        let mut schemas = Vec::new();
        for (url, c) in shards.iter().zip(&clients) {
            let r = c.clone().schema(pb::Empty {}).await.map_err(|e| ApiError { status: 502, message: format!("shard {url}: {}", e.message()) })?;
            schemas.push(r.into_inner().schema_json);
        }
        if schemas.iter().any(|s| *s != schemas[0]) {
            return Err(ApiError::bad("the shards serve different schemas"));
        }
        let schema: Schema = serde_json::from_str(&schemas[0]).map_err(ApiError::internal)?;
        if schema.primary_key.is_none() {
            return Err(ApiError::bad("a sharded table needs a primary key to route writes"));
        }
        Ok(Coordinator { shards, clients, schema })
    }

    fn named(&self, i: usize) -> impl Fn(tonic::Status) -> ApiError + '_ {
        move |s| {
            let mut e = ApiError::from(s);
            e.message = format!("shard {}: {}", self.shards[i], e.message);
            e
        }
    }

    /// Summed statistics and each shard's snapshot lease.
    pub async fn term_stats(&self, sql: &str, params: &Params, deadline: Instant) -> Result<(TermStats, Vec<u64>), ApiError> {
        let req = pb::QueryRequest { sql: sql.into(), params: params_to_pb(params), ..Default::default() };
        let calls = self.clients.iter().enumerate().map(|(i, c)| {
            let (mut c, req) = (c.clone(), req.clone());
            async move { c.term_stats(req).await.map_err(self.named(i)) }
        });
        let answers = within(deadline, async { join_all(calls).await.into_iter().collect::<Result<Vec<_>, _>>() }).await?;
        let mut sum = TermStats::default();
        let mut leases = Vec::new();
        for a in answers {
            let a = a.into_inner();
            let s: TermStats = serde_json::from_slice(&a.stats_json).map_err(ApiError::internal)?;
            sum.add(&s);
            leases.push(a.snapshot);
        }
        Ok((sum, leases))
    }

    pub async fn query(&self, req: &QueryReq, select: &Select, deadline: Instant) -> Result<strata::query::exec::ResultSet, ApiError> {
        let ranked_text = matches!(crate::metrics::kind(select), "bm25" | "hybrid" | "boosted");
        let (stats, leases) = match &req.stats {
            Some(s) => (Some(s.clone()), vec![0; self.clients.len()]),
            None if ranked_text && self.clients.len() > 1 => {
                let (s, l) = self.term_stats(&req.sql, &req.params, deadline).await?;
                (Some(s), l)
            }
            None => (None, vec![0; self.clients.len()]),
        };
        let stats_json = stats.map(|s| serde_json::to_vec(&s).unwrap_or_default()).unwrap_or_default();
        let calls = self.clients.iter().enumerate().map(|(i, c)| {
            let mut c = c.clone();
            let r = pb::QueryRequest {
                sql: req.sql.clone(),
                params: params_to_pb(&req.params),
                timeout_ms: ms_left(deadline),
                shard: true,
                stats_json: stats_json.clone(),
                snapshot: leases[i],
            };
            async move { c.query(r).await.map_err(self.named(i)) }
        });
        let answers = within(deadline, async { join_all(calls).await.into_iter().collect::<Result<Vec<_>, _>>() }).await?;
        let shards = answers.into_iter().map(|a| result_from_pb(a.into_inner())).collect::<Result<Vec<_>, _>>()?;
        Ok(strata::cluster::merge(select, shards)?)
    }

    async fn write(&self, i: usize, r: pb::WriteRequest) -> Result<u64, ApiError> {
        Ok(self.clients[i].clone().write(r).await.map_err(self.named(i))?.into_inner().written)
    }

    /// An INSERT goes to the shards of its documents' keys; UPDATE and
    /// DELETE go to every shard.
    pub async fn sql(&self, sql: &str, params: &Params) -> Result<u64, ApiError> {
        match bind_write(sql, &self.schema, params)? {
            Some(Write::Insert { docs, replace }) => {
                let op = if replace { "upsert" } else { "insert" };
                let ops = docs.iter().map(|d| json!({"op": op, "doc": js::document_json(&self.schema, d)})).collect();
                self.write_ops(ops).await
            }
            Some(_) => {
                let r = pb::WriteRequest { sql: sql.into(), params: params_to_pb(params), ops_json: Vec::new() };
                let calls = (0..self.clients.len()).map(|i| self.write(i, r.clone()));
                Ok(join_all(calls).await.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().sum())
            }
            None => Err(ApiError::bad("not a write statement")),
        }
    }

    /// Write operations, grouped by the shard of their key.
    pub async fn write_ops(&self, ops: Vec<Json>) -> Result<u64, ApiError> {
        let mut by_shard: BTreeMap<usize, Vec<Json>> = BTreeMap::new();
        for op in ops {
            let s = shard_of(&op_key(&self.schema, &op)?, self.clients.len());
            by_shard.entry(s).or_default().push(op);
        }
        let calls = by_shard.into_iter().map(|(i, ops)| {
            let r = pb::WriteRequest { ops_json: serde_json::to_vec(&ops).unwrap_or_default(), ..Default::default() };
            self.write(i, r)
        });
        Ok(join_all(calls).await.into_iter().collect::<Result<Vec<_>, _>>()?.into_iter().sum())
    }

    pub async fn health(&self) -> Json {
        let calls = self.clients.iter().map(|c| {
            let mut c = c.clone();
            async move { tokio::time::timeout(Duration::from_secs(2), c.health(pb::Empty {})).await }
        });
        let mut shards = Vec::new();
        let (mut docs, mut segments, mut ok) = (0, 0, true);
        for (url, r) in self.shards.iter().zip(join_all(calls).await) {
            match r {
                Ok(Ok(h)) => {
                    let h = h.into_inner();
                    docs += h.docs;
                    segments += h.segments;
                    ok &= h.status == "ok";
                    shards.push(json!({"url": url, "status": h.status, "docs": h.docs, "segments": h.segments, "txn": h.txn}));
                }
                Ok(Err(e)) => {
                    ok = false;
                    shards.push(json!({"url": url, "status": "down", "error": e.message()}));
                }
                Err(_) => {
                    ok = false;
                    shards.push(json!({"url": url, "status": "down", "error": "no answer within 2 s"}));
                }
            }
        }
        json!({"status": if ok { "ok" } else { "degraded" }, "role": "coordinator", "docs": docs, "segments": segments, "txn": 0, "shards": shards})
    }
}
