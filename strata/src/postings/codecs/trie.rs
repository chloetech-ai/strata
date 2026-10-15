//! Compact binary tries (Arroyuelo & Castillo, "Trie-Compressed Adaptive Set
//! Intersection", CPM 2023; arXiv 2212.00946).
//!
//! A set over `[0, universe)` is the binary trie of height
//! `h = max(1, bit_width(universe - 1))` that spells every docID most
//! significant bit first. It is stored level by level: each node at depth
//! `l < h` contributes two bits, "left child present" and "right child
//! present", in left-to-right order, and the bits of each level form a
//! [`RankSelect`]. Navigation needs rank only: the node whose bits start at
//! `2i` has its left child at index `rank1(2i)` of the next level and its right
//! child at `rank1(2i + 1)`. Leaves (depth `h`) are implicit. The space, about
//! two bits per trie edge plus the 6.25% rank directory, follows the trie
//! measure: docIDs that are close share their upper paths.
//!
//! [`RunTrie`] is the paper's run-compressed variant (rTrie). A subtree that
//! holds every leaf of its range is cut, and its root is encoded as `00`, a
//! code no node of a plain trie can have. Runs of consecutive docIDs then cost
//! O(log run) nodes instead of O(run).
//!
//! Set operations use Trabb Pardo's synchronised traversal of the k tries. At
//! each node the 2-bit codes are ANDed (intersection) or ORed (union), and only
//! the surviving children are visited. A subtree missing from any input is
//! pruned on the spot, which makes intersection adaptive: O(kδ lg(u/δ)) for
//! alternation δ. With run compression, a full subtree is the identity for
//! intersection (that input drops out below it) and absorbing for union (its
//! whole range is in the result). In a union, an input drops out of every
//! subtree it lacks.
//!
//! Implementation notes:
//! * Traversal order. The synchronised traversal visits exactly the nodes of
//!   the paper's recursive one, but level by level (breadth first) instead of
//!   depth first. Each node's children are appended to the next level's list
//!   without branching on which ones exist. A DFS mispredicts on the irregular
//!   shape of the tries, and in this testbed it was 20-35% slower. To keep the
//!   buffers small and cache resident, the traversal runs separately below each
//!   node of level `a - BATCH`. Each run is still in docID order, so the
//!   output needs no sorting.
//! * Leaf words. The bottom [`LEAF_BITS`] = 6 levels under a node are expanded
//!   into a 64-bit leaf bitmap with PDEP. In level order, the descendants of a
//!   node on any deeper level are contiguous. So each level contributes a single
//!   read of at most 64 bits, deposited under the doubled occupancy mask of the
//!   level above (bit `s` becomes bits `2s` and `2s + 1`). On aarch64, NEON
//!   emulates PDEP for masks of more than 8 set bits with byte-lane table
//!   lookups (smaller masks take the portable loop) and doubles the mask with
//!   a carry-less square. Leaf words are then ANDed or ORed whole and emitted
//!   with VPCOMPRESSD (AVX-512), byte-table lookups (NEON) or trailing-zero
//!   counts.
//! * Ranks. Each visited node costs at most one rank per input, because the
//!   right child's index is the left's plus the code's low bit. A traversal
//!   reaches each level's positions in increasing order, so each (input,
//!   level) pair remembers the rank just past the last node or range it
//!   consumed. The adjacent node then needs no rank, and a node less than 64
//!   bits further on needs one popcount. Anything else goes to the rank
//!   directory: a sample plus one masked VPOPCNTQ under AVX-512 (a masked
//!   `vcntq_u8` under NEON).
//! * `decode_into` visits every node, so it needs no rank at all: node indices
//!   are counters, one per level.
//! * The cursor keeps the root-to-leaf path. `next_geq` climbs to the level
//!   where the target leaves that path, then descends along the target's bits,
//!   backtracking to the next subtree to the right when a branch is missing.

use super::{Cursor, CursorList, PostingList, SetOps};
use crate::postings::bits::{bit_width, BitVec, RankSelect};
use crate::postings::END;

/// Number of bottom levels expanded into one leaf word (2^6 = 64 leaves).
const LEAF_BITS: u32 = 6;
/// Maximum trie height (docIDs are `u32`).
const MAX_H: usize = 32;
/// Marks a prefix inside a cut subtree during breadth-first decoding.
const FULL_BIT: u32 = 1 << 31;
const EVEN: u64 = 0x5555_5555_5555_5555;
/// Build-time flag: the node's subtree contains every leaf of its range.
const FULL: u8 = 4;

/// Trie height for `universe`: `max(1, bit_width(universe - 1))`.
fn height(universe: u32) -> u32 {
    bit_width(universe.saturating_sub(1) as u64).max(1)
}

/// Shared representation of [`Trie`] and [`RunTrie`].
#[derive(Clone, Debug, Default)]
struct Core {
    /// `levels[l]` holds the 2-bit codes of the nodes at depth `l`; empty for
    /// an empty set.
    levels: Vec<RankSelect>,
    h: u32,
    /// Leaf-word height: `min(h, LEAF_BITS)`.
    lb: u32,
    n: usize,
}

impl Core {
    /// Builds bottom-up in O(total nodes) time, grouping each level's sorted
    /// node prefixes under their parents in place. With `run`, a node is
    /// emitted only when its parent is not full, and a full node as `00`.
    fn build(docs: &[u32], universe: u32, run: bool, leaf_bits: u32) -> Self {
        let h = height(universe);
        let lb = h.min(leaf_bits);
        let n = docs.len();
        if n == 0 {
            return Self { levels: Vec::new(), h, lb, n };
        }
        debug_assert!(docs.windows(2).all(|w| w[0] < w[1]), "docIDs must increase");
        debug_assert!((docs[n - 1] as u64) < (1u64 << h), "docID outside the universe");

        // Nodes of the current level, left to right: prefix and flags
        // (child bits in bits 0..2, plus FULL).
        let mut pre: Vec<u32> = Vec::with_capacity(n);
        let mut flg: Vec<u8> = Vec::with_capacity(n);
        for &d in docs {
            let (p, bit) = (d >> 1, 1u8 << (d & 1));
            if pre.last() == Some(&p) {
                *flg.last_mut().unwrap() |= bit;
            } else {
                pre.push(p);
                flg.push(bit);
            }
        }
        if run {
            for f in flg.iter_mut().filter(|f| **f == 3) {
                *f |= FULL;
            }
        }

        let mut rev: Vec<BitVec> = Vec::with_capacity(h as usize);
        for _ in 1..h {
            let mut bv = BitVec::with_capacity(2 * pre.len());
            let (mut i, mut j) = (0, 0);
            while i < pre.len() {
                let p = pre[i] >> 1;
                let sib = i + 1 < pre.len() && pre[i + 1] >> 1 == p;
                let end = i + 1 + sib as usize;
                let full = run && sib && flg[i] & flg[i + 1] & FULL != 0;
                let mut code = 0u8;
                for c in i..end {
                    code |= 1 << (pre[c] & 1);
                    if !full {
                        let own = if flg[c] & FULL != 0 { 0 } else { flg[c] & 3 };
                        bv.push_bits(own as u64, 2);
                    }
                }
                pre[j] = p;
                flg[j] = code | if full { FULL } else { 0 };
                i = end;
                j += 1;
            }
            pre.truncate(j);
            flg.truncate(j);
            rev.push(bv);
        }
        debug_assert_eq!(pre, [0]);
        let mut root = BitVec::new();
        root.push_bits(if flg[0] & FULL != 0 { 0 } else { (flg[0] & 3) as u64 }, 2);
        rev.push(root);

        let levels = rev.into_iter().rev().map(|bv| RankSelect::new(bv, false, false)).collect();
        Self { levels, h, lb, n }
    }

    fn size_bits(&self) -> u64 {
        self.levels.iter().map(RankSelect::size_bits).sum()
    }

