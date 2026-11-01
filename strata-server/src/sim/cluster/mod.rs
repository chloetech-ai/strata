//! A deterministic simulator of a cluster whose map changes and whose
//! slots move between shards while it serves.
//!
//! A run is drawn from a seed ([`Config::of`]): two or three shards of one
//! server each and a spare server, two coordinators whose clients write
//! and delete keys and read them, and an operator who asks a coordinator
//! now and then to move slots (one move, a rebalance or a drain), to add
//! the spare as a shard, to remove an empty one, to change a shard's
//! servers, or to finish a move another left. With [`Config::racing`],
//! both coordinators change the map, each on its own; without it, only
//! the first does, and the second routes by the map it follows. The
//! network delays, reorders and drops calls and cuts nodes apart; servers
//! and coordinators crash and start again, and pause. Servers decide as
//! [`crate::cores::slots`] makes them; coordinators change the map with
//! its [`Change`](crate::cores::slots::Change) and move slots with its
//! [`MoveRun`](crate::cores::slots::MoveRun), routing and taking up newer
//! maps as [`crate::coord::Coordinator`] does.
//!
//! After the faults, the network heals, every node starts again, the first
//! coordinator finishes a move left in the map and the clients stop; the
//! run then checks ([`check`]) that
//!
//! * a read sees every write and delete of its key acknowledged before it
//!   was sent, and the key on one shard at most;
//! * at the end, each key's row is where the map puts it, once, holding
//!   what its last acknowledged write left (or what a write whose answer
//!   was lost did), and no shard holds rows outside its slots;
//! * the cluster settles: no move under way, every server of the map
//!   holding it.
//!
//! A server holds its rows, its map and its last number on a disk that
//! keeps every write; it forgets on a crash what it holds in memory (a
//! hold on writes, its last export, writes waiting for a hold).

pub mod check;
mod coord;
mod server;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::rng::Rng;
use super::{us, Time};
use crate::slots::SlotMap;
use crate::ApiError;
use check::History;

/// What a run is made of, all of it drawn from its seed.
#[derive(Clone, Debug)]
pub struct Config {
    pub seed: u64,
    /// Shards at the start, each one server.
    pub shards: usize,
    /// Servers that can be added as shards.
    pub spares: usize,
    pub coordinators: usize,
    /// Clients of each coordinator, each one operation at a time.
    pub clients: usize,
    /// Keys `0..keys`; each client writes its own, and reads any.
    pub keys: i64,
    /// Whether every coordinator changes the map (else only the first).
    pub racing: bool,
    /// How long a call waits for its answer.
    pub call_timeout: Duration,
    /// How long a hold on writes lasts unless asked for again, and how
    /// often its coordinator asks (the server's are 30 s and 10 s; a run
    /// shrinks them with its other times).
    pub hold_ttl: Duration,
    pub hold_renew: Duration,
    /// Rows of an export written to the shard they move to per write.
    pub export_rows: usize,
    /// Calls lost while faults run, in parts per million.
    pub drop_ppm: u64,
    /// Faults run for this long, then the network heals, every node starts
    /// again, and the run goes on for `settle`, its clients stopping
    /// `quiet` before the end.
    pub faults_for: Duration,
    pub settle: Duration,
    pub quiet: Duration,
    /// The mean times between crashes of servers, crashes of
    /// coordinators, cuts of the network and pauses of a node.
    pub crash_every: Duration,
    pub coord_crash_every: Duration,
    pub cut_every: Duration,
    pub pause_every: Duration,
    /// The mean time between the operator's asks.
    pub ask_every: Duration,
    /// Bugs put back, each as the server had it before the simulator
    /// found it: a run with one must find it ([`Bug`]).
    pub bugs: Vec<Bug>,
}

