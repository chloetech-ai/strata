//! Doc store: the stored, non-vector fields of every document, as rows in
//! LZ4-compressed blocks.
//!
//! Rows are appended to a block until it holds `forward.store_block` bytes
//! (default 16 KiB) uncompressed, then the block is compressed on its own.
//! A block is `n_rows`, each row's length (LEB128 varints), then the rows;
//! a row is a presence bitmap over the stored fields followed by the
//! present values: strings as length + UTF-8, integers zigzag varints,
//! floats 8 bytes little-endian (a FLOAT field given an integer stores it
//! as a float). Fetching a document decompresses one block.
//!
//! Sections: `store.index` (the stored fields, by id
//! ([`crate::schema::Field::id`]), and their types, then per block its
//! first docID, its byte offset in `store.data` and its uncompressed size)
//! and `store.data` (the compressed blocks). A reader finds each field of
//! its schema among the index's by id, so the columns a row holds need not
//! be the schema's in its order, and a column whose field the schema no
//! longer has is passed over. (A database written before field ids wrote
//! positions there, which its schema reads as ids.)

use crate::postings::bytes::{ByteReader, ByteWriter, OwnedBytes, Slice};

use crate::error::{corrupt, Error, Result};
use crate::schema::{FieldType, Schema};
use crate::segment::build::BuildOptions;
use crate::segment::{DocId, SegmentFile, SegmentWriter, Spill, SpillStream};
use crate::value::{Document, Value};

/// Default uncompressed block size (`forward.store_block`).
pub const DEFAULT_BLOCK: usize = 16 * 1024;

const VERSION: u64 = 1;

/// How a stored field is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Str = 0,
    Int = 1,
    Float = 2,
    /// Multi-valued fields: a count, then each value.
    Strs = 3,
    Ints = 4,
}

/// The position of a doc store column whose field the schema does not
/// have.
const GONE: usize = usize::MAX;

impl Kind {
    fn from_u8(k: u8) -> Option<Kind> {
        Some(match k {
            0 => Kind::Str,
            1 => Kind::Int,
            2 => Kind::Float,
            3 => Kind::Strs,
            4 => Kind::Ints,
            _ => return None,
        })
    }

    fn of(ty: &FieldType) -> Option<Kind> {
        match ty {
            FieldType::Text { .. } | FieldType::Keyword | FieldType::Edges | FieldType::Sparse | FieldType::Json => Some(Kind::Str),
            FieldType::Int => Some(Kind::Int),
            FieldType::Float => Some(Kind::Float),
            FieldType::Vector { .. } => None,
        }
    }
}

/// The stored, non-vector fields of `schema`: (position, kind).
fn stored_fields(schema: &Schema) -> Vec<(usize, Kind)> {
    schema
        .fields
        .iter()
        .enumerate()
        .filter(|(_, f)| f.stored)
        .filter_map(|(i, f)| {
            Kind::of(&f.ty).map(|k| match (f.multi, k) {
                (true, Kind::Str) => (i, Kind::Strs),
                (true, Kind::Int) => (i, Kind::Ints),
                (_, k) => (i, k),
            })
        })
        .collect()
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Reads a varint at `*pos`, advancing it.
#[inline]
fn get_varint(b: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let x = *b.get(*pos)?;
        *pos += 1;
        v |= ((x & 0x7f) as u64) << shift;
        if x < 0x80 {
            return Some(v);
        }
    }
    None
}

fn encode_row(fields: &[(usize, Kind)], doc: &Document, out: &mut Vec<u8>) {
    let start = out.len();
    out.resize(start + fields.len().div_ceil(8), 0);
    for (j, &(i, kind)) in fields.iter().enumerate() {
        let v = &doc[i];
        if matches!(v, Value::Null) {
            continue;
        }
        out[start + j / 8] |= 1 << (j % 8);
        match (kind, v) {
            (Kind::Str, Value::Str(s)) => {
                put_varint(out, s.len() as u64);
                out.extend_from_slice(s.as_bytes());
            }
            (Kind::Int, Value::Int(x)) => put_varint(out, ((x << 1) ^ (x >> 63)) as u64),
            (Kind::Float, v) => out.extend_from_slice(&v.as_f64().unwrap_or(f64::NAN).to_le_bytes()),
            (Kind::Strs, v) => {
                put_varint(out, v.items().len() as u64);
                for s in v.items().iter().filter_map(|x| x.as_str()) {
                    put_varint(out, s.len() as u64);
                    out.extend_from_slice(s.as_bytes());
                }
            }
            (Kind::Ints, v) => {
                put_varint(out, v.items().len() as u64);
                for x in v.items().iter().filter_map(|x| x.as_i64()) {
                    put_varint(out, ((x << 1) ^ (x >> 63)) as u64);
                }
            }
            // `validate` rejects other combinations before building.
            _ => unreachable!("value of the wrong type for a stored field"),
        }
    }
}

