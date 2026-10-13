//! Keyword columns: a sorted dictionary, a bit-packed code column and a
//! docID list per value.
//!
//! Sections:
//! * `kw.<field>.dict`: the distinct values in byte order, as `u32`
//!   offsets into one byte array; a value's code is its rank (binary search
//!   to look one up, an O(1) slice to print one).
//! * `kw.<field>.codes`: each document's code in the 128-document
//!   frame-of-reference blocks of the numeric columns (so `GROUP BY` can
//!   unpack whole blocks), with a presence bitmap if some documents have no
//!   value.
//! * `kw.<field>.post`: each value's documents. Values with at least
//!   `forward.kw_rare` documents (default [`DEFAULT_RARE`]) get a Pyramid
//!   list: O(1) membership probes at any density, fast unions for `IN`,
//!   count-only intersections for facets. Rarer values (a primary key, a
//!   long tail) share one sorted docID array (4 bytes per document, where a
//!   Pyramid's fixed cost is a few hundred bytes per list), located by a
//!   `u32` offset per value unless every value has exactly one document.

use std::collections::HashMap;

use posting_bench::bytes::{ByteReader, ByteWriter, Slice};
use posting_bench::codecs::pyramid::Pyramid;
use posting_bench::codecs::{PostingList, SetOps};

use super::blocks::{write_blocks, Blocks, BLOCK};
use super::lists::{build_list, merge_sorted, write_lists, ListTable};
use crate::error::{corrupt, Error, Result};
use crate::schema::Field;
use crate::segment::build::BuildOptions;
use crate::segment::{DocFilter, DocId, DocSet, FieldIndexBuilder, SegmentFile, SegmentWriter};
use crate::value::Value;

/// Values with fewer documents than this share the rare-value array.
pub const DEFAULT_RARE: usize = 64;

const VERSION: u64 = 1;

/// How [`KeywordColumn::facet_counts`] counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FacetStrategy {
    /// Pick by estimated cost.
    Auto,
    /// Read each result document's code into a counter array.
    Codes,
    /// Count-only intersection of the result with every value's list.
    Postings,
}

// --------------------------------------------------------------- builder

/// Builds the three sections of a KEYWORD field.
pub(crate) struct KeywordBuilder {
    name: String,
    rare: usize,
    ids: HashMap<String, u32>,
    /// Provisional id (first-seen order) per document.
    docs: Vec<Option<u32>>,
}

impl KeywordBuilder {
    pub fn new(field: &Field, opts: &BuildOptions) -> Result<Self> {
        let rare = super::usize_option(opts, "forward.kw_rare", DEFAULT_RARE)?;
        Ok(KeywordBuilder { name: field.name.clone(), rare, ids: HashMap::new(), docs: Vec::new() })
    }
}

impl FieldIndexBuilder for KeywordBuilder {
    fn add(&mut self, doc: DocId, value: &Value) -> Result<()> {
        self.docs.resize(doc as usize, None);
        self.docs.push(match value {
            Value::Null => None,
            Value::Str(s) => {
                let next = self.ids.len() as u32;
                Some(*self.ids.entry(s.clone()).or_insert(next))
            }
            _ => return Err(Error::Schema(format!("{}: not a string", self.name))),
        });
        Ok(())
    }

