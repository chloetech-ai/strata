//! Elias-Fano coding of monotone sequences (Elias 1974, Fano 1971), as used
//! for inverted indexes by Vigna, "Quasi-succinct indices" (WSDM 2013).
//!
//! `n` strictly increasing values in `[0, span)` are split at
//! `l = floor(log2(span / n))` bits:
//!
//! * the low `l` bits of every value are stored verbatim, packed;
//! * the high parts are stored in unary: value `i` sets bit `(v_i >> l) + i`
//!   of the high bitvector. Each bucket `h` (the values with high part `h`) is
//!   a run of ones closed by a zero, so bucket `h` starts at `select0(h-1)+1`.
//!
//! This takes at most `2 + ceil(log2(span / n))` bits per value, plus a
//! select0 directory (one 32-bit sample per 256 zeros).
//!
//! The cursor follows PISA's `compact_elias_fano` enumerator:
//! * `next` finds the next one bit in a cached high word (`tzcnt`) and reads
//!   `l` low bits with one unaligned load.
//! * `next_geq` scans value by value when the target is in the current or the
//!   next bucket. Up to 256 buckets ahead it skips buckets a word at a time
//!   (popcount of zeros) from the current position. Further ahead it jumps
//!   with `select0`. It then scans the target bucket.
//!
//! [`EfSequence`] is the reusable core (partitioned EF uses it for its sparse
//! partitions). [`EliasFano`] encodes a whole posting list with
//! `span = universe`.

use super::{Cursor, CursorList, PostingList};
use crate::bits::{select_in_word, BitVec, RankSelect};
use crate::END;

/// Zeros between consecutive select0 samples of [`RankSelect`].
const SELECT0_SAMPLE: u64 = 256;

/// `next_geq` scans value by value when the target bucket is at most this
/// many buckets ahead. PISA uses 8, but skipping zeros a word at a time is
/// cheap enough that 1 measured fastest here.
const LINEAR_SCAN_BUCKETS: u64 = 1;

/// Beyond this many buckets `next_geq` jumps with `select0` rather than
/// skipping zeros from the current position. With this threshold the sample
/// it starts from always lies ahead of the current position. Sequences with at
/// most this many buckets never jump, so they need no select directory.
const SELECT_JUMP_BUCKETS: u64 = SELECT0_SAMPLE;

/// Low bits per value for `n` values in `[0, span)`: `floor(log2(span / n))`,
/// or 0 when `span <= n`.
#[inline]
pub fn ef_lower_bits(span: u64, n: u64) -> u32 {
    if n == 0 || span <= n {
        return 0;
    }
    lower_bits_nonempty(span, n)
}

/// [`ef_lower_bits`] for `span >= n >= 1`, branch-free: the largest `l` with
/// `n << l <= span`, without a division. `n << l` cannot overflow because
/// `n < 2^(msb(n)+1)`.
#[inline(always)]
fn lower_bits_nonempty(span: u64, n: u64) -> u32 {
    debug_assert!(span >= n && n >= 1);
    let l = n.leading_zeros() - span.leading_zeros();
    l - ((n << l > span) as u32)
}

/// Size in bits of the select0 directory for a high bitvector with `zeros`
/// zeros (none when `next_geq` never jumps).
#[inline(always)]
fn select0_dir_bits(zeros: u64) -> u64 {
    if zeros > SELECT_JUMP_BUCKETS {
        32 * zeros.div_ceil(SELECT0_SAMPLE)
    } else {
        0
    }
}

/// [`EfSequence::size_bits`] of `n` values in `[0, span)`, in O(1). This is
/// the cost model used by partitioned EF.
#[inline]
pub fn ef_size_bits(span: u64, n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        ef_size_bits_nonempty(span, n)
    }
}

/// [`ef_size_bits`] for `span >= n >= 1`, branch-free so that a loop over
/// candidate partitions vectorizes.
#[inline(always)]
pub fn ef_size_bits_nonempty(span: u64, n: u64) -> u64 {
    let l = lower_bits_nonempty(span, n);
    let buckets = ((span - 1) >> l) + 1;
    n * l as u64 + n + buckets + select0_dir_bits(buckets)
}

/// Read the `l`-bit low field starting at bit `bit` of `low` (`mask` is
/// `2^l - 1`). Needs one readable word past the field's start word.
#[inline(always)]
unsafe fn read_low(low: &[u64], bit: usize, mask: u64) -> u64 {
    debug_assert!(bit / 64 + 1 < low.len());
    #[cfg(target_endian = "little")]
    {
        // l <= 32 and bit % 8 <= 7, so the field fits in one 64-bit load.
        let p = low.as_ptr().cast::<u8>().add(bit / 8).cast::<u64>();
        (p.read_unaligned() >> (bit % 8)) & mask
    }
    #[cfg(not(target_endian = "little"))]
    {
        let wi = bit / 64;
        let off = bit % 64;
        let lo = *low.get_unchecked(wi) >> off;
        let hi = (*low.get_unchecked(wi + 1) << 1) << (63 - off);
        (lo | hi) & mask
    }
}

