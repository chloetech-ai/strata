//! The index inspector (`GET /v1/index`, the command center's Index
//! view): a table's statistics ([`chloedb::inspect`]), how fresh it is,
//! and how often its documents are retrieved and its keys asked for
//! ([`crate::reads`]), for this server, or on a coordinator for each
//! replica of each shard, each shard, and the whole cluster.
//!
//! A coordinator asks every replica for its own ([`node`]) over gRPC, as a
//! profile of kind `index` ([`crate::profiling::Kind::Index`]; no protocol
//! change: a server older than this one answers that it has no such
//! profile, and its replica shows that error). A shard's data is its
//! primary's (the first replica that answered, without one); its reads are
//! its replicas' added up: what each answered its own clients and what it
//! answered through this coordinator. The cluster's data is its shards'
//! added up, with the bytes of every copy beside them.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value as Json};
use chloedb::inspect::{Options, TableStats, MAX_SAMPLE};

use crate::reads::{At, TableReads};
use crate::service::{Backend, Service};
use crate::{telemetry, ApiError};

/// What the inspector is asked for.
#[derive(Clone, Debug)]
pub struct Asked {
    /// The table (the first without one).
    pub table: Option<String>,
    /// Documents each server samples.
    pub sample: usize,
    pub seed: u64,
    /// Sketch the distinct terms.
    pub terms: bool,
    /// The most read keys to list.
    pub top: usize,
}

impl Asked {
    /// From a JSON object of the route's parameters (strings or numbers):
    /// `table`, `sample` (2000), `seed`, `terms` (true), `top` (25).
    pub fn of(q: &Json) -> Result<Asked, ApiError> {
        let num = |k: &str, d: u64| -> Result<u64, ApiError> {
            match q.get(k) {
                None | Some(Json::Null) => Ok(d),
                Some(Json::String(s)) if s.is_empty() => Ok(d),
                Some(Json::String(s)) => s.parse().map_err(|_| ApiError::bad(format!("{k} is a whole number, not {s:?}"))),
                Some(v) => v.as_u64().ok_or_else(|| ApiError::bad(format!("{k} is a whole number, not {v}"))),
            }
        };
        let sample = num("sample", chloedb::inspect::SAMPLE as u64)?;
        if sample > MAX_SAMPLE as u64 {
            return Err(ApiError::bad(format!("sample is at most {MAX_SAMPLE} documents")));
        }
        let terms = match q.get("terms") {
            None | Some(Json::Null) => true,
            Some(Json::Bool(b)) => *b,
            Some(Json::String(s)) => !matches!(s.as_str(), "false" | "0" | "off" | "no"),
            Some(v) => return Err(ApiError::bad(format!("terms is true or false, not {v}"))),
        };
        Ok(Asked {
            table: q.get("table").and_then(|t| t.as_str()).filter(|t| !t.is_empty()).map(str::to_string),
            sample: sample as usize,
            seed: num("seed", Options::default().seed)?,
            terms,
            top: num("top", 25)?.min(crate::reads::TOP as u64) as usize,
        })
    }

    /// As a profile request's body.
    pub fn body(&self) -> Json {
        json!({"table": self.table, "sample": self.sample, "seed": self.seed, "terms": self.terms})
    }
}

/// This server's statistics of the table, its freshness and the reads it
/// answered its own clients, with the distinct-term sketches and the read
/// summaries whole, for a coordinator to merge.
pub async fn node(svc: &Arc<Service>, a: &Asked) -> Result<Json, ApiError> {
    let Backend::Local(db) = &svc.backend else {
        return Err(ApiError::bad("a coordinator holds no table itself; its answer covers its shards"));
    };
    let schema = match &a.table {
        Some(t) => svc.table(t).await?,
        None => svc.default_table()?,
    };
    let name = schema.table.clone();
    let opts = Options { sample: a.sample, seed: a.seed, terms: a.terms };
    let (db2, table) = (db.clone(), name.clone());
    let stats = tokio::task::spawn_blocking(move || {
        let snap = db2.snapshot();
        let view = snap.table(&table).ok_or_else(|| ApiError::bad(format!("no such table: {table}")))?;
        chloedb::inspect::table(&view, &opts).map_err(ApiError::from)
    })
    .await
    .map_err(|e| ApiError::internal(format!("the statistics failed: {e}")))??;
    let activity = db.storage().activity(&name).unwrap_or_default();
    let mut reads = TableReads::default();
    for (at, r) in svc.reads.of(&name) {
        if at == At::here() {
            reads.merge(&r);
        }
    }
    let started = svc.started.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    Ok(json!({
        "table": name,
        "stats": stats,
        "freshness": {
            "last_write_ms": (activity.last_ms > 0).then_some(activity.last_ms),
            "writes": activity.ops,
            "commits": activity.commits,
            "modified_ms": db.storage().modified_ms(),
            "started_ms": started,
        },
        "reads": reads,
        "reads_on": svc.reads.on(),
        "role": svc.role().as_str_name().to_lowercase(),
        "at_ms": telemetry::now_ms(),
    }))
}

