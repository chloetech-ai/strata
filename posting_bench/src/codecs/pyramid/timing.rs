//! Timing experiments (ignored tests): bitmap extraction variants (x86),
//! NEON kernels against their scalar twins (aarch64) and a workload
//! profiler. Not part of the codec.
#![allow(dead_code)]

use super::simd::*;
use rand::{rngs::StdRng, Rng, SeedableRng};
#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::hint::black_box;
use std::time::Instant;

#[cfg(target_arch = "x86_64")]
unsafe fn ext_words_compress(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let iota = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let mut k = 0;
    for (i, &x) in w.iter().enumerate() {
        if x == 0 {
            continue;
        }
        for j in 0..4 {
            let m = (x >> (16 * j)) as u16;
            let vals = _mm512_add_epi32(iota, _mm512_set1_epi32((base + 64 * i as u32 + 16 * j) as i32));
            _mm512_storeu_si512(dst.add(k) as *mut _, _mm512_maskz_compress_epi32(m, vals));
            k += m.count_ones() as usize;
        }
    }
    k
}

#[cfg(target_arch = "x86_64")]
unsafe fn ext_all16(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let iota = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let pieces = w.as_ptr() as *const u16;
    let mut k = 0;
    let mut vb = _mm512_add_epi32(iota, _mm512_set1_epi32(base as i32));
    let step = _mm512_set1_epi32(16);
    for i in 0..16 {
        let m = *pieces.add(i);
        _mm512_storeu_si512(dst.add(k) as *mut _, _mm512_maskz_compress_epi32(m, vb));
        k += m.count_ones() as usize;
        vb = _mm512_add_epi32(vb, step);
    }
    k
}

