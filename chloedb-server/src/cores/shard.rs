//! A shard's copies, as decisions with no I/O ([`super`]): what each copy
//! keeps beside its database ([`Kept`]), how it votes ([`decide`]), how a
//! replica takes what its primary ships ([`Replica`]), how a replica is
//! elected ([`Campaign`], [`Failover`]), and how a primary ships to its
//! replicas, counts the copies that hold a write, is fenced and holds a
//! lease on reads ([`Primary`], [`Shipper`]).
//!
//! The server runs these decisions ([`crate::replication`], whose
//! documentation says what they promise and why), and so does the
//! simulator ([`crate::sim`]): [`crate::replication`] does the I/O, the
//! calls, the database and the files, and each function here decides one
//! step. What a caller must keep durably before it answers comes back as a
//! [`Kept`] to save; the time is passed in.

use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use chloedb::storage::{Applied, LogRead, Numbering, SeqNo};

use crate::replication::{Acks, Elected, KeptRole, PrimaryOptions};
use crate::{pb, ApiError};

fn conflict(message: String) -> ApiError {
    ApiError { code: None, status: 409, message }
}

fn unavailable(message: String) -> ApiError {
    ApiError { code: None, status: 503, message }
}

/// What a copy keeps beside its database (`<db>.replication`), durably
/// before it answers the call that changed it: its id; the primary it
/// follows and that one's term; the newer term of a primary that replaced
/// it, as a primary; its part; the term it last voted in and the copy it
/// voted for; whether a primary counted it among the copies a majority
/// is counted of (it grants votes from then); and the newest membership of
/// its shard it knows ([`Self::config`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Kept {
    pub id: String,
    pub follows: Option<(String, u64)>,
    pub replaced_by: Option<u64>,
    pub role: Option<KeptRole>,
    pub voted: Option<(u64, String)>,
    pub member: bool,
    /// The shard's membership as a primary last numbered it: its version,
    /// one more each time a primary is given other replicas
    /// ([`Primary::reconfigure`]), and the copies it counts, the primary
    /// among them. A primary ships it in every call; a replica keeps the
    /// newest it was sent, refuses its vote to a candidate that knows an
    /// older one ([`decide`]), and, as a candidate, needs a majority of
    /// its copies ([`Campaign::tally`]). Finding DS-1: a primary cut off
    /// from its majority was given other replicas, counted a majority of
    /// those, and acknowledged writes while the copies it was cut off from
    /// elected another.
    pub config: Option<(u64, u64)>,
    /// The transaction it holds under a term and number as another than
    /// its primary's, as a call's digest found ([`Replica::follows`]),
    /// until it installs a copy of a primary: it applies no call that
    /// carries a digest meanwhile, started again too, as it cannot tell
    /// once its log no longer holds that transaction.
    pub differs: Option<SeqNo>,
}

impl Kept {
    /// A copy with id `id` and nothing else kept yet.
    pub fn new(id: String) -> Kept {
        Kept { id, ..Kept::default() }
    }

    /// Whether it has had no part in replication yet: a copy started so,
    /// as its shard's primary (`--replicas`) or a replica (`--replica`), is
    /// one of the shard's first copies.
    pub fn first(&self) -> bool {
        self.role.is_none() && self.follows.is_none() && self.replaced_by.is_none()
    }

    /// The newest term it knows of, besides the database's `term`: of the
    /// primary it follows, of the one that replaced it, and the term it
    /// voted in.
    pub fn newest(&self, term: u64) -> u64 {
        term.max(self.follows.as_ref().map_or(0, |f| f.1)).max(self.replaced_by.unwrap_or(0)).max(self.voted.as_ref().map_or(0, |v| v.0))
    }

    /// The version of the membership it knows ([`Self::config`]; 0: none).
    pub fn config_version(&self) -> u64 {
        self.config.map_or(0, |c| c.0)
    }

    /// The copies that membership counts (0: none known).
    pub fn config_copies(&self) -> usize {
        self.config.map_or(0, |c| c.1 as usize)
    }

    /// It was sent membership `version` of `copies` copies: kept when newer
    /// than the one it knows.
    fn heard_config(&self, version: u64, copies: u32) -> Option<Kept> {
        (version > self.config_version() && copies > 0).then(|| Kept { config: Some((version, copies as u64)), ..self.clone() })
    }

    /// As `<db>.replication` holds it.
    pub fn to_json(&self) -> Json {
        let mut j = json!({"id": self.id});
        if let Some((primary, term)) = &self.follows {
            j["follows"] = json!({"primary": primary, "term": term});
        }
        if let Some(t) = self.replaced_by {
            j["replaced_by"] = json!(t);
        }
        if let Some((term, candidate)) = &self.voted {
            j["voted"] = json!({"term": term, "for": candidate});
        }
        if self.member {
            j["member"] = json!(true);
        }
        if let Some((version, copies)) = self.config {
            j["config"] = json!({"version": version, "copies": copies});
        }
        if let Some(at) = self.differs {
            j["differs"] = json!({"term": at.term, "seq_no": at.seq});
        }
        match &self.role {
            None => {}
            Some(KeptRole::Replica) => j["role"] = json!("replica"),
            Some(KeptRole::Primary { replicas, acks }) => {
                j["role"] = json!("primary");
                j["replicas"] = json!(replicas);
                j["write_acks"] = json!(acks.name());
            }
        }
        j
    }

    /// From what `<db>.replication` holds ([`Self::to_json`]).
    pub fn from_json(j: &Json) -> Result<Kept, String> {
        let id = j["id"].as_str().filter(|s| !s.is_empty()).ok_or("no id")?.to_string();
        let follows = match &j["follows"] {
            Json::Null => None,
            f => Some((f["primary"].as_str().unwrap_or_default().to_string(), f["term"].as_u64().ok_or("no term followed")?)),
        };
        let replaced_by = match &j["replaced_by"] {
            Json::Null => None,
            t => Some(t.as_u64().ok_or("replaced_by is not a term")?),
        };
        let role = match j["role"].as_str() {
            None => None,
            Some("replica") => Some(KeptRole::Replica),
            Some("primary") => {
                let replicas = j["replicas"].as_array().map(|a| a.iter().filter_map(|u| u.as_str().map(String::from)).collect()).unwrap_or_default();
                let acks = j["write_acks"].as_str().and_then(Acks::parse).unwrap_or_default();
                Some(KeptRole::Primary { replicas, acks })
            }
            Some(r) => return Err(format!("role {r:?} is primary or replica")),
        };
        let voted = match &j["voted"] {
            Json::Null => None,
            v => Some((v["term"].as_u64().ok_or("no term voted in")?, v["for"].as_str().unwrap_or_default().to_string())),
        };
        let member = j["member"].as_bool().unwrap_or(false);
        let config = match &j["config"] {
            Json::Null => None,
            c => Some((c["version"].as_u64().ok_or("no membership version")?, c["copies"].as_u64().ok_or("no copies in the membership")?)),
        };
        let differs = match &j["differs"] {
            Json::Null => None,
            d => Some(SeqNo { term: d["term"].as_u64().ok_or("no term of the transaction that differs")?, seq: d["seq_no"].as_u64().ok_or("no number of the transaction that differs")? }),
        };
        Ok(Kept { id, follows, replaced_by, role, voted, member, config, differs })
    }
}

/// A vote refused, by a copy that knows of term `term` and holds `last`.
pub fn refused(term: u64, last: SeqNo, reason: String) -> pb::VoteResponse {
    pb::VoteResponse { granted: false, term, last_seq: last.seq, last_term: last.term, reason, candidate: false }
}

/// Whether a copy keeping `kept`, at `term` and holding `last`, grants
/// vote `r`: one vote a term, for a newer term than any it knows, and for
/// a copy holding every transaction it holds (by last term, then last
/// number), once a primary counted it a member, and that knows the newest
/// membership of the shard it knows ([`Kept::config`]; a candidate that
/// numbers none, of an older server, is not asked). The candidate asking
/// itself is told so.
pub fn decide(kept: &Kept, term: u64, last: SeqNo, r: &pb::VoteRequest) -> pb::VoteResponse {
    let newest = kept.newest(term);
    if r.candidate == kept.id {
        return pb::VoteResponse { granted: true, term: newest, last_seq: last.seq, last_term: last.term, reason: String::new(), candidate: true };
    }
    let no = |reason: String| refused(newest, last, reason);
    match &kept.voted {
        Some((t, c)) if *t == r.term && *c == r.candidate && !r.pre => {}
        Some((t, c)) if *t >= r.term => return no(format!("it voted for {c} in term {t}")),
        _ if r.term <= newest => return no(format!("it knows term {newest}, not older than the term asked for ({})", r.term)),
        _ => {}
    }
    if (r.last_term, r.last_seq) < (last.term, last.seq) {
        return no(format!("it holds transactions up to {} of term {}, past the candidate's last ({} of term {})", last.seq, last.term, r.last_seq, r.last_term));
    }
    if !kept.member {
        return no("it has not caught up with a primary since it joined its shard, so it grants no votes".into());
    }
    // The candidate may count a majority of copies that are no longer the
    // shard's, while a primary given the newer membership counts another.
    if r.config > 0 && kept.config_version() > r.config {
        return no(format!(
            "it knows version {} of the shard's membership ({} copies), newer than the candidate's ({})",
            kept.config_version(),
            kept.config_copies(),
            r.config
        ));
    }
    let term = if r.pre { newest } else { newest.max(r.term) };
    pb::VoteResponse { granted: true, term, last_seq: last.seq, last_term: last.term, reason: String::new(), candidate: false }
}

/// How a server starts again in the part it keeps beside its database
/// (`role`, and whether a primary of a newer term `replaced` it as a
/// primary), started without a part on its command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restart {
    /// A replica, which follows the primary that ships to it.
    Replica,
    /// The primary it was, shipping to `replicas`.
    Primary { replicas: Vec<String>, acks: Acks },
}

/// [`Restart`]: a primary starts again as one, but as a replica when its
/// shard's copies `elect` their primary (automatic failover, and a map
/// naming more than one copy), as another may have been elected while it
/// was down; a replica, or a primary another replaced, as a replica.
/// `None`: standalone.
pub fn restart(role: Option<&KeptRole>, replaced: bool, elect: bool) -> Option<Restart> {
    match (role?, replaced, elect) {
        (KeptRole::Primary { .. }, false, true) => Some(Restart::Replica),
        (KeptRole::Primary { replicas, acks }, false, false) => Some(Restart::Primary { replicas: replicas.clone(), acks: *acks }),
        _ => Some(Restart::Replica),
    }
}

/// A digest of a shipped transaction, as `Replicate` carries the one a
/// call follows (`ReplicateRequest.prev_digest`,
/// [`crate::grpc::SAME_TERM_PROTOCOL`]): xxh3 of its records as every copy
/// logs them alike ([`chloedb::storage::wal::shipped_parts`]), so that a
/// primary and a replica that applied its transaction give the same one,
/// and two different transactions of one term and number different ones.
pub fn digest(bytes: &[u8]) -> u64 {
    match chloedb::storage::wal::shipped_parts(bytes) {
        Some(parts) => {
            // Each part after its length, so that no two runs of parts
            // read alike.
            let mut all = Vec::with_capacity(bytes.len() + 8 * parts.len());
            for p in parts {
                all.extend_from_slice(&(p.len() as u64).to_le_bytes());
                all.extend_from_slice(p);
            }
            twox_hash::XxHash3_64::oneshot(&all)
        }
        // Not records: hashed whole.
        None => twox_hash::XxHash3_64::oneshot(bytes),
    }
}

/// The digest (`digest`) of the transaction numbered `at`, in a log read
/// from its number: 0, which a call without one sends, when the log does
/// not hold it (it no longer reaches it, or `at` is the start).
fn held_digest(log: LogRead, at: SeqNo, digest: &dyn Fn(&[u8]) -> u64) -> u64 {
    match log {
        _ if at.seq == 0 => 0,
        LogRead::Txns(t) => t.first().filter(|t| t.seq == at).map_or(0, |t| digest(&t.bytes).max(1)),
        LogRead::Gone { .. } => 0,
    }
}

/// Puts in call `r` the primary's digest of the transaction it follows
/// (`prev_digest`), from its log read from that number (`log`), for the
/// replica to compare with its own ([`Replica::follows`]); 0 when the
/// call follows none, or the log no longer holds it.
pub fn sign(r: &mut pb::ReplicateRequest, log: LogRead, digest: &dyn Fn(&[u8]) -> u64) {
    r.prev_digest = held_digest(log, SeqNo { term: r.prev_term, seq: r.prev_seq }, digest);
}

/// Refuses (409) a call from primary `primary` of `term` that numbers the
/// shard's membership `config`, older than the one a copy keeping `kept`
/// knows (finding E-1): copies removed from the shard while they were
/// down come back holding the membership before, elect one of themselves
/// in a newer term (they are a majority of it), and its calls would
/// otherwise depose the shard's serving primary, or take a replica of it
/// away. The copy answers as for no call: neither follows the caller nor
/// is fenced by its term. A caller that numbers none (0, a server from
/// before memberships were numbered) is not refused.
pub fn stale_membership(kept: &Kept, primary: &str, term: u64, config: u64) -> Result<(), ApiError> {
    let mine = kept.config_version();
    if config == 0 || config >= mine {
        return Ok(());
    }
    Err(conflict(format!(
        "primary {primary} of term {term} ships version {config} of the shard's membership, and this copy holds version {mine}: the copies that elected it are no longer all the shard's (removed, or cut off while its copies changed), so it is not followed; start it on a new database, or add it to the shard again"
    )))
}

/// What a replica does with a call from a primary, before it applies
/// anything ([`Replica::take`]).
#[derive(Debug)]
pub enum Take {
    /// Answer this, applying nothing: the primary is of an older term.
    Answer(pb::ReplicateResponse),
    /// Keep `kept` beside the database when it is some, then take what
    /// was shipped ([`Replica::took`]).
    Apply { kept: Option<Kept> },
}

