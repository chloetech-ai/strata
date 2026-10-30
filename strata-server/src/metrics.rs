//! Counters and latency histograms, in the Prometheus text format
//! (`GET /metrics`): requests by route and status, queries by kind,
//! writes, embeddings, and (from [`crate::service::Service::prometheus`])
//! the database, its merges, a coordinator's shards and the process.
//! Durations are in seconds, and every family has `# HELP` and `# TYPE`.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::time::SystemTime;

use strata::query::{OrderBy, Projection, Score, Select};

/// Upper bounds of the latency buckets, in seconds.
const BUCKETS: [f64; 14] = [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0];

#[derive(Default)]
struct Histogram {
    counts: [u64; BUCKETS.len()],
    sum: f64,
    n: u64,
}

impl Histogram {
    fn observe(&mut self, secs: f64) {
        if let Some(i) = BUCKETS.iter().position(|&b| secs <= b) {
            self.counts[i] += 1;
        }
        self.sum += secs;
        self.n += 1;
    }

    /// The series of the histogram `name`, with `labels` (`k="v",...` or
    /// empty) on each.
    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        let mut cum = 0;
        for (b, c) in BUCKETS.iter().zip(self.counts) {
            cum += c;
            let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"{b}\"}} {cum}");
        }
        let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {}", self.n);
        let braces = |l: &str| if l.is_empty() { String::new() } else { format!("{{{l}}}") };
        let _ = writeln!(out, "{name}_sum{} {}", braces(labels), self.sum);
        let _ = writeln!(out, "{name}_count{} {}", braces(labels), self.n);
    }
}

#[derive(Default)]
struct Inner {
    /// (route, status) → requests.
    http: BTreeMap<(String, u16), u64>,
    http_seconds: BTreeMap<String, Histogram>,
    /// (method, code) → calls.
    grpc: BTreeMap<(&'static str, &'static str), u64>,
    grpc_seconds: BTreeMap<&'static str, Histogram>,
    /// (kind, outcome) → count.
    queries: BTreeMap<(&'static str, &'static str), u64>,
    query_seconds: BTreeMap<&'static str, Histogram>,
    admission_seconds: Histogram,
    rejected: u64,
    writes: u64,
    written_docs: u64,
    /// HTTP status → failed writes.
    write_errors: BTreeMap<u16, u64>,
    write_seconds: Histogram,
    replays: u64,
    /// "query" or "write" → requests slower than the slow log's threshold.
    slow: BTreeMap<&'static str, u64>,
    /// "query" or "write" → texts embedded.
    embedded: BTreeMap<&'static str, u64>,
    embed_seconds: BTreeMap<&'static str, Histogram>,
    agent_runs: u64,
}

pub struct Metrics {
    inner: Mutex<Inner>,
    started: SystemTime,
}

impl Default for Metrics {
    fn default() -> Metrics {
        Metrics { inner: Mutex::default(), started: SystemTime::now() }
    }
}

/// A series family the service measures when it is scraped (a gauge, or a
/// counter kept elsewhere): one value per label set (`k="v",...` or "").
pub struct Family {
    pub name: &'static str,
    pub ty: &'static str,
    pub help: &'static str,
    pub values: Vec<(String, f64)>,
}

impl Family {
    pub fn one(name: &'static str, ty: &'static str, help: &'static str, v: f64) -> Family {
        Family { name, ty, help, values: vec![(String::new(), v)] }
    }
}

/// The kind of a query, as its ranking or shape.
pub fn kind(q: &Select) -> &'static str {
    if !q.group_by.is_empty() {
        return if q.projection.iter().all(|(_, p)| matches!(p, Projection::Field(_) | Projection::Count)) { "facet" } else { "aggregate" };
    }
    if q.projection.iter().any(|(_, p)| matches!(p, Projection::Agg { .. })) {
        return "aggregate";
    }
    if q.projection.iter().all(|(_, p)| matches!(p, Projection::Count)) {
        return "count";
    }
    match &q.order {
        Some(OrderBy::Score { score, .. }) => match score {
            Score::Bm25 { .. } => "bm25",
            Score::Similarity { .. } | Score::MaxSim { .. } => "vector",
            Score::Sparse { .. } => "sparse",
            Score::Rrf { .. } | Score::Weighted(_) => "hybrid",
            Score::Column { .. } => "column",
            Score::Decay { .. } => "decay",
            Score::Product(_) => "boosted",
            Score::Ppr { .. } | Score::Hops { .. } => "graph",
            _ => "expression",
        },
        _ => "filter",
    }
}

/// A label value escaped for the text format.
fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn head(out: &mut String, name: &str, ty: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {ty}");
}

impl Metrics {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// An HTTP request answered: its route pattern and status.
    pub fn http(&self, route: &str, status: u16, secs: f64) {
        let mut m = self.inner();
        *m.http.entry((route.to_string(), status)).or_default() += 1;
        m.http_seconds.entry(route.to_string()).or_default().observe(secs);
    }

