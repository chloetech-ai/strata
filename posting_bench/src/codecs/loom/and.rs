//! Multi-list intersection.
//!
//! The shortest list (the *driver*) produces candidates; every other list
//! (rarest first) filters them in place through a [`Probe`], with early exit
//! as soon as no candidate survives.
//!
//! * Where the driver's partition is EF, it is decoded (SIMD) in batches of
//!   [`BATCH`]. A probe filters a batch partition by partition of its list:
//!   run → keep, bitvector → branch-free bit test, EF → [`ef_filter`], which
//!   picks per partition, from the number of candidates and of postings:
//!   - *merge*: when the list has at most [`MERGE_RATIO`] postings per
//!     candidate there, decode them and intersect with the AVX-512
//!     block-merge kernel;
//!   - *far probes*: when it has [`FAR_RATIO`] or more, locate each
//!     candidate's bucket independently from the select samples, so the
//!     probes (and their cache misses) overlap;
//!   - otherwise *probe streams*: a membership probe that moves forward
//!     from the previous one's bucket and also returns a lower bound on the
//!     next posting, below which candidates are skipped (so a list that is
//!     locally sparser than the driver effectively leads, leapfrog style);
//!     the candidates are split into two or four parts probed in lockstep,
//!     because one probe's position depends on the previous one's.
//!
//!   When a batch dies, the list that killed it reports (a lower bound on)
//!   its next posting and the driver jumps there, skipping partitions.
//! * Where the driver's partition is a bitvector or a run (bulk mode), up to
//!   [`BULK_WORDS`] candidate words are formed directly; every other list
//!   ANDs in its bitvector words over the overlap (aligned, no shifts), runs
//!   pass through, and only lists with EF pieces in the range filter the
//!   extracted candidates as above.

use super::{ef, find_partition, simd, Loom, Part, BV, EF, RUN};
use crate::END;
use std::cell::RefCell;

/// Driver postings decoded per batch.
pub const BATCH: usize = 256;
/// Candidate words per bulk step.
pub const BULK_WORDS: usize = 64;
/// Sentinel slots ([`END`]) kept after the candidates of a batch, so that
/// skipping counts 8 candidates at a time without bounds checks.
const PAD: usize = 8;
/// Size of the driver's batch buffer: candidates, decoder slack, sentinels.
const BATCH_BUF: usize = BATCH + ef::DECODE_SLACK + PAD;

/// Number of the (sorted) `p[0..8]` below `v`.
#[inline(always)]
unsafe fn count_below(p: *const u32, v: u32) -> usize {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vl"))]
    {
        use std::arch::x86_64::*;
        let x = _mm256_loadu_si256(p.cast());
        _mm256_cmplt_epu32_mask(x, _mm256_set1_epi32(v as i32)).count_ones() as usize
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx512f", target_feature = "avx512vl")))]
    {
        (0..8).map(|j| (*p.add(j) < v) as usize).sum()
    }
}

/// Receiver of intersection results (in increasing order).
pub trait Sink {
    fn values(&mut self, v: &[u32]);
    /// Every set bit of `w`; bit `i` of word `k` is docID `base + 64k + i`.
    fn words(&mut self, w: &[u64], base: u32);
    /// Every posting of `l`.
    fn list(&mut self, l: &Loom);
}

pub struct VecSink<'a>(pub &'a mut Vec<u32>);

impl Sink for VecSink<'_> {
    #[inline]
    fn values(&mut self, v: &[u32]) {
        self.0.extend_from_slice(v);
    }

    #[inline]
    fn words(&mut self, w: &[u64], base: u32) {
        let out = &mut *self.0;
        out.reserve(simd::popcount(w) as usize + simd::SLACK);
        // SAFETY: room for every set bit plus the extraction slack.
        unsafe {
            let end = simd::extract(w, base, out.as_mut_ptr().add(out.len()));
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
    fn values(&mut self, v: &[u32]) {
        self.0 += v.len() as u64;
    }

    #[inline]
    fn words(&mut self, w: &[u64], _base: u32) {
        self.0 += simd::popcount(w);
    }

    fn list(&mut self, l: &Loom) {
        self.0 += l.n as u64;
    }
}

/// Monotone membership / `next_geq` over one list.
#[derive(Clone, Copy)]
struct Probe<'a> {
    list: &'a Loom,
    p: usize,
    part: Part,
    /// EF membership probe of the current partition.
    efp: ef::EfProbe,
    /// Set once a candidate exceeded the list's last posting.
    exhausted: bool,
    /// Partition walked by bulk mode (independent of `p`, also monotone).
    walk: usize,
}

