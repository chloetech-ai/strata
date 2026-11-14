//! The relevance autopilot: from the queries a rank log kept, a proposed
//! change to a rank profile, with the measured before and after on the
//! database's own data, made before anything is applied.
//!
//! ```text
//! rank log ──► workload: each distinct query (its SQL and parameters) that
//!              ranked by the profile, with grades: the log's labels and the
//!              judgments table's (a person's, Claude's)
//!          ──► search: coordinate ascent (Metzler and Croft, "Linear
//!              feature-based models for information retrieval", 2007) over
//!              the profile's numbers (knobs::knobs), each candidate
//!              replayed on the training queries
//!          ──► proof: the profile now and the best candidate replayed on
//!              every query, on one snapshot, measured on the queries held
//!              out of the search: nDCG@k before and after, the queries made
//!              better and worse, the sign test, a bootstrap interval,
//!              latency; and a verdict
//! ```
//!
//! Every query reads the same [`Snapshot`], so the two sides see the same
//! rows and the same statistics, and a change in the numbers is the
//! profile's doing. A report names that snapshot's sequence number. The
//! held-out queries were not seen by the search, so the gain claimed is not
//! the gain fitted. A row a ranking shows that no one graded gains nothing;
//! the report lists those rows ([`Report::holes`]) for a grader to fill
//! (chloedb-server's autopilot asks Claude, by UMBRELA's prompt) and the
//! measure is then made again.
//!
//! A proposal is kept as a row of the proposals table ([`proposals_sql`])
//! with its report, and applied as `CREATE OR REPLACE RANK PROFILE`
//! ([`check_apply`]) only while the profile is still the one it was
//! measured against.

pub mod knobs;
pub mod metrics;

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use serde_json::{json, Map, Value as Json};

use crate::db::Db;
use crate::error::{Error, Result};
use crate::query::exec::ExecOptions;
use crate::query::Params;
use crate::storage::{RankProfile, Snapshot};
use crate::value::Value;

use knobs::Setting;

/// The judgments table the autopilot reads grades from unless told another.
pub const JUDGMENTS: &str = "relevance_judgments";
/// The table proposals are kept in.
pub const PROPOSALS: &str = "relevance_proposals";

fn bad(m: impl Into<String>) -> Error {
    Error::Query(m.into())
}

fn name_ok(table: &str) -> Result<()> {
    if table.is_empty() || !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || table.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(bad(format!("a table's name is letters, digits and _, not {table:?}")));
    }
    Ok(())
}

/// The statement that makes judgments table `table`: per query (`qid`,
/// [`qid`]) and row (`row_key`), a grade from 0 to 3 (UMBRELA's scale, and
/// the relevance lab's), who gave it (`human`, `claude`, `log`) and when;
/// `query` is the query as a person reads it. The key is `<qid>#<row_key>`.
pub fn judgments_sql(table: &str) -> Result<String> {
    name_ok(table)?;
    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {table} (id KEYWORD PRIMARY KEY, qid KEYWORD NOT NULL, query TEXT, row_key KEYWORD NOT NULL, \
         grade FLOAT NOT NULL, source KEYWORD, ts TIMESTAMP)"
    ))
}

/// The statement that makes proposals table `table`.
pub fn proposals_sql(table: &str) -> Result<String> {
    name_ok(table)?;
    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {table} (id KEYWORD PRIMARY KEY, target KEYWORD NOT NULL, profile KEYWORD NOT NULL, ts TIMESTAMP NOT NULL, \
         status KEYWORD NOT NULL, verdict KEYWORD NOT NULL, gain FLOAT, before FLOAT, after FLOAT, baseline_hash KEYWORD, \
         statement KEYWORD OPTIONS(indexed = false), proof KEYWORD OPTIONS(indexed = false))"
    ))
}

/// A judgment as a row of a judgments table ([`judgments_sql`]).
pub fn judgment_doc(qid: &str, query: &str, row_key: &str, grade: f64, source: &str, ts: i64) -> Json {
    json!({"id": format!("{qid}#{row_key}"), "qid": qid, "query": query, "row_key": row_key, "grade": grade, "source": source, "ts": crate::timestamp::format(ts)})
}

// ------------------------------------------------------------ parameters

/// Parameters as JSON that reads back as the same values
/// ([`params_from_json`]): by name, sorted; a vector as `{"$vector":
/// [...]}`, a time as `{"$ts": microseconds}`, a list as an array.
pub fn params_json(p: &Params) -> Json {
    let sorted: BTreeMap<&String, &Value> = p.0.iter().collect();
    Json::Object(sorted.into_iter().map(|(k, v)| (k.clone(), value_json(v))).collect::<Map<_, _>>())
}

fn value_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Int(x) => json!(x),
        Value::Float(x) if x.is_finite() => json!(x),
        Value::Float(_) => Json::Null,
        Value::Str(s) => json!(s),
        Value::Bool(b) => json!(b),
        Value::Vector(xs) => json!({"$vector": xs.iter().map(|x| json!(x)).collect::<Vec<_>>()}),
        Value::Timestamp(t) => json!({"$ts": t}),
        Value::List(xs) => Json::Array(xs.iter().map(value_json).collect()),
    }
}

/// A value of a row as JSON, as answers give it: a time as ISO 8601 text.
pub fn cell_json(v: &Value) -> Json {
    match v {
        Value::Timestamp(t) => json!(crate::timestamp::format(*t)),
        Value::Vector(_) => Json::Null,
        Value::List(xs) => Json::Array(xs.iter().map(cell_json).collect()),
        v => value_json(v),
    }
}

