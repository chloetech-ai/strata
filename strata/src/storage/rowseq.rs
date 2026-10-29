//! Row versions: in a database that numbers its writes
//! ([`crate::storage::Database::start_seq_no`]), the sequence number of the
//! write that last changed each row, which conditional writes compare
//! against and replicas compare by.
//!
//! A segment keeps its documents' numbers as they were when it was built,
//! in section `seq`. A value written in place since changes its document's
//! number too, kept with the value ([`crate::storage::updates::SEQ`]). A
//! document of a segment without the section (built before the database
//! numbered its writes, bulk loaded, or attached: [`without`]) has number
//! 0, as has a memtable document written before.
//!
//! Encoding: `u8` width (4 or 8), 7 zero bytes, a `u64` base, then each
//! document's number in docID order, little-endian: as a `u32` above the
//! base (width 4, when the numbers span less than 2^32), or as a `u64`
//! (width 8, base 0).

use crate::error::{corrupt, Result};
use crate::postings::bytes::OwnedBytes;
use crate::segment::{DocId, SegmentFile, SegmentWriter};

/// The section of a segment's numbers.
pub const SECTION: &str = "seq";

/// The section's bytes for documents numbered `seqs`, in docID order.
pub fn encode(seqs: &[u64]) -> Vec<u8> {
    let (lo, hi) = (seqs.iter().copied().min().unwrap_or(0), seqs.iter().copied().max().unwrap_or(0));
    let narrow = hi - lo <= u32::MAX as u64;
    let mut out = Vec::with_capacity(16 + seqs.len() * if narrow { 4 } else { 8 });
    out.push(if narrow { 4 } else { 8 });
    out.extend_from_slice(&[0; 7]);
    if narrow {
        out.extend_from_slice(&lo.to_le_bytes());
        for &s in seqs {
            out.extend_from_slice(&((s - lo) as u32).to_le_bytes());
        }
    } else {
        out.extend_from_slice(&0u64.to_le_bytes());
        for &s in seqs {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

/// A segment's numbers, as built.
pub struct Seqs {
    bytes: OwnedBytes,
    width: usize,
    base: u64,
}

impl Seqs {
    /// The numbers of `seg`'s documents, or `None` when it has none (they
    /// are all 0).
    pub fn of(seg: &SegmentFile) -> Result<Option<Seqs>> {
        let Some(bytes) = seg.section(SECTION)? else { return Ok(None) };
        let width = bytes.first().copied().unwrap_or(0) as usize;
        if !matches!(width, 4 | 8) || bytes.len() != 16 + seg.n_docs() * width {
            return Err(corrupt(format!("section {SECTION}: {} bytes of width {width} for {} documents", bytes.len(), seg.n_docs())));
        }
        let base = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        Ok(Some(Seqs { bytes, width, base }))
    }

    /// Document `d`'s number.
    pub fn get(&self, d: DocId) -> u64 {
        let at = 16 + d as usize * self.width;
        match self.width {
            4 => self.base + u32::from_le_bytes(self.bytes[at..at + 4].try_into().unwrap()) as u64,
            _ => u64::from_le_bytes(self.bytes[at..at + 8].try_into().unwrap()),
        }
    }
}

/// Document `d`'s number in `seg`, as built.
pub fn of(seg: &SegmentFile, d: DocId) -> Result<u64> {
    Ok(Seqs::of(seg)?.map_or(0, |s| s.get(d)))
}

/// `seg`'s image without its numbers, or `None` when it has none: a
/// segment attached from elsewhere carries the numbers of another
/// database's writes, which mean nothing here and could equal a later
/// write's.
pub fn without(seg: &SegmentFile) -> Result<Option<Vec<u8>>> {
    if !seg.has(SECTION) {
        return Ok(None);
    }
    let mut w = SegmentWriter::new(seg.n_docs());
    for (name, _) in seg.section_sizes() {
        if name != SECTION {
            let bytes = seg.require(&name)?.to_vec();
            w.put(name, bytes);
        }
    }
    Ok(Some(w.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(seqs: &[u64]) -> SegmentFile {
        let mut w = SegmentWriter::new(seqs.len());
        w.put(SECTION, encode(seqs));
        SegmentFile::from_vec(w.finish()).unwrap()
    }

    #[test]
    fn numbers_come_back() {
        for seqs in [vec![], vec![0, 0, 7], vec![5_000_000_000, 5_000_000_009, 5_000_000_001], vec![1, u64::MAX, 2]] {
            let seg = segment(&seqs);
            let got: Vec<u64> = (0..seqs.len() as DocId).map(|d| of(&seg, d).unwrap()).collect();
            assert_eq!(got, seqs);
        }
        assert_eq!(encode(&[9, 10]).len(), 16 + 8);
        assert_eq!(encode(&[1, 1 << 40]).len(), 16 + 16);
        // A segment without the section numbers its documents 0.
        let seg = SegmentFile::from_vec(SegmentWriter::new(3).finish()).unwrap();
        assert_eq!(of(&seg, 2).unwrap(), 0);
        // A section of the wrong length is damage.
        let mut w = SegmentWriter::new(3);
        w.put(SECTION, encode(&[1, 2]));
        assert!(of(&SegmentFile::from_vec(w.finish()).unwrap(), 0).is_err());
    }
}
