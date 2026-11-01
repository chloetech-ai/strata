//! A model of a copy's database, as replication sees it: its numbered
//! transactions and their terms ([`Numbering`]), the ship log it keeps of
//! the latest, and what a crash keeps.
//!
//! The model answers as [`strata::storage::Database`] does for the calls
//! replication makes: [`ModelDb::apply_shipped`] applies a batch only
//! after the transaction before it, checks that each follows the one
//! before and is of no older term, and raises the term to a newer one's;
//! [`ModelDb::begin_term`], [`ModelDb::log_from`], holds on the log,
//! checkpoints and [`ModelDb::install_copy`] likewise. A transaction is a
//! write's id, which the checks follow ([`super::check`]).
//!
//! With `sync` (Strata's default, `DbOptions::sync`) a commit is durable
//! when it returns. Without, only what a sync made durable survives a
//! crash, as `storage::io` models a power loss: the transactions since are
//! lost.

use std::sync::Arc;

use strata::storage::{Applied, LogRead, Numbering, SeqNo, Shipped};

use crate::ApiError;

/// One numbered transaction: its place in the history and the write it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Txn {
    pub seq: SeqNo,
    pub write: u64,
}

impl Txn {
    /// As it is shipped: its term, number and write, little-endian.
    pub fn bytes(&self) -> Arc<[u8]> {
        let mut b = Vec::with_capacity(24);
        b.extend_from_slice(&self.seq.term.to_le_bytes());
        b.extend_from_slice(&self.seq.seq.to_le_bytes());
        b.extend_from_slice(&self.write.to_le_bytes());
        b.into()
    }

    pub fn of(b: &[u8]) -> Result<Txn, String> {
        let word = |i: usize| b.get(i * 8..i * 8 + 8).map(|w| u64::from_le_bytes(w.try_into().expect("8 bytes")));
        match (b.len(), word(0), word(1), word(2)) {
            (24, Some(term), Some(seq), Some(write)) => Ok(Txn { seq: SeqNo { term, seq }, write }),
            _ => Err(format!("a shipped transaction of {} bytes, not 24", b.len())),
        }
    }
}

/// A copy of a database ([`ModelDb::checkpoint`]).
#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub numbering: Numbering,
    pub txns: Vec<Txn>,
}

#[derive(Clone, Debug)]
pub struct ModelDb {
    pub numbering: Numbering,
    /// Every transaction, the one numbered `s` at `s - 1`.
    pub txns: Vec<Txn>,
    /// What a crash keeps when commits are not synced.
    durable: Checkpoint,
    pub sync: bool,
    /// The ship log: transactions from `ship_from` to the last, at most
    /// `ship_max` of them but those held (up to [`strata::storage::ship::HELD`]
    /// times as many).
    ship_from: u64,
    pub ship_max: usize,
    holds: Vec<(u64, u64)>,
    next_hold: u64,
    /// [`super::Bug::RefusesOlderTerms`].
    pub refuses_older_terms: bool,
}

impl ModelDb {
    /// With [`super::Bug::RefusesOlderTerms`] when `on`.
    pub fn refusing_older_terms(self, on: bool) -> ModelDb {
        ModelDb { refuses_older_terms: on, ..self }
    }

    /// A new database that numbers its writes from now on (term 1).
    pub fn new(sync: bool, ship_max: usize) -> ModelDb {
        let numbering = Numbering::start();
        ModelDb { durable: Checkpoint { numbering: numbering.clone(), txns: Vec::new() }, numbering, txns: Vec::new(), sync, ship_from: 1, ship_max: ship_max.max(1), holds: Vec::new(), next_hold: 0, refuses_older_terms: false }
    }

    pub fn last(&self) -> SeqNo {
        self.numbering.last()
    }

    fn durable_now(&mut self) {
        if self.sync {
            self.durable = Checkpoint { numbering: self.numbering.clone(), txns: self.txns.clone() };
        }
    }

    /// What a sync makes durable: everything committed.
    pub fn sync_now(&mut self) {
        self.durable = Checkpoint { numbering: self.numbering.clone(), txns: self.txns.clone() };
    }

