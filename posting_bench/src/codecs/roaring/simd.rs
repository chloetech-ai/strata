//! Kernels on sorted, duplicate-free `u16` arrays (Roaring array containers).
//!
//! Intersection:
//! * [`intersect_scalar`]: branch-free two-pointer merge, the portable fallback.
//! * [`intersect_galloping`]: exponential search of every element of the small
//!   array in the large one. [`intersect`] uses it when one side is more than
//!   [`GALLOP_RATIO`] = 64 times larger (CRoaring's threshold).
//! * [`intersect_sse42`]: port of CRoaring's `intersect_vector16` (Lemire et
//!   al., "Roaring Bitmaps: Implementation of an Optimized Software Library",
//!   SPE 2018, after Schlegel, Willhalm and Lehner, "Fast Sorted-Set
//!   Intersection using SIMD Instructions", ADMS 2011). One `pcmpistrm`
//!   compares a block of 8 lanes of A with 8 lanes of B all-pairs and returns
//!   an 8-bit match mask over A's lanes; the mask indexes a 256-entry `pshufb`
//!   table that packs the matching lanes to the front of the vector.
//! * [`intersect_avx512`]: the same 8 x 8 block merge with AVX-512. A's block
//!   is broadcast to the four 128-bit lanes of a zmm register and compared
//!   (`vpcmpeqw`) with B's block rotated by 0..3 positions (one rotation per
//!   lane), then by 4..7, so two compares cover all 64 pairs. Folding the
//!   32-bit compare mask gives the match mask, and `vpcompressw`
//!   (AVX512-VBMI2) packs the matches without a lookup table.
//!
//! Both block kernels advance the block whose maximum is smaller (both on a
//! tie) and finish the tails (`len % 8` elements) with the scalar merge. They
//! store whole 16-byte vectors at the output cursor, so they reserve
//! [`OUT_SLACK`] spare slots past `min(a.len(), b.len())`.
//!
//! [`intersect_vector`] dispatches to the SSE4.2 kernel whenever it is
//! available. On the Sapphire Rapids test machine it takes about 0.2 ns per
//! input element against about 0.26 for the AVX-512 kernel, whose zmm
//! shuffles, mask compares and compress (about 5 port-5 uops per 8 x 8 step)
//! all compete for one port; both are 12-15x faster than the scalar merge.
//! The `kernel_timing` test (ignored) reproduces the numbers.
//!
//! Union: [`union_sse`] ports CRoaring's `union_vector16` (a min/max merge
//! network on 8-lane vectors plus a duplicate-removing `pshufb`, reusing the
//! same 256-entry table); [`union_scalar`] is the branch-free fallback. Large
//! unions go through bitmaps (see `container.rs`), so array unions stay at
//! most 4096 elements.
//!
//! Bit extraction ([`BitPos::extract`]: bitmap to sorted values, as `u16`
//! inside a container or `u32` docIDs). The textbook loop
//! `while w != 0 { emit(tz(w)); w &= w - 1 }` mispredicts its exit on almost
//! every word of a sparse or medium-density bitmap. [`extract_scalar`] writes
//! a fixed number of positions per word unconditionally and advances by the
//! popcount (the "flatten" loop of simdjson: Langdale and Lemire, "Parsing
//! gigabytes of JSON per second", VLDB J. 2019), branching only for words with
//! more set bits than that. With AVX512-VBMI2, `vpcompressb` packs the bit
//! positions 0..63 selected by a word into bytes, which are widened and offset
//! (`extract_avx512_u16` / `extract_avx512_u32`), in the spirit of CRoaring's
//! `bitset_extract_setbits_avx512`.

use std::ops::Range;

/// Galloping is used when one array is more than this many times larger.
pub const GALLOP_RATIO: usize = 64;

/// Spare output slots the vector kernels may write past the result.
pub const OUT_SLACK: usize = 16;

/// Spare output slots bit extraction may write past the result.
pub const EXTRACT_SLACK: usize = 64;

/// Output element of bit extraction.
pub trait BitPos: Copy {
    /// Writes `base + 64 * i + b` for every set bit `b` of `word(i)`, for
    /// `i` in `words`, in increasing order at `dst`; returns the end.
    ///
    /// # Safety
    /// `dst` must have room for every value plus [`EXTRACT_SLACK`] slots.
    unsafe fn extract(words: Range<usize>, word: impl Fn(usize) -> u64, base: u32, dst: *mut Self) -> *mut Self;
}

impl BitPos for u16 {
    #[inline(always)]
    unsafe fn extract(words: Range<usize>, word: impl Fn(usize) -> u64, base: u32, dst: *mut u16) -> *mut u16 {
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2"))]
        return extract_avx512_u16(words, word, base, dst);
        #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2")))]
        return extract_scalar::<u16, 4>(words, word, base, dst, |x| x as u16);
    }
}

