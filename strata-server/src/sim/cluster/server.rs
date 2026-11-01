//! A shard's server in the simulator: its rows, its cluster map and the
//! decisions of [`crate::cores::slots`] on what is routed to it.

use std::collections::BTreeMap;

use strata::value::Value as Key;

use super::{Bug, Req, Resp, Sim, Timer, Value};
use crate::cores::slots::{self as core, Hold, Moved, Take};
use crate::slots::{self, Move, SlotMap};
use crate::ApiError;

/// A row, and the number of the write that left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub value: Value,
    pub seq: u64,
}

/// A write waiting for a hold on writes to moving slots to end.
#[derive(Clone, Debug)]
struct Waiting {
    call: u64,
    from: usize,
    key: i64,
    value: Value,
    /// The epoch of the map it was checked by.
    checked: u64,
}

pub(crate) struct Server {
    pub up: bool,
    pub inc: u64,
    // On disk.
    /// Every key written here (deletes leave a row of no value).
    pub rows: BTreeMap<i64, Row>,
    pub seq: u64,
    pub map: Option<(SlotMap, usize)>,
    pub since: u64,
    // In memory.
    hold: Hold,
    exported: Option<(Move, u64)>,
    waiting: Vec<Waiting>,
}

/// The slot of key `k`, as a row keyed by the integer `k` is placed.
pub fn slot_of(k: i64) -> usize {
    slots::slot(&Key::Int(k))
}

impl Server {
    pub fn new(_i: usize, map: Option<(SlotMap, usize)>) -> Server {
        let since = map.as_ref().map_or(0, |(m, _)| m.epoch);
        Server { up: true, inc: 0, rows: BTreeMap::new(), seq: 0, map, since, hold: Hold::default(), exported: None, waiting: Vec::new() }
    }

    pub fn crash(&mut self) {
        self.up = false;
        self.inc += 1;
        self.hold = Hold::default();
        self.exported = None;
        self.waiting.clear();
    }

    fn held(&self) -> Option<(&SlotMap, usize)> {
        self.map.as_ref().map(|(m, s)| (m, *s))
    }

    fn epoch(&self) -> u64 {
        self.map.as_ref().map_or(0, |(m, _)| m.epoch)
    }

    fn check_epoch(&self, epoch: u64) -> Result<(), ApiError> {
        core::epoch_ok(self.epoch(), self.since, epoch)
    }

    /// The slots whose rows this server holds without serving them.
    pub fn residue(&self) -> &[usize] {
        self.map.as_ref().and_then(|(m, s)| m.residue(*s)).unwrap_or_default()
    }

    /// The value of `k` a read sees here: none for a row it hides.
    pub fn visible(&self, k: i64) -> Option<Value> {
        let row = self.rows.get(&k)?;
        if row.value.is_none() || self.residue().binary_search(&slot_of(k)).is_ok() {
            return None;
        }
        Some(row.value)
    }

    fn put(&mut self, k: i64, value: Value) -> u64 {
        self.seq += 1;
        self.rows.insert(k, Row { value, seq: self.seq });
        self.seq
    }

    /// Whether this server would refuse a row of key `k` routed by epoch
    /// `routed`, written as `moved` says.
    fn place(&self, k: i64, moved: Moved, routed: u64) -> Result<(), ApiError> {
        let Some((map, shard)) = self.held() else { return Ok(()) };
        let s = slot_of(k);
        match core::misplaced(map, shard, s, moved, routed) {
            None => Ok(()),
            Some(m) => Err(m.error(map, shard, s, "key", &k.to_string())),
        }
    }

    /// Takes `map` as shard `shard` ([`core::take`]), giving away the
    /// slots moving from it only while it held their writes since their
    /// last export ([`core::flip_ok`], unless `unheld`).
    fn give(&mut self, map: SlotMap, shard: usize, unheld: bool) -> Result<u64, ApiError> {
        if !unheld {
            let exported = self.exported.as_ref().filter(|(m, _)| self.held().and_then(|(h, s)| core::exporting(h, s)) == Some(m)).map(|(_, at)| *at);
            core::flip_ok(self.held(), &map, shard, self.hold.covers(exported))?;
        }
        match core::take(self.held(), self.since, &map, shard)? {
            Take::Held => return Ok(map.epoch),
            Take::Keep { since, check_rows } => {
                if check_rows {
                    let residue = map.residue(shard).unwrap_or_default();
                    let elsewhere: Vec<i64> = self.rows.iter().filter(|(&k, r)| r.value.is_some() && map.owner(slot_of(k)) != shard && residue.binary_search(&slot_of(k)).is_err()).map(|(&k, _)| k).collect();
                    if let Some(&k) = elsewhere.first() {
                        return Err(ApiError {
                            status: 409,
                            message: format!(
                                "{} rows here have keys in other shards' slots under slot map epoch {} (key {k} is in slot {}, which shard {} holds, not {shard})",
                                elsewhere.len(),
                                map.epoch,
                                slot_of(k),
                                map.owner(slot_of(k))
                            ),
                        });
                    }
                }
                if !core::keeps_export(&map, shard) {
                    self.exported = None;
                }
                self.since = since;
                let epoch = map.epoch;
                self.map = Some((map, shard));
                Ok(epoch)
            }
        }
    }
}

