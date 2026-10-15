//! Bitmap-to-docID extraction, popcount and word kernels, with AVX-512 and
//! NEON paths and portable fallbacks.
//!
//! The test machine has AVX-512 F/BW/VPOPCNTDQ but not VBMI2, so the byte
//! compress used by Roaring's extractor (`vpcompressb`) is unavailable.
//! [`extract`] instead:
//!
//! * finds the nonzero words 64 at a time (`vptestmq`) and visits only
//!   those, so sparse and empty stretches cost no mispredicted branches;
//! * extracts a word with more than [`SCALAR_MAX`] set bits by compressing
//!   dword lanes (`vpcompressd`, AVX-512F): each 16-bit quarter of the word
//!   selects from the 16 docIDs it covers;
//! * extracts sparser words with the scalar "flatten" loop (simdjson): four
//!   `tzcnt` positions written unconditionally, the output advanced by the
//!   popcount.
//!
//! On the clustered OR workload this is about 1.6x faster than the scalar
//! loop and 1.2x faster than testing every word for zero.
//!
//! NEON (aarch64) has no compress either. Its [`extract`] extracts a word a
//! byte at a time: the byte's bit positions come from a 256-entry table,
//! are widened to docIDs and stored (8 lanes, unconditionally) at the
//! byte's offset, a prefix sum of the byte popcounts (`cnt`). It finds the
//! nonzero words 16 at a time (`cmtst`, narrowed to a nibble mask with
//! `shrn`): a group with at least [`DENSE_GROUP`] of them is extracted word
//! by word without tests, otherwise only its nonzero words are visited, and
//! those with at most [`SCALAR_MAX`] set bits go to the scalar loop. On an
//! M2 this is 3-8x faster than the scalar loop from about 10 bits per word
//! and on mixed bitmaps, and 2x faster on very sparse ones.

/// Spare `u32` slots [`extract`] may write past the last value.
pub const SLACK: usize = 64;

/// Words with at most this many set bits are extracted by the scalar loop.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
const SCALAR_MAX: u32 = 4;
/// Words with at most this many set bits are extracted by the scalar loop.
#[cfg(target_arch = "aarch64")]
const SCALAR_MAX: u32 = 4;
/// NEON: a group of 16 words with at least this many nonzero words is
/// extracted word by word without testing them.
#[cfg(target_arch = "aarch64")]
const DENSE_GROUP: u32 = 12;

/// Write `base + 64 * i + b` for every set bit `b` of `words[i]`, in order,
/// at `dst`; returns the end.
///
/// # Safety
/// `dst` must have room for every value plus [`SLACK`] slots.
#[inline]
pub unsafe fn extract(words: &[u64], base: u32, dst: *mut u32) -> *mut u32 {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    {
        extract_avx512(words, base, dst)
    }
    #[cfg(target_arch = "aarch64")]
    {
        extract_neon(words, base, dst)
    }
    #[cfg(not(any(all(target_arch = "x86_64", target_feature = "avx512f"), target_arch = "aarch64")))]
    {
        extract_scalar(words, base, dst)
    }
}

/// Like [`extract`], then zero `words` (the OR accumulator is cleared as it
/// is drained).
///
/// # Safety
/// As for [`extract`].
#[inline]
pub unsafe fn extract_clear(words: &mut [u64], base: u32, dst: *mut u32) -> *mut u32 {
    let d = extract(words, base, dst);
    words.fill(0);
    d
}

/// Scalar extraction of one word: four positions per round, written
/// unconditionally.
#[inline(always)]
unsafe fn extract_word_scalar(mut w: u64, b: u32, dst: *mut u32) -> *mut u32 {
    let cnt = w.count_ones() as usize;
    let mut p = dst;
    loop {
        for _ in 0..4 {
            p.write(b.wrapping_add(w.trailing_zeros()));
            p = p.add(1);
            w &= w.wrapping_sub(1);
        }
        if w == 0 {
            break;
        }
    }
    dst.add(cnt)
}

