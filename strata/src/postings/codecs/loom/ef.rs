//! Elias-Fano partitions stored inline in a list's word array.
//!
//! An EF partition of `n` values in `[0, span)` occupies, from a word-aligned
//! offset:
//!
//! * the `n * l` low bits, packed (`l = floor(log2(span / n))`);
//! * the high bitvector (word-aligned): value `i` sets bit `(v_i >> l) + i`,
//!   so it holds `n` ones and `buckets = ((span - 1) >> l) + 1` zeros;
//! * select0 samples (word-aligned): the position of every
//!   [`SAMPLE`]-th zero as a partition-local `u16` (`n + buckets <= 3 * 4096`).
//!
//! This is [`crate::postings::codecs::elias_fano::EfSequence`] laid out flat, with
//! 16-bit instead of 32-bit select samples. [`EfCur`] is a copy of
//! `EfCursor` over that layout.

use crate::postings::bits::select_in_word;

/// Zeros between consecutive select0 samples. A cursor jumps with a sample
/// only when the target bucket is more than this many buckets ahead.
pub const SAMPLE: u64 = 256;

/// Largest supported partition (high bitvector positions must fit `u16`).
pub const MAX_N: u64 = 4096;

/// Spare output slots [`EfCur::decode`] may write past its values.
pub const DECODE_SLACK: usize = 16;

/// Fewer values than this (one 16-value block) are decoded by the scalar
/// loop on aarch64: the NEON decoder's fixed cost is about 14 scalar values.
#[cfg(target_arch = "aarch64")]
const NEON_MIN: usize = 16;

/// `floor(log2(span / n))` for `span >= n >= 1`, branch-free.
#[inline(always)]
pub fn lower_bits(span: u64, n: u64) -> u32 {
    debug_assert!(span >= n && n >= 1);
    let l = n.leading_zeros() - span.leading_zeros();
    l - ((n << l > span) as u32)
}

/// Payload size in bits of `n >= 1` values spanning `span >= n`: low bits,
/// high bits and select samples. Branch-free (used inside the DP).
#[inline(always)]
pub fn cost_bits(span: u64, n: u64) -> u64 {
    let l = lower_bits(span, n);
    let buckets = ((span - 1) >> l) + 1;
    n * l as u64 + n + buckets + 16 * ((buckets - 1) / SAMPLE)
}

/// Word layout of an EF partition.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub l: u32,
    pub low_words: usize,
    pub high_bits: usize,
    pub high_words: usize,
    pub samples: usize,
}

impl Layout {
    #[inline(always)]
    pub fn new(span: u64, n: u64) -> Self {
        let l = lower_bits(span, n);
        let buckets = ((span - 1) >> l) + 1;
        let high_bits = (n + buckets) as usize;
        Self {
            l,
            low_words: (n as usize * l as usize).div_ceil(64),
            high_bits,
            high_words: high_bits.div_ceil(64),
            samples: ((buckets - 1) / SAMPLE) as usize,
        }
    }

    /// Words occupied in memory.
    pub fn words(&self) -> usize {
        self.low_words + self.high_words + self.samples.div_ceil(4)
    }
}

/// Append the EF encoding of `values[i] - base` (all in `[base, base + span)`)
/// to `words`; returns the payload size in bits ([`cost_bits`]).
pub fn encode(values: &[u32], base: u32, span: u64, words: &mut Vec<u64>) -> u64 {
    let n = values.len() as u64;
    assert!(n >= 1 && n <= MAX_N && span >= n && span <= 1 << 32);
    let lay = Layout::new(span, n);
    let off = words.len();
    words.resize(off + lay.words(), 0);
    let (low, rest) = words[off..].split_at_mut(lay.low_words);
    let (high, samples) = rest.split_at_mut(lay.high_words);
    let l = lay.l as usize;
    let mask = (1u64 << l) - 1;
    for (i, &v) in values.iter().enumerate() {
        debug_assert!(v >= base && ((v - base) as u64) < span);
        let x = (v - base) as u64;
        if l > 0 {
            let bit = i * l;
            let (wi, sh) = (bit / 64, bit % 64);
            low[wi] |= (x & mask) << sh;
            if sh + l > 64 {
                low[wi + 1] |= (x & mask) >> (64 - sh);
            }
        }
        let h = (x >> l) as usize + i;
        high[h / 64] |= 1u64 << (h % 64);
    }
    // Sample s (1-based) is the position of zero number s * SAMPLE.
    let mut zeros = 0u64;
    let mut s = 0usize;
    'outer: for (wi, &w) in high.iter().enumerate() {
        let valid = (lay.high_bits - wi * 64).min(64);
        let mut z = !w & if valid == 64 { !0 } else { (1u64 << valid) - 1 };
        while z != 0 {
            if s == lay.samples {
                break 'outer;
            }
            let c = z.count_ones() as u64;
            let want = (s as u64 + 1) * SAMPLE;
            if zeros + c > want {
                let k = (want - zeros) as u32;
                let pos = wi * 64 + select_in_word(z, k) as usize;
                debug_assert!(pos < u16::MAX as usize);
                samples[s / 4] |= (pos as u64) << (16 * (s % 4));
                s += 1;
                // Keep scanning the same word: clear the zeros up to `pos`.
                let cut = pos % 64 + 1;
                zeros += k as u64 + 1;
                z = if cut == 64 { 0 } else { z & (!0u64 << cut) };
            } else {
                zeros += c;
                z = 0;
            }
        }
    }
    debug_assert_eq!(s, lay.samples);
    n * l as u64 + lay.high_bits as u64 + 16 * lay.samples as u64
}

