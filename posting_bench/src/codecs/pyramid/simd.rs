//! Kernels used by [`super::Pyramid`]. Every explicit SIMD path is gated on
//! the target features it needs and has a portable scalar twin (`*_scalar`,
//! always compiled, used as the fallback and by the tests).
//!
//! * [`select2`]: positions of the k-th and (k+1)-th set bits of a word
//!   (BMI2 `pdep`), the core of the sparse-block directory lookup.
//! * [`bytes_contain`], [`u16s_contain`]: membership in a short sorted array
//!   by one or two masked AVX-512BW compares (the O(1) probe of a sparse
//!   block or a Small chunk). Masked loads never touch bytes past the array,
//!   so no padding is needed.
//! * [`test_bits`], [`test_bytes`]: which of up to 32 low bytes are in a
//!   256-bit block bitmap (AVX512-VBMI `vpermb` byte lookup plus a bit
//!   select) or in another block's low bytes.
//! * [`emit_lanes`], [`extract256`], [`widen_bytes`]: write docIDs with
//!   AVX-512F `vpcompressd`: selected lanes of a byte vector, the set bits
//!   of a 256-bit block (one compress per non-zero 16-bit piece), or a run
//!   of low bytes plus a base. Where AVX512-VBMI2 is available,
//!   [`extract256`] instead uses Roaring's `vpcompressb` kernel (one
//!   compress per 64-bit word); without it, the `vpcompressd` path
//!   ([`extract256_compressd`]) is used.

#[cfg(target_arch = "x86_64")]
#[allow(unused_imports)]
use std::arch::x86_64::*;

/// Spare `u32` slots the writing kernels may touch past the values they emit.
pub const SLACK: usize = 16;

/// The low `n` bits set (`n <= 32`).
#[inline(always)]
pub fn low_mask32(n: usize) -> u32 {
    debug_assert!(n <= 32);
    ((1u64 << n) - 1) as u32
}

// ---------------------------------------------------------------- select

