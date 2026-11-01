//! The cluster map's decisions: a shard taking a map and checking what is
//! routed to it by one, and a coordinator changing the map and moving
//! slots between shards ([`crate::slots`]).
//!
//! The shard's side decides for [`crate::service::Service::set_slots`] and
//! its epoch and key checks, and for the hold on writes to moving slots,
//! the export of their rows, and the rows a shard hides and deletes
//! (`service/moving.rs`). The coordinator's side is
//! [`crate::coord::Coordinator::change`] ([`Change`]) and the move job of
//! [`crate::coord::moves`] ([`MoveRun`]), as steps that a driver carries
//! out (asking the servers, giving them a map, exporting rows) and
//! answers: the server, over gRPC, and the simulator
//! ([`crate::sim::cluster`]), on its model network.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::service::{NO_MAP_YET, OLDER_THAN_REQUEST, OLDER_THAN_SHARD};
use crate::slots::{Move, MoveState, SlotMap};
use crate::ApiError;

fn conflict(message: String) -> ApiError {
    ApiError { status: 409, message }
}

fn unavailable(message: String) -> ApiError {
    ApiError { status: 503, message }
}

/// What a shard does with a cluster map given to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
    /// It holds that map already.
    Held,
    /// It keeps the map, and serves requests routed by maps from epoch
    /// `since` on ([`epoch_ok`]); first, when `check_rows`, it must find
    /// that every row it holds is in its slots under the map, or in those
    /// of the map's move that it holds without serving.
    Keep { since: u64, check_rows: bool },
}

/// Whether a shard holding `held` (the map and its place in it), and
/// serving maps from epoch `since`, takes `map` as its shard `shard`: a
/// map older than its own is refused (409), as is another map of the same
/// epoch. A map that leaves the shard the slots it holds keeps serving
/// coordinators that route by the one before; one that changes them must
/// find its rows in place, unless it is the next step of a move, whose
/// rows the coordinator moved.
pub fn take(held: Option<(&SlotMap, usize)>, since: u64, map: &SlotMap, shard: usize) -> Result<Take, ApiError> {
    if shard >= map.shards() {
        return Err(ApiError::bad(format!("shard {shard} of a slot map of {} shards", map.shards())));
    }
    if let Some((m, s)) = held {
        if map.epoch < m.epoch {
            return Err(conflict(format!("slot map epoch {} is older than this shard's ({}): the coordinator must load the newer map (--slot-map)", map.epoch, m.epoch)));
        }
        if map.epoch == m.epoch {
            return match *m == *map && s == shard {
                true => Ok(Take::Held),
                false => Err(conflict(format!(
                    "this shard holds a different slot map of epoch {}, as shard {s} of {}: list the shards as when it was given, or give a map of a higher epoch (--slot-map)",
                    m.epoch,
                    m.shards()
                ))),
            };
        }
    }
    let same = held.is_some_and(|(m, s)| map.same_slots_as(shard, m, s));
    let since = match same {
        true => since.min(map.epoch),
        false => map.epoch,
    };
    let stepped = held.is_some_and(|(m, s)| s == shard && m.shards() == map.shards() && m.moves_to(map));
    Ok(Take::Keep { since, check_rows: !same && !stepped })
}

/// Checks a request routed by the map of epoch `epoch` against the map of
/// epoch `held` (0: none) a shard serving maps from `since` holds: 0 (not
/// from a coordinator with a map) passes, as does an older epoch from
/// `since` on, under which the shard held the slots it holds; another
/// older one is refused (409), as the coordinator routes by a map the
/// shards left; a newer one, or one the shard lacks, is refused as
/// unavailable (503), so that the coordinator asks again and gives it its
/// map.
pub fn epoch_ok(held: u64, since: u64, epoch: u64) -> Result<(), ApiError> {
    if epoch == 0 {
        return Ok(());
    }
    match held {
        e if e == epoch => Ok(()),
        e if e > epoch && epoch >= since => Ok(()),
        e if e > epoch => Err(conflict(format!("slot map epoch {epoch} is older than this shard's ({e}): the coordinator must load the newer map (--slot-map)"))),
        0 => Err(unavailable(format!("{NO_MAP_YET}; the request's is epoch {epoch}"))),
        e => Err(unavailable(format!("this shard has slot map epoch {e}, {OLDER_THAN_REQUEST} {epoch}"))),
    }
}

/// Why a shard refuses a row placed in a slot it does not hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Misplaced {
    /// The request was routed by an older map, which the shard's changed
    /// since: its coordinator routes it again (409, [`Misplaced::error`]).
    Routed { routed: u64 },
    /// Another shard holds the slot: the row must go through the
    /// coordinator (400).
    Elsewhere,
}

/// Why a row is written to a shard that does not hold its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moved {
    /// It is not: a client's write.
    No,
    /// A coordinator copies it to the shard its slot moves to.
    Copy,
    /// The shard deletes rows of moving slots that it holds without
    /// serving them ([`SlotMap::residue`]).
    Cleanup,
}

/// Whether shard `shard`, holding `map`, refuses a row placed in slot
/// `slot`, routed by the map of epoch `routed`: one of another shard's
/// slots, unless the coordinator moving it copies it to the shard it moves
/// to while the move copies ([`Moved::Copy`]), or the shard deletes it as
/// one of the move's two ([`Moved::Cleanup`]). A copy that arrives once the
/// move is given up would outlive the cleanup that deletes what was sent,
/// and show once the move ends (a row on two shards).
pub fn misplaced(map: &SlotMap, shard: usize, slot: usize, moved: Moved, routed: u64) -> Option<Misplaced> {
    let passes = match moved {
        Moved::No => false,
        Moved::Copy => map.moving().is_some_and(|m| m.to == shard && m.state == MoveState::Copying && m.has(slot)),
        Moved::Cleanup => map.takes_moved(shard, slot),
    };
    if map.owner(slot) == shard || passes {
        return None;
    }
    match routed > 0 && routed < map.epoch {
        true => Some(Misplaced::Routed { routed }),
        false => Some(Misplaced::Elsewhere),
    }
}

impl Misplaced {
    /// The refusal of a row in `slot`, held by another shard under `map`:
    /// `what` names the row's place (its key) and `key` is its value.
    pub fn error(self, map: &SlotMap, shard: usize, slot: usize, what: &str, key: &str) -> ApiError {
        match self {
            Misplaced::Routed { routed } => conflict(format!("slot map epoch {routed} {OLDER_THAN_SHARD} ({}), under which slot {slot} is shard {}'s: the coordinator must load the newer map", map.epoch, map.owner(slot))),
            Misplaced::Elsewhere => ApiError::bad(format!(
                "{what} {key} is in slot {slot}, which shard {} holds under slot map epoch {}, not this shard ({shard}): write through the coordinator",
                map.owner(slot),
                map.epoch
            )),
        }
    }
}

/// The move from shard `shard` that is copying under `map`: the one whose
/// writes it holds when asked and whose rows it exports.
pub fn exporting(map: &SlotMap, shard: usize) -> Option<&Move> {
    map.moving().filter(|m| m.from == shard && m.state == MoveState::Copying)
}

/// Whether a write to rows of `slots`, or an UPDATE or DELETE (`statement`,
/// which may change any row), must pass the hold on writes to the slots
/// moving from shard `shard` under `map`.
pub fn gated(map: &SlotMap, shard: usize, slots: &[usize], statement: bool) -> bool {
    exporting(map, shard).is_some_and(|m| statement || slots.iter().any(|&s| m.has(s)))
}

