//! Numeric columns (INT and FLOAT fields) and their equi-depth histograms.
//!
//! Every value is stored as an order-preserving 64-bit integer *key*, so
//! that range predicates, zone maps and histogram buckets all work on
//! integers and a float range is evaluated on the encoded form:
//!
//! * INT: the value itself.
//! * FLOAT with a decimal scale: if every value is `m / 10^s` exactly (in
//!   double arithmetic) for a small `s` (at most [`MAX_SCALE`]) and integers
//!   `|m| <= 2^53`, the key is `m`. Ratings with two decimals become 0..499
//!   and pack in 9 bits per document instead of the ~60 bits that frame of
//!   reference leaves of raw doubles. Decoding is `m as f64 / 10^s`, which
//!   reproduces the original double; the scale is detected at build time.
//! * FLOAT otherwise: the IEEE bits with the magnitude bits of negative
//!   numbers flipped, which orders keys like the doubles (`-0.0` is stored
//!   as `0.0`; NaN sorts above `+inf` and matches no range).
//!
//! A range `[lo, hi]` over doubles maps to the smallest and largest key
//! whose decoded value lies inside it, so per-document checks are integer
//! compares on block deltas (see [`super::blocks`]).
//!
//! Sections: `col.<field>` (kind, scale, blocks) and `col.<field>.hist`
//! (bucket key bounds and counts of an equi-depth histogram over the
//! present values, also the planner's selectivity estimator).

use posting_bench::bytes::{ByteReader, ByteWriter, OwnedBytes, Slice};

use super::blocks::{write_blocks, Blocks, KeyRange};
use crate::error::{corrupt, Error, Result};
use crate::schema::{Field, FieldType};
use crate::segment::build::BuildOptions;
use crate::segment::{DocFilter, DocId, DocSet, FieldIndexBuilder, SegmentFile, SegmentWriter};
use crate::value::Value;

/// Largest decimal scale tried for FLOAT columns.
pub const MAX_SCALE: u32 = 9;
/// Default number of histogram buckets (`forward.range_buckets`).
pub const DEFAULT_BUCKETS: usize = 64;

const COL_VERSION: u64 = 1;
const HIST_VERSION: u64 = 1;
/// Keys of decimal columns stay within +-2^53 (exact in a double).
const DECIMAL_LIMIT: f64 = 9_007_199_254_740_992.0;

const POW10: [f64; MAX_SCALE as usize + 1] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9];

/// How a column's values map to keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericKind {
    Int,
    /// FLOAT values `key / 10^scale`.
    Decimal(u32),
    /// FLOAT values as order-preserving IEEE bits.
    FloatBits,
}

impl NumericKind {
    fn code(self) -> (u64, u64) {
        match self {
            NumericKind::Int => (0, 0),
            NumericKind::Decimal(s) => (1, s as u64),
            NumericKind::FloatBits => (2, 0),
        }
    }

    fn from_code(kind: u64, scale: u64) -> Result<Self> {
        match (kind, scale) {
            (0, _) => Ok(NumericKind::Int),
            (1, s) if s <= MAX_SCALE as u64 => Ok(NumericKind::Decimal(s as u32)),
            (2, _) => Ok(NumericKind::FloatBits),
            _ => Err(corrupt("bad numeric column kind")),
        }
    }

    pub fn is_float(self) -> bool {
        self != NumericKind::Int
    }

    /// The value of key `k` as a double.
    #[inline]
    pub fn decode(self, k: i64) -> f64 {
        match self {
            NumericKind::Int => k as f64,
            NumericKind::Decimal(s) => k as f64 / POW10[s as usize],
            NumericKind::FloatBits => f64::from_bits((k ^ ((k >> 63) & i64::MAX)) as u64),
        }
    }

