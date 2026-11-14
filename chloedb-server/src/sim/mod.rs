//! A deterministic simulator of a shard's copies and its coordinators.
//!
//! A run is drawn from a seed ([`Config::of`]): three or five copies of a
//! shard with automatic failover, two coordinators whose clients write,
//! read and now and then promote a copy by hand, and faults. The network
//! delays, reorders and drops calls, cuts copies apart (both ways or one
//! way) and slows some links down for a while; copies crash and start
//! again, pause (a stalled process: what reaches it waits, its clock runs
//! on), their clocks run a little apart, and, when a run does not sync
//! its commits, a crash loses what was not synced. The copies run the protocol's decisions as [`crate::cores`]
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
//! I/O the server does around the cores ([`crate::replication`]), which
//! its replication and failover tests cover.

pub mod check;
pub mod cluster;
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

pub(crate) fn us(d: Duration) -> Time {
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
    /// Calls held up by 50 ms to 1.5 s while faults run, per thousand
    /// (at most 900): long enough to arrive after their caller gave up on
    /// them, and after calls it sent later.
    pub held_per_mille: u64,
    /// How far each clock's rate may be from true, in parts per million.
    pub drift_ppm: u64,
    /// Faults run for this long, then the network heals, the copies start
    /// again, and the run goes on for `settle`, its clients stopping
    /// `quiet` before the end.
    pub faults_for: Duration,
    pub settle: Duration,
    pub quiet: Duration,
    /// The mean times between crashes, cuts of the network, pauses of a
    /// node and links slowed down (zero: none of them).
    pub crash_every: Duration,
    pub cut_every: Duration,
    pub pause_every: Duration,
    pub slow_every: Duration,
    /// The chance of a client's operation being a promotion by hand, in
    /// parts per million.
    pub promote_ppm: u64,
    /// Bugs put back, each as the server had it before the simulator
    /// found it and the server was fixed: a run with one must find it
    /// ([`Bug`]).
    pub bugs: Vec<Bug>,
    /// Cuts placed by hand besides the drawn ones: copy `.0` cut off from
    /// every other node, both ways, from `.1` for `.2`.
    pub isolate: Vec<(usize, Duration, Duration)>,
    /// Crashes placed by hand besides the drawn ones: copy `.0` crashes at
    /// `.1` and starts again `.2` later.
    pub crashes: Vec<(usize, Duration, Duration)>,
    /// Volumes lost: copy `.0` (not copy 0, started as the primary with
    /// `--replicas`) crashes at `.1` and starts again `.2` later with the
    /// same `--replica` on an empty database, with nothing kept beside it
    /// (finding N-E2), while the other copies are sound
    /// ([`Sim::may_lose_volume`]).
    pub lost_volumes: Vec<(usize, Duration, Duration)>,
}

/// A bug the simulator found, put back with [`Config::bugs`] to check
/// that it still finds it (`chloedb-sim --bug NAME`).
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
    /// A shard's first replicas grant no votes until a primary has called
    /// them ([`crate::cores::shard::Replica::open`]): if the first primary
    /// stops before any of them heard from it, no copy is ever elected.
    WaitsForAFirstCall,
    /// A replica that granted a vote asks to be elected as soon as its
    /// primary has been silent for the failover delay, a moment after the
    /// vote ([`crate::cores::shard::Replica::granted`]), and an election
    /// round waits for every copy's answer, a cut-off copy's until the
    /// call's timeout ([`crate::cores::shard::Campaign::decided`]): the
    /// copy it voted for is still a replica while its round waits, grants
    /// that replica's pre-vote, and the replica's newer term then fences
    /// it once it is elected. For as long as their primary is cut off, the
    /// replicas replace each other: 28 terms in a minute in one seed. Either
    /// fix alone stops it in the simulator; the server has both.
    ReplacesTheCopyItElected,
    /// A primary elected commits nothing of its own term when it begins it
    /// ([`crate::replication::term_opened`]), and counts a transaction of
    /// an older term as acknowledged once a majority holds it
    /// ([`crate::cores::shard::Primary::term_start`]): it answers a
    /// client's retry of a write it holds from an older term (an
    /// idempotency key it finds) as acknowledged, dies, and a copy holding
    /// a transaction of a newer term at the same number is elected and
    /// drops it from every copy (finding DS-5, Raft's figure 8).
    AcksAnOlderTerm,
    /// A primary's write held by its writer as the primary steps down
    /// commits once it applied its new primary's first batch, under that
    /// term and the next number, which the new primary gives a write of its
    /// own (the server refuses it unapplied since, as the term it was taken
    /// in is over: `Queued.term`). Alone it breaks no promise: the copy
    /// refuses the next call that follows the new primary's transaction
    /// under that term and number, by its digest
    /// ([`crate::cores::shard::Replica::follows`]), and is sent a copy of
    /// the primary. With [`Bug::TrustsTheSameTerm`] the copies' histories
    /// stay apart.
    CommitsAfterSteppingDown,
    /// A replica applies a call that follows its last transaction by term
    /// and number alone, as before protocol 20, without comparing the
    /// primary's digest of it: a copy holding another transaction under
    /// them takes the primary's next ones on top of its own, and its
    /// history stays apart from the primary's.
    TrustsTheSameTerm,
    /// A replica installs a copy of the primary it follows that ends
    /// before transactions of that primary's term it holds since
    /// ([`chloedb::storage::Database::install_copy`]), as it did before it
    /// refused one: a copy delivered after its call gave up on it, after a newer
    /// copy and the transactions that follow, drops what the replica
    /// acknowledged since, and an election the replica then wins loses
    /// them (seed 624036738 of the nightly run, finding E-0g).
    InstallsALateCopy,
    /// A replica started with `--replica` on a database with no part kept
    /// counts itself a member and votes for any copy holding what it holds,
    /// and a replica counts itself a member at the first call that says so,
    /// caught up or not ([`crate::cores::shard::Replica::joined`]): a
    /// replica whose volume was lost starts again empty and votes, and a
    /// copy without the writes it had acknowledged is elected (finding
    /// N-E2).
    VotesWithALostVolume,
}