/// Writes `store.index` and `store.data` for `docs`.
pub(crate) fn build(schema: &Schema, docs: &[Document], opts: &BuildOptions, out: &mut SegmentWriter) -> Result<()> {
    let mut b = StoreBuilder::new(schema, opts)?;
    for doc in docs {
        b.add(doc);
    }
    b.finish(out);
    Ok(())
}

/// The doc store of documents added one at a time, in docID order.
pub struct StoreBuilder {
    target: usize,
    fields: Vec<(usize, Kind)>,
    /// Each stored field's id, as `store.index` names it.
    ids: Vec<u32>,
    blocks: Blocks,
    /// Documents added, and the first of the open block.
    n: usize,
    block_start: usize,
}

impl StoreBuilder {
    pub fn new(schema: &Schema, opts: &BuildOptions) -> Result<Self> {
        let fields = stored_fields(schema);
        Ok(StoreBuilder {
            target: super::usize_option(opts, "forward.store_block", DEFAULT_BLOCK)?,
            ids: fields.iter().map(|&(i, _)| schema.id_of(i)).collect(),
            fields,
            blocks: Blocks { first: Vec::new(), offs: vec![0], raw_len: Vec::new(), data: Data::Mem(Vec::new()), rows: Vec::new(), lens: Vec::new() },
            n: 0,
            block_start: 0,
        })
    }

    pub fn add(&mut self, doc: &Document) {
        let b = &mut self.blocks;
        let before = b.rows.len();
        encode_row(&self.fields, doc, &mut b.rows);
        b.lens.push(b.rows.len() - before);
        self.n += 1;
        if b.rows.len() >= self.target {
            b.flush(self.block_start);
            self.block_start = self.n;
        }
    }

    /// Writes the compressed blocks to `spill` as they are made, instead of
    /// holding them until [`finish`](Self::finish); called before the
    /// first document.
    pub fn spill_to(&mut self, spill: &std::sync::Arc<Spill>) -> Result<()> {
        if self.n == 0 {
            self.blocks.data = Data::Spill(spill.stream()?);
        }
        Ok(())
    }

    /// Writes `store.index` and `store.data`.
    pub fn finish(mut self, out: &mut SegmentWriter) {
        let b = &mut self.blocks;
        if !b.lens.is_empty() {
            b.flush(self.block_start);
        }
        let mut w = ByteWriter::new();
        w.u64(VERSION);
        w.u64(self.n as u64);
        w.array(&self.ids);
        w.array(&self.fields.iter().map(|&(_, k)| k as u8).collect::<Vec<u8>>());
        w.array(&b.first);
        w.array(&b.offs);
        w.array(&b.raw_len);
        out.put("store.index", w.finish());
        match std::mem::replace(&mut b.data, Data::Mem(Vec::new())) {
            Data::Mem(v) => out.put("store.data", v),
            Data::Spill(s) => match s.finish() {
                Ok((at, crc)) => out.put_spilled("store.data", at, crc),
                Err(e) => out.fail(e),
            },
        }
    }
}

/// The compressed blocks: in memory, or written out as they are made.
enum Data {
    Mem(Vec<u8>),
    Spill(SpillStream),
}

impl Data {
    fn len(&self) -> usize {
        match self {
            Data::Mem(v) => v.len(),
            Data::Spill(s) => s.len(),
        }
    }

    fn extend_from_slice(&mut self, bytes: &[u8]) {
        match self {
            Data::Mem(v) => v.extend_from_slice(bytes),
            Data::Spill(s) => s.extend_from_slice(bytes),
        }
    }
}

/// Compressed blocks under construction.
struct Blocks {
    first: Vec<u32>,
    offs: Vec<u64>,
    raw_len: Vec<u32>,
    data: Data,
    /// Rows of the open block and their lengths.
    rows: Vec<u8>,
    lens: Vec<usize>,
}

