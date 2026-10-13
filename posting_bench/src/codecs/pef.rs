//! Partitioned Elias-Fano (Ottaviano & Venturini, "Partitioned Elias-Fano
//! indexes", SIGIR 2014).
//!
//! The list is cut into partitions. Each partition holds `m` postings in
//! `[base, last]`, where `base` is the previous partition's last docID + 1
//! (0 for the first). It is encoded with the cheapest of:
//!
//! * **run**: the postings are exactly `[base, base + m)`, so there is no
//!   payload;
//! * **bitvector**: one bit per value of `[base, last]`. `next_geq` jumps
//!   straight to the target's bit and scans words for the next one;
//! * **Elias-Fano**: an [`EfSequence`] over the local universe
//!   `last - base + 1`.
//!
//! The upper level stores each partition's last docID, which `next_geq`
//! gallops over, and a 32-bit descriptor (encoding + payload offset). It is
//! accounted as 64 bits per partition, PISA's fixed cost.
//!
//! The cursor scans all three encodings with one [`EfCursor`] (bitvectors and
//! runs in its bitmap mode), so `next` does not branch on the encoding.
//!
//! Partitions are chosen by an exact dynamic program that minimises the total
//! size (64 bits + payload per partition), with each candidate's cost computed
//! in O(1). To keep it linear, boundaries are restricted to multiples of 32
//! postings (plus the list end) and partitions to at most 4096 postings: O(n/32
//! · 128) candidates. The paper instead finds a (1+ε)-approximation over
//! arbitrary boundaries. The coarser grid cannot isolate structure finer than
//! 32 postings, such as short runs.

use super::elias_fano::{ef_size_bits_nonempty, EfCursor, EfSequence};
use super::uncompressed::gallop_geq;
use super::{Cursor, CursorList, PostingList};
use crate::END;

/// Partition boundaries fall on multiples of this many postings.
const CHUNK: usize = 32;
/// Maximum postings per partition.
const MAX_PART: usize = 4096;
/// Upper-level cost per partition, in bits.
const FIXED_COST: u64 = 64;

// Descriptor layout: encoding in the top two bits, payload index below
// (word offset into `bits`, or index into `efs`).
const RUN: u32 = 0;
const BITS: u32 = 1;
const EF: u32 = 2;
const KIND_SHIFT: u32 = 30;
const PAYLOAD_MASK: u32 = (1 << KIND_SHIFT) - 1;

/// Encoding of one partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Run,
    BitVector,
    EliasFano,
}

/// Cheapest encoding of `m >= 1` postings spanning `span >= m` values, and its
/// payload size in bits. Ties go to the faster encoding. Branch-free, so that
/// the partitioning loop vectorizes.
#[inline(always)]
fn cheapest(span: u64, m: u64) -> (u32, u64) {
    let ef = ef_size_bits_nonempty(span, m);
    let kind = if span == m {
        RUN
    } else if span <= ef {
        BITS
    } else {
        EF
    };
    (kind, if span == m { 0 } else { span.min(ef) })
}