/// Read the `l`-bit low field starting at bit `bit` of the bytes at `low`
/// (`mask = 2^l - 1`). Needs 8 readable bytes from byte `bit / 8`.
#[inline(always)]
unsafe fn read_low(low: *const u8, bit: usize, mask: u64) -> u64 {
    #[cfg(target_endian = "little")]
    {
        // l <= 32 and bit % 8 <= 7, so the field fits in one 64-bit load.
        (low.add(bit / 8).cast::<u64>().read_unaligned() >> (bit % 8)) & mask
    }
    #[cfg(not(target_endian = "little"))]
    {
        let p = low.cast::<u64>();
        let (wi, off) = (bit / 64, bit % 64);
        let lo = p.add(wi).read() >> off;
        let hi = (p.add(wi + 1).read() << 1) << (63 - off);
        (lo | hi) & mask
    }
}

/// A readable all-zero stand-in for unused cursor pointers.
pub static ZEROS: [u64; 4] = [0; 4];

/// Forward cursor over an EF partition, returning values relative to the
/// partition base. A new cursor sits *before* the first value; the caller
/// never moves it past the last value.
#[derive(Clone, Copy, Debug)]
pub struct EfCur {
    low: *const u8,
    high: *const u64,
    samples: *const u16,
    l: u32,
    mask: u64,
    /// Index of the current value (`usize::MAX` before the first).
    pub idx: usize,
    /// High word holding the current one bit (`usize::MAX` before the first)...
    wi: usize,
    /// ...and the ones of that word after it.
    buf: u64,
    /// Current value (0 before the first).
    pub val: u64,
}

impl EfCur {
    /// A cursor that must never be used (placeholder for non-EF partitions).
    #[inline(always)]
    pub fn dummy() -> Self {
        let p = ZEROS.as_ptr();
        Self { low: p.cast(), high: p, samples: p.cast(), l: 0, mask: 0, idx: usize::MAX, wi: usize::MAX, buf: 0, val: 0 }
    }

    /// Cursor over the partition encoded at `p` with layout `lay`.
    ///
    /// # Safety
    /// `p` must point at a partition written by [`encode`] with this layout,
    /// followed by at least one readable word.
    #[inline(always)]
    pub unsafe fn new(p: *const u64, lay_l: u32, low_words: usize, high_words: usize) -> Self {
        let high = p.add(low_words);
        Self {
            low: p.cast(),
            high,
            samples: high.add(high_words).cast(),
            l: lay_l,
            mask: (1u64 << lay_l) - 1,
            idx: usize::MAX,
            wi: usize::MAX,
            buf: 0,
            val: 0,
        }
    }

    /// Move to the next value and return it.
    ///
    /// # Safety
    /// The cursor must not be on the last value.
    #[inline(always)]
    pub unsafe fn next(&mut self) -> u64 {
        self.idx = self.idx.wrapping_add(1);
        let mut buf = self.buf;
        while buf == 0 {
            self.wi = self.wi.wrapping_add(1);
            buf = *self.high.add(self.wi);
        }
        self.buf = buf & (buf - 1);
        let hpos = self.wi * 64 + buf.trailing_zeros() as usize;
        let lo = read_low(self.low, self.idx * self.l as usize, self.mask);
        self.val = (((hpos - self.idx) as u64) << self.l) | lo;
        self.val
    }

    /// Move to the first value `>= target` and return it.
    ///
    /// # Safety
    /// `target` must be at most the last value, and greater than the current
    /// value unless the cursor is before the first value.
    #[inline(always)]
    pub unsafe fn next_geq(&mut self, target: u64) -> u64 {
        let hb = target >> self.l;
        let cur_hb = self.val >> self.l;
        let diff = hb - cur_hb;
        if diff > 1 {
            // The bit after the current one (bit 0 before the first value).
            let start = self.idx.wrapping_add(cur_hb as usize).wrapping_add(1);
            let pos = bucket_start(self.high, self.samples, start, hb, diff);
            // `pos - hb` ones precede `pos`, so the next value has that index.
            self.idx = pos.wrapping_sub(hb as usize).wrapping_sub(1);
            self.wi = pos / 64;
            self.buf = *self.high.add(self.wi) & (!0u64 << (pos % 64));
        }
        loop {
            let v = self.next();
            if v >= target {
                return v;
            }
        }
    }

    /// Decode the `count >= 1` values after the current one, writing
    /// `add + value` at `out`; leaves the cursor on the last of them.
    ///
    /// With AVX-512 VBMI and `l <= 25`, 16 values at a time: the high bits'
    /// one positions are extracted with `vpcompressd` (as bitmaps are), and
    /// the low fields unpacked with one `vpermb` (each lane gathers the four
    /// bytes holding its field) and a variable shift. NEON (`l <= 25`, at
    /// least [`NEON_MIN`] values) does the same with its bitmap extraction
    /// and, per 4 values, one `tbl` over 16 bytes and a variable shift
    /// (`ushl`).
    ///
    /// # Safety
    /// At least `count` values follow the current one; `out` has room for
    /// `count + DECODE_SLACK` values.
    #[inline]
    pub unsafe fn decode(&mut self, count: usize, add: u32, out: *mut u32) {
        debug_assert!(count >= 1);
        #[cfg(target_arch = "x86_64")]
        if self.l <= 25 && crate::simd::avx512vbmi() {
            return self.decode_avx512(count, add, out);
        }
        #[cfg(target_arch = "aarch64")]
        if self.l <= 25 && count >= NEON_MIN {
            return self.decode_neon(count, add, out);
        }
        self.decode_scalar(count, add, out);
    }

    /// Portable [`EfCur::decode`].
    #[inline(always)]
    unsafe fn decode_scalar(&mut self, count: usize, add: u32, out: *mut u32) {
        let mut o = out;
        self.for_each_until(self.idx.wrapping_add(1) + count, |v| {
            o.write(add.wrapping_add(v as u32));
            o = o.add(1);
        });
    }

