//! Multi-list union over a windowed bitmap accumulator.
//!
//! The docID space is swept in windows of [`WIN`] docIDs ([`WIN_WORDS`]
//! words, 8 KiB, L1-resident), each starting at a multiple of 64. For each
//! window every list contributes the partitions overlapping it:
//!
//! * bitvector: its words are ORed in directly (both are aligned to docID
//!   words, so there are no shifts);
//! * run: the range is filled word-wise;
//! * EF: its postings are decoded 16 at a time ([`super::ef::EfCur::decode`])
//!   and their bits set; where a partition crosses the window end, the
//!   decoder stops at the first posting at or after it (found with one
//!   `next_geq`).
//!
//! Then the touched word range is extracted to docIDs (`vpcompressd`) and
//! cleared. Windows with nothing in them are skipped: every list keeps the
//! docID of its next unprocessed posting, and the next window starts at the
//! smallest of them.

use super::{simd, Loom, Part, BV, RUN};
use std::cell::RefCell;

/// Window size in docIDs.
pub const WIN: u64 = 1 << 16;
pub const WIN_WORDS: usize = (WIN / 64) as usize;

/// No posting left.
const NONE: u64 = u64::MAX;

/// Receiver of union results, one window at a time.
pub trait Sink {
    /// Consume the set bits of `words` (bit `i` of word `k` is docID
    /// `base + 64k + i`) and leave every word zero.
    fn window(&mut self, words: &mut [u64], base: u32);
    /// Every posting of `l`.
    fn list(&mut self, l: &Loom);
}

pub struct VecSink<'a>(pub &'a mut Vec<u32>);

impl Sink for VecSink<'_> {
    #[inline]
    fn window(&mut self, words: &mut [u64], base: u32) {
        let out = &mut *self.0;
        out.reserve(words.len() * 64 + simd::SLACK);
        // SAFETY: room for every bit of the window plus the slack.
        unsafe {
            let end = simd::extract_clear(words, base, out.as_mut_ptr().add(out.len()));
            out.set_len(end.offset_from(out.as_ptr()) as usize);
        }
    }

    fn list(&mut self, l: &Loom) {
        use crate::codecs::PostingList;
        l.decode_into(self.0);
    }
}

pub struct CountSink(pub u64);

impl Sink for CountSink {
    #[inline]
    fn window(&mut self, words: &mut [u64], _base: u32) {
        self.0 += simd::popcount(words);
        words.fill(0);
    }

    fn list(&mut self, l: &Loom) {
        self.0 += l.n as u64;
    }
}

/// One list's position in the sweep.
struct OrCur<'a> {
    list: &'a Loom,
    p: usize,
    part: Part,
    /// DocID of the next posting not yet added (for EF it is already decoded
    /// from the cursor; for bitvectors and runs it is a word-aligned resume
    /// point or the partition's first posting). [`NONE`] when done.
    next: u64,
}

impl<'a> OrCur<'a> {
    fn new(list: &'a Loom) -> Self {
        let mut c = OrCur { list, p: 0, part: Part::empty(), next: NONE };
        c.enter(0);
        c
    }

    /// Load partition `p` (if any) and find its first posting.
    #[inline(never)]
    fn enter(&mut self, p: usize) {
        self.p = p;
        if p >= self.list.ends.len() {
            self.next = NONE;
            return;
        }
        self.part = Part::load(self.list, p);
        let part = &mut self.part;
        self.next = match part.kind {
            RUN => part.base as u64,
            BV => part.bv_next_geq(part.base) as u64,
            // SAFETY: the partition has at least one value.
            _ => part.base as u64 + unsafe { part.ef.next() },
        };
    }