    fn finish(self: Box<Self>, out: &mut SegmentWriter) -> Result<()> {
        let n = out.n_docs();
        let mut values: Vec<(String, u32)> = self.ids.into_iter().collect();
        values.sort_unstable();
        let mut code_of = vec![0u32; values.len()];
        for (code, (_, id)) in values.iter().enumerate() {
            code_of[*id as usize] = code as u32;
        }
        let mut codes: Vec<Option<u32>> = self.docs.iter().map(|d| d.map(|id| code_of[id as usize])).collect();
        codes.resize(n, None);

        // Dictionary.
        let mut offs = Vec::with_capacity(values.len() + 1);
        let mut bytes = Vec::new();
        offs.push(0u32);
        for (v, _) in &values {
            bytes.extend_from_slice(v.as_bytes());
            offs.push(bytes.len() as u32);
        }
        let mut w = ByteWriter::new();
        w.u64(VERSION);
        w.array(&offs);
        w.array(&bytes);
        out.put(format!("kw.{}.dict", self.name), w.finish());

        // Codes.
        let mut w = ByteWriter::new();
        w.u64(VERSION);
        write_blocks(&codes.iter().map(|c| c.map(|c| c as i64)).collect::<Vec<_>>(), &mut w);
        out.put(format!("kw.{}.codes", self.name), w.finish());

        // Postings: documents grouped by code (counting sort keeps them
        // sorted), rare values in one array, the others as Pyramids.
        let nv = values.len();
        let mut start = vec![0usize; nv + 1];
        for c in codes.iter().flatten() {
            start[*c as usize + 1] += 1;
        }
        for i in 0..nv {
            start[i + 1] += start[i];
        }
        let mut grouped = vec![0u32; start[nv]];
        let mut fill = start.clone();
        for (d, c) in codes.iter().enumerate() {
            if let Some(c) = c {
                grouped[fill[*c as usize]] = d as u32;
                fill[*c as usize] += 1;
            }
        }
        let mut rare_offs = Vec::with_capacity(nv + 1);
        let mut rare = Vec::new();
        let mut pyr_codes = Vec::new();
        let mut lists = Vec::new();
        rare_offs.push(0u32);
        for c in 0..nv {
            let docs = &grouped[start[c]..start[c + 1]];
            if docs.len() < self.rare {
                rare.extend_from_slice(docs);
            } else {
                pyr_codes.push(c as u32);
                lists.push(build_list(docs, n));
            }
            rare_offs.push(rare.len() as u32);
        }
        // Every value on one document (a key): the offsets are implied.
        if nv > 0 && rare.len() == nv {
            rare_offs.clear();
        }
        let mut w = ByteWriter::new();
        w.u64(VERSION);
        w.array(&rare_offs);
        w.array(&rare);
        w.array(&pyr_codes);
        write_lists(&mut w, &lists);
        out.put(format!("kw.{}.post", self.name), w.finish());
        Ok(())
    }
}

// ---------------------------------------------------------------- reader