/// What a write that waited for a hold on moving slots does once it ends:
/// it goes on if the map is still the one it was checked by (epoch
/// `checked`), and is refused (409) for its coordinator to route it by the
/// new one if not.
pub fn after_hold(checked: u64, now: u64) -> Result<(), ApiError> {
    match checked == now {
        true => Ok(()),
        false => Err(conflict(format!(
            "slot map epoch {checked} {OLDER_THAN_SHARD} ({now}): it changed while this write waited for slots moving from this shard; the coordinator must load the newer map"
        ))),
    }
}

/// The move from this shard, to hold writes for or export: refused (409)
/// when none is copying.
pub fn moving_from_here(held: Option<(&SlotMap, usize)>) -> Result<Move, ApiError> {
    let (map, shard) = held.ok_or_else(|| ApiError::bad("this server holds no cluster map"))?;
    match exporting(map, shard) {
        Some(m) => Ok(m.clone()),
        None => Err(conflict(format!("no slots are moving from this shard (shard {shard}) under its cluster map, epoch {}", map.epoch))),
    }
}

/// What an export of `m`'s rows starts from: every row (`changes` false),
/// or what changed since the last export of the same move, `exported` (a
/// version `V` of the database), which a shard that restarted, or whose
/// primary changed, does not have (409). A shard that does not number its
/// writes cannot tell what changed.
pub fn export_from<'a, V>(changes: bool, exported: Option<&'a (Move, V)>, m: &Move, numbered: bool) -> Result<Option<&'a V>, ApiError> {
    if !changes {
        return Ok(None);
    }
    match exported {
        Some((pm, v)) if pm == m && numbered => Ok(Some(v)),
        _ => Err(conflict("this shard has no export of the moving slots to follow (it restarted, or its primary changed): export every row again".into())),
    }
}

/// Whether a shard keeps its last export under a new map: only while its
/// move is still copying from the shard.
pub fn keeps_export(map: &SlotMap, shard: usize) -> bool {
    exporting(map, shard).is_some()
}

/// A shard's hold on writes to the slots moving from it: held until
/// thawed, or until its time runs out unless asked for again.
#[derive(Clone, Debug, Default)]
pub struct Hold {
    asked: u64,
    /// The ask its time counts from, and the shard's last write when the
    /// hold began.
    held: Option<(u64, u64)>,
}

impl Hold {
    /// Holds the writes (or holds them longer), the shard's last write
    /// numbered `at`: returns the ask, whose time ends the hold when it
    /// runs out ([`Self::expire`]) unless another ask came since.
    pub fn freeze(&mut self, at: u64) -> u64 {
        self.asked += 1;
        let since = self.held.map_or(at, |(_, s)| s);
        self.held = Some((self.asked, since));
        self.asked
    }

    pub fn thaw(&mut self) {
        self.held = None;
    }

    /// The time of ask `id` ran out: whether that ended the hold.
    pub fn expire(&mut self, id: u64) -> bool {
        match self.held.is_some_and(|(a, _)| a == id) {
            true => {
                self.held = None;
                true
            }
            false => false,
        }
    }

    pub fn held(&self) -> bool {
        self.held.is_some()
    }

    /// Whether writes to the moving slots have been held since before an
    /// export taken at the shard's write `exported`: if so, that export
    /// holds every write to them.
    pub fn covers(&self, exported: Option<u64>) -> bool {
        matches!((self.held, exported), (Some((_, since)), Some(e)) if since <= e)
    }
}

/// Whether a shard whose slots are moving from it under `held` takes
/// `map`, as its shard `shard`, that gives some of them away (the move's
/// flip): only while writes to them have been held since before the last
/// export of their rows ([`Hold::covers`]), so that the shard they go to
/// holds every write to them. A hold lost meanwhile (the shard restarted,
/// or its coordinator did not ask for it again in time) let writes in
/// that no export copied, which the cleanup after the flip would delete
/// (409: the move gives up, and copies again when asked). A server of the
/// shard that takes no writes from clients (a replica, or a primary
/// another replaced) passes `covered`: what it holds came from the one
/// that does, which takes the map first ([`Change::synced`]).
pub fn flip_ok(held: Option<(&SlotMap, usize)>, map: &SlotMap, shard: usize, covered: bool) -> Result<(), ApiError> {
    let Some(mv) = held.and_then(|(m, s)| exporting(m, s)) else { return Ok(()) };
    if held.is_some_and(|(m, _)| map.epoch <= m.epoch) || mv.slots.iter().all(|&s| map.owner(s) == shard) || covered {
        return Ok(());
    }
    Err(conflict(format!(
        "writes to the slots moving from this shard were not held since their rows were last exported (it restarted, or the hold ran out), so the shard they move to may lack some: map epoch {} cannot give them away; copy them again",
        map.epoch
    )))
}

/// How a server answered a coordinator giving it a cluster map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Given {
    Took,
    /// It holds another map of the same epoch or a newer one: another
    /// coordinator changed the map first.
    Raced(String),
    /// It refused the map otherwise (409).
    Refused(String),
    /// No answer, or another error: it may take it when asked again.
    Missed(String),
}

/// What a server's answer `a` to being given `next` says, with `holds` the
/// map it said it holds when asked after a refusal (409).
pub fn given(a: Result<(), ApiError>, holds: Option<&SlotMap>, next: &SlotMap) -> Given {
    match a {
        Ok(()) => Given::Took,
        Err(e) if e.status == 409 => match holds {
            Some(m) if m.epoch >= next.epoch && m != next => Given::Raced(e.message),
            _ => Given::Refused(e.message),
        },
        Err(e) => Given::Missed(e.message),
    }
}

/// Changes of the cluster map a coordinator tries before giving up, each
/// on the map another coordinator's change left.
pub const CHANGE_TRIES: u32 = 3;

/// How many times a server that must take a map first is given it.
pub const FIRST_TRIES: u32 = 3;

/// The next step of a [`Change`].
#[derive(Clone, Debug)]
pub enum ChangeStep {
    /// Ask every server which map it holds and route by the newest
    /// (`Coordinator::sync`), then [`Change::synced`].
    Sync,
    /// Give `map` to each of these servers (as shard, URL), at once, and
    /// answer each with [`Change::answered`].
    Give { map: SlotMap, to: Vec<(usize, String)> },
    /// The change is made (route by the map from now on: the servers that
    /// did not take it get it when they answer again), or failed.
    Done(Result<(SlotMap, Vec<String>), ApiError>),
}

#[derive(Clone, Debug)]
enum Stage {
    Syncing,
    /// Giving `firsts[k]` the map, try `tries`.
    First { k: usize, tries: u32 },
    Rest { waiting: BTreeSet<String> },
    Done,
}