/// Extract one word (`base + b` for every set bit `b`). Used by the
/// AVX-512 VBMI and NEON Elias-Fano decoders.
///
/// # Safety
/// `dst` must have room for the word's bits plus [`SLACK`] slots.
#[cfg_attr(
    not(any(
        all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512vbmi"),
        target_arch = "aarch64"
    )),
    allow(dead_code)
)]
#[inline(always)]
pub unsafe fn extract_word(w: u64, base: u32, dst: *mut u32) -> *mut u32 {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    {
        if w.count_ones() <= SCALAR_MAX {
            extract_word_scalar(w, base, dst)
        } else {
            extract_word_compress(w, base, dst)
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if w.count_ones() <= SCALAR_MAX {
            extract_word_scalar(w, base, dst)
        } else {
            extract_word_neon(w, base, dst)
        }
    }
    #[cfg(not(any(all(target_arch = "x86_64", target_feature = "avx512f"), target_arch = "aarch64")))]
    {
        extract_word_scalar(w, base, dst)
    }
}

/// Portable extraction.
///
/// # Safety
/// As for [`extract`].
#[cfg_attr(
    all(any(all(target_arch = "x86_64", target_feature = "avx512f"), target_arch = "aarch64"), not(test)),
    allow(dead_code)
)]
#[inline]
pub unsafe fn extract_scalar(words: &[u64], base: u32, mut dst: *mut u32) -> *mut u32 {
    for (i, &w) in words.iter().enumerate() {
        if w != 0 {
            dst = extract_word_scalar(w, base.wrapping_add((i as u32) << 6), dst);
        }
    }
    dst
}

/// Bitmask of the nonzero words among `p[..n]` (`n <= 64`).
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline(always)]
unsafe fn nonzero_words(p: *const u64, n: usize) -> u64 {
    use std::arch::x86_64::*;
    debug_assert!(n <= 64);
    let mut m = 0u64;
    let full = n / 8;
    for j in 0..full {
        let v = _mm512_loadu_si512(p.add(8 * j).cast());
        m |= (_mm512_test_epi64_mask(v, v) as u64) << (8 * j);
    }
    if n % 8 != 0 {
        let v = _mm512_maskz_loadu_epi64(((1u32 << (n % 8)) - 1) as u8, p.add(8 * full).cast());
        m |= (_mm512_test_epi64_mask(v, v) as u64) << (8 * full);
    }
    m
}

/// Compress-extract one word, 16 lanes per 16-bit quarter.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline(always)]
unsafe fn extract_word_compress(w: u64, b: u32, mut dst: *mut u32) -> *mut u32 {
    use std::arch::x86_64::*;
    let iota = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let s16 = _mm512_set1_epi32(16);
    let v0 = _mm512_add_epi32(_mm512_set1_epi32(b as i32), iota);
    let v1 = _mm512_add_epi32(v0, s16);
    let v2 = _mm512_add_epi32(v1, s16);
    let v3 = _mm512_add_epi32(v2, s16);
    let (m0, m1, m2, m3) = (w as u16, (w >> 16) as u16, (w >> 32) as u16, (w >> 48) as u16);
    _mm512_storeu_si512(dst.cast(), _mm512_maskz_compress_epi32(m0, v0));
    dst = dst.add(m0.count_ones() as usize);
    _mm512_storeu_si512(dst.cast(), _mm512_maskz_compress_epi32(m1, v1));
    dst = dst.add(m1.count_ones() as usize);
    _mm512_storeu_si512(dst.cast(), _mm512_maskz_compress_epi32(m2, v2));
    dst = dst.add(m2.count_ones() as usize);
    _mm512_storeu_si512(dst.cast(), _mm512_maskz_compress_epi32(m3, v3));
    dst.add(m3.count_ones() as usize)
}

/// AVX-512 extraction (see the module documentation).
///
/// # Safety
/// As for [`extract`].
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline]
pub unsafe fn extract_avx512(words: &[u64], base: u32, mut dst: *mut u32) -> *mut u32 {
    let p = words.as_ptr();
    let mut g = 0;
    while g < words.len() {
        let mut m = nonzero_words(p.add(g), (words.len() - g).min(64));
        while m != 0 {
            let i = g + m.trailing_zeros() as usize;
            m &= m - 1;
            let w = *p.add(i);
            let b = base.wrapping_add((i as u32) << 6);
            dst = if w.count_ones() <= SCALAR_MAX { extract_word_scalar(w, b, dst) } else { extract_word_compress(w, b, dst) };
        }
        g += 64;
    }
    dst
}

