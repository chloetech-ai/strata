//! A shard's copy in the simulator: what a server does around the cores
//! ([`crate::cores::shard`]): keeping what they say to keep, applying to its
//! database, calling, and waiting.

use std::time::Duration;

use crate::cores::shard::{self, Asked, Campaign, Failover, Kept, Opts, Primary, Replica, Restart, Ship, Shipper, Take};
use crate::replication::PrimaryOptions;
use crate::service::failover_tuned;
use crate::{pb, ApiError};

use super::db::ModelDb;
use super::rng::Rng;
use super::{Bug, Config, Purpose, Req, Resp, Sim, Timer};

/// A copy: its files (what it keeps, its database) and, while it runs,
/// its part.
pub(crate) struct Copy {
    pub url: String,
    pub id: String,
    pub up: bool,
    /// Each start is a new incarnation: timers and calls of the one
    /// before are dropped.
    pub inc: u64,
    /// Its clock's rate, in millionths of true.
    pub rate: u64,
    /// `<db>.replication`, as kept.
    pub kept: Option<Kept>,
    pub db: ModelDb,
    pub role: Role,
    failover: Option<Failover>,
    election: Option<Election>,
    rounds: u64,
}

pub(crate) enum Role {
    Down,
    Replica(Replica),
    Primary(Box<Run>),
}

pub(crate) struct Run {
    pub core: Primary,
    ships: Vec<Task>,
    /// Writes numbered here and waiting for acknowledgments: the number,
    /// the write, and the call to answer.
    pending: Vec<(u64, u64, u64, usize)>,
}

/// A shipper's task: its state in the core, the wait it is in (`token`),
/// whether it waits for a commit, and its hold on the log.
struct Task {
    s: Shipper,
    token: u64,
    idle: bool,
    calling: bool,
    hold: Option<u64>,
}

/// An election under way: by hand (`reply`: the call to answer) or on its
/// own; asking the copies `Hello` (by hand), then whether they would vote,
/// then for their votes.
struct Election {
    c: Campaign,
    /// What the copies said to `Hello`, by hand.
    asked: Option<Asked>,
    phase: Phase,
    round: u64,
    answers: Vec<Option<Result<Resp, String>>>,
    left: usize,
    reply: Option<(usize, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Hello,
    Pre,
    Vote,
}

impl Copy {
    pub fn new(i: usize, cfg: &Config, r: &mut Rng) -> Copy {
        let rate = 1_000_000 - cfg.drift_ppm + r.below(2 * cfg.drift_ppm + 1);
        Copy {
            url: format!("s{i}"),
            id: format!("{:016x}", r.u64()),
            up: false,
            inc: 0,
            rate,
            kept: None,
            db: ModelDb::new(cfg.sync, cfg.ship_max).refusing_older_terms(cfg.bugs.contains(&Bug::RefusesOlderTerms)),
            role: Role::Down,
            failover: None,
            election: None,
            rounds: 0,
        }
    }
}

impl Sim {
    fn urls(&self) -> Vec<String> {
        self.copies.iter().map(|c| c.url.clone()).collect()
    }

    fn opts(&self) -> PrimaryOptions {
        let o = PrimaryOptions { acks: self.cfg.acks, call_timeout: self.cfg.call_timeout, copy_retry: self.cfg.copy_retry, retry_max: self.cfg.retry_max, ..PrimaryOptions::default() };
        failover_tuned(o, self.cfg.failover_after)
    }

    /// Keeps `k` beside copy `i`'s database (durable at once, as
    /// `save_json` syncs it).
    fn keep(&mut self, i: usize, k: &Kept) {
        self.copies[i].kept = Some(k.clone());
    }

