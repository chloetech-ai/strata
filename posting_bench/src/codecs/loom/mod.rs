//! Loom: word-aligned partitioned Elias-Fano with partition-native set
//! algebra.
//!
//! **Layout.** As in PEF ([`super::pef`]), the list is cut into partitions by
//! an exact dynamic program (boundaries every [`Params::chunk`] = 16
//! postings, at most 4096 postings per partition); partition `p` covers the
//! docIDs `[base, last]`, `base` being one past the previous partition's
//! last posting, and is stored as
//!
//! * **run**: exactly `[base, last]`, no payload;
//! * **bitvector**: bit `i` of the payload is docID `abase + i`, where
//!   `abase = base & !63`. Bitvectors are therefore aligned to docID words:
//!   payload word `w` covers docIDs `[abase + 64w, abase + 64w + 64)`, a
//!   word of the docID space, so two lists' bitvectors AND or OR word by
//!   word with no shifting (the cost is at most 63 leading zero bits per
//!   partition, which the DP accounts for);
//! * **Elias-Fano**: `ef`, laid out inline in the list's single word array
//!   (low bits, high bits, then partition-local 16-bit select0 samples every
//!   256 zeros where PEF uses 32-bit ones).
//!
//! **Space-time knob λ** ([`Params::lambda`], default 5/4): a partition is a
//! bitvector when that is at most `λ` times its EF size, and the DP
//! minimises `Σ (64 + min(bitvector / λ, EF))`; `λ = 1` minimises space.
//!
//! **Upper level**: per partition the last docID (`u32`, galloped over) and
//! a descriptor (encoding, payload word offset, size, EF low bits). It is
//! accounted as PEF's 64 bits per partition plus 12 bits of size for EF
//! partitions (run and bitvector partitions do not need theirs).
//! [`PostingList::size_bits`] is payload plus upper level, like PEF's.
//!
//! **Decoding** EF partitions is vectorised (`ef::EfCur::decode`): high
//! bits are extracted with `vpcompressd` like bitmaps, low bits unpacked 16
//! at a time with `vpermb` and a variable shift.
//!
//! **Set algebra** (see the `and` and `or` modules for details):
//! * AND: the shortest list is decoded in batches; every other list (rarest
//!   first) filters the batch in place, partition by partition: bit test
//!   for bitvectors, range check for runs, and for EF a choice between
//!   decode + SIMD merge (similar densities), independent sample-based
//!   probes (much denser list) and forward probes in 2-4 interleaved streams
//!   that also skip candidates in the list's gaps (leapfrog). Early exit,
//!   and the driver jumps ahead when a batch dies. Where the driver's
//!   partition is a bitvector or run, candidates are formed word-parallel
//!   by ANDing the other lists' aligned bitvector words.
//! * OR: a 2^16-docID window accumulator; per window every list ORs its
//!   bitvector words, fills its runs and sets its (SIMD-decoded) EF
//!   postings, then the window is extracted (`vpcompressd`) and cleared.
//! * [`Loom::and_count`] / [`Loom::or_count`] return cardinalities only.
//!
//! **Storage.** The arrays are [`Slice`]s: owned when built, zero-copy views
//! when read back with [`Loom::from_bytes`] from what [`Loom::write_to`]
//! wrote (e.g. a memory-mapped index file), so every algorithm runs on
//! either. Besides the upper level, a list keeps the rank of every
//! partition's first posting (32 bits per partition, not part of
//! [`PostingList::size_bits`], which stays comparable with the other codecs)
//! so that [`LoomCursor::rank`] can index arrays aligned with the postings,
//! such as term frequencies.

mod and;
mod ef;
mod or;
mod simd;

use std::borrow::Borrow;

use super::uncompressed::gallop_geq;
use super::{Cursor, CursorList, PostingList, SetOps};
use crate::bytes::{ByteReader, ByteWriter, OwnedBytes, Pod, Slice};
use crate::END;

/// Partition encodings.
pub const RUN: u32 = 0;
pub const BV: u32 = 1;
pub const EF: u32 = 2;

/// Upper-level bits per partition: last docID (32) and descriptor (32).
pub const FIXED_COST: u64 = 64;
/// Extra upper-level bits of an EF partition: its size (`n - 1 < 4096`).
pub const EF_SIZE_BITS: u64 = 12;
/// Maximum postings per partition.
pub const MAX_PART: usize = ef::MAX_N as usize;
/// Zero words after the payloads: unaligned 8-byte low-bit reads, the
/// SIMD decoder's 64-byte loads and the probes' 8-word loads stay inside.
const PAD_WORDS: usize = 9;

