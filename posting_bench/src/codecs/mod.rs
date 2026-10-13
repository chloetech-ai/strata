//! Posting-list representations.
//!
//! Every representation implements [`PostingList`]. Representations that
//! support forward iteration with skipping (`NextGEQ`) implement
//! [`CursorList`]; those with their own set-operation algorithms (Roaring,
//! compact tries, hybrid bitmaps, ...) implement [`SetOps`]. The benchmark
//! harness runs every applicable algorithm on every representation.

pub mod block;
pub mod bp128;
pub mod elias_fano;
pub mod entropic;
pub mod external;
pub mod hybrid;
pub mod interpolative;
pub mod loom;
pub mod pef;
pub mod pfor;
pub mod pyramid;
pub mod roaring;
pub mod trie;
pub mod uncompressed;
pub mod vbyte;

#[cfg(test)]
pub(crate) mod conformance;

/// A posting list: a strictly increasing sequence of docIDs, all smaller than
/// the collection's `universe` (and therefore smaller than [`crate::END`]).
pub trait PostingList: Sized + Send + Sync {
    /// Short display name used in reports.
    const NAME: &'static str;

    /// Encode a strictly increasing list of docIDs, each `< universe`.
    /// `docs` may be empty.
    fn build(docs: &[u32], universe: u32) -> Self;

    /// Number of postings.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Encoded size in bits, including skip data and other metadata needed
    /// for querying, but excluding in-memory container overhead (Vec headers,
    /// allocator slack). This is what a serialized index would occupy.
    fn size_bits(&self) -> u64;

    /// Decode the whole list, appending the docIDs to `out`.
    fn decode_into(&self, out: &mut Vec<u32>);

    /// Bytes of memory derived at load time that are not part of
    /// [`size_bits`](Self::size_bits), such as decode tables rebuilt from a
    /// stored model. Zero for most codecs.
    fn aux_memory_bytes(&self) -> u64 {
        0
    }

    /// Bytes of process-wide memory shared by all lists of this codec (e.g.
    /// decode tables of shared models), measured after building an index.
    fn shared_aux_memory_bytes() -> u64 {
        0
    }
}

/// Forward-only iterator over a posting list with skipping.
///
/// A fresh cursor is positioned on the first posting (or on [`crate::END`]
/// for an empty list). Cursors never move backwards.
pub trait Cursor {
    /// Current docID, or [`crate::END`] once exhausted.
    fn docid(&self) -> u32;

    /// Move to the next posting and return it ([`crate::END`] when exhausted).
    fn next(&mut self) -> u32;

    /// Move to the first posting `>= target` and return it. If the current
    /// posting is already `>= target` the cursor does not move.
    fn next_geq(&mut self, target: u32) -> u32;

    /// Number of postings in the underlying list (used to order cursors).
    fn size(&self) -> usize;
}

/// A posting list that can be traversed with a [`Cursor`].
pub trait CursorList: PostingList {
    type Cursor<'a>: Cursor
    where
        Self: 'a;

    fn cursor(&self) -> Self::Cursor<'_>;
}

/// A representation with native multi-list set operations.
pub trait SetOps: PostingList {
    /// Intersect all `lists` (at least one), appending the sorted result to `out`.
    fn intersect(lists: &[&Self], out: &mut Vec<u32>);

    /// Union all `lists` (at least one), appending the sorted result to `out`.
    fn union(lists: &[&Self], out: &mut Vec<u32>);
}
