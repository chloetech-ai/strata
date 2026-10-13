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
//!
//! On aarch64 every kernel but [`bytes_count_lt`] (whose callers, cursors,
//! are latency-bound: the scalar binary search wins there) has a NEON path
//! (`*_neon`, also checked against its scalar twin by the tests). NEON has
//! no masked loads: arrays are covered by overlapping in-bounds loads (or
//! bytes at clamped indices), so still no padding is needed. `tbl` lookups
//! replace `vpermb`, and, with a table of each byte value's set-bit
//! positions ([`BYTE_POS`]), the compress instructions; `cnt` byte
//! popcounts replace `vpopcntd` and, with a multiply for their prefix sums,
//! `pdep` in [`select2`].

#[cfg(target_arch = "x86_64")]
#[allow(unused_imports)]
use std::arch::x86_64::*;

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

/// Spare `u32` slots the writing kernels may touch past the values they emit.
pub const SLACK: usize = 16;

/// The low `n` bits set (`n <= 32`).
#[inline(always)]
pub fn low_mask32(n: usize) -> u32 {
    debug_assert!(n <= 32);
    ((1u64 << n) - 1) as u32
}

// ---------------------------------------------------------- NEON helpers

/// Lanes 0..16.
#[cfg(target_arch = "aarch64")]
static IOTA16: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// Bit `j % 8` for byte lane `j` (movemask weights).
#[cfg(target_arch = "aarch64")]
static LANE_BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];

/// Positions of the set bits of each byte value, in order, zero-filled (the
/// compaction table of the NEON writing kernels).
#[cfg(target_arch = "aarch64")]
pub(crate) static BYTE_POS: [[u8; 8]; 256] = {
    let mut t = [[0u8; 8]; 256];
    let mut m = 0;
    while m < 256 {
        let (mut b, mut k) = (0, 0);
        while b < 8 {
            if m & (1 << b) != 0 {
                t[m][k] = b as u8;
                k += 1;
            }
            b += 1;
        }
        m += 1;
    }
    t
};

/// One bit per byte lane of two compare results (`a`: bits 0..16, `b`:
/// 16..32): AND with the lane weights, then three pairwise adds.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn movemask32(a: uint8x16_t, b: uint8x16_t) -> u32 {
    let w = vld1q_u8(LANE_BITS.as_ptr());
    let s = vpaddq_u8(vandq_u8(a, w), vandq_u8(b, w));
    let s = vpaddq_u8(s, s);
    let s = vpaddq_u8(s, s);
    vgetq_lane_u32::<0>(vreinterpretq_u32_u8(s))
}

/// Whether any lane of a compare result is set (`shrn` to a nibble mask).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn any_lane(m: uint8x16_t) -> bool {
    vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(m)))) != 0
}

/// The `n <= 8` bytes at `p` in the low bytes of a word (the rest zero),
/// by in-bounds loads only: two overlapping 4-byte loads, or single bytes.
///
/// # Safety
/// `p..p + n` must be readable.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn load_le8(p: *const u8, n: usize) -> u64 {
    if n >= 4 {
        let lo = (p as *const u32).read_unaligned() as u64;
        let hi = (p.add(n - 4) as *const u32).read_unaligned() as u64;
        lo | (hi >> (8 * (8 - n))) << 32
    } else if n > 0 {
        *p as u64 | (*p.add(n / 2) as u64) << (8 * (n / 2)) | (*p.add(n - 1) as u64) << (8 * (n - 1))
    } else {
        0
    }
}

/// A word whose bytes are all among the `1 <= n <= 8` bytes at `p` and
/// include each of them (two overlapping 4-byte loads, or single bytes).
///
/// # Safety
/// `p..p + n` must be readable.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn cover8(p: *const u8, n: usize) -> u64 {
    if n >= 4 {
        (p as *const u32).read_unaligned() as u64 | ((p.add(n - 4) as *const u32).read_unaligned() as u64) << 32
    } else {
        let b = *p as u64 | (*p.add(n / 2) as u64) << 8 | (*p.add(n - 1) as u64) << 16 | (*p as u64) << 24;
        b | b << 32
    }
}