/// A replica: what it keeps, when its primary last called, and whether it
/// serves reads.
#[derive(Clone, Debug)]
pub struct Replica {
    pub kept: Kept,
    pub called_at: Option<Instant>,
    pub serves_reads: bool,
    /// When it started following, for [`Self::silent_for`].
    pub opened: Instant,
    /// The global checkpoint its primary last sent.
    pub global_checkpoint: u64,
    /// The newest term a copy said it knows, answering its votes
    /// ([`Self::heard`]); its next [`Self::campaign`] asks above it.
    pub heard: u64,
    /// When it last granted a vote ([`Self::granted`]).
    pub voted_at: Option<Instant>,
    /// How many calls it refused for holding another transaction than its
    /// primary's under the term and number they follow ([`Self::follows`]).
    pub mismatches: u64,
    /// The newest term a vote request named, granted or not
    /// ([`Self::saw_vote`]): until a primary of that term or a newer one
    /// calls, it serves no reads, since that term may have a primary
    /// already whose writes it does not hold (X-E1).
    pub asked_term: u64,
}

impl Replica {
    /// A replica keeping `kept`, from `now`, and what to keep when its
    /// part changes. Started with `--replica` (`flags`) on a database that
    /// had no part yet ([`Kept::first`]), it is one of its shard's first
    /// copies, which count among the copies a majority is counted of from
    /// the start, as Raft's first members do: the primary started with
    /// them counts them so, and if it stopped before any of them heard from
    /// it, they could otherwise never elect another. A replica that joins a
    /// serving shard counts once it caught up ([`Self::take`]).
    pub fn open(kept: Kept, now: Instant, flags: bool) -> (Replica, Option<Kept>) {
        let member = kept.member || (flags && kept.first());
        let next = Kept { role: Some(KeptRole::Replica), member, ..kept.clone() };
        let save = (next != kept).then_some(next);
        let kept = save.clone().unwrap_or(kept);
        (Replica { kept, called_at: None, serves_reads: false, opened: now, global_checkpoint: 0, heard: 0, voted_at: None, mismatches: 0, asked_term: 0 }, save)
    }

    /// How long since its primary last called or it granted a vote
    /// ([`Self::granted`]), or since it started following when neither
    /// happened: automatic failover asks to elect it after so long.
    pub fn silent_for(&self, now: Instant) -> Duration {
        let last = self.called_at.max(self.voted_at).unwrap_or(self.opened);
        now.saturating_duration_since(last)
    }

    /// It kept a vote it granted at `now` (not a pre-vote): as in Raft, it
    /// waits a whole failover delay from then before it asks to be elected
    /// itself. The copy it voted for may be a moment from winning; a
    /// campaign of its own would ask for a newer term, and that term,
    /// kept, fences the primary it just elected.
    pub fn granted(&mut self, now: Instant) {
        self.voted_at = Some(now);
    }

    /// It was asked for a vote of `r` (not a pre-vote, which asks only
    /// whether it would): a term newer than the one its primary ships in
    /// may have a primary already, whose writes it does not hold. It stops
    /// serving reads until a primary of that term or a newer one calls
    /// (X-E1); a read then goes to another copy or the primary.
    pub fn saw_vote(&mut self, r: &pb::VoteRequest) {
        let following = self.kept.follows.as_ref().map_or(0, |f| f.1);
        if !r.pre && r.term > following && r.term > self.asked_term {
            self.asked_term = r.term;
            self.serves_reads = false;
        }
    }

    /// Answers vote `r` ([`decide`]) at `n`, and what to keep (the vote)
    /// before answering. Refused, but for a `handover`, while its primary
    /// called within `sticky`: that primary serves.
    pub fn vote(&self, r: &pb::VoteRequest, n: &Numbering, now: Instant, sticky: Duration) -> (pb::VoteResponse, Option<Kept>) {
        let kept = &self.kept;
        let quiet = now.saturating_duration_since(self.called_at.unwrap_or(self.opened));
        if !r.handover && r.candidate != kept.id && self.called_at.is_some() && quiet < sticky {
            return (refused(kept.newest(n.term), n.last(), format!("its primary called {} ms ago", quiet.as_millis())), None);
        }
        let a = decide(kept, n.term, n.last(), r);
        let save = (a.granted && !a.candidate && !r.pre).then(|| Kept { voted: Some((r.term, r.candidate.clone())), ..kept.clone() });
        (a, save)
    }

    /// What it does with a call from primary `primary` of `term` (a
    /// `Replicate` or the first chunk of a `Restore`), at `n`: an older
    /// term than it knows is answered with its own, and a second primary of
    /// the term it follows is refused (409); otherwise it follows that
    /// primary, counting itself a member once a call says so (`member`),
    /// and keeping the membership the call names (`config`: its version
    /// and copies) when newer than the one it knows.
    pub fn take(&self, term: u64, primary: &str, member: bool, config: (u64, u32), n: &Numbering) -> Result<Take, ApiError> {
        let kept = &self.kept;
        // A term it voted in counts: the primary of an older one lost.
        let newest = kept.newest(n.term);
        if term < newest {
            let last = n.last();
            return Ok(Take::Answer(pb::ReplicateResponse { matched: false, seq: last.seq, term: last.term, replica_term: newest, ..Default::default() }));
        }
        stale_membership(kept, primary, term, config.0)?;
        let mut next = match &kept.follows {
            Some((id, t)) if *t == term && id != primary => {
                return Err(conflict(format!("this replica follows primary {id} in term {t}, and primary {primary} ships in the same term: a shard has one primary in a term")));
            }
            Some((_, t)) if *t == term => None,
            _ => Some(Kept { follows: Some((primary.to_string(), term)), replaced_by: None, role: Some(KeptRole::Replica), ..kept.clone() }),
        };
        if member && !kept.member {
            next.get_or_insert_with(|| kept.clone()).member = true;
        }
        if let Some(k) = next.as_ref().unwrap_or(kept).heard_config(config.0, config.1) {
            next = Some(k);
        }
        Ok(Take::Apply { kept: next })
    }

    /// It keeps `kept` (when some) and heard from its primary at `now`.
    pub fn took(&mut self, kept: Option<Kept>, now: Instant) {
        if let Some(k) = kept {
            self.kept = k;
        }
        self.called_at = Some(now);
    }

    /// The answer to a `Replicate` it took, once applied: `applied` is
    /// what [`chloedb::storage::Database::apply_shipped`] did, `db_term` the
    /// database's term after it, and `term`, `behind` and
    /// `global_checkpoint` are the call's. It serves reads while what it
    /// holds reaches the primary's last.
    pub fn applied(&mut self, term: u64, behind: bool, global_checkpoint: u64, applied: Result<Applied, ApiError>, db_term: u64) -> Result<pb::ReplicateResponse, ApiError> {
        let replica_term = db_term.max(term);
        self.serves_reads = matches!(applied, Ok(Applied::At(_))) && !behind && term >= self.asked_term;
        Ok(match applied? {
            Applied::At(at) => {
                self.global_checkpoint = global_checkpoint.min(at.seq);
                pb::ReplicateResponse { matched: true, seq: at.seq, term: at.term, replica_term, ..Default::default() }
            }
            Applied::Mismatch { seq, term } => pb::ReplicateResponse { matched: false, seq, term, replica_term, ..Default::default() },
        })
    }

    /// Whether call `r`, which it took ([`Self::take`]), at `n`, follows
    /// the transaction it holds under `r.prev_term` and `r.prev_seq`, when
    /// the call carries the primary's digest of its own
    /// (`prev_digest`, [`crate::grpc::SAME_TERM_PROTOCOL`]) and this
    /// replica's log still holds that one: `mine` reads the log from that
    /// number (the transaction there and on), and `digest` is [`digest`]
    /// (the simulator's model has its own). The answer to send instead of
    /// applying the call when the digests differ: the two copies hold
    /// different transactions under one term and number (a writer of a
    /// primary that stepped down, committing after it applied its new
    /// primary's first transaction, made one so before the term check
    /// under the writer lock), and a batch of the primary's would build on
    /// the other. It then serves no reads, and its primary, told so
    /// (`differs`), sends it a copy of itself, which drops its own
    /// ([`Primary::answered`]). It keeps that it differs ([`Kept::differs`],
    /// returned to keep before answering), and refuses every call that
    /// carries a digest until it installs a copy. `None`: apply the call as
    /// before, comparing terms and numbers only.
    pub fn follows(&mut self, r: &pb::ReplicateRequest, n: &Numbering, mine: impl FnOnce() -> LogRead, digest: &dyn Fn(&[u8]) -> u64) -> Option<(pb::ReplicateResponse, Option<Kept>)> {
        let prev = SeqNo { term: r.prev_term, seq: r.prev_seq };
        let last = n.last();
        let refused = pb::ReplicateResponse { matched: false, seq: last.seq, term: last.term, replica_term: n.term.max(r.term), differs: true };
        if r.prev_digest == 0 {
            return None;
        }
        if self.kept.differs.is_some() {
            self.serves_reads = false;
            self.mismatches += 1;
            return Some((refused, None));
        }
        if prev.seq == 0 || prev.seq > n.seq || n.term_at(prev.seq) != prev.term {
            return None;
        }
        let own = held_digest(mine(), prev, digest);
        if own == 0 || own == r.prev_digest {
            return None;
        }
        self.serves_reads = false;
        self.mismatches += 1;
        tracing::error!(
            primary = %r.primary,
            seq_no = prev.seq,
            term = prev.term,
            "this replica holds another transaction than its primary's under one term and number: it applies nothing of the call and waits for a copy of the primary"
        );
        Some((refused, Some(Kept { differs: Some(prev), ..self.kept.clone() })))
    }

    /// It takes a copy of its primary: no reads until the copy is in and a
    /// call finds it caught up.
    pub fn copying(&mut self) {
        self.serves_reads = false;
    }

    /// The answer to a `Restore` whose copy it installed, which holds up
    /// to `at`: `term` is the primary's, `db_term` the database's after.
    /// The primary wrote on meanwhile: its next call says whether this
    /// replica reaches its last.
    /// It no longer differs from a primary ([`Kept::differs`]): what to
    /// keep then comes back too.
    pub fn installed(&mut self, at: SeqNo, term: u64, db_term: u64) -> (pb::ReplicateResponse, Option<Kept>) {
        self.serves_reads = false;
        let save = self.kept.differs.is_some().then(|| Kept { differs: None, ..self.kept.clone() });
        (pb::ReplicateResponse { matched: true, seq: at.seq, term: at.term, replica_term: db_term.max(term), ..Default::default() }, save)
    }

    /// Whether a read may take a new snapshot here: once a call from its
    /// primary since it started reached the primary's last transaction,
    /// until a call finds it behind, it fails to apply one or it installs
    /// a copy. Refused otherwise (503), as it is catching up.
    pub fn readable(&self) -> Result<(), ApiError> {
        if self.serves_reads {
            return Ok(());
        }
        let why = match self.called_at {
            None => "its primary has not called since it started",
            Some(_) => "it is catching up with its primary",
        };
        Err(unavailable(format!("this replica serves no reads while {why}: read from another copy or the primary")))
    }

    /// An election of this replica at `n`, for the term after the newest
    /// it knows or heard of; a `handover` is a promotion asked by hand.
    pub fn campaign(&self, n: &Numbering, handover: bool) -> Campaign {
        Campaign { id: self.kept.id.clone(), term: self.kept.newest(n.term).max(self.heard) + 1, handover, config: self.kept.config_version(), copies: self.kept.config_copies() }
    }

    /// The copies answered a round of its campaign `t`: the newest term
    /// one said it knows counts for its next campaign. A term in which a
    /// minority voted for a candidate that lost is never begun, and the
    /// copies that did not vote in it never learn it from a primary: asked
    /// for it again, the copies that voted would refuse it every time, and
    /// the copies holding the most could never be elected.
    pub fn heard(&mut self, t: &Tally) {
        self.heard = self.heard.max(t.newer);
    }

    /// Begins the term it won (`won`, [`Campaign`]), or, when none, a term
    /// above every term it knows: the term to begin
    /// ([`chloedb::storage::Database::begin_term`]) and what to keep then,
    /// as the primary shipping to `replicas` under `acks`. Refused (409)
    /// when it learned of a newer term than the one it won.
    pub fn take_over(&self, won: Option<u64>, n: &Numbering, replicas: &[String], acks: Acks) -> Result<(u64, Kept), ApiError> {
        let kept = &self.kept;
        let newest = kept.newest(n.term);
        let term = match won {
            Some(t) if newest > t || kept.voted.as_ref().is_none_or(|(vt, c)| *vt != t || *c != kept.id) => {
                return Err(conflict(format!("this replica won term {t}, but learned of term {newest} since: another copy was elected")));
            }
            Some(t) => t,
            None => newest + 1,
        };
        let next = Kept { id: kept.id.clone(), role: Some(KeptRole::Primary { replicas: replicas.to_vec(), acks }), voted: kept.voted.clone(), member: true, config: kept.config, ..Kept::default() };
        Ok((term, next))
    }
}

/// What the copies said when a replica is promoted by hand
/// ([`promote_check`]).
pub struct Asked {
    /// Each copy asked, as the answer shows it.
    pub copies: Vec<Json>,
    /// The copies that said they serve as the shard's primary.
    pub primaries: Vec<String>,
}

/// Whether a replica at `mine` may be promoted by hand, given what each of
/// the shard's copies `checked` answered to `Hello`: refused (409) while a
/// copy holds a transaction past its last, or when it knows no copy to
/// ask, unless `force`.
pub fn promote_check(mine: &Numbering, said: &[(String, Result<pb::HelloResponse, String>)], force: bool) -> Result<Asked, ApiError> {
    // Its last transaction; when it holds none (its rows came in a copy),
    // its term, as a copy that holds none is taken.
    let last = match mine.seq {
        0 => (mine.term, 0),
        seq => (mine.last().term, seq),
    };
    let mut copies = Vec::new();
    let mut ahead = Vec::new();
    let mut primaries = Vec::new();
    for (url, a) in said {
        copies.push(match a {
            Ok(h) => {
                if h.role == pb::Role::Primary as i32 && h.replaced_by_term == 0 {
                    primaries.push(url.clone());
                }
                // By its last transaction: a copy that began or follows a
                // newer term but holds nothing of it holds nothing more.
                let last_term = if h.last_term > 0 { h.last_term } else { h.term };
                if (last_term, h.seq_no) > last {
                    ahead.push(format!("{url} holds transactions up to {} (term {last_term})", h.seq_no));
                }
                json!({"url": url, "role": h.role().as_str_name().to_lowercase(), "term": h.term, "seq_no": h.seq_no})
            }
            Err(e) => json!({"url": url, "error": e}),
        });
    }
    if said.is_empty() && !force {
        return Err(conflict("this replica knows no other copy of its shard to ask for votes (it has no cluster map): name its shard's other servers as its replicas, or pass force".into()));
    }
    if !ahead.is_empty() && !force {
        return Err(conflict(format!(
            "{}, past this replica's last ({} of term {}): the new primary would not hold those transactions, and a copy of it would drop them. Promote the copy with the latest transaction, or pass force",
            ahead.join("; "),
            last.1,
            last.0
        )));
    }
    Ok(Asked { copies, primaries })
}

