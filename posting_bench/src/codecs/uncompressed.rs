//! Plain sorted `u32` array: the uncompressed baseline (32 bits per posting).
//! `next_geq` uses galloping (exponential) search from the current position.

use super::{Cursor, CursorList, PostingList};
use crate::END;

#[derive(Clone, Debug, Default)]
pub struct Uncompressed {
    docs: Vec<u32>,
}

impl Uncompressed {
    pub fn as_slice(&self) -> &[u32] {
        &self.docs
    }
}

impl PostingList for Uncompressed {
    const NAME: &'static str = "Uncompressed";

    fn build(docs: &[u32], _universe: u32) -> Self {
        Self { docs: docs.to_vec() }
    }

    fn len(&self) -> usize {
        self.docs.len()
    }

    fn size_bits(&self) -> u64 {
        32 * self.docs.len() as u64
    }

    fn decode_into(&self, out: &mut Vec<u32>) {
        out.extend_from_slice(&self.docs);
    }
}

pub struct SliceCursor<'a> {
    docs: &'a [u32],
    pos: usize,
}

impl<'a> SliceCursor<'a> {
    pub fn new(docs: &'a [u32]) -> Self {
        Self { docs, pos: 0 }
    }
}

/// Index of the first element `>= target` in `docs[from..]` (or `docs.len()`),
/// found by galloping: probe `from + 1, from + 2, from + 4, ...` and then
/// binary-search the last interval.
#[inline]
pub fn gallop_geq(docs: &[u32], from: usize, target: u32) -> usize {
    let n = docs.len();
    if from >= n || docs[from] >= target {
        return from;
    }
    // Invariant: docs[lo] < target.
    let mut lo = from;
    let mut step = 1;
    let mut hi = from + 1;
    while hi < n && docs[hi] < target {
        lo = hi;
        step *= 2;
        hi = from + step;
    }
    let hi = hi.min(n);
    lo + 1 + docs[lo + 1..hi].partition_point(|&x| x < target)
}

impl Cursor for SliceCursor<'_> {
    #[inline]
    fn docid(&self) -> u32 {
        self.docs.get(self.pos).copied().unwrap_or(END)
    }

    #[inline]
    fn next(&mut self) -> u32 {
        if self.pos < self.docs.len() {
            self.pos += 1;
        }
        self.docid()
    }

    #[inline]
    fn next_geq(&mut self, target: u32) -> u32 {
        self.pos = gallop_geq(self.docs, self.pos, target);
        self.docid()
    }

    fn size(&self) -> usize {
        self.docs.len()
    }
}

impl CursorList for Uncompressed {
    type Cursor<'a> = SliceCursor<'a>;

    fn cursor(&self) -> SliceCursor<'_> {
        SliceCursor::new(&self.docs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<Uncompressed>();
        conformance::check_cursor::<Uncompressed>();
    }
}