    fn decode_into<const RUN: bool>(&self, out: &mut Vec<u32>) {
        if self.n == 0 {
            return;
        }
        out.reserve(self.n);
        let (h, lb) = (self.h as usize, self.lb as usize);
        let a = h - lb;
        let v = View::new(self);
        // Breadth first down to the leaf-word level: every node of a level is
        // visited in order, so node indices are plain counters and children
        // are appended without branches. Prefixes carry FULL_BIT inside cut
        // subtrees (only with RUN).
        let mut cur: Vec<u32> = vec![0];
        let mut nxt: Vec<u32> = Vec::new();
        for l in 0..a {
            nxt.clear();
            nxt.resize(2 * cur.len() + 1, 0);
            let (mut i, mut k) = (0, 0);
            for &p in &cur {
                let q = (p & !FULL_BIT) << 1;
                let code = if RUN && p & FULL_BIT != 0 {
                    0
                } else {
                    i += 1;
                    v.code(l, i - 1)
                };
                if RUN && code == 0 {
                    let f = q | FULL_BIT;
                    nxt[k] = f;
                    nxt[k + 1] = f | 1;
                    k += 2;
                } else {
                    nxt[k] = q;
                    k += (code & 1) as usize;
                    nxt[k] = q | 1;
                    k += (code >> 1) as usize;
                }
            }
            nxt.truncate(k);
            std::mem::swap(&mut cur, &mut nxt);
        }
        let mut next = [0usize; MAX_H];
        let mut i = 0;
        for &p in &cur {
            if RUN && p & FULL_BIT != 0 {
                emit_word(out, (p & !FULL_BIT) << lb, !0u64 >> (64 - (1 << lb)));
            } else {
                let w = v.leaf_word_seq::<RUN>(&mut next, a, h, i);
                i += 1;
                emit_word(out, p << lb, w);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bit tricks.

#[inline(always)]
fn pdep(src: u64, mask: u64) -> u64 {
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    {
        // SAFETY: guarded by the `bmi2` target feature.
        unsafe { core::arch::x86_64::_pdep_u64(src, mask) }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if mask.count_ones() <= PDEP_LOOP_MAX {
            pdep_portable(src, mask)
        } else {
            pdep_neon(src, mask)
        }
    }
    #[cfg(not(any(all(target_arch = "x86_64", target_feature = "bmi2"), target_arch = "aarch64")))]
    {
        pdep_portable(src, mask)
    }
}

/// On aarch64, masks with at most this many set bits take the portable loop
/// (about 3 cycles per set bit) rather than [`pdep_neon`] (about 35 cycles
/// of latency whatever the mask). Small masks are the upper levels of a leaf
/// word and sparse tries; on the M2, Trie decoding and set operations were
/// fastest with 8 (4 to 16 tried).
#[cfg(target_arch = "aarch64")]
const PDEP_LOOP_MAX: u32 = 8;

/// Byte lane `l` of `REP8[g]` is `2g + l / 8`: a `vqtbl1q_u8` of a `u64`
/// splat by it spreads bytes `2g` and `2g + 1` over 8 lanes each.
#[cfg(target_arch = "aarch64")]
static REP8: [[u8; 16]; 4] = {
    let mut t = [[0u8; 16]; 4];
    let mut g = 0;
    while g < 4 {
        let mut l = 0;
        while l < 16 {
            t[g][l] = (2 * g + l / 8) as u8;
            l += 1;
        }
        g += 1;
    }
    t
};
/// Per byte lane `l`: bit `l % 8`, and the bits below it.
#[cfg(target_arch = "aarch64")]
static LANE_BIT8: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
#[cfg(target_arch = "aarch64")]
static LANE_BELOW8: [u8; 16] = [0, 1, 3, 7, 15, 31, 63, 127, 0, 1, 3, 7, 15, 31, 63, 127];

/// PDEP with one byte lane per bit position `i` (four vectors): lane `i`
/// looks up bit `popcount(mask mod 2^i)` of `src`, spread to 64 lanes, with
/// one `vqtbl4q_u8`, and keeps it where `mask` has bit `i`. The rank is the
/// lane's byte's `vcntq_u8` below `i` plus the prefix popcount of the bytes
/// before; pairwise adds of the lanes weighted by bit fold them back.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn pdep_neon(src: u64, mask: u64) -> u64 {
    use core::arch::aarch64::*;
    // SAFETY: NEON is part of the aarch64 baseline; the loads read whole
    // statics.
    unsafe {
        let (bit, below) = (vld1q_u8(LANE_BIT8.as_ptr()), vld1q_u8(LANE_BELOW8.as_ptr()));
        let rep = |g: usize| vld1q_u8(REP8[g].as_ptr());
        let (vm, vs) = (vreinterpretq_u8_u64(vdupq_n_u64(mask)), vreinterpretq_u8_u64(vdupq_n_u64(src)));
        let cnt = vget_lane_u64::<0>(vreinterpret_u64_u8(vcnt_u8(vcreate_u8(mask))));
        let vo = vreinterpretq_u8_u64(vdupq_n_u64(cnt.wrapping_mul(0x0101_0101_0101_0101) << 8));
        let s = uint8x16x4_t(
            vtstq_u8(vqtbl1q_u8(vs, rep(0)), bit),
            vtstq_u8(vqtbl1q_u8(vs, rep(1)), bit),
            vtstq_u8(vqtbl1q_u8(vs, rep(2)), bit),
            vtstq_u8(vqtbl1q_u8(vs, rep(3)), bit),
        );
        let lanes = |g: usize| {
            let m = vqtbl1q_u8(vm, rep(g));
            let rank = vaddq_u8(vcntq_u8(vandq_u8(m, below)), vqtbl1q_u8(vo, rep(g)));
            vandq_u8(vqtbl4q_u8(s, rank), vandq_u8(m, bit))
        };
        let p = vpaddq_u8(vpaddq_u8(lanes(0), lanes(1)), vpaddq_u8(lanes(2), lanes(3)));
        vgetq_lane_u64::<0>(vreinterpretq_u64_u8(vpaddq_u8(p, p)))
    }
}

#[cfg_attr(any(all(target_arch = "x86_64", target_feature = "bmi2"), target_arch = "aarch64"), allow(dead_code))]
#[inline]
fn pdep_portable(mut src: u64, mut mask: u64) -> u64 {
    let mut out = 0;
    while mask != 0 {
        let low = mask & mask.wrapping_neg();
        if src & 1 != 0 {
            out |= low;
        }
        src >>= 1;
        mask ^= low;
    }
    out
}

/// Spread the low 32 bits of `x` to the even bit positions.
#[cfg_attr(any(all(target_arch = "x86_64", target_feature = "bmi2"), target_arch = "aarch64"), allow(dead_code))]
#[inline]
fn spread_portable(x: u64) -> u64 {
    let mut x = x & 0xFFFF_FFFF;
    x = (x | x << 16) & 0x0000_FFFF_0000_FFFF;
    x = (x | x << 8) & 0x00FF_00FF_00FF_00FF;
    x = (x | x << 4) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | x << 2) & 0x3333_3333_3333_3333;
    (x | x << 1) & EVEN
}

/// Spread the low 32 bits of `x` to the even bit positions with a
/// carry-less square: `vmull_p8` squares each byte, moving bit `b` to `2b`.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn spread_neon(x: u64) -> u64 {
    use core::arch::aarch64::*;
    // SAFETY: NEON is part of the aarch64 baseline.
    unsafe {
        let v = vcreate_p8(x);
        vgetq_lane_u64::<0>(vreinterpretq_u64_p16(vmull_p8(v, v)))
    }
}

/// Each bit `s` of `x` (< 2^32) becomes bits `2s` and `2s + 1`.
#[inline(always)]
fn dup(x: u64) -> u64 {
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    let s = pdep(x, EVEN);
    #[cfg(target_arch = "aarch64")]
    let s = spread_neon(x);
    #[cfg(not(any(all(target_arch = "x86_64", target_feature = "bmi2"), target_arch = "aarch64")))]
    let s = spread_portable(x);
    s | s << 1
}

/// One level of leaf-word expansion. `occ` marks the nodes of the current
/// depth that have codes on this level (`codes`, concatenated left to right)
/// and `full` the nodes inside cut subtrees. Returns the same pair one level
/// down: a node coded `00` is full, and so are both children of a full node.
#[inline(always)]
fn expand<const RUN: bool>(occ: u64, full: u64, codes: u64) -> (u64, u64) {
    let d = dup(occ);
    let e = pdep(codes, d);
    if RUN {
        let z = d & EVEN & !(e | e >> 1);
        (e, dup(full) | z | z << 1)
    } else {
        (e, 0)
    }
}

/// Depth-0 expansion: a node's own code is its children's occupancy (a `00`
/// code makes both children full).
#[inline(always)]
fn root_occ<const RUN: bool>(code: u32) -> (u64, u64) {
    let c = code as u64;
    (c, if RUN && c == 0 { 3 } else { 0 })
}

/// The 64 bits starting at bit `pos` (zero past the end). Codes are read with
/// it unmasked: PDEP consumes only as many low bits as its mask has ones.
#[inline(always)]
fn read64(words: &[u64], pos: usize) -> u64 {
    let (wi, off) = (pos >> 6, pos & 63);
    let lo = words.get(wi).map_or(0, |w| w >> off);
    let hi = words.get(wi + 1).map_or(0, |w| (w << 1) << (63 - off));
    lo | hi
}

/// `rank1(pos)` from the directory, for `pos <= len`. With AVX-512 this is
/// the block's sample plus one masked VPOPCNTQ over the words of the block
/// before `pos`, with no loop; with NEON, `vcntq_u8` over the block's 8
/// words, masked by byte lane.
#[inline(always)]
fn rank_dir(rs: &RankSelect, words: &[u64], pos: usize) -> usize {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vpopcntdq"))]
    {
        use core::arch::x86_64::*;
        let wi = pos >> 6;
        let blk = wi & !7; // first word of the 512-bit rank block
        let sample = rs.rank1(blk << 6); // a sample lookup, no words to scan
        // SAFETY: guarded by the target features; the mask selects only the
        // words `blk..wi`, which lie inside `words` (masked lanes never fault).
        let full = unsafe {
            let k = ((1u32 << (wi - blk)) - 1) as __mmask8;
            let v = _mm512_maskz_loadu_epi64(k, words.as_ptr().add(blk) as *const i64);
            _mm512_reduce_add_epi64(_mm512_popcnt_epi64(v)) as usize
        };
        let part = words.get(wi).map_or(0, |w| (w & ((1u64 << (pos & 63)) - 1)).count_ones() as usize);
        sample + full + part
    }
    #[cfg(target_arch = "aarch64")]
    {
        use core::arch::aarch64::*;
        let wi = pos >> 6;
        let blk = wi & !7;
        if blk + 8 > words.len() {
            return rs.rank1(pos); // the last block may be short
        }
        let sample = rs.rank1(blk << 6);
        // SAFETY: the words `blk..blk + 8` lie inside `words`; byte lanes
        // below `8 * (wi - blk)` hold the words `blk..wi`.
        let full = unsafe {
            let v = vld1q_u8_x4(words.as_ptr().add(blk) as *const u8);
            let n = vdupq_n_u8((8 * (wi - blk)) as u8);
            let c = |x: uint8x16_t, g: usize| vandq_u8(vcntq_u8(x), vcltq_u8(vld1q_u8(IOTA8[16 * g..].as_ptr()), n));
            vaddlvq_u8(vaddq_u8(vaddq_u8(c(v.0, 0), c(v.1, 1)), vaddq_u8(c(v.2, 2), c(v.3, 3)))) as usize
        };
        let part = words.get(wi).map_or(0, |w| (w & ((1u64 << (pos & 63)) - 1)).count_ones() as usize);
        sample + full + part
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vpopcntdq"),
        target_arch = "aarch64"
    )))]
    {
        let _ = words;
        rs.rank1(pos)
    }
}

