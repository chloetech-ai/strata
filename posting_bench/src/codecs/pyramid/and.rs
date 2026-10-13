//! Intersection: chunk join, then a per-chunk driver or block join.
//!
//! The chunk keys of the smallest list are galloped in the others. In each
//! common chunk, with `c` the chunk holding the fewest values:
//! * if `c` holds at most 64 values, or at most [`DRIVE_MAX`] values that
//!   average at most [`DRIVE_VPB`] per present block, its values are the
//!   candidates and are probed in the other chunks, list by list, compacting
//!   the survivors (SvS within the chunk);
//! * otherwise (all chunks Tiered or Flat) a *block join* walks `c`'s
//!   blocks in order (dense index and sparse offset advance sequentially)
//!   and handles those present in every list: a block dense everywhere is a
//!   4-word AND whose bits are extracted; any other block filters the low
//!   bytes of a sparse member (`c`'s when sparse) 32 at a time against the
//!   others (`vpermb` bit lookups for dense blocks, byte compares for sparse
//!   ones) and writes the survivors with `vpcompressd`.
//!
//! Latency hiding: chunk metadata is prefetched [`PREFETCH`] chunks ahead,
//! and the probes of a driven chunk into its first probed list are
//! *planned* one chunk ahead: while chunk `i` is processed, the leaf each
//! probe of chunk `i + 1` will read (a bitmap word, or a sparse block's
//! lows) is located from the (cached) masks and directory, once per block,
//! and prefetched. A planned probe is then evaluated branch-free: a bit
//! test on a word (of a static zero bitmap unless the block is dense) OR a
//! masked byte compare (over zero bytes unless the block is sparse). Probes
//! into Flat chunks are direct bit tests.

use super::simd::{self, SLACK};
use super::{prefetch, Chunk, Pyramid, Sink, View, DRIVE_MAX, DRIVE_VPB, PREFETCH};
use crate::codecs::roaring::simd::gallop;
use std::cell::RefCell;

/// A list's chunk as seen by probes.
#[derive(Clone, Copy)]
enum PView {
    Small(*const u16, u32),
    Tiered(View),
}

impl PView {
    /// # Safety
    /// `c` is a chunk of `l`.
    #[inline(always)]
    unsafe fn of(l: &Pyramid, c: &Chunk) -> PView {
        if c.small() {
            let v = l.small_vals(c);
            PView::Small(v.as_ptr(), v.len() as u32)
        } else {
            PView::Tiered(View::new(l, c))
        }
    }

    #[inline(always)]
    unsafe fn contains(&self, v: u16) -> bool {
        match self {
            PView::Small(p, n) => simd::u16s_contain(*p, *n as usize, v),
            PView::Tiered(view) => view.contains(v),
        }
    }
}

/// A block bitmap that is always zero, tested by planned probes of
/// non-dense blocks.
static ZERO: [u64; 4] = [0; 4];

/// A planned probe: hit iff bit `bit` of `*word` is set or `l` is among the
/// `n` bytes at `lows`.
#[derive(Clone, Copy)]
struct Target {
    word: *const u64,
    lows: *const u8,
    n: u32,
    bit: u32,
    l: u32,
}

impl Target {
    const NONE: Target = Target { word: ZERO.as_ptr(), lows: std::ptr::null(), n: 0, bit: 0, l: 0 };

    #[inline(always)]
    unsafe fn hit(&self) -> bool {
        let d = ((*self.word >> self.bit) & 1) as u8;
        let s = simd::bytes_contain(self.lows, self.n as usize, self.l as u8) as u8;
        d | s != 0
    }
}

impl View {
    /// Locates the leaf a probe of the low 16 bits `v` reads and prefetches
    /// it.
    #[inline(always)]
    unsafe fn plan(&self, v: u16) -> Target {
        let b = (v >> 8) as u32;
        let l = (v & 255) as u32;
        let w = (b >> 6) as usize;
        let bit = 1u64 << (b & 63);
        let p = *self.present.get_unchecked(w) & bit != 0;
        let d = *self.dense.get_unchecked(w) & bit != 0;
        let mut t = Target { word: ZERO.as_ptr(), lows: self.lows, n: 0, bit: l & 63, l };
        if p {
            if d {
                t.word = self.bitmap(w, bit).add((l >> 6) as usize);
                prefetch(t.word);
            } else {
                let (s, e) = self.sparse_range(self.sparse_rank(w, bit));
                t.lows = self.lows.add(s as usize);
                t.n = e - s;
                prefetch(t.lows);
            }
        }
        t
    }
}

