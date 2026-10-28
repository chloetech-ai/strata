//! In-place updates of numeric fields: values that replace a document's
//! INT or FLOAT field without rewriting the document, for counters, stock
//! levels and prices that change far more often than the text or vectors
//! beside them.
//!
//! A segment's updates are a sorted map from (field, docID) to the new
//! value. Like a delete set, the map is written beside the segment at a
//! flush (an extent of the file, or `upd-<id>-<txn>.bin` in the directory
//! layout), merged with the updates not yet flushed when a snapshot is
//! taken, and folded into the documents when the segment is merged. The
//! executor reads a field through its segment's updates: filters on the
//! field are corrected for the documents they name, and orders, scores and
//! projections read the new value.
//!
//! Encoding: `u32` entry count, then per entry `u16` field, `u32` docID, a
//! tag (0 null, 1 integer, 2 float) and 8 bytes of value, little-endian,
//! sorted by (field, docID).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::{corrupt, Result};
use crate::segment::DocId;
use crate::value::{Document, Value};

type Map = BTreeMap<(u16, DocId), Value>;

/// A segment's values written in place, by (field, docID).
///
/// The entries are kept in layers, oldest first, an entry of a later layer
/// replacing the same (field, docID) in an earlier one. [`Updates::set`]
/// merges the newest layer into the one before while it is more than half
/// that one's size, so a map of n entries has O(log n) layers and a set
/// costs O(log n) amortized; a layer a snapshot shares is never written
/// again, and [`Updates::merged`] shares the layers of both sides. So
/// taking a snapshot after a write copies no entries, where one map would
/// copy every value written in place since the segment was built.
#[derive(Clone, Debug, Default)]
pub struct Updates {
    layers: Vec<Arc<Map>>,
}

impl PartialEq for Updates {
    fn eq(&self, other: &Updates) -> bool {
        self.iter().eq(other.iter())
    }
}