impl Bug {
    pub const ALL: [Bug; 10] = [
        Bug::AsksAKnownTerm,
        Bug::TakesWritesInALease,
        Bug::RefusesOlderTerms,
        Bug::WaitsForAFirstCall,
        Bug::ReplacesTheCopyItElected,
        Bug::AcksAnOlderTerm,
        Bug::CommitsAfterSteppingDown,
        Bug::TrustsTheSameTerm,
        Bug::InstallsALateCopy,
        Bug::VotesWithALostVolume,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Bug::AsksAKnownTerm => "asks-a-known-term",
            Bug::TakesWritesInALease => "takes-writes-in-a-lease",
            Bug::RefusesOlderTerms => "refuses-older-terms",
            Bug::WaitsForAFirstCall => "waits-for-a-first-call",
            Bug::ReplacesTheCopyItElected => "replaces-the-copy-it-elected",
            Bug::AcksAnOlderTerm => "acks-an-older-term",
            Bug::CommitsAfterSteppingDown => "commits-after-stepping-down",
            Bug::TrustsTheSameTerm => "trusts-the-same-term",
            Bug::InstallsALateCopy => "installs-a-late-copy",
            Bug::VotesWithALostVolume => "votes-with-a-lost-volume",
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
            held_per_mille: 3,
            drift_ppm: r.below(10_000),
            faults_for: Duration::from_secs(20),
            settle: Duration::from_secs(20) + copy_retry * 3,
            quiet: Duration::from_secs(5) + copy_retry * 2,
            crash_every: r.ms(1000, 8000),
            cut_every: r.ms(500, 6000),
            promote_ppm: r.below(5_000),
            bugs: Vec::new(),
            isolate: Vec::new(),
            crashes: Vec::new(),
            lost_volumes: Vec::new(),
            // Drawn last, so that the draws above give what they gave
            // before pauses and slow links; each in half the seeds, so that
            // the runs without them (which find other bugs) stay as many.
            pause_every: if r.below(2) == 0 { Duration::ZERO } else { r.ms(1000, 10_000) },
            slow_every: if r.below(2) == 0 { Duration::ZERO } else { r.ms(1000, 10_000) },
        }
    }
}

impl Config {
    /// Seed `seed` where copies of the primary often reach a replica late
    /// (`chloedb-sim --late-copies`): a ship log of one transaction sends
    /// a copy to any replica a call left behind, a call is given up after
    /// 500 ms, and three in ten calls while faults run arrive 50 ms to
    /// 1.5 s later; three copies, acknowledged by a majority, three
    /// clients. A copy then often arrives after its call gave up, after a
    /// newer copy and the transactions that follow it: in about one seed
    /// in five, and with [`Bug::InstallsALateCopy`] put back an
    /// acknowledged write is lost in about one seed in a hundred.
    pub fn late_copies(seed: u64) -> Config {
        Config {
            copies: 3,
            clients: 3,
            acks: Acks::Majority,
            ship_max: 1,
            call_timeout: Duration::from_millis(500),
            copy_retry: Duration::from_millis(200),
            held_per_mille: 300,
            ..Config::of(seed)
        }
    }
}

