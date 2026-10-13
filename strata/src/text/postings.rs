//! Posting lists with term frequencies and block impacts, in two formats.
//!
//! Every list starts its scoring metadata with the **impact frontier**: the
//! Pareto-optimal `(tf, norm)` pairs of its postings (no other posting has
//! both a larger or equal tf and a smaller or equal norm). BM25 grows with tf
//! and falls with document length, so the best score of a list under any
//! `k1`, `b` and collection statistics is the best score over its frontier:
//! an exact list upper bound for MaxScore and WAND, computed per query from a
//! handful of pairs.
//!
//! Postings are grouped in blocks of [`BLOCK`] = 128 by rank. A block
//! table entry (12 bytes) holds the block's last docID, a byte offset and
//! its **impact**: the largest tf (saturated at 2^24 - 1) and smallest norm
//! of its postings, an upper bound for Block-Max WAND that holds for any
//! scoring parameters.
//!
//! **BP128 lists** (`text.postings = bp128`, and short lists of the Loom
//! format):
//!
//! ```text
//! frontier | block table (only with 2+ blocks) | blocks
//! block: bd: u8 | bf: u8 | docID gaps (bd bits) | tf - 1 (bf bits)
//! ```
//!
//! Full blocks use posting_bench's SIMD-BP128 vertical layout (16 * width
//! bytes); the last, partial block is packed horizontally. A cursor decodes
//! a block's docIDs when it enters it and its frequencies on the first
//! `freq()` in it.
//!
//! **Loom lists** (`text.postings = loom`, lists of at least
//! [`LOOM_MIN_DF`] postings):
//!
//! ```text
//! Loom list (8-byte aligned) | frontier | block table | freq blocks
//! freq block: bf: u8 | tf - 1 (bf bits, horizontal)
//! ```
//!
//! DocIDs are a zero-copy [`Loom`] view; a frequency is read in O(1) from
//! the cursor's rank (`LoomCursor::rank`), without decoding its block.

use posting_bench::bytes::{ByteReader, ByteWriter, OwnedBytes};
use posting_bench::codecs::block::{block_gaps, prefix_sum};
use posting_bench::codecs::bp128;
use posting_bench::codecs::loom::{Loom, LoomCursor};
use posting_bench::codecs::{Cursor, PostingList};
use serde::{Deserialize, Serialize};

use super::codec::{bit_width, get_packed, get_varint, pack_bits, packed_len, put_varint, unpack_bits};
use crate::segment::DocId;

/// Postings per block (frequency blocks, impacts, BP128 blocks).
pub const BLOCK: usize = 128;

/// Sentinel docID of an exhausted cursor.
pub const END: DocId = u32::MAX;

/// Default for the shortest list the Loom format stores as Loom. Shorter
/// lists use BP128 blocks: a serialised Loom list carries about 124 bytes
/// of headers and padding. Measured against 256, 1024 and 2048 (see
/// `notes/text.md`): 512 costs 0.5% over the smallest index and makes AND /
/// OR about a quarter faster than 1024.
pub const LOOM_MIN_DF: u32 = 512;

/// Encoding of the docIDs of the posting lists of a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PostingsFormat {
    Loom,
    Bp128,
}

impl PostingsFormat {
    /// `loom` or `bp128` (any case).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "loom" => Some(PostingsFormat::Loom),
            "bp128" => Some(PostingsFormat::Bp128),
            _ => None,
        }
    }
}

/// A block impact: `max_tf << 8 | min_norm`.
pub type Impact = u32;

/// Impact standing for "the whole list" (single-block lists, whose bound
/// is the exact list bound from the frontier).
pub const LIST_IMPACT: Impact = u32::MAX;

const MAX_IMPACT_TF: u32 = (1 << 24) - 1;

/// The impact of a block whose largest tf and smallest norm are given.
#[inline]
pub fn impact(tf: u32, norm: u8) -> Impact {
    tf.min(MAX_IMPACT_TF) << 8 | norm as u32
}

/// Bytes per block table entry: last docID, offset, impact.
const ENTRY: usize = 12;

#[inline(always)]
fn entry_field(table: &[u8], k: usize, field: usize) -> u32 {
    let at = k * ENTRY + 4 * field;
    u32::from_le_bytes(table[at..at + 4].try_into().unwrap())
}