/// A bug the cluster simulator found, put back with [`Config::bugs`] to
/// check that it still finds it (`strata-sim --cluster --bug NAME`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    /// The shard slots move from takes the flip although it lost its hold
    /// on their writes since their last export (it restarted, or the hold
    /// ran out and was asked for again): writes taken meanwhile are not
    /// copied, and the cleanup after the flip deletes them
    /// ([`crate::cores::slots::flip_ok`]).
    FlipsAnUnheldMove,
    /// A coordinator routes by a map no server took, and makes its next
    /// change from it: the map forks from the one the servers hold, and a
    /// move flipped there is given up here, its rows deleted
    /// ([`crate::cores::slots::Change`]).
    RoutesByAMapNoServerTook,
    /// The shard slots were to move to takes a copy of their rows after
    /// the move was given up and what was sent deleted: the row shows once
    /// the move ends, on two shards ([`crate::cores::slots::Moved::Copy`]).
    TakesLateCopies,
    /// A coordinator gives a map made while slots copy to every server at
    /// once, though the server they move from may hold the move's flip of
    /// that epoch (its answer lost): the map forks, and reads miss the
    /// moving rows until a newer map comes ([`crate::cores::slots::Change::synced`]).
    ChangesAroundTheSource,
}

impl Bug {
    pub const ALL: [Bug; 4] = [Bug::FlipsAnUnheldMove, Bug::RoutesByAMapNoServerTook, Bug::TakesLateCopies, Bug::ChangesAroundTheSource];

    pub fn name(self) -> &'static str {
        match self {
            Bug::FlipsAnUnheldMove => "flips-an-unheld-move",
            Bug::RoutesByAMapNoServerTook => "routes-by-a-map-no-server-took",
            Bug::TakesLateCopies => "takes-late-copies",
            Bug::ChangesAroundTheSource => "changes-around-the-source",
        }
    }

    pub fn of(name: &str) -> Option<Bug> {
        Bug::ALL.into_iter().find(|b| b.name() == name)
    }
}

impl Config {
    /// The run of seed `seed`.
    pub fn of(seed: u64) -> Config {
        let mut r = Rng::new(seed ^ 0x51_075e_ed);
        let hold_ttl = r.ms(500, 4000);
        Config {
            seed,
            shards: r.between(2, 3) as usize,
            spares: 1,
            coordinators: 2,
            clients: r.between(1, 3) as usize,
            keys: r.between(16, 64) as i64,
            racing: false,
            call_timeout: r.ms(500, 3000),
            hold_ttl,
            hold_renew: hold_ttl / 3,
            export_rows: r.between(1, 8) as usize,
            drop_ppm: r.below(20_000),
            faults_for: Duration::from_secs(20),
            settle: Duration::from_secs(25),
            quiet: Duration::from_secs(5),
            crash_every: r.ms(2000, 10_000),
            coord_crash_every: r.ms(3000, 20_000),
            cut_every: r.ms(1000, 8000),
            pause_every: r.ms(2000, 10_000),
            ask_every: r.ms(300, 3000),
            bugs: Vec::new(),
        }
    }
}

/// What a run found.
#[derive(Clone, Debug)]
pub struct Report {
    pub seed: u64,
    /// What it broke of what the cluster promises; empty when nothing.
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

/// A row's value: the number of the operation that wrote it (none: deleted).
pub type Value = Option<u64>;

/// A call to a server.
#[derive(Clone, Debug)]
pub(crate) enum Req {
    Hello,
    HeldMap,
    Give { map: SlotMap, shard: usize },
    Write { key: i64, value: Value, epoch: u64 },
    /// Rows a coordinator moves, to the shard their slots move to.
    Moved { rows: Vec<(i64, Value)>, epoch: u64 },
    Read { key: i64, epoch: u64 },
    Export { changes: bool, epoch: u64 },
    Freeze { thaw: bool, epoch: u64 },
    Cleanup { epoch: u64 },
}

/// A server's answer.
#[derive(Clone, Debug)]
pub(crate) enum Resp {
    Hello { epoch: u64 },
    HeldMap(Option<(SlotMap, usize)>),
    Given(Result<u64, ApiError>),
    Wrote(Result<u64, ApiError>),
    Read(Result<Value, ApiError>),
    Exported(Result<Vec<(i64, Value)>, ApiError>),
    Frozen(Result<u64, ApiError>),
    Cleaned(Result<u64, ApiError>),
}

#[derive(Debug)]
enum Msg {
    Req { call: u64, from: usize, req: Req },
    Resp { call: u64, resp: Resp },
    /// The server is down: the caller's connection is refused.
    Refused { call: u64 },
}

#[derive(Clone, Debug)]
pub(crate) enum Timer {
    /// A call's timeout.
    Call(u64),
    /// A server's hold on writes, asked for as `id`, runs out.
    HoldEnds { id: u64 },
    Client(usize),
    /// A client's operation goes again after a wait.
    Again(usize),
    /// A coordinator's watcher takes up a newer map.
    Watch,
    /// A coordinator asks for its hold again, while `token` is current.
    Renew { token: u64 },
}

/// What the operator asks of a coordinator.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Ask {
    /// Move slots: one move, a rebalance or a drain.
    Moves,
    AddShard,
    RemoveShard,
    /// Change one shard's servers (to the same: the map's epoch moves).
    Servers,
    /// Finish a move no job here runs.
    Finish,
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
    /// Node `i` does nothing for a while: what reaches it waits.
    Pause { i: usize, d: Time },
    Ask { j: usize, ask: Ask },
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
    queue: BTreeMap<(Time, u64), Event>,
    next_event: u64,
    pub(crate) servers: Vec<server::Server>,
    coords: Vec<coord::Coord>,
    calls: BTreeMap<u64, Call>,
    next_call: u64,
    cut: BTreeSet<(usize, usize)>,
    paused: Vec<Time>,
    faults: bool,
    quiet: bool,
    pub(crate) history: History,
    trace: Option<Vec<String>>,
    hash: u64,
    events: u64,
    next_op: u64,
    /// The spare servers the operator asked to add.
    spares_asked: Vec<bool>,
}

