//! Replication: a primary ships its numbered transactions to its replicas.
//!
//! A server started with `--replicas` is the primary of its database. It
//! numbers its writes ([`strata::storage::Database::start_seq_no`]) and
//! keeps the latest of them as it logged them (its ship log,
//! [`strata::storage::DbOptions::ship_log_bytes`]); a task per replica
//! ships that log to the replica's `Replicate` call in order, in batches,
//! under the primary's term, and hears how far the replica got. A server
//! started with `--replica` applies what it is shipped
//! ([`strata::storage::Database::apply_shipped`], which checks that each
//! batch follows its last transaction) and takes no writes from clients,
//! so that every copy holds the same rows under the same sequence numbers.
//!
//! A write is answered once the copies `--write-acks` asks for hold it
//! ([`Acks`]), waiting up to the write's deadline: the primary alone, a
//! majority of the copies, or every replica in sync. A replica is in sync
//! while it holds what the primary committed, as far as it was shipped; it
//! drops out when a call to it fails, when its history differs from the
//! primary's, when the primary's log no longer reaches it, or when it does
//! not acknowledge a write within the write's deadline, and it is back in
//! once it holds the primary's last transaction again. The global
//! checkpoint is the highest number every in-sync replica holds, shipped
//! with each call.
//!
//! Each server keeps an id beside its database (`<db>.replication`), and a
//! replica keeps there the id and term of the primary it follows: it
//! refuses a second primary of the same term, whose history would differ,
//! and follows the primary of a newer term. A primary of an older term
//! than a replica's is answered with the replica's term, and stops taking
//! writes: another primary replaced it.
//!
//! Not here yet: a replica the primary's log no longer reaches needs a
//! copy of the primary (start it from a checkpoint), and replacing a
//! primary by one of its replicas.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::{Applied, LogRead, SeqNo};
use tokio::sync::watch;

use crate::auth::Secret;
use crate::coord::{self, Client};
use crate::metrics::Family;
use crate::{pb, ApiError};

/// Which copies hold a write before it is answered (`--write-acks`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Acks {
    /// The primary: replicas get it after the answer.
    Primary,
    /// More than half the copies, the primary among them. A write is
    /// refused (503) before it is applied while fewer copies are in sync,
    /// and answered 504 when too few acknowledge it within its deadline,
    /// though the primary holds it.
    Majority,
    /// The primary and every replica in sync. A replica that does not
    /// acknowledge the write within its deadline drops out of sync, and
    /// the write is answered.
    #[default]
    All,
}

impl Acks {
    pub fn parse(s: &str) -> Option<Acks> {
        match s {
            "primary" => Some(Acks::Primary),
            "majority" => Some(Acks::Majority),
            "all" => Some(Acks::All),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Acks::Primary => "primary",
            Acks::Majority => "majority",
            Acks::All => "all",
        }
    }
}

/// How a primary ships to its replicas.
#[derive(Clone, Debug)]
pub struct PrimaryOptions {
    /// The replicas' gRPC URLs.
    pub replicas: Vec<String>,
    pub acks: Acks,
    /// The token the replicas require, and a CA for `https://` ones.
    pub token: Option<Secret>,
    pub ca: Option<Vec<u8>>,
    /// The most bytes of transactions a call ships (one at least).
    pub batch_bytes: usize,
    /// How often a replica with nothing to apply is called, which tells
    /// it the global checkpoint and the primary whether it is there.
    pub heartbeat: Duration,
    /// The longest wait before calling a replica that failed again.
    pub retry_max: Duration,
    /// How long a call may take.
    pub call_timeout: Duration,
}

impl Default for PrimaryOptions {
    fn default() -> PrimaryOptions {
        PrimaryOptions {
            replicas: Vec::new(),
            acks: Acks::All,
            token: None,
            ca: None,
            batch_bytes: 4 << 20,
            heartbeat: Duration::from_secs(1),
            retry_max: Duration::from_secs(2),
            call_timeout: Duration::from_secs(10),
        }
    }
}