#[inline(always)]
fn entry_last(table: &[u8], k: usize) -> DocId {
    entry_field(table, k, 0)
}

/// First entry at or after `from` whose last docID is `>= target`
/// (`n` if none): galloping, then binary search.
#[inline]
fn gallop_entries(table: &[u8], n: usize, from: usize, target: DocId) -> usize {
    let mut lo = from;
    let mut step = 1;
    let mut hi = from;
    while hi < n && entry_last(table, hi) < target {
        lo = hi + 1;
        hi += step;
        step *= 2;
    }
    let mut hi = hi.min(n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if entry_last(table, mid) < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// The impact frontier of `(doc, tf)` postings: pairs with increasing tf
/// and norm.
pub fn frontier(docs: &[DocId], freqs: &[u32], norms: &[u8]) -> Vec<(u32, u8)> {
    let mut best = [0u32; 256];
    for (&d, &f) in docs.iter().zip(freqs) {
        let n = norms[d as usize] as usize;
        best[n] = best[n].max(f);
    }
    let mut out = Vec::new();
    let mut top = 0;
    for (n, &tf) in best.iter().enumerate() {
        if tf > top {
            out.push((tf, n as u8));
            top = tf;
        }
    }
    out
}

fn write_frontier(out: &mut Vec<u8>, f: &[(u32, u8)]) {
    put_varint(out, f.len() as u64);
    for &(tf, n) in f {
        put_varint(out, tf as u64);
        out.push(n);
    }
}

fn block_impact(docs: &[DocId], freqs: &[u32], norms: &[u8]) -> Impact {
    let tf = freqs.iter().copied().max().unwrap_or(0);
    let n = docs.iter().map(|&d| norms[d as usize]).min().unwrap_or(0);
    impact(tf, n)
}

/// Byte sizes of a list's parts, for size breakdowns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListSizes {
    pub docs: usize,
    pub freqs: usize,
    /// Frontier, block table, block headers and alignment.
    pub meta: usize,
}

impl std::ops::AddAssign for ListSizes {
    fn add_assign(&mut self, o: Self) {
        self.docs += o.docs;
        self.freqs += o.freqs;
        self.meta += o.meta;
    }
}

/// Appends a list in the BP128 layout; returns its size breakdown.
pub fn write_bp128(out: &mut Vec<u8>, docs: &[DocId], freqs: &[u32], norms: &[u8]) -> ListSizes {
    let start = out.len();
    write_frontier(out, &frontier(docs, freqs, norms));
    let nb = docs.len().div_ceil(BLOCK);
    let table = out.len();
    if nb > 1 {
        out.resize(table + nb * ENTRY, 0);
    }
    let data = out.len();
    let mut sizes = ListSizes::default();
    let mut gaps = [0u32; BLOCK];
    let mut fs = [0u32; BLOCK];
    let mut base = 0u32;
    for (k, (d, f)) in docs.chunks(BLOCK).zip(freqs.chunks(BLOCK)).enumerate() {
        if nb > 1 {
            let e = table + k * ENTRY;
            out[e..e + 4].copy_from_slice(&d[d.len() - 1].to_le_bytes());
            let off = (out.len() - data) as u32;
            out[e + 4..e + 8].copy_from_slice(&off.to_le_bytes());
            out[e + 8..e + 12].copy_from_slice(&block_impact(d, f, norms).to_le_bytes());
        }
        block_gaps(d, base, &mut gaps);
        for (x, &y) in fs.iter_mut().zip(f) {
            *x = y - 1;
        }
        let m = d.len();
        let bd = bit_width(gaps[..m].iter().copied().max().unwrap());
        let bf = bit_width(fs[..m].iter().copied().max().unwrap());
        out.extend([bd as u8, bf as u8]);
        let before = out.len();
        if m == BLOCK {
            bp128::pack(&gaps, bd, out);
        } else {
            pack_bits(&gaps[..m], bd, out);
        }
        let mid = out.len();
        if m == BLOCK {
            bp128::pack(&fs, bf, out);
        } else {
            pack_bits(&fs[..m], bf, out);
        }
        sizes.docs += mid - before;
        sizes.freqs += out.len() - mid;
        base = d[m - 1] + 1;
    }
    sizes.meta = out.len() - start - sizes.docs - sizes.freqs;
    sizes
}

/// Appends a list in the Loom layout (`out.len()` must be a multiple of 8);
/// returns its size breakdown.
pub fn write_loom(out: &mut Vec<u8>, docs: &[DocId], freqs: &[u32], norms: &[u8]) -> ListSizes {
    assert!(out.len().is_multiple_of(8), "Loom lists must start 8-byte aligned");
    let start = out.len();
    let list = Loom::build(docs, END);
    let mut w = ByteWriter::new();
    list.write_to(&mut w);
    out.extend_from_slice(&w.finish());
    let docs_bytes = out.len() - start;
    write_frontier(out, &frontier(docs, freqs, norms));
    let nb = docs.len().div_ceil(BLOCK);
    let table = out.len();
    out.resize(table + nb * ENTRY, 0);
    let data = out.len();
    let mut fs = [0u32; BLOCK];
    let mut freq_bytes = 0;
    for (k, (d, f)) in docs.chunks(BLOCK).zip(freqs.chunks(BLOCK)).enumerate() {
        let e = table + k * ENTRY;
        out[e..e + 4].copy_from_slice(&d[d.len() - 1].to_le_bytes());
        let off = (out.len() - data) as u32;
        out[e + 4..e + 8].copy_from_slice(&off.to_le_bytes());
        out[e + 8..e + 12].copy_from_slice(&block_impact(d, f, norms).to_le_bytes());
        let m = d.len();
        for (x, &y) in fs.iter_mut().zip(f) {
            *x = y - 1;
        }
        let bf = bit_width(fs[..m].iter().copied().max().unwrap());
        out.push(bf as u8);
        let before = out.len();
        pack_bits(&fs[..m], bf, out);
        freq_bytes += out.len() - before;
    }
    ListSizes { docs: docs_bytes, freqs: freq_bytes, meta: out.len() - start - docs_bytes - freq_bytes }
}

/// Reads a frontier at `*pos`.
pub fn read_frontier(data: &[u8], pos: &mut usize) -> Vec<(u32, u8)> {
    let n = get_varint(data, pos) as usize;
    (0..n)
        .map(|_| {
            let tf = get_varint(data, pos) as u32;
            let norm = data[*pos];
            *pos += 1;
            (tf, norm)
        })
        .collect()
}

fn skip_frontier(data: &[u8], pos: &mut usize) {
    let n = get_varint(data, pos);
    for _ in 0..n {
        get_varint(data, pos);
        *pos += 1;
    }
}

/// Cursor over a BP128 list.
pub struct BpCursor<'a> {
    /// Block table (empty for single-block lists).
    table: &'a [u8],
    /// From the first block to the end of the section (padded).
    blocks: &'a [u8],
    n: u32,
    nb: u32,
    block: u32,
    pos: u32,
    len: u32,
    cur: DocId,
    /// Byte offset of the current block in `blocks`.
    off: usize,
    freqs_ready: bool,
    /// Shallow block for impact queries (never behind `block`).
    shallow: u32,
    docs: [u32; BLOCK],
    freqs: [u32; BLOCK],
}