/// One server's part of an answer.
struct Part {
    stats: Option<TableStats>,
    reads: TableReads,
    fresh: Fresh,
    /// Bytes of every copy.
    copies: u64,
}

/// When a table last changed.
#[derive(Clone, Copy, Default)]
struct Fresh {
    last_write_ms: Option<u64>,
    modified_ms: Option<u64>,
    writes: u64,
    started_ms: Option<u64>,
}

impl Fresh {
    fn of(j: &Json) -> Fresh {
        let f = &j["freshness"];
        Fresh { last_write_ms: f["last_write_ms"].as_u64(), modified_ms: f["modified_ms"].as_u64(), writes: f["writes"].as_u64().unwrap_or(0), started_ms: f["started_ms"].as_u64() }
    }

    /// The later of both; the writes `writes` says (a shard's are its
    /// primary's, the cluster's its shards' summed).
    fn latest(self, o: Fresh, writes: u64) -> Fresh {
        Fresh {
            last_write_ms: self.last_write_ms.max(o.last_write_ms),
            modified_ms: self.modified_ms.max(o.modified_ms),
            writes,
            started_ms: match (self.started_ms, o.started_ms) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        }
    }

    fn json(&self) -> Json {
        json!({"last_write_ms": self.last_write_ms, "modified_ms": self.modified_ms, "writes": self.writes, "started_ms": self.started_ms})
    }
}

impl Part {
    /// A server's answer ([`node`]), with the reads `extra` it served
    /// through this coordinator.
    fn of(j: &Json, extra: Option<&TableReads>) -> Result<Part, ApiError> {
        let stats: TableStats = serde_json::from_value(j["stats"].clone()).map_err(|e| ApiError::internal(format!("statistics that do not read: {e}")))?;
        let mut reads: TableReads = serde_json::from_value(j["reads"].clone()).unwrap_or_default();
        if let Some(x) = extra {
            reads.merge(x);
        }
        let copies = stats.bytes.total;
        Ok(Part { stats: Some(stats), reads, fresh: Fresh::of(j), copies })
    }

    /// For people: the statistics finished, the reads summarised.
    fn json(&self, top: usize) -> Json {
        let mut stats = self.stats.clone();
        if let Some(s) = &mut stats {
            s.finish(false);
        }
        let live = stats.as_ref().map(|s| s.docs.live);
        json!({
            "stats": stats,
            "freshness": self.fresh.json(),
            "reads": {"docs": self.reads.docs.summary(live, top), "keys": self.reads.keys.summary(live, top)},
            "copies_bytes": self.copies,
        })
    }
}

// ----- core: every table, cheaply (`GET /v1/index/tables`)

/// How long [`tables`] keeps its answer.
pub const TABLES_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// The window of [`tables`]' writes and reads a minute: 5 minutes.
pub const RATE_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

/// A table's writes and reads counted at a look of [`tables`]: of the
/// cluster, and of each shard by its index.
#[derive(Clone, Debug, Default)]
pub struct Look {
    pub writes: u64,
    pub reads: u64,
    pub shards: HashMap<u64, (u64, u64)>,
}

impl Look {
    /// The counts of an answer of [`answer`]: its cluster's and each
    /// shard's (a shard's writes are its primary's, its reads every copy's).
    fn of(j: &Json) -> Look {
        let count = |scope: &Json| (scope["freshness"]["writes"].as_u64().unwrap_or(0), scope["reads"]["docs"]["total"].as_u64().unwrap_or(0));
        let (writes, reads) = count(&j["cluster"]);
        let shards = j["shards"].as_array().into_iter().flatten().filter_map(|s| Some((s["shard"].as_u64()?, count(&s["scope"])))).collect();
        Look { writes, reads, shards }
    }
}

/// Each table's counts at a look of [`tables`], by table, and when.
pub type Counts = std::collections::VecDeque<(u64, HashMap<String, Look>)>;

