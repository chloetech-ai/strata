//! Tiered merge policy.
//!
//! Segments are grouped into tiers by live size: tier `k` holds segments of
//! `floor_docs * merge_factor^k ..floor_docs * merge_factor^(k+1)` live
//! documents (everything below `floor_docs` is tier 0). When a tier holds
//! `merge_factor` segments, its smallest `merge_factor` are merged into one
//! segment of the next tier, so every document is rewritten about
//! `log_factor(n / floor_docs)` times over the life of the table. A segment
//! whose deleted fraction exceeds `max_deleted_ratio` is rewritten alone to
//! drop its deleted documents. Segments too large to merge with a peer
//! without exceeding `max_merged_docs` are left alone.
//!
//! Also what the background merger needs ([`MergerOptions`]): an estimate
//! of a merge's memory ([`merge_memory`]), the memory the process may use
//! ([`memory_limit`]), a shared limit on how fast merges write ([`Pacer`]),
//! and the record of merges a crash interrupted ([`MergeRecord`]).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Which segments to merge.
#[derive(Clone, Debug)]
pub struct MergePolicy {
    pub merge_factor: usize,
    pub floor_docs: usize,
    pub max_merged_docs: usize,
    pub max_deleted_ratio: f64,
}

impl Default for MergePolicy {
    fn default() -> Self {
        MergePolicy { merge_factor: 8, floor_docs: 1000, max_merged_docs: 2_000_000, max_deleted_ratio: 0.3 }
    }
}

/// What a merge policy sees of a segment.
#[derive(Clone, Copy, Debug)]
pub struct SegmentInfo {
    pub id: u64,
    pub n_docs: u32,
    pub n_live: u32,
    /// The size of the segment's image.
    pub bytes: u64,
}

/// Work done by `Database::merge`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MergeStats {
    pub merges: usize,
    pub segments_in: usize,
    /// Live documents rewritten.
    pub docs_merged: u64,
    /// Deleted documents dropped.
    pub docs_dropped: u64,
    /// Documents past their table's retention dropped
    /// ([`crate::schema::Schema::with_retain`]).
    pub docs_expired: u64,
    pub bytes_written: u64,
    /// Bytes merges wrote to their spill files as they went, which the
    /// merged images then copied ([`crate::storage::DbOptions::merge_spill_bytes`]).
    pub bytes_spilled: u64,
    pub seconds: f64,
    /// Merges the policy selected that were made smaller, or left for
    /// later, to fit the memory budget or after a crash interrupted a merge
    /// of the same segments.
    pub merges_limited: usize,
}

/// How the background merger runs ([`crate::storage::Database::spawn_merger`]).
#[derive(Clone, Debug)]
pub struct MergerOptions {
    pub policy: MergePolicy,
    /// Merges that may run at once, each on a thread of its own.
    pub threads: usize,
    /// Memory the running merges may use together, as [`merge_memory`]
    /// estimates it; `None`: a quarter of [`memory_limit`]. A merge that
    /// would not fit is made smaller (fewer inputs), or waits for running
    /// ones to finish.
    pub memory_budget: Option<u64>,
    /// Bytes a second that merges may write, together, to their images and
    /// their spill files ([`crate::storage::DbOptions::merge_spill_bytes`]);
    /// `None`: as fast as the disk takes them.
    pub write_bytes_per_sec: Option<u64>,
}

impl Default for MergerOptions {
    /// Half the cores, between 1 and 4 merges at once (as Lucene's
    /// `ConcurrentMergeScheduler` on an SSD).
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        MergerOptions { policy: MergePolicy::default(), threads: (cores / 2).clamp(1, 4), memory_budget: None, write_bytes_per_sec: None }
    }
}

/// What the merges of a database are doing.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MergeStatus {
    /// Merges running now, and the segments they merge.
    pub running: usize,
    pub segments_merging: usize,
    /// Their estimated memory ([`merge_memory`]) and the budget it must
    /// fit, with the background merger.
    pub memory: u64,
    pub memory_budget: Option<u64>,
    /// Merges the background merger finished since the database opened.
    pub merges: u64,
    /// Merges the policy selected that were made smaller, or left for
    /// later, to fit the memory budget or a record of interrupted merges.
    pub merges_limited: u64,
    /// Groups of segments whose merge was interrupted (the process died
    /// while it ran): their merges are kept below half the memory each
    /// interrupted one was estimated to need.
    pub interrupted: usize,
    /// Why the background merger's last merge failed, until one succeeds.
    pub error: Option<String>,
}