impl<'a> Probe<'a> {
    fn new(list: &'a Loom) -> Self {
        let part = Part::load(list, 0);
        Probe { list, p: 0, part, efp: part.ef_probe(), exhausted: false, walk: 0 }
    }

    /// Load the partition containing `x > part.last`; false if `x` is past
    /// the list's last posting.
    #[inline(never)]
    fn seek(&mut self, x: u32) -> bool {
        let p = find_partition(&self.list.ends, self.p + 1, x);
        if p >= self.list.ends.len() {
            self.exhausted = true;
            return false;
        }
        self.p = p;
        self.part = Part::load(self.list, p);
        self.efp = self.part.ef_probe();
        true
    }

    /// EF partition: is `x` in it, and a lower bound on its first posting
    /// `>= x` (`base <= x <= last`; see [`ef::EfProbe::probe`]).
    #[inline(always)]
    fn ef_probe(&mut self, x: u32) -> (bool, u32) {
        // SAFETY: x - base <= last - base, and targets do not decrease.
        let (found, bound) = unsafe { self.efp.probe((x - self.part.base) as u64) };
        (found, self.part.base + bound as u32)
    }

    /// A lower bound on the first posting `>= x`: a posting, or a docID in a
    /// gap ([`END`] if none). `x` must not be below a previous argument.
    #[inline]
    fn next_geq(&mut self, x: u32) -> u32 {
        if self.exhausted || (x > self.part.last && !self.seek(x)) {
            return END;
        }
        match self.part.kind {
            RUN => x,
            BV => self.part.bv_next_geq(x),
            _ => self.ef_probe(x).1,
        }
    }

    /// Keep, in place and in order, the candidates of `c[..n]` that are in
    /// the list; returns how many (`k`). Candidates must increase across
    /// calls. `c[n..n + PAD]` must hold [`END`], and on return so does
    /// `c[k..k + PAD]`.
    #[inline]
    fn filter(&mut self, c: &mut [u32], n: usize, mb: &mut MergeBuf) -> usize {
        debug_assert!(n + PAD <= c.len() && c[n..n + PAD].iter().all(|&x| x == END));
        if self.exhausted {
            return 0;
        }
        let (mut i, mut k) = (0usize, 0usize);
        // SAFETY (all unchecked accesses): k <= i < n <= c.len().
        unsafe {
            while i < n {
                let x = *c.get_unchecked(i);
                if x > self.part.last && !self.seek(x) {
                    break;
                }
                let last = self.part.last;
                match self.part.kind {
                    RUN => {
                        while i < n && *c.get_unchecked(i) <= last {
                            *c.get_unchecked_mut(k) = *c.get_unchecked(i);
                            k += 1;
                            i += 1;
                        }
                    }
                    BV => {
                        let part = self.part;
                        while i < n {
                            let x = *c.get_unchecked(i);
                            if x > last {
                                break;
                            }
                            *c.get_unchecked_mut(k) = x;
                            k += part.bv_test(x) as usize;
                            i += 1;
                        }
                    }
                    _ => {
                        // The candidates inside this partition.
                        let j = i + c.get_unchecked(i..n).partition_point(|&x| x <= last);
                        (i, k) = ef_filter(&mut self.efp, &self.part, c.as_mut_ptr(), i, j, k, mb);
                    }
                }
            }
            c.get_unchecked_mut(k..k + PAD).fill(END);
        }
        k
    }
}

/// From this many candidates in one EF partition, two probe streams filter
/// them (see [`ef_filter`]).
const SPLIT2: usize = 6;
/// From this many candidates, four streams.
const SPLIT4: usize = 24;
/// When an EF partition holds this many times more postings than there are
/// candidates in it, probes locate buckets from the select samples.
const FAR_RATIO: usize = 64;
/// Test-only override of the EF filter mode (0: adaptive, 1: one stream,
/// 2: far probes, 3: merge when it fits, 4/5: two/four streams).
#[cfg(test)]
pub(crate) static FORCE_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
/// Merge mode (decode + SIMD merge) needs this many candidates in a
/// partition...
const MERGE_MIN: usize = 16;
/// ...and at most this many postings per candidate in their range.
const MERGE_RATIO: usize = 12;
/// Most postings decoded by one merge.
const MERGE_CAP: usize = 8192;