    /// The database as a crash leaves it and opening finds it: what was
    /// durable, and a ship log of the transactions since the last flush,
    /// `replayed` of them at most.
    pub fn crash(&mut self, replayed: u64) {
        self.numbering = self.durable.numbering.clone();
        self.txns = self.durable.txns.clone();
        self.holds.clear();
        let seq = self.numbering.seq;
        self.ship_from = seq + 1 - replayed.min(seq).min(self.ship_max as u64);
    }

    fn trim(&mut self) {
        let last = self.numbering.seq;
        let floor = self.holds.iter().map(|h| h.1).min();
        let held = strata::storage::ship::HELD as u64 * self.ship_max as u64;
        while last + 1 - self.ship_from > self.ship_max as u64 && last >= self.ship_from + 1 {
            if floor.is_some_and(|f| self.ship_from >= f) && last + 1 - self.ship_from <= held {
                break;
            }
            self.ship_from += 1;
        }
    }

    /// A client's write committed on a primary, numbered next.
    pub fn commit(&mut self, write: u64) -> SeqNo {
        let seq = SeqNo { term: self.numbering.term, seq: self.numbering.seq + 1 };
        self.txns.push(Txn { seq, write });
        self.numbering.seq = seq.seq;
        self.trim();
        self.durable_now();
        seq
    }

    /// [`strata::storage::Database::begin_term`]; durable on return.
    pub fn begin_term(&mut self, term: u64) -> Result<(), ApiError> {
        if term <= self.numbering.term {
            return Err(ApiError::bad(format!("term {term} is not after the database's term, {}", self.numbering.term)));
        }
        self.numbering.term = term;
        self.numbering.terms.push((term, self.numbering.seq + 1));
        self.sync_now();
        Ok(())
    }

    /// [`strata::storage::Database::log_from`], `max` transactions at most.
    pub fn log_from(&self, from: u64, max: usize) -> LogRead {
        let last = self.numbering.seq;
        if from > last {
            return LogRead::Txns(Vec::new());
        }
        if from < self.ship_from {
            return LogRead::Gone { oldest: self.ship_from };
        }
        LogRead::Txns(self.txns[from as usize - 1..].iter().take(max.max(1)).map(|t| Shipped { seq: t.seq, bytes: t.bytes() }).collect())
    }

    pub fn hold_log(&mut self, from: u64) -> u64 {
        self.next_hold += 1;
        self.holds.push((self.next_hold, from));
        self.next_hold
    }

    pub fn release_log(&mut self, id: u64) {
        self.holds.retain(|h| h.0 != id);
        self.trim();
    }

    /// [`strata::storage::Database::apply_shipped`]: the transactions that
    /// follow `prev`, its last, their terms never falling. Terms this
    /// database began, or took in a copy, after its last transaction hold
    /// none, and are dropped for those the primary it follows ships: that
    /// primary, of a newer term, holds transactions of an older one past
    /// this database's last (with [`super::Bug::RefusesOlderTerms`],
    /// refused as of a replaced primary).
    pub fn apply_shipped(&mut self, prev: SeqNo, txns: &[Vec<u8>]) -> Result<Applied, ApiError> {
        let now = self.numbering.last();
        if now != prev {
            return Ok(Applied::Mismatch { seq: now.seq, term: now.term });
        }
        if txns.is_empty() {
            return Ok(Applied::At(prev));
        }
        let mut shipped = Vec::with_capacity(txns.len());
        let mut at = now;
        for b in txns {
            let t = Txn::of(b).map_err(ApiError::internal)?;
            if t.seq.seq != at.seq + 1 {
                return Err(ApiError::internal(format!("shipped transaction {} does not follow {}", t.seq.seq, at.seq)));
            }
            let floor = if self.refuses_older_terms { at.term.max(self.numbering.term) } else { at.term };
            if t.seq.term < floor {
                return Err(ApiError::bad(format!(
                    "shipped transaction {} has term {}, older than term {floor}: it comes from a primary that another has replaced",
                    t.seq.seq, t.seq.term
                )));
            }
            at = t.seq;
            shipped.push(t);
        }
        for t in shipped {
            if t.seq.term < self.numbering.term {
                let last = self.numbering.seq;
                self.numbering.terms.retain(|&(_, first)| first <= last);
                self.numbering.term = self.numbering.terms.last().map_or(0, |t| t.0);
            }
            if t.seq.term > self.numbering.term {
                self.numbering.term = t.seq.term;
                self.numbering.terms.push((t.seq.term, self.numbering.seq + 1));
            }
            self.txns.push(t);
            self.numbering.seq = t.seq.seq;
        }
        self.trim();
        self.durable_now();
        Ok(Applied::At(at))
    }