    /// A gRPC call answered: its method and status code.
    pub fn grpc(&self, method: &'static str, code: &'static str, secs: f64) {
        let mut m = self.inner();
        *m.grpc.entry((method, code)).or_default() += 1;
        m.grpc_seconds.entry(method).or_default().observe(secs);
    }

    /// `outcome`: ok, partial, error or timeout.
    pub fn query(&self, kind: &'static str, outcome: &'static str, secs: f64) {
        let mut m = self.inner();
        *m.queries.entry((kind, outcome)).or_default() += 1;
        m.query_seconds.entry(kind).or_default().observe(secs);
    }

    /// How long a query waited for a slot to run in.
    pub fn admitted(&self, secs: f64) {
        self.inner().admission_seconds.observe(secs);
    }

    pub fn rejected(&self) {
        self.inner().rejected += 1;
    }

    pub fn write(&self, docs: u64, secs: f64) {
        let mut m = self.inner();
        m.writes += 1;
        m.written_docs += docs;
        m.write_seconds.observe(secs);
    }

    pub fn write_error(&self, status: u16) {
        *self.inner().write_errors.entry(status).or_default() += 1;
    }

    /// A write answered from its idempotency key, not applied again.
    pub fn replay(&self) {
        self.inner().replays += 1;
    }

    /// A request the slow log wrote down: "query" or "write".
    pub fn slow(&self, what: &'static str) {
        *self.inner().slow.entry(what).or_default() += 1;
    }

    /// `n` texts embedded for `what` ("query" or "write") in `secs`.
    pub fn embedded(&self, what: &'static str, n: u64, secs: f64) {
        let mut m = self.inner();
        *m.embedded.entry(what).or_default() += n;
        m.embed_seconds.entry(what).or_default().observe(secs);
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
        let write_errors: u64 = m.write_errors.values().sum();
        serde_json::json!({ "queries": by_kind, "writes": m.writes, "written_docs": m.written_docs, "write_errors": write_errors, "rejected": m.rejected, "agent_runs": m.agent_runs })
    }

    /// The text format: what this records, then `more`.
    pub fn render(&self, more: &[Family]) -> String {
        let m = self.inner();
        let mut out = String::new();
        let o = &mut out;
        head(o, "strata_build_info", "gauge", "The server's version and the SIMD kernels it runs (always 1).");
        let _ = writeln!(o, "strata_build_info{{version=\"{}\",simd=\"{}\"}} 1", env!("CARGO_PKG_VERSION"), esc(&strata::simd::describe()));
        head(o, "process_start_time_seconds", "gauge", "When the process started, in seconds since the Unix epoch.");
        let _ = writeln!(o, "process_start_time_seconds {}", self.started.duration_since(SystemTime::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64()));
        for p in process() {
            head(o, p.name, p.ty, p.help);
            let _ = writeln!(o, "{} {}", p.name, p.values[0].1);
        }

        head(o, "strata_http_requests_total", "counter", "HTTP requests answered, by route and status.");
        for ((route, status), n) in &m.http {
            let _ = writeln!(o, "strata_http_requests_total{{route=\"{}\",status=\"{status}\"}} {n}", esc(route));
        }
        head(o, "strata_http_request_duration_seconds", "histogram", "Time to answer an HTTP request, by route.");
        for (route, h) in &m.http_seconds {
            h.render(o, "strata_http_request_duration_seconds", &format!("route=\"{}\"", esc(route)));
        }
        head(o, "strata_grpc_requests_total", "counter", "gRPC calls answered, by method and status code.");
        for ((method, code), n) in &m.grpc {
            let _ = writeln!(o, "strata_grpc_requests_total{{method=\"{method}\",code=\"{code}\"}} {n}");
        }
        head(o, "strata_grpc_request_duration_seconds", "histogram", "Time to answer a gRPC call, by method.");
        for (method, h) in &m.grpc_seconds {
            h.render(o, "strata_grpc_request_duration_seconds", &format!("method=\"{method}\""));
        }

        head(o, "strata_queries_total", "counter", "Queries by kind (bm25, vector, sparse, hybrid, boosted, expression, decay, graph, column, facet, aggregate, count, filter, compound, invalid) and outcome (ok, partial, error, timeout, rejected).");
        for (&(k, outcome), n) in &m.queries {
            let _ = writeln!(o, "strata_queries_total{{kind=\"{k}\",outcome=\"{outcome}\"}} {n}");
        }
        head(o, "strata_query_duration_seconds", "histogram", "Time to answer a query, waiting for a slot included, by kind.");
        for (k, h) in &m.query_seconds {
            h.render(o, "strata_query_duration_seconds", &format!("kind=\"{k}\""));
        }
        head(o, "strata_query_admission_wait_seconds", "histogram", "Time a query waited for a slot to run in (--max-concurrent).");
        m.admission_seconds.render(o, "strata_query_admission_wait_seconds", "");
        head(o, "strata_rejected_total", "counter", "Queries refused because no slot came free before their deadline.");
        let _ = writeln!(o, "strata_rejected_total {}", m.rejected);

        head(o, "strata_writes_total", "counter", "Write requests that succeeded.");
        let _ = writeln!(o, "strata_writes_total {}", m.writes);
        head(o, "strata_written_docs_total", "counter", "Rows written by write requests.");
        let _ = writeln!(o, "strata_written_docs_total {}", m.written_docs);
        head(o, "strata_write_errors_total", "counter", "Write requests that failed, by HTTP status.");
        for (status, n) in &m.write_errors {
            let _ = writeln!(o, "strata_write_errors_total{{status=\"{status}\"}} {n}");
        }
        head(o, "strata_write_duration_seconds", "histogram", "Time from a write request to its commit, embedding and waiting for the writer included.");
        m.write_seconds.render(o, "strata_write_duration_seconds", "");
        head(o, "strata_idempotent_replays_total", "counter", "Writes answered from their idempotency key instead of being applied again.");
        let _ = writeln!(o, "strata_idempotent_replays_total {}", m.replays);
        head(o, "strata_slow_requests_total", "counter", "Queries and writes the slow log wrote down (--slow-ms).");
        for (what, n) in &m.slow {
            let _ = writeln!(o, "strata_slow_requests_total{{what=\"{what}\"}} {n}");
        }
        head(o, "strata_embedded_texts_total", "counter", "Texts embedded by the server's models: rows of writes, and query parameters.");
        for (what, n) in &m.embedded {
            let _ = writeln!(o, "strata_embedded_texts_total{{for=\"{what}\"}} {n}");
        }
        head(o, "strata_embed_duration_seconds", "histogram", "Time to embed the texts of one write request or one query parameter.");
        for (what, h) in &m.embed_seconds {
            h.render(o, "strata_embed_duration_seconds", &format!("for=\"{what}\""));
        }
        head(o, "strata_agent_runs_total", "counter", "Questions the agent was asked.");
        let _ = writeln!(o, "strata_agent_runs_total {}", m.agent_runs);

        for f in more {
            head(o, f.name, f.ty, f.help);
            for (labels, v) in &f.values {
                let labels = if labels.is_empty() { String::new() } else { format!("{{{labels}}}") };
                let _ = writeln!(o, "{}{labels} {v}", f.name);
            }
        }
        out
    }
}

