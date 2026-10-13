//! IVF: an inverted file over k-means clusters.
//!
//! **Training** is spherical k-means (similarity is the dot product, so
//! centroids are normalised means and points join the centroid with the
//! largest dot product): random initial centroids drawn from a sample of
//! `sample_per_list · nlist` points, a fixed number of Lloyd iterations,
//! empty clusters re-seeded from a member of the largest one. Assignment uses
//! the 4×4 dot-product kernel over cache-sized tiles, on several threads.
//!
//! **Layout.** Section `vec.<field>.ivf` holds the clusters back to back,
//! each 64-byte aligned:
//!
//! ```text
//! cluster c: docIDs (u32, ascending) | pad to 64 | SQ8 code rows (stride bytes each)
//! ```
//!
//! so probing a cluster is one sequential read, and a filter is applied to
//! the docID list before any code is touched. `vec.<field>.ivf_dir` holds the
//! byte offset and size of every cluster; `vec.<field>.ivf_centroids` the
//! centroids as SQ8 codes with their own ranges. Every query scores all
//! centroids to pick its probes, so they are scored like documents (VNNI on
//! 1 KiB codes): float32 centroids cost ~4× more, the scan being memory-bound
//! once `nlist` × 4 KiB outgrows L2.

use posting_bench::bytes::{ByteReader, ByteWriter, OwnedBytes, Slice};
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};

use crate::error::{corrupt, Result};
use crate::segment::DocId;
use crate::vector::build::Rows;
use crate::vector::sq8::{QueryCode, Sq8};
use crate::vector::topk::{Members, TopK};
use crate::vector::{exact, io, score_rows, SearchStats};

/// Training parameters.
#[derive(Clone, Debug)]
pub(crate) struct IvfConfig {
    pub nlist: usize,
    pub iters: usize,
    /// Training sample size per cluster.
    pub sample_per_list: usize,
    pub threads: usize,
    pub seed: u64,
}

/// A built IVF in its on-disk shape.
pub(crate) struct BuiltIvf {
    pub nlist: usize,
    dim: usize,
    stride: usize,
    code_stride: usize,
    centroids: Vec<f32>,
    pub offsets: Vec<u64>,
    pub sizes: Vec<u32>,
    pub lists: Vec<u8>,
    pub train_points: usize,
}

impl BuiltIvf {
    pub fn dir_bytes(&self) -> Vec<u8> {
        let mut w = ByteWriter::new();
        w.array(&self.offsets);
        w.array(&self.sizes);
        w.finish()
    }

    /// The `ivf_centroids` section: SQ8 ranges, then one code row per centroid.
    pub fn centroid_bytes(&self) -> Vec<u8> {
        let rows: Vec<&[f32]> = self.centroids.chunks_exact(self.stride).map(|c| &c[..self.dim]).collect();
        let sq = Sq8::fit(self.dim, rows.iter().copied());
        let mut codes = vec![0u8; self.nlist * self.code_stride];
        for (c, r) in codes.chunks_exact_mut(self.code_stride).zip(&rows) {
            sq.encode(r, c);
        }
        let mut w = ByteWriter::new();
        w.array(&sq.min);
        w.array(&sq.scale);
        w.array(&codes);
        w.finish()
    }
}

/// Default number of lists: about 4·√n.
pub fn default_nlist(n: usize) -> usize {
    ((4.0 * (n as f64).sqrt()).round() as usize).max(1)
}