/// The parameters [`params_json`] wrote.
pub fn params_from_json(j: &Json) -> Result<Params> {
    fn value(j: &Json) -> Result<Value> {
        Ok(match j {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => match n.as_i64() {
                Some(x) => Value::Int(x),
                None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            Json::String(s) => Value::Str(s.clone()),
            Json::Array(xs) => Value::List(xs.iter().map(value).collect::<Result<_>>()?),
            Json::Object(m) => match (m.get("$vector"), m.get("$ts")) {
                (Some(Json::Array(xs)), None) if m.len() == 1 => Value::Vector(xs.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect::<Vec<_>>().into()),
                (None, Some(t)) if m.len() == 1 => Value::Timestamp(t.as_i64().ok_or_else(|| bad("a logged time is microseconds"))?),
                _ => return Err(bad(format!("a logged parameter is not {j}"))),
            },
        })
    }
    let mut p = Params::new();
    match j {
        Json::Null => {}
        Json::Object(m) => {
            for (k, v) in m {
                p = p.set(k, value(v)?);
            }
        }
        _ => return Err(bad("logged parameters are an object")),
    }
    Ok(p)
}

/// A query's identity across requests: the same SQL with the same
/// parameters (16 hex digits of FNV-1a over both). Grades are kept by it.
pub fn qid(sql_text: &str, params: &Json) -> String {
    format!("{:016x}", crate::embed::fnv(&format!("{sql_text}\u{1f}{params}")))
}

/// [`qid`] of a query and its bound parameters.
pub fn qid_of(sql_text: &str, params: &Params) -> String {
    qid(sql_text, &params_json(params))
}

/// The query as a person reads it: its text parameter (`q`, `query`,
/// `text`, else its longest), else its SQL.
pub fn query_text(sql_text: &str, params: &Json) -> String {
    let m = params.as_object();
    let named = ["q", "query", "text", "t"].iter().find_map(|k| m.and_then(|m| m.get(*k)).and_then(|v| v.as_str()));
    let longest = || m.and_then(|m| m.values().filter_map(|v| v.as_str()).max_by_key(|s| s.len()));
    named.or_else(longest).map(str::to_string).unwrap_or_else(|| sql_text.to_string())
}

// ------------------------------------------------------------ options

/// What the autopilot looks at and how hard it searches.
#[derive(Clone, Debug)]
pub struct Options {
    /// The rank log ([`crate::rank_log::TABLE`] by default).
    pub log: String,
    /// The judgments table ([`JUDGMENTS`]); its grades replace the log's
    /// labels for the same row of the same query. Not needed to exist.
    pub judgments: String,
    /// The table the profile ranks, and the profile: the pair the most
    /// logged queries rank by when `None`.
    pub table: Option<String>,
    pub profile: Option<String>,
    /// Requests logged at or after this time (microseconds), all when `None`.
    pub since: Option<i64>,
    /// The depth measured: nDCG@k.
    pub k: usize,
    /// The most recent distinct queries replayed, at most.
    pub max_queries: usize,
    /// Candidates the search may replay, at most.
    pub budget: usize,
    /// The share of judged queries held out of the search, which the
    /// verdict is drawn from.
    pub held_out: f64,
    /// Seeds the split and the bootstrap, so that a report can be made again.
    pub seed: u64,
    /// The least held-out gain in mean nDCG@k worth proposing.
    pub min_gain: f64,
    /// The sign test's level on the held-out queries.
    pub alpha: f64,
    /// The most a proposal may slow the 95th percentile of the queries,
    /// as a ratio, beyond 2 ms.
    pub max_slowdown: f64,
    /// Judged queries needed to say anything.
    pub min_queries: usize,
    /// Unjudged rows listed in the report, at most.
    pub max_holes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            log: crate::rank_log::TABLE.into(),
            judgments: JUDGMENTS.into(),
            table: None,
            profile: None,
            since: None,
            k: 10,
            max_queries: 500,
            budget: 60,
            held_out: 0.3,
            seed: 0x5eed,
            min_gain: 0.005,
            alpha: 0.05,
            max_slowdown: 1.5,
            min_queries: 20,
            max_holes: 200,
        }
    }
}

impl Options {
    /// The options as JSON, which [`Options::from_json`] reads back.
    pub fn to_json(&self) -> Json {
        json!({
            "log": self.log, "judgments": self.judgments, "table": self.table, "profile": self.profile, "since": self.since,
            "k": self.k, "max_queries": self.max_queries, "budget": self.budget, "held_out": self.held_out, "seed": self.seed,
            "min_gain": self.min_gain, "alpha": self.alpha, "max_slowdown": self.max_slowdown, "min_queries": self.min_queries, "max_holes": self.max_holes,
        })
    }

    /// The options a kept proposal (`row`, with its `proof` as JSON text
    /// or as an object) was measured with, for its table and profile: as
    /// its proof names them, or, for a proof kept before proofs named
    /// them, the defaults with the proof's seed and depth.
    pub fn of_proposal(row: &Json) -> Result<Options> {
        let proof = match row.get("proof") {
            Some(Json::String(t)) => serde_json::from_str::<Json>(t).unwrap_or(Json::Null),
            Some(j) => j.clone(),
            None => Json::Null,
        };
        let mut o = match proof.get("options") {
            Some(j @ Json::Object(_)) => Options::from_json(j)?,
            _ => {
                let mut o = Options::default();
                if let Some(seed) = proof.get("seed").and_then(|v| v.as_u64()) {
                    o.seed = seed;
                }
                if let Some(k) = proof.get("k").and_then(|v| v.as_u64()).filter(|k| *k > 0) {
                    o.k = k as usize;
                }
                o
            }
        };
        let s = |k: &str| row.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from);
        o.table = s("target");
        o.profile = s("profile");
        Ok(o)
    }

    /// Options from JSON: an object of the fields above by name (`since` as
    /// ISO 8601 text or microseconds); unknown names are refused.
    pub fn from_json(j: &Json) -> Result<Options> {
        let mut o = Options::default();
        let Some(m) = j.as_object() else {
            return if j.is_null() { Ok(o) } else { Err(bad("autopilot options are an object")) };
        };
        const NAMES: [&str; 14] = ["log", "judgments", "table", "profile", "since", "k", "max_queries", "budget", "held_out", "seed", "min_gain", "alpha", "max_slowdown", "min_queries"];
        for k in m.keys() {
            if !NAMES.contains(&k.as_str()) && k != "max_holes" {
                return Err(bad(format!("the autopilot has no option {k:?}: {}, max_holes", NAMES.join(", "))));
            }
        }
        let text = |k: &str| -> Result<Option<String>> {
            match m.get(k) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::String(s)) => Ok(Some(s.clone())),
                Some(v) => Err(bad(format!("autopilot {k} is text, not {v}"))),
            }
        };
        let count = |k: &str, d: usize| -> Result<usize> {
            match m.get(k) {
                None | Some(Json::Null) => Ok(d),
                Some(v) => v.as_u64().filter(|n| *n > 0).map(|n| n as usize).ok_or_else(|| bad(format!("autopilot {k} is a positive whole number, not {v}"))),
            }
        };
        let real = |k: &str, d: f64, lo: f64, hi: f64| -> Result<f64> {
            match m.get(k) {
                None | Some(Json::Null) => Ok(d),
                Some(v) => v.as_f64().filter(|x| *x >= lo && *x <= hi).ok_or_else(|| bad(format!("autopilot {k} is a number from {lo} to {hi}, not {v}"))),
            }
        };
        if let Some(t) = text("log")? {
            o.log = t;
        }
        if let Some(t) = text("judgments")? {
            o.judgments = t;
        }
        o.table = text("table")?;
        o.profile = text("profile")?;
        o.since = match m.get("since") {
            None | Some(Json::Null) => None,
            Some(Json::String(s)) => Some(crate::timestamp::parse(s).ok_or_else(|| bad(format!("autopilot since: {s:?} is not an ISO 8601 time")))?),
            Some(Json::Number(n)) => Some(n.as_i64().ok_or_else(|| bad("autopilot since is microseconds"))?),
            Some(v) => return Err(bad(format!("autopilot since is a time, not {v}"))),
        };
        o.k = count("k", o.k)?;
        o.max_queries = count("max_queries", o.max_queries)?;
        o.budget = count("budget", o.budget)?;
        o.min_queries = count("min_queries", o.min_queries)?;
        o.max_holes = match m.get("max_holes") {
            None | Some(Json::Null) => o.max_holes,
            Some(v) => v.as_u64().ok_or_else(|| bad(format!("autopilot max_holes is a whole number, not {v}")))? as usize,
        };
        o.held_out = real("held_out", o.held_out, 0.05, 0.95)?;
        o.min_gain = real("min_gain", o.min_gain, 0.0, 1.0)?;
        o.alpha = real("alpha", o.alpha, 0.0, 1.0)?;
        o.max_slowdown = real("max_slowdown", o.max_slowdown, 1.0, 1000.0)?;
        o.seed = match m.get("seed") {
            None | Some(Json::Null) => o.seed,
            Some(v) => v.as_u64().ok_or_else(|| bad(format!("autopilot seed is a whole number, not {v}")))?,
        };
        Ok(o)
    }
}

