//! Multi-vector fields (`VECTOR(d)[]`): many vectors per row, such as a
//! page's patch embeddings or a video's frames, scored by `maxsim`.
//!
//! A row's score for query vectors `q_1..q_m` is the sum, over the query
//! vectors, of each one's best match among the row's vectors:
//!
//! ```text
//! maxsim(row) = Σ_i max_j q_i · v_j
//! ```
//!
//! The field's sections are the vector sections of [`super`] indexed by
//! vector instead of by row (`n` in [`super::VectorInfo`] counts vectors),
//! plus:
//!
//! | section | contents |
//! |---|---|
//! | `vec.<f>.rows` | u32 offsets, one per row plus one: row `r`'s vectors are `rows[r]..rows[r + 1]` |
//! | `vec.<f>.owner` | u32 per vector: its row |
//! | `vec.<f>.rescore`, `.rescorep` | SQ8 codes and their ranges, which score a row exactly, when the search codes are binary |
//! | `vec.<f>.fdep`, `vec.<f>.fde.*` | each row's MUVERA encoding ([`super::fde`]): its parameters, and the sections of a one-vector field of the encodings (SQ8 codes, HNSW graph, float32) |
//!
//! The float32 vectors (`vec.<f>.f32`) are kept only with `keep_f32`: a
//! 1,030-vector page at 128 dimensions is 527 KB in float32, 132 KB in SQ8
//! and 16.5 KB in binary codes. Without them a row's vectors read back
//! (and merge) as their SQ8 values, and scores are SQ8 scores.
//!
//! **Search.** A segment with its rows' encodings runs one filtered search
//! of them, with the query's encoding: its best [`CANDIDATES`] rows are the
//! candidates ([`Search::Encodings`]). Otherwise (or with `search =>
//! 'vectors'`) each query vector runs a filtered vector search over the
//! vectors ([`DEPTH`] best, with the field's index and strategies); the
//! rows that own them are the candidates, at most [`CANDIDATES`] of them,
//! kept by the sum of the best match each query vector found in them.
//! Each candidate is then scored exactly over all its vectors.
//!
//! **Pooling.** A field may merge each row's vectors when the row is
//! written: those whose cosine similarity is at least `pool` ([`pool`]), or
//! about one in every `pool_factor` by Ward's linkage ([`ward`]). Only new
//! values are pooled ([`crate::value::Pooling`]).

use std::sync::Arc;

use crate::error::{corrupt, Error, Result};
use crate::postings::bytes::Slice;
use crate::schema::{Field, FieldType};
use crate::segment::{DocFilter, DocId, SegmentFile};
use crate::value::Value;
use crate::vector::kernels::dot_f32;
use crate::vector::section;

/// Vectors each query vector's search keeps (`depth => n`).
pub const DEPTH: usize = 64;

/// Rows scored exactly per segment (`candidates => n`).
pub const CANDIDATES: usize = 200;

/// Where `maxsim` finds a segment's candidate rows (`search => ...`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Search {
    /// The rows' encodings where the segment has them, else the vectors.
    Auto,
    /// One search of the rows' MUVERA encodings ([`super::fde`]); segments
    /// without them search their vectors.
    Encodings,
    /// One search of the vectors for each query vector.
    Vectors,
}

/// The rows of a multi-vector field's vectors, and the vectors of its rows.
#[derive(Clone, Debug)]
pub struct Rows {
    offsets: Slice<u32>,
    owner: Slice<u32>,
}

impl Rows {
    /// Sections `rows` and `owner` of `field`, for `rows` rows holding `n`
    /// vectors.
    pub(crate) fn open(seg: &SegmentFile, field: &str, rows: usize, n: usize) -> Result<Rows> {
        let offsets: Slice<u32> = Slice::view(seg.require(&section(field, "rows"))?).map_err(corrupt)?;
        let owner: Slice<u32> = Slice::view(seg.require(&section(field, "owner"))?).map_err(corrupt)?;
        let ordered = offsets.first() == Some(&0) && offsets.windows(2).all(|w| w[0] <= w[1]);
        if offsets.len() != rows + 1 || !ordered || offsets[rows] as usize != n || owner.len() != n {
            return Err(corrupt(format!("vector field {field}: bad rows of a multi-vector field")));
        }
        Ok(Rows { offsets, owner })
    }

