//! Roaring containers (array, bitmap, run) and the kernels that combine them.
//!
//! Kernels write their result into a reusable [`Scratch`] (or, for large
//! bitmap results, describe it lazily) and return an [`AndOut`] / [`OrOut`]
//! that is then either materialised into a new [`Container`] or decoded
//! straight into `u32` docIDs, so the `u32` set operations never build the
//! final Roaring result.

use super::simd::{self, gallop, gallop_by, BitPos, EXTRACT_SLACK};
use std::ops::Range;

/// Largest array container (CRoaring's `DEFAULT_MAX_SIZE`).
pub const ARRAY_MAX: usize = 4096;
/// `u64` words in a bitmap container.
pub const WORDS: usize = 1024;
/// Serialized size of a bitmap container.
pub const BITMAP_BYTES: usize = 8 * WORDS;

pub type Words = [u64; WORDS];

/// The values `start..=start + len`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub start: u16,
    pub len: u16,
}

impl Run {
    #[inline]
    pub fn last(self) -> u16 {
        self.start + self.len
    }

    #[inline]
    pub fn card(self) -> u32 {
        self.len as u32 + 1
    }
}

/// The low 16 bits of the docIDs of one chunk. Never empty.
#[derive(Clone, Debug)]
pub enum Container {
    /// Sorted values, at most [`ARRAY_MAX`] of them.
    Array(Vec<u16>),
    /// 2^16-bit bitmap and its cardinality (more than [`ARRAY_MAX`]).
    Bitmap(Box<Words>, u32),
    /// Sorted, maximal (non-adjacent) runs.
    Run(Vec<Run>),
}

/// Serialized size of a run container with `runs` runs: a 16-bit run count
/// plus 2 x 16 bits per run.
#[inline]
pub fn run_bytes(runs: usize) -> usize {
    2 + 4 * runs
}

/// Serialized size of the smaller of the array and bitmap forms.
#[inline]
fn array_or_bitmap_bytes(card: usize) -> usize {
    (2 * card).min(BITMAP_BYTES)
}

pub fn zeroed_words() -> Box<Words> {
    vec![0u64; WORDS].into_boxed_slice().try_into().unwrap()
}

/// A new bitmap with word `i` equal to `f(i)`.
#[inline]
fn words_from_fn(f: impl Fn(usize) -> u64) -> Box<Words> {
    let mut b = Box::<Words>::new_uninit();
    let p = b.as_mut_ptr() as *mut u64;
    for i in 0..WORDS {
        // SAFETY: i < WORDS, and every word is written before assume_init.
        unsafe { p.add(i).write(f(i)) };
    }
    // SAFETY: fully initialised above.
    unsafe { b.assume_init() }
}

/// Sets word `i` of the bitmap in `slot` to `f(i)`, allocating it (without
/// zeroing) if the slot is empty.
#[inline]
fn fill_words(slot: &mut Option<Box<Words>>, f: impl Fn(usize) -> u64) -> &mut Words {
    match slot {
        Some(w) => {
            for (i, x) in w.iter_mut().enumerate() {
                *x = f(i);
            }
            w
        }
        None => slot.insert(words_from_fn(f)),
    }
}

#[inline]
fn set_bit(w: &mut Words, v: u16) {
    w[(v >> 6) as usize] |= 1 << (v & 63);
}

/// Calls `f(word, mask)` for every word overlapping the bit range `lo..=hi`.
#[inline]
fn for_range_words(lo: u16, hi: u16, mut f: impl FnMut(usize, u64)) {
    let (wl, wh) = ((lo >> 6) as usize, (hi >> 6) as usize);
    let ml = !0u64 << (lo & 63);
    let mh = !0u64 >> (63 - (hi & 63));
    if wl == wh {
        f(wl, ml & mh);
    } else {
        f(wl, ml);
        for i in wl + 1..wh {
            f(i, !0);
        }
        f(wh, mh);
    }
}

#[inline]
fn set_range(w: &mut Words, r: Run) {
    for_range_words(r.start, r.last(), |i, m| w[i] |= m);
}

#[inline]
fn popcount_range(w: &Words, r: Run) -> u32 {
    let mut c = 0;
    for_range_words(r.start, r.last(), |i, m| c += (w[i] & m).count_ones());
    c
}

#[inline]
pub fn popcount(w: &Words) -> u32 {
    w.iter().map(|x| x.count_ones() as u64).sum::<u64>() as u32
}