/// Byte lanes 0..63.
#[cfg(target_arch = "aarch64")]
static IOTA8: [u8; 64] = {
    let mut t = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        t[i] = i as u8;
        i += 1;
    }
    t
};

/// Append `base + i` for every set bit `i` of `w`.
#[inline(always)]
fn emit_word(out: &mut Vec<u32>, base: u32, w: u64) {
    if w == 0 {
        return;
    }
    out.reserve(64);
    let len = out.len();
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f"))]
    // SAFETY: guarded by the target feature. Chunk `c` stores 16 lanes at
    // offset `popcount(w mod 2^(16c)) <= 16c`, so every store ends within the
    // 64 reserved slots, and the length counts only the compressed lanes.
    unsafe {
        use core::arch::x86_64::*;
        let mut p = out.as_mut_ptr().add(len);
        let mut v = _mm512_add_epi32(
            _mm512_set1_epi32(base as i32),
            _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15),
        );
        for c in 0..4 {
            let m = (w >> (16 * c)) as u16;
            _mm512_storeu_si512(p as *mut __m512i, _mm512_maskz_compress_epi32(m, v));
            p = p.add(m.count_ones() as usize);
            v = _mm512_add_epi32(v, _mm512_set1_epi32(16));
        }
        out.set_len(len + w.count_ones() as usize);
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: both paths write at most 64 slots, all reserved, and the
    // length counts only the set bits.
    unsafe {
        use super::roaring::simd::{byte_prefix_counts, bytes_neon_u32};
        let p = out.as_mut_ptr().add(len);
        let pre = byte_prefix_counts(w);
        if pre >> 56 > 4 {
            bytes_neon_u32(w, pre, base, p);
        } else {
            // At most 4 set bits: 4 unconditional writes.
            let mut w = w;
            for i in 0..4 {
                p.add(i).write(base.wrapping_add(w.trailing_zeros()));
                w &= w.wrapping_sub(1);
            }
        }
        out.set_len(len + (pre >> 56) as usize);
    }
    #[cfg(not(any(all(target_arch = "x86_64", target_feature = "avx512f"), target_arch = "aarch64")))]
    {
        let spare = out.spare_capacity_mut();
        let (mut w, mut i) = (w, 0);
        while w != 0 {
            spare[i].write(base + w.trailing_zeros());
            w &= w - 1;
            i += 1;
        }
        // SAFETY: the first `i` spare slots were initialised just above.
        unsafe { out.set_len(len + i) };
    }
}

// ---------------------------------------------------------------------------
// Navigation.

/// A trie being traversed: per-level word slices, plus one remembered rank
/// per level. `hit[l] = (pos, rank1(pos))` is set at the end of the last node
/// or range consumed on level `l`. A traversal that moves on to the adjacent
/// node or range (the common case on dense data) therefore needs no rank.
struct View<'a> {
    rs: &'a [RankSelect],
    words: [&'a [u64]; MAX_H],
    hit: [(usize, usize); MAX_H],
}

impl<'a> View<'a> {
    fn new(core: &'a Core) -> Self {
        let mut words: [&[u64]; MAX_H] = [&[]; MAX_H];
        for (w, l) in words.iter_mut().zip(&core.levels) {
            *w = l.bits().words();
        }
        Self { rs: &core.levels, words, hit: [(0, 0); MAX_H] }
    }

    /// 2-bit code of node `i` on `level` (bit 0: left child, bit 1: right).
    #[inline(always)]
    fn code(&self, level: usize, i: usize) -> u32 {
        ((self.words[level][i >> 5] >> ((i & 31) << 1)) & 3) as u32
    }

    /// `rank1(pos)` on `level`: free at the remembered position, one popcount
    /// less than 64 bits past it, else the directory.
    #[inline(always)]
    fn rank(&self, level: usize, pos: usize) -> usize {
        let (p, r) = self.hit[level];
        if p == pos {
            return r;
        }
        let d = pos.wrapping_sub(p);
        if d < 64 {
            r + (read64(self.words[level], p) & ((1u64 << d) - 1)).count_ones() as usize
        } else {
            rank_dir(&self.rs[level], self.words[level], pos)
        }
    }

