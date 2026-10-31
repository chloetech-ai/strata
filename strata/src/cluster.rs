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
//! A query that reranks by a cross-encoder ([`Select::model_phase`]) runs
//! it once, on the coordinator: shards leave it NULL, the merge ranks up to
//! its phase and keeps that phase's rows, the fetch round reads their text
//! with the query's columns ([`with_texts`]), and [`rerank_models`] scores
//! them and ranks the phases from there on.
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
use crate::query::exec::{cmp_values, ExecOptions, ExecStats, RankParts, ResultSet, DEFAULT_K};
use crate::query::agg;
use crate::query::steps::Clock;
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
    // The coordinator scores rows with the models too.
    if let Some(m) = q.unopened_model(false) {
        return Err(Error::Query(format!("ranking model {m} is not open on the coordinator: it opens a plan's models from its models directory")));
    }
    let columns: Vec<String> = q.projection.iter().map(|(n, _)| n.clone()).collect();
    let mut stats = ExecStats::default();
    for (i, s) in shards.iter().enumerate() {
        stats.filter_docs += s.stats.filter_docs;
        stats.scored += s.stats.scored;
        stats.reranked += s.stats.reranked;
        stats.plan.extend(s.stats.plan.iter().map(|l| format!("shard {i}: {l}")));
        // Each shard's steps, which its caller put on one clock.
        stats.nest(&s.stats, 0);
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
        let (mut inputs, mut phases) = match s.parts {
            Some(p) => (p.scores.into_iter().zip(p.ranked).collect::<Vec<_>>().into_iter(), p.phases.into_iter()),
            None => (Vec::new().into_iter(), Vec::new().into_iter()),
        };
        for ((values, key), score) in s.rows.into_iter().zip(s.keys).zip(s.scores) {
            let (inputs, ranked) = inputs.next().unwrap_or_default();
            let phases = phases.next().unwrap_or_default();
            if phases.len() != q.phases.len() {
                return Err(err(format!("shard {si} answered without what its rerank phases need (not in shard mode?)")));
            }
            rows.push(Row { values, key, score, inputs, ranked, phases });
        }
    }
    let key_desc = matches!(q.tiebreak, Some(Tiebreak::Key { desc: true }));
    // The first phase of a query with rerank phases ranks the top `count`
    // rows, as a query with that LIMIT.
    let first = q.phases.first().map(|p| p.count);
    let (mut rows, want) = match &q.order {
        None => (rows, q.limit),
        Some(OrderBy::Score { score, .. }) if Rank::of(score, 0).is_some() => {
            let want = q.limit.unwrap_or(DEFAULT_K);
            (fuse(score, rows, first.unwrap_or(want.saturating_add(q.offset)))?, Some(want))
        }
        Some(OrderBy::Score { score, desc, .. }) => {
            let desc = *desc || !matches!(score, Score::Column { .. });
            rows.sort_by(|a, b| by_score(a.score, b.score, desc).then_with(|| by_key(&a.key, &b.key, key_desc)));
            let want = if matches!(score, Score::Column { .. }) { q.limit } else { Some(q.limit.unwrap_or(DEFAULT_K)) };
            (rows, want)
        }
        Some(_) => return Err(err("ORDER BY COUNT(*) needs GROUP BY")),
    };
    if matches!(&q.order, Some(OrderBy::Score { score, .. }) if Rank::of(score, 0).is_some()) {
        rows.sort_by(|a, b| by_score(a.score, b.score, true).then_with(|| by_key(&a.key, &b.key, key_desc)));
    }
    let want = match (first, q.model_phase()) {
        // The rows of a cross-encoder's phase, which the coordinator
        // scores over their text ([`rerank_models`]).
        (Some(count), Some(m)) => {
            rows.truncate(count);
            rows = rerank(q, rows, 0..m, &mut stats);
            rows.truncate(q.phases[m].count);
            stats.plan.push(format!("phase {}: {} rows for the cross-encoder, their text fetched", m + 1, rows.len()));
            let mut out = empty(Vec::with_capacity(rows.len()), stats);
            let mut phases = Vec::with_capacity(rows.len());
            for r in rows {
                out.keys.push(r.key);
                out.scores.push(r.score);
                out.rows.push(r.values);
                phases.push(r.phases);
            }
            out.parts = Some(RankParts { phases, ..RankParts::default() });
            q.typed(&mut out.rows);
            return Ok(out);
        }
        (Some(count), None) => {
            rows.truncate(count);
            rows = rerank(q, rows, 0..q.phases.len(), &mut stats);
            q.limit
        }
        (None, _) => want,
    };
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

/// Whether `q` scores BM25 (a `bm25` leaf of its ranking, alone or in an
/// expression), so that a coordinator sums its shards' statistics of the
/// terms first ([`ExecOptions::stats`]) and they score like one database.
pub fn scores_bm25(q: &Select) -> bool {
    let mut leaves = Vec::new();
    crate::query::exec::bm25_leaves(q, &mut leaves);
    !leaves.is_empty()
}