#[inline]
fn and_card(x: &Words, y: &Words) -> u32 {
    x.iter().zip(y.iter()).map(|(a, b)| (a & b).count_ones() as u64).sum::<u64>() as u32
}

/// Appends `base + 64 * i + b` for every set bit `b` of `word(i)`, `i` in
/// `words`.
///
/// # Safety
/// `out` must have spare capacity for those values plus [`EXTRACT_SLACK`].
#[inline(always)]
unsafe fn extract<T: BitPos>(words: Range<usize>, word: impl Fn(usize) -> u64, base: u32, out: &mut Vec<T>) {
    let start = out.as_mut_ptr();
    let end = T::extract(words, word, base, start.add(out.len()));
    out.set_len(end.offset_from(start) as usize);
}

/// Appends `base + v` for every set bit `v` of `w` inside the run `r`.
///
/// # Safety
/// As for [`extract`].
#[inline(always)]
unsafe fn extract_run<T: BitPos>(w: &Words, r: Run, base: u32, out: &mut Vec<T>) {
    let (wl, wh) = ((r.start >> 6) as usize, (r.last() >> 6) as usize);
    let ml = !0u64 << (r.start & 63);
    let mh = !0u64 >> (63 - (r.last() & 63));
    let word = |i: usize| {
        let mut x = w[i];
        if i == wl {
            x &= ml;
        }
        if i == wh {
            x &= mh;
        }
        x
    };
    extract(wl..wh + 1, word, base, out);
}

/// Appends the values of the bitmap `word(0..WORDS)` with `card` set bits.
#[inline(always)]
fn extract_all<T: BitPos>(word: impl Fn(usize) -> u64, card: u32, base: u32, out: &mut Vec<T>) {
    out.reserve(card as usize + EXTRACT_SLACK);
    // SAFETY: `card` values plus the slack are reserved.
    unsafe { extract(0..WORDS, word, base, out) };
}

/// Appends the values of `w` inside `runs`, `card` of them in total.
#[inline(always)]
fn extract_runs<T: BitPos>(w: &Words, runs: &[Run], card: u32, base: u32, out: &mut Vec<T>) {
    out.reserve(card as usize + EXTRACT_SLACK);
    for &r in runs {
        // SAFETY: `card` values over all runs plus the slack are reserved.
        unsafe { extract_run(w, r, base, out) };
    }
}

// Half-open ranges plus the last value: `Range` extends vectorise better
// than `RangeInclusive`, and `last + 1` could overflow.
#[inline]
fn decode_runs(runs: &[Run], base: u32, out: &mut Vec<u32>) {
    for r in runs {
        let last = base + r.last() as u32;
        out.extend(base + r.start as u32..last);
        out.push(last);
    }
}

/// Appends the values of `runs` to `out`.
#[inline]
fn expand_runs(runs: &[Run], out: &mut Vec<u16>) {
    for r in runs {
        out.extend(r.start..r.last());
        out.push(r.last());
    }
}

impl Container {
    /// The smallest container for a chunk: `chunk` holds sorted, distinct,
    /// non-empty docIDs sharing their high 16 bits. Run wins if it is smaller
    /// than both other forms; otherwise array up to 4096 values, else bitmap.
    pub fn from_chunk(chunk: &[u32]) -> Container {
        let n = chunk.len();
        let runs = 1 + chunk.windows(2).filter(|w| w[1] != w[0] + 1).count();
        if run_bytes(runs) < array_or_bitmap_bytes(n) {
            let mut v = Vec::with_capacity(runs);
            let mut start = chunk[0];
            for w in chunk.windows(2) {
                if w[1] != w[0] + 1 {
                    v.push(Run { start: start as u16, len: (w[0] - start) as u16 });
                    start = w[1];
                }
            }
            v.push(Run { start: start as u16, len: (chunk[n - 1] - start) as u16 });
            Container::Run(v)
        } else if n <= ARRAY_MAX {
            Container::Array(chunk.iter().map(|&d| d as u16).collect())
        } else {
            let mut w = zeroed_words();
            for &d in chunk {
                set_bit(&mut w, d as u16);
            }
            Container::Bitmap(w, n as u32)
        }
    }

