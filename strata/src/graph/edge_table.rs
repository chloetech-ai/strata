//! An edge table's index: each segment of a table with `OPTIONS(edges =
//! 'src, dst, w')` ([`crate::schema::Schema::with_edges`]) stores one
//! section, `edge_table`, that finds the rows leaving or reaching a key of
//! the table its rows link:
//!
//! ```text
//! version | key kind (1 str, 2 int) | n_rows | n_keys | flags | dir_bits
//! keys:    [i64; K]  or  key offsets [u64; K + 1] + key bytes   the distinct keys, by hash
//! hashes:  [u64; K]                    each key's hash
//! dir:     [u32; 2^dir_bits + 1]       radix directory over the hashes
//! src:     [u32; n]                    each row's source, as a key number (u32::MAX: none)
//! dst:     [u32; n]                    each row's target
//! weights: [f32; n]                    each row's weight when built (flag 1; else every 1)
//! out:     offsets [u32; K + 1] + rows   the rows leaving each key, in row order
//! in:      offsets [u32; K + 1] + rows   the rows reaching each key
//! ```
//!
//! A row without a source or a target links nothing and is in neither
//! list. A key is found as in the primary-key index: one directory read, a
//! short scan of hashes and one stored-key comparison. A row's other end is
//! a key number, so a walk resolves each distinct key of a segment to a
//! row of the linked table once. The walk reads weights changed in place
//! over the section's.

use std::collections::HashMap;

use super::{bucket, dir_bits};
use crate::error::{corrupt, Error, Result};
use crate::postings::bytes::{ByteReader, ByteWriter, OwnedBytes, Slice};
use crate::schema::{FieldType, Schema};
use crate::segment::{DocId, SegmentFile, SegmentWriter};
use crate::storage::{key_hash, KeyRef};
use crate::value::Value;

/// Section name of an edge table's index.
pub const SECTION: &str = "edge_table";

const VERSION: u64 = 1;
const KIND_STR: u64 = 1;
const KIND_INT: u64 = 2;
const FLAG_WEIGHTS: u64 = 1;
const NONE: u32 = u32::MAX;

/// The builder of an edge table's section; `None` for another table.
pub fn builder(schema: &Schema) -> Option<EdgeTableBuilder> {
    let (src, dst, weight) = schema.edge_fields()?;
    let int_keys = schema.fields[src].ty == FieldType::Int;
    Some(EdgeTableBuilder { table: schema.table.clone(), int_keys, src, dst, weight, ids: HashMap::new(), keys: Vec::new(), rows: Vec::new(), weights: Vec::new() })
}

/// An edge table row's weight as a number (0 when it is not one).
pub fn weight_of(v: &Value) -> f32 {
    match v {
        Value::Float(x) => *x as f32,
        Value::Int(x) => *x as f32,
        _ => 0.0,
    }
}

/// Builds an edge table's section from its rows.
pub struct EdgeTableBuilder {
    table: String,
    int_keys: bool,
    src: usize,
    dst: usize,
    weight: Option<usize>,
    /// Key numbers by key (an integer key as its 8 bytes), in order met.
    ids: HashMap<Vec<u8>, u32>,
    keys: Vec<Vec<u8>>,
    /// Each row's source and target numbers.
    rows: Vec<(u32, u32)>,
    weights: Vec<f32>,
}

impl EdgeTableBuilder {
    /// Adds the next row: an edge from its source to its target, if it has
    /// both, weighted by its weight (1 without a weight field, 0 for NULL).
    pub fn add(&mut self, doc: DocId, row: &[Value]) -> Result<()> {
        debug_assert_eq!(doc as usize, self.rows.len());
        let (src, dst) = (&row[self.src], &row[self.dst]);
        let link = if matches!(src, Value::Null) || matches!(dst, Value::Null) { (NONE, NONE) } else { (self.id(src)?, self.id(dst)?) };
        self.rows.push(link);
        self.weights.push(self.weight.map_or(1.0, |w| weight_of(&row[w])));
        Ok(())
    }

