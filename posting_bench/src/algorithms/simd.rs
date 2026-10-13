//! SIMD block-merge intersection of sorted `u32` slices (Schlegel et al.
//! ADMS 2011; Lemire, Boytsov & Kurz SPE 2016; Katsov 2012).
//!
//! Load a block of `W` values from each list, compare all `W×W` pairs with
//! `W` vector compares against rotations of the second block, compact the
//! matching lanes of the first block to the output, and advance the block
//! whose last element is smaller (both on a tie):
//!
//! ```text
//! a: [a0 a1 a2 a3] ─┬─ == [b0 b1 b2 b3]      mask = OR of 4 compares
//!                   ├─ == [b1 b2 b3 b0]      out += compact(a, mask)
//!                   ├─ == [b2 b3 b0 b1]      i += W if a3 <= b3
//!                   └─ == [b3 b0 b1 b2]      j += W if b3 <= a3
//! ```
//!
//! Three widths are provided: SSE (W = 4, `pshufb` compaction), AVX2
//! (W = 8, `vpermd` compaction) and AVX-512 (W = 16, `vpcompressd`, the
//! software analogue of `VP2INTERSECT`). Each is compiled only when the
//! target supports it; [`intersect`] picks the widest available and falls
//! back to a scalar merge elsewhere.

use super::pairwise::PairwiseFn;

/// Scalar tail: merge from positions `(i, j)` writing at `dst[k..]`.
#[inline(always)]
#[allow(dead_code)]
unsafe fn merge_tail(a: &[u32], b: &[u32], mut i: usize, mut j: usize, dst: *mut u32, mut k: usize) -> usize {
    while i < a.len() && j < b.len() {
        let (x, y) = (*a.get_unchecked(i), *b.get_unchecked(j));
        if x < y {
            i += 1;
        } else if y < x {
            j += 1;
        } else {
            *dst.add(k) = x;
            k += 1;
            i += 1;
            j += 1;
        }
    }
    k
}

#[cfg(all(target_arch = "x86_64", target_feature = "ssse3"))]
mod sse {
    use core::arch::x86_64::*;

    /// For each 4-bit match mask, a `pshufb` control moving matching lanes
    /// to the front.
    const fn lut4() -> [[u8; 16]; 16] {
        let mut lut = [[0x80u8; 16]; 16];
        let mut m = 0;
        while m < 16 {
            let (mut k, mut lane) = (0, 0);
            while lane < 4 {
                if m & (1 << lane) != 0 {
                    let mut byte = 0;
                    while byte < 4 {
                        lut[m][4 * k + byte] = (4 * lane + byte) as u8;
                        byte += 1;
                    }
                    k += 1;
                }
                lane += 1;
            }
            m += 1;
        }
        lut
    }
    static LUT4: [[u8; 16]; 16] = lut4();