/// Build parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// DP boundaries fall on multiples of this many postings.
    pub chunk: usize,
    /// `λ = lambda.0 / lambda.1 >= 1`.
    pub lambda: (u64, u64),
}

impl Params {
    pub const DEFAULT: Params = Params { chunk: 16, lambda: (5, 4) };
}

impl Default for Params {
    fn default() -> Self {
        Self::DEFAULT
    }
}

// Descriptor layout.
const OFF_MASK: u64 = (1 << 32) - 1;
const KIND_SHIFT: u32 = 32;
const N_SHIFT: u32 = 34;
const L_SHIFT: u32 = 48;

/// A posting list stored as Loom.
#[derive(Clone, Debug, Default)]
pub struct Loom {
    n: usize,
    /// Last docID of every partition.
    ends: Slice<u32>,
    /// Descriptor of every partition: payload word offset (bits 0..32),
    /// encoding (32..34), `n - 1` (34..47), EF low bits (48..54).
    desc: Slice<u64>,
    /// Postings before every partition.
    ranks: Slice<u32>,
    /// All payloads, then [`PAD_WORDS`] zero words.
    words: Slice<u64>,
    size_bits: u64,
}

/// Encoding and payload size in bits (including the EF size field) of a
/// partition of `m` postings over `[base, end)`, with `λ = ln / ld`. The DP
/// in [`partition`] makes the same choice, inlined so that it vectorises.
#[inline(always)]
fn choose(base: u64, end: u64, m: u64, ln: u64, ld: u64) -> (u32, u64) {
    let span = end - base;
    let ef = ef::cost_bits(span, m) + EF_SIZE_BITS;
    let bv = end - (base & !63);
    if span == m {
        (RUN, 0)
    } else if bv * ld <= ef * ln {
        (BV, bv)
    } else {
        (EF, ef)
    }
}

/// End indices of the partitions minimising `Σ fixed + effective cost`, by
/// dynamic programming over boundaries at multiples of `chunk` postings.
fn partition(docs: &[u32], params: Params) -> Vec<usize> {
    let (chunk, (ln, ld)) = (params.chunk, params.lambda);
    assert!(chunk >= 1 && MAX_PART % chunk == 0 && MAX_PART / chunk <= 4096 && ln >= ld && ld >= 1);
    let n = docs.len();
    let nb = n.div_ceil(chunk);
    let pos = |k: usize| (k * chunk).min(n);
    let bases: Vec<u64> = (0..nb).map(|k| if k == 0 { 0 } else { docs[k * chunk - 1] as u64 + 1 }).collect();
    let mut best = vec![0u64; nb + 1];
    let mut from = vec![0u32; nb + 1];
    let max_chunks = MAX_PART / chunk;
    for j in 1..=nb {
        let pj = pos(j);
        let end = docs[pj - 1] as u64 + 1;
        let lo = j.saturating_sub(max_chunks);
        // Minimise (cost << 12 | j - i - 1): ties go to the shortest partition.
        let mut key = u64::MAX;
        for i in lo..j {
            let m = (pj - i * chunk) as u64;
            let base = bases[i];
            let span = end - base;
            let ef = (ef::cost_bits(span, m) + EF_SIZE_BITS) * ln;
            let bv = (end - (base & !63)) * ld;
            let eff = if span == m { 0 } else { ef.min(bv) };
            key = key.min((best[i] + eff) << 12 | (j - i - 1) as u64);
        }
        best[j] = (key >> 12) + FIXED_COST * ln;
        from[j] = (j - 1 - (key & 0xfff) as usize) as u32;
    }
    let mut cuts = Vec::new();
    let mut j = nb;
    while j > 0 {
        cuts.push(pos(j));
        j = from[j] as usize;
    }
    cuts.reverse();
    cuts
}

