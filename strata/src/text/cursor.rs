//! The public posting cursor (both formats) and the positions stream.
//!
//! Positions of a term are stored per posting, in posting order, as varint
//! deltas (the first one absolute). Lists of more than one block start with
//! the byte offset of every block's positions (`u32` each), so a phrase
//! query reading the positions of a posting skips at most the 127
//! postings before it in its block, using their frequencies.

use posting_bench::bytes::OwnedBytes;
use posting_bench::codecs::PostingList;

use super::codec::{get_varint, put_varint};
use super::postings::{open_loom, read_frontier, BpCursor, Impact, LoomListCursor, BLOCK};
use crate::segment::DocId;

pub use super::postings::END;

/// Appends the positions of one term: `positions` holds every posting's
/// positions (increasing within a posting), `freqs` how many each has.
pub fn write_positions(out: &mut Vec<u8>, freqs: &[u32], positions: &[u32]) {
    let nb = freqs.len().div_ceil(BLOCK);
    let index = out.len();
    if nb > 1 {
        out.resize(index + 4 * nb, 0);
    }
    let data = out.len();
    let mut at = 0;
    for (r, &f) in freqs.iter().enumerate() {
        if nb > 1 && r.is_multiple_of(BLOCK) {
            let k = index + 4 * (r / BLOCK);
            let off = (out.len() - data) as u32;
            out[k..k + 4].copy_from_slice(&off.to_le_bytes());
        }
        let mut prev = 0;
        for &p in &positions[at..at + f as usize] {
            put_varint(out, (p - prev) as u64);
            prev = p;
        }
        at += f as usize;
    }
}

/// Reads one term's positions; remembers where it stopped so that reading
/// postings in order costs one pass over the stream.
struct PosReader<'a> {
    /// Block offsets (empty for single-block lists).
    index: &'a [u8],
    data: &'a [u8],
    /// Rank of the posting whose positions start at `off`.
    next_rank: usize,
    off: usize,
}

impl<'a> PosReader<'a> {
    fn new(bytes: &'a [u8], n: u32) -> Self {
        let nb = (n as usize).div_ceil(BLOCK);
        let ilen = if nb > 1 { 4 * nb } else { 0 };
        PosReader { index: &bytes[..ilen], data: &bytes[ilen..], next_rank: 0, off: 0 }
    }

    /// Positions of the posting of rank `r` (frequency `freq_at(r)`).
    fn read(&mut self, r: usize, mut freq_at: impl FnMut(usize) -> u32, out: &mut Vec<u32>) {
        let b = r / BLOCK;
        if r < self.next_rank || b > self.next_rank / BLOCK {
            self.next_rank = b * BLOCK;
            self.off = if self.index.is_empty() { 0 } else { u32::from_le_bytes(self.index[4 * b..4 * b + 4].try_into().unwrap()) as usize };
        }
        let data = self.data;
        while self.next_rank < r {
            let mut left = freq_at(self.next_rank);
            while left > 0 {
                left -= (data[self.off] < 0x80) as u32;
                self.off += 1;
            }
            self.next_rank += 1;
        }
        out.clear();
        let mut p = 0u32;
        for _ in 0..freq_at(r) {
            p += get_varint(data, &mut self.off) as u32;
            out.push(p);
        }
        self.next_rank = r + 1;
    }
}

enum Inner<'a> {
    Bp(BpCursor<'a>),
    Loom(LoomListCursor<'a>),
}

/// A forward cursor over one term's postings: docIDs, frequencies, block
/// impacts and (for fields with positions) positions.
///
/// A new cursor is on the first posting. [`doc`](Self::doc) is [`END`]
/// once exhausted.
pub struct PostingCursor<'a> {
    inner: Inner<'a>,
    df: u32,
    frontier: Vec<(u32, u8)>,
    positions: Option<PosReader<'a>>,
}

