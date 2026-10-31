//! Document sets and filters shared by every index family.
//!
//! A structured predicate evaluates to a [`DocSet`]; text, vector and facet
//! operators consume any [`DocFilter`] (a `DocSet`, a live-documents mask, a
//! keyword list view, a probe against a column, ...).

use crate::segment::DocId;

/// A set of segment-local docIDs usable as a constraint during search.
pub trait DocFilter: Send + Sync {
    fn contains(&self, doc: DocId) -> bool;
    /// Number of members (exact or estimated; used for planning).
    fn cardinality(&self) -> usize;
    /// The filter as a materialised set, if it is one, so that operators can
    /// enumerate or intersect its members instead of probing every document.
    fn as_doc_set(&self) -> Option<&DocSet> {
        None
    }
    /// The members in increasing order, if the filter holds them (a
    /// materialised set); `None` for probe-only filters. Lets a selective
    /// filter drive a search instead of being probed per candidate.
    fn members(&self) -> Option<Box<dyn Iterator<Item = DocId> + '_>> {
        self.as_doc_set().map(|s| s.iter())
    }
    /// Whether a member may lie in `docs`: false only when none can (by a
    /// column's zone maps, or a materialised set's own members), so that
    /// a scan can skip those documents.
    fn may_have(&self, docs: std::ops::Range<DocId>) -> bool {
        let _ = docs;
        true
    }
}

/// A materialised set of docIDs in `0..n`, sorted array when sparse and
/// bitset when dense (more than `n / 32` members).
#[derive(Clone, Debug, PartialEq)]
pub enum DocSet {
    Sorted { docs: Vec<DocId>, n: u32 },
    Bits { words: Vec<u64>, count: usize, n: u32 },
}

impl DocSet {
    pub fn empty(n: u32) -> Self {
        DocSet::Sorted { docs: Vec::new(), n }
    }

    pub fn all(n: u32) -> Self {
        let mut words = vec![!0u64; (n as usize).div_ceil(64)];
        if n % 64 != 0 {
            *words.last_mut().unwrap() = (1u64 << (n % 64)) - 1;
        }
        DocSet::Bits { words, count: n as usize, n }
    }

    /// From strictly increasing docIDs below `n`.
    pub fn from_sorted(docs: Vec<DocId>, n: u32) -> Self {
        debug_assert!(docs.windows(2).all(|w| w[0] < w[1]) && docs.last().is_none_or(|&d| d < n));
        DocSet::Sorted { docs, n }.normalize()
    }

    /// From a bitset over `0..n` (bits at or above `n` must be zero).
    pub fn from_bits(words: Vec<u64>, n: u32) -> Self {
        let count = words.iter().map(|w| w.count_ones() as usize).sum();
        DocSet::Bits { words, count, n }.normalize()
    }

    /// Universe size.
    pub fn universe(&self) -> u32 {
        match self {
            DocSet::Sorted { n, .. } | DocSet::Bits { n, .. } => *n,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            DocSet::Sorted { docs, .. } => docs.len(),
            DocSet::Bits { count, .. } => *count,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, d: DocId) -> bool {
        match self {
            DocSet::Sorted { docs, .. } => docs.binary_search(&d).is_ok(),
            DocSet::Bits { words, .. } => words.get(d as usize / 64).is_some_and(|w| w >> (d % 64) & 1 == 1),
        }
    }

    /// Members in increasing order.
    pub fn iter(&self) -> Box<dyn Iterator<Item = DocId> + '_> {
        match self {
            DocSet::Sorted { docs, .. } => Box::new(docs.iter().copied()),
            DocSet::Bits { words, .. } => Box::new(words.iter().enumerate().flat_map(|(i, &w)| BitIter { w, base: i as u32 * 64 })),
        }
    }

    pub fn to_sorted(&self) -> Vec<DocId> {
        match self {
            DocSet::Sorted { docs, .. } => docs.clone(),
            DocSet::Bits { .. } => self.iter().collect(),
        }
    }

    pub fn to_bits(&self) -> Vec<u64> {
        match self {
            DocSet::Bits { words, .. } => words.clone(),
            DocSet::Sorted { docs, n } => {
                let mut w = vec![0u64; (*n as usize).div_ceil(64)];
                for &d in docs {
                    w[d as usize / 64] |= 1 << (d % 64);
                }
                w
            }
        }
    }

