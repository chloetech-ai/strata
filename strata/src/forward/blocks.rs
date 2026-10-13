//! Bit-packed blocks of integer keys with min/max zone maps: the storage
//! of every forward column (numbers as order-preserving keys, keywords as
//! dictionary codes).
//!
//! Documents are cut into blocks of [`BLOCK`] = 128. A block stores its
//! keys frame-of-reference: the block minimum is the base and every key is
//! kept as `key - min` in `w` bits, where `w` is the width of the block's
//! largest delta. The payload of a block is exactly `128 w` bits (`2w`
//! words), so the payload offsets are a prefix sum and the width needs no
//! storage of its own (`w = (offs[b + 1] - offs[b]) / 2`).
//!
//! * `w <= 32`: the SIMD layout of `bitpacking::BitPacker4x` (four
//!   interleaved 32-bit lanes); a block unpacks with SSE in a few ns and a
//!   single key is still two loads and shifts (see [`extract4x`]).
//! * `w > 32` (only for keys spanning more than 2^32 within one block):
//!   plain little-endian bit packing over `u64` words.
//!
//! Zone maps are the `(min, max)` of the present keys of every block and of
//! every superblock of [`SUPER`] blocks (4096 documents), as `u32` offsets
//! from the column minimum when the column's key range fits in 32 bits
//! (else as `i64`), interleaved so that one access reads both. A unit
//! without present keys has `min > max`. A scan checks superblocks first,
//! so a selective range over clustered values (a timestamp) skips or
//! accepts 32 blocks per check. Documents without a value are recorded in a
//! presence bitmap, which exists only if there are any; their slot holds
//! the block minimum.
//!
//! Section layout (written with [`ByteWriter`]): `n`, flags, column
//! minimum, maximum and present count, then the arrays block zones,
//! superblock zones, offs, data and presence.

use bitpacking::{BitPacker, BitPacker4x};
use posting_bench::bytes::{ByteReader, ByteWriter, Slice};

use crate::error::{corrupt, Result};

/// Documents per block.
pub const BLOCK: usize = 128;
/// Blocks per superblock.
pub const SUPER: usize = 32;

const HAS_NULLS: u64 = 1;
const NARROW: u64 = 2;

