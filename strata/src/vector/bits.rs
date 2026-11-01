//! Binary codes: one bit per dimension.
//!
//! Bit `j` of a code says whether `x_j` is above the segment's mean `t_j` of
//! dimension `j`. The code stands for `x ≈ t + a∘s`, with `s_j = ±1` and
//! `a_j` the mean distance of dimension `j` from `t_j`, so for a query `q`
//!
//! ```text
//! q · x ≈ q·t − Σ q_j·a_j + 2 Σ_{bit j set} q_j·a_j
//! ```
//!
//! and ranking needs only `Σ_{bit j set} w_j` with `w_j = q_j·a_j`. The
//! weights are quantised once per query to int8 `v_j` and kept offset by 128
//! (`u_j = v_j + 128`, a byte), so a code's integer score is
//! `Σ_{set} u_j − 128·popcount(code)`: AVX-512 loads the bytes of the set
//! bits with the code as the load mask and sums them with `vpsadbw`, 64
//! dimensions per step; NEON turns the code's bits into byte masks and sums
//! mask times `v` with `sdot`; elsewhere the eight bit planes of `u` are
//! ANDed with the code and counted (see [`kernels::dot_bits_u8`]).
//!
//! A code row is `⌈dim/64⌉` little-endian u64 words: 1/32 of the float32
//! row and 1/8 of the SQ8 one. The query keeps all its precision (8 bits per
//! weight); only the documents are reduced to signs, so searches over binary
//! codes rescore more candidates with float32 than SQ8 searches do.

use crate::postings::bytes::Slice;

use crate::vector::kernels;

/// Bytes per binary code row of `dim` dimensions.
pub fn stride(dim: usize) -> usize {
    dim.div_ceil(64) * 8
}

/// Per-dimension thresholds and spreads.
#[derive(Clone, Debug)]
pub struct Bits {
    /// The threshold of each dimension (the mean of the segment's vectors).
    pub mean: Slice<f32>,
    /// The mean absolute distance from it.
    pub spread: Slice<f32>,
}

/// A query prepared for binary scoring.
#[derive(Clone, Debug)]
pub struct BitsQuery {
    /// Offset weights `u_j = v_j + 128`, padded to 64 bytes per code word.
    pub u: Vec<u8>,
    /// The weights `v_j` themselves, padded alike.
    pub v: Vec<i8>,
    /// Bit plane `p` of the weights of code word `w` at `planes[8w + p]`.
    pub planes: Vec<u64>,
    pub alpha: f32,
    pub bias: f32,
}

impl BitsQuery {
    /// Integer score of a code row: `Σ_{set} v_j`.
    #[inline]
    pub fn dot(&self, code: &[u8]) -> i32 {
        kernels::dot_bits_u8(code, self.weights())
    }

    /// Integer scores of four code rows.
    #[inline]
    pub fn dot4(&self, codes: [&[u8]; 4]) -> [i32; 4] {
        kernels::dot_bits_u8_x4(codes, self.weights())
    }

    #[inline]
    fn weights(&self) -> kernels::BitsWeights<'_> {
        kernels::BitsWeights { u: &self.u, v: &self.v, planes: &self.planes }
    }

    /// Approximate similarity from an integer score.
    #[inline]
    pub fn score(&self, dot: i32) -> f32 {
        self.bias + 2.0 * self.alpha * dot as f32
    }
}

impl Bits {
    /// Fits the thresholds and spreads to `rows` (each of length `dim`).
    pub fn fit<'a, I: Iterator<Item = &'a [f32]> + Clone>(dim: usize, rows: I) -> Bits {
        let mut sum = vec![0f64; dim];
        let mut n = 0usize;
        for r in rows.clone() {
            for j in 0..dim {
                sum[j] += r[j] as f64;
            }
            n += 1;
        }
        let mean: Vec<f32> = sum.iter().map(|s| if n > 0 { (s / n as f64) as f32 } else { 0.0 }).collect();
        let mut dev = vec![0f64; dim];
        for r in rows {
            for j in 0..dim {
                dev[j] += (r[j] - mean[j]).abs() as f64;
            }
        }
        let spread = dev.iter().map(|d| if n > 0 { (d / n as f64) as f32 } else { 0.0 }).collect();
        Bits { mean: Slice::Owned(mean), spread: Slice::Owned(spread) }
    }

    pub fn dim(&self) -> usize {
        self.mean.len()
    }

    /// The code of `x` into `out` (`stride(dim)` bytes, overwritten).
    pub fn encode(&self, x: &[f32], out: &mut [u8]) {
        out.fill(0);
        for j in 0..self.dim() {
            if x[j] > self.mean[j] {
                out[j / 8] |= 1 << (j % 8);
            }
        }
    }

