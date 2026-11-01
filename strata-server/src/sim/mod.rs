//! A deterministic simulator of a shard's copies and its coordinators.
//!
//! A run is drawn from a seed ([`Config::of`]): three or five copies of a
//! shard with automatic failover, two coordinators whose clients write,
//! read and now and then promote a copy by hand, and faults. The network
//! delays, reorders and drops calls and cuts copies apart (both ways or
//! one way); copies crash and start again, their clocks run a little apart,
//! and, when a run does not sync its commits, a crash loses what was not
//! synced. The copies run the protocol's decisions as [`crate::cores`]
//! makes them, on a model of their databases ([`db`]); the simulator does
//! their I/O, one event at a time in virtual time, every choice drawn from
//! the seed, so that a seed replays exactly ([`Report::fingerprint`]).
//!
//! After the faults, the network heals, every copy starts again and the
//! clients stop; the run then checks ([`check`]) that
//!
//! * one copy began each term (a term has one primary);
//! * every write answered as acknowledged is held by every primary elected
//!   after the answer, and by every copy at the end (no answered write is
//!   lost);
//! * two copies holding a transaction of the same number and term hold
//!   the same history up to it;
//! * a primary serving a read holds every write acknowledged before the
//!   read was sent (its lease kept it from serving once another could be
//!   elected);
//! * the shard settles: one primary, every replica holding what it holds.
//!
//! What the simulator does not see: the database's own threads (flushes,
//! merges) and its files, which the fault-injection tests cover, and the
//! glue between the server and the cores until the server drives them.

pub mod check;
mod coord;
mod copy;
pub mod db;
pub mod rng;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::replication::Acks;
use crate::{pb, ApiError};
use check::History;
use db::Checkpoint;
use rng::Rng;

/// Virtual time: microseconds from the run's start.
pub type Time = u64;

fn us(d: Duration) -> Time {
    d.as_micros() as Time
}

/// What a run is made of, all of it drawn from its seed.
#[derive(Clone, Debug)]
pub struct Config {
    pub seed: u64,
    pub copies: usize,
    pub coordinators: usize,
    /// Clients of each coordinator, each one operation at a time.
    pub clients: usize,
    pub acks: Acks,
    /// `--failover-after-ms`.
    pub failover_after: Duration,
    /// A write's deadline on its primary.
    pub write_timeout: Duration,
    /// [`crate::replication::PrimaryOptions`]'s.
    pub call_timeout: Duration,
    pub copy_retry: Duration,
    pub retry_max: Duration,
    /// Transactions a ship log keeps, and a call ships at most.
    pub ship_max: usize,
    pub batch: usize,
    /// Whether commits are durable on return (`DbOptions::sync`).
    pub sync: bool,
    /// Calls lost while faults run, in parts per million.
    pub drop_ppm: u64,
    /// How far each clock's rate may be from true, in parts per million.
    pub drift_ppm: u64,
    /// Faults run for this long, then the network heals, the copies start
    /// again, and the run goes on for `settle`, its clients stopping
    /// `quiet` before the end.
    pub faults_for: Duration,
    pub settle: Duration,
    pub quiet: Duration,
    /// The mean time between crashes and between cuts of the network.
    pub crash_every: Duration,
    pub cut_every: Duration,
    /// The chance of a client's operation being a promotion by hand, in
    /// parts per million.
    pub promote_ppm: u64,
    /// Bugs put back, each as the server had it before the simulator
    /// found it: a run with one must find it ([`Bug`]).
    pub bugs: Vec<Bug>,
}

/// A bug the simulator found, put back with [`Config::bugs`] to check
/// that it still finds it (`strata-sim --bug NAME`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    /// A replica whose pre-vote a copy refused, knowing its term, asks for
    /// that term again ([`crate::cores::shard::Replica::heard`]): once a
    /// minority voted in a term for a candidate that lost, the copies
    /// holding the most never learn it and are never elected.
    AsksAKnownTerm,
    /// A replica elected by hand takes writes at once
    /// ([`crate::cores::shard::handover_hold`]): the primary it replaced,
    /// which did not hear of it, serves reads under its lease without them.
    TakesWritesInALease,
    /// A replica refuses a transaction of a term older than its database's
    /// term ([`db::ModelDb::apply_shipped`]), even one of the primary it
    /// follows: a copy that began a term or installed a copy of a primary
    /// that wrote nothing in it never takes what a newer primary holds of
    /// an older term.
    RefusesOlderTerms,
}

