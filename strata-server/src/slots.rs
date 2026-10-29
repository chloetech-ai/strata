//! Slots: where a key lives in a sharded collection.
//!
//! A primary key hashes to one of [`SLOTS`] slots ([`slot`]: xxh3 over a
//! typed encoding, so that the INT 5 and the text '5' differ), and a
//! [`SlotMap`] assigns the slots to shards under an epoch, its version. A
//! coordinator routes writes by its map and sends its epoch with every
//! request; a shard keeps the map it was given (in a file beside its
//! database), refuses a request of an older epoch, and refuses to write a
//! key whose slot another shard holds ([`crate::service::Service::set_slots`]).
//! Moving slots between shards (online resharding) is a new map with a
//! higher epoch.
//!
//! Before slots, a key's shard was FNV-1a of its text modulo the shard
//! count ([`hash_shard_of`]), which a coordinator still routes by while a
//! shard is from before protocol 4.

use serde_json::{json, Value as Json};
use strata::value::Value;

/// Where a shard keeps the slot map a coordinator gave it: beside its
/// database, `<db>.slots`.
pub fn file_of(db: &std::path::Path) -> std::path::PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(".slots");
    p.into()
}

/// The slots keys hash to.
pub const SLOTS: usize = 4096;

/// The slot of a primary key. Never change it: every sharded collection's
/// rows are placed by it.
pub fn slot(key: &Value) -> usize {
    // Most keys fit on the stack; a longer one is copied to the heap.
    let (mut short, mut long, mut n) = ([0u8; 64], Vec::new(), 0);
    encode(key, |b| {
        if long.is_empty() && n + b.len() <= short.len() {
            short[n..n + b.len()].copy_from_slice(b);
        } else {
            if long.is_empty() {
                long.extend_from_slice(&short[..n]);
            }
            long.extend_from_slice(b);
        }
        n += b.len();
    });
    let bytes = if long.is_empty() { &short[..n] } else { &long[..] };
    (twox_hash::XxHash3_64::oneshot(bytes) % SLOTS as u64) as usize
}

/// A key as [`slot`] hashes it, written to `out`: a tag for its kind,
/// then its bytes. Integers (and BOOL and TIMESTAMP keys, stored as
/// integers) are tag 1 and 8 bytes little-endian, text tag 2 and its
/// UTF-8, a float tag 3 and its bits (0.0 for -0.0), and NULL tag 0 alone.
fn encode(key: &Value, mut out: impl FnMut(&[u8])) {
    match key {
        Value::Null => out(&[0]),
        Value::Int(x) | Value::Timestamp(x) => {
            out(&[1]);
            out(&x.to_le_bytes());
        }
        Value::Bool(b) => {
            out(&[1]);
            out(&(*b as i64).to_le_bytes());
        }
        Value::Str(s) => {
            out(&[2]);
            out(s.as_bytes());
        }
        Value::Float(x) => {
            out(&[3]);
            out(&(if *x == 0.0 { 0.0f64 } else { *x }).to_bits().to_le_bytes());
        }
        // Not a key's type; placed somewhere all the same.
        Value::Vector(_) | Value::List(_) => {
            out(&[4]);
            out(format!("{key:?}").as_bytes());
        }
    }
}

/// The shard of `key` among `n` under [`SlotMap::even`]: what a coordinator
/// with no map of its own routes by, and what `--demo-shard` loads.
pub fn shard_of(key: &Value, n: usize) -> usize {
    even_owner(slot(key), n)
}

fn even_owner(slot: usize, n: usize) -> usize {
    slot * n / SLOTS
}

/// The shard of `key` among `n` before slots: FNV-1a of its text (an
/// integer key as decimal digits), modulo `n`. A coordinator routes by it
/// while a shard is from before protocol 4.
pub fn hash_shard_of(key: &Value, n: usize) -> usize {
    let text = match key {
        Value::Str(s) => s.clone(),
        Value::Int(x) => x.to_string(),
        v => format!("{v:?}"),
    };
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h % n.max(1) as u64) as usize
}

/// Which shard holds each slot, under an epoch that a new assignment
/// raises. In JSON, each shard's slots as ranges, `[start, end)`:
/// `{"epoch": 1, "shards": [[[0, 2048]], [[2048, 4096]]]}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotMap {
    pub epoch: u64,
    owner: Vec<u16>,
    shards: usize,
}

impl SlotMap {
    /// `n` shards holding equal runs of slots, shard 0 the first.
    pub fn even(n: usize, epoch: u64) -> SlotMap {
        assert!((1..=SLOTS).contains(&n), "1 to {SLOTS} shards");
        SlotMap { epoch, owner: (0..SLOTS).map(|s| even_owner(s, n) as u16).collect(), shards: n }
    }

    pub fn shards(&self) -> usize {
        self.shards
    }

    /// The shard holding `slot`.
    pub fn owner(&self, slot: usize) -> usize {
        self.owner[slot] as usize
    }

    /// The shard holding `key`.
    pub fn shard_of(&self, key: &Value) -> usize {
        self.owner(slot(key))
    }

    /// Whether `shard` holds the same slots here as in `other`.
    pub fn same_slots(&self, other: &SlotMap, shard: usize) -> bool {
        self.owner.iter().zip(&other.owner).all(|(a, b)| (*a as usize == shard) == (*b as usize == shard))
    }