/// The `n <= 16` bytes at `p`, zero-padded, by in-bounds loads only.
///
/// # Safety
/// `p..p + n` must be readable.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) unsafe fn load_le16(p: *const u8, n: usize) -> uint8x16_t {
    if n > 8 {
        let lo = (p as *const u64).read_unaligned();
        let hi = (p.add(n - 8) as *const u64).read_unaligned() >> (8 * (16 - n));
        vcombine_u8(vcreate_u8(lo), vcreate_u8(hi))
    } else {
        vcombine_u8(vcreate_u8(load_le8(p, n)), vdup_n_u8(0))
    }
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
    #[cfg(target_arch = "aarch64")]
    {
        let a = select1_neon(w, k);
        (a, a + 1 + (w >> a >> 1).trailing_zeros())
    }
    #[cfg(not(any(all(target_arch = "x86_64", target_feature = "bmi2"), target_arch = "aarch64")))]
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
    #[cfg(target_arch = "aarch64")]
    {
        select1_neon(w, k)
    }
    #[cfg(not(any(all(target_arch = "x86_64", target_feature = "bmi2"), target_arch = "aarch64")))]
    select1_scalar(w, k)
}

/// aarch64 [`select1`] (no `pdep`): the byte popcounts (NEON `cnt`), summed
/// by a multiply, locate the byte holding the bit (SWAR compare with `k`);
/// [`BYTE_POS`] selects within it. Branch-free: about 3x the throughput of
/// the scalar loop on directory lookups, though a longer dependency chain.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub fn select1_neon(w: u64, k: u32) -> u32 {
    const L: u64 = 0x0101_0101_0101_0101;
    // Byte j of `s`: number of set bits in bytes 0..=j.
    // SAFETY: NEON is baseline on aarch64.
    let s = unsafe { vget_lane_u64::<0>(vreinterpret_u64_u8(vcnt_u8(vcreate_u8(w)))) }.wrapping_mul(L);
    // Byte j of `t` has its high bit set iff `s_j <= k`, i.e. for the
    // bytes below the one holding the bit, byte `b / 8`.
    let t = (k as u64 * L | L << 7) - s;
    let b = 64 - (t & L << 7).leading_zeros();
    b + BYTE_POS[(w >> b) as u8 as usize][(k - ((s << 8) >> b) as u8 as u32) as usize] as u32
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
    #[cfg(target_arch = "aarch64")]
    {
        bytes_contain_neon(p, n, x)
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl"),
        target_arch = "aarch64"
    )))]
    bytes_contain_scalar(p, n, x)
}

/// A readable stand-in for an empty array.
#[cfg(target_arch = "aarch64")]
static EMPTY8: [u8; 8] = [0; 8];

/// NEON [`bytes_contain`]: past 8 bytes, one byte compare of the array
/// covered by two overlapping in-bounds loads (repeated bytes are
/// harmless); up to 8, branch-free, the bytes at clamped indices as one
/// word and a SWAR zero-byte test.
///
/// # Safety
/// As for [`bytes_contain`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn bytes_contain_neon(p: *const u8, n: usize, x: u8) -> bool {
    if n <= 8 {
        const L: u64 = 0x0101_0101_0101_0101;
        let q = if n == 0 { EMPTY8.as_ptr() } else { p };
        let last = n.wrapping_sub(1);
        let mut w = 0u64;
        for i in 0..8 {
            w |= (*q.add(i.min(last)) as u64) << (8 * i);
        }
        let t = w ^ (x as u64 * L);
        (t.wrapping_sub(L) & !t & L << 7 != 0) & (n != 0)
    } else if n <= 16 {
        any_lane(vceqq_u8(vcombine_u8(vld1_u8(p), vld1_u8(p.add(n - 8))), vdupq_n_u8(x)))
    } else {
        let k = vdupq_n_u8(x);
        any_lane(vorrq_u8(vceqq_u8(vld1q_u8(p), k), vceqq_u8(vld1q_u8(p.add(n - 16)), k)))
    }
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
    #[cfg(target_arch = "aarch64")]
    {
        u16s_contain_neon(p, n, x)
    }
    #[cfg(not(any(all(target_arch = "x86_64", target_feature = "avx512bw"), target_arch = "aarch64")))]
    u16s_contain_scalar(p, n, x)
}