#[derive(Debug)]
struct Call {
    from: usize,
    inc: u64,
    to: usize,
    purpose: coord::Purpose,
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
    let map = s.coords[0].map.clone();
    s.history.settled(&s.servers, &map, s.now);
    Report { seed: s.cfg.seed, violations: s.history.violations.clone(), stats: s.history.stats.clone(), events: s.events, fingerprint: s.hash, trace: s.trace.unwrap_or_default() }
}

/// Runs `seeds`, each as `config` makes it of its seed, on `jobs` threads:
/// the reports of those that broke a promise, by seed.
pub fn run_seeds(seeds: std::ops::Range<u64>, config: &(dyn Fn(u64) -> Config + Sync), jobs: usize) -> Vec<Report> {
    super::on_threads(seeds, jobs, &|s| Some(run(config(s), false)).filter(|r| !r.ok()))
}

/// The URL of server `i`.
pub(crate) fn url(i: usize) -> String {
    format!("s{i}")
}

/// The server of URL `u`.
pub(crate) fn server_of(u: &str) -> Option<usize> {
    u.strip_prefix('s')?.parse().ok()
}

impl Sim {
    fn new(cfg: Config, trace: bool) -> Sim {
        let rng = Rng::new(cfg.seed);
        let n = cfg.shards + cfg.spares;
        let first = SlotMap::even(cfg.shards, 1).with_replicas((0..cfg.shards).map(|i| vec![url(i)]).collect()).expect("a map");
        let servers = (0..n).map(|i| server::Server::new(i, (i < cfg.shards).then(|| (first.clone(), i)))).collect();
        let coords = (0..cfg.coordinators).map(|_| coord::Coord::new(&cfg, first.clone(), n)).collect();
        Sim {
            history: History::new(&cfg, &first),
            paused: vec![0; n + cfg.coordinators],
            cfg,
            rng,
            now: 0,
            queue: BTreeMap::new(),
            next_event: 0,
            servers,
            coords,
            calls: BTreeMap::new(),
            next_call: 0,
            cut: BTreeSet::new(),
            faults: true,
            quiet: false,
            trace: trace.then(Vec::new),
            hash: 0xcbf2_9ce4_8422_2325,
            events: 0,
            next_op: 0,
            spares_asked: vec![false; n],
        }
    }

    pub(crate) fn n_servers(&self) -> usize {
        self.servers.len()
    }

    /// Every node of the run: the servers, then the coordinators.
    fn nodes(&self) -> usize {
        self.servers.len() + self.coords.len()
    }

    fn start(&mut self) {
        let n = self.n_servers();
        for j in 0..self.cfg.coordinators {
            for c in 0..self.cfg.clients {
                let at = self.rng.between(0, 50_000);
                self.at(at, Event::Timer { at: n + j, inc: 0, timer: Timer::Client(c) });
            }
            self.at(us(coord::WATCH_EVERY), Event::Timer { at: n + j, inc: 0, timer: Timer::Watch });
        }
        self.plan_faults();
    }