    /// Smallest and largest key whose value lies in `[lo, hi]` (`None` if
    /// there are none, or a bound is NaN).
    pub fn key_range(self, lo: f64, hi: f64) -> Option<KeyRange> {
        const TWO63: f64 = 9_223_372_036_854_775_808.0;
        if lo.is_nan() || hi.is_nan() {
            return None;
        }
        let (klo, khi) = match self {
            NumericKind::Int => {
                if lo >= TWO63 || hi < -TWO63 {
                    return None;
                }
                // `as` saturates at the i64 limits.
                (lo.ceil() as i64, if hi >= TWO63 { i64::MAX } else { hi.floor() as i64 })
            }
            NumericKind::Decimal(s) => {
                let p = POW10[s as usize];
                let dec = |k: i64| k as f64 / p;
                // Decoding is monotone, so the bounds are thresholds; start
                // from the scaled bound and correct its rounding.
                let klo = if lo * p < -DECIMAL_LIMIT {
                    i64::MIN
                } else if lo * p > DECIMAL_LIMIT {
                    return None;
                } else {
                    let mut c = (lo * p).ceil() as i64;
                    while dec(c - 1) >= lo {
                        c -= 1;
                    }
                    while dec(c) < lo {
                        c += 1;
                    }
                    c
                };
                let khi = if hi * p > DECIMAL_LIMIT {
                    i64::MAX
                } else if hi * p < -DECIMAL_LIMIT {
                    return None;
                } else {
                    let mut c = (hi * p).floor() as i64;
                    while dec(c + 1) <= hi {
                        c += 1;
                    }
                    while dec(c) > hi {
                        c -= 1;
                    }
                    c
                };
                (klo, khi)
            }
            NumericKind::FloatBits => (float_key(lo), float_key(hi)),
        };
        (klo <= khi).then_some(KeyRange { lo: klo, hi: khi })
    }
}

/// Order-preserving key of a double (`-0.0` as `0.0`, NaN canonical).
#[inline]
pub fn float_key(v: f64) -> i64 {
    let v = if v == 0.0 {
        0.0
    } else if v.is_nan() {
        f64::NAN
    } else {
        v
    };
    let b = v.to_bits() as i64;
    b ^ ((b >> 63) & i64::MAX)
}

/// The decimal scale of `vals` if one up to [`MAX_SCALE`] reproduces each
/// of them exactly.
fn decimal_scale(vals: &[f64]) -> Option<u32> {
    let fits = |v: f64, s: u32| {
        let p = POW10[s as usize];
        let m = (v * p).round();
        m.abs() <= DECIMAL_LIMIT && m / p == v
    };
    let mut s = 0;
    for &v in vals {
        if !v.is_finite() {
            return None;
        }
        while !fits(v, s) {
            s += 1;
            if s > MAX_SCALE {
                return None;
            }
        }
    }
    // A value that fits at a smaller scale also fits at a larger one
    // (both divisions are correctly rounded quotients of the same real);
    // the caller re-checks every key anyway.
    Some(s)
}

/// Encodes FLOAT values: decimal keys if a scale fits, else IEEE keys.
fn encode_floats(vals: &[Option<f64>]) -> (NumericKind, Vec<Option<i64>>) {
    let present: Vec<f64> = vals.iter().flatten().copied().collect();
    if let Some(s) = decimal_scale(&present) {
        let p = POW10[s as usize];
        let keys: Vec<Option<i64>> = vals.iter().map(|v| v.map(|v| (v * p).round() as i64)).collect();
        let kind = NumericKind::Decimal(s);
        if keys.iter().zip(vals).all(|(k, v)| k.map(|k| kind.decode(k)) == *v) {
            return (kind, keys);
        }
    }
    (NumericKind::FloatBits, vals.iter().map(|v| v.map(float_key)).collect())
}

// ------------------------------------------------------------ histogram