/// NEON [`u16s_contain`]: from 8 values, eight compares of 8 values loaded
/// at `8 i` clamped to `n - 8` (no loop; repeated values are harmless);
/// below, two overlapping 4-value loads, or three compares below 4.
///
/// # Safety
/// As for [`u16s_contain`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn u16s_contain_neon(p: *const u16, n: usize, x: u16) -> bool {
    let k = vdupq_n_u16(x);
    if n >= 8 {
        let eq = |i: usize| vceqq_u16(vld1q_u16(p.add((8 * i).min(n - 8))), k);
        let a = vorrq_u16(vorrq_u16(eq(0), eq(1)), vorrq_u16(eq(2), eq(3)));
        let b = vorrq_u16(vorrq_u16(eq(4), eq(5)), vorrq_u16(eq(6), eq(7)));
        any_lane(vreinterpretq_u8_u16(vorrq_u16(a, b)))
    } else if n >= 4 {
        any_lane(vreinterpretq_u8_u16(vceqq_u16(vcombine_u16(vld1_u16(p), vld1_u16(p.add(n - 4))), k)))
    } else {
        n > 0 && (*p == x) | (*p.add(n / 2) == x) | (*p.add(n - 1) == x)
    }
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
    #[cfg(target_arch = "aarch64")]
    {
        load_bytes32_neon(p, n)
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl"),
        target_arch = "aarch64"
    )))]
    load_bytes32_scalar(p, n)
}

/// NEON [`load_bytes32`]: past 16 bytes, the first 16 and the last 16
/// shifted into place by a table lookup (out-of-range lanes read zero);
/// otherwise [`load_le16`].
///
/// # Safety
/// As for [`load_bytes32`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn load_bytes32_neon(p: *const u8, n: usize) -> [u8; 32] {
    let mut d = [0u8; 32];
    if n > 16 {
        let idx = vaddq_u8(vld1q_u8(IOTA16.as_ptr()), vdupq_n_u8((32 - n) as u8));
        vst1q_u8(d.as_mut_ptr(), vld1q_u8(p));
        vst1q_u8(d.as_mut_ptr().add(16), vqtbl1q_u8(vld1q_u8(p.add(n - 16)), idx));
    } else {
        vst1q_u8(d.as_mut_ptr(), load_le16(p, n));
    }
    d
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
    #[cfg(target_arch = "aarch64")]
    {
        test_bits_neon(d, bm)
    }
    #[cfg(not(any(
        all(
            target_arch = "x86_64",
            target_feature = "avx512bw",
            target_feature = "avx512vl",
            target_feature = "avx512vbmi"
        ),
        target_arch = "aarch64"
    )))]
    test_bits_scalar(d, bm)
}

/// NEON [`test_bits`]: byte `d >> 3` of the bitmap by a two-register table
/// lookup (`tbl`), tested against `1 << (d & 7)`.
///
/// # Safety
/// As for [`test_bits`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn test_bits_neon(d: &[u8; 32], bm: *const u64) -> u32 {
    let t = vld1q_u8_x2(bm as *const u8);
    let one = vdupq_n_u8(1);
    let seven = vdupq_n_u8(7);
    let lane = |dv: uint8x16_t| {
        let byte = vqtbl2q_u8(t, vshrq_n_u8::<3>(dv));
        vtstq_u8(byte, vshlq_u8(one, vreinterpretq_s8_u8(vandq_u8(dv, seven))))
    };
    movemask32(lane(vld1q_u8(d.as_ptr())), lane(vld1q_u8(d.as_ptr().add(16))))
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
    #[cfg(target_arch = "aarch64")]
    {
        test_bytes_neon(d, nd, p, n)
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vl"),
        target_arch = "aarch64"
    )))]
    test_bytes_scalar(d, nd, p, n)
}

