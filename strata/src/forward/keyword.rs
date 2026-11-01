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
//! * `kw.<field>.multi`: present when the field is multi-valued
//!   ([`Field::multi`]). The postings then hold every (value, document)
//!   pair, the code column only each document's first value, so filters and
//!   facets go through the postings. INT values are stored as [`int_key`]
//!   strings, whose byte order is numeric order, so a range is a run of
//!   codes.
//!
//! A MEDIA field ([`Field::media`]) is a keyword column of its values'
//! `sha256`: the doc store keeps the whole value. A JSON field
//! ([`crate::schema::FieldType::Json`]) is a multi-valued keyword column of
//! its objects' terms ([`crate::json`]).

use std::borrow::Cow;
use std::collections::HashMap;

use crate::postings::bytes::{ByteReader, ByteWriter, Slice};
use crate::postings::codecs::pyramid::Pyramid;
use crate::postings::codecs::{PostingList, SetOps};

use super::blocks::{write_blocks, Blocks, BLOCK};
use super::lists::{build_list, merge_sorted, write_lists, ListTable};
use crate::error::{corrupt, Error, Result};
use crate::schema::Field;
use crate::segment::build::BuildOptions;
use crate::segment::{DocFilter, DocId, DocSet, FieldIndexBuilder, SegmentFile, SegmentWriter};
use crate::value::Value;

/// Values with fewer documents than this share the rare-value array.
pub const DEFAULT_RARE: usize = 64;

/// A prefix matching more values than this scans the code column of a
/// single-valued field instead of unioning the values' lists, and its
/// count is estimated.
pub const PREFIX_LISTS: usize = 1024;

const VERSION: u64 = 1;

/// An INT value as a keyword: 16 hex digits of the value with its sign bit
/// flipped, so that byte order is numeric order.
pub fn int_key(x: i64) -> String {
    format!("{:016x}", (x as u64) ^ (1 << 63))
}

/// The INT value of an [`int_key`].
pub fn int_of_key(s: &str) -> Option<i64> {
    u64::from_str_radix(s, 16).ok().map(|u| (u ^ (1 << 63)) as i64)
}

/// The keyword a predicate value matches in a keyword column: a string
/// itself, an integer as its [`int_key`].
pub fn key_of(v: &Value) -> Option<Cow<'_, str>> {
    match v {
        Value::Str(s) => Some(Cow::Borrowed(s)),
        Value::Int(x) => Some(Cow::Owned(int_key(*x))),
        _ => None,
    }
}

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
    /// Names its sections ([`Field::key`]).
    key: String,
    rare: usize,
    ids: HashMap<String, u32>,
    /// Provisional id (first-seen order) per document.
    docs: Vec<Option<u32>>,
    multi: bool,
    /// A MEDIA field ([`Field::media`]), indexed by its values' `sha256`.
    media: bool,
    /// A multi-valued field's values after each document's first.
    extra: Vec<(DocId, u32)>,
}

impl KeywordBuilder {
    pub fn new(field: &Field, opts: &BuildOptions) -> Result<Self> {
        let rare = super::usize_option(opts, "forward.kw_rare", DEFAULT_RARE)?;
        // A JSON field's column holds each row's terms (crate::json).
        let multi = field.multi || field.ty == crate::schema::FieldType::Json;
        Ok(KeywordBuilder { name: field.name.clone(), key: field.key(), rare, ids: HashMap::new(), docs: Vec::new(), multi, media: field.media.is_some(), extra: Vec::new() })
    }

    fn id(&mut self, v: &Value) -> Result<u32> {
        let hash;
        let key = match (v, self.multi) {
            (Value::Str(s), _) if self.media => {
                hash = crate::media::hash_of(s).ok_or_else(|| Error::Schema(format!("{}: not a MEDIA value", self.name)))?;
                Cow::Borrowed(hash.as_str())
            }
            (Value::Str(_), _) | (Value::Int(_), true) => key_of(v).expect("string or integer"),
            _ => return Err(Error::Schema(format!("{}: not a string", self.name))),
        };
        let next = self.ids.len() as u32;
        Ok(match self.ids.get(key.as_ref()) {
            Some(&id) => id,
            None => *self.ids.entry(key.into_owned()).or_insert(next),
        })
    }
}

