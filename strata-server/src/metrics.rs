//! Counters and latency histograms per query kind, in the Prometheus text
//! format (`GET /metrics`).

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;

use strata::query::{OrderBy, Projection, Score, Select};

/// Upper bounds of the latency buckets, in milliseconds.
const BUCKETS: [f64; 12] = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0];

#[derive(Default)]
struct Histogram {
    counts: [u64; BUCKETS.len()],
    sum_ms: f64,
    n: u64,
}

#[derive(Default)]
struct Inner {
    /// (kind, outcome) → count.
    queries: BTreeMap<(&'static str, &'static str), u64>,
    latency: BTreeMap<&'static str, Histogram>,
    writes: u64,
    written_docs: u64,
    rejected: u64,
    agent_runs: u64,
}

#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

/// The kind of a query, as its ranking or shape.
pub fn kind(q: &Select) -> &'static str {
    if q.group_by.is_some() {
        return "facet";
    }
    if q.projection.iter().all(|(_, p)| matches!(p, Projection::Count)) {
        return "count";
    }
    match &q.order {
        Some(OrderBy::Score { score, .. }) => match score {
            Score::Bm25 { .. } => "bm25",
            Score::Similarity { .. } => "vector",
            Score::Rrf { .. } | Score::Weighted(_) => "hybrid",
            Score::Column { .. } => "column",
            Score::Decay { .. } => "decay",
            Score::Product(_) => "boosted",
            Score::Ppr { .. } | Score::Hops { .. } => "graph",
        },
        _ => "filter",
    }
}

impl Metrics {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `outcome`: ok, error or timeout.
    pub fn query(&self, kind: &'static str, outcome: &'static str, ms: f64) {
        let mut m = self.inner();
        *m.queries.entry((kind, outcome)).or_default() += 1;
        let h = m.latency.entry(kind).or_default();
        if let Some(i) = BUCKETS.iter().position(|&b| ms <= b) {
            h.counts[i] += 1;
        }
        h.sum_ms += ms;
        h.n += 1;
    }

    pub fn write(&self, docs: u64) {
        let mut m = self.inner();
        m.writes += 1;
        m.written_docs += docs;
    }

    pub fn rejected(&self) {
        self.inner().rejected += 1;
    }

    pub fn agent_run(&self) {
        self.inner().agent_runs += 1;
    }

    /// Queries per kind and outcome, for the UI's status line.
    pub fn summary(&self) -> serde_json::Value {
        let m = self.inner();
        let mut by_kind = serde_json::Map::new();
        for (&(k, o), &n) in &m.queries {
            let e = by_kind.entry(k).or_insert_with(|| serde_json::json!({}));
            e[o] = serde_json::json!(n);
        }
        serde_json::json!({ "queries": by_kind, "writes": m.writes, "written_docs": m.written_docs, "rejected": m.rejected, "agent_runs": m.agent_runs })
    }

    pub fn render(&self, docs: u64, segments: u64) -> String {
        let m = self.inner();
        let mut out = String::new();
        let _ = writeln!(out, "# TYPE strata_queries_total counter");
        for (&(k, o), n) in &m.queries {
            let _ = writeln!(out, "strata_queries_total{{kind=\"{k}\",outcome=\"{o}\"}} {n}");
        }
        let _ = writeln!(out, "# TYPE strata_query_ms histogram");
        for (k, h) in &m.latency {
            let mut cum = 0;
            for (b, c) in BUCKETS.iter().zip(h.counts) {
                cum += c;
                let _ = writeln!(out, "strata_query_ms_bucket{{kind=\"{k}\",le=\"{b}\"}} {cum}");
            }
            let _ = writeln!(out, "strata_query_ms_bucket{{kind=\"{k}\",le=\"+Inf\"}} {}", h.n);
            let _ = writeln!(out, "strata_query_ms_sum{{kind=\"{k}\"}} {}", h.sum_ms);
            let _ = writeln!(out, "strata_query_ms_count{{kind=\"{k}\"}} {}", h.n);
        }
        for (name, v) in [
            ("strata_writes_total", m.writes),
            ("strata_written_docs_total", m.written_docs),
            ("strata_rejected_total", m.rejected),
            ("strata_agent_runs_total", m.agent_runs),
        ] {
            let _ = writeln!(out, "# TYPE {name} counter\n{name} {v}");
        }
        let _ = writeln!(out, "# TYPE strata_docs gauge\nstrata_docs {docs}");
        let _ = writeln!(out, "# TYPE strata_segments gauge\nstrata_segments {segments}");
        out
    }
}