impl<'a> BpCursor<'a> {
    /// `data` starts at the list and runs to the end of the section; the
    /// list's frontier ends at `pos`.
    pub fn new(data: &'a [u8], pos: usize, n: u32) -> Self {
        let nb = (n as usize).div_ceil(BLOCK) as u32;
        let tlen = if nb > 1 { nb as usize * ENTRY } else { 0 };
        let mut c = BpCursor {
            table: &data[pos..pos + tlen],
            blocks: &data[pos + tlen..],
            n,
            nb,
            block: 0,
            pos: 0,
            len: 0,
            cur: END,
            off: 0,
            freqs_ready: false,
            shallow: 0,
            docs: [0; BLOCK],
            freqs: [0; BLOCK],
        };
        if n > 0 {
            c.load(0);
        }
        c
    }

    fn load(&mut self, k: u32) {
        let k_us = k as usize;
        self.block = k;
        self.shallow = self.shallow.max(k);
        self.off = if self.nb > 1 { entry_field(self.table, k_us, 1) as usize } else { 0 };
        self.len = if k + 1 < self.nb { BLOCK as u32 } else { self.n - k * BLOCK as u32 };
        let base = if k == 0 { 0 } else { entry_last(self.table, k_us - 1) + 1 };
        let bd = self.blocks[self.off] as u32;
        let data = &self.blocks[self.off + 2..];
        if self.len as usize == BLOCK {
            bp128::unpack(data, bd, &mut self.docs);
        } else {
            unpack_bits(data, bd, 0, &mut self.docs[..self.len as usize]);
        }
        prefix_sum(&mut self.docs, self.len as usize, base);
        self.freqs_ready = false;
        self.pos = 0;
        self.cur = self.docs[0];
    }

