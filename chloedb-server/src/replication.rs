//! Replication: a primary ships its numbered transactions to its replicas.
//!
//! A server started with `--replicas` is the primary of its database. It
//! numbers its writes ([`chloedb::storage::Database::start_seq_no`]) and
//! keeps the latest of them as it logged them (its ship log,
//! [`chloedb::storage::DbOptions::ship_log_bytes`]); a task per replica
//! ships that log to the replica's `Replicate` call in order, in batches,
//! under the primary's term, and hears how far the replica got. A server
//! started with `--replica` applies what it is shipped
//! ([`chloedb::storage::Database::apply_shipped`], which checks that each
//! batch follows its last transaction) and takes no writes from clients,
//! so that every copy holds the same rows under the same sequence numbers.
//! A table the primary creates or drops (`CREATE TABLE`, `DROP TABLE`, an
//! import that makes one) and a rank profile are numbered transactions
//! too, shipped in order with the rows: a replica makes the same change,
//! with the primary's table ids.
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
//! A replica the primary's log no longer reaches is sent a copy of the
//! primary (`Restore`): a checkpoint, written beside the primary's
//! database and streamed in chunks, which the replica writes beside its
//! own, opens, and puts in place of its tables, rows, numbers and terms in
//! one commit ([`chloedb::storage::Database::install_copy`]). It then follows
//! from the copy's last transaction, which the primary's log holds until
//! the replica has caught up ([`chloedb::storage::Database::hold_log`]). A
//! replica whose history differs from the primary's is not copied, and
//! the error says so, unless its last transaction is of an older term
//! than the primary's: it then holds transactions of a primary this one
//! replaced (a replaced primary started again as a replica), and the copy
//! drops them.
//!
//! A replica serves reads once a call from its primary since it started
//! reached the primary's last transaction, until a call finds it behind,
//! it fails to apply one or it installs a copy: while it catches up,
//! reads are refused (503), and a coordinator asks another copy. A read
//! that must see a write passes the write's sequence number
//! ([`crate::service::After`]): a replica waits until it has applied it,
//! up to [`READ_WAIT`], and refuses the read otherwise, so that a
//! coordinator asks the primary, which holds every write it answered. A
//! replica its primary cannot reach does not know it fell behind, and
//! goes on serving what it holds.
//!
//! A primary compares each replica's rows with its own every
//! [`PrimaryOptions::verify_every`] (`--verify-every-s`), and when asked
//! ([`Primary::verify`], `POST /v1/admin/verify`). It takes a version of
//! its database and ships the replica up to that version's last
//! transaction, in a call that has the replica keep the version it leaves
//! (`pin`); then it compares the [`Digests`] of the two versions
//! (`Verify`): for each slot of the keys, how many rows it holds and the
//! sum of a hash of each one's table, primary key and sequence number. A
//! replica whose digests differ drops out of sync and is sent a copy of
//! the primary. The digests cover which rows a replica holds and the
//! number of the write that last changed each, not their values: copies
//! that applied the same writes hold the same values, but a replica that
//! held other values under the same keys before numbering began (loaded
//! from other rows) is not found by them.
//!
//! A shard's copies elect its primary, as Raft does: each keeps the newest
//! term it knows and the copy it voted for in it ([`Kept`]), and grants
//! its vote only to a copy whose last transaction is not behind its own
//! ([`shard::decide`]). A copy wins with a majority of the copies its cluster map
//! names for its shard ([`Follow::campaign`]); a write `all` or `majority`
//! answers is held by a majority too ([`Primary::acknowledged`]), so the
//! winner holds every write answered. A replica elects itself when its
//! primary is silent (`--failover-after-ms`, `service::failover`), or is
//! made the primary by hand ([`Follow::promote`]: `POST /v1/admin/promote`,
//! `chloedb-server promote`, a coordinator's
//! `POST /v1/admin/shards/:i/promote`), a stopped one with `--promote`
//! ([`PrimaryOptions::promote`]). By hand, it first asks the copies for
//! their last transaction (`Hello`) and refuses while one holds a later
//! one, then asks for their votes as a handover, which a serving primary
//! grants and steps down for; with `force`, neither. The winner begins
//! the term it won ([`chloedb::storage::Database::begin_term`]), so that
//! the replicas follow it, and an old primary that ships to a copy of the
//! new term is answered with it and stops taking writes and serving reads
//! (it is fenced). It keeps that beside its database, and starts again as
//! a primary only with `--promote`. A primary that a primary of a newer
//! term ships to is fenced the same way, and one of an older term is
//! answered with this one's.
//!
//! What each copy decides is the shard core's ([`crate::cores::shard`]),
//! which the simulator runs too ([`crate::sim`]): a replica's votes, calls
//! taken and reads ([`shard::Replica`]), an election ([`shard::Campaign`]),
//! a promotion by hand ([`shard::promote_check`],
//! [`shard::handover_hold`]), and a primary's shipping, acknowledgments,
//! fencing and lease ([`shard::Primary`], [`shard::Shipper`]). This module
//! does their I/O: the calls, the database, the ship log and the file
//! beside it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use chloedb::db::Db;
use chloedb::storage::{Applied, CheckpointOptions, Database, DbOptions, Layout, Numbering, SeqNo, Snapshot};
use tokio::sync::watch;

use crate::auth::Secret;
use crate::coord::{self, Client};
use crate::cores::shard::{self, Kept, Ship, Shipper, Take};
use crate::metrics::Family;
use crate::slots::{self, SLOTS};
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

/// Digests of a version's rows, slot by slot: what a primary and its
/// replicas compare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digests {
    /// For each of the [`SLOTS`] slots: the rows in it, and the wrapping
    /// sum of each one's hash.
    pub rows: Vec<u64>,
    pub sums: Vec<u64>,
}

impl Digests {
    /// Of every live row of every table `s` holds
    /// ([`Snapshot::row_versions`]): a row hashes its table, its primary
    /// key (as [`slots::slot`] encodes it) and the number of the write that
    /// last changed it (xxh3, 64 bits) into its key's slot. A row of a
    /// table without a primary key goes by its hash.
    pub fn of(s: &Snapshot) -> chloedb::Result<Digests> {
        let mut d = Digests { rows: vec![0; SLOTS], sums: vec![0; SLOTS] };
        let mut buf = Vec::new();
        for schema in s.schemas() {
            let Some(t) = s.table(&schema.table) else { continue };
            let name = schema.table.as_bytes();
            t.row_versions(|key, seq| {
                buf.clear();
                buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
                buf.extend_from_slice(name);
                let at = buf.len();
                let slot = key.map(|k| {
                    slots::encode(k, |b| buf.extend_from_slice(b));
                    (twox_hash::XxHash3_64::oneshot(&buf[at..]) % SLOTS as u64) as usize
                });
                buf.extend_from_slice(&seq.to_le_bytes());
                let h = twox_hash::XxHash3_64::oneshot(&buf);
                let slot = slot.unwrap_or((h % SLOTS as u64) as usize);
                d.rows[slot] += 1;
                d.sums[slot] = d.sums[slot].wrapping_add(h);
            })?;
        }
        Ok(d)
    }

    /// The slots where `self` and `other` differ.
    pub fn differ(&self, other: &Digests) -> Vec<usize> {
        (0..SLOTS).filter(|&i| self.rows.get(i) != other.rows.get(i) || self.sums.get(i) != other.sums.get(i)).collect()
    }

    pub fn to_pb(&self) -> pb::VerifyResponse {
        pb::VerifyResponse { rows: self.rows.clone(), sums: self.sums.clone() }
    }

    /// A replica's answer, which must hold every slot.
    pub fn from_pb(r: pb::VerifyResponse) -> Result<Digests, String> {
        if r.rows.len() != SLOTS || r.sums.len() != SLOTS {
            return Err(format!("its digests hold {} and {} slots, not {SLOTS}", r.rows.len(), r.sums.len()));
        }
        Ok(Digests { rows: r.rows, sums: r.sums })
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
    /// How long a call may take. A copy may take a second more for each
    /// MiB it holds.
    pub call_timeout: Duration,
    /// The bytes of a copy each `Restore` chunk carries.
    pub copy_chunk_bytes: usize,
    /// How long to wait before sending a replica a copy again after one
    /// failed: each copy writes a checkpoint of the database.
    pub copy_retry: Duration,
    /// How often each replica's rows are compared with the primary's
    /// ([`Digests`]); zero: only when asked ([`Primary::verify`]).
    pub verify_every: Duration,
    /// Start as the primary of a new term, above every term the database
    /// knows (`--promote`): needed when it was a replica, or another
    /// primary replaced it.
    pub promote: bool,
    /// Its replicas join it now (`Configure`): each starts out of sync
    /// and is sent a copy of the primary before what follows, as it may
    /// lack rows that took no number (a bulk load). A replica added later
    /// ([`Primary::reconfigure`]) joins so too.
    pub joining: bool,
    /// Serves reads only while a majority of its shard's copies answered a
    /// call that started within this (zero: always): the copies may elect
    /// another primary once they have not heard from this one for longer
    /// (`--failover-after-ms`), and it would serve what that one replaced.
    pub lease: Duration,
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
            copy_chunk_bytes: 1 << 20,
            copy_retry: Duration::from_secs(30),
            verify_every: Duration::from_secs(600),
            promote: false,
            joining: false,
            lease: Duration::ZERO,
        }
    }
}


/// Where a server keeps its id, and a replica the primary it follows.
pub fn file_of(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(".replication");
    p.into()
}

/// A server's part in replication, as kept beside its database, so that
/// it starts again in it without flags saying so ([`kept_role`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeptRole {
    /// The primary of its shard, shipping to these replicas.
    Primary { replicas: Vec<String>, acks: Acks },
    /// A replica, applying what its primary ships.
    Replica,
}

/// The part in replication kept beside the database at `db`
/// ([`KeptRole`]), and whether a primary of a newer term replaced it as a
/// primary: a server started without `--replicas`, `--replica` or
/// `--promote` takes it up again ([`shard::restart`]).
pub fn kept_role(db: &Path) -> Result<(Option<KeptRole>, bool), String> {
    let path = file_of(db);
    if !path.exists() {
        return Ok((None, false));
    }
    let k = open_kept(Some(&path))?;
    Ok((k.role, k.replaced_by.is_some()))
}

