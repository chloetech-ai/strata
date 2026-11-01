//! RaBitQ codes: 1, 2 or 4 bits per dimension and a correction factor per
//! row (Gao and Long, SIGMOD 2024; Gao et al., SIGMOD 2025, for more than
//! one bit).
//!
//! A row `x` is centred on the segment's mean `c` and rotated by a fixed
//! random orthogonal transform `P`, and its direction `o = P(x−c)/‖x−c‖`
//! is rounded to the grid of `B`-bit codes: each coordinate one of the
//! `2^B` values `u − (2^B−1)/2`, `u = 0..2^B−1`, scaled by the one factor
//! that brings the code `ȳ` closest in angle to `o` (one bit: the signs of
//! `o`). For a query `q`, RaBitQ's estimator
//!
//! ```text
//! q · x ≈ q·c + ‖x−c‖ · ⟨Pq, ȳ⟩ / ⟨ȳ, o⟩
//! ```
//!
//! has no bias and an error that shrinks like `1/√dim`, with one float a
//! row, `f = ‖x−c‖/⟨ȳ, o⟩`, kept at the end of its code. The error is
//! bounded too: with `cos = ⟨ȳ, o⟩/‖ȳ‖`, it stays under
//! `ε₀·‖q‖·e`, `e = ‖x−c‖·√(1−cos²)/(cos·√(d−1))`, but with a probability
//! that falls like `exp(−ε₀²)` (Theorem 3.2 of the 2024 paper; [`EPS0`]).
//! `e` is the row's second float, and a search rescores a candidate in
//! float32 only if its estimate plus that bound reaches the `k`-th exact
//! score found so far. The rotation
//! spreads every vector's energy evenly over the coordinates, which is what
//! makes a bit or two per coordinate enough: [`super::bits`]'s signs around
//! the mean, without it, rank candidates more loosely.
//!
//! **Layout.** A code row is `B` bit planes of `⌈dim/64⌉` little-endian u64
//! words (plane `b` holds bit `b` of every `u`), then `f` and `e` as
//! little-endian f32: `B·⌈dim/64⌉·8 + 8` bytes. The vectors are padded with
//! zeros to `⌈dim/64⌉·64` dimensions before the rotation, which mixes the
//! padding in, so every coordinate of the code is used.
//!
//! **Scoring.** The query is rotated (not centred: `q·c` is one number per
//! query) and quantised to int8 weights `v` as [`super::bits`] does, so
//! `⟨Pq, u⟩ = Σ_b 2^b Σ_{bit b of u_j set} v_j` is `B` runs of the binary
//! kernel ([`kernels::dot_bits_u8`]), one per plane. A row's estimate is a
//! float; [`RabitqQuery::dot`] returns it as an `i32` in the same order
//! ([`ordered`]), which is all the searches compare, and
//! [`RabitqQuery::score`] turns it back.
//!
//! **Rotation.** Four rounds of random signs, a normalised fast Hadamard
//! transform of the first (even rounds) or last (odd rounds) `2^⌊log2 d⌋`
//! coordinates and, when `d` is not a power of two, a butterfly mixing each
//! coordinate of the first half with its partner in the second (as the
//! RaBitQ library's `FhtKacRotator`): orthogonal, `O(d log d)` a vector,
//! and drawn from [`SEED`] by splitmix64, so every segment and every query
//! uses the same one for the same dimensions.

use std::sync::{Arc, Mutex, OnceLock};
use std::collections::HashMap;

use crate::postings::bytes::Slice;
use crate::vector::bits::Weights;
use crate::vector::kernels;

/// The rotation's seed. Codes written today are read with the rotation it
/// draws, so it never changes (pinned by a test).
pub const SEED: u64 = 0x5261_4269_7451_0001;

/// Rounds of the rotation.
const ROUNDS: usize = 4;

/// The width of the error bound, in units of a row's `‖q‖·e`, the
/// estimate's standard deviation when the rotation is random: three of
/// them, past which a normal error lies once in 740 estimates (the 2024
/// paper suggests 1.9, which 3% of estimates pass here).
pub const EPS0: f32 = 3.0;

/// Dimensions the vectors are padded to before the rotation.
pub fn padded(dim: usize) -> usize {
    dim.next_multiple_of(64)
}