/// A merge decodes its input documents in batches of about this many
/// bytes of their images, at least `MERGE_BATCH_DOCS.start` documents (so
/// that a TEXT field still analyses a run per build thread) and at most
/// `MERGE_BATCH_DOCS.end`.
pub(crate) const MERGE_BATCH_BYTES: usize = 32 << 20;
pub(crate) const MERGE_BATCH_DOCS: std::ops::Range<usize> = 16_384..262_144;

/// The estimated peak memory of merging segments with a build on
/// `threads` threads. A merge holds a batch of decoded documents, the next
/// one being decoded, and the builder's structures for the batch; it keeps
/// compact forms of the batches before (packed postings, keys, the
/// compressed doc store, columns, a vector graph) and finally the merged
/// sections. Over the first four batches' worth of input image that comes
/// to at most about [`memory_per_image_byte`] times the bytes; each byte
/// after them adds about [`memory_per_streamed_byte`]. Only image bytes
/// that hold live documents count.
pub fn merge_memory(segs: &[SegmentInfo], threads: usize) -> u64 {
    let live: f64 = segs.iter().map(|s| s.bytes as f64 * s.n_live as f64 / s.n_docs.max(1) as f64).sum();
    // A batch is MERGE_BATCH_BYTES, unless that is fewer documents than a
    // batch takes at least.
    let per_doc = segs.iter().map(|s| s.bytes as f64 / s.n_docs.max(1) as f64).fold(0.0, f64::max);
    let batch = (MERGE_BATCH_BYTES as f64).max(MERGE_BATCH_DOCS.start as f64 * per_doc);
    let first = live.min(4.0 * batch);
    (first * memory_per_image_byte(threads) + (live - first) * memory_per_streamed_byte(threads)) as u64
}

/// A merge's peak memory per byte of input image, over its first four
/// batches, when its build uses `threads` threads, with a margin over what
/// was measured (`examples/merge_memory.rs`, merges of 15 to 937 MB of
/// short text, long text, and text with 64- and 384-dim vectors): up to
/// 4.0 on one thread, 6.6 on four and 8.2 on eight, in merges of short
/// texts small enough to fit one batch (each thread analyses its own run
/// of documents).
pub fn memory_per_image_byte(threads: usize) -> f64 {
    5.5 + 0.5 * threads.max(1) as f64
}

/// What each byte of input image past a merge's first four batches adds to
/// its peak memory, with a margin over what was measured between merges of
/// 148 to 937 MB: up to 1.8 on one thread and 2.1 on four, long texts and
/// texts with 64-dim vectors taking the most (postings with positions
/// packed per run, a vector graph, and the merged sections they become),
/// 384-dim vectors 1.2 to 1.4 and short texts 0.4 to 0.5.
pub fn memory_per_streamed_byte(threads: usize) -> f64 {
    2.0 + 0.25 * threads.max(1) as f64
}

/// The memory this process may use: the tightest cgroup limit that
/// applies to it (v2 `memory.max`, v1 `memory.limit_in_bytes`), else the
/// machine's RAM; `None` when neither can be read.
pub fn memory_limit() -> Option<u64> {
    let ram = physical_memory();
    match cgroup_limit() {
        Some(c) => Some(ram.map_or(c, |r| r.min(c))),
        None => ram,
    }
}

fn physical_memory() -> Option<u64> {
    // SAFETY: sysconf only reads system configuration.
    let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    (pages > 0 && size > 0).then(|| pages as u64 * size as u64)
}

/// The cgroup memory limit of this process, from `/proc/self/cgroup`; on
/// v2 the limits of the cgroup and its ancestors (the smallest applies).
fn cgroup_limit() -> Option<u64> {
    let groups = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let read = |p: PathBuf| std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse::<u64>().ok());
    let mut best: Option<u64> = None;
    for line in groups.lines() {
        let mut parts = line.splitn(3, ':');
        let (_, controllers, path) = (parts.next()?, parts.next()?, parts.next()?);
        let path = path.trim_start_matches('/');
        if controllers.is_empty() {
            // v2: "max" (no limit) does not parse.
            let mut dir = Path::new("/sys/fs/cgroup").join(path);
            loop {
                if let Some(l) = read(dir.join("memory.max")) {
                    best = Some(best.map_or(l, |b| b.min(l)));
                }
                if !dir.pop() || !dir.starts_with("/sys/fs/cgroup") {
                    break;
                }
            }
        } else if controllers.split(',').any(|c| c == "memory") {
            // v1: no limit reads as a number near 2^63.
            let root = Path::new("/sys/fs/cgroup/memory");
            for dir in [root.join(path), root.to_path_buf()] {
                if let Some(l) = read(dir.join("memory.limit_in_bytes")).filter(|&l| l < 1 << 60) {
                    best = Some(best.map_or(l, |b| b.min(l)));
                }
            }
        }
    }
    best
}

