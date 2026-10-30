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
//!   over all shards, then the fused score over those candidates
//!   ([`crate::query::rank`]), normalised over the merged candidates as one
//!   database would;
//! * facet groups and counts by summing.
//!
//! Rows can also come in two phases, query then fetch: every shard answers
//! [`ids`], the query with only the primary key selected, and the merge of
//! those answers ranks the rows as the query's would; then the shards that
//! hold the rows of the page answer [`fetch`], the query's columns of
//! those rows alone, and [`fill`] puts them in the merged order. Stored
//! fields are read for the page, not for every shard's top rows.
//!
//! Shards answer with BOOL and TIMESTAMP values as the integers stored,
//! which the merged answer gives as such ([`Select::typed`]).
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
use crate::query::exec::{cmp_values, ExecOptions, ExecStats, ResultSet, DEFAULT_K};
use crate::query::agg;
use crate::query::logical::{OrderBy, Pred, Projection, Score, Select, Tiebreak};
use crate::query::rank::Rank;
use crate::schema::Schema;
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
        let mut rows = agg::finish(q, groups, &needed)?;
        q.typed(&mut rows);
        return Ok(empty(rows, stats));
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
    q.typed(&mut out.rows);
    Ok(out)
}

/// Whether a fetch phase reads less than one round would: `q` returns a
/// page of rows (not groups or counts, and with a LIMIT, as a query
/// without one keeps every shard's rows) and selects a column besides the
/// primary key and the score, which [`ids`] answers by themselves.
pub fn fetches(q: &Select, schema: &Schema) -> bool {
    let Some(pk) = schema.primary_key else { return false };
    let rows = q.limit.is_some() && q.group_by.is_empty() && !agg::is_general(q, |_| false) && !q.projection.iter().any(|(_, p)| matches!(p, Projection::Count | Projection::Agg { .. }));
    // A BOOL or TIMESTAMP key would come back as its stored integer.
    rows && q.graphs.is_empty() && schema.fields[pk].logical.is_none() && q.projection.iter().any(|(_, p)| matches!(p, Projection::Field(f) if *f != pk))
}

/// `q` answering only its rows' primary keys (with their scores, as every
/// shard-mode answer has them): the query phase of query then fetch.
pub fn ids(q: &Select, schema: &Schema) -> Result<Select> {
    let pk = schema.primary_key.ok_or_else(|| err(format!("{} has no primary key", schema.table)))?;
    Ok(Select { projection: vec![(schema.fields[pk].name.clone(), Projection::Field(pk))], types: vec![None], ..q.clone() })
}

/// `q`'s columns of the rows with primary keys `keys`, in no order: the
/// fetch phase, which a shard answers from the snapshot it ranked from.
pub fn fetch(q: &Select, schema: &Schema, keys: Vec<Value>) -> Result<Select> {
    let pk = schema.primary_key.ok_or_else(|| err(format!("{} has no primary key", schema.table)))?;
    Ok(Select {
        projection: q.projection.clone(),
        limit: Some(keys.len()),
        filter: if keys.is_empty() { Pred::False } else { Pred::In { field: pk, values: keys } },
        order: None,
        group_by: Vec::new(),
        having: None,
        tiebreak: None,
        offset: 0,
        graphs: Vec::new(),
        types: q.types.clone(),
    })
}

/// The answer to `q`: the rows of `ranked` (the merge of the shards'
/// answers to [`ids`]) in its order, their columns from `fetched` (the
/// shards' answers to [`fetch`]) and their scores from `ranked`. A row
/// that none fetched (it changed between the phases, read from another
/// version) is left out; the count of those comes back too.
pub fn fill(q: &Select, ranked: ResultSet, fetched: Vec<ResultSet>) -> Result<(ResultSet, usize)> {
    let mut rows: std::collections::HashMap<agg::Part, Vec<Value>> = std::collections::HashMap::new();
    let mut stats = ranked.stats;
    for f in fetched {
        if f.keys.len() != f.rows.len() {
            return Err(err("a shard fetched rows without their keys (not in shard mode?)"));
        }
        stats.plan.extend(f.stats.plan.into_iter().map(|l| format!("fetch: {l}")));
        rows.extend(f.keys.iter().map(agg::Part::of).zip(f.rows));
    }
    let columns: Vec<String> = q.projection.iter().map(|(n, _)| n.clone()).collect();
    let mut out = ResultSet { columns, rows: Vec::with_capacity(ranked.keys.len()), keys: Vec::new(), scores: Vec::new(), parts: None, stats };
    let mut lost = 0;
    for (key, score) in ranked.keys.into_iter().zip(ranked.scores) {
        let Some(mut row) = rows.remove(&agg::Part::of(&key)) else {
            lost += 1;
            continue;
        };
        for (v, (_, p)) in row.iter_mut().zip(&q.projection) {
            if *p == Projection::Score {
                // Rows without the ordering column's value score ±inf.
                *v = if score.is_finite() { Value::Float(score) } else { Value::Null };
            }
        }
        out.keys.push(key);
        out.scores.push(score);
        out.rows.push(row);
    }
    q.typed(&mut out.rows);
    Ok((out, lost))
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

/// Fuses the candidates of every shard: each input's top `depth` over all
/// shards (at least the `k` rows the page needs, as each shard took), then
/// the fused score over those ([`Rank`]), as one database computes it.
fn fuse(score: &Score, rows: Vec<Row>, k: usize) -> Result<Vec<Row>> {
    let rank = Rank::of(score, ExecOptions::default().fusion_depth).expect("a fused score");
    let n_inputs = rank.leaves.len();
    if rows.iter().any(|r| r.inputs.len() != n_inputs || r.ranked.len() != n_inputs) {
        return Err(err("shard answers lack the fused score's inputs (not in shard mode?)"));
    }
    let depth = rank.depth.max(k);
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
    let cands: Vec<usize> = if rank.exact { (0..rows.len()).collect() } else { rank.candidates(&lists) };
    let values: Vec<Vec<f64>> =
        if rank.reads_values() { (0..n_inputs).map(|j| cands.iter().map(|&i| rows[i].inputs[j].unwrap_or(f64::NEG_INFINITY)).collect()).collect() } else { Vec::new() };
    let mut at = vec![usize::MAX; rows.len()];
    for (c, &i) in cands.iter().enumerate() {
        at[i] = c;
    }
    let ranks: Vec<Vec<usize>> = lists.iter().map(|l| l.iter().map(|&i| at[i]).collect()).collect();
    let fused: Vec<(usize, f64)> = rank.eval(cands.len(), &values, &ranks, cands.len()).into_iter().map(|(c, s)| (cands[c], s)).collect();
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