/// A copy of the shard that holds more than an empty database: its URL,
/// the newest term it knows and its last transaction's number
/// ([`copies_holding`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holding {
    pub url: String,
    pub term: u64,
    pub seq: u64,
}

/// N-E2, a primary's half: asks each of `copies` what it holds, with a
/// vote of no term (a pre-vote, which none grants or keeps; each answer
/// carries the newest term the copy knows and its last transaction), and
/// answers those that hold a transaction (a new database is at term 1
/// already, and a new term begins with a transaction of its own). A
/// server started as a primary by its flags on an empty database with no part kept beside it
/// asks this first: a copy holding more means the shard served before, so
/// this server lost its volume (or is new to it), and made the primary of
/// term 1 it would ship an empty database to copies that hold the shard's
/// data. A copy that does not answer in `timeout` is left out, as on a
/// first start whose copies are not up yet.
pub async fn copies_holding(copies: &[String], token: Option<&Secret>, ca: Option<&[u8]>, timeout: Duration) -> Vec<Holding> {
    let req = pb::VoteRequest { candidate: format!("starting-{}", new_id()), pre: true, ..Default::default() };
    let asks = copies.iter().map(|url| {
        let req = req.clone();
        async move {
            let mut c = coord::client(url, token, ca).ok()?;
            let a = tokio::time::timeout(timeout, c.vote(req)).await.ok()?.ok()?.into_inner();
            (a.last_seq > 0).then(|| Holding { url: url.clone(), term: a.term.max(a.last_term), seq: a.last_seq })
        }
    });
    futures::future::join_all(asks).await.into_iter().flatten().collect()
}

/// What `path` keeps ([`Kept`]), or a new id, kept there.
fn open_kept(path: Option<&Path>) -> Result<Kept, String> {
    let Some(path) = path else { return Ok(Kept::new(new_id())) };
    if path.exists() {
        let j: Json = std::fs::read(path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string())).map_err(|e| format!("{}: {e}", path.display()))?;
        return Kept::from_json(&j).map_err(|e| format!("{}: {e}", path.display()));
    }
    let k = Kept::new(new_id());
    save_kept(&k, Some(path)).map_err(|e| format!("keep the replication id in {}: {e}", path.display()))?;
    Ok(k)
}

/// Keeps `k` at `path` (none: nowhere), durably.
fn save_kept(k: &Kept, path: Option<&Path>) -> std::io::Result<()> {
    let Some(path) = path else { return Ok(()) };
    crate::service::save_json(path, &k.to_json())
}

/// Forgets the part in replication kept at `file`: the server starts
/// again standalone.
pub fn forget_role(file: Option<&Path>) -> Result<(), String> {
    let k = Kept { role: None, ..open_kept(file)? };
    save_kept(&k, file).map_err(|e| e.to_string())
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
    ApiError { code: None, status: 409, message }
}

/// A path beside the database at `db` for a copy of it, which no other
/// copy has.
fn beside(db: &Path, what: &str) -> PathBuf {
    let name = db.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    db.with_file_name(format!(".{name}.{what}-{}", new_id()))
}

/// Removes the copies of the database at `db` that a transfer left beside
/// it ([`beside`]: a primary's `.<name>.copy-<id>`, the checkpoint it
/// writes first, `..<name>.copy-<id>.checkpoint-<id>`, and a replica's
/// `.<name>.restore-<id>`, each with its log and readers' folder) when its
/// server was killed during it: [`Removed`] runs only when the transfer
/// ends. A copy is the whole database, so a server that was stopped during
/// a few of them kept several times its data on its volume, which nothing
/// showed or removed (LIV-7). Called once the database is open (its
/// writer's lock taken), when no transfer runs. Answers the files removed
/// and their bytes.
pub fn sweep_copies(db: &Path) -> (usize, u64) {
    let Some(name) = db.file_name().map(|n| n.to_string_lossy().into_owned()) else { return (0, 0) };
    let dir = db.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let Ok(entries) = std::fs::read_dir(dir) else { return (0, 0) };
    let ours = |f: &str| [format!(".{name}.copy-"), format!(".{name}.restore-"), format!("..{name}.copy-")].iter().any(|p| f.starts_with(p.as_str()));
    let (mut n, mut bytes) = (0, 0u64);
    for e in entries.flatten() {
        let f = e.file_name().to_string_lossy().into_owned();
        if !ours(&f) {
            continue;
        }
        let p = e.path();
        let size = |p: &Path| -> u64 {
            fn walk(p: &Path) -> u64 {
                match std::fs::symlink_metadata(p) {
                    Ok(m) if m.is_dir() => std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| walk(&e.path())).sum(),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                }
            }
            walk(p)
        };
        let len = size(&p);
        let r = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        match r {
            Ok(()) => {
                n += 1;
                bytes += len;
            }
            Err(e) => tracing::warn!(path = %p.display(), error = %e, "could not remove a copy of the database a transfer left"),
        }
    }
    if n > 0 {
        tracing::warn!(db = %db.display(), files = n, bytes, "removed copies of the database that transfers left when the server was stopped during them");
    }
    (n, bytes)
}

/// Removes the copy at its path, and its log, when dropped.
struct Removed(PathBuf);

impl Drop for Removed {
    fn drop(&mut self) {
        let mut wal = self.0.as_os_str().to_owned();
        wal.push("-wal");
        for p in [self.0.as_path(), Path::new(&wal)] {
            match std::fs::remove_file(p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => tracing::warn!(path = %p.display(), error = %e, "could not remove a copy of the database"),
                _ => {}
            }
        }
        chloedb::storage::share::remove(&self.0, false);
    }
}

/// Keeps a primary's transactions from a number on in its ship log until
/// dropped ([`chloedb::storage::Database::hold_log`]).
struct LogHold {
    db: Arc<Db>,
    id: u64,
}

impl LogHold {
    fn new(db: Arc<Db>, from: u64) -> LogHold {
        let id = db.storage().hold_log(from);
        LogHold { db, id }
    }
}

impl Drop for LogHold {
    fn drop(&mut self) {
        self.db.storage().release_log(self.id);
    }
}

/// A replica, as its primary's I/O sees it: its client, and the rounds
/// comparing its rows with the primary's. What the primary decides about it
/// (how far it holds, whether it is in sync, a member, being sent a copy)
/// is the core's ([`shard::Peer`]).
struct Follower {
    url: String,
    client: Client,
    /// What the last round comparing its rows with the primary's found
    /// ([`verify`]), and whether a round is comparing.
    verified: Mutex<Verified>,
    verifying: AtomicBool,
    /// Rounds that compared its rows, those that found them differ, and
    /// those that could not compare.
    verifications: AtomicU64,
    divergences: AtomicU64,
    verify_failures: AtomicU64,
    /// Wakes its shipper: a round ended, or one was asked for.
    wake: Arc<tokio::sync::Notify>,
}

impl Follower {
    fn new(url: &str, opts: &PrimaryOptions) -> Result<Follower, String> {
        Ok(Follower {
            url: url.to_string(),
            client: coord::client(url, opts.token.as_ref(), opts.ca.as_deref()).map_err(|e| e.message)?,
            verified: Mutex::new(Verified::default()),
            verifying: AtomicBool::new(false),
            verifications: AtomicU64::new(0),
            divergences: AtomicU64::new(0),
            verify_failures: AtomicU64::new(0),
            wake: Arc::new(tokio::sync::Notify::new()),
        })
    }
}

/// What the last round comparing a replica's rows with its primary's found.
#[derive(Clone, Debug, Default)]
struct Verified {
    /// The transaction it compared at, when, and the slots that differed.
    at: Option<(u64, Instant, Vec<usize>)>,
    /// Why the last round could not compare.
    error: Option<String>,
    /// The latest request ([`Primary::verify`]) a round ended for.
    answered: u64,
}

impl Follower {
    fn verified(&self) -> std::sync::MutexGuard<'_, Verified> {
        self.verified.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What its health shows of the last round comparing its rows.
    fn verification(&self) -> Json {
        let v = self.verified().clone();
        let mut j = json!({});
        if let Some((seq, when, differ)) = &v.at {
            j = json!({"seq_no": seq, "ms_ago": when.elapsed().as_millis() as u64, "differing_slots": differ.len()});
            if let Some(first) = differ.first() {
                j["first_differing_slot"] = json!(first);
            }
        }
        if let Some(e) = v.error {
            j["error"] = json!(e);
        }
        j
    }
}

/// What a primary and the primary that takes over from it with other
/// replicas ([`Primary::reconfigure`]) share: the core's decisions, and a
/// counter that moves whenever they change what a write waits for.
struct Shared {
    core: Mutex<shard::Primary>,
    progress: watch::Sender<u64>,
    /// The writes it took that are not applied or refused yet ([`Flight`]).
    flights: watch::Sender<usize>,
}

/// A write a primary took ([`Primary::admit`]), until it is applied or
/// refused, when it is dropped: a primary asked to hand its shard over
/// waits for those ([`Primary::answer`]).
pub struct Flight(Arc<Shared>);

impl std::fmt::Debug for Flight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Flight")
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.0.flights.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// How long a primary asked to hand its shard over takes no writes, unless
/// the candidate's vote comes first ([`Primary::answer`]).
const HANDOVER_PAUSE: Duration = Duration::from_secs(1);

/// How long it waits for the writes it took to be applied then, and the
/// candidate for it to hold their transactions ([`Follow::catch_up`]).
const HANDOVER_CATCH_UP: Duration = Duration::from_millis(500);

/// A primary: ships its database's numbered transactions to its replicas,
/// and tells a write when enough copies hold it, as the core decides
/// ([`shard::Primary`]).
pub struct Primary {
    db: Arc<Db>,
    /// Where its id is kept, and the term that replaced it.
    file: Option<PathBuf>,
    id: String,
    opts: PrimaryOptions,
    /// Its replicas, in the order of `opts.replicas`, as the core's peers.
    followers: Vec<Arc<Follower>>,
    shared: Arc<Shared>,
    /// The last number committed, as the database's commit hook says.
    committed: Arc<watch::Sender<u64>>,
    /// Requests to compare the replicas now ([`Primary::verify`]).
    verify_asked: AtomicU64,
    /// Held while it answers a vote ([`Primary::vote`]).
    voting: Mutex<()>,
}