/// Writes the blocks of `keys` (one per document, `None` for no value).
pub fn write_blocks(keys: &[Option<i64>], w: &mut ByteWriter) {
    let n = keys.len();
    let present: Vec<i64> = keys.iter().flatten().copied().collect();
    let (gmin, gmax) = match (present.iter().min(), present.iter().max()) {
        (Some(&a), Some(&b)) => (a, b),
        _ => (0, 0),
    };
    let narrow = (gmax as i128 - gmin as i128) < u32::MAX as i128;
    let has_nulls = present.len() < n;
    let nb = n.div_ceil(BLOCK);
    let mut zones: Vec<(i64, i64)> = Vec::with_capacity(nb);
    let mut offs: Vec<u32> = Vec::with_capacity(nb + 1);
    let mut data: Vec<u64> = Vec::new();
    let packer = BitPacker4x::new();
    let mut buf = [0u32; BLOCK];
    let mut bytes = [0u8; 4 * BLOCK];
    offs.push(0);
    for b in 0..nb {
        let blk = &keys[b * BLOCK..((b + 1) * BLOCK).min(n)];
        let (min, max) = blk.iter().flatten().fold((i64::MAX, i64::MIN), |(lo, hi), &k| (lo.min(k), hi.max(k)));
        let empty = min > max;
        zones.push((min, max));
        let base = if empty { 0 } else { min };
        let deltas: Vec<u64> = (0..BLOCK).map(|i| blk.get(i).copied().flatten().map_or(0, |k| k.wrapping_sub(base) as u64)).collect();
        let width = deltas.iter().map(|&d| 64 - d.leading_zeros()).max().unwrap_or(0) as usize;
        if width == 0 {
        } else if width <= 32 {
            for (x, &d) in buf.iter_mut().zip(&deltas) {
                *x = d as u32;
            }
            let len = packer.compress(&buf, &mut bytes, width as u8);
            debug_assert_eq!(len, 16 * width);
            data.extend(bytes[..len].chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())));
        } else {
            let start = data.len();
            data.resize(start + 2 * width, 0);
            for (i, &d) in deltas.iter().enumerate() {
                let bit = i * width;
                let (k, sh) = (start + bit / 64, bit % 64);
                data[k] |= d << sh;
                if sh + width > 64 {
                    data[k + 1] |= d >> (64 - sh);
                }
            }
        }
        offs.push(data.len() as u32);
    }
    let mut presence = Vec::new();
    if has_nulls {
        presence = vec![0u64; n.div_ceil(64)];
        for (d, k) in keys.iter().enumerate() {
            if k.is_some() {
                presence[d / 64] |= 1 << (d % 64);
            }
        }
    }
    w.u64(n as u64);
    w.u64(if has_nulls { HAS_NULLS } else { 0 } | if narrow { NARROW } else { 0 });
    w.u64(gmin as u64);
    w.u64(gmax as u64);
    w.u64(present.len() as u64);
    let supers: Vec<(i64, i64)> =
        zones.chunks(SUPER).map(|c| c.iter().fold((i64::MAX, i64::MIN), |(lo, hi), &(a, z)| (lo.min(a), hi.max(z)))).collect();
    write_zone(&zones, gmin, narrow, w);
    write_zone(&supers, gmin, narrow, w);
    w.array(&offs);
    w.array(&data);
    w.array(&presence);
}

/// Writes `(min, max)` pairs (`min > max` for no keys) as zone maps.
fn write_zone(pairs: &[(i64, i64)], gmin: i64, narrow: bool, w: &mut ByteWriter) {
    if narrow {
        let z: Vec<u32> =
            pairs.iter().flat_map(|&(a, z)| if a > z { [1, 0] } else { [(a - gmin) as u32, (z - gmin) as u32] }).collect();
        w.array(&z);
    } else {
        w.array(&pairs.iter().flat_map(|&(a, z)| [a, z]).collect::<Vec<i64>>());
    }
}

/// Zone maps: `(min, max)` per unit, interleaved.
#[derive(Clone, Debug)]
enum Zone {
    /// Offsets from the column minimum.
    Narrow(Slice<u32>),
    Wide(Slice<i64>),
}

impl Zone {
    fn read(r: &mut ByteReader, narrow: bool) -> Result<Zone> {
        Ok(if narrow { Zone::Narrow(r.array().map_err(corrupt)?) } else { Zone::Wide(r.array().map_err(corrupt)?) })
    }

    /// `(min, max)` of unit `i`.
    #[inline]
    fn get(&self, i: usize, gmin: i64) -> (i64, i64) {
        match self {
            Zone::Narrow(z) => (gmin + z[2 * i] as i64, gmin + z[2 * i + 1] as i64),
            Zone::Wide(z) => (z[2 * i], z[2 * i + 1]),
        }
    }

    /// Number of units.
    fn len(&self) -> usize {
        match self {
            Zone::Narrow(z) => z.len() / 2,
            Zone::Wide(z) => z.len() / 2,
        }
    }

    fn byte_len(&self) -> usize {
        match self {
            Zone::Narrow(z) => z.byte_len(),
            Zone::Wide(z) => z.byte_len(),
        }
    }

    fn is_view(&self) -> bool {
        match self {
            Zone::Narrow(z) => matches!(z, Slice::Shared { .. }),
            Zone::Wide(z) => matches!(z, Slice::Shared { .. }),
        }
    }
}