/// Bytes per code row of `dim` dimensions at `bits` bits.
pub fn stride(dim: usize, bits: usize) -> usize {
    bits * padded(dim) / 8 + 8
}

/// A float as an `i32` in the same order (an involution: [`unordered`]
/// undoes it). Finite floats never map to `i32::MIN`.
#[inline]
pub fn ordered(f: f32) -> i32 {
    let b = f.to_bits() as i32;
    b ^ ((b >> 31) & 0x7FFF_FFFF)
}

/// The float [`ordered`] mapped to `i`.
#[inline]
pub fn unordered(i: i32) -> f32 {
    f32::from_bits((i ^ ((i >> 31) & 0x7FFF_FFFF)) as u32)
}

/// splitmix64.
struct Mix(u64);

impl Mix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The random orthogonal transform of `d` (a multiple of 64) dimensions.
#[derive(Debug)]
pub struct Rotation {
    d: usize,
    /// The largest power of two up to `d`.
    h: usize,
    /// Round `r`'s signs: `signs[r·d..][..d]`, each ±1.
    signs: Vec<f32>,
}

impl Rotation {
    fn new(d: usize, seed: u64) -> Rotation {
        assert!(d > 0 && d.is_multiple_of(64), "rotation of {d} dimensions");
        let mut g = Mix(seed);
        let signs = (0..ROUNDS * d).map(|_| if g.next() >> 63 == 1 { 1.0 } else { -1.0 }).collect();
        Rotation { d, h: 1 << (usize::BITS - 1 - d.leading_zeros()), signs }
    }

    /// The rotation of `d` dimensions, shared by every user in the process.
    pub fn of(d: usize) -> Arc<Rotation> {
        static ALL: OnceLock<Mutex<HashMap<usize, Arc<Rotation>>>> = OnceLock::new();
        let mut all = ALL.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        all.entry(d).or_insert_with(|| Arc::new(Rotation::new(d, SEED))).clone()
    }

    pub fn dims(&self) -> usize {
        self.d
    }

    /// Rotates `x` (`d` values) in place.
    pub fn apply(&self, x: &mut [f32]) {
        let (d, h) = (self.d, self.h);
        assert_eq!(x.len(), d);
        for r in 0..ROUNDS {
            for (v, s) in x.iter_mut().zip(&self.signs[r * d..(r + 1) * d]) {
                *v *= s;
            }
            if r % 2 == 0 {
                fht(&mut x[..h]);
            } else {
                fht(&mut x[d - h..]);
            }
            if h < d {
                let half = d / 2;
                let k = std::f32::consts::FRAC_1_SQRT_2;
                let (a, b) = x.split_at_mut(half);
                for (p, q) in a.iter_mut().zip(b.iter_mut()) {
                    (*p, *q) = ((*p + *q) * k, (*p - *q) * k);
                }
            }
        }
    }
}

/// Normalised Walsh-Hadamard transform of `x` (a power of two long).
fn fht(x: &mut [f32]) {
    let n = x.len();
    let mut h = 1;
    while h < n {
        for i in (0..n).step_by(2 * h) {
            let (a, b) = x[i..i + 2 * h].split_at_mut(h);
            for (p, q) in a.iter_mut().zip(b.iter_mut()) {
                (*p, *q) = (*p + *q, *p - *q);
            }
        }
        h *= 2;
    }
    let s = 1.0 / (n as f32).sqrt();
    x.iter_mut().for_each(|v| *v *= s);
}

/// A segment's RaBitQ quantiser: the bits per dimension, the mean the rows
/// are centred on, and the rotation of their padded dimensions.
#[derive(Clone, Debug)]
pub struct Rabitq {
    pub bits: usize,
    /// The segment's mean vector (`dim` values).
    pub centre: Slice<f32>,
    rot: Arc<Rotation>,
}

/// A query prepared for scoring RaBitQ codes.
#[derive(Clone, Debug)]
pub struct RabitqQuery {
    /// The rotated query's int8 weights, as [`kernels::BitsWeights`] reads them.
    pub u: Vec<u8>,
    pub v: Vec<i8>,
    pub planes: Vec<u64>,
    /// `Pq ≈ alpha·v`.
    pub alpha: f32,
    /// `q·c`.
    pub bias: f32,
    /// `(2^B−1)/2 · alpha·Σ v_j`: the offset of the codes' values.
    offset: f32,
    /// `ε₀·‖q‖`: a row's bound is this times its `e`.
    width: f32,
    bits: usize,
    /// Bytes per plane.
    plane: usize,
}

