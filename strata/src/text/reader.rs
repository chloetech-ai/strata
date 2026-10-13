//! Reading a text field: lookups, posting cursors, boolean retrieval and
//! cardinality estimates. Ranked retrieval is in [`super::bm25`].

use posting_bench::bytes::{OwnedBytes, Slice};
use posting_bench::codecs::loom::Loom;
use posting_bench::codecs::{PostingList, SetOps};

use super::bm25::FilterStrategy;
use super::builder::{section, TextMeta};
use super::codec::NORM_LENGTHS;
use super::cursor::{PostingCursor, END};
use super::dict::{Dict, TermInfo};
use super::postings::{list_sizes, open_loom, ListSizes, PostingsFormat};
use super::query::BoolQuery;
use crate::error::{corrupt, Result};
use crate::segment::{DocFilter, DocId, DocSet, SegmentFile};

/// Checking one filter member against one query term (a forward seek)
/// costs about as much as this many postings of a full evaluation
/// ([`TextFieldReader::eval_cost`]): `eval` drives from the filter when
/// `members * terms * PROBE_COST < eval_cost` (fitted by
/// `strata-text-bench`, see `notes/text.md`).
pub const PROBE_COST: f64 = 2.0;

/// Prefix expansions above this many terms are evaluated as one
/// materialised union inside filter-driven evaluation.
const MATCHER_MAX_EXPANSIONS: usize = 16;

/// Read access to one text field of a segment. Opening parses the small
/// meta and dictionary headers; every array is a view into the segment.
pub struct TextFieldReader {
    pub(crate) meta: TextMeta,
    pub(crate) dict: Dict,
    pub(crate) post: OwnedBytes,
    pub(crate) pos: Option<OwnedBytes>,
    pub(crate) norms: Slice<u8>,
}

/// Bytes of a field's sections by part.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextSizes {
    pub dict: usize,
    /// Postings section: docIDs, frequencies and list metadata (frontiers,
    /// block tables, headers, alignment).
    pub docs: usize,
    pub freqs: usize,
    pub list_meta: usize,
    pub positions: usize,
    pub norms: usize,
}

impl TextFieldReader {
    /// Opens field `field` of a segment: parses the meta and dictionary
    /// headers; every array stays a view into the segment.
    pub fn open(seg: &SegmentFile, field: &str) -> Result<Self> {
        let meta: TextMeta = serde_json::from_slice(&seg.require(&section(field, "meta"))?).map_err(corrupt)?;
        let dict = Dict::open(seg.require(&section(field, "dict"))?)?;
        let post = seg.require(&section(field, "post"))?;
        if !(post.as_ptr() as usize).is_multiple_of(8) {
            return Err(corrupt("text postings section is not 8-byte aligned"));
        }
        let pos = if meta.positions { Some(seg.require(&section(field, "pos"))?) } else { None };
        let norms: Slice<u8> = Slice::view(seg.require(&section(field, "norms"))?).map_err(corrupt)?;
        if norms.len() != meta.n_docs as usize || dict.n_terms() != meta.n_terms || meta.n_docs as usize != seg.n_docs() {
            return Err(corrupt(format!("text field {field}: inconsistent document or term counts")));
        }
        Ok(TextFieldReader { meta, dict, post, pos, norms })
    }

    /// Documents in the segment.
    pub fn n_docs(&self) -> u32 {
        self.meta.n_docs
    }

    /// Sum of document lengths.
    pub fn total_len(&self) -> u64 {
        self.meta.total_len
    }

    /// Length (analysed terms) of `doc`, as stored in its one-byte norm:
    /// exact up to 23, otherwise rounded down to 4 significant bits.
    pub fn doc_len(&self, doc: DocId) -> u32 {
        NORM_LENGTHS[self.norms[doc as usize] as usize]
    }

    /// One-byte length norms of every document.
    pub fn norms(&self) -> &[u8] {
        &self.norms
    }

    /// Encoding of the docIDs of this field's lists.
    pub fn format(&self) -> PostingsFormat {
        self.meta.format
    }

    /// The field's meta section (format, counts, size breakdown).
    pub fn meta(&self) -> &TextMeta {
        &self.meta
    }

    /// Distinct terms in the field.
    pub fn n_terms(&self) -> u32 {
        self.meta.n_terms
    }

    /// Document frequency of an (analysed) term; 0 if absent.
    pub fn doc_freq(&self, term: &str) -> u32 {
        self.dict.get(term).map_or(0, |i| i.df)
    }

