//! MUVERA's fixed dimensional encodings (Dhulipala et al., NeurIPS 2024): a
//! multi-vector row as one vector whose dot product with a query's encoding
//! approximates `maxsim`, so one single-vector search finds a multi-vector
//! field's candidate rows.
//!
//! Each of `reps` repetitions cuts the space with `ksim` random hyperplanes
//! into `2^ksim` buckets (SimHash) and projects the vectors to `dproj`
//! dimensions with a random ±1 matrix (when `dproj` is less than the
//! vectors' dimensions). A row's encoding holds, for each repetition and
//! bucket, the mean of its projected vectors in that bucket; a bucket none
//! of them falls in takes the vector whose bucket differs from it in the
//! fewest hyperplanes. A query's encoding holds the sum of its projected
//! vectors in each bucket. A query vector meets, in each repetition, the
//! mean of the row's vectors on its side of every hyperplane, which stands
//! for its nearest one; the dot product of the encodings sums those
//! matches over the query vectors and averages them over the repetitions.
//!
//! The encoding has `reps · 2^ksim · dproj` dimensions. The random
//! matrices come from the seed by a fixed generator (splitmix64), so every
//! build and every query of a segment draws the same ones: the parameters
//! are kept with the segment's encodings ([`FdeParams`]).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::vector::kernels::dot_f32;

/// The parameters of an encoding: kept beside a segment's encodings, so a
/// query encodes itself as the segment's rows were.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FdeParams {
    /// Repetitions.
    pub reps: usize,
    /// Hyperplanes of each repetition (`2^ksim` buckets).
    pub ksim: usize,
    /// Dimensions each vector is projected to, in each bucket (the vectors'
    /// own when it is at least theirs).
    pub dproj: usize,
    pub seed: u64,
}

impl FdeParams {
    /// The defaults: 8 repetitions of 16 buckets (4 hyperplanes) at 16
    /// dimensions, 2,048 dimensions in all.
    pub const DEFAULT: FdeParams = FdeParams { reps: 8, ksim: 4, dproj: 16, seed: 0x6D75_7665_7261 };
}

/// The random matrices of [`FdeParams`] for vectors of `dim` dimensions.
#[derive(Clone, Debug)]
pub struct Fde {
    pub params: FdeParams,
    dim: usize,
    /// Projected dimensions per bucket.
    dp: usize,
    /// Hyperplane `h` of repetition `r`: `planes[(r·ksim + h)·dim..][..dim]`.
    planes: Vec<f32>,
    /// Row `j` of repetition `r`'s projection: `proj[(r·dp + j)·dim..][..dim]`;
    /// empty when the vectors are not projected.
    proj: Vec<f32>,
}

/// splitmix64: a fixed generator, the same everywhere.
struct Mix(u64);

impl Mix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Close to a standard normal: the sum of twelve uniforms less six, in
    /// arithmetic every machine does alike.
    fn normal(&mut self) -> f32 {
        let mut s = 0f64;
        for _ in 0..12 {
            s += (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        }
        (s - 6.0) as f32
    }
}

impl Fde {
    pub fn new(dim: usize, params: FdeParams) -> Fde {
        let mut g = Mix(params.seed);
        let planes: Vec<f32> = (0..params.reps * params.ksim * dim).map(|_| g.normal()).collect();
        let (dp, proj) = if params.dproj < dim {
            let s = 1.0 / (params.dproj as f32).sqrt();
            (params.dproj, (0..params.reps * params.dproj * dim).map(|_| if g.next() >> 63 == 1 { s } else { -s }).collect())
        } else {
            (dim, Vec::new())
        };
        Fde { params, dim, dp, planes, proj }
    }

    /// The encoding's dimensions.
    pub fn dims(&self) -> usize {
        self.params.reps * (1 << self.params.ksim) * self.dp
    }

    /// `v`'s bucket in repetition `r`: bit `h` set when it is on the
    /// positive side of hyperplane `h`.
    fn bucket(&self, r: usize, v: &[f32]) -> usize {
        let k = self.params.ksim;
        (0..k).fold(0, |b, h| b | ((dot_f32(&self.planes[(r * k + h) * self.dim..][..self.dim], v) > 0.0) as usize) << h)
    }

    /// `v` projected for repetition `r`, added to `out` times `w`.
    fn add_projected(&self, r: usize, v: &[f32], w: f32, out: &mut [f32]) {
        if self.proj.is_empty() {
            for (o, x) in out.iter_mut().zip(v) {
                *o += w * x;
            }
            return;
        }
        for (j, o) in out.iter_mut().enumerate() {
            *o += w * dot_f32(&self.proj[(r * self.dp + j) * self.dim..][..self.dim], v);
        }
    }