// ------------------------------------------------------------ the report

/// What the autopilot concludes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The candidate is better on the held-out queries, by more than the
    /// least gain, the sign test says so, and it is not too slow.
    Propose,
    /// No logged query ranks by a profile, or none kept its query.
    NoWorkload,
    /// Fewer judged queries than [`Options::min_queries`].
    TooFewJudged,
    /// The search found nothing better on the training queries.
    NoChange,
    /// Better in training, not by the least gain on the held-out queries.
    NoGain,
    /// Better on the held-out queries, but the sign test does not say so.
    NotSignificant,
    /// Better, but slower than [`Options::max_slowdown`] allows.
    Slower,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Propose => "propose",
            Verdict::NoWorkload => "no_workload",
            Verdict::TooFewJudged => "too_few_judged",
            Verdict::NoChange => "no_change",
            Verdict::NoGain => "no_gain",
            Verdict::NotSignificant => "not_significant",
            Verdict::Slower => "slower",
        }
    }
}

/// One distinct query of the workload.
#[derive(Clone, Debug)]
pub struct Query {
    pub qid: String,
    /// The query as a person reads it ([`query_text`]).
    pub text: String,
    pub sql: String,
    pub params: Params,
    /// Requests that asked it, and the last one's time.
    pub requests: u64,
    pub last: i64,
    /// Kept out of the search; the verdict is drawn from these.
    pub held_out: bool,
    /// Its graded rows, by key.
    pub grades: HashMap<String, f64>,
}

/// The measures of one set of queries, before and after.
#[derive(Clone, Debug, Default)]
pub struct Side {
    pub queries: usize,
    pub before: f64,
    pub after: f64,
    pub better: u64,
    pub worse: u64,
    /// The sign test of better against worse.
    pub p: f64,
    /// The bootstrap's 95% interval of the mean change.
    pub ci: (f64, f64),
}

impl Side {
    fn of(pairs: &[(f64, f64)], seed: u64) -> Side {
        let d: Vec<f64> = pairs.iter().map(|(b, a)| a - b).collect();
        let (better, worse) = metrics::wins(&d, 1e-9);
        Side {
            queries: pairs.len(),
            before: metrics::mean(&pairs.iter().map(|p| p.0).collect::<Vec<_>>()),
            after: metrics::mean(&pairs.iter().map(|p| p.1).collect::<Vec<_>>()),
            better,
            worse,
            p: crate::rank_log::sign_test(better, worse),
            ci: metrics::bootstrap_ci(&d, 2000, seed),
        }
    }
    pub fn delta(&self) -> f64 {
        self.after - self.before
    }
    pub fn to_json(&self) -> Json {
        json!({"queries": self.queries, "before": r4(self.before), "after": r4(self.after), "delta": r4(self.delta()), "better": self.better, "worse": self.worse, "p": r4(self.p), "ci": [r4(self.ci.0), r4(self.ci.1)]})
    }
}

fn r4(x: f64) -> f64 {
    (x * 1e4).round() / 1e4
}

/// A query measured both ways.
#[derive(Clone, Debug)]
pub struct Measured {
    pub qid: String,
    pub text: String,
    pub held_out: bool,
    pub before: f64,
    pub after: f64,
    /// The first k keys each way.
    pub keys_before: Vec<String>,
    pub keys_after: Vec<String>,
}

/// A row a ranking showed in its first k that no one graded.
#[derive(Clone, Debug, PartialEq)]
pub struct Hole {
    pub qid: String,
    pub query: String,
    pub row_key: String,
}

/// What the autopilot found, and the evidence.
#[derive(Clone, Debug)]
pub struct Report {
    pub verdict: Verdict,
    /// The verdict in a sentence.
    pub reason: String,
    pub table: Option<String>,
    pub profile: Option<RankProfile>,
    /// The candidate, when the search moved any number.
    pub candidate: Option<RankProfile>,
    /// The statement that applies the candidate.
    pub statement: Option<String>,
    /// The numbers moved: each knob, from and to.
    pub changes: Vec<(Setting, f64)>,
    /// The sequence number of the snapshot every query read.
    pub snapshot: Option<u64>,
    /// Requests read from the log, those that kept their query, distinct
    /// queries, those with a grade above 0.
    pub requests: u64,
    pub requests_with_query: u64,
    pub queries: usize,
    pub judged: usize,
    /// Queries that failed to run as the profile ranks now (left out).
    pub failed: usize,
    /// Grades read: from the log's labels and from the judgments table.
    pub grades_log: usize,
    pub grades_table: usize,
    /// Candidates replayed by the search.
    pub evaluations: usize,
    pub k: usize,
    pub train: Side,
    pub held_out: Side,
    pub all: Side,
    /// Per side, milliseconds: p50 and p95 over the judged queries.
    pub latency_before: (f64, f64),
    pub latency_after: (f64, f64),
    /// The share of the first k rows graded, before and after.
    pub judged_before: f64,
    pub judged_after: f64,
    pub per_query: Vec<Measured>,
    pub holes: Vec<Hole>,
    pub seed: u64,
    /// The options it was made with ([`Options::to_json`]), which a
    /// proposal's proof is made again with ([`reprove`]).
    pub options: Json,
    pub took_ms: f64,
}