/// End indices of the partitions minimising the total size, by dynamic
/// programming over boundaries at multiples of [`CHUNK`].
fn partition(docs: &[u32]) -> Vec<usize> {
    let n = docs.len();
    let nb = n.div_ceil(CHUNK);
    let pos = |k: usize| (k * CHUNK).min(n);
    // `base` of a partition starting at boundary k.
    let bases: Vec<u64> =
        (0..nb).map(|k| if k == 0 { 0 } else { docs[k * CHUNK - 1] as u64 + 1 }).collect();
    // best[j]: minimum size of docs[..pos(j)]; from[j]: start of its last
    // partition.
    let mut best = vec![0u64; nb + 1];
    let mut from = vec![0u32; nb + 1];
    let max_chunks = MAX_PART / CHUNK;
    for j in 1..=nb {
        let pj = pos(j);
        let end = docs[pj - 1] as u64 + 1;
        let lo = j.saturating_sub(max_chunks);
        // Minimise (cost << 8 | j - i - 1): ties go to the shortest partition.
        // Any tie-break gives the optimum; packing keeps the loop branch-free.
        let mut key = u64::MAX;
        for i in lo..j {
            let m = (pj - i * CHUNK) as u64;
            let c = best[i] + cheapest(end - bases[i], m).1;
            key = key.min(c << 8 | (j - i - 1) as u64);
        }
        best[j] = (key >> 8) + FIXED_COST;
        from[j] = (j - 1 - (key & 0xff) as usize) as u32;
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

/// A posting list stored as Partitioned Elias-Fano.
#[derive(Clone, Debug, Default)]
pub struct PartitionedEF {
    n: usize,
    /// Last docID of every partition.
    ends: Vec<u32>,
    /// Descriptor of every partition (see `KIND_SHIFT`).
    meta: Vec<u32>,
    /// Bitvector partitions, each starting on a word boundary.
    bits: Vec<u64>,
    /// Elias-Fano partitions.
    efs: Vec<EfSequence>,
    payload_bits: u64,
}

impl PartitionedEF {
    pub fn num_partitions(&self) -> usize {
        self.ends.len()
    }

    /// `(base, last, encoding)` of every partition.
    pub fn partitions(&self) -> impl Iterator<Item = (u32, u32, Encoding)> + '_ {
        (0..self.ends.len()).map(move |p| {
            let enc = match self.meta[p] >> KIND_SHIFT {
                RUN => Encoding::Run,
                BITS => Encoding::BitVector,
                _ => Encoding::EliasFano,
            };
            (self.base(p), self.ends[p], enc)
        })
    }

    #[inline]
    fn base(&self, p: usize) -> u32 {
        if p == 0 {
            0
        } else {
            self.ends[p - 1] + 1
        }
    }
}

impl PostingList for PartitionedEF {
    const NAME: &'static str = "PEF";

    fn build(docs: &[u32], _universe: u32) -> Self {
        let cuts = partition(docs);
        let mut list = Self {
            n: docs.len(),
            ends: Vec::with_capacity(cuts.len()),
            meta: Vec::with_capacity(cuts.len()),
            ..Self::default()
        };
        let (mut start, mut base) = (0usize, 0u32);
        for end in cuts {
            let part = &docs[start..end];
            let last = part[part.len() - 1];
            let span = (last - base) as u64 + 1;
            let (kind, bits) = cheapest(span, part.len() as u64);
            let payload = match kind {
                RUN => 0,
                BITS => {
                    let off = list.bits.len();
                    list.bits.resize(off + span.div_ceil(64) as usize, 0);
                    for &d in part {
                        let r = (d - base) as usize;
                        list.bits[off + r / 64] |= 1u64 << (r % 64);
                    }
                    off
                }
                _ => {
                    let seq = EfSequence::new(part, base, span);
                    debug_assert_eq!(seq.size_bits(), bits);
                    list.efs.push(seq);
                    list.efs.len() - 1
                }
            };
            assert!(payload <= PAYLOAD_MASK as usize, "too many partitions");
            list.meta.push(kind << KIND_SHIFT | payload as u32);
            list.ends.push(last);
            list.payload_bits += bits;
            start = end;
            base = last.wrapping_add(1);
        }
        list.bits.shrink_to_fit();
        list.efs.shrink_to_fit();
        list
    }

    fn len(&self) -> usize {
        self.n
    }

    fn size_bits(&self) -> u64 {
        FIXED_COST * self.ends.len() as u64 + self.payload_bits
    }

    fn decode_into(&self, out: &mut Vec<u32>) {
        out.reserve(self.n);
        for p in 0..self.ends.len() {
            let (base, last) = (self.base(p), self.ends[p]);
            let payload = (self.meta[p] & PAYLOAD_MASK) as usize;
            match self.meta[p] >> KIND_SHIFT {
                RUN => out.extend(base..=last),
                BITS => {
                    let words = ((last - base) / 64 + 1) as usize;
                    for (wi, &w) in self.bits[payload..payload + words].iter().enumerate() {
                        let mut w = w;
                        while w != 0 {
                            out.push(base + (wi * 64) as u32 + w.trailing_zeros());
                            w &= w - 1;
                        }
                    }
                }
                _ => self.efs[payload].decode_into(base, out),
            }
        }
    }
}

/// First partition at or after `from` whose last docID is `>= target`
/// (`ends.len()` if none). Out of line, like [`Part::load`]: both run once per
/// partition change.
#[inline(never)]
fn find_partition(ends: &[u32], from: usize, target: u32) -> usize {
    gallop_geq(ends, from, target)
}

/// All-ones words: a run partition is scanned as a bitvector over these.
static ONES: [u64; MAX_PART / 64] = [!0; MAX_PART / 64];

/// The cursor's current partition: bounds, encoding and payload scanner.
///
/// Every encoding is scanned by one [`EfCursor`]: EF partitions natively, and
/// bitvector and run partitions in bitmap mode (over the partition's words, or
/// over [`ONES`]). So `next` never branches on the encoding.
#[derive(Clone, Debug)]
struct Part<'a> {
    base: u32,
    last: u32,
    kind: u32,
    scan: EfCursor<'a>,
}