/// The process's CPU time, and on Linux its resident memory and open
/// file descriptors.
fn process() -> Vec<Family> {
    let mut out = Vec::new();
    // SAFETY: getrusage fills the struct it is given.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } == 0 {
        let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
        out.push(Family::one("process_cpu_seconds_total", "counter", "CPU time the process used, user and system.", secs(ru.ru_utime) + secs(ru.ru_stime)));
    }
    #[cfg(target_os = "linux")]
    {
        // statm: sizes in pages; the second is the resident set.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as f64;
        if let Some(rss) = std::fs::read_to_string("/proc/self/statm").ok().and_then(|s| s.split_whitespace().nth(1)?.parse::<f64>().ok()) {
            out.push(Family::one("process_resident_memory_bytes", "gauge", "Memory the process holds in RAM, mapped database pages included.", rss * page));
        }
        if let Ok(d) = std::fs::read_dir("/proc/self/fd") {
            out.push(Family::one("process_open_fds", "gauge", "File descriptors the process has open.", d.count() as f64));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_has_help_and_type_and_its_series_follow() {
        let m = Metrics::default();
        m.http("/v1/query", 200, 0.003);
        m.grpc("Query", "Ok", 0.002);
        m.query("bm25", "ok", 0.0025);
        m.write(3, 0.01);
        m.embedded("write", 3, 0.2);
        let text = m.render(&[Family { name: "strata_x", ty: "gauge", help: "An example.", values: vec![("table=\"a\"".into(), 2.0)] }]);
        let mut family = "";
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                family = rest.split(' ').next().unwrap();
                continue;
            }
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                assert_eq!(rest.split(' ').next().unwrap(), family, "{line}");
                continue;
            }
            assert!(line.starts_with(family), "{line} outside {family}");
        }
        assert!(text.contains("strata_http_request_duration_seconds_bucket{route=\"/v1/query\",le=\"0.005\"} 1"), "{text}");
        assert!(text.contains("strata_query_duration_seconds_sum{kind=\"bm25\"} 0.0025"), "{text}");
        assert!(text.contains("strata_query_admission_wait_seconds_count 0"), "{text}");
        assert!(text.contains("strata_x{table=\"a\"} 2"), "{text}");
        assert!(text.contains("process_cpu_seconds_total "), "{text}");
    }
}
