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
//! In a database that numbers its writes, a document's sequence number
//! (`crate::storage::rowseq`) changes with each value written in place: it
//! is kept here too, as the integer of field [`SEQ`], which is no field of
//! the document's.
//!
//! Encoding: `u32` entry count, then per entry `u16` field, `u32` docID, a
//! tag (0 null, 1 integer, 2 float) and 8 bytes of value, little-endian,
//! sorted by (field, docID). The field is its id
//! ([`crate::schema::Field::id`], below [`SEQ`]); a database written
//! before field ids wrote its position, which its schema reads as its id.
//! In memory, entries are keyed by the field's position in the table's
//! schema, which [`Updates::encode`] and [`Updates::decode`] translate;
//! an entry of a field the schema no longer has is dropped when read.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::{corrupt, Result};
use crate::schema::Schema;
use crate::segment::DocId;
use crate::value::{Document, Value};

type Map = BTreeMap<(u16, DocId), Value>;

/// The field under which a document's sequence number is kept, when a
/// value written in place changed it; no schema has this many fields.
pub const SEQ: usize = u16::MAX as usize;

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

    /// The entries of every layer, an entry replaced in a later layer
    /// counted again: what reading all of them costs.
    pub fn stored(&self) -> usize {
        self.layers.iter().map(|m| m.len()).sum()
    }

    /// Calls `f` with each updated document of `field` and its value,
    /// layer by layer from the oldest, so a document's last call has its
    /// newest value.
    pub fn each_oldest_first(&self, field: usize, mut f: impl FnMut(DocId, &Value)) {
        let fl = field as u16;
        for m in &self.layers {
            for (&(_, d), v) in m.range((fl, 0)..=(fl, DocId::MAX)) {
                f(d, v);
            }
        }
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

    /// Writes document `d`'s updated fields into `doc` (not its sequence
    /// number, [`SEQ`]).
    pub fn apply(&self, d: DocId, doc: &mut Document) {
        for f in self.fields() {
            if f as usize == SEQ {
                continue;
            }
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

    /// The updates of a segment of a table of `schema`, as written beside
    /// it: by field id.
    pub fn encode(&self, schema: &Schema) -> Vec<u8> {
        let id = |f: usize| if f == SEQ { SEQ as u16 } else { schema.id_of(f) as u16 };
        let mut entries: Vec<(u16, DocId, &Value)> = self.iter().map(|(f, d, v)| (id(f), d, v)).collect();
        entries.sort_by_key(|&(f, d, _)| (f, d));
        let mut out = Vec::with_capacity(4 + entries.len() * 15);
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (f, d, v) in entries {
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

    /// Reads what [`Updates::encode`] wrote for `schema`'s table.
    pub fn decode(b: &[u8], schema: &Schema) -> Result<Updates> {
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
            let f = match f as usize {
                SEQ => SEQ,
                _ => match schema.position_of(f as u32) {
                    Some(i) => i,
                    None => continue,
                },
            };
            map.insert((f as u16, d), v);
        }
        Ok(Updates { layers: vec![Arc::new(map)] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Field, FieldType};

    /// A table with enough fields for the tests' positions.
    fn schema() -> Schema {
        Schema::new("t", (0..8).map(|i| Field::new(&format!("n{i}"), FieldType::Int)).collect(), None).unwrap()
    }

    #[test]
    fn written_by_field_id() {
        // Fields 1 and 2 have ids 7 and 4 (as after columns were dropped).
        let mut ids = schema();
        ids.fields.truncate(3);
        ids.fields[1].id = 7;
        ids.fields[2].id = 4;
        ids.next_field_id = 8;
        let mut a = Updates::default();
        a.set(1, 5, Value::Int(10));
        a.set(2, 5, Value::Int(20));
        a.set(SEQ, 5, Value::Int(3));
        let b = a.encode(&ids);
        assert_eq!(u16::from_le_bytes(b[4..6].try_into().unwrap()), 4, "sorted by id");
        assert_eq!(Updates::decode(&b, &ids).unwrap(), a);
        // A field the schema no longer has is dropped.
        let mut fewer = ids.clone();
        fewer.fields.remove(1);
        let back = Updates::decode(&b, &fewer).unwrap();
        assert_eq!((back.get(1, 5), back.get(SEQ, 5), back.len()), (Some(&Value::Int(20)), Some(&Value::Int(3)), 2));
    }

    #[test]
    fn round_trip_merge_and_slice() {
        let mut a = Updates::default();
        assert!(a.set(2, 7, Value::Int(-3)));
        assert!(a.set(1, 9, Value::Float(2.5)));
        assert!(!a.set(2, 7, Value::Int(4)));
        a.set(2, 300, Value::Null);
        assert_eq!(Updates::decode(&a.encode(&schema()), &schema()).unwrap(), a);
        let mut b = Updates::default();
        b.set(2, 7, Value::Int(10));
        let m = Updates::merged(Some(&a), &b);
        assert_eq!(m.get(2, 7), Some(&Value::Int(10)));
        assert_eq!(m.field(2).map(|(d, _)| d).collect::<Vec<_>>(), vec![7, 300]);
        let s = m.slice(256, 100);
        assert_eq!(s.get(2, 44), Some(&Value::Null));
        assert_eq!(s.len(), 1);
        assert!(Updates::decode(&[1, 0, 0, 0, 9], &schema()).is_err());
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
                    assert_eq!(Updates::decode(&c.encode(&schema()), &schema()).unwrap(), *c);
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
        let base = Updates::decode(&base.encode(&schema()), &schema()).unwrap();
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