    /// The dictionary entry of an (analysed) term.
    pub fn term_info(&self, term: &str) -> Option<TermInfo> {
        self.dict.get(term)
    }

    /// Up to `limit` indexed terms starting with `prefix`, in order.
    pub fn expand_prefix(&self, prefix: &str, limit: usize) -> Vec<String> {
        self.dict.prefix(prefix, limit).into_iter().map(|(t, _)| t).collect()
    }

    /// Every term with its entry, in order.
    pub fn for_each_term(&self, f: impl FnMut(&[u8], TermInfo)) {
        self.dict.for_each(f)
    }

    /// The entries of every term starting with `prefix`.
    fn prefix_infos(&self, prefix: &str) -> Vec<TermInfo> {
        let mut out = Vec::new();
        self.dict.for_each_prefix(prefix, |_, i| {
            out.push(i);
            true
        });
        out
    }

    /// A cursor over an (analysed) term's postings.
    pub fn postings(&self, term: &str) -> Option<PostingCursor<'_>> {
        self.dict.get(term).map(|i| self.cursor(&i))
    }

    /// A cursor over the postings of a dictionary entry.
    pub fn cursor(&self, info: &TermInfo) -> PostingCursor<'_> {
        let (start, end) = (info.post_off as usize, (info.post_off + info.post_len) as usize);
        let positions = self.pos.as_ref().map(|p| &p[info.pos_off as usize..(info.pos_off + info.pos_len) as usize]);
        if self.is_loom(info) {
            PostingCursor::loom(&self.post, start, end, info.df, positions)
        } else {
            PostingCursor::bp(&self.post[start..], info.df, positions)
        }
    }

    fn is_loom(&self, info: &TermInfo) -> bool {
        self.meta.format == PostingsFormat::Loom && info.df >= self.meta.loom_min_df
    }

    fn loom(&self, info: &TermInfo) -> Loom {
        open_loom(&self.post, info.post_off as usize, (info.post_off + info.post_len) as usize).0
    }

    /// Every docID of a list.
    pub fn decode(&self, info: &TermInfo) -> Vec<DocId> {
        let mut out = Vec::with_capacity(info.df as usize);
        if self.is_loom(info) {
            self.loom(info).decode_into(&mut out);
        } else {
            let mut c = self.cursor(info);
            while c.doc() != END {
                out.push(c.doc());
                c.next();
            }
        }
        out
    }

    /// Sizes of the sections by part (postings parts from the build).
    pub fn sizes(&self) -> TextSizes {
        TextSizes {
            dict: self.dict.byte_len(),
            docs: self.meta.doc_bytes as usize,
            freqs: self.meta.freq_bytes as usize,
            list_meta: self.meta.list_meta_bytes as usize,
            positions: self.pos.as_ref().map_or(0, |p| p.len()),
            norms: self.norms.len(),
        }
    }

    /// Sizes of one list's parts, read back from its bytes.
    pub fn list_sizes(&self, info: &TermInfo) -> ListSizes {
        list_sizes(&self.post, info.post_off as usize, (info.post_off + info.post_len) as usize, info.df, self.is_loom(info))
    }

    /// Whether the reader's arrays are views into the segment (no copies).
    pub fn is_zero_copy(&self) -> bool {
        self.dict.is_shared() && matches!(self.norms, Slice::Shared { .. })
    }

    /// Boolean retrieval: the documents matching `q` (and `filter`).
    ///
    /// Conjunctions and disjunctions of terms use Loom's native AND / OR
    /// when every list is a Loom list, else cursors (leapfrog AND; OR into
    /// a bitmap or a merged array by density). A selective filter that can
    /// list its members drives the evaluation instead: each member is
    /// checked with forward seeks (see [`PROBE_COST`]).
    pub fn eval(&self, q: &BoolQuery, filter: Option<&dyn DocFilter>) -> DocSet {
        self.eval_with(q, filter, FilterStrategy::Auto)
    }

    /// [`eval`](Self::eval) with an explicit filter strategy: `Drive`
    /// checks the filter's members one by one (if it can list them),
    /// `Probe` evaluates the query and keeps the documents in the filter.
    pub fn eval_with(&self, q: &BoolQuery, filter: Option<&dyn DocFilter>, strategy: FilterStrategy) -> DocSet {
        let n = self.n_docs();
        let Some(f) = filter else { return self.eval_set(q) };
        let drive = match strategy {
            FilterStrategy::Drive => true,
            FilterStrategy::Probe => false,
            FilterStrategy::Auto => f.cardinality() as f64 * q.terms().len().max(1) as f64 * PROBE_COST < self.eval_cost(q),
        };
        if drive {
            if let Some(members) = f.members() {
                let mut m = self.matcher(q);
                return DocSet::from_sorted(members.filter(|&d| m.matches(d)).collect(), n);
            }
        }
        let set = self.eval_set(q);
        DocSet::from_sorted(set.iter().filter(|&d| f.contains(d)).collect(), n)
    }

    /// Cost of a full evaluation of `q`, in postings: a list costs its
    /// length, a union the sum of its parts, a conjunction (which skips
    /// through its lists) its cheapest positive part times the number of
    /// positive parts, plus its negated parts.
    pub fn eval_cost(&self, q: &BoolQuery) -> f64 {
        match q {
            BoolQuery::Term(t) => self.doc_freq(t) as f64,
            BoolQuery::Phrase(ts) => ts.len() as f64 * ts.iter().map(|t| self.doc_freq(t) as f64).fold(f64::INFINITY, f64::min),
            BoolQuery::Prefix(p) => self.prefix_infos(p).iter().map(|i| i.df as f64).sum(),
            BoolQuery::Or(cs) => cs.iter().map(|c| self.eval_cost(c)).sum(),
            BoolQuery::And(cs) => {
                let (neg, pos): (Vec<&BoolQuery>, Vec<&BoolQuery>) = cs.iter().partition(|c| matches!(c, BoolQuery::Not(_)));
                let min = pos.iter().map(|c| self.eval_cost(c)).fold(f64::INFINITY, f64::min);
                let pos_cost = if pos.is_empty() { self.n_docs() as f64 } else { pos.len() as f64 * min };
                pos_cost + neg.iter().map(|c| self.eval_cost(c)).sum::<f64>()
            }
            BoolQuery::Not(c) => self.eval_cost(c),
        }
    }

    fn eval_set(&self, q: &BoolQuery) -> DocSet {
        let n = self.n_docs();
        match q {
            BoolQuery::Term(t) => match self.dict.get(t) {
                Some(i) => DocSet::from_sorted(self.decode(&i), n),
                None => DocSet::empty(n),
            },
            BoolQuery::Phrase(ts) => DocSet::from_sorted(self.phrase_docs(ts), n),
            BoolQuery::Prefix(p) => {
                let infos = self.prefix_infos(p);
                self.union_terms(&infos)
            }
            BoolQuery::And(cs) => {
                let mut acc: Option<DocSet> = None;
                let terms: Vec<&str> = cs.iter().filter_map(|c| if let BoolQuery::Term(t) = c { Some(t.as_str()) } else { None }).collect();
                if !terms.is_empty() {
                    let infos: Option<Vec<TermInfo>> = terms.iter().map(|t| self.dict.get(t)).collect();
                    let Some(mut infos) = infos else { return DocSet::empty(n) };
                    acc = Some(DocSet::from_sorted(self.intersect_terms(&mut infos), n));
                }
                for c in cs.iter().filter(|c| !matches!(c, BoolQuery::Term(_) | BoolQuery::Not(_))) {
                    if acc.as_ref().is_some_and(|a| a.is_empty()) {
                        break;
                    }
                    let s = self.eval_set(c);
                    acc = Some(match acc {
                        Some(a) => a.intersect(&s),
                        None => s,
                    });
                }
                let mut acc = acc.unwrap_or_else(|| DocSet::all(n));
                for c in cs {
                    if let BoolQuery::Not(inner) = c {
                        if acc.is_empty() {
                            break;
                        }
                        acc = acc.difference(&self.eval_set(inner));
                    }
                }
                acc
            }
            BoolQuery::Or(cs) => {
                let infos: Vec<TermInfo> =
                    cs.iter().filter_map(|c| if let BoolQuery::Term(t) = c { self.dict.get(t) } else { None }).collect();
                let mut acc = self.union_terms(&infos);
                for c in cs.iter().filter(|c| !matches!(c, BoolQuery::Term(_))) {
                    acc = acc.union(&self.eval_set(c));
                }
                acc
            }
            BoolQuery::Not(c) => self.eval_set(c).complement(),
        }
    }

    /// Documents containing every term (rarest first).
    pub fn intersect_terms(&self, infos: &mut [TermInfo]) -> Vec<DocId> {
        if infos.is_empty() {
            return Vec::new();
        }
        infos.sort_by_key(|i| i.df);
        if infos.iter().all(|i| self.is_loom(i)) {
            let lists: Vec<Loom> = infos.iter().map(|i| self.loom(i)).collect();
            let refs: Vec<&Loom> = lists.iter().collect();
            let mut out = Vec::new();
            Loom::intersect(&refs, &mut out);
            return out;
        }
        let mut cs: Vec<PostingCursor> = infos.iter().map(|i| self.cursor(i)).collect();
        leapfrog(&mut cs, |_, _| true)
    }

    /// Documents containing any of the terms.
    pub fn union_terms(&self, infos: &[TermInfo]) -> DocSet {
        let n = self.n_docs();
        let (looms, others): (Vec<&TermInfo>, Vec<&TermInfo>) = infos.iter().partition(|i| self.is_loom(i));
        let total: u64 = infos.iter().map(|i| i.df as u64).sum();
        if looms.is_empty() && total > n as u64 / 16 {
            // Dense: accumulate a bitmap.
            let mut words = vec![0u64; (n as usize).div_ceil(64)];
            for i in &others {
                let mut c = self.cursor(i);
                while c.doc() != END {
                    let d = c.doc() as usize;
                    words[d / 64] |= 1 << (d % 64);
                    c.next();
                }
            }
            return DocSet::from_bits(words, n);
        }
        let mut out = Vec::new();
        if !looms.is_empty() {
            let lists: Vec<Loom> = looms.iter().map(|i| self.loom(i)).collect();
            let refs: Vec<&Loom> = lists.iter().collect();
            Loom::union(&refs, &mut out);
        }
        if !others.is_empty() {
            let mut rest: Vec<DocId> = others.iter().flat_map(|i| self.decode(i)).collect();
            rest.sort_unstable();
            rest.dedup();
            out = if out.is_empty() { rest } else { merge_dedup(&out, &rest) };
        }
        DocSet::from_sorted(out, n)
    }

    /// Documents containing the terms at consecutive positions (all of
    /// them, without positions).
    pub fn phrase_docs(&self, terms: &[String]) -> Vec<DocId> {
        let infos: Option<Vec<TermInfo>> = terms.iter().map(|t| self.dict.get(t)).collect();
        let Some(mut infos) = infos else { return Vec::new() };
        if terms.len() <= 1 || self.pos.is_none() {
            return self.intersect_terms(&mut infos);
        }
        // Cursors rarest first (the leapfrog's driver); `slot[i]` is the
        // cursor of the phrase's term `i`.
        let mut order: Vec<usize> = (0..infos.len()).collect();
        order.sort_by_key(|&i| infos[i].df);
        let mut slot = vec![0; infos.len()];
        for (k, &i) in order.iter().enumerate() {
            slot[i] = k;
        }
        let mut cs: Vec<PostingCursor> = order.iter().map(|&i| self.cursor(&infos[i])).collect();
        let mut bufs = vec![Vec::new(); infos.len()];
        leapfrog(&mut cs, |cs, _| {
            for (i, b) in bufs.iter_mut().enumerate() {
                cs[slot[i]].positions(b);
            }
            phrase_at(&bufs)
        })
    }

    /// Cardinality estimate of `q` for the planner: exact for a single
    /// term; conjunctions assume independence, disjunctions combine as
    /// independent events, phrases are estimated as their conjunction.
    pub fn estimate(&self, q: &BoolQuery) -> f64 {
        let n = self.n_docs() as f64;
        if n == 0.0 {
            return 0.0;
        }
        match q {
            BoolQuery::Term(t) => self.doc_freq(t) as f64,
            BoolQuery::Phrase(ts) => n * ts.iter().map(|t| self.doc_freq(t) as f64 / n).product::<f64>(),
            BoolQuery::Prefix(p) => {
                n * (1.0 - self.prefix_infos(p).iter().map(|i| 1.0 - i.df as f64 / n).product::<f64>())
            }
            BoolQuery::And(cs) => {
                n * cs
                    .iter()
                    .map(|c| match c {
                        BoolQuery::Not(inner) => 1.0 - self.estimate(inner) / n,
                        c => self.estimate(c) / n,
                    })
                    .product::<f64>()
            }
            BoolQuery::Or(cs) => n * (1.0 - cs.iter().map(|c| 1.0 - self.estimate(c) / n).product::<f64>()),
            BoolQuery::Not(c) => n - self.estimate(c),
        }
    }

    /// A per-document evaluator of `q` for increasing docIDs.
    fn matcher(&self, q: &BoolQuery) -> Matcher<'_> {
        match q {
            BoolQuery::Term(t) => self.postings(t).map_or(Matcher::Never, Matcher::Term),
            BoolQuery::Phrase(ts) if ts.len() == 1 => self.matcher(&BoolQuery::Term(ts[0].clone())),
            BoolQuery::Phrase(ts) => {
                let cs: Option<Vec<PostingCursor>> = ts.iter().map(|t| self.postings(t)).collect();
                match cs {
                    Some(cs) => {
                        let n = cs.len();
                        Matcher::Phrase(cs, vec![Vec::new(); n], self.pos.is_some())
                    }
                    None => Matcher::Never,
                }
            }
            BoolQuery::Prefix(p) => {
                let infos = self.prefix_infos(p);
                if infos.len() > MATCHER_MAX_EXPANSIONS {
                    Matcher::Set(self.union_terms(&infos))
                } else {
                    Matcher::Any(infos.iter().map(|i| Matcher::Term(self.cursor(i))).collect())
                }
            }
            BoolQuery::And(cs) => Matcher::All(cs.iter().map(|c| self.matcher(c)).collect()),
            BoolQuery::Or(cs) => Matcher::Any(cs.iter().map(|c| self.matcher(c)).collect()),
            BoolQuery::Not(c) => Matcher::Not(Box::new(self.matcher(c))),
        }
    }
}

