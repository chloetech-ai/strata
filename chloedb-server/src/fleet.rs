//! Fleet health (`GET /v1/health/fleet`): whether the cluster is all
//! right and, when it is not, what is wrong first, in one answer the
//! command center's Home shows as its headline.
//!
//! It judges what the coordinator already holds: the shared cluster
//! snapshot (each shard's state, each copy's detail) and its own points
//! over the last five minutes (`WINDOW_S`), which describe the clients'
//! searches. Each check says `ok`, `warning` or `critical`; each problem
//! says where it is, since when, what it does to clients and what to do
//! next, worst first. On a shard server the same checks describe that
//! server alone.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value as Json};

/// The window of the SLO checks, in seconds.
pub const WINDOW_S: u64 = 300;
/// How long a copy's disk use is remembered to tell how fast it grows.
const DISK_MEMORY_MS: u64 = 3_600_000;
/// The shortest span of disk use that a growth rate is read from.
const DISK_SPAN_MS: u64 = 600_000;

/// What fleet health remembers between answers: since when its status
/// and each problem hold, and each copy's disk use over the last hour.
#[derive(Default)]
pub struct Fleet {
    memory: Mutex<Memory>,
}

#[derive(Default)]
struct Memory {
    status: Option<(&'static str, u64)>,
    /// Since when the fleet has not been all right (fleet: apart from the
    /// headline problem's own since).
    not_ok: Option<u64>,
    problems: HashMap<String, u64>,
    disk: HashMap<String, VecDeque<(u64, f64)>>,
    /// Copies down here that another coordinator reaches: since when that
    /// was first seen (ms), by URL. Said cut off once a survey of this
    /// coordinator begun since failed again, not before: a copy coming
    /// back answers another coordinator's survey a few seconds before
    /// this one's.
    cut_seen: HashMap<String, u64>,
    /// When this coordinator's survey last found each copy down (ms), by
    /// URL: another coordinator's report that it cannot reach a copy counts
    /// only from a survey begun since, as one begun before may have asked
    /// while the copy was down for all.
    down_seen: HashMap<String, u64>,
}

/// The window the headline judges searches by now: the last minute (the
/// SLO checks keep [`WINDOW_S`]), so that an outage shows within seconds
/// and its end as soon as searches answer again.
pub const RECENT_S: u64 = 60;
/// The fewest failures in [`RECENT_S`] that say searches fail now (one
/// failure in fifty searches is not an outage).
const RECENT_FAILURES: f64 = 3.0;

/// What another coordinator says of its searches ([`crate::peers`]):
/// over [`WINDOW_S`] and [`RECENT_S`], or why it did not say.
#[derive(Clone, Debug, Default)]
pub struct PeerWindow {
    pub url: String,
    pub name: String,
    pub window: Window,
    pub recent: Window,
    pub error: Option<String>,
    /// The copies that did not answer its last survey of the cluster, by
    /// URL, and when that survey began (ms); `None` from a coordinator
    /// that does not say.
    pub unreached: Option<Vec<String>>,
    pub surveyed_ms: u64,
}

/// How much later than a copy stopped answering this coordinator another
/// coordinator's survey begins, at least, for its answer to show that the
/// copy answers it still: a survey waits 2 s for each copy.
const CUT_MARGIN_MS: u64 = 3000;

/// What a judgement reads.
pub struct Inputs<'a> {
    /// The cluster: `{"shards": [...]}` as `/v1/cluster` has it, each shard
    /// with its `state`, `why` and copies.
    pub cluster: &'a Json,
    /// This server's points over the window.
    pub points: &'a [Arc<Json>],
    /// And over the last hour (the series holds an hour): a failure that
    /// began before the window began then (R2-scenarios-4).
    pub history: &'a [Arc<Json>],
    pub slo_error_ratio: f64,
    pub slo_p99_ms: f64,
    /// The alerts firing here that nobody silenced, as `/v1/alerts`'
    /// `states` has them (a coordinator's each_copy rules gather its
    /// copies': each counts once, `where` naming the copies).
    pub alerts: Vec<Json>,
    /// Whether searches leave out a shard that does not answer.
    pub partial: bool,
    /// This server, when it is a coordinator: `{"url", "name"}`.
    pub coordinator: Option<Json>,
    /// The other coordinators' searches (fleet): the SLO checks judge the
    /// sum of every coordinator's.
    pub peers: Vec<PeerWindow>,
    /// The coordinators the shards stopped hearing from in the last 10
    /// minutes (R2-workload-3): `(name, url, last seen)`.
    pub gone: Vec<(String, String, u64)>,
    pub now_ms: u64,
}

/// A problem found, with what ranks it.
struct Problem {
    rank: u32,
    critical: bool,
    headline: String,
    j: Json,
}

/// A problem's `next` for an agent too: `api`, the read its view makes,
/// and `runbook`, the title of the runbook for it ([`crate::runbooks`]).
fn for_agents(mut j: Json) -> Json {
    let view = j["next"]["view"].as_str().unwrap_or_default().to_string();
    let params = &j["next"]["params"];
    let api = match view.as_str() {
        "cluster" => Some("GET /v1/cluster".to_string()),
        "performance" => Some("GET /v1/queries/slow".to_string()),
        "queries" => Some("GET /v1/queries/recent?outcome=error".to_string()),
        "tables" => Some("GET /v1/tables".to_string()),
        "alerts" => Some("GET /v1/alerts".to_string()),
        "logs" => Some(match params["q"].as_str() {
            Some(q) => format!("GET /v1/logs?q={}", crate::mcp::encode(q)),
            None => "GET /v1/logs".to_string(),
        }),
        v => v.strip_prefix("alert/").map(|r| format!("GET /v1/alerts/{r}")),
    };
    if let (Some(api), true) = (api, j["next"].is_object()) {
        j["next"]["api"] = json!(api);
    }
    let id = j["id"].as_str().unwrap_or_default().to_string();
    if let (Some(r), true) = (crate::runbooks::for_id(&id), j["next"].is_object()) {
        j["next"]["runbook"] = json!(r.title);
    }
    j
}

fn severity(critical: bool) -> &'static str {
    if critical {
        "critical"
    } else {
        "warning"
    }
}

