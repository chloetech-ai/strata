//! What a run of the cluster simulator promises, checked as it goes and at
//! its end ([`super`]).

use std::collections::BTreeMap;

use super::server::{slot_of, Server};
use super::{server_of, Config, Time, Value};
use crate::slots::SlotMap;

/// What a run did.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub writes_ok: u64,
    pub writes_failed: u64,
    pub reads_ok: u64,
    pub reads_failed: u64,
    /// Operations that went again by a newer map.
    pub routed_again: u64,
    pub changes_made: u64,
    pub changes_failed: u64,
    pub moves_done: u64,
    pub moves_given_up: u64,
    pub moves_failed: u64,
    pub rows_moved: u64,
    /// Holds on writes that ran out unasked.
    pub holds_ran_out: u64,
    /// Epochs of which servers took two different maps.
    pub split_epochs: u64,
    /// Servers outside the cluster's map at the end that hold a newer one
    /// (a spare that took a change whose coordinator stopped before giving
    /// it to any other server): it must start on a new database to join.
    pub orphans: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Pending,
    Acked(Time),
    /// A server refused it: it was not written.
    Refused,
    /// No answer: it may have been written.
    Unknown,
}

#[derive(Clone, Debug)]
struct Write {
    value: Value,
    outcome: Outcome,
}

pub struct History {
    writes: BTreeMap<u64, Write>,
    /// Each key's writes in the order sent (one client writes a key).
    by_key: BTreeMap<i64, Vec<u64>>,
    /// The first map taken of each epoch.
    maps: BTreeMap<u64, SlotMap>,
    split: std::collections::BTreeSet<u64>,
    pub violations: Vec<String>,
    pub stats: Stats,
    keys: i64,
}

impl History {
    pub fn new(cfg: &Config, first: &SlotMap) -> History {
        History {
            writes: BTreeMap::new(),
            by_key: BTreeMap::new(),
            maps: BTreeMap::from([(first.epoch, first.clone())]),
            split: Default::default(),
            violations: Vec::new(),
            stats: Stats::default(),
            keys: cfg.keys,
        }
    }

    pub fn violation(&mut self, v: String) {
        if self.violations.len() < 50 {
            self.violations.push(v);
        }
    }

    pub fn sent(&mut self, op: u64, key: i64, value: Value, _at: Time) {
        self.writes.insert(op, Write { value, outcome: Outcome::Pending });
        self.by_key.entry(key).or_default().push(op);
    }

    fn set(&mut self, op: u64, o: Outcome) {
        if let Some(w) = self.writes.get_mut(&op) {
            w.outcome = o;
        }
    }

    pub fn acked(&mut self, op: u64, at: Time) {
        self.set(op, Outcome::Acked(at));
    }

    pub fn refused(&mut self, op: u64) {
        self.set(op, Outcome::Refused);
    }

    pub fn unknown(&mut self, op: u64) {
        self.set(op, Outcome::Unknown);
    }

    /// Server `i` took `map`.
    pub fn took(&mut self, i: usize, map: &SlotMap, _at: Time) {
        match self.maps.get(&map.epoch) {
            None => {
                self.maps.insert(map.epoch, map.clone());
            }
            Some(m) if m != map && self.split.insert(map.epoch) => {
                let _ = i;
                self.stats.split_epochs += 1;
            }
            Some(_) => {}
        }
    }