/// Leapfrog intersection of cursors (the first drives); `accept(cursors,
/// doc)` makes the final decision on a common document.
fn leapfrog(cs: &mut [PostingCursor], mut accept: impl FnMut(&mut [PostingCursor], DocId) -> bool) -> Vec<DocId> {
    let mut out = Vec::new();
    if cs.is_empty() {
        return out;
    }
    let mut d = cs[0].doc();
    'outer: while d != END {
        for i in 1..cs.len() {
            let x = cs[i].seek(d);
            if x != d {
                d = cs[0].seek(x);
                continue 'outer;
            }
        }
        if accept(cs, d) {
            out.push(d);
        }
        d = cs[0].next();
    }
    out
}

/// Is there a position `p` with `p + i` in `positions[i]` for every `i`?
fn phrase_at(positions: &[Vec<u32>]) -> bool {
    let mut ptr = vec![0usize; positions.len()];
    'next: for &p in &positions[0] {
        for i in 1..positions.len() {
            let want = p + i as u32;
            let ps = &positions[i];
            while ptr[i] < ps.len() && ps[ptr[i]] < want {
                ptr[i] += 1;
            }
            if ptr[i] == ps.len() {
                return false;
            }
            if ps[ptr[i]] != want {
                continue 'next;
            }
        }
        return true;
    }
    false
}