    #[cfg(target_arch = "aarch64")]
    #[inline(never)]
    unsafe fn decode_neon(&mut self, count: usize, add: u32, out: *mut u32) {
        use std::arch::aarch64::*;
        const CH: usize = 256;
        // Not zeroed (a 1.5 KiB memset per call): every slot read is
        // written first, see below.
        let mut buf = std::mem::MaybeUninit::<[u32; CH + 2 * super::simd::SLACK]>::uninit();
        let pos = buf.as_mut_ptr().cast::<u32>();
        let l = self.l;
        let iota = vld1q_u32([0, 1, 2, 3].as_ptr());
        let vl = vdupq_n_s32(l as i32);
        let vmask = vdupq_n_u32(self.mask as u32);
        let vadd = vdupq_n_u32(add);
        let mut done = 0;
        while done < count {
            let m = (count - done).min(CH);
            let idx0 = self.idx.wrapping_add(1);
            // 1. Positions of the next m ones of the high bits.
            let mut np = 0usize;
            let mut wi = self.wi;
            let mut w = self.buf;
            loop {
                np = super::simd::extract_word(w, (wi as u32).wrapping_mul(64), pos.add(np)).offset_from(pos) as usize;
                if np >= m {
                    break;
                }
                wi = wi.wrapping_add(1);
                w = *self.high.add(wi);
            }
            // The blocks below read up to 15 slots past m <= np.
            vst1q_u32_x4(pos.add(np), uint32x4x4_t(vdupq_n_u32(0), vdupq_n_u32(0), vdupq_n_u32(0), vdupq_n_u32(0)));
            let plast = *pos.add(m - 1) as usize;
            self.wi = plast / 64;
            self.buf = *self.high.add(self.wi) & ((!0u64 << (plast % 64)) << 1);
            // 2. Low fields, 16 values per block as in the AVX-512 decoder
            // (a block starting at bit B reads from byte B / 8, B % 8 being
            // the same for every block). Group g of a block (values 4g..4g+4)
            // starts at bit s0 + 4gl of those bytes and reads the 16 bytes
            // from byte gb[g] (enough for l <= 25); its lane j gathers with
            // `tbl` the four bytes from bit r = (s0 + 4gl) % 8 + j * l of
            // them and shifts right by r % 8.
            let s0 = ((idx0 * l as usize) % 8) as u32;
            let mut gb = [0usize; 4];
            let mut perm = [vdupq_n_u8(0); 4];
            let mut shift = [vdupq_n_s32(0); 4];
            for g in 0..4 {
                let start = s0 + 4 * g as u32 * l;
                gb[g] = (start / 8) as usize;
                let r = vmlaq_n_u32(vdupq_n_u32(start % 8), iota, l);
                let byte = vshrq_n_u32::<3>(r);
                perm[g] = vreinterpretq_u8_u32(vmlaq_n_u32(vdupq_n_u32(0x0302_0100), byte, 0x0101_0101));
                shift[g] = vnegq_s32(vreinterpretq_s32_u32(vandq_u32(r, vdupq_n_u32(7))));
            }
            let mut t = 0;
            while t < m {
                let i = idx0 + t;
                let low = self.low.add(i * l as usize / 8);
                let mut ib = vaddq_u32(vdupq_n_u32(i as u32), iota);
                for g in 0..4 {
                    let data = vld1q_u8(low.add(gb[g]));
                    let lo = vandq_u32(vshlq_u32(vreinterpretq_u32_u8(vqtbl1q_u8(data, perm[g])), shift[g]), vmask);
                    let hi = vsubq_u32(vld1q_u32(pos.add(t + 4 * g)), ib);
                    let v = vorrq_u32(vshlq_u32(hi, vl), lo);
                    vst1q_u32(out.add(done + t + 4 * g), vaddq_u32(v, vadd));
                    ib = vaddq_u32(ib, vdupq_n_u32(4));
                }
                t += 16;
            }
            self.idx = idx0 + m - 1;
            done += m;
            self.val = out.add(done - 1).read().wrapping_sub(add) as u64;
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512vbmi")]
    #[inline(never)]
    unsafe fn decode_avx512(&mut self, count: usize, add: u32, out: *mut u32) {
        use std::arch::x86_64::*;
        const CH: usize = 256;
        let mut pos = [0u32; CH + 2 * super::simd::SLACK];
        let l = self.l;
        let iota = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        let vl = _mm512_set1_epi32(l as i32);
        let vmask = _mm512_set1_epi32(self.mask as i32);
        let vadd = _mm512_set1_epi32(add as i32);
        let mut done = 0;
        while done < count {
            let m = (count - done).min(CH);
            let idx0 = self.idx.wrapping_add(1);
            // 1. Positions of the next m ones of the high bits.
            let mut np = 0usize;
            let mut wi = self.wi;
            let mut w = self.buf;
            loop {
                let dst = pos.as_mut_ptr().add(np);
                np = super::simd::extract_word(w, (wi as u32).wrapping_mul(64), dst).offset_from(pos.as_ptr()) as usize;
                if np >= m {
                    break;
                }
                wi = wi.wrapping_add(1);
                w = *self.high.add(wi);
            }
            let plast = pos[m - 1] as usize;
            self.wi = plast / 64;
            self.buf = *self.high.add(self.wi) & ((!0u64 << (plast % 64)) << 1);
            // 2. Low fields: lane j of a block starting at bit B reads bit
            // r_j = B % 8 + j * l of the 64 bytes from byte B / 8 (B % 8 is
            // the same for every block: a block spans 16 l bits).
            let s0 = ((idx0 * l as usize) % 8) as i32;
            let r = _mm512_add_epi32(_mm512_set1_epi32(s0), _mm512_mullo_epi32(iota, vl));
            let byte = _mm512_srli_epi32::<3>(r);
            let perm = _mm512_add_epi32(_mm512_mullo_epi32(byte, _mm512_set1_epi32(0x0101_0101)), _mm512_set1_epi32(0x0302_0100));
            let shift = _mm512_and_si512(r, _mm512_set1_epi32(7));
            let mut t = 0;
            while t < m {
                let i = idx0 + t;
                let data = _mm512_loadu_si512(self.low.add(i * l as usize / 8).cast());
                let lo = _mm512_and_si512(_mm512_srlv_epi32(_mm512_permutexvar_epi8(perm, data), shift), vmask);
                let hp = _mm512_loadu_si512(pos.as_ptr().add(t).cast());
                let hi = _mm512_sub_epi32(hp, _mm512_add_epi32(_mm512_set1_epi32(i as i32), iota));
                let v = _mm512_or_si512(_mm512_sllv_epi32(hi, vl), lo);
                _mm512_storeu_si512(out.add(done + t).cast(), _mm512_add_epi32(v, vadd));
                t += 16;
            }
            self.idx = idx0 + m - 1;
            done += m;
            self.val = out.add(done - 1).read().wrapping_sub(add) as u64;
        }
    }

    /// Decode the values after the current one up to index `end` (exclusive),
    /// calling `f(value)` for each; leaves the cursor on the last one.
    ///
    /// # Safety
    /// `end <= n`, and `end > idx + 1` (at least one value is decoded).
    #[inline(always)]
    pub unsafe fn for_each_until(&mut self, end: usize, mut f: impl FnMut(u64)) {
        let (low, l, mask) = (self.low, self.l as usize, self.mask);
        let mut idx = self.idx.wrapping_add(1);
        let mut wi = self.wi;
        let mut buf = self.buf;
        let mut hpos;
        let mut v;
        loop {
            while buf == 0 {
                wi = wi.wrapping_add(1);
                buf = *self.high.add(wi);
            }
            hpos = wi * 64 + buf.trailing_zeros() as usize;
            buf &= buf - 1;
            v = (((hpos - idx) as u64) << l) | read_low(low, idx * l, mask);
            f(v);
            idx += 1;
            if idx >= end {
                break;
            }
        }
        self.idx = idx - 1;
        self.wi = wi;
        self.buf = buf;
        self.val = v;
    }
}

/// First bit of bucket `hb` of the high bitvector `high`, reached by
/// passing `diff > 0` zeros from bit `start` (the bit after the one bit just
/// before it, or the first bit of the bucket `hb - diff`). Jumps from a
/// select0 sample when `diff > SAMPLE`.
///
/// Out of line: it runs only for long skips.
#[inline(never)]
unsafe fn bucket_start(high: *const u64, samples: *const u16, start: usize, hb: u64, diff: u64) -> usize {
    let (from, mut k) = if diff > SAMPLE {
        // select0(hb - 1) + 1, from the sample at or before zero hb - 1.
        let z = hb - 1;
        let s = (z / SAMPLE) as usize;
        debug_assert!(s >= 1);
        let p = *samples.add(s - 1) as usize;
        let r = z - s as u64 * SAMPLE;
        if r == 0 {
            return p + 1;
        }
        (p + 1, r)
    } else {
        (start, diff)
    };
    let mut wi = from / 64;
    let mut zeros = !*high.add(wi) & (!0u64 << (from % 64));
    loop {
        let c = zeros.count_ones() as u64;
        if k <= c {
            return wi * 64 + select_in_word(zeros, (k - 1) as u32) as usize + 1;
        }
        k -= c;
        wi += 1;
        zeros = !*high.add(wi);
    }
}

/// Position of the `k`-th set bit of `w` when `k < w.count_ones()`; some
/// value in `0..=64` otherwise (never panics, for branch-free selection).
/// On aarch64 the broadword select ([`crate::postings::bits::select_in_word_neon`])
/// runs without a branch and is clamped: 64 past the last set bit, as the
/// fallback and `pdep` give.
#[inline(always)]
fn select_unchecked(w: u64, k: u64) -> usize {
    #[cfg(target_arch = "x86_64")]
    if crate::simd::bmi2() {
        // SAFETY: the CPU has BMI2.
        return unsafe { pdep_select(w, k) };
    }
    #[cfg(target_arch = "aarch64")]
    {
        crate::postings::bits::select_in_word_neon(w, k.min(64) as u32).min(64) as usize
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        if k < w.count_ones() as u64 {
            select_in_word(w, k as u32) as usize
        } else {
            64
        }
    }
}

/// [`select_unchecked`] with `pdep` (64 when `w` has `k` or fewer set bits).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
#[inline]
unsafe fn pdep_select(w: u64, k: u64) -> usize {
    core::arch::x86_64::_pdep_u64(1u64.wrapping_shl(k as u32), w).trailing_zeros() as usize
}

/// Position of zero number `r` (0-based) of the high bitvector `high` at or
/// after bit `from`. With AVX-512 (VPOPCNTDQ) the zeros of the 8 words from
/// `from` are counted at once and prefix-summed, so up to 8 words are
/// searched without branches; further words (rare: at least 200-odd zeros
/// lie in 8 words of an EF high bitvector) are scanned one by one. NEON does
/// the same with byte counts (`cnt`, pairwise-added to one byte per word)
/// and a saturating byte prefix sum (`r < 256`).
///
/// # Safety
/// The zero must exist, and 8 words from `from / 64` must be readable.
#[inline(always)]
unsafe fn select0_from(high: *const u64, from: usize, r: u64) -> usize {
    #[cfg(target_arch = "x86_64")]
    if crate::simd::avx512vpopcntdq() {
        return select0_from_avx512(high, from, r);
    }
    #[cfg(target_arch = "aarch64")]
    {
        select0_from_neon(high, from, r)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        select0_from_scalar(high, from, r)
    }
}

/// [`select0_from`] with AVX-512 VPOPCNTDQ.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512vpopcntdq")]
#[inline]
unsafe fn select0_from_avx512(high: *const u64, from: usize, r: u64) -> usize {
    use std::arch::x86_64::*;
    let wi = from / 64;
    let first = _mm512_mask_set1_epi64(_mm512_set1_epi64(-1), 1, (!0u64 << (from % 64)) as i64);
    let zv = _mm512_andnot_si512(_mm512_loadu_si512(high.add(wi).cast()), first);
    let cnt = _mm512_popcnt_epi64(zv);
    let zero = _mm512_setzero_si512();
    let mut pre = _mm512_add_epi64(cnt, _mm512_alignr_epi64::<7>(cnt, zero));
    pre = _mm512_add_epi64(pre, _mm512_alignr_epi64::<6>(pre, zero));
    pre = _mm512_add_epi64(pre, _mm512_alignr_epi64::<4>(pre, zero));
    let m = _mm512_cmpgt_epu64_mask(pre, _mm512_set1_epi64(r as i64));
    if m != 0 {
        let j = _mm512_set1_epi64(m.trailing_zeros() as i64);
        let before = _mm_cvtsi128_si64(_mm512_castsi512_si128(_mm512_permutexvar_epi64(j, _mm512_sub_epi64(pre, cnt)))) as u64;
        let w = _mm_cvtsi128_si64(_mm512_castsi512_si128(_mm512_permutexvar_epi64(j, zv))) as u64;
        return (wi + m.trailing_zeros() as usize) * 64 + select_in_word(w, (r - before) as u32) as usize;
    }
    bucket_start(high, std::ptr::null(), from, 0, r + 1) - 1
}

/// Portable [`select0_from`].
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
#[inline(always)]
unsafe fn select0_from_scalar(high: *const u64, from: usize, r: u64) -> usize {
    // `bucket_start` passes r + 1 zeros from `from` (never via samples
    // here: r < SAMPLE).
    bucket_start(high, std::ptr::null(), from, 0, r + 1) - 1
}

/// NEON [`select0_from`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn select0_from_neon(high: *const u64, from: usize, r: u64) -> usize {
    use std::arch::aarch64::*;
    debug_assert!(r < SAMPLE);
    let wi = from / 64;
    let first = !0u64 << (from % 64);
    let v = vld1q_u8_x4(high.add(wi).cast());
    let z0 = vbicq_u8(vreinterpretq_u8_u64(vsetq_lane_u64::<0>(first, vdupq_n_u64(!0))), v.0);
    // Zeros per word, one byte per word (at most 64).
    let c = vpaddq_u8(
        vpaddq_u8(vcntq_u8(z0), vcntq_u8(vmvnq_u8(v.1))),
        vpaddq_u8(vcntq_u8(vmvnq_u8(v.2)), vcntq_u8(vmvnq_u8(v.3))),
    );
    let cnt = vget_low_u8(vpaddq_u8(c, c));
    // Inclusive prefix sums, saturating: a lane that saturates holds 255 or
    // more zeros, and r <= 255 falls through to the scan when it does.
    let zero = vdup_n_u8(0);
    let mut pre = vqadd_u8(cnt, vext_u8::<7>(zero, cnt));
    pre = vqadd_u8(pre, vext_u8::<6>(zero, pre));
    pre = vqadd_u8(pre, vext_u8::<4>(zero, pre));
    let m = vget_lane_u64::<0>(vreinterpret_u64_u8(vcgt_u8(pre, vdup_n_u8(r as u8))));
    if m != 0 {
        let j = (m.trailing_zeros() / 8) as usize;
        // Zeros before word j (exact: at most r).
        let before = (vget_lane_u64::<0>(vreinterpret_u64_u8(pre)) << 8 >> (8 * j)) & 0xff;
        let keep = if j == 0 { first } else { !0 };
        let w = !*high.add(wi + j) & keep;
        return (wi + j) * 64 + select_in_word(w, (r - before) as u32) as usize;
    }
    bucket_start(high, std::ptr::null(), from, 0, r + 1) - 1
}

/// Membership probe over an EF partition for non-decreasing targets.
///
/// Unlike [`EfCur`], which sits on a value, the probe sits on a bucket: its
/// state is a bucket `hb` and the position of the bucket's first bit in the
/// high bitvector. A probe skips to the target's bucket (within one word
/// inline, else via [`bucket_start`]) and compares the target's low bits with
/// the bucket's first two elements without branching (buckets hold at most
/// one element on average).
#[derive(Clone, Copy, Debug)]
pub struct EfProbe {
    low: *const u8,
    high: *const u64,
    samples: *const u16,
    l: u32,
    mask: u64,
    hb: u64,
    pos: usize,
}

impl EfProbe {
    /// A probe at bucket 0 of the partition of a fresh cursor `c`.
    #[inline(always)]
    pub fn new(c: &EfCur) -> Self {
        EfProbe { low: c.low, high: c.high, samples: c.samples, l: c.l, mask: c.mask, hb: 0, pos: 0 }
    }