    /// What is wrong with `got` as the value of `key` seen by a read sent
    /// at `sent`, if anything: it must not be older than a write
    /// acknowledged before then, nor one never written or refused.
    fn wrong(&self, key: i64, got: Value, sent: Time) -> Option<String> {
        let ops = self.by_key.get(&key).map(Vec::as_slice).unwrap_or_default();
        let w = |i: usize| &self.writes[&ops[i]];
        let last = (0..ops.len()).rev().find(|&i| matches!(w(i).outcome, Outcome::Acked(t) if t < sent));
        let deleted_since = |l: usize| w(l).value.is_none() || (l + 1..ops.len()).any(|i| w(i).value.is_none() && w(i).outcome != Outcome::Refused) || (0..l).any(|i| w(i).value.is_none() && matches!(w(i).outcome, Outcome::Unknown | Outcome::Pending));
        match got {
            Some(v) => {
                let Some(x) = ops.iter().position(|&o| o == v) else { return Some(format!("holds write #{v}, which is not one of key {key}'s")) };
                if w(x).outcome == Outcome::Refused {
                    return Some(format!("holds write #{v}, which a server refused"));
                }
                match last {
                    Some(l) if x < l && matches!(w(x).outcome, Outcome::Acked(_)) => {
                        let Outcome::Acked(t) = w(l).outcome else { unreachable!() };
                        Some(format!("holds write #{v}, older than write #{} acknowledged at {:.3} ms", ops[l], t as f64 / 1000.0))
                    }
                    _ => None,
                }
            }
            None => match last {
                Some(l) if !deleted_since(l) => {
                    let Outcome::Acked(t) = w(l).outcome else { unreachable!() };
                    Some(format!("misses write #{} acknowledged at {:.3} ms", ops[l], t as f64 / 1000.0))
                }
                _ => None,
            },
        }
    }

    /// A read of `key`, sent at `sent`, saw `got` at `at`.
    pub fn read(&mut self, key: i64, got: Value, sent: Time, at: Time) {
        if let Some(why) = self.wrong(key, got, sent) {
            self.violation(format!("a stale read of key {key} sent at {:.3} ms, answered at {:.3} ms: it {why}", sent as f64 / 1000.0, at as f64 / 1000.0));
        }
    }

    /// The end of a run: the map the first coordinator routes by, `newest`,
    /// held by every server of it, no move under way, and each key once,
    /// where the map puts it, holding what its writes left.
    pub(crate) fn settled(&mut self, servers: &[Server], newest: &SlotMap, now: Time) {
        let newest = newest.clone();
        let members: Vec<(usize, usize)> = newest.replicas().iter().enumerate().flat_map(|(i, rs)| rs.iter().filter_map(move |u| server_of(u).map(|s| (s, i)))).collect();
        for &(s, i) in &members {
            match &servers[s].map {
                Some((m, k)) if *m == newest && *k == i => {}
                other => self.violation(format!("the cluster did not settle: s{s} holds map epoch {}, the newest is {}", other.as_ref().map_or(0, |(m, _)| m.epoch), newest.epoch)),
            }
        }
        self.stats.orphans = (0..servers.len()).filter(|&s| !members.iter().any(|&(k, _)| k == s) && servers[s].map.as_ref().is_some_and(|(m, _)| m.epoch > newest.epoch)).count() as u64;
        if let Some(m) = newest.moving() {
            self.violation(format!("the move of {} slots from shard {} to {} did not end ({})", m.slots.len(), m.from, m.to, m.state.name()));
        }
        for &(s, i) in &members {
            let outside: Vec<i64> = servers[s].rows.iter().filter(|(&k, r)| r.value.is_some() && newest.owner(slot_of(k)) != i && servers[s].residue().binary_search(&slot_of(k)).is_err()).map(|(&k, _)| k).collect();
            if !outside.is_empty() {
                self.violation(format!("s{s} (shard {i}) holds {} rows outside its slots, keys {outside:?}", outside.len()));
            }
        }
        for key in 0..self.keys {
            let seen: Vec<(usize, u64)> = members.iter().filter_map(|&(s, _)| servers[s].visible(key).flatten().map(|v| (s, v))).collect();
            if seen.len() > 1 {
                self.violation(format!("key {key} is on {} shards at the end: {seen:?}", seen.len()));
            }
            let owner = newest.owner(slot_of(key));
            let on_owner = members.iter().find(|&&(_, i)| i == owner).and_then(|&(s, _)| servers[s].visible(key).flatten());
            if let Some(why) = self.wrong(key, on_owner, now + 1) {
                self.violation(format!("key {key} at the end, on shard {owner}: it {why}"));
            }
        }
    }
}