impl Report {
    fn empty(verdict: Verdict, reason: String, o: &Options) -> Report {
        Report {
            verdict,
            reason,
            table: None,
            profile: None,
            candidate: None,
            statement: None,
            changes: Vec::new(),
            snapshot: None,
            requests: 0,
            requests_with_query: 0,
            queries: 0,
            judged: 0,
            failed: 0,
            grades_log: 0,
            grades_table: 0,
            evaluations: 0,
            k: o.k,
            train: Side::default(),
            held_out: Side::default(),
            all: Side::default(),
            latency_before: (0.0, 0.0),
            latency_after: (0.0, 0.0),
            judged_before: 0.0,
            judged_after: 0.0,
            per_query: Vec::new(),
            holes: Vec::new(),
            seed: o.seed,
            options: o.to_json(),
            took_ms: 0.0,
        }
    }

    /// The gain claimed: the held-out queries' change in mean nDCG@k.
    pub fn gain(&self) -> f64 {
        self.held_out.delta()
    }

    /// The report as JSON: the proof a proposal is kept with.
    pub fn to_json(&self) -> Json {
        let profile = |p: &Option<RankProfile>| p.as_ref().map(|p| json!({"name": p.name, "ranking": p.ranking, "defaults": p.defaults, "hash": p.hash()}));
        json!({
            "verdict": self.verdict.as_str(),
            "reason": self.reason,
            "table": self.table,
            "profile": profile(&self.profile),
            "candidate": profile(&self.candidate),
            "statement": self.statement,
            "changes": self.changes.iter().map(|(s, to)| { let mut j = s.to_json(); j["from"] = json!(s.value); j["to"] = json!(to); j["knob"] = json!(s.knob.to_string()); j }).collect::<Vec<_>>(),
            "snapshot": self.snapshot,
            "workload": {"requests": self.requests, "requests_with_query": self.requests_with_query, "queries": self.queries, "judged": self.judged, "failed": self.failed},
            "grades": {"from_log": self.grades_log, "from_table": self.grades_table},
            "evaluations": self.evaluations,
            "k": self.k,
            "metric": format!("nDCG@{}", self.k),
            "train": self.train.to_json(),
            "held_out": self.held_out.to_json(),
            "all": self.all.to_json(),
            "gain": r4(self.gain()),
            "latency_ms": {"before": {"p50": r4(self.latency_before.0), "p95": r4(self.latency_before.1)}, "after": {"p50": r4(self.latency_after.0), "p95": r4(self.latency_after.1)}},
            "judged_at_k": {"before": r4(self.judged_before), "after": r4(self.judged_after)},
            "per_query": self.per_query.iter().map(|m| json!({"qid": m.qid, "query": m.text, "held_out": m.held_out, "before": r4(m.before), "after": r4(m.after), "keys_before": m.keys_before, "keys_after": m.keys_after})).collect::<Vec<_>>(),
            "holes": self.holes.iter().map(|h| json!({"qid": h.qid, "query": h.query, "row_key": h.row_key})).collect::<Vec<_>>(),
            "seed": self.seed,
            "options": self.options,
            "took_ms": r4(self.took_ms),
        })
    }

    /// The report as a row of the proposals table ([`proposals_sql`]),
    /// status `open`, under key `id`.
    pub fn proposal_doc(&self, id: &str, ts: i64) -> Json {
        json!({
            "id": id, "target": self.table.clone().unwrap_or_default(), "profile": self.profile.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
            "ts": crate::timestamp::format(ts), "status": "open", "verdict": self.verdict.as_str(), "gain": r4(self.gain()),
            "before": r4(self.held_out.before), "after": r4(self.held_out.after),
            "baseline_hash": self.profile.as_ref().map(|p| p.hash()), "statement": self.statement, "proof": self.to_json().to_string(),
        })
    }
}

/// A new proposal's key: the time and the candidate's hash.
pub fn proposal_id(r: &Report, ts: i64) -> String {
    format!("{:x}-{}", ts / 1_000_000, r.candidate.as_ref().map(|c| c.hash()).unwrap_or_else(|| "none".into()))
}

/// The candidate and the statement that applies proposal row `row` (a
/// JSON object of its columns) to a database whose profile of that name
/// is `current` now: refused when it was applied or dismissed, when the
/// profile changed since it was measured (the proof is of another
/// profile), and, unless `force`, when its verdict was not to propose.
///
/// The row is data, which whoever may write the proposals table could
/// have changed: its statement is run only as a `CREATE OR REPLACE RANK
/// PROFILE` of the row's profile on the row's table, rebuilt from what it
/// parses to, and never as anything else (a `DROP TABLE`, a function);
/// and only when that profile is the one now with some of its numbers
/// moved ([`knobs::moved`]), never a ranking of the row's own. Its proof
/// is a claim too: [`check_reproof`] holds the proof made again.
pub fn check_apply(row: &Json, current: Option<&RankProfile>, force: bool) -> Result<(RankProfile, String)> {
    let s = |k: &str| row.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let id = s("id");
    match s("status") {
        "open" => {}
        st => return Err(bad(format!("proposal {id} is {st}: only an open proposal is applied"))),
    }
    let statement = s("statement");
    if statement.is_empty() {
        return Err(bad(format!("proposal {id} changes nothing: the search found no better numbers")));
    }
    let current_hash = current.map(|p| p.hash());
    let (Some(current), true) = (current, current_hash.as_deref() == Some(s("baseline_hash"))) else {
        return Err(bad(format!(
            "rank profile {} of {} changed since proposal {id} was measured (it was {}, it is {}): run the autopilot again for a proof of the profile as it is",
            s("profile"),
            s("target"),
            s("baseline_hash"),
            current_hash.as_deref().unwrap_or("gone")
        )));
    };
    if s("verdict") != Verdict::Propose.as_str() && !force {
        return Err(bad(format!("proposal {id}'s verdict is {}, not propose: apply it with force to apply it anyway", s("verdict"))));
    }
    let not_profile = || bad(format!("proposal {id}'s statement is not CREATE OR REPLACE RANK PROFILE {} ON {}: it is not applied", s("profile"), s("target")));
    match crate::query::profile::parse_statement(statement).map_err(|_| not_profile())? {
        Some(crate::query::profile::Statement::Create { table, profile, replace: true }) if table.eq_ignore_ascii_case(s("target")) && profile.name.eq_ignore_ascii_case(s("profile")) => {
            let profile = RankProfile { name: current.name.clone(), ..profile };
            if knobs::moved(current, &profile).map_err(|e| bad(format!("proposal {id} is not applied: {e}")))?.is_empty() {
                return Err(bad(format!("proposal {id} changes nothing: its statement is rank profile {} as it is", current.name)));
            }
            let statement = knobs::statement(&table, &profile);
            Ok((profile, statement))
        }
        _ => Err(not_profile()),
    }
}

