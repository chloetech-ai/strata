//! Block-based posting lists (the PISA / Lucene / Tantivy layout).
//!
//! A list is cut into blocks of [`BLOCK`] = 128 postings. Each block is
//! encoded independently by a [`BlockCodec`]; a skip table stores, per
//! block, its last docID and the byte offset of its encoding (32 + 32 bits,
//! as in PISA's `block_posting_list`). `next_geq` gallops over the skip
//! table, decodes only the target block, and binary-searches inside it.
//!
//! Codecs see absolute docIDs plus two bounds known at decode time:
//! `base` (previous block's last docID + 1, or 0) and `last` (this block's
//! last docID, from the skip table). Gap-based codecs encode
//! `d[0] - base` and `d[i] - d[i-1] - 1`.

use super::{Cursor, CursorList, PostingList};
use crate::END;
use std::marker::PhantomData;

/// Postings per block.
pub const BLOCK: usize = 128;

/// Bytes of zero padding after the encoded data, so decoders may read a few
/// bytes past the end of a block without bounds checks failing.
pub const PADDING: usize = 16;

/// Encoder/decoder for one block of up to [`BLOCK`] postings.
pub trait BlockCodec: Send + Sync + 'static {
    const NAME: &'static str;

    /// Encode `docs` (1..=BLOCK strictly increasing docIDs, `docs[0] >= base`).
    fn encode(docs: &[u32], base: u32, out: &mut Vec<u8>);

    /// Decode `n` docIDs into `out[..n]`. `data` starts at the block's
    /// encoding and extends at least [`PADDING`] bytes past its end.
    /// `last` equals the block's final docID.
    fn decode(data: &[u8], n: usize, base: u32, last: u32, out: &mut [u32; BLOCK]);
}

/// Gaps for a block: `g[0] = d[0] - base`, `g[i] = d[i] - d[i-1] - 1`.
#[inline]
pub fn block_gaps(docs: &[u32], base: u32, gaps: &mut [u32; BLOCK]) {
    let mut prev = base.wrapping_sub(1);
    for (g, &d) in gaps.iter_mut().zip(docs) {
        *g = d.wrapping_sub(prev).wrapping_sub(1);
        prev = d;
    }
}

/// Inverse of [`block_gaps`]: in-place prefix sum over the first `n` gaps.
#[inline]
pub fn prefix_sum(vals: &mut [u32; BLOCK], n: usize, base: u32) {
    let mut prev = base.wrapping_sub(1);
    for v in vals[..n].iter_mut() {
        prev = prev.wrapping_add(*v).wrapping_add(1);
        *v = prev;
    }
}

/// A posting list stored as independently decodable blocks plus a skip table.
pub struct BlockList<C: BlockCodec> {
    len: usize,
    /// Last docID of every block.
    maxes: Vec<u32>,
    /// Byte offset of every block in `data`, plus the end offset.
    offsets: Vec<u32>,
    data: Vec<u8>,
    _codec: PhantomData<C>,
}

impl<C: BlockCodec> BlockList<C> {
    #[inline]
    fn num_blocks(&self) -> usize {
        self.maxes.len()
    }

    #[inline]
    fn block_len(&self, b: usize) -> usize {
        if b + 1 < self.num_blocks() {
            BLOCK
        } else {
            self.len - b * BLOCK
        }
    }

    #[inline]
    fn block_base(&self, b: usize) -> u32 {
        if b == 0 {
            0
        } else {
            self.maxes[b - 1] + 1
        }
    }

    #[inline]
    fn decode_block(&self, b: usize, out: &mut [u32; BLOCK]) -> usize {
        let n = self.block_len(b);
        let start = self.offsets[b] as usize;
        C::decode(&self.data[start..], n, self.block_base(b), self.maxes[b], out);
        n
    }

    /// Bytes of encoded block data (excluding the skip table).
    pub fn data_bytes(&self) -> usize {
        self.data.len() - PADDING
    }
}