    pub fn intersect(&self, other: &DocSet) -> DocSet {
        let n = self.universe().max(other.universe());
        match (self, other) {
            (DocSet::Bits { words: a, .. }, DocSet::Bits { words: b, .. }) => {
                DocSet::from_bits(a.iter().zip(b).map(|(x, y)| x & y).collect(), n)
            }
            (DocSet::Sorted { docs, .. }, o) | (o, DocSet::Sorted { docs, .. }) => {
                DocSet::from_sorted(docs.iter().copied().filter(|&d| o.contains(d)).collect(), n)
            }
        }
    }

    pub fn union(&self, other: &DocSet) -> DocSet {
        let n = self.universe().max(other.universe());
        match (self, other) {
            (DocSet::Sorted { docs: a, .. }, DocSet::Sorted { docs: b, .. }) => {
                let mut out = Vec::with_capacity(a.len() + b.len());
                let (mut i, mut j) = (0, 0);
                while i < a.len() && j < b.len() {
                    match a[i].cmp(&b[j]) {
                        std::cmp::Ordering::Less => {
                            out.push(a[i]);
                            i += 1
                        }
                        std::cmp::Ordering::Greater => {
                            out.push(b[j]);
                            j += 1
                        }
                        std::cmp::Ordering::Equal => {
                            out.push(a[i]);
                            i += 1;
                            j += 1
                        }
                    }
                }
                out.extend_from_slice(&a[i..]);
                out.extend_from_slice(&b[j..]);
                DocSet::from_sorted(out, n)
            }
            _ => {
                let mut w = self.to_bits();
                w.resize((n as usize).div_ceil(64), 0);
                for (x, y) in w.iter_mut().zip(other.to_bits()) {
                    *x |= y;
                }
                DocSet::from_bits(w, n)
            }
        }
    }

    /// Members of `self` not in `other`.
    pub fn difference(&self, other: &DocSet) -> DocSet {
        let n = self.universe();
        match self {
            DocSet::Sorted { docs, .. } => DocSet::from_sorted(docs.iter().copied().filter(|&d| !other.contains(d)).collect(), n),
            DocSet::Bits { words, .. } => {
                let o = other.to_bits();
                DocSet::from_bits(words.iter().enumerate().map(|(i, w)| w & !o.get(i).copied().unwrap_or(0)).collect(), n)
            }
        }
    }

    /// Complement within `0..n`.
    pub fn complement(&self) -> DocSet {
        DocSet::all(self.universe()).difference(self)
    }

    /// The docIDs in `0..n` that at least `need` of `sets` hold. Sparse
    /// sets are merged and counted; otherwise each set is added into a
    /// bit-sliced counter, ⌈log2(y + 1)⌉ words for each 64 docIDs that
    /// hold their counts in binary (a dense set adds 64 docIDs a word
    /// operation at a time, a sparse one a member at a time), and the
    /// counts at least `need` come out of a bitwise comparison.
    pub fn at_least(sets: &[DocSet], need: usize, n: u32) -> DocSet {
        let y = sets.len();
        if need == 0 {
            return DocSet::all(n);
        }
        if need > y {
            return DocSet::empty(n);
        }
        let words = (n as usize).div_ceil(64);
        let total: usize = sets.iter().map(DocSet::len).sum();
        if total <= words && sets.iter().all(|s| matches!(s, DocSet::Sorted { .. })) {
            let mut all: Vec<DocId> = Vec::with_capacity(total);
            for s in sets {
                if let DocSet::Sorted { docs, .. } = s {
                    all.extend_from_slice(docs);
                }
            }
            all.sort_unstable();
            let mut out = Vec::new();
            for run in all.chunk_by(|a, b| a == b) {
                if run.len() >= need {
                    out.push(run[0]);
                }
            }
            return DocSet::from_sorted(out, n);
        }
        // Bit b of a docID's count is bit d % 64 of `count[d / 64 * planes
        // + b]`; adding a set ripples its bits up as carries.
        let planes = (usize::BITS - y.leading_zeros()) as usize;
        let mut count = vec![0u64; words * planes];
        let add = |slot: &mut [u64], mut carry: u64| {
            for c in slot {
                let t = *c & carry;
                *c ^= carry;
                carry = t;
                if carry == 0 {
                    break;
                }
            }
        };
        for s in sets {
            match s {
                DocSet::Bits { words: ws, .. } => {
                    for (slot, &w) in count.chunks_exact_mut(planes).zip(ws) {
                        add(slot, w);
                    }
                }
                DocSet::Sorted { docs, .. } => {
                    for &d in docs {
                        let w = d as usize / 64;
                        add(&mut count[w * planes..(w + 1) * planes], 1 << (d % 64));
                    }
                }
            }
        }
        // From the highest bit down: greater once a count has a 1 where
        // `need` has a 0 and they were equal above.
        let out = count
            .chunks_exact(planes)
            .map(|slot| {
                let (mut gt, mut eq) = (0u64, !0u64);
                for b in (0..planes).rev() {
                    if need >> b & 1 == 1 {
                        eq &= slot[b];
                    } else {
                        gt |= eq & slot[b];
                        eq &= !slot[b];
                    }
                }
                gt | eq
            })
            .collect();
        DocSet::from_bits(out, n)
    }

