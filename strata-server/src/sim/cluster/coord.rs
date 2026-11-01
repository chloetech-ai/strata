//! A coordinator in the cluster simulator, and its clients: they write and
//! delete keys and read them, routed by the coordinator's map as
//! [`crate::coord::Coordinator`] routes them, and go again by a newer map
//! when a shard refuses one routed by a map it left
//! ([`crate::coord::Coordinator::route_again`]). The coordinator takes up
//! newer maps from the servers (its watcher, and when a shard refuses),
//! gives its map to a server it speaks to that holds another, changes the
//! map with [`Change`] and moves slots with [`MoveRun`].

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use super::server::slot_of;
use super::{server_of, url, Ask, Req, Resp, Sim, Time, Timer, Value};
use crate::coord::{plan_even, MOVE_SLOTS, ROUTE_TRIES};
use crate::cores::slots::{self as core, Change, ChangeStep, Edit, MoveRun, MoveStep};
use crate::service::{NO_MAP_YET, OLDER_THAN_REQUEST, OLDER_THAN_SHARD};
use crate::slots::SlotMap;
use crate::ApiError;

/// How often a coordinator takes up the map the servers hold.
pub(crate) const WATCH_EVERY: Duration = Duration::from_secs(2);

/// How long a server's answer to `Hello` is trusted.
const HELLO_EVERY: Time = 60_000_000;

/// Moves a job takes at most here, to keep a run's jobs short.
const JOB_MOVES: usize = 4;

/// What a coordinator does with the answer to a call.
#[derive(Clone, Debug)]
pub(crate) enum Purpose {
    /// A sync's `Hello` to a server, and its ask for the newest map.
    Sync,
    SyncMap,
    /// `Hello` to a server not asked for a while, and giving it the map it
    /// lacks, before `then`.
    Speak(Box<Then>),
    SpeakGive(Box<Then>),
    /// Round `round` of client `c`'s operation, to shard `shard`.
    Client { c: usize, round: u32, shard: usize },
    /// Giving the change's map; the map a server holds that refused it.
    Give,
    GiveHeld { err: ApiError },
    /// Step `round` of the job.
    Job { round: u64, what: JobCall },
    /// A hold asked for again: its answer is not waited for.
    Renew,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum JobCall {
    Cleanup,
    Export,
    Moved,
    Hold,
    Thaw,
}

/// A call to make once its server was spoken to.
#[derive(Clone, Debug)]
pub(crate) struct Then {
    req: Req,
    timeout: Duration,
    purpose: Purpose,
}

#[derive(Clone, Copy, Debug)]
enum Waiter {
    Client(usize),
    Change,
    Job,
}

/// A change of the map asked for, made from the map the servers hold.
#[derive(Clone, Debug)]
enum MapFn {
    Edit(Edit),
    AddShard(String),
    RemoveEmpty,
    Servers(usize),
}

/// A move, short: its shards, slots as runs, and state.
pub(crate) fn show(m: &crate::slots::Move) -> String {
    format!("{}->{} {:?} {}", m.from, m.to, m.ranges(), m.state.name())
}

impl std::fmt::Display for MapFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MapFn::Edit(Edit::Start { slots, to }) => write!(f, "start moving {:?} to {to}", crate::slots::runs(slots.iter().copied())),
            MapFn::Edit(Edit::Flip(m)) => write!(f, "flip {}", show(m)),
            MapFn::Edit(Edit::Abort(m)) => write!(f, "abort {}", show(m)),
            MapFn::Edit(Edit::End(m)) => write!(f, "end {}", show(m)),
            other => write!(f, "{other:?}"),
        }
    }
}