impl Blocks {
    /// Compresses the open block, whose first document is `first_doc`.
    fn flush(&mut self, first_doc: usize) {
        let mut block = Vec::with_capacity(self.rows.len() + 2 * self.lens.len() + 4);
        put_varint(&mut block, self.lens.len() as u64);
        for &l in &self.lens {
            put_varint(&mut block, l as u64);
        }
        block.extend_from_slice(&self.rows);
        self.data.extend_from_slice(&lz4_flex::block::compress(&block));
        self.first.push(first_doc as u32);
        self.offs.push(self.data.len() as u64);
        self.raw_len.push(block.len() as u32);
        self.rows.clear();
        self.lens.clear();
    }
}

/// The last block [`DocStore::get_fields_cached`] decompressed, and where
/// each of its rows lies.
#[derive(Debug, Default)]
pub struct StoreCache {
    block: Option<usize>,
    data: Vec<u8>,
    rows: Vec<(usize, usize)>,
}

/// A zero-copy view of a segment's doc store.
#[derive(Clone, Debug)]
pub struct DocStore {
    n: usize,
    n_fields: usize,
    /// Each column of a row: the schema position of its field
    /// ([`GONE`] for a field the schema no longer has), and its kind.
    fields: Vec<(usize, Kind)>,
    /// The stored fields of the schema that the segment lacks, added to its
    /// table after it was written, and the value its rows hold
    /// ([`crate::schema::Field::fill_value`]).
    fills: Vec<(usize, Value)>,
    first: Slice<u32>,
    offs: Slice<u64>,
    raw_len: Slice<u32>,
    data: OwnedBytes,
}

impl DocStore {
    /// Opens the doc store, reading it as `schema` names its fields: each
    /// stored field of `schema` must be a column of it, by id, of the
    /// field's type.
    pub fn open(seg: &SegmentFile, schema: &Schema) -> Result<Self> {
        let mut r = ByteReader::new(seg.require("store.index")?);
        if r.u64().map_err(corrupt)? != VERSION {
            return Err(corrupt("bad doc store version"));
        }
        let n = r.u64().map_err(corrupt)? as usize;
        let idx: Slice<u32> = r.array().map_err(corrupt)?;
        let kinds: Slice<u8> = r.array().map_err(corrupt)?;
        let first: Slice<u32> = r.array().map_err(corrupt)?;
        let offs: Slice<u64> = r.array().map_err(corrupt)?;
        let raw_len: Slice<u32> = r.array().map_err(corrupt)?;
        let data = seg.require("store.data")?;
        let mismatch = || Error::Schema("doc store fields do not match the schema".into());
        if idx.len() != kinds.len() {
            return Err(corrupt("bad doc store index"));
        }
        let mut fields = Vec::with_capacity(idx.len());
        for (&id, &k) in idx.iter().zip(kinds.iter()) {
            let kind = Kind::from_u8(k).ok_or_else(|| corrupt("bad doc store index"))?;
            fields.push((schema.position_of(id).unwrap_or(GONE), kind));
        }
        let mut seen: Vec<usize> = fields.iter().map(|&(i, _)| i).filter(|&i| i != GONE).collect();
        seen.sort_unstable();
        if seen.windows(2).any(|w| w[0] == w[1]) {
            return Err(corrupt("doc store index names a field twice"));
        }
        // A stored field the segment was written without (added to its
        // table since) reads the value its rows hold then.
        let written = seg.field_ids()?;
        let mut fills = Vec::new();
        for (i, k) in stored_fields(schema) {
            match fields.iter().find(|&&(j, _)| j == i) {
                Some(&(_, kj)) if kj == k => {}
                None if written.as_ref().is_some_and(|ids| !ids.contains(&schema.fields[i].id)) => fills.push((i, schema.fields[i].fill_value()?)),
                _ => return Err(mismatch()),
            }
        }
        // A column of a field the schema no longer has is read past; one of
        // a field it has but does not store does not fit it.
        if fields.iter().any(|&(i, _)| i != GONE && !schema.fields[i].stored) {
            return Err(mismatch());
        }
        if n != seg.n_docs()
            || first.len() != raw_len.len()
            || offs.len() != first.len() + 1
            || offs.last().is_some_and(|&o| o as usize != data.len())
            || first.first().is_some_and(|&f| f != 0)
        {
            return Err(corrupt("bad doc store index"));
        }
        Ok(DocStore { n, n_fields: schema.fields.len(), fields, fills, first, offs, raw_len, data })
    }