    fn normalize(self) -> Self {
        let n = self.universe();
        let dense = self.len() > n as usize / 32;
        match self {
            DocSet::Sorted { .. } if dense => {
                let words = self.to_bits();
                let count = self.len();
                DocSet::Bits { words, count, n }
            }
            DocSet::Bits { .. } if !dense => DocSet::Sorted { docs: self.iter().collect(), n },
            s => s,
        }
    }
}

impl DocFilter for DocSet {
    fn contains(&self, doc: DocId) -> bool {
        DocSet::contains(self, doc)
    }
    fn cardinality(&self) -> usize {
        self.len()
    }
    fn may_have(&self, docs: std::ops::Range<DocId>) -> bool {
        match self {
            DocSet::Sorted { docs: d, .. } => d.get(d.partition_point(|&x| x < docs.start)).is_some_and(|&x| x < docs.end),
            DocSet::Bits { words, .. } => {
                let (a, b) = (docs.start as usize, (docs.end as usize).min(64 * words.len()));
                a < b
                    && (a / 64..=(b - 1) / 64).any(|w| {
                        let lo = a.max(64 * w) - 64 * w;
                        let hi = b.min(64 * w + 64) - 64 * w;
                        let mask = if hi - lo == 64 { !0 } else { ((1u64 << (hi - lo)) - 1) << lo };
                        words[w] & mask != 0
                    })
            }
        }
    }
    fn as_doc_set(&self) -> Option<&DocSet> {
        Some(self)
    }
}

struct BitIter {
    w: u64,
    base: u32,
}

impl Iterator for BitIter {
    type Item = DocId;
    fn next(&mut self) -> Option<DocId> {
        if self.w == 0 {
            return None;
        }
        let t = self.w.trailing_zeros();
        self.w &= self.w - 1;
        Some(self.base + t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_algebra_matches_naive() {
        let n = 1000;
        let a: Vec<u32> = (0..n).filter(|x| x % 3 == 0).collect();
        let b: Vec<u32> = (0..n).filter(|x| x % 70 == 1).collect();
        let (sa, sb) = (DocSet::from_sorted(a.clone(), n), DocSet::from_sorted(b.clone(), n));
        assert!(matches!(sa, DocSet::Bits { .. }) && matches!(sb, DocSet::Sorted { .. }));
        let inter: Vec<u32> = a.iter().copied().filter(|x| b.contains(x)).collect();
        assert_eq!(sa.intersect(&sb).to_sorted(), inter);
        assert_eq!(sb.intersect(&sa).to_sorted(), inter);
        let mut uni: Vec<u32> = a.iter().chain(&b).copied().collect();
        uni.sort();
        uni.dedup();
        assert_eq!(sa.union(&sb).to_sorted(), uni);
        let diff: Vec<u32> = a.iter().copied().filter(|x| !b.contains(x)).collect();
        assert_eq!(sa.difference(&sb).to_sorted(), diff);
        assert_eq!(sa.complement().len(), n as usize - a.len());
        assert_eq!(DocSet::all(n).len(), n as usize);
        assert!(sa.contains(999) && !sa.contains(998));
    }

    #[test]
    fn at_least_counts_like_a_loop() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for n in [1u32, 63, 64, 65, 200, 3000] {
            for _ in 0..40 {
                let y = 1 + next(17) as usize;
                // Each set dense or sparse, some empty or full.
                let sets: Vec<DocSet> = (0..y)
                    .map(|_| {
                        let p = [0, 1, 2, 30, 50, 90, 100][next(7) as usize];
                        DocSet::from_sorted((0..n).filter(|_| next(100) < p).collect(), n)
                    })
                    .collect();
                for need in 0..=y + 1 {
                    let want: Vec<u32> = (0..n).filter(|&d| sets.iter().filter(|s| s.contains(d)).count() >= need).collect();
                    assert_eq!(DocSet::at_least(&sets, need, n).to_sorted(), want, "n {n}, y {y}, need {need}");
                }
            }
        }
    }
}
