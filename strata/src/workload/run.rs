//! Running a workload against a system: warm and cold latency, pages read
//! from disk, and retrieval quality.
//!
//! Any engine implements [`System`]; [`run_kind`] measures one query kind:
//!
//! * **Warm**: one untimed pass over the kind's queries (it fills the caches
//!   and records the outputs), then up to `reps` timed passes, fewer when the
//!   untimed pass shows they would exceed `warm_budget_secs` (at least one).
//!   A query's latency is the median of its repetitions; the kind reports
//!   the mean, p50 and p99 over queries.
//! * **Cold**: for a sample of the queries (evenly spaced, in shuffled order,
//!   stopping early once `cold_budget_secs` are spent), the system drops its
//!   in-process caches and the OS page cache of its files and reopens
//!   ([`System::evict_caches`]); then the query runs once. Pages read are the
//!   increase in the files' resident pages across the query
//!   ([`iostat::resident_pages`]), so they include kernel readahead; the
//!   pages resident after reopening are reported separately.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};

use super::{Kind, Workload, WorkloadQuery, TOP_K};
use crate::data::Collection;
use crate::error::Result;
use crate::{iostat, metrics};

/// The result of one workload query. Fields a kind does not produce are
/// empty.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryOutput {
    /// Result ids, in rank order (in load order for boolean kinds).
    pub ids: Vec<String>,
    /// Number of matching documents (boolean and `filter` kinds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// `(value, count)` by count descending, then value (`facet`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facets: Option<Vec<(String, u64)>>,
}

/// A searchable system under evaluation.
pub trait System {
    fn name(&self) -> &str;
    /// Whether the system can answer queries of `kind` at all.
    fn supports(&self, kind: Kind) -> bool;
    fn run(&mut self, q: &WorkloadQuery, coll: &Collection) -> Result<QueryOutput>;
    /// Makes the next query start cold: closes the database, evicts its files
    /// from the page cache, and reopens it (loading what an application loads
    /// once at startup, such as the schema).
    fn evict_caches(&mut self) -> Result<()>;
    /// Every file the system reads queries from.
    fn files(&self) -> Vec<PathBuf>;
}

/// Load time and size of a built database.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildReport {
    /// Wall time of the whole load, seconds.
    pub seconds: f64,
    /// Wall time of each phase, seconds.
    pub phases: Vec<(String, f64)>,
    /// Total size of the files, bytes.
    pub bytes: u64,
    /// Size by component, bytes.
    pub components: Vec<(String, u64)>,
}

/// How [`run_kind`] measures.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunOptions {
    /// Most timed warm repetitions of each query.
    pub reps: usize,
    /// Time for the timed warm passes of one kind, seconds.
    pub warm_budget_secs: f64,
    /// Most cold runs per kind; 0 disables them.
    pub cold_sample: usize,
    /// Time for the cold runs of one kind, seconds (at least
    /// [`MIN_COLD_RUNS`] run).
    pub cold_budget_secs: f64,
    /// At most this many queries per kind (an evenly spaced sample).
    pub max_per_kind: Option<usize>,
    /// A lower cap for the kinds that use vectors, which exact baselines
    /// answer by scanning every vector.
    pub max_vector: Option<usize>,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions { reps: 3, warm_budget_secs: 60.0, cold_sample: 100, cold_budget_secs: 60.0, max_per_kind: None, max_vector: None }
    }
}

/// Cold runs per kind regardless of the time budget.
pub const MIN_COLD_RUNS: usize = 5;

/// Latency distribution over queries, milliseconds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Latency {
    pub n: usize,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
}

impl Latency {
    pub fn of(ms: &[f64]) -> Latency {
        Latency { n: ms.len(), mean_ms: metrics::mean(ms), p50_ms: metrics::percentile(ms, 50.0), p99_ms: metrics::percentile(ms, 99.0) }
    }
}

/// Distribution of 4 KiB pages read per cold query.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pages {
    pub mean: f64,
    pub p50: f64,
    pub p99: f64,
    pub max: f64,
}

impl Pages {
    fn of(pages: &[f64]) -> Pages {
        Pages {
            mean: metrics::mean(pages),
            p50: metrics::percentile(pages, 50.0),
            p99: metrics::percentile(pages, 99.0),
            max: pages.iter().copied().fold(0.0, f64::max),
        }
    }
}

/// Measurements of one query kind on one system.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KindReport {
    pub kind: Kind,
    pub queries: usize,
    /// Timed warm repetitions per query.
    pub reps: usize,
    pub warm: Latency,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold: Option<Latency>,
    /// Pages read by cold queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_pages: Option<Pages>,
    /// Pages resident after reopening, before a cold query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_pages: Option<Pages>,
    /// Mean nDCG@10 over judged queries (judged kinds only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ndcg10: Option<f64>,
    /// Mean number of ids returned.
    pub mean_results: f64,
    /// Warm latency by target selectivity (filtered kinds).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_target: BTreeMap<String, Latency>,
    /// Warm latency by filter shape (filtered kinds).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_shape: BTreeMap<String, Latency>,
}

/// A kind's report plus each query's output (index into
/// `Workload::queries`, output of the untimed warm-up pass).
pub struct KindRun {
    pub report: KindReport,
    pub outputs: Vec<(usize, QueryOutput)>,
}