impl BitPos for u32 {
    #[inline(always)]
    unsafe fn extract(words: Range<usize>, word: impl Fn(usize) -> u64, base: u32, dst: *mut u32) -> *mut u32 {
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2"))]
        return extract_avx512_u32(words, word, base, dst);
        #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2")))]
        return extract_scalar::<u32, 4>(words, word, base, dst, |x| x);
    }
}

/// Portable bit extraction writing `U` positions per word unconditionally.
///
/// # Safety
/// As for [`BitPos::extract`] (`U <= EXTRACT_SLACK`).
#[inline(always)]
pub unsafe fn extract_scalar<T, const U: usize>(
    words: Range<usize>,
    word: impl Fn(usize) -> u64,
    base: u32,
    mut dst: *mut T,
    conv: impl Fn(u32) -> T,
) -> *mut T {
    for i in words {
        let mut w = word(i);
        let b = base.wrapping_add((i as u32) << 6);
        let cnt = w.count_ones() as usize;
        let mut p = dst;
        loop {
            // Past the last set bit these write garbage that the next word
            // (or the final length) overwrites or ignores.
            for _ in 0..U {
                p.write(conv(b.wrapping_add(w.trailing_zeros())));
                p = p.add(1);
                w &= w.wrapping_sub(1);
            }
            if w == 0 {
                break;
            }
        }
        dst = dst.add(cnt);
    }
    dst
}

/// The textbook one-branch-per-bit extraction loop, kept as the reference
/// for tests and timings.
///
/// # Safety
/// As for [`BitPos::extract`].
pub unsafe fn extract_naive<T>(
    words: Range<usize>,
    word: impl Fn(usize) -> u64,
    base: u32,
    mut dst: *mut T,
    conv: impl Fn(u32) -> T,
) -> *mut T {
    for i in words {
        let mut w = word(i);
        let b = base + ((i as u32) << 6);
        while w != 0 {
            dst.write(conv(b + w.trailing_zeros()));
            dst = dst.add(1);
            w &= w - 1;
        }
    }
    dst
}

/// Byte lanes 0..63.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2"))]
static IOTA8: [u8; 64] = {
    let mut t = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        t[i] = i as u8;
        i += 1;
    }
    t
};

/// AVX512-VBMI2 bit extraction to `u16`: per word, `vpcompressb` of the
/// byte lanes 0..63, widened to 32 `u16` lanes (64 when needed) plus the
/// word's base.
///
/// # Safety
/// As for [`BitPos::extract`].
#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2"))]
#[inline(always)]
pub unsafe fn extract_avx512_u16(
    words: Range<usize>,
    word: impl Fn(usize) -> u64,
    base: u32,
    mut dst: *mut u16,
) -> *mut u16 {
    use std::arch::x86_64::*;
    let iota = _mm512_loadu_si512(IOTA8.as_ptr() as *const _);
    let step = _mm512_set1_epi16(64);
    let mut vbase = _mm512_set1_epi16(base.wrapping_add((words.start as u32) << 6) as i16);
    for i in words {
        let w = word(i);
        let c = _mm512_maskz_compress_epi8(w, iota);
        let lo = _mm512_add_epi16(_mm512_cvtepu8_epi16(_mm512_castsi512_si256(c)), vbase);
        _mm512_storeu_si512(dst as *mut _, lo);
        let cnt = w.count_ones() as usize;
        if cnt > 32 {
            let hi = _mm512_add_epi16(_mm512_cvtepu8_epi16(_mm512_extracti64x4_epi64::<1>(c)), vbase);
            _mm512_storeu_si512(dst.add(32) as *mut _, hi);
        }
        dst = dst.add(cnt);
        vbase = _mm512_add_epi16(vbase, step);
    }
    dst
}

/// AVX512-VBMI2 bit extraction to `u32`: per word, `vpcompressb` of the
/// byte lanes 0..63, widened 16 lanes at a time (as many groups as needed).
///
/// # Safety
/// As for [`BitPos::extract`].
#[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2"))]
#[inline(always)]
pub unsafe fn extract_avx512_u32(
    words: Range<usize>,
    word: impl Fn(usize) -> u64,
    base: u32,
    mut dst: *mut u32,
) -> *mut u32 {
    use std::arch::x86_64::*;
    let iota = _mm512_loadu_si512(IOTA8.as_ptr() as *const _);
    let step = _mm512_set1_epi32(64);
    let mut vbase = _mm512_set1_epi32(base.wrapping_add((words.start as u32) << 6) as i32);
    for i in words {
        let w = word(i);
        let c = _mm512_maskz_compress_epi8(w, iota);
        let cnt = w.count_ones() as usize;
        let lane = |q: __m128i| _mm512_add_epi32(_mm512_cvtepu8_epi32(q), vbase);
        _mm512_storeu_si512(dst as *mut _, lane(_mm512_castsi512_si128(c)));
        if cnt > 16 {
            _mm512_storeu_si512(dst.add(16) as *mut _, lane(_mm512_extracti32x4_epi32::<1>(c)));
            if cnt > 32 {
                _mm512_storeu_si512(dst.add(32) as *mut _, lane(_mm512_extracti32x4_epi32::<2>(c)));
                if cnt > 48 {
                    _mm512_storeu_si512(dst.add(48) as *mut _, lane(_mm512_extracti32x4_epi32::<3>(c)));
                }
            }
        }
        dst = dst.add(cnt);
        vbase = _mm512_add_epi32(vbase, step);
    }
    dst
}

