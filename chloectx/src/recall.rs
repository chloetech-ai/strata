//! Hybrid retrieval with priors, the injection gate, snippets and the
//! text ChloeCtx injects.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::anchor::Checker;
use crate::capture::past_reference;
use crate::record::{Kind, Record};
use crate::store::{Filter, Hit, Reach, Store};
use crate::Result;

/// Ranking parameters (see DESIGN.md §5).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RankParams {
    /// Candidates from each of BM25 and vector search.
    pub n: usize,
    /// Weight of normalised BM25 against normalised cosine.
    pub w_bm25: f64,
    /// Importance prior: score × (1 + λ_ι ι).
    pub lambda_imp: f64,
    /// Recency prior weight λ_t and scale τ (in records).
    pub lambda_rec: f64,
    pub tau: f64,
    /// Use BM25 / vectors at all.
    pub use_bm25: bool,
    pub use_vectors: bool,
    /// Records linked to the best hits (see [`GraphParams`]).
    #[serde(default)]
    pub graph: GraphParams,
    /// The session's last records as context (see [`TailParams`]).
    #[serde(default)]
    pub tail: TailParams,
    /// Score × this for a record of routine work ([`boilerplate`]): a test
    /// run that passed, a bare test command, a tool's empty answer.
    #[serde(default = "boilerplate_weight")]
    pub boilerplate: f64,
}

fn boilerplate_weight() -> f64 {
    0.3
}

impl Default for RankParams {
    fn default() -> Self {
        RankParams { n: 50, w_bm25: 0.5, lambda_imp: 0.2, lambda_rec: 0.05, tau: 5000.0, use_bm25: true, use_vectors: true, graph: GraphParams::default(), tail: TailParams::default(), boilerplate: boilerplate_weight() }
    }
}

/// Graph recall: the best hits, and the entities nearest the query, start a
/// walk over the records' links that passes through entities and notes; the
/// records it reaches, best by similarity to the query, are a second ranked
/// list fused into the hybrid ranking by reciprocal rank,
/// score = 1/(k + rank_hybrid) + weight/(k + rank_graph).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphParams {
    pub enabled: bool,
    /// Top hits whose links start the walk.
    pub seeds: usize,
    /// Entities nearest the query that start it too, at this cosine or more.
    pub entity_seeds: usize,
    pub min_entity_cos: f64,
    pub hops: usize,
    /// Records the walk returns.
    pub n: usize,
    pub k: f64,
    pub weight: f64,
    pub max_nodes: usize,
}

impl Default for GraphParams {
    /// Off unless `CHLOECTX_GRAPH=1` (or the config turns it on) until the
    /// evaluations in eval/graph_eval_plan.md show it helps.
    fn default() -> Self {
        GraphParams { enabled: std::env::var("CHLOECTX_GRAPH").is_ok_and(|v| v == "1"), seeds: 5, entity_seeds: 3, min_entity_cos: 0.5, hops: 2, n: 20, k: 10.0, weight: 0.5, max_nodes: 5000 }
    }
}

/// The session's last records as context for a prompt's recall: what the
/// work in progress touches, which the prompt may not say ("add the
/// endpoint" while editing the billing module). Their files and failures
/// join the prompt on the BM25 side; their vectors rank records by `maxsim`
/// (the best cosine to any of them), a third list fused beside BM25 and
/// cosine; and records about their files get a boost. The gate's evidence
/// stays the prompt's own. Off unless `CHLOECTX_SESSION_CONTEXT=1` (or the
/// config turns it on) until the A/B harness shows it helps.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TailParams {
    pub enabled: bool,
    /// The session's newest records read.
    pub records: usize,
    /// Of those, the newest with vectors (prompts, answers, tool calls and
    /// errors; not tool output) that rank by `maxsim`.
    pub vectors: usize,
    /// Candidates of each of its lists (BM25 over the digest, `maxsim`).
    pub n: usize,
    /// Weight of a record's tail score (the better of its normalised scores
    /// in the two lists), beside BM25's and cosine's (which sum to 1).
    pub weight: f64,
    /// Score × (1 + boost) for a record about one of the tail's files.
    pub file_boost: f64,
    /// Words of the digest searched with BM25, at most.
    pub digest_words: usize,
}

impl Default for TailParams {
    fn default() -> Self {
        TailParams { enabled: std::env::var("CHLOECTX_SESSION_CONTEXT").is_ok_and(|v| v == "1"), records: 20, vectors: 4, n: 20, weight: 0.5, file_boost: 0.15, digest_words: 24 }
    }
}

/// What a session's last records say about the work in progress.
#[derive(Clone, Debug, Default)]
pub struct Tail {
    /// The files they name.
    pub files: HashSet<String>,
    /// Their files and failures, as words for BM25.
    pub digest: String,
    /// The newest records' vectors, for `maxsim`.
    pub vectors: Vec<Vec<f32>>,
}

impl Tail {
    /// The tail of `records`, newest first (as [`Store::tail`] returns them).
    pub fn of(records: &[(Record, Option<Vec<f32>>)], p: &TailParams) -> Tail {
        let mut files = HashSet::new();
        let mut words: Vec<String> = Vec::new();
        let add = |w: &str, words: &mut Vec<String>| {
            if !w.is_empty() && words.len() < p.digest_words && !words.iter().any(|x| x == w) {
                words.push(w.to_string());
            }
        };
        // A file by its name and its directory's: "billing money" for
        // src/billing/money.rs, not the "src" and "rs" every path has.
        for (r, _) in records {
            for f in r.files.split_whitespace() {
                if files.insert(f.to_string()) {
                    let path = std::path::Path::new(f);
                    let dir = path.parent().and_then(|d| d.file_name()).and_then(|d| d.to_str()).filter(|d| !matches!(*d, "src" | "lib" | "test" | "tests" | "." | ".."));
                    for w in [dir, path.file_stem().and_then(|s| s.to_str())].into_iter().flatten() {
                        add(w, &mut words);
                    }
                }
            }
        }
        // A failure's first line names what failed: a test, a command.
        for (r, _) in records.iter().filter(|(r, _)| r.kind == Kind::Error) {
            for w in r.text.lines().next().unwrap_or("").split_whitespace().take(8) {
                add(w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'), &mut words);
            }
        }
        let vectors = records
            .iter()
            .filter(|(r, _)| matches!(r.kind, Kind::Prompt | Kind::Answer | Kind::ToolCall | Kind::Error))
            .filter_map(|(_, v)| v.clone())
            .take(p.vectors)
            .collect();
        Tail { files, digest: words.join(" "), vectors }
    }

    /// Whether `r` is about one of the tail's files: it names one, or
    /// links to it.
    fn touches(&self, r: &Record) -> bool {
        !self.files.is_empty()
            && (r.files.split_whitespace().any(|f| self.files.contains(f)) || r.links.iter().any(|l| l.to.strip_prefix("f:").is_some_and(|f| self.files.contains(f))))
    }
}

/// Kinds a graph walk passes through: entities and the notes about them.
pub const THROUGH: [Kind; 6] = [Kind::Entity, Kind::Preference, Kind::Decision, Kind::Fact, Kind::Todo, Kind::Note];

/// The records linked to the best hits and to the entities nearest the
/// query, best first by cosine similarity to `qv`.
pub fn expand(store: &Store, qv: &[f32], hits: &[Hit], filter: &Filter, p: &GraphParams) -> Result<Vec<(Record, f64)>> {
    let mut seeds: Vec<String> = hits.iter().take(p.seeds).map(|h| h.record.id.clone()).collect();
    if p.entity_seeds > 0 {
        let f = Filter { kinds: vec![Kind::Entity], ..Default::default() };
        for (r, c) in store.vector_top(qv, p.entity_seeds, &f)? {
            if c >= p.min_entity_cos {
                seeds.push(r.id);
            }
        }
    }
    let reach = Reach { hops: p.hops, through: THROUGH.to_vec(), max_nodes: p.max_nodes };
    store.reach(&seeds, qv, p.n, filter, &reach)
}

/// Fuses the graph list into the hybrid ranking (see [`GraphParams`]).
/// Graph records keep their cosine; their score is the fused one.
pub fn merge_graph(hits: Vec<Hit>, graph: Vec<(Record, f64)>, p: &GraphParams) -> Vec<Hit> {
    if graph.is_empty() {
        return hits;
    }
    let mut by: HashMap<String, (Hit, f64)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (i, h) in hits.into_iter().enumerate() {
        order.push(h.record.id.clone());
        by.insert(h.record.id.clone(), (h, 1.0 / (p.k + 1.0 + i as f64)));
    }
    for (j, (r, c)) in graph.into_iter().enumerate() {
        let add = p.weight / (p.k + 1.0 + j as f64);
        match by.get_mut(&r.id) {
            Some(e) => e.1 += add,
            None => {
                order.push(r.id.clone());
                by.insert(r.id.clone(), (Hit { record: r, bm25: None, cos: Some(c), score: 0.0 }, add));
            }
        }
    }
    let mut out: Vec<(Hit, f64)> = order.into_iter().filter_map(|id| by.remove(&id)).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out.into_iter()
        .map(|(mut h, s)| {
            h.score = s;
            h
        })
        .collect()
}

/// Absolute evidence about the best candidates, for the gate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Evidence {
    /// Highest cosine similarity among vector candidates.
    pub max_cos: f64,
    /// Highest IDF-weighted query-term coverage among the top 5.
    pub max_cov: f64,
    /// The top candidate is in both the BM25 and the vector top 10.
    pub agree: bool,
    /// The prompt refers back to earlier work.
    pub cue: bool,
    /// How far the best vector candidate stands out: cos₁ − cos₁₀.
    #[serde(default)]
    pub cos_gap: f64,
    /// How far the best BM25 candidate stands out: 1 − bm25₁₀ / bm25₁.
    #[serde(default)]
    pub bm_gap: f64,
}