impl Loom {
    /// Build with explicit parameters.
    pub fn build_with(docs: &[u32], params: Params) -> Self {
        let mut words: Vec<u64> = Vec::new();
        if docs.is_empty() {
            words.extend([0; PAD_WORDS]);
            return Loom { words: words.into(), ..Self::default() };
        }
        assert!(docs.len() <= u32::MAX as usize, "list too long");
        let cuts = partition(docs, params);
        let mut ends = Vec::with_capacity(cuts.len());
        let mut desc = Vec::with_capacity(cuts.len());
        let mut ranks = Vec::with_capacity(cuts.len());
        let (mut start, mut base) = (0usize, 0u32);
        let mut bits = 0u64;
        for end in cuts {
            let part = &docs[start..end];
            let m = part.len();
            let last = part[m - 1];
            let (kind, real) = choose(base as u64, last as u64 + 1, m as u64, params.lambda.0, params.lambda.1);
            let off = words.len();
            let mut l = 0u32;
            match kind {
                RUN => {}
                BV => {
                    let abase = base & !63;
                    words.resize(off + ((last - abase) / 64 + 1) as usize, 0);
                    for &d in part {
                        let r = (d - abase) as usize;
                        words[off + r / 64] |= 1u64 << (r % 64);
                    }
                }
                _ => {
                    let span = (last - base) as u64 + 1;
                    let b = ef::encode(part, base, span, &mut words);
                    debug_assert_eq!(b + EF_SIZE_BITS, real);
                    l = ef::lower_bits(span, m as u64);
                }
            }
            assert!(off < 1 << 32, "list payload too large");
            desc.push(off as u64 | (kind as u64) << KIND_SHIFT | ((m - 1) as u64) << N_SHIFT | (l as u64) << L_SHIFT);
            ends.push(last);
            ranks.push(start as u32);
            bits += FIXED_COST + real;
            start = end;
            base = last + 1;
        }
        // Padding for unaligned low-bit reads (up to 64 bytes for the SIMD
        // decoder) and look-ahead word loads.
        words.extend([0; PAD_WORDS]);
        words.shrink_to_fit();
        Loom { n: docs.len(), ends: ends.into(), desc: desc.into(), ranks: ranks.into(), words: words.into(), size_bits: bits }
    }

    /// Appends the list to `w`; [`Loom::read_from`] views it in place.
    ///
    /// Layout (every array 8-byte aligned, see [`ByteWriter::array`]):
    /// `n`, `size_bits`, partition ends, descriptors, ranks, payload words
    /// (with their zero padding).
    pub fn write_to(&self, w: &mut ByteWriter) {
        w.u64(self.n as u64);
        w.u64(self.size_bits);
        w.array(&self.ends);
        w.array(&self.desc);
        w.array(&self.ranks);
        w.array(&self.words);
    }

    /// The serialised list (see [`Loom::write_to`]).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = ByteWriter::new();
        self.write_to(&mut w);
        w.finish()
    }

    /// Views a list written by [`Loom::write_to`] without copying: every
    /// array is a [`Slice::Shared`] into `bytes`, which must be 8-byte
    /// aligned. Checks the structure (array lengths, padding) but trusts the
    /// partition descriptors, as the codec trusts its own build; data from
    /// untrusted storage must be checksummed by the caller.
    pub fn from_bytes(bytes: OwnedBytes) -> Result<Self, String> {
        Self::read_from(&mut ByteReader::new(bytes))
    }

    /// Reads one list at the reader's position (see [`Loom::from_bytes`]).
    pub fn read_from(r: &mut ByteReader) -> Result<Self, String> {
        let n = r.u64()? as usize;
        let size_bits = r.u64()?;
        let ends: Slice<u32> = r.array()?;
        let desc: Slice<u64> = r.array()?;
        let ranks: Slice<u32> = r.array()?;
        let words: Slice<u64> = r.array()?;
        let p = ends.len();
        if desc.len() != p || ranks.len() != p || words.len() < PAD_WORDS || (n == 0) != (p == 0) || n > u32::MAX as usize {
            return Err(format!("inconsistent Loom list: {n} postings, {p} partitions, {} words", words.len()));
        }
        if p > 0 {
            let d = desc[p - 1];
            let last_n = ((d >> N_SHIFT) & 0x1fff) as usize + 1;
            if ranks[p - 1] as usize + last_n != n || (d & OFF_MASK) as usize >= words.len() {
                return Err("Loom list ranks or offsets out of range".into());
            }
        }
        Ok(Loom { n, ends, desc, ranks, words, size_bits })
    }

    /// Whether every array is a zero-copy view (as after [`Loom::from_bytes`]).
    pub fn is_shared(&self) -> bool {
        fn shared<T: Pod>(s: &Slice<T>) -> bool {
            matches!(s, Slice::Shared { .. })
        }
        shared(&self.ends) && shared(&self.desc) && shared(&self.ranks) && shared(&self.words)
    }

    /// A cursor that owns the list (a view is cheap to move), for callers
    /// that keep cursors beyond the scope of a borrowed list.
    pub fn into_cursor(self) -> LoomCursor<Loom> {
        LoomCursor::new(self)
    }

    /// Number of partitions.
    pub fn num_partitions(&self) -> usize {
        self.ends.len()
    }

    /// `(base, last, encoding, postings)` of every partition.
    pub fn partitions(&self) -> impl Iterator<Item = (u32, u32, u32, usize)> + '_ {
        (0..self.ends.len()).map(move |p| {
            let d = self.desc[p];
            (self.base(p), self.ends[p], (d >> KIND_SHIFT) as u32 & 3, ((d >> N_SHIFT) & 0x1fff) as usize + 1)
        })
    }

    #[inline(always)]
    fn base(&self, p: usize) -> u32 {
        if p == 0 {
            0
        } else {
            self.ends[p - 1] + 1
        }
    }

    /// Cardinality of the intersection of `lists` (no materialisation).
    pub fn and_count(lists: &[&Loom]) -> u64 {
        let mut sink = and::CountSink(0);
        and::intersect(lists, &mut sink);
        sink.0
    }

    /// Cardinality of the union of `lists` (no materialisation).
    pub fn or_count(lists: &[&Loom]) -> u64 {
        let mut sink = or::CountSink(0);
        or::union(lists, &mut sink);
        sink.0
    }
}