impl FieldIndexBuilder for KeywordBuilder {
    fn add(&mut self, doc: DocId, value: &Value) -> Result<()> {
        self.docs.resize(doc as usize, None);
        let items = if self.multi { value.items() } else if value == &Value::Null { &[] } else { std::slice::from_ref(value) };
        let mut first = None;
        for v in items {
            let id = self.id(v)?;
            match first {
                None => first = Some(id),
                Some(_) => self.extra.push((doc, id)),
            }
        }
        self.docs.push(first);
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
        out.put(format!("kw.{}.dict", self.key), w.finish());

        // Codes.
        let mut w = ByteWriter::new();
        w.u64(VERSION);
        write_blocks(&codes.iter().map(|c| c.map(|c| c as i64)).collect::<Vec<_>>(), &mut w);
        out.put(format!("kw.{}.codes", self.key), w.finish());

        // Postings: documents grouped by code (counting sort keeps them
        // sorted), rare values in one array, the others as Pyramids. A
        // multi-valued field adds its other (document, code) pairs.
        let nv = values.len();
        let mut pairs: Vec<(u32, u32)> = codes.iter().enumerate().filter_map(|(d, c)| c.map(|c| (d as u32, c))).collect();
        if self.multi {
            pairs.extend(self.extra.iter().map(|&(d, id)| (d, code_of[id as usize])));
            pairs.sort_unstable();
            pairs.dedup();
        }
        let mut start = vec![0usize; nv + 1];
        for &(_, c) in &pairs {
            start[c as usize + 1] += 1;
        }
        for i in 0..nv {
            start[i + 1] += start[i];
        }
        let mut grouped = vec![0u32; start[nv]];
        let mut fill = start.clone();
        for &(d, c) in &pairs {
            grouped[fill[c as usize]] = d;
            fill[c as usize] += 1;
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
        // Every value on one document (a key): the offsets are implied. As
        // many rare documents as values is not enough: with a Pyramid list,
        // some rare value has several documents.
        if nv > 0 && rare.len() == nv && lists.is_empty() {
            rare_offs.clear();
        }
        let mut w = ByteWriter::new();
        w.u64(VERSION);
        w.array(&rare_offs);
        w.array(&rare);
        w.array(&pyr_codes);
        write_lists(&mut w, &lists);
        out.put(format!("kw.{}.post", self.key), w.finish());
        if self.multi {
            let mut w = ByteWriter::new();
            w.u64(VERSION);
            out.put(format!("kw.{}.multi", self.key), w.finish());
        }
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
    /// Multi-valued: documents can have several codes, and `codes` holds
    /// only the first.
    multi: bool,
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
        let multi = seg.has(&format!("kw.{field}.multi"));
        let mut col = KeywordColumn { name: field.to_string(), n: seg.n_docs(), dict_offs, dict_bytes, codes, rare_offs, rare, pyr_codes, lists, multi };
        if col.needs_values() && !multi {
            let mut count = vec![0u32; nv];
            for d in 0..col.n as u32 {
                if let Some(c) = col.codes.get(d) {
                    count[c as usize] += 1;
                }
            }
            col.set_rare_offsets(&count)?;
        }
        Ok(col)
    }

    /// Whether the column comes from a writer that left out the offsets of
    /// the rare values whenever they held as many documents as there are
    /// values, Pyramid lists or not, so that its rare values cannot be told
    /// apart. [`KeywordColumn::open`] rebuilds the offsets of a
    /// single-valued column from its codes; a multi-valued one needs each
    /// document's values ([`KeywordColumn::repair`]).
    pub fn needs_values(&self) -> bool {
        self.rare_offs.is_empty() && !self.pyr_codes.is_empty()
    }

    /// Rebuilds the rare values' offsets of a column that
    /// [`KeywordColumn::needs_values`] from `values`, each document's value
    /// of the field (a list for a multi-valued field), as stored.
    pub fn repair(&mut self, values: &[Value]) -> Result<()> {
        if !self.needs_values() {
            return Ok(());
        }
        if values.len() != self.n {
            return Err(corrupt(format!("keyword column {}: {} stored values for {} documents", self.name, values.len(), self.n)));
        }
        let mut count = vec![0u32; self.n_values()];
        let mut seen = Vec::new();
        for v in values {
            seen.clear();
            for x in v.items() {
                match key_of(x).and_then(|k| self.code(&k)) {
                    Some(c) => seen.push(c),
                    None => return Err(corrupt(format!("keyword column {}: a stored value is not in the dictionary", self.name))),
                }
            }
            seen.sort_unstable();
            seen.dedup();
            for &c in &seen {
                count[c as usize] += 1;
            }
        }
        self.set_rare_offsets(&count)
    }

    /// Sets the offsets in `rare` from the number of documents of each
    /// value, checked against the array and the lists.
    fn set_rare_offsets(&mut self, count: &[u32]) -> Result<()> {
        let mut offs = Vec::with_capacity(count.len() + 1);
        offs.push(0u32);
        let mut at = 0u32;
        for (c, &k) in count.iter().enumerate() {
            match self.pyr_codes.binary_search(&(c as u32)) {
                Ok(i) if self.lists.card(i) == k as u64 => {}
                Ok(_) => return Err(corrupt(format!("keyword column {}: list {c} disagrees with the codes", self.name))),
                Err(_) if k > 0 => at += k,
                Err(_) => return Err(corrupt(format!("keyword column {}: value {c} has no documents", self.name))),
            }
            offs.push(at);
        }
        if at as usize != self.rare.len() {
            return Err(corrupt(format!("keyword column {}: rare documents disagree with the codes", self.name)));
        }
        self.rare_offs = Slice::Owned(offs);
        Ok(())
    }

    /// Whether documents can have several values (filters and facets must
    /// then use the postings, not the code column).
    pub fn is_multi(&self) -> bool {
        self.multi
    }

    /// The first code whose value is not below `value`.
    pub fn lower_bound(&self, value: &str) -> u32 {
        let (mut lo, mut hi) = (0usize, self.n_values());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.value_bytes(mid as u32) < value.as_bytes() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo as u32
    }

    /// Codes of the values starting with `prefix`: one run, since values
    /// are in byte order.
    pub fn prefix_codes(&self, prefix: &str) -> std::ops::Range<u32> {
        let start = self.lower_bound(prefix);
        let (mut lo, mut hi) = (start as usize, self.n_values());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.value_bytes(mid as u32).starts_with(prefix.as_bytes()) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        start..lo as u32
    }

    /// The runs of codes whose values start with `prefix` or, with
    /// `edits`, with a prefix within that many edits of it whose first
    /// letter is its own ([`crate::text::fuzzy::Match::Prefix`]), in
    /// order, a run of the dictionary per prefix found.
    pub fn prefix_runs(&self, prefix: &str, edits: u8) -> Vec<std::ops::Range<u32>> {
        use crate::text::fuzzy::{walk, Match};
        if edits == 0 {
            let r = self.prefix_codes(prefix);
            return if r.is_empty() { Vec::new() } else { vec![r] };
        }
        let q: Vec<char> = prefix.chars().collect();
        let mut runs: Vec<std::ops::Range<u32>> = Vec::new();
        walk(&q, edits, Match::Prefix, &mut ValueCursor { col: self, pos: 0 }, |_, r, _| {
            let r = r.start as u32..r.end as u32;
            match runs.last_mut() {
                Some(last) if last.end == r.start => last.end = r.end,
                _ => runs.push(r),
            }
        });
        runs
    }

    /// Documents with a value in `runs` of [`Self::prefix_runs`]
    /// (multi-valued: at least one). A long run of a single-valued field
    /// scans the code column, as a range of numbers does; short ones union
    /// their lists.
    pub fn docs_prefix(&self, runs: &[std::ops::Range<u32>]) -> DocSet {
        let n = self.n as u32;
        let (long, short): (Vec<_>, Vec<_>) = runs.iter().cloned().partition(|r| !self.multi && r.len() > PREFIX_LISTS);
        let short: Vec<u32> = short.into_iter().flatten().collect();
        let mut out = (!short.is_empty()).then(|| self.docs_codes(&short));
        for r in long {
            let r = super::KeyRange { lo: r.start as i64, hi: r.end as i64 - 1 };
            let set = DocSet::from_bits(self.codes.scan_bits(r), n);
            out = Some(match out {
                Some(o) => o.union(&set),
                None => set,
            });
        }
        out.unwrap_or_else(|| DocSet::empty(n))
    }

    /// Documents with a value in `runs`: exact for runs of up to
    /// [`PREFIX_LISTS`] values, else a run's share of the documents with a
    /// value.
    pub fn count_prefix(&self, runs: &[std::ops::Range<u32>]) -> usize {
        runs.iter()
            .map(|r| match r.len() {
                l if l <= PREFIX_LISTS => r.clone().map(|c| self.count_code(c)).sum(),
                l => (self.n_present() as f64 * l as f64 / self.n_values().max(1) as f64) as usize,
            })
            .sum()
    }

    /// A probe for `runs` against the code column of a single-valued
    /// field: one code lookup and a range test per document.
    pub fn filter_prefix(&self, runs: &[std::ops::Range<u32>]) -> CodeRangeFilter {
        let ranges = runs.iter().map(|r| (r.start as i64, r.end as i64)).collect();
        CodeRangeFilter { card: self.count_prefix(runs), codes: self.codes.clone(), ranges }
    }

    /// Codes of the INT values in `lo..=hi` of a multi-valued INT field.
    pub fn int_codes(&self, lo: f64, hi: f64) -> std::ops::Range<u32> {
        const TWO63: f64 = 9_223_372_036_854_775_808.0;
        let (lo, hi) = (lo.ceil(), hi.floor());
        if !(lo <= hi) || lo >= TWO63 || hi < -TWO63 {
            return 0..0;
        }
        // `as` saturates at the i64 limits.
        self.int_codes_exact(lo as i64, if hi >= TWO63 { i64::MAX } else { hi as i64 })
    }

    /// Codes of the INT values in `lo..=hi` (exact bounds).
    pub fn int_codes_exact(&self, lo: i64, hi: i64) -> std::ops::Range<u32> {
        if lo > hi {
            return 0..0;
        }
        let a = self.lower_bound(&int_key(lo));
        let b = match hi.checked_add(1) {
            Some(h) => self.lower_bound(&int_key(h)),
            None => self.n_values() as u32,
        };
        a..b.max(a)
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

    /// Number of documents with value `code` among those set in `words`
    /// (a bitset of the segment's documents).
    pub fn count_code_in(&self, code: u32, words: &[u64]) -> u64 {
        match self.list(code) {
            List::Rare(d) => d.iter().filter(|&&d| words[d as usize / 64] >> (d % 64) & 1 == 1).count() as u64,
            List::Pyramid(p) => p.and_count_bits(words),
        }
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

    /// The documents with value `code`, in order, into `out` (cleared
    /// first).
    pub fn docs_code_into(&self, code: u32, out: &mut Vec<DocId>) {
        out.clear();
        match self.list(code) {
            List::Rare(d) => out.extend_from_slice(d),
            List::Pyramid(p) => p.decode_into(out),
        }
    }

    /// Documents with a value (multi-valued: at least one).
    pub fn present(&self) -> DocSet {
        DocSet::from_bits(self.codes.scan_bits(super::KeyRange { lo: i64::MIN, hi: i64::MAX }), self.n as u32)
    }

    /// Number of documents with a value.
    pub fn n_present(&self) -> usize {
        self.codes.n_present()
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
        self.docs_codes(&codes)
    }

    /// Documents with any of `codes` (sorted, distinct).
    pub fn docs_codes(&self, codes: &[u32]) -> DocSet {
        let n = self.n as u32;
        let total: usize = codes.iter().map(|&c| self.count_code(c)).sum();
        if total > self.n / 32 {
            let mut words = vec![0u64; self.n.div_ceil(64)];
            for &c in codes {
                match self.list(c) {
                    List::Pyramid(p) => p.or_into_bits(&mut words),
                    List::Rare(d) => d.iter().for_each(|&d| words[d as usize / 64] |= 1 << (d % 64)),
                }
            }
            return DocSet::from_bits(words, n);
        }
        let mut pyrs = Vec::new();
        let mut rare = Vec::new();
        for &c in codes {
            match self.list(c) {
                List::Pyramid(p) => pyrs.push(p),
                List::Rare(d) => rare.extend_from_slice(d),
            }
        }
        let mut out = Vec::with_capacity(total);
        Pyramid::union(&pyrs, &mut out);
        rare.sort_unstable();
        let mut all = merge_sorted(out, rare);
        if self.multi {
            // A document can be on several of the lists.
            all.dedup();
        }
        DocSet::from_sorted(all, n)
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
            _ if self.multi => FacetStrategy::Postings,
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
        if self.multi {
            return FacetStrategy::Postings;
        }
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

/// Documents whose code is in one of some runs `lo..hi`: a prefix's run
/// of values, or the runs of a prefix with typos
/// ([`KeywordColumn::filter_prefix`]).
pub struct CodeRangeFilter {
    codes: Blocks,
    /// In order, apart.
    ranges: Vec<(i64, i64)>,
    card: usize,
}

impl DocFilter for CodeRangeFilter {
    #[inline]
    fn contains(&self, doc: DocId) -> bool {
        self.codes.get(doc).is_some_and(|c| match self.ranges[..] {
            [(lo, hi)] => lo <= c && c < hi,
            _ => {
                let i = self.ranges.partition_point(|&(_, hi)| hi <= c);
                self.ranges.get(i).is_some_and(|&(lo, _)| lo <= c)
            }
        })
    }

    fn cardinality(&self) -> usize {
        self.card
    }

    fn may_have(&self, docs: std::ops::Range<DocId>) -> bool {
        // A few runs one by one, more as the span from the first to the
        // last.
        let (Some(&(lo, _)), Some(&(_, hi))) = (self.ranges.first(), self.ranges.last()) else { return false };
        match self.ranges.len() {
            1..=4 => self.ranges.iter().any(|&(lo, hi)| self.codes.may_have(docs.clone(), super::KeyRange { lo, hi: hi - 1 })),
            _ => self.codes.may_have(docs, super::KeyRange { lo, hi: hi - 1 }),
        }
    }
}

/// A dictionary's values in order, by code, for typo walks.
struct ValueCursor<'a> {
    col: &'a KeywordColumn,
    pos: usize,
}

impl crate::text::fuzzy::Sorted for ValueCursor<'_> {
    fn term(&self) -> Option<&[u8]> {
        (self.pos < self.col.n_values()).then(|| self.col.value_bytes(self.pos as u32))
    }

    fn pos(&self) -> usize {
        self.pos
    }

    fn next(&mut self) {
        self.pos = (self.pos + 1).min(self.col.n_values());
    }

    fn seek(&mut self, t: &[u8]) {
        // Galloping: a walk's jumps are mostly short.
        let n = self.col.n_values();
        let below = |i: usize| self.col.value_bytes(i as u32) < t;
        let (mut lo, mut step) = (self.pos, 1);
        while lo + step < n && below(lo + step) {
            lo += step;
            step *= 2;
        }
        let mut hi = (lo + step).min(n);
        if lo < n && below(lo) {
            lo += 1;
        }
        while lo < hi {
            let mid = (lo + hi) / 2;
            if below(mid) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        self.pos = lo;
    }
}
