//! Fused scores: a score made of others (RRF, a weighted sum, a product) as
//! one expression over its leaves, and the one evaluator of that
//! expression, which a database and a coordinator both run.
//!
//! A database ranks by a fused score in three steps
//! ([`crate::query::exec`]):
//!
//! 1. **Candidates.** The top `depth` rows of each leaf that generates them
//!    (every input of RRF and of a weighted sum; a product's relevance
//!    factors, or all its factors when none is), or with `exact => 1` every
//!    row that passes `WHERE` ([`Rank::candidates`]).
//! 2. **Values.** Each leaf's exact value of every candidate, where the
//!    expression reads values and not only rankings ([`Rank::reads_values`]).
//! 3. **The expression** over those values and the leaves' rankings
//!    ([`Rank::eval`]).
//!
//! A shard answers every candidate with each leaf's value and whether the row
//! is in the leaf's top `depth` ([`crate::query::exec::FusionParts`]). The
//! coordinator takes each leaf's top `depth` over all shards and runs steps 1
//! and 3 over those rows with the same code ([`crate::cluster`]), so a split
//! collection ranks like the whole one.

use std::borrow::Cow;
use std::cmp::Ordering;

use crate::query::fusion::{self, Norm};
use crate::query::logical::Score;
use crate::schema::Schema;

/// A fused score, ready to run.
#[derive(Clone, Debug)]
pub struct Rank<'s> {
    /// The scores the expression reads, each of one row by itself.
    pub leaves: Vec<&'s Score>,
    /// Whether each leaf's top rows are candidates.
    pub generates: Vec<bool>,
    /// The top rows each generating leaf gives, before the query raises it
    /// to the rows its page needs.
    pub depth: usize,
    /// Every row that passes `WHERE` is a candidate.
    pub exact: bool,
    pub expr: Expr,
}

/// An expression over a [`Rank`]'s leaves and their rankings, computed for a
/// set of candidates.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A leaf's exact value; not finite where the row has none (a NULL
    /// column).
    Leaf(usize),
    /// `Σ wᵢ · eᵢ`, each term min-max normalised over the candidates
    /// ([`Norm::MinMax`]) or raw ([`Norm::None`]). A missing value counts as
    /// the worst of its term over the candidates ([`fusion::combine`]).
    Sum { terms: Vec<(f64, Expr)>, norm: Norm },
    /// `Π eᵢ`; a factor below 0, or missing, counts as 0.
    Product(Vec<Expr>),
    /// `Σ 1 / (k + rank)` over the leaves' rankings of their top rows, ranks
    /// from 1; a row gets nothing from a ranking it is not in.
    Rrf { leaves: Vec<usize>, k: f64 },
}