/// A value's documents.
enum List<'a> {
    Pyramid(&'a Pyramid),
    /// Sorted docIDs.
    Rare(&'a [u32]),
}

/// A zero-copy view of a KEYWORD field's dictionary, codes and postings.
#[derive(Clone, Debug)]
pub struct KeywordColumn {
    name: String,
    n: usize,
    dict_offs: Slice<u32>,
    dict_bytes: Slice<u8>,
    codes: Blocks,
    /// Start of each value's documents in `rare` (empty values have a
    /// Pyramid list); empty when every value has exactly one document.
    rare_offs: Slice<u32>,
    rare: Slice<u32>,
    /// Codes of the values with a Pyramid list (increasing), indexing
    /// `lists`.
    pyr_codes: Slice<u32>,
    lists: ListTable,
}

impl KeywordColumn {
    pub fn open(seg: &SegmentFile, field: &str) -> Result<Self> {
        let version = |r: &mut ByteReader| -> Result<()> {
            if r.u64().map_err(corrupt)? != VERSION {
                return Err(corrupt("bad keyword section version"));
            }
            Ok(())
        };
        let mut r = ByteReader::new(seg.require(&format!("kw.{field}.dict"))?);
        version(&mut r)?;
        let dict_offs: Slice<u32> = r.array().map_err(corrupt)?;
        let dict_bytes: Slice<u8> = r.array().map_err(corrupt)?;
        let mut r = ByteReader::new(seg.require(&format!("kw.{field}.codes"))?);
        version(&mut r)?;
        let codes = Blocks::read(&mut r)?;
        let post = seg.require(&format!("kw.{field}.post"))?;
        let mut r = ByteReader::new(post.clone());
        version(&mut r)?;
        let rare_offs: Slice<u32> = r.array().map_err(corrupt)?;
        let rare: Slice<u32> = r.array().map_err(corrupt)?;
        let pyr_codes: Slice<u32> = r.array().map_err(corrupt)?;
        let lists = ListTable::read(&mut r, &post)?;
        let nv = dict_offs.len().saturating_sub(1);
        if dict_offs.is_empty()
            || dict_offs.last().map(|&o| o as usize) != Some(dict_bytes.len())
            || codes.len() != seg.n_docs()
            || codes.key_bounds().is_some_and(|(lo, hi)| lo < 0 || hi as usize >= nv)
            || !(rare_offs.len() == nv + 1 && rare_offs.last().map(|&o| o as usize) == Some(rare.len())
                || rare_offs.is_empty() && nv > 0 && rare.len() == nv)
            || pyr_codes.len() != lists.len()
        {
            return Err(corrupt(format!("bad keyword column {field}")));
        }
        Ok(KeywordColumn { name: field.to_string(), n: seg.n_docs(), dict_offs, dict_bytes, codes, rare_offs, rare, pyr_codes, lists })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of documents.
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Number of distinct values.
    pub fn n_values(&self) -> usize {
        self.dict_offs.len() - 1
    }

    fn value_bytes(&self, code: u32) -> &[u8] {
        let (a, b) = (self.dict_offs[code as usize] as usize, self.dict_offs[code as usize + 1] as usize);
        &self.dict_bytes[a.min(b)..b]
    }

    /// The code of `value` (its rank among the distinct values).
    pub fn code(&self, value: &str) -> Option<u32> {
        let (mut lo, mut hi) = (0usize, self.n_values());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.value_bytes(mid as u32).cmp(value.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid as u32),
            }
        }
        None
    }

    /// The value of `code`.
    ///
    /// # Panics
    /// If `code >= n_values()`.
    pub fn value(&self, code: u32) -> &str {
        std::str::from_utf8(self.value_bytes(code)).unwrap_or("\u{fffd}")
    }

    /// The code of document `d`'s value.
    #[inline]
    pub fn code_of_doc(&self, d: DocId) -> Option<u32> {
        self.codes.get(d).map(|c| c as u32)
    }

    /// The value of document `d`.
    pub fn value_of_doc(&self, d: DocId) -> Option<&str> {
        self.code_of_doc(d).map(|c| self.value(c))
    }

    /// Range of value `code`'s documents in the rare-value array (empty
    /// if it has a Pyramid list).
    #[inline]
    fn rare_range(&self, code: u32) -> (usize, usize) {
        let c = code as usize;
        if self.rare_offs.is_empty() {
            return (c, c + 1);
        }
        (self.rare_offs[c] as usize, self.rare_offs[c + 1] as usize)
    }

    fn list(&self, code: u32) -> List<'_> {
        let (a, b) = self.rare_range(code);
        if a < b {
            return List::Rare(&self.rare[a..b]);
        }
        match self.pyr_codes.binary_search(&code) {
            Ok(i) => List::Pyramid(self.lists.get(i)),
            Err(_) => List::Rare(&[]),
        }
    }

    /// Number of documents with value `code`.
    pub fn count_code(&self, code: u32) -> usize {
        let (a, b) = self.rare_range(code);
        if a < b {
            return b - a;
        }
        self.pyr_codes.binary_search(&code).map_or(0, |i| self.lists.card(i) as usize)
    }

    /// Number of documents with `value`.
    pub fn count(&self, value: &str) -> usize {
        self.code(value).map_or(0, |c| self.count_code(c))
    }

    /// Documents with value `code`.
    pub fn docs_code(&self, code: u32) -> DocSet {
        let n = self.n as u32;
        match self.list(code) {
            List::Rare(d) => DocSet::from_sorted(d.to_vec(), n),
            List::Pyramid(p) if p.cardinality() as usize > self.n / 32 => {
                let mut words = vec![0u64; self.n.div_ceil(64)];
                p.or_into_bits(&mut words);
                DocSet::from_bits(words, n)
            }
            List::Pyramid(p) => {
                let mut out = Vec::with_capacity(p.cardinality() as usize);
                p.decode_into(&mut out);
                DocSet::from_sorted(out, n)
            }
        }
    }

    /// Documents with `value` (`= value`).
    pub fn docs(&self, value: &str) -> DocSet {
        self.code(value).map_or_else(|| DocSet::empty(self.n as u32), |c| self.docs_code(c))
    }

    /// Documents with any of `values` (`IN (...)`): the union of their
    /// lists (an OR into a bitset when dense, Pyramid's k-way union
    /// otherwise).
    pub fn docs_in(&self, values: &[&str]) -> DocSet {
        let mut codes: Vec<u32> = values.iter().filter_map(|v| self.code(v)).collect();
        codes.sort_unstable();
        codes.dedup();
        let n = self.n as u32;
        let total: usize = codes.iter().map(|&c| self.count_code(c)).sum();
        if total > self.n / 32 {
            let mut words = vec![0u64; self.n.div_ceil(64)];
            for &c in &codes {
                match self.list(c) {
                    List::Pyramid(p) => p.or_into_bits(&mut words),
                    List::Rare(d) => d.iter().for_each(|&d| words[d as usize / 64] |= 1 << (d % 64)),
                }
            }
            return DocSet::from_bits(words, n);
        }
        let mut pyrs = Vec::new();
        let mut rare = Vec::new();
        for &c in &codes {
            match self.list(c) {
                List::Pyramid(p) => pyrs.push(p),
                List::Rare(d) => rare.extend_from_slice(d),
            }
        }
        let mut out = Vec::with_capacity(total);
        Pyramid::union(&pyrs, &mut out);
        rare.sort_unstable();
        DocSet::from_sorted(merge_sorted(out, rare), n)
    }

    /// A membership probe for `value`: O(1) in its Pyramid list (a binary
    /// search for rare values).
    pub fn filter(&self, value: &str) -> KeywordFilter {
        let Some(code) = self.code(value) else { return KeywordFilter::Rare { docs: Slice::default(), start: 0, end: 0 } };
        let (a, b) = self.rare_range(code);
        match self.pyr_codes.binary_search(&code) {
            Ok(i) if a == b => KeywordFilter::List(self.lists.get(i).clone()),
            _ => KeywordFilter::Rare { docs: self.rare.clone(), start: a, end: b },
        }
    }

    /// A probe for `IN (values)` against the code column: one code lookup
    /// and one bit test per document, for any number of values.
    pub fn filter_in(&self, values: &[&str]) -> CodeSetFilter {
        let mut set = vec![0u64; self.n_values().div_ceil(64)];
        let mut card = 0;
        for v in values {
            if let Some(c) = self.code(v) {
                if set[c as usize / 64] >> (c % 64) & 1 == 0 {
                    set[c as usize / 64] |= 1 << (c % 64);
                    card += self.count_code(c);
                }
            }
        }
        CodeSetFilter { codes: self.codes.clone(), set, card }
    }

    /// Number of documents of `docs` per value, by `strategy`: pairs of
    /// value and count, count descending (ties by value), zero counts
    /// omitted. Documents without a value are not counted.
    pub fn facet_counts(&self, docs: &DocSet, strategy: FacetStrategy) -> Vec<(String, u64)> {
        let counts = self.facet_code_counts(docs, strategy);
        let mut out: Vec<(String, u64)> =
            counts.iter().enumerate().filter(|(_, &c)| c > 0).map(|(code, &c)| (self.value(code as u32).to_string(), c)).collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Count per code of the documents of `docs`.
    pub fn facet_code_counts(&self, docs: &DocSet, strategy: FacetStrategy) -> Vec<u64> {
        let strategy = match strategy {
            FacetStrategy::Auto => self.choose_facet_strategy(docs),
            s => s,
        };
        match strategy {
            FacetStrategy::Postings => self.facet_by_postings(docs),
            _ => self.facet_by_codes(docs),
        }
    }

    /// The cheaper facet strategy for `docs`, by a cost model fitted to
    /// strata-forward-bench (see notes/forward.md). Reading codes costs per
    /// result document and per block it touches; intersecting costs per
    /// list: per bitset word for dense lists (Flat chunks and dense blocks
    /// are ANDed word by word), per member for sparse ones, plus building
    /// the bitset of a sorted result. Postings win for large results over
    /// few, dense values.
    pub fn choose_facet_strategy(&self, docs: &DocSet) -> FacetStrategy {
        if self.lists.len() == 0 {
            return FacetStrategy::Codes;
        }
        let d = docs.len() as f64;
        let n = self.n as f64;
        let words = n / 64.0;
        let codes = d * cost::CODE_DOC + d.min(n / BLOCK as f64) * cost::CODE_BLOCK;
        let lists: f64 = (0..self.lists.len())
            .map(|i| {
                let c = self.lists.card(i) as f64;
                if c > cost::DENSE_LIST * n {
                    words * cost::LIST_WORD
                } else {
                    c * cost::LIST_DOC
                }
            })
            .sum();
        let bits = if matches!(docs, DocSet::Sorted { .. }) { words * cost::BITSET_WORD + d * cost::BITSET_DOC } else { 0.0 };
        let postings = lists + self.rare.len() as f64 * cost::RARE_DOC + bits;
        if postings < codes {
            FacetStrategy::Postings
        } else {
            FacetStrategy::Codes
        }
    }

    fn facet_by_codes(&self, docs: &DocSet) -> Vec<u64> {
        let mut counts = vec![0u64; self.n_values()];
        match docs {
            DocSet::Sorted { docs, .. } => {
                for &d in docs {
                    if let Some(c) = self.codes.get(d) {
                        counts[c as usize] += 1;
                    }
                }
            }
            DocSet::Bits { words, .. } => {
                let mut buf = [0u32; BLOCK];
                for b in 0..self.codes.n_blocks() {
                    let w = [words.get(2 * b).copied().unwrap_or(0), words.get(2 * b + 1).copied().unwrap_or(0)];
                    let ones = w[0].count_ones() + w[1].count_ones();
                    if ones == 0 {
                        continue;
                    }
                    let p = self.codes.present_mask(b);
                    let w = [w[0] & p[0], w[1] & p[1]];
                    // Unpack the block when it pays for itself (codes are
                    // narrow, so a block unpacks in a few ns).
                    if ones >= DECODE_MIN {
                        if let Some(base) = self.codes.decode_block_u32(b, &mut buf) {
                            for (h, mut x) in w.into_iter().enumerate() {
                                while x != 0 {
                                    let j = 64 * h + x.trailing_zeros() as usize;
                                    x &= x - 1;
                                    counts[(base + buf[j] as i64) as usize] += 1;
                                }
                            }
                            continue;
                        }
                    }
                    for (h, mut x) in w.into_iter().enumerate() {
                        while x != 0 {
                            let d = (b * BLOCK + 64 * h) as u32 + x.trailing_zeros();
                            x &= x - 1;
                            counts[self.codes.raw(d) as usize] += 1;
                        }
                    }
                }
            }
        }
        counts
    }

    fn facet_by_postings(&self, docs: &DocSet) -> Vec<u64> {
        let mut counts = vec![0u64; self.n_values()];
        // Count-only intersections of each value's list with the result as
        // a bitset.
        let tmp;
        let words = match docs {
            DocSet::Bits { words, .. } => words,
            DocSet::Sorted { .. } => {
                tmp = docs.to_bits();
                &tmp
            }
        };
        for (i, &code) in self.pyr_codes.iter().enumerate() {
            counts[code as usize] = self.lists.get(i).and_count_bits(words);
        }
        for code in 0..self.n_values() {
            let (a, b) = self.rare_range(code as u32);
            if a < b {
                counts[code] = self.rare[a..b].iter().filter(|&&d| words[d as usize / 64] >> (d % 64) & 1 == 1).count() as u64;
            }
        }
        counts
    }

    /// Bytes of the dictionary, the code column (zone maps, offsets,
    /// payload, presence), the rare-value array and the Pyramid lists.
    pub fn byte_sizes(&self) -> KeywordSizes {
        KeywordSizes {
            dict: self.dict_offs.byte_len() + self.dict_bytes.byte_len(),
            codes: self.codes.byte_sizes(),
            rare: self.rare_offs.byte_len() + self.rare.byte_len(),
            lists: self.lists.lists_bytes(),
        }
    }

    /// Whether the column's arrays are views into the segment.
    pub fn is_view(&self) -> bool {
        self.codes.is_view()
            && [&self.dict_offs, &self.rare_offs, &self.rare, &self.pyr_codes].iter().all(|s| matches!(s, Slice::Shared { .. }))
            && matches!(self.dict_bytes, Slice::Shared { .. })
            && (0..self.lists.len()).all(|i| self.lists.get(i).is_view())
    }
}