    /// Starts copy `i`: the first time, copy 0 as the primary of its shard
    /// (`--replicas`) and the others as replicas (`--replica`); again, in
    /// the part it keeps.
    pub(crate) fn boot(&mut self, i: usize) {
        let now = self.clock(i);
        let first = self.copies[i].kept.is_none();
        let c = &mut self.copies[i];
        c.up = true;
        c.inc += 1;
        c.election = None;
        let kept = c.kept.clone().unwrap_or_else(|| Kept::new(c.id.clone()));
        if first && i == 0 {
            let others: Vec<String> = self.urls().into_iter().skip(1).collect();
            let (core, save) = Primary::new(kept, &others, Opts::of(&self.opts()), false);
            if let Some(k) = save {
                self.keep(i, &k);
            }
            self.become_primary(i, core);
        } else {
            // The first time with --replica; again in the part it keeps.
            let flags = first && !self.cfg.bugs.contains(&Bug::WaitsForAFirstCall);
            match shard::restart(kept.role.as_ref(), kept.replaced_by.is_some(), true) {
                Some(Restart::Primary { .. }) => unreachable!("automatic failover starts a primary again as a replica"),
                _ => self.become_replica(i, kept, flags),
            }
        }
        let after = self.cfg.failover_after;
        let jitter = self.rng.part_of(after);
        let f = Failover::new(after, now, jitter);
        let every = f.every();
        self.copies[i].failover = Some(f);
        self.timer(i, every, Timer::Failover);
    }

    fn become_replica(&mut self, i: usize, kept: Kept, flags: bool) {
        let (r, save) = Replica::open(kept, self.clock(i), flags);
        if let Some(k) = save {
            self.keep(i, &k);
        }
        self.copies[i].role = Role::Replica(r);
    }

    fn become_primary(&mut self, i: usize, core: Primary) {
        let ships = core.shippers().into_iter().map(|s| Task { s, token: 0, idle: false, calling: false, hold: None }).collect();
        self.copies[i].role = Role::Primary(Box::new(Run { core, ships, pending: Vec::new() }));
        let n = self.copies[i].role_primary().map_or(0, |r| r.ships.len());
        for k in 0..n {
            self.drive(i, k, Ship::Again);
        }
    }

    pub(crate) fn crash(&mut self, i: usize) {
        let replayed = self.rng.below(self.cfg.ship_max as u64 + 1);
        let c = &mut self.copies[i];
        c.up = false;
        c.inc += 1;
        c.role = Role::Down;
        c.election = None;
        c.failover = None;
        c.db.crash(replayed);
        self.history.lost_memory(i);
    }

    /// A primary that another replaced becomes a replica of it.
    fn step_down(&mut self, i: usize) {
        let Role::Primary(run) = std::mem::replace(&mut self.copies[i].role, Role::Down) else { return };
        for t in &run.ships {
            if let Some(h) = t.hold {
                self.copies[i].db.release_log(h);
            }
        }
        let term = run.core.replaced_by();
        self.note(|| format!("s{i} steps down: a primary of term {term} replaced it"));
        // Its waiting writes were answered when it was fenced.
        let kept = self.copies[i].kept.clone().expect("a primary keeps its part");
        self.become_replica(i, kept, false);
    }

    /// The writes waiting on copy `i`'s primary that may be answered now.
    fn settle_writes(&mut self, i: usize) {
        let Role::Primary(run) = &mut self.copies[i].role else { return };
        let mut done = Vec::new();
        run.pending.retain(|&(seq, write, call, caller)| match run.core.acked(seq) {
            Some(a) => {
                done.push((seq, write, call, caller, a));
                false
            }
            None => true,
        });
        for (seq, write, call, caller, a) in done {
            self.answer_write(i, seq, write, call, caller, a);
        }
    }

    fn answer_write(&mut self, i: usize, seq: u64, write: u64, call: u64, caller: usize, a: Result<(), ApiError>) {
        if a.is_ok() {
            let at = self.copies[i].db.txns[seq as usize - 1];
            debug_assert_eq!(at.write, write);
            self.history.acknowledged(write, at.seq, i, self.now);
        }
        self.answer(i, caller, call, Resp::Write(a.map(|_| seq)));
    }

    fn election_copies(&self) -> Vec<String> {
        self.urls()
    }