/// Refuses to apply proposal row `row` when its proof, made again now
/// ([`reprove`], `again`), does not propose it, unless `force`: the row's
/// own verdict, gain and proof are what was written to the proposals
/// table, and the rank log and the grades may have moved since.
pub fn check_reproof(row: &Json, again: &Report, force: bool) -> Result<()> {
    if again.verdict == Verdict::Propose || force {
        return Ok(());
    }
    let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("");
    Err(bad(format!(
        "proposal {id}'s proof, made again now on the rank log and the grades as they are, does not propose it ({}: {}): run the autopilot again, or apply it with force to apply it anyway",
        again.verdict.as_str(),
        again.reason
    )))
}

// ------------------------------------------------------------ running it

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.clone()),
        Value::Int(x) => Some(x.to_string()),
        _ => None,
    }
}

fn float_of(v: &Value) -> Option<f64> {
    match v {
        Value::Float(x) if x.is_finite() => Some(*x),
        Value::Int(x) => Some(*x as f64),
        Value::Bool(b) => Some(*b as u8 as f64),
        _ => None,
    }
}

/// The table and profile a query ranks by, if it ranks by one.
fn ranks_by(sql_text: &str, snap: &Snapshot) -> Option<(String, String)> {
    let q = crate::query::sql::parse_query(sql_text).ok()?;
    let table = crate::query::compound::one_table(&q)?.1;
    let lookup = |t: &str, n: &str| snap.table(t).and_then(|s| s.rank_profile(n).cloned());
    let (_, r) = crate::query::profile::expand(&q, &lookup).ok()??;
    let table = snap.table(&table).map(|s| s.schema().table.clone()).unwrap_or(table);
    Some((table, r.name))
}

/// One query run one way: its first rows' keys and how long it took.
struct Run {
    keys: Vec<String>,
    ms: f64,
}

struct Replay<'a> {
    db: &'a Db,
    snap: &'a Snapshot,
    exec: &'a ExecOptions,
    table: String,
}

impl Replay<'_> {
    fn run(&self, q: &Query, p: &RankProfile) -> Result<Run> {
        let lookup = |t: &str, n: &str| {
            if t.eq_ignore_ascii_case(&self.table) && n.eq_ignore_ascii_case(&p.name) {
                Some(p.clone())
            } else {
                self.snap.table(t).and_then(|s| s.rank_profile(n).cloned())
            }
        };
        let text = crate::query::profile::written_out(&q.sql, &lookup)?;
        let t0 = Instant::now();
        let rs = self.db.query_at(self.snap, &text, &q.params, self.exec)?;
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        Ok(Run { keys: rs.keys.iter().filter_map(text_of).collect(), ms })
    }

    /// Mean nDCG@k over `qs`, or the first error.
    fn score(&self, qs: &[&Query], p: &RankProfile, k: usize) -> Result<f64> {
        let mut sum = 0.0;
        for q in qs {
            let r = self.run(q, p)?;
            sum += metrics::ndcg_at(&r.keys, &q.grades, k).unwrap_or(0.0);
        }
        Ok(if qs.is_empty() { 0.0 } else { sum / qs.len() as f64 })
    }
}

/// The multiplicative steps the search tries on each number, in order.
const STEPS: [f64; 6] = [2.0, 0.5, 1.25, 0.8, 4.0, 0.25];
/// The values the search tries for a knob at `x`: multiples of it, or for
/// one at 0 (a default that turns an option off, a weight not used) a few
/// values from it (a whole default rounds them: those it rounds alike are
/// replayed once).
fn tries(x: f64) -> Vec<f64> {
    if x == 0.0 {
        return vec![0.1, 0.5, 1.0, 3.0, 10.0];
    }
    STEPS.iter().map(|m| knobs::round_sig(x * m, 4)).collect()
}

/// The least training gain the search takes a step for.
const EPS: f64 = 1e-4;

/// Runs the autopilot on `snap` of `db` ([`Db::autopilot`]).
pub fn run(db: &Db, snap: &Snapshot, exec: &ExecOptions, o: &Options) -> Result<Report> {
    measure(db, snap, exec, o, None)
}

/// Makes the proof of `candidate` again on `snap` of `db`, without a
/// search: the profile now and `candidate` replayed on the logged queries
/// as [`run`] replays them, on the held-out queries of the same split, to
/// the same verdict. Applying a proposal does this first
/// ([`Db::apply_proposal`]), so that what is applied is what was measured
/// now, not what a row of the proposals table says was measured (audit 4,
/// N-C1 and A6). `candidate` must be the profile with numbers moved
/// ([`knobs::moved`]).
pub fn reprove(db: &Db, snap: &Snapshot, exec: &ExecOptions, o: &Options, candidate: &RankProfile) -> Result<Report> {
    measure(db, snap, exec, o, Some(candidate))
}

