//! Single-file storage: meta slots, extents, the shadow-paging commit, the
//! WAL and memtable, snapshots, delete sets, primary keys, merges and
//! vacuum (DESIGN.md §3; measurements in `notes/storage.md`).
//!
//! * [`pager`]: file layout, meta slots, the read-only mapping.
//! * [`dir`]: the directory layout, one file per segment.
//! * [`io`]: every change to the files, and (feature `fault-injection`)
//!   power losses and failing disks for tests.
//! * [`freelist`]: free page runs and deferred frees.
//! * [`manifest`]: one on-disk version (schema, segments, delete sets).
//! * [`wal`]: the `-wal` sidecar.
//! * [`live`]: delete sets as live-document filters.
//! * [`updates`]: in-place updates of numeric fields.
//! * [`pk`]: the primary-key index section.
//! * [`merge`]: the tiered merge policy, and what the background merger
//!   needs (memory estimates and limits, a write throttle, the record of
//!   interrupted merges).
//! * [`Database`], [`WriteTxn`], [`Snapshot`]: the API.

mod db;
pub mod dir;
pub mod freelist;
pub mod io;
pub mod live;
pub mod manifest;
pub mod merge;
pub mod pager;
pub mod pk;
mod snapshot;
mod state;
pub mod updates;
pub mod wal;

use std::sync::{Mutex, MutexGuard};

pub use db::{CheckpointOptions, CheckpointStats, Database, DbOptions, DocumentSource, DropOptions, FileStats, Layout, SegmentStats, TableTxn, WriteTxn, LOCK_WAIT};
pub use live::LiveDocs;
pub use manifest::SeqNo;
pub use merge::{memory_limit, merge_memory, MergePolicy, MergeStats, MergeStatus, MergerOptions, SegmentInfo};
pub use pk::{build_pk_index, build_pk_index_of, key_hash, KeyList, KeyRef, PkIndex};
pub use snapshot::{SegmentView, Snapshot};
pub use updates::Updates;

/// Locks a mutex, ignoring poisoning: every critical section leaves the
/// state consistent before anything that can panic.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