fn worst(a: &'static str, b: &str) -> &'static str {
    match (a, b) {
        ("critical", _) | (_, "critical") => "critical",
        ("warning", _) | (_, "warning") => "warning",
        _ => "ok",
    }
}

/// `a`, `a or b`, `a, b or c`.
fn either(v: &[String]) -> String {
    match v {
        [] => String::new(),
        [a] => a.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

/// `a`, `a and b`, `a, b and c`.
fn both(v: &[String]) -> String {
    match v {
        [] => String::new(),
        [a] => a.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn pct(r: f64) -> String {
    let p = r * 100.0;
    if p == 0.0 {
        "0%".into()
    } else if p < 0.1 {
        format!("{p:.2}%")
    } else if p < 10.0 {
        format!("{p:.1}%").replace(".0%", "%")
    } else {
        format!("{p:.0}%")
    }
}

/// The searches of the window: how many ended, how many failed on the
/// server's side, the latency histogram, and the mean rate and CPU.
#[derive(Clone, Debug, Default)]
pub struct Window {
    pub queries: f64,
    pub failures: f64,
    pub hist: Vec<u64>,
    pub secs: f64,
    pub cpu_s: f64,
    /// Points, oldest first, with their failure ratio (`None` without
    /// searches).
    ratios: Vec<(u64, Option<f64>)>,
}

impl Window {
    pub fn of(points: &[Arc<Json>]) -> Window {
        let mut w = Window::default();
        for p in points {
            let secs = p["secs"].as_f64().unwrap_or(2.0);
            // Counts where the point has them (server-side failures only);
            // else from the rates.
            let n = p["queries"].as_f64().unwrap_or_else(|| p["qps"].as_f64().unwrap_or(0.0) * secs);
            let f = p["query_errors"].as_f64().unwrap_or_else(|| p["errors_s"].as_f64().unwrap_or(0.0) * secs);
            w.queries += n;
            w.failures += f;
            w.secs += secs;
            w.cpu_s += p["cpu"].as_f64().unwrap_or(0.0) * secs;
            if let Some(h) = p["latency_hist"].as_array() {
                w.hist.resize(w.hist.len().max(h.len()), 0);
                for (i, c) in h.iter().enumerate() {
                    w.hist[i] += c.as_u64().unwrap_or(0);
                }
            }
            w.ratios.push((p["t"].as_u64().unwrap_or(0), (n > 0.0).then(|| f / n)));
        }
        w
    }

    /// The points of `points` from `from_ms` on.
    pub fn since(points: &[Arc<Json>], from_ms: u64) -> Window {
        let i = points.iter().position(|p| p["t"].as_u64().unwrap_or(0) >= from_ms).unwrap_or(points.len());
        Window::of(&points[i..])
    }

    /// What another coordinator is sent of it (`GET /v1/coordinators?window=1`).
    pub fn json(&self) -> Json {
        json!({"queries": self.queries, "failures": self.failures, "hist": self.hist, "secs": self.secs, "cpu_s": self.cpu_s})
    }

    pub fn from_json(j: &Json) -> Window {
        let f = |k: &str| j[k].as_f64().unwrap_or(0.0);
        Window { queries: f("queries"), failures: f("failures"), hist: j["hist"].as_array().map(|h| h.iter().map(|c| c.as_u64().unwrap_or(0)).collect()).unwrap_or_default(), secs: f("secs"), cpu_s: f("cpu_s"), ratios: Vec::new() }
    }

    /// `other`'s searches added to these (another coordinator's, over the
    /// same span): counts, CPU and histograms summed over the longer span,
    /// so that rates add up.
    pub fn add(&mut self, other: &Window) {
        self.queries += other.queries;
        self.failures += other.failures;
        self.cpu_s += other.cpu_s;
        self.secs = self.secs.max(other.secs);
        self.hist.resize(self.hist.len().max(other.hist.len()), 0);
        for (i, c) in other.hist.iter().enumerate() {
            self.hist[i] += c;
        }
    }

    pub fn error_ratio(&self) -> Option<f64> {
        (self.queries > 0.0).then(|| self.failures / self.queries)
    }

    /// The latest run of points whose failure ratio is over `over`: its
    /// first and last point, and how many searches of it failed. Points
    /// without searches do not break a run.
    fn last_run(&self, over: f64) -> Option<(u64, u64, f64)> {
        let mut run: Option<(u64, u64, f64)> = None;
        let mut open = false;
        for &(t, r) in &self.ratios {
            match r {
                Some(r) if r > over => {
                    match (&mut run, open) {
                        (Some(x), true) => {
                            x.1 = t;
                            x.2 = x.2.max(r);
                        }
                        _ => run = Some((t, t, r)),
                    }
                    open = true;
                }
                Some(_) => open = false,
                None => {}
            }
        }
        run
    }

    pub fn qps(&self) -> Option<f64> {
        (self.secs > 0.0).then(|| self.queries / self.secs)
    }

    pub fn cpu(&self) -> Option<f64> {
        (self.secs > 0.0).then(|| self.cpu_s / self.secs)
    }

    pub fn p99_ms(&self) -> Option<f64> {
        if self.hist.iter().sum::<u64>() == 0 {
            return None;
        }
        crate::telemetry::quantile(&crate::metrics::BUCKETS, &self.hist, 0.99).map(|s| s * 1e3)
    }

    /// Since when the points' failure ratio stays over `slo`: the first
    /// point of the latest run of them.
    fn over_since(&self, slo: f64) -> Option<u64> {
        let mut since = None;
        for &(t, r) in &self.ratios {
            match r {
                Some(r) if r > slo => {
                    since.get_or_insert(t);
                }
                Some(_) => since = None,
                None => {}
            }
        }
        since
    }
}

/// A span of time in words: "45 s", "6 min", "2 h 5 min".
fn dur(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=89 => format!("{s} s"),
        90..=5399 => format!("{} min", (s + 30) / 60),
        _ => {
            let m = (s + 30) / 60;
            if m % 60 == 0 { format!("{} h", m / 60) } else { format!("{} h {} min", m / 60, m % 60) }
        }
    }
}

/// How long until a disk fills, in words: hours under a day, else days.
fn fills_in(days: f64) -> String {
    if days < 1.0 / 24.0 {
        format!("{} min", (days * 1440.0).round().max(1.0))
    } else if days < 1.0 {
        format!("{} h", (days * 24.0).round())
    } else if days < 10.0 {
        format!("{:.1} days", days).replace(".0 days", " days")
    } else {
        format!("{:.0} days", days)
    }
}

/// What to do about a shard whose primary answers but cannot write until
/// it restarts (`failing`: a write failed partway, a full disk): hand the
/// primary over to a replica in sync, so that writes go on at once, then
/// fix the old primary (finding chaos-4: a restart alone, which the advice
/// said, fails again while the disk is full, and loses the shard's writes
/// meanwhile). None when the primary is not failing.
fn hand_over(copies: &[Json], primary: &Json, failover: bool) -> Option<String> {
    let p = copies.iter().find(|r| &r["url"] == primary && !down(r))?;
    if p["detail"]["health"]["status"] != "failing" {
        return None;
    }
    let name = short(p);
    let full = p["detail"]["load"]["disk_used_ratio"].as_f64().filter(|&d| d >= 0.95);
    let fix = match full {
        Some(d) => format!("free {name}'s volume ({:.0}% full) and restart it", d * 100.0),
        None => format!("fix the cause on {name} (its logs say what failed) and restart it"),
    };
    let in_sync = p["detail"]["health"]["replication"]["replicas"].as_array().into_iter().flatten().filter(|f| f["in_sync"] == true && f["copying"] != true).find_map(|f| copies.iter().find(|r| r["url"] == f["url"] && !down(r)));
    Some(match in_sync {
        Some(r) => format!(
            "Make {} the primary (its menu in Cluster: Make it the primary): it is in sync, so writes go on at once{}. Then {fix}",
            short(r),
            if failover { "; with failover on, the primary hands itself over within seconds" } else { "" }
        ),
        None => {
            let mut t = fix;
            t[..1].make_ascii_uppercase();
            format!("{t}: no replica is in sync to take over")
        }
    })
}

/// A copy's short name (`s1-0`), else its url.
fn short(r: &Json) -> String {
    r["short"].as_str().or(r["url"].as_str()).unwrap_or("?").to_string()
}

/// How much more of its disk a copy uses than its shard's other copies
/// before it is told, besides using over twice theirs.
const HEAVY_DISK_BYTES: f64 = 1e9;

fn down(r: &Json) -> bool {
    r["status"] == "down"
}

impl Fleet {
    /// Fleet health of `i` (see the module's doc).
    pub fn judge(&self, i: Inputs) -> Json {
        let mut m = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        let now = i.now_ms;
        let shards: Vec<Json> = i.cluster["shards"].as_array().cloned().unwrap_or_default();
        // A server that is not a coordinator, as one shard of itself
        // ([`local_shard`]): its problems are this server's, not shard 0's.
        let alone = shards.len() == 1 && shards[0]["local"] == true;
        let at_shard = |shard: usize, name: &str| if alone { vec![name.to_string()] } else { vec![format!("shard {shard}"), name.to_string()] };
        let mut checks = Vec::new();
        let mut problems: Vec<Problem> = Vec::new();

        // ----- what the other coordinators reach (Home over every
        // coordinator): a copy that does not answer this coordinator, but
        // answered another's survey begun since, is cut off from this one,
        // not down.
        let me = i.coordinator.as_ref().map(|c| c["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("this coordinator").to_string());
        let said: Vec<&PeerWindow> = if me.is_some() { i.peers.iter().filter(|p| p.error.is_none() && p.unreached.is_some()).collect() } else { Vec::new() };
        // When this coordinator's own survey began (a judgement without one
        // takes what it is given as of now).
        let surveyed = i.cluster["at_ms"].as_u64().unwrap_or(u64::MAX);
        let all_copies = || shards.iter().flat_map(|s| s["replicas"].as_array().into_iter().flatten());
        let mut cut_here: HashMap<String, Vec<String>> = HashMap::new();
        let mut seen = std::mem::take(&mut m.cut_seen);
        for r in all_copies().filter(|r| down(r)) {
            let url = r["url"].as_str().unwrap_or_default();
            let since = r["down_since_ms"].as_u64().unwrap_or(u64::MAX - CUT_MARGIN_MS);
            let by: Vec<String> = said.iter().filter(|p| p.surveyed_ms > since + CUT_MARGIN_MS && !p.unreached.iter().flatten().any(|u| u == url)).map(|p| p.name.clone()).collect();
            if by.is_empty() {
                continue;
            }
            let first = *seen.entry(url.to_string()).or_insert(now);
            m.cut_seen.insert(url.to_string(), first);
            if surveyed > first || surveyed == u64::MAX {
                cut_here.insert(url.to_string(), by);
            }
        }
        drop(seen);
        for r in all_copies().filter(|r| down(r)) {
            m.down_seen.insert(r["url"].as_str().unwrap_or_default().to_string(), surveyed.min(now));
        }
        m.down_seen.retain(|_, t| *t + 600_000 > now);
        let reached_by = |r: &Json| -> Vec<String> { cut_here.get(r["url"].as_str().unwrap_or_default()).cloned().unwrap_or_default() };

        // ----- shards
        let (mut no_reads, mut no_writes, mut degraded, mut downs, mut cut_off) = (0, 0, 0, 0, 0);
        let mut where_shards = Vec::new();
        for s in &shards {
            let n = s["shard"].as_u64().unwrap_or(0);
            let state = s["state"].as_str().unwrap_or("ok");
            let copies = s["replicas"].as_array().cloned().unwrap_or_default();
            let gone: Vec<String> = copies.iter().filter(|r| down(r)).map(short).collect();
            let up: Vec<String> = copies.iter().filter(|r| !down(r)).map(short).collect();
            // Down to the coordinators, but answering: started with another
            // --cluster-token.
            let refusing: Vec<String> = copies.iter().filter(|r| r["refuses_token"] == true).map(short).collect();
            let since = copies.iter().filter_map(|r| r["down_since_ms"].as_u64()).min();
            let why = s["why"].as_str().unwrap_or("").to_string();
            let name = format!("shard {n}");
            let mut where_: Vec<String> = vec![name.clone()];
            where_.extend(gone.iter().cloned());
            let next = |text: String| json!({"text": text, "view": "cluster", "params": {"shard": n}});
            let searches_fail = if i.partial { format!("Searches answer without {name}'s rows") } else { format!("Every search fails: each one needs {name}") };
            let p = match state {
                "ok" => None,
                "down" => {
                    downs += 1;
                    Some((0, true, format!("Searches are failing: {name} is down"), format!("Shard {n} is down: none of its {} copies answers", copies.len()), format!("{searches_fail}, and writes to its rows fail"), next(format!("Bring back {}", either(&gone)))))
                }
                "no_reads" => {
                    no_reads += 1;
                    let majority_lost = s["failover"] == true && s["majority"] == false;
                    let (head, title, todo) = if majority_lost {
                        let them = if gone.len() == 1 { "it" } else { "them" };
                        let has = if up.len() == 1 { "has" } else { "have" };
                        (
                            format!("Searches are failing: {name} has no majority"),
                            format!("Shard {n} has no majority, so it serves no reads"),
                            format!("Bring {} back, or remove {them} so {} {has} a majority", either(&gone), both(&up)),
                        )
                    } else {
                        (format!("Searches are failing: {name} serves no reads"), format!("Shard {n} serves no reads"), format!("Find out why on its copies: {why}"))
                    };
                    Some((1, true, head, title, searches_fail, next(todo)))
                }
                "no_writes" => {
                    no_writes += 1;
                    let elects = s["majority"] != false;
                    let todo = if s["primary"].is_null() && s["failover"] == true && !elects {
                        // No majority answers: no election can win.
                        let mut t = crate::coord::elect_by_hand(&gone, &up);
                        t[..1].make_ascii_uppercase();
                        t
                    } else if s["primary"].is_null() {
                        // A majority answers: the votes of those that do make
                        // the most up-to-date one the primary; the server
                        // removes a dead primary only once another is
                        // (finding chaos-2).
                        let best = copies.iter().filter(|r| !down(r)).max_by_key(|r| (r["term"].as_u64().unwrap_or(0), r["txn"].as_u64().unwrap_or(0))).map(short);
                        let make = best.map_or("make an up-to-date copy the primary".to_string(), |b| format!("make {b} the primary (its menu in Cluster: Make it the primary)"));
                        let then = if gone.is_empty() { String::new() } else { format!(", then replace {}", both(&gone)) };
                        if s["failover"] == true {
                            format!("An election should choose a primary within seconds. If none does, {make}{then}")
                        } else {
                            let mut t = format!("{make}{then}");
                            t[..1].make_ascii_uppercase();
                            t
                        }
                    } else if let Some(t) = hand_over(&copies, &s["primary"], s["failover"] == true) {
                        t
                    } else {
                        format!("Find out why on its primary: {why}")
                    };
                    let head = if s["primary"].is_null() { format!("Writes are failing: {name} has no primary") } else { format!("Writes are failing: {name} takes no writes") };
                    let title = if s["primary"].is_null() && s["failover"] == true && !elects {
                        // "Shard 0 cannot elect a primary or take writes until a second copy answers"
                        let c = crate::coord::cannot_elect(copies.len(), up.len());
                        format!("Shard {n} {}", c.trim_start_matches("it ").replacen("elect a primary", "elect a primary or take writes", 1))
                    } else {
                        format!("Shard {n} takes no writes")
                    };
                    Some((2, true, head, title, format!("Writes to {name}'s rows fail; searches still answer"), next(todo)))
                }
                _ if !refusing.is_empty() && refusing.len() == gone.len() => {
                    degraded += 1;
                    let (who, them, refuses) = (both(&refusing), if refusing.len() == 1 { "it" } else { "them" }, if refusing.len() == 1 { "refuses" } else { "refuse" });
                    let more = if why.contains("one more copy down") { "; one more copy down stops its reads and writes" } else { "" };
                    Some((
                        10,
                        false,
                        format!("{who} {refuses} the cluster's token; {name} still serves reads and takes writes"),
                        format!("Shard {n}'s {who} {} up but {refuses} the cluster's token", if refusing.len() == 1 { "is" } else { "are" }),
                        format!("None yet: the coordinators read from its other copies{more}"),
                        next(format!("Start {who} again with the cluster's --cluster-token, as its other copies have it, or replace {them}")),
                    ))
                }
                _ => {
                    degraded += 1;
                    let more = if why.contains("one more copy down") { "; one more copy down stops its reads and writes" } else { "" };
                    let them = if gone.len() == 1 { "it" } else { "them" };
                    Some((
                        10,
                        false,
                        format!("{} of {name}'s {} copies {} down; it still serves reads and takes writes", gone.len(), copies.len(), if gone.len() == 1 { "is" } else { "are" }),
                        format!("Shard {n} has {} of {} copies down", gone.len(), copies.len()),
                        format!("None yet: the shard serves reads and takes writes{more}"),
                        next(format!("Bring {} back, or replace {them}", either(&gone))),
                    ))
                }
            };
            let p = p.map(|(rank, critical, headline, title, impact, mut next)| {
                if !refusing.is_empty() && !headline.contains("cluster's token") {
                    let todo = next["text"].as_str().unwrap_or_default().to_string();
                    let refuses = if refusing.len() == 1 { "answers but refuses" } else { "answer but refuse" };
                    next["text"] = json!(format!("{todo}. {} {refuses} the cluster's token: start it again with the cluster's --cluster-token", both(&refusing)));
                }
                (rank, critical, headline, title, impact, next)
            });
            // Every copy down here answers another coordinator: this
            // coordinator is cut off from them, and the shard serves the
            // other coordinators' clients.
            let mut by: Vec<String> = copies.iter().filter(|r| down(r)).flat_map(reached_by).collect();
            by.sort();
            by.dedup();
            let only_here = !gone.is_empty() && copies.iter().filter(|r| down(r)).all(|r| !reached_by(r).is_empty());
            let mut id = format!("shard-{n}-{state}");
            let p = match p {
                Some(p) if only_here => {
                    let me = me.as_deref().unwrap_or("this coordinator");
                    let (them, reach) = (if gone.len() == 1 { "it" } else { "them" }, if by.len() == 1 { "reaches" } else { "reach" });
                    let look = format!("Look at the network between {me} and {} (a firewall, a network policy, DNS): {} {reach} {them}", both(&gone), both(&by));
                    // A shard that fails here stays critical: this
                    // coordinator's clients fail.
                    if !p.1 {
                        degraded -= 1;
                        cut_off += 1;
                    }
                    id = format!("shard-{n}-cut-off");
                    if p.1 {
                        // Its clients fail for real: critical still, said as it is.
                        let what = if state == "no_writes" { "Writes" } else { "Searches" };
                        let lower = |t: &str| t.get(..1).map_or(String::new(), |c| c.to_lowercase() + &t[1..]);
                        Some((
                            p.0,
                            true,
                            format!("{what} through this coordinator are failing: it cannot reach {}, which {} {reach}", both(&gone), both(&by)),
                            format!("From {me}, {}; {} {reach} {}", lower(&p.3), both(&by), both(&gone)),
                            format!("Clients of {me}: {}. Clients of {} are served", lower(&p.4), both(&by)),
                            next(format!("{look}; meanwhile its clients can use {}", either(&by))),
                        ))
                    } else {
                        Some((13, false, format!("This coordinator cannot reach {}, which {} {reach}", both(&gone), both(&by)), format!("{me} cannot reach {} of {name}, which {} {reach}", both(&gone), both(&by)), format!("None: {name} serves reads and takes writes, and searches through {me} go to its other copies"), next(look)))
                    }
                }
                p => p,
            };
            if let Some((rank, critical, headline, title, impact, next)) = p {
                where_shards.push(name.clone());
                problems.push(Problem {
                    rank,
                    critical,
                    headline,
                    j: json!({"id": id, "severity": severity(critical), "title": title, "where": where_, "since_ms": since, "impact": impact, "next": next, "why": why}),
                });
            }
        }
        let shards_status = if downs + no_reads + no_writes > 0 {
            "critical"
        } else if degraded + cut_off > 0 {
            "warning"
        } else {
            "ok"
        };
        checks.push(json!({"id": "shards", "status": shards_status, "value": {"down": downs, "no_reads": no_reads, "no_writes": no_writes, "degraded": degraded, "cut_off": cut_off, "total": shards.len()}, "where": where_shards}));

        // ----- the clients' searches over the window: every coordinator's
        // (fleet); the headline judges the last minute, the SLO checks the
        // window.
        let own = Window::of(i.points);
        let own_recent = Window::since(i.points, now.saturating_sub(RECENT_S * 1000));
        let (mut w, mut r) = (own.clone(), own_recent.clone());
        for p in i.peers.iter().filter(|p| p.error.is_none()) {
            w.add(&p.window);
            r.add(&p.recent);
        }
        let ratio = w.error_ratio();
        let recent = r.error_ratio();
        let slo = i.slo_error_ratio;
        // Failing now: over the SLO in the last minute (a few failures at
        // least), or still over half of it while the window is over it.
        // Without a search in the last minute, the window says.
        let failing_now = match recent {
            Some(x) => (x > slo && r.failures >= RECENT_FAILURES) || (x > slo / 2.0 && ratio.is_some_and(|y| y > slo)),
            None => ratio.is_some_and(|y| y > slo),
        };
        let shown = if recent.is_some() { recent } else { ratio };
        // The window's last run of failures, from its first failing point
        // in the hour: an outage longer than the window began before it
        // (R2-scenarios-4).
        let hour = Window::of(if i.history.len() > i.points.len() { i.history } else { i.points });
        let run = own.last_run(slo / 2.0).map(|(from, to, worst)| match hour.last_run(slo / 2.0) {
            Some((f, t, w)) if f <= from && t >= to => (f, to, worst.max(w)),
            _ => (from, to, worst),
        });
        let recovered = !failing_now && ratio.is_some_and(|y| y > slo / 2.0) && recent.is_some() && run.is_some();
        let er_status = if failing_now {
            if shown.is_some_and(|x| x > slo) { "critical" } else { "warning" }
        } else if recovered || ratio.is_some_and(|y| y > slo / 2.0) {
            "warning"
        } else {
            "ok"
        };
        let er_since = failing_now.then(|| hour.over_since(slo / 2.0).or_else(|| own.over_since(slo / 2.0))).flatten();
        let mut er = json!({"id": "error_ratio", "status": er_status, "value": ratio, "slo": slo, "window_s": WINDOW_S, "queries": w.queries.round(), "failures": w.failures.round(),
            "recent": {"value": recent, "window_s": RECENT_S, "queries": r.queries.round(), "failures": r.failures.round()}, "coordinators": 1 + i.peers.iter().filter(|p| p.error.is_none()).count()});
        if let Some(s) = er_since {
            er["since_ms"] = json!(s);
        }
        if ratio.is_none() {
            er["note"] = json!("No search ended in the last 5 min");
        }
        if let (true, Some((from, to, worst))) = (recovered, run) {
            er["recovered"] = json!({"from_ms": from, "to_ms": to, "worst": worst});
        }
        checks.push(er);
        if let (true, Some(x)) = (failing_now, shown) {
            let critical = er_status == "critical";
            let over = if recent.is_some() { "the last minute" } else { "the last 5 min" };
            problems.push(Problem {
                rank: if critical { 3 } else { 12 },
                critical,
                headline: format!("Searches are failing: more than the SLO's {} fail", pct(slo)),
                j: json!({"id": "error_ratio", "severity": severity(critical), "title": format!("{} of searches failed over {over} (SLO {})", pct(x), pct(slo)), "where": ["searches"], "since_ms": er_since,
                    "impact": format!("{} of searches fail", pct(x)), "next": {"text": "See which statements fail, and on which shards", "view": "queries", "params": {"outcome": "error"}}}),
            });
        } else if let (true, Some((from, to, worst))) = (recovered, run) {
            // A failure that ended: when, how long, how bad; searches answer again.
            let lasted = dur(to.saturating_sub(from) + 2000);
            let title = format!("Searches failed for {lasted} ({} at worst) and answer again", pct(worst));
            problems.push(Problem {
                rank: 11,
                critical: false,
                headline: title.clone(),
                j: json!({"id": "error_ratio_recovered", "severity": "warning", "title": title, "where": ["searches"], "since_ms": from, "from_ms": from, "to_ms": to, "recovered_ms": to, "worst": worst,
                    "impact": format!("{} of searches fail now (over the last minute)", pct(recent.unwrap_or(0.0))), "next": {"text": "See what failed in that window", "view": "queries", "params": {"tab": "statements", "from": from.saturating_sub(60_000), "to": to + 60_000}}}),
            });
        }
        let p99 = w.p99_ms();
        let p99_now = r.p99_ms();
        let slo_ms = i.slo_p99_ms;
        let judged = if r.queries > 0.0 { p99_now } else { p99 };
        let p99_status = match judged {
            Some(v) if v > slo_ms * 4.0 => "critical",
            Some(v) if v > slo_ms => "warning",
            _ => "ok",
        };
        let slow_was = p99_status == "ok" && r.queries > 0.0 && p99.is_some_and(|v| v > slo_ms);
        checks.push(json!({"id": "p99", "status": if slow_was { "warning" } else { p99_status }, "value_ms": p99, "slo_ms": slo_ms, "window_s": WINDOW_S, "recent": {"value_ms": p99_now, "window_s": RECENT_S}}));
        if let (Some(v), true) = (judged, p99_status != "ok") {
            let critical = p99_status == "critical";
            let over = if r.queries > 0.0 { "the last minute" } else { "the last 5 min" };
            problems.push(Problem {
                rank: if critical { 4 } else { 13 },
                critical,
                headline: format!("Searches are slow: p99 is over the SLO's {slo_ms:.0} ms"),
                j: json!({"id": "p99", "severity": severity(critical), "title": format!("Searches are slow: p99 {v:.0} ms over {over} (SLO {slo_ms:.0} ms)"), "where": ["searches"], "since_ms": null,
                    "impact": format!("1 search in 100 takes over {v:.0} ms"), "next": {"text": "See the slowest statements and where their time goes", "view": "performance", "params": {}}}),
            });
        } else if let (true, Some(v), Some(n)) = (slow_was, p99, p99_now) {
            let title = format!("Searches were slow (p99 {v:.0} ms over the last 5 min) and are fast again: p99 {n:.0} ms over the last minute");
            problems.push(Problem {
                rank: 17,
                critical: false,
                headline: "Searches were slow and are fast again".into(),
                j: json!({"id": "p99_recovered", "severity": "warning", "title": title, "where": ["searches"], "since_ms": null,
                    "impact": format!("p99 {n:.0} ms now (SLO {slo_ms:.0} ms)"), "next": {"text": "See where the time went", "view": "performance", "params": {}}}),
            });
        }

        // ----- copies
        let copies: Vec<(usize, Json)> = shards.iter().enumerate().flat_map(|(k, s)| s["replicas"].as_array().cloned().unwrap_or_default().into_iter().map(move |r| (k, r))).collect();
        let gone: Vec<String> = copies.iter().filter(|(_, r)| down(r)).map(|(_, r)| short(r)).collect();
        let cut: Vec<String> = copies.iter().filter(|(_, r)| down(r) && !reached_by(r).is_empty()).map(|(_, r)| short(r)).collect();
        // Copies another coordinator cannot reach which answer this one,
        // by coordinator: from its surveys begun since this one last found
        // the copy down.
        let peer_cuts: Vec<(&PeerWindow, Vec<(String, u64)>)> = said
            .iter()
            .map(|p| {
                let theirs = p
                    .unreached
                    .iter()
                    .flatten()
                    .filter(|u| m.down_seen.get(u.as_str()).is_none_or(|t| p.surveyed_ms > t + CUT_MARGIN_MS))
                    .filter_map(|u| copies.iter().find(|(_, r)| r["url"].as_str() == Some(u.as_str()) && !down(r)))
                    .map(|(k, r)| (short(r), shards[*k]["shard"].as_u64().unwrap_or(*k as u64)))
                    .collect::<Vec<_>>();
                (*p, theirs)
            })
            .filter(|(_, t)| !t.is_empty())
            .collect();
        let elsewhere: Vec<String> = peer_cuts.iter().flat_map(|(p, t)| t.iter().map(|c| format!("{} from {}", c.0, p.name))).collect();
        let status = if gone.is_empty() && elsewhere.is_empty() { "ok" } else { "warning" };
        checks.push(json!({"id": "copies", "status": status, "down": gone.len(), "total": copies.len(), "where": gone, "unreached_here": cut, "unreached_elsewhere": elsewhere}));

        // ----- copies another coordinator cannot reach which answer this
        // one: that coordinator's clients are served by fewer copies, or
        // fail, and its own Home says why.
        for (p, theirs) in &peer_cuts {
            let names: Vec<String> = theirs.iter().map(|t| t.0.clone()).collect();
            let mut w = vec![p.name.clone()];
            w.extend(names.iter().cloned());
            let title = format!("Coordinator {} cannot reach {}, which this coordinator reaches", p.name, both(&names));
            problems.push(Problem {
                rank: 13,
                critical: false,
                headline: title.clone(),
                j: json!({"id": format!("coordinator-{}-cut-off", p.name), "severity": "warning", "title": title, "where": w, "since_ms": null,
                    "impact": format!("Searches through {} go to the shards' other copies, and fail on a shard none of whose copies it reaches", p.name),
                    "next": {"text": format!("Look at the network between {} and {} (a firewall, a network policy, DNS), or its logs", p.name, both(&names)), "view": "cluster", "params": {"shard": theirs[0].1}}}),
            });
        }

        // ----- lag: what each primary says its replicas lack
        let mut lag: Option<(u64, Option<f64>, String, usize, bool)> = None;
        let mut out_of_sync = Vec::new();
        for (k, s) in shards.iter().enumerate() {
            let rs = s["replicas"].as_array().cloned().unwrap_or_default();
            let Some(p) = rs.iter().find(|r| !down(r) && r["detail"]["health"]["replication"]["role"] == "primary") else { continue };
            let writes_s = p["detail"]["load"]["writes_s"].as_f64().filter(|w| *w > 0.0);
            for f in p["detail"]["health"]["replication"]["replicas"].as_array().cloned().unwrap_or_default() {
                let copy = rs.iter().find(|r| r["url"] == f["url"]);
                if copy.is_some_and(down) {
                    continue;
                }
                let name = copy.map_or_else(|| f["url"].as_str().unwrap_or("?").to_string(), short);
                let behind = f["behind"].as_u64().unwrap_or(0);
                if f["in_sync"] == false && f["member"] != false {
                    out_of_sync.push(name.clone());
                }
                if lag.as_ref().is_none_or(|l| behind > l.0) {
                    lag = Some((behind, writes_s.map(|w| behind as f64 / w), name, s["shard"].as_u64().unwrap_or(k as u64) as usize, f["in_sync"] != false));
                }
            }
        }
        let lag_status = match &lag {
            Some((_, Some(secs), ..)) if *secs >= 30.0 => "warning",
            _ if !out_of_sync.is_empty() => "warning",
            _ => "ok",
        };
        let mut lc = json!({"id": "lag", "status": lag_status, "max_txn": lag.as_ref().map_or(0, |l| l.0), "max_s": lag.as_ref().and_then(|l| l.1).map(|s| (s * 10.0).round() / 10.0), "where": lag.as_ref().map(|l| l.2.clone()), "out_of_sync": out_of_sync.clone()});
        if lag.as_ref().is_some_and(|l| l.1.is_some()) {
            lc["max_s_from"] = json!("transactions behind over the primary's write rate");
        }
        checks.push(lc);
        if let (Some((txn, secs, name, shard, _)), "warning") = (&lag, lag_status) {
            let about = secs.map_or(String::new(), |s| format!(" (about {s:.0} s)"));
            let title = if out_of_sync.is_empty() { format!("{name} is {txn} transactions behind its primary{about}") } else { format!("{} not in sync with {} primary", both(&out_of_sync), if out_of_sync.len() == 1 { "its" } else { "their" }) };
            problems.push(Problem {
                rank: 14,
                critical: false,
                headline: if out_of_sync.is_empty() { format!("{name} is behind its primary") } else { title.clone() },
                j: json!({"id": "lag", "severity": "warning", "title": title, "where": [format!("shard {shard}"), name], "since_ms": null,
                    "impact": format!("Searches {name} serves may miss recent writes; it may not count toward a write majority"), "next": {"text": format!("Look at {name}'s replication"), "view": "cluster", "params": {"shard": shard, "copy": name}}}),
            });
        }

        // ----- data damaged on disk: what each copy's last scrub found
        // (finding chaos-6: bit rot on a replica showed nowhere).
        let damaged: Vec<(String, usize, usize)> = copies
            .iter()
            .filter(|(_, r)| !down(r))
            .filter_map(|(k, r)| {
                let n = r["detail"]["health"]["damaged"].as_array().map_or(0, Vec::len);
                (n > 0).then(|| (short(r), shards[*k]["shard"].as_u64().unwrap_or(*k as u64) as usize, n))
            })
            .collect();
        checks.push(json!({"id": "damaged", "status": if damaged.is_empty() { "ok" } else { "critical" }, "where": damaged.iter().map(|d| &d.0).collect::<Vec<_>>()}));
        for (name, shard, n) in &damaged {
            let title = format!("{name}'s data is damaged on disk: {n} {} {} {}", if *n == 1 { "segment" } else { "segments" }, if *n == 1 { "fails" } else { "fail" }, if *n == 1 { "its checksum" } else { "their checksums" });
            problems.push(Problem {
                rank: 4,
                critical: true,
                headline: title.clone(),
                j: json!({"id": format!("damaged-{name}"), "severity": "critical", "title": title, "where": at_shard(*shard, name), "since_ms": null,
                    "impact": format!("Searches that read the damaged segments fail on {name} (a coordinator tries another copy), and a primary elected from it would serve the damage"),
                    "next": {"text": format!("Replace {name} with a new copy (Cluster: Plan the repair), then remove it; or restore it from a checkpoint"), "view": "cluster", "params": {"shard": shard, "copy": name}}}),
            });
        }

        // ----- disk
        let mut disks: Vec<(f64, String, usize)> = Vec::new();
        for (k, r) in copies.iter().filter(|(_, r)| !down(r)) {
            let d = &r["detail"];
            if let Some(v) = d["load"]["disk_used_ratio"].as_f64().or(d["minute"]["disk_used_ratio"].as_f64()) {
                let name = short(r);
                let seen = m.disk.entry(name.clone()).or_default();
                if seen.back().is_none_or(|&(t, _)| now >= t + 10_000) {
                    seen.push_back((now, v));
                }
                while seen.front().is_some_and(|&(t, _)| t + DISK_MEMORY_MS < now) {
                    seen.pop_front();
                }
                disks.push((v, name, shards[*k]["shard"].as_u64().unwrap_or(*k as u64) as usize));
            }
        }
        let names: std::collections::HashSet<&String> = disks.iter().map(|d| &d.1).collect();
        m.disk.retain(|k, _| names.contains(k));
        let mut dc = json!({"id": "disk", "status": "ok"});
        // The fullest first (to the tenth of a percent shown), then by name:
        // the same order every look, though disks fill by bytes between looks.
        let tenth = |v: f64| (v * 1000.0).round() as i64;
        disks.sort_by(|a, b| tenth(b.0).cmp(&tenth(a.0)).then_with(|| a.1.cmp(&b.1)));
        if let Some((max, name, shard)) = disks.first().cloned() {
            let mut sorted: Vec<f64> = disks.iter().map(|d| d.0).collect();
            sorted.sort_by(f64::total_cmp);
            let median = sorted[sorted.len() / 2];
            // Days until the fullest disk fills, at the rate it grew over the
            // span remembered (ten minutes at least).
            let days = m.disk.get(&name).and_then(|s| {
                let (&(t0, v0), &(t1, v1)) = (s.front()?, s.back()?);
                let rate = (v1 - v0) / (t1.saturating_sub(t0)) as f64;
                (t1 >= t0 + DISK_SPAN_MS && rate > 0.0).then(|| (1.0 - v1) / rate / 86_400_000.0)
            });
            let status = if max >= 0.95 {
                "critical"
            } else if max >= 0.85 || days.is_some_and(|d| d < 3.0) {
                "warning"
            } else {
                "ok"
            };
            let r2 = |v: f64| (v * 1000.0).round() / 1000.0;
            // Every copy over the warning level: one problem for all of them.
            let over: Vec<&(f64, String, usize)> = disks.iter().filter(|d| d.0 >= 0.85).collect();
            dc = json!({"id": "disk", "status": status, "max": r2(max), "median": r2(median), "where": name, "days_to_full": days.map(|d| (d * 10.0).round() / 10.0), "over": over.iter().map(|d| &d.1).collect::<Vec<_>>()});
            if status != "ok" {
                let critical = status == "critical";
                let fills = days.map_or(String::new(), |d| format!(" and fills in about {} at the last hour's rate", fills_in(d)));
                let (title, where_) = if over.len() > 1 {
                    let lo = over.last().map_or(max, |d| d.0);
                    let range = if pct(lo) == pct(max) { pct(max) } else { format!("{} to {}", pct(lo), pct(max)) };
                    let mut shards: Vec<usize> = over.iter().map(|d| d.2).collect();
                    shards.sort_unstable();
                    shards.dedup();
                    let mut w: Vec<String> = shards.iter().map(|s| format!("shard {s}")).collect();
                    w.extend(over.iter().map(|d| d.1.clone()));
                    let fullest = if pct(lo) == pct(max) { if fills.is_empty() { String::new() } else { format!("; {name}{fills}") } } else { format!("; the fullest is {name}{fills}") };
                    (format!("{} copies' disks are {range} full{fullest}", over.len()), w)
                } else {
                    (format!("{name}'s disk is {} full{fills}", pct(max)), at_shard(shard, &name))
                };
                let impact = if over.len() > 1 {
                    "Writes to a shard stop when its primary's disk is full".to_string()
                } else if alone {
                    "Writes stop when the disk is full".to_string()
                } else {
                    format!("Writes to shard {shard} stop when its disk is full")
                };
                // The headline says the line crossed, not the share now
                // and when it fills, which change every look.
                let line = if critical { "95%" } else { "85%" };
                let headline = match over.len() {
                    // The estimate, as the problem's title says it: a
                    // headline of "within 3 days" (the line) read as days
                    // while the disk filled in hours.
                    0 => format!("{name}'s disk fills in about {} at the last hour's rate", days.map_or_else(|| "3 days".to_string(), fills_in)),
                    1 => format!("{name}'s disk is over {line} full"),
                    n => format!("{n} copies' disks are over {} full", if over.iter().all(|d| d.0 >= 0.95) { "95%" } else { "85%" }),
                };
                problems.push(Problem {
                    rank: if critical { 5 } else { 15 },
                    critical,
                    headline,
                    j: json!({"id": "disk", "severity": severity(critical), "title": title, "where": where_, "since_ms": null, "copies": over.len(),
                        "impact": impact, "next": {"text": "See which tables grow, then free space, add disk or add a shard", "view": "tables", "params": {"shard": shard}}}),
                });
            }
        }
        checks.push(dc);

        // ----- placement: a shard whose copies all run on one node, while
        // the cluster has others (LIV-6): a second copy protects it from a
        // lost disk, not from that node going. A cluster on one node is
        // laid out as it can be.
        let node_of = |r: &Json| r["detail"]["place"]["node"].as_str().filter(|n| !n.is_empty()).map(String::from);
        let nodes: std::collections::BTreeSet<String> = copies.iter().filter_map(|(_, r)| node_of(r)).collect();
        let mut together: Vec<(usize, String, Vec<String>)> = Vec::new();
        if nodes.len() > 1 {
            for (k, s) in shards.iter().enumerate() {
                let rs = s["replicas"].as_array().cloned().unwrap_or_default();
                let on: Vec<Option<String>> = rs.iter().map(node_of).collect();
                if rs.len() < 2 || on.iter().any(Option::is_none) {
                    continue;
                }
                let first = on[0].clone().unwrap_or_default();
                if on.iter().all(|n| n.as_deref() == Some(first.as_str())) {
                    together.push((s["shard"].as_u64().unwrap_or(k as u64) as usize, first, rs.iter().map(short).collect()));
                }
            }
        }
        checks.push(json!({"id": "placement", "status": if together.is_empty() { "ok" } else { "warning" }, "nodes": nodes.len(),
            "together": together.iter().map(|(sh, node, cs)| json!({"shard": sh, "node": node, "copies": cs})).collect::<Vec<_>>()}));
        for (shard, node, cs) in &together {
            let title = format!("Every copy of shard {shard} runs on node {node} ({})", both(cs));
            problems.push(Problem {
                rank: 17,
                critical: false,
                headline: format!("Shard {shard}'s copies all run on node {node}"),
                j: json!({"id": format!("placement-{shard}"), "severity": "warning", "title": title, "where": [format!("shard {shard}"), node],
                    "since_ms": null, "node": node, "copies": cs,
                    "impact": format!("Losing node {node} stops shard {shard}'s reads and writes: its other copies guard against a lost disk, not a lost node"),
                    "next": {"text": format!("Restart one of shard {shard}'s replicas so that the scheduler places it on another node (the operator asks for copies on different nodes; a node without room for it keeps it here), or add a node"), "view": "cluster", "params": {"shard": shard}}}),
            });
        }

        // ----- a copy whose disk holds far more than its shard's other
        // copies, which hold the same data (LIV-7: 4 GB against 0.8 GB):
        // files that are not its tables, such as a copy of the database a
        // transfer left when the server was stopped during it.
        let used = |r: &Json| {
            let l = &r["detail"]["load"];
            Some(l["disk_total_bytes"].as_f64()? - l["disk_free_bytes"].as_f64()?).filter(|u| *u > 0.0)
        };
        let mut heavy: Vec<(usize, String, f64, f64)> = Vec::new();
        for (k, s) in shards.iter().enumerate() {
            let rs: Vec<&Json> = s["replicas"].as_array().into_iter().flatten().filter(|r| !down(r)).collect();
            for r in &rs {
                let Some(mine) = used(r) else { continue };
                let peers: Vec<f64> = rs.iter().filter(|o| o["url"] != r["url"]).filter_map(|o| used(o)).collect();
                let Some(most) = peers.iter().copied().reduce(f64::max) else { continue };
                if mine > 2.0 * most && mine - most > HEAVY_DISK_BYTES {
                    heavy.push((s["shard"].as_u64().unwrap_or(k as u64) as usize, short(r), mine, most));
                }
            }
        }
        let gb = |b: f64| format!("{:.1} GB", b / 1e9);
        checks.push(json!({"id": "disk_balance", "status": if heavy.is_empty() { "ok" } else { "warning" },
            "where": heavy.iter().map(|h| json!({"shard": h.0, "copy": h.1, "used_bytes": h.2, "peers_bytes": h.3})).collect::<Vec<_>>()}));
        for (shard, name, mine, most) in &heavy {
            let title = format!("{name} uses {} of its disk, its shard's other copies {} or less, for the same data", gb(*mine), gb(*most));
            problems.push(Problem {
                rank: 18,
                critical: false,
                headline: format!("{name} uses {:.0} times the disk of its shard's other copies", mine / most),
                j: json!({"id": format!("disk-balance-{name}"), "severity": "warning", "title": title, "where": [format!("shard {shard}"), name], "since_ms": null,
                    "impact": format!("{} on {name} is not its share of the data: its disk fills sooner, and a rule that grows every volume by the fullest one grows them all", gb(mine - most)),
                    "next": {"text": format!("Look at what fills {name}'s volume besides the database and its log store; a server of this version removes the copies interrupted transfers left when it starts, so restarting {name} frees those"), "view": "cluster", "params": {"shard": shard, "copy": name}}}),
            });
        }

        // ----- alerts: the rules firing here that nobody silenced, each once
        // (a coordinator's each_copy rule gathers its copies'), as the bell
        // counts them. An info alert (a shard's primary changed) tells, but
        // is no problem of the fleet: it does not turn Home amber.
        let told: Vec<Json> = i.alerts.iter().filter(|a| a["severity"] != "info").cloned().collect();
        let info = i.alerts.len() - told.len();
        let alerts = &told;
        let firing = alerts.len() as u64;
        let critical = alerts.iter().filter(|a| a["severity"] == "critical").count() as u64;
        let al_status = if critical > 0 {
            "critical"
        } else if firing > 0 {
            "warning"
        } else {
            "ok"
        };
        let rules: Vec<Json> = alerts
            .iter()
            .map(|a| {
                let copies: Vec<&str> = a["where"].as_array().map_or_else(Vec::new, |w| w.iter().filter(|c| c["state"].is_null() || c["state"] == "firing").filter_map(|c| c["short"].as_str().or(c["copy"].as_str())).collect());
                json!({"rule": a["rule"], "name": a["name"], "severity": a["severity"], "since_ms": a["since_ms"], "copies": copies.len(), "where": copies})
            })
            .collect();
        checks.push(json!({"id": "alerts", "status": al_status, "firing": firing, "critical": critical, "info": info, "rules": rules}));
        if firing > 0 {
            let on = |a: &Json| match a["copies"].as_u64().unwrap_or(0) {
                0 => String::new(),
                1 => format!(" on {}", a["where"][0].as_str().unwrap_or("1 copy")),
                n => format!(" on {n} copies"),
            };
            let named: Vec<String> = rules.iter().take(2).map(|a| format!("{}{}", a["name"].as_str().unwrap_or("?"), on(a))).collect();
            let more = if rules.len() > 2 { format!(" and {} more", rules.len() - 2) } else { String::new() };
            let title = format!("{firing} {} firing{}: {}{more}", if firing == 1 { "alert is" } else { "alerts are" }, if critical > 0 { format!(", {critical} critical") } else { String::new() }, named.join("; "));
            let since = alerts.iter().filter_map(|a| a["since_ms"].as_u64()).min();
            // NEW-workload-4: each rule with its severity and where, not "see each alert".
            let each = |a: &Json| {
                let at = match a["copies"].as_u64().unwrap_or(0) {
                    0 => String::new(),
                    1 => format!(", {}", a["where"][0].as_str().unwrap_or("1 copy")),
                    n => format!(", {n} copies"),
                };
                format!("{} ({}{at})", a["name"].as_str().unwrap_or("?"), a["severity"].as_str().unwrap_or("warning"))
            };
            let mut impact = rules.iter().take(4).map(each).collect::<Vec<_>>().join(", ");
            if rules.len() > 4 {
                impact.push_str(&format!(" and {} more", rules.len() - 4));
            }
            problems.push(Problem {
                rank: if critical > 0 { 6 } else { 16 },
                critical: critical > 0,
                headline: title.clone(),
                j: json!({"id": "alerts", "severity": severity(critical > 0), "title": title, "where": [], "since_ms": since, "impact": impact, "next": {"text": if firing == 1 { "Open the alert" } else { "Open Alerts" }, "view": if firing == 1 { format!("alert/{}", alerts[0]["rule"].as_str().unwrap_or("")) } else { "alerts".to_string() }, "params": {}}}),
            });
        }

        // ----- a coordinator that stopped answering (R2-workload-3): its
        // clients' queries fail or went elsewhere, and its numbers are not
        // in these.
        if !i.gone.is_empty() && i.coordinator.is_some() {
            let names: Vec<String> = i.gone.iter().map(|(n, _, _)| n.clone()).collect();
            let since = i.gone.iter().map(|(_, _, t)| *t).max();
            let one = names.len() == 1;
            let title = if one { format!("Coordinator {} stopped answering", names[0]) } else { format!("{} coordinators stopped answering", names.len()) };
            let ago = since.map_or(String::new(), |t| format!(" {} ago", dur(now.saturating_sub(t))));
            problems.push(Problem {
                rank: 14,
                critical: false,
                headline: title.clone(),
                j: json!({"id": "coordinators_gone", "severity": "warning", "title": title, "where": names, "since_ms": since,
                    "impact": format!("The shards last heard from {}{ago}. Clients sent to {} fail or went to another coordinator, and {} searches, alerts and audit are not in these numbers", either(&names), if one { "it" } else { "them" }, if one { "its" } else { "their" }),
                    "next": {"text": format!("Look at {}'s logs and its pod or process (killed for memory?); if it was removed on purpose, this clears 10 minutes after it was last heard from", either(&names)), "view": "logs", "params": {"q": names[0].clone()}}}),
            });
        }

        // ----- since when each problem holds, worst first; equal ones by
        // their age, then their title: the same order every look.
        let ids: std::collections::HashSet<String> = problems.iter().map(|p| p.j["id"].as_str().unwrap_or("").to_string()).collect();
        m.problems.retain(|k, _| ids.contains(k));
        for p in &mut problems {
            let id = p.j["id"].as_str().unwrap_or("").to_string();
            let first = *m.problems.entry(id).or_insert(now);
            if p.j["since_ms"].is_null() {
                p.j["since_ms"] = json!(first);
            }
            // The first signal of a problem: when it, or what it stems from, began.
            let since = p.j["since_ms"].as_u64().unwrap_or(now);
            p.j["first_signal_ms"] = json!(since.min(first));
        }
        problems.sort_by(|a, b| {
            (!a.critical, a.rank, a.j["since_ms"].as_u64().unwrap_or(now))
                .cmp(&(!b.critical, b.rank, b.j["since_ms"].as_u64().unwrap_or(now)))
                .then_with(|| a.headline.cmp(&b.headline))
        });

        let status = checks.iter().fold("ok", |a, c| worst(a, c["status"].as_str().unwrap_or("ok")));
        let since = match m.status {
            Some((s, t)) if s == status => t,
            _ => now,
        };
        m.status = Some((status, since));
        m.not_ok = if status == "ok" { None } else { Some(m.not_ok.unwrap_or(now)) };
        let headline = match problems.first() {
            Some(p) => p.headline.clone(),
            None => {
                let copies_up = copies.len() - gone.len();
                let all = if shards.len() == 1 { "Its 1 shard serves reads and takes writes".to_string() } else { format!("All {} shards serve reads and take writes", shards.len()) };
                let mut h = format!("{all}; {copies_up} of {} {} up", copies.len(), if copies.len() == 1 { "copy is" } else { "copies are" });
                if shards.len() == 1 && i.coordinator.is_none() {
                    h = "This server serves reads and takes writes".into();
                }
                // Steady while nothing changes: the numbers are in the checks.
                match (ratio, p99) {
                    (Some(_), Some(_)) => format!("{h}. Searches are within their SLOs"),
                    _ => format!("{h}. No search ended in the last 5 min"),
                }
            }
        };
        // The headline problem's own since and first signal; how long the
        // fleet has not been all right is said apart.
        let since_ms = problems.first().and_then(|p| p.j["since_ms"].as_u64()).unwrap_or(since);
        let first_signal_ms = problems.first().and_then(|p| p.j["first_signal_ms"].as_u64());
        let mut coordinators: Vec<Json> = i
            .coordinator
            .iter()
            .map(|c| {
                json!({"url": c["url"], "name": c["name"], "qps": own.qps(), "p99_ms": own.p99_ms(), "error_ratio": own.error_ratio(), "cpu": own.cpu(), "recent_error_ratio": own_recent.error_ratio(), "this": true})
            })
            .collect();
        for p in &i.peers {
            let mut c = json!({"url": p.url, "name": p.name, "this": false});
            match &p.error {
                Some(e) => c["error"] = json!(e),
                None => {
                    for (k, v) in [("qps", p.window.qps()), ("p99_ms", p.window.p99_ms()), ("error_ratio", p.window.error_ratio()), ("cpu", p.window.cpu()), ("recent_error_ratio", p.recent.error_ratio())] {
                        c[k] = json!(v);
                    }
                }
            }
            coordinators.push(c);
        }
        for (name, url, last) in &i.gone {
            coordinators.push(json!({"url": url, "name": name, "this": false, "gone": true, "last_seen_ms": last, "error": format!("{name}: not heard from since {}", dur(now.saturating_sub(*last)))}));
        }
        json!({
            "at_ms": now, "status": status, "headline": headline, "since_ms": since_ms, "first_signal_ms": first_signal_ms, "not_ok_since_ms": m.not_ok,
            "api": {"ok": true, "server": if i.coordinator.is_some() { "coordinator" } else { "shard" }},
            "checks": checks,
            "problems": problems.into_iter().map(|p| for_agents(p.j)).collect::<Vec<_>>(),
            "coordinators": coordinators,
            "window_s": WINDOW_S,
        })
    }
}

/// A shard server as one shard of itself, for fleet health on a server
/// that is not a coordinator: its state from what it serves (`detail`'s
/// `capability`), its copies itself and the replicas it ships to.
pub fn local_shard(detail: &Json) -> Json {
    let cap = &detail["capability"];
    let rep = &detail["health"]["replication"];
    let role = rep["role"].as_str().unwrap_or("standalone");
    let mut me = json!({"short": "this server", "status": "up", "role": role, "detail": detail});
    if role == "primary" {
        me["takes_writes"] = json!(true);
    }
    let mut copies = vec![me];
    for f in rep["replicas"].as_array().cloned().unwrap_or_default() {
        let gone = f["error"].is_string() && f["in_sync"] != true;
        copies.push(json!({"short": f["url"], "url": f["url"], "status": if gone { "down" } else { "up" }, "role": "replica"}));
    }
    let reads = cap["serves_reads"] != false;
    let writes = cap["takes_writes"] != false || role == "replica";
    let gone: Vec<String> = copies.iter().filter(|r| down(r)).map(short).collect();
    let (state, why) = if !reads {
        ("no_reads", cap["no_reads"].as_str().unwrap_or("this server serves no reads").to_string())
    } else if !writes {
        ("no_writes", cap["no_writes"].as_str().unwrap_or("this server takes no writes").to_string())
    } else if !gone.is_empty() {
        ("degraded", format!("{} of its replicas {} not answer; this server serves reads and takes writes", gone.len(), if gone.len() == 1 { "does" } else { "do" }))
    } else {
        ("ok", "This server serves reads and takes writes".to_string())
    };
    let n = copies.len();
    let up = n - gone.len();
    json!({"shards": [{"shard": 0, "local": true, "state": state, "why": why, "serves_reads": reads, "takes_writes": writes, "majority": up * 2 > n,
        "failover": cap["failover_after_ms"].as_u64().unwrap_or(0) > 0, "primary": null, "replicas": copies}]})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(short: &str, up: bool) -> Json {
        if up {
            json!({"short": short, "url": format!("http://{short}"), "status": "ok", "detail": {"load": {"disk_used_ratio": 0.5}}})
        } else {
            json!({"short": short, "url": format!("http://{short}"), "status": "down", "down_since_ms": 1000})
        }
    }

    fn inputs<'a>(cluster: &'a Json, points: &'a [Arc<Json>]) -> Inputs<'a> {
        Inputs { cluster, points, history: &[], slo_error_ratio: 0.01, slo_p99_ms: 250.0, alerts: Vec::new(), partial: false, coordinator: Some(json!({"url": null, "name": "c"})), peers: Vec::new(), gone: Vec::new(), now_ms: 10_000 }
    }

    /// LIV-6 and LIV-7: both copies of each shard ran on one node, and one
    /// copy used five times its peer's disk; Home said every shard had two
    /// copies, and nothing of the disk.
    #[test]
    fn copies_on_one_node_and_a_copy_with_far_more_disk_are_problems() {
        let at = |short: &str, node: &str, used_gb: f64| json!({"short": short, "url": format!("http://{short}"), "status": "ok", "detail": {"place": {"node": node}, "load": {"disk_used_ratio": used_gb / 5.2, "disk_total_bytes": 5.2e9, "disk_free_bytes": 5.2e9 - used_gb * 1e9}}});
        let c = json!({"shards": [
            {"shard": 0, "state": "ok", "failover": true, "majority": true, "replicas": [at("s0-0", "a", 0.8), at("s0-1", "a", 0.8)]},
            {"shard": 1, "state": "ok", "failover": true, "majority": true, "replicas": [at("s1-0", "b", 4.0), at("s1-1", "c", 0.8)]},
        ]});
        let j = Fleet::default().judge(inputs(&c, &[]));
        let ps = j["problems"].as_array().unwrap();
        let p = ps.iter().find(|p| p["id"] == "placement-0").unwrap_or_else(|| panic!("{j}"));
        assert_eq!(p["title"], "Every copy of shard 0 runs on node a (s0-0 and s0-1)", "{p}");
        assert!(ps.iter().all(|p| p["id"] != "placement-1"), "{j}");
        let d = ps.iter().find(|p| p["id"] == "disk-balance-s1-0").unwrap_or_else(|| panic!("{j}"));
        assert_eq!(d["title"], "s1-0 uses 4.0 GB of its disk, its shard's other copies 0.8 GB or less, for the same data", "{d}");
        assert_eq!(ps.iter().filter(|p| p["id"].as_str().is_some_and(|i| i.starts_with("disk-balance"))).count(), 1);
        // One node for the whole cluster: laid out as it can be.
        let one = json!({"shards": [{"shard": 0, "state": "ok", "failover": true, "majority": true, "replicas": [at("s0-0", "a", 0.8), at("s0-1", "a", 0.9)]}]});
        let j = Fleet::default().judge(inputs(&one, &[]));
        assert!(j["problems"].as_array().unwrap().iter().all(|p| !p["id"].as_str().unwrap().starts_with("placement") && !p["id"].as_str().unwrap().starts_with("disk-balance")), "{j}");
    }

    /// CCW-14: a server that is no coordinator has no shard 0 to name.
    #[test]
    fn one_servers_disk_is_the_servers() {
        let detail = json!({"capability": {}, "health": {"replication": {"role": "standalone"}}, "load": {"disk_used_ratio": 0.96}});
        let c = local_shard(&detail);
        let j = Fleet::default().judge(inputs(&c, &[]));
        let p = j["problems"].as_array().unwrap().iter().find(|p| p["id"] == "disk").unwrap_or_else(|| panic!("{j}")).clone();
        assert_eq!(p["where"], json!(["this server"]), "{p}");
        assert_eq!(p["impact"], "Writes stop when the disk is full", "{p}");
    }

    #[test]
    fn a_coordinator_that_stopped_answering_is_a_problem() {
        let c = json!({"shards": [{"shard": 0, "state": "ok", "failover": true, "majority": true, "replicas": [copy("s0-0", true), copy("s0-1", true)]}]});
        let f = Fleet::default();
        let mut i = inputs(&c, &[]);
        i.gone = vec![("coordinator-b".into(), "http://b:7700".into(), 4_000)];
        let j = f.judge(i);
        let p = j["problems"].as_array().unwrap().iter().find(|p| p["id"] == "coordinators_gone").expect("a problem").clone();
        assert_eq!((p["title"].as_str().unwrap(), p["since_ms"].as_u64()), ("Coordinator coordinator-b stopped answering", Some(4_000)));
        assert!(p["impact"].as_str().unwrap().contains("6 s ago"), "{p}");
        let b = j["coordinators"].as_array().unwrap().iter().find(|c| c["name"] == "coordinator-b").unwrap().clone();
        assert_eq!(b["gone"], true);
    }

    #[test]
    fn a_copy_another_coordinator_reaches_is_cut_off_from_this_one_not_down() {
        let c = json!({"shards": [
            {"shard": 0, "state": "degraded", "failover": true, "majority": true, "why": "1 of 3 copies does not answer", "replicas": [copy("s0-0", true), copy("s0-1", false), copy("s0-2", true)]},
            {"shard": 1, "state": "no_reads", "failover": true, "majority": false, "why": "1 of 2 copies answers", "replicas": [copy("s1-0", true), copy("s1-1", false)]},
        ]});
        let peer = |surveyed_ms: u64, unreached: &[&str]| PeerWindow { url: "http://b".into(), name: "coord-b".into(), unreached: Some(unreached.iter().map(|u| format!("http://{u}")).collect()), surveyed_ms, ..Default::default() };
        // coord-b's survey began after s0-1 and s1-1 stopped answering here (at 1 s), and reached both.
        let mut i = inputs(&c, &[]);
        i.peers = vec![peer(5_000, &["s0-2"])];
        let j = Fleet::default().judge(i);
        let ps = j["problems"].as_array().unwrap();
        let id = |id: &str| ps.iter().find(|p| p["id"] == id).unwrap_or_else(|| panic!("no {id}: {j}")).clone();
        let s1 = id("shard-1-cut-off");
        assert_eq!((s1["severity"].as_str(), s1["title"].as_str()), (Some("critical"), Some("From c, shard 1 has no majority, so it serves no reads; coord-b reaches s1-1")), "its clients fail: {s1}");
        assert_eq!(j["headline"], "Searches through this coordinator are failing: it cannot reach s1-1, which coord-b reaches");
        assert!(s1["next"]["text"].as_str().unwrap().ends_with("meanwhile its clients can use coord-b"), "{s1}");
        let s0 = id("shard-0-cut-off");
        assert_eq!((s0["severity"].as_str(), s0["title"].as_str()), (Some("warning"), Some("c cannot reach s0-1 of shard 0, which coord-b reaches")));
        let b = id("coordinator-coord-b-cut-off");
        assert_eq!((b["title"].as_str(), b["where"].clone()), (Some("Coordinator coord-b cannot reach s0-2, which this coordinator reaches"), json!(["coord-b", "s0-2"])));
        let check = |id: &str| j["checks"].as_array().unwrap().iter().find(|c| c["id"] == id).unwrap().clone();
        assert_eq!((check("shards")["value"]["cut_off"].as_u64(), check("shards")["status"].as_str()), (Some(1), Some("critical")));
        assert_eq!(check("copies")["unreached_here"], json!(["s0-1", "s1-1"]));
        // A survey begun before they stopped answering here says nothing of them: down.
        let mut i = inputs(&c, &[]);
        i.peers = vec![peer(3_500, &[])];
        let j = Fleet::default().judge(i);
        assert_eq!(j["headline"], "Searches are failing: shard 1 has no majority");
        assert!(j["problems"].as_array().unwrap().iter().any(|p| p["id"] == "shard-0-degraded"), "{j}");
        // Down for coord-b too: down.
        let mut i = inputs(&c, &[]);
        i.peers = vec![peer(5_000, &["s0-1", "s1-1"])];
        let j = Fleet::default().judge(i);
        assert_eq!(j["headline"], "Searches are failing: shard 1 has no majority");
    }

    #[test]
    fn a_copy_coming_back_is_not_said_cut_off_on_the_way() {
        let peer = |surveyed_ms: u64, unreached: &[&str]| PeerWindow { url: "http://b".into(), name: "coord-b".into(), unreached: Some(unreached.iter().map(|u| format!("http://{u}")).collect()), surveyed_ms, ..Default::default() };
        let degraded = |at_ms: u64| json!({"at_ms": at_ms, "shards": [{"shard": 0, "state": "degraded", "failover": true, "majority": true, "replicas": [copy("s0-0", true), copy("s0-1", false), copy("s0-2", true)]}]});
        let whole = |at_ms: u64| json!({"at_ms": at_ms, "shards": [{"shard": 0, "state": "ok", "failover": true, "majority": true, "replicas": [copy("s0-0", true), copy("s0-1", true), copy("s0-2", true)]}]});
        let f = Fleet::default();
        let judge = |c: &Json, now_ms: u64, p: PeerWindow| {
            let mut i = inputs(c, &[]);
            (i.now_ms, i.peers) = (now_ms, vec![p]);
            f.judge(i)
        };
        let ids = |j: &Json| j["problems"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        // s0-1 answers coord-b again before this coordinator's next survey: down, as this one last saw it.
        let c = degraded(9_000);
        assert_eq!(ids(&judge(&c, 10_000, peer(9_500, &[]))), ["shard-0-degraded"]);
        // This coordinator's next survey reaches it: all well.
        assert_eq!(judge(&whole(14_000), 15_000, peer(9_500, &[]))["status"], "ok");
        // Cut off for real: a survey begun after coord-b reached it fails again.
        let c = degraded(19_000);
        assert_eq!(ids(&judge(&c, 20_000, peer(19_500, &[]))), ["shard-0-degraded"]);
        assert_eq!(ids(&judge(&degraded(24_000), 25_000, peer(24_500, &[]))), ["shard-0-cut-off"]);
        // Back here, while coord-b's last survey, begun before, did not reach it: said of nobody.
        let j = judge(&whole(29_000), 30_000, peer(26_000, &["s0-1"]));
        assert_eq!((j["status"].as_str(), ids(&j).len()), (Some("ok"), 0), "{j}");
        // coord-b's survey begun after this one found it back still fails: coord-b is cut off from it.
        let j = judge(&whole(34_000), 35_000, peer(34_500, &["s0-1"]));
        assert_eq!((j["status"].as_str(), ids(&j)), (Some("warning"), vec!["coordinator-coord-b-cut-off".to_string()]), "{j}");
        let copies = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "copies").unwrap().clone();
        assert_eq!(copies["unreached_elsewhere"], json!(["s0-1 from coord-b"]));
    }

    #[test]
    fn a_shard_without_majority_is_the_headline() {
        let c = json!({"shards": [
            {"shard": 0, "state": "ok", "failover": true, "majority": true, "replicas": [copy("s0-0", true), copy("s0-1", true)]},
            {"shard": 1, "state": "no_reads", "failover": true, "majority": false, "why": "1 of 2 copies answers: the primary has no majority, so it serves no reads", "replicas": [copy("s1-0", true), copy("s1-1", false)]},
        ]});
        let f = Fleet::default();
        let j = f.judge(inputs(&c, &[]));
        assert_eq!(j["status"], "critical");
        assert_eq!(j["headline"], "Searches are failing: shard 1 has no majority");
        assert_eq!(j["since_ms"], 1000);
        let p = &j["problems"][0];
        assert_eq!(p["where"], json!(["shard 1", "s1-1"]));
        assert_eq!(p["next"]["text"], "Bring s1-1 back, or remove it so s1-0 has a majority");
        assert_eq!(p["next"]["params"]["shard"], 1);
        assert_eq!(p["impact"], "Every search fails: each one needs shard 1");
        let shards = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "shards").unwrap().clone();
        assert_eq!(shards["value"], json!({"down": 0, "no_reads": 1, "no_writes": 0, "degraded": 0, "cut_off": 0, "total": 2}));
        let copies = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "copies").unwrap().clone();
        assert_eq!((copies["down"].as_u64(), copies["total"].as_u64()), (Some(1), Some(4)));
    }

    /// A copy whose scrub found segments failing their checksums is a
    /// critical problem of its own, naming the copy to replace (finding
    /// chaos-6: bit rot on a replica showed nowhere).
    #[test]
    fn a_copy_whose_data_is_damaged_is_to_be_replaced() {
        let mut bad = copy("s0-1", true);
        bad["detail"]["health"] = json!({"damaged": ["segment 7 fails its checksum"]});
        let c = json!({"shards": [{"shard": 0, "state": "ok", "failover": true, "majority": true, "replicas": [copy("s0-0", true), bad, copy("s0-2", true)]}]});
        let j = Fleet::default().judge(inputs(&c, &[]));
        assert_eq!(j["status"], "critical");
        assert_eq!(j["headline"], "s0-1's data is damaged on disk: 1 segment fails its checksum");
        let p = &j["problems"][0];
        assert_eq!((p["id"].as_str(), p["severity"].as_str()), (Some("damaged-s0-1"), Some("critical")));
        assert!(p["next"]["text"].as_str().unwrap().starts_with("Replace s0-1 with a new copy"), "{p}");
        let ck = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "damaged").unwrap().clone();
        assert_eq!(ck["where"], json!(["s0-1"]));
    }

    /// 1 of 2 copies answering is no majority: no election can choose a
    /// primary, so the advice is not to wait for one.
    /// A copy that answers but refuses the cluster's token is named so,
    /// not as one to bring back.
    #[test]
    fn a_copy_refusing_the_cluster_token_says_so() {
        let mut bad = copy("s0-0", false);
        bad["refuses_token"] = json!(true);
        bad["error"] = json!("missing or wrong token");
        let c = json!({"shards": [
            {"shard": 0, "state": "degraded", "failover": true, "majority": true, "primary": "s0-1", "why": "", "replicas": [bad, copy("s0-1", true), copy("s0-2", true)]},
        ]});
        let j = Fleet::default().judge(inputs(&c, &[]));
        let p = &j["problems"][0];
        assert_eq!(p["title"], "Shard 0's s0-0 is up but refuses the cluster's token");
        assert!(p["next"]["text"].as_str().unwrap().starts_with("Start s0-0 again with the cluster's --cluster-token"), "{p}");
        assert!(j["headline"].as_str().unwrap_or_default().contains("refuses the cluster's token") || p["severity"] == "warning", "{j}");
    }

    #[test]
    fn a_shard_of_two_with_one_down_cannot_elect() {
        let c = json!({"shards": [
            {"shard": 0, "state": "no_writes", "failover": true, "majority": false, "primary": null, "why": "1 of 2 copies answers: no copy takes writes", "replicas": [copy("s0-0", false), copy("s0-1", true)]},
        ]});
        let j = Fleet::default().judge(inputs(&c, &[]));
        let p = &j["problems"][0];
        assert_eq!(p["title"], "Shard 0 cannot elect a primary or take writes until a second copy answers");
        let next = p["next"]["text"].as_str().unwrap();
        assert!(next.starts_with("Bring s0-0 back, or, if it is gone for good, make s0-1 the primary by hand"), "{next}");
        assert!(!next.contains("within seconds"), "{next}");
        // With a majority, an election is under way.
        let c = json!({"shards": [
            {"shard": 0, "state": "no_writes", "failover": true, "majority": true, "primary": null, "why": "", "replicas": [copy("s0-0", false), copy("s0-1", true), copy("s0-2", true)]},
        ]});
        let j = Fleet::default().judge(inputs(&c, &[]));
        let next = j["problems"][0]["next"]["text"].as_str().unwrap();
        assert!(next.contains("within seconds"), "{next}");
        assert!(next.contains("Make it the primary") && next.ends_with("then replace s0-0"), "{next}");
        assert_eq!(j["headline"], "Writes are failing: shard 0 has no primary");
    }

    /// A primary that cannot write until it restarts (a full disk) is fixed
    /// by handing it over to a replica in sync, not by a restart, which
    /// fails again while its disk is full (finding chaos-4).
    #[test]
    fn a_primary_that_cannot_write_is_handed_over() {
        let mut p = copy("s0-0", true);
        p["detail"] = json!({"load": {"disk_used_ratio": 1.0}, "health": {"status": "failing", "replication": {"replicas": [
            {"url": "http://s0-1", "in_sync": false}, {"url": "http://s0-2", "in_sync": true}]}}});
        let c = json!({"shards": [{"shard": 0, "state": "no_writes", "failover": true, "majority": true, "primary": "http://s0-0",
            "why": "3 of 3 copies answer: the primary takes no writes: a write failed partway", "replicas": [p, copy("s0-1", true), copy("s0-2", true)]}]});
        let j = Fleet::default().judge(inputs(&c, &[]));
        let next = j["problems"][0]["next"]["text"].as_str().unwrap();
        assert!(next.starts_with("Make s0-2 the primary"), "{next}");
        assert!(next.contains("free s0-0's volume (100% full) and restart it") && !next.contains("restart the server"), "{next}");
        // No replica in sync: fix it in place.
        let mut c = c;
        c["shards"][0]["replicas"][0]["detail"]["health"]["replication"]["replicas"][1]["in_sync"] = json!(false);
        let j = Fleet::default().judge(inputs(&c, &[]));
        assert_eq!(j["problems"][0]["next"]["text"], "Free s0-0's volume (100% full) and restart it: no replica is in sync to take over");
    }

    #[test]
    fn the_error_ratio_counts_server_failures_over_the_window() {
        let c = json!({"shards": [{"shard": 0, "state": "ok", "replicas": [copy("s0-0", true)]}]});
        let pts: Vec<Arc<Json>> = (0..10u64).map(|k| Arc::new(json!({"t": k * 2000, "secs": 2.0, "queries": 100, "query_errors": if k >= 5 { 5 } else { 0 }, "latency_hist": [0, 0, 0, 0, 0, 100]}))).collect();
        let j = Fleet::default().judge(inputs(&c, &pts));
        let er = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "error_ratio").unwrap().clone();
        assert_eq!(er["value"], 0.025);
        assert_eq!(er["status"], "critical");
        assert_eq!(er["since_ms"], 10_000, "since the first point of the run over half the SLO");
        // The headline holds while the share moves; the problem says it.
        assert_eq!(j["headline"], "Searches are failing: more than the SLO's 1% fail");
        assert_eq!(j["problems"][0]["title"], "2.5% of searches failed over the last minute (SLO 1%)");
        assert!(j["coordinators"][0]["qps"].as_f64().unwrap() == 50.0);
        let p99 = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "p99").unwrap().clone();
        assert!(p99["value_ms"].as_f64().unwrap() <= 50.0 && p99["status"] == "ok", "{p99}");
    }

    fn pts(from_s: u64, to_s: u64, fails: impl Fn(u64) -> u64) -> Vec<Arc<Json>> {
        (from_s / 2..to_s / 2).map(|k| Arc::new(json!({"t": k * 2000, "secs": 2.0, "queries": 100, "query_errors": fails(k * 2000), "latency_hist": [0, 0, 0, 0, 0, 100]}))).collect()
    }

    #[test]
    fn a_failure_that_ended_says_when_and_for_how_long() {
        let c = json!({"shards": [{"shard": 0, "state": "ok", "replicas": [copy("s0-0", true)]}]});
        // Ten failures in a hundred from 100 s to 160 s; none in the last 4 min.
        let p = pts(0, 300, |t| if (100_000..=160_000).contains(&t) { 10 } else { 0 });
        let mut i = inputs(&c, &p);
        i.now_ms = 300_000;
        let j = Fleet::default().judge(i);
        assert_eq!(j["status"], "warning");
        assert_eq!(j["headline"], "Searches failed for 62 s (10% at worst) and answer again");
        let er = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "error_ratio").unwrap().clone();
        assert_eq!(er["recent"]["value"], 0.0);
        assert_eq!(er["recovered"]["from_ms"], 100_000);
        assert_eq!(er["recovered"]["to_ms"], 160_000);
        assert!(er["since_ms"].is_null(), "not failing now: no since");
        let pr = &j["problems"][0];
        assert_eq!(pr["id"], "error_ratio_recovered");
        assert_eq!(pr["since_ms"], 100_000);
        assert_eq!(j["since_ms"], 100_000, "the headline problem's since");
        // Failing again in the last minute: failing now, over the last minute.
        let p = pts(0, 300, |t| if t >= 260_000 { 5 } else { 0 });
        let mut i = inputs(&c, &p);
        i.now_ms = 300_000;
        let j = Fleet::default().judge(i);
        assert_eq!(j["status"], "critical");
        assert_eq!(j["problems"][0]["title"], "3.3% of searches failed over the last minute (SLO 1%)");
        assert_eq!(j["problems"][0]["since_ms"], 260_000);
        // Two failures in the last minute are not an outage.
        let p = pts(0, 300, |t| if t == 298_000 { 2 } else { 0 });
        let mut i = inputs(&c, &p);
        i.now_ms = 300_000;
        assert_eq!(Fleet::default().judge(i)["status"], "ok");
    }

    /// An outage longer than the window: its start and length come from
    /// its first failing point in the hour, not from the window's first
    /// (R2-scenarios-4: 6 min read "failed for 4 min" a minute after).
    #[test]
    fn a_failure_longer_than_the_window_says_when_it_began() {
        let c = json!({"shards": [{"shard": 0, "state": "ok", "replicas": [copy("s0-0", true)]}]});
        // Failing from 60 s to 436 s (6 min 16 s); the window holds 200 s to 500 s.
        let fails = |t: u64| if (60_000..=436_000).contains(&t) { 100 } else { 0 };
        let hour = pts(0, 500, fails);
        let window = pts(200, 500, fails);
        let mut i = inputs(&c, &window);
        i.history = &hour;
        i.now_ms = 500_000;
        let j = Fleet::default().judge(i);
        assert_eq!(j["headline"], "Searches failed for 6 min (100% at worst) and answer again");
        let er = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "error_ratio").unwrap().clone();
        assert_eq!((er["recovered"]["from_ms"].as_u64(), er["recovered"]["to_ms"].as_u64()), (Some(60_000), Some(436_000)));
        assert_eq!(j["problems"][0]["since_ms"], 60_000);
    }

    #[test]
    fn every_coordinators_searches_count() {
        let c = json!({"shards": [{"shard": 0, "state": "ok", "replicas": [copy("s0-0", true)]}]});
        let p = pts(0, 20, |_| 0);
        let theirs = Window { queries: 1000.0, failures: 50.0, hist: vec![0, 0, 0, 0, 0, 1000], secs: 20.0, cpu_s: 2.0, ratios: Vec::new() };
        let mut i = inputs(&c, &p);
        i.peers = vec![
            PeerWindow { url: "http://b".into(), name: "coord-b".into(), window: theirs.clone(), recent: theirs, error: None, ..Default::default() },
            PeerWindow { url: "http://c".into(), name: "coord-c".into(), error: Some("coord-c: cannot connect".into()), ..Default::default() },
        ];
        let j = Fleet::default().judge(i);
        assert_eq!(j["problems"][0]["title"], "2.5% of searches failed over the last minute (SLO 1%)");
        let er = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "error_ratio").unwrap().clone();
        assert_eq!((er["queries"].as_f64(), er["failures"].as_f64(), er["coordinators"].as_u64()), (Some(2000.0), Some(50.0), Some(2)));
        let cs = j["coordinators"].as_array().unwrap();
        assert_eq!(cs.len(), 3);
        assert_eq!((cs[0]["this"].as_bool(), cs[0]["error_ratio"].as_f64()), (Some(true), Some(0.0)));
        assert_eq!((cs[1]["name"].as_str(), cs[1]["error_ratio"].as_f64(), cs[1]["qps"].as_f64()), (Some("coord-b"), Some(0.05), Some(50.0)));
        assert_eq!(cs[2]["error"], "coord-c: cannot connect");
    }

    #[test]
    fn full_disks_are_one_problem_in_the_same_order_every_look() {
        let disk = |short: &str, v: f64| json!({"short": short, "url": format!("http://{short}"), "status": "ok", "detail": {"load": {"disk_used_ratio": v}}});
        let c = json!({"shards": [
            {"shard": 0, "state": "ok", "replicas": [disk("s0-0", 0.88), disk("s0-1", 0.5)]},
            {"shard": 1, "state": "ok", "replicas": [disk("s1-0", 0.9), disk("s1-1", 0.9)]},
        ]});
        let f = Fleet::default();
        let j = f.judge(inputs(&c, &[]));
        assert_eq!(j["status"], "warning");
        assert_eq!(j["headline"], "3 copies' disks are over 85% full");
        let pr = &j["problems"][0];
        assert_eq!(pr["title"], "3 copies' disks are 88% to 90% full; the fullest is s1-0");
        assert_eq!((pr["id"].as_str(), pr["copies"].as_u64()), (Some("disk"), Some(3)));
        assert_eq!(pr["where"], json!(["shard 0", "shard 1", "s1-0", "s1-1", "s0-0"]));
        let dc = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "disk").unwrap().clone();
        assert_eq!(dc["over"], json!(["s1-0", "s1-1", "s0-0"]));
        for _ in 0..3 {
            assert_eq!(f.judge(inputs(&c, &[]))["headline"], j["headline"]);
        }
        // Disks as full to the tenth of a percent: by name, whatever bytes moved between looks.
        for (a, b) in [(0.92001, 0.91999), (0.91999, 0.92001)] {
            let c = json!({"shards": [{"shard": 0, "state": "ok", "replicas": [disk("s0-1", a), disk("s0-0", b)]}]});
            let j = Fleet::default().judge(inputs(&c, &[]));
            assert_eq!(j["problems"][0]["title"], "2 copies' disks are 92% full");
            let dc = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "disk").unwrap().clone();
            assert_eq!((dc["where"].as_str(), dc["over"].clone()), (Some("s0-0"), json!(["s0-0", "s0-1"])));
        }
    }

    /// A disk under the line that fills within 3 days: the headline says
    /// the estimate, in the problem's unit (it said "within 3 days" while
    /// the problem said about 1 h).
    #[test]
    fn a_filling_disks_headline_says_when_it_fills() {
        let disk = |v: f64| json!({"shards": [{"shard": 0, "state": "ok", "replicas": [{"short": "s1-0", "url": "http://s1-0", "status": "ok", "detail": {"load": {"disk_used_ratio": v}}}]}]});
        let f = Fleet::default();
        let c = disk(0.70);
        f.judge(inputs(&c, &[]));
        let c = disk(0.74);
        let mut i = inputs(&c, &[]);
        i.now_ms = 10_000 + 600_000;
        let j = f.judge(i);
        let pr = j["problems"].as_array().unwrap().iter().find(|p| p["id"] == "disk").expect("a disk problem").clone();
        assert_eq!(pr["title"], "s1-0's disk is 74% full and fills in about 1 h at the last hour's rate");
        assert_eq!(j["headline"], "s1-0's disk fills in about 1 h at the last hour's rate");
    }

    #[test]
    fn firing_alerts_count_once_each_with_their_copies() {
        let c = json!({"shards": [{"shard": 0, "state": "ok", "replicas": [copy("s0-0", true)]}]});
        let mut i = inputs(&c, &[]);
        i.alerts = vec![
            json!({"rule": "disk", "name": "Disk filling", "severity": "critical", "state": "firing", "since_ms": 4000, "where": [{"short": "s0-0"}, {"short": "s1-0", "state": "firing"}, {"short": "s1-1", "state": "ok"}]}),
            json!({"rule": "p99", "name": "Slow searches", "severity": "warning", "state": "firing", "since_ms": 6000, "where": []}),
        ];
        let j = Fleet::default().judge(i);
        let al = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "alerts").unwrap().clone();
        assert_eq!((al["firing"].as_u64(), al["critical"].as_u64(), al["status"].as_str()), (Some(2), Some(1), Some("critical")));
        assert_eq!(al["rules"][0]["copies"], 2);
        let pr = j["problems"].as_array().unwrap().iter().find(|p| p["id"] == "alerts").unwrap().clone();
        assert_eq!(pr["title"], "2 alerts are firing, 1 critical: Disk filling on 2 copies; Slow searches");
        assert_eq!(pr["next"]["text"], "Open Alerts");
        assert_eq!(pr["since_ms"], 4000);
        let mut i = inputs(&c, &[]);
        i.alerts = vec![json!({"rule": "p99", "name": "Slow searches", "severity": "warning", "state": "firing", "since_ms": 6000, "where": [{"short": "s0-0"}]})];
        let pr = Fleet::default().judge(i)["problems"][0].clone();
        assert_eq!(pr["title"], "1 alert is firing: Slow searches on s0-0");
        assert_eq!((pr["next"]["text"].as_str(), pr["next"]["view"].as_str()), (Some("Open the alert"), Some("alert/p99")));
    }

    /// An info alert (a shard's primary changed) is told, but Home stays
    /// green: nothing is wrong.
    #[test]
    fn an_info_alert_does_not_turn_home_amber() {
        let c = json!({"shards": [{"shard": 0, "state": "ok", "replicas": [copy("s0-0", true), copy("s0-1", true)]}]});
        let mut i = inputs(&c, &[]);
        i.alerts = vec![json!({"rule": "primary-changed", "name": "A shard's primary changed", "severity": "info", "state": "firing", "since_ms": 6000, "where": []})];
        let j = Fleet::default().judge(i);
        assert!(j["problems"].as_array().unwrap().iter().all(|p| p["id"] != "alerts"), "{j}");
        let al = j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "alerts").unwrap().clone();
        assert_eq!((al["status"].as_str(), al["firing"].as_u64(), al["info"].as_u64()), (Some("ok"), Some(0), Some(1)), "{al}");
    }

    #[test]
    fn a_span_and_a_fill_time_read_as_words() {
        assert_eq!(dur(45_000), "45 s");
        assert_eq!(dur(6 * 60_000), "6 min");
        assert_eq!(dur(125 * 60_000), "2 h 5 min");
        assert_eq!(dur(120 * 60_000), "2 h");
        assert_eq!(fills_in(5.0 / 24.0), "5 h");
        assert_eq!(fills_in(20.0 / 1440.0), "20 min");
        assert_eq!(fills_in(2.5), "2.5 days");
        assert_eq!(fills_in(14.2), "14 days");
    }

    #[test]
    fn all_well_says_so_and_holds_since_the_status_began() {
        let c = json!({"shards": [{"shard": 0, "state": "ok", "replicas": [copy("s0-0", true), copy("s0-1", true)]}]});
        let f = Fleet::default();
        let j = f.judge(inputs(&c, &[]));
        assert_eq!(j["status"], "ok");
        assert_eq!(j["headline"], "Its 1 shard serves reads and takes writes; 2 of 2 copies are up. No search ended in the last 5 min");
        let mut later = inputs(&c, &[]);
        later.now_ms = 20_000;
        assert_eq!(f.judge(later)["since_ms"], 10_000);
        assert!(j["problems"].as_array().unwrap().is_empty());
    }
}
