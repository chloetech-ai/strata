//! Delete sets: which documents of a segment are deleted.
//!
//! A delete set is a bitset of deleted docIDs (`n_docs.div_ceil(64)` words),
//! stored in its own page-aligned extent and replaced copy-on-write when it
//! changes, so the segment itself stays immutable. [`LiveDocs`] views it in
//! place and exposes the complement, the live documents, as a [`DocFilter`].

use posting_bench::bytes::{OwnedBytes, Slice};

use crate::error::{corrupt, Result};
use crate::segment::{DocFilter, DocId, DocSet};

/// Live documents of a segment: `0..n_docs` minus a deleted-docs bitset.
#[derive(Clone, Debug)]
pub struct LiveDocs {
    deleted: Slice<u64>,
    n_docs: u32,
    n_live: u32,
}

impl LiveDocs {
    /// Views a stored delete set; `n_deleted` comes from the manifest, so
    /// opening reads nothing.
    pub(crate) fn view(bytes: OwnedBytes, n_docs: u32, n_deleted: u32) -> Result<LiveDocs> {
        let deleted: Slice<u64> = Slice::view(bytes).map_err(corrupt)?;
        if deleted.len() != words_for(n_docs) || n_deleted > n_docs {
            return Err(corrupt("delete set size disagrees with segment"));
        }
        Ok(LiveDocs { deleted, n_docs, n_live: n_docs - n_deleted })
    }

    /// From a deleted-docs bitset over `0..n_docs` (bits past `n_docs` zero).
    pub fn from_deleted(words: Vec<u64>, n_docs: u32) -> LiveDocs {
        assert_eq!(words.len(), words_for(n_docs));
        let n_deleted: u32 = words.iter().map(|w| w.count_ones()).sum();
        LiveDocs { deleted: Slice::Owned(words), n_docs, n_live: n_docs - n_deleted }
    }

    /// `base` (or no deletes) plus `more` deleted docIDs.
    pub fn with_deleted(base: Option<&LiveDocs>, n_docs: u32, more: impl IntoIterator<Item = DocId>) -> LiveDocs {
        let mut words = base.map_or_else(|| vec![0; words_for(n_docs)], |b| b.deleted.to_vec());
        for d in more {
            words[d as usize / 64] |= 1 << (d % 64);
        }
        LiveDocs::from_deleted(words, n_docs)
    }

    pub fn n_docs(&self) -> u32 {
        self.n_docs
    }

    pub fn n_live(&self) -> u32 {
        self.n_live
    }

    pub fn n_deleted(&self) -> u32 {
        self.n_docs - self.n_live
    }

    pub fn is_deleted(&self, d: DocId) -> bool {
        self.deleted[d as usize / 64] >> (d % 64) & 1 == 1
    }

    /// The deleted-docs bitset words.
    pub fn deleted_words(&self) -> &[u64] {
        &self.deleted
    }

    /// The live documents as a set.
    pub fn to_docset(&self) -> DocSet {
        let mut words: Vec<u64> = self.deleted.iter().map(|w| !w).collect();
        if !self.n_docs.is_multiple_of(64) {
            *words.last_mut().unwrap() &= (1u64 << (self.n_docs % 64)) - 1;
        }
        DocSet::from_bits(words, self.n_docs)
    }

    /// Bytes of the stored form.
    pub(crate) fn encode(&self) -> Vec<u8> {
        self.deleted.as_bytes().to_vec()
    }
}

impl DocFilter for LiveDocs {
    fn contains(&self, doc: DocId) -> bool {
        doc < self.n_docs && !self.is_deleted(doc)
    }
    fn cardinality(&self) -> usize {
        self.n_live as usize
    }
}

/// Words in the bitset of a segment of `n_docs` documents.
pub fn words_for(n_docs: u32) -> usize {
    (n_docs as usize).div_ceil(64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_and_stored_form() {
        let live = LiveDocs::with_deleted(None, 130, [0, 64, 129]);
        assert_eq!((live.n_live(), live.n_deleted()), (127, 3));
        assert!(!live.contains(0) && live.contains(1) && !live.contains(129) && !live.contains(130));
        let stored = LiveDocs::view(OwnedBytes::new(live.encode()), 130, 3).unwrap();
        assert_eq!(stored.deleted_words(), live.deleted_words());
        let more = LiveDocs::with_deleted(Some(&stored), 130, [5]);
        assert_eq!(more.n_live(), 126);
        let set = more.to_docset();
        assert_eq!(set.len(), 126);
        assert!(!set.contains(5) && set.contains(6) && !set.contains(129));
        assert!(LiveDocs::view(OwnedBytes::new(vec![0u8; 8]), 130, 0).is_err());
    }
}