    fn id(&mut self, k: &Value) -> Result<u32> {
        let bytes = match (k, self.int_keys) {
            (Value::Int(x), true) => x.to_le_bytes().to_vec(),
            (Value::Str(s), false) => s.as_bytes().to_vec(),
            (v, _) => return Err(Error::Schema(format!("edge table {}: {v:?} is not a key of the type its rows link", self.table))),
        };
        let next = self.keys.len() as u32;
        Ok(*self.ids.entry(bytes).or_insert_with_key(|b| {
            self.keys.push(b.clone());
            next
        }))
    }

    pub fn finish(self, out: &mut SegmentWriter) -> Result<()> {
        let b = self;
        let hash = |k: &[u8]| match b.int_keys {
            true => key_hash(KeyRef::Int(i64::from_le_bytes(k.try_into().expect("8 bytes")))),
            false => key_hash(KeyRef::Str(k)),
        };
        // Keys by hash, then by key (colliding hashes keep both keys).
        let mut order: Vec<(u64, u32)> = b.keys.iter().enumerate().map(|(i, k)| (hash(k), i as u32)).collect();
        order.sort_unstable_by(|x, y| x.0.cmp(&y.0).then_with(|| b.keys[x.1 as usize].cmp(&b.keys[y.1 as usize])));
        let mut renumber = vec![0u32; b.keys.len()];
        for (new, &(_, old)) in order.iter().enumerate() {
            renumber[old as usize] = new as u32;
        }
        let hashes: Vec<u64> = order.iter().map(|&(h, _)| h).collect();
        let at = |k: u32| if k == NONE { NONE } else { renumber[k as usize] };
        let src: Vec<u32> = b.rows.iter().map(|&(s, _)| at(s)).collect();
        let dst: Vec<u32> = b.rows.iter().map(|&(_, d)| at(d)).collect();
        // The rows of each key, in row order: a counting sort.
        let lists = |ends: &[u32]| -> (Vec<u32>, Vec<u32>) {
            let mut off = vec![0u32; hashes.len() + 1];
            for &k in ends.iter().filter(|&&k| k != NONE) {
                off[k as usize + 1] += 1;
            }
            for i in 1..off.len() {
                off[i] += off[i - 1];
            }
            let mut next = off.clone();
            let mut rows = vec![0u32; off[hashes.len()] as usize];
            for (r, &k) in ends.iter().enumerate().filter(|(_, &k)| k != NONE) {
                rows[next[k as usize] as usize] = r as u32;
                next[k as usize] += 1;
            }
            (off, rows)
        };
        let ((out_off, out_rows), (in_off, in_rows)) = (lists(&src), lists(&dst));
        let bits = dir_bits(hashes.len());
        let mut dir = vec![0u32; (1usize << bits) + 1];
        let mut i = 0;
        for (slot_b, slot) in dir.iter_mut().enumerate() {
            while i < hashes.len() && bucket(hashes[i], bits) < slot_b {
                i += 1;
            }
            *slot = i as u32;
        }
        let has_weights = b.weights.iter().any(|&w| w != 1.0);
        let mut w = ByteWriter::new();
        w.u64(VERSION);
        w.u64(if b.int_keys { KIND_INT } else { KIND_STR });
        w.u64(b.rows.len() as u64);
        w.u64(hashes.len() as u64);
        w.u64(if has_weights { FLAG_WEIGHTS } else { 0 });
        w.u64(bits as u64);
        if b.int_keys {
            let ints: Vec<i64> = order.iter().map(|&(_, k)| i64::from_le_bytes(b.keys[k as usize][..].try_into().expect("8 bytes"))).collect();
            w.array(&ints);
        } else {
            let mut key_off = vec![0u64];
            let mut key_bytes = Vec::new();
            for &(_, k) in &order {
                key_bytes.extend_from_slice(&b.keys[k as usize]);
                key_off.push(key_bytes.len() as u64);
            }
            w.array(&key_off);
            w.array(&key_bytes);
        }
        w.array(&hashes);
        w.array(&dir);
        w.array(&src);
        w.array(&dst);
        w.array(if has_weights { &b.weights[..] } else { &[] });
        w.array(&out_off);
        w.array(&out_rows);
        w.array(&in_off);
        w.array(&in_rows);
        out.put(SECTION.to_string(), w.finish());
        Ok(())
    }
}