    #[inline]
    pub fn doc(&self) -> DocId {
        self.cur
    }

    #[inline]
    pub fn next(&mut self) -> DocId {
        if self.cur == END {
            return END;
        }
        self.pos += 1;
        if self.pos < self.len {
            self.cur = self.docs[self.pos as usize];
        } else if self.block + 1 < self.nb {
            self.load(self.block + 1);
        } else {
            self.cur = END;
        }
        self.cur
    }

    #[inline]
    pub fn seek(&mut self, target: DocId) -> DocId {
        if target <= self.cur {
            return self.cur;
        }
        if target > self.docs[self.len as usize - 1] {
            if self.block + 1 >= self.nb {
                self.cur = END;
                return END;
            }
            let k = gallop_entries(self.table, self.nb as usize, self.block as usize + 1, target);
            if k >= self.nb as usize {
                self.cur = END;
                return END;
            }
            self.load(k as u32);
        }
        let p = self.pos as usize;
        self.pos += self.docs[p..self.len as usize].partition_point(|&d| d < target) as u32;
        self.cur = self.docs[self.pos as usize];
        self.cur
    }

    fn decode_freqs(&mut self) {
        let bd = self.blocks[self.off] as u32;
        let bf = self.blocks[self.off + 1] as u32;
        let m = self.len as usize;
        let at = self.off + 2 + if m == BLOCK { 16 * bd as usize } else { packed_len(m, bd) };
        let data = &self.blocks[at..];
        if m == BLOCK {
            bp128::unpack(data, bf, &mut self.freqs);
            for f in self.freqs.iter_mut() {
                *f += 1;
            }
        } else {
            unpack_bits(data, bf, 1, &mut self.freqs[..m]);
        }
        self.freqs_ready = true;
    }

    #[inline]
    pub fn freq(&mut self) -> u32 {
        if !self.freqs_ready {
            self.decode_freqs();
        }
        self.freqs[self.pos as usize]
    }

    /// Frequency of the posting of rank `r`, which must be in the current
    /// block.
    #[inline]
    pub fn freq_at(&mut self, r: usize) -> u32 {
        if !self.freqs_ready {
            self.decode_freqs();
        }
        self.freqs[r - self.block as usize * BLOCK]
    }

    #[inline]
    pub fn rank(&self) -> usize {
        if self.cur == END {
            self.n as usize
        } else {
            self.block as usize * BLOCK + self.pos as usize
        }
    }

    /// Last docID and impact of the block holding the first posting
    /// `>= target` ([`END`] past the list). Targets must not decrease.
    #[inline]
    pub fn block_bound(&mut self, target: DocId) -> (DocId, Impact) {
        if self.nb <= 1 {
            let last = if self.n == 0 { END } else { self.docs[self.len as usize - 1] };
            return if target > last { (END, 0) } else { (last, LIST_IMPACT) };
        }
        let mut k = self.shallow.max(self.block) as usize;
        if entry_last(self.table, k) < target {
            k = gallop_entries(self.table, self.nb as usize, k + 1, target);
            if k >= self.nb as usize {
                self.shallow = self.nb - 1;
                return (END, 0);
            }
            self.shallow = k as u32;
        }
        (entry_last(self.table, k), entry_field(self.table, k, 2))
    }
}

/// Cursor over a Loom list.
pub struct LoomListCursor<'a> {
    cur: LoomCursor<Loom>,
    table: &'a [u8],
    /// From the first frequency block to the end of the section (padded).
    freqs: &'a [u8],
    nb: u32,
    shallow: u32,
}