/// Where a server keeps its id, and a replica the primary it follows.
pub fn file_of(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(".replication");
    p.into()
}

/// What `<db>.replication` holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Kept {
    id: String,
    /// The primary followed, and its term.
    follows: Option<(String, u64)>,
}

impl Kept {
    /// Reads `path`, or makes a new id and keeps it there.
    fn open(path: Option<&Path>) -> Result<Kept, String> {
        let Some(path) = path else { return Ok(Kept { id: new_id(), follows: None }) };
        if path.exists() {
            let j: Json = std::fs::read(path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string())).map_err(|e| format!("{}: {e}", path.display()))?;
            let id = j["id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| format!("{}: no id", path.display()))?.to_string();
            let follows = match &j["follows"] {
                Json::Null => None,
                f => Some((f["primary"].as_str().unwrap_or_default().to_string(), f["term"].as_u64().ok_or_else(|| format!("{}: no term followed", path.display()))?)),
            };
            return Ok(Kept { id, follows });
        }
        let k = Kept { id: new_id(), follows: None };
        k.save(Some(path)).map_err(|e| format!("keep the replication id in {}: {e}", path.display()))?;
        Ok(k)
    }

    fn save(&self, path: Option<&Path>) -> std::io::Result<()> {
        let Some(path) = path else { return Ok(()) };
        let mut j = json!({"id": self.id});
        if let Some((primary, term)) = &self.follows {
            j["follows"] = json!({"primary": primary, "term": term});
        }
        crate::service::save_json(path, &j)
    }
}

/// 32 hex digits, drawn at random.
fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let half = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        h.finish()
    };
    format!("{:016x}{:016x}", half(), half())
}

fn conflict(message: String) -> ApiError {
    ApiError { status: 409, message }
}

/// A replica, as its primary sees it.
struct Follower {
    url: String,
    client: Client,
    /// The last number it holds, as it last said.
    acked: AtomicU64,
    in_sync: AtomicBool,
    /// Why it is out of sync, or why its last call failed.
    error: Mutex<Option<String>>,
    /// Transactions it applied from this primary, and calls that failed.
    shipped: AtomicU64,
    failures: AtomicU64,
}

impl Follower {
    fn acked(&self) -> u64 {
        self.acked.load(Ordering::Relaxed)
    }

    fn in_sync(&self) -> bool {
        self.in_sync.load(Ordering::Relaxed)
    }

    fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Takes it out of sync for `why`, logging the change.
    fn drop_out(&self, why: String) {
        let was = self.in_sync.swap(false, Ordering::Relaxed);
        let mut e = self.error.lock().unwrap_or_else(|e| e.into_inner());
        if was || e.as_deref() != Some(&why) {
            tracing::warn!(replica = %self.url, error = %why, "a replica is out of sync");
        }
        *e = Some(why);
    }