    /// Row `r`'s vectors.
    #[inline]
    pub fn vectors(&self, r: DocId) -> std::ops::Range<DocId> {
        self.offsets[r as usize]..self.offsets[r as usize + 1]
    }

    /// The row vector `v` belongs to (a damaged owner reads as none).
    #[inline]
    pub fn owner(&self, v: DocId) -> Option<DocId> {
        let r = *self.owner.get(v as usize)?;
        (r as usize + 1 < self.offsets.len() && self.vectors(r).contains(&v)).then_some(r)
    }

    pub fn n_rows(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn is_shared(&self) -> bool {
        matches!(self.offsets, Slice::Shared { .. }) && matches!(self.owner, Slice::Shared { .. })
    }
}

/// A filter of rows as a filter of their vectors.
pub struct OwnerFilter<'a> {
    pub rows: &'a Rows,
    pub filter: &'a dyn DocFilter,
    pub n: usize,
}

impl DocFilter for OwnerFilter<'_> {
    fn contains(&self, v: DocId) -> bool {
        self.rows.owner(v).is_some_and(|r| self.filter.contains(r))
    }

    /// The rows' share of the vectors.
    fn cardinality(&self) -> usize {
        let rows = self.rows.n_rows().max(1);
        ((self.filter.cardinality().min(rows) as f64 / rows as f64) * self.n as f64).ceil() as usize
    }
}

/// `v` as a multi-vector field's value: a list of vectors of `field`'s
/// dimensions, from a list of vectors or of lists of numbers, a vector
/// (a list of one), or JSON text of an array of arrays. NULL and an empty
/// list are NULL.
pub fn normalize(field: &Field, v: Value) -> Result<Value> {
    let FieldType::Vector { dim, .. } = field.ty else { return Ok(v) };
    let bad = |m: &str| Error::Schema(format!("bad value for multi-vector field {}: {m}", field.name));
    let one = |x: Value| -> Result<Arc<[f32]>> {
        let v: Arc<[f32]> = match x {
            Value::Vector(v) => v,
            Value::List(xs) => xs.iter().map(|x| x.as_f64().map(|f| f as f32)).collect::<Option<Vec<f32>>>().ok_or_else(|| bad("a vector is a list of numbers"))?.into(),
            other => return Err(bad(&format!("{other:?} is not a vector"))),
        };
        if v.len() != dim || v.iter().any(|x| !x.is_finite()) {
            return Err(bad(&format!("a vector of {} dimensions (finite numbers), not {}", dim, v.len())));
        }
        Ok(v)
    };
    let items = match v {
        Value::Null => return Ok(Value::Null),
        Value::Str(s) => {
            let j: serde_json::Value = serde_json::from_str(&s).map_err(|_| bad("text is a JSON array of vectors"))?;
            let rows = j.as_array().ok_or_else(|| bad("text is a JSON array of vectors"))?;
            rows.iter()
                .map(|r| r.as_array().and_then(|xs| xs.iter().map(|x| x.as_f64().map(|f| f as f32)).collect::<Option<Vec<f32>>>()).map(|v| Value::Vector(v.into())))
                .collect::<Option<Vec<Value>>>()
                .ok_or_else(|| bad("text is a JSON array of arrays of numbers"))?
        }
        Value::List(xs) if xs.iter().all(|x| x.as_f64().is_some()) && !xs.is_empty() => vec![Value::List(xs)],
        Value::List(xs) => xs,
        v @ Value::Vector(_) => vec![v],
        other => return Err(bad(&format!("{other:?} is not a list of vectors"))),
    };
    if items.is_empty() {
        return Ok(Value::Null);
    }
    let vs = items.into_iter().map(one).collect::<Result<Vec<_>>>()?;
    Ok(Value::List(vs.into_iter().map(Value::Vector).collect()))
}

