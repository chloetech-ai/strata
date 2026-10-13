//! Union: k-way merge of the chunk keys, then a merge of the chunks sharing
//! a key.
//!
//! Dense groups (whose union spans at least [`ACC_BLOCKS`] blocks, or with a
//! Flat member) are ORed into a 65536-bit thread-local accumulator touching
//! only present blocks: Flat members word by word, dense blocks 4 words at a
//! time, all sparse values of a member decoded at once by
//! [`super::simd::expand_sparse`] and set bit by bit, Small values bit by
//! bit. The touched blocks are then extracted in order (whole word ranges
//! where they are dense) and re-zeroed, or, counting, popcounted.
//!
//! In other groups, each 64-block word of the masks is split into
//! *multi* blocks (held by at least two inputs) and, for each input, its
//! *single* blocks. The union's blocks are visited in order as maximal runs
//! of one input's single blocks, interrupted by multi blocks or by another
//! input's run:
//! * a single run is emitted by decoding that input's blocks sequentially
//!   (dense: bitmap extraction; sparse: widening the low bytes; Small: the
//!   values), starting from ranks computed once per run. Count-only unions
//!   skip single runs entirely: they add the chunk cardinalities up front;
//! * a multi block ORs its members into a 256-bit accumulator (dense
//!   bitmaps 4 words at a time, low bytes and Small values bit by bit),
//!   which is extracted (or, counting, popcounted minus the members'
//!   counts).

use super::simd::{self, SLACK};
use super::{Chunk, Pyramid, Sink, SparseIds, View};
use std::cell::RefCell;

/// Chunk groups whose union spans at least this many blocks use the
/// accumulator.
pub const ACC_BLOCKS: u32 = 96;

struct Scratch {
    pos: Vec<usize>,
    group: Vec<(usize, usize)>,
    members: Vec<Member>,
    /// Sparse block ids per member, computed on first use.
    ids: Vec<SparseIds>,
    /// 65536-bit accumulator, all zero between uses.
    acc: Box<[u64; 1024]>,
    /// Decoded sparse values of one member.
    tmp: Vec<u32>,
}

impl Default for Scratch {
    fn default() -> Self {
        Scratch {
            pos: Vec::new(),
            group: Vec::new(),
            members: Vec::new(),
            ids: Vec::new(),
            acc: vec![0u64; 1024].into_boxed_slice().try_into().unwrap(),
            tmp: Vec::new(),
        }
    }
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

/// One input chunk of a union group.
#[derive(Clone, Copy)]
struct Member {
    /// Tiered view (Small chunks: masks only, no dense blocks).
    view: View,
    small: bool,
    flat: bool,
    /// Small: next value and end.
    vals: *const u16,
    end: *const u16,
    /// Whether this member's slot of `Scratch::ids` is filled.
    ids_ready: bool,
}

impl Member {
    /// # Safety
    /// `c` is a chunk of `p`.
    #[inline(always)]
    unsafe fn new(p: &Pyramid, c: &Chunk) -> Member {
        if c.small() {
            let v = p.small_vals(c);
            let mut view = View::EMPTY;
            for &x in v {
                view.present[(x >> 14) as usize] |= 1 << ((x >> 8) & 63);
            }
            return Member {
                view,
                small: true,
                flat: false,
                vals: v.as_ptr(),
                end: v.as_ptr().add(v.len()),
                ids_ready: false,
            };
        }
        Member {
            view: View::new(p, c),
            small: false,
            flat: c.flat(),
            vals: std::ptr::null(),
            end: std::ptr::null(),
            ids_ready: false,
        }
    }

    /// Small: the values of blocks `< b_end` (advancing past them).
    #[inline(always)]
    unsafe fn small_until(&mut self, b_end: u32) -> &'static [u16] {
        let start = self.vals;
        while self.vals < self.end && ((*self.vals >> 8) as u32) < b_end {
            self.vals = self.vals.add(1);
        }
        std::slice::from_raw_parts(start, self.vals.offset_from(start) as usize)
    }

    /// Emits this member's blocks `run` of mask word `w` (in order).
    #[inline(always)]
    unsafe fn emit_run<S: Sink>(&mut self, w: usize, run: u64, ids: &mut SparseIds, base: u32, sink: &mut S) {
        let last = (w as u32) << 6 | (63 - run.leading_zeros());
        if self.small {
            let vals = self.small_until(last + 1);
            sink.small(vals, base);
            return;
        }
        if !self.ids_ready {
            *ids = self.view.sparse_ids();
            self.ids_ready = true;
        }
        sink.run(&self.view, w, run, ids.as_ptr().add(1), base);
    }