/// Equi-depth buckets over the sorted present keys: disjoint key ranges
/// `[lo[i], hi[i]]` in increasing order, each about `n / buckets` keys.
/// Equal keys never straddle two buckets: a bucket ends after the run of
/// its last key, or before it when the run would more than double the
/// bucket (a heavy value then gets a bucket of its own).
pub(crate) fn equi_depth(sorted: &[i64], buckets: usize) -> (Vec<i64>, Vec<i64>, Vec<u32>) {
    let n = sorted.len();
    let (mut lo, mut hi, mut count) = (Vec::new(), Vec::new(), Vec::new());
    let mut i = 0;
    while i < n {
        // Ideal boundaries are multiples of n / buckets; aim for the next
        // one past `i`.
        let b = i * buckets / n;
        let target = ((b + 1) * n).div_ceil(buckets).clamp(i + 1, n);
        let last = sorted[target - 1];
        let run_start = i + sorted[i..].partition_point(|&k| k < last);
        let run_end = i + sorted[i..].partition_point(|&k| k <= last);
        let end = if run_start > i && run_end - i > 2 * (target - i) { run_start } else { run_end };
        lo.push(sorted[i]);
        hi.push(sorted[end - 1]);
        count.push((end - i) as u32);
        i = end;
    }
    (lo, hi, count)
}

/// The histogram section: bucket key bounds and counts.
#[derive(Clone, Debug)]
pub struct Histogram {
    lo: Slice<i64>,
    hi: Slice<i64>,
    count: Slice<u32>,
}

impl Histogram {
    fn write(lo: &[i64], hi: &[i64], count: &[u32]) -> Vec<u8> {
        let mut w = ByteWriter::new();
        w.u64(HIST_VERSION);
        w.array(lo);
        w.array(hi);
        w.array(count);
        w.finish()
    }

    fn read(bytes: OwnedBytes) -> Result<Histogram> {
        let mut r = ByteReader::new(bytes);
        if r.u64().map_err(corrupt)? != HIST_VERSION {
            return Err(corrupt("bad histogram version"));
        }
        let h = Histogram { lo: r.array().map_err(corrupt)?, hi: r.array().map_err(corrupt)?, count: r.array().map_err(corrupt)? };
        if h.lo.len() != h.hi.len() || h.lo.len() != h.count.len() {
            return Err(corrupt("bad histogram"));
        }
        Ok(h)
    }

    pub fn len(&self) -> usize {
        self.lo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lo.is_empty()
    }

    /// `(lo key, hi key, count)` of bucket `i`.
    #[inline]
    pub fn bucket(&self, i: usize) -> (i64, i64, u32) {
        (self.lo[i], self.hi[i], self.count[i])
    }

    /// Buckets overlapping `r`, as an index range.
    pub fn overlapping(&self, r: KeyRange) -> std::ops::Range<usize> {
        let start = self.hi.partition_point(|&h| h < r.lo);
        let end = self.lo.partition_point(|&l| l <= r.hi);
        start..end.max(start)
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.lo.byte_len() + self.hi.byte_len() + self.count.byte_len()
    }
}

// --------------------------------------------------------------- builder

/// Builds `col.<field>`, `col.<field>.hist` and the range postings of an
/// INT or FLOAT field.
pub(crate) struct NumericBuilder {
    name: String,
    float: bool,
    ints: Vec<Option<i64>>,
    floats: Vec<Option<f64>>,
    buckets: usize,
}

impl NumericBuilder {
    pub fn new(field: &Field, opts: &BuildOptions) -> Result<Self> {
        let buckets = super::usize_option(opts, "forward.range_buckets", DEFAULT_BUCKETS)?;
        Ok(NumericBuilder {
            name: field.name.clone(),
            float: matches!(field.ty, FieldType::Float),
            ints: Vec::new(),
            floats: Vec::new(),
            buckets,
        })
    }
}

