//! The engine: it creates the tables, seeds them, and then every minute
//! writes what the network reported, runs the alert rules (each a SQL
//! query), groups the alerts that fire into incidents over the topology,
//! and has the agents investigate. A director injects a fault now and then
//! (or when asked), so there is something to find; the fault only bends the
//! telemetry, and nothing downstream is told what it was.
//!
//! One lock serialises a minute's work with what a person does (injecting a
//! fault, approving an action), so the state below is changed by one task at
//! a time and its mutex is only ever held between awaits.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::graph::Graph;
use crate::seed;
use crate::strata::{Error, Result, Row, RowExt, Strata};
use crate::telemetry::{self, iso, parse_iso, Fault};
use crate::world::{Rng, World};
use crate::{agents, schema};

pub struct Opts {
    /// Hours of telemetry written when the tables are first made.
    pub history_hours: i64,
    /// Minutes between the faults the director injects; 0 for none.
    pub fault_every: i64,
    /// Whether the simulator writes at all (off: the tables are only read).
    pub simulate: bool,
    /// Drop the tables first.
    pub reset: bool,
    /// About how many objects the simulated estate has (0: none).
    pub estate: usize,
    /// The model the server's llm() is asked to rewrite a search with ("" for none).
    pub rewrite_model: String,
}

/// An incident that is not resolved yet.
#[derive(Default, Clone)]
pub struct Open {
    pub row: Row,
    pub quiet_since: Option<i64>,
    pub rca_at: Vec<i64>,
    pub alerts_at_rca: usize,
    /// The runbook's action was approved and the alerts cleared after it.
    pub applied_by: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Saved {
    last_minute: i64,
    faults: Vec<Fault>,
    next_fault_at: i64,
    fault_every: i64,
    autopilot: bool,
    seq: i64,
    rotation: usize,
    semantic: bool,
}

pub struct State {
    pub phase: String,
    pub progress: f64,
    pub ready: bool,
    pub error: Option<String>,
    pub faults: Vec<Fault>,
    pub last_minute: i64,
    /// Firing alerts by `rule:entity`.
    pub alerts: BTreeMap<String, Row>,
    pub misses: HashMap<String, i64>,
    pub open: BTreeMap<String, Open>,
    /// What is not in its usual state: `link:<id>`, `dev:<id>`, `bgp:<id>`.
    pub status: HashMap<String, String>,
    pub next_fault_at: i64,
    pub fault_every: i64,
    pub autopilot: bool,
    pub seq: i64,
    pub rotation: usize,
    pub written: BTreeMap<String, u64>,
    pub last_rows: BTreeMap<String, u64>,
    /// Whether the server walks graphs in SQL (a coordinator does not yet).
    pub graph_sql: Option<bool>,
    /// Whether the embedding model was there when the tables were made.
    pub semantic: bool,
    pub started_at: i64,
    pub eval_ms: f64,
    pub tick_ms: f64,
    pub sources: Vec<Value>,
    /// Silences that are not over, by id.
    pub silences: BTreeMap<String, Row>,
}

pub struct Engine {
    pub db: Strata,
    pub w: World,
    pub graph: Graph,
    /// The simulated estate behind the backbone, when there is one.
    pub estate: Option<crate::estate::Estate>,
    pub opts: Opts,
    state: Mutex<State>,
    pub lock: tokio::sync::Mutex<()>,
    counter: AtomicU64,
    reset_done: AtomicBool,
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn minute_now() -> i64 {
    let t = now();
    t - t % 60
}

const ROTATION: &[&str] = &["fiber_cut", "bad_change", "ddos", "optic_degrade", "cpu_bug", "power"];

fn bad(message: impl Into<String>) -> Error {
    Error { status: 400, message: message.into() }
}

impl Engine {
    pub fn new(db: Strata, opts: Opts) -> Arc<Engine> {
        Engine::with(db, opts, World::build())
    }

    /// The network as the inventory tables hold it, for running on real
    /// feeds with the simulator off. `None` when there is no inventory yet
    /// (the tables are created, so there is something to fill).
    pub async fn inventory(db: &Strata) -> Result<Option<World>> {
        db.health().await?;
        for (_, ddl, _) in schema::tables() {
            if let Err(e) = db.sql(&ddl).await {
                let Some(at) = ddl.find("emb VECTOR") else { return Err(e) };
                db.sql(&format!("{}\n)", ddl[..at].trim_end().trim_end_matches(','))).await?;
            }
        }
        let all = |t: &'static str| async move { db.query(&format!("SELECT * FROM {t} LIMIT 10000"), json!({})).await };
        let devices = all("netops_devices").await?;
        if devices.is_empty() {
            return Ok(None);
        }
        Ok(Some(World::from_rows(&all("netops_sites").await?, &devices, &all("netops_links").await?, &all("netops_services").await?, &all("netops_customers").await?, &all("netops_bgp").await?)))
    }

    pub fn with(db: Strata, opts: Opts, w: World) -> Arc<Engine> {
        let graph = Graph::of(&w);
        let fault_every = opts.fault_every;
        let estate = (opts.simulate && opts.estate > 0).then(|| crate::estate::Estate::build(&w, opts.estate));
        Arc::new(Engine {
            db,
            graph,
            estate,
            w,
            opts,
            state: Mutex::new(State {
                phase: "Starting".into(),
                progress: 0.0,
                ready: false,
                error: None,
                faults: Vec::new(),
                last_minute: 0,
                alerts: BTreeMap::new(),
                misses: HashMap::new(),
                open: BTreeMap::new(),
                status: HashMap::new(),
                next_fault_at: 0,
                fault_every,
                autopilot: false,
                seq: 2420,
                rotation: 0,
                written: BTreeMap::new(),
                last_rows: BTreeMap::new(),
                graph_sql: None,
                semantic: true,
                started_at: now(),
                eval_ms: 0.0,
                tick_ms: 0.0,
                sources: Vec::new(),
                silences: BTreeMap::new(),
            }),
            lock: tokio::sync::Mutex::new(()),
            counter: AtomicU64::new(0),
            reset_done: AtomicBool::new(false),
        })
    }