/// The injection gate, a linear score over the evidence:
/// G = a·max_cos + b·max_cov + c·cue + d·agree + e·cos_gap + f·bm_gap.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Gate {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
    /// Full injection at G ≥ hi; a brief one (two records) at lo ≤ G < hi.
    pub hi: f64,
    pub lo: f64,
}

impl Default for Gate {
    fn default() -> Self {
        // Logistic fit on LongMemEval-S with the static embedder and
        // 400-byte records (scripts/fit_gate.py, results/gate_static_c400.json):
        // hi holds precision 0.9, lo precision 0.7.
        Gate { a: 4.868, b: 4.794, c: 0.595, d: 0.981, e: 1.959, f: -1.448, hi: 6.650, lo: 4.508 }
    }
}

impl Gate {
    /// The gate fitted for an embedder spec (cosine scales differ).
    pub fn for_embedder(spec: &str) -> Gate {
        // BGE-small vectors (in the daemon, or from a benchmark cache).
        if spec.starts_with("bert") || spec.contains("bge") {
            Gate::bert()
        } else {
            Gate::default()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Decision {
    /// Strong evidence: the full budget.
    Inject,
    /// Weak evidence: the top two records in a small budget.
    Brief,
    Skip,
}

impl Gate {
    /// Logistic fit on every fifth LongMemEval-S haystack with BGE-small
    /// vectors (scripts/fit_gate.py); hi holds precision 0.9, lo 0.7.
    pub fn bert() -> Gate {
        Gate { a: 3.803, b: 4.311, c: 0.326, d: 1.583, e: 2.778, f: 0.617, hi: 9.711, lo: 6.374 }
    }

    pub fn score(&self, e: &Evidence) -> f64 {
        self.a * e.max_cos + self.b * e.max_cov + self.c * e.cue as u8 as f64 + self.d * e.agree as u8 as f64 + self.e * e.cos_gap + self.f * e.bm_gap
    }
    pub fn decide(&self, e: &Evidence) -> Decision {
        let g = self.score(e);
        if g >= self.hi {
            Decision::Inject
        } else if g >= self.lo {
            Decision::Brief
        } else {
            Decision::Skip
        }
    }
}

fn analyzed(text: &str) -> HashSet<String> {
    chloedb::text::analyze(text).into_iter().collect()
}

/// Requests running [`both`] now.
static REQUESTS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The threads this thread's part of a request may use; 0 outside one.
    static SHARE: Cell<usize> = const { Cell::new(0) };
}

fn cores() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// A request's share of the cores: divided among the requests running when
/// it starts.
struct Request;

impl Request {
    fn start() -> Request {
        let n = REQUESTS.fetch_add(1, Ordering::AcqRel) + 1;
        SHARE.set((cores() / n).max(1));
        Request
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        SHARE.set(0);
        REQUESTS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Runs `a` and `b` side by side (`a` on another thread) when the request's
/// share of the cores allows, else one after the other. A lone request
/// splits across every core; with every core busy, splitting a request
/// would only slow the others down.
pub fn both<A: Send, B>(a: impl FnOnce() -> A + Send, b: impl FnOnce() -> B) -> (A, B) {
    let _request = (SHARE.get() == 0).then(Request::start);
    let share = SHARE.get();
    if share < 2 {
        return (a(), b());
    }
    let theirs = share / 2;
    SHARE.set(share - theirs);
    let out = std::thread::scope(|s| {
        let h = s.spawn(move || {
            SHARE.set(theirs);
            a()
        });
        let b = b();
        (h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)), b)
    });
    SHARE.set(share);
    out
}

/// Hybrid search: BM25 and vector candidates, min-max fused, with priors.
/// The two searches run on one snapshot, side by side when a core is free.
pub fn search(store: &Store, query: &str, filter: &Filter, p: &RankParams) -> Result<(Vec<Hit>, Evidence)> {
    search_with(store, query, None, filter, p)
}

/// [`search`] with the session's last records as context ([`TailParams`]),
/// when `p.tail` is on.
pub fn search_with(store: &Store, query: &str, tail: Option<&Tail>, filter: &Filter, p: &RankParams) -> Result<(Vec<Hit>, Evidence)> {
    let store = &store.pinned();
    let terms = chloedb::text::analyze_query(query);
    let tail = tail.filter(|_| p.tail.enabled);
    let ((bm, td), (vq, dv, tv)) = both(
        || {
            let bm = if p.use_bm25 && !terms.is_empty() { store.bm25_top(query, p.n, filter) } else { Ok(Vec::new()) };
            let td = match tail {
                Some(t) if p.use_bm25 && !chloedb::text::analyze_query(&t.digest).is_empty() => store.bm25_top(&t.digest, p.tail.n, filter),
                _ => Ok(Vec::new()),
            };
            (bm, td)
        },
        || {
            let vq = p.use_vectors.then(|| store.embedder.embed_query(query)).transpose();
            let dv = match &vq {
                Ok(Some(v)) => store.vector_top(v, p.n, filter),
                _ => Ok(Vec::new()),
            };
            let tv = match tail {
                Some(t) if p.use_vectors => store.maxsim_top(&t.vectors, p.tail.n, filter),
                _ => Ok(Vec::new()),
            };
            (vq, dv, tv)
        },
    );
    let (mut bm, mut dv, mut td, mut tv, vq) = (bm?, dv?, td?, tv?, vq?);
    // Earlier copies of the same question and answers that admitted not
    // knowing match a repeated question best and carry nothing: they would
    // inflate the gate and repeat the failure.
    let qterms: HashSet<String> = terms.iter().cloned().collect();
    // Envelopes a harness wrapped prompts in, stored before they were
    // cleaned off ([`crate::envelope`]), are machinery, not earlier work.
    let useful = |r: &Record| r.tag != "uncertain" && !crate::record::closed_tag(&r.tag) && !same_question(r, &qterms) && !crate::envelope::is_envelope_record(r);
    bm.retain(|(r, _)| useful(r));
    dv.retain(|(r, _)| useful(r));
    td.retain(|(r, _)| useful(r));
    tv.retain(|(r, _)| useful(r));
    let (hits, ev) = fuse_with(store, query, &terms, bm, dv, [td, tv], tail, p);
    if !p.graph.enabled || hits.is_empty() {
        return Ok((hits, ev));
    }
    let qv = match vq {
        Some(v) => v,
        None => store.embedder.embed_query(query)?,
    };
    let mut graph = expand(store, &qv, &hits, filter, &p.graph)?;
    graph.retain(|(r, _)| useful(r));
    Ok((merge_graph(hits, graph, &p.graph), ev))
}

/// Fuses BM25 and vector candidate lists (exposed for benchmarks that run
/// the two searches themselves).
pub fn fuse(store: &Store, query: &str, terms: &[String], bm: Vec<(Record, f64)>, dv: Vec<(Record, f64)>, p: &RankParams) -> (Vec<Hit>, Evidence) {
    fuse_with(store, query, terms, bm, dv, [Vec::new(), Vec::new()], None, p)
}

/// [`fuse`] with the session tail's two lists (BM25 over its digest and
/// `maxsim` over its vectors) and its files. A record's tail score is the
/// better of its two normalised scores. The evidence is the prompt's own:
/// read from the order without the tail, so the tail reorders what the gate
/// lets through but does not open or close it.
#[allow(clippy::too_many_arguments)]
fn fuse_with(store: &Store, query: &str, terms: &[String], bm: Vec<(Record, f64)>, dv: Vec<(Record, f64)>, tails: [Vec<(Record, f64)>; 2], tail: Option<&Tail>, p: &RankParams) -> (Vec<Hit>, Evidence) {
    let top_ids = |l: &[(Record, f64)]| l.iter().take(10).map(|(r, _)| r.id.clone()).collect::<HashSet<_>>();
    let (bm_top, dv_top) = (top_ids(&bm), top_ids(&dv));
    let max_cos = dv.iter().map(|x| x.1).fold(0.0, f64::max);
    let nth = |l: &[(Record, f64)], i: usize| l.get(i.min(l.len().saturating_sub(1))).map_or(0.0, |x| x.1);
    let cos_gap = if dv.is_empty() { 0.0 } else { nth(&dv, 0) - nth(&dv, 9) };
    let bm_gap = if bm.is_empty() || nth(&bm, 0) <= 0.0 { 0.0 } else { 1.0 - nth(&bm, 9) / nth(&bm, 0) };
    let range = |l: &[(Record, f64)]| {
        let lo = l.iter().map(|x| x.1).fold(f64::INFINITY, f64::min);
        let hi = l.iter().map(|x| x.1).fold(f64::NEG_INFINITY, f64::max);
        (lo, hi)
    };
    let (blo, bhi) = range(&bm);
    let (vlo, vhi) = range(&dv);
    let norm = |x: f64, lo: f64, hi: f64| if hi > lo { (x - lo) / (hi - lo) } else { 1.0 };
    let mut by: HashMap<String, Hit> = HashMap::new();
    for (r, s) in bm {
        by.insert(r.id.clone(), Hit { record: r, bm25: Some(s), cos: None, score: 0.0 });
    }
    for (r, s) in dv {
        by.entry(r.id.clone()).and_modify(|h| h.cos = Some(s)).or_insert(Hit { record: r, bm25: None, cos: Some(s), score: 0.0 });
    }
    let mut near: HashMap<String, f64> = HashMap::new();
    for l in tails {
        let (lo, hi) = range(&l);
        for (r, s) in l {
            let t = near.entry(r.id.clone()).or_insert(0.0);
            *t = t.max(norm(s, lo, hi));
            by.entry(r.id.clone()).or_insert(Hit { record: r, bm25: None, cos: None, score: 0.0 });
        }
    }
    let wt = if near.is_empty() { 0.0 } else { p.tail.weight };
    let max_seq = by.values().map(|h| h.record.seq).max().unwrap_or(0).max(store.last_seq());
    let (wb, wv) = match (bhi.is_finite(), vhi.is_finite()) {
        (true, true) => (p.w_bm25, 1.0 - p.w_bm25),
        (true, false) => (1.0, 0.0),
        _ => (0.0, 1.0),
    };
    // Each hit with its score without the tail, which orders the evidence.
    let mut hits: Vec<(Hit, f64)> = by
        .into_values()
        .map(|mut h| {
            let b = h.bm25.map_or(0.0, |x| norm(x, blo, bhi));
            let c = h.cos.map_or(0.0, |x| norm(x, vlo, vhi));
            let t = near.get(&h.record.id).copied().unwrap_or(0.0);
            let age = (max_seq - h.record.seq).max(0) as f64;
            // Routine work matches any question about its turn and says nothing.
            let routine = if boilerplate(&h.record) { p.boilerplate } else { 1.0 };
            let score = |rel: f64| routine * (rel * (1.0 + p.lambda_imp * h.record.importance) + p.lambda_rec * (-age / p.tau).exp());
            let plain = score(wb * b + wv * c);
            h.score = score(wb * b + wv * c + wt * t);
            if tail.is_some_and(|t| t.touches(&h.record)) {
                h.score *= 1.0 + p.tail.file_boost;
            }
            (h, plain)
        })
        .collect();
    hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.record.seq.cmp(&a.0.record.seq)));
    let mut ev = Evidence { max_cos, cue: past_reference(query), cos_gap, bm_gap, ..Default::default() };
    if let Some((top, _)) = hits.first() {
        ev.agree = bm_top.contains(&top.record.id) && dv_top.contains(&top.record.id);
    }
    if !terms.is_empty() {
        let idf = store.idf(terms).unwrap_or_else(|_| vec![1.0; terms.len()]);
        let total: f64 = idf.iter().sum();
        for (h, _) in hits.iter().take(5) {
            let have = analyzed(&format!("{}\n{}", h.record.text, h.record.ctx));
            let got: f64 = terms.iter().zip(&idf).filter(|(t, _)| have.contains(*t)).map(|(_, w)| w).sum();
            ev.max_cov = ev.max_cov.max(if total > 0.0 { got / total } else { 0.0 });
        }
    }
    let mut hits: Vec<Hit> = hits.into_iter().map(|(h, _)| h).collect();
    if tail.is_some() {
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.record.seq.cmp(&a.record.seq)));
    }
    (hits, ev)
}