impl<C: BlockCodec> PostingList for BlockList<C> {
    const NAME: &'static str = C::NAME;

    fn build(docs: &[u32], _universe: u32) -> Self {
        let num_blocks = docs.len().div_ceil(BLOCK);
        let mut maxes = Vec::with_capacity(num_blocks);
        let mut offsets = Vec::with_capacity(num_blocks + 1);
        let mut data = Vec::new();
        let mut base = 0u32;
        for chunk in docs.chunks(BLOCK) {
            offsets.push(u32::try_from(data.len()).expect("posting list larger than 4 GiB"));
            C::encode(chunk, base, &mut data);
            let last = *chunk.last().unwrap();
            maxes.push(last);
            base = last.wrapping_add(1);
        }
        offsets.push(data.len() as u32);
        data.resize(data.len() + PADDING, 0);
        data.shrink_to_fit();
        Self {
            len: docs.len(),
            maxes,
            offsets,
            data,
            _codec: PhantomData,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn size_bits(&self) -> u64 {
        // Encoded blocks + skip table (last docID and offset per block).
        8 * self.data_bytes() as u64 + 64 * self.num_blocks() as u64
    }

    fn decode_into(&self, out: &mut Vec<u32>) {
        let mut buf = [0u32; BLOCK];
        out.reserve(self.len);
        for b in 0..self.num_blocks() {
            let n = self.decode_block(b, &mut buf);
            out.extend_from_slice(&buf[..n]);
        }
    }
}

pub struct BlockCursor<'a, C: BlockCodec> {
    list: &'a BlockList<C>,
    block: usize,
    pos: usize,
    n: usize,
    cur: u32,
    buf: [u32; BLOCK],
}

impl<'a, C: BlockCodec> BlockCursor<'a, C> {
    fn new(list: &'a BlockList<C>) -> Self {
        let mut c = Self {
            list,
            block: 0,
            pos: 0,
            n: 0,
            cur: END,
            buf: [0; BLOCK],
        };
        if list.len > 0 {
            c.n = list.decode_block(0, &mut c.buf);
            c.cur = c.buf[0];
        }
        c
    }

    #[inline]
    fn load(&mut self, b: usize) {
        self.block = b;
        self.pos = 0;
        self.n = self.list.decode_block(b, &mut self.buf);
    }
}

impl<C: BlockCodec> Cursor for BlockCursor<'_, C> {
    #[inline]
    fn docid(&self) -> u32 {
        self.cur
    }

    #[inline]
    fn next(&mut self) -> u32 {
        if self.cur == END {
            return END;
        }
        self.pos += 1;
        if self.pos < self.n {
            self.cur = self.buf[self.pos];
        } else if self.block + 1 < self.list.num_blocks() {
            self.load(self.block + 1);
            self.cur = self.buf[0];
        } else {
            self.cur = END;
        }
        self.cur
    }

    #[inline]
    fn next_geq(&mut self, target: u32) -> u32 {
        if self.cur >= target {
            return self.cur;
        }
        let maxes = &self.list.maxes;
        if target > maxes[self.block] {
            // Gallop over the skip table, then binary-search the last step.
            let from = self.block + 1;
            let b = super::uncompressed::gallop_geq(maxes, from, target);
            if b == maxes.len() {
                self.cur = END;
                return END;
            }
            self.load(b);
        }
        // The current block contains a docID >= target (its max is >= target).
        self.pos += self.buf[self.pos..self.n].partition_point(|&x| x < target);
        self.cur = self.buf[self.pos];
        self.cur
    }

    fn size(&self) -> usize {
        self.list.len
    }
}

impl<C: BlockCodec> CursorList for BlockList<C> {
    type Cursor<'a> = BlockCursor<'a, C>;

    fn cursor(&self) -> BlockCursor<'_, C> {
        BlockCursor::new(self)
    }
}