impl<'a> LoomListCursor<'a> {
    /// A cursor over `list` (see [`open_loom`]); `data` starts at the list's
    /// bytes and runs to the end of the section, and its frontier ends at
    /// `pos`.
    pub fn new(list: Loom, data: &'a [u8], pos: usize) -> Self {
        let n = list.len();
        let nb = n.div_ceil(BLOCK);
        LoomListCursor {
            cur: list.into_cursor(),
            table: &data[pos..pos + nb * ENTRY],
            freqs: &data[pos + nb * ENTRY..],
            nb: nb as u32,
            shallow: 0,
        }
    }

    #[inline]
    pub fn doc(&self) -> DocId {
        self.cur.docid()
    }

    #[inline]
    pub fn next(&mut self) -> DocId {
        self.cur.next()
    }

    #[inline]
    pub fn seek(&mut self, target: DocId) -> DocId {
        self.cur.next_geq(target)
    }

    #[inline]
    pub fn rank(&mut self) -> usize {
        self.cur.rank()
    }

    #[inline]
    pub fn freq(&mut self) -> u32 {
        let r = self.cur.rank();
        self.freq_at(r)
    }

    /// Frequency of the posting of rank `r` (any rank), in O(1).
    #[inline]
    pub fn freq_at(&self, r: usize) -> u32 {
        let off = entry_field(self.table, r / BLOCK, 1) as usize;
        let bf = self.freqs[off] as u32;
        1 + get_packed(&self.freqs[off + 1..], r % BLOCK, bf)
    }

    /// As [`BpCursor::block_bound`].
    #[inline]
    pub fn block_bound(&mut self, target: DocId) -> (DocId, Impact) {
        let mut k = self.shallow as usize;
        if entry_last(self.table, k) < target {
            k = gallop_entries(self.table, self.nb as usize, k + 1, target);
            if k >= self.nb as usize {
                self.shallow = self.nb - 1;
                return (END, 0);
            }
            self.shallow = k as u32;
        }
        (entry_last(self.table, k), entry_field(self.table, k, 2))
    }
}

/// The Loom view of the list at `start..end` of the section, and the
/// offset (relative to `start`) of what follows it.
pub fn open_loom(section: &OwnedBytes, start: usize, end: usize) -> (Loom, usize) {
    let mut r = ByteReader::new(section.slice(start..end));
    let list = Loom::read_from(&mut r).expect("Loom list written by the builder");
    (list, r.position())
}

/// Sizes of a list's parts, read back from its bytes.
pub fn list_sizes(section: &OwnedBytes, start: usize, end: usize, n: u32, loom: bool) -> ListSizes {
    let data = &section[start..];
    let nb = (n as usize).div_ceil(BLOCK);
    if loom {
        let (_, mut pos) = open_loom(section, start, end);
        let docs = pos;
        skip_frontier(data, &mut pos);
        let fstart = pos + nb * ENTRY;
        let mut freqs = 0;
        for k in 0..nb {
            let off = entry_field(&data[pos..], k, 1) as usize;
            let m = if k + 1 < nb { BLOCK } else { n as usize - k * BLOCK };
            freqs += packed_len(m, data[fstart + off] as u32);
        }
        ListSizes { docs, freqs, meta: end - start - docs - freqs }
    } else {
        let mut pos = 0;
        skip_frontier(data, &mut pos);
        let tlen = if nb > 1 { nb * ENTRY } else { 0 };
        let blocks = &data[pos + tlen..];
        let (mut docs, mut freqs, mut off) = (0, 0, 0);
        for k in 0..nb {
            let m = if k + 1 < nb { BLOCK } else { n as usize - k * BLOCK };
            let (bd, bf) = (blocks[off] as u32, blocks[off + 1] as u32);
            let (dl, fl) = if m == BLOCK { (16 * bd as usize, 16 * bf as usize) } else { (packed_len(m, bd), packed_len(m, bf)) };
            docs += dl;
            freqs += fl;
            off += 2 + dl + fl;
        }
        ListSizes { docs, freqs, meta: end - start - docs - freqs }
    }
}