/// A common chunk, prepared one iteration ahead.
struct Plan {
    /// Driven chunk (else block join).
    drive: bool,
    base: u32,
    /// Driver list and first probed list.
    dj: usize,
    j1: usize,
    n: usize,
    cand: [u16; DRIVE_MAX],
    /// Planned probes of `cand` into list `j1` (when Tiered there).
    tg: [Target; DRIVE_MAX],
    /// Chunk index per list.
    cis: Vec<u32>,
    /// Driven chunk: the view of list `j1`.
    views: Vec<PView>,
    /// Block join: every list's view.
    jviews: Vec<View>,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            drive: false,
            base: 0,
            dj: 0,
            j1: 0,
            n: 0,
            cand: [0; DRIVE_MAX],
            tg: [Target::NONE; DRIVE_MAX],
            cis: Vec::new(),
            views: Vec::new(),
            jviews: Vec::new(),
        }
    }
}

#[derive(Default)]
struct Scratch {
    order: Vec<usize>,
    pos: Vec<usize>,
    /// Common chunks: `k` chunk indices each.
    common: Vec<u32>,
    /// The current and next chunk's plans, alternating.
    plans: [Plan; 2],
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

/// Intersection of `lists` (at least two) into `sink`.
pub(super) fn and_lists<S: Sink>(lists: &[&Pyramid], sink: &mut S) {
    SCRATCH.with(|cell| match cell.try_borrow_mut() {
        Ok(mut s) => and_with(&mut s, lists, sink),
        Err(_) => and_with(&mut Scratch::default(), lists, sink),
    })
}

fn and_with<S: Sink>(s: &mut Scratch, lists: &[&Pyramid], sink: &mut S) {
    let k = lists.len();
    s.order.clear();
    s.order.extend(0..k);
    s.order.sort_by_key(|&i| lists[i].card);
    // The lists in order, on the stack for up to 8 lists.
    let mut stack: [&Pyramid; 8] = [lists[0]; 8];
    let heap: Vec<&Pyramid>;
    let ls: &[&Pyramid] = if k <= 8 {
        for (j, &i) in s.order.iter().enumerate() {
            stack[j] = lists[i];
        }
        &stack[..k]
    } else {
        heap = s.order.iter().map(|&i| lists[i]).collect();
        &heap
    };
    // Common chunks: the keys of the smallest list, galloped in the others.
    let first = ls[0];
    s.pos.clear();
    s.pos.resize(k, 0);
    s.common.clear();
    'keys: for (ci, &key) in first.keys.iter().enumerate() {
        for j in 1..k {
            let l = ls[j];
            let p = gallop(&l.keys, s.pos[j], key);
            s.pos[j] = p;
            match l.keys.get(p) {
                None => break 'keys,
                Some(&x) if x != key => continue 'keys,
                Some(_) => {}
            }
        }
        s.pos[0] = ci;
        s.common.extend(s.pos.iter().map(|&p| p as u32));
    }
    let n = s.common.len() / k;
    if n == 0 {
        return;
    }
    let Scratch { common, plans, .. } = s;
    for i in 0..PREFETCH.min(n) {
        for (j, l) in ls.iter().enumerate() {
            l.prefetch_chunk(common[i * k + j] as usize);
        }
    }
    // SAFETY: every plan is built from chunks of `ls` at indices taken from
    // their key lists.
    unsafe {
        plan(ls, &common[..k], &mut plans[0]);
        for i in 0..n {
            let [p0, p1] = plans;
            let (cur, next) = if i & 1 == 0 { (p0, p1) } else { (p1, p0) };
            if i + PREFETCH < n {
                for (j, l) in ls.iter().enumerate() {
                    l.prefetch_chunk(common[(i + PREFETCH) * k + j] as usize);
                }
            }
            if i + 1 < n {
                plan(ls, &common[(i + 1) * k..(i + 2) * k], next);
            }
            if cur.drive {
                drive(ls, cur, sink);
            } else {
                block_join(&cur.jviews, cur.dj, cur.base, sink);
            }
        }
    }
}

