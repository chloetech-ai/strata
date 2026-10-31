//! Automatic failover (`--failover-after-ms`): the copies of a shard elect
//! its primary among themselves.
//!
//! A replica that hears nothing from its primary for the failover delay,
//! and a random part of it more (so that two replicas seldom ask at once),
//! asks the servers its cluster map names for its shard to elect it
//! ([`Follow::campaign`]): first whether they would, then for their votes.
//! Each copy grants one vote a term, kept beside its database, and only to
//! a copy holding every transaction it holds; a replica whose primary
//! called within the delay refuses, and so does a primary that serves. A
//! replica a majority elects begins the term and ships to the others.
//!
//! A write is acknowledged once a majority of the copies hold it (with
//! `--write-acks all` or `majority`), and an election needs a majority of
//! votes from copies that hold no transaction the winner lacks: the two
//! majorities share a copy, so the new primary holds every acknowledged
//! write. A primary that does not hear from a majority of its copies for
//! most of the delay serves no reads ([`crate::replication::PrimaryOptions::lease`]),
//! as they may have elected another; and a primary another replaced becomes
//! a replica of it ([`Service::step_down`]), so that the new primary ships
//! to it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{conflict, Replication, Service};
use crate::replication::{Follow, PrimaryOptions, STICKY};
use crate::{pb, ApiError};

/// How a primary ships when its copies may elect another after `after`:
/// it calls each replica at least four times within it (so that a replica
/// does not take a quiet primary for a dead one), and holds a lease on
/// reads of three quarters of it, which leaves room for clocks that run
/// apart a little.
pub fn tuned(mut o: PrimaryOptions, after: Duration) -> PrimaryOptions {
    if !after.is_zero() {
        o.lease = after * 3 / 4;
        o.heartbeat = o.heartbeat.min(after / 4);
    }
    o
}

/// A random part of `d`.
fn jitter(d: Duration) -> Duration {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    d.mul_f64((h.finish() % 1_000_000) as f64 / 1_000_000.0)
}

impl Service {
    /// Answers a copy of this server's shard asking to be made its primary
    /// (`Vote`). A primary that grants a handover steps down at once.
    pub async fn vote(&self, r: pb::VoteRequest) -> Result<pb::VoteResponse, ApiError> {
        match self.replication() {
            Replication::Replica(f) => {
                let sticky = if self.failover_after.is_zero() { STICKY } else { self.failover_after };
                f.vote(&r, sticky).await
            }
            Replication::Primary(p) => {
                let a = p.vote(&r);
                if a.granted && !a.candidate && !r.pre {
                    self.step_down().await;
                }
                Ok(a)
            }
            Replication::None => Err(conflict("this server is not replicated: it has no shard copies to elect a primary among".into())),
        }
    }

    /// Makes a primary that another replaced a replica, which follows the
    /// new primary when it ships: the new primary sends it a copy when it
    /// holds transactions the new one does not.
    pub(crate) async fn step_down(&self) {
        let _one = self.promoting.lock().await;
        let Replication::Primary(p) = self.replication() else { return };
        let term = p.replaced_by();
        let Some(db) = self.local().cloned() else { return };
        if term == 0 {
            return;
        }
        match Follow::open(db, self.replication_file.as_deref()) {
            Ok(f) => {
                *self.replication.write().unwrap_or_else(|e| e.into_inner()) = Replication::Replica(Arc::new(f));
                tracing::warn!(replaced_by = term, "a primary of a newer term replaced this one, which is a replica now: it follows the new primary");
            }
            Err(e) => tracing::error!(error = %e, "could not make the replaced primary a replica"),
        }
    }

    /// Runs automatic failover while the server serves, when
    /// [`Self::failover_after`] is set: a replica whose primary is silent
    /// asks to be elected, and a replaced primary steps down.
    pub fn start_failover(self: &Arc<Self>) {
        let after = self.failover_after;
        if after.is_zero() {
            return;
        }
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            let every = (after / 8).max(Duration::from_millis(10));
            let mut next = Instant::now() + jitter(after);
            loop {
                tokio::time::sleep(every).await;
                let Some(svc) = me.upgrade() else { return };
                svc.fail_over(after, &mut next).await;
            }
        });
    }

    /// One look of [`Self::start_failover`]: an election not before
    /// `next`, which a lost one moves on.
    async fn fail_over(&self, after: Duration, next: &mut Instant) {
        let f = match self.replication() {
            Replication::Primary(p) if p.replaced_by() > 0 => return self.step_down().await,
            Replication::Replica(f) => f,
            _ => return,
        };
        let Some((map, shard)) = self.slots() else { return };
        let copies = map.replicas().get(shard).cloned().unwrap_or_default();
        if copies.len() < 2 || f.silent_for() < after || Instant::now() < *next {
            return;
        }
        let _one = self.promoting.lock().await;
        // Promoted meanwhile, or its primary called.
        if !matches!(self.replication(), Replication::Replica(_)) || f.silent_for() < after {
            return;
        }
        *next = Instant::now() + after / 2 + jitter(after);
        let mut opts = self.primary_options();
        tracing::warn!(silent_ms = f.silent_for().as_millis() as u64, "its primary is silent: asking its shard's copies to elect this replica");
        // A copy that does not answer within the delay is taken for down.
        let ask = PrimaryOptions { call_timeout: opts.call_timeout.min(after.max(Duration::from_millis(200))), ..opts.clone() };
        let won = match f.campaign(&copies, &ask, false).await {
            Ok(e) => e,
            Err(e) => {
                tracing::info!(error = %e.message, "not elected");
                return;
            }
        };
        opts.replicas = won.others.clone();
        match f.take_over(Some(won.term), opts).await {
            Ok((p, _)) => {
                *self.replication.write().unwrap_or_else(|e| e.into_inner()) = Replication::Primary(p);
                tracing::warn!(term = won.term, replicas = %won.others.join(", "), "elected: this server is its shard's primary");
            }
            Err(e) => tracing::warn!(error = %e.message, "elected, but could not begin the term"),
        }
    }
}