/// Filter the candidates `c[i..j]`, all inside the EF partition `part`,
/// through its probe `p`, writing survivors from `k <= i`. Returns the new
/// `(i, k)` (`i == j`). See the module documentation for the three modes.
///
/// In probe-stream mode a probe's position depends on the previous probe's,
/// so one stream runs at the latency of that chain (a load, a popcount and
/// a `pdep` per probe) and its cache misses do not overlap. So with enough
/// candidates the range is cut into two or four parts probed by independent
/// copies of the probe in lockstep, which the out-of-order core overlaps;
/// the parts' survivors are then moved together.
///
/// # Safety
/// `c[j..j + PAD]` must be readable, and every candidate from `j` on must be
/// above the partition or a sentinel.
#[inline(always)]
unsafe fn ef_filter(
    p: &mut ef::EfProbe,
    part: &Part,
    c: *mut u32,
    i: usize,
    j: usize,
    k: usize,
    mb: &mut MergeBuf,
) -> (usize, usize) {
    let (base, n) = (part.base, part.n);
    #[cfg(test)]
    match FORCE_MODE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => return ef_stream(p, base, c, i, j, k),
        2 => return ef_far(p, base, c, i, j, k),
        3 => {
            if let Some(r) = ef_merge(part, c, i, j, k, mb) {
                return r;
            }
        }
        4 => return ef_streams::<2>(p, base, c, i, j, k),
        5 => return ef_streams::<4>(p, base, c, i, j, k),
        _ => {}
    }
    if j - i >= MERGE_MIN {
        // The list's postings in the candidates' range, estimated from the
        // partition's density.
        let (lo, hi) = (*c.add(i) - base, *c.add(j - 1) - base);
        let est = n as u64 * (hi - lo + 1) as u64 / ((part.last - base) as u64 + 1);
        if est <= (MERGE_RATIO * (j - i)) as u64 {
            if let Some(r) = ef_merge(part, c, i, j, k, mb) {
                return r;
            }
        }
    }
    if n as usize >= FAR_RATIO * (j - i) {
        ef_far(p, base, c, i, j, k)
    } else if j - i >= SPLIT4 {
        ef_streams::<4>(p, base, c, i, j, k)
    } else if j - i >= SPLIT2 {
        ef_streams::<2>(p, base, c, i, j, k)
    } else {
        ef_stream(p, base, c, i, j, k)
    }
}

/// [`ef_filter`] with `S` streams over equal parts of `c[i..j]`.
#[inline(always)]
unsafe fn ef_streams<const S: usize>(p: &mut ef::EfProbe, base: u32, c: *mut u32, i: usize, j: usize, k: usize) -> (usize, usize) {
    let mut probe = [*p; S];
    // Stream s owns c[start[s]..start[s + 1]].
    let start: [usize; S] = std::array::from_fn(|s| i + (j - i) * s / S);
    let end: [usize; S] = std::array::from_fn(|s| if s + 1 < S { start[s + 1] } else { j });
    let mut ii = start;
    let mut kk = start;
    kk[0] = k;
    'lockstep: loop {
        for s in 0..S {
            if ii[s] >= end[s] {
                break 'lockstep;
            }
        }
        for s in 0..S {
            let x = *c.add(ii[s]);
            // SAFETY: the target lies in the partition; each stream's
            // targets increase.
            let (found, v) = probe[s].probe((x - base) as u64);
            *c.add(kk[s]) = x;
            kk[s] += found as usize;
            ii[s] = skip_below(c, ii[s] + 1, base + v as u32, end[s]);
        }
    }
    for s in 0..S {
        (ii[s], kk[s]) = ef_stream(&mut probe[s], base, c, ii[s], end[s], kk[s]);
    }
    let mut w = kk[0];
    for s in 1..S {
        let len = kk[s] - start[s];
        std::ptr::copy(c.add(start[s]), c.add(w), len);
        w += len;
    }
    *p = probe[S - 1];
    (j, w)
}

