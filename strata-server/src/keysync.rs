//! Keys that hold across a cluster ([`crate::keys`]). The servers of a
//! cluster hold the same API keys: a coordinator merges its keys with
//! every server of every shard, and a primary with its replicas, by the
//! internal call `Keys`, every [`EVERY`] and as soon as its keys change
//! (a key made or revoked, or a merge that brought something new). Each
//! server it calls merges the keys it is given into its own and answers
//! what it then holds, which the caller merges in turn.
//!
//! So the keys live on the shards, beside their databases, whatever a
//! coordinator keeps: a coordinator that starts again takes them back from
//! its shards before it serves ([`start`]), a second coordinator holds the
//! first one's, a key made on a shard reaches the coordinators and the
//! other shards, and a replica promoted to primary holds its primary's
//! keys. A revocation reaches every server the coordinator reaches within
//! a round, and wins wherever it is merged: a server that was down when a
//! key was revoked loses the key at its first merge.
//!
//! A coordinator calls its shards with the cluster's token, and a server
//! refuses keys from others when it has no token of its own (an open
//! server would be closed by them); [`crate::keys::Keys::peers`] says how
//! the last merge with each server went (`GET /v1/admin/keys`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;

use crate::coord::{self, Client};
use crate::keys::KeySet;
use crate::pb;
use crate::service::{Backend, Replication, Service};

/// How often a coordinator and a primary merge their keys with the
/// servers they call, besides whenever their keys change.
pub const EVERY: Duration = Duration::from_secs(5);

/// How long a server has to answer.
const TIMEOUT: Duration = Duration::from_secs(3);

/// The servers this one merges its keys with, by URL: a coordinator's,
/// every server of every shard; a primary's, its replicas (their clients
/// made once, in `clients`); any other server's, none.
pub(crate) fn peers(svc: &Service, clients: &mut HashMap<String, Client>) -> Vec<(String, Client)> {
    match &svc.backend {
        Backend::Cluster(c) => c.replica_clients().into_iter().map(|(_, url, c)| (url, c)).collect(),
        Backend::Local(_) => {
            let Replication::Primary(p) = svc.replication() else { return Vec::new() };
            let (token, ca) = svc.promote_options.as_ref().map(|o| (o.token.clone(), o.ca.clone())).unwrap_or_default();
            let mut out = Vec::new();
            for url in p.replicas() {
                if !clients.contains_key(url) {
                    match coord::client(url, token.as_ref(), ca.as_deref()) {
                        Ok(c) => {
                            clients.insert(url.clone(), c);
                        }
                        Err(e) => {
                            tracing::warn!(replica = %url, error = %e.message, "cannot pass keys on to this replica");
                            continue;
                        }
                    }
                }
                out.push((url.clone(), clients[url].clone()));
            }
            out
        }
    }
}

/// One round: gives each of `peers` this server's keys, merges what each
/// answers, and records how each went. Whether an answer changed the keys.
pub(crate) async fn round(svc: &Service, peers: Vec<(String, Client)>) -> bool {
    if peers.is_empty() {
        svc.keys.synced(Vec::new());
        return false;
    }
    let body = match serde_json::to_vec(&svc.keys.set()) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "cannot write the keys to pass them on");
            return false;
        }
    };
    let asks = peers.into_iter().map(|(url, mut c)| {
        let keys_json = body.clone();
        async move {
            let answer = match tokio::time::timeout(TIMEOUT, c.keys(pb::KeysRequest { keys_json })).await {
                Err(_) => Err(format!("no answer in {} s", TIMEOUT.as_secs())),
                Ok(Err(s)) if s.code() == tonic::Code::Unimplemented => Err("an older server, which keeps its own keys".to_string()),
                Ok(Err(s)) => Err(s.message().to_string()),
                Ok(Ok(a)) => KeySet::from_json(&a.into_inner().keys_json).map_err(|e| format!("its keys: {e}")),
            };
            (url, answer)
        }
    });
    let mut changed = false;
    let mut status = Vec::new();
    for (url, answer) in join_all(asks).await {
        let error = match answer.map(|set| svc.keys.merge(&set)) {
            Ok(Ok(c)) => {
                changed |= c;
                None
            }
            Ok(Err(e)) => Some(e.message),
            Err(e) => Some(e),
        };
        if let Some(e) = &error {
            tracing::debug!(server = %url, error = %e, "could not merge keys");
        }
        status.push((url, error));
    }
    svc.keys.synced(status);
    changed
}

/// Merges keys once before a coordinator serves, so that it admits its
/// cluster's keys from its first request, then keeps merging ([`spawn`]).
pub async fn start(svc: &Arc<Service>) {
    if matches!(svc.backend, Backend::Cluster(_)) {
        let peers = peers(svc, &mut HashMap::new());
        round(svc, peers).await;
    }
    spawn(svc);
}

/// Merges keys every [`EVERY`] and whenever they change, until the service
/// is dropped.
pub fn spawn(svc: &Arc<Service>) {
    let me = Arc::downgrade(svc);
    tokio::spawn(async move {
        let mut clients = HashMap::new();
        loop {
            let Some(svc) = me.upgrade() else { return };
            let peers = peers(&svc, &mut clients);
            // A replica no longer shipped to is let go.
            clients.retain(|url, _| peers.iter().any(|(u, _)| u == url));
            round(&svc, peers).await;
            let keys = svc.keys.clone();
            drop(svc);
            let _ = tokio::time::timeout(EVERY, keys.changed.notified()).await;
        }
    });
}
