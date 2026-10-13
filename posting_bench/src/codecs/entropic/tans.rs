//! Context-switching tANS (Duda's asymmetric numeral systems, table variant,
//! as in FSE) with the raw gap bits interleaved in the same bit stream.
//!
//! A model has `r`-bit states (`r` in 7..=10, `L = 2^r`). All contexts of a
//! model share the state space `[L, 2L)`, so switching context between
//! symbols is free: the decoder only picks another table. A block has
//! [`LANES`] = 2 states that code alternate symbols. Its stream is
//! `[initial state of lane 0: r bits][of lane 1: r bits]` (one state if the
//! block has a single symbol) followed, for every symbol in decoding order,
//! by `[state bits][raw gap bits]`, LSB-first.
//!
//! **Bits-back on the final states.** The encoder runs backwards and may
//! start each lane in any state; the decoder ends each lane in that state.
//! Instead of a fixed start (wasting r bits per lane), the encoder starts
//! lane `l` in `L + low`, where `low` holds the low `min(raw bits, r)` raw
//! bits of the lane's last symbol, and leaves only the remaining raw bits
//! of that symbol in the stream. The decoder reads them back from the final
//! state ([`Decoder::gap_last`]): this saves up to `2r` bits per block.
//!
//! Decode table entry (one `u64` per context and state):
//!
//! ```text
//! bits  0..8   state bits to read, nb (<= r, so `bzhi(window, e)` and
//!              `x >> e` use the entry itself as the count)
//! bits  8..16  bits to consume, tot = nb + raw bits of the token
//! bits 16..48  smallest gap of the token (its raw bits are ORed in)
//! bits 48..64  next table index = next context · L + new state base
//! ```
//!
//! One decoding step is a load, a `bzhi` (with a parallel shift) and an
//! `or` on the critical path: `next = (e >> 48) | bzhi(window, e)`. The
//! context of the next symbol is folded into the entry, so it costs nothing
//! extra, and the token itself is never materialised. The gap is
//! `(e >> 16) | (bzhi(window, e >> 8) >> e)`, using that BZHI reads only the
//! low byte of its count and shifts only the low six bits.

use super::model::{Model, BASE, LANES, NRAW, NTOK};

/// Builds the decode table of `model` (`nctx · 2^r` entries).
pub fn decode_table(model: &Model) -> Box<[u64]> {
    let (r, l) = (model.r, 1usize << model.r);
    let nctx = model.nctx();
    let mut table = vec![0u64; nctx * l].into_boxed_slice();
    for (c, f) in model.freq.iter().enumerate() {
        let spread = spread(f, r);
        let mut next: [u32; NTOK] = std::array::from_fn(|t| f[t] as u32);
        for (u, &s) in spread.iter().enumerate() {
            let s = s as usize;
            let xp = next[s];
            next[s] += 1;
            let nb = r - (31 - xp.leading_zeros());
            let newbase = (xp << nb) - l as u32;
            let next = model.ctx_of[s] as u64 * l as u64 + newbase as u64;
            let tot = nb + NRAW[s];
            debug_assert!(next < 1 << 16 && tot < 64);
            table[c * l + u] = nb as u64 | (tot as u64) << 8 | (BASE[s] as u64) << 16 | next << 48;
        }
    }
    table
}

/// FSE symbol spread over `2^r` states: symbol `s` occupies `f[s]` states,
/// scattered with an odd step so that every symbol's states are spread over
/// the table.
fn spread(f: &[u16; NTOK], r: u32) -> Vec<u8> {
    let l = 1usize << r;
    let mut spread = vec![0u8; l];
    let step = (l >> 1) + (l >> 3) + 3;
    let mut pos = 0usize;
    for (s, &fs) in f.iter().enumerate() {
        for _ in 0..fs {
            spread[pos] = s as u8;
            pos = (pos + step) & (l - 1);
        }
    }
    debug_assert_eq!(pos, 0);
    spread
}