/// A loaded partition. Raw pointers point into the list's `words`, which
/// outlives every user (cursors borrow the list).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Part {
    pub base: u32,
    pub last: u32,
    pub kind: u32,
    /// Bitvector: docID of bit 0 (`base & !63`).
    pub abase: u32,
    /// Postings in the partition.
    pub n: u32,
    /// Bitvector words.
    pub bits: *const u64,
    /// EF cursor (before its first value).
    pub ef: ef::EfCur,
}

impl Part {
    /// Placeholder for an exhausted or empty list.
    #[inline]
    pub fn empty() -> Self {
        Part { base: 0, last: END, kind: RUN, abase: 0, n: 0, bits: ef::ZEROS.as_ptr(), ef: ef::EfCur::dummy() }
    }

    /// Partition `p` of `list` (which must exist).
    #[inline(always)]
    pub fn load(list: &Loom, p: usize) -> Self {
        debug_assert!(p < list.ends.len());
        // SAFETY: p is in bounds (all per-partition arrays have equal length).
        let (base, last, d) = unsafe {
            (
                if p == 0 { 0 } else { *list.ends.get_unchecked(p - 1) + 1 },
                *list.ends.get_unchecked(p),
                *list.desc.get_unchecked(p),
            )
        };
        let kind = (d >> KIND_SHIFT) as u32 & 3;
        let n = ((d >> N_SHIFT) & 0x1fff) as u32 + 1;
        // SAFETY: offsets come from the build and are inside `words`.
        let data = unsafe { list.words.as_ptr().add((d & OFF_MASK) as usize) };
        let ef = if kind == EF {
            let l = (d >> L_SHIFT) as u32 & 63;
            let span = (last - base) as u64 + 1;
            let buckets = ((span - 1) >> l) + 1;
            let low_words = (n as usize * l as usize).div_ceil(64);
            let high_words = (n as usize + buckets as usize).div_ceil(64);
            // SAFETY: the partition was written by `ef::encode` with this
            // layout and is followed by at least the padding word.
            unsafe { ef::EfCur::new(data, l, low_words, high_words) }
        } else {
            ef::EfCur::dummy()
        };
        Part { base, last, kind, abase: base & !63, n, bits: data, ef }
    }

    /// A membership probe over this EF partition.
    #[inline(always)]
    pub fn ef_probe(&self) -> ef::EfProbe {
        ef::EfProbe::new(&self.ef)
    }

    /// Bitvector: is docID `x` (in `[base, last]`) set?
    #[inline(always)]
    pub fn bv_test(&self, x: u32) -> bool {
        let r = (x - self.abase) as usize;
        // SAFETY: x <= last, so its word is inside the partition.
        unsafe { (*self.bits.add(r / 64) >> (r % 64)) & 1 != 0 }
    }

