//! A coordinator in the simulator, and its clients: each client writes,
//! reads from the primary, or now and then asks a replica to be promoted
//! by hand, one operation at a time, as [`crate::coord::Coordinator`]
//! routes them: to the primary of the newest term that no other replaced,
//! as the copies last said (`Hello`), asking every copy again when it
//! knows none. A write a primary refuses as a replaced one would (409, or
//! no connection) goes, once, to the copy that took its place.

use std::time::Duration;

use crate::pb;

use super::{Config, Purpose, Req, Resp, Sim, Time, Timer};

#[derive(Clone, Debug, Default)]
struct Seen {
    role: i32,
    term: u64,
    replaced: bool,
}

pub(crate) struct Coord {
    seen: Vec<Seen>,
    clients: Vec<Option<Op>>,
    /// `Hello` to every copy under way: answers still to come, and the
    /// clients waiting for them.
    asking: Option<(usize, Vec<usize>)>,
}

#[derive(Clone, Debug)]
struct Op {
    kind: Kind,
    sent: Time,
    /// The copy called, and whether it was tried elsewhere already.
    to: Option<usize>,
    again: bool,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Write { id: u64 },
    Read,
    Promote,
}

impl Coord {
    pub fn new(cfg: &Config) -> Coord {
        Coord { seen: vec![Seen::default(); cfg.copies], clients: vec![None; cfg.clients], asking: None }
    }

    /// The primary of the newest term no other replaced, but `not`.
    fn primary(&self, not: Option<usize>) -> Option<usize> {
        (0..self.seen.len()).filter(|&k| Some(k) != not && self.seen[k].role == pb::Role::Primary as i32 && !self.seen[k].replaced).max_by_key(|&k| self.seen[k].term)
    }
}

impl Sim {
    fn node_of(&self, j: usize) -> usize {
        self.cfg.copies + j
    }

    pub(crate) fn client_tick(&mut self, j: usize, c: usize) {
        if self.quiet || self.coords[j].clients[c].is_some() {
            return;
        }
        let kind = match self.rng.below(1_000_000) {
            p if p < self.cfg.promote_ppm => Kind::Promote,
            p if p < 300_000 => Kind::Read,
            _ => Kind::Write { id: self.write_id() },
        };
        self.coords[j].clients[c] = Some(Op { kind, sent: self.now, to: None, again: false });
        self.dispatch(j, c);
    }

    /// Sends client `c`'s operation where it goes, asking every copy first
    /// when no primary is known.
    fn dispatch(&mut self, j: usize, c: usize) {
        let Some(op) = self.coords[j].clients[c].clone() else { return };
        let to = match op.kind {
            Kind::Promote => {
                let replicas: Vec<usize> = (0..self.cfg.copies).filter(|&k| self.coords[j].seen[k].role == pb::Role::Replica as i32).collect();
                match replicas.is_empty() {
                    true => None,
                    false => Some(*self.rng.pick(&replicas)),
                }
            }
            _ => self.coords[j].primary(if op.again { op.to } else { None }),
        };
        let Some(k) = to else {
            if op.to.is_none() && !op.again && self.coords[j].asking.as_ref().is_none_or(|a| !a.1.contains(&c)) {
                // None known: ask every copy, then once more.
                if let Some(op) = &mut self.coords[j].clients[c] {
                    op.again = true;
                }
                return self.ask_all(j, Some(c));
            }
            return self.done(j, c, false);
        };
        let req = match op.kind {
            Kind::Write { id } => Req::Write { id },
            Kind::Read => Req::Read { sent: op.sent },
            Kind::Promote => Req::Promote,
        };
        // A write's deadline runs on its primary; the call waits a little
        // longer for the answer.
        let timeout = match op.kind {
            Kind::Promote => self.cfg.call_timeout * 3,
            _ => self.cfg.write_timeout + Duration::from_secs(1),
        };
        if let Some(op) = &mut self.coords[j].clients[c] {
            op.to = Some(k);
        }
        let from = self.node_of(j);
        self.call(from, k, req, timeout, Purpose::Coord { client: Some(c), k });
    }

    /// Asks every copy `Hello`; `then`, a client waiting for the answers.
    fn ask_all(&mut self, j: usize, then: Option<usize>) {
        if let Some((_, waiting)) = &mut self.coords[j].asking {
            waiting.extend(then);
            return;
        }
        self.coords[j].asking = Some((self.cfg.copies, then.into_iter().collect()));
        let from = self.node_of(j);
        for k in 0..self.cfg.copies {
            self.call(from, k, Req::Hello, Duration::from_secs(2), Purpose::Coord { client: None, k });
        }
    }

    /// Client `c`'s operation ended; the next follows a moment later.
    fn done(&mut self, j: usize, c: usize, ok: bool) {
        let Some(op) = self.coords[j].clients[c].take() else { return };
        let s = &mut self.history.stats;
        match (op.kind, ok) {
            (Kind::Write { .. }, true) => s.writes_ok += 1,
            (Kind::Write { .. }, false) => s.writes_failed += 1,
            (Kind::Read, true) => s.reads_ok += 1,
            (Kind::Read, false) => s.reads_failed += 1,
            (Kind::Promote, _) => {}
        }
        let think = Duration::from_micros(self.rng.between(500, 30_000));
        let at = self.node_of(j);
        self.timer(at, think, Timer::Client(c));
    }

    pub(crate) fn coord_answer(&mut self, j: usize, client: Option<usize>, k: usize, a: Result<Resp, String>) {
        let Some(c) = client else {
            if let Ok(Resp::Hello(h)) = &a {
                self.coords[j].seen[k] = Seen { role: h.role, term: h.term, replaced: h.replaced_by_term > 0 };
            }
            let Some((left, waiting)) = &mut self.coords[j].asking else { return };
            *left -= 1;
            if *left > 0 {
                return;
            }
            let waiting = std::mem::take(waiting);
            self.coords[j].asking = None;
            for c in waiting {
                self.dispatch(j, c);
            }
            return;
        };
        let Some(op) = self.coords[j].clients[c].clone() else { return };
        match (op.kind, a) {
            (Kind::Write { .. }, Ok(Resp::Write(Ok(_)))) => self.done(j, c, true),
            (Kind::Read, Ok(Resp::Read(Ok(_)))) => self.done(j, c, true),
            (Kind::Promote, Ok(Resp::Promote(a))) => {
                if a.is_ok() {
                    self.history.stats.promoted += 1;
                }
                self.ask_all(j, None);
                self.done(j, c, true);
            }
            // Refused as a replaced primary would, or no connection: once
            // more, to the copy that took its place.
            (Kind::Write { .. }, a) if !op.again && moved(&a) => {
                if let Some(op) = &mut self.coords[j].clients[c] {
                    op.again = true;
                }
                self.ask_all(j, Some(c));
            }
            (_, a) => {
                if matches!(a, Err(_) | Ok(Resp::Write(Err(_))) | Ok(Resp::Read(Err(_)))) {
                    // The copies' parts may have changed.
                    self.ask_all(j, None);
                }
                self.done(j, c, false)
            }
        }
    }
}

/// Whether a write's answer is one a primary that is no longer the
/// shard's gives: 409 or 503, or no connection (`try_elsewhere`).
fn moved(a: &Result<Resp, String>) -> bool {
    match a {
        Ok(Resp::Write(Err(e))) => matches!(e.status, 409 | 503),
        Err(e) => e.contains("refused"),
        _ => false,
    }
}