/// A [`normalize`]d value pooled as `field` asks at write: by cosine
/// similarity ([`Field::pool`], [`pool`]) or by Ward's linkage with a pool
/// factor ([`Field::pool_factor`], [`ward`]). Writes pool their new values
/// only ([`crate::value::conform_as`]): a row read back and written again,
/// the rest of a row an UPDATE rewrites or a primary's rows on a replica,
/// keeps the vectors it has.
pub fn pooled(field: &Field, v: Value) -> Value {
    let (Value::List(items), true) = (&v, field.pool.is_some() || field.pool_factor.is_some()) else { return v };
    let vs: Vec<Arc<[f32]>> = items.iter().filter_map(|x| if let Value::Vector(v) = x { Some(v.clone()) } else { None }).collect();
    if vs.len() != items.len() {
        return v;
    }
    let out = match (field.pool, field.pool_factor) {
        (Some(min), _) => pool(&vs, min),
        (None, Some(f)) => ward(&vs, f),
        (None, None) => vs,
    };
    Value::List(out.into_iter().map(Value::Vector).collect())
}

/// Token pooling: merges the two most similar groups of `vs` (by the cosine
/// of their means) while that cosine is at least `min_cos`. A group's
/// vector is the mean of its members, scaled to their mean length. No two
/// of the results are that similar, so pooling them again keeps them.
pub fn pool(vs: &[Arc<[f32]>], min_cos: f32) -> Vec<Arc<[f32]>> {
    let n = vs.len();
    if n < 2 {
        return vs.to_vec();
    }
    let dim = vs[0].len();
    // Each group: the sum of its members, the sum of their lengths, its
    // member count and its unit direction.
    let norm = |v: &[f32]| dot_f32(v, v).sqrt();
    let unit = |s: &[f32]| {
        let l = norm(s);
        s.iter().map(|x| if l > 0.0 { x / l } else { 0.0 }).collect::<Vec<f32>>()
    };
    let mut sum: Vec<Vec<f32>> = vs.iter().map(|v| v.to_vec()).collect();
    let mut lens: Vec<f32> = vs.iter().map(|v| norm(v)).collect();
    let mut count = vec![1usize; n];
    let mut dir: Vec<Vec<f32>> = sum.iter().map(|s| unit(s)).collect();
    let mut alive = vec![true; n];
    // Each live group's most similar other group.
    let best_of = |i: usize, dir: &[Vec<f32>], alive: &[bool]| -> (f32, usize) {
        let mut best = (f32::NEG_INFINITY, usize::MAX);
        for j in 0..dir.len() {
            if j != i && alive[j] {
                let c = dot_f32(&dir[i], &dir[j]);
                if c > best.0 || (c == best.0 && j < best.1) {
                    best = (c, j);
                }
            }
        }
        best
    };
    let mut best: Vec<(f32, usize)> = (0..n).map(|i| best_of(i, &dir, &alive)).collect();
    loop {
        let Some((i, &(c, j))) = best.iter().enumerate().filter(|(i, _)| alive[*i]).max_by(|a, b| a.1 .0.total_cmp(&b.1 .0).then(b.0.cmp(&a.0))) else { break };
        if !(c >= min_cos) || j == usize::MAX {
            break;
        }
        let (keep, gone) = (i.min(j), i.max(j));
        for d in 0..dim {
            sum[keep][d] += sum[gone][d];
        }
        lens[keep] += lens[gone];
        count[keep] += count[gone];
        dir[keep] = unit(&sum[keep]);
        alive[gone] = false;
        // Groups whose best was either of the two look again.
        for k in 0..n {
            if alive[k] && (k == keep || best[k].1 == keep || best[k].1 == gone) {
                best[k] = best_of(k, &dir, &alive);
            } else if alive[k] {
                let c = dot_f32(&dir[k], &dir[keep]);
                if c > best[k].0 || (c == best[k].0 && keep < best[k].1) {
                    best[k] = (c, keep);
                }
            }
        }
    }
    (0..n)
        .filter(|&i| alive[i])
        .map(|i| {
            if count[i] == 1 {
                return vs[i].clone();
            }
            let len = lens[i] / count[i] as f32;
            dir[i].iter().map(|x| x * len).collect::<Vec<f32>>().into()
        })
        .collect()
}

/// The largest pool factor ([`Field::pool_factor`]).
pub const MAX_POOL_FACTOR: f32 = 64.0;