    /// Is `rel` a value? Returns that and a lower bound on the first value
    /// `>= rel`, which is `rel` itself iff it is a value, and otherwise a
    /// value or the start of a bucket (never above the last value).
    ///
    /// # Safety
    /// `rel` must be at most the last value and in a bucket at or after the
    /// bucket of every previous target.
    #[inline(always)]
    pub unsafe fn probe(&mut self, rel: u64) -> (bool, u64) {
        let l = self.l;
        let hb = rel >> l;
        let tl = rel & self.mask;
        let d = hb - self.hb;
        // Skip `d` zeros from `pos`: branch-free within this word and the
        // next (the word after the high bitvector is always readable).
        let pos0 = self.pos;
        let wi = pos0 / 64;
        let z0 = !*self.high.add(wi) & (!0u64 << (pos0 % 64));
        let z1 = !*self.high.add(wi + 1);
        let c0 = z0.count_ones() as u64;
        let c1 = z1.count_ones() as u64;
        let p0 = wi * 64 + select_unchecked(z0, d.wrapping_sub(1)) + 1;
        let p1 = wi * 64 + 64 + select_unchecked(z1, d.wrapping_sub(c0 + 1)) + 1;
        let pos = if d > c0 + c1 {
            bucket_start(self.high, self.samples, pos0, hb, d)
        } else {
            let p = if d > c0 { p1 } else { p0 };
            if d == 0 {
                pos0
            } else {
                p
            }
        };
        self.hb = hb;
        self.pos = pos;
        self.check_bucket(hb, tl, pos)
    }