/// A limit on how fast merges write, shared by the merges running at once:
/// each chunk written waits for its share of the rate.
pub struct Pacer {
    bytes_per_sec: u64,
    next: Mutex<Instant>,
}

impl Pacer {
    pub fn new(bytes_per_sec: u64) -> Pacer {
        Pacer { bytes_per_sec: bytes_per_sec.max(1), next: Mutex::new(Instant::now()) }
    }

    /// Waits until `bytes` more may be written.
    pub fn pace(&self, bytes: usize) {
        let wait = {
            let mut next = crate::storage::lock(&self.next);
            let now = Instant::now();
            let start = (*next).max(now);
            *next = start + Duration::from_secs_f64(bytes as f64 / self.bytes_per_sec as f64);
            start - now
        };
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

/// The merges a database is running, and those a crash interrupted, in a
/// file beside it, so that a process killed while merging (for running out
/// of memory, typically) does not restart into the same merge: segments of
/// an interrupted merge are merged only in groups estimated at under half
/// its memory, halving again at each interruption, and are left alone once
/// no group of them fits. Written without syncing: it must survive the
/// process, not the machine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MergeRecord {
    pub running: Vec<MergeEntry>,
    pub interrupted: Vec<MergeEntry>,
}

/// A merge: its input segments and its estimated memory, and for an
/// interrupted one how many times merges of those segments were.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeEntry {
    pub segments: Vec<u64>,
    pub memory: u64,
    #[serde(default)]
    pub times: u32,
}

impl MergeRecord {
    /// The record at `path`: merges that were running when it was last
    /// written are now interrupted ones. A record that cannot be read is
    /// treated as empty.
    pub fn load(path: &Path) -> MergeRecord {
        let Ok(bytes) = std::fs::read(path) else { return MergeRecord::default() };
        let Ok(mut r) = serde_json::from_slice::<MergeRecord>(&bytes) else { return MergeRecord::default() };
        for mut m in std::mem::take(&mut r.running) {
            // Earlier interruptions of the same segments count too.
            let before = r.interrupted.iter().filter(|i| i.segments.iter().any(|s| m.segments.contains(s))).map(|i| i.times).max().unwrap_or(0);
            m.times = before + 1;
            r.interrupted.push(m);
        }
        r
    }

