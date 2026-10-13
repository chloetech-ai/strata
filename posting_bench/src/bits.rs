//! Bit-level building blocks: a growable bit vector, a rank/select directory,
//! and a sequential bit reader.
//!
//! Bits are stored LSB-first in 64-bit words: bit `i` lives in word `i / 64`
//! at position `i % 64`. Unused bits of the last word are always zero.

/// Number of bits needed to represent `x` (0 for `x == 0`).
#[inline]
pub fn bit_width(x: u64) -> u32 {
    64 - x.leading_zeros()
}

/// Position of the `k`-th set bit (0-based) inside `w`. `k` must be smaller
/// than `w.count_ones()`.
#[inline]
pub fn select_in_word(w: u64, k: u32) -> u32 {
    debug_assert!(k < w.count_ones());
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
    {
        // SAFETY: guarded by the `bmi2` target feature.
        unsafe { core::arch::x86_64::_pdep_u64(1u64 << k, w).trailing_zeros() }
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
    {
        let mut w = w;
        for _ in 0..k {
            w &= w - 1;
        }
        w.trailing_zeros()
    }
}

/// Read `width` (<= 64) bits starting at bit `pos` of `words`.
#[inline]
pub fn get_bits(words: &[u64], pos: usize, width: u32) -> u64 {
    if width == 0 {
        return 0;
    }
    let wi = pos / 64;
    let off = (pos % 64) as u32;
    let mut v = words[wi] >> off;
    if off + width > 64 {
        v |= words[wi + 1] << (64 - off);
    }
    if width == 64 {
        v
    } else {
        v & ((1u64 << width) - 1)
    }
}

/// A growable bit vector.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct BitVec {
    words: Vec<u64>,
    len: usize,
}

impl BitVec {
    pub fn new() -> Self {
        Self::default()
    }

    /// A bit vector of `len` zero bits.
    pub fn zeros(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }

    pub fn with_capacity(bits: usize) -> Self {
        Self {
            words: Vec::with_capacity(bits.div_ceil(64)),
            len: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    #[inline]
    pub fn push(&mut self, bit: bool) {
        if self.len % 64 == 0 {
            self.words.push(0);
        }
        if bit {
            self.words[self.len / 64] |= 1u64 << (self.len % 64);
        }
        self.len += 1;
    }

    /// Append the low `width` (<= 64) bits of `value`.
    #[inline]
    pub fn push_bits(&mut self, value: u64, width: u32) {
        if width == 0 {
            return;
        }
        debug_assert!(width == 64 || value >> width == 0, "value wider than width");
        let off = self.len % 64;
        if off == 0 {
            self.words.push(value);
        } else {
            *self.words.last_mut().unwrap() |= value << off;
            if off + width as usize > 64 {
                self.words.push(value >> (64 - off));
            }
        }
        self.len += width as usize;
    }

    /// Set bit `i` to one.
    #[inline]
    pub fn set(&mut self, i: usize) {
        debug_assert!(i < self.len);
        self.words[i / 64] |= 1u64 << (i % 64);
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        debug_assert!(i < self.len);
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    /// Read `width` (<= 64) bits starting at bit `pos`.
    #[inline]
    pub fn get_bits(&self, pos: usize, width: u32) -> u64 {
        get_bits(&self.words, pos, width)
    }

    /// Position of the first set bit at index `>= pos`.
    #[inline]
    pub fn next_one(&self, pos: usize) -> Option<usize> {
        if pos >= self.len {
            return None;
        }
        let mut wi = pos / 64;
        let mut w = self.words[wi] & (!0u64 << (pos % 64));
        loop {
            if w != 0 {
                let p = wi * 64 + w.trailing_zeros() as usize;
                return (p < self.len).then_some(p);
            }
            wi += 1;
            if wi >= self.words.len() {
                return None;
            }
            w = self.words[wi];
        }
    }

    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn shrink_to_fit(&mut self) {
        self.words.shrink_to_fit();
    }

    /// Iterate over the positions of set bits.
    pub fn ones(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, &w)| {
            let mut w = w;
            std::iter::from_fn(move || {
                if w == 0 {
                    None
                } else {
                    let t = w.trailing_zeros() as usize;
                    w &= w - 1;
                    Some(wi * 64 + t)
                }
            })
        })
    }
}

/// Bits per rank block (one `u32` cumulative count per block: 6.25% overhead).
const RANK_BLOCK: usize = 512;
/// Every `SELECT_SAMPLE`-th one (or zero) has its position sampled.
const SELECT_SAMPLE: usize = 256;

/// Immutable bit vector with constant-time rank and sampled select.
///
/// * `rank1` uses one `u32` cumulative count per 512 bits plus popcounts.
/// * `select1` / `select0` (optional) sample the position of every 256th
///   one / zero and scan forward word by word from the sample.
#[derive(Clone, Debug, Default)]
pub struct RankSelect {
    bits: BitVec,
    rank_samples: Vec<u32>,
    select1_samples: Vec<u32>,
    select0_samples: Vec<u32>,
    ones: usize,
}

impl RankSelect {
    /// Build the directories. Rank support is always built; select support
    /// only when requested.
    pub fn new(bits: BitVec, select1: bool, select0: bool) -> Self {
        assert!(
            bits.len() < u32::MAX as usize,
            "RankSelect supports bit vectors shorter than 2^32 bits"
        );
        let words = bits.words();
        let mut rank_samples = Vec::with_capacity(bits.len() / RANK_BLOCK + 2);
        let mut acc = 0u32;
        for (i, w) in words.iter().enumerate() {
            if i % (RANK_BLOCK / 64) == 0 {
                rank_samples.push(acc);
            }
            acc += w.count_ones();
        }
        rank_samples.push(acc);
        let ones = acc as usize;

        let mut select1_samples = Vec::new();
        let mut select0_samples = Vec::new();
        if select1 || select0 {
            let (mut seen1, mut seen0) = (0usize, 0usize);
            for (wi, &w) in words.iter().enumerate() {
                let valid = (bits.len() - wi * 64).min(64);
                let valid_mask = if valid == 64 { !0 } else { (1u64 << valid) - 1 };
                if select1 {
                    let c = w.count_ones() as usize;
                    // Sample every one whose rank is a multiple of SELECT_SAMPLE.
                    while select1_samples.len() * SELECT_SAMPLE < seen1 + c {
                        let k = select1_samples.len() * SELECT_SAMPLE - seen1;
                        select1_samples.push((wi * 64) as u32 + select_in_word(w, k as u32));
                    }
                    seen1 += c;
                }
                if select0 {
                    let z = !w & valid_mask;
                    let c = z.count_ones() as usize;
                    while select0_samples.len() * SELECT_SAMPLE < seen0 + c {
                        let k = select0_samples.len() * SELECT_SAMPLE - seen0;
                        select0_samples.push((wi * 64) as u32 + select_in_word(z, k as u32));
                    }
                    seen0 += c;
                }
            }
        }
        let mut bits = bits;
        bits.shrink_to_fit();
        Self {
            bits,
            rank_samples,
            select1_samples,
            select0_samples,
            ones,
        }
    }

    #[inline]
    pub fn bits(&self) -> &BitVec {
        &self.bits
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bits.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }

    #[inline]
    pub fn num_ones(&self) -> usize {
        self.ones
    }

    #[inline]
    pub fn num_zeros(&self) -> usize {
        self.bits.len() - self.ones
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        self.bits.get(i)
    }

    /// Number of ones in positions `[0, pos)`; `pos <= len`.
    #[inline]
    pub fn rank1(&self, pos: usize) -> usize {
        debug_assert!(pos <= self.bits.len());
        let words = self.bits.words();
        let block = pos / RANK_BLOCK;
        let mut r = self.rank_samples[block] as usize;
        let last_word = pos / 64;
        for w in &words[block * (RANK_BLOCK / 64)..last_word] {
            r += w.count_ones() as usize;
        }
        let rem = pos % 64;
        if rem != 0 {
            r += (words[last_word] & ((1u64 << rem) - 1)).count_ones() as usize;
        }
        r
    }

    /// Number of zeros in positions `[0, pos)`.
    #[inline]
    pub fn rank0(&self, pos: usize) -> usize {
        pos - self.rank1(pos)
    }