/// Whether a fetch phase reads less than one round would: `q` returns a
/// page of rows (not groups or counts, and with a LIMIT, as a query
/// without one keeps every shard's rows) and selects a column besides the
/// primary key and the score, which [`ids`] answers by themselves: a field,
/// or a ranking expression, which the fetch then computes for the page
/// alone (with the query's BM25 statistics, [`ExecOptions::stats`]).
pub fn fetches(q: &Select, schema: &Schema) -> bool {
    let Some(pk) = schema.primary_key else { return false };
    let rows = q.limit.is_some() && q.group_by.is_empty() && !agg::is_general(q, |_| false) && !q.projection.iter().any(|(_, p)| matches!(p, Projection::Count | Projection::Agg { .. }));
    // A BOOL or TIMESTAMP key would come back as its stored integer.
    rows && q.graphs.is_empty()
        && schema.fields[pk].logical.is_none()
        && q.projection.iter().any(|(_, p)| matches!(p, Projection::Field(f) if *f != pk) || matches!(p, Projection::Feature(_)))
}

/// `q` answering only its rows' primary keys (with their scores, as every
/// shard-mode answer has them): the query phase of query then fetch.
pub fn ids(q: &Select, schema: &Schema) -> Result<Select> {
    let pk = schema.primary_key.ok_or_else(|| err(format!("{} has no primary key", schema.table)))?;
    Ok(Select { projection: vec![(schema.fields[pk].name.clone(), Projection::Field(pk))], types: vec![None], ..q.clone() })
}

/// `q`'s columns of the rows with primary keys `keys`, in no order: the
/// fetch phase, which a shard answers from the snapshot it ranked from,
/// with the BM25 statistics the query ranked with when a column scores
/// BM25 ([`scores_bm25`]).
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
        phases: Vec::new(),
        offset: 0,
        graphs: Vec::new(),
        types: q.types.clone(),
        guard: None,
    })
}