impl RabitqQuery {
    #[inline]
    fn weights(&self) -> kernels::BitsWeights<'_> {
        kernels::BitsWeights { u: &self.u, v: &self.v, planes: &self.planes }
    }

    #[inline]
    fn estimate(&self, row: &[u8], sum: i32) -> f32 {
        let at = self.bits * self.plane;
        let f = f32::from_le_bytes(row[at..at + 4].try_into().expect("4 bytes"));
        f * (self.alpha * sum as f32 - self.offset)
    }

    /// A code row's estimate of `q·x − q·c`, in [`ordered`] form.
    #[inline]
    pub fn dot(&self, row: &[u8]) -> i32 {
        let p = self.plane;
        let mut sum = 0i32;
        for b in 0..self.bits {
            sum += kernels::dot_bits_u8(&row[b * p..(b + 1) * p], self.weights()) << b;
        }
        ordered(self.estimate(row, sum))
    }

    /// [`RabitqQuery::dot`] of four rows.
    #[inline]
    pub fn dot4(&self, rows: [&[u8]; 4]) -> [i32; 4] {
        let p = self.plane;
        let mut sum = [0i32; 4];
        for b in 0..self.bits {
            let s = kernels::dot_bits_u8_x4(rows.map(|r| &r[b * p..(b + 1) * p]), self.weights());
            for i in 0..4 {
                sum[i] += s[i] << b;
            }
        }
        [0, 1, 2, 3].map(|i| ordered(self.estimate(rows[i], sum[i])))
    }

    /// The estimated dot product of a [`RabitqQuery::dot`] score.
    #[inline]
    pub fn score(&self, dot: i32) -> f32 {
        self.bias + unordered(dot)
    }

    /// The most the dot product of `row`'s vector can be, given its score
    /// `dot`, but with a probability of about 1/740 ([`EPS0`]).
    #[inline]
    pub fn upper(&self, dot: i32, row: &[u8]) -> f32 {
        let at = self.bits * self.plane + 4;
        let e = f32::from_le_bytes(row[at..at + 4].try_into().expect("4 bytes"));
        self.score(dot) + self.width * e
    }
}

impl Rabitq {
    /// The quantiser of `bits` bits for `rows` (each of `dim` values),
    /// centred on their mean.
    pub fn fit<'a, I: Iterator<Item = &'a [f32]>>(dim: usize, bits: usize, rows: I) -> Rabitq {
        let mut sum = vec![0f64; dim];
        let mut n = 0usize;
        for r in rows {
            for j in 0..dim {
                sum[j] += r[j] as f64;
            }
            n += 1;
        }
        let centre: Vec<f32> = sum.iter().map(|s| if n > 0 { (s / n as f64) as f32 } else { 0.0 }).collect();
        Rabitq::new(bits, Slice::Owned(centre))
    }

    /// The quantiser of a segment's parameters.
    pub fn new(bits: usize, centre: Slice<f32>) -> Rabitq {
        assert!(matches!(bits, 1 | 2 | 4), "{bits} bits");
        let rot = Rotation::of(padded(centre.len().max(1)));
        Rabitq { bits, centre, rot }
    }

    pub fn dim(&self) -> usize {
        self.centre.len()
    }

    /// `x` centred and rotated, and its norm.
    fn direction(&self, x: &[f32]) -> (Vec<f32>, f32) {
        let mut r = vec![0f32; self.rot.dims()];
        for (j, c) in self.centre.iter().enumerate() {
            r[j] = x[j] - c;
        }
        self.rot.apply(&mut r);
        let norm = r.iter().map(|v| v * v).sum::<f32>().sqrt();
        (r, norm)
    }