    pub(crate) fn copy_request(&mut self, i: usize, from: usize, call: u64, req: Req) {
        let now = self.clock(i);
        let n = self.copies[i].db.numbering.clone();
        let sticky = self.cfg.failover_after;
        match req {
            Req::Hello => {
                let c = &self.copies[i];
                let (role, replaced) = match &c.role {
                    Role::Primary(r) => (pb::Role::Primary, r.core.replaced_by()),
                    Role::Replica(_) => (pb::Role::Replica, 0),
                    Role::Down => (pb::Role::Standalone, 0),
                };
                let h = pb::HelloResponse { role: role as i32, term: n.term, seq_no: n.seq, last_term: n.last().term, replaced_by_term: replaced, ..Default::default() };
                self.answer(i, from, call, Resp::Hello(h));
            }
            Req::Vote(r) => {
                let mut step_down = false;
                let a = match &mut self.copies[i].role {
                    Role::Replica(rep) => {
                        let (a, save) = rep.vote(&r, &n, now, sticky);
                        if let Some(k) = save {
                            rep.kept = k.clone();
                            self.copies[i].kept = Some(k);
                            let (t, c) = (r.term, r.candidate.clone());
                            self.note(|| format!("s{i} votes for {c} in term {t}"));
                        }
                        Ok(a)
                    }
                    Role::Primary(run) => {
                        let (a, save) = run.core.vote(&r, &n);
                        if let Some(k) = save {
                            let fenced = run.core.gave_vote(k, r.term, n.term);
                            self.copies[i].kept = Some(fenced.unwrap_or_else(|| self.copies[i].role_primary().expect("primary").core.kept.clone()));
                            step_down = true;
                            let (t, c) = (r.term, r.candidate.clone());
                            self.note(|| format!("s{i}, the primary, votes for {c} in term {t} and steps down"));
                        }
                        Ok(a)
                    }
                    Role::Down => Err(ApiError { status: 409, message: "not replicated".into() }),
                };
                self.answer(i, from, call, Resp::Vote(a));
                if step_down {
                    self.settle_writes(i);
                    self.step_down(i);
                }
            }
            Req::Replicate(r) => {
                let a = match &mut self.copies[i].role {
                    Role::Replica(rep) => match rep.take(r.term, &r.primary, r.member, &n) {
                        Err(e) => Err(e),
                        Ok(Take::Answer(a)) => Ok(a),
                        Ok(Take::Apply { kept }) => {
                            if let Some(k) = &kept {
                                self.copies[i].kept = Some(k.clone());
                            }
                            let Role::Replica(rep) = &mut self.copies[i].role else { unreachable!() };
                            rep.took(kept, now);
                            let applied = self.copies[i].db.apply_shipped(pb_prev(&r), &r.txns);
                            let db_term = self.copies[i].db.numbering.term;
                            let Role::Replica(rep) = &mut self.copies[i].role else { unreachable!() };
                            rep.applied(r.term, r.behind, r.global_checkpoint, applied, db_term)
                        }
                    },
                    Role::Primary(run) => {
                        let (a, kept) = run.core.shipped_to(&r.primary, r.term, &n);
                        if let Some(k) = kept {
                            self.copies[i].kept = Some(k);
                            self.settle_writes(i);
                        }
                        a
                    }
                    Role::Down => Err(ApiError { status: 409, message: "not replicated".into() }),
                };
                self.answer(i, from, call, Resp::Replicate(a));
            }
            Req::Restore { term, primary, copy } => {
                let a = match &mut self.copies[i].role {
                    Role::Replica(rep) => match rep.take(term, &primary, false, &n) {
                        Err(e) => Err(e),
                        Ok(Take::Answer(a)) => Ok(a),
                        Ok(Take::Apply { kept }) => {
                            if let Some(k) = &kept {
                                self.copies[i].kept = Some(k.clone());
                            }
                            let Role::Replica(rep) = &mut self.copies[i].role else { unreachable!() };
                            rep.took(kept, now);
                            rep.copying();
                            let installed = self.copies[i].db.install_copy(&copy);
                            self.history.lost_memory(i);
                            let db_term = self.copies[i].db.numbering.term;
                            let Role::Replica(rep) = &mut self.copies[i].role else { unreachable!() };
                            installed.map(|at| rep.installed(at, term, db_term))
                        }
                    },
                    Role::Primary(run) => {
                        let (a, kept) = run.core.shipped_to(&primary, term, &n);
                        if let Some(k) = kept {
                            self.copies[i].kept = Some(k);
                            self.settle_writes(i);
                        }
                        a
                    }
                    Role::Down => Err(ApiError { status: 409, message: "not replicated".into() }),
                };
                if a.is_ok() {
                    self.history.stats.copies += 1;
                }
                self.answer(i, from, call, Resp::Replicate(a));
            }
            Req::Write { id } => {
                let admitted = match &self.copies[i].role {
                    Role::Primary(run) => run.core.admit(n.term, now),
                    _ => Err(ApiError { status: 409, message: "this server is a replica: it applies its primary's writes and takes none from clients; write to the primary".into() }),
                };
                if let Err(e) = admitted {
                    return self.answer(i, from, call, Resp::Write(Err(e)));
                }
                let seq = self.copies[i].db.commit(id).seq;
                let timeout = self.cfg.write_timeout;
                let Role::Primary(run) = &mut self.copies[i].role else { unreachable!() };
                run.pending.push((seq, id, call, from));
                self.timer(i, timeout, Timer::Deadline { seq });
                self.wake_idle(i);
                self.settle_writes(i);
            }
            Req::Read { sent } => {
                let a = match &self.copies[i].role {
                    Role::Primary(run) => run.core.readable(now, n.term).map(|_| true),
                    Role::Replica(rep) => rep.readable().map(|_| false),
                    Role::Down => Err(ApiError { status: 503, message: "down".into() }),
                };
                let a = match a {
                    Ok(primary) => {
                        if primary {
                            self.history.read_on_primary(i, sent, &self.copies[i].db, self.now);
                        }
                        Ok(n.seq)
                    }
                    Err(e) => Err(e),
                };
                self.answer(i, from, call, Resp::Read(a));
            }
            Req::Promote => {
                let a = match (&self.copies[i].role, self.copies[i].election.is_some()) {
                    (Role::Replica(_), false) => Ok(()),
                    (Role::Replica(_), true) => Err(ApiError { status: 409, message: "an election is under way".into() }),
                    _ => Err(ApiError { status: 409, message: "this server is not a replica (--replica): only a replica is promoted".into() }),
                };
                match a {
                    Err(e) => self.answer(i, from, call, Resp::Promote(Err(e))),
                    Ok(()) => {
                        self.note(|| format!("s{i} is asked to be promoted by hand"));
                        self.history.stats.promotions += 1;
                        self.elect(i, true, Some((from, call)));
                    }
                }
            }
        }
    }

