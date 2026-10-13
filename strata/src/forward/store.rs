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
//! Sections: `store.index` (the stored fields and their types, then per
//! block its first docID, its byte offset in `store.data` and its
//! uncompressed size) and `store.data` (the compressed blocks).

use posting_bench::bytes::{ByteReader, ByteWriter, OwnedBytes, Slice};

use crate::error::{corrupt, Error, Result};
use crate::schema::{FieldType, Schema};
use crate::segment::build::BuildOptions;
use crate::segment::{DocId, SegmentFile, SegmentWriter};
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
}

impl Kind {
    fn of(ty: &FieldType) -> Option<Kind> {
        match ty {
            FieldType::Text { .. } | FieldType::Keyword | FieldType::Edges => Some(Kind::Str),
            FieldType::Int => Some(Kind::Int),
            FieldType::Float => Some(Kind::Float),
            FieldType::Vector { .. } => None,
        }
    }
}

/// The stored, non-vector fields of `schema`: (field index, kind).
fn stored_fields(schema: &Schema) -> Vec<(usize, Kind)> {
    schema.fields.iter().enumerate().filter(|(_, f)| f.stored).filter_map(|(i, f)| Kind::of(&f.ty).map(|k| (i, k))).collect()
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
            // `validate` rejects other combinations before building.
            _ => unreachable!("value of the wrong type for a stored field"),
        }
    }
}

/// Writes `store.index` and `store.data` for `docs`.
pub(crate) fn build(schema: &Schema, docs: &[Document], opts: &BuildOptions, out: &mut SegmentWriter) -> Result<()> {
    let target = super::usize_option(opts, "forward.store_block", DEFAULT_BLOCK)?;
    let fields = stored_fields(schema);
    let mut b = Blocks { first: Vec::new(), offs: vec![0], raw_len: Vec::new(), data: Vec::new(), rows: Vec::new(), lens: Vec::new() };
    let mut block_start = 0;
    for (d, doc) in docs.iter().enumerate() {
        let before = b.rows.len();
        encode_row(&fields, doc, &mut b.rows);
        b.lens.push(b.rows.len() - before);
        if b.rows.len() >= target {
            b.flush(block_start);
            block_start = d + 1;
        }
    }
    if !b.lens.is_empty() {
        b.flush(block_start);
    }
    let mut w = ByteWriter::new();
    w.u64(VERSION);
    w.u64(docs.len() as u64);
    w.array(&fields.iter().map(|&(i, _)| i as u32).collect::<Vec<u32>>());
    w.array(&fields.iter().map(|&(_, k)| k as u8).collect::<Vec<u8>>());
    w.array(&b.first);
    w.array(&b.offs);
    w.array(&b.raw_len);
    out.put("store.index", w.finish());
    out.put("store.data", b.data);
    Ok(())
}

/// Compressed blocks under construction.
struct Blocks {
    first: Vec<u32>,
    offs: Vec<u64>,
    raw_len: Vec<u32>,
    data: Vec<u8>,
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

/// A zero-copy view of a segment's doc store.
#[derive(Clone, Debug)]
pub struct DocStore {
    n: usize,
    n_fields: usize,
    fields: Vec<(usize, Kind)>,
    first: Slice<u32>,
    offs: Slice<u64>,
    raw_len: Slice<u32>,
    data: OwnedBytes,
}

impl DocStore {
    /// Opens the doc store; `schema` must be the one the segment was built
    /// with (its stored fields and their types are checked).
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
        let fields = stored_fields(schema);
        if fields.len() != idx.len() || fields.iter().zip(idx.iter().zip(kinds.iter())).any(|(&(i, k), (&j, &kj))| i != j as usize || k as u8 != kj) {
            return Err(Error::Schema("doc store fields do not match the schema".into()));
        }
        if n != seg.n_docs()
            || first.len() != raw_len.len()
            || offs.len() != first.len() + 1
            || offs.last().is_some_and(|&o| o as usize != data.len())
            || first.first().is_some_and(|&f| f != 0)
        {
            return Err(corrupt("bad doc store index"));
        }
        Ok(DocStore { n, n_fields: schema.fields.len(), fields, first, offs, raw_len, data })
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
        let want: Vec<usize> = self.fields.iter().map(|&(i, _)| i).collect();
        for (i, v) in want.iter().zip(self.get_fields(d, &want)) {
            doc[*i] = v;
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
        let r = block.get(pos + start..pos + start + len)?;
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