    /// The code of `x` into `out` ([`stride`] bytes, overwritten).
    pub fn encode(&self, x: &[f32], out: &mut [u8]) {
        out.fill(0);
        let (r, norm) = self.direction(x);
        let plane = self.rot.dims() / 8;
        let at = self.bits * plane;
        // A row at the centre is estimated as `q·c` exactly (f = 0).
        if !(norm > 0.0 && norm.is_finite()) {
            return;
        }
        let o: Vec<f32> = r.iter().map(|v| v / norm).collect();
        let (u, ip) = grid(&o, self.bits);
        for (j, &u) in u.iter().enumerate() {
            for b in 0..self.bits {
                if u >> b & 1 == 1 {
                    out[b * plane + j / 8] |= 1 << (j % 8);
                }
            }
        }
        let f = if ip > 0.0 { norm / ip } else { 0.0 };
        let e = error(norm, ip, &u, self.bits);
        out[at..at + 4].copy_from_slice(&f.to_le_bytes());
        out[at + 4..at + 8].copy_from_slice(&e.to_le_bytes());
    }

    /// The vector a code stands for: `c + ‖x−c‖·Pᵀȳ/‖ȳ‖`, the point in
    /// the code's direction at `x`'s distance from the centre (`‖x−c‖` and
    /// `cos` follow from `f`, `e` and `‖ȳ‖`).
    pub fn decode(&self, code: &[u8], out: &mut [f32]) {
        let d = self.rot.dims();
        let plane = d / 8;
        let at = self.bits * plane;
        let read = |i: usize| f32::from_le_bytes(code[i..i + 4].try_into().expect("4 bytes"));
        let (f, e) = (read(at), read(at + 4));
        let half = ((1u32 << self.bits) - 1) as f32 / 2.0;
        let mut y: Vec<f32> = (0..d)
            .map(|j| {
                let u: u32 = (0..self.bits).map(|b| ((code[b * plane + j / 8] >> (j % 8) & 1) as u32) << b).sum();
                u as f32 - half
            })
            .collect();
        let len = y.iter().map(|v| v * v).sum::<f32>().sqrt();
        // e = f·‖ȳ‖·sin/√(d−1) and ‖x−c‖ = f·‖ȳ‖·cos.
        let sin = if f > 0.0 && len > 0.0 { (e * ((d - 1) as f32).sqrt() / (f * len)).min(1.0) } else { 1.0 };
        let norm = f * len * (1.0 - sin * sin).sqrt();
        // Pᵀ: the rotation's rounds undone in reverse.
        self.rot.invert(&mut y);
        for (j, o) in out.iter_mut().enumerate().take(self.dim()) {
            *o = self.centre[j] + if len > 0.0 { norm * y[j] / len } else { 0.0 };
        }
    }

    /// Prepares `q` for scoring code rows of `stride` bytes.
    pub fn prepare(&self, q: &[f32], row_bytes: usize) -> RabitqQuery {
        let d = self.rot.dims();
        assert_eq!(row_bytes, stride(self.dim(), self.bits), "code stride");
        let mut r = vec![0f32; d];
        r[..self.dim()].copy_from_slice(&q[..self.dim()]);
        self.rot.apply(&mut r);
        let bias: f64 = self.centre.iter().zip(q).map(|(c, x)| *c as f64 * *x as f64).sum();
        let qnorm = r.iter().map(|x| x * x).sum::<f32>().sqrt();
        let Weights { u, v, planes, alpha } = Weights::quantise(&r, d / 64);
        let vsum: i64 = v.iter().map(|&x| x as i64).sum();
        let half = ((1u32 << self.bits) - 1) as f32 / 2.0;
        RabitqQuery { u, v, planes, alpha, bias: bias as f32, offset: half * alpha * vsum as f32, width: EPS0 * qnorm, bits: self.bits, plane: d / 8 }
    }
}

impl Rotation {
    /// The inverse (the transpose) of [`Rotation::apply`].
    fn invert(&self, x: &mut [f32]) {
        let (d, h) = (self.d, self.h);
        for r in (0..ROUNDS).rev() {
            if h < d {
                let half = d / 2;
                let k = std::f32::consts::FRAC_1_SQRT_2;
                let (a, b) = x.split_at_mut(half);
                for (p, q) in a.iter_mut().zip(b.iter_mut()) {
                    (*p, *q) = ((*p + *q) * k, (*p - *q) * k);
                }
            }
            // The normalised Hadamard transform is its own inverse.
            if r % 2 == 0 {
                fht(&mut x[..h]);
            } else {
                fht(&mut x[d - h..]);
            }
            for (v, s) in x.iter_mut().zip(&self.signs[r * d..(r + 1) * d]) {
                *v *= s;
            }
        }
    }
}