fn measure(db: &Db, snap: &Snapshot, exec: &ExecOptions, o: &Options, fixed: Option<&RankProfile>) -> Result<Report> {
    let t0 = Instant::now();
    let report = |v: Verdict, why: String| Report::empty(v, why, o);
    let Some(log) = snap.table(&o.log) else {
        return Ok(report(Verdict::NoWorkload, format!("there is no rank log {}: log queries (\"log\": true) to give the autopilot a workload", o.log)));
    };
    crate::rank_log::check(log.schema())?;
    if !crate::rank_log::keeps_query(log.schema()) {
        return Ok(report(
            Verdict::NoWorkload,
            format!("rank log {} keeps no queries (it was made before column query was added): ALTER TABLE {} ADD COLUMN query KEYWORD OPTIONS(indexed = false), then log again", o.log, o.log),
        ));
    }
    let opts = ExecOptions { max_rows: None, ..exec.clone() };
    let (filter, params) = match o.since {
        Some(t) => (" WHERE ts >= :since".to_string(), Params::new().set("since", Value::Timestamp(t))),
        None => (String::new(), Params::new()),
    };
    let rs = db.query_at(snap, &format!("SELECT request, ts, query, row_key, label FROM {}{filter}", o.log), &params, &opts)?;
    // Per request: its query and time, and its rows' labels.
    let mut asked: HashMap<String, (String, i64)> = HashMap::new();
    let mut labels: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    for row in &rs.rows {
        let Some(request) = text_of(&row[0]) else { continue };
        let ts = match &row[1] {
            Value::Timestamp(t) | Value::Int(t) => *t,
            _ => 0,
        };
        if let Some(q) = text_of(&row[2]) {
            asked.insert(request.clone(), (q, ts));
        }
        if let (Some(key), Some(l)) = (text_of(&row[3]), float_of(&row[4])) {
            labels.entry(request).or_default().push((key, l));
        }
    }
    let n_requests = rs.rows.iter().filter_map(|r| text_of(&r[0])).collect::<std::collections::HashSet<_>>().len() as u64;
    // The queries, by identity, and which profile each ranks by.
    let mut ranks: HashMap<String, Option<(String, String)>> = HashMap::new();
    let mut by_qid: BTreeMap<String, Query> = BTreeMap::new();
    let mut uses: HashMap<(String, String), u64> = HashMap::new();
    let mut of_qid: HashMap<String, (String, String)> = HashMap::new();
    let mut request_qid: Vec<(String, String)> = Vec::new();
    for (request, (q, ts)) in &asked {
        let Ok(j) = serde_json::from_str::<Json>(q) else { continue };
        let Some(sql_text) = j.get("sql").and_then(|s| s.as_str()) else { continue };
        let pj = j.get("params").cloned().unwrap_or(Json::Null);
        let Ok(params) = params_from_json(&pj) else { continue };
        let Some(pair) = ranks.entry(sql_text.to_string()).or_insert_with(|| ranks_by(sql_text, snap)).clone() else { continue };
        let id = qid(sql_text, &pj);
        *uses.entry(pair.clone()).or_default() += 1;
        of_qid.insert(id.clone(), pair);
        request_qid.push((request.clone(), id.clone()));
        let e = by_qid.entry(id.clone()).or_insert_with(|| Query {
            qid: id,
            text: query_text(sql_text, &pj),
            sql: sql_text.to_string(),
            params,
            requests: 0,
            last: i64::MIN,
            held_out: false,
            grades: HashMap::new(),
        });
        e.requests += 1;
        e.last = e.last.max(*ts);
    }
    let mut out = report(Verdict::NoWorkload, String::new());
    out.requests = n_requests;
    out.requests_with_query = asked.len() as u64;
    // The pair to improve.
    let pair = match (&o.table, &o.profile) {
        (Some(t), Some(p)) => {
            let schema = snap.table(t).ok_or_else(|| bad(format!("no table {t}")))?;
            let p = schema.rank_profile(p).ok_or_else(|| bad(format!("{t} has no rank profile {p}")))?;
            Some((schema.schema().table.clone(), p.name.clone()))
        }
        _ => {
            let mut v: Vec<_> = uses.iter().filter(|((t, p), _)| o.table.as_ref().is_none_or(|x| x.eq_ignore_ascii_case(t)) && o.profile.as_ref().is_none_or(|x| x.eq_ignore_ascii_case(p))).collect();
            v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            v.first().map(|(k, _)| (*k).clone())
        }
    };
    let Some((table, pname)) = pair else {
        out.reason = format!(
            "of {} logged requests, {} kept their query and none ranks by a rank profile{}: the autopilot tunes a profile, so rank by one (ORDER BY rank_profile('name', q => :q)) and log those queries",
            out.requests,
            out.requests_with_query,
            o.profile.as_ref().map(|p| format!(" named {p}")).unwrap_or_default()
        );
        return Ok(out);
    };
    let profile = snap.table(&table).and_then(|s| s.rank_profile(&pname).cloned()).ok_or_else(|| bad(format!("{table} has no rank profile {pname}")))?;
    out.table = Some(table.clone());
    out.profile = Some(profile.clone());
    out.snapshot = snap.seq_no();
    let mut qs: Vec<Query> = by_qid.into_values().filter(|q| of_qid.get(&q.qid).is_some_and(|p| p.0.eq_ignore_ascii_case(&table) && p.1.eq_ignore_ascii_case(&pname))).collect();
    qs.sort_by(|a, b| b.last.cmp(&a.last).then(a.qid.cmp(&b.qid)));
    qs.truncate(o.max_queries);
    out.queries = qs.len();
    // Grades: the mean label of a row over the query's requests, then the
    // judgments table's.
    let index: HashMap<String, usize> = qs.iter().enumerate().map(|(i, q)| (q.qid.clone(), i)).collect();
    let mut sums: HashMap<(usize, String), (f64, u32)> = HashMap::new();
    for (request, id) in &request_qid {
        let (Some(&i), Some(ls)) = (index.get(id), labels.get(request)) else { continue };
        for (key, l) in ls {
            let e = sums.entry((i, key.clone())).or_default();
            e.0 += l;
            e.1 += 1;
        }
    }
    out.grades_log = sums.len();
    for ((i, key), (s, n)) in sums {
        qs[i].grades.insert(key, s / n as f64);
    }
    if snap.table(&o.judgments).is_some() {
        let rs = db.query_at(snap, &format!("SELECT qid, row_key, grade FROM {}", o.judgments), &Params::new(), &opts)?;
        for row in &rs.rows {
            let (Some(id), Some(key), Some(g)) = (text_of(&row[0]), text_of(&row[1]), float_of(&row[2])) else { continue };
            if let Some(&i) = index.get(&id) {
                qs[i].grades.insert(key, g);
                out.grades_table += 1;
            }
        }
    }
    let replay = Replay { db, snap, exec, table: table.clone() };
    // The judged queries that run as the profile ranks now.
    let mut judged: Vec<Query> = Vec::new();
    for mut q in qs.into_iter().filter(|q| q.grades.values().any(|g| *g > 0.0)) {
        if replay.run(&q, &profile).is_err() {
            out.failed += 1;
            continue;
        }
        let h = crate::embed::fnv(&format!("{}{}", o.seed, q.qid));
        q.held_out = (h % 10_000) as f64 / 10_000.0 < o.held_out;
        judged.push(q);
    }
    out.judged = judged.len();
    if judged.len() < o.min_queries {
        out.verdict = Verdict::TooFewJudged;
        out.reason = format!(
            "{} of {} queries ranked by {pname} have a graded row, fewer than the {} needed: grade more (the relevance lab, the judgments table {}, or labels in the rank log)",
            judged.len(),
            out.queries,
            o.min_queries,
            o.judgments
        );
        out.took_ms = t0.elapsed().as_secs_f64() * 1e3;
        return Ok(out);
    }
    // A split with no held-out query (or none trained on) holds one out.
    if judged.iter().all(|q| !q.held_out) {
        judged[0].held_out = true;
    }
    if judged.iter().all(|q| q.held_out) {
        judged[0].held_out = false;
    }
    let train: Vec<&Query> = judged.iter().filter(|q| !q.held_out).collect();
    let start = knobs::knobs(&profile)?;
    let (changes, candidate) = match fixed {
        Some(c) => (knobs::moved(&profile, c)?, c.clone()),
        None => search(&replay, &train, &profile, &start, o, &mut out)?,
    };
    out.changes = changes;
    // The proof: both rankings on every judged query, on this snapshot,
    // the order of the two alternating so that neither always runs warm.
    let (mut lat_b, mut lat_a) = (Vec::new(), Vec::new());
    let (mut jb, mut ja) = (0usize, 0usize);
    let mut holes: Vec<Hole> = Vec::new();
    for (n, q) in judged.iter().enumerate() {
        let (b, a) = if n % 2 == 0 {
            let b = replay.run(q, &profile)?;
            (b, replay.run(q, &candidate)?)
        } else {
            let a = replay.run(q, &candidate)?;
            (replay.run(q, &profile)?, a)
        };
        lat_b.push(b.ms);
        lat_a.push(a.ms);
        jb += metrics::judged_at(&b.keys, &q.grades, o.k);
        ja += metrics::judged_at(&a.keys, &q.grades, o.k);
        let mut kb = b.keys;
        let mut ka = a.keys;
        kb.truncate(o.k);
        ka.truncate(o.k);
        for key in ka.iter().chain(kb.iter()) {
            if !q.grades.contains_key(key) && holes.len() < o.max_holes && !holes.iter().any(|h| h.qid == q.qid && &h.row_key == key) {
                holes.push(Hole { qid: q.qid.clone(), query: q.text.clone(), row_key: key.clone() });
            }
        }
        out.per_query.push(Measured {
            qid: q.qid.clone(),
            text: q.text.clone(),
            held_out: q.held_out,
            before: metrics::ndcg_at(&kb, &q.grades, o.k).unwrap_or(0.0),
            after: metrics::ndcg_at(&ka, &q.grades, o.k).unwrap_or(0.0),
            keys_before: kb,
            keys_after: ka,
        });
    }
    out.holes = holes;
    let pairs = |f: &dyn Fn(&Measured) -> bool| out.per_query.iter().filter(|m| f(m)).map(|m| (m.before, m.after)).collect::<Vec<_>>();
    out.train = Side::of(&pairs(&|m| !m.held_out), o.seed);
    out.held_out = Side::of(&pairs(&|m| m.held_out), o.seed ^ 1);
    out.all = Side::of(&pairs(&|_| true), o.seed ^ 2);
    out.latency_before = (metrics::percentile(&lat_b, 0.5), metrics::percentile(&lat_b, 0.95));
    out.latency_after = (metrics::percentile(&lat_a, 0.5), metrics::percentile(&lat_a, 0.95));
    let shown = (judged.len() * o.k).max(1) as f64;
    out.judged_before = jb as f64 / shown;
    out.judged_after = ja as f64 / shown;
    (out.verdict, out.reason) = verdict(&out, o, &pname, train.len());
    if !out.changes.is_empty() {
        out.statement = Some(knobs::statement(&table, &candidate));
        out.candidate = Some(candidate);
    }
    out.took_ms = t0.elapsed().as_secs_f64() * 1e3;
    Ok(out)
}