    /// Visit node `i` of `level` with code `code`: returns its first child's
    /// index and remembers the rank just past the node.
    #[inline(always)]
    fn first_child(&mut self, level: usize, i: usize, code: u32) -> usize {
        let r = self.rank(level, 2 * i);
        self.hit[level] = (2 * i + 2, r + code.count_ones() as usize);
        r
    }

    /// Leaf bitmap of node `i` on the leaf-word level `a`: one read, one
    /// expansion and (unless remembered) one rank per level.
    #[inline(always)]
    fn leaf_word<const RUN: bool>(&mut self, a: usize, h: usize, i: usize) -> u64 {
        let (mut occ, mut full) = root_occ::<RUN>(self.code(a, i));
        // `cnt` nodes starting at bit `pos` of level `j - 1` have the nodes
        // of `occ` as children.
        let (mut pos, mut cnt) = (2 * i, 1);
        for j in a + 1..h {
            let ones = occ.count_ones() as usize;
            if !RUN || ones != 0 {
                let r = self.rank(j - 1, pos);
                self.hit[j - 1] = (pos + 2 * cnt, r + ones);
                pos = 2 * r;
            }
            (occ, full) = expand::<RUN>(occ, full, read64(self.words[j], pos));
            cnt = ones;
        }
        occ | full
    }

    /// Leaf bitmap of node `i` on level `a` when `next[j]` is already the
    /// index of its first descendant on every level `j > a`: no ranks needed.
    #[inline(always)]
    fn leaf_word_seq<const RUN: bool>(&self, next: &mut [usize; MAX_H], a: usize, h: usize, i: usize) -> u64 {
        let (mut occ, mut full) = root_occ::<RUN>(self.code(a, i));
        for (words, nx) in self.words[a + 1..h].iter().zip(&mut next[a + 1..h]) {
            let codes = read64(words, 2 * *nx);
            *nx += occ.count_ones() as usize;
            (occ, full) = expand::<RUN>(occ, full, codes);
        }
        occ | full
    }
}

/// Common height and leaf-word height of the inputs of a set operation.
fn shape(lists: &[&Core]) -> (usize, usize) {
    let (h, lb) = (lists[0].h, lists[0].lb);
    assert!(
        lists.iter().all(|l| l.h == h && l.lb == lb),
        "trie set operations need lists built over the same universe"
    );
    (h as usize, lb as usize)
}

/// Inputs per traversal: one bit each in an entry's mask. Larger operations
/// are split into groups whose results are merged.
const MAX_K: usize = 32;
/// The traversal runs separately below each node of level `a - BATCH`, so its
/// buffers hold at most 2^BATCH entries (they stay cache resident).
const BATCH: usize = 10;

// The synchronised traversal of k tries runs breadth first, one level at a
// time, which makes it branch-light: a DFS stalls on the unpredictable shape
// of the tries. An entry is `[prefix, mask, idx_0, .., idx_{k-1}]`: a node
// with this prefix, kept by the inputs in `mask`, at index `idx_t` of its
// level in input `t`. Mask 0 marks a full subtree (run compression); its
// entries carry no indices and reach the leaf-word level as all-ones words,
// so the output stays in order. Each step writes both children of an entry
// and advances the output only past those that exist.

/// One level of intersection: children present in every active input.
/// Returns the number of `u32`s written to `nxt`.
fn step_and<const RUN: bool>(views: &mut [View], k: usize, l: usize, cur: &[u32], nxt: &mut Vec<u32>) -> usize {
    let stride = k + 2;
    if nxt.len() < 2 * cur.len() {
        nxt.resize(2 * cur.len(), 0);
    }
    let mut codes = [0u32; MAX_K];
    let mut o = 0;
    for e in cur.chunks_exact(stride) {
        let (q, mask) = (e[0] << 1, e[1]);
        let (mut and, mut m) = (3u32, mask);
        let mut bits = mask;
        while bits != 0 {
            let t = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let c = views[t].code(l, e[2 + t] as usize);
            if RUN && c == 0 {
                m &= !(1 << t); // a full subtree is the identity: the input drops out
            } else {
                and &= c;
                codes[t] = c;
            }
        }
        if and == 0 {
            continue; // missing from some input: pruned
        }
        let o1 = o + (and & 1) as usize * stride;
        nxt[o] = q;
        nxt[o + 1] = m;
        nxt[o1] = q | 1;
        nxt[o1 + 1] = m;
        let mut bits = m;
        while bits != 0 {
            let t = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let r = views[t].first_child(l, e[2 + t] as usize, codes[t]) as u32;
            nxt[o + 2 + t] = r;
            nxt[o1 + 2 + t] = r + (codes[t] & 1);
        }
        o = o1 + (and >> 1) as usize * stride;
    }
    o
}

/// One level of union: children present in any active input, each kept by
/// the inputs that have it.
fn step_or<const RUN: bool>(views: &mut [View], k: usize, l: usize, cur: &[u32], nxt: &mut Vec<u32>) -> usize {
    let stride = k + 2;
    if nxt.len() < 2 * cur.len() {
        nxt.resize(2 * cur.len(), 0);
    }
    let mut codes = [0u32; MAX_K];
    let mut o = 0;
    for e in cur.chunks_exact(stride) {
        let (q, mask) = (e[0] << 1, e[1]);
        // Inputs keeping the left (m0) and right (m1) child.
        let (mut m0, mut m1, mut full) = (0u32, 0u32, mask == 0);
        let mut bits = mask;
        while bits != 0 {
            let t = bits.trailing_zeros();
            bits &= bits - 1;
            let c = views[t as usize].code(l, e[2 + t as usize] as usize);
            codes[t as usize] = c;
            m0 |= (c & 1) << t;
            m1 |= (c >> 1) << t;
            full |= RUN && c == 0; // a full subtree absorbs the others
        }
        if RUN && full {
            nxt[o] = q;
            nxt[o + 1] = 0;
            nxt[o + stride] = q | 1;
            nxt[o + stride + 1] = 0;
            o += 2 * stride;
            continue;
        }
        let o1 = o + (m0 != 0) as usize * stride;
        nxt[o] = q;
        nxt[o + 1] = m0;
        nxt[o1] = q | 1;
        nxt[o1 + 1] = m1;
        let mut bits = mask;
        while bits != 0 {
            let t = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let r = views[t].first_child(l, e[2 + t] as usize, codes[t]) as u32;
            nxt[o + 2 + t] = r;
            nxt[o1 + 2 + t] = r + (codes[t] & 1);
        }
        o = o1 + (m1 != 0) as usize * stride;
    }
    o
}

/// Leaf words of the entries on level `a`: the AND (or OR) of the active
/// inputs' words, emitted in order.
fn leaves<const RUN: bool, const OR: bool>(views: &mut [View], k: usize, a: usize, h: usize, cur: &[u32], out: &mut Vec<u32>) {
    let lb = h - a;
    let all = !0u64 >> (64 - (1 << lb));
    for e in cur.chunks_exact(k + 2) {
        let mask = e[1];
        let mut w = if OR && mask != 0 { 0 } else { all };
        let mut bits = mask;
        while bits != 0 {
            let t = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let x = views[t].leaf_word::<RUN>(a, h, e[2 + t] as usize);
            if OR {
                w |= x;
            } else {
                w &= x;
                if w == 0 {
                    break;
                }
            }
        }
        emit_word(out, e[0] << lb, w);
    }
}