/// How long a replica promoted by hand (`elected` in a handover, after the
/// copies said `asked`; no election with `force`) waits before it takes writes,
/// when primaries hold a `lease` on reads: the primary it replaced serves
/// reads until it learns of the new term or its lease runs out, and it
/// would serve them without what the new primary acknowledges. None when
/// that primary voted: it stepped down then.
///
/// Only a handover needs it: in an election on its own, a copy votes once
/// its primary has been silent for longer than that primary's lease
/// (`--failover-after-ms`, of which the lease is three quarters), and the
/// copies whose votes won were not counted toward it since. A copy voting
/// in a handover may have answered its primary a moment before, so the
/// winner waits as long: the lease, and a third more for the servers'
/// clocks, as the copies' failover delay.
pub fn handover_hold(asked: Option<&Asked>, elected: Option<&Elected>, lease: Duration) -> Duration {
    let stepped_down = asked.zip(elected).is_some_and(|(a, e)| {
        let granted = |url: &String| e.votes.as_array().is_some_and(|v| v.iter().any(|j| j["url"] == json!(url) && j["granted"] == json!(true)));
        !a.primaries.is_empty() && a.primaries.iter().all(granted)
    });
    match stepped_down || lease.is_zero() {
        true => Duration::ZERO,
        false => lease + lease / 3,
    }
}

/// A replica asking its shard's copies to make it the primary of `term`:
/// first whether they would (`pre`), which changes nothing, then for their
/// votes, keeping its own. It wins with a majority of the copies, itself
/// counted once, and with a majority of the copies of the newest
/// membership it knows ([`Kept::config`]), when that counts more.
#[derive(Clone, Debug)]
pub struct Campaign {
    pub id: String,
    pub term: u64,
    pub handover: bool,
    /// The membership it knows: version and copies (0: none).
    pub config: u64,
    pub copies: usize,
}

/// What the copies answered a round of a [`Campaign`].
#[derive(Clone, Debug)]
pub struct Tally {
    /// The copies besides the candidate.
    pub others: Vec<String>,
    /// Votes, the candidate's own among them, and how many win.
    pub granted: usize,
    pub need: usize,
    /// The newest term a copy said it knows.
    pub newer: u64,
    /// The newest last transaction a copy said it holds, as (term, number).
    pub last: (u64, u64),
    /// What each copy answered.
    pub said: Json,
}

impl Campaign {
    /// The vote asked, by a candidate holding `last`.
    pub fn ask(&self, last: SeqNo, pre: bool) -> pb::VoteRequest {
        pb::VoteRequest { term: self.term, candidate: self.id.clone(), last_seq: last.seq, last_term: last.term, pre, handover: self.handover, config: self.config, copies: self.copies as u32 }
    }

    /// The votes of `copies`, which answered `answers` in order (an error:
    /// no answer). The candidate among them answers as such, and is not
    /// counted again.
    pub fn tally(&self, copies: &[String], answers: Vec<Result<pb::VoteResponse, String>>) -> Tally {
        let mut others = Vec::new();
        let (mut granted, mut newer, mut last, mut said) = (1, 0, (0, 0), Vec::new());
        for (url, a) in copies.iter().zip(answers) {
            match a {
                Ok(a) if a.candidate => {}
                Ok(a) => {
                    others.push(url.clone());
                    granted += a.granted as usize;
                    newer = newer.max(a.term);
                    last = last.max((a.last_term, a.last_seq));
                    let mut j = json!({"url": url, "granted": a.granted, "term": a.term, "seq_no": a.last_seq});
                    if !a.reason.is_empty() {
                        j["reason"] = json!(a.reason);
                    }
                    said.push(j);
                }
                Err(e) => {
                    others.push(url.clone());
                    said.push(json!({"url": url, "granted": false, "error": e}));
                }
            }
        }
        let need = self.need(others.len() + 1);
        Tally { others, granted, need, newer, last, said: Json::Array(said) }
    }

    /// Whether the answers a round has so far (`None`: not yet) decide
    /// it, so that an election waits for no copy it does not need: the
    /// candidate answered (which tells the copies besides it), and a
    /// majority granted or too few are left to. A copy that is down, cut
    /// off or paused answers at the call's timeout; while its campaign
    /// waits for one, the copy that is winning is still a replica, grants
    /// another copy's pre-vote, and that copy's newer term, kept, fences
    /// it once it is elected.
    pub fn decided(&self, answers: &[Option<Result<pb::VoteResponse, String>>]) -> bool {
        let me = answers.iter().filter(|a| matches!(a, Some(Ok(a)) if a.candidate)).count();
        if me != 1 {
            return answers.iter().all(Option::is_some);
        }
        // As `tally` counts: the copies besides the candidate, and itself.
        let need = self.need(answers.len());
        let granted = 1 + answers.iter().filter(|a| matches!(a, Some(Ok(a)) if a.granted && !a.candidate)).count();
        let left = answers.iter().filter(|a| a.is_none()).count();
        granted >= need || granted + left < need
    }

    /// The votes that win among `asked` copies, itself counted: a majority
    /// of them, and of the membership it knows. A copy the map lacks but
    /// the membership counts (a replica added while the map was not given
    /// to this one) cannot be asked, so its vote is missing, never assumed.
    fn need(&self, asked: usize) -> usize {
        (asked / 2 + 1).max(self.copies / 2 + 1)
    }

    fn lost(&self, round: &str, t: &Tally) -> ApiError {
        conflict(format!(
            "{} of {} copies {round} for this replica to be the primary of term {}, and it needs {}: {}",
            t.granted,
            t.said.as_array().map_or(0, Vec::len) + 1,
            self.term,
            t.need,
            t.said
        ))
    }

    /// After the round that asks whether they would vote: refused (409)
    /// when a copy knows the term asked for or a newer one, or fewer than
    /// a majority would.
    pub fn pre_voted(&self, t: &Tally) -> Result<(), ApiError> {
        if t.newer >= self.term {
            return Err(conflict(format!("a copy knows term {}, not older than the term this replica would ask for ({}): {}", t.newer, self.term, t.said)));
        }
        if t.granted < t.need {
            return Err(self.lost("would vote", t));
        }
        Ok(())
    }

    /// Its own vote, kept before it asks for the others': refused (409)
    /// when it learned of its term or a newer one meanwhile.
    pub fn vote_self(&self, r: &Replica, n: &Numbering) -> Result<Kept, ApiError> {
        if r.kept.newest(n.term) >= self.term {
            return Err(conflict(format!("this replica learned of term {} or newer while it asked: another copy was elected first", self.term)));
        }
        // Its history is not its primary's ([`Kept::differs`]): elected, it
        // would ship it to the copies that hold the primary's.
        if let Some(at) = r.kept.differs {
            return Err(conflict(format!("this replica holds another transaction than its primary's as transaction {} of term {}, and waits for a copy of a primary: elect another copy", at.seq, at.term)));
        }
        Ok(Kept { voted: Some((self.term, self.id.clone())), ..r.kept.clone() })
    }

    /// After the round that asks for votes: won with a majority. The term
    /// stays voted in when it loses: copies that granted it refuse the
    /// primary of an older term from then.
    pub fn voted(&self, t: Tally) -> Result<Elected, ApiError> {
        if t.granted < t.need {
            return Err(self.lost("voted", &t));
        }
        Ok(Elected { term: self.term, others: t.others, votes: t.said })
    }
}

/// When a replica whose primary is silent asks to be elected
/// (`--failover-after-ms`, [`crate::service::Service::start_failover`]):
/// after `after` of silence, and not before `next`, which each election
/// it starts moves on by half the delay and a random part of it more, so
/// that two replicas seldom ask at once.
#[derive(Clone, Debug)]
pub struct Failover {
    pub after: Duration,
    pub next: Instant,
}

impl Failover {
    /// From `now`, the first election not before a random part of the
    /// delay (`jitter`, at most `after`).
    pub fn new(after: Duration, now: Instant, jitter: Duration) -> Failover {
        Failover { after, next: now + jitter }
    }

    /// How often it looks.
    pub fn every(&self) -> Duration {
        (self.after / 8).max(Duration::from_millis(10))
    }

    /// Whether a replica whose shard has `copies` copies, and whose
    /// primary is `silent` for that long, asks now.
    pub fn due(&self, copies: usize, silent: Duration, now: Instant) -> bool {
        copies >= 2 && silent >= self.after && now >= self.next
    }

    /// The look before this one was at `last`. A copy that did not run
    /// for longer than the delay itself (its process stopped, or its
    /// machine starved it) heard nothing because it was not listening, not
    /// because its primary was silent: it asks not before a delay and
    /// `jitter` from `now`, by when a primary that serves has called.
    pub fn resumed(&mut self, last: Instant, now: Instant, jitter: Duration) {
        if now.saturating_duration_since(last) > self.after {
            self.next = self.next.max(now + self.after + jitter);
        }
    }

    /// An election starts at `now`: the next not before half the delay
    /// and `jitter` more.
    pub fn started(&mut self, now: Instant, jitter: Duration) {
        self.next = now + self.after / 2 + jitter;
    }

    /// How long a copy has to answer an election: a copy that does not
    /// answer within the delay is taken for down.
    pub fn call_timeout(&self, call_timeout: Duration) -> Duration {
        call_timeout.min(self.after.max(Duration::from_millis(200)))
    }
}

/// The gRPC metadata key of a write's outcome ([`Primary::write_outcome`]),
/// from protocol 19.
pub const WRITE_OUTCOME: &str = "x-chloedb-write";
/// The write was not applied, and the shard takes it shortly.
pub const NOT_APPLIED: &str = "not-applied";
/// The write was applied, and a new primary may or may not hold it.
pub const IN_DOUBT: &str = "in-doubt";

/// How a primary ships, as [`PrimaryOptions`] says.
#[derive(Clone, Debug)]
pub struct Opts {
    pub acks: Acks,
    pub heartbeat: Duration,
    pub retry_max: Duration,
    pub copy_retry: Duration,
    pub lease: Duration,
}

impl Opts {
    pub fn of(o: &PrimaryOptions) -> Opts {
        Opts { acks: o.acks, heartbeat: o.heartbeat, retry_max: o.retry_max, copy_retry: o.copy_retry, lease: o.lease }
    }
}

/// A replica, as its primary sees it.
#[derive(Clone, Debug)]
pub struct Peer {
    pub url: String,
    /// The last number it holds, as it last said.
    pub acked: u64,
    pub in_sync: bool,
    /// Why it is out of sync, or why its last call failed.
    pub error: Option<String>,
    /// Whether it counts among the copies a majority is counted of: from
    /// the start, or once it caught up after joining.
    pub member: bool,
    /// When the last call it answered started, for the primary's lease.
    pub answered: Option<Instant>,
    /// Whether it is being sent a copy of the primary.
    pub copying: bool,
    /// Whether its rows were found to differ, until its shipper takes that
    /// up and sends it a copy.
    pub diverged: bool,
    /// Transactions it applied from this primary, calls that failed, and
    /// copies it installed.
    pub shipped: u64,
    pub failures: u64,
    pub copies: u64,
    /// The shipper that ships to it ([`Shipper::gen`]).
    pub owner: u64,
    /// The version of the membership it took a call of ([`Kept::config`]).
    pub config: u64,
    /// Calls it refused for holding another transaction than this
    /// primary's under the term and number a call followed
    /// ([`Replica::follows`]).
    pub mismatches: u64,
}

impl Peer {
    fn new(url: &str, joining: bool) -> Peer {
        let mut p = Peer {
            url: url.to_string(),
            acked: 0,
            in_sync: true,
            error: None,
            member: true,
            answered: None,
            copying: false,
            diverged: false,
            shipped: 0,
            failures: 0,
            copies: 0,
            owner: 0,
            config: 0,
            mismatches: 0,
        };
        // A replica that joins a serving primary: out of sync, so that
        // writes do not wait for it, until it has installed a copy of the
        // primary and caught up from there.
        if joining {
            p.error = Some("it joined: it is sent a copy of the primary first".into());
            p.in_sync = false;
            p.diverged = true;
            p.member = false;
        }
        p
    }

    /// Takes it out of sync for `why`, logging the change.
    fn drop_out(&mut self, why: String) {
        let was = std::mem::replace(&mut self.in_sync, false);
        if was || self.error.as_deref() != Some(&why) {
            tracing::warn!(replica = %self.url, error = %why, "a replica is out of sync");
        }
        self.error = Some(why);
    }

    /// Puts it in sync: it holds the primary's last transaction. A replica
    /// that joined counts among the copies from then, once a call it
    /// answered says so (`shipped`): not on installing a copy, whose call
    /// started too long ago to count toward the primary's lease on reads.
    fn caught_up(&mut self, shipped: bool) {
        if shipped {
            self.member = true;
        }
        if !std::mem::replace(&mut self.in_sync, true) {
            tracing::info!(replica = %self.url, seq_no = self.acked, "a replica is in sync");
        }
        self.error = None;
    }
}

/// What a shipper does next ([`Primary::poll`]).
#[derive(Debug)]
pub enum Ship {
    /// Poll again at once.
    Again,
    /// Wait this long, then poll.
    Sleep(Duration),
    /// Wait until the primary commits, the shipper is woken, or this long
    /// passes (a heartbeat), then poll.
    Idle(Duration),
    /// Read the log from `from` on ([`chloedb::storage::Database::log_from`])
    /// and give it to [`Primary::read`].
    Read { from: u64 },
    /// Call the replica (`Replicate`) and give its answer to
    /// [`Primary::answered`]. A call that pins the version a round compares
    /// at (`pin`) is followed by the round's `Verify` once answered
    /// ([`Shipper::take_pinned`]).
    Call(pb::ReplicateRequest),
    /// Hold the log from `hold_from` on ([`Shipper::holds`]), send the
    /// replica a copy of the database (`Restore`) under `term`, and give
    /// its answer to [`Primary::copied`].
    Copy { hold_from: u64, term: u64 },
    /// Another primary of this database ships to the replica now.
    Stop,
}