/// A change of the cluster map: the next map, made from the newest, is
/// given to every server of it, those of `first` first, one at a time, and
/// none other if one of them does not take it. A change another
/// coordinator made meanwhile (a server holds another map of that epoch or
/// newer) is taken up and the change made again on it,
/// [`CHANGE_TRIES`] times at most. A map the coordinator gave in a change
/// that failed, which a server may hold though none said so
/// ([`Change::in_doubt`]), is given again first, and the change made from
/// it once a server takes it.
#[derive(Clone, Debug)]
pub struct Change {
    first: Vec<String>,
    tries: u32,
    next: Option<SlotMap>,
    firsts: Vec<(usize, String)>,
    rest: Vec<(usize, String)>,
    stage: Stage,
    missed: Vec<String>,
    raced: Option<String>,
    took: usize,
    /// Whether a server did not answer for `next` (it may hold it).
    unanswered: bool,
    /// The newest epoch a server said it holds at the last sync, and how
    /// many syncs could not fetch a map that new.
    heard: u64,
    behind: u32,
    doubt: Option<SlotMap>,
    /// Whether `next` is the map in doubt, given again.
    redoing: bool,
    /// The map in doubt, once a server took it.
    base: Option<SlotMap>,
    /// Makes the next change from the map synced to although a server may
    /// hold a newer one the coordinator gave, as the server did before the
    /// simulator found that the map then forks
    /// ([`crate::sim::cluster::Bug::GivesAnEpochTwice`]).
    pub forgets_doubt: bool,
    /// Routes by a map no server took, as the server did before the
    /// simulator found that the next change then forks the map
    /// ([`crate::sim::cluster::Bug::RoutesByAMapNoServerTook`]).
    pub keeps_untaken: bool,
    /// Gives a map made while slots copy to the server they move from
    /// with the others, as the server did before the simulator found that
    /// the map then forks ([`crate::sim::cluster::Bug::ChangesAroundTheSource`]).
    pub skips_source: bool,
}

impl Change {
    /// A change whose servers of `first` take the map before any other is
    /// given it, by a coordinator whose last change left `doubt` (its
    /// [`Self::in_doubt`]).
    pub fn new(first: &[String], doubt: Option<SlotMap>) -> (Change, ChangeStep) {
        let c = Change {
            first: first.to_vec(),
            tries: 0,
            next: None,
            firsts: Vec::new(),
            rest: Vec::new(),
            stage: Stage::Syncing,
            missed: Vec::new(),
            raced: None,
            took: 0,
            unanswered: false,
            heard: 0,
            behind: 0,
            doubt,
            redoing: false,
            base: None,
            forgets_doubt: false,
            keeps_untaken: false,
            skips_source: false,
        };
        (c, ChangeStep::Sync)
    }

    /// Once the change failed, the map it gave that a server may hold
    /// though the coordinator does not route by it: one took it, or one
    /// did not answer (the answer came late, or was lost). The
    /// coordinator's next change gives it again before any other map: one
    /// of the same epoch would fork the map (two servers holding different
    /// maps of one epoch, neither ever taking the other's), and one newer,
    /// made from the map before, would undo a change the servers may act
    /// on (a flip the server slots move from took, and the others then
    /// learned of).
    pub fn in_doubt(&self) -> Option<SlotMap> {
        self.doubt.clone().filter(|_| !self.forgets_doubt)
    }

    /// The newest epoch a server said it holds at the sync just made: the
    /// change is not made from an older map ([`Self::synced`]).
    pub fn heard(&mut self, epoch: u64) {
        self.heard = epoch;
    }

    /// The map being given.
    pub fn next(&self) -> Option<&SlotMap> {
        self.next.as_ref()
    }

    /// Whether the map is being given to one of the servers that take it
    /// first.
    pub fn giving_first(&self) -> bool {
        matches!(self.stage, Stage::First { .. })
    }

    fn done(&mut self, r: Result<(SlotMap, Vec<String>), ApiError>) -> ChangeStep {
        self.stage = Stage::Done;
        ChangeStep::Done(r)
    }

    /// The map routed by after the sync is `now`; `f` makes the next one.
    /// While slots copy in `now`, the server they move from (`source`
    /// names it: the primary of the move's shard `from`) takes the next map
    /// before any other: it alone decides between the move's flip, which it
    /// takes first, and any other change, so that no server takes a map of
    /// the same epoch that the flip's servers do not (a fork of the map,
    /// under which reads miss the moving rows and a move given up deletes
    /// them).
    pub fn synced(&mut self, now: Option<&SlotMap>, f: &dyn Fn(&SlotMap) -> Result<SlotMap, String>, source: &dyn Fn(&Move) -> Option<String>) -> ChangeStep {
        let now = match (now, &self.base) {
            (Some(n), Some(b)) if b.epoch > n.epoch => b.clone(),
            (Some(n), _) => n.clone(),
            (None, Some(b)) => b.clone(),
            (None, None) => return self.done(Err(ApiError::bad("a coordinator routing by the hash before slots (a shard from before protocol 4) cannot change the cluster"))),
        };
        // A server holds a newer map than the sync could fetch: a change
        // made from an older one would fork the map (its epoch is one a
        // server holds another map of).
        if self.heard > now.epoch {
            self.behind += 1;
            if self.behind >= CHANGE_TRIES {
                let (heard, epoch) = (self.heard, now.epoch);
                return self.done(Err(unavailable(format!("a server holds cluster map epoch {heard}, newer than the epoch {epoch} this coordinator could fetch, and a change made from an older map would fork it: try again"))));
            }
            self.stage = Stage::Syncing;
            return ChangeStep::Sync;
        }
        if let Some(x) = self.doubt.clone().filter(|_| !self.forgets_doubt) {
            if x.epoch > now.epoch {
                self.redoing = true;
                return self.give(x, &now, source);
            }
            // Taken, or another map came after it.
            self.doubt = None;
        }
        let next = match f(&now) {
            Ok(m) => m,
            Err(e) => return self.done(Err(ApiError::bad(e))),
        };
        if next.epoch <= now.epoch {
            return self.done(Err(ApiError::internal("a change must raise the map's epoch")));
        }
        self.give(next, &now, source)
    }

    /// Gives `next`, made from `now`.
    fn give(&mut self, next: SlotMap, now: &SlotMap, source: &dyn Fn(&Move) -> Option<String>) -> ChangeStep {
        let mut first = self.first.clone();
        if let Some(src) = now.moving().filter(|m| m.state == MoveState::Copying && !self.skips_source).and_then(source) {
            first.retain(|u| *u != src);
            first.insert(0, src);
        }
        let all = next.replicas().iter().enumerate().flat_map(|(i, rs)| rs.iter().map(move |u| (i, u.clone())));
        let (mut firsts, rest): (Vec<_>, Vec<_>) = all.partition(|(_, u)| first.contains(u));
        firsts.sort_by_key(|(_, u)| first.iter().position(|f| f == u));
        self.firsts = firsts;
        self.rest = rest;
        self.next = Some(next);
        self.missed.clear();
        self.raced = None;
        self.took = 0;
        self.unanswered = false;
        self.give_next_first(0)
    }

    fn give_next_first(&mut self, k: usize) -> ChangeStep {
        let next = self.next.clone().expect("a map to give");
        if let Some(f) = self.firsts.get(k) {
            self.stage = Stage::First { k, tries: 1 };
            return ChangeStep::Give { map: next, to: vec![f.clone()] };
        }
        if self.rest.is_empty() {
            return self.landed(next, Vec::new());
        }
        self.stage = Stage::Rest { waiting: self.rest.iter().map(|(_, u)| u.clone()).collect() };
        ChangeStep::Give { map: next, to: self.rest.clone() }
    }