/// First index `i >= from` with `s[i] >= target` (or `s.len()`), found by
/// galloping: probe `from + 1, from + 2, from + 4, ...`, then binary-search.
#[inline]
pub fn gallop<T: Copy + Ord>(s: &[T], from: usize, target: T) -> usize {
    gallop_by(s, from, |&x| x < target)
}

/// First index `i >= from` with `!before(&s[i])` (or `s.len()`), where
/// `before` holds on a prefix of `s`.
#[inline]
pub fn gallop_by<T>(s: &[T], from: usize, before: impl Fn(&T) -> bool) -> usize {
    let n = s.len();
    if from >= n || !before(&s[from]) {
        return from;
    }
    // Invariant: before(s[lo]).
    let mut lo = from;
    let mut step = 1;
    let mut hi = from + 1;
    while hi < n && before(&s[hi]) {
        lo = hi;
        step *= 2;
        hi = from + step;
    }
    let hi = hi.min(n);
    lo + 1 + s[lo + 1..hi].partition_point(|x| before(x))
}

/// `out = a ∩ b`, choosing galloping or the block kernel by size ratio.
#[inline]
pub fn intersect(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if small.is_empty() {
        out.clear();
    } else if large.len() > GALLOP_RATIO * small.len() {
        intersect_galloping(small, large, out);
    } else {
        intersect_vector(a, b, out);
    }
}

/// The block kernel used by [`intersect`] on this target.
#[inline]
pub fn intersect_vector(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
    intersect_sse42(a, b, out);
    #[cfg(not(all(target_arch = "x86_64", target_feature = "sse4.2")))]
    intersect_scalar(a, b, out);
}