/// For every byte value, the positions of its set bits, packed to the
/// front (the rest is zero).
#[cfg(target_arch = "aarch64")]
const fn byte_bits() -> [[u8; 8]; 256] {
    let mut t = [[0u8; 8]; 256];
    let mut v = 0;
    while v < 256 {
        let (mut k, mut b) = (0, 0);
        while b < 8 {
            if v >> b & 1 == 1 {
                t[v][k] = b as u8;
                k += 1;
            }
            b += 1;
        }
        v += 1;
    }
    t
}
#[cfg(target_arch = "aarch64")]
static BYTE_BITS: [[u8; 8]; 256] = byte_bits();

/// Bitmask of the nonzero words among `p[..n]` (`n <= 16`), four bits per
/// word (bit `4i` for word `i`).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn nonzero_words_neon(p: *const u64, n: usize) -> u64 {
    use std::arch::aarch64::*;
    debug_assert!(n <= 16);
    if n < 16 {
        let mut m = 0u64;
        for i in 0..n {
            m |= ((*p.add(i) != 0) as u64) << (4 * i);
        }
        return m;
    }
    let (a, b) = (vld1q_u64_x4(p), vld1q_u64_x4(p.add(8)));
    let t = |v: uint64x2_t| vreinterpretq_u32_u64(vtstq_u64(v, v));
    // Narrow the all-ones / zero lanes to one byte per word, then to a
    // nibble per word.
    let h0 = vuzp1q_u16(vreinterpretq_u16_u32(vuzp1q_u32(t(a.0), t(a.1))), vreinterpretq_u16_u32(vuzp1q_u32(t(a.2), t(a.3))));
    let h1 = vuzp1q_u16(vreinterpretq_u16_u32(vuzp1q_u32(t(b.0), t(b.1))), vreinterpretq_u16_u32(vuzp1q_u32(t(b.2), t(b.3))));
    let bytes = vuzp1q_u8(vreinterpretq_u8_u16(h0), vreinterpretq_u8_u16(h1));
    vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(bytes)))) & 0x1111_1111_1111_1111
}

/// Extract one word a byte at a time (see the module documentation).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn extract_word_neon(w: u64, b: u32, dst: *mut u32) -> *mut u32 {
    use std::arch::aarch64::*;
    // Byte j of `pre` counts the set bits of bytes 0..=j (at most 64).
    let pre = vget_lane_u64::<0>(vreinterpret_u64_u8(vcnt_u8(vcreate_u8(w)))).wrapping_mul(0x0101_0101_0101_0101);
    let off = pre << 8;
    let vb = vdupq_n_u32(b);
    for j in 0..8 {
        // Byte j's positions plus 8j, independently of the other bytes.
        let pos = vaddl_u8(vld1_u8(BYTE_BITS.as_ptr().add((w >> (8 * j)) as usize & 0xff).cast()), vdup_n_u8(8 * j as u8));
        let p = dst.add((off >> (8 * j)) as usize & 0xff);
        vst1q_u32(p, vaddw_u16(vb, vget_low_u16(pos)));
        vst1q_u32(p.add(4), vaddw_high_u16(vb, pos));
    }
    dst.add((pre >> 56) as usize)
}

/// NEON extraction (see the module documentation).
///
/// # Safety
/// As for [`extract`].
#[cfg(target_arch = "aarch64")]
#[inline]
pub unsafe fn extract_neon(words: &[u64], base: u32, mut dst: *mut u32) -> *mut u32 {
    let p = words.as_ptr();
    let mut g = 0;
    while g < words.len() {
        let n = (words.len() - g).min(16);
        let mut m = nonzero_words_neon(p.add(g), n);
        if m.count_ones() >= DENSE_GROUP {
            // Mostly nonzero: every word, without branches.
            for i in g..g + n {
                dst = extract_word_neon(*p.add(i), base.wrapping_add((i as u32) << 6), dst);
            }
        } else {
            while m != 0 {
                let i = g + (m.trailing_zeros() / 4) as usize;
                m &= m - 1;
                let w = *p.add(i);
                let b = base.wrapping_add((i as u32) << 6);
                dst = if w.count_ones() <= SCALAR_MAX { extract_word_scalar(w, b, dst) } else { extract_word_neon(w, b, dst) };
            }
        }
        g += 16;
    }
    dst
}