/// What a call under way was sent at.
#[derive(Clone, Debug)]
struct Sent {
    numbering: Numbering,
    /// The last transaction sent; `None` for a copy.
    end: Option<u64>,
    /// Transactions sent.
    n: u64,
    started: Instant,
    /// Whether the call pins the version of the round under way.
    pin: bool,
    /// The membership version it names (0: a copy, which names none).
    config: u64,
    /// The transaction it follows (a copy: none, at 0).
    prev: SeqNo,
}

/// One task's shipping to a replica: the next number to send (`None`
/// until the replica said its last), whether it must be sent a copy, the
/// wait before calling again after a failure, whether the log is held for
/// it, and the round comparing its rows under way. The task holds it; the
/// [`Primary`] holds what is shared.
#[derive(Clone, Debug)]
pub struct Shipper {
    /// The [`Primary`] generation it ships for ([`Peer::owner`]), and the
    /// replica it ships to.
    pub gen: u64,
    url: String,
    next: Option<u64>,
    must_copy: bool,
    retry: Duration,
    holding: bool,
    from: u64,
    sent: Option<Sent>,
    /// The transaction a round comparing the replica's rows with the
    /// primary's compares at, until the call that pins it there; and
    /// whether that call was answered, until the driver takes it up.
    round: Option<u64>,
    pinned: bool,
    /// Whether `next` follows the last transaction the replica said it
    /// holds (a call it did not apply), not one a call it applied ended
    /// at: the call that follows it must carry the primary's digest of it
    /// ([`Replica::follows`]), and a log that no longer holds it sends a
    /// copy instead.
    claimed: bool,
}

impl Shipper {
    /// Whether the log is held from the copy it sent on: the driver
    /// releases its hold when it is not.
    pub fn holds(&self) -> bool {
        self.holding
    }

    /// Whether the call under way sends a copy of the primary
    /// ([`Ship::Copy`]): its answer goes to [`Primary::copied`].
    pub fn sending_copy(&self) -> bool {
        self.sent.as_ref().is_some_and(|s| s.end.is_none())
    }

    /// A round comparing the replica's rows with the primary's starts at
    /// `seq`, the last transaction of the version the driver took: when the
    /// replica was shipped no further than it ([`Primary::may_verify`]
    /// first). The replica is then shipped up to it, and not past it, in a
    /// call that has it keep the version it leaves (`pin`). Returns whether
    /// it started.
    pub fn verify_at(&mut self, seq: u64) -> bool {
        let starts = self.round.is_none() && self.next.is_some_and(|n| n <= seq + 1);
        if starts {
            self.round = Some(seq);
        }
        starts
    }

    /// The transaction the round under way compares at: once none, the
    /// driver lets go of the round's version.
    pub fn round(&self) -> Option<u64> {
        self.round
    }

    /// Whether the call that pinned the round's version was answered since
    /// last asked: the driver then compares the two versions' digests
    /// (`Verify`), and tells [`Primary::diverged`] when they differ.
    pub fn take_pinned(&mut self) -> bool {
        std::mem::take(&mut self.pinned)
    }
}

/// A primary: the replicas it ships to, whether another primary replaced
/// it, and what it keeps beside its database.
#[derive(Clone, Debug)]
pub struct Primary {
    pub id: String,
    pub opts: Opts,
    pub peers: Vec<Peer>,
    /// A newer term a replica holds (0: none): another primary replaced
    /// this one.
    pub fenced: u64,
    /// What it keeps beside its database, as it last kept it.
    pub kept: Kept,
    /// Changed whenever a replica acknowledges or drops out: writes
    /// waiting for acknowledgments look again.
    pub progress: u64,
    /// It takes no writes before then ([`handover_hold`]).
    pub writes_from: Option<Instant>,
    /// It takes no writes until then, as a copy asks it to hand the shard
    /// over ([`Self::hand_over`]), unless it answered that copy's vote first.
    pub handing_over: Option<Instant>,
    /// The number of the first transaction of its term, which it commits
    /// as it begins the term ([`crate::replication::term_opened`]; 0: none,
    /// the shard's first primary, or one started again in its term). No
    /// transaction is answered as acknowledged until a majority holds
    /// this one, as Raft's leader counts no entry of an older term as
    /// committed until one of its own is (finding DS-5).
    pub term_start: u64,
    gen: u64,
}

impl Primary {
    /// A primary keeping `kept` (whose id is its own) and shipping to
    /// `replicas`, and what to keep when its part changes. Every replica
    /// counts as in sync until it fails, so that the first writes wait for
    /// replicas that are there, but those `joining`, which are sent a copy
    /// of the primary first.
    pub fn new(kept: Kept, replicas: &[String], opts: Opts, joining: bool) -> (Primary, Option<Kept>) {
        // A primary counts among its shard's copies, as a replica once it
        // steps down or starts again.
        // A primary started before memberships were numbered numbers its
        // own the first.
        let config = kept.config.or(Some((1, 1 + replicas.len() as u64)));
        let next = Kept { role: Some(KeptRole::Primary { replicas: replicas.to_vec(), acks: opts.acks }), member: true, config, ..kept.clone() };
        let save = (next != kept).then_some(next);
        let kept = save.clone().unwrap_or(kept);
        let mut p = Primary { id: kept.id.clone(), opts, peers: replicas.iter().map(|u| Peer::new(u, joining)).collect(), fenced: 0, kept, progress: 0, writes_from: None, handing_over: None, term_start: 0, gen: 0 };
        p.gen = 1;
        for peer in &mut p.peers {
            peer.owner = p.gen;
        }
        (p, save)
    }

    /// A shipper for each replica, by index.
    pub fn shippers(&self) -> Vec<Shipper> {
        self.peers
            .iter()
            .map(|p| Shipper { gen: self.gen, url: p.url.clone(), next: None, must_copy: false, retry: Duration::from_millis(50), holding: false, from: 0, sent: None, round: None, pinned: false, claimed: false })
            .collect()
    }

    /// The copies a majority is counted of: the primary, and the replicas
    /// but those that joined and have not caught up yet.
    pub fn copies(&self) -> usize {
        1 + self.peers.iter().filter(|p| p.member).count()
    }

    pub fn majority(&self) -> usize {
        self.copies() / 2 + 1
    }

    /// The highest number every in-sync replica holds (`last`, the
    /// primary's, while none is in sync).
    pub fn global_checkpoint(&self, last: u64) -> u64 {
        self.peers.iter().filter(|p| p.in_sync).map(|p| p.acked).min().unwrap_or(last).min(last)
    }

    fn changed(&mut self) {
        self.progress += 1;
    }

    /// The newer term of a primary that replaced this one (0: none).
    pub fn replaced_by(&self) -> u64 {
        self.fenced
    }

    /// Takes no more writes and serves no reads: a primary of `term`,
    /// newer than this one's, replaced it. Returns what to keep (it starts
    /// again as a replica) the first time.
    pub fn fence(&mut self, term: u64, mine: u64) -> Option<Kept> {
        if self.fenced >= term {
            return None;
        }
        self.fenced = term;
        tracing::error!(term = mine, replaced_by = term, "a primary of a newer term replaced this one, which takes no more writes and serves no reads");
        self.kept = Kept { id: self.id.clone(), follows: None, replaced_by: Some(term), role: Some(KeptRole::Replica), voted: self.kept.voted.clone(), member: true, config: self.kept.config, differs: None };
        self.changed();
        Some(self.kept.clone())
    }

    /// Refuses a read at `now` once another primary replaced this one, of
    /// term `term` (503), and, with a lease, while fewer than a majority of
    /// the copies answered a call started within it.
    pub fn readable(&self, now: Instant, term: u64) -> Result<(), ApiError> {
        if let t @ 1.. = self.fenced {
            return Err(unavailable(format!("a primary of term {t} replaced this one (term {term}), which serves no reads: read from another copy")));
        }
        let lease = self.opts.lease;
        if !lease.is_zero() && !self.peers.is_empty() {
            let fresh = |p: &Peer| p.answered.is_some_and(|at| now.saturating_duration_since(at) < lease);
            let up = 1 + self.peers.iter().filter(|p| p.member && fresh(p)).count();
            if up < self.majority() {
                return Err(unavailable(format!(
                    "{up} of this primary's {} copies answered it within {} ms, fewer than a majority: the others may have elected another primary, so it serves no reads; read from another copy",
                    self.copies(),
                    lease.as_millis()
                )));
            }
        }
        Ok(())
    }

    /// Answers a primary's call to ship to this one, of `term` and
    /// numbering the shard's membership `config`, while this one is at
    /// `n`: an older term is answered with this one's (which fences that
    /// primary), one of an older membership is refused (409,
    /// [`stale_membership`]), a newer term fences this primary and is
    /// refused (409), and so is one of the same term. Returns what to keep
    /// when fenced.
    pub fn shipped_to(&mut self, primary: &str, term: u64, config: u64, n: &Numbering) -> (Result<pb::ReplicateResponse, ApiError>, Option<Kept>) {
        let (mine, last) = (n.term, n.last());
        if term < mine {
            return (Ok(pb::ReplicateResponse { matched: false, seq: last.seq, term: last.term, replica_term: mine, ..Default::default() }), None);
        }
        // A primary of copies no longer all the shard's does not fence
        // this one, however new its term (finding E-1).
        if let Err(e) = stale_membership(&self.kept, primary, term, config) {
            return (Err(e), None);
        }
        if term > mine {
            let kept = self.fence(term, mine);
            return (
                Err(conflict(format!(
                    "this server is the primary of term {mine}, and primary {primary} of term {term} ships to it: that one replaced this one, which takes no more writes; start this one with --replica"
                ))),
                kept,
            );
        }
        if primary == self.id {
            return (Err(conflict(format!("this server is primary {primary} of term {term}, not a replica: is its own URL among its replicas?"))), None);
        }
        (Err(conflict(format!("this server is the primary of term {term}, not a replica, and primary {primary} ships to it in the same term: a shard has one primary in a term"))), None)
    }

    /// Answers vote `r` at `n`, and what to keep (the vote) before
    /// answering; once kept, [`Self::gave_vote`] steps it down. One that
    /// serves grants only a handover, and only to a copy holding every
    /// transaction it holds. A primary another replaced answers as a
    /// replica would.
    pub fn vote(&self, r: &pb::VoteRequest, n: &Numbering) -> (pb::VoteResponse, Option<Kept>) {
        let kept = Kept { id: self.id.clone(), member: true, ..self.kept.clone() };
        if self.fenced == 0 && !r.handover && r.candidate != self.id {
            return (
                refused(kept.newest(n.term), n.last(), format!("this server is the shard's primary (term {}) and ships to its replicas: promote another copy by hand to hand over", n.term)),
                None,
            );
        }
        let a = decide(&kept, n.term, n.last(), r);
        if !a.granted || r.pre || a.candidate {
            return (a, None);
        }
        (a, Some(Kept { voted: Some((r.term, r.candidate.clone())), ..kept }))
    }

    /// A copy asked whether it would vote for it in a handover (a
    /// promotion by hand): it takes no writes until `until`, so that the
    /// copy can catch up with it before it asks for the vote, which a copy
    /// missing a transaction it holds is refused. False when another
    /// primary replaced it, which takes no writes anyway.
    pub fn hand_over(&mut self, until: Instant) -> bool {
        if self.fenced > 0 {
            return false;
        }
        self.handing_over = Some(until);
        true
    }

    /// It answered handover vote `r`, at `n`, with `a`: it takes writes
    /// again, unless it granted it or refused a copy only behind it, which
    /// catches up and asks again. A vote granted, not a question, steps
    /// it down ([`Self::gave_vote`]).
    pub fn answered_handover(&mut self, r: &pb::VoteRequest, n: &Numbering, a: &pb::VoteResponse) {
        let last = n.last();
        let behind = (r.last_term, r.last_seq) < (last.term, last.seq);
        if !r.pre || !(a.granted || behind) {
            self.handing_over = None;
        }
    }

    /// It kept `kept`, its vote for another copy in term `term`: it steps
    /// down, as a primary the new one replaced. Returns what to keep then.
    pub fn gave_vote(&mut self, kept: Kept, term: u64, mine: u64) -> Option<Kept> {
        tracing::warn!(term, "voted for another copy to be the shard's primary: this one steps down");
        self.kept = kept;
        self.fence(term, mine)
    }

    /// Whether a write may be applied at term `term` at `now`: not on a
    /// primary another replaced (409), not before [`Self::writes_from`]
    /// nor while it hands the shard over ([`Self::hand_over`]) (503), and,
    /// unless only the primary acknowledges, while a majority of copies is
    /// in sync (503).
    pub fn admit(&self, term: u64, now: Instant) -> Result<(), ApiError> {
        if self.fenced > 0 {
            return Err(conflict(format!("a copy holds term {}, newer than this primary's ({term}): another primary replaced this one, which takes no more writes", self.fenced)));
        }
        if let Some(from) = self.writes_from.filter(|&f| f > now) {
            return Err(unavailable(format!(
                "this primary was promoted by hand, and the primary it replaced may serve reads for {} ms more: it takes writes after that",
                (from - now).as_millis()
            )));
        }
        if self.handing_over.is_some_and(|until| until > now) {
            return Err(unavailable("this primary is handing its shard over to another copy, which takes the writes once elected: the write was not applied".into()));
        }
        if self.opts.acks != Acks::Primary {
            let up = 1 + self.peers.iter().filter(|p| p.member && p.in_sync).count();
            if up < self.majority() {
                return Err(unavailable(format!("{up} of {} copies are in sync, and --write-acks {} needs {}: the write was not applied", self.copies(), self.opts.acks.name(), self.majority())));
            }
        }
        Ok(())
    }