/// How often a server looks at its tables' counts by itself
/// ([`keep_looking`]), so that the first look of a person has rates.
pub const LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// The earlier look rates are measured against: the earliest within
/// [`RATE_WINDOW`] at least 30 s before `now` (older looks are dropped).
fn earlier(svc: &Service, now: u64) -> Option<(u64, HashMap<String, Look>)> {
    let mut counts = svc.tables_counts.lock().unwrap_or_else(|e| e.into_inner());
    counts.retain(|(at, _)| now.saturating_sub(*at) <= RATE_WINDOW.as_millis() as u64);
    counts.iter().find(|(at, _)| now.saturating_sub(*at) >= 30_000).cloned()
}

/// A table's writes and reads a minute, of the cluster and of each shard:
/// its counts `now_look` against the earlier look's, or, servers started
/// within the window, since they started (`started_ms`). None until a
/// window of 30 s at least is held; a count that went down (a server
/// restarted) gives none. -> {window_s, from_ms, writes_per_min,
/// reads_per_min, shards: {index: {writes_per_min, reads_per_min}}}
fn rates(then: Option<&(u64, HashMap<String, Look>)>, table: &str, cur: &Look, started_ms: u64, now: u64) -> Json {
    let since: Option<(u64, Option<&Look>)> = match then {
        Some((at, c)) => c.get(table).map(|l| (*at, Some(l))),
        None if started_ms > 0 && now.saturating_sub(started_ms) <= RATE_WINDOW.as_millis() as u64 => Some((started_ms, None)),
        None => None,
    };
    let window_ms = since.map(|(at, _)| now.saturating_sub(at)).filter(|ms| *ms >= 30_000);
    let per_min = |n: u64, before: Option<u64>| {
        let (Some(ms), Some(before)) = (window_ms, before) else { return None };
        n.checked_sub(before).map(|d| (d as f64 / (ms as f64 / 60_000.0) * 10.0).round() / 10.0)
    };
    // From a look, its count; from the servers' start, nothing before.
    let before = |f: &dyn Fn(&Look) -> Option<u64>| since.and_then(|(_, l)| l.map_or(Some(0), f));
    let shards: serde_json::Map<String, Json> = cur
        .shards
        .iter()
        .map(|(i, (w, r))| {
            let b = |k: usize| before(&|l: &Look| l.shards.get(i).map(|x| if k == 0 { x.0 } else { x.1 }));
            (i.to_string(), json!({"writes_per_min": per_min(*w, b(0)), "reads_per_min": per_min(*r, b(1))}))
        })
        .collect();
    json!({
        "window_s": window_ms.map(|ms| ms / 1000),
        "from_ms": window_ms.and(since.map(|(at, _)| at)),
        "writes_per_min": per_min(cur.writes, before(&|l: &Look| Some(l.writes))),
        "reads_per_min": per_min(cur.reads, before(&|l: &Look| Some(l.reads))),
        "shards": shards,
    })
}