impl<'s> Rank<'s> {
    /// The fused form of `score`, or `None` for a leaf. `default_depth` is
    /// the depth of a weighted sum or product whose leaves set none.
    pub fn of(score: &'s Score, default_depth: usize) -> Option<Rank<'s>> {
        Some(match score {
            Score::Rrf { inputs, k, depth } => Rank {
                leaves: inputs.iter().collect(),
                generates: vec![true; inputs.len()],
                depth: *depth,
                exact: false,
                expr: Expr::Rrf { leaves: (0..inputs.len()).collect(), k: *k },
            },
            Score::Weighted(terms) => {
                let leaves: Vec<&Score> = terms.iter().map(|(_, s)| s).collect();
                // An option holds for the whole sum, given on any term.
                let opt = |key: &str| leaves.iter().find_map(|s| s.opts().and_then(|o| o.get(key)));
                let norm = match opt("norm").and_then(|v| v.as_str()) {
                    Some("none") => Norm::None,
                    _ => Norm::MinMax,
                };
                Rank {
                    generates: vec![true; leaves.len()],
                    depth: opt("depth").and_then(|v| v.as_f64()).map_or(default_depth, |d| d as usize),
                    exact: opt("exact").and_then(|v| v.as_i64()).unwrap_or(0) != 0,
                    expr: Expr::Sum { terms: terms.iter().enumerate().map(|(i, (w, _))| (*w, Expr::Leaf(i))).collect(), norm },
                    leaves,
                }
            }
            Score::Product(factors) => {
                let any_relevance = factors.iter().any(Score::is_relevance);
                Rank {
                    leaves: factors.iter().collect(),
                    generates: factors.iter().map(|f| f.is_relevance() || !any_relevance).collect(),
                    depth: factors.iter().find_map(|s| s.opts().and_then(|o| o.get("depth")).and_then(|v| v.as_f64())).map_or(default_depth, |d| d as usize),
                    exact: false,
                    expr: Expr::Product((0..factors.len()).map(Expr::Leaf).collect()),
                }
            }
            _ => return None,
        })
    }

    /// Whether the expression reads the leaves' values; RRF reads only
    /// their rankings.
    pub fn reads_values(&self) -> bool {
        fn reads(e: &Expr) -> bool {
            match e {
                Expr::Leaf(_) => true,
                Expr::Sum { terms, .. } => terms.iter().any(|(_, e)| reads(e)),
                Expr::Product(fs) => fs.iter().any(reads),
                Expr::Rrf { .. } => false,
            }
        }
        reads(&self.expr)
    }

    /// The candidates of a fused score that is not `exact`: the rows in the
    /// top rows of a generating leaf (`lists`, one per leaf), each once, in
    /// order.
    pub fn candidates<K: Copy + Ord>(&self, lists: &[Vec<K>]) -> Vec<K> {
        let mut cands: Vec<K> = lists.iter().zip(&self.generates).filter(|(_, g)| **g).flat_map(|(l, _)| l.iter().copied()).collect();
        cands.sort_unstable();
        cands.dedup();
        cands
    }

    /// The fused score of the best `keep` of `n` candidates and of every
    /// candidate tied on score with the last of them, best first:
    /// `values[j]` is leaf `j`'s value of each candidate (empty when the
    /// expression [reads](Self::reads_values) none), and `lists[j]` the
    /// candidates in leaf `j`'s top rows, best first. Rows tied on score
    /// come in the order they first appear in a ranking under RRF, in
    /// candidate order otherwise. Every score is finite.
    pub fn eval(&self, n: usize, values: &[Vec<f64>], lists: &[Vec<usize>], keep: usize) -> Vec<(usize, f64)> {
        let mut out: Vec<(usize, f64)> = self.values(&self.expr, n, values, lists).iter().copied().enumerate().collect();
        if let Expr::Rrf { leaves, .. } = &self.expr {
            let mut first = vec![usize::MAX; n];
            let mut seen = 0;
            for &c in leaves.iter().flat_map(|&j| &lists[j]) {
                if first[c] == usize::MAX {
                    first[c] = seen;
                    seen += 1;
                }
            }
            best_first(&mut out, keep, |a, b| b.1.total_cmp(&a.1).then(first[a.0].cmp(&first[b.0])));
        } else {
            // A sum over every row `WHERE` keeps can tie on most of them:
            // no lookup per comparison.
            best_first(&mut out, keep, |a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        }
        out
    }

    /// The value of `e` for each candidate; a leaf's are borrowed.
    fn values<'v>(&self, e: &Expr, n: usize, values: &'v [Vec<f64>], lists: &[Vec<usize>]) -> Cow<'v, [f64]> {
        match e {
            Expr::Leaf(j) => Cow::Borrowed(&values[*j]),
            Expr::Sum { terms, norm } => {
                let weights: Vec<f64> = terms.iter().map(|(w, _)| *w).collect();
                let inputs: Vec<Cow<[f64]>> = terms.iter().map(|(_, e)| self.values(e, n, values, lists)).collect();
                Cow::Owned(fusion::combine(&weights, &inputs, *norm))
            }
            Expr::Product(fs) => {
                let inputs: Vec<Cow<[f64]>> = fs.iter().map(|e| self.values(e, n, values, lists)).collect();
                Cow::Owned(fusion::combine(&vec![1.0; fs.len()], &inputs, Norm::Product))
            }
            Expr::Rrf { leaves, k } => {
                let mut out = vec![0.0; n];
                for &j in leaves {
                    for (i, &c) in lists[j].iter().enumerate() {
                        out[c] += 1.0 / (k + (i + 1) as f64);
                    }
                }
                Cow::Owned(out)
            }
        }
    }