    /// Count-only: skips this member's blocks `run` of word `w`.
    #[inline(always)]
    unsafe fn skip_run(&mut self, w: usize, run: u64) {
        if self.small {
            let last = (w as u32) << 6 | (63 - run.leading_zeros());
            self.small_until(last + 1);
        }
    }

    /// ORs block `b = 64 w + tz(bit)` into `acc`; returns its count.
    #[inline(always)]
    unsafe fn or_into(&mut self, w: usize, bit: u64, b: u32, acc: &mut [u64; 4]) -> u32 {
        if self.small {
            let vals = self.small_until(b + 1);
            for &x in vals {
                acc[((x >> 6) & 3) as usize] |= 1 << (x & 63);
            }
            return vals.len() as u32;
        }
        let v = &self.view;
        if v.dense[w] & bit != 0 {
            let bm = v.bitmap(w, bit);
            let mut c = 0;
            for (i, a) in acc.iter_mut().enumerate() {
                let x = *bm.add(i);
                *a |= x;
                c += x.count_ones();
            }
            c
        } else {
            let (s, e) = v.sparse_range(v.sparse_rank(w, bit));
            for x in s..e {
                let l = *v.lows.add(x as usize);
                acc[(l >> 6) as usize] |= 1 << (l & 63);
            }
            e - s
        }
    }
}

/// Union of `lists` (at least two) into `sink`.
pub(super) fn or_lists<S: Sink>(lists: &[&Pyramid], sink: &mut S) {
    SCRATCH.with(|cell| match cell.try_borrow_mut() {
        Ok(mut s) => or_with(&mut s, lists, sink),
        Err(_) => or_with(&mut Scratch::default(), lists, sink),
    })
}

fn or_with<S: Sink>(s: &mut Scratch, lists: &[&Pyramid], sink: &mut S) {
    let Scratch { pos, group, members, ids, acc, tmp } = s;
    pos.clear();
    pos.resize(lists.len(), 0);
    loop {
        let mut key = u32::MAX;
        for (l, &p) in lists.iter().zip(pos.iter()) {
            if let Some(&k) = l.keys.get(p) {
                key = key.min(k as u32);
            }
        }
        if key == u32::MAX {
            return;
        }
        group.clear();
        for (i, (l, p)) in lists.iter().zip(pos.iter_mut()).enumerate() {
            if l.keys.get(*p) == Some(&(key as u16)) {
                group.push((i, *p));
                *p += 1;
            }
        }
        let base = key << 16;
        if let [(i, ci)] = group[..] {
            sink.chunk(lists[i], &lists[i].chunks[ci], base);
            continue;
        }
        members.clear();
        let mut flat = false;
        let mut cards = 0u64;
        for &(i, ci) in group.iter() {
            let c = &lists[i].chunks[ci];
            if S::COUNT {
                sink.add(c.card as i64);
            }
            cards += c.card as u64;
            flat |= c.flat();
            // SAFETY: chunk of lists[i].
            members.push(unsafe { Member::new(lists[i], c) });
        }
        if ids.len() < members.len() {
            ids.resize(members.len(), SparseIds([0; 1 + 256 + 48]));
        }
        let mut union = [0u64; 4];
        for mb in members.iter() {
            for (u, p) in union.iter_mut().zip(mb.view.present) {
                *u |= p;
            }
        }
        // SAFETY: members of one chunk group; acc is all zero.
        unsafe {
            if flat || union.iter().map(|w| w.count_ones()).sum::<u32>() >= ACC_BLOCKS {
                or_acc(members, &union, cards, acc, tmp, base, sink);
            } else {
                or_chunk(members, ids, base, sink);
            }
        }
    }
}