fn merge_dedup(a: &[DocId], b: &[DocId]) -> Vec<DocId> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        out.push(x.min(y));
        i += (x <= y) as usize;
        j += (y <= x) as usize;
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// Evaluates a query one document at a time, for increasing docIDs.
enum Matcher<'a> {
    Term(PostingCursor<'a>),
    /// Cursors in phrase order, position buffers, whether positions exist.
    Phrase(Vec<PostingCursor<'a>>, Vec<Vec<u32>>, bool),
    Set(DocSet),
    All(Vec<Matcher<'a>>),
    Any(Vec<Matcher<'a>>),
    Not(Box<Matcher<'a>>),
    Never,
}

impl Matcher<'_> {
    fn matches(&mut self, d: DocId) -> bool {
        match self {
            Matcher::Term(c) => c.seek(d) == d,
            Matcher::Phrase(cs, bufs, has_pos) => {
                if !cs.iter_mut().all(|c| c.seek(d) == d) {
                    return false;
                }
                if !*has_pos {
                    return true;
                }
                for (c, b) in cs.iter_mut().zip(bufs.iter_mut()) {
                    c.positions(b);
                }
                phrase_at(bufs)
            }
            Matcher::Set(s) => s.contains(d),
            Matcher::All(ms) => ms.iter_mut().all(|m| m.matches(d)),
            Matcher::Any(ms) => ms.iter_mut().any(|m| m.matches(d)),
            Matcher::Not(m) => !m.matches(d),
            Matcher::Never => false,
        }
    }
}