/// Every table's size and traffic: of this server, or on a coordinator of
/// each shard (its primary's data) and the cluster, with how skewed the
/// shards are, how far each copy is behind, and how much each table grew
/// over a day and a week (the log store's hourly `tables` rows). No
/// document is sampled; each table is one call to each copy. Kept
/// [`TABLES_EVERY`], unless a table was made or dropped since (ONB-11: a
/// table made after the server started is not missing for 30 s).
pub async fn tables(svc: &Arc<Service>) -> Result<Json, ApiError> {
    let mut kept = svc.tables_kept.lock().await;
    if let Some((at, j)) = kept.as_ref() {
        let catalog = svc.catalog();
        let known: std::collections::BTreeSet<&str> = catalog.iter().filter(|t| !t.is_placeholder()).map(|t| t.table.as_str()).collect();
        let had: std::collections::BTreeSet<&str> = j["tables"].as_array().into_iter().flatten().filter_map(|t| t["table"].as_str()).collect();
        if at.elapsed() < TABLES_EVERY && known == had {
            return Ok(j.clone());
        }
    }
    let now = telemetry::now_ms();
    let names: Vec<String> = svc.tables_listed().await.iter().map(|s| s.table.clone()).collect();
    // On a coordinator, each copy as the cluster's snapshot knows it: its
    // short name, and since when it does not answer.
    let mut copies: HashMap<String, (Json, Json, bool)> = HashMap::new();
    if let Backend::Cluster(c) = &svc.backend {
        let snap = c.snapshot().await;
        for r in snap["shards"].as_array().into_iter().flatten().flat_map(|s| s["replicas"].as_array().into_iter().flatten()) {
            if let Some(u) = r["url"].as_str() {
                copies.insert(u.to_string(), (r["short"].clone(), r["down_since_ms"].clone(), r["status"] == "down"));
            }
        }
    }
    // Writes and reads a minute over the last minutes: each table's counts
    // now against those of the earliest look within [`RATE_WINDOW`], at
    // least 30 s ago.
    let then = earlier(svc, now);
    let mut now_counts = HashMap::new();
    let mut out = Vec::new();
    for t in names {
        let a = Asked { table: Some(t.clone()), sample: 0, seed: Options::default().seed, terms: false, top: 0 };
        let j = match answer(svc, a).await {
            Ok(j) => j,
            Err(e) => {
                out.push(json!({"table": t, "error": e.message}));
                continue;
            }
        };
        let of = |scope: &Json| (scope["stats"]["docs"]["live"].as_u64().unwrap_or(0), scope["stats"]["bytes"]["total"].as_u64().unwrap_or(0), scope["stats"]["segments"].as_array().map_or(0, |s| s.len()));
        let (docs, bytes, segments) = of(&j["cluster"]);
        let fresh = &j["cluster"]["freshness"];
        let look = Look::of(&j);
        let rate = rates(then.as_ref(), &t, &look, fresh["started_ms"].as_u64().unwrap_or(0), now);
        now_counts.insert(t.clone(), look);
        // Each shard's size, and its writes and reads a minute.
        let shards: Vec<Json> = j["shards"].as_array().into_iter().flatten().map(|s| {
            let (d, b, _) = of(&s["scope"]);
            let r = &rate["shards"][s["shard"].as_u64().unwrap_or(0).to_string()];
            json!({"shard": s["shard"], "docs": d, "bytes": b, "writes_per_min": r["writes_per_min"], "reads_per_min": r["reads_per_min"]})
        }).collect();
        // The shard furthest over the mean, by documents.
        let skew = (shards.len() > 1).then(|| {
            let mean = shards.iter().filter_map(|s| s["docs"].as_f64()).sum::<f64>() / shards.len() as f64;
            let top = shards.iter().max_by(|a, b| a["docs"].as_f64().unwrap_or(0.0).total_cmp(&b["docs"].as_f64().unwrap_or(0.0)));
            top.map(|s| json!({"shard": s["shard"], "over_mean": if mean > 0.0 { ((s["docs"].as_f64().unwrap_or(0.0) / mean - 1.0) * 100.0).round() / 100.0 } else { 0.0 }}))
        }).flatten();
        // Copies behind their primary, and copies that did not answer,
        // which may be any distance behind.
        let mut stale = Vec::new();
        for s in j["shards"].as_array().into_iter().flatten() {
            for r in s["replicas"].as_array().into_iter().flatten() {
                let url = r["url"].as_str().unwrap_or_default();
                let (short, since, down) = copies.get(url).cloned().unwrap_or((Json::Null, Json::Null, false));
                if down || !r["error"].is_null() {
                    stale.push(json!({"shard": s["shard"], "copy": url, "short": short, "down": true, "since_ms": since, "error": r["error"]}));
                } else if r["behind"].as_u64().is_some_and(|b| b > 0) {
                    stale.push(json!({"shard": s["shard"], "copy": url, "short": short, "behind_txn": r["behind"]}));
                }
            }
        }
        out.push(json!({
            "table": t, "docs": docs, "bytes": bytes, "segments": segments, "shards": shards, "skew": skew,
            "writes_per_min": rate["writes_per_min"], "reads_per_min": rate["reads_per_min"], "rate_window_s": rate["window_s"], "rate_from_ms": rate["from_ms"],
            "last_write_ms": fresh["last_write_ms"], "stale": stale,
        }));
    }
    // Growth: the size an hourly row of the log store had a day and a week
    // ago (this server's rows; a coordinator keeps them of the cluster).
    if let Some(store) = &svc.log_store {
        for (key, ago) in [("d1_bytes", 86_400_000u64), ("d7_bytes", 7 * 86_400_000)] {
            let rows = store.kept_rows(crate::logstore::TABLE_SIZES, now.saturating_sub(ago + 3_600_000), now.saturating_sub(ago), None, 1000).await.unwrap_or_default();
            for t in &mut out {
                let then = rows.iter().find(|r| r["name"] == t["table"]).and_then(|r| r["bytes"].as_u64());
                if let (Some(then), Some(b)) = (then, t["bytes"].as_u64()) {
                    t["growth"][key] = json!(b as i64 - then as i64);
                }
            }
        }
    }
    svc.tables_counts.lock().unwrap_or_else(|e| e.into_inner()).push_back((now, now_counts));
    // The rates' window, when there is none yet: the next look that can
    // measure one (30 s after this one).
    let j = json!({"tables": out, "at_ms": now, "cached_s": TABLES_EVERY.as_secs(), "rate_window_max_s": RATE_WINDOW.as_secs(), "look_every_s": LOOK_EVERY.as_secs()});
    *kept = Some((std::time::Instant::now(), j.clone()));
    Ok(j)
}

