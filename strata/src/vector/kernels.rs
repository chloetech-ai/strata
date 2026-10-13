//! Distance kernels.
//!
//! * [`dot_f32`]: exact float32 dot product (AVX-512 FMA, four accumulators).
//! * [`dot_u8_i8`], [`dot_u8_i8_x4`]: SQ8 codes against an int8-quantised
//!   query, summed in int32 with AVX-512 VNNI (`vpdpbusd`: 64 byte products
//!   per instruction). The `x4` form scores four rows per query load, which
//!   is what scans use.
//! * [`dot_u8_f32`]: SQ8 codes against a float32 query (asymmetric: widen the
//!   codes, then FMA). Kept for the kernel comparison in the bench; scans use
//!   the VNNI kernel (see `notes/vector.md`).
//! * [`dot_f32_4x4`]: a 4×4 block of float32 dot products sharing loads, for
//!   k-means and brute-force ground truth.
//!
//! The SIMD paths are chosen at compile time (`-C target-cpu=native`); every
//! kernel has a portable scalar version, used on other targets and as the
//! reference in tests. Lengths need not be multiples of the vector width:
//! tails use masked loads.

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
use std::arch::x86_64::*;

/// Exact dot product of two float32 vectors of equal length.
#[inline]
pub fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    // SAFETY: both slices hold `a.len()` elements; tails use masked loads.
    unsafe {
        dot_f32_avx512(a.as_ptr(), b.as_ptr(), a.len())
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
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
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512vnni", target_feature = "avx512bw"))]
    // SAFETY: both slices hold `a.len()` bytes; tails use masked loads.
    unsafe {
        dot_u8_i8_vnni(a.as_ptr(), b.as_ptr(), a.len())
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512vnni", target_feature = "avx512bw")))]
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
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512vnni", target_feature = "avx512bw"))]
    // SAFETY: every row holds `q.len()` bytes; tails use masked loads.
    unsafe {
        dot_u8_i8_x4_vnni([rows[0].as_ptr(), rows[1].as_ptr(), rows[2].as_ptr(), rows[3].as_ptr()], q.as_ptr(), q.len())
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512vnni", target_feature = "avx512bw")))]
    rows.map(|r| dot_u8_i8_scalar(r, q))
}

/// Dot product of unsigned 8-bit codes with float32 weights.
#[inline]
pub fn dot_u8_f32(a: &[u8], w: &[f32]) -> f32 {
    assert_eq!(a.len(), w.len());
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    // SAFETY: `a` holds `w.len()` bytes; tails use masked loads.
    unsafe {
        dot_u8_f32_avx512(a.as_ptr(), w.as_ptr(), a.len())
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
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

/// All 16 dot products `a[i] · b[j]` of four vectors against four others
/// (equal lengths). Each loaded chunk feeds four FMAs, so this runs near the
/// FMA throughput instead of the load throughput.
#[inline]
pub fn dot_f32_4x4(a: [&[f32]; 4], b: [&[f32]; 4]) -> [[f32; 4]; 4] {
    let n = a[0].len();
    for v in a.iter().chain(&b) {
        assert_eq!(v.len(), n);
    }
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    // SAFETY: all eight vectors hold `n` elements; tails use masked loads.
    unsafe {
        dot_f32_4x4_avx512(a.map(|v| v.as_ptr()), b.map(|v| v.as_ptr()), n)
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f")))]
    a.map(|x| b.map(|y| dot_f32_scalar(x, y)))
}

/// Hints the CPU to fetch `bytes` starting at `p` into cache (one hint per
/// 64-byte line). Used ahead of random accesses (graph neighbours, sparse
/// filter members).
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
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (p, bytes);
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline]
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

#[cfg(all(target_arch = "x86_64", target_feature = "avx512vnni", target_feature = "avx512bw"))]
#[inline]
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

#[cfg(all(target_arch = "x86_64", target_feature = "avx512vnni", target_feature = "avx512bw"))]
#[inline]
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

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline]
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

#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline]
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
}
