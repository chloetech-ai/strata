//! Distance kernels.
//!
//! * [`dot_f32`]: exact float32 dot product (AVX-512 FMA, four accumulators;
//!   AVX2 FMA, four; NEON FMA, eight).
//! * [`dot_u8_i8`], [`dot_u8_i8_x4`]: SQ8 codes against an int8-quantised
//!   query, summed in int32 with AVX-512 VNNI (`vpdpbusd`: 64 byte products
//!   per instruction), with AVX2 by widening to 16 bits (`vpmaddwd`: 16 per
//!   instruction), or on aarch64 with `usdot` (i8mm: 16 per instruction;
//!   with dotprod only, `sdot` on the codes biased to signed and a
//!   correction, see `dot4`). The `x4` form scores four rows per query
//!   load, which is what scans use.
//! * [`dot_u8_f32`]: SQ8 codes against a float32 query (asymmetric: widen the
//!   codes, then FMA). Kept for the kernel comparison in the bench; scans use
//!   the int8 kernel (VNNI or NEON; see `notes/vector.md`).
//! * [`dot_f32_4x4`]: a 4×4 block of float32 dot products sharing loads, for
//!   k-means and brute-force ground truth.
//! * [`dot_bits_u8`], [`dot_bits_u8_x4`]: a binary code against a query of
//!   a byte per dimension (AVX-512BW masked loads and `vpsadbw`; on aarch64
//!   the code's bits spread to byte masks with `tbl` and `cmtst`, then
//!   `sdot`; elsewhere popcounts of the query's bit planes).
//!
//! The SIMD paths are chosen at run time from the CPU's features
//! ([`crate::simd`]), so a binary built for a generic CPU still uses
//! AVX-512 or the dot product instructions where it runs on them, and one
//! built with `-C target-cpu=native` decides at compile time. Every kernel
//! has a portable scalar version, used on other CPUs and as the reference
//! in tests. Lengths need not be multiples of the vector width: tails use
//! masked loads (AVX-512) or a scalar loop (AVX2, NEON). [`sq8_kernel`]
//! names the int8 kernel in use.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

use crate::simd;

/// The int8 SQ8 kernel this process runs ([`dot_u8_i8`]), for reports.
pub fn sq8_kernel() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        if simd::avx512vnni() {
            "vnni"
        } else if simd::avx2() {
            "avx2"
        } else {
            "scalar"
        }
    } else if cfg!(all(target_arch = "aarch64", target_feature = "i8mm")) {
        "neon-usdot"
    } else if simd::dotprod() {
        "neon-sdot"
    } else {
        "scalar"
    }
}