/// Merge mode of [`ef_filter`]: decode the partition's postings from the
/// first candidate to the last (`ef::EfCur::decode`, SIMD) and intersect
/// them with the candidates with the crate's widest block-merge kernel
/// (AVX-512 16 x 16 here). Returns `None` (nothing done) when there are
/// more than about [`MERGE_RATIO`] times as many postings as candidates in
/// that range (the caller only estimated it).
#[inline(never)]
unsafe fn ef_merge(part: &Part, c: *mut u32, i: usize, j: usize, k: usize, mb: &mut MergeBuf) -> Option<(usize, usize)> {
    let base = part.base;
    let (lo, hi) = ((*c.add(i) - base) as u64, (*c.add(j - 1) - base) as u64);
    // Exact count: the first postings >= lo and >= hi (both exist: the last
    // candidate is at most the partition's last posting).
    let mut first = part.ef;
    let v1 = first.next_geq(lo);
    let mut end = first;
    if hi > v1 {
        end.next_geq(hi);
    }
    let count = end.idx - first.idx + 1;
    if count > MERGE_RATIO * (j - i) + 16 || count > MERGE_CAP {
        return None;
    }
    let dec = mb.dec.as_mut_ptr();
    *dec = base + v1 as u32;
    if count > 1 {
        first.decode(count - 1, base, dec.add(1));
    }
    mb.out.clear();
    let cands = std::slice::from_raw_parts(c.add(i), j - i);
    crate::algorithms::simd::intersect(cands, &mb.dec[..count], &mut mb.out);
    std::ptr::copy_nonoverlapping(mb.out.as_ptr(), c.add(k), mb.out.len());
    Some((j, k + mb.out.len()))
}

/// [`ef_filter`] when the partition is much denser than the candidates:
/// every candidate is probed independently ([`ef::EfProbe::probe_far`]), so
/// the probes and their cache misses overlap. Candidates are not skipped
/// (that would chain the probes again, and rarely applies at this density).
#[inline(always)]
unsafe fn ef_far(p: &ef::EfProbe, base: u32, c: *mut u32, i: usize, j: usize, mut k: usize) -> (usize, usize) {
    for t in i..j {
        let x = *c.add(t);
        // SAFETY: the target lies in the partition.
        let (found, _) = p.probe_far((x - base) as u64);
        *c.add(k) = x;
        k += found as usize;
    }
    (j, k)
}

/// One probe stream over `c[i..end]` (see [`ef_filter`]).
#[inline(always)]
unsafe fn ef_stream(p: &mut ef::EfProbe, base: u32, c: *mut u32, mut i: usize, end: usize, mut k: usize) -> (usize, usize) {
    while i < end {
        let x = *c.add(i);
        let (found, v) = p.probe((x - base) as u64);
        *c.add(k) = x;
        k += found as usize;
        i = skip_below(c, i + 1, base + v as u32, end);
    }
    (end, k)
}

/// First index from `i` whose candidate is `>= v`, at most `end`. Every
/// candidate below `v` precedes it: the list has no posting there.
#[inline(always)]
unsafe fn skip_below(c: *const u32, mut i: usize, v: u32, end: usize) -> usize {
    loop {
        let below = count_below(c.add(i), v);
        i += below;
        if below < PAD || i >= end {
            return i.min(end);
        }
    }
}

/// The driver: partition-wise access to the shortest list.
struct Driver<'a> {
    list: &'a Loom,
    p: usize,
    part: Part,
    /// Every posting below `pos` has been produced.
    pos: u32,
}

impl<'a> Driver<'a> {
    fn new(list: &'a Loom) -> Self {
        Driver { list, p: 0, part: Part::load(list, 0), pos: 0 }
    }

    /// Make the current partition the one containing `lb` (`lb >= pos`);
    /// false when no posting `>= lb` exists.
    #[inline]
    fn seek(&mut self, lb: u32) -> bool {
        if lb <= self.part.last {
            return true;
        }
        let p = find_partition(&self.list.ends, self.p + 1, lb);
        if p >= self.list.ends.len() {
            return false;
        }
        self.p = p;
        self.part = Part::load(self.list, p);
        self.pos = self.part.base;
        true
    }

    /// EF partition: write up to [`BATCH`] postings `>= lb` to `out`
    /// (at least one: `pos <= lb <= last`).
    #[inline]
    fn fill(&mut self, lb: u32, out: &mut [u32; BATCH_BUF]) -> usize {
        debug_assert!(self.part.kind == EF && lb >= self.pos && lb <= self.part.last);
        let base = self.part.base;
        let ef = &mut self.part.ef;
        let n = self.part.n as usize;
        // SAFETY: a posting >= lb exists in the partition, and lb is above
        // the cursor's current value (every value below pos was produced).
        unsafe {
            let first = if lb == self.pos && ef.idx != usize::MAX {
                ef.next()
            } else {
                ef.next_geq((lb - base) as u64)
            };
            *out.get_unchecked_mut(0) = base + first as u32;
            let k = (n - ef.idx).min(BATCH);
            if k > 1 {
                // SAFETY: k - 1 values follow; `out` has decode slack.
                ef.decode(k - 1, base, out.as_mut_ptr().add(1));
            }
            self.pos = *out.get_unchecked(k - 1) + 1;
            out.get_unchecked_mut(k..k + PAD).fill(END);
            k
        }
    }
}