    /// `next` is made: the change is, or, the map in doubt taken, the
    /// change is made from it.
    fn landed(&mut self, next: SlotMap, missed: Vec<String>) -> ChangeStep {
        self.doubt = None;
        if std::mem::take(&mut self.redoing) {
            self.base = Some(next);
            self.stage = Stage::Syncing;
            return ChangeStep::Sync;
        }
        self.done(Ok((next, missed)))
    }

    /// `next` is not made, as `e` says.
    fn failed(&mut self, e: ApiError) -> ChangeStep {
        if self.took > 0 || self.unanswered {
            self.doubt = self.next.clone();
            if self.redoing {
                let epoch = self.next.as_ref().map_or(0, |m| m.epoch);
                return self.done(Err(unavailable(format!("cluster map epoch {epoch}, given before and maybe taken, reached no server that answered, so no other map is made: {}", e.message))));
            }
            return self.done(Err(e));
        }
        if std::mem::take(&mut self.redoing) {
            // No server holds the map in doubt: the change is made from the
            // map synced to.
            self.doubt = None;
            self.stage = Stage::Syncing;
            return ChangeStep::Sync;
        }
        self.done(Err(e))
    }

    /// Another coordinator's change came first: this one is made again on
    /// it.
    fn again(&mut self) -> ChangeStep {
        self.tries += 1;
        if self.tries >= CHANGE_TRIES {
            return self.done(Err(ApiError { status: 409, message: "other changes of the cluster map kept coming first: try again".into() }));
        }
        self.stage = Stage::Syncing;
        ChangeStep::Sync
    }

    /// Server `url` answered being given the map as `g` ([`given`]):
    /// the next step, if this answer was the last awaited.
    pub fn answered(&mut self, url: &str, g: Given) -> Option<ChangeStep> {
        let epoch = self.next.as_ref().map_or(0, |m| m.epoch);
        match self.stage.clone() {
            Stage::First { k, tries } => {
                if self.firsts.get(k).is_none_or(|(_, u)| u != url) {
                    return None;
                }
                let why = match g {
                    Given::Took => {
                        self.took += 1;
                        return Some(self.give_next_first(k + 1));
                    }
                    Given::Raced(_) => return Some(self.again()),
                    Given::Refused(why) => why,
                    Given::Missed(_) if tries < FIRST_TRIES => {
                        // A map given again is taken again: an answer lost
                        // is asked for again.
                        self.unanswered = true;
                        self.stage = Stage::First { k, tries: tries + 1 };
                        let next = self.next.clone().expect("a map to give");
                        return Some(ChangeStep::Give { map: next, to: vec![self.firsts[k].clone()] });
                    }
                    Given::Missed(why) => {
                        self.unanswered = true;
                        why
                    }
                };
                Some(self.failed(unavailable(format!("{url} did not take cluster map epoch {epoch} first, so no other server was given it: {why}"))))
            }
            Stage::Rest { mut waiting } => {
                if !waiting.remove(url) {
                    return None;
                }
                match g {
                    Given::Took => self.took += 1,
                    Given::Raced(why) => self.raced = Some(format!("{url}: {why}")),
                    Given::Refused(why) => self.missed.push(format!("{url}: {why}")),
                    Given::Missed(why) => {
                        self.unanswered = true;
                        self.missed.push(format!("{url}: {why}"));
                    }
                }
                if !waiting.is_empty() {
                    self.stage = Stage::Rest { waiting };
                    return None;
                }
                if self.raced.take().is_some() {
                    return Some(self.again());
                }
                let next = self.next.clone().expect("a map to give");
                let missed = std::mem::take(&mut self.missed);
                // A map no server holds is not the cluster's: routed by, the
                // next change would be made from it, and fork the map from
                // the one the servers hold.
                if self.took == 0 && !self.keeps_untaken {
                    return Some(self.failed(unavailable(format!("no server took cluster map epoch {}: {}", next.epoch, missed.join("; ")))));
                }
                Some(self.landed(next, missed))
            }
            Stage::Syncing | Stage::Done => None,
        }
    }
}

/// Rounds of copying what changed before writes are held for the last.
pub const CATCH_UP_ROUNDS: u32 = 8;

/// Changed rows few enough to copy while writes are held.
pub const CATCH_UP_ROWS: u64 = 2_000;

/// How long a hold on writes lasts unless asked for again, and how often
/// a coordinator asks.
pub const HOLD_TTL: Duration = Duration::from_secs(30);
pub const HOLD_RENEW: Duration = Duration::from_secs(10);

/// Whether `a` and `b` are the same move (in any state).
pub fn same_move(a: &Move, b: &Move) -> bool {
    a.from == b.from && a.to == b.to && a.slots == b.slots
}

/// A change of the map a move makes ([`Change`] runs it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Start moving `slots` (of one shard) to shard `to`.
    Start { slots: Vec<usize>, to: usize },
    /// Give the slots of the move, copying, to the shard they move to.
    Flip(Move),
    /// Give up the move, copying.
    Abort(Move),
    /// End the move, cleaning or aborting.
    End(Move),
}

impl Edit {
    /// The next map from `m`.
    pub fn apply(&self, m: &SlotMap) -> Result<SlotMap, String> {
        let moving = |mv: &Move| m.moving().filter(|x| same_move(x, mv)).cloned();
        match self {
            Edit::Start { slots, to } => m.start_move(slots, *to),
            Edit::Flip(mv) => match moving(mv) {
                Some(x) if x.state == MoveState::Copying => m.flip_move(),
                _ => Err("the move changed while its rows were copied".into()),
            },
            Edit::Abort(mv) => match moving(mv) {
                Some(x) if x.state == MoveState::Copying => m.abort_move(),
                _ => Err("the move changed meanwhile".into()),
            },
            Edit::End(mv) => match moving(mv) {
                Some(x) if x.state != MoveState::Copying => m.end_move(),
                _ => Err("the move ended meanwhile".into()),
            },
        }
    }
}

/// The next step of a [`MoveRun`].
#[derive(Clone, Debug)]
pub enum MoveStep {
    /// Change the map with `edit` ([`Change`]); with `first_from`, the
    /// server the move's rows come from takes it first. Answer with
    /// [`MoveRun::changed`].
    Change { edit: Edit, first_from: bool },
    /// Find the server of shard `from` the rows come from (its primary),
    /// for the steps after: [`MoveRun::found`].
    Source,
    /// Have shard `shard` delete the rows of moving slots it holds without
    /// serving them: [`MoveRun::cleaned`].
    Cleanup { shard: usize },
    /// Export the move's rows from the source (every row, or what changed
    /// since the export before), writing them to the shard they move to:
    /// [`MoveRun::exported`].
    Export { changes: bool },
    /// Hold writes to the moving slots at the source, and ask again every
    /// [`HOLD_RENEW`] while [`MoveRun::renewing`]: [`MoveRun::held`].
    Hold,
    /// End the hold (a failure only waits out its time): [`MoveRun::thawed`].
    Thaw,
    /// Take up the newest map the servers hold: [`MoveRun::synced`].
    Sync,
    /// The move ended: whether its slots moved (false: given up, and the
    /// rows sent deleted), or why it failed.
    Done(Result<bool, ApiError>),
}

