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
//! The directory names each section with its offset, length and crc32;
//! segments written before sections had a crc32 lack the key, and older
//! binaries ignore it. Sections of one page or more start on a 4 KiB
//! boundary, smaller ones on a 64-byte boundary, relative to the segment
//! start. The storage layer places
//! segments at page-aligned file offsets, so a section mapped from disk can be
//! viewed in place as `&[u64]` (see [`crate::postings::bytes`]).
//!
//! Section names are `component.field.part`, e.g. `text.body.dict`,
//! `col.year`, `kw.category.post`, `vec.emb.ivf`, `graph.links`,
//! `store.data`, `pk`.

pub mod build;
pub mod filter;

pub use filter::{DocFilter, DocSet};

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::postings::bytes::OwnedBytes;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    crc32: Option<u32>,
}

/// Collects sections and lays them out as a segment image.
pub struct SegmentWriter {
    n_docs: u64,
    /// Name, bytes and crc32 of each section.
    sections: Vec<(String, Vec<u8>, u32)>,
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
        let crc = crc32fast::hash(&data);
        self.push(name.into(), data, crc);
    }

    fn push(&mut self, name: String, data: Vec<u8>, crc: u32) {
        assert!(!self.sections.iter().any(|(n, ..)| *n == name), "duplicate section {name}");
        self.sections.push((name, data, crc));
    }

    /// Adds `other`'s sections after this writer's, in their order.
    pub fn append(&mut self, other: SegmentWriter) {
        for (name, data, crc) in other.sections {
            self.push(name, data, crc);
        }
    }

    /// The length of the image [`finish`](Self::finish) would return.
    pub fn len(&self) -> usize {
        self.layout().2
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
        for ((_, data, _), e) in self.sections.into_iter().zip(&dir) {
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
            for (name, data, crc) in &self.sections {
                off = off.next_multiple_of(align(data.len()));
                dir.push(DirEntry { name: name.clone(), offset: off as u64, len: data.len() as u64, crc32: Some(*crc) });
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
    sections: BTreeMap<String, Section>,
    /// With [`checked_on_read`](Self::checked_on_read): what is known of
    /// each section's checksum, by [`Section::slot`].
    checked: Option<Arc<[AtomicU8]>>,
    random: Random,
}

/// Which sections of a mapped segment the kernel was advised to read
/// without read-ahead (`MADV_RANDOM`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Random {
    None,
    /// [`SegmentFile::advise_random_sections`]
    Sections,
    /// [`SegmentFile::mapped_random`]
    All,
}

#[derive(Clone, Copy, Debug)]
struct Section {
    offset: usize,
    len: usize,
    crc: Option<u32>,
    slot: usize,
}

/// States of a section's checksum in [`SegmentFile::checked`].
const UNCHECKED: u8 = 0;
const GOOD: u8 = 1;
const BAD: u8 = 2;

impl SegmentFile {
    /// Opens an image whose sections are trusted as far as their structure
    /// checks go (see [`checked_on_read`](Self::checked_on_read)).
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
        for (slot, e) in dir.into_iter().enumerate() {
            let (offset, len) = (e.offset as usize, e.len as usize);
            if offset.checked_add(len).is_none_or(|end| end > bytes.len()) {
                return Err(corrupt(format!("section {} out of bounds", e.name)));
            }
            sections.insert(e.name, Section { offset, len, crc: e.crc32, slot });
        }
        Ok(SegmentFile { n_docs, bytes, sections, checked: None, random: Random::None })
    }

    /// Builds a segment in memory (tests, memtable snapshots).
    pub fn from_vec(image: Vec<u8>) -> Result<Self> {
        SegmentFile::open(OwnedBytes::new(image))
    }

    /// Checks each section against its crc32 the first time it is read
    /// (sections of segments written before they had one are not checked),
    /// which reads the whole section then. Clones share what was checked.
    pub fn checked_on_read(mut self) -> Self {
        let slots = self.sections.values().map(|s| s.slot + 1).max().unwrap_or(0);
        self.checked = Some((0..slots).map(|_| AtomicU8::new(UNCHECKED)).collect());
        self
    }

    pub fn n_docs(&self) -> usize {
        self.n_docs
    }

    /// The bytes of section `name`, if the segment has one; an error if it
    /// fails its checksum ([`checked_on_read`](Self::checked_on_read)).
    pub fn section(&self, name: &str) -> Result<Option<OwnedBytes>> {
        let Some(s) = self.sections.get(name) else { return Ok(None) };
        if let (Some(checked), Some(_)) = (&self.checked, s.crc) {
            let state = &checked[s.slot];
            let known = match state.load(Ordering::Acquire) {
                UNCHECKED => {
                    let now = if self.matches(name, s) { GOOD } else { BAD };
                    state.store(now, Ordering::Release);
                    now
                }
                known => known,
            };
            if known == BAD {
                return Err(corrupt(format!("section {name} fails its checksum")));
            }
        }
        Ok(Some(self.bytes.slice(s.offset..s.offset + s.len)))
    }

    pub fn require(&self, name: &str) -> Result<OwnedBytes> {
        self.section(name)?.ok_or_else(|| corrupt(format!("missing section {name}")))
    }

    /// Whether the segment has section `name` (without reading it).
    pub fn has(&self, name: &str) -> bool {
        self.sections.contains_key(name)
    }

    /// Checks every section that has a crc32 against it, reading the whole
    /// segment; the error names the first section that fails.
    pub fn verify_sections(&self) -> Result<()> {
        for (name, s) in &self.sections {
            if !self.matches(name, s) {
                return Err(corrupt(format!("section {name} fails its checksum")));
            }
        }
        Ok(())
    }

    fn matches(&self, name: &str, s: &Section) -> bool {
        let Some(crc) = s.crc else { return true };
        let bytes = &self.bytes[s.offset..s.offset + s.len];
        // Without read-ahead the kernel would read the section from disk a
        // page per fault; let it read ahead while the section is checked.
        let random = match self.random {
            Random::None => false,
            Random::Sections => self.read_at_random(name),
            Random::All => true,
        };
        if random {
            advise(bytes, libc::MADV_NORMAL);
        }
        let ok = crc32fast::hash(bytes) == crc;
        if random {
            advise(bytes, libc::MADV_RANDOM);
        }
        ok
    }

    /// Section names with sizes in bytes, for size breakdowns.
    pub fn section_sizes(&self) -> Vec<(String, usize)> {
        self.sections.iter().map(|(n, s)| (n.clone(), s.len)).collect()
    }

    /// Byte range of a section within the segment image (for I/O accounting).
    pub fn section_range(&self, name: &str) -> Option<std::ops::Range<usize>> {
        self.sections.get(name).map(|s| s.offset..s.offset + s.len)
    }

    pub fn image(&self) -> &OwnedBytes {
        &self.bytes
    }

    /// Advises the kernel to read the [`random_access`] sections of a mapped
    /// segment without read-ahead (`MADV_RANDOM`), so a cold page fault there
    /// reads one page instead of up to 32. Only whole pages inside a section
    /// are advised.
    pub fn advise_random_sections(&mut self) {
        if self.random == Random::None {
            self.random = Random::Sections;
        }
        for (name, s) in &self.sections {
            if self.read_at_random(name) {
                advise(&self.bytes[s.offset..s.offset + s.len], libc::MADV_RANDOM);
            }
        }
    }

    /// Says that the whole mapping was advised `MADV_RANDOM`, so that a
    /// check of a section ([`checked_on_read`](Self::checked_on_read)) lets
    /// the kernel read ahead meanwhile.
    pub fn mapped_random(mut self) -> Self {
        self.random = Random::All;
        self
    }

    /// Whether [`advise_random_sections`](Self::advise_random_sections)
    /// advises section `name`.
    fn read_at_random(&self, name: &str) -> bool {
        // HNSW computes its traversal distances on the SQ8 codes, so in a
        // segment with a graph the codes are read at random positions too.
        random_access(name) || (name.ends_with(".sq8") && self.sections.keys().any(|n| n.contains(".hnsw")))
    }
}