/// A zero-copy view of blocks written by [`write_blocks`].
#[derive(Clone, Debug)]
pub struct Blocks {
    n: usize,
    gmin: i64,
    gmax: i64,
    n_present: usize,
    zone: Zone,
    super_zone: Zone,
    offs: Slice<u32>,
    data: Slice<u64>,
    /// Empty when every document has a value.
    present: Slice<u64>,
}

/// A key range `[lo, hi]` (inclusive) to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRange {
    pub lo: i64,
    pub hi: i64,
}

impl KeyRange {
    #[inline]
    pub fn contains(&self, k: i64) -> bool {
        self.lo <= k && k <= self.hi
    }
}

impl Blocks {
    pub fn read(r: &mut ByteReader) -> Result<Blocks> {
        let n = r.u64().map_err(corrupt)? as usize;
        let flags = r.u64().map_err(corrupt)?;
        let gmin = r.u64().map_err(corrupt)? as i64;
        let gmax = r.u64().map_err(corrupt)? as i64;
        let n_present = r.u64().map_err(corrupt)? as usize;
        let zone = Zone::read(r, flags & NARROW != 0)?;
        let super_zone = Zone::read(r, flags & NARROW != 0)?;
        let offs: Slice<u32> = r.array().map_err(corrupt)?;
        let data: Slice<u64> = r.array().map_err(corrupt)?;
        let present: Slice<u64> = r.array().map_err(corrupt)?;
        let nb = n.div_ceil(BLOCK);
        // O(1) checks; per-block offsets are checked where they are read.
        if zone.len() != nb
            || super_zone.len() != nb.div_ceil(SUPER)
            || offs.len() != nb + 1
            || offs.last().map_or(true, |&o| o as usize != data.len())
            || (flags & HAS_NULLS != 0) != (present.len() == n.div_ceil(64) && n > 0)
            || n_present > n
        {
            return Err(corrupt("bad column blocks"));
        }
        Ok(Blocks { n, gmin, gmax, n_present, zone, super_zone, offs, data, present })
    }

    pub fn len(&self) -> usize {
        self.n
    }

    /// Number of documents with a value.
    pub fn n_present(&self) -> usize {
        self.n_present
    }

    /// Smallest and largest key (`None` without values).
    pub fn key_bounds(&self) -> Option<(i64, i64)> {
        (self.n_present > 0).then_some((self.gmin, self.gmax))
    }

    pub fn n_blocks(&self) -> usize {
        self.n.div_ceil(BLOCK)
    }

    /// `(min, max)` of the present keys of block `b` (`min > max` if none).
    #[inline]
    pub fn zone(&self, b: usize) -> (i64, i64) {
        self.zone.get(b, self.gmin)
    }

    /// `(min, max)` of the present keys of superblock `s`.
    #[inline]
    pub fn super_zone(&self, s: usize) -> (i64, i64) {
        self.super_zone.get(s, self.gmin)
    }

    /// Whether document `d` has a value.
    #[inline]
    pub fn is_present(&self, d: u32) -> bool {
        self.present.is_empty() || self.present[d as usize / 64] >> (d % 64) & 1 == 1
    }

    /// Bit width and payload of block `b`.
    #[inline]
    fn payload(&self, b: usize) -> (usize, &[u64]) {
        let (o, e) = (self.offs[b] as usize, self.offs[b + 1] as usize);
        let p = &self.data[o.min(e)..e];
        // Widths above 64 only come from corrupt offsets.
        ((p.len() / 2).min(64), p)
    }

    /// The key stored for document `d` (the block minimum if `d` has no
    /// value).
    #[inline]
    pub fn raw(&self, d: u32) -> i64 {
        let (b, j) = (d as usize / BLOCK, d as usize % BLOCK);
        let (w, p) = self.payload(b);
        let base = self.zone(b).0;
        if w == 0 {
            return base;
        }
        // SAFETY: j < 128, and the payload holds 2w words (w = len / 2).
        let delta = unsafe { if w <= 32 { extract4x(p, j, w) as u64 } else { extract_wide(p, j, w) } };
        base.wrapping_add(delta as i64)
    }