impl Primary {
    /// Starts shipping `db`'s transactions to `opts.replicas`, keeping
    /// the primary's id in `file` (none: a new one each start). Every
    /// replica counts as in sync until it fails, so that the first writes
    /// wait for replicas that are there.
    ///
    /// A database that followed a primary as a replica, or whose primary
    /// another replaced, starts only with [`PrimaryOptions::promote`],
    /// which begins a term above every term it knows.
    pub fn start(db: Arc<Db>, file: Option<&Path>, opts: PrimaryOptions) -> Result<Arc<Primary>, String> {
        let numbering = db.storage().numbering().ok_or("a primary numbers its writes: start it with --seq-no")?;
        let mut kept = open_kept(file)?;
        let mut first = 0;
        if opts.promote {
            let term = kept.newest(numbering.term) + 1;
            db.storage().begin_term(term).map_err(|e| format!("begin term {term}: {e}"))?;
            first = term_opened(&db, term).map_err(|e| e.message)?;
            kept = Kept { config: kept.config, ..Kept::new(kept.id) };
            tracing::info!(term, "promoted: the primary of a new term");
        } else if let Some((primary, term)) = &kept.follows {
            return Err(format!("this database is a replica of primary {primary} in term {term}: start it with --replica, or make it the primary of a new term with --promote"));
        } else if let Some(term) = kept.replaced_by {
            return Err(format!("a primary of term {term} replaced this one: start it with --replica, as a replica of that primary, or make it the primary of a newer term with --promote"));
        }
        let p = Primary::run(db, file, kept, opts)?;
        p.core(|c| c.term_start = first);
        Ok(p)
    }

    /// Starts shipping as [`Primary::start`] does, under id `kept.id`.
    fn run(db: Arc<Db>, file: Option<&Path>, kept: Kept, opts: PrimaryOptions) -> Result<Arc<Primary>, String> {
        ships(&db)?;
        let (core, save) = shard::Primary::new(kept, &opts.replicas, shard::Opts::of(&opts), opts.joining);
        // A primary counts among its shard's copies, as a replica once it
        // steps down or starts again (`shard::Primary::new`).
        let save = save.or_else(|| file.is_some_and(|f| !f.exists()).then(|| core.kept.clone()));
        if let Some(k) = save {
            save_kept(&k, file).map_err(|e| format!("keep the replication id and role: {e}"))?;
        }
        let followers = opts.replicas.iter().map(|url| Follower::new(url, &opts).map(Arc::new)).collect::<Result<Vec<_>, String>>()?;
        let shared = Arc::new(Shared { core: Mutex::new(core), progress: watch::channel(0).0, flights: watch::channel(0).0 });
        Primary::run_with(db, file, shared, opts, followers)
    }

    /// [`Primary::run`] shipping to `followers` (those of
    /// `opts.replicas`, the core's peers) as `shared` decides.
    fn run_with(db: Arc<Db>, file: Option<&Path>, shared: Arc<Shared>, opts: PrimaryOptions, followers: Vec<Arc<Follower>>) -> Result<Arc<Primary>, String> {
        let numbering = ships(&db)?;
        let committed = Arc::new(watch::channel(numbering.seq).0);
        let hook = committed.clone();
        db.storage().on_commit(move |n| {
            hook.send_replace(n.seq);
        });
        let (id, shippers) = {
            let core = shared.core.lock().unwrap_or_else(|e| e.into_inner());
            (core.id.clone(), core.shippers())
        };
        let p = Arc::new(Primary { db, file: file.map(Path::to_path_buf), id, opts, followers, shared, committed, verify_asked: AtomicU64::new(0), voting: Mutex::new(()) });
        for (k, s) in shippers.into_iter().enumerate() {
            tokio::spawn(ship(Arc::downgrade(&p), k, s));
        }
        Ok(p)
    }

    /// Runs `f` on the core's decisions; writes waiting for copies look
    /// again when they changed what a write waits for.
    fn core<R>(&self, f: impl FnOnce(&mut shard::Primary) -> R) -> R {
        let mut core = self.shared.core.lock().unwrap_or_else(|e| e.into_inner());
        let before = core.progress;
        let r = f(&mut core);
        let moved = core.progress != before;
        drop(core);
        if moved {
            self.changed();
        }
        r
    }

    fn changed(&self) {
        self.shared.progress.send_modify(|n| *n += 1);
    }

    /// Keeps what the core said to keep, when another primary replaced
    /// this one.
    fn keep_fenced(&self, kept: Option<Kept>) {
        if let Some(k) = kept {
            if let Err(e) = save_kept(&k, self.file.as_deref()) {
                tracing::error!(error = %e, "could not keep that another primary replaced this one");
            }
        }
    }

    pub fn acks(&self) -> Acks {
        self.opts.acks
    }

    /// The replicas it ships to.
    pub fn replicas(&self) -> &[String] {
        &self.opts.replicas
    }

    /// Whether it may ship to `replicas` now
    /// ([`shard::Primary::may_reconfigure`]).
    pub fn may_reconfigure(&self, replicas: &[String], accept_data_loss: bool) -> Result<(), ApiError> {
        self.core(|c| c.may_reconfigure(replicas, Instant::now(), accept_data_loss))
    }

    /// A primary of the same database, id and term shipping to `replicas`
    /// under `acks` from now on, in place of this one, which stops
    /// shipping: replicas it ships to already keep what this one knows of
    /// them, and new ones are asked their last transaction (an empty one
    /// is sent a copy). Refused once another primary replaced this one.
    pub fn reconfigure(&self, replicas: Vec<String>, acks: Acks) -> Result<Arc<Primary>, ApiError> {
        ships(&self.db).map_err(ApiError::internal)?;
        let opts = PrimaryOptions { replicas: replicas.clone(), acks, promote: false, ..self.opts.clone() };
        let followers = opts
            .replicas
            .iter()
            .map(|url| match self.followers.iter().find(|f| f.url == *url) {
                Some(f) => Ok(f.clone()),
                None => Follower::new(url, &opts).map(Arc::new),
            })
            .collect::<Result<Vec<_>, String>>()
            .map_err(ApiError::bad)?;
        if let Some(k) = self.core(|c| c.reconfigure(replicas, acks))? {
            save_kept(&k, self.file.as_deref()).map_err(|e| ApiError::internal(format!("keep the replication id and role: {e}")))?;
        }
        let p = Primary::run_with(self.db.clone(), self.file.as_deref(), self.shared.clone(), opts, followers).map_err(ApiError::internal)?;
        tracing::info!(replicas = %p.opts.replicas.join(", "), write_acks = p.opts.acks.name(), "primary: ships to these replicas from now on");
        Ok(p)
    }

    /// Takes no writes for `d` from now ([`Follow::promote`]).
    fn hold_writes(&self, d: Duration) {
        self.core(|c| c.writes_from = Some(Instant::now() + d));
    }

    fn numbering(&self) -> Numbering {
        self.db.storage().numbering().unwrap_or_default()
    }

    fn term(&self) -> u64 {
        self.numbering().term
    }

    /// The highest number every in-sync replica holds (the primary's last
    /// while none is in sync).
    pub fn global_checkpoint(&self) -> u64 {
        let last = *self.committed.borrow();
        self.core(|c| c.global_checkpoint(last))
    }

    /// The newer term of a primary that replaced this one (0: none).
    pub fn replaced_by(&self) -> u64 {
        self.core(|c| c.replaced_by())
    }

    /// Refuses a read once another primary replaced this one (503): it
    /// may hold writes the new primary does not, and not the new
    /// primary's.
    ///
    /// With a lease ([`PrimaryOptions::lease`]), refused too while fewer
    /// than a majority of the copies answered a call started within it:
    /// the others may have elected another primary.
    pub fn readable(&self) -> Result<(), ApiError> {
        let term = self.term();
        self.core(|c| c.readable(Instant::now(), term))
    }

    /// Answers a primary's call to ship to this one (`Replicate`,
    /// `Restore`, `Verify`), as a primary of term `term` makes when it
    /// does not know this one replaced it: answered with this primary's
    /// term, which fences it, when `term` is older. A newer term fences
    /// this primary, and the call is refused (409).
    pub fn shipped_to(&self, primary: &str, term: u64, config: u64) -> Result<pb::ReplicateResponse, ApiError> {
        let n = self.numbering();
        let (a, kept) = self.core(|c| c.shipped_to(primary, term, config, &n));
        self.keep_fenced(kept);
        a
    }

    /// Answers a copy asking to be made the primary of a newer term
    /// (`Vote`). A primary another replaced answers as a replica would.
    /// One that serves grants only a `handover`, a promotion asked by hand,
    /// and only to a copy holding every transaction it holds: it then
    /// steps down, as a primary the new one replaced, and writes it had
    /// not acknowledged are refused.
    pub fn vote(&self, r: &pb::VoteRequest) -> pb::VoteResponse {
        let _one = self.voting.lock().unwrap_or_else(|e| e.into_inner());
        let n = self.numbering();
        let (a, save) = self.core(|c| c.vote(r, &n));
        let Some(k) = save else { return a };
        if let Err(e) = save_kept(&k, self.file.as_deref()) {
            return shard::refused(n.term, n.last(), format!("keep the vote: {e}"));
        }
        let fenced = self.core(|c| c.gave_vote(k, r.term, n.term));
        self.keep_fenced(fenced);
        a
    }

    /// Answers a primary's copy of itself (`Restore`), as
    /// [`Primary::shipped_to`] answers a call.
    pub async fn copied_to(&self, mut chunks: tonic::Streaming<pb::RestoreChunk>) -> Result<pb::ReplicateResponse, ApiError> {
        let first = chunks.message().await.map_err(|s| ApiError::bad(format!("the copy was cut off: {}", s.message())))?.ok_or_else(|| ApiError::bad("the copy is empty"))?;
        self.shipped_to(&first.primary, first.term, first.config)
    }

    /// Whether a write may be applied: not on a primary another replaced
    /// (409), not while it waits out the lease of the primary it replaced
    /// by hand (503, [`Follow::promote`]) or hands its shard over (503,
    /// [`Self::answer`]), and with `majority`, while a majority of copies
    /// is in sync (503), so that a write the copies cannot acknowledge is
    /// not applied. A write taken counts until its [`Flight`] is dropped.
    pub fn admit(&self) -> Result<Flight, ApiError> {
        let term = self.term();
        // Counted as it is taken: a handover that stops writes after it
        // waits for it.
        self.core(|c| c.admit(term, Instant::now()).map(|()| self.shared.flights.send_modify(|n| *n += 1)))?;
        Ok(Flight(self.shared.clone()))
    }

