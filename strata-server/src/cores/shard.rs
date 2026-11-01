//! A shard's copies, as decisions with no I/O ([`super`]): what each copy
//! keeps beside its database ([`Kept`]), how it votes ([`decide`]), how a
//! replica takes what its primary ships ([`Replica`]), how a replica is
//! elected ([`Campaign`], [`Failover`]), and how a primary ships to its
//! replicas, counts the copies that hold a write, is fenced and holds a
//! lease on reads ([`Primary`], [`Shipper`]).
//!
//! The rules are [`crate::replication`]'s, whose documentation says what
//! they promise and why; each function here is one of its decisions, with
//! the same answers and messages. What a caller must keep durably before it
//! answers comes back as a [`Kept`] to save; the time is passed in.

use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata::storage::{Applied, LogRead, Numbering, SeqNo};

use crate::replication::{Acks, Elected, KeptRole, PrimaryOptions};
use crate::{pb, ApiError};

fn conflict(message: String) -> ApiError {
    ApiError { status: 409, message }
}

fn unavailable(message: String) -> ApiError {
    ApiError { status: 503, message }
}

/// What a copy keeps beside its database (`<db>.replication`), durably
/// before it answers the call that changed it: its id; the primary it
/// follows and that one's term; the newer term of a primary that replaced
/// it, as a primary; its part; the term it last voted in and the copy it
/// voted for; and whether a primary counted it among the copies a majority
/// is counted of (it grants votes from then).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Kept {
    pub id: String,
    pub follows: Option<(String, u64)>,
    pub replaced_by: Option<u64>,
    pub role: Option<KeptRole>,
    pub voted: Option<(u64, String)>,
    pub member: bool,
}

impl Kept {
    /// A copy with id `id` and nothing else kept yet.
    pub fn new(id: String) -> Kept {
        Kept { id, ..Kept::default() }
    }

    /// The newest term it knows of, besides the database's `term`: of the
    /// primary it follows, of the one that replaced it, and the term it
    /// voted in.
    pub fn newest(&self, term: u64) -> u64 {
        term.max(self.follows.as_ref().map_or(0, |f| f.1)).max(self.replaced_by.unwrap_or(0)).max(self.voted.as_ref().map_or(0, |v| v.0))
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
        Ok(Kept { id, follows, replaced_by, role, voted, member })
    }
}

/// A vote refused, by a copy that knows of term `term` and holds `last`.
pub fn refused(term: u64, last: SeqNo, reason: String) -> pb::VoteResponse {
    pb::VoteResponse { granted: false, term, last_seq: last.seq, last_term: last.term, reason, candidate: false }
}

/// Whether a copy keeping `kept`, at `term` and holding `last`, grants
/// vote `r`: one vote a term, for a newer term than any it knows, and for
/// a copy holding every transaction it holds (by last term, then last
/// number), once a primary counted it a member. The candidate asking
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
}

impl Replica {
    /// A replica keeping `kept`, from `now`, and what to keep when its
    /// part changes.
    pub fn open(kept: Kept, now: Instant) -> (Replica, Option<Kept>) {
        let save = (kept.role != Some(KeptRole::Replica)).then(|| Kept { role: Some(KeptRole::Replica), ..kept.clone() });
        let kept = save.clone().unwrap_or(kept);
        (Replica { kept, called_at: None, serves_reads: false, opened: now, global_checkpoint: 0, heard: 0 }, save)
    }