/// Positions of the `k`-th and `(k+1)`-th set bits of `w` (0-based `k`).
/// Requires `w.count_ones() >= k + 2`.
#[inline(always)]
pub fn select2(w: u64, k: u32) -> (u32, u32) {
    debug_assert!(w.count_ones() >= k + 2);
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    {
        // SAFETY: BMI2 is enabled for this build.
        let p = unsafe { _pdep_u64(3u64 << k, w) };
        (p.trailing_zeros(), 63 - p.leading_zeros())
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    select2_scalar(w, k)
}

/// Position of the `k`-th set bit of `w` (0-based). Requires
/// `w.count_ones() > k`.
#[inline(always)]
pub fn select1(w: u64, k: u32) -> u32 {
    debug_assert!(w.count_ones() > k);
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    {
        // SAFETY: BMI2 is enabled for this build.
        unsafe { _pdep_u64(1u64 << k, w) }.trailing_zeros()
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    select1_scalar(w, k)
}

/// Portable [`select1`].
#[inline(always)]
pub fn select1_scalar(mut w: u64, k: u32) -> u32 {
    for _ in 0..k {
        w &= w - 1;
    }
    w.trailing_zeros()
}

/// Portable [`select2`]: clear the `k` lowest set bits.
#[inline(always)]
pub fn select2_scalar(mut w: u64, k: u32) -> (u32, u32) {
    for _ in 0..k {
        w &= w - 1;
    }
    let a = w.trailing_zeros();
    w &= w - 1;
    (a, w.trailing_zeros())
}

// ---------------------------------------------------------- membership

/// Whether `x` is among the `n <= 32` bytes at `p`.
///
/// # Safety
/// `p..p + n` must be readable.
#[inline(always)]
pub unsafe fn bytes_contain(p: *const u8, n: usize, x: u8) -> bool {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl"))]
    {
        let m = low_mask32(n);
        let v = _mm256_maskz_loadu_epi8(m, p as *const i8);
        _mm256_mask_cmpeq_epi8_mask(m, v, _mm256_set1_epi8(x as i8)) != 0
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl")))]
    bytes_contain_scalar(p, n, x)
}

/// # Safety
/// As for [`bytes_contain`].
#[inline(always)]
pub unsafe fn bytes_contain_scalar(p: *const u8, n: usize, x: u8) -> bool {
    std::slice::from_raw_parts(p, n).contains(&x)
}

/// Number of the `n <= 32` (sorted) bytes at `p` that are smaller than `x`.
///
/// # Safety
/// As for [`bytes_contain`].
#[inline(always)]
pub unsafe fn bytes_count_lt(p: *const u8, n: usize, x: u8) -> usize {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl"))]
    {
        let m = low_mask32(n);
        let v = _mm256_maskz_loadu_epi8(m, p as *const i8);
        _mm256_mask_cmplt_epu8_mask(m, v, _mm256_set1_epi8(x as i8)).count_ones() as usize
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl")))]
    bytes_count_lt_scalar(p, n, x)
}

/// # Safety
/// As for [`bytes_contain`].
#[inline(always)]
pub unsafe fn bytes_count_lt_scalar(p: *const u8, n: usize, x: u8) -> usize {
    std::slice::from_raw_parts(p, n).partition_point(|&b| b < x)
}

/// Whether `x` is among the `n <= 64` values at `p`.
///
/// # Safety
/// `p..p + n` must be readable.
#[inline(always)]
pub unsafe fn u16s_contain(p: *const u16, n: usize, x: u16) -> bool {
    debug_assert!(n <= 64);
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw"))]
    {
        let key = _mm512_set1_epi16(x as i16);
        let m0 = low_mask32(n.min(32));
        let v0 = _mm512_maskz_loadu_epi16(m0, p as *const i16);
        let mut hit = _mm512_mask_cmpeq_epi16_mask(m0, v0, key);
        if n > 32 {
            let m1 = low_mask32(n - 32);
            let v1 = _mm512_maskz_loadu_epi16(m1, p.add(32) as *const i16);
            hit |= _mm512_mask_cmpeq_epi16_mask(m1, v1, key);
        }
        hit != 0
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw")))]
    u16s_contain_scalar(p, n, x)
}

/// # Safety
/// As for [`u16s_contain`].
#[inline(always)]
pub unsafe fn u16s_contain_scalar(p: *const u16, n: usize, x: u16) -> bool {
    std::slice::from_raw_parts(p, n).binary_search(&x).is_ok()
}

/// The `n <= 32` bytes at `p`, zero-padded to 32.
///
/// # Safety
/// `p..p + n` must be readable.
#[inline(always)]
pub unsafe fn load_bytes32(p: *const u8, n: usize) -> [u8; 32] {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl"))]
    {
        std::mem::transmute::<__m256i, [u8; 32]>(_mm256_maskz_loadu_epi8(low_mask32(n), p as *const i8))
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl")))]
    load_bytes32_scalar(p, n)
}

/// # Safety
/// As for [`load_bytes32`].
#[inline(always)]
pub unsafe fn load_bytes32_scalar(p: *const u8, n: usize) -> [u8; 32] {
    let mut d = [0u8; 32];
    d[..n].copy_from_slice(std::slice::from_raw_parts(p, n));
    d
}

// ------------------------------------------------- block-local filtering

/// Bit `j` of the result is bit `d[j]` of the 256-bit bitmap at `bm`, for
/// all 32 lanes (the caller masks the valid ones).
///
/// # Safety
/// `bm..bm + 4` must be readable.
#[inline(always)]
pub unsafe fn test_bits(d: &[u8; 32], bm: *const u64) -> u32 {
    #[cfg(all(
        target_arch = "x86_64",
        target_feature = "avx512bw",
        target_feature = "avx512vl",
        target_feature = "avx512vbmi"
    ))]
    {
        // Byte `d >> 3` of the bitmap (vpermb), then its bit `d & 7`.
        let dv = _mm256_loadu_si256(d.as_ptr() as *const __m256i);
        let idx = _mm256_and_si256(_mm256_srli_epi16::<3>(dv), _mm256_set1_epi8(0x1F));
        let bytes = _mm256_permutexvar_epi8(idx, _mm256_loadu_si256(bm as *const __m256i));
        let pow2 = _mm256_setr_epi8(
            1, 2, 4, 8, 16, 32, 64, -128, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 4, 8, 16, 32, 64, -128, 0, 0, 0, 0, 0, 0, 0, 0,
        );
        let sel = _mm256_shuffle_epi8(pow2, _mm256_and_si256(dv, _mm256_set1_epi8(7)));
        _mm256_test_epi8_mask(bytes, sel)
    }
    #[cfg(not(all(
        target_arch = "x86_64",
        target_feature = "avx512bw",
        target_feature = "avx512vl",
        target_feature = "avx512vbmi"
    )))]
    test_bits_scalar(d, bm)
}

