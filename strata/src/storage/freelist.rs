//! Free page runs and deferred frees.
//!
//! Pages that a commit stops referencing are *pending*, tagged with the txn
//! of that commit, until two conditions hold:
//!
//! * no live snapshot has a txn below the tag (it might still read them), and
//! * the commit that freed them is no longer the newest on disk: until the
//!   next commit overwrites the other meta slot, that slot still names the
//!   previous version, which references them. Waiting one commit keeps every
//!   page reachable from *either* valid slot intact, so falling back to the
//!   older slot (when the newer one is damaged) always finds a whole version.
//!
//! Released pages join the free runs, which are coalesced and allocated
//! best-fit (smallest run that fits, lowest address first); a request that
//! fits nowhere extends the file. Free runs at the end of the file are
//! trimmed off so the file shrinks.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{corrupt, Result};
use crate::storage::pager::{Extent, META_PAGES};

/// Pages freed by commit `freed_at`, not yet reusable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    pub freed_at: u64,
    pub page: u64,
    pub pages: u64,
}

/// Free runs and pending frees of one version of the file.
#[derive(Clone, Debug, PartialEq)]
pub struct FreeList {
    /// start -> length, disjoint and coalesced.
    runs: BTreeMap<u64, u64>,
    /// (length, start) of every run, for best-fit search.
    by_size: BTreeSet<(u64, u64)>,
    pending: Vec<Pending>,
    /// End of the allocated area (the file length) in pages.
    page_count: u64,
}

impl FreeList {
    /// A file of `page_count` pages with nothing free.
    pub fn new(page_count: u64) -> Self {
        FreeList { runs: BTreeMap::new(), by_size: BTreeSet::new(), pending: Vec::new(), page_count }
    }

    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Allocates `pages` contiguous pages, returning the first.
    pub fn alloc(&mut self, pages: u64) -> u64 {
        debug_assert!(pages > 0);
        if let Some(&(len, start)) = self.by_size.range((pages, 0)..).next() {
            self.take(start, len, pages);
            return start;
        }
        let start = self.page_count;
        self.page_count += pages;
        start
    }

    /// Best-fit allocation restricted to pages below `limit` (to move an
    /// extent towards the start of the file); `None` if nothing fits.
    pub fn alloc_below(&mut self, pages: u64, limit: u64) -> Option<u64> {
        let &(len, start) = self.by_size.range((pages, 0)..).find(|&&(_, s)| s + pages <= limit)?;
        self.take(start, len, pages);
        Some(start)
    }

    /// Takes `pages` from the front of run `start..start + len`.
    fn take(&mut self, start: u64, len: u64, pages: u64) {
        self.remove_run(start, len);
        if len > pages {
            self.insert_run(start + pages, len - pages);
        }
    }

    /// Returns pages to the free runs immediately (only for pages that no
    /// meta slot or snapshot references, e.g. a failed commit's extents).
    pub fn free(&mut self, page: u64, pages: u64) {
        let (mut start, mut len) = (page, pages);
        if let Some((&s, &l)) = self.runs.range(..start).next_back() {
            debug_assert!(s + l <= start, "double free of page {start}");
            if s + l == start {
                self.remove_run(s, l);
                start = s;
                len += l;
            }
        }
        if let Some(&l) = self.runs.get(&(page + pages)) {
            self.remove_run(page + pages, l);
            len += l;
        }
        self.insert_run(start, len);
    }

    /// Frees an extent as of commit `txn` (pending until released).
    pub fn defer(&mut self, txn: u64, e: Extent) {
        if e.pages() > 0 {
            self.pending.push(Pending { freed_at: txn, page: e.page, pages: e.pages() });
        }
    }

    /// Makes pending pages with `freed_at <= limit` reusable.
    pub fn release(&mut self, limit: u64) {
        let (ready, keep): (Vec<Pending>, Vec<Pending>) = self.pending.iter().partition(|p| p.freed_at <= limit);
        self.pending = keep;
        for p in ready {
            self.free(p.page, p.pages);
        }
    }

    /// Drops a free run that ends the file, shrinking it.
    pub fn trim_tail(&mut self) {
        if let Some((&s, &l)) = self.runs.iter().next_back() {
            if s + l == self.page_count {
                self.remove_run(s, l);
                self.page_count = s;
            }
        }
    }

    pub fn free_pages(&self) -> u64 {
        self.runs.values().sum()
    }

    pub fn pending_pages(&self) -> u64 {
        self.pending.iter().map(|p| p.pages).sum()
    }

    /// Free runs, in address order.
    pub fn runs(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.runs.iter().map(|(&s, &l)| (s, l))
    }

    pub fn pending(&self) -> &[Pending] {
        &self.pending
    }

    /// Exact size of [`encode`](Self::encode)'s output. Allocating never adds
    /// a run, so the size measured before allocating the freelist's own
    /// extent bounds the size after.
    pub fn encoded_len(&self) -> usize {
        16 + 16 * self.runs.len() + 24 * self.pending.len()
    }