/// Looks at every table's counts each [`LOOK_EVERY`] while the service
/// lives ([`tables`]: one call to each copy a table, no document
/// sampled), so that a person's first look has rates over the last
/// minutes rather than none.
pub fn keep_looking(svc: &Arc<Service>) {
    let svc = Arc::downgrade(svc);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(LOOK_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(svc) = svc.upgrade() else { return };
            let _ = tables(&svc).await;
        }
    });
}

/// Keeps each table's size in the log store's `tables`, an hour apart
/// (growth for [`tables`]).
pub async fn keep_sizes(svc: &Arc<Service>) {
    let Some(store) = svc.log_store.clone() else { return };
    let Ok(j) = tables(svc).await else { return };
    let now = telemetry::now_ms();
    for t in j["tables"].as_array().into_iter().flatten() {
        if let (Some(name), Some(docs), Some(bytes)) = (t["table"].as_str(), t["docs"].as_u64(), t["bytes"].as_u64()) {
            store.keep_table_size(now, name, docs, bytes, t["segments"].as_u64().unwrap_or(0));
        }
    }
}
// ----- end core

/// The answer of `GET /v1/index`: this server's ([`node`]) as the
/// `cluster` scope with no shards, or a coordinator's cluster, shards and
/// replicas.
pub async fn answer(svc: &Arc<Service>, a: Asked) -> Result<Json, ApiError> {
    let t0 = std::time::Instant::now();
    let reads_on = svc.reads.on();
    match &svc.backend {
        Backend::Local(_) => {
            let n = node(svc, &a).await?;
            let part = Part::of(&n, None)?;
            let mut j = json!({
                "role": "server", "table": n["table"], "server_role": n["role"], "reads_on": reads_on,
                "cluster": part.json(a.top), "shards": [], "sample": a.sample, "at_ms": telemetry::now_ms(), "took_ms": t0.elapsed().as_secs_f64() * 1e3,
            });
            j["rates"] = rates_now(svc, &j);
            Ok(j)
        }
        Backend::Cluster(c) => {
            let schema = match &a.table {
                Some(t) => svc.table(t).await?,
                None => svc.default_table()?,
            };
            let mut asked = a.clone();
            asked.table = Some(schema.table.clone());
            let nodes = c.inspect(&asked.body()).await;
            let (through, reads_from) = reads_of_every_coordinator(svc, &schema.table).await;
            let extra = |shard: &str, url: &str| through.iter().find(|(at, _)| at.shard == shard && at.replica == url).map(|(_, r)| r);
            let mut cluster: Option<Part> = None;
            let mut shards = Vec::new();
            for shard in nodes {
                let mut replicas = Vec::new();
                let mut parts: Vec<(bool, Part)> = Vec::new();
                for r in &shard.replicas {
                    let mut j = json!({"url": r.url, "role": r.role, "takes_writes": r.takes_writes});
                    match r.answer.as_ref().map_err(|e| e.clone()).and_then(|n| Part::of(n, extra(&shard.url, &r.url)).map(|p| (n, p)).map_err(|e| e.message)) {
                        Ok((n, p)) => {
                            j["server_role"] = n["role"].clone();
                            j["reads_on"] = n["reads_on"].clone();
                            j["scope"] = p.json(a.top);
                            parts.push((r.takes_writes, p));
                        }
                        Err(e) => {
                            // Its reads through here still count.
                            j["error"] = json!(e);
                            if let Some(x) = extra(&shard.url, &r.url) {
                                parts.push((false, Part { stats: None, reads: x.clone(), fresh: Fresh::default(), copies: 0 }));
                            }
                        }
                    }
                    replicas.push(j);
                }
                // The shard: its primary's data, every copy's reads.
                let data = parts.iter().find(|(w, p)| *w && p.stats.is_some()).or_else(|| parts.iter().find(|(_, p)| p.stats.is_some())).map(|(_, p)| p);
                let mut part = Part { stats: data.and_then(|p| p.stats.clone()), reads: TableReads::default(), fresh: Fresh::default(), copies: 0 };
                let writes = data.map_or(0, |p| p.fresh.writes);
                for (_, p) in &parts {
                    part.reads.merge(&p.reads);
                    part.fresh = part.fresh.latest(p.fresh, writes);
                    part.copies += p.copies;
                }
                // How far behind its primary each copy is.
                let head = data.and_then(|p| p.stats.as_ref()).and_then(|s| s.seq_no);
                for j in &mut replicas {
                    if let (Some(h), Some(s)) = (head, j["scope"]["stats"]["seq_no"].as_u64()) {
                        j["behind"] = json!(h.saturating_sub(s));
                    }
                }
                shards.push(json!({"shard": shard.index, "url": shard.url, "replicas": replicas, "scope": part.json(a.top)}));
                cluster = Some(match cluster.take() {
                    None => part,
                    Some(mut all) => {
                        match (&mut all.stats, &part.stats) {
                            (Some(x), Some(y)) => x.merge(y),
                            (None, Some(y)) => all.stats = Some(y.clone()),
                            _ => {}
                        }
                        all.reads.merge(&part.reads);
                        let writes = all.fresh.writes + part.fresh.writes;
                        all.fresh = all.fresh.latest(part.fresh, writes);
                        all.copies += part.copies;
                        all
                    }
                });
            }
            let cluster = cluster.unwrap_or(Part { stats: None, reads: TableReads::default(), fresh: Fresh::default(), copies: 0 });
            let mut j = json!({
                "role": "coordinator", "table": schema.table, "reads_on": reads_on, "reads_from": reads_from,
                "cluster": cluster.json(a.top), "shards": shards, "sample": a.sample, "at_ms": telemetry::now_ms(), "took_ms": t0.elapsed().as_secs_f64() * 1e3,
            });
            j["rates"] = rates_now(svc, &j);
            Ok(j)
        }
    }
}