/// Whether a record only says that routine work went as expected: a test
/// run that passed (`test result: ok. 42 passed`), a test command that
/// names nothing of its own (`$ cargo test`), or a tool's empty or
/// one-word answer. Every turn of a coding session has some, so they match
/// any question about the turn and fill the record cap with words that
/// carry nothing; recall scores them down ([`RankParams::boilerplate`]).
pub fn boilerplate(r: &Record) -> bool {
    match r.kind {
        Kind::ToolResult => no_op_output(&r.text) || passing_tests(&r.text),
        Kind::ToolCall => bare_test_command(&r.text),
        _ => false,
    }
}

/// A test run's output that reports passes and no failure.
fn passing_tests(text: &str) -> bool {
    static PASS: OnceLock<regex::Regex> = OnceLock::new();
    static NONE_FAILED: OnceLock<regex::Regex> = OnceLock::new();
    static FAIL: OnceLock<regex::Regex> = OnceLock::new();
    let pass = PASS.get_or_init(|| regex::Regex::new(r"(?im)(test result: ok\b|\b\d+ (passed|passing)\b|\ball tests? passed\b|^ok\s+\S|^PASS\b|\btests? passed\b)").unwrap());
    let none_failed = NONE_FAILED.get_or_init(|| regex::Regex::new(r"(?i)\b0 (failed|failures?|failing|errors?)\b|\b(failed|failures|errors): 0\b").unwrap());
    let fail = FAIL.get_or_init(|| regex::Regex::new(r"(?i)\b(fail(ed|ures?|ing)?|errors?|panicked|traceback|exception)\b").unwrap());
    pass.is_match(text) && !fail.is_match(&none_failed.replace_all(text, ""))
}

