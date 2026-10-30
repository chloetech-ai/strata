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
//!
//! The float32 vectors (`vec.<f>.f32`) are kept only with `keep_f32`: a
//! 1,030-vector page at 128 dimensions is 527 KB in float32, 132 KB in SQ8
//! and 16.5 KB in binary codes. Without them a row's vectors read back
//! (and merge) as their SQ8 values, and scores are SQ8 scores.
//!
//! **Search.** Each query vector runs a filtered vector search over the
//! vectors ([`DEPTH`] best, with the field's index and strategies); the
//! rows that own them are the candidates, at most [`CANDIDATES`] of them,
//! kept by the sum of the best match each query vector found in them.
//! Each candidate is then scored exactly over all its vectors.

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
    let mut vs = items.into_iter().map(one).collect::<Result<Vec<_>>>()?;
    if let Some(min) = field.pool {
        vs = pool(&vs, min);
    }
    Ok(Value::List(vs.into_iter().map(Value::Vector).collect()))
}

/// Token pooling: merges the two most similar groups of `vs` (by the cosine
/// of their means) while that cosine is at least `min_cos`. A group's
/// vector is the mean of its members, scaled to their mean length. No two
/// of the results are that similar, so pooling them again keeps them: a
/// row read back and written again (an update, a merge) is not pooled
/// twice.
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