/// Elias-Fano encoding of a strictly increasing sequence of values in
/// `[0, span)`, where `span <= 2^32`.
#[derive(Clone, Debug, Default)]
pub struct EfSequence {
    n: usize,
    l: u32,
    /// `n * l` low bits, then zero padding for [`read_low`].
    low: BitVec,
    /// High parts in unary: `n` ones and `((span - 1) >> l) + 1` zeros.
    /// Select0 support only when there are enough buckets to jump.
    high: RankSelect,
}

impl EfSequence {
    /// Encode `values[i] - base`. `values` must be strictly increasing, with
    /// every value in `[base, base + span)`.
    pub fn new(values: &[u32], base: u32, span: u64) -> Self {
        let n = values.len();
        assert!(span <= 1 << 32, "EfSequence span must be at most 2^32");
        debug_assert!(n as u64 <= span);
        let l = ef_lower_bits(span, n as u64);
        let mask = (1u64 << l) - 1;
        let buckets = if n == 0 { 0 } else { ((span - 1) >> l) + 1 };
        let mut low = BitVec::with_capacity(n * l as usize + 64);
        let mut high = BitVec::zeros(n + buckets as usize);
        let mut prev = None;
        for (i, &v) in values.iter().enumerate() {
            debug_assert!(v >= base && ((v - base) as u64) < span, "value out of range");
            debug_assert!(prev < Some(v), "values must be strictly increasing");
            prev = Some(v);
            let x = v.wrapping_sub(base) as u64;
            low.push_bits(x & mask, l);
            high.set((x >> l) as usize + i);
        }
        // Padding: one word past the last field's word, and at least two
        // words (for l = 0).
        low.push_bits(0, 64);
        if low.len() < 128 {
            low.push_bits(0, 64);
        }
        low.shrink_to_fit();
        let high = RankSelect::new(high, false, buckets > SELECT_JUMP_BUCKETS);
        Self { n, l, low, high }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.n
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Number of low bits per value.
    #[inline]
    pub fn lower_bits(&self) -> u32 {
        self.l
    }

    /// Low bits + high bits + select0 directory. The rank directory that
    /// [`RankSelect`] always builds is not used, so it is not counted.
    pub fn size_bits(&self) -> u64 {
        let buckets = (self.high.len() - self.n) as u64;
        (self.n * self.l as usize + self.high.len()) as u64 + select0_dir_bits(buckets)
    }

    /// Append `base + v` for every value `v`.
    pub fn decode_into(&self, base: u32, out: &mut Vec<u32>) {
        out.reserve(self.n);
        let (low, l, mask) = (self.low.words(), self.l as usize, (1u64 << self.l) - 1);
        let mut i = 0usize;
        for (wi, &w) in self.high.bits().words().iter().enumerate() {
            let mut w = w;
            while w != 0 {
                let p = wi * 64 + w.trailing_zeros() as usize;
                w &= w - 1;
                // SAFETY: i < n, and `low` is padded by one word.
                let v = (((p - i) as u64) << l) | unsafe { read_low(low, i * l, mask) };
                out.push(base.wrapping_add(v as u32));
                i += 1;
            }
        }
        debug_assert_eq!(i, self.n);
    }

    #[inline]
    pub fn cursor(&self) -> EfCursor<'_> {
        EfCursor::new(self)
    }
}

/// Readable stand-in for the low bits of a bitmap-mode cursor (`l = 0`).
static NO_LOW_BITS: [u64; 2] = [0; 2];

/// Forward cursor over an [`EfSequence`], returning values relative to the
/// sequence (without `base`).
///
/// A new cursor sits *before* the first value. The caller is responsible for
/// not moving past the last value (typically by comparing with it), which keeps
/// end-of-sequence checks out of the inner loops.
///
/// In *bitmap mode* ([`EfCursor::bitmap`]) it returns the positions of the set
/// bits of a plain bitvector instead. That bitvector is a high-bits vector with
/// `l = 0` whose ones are not offset by their rank. So `next_unchecked` serves
/// both, without branching on the mode (partitioned EF relies on this).
#[derive(Clone, Debug)]
pub struct EfCursor<'a> {
    /// The sequence (`None` in bitmap mode).
    seq: Option<&'a EfSequence>,
    high: &'a [u64],
    low: &'a [u64],
    l: u32,
    mask: u64,
    /// Added to `idx` per value: 1, or 0 in bitmap mode.
    step: usize,
    /// Index of the current value (`usize::MAX` before the first; always 0
    /// in bitmap mode).
    idx: usize,
    /// Word of `high` holding the current one bit (`usize::MAX` before the
    /// first)...
    wi: usize,
    /// ...and the ones of that word after it.
    buf: u64,
    /// Current value (0 before the first).
    val: u64,
}

