//! Variable-byte (VByte / varint) coding of d-gaps: 7 data bits per byte,
//! the high bit set on every byte except the last of a value. Byte-aligned
//! and branchy, but simple and robust; used as the tail codec of the
//! bit-packing codecs (as Lucene and Tantivy do).

use super::block::{BlockCodec, BlockList, BLOCK};

#[inline]
pub fn write_vbyte(mut v: u32, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline]
pub fn read_vbyte(data: &[u8], pos: &mut usize) -> u32 {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        let b = data[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u32) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
}

/// Number of bytes VByte uses for `v`.
#[inline]
pub fn vbyte_len(v: u32) -> usize {
    match v {
        0..=0x7f => 1,
        0x80..=0x3fff => 2,
        0x4000..=0x1f_ffff => 3,
        0x20_0000..=0x0fff_ffff => 4,
        _ => 5,
    }
}

/// Encode a block's d-gaps with VByte.
pub fn encode_gaps(docs: &[u32], base: u32, out: &mut Vec<u8>) {
    let mut prev = base.wrapping_sub(1);
    for &d in docs {
        write_vbyte(d.wrapping_sub(prev).wrapping_sub(1), out);
        prev = d;
    }
}

/// Decode `n` VByte d-gaps into docIDs; returns the number of bytes read.
#[inline]
pub fn decode_gaps(data: &[u8], n: usize, base: u32, out: &mut [u32; BLOCK]) -> usize {
    let mut pos = 0;
    let mut prev = base.wrapping_sub(1);
    for o in out[..n].iter_mut() {
        prev = prev.wrapping_add(read_vbyte(data, &mut pos)).wrapping_add(1);
        *o = prev;
    }
    pos
}

pub struct VByteBlock;

impl BlockCodec for VByteBlock {
    const NAME: &'static str = "VByte";

    fn encode(docs: &[u32], base: u32, out: &mut Vec<u8>) {
        encode_gaps(docs, base, out);
    }

    #[inline]
    fn decode(data: &[u8], n: usize, base: u32, _last: u32, out: &mut [u32; BLOCK]) {
        decode_gaps(data, n, base, out);
    }
}

/// VByte-coded block posting list.
pub type VByte = BlockList<VByteBlock>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<VByte>();
        conformance::check_cursor::<VByte>();
    }

    #[test]
    fn vbyte_values() {
        let vals = [0u32, 1, 127, 128, 16383, 16384, u32::MAX];
        let mut buf = Vec::new();
        for &v in &vals {
            write_vbyte(v, &mut buf);
        }
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(read_vbyte(&buf, &mut pos), v);
        }
        assert_eq!(pos, buf.len());
        assert_eq!(buf.len(), vals.iter().map(|&v| vbyte_len(v)).sum::<usize>());
    }
}