#[cfg(target_arch = "x86_64")]
static LUT: [[u8; 8]; 256] = {
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

#[cfg(target_arch = "x86_64")]
unsafe fn ext_lut(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let bytes = w.as_ptr() as *const u8;
    let mut k = 0;
    let mut vb = _mm256_set1_epi32(base as i32);
    let step = _mm256_set1_epi32(8);
    for i in 0..32 {
        let m = *bytes.add(i);
        let pos = _mm256_cvtepu8_epi32(_mm_loadl_epi64(LUT.as_ptr().add(m as usize) as *const __m128i));
        _mm256_storeu_si256(dst.add(k) as *mut __m256i, _mm256_add_epi32(pos, vb));
        k += m.count_ones() as usize;
        vb = _mm256_add_epi32(vb, step);
    }
    k
}

#[cfg(target_arch = "x86_64")]
static LUT32: [[u32; 8]; 256] = {
    let mut t = [[0u32; 8]; 256];
    let mut m = 0;
    while m < 256 {
        let (mut b, mut k) = (0, 0);
        while b < 8 {
            if m & (1 << b) != 0 {
                t[m][k] = b as u32;
                k += 1;
            }
            b += 1;
        }
        m += 1;
    }
    t
};

#[cfg(target_arch = "x86_64")]
unsafe fn ext_lut32(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let mut k = 0;
    let mut vb = _mm256_set1_epi32(base as i32);
    let step = _mm256_set1_epi32(8);
    for &x in w {
        let mut x = x;
        for _ in 0..8 {
            let m = (x & 255) as usize;
            let pos = _mm256_loadu_si256(LUT32.as_ptr().add(m) as *const __m256i);
            _mm256_storeu_si256(dst.add(k) as *mut __m256i, _mm256_add_epi32(pos, vb));
            k += (m as u32).count_ones() as usize;
            vb = _mm256_add_epi32(vb, step);
            x >>= 8;
        }
    }
    k
}

#[cfg(target_arch = "x86_64")]
unsafe fn ext_lut32_skip(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let mut k = 0;
    for (i, &x) in w.iter().enumerate() {
        if x == 0 {
            continue;
        }
        let mut x = x;
        let mut vb = _mm256_set1_epi32((base + 64 * i as u32) as i32);
        let step = _mm256_set1_epi32(8);
        for _ in 0..8 {
            let m = (x & 255) as usize;
            let pos = _mm256_loadu_si256(LUT32.as_ptr().add(m) as *const __m256i);
            _mm256_storeu_si256(dst.add(k) as *mut __m256i, _mm256_add_epi32(pos, vb));
            k += (m as u32).count_ones() as usize;
            vb = _mm256_add_epi32(vb, step);
            x >>= 8;
        }
    }
    k
}

#[cfg(target_arch = "x86_64")]
unsafe fn ext_kload(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let v = _mm256_loadu_si256(w.as_ptr() as *const __m256i);
    let mut nz = _mm256_test_epi16_mask(v, v) as u32;
    let iota = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let pieces = w.as_ptr() as *const u16;
    let mut k = 0usize;
    while nz != 0 {
        let i = nz.trailing_zeros();
        nz &= nz - 1;
        let m = _load_mask16(pieces.add(i as usize));
        let c = (*pieces.add(i as usize)).count_ones() as usize;
        let vals = _mm512_add_epi32(iota, _mm512_set1_epi32((base + 16 * i) as i32));
        _mm512_storeu_si512(dst.add(k) as *mut _, _mm512_maskz_compress_epi32(m, vals));
        k += c;
    }
    k
}

#[cfg(target_arch = "x86_64")]
unsafe fn ext_kload_all(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let iota = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let pieces = w.as_ptr() as *const u16;
    let mut k = 0;
    let mut vb = _mm512_add_epi32(iota, _mm512_set1_epi32(base as i32));
    let step = _mm512_set1_epi32(16);
    for i in 0..16 {
        let m = _load_mask16(pieces.add(i));
        let c = (*pieces.add(i)).count_ones() as usize;
        _mm512_storeu_si512(dst.add(k) as *mut _, _mm512_maskz_compress_epi32(m, vb));
        k += c;
        vb = _mm512_add_epi32(vb, step);
    }
    k
}

#[cfg(target_arch = "x86_64")]
unsafe fn ext_hybrid(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let c: u32 = w.iter().map(|x| x.count_ones()).sum();
    if c <= 8 {
        let mut k = 0;
        for (i, &x) in w.iter().enumerate() {
            let mut x = x;
            let b = base + 64 * i as u32;
            while x != 0 {
                *dst.add(k) = b + x.trailing_zeros();
                k += 1;
                x &= x - 1;
            }
        }
        k
    } else {
        extract256(w, base, dst)
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn ext_vbmi2(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    #[cfg(target_feature = "avx512vbmi2")]
    {
        let start = dst;
        crate::codecs::roaring::simd::extract_avx512_u32(0..4, |i| w[i], base, dst).offset_from(start) as usize
    }
    #[cfg(not(target_feature = "avx512vbmi2"))]
    extract256(w, base, dst)
}

#[cfg(target_arch = "x86_64")]
#[test]
#[ignore = "timing only"]
fn extract_timing() {
    let mut rng = StdRng::seed_from_u64(1);
    let mut out = vec![0u32; 1 << 22];
    for (label, dens) in [("1 bit", -1.0), ("3 bits", -3.0), ("p=0.05", 0.05), ("p=0.2", 0.2), ("p=0.5", 0.5), ("mix", 0.0)] {
        let blocks: Vec<[u64; 4]> = (0..8192)
            .map(|_| {
                let d: f64 = if dens == 0.0 {
                    if rng.gen_bool(0.6) {
                        -1.0
                    } else {
                        rng.gen_range(0.1..0.6)
                    }
                } else {
                    dens
                };
                if d < 0.0 {
                    let mut w = [0u64; 4];
                    for _ in 0..(-d) as usize {
                        let b = rng.gen_range(0..256);
                        w[b >> 6] |= 1 << (b & 63);
                    }
                    w
                } else {
                    std::array::from_fn(|_| (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(d) as u64) << b)))
                }
            })
            .collect();
        let total: usize = blocks.iter().map(|w| w.iter().map(|x| x.count_ones() as usize).sum::<usize>()).sum();
        let mut row = format!("{label:>8} ({:5.1} bits/block):", total as f64 / blocks.len() as f64);
        type F = unsafe fn(&[u64; 4], u32, *mut u32) -> usize;
        let fs: [(&str, F); 12] = [
            ("pieces", extract256_compressd),
            ("dispatch", extract256),
            ("kload", ext_kload),
            ("kload_all", ext_kload_all),
            ("words", ext_words_compress),
            ("all16", ext_all16),
            ("lut", ext_lut),
            ("lut32", ext_lut32),
            ("lut32skip", ext_lut32_skip),
            ("scalar", extract256_scalar),
            ("hybrid", ext_hybrid),
            ("vbmi2", ext_vbmi2),
        ];
        for (name, f) in fs {
            let mut best = f64::MAX;
            for _ in 0..7 {
                let t = Instant::now();
                let mut k = 0;
                for (i, w) in blocks.iter().enumerate() {
                    k += unsafe { f(w, (i as u32) << 8, out.as_mut_ptr().add(k)) };
                }
                black_box(&out);
                assert_eq!(k, total);
                best = best.min(t.elapsed().as_secs_f64());
            }
            row += &format!(" {name} {:.2}", best * 1e9 / total as f64);
        }
        println!("{row}  (ns/value)");
    }
}

/// [`extract256_neon`] with `T` unconditional writes for words of at most
/// `T` bits (the kernel uses 4; 0: the byte table for every word).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn ext_neon_hybrid<const T: u32>(w: &[u64; 4], base: u32, dst: *mut u32) -> usize {
    let step = vdupq_n_u32(8);
    let mut k = 0usize;
    for (i, &x) in w.iter().enumerate() {
        let b = base + 64 * i as u32;
        let s = vget_lane_u64::<0>(vreinterpret_u64_u8(vcnt_u8(vcreate_u8(x)))).wrapping_mul(0x0101_0101_0101_0101);
        let o = dst.add(k);
        if (s >> 56) as u32 <= T {
            let mut x = x;
            for j in 0..T as usize {
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
        k += (s >> 56) as usize;
    }
    k
}

/// [`u16s_contain_neon`] with a loop over 8 values at a time (instead of
/// eight clamped loads).
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn u16s_contain_loop(p: *const u16, n: usize, x: u16) -> bool {
    if n >= 8 {
        let k = vdupq_n_u16(x);
        let mut hit = vceqq_u16(vld1q_u16(p.add(n - 8)), k);
        let mut i = 0;
        while i + 8 < n {
            hit = vorrq_u16(hit, vceqq_u16(vld1q_u16(p.add(i)), k));
            i += 8;
        }
        vmaxvq_u16(hit) != 0
    } else {
        u16s_contain_neon(p, n, x)
    }
}

/// NEON [`bytes_count_lt`] (not used: faster per call, but its callers,
/// cursors, are latency-bound, where the scalar binary search wins): a
/// byte compare of the zero-padded array (two loads, the second one
/// shifted into place), restricted to the valid lanes, summed across lanes.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn bytes_count_lt_neon(p: *const u8, n: usize, x: u8) -> usize {
    static IOTA: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let iota = vld1q_u8(IOTA.as_ptr());
    let k = vdupq_n_u8(x);
    if n > 16 {
        // Lanes `j >= 32 - n` of the last 16 bytes are the bytes past 16.
        let hi = vandq_u8(vcltq_u8(vld1q_u8(p.add(n - 16)), k), vcgeq_u8(iota, vdupq_n_u8((32 - n) as u8)));
        let lo = vcltq_u8(vld1q_u8(p), k);
        // Lanes are 0 or -1: negate the sum to count.
        vaddvq_u8(vsubq_u8(vdupq_n_u8(0), vaddq_u8(lo, hi))) as usize
    } else {
        let lt = vandq_u8(vcltq_u8(load_le16(p, n), k), vcltq_u8(iota, vdupq_n_u8(n as u8)));
        vaddvq_u8(vandq_u8(lt, vdupq_n_u8(1))) as usize
    }
}

/// Picks `n` from `weights` (index = value).
#[cfg(target_arch = "aarch64")]
fn pick(rng: &mut StdRng, weights: &[u32]) -> usize {
    let tot: u32 = weights.iter().sum();
    let mut r = rng.gen_range(0..tot);
    for (i, &w) in weights.iter().enumerate() {
        if r < w {
            return i;
        }
        r -= w;
    }
    unreachable!()
}

#[cfg(target_arch = "aarch64")]
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

/// Best of 9 runs of `f`, in ns per call (`calls` per run).
#[cfg(target_arch = "aarch64")]
fn ns_per_call(calls: usize, mut f: impl FnMut() -> u64) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..9 {
        let t = Instant::now();
        black_box(f());
        best = best.min(t.elapsed().as_secs_f64());
    }
    best * 1e9 / calls as f64
}