    /// [`EfProbe::probe`] without using (or updating) the probe's position:
    /// the bucket is located from the select0 sample before it, so probes
    /// do not depend on each other and their cache misses overlap. Best when
    /// targets are many buckets apart.
    ///
    /// # Safety
    /// `rel` must be at most the last value.
    #[inline(always)]
    pub unsafe fn probe_far(&self, rel: u64) -> (bool, u64) {
        let hb = rel >> self.l;
        let tl = rel & self.mask;
        // Bucket hb starts after zero number hb - 1: find it from the
        // sample of zero number SAMPLE * s, s = (hb - 1) / SAMPLE.
        let z = hb.max(1) - 1;
        let si = (z / SAMPLE) as usize;
        let from = if si == 0 { 0 } else { *self.samples.add(si - 1) as usize };
        let p = select0_from(self.high, from, z - si as u64 * SAMPLE) + 1;
        let pos = if hb == 0 { 0 } else { p };
        self.check_bucket(hb, tl, pos)
    }

    /// Membership of `hb << l | tl` in bucket `hb`, whose first bit is
    /// `pos` (see [`EfProbe::probe`]).
    #[inline(always)]
    unsafe fn check_bucket(&self, hb: u64, tl: u64, pos: usize) -> (bool, u64) {
        let l = self.l;
        // The bucket's elements are the ones starting at `pos`.
        let s = pos % 64;
        let c = (!(*self.high.add(pos / 64) >> s)).trailing_zeros() as usize;
        let idx = pos - hb as usize;
        if c > 2 || c == 64 - s {
            return self.probe_slow(hb, tl, pos);
        }
        // Reading one element past the last value is harmless (padding).
        let l0 = read_low(self.low, idx * l as usize, self.mask);
        let l1 = read_low(self.low, (idx + 1) * l as usize, self.mask);
        let has0 = c >= 1;
        let has1 = c == 2;
        let found = (has0 & (l0 == tl)) | (has1 & (l1 == tl));
        let hv = hb << l;
        let g1 = has1 & (l1 >= tl);
        let b1 = if g1 { hv | l1 } else { (hb + 1) << l };
        let g0 = has0 & (l0 >= tl);
        let bound = if g0 { hv | l0 } else { b1 };
        (found, bound)
    }

