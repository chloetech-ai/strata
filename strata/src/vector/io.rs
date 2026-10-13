//! Read-ahead hints for segments mapped from disk.
//!
//! A search on a cold segment otherwise reads one page per page fault, in
//! sequence: an IVF probe of 15 pages costs 15 device round trips, an HNSW
//! expansion one per neighbour. When hints are on, the search first tells
//! the kernel every range it is about to touch (`madvise(MADV_WILLNEED)`),
//! so the reads are issued together and overlap. On memory that is already
//! resident a hint is a cheap system call; on heap memory it is a no-op.

use crate::iostat::PAGE;

/// Asks the kernel to start reading the pages covering `ranges`
/// (`(start, len)` byte ranges in one mapping), merging ranges that share or
/// touch pages into one call.
pub(crate) fn will_need(ranges: &mut [(usize, usize)]) {
    if ranges.is_empty() {
        return;
    }
    ranges.sort_unstable();
    let mut run: Option<(usize, usize)> = None;
    for &(start, len) in ranges.iter() {
        let (lo, hi) = (start & !(PAGE - 1), (start + len).next_multiple_of(PAGE));
        run = match run {
            Some((a, b)) if lo <= b => Some((a, b.max(hi))),
            Some((a, b)) => {
                advise(a, b);
                Some((lo, hi))
            }
            None => Some((lo, hi)),
        };
    }
    if let Some((a, b)) = run {
        advise(a, b);
    }
}

/// Hint for one contiguous range.
pub(crate) fn will_need_range(start: *const u8, len: usize) {
    will_need(&mut [(start as usize, len)]);
}

fn advise(lo: usize, hi: usize) {
    // SAFETY: WILLNEED only schedules reads of pages of an existing mapping;
    // it never changes contents, and errors (e.g. a range that is not a file
    // mapping) are harmless and ignored.
    unsafe {
        libc::madvise(lo as *mut libc::c_void, hi - lo, libc::MADV_WILLNEED);
    }
}
