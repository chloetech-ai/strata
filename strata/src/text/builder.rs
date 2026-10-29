//! Building a text field's sections from its values in docID order.

use std::cmp::Reverse;
use std::collections::binary_heap::PeekMut;
use std::collections::{BinaryHeap, HashMap};

use serde::{Deserialize, Serialize};

use super::analyzer::{for_each_word, term_of};
use super::codec::{encode_norm, get_varint, put_varint};
use super::cursor::write_positions;
use super::dict::DictBuilder;
use super::postings::{write_bp128, write_loom, ListSizes, PostingsFormat, LOOM_MIN_DF};
use crate::error::{Error, Result};
use crate::segment::{DocId, FieldIndexBuilder, SegmentWriter};
use crate::value::Value;

/// Zero bytes after the postings and positions data, so that 8-byte reads
/// of packed values near the end stay inside the section.
pub const SECTION_PAD: usize = 16;

/// Section names of a text field.
pub fn section(field: &str, part: &str) -> String {
    format!("text.{field}.{part}")
}

/// The `text.<field>.meta` section (JSON).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextMeta {
    pub format: PostingsFormat,
    /// Lists with at least this many postings are Loom lists (Loom format).
    pub loom_min_df: u32,
    pub n_docs: u32,
    /// Sum of document lengths (analysed terms).
    pub total_len: u64,
    pub n_terms: u32,
    pub n_postings: u64,
    /// Whether positions are stored (`n_positions` = `total_len` then).
    pub positions: bool,
    /// Bytes of the postings section by part (docIDs, frequencies, list
    /// metadata), for size breakdowns.
    pub doc_bytes: u64,
    pub freq_bytes: u64,
    pub list_meta_bytes: u64,
}

#[derive(Default)]
struct TermPostings {
    docs: Vec<DocId>,
    freqs: Vec<u32>,
    positions: Vec<u32>,
}

/// A sealed builder's posting lists packed as varints (docID gaps,
/// frequencies, and position gaps within each document), as a build of
/// many batches holds its earlier batches' runs: a third or less of their
/// memory unpacked.
struct Packed {
    bytes: Vec<u8>,
    /// Per term: where its list starts in `bytes`, and its df.
    lists: Vec<(usize, u32)>,
}

/// Builds `text.<field>.{meta,dict,post,pos,norms}`.
pub struct TextFieldBuilder {
    field: String,
    positions: bool,
    format: PostingsFormat,
    loom_min_df: u32,
    /// Analysis cache: folded word -> term id (`None`: stopword).
    words: HashMap<String, Option<u32>>,
    term_ids: HashMap<String, u32>,
    terms: Vec<String>,
    postings: Vec<TermPostings>,
    /// The posting lists once [packed](Self::pack) (`postings` is empty then).
    packed: Option<Packed>,
    norms: Vec<u8>,
    total_len: u64,
    /// The current document's (term, position) pairs.
    scratch: Vec<(u32, u32)>,
    sealed: bool,
}

impl TextFieldBuilder {
    /// A builder for field `field`, storing positions if asked, with lists
    /// of at least `loom_min_df` postings stored as Loom in the Loom format.
    pub fn new(field: &str, positions: bool, format: PostingsFormat, loom_min_df: u32) -> Self {
        TextFieldBuilder {
            field: field.to_string(),
            positions,
            format,
            loom_min_df,
            words: HashMap::new(),
            term_ids: HashMap::new(),
            terms: Vec::new(),
            postings: Vec::new(),
            packed: None,
            norms: Vec::new(),
            total_len: 0,
            scratch: Vec::new(),
            sealed: false,
        }
    }

    /// Frees the lookups only adding documents needs (each distinct word
    /// and term), before writing; no document can be added afterwards.
    pub fn seal(&mut self) {
        self.words = HashMap::new();
        self.term_ids = HashMap::new();
        self.scratch = Vec::new();
        self.sealed = true;
    }

    /// Packs a sealed builder's posting lists ([`Packed`]): for a build
    /// that holds this run while it adds more.
    pub fn pack(&mut self) {
        assert!(self.sealed, "a text builder packed before it is sealed");
        if self.packed.is_some() {
            return;
        }
        let mut bytes = Vec::new();
        let mut lists = Vec::with_capacity(self.postings.len());
        for p in std::mem::take(&mut self.postings) {
            lists.push((bytes.len(), p.docs.len() as u32));
            let (mut prev, mut at) = (0, 0);
            for (&d, &f) in p.docs.iter().zip(&p.freqs) {
                put_varint(&mut bytes, (d - prev) as u64);
                put_varint(&mut bytes, f as u64);
                prev = d;
                if self.positions {
                    let mut last = 0;
                    for &x in &p.positions[at..at + f as usize] {
                        put_varint(&mut bytes, (x - last) as u64);
                        last = x;
                    }
                    at += f as usize;
                }
            }
        }
        bytes.shrink_to_fit();
        self.terms.shrink_to_fit();
        self.packed = Some(Packed { bytes, lists });
    }