/// NEON [`test_bytes`], looping over the shorter side as on x86: each byte
/// at `p` compared with the 32 lanes of `d`, or each `d[j]` with the bytes
/// at `p` covered by two overlapping in-bounds loads.
///
/// # Safety
/// As for [`test_bytes`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn test_bytes_neon(d: &[u8; 32], nd: usize, p: *const u8, n: usize) -> u32 {
    if n <= nd {
        let (d0, d1) = (vld1q_u8(d.as_ptr()), vld1q_u8(d.as_ptr().add(16)));
        let (mut a, mut b) = (vdupq_n_u8(0), vdupq_n_u8(0));
        for i in 0..n {
            let o = vld1q_dup_u8(p.add(i));
            a = vorrq_u8(a, vceqq_u8(d0, o));
            b = vorrq_u8(b, vceqq_u8(d1, o));
        }
        movemask32(a, b) & low_mask32(nd)
    } else {
        // 1 <= n <= 32; repeated bytes are harmless.
        let (o0, o1) = if n > 16 {
            (vld1q_u8(p), vld1q_u8(p.add(n - 16)))
        } else if n > 8 {
            let o = vcombine_u8(vld1_u8(p), vld1_u8(p.add(n - 8)));
            (o, o)
        } else {
            let o = vreinterpretq_u8_u64(vdupq_n_u64(cover8(p, n)));
            (o, o)
        };
        let mut r = 0u32;
        for (j, &x) in d.iter().enumerate().take(nd) {
            let k = vdupq_n_u8(x);
            r |= (any_lane(vorrq_u8(vceqq_u8(o0, k), vceqq_u8(o1, k))) as u32) << j;
        }
        r
    }
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
    #[cfg(target_arch = "aarch64")]
    {
        emit_lanes_neon(d, hits, base, dst)
    }
    #[cfg(not(any(all(target_arch = "x86_64", target_feature = "avx512f"), target_arch = "aarch64")))]
    emit_lanes_scalar(d, hits, base, dst)
}

/// NEON [`emit_lanes`]: per 8 lanes, the selected bytes compacted by a
/// `tbl` lookup with [`BYTE_POS`], widened and stored as 8 values at the
/// prefix popcount of the lower lanes.
///
/// # Safety
/// As for [`emit_lanes`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn emit_lanes_neon(d: &[u8; 32], hits: u32, base: u32, dst: *mut u32) -> usize {
    let vb = vdupq_n_u32(base);
    // Byte j of `s`: number of hits in lanes `0..8 * (j + 1)`.
    let s = vget_lane_u64::<0>(vreinterpret_u64_u8(vcnt_u8(vcreate_u8(hits as u64)))).wrapping_mul(0x0101_0101_0101_0101);
    let off = s << 8;
    for g in 0..4 {
        let m = (hits >> (8 * g)) as u8;
        let v = vmovl_u8(vtbl1_u8(vld1_u8(d.as_ptr().add(8 * g)), vld1_u8(BYTE_POS[m as usize].as_ptr())));
        let o = dst.add((off >> (8 * g)) as u8 as usize);
        vst1q_u32(o, vaddw_u16(vb, vget_low_u16(v)));
        vst1q_u32(o.add(4), vaddw_high_u16(vb, v));
    }
    (s >> 24) as u8 as usize
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
    #[cfg(target_arch = "aarch64")]
    {
        extract256_neon(w, base, dst)
    }
    #[cfg(not(any(
        all(
            target_arch = "x86_64",
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512vl"
        ),
        target_arch = "aarch64"
    )))]
    extract256_scalar(w, base, dst)
}

/// NEON [`extract256`], per word: with at most 4 set bits, four
/// unconditional scalar writes; otherwise the positions of each byte's set
/// bits from [`BYTE_POS`], widened, plus the base, stored as 8 values at the
/// prefix popcount (`cnt` and a multiply) of the lower bytes.
///
/// # Safety
/// As for [`extract256`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn extract256_neon(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let step = vdupq_n_u32(8);
    let mut k = 0usize;
    for (i, &x) in w.iter().enumerate() {
        let b = base + 64 * i as u32;
        // Byte j of `s`: number of set bits in bytes 0..=j.
        let s = vget_lane_u64::<0>(vreinterpret_u64_u8(vcnt_u8(vcreate_u8(x)))).wrapping_mul(0x0101_0101_0101_0101);
        let c = (s >> 56) as usize;
        let o = dst.add(k);
        if c <= 4 {
            let mut x = x;
            for j in 0..4 {
                o.add(j).write(b.wrapping_add(x.trailing_zeros()));
                x &= x.wrapping_sub(1);
            }
        } else {
            let off = s << 8;
            let mut vb = vdupq_n_u32(b);
            for j in 0..8 {
                let pos = vmovl_u8(vld1_u8(BYTE_POS[(x >> (8 * j)) as u8 as usize].as_ptr()));
                let q = o.add((off >> (8 * j)) as u8 as usize);
                vst1q_u32(q, vaddw_u16(vb, vget_low_u16(pos)));
                vst1q_u32(q.add(4), vaddw_high_u16(vb, pos));
                vb = vaddq_u32(vb, step);
            }
        }
        k += c;
    }
    k
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
    #[cfg(target_arch = "aarch64")]
    {
        widen_bytes_neon(p, n, base, dst)
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512bw", target_feature = "avx512vl"),
        target_arch = "aarch64"
    )))]
    widen_bytes_scalar(p, n, base, dst)
}