    /// The key of document `d`.
    #[inline]
    pub fn get(&self, d: u32) -> Option<i64> {
        self.is_present(d).then(|| self.raw(d))
    }

    /// Unpacks block `b` if its width is at most 32: writes `key - min`
    /// for its 128 slots (padding slots past `n` and slots without a value
    /// hold 0) and returns the minimum; `None` for wider blocks.
    #[inline]
    pub fn decode_block_u32(&self, b: usize, out: &mut [u32; BLOCK]) -> Option<i64> {
        let (w, p) = self.payload(b);
        if w > 32 {
            return None;
        }
        unpack4x(p, w, out);
        Some(self.zone(b).0)
    }

    /// The presence (and end-of-column) mask of block `b`'s two words.
    #[inline]
    pub fn present_mask(&self, b: usize) -> [u64; 2] {
        let mut m = [!0u64; 2];
        let rest = self.n - b * BLOCK;
        if rest < BLOCK {
            m[0] = if rest >= 64 { !0 } else { (1u64 << rest) - 1 };
            m[1] = if rest > 64 { (1u64 << (rest - 64)) - 1 } else { 0 };
        }
        if !self.present.is_empty() {
            m[0] &= self.present[2 * b];
            m[1] &= self.present.get(2 * b + 1).copied().unwrap_or(0);
        }
        m
    }

    /// Calls `f(b, mask)` for every block `b` holding a document with a key
    /// in `r`, with the 128-bit mask of those documents (bit `i` of word `k`
    /// is document `128 b + 64 k + i`). Zone maps of superblocks, then of
    /// blocks, skip disjoint units and accept contained ones without
    /// decoding.
    #[inline]
    pub fn scan(&self, r: KeyRange, mut f: impl FnMut(usize, [u64; 2])) {
        let mut d = [0u32; BLOCK];
        let nb = self.n_blocks();
        for s in 0..nb.div_ceil(SUPER) {
            let (min, max) = self.super_zone.get(s, self.gmin);
            if min > max || max < r.lo || min > r.hi {
                continue;
            }
            let inside = r.lo <= min && max <= r.hi;
            for b in s * SUPER..((s + 1) * SUPER).min(nb) {
                let m = if inside {
                    self.present_mask(b)
                } else {
                    let (min, max) = self.zone(b);
                    if min > max || max < r.lo || min > r.hi {
                        continue;
                    }
                    let valid = self.present_mask(b);
                    if r.lo <= min && max <= r.hi {
                        valid
                    } else {
                        let m = self.block_mask(b, r, &mut d);
                        [m[0] & valid[0], m[1] & valid[1]]
                    }
                };
                if m != [0, 0] {
                    f(b, m);
                }
            }
        }
    }

    /// Match mask of the keys of block `b` (padding and null slots
    /// included; the caller masks them).
    #[inline]
    fn block_mask(&self, b: usize, r: KeyRange, d: &mut [u32; BLOCK]) -> [u64; 2] {
        let (min, _) = self.zone(b);
        // Delta bounds: lo <= min + x <= hi, i.e. x - dlo <= span (wrapping).
        let dlo = (r.lo as i128 - min as i128).max(0);
        let dhi = r.hi as i128 - min as i128;
        if let Some(_) = self.decode_block_u32(b, d) {
            let dhi = dhi.min(u32::MAX as i128);
            return range_mask128(d, dlo as u32, (dhi - dlo) as u32);
        }
        let (w, p) = self.payload(b);
        let span = (dhi - dlo) as u64;
        let mut m = [0u64; 2];
        for j in 0..BLOCK {
            // SAFETY: j < 128; w > 32 and the payload holds 2w words.
            let x = unsafe { extract_wide(p, j, w) };
            m[j / 64] |= ((x.wrapping_sub(dlo as u64) <= span) as u64) << (j % 64);
        }
        m
    }