/// `$ cargo test`, `$ npm test -- --silent  # Run the tests`: a test runner
/// with only flags or paths after it, not a test it names.
fn bare_test_command(text: &str) -> bool {
    const RUNNERS: [&str; 20] = [
        "cargo test", "cargo nextest run", "npm test", "npm run test", "yarn test", "pnpm test", "bun test", "deno test", "python -m pytest", "python3 -m pytest", "pytest", "go test",
        "make test", "make check", "mvn test", "gradle test", "./gradlew test", "rspec", "npx jest", "npx vitest",
    ];
    let Some(cmd) = text.trim().strip_prefix("$ ") else { return false };
    if cmd.lines().count() > 1 {
        return false;
    }
    let cmd = cmd.split("  # ").next().unwrap_or("").trim();
    RUNNERS.iter().any(|r| {
        cmd.strip_prefix(r).is_some_and(|rest| (rest.is_empty() || rest.starts_with(' ')) && rest.split_whitespace().all(|w| w.starts_with('-') || w.starts_with("./") || w == "--" || w == "."))
    })
}

/// A tool's answer that says nothing: empty, one word (`ok`, `Done`), or a
/// response whose every field is empty (`{"stdout":"","stderr":""...}`).
fn no_op_output(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || matches!(t.to_lowercase().trim_end_matches('.'), "ok" | "done" | "success" | "(no output)" | "no output" | "(no content)" | "true" | "null" | "[]" | "{}") {
        return true;
    }
    match serde_json::from_str::<serde_json::Value>(t) {
        Ok(serde_json::Value::Object(m)) => m.values().all(|v| match v {
            serde_json::Value::String(s) => s.trim().is_empty(),
            serde_json::Value::Array(a) => a.is_empty(),
            serde_json::Value::Bool(_) | serde_json::Value::Null | serde_json::Value::Number(_) => true,
            serde_json::Value::Object(o) => o.is_empty(),
        }),
        _ => false,
    }
}

/// The part of `text` (at most `max` bytes) with the most query terms.
pub fn snippet(text: &str, terms: &HashSet<String>, max: usize) -> String {
    let (s, e) = snippet_range(text, terms, max);
    if s == 0 && e == text.len() {
        return text.to_string();
    }
    format!("{}{}{}", if s > 0 { "…" } else { "" }, text[s..e].trim(), if e < text.len() { "…" } else { "" })
}

/// The byte range [`snippet`] shows.
pub fn snippet_range(text: &str, terms: &HashSet<String>, max: usize) -> (usize, usize) {
    if text.len() <= max {
        return (0, text.len());
    }
    let mut hits = Vec::new();
    let mut pos = 0;
    for w in text.split(|c: char| !c.is_alphanumeric()) {
        if !w.is_empty() {
            if let Some(t) = chloedb::text::analyzer::term_of(&w.to_lowercase()) {
                if terms.contains(t.as_ref()) {
                    hits.push(pos);
                }
            }
        }
        pos += w.len() + 1;
    }
    let mut best = (0usize, 0usize);
    for &h in &hits {
        let start = h.saturating_sub(max / 5);
        let n = hits.iter().filter(|&&x| x >= start && x < start + max).count();
        if n > best.1 {
            best = (start, n);
        }
    }
    let floor = |mut i: usize| {
        i = i.min(text.len());
        while !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    (floor(best.0), floor(best.0 + max))
}

/// How retrieved records become injected text.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackParams {
    pub budget_chars: usize,
    pub snippet_chars: usize,
    /// Records taken from one session turn at most.
    pub per_turn: usize,
    /// Records at most.
    pub max_records: usize,
}

impl Default for PackParams {
    fn default() -> Self {
        // Records are at most CHUNK_CHARS (400) bytes, so about ten fit the
        // budget with their headers. A coding turn spans many records (each
        // tool call and result), so the per-turn cap is loose: at 3, the
        // answer was often a fourth record of a turn already shown.
        PackParams { budget_chars: 4000, snippet_chars: 600, per_turn: 8, max_records: 12 }
    }
}

impl PackParams {
    /// The weak-evidence tier: three records, about 300 tokens.
    pub fn brief() -> PackParams {
        PackParams { budget_chars: 1200, snippet_chars: 500, per_turn: 1, max_records: 3 }
    }
}

/// Whether a stored prompt is essentially the query asked again: it matches
/// best and says nothing new, so it only takes up the budget.
fn same_question(r: &Record, terms: &HashSet<String>) -> bool {
    // Only stored questions: a statement that repeats the question's words
    // ("my dog is called Max") is the answer, not a repeat.
    if r.kind != Kind::Prompt || terms.is_empty() || !r.text.contains('?') {
        return false;
    }
    let other: HashSet<String> = chloedb::text::analyze_query(&r.text).into_iter().collect();
    let inter = terms.intersection(&other).count() as f64;
    let union = terms.union(&other).count() as f64;
    union > 0.0 && inter / union >= 0.7
}

/// The prompts, answers and episodes among `hits` of turns whose notes a
/// later note superseded: what the turn asked for or settled was changed
/// since ("100 requests per minute", raised to 300 later). A weak match
/// shows its few records with no note of that, so it leaves them out.
pub fn superseded_turns(store: &Store, hits: &[Hit]) -> Result<HashSet<String>> {
    let said = |h: &&Hit| matches!(h.record.kind, Kind::Prompt | Kind::Answer | Kind::Episode);
    let mut sessions: Vec<String> = hits.iter().filter(said).map(|h| h.record.session.clone()).collect();
    sessions.sort();
    sessions.dedup();
    let turns: HashSet<(String, i64)> = store.superseded_in(&sessions)?.into_iter().map(|r| (r.session, r.turn)).collect();
    Ok(hits.iter().filter(said).filter(|h| turns.contains(&(h.record.session.clone(), h.record.turn))).map(|h| h.record.id.clone()).collect())
}

/// Selects hits within the budget; returns them oldest first with snippets.
/// Answers marked uncertain and earlier copies of the same question are skipped.
pub fn pack(hits: &[Hit], query: &str, skip: &HashSet<String>, p: &PackParams) -> Vec<(Record, String)> {
    pack_explained(hits, query, skip, p).0
}