impl<'a> PostingCursor<'a> {
    /// A BP128 list starting at `data[0]` (`data` runs to the end of the
    /// postings section).
    pub(crate) fn bp(data: &'a [u8], df: u32, positions: Option<&'a [u8]>) -> Self {
        let mut pos = 0;
        let frontier = read_frontier(data, &mut pos);
        let inner = Inner::Bp(BpCursor::new(data, pos, df));
        PostingCursor { inner, df, frontier, positions: positions.map(|p| PosReader::new(p, df)) }
    }

    /// The Loom list at `start..end` of the postings section.
    pub(crate) fn loom(section: &'a OwnedBytes, start: usize, end: usize, df: u32, positions: Option<&'a [u8]>) -> Self {
        let (list, mut pos) = open_loom(section, start, end);
        debug_assert_eq!(list.len(), df as usize);
        let data = &section[start..];
        let frontier = read_frontier(data, &mut pos);
        let inner = Inner::Loom(LoomListCursor::new(list, data, pos));
        PostingCursor { inner, df, frontier, positions: positions.map(|p| PosReader::new(p, df)) }
    }

    /// Current docID ([`END`] when exhausted).
    #[inline]
    pub fn doc(&self) -> DocId {
        match &self.inner {
            Inner::Bp(c) => c.doc(),
            Inner::Loom(c) => c.doc(),
        }
    }

    /// Moves to the next posting.
    #[inline]
    pub fn next(&mut self) -> DocId {
        match &mut self.inner {
            Inner::Bp(c) => c.next(),
            Inner::Loom(c) => c.next(),
        }
    }

    /// Moves to the first posting `>= target` (NextGEQ); never backwards.
    #[inline]
    pub fn seek(&mut self, target: DocId) -> DocId {
        match &mut self.inner {
            Inner::Bp(c) => c.seek(target),
            Inner::Loom(c) => c.seek(target),
        }
    }

    /// Term frequency in the current document.
    #[inline]
    pub fn freq(&mut self) -> u32 {
        match &mut self.inner {
            Inner::Bp(c) => c.freq(),
            Inner::Loom(c) => c.freq(),
        }
    }

    /// Number of postings (document frequency in this segment).
    pub fn len(&self) -> u32 {
        self.df
    }

    pub fn is_empty(&self) -> bool {
        self.df == 0
    }

    /// Rank of the current posting (the list length once exhausted).
    #[inline]
    pub fn rank(&mut self) -> usize {
        match &mut self.inner {
            Inner::Bp(c) => c.rank(),
            Inner::Loom(c) => c.rank(),
        }
    }

    /// The list's impact frontier: `(tf, norm)` pairs, increasing in both,
    /// such that every posting has a smaller or equal tf than some pair with
    /// a smaller or equal norm.
    pub fn frontier(&self) -> &[(u32, u8)] {
        &self.frontier
    }

    /// Last docID and impact (`max_tf << 8 | min_norm`, or
    /// [`super::postings::LIST_IMPACT`] for "the list's frontier") of the
    /// block holding the first posting `>= target`, without decoding it;
    /// ([`END`], 0) past the end. Targets must not decrease between calls.
    #[inline]
    pub fn block_bound(&mut self, target: DocId) -> (DocId, Impact) {
        match &mut self.inner {
            Inner::Bp(c) => c.block_bound(target),
            Inner::Loom(c) => c.block_bound(target),
        }
    }

    /// Whether the field has positions.
    pub fn has_positions(&self) -> bool {
        self.positions.is_some()
    }

    /// Positions of the current posting, in increasing order (empty for
    /// fields without positions or an exhausted cursor).
    pub fn positions(&mut self, out: &mut Vec<u32>) {
        out.clear();
        if self.doc() == END {
            return;
        }
        let Some(pr) = self.positions.as_mut() else { return };
        match &mut self.inner {
            Inner::Bp(c) => {
                let r = c.rank();
                pr.read(r, |j| c.freq_at(j), out);
            }
            Inner::Loom(c) => {
                let r = c.rank();
                pr.read(r, |j| c.freq_at(j), out);
            }
        }
    }

    /// Whether the docIDs are a Loom list.
    pub fn is_loom(&self) -> bool {
        matches!(self.inner, Inner::Loom(_))
    }
}