    /// Puts it in sync: it holds the primary's last transaction.
    fn caught_up(&self) {
        if !self.in_sync.swap(true, Ordering::Relaxed) {
            tracing::info!(replica = %self.url, seq_no = self.acked(), "a replica is in sync");
        }
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// A primary: ships its database's numbered transactions to its replicas,
/// and tells a write when enough copies hold it.
pub struct Primary {
    db: Arc<Db>,
    id: String,
    opts: PrimaryOptions,
    followers: Vec<Follower>,
    /// The last number committed, as the database's commit hook says.
    committed: Arc<watch::Sender<u64>>,
    /// Changed whenever a replica acknowledges or drops out.
    progress: watch::Sender<u64>,
    /// A newer term a replica holds (0: none): another primary replaced
    /// this one.
    fenced: AtomicU64,
}

impl Primary {
    /// Starts shipping `db`'s transactions to `opts.replicas`, keeping
    /// the primary's id in `file` (none: a new one each start). Every
    /// replica counts as in sync until it fails, so that the first writes
    /// wait for replicas that are there.
    pub fn start(db: Arc<Db>, file: Option<&Path>, opts: PrimaryOptions) -> Result<Arc<Primary>, String> {
        let numbering = db.storage().numbering().ok_or("a primary numbers its writes: start it with --seq-no")?;
        if !db.storage().keeps_ship_log() {
            return Err("a primary keeps a log of its transactions to ship (DbOptions::ship_log_bytes)".into());
        }
        let kept = Kept::open(file)?;
        let followers = opts
            .replicas
            .iter()
            .map(|url| {
                Ok(Follower {
                    url: url.clone(),
                    client: coord::client(url, opts.token.as_ref(), opts.ca.as_deref()).map_err(|e| e.message)?,
                    acked: AtomicU64::new(0),
                    in_sync: AtomicBool::new(true),
                    error: Mutex::new(None),
                    shipped: AtomicU64::new(0),
                    failures: AtomicU64::new(0),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let committed = Arc::new(watch::channel(numbering.seq).0);
        let hook = committed.clone();
        db.storage().on_commit(move |n| {
            hook.send_replace(n.seq);
        });
        let p = Arc::new(Primary { db, id: kept.id, opts, followers, committed, progress: watch::channel(0).0, fenced: AtomicU64::new(0) });
        for k in 0..p.followers.len() {
            tokio::spawn(ship(Arc::downgrade(&p), k));
        }
        Ok(p)
    }

    pub fn acks(&self) -> Acks {
        self.opts.acks
    }

    fn term(&self) -> u64 {
        self.db.storage().numbering().map_or(0, |n| n.term)
    }

    /// Copies in all, the primary among them, and how many make a majority.
    fn majority(&self) -> usize {
        (self.followers.len() + 1) / 2 + 1
    }

    /// The highest number every in-sync replica holds (the primary's last
    /// while none is in sync).
    pub fn global_checkpoint(&self) -> u64 {
        let last = *self.committed.borrow();
        self.followers.iter().filter(|f| f.in_sync()).map(|f| f.acked()).min().unwrap_or(last).min(last)
    }

    fn changed(&self) {
        self.progress.send_modify(|n| *n += 1);
    }

    /// Whether a write may be applied: not on a primary another replaced
    /// (409), and with `majority`, while a majority of copies is in sync
    /// (503), so that a write the copies cannot acknowledge is not applied.
    pub fn admit(&self) -> Result<(), ApiError> {
        let fenced = self.fenced.load(Ordering::Relaxed);
        if fenced > 0 {
            return Err(conflict(format!(
                "a replica holds term {fenced}, newer than this primary's ({}): another primary replaced this one, which takes no more writes",
                self.term()
            )));
        }
        if self.opts.acks == Acks::Majority {
            let up = 1 + self.followers.iter().filter(|f| f.in_sync()).count();
            if up < self.majority() {
                return Err(ApiError {
                    status: 503,
                    message: format!("{up} of {} copies are in sync, and --write-acks majority needs {}: the write was not applied", self.followers.len() + 1, self.majority()),
                });
            }
        }
        Ok(())
    }

    /// Waits until the copies `--write-acks` asks for hold transaction
    /// `seq`, or `deadline`. Then, with `all`, the replicas in sync that
    /// do not hold it drop out of sync; with `majority`, the write is
    /// answered 504.
    pub async fn acknowledged(&self, seq: u64, deadline: Instant) -> Result<(), ApiError> {
        let mut progress = self.progress.subscribe();
        loop {
            let holding = 1 + self.followers.iter().filter(|f| f.acked() >= seq).count();
            match self.opts.acks {
                Acks::Primary => return Ok(()),
                Acks::All if self.followers.iter().all(|f| !f.in_sync() || f.acked() >= seq) => return Ok(()),
                Acks::Majority if holding >= self.majority() => return Ok(()),
                _ => {}
            }
            if tokio::time::timeout_at(deadline.into(), progress.changed()).await.is_ok() {
                continue;
            }
            if self.opts.acks == Acks::Majority {
                return Err(ApiError {
                    status: 504,
                    message: format!(
                        "the write is durable on the primary as sequence number {seq}, but {holding} of {} copies held it by its deadline, and --write-acks majority needs {}",
                        self.followers.len() + 1,
                        self.majority()
                    ),
                });
            }
            for f in self.followers.iter().filter(|f| f.in_sync() && f.acked() < seq) {
                f.drop_out(format!("it did not acknowledge transaction {seq} within the write's deadline"));
            }
            self.changed();
            return Ok(());
        }
    }

    /// What `/v1/health` shows of replication.
    pub fn health(&self) -> Json {
        let last = *self.committed.borrow();
        let replicas: Vec<Json> = self
            .followers
            .iter()
            .map(|f| {
                let mut j = json!({"url": f.url, "seq_no": f.acked(), "behind": last.saturating_sub(f.acked()), "in_sync": f.in_sync()});
                if let Some(e) = f.error() {
                    j["error"] = json!(e);
                }
                j
            })
            .collect();
        let mut h = json!({
            "role": "primary", "id": self.id, "term": self.term(), "seq_no": last, "write_acks": self.opts.acks.name(),
            "global_checkpoint": self.global_checkpoint(), "replicas": replicas,
        });
        let fenced = self.fenced.load(Ordering::Relaxed);
        if fenced > 0 {
            h["replaced_by_term"] = json!(fenced);
        }
        h
    }

    pub fn metrics(&self) -> Vec<Family> {
        let last = *self.committed.borrow();
        let by = |f: &dyn Fn(&Follower) -> f64| self.followers.iter().map(|r| (format!("replica=\"{}\"", r.url), f(r))).collect();
        vec![
            Family::one("strata_replication_role", "gauge", "1 on a primary (--replicas), 2 on a replica (--replica).", 1.0),
            Family::one("strata_global_checkpoint", "gauge", "The highest sequence number every in-sync replica holds.", self.global_checkpoint() as f64),
            Family { name: "strata_replica_in_sync", ty: "gauge", help: "1 while a replica holds what the primary committed, as far as it was shipped.", values: by(&|f| f.in_sync() as u8 as f64) },
            Family { name: "strata_replica_behind", ty: "gauge", help: "Transactions the primary committed that a replica does not hold yet.", values: by(&|f| last.saturating_sub(f.acked()) as f64) },
            Family { name: "strata_replica_shipped_total", ty: "counter", help: "Transactions a replica applied from this primary.", values: by(&|f| f.shipped.load(Ordering::Relaxed) as f64) },
            Family { name: "strata_replica_failures_total", ty: "counter", help: "Calls to a replica that failed or that it refused.", values: by(&|f| f.failures.load(Ordering::Relaxed) as f64) },
        ]
    }
}

/// Ships `p`'s transactions to its replica `k` until `p` is dropped:
/// asks the replica its last transaction, sends what follows in batches,
/// and calls it every heartbeat while there is nothing to send.
async fn ship(p: Weak<Primary>, k: usize) {
    let Some(mut committed) = p.upgrade().map(|p| p.committed.subscribe()) else { return };
    // The next number to send; `None` until the replica said its last.
    let mut next: Option<u64> = None;
    let mut retry = Duration::from_millis(50);
    loop {
        let Some(p) = p.upgrade() else { return };
        let f = &p.followers[k];
        let Some(numbering) = p.db.storage().numbering() else { return };
        let last = numbering.seq;
        let from = next.unwrap_or(last + 1);
        let txns = match from > last {
            true => Vec::new(),
            false => match p.db.storage().log_from(from, p.opts.batch_bytes) {
                LogRead::Txns(t) => t,
                LogRead::Gone { oldest } => {
                    f.drop_out(format!(
                        "it holds transactions up to {}, and this primary keeps them from {oldest} on: it needs a copy of the primary (start it from a checkpoint of the primary)",
                        from - 1
                    ));
                    p.changed();
                    next = None;
                    let wait = p.opts.retry_max;
                    drop(p);
                    tokio::time::sleep(wait).await;
                    continue;
                }
            },
        };
        let prev = SeqNo { term: numbering.term_at(from - 1), seq: from - 1 };
        let end = txns.last().map_or(prev.seq, |t| t.seq.seq);
        let n = txns.len() as u64;
        let req = pb::ReplicateRequest {
            term: numbering.term,
            prev_term: prev.term,
            prev_seq: prev.seq,
            txns: txns.iter().map(|t| t.bytes.to_vec()).collect(),
            global_checkpoint: p.global_checkpoint(),
            primary: p.id.clone(),
        };
        drop(txns);
        let mut c = f.client.clone();
        let failed = match tokio::time::timeout(p.opts.call_timeout, c.replicate(req)).await {
            Ok(Ok(r)) => {
                let r = r.into_inner();
                if r.replica_term > numbering.term {
                    p.fenced.fetch_max(r.replica_term, Ordering::Relaxed);
                    tracing::error!(replica = %f.url, term = numbering.term, replica_term = r.replica_term, "a replica holds a newer term: another primary replaced this one, which takes no more writes");
                    Some(format!("it holds term {}, newer than this primary's ({}): another primary replaced this one", r.replica_term, numbering.term))
                } else if r.matched {
                    debug_assert_eq!(r.seq, end);
                    f.acked.store(r.seq, Ordering::Relaxed);
                    f.shipped.fetch_add(n, Ordering::Relaxed);
                    next = Some(r.seq + 1);
                    // It holds what was committed when the batch was read.
                    if end >= last {
                        f.caught_up();
                    }
                    p.changed();
                    retry = Duration::from_millis(50);
                    None
                } else if r.seq <= last && numbering.term_at(r.seq) == r.term {
                    // It holds a run of this primary's history: send what
                    // follows its last.
                    f.acked.store(r.seq, Ordering::Relaxed);
                    next = Some(r.seq + 1);
                    continue;
                } else {
                    Some(format!(
                        "its last transaction, {} of term {}, is not this primary's (its last is {last}): its history differs from the primary's; start it again from a checkpoint of the primary",
                        r.seq, r.term
                    ))
                }
            }
            Ok(Err(s)) => Some(format!("{}: {}", crate::grpc::code_name(s.code()), s.message())),
            Err(_) => Some(format!("no answer within {} s", p.opts.call_timeout.as_secs())),
        };
        if let Some(why) = failed {
            f.failures.fetch_add(1, Ordering::Relaxed);
            f.drop_out(why);
            p.changed();
            next = None;
            let wait = retry;
            retry = (retry * 2).min(p.opts.retry_max);
            drop(p);
            tokio::time::sleep(wait).await;
            continue;
        }
        if next.is_some_and(|n| n > p.db.storage().numbering().map_or(0, |n| n.seq)) {
            let beat = p.opts.heartbeat;
            drop(p);
            let _ = tokio::time::timeout(beat, committed.changed()).await;
        }
    }
}

/// How long a replica waits for its writer, which a flush, a merge or a
/// checkpoint may hold, before refusing a batch (503).
const WRITER_WAIT: Duration = Duration::from_secs(5);

/// A replica: applies what its primary ships.
pub struct Follow {
    db: Arc<Db>,
    file: Option<PathBuf>,
    kept: tokio::sync::Mutex<Kept>,
    /// The global checkpoint its primary last sent.
    global_checkpoint: AtomicU64,
    /// Transactions applied, and when its primary last called.
    applied: AtomicU64,
    called_at: Mutex<Option<Instant>>,
}

impl Follow {
    /// A replica of `db`, keeping its id and the primary it follows in
    /// `file` (none: in memory).
    pub fn open(db: Arc<Db>, file: Option<&Path>) -> Result<Follow, String> {
        if db.storage().numbering().is_none() {
            return Err("a replica numbers its writes as its primary does: start it with --seq-no".into());
        }
        let kept = Kept::open(file)?;
        Ok(Follow { db, file: file.map(Path::to_path_buf), kept: tokio::sync::Mutex::new(kept), global_checkpoint: AtomicU64::new(0), applied: AtomicU64::new(0), called_at: Mutex::new(None) })
    }

    /// The primary it follows, if one has called.
    pub async fn follows(&self) -> Option<String> {
        self.kept.lock().await.follows.as_ref().map(|(p, _)| p.clone())
    }

    /// Applies a primary's batch (`Replicate`), one batch at a time.
    pub async fn replicate(&self, r: pb::ReplicateRequest) -> Result<pb::ReplicateResponse, ApiError> {
        let mut kept = self.kept.lock().await;
        let numbering = self.db.storage().numbering().ok_or_else(|| ApiError::internal("the replica does not number its writes"))?;
        let term = numbering.term.max(kept.follows.as_ref().map_or(0, |f| f.1));
        let answer = |matched: bool, at: SeqNo, replica_term: u64| pb::ReplicateResponse { matched, seq: at.seq, term: at.term, replica_term };
        if r.term < term {
            return Ok(answer(false, numbering.last(), term));
        }
        match &kept.follows {
            Some((id, t)) if *t == r.term && *id != r.primary => {
                return Err(conflict(format!("this replica follows primary {id} in term {t}, and primary {} ships in the same term: a shard has one primary in a term", r.primary)));
            }
            Some((_, t)) if *t == r.term => {}
            _ => {
                let next = Kept { follows: Some((r.primary.clone(), r.term)), ..kept.clone() };
                next.save(self.file.as_deref()).map_err(|e| ApiError::internal(format!("keep the primary followed: {e}")))?;
                *kept = next;
            }
        }
        *self.called_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let (db, prev, txns) = (self.db.clone(), SeqNo { term: r.prev_term, seq: r.prev_seq }, r.txns);
        let n = txns.len() as u64;
        let applied = tokio::task::spawn_blocking(move || {
            let txns: Vec<&[u8]> = txns.iter().map(|t| &t[..]).collect();
            // A flush or a merge may hold the writer for a moment.
            crate::service::wait_for_writer(Instant::now() + WRITER_WAIT, || db.storage().apply_shipped(prev, &txns).map_err(ApiError::from))
        })
        .await
        .map_err(ApiError::internal)??;
        let replica_term = self.db.storage().numbering().map_or(0, |n| n.term).max(r.term);
        Ok(match applied {
            Applied::At(at) => {
                self.applied.fetch_add(n, Ordering::Relaxed);
                self.global_checkpoint.store(r.global_checkpoint.min(at.seq), Ordering::Relaxed);
                answer(true, at, replica_term)
            }
            Applied::Mismatch { seq, term } => answer(false, SeqNo { term, seq }, replica_term),
        })
    }

    /// What `/v1/health` shows of replication.
    pub async fn health(&self) -> Json {
        let kept = self.kept.lock().await.clone();
        let n = self.db.storage().numbering().unwrap_or_default();
        let mut h = json!({"role": "replica", "id": kept.id, "term": n.term, "seq_no": n.seq, "global_checkpoint": self.global_checkpoint.load(Ordering::Relaxed)});
        if let Some((primary, term)) = kept.follows {
            h["follows"] = json!({"primary": primary, "term": term});
        }
        if let Some(at) = *self.called_at.lock().unwrap_or_else(|e| e.into_inner()) {
            h["called_ms_ago"] = json!(at.elapsed().as_millis() as u64);
        }
        h
    }

    pub fn metrics(&self) -> Vec<Family> {
        vec![
            Family::one("strata_replication_role", "gauge", "1 on a primary (--replicas), 2 on a replica (--replica).", 2.0),
            Family::one("strata_global_checkpoint", "gauge", "The highest sequence number every in-sync replica holds, as the primary last said.", self.global_checkpoint.load(Ordering::Relaxed) as f64),
            Family::one("strata_replica_applied_total", "counter", "Transactions this replica applied from its primary.", self.applied.load(Ordering::Relaxed) as f64),
        ]
    }
}