/// NEON [`widen_bytes`]: 16 bytes at a time widened to `u32` (`uxtl`,
/// `uaddw`); the last 16 end at `p + n` (overlapping the previous ones), or
/// below 16 bytes [`load_le16`].
///
/// # Safety
/// As for [`widen_bytes`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn widen_bytes_neon(p: *const u8, n: usize, base: u32, dst: *mut u32) {
    let vb = vdupq_n_u32(base);
    let put = |v: uint8x16_t, o: *mut u32| {
        let (lo, hi) = (vmovl_u8(vget_low_u8(v)), vmovl_high_u8(v));
        vst1q_u32(o, vaddw_u16(vb, vget_low_u16(lo)));
        vst1q_u32(o.add(4), vaddw_high_u16(vb, lo));
        vst1q_u32(o.add(8), vaddw_u16(vb, vget_low_u16(hi)));
        vst1q_u32(o.add(12), vaddw_high_u16(vb, hi));
    };
    if n < 16 {
        put(load_le16(p, n), dst);
        return;
    }
    let mut i = 0;
    while i + 16 < n {
        put(vld1q_u8(p.add(i)), dst.add(i));
        i += 16;
    }
    put(vld1q_u8(p.add(n - 16)), dst.add(n - 16));
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
/// `vpopcntd`) used as a `vpermt2d` index into the block ids. NEON: the
/// same with byte lanes ([`expand_sparse_neon`]).
///
/// # Safety
/// `lows..lows + n` readable; `U` readable 8 bytes past bit `pos + n`;
/// `blk[-1..r_max + 32]` readable where `r_max` is the last block's index,
/// holding block ids (below 256; NEON reads their low bytes); `dst` has
/// room for `n + SLACK` values.
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
    #[cfg(target_arch = "aarch64")]
    {
        expand_sparse_neon(lows, u, pos, n, blk, base, dst)
    }
    #[cfg(not(any(
        all(
            target_arch = "x86_64",
            target_feature = "avx512f",
            target_feature = "avx512bw",
            target_feature = "avx512vl",
            target_feature = "avx512vpopcntdq"
        ),
        target_arch = "aarch64"
    )))]
    expand_sparse_scalar(lows, u, pos, n, blk, base, dst)
}

/// Lane `j` reads byte `j / 8` (spreads 16 bits over 16 byte lanes).
#[cfg(target_arch = "aarch64")]
static SPREAD16: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1];