/// Entry `k` of the lists `rows` with offsets `off` (empty if damaged).
#[inline]
fn list<'a>(off: &[u32], rows: &'a [u32], k: u32) -> &'a [u32] {
    let k = k as usize;
    match (off.get(k), off.get(k + 1)) {
        (Some(&a), Some(&b)) => rows.get(a as usize..b as usize).unwrap_or(&[]),
        _ => &[],
    }
}

/// Zero-copy reader of one segment's `edge_table` section.
#[derive(Clone, Debug)]
pub struct EdgeTableReader {
    int_keys: bool,
    ints: Slice<i64>,
    key_off: Slice<u64>,
    key_bytes: Slice<u8>,
    hashes: Slice<u64>,
    bits: u32,
    dir: Slice<u32>,
    src: Slice<u32>,
    dst: Slice<u32>,
    weights: Slice<f32>,
    out_off: Slice<u32>,
    out_rows: Slice<u32>,
    in_off: Slice<u32>,
    in_rows: Slice<u32>,
}

impl EdgeTableReader {
    /// The section of an edge table's segment, `None` if it has none.
    pub fn open(seg: &SegmentFile) -> Result<Option<EdgeTableReader>> {
        let Some(r) = seg.section(SECTION)?.map(EdgeTableReader::from_section).transpose()? else { return Ok(None) };
        if r.n_rows() != seg.n_docs() {
            return Err(corrupt("the edge table section's rows are not the segment's"));
        }
        Ok(Some(r))
    }

    /// The section's parts, their sizes checked: it is opened by every walk,
    /// so what they hold is checked as it is read (a damaged section finds
    /// wrong rows or none, never past its arrays).
    pub fn from_section(bytes: OwnedBytes) -> Result<EdgeTableReader> {
        let mut r = ByteReader::new(bytes);
        let mut u = || r.u64().map_err(corrupt);
        let (version, kind, n_rows, n_keys, flags, bits) = (u()?, u()?, u()? as usize, u()? as usize, u()?, u()? as u32);
        if version != VERSION || !(kind == KIND_STR || kind == KIND_INT) || bits > 24 {
            return Err(corrupt("bad edge table section header"));
        }
        let int_keys = kind == KIND_INT;
        let (mut ints, mut key_off, mut key_bytes) = (Slice::default(), Slice::default(), Slice::default());
        if int_keys {
            ints = r.array().map_err(corrupt)?;
        } else {
            key_off = r.array().map_err(corrupt)?;
            key_bytes = r.array().map_err(corrupt)?;
        }
        let hashes: Slice<u64> = r.array().map_err(corrupt)?;
        let dir: Slice<u32> = r.array().map_err(corrupt)?;
        let src: Slice<u32> = r.array().map_err(corrupt)?;
        let dst: Slice<u32> = r.array().map_err(corrupt)?;
        let weights: Slice<f32> = r.array().map_err(corrupt)?;
        let out_off: Slice<u32> = r.array().map_err(corrupt)?;
        let out_rows: Slice<u32> = r.array().map_err(corrupt)?;
        let in_off: Slice<u32> = r.array().map_err(corrupt)?;
        let in_rows: Slice<u32> = r.array().map_err(corrupt)?;
        let keys_ok = match int_keys {
            true => ints.len() == n_keys,
            false => key_off.len() == n_keys + 1 && key_off[n_keys] as usize == key_bytes.len(),
        };
        let list_ok = |off: &[u32], rows: &[u32]| off.len() == n_keys + 1 && off[n_keys] as usize == rows.len();
        if !keys_ok
            || hashes.len() != n_keys
            || dir.len() != (1 << bits) + 1
            || src.len() != n_rows
            || dst.len() != n_rows
            || weights.len() != if flags & FLAG_WEIGHTS != 0 { n_rows } else { 0 }
            || !list_ok(&out_off, &out_rows)
            || !list_ok(&in_off, &in_rows)
        {
            return Err(corrupt("edge table section sizes disagree"));
        }
        Ok(EdgeTableReader { int_keys, ints, key_off, key_bytes, hashes, bits, dir, src, dst, weights, out_off, out_rows, in_off, in_rows })
    }