/// Encoder tables of one context.
pub struct EncContext {
    freq: [u16; NTOK],
    /// Start of every symbol's run in `states`.
    cum: [u16; NTOK],
    /// `states[cum[s] + x - f[s]]`: the state `L + u` whose decoding yields
    /// `s` and the intermediate value `x ∈ [f[s], 2 f[s])`.
    states: Vec<u16>,
}

/// Encoder for a model.
pub struct Encoder {
    ctxs: Vec<EncContext>,
    ctx_of: [u8; NTOK],
    start_ctx: u8,
    r: u32,
}

impl Encoder {
    pub fn new(model: &Model) -> Self {
        let l = 1usize << model.r;
        let ctxs = model
            .freq
            .iter()
            .map(|f| {
                let mut cum = [0u16; NTOK];
                let mut acc = 0u16;
                for s in 0..NTOK {
                    cum[s] = acc;
                    acc += f[s];
                }
                let spread = spread(f, model.r);
                let mut states = vec![0u16; l];
                let mut fill = cum;
                for (u, &s) in spread.iter().enumerate() {
                    states[fill[s as usize] as usize] = (l + u) as u16;
                    fill[s as usize] += 1;
                }
                EncContext { freq: *f, cum, states }
            })
            .collect();
        Self { ctxs, ctx_of: model.ctx_of, start_ctx: model.start_ctx, r: model.r }
    }

    /// Append the stream of one block of gaps (at least one) to `out`.
    /// `scratch` is reused between calls.
    pub fn encode_block(&self, gaps: &[u32], out: &mut crate::bits::BitVec, scratch: &mut Vec<(u64, u32)>) {
        debug_assert!(!gaps.is_empty());
        scratch.clear();
        let m = gaps.len();
        let (r, l) = (self.r, 1u32 << self.r);
        // Final decoder states: each lane's last symbol's low raw bits.
        let mut x = [l; LANES];
        for i in m.saturating_sub(LANES)..m {
            let (_, nraw) = super::model::token(gaps[i]);
            x[i % LANES] = l + (gaps[i] & low_mask(nraw.min(r)));
        }
        for i in (0..m).rev() {
            let (t, nraw) = super::model::token(gaps[i]);
            let ctx = if i < LANES {
                self.start_ctx
            } else {
                self.ctx_of[super::model::token(gaps[i - LANES]).0]
            } as usize;
            let c = &self.ctxs[ctx];
            let f = c.freq[t] as u32;
            assert!(f > 0, "token {t} not codable in context {ctx}");
            let x = &mut x[i % LANES];
            // Smallest n with (x >> n) < 2f.
            let mut n = 0;
            while (*x >> n) >= 2 * f {
                n += 1;
            }
            let state_bits = (*x & ((1u32 << n) - 1)) as u64;
            let mut raw = (gaps[i] & low_mask(nraw)) as u64;
            let mut nraw = nraw;
            if i + LANES >= m {
                // The lane's last symbol: its low raw bits are in the state.
                let k = nraw.min(r);
                raw >>= k;
                nraw -= k;
            }
            // Forward order per symbol: state bits, then raw bits.
            scratch.push((state_bits | raw << n, n + nraw));
            *x = c.states[c.cum[t] as usize + ((*x >> n) - f) as usize] as u32;
        }
        for &x in &x[..m.min(LANES)] {
            out.push_bits((x - l) as u64, r);
        }
        for &(v, w) in scratch.iter().rev() {
            out.push_bits(v, w);
        }
    }
}

/// Mask of the low `k <= 32` bits.
#[inline(always)]
fn low_mask(k: u32) -> u32 {
    ((1u64 << k) - 1) as u32
}