    /// Answers vote `r` ([`Self::vote`]). A copy asking whether it would
    /// vote in a handover first stops it taking writes, for
    /// [`HANDOVER_PAUSE`] or until it answers the vote, and it waits for
    /// the writes it took to be applied, up to [`HANDOVER_CATCH_UP`]: its
    /// answer names its last transaction then, which the copy catches up
    /// with before it asks for the vote ([`Follow::catch_up`]). A copy a
    /// transaction behind is refused the vote, and refused once it kept
    /// its vote for itself, it would leave the shard with no primary: this
    /// one's next call would meet the copy's term, which fences it.
    pub async fn answer(&self, r: &pb::VoteRequest) -> pb::VoteResponse {
        if r.handover && r.pre && self.core(|c| c.hand_over(Instant::now() + HANDOVER_PAUSE)) {
            let mut flights = self.shared.flights.subscribe();
            let _ = tokio::time::timeout(HANDOVER_CATCH_UP, flights.wait_for(|&n| n == 0)).await;
        }
        let a = self.vote(r);
        if r.handover {
            let n = self.numbering();
            self.core(|c| c.answered_handover(r, &n, &a));
        }
        a
    }

    /// Waits until the copies `--write-acks` asks for hold transaction
    /// `seq`, or `deadline`. Then, with `all`, the replicas in sync that
    /// do not hold it drop out of sync; with `majority`, the write is
    /// answered 504.
    pub async fn acknowledged(&self, seq: u64, deadline: Instant) -> Result<(), ApiError> {
        let mut progress = self.shared.progress.subscribe();
        loop {
            if let Some(a) = self.core(|c| c.acked(seq)) {
                return a;
            }
            if tokio::time::timeout_at(deadline.into(), progress.changed()).await.is_err() {
                return self.core(|c| c.deadline(seq));
            }
        }
    }

    /// The core's peers, by URL.
    fn peers(&self) -> std::collections::HashMap<String, shard::Peer> {
        self.core(|c| c.peers.iter().map(|p| (p.url.clone(), p.clone())).collect())
    }

    /// Compares each replica in sync with this primary now, as every
    /// `verify_every` does, and answers what each comparison found,
    /// waiting for them until `deadline`: `compared` is false for a
    /// replica out of sync, or whose round did not end in time.
    pub async fn verify(&self, deadline: Instant) -> Json {
        let n = self.verify_asked.fetch_add(1, Ordering::Relaxed) + 1;
        let mut progress = self.shared.progress.subscribe();
        for f in &self.followers {
            f.wake.notify_one();
        }
        let done = |f: &Follower| f.verified().answered >= n;
        let in_sync = |peers: &std::collections::HashMap<String, shard::Peer>, f: &Follower| peers.get(&f.url).is_some_and(|p| p.in_sync);
        loop {
            let peers = self.peers();
            if self.followers.iter().all(|f| done(f) || !in_sync(&peers, f)) || tokio::time::timeout_at(deadline.into(), progress.changed()).await.is_err() {
                break;
            }
        }
        let peers = self.peers();
        let replicas: Vec<Json> = self
            .followers
            .iter()
            .map(|f| {
                let mut j = f.verification();
                j["url"] = json!(f.url);
                j["in_sync"] = json!(in_sync(&peers, f));
                j["compared"] = json!(done(f));
                j
            })
            .collect();
        json!({"replicas": replicas})
    }

    /// What `/v1/health` shows of replication.
    pub fn health(&self) -> Json {
        let last = *self.committed.borrow();
        let (peers, copies, majority, fenced) = self.core(|c| (c.peers.clone(), c.copies(), c.majority(), c.replaced_by()));
        let replicas: Vec<Json> = peers
            .iter()
            .map(|p| {
                let f = self.followers.iter().find(|f| f.url == p.url);
                let count = |a: Option<&AtomicU64>| a.map_or(0, |a| a.load(Ordering::Relaxed));
                let mut j = json!({
                    "url": p.url, "seq_no": p.acked, "behind": last.saturating_sub(p.acked), "in_sync": p.in_sync,
                    "copying": p.copying, "copies": p.copies, "answered": p.answered.is_some(),
                    "verifications": count(f.map(|f| &f.verifications)), "divergences": count(f.map(|f| &f.divergences)),
                    "verify_failures": count(f.map(|f| &f.verify_failures)), "member": p.member, "txn_mismatches": p.mismatches,
                });
                if let Some(e) = &p.error {
                    j["error"] = json!(e);
                }
                let v = f.map_or(json!({}), |f| f.verification());
                if v.as_object().is_some_and(|o| !o.is_empty()) {
                    j["verified"] = v;
                }
                j
            })
            .collect();
        let mut h = json!({
            "role": "primary", "id": self.id, "term": self.term(), "seq_no": last, "write_acks": self.opts.acks.name(), "copies": copies, "majority": majority,
            "global_checkpoint": self.global_checkpoint(), "verify_every_s": self.opts.verify_every.as_secs(), "replicas": replicas,
        });
        if fenced > 0 {
            h["replaced_by_term"] = json!(fenced);
        }
        h
    }

    pub fn metrics(&self) -> Vec<Family> {
        let last = *self.committed.borrow();
        let peers = self.core(|c| c.peers.clone());
        let by_peer = |f: &dyn Fn(&shard::Peer) -> f64| peers.iter().map(|p| (format!("replica=\"{}\"", p.url), f(p))).collect();
        let by = |f: &dyn Fn(&Follower) -> f64| self.followers.iter().map(|r| (format!("replica=\"{}\"", r.url), f(r))).collect();
        vec![
            Family::one("chloedb_replication_role", "gauge", "1 on a primary (--replicas), 2 on a replica (--replica).", 1.0),
            Family::one("chloedb_global_checkpoint", "gauge", "The highest sequence number every in-sync replica holds.", self.global_checkpoint() as f64),
            Family { name: "chloedb_replica_in_sync", ty: "gauge", help: "1 while a replica holds what the primary committed, as far as it was shipped.", values: by_peer(&|p| p.in_sync as u8 as f64) },
            Family { name: "chloedb_replica_behind", ty: "gauge", help: "Transactions the primary committed that a replica does not hold yet.", values: by_peer(&|p| last.saturating_sub(p.acked) as f64) },
            Family { name: "chloedb_replica_shipped_total", ty: "counter", help: "Transactions a replica applied from this primary.", values: by_peer(&|p| p.shipped as f64) },
            Family { name: "chloedb_replica_failures_total", ty: "counter", help: "Calls to a replica that failed or that it refused.", values: by_peer(&|p| p.failures as f64) },
            Family { name: "chloedb_replica_copies_total", ty: "counter", help: "Copies of the primary a replica installed: the log no longer reached it, or its rows differed.", values: by_peer(&|p| p.copies as f64) },
            Family { name: "chloedb_replica_verifications_total", ty: "counter", help: "Times a replica's rows were compared with the primary's.", values: by(&|f| f.verifications.load(Ordering::Relaxed) as f64) },
            Family { name: "chloedb_replica_divergences_total", ty: "counter", help: "Times a replica's rows differed from the primary's.", values: by(&|f| f.divergences.load(Ordering::Relaxed) as f64) },
            Family { name: "chloedb_replica_verify_failures_total", ty: "counter", help: "Rounds that could not compare a replica's rows with the primary's.", values: by(&|f| f.verify_failures.load(Ordering::Relaxed) as f64) },
            Family { name: "chloedb_replica_txn_mismatches_total", ty: "counter", help: "Calls a replica refused for holding another transaction than the primary's under the term and number they follow (it is then sent a copy of the primary).", values: by_peer(&|p| p.mismatches as f64) },
        ]
    }
}

/// The database's numbering, as a primary ships it: refused when it does
/// not number its writes, or keeps no log of them.
/// The mark a new primary's first transaction sets and clears
/// ([`term_opened`]).
pub const TERM_MARK: &str = "_chloedb/term";

/// Commits the first transaction of `term`, which `db` began: one that
/// changes no row (it sets and clears [`TERM_MARK`] among the stream
/// sources), and returns its number. As Raft's leader does at the start
/// of its term, a new primary counts no transaction of an older term as
/// held by a majority until a majority holds this one
/// ([`shard::Primary::term_start`]). Finding DS-5 (Raft's figure 8): a
/// primary elected in term 3 answered the retry of a term-1 transaction
/// as acknowledged once a majority held it, then died; the copy holding a
/// term-2 transaction at the same number was elected, as its last term
/// was newer, and the acknowledged transaction was dropped from every
/// copy. Once a majority holds a transaction of term 3, no copy without
/// it can be elected.
pub fn term_opened(db: &Db, term: u64) -> Result<u64, ApiError> {
    let s = db.storage();
    s.ensure_sources()?;
    let mut t = s.begin()?;
    t.mark_source(TERM_MARK, term as i64, chloedb::timestamp::now())?;
    t.commit_seq()?.ok_or_else(|| ApiError::internal(format!("the first transaction of term {term} has no number")))
}

fn ships(db: &Db) -> Result<Numbering, String> {
    let numbering = db.storage().numbering().ok_or("a primary numbers its writes: start it with --seq-no")?;
    if !db.storage().keeps_ship_log() {
        return Err("a primary keeps a log of its transactions to ship (DbOptions::ship_log_bytes)".into());
    }
    Ok(numbering)
}

/// A round comparing a replica's rows with its primary's: the primary's
/// version, its last transaction, and the latest request
/// ([`Primary::verify`]) the round answers.
struct Round {
    snap: Snapshot,
    seq: u64,
    asked: u64,
}