    pub(crate) fn copy_timer(&mut self, i: usize, t: Timer) {
        match t {
            Timer::Failover => {
                self.fail_over(i);
                let every = self.copies[i].failover.as_ref().map_or(Duration::from_millis(100), |f| f.every());
                self.timer(i, every, Timer::Failover);
            }
            Timer::Ship { k, token } => {
                let live = self.copies[i].role_primary().and_then(|r| r.ships.get(k)).is_some_and(|t| t.token == token && !t.calling);
                if live {
                    self.drive(i, k, Ship::Again);
                }
            }
            Timer::Deadline { seq } => {
                let Role::Primary(run) = &mut self.copies[i].role else { return };
                let Some(p) = run.pending.iter().position(|w| w.0 == seq) else { return };
                let (seq, write, call, caller) = run.pending.remove(p);
                let a = run.core.deadline(seq);
                self.answer_write(i, seq, write, call, caller, a);
                self.settle_writes(i);
            }
            Timer::Call(_) | Timer::Client(_) => {}
        }
    }

    /// One look of automatic failover ([`Failover`]): a replaced primary
    /// steps down; a replica whose primary is silent asks to be elected.
    fn fail_over(&mut self, i: usize) {
        let now = self.clock(i);
        let copies = self.election_copies().len();
        match &self.copies[i].role {
            Role::Primary(run) if run.core.replaced_by() > 0 => self.step_down(i),
            Role::Replica(rep) => {
                let silent = rep.silent_for(now);
                let due = self.copies[i].election.is_none() && self.copies[i].failover.as_ref().is_some_and(|f| f.due(copies, silent, now));
                if due {
                    let jitter = self.rng.part_of(self.cfg.failover_after);
                    if let Some(f) = &mut self.copies[i].failover {
                        f.started(now, jitter);
                    }
                    self.note(|| format!("s{i} hears nothing from its primary for {} ms: asks to be elected", silent.as_millis()));
                    self.elect(i, false, None);
                }
            }
            _ => {}
        }
    }