    /// Term `t`'s document frequency.
    fn df(&self, t: u32) -> u32 {
        match &self.packed {
            Some(p) => p.lists[t as usize].1,
            None => self.postings[t as usize].docs.len() as u32,
        }
    }

    /// Appends term `t`'s postings, its docIDs shifted by `base`.
    fn append_list(&self, t: u32, base: DocId, docs: &mut Vec<DocId>, freqs: &mut Vec<u32>, ps: &mut Vec<u32>) {
        let Some(p) = &self.packed else {
            let q = &self.postings[t as usize];
            docs.extend(q.docs.iter().map(|&x| x + base));
            freqs.extend_from_slice(&q.freqs);
            ps.extend_from_slice(&q.positions);
            return;
        };
        let (mut at, n) = p.lists[t as usize];
        let mut d = 0;
        for _ in 0..n {
            d += get_varint(&p.bytes, &mut at) as DocId;
            let f = get_varint(&p.bytes, &mut at) as u32;
            docs.push(d + base);
            freqs.push(f);
            if self.positions {
                let mut x = 0;
                for _ in 0..f {
                    x += get_varint(&p.bytes, &mut at) as u32;
                    ps.push(x);
                }
            }
        }
    }

    /// Adds the next document's text.
    pub fn add_text(&mut self, text: &str) {
        assert!(!self.sealed, "a document added to a sealed text builder");
        let doc = self.norms.len() as DocId;
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        let mut pos = 0u32;
        for_each_word(text, |w| {
            let id = match self.words.get(w) {
                Some(&id) => id,
                None => {
                    let id = term_of(w).map(|t| match self.term_ids.get(t.as_ref()) {
                        Some(&id) => id,
                        None => {
                            let id = self.terms.len() as u32;
                            self.term_ids.insert(t.to_string(), id);
                            self.terms.push(t.into_owned());
                            self.postings.push(TermPostings::default());
                            id
                        }
                    });
                    self.words.insert(w.to_string(), id);
                    id
                }
            };
            if let Some(id) = id {
                scratch.push((id, pos));
                pos += 1;
            }
        });
        // Group by term; positions stay increasing within a term.
        scratch.sort_unstable();
        for run in scratch.chunk_by(|a, b| a.0 == b.0) {
            let p = &mut self.postings[run[0].0 as usize];
            p.docs.push(doc);
            p.freqs.push(run.len() as u32);
            if self.positions {
                p.positions.extend(run.iter().map(|&(_, x)| x));
            }
        }
        self.norms.push(encode_norm(pos));
        self.total_len += pos as u64;
        self.scratch = scratch;
    }

    /// Adds the next document as distinct terms with their frequencies (a
    /// sparse field's quantised weights, see [`super::sparse`]); its length
    /// is its number of terms.
    pub fn add_weighted(&mut self, terms: &[(String, u32)]) {
        assert!(!self.sealed, "a document added to a sealed text builder");
        let doc = self.norms.len() as DocId;
        for (t, f) in terms {
            let id = match self.term_ids.get(t.as_str()) {
                Some(&id) => id,
                None => {
                    let id = self.terms.len() as u32;
                    self.term_ids.insert(t.clone(), id);
                    self.terms.push(t.clone());
                    self.postings.push(TermPostings::default());
                    id
                }
            };
            let p = &mut self.postings[id as usize];
            p.docs.push(doc);
            p.freqs.push(*f);
        }
        let len = terms.len() as u32;
        self.norms.push(encode_norm(len));
        self.total_len += len as u64;
    }

    /// Writes the sections.
    pub fn write(self, out: &mut SegmentWriter) -> Result<()> {
        self.write_as(out, self.format, self.loom_min_df)
    }

    /// Writes the sections with another postings format (experiments write
    /// one analysed field in several formats).
    pub fn write_as(&self, out: &mut SegmentWriter, format: PostingsFormat, loom_min_df: u32) -> Result<()> {
        write_runs(&[self], format, loom_min_df, 1, out)
    }

    /// Writes the field from `runs`, builders for it over consecutive runs
    /// of documents (analysed on several threads), encoding posting lists
    /// on `threads` threads: the same bytes as one builder over every
    /// document would write.
    pub fn write_joined(runs: Vec<TextFieldBuilder>, threads: usize, out: &mut SegmentWriter) -> Result<()> {
        let first = runs.first().ok_or_else(|| Error::Schema("a text field built from no runs".into()))?;
        write_runs(&runs.iter().collect::<Vec<_>>(), first.format, first.loom_min_df, threads, out)
    }
}

