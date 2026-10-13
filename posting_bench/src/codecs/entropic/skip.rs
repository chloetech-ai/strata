//! Compressed skip table: an Elias-Fano sequence ([`Ef`]) of the blocks'
//! last docIDs and another of the blocks' stream offsets.
//!
//! An [`Ef`] stores `n` strictly increasing values below `span` with `l =
//! floor(log2(span / n))` low bits each, packed, and the high parts in unary
//! (element `i` sets bit `(v_i >> l) + i`). That is at most
//! `2 + ceil(log2(span / n))` bits per value, about 11 bits per block for the
//! long query lists, against 64 for a plain `(u32, u32)` skip entry.
//!
//! Optional sampled directories (one `u32` per [`SAMPLE`] ones or zeros)
//! give `select1` (random access by index, used for offsets) and `select0`
//! (jump to the bucket of a target, used by `next_geq` over the maxima).
//! Short sequences need no directory: they are scanned from the start.

use crate::bits::{get_bits, select_in_word};

/// Ones (or zeros) between directory samples.
const SAMPLE: usize = 256;

/// `seek` counts zeros word by word up to this many buckets ahead, and jumps
/// with the select0 directory beyond.
const SCAN_BUCKETS: usize = 128;

/// Size in bits of an [`Ef`] of `n >= 1` values below `span`, without
/// directories.
pub fn ef_size_bits(n: u64, span: u64) -> u64 {
    let l = if span <= n { 0 } else { 63 - (span / n).leading_zeros() };
    n * l as u64 + n + ((span - 1) >> l) + 1
}

#[derive(Clone, Debug, Default)]
pub struct Ef {
    n: usize,
    l: u32,
    /// `n · l` low bits, plus two words of padding.
    low: Vec<u64>,
    /// High parts in unary, plus one word of padding.
    high: Vec<u64>,
    high_bits: usize,
    /// Position of one number `k · SAMPLE`, for `k >= 1`.
    sel1: Vec<u32>,
    /// Position of zero number `k · SAMPLE`, for `k >= 1`.
    sel0: Vec<u32>,
}

impl Ef {
    /// The empty sequence.
    pub const EMPTY: Ef =
        Ef { n: 0, l: 0, low: Vec::new(), high: Vec::new(), high_bits: 0, sel1: Vec::new(), sel0: Vec::new() };