/// The verdict on a report's held-out queries and its latency, and why.
/// The held-out mean must rise by more than the least gain, more queries
/// must be better than worse and the sign test must tell the split from
/// chance: the sign test is two-sided, so a gain made of a few queries up
/// by much and more queries down by a little has a small p too, which
/// alone would propose it (audit 4, A1).
fn verdict(out: &Report, o: &Options, pname: &str, trained: usize) -> (Verdict, String) {
    let metric = format!("nDCG@{}", o.k);
    let h = &out.held_out;
    if out.changes.is_empty() {
        (Verdict::NoChange, format!("no change to {pname}'s numbers did better on the {trained} training queries ({} candidates replayed)", out.evaluations))
    } else if h.delta() <= o.min_gain {
        (Verdict::NoGain, format!("better in training ({:.4} to {:.4}) but not on the {} held-out queries ({:.4} to {:.4}, the least gain proposed is {})", out.train.before, out.train.after, h.queries, h.before, h.after, o.min_gain))
    } else if h.better <= h.worse {
        (Verdict::NotSignificant, format!("held-out {metric} {:.4} to {:.4}, but {} queries better and {} worse: a gain of a few queries paid for by more is not proposed", h.before, h.after, h.better, h.worse))
    } else if h.p > o.alpha {
        (Verdict::NotSignificant, format!("held-out {metric} {:.4} to {:.4}, {} queries better and {} worse, which the sign test does not tell from chance (p = {:.3} > {})", h.before, h.after, h.better, h.worse, h.p, o.alpha))
    } else if out.latency_after.1 > out.latency_before.1 * o.max_slowdown && out.latency_after.1 - out.latency_before.1 > 2.0 {
        (Verdict::Slower, format!("better ({metric} {:.4} to {:.4}) but its p95 is {:.1} ms against {:.1} ms now", h.before, h.after, out.latency_after.1, out.latency_before.1))
    } else {
        (Verdict::Propose, format!("held-out {metric} {:.4} to {:.4} ({:+.4}, 95% interval {:+.4} to {:+.4}): {} queries better, {} worse (sign test p = {:.3})", h.before, h.after, h.delta(), h.ci.0, h.ci.1, h.better, h.worse, h.p))
    }
}