impl<'a> Part<'a> {
    /// Partition `p` of `list` (which must exist), positioned before its
    /// first posting. Out of line, so that `next` and `next_geq` stay small
    /// enough to inline.
    #[inline(never)]
    fn load(list: &'a PartitionedEF, p: usize) -> Self {
        let base = if p == 0 { 0 } else { list.ends[p - 1] + 1 };
        let (last, meta) = (list.ends[p], list.meta[p]);
        let kind = meta >> KIND_SHIFT;
        let payload = (meta & PAYLOAD_MASK) as usize;
        let words = ((last - base) / 64 + 1) as usize;
        let scan = match kind {
            RUN => EfCursor::bitmap(&ONES[..words]),
            BITS => EfCursor::bitmap(&list.bits[payload..payload + words]),
            _ => EfCursor::new(&list.efs[payload]),
        };
        Self { base, last, kind, scan }
    }
}

/// Cursor over a [`PartitionedEF`].
pub struct PefCursor<'a> {
    list: &'a PartitionedEF,
    cur: u32,
    /// Index of the current partition.
    p: usize,
    part: Part<'a>,
}

impl<'a> PefCursor<'a> {
    #[inline]
    fn new(list: &'a PartitionedEF) -> Self {
        let empty = Part { base: 0, last: END, kind: RUN, scan: EfCursor::bitmap(&[]) };
        let mut c = Self { list, cur: END, p: 0, part: empty };
        if list.n > 0 {
            c.part = Part::load(list, 0);
            // SAFETY: partitions are not empty.
            c.cur = c.part.base + unsafe { c.part.scan.next_unchecked() } as u32;
        }
        c
    }

    #[inline]
    fn exhaust(&mut self) -> u32 {
        self.cur = END;
        self.part.last = END;
        self.p = self.list.ends.len();
        END
    }
}

impl Cursor for PefCursor<'_> {
    #[inline]
    fn docid(&self) -> u32 {
        self.cur
    }

    #[inline(always)]
    fn next(&mut self) -> u32 {
        if self.cur >= self.part.last {
            if self.p + 1 >= self.list.ends.len() {
                return self.exhaust();
            }
            self.p += 1;
            self.part = Part::load(self.list, self.p);
        }
        // SAFETY: the scanner is not on the partition's last posting (or the
        // partition was just loaded; partitions are not empty).
        self.cur = self.part.base + unsafe { self.part.scan.next_unchecked() } as u32;
        self.cur
    }

    #[inline(always)]
    fn next_geq(&mut self, target: u32) -> u32 {
        if target <= self.cur {
            return self.cur;
        }
        if target > self.part.last {
            let p = find_partition(&self.list.ends, self.p + 1, target);
            if p >= self.list.ends.len() {
                return self.exhaust();
            }
            self.p = p;
            self.part = Part::load(self.list, p);
        }
        // Here base <= target <= last, and target > cur unless the partition
        // was just loaded (then its scanner is before its first posting).
        let part = &mut self.part;
        let rel = target - part.base;
        // SAFETY: a posting >= target exists in the partition (its last).
        let found = unsafe {
            if part.kind == EF {
                part.scan.next_geq_unchecked(rel as u64)
            } else {
                // Bitmap mode: jump straight to the target's bit.
                part.scan.seek_bit(rel as usize);
                part.scan.next_unchecked()
            }
        };
        self.cur = part.base + found as u32;
        self.cur
    }

    fn size(&self) -> usize {
        self.list.n
    }
}