    /// Starts an election of replica `i`: by hand (a handover, answering
    /// `reply`) or on its own.
    fn elect(&mut self, i: usize, by_hand: bool, reply: Option<(usize, u64)>) {
        let n = self.copies[i].db.numbering.clone();
        let Role::Replica(rep) = &self.copies[i].role else { return };
        let c = rep.campaign(&n, by_hand);
        let phase = if by_hand { Phase::Hello } else { Phase::Pre };
        self.copies[i].election = Some(Election { c, asked: None, phase, round: 0, answers: Vec::new(), left: 0, reply });
        self.ask_round(i, phase);
    }

    fn ask_round(&mut self, i: usize, phase: Phase) {
        let copies = self.election_copies();
        self.copies[i].rounds += 1;
        let round = self.copies[i].rounds;
        let last = self.copies[i].db.last();
        let timeout = match self.copies[i].election.as_ref().is_some_and(|e| e.reply.is_some()) {
            true => self.cfg.call_timeout,
            false => self.copies[i].failover.as_ref().map_or(self.cfg.call_timeout, |f| f.call_timeout(self.cfg.call_timeout)),
        };
        let Some(e) = &mut self.copies[i].election else { return };
        e.phase = phase;
        e.round = round;
        e.answers = vec![None; copies.len()];
        e.left = copies.len();
        let req = match phase {
            Phase::Hello => Req::Hello,
            Phase::Pre => Req::Vote(e.c.ask(last, true)),
            Phase::Vote => Req::Vote(e.c.ask(last, false)),
        };
        for (idx, url) in copies.iter().enumerate() {
            let to = self.copies.iter().position(|c| c.url == *url).expect("a copy");
            self.call(i, to, req.clone(), timeout, Purpose::Elect { round, idx });
        }
    }

    pub(crate) fn copy_answer(&mut self, i: usize, p: Purpose, a: Result<Resp, String>) {
        match p {
            Purpose::Ship { k, gen } => self.ship_answer(i, k, gen, a),
            Purpose::Elect { round, idx } => {
                let Some(e) = &mut self.copies[i].election else { return };
                if e.round != round || e.answers[idx].is_some() {
                    return;
                }
                e.answers[idx] = Some(a);
                e.left -= 1;
                if e.left == 0 {
                    self.next_round(i);
                }
            }
            Purpose::Coord { .. } => {}
        }
    }

    fn end_election(&mut self, i: usize, a: Result<(), ApiError>) {
        let Some(e) = self.copies[i].election.take() else { return };
        if let Err(err) = &a {
            let m = err.message.clone();
            self.note(|| format!("s{i} is not elected: {m}"));
        }
        if let Some((to, call)) = e.reply {
            self.answer(i, to, call, Resp::Promote(a));
        }
    }