/// Scalar merge of `a[i..]` and `b[j..]`, writing matches at `dst[k..]`;
/// returns the new `k`. `dst` must have room for `min` of the remaining
/// lengths past `k`.
#[inline(always)]
unsafe fn merge_tail(a: &[u16], b: &[u16], mut i: usize, mut j: usize, dst: *mut u16, mut k: usize) -> usize {
    while i < a.len() && j < b.len() {
        let (x, y) = (*a.get_unchecked(i), *b.get_unchecked(j));
        *dst.add(k) = x;
        k += (x == y) as usize;
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    k
}

/// `out = a ∩ b` by a branch-free two-pointer merge.
pub fn intersect_scalar(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    out.clear();
    out.reserve(a.len().min(b.len()));
    // SAFETY: the merge writes at most min(len) elements (each match
    // consumes one element of each side), all of them reserved.
    unsafe {
        let k = merge_tail(a, b, 0, 0, out.as_mut_ptr(), 0);
        out.set_len(k);
    }
}

/// `out = small ∩ large` by galloping through `large`.
pub fn intersect_galloping(small: &[u16], large: &[u16], out: &mut Vec<u16>) {
    out.clear();
    out.reserve(small.len());
    let mut j = 0;
    for &x in small {
        j = gallop(large, j, x);
        match large.get(j) {
            None => break,
            Some(&y) if y == x => {
                out.push(x);
                j += 1;
            }
            Some(_) => {}
        }
    }
}

/// Scalar union of `a` and `b` written at `dst`; returns the count. `dst`
/// must have room for `a.len() + b.len()` values.
#[inline(always)]
unsafe fn union_tail(a: &[u16], b: &[u16], dst: *mut u16) -> usize {
    let (mut i, mut j, mut k) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (*a.get_unchecked(i), *b.get_unchecked(j));
        *dst.add(k) = x.min(y);
        k += 1;
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    let (ra, rb) = (&a[i..], &b[j..]);
    std::ptr::copy_nonoverlapping(ra.as_ptr(), dst.add(k), ra.len());
    k += ra.len();
    std::ptr::copy_nonoverlapping(rb.as_ptr(), dst.add(k), rb.len());
    k + rb.len()
}

/// `out = a ∪ b` by a branch-free merge.
pub fn union_scalar(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    out.clear();
    out.reserve(a.len() + b.len());
    // SAFETY: at most a.len() + b.len() values are written, all reserved.
    unsafe {
        let k = union_tail(a, b, out.as_mut_ptr());
        out.set_len(k);
    }
}

/// `out = a ∪ b` with the best kernel for this target.
#[inline]
pub fn union(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    #[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
    union_sse(a, b, out);
    #[cfg(not(all(target_arch = "x86_64", target_feature = "sse4.2")))]
    union_scalar(a, b, out);
}

/// Merges two sorted vectors of 8 `u16` into the 8 smallest and the 8
/// largest, both sorted (CRoaring's `sse_merge`, after Inoue and Taura,
/// "SIMD- and Cache-Friendly Algorithm for Sorting an Array of Structures",
/// VLDB 2015): one min/max, then seven rounds of rotating the min vector by
/// one lane and min/max against the max vector.
#[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
#[inline(always)]
unsafe fn merge8(
    a: std::arch::x86_64::__m128i,
    b: std::arch::x86_64::__m128i,
) -> (std::arch::x86_64::__m128i, std::arch::x86_64::__m128i) {
    use std::arch::x86_64::*;
    let t = _mm_min_epu16(a, b);
    let mut max = _mm_max_epu16(a, b);
    let t = _mm_alignr_epi8::<2>(t, t);
    let mut min = _mm_min_epu16(t, max);
    max = _mm_max_epu16(t, max);
    for _ in 0..6 {
        let t = _mm_alignr_epi8::<2>(min, min);
        min = _mm_min_epu16(t, max);
        max = _mm_max_epu16(t, max);
    }
    (_mm_alignr_epi8::<2>(min, min), max)
}

/// Stores the lanes of `new` that differ from their predecessor (the last
/// lane of `old` for lane 0) at `dst`; returns how many. Writes 8 lanes.
#[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
#[inline(always)]
unsafe fn store_unique(old: std::arch::x86_64::__m128i, new: std::arch::x86_64::__m128i, dst: *mut u16) -> usize {
    use std::arch::x86_64::*;
    let prev = _mm_alignr_epi8::<14>(new, old);
    let dup = _mm_movemask_epi8(_mm_packs_epi16(_mm_cmpeq_epi16(prev, new), _mm_setzero_si128())) as usize;
    let keep = !dup & 0xFF;
    let shuf = _mm_loadu_si128(SHUFFLE16.as_ptr().add(keep) as *const __m128i);
    _mm_storeu_si128(dst as *mut __m128i, _mm_shuffle_epi8(new, shuf));
    keep.count_ones() as usize
}

/// `out = a ∪ b`: CRoaring's `union_vector16`. Vectors of 8 are merged with
/// [`merge8`], always loading next from the array whose next vector starts
/// lower; the low half is stored with duplicates removed and the high half
/// carried. The leftovers (carried half plus the `< 8` trailing values of the
/// array that ran out of vectors) are merged with the rest of the other
/// array by the scalar kernel.
#[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
pub fn union_sse(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    use std::arch::x86_64::*;
    if a.len() < 8 || b.len() < 8 {
        return union_scalar(a, b, out);
    }
    out.clear();
    out.reserve(a.len() + b.len() + OUT_SLACK);
    let (la, lb) = (a.len() / 8, b.len() / 8);
    let (pa, pb) = (a.as_ptr() as *const __m128i, b.as_ptr() as *const __m128i);
    // SAFETY: vector loads stay below la / lb full vectors; stores write 8
    // lanes at most `a.len() + b.len()` values in, within the slack.
    unsafe {
        let start = out.as_mut_ptr();
        let mut dst = start;
        let (mut ia, mut ib) = (1, 1);
        let (mut vmin, mut vmax) = merge8(_mm_loadu_si128(pa), _mm_loadu_si128(pb));
        // All-ones never equals the first (smallest) lane: it would need 8
        // copies of 65535 among two duplicate-free arrays.
        dst = dst.add(store_unique(_mm_set1_epi16(-1), vmin, dst));
        let mut last = vmin;
        if ia < la && ib < lb {
            let (mut ca, mut cb) = (a[8], b[8]);
            loop {
                let v;
                let done;
                if ca <= cb {
                    v = _mm_loadu_si128(pa.add(ia));
                    ia += 1;
                    done = ia == la;
                    if !done {
                        ca = *a.get_unchecked(8 * ia);
                    }
                } else {
                    v = _mm_loadu_si128(pb.add(ib));
                    ib += 1;
                    done = ib == lb;
                    if !done {
                        cb = *b.get_unchecked(8 * ib);
                    }
                }
                (vmin, vmax) = merge8(v, vmax);
                dst = dst.add(store_unique(last, vmin, dst));
                last = vmin;
                if done {
                    break;
                }
            }
        }
        // Leftovers: the carried high half (deduplicated against the last
        // store) and the tail of the array whose vectors ran out.
        let mut carry = [0u16; 8];
        let nc = store_unique(last, vmax, carry.as_mut_ptr());
        let (tail, rest) = if ia == la { (&a[8 * la..], &b[8 * ib..]) } else { (&b[8 * lb..], &a[8 * ia..]) };
        let mut small = [0u16; 16];
        let ns = union_tail(&carry[..nc], tail, small.as_mut_ptr());
        let k = dst.offset_from(start) as usize;
        let k = k + union_tail(&small[..ns], rest, dst);
        out.set_len(k);
    }
}

/// `pshufb` controls that pack the u16 lanes selected by an 8-bit mask to
/// the front (CRoaring's `shuffle_mask16`). Unused bytes select zero.
#[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
static SHUFFLE16: [[u8; 16]; 256] = {
    let mut t = [[0x80u8; 16]; 256];
    let mut m = 0;
    while m < 256 {
        let (mut lane, mut k) = (0, 0);
        while lane < 8 {
            if m & (1 << lane) != 0 {
                t[m][2 * k] = 2 * lane as u8;
                t[m][2 * k + 1] = 2 * lane as u8 + 1;
                k += 1;
            }
            lane += 1;
        }
        m += 1;
    }
    t
};

/// `out = a ∩ b`: CRoaring's `intersect_vector16` (SSE4.2 `pcmpistrm`).
#[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
pub fn intersect_sse42(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    use std::arch::x86_64::*;
    const MODE: i32 = _SIDD_UWORD_OPS | _SIDD_CMP_EQUAL_ANY | _SIDD_BIT_MASK;

    out.clear();
    out.reserve(a.len().min(b.len()) + OUT_SLACK);
    let (sta, stb) = (a.len() & !7, b.len() & !7);
    let (pa, pb, dst) = (a.as_ptr(), b.as_ptr(), out.as_mut_ptr());
    let (mut i, mut j, mut k) = (0usize, 0usize, 0usize);
    // SAFETY: vector loads read a[i..i + 8] with i < sta (same for b); each
    // store writes 8 lanes at k <= min(i, j) <= min(len), within the
    // reserved min(len) + OUT_SLACK slots.
    unsafe {
        if sta > 0 && stb > 0 {
            let mut va = _mm_loadu_si128(pa as *const __m128i);
            let mut vb = _mm_loadu_si128(pb as *const __m128i);
            macro_rules! step {
                ($mask:expr, $done:lifetime) => {{
                    let r = $mask as u32 as usize;
                    let shuf = _mm_loadu_si128(SHUFFLE16.as_ptr().add(r) as *const __m128i);
                    _mm_storeu_si128(dst.add(k) as *mut __m128i, _mm_shuffle_epi8(va, shuf));
                    k += r.count_ones() as usize;
                    let (amax, bmax) = (*pa.add(i + 7), *pb.add(j + 7));
                    if amax <= bmax {
                        i += 8;
                        if i == sta {
                            break $done;
                        }
                        va = _mm_loadu_si128(pa.add(i) as *const __m128i);
                    }
                    if bmax <= amax {
                        j += 8;
                        if j == stb {
                            break $done;
                        }
                        vb = _mm_loadu_si128(pb.add(j) as *const __m128i);
                    }
                }};
            }
            'blocks: {
                // `pcmpistrm` reads a zero lane as end-of-string. Only the
                // first lane of a block can be zero (the arrays are sorted
                // and distinct), and only in the first block, so the
                // explicit-length `pcmpestrm` handles that case.
                while *pa.add(i) == 0 || *pb.add(j) == 0 {
                    step!(_mm_cvtsi128_si32(_mm_cmpestrm::<MODE>(vb, 8, va, 8)), 'blocks);
                }
                loop {
                    step!(_mm_cvtsi128_si32(_mm_cmpistrm::<MODE>(vb, va)), 'blocks);
                }
            }
        }
        let k = merge_tail(a, b, i, j, dst, k);
        out.set_len(k);
    }
}

/// `vpshufb` controls rotating the 8 u16 lanes of each 128-bit lane `g` of a
/// zmm register by `g + off` positions.
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512bw",
    target_feature = "avx512vl",
    target_feature = "avx512vbmi2"
))]
const fn rotations(off: usize) -> [u8; 64] {
    let mut t = [0u8; 64];
    let mut g = 0;
    while g < 4 {
        let mut p = 0;
        while p < 8 {
            let src = (p + g + off) % 8;
            t[16 * g + 2 * p] = 2 * src as u8;
            t[16 * g + 2 * p + 1] = 2 * src as u8 + 1;
            p += 1;
        }
        g += 1;
    }
    t
}