/// Exact dot product of two float32 vectors of equal length.
#[inline]
pub fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if simd::avx512() {
            // SAFETY: the CPU has AVX-512; both slices hold `a.len()`
            // elements; tails use masked loads.
            return unsafe { dot_f32_avx512(a.as_ptr(), b.as_ptr(), a.len()) };
        }
        if simd::avx2() {
            // SAFETY: the CPU has AVX2 and FMA; both slices hold `a.len()`
            // elements; the tail is scalar.
            return unsafe { dot_f32_avx2(a.as_ptr(), b.as_ptr(), a.len()) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: both slices hold `a.len()` elements; the tail is scalar.
    unsafe {
        dot_f32_neon(a.as_ptr(), b.as_ptr(), a.len())
    }
    #[cfg(not(target_arch = "aarch64"))]
    dot_f32_scalar(a, b)
}

/// Portable [`dot_f32`] (eight partial sums, so it vectorises too).
pub fn dot_f32_scalar(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    let mut acc = [0f32; 8];
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    let tail: f32 = ca.remainder().iter().zip(cb.remainder()).map(|(x, y)| x * y).sum();
    for (x, y) in ca.zip(cb) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    acc.iter().sum::<f32>() + tail
}

/// Dot product of unsigned 8-bit codes with signed 8-bit query weights.
#[inline]
pub fn dot_u8_i8(a: &[u8], b: &[i8]) -> i32 {
    assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if simd::avx512vnni() {
            // SAFETY: the CPU has AVX-512 VNNI; both slices hold `a.len()`
            // bytes; tails use masked loads.
            return unsafe { dot_u8_i8_vnni(a.as_ptr(), b.as_ptr(), a.len()) };
        }
        if simd::avx2() {
            // SAFETY: the CPU has AVX2; both slices hold `a.len()` bytes;
            // the tail is scalar.
            return unsafe { dot_u8_i8_avx2(a.as_ptr(), b.as_ptr(), a.len()) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    if simd::dotprod() {
        // SAFETY: the CPU has the dot product instructions; both slices
        // hold `a.len()` bytes; the tail is scalar.
        return unsafe { dot_u8_i8_neon(a.as_ptr(), b.as_ptr(), a.len()) };
    }
    dot_u8_i8_scalar(a, b)
}

/// Portable [`dot_u8_i8`].
pub fn dot_u8_i8_scalar(a: &[u8], b: &[i8]) -> i32 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(&x, &y)| x as i32 * y as i32).sum()
}

/// [`dot_u8_i8`] of four code rows against one query, loading each query
/// chunk once.
#[inline]
pub fn dot_u8_i8_x4(rows: [&[u8]; 4], q: &[i8]) -> [i32; 4] {
    for r in rows {
        assert_eq!(r.len(), q.len());
    }
    #[cfg(target_arch = "x86_64")]
    {
        if simd::avx512vnni() {
            // SAFETY: the CPU has AVX-512 VNNI; every row holds `q.len()`
            // bytes; tails use masked loads.
            return unsafe { dot_u8_i8_x4_vnni(rows.map(|r| r.as_ptr()), q.as_ptr(), q.len()) };
        }
        if simd::avx2() {
            // SAFETY: the CPU has AVX2; every row holds `q.len()` bytes; the
            // tail is scalar.
            return unsafe { dot_u8_i8_x4_avx2(rows.map(|r| r.as_ptr()), q.as_ptr(), q.len()) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    if simd::dotprod() {
        // SAFETY: the CPU has the dot product instructions; every row holds
        // `q.len()` bytes; the tail is scalar.
        return unsafe { dot_u8_i8_x4_neon(rows.map(|r| r.as_ptr()), q.as_ptr(), q.len()) };
    }
    rows.map(|r| dot_u8_i8_scalar(r, q))
}

/// Dot product of unsigned 8-bit codes with float32 weights.
#[inline]
pub fn dot_u8_f32(a: &[u8], w: &[f32]) -> f32 {
    assert_eq!(a.len(), w.len());
    #[cfg(target_arch = "x86_64")]
    if simd::avx512() {
        // SAFETY: the CPU has AVX-512; `a` holds `w.len()` bytes; tails use
        // masked loads.
        return unsafe { dot_u8_f32_avx512(a.as_ptr(), w.as_ptr(), a.len()) };
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: `a` holds `w.len()` bytes; the tail is scalar.
    unsafe {
        dot_u8_f32_neon(a.as_ptr(), w.as_ptr(), a.len())
    }
    #[cfg(not(target_arch = "aarch64"))]
    dot_u8_f32_scalar(a, w)
}

/// Portable [`dot_u8_f32`].
pub fn dot_u8_f32_scalar(a: &[u8], w: &[f32]) -> f32 {
    assert_eq!(a.len(), w.len());
    let mut acc = [0f32; 8];
    let (ca, cw) = (a.chunks_exact(8), w.chunks_exact(8));
    let tail: f32 = ca.remainder().iter().zip(cw.remainder()).map(|(&x, y)| x as f32 * y).sum();
    for (x, y) in ca.zip(cw) {
        for i in 0..8 {
            acc[i] += x[i] as f32 * y[i];
        }
    }
    acc.iter().sum::<f32>() + tail
}

/// A query for [`dot_bits_u8`]: the same weights three ways, one per kernel.
#[derive(Clone, Copy, Debug)]
pub struct BitsWeights<'a> {
    /// A byte per dimension, `v[j] + 128`, 64 per code word (AVX-512).
    pub u: &'a [u8],
    /// The signed weights `v[j]`, 64 per code word (NEON).
    pub v: &'a [i8],
    /// The eight bit planes of `u` per word, `planes[8w + p]` (portable).
    pub planes: &'a [u64],
}

/// Score of a binary code (see [`crate::vector::bits`]): the sum of the
/// weights `v[j] = u[j] - 128` over the set bits `j` of `code`, a row of
/// little-endian u64 words. AVX-512 loads the bytes of `u` under the code as
/// a mask and sums them with `vpsadbw`. NEON spreads each 16 bits of the code
/// to 16 byte masks (0 or -1: `tbl` repeats each byte 8 times, `cmtst` picks
/// one bit per lane) and sums mask times `v` with `sdot`, 64 dimensions in
/// 13 instructions. Elsewhere each bit plane of `u` is ANDed with the code
/// and counted.
#[inline]
pub fn dot_bits_u8(code: &[u8], q: BitsWeights) -> i32 {
    let words = code.len() / 8;
    assert!(code.len().is_multiple_of(8) && q.u.len() >= 64 * words && q.v.len() >= 64 * words && q.planes.len() >= 8 * words);
    #[cfg(target_arch = "x86_64")]
    if simd::avx512() {
        // SAFETY: the CPU has AVX-512; `code` holds `words` u64s and `u` 64
        // bytes per word.
        return unsafe { dot_bits_avx512(code.as_ptr(), q.u.as_ptr(), words) };
    }
    #[cfg(target_arch = "aarch64")]
    if simd::dotprod() {
        // SAFETY: the CPU has the dot product instructions; `code` holds
        // `words` u64s and `v` 64 bytes per word.
        return unsafe { dot_bits_neon([code.as_ptr()], q.v.as_ptr(), words)[0] };
    }
    dot_bits_planes(code, q.planes)
}

/// [`dot_bits_u8`] of four code rows (of equal length), loading the query
/// once per word for all four on aarch64.
#[inline]
pub fn dot_bits_u8_x4(codes: [&[u8]; 4], q: BitsWeights) -> [i32; 4] {
    #[cfg(target_arch = "aarch64")]
    if simd::dotprod() {
        let words = codes[0].len() / 8;
        assert!(codes.iter().all(|c| c.len() == 8 * words) && q.v.len() >= 64 * words);
        // SAFETY: the CPU has the dot product instructions; every row holds
        // `words` u64s and `v` 64 bytes per word.
        return unsafe { dot_bits_neon(codes.map(|c| c.as_ptr()), q.v.as_ptr(), words) };
    }
    codes.map(|c| dot_bits_u8(c, q))
}

/// [`dot_bits_u8`] by bit planes: nine popcounts per 64 dimensions.
pub fn dot_bits_planes(code: &[u8], planes: &[u64]) -> i32 {
    let mut s = 0i64;
    for (w, c) in code.chunks_exact(8).enumerate() {
        let c = u64::from_le_bytes(c.try_into().expect("8 bytes"));
        let p = &planes[8 * w..8 * w + 8];
        let mut t = 0i64;
        for (b, &plane) in p.iter().enumerate() {
            t += ((plane & c).count_ones() as i64) << b;
        }
        s += t - 128 * c.count_ones() as i64;
    }
    s as i32
}

/// Reference [`dot_bits_u8`], one set bit at a time.
pub fn dot_bits_u8_scalar(code: &[u8], u: &[u8]) -> i32 {
    let mut s = 0i32;
    for (i, &byte) in code.iter().enumerate() {
        for b in 0..8 {
            if byte >> b & 1 == 1 {
                s += u[8 * i + b] as i32 - 128;
            }
        }
    }
    s
}

/// All 16 dot products `a[i] · b[j]` of four vectors against four others
/// (equal lengths). Each loaded chunk feeds four FMAs, so this runs near the
/// FMA throughput instead of the load throughput.
#[inline]
pub fn dot_f32_4x4(a: [&[f32]; 4], b: [&[f32]; 4]) -> [[f32; 4]; 4] {
    let n = a[0].len();
    for v in a.iter().chain(&b) {
        assert_eq!(v.len(), n);
    }
    #[cfg(target_arch = "x86_64")]
    if simd::avx512() {
        // SAFETY: the CPU has AVX-512; all eight vectors hold `n` elements;
        // tails use masked loads.
        return unsafe { dot_f32_4x4_avx512(a.map(|v| v.as_ptr()), b.map(|v| v.as_ptr()), n) };
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: all eight vectors hold `n` elements; the tail is scalar.
    unsafe {
        dot_f32_4x4_neon(a.map(|v| v.as_ptr()), b.map(|v| v.as_ptr()), n)
    }
    #[cfg(not(target_arch = "aarch64"))]
    a.map(|x| b.map(|y| dot_f32(x, y)))
}

/// Hints the CPU to fetch `bytes` starting at `p` into cache (one hint per
/// 64-byte line; 128-byte on Apple silicon). Used ahead of random accesses
/// (graph neighbours, sparse filter members).
#[inline]
pub fn prefetch(p: *const u8, bytes: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let mut off = 0;
        while off < bytes {
            // SAFETY: prefetch never faults, whatever the address.
            unsafe { std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(p.wrapping_add(off) as *const i8) };
            off += 64;
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        let line = if cfg!(target_vendor = "apple") { 128 } else { 64 };
        let mut off = 0;
        while off < bytes {
            // SAFETY: prefetch never faults, whatever the address
            // (`_prefetch` is not stable yet).
            unsafe { std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p.wrapping_add(off), options(nostack, readonly, preserves_flags)) };
            off += line;
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = (p, bytes);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl")]
unsafe fn dot_f32_avx512(a: *const f32, b: *const f32, n: usize) -> f32 {
    let mut s = [_mm512_setzero_ps(); 4];
    let mut i = 0;
    while i + 64 <= n {
        for (j, acc) in s.iter_mut().enumerate() {
            *acc = _mm512_fmadd_ps(_mm512_loadu_ps(a.add(i + 16 * j)), _mm512_loadu_ps(b.add(i + 16 * j)), *acc);
        }
        i += 64;
    }
    while i + 16 <= n {
        s[0] = _mm512_fmadd_ps(_mm512_loadu_ps(a.add(i)), _mm512_loadu_ps(b.add(i)), s[0]);
        i += 16;
    }
    if i < n {
        let m: __mmask16 = (1u16 << (n - i)) - 1;
        s[1] = _mm512_fmadd_ps(_mm512_maskz_loadu_ps(m, a.add(i)), _mm512_maskz_loadu_ps(m, b.add(i)), s[1]);
    }
    _mm512_reduce_add_ps(_mm512_add_ps(_mm512_add_ps(s[0], s[1]), _mm512_add_ps(s[2], s[3])))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl")]
unsafe fn dot_bits_avx512(code: *const u8, u: *const u8, words: usize) -> i32 {
    let zero = _mm512_setzero_si512();
    let mut acc = [zero; 2];
    let mut set = 0u64;
    for w in 0..words {
        let m = (code.add(8 * w) as *const u64).read_unaligned();
        let x = _mm512_maskz_loadu_epi8(m, u.add(64 * w) as *const i8);
        acc[w & 1] = _mm512_add_epi64(acc[w & 1], _mm512_sad_epu8(x, zero));
        set += m.count_ones() as u64;
    }
    (_mm512_reduce_add_epi64(_mm512_add_epi64(acc[0], acc[1])) - 128 * set as i64) as i32
}

/// `acc` plus, per 32-bit lane, the dot product of four signed bytes with
/// four others (`sdot`; the intrinsic is not stable yet).
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "dotprod")]
#[inline]
unsafe fn sdot(mut acc: int32x4_t, a: int8x16_t, b: int8x16_t) -> int32x4_t {
    std::arch::asm!("sdot {0:v}.4s, {1:v}.16b, {2:v}.16b", inout(vreg) acc, in(vreg) a, in(vreg) b, options(pure, nomem, nostack, preserves_flags));
    acc
}

/// [`dot_bits_u8`] of `R` rows. For lanes `16k..16k+16` of a 64-bit word,
/// `tbl` repeats code bytes `2k` and `2k+1` eight times each and `cmtst`
/// against bits 1, 2, ..., 128 gives -1 where the dimension's bit is set, so
/// `sdot` of the mask with the weights adds `-v[j]` over the set bits.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "dotprod")]
#[inline]
unsafe fn dot_bits_neon<const R: usize>(codes: [*const u8; R], v: *const i8, words: usize) -> [i32; R] {
    const SPREAD: [[u8; 16]; 4] = {
        let mut t = [[0u8; 16]; 4];
        let mut k = 0;
        while k < 4 {
            let mut i = 0;
            while i < 16 {
                t[k][i] = (2 * k + i / 8) as u8;
                i += 1;
            }
            k += 1;
        }
        t
    };
    const SELECT: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    let spread = SPREAD.map(|t| vld1q_u8(t.as_ptr()));
    let select = vld1q_u8(SELECT.as_ptr());
    // Two chains per row, so one row alone is not bound by sdot's latency.
    let mut acc = [[vdupq_n_s32(0); 2]; R];
    for w in 0..words {
        let q = [0, 1, 2, 3].map(|k| vld1q_s8(v.add(64 * w + 16 * k)));
        for (r, a) in acc.iter_mut().enumerate() {
            let c = vreinterpretq_u8_u64(vld1q_dup_u64(codes[r].add(8 * w) as *const u64));
            for k in 0..4 {
                let m = vtstq_u8(vqtbl1q_u8(c, spread[k]), select);
                a[k & 1] = sdot(a[k & 1], vreinterpretq_s8_u8(m), q[k]);
            }
        }
    }
    acc.map(|a| -vaddvq_s32(vaddq_s32(a[0], a[1])))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512vnni")]
unsafe fn dot_u8_i8_vnni(a: *const u8, b: *const i8, n: usize) -> i32 {
    let mut s = [_mm512_setzero_si512(); 4];
    let mut i = 0;
    while i + 256 <= n {
        for (j, acc) in s.iter_mut().enumerate() {
            let x = _mm512_loadu_epi8(a.add(i + 64 * j) as *const i8);
            *acc = _mm512_dpbusd_epi32(*acc, x, _mm512_loadu_epi8(b.add(i + 64 * j)));
        }
        i += 256;
    }
    while i + 64 <= n {
        s[0] = _mm512_dpbusd_epi32(s[0], _mm512_loadu_epi8(a.add(i) as *const i8), _mm512_loadu_epi8(b.add(i)));
        i += 64;
    }
    if i < n {
        let m: __mmask64 = (1u64 << (n - i)) - 1;
        s[1] = _mm512_dpbusd_epi32(s[1], _mm512_maskz_loadu_epi8(m, a.add(i) as *const i8), _mm512_maskz_loadu_epi8(m, b.add(i)));
    }
    _mm512_reduce_add_epi32(_mm512_add_epi32(_mm512_add_epi32(s[0], s[1]), _mm512_add_epi32(s[2], s[3])))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512vnni")]
unsafe fn dot_u8_i8_x4_vnni(rows: [*const u8; 4], q: *const i8, n: usize) -> [i32; 4] {
    let mut s = [_mm512_setzero_si512(); 4];
    let mut i = 0;
    while i + 64 <= n {
        let qv = _mm512_loadu_epi8(q.add(i));
        for (acc, r) in s.iter_mut().zip(rows) {
            *acc = _mm512_dpbusd_epi32(*acc, _mm512_loadu_epi8(r.add(i) as *const i8), qv);
        }
        i += 64;
    }
    if i < n {
        let m: __mmask64 = (1u64 << (n - i)) - 1;
        let qv = _mm512_maskz_loadu_epi8(m, q.add(i));
        for (acc, r) in s.iter_mut().zip(rows) {
            *acc = _mm512_dpbusd_epi32(*acc, _mm512_maskz_loadu_epi8(m, r.add(i) as *const i8), qv);
        }
    }
    s.map(|v| _mm512_reduce_add_epi32(v))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl")]
unsafe fn dot_u8_f32_avx512(a: *const u8, w: *const f32, n: usize) -> f32 {
    let widen = |p: *const u8| _mm512_cvtepi32_ps(_mm512_cvtepu8_epi32(_mm_loadu_si128(p as *const __m128i)));
    let mut s = [_mm512_setzero_ps(); 4];
    let mut i = 0;
    while i + 64 <= n {
        for (j, acc) in s.iter_mut().enumerate() {
            *acc = _mm512_fmadd_ps(widen(a.add(i + 16 * j)), _mm512_loadu_ps(w.add(i + 16 * j)), *acc);
        }
        i += 64;
    }
    while i < n {
        let r = (n - i).min(16);
        let m: __mmask16 = if r == 16 { !0 } else { (1u16 << r) - 1 };
        let c = _mm512_cvtepi32_ps(_mm512_cvtepu8_epi32(_mm_maskz_loadu_epi8(m, a.add(i) as *const i8)));
        s[0] = _mm512_fmadd_ps(c, _mm512_maskz_loadu_ps(m, w.add(i)), s[0]);
        i += r;
    }
    _mm512_reduce_add_ps(_mm512_add_ps(_mm512_add_ps(s[0], s[1]), _mm512_add_ps(s[2], s[3])))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl")]
unsafe fn dot_f32_4x4_avx512(a: [*const f32; 4], b: [*const f32; 4], n: usize) -> [[f32; 4]; 4] {
    let mut s = [[_mm512_setzero_ps(); 4]; 4];
    let mut i = 0;
    while i < n {
        let m: __mmask16 = if n - i >= 16 { !0 } else { (1u16 << (n - i)) - 1 };
        let bv = b.map(|p| _mm512_maskz_loadu_ps(m, p.add(i)));
        for (row, p) in s.iter_mut().zip(a) {
            let av = _mm512_maskz_loadu_ps(m, p.add(i));
            for (acc, bj) in row.iter_mut().zip(bv) {
                *acc = _mm512_fmadd_ps(av, bj, *acc);
            }
        }
        i += 16;
    }
    s.map(|row| row.map(|v| _mm512_reduce_add_ps(v)))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_f32_avx2(a: *const f32, b: *const f32, n: usize) -> f32 {
    let mut s = [_mm256_setzero_ps(); 4];
    let mut i = 0;
    while i + 32 <= n {
        for (j, acc) in s.iter_mut().enumerate() {
            *acc = _mm256_fmadd_ps(_mm256_loadu_ps(a.add(i + 8 * j)), _mm256_loadu_ps(b.add(i + 8 * j)), *acc);
        }
        i += 32;
    }
    while i + 8 <= n {
        s[0] = _mm256_fmadd_ps(_mm256_loadu_ps(a.add(i)), _mm256_loadu_ps(b.add(i)), s[0]);
        i += 8;
    }
    let mut t = 0f32;
    while i < n {
        t += *a.add(i) * *b.add(i);
        i += 1;
    }
    let v = _mm256_add_ps(_mm256_add_ps(s[0], s[1]), _mm256_add_ps(s[2], s[3]));
    let h = _mm_add_ps(_mm256_castps256_ps128(v), _mm256_extractf128_ps::<1>(v));
    let h = _mm_add_ps(h, _mm_movehl_ps(h, h));
    _mm_cvtss_f32(_mm_add_ss(h, _mm_shuffle_ps::<1>(h, h))) + t
}

/// The sum of the eight int32 lanes of `v`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn hsum_epi32_avx2(v: __m256i) -> i32 {
    let h = _mm_add_epi32(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v));
    let h = _mm_add_epi32(h, _mm_shuffle_epi32::<0b01_00_11_10>(h));
    _mm_cvtsi128_si32(_mm_add_epi32(h, _mm_shuffle_epi32::<0b10_11_00_01>(h)))
}

/// 16 codes and 16 weights widened to 16 bits and multiplied pairwise into
/// eight int32 sums (`vpmaddwd`; a product of a code and a weight fits in
/// 16 bits, so nothing saturates).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn madd16_avx2(c: *const u8, q: __m256i) -> __m256i {
    _mm256_madd_epi16(_mm256_cvtepu8_epi16(_mm_loadu_si128(c as *const __m128i)), q)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_u8_i8_avx2(a: *const u8, b: *const i8, n: usize) -> i32 {
    let mut s = [_mm256_setzero_si256(); 4];
    let mut i = 0;
    while i + 64 <= n {
        for (j, acc) in s.iter_mut().enumerate() {
            let q = _mm256_cvtepi8_epi16(_mm_loadu_si128(b.add(i + 16 * j) as *const __m128i));
            *acc = _mm256_add_epi32(*acc, madd16_avx2(a.add(i + 16 * j), q));
        }
        i += 64;
    }
    while i + 16 <= n {
        let q = _mm256_cvtepi8_epi16(_mm_loadu_si128(b.add(i) as *const __m128i));
        s[0] = _mm256_add_epi32(s[0], madd16_avx2(a.add(i), q));
        i += 16;
    }
    let mut t = hsum_epi32_avx2(_mm256_add_epi32(_mm256_add_epi32(s[0], s[1]), _mm256_add_epi32(s[2], s[3])));
    while i < n {
        t += *a.add(i) as i32 * *b.add(i) as i32;
        i += 1;
    }
    t
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_u8_i8_x4_avx2(rows: [*const u8; 4], q: *const i8, n: usize) -> [i32; 4] {
    let mut s = [_mm256_setzero_si256(); 4];
    let mut i = 0;
    while i + 16 <= n {
        let qv = _mm256_cvtepi8_epi16(_mm_loadu_si128(q.add(i) as *const __m128i));
        for (acc, r) in s.iter_mut().zip(rows) {
            *acc = _mm256_add_epi32(*acc, madd16_avx2(r.add(i), qv));
        }
        i += 16;
    }
    let mut t = s.map(|v| hsum_epi32_avx2(v));
    while i < n {
        let w = *q.add(i) as i32;
        for (v, r) in t.iter_mut().zip(rows) {
            *v += *r.add(i) as i32 * w;
        }
        i += 1;
    }
    t
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn dot_f32_neon(a: *const f32, b: *const f32, n: usize) -> f32 {
    // Eight chains: the loop is load-bound only once FMA latency is hidden.
    let mut s = [vdupq_n_f32(0.0); 8];
    let mut i = 0;
    while i + 32 <= n {
        for (j, acc) in s.iter_mut().enumerate() {
            *acc = vfmaq_f32(*acc, vld1q_f32(a.add(i + 4 * j)), vld1q_f32(b.add(i + 4 * j)));
        }
        i += 32;
    }
    while i + 4 <= n {
        s[0] = vfmaq_f32(s[0], vld1q_f32(a.add(i)), vld1q_f32(b.add(i)));
        i += 4;
    }
    let mut t = 0f32;
    while i < n {
        t += *a.add(i) * *b.add(i);
        i += 1;
    }
    let h = |k: usize| vaddq_f32(vaddq_f32(s[k], s[k + 1]), vaddq_f32(s[k + 2], s[k + 3]));
    vaddvq_f32(vaddq_f32(h(0), h(4))) + t
}

/// `acc` plus, per 32-bit lane, the dot product of four unsigned codes with
/// four signed weights: `usdot` with i8mm. With dotprod only, `sdot` of the
/// codes biased to signed (`c - 128`), which falls short by `128 Σq`: the
/// kernels sum `q` with [`qsum4`] and add that back, exactly (the identity
/// holds modulo 2^32, as int32 sums wrap). The intrinsics are not stable
/// yet, hence `asm!`.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "dotprod")]
#[inline]
unsafe fn dot4(mut acc: int32x4_t, c: uint8x16_t, q: int8x16_t) -> int32x4_t {
    #[cfg(target_feature = "i8mm")]
    std::arch::asm!("usdot {0:v}.4s, {1:v}.16b, {2:v}.16b", inout(vreg) acc, in(vreg) c, in(vreg) q, options(pure, nomem, nostack, preserves_flags));
    #[cfg(not(target_feature = "i8mm"))]
    {
        let c = vreinterpretq_s8_u8(veorq_u8(c, vdupq_n_u8(0x80)));
        std::arch::asm!("sdot {0:v}.4s, {1:v}.16b, {2:v}.16b", inout(vreg) acc, in(vreg) c, in(vreg) q, options(pure, nomem, nostack, preserves_flags));
    }
    acc
}

/// `acc + Σq` per 32-bit lane when [`dot4`] needs the correction; `acc`
/// (optimised away) with i8mm.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "dotprod")]
#[inline]
unsafe fn qsum4(acc: int32x4_t, q: int8x16_t) -> int32x4_t {
    #[cfg(target_feature = "i8mm")]
    {
        let _ = q;
        acc
    }
    #[cfg(not(target_feature = "i8mm"))]
    {
        let mut acc = acc;
        std::arch::asm!("sdot {0:v}.4s, {1:v}.16b, {2:v}.16b", inout(vreg) acc, in(vreg) vdupq_n_s8(1), in(vreg) q, options(pure, nomem, nostack, preserves_flags));
        acc
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "dotprod")]
unsafe fn dot_u8_i8_neon(a: *const u8, b: *const i8, n: usize) -> i32 {
    // Four chains, and four for the correction (one would be latency-bound).
    let mut s = [vdupq_n_s32(0); 4];
    let mut qs = [vdupq_n_s32(0); 4];
    let mut i = 0;
    while i + 64 <= n {
        for (j, (acc, qa)) in s.iter_mut().zip(&mut qs).enumerate() {
            let qv = vld1q_s8(b.add(i + 16 * j));
            *qa = qsum4(*qa, qv);
            *acc = dot4(*acc, vld1q_u8(a.add(i + 16 * j)), qv);
        }
        i += 64;
    }
    while i + 16 <= n {
        let qv = vld1q_s8(b.add(i));
        qs[0] = qsum4(qs[0], qv);
        s[0] = dot4(s[0], vld1q_u8(a.add(i)), qv);
        i += 16;
    }
    let sum = |v: [int32x4_t; 4]| vaddvq_s32(vaddq_s32(vaddq_s32(v[0], v[1]), vaddq_s32(v[2], v[3])));
    let mut t = sum(s).wrapping_add(sum(qs).wrapping_mul(128));
    while i < n {
        t = t.wrapping_add(*a.add(i) as i32 * *b.add(i) as i32);
        i += 1;
    }
    t
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "dotprod")]
unsafe fn dot_u8_i8_x4_neon(rows: [*const u8; 4], q: *const i8, n: usize) -> [i32; 4] {
    // Two chains per row (and for the correction): 32 bytes per step.
    let mut s = [[vdupq_n_s32(0); 4]; 2];
    let mut qs = [vdupq_n_s32(0); 2];
    let mut i = 0;
    while i + 32 <= n {
        for (h, (acc, qa)) in s.iter_mut().zip(&mut qs).enumerate() {
            let qv = vld1q_s8(q.add(i + 16 * h));
            *qa = qsum4(*qa, qv);
            for (a, r) in acc.iter_mut().zip(rows) {
                *a = dot4(*a, vld1q_u8(r.add(i + 16 * h)), qv);
            }
        }
        i += 32;
    }
    if i + 16 <= n {
        let qv = vld1q_s8(q.add(i));
        qs[0] = qsum4(qs[0], qv);
        for (a, r) in s[0].iter_mut().zip(rows) {
            *a = dot4(*a, vld1q_u8(r.add(i)), qv);
        }
        i += 16;
    }
    let fix = vaddvq_s32(vaddq_s32(qs[0], qs[1])).wrapping_mul(128);
    let mut t = [0; 4];
    for (k, v) in t.iter_mut().enumerate() {
        *v = vaddvq_s32(vaddq_s32(s[0][k], s[1][k])).wrapping_add(fix);
    }
    while i < n {
        let w = *q.add(i) as i32;
        for (v, r) in t.iter_mut().zip(rows) {
            *v = v.wrapping_add(*r.add(i) as i32 * w);
        }
        i += 1;
    }
    t
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn dot_u8_f32_neon(a: *const u8, w: *const f32, n: usize) -> f32 {
    let mut s = [vdupq_n_f32(0.0); 4];
    let mut i = 0;
    while i + 16 <= n {
        // Widen 16 codes to four vectors of float32.
        let c = vld1q_u8(a.add(i));
        let (lo, hi) = (vmovl_u8(vget_low_u8(c)), vmovl_high_u8(c));
        let f = [vmovl_u16(vget_low_u16(lo)), vmovl_high_u16(lo), vmovl_u16(vget_low_u16(hi)), vmovl_high_u16(hi)];
        for (j, acc) in s.iter_mut().enumerate() {
            *acc = vfmaq_f32(*acc, vcvtq_f32_u32(f[j]), vld1q_f32(w.add(i + 4 * j)));
        }
        i += 16;
    }
    let mut t = 0f32;
    while i < n {
        t += *a.add(i) as f32 * *w.add(i);
        i += 1;
    }
    vaddvq_f32(vaddq_f32(vaddq_f32(s[0], s[1]), vaddq_f32(s[2], s[3]))) + t
}

#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn dot_f32_4x4_neon(a: [*const f32; 4], b: [*const f32; 4], n: usize) -> [[f32; 4]; 4] {
    let mut s = [[vdupq_n_f32(0.0); 4]; 4];
    let mut i = 0;
    while i + 4 <= n {
        let bv = b.map(|p| vld1q_f32(p.add(i)));
        for (row, p) in s.iter_mut().zip(a) {
            let av = vld1q_f32(p.add(i));
            for (acc, bj) in row.iter_mut().zip(bv) {
                *acc = vfmaq_f32(*acc, av, bj);
            }
        }
        i += 4;
    }
    let mut t = s.map(|row| row.map(|v| vaddvq_f32(v)));
    while i < n {
        for (row, p) in t.iter_mut().zip(a) {
            for (x, q) in row.iter_mut().zip(b) {
                *x += *p.add(i) * *q.add(i);
            }
        }
        i += 1;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    fn close(a: f32, b: f32, scale: f32) -> bool {
        (a - b).abs() <= 1e-4 * scale.max(1.0)
    }

    #[test]
    fn kernels_match_scalar_at_every_length() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        for n in (0..300).chain([511, 512, 513, 1023, 1024, 1025, 1536]) {
            let a: Vec<f32> = (0..n).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let b: Vec<f32> = (0..n).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let mag: f32 = a.iter().zip(&b).map(|(x, y)| (x * y).abs()).sum();
            assert!(close(dot_f32(&a, &b), dot_f32_scalar(&a, &b), mag), "dot_f32 n={n}");

            let c: Vec<u8> = (0..n).map(|_| rng.gen()).collect();
            let q: Vec<i8> = (0..n).map(|_| rng.gen_range(-127..=127)).collect();
            assert_eq!(dot_u8_i8(&c, &q), dot_u8_i8_scalar(&c, &q), "dot_u8_i8 n={n}");
            // Extremes: the int32 sum must not saturate.
            let hi = vec![255u8; n];
            let lo = vec![-128i8; n];
            assert_eq!(dot_u8_i8(&hi, &lo), -(255 * 128 * n as i32));

            let rows: Vec<Vec<u8>> = (0..4).map(|_| (0..n).map(|_| rng.gen()).collect()).collect();
            let x4 = dot_u8_i8_x4([&rows[0], &rows[1], &rows[2], &rows[3]], &q);
            for r in 0..4 {
                assert_eq!(x4[r], dot_u8_i8_scalar(&rows[r], &q), "x4 n={n} row {r}");
            }

            let wmag: f32 = c.iter().zip(&a).map(|(&x, y)| (x as f32 * y).abs()).sum();
            assert!(close(dot_u8_f32(&c, &a), dot_u8_f32_scalar(&c, &a), wmag), "dot_u8_f32 n={n}");

            let vs: Vec<Vec<f32>> = (0..8).map(|_| (0..n).map(|_| rng.gen_range(-1.0..1.0)).collect()).collect();
            let m = dot_f32_4x4([&vs[0], &vs[1], &vs[2], &vs[3]], [&vs[4], &vs[5], &vs[6], &vs[7]]);
            for i in 0..4 {
                for j in 0..4 {
                    assert!(close(m[i][j], dot_f32_scalar(&vs[i], &vs[4 + j]), n as f32), "4x4 n={n}");
                }
            }
        }
    }

    // Each x86_64 tier this CPU has against the scalar kernels, whichever
    // tier the dispatch picks.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn every_x86_tier_matches_scalar() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(2);
        for n in (0..200).chain([255, 256, 257, 1023, 1024, 1025]) {
            let a: Vec<f32> = (0..n).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let b: Vec<f32> = (0..n).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let mag: f32 = a.iter().zip(&b).map(|(x, y)| (x * y).abs()).sum();
            let c: Vec<u8> = (0..n).map(|_| rng.gen()).collect();
            let rows: Vec<Vec<u8>> = (0..4).map(|_| (0..n).map(|_| rng.gen()).collect()).collect();
            let rows4 = [rows[0].as_ptr(), rows[1].as_ptr(), rows[2].as_ptr(), rows[3].as_ptr()];
            for q in [(0..n).map(|_| rng.gen_range(-128..=127)).collect::<Vec<i8>>(), vec![-128; n], vec![127; n]] {
                let want = dot_u8_i8_scalar(&c, &q);
                let want4 = [0, 1, 2, 3].map(|r| dot_u8_i8_scalar(&rows[r], &q));
                let hi = vec![255u8; n];
                if simd::avx2() {
                    // SAFETY: the CPU has AVX2 and FMA; every slice holds `n` elements.
                    unsafe {
                        assert!(close(dot_f32_avx2(a.as_ptr(), b.as_ptr(), n), dot_f32_scalar(&a, &b), mag), "avx2 dot_f32 n={n}");
                        assert_eq!(dot_u8_i8_avx2(c.as_ptr(), q.as_ptr(), n), want, "avx2 dot_u8_i8 n={n}");
                        assert_eq!(dot_u8_i8_avx2(hi.as_ptr(), q.as_ptr(), n), dot_u8_i8_scalar(&hi, &q), "avx2 extremes n={n}");
                        assert_eq!(dot_u8_i8_x4_avx2(rows4, q.as_ptr(), n), want4, "avx2 x4 n={n}");
                    }
                }
                if simd::avx512vnni() {
                    // SAFETY: the CPU has AVX-512 VNNI; every slice holds `n` elements.
                    unsafe {
                        assert_eq!(dot_u8_i8_vnni(c.as_ptr(), q.as_ptr(), n), want, "vnni n={n}");
                        assert_eq!(dot_u8_i8_x4_vnni(rows4, q.as_ptr(), n), want4, "vnni x4 n={n}");
                    }
                }
            }
            if simd::avx512() {
                // SAFETY: the CPU has AVX-512; every slice holds `n` elements.
                unsafe { assert!(close(dot_f32_avx512(a.as_ptr(), b.as_ptr(), n), dot_f32_scalar(&a, &b), mag), "avx512 dot_f32 n={n}") };
            }
        }
        eprintln!("SIMD tier: {}, SQ8 kernel: {}", simd::describe(), sq8_kernel());
    }

    #[test]
    fn binary_kernels_match_scalar() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        for words in [1, 2, 3, 6, 12, 16, 17] {
            let u: Vec<u8> = (0..64 * words).map(|_| rng.gen()).collect();
            let mut planes = vec![0u64; 8 * words];
            for (j, &x) in u.iter().enumerate() {
                for p in 0..8 {
                    planes[j / 64 * 8 + p] |= ((x >> p & 1) as u64) << (j % 64);
                }
            }
            let v: Vec<i8> = u.iter().map(|&x| (x as i32 - 128) as i8).collect();
            let q = BitsWeights { u: &u, v: &v, planes: &planes };
            let codes: Vec<Vec<u8>> = vec![vec![0u8; 8 * words], vec![255u8; 8 * words], (0..8 * words).map(|_| rng.gen()).collect(), (0..8 * words).map(|_| rng.gen()).collect()];
            for code in &codes {
                let want = dot_bits_u8_scalar(code, &u);
                assert_eq!(dot_bits_u8(code, q), want, "{words} words");
                assert_eq!(dot_bits_planes(code, &planes), want, "{words} words");
            }
            let four = dot_bits_u8_x4([&codes[0], &codes[1], &codes[2], &codes[3]], q);
            assert_eq!(four, [0, 1, 2, 3].map(|i| dot_bits_u8_scalar(&codes[i], &u)), "{words} words, four rows");
        }
    }

    #[test]
    fn kernels_match_scalar_on_edge_cases() {
        let lens = [0, 1, 3, 4, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 1023, 1024, 1025];
        for n in lens {
            // Codes: zeros, all 255, alternating extremes, a ramp.
            let codes: [Vec<u8>; 4] = [vec![0; n], vec![255; n], (0..n).map(|i| if i % 2 == 0 { 255 } else { 0 }).collect(), (0..n).map(|i| i as u8).collect()];
            // Weights: zeros, both extremes (including -128, which `prepare`
            // never makes), alternating extremes, a signed ramp.
            let qs: [Vec<i8>; 5] = [
                vec![0; n],
                vec![-128; n],
                vec![127; n],
                (0..n).map(|i| if i % 3 == 0 { -128 } else { 127 }).collect(),
                (0..n).map(|i| (i as i32 % 256 - 128) as i8).collect(),
            ];
            for q in &qs {
                for c in &codes {
                    assert_eq!(dot_u8_i8(c, q), dot_u8_i8_scalar(c, q), "dot_u8_i8 n={n}");
                }
                let x4 = dot_u8_i8_x4([&codes[0], &codes[1], &codes[2], &codes[3]], q);
                assert_eq!(x4, codes.each_ref().map(|c| dot_u8_i8_scalar(c, q)), "x4 n={n}");
                // A non-zero element at every position lands in its own lane.
                for p in 0..n.min(70) {
                    let mut one = vec![0u8; n];
                    one[p] = 255;
                    assert_eq!(dot_u8_i8(&one, q), 255 * q[p] as i32, "dot_u8_i8 n={n} p={p}");
                    let x4 = dot_u8_i8_x4([&one, &codes[0], &one, &codes[1]], q);
                    assert_eq!(x4, [&one, &codes[0], &one, &codes[1]].map(|c| dot_u8_i8_scalar(c, q)), "x4 n={n} p={p}");
                }
            }

            // Float32: zeros are exact; mixed magnitudes and signs agree up
            // to summation order.
            let z = vec![0f32; n];
            let f: Vec<f32> = (0..n).map(|i| [1e3, -1e-3, 7.5, -2e2, 0.0][i % 5]).collect();
            let g: Vec<f32> = (0..n).map(|i| [-3.0, 1e2, 0.25, 1e-2][i % 4]).collect();
            assert_eq!(dot_f32(&z, &f), 0.0);
            let mag: f32 = f.iter().zip(&g).map(|(x, y)| (x * y).abs()).sum();
            assert!(close(dot_f32(&f, &g), dot_f32_scalar(&f, &g), mag), "dot_f32 n={n}");
            for c in &codes {
                let wmag: f32 = c.iter().zip(&g).map(|(&x, y)| (x as f32 * y).abs()).sum();
                assert!(close(dot_u8_f32(c, &g), dot_u8_f32_scalar(c, &g), wmag), "dot_u8_f32 n={n}");
                assert_eq!(dot_u8_f32(c, &z), 0.0);
            }
            let m = dot_f32_4x4([&f, &g, &z, &f], [&g, &z, &f, &f]);
            for (i, a) in [&f, &g, &z, &f].into_iter().enumerate() {
                for (j, b) in [&g, &z, &f, &f].into_iter().enumerate() {
                    let mag: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x * y).abs()).sum();
                    assert!(close(m[i][j], dot_f32_scalar(a, b), mag), "4x4 n={n} ({i}, {j})");
                }
            }
        }
    }
}