    /// Add this list's postings in `[ws, we)` to `win` (word 0 = docID
    /// `ws`), widening the touched word range `[lo, hi]`. Returns the docID
    /// of the next posting not added (at least `we`), or [`NONE`].
    #[inline(always)]
    fn fill(&mut self, win: &mut [u64; WIN_WORDS], ws: u64, we: u64, lo: &mut usize, hi: &mut usize) -> u64 {
        debug_assert!(ws % 64 == 0 && we == ws + WIN);
        loop {
            let next = self.next;
            if next >= we {
                return next;
            }
            let part = &mut self.part;
            let last = part.last as u64;
            let e = last.min(we - 1);
            match part.kind {
                RUN => {
                    simd::set_range(win, (next - ws) as usize, (e - ws) as usize);
                    *lo = (*lo).min(((next - ws) / 64) as usize);
                    *hi = (*hi).max(((e - ws) / 64) as usize);
                }
                BV => {
                    let s = next & !63;
                    let a = ((s - ws) / 64) as usize;
                    let b = ((e - ws) / 64) as usize;
                    let off = ((s - part.abase as u64) / 64) as usize;
                    // SAFETY: bitvector words off..=off + b - a cover
                    // docIDs [s, e], inside the partition.
                    let src = unsafe { std::slice::from_raw_parts(part.bits.add(off), b - a + 1) };
                    // SAFETY: a <= b < WIN_WORDS.
                    simd::or_into(unsafe { win.get_unchecked_mut(a..=b) }, src);
                    *lo = (*lo).min(a);
                    *hi = (*hi).max(b);
                }
                _ => {
                    let base = part.base as u64;
                    let n = part.n as usize;
                    let ef = &mut part.ef;
                    let rel = next - ws;
                    win[(rel / 64) as usize] |= 1u64 << (rel % 64);
                    *lo = (*lo).min((rel / 64) as usize);
                    // Window-relative values: base - ws (mod 2^32).
                    let add = (base as u32).wrapping_sub(ws as u32);
                    let remaining = n - (ef.idx + 1);
                    if last < we {
                        // The rest of the partition is inside the window.
                        if remaining > 0 {
                            // SAFETY: `remaining` values follow.
                            unsafe { set_decoded(ef, remaining, add, win) };
                        }
                        *hi = (*hi).max(((last - ws) / 64) as usize);
                    } else {
                        // Values below `we`, then the first one at or after.
                        let mut after = *ef;
                        // SAFETY: we - base is above the current value and at
                        // most the last value.
                        let v = base + unsafe { after.next_geq(we - base) };
                        let count = after.idx - (ef.idx + 1);
                        if count > 0 {
                            // SAFETY: `count` values follow, all below we.
                            unsafe { set_decoded(ef, count, add, win) };
                            *hi = (*hi).max(((base + ef.val - ws) / 64) as usize);
                        } else {
                            *hi = (*hi).max((rel / 64) as usize);
                        }
                        *ef = after;
                        self.next = v;
                        return v;
                    }
                }
            }
            if last >= we {
                self.next = we;
                return we;
            }
            self.enter(self.p + 1);
        }
    }
}

/// Decode `count` values after the cursor's current one (as window offsets
/// with `add`) and set their bits in `win`.
///
/// # Safety
/// `count` values must follow, and each `value + add` must be below [`WIN`].
#[inline(always)]
unsafe fn set_decoded(ef: &mut super::ef::EfCur, count: usize, add: u32, win: &mut [u64; WIN_WORDS]) {
    const CH: usize = 256;
    let mut buf = [0u32; CH + super::ef::DECODE_SLACK];
    let mut left = count;
    while left > 0 {
        let m = left.min(CH);
        ef.decode(m, add, buf.as_mut_ptr());
        for &r in &buf[..m] {
            debug_assert!((r as u64) < WIN);
            *win.get_unchecked_mut((r / 64) as usize) |= 1u64 << (r % 64);
        }
        left -= m;
    }
}

thread_local! {
    static WINDOW: RefCell<Box<[u64; WIN_WORDS]>> = RefCell::new(Box::new([0; WIN_WORDS]));
}

/// At most this many lists use stack storage for their cursors.
const INLINE_LISTS: usize = 8;

/// Union of `lists` into `sink`.
pub fn union<S: Sink>(lists: &[&Loom], sink: &mut S) {
    let k = lists.iter().filter(|l| l.n > 0).count();
    match k {
        0 => return,
        1 => return sink.list(lists.iter().find(|l| l.n > 0).unwrap()),
        _ => {}
    }
    WINDOW.with(|cell| match cell.try_borrow_mut() {
        Ok(mut w) => union_k(lists, sink, &mut w),
        Err(_) => union_k(lists, sink, &mut Box::new([0; WIN_WORDS])),
    })
}

fn union_k<S: Sink>(lists: &[&Loom], sink: &mut S, win: &mut [u64; WIN_WORDS]) {
    let mut nonempty = lists.iter().filter(|l| l.n > 0);
    if lists.len() <= INLINE_LISTS {
        let mut m = 0;
        let mut arr: [OrCur; INLINE_LISTS] = std::array::from_fn(|_| match nonempty.next() {
            Some(l) => {
                m += 1;
                OrCur::new(l)
            }
            None => OrCur { list: lists[0], p: 0, part: Part::empty(), next: NONE },
        });
        sweep(&mut arr[..m], sink, win);
    } else {
        let mut curs: Vec<OrCur> = nonempty.map(|l| OrCur::new(l)).collect();
        sweep(&mut curs, sink, win);
    }
}

fn sweep<S: Sink>(curs: &mut [OrCur], sink: &mut S, win: &mut [u64; WIN_WORDS]) {
    let mut start = curs.iter().map(|c| c.next).min().unwrap_or(NONE);
    while start != NONE {
        let ws = start & !63;
        let we = ws + WIN;
        let (mut lo, mut hi) = (WIN_WORDS, 0usize);
        let mut next = NONE;
        for c in curs.iter_mut() {
            next = next.min(c.fill(win, ws, we, &mut lo, &mut hi));
        }
        if lo <= hi {
            sink.window(&mut win[lo..=hi], (ws + 64 * lo as u64) as u32);
        }
        start = next;
    }
}