/// Number of set bits in `words`.
#[inline]
pub fn popcount(words: &[u64]) -> u64 {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vpopcntdq"))]
    {
        // SAFETY: guarded by the target features; loads stay inside `words`.
        unsafe {
            use std::arch::x86_64::*;
            let mut acc = _mm512_setzero_si512();
            let chunks = words.chunks_exact(8);
            let rest = chunks.remainder();
            for c in chunks {
                let v = _mm512_loadu_si512(c.as_ptr().cast());
                acc = _mm512_add_epi64(acc, _mm512_popcnt_epi64(v));
            }
            let mut total = _mm512_reduce_add_epi64(acc) as u64;
            for &w in rest {
                total += w.count_ones() as u64;
            }
            total
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        popcount_neon(words)
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vpopcntdq"),
        target_arch = "aarch64"
    )))]
    {
        popcount_scalar(words)
    }
}

/// Portable [`popcount`].
#[cfg_attr(
    any(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vpopcntdq"), target_arch = "aarch64"),
    allow(dead_code)
)]
#[inline]
pub fn popcount_scalar(words: &[u64]) -> u64 {
    words.iter().map(|w| w.count_ones() as u64).sum()
}

/// NEON [`popcount`]: byte counts (`cnt`) of 16 words summed bytewise, then
/// accumulated into 16-bit lanes (`uadalp`), widened every 256 rounds.
#[cfg(target_arch = "aarch64")]
#[inline]
pub fn popcount_neon(words: &[u64]) -> u64 {
    use std::arch::aarch64::*;
    // SAFETY: NEON is baseline on aarch64; loads stay inside `words`.
    unsafe {
        let mut total = vdupq_n_u64(0);
        let (body, rest) = words.split_at(words.len() & !15);
        for run in body.chunks(16 * 256) {
            // A 16-bit lane gains at most 128 per round.
            let mut acc = vdupq_n_u16(0);
            for c in run.chunks_exact(16) {
                let (v, u) = (vld1q_u8_x4(c.as_ptr().cast()), vld1q_u8_x4(c.as_ptr().add(8).cast()));
                let s = vaddq_u8(vaddq_u8(vcntq_u8(v.0), vcntq_u8(v.1)), vaddq_u8(vcntq_u8(v.2), vcntq_u8(v.3)));
                let t = vaddq_u8(vaddq_u8(vcntq_u8(u.0), vcntq_u8(u.1)), vaddq_u8(vcntq_u8(u.2), vcntq_u8(u.3)));
                acc = vpadalq_u8(acc, vaddq_u8(s, t));
            }
            total = vpadalq_u32(total, vpaddlq_u16(acc));
        }
        let mut total = vaddvq_u64(total);
        for &w in rest {
            total += w.count_ones() as u64;
        }
        total
    }
}

/// `dst[i] |= src[i]` (auto-vectorized).
#[inline(always)]
pub fn or_into(dst: &mut [u64], src: &[u64]) {
    debug_assert_eq!(dst.len(), src.len());
    for (d, &s) in dst.iter_mut().zip(src) {
        *d |= s;
    }
}

/// `dst[i] &= src[i]` (auto-vectorized).
#[inline(always)]
pub fn and_into(dst: &mut [u64], src: &[u64]) {
    debug_assert_eq!(dst.len(), src.len());
    for (d, &s) in dst.iter_mut().zip(src) {
        *d &= s;
    }
}