#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512bw",
    target_feature = "avx512vl",
    target_feature = "avx512vbmi2"
))]
static ROTATIONS: [[u8; 64]; 2] = [rotations(0), rotations(4)];

/// `out = a ∩ b`: 8 x 8 all-pairs blocks with AVX-512BW compares and a
/// VBMI2 `vpcompressw`.
#[cfg(all(
    target_arch = "x86_64",
    target_feature = "avx512bw",
    target_feature = "avx512vl",
    target_feature = "avx512vbmi2"
))]
pub fn intersect_avx512(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    use std::arch::x86_64::*;

    out.clear();
    out.reserve(a.len().min(b.len()) + OUT_SLACK);
    let (sta, stb) = (a.len() & !7, b.len() & !7);
    let (pa, pb, dst) = (a.as_ptr(), b.as_ptr(), out.as_mut_ptr());
    let (mut i, mut j, mut k) = (0usize, 0usize, 0usize);
    // SAFETY: as in `intersect_sse42`.
    unsafe {
        if sta > 0 && stb > 0 {
            let rot_lo = _mm512_loadu_si512(ROTATIONS[0].as_ptr() as *const _);
            let rot_hi = _mm512_loadu_si512(ROTATIONS[1].as_ptr() as *const _);
            let load = |p: *const u16| _mm512_broadcast_i32x4(_mm_loadu_si128(p as *const __m128i));
            // B's two rotation vectors are built once per B block.
            let rotate = |v: __m512i| (_mm512_shuffle_epi8(v, rot_lo), _mm512_shuffle_epi8(v, rot_hi));
            let mut va = load(pa);
            let (mut vb_lo, mut vb_hi) = rotate(load(pb));
            loop {
                // Bit 8g + p of `m` is set when A[p] equals B[(p + g) % 8]
                // or B[(p + g + 4) % 8].
                let m = _mm512_cmpeq_epi16_mask(va, vb_lo) | _mm512_cmpeq_epi16_mask(va, vb_hi);
                let m = m | (m >> 16);
                let r = (m | (m >> 8)) as u8;
                let packed = _mm_maskz_compress_epi16(r, _mm512_castsi512_si128(va));
                _mm_storeu_si128(dst.add(k) as *mut __m128i, packed);
                k += r.count_ones() as usize;
                let (amax, bmax) = (*pa.add(i + 7), *pb.add(j + 7));
                if amax <= bmax {
                    i += 8;
                    if i == sta {
                        break;
                    }
                    va = load(pa.add(i));
                }
                if bmax <= amax {
                    j += 8;
                    if j == stb {
                        break;
                    }
                    (vb_lo, vb_hi) = rotate(load(pb.add(j)));
                }
            }
        }
        let k = merge_tail(a, b, i, j, dst, k);
        out.set_len(k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    /// About `n` distinct sorted values drawn from `[lo, lo + span)`.
    fn random_array(rng: &mut StdRng, n: usize, lo: u32, span: u32) -> Vec<u16> {
        let span = span.min(65536 - lo);
        let p = (n as f64 / span as f64).min(1.0);
        (lo..lo + span).filter(|_| rng.gen_bool(p)).map(|x| x as u16).collect()
    }

    fn reference(a: &[u16], b: &[u16]) -> Vec<u16> {
        a.iter().copied().filter(|x| b.binary_search(x).is_ok()).collect()
    }

    /// Pairs of arrays of many sizes (including lengths that are not
    /// multiples of 8 or 32), overlap levels, and edge values 0 and 65535.
    fn pairs() -> Vec<(Vec<u16>, Vec<u16>)> {
        let mut rng = StdRng::seed_from_u64(7);
        let mut out = vec![
            (vec![], vec![]),
            (vec![], vec![1, 2, 3]),
            (vec![0], vec![0]),
            (vec![65535], vec![65535]),
            ((0..8).collect(), (0..8).collect()),
            ((0..16).collect(), (8..24).collect()),
            ((0..4096).collect(), (0..4096).step_by(3).collect()),
            ((0..65535).step_by(16).chain([65535]).collect(), (0..=65535).step_by(48).collect()),
        ];
        let sizes = [1usize, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 100, 255, 256, 257, 1000, 4096];
        for &na in &sizes {
            for &nb in &sizes {
                // Dense overlap, sparse overlap, and disjoint-ish ranges.
                let span = (2 * na.max(nb)) as u32 + 8;
                out.push((random_array(&mut rng, na, 0, span), random_array(&mut rng, nb, 0, span)));
                out.push((random_array(&mut rng, na, 0, 65536), random_array(&mut rng, nb, 0, 65536)));
                let lo = rng.gen_range(0..1000);
                out.push((random_array(&mut rng, na, lo, span), random_array(&mut rng, nb, 0, 4 * span)));
            }
        }
        // Many sizes not tied to the vector width.
        for _ in 0..300 {
            let (na, nb) = (rng.gen_range(0..600), rng.gen_range(0..600));
            let span = rng.gen_range(1..4000);
            out.push((random_array(&mut rng, na, 0, span), random_array(&mut rng, nb, 0, span)));
        }
        out
    }

    fn check_kernel(name: &str, f: fn(&[u16], &[u16], &mut Vec<u16>)) {
        let mut out = vec![1, 2, 3]; // stale content must be cleared
        for (a, b) in pairs() {
            let want = reference(&a, &b);
            f(&a, &b, &mut out);
            assert_eq!(out, want, "{name}: |a|={} |b|={}", a.len(), b.len());
            f(&b, &a, &mut out);
            assert_eq!(out, want, "{name} (swapped): |a|={} |b|={}", a.len(), b.len());
        }
    }

    #[test]
    fn scalar_kernels() {
        check_kernel("scalar", intersect_scalar);
        check_kernel("dispatch", intersect);
        check_kernel("vector", intersect_vector);
        let mut out = Vec::new();
        for (a, b) in pairs() {
            let want = reference(&a, &b);
            let (s, l) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
            intersect_galloping(s, l, &mut out);
            assert_eq!(out, want, "galloping: |a|={} |b|={}", a.len(), b.len());
        }
    }

    #[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
    #[test]
    fn sse42_matches_scalar() {
        check_kernel("sse4.2", intersect_sse42);
    }

    #[cfg(all(
        target_arch = "x86_64",
        target_feature = "avx512bw",
        target_feature = "avx512vl",
        target_feature = "avx512vbmi2"
    ))]
    #[test]
    fn avx512_matches_scalar() {
        check_kernel("avx512", intersect_avx512);
    }

    #[test]
    fn union_matches_reference() {
        let mut out = Vec::new();
        for (a, b) in pairs() {
            let mut want: Vec<u16> = a.iter().chain(&b).copied().collect();
            want.sort_unstable();
            want.dedup();
            union_scalar(&a, &b, &mut out);
            assert_eq!(out, want, "scalar |a|={} |b|={}", a.len(), b.len());
            union(&b, &a, &mut out);
            assert_eq!(out, want, "dispatch |a|={} |b|={}", a.len(), b.len());
            #[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
            {
                union_sse(&a, &b, &mut out);
                assert_eq!(out, want, "sse |a|={} |b|={}", a.len(), b.len());
                union_sse(&b, &a, &mut out);
                assert_eq!(out, want, "sse swapped |a|={} |b|={}", a.len(), b.len());
            }
        }
    }

    /// Random bitmaps of 1024 words at densities from empty to full.
    fn bitmaps() -> Vec<Vec<u64>> {
        let mut rng = StdRng::seed_from_u64(9);
        let mut out = vec![vec![0u64; 1024], vec![!0u64; 1024]];
        for p in [0.001, 0.01, 0.1, 0.3, 0.5, 0.7, 0.9, 0.99] {
            out.push((0..1024).map(|_| (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(p) as u64) << b))).collect());
        }
        // Words with exactly k bits set, for every k, at shuffled positions.
        out.push(
            (0..1024).map(|i| if i % 65 == 64 { !0 } else { (1u64 << (i % 65)) - 1 }.rotate_left(i as u32)).collect(),
        );
        out
    }

    type ExtractFn<T> = unsafe fn(Range<usize>, &[u64], u32, *mut T) -> *mut T;

    fn check_extract<T: Copy + PartialEq + std::fmt::Debug>(name: &str, f: ExtractFn<T>, conv: fn(u32) -> T) {
        for words in bitmaps() {
            for (range, base) in [(0..1024, 0u32), (3..700, 0), (0..1024, 7 << 16), (1023..1024, 0xFFFF_0000)] {
                let mut want: Vec<T> = Vec::new();
                for i in range.clone() {
                    for b in 0..64 {
                        if words[i] >> b & 1 == 1 {
                            want.push(conv(base + (i as u32) * 64 + b));
                        }
                    }
                }
                let mut got: Vec<T> = Vec::with_capacity(want.len() + EXTRACT_SLACK);
                // SAFETY: capacity for every value plus the slack.
                unsafe {
                    let end = f(range.clone(), &words, base, got.as_mut_ptr());
                    got.set_len(end.offset_from(got.as_ptr()) as usize);
                }
                assert!(got == want, "{name}: range {range:?} base {base}");
            }
        }
    }

    #[test]
    fn extract_kernels_match_naive() {
        check_extract::<u16>(
            "naive u16",
            |r, w, b, d| unsafe { extract_naive(r, |i| w[i], b, d, |x| x as u16) },
            |x| x as u16,
        );
        check_extract::<u16>(
            "scalar u16",
            |r, w, b, d| unsafe { extract_scalar::<_, 4>(r, |i| w[i], b, d, |x| x as u16) },
            |x| x as u16,
        );
        check_extract::<u32>(
            "scalar u32",
            |r, w, b, d| unsafe { extract_scalar::<_, 8>(r, |i| w[i], b, d, |x| x) },
            |x| x,
        );
        check_extract::<u16>("dispatch u16", |r, w, b, d| unsafe { u16::extract(r, |i| w[i], b, d) }, |x| x as u16);
        check_extract::<u32>("dispatch u32", |r, w, b, d| unsafe { u32::extract(r, |i| w[i], b, d) }, |x| x);
        #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2"))]
        {
            check_extract::<u16>(
                "avx512 u16",
                |r, w, b, d| unsafe { extract_avx512_u16(r, |i| w[i], b, d) },
                |x| x as u16,
            );
            check_extract::<u32>("avx512 u32", |r, w, b, d| unsafe { extract_avx512_u32(r, |i| w[i], b, d) }, |x| x);
        }
    }

    /// Throwaway kernel timings: `cargo test --release -- --ignored --nocapture kernel_timing`
    #[test]
    #[ignore = "timing only"]
    fn kernel_timing() {
        use std::hint::black_box;
        use std::time::Instant;
        fn ns_per<R>(items: usize, mut f: impl FnMut() -> R) -> f64 {
            let reps = (2_000_000 / items.max(1)).clamp(20, 20_000);
            let mut best = f64::MAX;
            for _ in 0..5 {
                let s = Instant::now();
                for _ in 0..reps {
                    black_box(f());
                }
                best = best.min(s.elapsed().as_secs_f64() * 1e9 / (reps * items) as f64);
            }
            best
        }
        let mut rng = StdRng::seed_from_u64(1);
        let mut out = Vec::new();
        println!("intersection, ns per input element (|a| + |b|):");
        for (na, nb, span) in
            [(1000, 1000, 65_536), (4000, 4000, 65_536), (4000, 4000, 8000), (500, 4000, 65_536), (200, 4000, 16_000)]
        {
            let a = random_array(&mut rng, na, 0, span);
            let b = random_array(&mut rng, nb, 0, span);
            let n = a.len() + b.len();
            let mut row = format!("  |a|={:>4} |b|={:>4} span={span:>5}:", a.len(), b.len());
            row += &format!(" scalar {:.2}", ns_per(n, || intersect_scalar(&a, &b, &mut out)));
            #[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
            {
                row += &format!(" sse4.2 {:.2}", ns_per(n, || intersect_sse42(&a, &b, &mut out)));
            }
            #[cfg(all(
                target_arch = "x86_64",
                target_feature = "avx512bw",
                target_feature = "avx512vl",
                target_feature = "avx512vbmi2"
            ))]
            {
                row += &format!(" avx512 {:.2}", ns_per(n, || intersect_avx512(&a, &b, &mut out)));
            }
            println!("{row}");
        }
        println!("union, ns per input element:");
        for (na, nb, span) in [(100, 100, 65_536), (1000, 1000, 65_536), (2000, 2000, 8000), (200, 3000, 65_536)] {
            let a = random_array(&mut rng, na, 0, span);
            let b = random_array(&mut rng, nb, 0, span);
            let n = a.len() + b.len();
            let mut row = format!("  |a|={:>4} |b|={:>4} span={span:>5}:", a.len(), b.len());
            row += &format!(" scalar {:.2}", ns_per(n, || union_scalar(&a, &b, &mut out)));
            #[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
            {
                row += &format!(" sse {:.2}", ns_per(n, || union_sse(&a, &b, &mut out)));
            }
            println!("{row}");
        }
        // 64 different bitmaps per density, so the branch predictor cannot
        // learn one bitmap's pattern.
        println!("bit extraction, ns per 64-bit word (64 distinct bitmaps):");
        let mut o16: Vec<u16> = Vec::with_capacity(65_536 + EXTRACT_SLACK);
        let mut o32: Vec<u32> = Vec::with_capacity(65_536 + EXTRACT_SLACK);
        for p in [0.002, 0.01, 0.05, 0.1, 0.3, 0.5, 0.9] {
            let set: Vec<Vec<u64>> = (0..64)
                .map(|_| (0..1024).map(|_| (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(p) as u64) << b))).collect())
                .collect();
            let mut row = format!("  bits/word {:>5.1}:", 64.0 * p);
            let n = 64 * 1024;
            let (p16, p32) = (o16.as_mut_ptr(), o32.as_mut_ptr());
            macro_rules! time {
                ($name:expr, $f:expr) => {
                    // SAFETY: capacity 65536 + slack covers any 1024-word bitmap.
                    row += &format!(
                        " {} {:.2}",
                        $name,
                        ns_per(n, || set.iter().map(|w| unsafe { $f(&w[..]) } as usize).sum::<usize>())
                    );
                };
            }
            time!("naive16", |w: &[u64]| extract_naive(0..1024, |i| w[i], 0, p16, |x| x as u16));
            time!("scalar16", |w: &[u64]| extract_scalar::<_, 4>(0..1024, |i| w[i], 0, p16, |x| x as u16));
            time!("naive32", |w: &[u64]| extract_naive(0..1024, |i| w[i], 0, p32, |x| x));
            time!("scalar32/4", |w: &[u64]| extract_scalar::<_, 4>(0..1024, |i| w[i], 0, p32, |x| x));
            time!("scalar32/8", |w: &[u64]| extract_scalar::<_, 8>(0..1024, |i| w[i], 0, p32, |x| x));
            #[cfg(all(target_arch = "x86_64", target_feature = "avx512bw", target_feature = "avx512vbmi2"))]
            {
                time!("avx512_16", |w: &[u64]| extract_avx512_u16(0..1024, |i| w[i], 0, p16));
                time!("avx512_32", |w: &[u64]| extract_avx512_u32(0..1024, |i| w[i], 0, p32));
            }
            println!("{row}");
        }
    }

    #[test]
    fn gallop_bounds() {
        let s: Vec<u16> = (0..1000).map(|x| x * 2).collect();
        for from in [0, 1, 5, 999, 1000] {
            for t in [0u16, 1, 2, 3, 500, 1997, 1998, 1999, 65535] {
                assert_eq!(gallop(&s, from, t), from.max(s.partition_point(|&x| x < t)));
            }
        }
    }
}
