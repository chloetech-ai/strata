//! Merging the answers of shards.
//!
//! A collection split across several databases (shards) answers a query in
//! two rounds. The coordinator first gathers the BM25 statistics of the
//! query's terms from every shard ([`crate::db::Db::term_stats`]) and sums
//! them ([`TermStats::sum`]). It then runs the query on every shard with
//! that sum ([`ExecOptions::stats`]) in shard mode ([`ExecOptions::shard`]),
//! and merges the answers with [`merge`]:
//!
//! * ranked rows (BM25, similarity, a column) by score, ties by key;
//! * fused scores from each input's raw scores: every input's top `depth`
//!   over all shards, then RRF or the weighted sum over those candidates,
//!   normalised over the merged candidates as one database would;
//! * facet groups and counts by summing.
//!
//! When the shards hold the same segments as one database would, the
//! merged answer is that database's: the same keys in the same order, and
//! the same scores up to floating-point rounding. Rows that tie exactly may
//! come in another order, since one database breaks some ties by segment
//! and docID. Graph queries are refused in shard mode, because edges cross
//! shards.
//!
//! [`ExecOptions::stats`]: crate::query::exec::ExecOptions::stats
//! [`ExecOptions::shard`]: crate::query::exec::ExecOptions::shard
//! [`TermStats::sum`]: crate::query::stats::TermStats::sum

use std::cmp::Ordering;

use crate::error::{Error, Result};
use crate::query::exec::{cmp_values, ExecStats, ResultSet, DEFAULT_K};
use crate::query::agg;
use crate::query::fusion::{self, Norm};
use crate::query::logical::{OrderBy, Projection, Score, Select, Tiebreak};
use crate::value::Value;

fn err(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

/// Merges the shard-mode answers of every shard to `q` into the answer of
/// the whole collection.
pub fn merge(q: &Select, shards: Vec<ResultSet>) -> Result<ResultSet> {
    let columns: Vec<String> = q.projection.iter().map(|(n, _)| n.clone()).collect();
    let mut stats = ExecStats::default();
    for (i, s) in shards.iter().enumerate() {
        stats.filter_docs += s.stats.filter_docs;
        stats.scored += s.stats.scored;
        stats.plan.extend(s.stats.plan.iter().map(|l| format!("shard {i}: {l}")));
    }
    let empty = |rows, stats| ResultSet { columns: columns.clone(), rows, keys: Vec::new(), scores: Vec::new(), parts: None, stats };
    // Groups and aggregates: each shard returns its groups with partial
    // aggregates.
    if !q.group_by.is_empty() || agg::is_general(q, |_| false) {
        let needed = agg::needed(q);
        let mut groups = agg::Groups::default();
        for s in &shards {
            groups.merge_rows(&s.rows, q.group_by.len(), &needed)?;
        }
        return Ok(empty(agg::finish(q, groups, &needed), stats));
    }
    if q.projection.iter().any(|(_, p)| *p == Projection::Count) {
        let n: i64 = shards.iter().filter_map(|s| s.rows.first().and_then(|r| r.first()).and_then(|v| v.as_i64())).sum();
        let rows = if q.offset == 0 && q.limit != Some(0) { vec![vec![Value::Int(n); columns.len()]] } else { Vec::new() };
        return Ok(empty(rows, stats));
    }
    let mut rows: Vec<Row> = Vec::new();
    for (si, s) in shards.into_iter().enumerate() {
        if s.keys.len() != s.rows.len() || s.scores.len() != s.rows.len() {
            return Err(err(format!("shard {si} answered without keys and scores (not in shard mode?)")));
        }
        let parts = s.parts.map(|p| p.scores.into_iter().zip(p.ranked));
        let mut parts = parts.map(|it| it.collect::<Vec<_>>().into_iter());
        for ((values, key), score) in s.rows.into_iter().zip(s.keys).zip(s.scores) {
            let (inputs, ranked) = parts.as_mut().and_then(|p| p.next()).unwrap_or_default();
            rows.push(Row { values, key, score, inputs, ranked });
        }
    }
    let key_desc = matches!(q.tiebreak, Some(Tiebreak::Key { desc: true }));
    let (mut rows, want) = match &q.order {
        None => (rows, q.limit),
        Some(OrderBy::Score { score: score @ (Score::Rrf { .. } | Score::Weighted(_) | Score::Product(_)), .. }) => {
            let want = q.limit.unwrap_or(DEFAULT_K);
            (fuse(score, rows, want.saturating_add(q.offset))?, Some(want))
        }
        Some(OrderBy::Score { score, desc, .. }) => {
            let desc = *desc || !matches!(score, Score::Column { .. });
            rows.sort_by(|a, b| by_score(a.score, b.score, desc).then_with(|| by_key(&a.key, &b.key, key_desc)));
            let want = if matches!(score, Score::Column { .. }) { q.limit } else { Some(q.limit.unwrap_or(DEFAULT_K)) };
            (rows, want)
        }
        Some(_) => return Err(err("ORDER BY COUNT(*) needs GROUP BY")),
    };
    if matches!(&q.order, Some(OrderBy::Score { score: Score::Rrf { .. } | Score::Weighted(_) | Score::Product(_), .. })) {
        rows.sort_by(|a, b| by_score(a.score, b.score, true).then_with(|| by_key(&a.key, &b.key, key_desc)));
    }
    let end = want.map_or(rows.len(), |w| w.saturating_add(q.offset).min(rows.len()));
    let rows: Vec<Row> = rows.into_iter().take(end).skip(q.offset).collect();
    let score_cols: Vec<usize> = q.projection.iter().enumerate().filter(|(_, (_, p))| *p == Projection::Score).map(|(i, _)| i).collect();
    let mut out = empty(Vec::with_capacity(rows.len()), stats);
    for mut r in rows {
        for &c in &score_cols {
            // Rows without the ordering column's value score ±inf.
            r.values[c] = if r.score.is_finite() { Value::Float(r.score) } else { Value::Null };
        }
        out.keys.push(r.key);
        out.scores.push(r.score);
        out.rows.push(r.values);
    }
    Ok(out)
}

/// One row of a shard's answer.
struct Row {
    values: Vec<Value>,
    key: Value,
    score: f64,
    /// Fused scores: each input's raw score, and whether the row is in the
    /// input's top `depth` of its shard.
    inputs: Vec<Option<f64>>,
    ranked: Vec<bool>,
}

fn by_score(a: f64, b: f64, desc: bool) -> Ordering {
    let o = a.total_cmp(&b);
    if desc {
        o.reverse()
    } else {
        o
    }
}

fn by_key(a: &Value, b: &Value, desc: bool) -> Ordering {
    let o = cmp_values(a, b);
    if desc {
        o.reverse()
    } else {
        o
    }
}

/// The options a fused score's inputs carry (`depth`, `exact`, `norm`).
fn fused_opt<'a>(terms: &'a [(f64, Score)], key: &str) -> Option<&'a Value> {
    terms.iter().find_map(|(_, s)| s.opts().and_then(|o| o.get(key)))
}