    /// Bitset (`n.div_ceil(64)` words) of the documents with a key in `r`.
    pub fn scan_bits(&self, r: KeyRange) -> Vec<u64> {
        let mut words = vec![0u64; self.n.div_ceil(64)];
        self.scan(r, |b, m| {
            words[2 * b] = m[0];
            if let Some(w) = words.get_mut(2 * b + 1) {
                *w = m[1];
            }
        });
        words
    }

    /// Number of documents with a key in `r`.
    pub fn scan_count(&self, r: KeyRange) -> usize {
        let mut c = 0;
        self.scan(r, |_, m| c += (m[0].count_ones() + m[1].count_ones()) as usize);
        c
    }

    /// Bytes of the arrays (zone maps, offsets, payload, presence).
    pub fn byte_sizes(&self) -> [usize; 4] {
        [self.zone.byte_len() + self.super_zone.byte_len(), self.offs.byte_len(), self.data.byte_len(), self.present.byte_len()]
    }

    /// Whether every array is a view into shared bytes.
    pub fn is_view(&self) -> bool {
        self.zone.is_view() && self.super_zone.is_view() && matches!(self.offs, Slice::Shared { .. }) && matches!(self.data, Slice::Shared { .. }) && matches!(self.present, Slice::Shared { .. })
    }
}

/// Unpacks a `BitPacker4x` block of width `w <= 32` (`2w` words).
#[inline]
fn unpack4x(p: &[u64], w: usize, out: &mut [u32; BLOCK]) {
    if w == 0 {
        *out = [0; BLOCK];
        return;
    }
    // SAFETY: u64 words viewed as their little-endian bytes.
    let bytes = unsafe { std::slice::from_raw_parts(p.as_ptr() as *const u8, 16 * w) };
    BitPacker4x::new().decompress(bytes, out, w as u8);
}

/// Value `j` of a `BitPacker4x` block of width `1..=32`: lane `j % 4`
/// holds values `j % 4, j % 4 + 4, ...` packed into its 32-bit words, which
/// are every fourth word of the block.
///
/// # Safety
/// `j < 128`, `1 <= w <= 32` and `p.len() >= 2w` (the block's `4w` 32-bit
/// words). Then `bit <= 31w`, so `4k + lane < 4w`; the next word is read
/// only if `bit + w > 32(k + 1)`, i.e. `k + 1 < w`.
#[inline]
unsafe fn extract4x(p: &[u64], j: usize, w: usize) -> u32 {
    debug_assert!(j < BLOCK && (1..=32).contains(&w) && p.len() >= 2 * w);
    // Little-endian u64 words are pairs of u32 words.
    let u = p.as_ptr() as *const u32;
    let (lane, bit) = (j & 3, (j >> 2) * w);
    let (k, sh) = (bit >> 5, bit & 31);
    let mut v = *u.add(4 * k + lane) >> sh;
    if sh + w > 32 {
        v |= *u.add(4 * (k + 1) + lane) << (32 - sh);
    }
    if w < 32 {
        v &= (1u32 << w) - 1;
    }
    v
}

/// Value `j` of a plain bit-packed block of width `33..=64`.
///
/// # Safety
/// `j < 128`, `33 <= w <= 64` and `p.len() >= 2w`. Then `k <= 127w / 64
/// < 2w`, and the next word is read only if `bit + w > 64(k + 1)`, i.e.
/// `k + 1 < 2w`.
#[inline]
unsafe fn extract_wide(p: &[u64], j: usize, w: usize) -> u64 {
    debug_assert!(j < BLOCK && (33..=64).contains(&w) && p.len() >= 2 * w);
    let bit = j * w;
    let (k, sh) = (bit / 64, bit % 64);
    let mut v = *p.get_unchecked(k) >> sh;
    if sh + w > 64 {
        v |= *p.get_unchecked(k + 1) << (64 - sh);
    }
    if w < 64 {
        v &= (1u64 << w) - 1;
    }
    v
}