fn normalise(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

/// For every point, the centroid with the largest dot product.
fn assign(rows: &Rows, points: &[u32], centroids: &[f32], threads: usize) -> Vec<u32> {
    let cents: Vec<&[f32]> = centroids.chunks_exact(rows.stride).collect();
    let pts: Vec<&[f32]> = points.iter().map(|&p| rows.row(p as usize)).collect();
    let mut out = vec![0u32; points.len()];
    let per = points.len().div_ceil(threads.max(1)).max(1);
    std::thread::scope(|s| {
        for (chunk, res) in pts.chunks(per).zip(out.chunks_mut(per)) {
            let cents = &cents;
            s.spawn(move || {
                let mut best = vec![(f32::NEG_INFINITY, 0u32); chunk.len()];
                exact::block_dots(chunk, cents, 64, |i, j, d| {
                    if d > best[i].0 {
                        best[i] = (d, j as u32);
                    }
                });
                for (r, b) in res.iter_mut().zip(best) {
                    *r = b.1;
                }
            });
        }
    });
    out
}

/// Trains the centroids and lays out the clusters. `codes` are the forward
/// SQ8 rows (`code_stride` bytes each), copied into cluster order.
pub(crate) fn build(rows: &Rows, ids: &[u32], codes: &[u8], code_stride: usize, cfg: &IvfConfig) -> BuiltIvf {
    let mut rng = rand::rngs::StdRng::seed_from_u64(cfg.seed);
    let nlist = cfg.nlist.clamp(1, ids.len().max(1));
    let stride = rows.stride;
    let mut centroids = vec![0f32; nlist * stride];
    let mut sizes = vec![0u32; nlist];
    let mut train_points = 0;
    if !ids.is_empty() {
        let want = (nlist * cfg.sample_per_list.max(1)).min(ids.len());
        let sample: Vec<u32> = if want < ids.len() {
            let mut s: Vec<u32> = ids.choose_multiple(&mut rng, want).copied().collect();
            s.sort_unstable();
            s
        } else {
            ids.to_vec()
        };
        train_points = sample.len();
        for (c, &p) in sample.choose_multiple(&mut rng, nlist).enumerate() {
            centroids[c * stride..(c + 1) * stride].copy_from_slice(rows.row(p as usize));
            normalise(&mut centroids[c * stride..(c + 1) * stride]);
        }
        for _ in 0..cfg.iters {
            let a = assign(rows, &sample, &centroids, cfg.threads);
            let mut sums = vec![0f32; nlist * stride];
            let mut counts = vec![0usize; nlist];
            for (&p, &c) in sample.iter().zip(&a) {
                counts[c as usize] += 1;
                for (s, x) in sums[c as usize * stride..(c as usize + 1) * stride].iter_mut().zip(rows.row(p as usize)) {
                    *s += x;
                }
            }
            for c in 0..nlist {
                if counts[c] == 0 {
                    // Re-seed from a random member of the largest cluster.
                    let big = (0..nlist).max_by_key(|&j| counts[j]).unwrap() as u32;
                    let members: Vec<u32> = sample.iter().zip(&a).filter(|(_, &c)| c == big).map(|(&p, _)| p).collect();
                    let p = members[rng.gen_range(0..members.len())];
                    sums[c * stride..(c + 1) * stride].copy_from_slice(rows.row(p as usize));
                }
                normalise(&mut sums[c * stride..(c + 1) * stride]);
            }
            centroids = sums;
        }
    }
    let a = assign(rows, ids, &centroids, cfg.threads);
    let mut members: Vec<Vec<u32>> = vec![Vec::new(); nlist];
    for (&d, &c) in ids.iter().zip(&a) {
        members[c as usize].push(d);
    }
    let mut offsets = Vec::with_capacity(nlist + 1);
    let mut off = 0u64;
    for (c, m) in members.iter().enumerate() {
        offsets.push(off);
        sizes[c] = m.len() as u32;
        off += ((4 * m.len()).next_multiple_of(64) + m.len() * code_stride).next_multiple_of(64) as u64;
    }
    offsets.push(off);
    let mut lists = vec![0u8; off as usize];
    for (c, m) in members.iter().enumerate() {
        let base = offsets[c] as usize;
        for (i, &d) in m.iter().enumerate() {
            lists[base + 4 * i..base + 4 * i + 4].copy_from_slice(&d.to_le_bytes());
        }
        let cb = base + (4 * m.len()).next_multiple_of(64);
        for (i, &d) in m.iter().enumerate() {
            let src = &codes[d as usize * code_stride..(d as usize + 1) * code_stride];
            lists[cb + i * code_stride..cb + (i + 1) * code_stride].copy_from_slice(src);
        }
    }
    BuiltIvf { nlist, dim: rows.dim, stride, code_stride, centroids, offsets, sizes, lists, train_points }
}

/// How much a probed cluster counts towards `nprobe` in a filtered search:
/// `min(1, f/√s)`, where `f` is the fraction of its documents in the filter
/// and `s` the filter's selectivity.
///
/// The filtered top-k of a filter with selectivity `s` sits around rank k/s
/// of the unfiltered order, spread over more clusters than the unfiltered
/// top-k, so a filtered search must probe further. For a filter independent
/// of the query every cluster holds `f ≈ s`, so probing stops after
/// `nprobe/√s` clusters (see [`filtered_probes`]), which keeps recall near
/// the unfiltered level. When the members come in whole clusters (a filter
/// correlated with the embedding, such as a topic), clusters without members
/// count nothing and full ones count 1: the search probes about `nprobe`
/// clusters of members, however far down the centroid order they start
/// (see `notes/vector.md`, filtered study).
pub fn filtered_probe_weight(members: usize, size: usize, selectivity: f64) -> f64 {
    if size == 0 {
        return 0.0;
    }
    (members as f64 / size as f64 / selectivity.clamp(1e-12, 1.0).sqrt()).min(1.0)
}

/// Clusters a filtered probe visits when the filter is independent of the
/// query: `nprobe/√s`, at most `nlist`.
pub fn filtered_probes(nprobe: usize, selectivity: f64, nlist: usize) -> usize {
    let s = selectivity.clamp(1e-9, 1.0);
    ((nprobe as f64 / s.sqrt()).ceil() as usize).clamp(nprobe.min(nlist), nlist)
}

/// Zero-copy view of a stored IVF.
pub(crate) struct IvfView {
    nlist: usize,
    code_stride: usize,
    centroid_sq8: Sq8,
    centroids: Slice<u8>,
    offsets: Slice<u64>,
    sizes: Slice<u32>,
    /// The lists section, viewed as bytes (codes) and as u32 (docIDs).
    bytes: Slice<u8>,
    words: Slice<u32>,
}

impl IvfView {
    pub fn open(nlist: usize, dim: usize, code_stride: usize, centroids: OwnedBytes, dir: OwnedBytes, lists: OwnedBytes) -> Result<Self> {
        let mut r = ByteReader::new(centroids);
        let centroid_sq8 = Sq8 { min: r.array().map_err(corrupt)?, scale: r.array().map_err(corrupt)? };
        let centroids: Slice<u8> = r.array().map_err(corrupt)?;
        let mut r = ByteReader::new(dir);
        let offsets: Slice<u64> = r.array().map_err(corrupt)?;
        let sizes: Slice<u32> = r.array().map_err(corrupt)?;
        if centroid_sq8.dim() != dim || centroids.len() != nlist * code_stride || offsets.len() != nlist + 1 || sizes.len() != nlist {
            return Err(corrupt("ivf directory does not match nlist"));
        }
        for c in 0..nlist {
            let need = (4 * sizes[c] as u64).next_multiple_of(64) + sizes[c] as u64 * code_stride as u64;
            if !offsets[c].is_multiple_of(64) || offsets[c] + need > offsets[c + 1] {
                return Err(corrupt("ivf cluster out of place"));
            }
        }
        if offsets[nlist] as usize != lists.len() {
            return Err(corrupt("ivf lists have the wrong size"));
        }
        let words: Slice<u32> = Slice::view(lists.clone()).map_err(corrupt)?;
        let bytes: Slice<u8> = Slice::view(lists).map_err(corrupt)?;
        Ok(IvfView { nlist, code_stride, centroid_sq8, centroids, offsets, sizes, bytes, words })
    }

    pub fn is_shared(&self) -> bool {
        [matches!(self.centroids, Slice::Shared { .. }), matches!(self.centroid_sq8.min, Slice::Shared { .. }), matches!(self.offsets, Slice::Shared { .. }), matches!(self.bytes, Slice::Shared { .. })]
            .iter()
            .all(|&x| x)
    }

    /// Cluster `c`: its docIDs and its code rows.
    pub fn cluster(&self, c: usize) -> (&[DocId], &[u8]) {
        let (o, n) = (self.offsets[c] as usize, self.sizes[c] as usize);
        let cb = o + (4 * n).next_multiple_of(64);
        (&self.words[o / 4..o / 4 + n], &self.bytes[cb..cb + n * self.code_stride])
    }

    /// The best `r` codes by SQ8 score among the probed clusters, best first.
    ///
    /// Clusters are probed in decreasing centroid similarity to `qf` (SQ8
    /// scores): unfiltered, the `nprobe` closest; filtered, each cluster's
    /// docIDs are intersected with the filter before any code is scored, and
    /// probing goes on until the clusters' [`filtered_probe_weight`]s add up
    /// to `nprobe`.
    /// Either way probing goes on until `r` codes were scored, so a result is
    /// short only when the filter has fewer than `r` members.
    ///
    /// With `hints`, the centroids and the clusters expected to be probed are
    /// announced to the kernel first (filtered: their docIDs, then each
    /// cluster's matching rows), so cold reads overlap.
    #[allow(clippy::too_many_arguments)]
    pub fn search(&self, q: &QueryCode, qf: &[f32], r: usize, nprobe: usize, filter: Option<(Members, f64)>, hints: bool, stats: &mut SearchStats) -> Vec<(i32, DocId)> {
        let nprobe = nprobe.clamp(1, self.nlist);
        let expect = match &filter {
            None => nprobe,
            Some((_, s)) => filtered_probes(nprobe, *s, self.nlist),
        };
        if hints {
            io::will_need_range(self.centroids.as_ptr(), self.centroids.len());
        }
        let cq = self.centroid_sq8.prepare(qf, self.code_stride);
        let mut order: Vec<(i32, u32)> = Vec::with_capacity(self.nlist);
        score_rows(&self.centroids, self.code_stride, self.nlist, None, &cq, |c, s| order.push((s, c as u32)));
        stats.centroids += self.nlist;
        let cmp = |a: &(i32, u32), b: &(i32, u32)| b.0.cmp(&a.0).then(a.1.cmp(&b.1));
        // Sort only the clusters expected to be probed; the rest on demand.
        let mut sorted = expect.min(self.nlist);
        if sorted < self.nlist {
            order.select_nth_unstable_by(sorted, cmp);
        }
        order[..sorted].sort_unstable_by(cmp);
        if hints {
            let filtered = filter.is_some();
            io::will_need(
                &mut order[..sorted]
                    .iter()
                    .map(|&(_, c)| {
                        let (ids, codes) = self.cluster(c as usize);
                        let end = if filtered { ids.as_ptr_range().end as usize } else { codes.as_ptr_range().end as usize };
                        (ids.as_ptr() as usize, end - ids.as_ptr() as usize)
                    })
                    .collect::<Vec<_>>(),
            );
        }
        let mut top = TopK::new(r);
        let mut pos: Vec<u32> = Vec::new();
        let mut scored = 0usize;
        // Probes so far; a filtered probe counts by its members.
        let mut probes = 0f64;
        for i in 0..self.nlist {
            if probes >= nprobe as f64 && scored >= r {
                break;
            }
            if i == sorted {
                order[sorted..].sort_unstable_by(cmp);
                sorted = self.nlist;
            }
            let (ids, codes) = self.cluster(order[i].1 as usize);
            stats.clusters_probed += 1;
            match &filter {
                None => {
                    score_rows(codes, self.code_stride, ids.len(), None, q, |j, s| top.push(s, ids[j]));
                    scored += ids.len();
                    probes += 1.0;
                }
                Some((m, sel)) => {
                    pos.clear();
                    m.select(ids, &mut pos);
                    stats.filter_checks += ids.len();
                    if hints {
                        let base = codes.as_ptr() as usize;
                        io::will_need(&mut pos.iter().map(|&p| (base + p as usize * self.code_stride, self.code_stride)).collect::<Vec<_>>());
                    }
                    score_rows(codes, self.code_stride, pos.len(), Some(&pos), q, |j, s| top.push(s, ids[pos[j] as usize]));
                    scored += pos.len();
                    probes += filtered_probe_weight(pos.len(), ids.len(), *sel);
                }
            }
        }
        stats.sq8_distances += scored;
        if filter.is_none() {
            stats.sq8_contiguous += scored;
        }
        top.into_sorted()
    }
}
