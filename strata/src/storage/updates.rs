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

use crate::error::{corrupt, Result};
use crate::segment::DocId;
use crate::value::{Document, Value};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Updates {
    map: BTreeMap<(u16, DocId), Value>,
}

impl Updates {
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Sets document `d`'s `field`; returns whether the entry is new.
    pub fn set(&mut self, field: usize, d: DocId, v: Value) -> bool {
        self.map.insert((field as u16, d), v).is_none()
    }

    pub fn get(&self, field: usize, d: DocId) -> Option<&Value> {
        self.map.get(&(field as u16, d))
    }

    /// Whether any document's `field` was updated.
    pub fn has(&self, field: usize) -> bool {
        self.field(field).next().is_some()
    }

    /// The updated documents of `field`, by docID.
    pub fn field(&self, field: usize) -> impl Iterator<Item = (DocId, &Value)> + '_ {
        let f = field as u16;
        self.map.range((f, 0)..=(f, DocId::MAX)).map(|(&(_, d), v)| (d, v))
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, DocId, &Value)> + '_ {
        self.map.iter().map(|(&(f, d), v)| (f as usize, d, v))
    }

    /// `base` with `newer` written over it.
    pub fn merged(base: Option<&Updates>, newer: &Updates) -> Updates {
        let mut out = base.cloned().unwrap_or_default();
        out.map.extend(newer.map.iter().map(|(k, v)| (*k, v.clone())));
        out
    }

    /// Writes document `d`'s updated fields into `doc`.
    pub fn apply(&self, d: DocId, doc: &mut Document) {
        for (f, x, v) in self.iter() {
            if x == d {
                doc[f] = v.clone();
            }
        }
    }

    /// The updates of documents `from..from + len`, renumbered from 0.
    pub fn slice(&self, from: DocId, len: u32) -> Updates {
        let end = from.saturating_add(len);
        let map = self.map.iter().filter(|((_, d), _)| (from..end).contains(d)).map(|(&(f, d), v)| ((f, d - from), v.clone())).collect();
        Updates { map }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.map.len() * 15);
        out.extend_from_slice(&(self.map.len() as u32).to_le_bytes());
        for (&(f, d), v) in &self.map {
            out.extend_from_slice(&f.to_le_bytes());
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
        let mut map = BTreeMap::new();
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
        Ok(Updates { map })
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
}
