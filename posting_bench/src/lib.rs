//! Compressed posting-list representations, set-operation algorithms, and a
//! benchmark testbed for comparing them.
//!
//! * [`codecs`] — posting-list representations (uncompressed, block codecs,
//!   Elias-Fano family, Roaring, hybrid bitmaps, compact tries, and production
//!   crates as reference baselines).
//! * [`algorithms`] — intersection and union algorithms over sorted arrays and
//!   over skip-capable cursors.
//! * [`data`] — synthetic collections, dataset loaders and query generation.
//! * [`bench`] — the measurement harness used by the `posting-bench` binary.

pub mod algorithms;
pub mod bench;
pub mod bits;
pub mod bytes;
pub mod codecs;
pub mod data;
pub mod stats;

/// Sentinel docID returned by exhausted cursors. Valid docIDs are `< END`.
pub const END: u32 = u32::MAX;