    /// How many copies hold transaction `seq`, and whether a majority
    /// does: counted at the term's first transaction when `seq` is older
    /// ([`Self::term_start`]), so that a retry answered by an older
    /// transaction waits for one of this term.
    /// What a primary's refusal of a write `e` says of it, as the code its
    /// gRPC answer carries under [`WRITE_OUTCOME`], which a coordinator
    /// reads instead of the message (finding DS-6: retry safety hung on
    /// substrings of English messages of another server, which a rolling
    /// upgrade could reword): [`NOT_APPLIED`] when [`Self::admit`] refused
    /// it before applying it (sending it again cannot apply it twice), and
    /// [`IN_DOUBT`] when [`Self::acked`] found the primary replaced after
    /// it applied it. The messages are this file's own, and its tests
    /// check that each is classified.
    pub fn write_outcome(e: &ApiError) -> Option<&'static str> {
        let m = &e.message;
        match e.status {
            409 if m.contains("before it was acknowledged") => Some(IN_DOUBT),
            409 if m.contains("another primary replaced this one, which takes no more writes") => Some(NOT_APPLIED),
            503 if m.contains("the write was not applied") || m.contains("it takes writes after that") => Some(NOT_APPLIED),
            _ => None,
        }
    }

    fn holding(&self, seq: u64) -> (usize, bool) {
        let seq = seq.max(self.term_start);
        let holding = 1 + self.peers.iter().filter(|p| p.member && p.acked >= seq).count();
        (holding, holding >= self.majority())
    }

    /// Whether transaction `seq` may be answered now: `None` while it
    /// waits for the copies `--write-acks` asks for, an error once another
    /// primary replaced this one.
    pub fn acked(&self, seq: u64) -> Option<Result<(), ApiError>> {
        // Its replicas follow the new primary, which may not hold it.
        if let t @ 1.. = self.fenced {
            return Some(Err(conflict(format!(
                "the write is durable on this primary as sequence number {seq}, but a primary of term {t} replaced this one before it was acknowledged, and the new primary may not hold it: write to the new primary"
            ))));
        }
        let (_, majority) = self.holding(seq);
        match self.opts.acks {
            Acks::Primary => Some(Ok(())),
            Acks::All if majority && self.peers.iter().all(|p| !p.in_sync || p.acked >= seq) => Some(Ok(())),
            Acks::Majority if majority => Some(Ok(())),
            _ => None,
        }
    }

    /// The deadline of transaction `seq` passed while it waited
    /// ([`Self::acked`]): with `all`, the replicas in sync that do not hold
    /// it drop out and it is answered, but never with fewer than a
    /// majority holding it (a write fewer copies hold could be lost to an
    /// election); otherwise it is answered 504.
    pub fn deadline(&mut self, seq: u64) -> Result<(), ApiError> {
        let (holding, majority) = self.holding(seq);
        if self.opts.acks == Acks::All && majority {
            for p in self.peers.iter_mut().filter(|p| p.in_sync && p.acked < seq) {
                p.drop_out(format!("it did not acknowledge transaction {seq} within the write's deadline"));
            }
            self.changed();
            return Ok(());
        }
        Err(ApiError {
            code: None, status: 504,
            message: format!(
                "the write is durable on the primary as sequence number {seq}, but {holding} of {} copies held it by its deadline, and --write-acks {} needs {}",
                self.copies(),
                self.opts.acks.name(),
                self.majority()
            ),
        })
    }

    /// Ships to `replicas` under `acks` from now on: replicas it ships to
    /// already keep what it knows of them, and new ones join (each is sent
    /// a copy first). Each replica's shipper is replaced
    /// ([`Self::shippers`]); those before stop. Refused once another
    /// primary replaced this one. Returns what to keep.
    pub fn reconfigure(&mut self, replicas: Vec<String>, acks: Acks) -> Result<Option<Kept>, ApiError> {
        if let t @ 1.. = self.fenced {
            return Err(conflict(format!("a primary of term {t} replaced this one, which ships to no replica: make it a replica of the new primary")));
        }
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = replicas.iter().find(|u| !seen.insert(u.as_str())) {
            return Err(ApiError::bad(format!("{dup} is named twice")));
        }
        self.gen += 1;
        let mut peers = Vec::new();
        for url in &replicas {
            let mut p = match self.peers.iter().position(|p| p.url == *url) {
                Some(k) => self.peers[k].clone(),
                None => Peer::new(url, true),
            };
            p.owner = self.gen;
            peers.push(p);
        }
        // Another membership, when the copies change: a version more.
        let config = match self.peers.iter().map(|p| &p.url).eq(peers.iter().map(|p| &p.url)) {
            true => self.kept.config,
            false => Some((self.kept.config_version() + 1, 1 + replicas.len() as u64)),
        };
        self.peers = peers;
        self.opts.acks = acks;
        let role = Some(KeptRole::Primary { replicas, acks });
        self.kept = Kept { id: self.id.clone(), voted: self.kept.voted.clone(), role, member: true, config, ..Kept::default() };
        Ok(Some(self.kept.clone()))
    }

    /// Whether it may ship to `replicas` from `now` ([`Self::reconfigure`]),
    /// as Raft changes a membership: one copy added or removed at a time,
    /// by a primary that a majority of its current copies answered within
    /// its lease (none of them can have voted for another since), once a
    /// majority of them holds its current membership (the change before
    /// is complete). Refused (503) otherwise, unless the caller accepts
    /// that writes may be lost (`accept_data_loss`, logged): finding DS-1,
    /// a primary cut off from its majority was given two new replicas, and
    /// it and the primary its old replicas elected both acknowledged
    /// writes, the old one's lost at the heal. Without a lease (no
    /// automatic failover), a call answered within four heartbeats counts.
    pub fn may_reconfigure(&self, replicas: &[String], now: Instant, accept_data_loss: bool) -> Result<(), ApiError> {
        if let t @ 1.. = self.fenced {
            return Err(conflict(format!("a primary of term {t} replaced this one, which ships to no replica: make it a replica of the new primary")));
        }
        let old: std::collections::BTreeSet<&str> = self.peers.iter().map(|p| p.url.as_str()).collect();
        let new: std::collections::BTreeSet<&str> = replicas.iter().map(String::as_str).collect();
        if old == new {
            return Ok(());
        }
        let changed = old.symmetric_difference(&new).count();
        let window = match self.opts.lease.is_zero() {
            true => self.opts.heartbeat * 4,
            false => self.opts.lease,
        };
        let fresh = |p: &Peer| p.answered.is_some_and(|at| now.saturating_duration_since(at) < window);
        let up = 1 + self.peers.iter().filter(|p| p.member && fresh(p)).count();
        let version = self.kept.config_version();
        let hold = 1 + self.peers.iter().filter(|p| p.member && p.config >= version).count();
        let need = self.majority();
        let why = if changed > 1 {
            Some(format!("{changed} copies would change at once, and a shard's copies change one at a time: add or remove one, then the next"))
        } else if up < need {
            Some(format!(
                "{up} of this primary's {} copies answered it within {} ms, fewer than a majority ({need}): the others may have elected another primary, which would go on acknowledging writes beside this one",
                self.copies(),
                window.as_millis()
            ))
        } else if hold < need {
            Some(format!("{hold} of this primary's {} copies hold version {version} of the shard's membership, fewer than a majority ({need}): the change before is not complete yet", self.copies()))
        } else {
            None
        };
        match (why, accept_data_loss) {
            (None, _) => Ok(()),
            (Some(why), true) => {
                tracing::error!(reason = %why, replicas = %replicas.join(", "), "the shard's copies change with accept_data_loss: writes another primary or this one acknowledges may be lost");
                Ok(())
            }
            (Some(why), false) => Err(unavailable(format!("this primary takes no other replicas: {why}; pass accept_data_loss to change them anyway, accepting that acknowledged writes may be lost"))),
        }
    }

    /// Replica `k`, when shipper `s` still ships to it: a primary given
    /// other replicas ([`Self::reconfigure`]) stops the shippers before.
    fn ships_to(&self, k: usize, s: &Shipper) -> Option<usize> {
        self.peers.get(k).filter(|p| p.owner == s.gen && p.url == s.url).map(|_| k)
    }

    /// Whether a round comparing replica `k`'s rows with the primary's may
    /// start ([`Shipper::verify_at`]): none is under way, the replica said
    /// its last and holds a run of this primary's history, it is in sync,
    /// and it is not to be sent a copy.
    pub fn may_verify(&self, k: usize, s: &Shipper) -> bool {
        self.ships_to(k, s).is_some_and(|k| {
            let p = &self.peers[k];
            s.round.is_none() && !s.must_copy && !p.diverged && s.next.is_some() && p.in_sync
        })
    }

    /// A round found the rows of the replica at `url` differ from the
    /// primary's, for `why`: it drops out of sync, and its shipper sends it
    /// a copy of the primary.
    pub fn diverged(&mut self, url: &str, why: String) {
        if let Some(p) = self.peers.iter_mut().find(|p| p.url == url) {
            p.drop_out(why);
            p.diverged = true;
        }
        self.changed();
    }

    /// What replica `k`'s shipper `s` does next, the database at `n`, at
    /// `now`: a replica whose rows differ, or one holding a replaced
    /// primary's transactions, is sent a copy; one past the primary's last
    /// is called with no transactions (a heartbeat); otherwise the log is
    /// read from its next number.
    pub fn poll(&mut self, k: usize, s: &mut Shipper, n: &Numbering, now: Instant) -> Ship {
        let Some(k) = self.ships_to(k, s) else { return Ship::Stop };
        let p = &mut self.peers[k];
        // A shipper of the primary before this one is sending it a copy.
        if p.copying && s.next.is_none() && !s.holding {
            return Ship::Sleep(Duration::from_millis(100));
        }
        if std::mem::take(&mut p.diverged) {
            s.must_copy = true;
            s.round = None;
        }
        let last = n.seq;
        let from = s.next.unwrap_or(last + 1);
        s.from = from;
        // A replica that said a last past the round's version holds rows
        // the version does not.
        if s.round.is_some_and(|r| from > r + 1) {
            s.round = None;
        }
        if s.must_copy {
            return self.copy(k, s, n, now);
        }
        if from > last {
            return self.call(k, s, n, Vec::new(), now);
        }
        // The transaction the call follows too, when only the replica said
        // it holds it.
        match s.claimed && from >= 2 {
            true => Ship::Read { from: from - 1 },
            false => Ship::Read { from },
        }
    }

    /// [`Self::poll`], given what the log holds from the number asked.
    pub fn read(&mut self, k: usize, s: &mut Shipper, n: &Numbering, log: LogRead, now: Instant) -> Ship {
        let log = match log {
            // Read from the transaction the call follows, which the
            // replica said it holds: the call carries its digest, for the
            // replica to compare, or it is sent a copy.
            LogRead::Txns(mut t) if s.claimed && s.from >= 2 => match t.first().is_some_and(|t| t.seq.seq == s.from - 1) {
                true => {
                    t.remove(0);
                    LogRead::Txns(t)
                }
                false => LogRead::Gone { oldest: t.first().map_or(s.from, |t| t.seq.seq) },
            },
            log => log,
        };
        match log {
            LogRead::Txns(t) => self.call(k, s, n, t, now),
            LogRead::Gone { .. } => {
                let from = s.from;
                self.peers[k].drop_out(format!("it holds transactions up to {}, and this primary's log no longer reaches them: it is sent a copy of the primary", from - 1));
                self.copy(k, s, n, now)
            }
        }
    }

    fn copy(&mut self, k: usize, s: &mut Shipper, n: &Numbering, now: Instant) -> Ship {
        self.changed();
        self.peers[k].copying = true;
        // The copy holds at least what was committed by now.
        s.holding = true;
        s.sent = Some(Sent { numbering: n.clone(), end: None, n: 0, started: now, pin: false, config: 0, prev: SeqNo::default() });
        Ship::Copy { hold_from: n.seq + 1, term: n.term }
    }

    fn call(&mut self, k: usize, s: &mut Shipper, n: &Numbering, mut txns: Vec<chloedb::storage::Shipped>, now: Instant) -> Ship {
        // The replica is shipped up to the round's version, and not past it.
        if let Some(r) = s.round {
            txns.retain(|t| t.seq.seq <= r);
        }
        let from = s.from;
        let prev = SeqNo { term: n.term_at(from - 1), seq: from - 1 };
        let end = txns.last().map_or(prev.seq, |t| t.seq.seq);
        let pin = s.round == Some(end);
        let req = pb::ReplicateRequest {
            term: n.term,
            prev_term: prev.term,
            prev_seq: prev.seq,
            txns: txns.iter().map(|t| t.bytes.to_vec()).collect(),
            global_checkpoint: self.global_checkpoint(n.seq),
            primary: self.id.clone(),
            behind: end < n.seq,
            pin,
            member: self.peers[k].member,
            config: self.kept.config_version(),
            copies: self.kept.config_copies() as u32,
            // The driver signs the call ([`sign`]).
            prev_digest: 0,
        };
        s.sent = Some(Sent { numbering: n.clone(), end: Some(end), n: txns.len() as u64, started: now, pin, config: self.kept.config_version(), prev });
        Ship::Call(req)
    }

    /// Replica `k` answered the call `s` made (an error: the call failed),
    /// the database now at `n`. Returns what to do next, and what to keep
    /// when the answer fenced this primary.
    pub fn answered(&mut self, k: usize, s: &mut Shipper, answer: Result<pb::ReplicateResponse, String>, n: &Numbering) -> (Ship, Option<Kept>) {
        let Some(k) = self.ships_to(k, s) else { return (Ship::Stop, None) };
        if let Ok(a) = &answer {
            self.peers[k].answered = s.sent.as_ref().map(|c| c.started);
            // It took the call, and kept the membership it named, unless it
            // knows a newer term (it applied nothing).
            if a.replica_term <= n.term {
                let v = s.sent.as_ref().map_or(0, |c| c.config);
                self.peers[k].config = self.peers[k].config.max(v);
            }
        }
        self.outcome(k, s, answer, n)
    }

    /// Replica `k` answered the copy `s` sent it. The shipper of a
    /// replica the primary no longer ships to stops, and the replica's next
    /// shipper, which waited for the copy, goes on: it sends another when
    /// this one failed, as the replica still needs one (a replica that
    /// joined holds none of the primary's rows, and would refuse every
    /// batch).
    pub fn copied(&mut self, k: usize, s: &mut Shipper, answer: Result<pb::ReplicateResponse, String>, n: &Numbering) -> (Ship, Option<Kept>) {
        let Some(k) = self.ships_to(k, s) else {
            if let Some(p) = self.peers.iter_mut().find(|p| p.url == s.url) {
                p.copying = false;
                if !answer.as_ref().is_ok_and(|a| a.matched) {
                    p.diverged = true;
                }
            }
            return (Ship::Stop, None);
        };
        self.peers[k].copying = false;
        self.outcome(k, s, answer.map_err(|e| format!("sending it a copy of the primary failed: {e}")), n)
    }

    fn outcome(&mut self, k: usize, s: &mut Shipper, answer: Result<pb::ReplicateResponse, String>, now: &Numbering) -> (Ship, Option<Kept>) {
        let Some(sent) = s.sent.take() else { return (Ship::Again, None) };
        let (numbering, end) = (&sent.numbering, sent.end);
        let last = numbering.seq;
        let mut kept = None;
        let failed = match answer {
            Ok(r) => {
                if r.replica_term > numbering.term {
                    tracing::error!(replica = %self.peers[k].url, term = numbering.term, replica_term = r.replica_term, "a copy holds a newer term: another primary replaced this one");
                    kept = self.fence(r.replica_term, numbering.term);
                    Some(format!("it holds term {}, newer than this primary's ({}): another primary replaced this one", r.replica_term, numbering.term))
                } else if r.differs && end.is_some() {
                    // It holds another transaction than this primary's
                    // under the term and number the call followed: a copy
                    // drops it, as it drops a replaced primary's.
                    let prev = sent.prev;
                    let p = &mut self.peers[k];
                    p.mismatches += 1;
                    tracing::error!(replica = %p.url, seq_no = prev.seq, term = prev.term, "a replica holds another transaction than the primary's under one term and number: it is sent a copy of the primary");
                    p.drop_out(format!(
                        "it holds another transaction than this primary's as transaction {} of term {} (their digests differ): it is sent a copy of this one, which drops its own",
                        prev.seq, prev.term
                    ));
                    // Kept with the replica, not the shipper, so that the
                    // shipper of a primary given other replicas meanwhile
                    // sends the copy too.
                    p.diverged = true;
                    self.changed();
                    return (Ship::Again, None);
                } else if r.matched {
                    let p = &mut self.peers[k];
                    p.acked = r.seq;
                    p.shipped += sent.n;
                    if end.is_none() {
                        p.copies += 1;
                        s.must_copy = false;
                        tracing::info!(replica = %p.url, seq_no = r.seq, "a replica installed a copy of the primary");
                    }
                    s.next = Some(r.seq + 1);
                    s.claimed = false;
                    // It holds what was committed when the batch was read.
                    // A copy is taken after that, and writes go on while it
                    // is sent and installed: it is in sync if it holds what
                    // is committed now.
                    let last = match end {
                        Some(_) => last,
                        None => now.seq,
                    };
                    if r.seq >= last {
                        p.caught_up(end.is_some());
                        // Shipped up to the last, and not just copied: what
                        // followed the copy is no longer needed.
                        if end.is_some() {
                            s.holding = false;
                        }
                    }
                    // The replica keeps its version at the round's
                    // transaction: compare them, while shipping goes on.
                    if sent.pin {
                        s.round = None;
                        s.pinned = true;
                    }
                    self.changed();
                    s.retry = Duration::from_millis(50);
                    None
                } else if end.is_some() && r.seq <= last && numbering.term_at(r.seq) == r.term {
                    // It says it holds a run of this primary's history:
                    // send what follows its last. It counts as holding it
                    // once a call that follows it is applied, which
                    // compares the transaction there by its digest
                    // ([`Replica::follows`]): it may hold another under
                    // that term and number, and no write is to be
                    // acknowledged by it meanwhile.
                    let p = &mut self.peers[k];
                    p.acked = p.acked.min(r.seq);
                    s.next = Some(r.seq + 1);
                    s.claimed = true;
                    return (Ship::Again, None);
                } else if end.is_some() && r.term < numbering.term {
                    // Its last transactions are of a primary this one
                    // replaced, and not this one's: a copy drops them.
                    self.peers[k].drop_out(format!(
                        "its last transaction, {} of term {}, is not this primary's (its last is {last}): it holds transactions of a replaced primary of an older term, and is sent a copy of this one, which drops them",
                        r.seq, r.term
                    ));
                    self.changed();
                    s.must_copy = true;
                    s.round = None;
                    return (Ship::Again, None);
                } else {
                    Some(format!(
                        "its last transaction, {} of term {}, is not this primary's (its last is {last}): its history differs from the primary's; start it again from a checkpoint of the primary",
                        r.seq, r.term
                    ))
                }
            }
            Err(why) => Some(why),
        };
        if let Some(why) = failed {
            let p = &mut self.peers[k];
            p.failures += 1;
            p.drop_out(why);
            self.changed();
            s.next = None;
            s.holding = false;
            s.round = None;
            // A copy is a checkpoint of the database: not again at once.
            let wait = if end.is_none() { self.opts.copy_retry } else { s.retry };
            s.retry = (s.retry * 2).min(self.opts.retry_max);
            return (Ship::Sleep(wait), kept);
        }
        if s.round.is_none() && !s.must_copy && s.next.is_some_and(|next| next > now.seq) {
            return (Ship::Idle(self.opts.heartbeat), kept);
        }
        (Ship::Again, kept)
    }
}