    /// [`EfProbe::probe`] for buckets with more than two elements or
    /// crossing a word.
    #[inline(never)]
    unsafe fn probe_slow(&self, hb: u64, tl: u64, pos: usize) -> (bool, u64) {
        let (mut p, mut idx) = (pos, pos - hb as usize);
        loop {
            if (*self.high.add(p / 64) >> (p % 64)) & 1 == 0 {
                return (false, (hb + 1) << self.l);
            }
            let lo = read_low(self.low, idx * self.l as usize, self.mask);
            if lo >= tl {
                return (lo == tl, hb << self.l | lo);
            }
            p += 1;
            idx += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postings::codecs::conformance::random_sorted;
    use crate::postings::codecs::elias_fano::ef_lower_bits;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn lower_bits_matches() {
        for span in [1u64, 2, 3, 7, 8, 9, 1000, 1 << 20, (1 << 32) - 1, 1 << 32] {
            for n in [1u64, 2, 3, 5, 64, 999, 1000] {
                if n <= span {
                    assert_eq!(lower_bits(span, n), ef_lower_bits(span, n));
                }
            }
        }
    }

    /// An EF partition of `n` random values in `[0, span)` with list-like
    /// padding, and the values.
    #[cfg(target_arch = "aarch64")]
    fn partition(rng: &mut StdRng, n: usize, span: u64) -> (Vec<u64>, Vec<u32>, Layout) {
        // Dense draws are Bernoulli: about n values.
        let mut vals = random_sorted(rng, n, span.min(u32::MAX as u64) as u32);
        vals.truncate(MAX_N as usize);
        if vals.is_empty() {
            vals.push(0);
        }
        let mut words = Vec::new();
        encode(&vals, 0, span, &mut words);
        words.extend([0; 9]); // as the list padding (PAD_WORDS)
        let lay = Layout::new(span, vals.len() as u64);
        (words, vals, lay)
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_decode_matches_scalar() {
        let mut rng = StdRng::seed_from_u64(78);
        // Every l from 0 to 25 and beyond, single values, partial blocks.
        let mut shapes: Vec<(usize, u64)> = vec![(1, 1), (1, 1000), (2, 2), (5, 5), (17, 17), (4096, 4096), (4096, 4097), (4096, 1 << 32)];
        for l in 0..=31u32 {
            for n in [1usize, 15, 16, 17, 255, 256, 257, 1000, 4096] {
                // span / n in [2^l, 2^(l + 1)): l low bits.
                let lo = (n as u64) << l;
                if lo <= 1 << 32 {
                    shapes.push((n, (lo + rng.gen_range(0..lo)).min(1 << 32)));
                }
            }
        }
        for (n, span) in shapes {
            let (words, vals, lay) = partition(&mut rng, n, span);
            let n = vals.len();
            let mk = || unsafe { EfCur::new(words.as_ptr(), lay.l, lay.low_words, lay.high_words) };
            for add in [0u32, 12345, u32::MAX - 7] {
                // Pieces of various sizes from the start (piece 3: from
                // mid-partition, after a next_geq); both decoders agree on
                // output and cursor state.
                for piece in [1usize, 3, 16, 33, 256, 300, 4096] {
                    let (mut a, mut b) = (mk(), mk());
                    let mut done = 0;
                    if n > 2 && piece == 3 {
                        // Start after a next_geq, mid-word.
                        let t = vals[n / 3] as u64;
                        unsafe {
                            a.next_geq(t);
                            b.next_geq(t);
                        }
                        done = a.idx + 1;
                    }
                    while done < n {
                        let m = piece.min(n - done);
                        let mut x = vec![0u32; m + DECODE_SLACK];
                        let mut y = vec![0u32; m + DECODE_SLACK];
                        unsafe {
                            a.decode_scalar(m, add, x.as_mut_ptr());
                            if lay.l <= 25 {
                                b.decode_neon(m, add, y.as_mut_ptr());
                            } else {
                                b.decode(m, add, y.as_mut_ptr());
                            }
                        }
                        assert_eq!(&x[..m], &y[..m], "n={n} span={span} l={} piece={piece} done={done}", lay.l);
                        let exp: Vec<u32> = vals[done..done + m].iter().map(|&v| v.wrapping_add(add)).collect();
                        assert_eq!(&x[..m], &exp[..]);
                        assert_eq!((a.idx, a.val, a.wi, a.buf), (b.idx, b.val, b.wi, b.buf), "cursor state");
                        done += m;
                    }
                    if n > 1 {
                        // The cursors keep working after decoding.
                        let (mut a, mut b) = (mk(), mk());
                        unsafe {
                            a.decode_scalar(n - 1, 0, vec![0u32; n + DECODE_SLACK].as_mut_ptr());
                            b.decode(n - 1, 0, vec![0u32; n + DECODE_SLACK].as_mut_ptr());
                            assert_eq!(a.next(), vals[n - 1] as u64);
                            assert_eq!(b.next(), vals[n - 1] as u64);
                        }
                    }
                }
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_select_matches_scalar() {
        let mut rng = StdRng::seed_from_u64(79);
        // select_unchecked: every k up to past the end, and huge k.
        let scalar = |w: u64, k: u64| {
            if k < w.count_ones() as u64 {
                crate::postings::bits::select_in_word_scalar(w, k as u32) as usize
            } else {
                64
            }
        };
        let mut words: Vec<u64> = vec![0, 1, 1 << 63, !0, 0xff, 0xff << 56, 0x5555_5555_5555_5555];
        words.extend((0..1000).map(|_| rng.gen::<u64>() & rng.gen::<u64>()));
        words.extend((0..1000).map(|_| rng.gen::<u64>() | rng.gen::<u64>()));
        for &w in &words {
            for k in (0..=70).chain([100, 1 << 32, u64::MAX - 1, u64::MAX]) {
                assert_eq!(select_unchecked(w, k), scalar(w, k), "w={w:#x} k={k}");
            }
        }
        // select0_from over high bitvectors (EF ones, and words with few or
        // no zeros, so that the 8-word window can miss), from every bit of
        // the first words, for every r < SAMPLE.
        let mut highs: Vec<Vec<u64>> = Vec::new();
        for (n, span) in [(4096usize, 8192u64), (4096, 4500), (1000, 1_000_000), (300, 300 * 3)] {
            let (words, _, lay) = partition(&mut rng, n, span);
            highs.push(words[lay.low_words..lay.low_words + lay.high_words].to_vec());
        }
        highs.push((0..40).map(|_| !(1u64 << rng.gen_range(0..64))).collect());
        highs.push((0..40).map(|i| if i % 9 == 4 { 0 } else { !0 }).collect());
        highs.push((0..40).map(|_| rng.gen::<u64>() | rng.gen::<u64>() | rng.gen::<u64>()).collect());
        highs.push(vec![0; 16]);
        for mut h in highs {
            // Zeros after the end, as in a list (padding), and enough of them.
            h.extend([0; 16]);
            let zeros_from = |from: usize| -> u64 {
                (from..h.len() * 64).filter(|&b| h[b / 64] >> (b % 64) & 1 == 0).count() as u64
            };
            for from in (0..200.min(h.len() * 64 - 8 * 64)).chain([511, 512, 513]) {
                let avail = zeros_from(from).min(SAMPLE);
                for r in 0..avail {
                    let (x, y) = unsafe { (select0_from_scalar(h.as_ptr(), from, r), select0_from_neon(h.as_ptr(), from, r)) };
                    assert_eq!(x, y, "from={from} r={r}");
                }
            }
        }
    }

    /// NEON vs scalar decoding and probes:
    /// `cargo test --release ef_neon_timing -- --ignored --nocapture`
    #[cfg(target_arch = "aarch64")]
    #[test]
    #[ignore = "timing only"]
    fn ef_neon_timing() {
        use std::hint::black_box;
        use std::time::Instant;
        let best = |f: &mut dyn FnMut()| {
            (0..7)
                .map(|_| {
                    let t = Instant::now();
                    f();
                    t.elapsed().as_secs_f64()
                })
                .fold(f64::MAX, f64::min)
        };
        let mut rng = StdRng::seed_from_u64(5);
        println!("decode, ns per value (64 partitions of 4096 values, decoded in pieces):");
        for span in [8192u64, 65_536, 1 << 20, 1 << 24, 1 << 30] {
            let parts: Vec<_> = (0..64).map(|_| partition(&mut rng, 4096, span)).collect();
            let mut out = vec![0u32; 4096 + DECODE_SLACK];
            let mut row = format!("  l={:>2}:", parts[0].2.l);
            for piece in [1usize, 4, 16, 64, 256, 4096] {
                for neon in [false, true] {
                    let t = best(&mut || {
                        for (w, _, lay) in &parts {
                            let mut c = unsafe { EfCur::new(w.as_ptr(), lay.l, lay.low_words, lay.high_words) };
                            let mut done = 0;
                            while done < 4096 {
                                let m = piece.min(4096 - done);
                                unsafe {
                                    if neon {
                                        c.decode(m, 0, out.as_mut_ptr());
                                    } else {
                                        c.decode_scalar(m, 0, out.as_mut_ptr());
                                    }
                                }
                                done += m;
                            }
                            black_box(&out);
                        }
                    });
                    row += &format!(" {}{piece} {:.2}", if neon { "/" } else { " " }, t * 1e9 / (64.0 * 4096.0));
                }
            }
            println!("{row}  (piece: scalar/dispatch)");
        }
        println!("probe_far (select0_from), ns per probe (4096 values, random targets):");
        for span in [8192u64, 65_536, 1 << 20] {
            let (w, vals, lay) = partition(&mut rng, 4096, span);
            let c = unsafe { EfCur::new(w.as_ptr(), lay.l, lay.low_words, lay.high_words) };
            let last = *vals.last().unwrap() as u64;
            let ts: Vec<u64> = (0..1 << 16).map(|_| rng.gen_range(0..=last)).collect();
            let p = EfProbe::new(&c);
            let high = w[lay.low_words..].as_ptr();
            let froms: Vec<(usize, u64)> = ts
                .iter()
                .map(|&t| {
                    let z = (t >> lay.l).max(1) - 1;
                    let si = (z / SAMPLE) as usize;
                    let from = if si == 0 { 0 } else { unsafe { *p.samples.add(si - 1) as usize } };
                    (from, z - si as u64 * SAMPLE)
                })
                .collect();
            let t_scalar = best(&mut || {
                let mut acc = 0;
                for &(from, r) in &froms {
                    acc += unsafe { select0_from_scalar(high, from, r) };
                }
                black_box(acc);
            });
            let t_neon = best(&mut || {
                let mut acc = 0;
                for &(from, r) in &froms {
                    acc += unsafe { select0_from_neon(high, from, r) };
                }
                black_box(acc);
            });
            let t_probe = best(&mut || {
                let mut hits = 0;
                for &t in &ts {
                    hits += unsafe { p.probe_far(t) }.0 as u32;
                }
                black_box(hits);
            });
            println!(
                "  l={:>2}: select0_from scalar {:.2}, neon {:.2}; probe_far (dispatch) {:.2}",
                lay.l,
                t_scalar * 1e9 / ts.len() as f64,
                t_neon * 1e9 / ts.len() as f64,
                t_probe * 1e9 / ts.len() as f64
            );
        }
    }

    #[test]
    fn encode_and_cursor() {
        let mut rng = StdRng::seed_from_u64(77);
        for (n, span) in [(1usize, 1u64), (1, 1 << 32), (1, 1000), (5, 5), (300, 300), (300, 100_000), (4096, 4096 * 3), (4096, 1 << 30), (2000, 2_000_000), (4096, 70_000)] {
            let base = if span < 1 << 31 { 12345u32 } else { 0 };
            let vals: Vec<u32> = random_sorted(&mut rng, n, span.min(u32::MAX as u64) as u32)
                .into_iter()
                .map(|v| v + base)
                .collect();
            let n = vals.len();
            let mut words = vec![7u64]; // non-zero offset
            let bits = encode(&vals, base, span, &mut words);
            assert_eq!(bits, cost_bits(span, n as u64));
            let lay = Layout::new(span, n as u64);
            assert_eq!(words.len(), 1 + lay.words());
            words.extend([0; 9]); // as the list padding (PAD_WORDS)
            let mk = || unsafe { EfCur::new(words.as_ptr().add(1), lay.l, lay.low_words, lay.high_words) };
            // Sequential.
            let mut c = mk();
            for (i, &v) in vals.iter().enumerate() {
                assert_eq!(unsafe { c.next() }, (v - base) as u64, "i={i}");
            }
            // decode, in pieces of various sizes.
            for piece in [1usize, 7, 16, 100, 300] {
                let mut c = mk();
                let mut got = vec![0u32; n + DECODE_SLACK];
                let mut done = 0;
                while done < n {
                    let m = piece.min(n - done);
                    unsafe { c.decode(m, base, got.as_mut_ptr().add(done)) };
                    done += m;
                    assert_eq!(c.idx, done - 1);
                    assert_eq!(c.val, (vals[done - 1] - base) as u64);
                }
                assert_eq!(&got[..n], &vals[..], "piece {piece}");
            }
            // decode after next_geq, then next.
            if n > 3 {
                let mut c = mk();
                let t = (vals[n / 2] - base) as u64;
                assert_eq!(unsafe { c.next_geq(t) }, t);
                let rest = n - n / 2 - 1;
                if rest > 1 {
                    let mut got = vec![0u32; rest + DECODE_SLACK];
                    unsafe { c.decode(rest - 1, 0, got.as_mut_ptr()) };
                    let exp: Vec<u32> = vals[n / 2 + 1..n - 1].iter().map(|&v| v - base).collect();
                    assert_eq!(&got[..rest - 1], &exp[..]);
                    assert_eq!(unsafe { c.next() }, (vals[n - 1] - base) as u64);
                }
            }
            // for_each_until.
            let mut c = mk();
            let mut got = Vec::new();
            unsafe { c.for_each_until(n, |v| got.push(v as u32 + base)) };
            assert_eq!(got, vals);
            // Membership probes on random monotone targets.
            for _ in 0..20 {
                let mut pr = EfProbe::new(&mk());
                let mut t = 0u64;
                let last = (vals[n - 1] - base) as u64;
                let rels: Vec<u64> = vals.iter().map(|&v| (v - base) as u64).collect();
                loop {
                    if rng.gen_bool(0.5) {
                        t += rng.gen_range(0..=(span / n as u64 * 4).max(1));
                    } else if let Some(&v) = rels.iter().find(|&&v| v > t) {
                        t = v;
                    }
                    if t > last {
                        break;
                    }
                    let next = rels[rels.partition_point(|&v| v < t)];
                    let (found, bound) = unsafe { pr.probe(t) };
                    assert_eq!(unsafe { pr.probe_far(t) }, (found, bound), "probe_far n={n} span={span} t={t}");
                    assert_eq!(found, next == t, "n={n} span={span} t={t}");
                    assert!(bound >= t && bound <= next && (bound == t) == found, "t={t} bound={bound} next={next}");
                }
            }
            // next_geq on random monotone targets.
            for _ in 0..20 {
                let mut c = mk();
                let mut t = 0u64;
                let last = (vals[n - 1] - base) as u64;
                loop {
                    t += rng.gen_range(0..=(span / n as u64 * 20).max(1));
                    if t > last {
                        break;
                    }
                    if c.idx != usize::MAX && t <= c.val {
                        continue;
                    }
                    let exp = vals.iter().map(|&v| (v - base) as u64).find(|&v| v >= t).unwrap();
                    assert_eq!(unsafe { c.next_geq(t) }, exp, "n={n} span={span} t={t}");
                }
            }
        }
    }
}