impl<'a> EfCursor<'a> {
    #[inline]
    pub fn new(seq: &'a EfSequence) -> Self {
        Self {
            seq: Some(seq),
            high: seq.high.bits().words(),
            low: seq.low.words(),
            l: seq.l,
            mask: (1u64 << seq.l) - 1,
            step: 1,
            idx: usize::MAX,
            // Before word 0 with no ones left: the first `next_unchecked`
            // loads word 0, and a jump does not touch it at all.
            wi: usize::MAX,
            buf: 0,
            val: 0,
        }
    }

    /// A bitmap-mode cursor over the set bits of `words`. It supports
    /// `next_unchecked` and `seek_bit`, but not `next_geq_unchecked`.
    #[inline]
    pub fn bitmap(words: &'a [u64]) -> Self {
        Self {
            seq: None,
            high: words,
            low: &NO_LOW_BITS,
            l: 0,
            mask: 0,
            step: 0,
            idx: 0,
            wi: usize::MAX,
            buf: 0,
            val: 0,
        }
    }

    /// Current value (0 before the first).
    #[inline]
    pub fn value(&self) -> u64 {
        self.val
    }

    /// Index of the current value (`usize::MAX` before the first; 0 in bitmap
    /// mode).
    #[inline]
    pub fn index(&self) -> usize {
        self.idx
    }

    /// Bitmap mode: position the cursor so that `next_unchecked` returns the
    /// first set bit at or after `pos`.
    ///
    /// # Safety
    /// Bitmap mode only, and a set bit must exist at or after `pos`.
    #[inline(always)]
    pub unsafe fn seek_bit(&mut self, pos: usize) {
        debug_assert!(self.seq.is_none() && pos / 64 < self.high.len());
        self.wi = pos / 64;
        self.buf = *self.high.get_unchecked(self.wi) & (!0u64 << (pos % 64));
    }

    /// Move to the next value and return it.
    ///
    /// # Safety
    /// The cursor must not be on the last value.
    #[inline(always)]
    pub unsafe fn next_unchecked(&mut self) -> u64 {
        debug_assert!(self.seq.is_none_or(|s| self.idx.wrapping_add(1) < s.n));
        self.idx = self.idx.wrapping_add(self.step);
        let mut buf = self.buf;
        while buf == 0 {
            self.wi = self.wi.wrapping_add(1);
            debug_assert!(self.wi < self.high.len());
            buf = *self.high.get_unchecked(self.wi);
        }
        self.buf = buf & (buf - 1);
        let hpos = self.wi * 64 + buf.trailing_zeros() as usize;
        let lo = read_low(self.low, self.idx * self.l as usize, self.mask);
        self.val = (((hpos - self.idx) as u64) << self.l) | lo;
        self.val
    }

    /// Move to the first value `>= target` and return it.
    ///
    /// # Safety
    /// Not in bitmap mode. `target` must be at most the last value, and
    /// greater than the current value unless the cursor is still before the
    /// first value.
    #[inline(always)]
    pub unsafe fn next_geq_unchecked(&mut self, target: u64) -> u64 {
        debug_assert!(self.seq.is_some());
        let hb = target >> self.l;
        let cur_hb = self.val >> self.l;
        let diff = hb - cur_hb;
        if diff > LINEAR_SCAN_BUCKETS {
            // The bit after the current one (bit 0 before the first value).
            let start = self.idx.wrapping_add(cur_hb as usize).wrapping_add(1);
            let rs = &self.seq.unwrap_unchecked().high;
            let pos = bucket_start(rs, self.high, start, hb, diff);
            // `pos - hb` ones precede `pos`, so the next value has that index.
            self.idx = pos.wrapping_sub(hb as usize).wrapping_sub(1);
            self.wi = pos / 64;
            debug_assert!(self.wi < self.high.len());
            self.buf = *self.high.get_unchecked(self.wi) & (!0u64 << (pos % 64));
        }
        loop {
            let v = self.next_unchecked();
            if v >= target {
                return v;
            }
        }
    }
}

