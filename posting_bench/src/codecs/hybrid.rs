//! Hybrid bitmap / compressed-list index (Culpepper & Moffat, "Efficient set
//! intersection for inverted indexing", TOIS 2010).
//!
//! Lists whose average d-gap is at most [`DENSE_GAP`] (i.e. `n ≥ u / 8`)
//! are stored as plain bitmaps over the whole universe, costing `u` bits
//! (at most 8 bits per posting); all other lists use [`Bp128`].
//!
//! Conjunctions intersect only the sparse lists (rarest-first leapfrog) and
//! then filter each candidate with `O(1)` bitmap probes; if every list is a
//! bitmap they are ANDed word by word. Disjunctions OR the bitmaps and set
//! the sparse lists' bits. Lucene 10.2 applies the same idea per block,
//! storing dense postings blocks as bitsets.

use super::bp128::Bp128;
use super::{Cursor, CursorList, PostingList, SetOps};
use crate::algorithms::{cursor as cur, union::extract_bits};
use crate::bits::BitVec;
use crate::END;

/// Lists with `universe / len <= DENSE_GAP` become bitmaps.
pub const DENSE_GAP: u64 = 8;

pub enum Hybrid {
    Bitmap { bits: BitVec, len: usize },
    List(Bp128),
}

impl Hybrid {
    fn bitmap(&self) -> Option<&BitVec> {
        match self {
            Hybrid::Bitmap { bits, .. } => Some(bits),
            Hybrid::List(_) => None,
        }
    }
}

impl PostingList for Hybrid {
    const NAME: &'static str = "Hybrid (bitmap + BP128)";

    fn build(docs: &[u32], universe: u32) -> Self {
        if !docs.is_empty() && (universe as u64) <= DENSE_GAP * docs.len() as u64 {
            let mut bits = BitVec::zeros(universe as usize);
            for &d in docs {
                bits.set(d as usize);
            }
            Hybrid::Bitmap { bits, len: docs.len() }
        } else {
            Hybrid::List(Bp128::build(docs, universe))
        }
    }

    fn len(&self) -> usize {
        match self {
            Hybrid::Bitmap { len, .. } => *len,
            Hybrid::List(l) => l.len(),
        }
    }

    fn size_bits(&self) -> u64 {
        match self {
            Hybrid::Bitmap { bits, .. } => bits.len() as u64,
            Hybrid::List(l) => l.size_bits(),
        }
    }

    fn decode_into(&self, out: &mut Vec<u32>) {
        match self {
            Hybrid::Bitmap { bits, .. } => extract_bits(bits.words(), 0, out),
            Hybrid::List(l) => l.decode_into(out),
        }
    }
}

pub enum HybridCursor<'a> {
    Bitmap { bits: &'a BitVec, cur: u32, len: usize },
    List(<Bp128 as CursorList>::Cursor<'a>),
}

impl Cursor for HybridCursor<'_> {
    #[inline]
    fn docid(&self) -> u32 {
        match self {
            HybridCursor::Bitmap { cur, .. } => *cur,
            HybridCursor::List(c) => c.docid(),
        }
    }

    #[inline]
    fn next(&mut self) -> u32 {
        match self {
            HybridCursor::Bitmap { bits, cur, .. } => {
                if *cur != END {
                    *cur = bits.next_one(*cur as usize + 1).map_or(END, |p| p as u32);
                }
                *cur
            }
            HybridCursor::List(c) => c.next(),
        }
    }

    #[inline]
    fn next_geq(&mut self, target: u32) -> u32 {
        match self {
            HybridCursor::Bitmap { bits, cur, .. } => {
                if *cur < target {
                    *cur = bits.next_one(target as usize).map_or(END, |p| p as u32);
                }
                *cur
            }
            HybridCursor::List(c) => c.next_geq(target),
        }
    }

    fn size(&self) -> usize {
        match self {
            HybridCursor::Bitmap { len, .. } => *len,
            HybridCursor::List(c) => c.size(),
        }
    }
}

impl CursorList for Hybrid {
    type Cursor<'a> = HybridCursor<'a>;

    fn cursor(&self) -> HybridCursor<'_> {
        match self {
            Hybrid::Bitmap { bits, len } => HybridCursor::Bitmap {
                bits,
                cur: bits.next_one(0).map_or(END, |p| p as u32),
                len: *len,
            },
            Hybrid::List(l) => HybridCursor::List(l.cursor()),
        }
    }
}

impl SetOps for Hybrid {
    fn intersect(lists: &[&Self], out: &mut Vec<u32>) {
        let bitmaps: Vec<&BitVec> = lists.iter().filter_map(|l| l.bitmap()).collect();
        let mut sparse: Vec<_> = lists
            .iter()
            .filter_map(|l| match l {
                Hybrid::List(l) => Some(l.cursor()),
                Hybrid::Bitmap { .. } => None,
            })
            .collect();
        if sparse.is_empty() {
            let words = bitmaps[0].words();
            let mut acc = words.to_vec();
            for b in &bitmaps[1..] {
                for (a, w) in acc.iter_mut().zip(b.words()) {
                    *a &= w;
                }
            }
            return extract_bits(&acc, 0, out);
        }
        let start = out.len();
        cur::and_leapfrog(&mut sparse, out);
        if !bitmaps.is_empty() {
            let mut kept = start;
            for r in start..out.len() {
                let d = out[r];
                if bitmaps.iter().all(|b| b.get(d as usize)) {
                    out[kept] = d;
                    kept += 1;
                }
            }
            out.truncate(kept);
        }
    }

    fn union(lists: &[&Self], out: &mut Vec<u32>) {
        let bitmaps: Vec<&BitVec> = lists.iter().filter_map(|l| l.bitmap()).collect();
        if bitmaps.is_empty() {
            let mut cursors: Vec<_> = lists.iter().map(|l| l.cursor()).collect();
            return cur::or_heap(&mut cursors, out);
        }
        let mut acc = bitmaps[0].words().to_vec();
        for b in &bitmaps[1..] {
            for (a, w) in acc.iter_mut().zip(b.words()) {
                *a |= w;
            }
        }
        let mut buf = Vec::new();
        for l in lists {
            if let Hybrid::List(l) = l {
                buf.clear();
                l.decode_into(&mut buf);
                for &d in &buf {
                    acc[d as usize / 64] |= 1u64 << (d % 64);
                }
            }
        }
        extract_bits(&acc, 0, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<Hybrid>();
        conformance::check_cursor::<Hybrid>();
        conformance::check_setops::<Hybrid>();
    }
}