/// [`pack`], and for each hit why it was taken or left out, in the words a
/// receipt uses ([`crate::receipt`]): `take`, `excerpt` (taken as a
/// snippet), or `skip: ` and the reason.
pub fn pack_explained(hits: &[Hit], query: &str, skip: &HashSet<String>, p: &PackParams) -> (Vec<(Record, String)>, HashMap<String, String>) {
    let terms: HashSet<String> = chloedb::text::analyze_query(query).into_iter().collect();
    let mut per: HashMap<(String, i64), usize> = HashMap::new();
    let mut used = 0usize;
    let mut out: Vec<(Record, String)> = Vec::new();
    let mut why: HashMap<String, String> = HashMap::new();
    for h in hits {
        let mut note = |w: &str| {
            why.entry(h.record.id.clone()).or_insert_with(|| w.to_string());
        };
        if out.len() >= p.max_records {
            note("skip: record cap");
            continue;
        }
        if skip.contains(&h.record.id) {
            note("skip: shown before");
            continue;
        }
        if h.record.tag == "uncertain" {
            note("skip: uncertain");
            continue;
        }
        if same_question(&h.record, &terms) {
            note("skip: same question");
            continue;
        }
        let k = (h.record.session.clone(), h.record.turn);
        if *per.get(&k).unwrap_or(&0) >= p.per_turn {
            note("skip: per-turn cap");
            continue;
        }
        let snip = snippet(&h.record.text, &terms, p.snippet_chars);
        if used + snip.len() + 80 > p.budget_chars && !out.is_empty() {
            note("skip: over budget");
            continue;
        }
        used += snip.len() + 80;
        *per.entry(k).or_default() += 1;
        note(if snip == h.record.text { "take" } else { "excerpt" });
        out.push((h.record.clone(), snip));
    }
    out.sort_by_key(|(r, _)| r.seq);
    (out, why)
}

/// `2026-10-11 07:43 UTC` for a Unix time.
pub fn fmt_time(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", secs / 3600, (secs % 3600) / 60)
}

/// `3 h ago`, `2 days ago`.
pub fn fmt_ago(ts: i64, now: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=119 => "just now".into(),
        120..=7199 => format!("{} min ago", d / 60),
        7200..=172_799 => format!("{} h ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

fn short(session: &str) -> String {
    session.chars().take(8).collect()
}

/// One injected record: a provenance line and its snippet.
pub fn fmt_record(r: &Record, snip: &str, now: i64) -> String {
    fmt_record_labeled(r, snip, now, "")
}

/// One injected record with a freshness label after its provenance line.
pub fn fmt_record_labeled(r: &Record, snip: &str, now: i64, label: &str) -> String {
    let mut head = format!("[{}] {} · session {} turn {} · {}", r.id, fmt_time(r.ts), short(&r.session), r.turn, r.kind.as_str());
    if r.tag != "-" && !r.tag.is_empty() {
        head += &format!(" ({})", r.tag);
    }
    head += &format!(" · {}{label}", fmt_ago(r.ts, now));
    format!("{head}\n{}", crate::envelope::defuse(snip.trim()))
}

/// The recall block for a prompt (`weak`: a low-confidence match).
pub fn fmt_recall_as(items: &[(Record, String)], now: i64, weak: bool) -> String {
    fmt_recall_checked(items, now, weak, None)
}

/// The recall block, notes labelled when their code changed since.
pub fn fmt_recall_checked(items: &[(Record, String)], now: i64, weak: bool, mut check: Option<&mut Checker>) -> String {
    let mut s = if weak {
        format!(
            "ChloeCtx found {} possibly related record{} from earlier work in this project (a weak match, outside the current context window). They may be irrelevant or outdated.\n",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "ChloeCtx recalled {} record{} from earlier work in this project, outside the current context window (oldest first). They may be outdated; newer records and the current files take precedence.\n",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        )
    };
    for (r, snip) in items {
        let label = match check.as_deref_mut() {
            Some(c) if r.kind.is_knowledge() => c.check_record(&r).label(),
            _ => String::new(),
        };
        s += "\n";
        s += &fmt_record_labeled(r, snip, now, &label);
        s += "\n";
    }
    s += "\nThe memory_timeline tool shows what surrounded a record id; memory_search finds more.";
    s
}

pub fn fmt_recall(items: &[(Record, String)], now: i64) -> String {
    fmt_recall_as(items, now, false)
}

/// One line per further candidate (id, kind, opening words), for the model
/// to follow up with `memory_timeline`.
pub fn fmt_more(hits: &[Hit], shown: &HashSet<String>, n: usize) -> String {
    let more: Vec<String> = hits
        .iter()
        .filter(|h| !shown.contains(&h.record.id) && h.record.tag != "uncertain")
        .take(n)
        .map(|h| {
            let first: String = h.record.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(90).collect();
            format!("- [{}] {}: {}…", h.record.id, h.record.kind.as_str(), first)
        })
        .collect();
    if more.is_empty() {
        String::new()
    } else {
        format!("\nFurther candidates (memory_timeline with an id shows the full record and its surroundings):\n{}", more.join("\n"))
    }
}

/// When a prompt gets the knowledge written down earlier (decisions, rules,
/// requirements, facts) that bears on it, independently of the gate.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeParams {
    pub enabled: bool,
    /// At most this many records per prompt.
    pub max_records: usize,
    /// A record qualifies with this cosine similarity to the prompt…
    pub min_cos: f64,
    /// …or with this share of the prompt's IDF weight among its terms.
    pub min_cov: f64,
    /// Candidates per search.
    pub n: usize,
}

impl Default for KnowledgeParams {
    fn default() -> Self {
        KnowledgeParams { enabled: true, max_records: 4, min_cos: 0.40, min_cov: 0.30, n: 20 }
    }
}

pub const KNOWLEDGE: [Kind; 5] = [Kind::Preference, Kind::Decision, Kind::Fact, Kind::Todo, Kind::Note];

/// Knowledge records that match a prompt well enough on their own text,
/// best first, leaving out `skip` and records closed as done or superseded.
pub fn knowledge(store: &Store, query: &str, filter: &Filter, skip: &HashSet<String>, p: &KnowledgeParams) -> Result<Vec<(Record, f64, f64)>> {
    let store = &store.pinned();
    let mut f = filter.clone();
    f.kinds = KNOWLEDGE.to_vec();
    let terms = chloedb::text::analyze_query(query);
    let (bm, dv) = both(
        || if terms.is_empty() { Ok(Vec::new()) } else { store.bm25_top(query, p.n, &f) },
        || store.vector_top(&store.embedder.embed_query(query)?, p.n, &f),
    );
    let mut by: HashMap<String, (Record, f64)> = HashMap::new();
    for (r, c) in dv? {
        by.insert(r.id.clone(), (r, c));
    }
    for (r, _) in bm? {
        by.entry(r.id.clone()).or_insert((r, 0.0));
    }
    let idf = if terms.is_empty() { Vec::new() } else { store.idf(&terms).unwrap_or_else(|_| vec![1.0; terms.len()]) };
    let total: f64 = idf.iter().sum();
    let todo_open = |r: &Record| r.kind != Kind::Todo || r.tag == "planned";
    let mut out: Vec<(Record, f64, f64)> = by
        .into_values()
        .filter(|(r, _)| !skip.contains(&r.id) && !crate::record::closed_tag(&r.tag) && r.tag != "done" && todo_open(r) && !crate::envelope::is_envelope_record(r))
        .map(|(r, c)| {
            // A note's `When:` line counts: it names the requests it bears on.
            let have = analyzed(&r.key_text());
            let got: f64 = terms.iter().zip(&idf).filter(|(t, _)| have.contains(*t)).map(|(_, w)| w).sum();
            let cov = if total > 0.0 { got / total } else { 0.0 };
            (r, c, cov)
        })
        .filter(|(_, c, cov)| *c >= p.min_cos || *cov >= p.min_cov)
        .collect();
    out.sort_by(|a, b| (b.1 + b.2).total_cmp(&(a.1 + a.2)));
    out.truncate(p.max_records);
    Ok(out)
}

/// The live notes most related to `text` (by vector and BM25, no
/// thresholds), best first: what the distiller is shown, so that it can
/// update an old note instead of only the newest ones.
pub fn related(store: &Store, text: &str, n: usize) -> Result<Vec<Record>> {
    let store = &store.pinned();
    let f = Filter { kinds: KNOWLEDGE.to_vec(), ..Default::default() };
    let live = |r: &Record| r.tag != "superseded" && r.tag != "done" && r.tag != "cue" && (r.kind != Kind::Todo || r.tag == "planned") && !crate::envelope::is_envelope_record(r);
    let (bm, dv) = both(
        || if chloedb::text::analyze_query(text).is_empty() { Ok(Vec::new()) } else { store.bm25_top(text, n * 2, &f) },
        || store.vector_top(&store.embedder.embed_query(text)?, n * 2, &f),
    );
    let mut score: HashMap<String, (Record, f64)> = HashMap::new();
    for (i, (r, _)) in dv?.into_iter().enumerate() {
        score.entry(r.id.clone()).or_insert((r, 0.0)).1 += 1.0 / (10.0 + i as f64);
    }
    for (i, (r, _)) in bm?.into_iter().enumerate() {
        score.entry(r.id.clone()).or_insert((r, 0.0)).1 += 1.0 / (10.0 + i as f64);
    }
    let mut v: Vec<(Record, f64)> = score.into_values().filter(|(r, _)| live(r)).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok(v.into_iter().take(n).map(|x| x.0).collect())
}

fn note_label(r: &Record) -> &'static str {
    match r.kind {
        Kind::Todo => "planned",
        Kind::Preference => "rule",
        k => k.as_str(),
    }
}