    /// How long since its primary last called, or since it started
    /// following when none has.
    pub fn silent_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.called_at.unwrap_or(self.opened))
    }

    /// Answers vote `r` ([`decide`]) at `n`, and what to keep (the vote)
    /// before answering. Refused, but for a `handover`, while its primary
    /// called within `sticky`: that primary serves.
    pub fn vote(&self, r: &pb::VoteRequest, n: &Numbering, now: Instant, sticky: Duration) -> (pb::VoteResponse, Option<Kept>) {
        let kept = &self.kept;
        let quiet = self.silent_for(now);
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
    /// primary, counting itself a member once a call says so (`member`).
    pub fn take(&self, term: u64, primary: &str, member: bool, n: &Numbering) -> Result<Take, ApiError> {
        let kept = &self.kept;
        // A term it voted in counts: the primary of an older one lost.
        let newest = kept.newest(n.term);
        if term < newest {
            let last = n.last();
            return Ok(Take::Answer(pb::ReplicateResponse { matched: false, seq: last.seq, term: last.term, replica_term: newest }));
        }
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
    /// what [`strata::storage::Database::apply_shipped`] did, `db_term` the
    /// database's term after it, and `term`, `behind` and
    /// `global_checkpoint` are the call's. It serves reads while what it
    /// holds reaches the primary's last.
    pub fn applied(&mut self, term: u64, behind: bool, global_checkpoint: u64, applied: Result<Applied, ApiError>, db_term: u64) -> Result<pb::ReplicateResponse, ApiError> {
        let replica_term = db_term.max(term);
        self.serves_reads = matches!(applied, Ok(Applied::At(_))) && !behind;
        Ok(match applied? {
            Applied::At(at) => {
                self.global_checkpoint = global_checkpoint.min(at.seq);
                pb::ReplicateResponse { matched: true, seq: at.seq, term: at.term, replica_term }
            }
            Applied::Mismatch { seq, term } => pb::ReplicateResponse { matched: false, seq, term, replica_term },
        })
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
    pub fn installed(&mut self, at: SeqNo, term: u64, db_term: u64) -> pb::ReplicateResponse {
        self.serves_reads = false;
        pb::ReplicateResponse { matched: true, seq: at.seq, term: at.term, replica_term: db_term.max(term) }
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
        Campaign { id: self.kept.id.clone(), term: self.kept.newest(n.term).max(self.heard) + 1, handover }
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
    /// ([`strata::storage::Database::begin_term`]) and what to keep then,
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
        let next = Kept { id: kept.id.clone(), role: Some(KeptRole::Primary { replicas: replicas.to_vec(), acks }), voted: kept.voted.clone(), member: true, ..Kept::default() };
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

/// How long a replica elected by hand (`elected`, a handover, after the
/// copies said `asked`, none with `force`) waits before it takes writes,
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
pub fn handover_hold(asked: Option<&Asked>, elected: &Elected, lease: Duration) -> Duration {
    let stepped_down = asked.is_some_and(|a| {
        let granted = |url: &String| elected.votes.as_array().is_some_and(|v| v.iter().any(|j| j["url"] == json!(url) && j["granted"] == json!(true)));
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
/// counted once.
#[derive(Clone, Debug)]
pub struct Campaign {
    pub id: String,
    pub term: u64,
    pub handover: bool,
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
    /// What each copy answered.
    pub said: Json,
}

impl Campaign {
    /// The vote asked, by a candidate holding `last`.
    pub fn ask(&self, last: SeqNo, pre: bool) -> pb::VoteRequest {
        pb::VoteRequest { term: self.term, candidate: self.id.clone(), last_seq: last.seq, last_term: last.term, pre, handover: self.handover }
    }

    /// The votes of `copies`, which answered `answers` in order (an error:
    /// no answer). The candidate among them answers as such, and is not
    /// counted again.
    pub fn tally(&self, copies: &[String], answers: Vec<Result<pb::VoteResponse, String>>) -> Tally {
        let mut others = Vec::new();
        let (mut granted, mut newer, mut said) = (1, 0, Vec::new());
        for (url, a) in copies.iter().zip(answers) {
            match a {
                Ok(a) if a.candidate => {}
                Ok(a) => {
                    others.push(url.clone());
                    granted += a.granted as usize;
                    newer = newer.max(a.term);
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
        let need = (others.len() + 1) / 2 + 1;
        Tally { others, granted, need, newer, said: Json::Array(said) }
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
    /// Read the log from `from` on ([`strata::storage::Database::log_from`])
    /// and give it to [`Primary::read`].
    Read { from: u64 },
    /// Call the replica (`Replicate`) and give its answer to
    /// [`Primary::answered`].
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
}

/// One task's shipping to a replica: the next number to send (`None`
/// until the replica said its last), whether it must be sent a copy, the
/// wait before calling again after a failure, and whether the log is held
/// for it. The task holds it; the [`Primary`] holds what is shared.
#[derive(Clone, Debug)]
pub struct Shipper {
    /// The [`Primary`] generation it ships for ([`Peer::owner`]).
    pub gen: u64,
    next: Option<u64>,
    must_copy: bool,
    retry: Duration,
    holding: bool,
    from: u64,
    sent: Option<Sent>,
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
    gen: u64,
}

impl Primary {
    /// A primary keeping `kept` (whose id is its own) and shipping to
    /// `replicas`, and what to keep when its part changes. Every replica
    /// counts as in sync until it fails, so that the first writes wait for
    /// replicas that are there, but those `joining`, which are sent a copy
    /// of the primary first.
    pub fn new(kept: Kept, replicas: &[String], opts: Opts, joining: bool) -> (Primary, Option<Kept>) {
        let role = Some(KeptRole::Primary { replicas: replicas.to_vec(), acks: opts.acks });
        let save = (kept.role != role).then(|| Kept { role: role.clone(), ..kept.clone() });
        let kept = save.clone().unwrap_or(kept);
        let mut p = Primary { id: kept.id.clone(), opts, peers: replicas.iter().map(|u| Peer::new(u, joining)).collect(), fenced: 0, kept, progress: 0, writes_from: None, gen: 0 };
        p.gen = 1;
        for peer in &mut p.peers {
            peer.owner = p.gen;
        }
        (p, save)
    }

    /// A shipper for each replica, by index.
    pub fn shippers(&self) -> Vec<Shipper> {
        self.peers.iter().map(|_| Shipper { gen: self.gen, next: None, must_copy: false, retry: Duration::from_millis(50), holding: false, from: 0, sent: None }).collect()
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
        self.kept = Kept { id: self.id.clone(), follows: None, replaced_by: Some(term), role: Some(KeptRole::Replica), voted: self.kept.voted.clone(), member: true };
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

    /// Answers a primary's call to ship to this one, of `term`, while this
    /// one is at `n`: an older term is answered with this one's (which
    /// fences that primary), a newer one fences this primary and is
    /// refused (409), and so is one of the same term. Returns what to keep
    /// when fenced.
    pub fn shipped_to(&mut self, primary: &str, term: u64, n: &Numbering) -> (Result<pb::ReplicateResponse, ApiError>, Option<Kept>) {
        let (mine, last) = (n.term, n.last());
        if term < mine {
            return (Ok(pb::ReplicateResponse { matched: false, seq: last.seq, term: last.term, replica_term: mine }), None);
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

    /// It kept `kept`, its vote for another copy in term `term`: it steps
    /// down, as a primary the new one replaced. Returns what to keep then.
    pub fn gave_vote(&mut self, kept: Kept, term: u64, mine: u64) -> Option<Kept> {
        tracing::warn!(term, "voted for another copy to be the shard's primary: this one steps down");
        self.kept = kept;
        self.fence(term, mine)
    }

    /// Whether a write may be applied at term `term` at `now`: not on a
    /// primary another replaced (409), not before [`Self::writes_from`]
    /// (503), and, unless only the primary acknowledges, while a majority
    /// of copies is in sync (503).
    pub fn admit(&self, term: u64, now: Instant) -> Result<(), ApiError> {
        if self.fenced > 0 {
            return Err(conflict(format!("a copy holds term {}, newer than this primary's ({term}): another primary replaced this one, which takes no more writes", self.fenced)));
        }
        if let Some(from) = self.writes_from.filter(|&f| f > now) {
            return Err(unavailable(format!(
                "this primary was elected by hand, and the primary it replaced may serve reads for {} ms more: it takes writes after that",
                (from - now).as_millis()
            )));
        }
        if self.opts.acks != Acks::Primary {
            let up = 1 + self.peers.iter().filter(|p| p.member && p.in_sync).count();
            if up < self.majority() {
                return Err(unavailable(format!("{up} of {} copies are in sync, and --write-acks {} needs {}: the write was not applied", self.copies(), self.opts.acks.name(), self.majority())));
            }
        }
        Ok(())
    }

    fn holding(&self, seq: u64) -> (usize, bool) {
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
            status: 504,
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
        self.peers = peers;
        self.opts.acks = acks;
        let role = Some(KeptRole::Primary { replicas, acks });
        self.kept = Kept { id: self.id.clone(), voted: self.kept.voted.clone(), role, ..Kept::default() };
        Ok(Some(self.kept.clone()))
    }

    /// What replica `k`'s shipper `s` does next, the database at `n`, at
    /// `now`: a replica whose rows differ, or one holding a replaced
    /// primary's transactions, is sent a copy; one past the primary's last
    /// is called with no transactions (a heartbeat); otherwise the log is
    /// read from its next number.
    pub fn poll(&mut self, k: usize, s: &mut Shipper, n: &Numbering, now: Instant) -> Ship {
        let p = &mut self.peers[k];
        if p.owner != s.gen {
            return Ship::Stop;
        }
        // A shipper of the primary before this one is sending it a copy.
        if p.copying && s.next.is_none() && !s.holding {
            return Ship::Sleep(Duration::from_millis(100));
        }
        if std::mem::take(&mut p.diverged) {
            s.must_copy = true;
        }
        let last = n.seq;
        let from = s.next.unwrap_or(last + 1);
        s.from = from;
        if s.must_copy {
            return self.copy(k, s, n, now);
        }
        if from > last {
            return self.call(k, s, n, Vec::new(), now);
        }
        Ship::Read { from }
    }

    /// [`Self::poll`], given what the log holds from the number asked.
    pub fn read(&mut self, k: usize, s: &mut Shipper, n: &Numbering, log: LogRead, now: Instant) -> Ship {
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
        s.sent = Some(Sent { numbering: n.clone(), end: None, n: 0, started: now });
        Ship::Copy { hold_from: n.seq + 1, term: n.term }
    }

    fn call(&mut self, k: usize, s: &mut Shipper, n: &Numbering, txns: Vec<strata::storage::Shipped>, now: Instant) -> Ship {
        let from = s.from;
        let prev = SeqNo { term: n.term_at(from - 1), seq: from - 1 };
        let end = txns.last().map_or(prev.seq, |t| t.seq.seq);
        let req = pb::ReplicateRequest {
            term: n.term,
            prev_term: prev.term,
            prev_seq: prev.seq,
            txns: txns.iter().map(|t| t.bytes.to_vec()).collect(),
            global_checkpoint: self.global_checkpoint(n.seq),
            primary: self.id.clone(),
            behind: end < n.seq,
            pin: false,
            member: self.peers[k].member,
        };
        s.sent = Some(Sent { numbering: n.clone(), end: Some(end), n: txns.len() as u64, started: now });
        Ship::Call(req)
    }

    /// Replica `k` answered the call `s` made (an error: the call failed),
    /// the database now at `n`. Returns what to do next, and what to keep
    /// when the answer fenced this primary.
    pub fn answered(&mut self, k: usize, s: &mut Shipper, answer: Result<pb::ReplicateResponse, String>, n: &Numbering) -> (Ship, Option<Kept>) {
        if answer.is_ok() {
            self.peers[k].answered = s.sent.as_ref().map(|c| c.started);
        }
        self.outcome(k, s, answer, n)
    }

    /// Replica `k` answered the copy `s` sent it.
    pub fn copied(&mut self, k: usize, s: &mut Shipper, answer: Result<pb::ReplicateResponse, String>, n: &Numbering) -> (Ship, Option<Kept>) {
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
                    self.changed();
                    s.retry = Duration::from_millis(50);
                    None
                } else if end.is_some() && r.seq <= last && numbering.term_at(r.seq) == r.term {
                    // It holds a run of this primary's history: send what
                    // follows its last.
                    self.peers[k].acked = r.seq;
                    s.next = Some(r.seq + 1);
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
            // A copy is a checkpoint of the database: not again at once.
            let wait = if end.is_none() { self.opts.copy_retry } else { s.retry };
            s.retry = (s.retry * 2).min(self.opts.retry_max);
            return (Ship::Sleep(wait), kept);
        }
        if !s.must_copy && s.next.is_some_and(|next| next > now.seq) {
            return (Ship::Idle(self.opts.heartbeat), kept);
        }
        (Ship::Again, kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(term: u64, seq: u64, terms: &[(u64, u64)]) -> Numbering {
        Numbering { term, seq, terms: terms.to_vec() }
    }

    fn ask(term: u64, candidate: &str, last: (u64, u64), pre: bool) -> pb::VoteRequest {
        pb::VoteRequest { term, candidate: candidate.into(), last_term: last.0, last_seq: last.1, pre, handover: false }
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
        let (a, kept) = p.shipped_to("q", 3, &n(2, 9, &[(1, 1), (2, 5)]));
        assert!(a.is_err() && kept.as_ref().is_some_and(|k| k.replaced_by == Some(3) && k.role == Some(KeptRole::Replica)));
        assert!(p.admit(2, t0).is_err() && p.readable(t0, 2).is_err() && matches!(p.acked(5), Some(Err(_))));
    }

    #[test]
    fn a_replica_follows_one_primary_a_term() {
        let t0 = Instant::now();
        let (mut r, save) = Replica::open(Kept::new("r".into()), t0);
        assert_eq!(save.unwrap().role, Some(KeptRole::Replica));
        let at = n(2, 4, &[(1, 1), (2, 3)]);
        let Ok(Take::Apply { kept }) = r.take(2, "p", true, &at) else { panic!() };
        let kept = kept.unwrap();
        assert_eq!((kept.follows.clone(), kept.member), (Some(("p".into(), 2)), true));
        r.took(Some(kept), t0);
        assert!(r.take(2, "q", true, &at).is_err(), "a second primary of term 2");
        let Ok(Take::Answer(a)) = r.take(1, "old", false, &at) else { panic!() };
        assert_eq!((a.matched, a.replica_term, a.seq, a.term), (false, 2, 4, 2));
        // Its primary called a moment ago: no vote but a handover.
        let (v, _) = r.vote(&ask(3, "x", (2, 4), false), &at, t0 + Duration::from_millis(10), Duration::from_secs(1));
        assert!(!v.granted);
        let (v, save) = r.vote(&ask(3, "x", (2, 4), false), &at, t0 + Duration::from_secs(1), Duration::from_secs(1));
        assert!(v.granted && save.is_some_and(|k| k.voted == Some((3, "x".into()))));
    }

    fn said(term: u64, granted: bool) -> Result<pb::VoteResponse, String> {
        Ok(pb::VoteResponse { granted, term, last_seq: 4, last_term: 2, reason: String::new(), candidate: false })
    }

    #[test]
    fn a_replica_asks_above_the_newest_term_a_copy_told_it() {
        let t0 = Instant::now();
        let (mut r, _) = Replica::open(Kept::new("r".into()), t0);
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
        assert_eq!(handover_hold(Some(&asked), &elected(true), lease), Duration::ZERO, "the primary voted and stepped down");
        assert_eq!(handover_hold(Some(&asked), &elected(false), lease), Duration::from_millis(1000));
        assert_eq!(handover_hold(None, &elected(true), lease), Duration::from_millis(1000), "with force, no copy was asked");
        assert_eq!(handover_hold(Some(&asked), &elected(false), Duration::ZERO), Duration::ZERO, "no lease");
        let opts = Opts { acks: Acks::Majority, heartbeat: Duration::from_millis(250), retry_max: Duration::from_secs(2), copy_retry: Duration::from_secs(30), lease };
        let (mut p, _) = Primary::new(Kept::new("r".into()), &["p".into(), "q".into()], opts, false);
        let t0 = Instant::now();
        p.writes_from = Some(t0 + Duration::from_millis(1000));
        assert_eq!(p.admit(3, t0).unwrap_err().status, 503);
        assert!(p.admit(3, t0 + Duration::from_millis(1000)).is_ok());
    }
}
