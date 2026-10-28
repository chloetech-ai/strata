//! A memory breaker for queries, as Elasticsearch's request circuit
//! breaker: the memory the queries of a process build up (groups,
//! candidate rows, the rows they return, a compound query's steps and
//! joins) is counted against one [`MemoryBudget`], and a query that would
//! go past it fails with [`Error::Memory`] instead of the process running
//! out of memory.
//!
//! The counts are estimates of what a query holds, made as its structures
//! grow (a vector's next capacity, a new group, a row's values), not of
//! every allocation. Index data a query reads is mapped from the file and
//! not counted.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::value::Value;

/// Bytes a query reserves from the budget beyond what it has counted, so
/// that most counts touch only the query's own counter.
const CHUNK: u64 = 1 << 20;

/// The memory the queries of a process may hold at once.
#[derive(Debug)]
pub struct MemoryBudget {
    limit: u64,
    used: AtomicU64,
    trips: AtomicU64,
}

impl MemoryBudget {
    pub fn new(limit: u64) -> Arc<MemoryBudget> {
        Arc::new(MemoryBudget { limit, used: AtomicU64::new(0), trips: AtomicU64::new(0) })
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Bytes the queries running now hold, reservations included.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Queries refused because the budget was spent.
    pub fn trips(&self) -> u64 {
        self.trips.load(Ordering::Relaxed)
    }

    fn reserve(&self, n: u64) -> bool {
        let mut cur = self.used.load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_add(n);
            if next > self.limit {
                return false;
            }
            match self.used.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(now) => cur = now,
            }
        }
    }

    fn release(&self, n: u64) {
        self.used.fetch_sub(n, Ordering::AcqRel);
    }
}

/// What one query holds of a [`MemoryBudget`]: counted as the query's
/// structures grow, and given back when the last clone is dropped. A
/// query without a budget counts nothing.
#[derive(Clone, Default)]
pub struct Charge(Option<Arc<Held>>);

struct Held {
    budget: Arc<MemoryBudget>,
    /// Bytes counted.
    counted: AtomicU64,
    /// Bytes reserved from the budget; raised under `grow`.
    reserved: AtomicU64,
    grow: Mutex<()>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.budget.release(*self.reserved.get_mut());
    }
}

impl Charge {
    /// A query's count against `budget`, or none.
    pub fn new(budget: Option<Arc<MemoryBudget>>) -> Charge {
        Charge(budget.map(|budget| Arc::new(Held { budget, counted: AtomicU64::new(0), reserved: AtomicU64::new(0), grow: Mutex::new(()) })))
    }

    /// Bytes this query has counted.
    pub fn counted(&self) -> u64 {
        self.0.as_ref().map_or(0, |h| h.counted.load(Ordering::Relaxed))
    }

    /// Counts `bytes` more, or fails with [`Error::Memory`] if the budget
    /// has not that much left.
    pub fn add(&self, bytes: u64) -> Result<()> {
        let Some(h) = &self.0 else { return Ok(()) };
        let counted = h.counted.fetch_add(bytes, Ordering::Relaxed).saturating_add(bytes);
        if counted <= h.reserved.load(Ordering::Acquire) {
            return Ok(());
        }
        let _grow = h.grow.lock().unwrap_or_else(|e| e.into_inner());
        let (counted, reserved) = (h.counted.load(Ordering::Relaxed), h.reserved.load(Ordering::Acquire));
        if counted <= reserved {
            return Ok(());
        }
        let want = counted - reserved;
        // A chunk ahead when there is room; just what is needed when not.
        for more in [want.saturating_add(CHUNK), want] {
            if h.budget.reserve(more) {
                h.reserved.store(reserved + more, Ordering::Release);
                return Ok(());
            }
        }
        h.budget.trips.fetch_add(1, Ordering::Relaxed);
        let mb = |b: u64| b.div_ceil(1 << 20);
        Err(Error::Memory(format!(
            "the query needs more than the {} MiB the server lets queries hold at once ({} MiB of them in use, {} MiB by this query); add conditions or a LIMIT, or try again",
            mb(h.budget.limit),
            mb(h.budget.used()),
            mb(counted)
        )))
    }

    /// Counts room for one more item in `v`: when it is full, its next
    /// capacity, which it reserves.
    pub fn room<T>(&self, v: &mut Vec<T>) -> Result<()> {
        if self.0.is_some() && v.len() == v.capacity() {
            let more = v.capacity().max(64);
            self.add((more * std::mem::size_of::<T>()) as u64)?;
            v.reserve_exact(more);
        }
        Ok(())
    }

    /// Counts a row of values.
    pub fn row(&self, row: &[Value]) -> Result<()> {
        if self.0.is_none() {
            return Ok(());
        }
        self.add(row_bytes(row))
    }
}

/// About the bytes a value holds, in place and on the heap.
pub fn value_bytes(v: &Value) -> u64 {
    let heap = match v {
        Value::Str(s) => s.len() as u64,
        Value::Vector(x) => 4 * x.len() as u64,
        Value::List(xs) => xs.iter().map(value_bytes).sum(),
        _ => 0,
    };
    std::mem::size_of::<Value>() as u64 + heap
}

/// About the bytes a row holds.
pub fn row_bytes(row: &[Value]) -> u64 {
    24 + row.iter().map(value_bytes).sum::<u64>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_share_the_budget_and_give_it_back() {
        let b = MemoryBudget::new(10 << 20);
        let none = Charge::new(None);
        none.add(u64::MAX).unwrap();
        let first = Charge::new(Some(b.clone()));
        first.add(6 << 20).unwrap();
        // Reserved a chunk ahead.
        assert_eq!(b.used(), 7 << 20);
        let second = Charge::new(Some(b.clone()));
        second.add(2 << 20).unwrap();
        assert_eq!(b.used(), 10 << 20);
        let e = second.add(2 << 20).unwrap_err();
        assert!(matches!(e, Error::Memory(_)), "{e}");
        assert!(e.to_string().contains("10 MiB"), "{e}");
        assert_eq!(b.trips(), 1);
        // A clone holds the query's count until the last one goes.
        let clone = first.clone();
        drop(first);
        assert_eq!(b.used(), 10 << 20);
        drop(clone);
        assert_eq!(b.used(), 3 << 20);
        second.add(5 << 20).unwrap();
        drop(second);
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn room_counts_each_growth() {
        let b = MemoryBudget::new(1 << 30);
        let c = Charge::new(Some(b));
        let mut v: Vec<u64> = Vec::new();
        for i in 0..10_000 {
            c.room(&mut v).unwrap();
            v.push(i);
        }
        assert_eq!(c.counted(), 8 * v.capacity() as u64);
        assert!(v.capacity() < 20_000);
    }
}
