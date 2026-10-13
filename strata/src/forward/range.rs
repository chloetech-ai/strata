//! Range postings: the documents of every equi-depth histogram bucket of a
//! numeric field as a Pyramid list (section `range.<field>`).
//!
//! A range `[lo, hi]` is the union of the buckets it covers plus the
//! documents of the (at most two) buckets it cuts whose column value lies
//! inside. Covered buckets are disjoint, so counting them is a sum of
//! stored cardinalities and materialising them is a union with no
//! duplicates: an OR into a bitset for dense results (Flat chunks and dense
//! blocks OR word by word), Pyramid's k-way union for sparse ones.

use posting_bench::bytes::{ByteReader, ByteWriter};
use posting_bench::codecs::pyramid::Pyramid;
use posting_bench::codecs::{PostingList, SetOps};

use super::blocks::KeyRange;
use super::lists::{build_list, merge_sorted, write_lists, ListTable};
use super::numeric::NumericColumn;
use crate::error::{corrupt, Result};
use crate::segment::{DocSet, SegmentFile};

const VERSION: u64 = 1;

/// The section of bucket lists for `keys` and buckets `[lo[i], hi[i]]`.
pub(crate) fn write_buckets(keys: &[Option<i64>], lo: &[i64], hi: &[i64]) -> Vec<u8> {
    let mut docs: Vec<Vec<u32>> = vec![Vec::new(); lo.len()];
    for (d, k) in keys.iter().enumerate() {
        if let Some(k) = k {
            let b = hi.partition_point(|&h| h < *k);
            debug_assert!(lo[b] <= *k && *k <= hi[b]);
            docs[b].push(d as u32);
        }
    }
    let lists: Vec<Pyramid> = docs.iter().map(|l| build_list(l, keys.len())).collect();
    let mut w = ByteWriter::new();
    w.u64(VERSION);
    write_lists(&mut w, &lists);
    w.finish()
}

/// How [`eval_range`] evaluates a range predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeStrategy {
    /// Pick by estimated cost.
    Auto,
    /// Scan the column (zone maps skip blocks).
    Scan,
    /// Union of the histogram's bucket lists, boundary buckets checked
    /// against the column.
    Postings,
}

/// The buckets a range touches.
#[derive(Clone, Debug)]
pub struct RangePlan {
    pub range: KeyRange,
    /// Buckets inside the range.
    pub covered: Vec<usize>,
    /// Buckets cut by the range (at most two).
    pub boundary: Vec<usize>,
    pub covered_docs: u64,
    pub boundary_docs: u64,
}

/// A numeric field's range postings, with its column for boundary checks.
#[derive(Clone, Debug)]
pub struct RangeIndex {
    col: NumericColumn,
    lists: ListTable,
}

impl RangeIndex {
    pub fn open(seg: &SegmentFile, field: &str) -> Result<Self> {
        let col = NumericColumn::open(seg, field)?;
        let section = seg.require(&format!("range.{field}"))?;
        let mut r = ByteReader::new(section.clone());
        if r.u64().map_err(corrupt)? != VERSION {
            return Err(corrupt("bad range postings version"));
        }
        let lists = ListTable::read(&mut r, &section)?;
        if lists.len() != col.histogram().len() {
            return Err(corrupt(format!("range postings of {field} do not match its histogram")));
        }
        Ok(RangeIndex { col, lists })
    }

    /// Opens the range postings of `field` if the segment has them.
    pub fn open_if_present(seg: &SegmentFile, field: &str) -> Result<Option<Self>> {
        if seg.section(&format!("range.{field}")).is_none() {
            return Ok(None);
        }
        RangeIndex::open(seg, field).map(Some)
    }

    pub fn column(&self) -> &NumericColumn {
        &self.col
    }

    /// Number of buckets.
    pub fn len(&self) -> usize {
        self.lists.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lists.len() == 0
    }

    /// The documents of bucket `i`.
    pub fn bucket(&self, i: usize) -> &Pyramid {
        self.lists.get(i)
    }

    /// Bytes of the bucket lists.
    pub fn byte_len(&self) -> usize {
        self.lists.lists_bytes()
    }

    /// The buckets `[lo, hi]` covers and cuts (`None` for an empty range).
    pub fn plan(&self, lo: f64, hi: f64) -> Option<RangePlan> {
        let range = self.col.key_range(lo, hi)?;
        let h = self.col.histogram();
        let mut p = RangePlan { range, covered: Vec::new(), boundary: Vec::new(), covered_docs: 0, boundary_docs: 0 };
        for i in h.overlapping(range) {
            let (blo, bhi, c) = h.bucket(i);
            if range.lo <= blo && bhi <= range.hi {
                p.covered.push(i);
                p.covered_docs += c as u64;
            } else {
                p.boundary.push(i);
                p.boundary_docs += c as u64;
            }
        }
        Some(p)
    }