/// ORs the chunks of `members` (same key, blocks `union`) into `acc`, then
/// emits (count-only: counts, minus the cardinalities already added) and
/// clears the touched blocks.
///
/// # Safety
/// `members` are fresh members of chunks with the same key; `acc` is all
/// zero.
#[inline(never)]
unsafe fn or_acc<S: Sink>(
    members: &[Member],
    union: &[u64; 4],
    cards: u64,
    acc: &mut [u64; 1024],
    tmp: &mut Vec<u32>,
    base: u32,
    sink: &mut S,
) {
    let a = acc.as_mut_ptr();
    for mb in members {
        if mb.small {
            let n = mb.end.offset_from(mb.vals) as usize;
            for &x in std::slice::from_raw_parts(mb.vals, n) {
                *a.add((x >> 6) as usize) |= 1 << (x & 63);
            }
            continue;
        }
        let v = &mb.view;
        if mb.flat {
            for i in 0..1024 {
                *a.add(i) |= *v.bitmaps.add(i);
            }
            continue;
        }
        // Dense blocks, 4 words each.
        let mut bm = v.bitmaps;
        for w in 0..4 {
            let mut m = v.dense[w];
            while m != 0 {
                let b = (w << 6) | m.trailing_zeros() as usize;
                m &= m - 1;
                for i in 0..4 {
                    *a.add(4 * b + i) |= *bm.add(i);
                }
                bm = bm.add(4);
            }
        }
        // Sparse values, decoded all at once (blocks with offsets: block
        // by block).
        if v.ms > 0 && v.offs {
            let mut r = 0;
            for w in 0..4 {
                let mut m = v.present[w] & !v.dense[w];
                while m != 0 {
                    let b = (w << 6) | m.trailing_zeros() as usize;
                    m &= m - 1;
                    let (s, e) = v.sparse_range(r);
                    r += 1;
                    for x in s..e {
                        let l = *v.lows.add(x as usize);
                        *a.add(4 * b + (l >> 6) as usize) |= 1 << (l & 63);
                    }
                }
            }
        } else if v.ms > 0 {
            let ns = v.ns as usize;
            let ids = v.sparse_ids();
            tmp.clear();
            tmp.reserve(ns + SLACK);
            simd::expand_sparse(v.lows, v.u, 0, ns, ids.as_ptr().add(1), 0, tmp.as_mut_ptr());
            for &x in std::slice::from_raw_parts(tmp.as_ptr(), ns) {
                *a.add((x >> 6) as usize) |= 1 << (x & 63);
            }
        }
    }
    if S::COUNT {
        let mut c = 0u64;
        for x in acc.iter_mut() {
            c += x.count_ones() as u64;
            *x = 0;
        }
        // The members' cardinalities were added by the caller.
        sink.add(c as i64 - cards as i64);
        return;
    }
    // Extract each word of the masks' span of touched blocks in one pass
    // (whole 256-word ranges when dense, block by block otherwise).
    for (w, &u) in union.iter().enumerate() {
        if u == 0 {
            continue;
        }
        if u.count_ones() >= 16 {
            let lo = u.trailing_zeros() as usize;
            let hi = 64 - u.leading_zeros() as usize;
            let words = std::slice::from_raw_parts_mut(a.add(4 * (64 * w + lo)), 4 * (hi - lo));
            sink.words(words, base | ((64 * w + lo) as u32) << 8);
            words.fill(0);
            continue;
        }
        let mut m = u;
        while m != 0 {
            let b = (w << 6) | m.trailing_zeros() as usize;
            m &= m - 1;
            let blk = &mut *(a.add(4 * b) as *mut [u64; 4]);
            sink.bitmap(blk, base | (b as u32) << 8);
            *blk = [0; 4];
        }
    }
}

/// Merges the chunks of `members` (same key): single runs and multi blocks.
///
/// # Safety
/// `members` are fresh members of chunks with the same key.
#[inline]
unsafe fn or_chunk<S: Sink>(members: &mut [Member], ids: &mut [SparseIds], base: u32, sink: &mut S) {
    for w in 0..4 {
        let (mut once, mut multi) = (0u64, 0u64);
        for mb in members.iter() {
            let p = mb.view.present[w];
            multi |= once & p;
            once |= p;
        }
        let mut m = once;
        while m != 0 {
            let bit = m & m.wrapping_neg();
            if multi & bit != 0 {
                let b = (w as u32) << 6 | bit.trailing_zeros();
                m ^= bit;
                let mut acc = [0u64; 4];
                let mut sum = 0u32;
                for mb in members.iter_mut() {
                    if mb.view.present[w] & bit != 0 {
                        sum += mb.or_into(w, bit, b, &mut acc);
                    }
                }
                if S::COUNT {
                    let c: u32 = acc.iter().map(|x| x.count_ones()).sum();
                    sink.add(c as i64 - sum as i64);
                } else {
                    sink.bitmap(&acc, base | b << 8);
                }
                continue;
            }
            // A run of one member's single blocks, up to the next block that
            // is not one of them.
            let owner = members.iter().position(|mb| mb.view.present[w] & bit != 0).unwrap_or(0);
            let own = members.get_unchecked(owner).view.present[w] & !multi;
            let others = m & !own;
            let below = if others == 0 { !0 } else { (others & others.wrapping_neg()) - 1 };
            let run = m & own & below;
            m &= !run;
            let mb = members.get_unchecked_mut(owner);
            if S::COUNT {
                mb.skip_run(w, run);
            } else {
                mb.emit_run(w, run, ids.get_unchecked_mut(owner), base, sink);
            }
        }
    }
}