/// Per-thread buffers of the bulk path.
struct Scratch {
    words: [u64; BULK_WORDS],
    vals: [u32; BULK_WORDS * 64 + simd::SLACK],
    merge: MergeBuf,
}

/// Buffers of merge mode (see [`ef_filter`]).
struct MergeBuf {
    /// Decoded postings of the probed list.
    dec: Vec<u32>,
    /// Intersection output.
    out: Vec<u32>,
}

impl Scratch {
    fn new() -> Box<Self> {
        Box::new(Scratch {
            words: [0; BULK_WORDS],
            vals: [0; BULK_WORDS * 64 + simd::SLACK],
            merge: MergeBuf {
                dec: vec![0; MERGE_CAP + ef::DECODE_SLACK],
                out: Vec::with_capacity(BULK_WORDS * 64 + simd::SLACK),
            },
        })
    }
}

thread_local! {
    static SCRATCH: RefCell<Box<Scratch>> = RefCell::new(Scratch::new());
}

/// Intersect `lists` into `sink`.
pub fn intersect<S: Sink>(lists: &[&Loom], sink: &mut S) {
    match lists {
        [] => {}
        _ if lists.iter().any(|l| l.n == 0) => {}
        [l] => sink.list(l),
        _ => SCRATCH.with(|cell| match cell.try_borrow_mut() {
            Ok(mut s) => intersect_k(lists, sink, &mut s),
            Err(_) => intersect_k(lists, sink, &mut Scratch::new()),
        }),
    }
}

/// At most this many lists use stack storage for their probes.
const INLINE_LISTS: usize = 8;

fn intersect_k<S: Sink>(lists: &[&Loom], sink: &mut S, s: &mut Scratch) {
    // Driver: the shortest list; probes: the others, rarest first.
    let di = (0..lists.len()).min_by_key(|&i| lists[i].n).unwrap();
    let driver = Driver::new(lists[di]);
    let mut others: [&Loom; INLINE_LISTS] = [lists[0]; INLINE_LISTS];
    if lists.len() <= INLINE_LISTS + 1 {
        let mut m = 0;
        for (i, l) in lists.iter().enumerate() {
            if i != di {
                others[m] = l;
                m += 1;
            }
        }
        let others = &mut others[..m];
        others.sort_by_key(|l| l.n);
        let mut probes = [Probe::new(others[0]); INLINE_LISTS];
        for (pr, l) in probes.iter_mut().zip(others.iter()).skip(1) {
            *pr = Probe::new(l);
        }
        run(driver, &mut probes[..m], sink, s);
    } else {
        let mut v: Vec<&Loom> = lists.iter().enumerate().filter(|&(i, _)| i != di).map(|(_, l)| *l).collect();
        v.sort_by_key(|l| l.n);
        let mut probes: Vec<Probe> = v.into_iter().map(Probe::new).collect();
        run(driver, &mut probes, sink, s);
    }
}

fn run<S: Sink>(mut d: Driver, probes: &mut [Probe], sink: &mut S, s: &mut Scratch) {
    let mut batch = [END; BATCH_BUF];
    let mut lb = 0u32;
    loop {
        if lb == END || !d.seek(lb) {
            return;
        }
        lb = lb.max(d.pos);
        if d.part.kind != EF {
            match bulk(&mut d, probes, sink, s, lb) {
                Some(next) => lb = next,
                None => return,
            }
            continue;
        }
        let n = d.fill(lb, &mut batch);
        let batch_last = batch[n - 1];
        let mut k = n;
        let mut done = false;
        let mut hint = batch_last.saturating_add(1);
        for pr in probes.iter_mut() {
            k = pr.filter(&mut batch, k, &mut s.merge);
            done |= pr.exhausted;
            if k == 0 {
                if !done {
                    // Leapfrog: the driver jumps to this list's next posting.
                    hint = hint.max(pr.next_geq(hint));
                }
                break;
            }
        }
        if k > 0 {
            sink.values(&batch[..k]);
        }
        if done {
            return;
        }
        lb = hint;
    }
}

/// Mask of the bits `>= lo % 64` / `<= hi % 64` of a word.
#[inline(always)]
fn from_bit(lo: u32) -> u64 {
    !0u64 << (lo % 64)
}