#[cfg(test)]
mod tests {
    /// X-E1: a vote request of a newer term than the replica's primary
    /// ships in, granted or not, stops its reads until a primary of that
    /// term (or a newer one) calls; the old primary's calls do not bring
    /// them back. A pre-vote does not stop them.
    #[test]
    fn a_replica_asked_to_vote_in_a_newer_term_serves_no_reads_until_its_primary_calls() {
        let now = Instant::now();
        let (mut r, _) = Replica::open(Kept { id: "r".into(), follows: Some(("p".into(), 1)), role: Some(KeptRole::Replica), ..Kept::default() }, now, false);
        let at = Applied::At(chloedb::storage::SeqNo { term: 1, seq: 200 });
        r.applied(1, false, 0, Ok(at), 1).unwrap();
        assert!(r.readable().is_ok());
        r.saw_vote(&ask(2, "x", (1, 150), true));
        assert!(r.readable().is_ok(), "a pre-vote changes nothing");
        r.saw_vote(&ask(2, "x", (1, 150), false));
        assert!(r.readable().is_err(), "term 2 may have a primary already");
        r.applied(1, false, 0, Ok(at), 1).unwrap();
        assert!(r.readable().is_err(), "the primary of term 1 does not bring reads back");
        r.applied(2, false, 0, Ok(at), 2).unwrap();
        assert!(r.readable().is_ok(), "term 2's primary called and it caught up");
        r.saw_vote(&ask(2, "y", (1, 150), false));
        assert!(r.readable().is_ok(), "its primary's own term is not newer");
    }

    #[test]
    fn a_copy_that_did_not_run_waits_a_delay_before_it_asks() {
        let after = Duration::from_millis(600);
        let t0 = Instant::now();
        let mut f = Failover::new(after, t0, Duration::ZERO);
        // Looks every few ms: nothing changes.
        f.resumed(t0, t0 + Duration::from_millis(75), Duration::ZERO);
        assert!(f.due(3, Duration::from_secs(1), t0 + Duration::from_millis(75)));
        // Stopped for 10 s: its primary's silence is its own.
        let woke = t0 + Duration::from_secs(10);
        f.resumed(t0 + Duration::from_millis(75), woke, Duration::from_millis(50));
        assert!(!f.due(3, Duration::from_secs(10), woke), "not at once");
        assert!(!f.due(3, Duration::from_secs(10), woke + after), "nor before the jitter");
        assert!(f.due(3, Duration::from_secs(11), woke + after + Duration::from_millis(50)), "a delay and the jitter on, if the primary is still silent");
    }

    use super::*;

    fn n(term: u64, seq: u64, terms: &[(u64, u64)]) -> Numbering {
        Numbering { term, seq, terms: terms.to_vec() }
    }

    fn ask(term: u64, candidate: &str, last: (u64, u64), pre: bool) -> pb::VoteRequest {
        pb::VoteRequest { term, candidate: candidate.into(), last_term: last.0, last_seq: last.1, pre, handover: false, ..Default::default() }
    }

    #[test]
    fn kept_round_trips_through_json() {
        let k = Kept {
            id: "a".into(),
            follows: Some(("b".into(), 3)),
            replaced_by: Some(4),
            role: Some(KeptRole::Primary { replicas: vec!["http://c".into()], acks: Acks::Majority }),
            voted: Some((5, "c".into())),
            member: true,
            config: Some((3, 2)),
            differs: Some(SeqNo { term: 3, seq: 7 }),
        };
        assert_eq!(Kept::from_json(&k.to_json()).unwrap(), k);
        assert_eq!(k.newest(2), 5);
        assert!(Kept::from_json(&json!({"id": ""})).is_err());
    }

    #[test]
    fn a_copy_votes_once_a_term_for_a_copy_not_behind() {
        let mut kept = Kept { id: "me".into(), member: true, ..Kept::default() };
        let at = SeqNo { term: 2, seq: 10 };
        assert!(decide(&kept, 2, at, &ask(3, "x", (2, 10), false)).granted);
        assert!(!decide(&kept, 2, at, &ask(3, "x", (2, 9), false)).granted, "behind");
        assert!(!decide(&kept, 2, at, &ask(2, "x", (2, 10), false)).granted, "not a newer term");
        kept.voted = Some((3, "x".into()));
        assert!(decide(&kept, 2, at, &ask(3, "x", (2, 10), false)).granted, "the same vote again");
        assert!(!decide(&kept, 2, at, &ask(3, "y", (2, 10), false)).granted, "another candidate, same term");
        assert!(!decide(&kept, 2, at, &ask(3, "x", (2, 10), true)).granted, "a pre-vote of a term voted in");
        kept.member = false;
        assert!(!decide(&kept, 2, at, &ask(4, "x", (2, 10), false)).granted, "not a member");
        assert!(decide(&kept, 2, at, &ask(4, "me", (0, 0), false)).candidate);
    }

    #[test]
    fn a_primary_counts_majorities_of_its_members() {
        let t0 = Instant::now();
        let opts = Opts { acks: Acks::All, heartbeat: Duration::from_millis(250), retry_max: Duration::from_secs(2), copy_retry: Duration::from_secs(30), lease: Duration::from_millis(750) };
        let (mut p, save) = Primary::new(Kept::new("p".into()), &["a".into(), "b".into()], opts, false);
        assert!(save.is_some());
        assert_eq!((p.copies(), p.majority()), (3, 2));
        assert!(p.acked(5).is_none());
        p.peers[0].acked = 5;
        assert!(p.acked(5).is_none(), "b is in sync and does not hold it");
        assert!(p.deadline(5).is_ok());
        assert!(!p.peers[1].in_sync);
        assert!(matches!(p.acked(5), Some(Ok(()))));
        assert_eq!(p.global_checkpoint(7), 5);
        // A lease of reads: a majority answered within it.
        assert!(p.readable(t0, 1).is_err());
        p.peers[0].answered = Some(t0);
        assert!(p.readable(t0 + Duration::from_millis(700), 1).is_ok());
        assert!(p.readable(t0 + Duration::from_millis(750), 1).is_err());
        // Fenced by a newer term: no reads, no writes.
        let (a, kept) = p.shipped_to("q", 3, 1, &n(2, 9, &[(1, 1), (2, 5)]));
        assert!(a.is_err() && kept.as_ref().is_some_and(|k| k.replaced_by == Some(3) && k.role == Some(KeptRole::Replica)));
        assert!(p.admit(2, t0).is_err() && p.readable(t0, 2).is_err() && matches!(p.acked(5), Some(Err(_))));
        // The refusals' codes, which a coordinator reads.
        assert_eq!(Primary::write_outcome(&p.admit(2, t0).unwrap_err()), Some(NOT_APPLIED));
        assert_eq!(Primary::write_outcome(&p.acked(5).unwrap().unwrap_err()), Some(IN_DOUBT));
    }

    /// Finding E-1: shard {p, b, c}, p its primary. b and c stop, and p
    /// is given no replicas (two removals, with accept_data_loss): p
    /// serves alone under membership 3. b and c come back holding
    /// membership 1, which names them; b is elected by c in a newer term.
    /// Its calls neither fence p nor take a replica of p's away, however
    /// new their term; a call of p's own membership, or of a newer one,
    /// still does, and so does a server that numbers no membership.
    #[test]
    fn a_primary_of_removed_copies_neither_fences_the_primary_nor_takes_its_replicas() {
        let (mut p, _) = Primary::new(Kept::new("p".into()), &["b".into(), "c".into()], opts(), false);
        assert_eq!(p.kept.config_version(), 1);
        p.reconfigure(vec!["c".into()], Acks::Majority).unwrap();
        p.reconfigure(Vec::new(), Acks::Majority).unwrap();
        assert_eq!(p.kept.config, Some((3, 1)));
        let at = n(2, 40, &[(1, 1), (2, 5)]);
        // b, elected by c in term 3, under membership 1.
        let (a, kept) = p.shipped_to("b", 3, 1, &at);
        let e = a.unwrap_err();
        assert!(e.status == 409 && e.message.contains("version 1 of the shard's membership") && e.message.contains("holds version 3"), "{}", e.message);
        assert!(kept.is_none() && p.replaced_by() == 0 && p.admit(2, Instant::now()).is_ok(), "p still serves");
        // Its copy (Restore) and its comparison (Verify) go through the
        // same check, with the membership their first chunk carries.
        assert!(p.shipped_to("b", 9, 2, &at).0.is_err() && p.replaced_by() == 0);
        // A primary of p's membership, or a newer one, replaces it.
        let (a, kept) = p.shipped_to("d", 4, 3, &at);
        assert!(a.is_err() && kept.is_some() && p.replaced_by() == 4);

        // A replica of the serving primary, which knows membership 3, is
        // not taken by b either: it would leave its primary, whose next
        // call it refuses for its term, which fences that primary.
        let kept = Kept { id: "d".into(), member: true, follows: Some(("p".into(), 2)), role: Some(KeptRole::Replica), config: Some((3, 2)), ..Kept::default() };
        let (r, _) = Replica::open(kept, Instant::now(), false);
        let e = r.take(5, "b", true, (1, 3), &at).unwrap_err();
        assert!(e.status == 409 && e.message.contains("is not followed"), "{}", e.message);
        assert!(matches!(r.take(5, "e", true, (3, 2), &at), Ok(Take::Apply { kept: Some(k) }) if k.follows == Some(("e".into(), 5))));
        assert!(matches!(r.take(5, "e", true, (4, 3), &at), Ok(Take::Apply { kept: Some(k) }) if k.config == Some((4, 3))));
        // A primary that numbers no membership (0), and a replica that
        // knows none, are as before.
        assert!(matches!(r.take(5, "old", true, (0, 0), &at), Ok(Take::Apply { .. })));
        let (fresh, _) = Replica::open(Kept::new("f".into()), Instant::now(), false);
        assert!(matches!(fresh.take(5, "b", true, (1, 3), &at), Ok(Take::Apply { .. })));
        // A copy's first chunk names no copies: the version alone is not
        // kept from it.
        assert!(matches!(fresh.take(5, "b", false, (7, 0), &at), Ok(Take::Apply { kept: Some(k) }) if k.config.is_none()));
    }