    /// Shard `shard`'s slots, as runs `[start, end)`.
    pub fn ranges(&self, shard: usize) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for s in (0..SLOTS).filter(|&s| self.owner(s) == shard) {
            match out.last_mut() {
                Some(r) if r.1 == s => r.1 = s + 1,
                _ => out.push((s, s + 1)),
            }
        }
        out
    }

    pub fn to_json(&self) -> Json {
        json!({"epoch": self.epoch, "shards": (0..self.shards).map(|i| self.ranges(i)).collect::<Vec<_>>()})
    }

    /// A map from JSON, which must give every slot to exactly one shard,
    /// and every shard a slot.
    pub fn from_json(j: &Json) -> Result<SlotMap, String> {
        let epoch = j.get("epoch").and_then(Json::as_u64).filter(|&e| e > 0).ok_or("a slot map needs an epoch, a positive integer")?;
        let shards = j.get("shards").and_then(Json::as_array).ok_or("a slot map needs \"shards\": each shard's slots, as [start, end) ranges")?;
        if shards.is_empty() || shards.len() > SLOTS {
            return Err(format!("a slot map has 1 to {SLOTS} shards, not {}", shards.len()));
        }
        let mut owner = vec![u16::MAX; SLOTS];
        for (i, ranges) in shards.iter().enumerate() {
            let ranges = ranges.as_array().ok_or_else(|| format!("shard {i}: its slots must be a list of [start, end) ranges"))?;
            if ranges.is_empty() {
                return Err(format!("shard {i} holds no slot"));
            }
            for r in ranges {
                let (a, b) = match r.as_array().map(|r| r.iter().map(Json::as_u64).collect::<Vec<_>>()).as_deref() {
                    Some([Some(a), Some(b)]) => (*a as usize, *b as usize),
                    _ => return Err(format!("shard {i}: {r} is not a [start, end) range")),
                };
                if a >= b || b > SLOTS {
                    return Err(format!("shard {i}: [{a}, {b}) is not a range of slots 0 to {SLOTS}"));
                }
                for s in a..b {
                    if owner[s] != u16::MAX {
                        return Err(format!("slot {s} is given to shards {} and {i}", owner[s]));
                    }
                    owner[s] = i as u16;
                }
            }
        }
        if let Some(s) = owner.iter().position(|&o| o == u16::MAX) {
            return Err(format!("slot {s} is given to no shard"));
        }
        Ok(SlotMap { epoch, owner, shards: shards.len() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_never_change() {
        // Rows of every sharded collection are placed by these: a change
        // to the hash or the encoding moves them. The same slots come from
        // the reference xxh3 (Python's xxhash 3, xxh3_64_intdigest of the
        // encoded bytes, modulo 4096).
        let got: Vec<usize> = [Value::Int(5), Value::Str("5".into()), Value::Str("p0000042".into()), Value::Int(-1), Value::Str(String::new()), Value::Float(2.5)].iter().map(slot).collect();
        assert_eq!(got, vec![2555, 1819, 3725, 1676, 4095, 1346]);
        assert_ne!(slot(&Value::Int(5)), slot(&Value::Str("5".into())));
        // Keys longer than the stack buffer hash the same way.
        assert_eq!(slot(&Value::Str("k".repeat(63))), 3130);
        assert_eq!(slot(&Value::Str("k".repeat(200))), 3217);
        // BOOL and TIMESTAMP keys hash as the integers they are stored as.
        assert_eq!(slot(&Value::Timestamp(5)), slot(&Value::Int(5)));
        assert_eq!(slot(&Value::Bool(true)), slot(&Value::Int(1)));
        assert_eq!(slot(&Value::Float(-0.0)), slot(&Value::Float(0.0)));
        // Spread: 100,000 keys over 4,096 slots, none far from the mean.
        let mut n = vec![0usize; SLOTS];
        for i in 0..100_000 {
            n[slot(&Value::Str(format!("p{i:07}")))] += 1;
        }
        assert!(n.iter().all(|&c| (5..=50).contains(&c)), "{:?}", (n.iter().min(), n.iter().max()));
    }

    #[test]
    fn maps_round_trip_and_refuse_gaps() {
        for n in [1, 2, 3, 7, 4096] {
            let m = SlotMap::even(n, 3);
            assert_eq!(SlotMap::from_json(&m.to_json()).unwrap(), m);
            assert!((0..n).all(|i| m.ranges(i).len() == 1));
            assert_eq!(shard_of(&Value::Str("p0000042".into()), n), m.shard_of(&Value::Str("p0000042".into())));
        }
        assert_eq!(SlotMap::even(2, 1).to_json(), json!({"epoch": 1, "shards": [[[0, 2048]], [[2048, 4096]]]}));
        let moved = SlotMap::from_json(&json!({"epoch": 2, "shards": [[[0, 1024], [2048, 4096]], [[1024, 2048]]]})).unwrap();
        assert!(!moved.same_slots(&SlotMap::even(2, 1), 0) && moved.same_slots(&moved, 1));
        for (bad, why) in [
            (json!({"shards": [[[0, 4096]]]}), "epoch"),
            (json!({"epoch": 1, "shards": [[[0, 4000]]]}), "slot 4000 is given to no shard"),
            (json!({"epoch": 1, "shards": [[[0, 4096]], [[5, 6]]]}), "slot 5 is given to shards 0 and 1"),
            (json!({"epoch": 1, "shards": [[[0, 4097]]]}), "not a range"),
            (json!({"epoch": 1, "shards": [[[0, 4096]], []]}), "shard 1 holds no slot"),
            (json!({"epoch": 1, "shards": [[[7, 3]]]}), "not a range"),
        ] {
            let e = SlotMap::from_json(&bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }
}