/// The knowledge block of a prompt's context.
pub fn fmt_knowledge(items: &[(Record, f64, f64)], now: i64) -> String {
    fmt_knowledge_checked(items, now, None)
}

/// The knowledge block, each note labelled when the code it is anchored to
/// changed since.
pub fn fmt_knowledge_checked(items: &[(Record, f64, f64)], now: i64, mut check: Option<&mut Checker>) -> String {
    let mut s = String::from("ChloeCtx notes from earlier sessions that may bear on this request (decisions, rules and requirements agreed before; check them before deciding):\n");
    for (r, _, _) in items {
        let t = r.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let fresh = check.as_deref_mut().map(|c| c.check_record(&r).label()).unwrap_or_default();
        s += &format!("- [{}] {t} ({}, {}){fresh}\n", note_label(r), r.id, fmt_ago(r.ts, now));
    }
    s
}

/// The briefing given at the start of a session: the knowledge written
/// down earlier (planned work, rules, decisions, facts), where the last
/// session left off, and how to search the rest. Returns the text and the
/// ids it shows, so that per-prompt recall does not repeat them.
pub fn briefing(store: &Store, current_session: &str, max_chars: usize, now: i64) -> Result<(String, Vec<String>)> {
    briefing_checked(store, current_session, max_chars, now, None)
}