    /// Key number `k`.
    #[inline]
    pub fn key(&self, k: u32) -> Option<KeyRef<'_>> {
        let k = k as usize;
        match self.int_keys {
            true => self.ints.get(k).map(|&x| KeyRef::Int(x)),
            false => self.key_bytes.get(*self.key_off.get(k)? as usize..*self.key_off.get(k + 1)? as usize).map(KeyRef::Str),
        }
    }

    /// The number of `key` (with hash `h = key_hash(key)`), if a row of
    /// this segment links it.
    pub fn find(&self, key: KeyRef, h: u64) -> Option<u32> {
        let b = bucket(h, self.bits);
        let (mut i, hi) = (self.dir[b] as usize, (self.dir[b + 1] as usize).min(self.hashes.len()));
        while i < hi && self.hashes[i] < h {
            i += 1;
        }
        while i < hi && self.hashes[i] == h {
            // Integer hashes are a bijection: equal hashes are equal keys.
            if self.int_keys || self.key(i as u32) == Some(key) {
                return Some(i as u32);
            }
            i += 1;
        }
        None
    }

    /// The rows whose source is key number `k`, in row order.
    #[inline]
    pub fn leaving(&self, k: u32) -> &[u32] {
        list(&self.out_off, &self.out_rows, k)
    }

    /// The rows whose target is key number `k`, in row order.
    #[inline]
    pub fn reaching(&self, k: u32) -> &[u32] {
        list(&self.in_off, &self.in_rows, k)
    }

    /// Row `d`'s source key number, `None` if it has none.
    #[inline]
    pub fn source(&self, d: DocId) -> Option<u32> {
        self.src.get(d as usize).copied().filter(|&k| k != NONE)
    }

    /// Row `d`'s target key number, `None` if it has none.
    #[inline]
    pub fn target(&self, d: DocId) -> Option<u32> {
        self.dst.get(d as usize).copied().filter(|&k| k != NONE)
    }

    /// Row `d`'s weight when the segment was built.
    #[inline]
    pub fn weight(&self, d: DocId) -> f32 {
        if self.weights.is_empty() { 1.0 } else { self.weights.get(d as usize).copied().unwrap_or(0.0) }
    }

    /// Every row's weight when the segment was built.
    pub fn weights(&self) -> Vec<f32> {
        match self.weights.is_empty() {
            true => vec![1.0; self.n_rows()],
            false => self.weights.to_vec(),
        }
    }

    pub fn n_keys(&self) -> usize {
        self.hashes.len()
    }

    pub fn n_rows(&self) -> usize {
        self.src.len()
    }

    /// Bytes of the section's parts, for size breakdowns.
    pub fn sizes(&self) -> Vec<(&'static str, usize)> {
        vec![
            ("keys", self.ints.byte_len() + self.key_off.byte_len() + self.key_bytes.byte_len() + self.hashes.byte_len() + self.dir.byte_len()),
            ("ends", self.src.byte_len() + self.dst.byte_len()),
            ("weights", self.weights.byte_len()),
            ("lists", self.out_off.byte_len() + self.out_rows.byte_len() + self.in_off.byte_len() + self.in_rows.byte_len()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Field;

    fn schema(int: bool) -> Schema {
        let ty = if int { FieldType::Int } else { FieldType::Keyword };
        let fields = vec![Field::new("id", FieldType::Keyword), Field::new("a", ty.clone()), Field::new("b", ty), Field::new("w", FieldType::Float)];
        Schema::new("e", fields, Some("id")).unwrap().with_edges("a", "b", Some("w")).unwrap()
    }

    fn section(schema: &Schema, rows: &[[Value; 4]]) -> EdgeTableReader {
        let mut b = builder(schema).unwrap();
        for (d, r) in rows.iter().enumerate() {
            b.add(d as DocId, r).unwrap();
        }
        let mut out = SegmentWriter::new(rows.len());
        b.finish(&mut out).unwrap();
        EdgeTableReader::open(&SegmentFile::from_vec(out.finish()).unwrap()).unwrap().unwrap()
    }

    #[test]
    fn finds_the_rows_leaving_and_reaching_each_key() {
        let s = |x: &str| Value::Str(x.into());
        let rows = [
            [s("r0"), s("x"), s("y"), Value::Float(2.0)],
            [s("r1"), s("y"), s("x"), Value::Float(1.0)],
            [s("r2"), s("x"), s("z"), Value::Null],
            [s("r3"), Value::Null, s("z"), Value::Float(1.0)],
            [s("r4"), s("x"), s("y"), Value::Float(0.5)],
        ];
        let r = section(&schema(false), &rows);
        let k = |x: &str| r.find(KeyRef::Str(x.as_bytes()), key_hash(KeyRef::Str(x.as_bytes()))).unwrap();
        let (x, y, z) = (k("x"), k("y"), k("z"));
        assert_eq!(r.leaving(x), [0, 2, 4]);
        assert_eq!(r.reaching(y), [0, 4]);
        assert_eq!(r.reaching(z), [2], "a row without a source links nothing");
        assert!(r.leaving(z).is_empty());
        assert_eq!((r.source(1), r.target(1)), (Some(y), Some(x)));
        assert_eq!(r.source(3), None);
        assert_eq!(r.key(z), Some(KeyRef::Str(b"z")));
        assert_eq!([r.weight(0), r.weight(2), r.weight(4)], [2.0, 0.0, 0.5]);
        assert!(r.find(KeyRef::Str(b"q"), key_hash(KeyRef::Str(b"q"))).is_none());
    }

    #[test]
    fn integer_keys_and_unit_weights() {
        let rows: Vec<[Value; 4]> = (0..300).map(|i| [Value::Str(format!("r{i}")), Value::Int(i % 17), Value::Int((i * 7) % 23), Value::Float(1.0)]).collect();
        let r = section(&schema(true), &rows);
        for k in 0..17i64 {
            let n = r.find(KeyRef::Int(k), key_hash(KeyRef::Int(k))).unwrap();
            assert_eq!(r.key(n), Some(KeyRef::Int(k)));
            let want: Vec<u32> = (0..300u32).filter(|i| (*i as i64) % 17 == k).collect();
            assert_eq!(r.leaving(n), &want[..]);
        }
        assert!(r.weights.is_empty(), "every weight is 1");
        assert_eq!(r.weight(5), 1.0);
    }

    #[test]
    fn a_damaged_section_is_refused() {
        let rows = [[Value::Str("r0".into()), Value::Int(1), Value::Int(2), Value::Float(1.0)]];
        let mut b = builder(&schema(true)).unwrap();
        b.add(0, &rows[0]).unwrap();
        let mut out = SegmentWriter::new(1);
        b.finish(&mut out).unwrap();
        let seg = SegmentFile::from_vec(out.finish()).unwrap();
        let bytes = seg.section(SECTION).unwrap().unwrap().as_slice().to_vec();
        assert_eq!(EdgeTableReader::from_section(OwnedBytes::new(bytes.clone())).unwrap().n_rows(), 1);
        for cut in [8, 48, bytes.len() / 2, bytes.len() - 8] {
            assert!(EdgeTableReader::from_section(OwnedBytes::new(bytes[..cut].to_vec())).is_err(), "cut at {cut}");
        }
        // The last array's one row (then 4 bytes of padding), past the rows,
        // and the offsets before it: read as no row.
        let mut bad = bytes.clone();
        let n = bad.len();
        bad[n - 8..n - 4].fill(0xff);
        let r = EdgeTableReader::from_section(OwnedBytes::new(bad.clone())).unwrap();
        let k = r.find(KeyRef::Int(2), key_hash(KeyRef::Int(2))).unwrap();
        assert_eq!(r.reaching(k), [u32::MAX]);
        assert_eq!((r.source(u32::MAX), r.target(u32::MAX)), (None, None));
        bad[n - 32..n - 24].fill(0xff);
        let r = EdgeTableReader::from_section(OwnedBytes::new(bad)).unwrap();
        assert!(r.reaching(k).is_empty());
    }
}
