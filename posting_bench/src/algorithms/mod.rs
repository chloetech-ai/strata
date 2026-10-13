//! Intersection and union algorithms.
//!
//! * [`pairwise`] — two sorted slices (merge, galloping, binary search,
//!   Baeza-Yates, adaptive).
//! * [`simd`] — SIMD block-merge kernels (SSE, AVX2, AVX-512).
//! * [`multiway`] — `k` sorted slices (SvS, Small Adaptive, Sequential).
//! * [`union`] — `k` sorted slices (heap, fold, bitmap).
//! * [`cursor`] — `k` compressed lists through skip-capable cursors
//!   (leapfrog AND, round-robin AND, heap / min-scan / bitset-window OR).

pub mod cursor;
pub mod multiway;
pub mod pairwise;
pub mod simd;
pub mod union;
