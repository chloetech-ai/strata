//! Idempotency keys of write requests. A client that sends a key with a
//! write and retries it after losing the answer (a timeout, a dropped
//! connection, a coordinator whose other replica failed) gets the first
//! attempt's answer instead of writing twice: an `add` applied once, an
//! `insert` that does not fail as a duplicate of itself.
//!
//! Keys are kept in memory for [`TTL`], the most recent [`CAPACITY`] of
//! them, so a restart forgets them. A key is recorded when its request
//! commits, by the writer thread, so a client that disconnects mid-write
//! still finds it.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value as Json;

use crate::service::Written;

/// How long a key is remembered.
pub const TTL: Duration = Duration::from_secs(3600);
/// How many keys are remembered at most.
pub const CAPACITY: usize = 100_000;

/// What a key says about a request.
#[derive(Debug, PartialEq, Eq)]
pub enum Begin {
    /// Unseen: apply the request.
    New,
    /// Already committed, with this answer.
    Done(Written),
    /// A request with this key is being applied now.
    Busy,
    /// The key was used for different operations.
    Mismatch,
}

#[derive(Default)]
pub struct Keys {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The hash of the key's operations, and what they wrote once
    /// committed (`None` while running).
    keys: HashMap<String, (u64, Option<Written>)>,
    /// Keys in the order they arrived, to forget the oldest.
    order: VecDeque<(Instant, String)>,
}

impl Keys {
    /// Looks `key` up for a request whose operations hash to `hash`,
    /// claiming it if it is new.
    pub fn begin(&self, key: &str, hash: u64) -> Begin {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        while g.order.front().is_some_and(|(t, _)| now.duration_since(*t) > TTL) || g.order.len() > CAPACITY {
            let (_, k) = g.order.pop_front().expect("not empty");
            g.keys.remove(&k);
        }
        match g.keys.get(key) {
            Some((h, _)) if *h != hash => Begin::Mismatch,
            Some((_, Some(n))) => Begin::Done(*n),
            Some((_, None)) => Begin::Busy,
            None => {
                g.keys.insert(key.to_string(), (hash, None));
                g.order.push_back((now, key.to_string()));
                Begin::New
            }
        }
    }

    /// Records the outcome of the request that claimed `key`: what it
    /// wrote, or `None` if it did not commit (the key is then forgotten,
    /// so a retry applies it).
    pub fn finish(&self, key: &str, written: Option<Written>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match written {
            Some(n) => {
                if let Some(e) = g.keys.get_mut(key) {
                    e.1 = Some(n);
                }
            }
            None => {
                if g.keys.get(key).is_some_and(|e| e.1.is_none()) {
                    g.keys.remove(key);
                }
            }
        }
    }
}

/// A hash of a request's operations, to tell a retry from a different
/// request that reuses its key.
pub fn hash_ops(ops: &[Json]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(ops).unwrap_or_default().hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_get_the_first_answer() {
        let k = Keys::default();
        assert_eq!(k.begin("a", 1), Begin::New);
        assert_eq!(k.begin("a", 1), Begin::Busy);
        assert_eq!(k.begin("a", 2), Begin::Mismatch);
        // The answer comes back whole, with its sequence number.
        let w = Written { rows: 3, seq_no: Some(7) };
        k.finish("a", Some(w));
        assert_eq!(k.begin("a", 1), Begin::Done(w));
        // A request that did not commit can be retried.
        assert_eq!(k.begin("b", 1), Begin::New);
        k.finish("b", None);
        assert_eq!(k.begin("b", 1), Begin::New);
        // A late failure report does not forget a committed key.
        k.finish("a", None);
        assert_eq!(k.begin("a", 1), Begin::Done(w));
    }
}