/// First bit of bucket `hb` (one past the `hb`-th zero) of the high bitvector
/// `rs` (whose words are `high`), `diff > 0` buckets after the bucket of the one
/// bit just before `start`. A value must lie at or after that bit.
///
/// Out of line (it runs only for skips of more than [`LINEAR_SCAN_BUCKETS`])
/// and free-standing, so the cursor itself never escapes to a call.
#[inline(never)]
unsafe fn bucket_start(rs: &RankSelect, high: &[u64], start: usize, hb: u64, diff: u64) -> usize {
    if diff > SELECT_JUMP_BUCKETS {
        return rs.select0(hb as usize - 1) + 1;
    }
    // Pass `diff` zeros from `start`, a word at a time.
    let mut wi = start / 64;
    let mut zeros = !*high.get_unchecked(wi) & (!0u64 << (start % 64));
    let mut k = diff;
    loop {
        let c = zeros.count_ones() as u64;
        if k <= c {
            return wi * 64 + select_in_word(zeros, (k - 1) as u32) as usize + 1;
        }
        k -= c;
        wi += 1;
        debug_assert!(wi < high.len());
        zeros = !*high.get_unchecked(wi);
    }
}

/// A posting list stored as one Elias-Fano sequence over `[0, universe)`.
#[derive(Clone, Debug, Default)]
pub struct EliasFano {
    seq: EfSequence,
    last: u32,
}

impl EliasFano {
    pub fn sequence(&self) -> &EfSequence {
        &self.seq
    }
}

impl PostingList for EliasFano {
    const NAME: &'static str = "Elias-Fano";

    fn build(docs: &[u32], universe: u32) -> Self {
        Self {
            seq: EfSequence::new(docs, 0, universe as u64),
            last: docs.last().copied().unwrap_or(0),
        }
    }

    fn len(&self) -> usize {
        self.seq.len()
    }

    fn size_bits(&self) -> u64 {
        self.seq.size_bits()
    }

    fn decode_into(&self, out: &mut Vec<u32>) {
        self.seq.decode_into(0, out);
    }
}

pub struct EliasFanoCursor<'a> {
    ef: EfCursor<'a>,
    cur: u32,
    last: u32,
    n: usize,
}

impl Cursor for EliasFanoCursor<'_> {
    #[inline]
    fn docid(&self) -> u32 {
        self.cur
    }

    // `inline(always)`: LLVM otherwise sometimes keeps cursor methods out of
    // line, which also forces the cursor's state from registers to memory.
    #[inline(always)]
    fn next(&mut self) -> u32 {
        self.cur = if self.cur < self.last {
            // SAFETY: the current posting is not the last one.
            unsafe { self.ef.next_unchecked() as u32 }
        } else {
            END
        };
        self.cur
    }

    #[inline(always)]
    fn next_geq(&mut self, target: u32) -> u32 {
        if target <= self.cur {
            return self.cur;
        }
        self.cur = if target <= self.last {
            // SAFETY: current < target <= last.
            unsafe { self.ef.next_geq_unchecked(target as u64) as u32 }
        } else {
            END
        };
        self.cur
    }

    fn size(&self) -> usize {
        self.n
    }
}

impl CursorList for EliasFano {
    type Cursor<'a> = EliasFanoCursor<'a>;

    #[inline]
    fn cursor(&self) -> EliasFanoCursor<'_> {
        let mut ef = self.seq.cursor();
        let cur = if self.seq.is_empty() {
            END
        } else {
            // SAFETY: the sequence is not empty.
            unsafe { ef.next_unchecked() as u32 }
        };
        EliasFanoCursor { ef, cur, last: self.last, n: self.seq.len() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<EliasFano>();
        conformance::check_cursor::<EliasFano>();
    }

    #[test]
    fn lower_bits_is_floor_log2_of_ratio() {
        for span in [1u64, 2, 3, 7, 8, 9, 1000, 1 << 20, (1 << 32) - 1, 1 << 32] {
            for n in [1u64, 2, 3, 5, 64, 999, 1000, 1 << 19] {
                let expected = if span <= n { 0 } else { 63 - (span / n).leading_zeros() };
                assert_eq!(ef_lower_bits(span, n), expected, "span={span} n={n}");
            }
        }
    }

    #[test]
    fn size_model_matches_encoding() {
        let mut rng = <rand::rngs::StdRng as rand::SeedableRng>::seed_from_u64(5);
        for (n, span) in
            [(1, 1u64), (1, 1 << 32), (300, 300), (300, 100_000), (5000, 6000), (100_000, 1 << 31)]
        {
            let docs = conformance::random_sorted(&mut rng, n, span.min(u32::MAX as u64) as u32);
            let seq = EfSequence::new(&docs, 0, span);
            assert_eq!(seq.size_bits(), ef_size_bits(span, docs.len() as u64), "n={n} span={span}");
        }
        // Values relative to a base, and the empty sequence.
        let seq = EfSequence::new(&[1000, 1001, 1500], 1000, 501);
        let mut out = Vec::new();
        seq.decode_into(1000, &mut out);
        assert_eq!(out, [1000, 1001, 1500]);
        assert_eq!(EfSequence::new(&[], 0, 1 << 20).size_bits(), 0);
    }
}
