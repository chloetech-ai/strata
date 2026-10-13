//! Encoding primitives of the text index: varints, horizontally bit-packed
//! arrays with O(1) random access, and one-byte document-length norms.

/// Appends `v` as a LEB128 varint.
#[inline]
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Reads a LEB128 varint at `*pos`, advancing it. Panics past the end of
/// `data` (callers read what the builder wrote; sections are bounds-checked
/// slices).
#[inline]
pub fn get_varint(data: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = data[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
    }
}

/// Bytes needed for `n` values of `width` bits, packed.
#[inline]
pub fn packed_len(n: usize, width: u32) -> usize {
    (n * width as usize).div_ceil(8)
}

/// Appends the low `width` bits of every value, LSB first, value `i` at bit
/// `i * width` ([`packed_len`] bytes).
pub fn pack_bits(vals: &[u32], width: u32, out: &mut Vec<u8>) {
    let start = out.len();
    out.resize(start + packed_len(vals.len(), width), 0);
    if width == 0 {
        return;
    }
    let dst = &mut out[start..];
    let mut acc = 0u64;
    let mut bits = 0u32;
    let mut at = 0;
    for &v in vals {
        debug_assert!(width == 32 || v >> width == 0);
        acc |= (v as u64) << bits;
        bits += width;
        while bits >= 8 {
            dst[at] = acc as u8;
            at += 1;
            acc >>= 8;
            bits -= 8;
        }
    }
    if bits > 0 {
        dst[at] = acc as u8;
    }
}

/// Value `i` of an array packed by [`pack_bits`] starting at `data[0]`.
///
/// Reads 8 bytes from byte `i * width / 8`: callers keep 8 readable bytes
/// after every packed array (the sections end with padding).
#[inline(always)]
pub fn get_packed(data: &[u8], i: usize, width: u32) -> u32 {
    let bit = i * width as usize;
    let at = bit / 8;
    let w = u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
    // width <= 32 and bit % 8 <= 7, so the value lies within the 8 bytes.
    ((w >> (bit % 8)) & ((1u64 << width) - 1)) as u32
}

/// Decodes `out.len()` values packed by [`pack_bits`] at `data[0]`, adding
/// `add` to each (8 readable bytes after the array, as for [`get_packed`]).
#[inline]
pub fn unpack_bits(data: &[u8], width: u32, add: u32, out: &mut [u32]) {
    if width == 0 {
        out.fill(add);
        return;
    }
    for (i, o) in out.iter_mut().enumerate() {
        *o = get_packed(data, i, width) + add;
    }
}

/// Bit width of `x` (0 for 0).
#[inline]
pub fn bit_width(x: u32) -> u32 {
    32 - x.leading_zeros()
}

/// Document length to a one-byte norm, as Lucene's `SmallFloat.intToByte4`:
/// lengths 0..=23 exactly, longer ones with 4 significant bits (rounded
/// down, so at most 12.5% short). Monotone, so the norm order is the length
/// order.
pub const fn encode_norm(len: u32) -> u8 {
    if len < NORM_EXACT {
        return len as u8;
    }
    let i = (len - NORM_EXACT) as u64;
    let num_bits = 64 - i.leading_zeros();
    let int4 = if num_bits < 4 {
        i
    } else {
        let shift = num_bits - 4;
        ((i >> shift) & 0x07) | ((shift as u64 + 1) << 3)
    };
    (NORM_EXACT as u64 + int4) as u8
}

/// The length a norm stands for (the smallest length encoding to it).
pub const fn decode_norm(norm: u8) -> u32 {
    let b = norm as u32;
    if b < NORM_EXACT {
        return b;
    }
    let i = b - NORM_EXACT;
    let bits = (i & 0x07) as u64;
    let shift = (i >> 3) as i32 - 1;
    let v = if shift < 0 { bits } else { (bits | 0x08) << shift };
    NORM_EXACT + v as u32
}

/// Lengths below this are encoded exactly.
const NORM_EXACT: u32 = 24;

/// [`decode_norm`] of every byte.
pub static NORM_LENGTHS: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = decode_norm(i as u8);
        i += 1;
    }
    t
};

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn varints_roundtrip() {
        let vals = [0u64, 1, 127, 128, 300, 1 << 35, u64::MAX];
        let mut buf = Vec::new();
        for &v in &vals {
            put_varint(&mut buf, v);
        }
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(get_varint(&buf, &mut pos), v);
        }
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn packing_roundtrip() {
        let mut rng = StdRng::seed_from_u64(3);
        for width in 0..=32u32 {
            for n in [0usize, 1, 7, 128, 300] {
                let vals: Vec<u32> = (0..n).map(|_| if width == 0 { 0 } else { (rng.gen::<u64>() & ((1u64 << width) - 1)) as u32 }).collect();
                let mut buf = vec![9u8];
                pack_bits(&vals, width, &mut buf);
                assert_eq!(buf.len(), 1 + packed_len(n, width));
                buf.extend([0u8; 8]);
                for (i, &v) in vals.iter().enumerate() {
                    assert_eq!(get_packed(&buf[1..], i, width), v);
                }
                let mut out = vec![0u32; n];
                unpack_bits(&buf[1..], width, 1, &mut out);
                assert!(out.iter().zip(&vals).all(|(o, v)| *o == v + 1));
            }
        }
    }

    #[test]
    fn norms_match_lucene() {
        // Exact below 24, then 3 explicit mantissa bits, rounding down.
        for len in 0..24 {
            assert_eq!(decode_norm(encode_norm(len)), len);
        }
        let mut prev = 0;
        for len in (0..100_000u32).chain([1 << 20, u32::MAX >> 1]) {
            let n = encode_norm(len);
            let back = decode_norm(n);
            assert!(back <= len && (len - back) as f64 <= len as f64 / 8.0, "{len} -> {n} -> {back}");
            assert!(n >= prev, "monotone");
            prev = n;
            assert_eq!(encode_norm(back), n);
        }
        // Values from Lucene's SmallFloat tests.
        assert_eq!(encode_norm(24), 24);
        assert_eq!(decode_norm(255), 24 + (15 << 27));
        assert_eq!(NORM_LENGTHS[30], decode_norm(30));
    }
}
