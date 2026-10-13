//! Segments: the immutable unit of storage.
//!
//! A segment holds `n` documents with local docIDs `0..n` and every index
//! structure built over them, as named sections:
//!
//! ```text
//! "STRSEG01" | n_docs: u64 | dir_len: u64 | directory (JSON) | pad
//! section 0 | pad | section 1 | pad | ...
//! ```
//!
//! Sections of one page or more start on a 4 KiB boundary, smaller ones on a
//! 64-byte boundary, relative to the segment start. The storage layer places
//! segments at page-aligned file offsets, so a section mapped from disk can be
//! viewed in place as `&[u64]` (see [`posting_bench::bytes`]).
//!
//! Section names are `component.field.part`, e.g. `text.body.dict`,
//! `col.year`, `kw.category.post`, `vec.emb.ivf`, `graph.links`,
//! `store.data`, `pk`.

pub mod build;
pub mod filter;

pub use filter::{DocFilter, DocSet};

use std::collections::BTreeMap;

use posting_bench::bytes::OwnedBytes;
use serde::{Deserialize, Serialize};

use crate::error::{corrupt, Result};
use crate::value::Value;

/// Segment-local document number.
pub type DocId = u32;

pub const MAGIC: &[u8; 8] = b"STRSEG01";
pub const PAGE: usize = 4096;

/// Builds one field's index sections from its values, in docID order.
pub trait FieldIndexBuilder: Send {
    /// The field's value for document `doc`. Called for docIDs 0, 1, 2, ...
    /// in order; `Value::Null` for documents without a value.
    fn add(&mut self, doc: DocId, value: &Value) -> Result<()>;
    /// Writes this field's sections.
    fn finish(self: Box<Self>, out: &mut SegmentWriter) -> Result<()>;
}

/// A builder that writes nothing (placeholder for unimplemented components).
pub struct NoopBuilder;

impl FieldIndexBuilder for NoopBuilder {
    fn add(&mut self, _doc: DocId, _value: &Value) -> Result<()> {
        Ok(())
    }
    fn finish(self: Box<Self>, _out: &mut SegmentWriter) -> Result<()> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct DirEntry {
    name: String,
    offset: u64,
    len: u64,
}

/// Collects sections and lays them out as a segment image.
pub struct SegmentWriter {
    n_docs: u64,
    sections: Vec<(String, Vec<u8>)>,
}

impl SegmentWriter {
    pub fn new(n_docs: usize) -> Self {
        SegmentWriter { n_docs: n_docs as u64, sections: Vec::new() }
    }

    pub fn n_docs(&self) -> usize {
        self.n_docs as usize
    }

    /// Adds a section. Names must be unique within the segment.
    pub fn put(&mut self, name: impl Into<String>, data: Vec<u8>) {
        let name = name.into();
        assert!(!self.sections.iter().any(|(n, _)| *n == name), "duplicate section {name}");
        self.sections.push((name, data));
    }

    /// The segment image; its start must be placed at a page-aligned offset.
    pub fn finish(self) -> Vec<u8> {
        let (_, _, total) = self.layout();
        let mut out = Vec::with_capacity(total);
        self.write_to(&mut out).expect("writing to a Vec cannot fail");
        out
    }

    /// Streams the image [`finish`](Self::finish) would return to `w`,
    /// releasing each section once written, so a segment never needs memory
    /// for two copies of itself. Returns the image length.
    pub fn write_to(self, w: &mut impl std::io::Write) -> std::io::Result<usize> {
        let (header_len, dir, total) = self.layout();
        let json = serde_json::to_vec(&dir).unwrap();
        let mut header = vec![0u8; header_len];
        header[..8].copy_from_slice(MAGIC);
        header[8..16].copy_from_slice(&self.n_docs.to_le_bytes());
        header[16..24].copy_from_slice(&(json.len() as u64).to_le_bytes());
        header[24..24 + json.len()].copy_from_slice(&json);
        w.write_all(&header)?;
        let mut pos = header_len;
        for ((_, data), e) in self.sections.into_iter().zip(&dir) {
            w.write_all(&vec![0u8; e.offset as usize - pos])?;
            w.write_all(&data)?;
            pos = (e.offset + e.len) as usize;
        }
        debug_assert_eq!(pos, total);
        Ok(total)
    }