    fn next_round(&mut self, i: usize) {
        let copies = self.election_copies();
        let n = self.copies[i].db.numbering.clone();
        let Some(e) = &mut self.copies[i].election else { return };
        let answers = std::mem::take(&mut e.answers).into_iter().map(|a| a.expect("every copy answered"));
        match e.phase {
            Phase::Hello => {
                let said: Vec<(String, Result<pb::HelloResponse, String>)> = copies
                    .iter()
                    .cloned()
                    .zip(answers.map(|a| match a {
                        Ok(Resp::Hello(h)) => Ok(h),
                        Ok(r) => Err(format!("{r:?}")),
                        Err(e) => Err(e),
                    }))
                    .collect();
                match shard::promote_check(&n, &said, false) {
                    Err(err) => self.end_election(i, Err(err)),
                    Ok(asked) => {
                        e.asked = Some(asked);
                        self.ask_round(i, Phase::Pre)
                    }
                }
            }
            Phase::Pre | Phase::Vote => {
                let votes: Vec<Result<pb::VoteResponse, String>> = answers
                    .map(|a| match a {
                        Ok(Resp::Vote(Ok(v))) => Ok(v),
                        Ok(Resp::Vote(Err(e))) => Err(e.message),
                        Ok(r) => Err(format!("{r:?}")),
                        Err(e) => Err(e),
                    })
                    .collect();
                let tally = e.c.tally(&copies, votes);
                let (c, phase) = (e.c.clone(), e.phase);
                let heard = !self.cfg.bugs.contains(&Bug::AsksAKnownTerm);
                if let (Role::Replica(rep), true) = (&mut self.copies[i].role, heard) {
                    rep.heard(&tally);
                }
                if phase == Phase::Pre {
                    if let Err(err) = c.pre_voted(&tally) {
                        return self.end_election(i, Err(err));
                    }
                    let Role::Replica(rep) = &mut self.copies[i].role else { return self.end_election(i, Err(ApiError::internal("not a replica"))) };
                    match c.vote_self(rep, &n) {
                        Err(err) => self.end_election(i, Err(err)),
                        Ok(k) => {
                            rep.kept = k.clone();
                            self.copies[i].kept = Some(k);
                            self.ask_round(i, Phase::Vote);
                        }
                    }
                    return;
                }
                let elected = match c.voted(tally) {
                    Err(err) => return self.end_election(i, Err(err)),
                    Ok(e) => e,
                };
                let acks = self.cfg.acks;
                let Role::Replica(rep) = &self.copies[i].role else { return self.end_election(i, Err(ApiError::internal("not a replica"))) };
                let (term, kept) = match rep.take_over(Some(elected.term), &n, &elected.others, acks) {
                    Err(err) => return self.end_election(i, Err(err)),
                    Ok(t) => t,
                };
                if let Err(err) = self.copies[i].db.begin_term(term) {
                    return self.end_election(i, Err(err));
                }
                self.keep(i, &kept);
                let others = elected.others.clone();
                let by_hand = self.copies[i].election.as_ref().is_some_and(|e| e.reply.is_some());
                self.note(|| format!("s{i} is elected the primary of term {term}{}, shipping to {others:?}", if by_hand { " by hand" } else { "" }));
                self.history.began(i, term, &self.copies[i].db, self.now);
                let opts = Opts::of(&self.opts());
                let (mut core, save) = Primary::new(kept, &others, opts.clone(), false);
                if let Some(k) = save {
                    self.keep(i, &k);
                }
                let asked = self.copies[i].election.as_ref().and_then(|e| e.asked.as_ref());
                let hold = match (by_hand, self.cfg.bugs.contains(&Bug::TakesWritesInALease)) {
                    (true, false) => shard::handover_hold(asked, &elected, opts.lease),
                    _ => Duration::ZERO,
                };
                if !hold.is_zero() {
                    core.writes_from = Some(self.clock(i) + hold);
                    self.note(|| format!("s{i} takes writes in {} ms, once the lease of the primary it replaced has run out", hold.as_millis()));
                }
                self.become_primary(i, core);
                self.end_election(i, Ok(()));
            }
        }
    }

    /// Wakes copy `i`'s shippers that wait for a commit.
    fn wake_idle(&mut self, i: usize) {
        let Some(run) = self.copies[i].role_primary() else { return };
        let idle: Vec<usize> = run.ships.iter().enumerate().filter(|(_, t)| t.idle && !t.calling).map(|(k, _)| k).collect();
        for k in idle {
            self.drive(i, k, Ship::Again);
        }
    }