/// Prepares the common chunk `cis` (chunk index per list): views, driver,
/// candidates and planned probes into the first probed list.
///
/// # Safety
/// `cis[j]` is a chunk of `ls[j]`, all with the same key.
#[inline]
unsafe fn plan(ls: &[&Pyramid], cis: &[u32], p: &mut Plan) {
    p.views.clear();
    p.cis.clear();
    p.cis.extend_from_slice(cis);
    let (mut dj, mut dcard) = (0, u32::MAX);
    for (j, l) in ls.iter().enumerate() {
        let c = l.chunks.get_unchecked(cis[j] as usize);
        if c.card < dcard {
            dcard = c.card;
            dj = j;
        }
    }
    p.base = (ls[0].keys[cis[0] as usize] as u32) << 16;
    p.dj = dj;
    p.drive = dcard <= 64
        || (dcard as usize <= DRIVE_MAX && {
            // Mid-sized drivers: values only when blocks are sparse (few
            // values per present block); otherwise the block join.
            let c = ls[dj].chunks.get_unchecked(cis[dj] as usize);
            let m = ls[dj].meta.as_ptr().add(c.off as usize);
            let blocks = (*m).count_ones() + (*m.add(1)).count_ones() + (*m.add(2)).count_ones() + (*m.add(3)).count_ones();
            dcard <= DRIVE_VPB * blocks
        });
    if !p.drive {
        p.jviews.clear();
        for (j, l) in ls.iter().enumerate() {
            p.jviews.push(View::new(l, l.chunks.get_unchecked(cis[j] as usize)));
        }
        return;
    }
    let l = ls[dj];
    let c = l.chunks.get_unchecked(cis[dj] as usize);
    p.n = if c.small() {
        // Fixed-size copy: `small` is padded past its last chunk.
        *(p.cand.as_mut_ptr() as *mut [u16; 64]) = *(l.small.as_ptr().add(c.off as usize) as *const [u16; 64]);
        c.card as usize
    } else {
        // A Tiered chunk with at most DRIVE_MAX values (rare).
        let mut tmp = [0u32; DRIVE_MAX + SLACK];
        let m = l.decode_tiered(c, 0, tmp.as_mut_ptr());
        for (x, &v) in p.cand.iter_mut().zip(&tmp[..m]) {
            *x = v as u16;
        }
        m
    };
    p.j1 = if dj == 0 { 1 } else { 0 };
    let l1 = ls[p.j1];
    p.views.push(PView::of(l1, l1.chunks.get_unchecked(cis[p.j1] as usize)));
    if let PView::Tiered(v) = &p.views[0] {
        if v.flat {
            for i in 0..p.n {
                let x = *p.cand.get_unchecked(i);
                let word = v.bitmaps.add((x >> 6) as usize);
                prefetch(word);
                *p.tg.get_unchecked_mut(i) = Target { word, lows: v.lows, n: 0, bit: (x & 63) as u32, l: 0 };
            }
        } else {
            // Locate each block once for its run of candidates.
            let mut last = u32::MAX;
            let mut t = Target::NONE;
            for i in 0..p.n {
                let x = *p.cand.get_unchecked(i);
                let b = (x >> 8) as u32;
                if b != last {
                    last = b;
                    t = v.plan(x);
                } else if t.n == 0 && t.word != ZERO.as_ptr() {
                    // Dense block: the probed word depends on the low byte.
                    let bm = t.word.sub((t.l >> 6) as usize);
                    t.word = bm.add(((x & 255) >> 6) as usize);
                    prefetch(t.word);
                }
                t.l = (x & 255) as u32;
                t.bit = (x & 63) as u32;
                *p.tg.get_unchecked_mut(i) = t;
            }
        }
    }
}

/// SvS within a driven chunk: planned probes into `j1`, then direct probes
/// into the remaining lists.
///
/// # Safety
/// `p` was planned from live lists.
#[inline]
unsafe fn drive<S: Sink>(ls: &[&Pyramid], p: &mut Plan, sink: &mut S) {
    let mut n = 0;
    match &p.views[0] {
        PView::Tiered(_) => {
            for i in 0..p.n {
                let x = *p.cand.get_unchecked(i);
                *p.cand.get_unchecked_mut(n) = x;
                n += p.tg.get_unchecked(i).hit() as usize;
            }
        }
        PView::Small(v, m) => {
            for i in 0..p.n {
                let x = *p.cand.get_unchecked(i);
                *p.cand.get_unchecked_mut(n) = x;
                n += simd::u16s_contain(*v, *m as usize, x) as usize;
            }
        }
    }
    for (j, l) in ls.iter().enumerate() {
        if n == 0 {
            return;
        }
        if j == p.dj || j == p.j1 {
            continue;
        }
        let view = PView::of(l, l.chunks.get_unchecked(*p.cis.get_unchecked(j) as usize));
        let mut m = 0;
        for i in 0..n {
            let x = *p.cand.get_unchecked(i);
            *p.cand.get_unchecked_mut(m) = x;
            m += view.contains(x) as usize;
        }
        n = m;
    }
    if n > 0 {
        sink.small(&p.cand[..n], p.base);
    }
}