/// NEON [`expand_sparse`]: 16 values per step; the block of each lane is
/// an inclusive prefix count of 16 bits of `U` (bits spread over byte lanes
/// with `tbl` and `cmtst`, four shifted adds), used as a `tbl` index into
/// the low bytes of the next 17 block ids, which are zipped with the low
/// bytes and widened.
///
/// # Safety
/// As for [`expand_sparse`].
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn expand_sparse_neon(lows: *const u8, u: *const u64, pos: u32, n: usize, blk: *const u32, base: u32, dst: *mut u32) {
    let spread = vld1q_u8(SPREAD16.as_ptr());
    let bits = vld1q_u8(LANE_BITS.as_ptr());
    let z = vdupq_n_u8(0);
    let vb = vdupq_n_u32(base);
    let ub = u as *const u8;
    let mut carry = 0isize;
    let mut i = 0usize;
    while i < n {
        let bp = pos as usize + i;
        let m = ((ub.add(bp >> 3) as *const u64).read_unaligned() >> (bp & 7)) & 0xFFFF;
        // Lanes of set bits are -1; prefix sums, negated.
        let mut c = vtstq_u8(vqtbl1q_u8(vreinterpretq_u8_u64(vdupq_n_u64(m)), spread), bits);
        c = vaddq_u8(c, vextq_u8::<15>(z, c));
        c = vaddq_u8(c, vextq_u8::<14>(z, c));
        c = vaddq_u8(c, vextq_u8::<12>(z, c));
        c = vaddq_u8(c, vextq_u8::<8>(z, c));
        let r = vsubq_u8(z, c);
        // Low bytes of block ids t[0..=16].
        let t = blk.offset(carry - 1);
        let q = vld1q_u32_x4(t);
        let ids = uint8x16x2_t(
            vuzp1q_u8(
                vreinterpretq_u8_u16(vuzp1q_u16(vreinterpretq_u16_u32(q.0), vreinterpretq_u16_u32(q.1))),
                vreinterpretq_u8_u16(vuzp1q_u16(vreinterpretq_u16_u32(q.2), vreinterpretq_u16_u32(q.3))),
            ),
            vld1q_dup_u8(t.add(16) as *const u8),
        );
        let id = vqtbl2q_u8(ids, r);
        let l = if n - i >= 16 { vld1q_u8(lows.add(i)) } else { load_le16(lows.add(i), n - i) };
        // `low | id << 8` as u16 lanes, widened, ORed with the base.
        let (lo, hi) = (vreinterpretq_u16_u8(vzip1q_u8(l, id)), vreinterpretq_u16_u8(vzip2q_u8(l, id)));
        let o = dst.add(i);
        vst1q_u32(o, vorrq_u32(vmovl_u16(vget_low_u16(lo)), vb));
        vst1q_u32(o.add(4), vorrq_u32(vmovl_high_u16(lo), vb));
        vst1q_u32(o.add(8), vorrq_u32(vmovl_u16(vget_low_u16(hi)), vb));
        vst1q_u32(o.add(12), vorrq_u32(vmovl_high_u16(hi), vb));
        carry += vgetq_lane_u8::<15>(r) as isize;
        i += 16;
    }
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
            #[cfg(target_arch = "aarch64")]
            assert_eq!(select1_neon(w, k), want[k as usize]);
        }
        // Every rank of dense, top-heavy and single-byte words.
        for w in [!0u64, 3 << 62, 0x8000_0000_0000_0001, 0xFF00_0000_0000_0000, 0x00FF_0000_0000_FF00, 0x5555_5555_5555_5555] {
            let want: Vec<u32> = (0..64).filter(|b| w >> b & 1 == 1).collect();
            for k in 0..want.len() as u32 {
                assert_eq!(select1(w, k), want[k as usize]);
                assert_eq!(select1_scalar(w, k), want[k as usize]);
                #[cfg(target_arch = "aarch64")]
                assert_eq!(select1_neon(w, k), want[k as usize]);
                if k + 1 < want.len() as u32 {
                    assert_eq!(select2(w, k), (want[k as usize], want[k as usize + 1]));
                }
            }
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
                    #[cfg(target_arch = "aarch64")]
                    {
                        assert_eq!(load_bytes32_neon(v.as_ptr(), n), load_bytes32_scalar(v.as_ptr(), n));
                        assert_eq!(bytes_contain_neon(v.as_ptr(), n, x), want);
                    }
                }
            }
        }
        // Every size, with the extreme bytes, at the end of an exact-size
        // allocation.
        for n in 0..=32usize {
            let v: Vec<u8> = (0..n).map(|i| if i + 1 == n { 255 } else { i as u8 }).collect::<Box<[u8]>>().into();
            for x in [0u8, 1, n as u8, 254, 255] {
                unsafe {
                    assert_eq!(bytes_contain(v.as_ptr(), n, x), v.contains(&x));
                    assert_eq!(bytes_count_lt(v.as_ptr(), n, x), v.iter().filter(|&&b| b < x).count());
                    assert_eq!(load_bytes32(v.as_ptr(), n), load_bytes32_scalar(v.as_ptr(), n));
                }
            }
        }
        // Planned probes of non-sparse blocks pass no array.
        #[cfg(target_arch = "aarch64")]
        unsafe {
            assert!(!bytes_contain_neon(std::ptr::null(), 0, 0));
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
                    #[cfg(target_arch = "aarch64")]
                    assert_eq!(u16s_contain_neon(v.as_ptr(), v.len(), x), want);
                }
            }
        }
        // Every size, with the extreme values.
        for n in 0..=64usize {
            let v: Vec<u16> = (0..n).map(|i| if i == 0 { 0 } else if i + 1 == n { u16::MAX } else { 7 * i as u16 }).collect();
            for x in [0u16, 1, 7, 7 * (n as u16 / 2), u16::MAX - 1, u16::MAX] {
                unsafe {
                    assert_eq!(u16s_contain(v.as_ptr(), n, x), v.contains(&x));
                    assert_eq!(u16s_contain_scalar(v.as_ptr(), n, x), v.contains(&x));
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
                #[cfg(target_arch = "aarch64")]
                {
                    assert_eq!(test_bits_neon(&d, bm.as_ptr()) & valid, test_bits_scalar(&d, bm.as_ptr()) & valid);
                    assert_eq!(test_bytes_neon(&d, nd, o.as_ptr(), n), want);
                }
            }
        }
        // Full and empty sides, extreme bytes, all-ones and single-bit bitmaps.
        let all: Vec<u8> = (0..=255u8).collect();
        for (nd, n) in [(32, 32), (32, 0), (1, 32), (32, 1), (16, 16), (17, 9), (9, 17)] {
            for shift in [0usize, 224] {
                let mut d = [0u8; 32];
                d[..nd].copy_from_slice(&all[shift..shift + nd]);
                let o = &all[256 - n..];
                let want: u32 = (0..nd).fold(0, |r, j| r | (o.contains(&d[j]) as u32) << j);
                unsafe {
                    assert_eq!(test_bytes(&d, nd, o.as_ptr(), n), want);
                    assert_eq!(test_bytes_scalar(&d, nd, o.as_ptr(), n), want);
                }
            }
        }
        for bm in [[!0u64; 4], [0; 4], [1, 0, 0, 1 << 63], [0, 1 << 63, 1, 0]] {
            let d: [u8; 32] = std::array::from_fn(|j| [0u8, 63, 64, 127, 128, 191, 192, 255][j % 8] ^ (j / 8) as u8);
            unsafe {
                assert_eq!(test_bits(&d, bm.as_ptr()), test_bits_scalar(&d, bm.as_ptr()));
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
            #[cfg(target_arch = "aarch64")]
            unsafe {
                expand_sparse_neon(lows.as_ptr(), u.as_ptr(), lead as u32, n, blk.as_ptr().add(1), base, a.as_mut_ptr());
                assert_eq!(a[..n], want[..]);
            }
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
                for hits in [hits, 0, !0, 1 << 31] {
                    let n = emit_lanes(&d, hits, base, a.as_mut_ptr());
                    let m = emit_lanes_scalar(&d, hits, base, b.as_mut_ptr());
                    assert_eq!(n, m);
                    assert_eq!(a[..n], b[..n]);
                    let want: Vec<u32> = (0..32).filter(|j| hits >> j & 1 == 1).map(|j| base + d[j] as u32).collect();
                    assert_eq!(a[..n], want[..]);
                    #[cfg(target_arch = "aarch64")]
                    {
                        let n = emit_lanes_neon(&d, hits, base, a.as_mut_ptr());
                        assert_eq!(a[..n], want[..]);
                    }
                }

                let density = rng.gen_range(0.0..1.0);
                let mut w: [u64; 4] =
                    std::array::from_fn(|_| (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(density) as u64) << b)));
                // Words of 4 and 5 bits (either side of the NEON sparse-word cut).
                if rng.gen_bool(0.2) {
                    for x in w.iter_mut() {
                        *x = 0;
                        while x.count_ones() < rng.gen_range(4..=5) {
                            *x |= 1 << rng.gen_range(0..64);
                        }
                    }
                }
                let w = match rng.gen_range(0..40) {
                    0 => [!0; 4],
                    1 => [0; 4],
                    2 => [1, 0, 0, 1 << 63],
                    _ => w,
                };
                let n = extract256(&w, base, a.as_mut_ptr());
                let want: Vec<u32> =
                    (0..256u32).filter(|&i| w[(i >> 6) as usize] >> (i & 63) & 1 == 1).map(|i| base + i).collect();
                assert_eq!(a[..n], want[..]);
                let m = extract256_scalar(&w, base, b.as_mut_ptr());
                assert_eq!(b[..m], want[..]);
                let m = extract_words(&w, base, b.as_mut_ptr());
                assert_eq!(b[..m], want[..]);
                #[cfg(target_arch = "aarch64")]
                {
                    let m = extract256_neon(&w, base, b.as_mut_ptr());
                    assert_eq!(b[..m], want[..]);
                }
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
                #[cfg(target_arch = "aarch64")]
                {
                    widen_bytes_neon(bytes.as_ptr(), n, base, a.as_mut_ptr());
                    assert_eq!(a[..n], want[..]);
                }
            }
        }
    }
}