    /// Bitvector: first set docID `>= x` (`x <= last`).
    #[inline(always)]
    pub fn bv_next_geq(&self, x: u32) -> u32 {
        let r = (x - self.abase) as usize;
        let mut wi = r / 64;
        // SAFETY: the bit of `last` is set and at or after `x`.
        unsafe {
            let mut w = *self.bits.add(wi) & (!0u64 << (r % 64));
            while w == 0 {
                wi += 1;
                w = *self.bits.add(wi);
            }
            self.abase + (wi * 64) as u32 + w.trailing_zeros()
        }
    }
}

impl PostingList for Loom {
    const NAME: &'static str = "Loom";

    fn build(docs: &[u32], _universe: u32) -> Self {
        Self::build_with(docs, Params::DEFAULT)
    }

    fn len(&self) -> usize {
        self.n
    }

    fn size_bits(&self) -> u64 {
        self.size_bits
    }

    fn decode_into(&self, out: &mut Vec<u32>) {
        out.reserve(self.n + simd::SLACK);
        for p in 0..self.ends.len() {
            let part = Part::load(self, p);
            match part.kind {
                RUN => out.extend(part.base..=part.last),
                BV => {
                    let words = ((part.last - part.abase) / 64 + 1) as usize;
                    // SAFETY: the bitvector has `words` words.
                    let ws = unsafe { std::slice::from_raw_parts(part.bits, words) };
                    out.reserve(words * 64 + simd::SLACK);
                    // SAFETY: room for every bit of the partition plus slack.
                    unsafe {
                        let end = simd::extract(ws, part.abase, out.as_mut_ptr().add(out.len()));
                        out.set_len(end.offset_from(out.as_ptr()) as usize);
                    }
                }
                _ => {
                    let mut c = part.ef;
                    let n = part.n as usize;
                    out.reserve(n + ef::DECODE_SLACK);
                    // SAFETY: n >= 1 values follow the fresh cursor, and
                    // `out` has room for them plus the decoder's slack.
                    unsafe {
                        c.decode(n, part.base, out.as_mut_ptr().add(out.len()));
                        out.set_len(out.len() + n);
                    }
                }
            }
        }
    }
}

/// First partition at or after `from` whose last docID is `>= target`.
#[inline(never)]
fn find_partition(ends: &[u32], from: usize, target: u32) -> usize {
    gallop_geq(ends, from, target)
}

/// Cursor over a [`Loom`] list, holding it as `L`: borrowed (`&Loom`, what
/// [`CursorList::cursor`] returns) or owned (`Loom`, from
/// [`Loom::into_cursor`]).
pub struct LoomCursor<L: Borrow<Loom>> {
    list: L,
    cur: u32,
    p: usize,
    part: Part,
    /// Bitvector scan state: word index of `cur` and the bits after it.
    wi: usize,
    buf: u64,
    /// Bitvector rank state, valid for partition `rank_p`: the set bits in
    /// the words before word `rank_wi` (see [`LoomCursor::rank`]).
    rank_p: usize,
    rank_wi: usize,
    rank_acc: usize,
}

impl<L: Borrow<Loom>> LoomCursor<L> {
    fn new(list: L) -> Self {
        let n = list.borrow().n;
        let mut c =
            LoomCursor { list, cur: END, p: 0, part: Part::empty(), wi: 0, buf: 0, rank_p: usize::MAX, rank_wi: 0, rank_acc: 0 };
        if n > 0 {
            c.enter(0, 0);
        }
        c
    }

    #[inline(always)]
    fn list(&self) -> &Loom {
        self.list.borrow()
    }

    /// Rank of the current posting (the number of postings before it), or
    /// the list length once exhausted.
    ///
    /// Constant time for run and EF partitions. In a bitvector partition it
    /// counts the set bits of the words before the current one, resuming
    /// from where the previous call in the same partition stopped, so a
    /// pass that asks for every rank costs one popcount per word.
    #[inline]
    pub fn rank(&mut self) -> usize {
        if self.cur == END {
            return self.list().n;
        }
        // SAFETY: p < num_partitions while the cursor is on a posting.
        let first = unsafe { *self.list().ranks.get_unchecked(self.p) } as usize;
        let part = &self.part;
        first
            + match part.kind {
                RUN => (self.cur - part.base) as usize,
                BV => {
                    if self.rank_p != self.p {
                        (self.rank_p, self.rank_wi, self.rank_acc) = (self.p, 0, 0);
                    }
                    // SAFETY: words up to `wi` (the current posting's) are
                    // inside the partition.
                    unsafe {
                        while self.rank_wi < self.wi {
                            self.rank_acc += (*part.bits.add(self.rank_wi)).count_ones() as usize;
                            self.rank_wi += 1;
                        }
                        let w = *part.bits.add(self.wi);
                        // Bits of the word below `cur`: all but `cur` and those after it.
                        self.rank_acc + (w.count_ones() - self.buf.count_ones() - 1) as usize
                    }
                }
                _ => part.ef.idx,
            }
    }