/// One thread's share of a field's posting lists, encoded.
#[derive(Default)]
struct Encoded {
    post: Vec<u8>,
    pos: Vec<u8>,
    /// Per list: its start in `post`, its positions' length and its df.
    lists: Vec<(usize, usize, u32)>,
    sizes: ListSizes,
}

/// Writes a text field's sections from builders over consecutive runs of
/// its documents. Lists are encoded in term order on `threads` threads,
/// each taking a range of terms; a range after the first starts at a list
/// that begins 8-byte aligned (any list in BP128, a Loom list in Loom), so
/// placing it after the previous range gives the bytes one thread writes.
fn write_runs(runs: &[&TextFieldBuilder], format: PostingsFormat, loom_min_df: u32, threads: usize, out: &mut SegmentWriter) -> Result<()> {
    let b0 = runs[0];
    let mut base = Vec::with_capacity(runs.len());
    let mut n_docs = 0usize;
    for r in runs {
        base.push(n_docs as DocId);
        n_docs += r.norms.len();
    }
    let joined;
    let norms: &[u8] = if runs.len() == 1 {
        &b0.norms
    } else {
        joined = runs.iter().flat_map(|r| r.norms.iter().copied()).collect::<Vec<u8>>();
        &joined
    };
    // Each run's terms in byte order, merged through a heap of the runs'
    // next terms (a merge builds from many runs): every term of the field,
    // and the runs holding it, in run order, with their term ids.
    let orders: Vec<Vec<u32>> = on_threads(runs.len(), |r| {
        let terms = &runs[r].terms;
        let mut o: Vec<u32> = (0..terms.len() as u32).collect();
        o.sort_unstable_by(|&a, &b| terms[a as usize].cmp(&terms[b as usize]));
        o
    });
    let mut terms: Vec<&str> = Vec::new();
    let mut holders: Vec<(usize, u32)> = Vec::new();
    let mut first: Vec<usize> = Vec::new();
    let mut df: Vec<u32> = Vec::new();
    let mut heads = vec![0usize; runs.len()];
    let head = |r: usize, at: usize| orders[r].get(at).map(|&t| runs[r].terms[t as usize].as_str());
    let mut heap: BinaryHeap<Reverse<(&str, usize)>> = (0..runs.len()).filter_map(|r| Some(Reverse((head(r, 0)?, r)))).collect();
    while let Some(&Reverse((min, _))) = heap.peek() {
        first.push(holders.len());
        terms.push(min);
        let mut n = 0;
        while let Some(Reverse((_, r))) = heap.peek_mut().filter(|h| h.0 .0 == min).map(PeekMut::pop) {
            let t = orders[r][heads[r]];
            holders.push((r, t));
            n += runs[r].df(t);
            heads[r] += 1;
            if let Some(next) = head(r, heads[r]) {
                heap.push(Reverse((next, r)));
            }
        }
        df.push(n);
    }
    first.push(holders.len());
    // Ranges of about equal postings.
    let total: u64 = df.iter().map(|&d| d as u64).sum();
    let mut cuts = vec![0usize];
    let mut acc = 0u64;
    for (i, &d) in df.iter().enumerate() {
        let aligned = format != PostingsFormat::Loom || d >= loom_min_df;
        if i > 0 && aligned && cuts.len() < threads && acc * threads as u64 >= total * cuts.len() as u64 {
            cuts.push(i);
        }
        acc += d as u64;
    }
    cuts.push(terms.len());
    let encoded: Vec<Encoded> = on_threads(cuts.len() - 1, |k| {
        let mut e = Encoded::default();
        let (mut docs, mut freqs, mut ps) = (Vec::new(), Vec::new(), Vec::new());
        for i in cuts[k]..cuts[k + 1] {
            let hs = &holders[first[i]..first[i + 1]];
            let (d, f, p): (&[DocId], &[u32], &[u32]) = match hs {
                [(r, t)] if runs[*r].packed.is_none() && base[*r] == 0 => {
                    let q = &runs[*r].postings[*t as usize];
                    (&q.docs, &q.freqs, &q.positions)
                }
                [(r, t)] if runs[*r].packed.is_none() => {
                    let q = &runs[*r].postings[*t as usize];
                    docs.clear();
                    docs.extend(q.docs.iter().map(|&x| x + base[*r]));
                    (&docs, &q.freqs, &q.positions)
                }
                _ => {
                    docs.clear();
                    freqs.clear();
                    ps.clear();
                    for &(r, t) in hs {
                        runs[r].append_list(t, base[r], &mut docs, &mut freqs, &mut ps);
                    }
                    (&docs, &freqs, &ps)
                }
            };
            let loom = format == PostingsFormat::Loom && df[i] >= loom_min_df;
            if loom {
                // The padding belongs to the previous list's bytes.
                e.post.resize(e.post.len().next_multiple_of(8), 0);
            }
            let start = e.post.len();
            e.sizes += if loom { write_loom(&mut e.post, d, f, norms) } else { write_bp128(&mut e.post, d, f, norms) };
            let pos_start = e.pos.len();
            if b0.positions {
                write_positions(&mut e.pos, f, p);
            }
            e.lists.push((start, e.pos.len() - pos_start, df[i]));
        }
        e
    });
    // The first range's buffers become the sections.
    let mut encoded = encoded.into_iter();
    let Encoded { mut post, mut pos, lists, mut sizes } = encoded.next().unwrap_or_default();
    post.reserve(encoded.as_slice().iter().map(|e| e.post.len() + 8).sum::<usize>() + SECTION_PAD);
    pos.reserve(encoded.as_slice().iter().map(|e| e.pos.len()).sum::<usize>() + SECTION_PAD);
    // (df, postings start, positions length)
    let mut entries: Vec<(u32, usize, usize)> = Vec::with_capacity(terms.len());
    entries.extend(lists.iter().map(|&(start, pos_len, df)| (df, start, pos_len)));
    for e in encoded {
        if e.lists.first().is_some_and(|l| format == PostingsFormat::Loom && l.2 >= loom_min_df) {
            post.resize(post.len().next_multiple_of(8), 0);
        }
        let off = post.len();
        post.extend_from_slice(&e.post);
        pos.extend_from_slice(&e.pos);
        entries.extend(e.lists.iter().map(|&(start, pos_len, df)| (df, off + start, pos_len)));
        sizes += e.sizes;
    }
    let n_postings = total;
    sizes.meta += post.len() - (sizes.docs + sizes.freqs + sizes.meta);
    let mut dict = DictBuilder::new();
    for (i, &(df, start, pos_len)) in entries.iter().enumerate() {
        let end = entries.get(i + 1).map_or(post.len(), |e| e.1);
        dict.add(terms[i].as_bytes(), df, (end - start) as u64, pos_len as u64);
    }
    post.extend([0u8; SECTION_PAD]);
    pos.extend([0u8; SECTION_PAD]);
    let meta = TextMeta {
        format,
        loom_min_df,
        n_docs: n_docs as u32,
        total_len: runs.iter().map(|r| r.total_len).sum(),
        n_terms: terms.len() as u32,
        n_postings,
        positions: b0.positions,
        doc_bytes: sizes.docs as u64,
        freq_bytes: sizes.freqs as u64,
        list_meta_bytes: sizes.meta as u64,
    };
    let f = &b0.field;
    out.put(section(f, "meta"), serde_json::to_vec(&meta).map_err(|e| Error::Schema(e.to_string()))?);
    out.put(section(f, "dict"), dict.finish());
    out.put(section(f, "post"), post);
    if b0.positions {
        out.put(section(f, "pos"), pos);
    }
    out.put(section(f, "norms"), norms.to_vec());
    Ok(())
}