impl Config {
    /// Seed `seed` where a replica loses its volume (`chloedb-sim
    /// --lost-volume`): three copies acknowledged by a majority, three
    /// clients, the drawn faults, and at 4 to 12 s copy 2 cut off for 1 to
    /// 3 s, so that it lacks what copies 0 and 1 acknowledge meanwhile;
    /// copy 1 losing its volume a moment before that cut ends, empty for
    /// 50 ms to 1 s; and copy 0, usually the primary, crashing within that
    /// time, for 2 to 5 s. Copy 2 then asks to be elected while copy 0 is
    /// down: with [`Bug::VotesWithALostVolume`] put back, copy 1's vote
    /// elects it and the writes it lacks are lost in about one seed in
    /// five; without, copy 1 refuses it, and copy 0 is elected once back.
    pub fn lost_volume(seed: u64) -> Config {
        let mut r = Rng::new(seed ^ 0x10_57_70_1c);
        let at = r.ms(4000, 12_000);
        let cut = r.ms(1000, 3000);
        let lost = at + cut - r.ms(100, 900);
        let empty = r.ms(50, 1000);
        let crash = lost + r.part_of(empty);
        Config {
            copies: 3,
            clients: 3,
            acks: Acks::Majority,
            isolate: vec![(2, at, cut)],
            lost_volumes: vec![(1, lost, empty)],
            crashes: vec![(0, crash, r.ms(2000, 5000))],
            ..Config::of(seed)
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
    Restore { term: u64, primary: String, config: u64, copy: Checkpoint },
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
    /// Copy `i` crashes and loses its volume: its database and what it
    /// keeps beside it ([`Config::lost_volumes`]).
    LoseVolume(usize),
    /// Calls from each of `from` to each of `to` are lost (and back, unless
    /// `one_way`), until `Heal`.
    Cut { from: Vec<usize>, to: Vec<usize>, one_way: bool },
    Heal,
    /// Node `i` stops for `d`: what reaches it waits until then, and its
    /// clock runs on.
    Pause { i: usize, d: Time },
    /// Calls from `from` to each of `to` (and back, unless `one_way`) take
    /// up to `extra` longer, for `lasts`.
    Slow { from: usize, to: Vec<usize>, one_way: bool, extra: Time, lasts: Time },
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
    /// Links slowed down: how much longer a call takes, at most, and until
    /// when.
    slow: BTreeMap<(usize, usize), (Time, Time)>,
    /// Until when each node is paused.
    paused: Vec<Time>,
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
        let nodes = cfg.copies + cfg.coordinators;
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
            slow: BTreeMap::new(),
            paused: vec![0; nodes],
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
        while !self.cfg.crash_every.is_zero() && t < until {
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
        while !self.cfg.cut_every.is_zero() && t < until {
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
        let mut t = us(r.part_of(self.cfg.pause_every));
        while !self.cfg.pause_every.is_zero() && t < until {
            let i = r.below(self.nodes() as u64) as usize;
            // Now and then about as long as a primary's silence that has
            // its replicas elect another, or its lease on reads.
            let after = self.cfg.failover_after.as_millis() as u64;
            let d = match r.below(4) {
                0 => us(r.ms(after / 2, after * 3 / 2)),
                _ => us(r.ms(5, 1500)),
            };
            self.at(t, Event::Pause { i, d });
            t += us(r.ms(10, 2 * self.cfg.pause_every.as_millis() as u64));
        }
        let mut t = us(r.part_of(self.cfg.slow_every));
        while !self.cfg.slow_every.is_zero() && t < until {
            let n = self.nodes();
            let from = r.below(n as u64) as usize;
            // To one other node, or to every other.
            let to: Vec<usize> = match r.below(2) {
                0 => vec![(from + 1 + r.below(n as u64 - 1) as usize) % n],
                _ => (0..n).filter(|&x| x != from).collect(),
            };
            let one_way = r.below(3) == 0;
            let extra = us(r.ms(20, 3000));
            let lasts = us(r.ms(200, 8000));
            self.at(t, Event::Slow { from, to, one_way, extra, lasts });
            t += lasts + us(r.ms(10, 2 * self.cfg.slow_every.as_millis() as u64));
        }
        for &(i, at, down) in &self.cfg.crashes.clone() {
            self.at(us(at), Event::Crash(i));
            self.at(us(at + down), Event::Restart(i));
        }
        for &(i, at, down) in &self.cfg.lost_volumes.clone() {
            self.at(us(at), Event::LoseVolume(i));
            self.at(us(at + down), Event::Restart(i));
        }
        for &(i, from, lasts) in &self.cfg.isolate.clone() {
            let to = (0..self.nodes()).filter(|&x| x != i).collect();
            self.at(us(from), Event::Cut { from: vec![i], to, one_way: false });
            self.at(us(from + lasts), Event::Heal);
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

    /// How long a message from `from` to `to` takes: usually a fraction
    /// of a millisecond, now and then far longer while faults run, and up
    /// to the link's extra while it is slowed down.
    fn delay(&mut self, from: usize, to: usize) -> Time {
        let base = self.rng.between(50, 600);
        let held = 30 + self.cfg.held_per_mille.min(900);
        let spike = match self.rng.below(1000) {
            0..=29 => self.rng.between(1_000, 20_000),
            n if n < held && self.faults => self.rng.between(50_000, 1_500_000),
            _ => 0,
        };
        let slow = match self.slow.get(&(from, to)) {
            Some(&(extra, until)) if until > self.now => self.rng.between(extra / 4, extra),
            _ => 0,
        };
        base + spike + slow
    }

    fn send(&mut self, from: usize, to: usize, msg: Msg) {
        if from != to && (self.cut.contains(&(from, to)) || (self.faults && self.rng.ppm(self.cfg.drop_ppm))) {
            self.mix(0xd409);
            return;
        }
        let d = if from == to { self.rng.between(5, 50) } else { self.delay(from, to) };
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
        // A paused node takes what reaches it once it goes on.
        if let Event::Deliver { to: i, .. } | Event::Timer { at: i, .. } = &e {
            if self.paused[*i] > self.now {
                let t = self.paused[*i];
                self.at(t, e);
                return;
            }
        }
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
                    self.paused[i] = 0;
                    self.crash(i);
                }
            }
            Event::LoseVolume(i) => {
                if self.faults && i != 0 && self.copies[i].up && self.may_lose_volume(i) {
                    self.note(|| format!("s{i} crashes and loses its volume"));
                    self.paused[i] = 0;
                    self.crash(i);
                    self.lose_volume(i);
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
            Event::Pause { i, d } => {
                if self.faults && (i >= self.cfg.copies || self.copies[i].up) {
                    self.note(|| format!("node {i} pauses for {} ms", d / 1000));
                    self.paused[i] = self.paused[i].max(self.now + d);
                }
            }
            Event::Slow { from, to, one_way, extra, lasts } => {
                if !self.faults {
                    return;
                }
                self.note(|| format!("calls between node {from} and {to:?}{} take up to {} ms longer for {} ms", if one_way { " (one way)" } else { "" }, extra / 1000, lasts / 1000));
                let until = self.now + lasts;
                for &b in &to {
                    self.slow.insert((from, b), (extra, until));
                    if !one_way {
                        self.slow.insert((b, from), (extra, until));
                    }
                }
            }
            Event::Settle => {
                self.note(|| "faults end: the network heals and every copy starts again".to_string());
                self.faults = false;
                self.cut.clear();
                self.slow.clear();
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

    /// Whether copy `i` may lose its volume now: one copy forgetting what
    /// it kept is a fault the shard is to survive while the others are
    /// sound, which a copy cannot tell from here when (a) another copy
    /// has heard from no primary yet: the two, empty, could elect one of
    /// them, as a shard's first copies do; or (b) a copy serves as the
    /// primary of an older term than the newest one the copies know: the
    /// copy that lost its volume forgot that term, takes that primary's
    /// calls and counts toward its majority again. Raft has both holes for
    /// a member that lost its state, which must join again as a new one
    /// (here: started without --replica and added, `Configure`).
    fn may_lose_volume(&self, i: usize) -> bool {
        let others = || self.copies.iter().enumerate().filter(move |(j, _)| *j != i).map(|(_, c)| c);
        let heard = others().all(|c| c.kept.as_ref().is_some_and(|k| k.config.is_some()));
        let newest = others().map(|c| c.kept.as_ref().map_or(c.db.numbering.term, |k| k.newest(c.db.numbering.term))).max().unwrap_or(0);
        let stale = others().any(|c| c.up && c.role_primary().is_some() && c.db.numbering.term < newest);
        heard && !stale
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
    on_threads(seeds, jobs, &|s| Some(run(config(s), false)).filter(|r| !r.ok()))
}

/// Runs `run` on each of `seeds`, on `jobs` threads.
pub fn on_seeds(seeds: std::ops::Range<u64>, jobs: usize, run: &(dyn Fn(u64) + Sync)) {
    on_threads(seeds, jobs, &|s| {
        run(s);
        None::<()>
    });
}

/// What `run` gives of each of `seeds`, run on `jobs` threads, by seed.
pub(crate) fn on_threads<T: Send>(seeds: std::ops::Range<u64>, jobs: usize, run: &(dyn Fn(u64) -> Option<T> + Sync)) -> Vec<T> {
    let next = std::sync::atomic::AtomicU64::new(seeds.start);
    let found = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| loop {
                let s = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if s >= seeds.end {
                    return;
                }
                if let Some(r) = run(s) {
                    found.lock().unwrap_or_else(|e| e.into_inner()).push((s, r));
                }
            });
        }
    });
    let mut found = found.into_inner().unwrap_or_else(|e| e.into_inner());
    found.sort_by_key(|(s, _)| *s);
    found.into_iter().map(|(_, r)| r).collect()
}