    /// The expression as SQL would write it with normalisation explicit,
    /// leaves by field (`0.3 * minmax(bm25(title)) + 0.7 * minmax(similarity(vec))`),
    /// for plan lines.
    pub fn explain(&self, schema: &Schema) -> String {
        self.show(&self.expr, schema)
    }

    fn show(&self, e: &Expr, schema: &Schema) -> String {
        match e {
            Expr::Leaf(j) => leaf_name(self.leaves[*j], schema),
            Expr::Sum { terms, norm } => terms
                .iter()
                .map(|(w, e)| match (norm, e) {
                    (Norm::MinMax, e) => format!("{w} * minmax({})", self.show(e, schema)),
                    (_, e @ Expr::Sum { .. }) => format!("{w} * ({})", self.show(e, schema)),
                    (_, e) => format!("{w} * {}", self.show(e, schema)),
                })
                .collect::<Vec<_>>()
                .join(" + "),
            Expr::Product(fs) => fs
                .iter()
                .map(|e| match e {
                    Expr::Sum { .. } => format!("({})", self.show(e, schema)),
                    e => self.show(e, schema),
                })
                .collect::<Vec<_>>()
                .join(" * "),
            Expr::Rrf { leaves, k } => format!("rrf({}, k => {k})", leaves.iter().map(|&j| leaf_name(self.leaves[j], schema)).collect::<Vec<_>>().join(", ")),
        }
    }
}

/// A leaf as plan lines name it: its function and field.
fn leaf_name(s: &Score, schema: &Schema) -> String {
    let name = |f: &usize| schema.fields.get(*f).map_or("?", |f| f.name.as_str());
    match s {
        Score::Bm25 { field, .. } => format!("bm25({})", name(field)),
        Score::Similarity { field, .. } | Score::Sparse { field, .. } => format!("similarity({})", name(field)),
        Score::Column { field } => name(field).to_string(),
        Score::Decay { field, .. } => format!("decay({})", name(field)),
        Score::Ppr { .. } => "ppr()".into(),
        Score::Hops { .. } => "hops()".into(),
        Score::Rrf { .. } | Score::Weighted(_) | Score::Product(_) => "(a fused score)".into(),
    }
}