    /// `n_runs | n_pending | (start, len)* | (freed_at, page, pages)*`, u64 LE.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(self.encoded_len());
        let mut put = |v: u64| b.extend_from_slice(&v.to_le_bytes());
        put(self.runs.len() as u64);
        put(self.pending.len() as u64);
        for (&s, &l) in &self.runs {
            put(s);
            put(l);
        }
        for p in &self.pending {
            put(p.freed_at);
            put(p.page);
            put(p.pages);
        }
        b
    }

    /// Decodes [`encode`](Self::encode)'s output; trailing bytes (padding to
    /// the extent's reserved size) are ignored.
    pub fn decode(b: &[u8], page_count: u64) -> Result<FreeList> {
        let words: Vec<u64> = b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
        let bad = || corrupt("malformed freelist");
        let (nr, np) = (*words.first().ok_or_else(bad)? as usize, *words.get(1).ok_or_else(bad)? as usize);
        if words.len() < 2 + 2 * nr + 3 * np {
            return Err(bad());
        }
        let mut fl = FreeList::new(page_count);
        let mut end = META_PAGES;
        for r in words[2..2 + 2 * nr].chunks_exact(2) {
            if r[0] < end || r[1] == 0 || r[0] + r[1] > page_count {
                return Err(bad());
            }
            end = r[0] + r[1];
            fl.insert_run(r[0], r[1]);
        }
        for p in words[2 + 2 * nr..2 + 2 * nr + 3 * np].chunks_exact(3) {
            if p[1] < META_PAGES || p[1] + p[2] > page_count {
                return Err(bad());
            }
            fl.pending.push(Pending { freed_at: p[0], page: p[1], pages: p[2] });
        }
        Ok(fl)
    }

    fn insert_run(&mut self, start: u64, len: u64) {
        self.runs.insert(start, len);
        self.by_size.insert((len, start));
    }

    fn remove_run(&mut self, start: u64, len: u64) {
        self.runs.remove(&start);
        self.by_size.remove(&(len, start));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    #[test]
    fn best_fit_split_coalesce_trim() {
        let mut fl = FreeList::new(2);
        let a = fl.alloc(4);
        let b = fl.alloc(1);
        let c = fl.alloc(8);
        let d = fl.alloc(2);
        assert_eq!((a, b, c, d), (2, 6, 7, 15));
        fl.free(a, 4);
        fl.free(c, 8);
        // Best fit: a 3-page request takes the 4-page hole, not the 8-page one.
        assert_eq!(fl.alloc(3), 2);
        assert_eq!(fl.alloc(1), 5);
        // Freeing b joins the holes on both sides: 6..15 is one run.
        fl.free(b, 1);
        assert_eq!(fl.runs().collect::<Vec<_>>(), vec![(6, 9)]);
        fl.free(d, 2);
        fl.trim_tail();
        assert_eq!(fl.page_count(), 6);
        assert_eq!(fl.free_pages(), 0);
        // Only runs ending at or below the limit qualify.
        let e = fl.alloc(10);
        fl.free(2, 3);
        assert_eq!(fl.alloc_below(4, e), None);
        assert_eq!(fl.alloc_below(2, 3), None);
        assert_eq!(fl.alloc_below(2, 4), Some(2));
    }

    #[test]
    fn pending_released_by_limit_and_roundtrip() {
        let mut fl = FreeList::new(2);
        let a = fl.alloc(3);
        let b = fl.alloc(2);
        let _c = fl.alloc(1);
        fl.defer(5, Extent { page: a, len: 3 * 4096 });
        fl.defer(6, Extent { page: b, len: 1 });
        let back = FreeList::decode(&fl.encode(), fl.page_count()).unwrap();
        assert_eq!(back, fl);
        assert_eq!(fl.encoded_len(), fl.encode().len());
        fl.release(4);
        assert_eq!(fl.pending_pages(), 4);
        fl.release(5);
        assert_eq!((fl.free_pages(), fl.pending_pages()), (3, 1));
        fl.release(6);
        assert_eq!(fl.runs().collect::<Vec<_>>(), vec![(2, 4)]);
    }

    /// Random allocations and frees against a page bitmap: no page is ever
    /// handed out twice, and free + allocated always covers the file.
    #[test]
    fn random_against_bitmap() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        let mut fl = FreeList::new(META_PAGES);
        let mut used: Vec<bool> = vec![true; META_PAGES as usize];
        let mut live: Vec<(u64, u64)> = Vec::new();
        for step in 0..20_000 {
            if live.is_empty() || rng.gen_bool(0.55) {
                let n = if rng.gen_bool(0.9) { rng.gen_range(1..4) } else { rng.gen_range(4..64) };
                let p = fl.alloc(n);
                used.resize(fl.page_count() as usize, false);
                for i in p..p + n {
                    assert!(!used[i as usize], "page {i} allocated twice at step {step}");
                    used[i as usize] = true;
                }
                live.push((p, n));
            } else {
                let (p, n) = live.swap_remove(rng.gen_range(0..live.len()));
                for i in p..p + n {
                    used[i as usize] = false;
                }
                fl.free(p, n);
            }
            if step % 97 == 0 {
                fl.trim_tail();
                used.truncate(fl.page_count() as usize);
            }
            let allocated = used.iter().filter(|&&u| u).count() as u64;
            assert_eq!(allocated + fl.free_pages(), fl.page_count());
        }
        let back = FreeList::decode(&fl.encode(), fl.page_count()).unwrap();
        assert_eq!(back, fl);
    }
}
