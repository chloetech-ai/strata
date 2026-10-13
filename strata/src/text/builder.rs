//! Building a text field's sections from its values in docID order.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::analyzer::{for_each_word, term_of};
use super::codec::encode_norm;
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
    norms: Vec<u8>,
    total_len: u64,
    /// The current document's (term, position) pairs.
    scratch: Vec<(u32, u32)>,
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
            norms: Vec::new(),
            total_len: 0,
            scratch: Vec::new(),
        }
    }

    /// Adds the next document's text.
    pub fn add_text(&mut self, text: &str) {
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

    /// Writes the sections.
    pub fn write(self, out: &mut SegmentWriter) -> Result<()> {
        self.write_as(out, self.format, self.loom_min_df)
    }

    /// Writes the sections with another postings format (experiments write
    /// one analysed field in several formats).
    pub fn write_as(&self, out: &mut SegmentWriter, format: PostingsFormat, loom_min_df: u32) -> Result<()> {
        let n_docs = self.norms.len();
        let mut order: Vec<u32> = (0..self.terms.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| self.terms[a as usize].as_bytes().cmp(self.terms[b as usize].as_bytes()));
        let mut post: Vec<u8> = Vec::new();
        let mut pos: Vec<u8> = Vec::new();
        let mut sizes = ListSizes::default();
        let mut n_postings = 0u64;
        // (term, df, postings start, positions length)
        let mut entries: Vec<(u32, u32, usize, usize)> = Vec::with_capacity(order.len());
        for &t in &order {
            let p = &self.postings[t as usize];
            let df = p.docs.len() as u32;
            let loom = format == PostingsFormat::Loom && df >= loom_min_df;
            if loom {
                // The padding belongs to the previous list's bytes.
                post.resize(post.len().next_multiple_of(8), 0);
            }
            let start = post.len();
            sizes += if loom { write_loom(&mut post, &p.docs, &p.freqs, &self.norms) } else { write_bp128(&mut post, &p.docs, &p.freqs, &self.norms) };
            let pos_start = pos.len();
            if self.positions {
                write_positions(&mut pos, &p.freqs, &p.positions);
            }
            entries.push((t, df, start, pos.len() - pos_start));
            n_postings += df as u64;
        }
        sizes.meta += post.len() - (sizes.docs + sizes.freqs + sizes.meta);
        let mut dict = DictBuilder::new();
        for (i, &(t, df, start, pos_len)) in entries.iter().enumerate() {
            let end = entries.get(i + 1).map_or(post.len(), |e| e.2);
            dict.add(self.terms[t as usize].as_bytes(), df, (end - start) as u64, pos_len as u64);
        }
        post.extend([0u8; SECTION_PAD]);
        pos.extend([0u8; SECTION_PAD]);
        let meta = TextMeta {
            format,
            loom_min_df,
            n_docs: n_docs as u32,
            total_len: self.total_len,
            n_terms: order.len() as u32,
            n_postings,
            positions: self.positions,
            doc_bytes: sizes.docs as u64,
            freq_bytes: sizes.freqs as u64,
            list_meta_bytes: sizes.meta as u64,
        };
        let f = &self.field;
        out.put(section(f, "meta"), serde_json::to_vec(&meta).map_err(|e| Error::Schema(e.to_string()))?);
        out.put(section(f, "dict"), dict.finish());
        out.put(section(f, "post"), post);
        if self.positions {
            out.put(section(f, "pos"), pos);
        }
        out.put(section(f, "norms"), self.norms.clone());
        Ok(())
    }
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

    fn finish(self: Box<Self>, out: &mut SegmentWriter) -> Result<()> {
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
