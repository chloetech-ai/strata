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

use serde::Serialize;

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
    pub bytes_written: u64,
    pub seconds: f64,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(id: u64, n_docs: u32, n_live: u32) -> SegmentInfo {
        SegmentInfo { id, n_docs, n_live }
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
}