    /// [`strata::storage::Database::checkpoint`].
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint { numbering: self.numbering.clone(), txns: self.txns.clone() }
    }

    /// [`strata::storage::Database::install_copy`]: the copy's rows,
    /// numbers and terms in place of this database's, the ship log emptied;
    /// durable on return. A copy of an older term is refused.
    pub fn install_copy(&mut self, copy: &Checkpoint) -> Result<SeqNo, ApiError> {
        if copy.numbering.term < self.numbering.term {
            return Err(ApiError::bad(format!("the copy is of term {}, older than this database's ({}): it is a copy of a replaced primary", copy.numbering.term, self.numbering.term)));
        }
        self.numbering = copy.numbering.clone();
        self.txns = copy.txns.clone();
        self.ship_from = self.numbering.seq + 1;
        self.holds.clear();
        self.sync_now();
        Ok(self.numbering.last())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_numbers_ships_and_copies_as_a_database_does() {
        let mut p = ModelDb::new(true, 2);
        for w in 1..=3 {
            p.commit(w);
        }
        assert!(matches!(p.log_from(1, 10), LogRead::Gone { oldest: 2 }));
        let LogRead::Txns(t) = p.log_from(2, 10) else { panic!() };
        assert_eq!(t.len(), 2);
        let mut r = ModelDb::new(true, 2);
        let bytes: Vec<Vec<u8>> = t.iter().map(|t| t.bytes.to_vec()).collect();
        assert_eq!(r.apply_shipped(SeqNo { term: 1, seq: 1 }, &bytes).unwrap(), Applied::Mismatch { seq: 0, term: 0 });
        assert_eq!(r.install_copy(&p.checkpoint()).unwrap(), SeqNo { term: 1, seq: 3 });
        p.begin_term(2).unwrap();
        p.commit(4);
        let LogRead::Txns(t) = p.log_from(4, 10) else { panic!() };
        let bytes: Vec<Vec<u8>> = t.iter().map(|t| t.bytes.to_vec()).collect();
        assert_eq!(r.apply_shipped(SeqNo { term: 1, seq: 3 }, &bytes).unwrap(), Applied::At(SeqNo { term: 2, seq: 4 }));
        assert_eq!(r.numbering, p.numbering);
        // A database that began a term and wrote nothing in it takes what a
        // newer primary holds of an older one.
        let mut q = r.clone();
        q.begin_term(3).unwrap();
        p.commit(5);
        p.begin_term(4).unwrap();
        let LogRead::Txns(t) = p.log_from(5, 10) else { panic!() };
        let bytes: Vec<Vec<u8>> = t.iter().map(|t| t.bytes.to_vec()).collect();
        let mut strict = q.clone();
        strict.refuses_older_terms = true;
        assert!(strict.apply_shipped(SeqNo { term: 2, seq: 4 }, &bytes).is_err());
        assert_eq!(q.apply_shipped(SeqNo { term: 2, seq: 4 }, &bytes).unwrap(), Applied::At(SeqNo { term: 2, seq: 5 }));
        assert_eq!(q.txns, p.txns);
        assert_eq!(q.numbering.terms, p.numbering.terms[..2]);
        // Unsynced commits are lost to a crash.
        let mut u = ModelDb::new(false, 8);
        u.commit(1);
        u.sync_now();
        u.commit(2);
        u.crash(8);
        assert_eq!(u.numbering.seq, 1);
    }
}