impl MapFn {
    fn apply(&self, m: &SlotMap) -> Result<SlotMap, String> {
        match self {
            MapFn::Edit(e) => e.apply(m),
            MapFn::AddShard(u) if m.shard_of_server(u).is_some() => Err(format!("{u} serves a shard already")),
            MapFn::AddShard(u) => m.add_shard(vec![u.clone()]),
            MapFn::RemoveEmpty => match (0..m.shards()).find(|&i| m.count(i) == 0) {
                Some(i) => m.remove_shard(i),
                None => Err("no shard is empty".into()),
            },
            MapFn::Servers(i) => {
                let i = i % m.shards();
                m.set_servers(i, m.replicas()[i].clone())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asker {
    Job,
    Operator,
}

struct Changing {
    change: Change,
    f: MapFn,
    asker: Asker,
}

struct Syncing {
    left: usize,
    newest: Option<(u64, usize)>,
    waiters: Vec<Waiter>,
}

struct Job {
    plan: VecDeque<(Vec<usize>, usize)>,
    run: Option<MoveRun>,
    /// The server the move's rows come from.
    src: Option<usize>,
    /// An export's rows being written to the shard they move to, and how
    /// many were.
    export: Vec<(i64, Value)>,
    at: usize,
    round: u64,
    finishing: bool,
}

#[derive(Clone, Debug)]
enum Kind {
    Write { key: i64, value: Value, op: u64 },
    Read { key: i64 },
}

#[derive(Clone, Debug)]
struct Op {
    kind: Kind,
    sent: Time,
    tries: u32,
    round: u32,
    routed: u64,
    /// Each shard's answer to a read, and how many are still to come.
    reads: BTreeMap<usize, Result<Value, ApiError>>,
    left: usize,
    /// A refusal waiting for a sync, to route the operation again.
    again: Option<ApiError>,
    /// Whether an earlier round may have written (no answer came).
    ambiguous: bool,
}

pub(crate) struct Coord {
    pub up: bool,
    pub inc: u64,
    pub map: SlotMap,
    /// When each server last answered `Hello` (none: ask it first).
    hello_at: Vec<Option<Time>>,
    clients: Vec<Option<Op>>,
    syncing: Option<Syncing>,
    changing: Option<Changing>,
    queued: VecDeque<(MapFn, Vec<String>, Asker)>,
    job: Option<Job>,
    renew_token: u64,
}

impl Coord {
    pub fn new(cfg: &super::Config, map: SlotMap, servers: usize) -> Coord {
        Coord {
            up: true,
            inc: 0,
            map,
            hello_at: vec![None; servers],
            clients: vec![None; cfg.clients],
            syncing: None,
            changing: None,
            queued: VecDeque::new(),
            job: None,
            renew_token: 0,
        }
    }
}

/// Whether a server's error may not be another's: the coordinator asks it
/// `Hello` again before its next call (`try_elsewhere`).
fn try_elsewhere(e: &ApiError) -> bool {
    !matches!(e.status, 400 | 401 | 403 | 404 | 409 | 429)
}

/// Whether a refusal is one a newer map answers
/// ([`crate::coord::Coordinator::route_again`]).
fn refused_by_map(e: &ApiError) -> bool {
    match e.status {
        409 => e.message.contains(OLDER_THAN_SHARD),
        503 => e.message.contains(OLDER_THAN_REQUEST) || e.message.contains(NO_MAP_YET),
        _ => false,
    }
}

impl Sim {
    fn node_of(&self, j: usize) -> usize {
        self.n_servers() + j
    }

    /// The server of shard `i` under coordinator `j`'s map.
    fn server_of_shard(&self, j: usize, i: usize) -> Option<usize> {
        self.coords[j].map.replicas().get(i).and_then(|rs| rs.first()).and_then(|u| server_of(u))
    }

    fn call_from(&mut self, j: usize, s: usize, req: Req, timeout: Duration, purpose: Purpose) {
        let from = self.node_of(j);
        self.call(from, s, req, timeout, purpose);
    }

    /// Calls server `s`, saying `Hello` first when not asked for a while,
    /// and giving it the map when it holds an older one.
    fn send_to(&mut self, j: usize, s: usize, req: Req, timeout: Duration, purpose: Purpose) {
        let fresh = self.coords[j].hello_at[s].is_some_and(|t| self.now < t + HELLO_EVERY);
        if fresh {
            return self.call_from(j, s, req, timeout, purpose);
        }
        self.call_from(j, s, Req::Hello, Duration::from_secs(2), Purpose::Speak(Box::new(Then { req, timeout, purpose })));
    }

    // ---- Clients ----

    pub(crate) fn client_tick(&mut self, j: usize, c: usize) {
        if self.quiet || self.coords[j].clients[c].is_some() {
            return;
        }
        let total = (self.cfg.coordinators * self.cfg.clients) as i64;
        let me = (j * self.cfg.clients + c) as i64;
        let kind = match self.rng.below(100) {
            0..=39 => Kind::Read { key: self.rng.below(self.cfg.keys as u64) as i64 },
            p => {
                // Each client writes its own keys, one operation at a time.
                let mine = (self.cfg.keys - me + total - 1) / total;
                let key = me + total * self.rng.below(mine.max(1) as u64) as i64;
                let op = self.op_id();
                let value = if p < 45 { None } else { Some(op) };
                self.history.sent(op, key, value, self.now);
                Kind::Write { key, value, op }
            }
        };
        self.coords[j].clients[c] = Some(Op { kind, sent: self.now, tries: 0, round: 0, routed: 0, reads: BTreeMap::new(), left: 0, again: None, ambiguous: false });
        self.dispatch(j, c);
    }

    /// Sends client `c`'s operation by the map routed by now.
    fn dispatch(&mut self, j: usize, c: usize) {
        let epoch = self.coords[j].map.epoch;
        let Some(op) = self.coords[j].clients[c].as_mut() else { return };
        op.round += 1;
        op.routed = epoch;
        let (round, routed, kind) = (op.round, op.routed, op.kind.clone());
        let timeout = self.cfg.call_timeout;
        match kind {
            Kind::Write { key, value, .. } => {
                let shard = self.coords[j].map.owner(slot_of(key));
                let Some(s) = self.server_of_shard(j, shard) else { return self.client_failed(j, c, ApiError::internal("a shard with no server")) };
                self.send_to(j, s, Req::Write { key, value, epoch: routed }, timeout, Purpose::Client { c, round, shard });
            }
            Kind::Read { key } => {
                let n = self.coords[j].map.shards();
                if let Some(op) = self.coords[j].clients[c].as_mut() {
                    op.reads.clear();
                    op.left = n;
                }
                for shard in 0..n {
                    let Some(s) = self.server_of_shard(j, shard) else { return self.client_failed(j, c, ApiError::internal("a shard with no server")) };
                    self.send_to(j, s, Req::Read { key, epoch: routed }, timeout, Purpose::Client { c, round, shard });
                }
            }
        }
    }

    fn client_answer(&mut self, j: usize, s: usize, c: usize, round: u32, shard: usize, a: Result<Resp, ApiError>) {
        let Some(op) = self.coords[j].clients[c].as_ref() else { return };
        if op.round != round {
            return;
        }
        let kind = op.kind.clone();
        match kind {
            Kind::Write { op: id, .. } => {
                let r = match a {
                    Ok(Resp::Wrote(r)) => r.map(|_| ()),
                    Ok(_) => Err(ApiError::internal("an answer to another call")),
                    Err(e) => {
                        // No answer: it may have been written.
                        if let Some(op) = self.coords[j].clients[c].as_mut() {
                            op.ambiguous = true;
                        }
                        Err(e)
                    }
                };
                match r {
                    Ok(()) => {
                        self.history.acked(id, self.now);
                        self.history.stats.writes_ok += 1;
                        self.client_done(j, c);
                    }
                    Err(e) => {
                        if try_elsewhere(&e) {
                            self.coords[j].hello_at[s] = None;
                        }
                        self.route_again(j, c, e);
                    }
                }
            }
            Kind::Read { key } => {
                let r = match a {
                    Ok(Resp::Read(r)) => r,
                    Ok(_) => Err(ApiError::internal("an answer to another call")),
                    Err(e) => Err(e),
                };
                if r.as_ref().is_err_and(try_elsewhere) {
                    self.coords[j].hello_at[s] = None;
                }
                let Some(op) = self.coords[j].clients[c].as_mut() else { return };
                op.reads.insert(shard, r);
                op.left -= 1;
                if op.left > 0 {
                    return;
                }
                let reads = std::mem::take(&mut op.reads);
                let sent = op.sent;
                if let Some(e) = reads.values().find_map(|r| r.as_ref().err()).cloned() {
                    return self.route_again(j, c, e);
                }
                let found: Vec<(usize, u64)> = reads.iter().filter_map(|(&i, r)| r.as_ref().ok().copied().flatten().map(|v| (i, v))).collect();
                if found.len() > 1 {
                    let routed = self.coords[j].clients[c].as_ref().map_or(0, |o| o.routed);
                    self.history.violation(format!("a read of key {key} routed by epoch {routed} found it on {} shards: {found:?}", found.len()));
                }
                let value = found.first().map(|&(_, v)| v);
                self.history.read(key, value, sent, self.now);
                self.history.stats.reads_ok += 1;
                self.client_done(j, c);
            }
        }
    }

    /// Whether client `c`'s operation, refused with `e`, goes again by a
    /// newer map: taken up from the servers if need be, or the shard is
    /// being given this one, and it waits a little longer each try.
    fn route_again(&mut self, j: usize, c: usize, e: ApiError) {
        let epoch = self.coords[j].map.epoch;
        let Some(op) = self.coords[j].clients[c].as_mut() else { return };
        if op.tries + 1 >= ROUTE_TRIES || !refused_by_map(&e) {
            return self.client_failed(j, c, e);
        }
        self.history.stats.routed_again += 1;
        let routed = op.routed;
        if epoch <= routed {
            op.again = Some(e);
            return self.sync(j, Some(Waiter::Client(c)));
        }
        op.tries += 1;
        self.dispatch(j, c);
    }

    fn client_synced(&mut self, j: usize, c: usize) {
        let epoch = self.coords[j].map.epoch;
        let Some(op) = self.coords[j].clients[c].as_mut() else { return };
        let Some(e) = op.again.take() else { return };
        if epoch > op.routed {
            op.tries += 1;
            return self.dispatch(j, c);
        }
        if e.status != 503 {
            return self.client_failed(j, c, e);
        }
        let wait = Duration::from_millis(5 << op.tries);
        op.tries += 1;
        let at = self.node_of(j);
        self.timer(at, wait, Timer::Again(c));
    }

    pub(crate) fn client_again(&mut self, j: usize, c: usize) {
        if self.coords[j].clients[c].is_some() {
            self.dispatch(j, c);
        }
    }

    fn client_failed(&mut self, j: usize, c: usize, e: ApiError) {
        let Some(op) = self.coords[j].clients[c].as_ref() else { return };
        match op.kind {
            Kind::Write { op: id, .. } => {
                match op.ambiguous {
                    true => self.history.unknown(id),
                    false => self.history.refused(id),
                }
                self.history.stats.writes_failed += 1;
            }
            Kind::Read { .. } => self.history.stats.reads_failed += 1,
        }
        self.note(|| format!("c{j}'s client {c} fails: {}", e.message));
        self.client_done(j, c);
    }

    /// Client `c`'s operation ended; the next follows a moment later.
    fn client_done(&mut self, j: usize, c: usize) {
        self.coords[j].clients[c] = None;
        let think = Duration::from_micros(self.rng.between(500, 30_000));
        let at = self.node_of(j);
        self.timer(at, think, Timer::Client(c));
    }

    // ---- The map ----

    /// Asks every server of the map which map it holds and takes up the
    /// newest; `then` waits for it.
    fn sync(&mut self, j: usize, then: Option<Waiter>) {
        if let Some(s) = &mut self.coords[j].syncing {
            s.waiters.extend(then);
            return;
        }
        let servers: Vec<usize> = self.coords[j].map.replicas().iter().flatten().filter_map(|u| server_of(u)).collect();
        self.coords[j].syncing = Some(Syncing { left: servers.len(), newest: None, waiters: then.into_iter().collect() });
        for s in servers {
            self.call_from(j, s, Req::Hello, Duration::from_secs(2), Purpose::Sync);
        }
    }

    fn synced(&mut self, j: usize) {
        let Some(s) = self.coords[j].syncing.take() else { return };
        for w in s.waiters {
            if !self.coords[j].up {
                return;
            }
            match w {
                Waiter::Client(c) => self.client_synced(j, c),
                Waiter::Change => self.change_synced(j),
                Waiter::Job => self.job_synced(j),
            }
        }
    }

    /// Routes by `map` from now on, if it is newer.
    fn install(&mut self, j: usize, map: SlotMap) {
        if map.epoch > self.coords[j].map.epoch {
            let e = map.epoch;
            self.note(|| format!("c{j} routes by map epoch {e}"));
            self.coords[j].map = map;
        }
    }

    pub(crate) fn watch(&mut self, j: usize) {
        self.sync(j, None);
        let at = self.node_of(j);
        self.timer(at, WATCH_EVERY, Timer::Watch);
    }

    // ---- Changes of the map ----

    fn change(&mut self, j: usize, f: MapFn, first: Vec<String>, asker: Asker) {
        if self.coords[j].changing.is_some() {
            self.coords[j].queued.push_back((f, first, asker));
            return;
        }
        let (mut change, step) = Change::new(&first);
        change.keeps_untaken = self.cfg.bugs.contains(&super::Bug::RoutesByAMapNoServerTook);
        change.skips_source = self.cfg.bugs.contains(&super::Bug::ChangesAroundTheSource);
        self.coords[j].changing = Some(Changing { change, f, asker });
        self.change_step(j, step);
    }

    fn change_step(&mut self, j: usize, step: ChangeStep) {
        match step {
            ChangeStep::Sync => self.sync(j, Some(Waiter::Change)),
            ChangeStep::Give { map, to } => {
                let timeout = self.cfg.call_timeout;
                for (shard, u) in to {
                    if let Some(s) = server_of(&u) {
                        self.call_from(j, s, Req::Give { map: map.clone(), shard }, timeout, Purpose::Give);
                    }
                }
            }
            ChangeStep::Done(r) => {
                let Some(ch) = self.coords[j].changing.take() else { return };
                let r = r.map(|(m, _)| m);
                match &r {
                    Ok(m) => {
                        self.history.stats.changes_made += 1;
                        let (e, f) = (m.epoch, ch.f.clone());
                        self.note(|| format!("c{j} changed the map to epoch {e}: {f}"));
                        self.install(j, m.clone());
                    }
                    Err(e) => {
                        self.history.stats.changes_failed += 1;
                        let (msg, f) = (e.message.clone(), ch.f.clone());
                        self.note(|| format!("c{j} did not change the map ({f}): {msg}"));
                    }
                }
                if ch.asker == Asker::Job {
                    self.job_changed(j, r);
                }
                if self.coords[j].up && self.coords[j].changing.is_none() {
                    if let Some((f, first, asker)) = self.coords[j].queued.pop_front() {
                        self.change(j, f, first, asker);
                    }
                }
            }
        }
    }

    fn change_synced(&mut self, j: usize) {
        let map = self.coords[j].map.clone();
        let Some(ch) = self.coords[j].changing.as_mut() else { return };
        let f = ch.f.clone();
        // The server of the move's shard `from` under the map synced to.
        let source = |m: &crate::slots::Move| map.replicas().get(m.from).and_then(|rs| rs.first()).cloned();
        let step = ch.change.synced(Some(&map), &|m| f.apply(m), &source);
        self.change_step(j, step);
    }

    fn change_answer(&mut self, j: usize, s: usize, g: core::Given) {
        let Some(ch) = self.coords[j].changing.as_mut() else { return };
        if let Some(step) = ch.change.answered(&url(s), g) {
            self.change_step(j, step);
        }
    }

    // ---- The operator's asks, and jobs of moves ----

    pub(crate) fn ask(&mut self, j: usize, ask: Ask) {
        match ask {
            Ask::Moves => {
                let map = self.coords[j].map.clone();
                if self.coords[j].job.is_some() || map.moving().is_some() {
                    return;
                }
                let plan = self.plan_moves(&map);
                if plan.is_empty() {
                    return;
                }
                let shown: Vec<String> = plan.iter().map(|(s, to)| format!("{:?} to {to}", crate::slots::runs(s.iter().copied()))).collect();
                self.note(|| format!("c{j} starts a job of {} moves: {shown:?}", plan.len()));
                self.coords[j].job = Some(Job { plan: plan.into(), run: None, src: None, export: Vec::new(), at: 0, round: 0, finishing: false });
                self.next_move(j);
            }
            Ask::AddShard => {
                let Some(s) = (self.cfg.shards..self.n_servers()).find(|&s| !self.spares_asked[s]) else { return };
                self.spares_asked[s] = true;
                self.change(j, MapFn::AddShard(url(s)), Vec::new(), Asker::Operator);
            }
            Ask::RemoveShard => self.change(j, MapFn::RemoveEmpty, Vec::new(), Asker::Operator),
            Ask::Servers => {
                let i = self.rng.below(8) as usize;
                self.change(j, MapFn::Servers(i), Vec::new(), Asker::Operator);
            }
            Ask::Finish => {
                if self.coords[j].job.is_some() {
                    return;
                }
                self.coords[j].job = Some(Job { plan: VecDeque::new(), run: None, src: None, export: Vec::new(), at: 0, round: 0, finishing: true });
                self.sync(j, Some(Waiter::Job));
            }
        }
    }

    /// Moves drawn on `map`: of some keys' slots from one shard to another,
    /// or a rebalance, or a drain ([`plan_even`]).
    fn plan_moves(&mut self, map: &SlotMap) -> Vec<(Vec<usize>, usize)> {
        let n = map.shards();
        if n < 2 {
            return Vec::new();
        }
        let chunk = [16, 64, MOVE_SLOTS, 1024][self.rng.below(4) as usize];
        let even = |empty| plan_even(map, empty, chunk).into_iter().take(JOB_MOVES).map(|(s, _, to)| (s, to)).collect();
        match self.rng.below(5) {
            0 => even(None),
            1 => {
                let i = self.rng.below(n as u64) as usize;
                even(Some(i))
            }
            _ => {
                let from = self.rng.below(n as u64) as usize;
                let keys: Vec<i64> = (0..self.cfg.keys).filter(|&k| map.owner(slot_of(k)) == from).collect();
                if keys.is_empty() {
                    return Vec::new();
                }
                let mut slots: Vec<usize> = (0..self.rng.between(1, 4)).map(|_| slot_of(*self.rng.pick(&keys))).collect();
                slots.sort_unstable();
                slots.dedup();
                let to = (from + 1 + self.rng.below(n as u64 - 1) as usize) % n;
                vec![(slots, to)]
            }
        }
    }

    fn next_move(&mut self, j: usize) {
        let Some(job) = self.coords[j].job.as_mut() else { return };
        let Some((slots, to)) = job.plan.pop_front() else {
            self.coords[j].job = None;
            return;
        };
        let (run, step) = MoveRun::new(slots, to);
        job.run = Some(run);
        job.src = None;
        self.job_step(j, step);
    }

    fn job_step(&mut self, j: usize, step: MoveStep) {
        let Some(job) = self.coords[j].job.as_mut() else { return };
        job.round += 1;
        let round = job.round;
        let src = job.src;
        let renewing = job.run.as_ref().is_some_and(MoveRun::renewing);
        let epoch = self.coords[j].map.epoch;
        let timeout = self.cfg.call_timeout;
        // A hold asked for again while the move says so.
        self.coords[j].renew_token += 1;
        if renewing {
            let token = self.coords[j].renew_token;
            let at = self.node_of(j);
            self.timer(at, self.cfg.hold_renew, Timer::Renew { token });
        }
        match step {
            MoveStep::Change { edit, first_from } => {
                let first = match (first_from, src) {
                    (true, Some(s)) => vec![url(s)],
                    _ => Vec::new(),
                };
                self.change(j, MapFn::Edit(edit), first, Asker::Job);
            }
            MoveStep::Source => {
                let from = self.coords[j].job.as_ref().and_then(|jb| jb.run.as_ref()).and_then(|r| r.moving()).map(|m| m.from);
                let s = from.and_then(|i| self.server_of_shard(j, i));
                let Some(job) = self.coords[j].job.as_mut() else { return };
                job.src = s;
                let Some(run) = job.run.as_mut() else { return };
                let step = run.found(s.map(|_| ()).ok_or_else(|| ApiError::internal("the shard the slots move from has no server")));
                self.job_step(j, step);
            }
            MoveStep::Cleanup { shard } => match self.server_of_shard(j, shard) {
                Some(s) => self.call_from(j, s, Req::Cleanup { epoch }, timeout, Purpose::Job { round, what: JobCall::Cleanup }),
                None => self.job_answer(j, round, JobCall::Cleanup, Err(ApiError::internal("a shard with no server"))),
            },
            MoveStep::Export { changes } => match src {
                Some(s) => self.call_from(j, s, Req::Export { changes, epoch }, timeout, Purpose::Job { round, what: JobCall::Export }),
                None => self.job_answer(j, round, JobCall::Export, Err(ApiError::internal("no source"))),
            },
            MoveStep::Hold => match src {
                Some(s) => self.call_from(j, s, Req::Freeze { thaw: false, epoch }, timeout, Purpose::Job { round, what: JobCall::Hold }),
                None => self.job_answer(j, round, JobCall::Hold, Err(ApiError::internal("no source"))),
            },
            MoveStep::Thaw => match src {
                Some(s) => self.call_from(j, s, Req::Freeze { thaw: true, epoch }, timeout, Purpose::Job { round, what: JobCall::Thaw }),
                None => self.job_answer(j, round, JobCall::Thaw, Err(ApiError::internal("no source"))),
            },
            MoveStep::Sync => self.sync(j, Some(Waiter::Job)),
            MoveStep::Done(r) => {
                let finishing = self.coords[j].job.as_ref().is_some_and(|jb| jb.finishing);
                let mv = self.coords[j].job.as_ref().and_then(|jb| jb.run.as_ref()).and_then(|r| r.moving().cloned());
                match &r {
                    Ok(true) => self.history.stats.moves_done += 1,
                    Ok(false) => self.history.stats.moves_given_up += 1,
                    Err(_) => self.history.stats.moves_failed += 1,
                }
                let shown = mv.as_ref().map(show).unwrap_or_default();
                self.note(|| format!("c{j}'s {} of {shown} ends: {r:?}", if finishing { "finish" } else { "move" }));
                match (r, finishing) {
                    (Ok(true), false) => self.next_move(j),
                    _ => self.coords[j].job = None,
                }
            }
        }
    }

    fn job_changed(&mut self, j: usize, r: Result<SlotMap, ApiError>) {
        let map = self.coords[j].map.clone();
        let Some(run) = self.coords[j].job.as_mut().and_then(|jb| jb.run.as_mut()) else { return };
        let step = run.changed(r, Some(&map));
        self.job_step(j, step);
    }

    fn job_synced(&mut self, j: usize) {
        let map = self.coords[j].map.clone();
        let Some(job) = self.coords[j].job.as_mut() else { return };
        let step = match job.run.as_mut() {
            Some(run) => run.synced(Some(&map)),
            None if job.finishing => match MoveRun::finish(&map) {
                Some((run, step)) => {
                    job.run = Some(run);
                    step
                }
                None => {
                    self.coords[j].job = None;
                    return;
                }
            },
            None => return,
        };
        self.job_step(j, step);
    }

    fn job_answer(&mut self, j: usize, round: u64, what: JobCall, a: Result<Resp, ApiError>) {
        let Some(job) = self.coords[j].job.as_mut() else { return };
        if job.round != round {
            return;
        }
        let Some(run) = job.run.as_mut() else { return };
        let step = match (what, a) {
            (JobCall::Cleanup, Ok(Resp::Cleaned(r))) => run.cleaned(r),
            (JobCall::Cleanup, Err(e)) => run.cleaned(Err(e)),
            (JobCall::Export, Ok(Resp::Exported(Ok(rows)))) => {
                job.export = rows;
                job.at = 0;
                return self.write_moved(j);
            }
            (JobCall::Export, Ok(Resp::Exported(Err(e))) | Err(e)) => run.exported(Err(e)),
            (JobCall::Moved, Ok(Resp::Wrote(Ok(_)))) => {
                job.at = (job.at + self.cfg.export_rows).min(job.export.len());
                return self.write_moved(j);
            }
            (JobCall::Moved, Ok(Resp::Wrote(Err(e))) | Err(e)) => run.exported(Err(e)),
            (JobCall::Hold, Ok(Resp::Frozen(r))) => run.held(r),
            (JobCall::Hold, Err(e)) => run.held(Err(e)),
            (JobCall::Thaw, _) => run.thawed(),
            (_, _) => run.exported(Err(ApiError::internal("an answer to another call"))),
        };
        self.job_step(j, step);
    }

    /// Writes the next rows of the export to the shard they move to, or
    /// tells the move how many it sent once all are.
    fn write_moved(&mut self, j: usize) {
        let epoch = self.coords[j].map.epoch;
        let Some(job) = self.coords[j].job.as_mut() else { return };
        let Some(run) = job.run.as_mut() else { return };
        if job.at >= job.export.len() {
            let n = job.export.len() as u64;
            self.history.stats.rows_moved += n;
            let step = run.exported(Ok((n, false)));
            return self.job_step(j, step);
        }
        let to = run.moving().map(|m| m.to);
        let rows = job.export[job.at..(job.at + self.cfg.export_rows).min(job.export.len())].to_vec();
        let round = job.round;
        match to.and_then(|i| self.server_of_shard(j, i)) {
            Some(s) => {
                let timeout = self.cfg.call_timeout;
                self.send_to(j, s, Req::Moved { rows, epoch }, timeout, Purpose::Job { round, what: JobCall::Moved });
            }
            None => self.job_answer(j, round, JobCall::Moved, Err(ApiError::internal("the shard the slots move to has no server"))),
        }
    }

    pub(crate) fn renew(&mut self, j: usize, token: u64) {
        if token != self.coords[j].renew_token {
            return;
        }
        let Some(job) = self.coords[j].job.as_ref() else { return };
        let (Some(s), true) = (job.src, job.run.as_ref().is_some_and(MoveRun::renewing)) else { return };
        let epoch = self.coords[j].map.epoch;
        let timeout = self.cfg.call_timeout;
        self.call_from(j, s, Req::Freeze { thaw: false, epoch }, timeout, Purpose::Renew);
        let at = self.node_of(j);
        self.timer(at, self.cfg.hold_renew, Timer::Renew { token });
    }

    // ---- Crashes, and answers ----

    pub(crate) fn coord_crash(&mut self, j: usize) {
        for c in 0..self.cfg.clients {
            if let Some(Op { kind: Kind::Write { op, .. }, .. }) = self.coords[j].clients[c].take() {
                self.history.unknown(op);
                self.history.stats.writes_failed += 1;
            }
        }
        let k = &mut self.coords[j];
        k.up = false;
        k.inc += 1;
        k.syncing = None;
        k.changing = None;
        k.queued.clear();
        k.job = None;
        k.renew_token += 1;
        k.hello_at.iter_mut().for_each(|h| *h = None);
    }

    pub(crate) fn coord_restart(&mut self, j: usize) {
        self.coords[j].up = true;
        let at = self.node_of(j);
        for c in 0..self.cfg.clients {
            let d = Duration::from_micros(self.rng.between(0, 50_000));
            self.timer(at, d, Timer::Client(c));
        }
        self.timer(at, WATCH_EVERY, Timer::Watch);
        self.sync(j, None);
    }

    pub(crate) fn coord_answer(&mut self, j: usize, s: usize, purpose: Purpose, a: Result<Resp, ApiError>) {
        match purpose {
            Purpose::Sync => {
                if let Ok(Resp::Hello { epoch }) = &a {
                    self.coords[j].hello_at[s] = Some(self.now);
                    let mine = self.coords[j].map.epoch;
                    if let Some(sy) = self.coords[j].syncing.as_mut() {
                        if *epoch > mine && sy.newest.is_none_or(|(e, _)| *epoch >= e) {
                            sy.newest = Some((*epoch, s));
                        }
                    }
                }
                let Some(sy) = self.coords[j].syncing.as_mut() else { return };
                sy.left -= 1;
                if sy.left > 0 {
                    return;
                }
                match sy.newest {
                    Some((_, k)) => self.call_from(j, k, Req::HeldMap, Duration::from_secs(5), Purpose::SyncMap),
                    None => self.synced(j),
                }
            }
            Purpose::SyncMap => {
                if let Ok(Resp::HeldMap(Some((m, _)))) = a {
                    self.install(j, m);
                }
                self.synced(j);
            }
            Purpose::Speak(then) => match a {
                Ok(Resp::Hello { epoch }) => {
                    let mine = self.coords[j].map.epoch;
                    if epoch > mine {
                        // It holds a newer map: the watcher takes it up.
                        self.sync(j, None);
                    } else if epoch != mine {
                        let Some(shard) = self.coords[j].map.shard_of_server(&url(s)) else { return self.coord_answer(j, s, then.purpose, Err(ApiError::internal("a server not in the map"))) };
                        let map = self.coords[j].map.clone();
                        return self.call_from(j, s, Req::Give { map, shard }, Duration::from_secs(2), Purpose::SpeakGive(then));
                    }
                    self.coords[j].hello_at[s] = Some(self.now);
                    self.call_from(j, s, then.req, then.timeout, then.purpose);
                }
                Ok(_) => self.coord_answer(j, s, then.purpose, Err(ApiError::internal("an answer to another call"))),
                Err(e) => self.coord_answer(j, s, then.purpose, Err(e)),
            },
            Purpose::SpeakGive(then) => match a {
                Ok(Resp::Given(Ok(_))) => {
                    self.coords[j].hello_at[s] = Some(self.now);
                    self.call_from(j, s, then.req, then.timeout, then.purpose);
                }
                Ok(Resp::Given(Err(e))) | Err(e) => self.coord_answer(j, s, then.purpose, Err(e)),
                Ok(_) => self.coord_answer(j, s, then.purpose, Err(ApiError::internal("an answer to another call"))),
            },
            Purpose::Client { c, round, shard } => self.client_answer(j, s, c, round, shard, a),
            Purpose::Give => {
                let next = self.coords[j].changing.as_ref().and_then(|ch| ch.change.next().cloned());
                let Some(next) = next else { return };
                match a {
                    Ok(Resp::Given(Ok(_))) => self.change_answer(j, s, core::Given::Took),
                    Ok(Resp::Given(Err(e))) if e.status == 409 => self.call_from(j, s, Req::HeldMap, Duration::from_secs(2), Purpose::GiveHeld { err: e }),
                    Ok(Resp::Given(Err(e))) | Err(e) => self.change_answer(j, s, core::given(Err(e), None, &next)),
                    Ok(_) => self.change_answer(j, s, core::Given::Missed("an answer to another call".into())),
                }
            }
            Purpose::GiveHeld { err } => {
                let next = self.coords[j].changing.as_ref().and_then(|ch| ch.change.next().cloned());
                let Some(next) = next else { return };
                let holds = match &a {
                    Ok(Resp::HeldMap(Some((m, _)))) => Some(m),
                    _ => None,
                };
                let g = core::given(Err(err), holds, &next);
                self.change_answer(j, s, g);
            }
            Purpose::Job { round, what } => self.job_answer(j, round, what, a),
            Purpose::Renew => {}
        }
    }
}