/// [`KeywordColumn::byte_sizes`].
#[derive(Clone, Copy, Debug)]
pub struct KeywordSizes {
    pub dict: usize,
    pub codes: [usize; 4],
    pub rare: usize,
    pub lists: usize,
}

/// Blocks of a bitset result with at least this many members are counted
/// by unpacking all their codes.
const DECODE_MIN: u32 = 8;

/// Facet cost model (ns), fitted to strata-forward-bench on TREC-COVID
/// and Quora.
mod cost {
    /// Codes: per result document, and per 128-document block touched
    /// (a lookup, or an unpacked block).
    pub const CODE_DOC: f64 = 1.0;
    pub const CODE_BLOCK: f64 = 6.0;
    /// Postings: lists holding more than this fraction of the documents are
    /// mostly Flat chunks and dense blocks.
    pub const DENSE_LIST: f64 = 0.07;
    /// Postings: per bitset word for a dense list, per member of a sparse
    /// list or of the rare-value array.
    pub const LIST_WORD: f64 = 0.3;
    pub const LIST_DOC: f64 = 0.6;
    pub const RARE_DOC: f64 = 1.5;
    /// Postings over a sorted result: building its bitset, per word and
    /// per member.
    pub const BITSET_WORD: f64 = 0.5;
    pub const BITSET_DOC: f64 = 1.0;
}