/// `f(0)`, ..., `f(n - 1)`, each on a thread of its own when `n > 1`.
fn on_threads<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    if n <= 1 {
        return (0..n).map(&f).collect();
    }
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..n).map(|k| s.spawn({
            let f = &f;
            move || f(k)
        })).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
    })
}

impl FieldIndexBuilder for TextFieldBuilder {
    fn add(&mut self, doc: DocId, value: &Value) -> Result<()> {
        if doc as usize != self.norms.len() {
            return Err(Error::Schema(format!("text field {}: document {doc} added out of order", self.field)));
        }
        match value {
            Value::Str(s) => self.add_text(s),
            Value::Null => self.add_text(""),
            v => return Err(Error::Schema(format!("text field {} got {v:?}", self.field))),
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>, out: &mut SegmentWriter) -> Result<()> {
        self.seal();
        self.write(out)
    }
}

/// Postings format and Loom threshold from build options `text.postings`
/// (`loom` | `bp128`) and `text.loom_min_df`.
pub fn options(opts: &crate::segment::build::BuildOptions) -> Result<(PostingsFormat, u32)> {
    let f = opts.get_or("text.postings", "loom");
    let format = PostingsFormat::parse(f).ok_or_else(|| Error::Schema(format!("text.postings: unknown format {f:?}")))?;
    let min_df = match opts.get("text.loom_min_df") {
        Some(s) => s.parse().map_err(|_| Error::Schema(format!("text.loom_min_df: bad value {s:?}")))?,
        None => LOOM_MIN_DF,
    };
    Ok((format, min_df))
}