/// Ships `p`'s transactions to its replica `k` until `p` is dropped or
/// ships to it no more, carrying out what shipper `s` and the core decide
/// ([`shard::Primary::poll`]): asks the replica its last transaction,
/// sends what follows in batches, sends it a copy of the primary when the
/// log no longer reaches it or its rows differ, calls it every heartbeat
/// while there is nothing to send, and starts the rounds that compare its
/// rows with the primary's.
async fn ship(p: Weak<Primary>, k: usize, mut s: Shipper) {
    let Some(mut committed) = p.upgrade().map(|p| p.committed.subscribe()) else { return };
    // What follows a copy, kept until the replica has it.
    let mut _hold: Option<LogHold> = None;
    // A round under way, until the call that pins the replica's version
    // at the round's transaction, and when the next round is due.
    let mut round: Option<Round> = None;
    let mut due = Instant::now() + p.upgrade().map_or(Duration::ZERO, |p| p.opts.verify_every);
    let mut step = Ship::Again;
    // The primary that decided a copy, kept until the copy is sent and
    // its answer given to the core: the core marks the replica as being
    // sent one (`Peer::copying`), and the shipper of a primary given other
    // replicas meanwhile waits for that copy's answer before it ships to
    // the replica. A task that let go of its primary between the decision
    // and the copy (it was replaced, and dropped) would leave it waiting
    // for ever.
    let mut copying: Option<Arc<Primary>> = None;
    loop {
        let pr = match copying.take() {
            Some(pr) => pr,
            None => match p.upgrade() {
                Some(pr) => pr,
                None => return,
            },
        };
        let f = pr.followers[k].clone();
        let Some(n) = pr.db.storage().numbering() else { return };
        step = match step {
            Ship::Again => {
                // A round starts when asked or due, once the replica holds a
                // run of this primary's history and is in sync.
                let every = pr.opts.verify_every;
                let asked = pr.verify_asked.load(Ordering::Relaxed);
                let wanted = asked > f.verified().answered || (!every.is_zero() && Instant::now() >= due);
                if wanted && !f.verifying.load(Ordering::Relaxed) && pr.core(|c| c.may_verify(k, &s)) {
                    let snap = pr.db.snapshot();
                    let seq = snap.seq_no().unwrap_or(0);
                    if s.verify_at(seq) {
                        round = Some(Round { snap, seq, asked });
                        due = Instant::now() + every;
                    }
                }
                let next = pr.core(|c| c.poll(k, &mut s, &n, Instant::now()));
                if matches!(next, Ship::Copy { .. }) {
                    copying = Some(pr.clone());
                }
                next
            }
            Ship::Read { from } => {
                let log = pr.db.storage().log_from(from, pr.opts.batch_bytes);
                let next = pr.core(|c| c.read(k, &mut s, &n, log, Instant::now()));
                if matches!(next, Ship::Copy { .. }) {
                    copying = Some(pr.clone());
                }
                next
            }
            Ship::Call(mut req) => {
                // The digest of the transaction the call follows, for the
                // replica to compare with its own.
                if req.prev_seq > 0 {
                    let log = pr.db.storage().log_from(req.prev_seq, 1);
                    shard::sign(&mut req, log, &shard::digest);
                }
                let mut client = f.client.clone();
                let answer = match tokio::time::timeout(pr.opts.call_timeout, client.replicate(req)).await {
                    Ok(Ok(r)) => Ok(r.into_inner()),
                    Ok(Err(s)) => Err(format!("{}: {}", crate::grpc::code_name(s.code()), s.message())),
                    Err(_) => Err(format!("no answer within {} s", pr.opts.call_timeout.as_secs())),
                };
                let n = pr.numbering();
                let (next, kept) = pr.core(|c| c.answered(k, &mut s, answer, &n));
                pr.keep_fenced(kept);
                next
            }
            Ship::Copy { hold_from, term } => {
                // The copy holds at least what was committed by now.
                _hold = Some(LogHold::new(pr.db.clone(), hold_from));
                let answer = send_copy(&pr, &f, term).await;
                let n = pr.numbering();
                let (next, kept) = pr.core(|c| c.copied(k, &mut s, answer, &n));
                pr.keep_fenced(kept);
                next
            }
            Ship::Sleep(d) => {
                drop(pr);
                tokio::time::sleep(d).await;
                Ship::Again
            }
            Ship::Idle(d) => {
                drop(pr);
                let _ = tokio::time::timeout(d, async {
                    tokio::select! {
                        _ = committed.changed() => {}
                        _ = f.wake.notified() => {}
                    }
                })
                .await;
                Ship::Again
            }
            Ship::Stop => return,
        };
        if !s.holds() {
            _hold = None;
        }
        // The replica keeps its version at the round's transaction:
        // compare them, while shipping goes on.
        if s.take_pinned() {
            if let (Some(rd), Some(pr)) = (round.take(), p.upgrade()) {
                pr.followers[k].verifying.store(true, Ordering::Relaxed);
                tokio::spawn(verify(Arc::downgrade(&pr), k, rd));
            }
        }
        if s.round().is_none() {
            round = None;
        }
    }
}

/// Compares replica `k`'s digests at the version its last call pinned
/// with `p`'s at the same transaction, the round's: a replica whose
/// digests differ drops out of sync, and its shipper sends it a copy of
/// the primary ([`shard::Primary::diverged`]). Shipping goes on meanwhile.
async fn verify(p: Weak<Primary>, k: usize, r: Round) {
    let Some(pr) = p.upgrade() else { return };
    let f = pr.followers[k].clone();
    let req = pb::VerifyRequest { term: pr.term(), primary: pr.id.clone(), seq_no: r.seq, config: pr.core(|c| c.kept.config_version()) };
    // A second more for each 100,000 rows to hash.
    let rows: u64 = r.snap.schemas().filter_map(|s| r.snap.table(&s.table)).map(|t| t.n_live_docs()).sum();
    let timeout = pr.opts.call_timeout + Duration::from_secs(rows / 100_000);
    let mut c = f.client.clone();
    drop(pr);
    let (seq, asked, snap) = (r.seq, r.asked, r.snap);
    let (theirs, ours) = tokio::join!(tokio::time::timeout(timeout, c.verify(req)), tokio::task::spawn_blocking(move || Digests::of(&snap)));
    let found = match (theirs, ours) {
        (_, Err(e)) => Err(format!("the primary's digests: {e}")),
        (_, Ok(Err(e))) => Err(format!("the primary's digests: {e}")),
        (Err(_), _) => Err(format!("no digests within {} s", timeout.as_secs())),
        (Ok(Err(s)), _) => Err(format!("{}: {}", crate::grpc::code_name(s.code()), s.message())),
        (Ok(Ok(theirs)), Ok(Ok(ours))) => Digests::from_pb(theirs.into_inner()).map(|theirs| ours.differ(&theirs)),
    };
    let Some(pr) = p.upgrade() else { return };
    {
        let mut v = f.verified();
        match found {
            Ok(differ) => {
                f.verifications.fetch_add(1, Ordering::Relaxed);
                if let Some(first) = differ.first() {
                    f.divergences.fetch_add(1, Ordering::Relaxed);
                    let why = format!("its rows differ from the primary's at transaction {seq}, in {} of the {SLOTS} slots (the first: {first}): it is sent a copy of the primary", differ.len());
                    pr.core(|c| c.diverged(&f.url, why));
                }
                v.at = Some((seq, Instant::now(), differ));
                v.error = None;
            }
            Err(e) => {
                f.verify_failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(replica = %f.url, seq_no = seq, error = %e, "could not compare a replica's rows with the primary's");
                v.error = Some(e);
            }
        }
        v.answered = v.answered.max(asked);
    }
    f.verifying.store(false, Ordering::Relaxed);
    f.wake.notify_one();
    pr.changed();
}

/// Sends replica `f` a copy of `p`'s database (`Restore`) under `term`: a
/// checkpoint written beside the database and removed once sent, in
/// chunks. Returns the replica's answer.
async fn send_copy(p: &Primary, f: &Follower, term: u64) -> Result<pb::ReplicateResponse, String> {
    let path = beside(p.db.storage().path(), "copy");
    let _removed = Removed(path.clone());
    let (db, to) = (p.db.clone(), path.clone());
    tokio::task::spawn_blocking(move || db.storage().checkpoint(&to, &CheckpointOptions::default()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("checkpoint: {e}"))?;
    let mut file = std::fs::File::open(&path).map_err(|e| format!("open the checkpoint: {e}"))?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    // A thread reads the file a chunk ahead of the call.
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    let (chunk, primary) = (p.opts.copy_chunk_bytes.max(1), p.id.clone());
    let config = p.core(|c| c.kept.config_version());
    let at = path.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut first = Some((term, primary, size, config));
        let mut sent = 0;
        loop {
            let mut data = Vec::with_capacity(chunk);
            if let Err(e) = (&mut file).take(chunk as u64).read_to_end(&mut data) {
                // The replica finds the copy short.
                tracing::warn!(path = %at.display(), error = %e, "could not read the checkpoint to copy");
                return;
            }
            if data.is_empty() && first.is_none() {
                return;
            }
            sent += data.len() as u64;
            let (term, primary, said, config) = first.take().unwrap_or_default();
            if tx.blocking_send(pb::RestoreChunk { term, primary, size: said, data, config }).is_err() || sent >= size {
                return;
            }
        }
    });
    let timeout = p.opts.call_timeout + Duration::from_secs(size >> 20);
    let mut c = f.client.clone();
    match tokio::time::timeout(timeout, c.restore(tokio_stream::wrappers::ReceiverStream::new(rx))).await {
        Ok(Ok(r)) => Ok(r.into_inner()),
        Ok(Err(s)) => Err(format!("{}: {}", crate::grpc::code_name(s.code()), s.message())),
        Err(_) => Err(format!("no answer within {} s", timeout.as_secs())),
    }
}

/// How long a replica waits for its writer, which a flush, a merge or a
/// checkpoint may hold, before refusing a batch (503).
const WRITER_WAIT: Duration = Duration::from_secs(5);

/// How long a read on a replica waits for a write it must see
/// ([`crate::service::After`]) before it is refused (503), within its
/// deadline. An in-sync replica applies a write within milliseconds of
/// its primary; one a second behind is better skipped for the primary.
pub const READ_WAIT: Duration = Duration::from_secs(1);