/// Synchronised traversal of `lists` (1 to MAX_K non-empty tries), appending
/// their intersection (or union) to `out`.
fn traverse<const RUN: bool, const OR: bool>(lists: &[&Core], out: &mut Vec<u32>) {
    let (h, lb) = shape(lists);
    let (a, k) = (h - lb, lists.len());
    debug_assert!((1..=MAX_K).contains(&k));
    let stride = k + 2;
    let mut views: Vec<View> = lists.iter().map(|c| View::new(c)).collect();
    let step = |views: &mut [View], l: usize, cur: &[u32], nxt: &mut Vec<u32>| {
        if OR {
            step_or::<RUN>(views, k, l, cur, nxt)
        } else {
            step_and::<RUN>(views, k, l, cur, nxt)
        }
    };
    // The root entry, then the levels above the batches.
    let mut top = vec![0u32; stride];
    top[1] = (!0u64 >> (64 - k)) as u32;
    let mut nxt = Vec::new();
    let s = a.saturating_sub(BATCH);
    for l in 0..s {
        let n = step(&mut views, l, &top, &mut nxt);
        std::mem::swap(&mut top, &mut nxt);
        top.truncate(n);
    }
    let (mut cur, mut tmp) = (Vec::new(), Vec::new());
    for e in top.chunks_exact(stride) {
        cur.clear();
        cur.extend_from_slice(e);
        for l in s..a {
            let n = step(&mut views, l, &cur, &mut tmp);
            std::mem::swap(&mut cur, &mut tmp);
            cur.truncate(n);
        }
        leaves::<RUN, OR>(&mut views, k, a, h, &cur, out);
    }
}

fn intersect<const RUN: bool>(lists: &[&Core], out: &mut Vec<u32>) {
    if lists.is_empty() || lists.iter().any(|l| l.n == 0) {
        return;
    }
    if lists.len() == 1 {
        lists[0].decode_into::<RUN>(out);
        return;
    }
    // Smallest first: its codes empty the AND soonest.
    let mut sorted = lists.to_vec();
    sorted.sort_by_key(|l| l.n);
    if sorted.len() <= MAX_K {
        traverse::<RUN, false>(&sorted, out);
        return;
    }
    let mut groups = sorted.chunks(MAX_K);
    let mut acc = Vec::new();
    traverse::<RUN, false>(groups.next().unwrap(), &mut acc);
    for g in groups {
        let mut part = Vec::new();
        traverse::<RUN, false>(g, &mut part);
        acc.retain(|x| part.binary_search(x).is_ok());
    }
    out.extend_from_slice(&acc);
}

fn union<const RUN: bool>(lists: &[&Core], out: &mut Vec<u32>) {
    let lists: Vec<&Core> = lists.iter().copied().filter(|l| l.n > 0).collect();
    match lists.len() {
        0 => {}
        1 => lists[0].decode_into::<RUN>(out),
        k if k <= MAX_K => traverse::<RUN, true>(&lists, out),
        _ => {
            let mut acc: Vec<u32> = Vec::new();
            for g in lists.chunks(MAX_K) {
                let mut part = Vec::new();
                traverse::<RUN, true>(g, &mut part);
                acc = merge_union(&acc, &part);
            }
            out.extend_from_slice(&acc);
        }
    }
}

/// Union of two sorted, duplicate-free slices.
fn merge_union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        out.push(x.min(y));
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

// ---------------------------------------------------------------------------
// Cursor.

/// Forward cursor over a [`Trie`] or [`RunTrie`]. It holds the root-to-leaf
/// path and the current block: a leaf word, or (for [`RunTrie`]) the range of
/// a full subtree.
pub struct TrieCursor<'a, const RUN: bool> {
    v: View<'a>,
    h: usize,
    a: usize,
    lb: usize,
    n: usize,
    /// Per path level: the node's code, the index of its first child, its
    /// prefix, and whether its right child is still to be visited.
    code: [u32; MAX_H],
    fc: [usize; MAX_H],
    pre: [u32; MAX_H],
    right: [bool; MAX_H],
    /// Level of the current block's root.
    blevel: usize,
    in_run: bool,
    /// Leaf word: remaining bits (the current one included) and base docID.
    word: u64,
    base: u32,
    /// Run: exclusive end.
    run_end: u64,
    cur: u32,
}

impl<'a, const RUN: bool> TrieCursor<'a, RUN> {
    fn new(core: &'a Core) -> Self {
        let (h, lb) = (core.h as usize, core.lb as usize);
        let mut c = Self {
            v: View::new(core),
            h,
            a: h - lb,
            lb,
            n: core.n,
            code: [0; MAX_H],
            fc: [0; MAX_H],
            pre: [0; MAX_H],
            right: [false; MAX_H],
            blevel: 0,
            in_run: false,
            word: 0,
            base: 0,
            run_end: 0,
            cur: END,
        };
        if core.n > 0 {
            c.enter_leftmost(0, 0, 0);
        }
        c
    }

    fn set_word(&mut self, pre: u32, w: u64) {
        debug_assert!(w != 0);
        self.blevel = self.a;
        self.in_run = false;
        self.base = pre << self.lb;
        self.word = w;
        self.cur = self.base + w.trailing_zeros();
    }

    fn set_run(&mut self, l: usize, pre: u32) {
        let start = (pre as u64) << (self.h - l);
        self.blevel = l;
        self.in_run = true;
        self.run_end = start + (1u64 << (self.h - l));
        self.cur = start as u32;
    }

    /// Record node `i` of path level `l`, its code and its first child.
    #[inline]
    fn visit(&mut self, l: usize, i: usize, pre: u32, code: u32) -> usize {
        let fc = self.v.first_child(l, i, code);
        self.code[l] = code;
        self.fc[l] = fc;
        self.pre[l] = pre;
        fc
    }

    /// Enter node `i` of level `l` (prefix `pre`) and stop at its first leaf.
    fn enter_leftmost(&mut self, mut l: usize, mut i: usize, mut pre: u32) {
        loop {
            if l == self.a {
                let w = self.v.leaf_word::<RUN>(self.a, self.h, i);
                self.set_word(pre, w);
                return;
            }
            let code = self.v.code(l, i);
            if RUN && code == 0 {
                self.set_run(l, pre);
                return;
            }
            // The leftmost existing child is the first child either way.
            i = self.visit(l, i, pre, code);
            self.right[l] = code == 3;
            pre = (pre << 1) | (code == 2) as u32;
            l += 1;
        }
    }

    /// Continue the DFS at the deepest level below `top` with a pending right
    /// child; exhausts the cursor if there is none.
    fn advance(&mut self, top: usize) {
        let mut l = top;
        while l > 0 {
            l -= 1;
            if self.right[l] {
                self.right[l] = false;
                let i = self.fc[l] + (self.code[l] & 1) as usize;
                self.enter_leftmost(l + 1, i, (self.pre[l] << 1) | 1);
                return;
            }
        }
        self.cur = END;
    }

    /// Enter node `i` of level `l`, whose prefix `pre` is also the target's,
    /// and stop at the first leaf `>= target`.
    fn seek(&mut self, mut l: usize, mut i: usize, mut pre: u32, target: u32) {
        loop {
            if l == self.a {
                let w = self.v.leaf_word::<RUN>(self.a, self.h, i) & (!0u64 << (target & ((1 << self.lb) - 1)));
                if w != 0 {
                    self.set_word(pre, w);
                } else {
                    self.advance(l);
                }
                return;
            }
            let code = self.v.code(l, i);
            if RUN && code == 0 {
                self.set_run(l, pre);
                self.cur = target;
                return;
            }
            let fc = self.visit(l, i, pre, code);
            if (target >> (self.h - 1 - l)) & 1 == 0 {
                if code & 1 != 0 {
                    self.right[l] = code == 3;
                    (i, pre) = (fc, pre << 1);
                } else {
                    // No left child: the whole right subtree exceeds the target.
                    self.right[l] = false;
                    self.enter_leftmost(l + 1, fc, (pre << 1) | 1);
                    return;
                }
            } else {
                self.right[l] = false;
                if code & 2 == 0 {
                    self.advance(l);
                    return;
                }
                (i, pre) = (fc + (code & 1) as usize, (pre << 1) | 1);
            }
            l += 1;
        }
    }
}