/// Fuses the candidates of every shard: each input's top `depth` over all
/// shards (at least the `k` rows the page needs, as each shard took), then
/// RRF or the weighted sum over the union.
fn fuse(score: &Score, rows: Vec<Row>, k: usize) -> Result<Vec<Row>> {
    let n_inputs = match score {
        Score::Rrf { inputs, .. } | Score::Product(inputs) => inputs.len(),
        Score::Weighted(t) => t.len(),
        _ => unreachable!(),
    };
    if rows.iter().any(|r| r.inputs.len() != n_inputs || r.ranked.len() != n_inputs) {
        return Err(err("shard answers lack the fused score's inputs (not in shard mode?)"));
    }
    let (depth, exact) = match score {
        Score::Rrf { depth, .. } => (*depth, false),
        Score::Weighted(t) => (
            fused_opt(t, "depth").and_then(|v| v.as_f64()).map_or(crate::query::exec::ExecOptions::default().fusion_depth, |d| d as usize),
            fused_opt(t, "exact").and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        ),
        Score::Product(f) => (
            f.iter().find_map(|s| s.opts().and_then(|o| o.get("depth")).and_then(|v| v.as_f64())).map_or(crate::query::exec::ExecOptions::default().fusion_depth, |d| d as usize),
            false,
        ),
        _ => unreachable!(),
    };
    let depth = depth.max(k);
    // Input j's ranking over all shards: its own top `depth` of each shard,
    // by its score, ties by key (as one database orders them).
    let lists: Vec<Vec<usize>> = (0..n_inputs)
        .map(|j| {
            let mut l: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].ranked[j]).collect();
            l.sort_by(|&a, &b| {
                let (x, y) = (rows[a].inputs[j].unwrap_or(f64::NEG_INFINITY), rows[b].inputs[j].unwrap_or(f64::NEG_INFINITY));
                by_score(x, y, true).then_with(|| cmp_values(&rows[a].key, &rows[b].key))
            });
            l.truncate(depth);
            l
        })
        .collect();
    let fused: Vec<(usize, f64)> = match score {
        Score::Rrf { k, .. } => fusion::rrf(&lists, *k),
        Score::Weighted(terms) => {
            let mut cands: Vec<usize> = if exact { (0..rows.len()).collect() } else { lists.concat() };
            cands.sort_unstable();
            cands.dedup();
            let norm = match fused_opt(terms, "norm").and_then(|v| v.as_str()) {
                Some("none") => Norm::None,
                _ => Norm::MinMax,
            };
            let weights: Vec<f64> = terms.iter().map(|(w, _)| *w).collect();
            let scores: Vec<Vec<f32>> =
                (0..n_inputs).map(|j| cands.iter().map(|&i| rows[i].inputs[j].map_or(f32::NEG_INFINITY, |x| x as f32)).collect()).collect();
            fusion::weighted(&weights, &scores, norm).into_iter().map(|(c, s)| (cands[c], s)).collect()
        }
        Score::Product(factors) => {
            // Candidates from the relevance factors (lists of the others
            // are empty, see `fusion_candidates`).
            let any_relevance = factors.iter().any(Score::is_relevance);
            let mut cands: Vec<usize> =
                lists.iter().zip(factors).filter(|(_, f)| f.is_relevance() || !any_relevance).flat_map(|(l, _)| l.iter().copied()).collect();
            cands.sort_unstable();
            cands.dedup();
            let scores: Vec<Vec<f32>> =
                (0..n_inputs).map(|j| cands.iter().map(|&i| rows[i].inputs[j].map_or(f32::NEG_INFINITY, |x| x as f32)).collect()).collect();
            fusion::weighted(&vec![1.0; n_inputs], &scores, Norm::Product).into_iter().map(|(c, s)| (cands[c], s)).collect()
        }
        _ => unreachable!(),
    };
    let mut slots: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
    Ok(fused
        .into_iter()
        .map(|(i, s)| {
            let mut r = slots[i].take().expect("each candidate fused once");
            r.score = s;
            r
        })
        .collect())
}