/// `k` evenly spaced positions in `0..n` (all of them if `k >= n`).
pub fn spread(n: usize, k: usize) -> Vec<usize> {
    if k >= n {
        (0..n).collect()
    } else {
        (0..k).map(|i| i * n / k).collect()
    }
}

/// The queries of `kind` that [`run_kind`] runs.
pub fn selected(w: &Workload, kind: Kind, opts: &RunOptions) -> Vec<usize> {
    let all = w.of_kind(kind);
    let mut k = opts.max_per_kind.unwrap_or(all.len());
    if kind.uses_vector() {
        k = k.min(opts.max_vector.unwrap_or(k));
    }
    spread(all.len(), k).into_iter().map(|i| all[i]).collect()
}

fn resident(files: &[PathBuf]) -> usize {
    // A file may vanish between listing and counting (e.g. SQLite's -shm).
    files.iter().filter_map(|f| iostat::resident_pages(f).ok()).sum()
}

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// Mean nDCG@10 of the outputs of judged queries.
pub fn ndcg10(w: &Workload, coll: &Collection, outputs: &[(usize, QueryOutput)]) -> Option<f64> {
    let scores: Vec<f64> = outputs
        .iter()
        .filter_map(|(i, out)| {
            let q = &coll.queries[w.queries[*i].query?];
            coll.qrels.get(&q.id).map(|judged| metrics::ndcg_at_k(&out.ids, judged, TOP_K))
        })
        .collect();
    (!scores.is_empty()).then(|| metrics::mean(&scores))
}

/// Measures one query kind (see the module docs).
pub fn run_kind(sys: &mut dyn System, w: &Workload, coll: &Collection, kind: Kind, opts: &RunOptions) -> Result<KindRun> {
    let qs = selected(w, kind, opts);
    let mut outputs = Vec::with_capacity(qs.len());
    let t = Instant::now();
    for &i in &qs {
        outputs.push((i, sys.run(&w.queries[i], coll)?));
    }
    let pass_secs = t.elapsed().as_secs_f64();
    let n_reps = ((opts.warm_budget_secs / pass_secs.max(1e-9)) as usize).clamp(1, opts.reps.max(1));
    let mut reps: Vec<Vec<f64>> = vec![Vec::with_capacity(n_reps); qs.len()];
    for _ in 0..n_reps {
        for (j, &i) in qs.iter().enumerate() {
            let t = Instant::now();
            sys.run(&w.queries[i], coll)?;
            reps[j].push(ms_since(t));
        }
    }
    let warm: Vec<f64> = reps.iter().map(|r| metrics::percentile(r, 50.0)).collect();
    let mut by_target: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut by_shape: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for (j, &i) in qs.iter().enumerate() {
        let q = &w.queries[i];
        if let Some(t) = q.target {
            by_target.entry(format!("{t}")).or_default().push(warm[j]);
        }
        if let Some(s) = q.shape {
            by_shape.entry(s.name().to_string()).or_default().push(warm[j]);
        }
    }

    // Shuffled, so that a sample cut short by the budget is still spread
    // over the kind (whose queries are grouped by target and shape).
    let mut sample = spread(qs.len(), opts.cold_sample);
    sample.shuffle(&mut StdRng::seed_from_u64(qs.len() as u64));
    let (mut cold, mut pages, mut open) = (Vec::new(), Vec::new(), Vec::new());
    let start = Instant::now();
    for j in sample {
        if cold.len() >= MIN_COLD_RUNS && start.elapsed().as_secs_f64() > opts.cold_budget_secs {
            break;
        }
        sys.evict_caches()?;
        let files = sys.files();
        let before = resident(&files);
        let t = Instant::now();
        sys.run(&w.queries[qs[j]], coll)?;
        cold.push(ms_since(t));
        pages.push(resident(&files).saturating_sub(before) as f64);
        open.push(before as f64);
    }

    let mean_results = metrics::mean(&outputs.iter().map(|(_, o)| o.ids.len() as f64).collect::<Vec<_>>());
    let report = KindReport {
        kind,
        queries: qs.len(),
        reps: n_reps,
        warm: Latency::of(&warm),
        cold: (!cold.is_empty()).then(|| Latency::of(&cold)),
        cold_pages: (!pages.is_empty()).then(|| Pages::of(&pages)),
        open_pages: (!open.is_empty()).then(|| Pages::of(&open)),
        ndcg10: if kind.judged() { ndcg10(w, coll, &outputs) } else { None },
        mean_results,
        by_target: by_target.into_iter().map(|(k, v)| (k, Latency::of(&v))).collect(),
        by_shape: by_shape.into_iter().map(|(k, v)| (k, Latency::of(&v))).collect(),
    };
    Ok(KindRun { report, outputs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_is_even_and_bounded() {
        assert_eq!(spread(10, 4), vec![0, 2, 5, 7]);
        assert_eq!(spread(3, 5), vec![0, 1, 2]);
        assert!(spread(0, 5).is_empty());
        let l = Latency::of(&[3.0, 1.0, 2.0]);
        assert_eq!((l.n, l.p50_ms, l.mean_ms), (3, 2.0, 2.0));
    }
}