/// The search: coordinate ascent over the profile's numbers, on the
/// training queries; the knobs moved and the best candidate.
#[allow(clippy::type_complexity)]
fn search(replay: &Replay, train: &[&Query], profile: &RankProfile, start: &[Setting], o: &Options, out: &mut Report) -> Result<(Vec<(Setting, f64)>, RankProfile)> {
    let mut values: Vec<f64> = start.iter().map(|s| s.value).collect();
    let mut best = replay.score(train, profile, o.k)?;
    out.evaluations = 1;
    let mut seen: HashMap<String, f64> = HashMap::new();
    'rounds: for _round in 0..4 {
        let mut moved = false;
        for i in 0..start.len() {
            for next in tries(values[i]) {
                if out.evaluations >= o.budget {
                    break 'rounds;
                }
                let mut v = values.clone();
                v[i] = next;
                let settings: Vec<Setting> = start.iter().zip(&v).map(|(s, x)| Setting { knob: s.knob.clone(), value: *x }).collect();
                let Ok(cand) = knobs::with(profile, &settings) else { continue };
                let key = cand.hash();
                let score = match seen.get(&key) {
                    Some(s) => *s,
                    None => {
                        out.evaluations += 1;
                        let s = replay.score(train, &cand, o.k).unwrap_or(f64::NEG_INFINITY);
                        seen.insert(key, s);
                        s
                    }
                };
                if score > best + EPS {
                    best = score;
                    values = v;
                    moved = true;
                    break;
                }
            }
        }
        if !moved {
            break;
        }
    }
    let settings: Vec<Setting> = start.iter().zip(&values).map(|(s, x)| Setting { knob: s.knob.clone(), value: *x }).collect();
    let changes = start.iter().zip(&values).filter(|(s, x)| s.value != **x).map(|(s, x)| (s.clone(), *x)).collect();
    Ok((changes, knobs::with(profile, &settings)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_read_back_as_they_were() {
        let p = Params::new()
            .set("q", Value::Str("rust async".into()))
            .set("v", Value::Vector(vec![1.0f32, 0.5, -2.25].into()))
            .set("n", Value::Int(3))
            .set("w", Value::Float(0.25))
            .set("at", Value::Timestamp(1_700_000_000_000_000))
            .set("ids", Value::List(vec![Value::Int(1), Value::Int(2)]));
        let j = params_json(&p);
        let back = params_from_json(&j).unwrap();
        assert_eq!(params_json(&back), j);
        assert_eq!(back.0["v"], p.0["v"]);
        assert_eq!(back.0["at"], p.0["at"]);
        assert_eq!(qid_of("SELECT 1", &p), qid_of("SELECT 1", &back));
        assert_ne!(qid_of("SELECT 1", &p), qid_of("SELECT 2", &p));
        assert_eq!(query_text("SELECT 1", &j), "rust async");
    }

    #[test]
    fn options_refuse_what_they_do_not_know() {
        let o = Options::from_json(&json!({"profile": "recall", "k": 5, "since": "2026-11-01T00:00:00Z", "held_out": 0.5})).unwrap();
        assert_eq!((o.profile.as_deref(), o.k, o.held_out), (Some("recall"), 5, 0.5));
        assert!(o.since.is_some());
        assert!(Options::from_json(&json!({"profiles": "x"})).unwrap_err().to_string().contains("no option"));
        assert!(Options::from_json(&json!({"k": 0})).is_err());
    }

    #[test]
    fn apply_is_refused_when_stale_closed_or_not_proposed() {
        let (now, other) = (profile("SELECT * FROM t ORDER BY 0.5 * x DESC"), profile("SELECT * FROM t ORDER BY 0.7 * x DESC"));
        let row = row_of(&now, "CREATE OR REPLACE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY 0.25 * x DESC");
        assert!(check_apply(&row, Some(&now), false).is_ok());
        assert!(check_apply(&row, Some(&other), false).unwrap_err().to_string().contains("changed since"));
        assert!(check_apply(&row, None, true).unwrap_err().to_string().contains("gone"));
        let mut weak = row.clone();
        weak["verdict"] = json!("not_significant");
        assert!(check_apply(&weak, Some(&now), false).unwrap_err().to_string().contains("force"));
        assert!(check_apply(&weak, Some(&now), true).is_ok());
        let mut done = row.clone();
        done["status"] = json!("applied");
        assert!(check_apply(&done, Some(&now), true).is_err());
    }

    fn profile(ranking: &str) -> RankProfile {
        RankProfile { name: "p".into(), ranking: ranking.into(), defaults: Default::default() }
    }

    fn row_of(now: &RankProfile, statement: &str) -> Json {
        json!({"id": "p1", "status": "open", "statement": statement, "baseline_hash": now.hash(), "profile": "p", "target": "t", "verdict": "propose"})
    }

    #[test]
    fn apply_runs_only_the_rank_profile_of_the_row() {
        let now = profile("SELECT * FROM t ORDER BY 0.5 * x DESC");
        let row = row_of(&now, "CREATE OR REPLACE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY 0.25 * x DESC");
        assert!(check_apply(&row, Some(&now), false).unwrap().1.starts_with("CREATE OR REPLACE RANK PROFILE p ON t AS "));
        for bad in [
            "DROP TABLE orders",
            "CREATE FUNCTION f(x INT) RETURNS INT AS 'x'",
            "CREATE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY x DESC",
            "CREATE OR REPLACE RANK PROFILE p ON orders AS SELECT * FROM orders ORDER BY x DESC",
            "CREATE OR REPLACE RANK PROFILE q ON t AS SELECT * FROM t ORDER BY x DESC",
            "DROP RANK PROFILE p ON t",
            "CREATE OR REPLACE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY x DESC; DROP TABLE orders",
        ] {
            let mut r = row.clone();
            r["statement"] = json!(bad);
            let e = check_apply(&r, Some(&now), true).unwrap_err().to_string();
            assert!(e.contains("is not applied"), "{bad}: {e}");
        }
        // A planted ranking of the row's own (audit 4, N-C1): the right
        // statement, the right profile and table, a verdict to propose and
        // a large gain, refused even with force, because it is not the
        // profile now with its numbers moved.
        let mut forged = row_of(&now, "CREATE OR REPLACE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY CASE WHEN vendor = 'me' THEN 1e9 ELSE 0.5 * x END DESC");
        forged["gain"] = json!(0.9);
        let e = check_apply(&forged, Some(&now), true).unwrap_err().to_string();
        assert!(e.contains("numbers moved"), "{e}");
        // Nothing moved: nothing to apply.
        let same = row_of(&now, "CREATE OR REPLACE RANK PROFILE p ON t AS SELECT * FROM t ORDER BY 0.5 * x DESC");
        assert!(check_apply(&same, Some(&now), true).unwrap_err().to_string().contains("changes nothing"));
    }

    #[test]
    fn a_few_queries_up_by_much_and_more_down_do_not_propose() {
        // Audit 4, A1: 2 held-out queries up by 0.5 and 12 down by 0.01
        // pass the least gain and a two-sided sign test (p = 0.013).
        let o = Options::default();
        let mut r = Report::empty(Verdict::NoWorkload, String::new(), &o);
        r.changes = vec![(knobs::Setting { knob: knobs::Knob::Default("w".into()), value: 1.0 }, 2.0)];
        let mut pairs = vec![(0.2, 0.7); 2];
        pairs.extend(vec![(0.5, 0.49); 12]);
        r.held_out = Side::of(&pairs, 1);
        assert!(r.held_out.delta() > o.min_gain && r.held_out.p < o.alpha, "{:?}", r.held_out);
        let (v, why) = verdict(&r, &o, "p", 30);
        assert_eq!(v, Verdict::NotSignificant, "{why}");
        // The same gain from more queries better than worse proposes.
        let mut pairs = vec![(0.2, 0.3); 12];
        pairs.extend(vec![(0.5, 0.49); 2]);
        r.held_out = Side::of(&pairs, 1);
        assert_eq!(verdict(&r, &o, "p", 30).0, Verdict::Propose);
    }

    #[test]
    fn a_proposal_names_the_options_it_was_measured_with() {
        let o = Options::from_json(&json!({"min_queries": 5, "seed": 7, "alpha": 0.01, "since": 12})).unwrap();
        let back = Options::from_json(&o.to_json()).unwrap();
        assert_eq!((back.min_queries, back.seed, back.alpha, back.since), (5, 7, 0.01, Some(12)));
        let row = json!({"target": "t", "profile": "p", "proof": json!({"options": o.to_json()}).to_string()});
        let p = Options::of_proposal(&row).unwrap();
        assert_eq!((p.table.as_deref(), p.profile.as_deref(), p.min_queries, p.seed), (Some("t"), Some("p"), 5, 7));
        // A proof kept before proofs named their options.
        let old = Options::of_proposal(&json!({"target": "t", "profile": "p", "proof": {"seed": 9, "k": 5}})).unwrap();
        assert_eq!((old.seed, old.k, old.min_queries), (9, 5, Options::default().min_queries));
    }
}