    /// Runs replica `k`'s shipper on copy `i` from `step` until it waits.
    fn drive(&mut self, i: usize, k: usize, mut step: Ship) {
        let id = self.copies[i].id.clone();
        loop {
            let now = self.clock(i);
            let c = &mut self.copies[i];
            let Role::Primary(run) = &mut c.role else { return };
            let n = c.db.numbering.clone();
            let task = &mut run.ships[k];
            task.token += 1;
            task.idle = false;
            step = match step {
                Ship::Again => run.core.poll(k, &mut task.s, &n, now),
                Ship::Read { from } => {
                    let log = c.db.log_from(from, self.cfg.batch);
                    run.core.read(k, &mut task.s, &n, log, now)
                }
                Ship::Call(req) => {
                    task.calling = true;
                    let gen = task.s.gen;
                    let to = k_to_copy(&self.copies, &self.copies[i], k);
                    let timeout = self.cfg.call_timeout;
                    self.call(i, to, Req::Replicate(req), timeout, Purpose::Ship { k, gen });
                    return;
                }
                Ship::Copy { hold_from, term } => {
                    if let Some(h) = task.hold.take() {
                        c.db.release_log(h);
                    }
                    let h = c.db.hold_log(hold_from);
                    let Role::Primary(run) = &mut c.role else { return };
                    let task = &mut run.ships[k];
                    task.hold = Some(h);
                    task.calling = true;
                    let gen = task.s.gen;
                    let copy = c.db.checkpoint();
                    let to = k_to_copy(&self.copies, &self.copies[i], k);
                    let timeout = self.cfg.call_timeout;
                    self.call(i, to, Req::Restore { term, primary: id.clone(), copy }, timeout, Purpose::Ship { k, gen });
                    return;
                }
                Ship::Sleep(d) => {
                    let token = task.token;
                    self.timer(i, d, Timer::Ship { k, token });
                    return;
                }
                Ship::Idle(d) => {
                    task.idle = true;
                    let token = task.token;
                    self.timer(i, d, Timer::Ship { k, token });
                    return;
                }
                Ship::Stop => return,
            };
            let c = &mut self.copies[i];
            let Role::Primary(run) = &mut c.role else { return };
            let task = &mut run.ships[k];
            if !task.s.holds() {
                if let Some(h) = task.hold.take() {
                    c.db.release_log(h);
                }
            }
        }
    }

    fn ship_answer(&mut self, i: usize, k: usize, gen: u64, a: Result<Resp, String>) {
        let n = self.copies[i].db.numbering.clone();
        let c = &mut self.copies[i];
        let Role::Primary(run) = &mut c.role else { return };
        let Some(task) = run.ships.get_mut(k).filter(|t| t.s.gen == gen && t.calling) else { return };
        task.calling = false;
        let answer = match a {
            Ok(Resp::Replicate(Ok(r))) => Ok(r),
            Ok(Resp::Replicate(Err(e))) => Err(e.message),
            Ok(r) => Err(format!("{r:?}")),
            Err(e) => Err(e),
        };
        let copy = task.hold.is_some() && task.s.holds() && is_copy(task);
        let (step, kept) = match copy {
            true => run.core.copied(k, &mut task.s, answer, &n),
            false => run.core.answered(k, &mut task.s, answer, &n),
        };
        if let Some(kept) = kept {
            self.copies[i].kept = Some(kept);
        }
        self.settle_writes(i);
        if self.copies[i].role_primary().is_some() {
            let c = &mut self.copies[i];
            if let Role::Primary(run) = &mut c.role {
                let task = &mut run.ships[k];
                if !task.s.holds() {
                    if let Some(h) = task.hold.take() {
                        c.db.release_log(h);
                    }
                }
            }
            self.drive(i, k, step);
        }
    }
}

/// Whether the call `t` waits for is a copy: it holds the log from the
/// copy on and has not heard back.
fn is_copy(t: &Task) -> bool {
    t.s.sending_copy()
}

/// The copy that replica `k` of copy `p`'s primary is.
fn k_to_copy(copies: &[Copy], p: &Copy, k: usize) -> usize {
    let Role::Primary(run) = &p.role else { return 0 };
    let url = &run.core.peers[k].url;
    copies.iter().position(|c| c.url == *url).expect("a replica is a copy")
}

fn pb_prev(r: &pb::ReplicateRequest) -> strata::storage::SeqNo {
    strata::storage::SeqNo { term: r.prev_term, seq: r.prev_seq }
}

impl Copy {
    pub fn role_primary(&self) -> Option<&Run> {
        match &self.role {
            Role::Primary(r) => Some(r),
            _ => None,
        }
    }

    /// The primary it runs as, if any, and whether another replaced it.
    pub fn primary(&self) -> Option<&Primary> {
        self.role_primary().map(|r| &r.core)
    }
}