/// Where a move is, as `GET /v1/admin/moves` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Planned,
    Copying,
    CatchingUp,
    Holding,
    Flipping,
    Cleaning,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Planned => "planned",
            Phase::Copying => "copying",
            Phase::CatchingUp => "catching up",
            Phase::Holding => "holding writes",
            Phase::Flipping => "flipping",
            Phase::Cleaning => "cleaning",
        }
    }
}

#[derive(Clone, Debug)]
enum Run {
    Starting,
    Finding,
    CleaningTo,
    FirstExport,
    CatchingUp,
    Holding,
    LastExport,
    Flipping,
    /// The hold ends after the flip, which went as said.
    Thawing(Result<(), ApiError>),
    /// After a failure: whether the flip was taken although its answer
    /// was lost.
    Checking,
    Aborting,
    AbortThawing,
    /// The holder of the slots' rows left behind deletes them.
    Settling,
    Ending,
    Done,
}

/// A move of slots between two shards while the cluster serves, as a
/// coordinator's job runs it ([`crate::coord::moves`]): started in the map
/// (copying), the rows copied from the shard the slots leave to the one
/// they go to, then what changed, round after round until little does;
/// writes to them held at the source, the last changes copied, and the
/// slots given to the other shard (cleaning), the source first; the
/// source deletes its copies and the move ends. One that fails before its
/// flip is given up (aborting): the shard the slots were to go to deletes
/// what it was sent. [`MoveRun::finish`] ends a move another coordinator
/// left.
#[derive(Clone, Debug)]
pub struct MoveRun {
    run: Run,
    mv: Option<Move>,
    phase: Phase,
    rounds: u32,
    last: u64,
    renewing: bool,
    /// What the move ends with once it settles.
    outcome: Option<Result<bool, ApiError>>,
}

impl MoveRun {
    /// Moves `slots` (of one shard) to shard `to`.
    pub fn new(slots: Vec<usize>, to: usize) -> (MoveRun, MoveStep) {
        let r = MoveRun { run: Run::Starting, mv: None, phase: Phase::Planned, rounds: 0, last: 0, renewing: false, outcome: None };
        (r, MoveStep::Change { edit: Edit::Start { slots, to }, first_from: false })
    }

    /// Finishes the move in `now` (the map after a sync) that no job runs:
    /// one flipped ends once the shard the slots left deletes its copies;
    /// one copying, or being given up, is given up. None when nothing
    /// moves.
    pub fn finish(now: &SlotMap) -> Option<(MoveRun, MoveStep)> {
        let mv = now.moving()?.clone();
        let mut r = MoveRun { run: Run::Checking, mv: Some(mv.clone()), phase: Phase::Cleaning, rounds: 0, last: 0, renewing: false, outcome: None };
        let step = match mv.state {
            MoveState::Cleaning => {
                r.outcome = Some(Ok(true));
                r.settle(&mv)
            }
            MoveState::Copying | MoveState::Aborting => {
                r.outcome = Some(Ok(false));
                r.give_up(now)
            }
        };
        Some((r, step))
    }