/// A replica: applies what its primary ships, as the core decides
/// ([`shard::Replica`]).
pub struct Follow {
    db: Arc<Db>,
    file: Option<PathBuf>,
    /// Taken by each call that may change what it keeps, one at a time: a
    /// batch, a copy, a vote, a campaign's own vote, taking over. Shared
    /// so that a copy's install, which outlives a call its caller gave up,
    /// holds it to the end ([`Follow::restore`]).
    one: Arc<tokio::sync::Mutex<()>>,
    /// The core's decisions, and what it keeps beside its database.
    core: Mutex<shard::Replica>,
    /// Transactions applied, and copies of the primary installed.
    applied: AtomicU64,
    restores: AtomicU64,
    /// The last number committed, as the database's commit hook says, for
    /// reads waiting for a write ([`Follow::holding`]).
    committed: Arc<watch::Sender<u64>>,
    /// Reads that waited for a write, and reads refused.
    read_waits: AtomicU64,
    reads_refused: AtomicU64,
    /// The version a call from its primary pinned for the `Verify` that
    /// follows, and the digests it computed for its primary.
    pinned: Mutex<Option<Snapshot>>,
    digests: AtomicU64,
}

/// What a copy that won an election holds ([`Follow::campaign`]).
#[derive(Clone, Debug)]
pub struct Elected {
    /// The term it won.
    pub term: u64,
    /// The shard's other copies asked, which it ships to as primary.
    pub others: Vec<String>,
    /// What each copy answered.
    pub votes: Json,
}

/// How recently a replica's primary called for the replica to refuse
/// another copy's vote that is no handover, when the replica has no
/// failover delay of its own.
pub const STICKY: Duration = Duration::from_secs(5);

impl Follow {
    /// A replica of `db`, keeping its id and the primary it follows in
    /// `file` (none: in memory).
    pub fn open(db: Arc<Db>, file: Option<&Path>) -> Result<Follow, String> {
        Follow::open_as(db, file, false)
    }

    /// A replica started with `--replica`, as [`Self::open`]: on a database
    /// that had no part in replication yet, it is one of its shard's first
    /// copies, which count among the copies a majority is counted of from
    /// the start, as Raft's first members do ([`shard::Replica::open`]).
    pub fn open_first(db: Arc<Db>, file: Option<&Path>) -> Result<Follow, String> {
        Follow::open_as(db, file, true)
    }

    fn open_as(db: Arc<Db>, file: Option<&Path>, flags: bool) -> Result<Follow, String> {
        if db.storage().numbering().is_none() {
            return Err("a replica numbers its writes as its primary does: start it with --seq-no".into());
        }
        let (core, save) = shard::Replica::open(open_kept(file)?, Instant::now(), flags);
        if let Some(k) = save {
            save_kept(&k, file).map_err(|e| format!("keep the replication role: {e}"))?;
        }
        let committed = Arc::new(watch::channel(db.storage().numbering().map_or(0, |n| n.seq)).0);
        let hook = committed.clone();
        db.storage().on_commit(move |n| {
            hook.send_replace(n.seq);
        });
        Ok(Follow {
            db,
            file: file.map(Path::to_path_buf),
            one: Arc::new(tokio::sync::Mutex::new(())),
            core: Mutex::new(core),
            applied: AtomicU64::new(0),
            restores: AtomicU64::new(0),
            committed,
            read_waits: AtomicU64::new(0),
            reads_refused: AtomicU64::new(0),
            pinned: Mutex::new(None),
            digests: AtomicU64::new(0),
        })
    }