    pub fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
        let start = out.len();
        out.reserve(a.len().min(b.len()) + 4);
        // SAFETY: loads stay within `na4`/`nb4`; stores write at most
        // `min(|a|, |b|) + 4` elements into reserved capacity.
        unsafe {
            let dst = out.as_mut_ptr().add(start);
            let (mut i, mut j, mut k) = (0, 0, 0);
            let (na4, nb4) = (a.len() & !3, b.len() & !3);
            while i < na4 && j < nb4 {
                let va = _mm_loadu_si128(a.as_ptr().add(i) as *const __m128i);
                let vb = _mm_loadu_si128(b.as_ptr().add(j) as *const __m128i);
                let c0 = _mm_cmpeq_epi32(va, vb);
                let c1 = _mm_cmpeq_epi32(va, _mm_shuffle_epi32::<0x39>(vb));
                let c2 = _mm_cmpeq_epi32(va, _mm_shuffle_epi32::<0x4e>(vb));
                let c3 = _mm_cmpeq_epi32(va, _mm_shuffle_epi32::<0x93>(vb));
                let cmp = _mm_or_si128(_mm_or_si128(c0, c1), _mm_or_si128(c2, c3));
                let mask = _mm_movemask_ps(_mm_castsi128_ps(cmp)) as usize;
                let ctrl = _mm_loadu_si128(LUT4[mask].as_ptr() as *const __m128i);
                _mm_storeu_si128(dst.add(k) as *mut __m128i, _mm_shuffle_epi8(va, ctrl));
                k += mask.count_ones() as usize;
                let (amax, bmax) = (*a.get_unchecked(i + 3), *b.get_unchecked(j + 3));
                i += if amax <= bmax { 4 } else { 0 };
                j += if bmax <= amax { 4 } else { 0 };
            }
            k = super::merge_tail(a, b, i, j, dst, k);
            out.set_len(start + k);
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
mod avx2 {
    use core::arch::x86_64::*;

    /// For each 8-bit match mask, `vpermd` indices moving matching lanes to
    /// the front.
    const fn lut8() -> [[u32; 8]; 256] {
        let mut lut = [[0u32; 8]; 256];
        let mut m = 0;
        while m < 256 {
            let (mut k, mut lane) = (0, 0);
            while lane < 8 {
                if m & (1 << lane) != 0 {
                    lut[m][k] = lane as u32;
                    k += 1;
                }
                lane += 1;
            }
            m += 1;
        }
        lut
    }
    static LUT8: [[u32; 8]; 256] = lut8();

    pub fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
        let start = out.len();
        out.reserve(a.len().min(b.len()) + 8);
        // SAFETY: as in the SSE kernel, with W = 8.
        unsafe {
            let dst = out.as_mut_ptr().add(start);
            let (mut i, mut j, mut k) = (0, 0, 0);
            let (na8, nb8) = (a.len() & !7, b.len() & !7);
            while i < na8 && j < nb8 {
                let va = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
                let vb = _mm256_loadu_si256(b.as_ptr().add(j) as *const __m256i);
                // In-lane rotations of vb and of vb with its 128-bit halves
                // swapped cover all 64 pairs with cheap shuffles.
                let vs = _mm256_permute2x128_si256::<0x01>(vb, vb);
                let c0 = _mm256_or_si256(
                    _mm256_or_si256(_mm256_cmpeq_epi32(va, vb), _mm256_cmpeq_epi32(va, _mm256_shuffle_epi32::<0x39>(vb))),
                    _mm256_or_si256(
                        _mm256_cmpeq_epi32(va, _mm256_shuffle_epi32::<0x4e>(vb)),
                        _mm256_cmpeq_epi32(va, _mm256_shuffle_epi32::<0x93>(vb)),
                    ),
                );
                let c1 = _mm256_or_si256(
                    _mm256_or_si256(_mm256_cmpeq_epi32(va, vs), _mm256_cmpeq_epi32(va, _mm256_shuffle_epi32::<0x39>(vs))),
                    _mm256_or_si256(
                        _mm256_cmpeq_epi32(va, _mm256_shuffle_epi32::<0x4e>(vs)),
                        _mm256_cmpeq_epi32(va, _mm256_shuffle_epi32::<0x93>(vs)),
                    ),
                );
                let mask = _mm256_movemask_ps(_mm256_castsi256_ps(_mm256_or_si256(c0, c1))) as usize;
                let perm = _mm256_loadu_si256(LUT8[mask].as_ptr() as *const __m256i);
                _mm256_storeu_si256(dst.add(k) as *mut __m256i, _mm256_permutevar8x32_epi32(va, perm));
                k += mask.count_ones() as usize;
                let (amax, bmax) = (*a.get_unchecked(i + 7), *b.get_unchecked(j + 7));
                i += if amax <= bmax { 8 } else { 0 };
                j += if bmax <= amax { 8 } else { 0 };
            }
            k = super::merge_tail(a, b, i, j, dst, k);
            out.set_len(start + k);
        }
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
mod avx512 {
    use core::arch::x86_64::*;

    pub fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
        let start = out.len();
        out.reserve(a.len().min(b.len()) + 16);
        // SAFETY: as in the SSE kernel, with W = 16.
        unsafe {
            let dst = out.as_mut_ptr().add(start);
            let (mut i, mut j, mut k) = (0, 0, 0);
            let (na16, nb16) = (a.len() & !15, b.len() & !15);
            while i < na16 && j < nb16 {
                let va = _mm512_loadu_si512(a.as_ptr().add(i) as *const _);
                let vb = _mm512_loadu_si512(b.as_ptr().add(j) as *const _);
                // Four 128-bit lane rotations × four in-lane rotations = all
                // 256 pairs (the VP2INTERSECT emulation pattern).
                let mut mask: __mmask16 = 0;
                for vr in [
                    vb,
                    _mm512_shuffle_i32x4::<0x39>(vb, vb),
                    _mm512_shuffle_i32x4::<0x4e>(vb, vb),
                    _mm512_shuffle_i32x4::<0x93>(vb, vb),
                ] {
                    mask |= _mm512_cmpeq_epi32_mask(va, vr)
                        | _mm512_cmpeq_epi32_mask(va, _mm512_shuffle_epi32::<0x39>(vr))
                        | _mm512_cmpeq_epi32_mask(va, _mm512_shuffle_epi32::<0x4e>(vr))
                        | _mm512_cmpeq_epi32_mask(va, _mm512_shuffle_epi32::<0x93>(vr));
                }
                _mm512_storeu_si512(dst.add(k) as *mut _, _mm512_maskz_compress_epi32(mask, va));
                k += mask.count_ones() as usize;
                let (amax, bmax) = (*a.get_unchecked(i + 15), *b.get_unchecked(j + 15));
                i += if amax <= bmax { 16 } else { 0 };
                j += if bmax <= amax { 16 } else { 0 };
            }
            k = super::merge_tail(a, b, i, j, dst, k);
            out.set_len(start + k);
        }
    }
}

/// Widest SIMD block-merge kernel available on this target (scalar merge
/// if none).
pub fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    return avx512::intersect(a, b, out);
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2", not(target_feature = "avx512f")))]
    return avx2::intersect(a, b, out);
    #[cfg(all(target_arch = "x86_64", target_feature = "ssse3", not(target_feature = "avx2")))]
    return sse::intersect(a, b, out);
    #[allow(unreachable_code)]
    super::pairwise::merge(a, b, out)
}

/// Every SIMD kernel compiled for this target.
pub fn kernels() -> Vec<(&'static str, PairwiseFn)> {
    #[allow(unused_mut)]
    let mut v: Vec<(&'static str, PairwiseFn)> = Vec::new();
    #[cfg(all(target_arch = "x86_64", target_feature = "ssse3"))]
    v.push(("SIMD SSE 4x4", sse::intersect));
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    v.push(("SIMD AVX2 8x8", avx2::intersect));
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    v.push(("SIMD AVX-512 16x16", avx512::intersect));
    v
}