impl Bug {
    pub const ALL: [Bug; 3] = [Bug::AsksAKnownTerm, Bug::TakesWritesInALease, Bug::RefusesOlderTerms];

    pub fn name(self) -> &'static str {
        match self {
            Bug::AsksAKnownTerm => "asks-a-known-term",
            Bug::TakesWritesInALease => "takes-writes-in-a-lease",
            Bug::RefusesOlderTerms => "refuses-older-terms",
        }
    }

    pub fn of(name: &str) -> Option<Bug> {
        Bug::ALL.into_iter().find(|b| b.name() == name)
    }
}

impl Config {
    /// The run of seed `seed`.
    pub fn of(seed: u64) -> Config {
        let mut r = Rng::new(seed ^ 0x5eed_c0de);
        let failover_after = r.ms(300, 2000);
        let copy_retry = r.ms(200, 5000);
        Config {
            seed,
            copies: if r.below(4) == 0 { 5 } else { 3 },
            coordinators: 2,
            clients: r.between(1, 3) as usize,
            acks: if r.below(2) == 0 { Acks::All } else { Acks::Majority },
            failover_after,
            write_timeout: r.ms(500, 3000),
            call_timeout: if r.below(3) == 0 { Duration::from_secs(10) } else { r.ms(500, 5000) },
            copy_retry,
            retry_max: Duration::from_secs(2),
            ship_max: r.between(2, 64) as usize,
            batch: r.between(1, 16) as usize,
            sync: true,
            drop_ppm: r.below(20_000),
            drift_ppm: r.below(10_000),
            faults_for: Duration::from_secs(20),
            settle: Duration::from_secs(20) + copy_retry * 3,
            quiet: Duration::from_secs(5) + copy_retry * 2,
            crash_every: r.ms(1000, 8000),
            cut_every: r.ms(500, 6000),
            promote_ppm: r.below(5_000),
            bugs: Vec::new(),
        }
    }
}

