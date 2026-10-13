//! Bitmap-to-docID extraction, popcount and word kernels, with AVX-512
//! paths and portable fallbacks.
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

/// Spare `u32` slots [`extract`] may write past the last value.
pub const SLACK: usize = 64;

/// Words with at most this many set bits are extracted by the scalar loop.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
const SCALAR_MAX: u32 = 4;

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
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
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
/// AVX-512 VBMI Elias-Fano decoder.
///
/// # Safety
/// `dst` must have room for the word's bits plus [`SLACK`] slots.
#[cfg_attr(
    not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512vbmi")),
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
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
    {
        extract_word_scalar(w, base, dst)
    }
}

/// Portable extraction.
///
/// # Safety
/// As for [`extract`].
#[cfg_attr(all(target_arch = "x86_64", target_feature = "avx512f", not(test)), allow(dead_code))]
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
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vpopcntdq")))]
    {
        words.iter().map(|w| w.count_ones() as u64).sum()
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