#[inline(always)]
fn to_bit(hi: u32) -> u64 {
    !0u64 >> (63 - hi % 64)
}

/// One bulk step over the driver's bitvector or run partition, from `lb`
/// for up to [`BULK_WORDS`] words. Returns the next lower bound, or `None`
/// when the intersection is complete.
#[inline(never)]
fn bulk<S: Sink>(d: &mut Driver, probes: &mut [Probe], sink: &mut S, s: &mut Scratch, lb: u32) -> Option<u32> {
    let part = d.part;
    debug_assert!(part.kind != EF && lb >= part.base && lb <= part.last);
    let ws = lb & !63;
    let we = part.last.min(ws.saturating_add((BULK_WORDS * 64 - 1) as u32));
    let nw = ((we - ws) / 64 + 1) as usize;
    let cand = &mut s.words[..nw];
    if part.kind == BV {
        let off = ((ws - part.abase) / 64) as usize;
        // SAFETY: words [off, off + nw) lie inside the partition's bitvector.
        cand.copy_from_slice(unsafe { std::slice::from_raw_parts(part.bits.add(off), nw) });
    } else {
        cand.fill(!0);
    }
    cand[0] &= from_bit(lb);
    cand[nw - 1] &= to_bit(we);
    d.pos = we.saturating_add(1);
    let next = if we == u32::MAX { END } else { we + 1 };

    let mut done = false;
    let mut probe_mask = 0u64; // probes needing per-candidate checks
    for (j, pr) in probes.iter_mut().enumerate() {
        let list = pr.list;
        let ends = &list.ends;
        let mut q = pr.walk.max(pr.p);
        if ends[q] < lb {
            q = find_partition(ends, q, lb);
        }
        loop {
            if q >= ends.len() {
                // The list ends before `we`: nothing after its last posting.
                let lastj = ends[ends.len() - 1];
                if lastj < ws {
                    cand.fill(0);
                } else {
                    let wi = ((lastj - ws) / 64) as usize;
                    cand[wi] &= to_bit(lastj);
                    cand[wi + 1..].fill(0);
                }
                done = true;
                break;
            }
            let qp = Part::load(list, q);
            if qp.base > we {
                break;
            }
            let lo = qp.base.max(lb);
            let hi = qp.last.min(we);
            match qp.kind {
                RUN => {}
                BV => {
                    let a = ((lo - ws) / 64) as usize;
                    let b = ((hi - ws) / 64) as usize;
                    // Candidate word `a` covers docIDs from `lo & !63`, which
                    // is bitvector word `off` (lo >= base >= abase).
                    let off = (((lo & !63) - qp.abase) / 64) as usize;
                    // SAFETY: cand words a..=b map to bitvector words
                    // off ..= off + b - a, all inside the partition.
                    let src = unsafe { std::slice::from_raw_parts(qp.bits.add(off), b - a + 1) };
                    if a == b {
                        cand[a] &= src[0] | !(from_bit(lo) & to_bit(hi));
                    } else {
                        cand[a] &= src[0] | !from_bit(lo);
                        simd::and_into(&mut cand[a + 1..b], &src[1..b - a]);
                        cand[b] &= src[b - a] | !to_bit(hi);
                    }
                }
                _ => probe_mask |= 1 << j.min(63),
            }
            if qp.last > we {
                break;
            }
            q += 1;
        }
        pr.walk = q.min(ends.len() - 1);
        if cand.iter().all(|&w| w == 0) {
            // No candidate left: lists not walked yet need no flags.
            probe_mask = 0;
            break;
        }
    }

    if probe_mask == 0 {
        sink.words(cand, ws);
    } else {
        // Extract the candidates and probe the lists with EF pieces.
        let dst = s.vals.as_mut_ptr();
        // SAFETY: at most 64 * nw values plus slack fit in `vals`.
        let mut n = unsafe { simd::extract(cand, ws, dst).offset_from(dst) as usize };
        s.vals[n..n + PAD].fill(END);
        for (j, pr) in probes.iter_mut().enumerate() {
            if n == 0 {
                break;
            }
            if probe_mask >> j.min(63) & 1 == 1 {
                n = pr.filter(&mut s.vals, n, &mut s.merge);
                done |= pr.exhausted;
            }
        }
        if n > 0 {
            sink.values(&s.vals[..n]);
        }
    }
    if done {
        None
    } else {
        Some(next)
    }
}