/// The answer to `q`: the rows of `ranked` (the merge of the shards'
/// answers to [`ids`]) in its order, their columns from `fetched` (the
/// shards' answers to [`fetch`]) and their scores from `ranked`, and what
/// they rerank by in the phases left ([`RankParts::phases`], for
/// [`rerank_models`]). A row that none fetched (it changed between the
/// phases, read from another version) is left out; the count of those
/// comes back too.
pub fn fill(q: &Select, ranked: ResultSet, fetched: Vec<ResultSet>) -> Result<(ResultSet, usize)> {
    let mut rows: std::collections::HashMap<agg::Part, Vec<Value>> = std::collections::HashMap::new();
    let mut stats = ranked.stats;
    for f in fetched {
        if f.keys.len() != f.rows.len() {
            return Err(err("a shard fetched rows without their keys (not in shard mode?)"));
        }
        stats.plan.extend(f.stats.plan.iter().map(|l| format!("fetch: {l}")));
        stats.nest(&f.stats, 0);
        rows.extend(f.keys.iter().map(agg::Part::of).zip(f.rows));
    }
    let columns: Vec<String> = q.projection.iter().map(|(n, _)| n.clone()).collect();
    let mut out = ResultSet { columns, rows: Vec::with_capacity(ranked.keys.len()), keys: Vec::new(), scores: Vec::new(), parts: None, stats };
    let mut phases = ranked.parts.map(|p| p.phases.into_iter());
    let mut lost = 0;
    for (key, score) in ranked.keys.into_iter().zip(ranked.scores) {
        let these = phases.as_mut().and_then(|p| p.next());
        let Some(mut row) = rows.remove(&agg::Part::of(&key)) else {
            lost += 1;
            continue;
        };
        if let Some(p) = these {
            out.parts.get_or_insert_with(RankParts::default).phases.push(p);
        }
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

/// The fields whose text the cross-encoders of `q`'s phases read, each
/// once, in order: the columns [`with_texts`] adds.
fn model_fields(q: &Select) -> Vec<usize> {
    let mut out = Vec::new();
    for p in &q.phases {
        p.score.visit(&mut |s| {
            if let Score::CrossEncoder { fields, .. } = s {
                for f in fields {
                    if !out.contains(f) {
                        out.push(*f);
                    }
                }
            }
        });
    }
    out
}

/// `q` with a column of each field its cross-encoders read
/// ([`model_fields`]) after its own: what the fetch round of a query with
/// a model phase reads, for [`rerank_models`].
pub fn with_texts(q: &Select) -> Select {
    let mut out = q.clone();
    for f in model_fields(q) {
        out.projection.push((format!("\0text {f}"), Projection::Field(f)));
        out.types.push(None);
    }
    out
}

/// The answer to `q`, a query with a model phase ([`Select::model_phase`]),
/// from `rs`: that phase's rows, in order, with the columns of
/// [`with_texts`] and what each phase reranks them by ([`fill`] of the
/// merge of [`ids`]). Each phase from the model phase on, its
/// cross-encoders scoring the rows' text here, once over every shard's
/// rows, then the page. `check` runs between the model's batches (the
/// query's deadline).
pub fn rerank_models(q: &Select, rs: ResultSet, check: &mut dyn FnMut() -> Result<()>) -> Result<ResultSet> {
    let m = q.model_phase().ok_or_else(|| err("the query ranks with no cross-encoder"))?;
    let fields = model_fields(q);
    let own = q.projection.len();
    let ResultSet { columns, rows: values, keys, scores, parts, mut stats } = rs;
    let phases = parts.map(|p| p.phases).unwrap_or_default();
    if phases.len() != values.len() || values.iter().any(|v| v.len() != own + fields.len()) || phases.iter().any(|p| p.len() != q.phases.len()) {
        return Err(err("the rows of a model phase came without their text or what their phases rerank by"));
    }
    let mut rows: Vec<Row> = values
        .into_iter()
        .zip(keys)
        .zip(scores)
        .zip(phases)
        .map(|(((values, key), score), phases)| Row { values, key, score, inputs: Vec::new(), ranked: Vec::new(), phases })
        .collect();
    for (j, p) in q.phases.iter().enumerate().skip(m) {
        rows.truncate(p.count);
        let rank = Rank::of_phase(&p.score);
        for (l, leaf) in rank.leaves.iter().enumerate() {
            let Score::CrossEncoder { model, fields: read, query, open, .. } = leaf else { continue };
            let model = open.0.as_ref().ok_or_else(|| err(format!("cross-encoder {model} is not open on the coordinator: it opens a plan's models from its models directory")))?;
            let texts: Vec<String> = rows
                .iter()
                .map(|r| crate::query::cross_encoder::text_of(read.iter().map(|f| fields.iter().position(|x| x == f).map_or(Value::Null, |i| r.values[own + i].clone()))))
                .collect();
            let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
            let clock = Clock::new(false);
            let mark = clock.mark(&stats.steps);
            for (r, s) in rows.iter_mut().zip(model.score(query, &texts, check)?) {
                r.phases[j][l] = Some(s).filter(|x| x.is_finite());
            }
            stats.plan.push(format!("cross-encoder {}: scored {} rows", model.name, rows.len()));
            stats.wrap(&clock, mark, "cross-encoder", format!("{} in phase {}", model.name, j + 1), Some(rows.len() as u64));
        }
        rows = rerank(q, rows, j..j + 1, &mut stats);
    }
    let end = q.limit.map_or(rows.len(), |l| l.saturating_add(q.offset).min(rows.len()));
    let score_cols: Vec<usize> = q.projection.iter().enumerate().filter(|(_, (_, p))| *p == Projection::Score).map(|(i, _)| i).collect();
    let mut out = ResultSet { columns: columns.into_iter().take(own).collect(), rows: Vec::new(), keys: Vec::new(), scores: Vec::new(), parts: None, stats };
    for mut r in rows.into_iter().take(end).skip(q.offset) {
        r.values.truncate(own);
        for &c in &score_cols {
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
    /// What each rerank phase ranks the row by ([`RankParts::phases`]).
    ///
    /// [`RankParts::phases`]: crate::query::exec::RankParts::phases
    phases: Vec<Vec<Option<f64>>>,
}

/// The rerank phases `which` of `q` over `rows`, in the order of the phase
/// before: each phase's rows, cut to its `count`, ranked by it as one
/// database ranks them, rows tied keeping their order. A second phase
/// ranks by the scores the shards sent; a global one is scored here, over
/// every row it reranks, from its leaves' values.
fn rerank(q: &Select, mut rows: Vec<Row>, which: std::ops::Range<usize>, stats: &mut ExecStats) -> Vec<Row> {
    let nan = |x: Option<f64>| x.unwrap_or(f64::NAN);
    for (j, p) in q.phases.iter().enumerate().skip(which.start).take(which.len()) {
        rows.truncate(p.count);
        let order = match p.global {
            false => Rank::by_scores(rows.iter().map(|r| nan(r.phases[j].first().copied().flatten())).collect()),
            true => {
                let rank = Rank::of_phase(&p.score);
                let values: Vec<Vec<f64>> = (0..rank.leaves.len()).map(|l| rows.iter().map(|r| nan(r.phases[j].get(l).copied().flatten())).collect()).collect();
                stats.reranked += rows.len() as u64;
                stats.plan.push(format!("global phase {}: scored {} rows", j + 1, rows.len()));
                rank.rerank(rows.len(), &values)
            }
        };
        let mut slots: Vec<Option<Row>> = rows.into_iter().map(Some).collect();
        rows = order
            .into_iter()
            .map(|(i, s)| {
                let mut r = slots[i].take().expect("each row ranked once");
                r.score = s;
                r
            })
            .collect();
    }
    rows
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
/// shards (at least the `k` rows the page needs, as each shard took), or
/// every row for a rank-safe boost ([`Rank::driver`]), then the fused score
/// over those ([`Rank`]), as one database computes it.
fn fuse(score: &Score, rows: Vec<Row>, k: usize) -> Result<Vec<Row>> {
    let rank = Rank::of(score, ExecOptions::default().fusion_depth).expect("a ranking expression");
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
    // A rank-safe boost: each shard sent its exact top rows by the
    // expression (and its inputs' top `depth`), so every row is one.
    let cands: Vec<usize> = if rank.exact || rank.driver().is_some() { (0..rows.len()).collect() } else { rank.candidates(&lists) };
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