/// Advises the whole pages inside `bytes`, part of a mapping.
fn advise(bytes: &[u8], advice: libc::c_int) {
    let start = (bytes.as_ptr() as usize).next_multiple_of(PAGE);
    let end = (bytes.as_ptr() as usize + bytes.len()) / PAGE * PAGE;
    if end > start {
        // SAFETY: the range lies inside `bytes`; advice only changes
        // read-ahead, never the contents.
        unsafe { libc::madvise(start as *mut libc::c_void, end - start, advice) };
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
        assert!(seg.section("missing").unwrap().is_none());
    }

    /// The directory as an older binary reads it: no `crc32`.
    #[derive(Deserialize)]
    struct OldDirEntry {
        name: String,
        offset: u64,
        len: u64,
    }

    fn directory<T: serde::de::DeserializeOwned>(image: &[u8]) -> Vec<T> {
        let dir_len = u64::from_le_bytes(image[16..24].try_into().unwrap()) as usize;
        serde_json::from_slice(&image[24..24 + dir_len]).unwrap()
    }

    #[test]
    fn a_damaged_section_fails_its_checksum_when_first_read() {
        let mut w = SegmentWriter::new(1);
        w.put("a", vec![1, 2, 3]);
        w.put("big", vec![7u8; 5000]);
        let mut image = w.finish();
        let old: Vec<OldDirEntry> = directory(&image);
        assert_eq!(old.iter().map(|e| (e.name.as_str(), e.len)).collect::<Vec<_>>(), [("a", 3), ("big", 5000)]);
        image[old[1].offset as usize + 4321] ^= 0x10;

        // Unchecked (memtable images), the damage is read as it is.
        let seg = SegmentFile::from_vec(image.clone()).unwrap();
        assert_eq!(seg.section("big").unwrap().unwrap()[4321], 7 ^ 0x10);

        let seg = SegmentFile::from_vec(image).unwrap().checked_on_read();
        assert_eq!(&*seg.require("a").unwrap(), &[1, 2, 3]);
        assert!(seg.has("big"));
        for _ in 0..2 {
            let e = seg.section("big").unwrap_err().to_string();
            assert!(e.contains("section big fails its checksum"), "{e}");
        }
        assert!(seg.clone().require("big").is_err(), "clones share what was checked");
        let e = seg.verify_sections().unwrap_err().to_string();
        assert!(e.contains("section big fails its checksum"), "{e}");
    }

    #[test]
    fn sections_without_a_crc32_are_read_unchecked() {
        // A segment written before sections had a crc32.
        let json = br#"[{"name":"a","offset":64,"len":3}]"#;
        let mut image = vec![0u8; 67];
        image[..8].copy_from_slice(MAGIC);
        image[8..16].copy_from_slice(&1u64.to_le_bytes());
        image[16..24].copy_from_slice(&(json.len() as u64).to_le_bytes());
        image[24..24 + json.len()].copy_from_slice(json);
        image[64..].copy_from_slice(&[4, 5, 6]);
        let seg = SegmentFile::from_vec(image).unwrap().checked_on_read();
        assert_eq!(&*seg.require("a").unwrap(), &[4, 5, 6]);
        seg.verify_sections().unwrap();
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