    /// Header length, directory and total length of the image.
    fn layout(&self) -> (usize, Vec<DirEntry>, usize) {
        let align = |len: usize| if len >= PAGE { PAGE } else { 64 };
        // Directory offsets depend on the directory's own length; iterate
        // until the layout is stable (twice in practice).
        let mut header_len = 0usize;
        loop {
            let mut off = header_len;
            let mut dir = Vec::with_capacity(self.sections.len());
            for (name, data) in &self.sections {
                off = off.next_multiple_of(align(data.len()));
                dir.push(DirEntry { name: name.clone(), offset: off as u64, len: data.len() as u64 });
                off += data.len();
            }
            let json = serde_json::to_vec(&dir).unwrap();
            let need = (24 + json.len()).next_multiple_of(64);
            if need == header_len {
                let total = dir.last().map_or(header_len, |e| (e.offset + e.len) as usize);
                return (header_len, dir, total);
            }
            header_len = need;
        }
    }
}

/// Whether a section is read at random positions: graph hops, float32
/// re-ranking, dictionary blocks, doc store and key lookups. Everything else
/// (postings, codes, columns, IVF clusters, and document lengths, which
/// ranking reads densely) is read a run at a time and benefits from the
/// kernel's read-ahead.
pub fn random_access(name: &str) -> bool {
    name == "pk"
        || name.starts_with("store.")
        || name.ends_with(".dict")
        || name.ends_with(".f32")
        || name.contains(".hnsw")
        || name.starts_with("graph.")
}

/// A read-only segment: zero-copy views of its sections.
#[derive(Clone, Debug)]
pub struct SegmentFile {
    n_docs: usize,
    bytes: OwnedBytes,
    sections: BTreeMap<String, (usize, usize)>,
}

impl SegmentFile {
    pub fn open(bytes: OwnedBytes) -> Result<Self> {
        if bytes.len() < 24 || &bytes[..8] != MAGIC {
            return Err(corrupt("bad segment magic"));
        }
        let n_docs = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
        let dir_len = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
        if 24 + dir_len > bytes.len() {
            return Err(corrupt("segment directory out of bounds"));
        }
        let dir: Vec<DirEntry> = serde_json::from_slice(&bytes[24..24 + dir_len]).map_err(corrupt)?;
        let mut sections = BTreeMap::new();
        for e in dir {
            let (o, l) = (e.offset as usize, e.len as usize);
            if o + l > bytes.len() {
                return Err(corrupt(format!("section {} out of bounds", e.name)));
            }
            sections.insert(e.name, (o, l));
        }
        Ok(SegmentFile { n_docs, bytes, sections })
    }

    /// Builds a segment in memory (tests, memtable snapshots).
    pub fn from_vec(image: Vec<u8>) -> Result<Self> {
        SegmentFile::open(OwnedBytes::new(image))
    }

    pub fn n_docs(&self) -> usize {
        self.n_docs
    }

    pub fn section(&self, name: &str) -> Option<OwnedBytes> {
        self.sections.get(name).map(|&(o, l)| self.bytes.slice(o..o + l))
    }

    pub fn require(&self, name: &str) -> Result<OwnedBytes> {
        self.section(name).ok_or_else(|| corrupt(format!("missing section {name}")))
    }

    /// Section names with sizes in bytes, for size breakdowns.
    pub fn section_sizes(&self) -> Vec<(String, usize)> {
        self.sections.iter().map(|(n, &(_, l))| (n.clone(), l)).collect()
    }

    /// Byte range of a section within the segment image (for I/O accounting).
    pub fn section_range(&self, name: &str) -> Option<std::ops::Range<usize>> {
        self.sections.get(name).map(|&(o, l)| o..o + l)
    }

    pub fn image(&self) -> &OwnedBytes {
        &self.bytes
    }

    /// Advises the kernel to read the [`random_access`] sections of a mapped
    /// segment without read-ahead (`MADV_RANDOM`), so a cold page fault there
    /// reads one page instead of up to 32. Only whole pages inside a section
    /// are advised.
    pub fn advise_random_sections(&self) {
        let base = self.bytes.as_ptr() as usize;
        // HNSW computes its traversal distances on the SQ8 codes, so in a
        // segment with a graph the codes are read at random positions too.
        let graph = self.sections.keys().any(|n| n.contains(".hnsw"));
        for (name, &(o, l)) in &self.sections {
            let random = random_access(name) || (graph && name.ends_with(".sq8"));
            if !random || l < PAGE {
                continue;
            }
            let start = (base + o).next_multiple_of(PAGE);
            let end = (base + o + l) / PAGE * PAGE;
            if end > start {
                // SAFETY: the range lies inside this segment's mapped bytes;
                // MADV_RANDOM only changes read-ahead, never the contents.
                unsafe { libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_RANDOM) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_roundtrip_aligned() {
        let mut w = SegmentWriter::new(3);
        w.put("a", vec![1, 2, 3]);
        w.put("big", vec![7u8; 5000]);
        w.put("c", Vec::new());
        let seg = SegmentFile::from_vec(w.finish()).unwrap();
        assert_eq!(seg.n_docs(), 3);
        assert_eq!(&*seg.require("a").unwrap(), &[1, 2, 3]);
        assert_eq!(seg.section_range("big").unwrap().start % PAGE, 0);
        assert_eq!(seg.section_range("a").unwrap().start % 64, 0);
        assert!(seg.require("c").unwrap().is_empty());
        assert!(seg.section("missing").is_none());
    }

    #[test]
    fn streamed_image_equals_finished_image() {
        let make = || {
            let mut w = SegmentWriter::new(2);
            w.put("x", vec![9u8; 100]);
            w.put("y", vec![3u8; 9000]);
            w
        };
        let mut streamed = Vec::new();
        let len = make().write_to(&mut streamed).unwrap();
        assert_eq!(len, streamed.len());
        assert_eq!(streamed, make().finish());
    }
}