    /// Writes the record (a temporary file renamed over it), or removes the
    /// file when there is nothing to record.
    pub fn store(&self, path: &Path) -> Result<()> {
        if self.running.is_empty() && self.interrupted.is_empty() {
            match std::fs::remove_file(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, serde_json::to_vec(self).expect("record serialises"))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The most memory a merge of `group` may be estimated to need, given
    /// the interrupted merges of any of its segments.
    pub fn cap(&self, group: &[u64]) -> Option<u64> {
        self.interrupted.iter().filter(|i| i.segments.iter().any(|s| group.contains(s))).map(|i| i.memory >> i.times.min(63)).min()
    }

    /// Forgets interrupted merges none of whose segments are left.
    pub fn retain_live(&mut self, live: impl Fn(u64) -> bool) {
        self.interrupted.retain(|i| i.segments.iter().any(|&s| live(s)));
    }
}

impl MergePolicy {
    /// The next group of segments to merge, if any.
    pub fn select(&self, segs: &[SegmentInfo]) -> Option<Vec<u64>> {
        let factor = self.merge_factor.max(2);
        let floor = self.floor_docs.max(1) as f64;
        let tier = |s: &SegmentInfo| ((s.n_live as f64).max(floor) / floor).log(factor as f64).floor() as usize;
        let mut cands: Vec<SegmentInfo> = segs.iter().copied().filter(|s| (s.n_live as usize) <= self.max_merged_docs / 2).collect();
        cands.sort_by_key(|s| (s.n_live, s.id));
        let max_tier = cands.iter().map(tier).max().unwrap_or(0);
        for t in 0..=max_tier {
            let members: Vec<&SegmentInfo> = cands.iter().filter(|s| tier(s) == t).collect();
            if members.len() < factor {
                continue;
            }
            let mut group = Vec::new();
            let mut total = 0usize;
            for s in members.into_iter().take(factor) {
                if total + s.n_live as usize > self.max_merged_docs {
                    break;
                }
                total += s.n_live as usize;
                group.push(s.id);
            }
            if group.len() >= 2 {
                return Some(group);
            }
        }
        let ratio = |s: &SegmentInfo| 1.0 - s.n_live as f64 / s.n_docs.max(1) as f64;
        segs.iter()
            .filter(|s| s.n_docs > 0 && ratio(s) > self.max_deleted_ratio)
            .max_by(|a, b| ratio(a).total_cmp(&ratio(b)))
            .map(|s| vec![s.id])
    }

    /// [`select`](Self::select) among the segments `skip` does not name,
    /// with the selected group cut to its smallest inputs until
    /// [`merge_memory`] (with a build on `threads` threads) estimates it at
    /// most `cap(group)`. A group that
    /// cannot be cut that far (one input, or two that do not fit) is left
    /// alone and the policy selects again without it. Returns the group and
    /// whether it was cut, or whether any group was left alone.
    pub fn select_within(&self, segs: &[SegmentInfo], skip: impl Fn(u64) -> bool, cap: impl Fn(&[u64]) -> Option<u64>, threads: usize) -> (Option<Vec<u64>>, bool) {
        let mut left: Vec<SegmentInfo> = segs.iter().copied().filter(|s| !skip(s.id)).collect();
        let mut limited = false;
        while let Some(mut group) = self.select(&left) {
            let info = |id: &u64| *left.iter().find(|s| s.id == *id).expect("selected from these");
            let fits = |g: &[u64]| cap(g).is_none_or(|c| merge_memory(&g.iter().map(info).collect::<Vec<_>>(), threads) <= c);
            // Smallest inputs first: they are cheapest to keep.
            group.sort_by_key(|id| (info(id).n_live, *id));
            let whole = group.len();
            while !fits(&group) && group.len() > 2 {
                group.pop();
            }
            if fits(&group) {
                return (Some(group.clone()), limited || group.len() < whole);
            }
            limited = true;
            left.retain(|s| !group.contains(&s.id));
        }
        (None, limited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(id: u64, n_docs: u32, n_live: u32) -> SegmentInfo {
        SegmentInfo { id, n_docs, n_live, bytes: n_docs as u64 * 100 }
    }

    #[test]
    fn tiers_and_expunge() {
        let p = MergePolicy { merge_factor: 3, floor_docs: 100, max_merged_docs: 10_000, max_deleted_ratio: 0.5 };
        // Two small segments: not enough for a merge.
        assert_eq!(p.select(&[seg(1, 50, 50), seg(2, 80, 80)]), None);
        // Three tier-0 segments merge, smallest first; the tier-1 one stays.
        let segs = [seg(1, 50, 50), seg(2, 500, 500), seg(3, 80, 80), seg(4, 10, 10), seg(5, 90, 90)];
        assert_eq!(p.select(&segs), Some(vec![4, 1, 3]));
        // Tier 1 (300..900 live) fills up.
        let segs = [seg(1, 400, 400), seg(2, 500, 500), seg(3, 450, 450)];
        assert_eq!(p.select(&segs), Some(vec![1, 3, 2]));
        // Segments too large to pair are left alone even when their tier is full.
        let segs = [seg(1, 6000, 6000), seg(2, 6000, 6000), seg(3, 6000, 6000)];
        assert_eq!(p.select(&segs), None);
        // A mostly deleted segment is rewritten alone; the worst one first.
        let segs = [seg(1, 6000, 1000), seg(2, 6000, 6000), seg(3, 100, 0)];
        assert_eq!(p.select(&segs), Some(vec![3]));
    }

    #[test]
    fn selections_fit_their_memory_cap() {
        let p = MergePolicy { merge_factor: 4, floor_docs: 100, max_merged_docs: 10_000, max_deleted_ratio: 0.5 };
        let segs: Vec<SegmentInfo> = (1..=6).map(|i| seg(i, 50 + i as u32, 50 + i as u32)).collect();
        let one = merge_memory(&segs[..1], 1);
        let none = |_: u64| false;
        // No cap: the policy's group.
        assert_eq!(p.select_within(&segs, none, |_| None, 1), (Some(vec![1, 2, 3, 4]), false));
        // Cut to its smallest inputs.
        assert_eq!(p.select_within(&segs, none, |_| Some(3 * one), 1), (Some(vec![1, 2]), true));
        // Claimed segments are left to their merge.
        assert_eq!(p.select_within(&segs, |id| id <= 2, |_| None, 1), (Some(vec![3, 4, 5, 6]), false));
        // A group that cannot be cut to fit is left alone; the rest are
        // too few for another.
        assert_eq!(p.select_within(&segs, none, |_| Some(one), 1), (None, true));
        // A cap on some segments only (an interrupted merge's): the
        // smallest two are left alone, the next four fit.
        let cap = |g: &[u64]| g.contains(&1).then_some(one);
        assert_eq!(p.select_within(&segs, none, cap, 1), (Some(vec![3, 4, 5, 6]), true));
        // A single-segment rewrite that does not fit is left alone too.
        let gone = [seg(7, 1000, 100)];
        assert_eq!(p.select_within(&gone, none, |_| Some(1), 1), (None, true));
        assert_eq!(p.select_within(&gone, none, |_| None, 1), (Some(vec![7]), false));
    }

    #[test]
    fn merges_past_four_batches_cost_less_per_byte() {
        const MB: u64 = 1 << 20;
        // Short documents, 128 bytes each: a batch is 32 MB of image.
        let segs = |mb: u64| -> Vec<SegmentInfo> { (1..=4).map(|id| SegmentInfo { id, n_docs: (mb * MB / 4 / 128) as u32, n_live: (mb * MB / 4 / 128) as u32, bytes: mb * MB / 4 }).collect() };
        let at = |mb: u64, threads: usize| merge_memory(&segs(mb), threads) as f64;
        for threads in [1, 4, 8] {
            // Up to four batches, each byte costs the full estimate.
            assert!((at(100, threads) - 100.0 * MB as f64 * memory_per_image_byte(threads)).abs() < 1e3);
            // Past them, each byte adds the streamed cost.
            let added = (at(1000, threads) - at(500, threads)) / (500 * MB) as f64;
            assert!((added - memory_per_streamed_byte(threads)).abs() < 1e-6, "{added}");
            assert!(memory_per_streamed_byte(threads) < memory_per_image_byte(threads) / 2.0);
        }
        // Deleted documents' bytes do not count.
        let half: Vec<SegmentInfo> = segs(1000).into_iter().map(|s| SegmentInfo { n_live: s.n_docs / 2, ..s }).collect();
        assert!((merge_memory(&half, 1) as f64 - at(500, 1)).abs() < 1e3);
        // Long documents make a batch of at least 16,384 of them, so four
        // batches of 8 KB documents are 512 MB.
        let long: Vec<SegmentInfo> = (1..=4).map(|id| SegmentInfo { id, n_docs: 32_768, n_live: 32_768, bytes: 32_768 * 8192 }).collect();
        assert!((merge_memory(&long, 1) as f64 - 1024.0 * MB as f64 * memory_per_image_byte(1) + 512.0 * MB as f64 * (memory_per_image_byte(1) - memory_per_streamed_byte(1))).abs() < 1e3);
    }

    #[test]
    fn interrupted_merges_halve_their_cap_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db-merging");
        assert_eq!(MergeRecord::load(&path), MergeRecord::default());
        let entry = |segments: Vec<u64>, memory, times| MergeEntry { segments, memory, times };
        MergeRecord { running: vec![entry(vec![1, 2], 1000, 0)], interrupted: vec![] }.store(&path).unwrap();
        // Running when the process died: interrupted once.
        let r = MergeRecord::load(&path);
        assert_eq!(r, MergeRecord { running: vec![], interrupted: vec![entry(vec![1, 2], 1000, 1)] });
        assert_eq!(r.cap(&[2, 3]), Some(500));
        assert_eq!(r.cap(&[3, 4]), None);
        // Interrupted again, a smaller merge of some of the same segments.
        MergeRecord { running: vec![entry(vec![2, 3], 400, 0)], ..r }.store(&path).unwrap();
        let mut r = MergeRecord::load(&path);
        assert_eq!(r.interrupted, vec![entry(vec![1, 2], 1000, 1), entry(vec![2, 3], 400, 2)]);
        assert_eq!(r.cap(&[2]), Some(100));
        assert_eq!(r.cap(&[1]), Some(500));
        r.retain_live(|id| id == 3);
        assert_eq!(r.interrupted, vec![entry(vec![2, 3], 400, 2)]);
        // Nothing to record: no file.
        MergeRecord::default().store(&path).unwrap();
        assert!(!path.exists());
        // A record that cannot be read is no record.
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(MergeRecord::load(&path), MergeRecord::default());
    }

    #[test]
    fn the_pacer_spreads_writes_over_time() {
        let p = Pacer::new(1 << 20);
        let t0 = Instant::now();
        for _ in 0..5 {
            p.pace(64 << 10);
        }
        // 320 KiB at 1 MiB/s: the first chunk goes at once.
        let took = t0.elapsed().as_secs_f64();
        assert!((0.2..1.0).contains(&took), "{took} s");
    }

    #[test]
    fn the_memory_limit_is_known() {
        let l = memory_limit().expect("RAM or a cgroup limit");
        assert!(l >= 64 << 20, "{l}");
    }
}
