//! Patched frame of reference with an optimal bit width per block
//! (OptPFor; Yan, Ding & Suel, "Inverted index compression and query
//! processing with optimized document ordering", WWW 2009; Lemire &
//! Boytsov 2015).
//!
//! Each block packs the low `b` bits of all 128 d-gaps (same vertical layout
//! as [`super::bp128`]) and stores the few gaps that need more than `b` bits
//! as *exceptions*: their position (1 byte) and their high part
//! `gap >> b` (VByte). `b` is chosen per block to minimise the total size:
//!
//! ```text
//! cost(b) = 2 + 16·b + Σ_{g ≥ 2^b} (1 + ⌈(width(g) − b) / 7⌉)   bytes
//! ```
//!
//! Full block: `[b][#exceptions][16·b packed bytes][positions][VByte highs]`.
//! A trailing partial block is VByte coded.

use super::block::{block_gaps, prefix_sum, BlockCodec, BlockList, BLOCK};
use super::{bp128, vbyte};
use crate::bits::bit_width;

/// Pick the bit width minimising the block size, from the histogram of gap
/// bit widths. Returns `(b, number of exceptions)`.
fn choose_width(gaps: &[u32; BLOCK]) -> (u32, usize) {
    let mut hist = [0usize; 33];
    for &g in gaps {
        hist[bit_width(g as u64) as usize] += 1;
    }
    let max_w = (0..=32).rev().find(|&w| hist[w] > 0).unwrap_or(0);
    let (mut best_b, mut best_cost, mut best_exc) = (max_w as u32, usize::MAX, 0);
    for b in 0..=max_w {
        let mut cost = 2 + 16 * b;
        let mut exceptions = 0;
        for (w, &count) in hist.iter().enumerate().skip(b + 1) {
            exceptions += count;
            cost += count * (1 + (w - b).div_ceil(7));
        }
        if cost < best_cost {
            (best_b, best_cost, best_exc) = (b as u32, cost, exceptions);
        }
    }
    (best_b, best_exc)
}

pub struct OptPForBlock;

impl BlockCodec for OptPForBlock {
    const NAME: &'static str = "OptPFor";

    fn encode(docs: &[u32], base: u32, out: &mut Vec<u8>) {
        if docs.len() < BLOCK {
            vbyte::encode_gaps(docs, base, out);
            return;
        }
        let mut gaps = [0u32; BLOCK];
        block_gaps(docs, base, &mut gaps);
        let (b, exceptions) = choose_width(&gaps);
        out.push(b as u8);
        out.push(exceptions as u8);
        bp128::pack(&gaps, b, out);
        if exceptions > 0 {
            for (i, &g) in gaps.iter().enumerate() {
                if bit_width(g as u64) > b {
                    out.push(i as u8);
                }
            }
            for &g in &gaps {
                if bit_width(g as u64) > b {
                    vbyte::write_vbyte(g >> b, out);
                }
            }
        }
    }

    #[inline]
    fn decode(data: &[u8], n: usize, base: u32, _last: u32, out: &mut [u32; BLOCK]) {
        if n < BLOCK {
            vbyte::decode_gaps(data, n, base, out);
            return;
        }
        let b = data[0] as u32;
        let exceptions = data[1] as usize;
        let packed = 2 + 16 * b as usize;
        bp128::unpack(&data[2..], b, out);
        if exceptions > 0 {
            let positions = &data[packed..packed + exceptions];
            let mut pos = packed + exceptions;
            for &p in positions {
                out[p as usize] |= vbyte::read_vbyte(data, &mut pos) << b;
            }
        }
        prefix_sum(out, BLOCK, base);
    }
}

/// OptPFor block posting list.
pub type OptPFor = BlockList<OptPForBlock>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<OptPFor>();
        conformance::check_cursor::<OptPFor>();
    }

    #[test]
    fn outliers_become_exceptions() {
        let mut gaps = [3u32; BLOCK];
        gaps[17] = 1 << 20;
        gaps[90] = u32::MAX;
        let (b, exc) = choose_width(&gaps);
        assert_eq!((b, exc), (2, 2));
    }
}