    #[test]
    fn a_refusal_before_a_write_is_applied_says_so_in_its_code() {
        let t0 = Instant::now();
        let (mut p, _) = Primary::new(Kept::new("p".into()), &["a".into(), "b".into()], Opts { acks: Acks::Majority, ..opts() }, false);
        p.peers[0].in_sync = false;
        p.peers[1].in_sync = false;
        assert_eq!(Primary::write_outcome(&p.admit(1, t0).unwrap_err()), Some(NOT_APPLIED), "too few copies in sync");
        p.peers[0].in_sync = true;
        p.writes_from = Some(t0 + Duration::from_secs(1));
        assert_eq!(Primary::write_outcome(&p.admit(1, t0).unwrap_err()), Some(NOT_APPLIED), "a lease to wait out");
        p.writes_from = None;
        p.handing_over = Some(t0 + Duration::from_secs(1));
        assert_eq!(Primary::write_outcome(&p.admit(1, t0).unwrap_err()), Some(NOT_APPLIED), "a handover");
        assert_eq!(Primary::write_outcome(&ApiError::bad("a row with key k exists")), None);
    }

    #[test]
    fn a_replica_follows_one_primary_a_term() {
        let t0 = Instant::now();
        let (mut r, save) = Replica::open(Kept::new("r".into()), t0, false);
        assert_eq!(save.unwrap().role, Some(KeptRole::Replica));
        let at = n(2, 4, &[(1, 1), (2, 3)]);
        let Ok(Take::Apply { kept }) = r.take(2, "p", true, (0, 0), &at) else { panic!() };
        let kept = kept.unwrap();
        assert_eq!((kept.follows.clone(), kept.member), (Some(("p".into(), 2)), true));
        r.took(Some(kept), t0);
        assert!(r.take(2, "q", true, (0, 0), &at).is_err(), "a second primary of term 2");
        let Ok(Take::Answer(a)) = r.take(1, "old", false, (0, 0), &at) else { panic!() };
        assert_eq!((a.matched, a.replica_term, a.seq, a.term), (false, 2, 4, 2));
        // Its primary called a moment ago: no vote but a handover.
        let (v, _) = r.vote(&ask(3, "x", (2, 4), false), &at, t0 + Duration::from_millis(10), Duration::from_secs(1));
        assert!(!v.granted);
        let (v, save) = r.vote(&ask(3, "x", (2, 4), false), &at, t0 + Duration::from_secs(1), Duration::from_secs(1));
        assert!(v.granted && save.is_some_and(|k| k.voted == Some((3, "x".into()))));
    }

    #[test]
    fn a_replica_that_voted_waits_a_whole_delay_before_it_campaigns() {
        let t0 = Instant::now();
        let after = Duration::from_secs(2);
        let (mut r, _) = Replica::open(Kept { member: true, ..Kept::new("r".into()) }, t0, false);
        r.took(None, t0);
        let at = n(1, 4, &[(1, 1)]);
        // Its primary went silent 3 s ago; it grants a vote at 3 s.
        let voted = t0 + Duration::from_secs(3);
        assert!(r.silent_for(voted) >= after);
        let (v, save) = r.vote(&ask(2, "x", (1, 4), false), &at, voted, after);
        assert!(v.granted);
        r.kept = save.unwrap();
        r.granted(voted);
        assert_eq!(r.silent_for(voted + Duration::from_millis(500)), Duration::from_millis(500));
        // A pre-vote changes nothing, and is not a vote to wait after.
        let (v, save) = r.vote(&ask(3, "y", (1, 4), true), &at, voted + Duration::from_millis(500), after);
        assert!(v.granted && save.is_none(), "its primary did not call: the stickiness counts the primary's calls only");
        // Its primary calling counts as before.
        r.took(None, voted + Duration::from_secs(1));
        assert_eq!(r.silent_for(voted + Duration::from_secs(1)), Duration::ZERO);
    }

    #[test]
    fn a_round_is_decided_without_the_copies_it_does_not_need() {
        let c = Campaign { id: "me".into(), term: 5, handover: false, config: 0, copies: 0 };
        let me = Some(Ok(pb::VoteResponse { granted: true, candidate: true, ..Default::default() }));
        let yes = Some(Ok(pb::VoteResponse { granted: true, ..Default::default() }));
        let no = Some(Ok(pb::VoteResponse { granted: false, reason: "no".into(), ..Default::default() }));
        let down = Some(Err("no answer within 2 s".into()));
        // Three copies: itself and one more win; one copy cut off is not waited for.
        assert!(c.decided(&[me.clone(), yes.clone(), None]));
        assert!(!c.decided(&[me.clone(), None, None]));
        assert!(!c.decided(&[None, yes.clone(), None]), "the candidate's own answer tells the copies besides it");
        assert!(c.decided(&[me.clone(), no.clone(), down.clone()]), "lost");
        assert!(!c.decided(&[me.clone(), no.clone(), None]), "the last copy could still grant it");
        // Five copies: three votes win.
        assert!(!c.decided(&[me.clone(), yes.clone(), None, None, None]));
        assert!(c.decided(&[me.clone(), yes.clone(), yes.clone(), None, None]));
        assert!(c.decided(&[me.clone(), no.clone(), no.clone(), down.clone(), None]), "1 granted, 1 left: fewer than 3");
        // Not among the copies asked (its own answer never comes): every answer.
        assert!(!c.decided(&[yes.clone(), yes.clone(), None]));
        assert!(c.decided(&[yes.clone(), yes.clone(), no.clone()]));
    }

    #[test]
    fn a_shards_first_copies_vote_from_the_start() {
        let t0 = Instant::now();
        let at = n(1, 0, &[]);
        let long_ago = t0 + Duration::from_secs(60);
        // Started with --replica on a database that had no part yet.
        let (r, save) = Replica::open(Kept::new("r".into()), t0, true);
        assert!(save.is_some_and(|k| k.member && k.role == Some(KeptRole::Replica)));
        assert!(r.vote(&ask(2, "x", (1, 0), false), &at, long_ago, Duration::from_secs(1)).0.granted, "a first copy");
        // A server made a replica of a serving shard (Configure), or one
        // that was a replica before: none until a primary says it caught up.
        for (kept, flags) in [(Kept::new("r".into()), false), (Kept { follows: Some(("p".into(), 1)), role: Some(KeptRole::Replica), ..Kept::new("r".into()) }, true)] {
            let (r, save) = Replica::open(kept, t0, flags);
            assert!(!save.is_some_and(|k| k.member) && !r.kept.member);
            assert!(!r.vote(&ask(2, "x", (1, 0), false), &at, long_ago, Duration::from_secs(1)).0.granted, "a copy that joined");
        }
        // The primary counts itself one, as a replica once it starts again.
        let (p, save) = Primary::new(Kept::new("p".into()), &["r".into()], Opts::of(&PrimaryOptions::default()), true);
        assert!(save.is_some_and(|k| k.member) && p.kept.member);
    }

    fn opts() -> Opts {
        Opts { acks: Acks::All, heartbeat: Duration::from_millis(250), retry_max: Duration::from_secs(2), copy_retry: Duration::from_secs(30), lease: Duration::ZERO }
    }

    fn txns(term: u64, seqs: std::ops::RangeInclusive<u64>) -> Vec<chloedb::storage::Shipped> {
        seqs.map(|seq| chloedb::storage::Shipped { seq: SeqNo { term, seq }, bytes: std::sync::Arc::from(vec![0u8; 1]) }).collect()
    }

    fn matched(seq: u64) -> Result<pb::ReplicateResponse, String> {
        Ok(pb::ReplicateResponse { matched: true, seq, term: 1, replica_term: 1, ..Default::default() })
    }

    #[test]
    fn a_round_ships_a_replica_up_to_its_version_and_pins_it() {
        let t0 = Instant::now();
        let (mut p, _) = Primary::new(Kept::new("p".into()), &["a".into()], opts(), false);
        let mut s = p.shippers().remove(0);
        let at = n(1, 2, &[(1, 1)]);
        // The replica says its last: 2, all the primary holds.
        let Ship::Call(_) = p.poll(0, &mut s, &at, t0) else { panic!() };
        assert!(!p.may_verify(0, &s), "no round before the replica said its last");
        let (next, _) = p.answered(0, &mut s, matched(2), &at);
        assert!(matches!(next, Ship::Idle(_)));
        // The primary writes 3 to 5, and takes a version at 4.
        let at = n(1, 5, &[(1, 1)]);
        assert!(p.may_verify(0, &s) && s.verify_at(4) && s.round() == Some(4));
        assert!(!s.verify_at(4), "one round at a time");
        let Ship::Read { from: 3 } = p.poll(0, &mut s, &at, t0) else { panic!() };
        let Ship::Call(req) = p.read(0, &mut s, &at, LogRead::Txns(txns(1, 3..=5)), t0) else { panic!() };
        assert_eq!((req.txns.len(), req.pin, req.behind), (2, true, true), "up to the version, not past it");
        let (next, _) = p.answered(0, &mut s, matched(4), &at);
        assert!(matches!(next, Ship::Again), "it is behind: ship on");
        assert!(s.take_pinned() && !s.take_pinned() && s.round().is_none());
        let Ship::Read { from: 5 } = p.poll(0, &mut s, &at, t0) else { panic!() };
        // Its rows differed: no round, and it is sent a copy.
        p.diverged("a", "its rows differ".into());
        assert!(!p.peers[0].in_sync && !p.may_verify(0, &s));
        assert!(matches!(p.poll(0, &mut s, &at, t0), Ship::Copy { hold_from: 6, term: 1 }));
    }

    #[test]
    fn a_primary_given_other_replicas_stops_the_shippers_before() {
        let t0 = Instant::now();
        let at = n(1, 2, &[(1, 1)]);
        let (mut p, _) = Primary::new(Kept::new("p".into()), &["a".into(), "b".into()], opts(), false);
        let mut old = p.shippers();
        // b is sent a copy when it changes.
        p.peers[1].diverged = true;
        assert!(matches!(p.poll(1, &mut old[1], &at, t0), Ship::Copy { .. }));
        let kept = p.reconfigure(vec!["b".into(), "c".into()], Acks::Majority).unwrap().unwrap();
        assert!(kept.member && kept.role == Some(KeptRole::Primary { replicas: vec!["b".into(), "c".into()], acks: Acks::Majority }));
        let mut new = p.shippers();
        assert!(matches!(p.poll(0, &mut old[0], &at, t0), Ship::Stop), "a is shipped to no more");
        // b's new shipper waits for the copy its shipper before sends.
        assert!(matches!(p.poll(0, &mut new[0], &at, t0), Ship::Sleep(_)));
        let (next, kept) = p.copied(1, &mut old[1], matched(2), &at);
        assert!(matches!(next, Ship::Stop) && kept.is_none() && !p.peers[0].copying);
        assert!(!matches!(p.poll(0, &mut new[0], &at, t0), Ship::Sleep(_) | Ship::Stop));
        assert!(matches!(p.poll(1, &mut new[1], &at, t0), Ship::Copy { .. }), "c joins: it is sent a copy first");
        assert_eq!((p.copies(), p.majority()), (2, 2), "c counts once it caught up");

        // C is given to a third set of replicas while its copy is sent,
        // and the copy fails: C's next shipper sends another, as C holds
        // none of the primary's rows.
        p.reconfigure(vec!["b".into(), "c".into(), "d".into()], Acks::Majority).unwrap();
        let mut third = p.shippers();
        assert!(matches!(p.poll(1, &mut third[1], &at, t0), Ship::Sleep(_)), "it waits for the copy under way");
        let (next, _) = p.copied(1, &mut new[1], Err("no answer within 10 s".into()), &at);
        assert!(matches!(next, Ship::Stop) && !p.peers[1].copying);
        assert!(matches!(p.poll(1, &mut third[1], &at, t0), Ship::Copy { .. }), "the copy that failed is sent again");
    }

    /// Finding DS-1, as the verifier ran it: {A} cut off from {B, C},
    /// which elect C; A given D, then E, counted 3 of 5 and acknowledged
    /// writes beside C.
    #[test]
    fn a_primary_cut_off_from_its_majority_takes_no_other_replicas() {
        let t0 = Instant::now();
        let lease = Duration::from_millis(750);
        let at = n(1, 2, &[(1, 1)]);
        let opts = Opts { lease, ..opts() };
        let (mut p, _) = Primary::new(Kept::new("a".into()), &["b".into(), "c".into()], opts, false);
        assert_eq!(p.kept.config, Some((1, 3)), "a primary numbers its first membership");
        let mut s = p.shippers();
        // B and C answer at t0, and hold membership 1.
        for k in 0..2 {
            let Ship::Call(req) = p.poll(k, &mut s[k], &at, t0) else { panic!() };
            assert_eq!((req.config, req.copies), (1, 3));
            p.answered(k, &mut s[k], matched(2), &at);
        }
        let d = |more: &[&str]| ["b", "c"].iter().chain(more).map(|u| u.to_string()).collect::<Vec<_>>();
        assert!(p.may_reconfigure(&d(&["d"]), t0 + Duration::from_millis(100), false).is_ok(), "with its majority");
        assert!(p.may_reconfigure(&d(&["d", "e"]), t0, false).unwrap_err().message.contains("one at a time"));
        // Cut off: neither answered within the lease.
        let later = t0 + Duration::from_secs(2);
        let e = p.may_reconfigure(&d(&["d"]), later, false).unwrap_err();
        assert!(e.status == 503 && e.message.contains("fewer than a majority") && e.message.contains("accept_data_loss"), "{}", e.message);
        let e = p.may_reconfigure(&["c".to_string()], later, false).unwrap_err();
        assert!(e.message.contains("fewer than a majority"), "removing the copies it cannot reach is refused too: {}", e.message);
        assert!(p.may_reconfigure(&d(&["d"]), later, true).is_ok(), "the caller accepts the loss");
        // With its majority again: D joins (membership 2, of 4 copies); E
        // waits until a majority holds membership 2.
        let back = later + Duration::from_secs(1);
        let kept = p.reconfigure(d(&["d"]), Acks::Majority).unwrap().unwrap();
        assert_eq!(kept.config, Some((2, 4)));
        let mut s = p.shippers();
        p.peers[0].answered = Some(back);
        p.peers[1].answered = Some(back);
        let e = p.may_reconfigure(&d(&["d", "e"]), back, false).unwrap_err();
        assert!(e.message.contains("not complete"), "{}", e.message);
        let Ship::Call(req) = p.poll(0, &mut s[0], &at, back) else { panic!() };
        assert_eq!((req.config, req.copies), (2, 4));
        p.answered(0, &mut s[0], matched(2), &at);
        p.peers[1].answered = Some(back);
        assert!(p.may_reconfigure(&d(&["d", "e"]), back, false).is_ok(), "A and B hold membership 2");
    }