impl<const RUN: bool> Cursor for TrieCursor<'_, RUN> {
    #[inline]
    fn docid(&self) -> u32 {
        self.cur
    }

    #[inline]
    fn next(&mut self) -> u32 {
        if self.cur == END {
            return END;
        }
        if RUN && self.in_run {
            self.cur += 1;
            if (self.cur as u64) < self.run_end {
                return self.cur;
            }
        } else {
            self.word &= self.word - 1;
            if self.word != 0 {
                self.cur = self.base + self.word.trailing_zeros();
                return self.cur;
            }
        }
        self.advance(self.blevel);
        self.cur
    }

    fn next_geq(&mut self, target: u32) -> u32 {
        if self.cur >= target {
            return self.cur;
        }
        if (target as u64) >> self.h != 0 {
            self.cur = END;
            return END;
        }
        // Inside the current block?
        if RUN && self.in_run {
            if (target as u64) < self.run_end {
                self.cur = target;
                return target;
            }
        } else if (target - self.base) >> self.lb == 0 {
            self.word &= !0u64 << (target - self.base);
            if self.word != 0 {
                self.cur = self.base + self.word.trailing_zeros();
            } else {
                self.advance(self.blevel);
            }
            return self.cur;
        }
        // The path to `cur` and the target part at level `l`: `cur` went left,
        // and the target goes right.
        let l = (self.cur ^ target).leading_zeros() as usize + self.h - 32;
        if self.right[l] {
            self.right[l] = false;
            let i = self.fc[l] + (self.code[l] & 1) as usize;
            self.seek(l + 1, i, (self.pre[l] << 1) | 1, target);
        } else {
            self.advance(l);
        }
        self.cur
    }

    fn size(&self) -> usize {
        self.n
    }
}

// ---------------------------------------------------------------------------
// Public codecs.

/// Compact binary trie (see the module docs).
#[derive(Clone, Debug, Default)]
pub struct Trie(Core);

/// Run-compressed compact binary trie: full subtrees are cut (rTrie).
#[derive(Clone, Debug, Default)]
pub struct RunTrie(Core);

macro_rules! trie_codec {
    ($ty:ident, $name:literal, $run:literal) => {
        impl PostingList for $ty {
            const NAME: &'static str = $name;

            fn build(docs: &[u32], universe: u32) -> Self {
                Self(Core::build(docs, universe, $run, LEAF_BITS))
            }

            fn len(&self) -> usize {
                self.0.n
            }

            fn size_bits(&self) -> u64 {
                self.0.size_bits()
            }

            fn decode_into(&self, out: &mut Vec<u32>) {
                self.0.decode_into::<$run>(out);
            }
        }

        impl SetOps for $ty {
            fn intersect(lists: &[&Self], out: &mut Vec<u32>) {
                let cores: Vec<&Core> = lists.iter().map(|l| &l.0).collect();
                intersect::<$run>(&cores, out);
            }

            fn union(lists: &[&Self], out: &mut Vec<u32>) {
                let cores: Vec<&Core> = lists.iter().map(|l| &l.0).collect();
                union::<$run>(&cores, out);
            }
        }

        impl CursorList for $ty {
            type Cursor<'a> = TrieCursor<'a, $run>;

            fn cursor(&self) -> TrieCursor<'_, $run> {
                TrieCursor::new(&self.0)
            }
        }
    };
}