/// NEON kernels against their scalar twins on inputs shaped like the
/// benchmark workloads' (sizes and densities as measured on the synthetic
/// collections), in ns per call.
///
/// `cargo test --release neon_timing -- --ignored --nocapture`
#[cfg(target_arch = "aarch64")]
#[test]
#[ignore = "timing only"]
fn neon_timing() {
    let mut rng = StdRng::seed_from_u64(1);
    const N: usize = 1 << 14;
    let report = |name: &str, ts: &[(&str, f64)]| {
        let s: Vec<String> = ts.iter().map(|(l, t)| format!("{l} {t:.2}")).collect();
        println!("{name:>24}: {}  (ns/call)", s.join(", "));
    };

    // Select in 64 bits of U (about one set bit in 4), k < SAMPLE.
    let sel: Vec<(u64, u32)> = (0..N)
        .map(|_| loop {
            let w = rng.gen::<u64>() & rng.gen::<u64>();
            let c = w.count_ones();
            if c >= 9 {
                break (w, rng.gen_range(0..8));
            }
        })
        .collect();
    let f = |g: fn(u64, u32) -> (u32, u32)| {
        ns_per_call(N, || sel.iter().map(|&(w, k)| {
            let (a, b) = g(black_box(w), k);
            (a + b) as u64
        }).sum())
    };
    report("select2", &[("scalar", f(select2_scalar)), ("neon", f(select2))]);
    // Latency: each word depends on the previous result (as in cursors).
    let f = |g: fn(u64, u32) -> (u32, u32)| {
        ns_per_call(N, || {
            let mut acc = 0u32;
            for &(w, k) in &sel {
                let (a, b) = g(w.rotate_left(acc & 1), k);
                acc = acc.wrapping_add(a + b);
            }
            acc as u64
        })
    };
    report("select2 (chained)", &[("scalar", f(select2_scalar)), ("neon", f(select2))]);

    // Membership in sparse blocks' low bytes (n as probed by intersections).
    let wn: [u32; 29] = [35, 15, 10, 8, 6, 5, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1];
    let mut buf = Vec::new();
    let mut probes = Vec::new();
    for _ in 0..N {
        let n = pick(&mut rng, &wn);
        let v = sorted_bytes(&mut rng, n);
        let x = if n > 0 && rng.gen_bool(0.5) { v[rng.gen_range(0..n)] } else { rng.gen() };
        probes.push((buf.len(), n, x));
        buf.extend_from_slice(&v);
    }
    let f = |g: unsafe fn(*const u8, usize, u8) -> bool| {
        ns_per_call(N, || probes.iter().map(|&(o, n, x)| unsafe { g(buf.as_ptr().add(o), n, black_box(x)) } as u64).sum())
    };
    report("bytes_contain", &[("scalar", f(bytes_contain_scalar)), ("neon", f(bytes_contain_neon))]);
    for &(o, n, x) in &probes {
        unsafe { assert_eq!(bytes_count_lt_neon(buf.as_ptr().add(o), n, x), bytes_count_lt_scalar(buf.as_ptr().add(o), n, x)) };
    }
    let f = |g: unsafe fn(*const u8, usize, u8) -> usize| {
        ns_per_call(N, || probes.iter().map(|&(o, n, x)| unsafe { g(buf.as_ptr().add(o), n, black_box(x)) } as u64).sum())
    };
    report("bytes_count_lt", &[("scalar", f(bytes_count_lt_scalar)), ("neon", f(bytes_count_lt_neon))]);
    let f = |g: unsafe fn(*const u8, usize, u8) -> usize| {
        ns_per_call(N, || {
            let mut acc = 0usize;
            for &(o, n, x) in &probes {
                acc += unsafe { g(buf.as_ptr().add(o), n, x ^ (acc & 1) as u8) };
            }
            acc as u64
        })
    };
    report("bytes_count_lt (chained)", &[("scalar", f(bytes_count_lt_scalar)), ("neon", f(bytes_count_lt_neon))]);
    let wl: [u32; 29] = [0, 31, 9, 4, 3, 4, 3, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2];
    let loads: Vec<(usize, usize)> = (0..N)
        .map(|_| {
            let n = pick(&mut rng, &wl);
            (rng.gen_range(0..buf.len() - n), n)
        })
        .collect();
    let f = |g: unsafe fn(*const u8, usize) -> [u8; 32]| {
        ns_per_call(N, || loads.iter().map(|&(o, n)| unsafe { black_box(g(buf.as_ptr().add(o), n)) }[1] as u64).sum())
    };
    report("load_bytes32", &[("scalar", f(load_bytes32_scalar)), ("neon", f(load_bytes32_neon))]);

    // Small chunks: up to 48 sorted values.
    let mut b16 = Vec::new();
    let mut p16 = Vec::new();
    for _ in 0..N {
        let n = rng.gen_range(1..=48);
        let mut v: Vec<u16> = (0..n).map(|_| rng.gen()).collect();
        v.sort_unstable();
        v.dedup();
        let x = if rng.gen_bool(0.5) { v[rng.gen_range(0..v.len())] } else { rng.gen() };
        p16.push((b16.len(), v.len(), x));
        b16.extend_from_slice(&v);
    }
    let f = |g: unsafe fn(*const u16, usize, u16) -> bool| {
        ns_per_call(N, || p16.iter().map(|&(o, n, x)| unsafe { g(b16.as_ptr().add(o), n, black_box(x)) } as u64).sum())
    };
    report("u16s_contain", &[("scalar", f(u16s_contain_scalar)), ("neon", f(u16s_contain_neon)), ("loop", f(u16s_contain_loop))]);

    // Block filters: lows against bitmaps, against other lows.
    let ds: Vec<[u8; 32]> = (0..N).map(|_| std::array::from_fn(|_| rng.gen())).collect();
    let bms: Vec<[u64; 4]> = (0..N).map(|_| std::array::from_fn(|_| rng.gen())).collect();
    let f = |g: unsafe fn(&[u8; 32], *const u64) -> u32| {
        ns_per_call(N, || ds.iter().zip(&bms).map(|(d, b)| unsafe { g(black_box(d), b.as_ptr()) } as u64).sum())
    };
    report("test_bits", &[("scalar", f(test_bits_scalar)), ("neon", f(test_bits_neon))]);
    let wd: [u32; 29] = [0, 48, 11, 3, 2, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1];
    let tb: Vec<([u8; 32], usize, usize, usize)> = (0..N)
        .map(|_| {
            let nd = pick(&mut rng, &wd);
            let mut d = [0u8; 32];
            d[..nd].copy_from_slice(&sorted_bytes(&mut rng, nd));
            let n = rng.gen_range(1..=16);
            let o = rng.gen_range(0..buf.len() - n);
            (d, nd, o, n)
        })
        .collect();
    let f = |g: unsafe fn(&[u8; 32], usize, *const u8, usize) -> u32| {
        ns_per_call(N, || tb.iter().map(|(d, nd, o, n)| unsafe { g(black_box(d), *nd, buf.as_ptr().add(*o), *n) } as u64).sum())
    };
    report("test_bytes", &[("scalar", f(test_bytes_scalar)), ("neon", f(test_bytes_neon))]);

    // Writing: lanes (few hits), widening, sparse expansion.
    let mut out = vec![0u32; 1 << 22];
    let wh: [u32; 13] = [0, 34, 16, 11, 9, 7, 5, 4, 3, 2, 2, 1, 1];
    let lanes: Vec<([u8; 32], u32)> = (0..N)
        .map(|_| {
            let c = pick(&mut rng, &wh);
            let mut h = 0u32;
            while h.count_ones() < c as u32 {
                h |= 1 << rng.gen_range(0..32);
            }
            (std::array::from_fn(|_| rng.gen()), h)
        })
        .collect();
    let f = |g: unsafe fn(&[u8; 32], u32, u32, *mut u32) -> usize, out: &mut Vec<u32>| {
        ns_per_call(N, || {
            let mut k = 0;
            for (i, (d, h)) in lanes.iter().enumerate() {
                k += unsafe { g(d, *h, (i as u32) << 8, out.as_mut_ptr().add(k)) };
            }
            k as u64
        })
    };
    report("emit_lanes", &[("scalar", f(emit_lanes_scalar, &mut out)), ("neon", f(emit_lanes_neon, &mut out))]);
    let ww: [u32; 29] = [0, 18, 0, 0, 0, 0, 0, 2, 2, 2, 2, 3, 3, 3, 3, 5, 4, 3, 5, 4, 3, 5, 4, 4, 4, 3, 5, 5, 0];
    let wides: Vec<(usize, usize)> = (0..N)
        .map(|_| {
            let n = pick(&mut rng, &ww);
            (rng.gen_range(0..buf.len() - n), n)
        })
        .collect();
    let f = |g: unsafe fn(*const u8, usize, u32, *mut u32), out: &mut Vec<u32>| {
        ns_per_call(N, || {
            let mut k = 0;
            for &(o, n) in &wides {
                unsafe { g(buf.as_ptr().add(o), n, 7 << 16, out.as_mut_ptr().add(k)) };
                k += n;
            }
            k as u64
        })
    };
    report("widen_bytes", &[("scalar", f(widen_bytes_scalar, &mut out)), ("neon", f(widen_bytes_neon, &mut out))]);

    // Sparse runs: blocks of 1..=28 values (singletons more likely), as
    // decoded by unions (whole chunks) and by short runs.
    for (label, nb_max) in [("expand_sparse short", 3usize), ("expand_sparse long", 40)] {
        let mut cases = Vec::new();
        let mut total = 0;
        for _ in 0..N / 16 {
            let nb = rng.gen_range(1..=nb_max);
            let counts: Vec<usize> = (0..nb).map(|_| if rng.gen_bool(0.5) { 1 } else { rng.gen_range(1..=28) }).collect();
            let n: usize = counts.iter().sum();
            let lead = rng.gen_range(0..70);
            let mut u = vec![0u64; (lead + n + 1).div_ceil(64) + 2];
            u[0] |= 1;
            let mut p = lead;
            for &c in &counts {
                u[p / 64] |= 1 << (p % 64);
                p += c;
            }
            u[p / 64] |= 1 << (p % 64);
            let lows: Vec<u8> = (0..n).map(|_| rng.gen()).collect();
            let mut blk = vec![0u32; 1 + nb + 40];
            for (i, b) in blk[1..1 + nb].iter_mut().enumerate() {
                *b = (i * 255 / nb) as u32;
            }
            total += n;
            cases.push((u, lead, n, lows, blk));
        }
        let f = |g: unsafe fn(*const u8, *const u64, u32, usize, *const u32, u32, *mut u32), out: &mut Vec<u32>| {
            let ns = ns_per_call(cases.len(), || {
                let mut k = 0;
                for (u, lead, n, lows, blk) in &cases {
                    unsafe { g(lows.as_ptr(), u.as_ptr(), *lead as u32, *n, blk.as_ptr().add(1), 5 << 16, out.as_mut_ptr().add(k)) };
                    k += n;
                }
                k as u64
            });
            ns * cases.len() as f64 / total as f64
        };
        let (a, b) = (f(expand_sparse_scalar, &mut out), f(expand_sparse_neon, &mut out));
        println!("{label:>24}: scalar {a:.2}, neon {b:.2}  (ns/value, {:.1} values/call)", total as f64 / cases.len() as f64);
    }

    // Bitmap extraction at several densities (ns per value).
    let mix = |rng: &mut StdRng| -> f64 {
        // As in unions: some empty and few-bit blocks, then denser ones.
        match rng.gen_range(0..100) {
            0..=7 => 0.0,
            8..=47 => -(rng.gen_range(1..=9) as f64),
            48..=74 => -(rng.gen_range(10..=60) as f64),
            _ => -(rng.gen_range(60..=200) as f64),
        }
    };
    for (label, dens) in [("1 bit", -1.0), ("3 bits", -3.0), ("p=0.05", 0.05), ("p=0.2", 0.2), ("p=0.5", 0.5), ("union mix", 2.0)] {
        let blocks: Vec<[u64; 4]> = (0..8192)
            .map(|_| {
                let d = if dens == 2.0 { mix(&mut rng) } else { dens };
                if d <= 0.0 {
                    let mut w = [0u64; 4];
                    while w.iter().map(|x| x.count_ones()).sum::<u32>() < (-d) as u32 {
                        let b = rng.gen_range(0..256);
                        w[b >> 6] |= 1 << (b & 63);
                    }
                    w
                } else {
                    std::array::from_fn(|_| (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(d) as u64) << b)))
                }
            })
            .collect();
        let total: usize = blocks.iter().map(|w| w.iter().map(|x| x.count_ones() as usize).sum::<usize>()).sum();
        type F = unsafe fn(&[u64; 4], u32, *mut u32) -> usize;
        let fs: [(&str, F); 4] = [
            ("scalar", extract256_scalar),
            ("neon", extract256_neon),
            ("lut", ext_neon_hybrid::<0>),
            ("sparse8", ext_neon_hybrid::<8>),
        ];
        let mut row = format!("{label:>10} ({:5.1} bits/block):", total as f64 / blocks.len() as f64);
        for (name, g) in fs {
            let ns = ns_per_call(total, || {
                let mut k = 0;
                for (i, w) in blocks.iter().enumerate() {
                    k += unsafe { g(w, (i as u32) << 8, out.as_mut_ptr().add(k)) };
                }
                assert_eq!(k, total);
                k as u64
            });
            row += &format!(" {name} {ns:.2}");
        }
        println!("{row}  (ns/value)");
    }
}