/// Orders `rows` by `cmp`, a total order that ranks by score first, and
/// keeps its best `keep` rows and every row tied on score with the last of
/// them: the prefix of the whole order that a page of `keep` rows can reach,
/// without ordering the rest.
fn best_first(rows: &mut Vec<(usize, f64)>, keep: usize, cmp: impl Fn(&(usize, f64), &(usize, f64)) -> Ordering) {
    if keep == 0 {
        rows.clear();
        return;
    }
    if keep < rows.len() {
        rows.select_nth_unstable_by(keep - 1, &cmp);
        let cut = rows[keep - 1].1;
        let mut end = keep;
        for i in keep..rows.len() {
            if rows[i].1.total_cmp(&cut).is_eq() {
                rows.swap(i, end);
                end += 1;
            }
        }
        rows.truncate(end);
    }
    rows.sort_unstable_by(&cmp);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::logical::Opts;
    use crate::schema::{Field, FieldType};

    fn bm25(field: usize) -> Score {
        Score::Bm25 { field, text: "x".into(), opts: Opts::default() }
    }

    #[test]
    fn today_s_shapes_compile_to_their_explicit_forms() {
        let schema = Schema::new(
            "t",
            vec![Field::new("id", FieldType::Keyword), Field::new("body", FieldType::Text { positions: false }), Field::new("rating", FieldType::Float)],
            Some("id"),
        )
        .unwrap();
        let w = Score::Weighted(vec![(0.3, bm25(1)), (0.7, Score::Column { field: 2 })]);
        let r = Rank::of(&w, 100).unwrap();
        assert_eq!(r.explain(&schema), "0.3 * minmax(bm25(body)) + 0.7 * minmax(rating)");
        assert_eq!((r.depth, r.exact, r.generates.clone()), (100, false, vec![true, true]));
        assert!(r.reads_values());

        let p = Score::Product(vec![bm25(1), Score::Column { field: 2 }]);
        let r = Rank::of(&p, 100).unwrap();
        assert_eq!(r.explain(&schema), "bm25(body) * rating");
        // A product's candidates come from its relevance factors.
        assert_eq!(r.generates, vec![true, false]);

        let f = Score::Rrf { inputs: vec![bm25(1), Score::Column { field: 2 }], k: 60.0, depth: 30 };
        let r = Rank::of(&f, 100).unwrap();
        assert_eq!(r.explain(&schema), "rrf(bm25(body), rating, k => 60)");
        assert_eq!(r.depth, 30);
        assert!(!r.reads_values());
        assert!(Rank::of(&bm25(1), 100).is_none());
    }

    #[test]
    fn eval_matches_the_fusion_functions() {
        // Weighted sums and products: fusion::weighted's scores and order.
        let s0 = vec![1.0f32, 3.0, 2.0, 3.0];
        let s1 = vec![10.0f32, f32::NEG_INFINITY, 20.0, 5.0];
        let values: Vec<Vec<f64>> = [&s0, &s1].iter().map(|s| s.iter().map(|&x| f64::from(x)).collect()).collect();
        for (score, norm) in [
            (Score::Weighted(vec![(0.7, bm25(0)), (0.3, bm25(1))]), Norm::MinMax),
            (Score::Weighted(vec![(1.0, bm25(0)), (-0.1, bm25(1))]), Norm::MinMax),
            (Score::Product(vec![bm25(0), bm25(1)]), Norm::Product),
        ] {
            let r = Rank::of(&score, 100).unwrap();
            let weights: Vec<f64> = match &score {
                Score::Weighted(t) => t.iter().map(|(w, _)| *w).collect(),
                _ => vec![1.0, 1.0],
            };
            assert_eq!(r.eval(4, &values, &[vec![], vec![]], 4), fusion::weighted(&weights, &[s0.clone(), s1.clone()], norm), "{score:?}");
        }
        // RRF: fusion::rrf's scores and order, ties by first appearance.
        let lists = vec![vec![2, 0, 1], vec![1, 3, 2]];
        let f = Score::Rrf { inputs: vec![bm25(0), bm25(1)], k: 60.0, depth: 3 };
        assert_eq!(Rank::of(&f, 100).unwrap().eval(4, &[], &lists, 4), fusion::rrf(&lists, 60.0));
    }

    #[test]
    fn eval_keeps_the_page_and_the_tie_at_its_cut() {
        // Scores 5, 4, 4, 4, 3, 2 in a scrambled candidate order.
        let v = vec![4.0, 3.0, 4.0, 5.0, 2.0, 4.0];
        let w = Score::Weighted(vec![(1.0, bm25(0)), (0.0, bm25(1))]);
        let r = Rank::of(&w, 100).unwrap();
        let values = vec![v.clone(), vec![0.0; 6]];
        let all = r.eval(6, &values, &[vec![], vec![]], 6);
        assert_eq!(all.iter().map(|x| x.0).collect::<Vec<_>>(), vec![3, 0, 2, 5, 1, 4]);
        for (keep, want) in [(0, 0), (1, 1), (2, 4), (3, 4), (4, 4), (5, 5), (6, 6), (9, 6)] {
            assert_eq!(r.eval(6, &values, &[vec![], vec![]], keep), all[..want], "keep {keep}");
        }
    }
}
