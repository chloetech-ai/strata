//! The file: pages, extents, meta slots and the shared read-only mapping.
//!
//! ```text
//! page 0   meta slot A ┐ a [`Meta`] each; the valid one with the larger txn
//! page 1   meta slot B ┘ is the database
//! page 2.. extents (runs of pages): segments, delete sets, manifests, freelists
//! ```
//!
//! Reads go through one `MAP_SHARED` read-only mapping of the whole file.
//! Writes use positioned writes and never touch the mapping, so a page that a
//! reader can see changes only if the allocator handed it out again, which it
//! does only once no snapshot can reach it (see `freelist`).

use std::fs::File;
use std::os::unix::fs::FileExt;

use memmap2::Mmap;
use posting_bench::bytes::OwnedBytes;
use serde::{Deserialize, Serialize};

use crate::error::{corrupt, Result};

/// Page size in bytes.
pub const PAGE: u64 = 4096;
/// Pages taken by the two meta slots.
pub const META_PAGES: u64 = 2;

const MAGIC: &[u8; 8] = b"STRATADB";
/// On-disk format version, bumped on incompatible changes.
pub const FORMAT_VERSION: u32 = 1;
const META_LEN: usize = 84;

/// A run of pages holding `len` bytes, starting at page `page`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Extent {
    pub page: u64,
    pub len: u64,
}

impl Extent {
    /// Pages covered.
    pub fn pages(&self) -> u64 {
        self.len.div_ceil(PAGE)
    }

    /// Byte range within the file.
    pub fn range(&self) -> std::ops::Range<usize> {
        let start = (self.page * PAGE) as usize;
        start..start + self.len as usize
    }
}

/// Contents of a meta slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    /// Random id tying the WAL sidecar to this file.
    pub db_id: u64,
    pub txn: u64,
    pub manifest: Extent,
    pub manifest_crc: u32,
    pub freelist: Extent,
    pub freelist_crc: u32,
    /// File length in pages.
    pub page_count: u64,
}

impl Meta {
    /// One page: the fields, then a crc32 over them, then zeros.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(PAGE as usize);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        b.extend_from_slice(&(PAGE as u32).to_le_bytes());
        for v in [self.db_id, self.txn, self.manifest.page, self.manifest.len] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.manifest_crc.to_le_bytes());
        b.extend_from_slice(&self.freelist_crc.to_le_bytes());
        for v in [self.freelist.page, self.freelist.len, self.page_count] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let crc = crc32fast::hash(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        debug_assert_eq!(b.len(), META_LEN);
        b.resize(PAGE as usize, 0);
        b
    }

    /// `Ok(None)` for a slot that is empty, torn or otherwise fails its
    /// checksum; an error for a valid slot of an unsupported format.
    pub fn decode(page: &[u8]) -> Result<Option<Meta>> {
        if page.len() < META_LEN {
            return Ok(None);
        }
        let crc = u32::from_le_bytes(page[META_LEN - 4..META_LEN].try_into().unwrap());
        if crc32fast::hash(&page[..META_LEN - 4]) != crc || &page[..8] != MAGIC {
            return Ok(None);
        }
        let u32_at = |o: usize| u32::from_le_bytes(page[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(page[o..o + 8].try_into().unwrap());
        if u32_at(8) != FORMAT_VERSION || u32_at(12) as u64 != PAGE {
            return Err(corrupt(format!("unsupported format version {} / page size {}", u32_at(8), u32_at(12))));
        }
        Ok(Some(Meta {
            db_id: u64_at(16),
            txn: u64_at(24),
            manifest: Extent { page: u64_at(32), len: u64_at(40) },
            manifest_crc: u32_at(48),
            freelist_crc: u32_at(52),
            freelist: Extent { page: u64_at(56), len: u64_at(64) },
            page_count: u64_at(72),
        }))
    }
}

/// Reads both meta slots; returns the valid one with the larger txn and its
/// slot number.
pub fn read_meta(file: &File) -> Result<(Meta, usize)> {
    let mut best: Option<(Meta, usize)> = None;
    for slot in 0..2 {
        let mut page = vec![0u8; PAGE as usize];
        // A short file (crash during creation) reads as an invalid slot.
        let n = read_up_to(file, &mut page, slot as u64 * PAGE)?;
        if let Some(m) = Meta::decode(&page[..n])? {
            if best.is_none_or(|(b, _)| m.txn > b.txn) {
                best = Some((m, slot));
            }
        }
    }
    best.ok_or_else(|| corrupt("no valid meta slot"))
}

/// Writes `meta` into `slot`.
pub fn write_meta(file: &File, slot: usize, meta: &Meta) -> Result<()> {
    file.write_all_at(&meta.encode(), slot as u64 * PAGE)?;
    Ok(())
}

/// Reads an extent with a positioned read and checks its crc32.
pub fn read_extent(file: &File, e: Extent, crc: u32) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; e.len as usize];
    file.read_exact_at(&mut buf, e.page * PAGE)?;
    if crc32fast::hash(&buf) != crc {
        return Err(corrupt(format!("checksum mismatch in extent at page {}", e.page)));
    }
    Ok(buf)
}

fn read_up_to(file: &File, buf: &mut [u8], offset: u64) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read_at(&mut buf[n..], offset + n as u64)? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

struct MapOwner(Mmap);

impl AsRef<[u8]> for MapOwner {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Maps the whole file read-only. The bytes stay valid while any slice of
/// them is alive, so snapshots opened against an older (shorter) mapping keep
/// working after the file grows and is mapped again. Without `read_ahead`,
/// the mapping is advised `MADV_RANDOM`.
pub fn map_file(file: &File, read_ahead: bool) -> Result<OwnedBytes> {
    if file.metadata()?.len() == 0 {
        return Ok(OwnedBytes::empty());
    }
    // SAFETY: the mapping is read-only and Strata never writes a page that a
    // live reader can reach; the file is guarded by an exclusive lock.
    let map = unsafe { memmap2::MmapOptions::new().map(file)? };
    if !read_ahead {
        map.advise(memmap2::Advice::Random)?;
    }
    Ok(OwnedBytes::new(MapOwner(map)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_roundtrip_and_torn_slot() {
        let m = Meta {
            db_id: 0xdead_beef,
            txn: 42,
            manifest: Extent { page: 9, len: 5000 },
            manifest_crc: 7,
            freelist: Extent { page: 11, len: 64 },
            freelist_crc: 8,
            page_count: 12,
        };
        let page = m.encode();
        assert_eq!(page.len(), PAGE as usize);
        assert_eq!(Meta::decode(&page).unwrap(), Some(m));
        for i in [0, 20, 60, 83] {
            let mut bad = page.clone();
            bad[i] ^= 1;
            assert_eq!(Meta::decode(&bad).unwrap(), None, "flip at byte {i}");
        }
        assert_eq!(Meta::decode(&vec![0u8; PAGE as usize]).unwrap(), None);
        assert_eq!(Extent { page: 3, len: 4097 }.pages(), 2);
    }
}