/// # Safety
/// As for [`test_bits`].
#[inline(always)]
pub unsafe fn test_bits_scalar(d: &[u8; 32], bm: *const u64) -> u32 {
    let mut r = 0u32;
    for (j, &x) in d.iter().enumerate() {
        r |= (((*bm.add((x >> 6) as usize) >> (x & 63)) & 1) as u32) << j;
    }
    r
}

/// Bit `j < nd` of the result: whether `d[j]` is among the `n <= 32` bytes
/// at `p`.
///
/// # Safety
/// `p..p + n` must be readable.
#[inline(always)]
pub unsafe fn test_bytes(d: &[u8; 32], nd: usize, p: *const u8, n: usize) -> u32 {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl"))]
    {
        // Loop over the shorter side: each step is one compare of a whole
        // vector against a broadcast byte.
        let mut r = 0u32;
        if n <= nd {
            let dv = _mm256_loadu_si256(d.as_ptr() as *const __m256i);
            for i in 0..n {
                r |= _mm256_cmpeq_epi8_mask(dv, _mm256_set1_epi8(*p.add(i) as i8));
            }
            r & low_mask32(nd)
        } else {
            let m = low_mask32(n);
            let ov = _mm256_maskz_loadu_epi8(m, p as *const i8);
            for (j, &x) in d.iter().enumerate().take(nd) {
                r |= ((_mm256_mask_cmpeq_epi8_mask(m, ov, _mm256_set1_epi8(x as i8)) != 0) as u32) << j;
            }
            r
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl")))]
    test_bytes_scalar(d, nd, p, n)
}

/// Portable [`test_bytes`]: a merge of the two sorted byte arrays.
///
/// # Safety
/// As for [`test_bytes`].
#[inline(always)]
pub unsafe fn test_bytes_scalar(d: &[u8; 32], nd: usize, p: *const u8, n: usize) -> u32 {
    let o = std::slice::from_raw_parts(p, n);
    let (mut i, mut j, mut r) = (0, 0, 0u32);
    while i < nd && j < n {
        let (x, y) = (d[i], o[j]);
        r |= ((x == y) as u32) << i;
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    r
}

// --------------------------------------------------------------- writing

/// Writes `base + d[j]` for every set bit `j` of `hits`, in order, at
/// `dst`; returns how many. May write up to [`SLACK`] slots past them.
///
/// # Safety
/// `dst` must have room for `popcount(hits) + SLACK` values.
#[inline(always)]
pub unsafe fn emit_lanes(d: &[u8; 32], hits: u32, base: u32, dst: *mut u32) -> usize {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    {
        let vb = _mm512_set1_epi32(base as i32);
        let lo = hits as u16;
        let v = _mm512_add_epi32(_mm512_cvtepu8_epi32(_mm_loadu_si128(d.as_ptr() as *const __m128i)), vb);
        _mm512_storeu_si512(dst as *mut _, _mm512_maskz_compress_epi32(lo, v));
        let c0 = lo.count_ones() as usize;
        let hi = (hits >> 16) as u16;
        if hi != 0 {
            let v = _mm512_add_epi32(_mm512_cvtepu8_epi32(_mm_loadu_si128(d.as_ptr().add(16) as *const __m128i)), vb);
            _mm512_storeu_si512(dst.add(c0) as *mut _, _mm512_maskz_compress_epi32(hi, v));
        }
        c0 + hi.count_ones() as usize
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
    emit_lanes_scalar(d, hits, base, dst)
}

/// # Safety
/// As for [`emit_lanes`].
#[inline(always)]
pub unsafe fn emit_lanes_scalar(d: &[u8; 32], mut hits: u32, base: u32, dst: *mut u32) -> usize {
    let mut k = 0;
    while hits != 0 {
        *dst.add(k) = base + d[hits.trailing_zeros() as usize] as u32;
        k += 1;
        hits &= hits - 1;
    }
    k
}

/// Writes `base + i` for every set bit `i` of the 256-bit `w`, in order, at
/// `dst`; returns how many. May write up to [`SLACK`] slots past them.
///
/// # Safety
/// `dst` must have room for `popcount(w) + SLACK` values.
#[inline(always)]
pub unsafe fn extract256(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2"))]
    {
        // VBMI2: one vpcompressb per word (Roaring's kernel).
        crate::codecs::roaring::simd::extract_avx512_u32(0..4, |i| w[i], base, dst).offset_from(dst) as usize
    }
    #[cfg(all(
        target_arch = "x86_64",
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vl",
        not(target_feature = "avx512vbmi2")
    ))]
    {
        extract256_compressd(w, base, dst)
    }
    #[cfg(not(all(
        target_arch = "x86_64",
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vl"
    )))]
    extract256_scalar(w, base, dst)
}