    /// The faults and the operator's asks, drawn up front so that the
    /// clients' and the nodes' draws do not move them.
    fn plan_faults(&mut self) {
        let mut r = self.rng.fork();
        let until = us(self.cfg.faults_for);
        let n = self.n_servers();
        let mut t = us(r.part_of(self.cfg.crash_every));
        while t < until {
            let i = r.below(n as u64) as usize;
            self.at(t, Event::Crash(i));
            self.at(t + us(r.ms(50, 3000)), Event::Restart(i));
            t += us(r.ms(10, 2 * self.cfg.crash_every.as_millis() as u64));
        }
        let mut t = us(r.part_of(self.cfg.coord_crash_every));
        while t < until {
            let j = r.below(self.cfg.coordinators as u64) as usize;
            self.at(t, Event::Crash(n + j));
            self.at(t + us(r.ms(50, 3000)), Event::Restart(n + j));
            t += us(r.ms(10, 2 * self.cfg.coord_crash_every.as_millis() as u64));
        }
        let mut t = us(r.part_of(self.cfg.cut_every));
        while t < until {
            let all = self.nodes();
            let mut side: Vec<bool> = (0..all).map(|_| r.below(2) == 0).collect();
            // One node alone, half the time.
            if r.below(2) == 0 {
                let alone = r.below(all as u64) as usize;
                side = (0..all).map(|x| x == alone).collect();
            }
            let from: Vec<usize> = (0..all).filter(|&x| side[x]).collect();
            let to: Vec<usize> = (0..all).filter(|&x| !side[x]).collect();
            let one_way = r.below(5) == 0;
            self.at(t, Event::Cut { from, to, one_way });
            let lasts = us(r.ms(20, 4000));
            self.at(t + lasts, Event::Heal);
            t += lasts + us(r.ms(10, 2 * self.cfg.cut_every.as_millis() as u64));
        }
        let mut t = us(r.part_of(self.cfg.pause_every));
        while t < until {
            let i = r.below(self.nodes() as u64) as usize;
            // Now and then past a hold's time.
            let d = match r.below(4) {
                0 => us(self.cfg.hold_ttl) + us(r.ms(0, 2000)),
                _ => us(r.ms(5, 1500)),
            };
            self.at(t, Event::Pause { i, d });
            t += us(r.ms(10, 2 * self.cfg.pause_every.as_millis() as u64));
        }
        let mut t = us(r.part_of(self.cfg.ask_every));
        while t < until {
            let j = if self.cfg.racing { r.below(self.cfg.coordinators as u64) as usize } else { 0 };
            let ask = match r.below(20) {
                0..=10 => Ask::Moves,
                11..=12 => Ask::AddShard,
                13..=14 => Ask::RemoveShard,
                15..=17 => Ask::Servers,
                _ => Ask::Finish,
            };
            self.at(t, Event::Ask { j, ask });
            t += us(r.ms(10, 2 * self.cfg.ask_every.as_millis() as u64));
        }
        self.at(until, Event::Settle);
        let end = us(self.cfg.faults_for + self.cfg.settle);
        // The first coordinator finishes a move left in the map, now and
        // then until the end.
        let mut t = until + us(Duration::from_secs(3));
        while t + us(self.cfg.quiet) < end {
            self.at(t, Event::Ask { j: 0, ask: Ask::Finish });
            t += us(Duration::from_secs(3));
        }
        self.at(end.saturating_sub(us(self.cfg.quiet)), Event::Quiet);
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

    pub(crate) fn inc(&self, i: usize) -> u64 {
        let n = self.n_servers();
        match i < n {
            true => self.servers[i].inc,
            false => self.coords[i - n].inc,
        }
    }

    fn up(&self, i: usize) -> bool {
        let n = self.n_servers();
        match i < n {
            true => self.servers[i].up,
            false => self.coords[i - n].up,
        }
    }

    /// Schedules `timer` on node `i` after `d`.
    pub(crate) fn timer(&mut self, i: usize, d: Duration, timer: Timer) {
        let inc = self.inc(i);
        self.at(self.now + us(d), Event::Timer { at: i, inc, timer });
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
        let d = self.delay();
        self.at(self.now + d, Event::Deliver { to, msg });
    }

    /// Calls server `to` from coordinator node `from`, which gives the
    /// answer to `purpose`, or an error after `timeout`.
    pub(crate) fn call(&mut self, from: usize, to: usize, req: Req, timeout: Duration, purpose: coord::Purpose) -> u64 {
        self.next_call += 1;
        let id = self.next_call;
        let inc = self.inc(from);
        self.calls.insert(id, Call { from, inc, to, purpose });
        self.send(from, to, Msg::Req { call: id, from, req });
        self.timer(from, timeout, Timer::Call(id));
        id
    }

    /// Server `from` answers call `call` of node `to`.
    pub(crate) fn answer(&mut self, from: usize, to: usize, call: u64, resp: Resp) {
        self.send(from, to, Msg::Resp { call, resp });
    }

    pub(crate) fn op_id(&mut self) -> u64 {
        self.next_op += 1;
        self.next_op
    }

    fn target(e: &Event) -> Option<usize> {
        match e {
            Event::Deliver { to, .. } => Some(*to),
            Event::Timer { at, .. } => Some(*at),
            _ => None,
        }
    }

    fn handle(&mut self, e: Event) {
        // A paused node takes what reaches it once it goes on.
        if let Some(i) = Self::target(&e) {
            if self.paused[i] > self.now {
                let t = self.paused[i];
                self.at(t, e);
                return;
            }
        }
        self.events += 1;
        self.mix(self.now);
        let n = self.n_servers();
        match e {
            Event::Deliver { to, msg } => match msg {
                Msg::Req { call, from, req } => {
                    self.mix(1 + to as u64);
                    if !self.servers[to].up {
                        return self.send(to, from, Msg::Refused { call });
                    }
                    self.serve(to, from, call, req);
                }
                Msg::Resp { call, resp } => self.answered(to, call, Ok(resp)),
                Msg::Refused { call } => self.answered(to, call, Err(ApiError { status: 503, message: "connection refused".into() })),
            },
            Event::Timer { at, inc, timer } => {
                self.mix(100 + at as u64);
                if self.inc(at) != inc || !self.up(at) {
                    return;
                }
                match timer {
                    Timer::Call(id) => {
                        if self.calls.contains_key(&id) {
                            self.answered(at, id, Err(ApiError { status: 504, message: "no answer within the call's timeout".into() }));
                        }
                    }
                    Timer::HoldEnds { id } => self.hold_ends(at, id),
                    Timer::Client(c) => self.client_tick(at - n, c),
                    Timer::Again(c) => self.client_again(at - n, c),
                    Timer::Watch => self.watch(at - n),
                    Timer::Renew { token } => self.renew(at - n, token),
                }
            }
            Event::Crash(i) => {
                if self.faults && self.up(i) {
                    match i < n {
                        true => {
                            self.note(|| format!("s{i} crashes"));
                            self.servers[i].crash();
                        }
                        false => {
                            self.note(|| format!("c{} crashes", i - n));
                            self.coord_crash(i - n);
                        }
                    }
                }
            }
            Event::Restart(i) => self.restart(i),
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
                if self.faults && self.up(i) {
                    self.note(|| format!("node {i} pauses for {} ms", d / 1000));
                    self.paused[i] = self.paused[i].max(self.now + d);
                }
            }
            Event::Ask { j, ask } => {
                if self.coords[j].up && (self.faults || matches!(ask, Ask::Finish)) {
                    self.ask(j, ask);
                }
            }
            Event::Settle => {
                self.note(|| "faults end: the network heals and every node starts again".to_string());
                self.faults = false;
                self.cut.clear();
                for i in 0..self.nodes() {
                    self.restart(i);
                }
            }
            Event::Quiet => {
                self.note(|| "the clients stop".to_string());
                self.quiet = true;
            }
        }
    }

    fn restart(&mut self, i: usize) {
        if self.up(i) {
            return;
        }
        let n = self.n_servers();
        match i < n {
            true => {
                self.note(|| format!("s{i} starts again"));
                self.servers[i].up = true;
            }
            false => {
                self.note(|| format!("c{} starts again", i - n));
                self.coord_restart(i - n);
            }
        }
    }

    /// The answer to call `id` reached node `at` (an error: none came).
    fn answered(&mut self, at: usize, id: u64, a: Result<Resp, ApiError>) {
        let Some(c) = self.calls.remove(&id) else { return };
        if c.from != at || c.inc != self.inc(at) || !self.up(at) {
            return;
        }
        self.mix(200 + at as u64);
        let n = self.n_servers();
        self.coord_answer(at - n, c.to, c.purpose, a);
    }
}