    pub fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn next_id(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn phase(&self, phase: &str, progress: f64) {
        let mut st = self.st();
        st.phase = phase.into();
        st.progress = progress;
    }

    /// Sets the tables up, then keeps the minute: forever.
    pub async fn run(self: Arc<Self>) {
        loop {
            match self.setup().await {
                Ok(()) => break,
                Err(e) => {
                    tracing::warn!("setting up: {e}");
                    self.st().error = Some(format!("Setting up: {e}"));
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
        {
            let mut st = self.st();
            st.ready = true;
            st.error = None;
            st.phase = "Running".into();
            st.progress = 1.0;
        }
        if !self.opts.simulate {
            // Real feeds: nothing is written by the app but what it derives.
            // Each minute, once it has had a minute to arrive, the rules run
            // over it, alerts are grouped over the topology and agents work.
            loop {
                let m = minute_now() - 60;
                if m > self.st().last_minute {
                    let t0 = Instant::now();
                    let res = self.live_minute(m).await;
                    {
                        let mut st = self.st();
                        st.tick_ms = t0.elapsed().as_secs_f64() * 1000.0;
                        st.error = res.err().map(|e| { tracing::warn!("minute {}: {e}", iso(m)); format!("{}: {e}", iso(m)) });
                    }
                }
                tokio::time::sleep(Duration::from_secs((60 - now() % 60 + 1) as u64)).await;
            }
        }
        loop {
            let m = minute_now();
            if m > self.st().last_minute {
                let t0 = Instant::now();
                match self.tick(m).await {
                    Ok(()) => {
                        let mut st = self.st();
                        st.error = None;
                        st.tick_ms = t0.elapsed().as_secs_f64() * 1000.0;
                    }
                    Err(e) => {
                        tracing::warn!("minute {}: {e}", iso(m));
                        self.st().error = Some(format!("{}: {e}", iso(m)));
                    }
                }
            }
            let wait = 60 - now() % 60 + 1;
            tokio::time::sleep(Duration::from_secs(wait as u64)).await;
        }
    }

    async fn setup(&self) -> Result<()> {
        self.phase("Waiting for ChloeDB", 0.0);
        self.db.health().await?;
        if self.opts.reset && !self.reset_done.swap(true, Ordering::SeqCst) {
            self.phase("Dropping the tables", 0.01);
            for name in schema::names().iter().rev() {
                let _ = self.db.sql(&format!("DROP TABLE IF EXISTS {name}")).await;
            }
        }
        self.phase("Creating the tables", 0.02);
        let mut semantic = true;
        for (name, ddl, _) in schema::tables() {
            if let Err(e) = self.db.sql(&ddl).await {
                // Without the embedding model the tables are made without
                // their vector column, and searches rank by words alone.
                let Some(at) = ddl.find("emb VECTOR") else { return Err(e) };
                tracing::warn!("{name}: {e}; creating it without embeddings");
                let plain = format!("{}\n)", ddl[..at].trim_end().trim_end_matches(','));
                self.db.sql(&plain).await?;
                semantic = false;
            }
        }
        // A server may take the column and only refuse to use it: ask once,
        // so that searches rank by words alone instead of being refused.
        if semantic {
            if let Err(e) = self.db.query("SELECT id FROM netops_runbooks\nORDER BY similarity(emb, 'probe') DESC LIMIT 1", json!({})).await {
                tracing::warn!("search by meaning is off: {e}");
                semantic = false;
            }
        }
        let saved = self.db.query("SELECT val FROM netops_state WHERE id = 'sim'", json!({})).await?;
        let saved: Option<Saved> = saved.first().and_then(|r| serde_json::from_str(&r.s("val")).ok());
        match saved {
            Some(s) => {
                self.restore(s).await?;
                self.builtins().await?;
                let mut st = self.st();
                st.semantic = st.semantic && semantic;
            }
            None => {
                if !self.opts.simulate {
                    // A real network's first start: the rules and runbooks
                    // the app ships with, and nothing made up.
                    self.st().semantic = semantic;
                    let now = minute_now();
                    self.db.upsert("netops_alert_rules", seed::rule_rows()).await?;
                    self.db.upsert("netops_runbooks", seed::runbook_rows(now)).await?;
                    {
                        let mut st = self.st();
                        st.sources = seed::source_rows(now);
                        st.last_minute = now - 120;
                    }
                    return self.persist().await;
                }
                self.st().semantic = semantic;
                self.seed(minute_now()).await?
            }
        }
        crate::estate::ensure(self).await
    }

    /// The built-in rules and runbooks a newer version ships, added to
    /// tables an older one made; those already there are left as they are
    /// (a rule switched off stays off).
    async fn builtins(&self) -> Result<()> {
        let have: HashSet<String> = self.db.query("SELECT id FROM netops_alert_rules LIMIT 1000", json!({})).await?.iter().map(|r| r.s("id")).collect();
        let rules: Vec<Value> = seed::rule_rows().into_iter().filter(|r| !have.contains(r["id"].as_str().unwrap_or(""))).collect();
        if !rules.is_empty() {
            self.db.upsert("netops_alert_rules", rules).await?;
        }
        let have: HashSet<String> = self.db.query("SELECT id FROM netops_runbooks LIMIT 1000", json!({})).await?.iter().map(|r| r.s("id")).collect();
        let books: Vec<Value> = seed::runbook_rows(minute_now()).into_iter().filter(|r| !have.contains(r["id"].as_str().unwrap_or(""))).collect();
        if !books.is_empty() {
            self.db.upsert("netops_runbooks", books).await?;
        }
        Ok(())
    }

    async fn restore(&self, s: Saved) -> Result<()> {
        self.phase("Reading where the network was left", 0.5);
        let alerts = self.db.query("SELECT * FROM netops_alerts WHERE status IN ('firing', 'silenced') LIMIT 5000", json!({})).await?;
        let silences = self.db.query("SELECT * FROM netops_silences WHERE status = 'active' LIMIT 1000", json!({})).await?;
        let open = self.db.query("SELECT * FROM netops_incidents WHERE status != 'resolved' AND origin = 'live' LIMIT 200", json!({})).await?;
        let links = self.db.query("SELECT id FROM netops_links WHERE status != 'up' LIMIT 1000", json!({})).await?;
        let devices = self.db.query("SELECT id FROM netops_devices WHERE status != 'up' LIMIT 1000", json!({})).await?;
        let bgp = self.db.query("SELECT id FROM netops_bgp WHERE state != 'established' LIMIT 1000", json!({})).await?;
        let last = {
            let mut st = self.st();
            st.faults = s.faults;
            st.last_minute = s.last_minute;
            st.next_fault_at = s.next_fault_at;
            st.autopilot = s.autopilot;
            st.seq = s.seq.max(2420);
            st.rotation = s.rotation;
            st.semantic = s.semantic;
            for r in silences {
                st.silences.insert(r.s("id"), r);
            }
            for a in alerts {
                st.alerts.insert(format!("{}:{}", a.s("rule"), a.s("entity")), a);
            }
            for mut r in open {
                r.remove("emb");
                st.open.insert(r.s("id"), Open { row: r, ..Default::default() });
            }
            for l in links {
                st.status.insert(format!("link:{}", l.s("id")), "down".into());
            }
            for d in devices {
                st.status.insert(format!("dev:{}", d.s("id")), "down".into());
            }
            for b in bgp {
                st.status.insert(format!("bgp:{}", b.s("id")), "idle".into());
            }
            st.sources = seed::source_rows(s.last_minute);
            st.last_minute
        };
        if !self.opts.simulate {
            return Ok(());
        }
        // The minutes missed while the app was down, up to the history kept.
        let m = minute_now();
        let from = (last + 60).max(m - self.opts.history_hours * 3600);
        let total = ((m - from) / 60).max(1) as f64;
        let mut t = from;
        while t < m {
            self.write_minute(t).await?;
            self.phase("Writing the minutes missed while the app was down", 0.5 + 0.5 * ((t - from) / 60) as f64 / total);
            t += 60;
        }
        self.st().last_minute = m - 60;
        Ok(())
    }

    /// Fills empty tables: the network's records, a month of history, and
    /// the last hours of telemetry.
    async fn seed(&self, now: i64) -> Result<()> {
        let w = &self.w;
        let day = 86400;
        self.phase("Writing the inventory", 0.04);
        let mut rng = Rng::new(5);
        self.db.upsert("netops_sites", w.site_rows()).await?;
        self.db.upsert("netops_devices", w.device_rows(&iso(now - 400 * day))).await?;
        self.db.upsert("netops_links", w.link_rows()).await?;
        self.db.upsert("netops_circuits", w.circuit_rows(&mut rng)).await?;
        self.db.upsert("netops_bgp", w.peer_rows(&iso(now - 23 * day))).await?;
        self.db.upsert("netops_services", w.service_rows()).await?;
        self.db.upsert("netops_customers", w.customer_rows()).await?;
        self.phase("Writing rules, runbooks and a month of history", 0.08);
        self.db.upsert("netops_alert_rules", seed::rule_rows()).await?;
        self.db.upsert("netops_runbooks", seed::runbook_rows(now)).await?;
        self.db.upsert("netops_advisories", seed::advisory_rows(now)).await?;
        let h = seed::history(w, now);
        self.db.upsert("netops_changes", h.changes).await?;
        self.db.upsert("netops_configs", h.configs).await?;
        self.db.upsert("netops_incidents", h.incidents).await?;
        self.db.upsert("netops_timeline", h.timeline).await?;
        self.db.upsert("netops_tickets", h.tickets).await?;
        self.db.upsert("netops_judgments", h.judgments).await?;
        self.db.upsert("netops_oncall", h.oncall).await?;
        self.db.upsert("netops_maintenance", seed::maintenance_rows(w, now)).await?;
        self.db.upsert("netops_handovers", seed::handover_rows(w, now)).await?;
        self.db.upsert("netops_faults", h.faults.iter().map(|f| fault_row(f, now)).collect()).await?;
        let sources = seed::source_rows(now);
        self.db.upsert("netops_sources", sources.clone()).await?;
        let start = now - self.opts.history_hours * 3600;
        {
            let mut st = self.st();
            st.sources = sources;
            st.faults = h.faults.into_iter().filter(|f| f.ended.is_none_or(|e| e > start - 3600)).collect();
        }
        // Every link's two ends announce their neighbour once, as they did
        // when the link came up: what the topology is read back from.
        let mut logs = Vec::new();
        for (i, l) in w.links.iter().enumerate() {
            if l.live {
                telemetry::adjacency_logs(w, i, start - 40 + (i as i64 % 20), &mut logs);
            }
        }
        self.db.upsert("netops_syslog", telemetry::log_rows(w, &logs)).await?;
        let total = ((now - start) / 60).max(1) as f64;
        let mut t = start;
        let mut ops: Vec<Value> = Vec::new();
        let faults = self.st().faults.clone();
        while t < now {
            for (table, rows) in telemetry::minute(w, &faults, t) {
                self.count(table, rows.len());
                ops.extend(rows.into_iter().map(|doc| json!({"op": "upsert", "table": table, "doc": doc})));
            }
            if ops.len() >= 4000 {
                self.db.write(std::mem::take(&mut ops)).await?;
                self.phase(&format!("Writing {} hours of telemetry", self.opts.history_hours), 0.12 + 0.82 * ((t - start) / 60) as f64 / total);
            }
            t += 60;
        }
        self.db.write(ops).await?;
        {
            let mut st = self.st();
            st.last_minute = now - 60;
            st.next_fault_at = now + 4 * 60;
        }
        self.phase("Reading the topology back from the logs", 0.96);
        agents::discover(self, now, false).await?;
        self.persist().await
    }

    fn count(&self, table: &str, n: usize) {
        *self.st().written.entry(table.to_string()).or_default() += n as u64;
    }

    async fn persist(&self) -> Result<()> {
        let (val, at) = {
            let st = self.st();
            let s = Saved { last_minute: st.last_minute, faults: st.faults.clone(), next_fault_at: st.next_fault_at, fault_every: st.fault_every, autopilot: st.autopilot, seq: st.seq, rotation: st.rotation, semantic: st.semantic };
            (serde_json::to_string(&s).unwrap_or_default(), st.last_minute)
        };
        self.db.upsert("netops_state", vec![json!({"id": "sim", "val": val, "updated": iso(at)})]).await?;
        Ok(())
    }

    /// The rows the network reports for the minute starting at `t`.
    async fn write_minute(&self, t: i64) -> Result<()> {
        let faults = self.st().faults.clone();
        let mut ops = Vec::new();
        let mut counts = BTreeMap::new();
        for (table, rows) in telemetry::minute(&self.w, &faults, t) {
            counts.insert(table.to_string(), rows.len() as u64);
            ops.extend(rows.into_iter().map(|doc| json!({"op": "upsert", "table": table, "doc": doc})));
        }
        self.db.write(ops).await?;
        let mut st = self.st();
        for (k, v) in &counts {
            *st.written.entry(k.clone()).or_default() += v;
        }
        st.last_rows = counts;
        Ok(())
    }

    /// One minute: the director, the telemetry, then everything that reads it.
    pub async fn tick(&self, m: i64) -> Result<()> {
        let _guard = self.lock.lock().await;
        let last = self.st().last_minute;
        if m <= last {
            return Ok(());
        }
        let mut t = (last + 60).max(m - 30 * 60);
        while t < m {
            self.write_minute(t).await?;
            t += 60;
        }
        self.director(m).await?;
        self.write_minute(m).await?;
        self.st().last_minute = m;
        crate::estate::minute(self, m).await?;
        self.after_minute(m).await
    }

    /// A minute of a real network: the feeds wrote it, the app reads it.
    async fn live_minute(&self, m: i64) -> Result<()> {
        let _guard = self.lock.lock().await;
        self.st().last_minute = m;
        self.housekeeping(m).await?;
        self.evaluate(m).await?;
        self.correlate(m).await?;
        self.lifecycle(m).await?;
        if m % 300 == 0 {
            agents::discover(self, m, false).await?;
        }
        self.sources(m).await?;
        self.persist().await
    }

    async fn after_minute(&self, m: i64) -> Result<()> {
        self.sync_status(m).await?;
        self.housekeeping(m).await?;
        self.evaluate(m).await?;
        self.correlate(m).await?;
        self.lifecycle(m).await?;
        if m % 300 == 0 {
            agents::discover(self, m, false).await?;
        }
        self.sources(m).await?;
        self.persist().await
    }

    // ---- the director: faults begin, are mitigated and are repaired

    async fn director(&self, m: i64) -> Result<()> {
        let mut todo: Vec<(String, &'static str)> = Vec::new();
        {
            let mut st = self.st();
            for f in st.faults.iter_mut().filter(|f| f.origin == "live" && f.ended.is_none()) {
                match f.mitigated {
                    None if m >= f.started + f.auto_mitigate_after => todo.push((f.id.clone(), "mitigate")),
                    Some(at) if m >= at + f.repair_after => {
                        f.ended = Some(m);
                        todo.push((f.id.clone(), "repaired"));
                    }
                    _ => {}
                }
            }
        }
        for (id, what) in todo {
            if what == "mitigate" {
                self.mitigate(&id, m, "On-call engineer (simulated)").await?;
            } else {
                self.repaired(&id, m).await?;
            }
        }
        let scenario = {
            let mut st = self.st();
            let busy = st.faults.iter().any(|f| f.origin == "live" && f.ended.is_none());
            if st.fault_every > 0 && !busy && m >= st.next_fault_at {
                st.rotation += 1;
                Some(ROTATION[(st.rotation - 1) % ROTATION.len()])
            } else {
                None
            }
        };
        if let Some(s) = scenario {
            self.start_fault(s, m, false, None).await?;
        }
        Ok(())
    }

    /// Begins a fault. With `rewind` it began a few minutes ago and those
    /// minutes are written again, so that someone who asks for a fault sees
    /// its alerts at once rather than after the rules' windows fill.
    async fn start_fault(&self, scenario: &str, m: i64, rewind: bool, seed: Option<u64>) -> Result<Fault> {
        let lead = if rewind { match scenario { "optic_degrade" => 17, "cpu_bug" => 8, "ddos" => 5, _ => 3 } } else { 0 };
        let started = m - lead * 60;
        let mut rng = Rng::new(seed.unwrap_or(m as u64 ^ 0x5151));
        let mut f = seed::new_fault(&self.w, scenario, started, &mut rng, "live");
        // The minute, the scenario, and how many came before: two in one minute stay apart.
        f.id = format!("flt-{}-{}-{}", m / 60, f.scenario, self.st().faults.len());
        f.auto_mitigate_after += lead * 60;
        if f.scenario == "bad_change" {
            let dev = f.device.clone().unwrap_or_default();
            let chg = f.change.clone().unwrap_or_default();
            let d = self.w.device(&dev).ok_or_else(|| bad("no such device"))?;
            self.db.upsert("netops_changes", vec![json!({
                "id": chg, "title": format!("Route-policy update on {dev}: drop bogons from transit"), "description": "Add a bogon filter to RP-TRANSIT-IN.", "kind": "config", "risk": "medium",
                "status": "completed", "window_start": iso(started - 600), "window_end": iso(started + 3000), "executed": iso(started - 20), "owner": "J. Vermeer", "approver": "S. Haddad",
                "devices": [dev], "device_nums": [d.num], "sites": [d.site], "services": ["ip-transit"], "ticket": format!("TKT-{}", 70000 + (m / 60) % 9000),
                "rollback_plan": "rollback configuration last 1", "diff": "  route-policy RP-TRANSIT-IN\n+   if destination in PS-BOGONS-V4 then drop endif\n    set local-preference 100", "outcome": "",
            })]).await?;
            self.config_version(&dev, started - 20, "J. Vermeer", &chg, |body| body.replace("route-policy RP-TRANSIT-IN\n", "route-policy RP-TRANSIT-IN\n  if destination in PS-BOGONS-V4 then drop endif\n")).await?;
        }
        self.db.upsert("netops_faults", vec![fault_row(&f, m)]).await?;
        self.st().faults.push(f.clone());
        if rewind {
            let mut t = started;
            while t <= m {
                self.write_minute(t).await?;
                t += 60;
            }
        }
        tracing::info!("fault {} began ({})", f.id, f.truth_entity);
        Ok(f)
    }

    /// A new version of a device's configuration, made from its latest.
    async fn config_version(&self, dev: &str, at: i64, author: &str, change: &str, edit: impl Fn(&str) -> String) -> Result<()> {
        let last = self.db.query("SELECT version, body FROM netops_configs WHERE device = :d ORDER BY version DESC LIMIT 1", json!({"d": dev})).await?;
        let Some(last) = last.first() else { return Ok(()) };
        let body = edit(&last.s("body"));
        let v = last.i("version") + 1;
        self.db.upsert("netops_configs", vec![json!({"id": format!("{dev}@{v}"), "device": dev, "version": v, "ts": iso(at), "author": author, "change_id": change, "lines": body.lines().count(), "body": body})]).await?;
        Ok(())
    }

    /// Takes the traffic off a fault (or undoes it): what an approved action
    /// or the simulated engineer does.
    pub async fn mitigate(&self, fault: &str, m: i64, actor: &str) -> Result<()> {
        let f = {
            let mut st = self.st();
            let Some(f) = st.faults.iter_mut().find(|f| f.id == fault) else { return Ok(()) };
            if f.mitigated.is_some() {
                return Ok(());
            }
            f.mitigated = Some(m);
            f.clone()
        };
        if m == self.st().last_minute {
            self.write_minute(m).await?;
        }
        self.db.upsert("netops_faults", vec![fault_row(&f, m)]).await?;
        if f.scenario == "bad_change" {
            let (dev, chg) = (f.device.clone().unwrap_or_default(), f.change.clone().unwrap_or_default());
            self.db.sql(&format!("UPDATE netops_changes SET status = 'rolled_back', outcome = 'rolled back after alarms' WHERE id = '{chg}'")).await?;
            self.config_version(&dev, m, "noc-automation", &format!("rollback of {chg}"), |body| body.replace("  if destination in PS-BOGONS-V4 then drop endif\n", "")).await?;
        }
        if let Some(inc) = &f.incident {
            let note = match f.scenario.as_str() {
                "fiber_cut" => "Spread the span's traffic across a third path (TE template PLANE-FAILOVER); the surviving plane is back under 80%".to_string(),
                "optic_degrade" => "Drained the link (IGP metric 65535); traffic moved to the parallel path and the errors stopped mattering".to_string(),
                "bad_change" => format!("Rolled back change {}; BGP sessions are re-establishing", f.change.clone().unwrap_or_default()),
                "ddos" => "Announced a flowspec rule that redirects the attacked prefix to scrubbing; the borders are draining".to_string(),
                "cpu_bug" => "Restarted the routing process gracefully; CPU fell and the neighbours stopped timing out".to_string(),
                _ => "Facilities restored power feed A; the devices are booting".to_string(),
            };
            self.timeline(inc, m, "action", actor, &note, &f.id).await?;
            let row = {
                let mut st = self.st();
                st.open.get_mut(inc).map(|o| {
                    o.row.insert("status".into(), json!("mitigated"));
                    o.row.insert("mitigated".into(), json!(iso(m)));
                    o.row.clone()
                })
            };
            if let Some(row) = row {
                self.db.upsert("netops_incidents", vec![Value::Object(row)]).await?;
            }
        }
        Ok(())
    }

    async fn repaired(&self, fault: &str, m: i64) -> Result<()> {
        let f = {
            let mut st = self.st();
            let every = st.fault_every.max(10);
            st.next_fault_at = m + every * 60;
            st.faults.iter().find(|f| f.id == fault).cloned()
        };
        let Some(f) = f else { return Ok(()) };
        self.db.upsert("netops_faults", vec![fault_row(&f, m)]).await?;
        if let Some(inc) = &f.incident {
            let note = match f.scenario.as_str() {
                "fiber_cut" => "The provider repaired the span; light is back on both ends",
                "optic_degrade" => "Field services replaced the optic; receive power is back to normal",
                "bad_change" => "The change was withdrawn for review",
                "ddos" => "The attack stopped; the flowspec rule can be removed",
                "cpu_bug" => "The router is stable; an upgrade is still needed to remove the defect",
                _ => "All devices at the site are back and adjacent",
            };
            self.timeline(inc, m, "status", "Simulator", note, &f.id).await?;
        }
        Ok(())
    }

    pub async fn timeline(&self, incident: &str, ts: i64, kind: &str, actor: &str, note: &str, ref_id: &str) -> Result<()> {
        let id = format!("{incident}:{ts}:{}", self.next_id());
        self.db.upsert("netops_timeline", vec![json!({"id": id, "incident": incident, "ts": iso(ts), "kind": kind, "actor": actor, "note": note, "ref_id": ref_id})]).await?;
        Ok(())
    }

    // ---- what is up and what is not, kept in the records

    async fn sync_status(&self, m: i64) -> Result<()> {
        let w = &self.w;
        let faults = self.st().faults.clone();
        let fx = telemetry::effects(w, &faults, m);
        let mut want: HashMap<String, String> = HashMap::new();
        for (i, l) in w.links.iter().enumerate().filter(|(_, l)| l.live && l.documented) {
            if fx.link_down(i) || fx.dev_down(l.a) || fx.dev_down(l.b) {
                want.insert(format!("link:{}", l.id), "down".into());
            }
        }
        for (i, d) in w.devices.iter().enumerate() {
            if fx.dev_down(i) {
                want.insert(format!("dev:{}", d.id), "down".into());
            }
        }
        for (i, p) in w.peers.iter().enumerate() {
            if fx.bgp_down.contains(&i) || fx.dev_down(p.device) {
                want.insert(format!("bgp:{}", p.id), "idle".into());
            }
        }
        let have = self.st().status.clone();
        let mut sql: Vec<String> = Vec::new();
        for key in want.keys().filter(|k| !have.contains_key(*k)).chain(have.keys().filter(|k| !want.contains_key(*k))) {
            let down = want.contains_key(key);
            let (kind, id) = key.split_once(':').unwrap_or(("", ""));
            sql.push(match kind {
                "link" => format!("UPDATE netops_links SET status = '{}' WHERE id = '{id}'", if down { "down" } else { "up" }),
                "dev" => format!("UPDATE netops_devices SET status = '{}' WHERE id = '{id}'", if down { "down" } else { "up" }),
                _ => {
                    let rx = w.peers.iter().find(|p| p.id == id).map(|p| p.prefixes).unwrap_or(0);
                    format!("UPDATE netops_bgp SET state = '{}', prefixes_rx = {}, since = '{}' WHERE id = '{id}'", if down { "idle" } else { "established" }, if down { 0 } else { rx }, iso(m))
                }
            });
        }
        for s in sql {
            self.db.sql(&s).await?;
        }
        self.st().status = want;
        Ok(())
    }

    // ---- alert rules: each one a query

    /// The devices and sites an alert's entity stands for.
    pub fn scope(&self, kind: &str, entity: &str) -> (Vec<String>, Vec<String>) {
        let w = &self.w;
        match kind {
            "link" => match w.link_by_id(entity) {
                Some(i) => {
                    let l = &w.links[i];
                    let (a, b) = (&w.devices[l.a], &w.devices[l.b]);
                    let mut sites = vec![a.site.to_string()];
                    if b.site != a.site {
                        sites.push(b.site.to_string());
                    }
                    (vec![a.id.clone(), b.id.clone()], sites)
                }
                None => (Vec::new(), Vec::new()),
            },
            "device" => match w.device(entity) {
                Some(d) => (vec![d.id.clone()], vec![d.site.to_string()]),
                None => (Vec::new(), Vec::new()),
            },
            "service" => (Vec::new(), w.services.iter().find(|s| s.id == entity).map(|s| s.sites.iter().map(|x| x.to_string()).collect()).unwrap_or_default()),
            "site" => (w.devices.iter().filter(|d| d.site == entity && d.role == "border").map(|d| d.id.clone()).collect(), vec![entity.to_string()]),
            // An object of the estate stands for itself, at its hub: what
            // breaks in one hub is grouped, and kept apart from the backbone.
            "estate" => (Vec::new(), crate::estate::hub_of(self.estate.as_ref(), entity).into_iter().collect()),
            _ => (Vec::new(), Vec::new()),
        }
    }

    async fn evaluate(&self, m: i64) -> Result<()> {
        let t_all = Instant::now();
        let rules = self.db.query("SELECT * FROM netops_alert_rules LIMIT 500", json!({})).await?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut alert_rows: Vec<Value> = Vec::new();
        let mut rule_rows: Vec<Value> = Vec::new();
        let mut managed: HashSet<String> = HashSet::new();
        for mut r in rules {
            let rid = r.s("id");
            managed.insert(rid.clone());
            if r.get("enabled") != Some(&Value::Bool(true)) {
                continue;
            }
            let sql = r.s("sql_text");
            let window = r.i("window_s").max(60);
            let params = if sql.contains(":since") { json!({"since": iso(m - window + 1)}) } else { json!({}) };
            let t0 = Instant::now();
            let res = self.db.query(&sql, params).await;
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mut firing = 0;
            let rows = match res {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!("rule {rid}: {e}");
                    r.insert("last_eval".into(), json!(iso(m)));
                    r.insert("last_rows".into(), json!(-1));
                    rule_rows.push(Value::Object(r));
                    // A rule that cannot run keeps its alerts as they were.
                    seen.extend(self.st().alerts.keys().filter(|k| k.starts_with(&format!("{rid}:"))).cloned());
                    continue;
                }
            };
            let n = rows.len();
            let (cmp, threshold) = (r.s("cmp"), r.f("threshold"));
            for row in rows.into_iter().take(300) {
                let entity = row.s("entity");
                // The rule fires on a row only when its reading passes the
                // rule's comparison with its threshold.
                if entity.is_empty() || !fires(&cmp, threshold, &row) {
                    continue;
                }
                let v = telemetry::round(row.f("v"), 3);
                let key = format!("{rid}:{entity}");
                firing += 1;
                seen.insert(key.clone());
                let mut st = self.st();
                st.misses.remove(&key);
                if st.alerts.contains_key(&key) {
                    // A silence that began or ended since the last minute
                    // changes what an alert outside an incident is.
                    let quiet = st.alerts.get(&key).filter(|a| a.s("incident").is_empty()).map(|a| crate::daily::silence_for(&st.silences, a, m));
                    let a = st.alerts.get_mut(&key).expect("it is there");
                    a.insert("reading".into(), json!(v));
                    a.insert("last_seen".into(), json!(iso(m)));
                    if let Some(by) = quiet {
                        a.insert("status".into(), json!(if by.is_empty() { "firing" } else { "silenced" }));
                        a.insert("silenced".into(), json!(by));
                    }
                    alert_rows.push(Value::Object(a.clone()));
                    continue;
                }
                drop(st);
                let kind = r.s("entity_kind");
                let (devices, sites) = self.scope(&kind, &entity);
                let name = r.s("name");
                let unit = r.s("unit");
                // A row may say more than its reading (`detail`), and how
                // serious it is (`severity`), where the rule's query knows.
                let detail = row.s("detail");
                let summary = if unit == "up" { format!("{name}: {entity}") } else if kind == "estate" { format!("{name}: {entity}, {} {unit}", fmt_num(v)) } else { format!("{name}: {entity} at {} {unit}", fmt_num(v)) };
                let summary = if detail.is_empty() { summary } else { format!("{summary}: {}", detail.chars().take(200).collect::<String>()) };
                let severity = Some(row.s("severity")).filter(|s| ["critical", "major", "minor"].contains(&s.as_str())).unwrap_or_else(|| r.s("severity"));
                let a = json!({"id": format!("{key}:{}", m / 60), "rule": rid, "rule_name": name, "severity": severity, "status": "firing", "entity_kind": kind, "entity": entity,
                    "site": sites.first().cloned().unwrap_or_default(), "devices": devices, "summary": summary, "reading": v, "threshold": r.f("threshold"), "unit": unit,
                    "started": iso(m), "last_seen": iso(m), "incident": "", "acked_by": "", "assignee": "", "silenced": ""});
                if let Value::Object(mut map) = a {
                    let mut st = self.st();
                    let by = crate::daily::silence_for(&st.silences, &map, m);
                    if !by.is_empty() {
                        map.insert("status".into(), json!("silenced"));
                        map.insert("silenced".into(), json!(by));
                    }
                    st.alerts.insert(key, map.clone());
                    alert_rows.push(Value::Object(map));
                }
            }
            r.insert("last_eval".into(), json!(iso(m)));
            r.insert("last_ms".into(), json!(telemetry::round(ms, 2)));
            r.insert("last_rows".into(), json!(n));
            r.insert("firing".into(), json!(firing));
            rule_rows.push(Value::Object(r));
        }
        // What stopped matching clears after two evaluations without it.
        {
            let mut st = self.st();
            let gone: Vec<String> = st.alerts.keys().filter(|k| !seen.contains(*k) && managed.contains(k.split(':').next().unwrap_or(""))).cloned().collect();
            for key in gone {
                let n = st.misses.entry(key.clone()).or_default();
                *n += 1;
                if *n >= 2 {
                    st.misses.remove(&key);
                    if let Some(mut a) = st.alerts.remove(&key) {
                        a.insert("status".into(), json!("resolved"));
                        a.insert("ended".into(), json!(iso(m)));
                        alert_rows.push(Value::Object(a));
                    }
                }
            }
        }
        self.db.upsert("netops_alerts", alert_rows).await?;
        self.db.upsert("netops_alert_rules", rule_rows).await?;
        self.st().eval_ms = t_all.elapsed().as_secs_f64() * 1000.0;
        Ok(())
    }

    // ---- incidents: alerts grouped over the topology

    fn attaches(&self, alert: &Row, devices: &HashSet<String>, sites: &HashSet<String>) -> bool {
        let (devs, alert_sites) = self.scope(&alert.s("entity_kind"), &alert.s("entity"));
        devs.iter().any(|d| devices.contains(d)) || alert_sites.iter().any(|s| sites.contains(s))
    }

    async fn correlate(&self, m: i64) -> Result<()> {
        let (mut unassigned, open): (Vec<(String, Row)>, Vec<(String, Row)>) = {
            let st = self.st();
            (st.alerts.iter().filter(|(_, a)| a.s("incident").is_empty() && a.s("status") != "silenced").map(|(k, a)| (k.clone(), a.clone())).collect(), st.open.iter().map(|(k, o)| (k.clone(), o.row.clone())).collect())
        };
        if unassigned.is_empty() {
            return Ok(());
        }
        // Alerts next to an open incident join it: within a hop of its
        // devices, or at one of its sites.
        for (id, row) in open {
            let ball = self.graph.ball(&row.list("devices"), 1);
            let sites: HashSet<String> = row.list("sites").into_iter().collect();
            let (join, rest): (Vec<_>, Vec<_>) = unassigned.into_iter().partition(|(_, a)| self.attaches(a, &ball, &sites));
            unassigned = rest;
            if !join.is_empty() {
                self.assign(&id, &join, m).await?;
            }
        }
        // The rest open an incident when one of them is serious: a critical
        // or major alert on the network, or a service that has been failing
        // for three minutes with nothing else to explain it.
        loop {
            let seed = unassigned.iter().position(|(_, a)| {
                let sev = a.s("severity");
                if a.s("entity_kind") == "service" { m - parse_iso(&a.s("started")) >= 180 } else { sev == "critical" || sev == "major" }
            });
            let Some(si) = seed else { break };
            let mut members = vec![unassigned.remove(si)];
            let (mut devices, mut sites): (HashSet<String>, HashSet<String>) = (HashSet::new(), HashSet::new());
            let grow = |a: &Row, devices: &mut HashSet<String>, sites: &mut HashSet<String>| {
                if a.s("entity_kind") != "service" {
                    let (d, s) = self.scope(&a.s("entity_kind"), &a.s("entity"));
                    devices.extend(d);
                    sites.extend(s);
                } else if devices.is_empty() {
                    sites.extend(self.scope("service", &a.s("entity")).1);
                }
            };
            grow(&members[0].1, &mut devices, &mut sites);
            loop {
                let ball = self.graph.ball(&devices.iter().cloned().collect::<Vec<_>>(), 1);
                let Some(i) = unassigned.iter().position(|(_, a)| self.attaches(a, &ball, &sites)) else { break };
                let a = unassigned.remove(i);
                grow(&a.1, &mut devices, &mut sites);
                members.push(a);
            }
            self.open_incident(members, m).await?;
        }
        Ok(())
    }

    /// Adds alerts to an incident and widens its scope by theirs.
    async fn assign(&self, incident: &str, alerts: &[(String, Row)], m: i64) -> Result<()> {
        let mut rows = Vec::new();
        let inc = {
            let mut st = self.st();
            for (key, _) in alerts {
                if let Some(a) = st.alerts.get_mut(key) {
                    a.insert("incident".into(), json!(incident));
                    rows.push(Value::Object(a.clone()));
                }
            }
            let Some(o) = st.open.get_mut(incident) else { return Ok(()) };
            let mut devices: Vec<String> = o.row.list("devices");
            let mut sites: Vec<String> = o.row.list("sites");
            let mut services: Vec<String> = o.row.list("services");
            for (_, a) in alerts {
                if a.s("entity_kind") == "service" {
                    services.push(a.s("entity"));
                } else {
                    let (d, s) = self.scope(&a.s("entity_kind"), &a.s("entity"));
                    devices.extend(d);
                    sites.extend(s);
                }
            }
            for v in [&mut devices, &mut sites, &mut services] {
                v.sort();
                v.dedup();
            }
            let nums: Vec<i64> = devices.iter().filter_map(|d| self.w.device(d).map(|x| x.num)).collect();
            o.row.insert("devices".into(), json!(devices));
            o.row.insert("device_nums".into(), json!(nums));
            o.row.insert("sites".into(), json!(sites));
            o.row.insert("services".into(), json!(services));
            o.row.insert("alert_count".into(), json!(o.row.i("alert_count") + alerts.len() as i64));
            o.quiet_since = None;
            o.row.clone()
        };
        self.db.upsert("netops_alerts", rows).await?;
        self.db.upsert("netops_incidents", vec![Value::Object(inc)]).await?;
        let _ = m;
        Ok(())
    }

    async fn open_incident(&self, members: Vec<(String, Row)>, m: i64) -> Result<()> {
        let id = {
            let mut st = self.st();
            st.seq += 1;
            format!("INC-{}", st.seq)
        };
        let started = members.iter().map(|(_, a)| parse_iso(&a.s("started"))).min().unwrap_or(m);
        let first = &members[0].1;
        let row = json!({
            "id": id, "title": format!("Investigating: {}", first.s("summary")), "severity": "sev2", "status": "investigating", "started": iso(started), "detected": iso(m),
            "commander": "", "summary": "", "root_kind": "", "root_entity": "", "root_cause": "", "confidence": 0.0, "category": "", "devices": [], "device_nums": [], "sites": [], "services": [],
            "customers": 0, "revenue_at_risk": 0.0, "alert_count": 0, "change_id": "", "runbook": "", "postmortem": "", "origin": "live", "fault_id": "", "mttr_min": 0.0, "ttd_s": 0.0,
        });
        let Value::Object(map) = row else { unreachable!() };
        self.st().open.insert(id.clone(), Open { row: map, ..Default::default() });
        self.assign(&id, &members, m).await?;
        self.timeline(&id, m, "alert", "Alert rules", &format!("{} alert{} grouped into this incident; the first was: {}", members.len(), if members.len() == 1 { "" } else { "s" }, first.s("summary")), &first.s("id")).await?;
        tracing::info!("{id} opened with {} alerts", members.len());
        self.link_fault(&id, m);
        if self.is_estate(&id) {
            return crate::estate::diagnose(self, &id, m).await;
        }
        agents::triage(self, &id, m).await?;
        self.investigate(&id, m).await
    }

    /// Whether an open incident is the estate's: every alert in it is on an
    /// object behind the backbone, so the backbone's playbooks do not apply.
    pub fn is_estate(&self, incident: &str) -> bool {
        let st = self.st();
        let mut alerts = st.alerts.values().filter(|a| a.s("incident") == incident).peekable();
        let kind = st.open.get(incident).map(|o| o.row.s("root_kind")).unwrap_or_default();
        kind == "estate" || (alerts.peek().is_some() && alerts.all(|a| a.s("entity_kind") == "estate"))
    }

    /// Ties an incident to the fault the simulator injected, for the answer
    /// key only: no agent is given it.
    fn link_fault(&self, incident: &str, m: i64) {
        let mut st = self.st();
        let Some(o) = st.open.get(incident) else { return };
        let devices: HashSet<String> = o.row.list("devices").into_iter().collect();
        let sites: HashSet<String> = o.row.list("sites").into_iter().collect();
        let w = &self.w;
        let Some(f) = st.faults.iter().find(|f| f.origin == "live" && f.incident.is_none() && f.active_at(m) && (seed::scope(w, f).iter().any(|d| devices.contains(d)) || f.site.as_ref().is_some_and(|s| sites.contains(s)))) else { return };
        let (fid, started) = (f.id.clone(), f.started);
        if let Some(f) = st.faults.iter_mut().find(|f| f.id == fid) {
            f.incident = Some(incident.to_string());
        }
        if let Some(o) = st.open.get_mut(incident) {
            o.row.insert("fault_id".into(), json!(fid));
            o.row.insert("ttd_s".into(), json!(m - started));
        }
    }

    /// Runs the root-cause agent and marks its answer against the key.
    pub async fn investigate(&self, incident: &str, m: i64) -> Result<()> {
        if self.is_estate(incident) {
            return crate::estate::diagnose(self, incident, m).await;
        }
        let run = agents::root_cause(self, incident, m).await?;
        let (truth, alerts) = {
            let mut st = self.st();
            let alerts = st.alerts.values().filter(|a| a.s("incident") == incident).count();
            let fid = st.open.get(incident).map(|o| o.row.s("fault_id")).unwrap_or_default();
            if let Some(o) = st.open.get_mut(incident) {
                o.rca_at.push(m);
                o.alerts_at_rca = alerts;
            }
            (st.faults.iter().find(|f| f.id == fid).map(|f| (f.truth_kind.clone(), f.truth_entity.clone(), f.clone())), alerts)
        };
        let _ = alerts;
        if let (Some((kind, entity, f)), Some((run_id, got_kind, got_entity))) = (truth, run) {
            let verdict = if got_kind.is_empty() { "no answer" } else if got_kind == kind && got_entity == entity { "correct" } else { "wrong" };
            self.db.sql(&format!("UPDATE netops_agent_runs SET verdict = '{verdict}' WHERE id = '{run_id}'")).await?;
            self.db.upsert("netops_faults", vec![fault_row(&f, m)]).await?;
        }
        self.save_incident(incident).await
    }

    pub async fn save_incident(&self, incident: &str) -> Result<()> {
        let row = self.st().open.get(incident).map(|o| o.row.clone());
        if let Some(row) = row {
            self.db.upsert("netops_incidents", vec![Value::Object(row)]).await?;
        }
        Ok(())
    }

    async fn lifecycle(&self, m: i64) -> Result<()> {
        let ids: Vec<String> = self.st().open.keys().cloned().collect();
        for id in ids {
            let (firing, quiet, again, autopilot) = {
                let mut st = self.st();
                let firing = st.alerts.values().filter(|a| a.s("incident") == id).count();
                let autopilot = st.autopilot;
                let Some(o) = st.open.get_mut(&id) else { continue };
                if firing == 0 {
                    o.quiet_since.get_or_insert(m);
                } else {
                    o.quiet_since = None;
                }
                // More evidence since the agent last looked, and it was not sure.
                let again = firing > o.alerts_at_rca && o.row.f("confidence") < 0.8 && o.rca_at.len() < 4 && o.rca_at.last().is_some_and(|t| m - t >= 120);
                (firing, o.quiet_since, again, autopilot)
            };
            if firing == 0 && quiet.is_some_and(|q| m - q >= 120) {
                self.resolve(&id, m, "Alert rules", "Every alert has been clear for two minutes").await?;
                continue;
            }
            if again {
                self.investigate(&id, m).await?;
            }
            if autopilot {
                let due = self.db.query("SELECT id FROM netops_actions WHERE incident = :i AND status = 'proposed' AND risk = 'low' AND kind != 'comms' AND proposed <= :t LIMIT 5", json!({"i": id, "t": iso(m - 60)})).await?;
                for a in due {
                    self.apply_action_locked(&a.s("id"), "Autopilot", true).await?;
                }
            }
        }
        Ok(())
    }

    pub async fn resolve(&self, incident: &str, m: i64, actor: &str, why: &str) -> Result<()> {
        let Some(mut o) = self.st().open.remove(incident) else { return Ok(()) };
        let started = parse_iso(&o.row.s("started"));
        let mitigated = match o.row.get("mitigated").and_then(|v| v.as_str()) {
            Some(s) => parse_iso(s),
            None => {
                o.row.insert("mitigated".into(), json!(iso(m)));
                m
            }
        };
        o.row.insert("status".into(), json!("resolved"));
        o.row.insert("resolved".into(), json!(iso(m)));
        o.row.insert("mttr_min".into(), json!(telemetry::round((mitigated - started) as f64 / 60.0, 1)));
        o.row.insert("postmortem".into(), json!(agents::postmortem(&o.row, m)));
        self.db.upsert("netops_incidents", vec![Value::Object(o.row.clone())]).await?;
        self.timeline(incident, m, "status", actor, &format!("Resolved: {why}. A postmortem draft was written."), "").await?;
        self.db.sql(&format!("UPDATE netops_actions SET status = 'expired' WHERE incident = '{incident}' AND status = 'proposed'")).await.ok();
        if let Some(who) = &o.applied_by {
            let runbook = o.row.s("runbook");
            if !runbook.is_empty() {
                self.judge(incident, &runbook, 3, who, &o.row.s("title"), m).await?;
            }
        }
        Ok(())
    }

    /// Records that a runbook closed an incident: the grade a ranking learns from.
    pub async fn judge(&self, incident: &str, runbook: &str, grade: i64, who: &str, search: &str, m: i64) -> Result<()> {
        self.db.upsert("netops_judgments", vec![json!({"id": format!("{incident}:{runbook}"), "incident": incident, "runbook": runbook, "grade": grade, "ts": iso(m), "judge": who, "search": search})]).await?;
        Ok(())
    }

    // ---- actions: proposed by agents, approved by a person

    pub async fn apply_action(&self, action: &str, who: &str, approve: bool) -> Result<Value> {
        let _guard = self.lock.lock().await;
        self.apply_action_locked(action, who, approve).await
    }

    async fn apply_action_locked(&self, action: &str, who: &str, approve: bool) -> Result<Value> {
        let rows = self.db.query("SELECT * FROM netops_actions WHERE id = :id", json!({"id": action})).await?;
        let Some(mut a) = rows.into_iter().next() else { return Err(Error { status: 404, message: "no such action".into() }) };
        if a.s("status") != "proposed" {
            return Err(Error { status: 409, message: format!("this action is already {}", a.s("status")) });
        }
        let m = self.st().last_minute;
        let (kind, target, incident) = (a.s("kind"), a.s("target"), a.s("incident"));
        let mut result = "Rejected".to_string();
        if approve {
            let fault = self.st().faults.iter().find(|f| f.incident.as_deref() == Some(incident.as_str()) && f.ended.is_none()).cloned();
            let fixes = fault.as_ref().is_some_and(|f| f.mitigated.is_none() && match f.scenario.as_str() {
                "fiber_cut" => kind == "reroute" && f.link.as_deref() == Some(target.as_str()),
                "optic_degrade" => kind == "drain" && f.link.as_deref() == Some(target.as_str()),
                "bad_change" => kind == "rollback" && f.change.as_deref() == Some(target.as_str()),
                "ddos" => kind == "flowspec" && f.site.as_deref() == Some(target.as_str()),
                "cpu_bug" => kind == "restart_routing" && f.device.as_deref() == Some(target.as_str()),
                "power" => kind == "dispatch" && f.site.as_deref() == Some(target.as_str()),
                _ => false,
            });
            result = match kind.as_str() {
                "ticket" => {
                    let id = format!("TKT-{}", 80000 + self.next_id() + (m as u64 / 60) % 10000);
                    self.db.upsert("netops_tickets", vec![json!({"id": id, "title": a.s("title"), "body": a.s("detail"), "status": "open", "priority": "P2", "assignee": who, "queue": "NOC", "incident": incident,
                        "device": "", "customer": "", "opened": iso(m), "updated": iso(m), "resolution": "", "runbook": ""})]).await?;
                    format!("Opened ticket {id}")
                }
                "comms" => "Posted to the status page (simulated)".to_string(),
                "dispatch" if fixes => {
                    let mut st = self.st();
                    if let Some(f) = st.faults.iter_mut().find(|f| f.incident.as_deref() == Some(incident.as_str()) && f.ended.is_none()) {
                        f.auto_mitigate_after = (m - f.started) + 120;
                    }
                    "Facilities paged (simulated); the feed is expected back in about two minutes".to_string()
                }
                _ if fixes => {
                    let f = fault.expect("a fault");
                    self.mitigate(&f.id, m, who).await?;
                    if let Some(o) = self.st().open.get_mut(&incident) {
                        o.applied_by = Some(who.to_string());
                    }
                    "Executed (simulated). The alerts should clear within about three minutes".to_string()
                }
                _ if !self.opts.simulate => "Approved and recorded. This app pushes nothing to a device: carry it out, and the alerts will show whether it worked".to_string(),
                _ => "Executed (simulated). Nothing changed in the alerts: the cause is elsewhere".to_string(),
            };
            if kind == "dispatch" {
                if let Some(o) = self.st().open.get_mut(&incident) {
                    o.applied_by = Some(who.to_string());
                }
            }
        }
        a.insert("status".into(), json!(if approve { "executed" } else { "rejected" }));
        a.insert("decided".into(), json!(iso(now())));
        a.insert("decided_by".into(), json!(who));
        a.insert("result".into(), json!(result));
        self.db.upsert("netops_actions", vec![Value::Object(a.clone())]).await?;
        if !incident.is_empty() {
            self.timeline(&incident, now(), "action", who, &format!("{} \"{}\": {}", if approve { "Approved" } else { "Rejected" }, a.s("title"), result), action).await?;
        }
        Ok(Value::Object(a))
    }

    // ---- what a person asks of the simulator

    /// Begins a fault now. `seed` fixes which device or link it lands on (tests
    /// use it; without one the minute decides).
    pub async fn inject(&self, scenario: &str, seed: Option<u64>) -> Result<Value> {
        if !self.opts.simulate {
            return Err(bad("the simulator is off"));
        }
        if !telemetry::SCENARIOS.iter().any(|s| s.0 == scenario) {
            return Err(bad(format!("no such scenario: {scenario}")));
        }
        let _guard = self.lock.lock().await;
        if self.st().faults.iter().any(|f| f.origin == "live" && f.ended.is_none()) {
            return Err(Error { status: 409, message: "a fault is already playing out: one at a time, so its cause can be told apart".into() });
        }
        let m = self.st().last_minute;
        let f = self.start_fault(scenario, m, true, seed).await?;
        self.after_minute(m).await?;
        Ok(json!({"fault": f.id, "scenario": f.scenario, "started": iso(f.started)}))
    }

    /// Ends the fault that is playing out, as if it had been repaired now.
    pub async fn end_fault(&self) -> Result<Value> {
        let _guard = self.lock.lock().await;
        let m = self.st().last_minute;
        let id = {
            let mut st = self.st();
            let Some(f) = st.faults.iter_mut().find(|f| f.origin == "live" && f.ended.is_none()) else { return Err(bad("no fault is playing out")) };
            f.mitigated.get_or_insert(m);
            f.ended = Some(m);
            f.id.clone()
        };
        self.write_minute(m).await?;
        self.repaired(&id, m).await?;
        self.sync_status(m).await?;
        self.persist().await?;
        Ok(json!({"ended": id}))
    }

    pub async fn settings(&self, autopilot: Option<bool>, fault_every: Option<i64>) -> Result<()> {
        {
            let mut st = self.st();
            if let Some(a) = autopilot {
                st.autopilot = a;
            }
            if let Some(n) = fault_every {
                st.fault_every = n.clamp(0, 24 * 60);
                st.next_fault_at = st.last_minute + st.fault_every * 60;
            }
        }
        self.persist().await
    }

    async fn sources(&self, m: i64) -> Result<()> {
        // Which sources are arriving, read from the tables themselves: a
        // table the simulator does not write, or any table with it off, is
        // live when rows landed in the last five minutes (telemetry) or it
        // holds rows at all (records).
        let mut seen: HashMap<String, (String, f64, i64)> = HashMap::new();
        let ours = self.st().last_rows.clone();
        let tables: Vec<String> = self.st().sources.iter().map(|s| s["table_name"].as_str().unwrap_or("").split(',').next().unwrap_or("").trim().to_string()).collect();
        for t in tables.iter().filter(|t| !t.is_empty() && (!self.opts.simulate || !ours.contains_key(*t))) {
            if schema::TELEMETRY.contains(&t.as_str()) {
                if let Ok(r) = self.db.query(&format!("SELECT COUNT(*) AS n, MAX(ts) AS last FROM {t} WHERE ts >= :since"), json!({"since": iso(m - 300)})).await {
                    let (n, last) = r.first().map(|r| (r.f("n"), r.s("last"))).unwrap_or((0.0, String::new()));
                    seen.insert(t.clone(), (if n > 0.0 { "live" } else { "silent" }.to_string(), n / 5.0, if last.is_empty() { 0 } else { parse_iso(&last) }));
                }
            } else if !self.opts.simulate {
                if let Ok(r) = self.db.query(&format!("SELECT COUNT(*) AS n FROM {t}"), json!({})).await {
                    let n = r.first().map(|r| r.f("n")).unwrap_or(0.0);
                    seen.insert(t.clone(), (if n > 0.0 { "live" } else { "empty" }.to_string(), n, m));
                }
            }
        }
        let rows: Vec<Value> = {
            let mut st = self.st();
            let last = st.last_rows.clone();
            for s in st.sources.iter_mut() {
                let table = s["table_name"].as_str().unwrap_or("").to_string();
                let first = table.split(',').next().unwrap_or("").trim().to_string();
                if let Some((status, rate, at)) = seen.get(&first) {
                    s["status"] = json!(status);
                    s["rate"] = json!(rate);
                    if *at > 0 {
                        s["last_seen"] = json!(iso(*at));
                        s["lag_s"] = json!((now() - at).max(0));
                    }
                } else if let Some(n) = last.get(&table) {
                    s["rate"] = json!(*n as f64);
                    s["last_seen"] = json!(iso(m));
                    s["lag_s"] = json!((now() - m).max(0));
                }
            }
            st.sources.clone()
        };
        self.db.upsert("netops_sources", rows).await?;
        Ok(())
    }
}

/// The comparisons a rule may make of its reading `v` with its threshold.
pub const COMPARATORS: &[&str] = &[">", ">=", "<", "<=", "=", "!="];

/// Whether a row a rule's query returned fires: its `v` compared with the
/// rule's threshold by the rule's comparator. A row without a number in
/// `v`, or a rule without a comparator, fires as every row did before
/// rules had one.
pub fn fires(cmp: &str, threshold: f64, row: &Row) -> bool {
    let Some(v) = row.get("v").and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))) else { return true };
    match cmp.trim() {
        ">" => v > threshold,
        ">=" => v >= threshold,
        "<" => v < threshold,
        "<=" => v <= threshold,
        "=" | "==" => v == threshold,
        "!=" | "<>" => v != threshold,
        _ => true,
    }
}

fn fmt_num(v: f64) -> String {
    if v.abs() >= 1000.0 {
        format!("{:.0}", v)
    } else if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

/// A fault as the answer key holds it.
pub fn fault_row(f: &Fault, at: i64) -> Value {
    json!({"id": f.id, "scenario": f.scenario, "started": iso(f.started), "ended": f.ended.map(iso), "phase": f.phase(at), "truth_kind": f.truth_kind, "truth_entity": f.truth_entity,
        "category": f.category, "incident": f.incident.clone().unwrap_or_default(), "origin": f.origin, "detail": serde_json::to_string(f).unwrap_or_default()})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: Value) -> Row {
        json!({"entity": "cr1-lon", "v": v}).as_object().cloned().unwrap()
    }

    #[test]
    fn a_rule_fires_only_where_its_reading_passes_its_threshold() {
        // The auditor's rule: v is 1, and "< 0" must not fire.
        assert!(!fires("<", 0.0, &row(json!(1))));
        assert!(fires(">", 0.0, &row(json!(1))));
        assert!(fires(">=", 1.0, &row(json!(1))) && !fires(">", 1.0, &row(json!(1))));
        assert!(fires("<=", 1.0, &row(json!(1))) && fires("=", 1.0, &row(json!(1.0))) && !fires("!=", 1.0, &row(json!(1))));
        // The built-in rules' readings pass their own thresholds.
        assert!(fires("<", 1.0, &row(json!(0))), "link-down: MIN(up) = 0");
        assert!(fires(">", 92.0, &row(json!(95.5))), "link-saturated");
        assert!(fires("<", -10.0, &row(json!(-14.2))), "optic-rx-low");
        // A row without a number in v, or a rule without a comparator, fires as before.
        assert!(fires("<", 0.0, &json!({"entity": "x"}).as_object().cloned().unwrap()));
        assert!(fires("", 100.0, &row(json!(1))));
        assert!(fires(">", 1.0, &row(json!("2.5"))), "a number the server sent as text");
    }

    #[test]
    fn every_built_in_rule_has_a_comparator_the_evaluator_knows() {
        for r in seed::rule_rows() {
            assert!(COMPARATORS.contains(&r["cmp"].as_str().unwrap()), "{}", r["id"]);
        }
    }
}