    /// The vector a code stands for.
    pub fn decode(&self, code: &[u8], out: &mut [f32]) {
        for j in 0..self.dim() {
            let s = if code[j / 8] >> (j % 8) & 1 == 1 { 1.0 } else { -1.0 };
            out[j] = self.mean[j] + s * self.spread[j];
        }
    }

    /// Prepares `q` for scoring code rows of `stride` bytes.
    pub fn prepare(&self, q: &[f32], stride: usize) -> BitsQuery {
        let dim = self.dim();
        let words = stride / 8;
        let mut w = vec![0f32; dim];
        let (mut qt, mut wsum) = (0f64, 0f64);
        for j in 0..dim {
            w[j] = q[j] * self.spread[j];
            qt += q[j] as f64 * self.mean[j] as f64;
            wsum += w[j] as f64;
        }
        let Weights { u, v, planes, alpha } = Weights::quantise(&w, words);
        BitsQuery { u, v, planes, alpha, bias: (qt - wsum) as f32 }
    }
}

/// Weights a bit each for [`kernels::dot_bits_u8`]: `w` quantised to int8
/// `v_j` with `w_j ≈ alpha·v_j`, in the three layouts of
/// [`kernels::BitsWeights`], padded to 64 a code word.
pub(crate) struct Weights {
    pub u: Vec<u8>,
    pub v: Vec<i8>,
    pub planes: Vec<u64>,
    pub alpha: f32,
}

impl Weights {
    pub fn quantise(w: &[f32], words: usize) -> Weights {
        let max = w.iter().fold(0f32, |m, &x| m.max(x.abs()));
        let alpha = max / 127.0;
        let mut u = vec![128u8; words * 64];
        for (j, &x) in w.iter().enumerate() {
            let v = if alpha > 0.0 { (x / alpha).round().clamp(-127.0, 127.0) } else { 0.0 };
            u[j] = (v as i32 + 128) as u8;
        }
        let mut planes = vec![0u64; words * 8];
        for (j, &x) in u.iter().enumerate() {
            for p in 0..8 {
                planes[j / 64 * 8 + p] |= ((x >> p & 1) as u64) << (j % 64);
            }
        }
        let v = u.iter().map(|&x| (x as i32 - 128) as i8).collect();
        Weights { u, v, planes, alpha }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    fn unit(rng: &mut rand::rngs::StdRng, dim: usize) -> Vec<f32> {
        let v: Vec<f32> = (0..dim).map(|j| rng.gen_range(-1.0f32..1.0) + 0.3 * (j % 3) as f32).collect();
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.into_iter().map(|x| x / n).collect()
    }

    #[test]
    fn codes_are_signs_around_the_mean() {
        let rows = [vec![1.0f32, -1.0, 5.0], vec![3.0, 1.0, 5.0]];
        let b = Bits::fit(3, rows.iter().map(|r| r.as_slice()));
        assert_eq!(&b.mean[..], &[2.0, 0.0, 5.0]);
        assert_eq!(&b.spread[..], &[1.0, 1.0, 0.0]);
        let mut c = vec![0u8; stride(3)];
        b.encode(&rows[0], &mut c);
        assert_eq!(c[0], 0b000);
        b.encode(&rows[1], &mut c);
        assert_eq!(c[0], 0b011);
        let mut back = [0f32; 3];
        b.decode(&c, &mut back);
        assert_eq!(back, [3.0, 1.0, 5.0]);
    }

    #[test]
    fn integer_scores_track_the_decoded_dot_product() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(9);
        for dim in [1, 63, 64, 100, 384, 1024] {
            let rows: Vec<Vec<f32>> = (0..300).map(|_| unit(&mut rng, dim)).collect();
            let b = Bits::fit(dim, rows.iter().map(|r| r.as_slice()));
            let q = unit(&mut rng, dim);
            let qc = b.prepare(&q, stride(dim));
            let mut c = vec![0u8; stride(dim)];
            let mut back = vec![0f32; dim];
            for r in &rows {
                b.encode(r, &mut c);
                b.decode(&c, &mut back);
                let want = kernels::dot_f32_scalar(&q, &back);
                let got = qc.score(qc.dot(&c));
                // Only the weights' int8 rounding separates the two.
                let tol = qc.alpha * dim as f32 + 1e-5;
                assert!((want - got).abs() <= tol, "dim {dim}: {want} vs {got} (tolerance {tol})");
                assert_eq!(qc.dot(&c), kernels::dot_bits_u8_scalar(&c, &qc.u), "dim {dim}");
            }
        }
    }
}