    fn core(&self) -> std::sync::MutexGuard<'_, shard::Replica> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn numbering(&self) -> Result<Numbering, ApiError> {
        self.db.storage().numbering().ok_or_else(|| ApiError::internal("the replica does not number its writes"))
    }

    /// Keeps `k` beside the database, then in the core.
    fn keep(&self, k: Kept, what: &str) -> Result<(), ApiError> {
        save_kept(&k, self.file.as_deref()).map_err(|e| ApiError::internal(format!("keep {what}: {e}")))?;
        self.core().kept = k;
        Ok(())
    }

    /// How long since its primary last called, or since it started
    /// following when none has.
    pub fn silent_for(&self) -> Duration {
        self.core().silent_for(Instant::now())
    }

    /// Answers a copy asking to be made the primary of a newer term
    /// (`Vote`), as [`shard::decide`] does, keeping the vote unless only
    /// asked (`pre`). Refused, but for a `handover`, while its primary
    /// called within `sticky`: that primary serves.
    pub async fn vote(&self, r: &pb::VoteRequest, sticky: Duration) -> Result<pb::VoteResponse, ApiError> {
        let _one = self.one.lock().await;
        let n = self.numbering()?;
        self.core().saw_vote(r);
        let (a, save) = self.core().vote(r, &n, Instant::now(), sticky);
        if let Some(k) = save {
            self.keep(k, "the vote")?;
            self.core().granted(Instant::now());
            tracing::info!(term = r.term, candidate = %r.candidate, handover = r.handover, "voted for a copy to be the shard's primary");
        }
        Ok(a)
    }

    /// Asks `copies`, the servers of its shard (itself among them or not),
    /// to make this replica the primary of the next term
    /// ([`shard::Campaign`]): first whether they would (`pre`), which
    /// changes nothing, then for their votes, keeping its own. Wins with a
    /// majority of the copies, itself counted once; refused (409)
    /// otherwise, saying what each answered. A `handover` is a promotion
    /// asked by hand. The term it asked for stays voted in when it loses:
    /// copies that granted it refuse the primary of an older term from then.
    ///
    /// It asks for the term after the newest it knows or a copy told it
    /// ([`shard::Replica::heard`]). On its own (not a `handover`), each
    /// round ends once its answers decide it ([`shard::Campaign::decided`]):
    /// it waits for no copy that is down.
    ///
    /// The copies of `first` (in a handover, those that serve as the
    /// shard's primary) are asked for their votes before the others, while
    /// this replica answers no call of theirs: once it voted for itself, it
    /// would answer a call with the term it asks for, and so would a copy
    /// that voted for it, and a primary that knows a term refuses a vote in
    /// it. Refused, the primary is fenced and does not step down, and with
    /// two copies the shard is left with no primary.
    pub async fn campaign(&self, copies: &[String], opts: &PrimaryOptions, handover: bool, first: &[String]) -> Result<Elected, ApiError> {
        let c = self.core().campaign(&self.numbering()?, handover);
        let early = (!handover && first.is_empty()).then_some(&c);
        let t = c.tally(copies, self.ask(c.ask(self.numbering()?.last(), true), copies, opts, early).await);
        self.core().heard(&t);
        c.pre_voted(&t)?;
        let (before, after): (Vec<String>, Vec<String>) = copies.iter().cloned().partition(|u| first.contains(u));
        let (req, said) = {
            let _one = self.one.lock().await;
            let n = self.numbering()?;
            let k = c.vote_self(&self.core(), &n)?;
            self.keep(k, "the vote")?;
            let req = c.ask(n.last(), false);
            let said = self.ask(req.clone(), &before, opts, None).await;
            (req, said)
        };
        let (mut said, mut rest) = (said.into_iter(), self.ask(req, &after, opts, early).await.into_iter());
        let answers = copies.iter().map(|u| if first.contains(u) { said.next() } else { rest.next() }.expect("an answer from each copy")).collect();
        let t = c.tally(copies, answers);
        self.core().heard(&t);
        let votes = t.granted;
        let e = c.voted(t)?;
        tracing::info!(term = e.term, votes, of = e.others.len() + 1, "elected the shard's primary");
        Ok(e)
    }

    /// What each of `copies` answered vote `req` (an error: no answer),
    /// waiting for every answer, or, with `until`, only until the answers
    /// decide that campaign's round
    /// ([`shard::Campaign::decided`]): a copy it stopped waiting for is
    /// answered as not waited for.
    async fn ask(&self, req: pb::VoteRequest, copies: &[String], opts: &PrimaryOptions, until: Option<&shard::Campaign>) -> Vec<Result<pb::VoteResponse, String>> {
        use futures::StreamExt;
        let mut calls: futures::stream::FuturesUnordered<_> = copies
            .iter()
            .enumerate()
            .map(|(i, url)| {
                let req = req.clone();
                async move {
                    let a = async {
                        let mut c = coord::client(url, opts.token.as_ref(), opts.ca.as_deref()).map_err(|e| e.message)?;
                        match tokio::time::timeout(opts.call_timeout, c.vote(req)).await {
                            Ok(Ok(a)) => Ok(a.into_inner()),
                            Ok(Err(s)) => Err(format!("{}: {}", crate::grpc::code_name(s.code()), s.message())),
                            Err(_) => Err(format!("no answer within {} s", opts.call_timeout.as_secs())),
                        }
                    };
                    (i, a.await)
                }
            })
            .collect();
        let mut said: Vec<Option<Result<pb::VoteResponse, String>>> = vec![None; copies.len()];
        while let Some((i, a)) = calls.next().await {
            said[i] = Some(a);
            if until.is_some_and(|c| c.decided(&said)) {
                break;
            }
        }
        said.into_iter().map(|a| a.unwrap_or_else(|| Err("not waited for: the copies that answered decided the round".into()))).collect()
    }

    /// Before a handover: asks `copies` whether they would vote for it,
    /// which stops their primary taking writes and names the last
    /// transaction each holds ([`Primary::answer`]), and waits until it
    /// holds the newest of them, up to [`HANDOVER_CATCH_UP`]. Under writes,
    /// its primary and the other copies hold a transaction or two it has
    /// not applied yet, and would refuse it the vote.
    async fn catch_up(&self, copies: &[String], opts: &PrimaryOptions) -> Result<(), ApiError> {
        let c = self.core().campaign(&self.numbering()?, true);
        let t = c.tally(copies, self.ask(c.ask(self.numbering()?.last(), true), copies, opts, None).await);
        let until = Instant::now() + HANDOVER_CATCH_UP;
        let mut committed = self.committed.subscribe();
        loop {
            let (at, now) = (self.numbering()?.last(), Instant::now());
            if (at.term, at.seq) >= t.last || now >= until {
                return Ok(());
            }
            // As in `holding`: a copy installed tells no hook.
            let _ = tokio::time::timeout((until - now).min(Duration::from_millis(10)), committed.changed()).await;
        }
    }

    /// Whether a read may take a new snapshot here: once a call from its
    /// primary since this server started reached the primary's last
    /// transaction, until a call finds it behind, it fails to apply one or
    /// it installs a copy. Refused otherwise (503), as it is catching up.
    pub fn readable(&self) -> Result<(), ApiError> {
        let r = self.core().readable();
        if r.is_err() {
            self.reads_refused.fetch_add(1, Ordering::Relaxed);
        }
        r
    }

    /// Counts a read refused for its write's term ([`crate::service`]'s
    /// `holds_term`), with those refused for its number.
    pub fn refused_a_read(&self) {
        self.reads_refused.fetch_add(1, Ordering::Relaxed);
    }

    /// The latest version, once it holds transaction `seq`, waiting for it
    /// until `until`: refused then (503), as the primary holds it.
    pub async fn holding(&self, seq: u64, until: Instant) -> Result<Snapshot, ApiError> {
        let mut committed = self.committed.subscribe();
        let mut waited = false;
        loop {
            let snap = self.db.snapshot();
            let at = snap.seq_no().unwrap_or(0);
            if at >= seq {
                return Ok(snap);
            }
            if !waited {
                waited = true;
                self.read_waits.fetch_add(1, Ordering::Relaxed);
            }
            let now = Instant::now();
            if now >= until {
                self.reads_refused.fetch_add(1, Ordering::Relaxed);
                return Err(ApiError {
                    code: None, status: 503,
                    message: format!("this replica holds transactions up to {at}, and the read must see transaction {seq}, which it has not applied in time: read from the primary"),
                });
            }
            // A copy installed tells no hook: look again every few
            // milliseconds too.
            let _ = tokio::time::timeout((until - now).min(Duration::from_millis(10)), committed.changed()).await;
        }
    }

    /// The primary it follows, if one has called.
    pub async fn follows(&self) -> Option<String> {
        self.core().kept.follows.as_ref().map(|(p, _)| p.clone())
    }

    /// Makes this replica the primary of a new term by hand, shipping as
    /// `opts` says: once the copies in `opts.replicas` that answer hold no
    /// transaction past this replica's last, nor a newer term
    /// ([`shard::promote_check`]), and a majority of `voters` (the shard's
    /// copies; `opts.replicas` when `None`) voted for it
    /// ([`Self::campaign`], as a handover: a primary that serves steps
    /// down). It catches up with them first ([`Self::catch_up`]), which
    /// stops a primary that serves taking writes until it votes, and asks
    /// that primary for its vote before the others. Refused
    /// (409) otherwise, unless `force`, which asks for no
    /// votes: the new primary may then not hold transactions another copy
    /// holds, which a copy of it drops, and a primary that serves still may
    /// until it hears of the new term. Answers the new term, its last
    /// transaction, what each copy said and the votes.
    pub async fn promote(&self, mut opts: PrimaryOptions, voters: Option<Vec<String>>, force: bool) -> Result<(Arc<Primary>, Json), ApiError> {
        // With no replicas named, the shard's copies are asked, and shipped
        // to once elected.
        let named = !opts.replicas.is_empty();
        let checked = match (named, &voters) {
            (false, Some(v)) => v.clone(),
            _ => opts.replicas.clone(),
        };
        if !force {
            self.catch_up(voters.as_deref().unwrap_or(&opts.replicas), &opts).await?;
        }
        let mine = self.numbering()?;
        let asks = checked.iter().map(|url| async {
            let mut c = coord::client(url, opts.token.as_ref(), opts.ca.as_deref()).map_err(|e| e.message)?;
            let hello = c.hello(pb::HelloRequest { min_protocol: crate::grpc::MIN_PROTOCOL, max_protocol: crate::grpc::PROTOCOL });
            match tokio::time::timeout(opts.call_timeout, hello).await {
                Ok(Ok(h)) => Ok(h.into_inner()),
                Ok(Err(s)) => Err(format!("{}: {}", crate::grpc::code_name(s.code()), s.message())),
                Err(_) => Err(format!("no answer within {} s", opts.call_timeout.as_secs())),
            }
        });
        let said: Vec<_> = checked.iter().cloned().zip(futures::future::join_all(asks).await).collect();
        let asked = shard::promote_check(&mine, &said, force)?;
        let elected = match force {
            true => None,
            false => Some(self.campaign(voters.as_deref().unwrap_or(&opts.replicas), &opts, true, &asked.primaries).await?),
        };
        match (named, &elected, &voters) {
            (false, Some(e), _) => opts.replicas = e.others.clone(),
            (false, None, Some(v)) => opts.replicas = self.others(v, &opts).await,
            _ => {}
        }
        let lease = opts.lease;
        let (p, mut answer) = self.take_over(elected.as_ref().map(|e| e.term), opts).await?;
        let hold = shard::handover_hold(Some(&asked), elected.as_ref(), lease);
        if !hold.is_zero() {
            p.hold_writes(hold);
            tracing::info!(ms = hold.as_millis() as u64, "promoted by hand: takes writes once the lease of the primary it replaced has run out");
            answer["writes_in_ms"] = json!(hold.as_millis() as u64);
        }
        answer["replicas"] = json!(asked.copies);
        if let Some(e) = elected {
            answer["votes"] = e.votes;
        }
        Ok((p, answer))
    }

    /// The servers of `copies` that are not this one, which each says when
    /// asked whether it would vote (a vote of no term, which none grants
    /// or keeps); one that does not answer is taken for another.
    async fn others(&self, copies: &[String], opts: &PrimaryOptions) -> Vec<String> {
        let id = self.core().kept.id.clone();
        let req = pb::VoteRequest { candidate: id, pre: true, ..Default::default() };
        let asks = copies.iter().map(|url| {
            let req = req.clone();
            async move {
                let mut c = coord::client(url, opts.token.as_ref(), opts.ca.as_deref()).ok()?;
                match tokio::time::timeout(opts.call_timeout, c.vote(req)).await {
                    Ok(Ok(a)) => Some(a.into_inner().candidate),
                    _ => None,
                }
            }
        });
        let said = futures::future::join_all(asks).await;
        copies.iter().zip(said).filter(|(_, me)| *me != Some(true)).map(|(u, _)| u.clone()).collect()
    }

    /// Begins the term it won (`won`, [`Self::campaign`]), or, when none,
    /// a term above every term it knows ([`shard::Replica::take_over`]),
    /// while no call from its primary is taken, so that a call of the old
    /// primary that follows is answered with the new term; then ships as
    /// `opts` says. Refused (409) when it learned of a newer term than the
    /// one it won.
    pub async fn take_over(&self, won: Option<u64>, opts: PrimaryOptions) -> Result<(Arc<Primary>, Json), ApiError> {
        let _one = self.one.lock().await;
        let (term, next) = self.core().take_over(won, &self.numbering()?, &opts.replicas, opts.acks)?;
        let db = self.db.clone();
        let first = tokio::task::spawn_blocking(move || {
            let until = Instant::now() + WRITER_WAIT;
            crate::service::wait_for_writer(until, || db.storage().begin_term(term).map_err(ApiError::from))?;
            crate::service::wait_for_writer(until, || term_opened(&db, term))
        })
        .await
        .map_err(ApiError::internal)??;
        self.keep(next.clone(), "the replication id")?;
        let at = self.db.storage().numbering().unwrap_or_default();
        tracing::info!(term, seq_no = at.seq, replicas = %opts.replicas.join(", "), write_acks = opts.acks.name(), elected = won.is_some(), "promoted: the primary of a new term");
        let p = Primary::run(self.db.clone(), self.file.as_deref(), next, opts).map_err(ApiError::internal)?;
        p.core(|c| c.term_start = first);
        Ok((p, json!({"term": at.term, "seq_no": at.seq})))
    }

    /// What it does with a call from `primary` of `term` before it applies
    /// anything ([`shard::Replica::take`]): `Some` answer to give at once,
    /// or `None` once it follows that primary (kept beside the database).
    fn take(&self, term: u64, primary: &str, member: bool, config: (u64, u32), n: &Numbering) -> Result<Option<pb::ReplicateResponse>, ApiError> {
        let take = self.core().take(term, primary, member, config, n)?;
        match take {
            Take::Answer(a) => Ok(Some(a)),
            Take::Apply { kept } => {
                if let Some(k) = &kept {
                    save_kept(k, self.file.as_deref()).map_err(|e| ApiError::internal(format!("keep the primary followed: {e}")))?;
                }
                self.core().took(kept, Instant::now());
                Ok(None)
            }
        }
    }

    /// Applies a primary's batch (`Replicate`), one batch at a time.
    pub async fn replicate(&self, r: pb::ReplicateRequest) -> Result<pb::ReplicateResponse, ApiError> {
        let _one = self.one.lock().await;
        let n = self.numbering()?;
        if let Some(a) = self.take(r.term, &r.primary, r.member, (r.config, r.copies), &n)? {
            return Ok(a);
        }
        // Nothing else writes here: the transaction the call follows, when
        // this replica holds one of its term and number, stays as compared.
        let storage = self.db.storage();
        let refused = self.core().follows(&r, &n, || storage.log_from(r.prev_seq, 1), &shard::digest);
        if let Some((a, kept)) = refused {
            if let Some(k) = &kept {
                save_kept(k, self.file.as_deref()).map_err(|e| ApiError::internal(format!("keep that this replica differs from its primary: {e}")))?;
            }
            self.core().took(kept, Instant::now());
            return Ok(a);
        }
        let (db, prev, txns) = (self.db.clone(), SeqNo { term: r.prev_term, seq: r.prev_seq }, r.txns);
        let count = txns.len() as u64;
        let applied = tokio::task::spawn_blocking(move || {
            let txns: Vec<&[u8]> = txns.iter().map(|t| &t[..]).collect();
            // A flush or a merge may hold the writer for a moment.
            crate::service::wait_for_writer(Instant::now() + WRITER_WAIT, || db.apply_shipped(prev, &txns).map_err(ApiError::from))
        })
        .await
        .map_err(ApiError::internal)
        .and_then(|r| r);
        // Nothing else writes here: the latest version holds the batch's
        // last transaction, and no later one.
        if let (true, Ok(Applied::At(at))) = (r.pin, &applied) {
            let snap = self.db.snapshot();
            if snap.seq_no() == Some(at.seq) {
                *self.pinned.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap);
            }
        }
        if let Ok(Applied::At(_)) = &applied {
            self.applied.fetch_add(count, Ordering::Relaxed);
        }
        let db_term = self.db.storage().numbering().map_or(0, |n| n.term);
        self.core().applied(r.term, r.behind, r.global_checkpoint, applied, db_term)
    }

    /// Its digests at the version a call from its primary pinned
    /// (`Verify`): refused (409) for a primary or term it does not follow,
    /// and when it holds no version pinned at that transaction. The
    /// version is let go once answered.
    pub async fn verify(&self, r: pb::VerifyRequest) -> Result<pb::VerifyResponse, ApiError> {
        let follows = self.core().kept.follows.clone();
        match &follows {
            Some((id, t)) if *id == r.primary && *t == r.term => {}
            f => {
                let follows = f.as_ref().map_or("no primary".to_string(), |(id, t)| format!("primary {id} in term {t}"));
                return Err(conflict(format!("this replica follows {follows}, not primary {} in term {}", r.primary, r.term)));
            }
        }
        let snap = {
            let mut pinned = self.pinned.lock().unwrap_or_else(|e| e.into_inner());
            match pinned.as_ref().map(|s| s.seq_no().unwrap_or(0)) {
                Some(at) if at == r.seq_no => pinned.take(),
                at => {
                    let holds = at.map_or("none".to_string(), |at| format!("one at {at}"));
                    return Err(conflict(format!(
                        "this replica holds no version pinned at transaction {} (it holds {holds}): its primary pins one with the call that ships that transaction",
                        r.seq_no
                    )));
                }
            }
        };
        let d = tokio::task::spawn_blocking(move || snap.map(|s| Digests::of(&s))).await.map_err(ApiError::internal)?;
        let d = d.ok_or_else(|| ApiError::internal("the pinned version went"))??;
        self.digests.fetch_add(1, Ordering::Relaxed);
        Ok(d.to_pb())
    }

    /// Puts the copy of its primary that `chunks` carry (`Restore`) in
    /// place of its database: writes it beside the database, opens it,
    /// checking every segment's checksum, and installs it in one commit
    /// ([`chloedb::storage::Database::install_copy`]). The primary's term
    /// is checked as [`Follow::replicate`] checks it. Answers the copy's
    /// last transaction.
    pub async fn restore(&self, mut chunks: tonic::Streaming<pb::RestoreChunk>) -> Result<pb::ReplicateResponse, ApiError> {
        let cut = |s: tonic::Status| ApiError::bad(format!("the copy was cut off: {}", s.message()));
        let first = chunks.message().await.map_err(cut)?.ok_or_else(|| ApiError::bad("the copy is empty"))?;
        let one = self.one.clone().lock_owned().await;
        let n = self.numbering()?;
        if let Some(a) = self.take(first.term, &first.primary, false, (first.config, 0), &n)? {
            return Ok(a);
        }
        // Its rows are behind its primary's log, or differ: no reads
        // until the copy is in and a call finds it caught up.
        self.core().copying();
        *self.pinned.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let path = beside(self.db.storage().path(), "restore");
        let _removed = Removed(path.clone());
        let size = first.size;
        let mut file = std::fs::File::create_new(&path).map_err(|e| ApiError::internal(format!("write the copy beside the database: {e}")))?;
        let (mut got, mut data) = (0u64, Some(first.data));
        loop {
            let d = match data.take() {
                Some(d) => d,
                None => match chunks.message().await.map_err(cut)? {
                    Some(c) => c.data,
                    None => break,
                },
            };
            got += d.len() as u64;
            if got > size {
                return Err(ApiError::bad(format!("the copy is longer than the {size} bytes it said")));
            }
            file = tokio::task::spawn_blocking(move || {
                use std::io::Write;
                file.write_all(&d).map(|_| file)
            })
            .await
            .map_err(ApiError::internal)?
            .map_err(|e| ApiError::internal(format!("write the copy beside the database: {e}")))?;
        }
        if got != size {
            return Err(ApiError::bad(format!("the copy ended at {got} of its {size} bytes")));
        }
        let db = self.db.clone();
        // One that differs holds transactions that are not its primary's,
        // however far they reach: the copy goes in over them. Otherwise a
        // copy that ends before a transaction of its term held here is
        // refused (one taken before what its primary shipped here since).
        let over = self.core().kept.differs.is_some();
        // The install goes on when the call ends early (its caller gave
        // up, or the connection broke) and this future is dropped: it keeps
        // `one` until it is done, so that no call is taken meanwhile.
        let (at, _one) = tokio::task::spawn_blocking(move || {
            file.sync_all().map_err(|e| ApiError::internal(format!("write the copy beside the database: {e}")))?;
            drop(file);
            let copy = Database::open(&path, DbOptions { layout: Layout::SingleFile, verify_checksums: true, ..DbOptions::default() }).map_err(|e| ApiError::bad(format!("the copy: {e}")))?;
            // Opening falls back past a damaged version; a copy must not.
            if let Some(what) = copy.damaged() {
                return Err(ApiError::bad(format!("the copy is damaged: {what}")));
            }
            // A flush or a merge may hold the writer for a moment.
            let install = || match over {
                true => db.install_copy_over(&copy),
                false => db.install_copy(&copy),
            };
            crate::service::wait_for_writer(Instant::now() + WRITER_WAIT, || install().map_err(ApiError::from)).map(|at| (at, one))
        })
        .await
        .map_err(ApiError::internal)??;
        self.restores.fetch_add(1, Ordering::Relaxed);
        tracing::info!(primary = %first.primary, seq_no = at.seq, "installed a copy of the primary");
        let db_term = self.db.storage().numbering().map_or(0, |n| n.term);
        let (a, kept) = self.core().installed(at, first.term, db_term);
        if let Some(k) = &kept {
            save_kept(k, self.file.as_deref()).map_err(|e| ApiError::internal(format!("keep that this replica holds a copy of its primary: {e}")))?;
        }
        self.core().took(kept, Instant::now());
        Ok(a)
    }

    /// Whether it serves reads now: its primary's last call reached the
    /// primary's last transaction.
    pub fn serves_reads(&self) -> bool {
        self.core().serves_reads
    }

    /// The transaction it holds under a term and number as another than
    /// its primary's, as the primary's last call found
    /// ([`shard::Replica::follows`]), until it applies a call or installs
    /// a copy of the primary.
    pub fn differs(&self) -> Option<SeqNo> {
        self.core().kept.differs
    }

    /// What `/v1/health` shows of replication.
    pub async fn health(&self) -> Json {
        let core = self.core().clone();
        let kept = core.kept;
        let n = self.db.storage().numbering().unwrap_or_default();
        let mut h = json!({
            "role": "replica", "id": kept.id, "term": n.term, "seq_no": n.seq, "global_checkpoint": core.global_checkpoint,
            "restores": self.restores.load(Ordering::Relaxed), "serves_reads": core.serves_reads,
            "read_waits": self.read_waits.load(Ordering::Relaxed), "reads_refused": self.reads_refused.load(Ordering::Relaxed),
            "digests": self.digests.load(Ordering::Relaxed), "txn_mismatches": core.mismatches,
        });
        if let Some(at) = kept.differs {
            h["differs"] = json!({"seq_no": at.seq, "term": at.term});
        }
        if let Some((primary, term)) = kept.follows {
            h["follows"] = json!({"primary": primary, "term": term});
        }
        if let Some((term, candidate)) = kept.voted {
            h["voted"] = json!({"term": term, "for": candidate});
        }
        h["member"] = json!(kept.member);
        if let Some(at) = core.called_at {
            h["called_ms_ago"] = json!(at.elapsed().as_millis() as u64);
        }
        h
    }

    pub fn metrics(&self) -> Vec<Family> {
        let (gc, serves, mismatches) = {
            let c = self.core();
            (c.global_checkpoint, c.serves_reads, c.mismatches)
        };
        vec![
            Family::one("chloedb_replication_role", "gauge", "1 on a primary (--replicas), 2 on a replica (--replica).", 2.0),
            Family::one("chloedb_global_checkpoint", "gauge", "The highest sequence number every in-sync replica holds, as the primary last said.", gc as f64),
            Family::one("chloedb_replica_applied_total", "counter", "Transactions this replica applied from its primary.", self.applied.load(Ordering::Relaxed) as f64),
            Family::one("chloedb_replica_restores_total", "counter", "Copies of its primary this replica installed.", self.restores.load(Ordering::Relaxed) as f64),
            Family::one("chloedb_replica_serves_reads", "gauge", "1 while this replica serves reads: its primary's last call reached the primary's last transaction.", serves as u8 as f64),
            Family::one("chloedb_replica_read_waits_total", "counter", "Reads that waited for this replica to apply a write they must see (min_seq_no).", self.read_waits.load(Ordering::Relaxed) as f64),
            Family::one("chloedb_replica_reads_refused_total", "counter", "Reads refused while this replica caught up, or because it had not applied a write they must see in time.", self.reads_refused.load(Ordering::Relaxed) as f64),
            Family::one("chloedb_replica_digests_total", "counter", "Digests of its rows this replica computed for its primary to compare (Verify).", self.digests.load(Ordering::Relaxed) as f64),
            Family::one("chloedb_replica_txn_mismatches_total", "counter", "Calls of its primary this replica refused for holding another transaction under the term and number they follow (it is then sent a copy of the primary).", mismatches as f64),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LIV-7: a server stopped during a transfer left its copy of the
    /// database beside it, and the next start kept it: a replica held five
    /// times its data. The next start removes those, and nothing else.
    #[test]
    fn a_start_removes_the_copies_transfers_left() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db.strata");
        let write = |name: &str, n: usize| std::fs::write(dir.path().join(name), vec![7u8; n]).unwrap();
        write("db.strata", 10);
        write("db.strata-wal", 10);
        write(".db.strata.restore-00ab", 1000);
        write(".db.strata.restore-00ab-wal", 10);
        write(".db.strata.copy-00cd", 500);
        write("..db.strata.copy-00cd.checkpoint-0001", 300);
        write(".other.strata.copy-00ef", 50);
        std::fs::create_dir(dir.path().join(".db.strata.restore-00ab-readers")).unwrap();
        write(".db.strata.restore-00ab-readers/WRITER", 5);
        assert_eq!(sweep_copies(&db), (5, 1815));
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, [".other.strata.copy-00ef", "db.strata", "db.strata-wal"]);
        assert_eq!(sweep_copies(&db), (0, 0));
    }
}