    /// The move, once started.
    pub fn moving(&self) -> Option<&Move> {
        self.mv.as_ref()
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn rounds(&self) -> u32 {
        self.rounds
    }

    /// Whether the hold at the source is to be asked for again.
    pub fn renewing(&self) -> bool {
        self.renewing
    }

    fn done(&mut self, r: Result<bool, ApiError>) -> MoveStep {
        self.run = Run::Done;
        self.renewing = false;
        MoveStep::Done(r)
    }

    fn mv(&self) -> Move {
        self.mv.clone().expect("a started move")
    }

    /// A step before the flip failed with `e`: was the flip taken anyway?
    fn fail(&mut self, e: ApiError) -> MoveStep {
        self.renewing = false;
        self.outcome = Some(Err(e));
        self.run = Run::Checking;
        MoveStep::Sync
    }

    /// Gives the move up, in `now`: the map says so (aborting), the hold
    /// ends, and the shard the slots were to go to deletes what it was
    /// sent.
    fn give_up(&mut self, now: &SlotMap) -> MoveStep {
        let mv = self.mv();
        if now.moving().is_some_and(|x| same_move(x, &mv) && x.state == MoveState::Copying) {
            self.run = Run::Aborting;
            return MoveStep::Change { edit: Edit::Abort(mv), first_from: false };
        }
        self.settle(&Move { state: MoveState::Aborting, ..mv })
    }

    /// Ends `mv` (cleaning or aborting): the holder of rows of its slots
    /// that it does not serve deletes them, then the map drops it.
    fn settle(&mut self, mv: &Move) -> MoveStep {
        self.mv = Some(mv.clone());
        self.run = Run::Settling;
        let holder = if mv.state == MoveState::Cleaning { mv.from } else { mv.to };
        MoveStep::Cleanup { shard: holder }
    }

    fn flipped(&self) -> Move {
        Move { state: MoveState::Cleaning, ..self.mv() }
    }

    /// A change of the map asked for ended: `r`, and the map routed by now.
    pub fn changed(&mut self, r: Result<SlotMap, ApiError>, now: Option<&SlotMap>) -> MoveStep {
        match self.run.clone() {
            Run::Starting => match r {
                Ok(m) => match m.moving().cloned() {
                    Some(mv) => {
                        self.mv = Some(mv);
                        self.run = Run::Finding;
                        MoveStep::Source
                    }
                    None => self.done(Err(ApiError::internal("a started move is not in the map"))),
                },
                Err(e) => self.done(Err(e)),
            },
            Run::Flipping => {
                self.renewing = false;
                self.run = Run::Thawing(r.map(|_| ()));
                MoveStep::Thaw
            }
            Run::Aborting => match r {
                Ok(_) => {
                    self.run = Run::AbortThawing;
                    MoveStep::Thaw
                }
                Err(e) => self.done(Err(e)),
            },
            Run::Ending => {
                let mv = self.mv();
                let outcome = self.outcome.take().unwrap_or(Ok(true));
                match r {
                    Ok(_) => self.done(outcome),
                    // Ended by another coordinator meanwhile.
                    Err(_) if now.is_some_and(|m| m.moving().is_none_or(|x| !same_move(x, &mv))) => self.done(outcome),
                    Err(e) => self.done(Err(e)),
                }
            }
            _ => self.done(Err(ApiError::internal("a change of the map the move did not ask for"))),
        }
    }

    /// The source was found, or not.
    pub fn found(&mut self, r: Result<(), ApiError>) -> MoveStep {
        match r {
            Ok(()) => {
                // What an earlier try sent the shard they move to.
                self.run = Run::CleaningTo;
                MoveStep::Cleanup { shard: self.mv().to }
            }
            Err(e) => self.fail(e),
        }
    }

    /// A cleanup asked for ended: the rows deleted, or why not.
    pub fn cleaned(&mut self, r: Result<u64, ApiError>) -> MoveStep {
        match (self.run.clone(), r) {
            (Run::CleaningTo, Ok(_)) => {
                self.phase = Phase::Copying;
                self.run = Run::FirstExport;
                MoveStep::Export { changes: false }
            }
            (Run::CleaningTo, Err(e)) => self.fail(e),
            (Run::Settling, Ok(_)) => {
                self.run = Run::Ending;
                MoveStep::Change { edit: Edit::End(self.mv()), first_from: false }
            }
            (Run::Settling, Err(e)) => self.done(Err(e)),
            _ => self.done(Err(ApiError::internal("a cleanup the move did not ask for"))),
        }
    }

    /// An export asked for ended: the rows it sent and whether the source
    /// holds the writes to the slots from the start (it cannot tell what
    /// changed), or why it failed.
    pub fn exported(&mut self, r: Result<(u64, bool), ApiError>) -> MoveStep {
        let (rows, frozen) = match r {
            Ok(a) => a,
            Err(e) => return self.fail(e),
        };
        match self.run {
            Run::FirstExport if frozen => {
                self.renewing = true;
                self.flip()
            }
            Run::FirstExport | Run::CatchingUp => {
                if matches!(self.run, Run::CatchingUp) {
                    self.rounds += 1;
                }
                self.last = rows;
                if self.last > CATCH_UP_ROWS && self.rounds < CATCH_UP_ROUNDS {
                    self.phase = Phase::CatchingUp;
                    self.run = Run::CatchingUp;
                    return MoveStep::Export { changes: true };
                }
                self.phase = Phase::Holding;
                self.run = Run::Holding;
                MoveStep::Hold
            }
            Run::LastExport => self.flip(),
            _ => self.done(Err(ApiError::internal("an export the move did not ask for"))),
        }
    }

    fn flip(&mut self) -> MoveStep {
        self.phase = Phase::Flipping;
        self.run = Run::Flipping;
        MoveStep::Change { edit: Edit::Flip(self.mv()), first_from: true }
    }

    /// The hold asked for was taken, or not.
    pub fn held(&mut self, r: Result<u64, ApiError>) -> MoveStep {
        match (self.run.clone(), r) {
            (Run::Holding, Ok(_)) => {
                self.renewing = true;
                self.run = Run::LastExport;
                MoveStep::Export { changes: true }
            }
            (Run::Holding, Err(e)) => self.fail(e),
            _ => self.done(Err(ApiError::internal("a hold the move did not ask for"))),
        }
    }

    /// The hold was ended (or its end failed, and it waits out its time).
    pub fn thawed(&mut self) -> MoveStep {
        match self.run.clone() {
            Run::Thawing(Ok(())) => {
                self.phase = Phase::Cleaning;
                self.outcome = Some(Ok(true));
                let f = self.flipped();
                self.settle(&f)
            }
            Run::Thawing(Err(e)) => self.fail(e),
            Run::AbortThawing => {
                let mv = Move { state: MoveState::Aborting, ..self.mv() };
                self.settle(&mv)
            }
            _ => self.done(Err(ApiError::internal("a thaw the move did not ask for"))),
        }
    }

    /// The map routed by after a sync is `now`.
    pub fn synced(&mut self, now: Option<&SlotMap>) -> MoveStep {
        if !matches!(self.run, Run::Checking) {
            return self.done(Err(ApiError::internal("a sync the move did not ask for")));
        }
        let mv = self.mv();
        // The flip may have been taken although its answer was lost.
        if now.and_then(|m| m.moving()).is_some_and(|x| same_move(x, &mv) && x.state == MoveState::Cleaning) {
            self.phase = Phase::Cleaning;
            self.outcome = Some(Ok(true));
            let f = self.flipped();
            return self.settle(&f);
        }
        match now {
            Some(m) => self.give_up(m),
            None => {
                let e = self.outcome.take().and_then(Result::err).unwrap_or_else(|| ApiError::internal("no map to give the move up in"));
                self.done(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots::SLOTS;

    fn no_source(_: &Move) -> Option<String> {
        None
    }

    fn map(n: usize) -> SlotMap {
        SlotMap::even(n, 1).with_replicas((0..n).map(|i| vec![format!("s{i}")]).collect()).unwrap()
    }

    #[test]
    fn a_shard_takes_newer_maps_and_keeps_serving_its_slots() {
        let m1 = map(2);
        assert_eq!(take(None, 0, &m1, 0).unwrap(), Take::Keep { since: 1, check_rows: true });
        assert_eq!(take(Some((&m1, 0)), 1, &m1, 0).unwrap(), Take::Held);
        assert_eq!(take(Some((&m1, 0)), 1, &m1, 1).unwrap_err().status, 409);
        let m2 = m1.start_move(&[0, 1], 1).unwrap();
        assert_eq!(take(Some((&m2, 0)), 1, &m1, 0).unwrap_err().status, 409);
        // Shard 0 holds the same slots while they copy: it serves epoch 1.
        assert_eq!(take(Some((&m1, 0)), 1, &m2, 0).unwrap(), Take::Keep { since: 1, check_rows: false });
        let m3 = m2.flip_move().unwrap();
        // The flip changes its slots: only the new epoch, no row check (a
        // step of the move).
        assert_eq!(take(Some((&m2, 0)), 1, &m3, 0).unwrap(), Take::Keep { since: 3, check_rows: false });
        // Skipping the steps changes the slots: the rows are checked.
        assert_eq!(take(Some((&m1, 1)), 1, &m3, 1).unwrap(), Take::Keep { since: 3, check_rows: true });
        assert!(epoch_ok(3, 3, 3).is_ok() && epoch_ok(3, 1, 2).is_ok() && epoch_ok(3, 0, 0).is_ok());
        assert!(epoch_ok(3, 3, 2).unwrap_err().message.contains(OLDER_THAN_SHARD));
        assert!(epoch_ok(2, 1, 3).unwrap_err().message.contains(OLDER_THAN_REQUEST));
        assert!(epoch_ok(0, 0, 3).unwrap_err().message.contains(NO_MAP_YET));
    }

    #[test]
    fn a_shard_refuses_rows_of_slots_it_does_not_hold() {
        let m1 = map(2);
        let mine = 0;
        let theirs = SLOTS - 1;
        assert_eq!(misplaced(&m1, 0, mine, Moved::No, 1), None);
        assert_eq!(misplaced(&m1, 0, theirs, Moved::No, 1), Some(Misplaced::Elsewhere));
        let m2 = m1.start_move(&[theirs], 0).unwrap();
        // The move's rows pass to the shard they go to when copied.
        assert_eq!(misplaced(&m2, 0, theirs, Moved::Copy, 2), None);
        assert_eq!(misplaced(&m2, 0, theirs, Moved::No, 2), Some(Misplaced::Elsewhere));
        // A copy that arrives once the move is given up does not, but the
        // cleanup of what was sent does.
        let aborted = m2.abort_move().unwrap();
        assert_eq!(misplaced(&aborted, 0, theirs, Moved::Copy, 3), Some(Misplaced::Elsewhere));
        assert_eq!(misplaced(&aborted, 0, theirs, Moved::Copy, 2), Some(Misplaced::Routed { routed: 2 }));
        assert_eq!(misplaced(&aborted, 0, theirs, Moved::Cleanup, 3), None);
        let m3 = m2.flip_move().unwrap();
        // Routed by the map before the flip: route again.
        assert_eq!(misplaced(&m3, 1, theirs, Moved::No, 2), Some(Misplaced::Routed { routed: 2 }));
        assert_eq!(misplaced(&m3, 1, theirs, Moved::Cleanup, 3), None);
        assert!(Misplaced::Routed { routed: 2 }.error(&m3, 1, theirs, "key", "7").message.contains(OLDER_THAN_SHARD));
        assert!(gated(&m2, 1, &[theirs], false) && !gated(&m2, 1, &[mine], false) && gated(&m2, 1, &[], true) && !gated(&m2, 0, &[theirs], false));
        assert!(after_hold(2, 2).is_ok() && after_hold(2, 3).unwrap_err().message.contains(OLDER_THAN_SHARD));
        assert_eq!(moving_from_here(Some((&m2, 1))).unwrap().to, 0);
        assert_eq!(moving_from_here(Some((&m3, 1))).unwrap_err().status, 409);
        let mv = m2.moving().unwrap().clone();
        let exported = (mv.clone(), 7u64);
        assert_eq!(export_from(true, Some(&exported), &mv, true).unwrap(), Some(&7));
        assert_eq!(export_from::<u64>(true, None, &mv, true).unwrap_err().status, 409);
        assert_eq!(export_from(false, Some(&exported), &mv, true).unwrap(), None);
    }

    #[test]
    fn a_hold_ends_with_the_time_of_its_last_ask() {
        let mut h = Hold::default();
        let a = h.freeze(5);
        let b = h.freeze(9);
        assert!(!h.expire(a) && h.held());
        // Held since write 5: an export at 7 holds every write.
        assert!(h.covers(Some(7)) && h.covers(Some(5)) && !h.covers(Some(4)) && !h.covers(None));
        assert!(h.expire(b) && !h.held() && !h.covers(Some(7)));
        // Asked for again after it ran out: held since then only.
        h.freeze(12);
        assert!(!h.covers(Some(7)) && h.covers(Some(12)));
        h.thaw();
        assert!(!h.held());
    }

    #[test]
    fn a_shard_gives_its_moving_slots_away_only_while_it_held_their_writes() {
        let m2 = map(2).start_move(&[0, 1], 1).unwrap();
        let m3 = m2.flip_move().unwrap();
        assert!(flip_ok(Some((&m2, 0)), &m3, 0, true).is_ok());
        assert_eq!(flip_ok(Some((&m2, 0)), &m3, 0, false).unwrap_err().status, 409);
        // Given up, or another change: the slots stay.
        assert!(flip_ok(Some((&m2, 0)), &m2.abort_move().unwrap(), 0, false).is_ok());
        assert!(flip_ok(Some((&m2, 0)), &m2.set_servers(1, vec!["s9".into()]).unwrap(), 0, false).is_ok());
        // The shard the slots go to, and one with no move, need no hold.
        assert!(flip_ok(Some((&m2, 1)), &m3, 1, false).is_ok());
        assert!(flip_ok(None, &m3, 0, false).is_ok());
    }

    #[test]
    fn a_change_gives_the_first_servers_the_map_first_and_retries_a_race() {
        let m1 = map(2);
        let (mut c, s) = Change::new(&["s1".to_string()], None);
        assert!(matches!(s, ChangeStep::Sync));
        let f = |m: &SlotMap| m.start_move(&[0], 1);
        let ChangeStep::Give { map: next, to } = c.synced(Some(&m1), &f, &no_source) else { panic!() };
        assert_eq!(to, vec![(1, "s1".to_string())]);
        // A lost answer: given again.
        assert!(matches!(c.answered("s1", Given::Missed("timeout".into())), Some(ChangeStep::Give { .. })));
        let Some(ChangeStep::Give { to, .. }) = c.answered("s1", Given::Took) else { panic!() };
        assert_eq!(to, vec![(0, "s0".to_string())]);
        // Another coordinator's map of the same epoch came first.
        let other = m1.start_move(&[1], 1).unwrap();
        assert_eq!(given(Err(conflict("different".into())), Some(&other), &next), Given::Raced("different".into()));
        assert!(matches!(c.answered("s0", Given::Raced("x".into())), Some(ChangeStep::Sync)));
        // On the other map, the move cannot start.
        let Some(ChangeStep::Done(Err(e))) = Some(c.synced(Some(&other), &f, &no_source)) else { panic!() };
        assert_eq!(e.status, 400);
        // A first server that refuses stops the change.
        let (mut c, _) = Change::new(&["s0".to_string()], None);
        c.synced(Some(&m1), &f, &no_source);
        let Some(ChangeStep::Done(Err(e))) = c.answered("s0", Given::Refused("no".into())) else { panic!() };
        assert_eq!(e.status, 503);
        // Those after the first that miss it get it later.
        let (mut c, _) = Change::new(&[], None);
        let ChangeStep::Give { to, .. } = c.synced(Some(&m1), &f, &no_source) else { panic!() };
        assert_eq!(to.len(), 2);
        assert!(c.answered("s0", Given::Took).is_none());
        let Some(ChangeStep::Done(Ok((m, missed)))) = c.answered("s1", Given::Missed("down".into())) else { panic!() };
        assert_eq!((m.epoch, missed.len()), (2, 1));
        // A map no server took is not made.
        let (mut c, _) = Change::new(&[], None);
        c.synced(Some(&m1), &f, &no_source);
        assert!(c.answered("s0", Given::Missed("down".into())).is_none());
        let Some(ChangeStep::Done(Err(e))) = c.answered("s1", Given::Missed("down".into())) else { panic!() };
        assert!(e.status == 503 && e.message.contains("no server took"), "{e:?}");
        // While slots copy, the server they move from takes any change
        // first, before the servers asked to.
        let copying = m1.start_move(&[0], 1).unwrap();
        let (mut c, _) = Change::new(&["s1".to_string()], None);
        let source = |m: &Move| Some(format!("s{}", m.from));
        let ChangeStep::Give { to, .. } = c.synced(Some(&copying), &|m: &SlotMap| m.set_servers(1, vec!["s1".into()]), &source) else { panic!() };
        assert_eq!(to, vec![(0, "s0".to_string())]);
        let Some(ChangeStep::Give { to, .. }) = c.answered("s0", Given::Took) else { panic!() };
        assert_eq!(to, vec![(1, "s1".to_string())]);
    }

    #[test]
    fn a_map_no_server_answered_for_is_given_again_before_another() {
        let m1 = map(2);
        let f = |m: &SlotMap| m.start_move(&[0], 1);
        let g = |m: &SlotMap| m.set_servers(1, vec!["s1".into()]);
        let (mut c, _) = Change::new(&[], None);
        let ChangeStep::Give { map: x, .. } = c.synced(Some(&m1), &f, &no_source) else { panic!() };
        assert!(c.answered("s0", Given::Missed("no answer".into())).is_none());
        let Some(ChangeStep::Done(Err(_))) = c.answered("s1", Given::Missed("no answer".into())) else { panic!() };
        assert_eq!(c.in_doubt().as_ref(), Some(&x), "a server may hold it");
        // Synced to the map before (the answers still slow), the next change
        // gives that map again first, then makes its own from it.
        let (mut c, _) = Change::new(&[], Some(x.clone()));
        let ChangeStep::Give { map, .. } = c.synced(Some(&m1), &g, &no_source) else { panic!() };
        assert_eq!(map, x);
        assert!(c.answered("s0", Given::Took).is_none());
        let Some(ChangeStep::Sync) = c.answered("s1", Given::Missed("no answer".into())) else { panic!() };
        let ChangeStep::Give { map: y, .. } = c.synced(Some(&m1), &g, &no_source) else { panic!() };
        assert_eq!(y, g(&x).unwrap(), "made from the map given again");
        c.answered("s0", Given::Took);
        let Some(ChangeStep::Done(Ok((made, _)))) = c.answered("s1", Given::Took) else { panic!() };
        assert!(made.epoch == 3 && c.in_doubt().is_none());
        // Still unanswered: no other map is made.
        let (mut c, _) = Change::new(&[], Some(x.clone()));
        c.synced(Some(&m1), &g, &no_source);
        c.answered("s0", Given::Missed("no answer".into()));
        let Some(ChangeStep::Done(Err(e))) = c.answered("s1", Given::Missed("no answer".into())) else { panic!() };
        assert!(e.status == 503 && c.in_doubt() == Some(x.clone()), "{e:?}");
        // Refused by every server it was given to: no server holds it.
        let (mut c, _) = Change::new(&[], Some(x.clone()));
        c.synced(Some(&m1), &g, &no_source);
        c.answered("s0", Given::Refused("no".into()));
        let Some(ChangeStep::Sync) = c.answered("s1", Given::Refused("no".into())) else { panic!() };
        let ChangeStep::Give { map: y, .. } = c.synced(Some(&m1), &g, &no_source) else { panic!() };
        assert_eq!(y, g(&m1).unwrap());
        // Synced to it, or past it: nothing to give again.
        let (mut c, _) = Change::new(&[], Some(x.clone()));
        let ChangeStep::Give { map: y, .. } = c.synced(Some(&x), &g, &no_source) else { panic!() };
        assert_eq!(y, g(&x).unwrap());
        // As the server made it before: another map of the epoch given,
        // which a server holding that one refuses, and the map forks.
        let (mut c, _) = Change::new(&[], Some(x.clone()));
        c.forgets_doubt = true;
        let ChangeStep::Give { map: y, .. } = c.synced(Some(&m1), &g, &no_source) else { panic!() };
        assert_eq!(y.epoch, x.epoch);
        assert_eq!(take(Some((&x, 0)), 1, &y, 0).unwrap_err().status, 409);
    }

    #[test]
    fn a_change_is_not_made_from_a_map_older_than_one_a_server_holds() {
        let m1 = map(2);
        let g = |m: &SlotMap| m.set_servers(1, vec!["s1".into()]);
        let (mut c, _) = Change::new(&[], None);
        // A server said it holds epoch 2, which the sync could not fetch.
        for _ in 1..CHANGE_TRIES {
            c.heard(2);
            assert!(matches!(c.synced(Some(&m1), &g, &no_source), ChangeStep::Sync), "synced again");
        }
        c.heard(2);
        let ChangeStep::Done(Err(e)) = c.synced(Some(&m1), &g, &no_source) else { panic!() };
        assert!(e.status == 503 && e.message.contains("epoch 2"), "{e:?}");
        // Fetched, or none newer: made.
        let (mut c, _) = Change::new(&[], None);
        c.heard(2);
        c.synced(Some(&m1), &g, &no_source);
        c.heard(1);
        let ChangeStep::Give { map, .. } = c.synced(Some(&m1), &g, &no_source) else { panic!() };
        assert_eq!(map.epoch, 2);
    }

    #[test]
    fn a_move_copies_holds_flips_and_cleans_up() {
        let m1 = map(2);
        let (mut r, s) = MoveRun::new(vec![SLOTS - 1], 0);
        let MoveStep::Change { edit, .. } = s else { panic!() };
        let m2 = edit.apply(&m1).unwrap();
        assert!(matches!(r.changed(Ok(m2.clone()), Some(&m2)), MoveStep::Source));
        assert!(matches!(r.found(Ok(())), MoveStep::Cleanup { shard: 0 }));
        assert!(matches!(r.cleaned(Ok(0)), MoveStep::Export { changes: false }));
        // Many rows changed: another round.
        assert!(matches!(r.exported(Ok((5000, false))), MoveStep::Export { changes: true }));
        assert_eq!(r.phase(), Phase::CatchingUp);
        assert!(matches!(r.exported(Ok((10, false))), MoveStep::Hold));
        assert!(matches!(r.held(Ok(1)), MoveStep::Export { changes: true }));
        assert!(r.renewing());
        let MoveStep::Change { edit, first_from: true } = r.exported(Ok((1, false))) else { panic!() };
        let m3 = edit.apply(&m2).unwrap();
        assert!(matches!(r.changed(Ok(m3.clone()), Some(&m3)), MoveStep::Thaw));
        assert!(!r.renewing());
        // The slots' old shard deletes its copies.
        assert!(matches!(r.thawed(), MoveStep::Cleanup { shard: 1 }));
        let MoveStep::Change { edit, .. } = r.cleaned(Ok(1)) else { panic!() };
        let m4 = edit.apply(&m3).unwrap();
        assert!(m4.moving().is_none() && m4.owner(SLOTS - 1) == 0);
        assert!(matches!(r.changed(Ok(m4.clone()), Some(&m4)), MoveStep::Done(Ok(true))));
    }

    #[test]
    fn a_move_that_fails_before_its_flip_is_given_up() {
        let m1 = map(2);
        let (mut r, _) = MoveRun::new(vec![SLOTS - 1], 0);
        let m2 = m1.start_move(&[SLOTS - 1], 0).unwrap();
        r.changed(Ok(m2.clone()), Some(&m2));
        r.found(Ok(()));
        r.cleaned(Ok(0));
        assert!(matches!(r.exported(Err(ApiError::internal("cut"))), MoveStep::Sync));
        let MoveStep::Change { edit: Edit::Abort(_), .. } = r.synced(Some(&m2)) else { panic!() };
        let m3 = m2.abort_move().unwrap();
        assert!(matches!(r.changed(Ok(m3.clone()), Some(&m3)), MoveStep::Thaw));
        // The shard the slots were to go to deletes what it was sent.
        assert!(matches!(r.thawed(), MoveStep::Cleanup { shard: 0 }));
        let MoveStep::Change { edit, .. } = r.cleaned(Ok(3)) else { panic!() };
        let m4 = edit.apply(&m3).unwrap();
        let MoveStep::Done(Err(e)) = r.changed(Ok(m4.clone()), Some(&m4)) else { panic!() };
        assert_eq!(e.message, "cut");
        // A flip whose answer was lost, but taken, settles as moved.
        let (mut r, _) = MoveRun::new(vec![SLOTS - 1], 0);
        r.changed(Ok(m2.clone()), Some(&m2));
        r.found(Ok(()));
        r.cleaned(Ok(0));
        r.exported(Ok((1, false)));
        r.held(Ok(1));
        r.exported(Ok((0, false)));
        assert!(matches!(r.changed(Err(ApiError::internal("lost")), Some(&m2)), MoveStep::Thaw));
        assert!(matches!(r.thawed(), MoveStep::Sync));
        let flipped = m2.flip_move().unwrap();
        assert!(matches!(r.synced(Some(&flipped)), MoveStep::Cleanup { shard: 1 }));
        // A move another coordinator left, flipped, is finished as moved.
        let (mut f, s) = MoveRun::finish(&flipped).unwrap();
        assert!(matches!(s, MoveStep::Cleanup { shard: 1 }));
        let MoveStep::Change { edit, .. } = f.cleaned(Ok(0)) else { panic!() };
        // Ended meanwhile by the other: done all the same.
        let ended = edit.apply(&flipped).unwrap();
        assert!(matches!(f.changed(Err(ApiError::bad("the move ended meanwhile")), Some(&ended)), MoveStep::Done(Ok(true))));
        assert!(MoveRun::finish(&m1).is_none());
    }
}