/// What a run found.
#[derive(Clone, Debug)]
pub struct Report {
    pub seed: u64,
    /// What it broke of what the protocol promises; empty when nothing.
    pub violations: Vec<String>,
    pub stats: check::Stats,
    pub events: u64,
    /// A hash of every event in order: the same seed gives the same one.
    pub fingerprint: u64,
    /// With tracing on, what happened, a line per event worth reading.
    pub trace: Vec<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// A call to a server.
#[derive(Clone, Debug)]
pub(crate) enum Req {
    Hello,
    Vote(pb::VoteRequest),
    Replicate(pb::ReplicateRequest),
    Restore { term: u64, primary: String, copy: Checkpoint },
    Write { id: u64 },
    Read { sent: Time },
    Promote,
}

/// A server's answer.
#[derive(Clone, Debug)]
pub(crate) enum Resp {
    Hello(pb::HelloResponse),
    Vote(Result<pb::VoteResponse, ApiError>),
    Replicate(Result<pb::ReplicateResponse, ApiError>),
    Write(Result<u64, ApiError>),
    Read(Result<u64, ApiError>),
    Promote(Result<(), ApiError>),
}

#[derive(Debug)]
enum Msg {
    Req { call: u64, from: usize, req: Req },
    Resp { call: u64, resp: Resp },
    /// The server is down: the caller's connection is refused.
    Refused { call: u64 },
}

/// What a caller does with the answer to a call.
#[derive(Clone, Debug)]
pub(crate) enum Purpose {
    /// Replica `k`'s shipper of primary generation `gen`.
    Ship { k: usize, gen: u64 },
    /// Copy `idx` of an election's round `round`.
    Elect { round: u64, idx: usize },
    /// A coordinator's call for client `client` (none: a `Hello` of its
    /// own), to copy `k`.
    Coord { client: Option<usize>, k: usize },
}

#[derive(Debug)]
struct Call {
    from: usize,
    inc: u64,
    purpose: Purpose,
}

#[derive(Clone, Debug)]
pub(crate) enum Timer {
    Failover,
    /// Replica `k`'s shipper, if its wait is still `token`.
    Ship { k: usize, token: u64 },
    /// The deadline of a write the primary numbered `seq`.
    Deadline { seq: u64 },
    Call(u64),
    Client(usize),
}

#[derive(Debug)]
enum Event {
    Deliver { to: usize, msg: Msg },
    Timer { at: usize, inc: u64, timer: Timer },
    Crash(usize),
    Restart(usize),
    /// Calls from each of `from` to each of `to` are lost (and back, unless
    /// `one_way`), until `Heal`.
    Cut { from: Vec<usize>, to: Vec<usize>, one_way: bool },
    Heal,
    /// The faults end.
    Settle,
    /// The clients stop.
    Quiet,
}

/// A run under way.
pub struct Sim {
    pub cfg: Config,
    rng: Rng,
    now: Time,
    base: Instant,
    queue: BTreeMap<(Time, u64), Event>,
    next_event: u64,
    pub(crate) copies: Vec<copy::Copy>,
    coords: Vec<coord::Coord>,
    calls: BTreeMap<u64, Call>,
    next_call: u64,
    cut: BTreeSet<(usize, usize)>,
    faults: bool,
    quiet: bool,
    pub(crate) history: History,
    trace: Option<Vec<String>>,
    hash: u64,
    events: u64,
    next_write: u64,
}

/// Runs seed `cfg.seed` to its end, keeping a trace when `trace`.
pub fn run(cfg: Config, trace: bool) -> Report {
    let mut s = Sim::new(cfg, trace);
    s.start();
    let end = us(s.cfg.faults_for + s.cfg.settle);
    while let Some(((t, _), e)) = s.queue.pop_first() {
        if t > end {
            break;
        }
        s.now = t;
        s.handle(e);
    }
    s.now = end;
    s.history.settled(&s.copies, s.now);
    Report { seed: s.cfg.seed, violations: s.history.violations.clone(), stats: s.history.stats.clone(), events: s.events, fingerprint: s.hash, trace: s.trace.unwrap_or_default() }
}

impl Sim {
    fn new(cfg: Config, trace: bool) -> Sim {
        let mut rng = Rng::new(cfg.seed);
        let copies = (0..cfg.copies).map(|i| copy::Copy::new(i, &cfg, &mut rng)).collect();
        let coords = (0..cfg.coordinators).map(|_| coord::Coord::new(&cfg)).collect();
        Sim {
            history: History::new(cfg.copies),
            cfg,
            rng,
            now: 0,
            base: Instant::now(),
            queue: BTreeMap::new(),
            next_event: 0,
            copies,
            coords,
            calls: BTreeMap::new(),
            next_call: 0,
            cut: BTreeSet::new(),
            faults: true,
            quiet: false,
            trace: trace.then(Vec::new),
            hash: 0xcbf2_9ce4_8422_2325,
            events: 0,
            next_write: 0,
        }
    }

    /// Every node of the run: the copies, then the coordinators.
    fn nodes(&self) -> usize {
        self.cfg.copies + self.cfg.coordinators
    }

    fn start(&mut self) {
        for i in 0..self.cfg.copies {
            self.boot(i);
        }
        for j in 0..self.cfg.coordinators {
            for c in 0..self.cfg.clients {
                let at = self.rng.between(0, 50_000);
                self.at(at, Event::Timer { at: self.cfg.copies + j, inc: 0, timer: Timer::Client(c) });
            }
        }
        self.plan_faults();
    }