/// Bit `j` set iff `d[j] - lo <= span` (wrapping), i.e.
/// `lo <= d[j] <= lo + span`: one unsigned compare per lane.
#[inline]
pub fn range_mask128(d: &[u32; BLOCK], lo: u32, span: u32) -> [u64; 2] {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    {
        use std::arch::x86_64::*;
        // SAFETY: AVX-512F is enabled for this build; 8 loads of 16 lanes
        // stay inside `d`.
        unsafe {
            let vlo = _mm512_set1_epi32(lo as i32);
            let vspan = _mm512_set1_epi32(span as i32);
            let mut m = [0u64; 2];
            for q in 0..8 {
                let v = _mm512_loadu_si512(d.as_ptr().add(16 * q) as *const _);
                let k = _mm512_cmple_epu32_mask(_mm512_sub_epi32(v, vlo), vspan) as u64;
                m[q / 4] |= k << (16 * (q % 4));
            }
            m
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
    range_mask128_scalar(d, lo, span)
}

/// Portable [`range_mask128`] (the fallback, and the tests' reference).
#[cfg_attr(all(target_arch = "x86_64", target_feature = "avx512f", not(test)), allow(dead_code))]
#[inline]
pub fn range_mask128_scalar(d: &[u32; BLOCK], lo: u32, span: u32) -> [u64; 2] {
    let mut m = [0u64; 2];
    for (h, half) in d.chunks_exact(64).enumerate() {
        for (j, &x) in half.iter().enumerate() {
            m[h] |= ((x.wrapping_sub(lo) <= span) as u64) << j;
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use posting_bench::bytes::OwnedBytes;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn roundtrip(keys: &[Option<i64>]) -> Blocks {
        let mut w = ByteWriter::new();
        write_blocks(keys, &mut w);
        Blocks::read(&mut ByteReader::new(OwnedBytes::new(w.finish()))).unwrap()
    }

    #[test]
    fn widths_and_layouts() {
        let mut rng = StdRng::seed_from_u64(1);
        for width in [0u32, 1, 5, 17, 31, 32, 33, 47, 63, 64] {
            let span = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
            let base: i64 = rng.gen_range(-1000..1000);
            let keys: Vec<Option<i64>> = (0..300)
                .map(|i| {
                    let x = if i % 7 == 0 { span } else { rng.gen::<u64>() & span };
                    if i % 11 == 5 {
                        None
                    } else {
                        Some(base.wrapping_add(x as i64))
                    }
                })
                .collect();
            let b = roundtrip(&keys);
            assert!(b.is_view());
            for (d, &k) in keys.iter().enumerate() {
                assert_eq!(b.get(d as u32), k, "width {width} doc {d}");
            }
            let mut out = [0u32; BLOCK];
            for blk in 0..b.n_blocks() {
                let Some(base) = b.decode_block_u32(blk, &mut out) else {
                    assert!(width > 32);
                    continue;
                };
                for j in 0..BLOCK.min(keys.len() - blk * BLOCK) {
                    if let Some(k) = keys[blk * BLOCK + j] {
                        assert_eq!(base + out[j] as i64, k);
                    }
                }
            }
        }
    }

    #[test]
    fn mask_kernel_matches_scalar() {
        let mut rng = StdRng::seed_from_u64(2);
        for _ in 0..2000 {
            let mut d = [0u32; BLOCK];
            let max = if rng.gen_bool(0.5) { 100 } else { u32::MAX };
            for x in d.iter_mut() {
                *x = rng.gen_range(0..=max);
            }
            let lo = rng.gen_range(0..=max);
            let span = rng.gen_range(0..=max);
            assert_eq!(range_mask128(&d, lo, span), range_mask128_scalar(&d, lo, span));
        }
    }
}
