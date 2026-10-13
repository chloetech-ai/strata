//! Production libraries wrapped as reference baselines:
//!
//! * [`TantivyBp128`] — the `bitpacking` crate's `BitPacker4x` (SIMD-BP128
//!   with strictly-sorted delta coding; the codec Tantivy uses for postings)
//!   inside our block/skip framework, VByte for the trailing partial block.
//! * [`RoaringRs`] — the `roaring` crate (`RoaringBitmap`), the Rust port of
//!   Roaring used by many Rust data systems.

use super::block::{BlockCodec, BlockList, BLOCK};
use super::{vbyte, Cursor, CursorList, PostingList, SetOps};
use crate::END;
use bitpacking::{BitPacker, BitPacker4x};
use roaring::{MultiOps, RoaringBitmap};

const _: () = assert!(BitPacker4x::BLOCK_LEN == BLOCK);

pub struct TantivyBlock;

#[inline]
fn initial(base: u32) -> Option<u32> {
    // Strictly-sorted deltas are `d[i] - d[i-1] - 1`, starting from the
    // previous block's last docID.
    base.checked_sub(1)
}

impl BlockCodec for TantivyBlock {
    const NAME: &'static str = "BP128 (bitpacking crate)";

    fn encode(docs: &[u32], base: u32, out: &mut Vec<u8>) {
        if docs.len() < BLOCK {
            vbyte::encode_gaps(docs, base, out);
            return;
        }
        let packer = BitPacker4x::new();
        let num_bits = packer.num_bits_strictly_sorted(initial(base), docs);
        out.push(num_bits);
        let start = out.len();
        out.resize(start + BitPacker4x::compressed_block_size(num_bits), 0);
        packer.compress_strictly_sorted(initial(base), docs, &mut out[start..], num_bits);
    }

    #[inline]
    fn decode(data: &[u8], n: usize, base: u32, _last: u32, out: &mut [u32; BLOCK]) {
        if n < BLOCK {
            vbyte::decode_gaps(data, n, base, out);
            return;
        }
        BitPacker4x::new().decompress_strictly_sorted(initial(base), &data[1..], out, data[0]);
    }
}

/// Block posting list using the `bitpacking` crate.
pub type TantivyBp128 = BlockList<TantivyBlock>;

/// The `roaring` crate's `RoaringBitmap`.
pub struct RoaringRs(pub RoaringBitmap);

impl PostingList for RoaringRs {
    const NAME: &'static str = "Roaring (roaring crate)";

    fn build(docs: &[u32], _universe: u32) -> Self {
        let mut bitmap = RoaringBitmap::from_sorted_iter(docs.iter().copied()).expect("sorted input");
        // Convert chunks to run containers where smaller (CRoaring's runOptimize).
        bitmap.optimize();
        Self(bitmap)
    }

    fn len(&self) -> usize {
        self.0.len() as usize
    }

    fn size_bits(&self) -> u64 {
        8 * self.0.serialized_size() as u64
    }

    fn decode_into(&self, out: &mut Vec<u32>) {
        out.extend(self.0.iter());
    }
}

impl SetOps for RoaringRs {
    fn intersect(lists: &[&Self], out: &mut Vec<u32>) {
        out.extend(lists.iter().map(|l| &l.0).intersection().iter());
    }

    fn union(lists: &[&Self], out: &mut Vec<u32>) {
        out.extend(lists.iter().map(|l| &l.0).union().iter());
    }
}

impl RoaringRs {
    /// Intersection without materialising docIDs (returns the cardinality).
    pub fn and_cardinality(lists: &[&Self]) -> u64 {
        lists.iter().map(|l| &l.0).intersection().len()
    }

    /// Union without materialising docIDs (returns the cardinality).
    pub fn or_cardinality(lists: &[&Self]) -> u64 {
        lists.iter().map(|l| &l.0).union().len()
    }
}

pub struct RoaringRsCursor<'a> {
    iter: roaring::bitmap::Iter<'a>,
    cur: u32,
    len: usize,
}

impl Cursor for RoaringRsCursor<'_> {
    #[inline]
    fn docid(&self) -> u32 {
        self.cur
    }

    #[inline]
    fn next(&mut self) -> u32 {
        if self.cur != END {
            self.cur = self.iter.next().unwrap_or(END);
        }
        self.cur
    }

    #[inline]
    fn next_geq(&mut self, target: u32) -> u32 {
        if self.cur < target {
            self.iter.advance_to(target);
            self.cur = self.iter.next().unwrap_or(END);
        }
        self.cur
    }

    fn size(&self) -> usize {
        self.len
    }
}

impl CursorList for RoaringRs {
    type Cursor<'a> = RoaringRsCursor<'a>;

    fn cursor(&self) -> RoaringRsCursor<'_> {
        let mut iter = self.0.iter();
        let cur = iter.next().unwrap_or(END);
        RoaringRsCursor {
            iter,
            cur,
            len: self.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;

    #[test]
    fn conformance_tantivy() {
        conformance::check_roundtrip::<TantivyBp128>();
        conformance::check_cursor::<TantivyBp128>();
    }

    #[test]
    fn conformance_roaring_rs() {
        conformance::check_roundtrip::<RoaringRs>();
        conformance::check_cursor::<RoaringRs>();
        conformance::check_setops::<RoaringRs>();
    }
}