trie_codec!(Trie, "Trie", false);
trie_codec!(RunTrie, "Run-Trie", true);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postings::codecs::conformance;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use std::collections::BTreeSet;

    // The conformance checks are separate tests so that they run in parallel.
    #[test]
    fn conformance_trie_roundtrip() {
        conformance::check_roundtrip::<Trie>();
    }

    #[test]
    fn conformance_trie_setops() {
        conformance::check_setops::<Trie>();
    }

    #[test]
    fn conformance_trie_cursor() {
        conformance::check_cursor::<Trie>();
    }

    #[test]
    fn conformance_run_trie_roundtrip() {
        conformance::check_roundtrip::<RunTrie>();
    }

    #[test]
    fn conformance_run_trie_setops() {
        conformance::check_setops::<RunTrie>();
    }

    #[test]
    fn conformance_run_trie_cursor() {
        conformance::check_cursor::<RunTrie>();
    }

    fn ref_and(lists: &[&[u32]]) -> Vec<u32> {
        let mut acc = lists[0].to_vec();
        for l in &lists[1..] {
            acc.retain(|x| l.binary_search(x).is_ok());
        }
        acc
    }

    fn ref_or(lists: &[&[u32]]) -> Vec<u32> {
        let set: BTreeSet<u32> = lists.iter().flat_map(|l| l.iter().copied()).collect();
        set.into_iter().collect()
    }

    /// Checks decode, intersection, union and the cursor of both variants on
    /// `lists`, with leaf words of every height from 1 to LEAF_BITS so that
    /// small universes also exercise the traversal above the leaf words.
    fn check_all(lists: &[Vec<u32>], universe: u32) {
        let refs: Vec<&[u32]> = lists.iter().map(|l| l.as_slice()).collect();
        let (and, or) = (ref_and(&refs), ref_or(&refs));
        for run in [false, true] {
            for lb in 1..=LEAF_BITS {
                let cores: Vec<Core> = lists.iter().map(|l| Core::build(l, universe, run, lb)).collect();
                let cr: Vec<&Core> = cores.iter().collect();
                let ctx = format!("run={run} lb={lb} u={universe} lists={lists:?}");
                for (c, l) in cores.iter().zip(lists) {
                    let mut out = Vec::new();
                    if run {
                        c.decode_into::<true>(&mut out);
                    } else {
                        c.decode_into::<false>(&mut out);
                    }
                    assert_eq!(&out, l, "decode {ctx}");
                    check_cursor_exhaustive(c, l, run, &ctx);
                }
                let mut out = Vec::new();
                if run {
                    intersect::<true>(&cr, &mut out);
                } else {
                    intersect::<false>(&cr, &mut out);
                }
                assert_eq!(out, and, "intersect {ctx}");
                out.clear();
                if run {
                    union::<true>(&cr, &mut out);
                } else {
                    union::<false>(&cr, &mut out);
                }
                assert_eq!(out, or, "union {ctx}");
            }
        }
    }

    /// A full `next` scan, `next_geq` from a fresh cursor for every target,
    /// and one cursor following every target in increasing order.
    fn check_cursor_exhaustive(c: &Core, docs: &[u32], run: bool, ctx: &str) {
        fn go<const RUN: bool>(c: &Core, docs: &[u32], ctx: &str) {
            let mut cur = TrieCursor::<RUN>::new(c);
            let mut got = Vec::new();
            let mut d = cur.docid();
            while d != END {
                got.push(d);
                d = cur.next();
            }
            assert_eq!(got, docs, "cursor scan {ctx}");
            let top = docs.last().map_or(4, |&x| x.saturating_add(4)).min(u32::MAX - 1);
            for t in 0..=top {
                let e = docs.get(docs.partition_point(|&x| x < t)).copied().unwrap_or(END);
                let mut cur = TrieCursor::<RUN>::new(c);
                assert_eq!(cur.next_geq(t), e, "next_geq({t}) {ctx}");
                if e != END {
                    let e2 = docs.get(docs.partition_point(|&x| x <= e)).copied().unwrap_or(END);
                    assert_eq!(cur.next(), e2, "next after next_geq({t}) {ctx}");
                }
            }
            let mut cur = TrieCursor::<RUN>::new(c);
            for t in 0..=top {
                let e = docs.get(docs.partition_point(|&x| x < t)).copied().unwrap_or(END);
                assert_eq!(cur.next_geq(t), e, "monotone next_geq({t}) {ctx}");
            }
        }
        if docs.len() > 300 {
            return;
        }
        if run {
            go::<true>(c, docs, ctx);
        } else {
            go::<false>(c, docs, ctx);
        }
    }

    fn subset(mask: u32, u: u32) -> Vec<u32> {
        (0..u).filter(|&i| mask >> i & 1 != 0).collect()
    }

    #[test]
    fn small_universes_exhaustive() {
        // Every pair of subsets for universes 1..=6 (2^h leaves with h <= 3).
        for u in 1..=6u32 {
            for x in 0..1u32 << u {
                for y in 0..1u32 << u {
                    check_all(&[subset(x, u), subset(y, u)], u);
                }
            }
        }
        // Every triple of subsets for universes 1..=3.
        for u in 1..=3u32 {
            for x in 0..1u32 << u {
                for y in 0..1u32 << u {
                    for z in 0..1u32 << u {
                        check_all(&[subset(x, u), subset(y, u), subset(z, u)], u);
                    }
                }
            }
        }
        // Single lists.
        for u in 1..=10u32 {
            for x in 0..1u32 << u {
                check_all(&[subset(x, u)], u);
            }
        }
    }

    #[test]
    fn small_universes_random() {
        // Heights 3..=8 with every leaf-word height: random densities, with
        // runs, in groups of 1..=4 lists.
        let mut rng = StdRng::seed_from_u64(11);
        for u in 7..=200u32 {
            for _ in 0..40 {
                let k = rng.gen_range(1..=4);
                let lists: Vec<Vec<u32>> = (0..k)
                    .map(|_| {
                        if rng.gen_bool(0.3) {
                            let runs = rng.gen_range(1..4);
                            runny(&mut rng, u, runs)
                        } else {
                            let p = rng.gen_range(0.0..=1.0);
                            (0..u).filter(|_| rng.gen_bool(p)).collect()
                        }
                    })
                    .collect();
                check_all(&lists, u);
            }
        }
    }

    #[test]
    fn full_ranges() {
        for u in [1u32, 2, 3, 4, 5, 7, 8, 63, 64, 65, 127, 128, 129, 1000, 1024, 4096, 1 << 16, (1 << 17) + 3] {
            let all: Vec<u32> = (0..u).collect();
            let half: Vec<u32> = (0..u).filter(|x| x % 2 == 0).collect();
            check_all(std::slice::from_ref(&all), u);
            check_all(&[all.clone(), all.clone()], u);
            check_all(&[all.clone(), half.clone()], u);
            check_all(&[half, all.clone(), Vec::new()], u);
            let t = Trie::build(&all, u);
            let r = RunTrie::build(&all, u);
            assert!(r.size_bits() <= t.size_bits());
            if u as u64 == 1 << height(u) {
                // A full universe is a single `00` root.
                assert_eq!(r.0.levels[0].len(), 2);
                assert!(r.0.levels[1..].iter().all(|l| l.is_empty()));
            }
        }
    }

    #[test]
    fn empty_lists() {
        for u in [1u32, 2, 3, 100, 1 << 20, u32::MAX] {
            let e: Vec<u32> = Vec::new();
            let t = Trie::build(&e, u);
            let r = RunTrie::build(&e, u);
            assert_eq!((t.len(), r.len()), (0, 0));
            assert_eq!((t.size_bits(), r.size_bits()), (0, 0));
            let mut out = Vec::new();
            t.decode_into(&mut out);
            r.decode_into(&mut out);
            assert!(out.is_empty());
            assert_eq!(t.cursor().docid(), END);
            assert_eq!(r.cursor().next_geq(0), END);
            let one = vec![u - 1];
            let t1 = Trie::build(&one, u);
            Trie::intersect(&[&t, &t1], &mut out);
            Trie::intersect(&[&t1, &t], &mut out);
            Trie::intersect(&[&t, &t], &mut out);
            Trie::union(&[&t, &t], &mut out);
            assert!(out.is_empty());
            Trie::union(&[&t, &t1, &t], &mut out);
            assert_eq!(out, one);
            out.clear();
            let r1 = RunTrie::build(&one, u);
            RunTrie::intersect(&[&r1, &r], &mut out);
            RunTrie::union(&[&r], &mut out);
            assert!(out.is_empty());
            RunTrie::union(&[&r, &r1], &mut out);
            assert_eq!(out, one);
        }
    }

    /// Lists made of aligned and unaligned runs of many lengths, plus noise.
    fn runny(rng: &mut StdRng, universe: u32, runs: usize) -> Vec<u32> {
        let mut set = BTreeSet::new();
        for _ in 0..runs {
            let len = 1u32 << rng.gen_range(0..12);
            let len = if rng.gen_bool(0.5) { len } else { rng.gen_range(1..=len) };
            let start = rng.gen_range(0..universe);
            let start = if rng.gen_bool(0.5) { start & !(len.next_power_of_two() - 1) } else { start };
            set.extend(start..start.saturating_add(len).min(universe));
        }
        for _ in 0..runs {
            set.insert(rng.gen_range(0..universe));
        }
        set.into_iter().collect()
    }

    #[test]
    fn random_runs_all_leaf_heights() {
        let mut rng = StdRng::seed_from_u64(77);
        for &u in &[100u32, 1000, 1 << 12, 5000, 1 << 16] {
            for _ in 0..12 {
                let k = rng.gen_range(1..=4);
                let lists: Vec<Vec<u32>> = (0..k)
                    .map(|_| {
                        let (kind, n, runs) = (rng.gen_range(0..4), rng.gen_range(0..u as usize / 2), rng.gen_range(1..20));
                        match kind {
                            0 => conformance::random_sorted(&mut rng, n, u),
                            1 => (0..u).collect(),
                            _ => runny(&mut rng, u, runs),
                        }
                    })
                    .collect();
                check_all(&lists, u);
            }
        }
    }

    #[test]
    fn more_inputs_than_one_traversal() {
        // 40 inputs > MAX_K: groups are traversed separately and merged.
        let mut rng = StdRng::seed_from_u64(21);
        let u = 1 << 14;
        let common: Vec<u32> = conformance::random_sorted(&mut rng, 300, u);
        let lists: Vec<Vec<u32>> = (0..40)
            .map(|i| {
                let mut s: BTreeSet<u32> = common.iter().copied().collect();
                let extra = if i % 2 == 0 { runny(&mut rng, u, 8) } else { conformance::random_sorted(&mut rng, 4000, u) };
                s.extend(extra);
                s.into_iter().collect()
            })
            .collect();
        let refs: Vec<&[u32]> = lists.iter().map(|l| l.as_slice()).collect();
        let (and, or) = (ref_and(&refs), ref_or(&refs));
        let tries: Vec<Trie> = lists.iter().map(|l| Trie::build(l, u)).collect();
        let runs: Vec<RunTrie> = lists.iter().map(|l| RunTrie::build(l, u)).collect();
        let (tr, rr): (Vec<&Trie>, Vec<&RunTrie>) = (tries.iter().collect(), runs.iter().collect());
        let mut out = Vec::new();
        Trie::intersect(&tr, &mut out);
        assert_eq!(out, and);
        out.clear();
        RunTrie::intersect(&rr, &mut out);
        assert_eq!(out, and);
        out.clear();
        Trie::union(&tr, &mut out);
        assert_eq!(out, or);
        out.clear();
        RunTrie::union(&rr, &mut out);
        assert_eq!(out, or);
    }

    #[test]
    #[should_panic(expected = "same universe")]
    fn mismatched_universes_are_rejected() {
        let (a, b) = (Trie::build(&[1, 2], 100), Trie::build(&[1, 2], 1 << 20));
        Trie::intersect(&[&a, &b], &mut Vec::new());
    }

    #[test]
    fn run_trie_compresses_runs() {
        let mut rng = StdRng::seed_from_u64(5);
        let u = 1 << 24;
        let docs = conformance::clustered(&mut rng, 200_000, u);
        let (t, r) = (Trie::build(&docs, u), RunTrie::build(&docs, u));
        assert!(r.size_bits() < t.size_bits());
        let mut out = Vec::new();
        r.decode_into(&mut out);
        assert_eq!(out, docs);
        // Long runs cost O(log run) nodes.
        let docs: Vec<u32> = (1000..1_001_000).collect();
        let r = RunTrie::build(&docs, u);
        assert!(r.size_bits() < 4000, "{}", r.size_bits());
    }

    #[test]
    fn portable_bit_tricks() {
        let mut rng = StdRng::seed_from_u64(9);
        for _ in 0..10_000 {
            let (s, m) = (rng.gen::<u64>(), rng.gen::<u64>());
            assert_eq!(pdep_portable(s, m), pdep(s, m));
            let x = rng.gen::<u32>() as u64;
            assert_eq!(spread_portable(x) * 3, dup(x));
        }
    }

    /// Words with every popcount, at densities from empty to full.
    #[cfg(target_arch = "aarch64")]
    fn words(rng: &mut StdRng) -> Vec<u64> {
        let mut out = vec![0, !0, 1, 1 << 63, EVEN, !EVEN, 0xFF, 0xFF << 56, 0x8000_0001_8000_0001];
        for k in 0..=64 {
            for _ in 0..20 {
                let mut w = 0u64;
                while w.count_ones() < k {
                    w |= 1 << rng.gen_range(0..64);
                }
                out.push(w);
            }
        }
        out
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_matches_scalar() {
        let mut rng = StdRng::seed_from_u64(13);
        let ws = words(&mut rng);
        // PDEP: every pair of the edge and random words, plus the masks
        // `expand` uses (doubled occupancies).
        for &s in &ws {
            for &m in &ws {
                assert_eq!(pdep_neon(s, m), pdep_portable(s, m), "pdep({s:#x}, {m:#x})");
            }
            let occ = rng.gen::<u32>() as u64 >> rng.gen_range(0..32);
            assert_eq!(pdep_neon(s, dup(occ)), pdep_portable(s, spread_portable(occ) * 3));
        }
        for &x in &ws {
            assert_eq!(spread_neon(x), spread_portable(x), "spread({x:#x})");
        }
        // emit_word: every word at a few bases, appended after a prefix.
        for &w in &ws {
            for base in [0u32, 64, 12_345 << 6, u32::MAX - 63] {
                let mut out = vec![7u32; 3];
                emit_word(&mut out, base, w);
                let want: Vec<u32> = [7, 7, 7].into_iter().chain((0..64).filter(|b| w >> b & 1 == 1).map(|b| base + b)).collect();
                assert_eq!(out, want, "emit_word({base}, {w:#x})");
            }
        }
        // rank_dir: every position of bit vectors of many lengths (short
        // last blocks included) and densities.
        for len in [1usize, 63, 64, 65, 511, 512, 513, 1000, 4096, 4097, 5000] {
            for p in [0.0, 0.01, 0.5, 0.99, 1.0] {
                let mut bv = BitVec::new();
                for _ in 0..len {
                    bv.push_bits(rng.gen_bool(p) as u64, 1);
                }
                let rs = RankSelect::new(bv, false, false);
                for pos in 0..=len {
                    assert_eq!(rank_dir(&rs, rs.bits().words(), pos), rs.rank1(pos), "rank_dir(len {len}, p {p}, {pos})");
                }
            }
        }
    }

    /// Throwaway kernel timings: `cargo test --release -- --ignored --nocapture trie::tests::kernel_timing`
    #[cfg(target_arch = "aarch64")]
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
        let mut rng = StdRng::seed_from_u64(3);
        let n = 4096;
        // `expand`'s PDEP: masks doubling occupancies of `k` random nodes
        // out of 32, chained through the source as in a leaf word.
        println!("pdep (doubled occupancy masks), ns per call:");
        for k in [1, 2, 4, 8, 16, 32] {
            let occ: Vec<u64> = (0..n)
                .map(|_| {
                    let mut o = 0u64;
                    while o.count_ones() < k {
                        o |= 1 << rng.gen_range(0..32);
                    }
                    o
                })
                .collect();
            let src: Vec<u64> = (0..n).map(|_| rng.gen()).collect();
            let masks: Vec<u64> = occ.iter().map(|&o| spread_portable(o) * 3).collect();
            let chain = |f: fn(u64, u64) -> u64| {
                ns_per(n, || masks.iter().zip(&src).fold(0u64, |acc, (&m, &s)| f(s ^ (acc & 1), m)))
            };
            let tput = |f: fn(u64, u64) -> u64| {
                ns_per(n, || masks.iter().zip(&src).fold(0u64, |acc, (&m, &s)| acc ^ f(s, m)))
            };
            println!(
                "  k={k:>2}: latency portable {:.2} neon {:.2} pdep {:.2}; throughput portable {:.2} neon {:.2} pdep {:.2}",
                chain(pdep_portable),
                chain(pdep_neon),
                chain(pdep),
                tput(pdep_portable),
                tput(pdep_neon),
                tput(pdep)
            );
        }
        let xs: Vec<u64> = (0..n).map(|_| rng.gen::<u32>() as u64).collect();
        println!(
            "spread, ns per call: latency portable {:.2} neon {:.2}; throughput portable {:.2} neon {:.2}",
            ns_per(n, || xs.iter().fold(0u64, |acc, &x| spread_portable(x ^ (acc & 1)))),
            ns_per(n, || xs.iter().fold(0u64, |acc, &x| spread_neon(x ^ (acc & 1)))),
            ns_per(n, || xs.iter().fold(0u64, |acc, &x| acc ^ spread_portable(x))),
            ns_per(n, || xs.iter().fold(0u64, |acc, &x| acc ^ spread_neon(x))),
        );
        // rank_dir at random positions of a 2^20-bit level.
        let mut bv = BitVec::new();
        for _ in 0..1 << 20 {
            bv.push_bits(rng.gen_bool(0.5) as u64, 1);
        }
        let rs = RankSelect::new(bv, false, false);
        let pos: Vec<usize> = (0..n).map(|_| rng.gen_range(0..1 << 20)).collect();
        println!(
            "rank, ns per call: rank1 {:.2} rank_dir {:.2}",
            ns_per(n, || pos.iter().map(|&p| rs.rank1(p)).sum::<usize>()),
            ns_per(n, || pos.iter().map(|&p| rank_dir(&rs, rs.bits().words(), p)).sum::<usize>()),
        );
        // emit_word on random non-zero words of k bits on average.
        println!("emit_word, ns per word:");
        let mut out: Vec<u32> = Vec::with_capacity(64 * n + 64);
        for p in [0.01, 0.05, 0.1, 0.3, 0.5, 0.9] {
            let ws: Vec<u64> = (0..n)
                .map(|_| loop {
                    let w = (0..64).fold(0u64, |w, b| w | ((rng.gen_bool(p) as u64) << b));
                    if w != 0 {
                        break w;
                    }
                })
                .collect();
            let naive = ns_per(n, || {
                out.clear();
                for (i, &w) in ws.iter().enumerate() {
                    let (mut w, base) = (w, (i as u32) << 6);
                    out.reserve(64);
                    while w != 0 {
                        out.push(base + w.trailing_zeros());
                        w &= w - 1;
                    }
                }
                out.len()
            });
            let neon = ns_per(n, || {
                out.clear();
                for (i, &w) in ws.iter().enumerate() {
                    emit_word(&mut out, (i as u32) << 6, w);
                }
                out.len()
            });
            println!("  bits/word {:>5.1}: naive {naive:.2} neon {neon:.2}", 64.0 * p);
        }
        // Whole codecs (all kernels together): decode, AND and OR of three
        // lists of 30K, 300K and 2M postings out of 8M.
        fn codec<C: PostingList + SetOps>(lists: &[Vec<u32>], u: u32) -> String {
            let cs: Vec<C> = lists.iter().map(|l| C::build(l, u)).collect();
            let refs: Vec<&C> = cs.iter().collect();
            let n: usize = lists.iter().map(|l| l.len()).sum();
            let mut out = Vec::new();
            let dec = ns_per(n, || {
                out.clear();
                for c in &cs {
                    c.decode_into(&mut out);
                }
                out.len()
            });
            let and = ns_per(n, || {
                out.clear();
                C::intersect(&refs, &mut out);
                out.len()
            });
            let or = ns_per(n, || {
                out.clear();
                C::union(&refs, &mut out);
                out.len()
            });
            format!("{}: decode {dec:.2} and {and:.3} or {or:.2}", C::NAME)
        }
        let u = 8_000_000;
        for (name, clustered) in [("uniform", false), ("clustered", true)] {
            let lists: Vec<Vec<u32>> = [30_000, 300_000, 2_000_000]
                .iter()
                .map(|&n| if clustered { conformance::clustered(&mut rng, n, u) } else { conformance::random_sorted(&mut rng, n, u) })
                .collect();
            println!("{name} ns/posting: {} | {}", codec::<Trie>(&lists, u), codec::<RunTrie>(&lists, u));
        }
    }
}