impl Updates {
    /// The number of (field, docID) entries.
    pub fn len(&self) -> usize {
        match &self.layers[..] {
            [] => 0,
            [m] => m.len(),
            _ => self.iter().count(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.layers.iter().all(|m| m.is_empty())
    }

    /// Sets document `d`'s `field`; returns whether the entry is new.
    pub fn set(&mut self, field: usize, d: DocId, v: Value) -> bool {
        let new = self.get(field, d).is_none();
        let key = (field as u16, d);
        match self.layers.last_mut().and_then(Arc::get_mut) {
            Some(m) => {
                m.insert(key, v);
            }
            None => self.layers.push(Arc::new(Map::from([(key, v)]))),
        }
        while let [.., below, top] = &self.layers[..] {
            if 2 * top.len() <= below.len() {
                break;
            }
            let top = self.layers.pop().expect("two layers");
            let below = Arc::make_mut(self.layers.last_mut().expect("two layers"));
            match Arc::try_unwrap(top) {
                Ok(top) => below.extend(top),
                Err(top) => below.extend(top.iter().map(|(k, v)| (*k, v.clone()))),
            }
        }
        new
    }

    pub fn get(&self, field: usize, d: DocId) -> Option<&Value> {
        let key = (field as u16, d);
        self.layers.iter().rev().find_map(|m| m.get(&key))
    }

    /// Whether any document's `field` was updated.
    pub fn has(&self, field: usize) -> bool {
        let f = field as u16;
        self.layers.iter().any(|m| m.range((f, 0)..=(f, DocId::MAX)).next().is_some())
    }

    /// The fields some document's value was written of, in order.
    fn fields(&self) -> Vec<u16> {
        let mut out: Vec<u16> = Vec::new();
        for m in &self.layers {
            let mut next = m.keys().next().map(|k| k.0);
            while let Some(f) = next {
                out.push(f);
                next = f.checked_add(1).and_then(|g| m.range((g, 0)..).next()).map(|(k, _)| k.0);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The updated documents of `field`, by docID.
    pub fn field(&self, field: usize) -> impl Iterator<Item = (DocId, &Value)> + '_ {
        let f = field as u16;
        self.range(f, 0..=DocId::MAX).into_iter()
    }

    /// The entries of `field` for documents in `docs`, by docID, each with
    /// its newest value.
    fn range(&self, f: u16, docs: std::ops::RangeInclusive<DocId>) -> Vec<(DocId, &Value)> {
        let (lo, hi) = (*docs.start(), *docs.end());
        if let [m] = &self.layers[..] {
            return m.range((f, lo)..=(f, hi)).map(|(&(_, d), v)| (d, v)).collect();
        }
        let mut out = BTreeMap::new();
        for m in &self.layers {
            out.extend(m.range((f, lo)..=(f, hi)).map(|(&(_, d), v)| (d, v)));
        }
        out.into_iter().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, DocId, &Value)> + '_ {
        let all: Vec<(usize, DocId, &Value)> = match &self.layers[..] {
            [m] => m.iter().map(|(&(f, d), v)| (f as usize, d, v)).collect(),
            _ => {
                let mut out = BTreeMap::new();
                for m in &self.layers {
                    out.extend(m.iter().map(|(k, v)| (*k, v)));
                }
                out.into_iter().map(|((f, d), v)| (f as usize, d, v)).collect()
            }
        };
        all.into_iter()
    }

    /// `base` with `newer` written over it, sharing the layers of both.
    pub fn merged(base: Option<&Updates>, newer: &Updates) -> Updates {
        let mut layers = base.map(|b| b.layers.clone()).unwrap_or_default();
        layers.extend(newer.layers.iter().cloned());
        Updates { layers }
    }

    /// Writes document `d`'s updated fields into `doc`.
    pub fn apply(&self, d: DocId, doc: &mut Document) {
        for f in self.fields() {
            if let Some(v) = self.get(f as usize, d) {
                doc[f as usize] = v.clone();
            }
        }
    }

    /// The updates of documents `from..from + len`, renumbered from 0.
    pub fn slice(&self, from: DocId, len: u32) -> Updates {
        if len == 0 || self.is_empty() {
            return Updates::default();
        }
        let last = from.saturating_add(len - 1);
        let mut map = Map::new();
        for f in self.fields() {
            map.extend(self.range(f, from..=last).into_iter().map(|(d, v)| ((f, d - from), v.clone())));
        }
        Updates { layers: vec![Arc::new(map)] }
    }

    pub fn encode(&self) -> Vec<u8> {
        let entries: Vec<(usize, DocId, &Value)> = self.iter().collect();
        let mut out = Vec::with_capacity(4 + entries.len() * 15);
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (f, d, v) in entries {
            out.extend_from_slice(&(f as u16).to_le_bytes());
            out.extend_from_slice(&d.to_le_bytes());
            let (tag, bits) = match v {
                Value::Int(x) => (1u8, *x as u64),
                Value::Float(x) => (2, x.to_bits()),
                _ => (0, 0),
            };
            out.push(tag);
            out.extend_from_slice(&bits.to_le_bytes());
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<Updates> {
        let bad = || corrupt("bad in-place updates");
        let n = u32::from_le_bytes(b.get(..4).ok_or_else(bad)?.try_into().unwrap()) as usize;
        if b.len() != 4 + n * 15 {
            return Err(bad());
        }
        let mut map = Map::new();
        for e in b[4..].chunks_exact(15) {
            let f = u16::from_le_bytes(e[0..2].try_into().unwrap());
            let d = u32::from_le_bytes(e[2..6].try_into().unwrap());
            let bits = u64::from_le_bytes(e[7..15].try_into().unwrap());
            let v = match e[6] {
                0 => Value::Null,
                1 => Value::Int(bits as i64),
                2 => Value::Float(f64::from_bits(bits)),
                _ => return Err(bad()),
            };
            map.insert((f, d), v);
        }
        Ok(Updates { layers: vec![Arc::new(map)] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_merge_and_slice() {
        let mut a = Updates::default();
        assert!(a.set(2, 7, Value::Int(-3)));
        assert!(a.set(1, 9, Value::Float(2.5)));
        assert!(!a.set(2, 7, Value::Int(4)));
        a.set(2, 300, Value::Null);
        assert_eq!(Updates::decode(&a.encode()).unwrap(), a);
        let mut b = Updates::default();
        b.set(2, 7, Value::Int(10));
        let m = Updates::merged(Some(&a), &b);
        assert_eq!(m.get(2, 7), Some(&Value::Int(10)));
        assert_eq!(m.field(2).map(|(d, _)| d).collect::<Vec<_>>(), vec![7, 300]);
        let s = m.slice(256, 100);
        assert_eq!(s.get(2, 44), Some(&Value::Null));
        assert_eq!(s.len(), 1);
        assert!(Updates::decode(&[1, 0, 0, 0, 9]).is_err());
    }

    /// Sets with copies taken in between (as snapshots take them) against
    /// one map: every copy keeps the entries of its moment, and the layers
    /// stay few.
    #[test]
    fn layers_read_like_one_map() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        let mut rng = StdRng::seed_from_u64(5);
        let mut u = Updates::default();
        let mut model: BTreeMap<(u16, DocId), Value> = BTreeMap::new();
        let mut copies: Vec<(Updates, BTreeMap<(u16, DocId), Value>)> = Vec::new();
        for step in 0..20_000 {
            let (f, d) = (rng.gen_range(0..4usize), rng.gen_range(0..3000));
            let v = Value::Int(rng.gen_range(-5..5));
            assert_eq!(u.set(f, d, v.clone()), model.insert((f as u16, d), v).is_none());
            if rng.gen_bool(0.3) {
                // A snapshot: the stored map with these written over.
                copies.push((Updates::merged(None, &u), model.clone()));
                if copies.len() > 8 {
                    copies.remove(rng.gen_range(0..copies.len()));
                }
            }
            if step % 1000 == 999 {
                assert!(u.layers.len() <= 2 + 2 * (usize::BITS - u.len().leading_zeros()) as usize, "{} layers for {}", u.layers.len(), u.len());
                for (c, m) in copies.iter().chain([(&u, &model)].map(|(a, b)| (a.clone(), b.clone())).iter()) {
                    assert_eq!(c.len(), m.len());
                    assert!(c.iter().map(|(f, d, v)| ((f as u16, d), v.clone())).eq(m.clone()));
                    for _ in 0..50 {
                        let (f, d) = (rng.gen_range(0..5usize), rng.gen_range(0..3000));
                        assert_eq!(c.get(f, d), m.get(&(f as u16, d)));
                    }
                    let f = rng.gen_range(0..4u16);
                    assert!(c.field(f as usize).map(|(d, v)| ((f, d), v.clone())).eq(m.range((f, 0)..=(f, DocId::MAX)).map(|(k, v)| (*k, v.clone()))));
                    let mut doc = vec![Value::Null; 5];
                    let d = rng.gen_range(0..3000);
                    c.apply(d, &mut doc);
                    for (f, x) in doc.iter().enumerate() {
                        assert_eq!(m.get(&(f as u16, d)).unwrap_or(&Value::Null), x);
                    }
                    let (from, len) = (rng.gen_range(0..3000), rng.gen_range(0..500));
                    let s = c.slice(from, len);
                    let want: Vec<_> = m.iter().filter(|((_, d), _)| (from..from + len).contains(d)).map(|(&(f, d), v)| (f as usize, d - from, v.clone())).collect();
                    assert!(s.iter().map(|(f, d, v)| (f, d, v.clone())).eq(want));
                    assert_eq!(Updates::decode(&c.encode()).unwrap(), *c);
                }
            }
        }
    }

    /// A snapshot's view of a segment shares the stored layers and the
    /// pending ones instead of copying their entries.
    #[test]
    fn merged_shares_layers() {
        let mut base = Updates::default();
        for d in 0..1000 {
            base.set(1, d, Value::Int(d as i64));
        }
        let base = Updates::decode(&base.encode()).unwrap();
        let mut newer = Updates::default();
        newer.set(1, 5, Value::Int(-1));
        let m = Updates::merged(Some(&base), &newer);
        assert!(Arc::ptr_eq(&m.layers[0], &base.layers[0]) && Arc::ptr_eq(&m.layers[1], &newer.layers[0]));
        assert_eq!(m.get(1, 5), Some(&Value::Int(-1)));
        assert_eq!(m.get(1, 6), Some(&Value::Int(6)));
        // Setting after the snapshot leaves its layers alone.
        newer.set(1, 6, Value::Int(-2));
        assert_eq!(m.get(1, 6), Some(&Value::Int(6)));
        assert_eq!(newer.get(1, 6), Some(&Value::Int(-2)));
    }
}