    /// Finding DS-5 as the verifier ran it: A, elected in term 3, holds a
    /// term-1 transaction at 2 that B took from it; C holds a term-2 one at
    /// 2. A retry of the term-1 write is answered by the database's last
    /// number: it waits for a majority to hold A's own first transaction
    /// of term 3, which C, holding less, can then never be elected past.
    #[test]
    fn a_new_primary_acknowledges_nothing_before_a_transaction_of_its_term() {
        let (mut p, _) = Primary::new(Kept::new("a".into()), &["b".into(), "c".into()], Opts { acks: Acks::Majority, ..opts() }, false);
        p.term_start = 3;
        p.peers[0].acked = 2;
        assert!(p.acked(2).is_none(), "a majority holds 2, of term 1, and none the term's first");
        assert!(p.deadline(2).is_err());
        p.peers[0].acked = 3;
        assert!(matches!(p.acked(2), Some(Ok(()))) && matches!(p.acked(3), Some(Ok(()))));
    }

    /// A replica holding another transaction than its primary's under
    /// one term and number: it refuses the call that follows the primary's
    /// by its digest, serves no reads, and is sent a copy; meanwhile its
    /// say that it holds a run of the primary's history counts no write.
    #[test]
    fn a_replica_holding_another_transaction_of_a_number_is_found_and_copied() {
        let t0 = Instant::now();
        let shipped = |seq: u64, write: u8| chloedb::storage::Shipped { seq: SeqNo { term: 1, seq }, bytes: std::sync::Arc::from(vec![write; 3]) };
        let at = n(1, 3, &[(1, 1)]);
        let (mut p, _) = Primary::new(Kept::new("p".into()), &["a".into(), "b".into()], Opts { acks: Acks::Majority, ..opts() }, false);
        let mut s = p.shippers().remove(0);
        // Its first call follows 3, the primary's last, signed by the
        // primary's digest of it.
        let Ship::Call(mut req) = p.poll(0, &mut s, &at, t0) else { panic!() };
        assert_eq!((req.prev_term, req.prev_seq, req.prev_digest), (1, 3, 0), "the driver signs it");
        sign(&mut req, LogRead::Txns(vec![shipped(3, 7)]), &digest);
        assert_eq!(req.prev_digest, digest(&[7; 3]));
        let mut none = req.clone();
        sign(&mut none, LogRead::Gone { oldest: 4 }, &digest);
        assert_eq!(none.prev_digest, 0, "a log that no longer holds it: none");

        // The replica holds transaction 3 of term 1 as write 9.
        let (mut r, _) = Replica::open(Kept::new("a".into()), t0, false);
        r.serves_reads = true;
        let mine = || LogRead::Txns(vec![shipped(3, 9)]);
        assert!(r.follows(&req, &at, || LogRead::Txns(vec![shipped(3, 7)]), &digest).is_none(), "its own: applied");
        assert!(r.follows(&req, &at, || LogRead::Gone { oldest: 4 }, &digest).is_none(), "a log that no longer holds it cannot tell");
        assert!(r.follows(&req, &n(1, 2, &[(1, 1)]), mine, &digest).is_none(), "behind: by number");
        assert!(r.follows(&req, &n(2, 3, &[(1, 1), (2, 3)]), mine, &digest).is_none(), "3 of another term: by number");
        let (a, kept) = r.follows(&req, &at, mine, &digest).expect("refused");
        assert!(!a.matched && a.differs && (a.seq, a.term) == (3, 1), "{a:?}");
        let kept = kept.expect("kept that it differs");
        assert_eq!(kept.differs, Some(SeqNo { term: 1, seq: 3 }));
        assert_eq!(Kept::from_json(&kept.to_json()).unwrap(), kept);
        r.took(Some(kept), t0);
        assert_eq!((r.mismatches, r.serves_reads), (1, false));
        // Until it installs a copy, every call with a digest is refused,
        // its own transaction's too, and one without is applied as before.
        let (a, kept) = r.follows(&req, &at, || LogRead::Txns(vec![shipped(3, 7)]), &digest).expect("refused");
        assert!(a.differs && kept.is_none() && r.mismatches == 2);
        assert!(r.follows(&none, &at, mine, &digest).is_none());
        let (a, kept) = r.installed(SeqNo { term: 1, seq: 3 }, 1, 1);
        assert!(a.matched && kept.as_ref().is_some_and(|k| k.differs.is_none()));
        r.took(kept, t0);
        assert!(r.follows(&req, &at, || LogRead::Txns(vec![shipped(3, 7)]), &digest).is_none());

        // The primary, told so, takes it out of sync and sends it a copy.
        let (next, kept) = p.answered(0, &mut s, Ok(pb::ReplicateResponse { matched: false, seq: 3, term: 1, replica_term: 1, differs: true }), &at);
        assert!(matches!(next, Ship::Again) && kept.is_none());
        let peer = &p.peers[0];
        assert!(!peer.in_sync && peer.mismatches == 1 && peer.error.as_deref().is_some_and(|e| e.contains("transaction 3 of term 1")), "{peer:?}");
        assert!(matches!(p.poll(0, &mut s, &at, t0), Ship::Copy { .. }));

        // B says its last is 3 of term 1, where the primary is at 4: it is
        // shipped from 4, and holds 3 for no write until that call applies.
        let at = n(1, 4, &[(1, 1)]);
        let mut s = p.shippers().remove(1);
        let Ship::Call(_) = p.poll(1, &mut s, &at, t0) else { panic!() };
        let (next, _) = p.answered(1, &mut s, Ok(pb::ReplicateResponse { matched: false, seq: 3, term: 1, replica_term: 1, ..Default::default() }), &at);
        assert!(matches!(next, Ship::Again));
        assert_eq!(p.peers[1].acked, 0, "not counted by what it says");
        assert!(p.acked(3).is_none(), "the primary alone of three");
        // Read from 3 on: the call that follows 3 carries its digest, or B
        // is sent a copy when the log no longer holds 3.
        let mut lost = s.clone();
        let Ship::Read { from: 3 } = p.poll(1, &mut lost, &at, t0) else { panic!() };
        assert!(matches!(p.read(1, &mut lost, &at, LogRead::Txns(vec![shipped(4, 1)]), t0), Ship::Copy { .. }), "3 is not in the log");
        p.peers[1].copying = false;
        let Ship::Read { from: 3 } = p.poll(1, &mut s, &at, t0) else { panic!() };
        let Ship::Call(req) = p.read(1, &mut s, &at, LogRead::Txns(vec![shipped(3, 7), shipped(4, 1)]), t0) else { panic!() };
        assert_eq!((req.prev_term, req.prev_seq, req.txns.len()), (1, 3, 1), "4 follows 3");
        p.answered(1, &mut s, matched(4), &at);
        assert_eq!(p.peers[1].acked, 4);
        assert!(matches!(p.acked(4), Some(Ok(()))));
    }

    /// The other half of DS-1: a copy that heard of a newer membership
    /// votes for no candidate that knows an older one, and a candidate
    /// needs a majority of the membership it knows besides one of the
    /// copies it asks (its map may lack the copies added since).
    #[test]
    fn a_candidate_of_an_older_membership_is_refused_and_counts_the_newest() {
        let t0 = Instant::now();
        let at = n(1, 2, &[(1, 1)]);
        let (r, _) = Replica::open(Kept { member: true, ..Kept::new("b".into()) }, t0, false);
        let Ok(Take::Apply { kept }) = r.take(1, "a", true, (2, 4), &at) else { panic!() };
        let kept = kept.unwrap();
        assert_eq!(kept.config, Some((2, 4)));
        let old = pb::VoteRequest { config: 1, copies: 3, ..ask(2, "c", (1, 2), false) };
        let v = decide(&kept, 1, at.last(), &old);
        assert!(!v.granted && v.reason.contains("membership"), "{}", v.reason);
        assert!(decide(&kept, 1, at.last(), &pb::VoteRequest { config: 2, ..old.clone() }).granted);
        assert!(decide(&kept, 1, at.last(), &pb::VoteRequest { config: 0, ..old }).granted, "an older server numbers none");
        // B, knowing 4 copies, asks the 3 of its map: 2 votes do not win.
        let (mut r, _) = Replica::open(kept, t0, false);
        r.took(None, t0);
        let c = r.campaign(&at, false);
        assert_eq!((c.config, c.copies), (2, 4));
        let me = Ok(pb::VoteResponse { granted: true, candidate: true, ..Default::default() });
        let t = c.tally(&["a".into(), "b".into(), "c".into()], vec![Err("cut off".into()), me, said(1, true)]);
        assert_eq!((t.granted, t.need), (2, 3));
        assert!(c.voted(t).is_err());
    }

    fn said(term: u64, granted: bool) -> Result<pb::VoteResponse, String> {
        Ok(pb::VoteResponse { granted, term, last_seq: 4, last_term: 2, reason: String::new(), candidate: false })
    }

    #[test]
    fn a_replica_asks_above_the_newest_term_a_copy_told_it() {
        let t0 = Instant::now();
        let (mut r, _) = Replica::open(Kept::new("r".into()), t0, false);
        let at = n(2, 4, &[(1, 1), (2, 3)]);
        let c = r.campaign(&at, false);
        assert_eq!(c.term, 3);
        // Two copies of five voted in term 3 for a candidate that lost.
        let copies: Vec<String> = ["a", "b", "c", "d"].map(String::from).to_vec();
        let t = c.tally(&copies, vec![said(3, false), said(3, false), said(2, true), Err("down".into())]);
        assert!(c.pre_voted(&t).is_err());
        r.heard(&t);
        assert_eq!(r.campaign(&at, false).term, 4, "not term 3 again, which the copies that voted refuse");
        let t = r.campaign(&at, false).tally(&copies, vec![said(3, true), said(3, true), said(2, true), Err("down".into())]);
        assert!(r.campaign(&at, false).pre_voted(&t).is_ok());
    }

    #[test]
    fn a_primary_elected_by_hand_waits_out_the_lease_of_the_one_it_replaced() {
        let lease = Duration::from_millis(750);
        let hello = |role: pb::Role| Ok(pb::HelloResponse { role: role as i32, term: 2, seq_no: 4, last_term: 2, ..Default::default() });
        let said = vec![("p".to_string(), hello(pb::Role::Primary)), ("q".to_string(), hello(pb::Role::Replica))];
        let asked = promote_check(&n(2, 4, &[(1, 1), (2, 3)]), &said, false).unwrap();
        assert_eq!(asked.primaries, ["p"]);
        let elected = |p_granted: bool| Elected { term: 3, others: vec!["p".into(), "q".into()], votes: json!([{"url": "p", "granted": p_granted}, {"url": "q", "granted": true}]) };
        assert_eq!(handover_hold(Some(&asked), Some(&elected(true)), lease), Duration::ZERO, "the primary voted and stepped down");
        assert_eq!(handover_hold(Some(&asked), Some(&elected(false)), lease), Duration::from_millis(1000));
        assert_eq!(handover_hold(Some(&asked), None, lease), Duration::from_millis(1000), "with force, no votes");
        assert_eq!(handover_hold(None, Some(&elected(true)), lease), Duration::from_millis(1000), "no copy was asked");
        assert_eq!(handover_hold(Some(&asked), Some(&elected(false)), Duration::ZERO), Duration::ZERO, "no lease");
        let opts = Opts { acks: Acks::Majority, heartbeat: Duration::from_millis(250), retry_max: Duration::from_secs(2), copy_retry: Duration::from_secs(30), lease };
        let (mut p, _) = Primary::new(Kept::new("r".into()), &["p".into(), "q".into()], opts, false);
        let t0 = Instant::now();
        p.writes_from = Some(t0 + Duration::from_millis(1000));
        assert_eq!(p.admit(3, t0).unwrap_err().status, 503);
        assert!(p.admit(3, t0 + Duration::from_millis(1000)).is_ok());
    }

    #[test]
    fn a_primary_asked_to_hand_over_takes_no_writes_until_it_answers_the_vote() {
        let opts = Opts { acks: Acks::Primary, heartbeat: Duration::from_millis(250), retry_max: Duration::from_secs(2), copy_retry: Duration::from_secs(30), lease: Duration::ZERO };
        let (mut p, _) = Primary::new(Kept::new("p".into()), &["q".into()], opts, false);
        let (t0, at, pause) = (Instant::now(), n(1, 5, &[(1, 1)]), Duration::from_secs(2));
        let handover = |last: (u64, u64), pre: bool| pb::VoteRequest { handover: true, ..ask(2, "q", last, pre) };
        assert!(p.hand_over(t0 + pause));
        let e = p.admit(1, t0).unwrap_err();
        assert!(e.status == 503 && e.message.contains("the write was not applied"), "{}", e.message);
        // A copy a transaction behind would not be voted for; it catches up,
        // and no write comes meanwhile.
        let r = handover((1, 4), true);
        let (a, _) = p.vote(&r, &at);
        assert!(!a.granted && a.reason.contains("past the candidate's last"), "{}", a.reason);
        p.answered_handover(&r, &at, &a);
        assert!(p.admit(1, t0).is_err());
        // Refused the vote itself: it takes writes again.
        let r = handover((1, 4), false);
        let (a, _) = p.vote(&r, &at);
        assert!(!a.granted);
        p.answered_handover(&r, &at, &a);
        assert!(p.admit(1, t0).is_ok());
        // Asked for a vote it would not grant for another reason: at once.
        let r = handover((1, 5), true);
        assert!(p.hand_over(t0 + pause));
        let (a, _) = p.vote(&ask(1, "q", (1, 5), true), &at);
        assert!(!a.granted);
        p.answered_handover(&r, &at, &a);
        assert!(p.admit(1, t0).is_ok());
        // Asked and never voted: once the pause ran out.
        assert!(p.hand_over(t0 + pause));
        assert!(p.admit(1, t0).is_err());
        assert!(p.admit(1, t0 + pause).is_ok());
        // Replaced, it takes no writes anyway.
        p.fenced = 2;
        assert!(!p.hand_over(t0 + pause));
    }
}