    /// A row's encoding: the mean of its vectors in each bucket. A row
    /// without vectors encodes as zeros.
    pub fn row<'v>(&self, vectors: impl Iterator<Item = &'v [f32]> + Clone) -> Vec<f32> {
        let (b_count, dp) = (1usize << self.params.ksim, self.dp);
        let mut out = vec![0f32; self.dims()];
        let vs: Vec<&[f32]> = vectors.collect();
        if vs.is_empty() {
            return out;
        }
        let mut count = vec![0u32; b_count];
        let mut buckets = vec![0usize; vs.len()];
        for r in 0..self.params.reps {
            count.iter_mut().for_each(|c| *c = 0);
            let base = r * b_count * dp;
            for (i, v) in vs.iter().enumerate() {
                let b = self.bucket(r, v);
                buckets[i] = b;
                count[b] += 1;
                self.add_projected(r, v, 1.0, &mut out[base + b * dp..base + (b + 1) * dp]);
            }
            for b in 0..b_count {
                let cell = &mut out[base + b * dp..base + (b + 1) * dp];
                if count[b] > 0 {
                    let inv = 1.0 / count[b] as f32;
                    cell.iter_mut().for_each(|x| *x *= inv);
                    continue;
                }
                // An empty bucket takes the vector nearest it in hyperplanes
                // (the first of those).
                let near = (0..vs.len()).min_by_key(|&i| ((buckets[i] ^ b).count_ones(), i)).expect("a vector");
                self.add_projected(r, vs[near], 1.0, cell);
            }
        }
        out
    }

    /// A query's encoding: the sum of its vectors in each bucket.
    pub fn query(&self, vectors: &[Arc<[f32]>]) -> Vec<f32> {
        let (b_count, dp) = (1usize << self.params.ksim, self.dp);
        let mut out = vec![0f32; self.dims()];
        for r in 0..self.params.reps {
            let base = r * b_count * dp;
            for v in vectors {
                let b = self.bucket(r, v);
                self.add_projected(r, v, 1.0, &mut out[base + b * dp..base + (b + 1) * dp]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    fn unit(rng: &mut impl Rng, dim: usize) -> Arc<[f32]> {
        let v: Vec<f32> = (0..dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
        let l = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / l).collect::<Vec<f32>>().into()
    }

    fn maxsim(q: &[Arc<[f32]>], row: &[Arc<[f32]>]) -> f32 {
        q.iter().map(|x| row.iter().map(|v| dot_f32(x, v)).fold(f32::MIN, f32::max)).sum()
    }

    #[test]
    fn the_encodings_are_fixed_by_their_parameters() {
        let p = FdeParams { reps: 3, ksim: 2, dproj: 4, seed: 9 };
        let (a, b) = (Fde::new(8, p), Fde::new(8, p));
        assert_eq!(a.planes, b.planes);
        assert_eq!(a.proj, b.proj);
        assert_eq!(a.dims(), 3 * 4 * 4);
        assert_ne!(Fde::new(8, FdeParams { seed: 10, ..p }).planes, a.planes);
        // The first numbers of the generator, pinned: encodings written
        // today must be read the same by every later version.
        let mut g = Mix(0);
        assert_eq!([g.next(), g.next()], [0xE220_A839_7B1D_CDAF, 0x6E78_9E6A_A1B9_65F4]);
        assert_eq!(Fde::new(16, FdeParams { dproj: 32, ..p }).dims(), 3 * 4 * 16, "no projection to more dimensions");
    }

    #[test]
    fn a_row_in_one_bucket_is_its_mean_and_empty_buckets_are_filled() {
        let p = FdeParams { reps: 2, ksim: 3, dproj: 64, seed: 1 };
        let f = Fde::new(4, p);
        let row: Vec<Arc<[f32]>> = vec![vec![1.0, 0.0, 0.0, 0.0].into(), vec![1.0, 0.0, 0.0, 0.0].into()];
        let e = f.row(row.iter().map(|v| &v[..]));
        // Every bucket holds the vector: its own bucket the mean of the two
        // equal vectors, the others the nearest (the same vector).
        for cell in e.chunks(4) {
            assert_eq!(cell, &[1.0, 0.0, 0.0, 0.0]);
        }
        assert!(f.row(std::iter::empty()).iter().all(|&x| x == 0.0));
        // A query of one vector holds it in one bucket a repetition.
        let q = f.query(&row[..1]);
        assert_eq!(q.iter().filter(|&&x| x != 0.0).count(), 2);
        assert!((dot_f32(&q, &e) - 2.0 * maxsim(&row[..1], &row)).abs() < 1e-5, "reps times maxsim");
    }

    #[test]
    fn the_encodings_rank_rows_like_maxsim() {
        // Rows of 32 vectors around 4 topics each; queries of 8 vectors
        // near one row's: the encodings' dot products follow maxsim closely
        // enough that its best row is among their first few.
        let mut rng = rand::rngs::StdRng::seed_from_u64(2);
        let dim = 32;
        let topics: Vec<Arc<[f32]>> = (0..64).map(|_| unit(&mut rng, dim)).collect();
        let near = |rng: &mut rand::rngs::StdRng, c: &[f32], noise: f32| -> Arc<[f32]> {
            let v: Vec<f32> = c.iter().map(|x| x + rng.gen_range(-noise..noise)).collect();
            let l = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.into_iter().map(|x| x / l).collect::<Vec<f32>>().into()
        };
        let rows: Vec<Vec<Arc<[f32]>>> = (0..300)
            .map(|_| {
                let ts: Vec<usize> = (0..4).map(|_| rng.gen_range(0..topics.len())).collect();
                (0..32).map(|i| near(&mut rng, &topics[ts[i % 4]], 0.3)).collect()
            })
            .collect();
        let f = Fde::new(dim, FdeParams::DEFAULT);
        let enc: Vec<Vec<f32>> = rows.iter().map(|r| f.row(r.iter().map(|v| &v[..]))).collect();
        let mut found = 0;
        for t in 0..50 {
            let target = &rows[t * 5];
            let q: Vec<Arc<[f32]>> = (0..8).map(|i| near(&mut rng, &target[i * 4], 0.2)).collect();
            let best = (0..rows.len()).max_by(|&a, &b| maxsim(&q, &rows[a]).total_cmp(&maxsim(&q, &rows[b]))).unwrap();
            let qe = f.query(&q);
            let mut by: Vec<usize> = (0..rows.len()).collect();
            by.sort_by(|&a, &b| dot_f32(&qe, &enc[b]).total_cmp(&dot_f32(&qe, &enc[a])));
            found += by[..10].contains(&best) as usize;
        }
        assert!(found >= 45, "maxsim's best row in the encodings' top 10 for {found} of 50 queries");
    }
}