/// Block join of the Tiered chunks `views` (same key): the blocks of the
/// smallest chunk `dj` are walked in order (dense index and sparse offset
/// advance sequentially, no select), and each of its blocks present in
/// every list is tested against the others: a block dense everywhere is a
/// 4-word AND; otherwise the low bytes of a sparse block (the driver's when
/// sparse) are filtered 32 at a time against the others' bitmaps (`vpermb`)
/// or low bytes (byte compares).
///
/// # Safety
/// `views` are Tiered views of one common chunk.
#[inline]
unsafe fn block_join<S: Sink>(views: &[View], dj: usize, base: u32, sink: &mut S) {
    let dv = views.get_unchecked(dj);
    for w in 0..4 {
        let (mut cand, mut all_dense) = (!0u64, !0u64);
        for v in views {
            cand &= v.present[w];
            all_dense &= v.dense[w];
        }
        if cand == 0 {
            continue;
        }
        let ddw = dv.dense[w];
        let mut sr = dv.scum[w];
        let mut so = dv.sparse_start(sr);
        // Driver blocks up to the last candidate of the word.
        let mut m = dv.present[w] & (!0u64 >> cand.leading_zeros());
        while m != 0 {
            let bit = m & m.wrapping_neg();
            m ^= bit;
            let b = (w as u32) << 6 | bit.trailing_zeros();
            let bbase = base | b << 8;
            if ddw & bit == 0 {
                let e = dv.sparse_end(sr, so);
                sr += 1;
                if cand & bit != 0 {
                    let n = (e - so) as usize;
                    let d = simd::load_bytes32(dv.lows.add(so as usize), n);
                    let hits = filter_lows(views, dj, w, bit, &d, n);
                    if hits != 0 {
                        sink.lanes(&d, hits, bbase);
                    }
                }
                so = e;
                continue;
            }
            if cand & bit != 0 {
                if all_dense & bit != 0 {
                    let mut acc = *(dv.bitmap(w, bit) as *const [u64; 4]);
                    for (i, v) in views.iter().enumerate() {
                        if i != dj {
                            let x = &*(v.bitmap(w, bit) as *const [u64; 4]);
                            for (a, &y) in acc.iter_mut().zip(x) {
                                *a &= y;
                            }
                        }
                    }
                    sink.bitmap(&acc, bbase);
                } else {
                    // The driver is dense here: the first sparse member drives.
                    let j = views.iter().position(|v| v.dense[w] & bit == 0).unwrap_or(0);
                    let v = views.get_unchecked(j);
                    let (s, e) = v.sparse_range(v.sparse_rank(w, bit));
                    let n = (e - s) as usize;
                    let d = simd::load_bytes32(v.lows.add(s as usize), n);
                    let hits = filter_lows(views, j, w, bit, &d, n);
                    if hits != 0 {
                        sink.lanes(&d, hits, bbase);
                    }
                }
            }
        }
    }
}

/// Which of the `n` low bytes `d` (of list `src`'s block `64 w + tz(bit)`)
/// are in the same block of every other list.
#[inline(always)]
unsafe fn filter_lows(views: &[View], src: usize, w: usize, bit: u64, d: &[u8; 32], n: usize) -> u32 {
    let mut hits = simd::low_mask32(n);
    for (i, v) in views.iter().enumerate() {
        if i == src {
            continue;
        }
        if v.dense[w] & bit != 0 {
            hits &= simd::test_bits(d, v.bitmap(w, bit));
        } else {
            let (s, e) = v.sparse_range(v.sparse_rank(w, bit));
            hits &= simd::test_bytes(d, n, v.lows.add(s as usize), (e - s) as usize);
        }
        if hits == 0 {
            break;
        }
    }
    hits
}