    /// Number of documents.
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Number of compressed blocks.
    pub fn n_blocks(&self) -> usize {
        self.first.len()
    }

    /// Compressed and uncompressed bytes of all blocks.
    pub fn byte_sizes(&self) -> (usize, usize) {
        (self.data.len(), self.raw_len.iter().map(|&l| l as usize).sum())
    }

    /// Document `d` with every schema field: stored values, `Null` for
    /// fields that are not stored (and vectors).
    ///
    /// # Panics
    /// If `d` is out of range or the block is corrupt.
    pub fn get(&self, d: DocId) -> Document {
        let mut doc = vec![Value::Null; self.n_fields];
        let want: Vec<usize> = self.fields.iter().map(|&(i, _)| i).filter(|&i| i != GONE).collect();
        for (i, v) in want.iter().zip(self.get_fields(d, &want)) {
            doc[*i] = v;
        }
        for (i, v) in &self.fills {
            doc[*i] = v.clone();
        }
        doc
    }

    /// The values of schema fields `fields` of document `d`, in that order
    /// (`Null` for fields that are not stored).
    ///
    /// # Panics
    /// As for [`DocStore::get`].
    pub fn get_fields(&self, d: DocId, fields: &[usize]) -> Vec<Value> {
        assert!((d as usize) < self.n, "document {d} out of range");
        let b = self.first.partition_point(|&f| f <= d) - 1;
        let block = self.decompress(b);
        self.decode(&block, (d - self.first[b]) as usize, fields).unwrap_or_else(|| panic!("corrupt doc store block {b}"))
    }

    /// [`Self::get_fields`], keeping the decompressed block in `cache` for
    /// the next call: documents read in docID order decompress each block
    /// once, where `get_fields` decompresses a block a document (a block
    /// holds hundreds of small rows).
    ///
    /// # Panics
    /// If `d` is out of range or its block is corrupt.
    pub fn get_fields_cached(&self, d: DocId, fields: &[usize], cache: &mut StoreCache) -> Vec<Value> {
        assert!((d as usize) < self.n, "document {d} out of range");
        let b = self.first.partition_point(|&f| f <= d) - 1;
        if cache.block != Some(b) {
            cache.block = None;
            cache.data = self.decompress(b);
            cache.rows.clear();
            let parsed = (|| {
                let mut pos = 0;
                let n_rows = get_varint(&cache.data, &mut pos)? as usize;
                let lens = (0..n_rows).map(|_| get_varint(&cache.data, &mut pos)).collect::<Option<Vec<u64>>>()?;
                for l in lens {
                    let end = pos.checked_add(l as usize).filter(|&e| e <= cache.data.len())?;
                    cache.rows.push((pos, end));
                    pos = end;
                }
                Some(())
            })();
            parsed.unwrap_or_else(|| panic!("corrupt doc store block {b}"));
            cache.block = Some(b);
        }
        let row = cache.rows.get((d - self.first[b]) as usize).and_then(|&(a, z)| self.decode_row(&cache.data[a..z], fields));
        row.unwrap_or_else(|| panic!("corrupt doc store block {b}"))
    }

    fn decompress(&self, b: usize) -> Vec<u8> {
        let src = &self.data[self.offs[b] as usize..self.offs[b + 1] as usize];
        let mut out = vec![0u8; self.raw_len[b] as usize];
        match lz4_flex::block::decompress_into(src, &mut out) {
            Ok(n) if n == out.len() => out,
            _ => panic!("corrupt doc store block {b}"),
        }
    }

    /// Row `row` of a decompressed block, projected on `fields`.
    fn decode(&self, block: &[u8], row: usize, fields: &[usize]) -> Option<Vec<Value>> {
        let mut pos = 0;
        let n_rows = get_varint(block, &mut pos)? as usize;
        if row >= n_rows {
            return None;
        }
        let mut start = 0usize;
        let mut len = 0usize;
        for i in 0..=row {
            let l = get_varint(block, &mut pos)? as usize;
            if i < row {
                start += l;
            } else {
                len = l;
            }
        }
        for _ in row + 1..n_rows {
            get_varint(block, &mut pos)?;
        }
        self.decode_row(block.get(pos + start..pos + start + len)?, fields)
    }