impl CursorList for PartitionedEF {
    type Cursor<'a> = PefCursor<'a>;

    #[inline]
    fn cursor(&self) -> PefCursor<'_> {
        PefCursor::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance::{self, clustered, random_sorted};
    use crate::codecs::elias_fano::EliasFano;
    use rand::{rngs::StdRng, SeedableRng};

    fn kinds(list: &PartitionedEF, lo: u32, hi: u32) -> Vec<Encoding> {
        // Encodings of the partitions lying entirely inside [lo, hi].
        list.partitions()
            .filter(|&(base, last, _)| base >= lo && last <= hi)
            .map(|(_, _, e)| e)
            .collect()
    }

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<PartitionedEF>();
        conformance::check_cursor::<PartitionedEF>();
    }

    #[test]
    fn long_run_is_run_partitions() {
        let docs: Vec<u32> = (1000..2_001_000).collect();
        let pef = PartitionedEF::build(&docs, 3_000_000);
        // The first partition absorbs the offset; the rest are free runs.
        assert!(pef.partitions().skip(1).all(|(_, _, e)| e == Encoding::Run));
        assert!(pef.size_bits() < docs.len() as u64 / 50, "{} bits", pef.size_bits());
    }

    #[test]
    fn regions_get_matching_encodings() {
        let mut rng = StdRng::seed_from_u64(11);
        // Run, then a dense random region, then a sparse random region.
        let mut docs: Vec<u32> = (0..300_000).collect();
        docs.extend(random_sorted(&mut rng, 200_000, 400_000).iter().map(|&d| d + 1_000_000));
        docs.extend(random_sorted(&mut rng, 20_000, 390_000_000).iter().map(|&d| d + 10_000_000));
        let pef = PartitionedEF::build(&docs, 400_000_000);

        let runs = kinds(&pef, 0, 299_999);
        let dense = kinds(&pef, 1_000_000, 1_399_999);
        let sparse = kinds(&pef, 10_000_000, 400_000_000);
        assert!(runs.len() >= 70 && runs.iter().all(|&e| e == Encoding::Run), "{runs:?}");
        assert!(dense.len() >= 40 && dense.iter().all(|&e| e == Encoding::BitVector), "{dense:?}");
        assert!(
            sparse.len() >= 4 && sparse.iter().all(|&e| e == Encoding::EliasFano),
            "{sparse:?}"
        );

        let ef = EliasFano::build(&docs, 400_000_000);
        assert!(
            pef.size_bits() * 3 < ef.size_bits(),
            "PEF {} vs EF {}",
            pef.size_bits(),
            ef.size_bits()
        );
        let mut out = Vec::new();
        pef.decode_into(&mut out);
        assert!(out == docs);

        // The cursor across run / bitvector / EF partitions: full traversal,
        // then next_geq to targets on, before and after postings in each
        // region, interleaved with next.
        let mut cur = pef.cursor();
        let mut got = vec![cur.docid()];
        while got.last() != Some(&END) {
            got.push(cur.next());
        }
        assert!(got[..docs.len()] == docs[..] && got.len() == docs.len() + 1);
        let mut cur = pef.cursor();
        let mut pos = 0;
        for i in (0..docs.len()).step_by(997) {
            for t in [docs[i].saturating_sub(1), docs[i], docs[i] + 1] {
                pos = pos.max(docs.partition_point(|&x| x < t));
                assert_eq!(cur.next_geq(t), docs.get(pos).copied().unwrap_or(END), "next_geq({t})");
            }
            pos += 1;
            assert_eq!(cur.next(), docs.get(pos).copied().unwrap_or(END));
        }
    }

    #[test]
    fn not_larger_than_ef_on_random_lists() {
        let mut rng = StdRng::seed_from_u64(12);
        for (n, universe) in [
            (100, u32::MAX),
            (1000, 1_000_000),
            (10_000, 1_000_000),
            (100_000, 1_000_000),
            (300_000, 1_000_000),
            (100_000, 100_000_000),
            (1_000_000, 10_000_000),
        ] {
            let docs = random_sorted(&mut rng, n, universe);
            let (ef, pef) =
                (EliasFano::build(&docs, universe), PartitionedEF::build(&docs, universe));
            let limit = ef.size_bits() + ef.size_bits() / 100 + FIXED_COST;
            assert!(
                pef.size_bits() <= limit,
                "n={n} u={universe}: PEF {} vs EF {}",
                pef.size_bits(),
                ef.size_bits()
            );
        }
    }

    /// The DP finds the minimum over every partition into multiples of
    /// `CHUNK` postings (exhaustive search on short lists).
    #[test]
    fn dp_is_optimal() {
        fn brute(docs: &[u32], start: usize) -> u64 {
            if start == docs.len() {
                return 0;
            }
            let base = if start == 0 { 0 } else { docs[start - 1] as u64 + 1 };
            let mut best = u64::MAX;
            let mut end = start;
            while end < docs.len() {
                end = (end + CHUNK).min(docs.len());
                let span = docs[end - 1] as u64 + 1 - base;
                let c = FIXED_COST + cheapest(span, (end - start) as u64).1 + brute(docs, end);
                best = best.min(c);
            }
            best
        }
        let mut rng = StdRng::seed_from_u64(13);
        for seed_list in 0..20 {
            let n = 40 + seed_list * 15;
            let docs = if seed_list % 2 == 0 {
                clustered(&mut rng, n, 1_000_000)
            } else {
                let mut d = random_sorted(&mut rng, n / 2, 2_000);
                d.extend(random_sorted(&mut rng, n / 2, 1 << 30).iter().map(|&x| x + 5_000));
                d
            };
            let pef = PartitionedEF::build(&docs, u32::MAX);
            assert_eq!(pef.size_bits(), brute(&docs, 0), "n={}", docs.len());
        }
    }

    /// Size and build-time report: `cargo test --release pef_size_report -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn pef_size_report() {
        let mut rng = StdRng::seed_from_u64(14);
        let u = 10_000_000;
        let mut big_clusters = Vec::new();
        while big_clusters.len() < 1_000_000 {
            let start = big_clusters.last().map_or(0, |&l: &u32| l + 1)
                + rand::Rng::gen_range(&mut rng, 0..20_000);
            let len = rand::Rng::gen_range(&mut rng, 100..5_000);
            big_clusters
                .extend((start..start + len).filter(|_| rand::Rng::gen_bool(&mut rng, 0.7)));
        }
        let lists: Vec<(&str, Vec<u32>, u32)> = vec![
            ("random 1M in 10M", random_sorted(&mut rng, 1_000_000, u), u),
            ("clustered runs of 1..32, 1M in 10M", clustered(&mut rng, 1_000_000, u), u),
            ("clustered runs of 1..32, 100K in 10M", clustered(&mut rng, 100_000, u), u),
            (
                "70%-dense clusters of 100..5000, gaps <20K",
                big_clusters.clone(),
                *big_clusters.last().unwrap() + 1,
            ),
            (
                "random 20M in 100M",
                (0..100_000_000).filter(|_| rand::Rng::gen_bool(&mut rng, 0.2)).collect(),
                100_000_000,
            ),
        ];
        for (name, docs, universe) in &lists {
            let t = std::time::Instant::now();
            let ef = EliasFano::build(docs, *universe);
            let t_ef = t.elapsed();
            let t = std::time::Instant::now();
            let pef = PartitionedEF::build(docs, *universe);
            let t_pef = t.elapsed();
            let t = std::time::Instant::now();
            std::hint::black_box(partition(docs));
            let t_dp = t.elapsed();
            let n = docs.len() as f64;
            let k = pef.partitions().fold([0usize; 3], |mut k, (_, _, e)| {
                k[e as usize] += 1;
                k
            });
            println!(
                "{name} (n={}): EF {:.3} bits/posting (build {t_ef:.1?}), PEF {:.3} bits/posting (build {t_pef:.1?}, of which DP {t_dp:.1?}; partitions run/bv/ef = {k:?})",
                docs.len(),
                ef.size_bits() as f64 / n,
                pef.size_bits() as f64 / n,
            );
        }
    }
}