/// Rows of more vectors than this are pooled by [`ward`] this many (or
/// fewer, in equal parts) at a time, in their order: the clustering keeps
/// a matrix of the distances between a part's groups.
pub const WARD_PART: usize = 2048;

/// Hierarchical token pooling (Clavié, Chaffin and Adams 2024): clusters
/// `vs` into `⌈n / factor⌉` groups by Ward's linkage over their directions
/// (each group's cost is the squared distances of its members' unit vectors
/// from their mean, so the closest directions merge first, and large groups
/// grow more slowly than small ones) and keeps one vector per group: the
/// mean of its members, scaled to their mean length, as [`pool`] does. The
/// groups keep the order of their first members.
///
/// The clustering is the nearest-neighbour chain over a matrix of Ward
/// distances updated by Lance and Williams' formula, which builds the same
/// tree as merging the closest pair again and again in `O(n²)` time; the
/// tree is then cut where `⌈n / factor⌉` groups remain. Clavié et al.
/// cluster the rows of the cosine distance matrix instead of the vectors;
/// both put together the tokens of nearly the same direction.
pub fn ward(vs: &[Arc<[f32]>], factor: f32) -> Vec<Arc<[f32]>> {
    let n = vs.len();
    if n > WARD_PART {
        let parts = n.div_ceil(WARD_PART);
        let size = n.div_ceil(parts);
        return vs.chunks(size).flat_map(|c| ward(c, factor)).collect();
    }
    let target = ((n as f64 / factor.max(1.0) as f64).ceil() as usize).max(1);
    if n <= target {
        return vs.to_vec();
    }
    let norm = |v: &[f32]| dot_f32(v, v).sqrt();
    let units: Vec<Vec<f32>> = vs
        .iter()
        .map(|v| {
            let l = norm(v);
            v.iter().map(|x| if l > 0.0 { x / l } else { 0.0 }).collect()
        })
        .collect();
    // d[i·n + j]: the Ward distance of groups i and j, twice the growth of
    // the squared distances from the mean their merge would make; for two
    // vectors, their squared distance.
    let mut d = vec![0f32; n * n];
    for i in 0..n {
        let ii = dot_f32(&units[i], &units[i]);
        for j in i + 1..n {
            let x = (ii + dot_f32(&units[j], &units[j]) - 2.0 * dot_f32(&units[i], &units[j])).max(0.0);
            d[i * n + j] = x;
            d[j * n + i] = x;
        }
    }
    let mut size = vec![1f32; n];
    let mut alive = vec![true; n];
    // Merges: (distance, a member of each group).
    let mut merges: Vec<(f32, usize, usize)> = Vec::with_capacity(n - 1);
    let mut chain: Vec<usize> = Vec::with_capacity(n);
    for _ in 1..n {
        loop {
            if chain.is_empty() {
                chain.push(alive.iter().position(|&a| a).expect("two groups left"));
            }
            let a = *chain.last().expect("chain");
            let prev = chain.len().checked_sub(2).map(|i| chain[i]);
            // a's nearest group; on a tie the one before it on the chain,
            // else the first, so the chain cannot cycle.
            let mut best = prev.map_or((f32::INFINITY, usize::MAX), |p| (d[a * n + p], p));
            for j in 0..n {
                if alive[j] && j != a && Some(j) != prev && d[a * n + j] < best.0 {
                    best = (d[a * n + j], j);
                }
            }
            let b = best.1;
            if Some(b) != prev {
                chain.push(b);
                continue;
            }
            chain.truncate(chain.len() - 2);
            merges.push((best.0, a, b));
            let (keep, gone) = (a.min(b), a.max(b));
            let (sa, sb) = (size[a], size[b]);
            let dab = d[a * n + b];
            for k in 0..n {
                if alive[k] && k != a && k != b {
                    let sk = size[k];
                    let x = ((sa + sk) * d[k * n + a] + (sb + sk) * d[k * n + b] - sk * dab) / (sa + sb + sk);
                    d[keep * n + k] = x;
                    d[k * n + keep] = x;
                }
            }
            size[keep] = sa + sb;
            alive[gone] = false;
            break;
        }
    }
    // The tree's merges from the closest, until `target` groups remain.
    merges.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut parent: Vec<usize> = (0..n).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for &(_, a, b) in &merges[..n - target] {
        let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
        parent[ra.max(rb)] = ra.min(rb);
    }
    let dim = vs[0].len();
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut slot = vec![usize::MAX; n];
    for i in 0..n {
        let r = root(&mut parent, i);
        if slot[r] == usize::MAX {
            slot[r] = groups.len();
            groups.push((i, Vec::new()));
        }
        groups[slot[r]].1.push(i);
    }
    groups
        .into_iter()
        .map(|(first, members)| {
            if members.len() == 1 {
                return vs[first].clone();
            }
            let mut sum = vec![0f32; dim];
            let mut len = 0f32;
            for &m in &members {
                for (s, x) in sum.iter_mut().zip(vs[m].iter()) {
                    *s += x;
                }
                len += norm(&vs[m]);
            }
            let l = norm(&sum);
            let scale = if l > 0.0 { len / members.len() as f32 / l } else { 0.0 };
            sum.iter().map(|x| x * scale).collect::<Vec<f32>>().into()
        })
        .collect()
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

    #[test]
    fn pooling_merges_near_copies_and_is_a_fixed_point() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        let dim = 16;
        // Ten directions, each written four times with a little noise.
        let base: Vec<Arc<[f32]>> = (0..10).map(|_| unit(&mut rng, dim)).collect();
        let vs: Vec<Arc<[f32]>> = base
            .iter()
            .flat_map(|b| {
                (0..4)
                    .map(|_| {
                        let v: Vec<f32> = b.iter().map(|x| x + rng.gen_range(-0.02f32..0.02)).collect();
                        Arc::from(v)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let pooled = pool(&vs, 0.95);
        assert_eq!(pooled.len(), 10);
        for p in &pooled {
            let best = base.iter().map(|b| dot_f32(b, p) / dot_f32(p, p).sqrt()).fold(f32::MIN, f32::max);
            assert!(best > 0.99, "{best}");
            assert!((dot_f32(p, p).sqrt() - 1.0).abs() < 0.05);
        }
        assert_eq!(pool(&pooled, 0.95), pooled, "pooled vectors pool to themselves");
        assert_eq!(pool(&vs, 1.0).len(), vs.len(), "only equal directions merge at 1");
        assert_eq!(pool(&vs[..1], 0.5), vs[..1].to_vec());
    }

    /// Ward's clustering by merging the cheapest pair again and again,
    /// from scratch each time: the groups `ward` should find.
    fn ward_by_brute_force(vs: &[Arc<[f32]>], target: usize) -> Vec<Vec<usize>> {
        let unit: Vec<Vec<f64>> = vs
            .iter()
            .map(|v| {
                let l = v.iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>().sqrt();
                v.iter().map(|&x| x as f64 / l).collect()
            })
            .collect();
        let mut groups: Vec<Vec<usize>> = (0..vs.len()).map(|i| vec![i]).collect();
        let mean = |g: &[usize]| -> Vec<f64> { (0..unit[0].len()).map(|d| g.iter().map(|&i| unit[i][d]).sum::<f64>() / g.len() as f64).collect() };
        while groups.len() > target {
            let mut best = (f64::INFINITY, 0, 0);
            for a in 0..groups.len() {
                for b in a + 1..groups.len() {
                    let (ma, mb) = (mean(&groups[a]), mean(&groups[b]));
                    let (na, nb) = (groups[a].len() as f64, groups[b].len() as f64);
                    let cost = na * nb / (na + nb) * ma.iter().zip(&mb).map(|(x, y)| (x - y) * (x - y)).sum::<f64>();
                    if cost < best.0 {
                        best = (cost, a, b);
                    }
                }
            }
            let gone = groups.remove(best.2);
            groups[best.1].extend(gone);
            groups[best.1].sort();
        }
        groups.sort();
        groups
    }

    #[test]
    fn ward_pooling_finds_the_groups_merging_the_closest_pairs_finds() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(11);
        for (n, dim, factor) in [(9, 4, 2.0), (40, 8, 3.0), (64, 16, 2.0), (31, 3, 4.0), (100, 32, 1.5)] {
            let vs: Vec<Arc<[f32]>> = (0..n).map(|_| unit(&mut rng, dim)).collect();
            let target = (n as f32 / factor).ceil() as usize;
            let want = ward_by_brute_force(&vs, target);
            let got = ward(&vs, factor);
            assert_eq!(got.len(), target, "n {n}, factor {factor}");
            // Each pooled vector is its group's mean direction at unit length.
            for (g, v) in want.iter().zip(&got) {
                let mut sum = vec![0f32; dim];
                for &i in g {
                    for d in 0..dim {
                        sum[d] += vs[i][d];
                    }
                }
                let l = dot_f32(&sum, &sum).sqrt();
                let cos = dot_f32(&sum, v) / l / dot_f32(v, v).sqrt();
                assert!(cos > 0.9999 && (dot_f32(v, v).sqrt() - 1.0).abs() < 1e-4, "n {n}: group {g:?}: cos {cos}");
            }
        }
    }

    #[test]
    fn ward_pooling_keeps_near_copies_together_and_small_rows_whole() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(5);
        let dim = 16;
        // Ten directions, each written four times with a little noise:
        // a factor of 4 keeps one vector per direction.
        let base: Vec<Arc<[f32]>> = (0..10).map(|_| unit(&mut rng, dim)).collect();
        let vs: Vec<Arc<[f32]>> = base
            .iter()
            .flat_map(|b| (0..4).map(|_| Arc::from(b.iter().map(|x| x + rng.gen_range(-0.02f32..0.02)).collect::<Vec<f32>>())).collect::<Vec<_>>())
            .collect();
        let pooled = ward(&vs, 4.0);
        assert_eq!(pooled.len(), 10);
        for (p, b) in pooled.iter().zip(&base) {
            assert!(dot_f32(b, p) / dot_f32(p, p).sqrt() > 0.999, "groups keep their first member's order");
        }
        // Two vectors at a factor of 2 merge; one stays; 3 at 2 make 2.
        assert_eq!(ward(&vs[..2], 2.0).len(), 1);
        assert_eq!(ward(&vs[..1], 2.0), vs[..1].to_vec());
        assert_eq!(ward(&vs[..3], 2.0).len(), 2);
        assert_eq!(ward(&vs, 1.0), vs, "a factor of 1 keeps every vector");
        // Rows longer than a part are pooled a part at a time.
        let long: Vec<Arc<[f32]>> = (0..WARD_PART + 10).map(|_| unit(&mut rng, 4)).collect();
        assert_eq!(ward(&long, 2.0).len(), (WARD_PART + 10).div_ceil(2) + 1);
    }

    #[test]
    fn values_normalize_to_lists_of_vectors() {
        let f = Field::new("p", FieldType::Vector { dim: 2, index: crate::schema::VectorIndexKind::Flat }).multi();
        let want = Value::List(vec![Value::Vector(vec![1.0, 2.0].into()), Value::Vector(vec![3.0, 4.0].into())]);
        assert_eq!(normalize(&f, Value::Str("[[1, 2], [3, 4]]".into())).unwrap(), want);
        let lists = Value::List(vec![Value::List(vec![Value::Int(1), Value::Float(2.0)]), Value::Vector(vec![3.0, 4.0].into())]);
        assert_eq!(normalize(&f, lists).unwrap(), want);
        assert_eq!(normalize(&f, Value::Vector(vec![1.0, 2.0].into())).unwrap(), Value::List(vec![Value::Vector(vec![1.0, 2.0].into())]));
        assert_eq!(normalize(&f, Value::List(vec![Value::Int(1), Value::Int(2)])).unwrap(), Value::List(vec![Value::Vector(vec![1.0, 2.0].into())]));
        assert_eq!(normalize(&f, Value::List(Vec::new())).unwrap(), Value::Null);
        assert_eq!(normalize(&f, Value::Null).unwrap(), Value::Null);
        for bad in [Value::Str("[1, 2]".into()), Value::Str("[[1, 2, 3]]".into()), Value::Int(3), Value::List(vec![Value::Str("x".into())])] {
            assert!(normalize(&f, bad.clone()).is_err(), "{bad:?}");
        }
    }
}