/// Workload-level timing of the benchmark queries against Roaring, built
/// in one process and measured interleaved (best of 9 passes).
/// `PYRAMID_DIST=clustered|uniform|text` (text reads `PYRAMID_TEXT`);
/// `PYRAMID_LOOP=and|or|andc|orc` loops one operation (for profilers;
/// `PYRAMID_REPS`, `PYRAMID_FEW`); `PYRAMID_STATS=1` prints the structure of
/// the queries (chunk groups, multi blocks, driven chunks, probe kinds).
///
/// `cargo test --release workload_profile -- --ignored --nocapture`
#[test]
#[ignore = "timing only"]
fn workload_profile() {
    use super::Pyramid;
    use crate::codecs::{PostingList, SetOps};
    use crate::data::{self, Distribution, QueryConfig, QueryMode, SyntheticConfig};
    use crate::codecs::roaring::Roaring;
    let (coll, min_len) = match std::env::var("PYRAMID_DIST").as_deref() {
        Ok("uniform") => {
            (data::synthetic(&SyntheticConfig { distribution: Distribution::Uniform, ..SyntheticConfig::default() }), 1000)
        }
        Ok("text") => {
            let path = std::env::var("PYRAMID_TEXT").expect("PYRAMID_TEXT: path of a text collection");
            (data::load_text(std::path::Path::new(&path), None).unwrap(), 100)
        }
        _ => (data::synthetic(&SyntheticConfig::default()), 1000),
    };
    let queries = data::generate_queries(
        &coll,
        &QueryConfig {
            num: 300,
            min_terms: 2,
            max_terms: 4,
            min_list_len: min_len,
            max_list_len: None,
            mode: QueryMode::Cooccur,
            seed: 1001,
        },
    );
    let mut built: std::collections::HashMap<u32, (Pyramid, Roaring)> = Default::default();
    for q in &queries {
        for &t in q {
            built.entry(t).or_insert_with(|| {
                let l = &coll.lists[t as usize];
                (Pyramid::build(l, coll.universe), Roaring::build(l, coll.universe))
            });
        }
    }
    let (mut pb, mut rb, mut n) = (0u64, 0u64, 0u64);
    for (t, (p, r)) in &built {
        pb += p.size_bits();
        rb += r.size_bits();
        n += coll.lists[*t as usize].len() as u64;
    }
    println!("bits/posting: Pyramid {:.2}, Roaring {:.2}", pb as f64 / n as f64, rb as f64 / n as f64);
    let qrefs: Vec<Vec<&Pyramid>> = queries.iter().map(|q| q.iter().map(|t| &built[t].0).collect()).collect();
    let rrefs: Vec<Vec<&Roaring>> = queries.iter().map(|q| q.iter().map(|t| &built[t].1).collect()).collect();
    if let Ok(op) = std::env::var("PYRAMID_LOOP") {
        // Profiling loop: one operation only, many passes.
        let mut out = Vec::new();
        let t = Instant::now();
        let mut total = 0usize;
        let few: usize = std::env::var("PYRAMID_FEW").ok().and_then(|v| v.parse().ok()).unwrap_or(qrefs.len());
        let reps = std::env::var("PYRAMID_REPS").ok().and_then(|v| v.parse().ok()).unwrap_or(200) * qrefs.len() / few;
        for _ in 0..reps {
            for q in &qrefs[..few] {
                out.clear();
                match op.as_str() {
                    "and" => Pyramid::intersect(q, &mut out),
                    "or" => Pyramid::union(q, &mut out),
                    "andc" => total += Pyramid::and_count(q) as usize,
                    _ => total += Pyramid::or_count(q) as usize,
                }
                total += out.len();
            }
        }
        println!("{op}: {:.4} ms/query ({total})", t.elapsed().as_secs_f64() * 1e3 / (reps * few) as f64);
        return;
    }
    let reps = 9;
    let mut out = Vec::new();
    let mut best = [f64::MAX; 8];
    let mut sizes = [0usize; 2];
    for _ in 0..reps {
        let mut lap = |i: usize, f: &mut dyn FnMut(usize) -> usize| {
            let t = Instant::now();
            let mut tot = 0;
            for q in 0..qrefs.len() {
                tot += f(q);
            }
            best[i] = best[i].min(t.elapsed().as_secs_f64() * 1e3 / qrefs.len() as f64);
            tot
        };
        sizes[0] = lap(0, &mut |q| {
            out.clear();
            Pyramid::intersect(&qrefs[q], &mut out);
            out.len()
        });
        lap(1, &mut |q| {
            out.clear();
            Roaring::intersect(&rrefs[q], &mut out);
            out.len()
        });
        sizes[1] = lap(2, &mut |q| {
            out.clear();
            Pyramid::union(&qrefs[q], &mut out);
            out.len()
        });
        lap(3, &mut |q| {
            out.clear();
            Roaring::union(&rrefs[q], &mut out);
            out.len()
        });
        lap(4, &mut |q| Pyramid::and_count(&qrefs[q]) as usize);
        lap(5, &mut |q| Roaring::and_native(&rrefs[q]).cardinality() as usize);
        lap(6, &mut |q| Pyramid::or_count(&qrefs[q]) as usize);
        lap(7, &mut |q| Roaring::or_native(&rrefs[q]).cardinality() as usize);
    }
    let nq = qrefs.len();
    println!(
        "AND {:.4} ms vs Roaring {:.4} ({:.2}x) | count {:.4} vs {:.4} ({:.2}x) | {} values/query",
        best[0], best[1], best[1] / best[0], best[4], best[5], best[5] / best[4], sizes[0] / nq
    );
    println!(
        "OR  {:.4} ms vs Roaring {:.4} ({:.2}x) | count {:.4} vs {:.4} ({:.2}x) | {} values/query",
        best[2], best[3], best[3] / best[2], best[6], best[7], best[7] / best[6], sizes[1] / nq
    );
    // Warm: each query repeated 8 times back to back (data cached after the first).
    let mut warm = [f64::MAX; 2];
    for _ in 0..3 {
        let t = Instant::now();
        for q in 0..nq {
            for _ in 0..8 {
                out.clear();
                Pyramid::intersect(&qrefs[q], &mut out);
            }
        }
        warm[0] = warm[0].min(t.elapsed().as_secs_f64() * 1e3 / (8 * nq) as f64);
        let t = Instant::now();
        for q in 0..nq {
            for _ in 0..8 {
                out.clear();
                Roaring::intersect(&rrefs[q], &mut out);
            }
        }
        warm[1] = warm[1].min(t.elapsed().as_secs_f64() * 1e3 / (8 * nq) as f64);
    }
    println!("AND warm (repeated query): Pyramid {:.4} ms, Roaring {:.4} ms", warm[0], warm[1]);

    if std::env::var("PYRAMID_STATS").is_err() {
        return;
    }

    // OR result structure: values per non-empty result block.
    let mut hist = [0usize; 10];
    let mut blocks = 0usize;
    for q in &qrefs {
        out.clear();
        Pyramid::union(q, &mut out);
        let mut i = 0;
        while i < out.len() {
            let b = out[i] >> 8;
            let j = i + out[i..].partition_point(|&d| d >> 8 == b);
            let n = j - i;
            hist[(usize::BITS - n.leading_zeros()) as usize] += 1;
            blocks += 1;
            i = j;
        }
    }
    println!("OR result blocks/query {}, by values (1,2-3,4-7,..,128-255,256): {:?}", blocks / qrefs.len(), &hist[1..]);

    // OR structure: chunk groups, multi blocks by member kinds, single runs.
    {
        let (mut groups, mut singles_chunks, mut multi, mut runs) = (0usize, 0usize, [0usize; 4], 0usize);
        for q in &qrefs {
            let mut keys: Vec<u16> = q.iter().flat_map(|l| l.keys.iter().copied()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                let mem: Vec<(&Pyramid, &super::Chunk)> = q
                    .iter()
                    .filter_map(|l| l.keys.binary_search(&key).ok().map(|i| (*l, &l.chunks[i])))
                    .collect();
                if mem.len() == 1 {
                    singles_chunks += 1;
                    continue;
                }
                groups += 1;
                let masks: Vec<([u64; 4], [u64; 4], bool)> = mem
                    .iter()
                    .map(|(l, c)| {
                        if c.small() {
                            let mut p = [0u64; 4];
                            for &x in l.small_vals(c) {
                                p[(x >> 14) as usize] |= 1 << ((x >> 8) & 63);
                            }
                            (p, [0; 4], true)
                        } else {
                            let v = unsafe { super::View::new(l, c) };
                            (v.present, v.dense, false)
                        }
                    })
                    .collect();
                for w in 0..4 {
                    let (mut once, mut twice) = (0u64, 0u64);
                    for m in &masks {
                        twice |= once & m.0[w];
                        once |= m.0[w];
                    }
                    let mut mm = twice;
                    while mm != 0 {
                        let bit = mm & mm.wrapping_neg();
                        mm ^= bit;
                        let (mut nd, mut nsp, mut nsm) = (0, 0, 0);
                        for m in masks.iter().filter(|m| m.0[w] & bit != 0) {
                            if m.2 {
                                nsm += 1;
                            } else if m.1[w] & bit != 0 {
                                nd += 1;
                            } else {
                                nsp += 1;
                            }
                        }
                        multi[if nsm > 0 { 3 } else if nd == 0 { 0 } else if nsp == 0 { 2 } else { 1 }] += 1;
                    }
                    // Runs: changes of owner among single blocks in union order.
                    let mut m = once;
                    let mut prev = usize::MAX;
                    while m != 0 {
                        let bit = m & m.wrapping_neg();
                        m ^= bit;
                        if twice & bit != 0 {
                            prev = usize::MAX;
                            continue;
                        }
                        let o = masks.iter().position(|x| x.0[w] & bit != 0).unwrap();
                        if o != prev {
                            runs += 1;
                            prev = o;
                        }
                    }
                }
            }
        }
        let nq = qrefs.len() as f64;
        println!(
            "OR per query: groups {:.1} (+{:.1} single-list chunks), multi blocks sparse-only {:.0}, mixed {:.0}, dense-only {:.0}, with Small {:.0}; single runs {:.0}",
            groups as f64 / nq,
            singles_chunks as f64 / nq,
            multi[0] as f64 / nq,
            multi[1] as f64 / nq,
            multi[2] as f64 / nq,
            multi[3] as f64 / nq,
            runs as f64 / nq
        );
    }

    // AND structure.
    let (mut common, mut drive, mut drive_cands, mut join, mut cand_blocks, mut all_dense) = (0, 0, 0, 0, 0, 0);
    let (mut probes_small, mut probes_absent, mut probes_dense, mut probes_sparse) = (0, 0, 0, 0);
    let mut probes_flat = 0;
    let mut drive_runs = 0usize;
    let mut first_chunks = 0;
    let mut dcard_hist = [0usize; 8];
    for q in &qrefs {
        let mut order: Vec<&Pyramid> = q.clone();
        order.sort_by_key(|l| l.card);
        first_chunks += order[0].keys.len();
        for (ci, &key) in order[0].keys.iter().enumerate() {
            let mut chunks = vec![&order[0].chunks[ci]];
            let mut ok = true;
            for l in &order[1..] {
                match l.keys.binary_search(&key) {
                    Ok(j) => chunks.push(&l.chunks[j]),
                    Err(_) => ok = false,
                }
            }
            if !ok {
                continue;
            }
            common += 1;
            let (dj, dc) = chunks.iter().enumerate().min_by_key(|(_, c)| c.card).unwrap();
            dcard_hist[match dc.card {
                0..=4 => 0,
                5..=8 => 1,
                9..=16 => 2,
                17..=32 => 3,
                33..=64 => 4,
                65..=256 => 5,
                257..=2048 => 6,
                _ => 7,
            }] += 1;
            let driven = dc.card <= 64 || (dc.card as usize <= super::DRIVE_MAX && {
                let l = order[dj];
                let blocks = unsafe {
                    let m = l.meta.as_ptr().add(dc.off as usize);
                    (*m).count_ones() + (*m.add(1)).count_ones() + (*m.add(2)).count_ones() + (*m.add(3)).count_ones()
                };
                dc.card <= super::DRIVE_VPB * blocks
            });
            if driven {
                drive += 1;
                let l = order[dj];
                let vals: Vec<u32> = {
                    let mut v = Vec::new();
                    l.decode_chunk(dc, 0, &mut v);
                    v
                };
                drive_cands += vals.len();
                let mut blocks: Vec<u32> = vals.iter().map(|&x| x >> 8).collect();
                blocks.dedup();
                drive_runs += blocks.len();
                let mut cand = vals.clone();
                for (j, c) in chunks.iter().enumerate() {
                    if j == dj {
                        continue;
                    }
                    let lj = order[j];
                    if c.small() {
                        probes_small += cand.len();
                        let s = lj.small_vals(c);
                        cand.retain(|&x| s.contains(&(x as u16)));
                    } else {
                        let v = unsafe { super::View::new(lj, c) };
                        for &x in &cand {
                            let b = x >> 8;
                            let (w, bit) = ((b >> 6) as usize, 1u64 << (b & 63));
                            if v.flat {
                                probes_flat += 1;
                            } else if v.present[w] & bit == 0 {
                                probes_absent += 1;
                            } else if v.dense[w] & bit != 0 {
                                probes_dense += 1;
                            } else {
                                probes_sparse += 1;
                            }
                        }
                        cand.retain(|&x| unsafe { v.contains(x as u16) });
                    }
                    if cand.is_empty() {
                        break;
                    }
                }
            } else {
                join += 1;
                let views: Vec<super::View> = chunks
                    .iter()
                    .enumerate()
                    .map(|(j, c)| unsafe { super::View::new(order[j], c) })
                    .collect();
                for w in 0..4 {
                    let c = views.iter().fold(!0u64, |a, v| a & v.present[w]);
                    let d = views.iter().fold(!0u64, |a, v| a & v.dense[w]);
                    cand_blocks += c.count_ones() as usize;
                    all_dense += (c & d).count_ones() as usize;
                }
            }
        }
    }
    let nq = qrefs.len() as f64;
    println!(
        "AND per query: first-list chunks {:.1}, common chunks {:.1}, drive {:.1} (cands {:.1}), join {:.1} (cand blocks {:.1}, all dense {:.1})",
        first_chunks as f64 / nq,
        common as f64 / nq,
        drive as f64 / nq,
        drive_cands as f64 / nq,
        join as f64 / nq,
        cand_blocks as f64 / nq,
        all_dense as f64 / nq
    );
    println!(
        "AND driver blocks per query {:.1}; probes per query: flat {:.1}, small {:.1}, tiered absent {:.1}, dense {:.1}, sparse {:.1}; driver card hist (<=4,8,16,32,64,256,2048,more) {:?}",
        drive_runs as f64 / nq,
        probes_flat as f64 / nq,
        probes_small as f64 / nq,
        probes_absent as f64 / nq,
        probes_dense as f64 / nq,
        probes_sparse as f64 / nq,
        dcard_hist
    );
}