/// A row's error scale `e = ‖x−c‖·√(1−cos²)/(cos·√(d−1))`, with
/// `cos = ⟨ȳ, o⟩/‖ȳ‖` (`ip = ⟨ȳ, o⟩`), for its code `u` of `bits` bits.
fn error(norm: f32, ip: f32, u: &[u32], bits: usize) -> f32 {
    let half = ((1u32 << bits) - 1) as f64 / 2.0;
    let len = u.iter().map(|&u| (u as f64 - half).powi(2)).sum::<f64>().sqrt();
    let cos = (ip as f64 / len).clamp(1e-6, 1.0);
    (norm as f64 * (1.0 - cos * cos).sqrt() / (cos * ((u.len() - 1) as f64).sqrt())) as f32
}

/// The `bits`-bit code of unit vector `o` closest to it in angle, and
/// `⟨ȳ, o⟩` with `ȳ_j = u_j − (2^B−1)/2`. One bit is the signs. With more,
/// coordinate `j` takes `±(k_j + ½)`, `k_j = min(⌊t·|o_j|⌋, 2^{B−1}−1)`,
/// for the scale `t` that maximises the cosine. The cosine changes only
/// where some `k_j` steps up, at `t = s/|o_j|`, so the steps are visited
/// in order of `t` (Gao et al. 2025, §3.2): with the coordinates sorted by
/// `|o_j|`, largest first, each step `s` comes to them in that order, and
/// the sweep merges the `2^{B−1}−1` runs.
pub(crate) fn grid(o: &[f32], bits: usize) -> (Vec<u32>, f32) {
    let top = 1u32 << (bits - 1);
    let abs: Vec<f32> = o.iter().map(|v| v.abs()).collect();
    let mut k = vec![0u32; o.len()];
    if bits > 1 {
        let steps = (top - 1) as usize;
        let mut order: Vec<u32> = (0..o.len() as u32).filter(|&j| abs[j as usize] > 0.0).collect();
        order.sort_unstable_by(|&a, &b| abs[b as usize].total_cmp(&abs[a as usize]).then(a.cmp(&b)));
        // head[s]: the rank of the next coordinate to take step s + 1;
        // those ranked before head[s − 1] have taken step s.
        let mut head = vec![0usize; steps];
        let mut num: f64 = abs.iter().map(|&a| a as f64).sum::<f64>() * 0.5;
        let mut den = o.len() as f64 * 0.25;
        let mut best = num / den.sqrt();
        let mut best_head = head.clone();
        loop {
            let mut pick: Option<(usize, f32)> = None;
            for s in 0..steps {
                if head[s] < order.len() && (s == 0 || head[s] < head[s - 1]) {
                    let t = (s + 1) as f32 / abs[order[head[s]] as usize];
                    if pick.is_none_or(|(_, m)| t < m) {
                        pick = Some((s, t));
                    }
                }
            }
            let Some((s, _)) = pick else { break };
            let j = order[head[s]] as usize;
            // k_j goes from s to s + 1.
            num += abs[j] as f64;
            den += 2.0 * s as f64 + 2.0;
            head[s] += 1;
            let cos = num / den.sqrt();
            if cos > best {
                best = cos;
                best_head.copy_from_slice(&head);
            }
        }
        for (rank, &j) in order.iter().enumerate() {
            k[j as usize] = best_head.iter().filter(|&&h| h > rank).count() as u32;
        }
    }
    let mut ip = 0f64;
    let u = o
        .iter()
        .zip(&k)
        .zip(&abs)
        .map(|((&v, &k), &a)| {
            ip += a as f64 * (k as f64 + 0.5);
            if v > 0.0 { top + k } else { top - 1 - k }
        })
        .collect();
    (u, ip as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};

    fn gaussian(rng: &mut impl Rng) -> f32 {
        let (u, v): (f64, f64) = (rng.gen_range(1e-12..1.0), rng.gen());
        ((-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()) as f32
    }

    /// Unit vectors around a shared direction, as embeddings are.
    fn embeddings(rng: &mut rand::rngs::StdRng, n: usize, dim: usize) -> Vec<Vec<f32>> {
        let common: Vec<f32> = (0..dim).map(|_| gaussian(rng)).collect();
        (0..n)
            .map(|_| {
                let v: Vec<f32> = common.iter().map(|c| c + gaussian(rng)).collect();
                let l = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                v.into_iter().map(|x| x / l).collect()
            })
            .collect()
    }

    #[test]
    fn floats_keep_their_order_as_integers() {
        let fs = [f32::NEG_INFINITY, -1e30, -2.0, -1.0, -1e-30, -0.0, 0.0, 1e-30, 1.0, 2.0, 1e30, f32::INFINITY];
        for w in fs.windows(2) {
            assert!(ordered(w[0]) <= ordered(w[1]), "{} {}", w[0], w[1]);
        }
        for &f in &fs {
            assert_eq!(unordered(ordered(f)).to_bits(), f.to_bits());
            assert_ne!(ordered(f), i32::MIN);
        }
    }

    #[test]
    fn the_rotation_is_orthogonal_and_fixed() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        for d in [64, 128, 192, 384, 768, 1024, 1536] {
            let rot = Rotation::new(d, SEED);
            let x: Vec<f32> = (0..d).map(|_| gaussian(&mut rng)).collect();
            let y: Vec<f32> = (0..d).map(|_| gaussian(&mut rng)).collect();
            let (mut rx, mut ry) = (x.clone(), y.clone());
            rot.apply(&mut rx);
            rot.apply(&mut ry);
            let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(p, q)| p * q).sum::<f32>();
            assert!((dot(&x, &y) - dot(&rx, &ry)).abs() < 1e-3 * d as f32, "d {d}: inner products kept");
            assert!((dot(&x, &x) - dot(&rx, &rx)).abs() < 1e-3 * d as f32, "d {d}: norms kept");
            rot.invert(&mut rx);
            assert!(rx.iter().zip(&x).all(|(a, b)| (a - b).abs() < 1e-4), "d {d}: inverted");
            // A basis vector is spread over every coordinate.
            let mut e = vec![0f32; d];
            e[3] = 1.0;
            rot.apply(&mut e);
            let max = e.iter().fold(0f32, |m, v| m.max(v.abs()));
            assert!(max < 6.0 / (d as f32).sqrt(), "d {d}: largest coordinate {max}");
        }
        // The generator's first numbers and the first signs, pinned: codes
        // written today are read with this rotation by every later version.
        let mut g = Mix(SEED);
        assert_eq!([g.next(), g.next()], [0x5EEA_054A_7035_C0E3, 0xE5FA_6965_EC87_ED6F]);
        let r = Rotation::new(64, SEED);
        let signs: String = r.signs[..16].iter().map(|&s| if s > 0.0 { '+' } else { '-' }).collect();
        assert_eq!(signs, "-++-++++++---+--");
    }

    #[test]
    fn the_grid_finds_the_closest_code() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        for bits in [1, 2, 4] {
            for d in [64, 128, 384] {
                let v: Vec<f32> = (0..d).map(|_| gaussian(&mut rng)).collect();
                let l = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                let o: Vec<f32> = v.iter().map(|x| x / l).collect();
                let (u, ip) = grid(&o, bits);
                let half = ((1u32 << bits) - 1) as f32 / 2.0;
                let cos = |u: &[u32]| {
                    let y: Vec<f32> = u.iter().map(|&u| u as f32 - half).collect();
                    let n = y.iter().map(|x| x * x).sum::<f32>().sqrt();
                    y.iter().zip(&o).map(|(a, b)| a * b).sum::<f32>() / n
                };
                assert!(u.iter().all(|&u| u < 1 << bits));
                let y: Vec<f32> = u.iter().map(|&u| u as f32 - half).collect();
                assert!((ip - y.iter().zip(&o).map(|(a, b)| a * b).sum::<f32>()).abs() < 1e-4);
                let best = cos(&u);
                // No code from a scale on a fine grid does better.
                let top = 1u32 << (bits - 1);
                for s in 1..400 {
                    let t = s as f32 * 0.05 * (d as f32).sqrt();
                    let alt: Vec<u32> = o.iter().map(|&x| {
                        let k = ((t * x.abs()) as u32).min(top - 1);
                        if x > 0.0 { top + k } else { top - 1 - k }
                    }).collect();
                    assert!(cos(&alt) <= best + 1e-5, "bits {bits} d {d}");
                }
                // Known cosines for Gaussian directions: about 0.80 for one
                // bit, 0.94 for two and 0.995 for four.
                let want = [0.0, 0.7, 0.88, 0.0, 0.98][bits];
                assert!(best > want, "bits {bits} d {d}: cosine {best}");
            }
        }
    }

    #[test]
    fn estimates_track_the_dot_product_and_tighten_with_bits() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(5);
        for dim in [100, 384, 768] {
            let rows = embeddings(&mut rng, 300, dim);
            let queries = embeddings(&mut rng, 20, dim);
            let mut errs = Vec::new();
            for bits in [1, 2, 4] {
                let rq = Rabitq::fit(dim, bits, rows.iter().map(|r| r.as_slice()));
                let s = stride(dim, bits);
                let codes: Vec<Vec<u8>> = rows.iter().map(|r| {
                    let mut c = vec![0u8; s];
                    rq.encode(r, &mut c);
                    c
                }).collect();
                let (mut err, mut sum, mut over, mut width) = (0f64, 0f64, 0, 0f64);
                for q in &queries {
                    let p = rq.prepare(q, s);
                    for (r, c) in rows.iter().zip(&codes) {
                        let exact: f32 = q.iter().zip(r).map(|(a, b)| a * b).sum();
                        let dot = p.dot(c);
                        let est = p.score(dot);
                        err += (est - exact).abs() as f64;
                        sum += 1.0;
                        let up = p.upper(dot, c);
                        over += (exact > up) as usize;
                        width += (up - est) as f64;
                    }
                    // Four at once score as one at a time.
                    let four = p.dot4([&codes[0], &codes[1], &codes[2], &codes[3]]);
                    assert_eq!(four, [0, 1, 2, 3].map(|i| p.dot(&codes[i])));
                }
                errs.push(err / sum);
                // The bound holds for all but a few in a thousand, and is
                // a few times the mean error wide.
                assert!((over as f64) < 0.005 * sum, "dim {dim} bits {bits}: {over} of {sum} above their bound");
                assert!(width / sum < 6.0 * err / sum, "dim {dim} bits {bits}: bound {} against error {}", width / sum, err / sum);
                // A decoded code points the way its row does, from as far.
                let mut back = vec![0f32; dim];
                rq.decode(&codes[7], &mut back);
                let (a, b): (Vec<f32>, Vec<f32>) = (rows[7].iter().zip(rq.centre.iter()).map(|(x, c)| x - c).collect(), back.iter().zip(rq.centre.iter()).map(|(x, c)| x - c).collect());
                let cos = a.iter().zip(&b).map(|(x, y)| x * y).sum::<f32>() / (a.iter().map(|x| x * x).sum::<f32>().sqrt() * b.iter().map(|x| x * x).sum::<f32>().sqrt());
                assert!(cos > [0.0, 0.75, 0.9, 0.0, 0.98][bits], "dim {dim} bits {bits}: decoded cosine {cos}");
                let len = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
                // (At 100 dimensions part of the code lies in the padding.)
                let tol = if dim % 64 == 0 { 0.03 } else { 0.15 };
                assert!((len(&a) / len(&b) - 1.0).abs() < tol, "dim {dim} bits {bits}: {} against {}", len(&a), len(&b));
            }
            // The mean error falls with each step of bits, and like
            // 1/√dim: 0.037, 0.018 and 0.005 at 100 dimensions, 0.015,
            // 0.007 and 0.002 at 768.
            assert!(errs[0] > errs[1] && errs[1] > errs[2], "dim {dim}: {errs:?}");
            assert!(errs[0] < 0.5 / (padded(dim) as f64).sqrt() && errs[2] < errs[0] / 5.0, "dim {dim}: {errs:?}");
        }
    }

    #[test]
    fn a_row_at_the_centre_scores_the_centre() {
        let rq = Rabitq::new(2, Slice::Owned(vec![0.5, -0.5, 0.25]));
        let s = stride(3, 2);
        let mut c = vec![0u8; s];
        rq.encode(&[0.5, -0.5, 0.25], &mut c);
        let p = rq.prepare(&[1.0, 2.0, 4.0], s);
        assert!((p.score(p.dot(&c)) - (0.5 - 1.0 + 1.0)).abs() < 1e-6);
    }
}