/// `x` with all bits at positions `>= (n & 0xff)` cleared (BZHI semantics:
/// only the low byte of `n` counts, and counts of 64 or more keep `x`).
///
/// With BMI2 this is inline assembly rather than `_bzhi_u64`, whose `u32`
/// count makes the compiler zero-extend the count register first: one
/// cycle more on the decoder's critical path.
#[inline(always)]
pub fn bzhi(x: u64, n: u64) -> u64 {
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    {
        let r: u64;
        // SAFETY: guarded by the `bmi2` target feature; BZHI touches only
        // registers and flags.
        unsafe {
            core::arch::asm!("bzhi {r}, {x}, {n}", r = lateout(reg) r, x = in(reg) x, n = in(reg) n,
                options(pure, nomem, nostack));
        }
        r
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    {
        let n = n & 0xff;
        if n >= 64 {
            x
        } else {
            x & ((1u64 << n) - 1)
        }
    }
}

/// Bytes a decoder may read past the end of a stream.
pub const PADDING: usize = 16;

/// Little-endian 64-bit load.
#[inline(always)]
unsafe fn load_u64(p: *const u8) -> u64 {
    u64::from_le(std::ptr::read_unaligned(p as *const u64))
}

/// Decoder state of one block: the table indices of its two lanes and a
/// bit reader (branch-free refill, after Giesen's "variant 4").
#[derive(Clone, Copy)]
pub struct Decoder {
    table: *const u64,
    /// Index into the table (context · L + state − L) of the lane that
    /// decodes the next symbol...
    idx: u64,
    /// ...and of the other lane.
    idx_next: u64,
    /// The low 6 bits count the valid bits in `buf` (the bits above are
    /// garbage: subtracting whole entry fields keeps the low bits exact).
    cnt: u64,
    buf: u64,
    /// Next byte to load.
    ptr: *const u8,
    /// State bits of the model.
    r: u64,
}

impl Decoder {
    /// A decoder positioned at bit `bit` of `data`, reading the initial
    /// states of a block with `symbols >= 1` coded gaps, for a model with
    /// `r`-bit states. `data` must extend [`PADDING`] bytes past the stream.
    ///
    /// # Safety
    /// `bit` must be the start of a block stream encoded with the model of
    /// `table`, and `data` must be padded.
    #[inline(always)]
    pub unsafe fn new(table: &[u64], data: *const u8, bit: usize, start_ctx: u32, symbols: usize, r: u32) -> Self {
        let r = r as u64;
        let mut d =
            Self { table: table.as_ptr(), idx: 0, idx_next: 0, cnt: 0, buf: 0, ptr: data.add(bit >> 3), r };
        d.refill();
        d.consume((bit & 7) as u64);
        d.refill();
        let base = (start_ctx as u64) << r;
        d.idx = base + bzhi(d.buf, r);
        d.consume(r);
        if symbols > 1 {
            d.idx_next = base + bzhi(d.buf, r);
            d.consume(r);
        }
        d
    }

    /// An inert decoder (never used to decode).
    pub fn empty() -> Self {
        Self { table: std::ptr::null(), idx: 0, idx_next: 0, cnt: 0, buf: 0, ptr: std::ptr::null(), r: 0 }
    }

    /// Top up the bit buffer to at least 56 bits.
    #[inline(always)]
    pub unsafe fn refill(&mut self) {
        self.buf |= load_u64(self.ptr) << (self.cnt & 63);
        self.ptr = self.ptr.add(((!self.cnt & 63) >> 3) as usize);
        self.cnt |= 56;
    }

    #[inline(always)]
    fn consume(&mut self, n: u64) {
        self.buf >>= n & 63;
        self.cnt = self.cnt.wrapping_sub(n);
    }

    /// Decode the next gap.
    ///
    /// # Safety
    /// The block must have a symbol left.
    #[inline(always)]
    pub unsafe fn gap(&mut self) -> u32 {
        self.refill();
        self.gap_buffered()
    }

    /// Decode the next gap from the bits already buffered: the buffer must
    /// hold the symbol's bits (at most `r + 30`, or [`PAIR_MAX_BITS`] when
    /// the model guarantees it).
    ///
    /// # Safety
    /// The block must have a symbol left.
    #[inline(always)]
    pub unsafe fn gap_buffered(&mut self) -> u32 {
        let e = *self.table.add(self.idx as usize);
        let v = self.buf;
        // The lanes alternate: this lane's next index is used two symbols on.
        self.idx = self.idx_next;
        self.idx_next = (e >> 48) | bzhi(v, e);
        let t = e >> 8;
        self.consume(t);
        // bzhi(v, tot) holds the state bits (low nb bits) and the raw bits.
        ((e >> 16) as u32) | (bzhi(v, t) >> (e & 63)) as u32
    }

    /// Decode the next gap, which is the last symbol of its lane (one of
    /// the block's final [`LANES`] symbols): its low raw bits come from the
    /// lane's final state.
    ///
    /// # Safety
    /// The next symbol must be the last of its lane.
    #[inline(always)]
    pub unsafe fn gap_last(&mut self) -> u32 {
        self.refill();
        let e = *self.table.add(self.idx as usize);
        let v = self.buf;
        let nb = e & 0xff;
        let nraw = ((e >> 8) & 0xff) - nb;
        let state = bzhi((e >> 48) | bzhi(v, e), self.r);
        self.idx = self.idx_next;
        let k = nraw.min(self.r);
        let low = state & ((1u64 << k) - 1);
        let high = bzhi(v >> nb, nraw - k);
        self.consume(nb + nraw - k);
        ((e >> 16) as u32) | ((high << k) | low) as u32
    }

    /// Decode the block's final `out.len()` (at most [`LANES`]) gaps,
    /// writing the running docID after `prev`; returns the last docID.
    ///
    /// # Safety
    /// Exactly `out.len()` symbols, the lanes' last ones, must remain.
    #[inline(always)]
    pub unsafe fn finish(&mut self, prev: u32, out: &mut [u32]) -> u32 {
        let mut p = prev;
        for x in out.iter_mut() {
            p = p.wrapping_add(self.gap_last()).wrapping_add(1);
            *x = p;
        }
        p
    }

    /// Decode all `out.len()` gaps of a block (the decoder must be at its
    /// start), writing the running docID after `prev`.
    ///
    /// # Safety
    /// The block must have exactly `out.len()` symbols.
    #[inline(always)]
    pub unsafe fn block<const PAIR: bool>(&mut self, prev: u32, out: &mut [u32]) -> u32 {
        let (body, tail) = out.split_at_mut(out.len().saturating_sub(LANES));
        let p = self.run::<PAIR>(prev, body);
        self.finish(p, tail)
    }

    /// Decode `out.len()` gaps, none of them a lane's last symbol, writing
    /// the running docID (starting after `prev`) to `out`; returns the last
    /// docID. With `PAIR`, one refill serves two symbols, which requires
    /// every symbol of the model to take at most [`PAIR_MAX_BITS`] bits.
    ///
    /// # Safety
    /// The block must have more than `out.len() + LANES - 1` symbols left.
    #[inline(always)]
    pub unsafe fn run<const PAIR: bool>(&mut self, prev: u32, out: &mut [u32]) -> u32 {
        // Work on a copy: the compiler cannot always tell that `out` does
        // not alias the decoder, and would keep the state in memory.
        let mut d = *self;
        let mut p = prev;
        if PAIR {
            let mut pairs = out.chunks_exact_mut(2);
            for c in &mut pairs {
                d.refill();
                p = p.wrapping_add(d.gap_buffered()).wrapping_add(1);
                c[0] = p;
                p = p.wrapping_add(d.gap_buffered()).wrapping_add(1);
                c[1] = p;
            }
            for x in pairs.into_remainder() {
                p = p.wrapping_add(d.gap()).wrapping_add(1);
                *x = p;
            }
        } else {
            for x in out.iter_mut() {
                p = p.wrapping_add(d.gap()).wrapping_add(1);
                *x = p;
            }
        }
        *self = d;
        p
    }
}

/// Two whole blocks of equal length, interleaved so that the decoders'
/// four lanes overlap. See [`Decoder::block`].
///
/// # Safety
/// As for [`Decoder::block`], for both.
#[inline(always)]
pub unsafe fn run2<const PAIR: bool>(
    dec0: &mut Decoder,
    dec1: &mut Decoder,
    prev0: u32,
    prev1: u32,
    out0: &mut [u32],
    out1: &mut [u32],
) {
    debug_assert_eq!(out0.len(), out1.len());
    let body = out0.len().saturating_sub(LANES);
    let (out0, tail0) = out0.split_at_mut(body);
    let (out1, tail1) = out1.split_at_mut(body);
    // Local copies, as in `Decoder::run`.
    let (mut l0, mut l1) = (*dec0, *dec1);
    let (d0, d1) = (&mut l0, &mut l1);
    let (mut p0, mut p1) = (prev0, prev1);
    if PAIR {
        let n = out0.len() & !1;
        for i in (0..n).step_by(2) {
            d0.refill();
            d1.refill();
            p0 = p0.wrapping_add(d0.gap_buffered()).wrapping_add(1);
            p1 = p1.wrapping_add(d1.gap_buffered()).wrapping_add(1);
            *out0.get_unchecked_mut(i) = p0;
            *out1.get_unchecked_mut(i) = p1;
            p0 = p0.wrapping_add(d0.gap_buffered()).wrapping_add(1);
            p1 = p1.wrapping_add(d1.gap_buffered()).wrapping_add(1);
            *out0.get_unchecked_mut(i + 1) = p0;
            *out1.get_unchecked_mut(i + 1) = p1;
        }
        if n < out0.len() {
            p0 = p0.wrapping_add(d0.gap()).wrapping_add(1);
            p1 = p1.wrapping_add(d1.gap()).wrapping_add(1);
            *out0.get_unchecked_mut(n) = p0;
            *out1.get_unchecked_mut(n) = p1;
        }
    } else {
        for (x0, x1) in out0.iter_mut().zip(out1.iter_mut()) {
            p0 = p0.wrapping_add(d0.gap()).wrapping_add(1);
            p1 = p1.wrapping_add(d1.gap()).wrapping_add(1);
            *x0 = p0;
            *x1 = p1;
        }
    }
    d0.finish(p0, tail0);
    d1.finish(p1, tail1);
    *dec0 = l0;
    *dec1 = l1;
}

/// Blocks decoded at once by [`run8`].
pub const SIMD_BLOCKS: usize = 8;

/// Decode eight whole blocks of `out.len()` gaps at once: lane `j` of an
/// AVX-512 register runs block `j`'s decoder, the table lookups and the
/// bit-buffer refills are gathers (the lanes' final symbols are finished
/// one block at a time). Row `s` of `out` receives the `s`-th docID of
/// every block (a transposed layout). All eight decoders must use the same
/// table and read from `data`. With `PAIR`, one refill serves two symbols,
/// as in [`Decoder::run`].
///
/// # Safety
/// Every block must have exactly `out.len()` symbols, and `data` must be
/// the padded stream all decoders read.
#[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
#[inline(never)]
pub unsafe fn run8<const PAIR: bool>(decs: &mut [Decoder; SIMD_BLOCKS], prev: &[u32; SIMD_BLOCKS], data: *const u8, out: &mut [[u32; SIMD_BLOCKS]]) {
    use core::arch::x86_64::*;
    let table = decs[0].table;
    let load = |f: &dyn Fn(&Decoder) -> u64| -> __m512i {
        let a: [u64; SIMD_BLOCKS] = std::array::from_fn(|j| f(&decs[j]));
        _mm512_loadu_si512(a.as_ptr() as *const _)
    };
    let mut idx = load(&|d| d.idx);
    let mut idx_next = load(&|d| d.idx_next);
    let mut buf = load(&|d| d.buf);
    // Exact bit counts (the scalar decoder keeps garbage above bit 5).
    let mut cnt = load(&|d| d.cnt & 63);
    // Byte positions relative to `data`.
    let mut pos = load(&|d| d.ptr.offset_from(data) as u64);
    let mut cur = {
        let a: [u64; SIMD_BLOCKS] = std::array::from_fn(|j| prev[j] as u64);
        _mm512_loadu_si512(a.as_ptr() as *const _)
    };
    let ones = _mm512_set1_epi64(-1);
    let m8 = _mm512_set1_epi64(0xff);
    let c63 = _mm512_set1_epi64(63);
    let c56 = _mm512_set1_epi64(56);
    let one = _mm512_set1_epi64(1);
    macro_rules! refill {
        () => {
            let w = _mm512_i64gather_epi64::<1>(pos, data as _);
            buf = _mm512_or_si512(buf, _mm512_sllv_epi64(w, cnt));
            pos = _mm512_add_epi64(pos, _mm512_srli_epi64::<3>(_mm512_sub_epi64(c63, cnt)));
            cnt = _mm512_or_si512(cnt, c56);
        };
    }
    macro_rules! step {
        ($row:expr) => {
            let e = _mm512_i64gather_epi64::<8>(idx, table as _);
            let v = buf;
            let nb = _mm512_and_si512(e, m8);
            let t = _mm512_and_si512(_mm512_srli_epi64::<8>(e), m8);
            let state_bits = _mm512_andnot_si512(_mm512_sllv_epi64(ones, nb), v);
            idx = idx_next;
            idx_next = _mm512_or_si512(_mm512_srli_epi64::<48>(e), state_bits);
            let raw = _mm512_srlv_epi64(_mm512_andnot_si512(_mm512_sllv_epi64(ones, t), v), nb);
            // The bits above 32 are garbage; they vanish in the narrowing.
            let g = _mm512_or_si512(_mm512_srli_epi64::<16>(e), raw);
            cur = _mm512_add_epi64(cur, _mm512_add_epi64(g, one));
            buf = _mm512_srlv_epi64(v, t);
            cnt = _mm512_sub_epi64(cnt, t);
            _mm256_storeu_si256($row.as_mut_ptr() as *mut __m256i, _mm512_cvtepi64_epi32(cur));
        };
    }
    let body = out.len().saturating_sub(LANES);
    let (out, tail) = out.split_at_mut(body);
    let mut rows = out.iter_mut();
    if PAIR {
        while rows.len() >= 2 {
            refill!();
            let r0 = rows.next().unwrap();
            step!(r0);
            let r1 = rows.next().unwrap();
            step!(r1);
        }
    }
    for r in rows {
        refill!();
        step!(r);
    }
    let store = |v: __m512i| -> [u64; SIMD_BLOCKS] {
        let mut a = [0u64; SIMD_BLOCKS];
        _mm512_storeu_si512(a.as_mut_ptr() as *mut _, v);
        a
    };
    let (idx, idx_next, buf, cnt, pos) = (store(idx), store(idx_next), store(buf), store(cnt), store(pos));
    let cur = store(cur);
    for (j, d) in decs.iter_mut().enumerate() {
        d.idx = idx[j];
        d.idx_next = idx_next[j];
        d.buf = buf[j];
        d.cnt = cnt[j];
        d.ptr = data.add(pos[j] as usize);
        let mut p = cur[j] as u32;
        for row in tail.iter_mut() {
            p = p.wrapping_add(d.gap_last()).wrapping_add(1);
            row[j] = p;
        }
    }
}

/// Largest symbol (state bits plus raw bits) for which two symbols fit in
/// one refill of at least 56 bits.
pub const PAIR_MAX_BITS: u32 = 28;

#[cfg(test)]
mod tests {
    use super::super::model::{fit_own, SharedParams, Stats, L_MAX, MAX_CTX, R_MAX, R_MIN};
    use super::*;
    use crate::bits::BitVec;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn roundtrip(blocks: &[Vec<u32>], model: &Model) {
        let enc = Encoder::new(model);
        let table = decode_table(model);
        let mut bv = BitVec::new();
        let mut starts = Vec::new();
        let mut scratch = Vec::new();
        for b in blocks {
            starts.push(bv.len());
            enc.encode_block(b, &mut bv, &mut scratch);
        }
        let mut bytes: Vec<u8> = bv.words().iter().flat_map(|w| w.to_le_bytes()).collect();
        bytes.resize(bytes.len() + PADDING, 0);
        for (b, &s) in blocks.iter().zip(&starts) {
            // Gap by gap, with the lanes' last symbols special.
            let mut d = unsafe { Decoder::new(&table, bytes.as_ptr(), s, model.start_ctx as u32, b.len(), model.r) };
            for (i, &g) in b.iter().enumerate() {
                let got = unsafe { if i + LANES >= b.len() { d.gap_last() } else { d.gap() } };
                assert_eq!(got, g, "symbol {i}");
            }
            // Whole block, as docIDs.
            let docs: Vec<u32> = b
                .iter()
                .scan(7u32, |p, &g| {
                    *p = p.wrapping_add(g).wrapping_add(1);
                    Some(*p)
                })
                .collect();
            let mut out = vec![0u32; b.len()];
            let mut d = unsafe { Decoder::new(&table, bytes.as_ptr(), s, model.start_ctx as u32, b.len(), model.r) };
            unsafe { d.block::<false>(7, &mut out) };
            assert_eq!(out, docs);
            if b.iter().all(|&g| super::super::model::token(g).1 + model.r <= PAIR_MAX_BITS) {
                let mut d = unsafe { Decoder::new(&table, bytes.as_ptr(), s, model.start_ctx as u32, b.len(), model.r) };
                unsafe { d.block::<true>(7, &mut out) };
                assert_eq!(out, docs);
            }
        }
    }

    /// Own models fitted to `blocks`, with every state size.
    fn fitted(blocks: &[Vec<u32>]) -> Vec<Model> {
        let mut s = Stats::new();
        for b in blocks {
            let toks: Vec<u8> = b.iter().map(|&g| super::super::model::token(g).0 as u8).collect();
            s.add_block(&toks);
        }
        let own = fit_own(&s).0;
        (R_MIN..=R_MAX).map(|r| super::super::model::OwnModel { r, ..own.clone() }.model()).collect()
    }

    #[test]
    fn random_streams_roundtrip() {
        let mut rng = StdRng::seed_from_u64(9);
        for trial in 0..40 {
            let nblocks = rng.gen_range(1..20);
            let blocks: Vec<Vec<u32>> = (0..nblocks)
                .map(|_| {
                    let n = rng.gen_range(1..300);
                    (0..n)
                        .map(|_| match trial % 4 {
                            0 => rng.gen_range(0..4),
                            1 => rng.gen::<u32>() >> rng.gen_range(0..32),
                            2 => {
                                if rng.gen_bool(0.7) {
                                    0
                                } else {
                                    let w = rng.gen_range(1..32);
                                    rng.gen_range(0..1u32 << w)
                                }
                            }
                            _ => rng.gen_range(0..u32::MAX),
                        })
                        .collect()
                })
                .collect();
            for m in fitted(&blocks) {
                roundtrip(&blocks, &m);
            }
        }
    }

    #[test]
    fn adversarial_streams_roundtrip() {
        // Long runs, huge gaps, a single symbol, all tokens in every context.
        let all_tokens: Vec<u32> = (0..NTOK).map(|t| BASE[t]).collect();
        let mut every_pair = Vec::new();
        for a in 0..NTOK {
            for b in 0..NTOK {
                every_pair.push(super::super::model::token_max(a));
                every_pair.push(BASE[b]);
            }
        }
        let cases: Vec<Vec<Vec<u32>>> = vec![
            vec![vec![0; 5000]],
            vec![vec![u32::MAX - 1; 300]],
            vec![vec![7]],
            vec![all_tokens.clone(), all_tokens.iter().rev().copied().collect()],
            vec![every_pair.clone()],
            vec![(0..2000).map(|i| if i % 97 == 0 { u32::MAX - 1 } else { 0 }).collect()],
        ];
        for blocks in &cases {
            for m in fitted(blocks) {
                roundtrip(blocks, &m);
            }
        }
        // Shared models code every token in every context.
        for (max_tok, r) in [(20u8, R_MIN), (45, 9), (63, R_MAX)] {
            for (mean, pi) in [(0u8, [7u8, 0u8]), (20, [3, 5]), (63, [0, 7])] {
                let m = SharedParams { mean, pi, max_tok, r: r as u8 }.model();
                let toks: Vec<u32> = every_pair
                    .iter()
                    .copied()
                    .filter(|&g| super::super::model::token(g).0 <= max_tok as usize)
                    .collect();
                roundtrip(&[toks.clone(), vec![0; 3000], toks], &m);
            }
        }
        assert!(MAX_CTX * L_MAX <= 1 << 16, "table index must fit the entry layout");
    }
}
