//! The manifest: one committed version of the database's on-disk state,
//! stored as JSON in its own extent and named by a meta slot.

use serde::{Deserialize, Serialize};

use crate::error::{corrupt, Result};
use crate::schema::Schema;
use crate::storage::pager::Extent;

/// The on-disk state named by a meta slot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: Schema,
    /// The commit that wrote this manifest.
    pub txn: u64,
    /// WAL transactions up to this txn are contained in the segments and
    /// skipped on replay (they can survive a crash between a flush's commit
    /// and the WAL truncation that follows it).
    pub wal_txn: u64,
    pub next_segment_id: u64,
    /// Oldest first.
    pub segments: Vec<SegmentMeta>,
}

/// Where a segment and its delete set live.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub id: u64,
    pub extent: Extent,
    /// crc32 of the segment image (checked by `Database::check`).
    pub crc: u32,
    pub n_docs: u32,
    pub n_deleted: u32,
    /// Bitset of deleted docIDs, `n_docs.div_ceil(64)` little-endian words.
    pub deletes: Option<Extent>,
    pub deletes_crc: u32,
    /// The commit that wrote the segment.
    pub txn: u64,
}

impl SegmentMeta {
    pub fn n_live(&self) -> u32 {
        self.n_docs - self.n_deleted
    }
}

impl Manifest {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("manifest serialises")
    }

    pub fn decode(b: &[u8]) -> Result<Manifest> {
        serde_json::from_slice(b).map_err(|e| corrupt(format!("manifest: {e}")))
    }
}