/// AVX-512F [`extract256`] without VBMI2: one `vpcompressd` of 16 lanes per
/// non-zero 16-bit piece.
///
/// # Safety
/// As for [`extract256`].
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512vl"
))]
#[inline(always)]
pub unsafe fn extract256_compressd(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    {
        // One vpcompressd per non-zero 16-bit piece.
        let v = _mm256_loadu_si256(w.as_ptr() as *const __m256i);
        let mut nz = _mm256_test_epi16_mask(v, v) as u32;
        let iota = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        let pieces = w.as_ptr() as *const u16;
        let mut k = 0usize;
        while nz != 0 {
            let i = nz.trailing_zeros();
            nz &= nz - 1;
            let m = *pieces.add(i as usize);
            let vals = _mm512_add_epi32(iota, _mm512_set1_epi32((base + 16 * i) as i32));
            _mm512_storeu_si512(dst.add(k) as *mut _, _mm512_maskz_compress_epi32(m, vals));
            k += m.count_ones() as usize;
        }
        k
    }
}

/// Writes `base + i` for every set bit `i` of the bitmap `w` (any length),
/// in order, at `dst`; returns how many. May write up to [`SLACK`] slots
/// past them. With VBMI2 one `vpcompressb` per word (Roaring's kernel);
/// otherwise [`extract256`] per 4 words.
///
/// # Safety
/// `w.len()` is a multiple of 4; `dst` has room for `popcount(w) + SLACK`.
#[inline(always)]
pub unsafe fn extract_words(w: &[u64], base: u32, dst: *mut u32) -> usize {
    debug_assert!(w.len().is_multiple_of(4));
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2"))]
    {
        crate::codecs::roaring::simd::extract_avx512_u32(0..w.len(), |i| *w.get_unchecked(i), base, dst).offset_from(dst)
            as usize
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2")))]
    {
        let mut k = 0;
        for (i, b) in w.chunks_exact(4).enumerate() {
            k += extract256(&*(b.as_ptr() as *const [u64; 4]), base + 256 * i as u32, dst.add(k));
        }
        k
    }
}

/// Portable [`extract256`]: writes 4 positions per word unconditionally and
/// advances by the popcount (branching only for words with more bits).
///
/// # Safety
/// As for [`extract256`].
#[inline(always)]
pub unsafe fn extract256_scalar(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let mut k = 0usize;
    for (i, &x) in w.iter().enumerate() {
        let b = base + 64 * i as u32;
        let cnt = x.count_ones() as usize;
        let mut x = x;
        let mut p = dst.add(k);
        loop {
            for _ in 0..4 {
                p.write(b.wrapping_add(x.trailing_zeros()));
                p = p.add(1);
                x &= x.wrapping_sub(1);
            }
            if x == 0 {
                break;
            }
        }
        k += cnt;
    }
    k
}

/// Writes `base + p[i]` for `i < n` at `dst`. May write up to [`SLACK`]
/// slots past them.
///
/// # Safety
/// `p..p + n` must be readable and `dst` must have room for `n + SLACK`.
#[inline(always)]
pub unsafe fn widen_bytes(p: *const u8, n: usize, base: u32, dst: *mut u32) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512vl"))]
    {
        let vb = _mm512_set1_epi32(base as i32);
        let mut i = 0;
        while i < n {
            let m = low_mask32((n - i).min(16)) as u16;
            let b = _mm_maskz_loadu_epi8(m, p.add(i) as *const i8);
            _mm512_storeu_si512(dst.add(i) as *mut _, _mm512_add_epi32(_mm512_cvtepu8_epi32(b), vb));
            i += 16;
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512vl")))]
    widen_bytes_scalar(p, n, base, dst)
}

/// # Safety
/// As for [`widen_bytes`].
#[inline(always)]
pub unsafe fn widen_bytes_scalar(p: *const u8, n: usize, base: u32, dst: *mut u32) {
    for i in 0..n {
        *dst.add(i) = base + *p.add(i) as u32;
    }
}

/// Inclusive prefix masks `(2 << j) - 1` for 16 lanes.
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512f",
    target_feature = "avx512bw",
    target_feature = "avx512vl",
    target_feature = "avx512vpopcntdq"
))]
static PREFIX16: [u32; 16] = {
    let mut t = [0u32; 16];
    let mut j = 0;
    while j < 16 {
        t[j] = (2u32 << j) - 1;
        j += 1;
    }
    t
};