    /// The smallest container holding the (sorted, maximal, non-empty) `runs`.
    pub fn from_runs(runs: &[Run]) -> Container {
        let card: u32 = runs.iter().map(|r| r.card()).sum();
        if run_bytes(runs.len()) < array_or_bitmap_bytes(card as usize) {
            Container::Run(runs.to_vec())
        } else if card as usize <= ARRAY_MAX {
            let mut a = Vec::with_capacity(card as usize);
            expand_runs(runs, &mut a);
            Container::Array(a)
        } else {
            let mut w = zeroed_words();
            for &r in runs {
                set_range(&mut w, r);
            }
            Container::Bitmap(w, card)
        }
    }

    /// All 2^16 values.
    pub fn full() -> Container {
        Container::Run(vec![Run { start: 0, len: u16::MAX }])
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        match self {
            Container::Run(r) => r.len() == 1 && r[0].start == 0 && r[0].len == u16::MAX,
            Container::Bitmap(_, card) => *card == 1 << 16,
            Container::Array(_) => false,
        }
    }

    #[inline]
    pub fn card(&self) -> u32 {
        match self {
            Container::Array(a) => a.len() as u32,
            Container::Bitmap(_, card) => *card,
            Container::Run(r) => r.iter().map(|r| r.card()).sum(),
        }
    }

    /// Payload bytes in the portable serialization format.
    #[inline]
    pub fn serialized_bytes(&self) -> usize {
        match self {
            Container::Array(a) => 2 * a.len(),
            Container::Bitmap(..) => BITMAP_BYTES,
            Container::Run(r) => run_bytes(r.len()),
        }
    }

    /// Appends `base | v` for every value `v`, in order.
    pub fn decode(&self, base: u32, out: &mut Vec<u32>) {
        match self {
            Container::Array(a) => out.extend(a.iter().map(|&v| base | v as u32)),
            Container::Bitmap(w, card) => extract_all(|i| w[i], *card, base, out),
            Container::Run(r) => decode_runs(r, base, out),
        }
    }
}

/// Buffers reused across containers and calls.
#[derive(Default)]
pub struct Scratch {
    /// Array results.
    pub arr: Vec<u16>,
    /// Run results.
    pub runs: Vec<Run>,
    /// Bitmap results of unions (taken when materialised).
    words: Option<Box<Words>>,
    tmp: Vec<u16>,
    tmp2: Vec<u16>,
}

/// Result of [`and`].
pub enum AndOut<'a> {
    Empty,
    /// In `Scratch::arr`.
    Array,
    /// In `Scratch::runs`.
    Runs,
    /// Equal to this operand (the other one was full).
    Same(&'a Container),
    /// `x & y`, with its cardinality (more than [`ARRAY_MAX`]).
    BitAnd(&'a Words, &'a Words, u32),
    /// A bitmap restricted to runs, with its cardinality (more than [`ARRAY_MAX`]).
    BitRuns(&'a Words, &'a [Run], u32),
}

#[inline]
fn array_out<'a>(s: &Scratch) -> AndOut<'a> {
    if s.arr.is_empty() {
        AndOut::Empty
    } else {
        AndOut::Array
    }
}

/// Intersection of two containers.
#[inline]
pub fn and<'a>(a: &'a Container, b: &'a Container, s: &mut Scratch) -> AndOut<'a> {
    use Container::*;
    match (a, b) {
        (Array(x), Array(y)) => {
            simd::intersect(x, y, &mut s.arr);
            array_out(s)
        }
        (Array(x), Bitmap(w, _)) | (Bitmap(w, _), Array(x)) => {
            array_and_bitmap(x, w, &mut s.arr);
            array_out(s)
        }
        (Bitmap(x, _), Bitmap(y, _)) => {
            let card = and_card(x, y);
            if card as usize > ARRAY_MAX {
                return AndOut::BitAnd(x, y, card);
            }
            s.arr.clear();
            extract_all(|i| x[i] & y[i], card, 0, &mut s.arr);
            array_out(s)
        }
        _ if a.is_full() => AndOut::Same(b),
        _ if b.is_full() => AndOut::Same(a),
        (Array(x), Run(r)) | (Run(r), Array(x)) => {
            array_and_runs(x, r, &mut s.arr);
            array_out(s)
        }
        (Bitmap(w, _), Run(r)) | (Run(r), Bitmap(w, _)) => {
            let card: u32 = r.iter().map(|&run| popcount_range(w, run)).sum();
            if card as usize > ARRAY_MAX {
                return AndOut::BitRuns(w, r, card);
            }
            s.arr.clear();
            extract_runs(w, r, card, 0, &mut s.arr);
            array_out(s)
        }
        (Run(x), Run(y)) => {
            runs_and_runs(x, y, &mut s.runs);
            if s.runs.is_empty() {
                AndOut::Empty
            } else {
                AndOut::Runs
            }
        }
    }
}