/// An answer's writes and reads a minute, over the same window as
/// [`tables`]' (the drill-down and the list agree).
fn rates_now(svc: &Service, j: &Json) -> Json {
    let now = telemetry::now_ms();
    let table = j["table"].as_str().unwrap_or_default();
    rates(earlier(svc, now).as_ref(), table, &Look::of(j), j["cluster"]["freshness"]["started_ms"].as_u64().unwrap_or(0), now)
}

/// Table `table`'s reads through this coordinator and every other one
/// ([`crate::peers`], `GET /v1/index/reads`), merged by the copy that
/// served them (INS-3); and which coordinators said theirs.
async fn reads_of_every_coordinator(svc: &Arc<Service>, table: &str) -> (Vec<(At, TableReads)>, Json) {
    let mut through = svc.reads.of(table);
    let answers = crate::peers::get_all(svc, &format!("/v1/index/reads?table={}", crate::peers::encode(table))).await;
    for (_, a) in &answers {
        let Ok(j) = a else { continue };
        for x in j["reads"].as_array().map_or(&[][..], |v| v.as_slice()) {
            let (Ok(at), Ok(r)) = (serde_json::from_value::<At>(x["at"].clone()), serde_json::from_value::<TableReads>(x["reads"].clone())) else { continue };
            match through.iter_mut().find(|(a, _)| *a == at) {
                Some((_, t)) => t.merge(&r),
                None => through.push((at, r)),
            }
        }
    }
    (through, crate::peers::reach(svc, &answers))
}

/// `GET /v1/index/reads?table=`: this server's reads of the table, by
/// the copy that served them, for another coordinator to merge.
pub fn reads_json(svc: &Service, table: &str) -> Json {
    json!({"table": table, "reads": svc.reads.of(table).into_iter().map(|(at, r)| json!({"at": at, "reads": r})).collect::<Vec<_>>()})
}

/// A shard's replicas' answers to a coordinator's inspection.
pub struct ShardNodes {
    pub index: usize,
    pub url: String,
    pub replicas: Vec<ReplicaNode>,
}

/// A replica's answer: its [`node`], or why it gave none.
pub struct ReplicaNode {
    pub url: String,
    pub role: String,
    pub takes_writes: bool,
    pub answer: Result<Json, String>,
}