/// The briefing, with notes whose code changed since moved to a section of
/// their own and labelled, and an index of what else the notes are about.
pub fn briefing_checked(store: &Store, current_session: &str, max_chars: usize, now: i64, mut check: Option<&mut Checker>) -> Result<(String, Vec<String>)> {
    let store = &store.pinned();
    let total = store.len();
    if total == 0 {
        return Ok((String::new(), Vec::new()));
    }
    let none = Filter::default();
    let sessions = store.query("SELECT session, COUNT(*) FROM memory GROUP BY session ORDER BY 2 DESC LIMIT 1000")?.rows.len();
    let prev = Filter { exclude_window: Some((current_session.to_string(), 0)), ..Default::default() };
    let last = store.latest(&[], 1, &prev)?;
    let mut s = format!("ChloeCtx memory for this project: {total} records from {sessions} session{}", if sessions == 1 { "" } else { "s" });
    if let Some(r) = last.first() {
        s += &format!(", last activity {}", fmt_ago(r.ts, now));
    }
    s += ". Requirements, decisions and conventions agreed in earlier conversations are often not in the code: \
          before a design decision, and whenever a task refers to earlier work, search memory (memory_ask answers a question about earlier work from notes, replies and messages at once; \
          memory_search is one quick search; memory_timeline shows a record's surroundings; memory_sql runs structured queries). memory_remember stores a new decision or fact.\n";
    let clip = |t: &str, n: usize| -> String {
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.chars().count() > n {
            format!("{}…", t.chars().take(n).collect::<String>())
        } else {
            t
        }
    };
    let norm = |t: &str| t.split_whitespace().map(|w| w.to_lowercase()).collect::<Vec<_>>().join(" ");
    // Once knowledge is distilled, the cue-word preferences (sentences
    // picked from prompts by their wording) only repeat it in fragments.
    let distilled = !store.query("SELECT id FROM memory WHERE tag = 'distilled' LIMIT 1")?.rows.is_empty();
    let mut seen: HashSet<String> = HashSet::new();
    let mut ids = Vec::new();
    let mut stale: Vec<(Record, String)> = Vec::new();
    let mut unshown: Vec<Record> = Vec::new();
    // Most recent first, oldest shown first; one line per record, while the budget lasts.
    let mut section = |title: &str, recs: Vec<Record>, n: usize, keep: usize, s: &mut String, stale: &mut Vec<(Record, String)>, unshown: &mut Vec<Record>| {
        let mut lines = Vec::new();
        for r in recs {
            if r.tag == "superseded" || r.tag == "done" || (distilled && r.tag == "cue") || crate::envelope::is_envelope_record(&r) || !seen.insert(norm(&r.text)) {
                continue;
            }
            let fresh = match check.as_deref_mut() {
                Some(c) if r.kind.is_knowledge() => c.check_record(&r),
                _ => crate::anchor::Freshness::Current,
            };
            if fresh.is_stale() {
                stale.push((r, fresh.label()));
                continue;
            }
            if lines.len() == keep {
                unshown.push(r);
                continue;
            }
            lines.push((r.id.clone(), format!("- {} ({}, {}){}\n", crate::envelope::defuse(&clip(&r.text, n)), r.id, fmt_ago(r.ts, now), fresh.label())));
        }
        lines.reverse();
        let head = format!("\n{title}\n");
        let mut added = false;
        for (id, line) in lines {
            if s.len() + line.len() + if added { 0 } else { head.len() } > max_chars {
                continue;
            }
            if !added {
                *s += &head;
                added = true;
            }
            *s += &line;
            ids.push(id);
        }
    };
    let planned: Vec<Record> = store.latest(&[Kind::Todo], 60, &none)?.into_iter().filter(|r| r.tag == "planned").collect();
    section("Planned work, with the requirements agreed for it:", planned, 400, 10, &mut s, &mut stale, &mut unshown);
    section("Rules and conventions:", store.latest(&[Kind::Preference], 80, &none)?, 260, 14, &mut s, &mut stale, &mut unshown);
    section("Decisions:", store.latest(&[Kind::Decision], 80, &none)?, 260, 10, &mut s, &mut stale, &mut unshown);
    section("Facts about the project:", store.latest(&[Kind::Fact, Kind::Note], 80, &none)?, 260, 10, &mut s, &mut stale, &mut unshown);
    let mut stale_ids: Vec<String> = Vec::new();
    if !stale.is_empty() {
        let head = "\nNotes that may be out of date (the code they describe has changed since they were written; check it before relying on them):\n";
        let mut added = false;
        for (r, label) in stale.iter().take(6) {
            let line = format!("- {} ({}, {}){label}\n", crate::envelope::defuse(&clip(&r.text, 220)), r.id, fmt_ago(r.ts, now));
            if s.len() + line.len() + if added { 0 } else { head.len() } > max_chars {
                continue;
            }
            if !added {
                s += head;
                added = true;
            }
            s += &line;
            stale_ids.push(r.id.clone());
        }
    }
    let checkpoint: Vec<Record> = store.latest(&[Kind::Todo], 10, &none)?.into_iter().filter(|r| r.tag != "planned" && r.tag != "done").take(1).collect();
    section("Open tasks at the last checkpoint:", checkpoint, 500, 1, &mut s, &mut stale, &mut unshown);
    section("Where the most recent earlier session left off:", store.latest(&[Kind::Episode], 3, &prev)?, 300, 3, &mut s, &mut stale, &mut unshown);
    ids.extend(stale_ids);
    // What the notes left out are about, so the model knows what to ask for.
    let unshown: Vec<Record> = unshown.into_iter().filter(|r| r.kind.is_knowledge()).collect();
    if !unshown.is_empty() {
        let mut count: HashMap<String, usize> = HashMap::new();
        for r in &unshown {
            for e in r.linked("about") {
                if let Some(name) = e.strip_prefix("e:") {
                    *count.entry(name.to_string()).or_default() += 1;
                }
            }
        }
        let mut top: Vec<(String, usize)> = count.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let names: Vec<String> = top.iter().take(20).map(|(n, c)| if *c > 1 { format!("{n} ({c})") } else { n.clone() }).collect();
        let mut line = format!("\n{} older note{} not shown here", unshown.len(), if unshown.len() == 1 { " is" } else { "s are" });
        if !names.is_empty() {
            line += &format!(", about: {}", names.join(", "));
        }
        line += ". memory_ask or memory_search finds them.\n";
        if s.len() + line.len() <= max_chars + 400 {
            s += &line;
        }
    }
    Ok((s, ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parts of a request divide its share of the cores between them,
    /// and a request (even one that panics) gives its share back.
    #[test]
    fn requests_divide_their_share() {
        let ((sa, (x, y)), sb) = both(|| (SHARE.get(), both(|| SHARE.get(), || SHARE.get())), || SHARE.get());
        assert!(sa >= 1 && sb >= 1 && sa + sb <= cores().max(2), "{sa} + {sb}");
        if sa >= 2 {
            assert_eq!(x + y, sa);
        } else {
            assert_eq!((x, y), (1, 1));
        }
        assert_eq!(SHARE.get(), 0);
        assert!(std::panic::catch_unwind(|| both(|| panic!("a search failed"), || 1)).is_err());
        assert_eq!(SHARE.get(), 0);
    }

    /// The session's last records bring back what the prompt does not
    /// say: a rule about the file being edited.
    #[test]
    fn the_session_tail_recalls_what_the_work_touches() {
        use crate::embed::HashEmbedder;
        use crate::record::Draft;
        use crate::store::StoreOptions;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("m.strata"), std::sync::Arc::new(HashEmbedder { dim: 64 }), StoreOptions { sync: false, ..Default::default() }).unwrap();
        let money = "src/billing/money.rs".to_string();
        store
            .append(
                "old",
                1,
                100,
                vec![
                    Draft::new(Kind::Decision, "Amounts in the billing module are integer cents, never floats").files(vec![money.clone()]),
                    Draft::new(Kind::Decision, "A new endpoint goes in src/api/routes.rs and gets an integration test"),
                    Draft::new(Kind::Decision, "Endpoints return JSON errors with a code and a message"),
                    Draft::new(Kind::Preference, "Keep pull requests small, one endpoint each"),
                ],
            )
            .unwrap();
        let first = store
            .append(
                "now",
                1,
                200,
                vec![
                    Draft::new(Kind::Prompt, "Refactor how invoice totals are rounded"),
                    Draft::new(Kind::ToolCall, "Edit src/billing/money.rs").tag("Edit").files(vec![money.clone()]),
                    Draft::new(Kind::Error, "test billing::rounding_of_totals failed: left 10.01 right 10.00").tag("Bash").files(vec![money.clone()]),
                ],
            )
            .unwrap()[0]
            .seq;
        let filter = Filter { exclude_window: Some(("now".into(), first)), ..Default::default() };
        let mut p = RankParams::default();
        p.tail = TailParams { enabled: true, ..TailParams::default() };
        let tail = Tail::of(&store.tail("now", p.tail.records).unwrap(), &p.tail);
        assert!(tail.files.contains(&money) && tail.digest.contains("rounding_of_totals"), "{tail:?}");
        assert_eq!(tail.vectors.len(), 3);
        let prompt = "now add a discount endpoint";
        let rank = |hits: &[Hit]| hits.iter().position(|h| h.record.text.contains("integer cents"));
        let (with, ev_with) = search_with(&store, prompt, Some(&tail), &filter, &p).unwrap();
        let (without, ev) = search_with(&store, prompt, None, &filter, &p).unwrap();
        // The gate reads the prompt's own evidence either way.
        assert_eq!(format!("{ev_with:?}"), format!("{ev:?}"));
        let show = |hits: &[Hit]| hits.iter().map(|h| format!("{:.3} {}", h.score, h.record.text)).collect::<Vec<_>>();
        // The billing rule the work touches gains about the tail's weight
        // (it heads the digest's list) and the file boost; an endpoint rule,
        // which the prompt names, stays first.
        let score = |hits: &[Hit]| hits.iter().find(|h| h.record.text.contains("integer cents")).map_or(0.0, |h| h.score);
        assert_eq!(rank(&without), Some(3), "{:?}", show(&without));
        assert!(score(&with) > score(&without) + p.tail.weight, "{:?} against {:?}", show(&with), show(&without));
        assert!(with[0].record.text.to_lowercase().contains("endpoint"), "{:?}", show(&with));
        // Off, the tail changes nothing.
        p.tail.enabled = false;
        let (off, _) = search_with(&store, prompt, Some(&tail), &filter, &p).unwrap();
        assert_eq!(off.iter().map(|h| &h.record.id).collect::<Vec<_>>(), without.iter().map(|h| &h.record.id).collect::<Vec<_>>());
    }

    /// CTX-17: a test run that passed, a bare test command and an empty
    /// tool answer are routine work, scored down; a test failure, a test
    /// command that names its test and a real answer are not.
    #[test]
    fn routine_work_is_boilerplate() {
        use crate::record::Draft;
        let r = |k: Kind, t: &str| Record::from_draft(Draft::new(k, t), "s", 1, 1, 0);
        for (k, t) in [
            (Kind::ToolResult, "running 42 tests\ntest t0 ... ok\ntest t1 ... ok\ntest result: ok. 42 passed; 0 failed; 0 ignored"),
            (Kind::ToolResult, "============ 12 passed in 0.31s ============"),
            (Kind::ToolResult, "ok  \texample.com/billing\t0.012s"),
            (Kind::ToolResult, "{\"stdout\":\"\",\"stderr\":\"\",\"interrupted\":false,\"isImage\":false}"),
            (Kind::ToolResult, "Done."),
            (Kind::ToolCall, "$ cargo test  # Run the test suite"),
            (Kind::ToolCall, "$ npm test -- --silent"),
            (Kind::ToolCall, "$ pytest -q"),
        ] {
            assert!(boilerplate(&r(k, t)), "{t}");
        }
        for (k, t) in [
            (Kind::ToolResult, "test result: FAILED. 41 passed; 1 failed"),
            (Kind::ToolResult, "running 42 tests\ntest t3 ... FAILED\nthread 't3' panicked at src/tax.rs:9"),
            (Kind::ToolResult, "number | count\nINV-20931 | 2"),
            (Kind::ToolCall, "$ cargo test test_tax_rounding"),
            (Kind::ToolCall, "$ cargo test && git push"),
            (Kind::Answer, "Done: 42 tests passed."),
            (Kind::Prompt, "ok"),
        ] {
            assert!(!boilerplate(&r(k, t)), "{t}");
        }
    }

    fn rate_limit_store(dir: &std::path::Path) -> Store {
        use crate::embed::HashEmbedder;
        use crate::record::Draft;
        use crate::store::StoreOptions;
        let store = Store::open(&dir.join("m.strata"), std::sync::Arc::new(HashEmbedder { dim: 256 }), StoreOptions { sync: false, ..Default::default() }).unwrap();
        let turn = |sid: &str, ts: i64, prompt: &str, answer: &str, note: Draft| {
            let mut d = vec![
                Draft::new(Kind::Prompt, prompt),
                Draft::new(Kind::ToolCall, "Edit src/ratelimit.rs: replaced\nconst PER_MINUTE: u32 = 60;").tag("Edit").files(vec!["src/ratelimit.rs".into()]),
                Draft::new(Kind::ToolCall, "$ cargo test  # Run the test suite").tag("Bash"),
                Draft::new(Kind::ToolResult, "running 42 tests\ntest t0 ... ok\ntest t1 ... ok\ntest result: ok. 42 passed; 0 failed; 0 ignored").tag("Bash"),
                Draft::new(Kind::Answer, answer),
            ];
            d.push(note);
            store.append(sid, 1, ts, d).unwrap()
        };
        let old = turn(
            "s1",
            100,
            "Set the public API rate limit. 100 requests per minute per API key sounds right.",
            "Rate limit set to 100 requests per minute per API key in src/ratelimit.rs.",
            Draft::new(Kind::Decision, "The public API rate limit is 100 requests per minute per API key.").tag("distilled"),
        );
        let note = old.last().unwrap().id.clone();
        store.retag(&note, "superseded").unwrap();
        turn(
            "s2",
            200,
            "Customers with bulk imports keep hitting 429s. Raise the API rate limit to 300 per minute per key.",
            "Raised the rate limit from 100 to 300 requests per minute per API key.",
            Draft::new(Kind::Decision, "The public API rate limit is 300 requests per minute per API key.").tag("distilled").link("supersedes", note),
        );
        // A turn about something else that shares a word with the question.
        store.append("s3", 1, 300, vec![Draft::new(Kind::Prompt, "explain what src/api/routes.rs does"), Draft::new(Kind::Answer, "routes.rs maps each API path to its handler.")]).unwrap();
        store
    }

    /// CTX-17: a question that mentions tests matches every passing run of
    /// the turns it is about. Scored down, they rank below what those turns
    /// asked and answered and below another turn's prompt, which they
    /// outranked at full weight.
    #[test]
    fn passing_test_runs_are_scored_down() {
        let dir = tempfile::tempdir().unwrap();
        let store = rate_limit_store(dir.path());
        let q = "What is the public API rate limit per key? Do the tests pass?";
        let p = RankParams::default();
        let show = |hits: &[Hit]| hits.iter().map(|h| format!("{:.3} {:?} {}", h.score, h.record.kind, h.record.text.lines().next().unwrap_or(""))).collect::<Vec<_>>();
        let at = |hits: &[Hit], f: &dyn Fn(&Record) -> bool| hits.iter().position(|h| f(&h.record)).unwrap();
        let other: &dyn Fn(&Record) -> bool = &|r| r.session == "s3" && r.kind == Kind::Prompt;
        let (hits, _) = search(&store, q, &Filter::default(), &p).unwrap();
        let first_routine = at(&hits, &boilerplate);
        let last_said = hits.iter().rposition(|h| h.record.session != "s3" && matches!(h.record.kind, Kind::Prompt | Kind::Answer)).unwrap();
        assert!(first_routine > last_said && first_routine > at(&hits, other), "{:?}", show(&hits));
        let (flat, _) = search(&store, q, &Filter::default(), &RankParams { boilerplate: 1.0, ..RankParams::default() }).unwrap();
        assert!(at(&flat, &boilerplate) < at(&flat, other), "at full weight a test run outranks the other turn: {:?}", show(&flat));
        let (id, weighted) = hits.iter().find(|h| boilerplate(&h.record)).map(|h| (h.record.id.clone(), h.score)).unwrap();
        let unweighted = flat.iter().find(|h| h.record.id == id).unwrap().score;
        assert!((weighted - unweighted * p.boilerplate).abs() < 1e-9, "{weighted} against {unweighted}");
    }

    /// CTX-17: a weak match leaves out what a turn said when a later note
    /// superseded the turn's own: the prompt that set 100 requests a minute
    /// is not shown, unmarked, for a question about something else.
    #[test]
    fn a_weak_match_leaves_out_a_superseded_turn() {
        use crate::receipt::{retrieve, Inputs};
        let dir = tempfile::tempdir().unwrap();
        let store = rate_limit_store(dir.path());
        let q = "What did we decide about adding a GraphQL API?";
        let weak = Gate { hi: f64::INFINITY, lo: f64::NEG_INFINITY, ..Gate::default() };
        let off = KnowledgeParams { enabled: false, ..KnowledgeParams::default() };
        let out = retrieve(&store, &Inputs::recall(q, "now", &Filter::default(), &HashSet::new(), false, &RankParams::default(), &PackParams::default(), &weak, &off)).unwrap();
        assert_eq!(out.decision, Some(Decision::Brief));
        assert!(!out.items.is_empty());
        let old: Vec<&Hit> = out.hits.iter().filter(|h| h.record.session == "s1" && matches!(h.record.kind, Kind::Prompt | Kind::Answer)).collect();
        assert!(!old.is_empty(), "the old turn's prompt is a candidate");
        for h in &old {
            assert!(!out.items.iter().any(|(r, _)| r.id == h.record.id), "{} shown: {:?}", h.record.text, out.items);
            let row = out.rows.iter().find(|r| r.id == h.record.id).unwrap();
            assert_eq!(row.decision, "skip: its turn was superseded");
        }
        let text = fmt_recall_as(&out.items, 1000, true) + &fmt_more(&out.hits, &out.shown, 5);
        assert!(!text.contains("100 requests per minute"), "{text}");
        // A full recall shows both turns, in order: the change is the answer.
        let strong = Gate { hi: f64::NEG_INFINITY, lo: f64::NEG_INFINITY, ..Gate::default() };
        let out = retrieve(&store, &Inputs::recall("What is the public API rate limit per key?", "now", &Filter::default(), &HashSet::new(), false, &RankParams::default(), &PackParams::default(), &strong, &off)).unwrap();
        assert_eq!(out.decision, Some(Decision::Inject));
        assert!(out.items.iter().any(|(r, _)| r.session == "s1" && r.kind == Kind::Prompt), "{:?}", out.items);
    }

    #[test]
    fn time_formats() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(fmt_time(1_791_788_086), "2026-10-12 06:54 UTC");
        assert_eq!(fmt_ago(100, 100 + 3 * 3600), "3 h ago");
    }

    #[test]
    fn snippet_finds_terms() {
        let text = format!("{} the planner probes local selectivity {}", "x ".repeat(500), "y ".repeat(500));
        let terms: HashSet<String> = chloedb::text::analyze_query("planner selectivity").into_iter().collect();
        let s = snippet(&text, &terms, 200);
        assert!(s.contains("planner probes local selectivity"), "{s}");
        assert!(s.len() <= 210);
    }
}