impl AndOut<'_> {
    /// The result as a new container (`None` if empty).
    pub fn materialize(self, s: &Scratch) -> Option<Container> {
        Some(match self {
            AndOut::Empty => return None,
            AndOut::Array => Container::Array(s.arr.clone()),
            AndOut::Runs => Container::from_runs(&s.runs),
            AndOut::Same(c) => c.clone(),
            AndOut::BitAnd(x, y, card) => Container::Bitmap(words_from_fn(|i| x[i] & y[i]), card),
            AndOut::BitRuns(w, runs, card) => {
                let mut out = zeroed_words();
                for &r in runs {
                    for_range_words(r.start, r.last(), |i, m| out[i] |= w[i] & m);
                }
                Container::Bitmap(out, card)
            }
        })
    }

    /// Appends `base | v` for every value of the result.
    pub fn decode(self, base: u32, s: &Scratch, out: &mut Vec<u32>) {
        match self {
            AndOut::Empty => {}
            AndOut::Array => out.extend(s.arr.iter().map(|&v| base | v as u32)),
            AndOut::Runs => decode_runs(&s.runs, base, out),
            AndOut::Same(c) => c.decode(base, out),
            AndOut::BitAnd(x, y, card) => extract_all(|i| x[i] & y[i], card, base, out),
            AndOut::BitRuns(w, runs, card) => extract_runs(w, runs, card, base, out),
        }
    }
}

/// `out = a ∩ bitmap` by probing one bit per array value (branch-free).
#[inline]
fn array_and_bitmap(a: &[u16], w: &Words, out: &mut Vec<u16>) {
    out.clear();
    out.reserve(a.len());
    let mut k = 0;
    // SAFETY: at most a.len() values are written, all reserved.
    unsafe {
        let dst = out.as_mut_ptr();
        for &v in a {
            *dst.add(k) = v;
            k += ((w[(v >> 6) as usize] >> (v & 63)) & 1) as usize;
        }
        out.set_len(k);
    }
}

/// `out = a ∩ runs` with a moving pointer. With fewer runs than values, each
/// run's slice of `a` is located by galloping and copied whole; otherwise each
/// value gallops over the run ends.
fn array_and_runs(a: &[u16], runs: &[Run], out: &mut Vec<u16>) {
    out.clear();
    if runs.len() <= a.len() {
        let mut pos = 0;
        for r in runs {
            let lo = gallop(a, pos, r.start);
            if lo == a.len() {
                break;
            }
            let last = r.last();
            pos = gallop_by(a, lo, |&x| x <= last);
            out.extend_from_slice(&a[lo..pos]);
        }
    } else {
        let mut ri = 0;
        for &v in a {
            ri = gallop_by(runs, ri, |r| r.last() < v);
            match runs.get(ri) {
                None => break,
                Some(r) if r.start <= v => out.push(v),
                Some(_) => {}
            }
        }
    }
}

/// `out = x ∩ y` for two run lists (interval intersection). Maximal inputs
/// give maximal outputs.
fn runs_and_runs(x: &[Run], y: &[Run], out: &mut Vec<Run>) {
    out.clear();
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        let (a, b) = (x[i], y[j]);
        let lo = a.start.max(b.start);
        let hi = a.last().min(b.last());
        if lo <= hi {
            out.push(Run { start: lo, len: hi - lo });
        }
        if a.last() <= b.last() {
            i += 1;
        }
        if b.last() <= a.last() {
            j += 1;
        }
    }
}

/// Result of [`or_many`].
pub enum OrOut<'a> {
    /// Equal to this container (single input, or a full one).
    Same(&'a Container),
    /// In `Scratch::arr`.
    Array,
    /// In `Scratch::runs`.
    Runs,
    /// In `Scratch::words`, with its cardinality.
    Bitmap(u32),
    /// All 2^16 values.
    Full,
}

