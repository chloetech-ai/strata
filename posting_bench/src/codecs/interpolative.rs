//! Binary interpolative coding (BIC; Moffat & Stuiver, "Binary interpolative
//! coding for effective index compression", Information Retrieval 2000),
//! applied per block of 128 docIDs as in PISA's `block_interpolative`.
//!
//! To code a sorted sequence `v[0..n)` known to lie in `[lo, hi]`, write the
//! middle element `v[m]` (`m = n/2`) relative to the range it can occupy,
//! then recurse on both halves with tightened bounds:
//!
//! ```text
//! v[m] ∈ [lo + m, hi − (n − 1 − m)]   →   ⌈log2(range)⌉ bits
//! left half  ⊂ [lo, v[m] − 1],   right half ⊂ [v[m] + 1, hi]
//! ```
//!
//! Dense runs cost zero bits (the range collapses to a single value), which
//! makes BIC the most compact codec on clustered lists, at the price of a
//! slow, branchy, bit-serial decoder. The block's last docID is already in
//! the skip table, so only the first `n − 1` values are coded, in
//! `[base, last − 1]`.

use super::block::{BlockCodec, BlockList, BLOCK};
use crate::bits::{bit_width, BitVec};

fn encode_rec(vals: &[u32], lo: u32, hi: u32, bits: &mut BitVec) {
    let n = vals.len();
    if n == 0 || (hi - lo) as usize + 1 == n {
        return; // empty, or the range is exactly filled: zero bits
    }
    let m = n / 2;
    let v = vals[m];
    let low = lo + m as u32;
    let high = hi - (n - 1 - m) as u32;
    bits.push_bits((v - low) as u64, bit_width((high - low) as u64));
    if m > 0 {
        encode_rec(&vals[..m], lo, v - 1, bits);
    }
    encode_rec(&vals[m + 1..], v + 1, hi, bits);
}

/// Little-endian bit reader over bytes; relies on the block list's padding
/// to load 8 bytes at a time.
struct ByteBits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl ByteBits<'_> {
    #[inline]
    fn read(&mut self, width: u32) -> u32 {
        if width == 0 {
            return 0;
        }
        let byte = self.pos / 8;
        let word = u64::from_le_bytes(self.data[byte..byte + 8].try_into().unwrap());
        let v = (word >> (self.pos % 8)) & ((1u64 << width) - 1);
        self.pos += width as usize;
        v as u32
    }
}

fn decode_rec(r: &mut ByteBits, out: &mut [u32], lo: u32, hi: u32) {
    let n = out.len();
    if n == 0 {
        return;
    }
    if (hi - lo) as usize + 1 == n {
        for (i, o) in out.iter_mut().enumerate() {
            *o = lo + i as u32;
        }
        return;
    }
    let m = n / 2;
    let low = lo + m as u32;
    let high = hi - (n - 1 - m) as u32;
    let v = low + r.read(bit_width((high - low) as u64));
    out[m] = v;
    let (left, right) = out.split_at_mut(m);
    if m > 0 {
        decode_rec(r, left, lo, v - 1);
    }
    decode_rec(r, &mut right[1..], v + 1, hi);
}

pub struct InterpolativeBlock;

impl BlockCodec for InterpolativeBlock {
    const NAME: &'static str = "Interpolative";

    fn encode(docs: &[u32], base: u32, out: &mut Vec<u8>) {
        let n = docs.len();
        if n < 2 {
            return;
        }
        let last = docs[n - 1];
        let mut bits = BitVec::new();
        encode_rec(&docs[..n - 1], base, last - 1, &mut bits);
        let bytes = bits.len().div_ceil(8);
        out.extend(bits.words().iter().flat_map(|w| w.to_le_bytes()).take(bytes));
    }

    #[inline]
    fn decode(data: &[u8], n: usize, base: u32, last: u32, out: &mut [u32; BLOCK]) {
        out[n - 1] = last;
        if n >= 2 {
            let mut r = ByteBits { data, pos: 0 };
            decode_rec(&mut r, &mut out[..n - 1], base, last - 1);
        }
    }
}

/// Binary-interpolative-coded block posting list.
pub type Interpolative = BlockList<InterpolativeBlock>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;
    use crate::codecs::PostingList;

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<Interpolative>();
        conformance::check_cursor::<Interpolative>();
    }

    #[test]
    fn dense_runs_are_nearly_free() {
        let docs: Vec<u32> = (0..128 * 50).collect();
        let list = Interpolative::build(&docs, 10_000);
        // Only the skip table remains: 64 bits per block.
        assert_eq!(list.size_bits(), 64 * 50);
    }
}