    /// Load partition `p` and move to its first posting `>= target`
    /// (`target <= last`).
    #[inline(never)]
    fn enter(&mut self, p: usize, target: u32) -> u32 {
        self.p = p;
        self.part = Part::load(self.list.borrow(), p);
        let t = target.max(self.part.base);
        self.seek_in(t)
    }

    /// Move to the first posting `>= t` of the current partition, where
    /// `cur < t <= last` (or the partition was just loaded and `base <= t`).
    #[inline(always)]
    fn seek_in(&mut self, t: u32) -> u32 {
        let part = &mut self.part;
        self.cur = match part.kind {
            RUN => t,
            BV => {
                let r = (t - part.abase) as usize;
                self.wi = r / 64;
                // SAFETY: `last` is set and at or after `t`.
                unsafe {
                    let mut w = *part.bits.add(self.wi) & (!0u64 << (r % 64));
                    while w == 0 {
                        self.wi += 1;
                        w = *part.bits.add(self.wi);
                    }
                    self.buf = w & (w - 1);
                    part.abase + (self.wi * 64) as u32 + w.trailing_zeros()
                }
            }
            // SAFETY: t - base <= last - base (the last value), and above the
            // current value (or the cursor was just created).
            _ => part.base + unsafe { part.ef.next_geq((t - part.base) as u64) } as u32,
        };
        self.cur
    }

    fn exhaust(&mut self) -> u32 {
        self.cur = END;
        self.part.last = END;
        self.part.kind = RUN;
        self.p = self.list().ends.len();
        END
    }
}

impl<L: Borrow<Loom>> Cursor for LoomCursor<L> {
    #[inline]
    fn docid(&self) -> u32 {
        self.cur
    }

    #[inline(always)]
    fn next(&mut self) -> u32 {
        if self.cur >= self.part.last {
            if self.cur == END || self.p + 1 >= self.list().ends.len() {
                return self.exhaust();
            }
            return self.enter(self.p + 1, 0);
        }
        let part = &mut self.part;
        self.cur = match part.kind {
            RUN => self.cur + 1,
            BV => {
                // SAFETY: a set bit follows (cur < last).
                unsafe {
                    while self.buf == 0 {
                        self.wi += 1;
                        self.buf = *part.bits.add(self.wi);
                    }
                    let b = self.buf;
                    self.buf = b & (b - 1);
                    part.abase + (self.wi * 64) as u32 + b.trailing_zeros()
                }
            }
            // SAFETY: cur < last, so the EF cursor is not on its last value.
            _ => part.base + unsafe { part.ef.next() } as u32,
        };
        self.cur
    }

    #[inline(always)]
    fn next_geq(&mut self, target: u32) -> u32 {
        if target <= self.cur {
            return self.cur;
        }
        if target > self.part.last {
            let ends = &self.list().ends;
            let p = find_partition(ends, self.p + 1, target);
            if p >= ends.len() {
                return self.exhaust();
            }
            return self.enter(p, target);
        }
        self.seek_in(target)
    }

    fn size(&self) -> usize {
        self.list().n
    }
}

impl CursorList for Loom {
    type Cursor<'a> = LoomCursor<&'a Loom>;

    #[inline]
    fn cursor(&self) -> LoomCursor<&Loom> {
        LoomCursor::new(self)
    }
}

impl SetOps for Loom {
    fn intersect(lists: &[&Self], out: &mut Vec<u32>) {
        let mut sink = and::VecSink(out);
        and::intersect(lists, &mut sink);
    }

    fn union(lists: &[&Self], out: &mut Vec<u32>) {
        let mut sink = or::VecSink(out);
        or::union(lists, &mut sink);
    }
}

#[cfg(test)]
mod profile;
#[cfg(test)]
mod tests;