    /// Documents with a value in `[lo, hi]` (inclusive; `+-inf` for open
    /// ends).
    pub fn eval(&self, lo: f64, hi: f64) -> DocSet {
        let n = self.col.len();
        let Some(p) = self.plan(lo, hi) else { return DocSet::empty(n as u32) };
        let blocks = self.col.blocks();
        let mut buf = Vec::new();
        if (p.covered_docs + p.boundary_docs) as usize > n / 32 {
            // Dense: OR the covered lists into a bitset, then the boundary
            // documents that match.
            let mut words = vec![0u64; n.div_ceil(64)];
            for &i in &p.covered {
                self.lists.get(i).or_into_bits(&mut words);
            }
            for &i in &p.boundary {
                buf.clear();
                self.lists.get(i).decode_into(&mut buf);
                // Bucket members always have a value.
                for &d in &buf {
                    if p.range.contains(blocks.raw(d)) {
                        words[d as usize / 64] |= 1 << (d % 64);
                    }
                }
            }
            return DocSet::from_bits(words, n as u32);
        }
        let covered: Vec<&Pyramid> = p.covered.iter().map(|&i| self.lists.get(i)).collect();
        let mut out = Vec::with_capacity(p.covered_docs as usize);
        Pyramid::union(&covered, &mut out);
        for &i in &p.boundary {
            buf.clear();
            self.lists.get(i).decode_into(&mut buf);
            buf.retain(|&d| p.range.contains(blocks.raw(d)));
            out = merge_sorted(out, std::mem::take(&mut buf));
        }
        DocSet::from_sorted(out, n as u32)
    }

    /// Number of documents with a value in `[lo, hi]`: covered buckets
    /// count by their stored cardinality, cut ones by checking members.
    pub fn count(&self, lo: f64, hi: f64) -> usize {
        let Some(p) = self.plan(lo, hi) else { return 0 };
        let blocks = self.col.blocks();
        let mut buf = Vec::new();
        let mut c = p.covered_docs as usize;
        for &i in &p.boundary {
            buf.clear();
            self.lists.get(i).decode_into(&mut buf);
            c += buf.iter().filter(|&&d| p.range.contains(blocks.raw(d))).count();
        }
        c
    }
}

/// Costs (ns) of the range strategies, fitted to `strata-forward-bench`
/// on TREC-COVID and Quora (see notes/forward.md).
mod cost {
    /// Scan: per zone map checked (every superblock's, and the blocks' of
    /// the superblocks the range cuts).
    pub const ZONE: f64 = 1.0;
    /// Scan: per decoded block (unpack 128 values, compare, write two
    /// words).
    pub const DECODE: f64 = 34.0;
    /// Postings: per covered bucket member, ORed into a bitset or merged.
    pub const COVER: f64 = 0.6;
    /// Postings: per boundary bucket member, decoded and checked against
    /// the column (a random column access each).
    pub const BOUNDARY: f64 = 9.0;
    /// Per document of the segment when the result is a bitset (allocate,
    /// zero and normalise it).
    pub const BITSET: f64 = 0.02;
}

/// Estimated costs (ns) of a scan and of the postings for `[lo, hi]`
/// (`None` without an index or for an empty range).
pub fn range_costs(col: &NumericColumn, idx: Option<&RangeIndex>, lo: f64, hi: f64, count_only: bool) -> Option<(f64, f64)> {
    let p = idx?.plan(lo, hi)?;
    let n = col.len() as f64;
    let blocks = col.len().div_ceil(super::BLOCK) as f64;
    let bitset = if count_only { 0.0 } else { n * cost::BITSET };
    let (checked, decoded) = col.scan_fractions(p.range);
    let supers = blocks / super::blocks::SUPER as f64;
    let scan = supers * cost::ZONE + blocks * (checked * cost::ZONE + decoded * cost::DECODE) + bitset;
    let dense = (p.covered_docs + p.boundary_docs) as f64 > n / 32.0;
    let postings = p.boundary_docs as f64 * cost::BOUNDARY
        + if count_only { 0.0 } else { p.covered_docs as f64 * cost::COVER + if dense { bitset } else { 0.0 } };
    Some((scan, postings))
}

/// Picks the cheaper strategy for `[lo, hi]` (`count_only`: whether only
/// the number of matches is needed, which makes covered buckets free).
pub fn choose_range_strategy(col: &NumericColumn, idx: Option<&RangeIndex>, lo: f64, hi: f64, count_only: bool) -> RangeStrategy {
    match range_costs(col, idx, lo, hi, count_only) {
        Some((scan, postings)) if postings < scan => RangeStrategy::Postings,
        // An empty range costs nothing either way.
        None if idx.is_some() => RangeStrategy::Postings,
        _ => RangeStrategy::Scan,
    }
}

/// Documents of `col` with a value in `[lo, hi]` (inclusive; `+-inf` for
/// open ends), by `strategy` (`Postings` without an index scans).
pub fn eval_range(col: &NumericColumn, idx: Option<&RangeIndex>, lo: f64, hi: f64, strategy: RangeStrategy) -> DocSet {
    let strategy = match strategy {
        RangeStrategy::Auto => choose_range_strategy(col, idx, lo, hi, false),
        s => s,
    };
    match (strategy, idx) {
        (RangeStrategy::Postings, Some(idx)) => idx.eval(lo, hi),
        _ => col.scan(lo, hi),
    }
}

/// Number of documents of `col` with a value in `[lo, hi]`, by `strategy`.
pub fn count_range(col: &NumericColumn, idx: Option<&RangeIndex>, lo: f64, hi: f64, strategy: RangeStrategy) -> usize {
    let strategy = match strategy {
        RangeStrategy::Auto => choose_range_strategy(col, idx, lo, hi, true),
        s => s,
    };
    match (strategy, idx) {
        (RangeStrategy::Postings, Some(idx)) => idx.count(lo, hi),
        _ => col.scan_count(lo, hi),
    }
}