/// Decodes `n` consecutive sparse values spanning one or more sparse blocks:
/// writes `base | blk[r] << 8 | lows[i]` for `i < n`, where `r` (the block
/// of value `i`) is the number of set bits of `U` at bit positions
/// `pos..=pos + i`, minus one (bit `pos` must be set: value 0 starts a
/// block). May write up to [`SLACK`] slots past the values.
///
/// AVX-512 (VPOPCNTDQ): 16 values per step; the block of each lane is an
/// inclusive prefix popcount of 16 bits of `U` (`vpandd` with prefix masks,
/// `vpopcntd`) used as a `vpermt2d` index into the block ids.
///
/// # Safety
/// `lows..lows + n` readable; `U` readable 8 bytes past bit `pos + n`;
/// `blk[-1..r_max + 32]` readable where `r_max` is the last block's index;
/// `dst` has room for `n + SLACK` values.
#[inline(always)]
pub unsafe fn expand_sparse(lows: *const u8, u: *const u64, pos: u32, n: usize, blk: *const u32, base: u32, dst: *mut u32) {
    #[cfg(all(
        target_arch = "x86_64",
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vl",
        target_feature = "avx512vpopcntdq"
    ))]
    {
        let prefix = _mm512_loadu_si512(PREFIX16.as_ptr() as *const _);
        let vb = _mm512_set1_epi32(base as i32);
        let ub = u as *const u8;
        let mut carry = 0isize;
        let mut i = 0usize;
        while i < n {
            let bp = pos as usize + i;
            let m = ((ub.add(bp >> 3) as *const u64).read_unaligned() >> (bp & 7)) as u32 & 0xFFFF;
            let r = _mm512_popcnt_epi32(_mm512_and_si512(_mm512_set1_epi32(m as i32), prefix));
            let t = blk.offset(carry - 1);
            let ids = _mm512_permutex2var_epi32(
                _mm512_loadu_si512(t as *const _),
                r,
                _mm512_loadu_si512(t.add(16) as *const _),
            );
            let l = _mm512_cvtepu8_epi32(_mm_maskz_loadu_epi8(low_mask32((n - i).min(16)) as u16, lows.add(i) as *const i8));
            let out = _mm512_or_si512(_mm512_or_si512(_mm512_slli_epi32::<8>(ids), l), vb);
            _mm512_storeu_si512(dst.add(i) as *mut _, out);
            carry += m.count_ones() as isize;
            i += 16;
        }
    }
    #[cfg(not(all(
        target_arch = "x86_64",
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512vl",
        target_feature = "avx512vpopcntdq"
    )))]
    expand_sparse_scalar(lows, u, pos, n, blk, base, dst)
}

