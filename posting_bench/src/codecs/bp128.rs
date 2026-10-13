//! Binary packing ("frame of reference") of 128 d-gaps per block with a
//! single bit width `b = bit_width(max gap)` per block — the codec family
//! behind Lucene's and Tantivy's postings and Lemire & Boytsov's SIMD-BP128
//! ("Decoding billions of integers per second through vectorization",
//! SPE 2015).
//!
//! Values use the 4-lane *vertical* layout of SIMD-BP128: integer `i` lives
//! in lane `i % 4`, and word `k` of lane `l` is stored at word index
//! `4k + l`. Each lane packs its 32 values independently, so unpacking is
//! the same shift/mask sequence applied to 4 lanes at once; the code below
//! is written lane-wise with a compile-time bit width so LLVM fully unrolls
//! and vectorizes it.
//!
//! Full block: `[b: u8][16·b bytes]`. A trailing partial block is VByte
//! coded, as in Lucene and Tantivy.

use super::block::{block_gaps, prefix_sum, BlockCodec, BlockList, BLOCK};
use super::vbyte;
use crate::bits::bit_width;

/// Append the low `b` bits of every value in vertical 4-lane layout
/// (`16·b` bytes).
pub fn pack(vals: &[u32; BLOCK], b: u32, out: &mut Vec<u8>) {
    if b == 0 {
        return;
    }
    let start = out.len();
    out.resize(start + 16 * b as usize, 0);
    let dst = &mut out[start..];
    let mask = ((1u64 << b) - 1) as u32;
    for lane in 0..4 {
        let mut acc = 0u64;
        let mut bits = 0u32;
        let mut word = 0usize;
        for j in 0..32 {
            acc |= ((vals[4 * j + lane] & mask) as u64) << bits;
            bits += b;
            if bits >= 32 {
                let at = 4 * (4 * word + lane);
                dst[at..at + 4].copy_from_slice(&(acc as u32).to_le_bytes());
                word += 1;
                acc >>= 32;
                bits -= 32;
            }
        }
    }
}

#[inline(always)]
fn load4(d: &[u8], word: usize) -> [u32; 4] {
    let mut r = [0u32; 4];
    for (l, r) in r.iter_mut().enumerate() {
        let at = 16 * word + 4 * l;
        *r = u32::from_le_bytes(d[at..at + 4].try_into().unwrap());
    }
    r
}

#[inline(always)]
fn unpack_const<const B: u32>(data: &[u8], out: &mut [u32; BLOCK]) {
    if B == 0 {
        *out = [0; BLOCK];
        return;
    }
    let d = &data[..16 * B as usize];
    if B == 32 {
        for (i, o) in out.iter_mut().enumerate() {
            *o = u32::from_le_bytes(d[4 * i..4 * i + 4].try_into().unwrap());
        }
        return;
    }
    let mask = ((1u64 << B) - 1) as u32;
    let mut word = 0usize;
    let mut cur = load4(d, 0);
    let mut shift = 0u32;
    for j in 0..32 {
        let mut v = [0u32; 4];
        for l in 0..4 {
            v[l] = cur[l] >> shift;
        }
        let end = shift + B;
        if end >= 32 {
            word += 1;
            if word < B as usize {
                let next = load4(d, word);
                if end > 32 {
                    for l in 0..4 {
                        v[l] |= next[l] << (32 - shift);
                    }
                }
                cur = next;
            }
            shift = end - 32;
        } else {
            shift = end;
        }
        for l in 0..4 {
            out[4 * j + l] = v[l] & mask;
        }
    }
}

macro_rules! dispatch_unpack {
    ($b:expr, $data:expr, $out:expr; $($n:literal)*) => {
        match $b {
            $($n => unpack_const::<$n>($data, $out),)*
            _ => panic!("invalid bit width {}", $b),
        }
    };
}

/// Unpack 128 `b`-bit values written by [`pack`].
#[inline]
pub fn unpack(data: &[u8], b: u32, out: &mut [u32; BLOCK]) {
    dispatch_unpack!(b, data, out; 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16
        17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);
}

pub struct Bp128Block;

impl BlockCodec for Bp128Block {
    const NAME: &'static str = "BP128";

    fn encode(docs: &[u32], base: u32, out: &mut Vec<u8>) {
        if docs.len() < BLOCK {
            vbyte::encode_gaps(docs, base, out);
            return;
        }
        let mut gaps = [0u32; BLOCK];
        block_gaps(docs, base, &mut gaps);
        let b = bit_width(*gaps.iter().max().unwrap() as u64);
        out.push(b as u8);
        pack(&gaps, b, out);
    }

    #[inline]
    fn decode(data: &[u8], n: usize, base: u32, _last: u32, out: &mut [u32; BLOCK]) {
        if n < BLOCK {
            vbyte::decode_gaps(data, n, base, out);
            return;
        }
        unpack(&data[1..], data[0] as u32, out);
        prefix_sum(out, BLOCK, base);
    }
}

/// BP128 block posting list.
pub type Bp128 = BlockList<Bp128Block>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;
    use rand::{Rng, SeedableRng};

    #[test]
    fn pack_unpack_all_widths() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(5);
        for b in 0..=32u32 {
            let mut vals = [0u32; BLOCK];
            for v in vals.iter_mut() {
                *v = if b == 0 { 0 } else { (rng.gen::<u64>() & ((1u64 << b) - 1)) as u32 };
            }
            let mut buf = Vec::new();
            pack(&vals, b, &mut buf);
            assert_eq!(buf.len(), 16 * b as usize);
            let mut out = [0u32; BLOCK];
            unpack(&buf, b, &mut out);
            assert_eq!(out, vals, "bit width {b}");
        }
    }

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<Bp128>();
        conformance::check_cursor::<Bp128>();
    }
}