    /// The faults of the run, drawn up front so that the clients' and the
    /// copies' draws do not move them.
    fn plan_faults(&mut self) {
        let mut r = self.rng.fork();
        let until = us(self.cfg.faults_for);
        let mut t = us(r.part_of(self.cfg.crash_every));
        while t < until {
            let i = r.below(self.cfg.copies as u64) as usize;
            self.at(t, Event::Crash(i));
            let down = us(r.ms(50, 4000));
            self.at(t + down, Event::Restart(i));
            // Now and then a majority goes down at once.
            if r.below(8) == 0 {
                for j in 0..self.cfg.copies / 2 {
                    let k = (i + 1 + j) % self.cfg.copies;
                    self.at(t + us(r.ms(0, 200)), Event::Crash(k));
                    self.at(t + down + us(r.ms(0, 500)), Event::Restart(k));
                }
            }
            t += us(r.ms(10, 2 * self.cfg.crash_every.as_millis() as u64));
        }
        let mut t = us(r.part_of(self.cfg.cut_every));
        while t < until {
            let n = self.nodes();
            let mut side: Vec<bool> = (0..n).map(|_| r.below(2) == 0).collect();
            // One node alone, half the time.
            if r.below(2) == 0 {
                let alone = r.below(n as u64) as usize;
                side = (0..n).map(|x| x == alone).collect();
            }
            let from: Vec<usize> = (0..n).filter(|&x| side[x]).collect();
            let to: Vec<usize> = (0..n).filter(|&x| !side[x]).collect();
            let one_way = r.below(5) == 0;
            self.at(t, Event::Cut { from, to, one_way });
            let lasts = us(r.ms(20, 5000));
            self.at(t + lasts, Event::Heal);
            t += lasts + us(r.ms(10, 2 * self.cfg.cut_every.as_millis() as u64));
        }
        self.at(until, Event::Settle);
        self.at(us(self.cfg.faults_for + self.cfg.settle).saturating_sub(us(self.cfg.quiet)), Event::Quiet);
    }

    fn at(&mut self, t: Time, e: Event) {
        self.next_event += 1;
        self.queue.insert((t.max(self.now), self.next_event), e);
    }

    /// Mixes `x` into the run's fingerprint.
    fn mix(&mut self, x: u64) {
        self.hash = (self.hash ^ x).wrapping_mul(0x0100_0000_01b3);
    }

    pub(crate) fn note(&mut self, line: impl FnOnce() -> String) {
        if let Some(t) = &mut self.trace {
            t.push(format!("{:>10.3} ms  {}", self.now as f64 / 1000.0, line()));
        }
    }

    /// The clock of copy `i` (a coordinator's runs true).
    pub(crate) fn clock(&self, i: usize) -> Instant {
        let rate = self.copies.get(i).map_or(1_000_000, |c| c.rate);
        self.base + Duration::from_nanos((self.now as u128 * rate as u128 / 1000) as u64)
    }

    /// Schedules `timer` on node `i` after `d` of its own clock.
    pub(crate) fn timer(&mut self, i: usize, d: Duration, timer: Timer) {
        let rate = self.copies.get(i).map_or(1_000_000, |c| c.rate);
        let after = (us(d) as u128 * 1_000_000 / rate as u128) as Time;
        let inc = self.inc(i);
        self.at(self.now + after, Event::Timer { at: i, inc, timer });
    }

    fn inc(&self, i: usize) -> u64 {
        self.copies.get(i).map_or(0, |c| c.inc)
    }

    /// How long a message takes: usually a fraction of a millisecond, now
    /// and then far longer while faults run.
    fn delay(&mut self) -> Time {
        let base = self.rng.between(50, 600);
        let spike = match self.rng.below(1000) {
            0..=29 => self.rng.between(1_000, 20_000),
            30..=32 if self.faults => self.rng.between(50_000, 1_500_000),
            _ => 0,
        };
        base + spike
    }

    fn send(&mut self, from: usize, to: usize, msg: Msg) {
        if from != to && (self.cut.contains(&(from, to)) || (self.faults && self.rng.ppm(self.cfg.drop_ppm))) {
            self.mix(0xd409);
            return;
        }
        let d = if from == to { self.rng.between(5, 50) } else { self.delay() };
        self.at(self.now + d, Event::Deliver { to, msg });
    }

    /// Calls node `to` from node `from`, which gives the answer to
    /// `purpose`, or an error after `timeout` of its clock.
    pub(crate) fn call(&mut self, from: usize, to: usize, req: Req, timeout: Duration, purpose: Purpose) -> u64 {
        self.next_call += 1;
        let id = self.next_call;
        let inc = self.inc(from);
        self.calls.insert(id, Call { from, inc, purpose });
        self.send(from, to, Msg::Req { call: id, from, req });
        self.timer(from, timeout, Timer::Call(id));
        id
    }