/// Portable [`expand_sparse`].
///
/// # Safety
/// As for [`expand_sparse`].
#[inline(always)]
pub unsafe fn expand_sparse_scalar(lows: *const u8, u: *const u64, pos: u32, n: usize, blk: *const u32, base: u32, dst: *mut u32) {
    let mut r = -1isize;
    for i in 0..n {
        let bp = pos as usize + i;
        r += ((*u.add(bp >> 6) >> (bp & 63)) & 1) as isize;
        *dst.add(i) = base | *blk.offset(r) << 8 | *lows.add(i) as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn sorted_bytes(rng: &mut StdRng, n: usize) -> Vec<u8> {
        let mut v: Vec<u8> = (0..=255u8).collect();
        for i in 0..n {
            let j = rng.gen_range(i..256);
            v.swap(i, j);
        }
        v.truncate(n);
        v.sort_unstable();
        v
    }

    #[test]
    fn select2_matches_scalar() {
        let mut rng = StdRng::seed_from_u64(1);
        for _ in 0..20_000 {
            let w: u64 = rng.gen::<u64>() & rng.gen::<u64>();
            let c = w.count_ones();
            if c < 2 {
                continue;
            }
            let k = rng.gen_range(0..c - 1);
            let want: Vec<u32> = (0..64).filter(|b| w >> b & 1 == 1).collect();
            assert_eq!(select2_scalar(w, k), (want[k as usize], want[k as usize + 1]));
            assert_eq!(select2(w, k), select2_scalar(w, k));
            assert_eq!(select1_scalar(w, k + 1), want[k as usize + 1]);
            assert_eq!(select1(w, k + 1), want[k as usize + 1]);
        }
    }

    #[test]
    fn membership_kernels_match_scalar() {
        let mut rng = StdRng::seed_from_u64(2);
        for _ in 0..3000 {
            let n = rng.gen_range(0..=32);
            let v = sorted_bytes(&mut rng, n);
            for x in 0..=255u8 {
                unsafe {
                    let want = v.contains(&x);
                    assert_eq!(load_bytes32(v.as_ptr(), n), load_bytes32_scalar(v.as_ptr(), n));
                    assert_eq!(bytes_contain_scalar(v.as_ptr(), n, x), want);
                    assert_eq!(bytes_contain(v.as_ptr(), n, x), want);
                    let lt = v.iter().filter(|&&b| b < x).count();
                    assert_eq!(bytes_count_lt_scalar(v.as_ptr(), n, x), lt);
                    assert_eq!(bytes_count_lt(v.as_ptr(), n, x), lt);
                }
            }
        }
        for _ in 0..3000 {
            let n = rng.gen_range(0..=64);
            let mut v: Vec<u16> = (0..n).map(|_| rng.gen_range(0..2000)).collect();
            v.sort_unstable();
            v.dedup();
            for _ in 0..50 {
                let x = if rng.gen_bool(0.5) && !v.is_empty() { v[rng.gen_range(0..v.len())] } else { rng.gen_range(0..2000) };
                unsafe {
                    let want = v.contains(&x);
                    assert_eq!(u16s_contain_scalar(v.as_ptr(), v.len(), x), want);
                    assert_eq!(u16s_contain(v.as_ptr(), v.len(), x), want);
                }
            }
        }
    }

    #[test]
    fn block_filter_kernels_match_scalar() {
        let mut rng = StdRng::seed_from_u64(3);
        for _ in 0..5000 {
            let nd = rng.gen_range(1..=32);
            let dv = sorted_bytes(&mut rng, nd);
            let mut d = [0u8; 32];
            d[..nd].copy_from_slice(&dv);
            let density = rng.gen_range(0.0..1.0);
            let bm: [u64; 4] =
                std::array::from_fn(|_| (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(density) as u64) << b)));
            unsafe {
                let want: u32 = (0..nd).fold(0, |r, j| r | (((bm[(d[j] >> 6) as usize] >> (d[j] & 63)) & 1) as u32) << j);
                let valid = low_mask32(nd);
                assert_eq!(test_bits_scalar(&d, bm.as_ptr()) & valid, want);
                assert_eq!(test_bits(&d, bm.as_ptr()) & valid, want);
                let n = rng.gen_range(0..=32);
                let o = sorted_bytes(&mut rng, n);
                let want: u32 = (0..nd).fold(0, |r, j| r | (o.contains(&d[j]) as u32) << j);
                assert_eq!(test_bytes_scalar(&d, nd, o.as_ptr(), n), want);
                assert_eq!(test_bytes(&d, nd, o.as_ptr(), n), want);
            }
        }
    }

    #[test]
    fn expand_sparse_matches_scalar() {
        let mut rng = StdRng::seed_from_u64(5);
        for _ in 0..3000 {
            // Blocks of 1..=28 values (singletons more likely).
            let nb = rng.gen_range(1..80);
            let counts: Vec<usize> =
                (0..nb).map(|_| if rng.gen_bool(0.5) { 1 } else { rng.gen_range(1..=28) }).collect();
            let n: usize = counts.iter().sum();
            let lead = rng.gen_range(0..70); // values of earlier blocks in U
            let mut u = vec![0u64; (lead + n + 1).div_ceil(64) + 2];
            u[0] |= 1; // an earlier block starts at 0
            let mut p = lead;
            for &c in &counts {
                u[p / 64] |= 1 << (p % 64);
                p += c;
            }
            u[p / 64] |= 1 << (p % 64);
            let lows: Vec<u8> = (0..n).map(|_| rng.gen()).collect();
            let mut blk = vec![0u32; 1 + nb + 40];
            let mut ids: Vec<u32> = (0..256).collect();
            for i in 0..nb {
                let j = rng.gen_range(i..256);
                ids.swap(i, j);
            }
            let mut ids = ids[..nb].to_vec();
            ids.sort_unstable();
            blk[1..1 + nb].copy_from_slice(&ids);
            let base = rng.gen_range(0..1u32 << 16) << 16;
            let mut want = Vec::new();
            for (b, &c) in counts.iter().enumerate() {
                want.extend((0..c).map(|_| base | ids[b] << 8));
            }
            for (w, &l) in want.iter_mut().zip(&lows) {
                *w |= l as u32;
            }
            let mut a = vec![0u32; n + SLACK];
            let mut s = vec![0u32; n + SLACK];
            unsafe {
                expand_sparse(lows.as_ptr(), u.as_ptr(), lead as u32, n, blk.as_ptr().add(1), base, a.as_mut_ptr());
                expand_sparse_scalar(lows.as_ptr(), u.as_ptr(), lead as u32, n, blk.as_ptr().add(1), base, s.as_mut_ptr());
            }
            assert_eq!(s[..n], want[..]);
            assert_eq!(a[..n], want[..]);
        }
    }

    #[test]
    fn writing_kernels_match_scalar() {
        let mut rng = StdRng::seed_from_u64(4);
        let mut a = vec![0u32; 256 + SLACK];
        let mut b = vec![0u32; 256 + SLACK];
        for _ in 0..5000 {
            let base = rng.gen_range(0..1u32 << 24) << 8;
            let mut d = [0u8; 32];
            for x in d.iter_mut() {
                *x = rng.gen();
            }
            let hits: u32 = rng.gen::<u32>() & rng.gen::<u32>();
            unsafe {
                let n = emit_lanes(&d, hits, base, a.as_mut_ptr());
                let m = emit_lanes_scalar(&d, hits, base, b.as_mut_ptr());
                assert_eq!(n, m);
                assert_eq!(a[..n], b[..n]);
                let want: Vec<u32> = (0..32).filter(|j| hits >> j & 1 == 1).map(|j| base + d[j] as u32).collect();
                assert_eq!(a[..n], want[..]);

                let density = rng.gen_range(0.0..1.0);
                let w: [u64; 4] =
                    std::array::from_fn(|_| (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(density) as u64) << b)));
                let n = extract256(&w, base, a.as_mut_ptr());
                let want: Vec<u32> =
                    (0..256u32).filter(|&i| w[(i >> 6) as usize] >> (i & 63) & 1 == 1).map(|i| base + i).collect();
                assert_eq!(a[..n], want[..]);
                let m = extract256_scalar(&w, base, b.as_mut_ptr());
                assert_eq!(b[..m], want[..]);
                let m = extract_words(&w, base, b.as_mut_ptr());
                assert_eq!(b[..m], want[..]);
                #[cfg(all(
                    target_arch = "x86_64",
                    target_feature = "avx512f",
                    target_feature = "avx512bw",
                    target_feature = "avx512vl"
                ))]
                {
                    let m = extract256_compressd(&w, base, b.as_mut_ptr());
                    assert_eq!(b[..m], want[..]);
                }

                let n = rng.gen_range(0..=200);
                let bytes: Vec<u8> = (0..n).map(|_| rng.gen()).collect();
                widen_bytes(bytes.as_ptr(), n, base, a.as_mut_ptr());
                widen_bytes_scalar(bytes.as_ptr(), n, base, b.as_mut_ptr());
                let want: Vec<u32> = bytes.iter().map(|&x| base + x as u32).collect();
                assert_eq!(a[..n], want[..]);
                assert_eq!(b[..n], want[..]);
            }
        }
    }
}