    /// Field `field` of every document, in docID order, decompressing each
    /// block once.
    ///
    /// # Panics
    /// If a block is corrupt.
    pub fn field_values(&self, field: usize) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.n);
        for b in 0..self.n_blocks() {
            let block = self.decompress(b);
            let rows = (|| {
                let mut pos = 0;
                let n_rows = get_varint(&block, &mut pos)? as usize;
                let lens = (0..n_rows).map(|_| get_varint(&block, &mut pos)).collect::<Option<Vec<u64>>>()?;
                for l in lens {
                    let row = block.get(pos..pos + l as usize)?;
                    pos += l as usize;
                    out.push(self.decode_row(row, &[field])?.pop()?);
                }
                Some(())
            })();
            rows.unwrap_or_else(|| panic!("corrupt doc store block {b}"));
        }
        assert_eq!(out.len(), self.n, "corrupt doc store: blocks hold {} rows for {} documents", out.len(), self.n);
        out
    }

    /// A row, projected on `fields`.
    fn decode_row(&self, r: &[u8], fields: &[usize]) -> Option<Vec<Value>> {
        // Slot of each stored field in the row, if requested.
        let mut out = vec![Value::Null; fields.len()];
        let mut p = self.fields.len().div_ceil(8);
        let bitmap = r.get(..p)?;
        for (j, &(i, kind)) in self.fields.iter().enumerate() {
            if bitmap[j / 8] >> (j % 8) & 1 == 0 {
                continue;
            }
            let wanted = fields.iter().position(|&f| f == i);
            match kind {
                Kind::Str => {
                    let l = get_varint(r, &mut p)? as usize;
                    let s = r.get(p..p + l)?;
                    p += l;
                    if let Some(k) = wanted {
                        out[k] = Value::Str(String::from_utf8(s.to_vec()).ok()?);
                    }
                }
                Kind::Int => {
                    let z = get_varint(r, &mut p)?;
                    if let Some(k) = wanted {
                        out[k] = Value::Int((z >> 1) as i64 ^ -((z & 1) as i64));
                    }
                }
                Kind::Float => {
                    let x = f64::from_le_bytes(r.get(p..p + 8)?.try_into().ok()?);
                    p += 8;
                    if let Some(k) = wanted {
                        out[k] = Value::Float(x);
                    }
                }
                Kind::Strs | Kind::Ints => {
                    let n = get_varint(r, &mut p)? as usize;
                    let mut xs = Vec::with_capacity(n.min(1024));
                    for _ in 0..n {
                        if kind == Kind::Strs {
                            let l = get_varint(r, &mut p)? as usize;
                            xs.push(Value::Str(String::from_utf8(r.get(p..p + l)?.to_vec()).ok()?));
                            p += l;
                        } else {
                            let z = get_varint(r, &mut p)?;
                            xs.push(Value::Int((z >> 1) as i64 ^ -((z & 1) as i64)));
                        }
                    }
                    if let Some(k) = wanted {
                        out[k] = Value::List(xs);
                    }
                }
            }
            // A field requested twice gets the value in every slot.
            if let Some(k) = wanted {
                for k2 in k + 1..fields.len() {
                    if fields[k2] == i {
                        out[k2] = out[k].clone();
                    }
                }
            }
        }
        for (k, f) in fields.iter().enumerate() {
            if let Some((_, v)) = self.fills.iter().find(|(i, _)| i == f) {
                out[k] = v.clone();
            }
        }
        Some(out)
    }

    /// Whether the index arrays are views into the segment.
    pub fn is_view(&self) -> bool {
        matches!(self.first, Slice::Shared { .. }) && matches!(self.offs, Slice::Shared { .. }) && matches!(self.raw_len, Slice::Shared { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_and_zigzag() {
        let mut b = Vec::new();
        let vals = [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX];
        for &v in &vals {
            put_varint(&mut b, v);
        }
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(get_varint(&b, &mut pos), Some(v));
        }
        assert_eq!(get_varint(&b, &mut pos), None);
        for x in [0i64, -1, 1, i64::MIN, i64::MAX, -12345] {
            let z = ((x << 1) ^ (x >> 63)) as u64;
            assert_eq!((z >> 1) as i64 ^ -((z & 1) as i64), x);
        }
    }
}
