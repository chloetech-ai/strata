//! Single-file storage: meta slots, extents, the shadow-paging commit, the
//! WAL and memtable, snapshots, delete sets, primary keys, merges and
//! vacuum (DESIGN.md §3; measurements in `notes/storage.md`).
//!
//! * [`pager`]: file layout, meta slots, the read-only mapping.
//! * [`freelist`]: free page runs and deferred frees.
//! * [`manifest`]: one on-disk version (schema, segments, delete sets).
//! * [`wal`]: the `-wal` sidecar.
//! * [`live`]: delete sets as live-document filters.
//! * [`pk`]: the primary-key index section.
//! * [`merge`]: the tiered merge policy.
//! * [`Database`], [`WriteTxn`], [`Snapshot`]: the API.

mod db;
pub mod freelist;
pub mod live;
pub mod manifest;
pub mod merge;
pub mod pager;
pub mod pk;
mod snapshot;
mod state;
pub mod wal;

use std::sync::{Mutex, MutexGuard};

pub use db::{Database, DbOptions, DocumentSource, FileStats, SegmentStats, WriteTxn};
pub use live::LiveDocs;
pub use merge::{MergePolicy, MergeStats, SegmentInfo};
pub use pk::{build_pk_index, key_hash, KeyRef, PkIndex};
pub use snapshot::{SegmentView, Snapshot};

/// Locks a mutex, ignoring poisoning: every critical section leaves the
/// state consistent before anything that can panic.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
