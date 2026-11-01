//! What a run promises, checked as it goes and at its end ([`super`]).

use std::collections::BTreeMap;

use strata::storage::SeqNo;

use super::copy::{Copy, Role};
use super::db::{ModelDb, Txn};
use super::Time;

/// A write a primary answered as acknowledged.
#[derive(Clone, Copy, Debug)]
pub struct Acked {
    pub write: u64,
    pub seq: SeqNo,
    pub by: usize,
    pub at: Time,
}

/// What a run did.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub writes_ok: u64,
    pub writes_failed: u64,
    pub reads_ok: u64,
    pub reads_failed: u64,
    /// Terms begun past the first, by elections.
    pub elections: u64,
    /// Promotions by hand asked for, and those that won.
    pub promotions: u64,
    pub promoted: u64,
    /// Copies of a primary installed.
    pub copies: u64,
    pub acknowledged: u64,
}

pub struct History {
    /// In the order they were answered.
    pub acked: Vec<Acked>,
    /// The copy that began each term.
    pub terms: BTreeMap<u64, usize>,
    pub violations: Vec<String>,
    pub stats: Stats,
    /// For each copy, how many of `acked` it was found to hold, in order:
    /// its history only grows until it installs a copy or loses what it
    /// did not sync.
    held: Vec<usize>,
    /// Writes already reported lost.
    lost: std::collections::BTreeSet<u64>,
}

fn holds(db: &ModelDb, a: &Acked) -> bool {
    db.txns.get(a.seq.seq as usize - 1) == Some(&Txn { seq: a.seq, write: a.write })
}

impl History {
    pub fn new(copies: usize) -> History {
        History { acked: Vec::new(), terms: BTreeMap::from([(1, 0)]), violations: Vec::new(), stats: Stats::default(), held: vec![0; copies], lost: Default::default() }
    }

    fn violation(&mut self, v: String) {
        if self.violations.len() < 50 {
            self.violations.push(v);
        }
    }

    pub fn acknowledged(&mut self, write: u64, seq: SeqNo, by: usize, at: Time) {
        self.stats.acknowledged += 1;
        self.acked.push(Acked { write, seq, by, at });
    }

    /// Copy `i`'s history changed other than by growing.
    pub fn lost_memory(&mut self, i: usize) {
        self.held[i] = 0;
    }

    /// Whether copy `i` (its database `db`) holds every write acknowledged
    /// before `before` as a transaction of a term older than `older_than`,
    /// up to the first that is not; reports those it lacks as `what` does.
    fn holds_all(&mut self, i: usize, db: &ModelDb, before: Time, older_than: u64, what: &dyn Fn(&Acked) -> String) {
        while let Some(a) = self.acked.get(self.held[i]).copied() {
            if a.at >= before || a.seq.term >= older_than {
                break;
            }
            self.held[i] += 1;
            if !holds(db, &a) && self.lost.insert(a.write) {
                let v = what(&a);
                self.violation(v);
            }
        }
    }

    /// Copy `i` began term `term`, holding `db`, at `now`: no other copy
    /// began it, and it holds every write acknowledged so far in an older
    /// term. A copy can win a term after a newer one acknowledged writes,
    /// when the votes it asked for come late: it serves none of them, as
    /// its first call to a replica that knows the newer term ends it.
    pub fn began(&mut self, i: usize, term: u64, db: &ModelDb, now: Time) {
        self.stats.elections += 1;
        if let Some(&j) = self.terms.get(&term) {
            if j != i {
                self.violation(format!("two primaries in term {term}: s{j} and s{i} both began it"));
            }
        }
        self.terms.insert(term, i);
        self.holds_all(i, db, now, term, &|a| {
            format!(
                "write {} acknowledged by s{} as transaction {} of term {} at {} ms is not held by s{i}, elected the primary of term {term} at {} ms",
                a.write,
                a.by,
                a.seq.seq,
                a.seq.term,
                a.at / 1000,
                now / 1000
            )
        });
    }

    /// Copy `i`, a primary, served a read sent at `sent`, holding `db`.
    pub fn read_on_primary(&mut self, i: usize, sent: Time, db: &ModelDb, now: Time) {
        self.holds_all(i, db, sent, u64::MAX, &|a| {
            format!(
                "s{i} served a read at {} ms as a primary without write {} (transaction {} of term {}), acknowledged by s{} at {} ms, before the read was sent at {} ms: a stale read",
                now / 1000,
                a.write,
                a.seq.seq,
                a.seq.term,
                a.by,
                a.at / 1000,
                sent / 1000
            )
        });
    }

    /// At the end, after the faults: two copies holding the same number of
    /// the same term hold the same history up to it; one primary serves,
    /// every copy holds what it holds, and that holds every acknowledged
    /// write.
    pub(crate) fn settled(&mut self, copies: &[Copy], now: Time) {
        for a in 0..copies.len() {
            for b in a + 1..copies.len() {
                let (x, y) = (&copies[a].db.txns, &copies[b].db.txns);
                let Some(d) = x.iter().zip(y.iter()).position(|(p, q)| p != q) else { continue };
                if let Some(s) = (d..x.len().min(y.len())).find(|&s| x[s].seq.term == y[s].seq.term) {
                    self.violation(format!(
                        "s{a} and s{b} both hold transaction {} of term {}, but their histories differ from transaction {} (s{a}: write {} of term {}, s{b}: write {} of term {})",
                        s + 1,
                        x[s].seq.term,
                        d + 1,
                        x[d].write,
                        x[d].seq.term,
                        y[d].write,
                        y[d].seq.term
                    ));
                }
            }
        }
        let primaries: Vec<usize> = (0..copies.len()).filter(|&i| copies[i].primary().is_some_and(|p| p.replaced_by() == 0)).collect();
        let [p] = primaries[..] else {
            let roles: Vec<String> = copies.iter().map(describe).collect();
            return self.violation(format!("the shard did not settle: {} primaries serve after the faults ({})", primaries.len(), roles.join(", ")));
        };
        let db = &copies[p].db;
        for a in self.acked.clone() {
            if !holds(db, &a) && self.lost.insert(a.write) {
                self.violation(format!(
                    "write {} acknowledged by s{} as transaction {} of term {} at {} ms is not held by s{p}, the primary at the end",
                    a.write,
                    a.by,
                    a.seq.seq,
                    a.seq.term,
                    a.at / 1000
                ));
            }
        }
        for (i, c) in copies.iter().enumerate() {
            if i != p && c.db.txns != db.txns {
                self.violation(format!(
                    "the shard did not settle: s{i} holds transactions up to {} of term {}, and s{p}, its primary, up to {} of term {}, {} ms after the faults ({})",
                    c.db.numbering.seq,
                    c.db.last().term,
                    db.numbering.seq,
                    db.last().term,
                    now / 1000,
                    describe(c)
                ));
            }
        }
    }
}

fn describe(c: &Copy) -> String {
    let role = match &c.role {
        Role::Down => "down".to_string(),
        Role::Replica(r) => format!("replica following {:?}", r.kept.follows),
        Role::Primary(p) => format!("primary, replaced by term {}, peers {:?}", p.core.replaced_by(), p.core.peers.iter().map(|p| (p.acked, p.in_sync, p.error.clone())).collect::<Vec<_>>()),
    };
    format!("{} {role} at {} of term {}", c.url, c.db.numbering.seq, c.db.numbering.term)
}
