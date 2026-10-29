//! Numbered transactions as a primary ships them to its replicas.
//!
//! A database that numbers its writes and keeps a ship log
//! ([`crate::storage::DbOptions::ship_log_bytes`]) keeps its most recent
//! transactions in memory as it logged them: the WAL's records of each, a
//! commit record carrying its term and sequence number last
//! ([`crate::storage::wal`]). [`crate::storage::Database::log_from`] reads
//! them in order, and a replica applies them with
//! [`crate::storage::Database::apply_shipped`] under the primary's term
//! and numbers, so that every copy holds the same rows with the same
//! numbers. Opening fills the log with the transactions it replays from
//! the WAL, those since the last flush; a replica further behind than the
//! log reaches needs a copy of the primary's segments first.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::storage::manifest::SeqNo;

/// One transaction as its database logged it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shipped {
    pub seq: SeqNo,
    /// Its WAL records, ending with its commit.
    pub bytes: Arc<[u8]>,
}

/// What [`crate::storage::Database::log_from`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogRead {
    /// The transactions from the number asked for on, in order (none when
    /// the database has not given that number yet).
    Txns(Vec<Shipped>),
    /// The log no longer holds that number: it holds them from `oldest`
    /// on (none at all when `oldest` is past the last number given).
    Gone { oldest: u64 },
}

/// What [`crate::storage::Database::apply_shipped`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Applied every transaction; the database's last number is now `at`.
    At(SeqNo),
    /// Nothing: the transaction before the first shipped is not this
    /// database's last. `seq` is its last number and `term` that number's
    /// term, from which the primary ships again, or finds that their
    /// histories differ.
    Mismatch { seq: u64, term: u64 },
}

/// The most recent transactions, up to a number of bytes.
#[derive(Debug, Default)]
pub(crate) struct ShipLog {
    txns: VecDeque<Shipped>,
    bytes: usize,
    max: usize,
}

impl ShipLog {
    pub fn new(max: usize) -> ShipLog {
        ShipLog { max, ..Default::default() }
    }

    pub fn keeps(&self) -> bool {
        self.max > 0
    }

    /// Adds the next transaction, dropping the oldest past `max` bytes (the
    /// newest stays, whatever its size). One that does not follow the last
    /// starts the log again, so that it holds a run without gaps.
    pub fn push(&mut self, t: Shipped) {
        if !self.keeps() {
            return;
        }
        if self.txns.back().is_some_and(|b| b.seq.seq + 1 != t.seq.seq) {
            self.txns.clear();
            self.bytes = 0;
        }
        self.bytes += t.bytes.len();
        self.txns.push_back(t);
        while self.bytes > self.max && self.txns.len() > 1 {
            let old = self.txns.pop_front().expect("not empty");
            self.bytes -= old.bytes.len();
        }
    }

    /// The transactions from number `from` on, up to `max_bytes` of them
    /// (at least one), given that `last` is the last number given.
    pub fn read(&self, from: u64, last: u64, max_bytes: usize) -> LogRead {
        if from > last {
            return LogRead::Txns(Vec::new());
        }
        // A log that stops short of the last number holds no run up to it.
        let oldest = match (self.txns.front(), self.txns.back()) {
            (Some(f), Some(b)) if b.seq.seq == last => f.seq.seq,
            _ => last + 1,
        };
        if from < oldest {
            return LogRead::Gone { oldest };
        }
        let mut out = Vec::new();
        let mut bytes = 0;
        for t in self.txns.iter().skip((from - oldest) as usize) {
            if !out.is_empty() && bytes + t.bytes.len() > max_bytes {
                break;
            }
            bytes += t.bytes.len();
            out.push(t.clone());
        }
        LogRead::Txns(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(seq: u64, len: usize) -> Shipped {
        Shipped { seq: SeqNo { term: 1, seq }, bytes: vec![0u8; len].into() }
    }

    fn seqs(r: LogRead) -> Vec<u64> {
        match r {
            LogRead::Txns(ts) => ts.iter().map(|t| t.seq.seq).collect(),
            LogRead::Gone { oldest } => panic!("gone, from {oldest}"),
        }
    }

    #[test]
    fn the_log_keeps_the_newest_run() {
        let mut log = ShipLog::new(100);
        for s in 1..=5 {
            log.push(t(s, 30));
        }
        // 100 bytes keep three of 30.
        assert_eq!(seqs(log.read(3, 5, usize::MAX)), [3, 4, 5]);
        assert_eq!(log.read(2, 5, usize::MAX), LogRead::Gone { oldest: 3 });
        assert_eq!(seqs(log.read(6, 5, usize::MAX)), Vec::<u64>::new());
        // A read takes up to its bytes, and at least one.
        assert_eq!(seqs(log.read(3, 5, 65)), [3, 4]);
        assert_eq!(seqs(log.read(3, 5, 1)), [3]);
        // The newest stays whatever its size.
        log.push(t(6, 500));
        assert_eq!(seqs(log.read(6, 6, usize::MAX)), [6]);
        assert_eq!(log.read(5, 6, usize::MAX), LogRead::Gone { oldest: 6 });
        // A gap starts the log again.
        log.push(t(9, 10));
        assert_eq!(seqs(log.read(9, 9, usize::MAX)), [9]);
        assert_eq!(log.read(7, 9, usize::MAX), LogRead::Gone { oldest: 9 });
        // A log behind the last number given holds no run up to it.
        assert_eq!(log.read(9, 10, usize::MAX), LogRead::Gone { oldest: 11 });
        // A database that keeps no log has none.
        let mut none = ShipLog::new(0);
        none.push(t(1, 10));
        assert_eq!(none.read(1, 1, usize::MAX), LogRead::Gone { oldest: 2 });
    }
}