/// Union of the containers of one key (CRoaring's lazy union). Run-only
/// groups union their intervals; groups of at most 4096 values in total are
/// merged as arrays; anything else is ORed into one bitmap whose cardinality
/// is computed once at the end.
pub fn or_many<'a>(group: &[&'a Container], s: &mut Scratch) -> OrOut<'a> {
    use Container::*;
    if group.len() == 1 {
        return OrOut::Same(group[0]);
    }
    if let Some(full) = group.iter().find(|c| c.is_full()) {
        return OrOut::Same(full);
    }
    let mut total = 0usize;
    let mut all_runs = true;
    for c in group {
        total += c.card() as usize;
        all_runs &= matches!(c, Run(_));
    }

    if all_runs {
        union_runs(group, &mut s.runs);
        return OrOut::Runs;
    }

    if total <= ARRAY_MAX {
        if let [Array(x), Array(y)] = group {
            simd::union(x, y, &mut s.arr);
            return OrOut::Array;
        }
        // Bitmaps hold more than ARRAY_MAX values, so only arrays and runs
        // reach this branch.
        let Scratch { arr, tmp, tmp2, .. } = s;
        arr.clear();
        match group[0] {
            Array(x) => arr.extend_from_slice(x),
            Run(r) => expand_runs(r, arr),
            Bitmap(..) => unreachable!(),
        }
        for c in &group[1..] {
            let other: &[u16] = match c {
                Array(x) => x,
                Run(r) => {
                    tmp.clear();
                    expand_runs(r, tmp);
                    tmp
                }
                Bitmap(..) => unreachable!(),
            };
            simd::union(arr, other, tmp2);
            std::mem::swap(arr, tmp2);
        }
        return OrOut::Array;
    }

    let mut bitmaps = group.iter().filter_map(|c| match c {
        Bitmap(x, _) => Some(x),
        _ => None,
    });
    let w = match (bitmaps.next(), bitmaps.next()) {
        (Some(x), Some(y)) => {
            let w = fill_words(&mut s.words, |i| x[i] | y[i]);
            for z in bitmaps {
                for i in 0..WORDS {
                    w[i] |= z[i];
                }
            }
            w
        }
        (Some(x), None) => fill_words(&mut s.words, |i| x[i]),
        _ => fill_words(&mut s.words, |_| 0),
    };
    for c in group {
        match c {
            Array(x) => {
                for &v in x {
                    set_bit(w, v);
                }
            }
            Run(r) => {
                for &run in r {
                    set_range(w, run);
                }
            }
            Bitmap(..) => {}
        }
    }
    let card = popcount(w);
    if card as usize <= ARRAY_MAX {
        s.arr.clear();
        extract_all(|i| w[i], card, 0, &mut s.arr);
        OrOut::Array
    } else if card == 1 << 16 {
        OrOut::Full
    } else {
        OrOut::Bitmap(card)
    }
}

/// Union of the run lists in `group` (all run containers), coalescing
/// overlapping and adjacent runs.
fn union_runs(group: &[&Container], out: &mut Vec<Run>) {
    out.clear();
    for c in group {
        if let Container::Run(r) = c {
            out.extend_from_slice(r);
        }
    }
    out.sort_unstable_by_key(|r| r.start);
    let mut w = 0;
    for i in 1..out.len() {
        let r = out[i];
        let cur = &mut out[w];
        if r.start as u32 <= cur.last() as u32 + 1 {
            cur.len = cur.last().max(r.last()) - cur.start;
        } else {
            w += 1;
            out[w] = r;
        }
    }
    out.truncate(w + 1);
}

impl OrOut<'_> {
    /// The result as a new container.
    pub fn materialize(self, s: &mut Scratch) -> Container {
        match self {
            OrOut::Same(c) => c.clone(),
            OrOut::Array => Container::Array(s.arr.clone()),
            OrOut::Runs => Container::from_runs(&s.runs),
            OrOut::Bitmap(card) => Container::Bitmap(s.words.take().unwrap(), card),
            OrOut::Full => Container::full(),
        }
    }

    /// Appends `base | v` for every value of the result.
    pub fn decode(self, base: u32, s: &Scratch, out: &mut Vec<u32>) {
        match self {
            OrOut::Same(c) => c.decode(base, out),
            OrOut::Array => out.extend(s.arr.iter().map(|&v| base | v as u32)),
            OrOut::Runs => decode_runs(&s.runs, base, out),
            OrOut::Bitmap(card) => {
                let w = s.words.as_deref().unwrap();
                extract_all(|i| w[i], card, base, out);
            }
            OrOut::Full => decode_runs(&[Run { start: 0, len: u16::MAX }], base, out),
        }
    }
}