    /// Node `from` answers call `call` of node `to`.
    pub(crate) fn answer(&mut self, from: usize, to: usize, call: u64, resp: Resp) {
        self.send(from, to, Msg::Resp { call, resp });
    }

    pub(crate) fn write_id(&mut self) -> u64 {
        self.next_write += 1;
        self.next_write
    }

    fn handle(&mut self, e: Event) {
        self.events += 1;
        self.mix(self.now);
        match e {
            Event::Deliver { to, msg } => match msg {
                Msg::Req { call, from, req } => {
                    self.mix(1 + to as u64);
                    if to < self.cfg.copies {
                        if !self.copies[to].up {
                            return self.send(to, from, Msg::Refused { call });
                        }
                        self.copy_request(to, from, call, req);
                    }
                }
                Msg::Resp { call, resp } => self.answered(to, call, Ok(resp)),
                Msg::Refused { call } => self.answered(to, call, Err("connection refused".into())),
            },
            Event::Timer { at, inc, timer } => {
                self.mix(100 + at as u64);
                if self.inc(at) != inc || (at < self.cfg.copies && !self.copies[at].up) {
                    return;
                }
                match timer {
                    Timer::Call(id) => {
                        if self.calls.contains_key(&id) {
                            self.answered(at, id, Err("no answer within the call's timeout".into()));
                        }
                    }
                    Timer::Client(c) => self.client_tick(at - self.cfg.copies, c),
                    t => self.copy_timer(at, t),
                }
            }
            Event::Crash(i) => {
                if self.faults && self.copies[i].up {
                    self.note(|| format!("s{i} crashes"));
                    self.crash(i);
                }
            }
            Event::Restart(i) => {
                if !self.copies[i].up {
                    self.note(|| format!("s{i} starts again"));
                    self.boot(i);
                }
            }
            Event::Cut { from, to, one_way } => {
                if !self.faults {
                    return;
                }
                self.note(|| format!("the network cuts {from:?} from {to:?}{}", if one_way { " (one way)" } else { "" }));
                for &a in &from {
                    for &b in &to {
                        self.cut.insert((a, b));
                        if !one_way {
                            self.cut.insert((b, a));
                        }
                    }
                }
            }
            Event::Heal => {
                if !self.cut.is_empty() {
                    self.note(|| "the network heals".to_string());
                }
                self.cut.clear();
            }
            Event::Settle => {
                self.note(|| "faults end: the network heals and every copy starts again".to_string());
                self.faults = false;
                self.cut.clear();
                for i in 0..self.cfg.copies {
                    if !self.copies[i].up {
                        self.boot(i);
                    }
                }
            }
            Event::Quiet => {
                self.note(|| "the clients stop".to_string());
                self.quiet = true;
            }
        }
    }

    /// The answer to call `id` reached node `at` (an error: none came).
    fn answered(&mut self, at: usize, id: u64, a: Result<Resp, String>) {
        let Some(c) = self.calls.remove(&id) else { return };
        if c.from != at || c.inc != self.inc(at) || (at < self.cfg.copies && !self.copies[at].up) {
            return;
        }
        self.mix(200 + at as u64);
        match c.purpose {
            Purpose::Coord { client, k } => self.coord_answer(at - self.cfg.copies, client, k, a),
            p => self.copy_answer(at, p, a),
        }
    }
}

/// Runs `seeds`, each as `config` makes it of its seed, on `jobs` threads:
/// the reports of those that broke a promise, by seed.
pub fn run_seeds(seeds: std::ops::Range<u64>, config: &(dyn Fn(u64) -> Config + Sync), jobs: usize) -> Vec<Report> {
    let next = std::sync::atomic::AtomicU64::new(seeds.start);
    let failed = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| loop {
                let s = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if s >= seeds.end {
                    return;
                }
                let r = run(config(s), false);
                if !r.ok() {
                    failed.lock().unwrap_or_else(|e| e.into_inner()).push(r);
                }
            });
        }
    });
    let mut failed = failed.into_inner().unwrap_or_else(|e| e.into_inner());
    failed.sort_by_key(|r| r.seed);
    failed
}