/// Set bits `[lo, hi]` (inclusive) of `words`.
#[inline]
pub fn set_range(words: &mut [u64], lo: usize, hi: usize) {
    debug_assert!(lo <= hi && hi / 64 < words.len());
    let (a, b) = (lo / 64, hi / 64);
    let first = !0u64 << (lo % 64);
    let last = !0u64 >> (63 - hi % 64);
    if a == b {
        words[a] |= first & last;
    } else {
        words[a] |= first;
        words[a + 1..b].fill(!0);
        words[b] |= last;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn extraction_matches_naive() {
        let mut rng = StdRng::seed_from_u64(9);
        for density in [0.0, 0.01, 0.05, 0.2, 0.5, 0.9, 1.0] {
            for len in [0usize, 1, 7, 63, 64, 65, 1000] {
                let words: Vec<u64> = (0..len)
                    .map(|_| (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(density) as u64) << b)))
                    .collect();
                let base = rng.gen_range(0..1u32 << 20) << 6;
                let mut expected = Vec::new();
                for (i, &w) in words.iter().enumerate() {
                    for b in 0..64 {
                        if w >> b & 1 == 1 {
                            expected.push(base + 64 * i as u32 + b);
                        }
                    }
                }
                for f in [extract, extract_scalar] {
                    let mut out: Vec<u32> = Vec::with_capacity(expected.len() + SLACK);
                    unsafe {
                        let end = f(&words, base, out.as_mut_ptr());
                        out.set_len(end.offset_from(out.as_ptr()) as usize);
                    }
                    assert_eq!(out, expected, "density {density} len {len}");
                }
                assert_eq!(popcount(&words), expected.len() as u64);
            }
        }
    }

    /// Single words of every shape: empty, single bits, whole bytes, all
    /// ones, alternating bits, and random words of every popcount.
    #[cfg(target_arch = "aarch64")]
    fn edge_words(rng: &mut StdRng) -> Vec<u64> {
        let mut v = vec![
            0,
            1,
            1 << 63,
            !0,
            0xff,
            0xff << 56,
            0x8000_0000_0000_0001,
            0x5555_5555_5555_5555,
            0xaaaa_aaaa_aaaa_aaaa,
            0x0f0f_0f0f_0f0f_0f0f,
            0x00ff_00ff_00ff_00ff,
            !0 >> 1,
            !1,
        ];
        for b in 0..64 {
            v.push(1 << b);
            v.push(!(1 << b));
        }
        for c in 0..=64 {
            // A random word with c set bits.
            let mut w = 0u64;
            while w.count_ones() < c {
                w |= 1 << rng.gen_range(0..64);
            }
            v.push(w);
        }
        v
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_matches_scalar() {
        let mut rng = StdRng::seed_from_u64(10);
        let run = |f: unsafe fn(&[u64], u32, *mut u32) -> *mut u32, words: &[u64], base: u32| {
            let n = popcount_scalar(words) as usize;
            let mut out: Vec<u32> = Vec::with_capacity(n + SLACK);
            unsafe {
                let end = f(words, base, out.as_mut_ptr());
                out.set_len(end.offset_from(out.as_ptr()) as usize);
            }
            out
        };
        // One word at a time (bases near u32::MAX wrap as the scalar code's).
        let words = edge_words(&mut rng);
        for &w in &words {
            for base in [0u32, 12345 << 6, u32::MAX - 63] {
                let mut a = vec![0u32; 64 + SLACK];
                let mut b = vec![0u32; 64 + SLACK];
                unsafe {
                    let na = extract_word_scalar(w, base, a.as_mut_ptr()).offset_from(a.as_ptr()) as usize;
                    let nb = extract_word_neon(w, base, b.as_mut_ptr()).offset_from(b.as_ptr()) as usize;
                    assert_eq!(&a[..na], &b[..nb], "word {w:#x} base {base}");
                    let nd = extract_word(w, base, b.as_mut_ptr()).offset_from(b.as_ptr()) as usize;
                    assert_eq!(&a[..na], &b[..nd], "dispatch: word {w:#x} base {base}");
                }
                let one = [w];
                assert_eq!(run(extract_neon, &one, base), run(extract_scalar, &one, base));
            }
        }
        // Bitmaps: the edge words in order, then random ones with runs of
        // empty and full words, at lengths around the 16-word groups.
        assert_eq!(run(extract_neon, &words, 64), run(extract_scalar, &words, 64));
        assert_eq!(popcount_neon(&words), popcount_scalar(&words));
        for len in [0usize, 1, 2, 7, 8, 15, 16, 17, 31, 32, 33, 64, 100, 1023, 1024, 1025] {
            for density in [0.0, 0.01, 0.07, 0.2, 0.5, 0.95, 1.0, -1.0] {
                let words: Vec<u64> = (0..len)
                    .map(|_| {
                        let d = if density < 0.0 { [0.0, 0.0, 0.03, 0.5, 1.0][rng.gen_range(0..5)] } else { density };
                        (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(d) as u64) << b))
                    })
                    .collect();
                let base = rng.gen_range(0..1u32 << 20) << 6;
                assert_eq!(run(extract_neon, &words, base), run(extract_scalar, &words, base), "len {len} density {density}");
                assert_eq!(popcount_neon(&words), popcount_scalar(&words), "len {len} density {density}");
            }
        }
        // Long enough for the popcount's 16-bit lanes to be widened.
        for len in [16 * 256 - 1, 16 * 256, 16 * 256 + 17, 3 * 16 * 256 + 5] {
            let words = vec![!0u64; len];
            assert_eq!(popcount_neon(&words), 64 * len as u64);
        }
    }

    /// NEON vs scalar kernels:
    /// `cargo test --release neon_kernel_timing -- --ignored --nocapture`
    #[cfg(target_arch = "aarch64")]
    #[test]
    #[ignore = "timing only"]
    fn neon_kernel_timing() {
        use std::hint::black_box;
        use std::time::Instant;
        let mut rng = StdRng::seed_from_u64(1);
        let best = |mut f: Box<dyn FnMut() + '_>| {
            (0..7)
                .map(|_| {
                    let t = Instant::now();
                    f();
                    t.elapsed().as_secs_f64()
                })
                .fold(f64::MAX, f64::min)
        };
        let mut out = vec![0u32; 1024 * 64 + SLACK];
        println!("extraction, ns per word (64 distinct 1024-word bitmaps):");
        for (label, p) in [
            ("p=0.002", 0.002),
            ("p=0.01", 0.01),
            ("p=0.05", 0.05),
            ("p=0.1", 0.1),
            ("p=0.3", 0.3),
            ("p=0.5", 0.5),
            ("p=0.9", 0.9),
            ("mixed", -1.0),
        ] {
            let set: Vec<Vec<u64>> = (0..64)
                .map(|_| {
                    (0..1024)
                        .map(|_| {
                            let d = if p < 0.0 { [0.0, 0.0, 0.0, 0.02, 0.1, 0.5, 1.0][rng.gen_range(0..7)] } else { p };
                            (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(d) as u64) << b))
                        })
                        .collect()
                })
                .collect();
            let o = out.as_mut_ptr();
            let mut row = format!("  {label:>8}:");
            for (name, f) in [("scalar", extract_scalar as unsafe fn(&[u64], u32, *mut u32) -> *mut u32), ("neon", extract_neon)] {
                let t = best(Box::new(|| {
                    for w in &set {
                        black_box(unsafe { f(w, 0, o) });
                    }
                }));
                row += &format!(" {name} {:.2}", t * 1e9 / (64.0 * 1024.0));
            }
            let t = best(Box::new(|| {
                for w in &set {
                    black_box(popcount_scalar(w));
                }
            }));
            row += &format!(" | popcount scalar {:.3}", t * 1e9 / (64.0 * 1024.0));
            let t = best(Box::new(|| {
                for w in &set {
                    black_box(popcount_neon(w));
                }
            }));
            row += &format!(" neon {:.3}", t * 1e9 / (64.0 * 1024.0));
            println!("{row}");
        }
        println!("extract_word, ns per word (64 Ki words with c random set bits):");
        for c in [1u32, 2, 3, 4, 5, 6, 8, 12, 16, 24, 32, 48, 64] {
            let ws: Vec<u64> = (0..65536)
                .map(|_| {
                    let mut w = 0u64;
                    while w.count_ones() < c {
                        w |= 1 << rng.gen_range(0..64);
                    }
                    w
                })
                .collect();
            let mut big = vec![0u32; 65536 * 64 + SLACK];
            let o = big.as_mut_ptr();
            let mut row = format!("  {c:>2} bits:");
            for (name, f) in [
                ("scalar", extract_word_scalar as unsafe fn(u64, u32, *mut u32) -> *mut u32),
                ("neon", extract_word_neon),
            ] {
                let t = best(Box::new(|| {
                    let mut d = o;
                    for (i, &w) in ws.iter().enumerate() {
                        d = unsafe { f(w, (i as u32) << 6, d) };
                    }
                    black_box(d);
                }));
                row += &format!(" {name} {:.2}", t * 1e9 / ws.len() as f64);
            }
            println!("{row}");
        }
    }

    #[test]
    fn range_fill() {
        for lo in 0..200 {
            for hi in lo..200 {
                let mut w = [0u64; 4];
                set_range(&mut w, lo, hi);
                for b in 0..256 {
                    assert_eq!(w[b / 64] >> (b % 64) & 1 == 1, (lo..=hi).contains(&b));
                }
            }
        }
    }
}