    /// Encode strictly increasing `values`, all `< span <= 2^32`.
    pub fn new(values: &[u32], span: u64, select1: bool, select0: bool) -> Self {
        let n = values.len();
        if n == 0 {
            return Self::default();
        }
        debug_assert!(span <= 1 << 32 && values.windows(2).all(|w| w[0] < w[1]));
        debug_assert!((*values.last().unwrap() as u64) < span);
        let l = if span <= n as u64 { 0 } else { 63 - (span / n as u64).leading_zeros() };
        let buckets = ((span - 1) >> l) as usize + 1;
        let high_bits = n + buckets;
        let mut low = crate::bits::BitVec::with_capacity(n * l as usize + 128);
        let mut high = vec![0u64; high_bits.div_ceil(64) + 1];
        for (i, &v) in values.iter().enumerate() {
            low.push_bits(v as u64 & ((1u64 << l) - 1), l);
            let p = (v as u64 >> l) as usize + i;
            high[p / 64] |= 1 << (p % 64);
        }
        low.push_bits(0, 64);
        low.push_bits(0, 64);
        let mut ef = Self { n, l, low: low.words().to_vec(), high, high_bits, sel1: Vec::new(), sel0: Vec::new() };
        if select1 && n > SAMPLE {
            let (mut ones, mut next) = (0usize, SAMPLE);
            for (wi, &w) in ef.high.iter().enumerate() {
                let c = w.count_ones() as usize;
                while next < ones + c {
                    ef.sel1.push((wi * 64) as u32 + select_in_word(w, (next - ones) as u32));
                    next += SAMPLE;
                }
                ones += c;
            }
        }
        if select0 && buckets > SCAN_BUCKETS {
            let (mut zeros, mut next) = (0usize, SAMPLE);
            for (wi, &w) in ef.high.iter().enumerate() {
                let valid = high_bits.saturating_sub(wi * 64).min(64);
                let z = !w & if valid == 64 { !0 } else { (1u64 << valid) - 1 };
                let c = z.count_ones() as usize;
                while next < zeros + c {
                    ef.sel0.push((wi * 64) as u32 + select_in_word(z, (next - zeros) as u32));
                    next += SAMPLE;
                }
                zeros += c;
            }
        }
        ef.low.shrink_to_fit();
        ef
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.n
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Low bits, high bits and directories.
    pub fn size_bits(&self) -> u64 {
        (self.n * self.l as usize + self.high_bits + 32 * (self.sel1.len() + self.sel0.len())) as u64
    }

    /// Value of element `i`, whose one bit is at `pos`.
    #[inline(always)]
    pub fn value_at(&self, i: usize, pos: usize) -> u32 {
        ((((pos - i) as u64) << self.l) | get_bits(&self.low, i * self.l as usize, self.l)) as u32
    }

    /// Position of the first set bit at or after `pos` (one must exist).
    #[inline(always)]
    pub fn next_one(&self, pos: usize) -> usize {
        let mut wi = pos / 64;
        // SAFETY (indexing): a set bit exists at or after pos, so the scan
        // stops inside `high`.
        let mut w = self.high[wi] & (!0u64 << (pos % 64));
        while w == 0 {
            wi += 1;
            w = self.high[wi];
        }
        wi * 64 + w.trailing_zeros() as usize
    }

    /// Position of the last set bit before `pos` (one must exist).
    #[inline(always)]
    pub fn prev_one(&self, pos: usize) -> usize {
        let mut wi = pos / 64;
        let mut w = self.high[wi] & ((1u64 << (pos % 64)) - 1);
        while w == 0 {
            wi -= 1;
            w = self.high[wi];
        }
        wi * 64 + 63 - w.leading_zeros() as usize
    }

    /// Position of the one bit of element `i` (`i < n`).
    #[inline]
    pub fn select1(&self, i: usize) -> usize {
        let s = i / SAMPLE;
        let (start, mut k) = if s == 0 || self.sel1.is_empty() {
            (0, i)
        } else {
            (self.sel1[s - 1] as usize, i % SAMPLE)
        };
        let mut wi = start / 64;
        let mut w = self.high[wi] & (!0u64 << (start % 64));
        loop {
            let c = w.count_ones() as usize;
            if k < c {
                return wi * 64 + select_in_word(w, k as u32) as usize;
            }
            k -= c;
            wi += 1;
            w = self.high[wi];
        }
    }

    /// Value of element `i`.
    #[inline]
    pub fn get(&self, i: usize) -> u32 {
        self.value_at(i, self.select1(i))
    }

    /// Position just after zero number `k` (the start of bucket `k + 1`),
    /// counting zeros from position `from`, where `skip` zeros are known to
    /// precede `from`.
    #[inline]
    fn after_zero(&self, k: usize, from: usize, skip: usize) -> usize {
        let (start, mut left) = if k >= skip + SCAN_BUCKETS && k / SAMPLE > 0 && !self.sel0.is_empty() {
            let s = (k / SAMPLE).min(self.sel0.len());
            let p = self.sel0[s - 1] as usize;
            if s * SAMPLE >= skip {
                (p, k - s * SAMPLE)
            } else {
                (from, k - skip)
            }
        } else {
            (from, k - skip)
        };
        // Find zero number `left` (0-based) at or after `start`.
        let mut wi = start / 64;
        let mut z = !self.high[wi] & (!0u64 << (start % 64));
        loop {
            let c = z.count_ones() as usize;
            if left < c {
                return wi * 64 + select_in_word(z, left as u32) as usize + 1;
            }
            left -= c;
            wi += 1;
            z = !self.high[wi];
        }
    }

    /// First element at or after `from` with value `>= target`, returning
    /// `(index, one position, value)`. `pos` is the one position of element
    /// `from` (or anything if `from == 0`). Such an element must exist.
    #[inline]
    pub fn seek(&self, from: usize, pos: usize, target: u32) -> (usize, usize, u32) {
        let hb = (target as u64 >> self.l) as usize;
        let (mut i, mut p) = if from == 0 { (0, 0) } else { (from, pos) };
        let cur_hb = p - i;
        if hb > cur_hb {
            // Jump to the first position of bucket hb: just after zero
            // number hb - 1. The zeros before p number cur_hb.
            let start = self.after_zero(hb - 1, p, cur_hb);
            i = start - hb;
            p = start;
        }
        loop {
            p = self.next_one(p);
            let v = self.value_at(i, p);
            if v >= target {
                return (i, p, v);
            }
            i += 1;
            p += 1;
        }
    }

    /// Call `f(i, value)` for every element in order.
    pub fn for_each(&self, mut f: impl FnMut(usize, u32)) {
        let mut i = 0;
        for (wi, &w) in self.high.iter().enumerate() {
            let mut w = w;
            while w != 0 && i < self.n {
                let p = wi * 64 + w.trailing_zeros() as usize;
                w &= w - 1;
                f(i, self.value_at(i, p));
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn ef_access_and_seek() {
        let mut rng = StdRng::seed_from_u64(5);
        for (n, span) in [(1usize, 1u64), (1, 1 << 32), (3, 10), (100, 1000), (700, 1 << 32), (5000, 6000), (20_000, 1 << 24), (3000, 3000)] {
            let mut set = std::collections::BTreeSet::new();
            while set.len() < n {
                set.insert(rng.gen_range(0..span) as u32);
            }
            let v: Vec<u32> = set.into_iter().collect();
            for (s1, s0) in [(true, true), (false, false)] {
                let ef = Ef::new(&v, span, s1, s0);
                for (i, &x) in v.iter().enumerate() {
                    assert_eq!(ef.get(i), x, "get({i})");
                }
                if !s1 && !s0 {
                    assert_eq!(ef.size_bits(), ef_size_bits(n as u64, span));
                }
                let mut seen = Vec::new();
                ef.for_each(|i, x| {
                    assert_eq!(i, seen.len());
                    seen.push(x)
                });
                assert_eq!(seen, v);
                // Monotone seeks from random positions.
                for _ in 0..200 {
                    let from = rng.gen_range(0..n);
                    let pos = ef.select1(from);
                    let lo = v[from] as u64;
                    let t = rng.gen_range(lo..=*v.last().unwrap() as u64) as u32;
                    let e = v.partition_point(|&x| x < t);
                    let (i, p, x) = ef.seek(from, pos, t);
                    assert_eq!((i, x), (e, v[e]), "seek({from}, {t})");
                    assert_eq!(p, ef.select1(i));
                    if i > 0 {
                        assert_eq!(ef.prev_one(p), ef.select1(i - 1));
                    }
                }
            }
        }
    }
}