    /// Position of the `k`-th one (0-based). Requires select1 support.
    #[inline]
    pub fn select1(&self, k: usize) -> usize {
        debug_assert!(k < self.ones);
        let words = self.bits.words();
        let s = k / SELECT_SAMPLE;
        let start = self.select1_samples[s] as usize;
        let mut remaining = (k - s * SELECT_SAMPLE) as u32;
        let mut wi = start / 64;
        let mut w = words[wi] & (!0u64 << (start % 64));
        loop {
            let c = w.count_ones();
            if remaining < c {
                return wi * 64 + select_in_word(w, remaining) as usize;
            }
            remaining -= c;
            wi += 1;
            w = words[wi];
        }
    }

    /// Position of the `k`-th zero (0-based). Requires select0 support.
    #[inline]
    pub fn select0(&self, k: usize) -> usize {
        debug_assert!(k < self.num_zeros());
        let words = self.bits.words();
        let s = k / SELECT_SAMPLE;
        let start = self.select0_samples[s] as usize;
        let mut remaining = (k - s * SELECT_SAMPLE) as u32;
        let mut wi = start / 64;
        let mut w = !words[wi] & (!0u64 << (start % 64));
        loop {
            let c = w.count_ones();
            if remaining < c {
                return wi * 64 + select_in_word(w, remaining) as usize;
            }
            remaining -= c;
            wi += 1;
            w = !words[wi];
        }
    }

    /// Size in bits of the bit vector plus all directories.
    pub fn size_bits(&self) -> u64 {
        self.bits.len() as u64
            + 32 * (self.rank_samples.len() + self.select1_samples.len() + self.select0_samples.len())
                as u64
    }
}

/// Sequential reader over a bit slice.
#[derive(Clone, Copy, Debug)]
pub struct BitReader<'a> {
    words: &'a [u64],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(words: &'a [u64], pos: usize) -> Self {
        Self { words, pos }
    }

    #[inline]
    pub fn read(&mut self, width: u32) -> u64 {
        let v = get_bits(self.words, self.pos, width);
        self.pos += width as usize;
        v
    }

    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    fn random_bits(n: usize, density: f64, seed: u64) -> (BitVec, Vec<bool>) {
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let mut bv = BitVec::new();
        let mut v = Vec::new();
        for _ in 0..n {
            let b = rng.gen_bool(density);
            bv.push(b);
            v.push(b);
        }
        (bv, v)
    }

    #[test]
    fn push_bits_roundtrip() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let mut bv = BitVec::new();
        let mut items = Vec::new();
        for _ in 0..10_000 {
            let w = rng.gen_range(0..=64u32);
            let v = if w == 64 { rng.gen::<u64>() } else { rng.gen::<u64>() & ((1u64 << w) - 1) };
            items.push((bv.len(), w, v));
            bv.push_bits(v, w);
        }
        for (pos, w, v) in items {
            assert_eq!(bv.get_bits(pos, w), v);
        }
    }

    #[test]
    fn rank_select_match_naive() {
        for (n, density, seed) in [(1, 0.5, 1), (63, 0.3, 2), (64, 0.5, 3), (5000, 0.01, 4), (100_000, 0.5, 5), (70_000, 0.97, 6)] {
            let (bv, v) = random_bits(n, density, seed);
            let rs = RankSelect::new(bv, true, true);
            let mut ones = Vec::new();
            let mut zeros = Vec::new();
            let mut r = 0;
            for (i, &b) in v.iter().enumerate() {
                assert_eq!(rs.rank1(i), r, "rank1({i})");
                if b {
                    ones.push(i);
                    r += 1;
                } else {
                    zeros.push(i);
                }
            }
            assert_eq!(rs.rank1(n), r);
            for (k, &p) in ones.iter().enumerate() {
                assert_eq!(rs.select1(k), p, "select1({k})");
            }
            for (k, &p) in zeros.iter().enumerate() {
                assert_eq!(rs.select0(k), p, "select0({k})");
            }
        }
    }

    #[test]
    fn next_one_and_ones() {
        let (bv, v) = random_bits(10_000, 0.02, 9);
        let expected: Vec<usize> = (0..v.len()).filter(|&i| v[i]).collect();
        assert_eq!(bv.ones().collect::<Vec<_>>(), expected);
        for pos in 0..v.len() {
            let e = expected.iter().copied().find(|&p| p >= pos);
            assert_eq!(bv.next_one(pos), e);
        }
    }
}
