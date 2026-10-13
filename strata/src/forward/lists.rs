//! A table of Pyramid lists in one section (keyword value lists, range
//! buckets), parsed lazily.
//!
//! Layout: the list count, each list's cardinality, each list's byte
//! offset, then the lists ([`Pyramid::write_to`]), each starting on a
//! 64-byte boundary of the section so that their masks and bitmaps sit on
//! cache lines in a mapped file. Opening reads only the two small arrays;
//! a list's descriptors are parsed (and checked) on first use and cached,
//! so a cold open touches no list.

use std::sync::{Arc, OnceLock};

use posting_bench::bytes::{ByteReader, ByteWriter, OwnedBytes, Slice};
use posting_bench::codecs::pyramid::Pyramid;
use posting_bench::codecs::PostingList;

use crate::error::{corrupt, Result};

/// Appends `lists` to `w` (see the module docs).
pub(crate) fn write_lists(w: &mut ByteWriter, lists: &[Pyramid]) {
    w.u64(lists.len() as u64);
    w.array(&lists.iter().map(|l| l.cardinality()).collect::<Vec<u64>>());
    // `array` writes the length, then the elements (already 8-aligned).
    let at = w.buf.len() + 8;
    w.array(&vec![0u64; lists.len()]);
    for (i, l) in lists.iter().enumerate() {
        while w.buf.len() % 64 != 0 {
            w.u64(0);
        }
        let off = w.buf.len() as u64;
        w.buf[at + 8 * i..at + 8 * i + 8].copy_from_slice(&off.to_le_bytes());
        l.write_to(w);
    }
}

/// Pyramid lists of a section, parsed on first use.
#[derive(Clone, Debug)]
pub(crate) struct ListTable {
    section: OwnedBytes,
    cards: Slice<u64>,
    offs: Slice<u64>,
    cache: Arc<[OnceLock<Pyramid>]>,
}

impl ListTable {
    /// Reads the table that `r` (a reader over `section`) is positioned at.
    pub fn read(r: &mut ByteReader, section: &OwnedBytes) -> Result<ListTable> {
        let n = r.u64().map_err(corrupt)? as usize;
        let cards: Slice<u64> = r.array().map_err(corrupt)?;
        let offs: Slice<u64> = r.array().map_err(corrupt)?;
        if cards.len() != n || offs.len() != n || offs.iter().any(|&o| o as usize >= section.len()) {
            return Err(corrupt("bad list table"));
        }
        Ok(ListTable { section: section.clone(), cards, offs, cache: (0..n).map(|_| OnceLock::new()).collect() })
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    /// Number of docIDs of list `i` (without parsing it).
    #[inline]
    pub fn card(&self, i: usize) -> u64 {
        self.cards[i]
    }

    /// List `i`, a zero-copy view.
    ///
    /// # Panics
    /// If the list's descriptors are corrupt (they are checked here, on
    /// first use, rather than when the segment is opened).
    pub fn get(&self, i: usize) -> &Pyramid {
        self.cache[i].get_or_init(|| {
            let bytes = self.section.slice(self.offs[i] as usize..self.section.len());
            match Pyramid::from_bytes(bytes) {
                Ok(p) if p.cardinality() == self.cards[i] => p,
                Ok(_) => panic!("corrupt segment: list {i} cardinality mismatch"),
                Err(e) => panic!("corrupt segment: list {i}: {e}"),
            }
        })
    }

    /// Bytes of the lists (from the first list to the end of the section).
    pub fn lists_bytes(&self) -> usize {
        self.offs.first().map_or(0, |&o| self.section.len() - o as usize)
    }
}

/// Merges two sorted, disjoint docID lists.
pub(crate) fn merge_sorted(a: Vec<u32>, b: Vec<u32>) -> Vec<u32> {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] < b[j] {
            out.push(a[i]);
            i += 1;
        } else {
            out.push(b[j]);
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// Builds a Pyramid list of sorted docIDs.
pub(crate) fn build_list(docs: &[u32], n: usize) -> Pyramid {
    Pyramid::build(docs, n as u32)
}