/// [`KeywordColumn::filter`]: membership in one value's documents.
#[derive(Clone, Debug)]
pub enum KeywordFilter {
    List(Pyramid),
    /// `docs[start..end]`, sorted.
    Rare { docs: Slice<u32>, start: usize, end: usize },
}

impl DocFilter for KeywordFilter {
    #[inline]
    fn contains(&self, doc: DocId) -> bool {
        match self {
            KeywordFilter::List(p) => p.contains(doc),
            KeywordFilter::Rare { docs, start, end } => docs[*start..*end].binary_search(&doc).is_ok(),
        }
    }

    fn cardinality(&self) -> usize {
        match self {
            KeywordFilter::List(p) => p.cardinality() as usize,
            KeywordFilter::Rare { start, end, .. } => end - start,
        }
    }
}

/// [`KeywordColumn::filter_in`]: a document's code tested against a set.
#[derive(Clone, Debug)]
pub struct CodeSetFilter {
    codes: Blocks,
    set: Vec<u64>,
    card: usize,
}

impl DocFilter for CodeSetFilter {
    #[inline]
    fn contains(&self, doc: DocId) -> bool {
        self.codes.get(doc).is_some_and(|c| self.set[c as usize / 64] >> (c % 64) & 1 == 1)
    }

    fn cardinality(&self) -> usize {
        self.card
    }
}