impl Sim {
    /// Server `i` serves call `call` of node `from`.
    pub(crate) fn serve(&mut self, i: usize, from: usize, call: u64, req: Req) {
        let resp = match req {
            Req::Hello => Resp::Hello { epoch: self.servers[i].epoch() },
            Req::HeldMap => Resp::HeldMap(self.servers[i].map.clone()),
            Req::Give { map, shard } => {
                let epoch = map.epoch;
                let m = map.clone();
                let unheld = self.cfg.bugs.contains(&Bug::FlipsAnUnheldMove);
                let r = self.servers[i].give(map, shard, unheld);
                if r.is_ok() {
                    self.history.took(i, &m, self.now);
                }
                self.note(|| format!("s{i} {} map epoch {epoch} as shard {shard}{}", if r.is_ok() { "takes" } else { "refuses" }, r.as_ref().err().map(|e| format!(": {}", e.message)).unwrap_or_default()));
                Resp::Given(r)
            }
            Req::Write { key, value, epoch } => {
                let s = &self.servers[i];
                let r = s.check_epoch(epoch).and_then(|_| s.place(key, Moved::No, epoch));
                if let Err(e) = r {
                    Resp::Wrote(Err(e))
                } else {
                    let gated = s.held().is_some_and(|(m, shard)| core::gated(m, shard, &[slot_of(key)], false));
                    if gated && s.hold.held() {
                        let checked = s.epoch();
                        self.servers[i].waiting.push(Waiting { call, from, key, value, checked });
                        return;
                    }
                    Resp::Wrote(Ok(self.servers[i].put(key, value)))
                }
            }
            Req::Moved { rows, epoch } => {
                let s = &self.servers[i];
                let moved = if self.cfg.bugs.contains(&Bug::TakesLateCopies) { Moved::Cleanup } else { Moved::Copy };
                let r = s.check_epoch(epoch).and_then(|_| rows.iter().try_for_each(|&(k, _)| s.place(k, moved, epoch)));
                match r {
                    Err(e) => Resp::Wrote(Err(e)),
                    Ok(()) => {
                        let mut seq = 0;
                        for (k, v) in rows {
                            seq = self.servers[i].put(k, v);
                        }
                        Resp::Wrote(Ok(seq))
                    }
                }
            }
            Req::Read { key, epoch } => {
                let s = &self.servers[i];
                Resp::Read(s.check_epoch(epoch).map(|_| s.visible(key).flatten()))
            }
            Req::Export { changes, epoch } => Resp::Exported(self.export(i, changes, epoch)),
            Req::Freeze { thaw, epoch } => Resp::Frozen(self.freeze(i, thaw, epoch)),
            Req::Cleanup { epoch } => Resp::Cleaned(self.cleanup(i, epoch)),
        };
        self.answer(i, from, call, resp);
    }

    fn export(&mut self, i: usize, changes: bool, epoch: u64) -> Result<Vec<(i64, Value)>, ApiError> {
        let s = &self.servers[i];
        s.check_epoch(epoch)?;
        let m = core::moving_from_here(s.held())?;
        let prev = core::export_from(changes, s.exported.as_ref(), &m, true)?.copied();
        let rows: Vec<(i64, Value)> = s
            .rows
            .iter()
            .filter(|(&k, r)| m.has(slot_of(k)) && match prev {
                None => r.value.is_some(),
                Some(v) => r.seq > v,
            })
            .map(|(&k, r)| (k, r.value))
            .collect();
        let seq = s.seq;
        self.note(|| format!("s{i} exports {} rows of the move to shard {}{}", rows.len(), m.to, if changes { " (changes)" } else { "" }));
        self.servers[i].exported = Some((m, seq));
        Ok(rows)
    }

    fn freeze(&mut self, i: usize, thaw: bool, epoch: u64) -> Result<u64, ApiError> {
        self.servers[i].check_epoch(epoch)?;
        if thaw {
            self.servers[i].hold.thaw();
            self.note(|| format!("s{i} ends its hold on writes"));
            self.release(i);
        } else {
            core::moving_from_here(self.servers[i].held())?;
            let at = self.servers[i].seq;
            let id = self.servers[i].hold.freeze(at);
            let ttl = self.cfg.hold_ttl;
            self.timer(i, ttl, Timer::HoldEnds { id });
            self.note(|| format!("s{i} holds writes to the moving slots"));
        }
        Ok(self.servers[i].seq)
    }

    pub(crate) fn hold_ends(&mut self, i: usize, id: u64) {
        if self.servers[i].hold.expire(id) {
            self.note(|| format!("s{i}'s hold on writes runs out: its coordinator did not ask again"));
            self.history.stats.holds_ran_out += 1;
            self.release(i);
        }
    }

    /// The writes that waited for the hold go on, or are refused if the
    /// map changed meanwhile.
    fn release(&mut self, i: usize) {
        let waiting = std::mem::take(&mut self.servers[i].waiting);
        for w in waiting {
            let now = self.servers[i].epoch();
            let r = core::after_hold(w.checked, now).map(|_| self.servers[i].put(w.key, w.value));
            self.answer(i, w.from, w.call, Resp::Wrote(r));
        }
    }

    fn cleanup(&mut self, i: usize, epoch: u64) -> Result<u64, ApiError> {
        self.servers[i].check_epoch(epoch)?;
        let residue = self.servers[i].residue().to_vec();
        let keys: Vec<i64> = self.servers[i].rows.iter().filter(|(&k, r)| r.value.is_some() && residue.binary_search(&slot_of(k)).is_ok()).map(|(&k, _)| k).collect();
        for &k in &keys {
            self.servers[i].put(k, None);
        }
        self.note(|| format!("s{i} deletes {} rows of moving slots it holds without serving", keys.len()));
        Ok(keys.len() as u64)
    }
}