impl FieldIndexBuilder for NumericBuilder {
    fn add(&mut self, doc: DocId, value: &Value) -> Result<()> {
        let d = doc as usize;
        if self.float {
            self.floats.resize(d, None);
            self.floats.push(match value {
                Value::Null => None,
                v => Some(v.as_f64().ok_or_else(|| Error::Schema(format!("{}: not a number", self.name)))?),
            });
        } else {
            self.ints.resize(d, None);
            self.ints.push(match value {
                Value::Null => None,
                Value::Int(v) => Some(*v),
                _ => return Err(Error::Schema(format!("{}: not an integer", self.name))),
            });
        }
        Ok(())
    }

    fn finish(self: Box<Self>, out: &mut SegmentWriter) -> Result<()> {
        let n = out.n_docs();
        let (kind, mut keys) = if self.float { encode_floats(&self.floats) } else { (NumericKind::Int, self.ints) };
        keys.resize(n, None);
        let mut w = ByteWriter::new();
        w.u64(COL_VERSION);
        let (k, s) = kind.code();
        w.u64(k);
        w.u64(s);
        write_blocks(&keys, &mut w);
        out.put(format!("col.{}", self.name), w.finish());

        let mut sorted: Vec<i64> = keys.iter().flatten().copied().collect();
        sorted.sort_unstable();
        let (lo, hi, count) = if self.buckets > 0 { equi_depth(&sorted, self.buckets) } else { Default::default() };
        out.put(format!("col.{}.hist", self.name), Histogram::write(&lo, &hi, &count));
        if self.buckets > 0 {
            out.put(format!("range.{}", self.name), super::range::write_buckets(&keys, &lo, &hi));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- reader

/// A zero-copy view of an INT or FLOAT column: O(1) value access, range
/// scans with zone maps, probes, and histogram estimates.
#[derive(Clone, Debug)]
pub struct NumericColumn {
    name: String,
    kind: NumericKind,
    blocks: Blocks,
    hist: Histogram,
}

impl NumericColumn {
    pub fn open(seg: &SegmentFile, field: &str) -> Result<Self> {
        let mut r = ByteReader::new(seg.require(&format!("col.{field}"))?);
        if r.u64().map_err(corrupt)? != COL_VERSION {
            return Err(corrupt("bad column version"));
        }
        let (k, s) = (r.u64().map_err(corrupt)?, r.u64().map_err(corrupt)?);
        let kind = NumericKind::from_code(k, s)?;
        let blocks = Blocks::read(&mut r)?;
        if blocks.len() != seg.n_docs() {
            return Err(corrupt(format!("column {field} has {} documents", blocks.len())));
        }
        let hist = Histogram::read(seg.require(&format!("col.{field}.hist"))?)?;
        Ok(NumericColumn { name: field.to_string(), kind, blocks, hist })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of documents.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.len() == 0
    }

    /// Number of documents with a value.
    pub fn n_present(&self) -> usize {
        self.blocks.n_present()
    }

    pub fn kind(&self) -> NumericKind {
        self.kind
    }

    pub fn histogram(&self) -> &Histogram {
        &self.hist
    }

    pub(crate) fn blocks(&self) -> &Blocks {
        &self.blocks
    }

    /// Order-preserving integer key of document `d`'s value (for sorting
    /// by the column without decoding).
    #[inline]
    pub fn sort_key(&self, d: DocId) -> Option<i64> {
        self.blocks.get(d)
    }

    /// The value of document `d` as a double.
    #[inline]
    pub fn get_f64(&self, d: DocId) -> Option<f64> {
        self.blocks.get(d).map(|k| self.kind.decode(k))
    }

    /// The value of document `d` as an integer (FLOAT values are truncated
    /// toward zero, saturating).
    #[inline]
    pub fn get_i64(&self, d: DocId) -> Option<i64> {
        self.blocks.get(d).map(|k| if self.kind == NumericKind::Int { k } else { self.kind.decode(k) as i64 })
    }

    /// The value of document `d` (`Int`, `Float` or `Null`).
    pub fn get(&self, d: DocId) -> Value {
        match self.blocks.get(d) {
            None => Value::Null,
            Some(k) if self.kind == NumericKind::Int => Value::Int(k),
            Some(k) => Value::Float(self.kind.decode(k)),
        }
    }

    /// Smallest value.
    pub fn min(&self) -> Option<f64> {
        self.blocks.key_bounds().map(|(lo, _)| self.kind.decode(lo))
    }

    /// Largest value.
    pub fn max(&self) -> Option<f64> {
        self.blocks.key_bounds().map(|(_, hi)| self.kind.decode(hi))
    }

    /// The keys of the values in `[lo, hi]` (inclusive; `+-inf` for open
    /// ends).
    pub fn key_range(&self, lo: f64, hi: f64) -> Option<KeyRange> {
        self.kind.key_range(lo, hi)
    }

    /// Documents with a value in `[lo, hi]`: a scan of the packed blocks,
    /// skipping blocks by their zone maps.
    pub fn scan(&self, lo: f64, hi: f64) -> DocSet {
        let n = self.len() as u32;
        match self.key_range(lo, hi) {
            Some(r) => DocSet::from_bits(self.blocks.scan_bits(r), n),
            None => DocSet::empty(n),
        }
    }

    /// Number of documents with a value in `[lo, hi]` (a scan that only
    /// counts).
    pub fn scan_count(&self, lo: f64, hi: f64) -> usize {
        self.key_range(lo, hi).map_or(0, |r| self.blocks.scan_count(r))
    }

    /// Whether document `d` has a value in `[lo, hi]`. Converts the bounds
    /// on every call; use [`NumericColumn::filter`] to probe many documents.
    pub fn probe(&self, d: DocId, lo: f64, hi: f64) -> bool {
        self.key_range(lo, hi).is_some_and(|r| self.blocks.get(d).is_some_and(|k| r.contains(k)))
    }

    /// A per-document check of `[lo, hi]` against the column.
    pub fn filter(&self, lo: f64, hi: f64) -> ColumnRangeFilter {
        ColumnRangeFilter { blocks: self.blocks.clone(), range: self.key_range(lo, hi), estimate: self.estimate(lo, hi).round() as usize }
    }

    /// Estimated number of documents with a value in `[lo, hi]`, from the
    /// histogram: covered buckets count fully, partially covered ones by
    /// the covered fraction of their value range. Without a histogram,
    /// values are assumed uniform between the minimum and maximum.
    pub fn estimate(&self, lo: f64, hi: f64) -> f64 {
        let Some(r) = self.key_range(lo, hi) else { return 0.0 };
        let Some((gmin, gmax)) = self.blocks.key_bounds() else { return 0.0 };
        if self.hist.is_empty() {
            return self.n_present() as f64 * self.fraction(r, gmin, gmax);
        }
        self.hist.overlapping(r).map(|i| {
            let (blo, bhi, c) = self.hist.bucket(i);
            c as f64 * self.fraction(r, blo, bhi)
        }).sum()
    }

    /// Fraction of the values in keys `[a, b]` that `r` covers, assuming
    /// they are uniform (over keys for integer and decimal columns, over
    /// values for IEEE keys).
    fn fraction(&self, r: KeyRange, a: i64, b: i64) -> f64 {
        let (lo, hi) = (r.lo.max(a), r.hi.min(b));
        if lo > hi {
            return 0.0;
        }
        if self.kind == NumericKind::FloatBits {
            let (va, vb) = (self.kind.decode(a), self.kind.decode(b));
            let (vl, vh) = (self.kind.decode(lo), self.kind.decode(hi));
            let f = if vb > va { (vh - vl) / (vb - va) } else { 1.0 };
            return if f.is_finite() { f.clamp(0.0, 1.0) } else { 1.0 };
        }
        ((hi as i128 - lo as i128 + 1) as f64 / (b as i128 - a as i128 + 1) as f64).clamp(0.0, 1.0)
    }

    /// Bytes per part: zone maps, block offsets, packed values, presence
    /// bitmap, histogram.
    pub fn byte_sizes(&self) -> [usize; 5] {
        let [z, o, d, p] = self.blocks.byte_sizes();
        [z, o, d, p, self.hist.byte_len()]
    }

    /// Whether the column's arrays are views into the segment.
    pub fn is_view(&self) -> bool {
        self.blocks.is_view()
    }

    /// Number of blocks a scan of `[lo, hi]` decodes (zone maps neither
    /// exclude nor fully include them).
    pub fn blocks_to_decode(&self, lo: f64, hi: f64) -> usize {
        let Some(r) = self.key_range(lo, hi) else { return 0 };
        (0..self.blocks.n_blocks()).filter(|&b| self.must_decode(b, r)).count()
    }

    /// Estimated fractions of the blocks whose zone map a scan of `r`
    /// checks (their superblock is cut by `r`) and of those it decodes,
    /// from at most 64 evenly spaced blocks (cheap enough for planning).
    pub fn scan_fractions(&self, r: KeyRange) -> (f64, f64) {
        let nb = self.blocks.n_blocks();
        if nb == 0 {
            return (0.0, 0.0);
        }
        let k = nb.min(64);
        let (mut checked, mut decoded) = (0, 0);
        for i in 0..k {
            let b = i * nb / k;
            let (min, max) = self.blocks.super_zone(b / super::blocks::SUPER);
            if cut(min, max, r) {
                checked += 1;
                decoded += self.must_decode(b, r) as usize;
            }
        }
        (checked as f64 / k as f64, decoded as f64 / k as f64)
    }

    fn must_decode(&self, b: usize, r: KeyRange) -> bool {
        let (min, max) = self.blocks.zone(b);
        cut(min, max, r)
    }
}

/// Whether `r` overlaps a zone `[min, max]` without containing it.
fn cut(min: i64, max: i64, r: KeyRange) -> bool {
    min <= max && max >= r.lo && min <= r.hi && !(r.lo <= min && max <= r.hi)
}

/// [`NumericColumn::filter`]: probes a document's key against a range.
#[derive(Clone, Debug)]
pub struct ColumnRangeFilter {
    blocks: Blocks,
    range: Option<KeyRange>,
    estimate: usize,
}

impl DocFilter for ColumnRangeFilter {
    #[inline]
    fn contains(&self, doc: DocId) -> bool {
        self.range.is_some_and(|r| self.blocks.get(doc).is_some_and(|k| r.contains(k)))
    }

    fn cardinality(&self) -> usize {
        self.estimate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    #[test]
    fn float_keys_order_like_doubles() {
        let mut rng = StdRng::seed_from_u64(1);
        let mut vals: Vec<f64> = vec![f64::NEG_INFINITY, -1e300, -1.0, -f64::MIN_POSITIVE, 0.0, f64::MIN_POSITIVE, 1.0, 1e300, f64::INFINITY];
        vals.extend((0..1000).map(|_| (rng.gen::<f64>() - 0.5) * 10f64.powi(rng.gen_range(-20..20))));
        for &a in &vals {
            assert_eq!(NumericKind::FloatBits.decode(float_key(a)), a);
            for &b in &vals {
                assert_eq!(a < b, float_key(a) < float_key(b), "{a} {b}");
            }
        }
        assert_eq!(float_key(-0.0), float_key(0.0));
        assert!(float_key(f64::NAN) > float_key(f64::INFINITY));
    }

    #[test]
    fn decimal_scale_detection() {
        assert_eq!(decimal_scale(&[1.0, 2.0, -7.0]), Some(0));
        assert_eq!(decimal_scale(&[0.07, 4.99, 1.5]), Some(2));
        assert_eq!(decimal_scale(&[1.005, 2.0]), Some(3));
        assert_eq!(decimal_scale(&[0.1 + 0.2]), None);
        assert_eq!(decimal_scale(&[f64::INFINITY]), None);
        assert_eq!(decimal_scale(&[1e300]), None);
        let (kind, keys) = encode_floats(&[Some(0.07), None, Some(4.99)]);
        assert_eq!(kind, NumericKind::Decimal(2));
        assert_eq!(keys, vec![Some(7), None, Some(499)]);
    }

    #[test]
    fn key_ranges_match_decoded_values() {
        let mut rng = StdRng::seed_from_u64(2);
        for kind in [NumericKind::Int, NumericKind::Decimal(0), NumericKind::Decimal(2), NumericKind::Decimal(5), NumericKind::FloatBits] {
            let keys: Vec<i64> = (0..300)
                .map(|_| match kind {
                    NumericKind::FloatBits => float_key((rng.gen::<f64>() - 0.5) * 1e3),
                    _ => rng.gen_range(-5000..5000),
                })
                .collect();
            for _ in 0..300 {
                let mut b = [(rng.gen::<f64>() - 0.5) * 120.0, (rng.gen::<f64>() - 0.5) * 120.0];
                if rng.gen_bool(0.3) {
                    // Bounds exactly on values.
                    b[0] = kind.decode(keys[rng.gen_range(0..keys.len())]);
                }
                if rng.gen_bool(0.1) {
                    b[rng.gen_range(0..2)] = if rng.gen_bool(0.5) { f64::INFINITY } else { f64::NEG_INFINITY };
                }
                let (lo, hi) = (b[0].min(b[1]), b[0].max(b[1]));
                let r = kind.key_range(lo, hi);
                for &k in &keys {
                    let v = kind.decode(k);
                    assert_eq!(r.is_some_and(|r| r.contains(k)), lo <= v && v <= hi, "{kind:?} [{lo}, {hi}] {v}");
                }
            }
        }
        assert_eq!(NumericKind::Int.key_range(2.5, 7.9), Some(KeyRange { lo: 3, hi: 7 }));
        assert_eq!(NumericKind::Int.key_range(9.3e18, f64::INFINITY), None);
        assert_eq!(NumericKind::Int.key_range(f64::NEG_INFINITY, f64::INFINITY), Some(KeyRange { lo: i64::MIN, hi: i64::MAX }));
        assert_eq!(NumericKind::Decimal(2).key_range(f64::NAN, 1.0), None);
    }

    #[test]
    fn equi_depth_buckets() {
        let sorted: Vec<i64> = (0..1000).collect();
        let (lo, hi, c) = equi_depth(&sorted, 64);
        assert_eq!(lo.len(), 64);
        assert!(c.iter().all(|&c| (15..=16).contains(&c)));
        assert_eq!((lo[0], *hi.last().unwrap()), (0, 999));
        // 35 distinct values, 64 buckets: one bucket per value.
        let mut sorted: Vec<i64> = (0..3500).map(|i| 1990 + i % 35).collect();
        sorted.sort();
        let (lo, hi, c) = equi_depth(&sorted, 64);
        assert_eq!(lo.len(), 35);
        assert!(lo.iter().zip(&hi).all(|(a, b)| a == b) && c.iter().all(|&c| c == 100));
        // A heavy value takes one bucket; the rest still split.
        let mut sorted: Vec<i64> = (0..1000).collect();
        sorted.extend(std::iter::repeat(500).take(5000));
        sorted.sort();
        let (lo, hi, c) = equi_depth(&sorted, 10);
        assert!(lo.windows(2).all(|w| w[0] < w[1]) && hi.windows(2).all(|w| w[0] < w[1]));
        assert!(lo.iter().zip(&hi).any(|(&a, &b)| a == 500 && b == 500));
        assert_eq!(c.iter().sum::<u32>(), 6000);
        assert!(equi_depth(&[], 64).0.is_empty());
    }
}
