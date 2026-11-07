//! Opening the database, and serving HTTP and gRPC.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use strata::db::Db;
use strata::ingest::Format;
use strata::schema::Schema;
use strata::segment::build::BuildOptions;
use strata::storage::{DbOptions, Layout, MergePolicy, MergerOptions};
use tokio::net::TcpListener;

use crate::auth::{self, Access, Secret};
use crate::coord::{CoordOptions, Coordinator};
use crate::cores::shard::{self, Restart};
use crate::replication::{self, Acks, Follow, Primary, PrimaryOptions};
use crate::service::{shard_of, AgentConfig, Backend, Replication, Service};
use crate::alerts::Alerts;
use crate::keys::{Audit, Keys};
use crate::{demo, grpc, http, pb};

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The database to serve; created if missing (with `schema` or `demo`).
    pub db: Option<PathBuf>,
    /// A JSON schema for a new database, in either form ([`Schema::from_json`]).
    pub schema: Option<PathBuf>,
    /// Fill a new database with the rows of this one ([`Db::load_from`]).
    pub copy_from: Option<PathBuf>,
    /// Create the demo catalogue with this many products.
    pub demo: Option<usize>,
    /// Keep only the demo products of shard `i` of `n`.
    pub demo_shard: Option<(usize, usize)>,
    /// A new database as a directory of segment files.
    pub directory: bool,
    /// Open a database whose WAL was damaged after it was durable
    /// ([`DbOptions::wal_drop_damaged`]).
    pub wal_drop_damaged: bool,
    /// Check every segment's checksum on opening
    /// ([`DbOptions::verify_checksums`]).
    pub verify_checksums: bool,
    /// Number the database's writes ([`strata::storage::Database::start_seq_no`]),
    /// for `seq_no()` and conditional writes (`if_seq_no`); once started,
    /// for good.
    pub seq_no: bool,
    /// Ship the database's writes to these replicas (gRPC URLs of servers
    /// started with `replica`), as their primary
    /// ([`crate::replication`]); the replicas' token and CA are
    /// `coord.token` and `coord.ca`. With `replica`: the shard's other
    /// copies, which it ships to once promoted, and the only ones a
    /// promotion asked for with a key may name.
    pub replicas: Vec<String>,
    /// Apply the writes a primary ships, and take none from clients.
    pub replica: bool,
    /// Start as the primary of a new term, shipping to `replicas` (none:
    /// to no replica), though the database was a replica or another
    /// primary replaced it ([`PrimaryOptions::promote`]).
    pub promote: bool,
    /// With `replicas`: which copies hold a write before it is answered.
    pub write_acks: Acks,
    /// On a primary or a replica: the latest transactions kept to ship, in
    /// MiB (0: 64).
    pub ship_log_mb: usize,
    /// With `replicas`: how often each replica's rows are compared with
    /// the primary's, in seconds (0: only when asked, `POST
    /// /v1/admin/verify`).
    pub verify_every_s: u64,
    /// On a primary or a replica: how long a replica hears nothing from
    /// its primary before it asks its shard's copies to elect it, in
    /// milliseconds (0: never; [`Service::start_failover`]).
    pub failover_after_ms: u64,
    /// Coordinate these shard servers (gRPC URLs; replicas of a shard
    /// joined by `|`) instead of serving a database.
    pub shards: Vec<String>,
    pub coord: CoordOptions,
    /// With `shards`: the `StrataCluster` (`namespace/name`) that runs this
    /// coordinator on Kubernetes, which the command center shows and scales.
    pub kube_cluster: Option<String>,
    pub timeout_ms: u64,
    /// The longest deadline a request may ask for (0: the larger of
    /// `timeout_ms` and 60 s).
    pub max_timeout_ms: u64,
    pub max_concurrent: usize,
    /// The most rows a query may reach, OFFSET included (0: no limit).
    pub max_result_window: usize,
    /// The most groups a query may make (0: no limit).
    pub max_groups: usize,
    /// The most rows a cross-encoder's rerank phase may score (0: no
    /// limit).
    pub max_rerank: usize,
    /// Memory the running queries may hold together, in MiB (`None`: a
    /// third of [`strata::storage::memory_limit`]; 0: no limit).
    pub query_memory_mb: Option<u64>,
    /// Memory for filters kept between queries, in MiB (`None`:
    /// [`strata::query::exec::FILTER_CACHE_BYTES`]; 0: none kept).
    pub filter_cache_mb: Option<usize>,
    /// Write requests held at once before more are refused (0: no limit).
    pub max_queued_writes: usize,
    /// The largest write or import body, in MiB (0: 256).
    pub max_body_mb: usize,
    pub model: String,
    pub api_key: Option<String>,
    pub api_base: String,
    /// Where embedded fields' models are ([`strata::embed::open`]).
    pub models: PathBuf,
    /// Fill in the vectors of rows stored while their embedded field had
    /// no model open, once it has one ([`Service::keep_models`]).
    pub backfill: bool,
    /// Fetch the models Strata knows ([`strata::embed::KNOWN_MODELS`]) that
    /// an embedded field names and `models` lacks.
    pub fetch_models: bool,
    /// The synonym list for `synonyms => 1` searches (Solr format), read at
    /// start if it exists and written by `PUT /v1/synonyms`.
    pub synonyms: Option<PathBuf>,
    /// Where `POST /v1/admin/checkpoint` writes checkpoints.
    pub backup_dir: Option<PathBuf>,
    /// What writes may fetch for MEDIA fields: `file:` roots and hosts
    /// ([`strata::media::Allowlist::parse`]).
    pub media_fetch: Vec<String>,
    /// The largest media file a write fetches or gives, in bytes (0:
    /// [`strata::media::DEFAULT_MAX_BYTES`]).
    pub media_max_bytes: u64,
    /// Where bytes that writes give a MEDIA field without a URI are kept.
    pub media_store: Option<PathBuf>,
    /// How long a media fetch may take, in seconds (0:
    /// [`crate::media::DEFAULT_TIMEOUT`]).
    pub media_timeout_s: u64,
    /// The background merger ([`strata::storage::Database::spawn_merger`]).
    pub merger: MergerOptions,
    /// Log queries and writes that take this many milliseconds or more
    /// (0: none).
    pub slow_ms: u64,
    /// The API keys' file (default: `<db>.keys.json` beside a database; a
    /// coordinator's keys are in memory without it).
    pub keys: Option<PathBuf>,
    /// Append every audited request here, as JSON lines.
    pub audit_log: Option<PathBuf>,
    /// The cloud view's AWS credentials file (default: `<db>.aws.json` beside a
    /// database; a coordinator keeps them in memory without it).
    pub aws_file: Option<PathBuf>,
    /// The cloud view's DigitalOcean token file (default:
    /// `<db>.digitalocean.json` beside a database; in memory on a
    /// coordinator without it).
    pub digitalocean_file: Option<PathBuf>,
    /// The clusters the cloud view made (default: `<db>.cloud.json` beside
    /// a database; in memory on a coordinator without it).
    pub cloud_file: Option<PathBuf>,
    /// The hub's book of connections to other clusters ([`crate::hub`]).
    pub hub_file: Option<PathBuf>,
    /// Who this hub's calls to other clusters say they come from.
    pub hub_device: Option<String>,
    /// The alert rules' file (default rules, in memory, without it).
    pub alerts: Option<PathBuf>,
    /// A coordinator's Contain rules' file (in memory without it): fleet.
    pub contain: Option<PathBuf>,
    /// Post alerts that fire or resolve to this URL.
    pub alert_webhook: Option<String>,
    /// The log store ([`crate::logstore`]): `None` beside the database
    /// (`<db>.logs`), and none on a coordinator; `Some(None)` none.
    pub log_store: Option<Option<PathBuf>>,
    /// The name this server's rows carry in log stores (empty: the host's
    /// name and the HTTP port, [`crate::logstore::hostname`]).
    pub node_name: String,
    /// Log store rows kept this long (zero: 7 days).
    pub log_retain: Duration,
    /// The most bytes the log store takes (0: no limit).
    pub log_store_bytes: u64,
    /// Queries a second the log store keeps besides slow and failed ones.
    pub log_queries_per_sec: u64,
    // ----- core (command center rebuild)
    /// The cluster's name and environment, as people call them, and where
    /// its command center is, for links (`--cluster-name`,
    /// `--environment`, `--public-url`; a coordinator run by the operator
    /// is named by its StrataCluster without one).
    pub cluster_name: Option<String>,
    pub environment: Option<String>,
    pub public_url: Option<String>,
    /// The service levels fleet health checks (`None`: 1% of queries
    /// failing, a p99 of 250 ms).
    pub slo_error_ratio: Option<f64>,
    pub slo_p99_ms: Option<f64>,
    /// Proxies whose `X-Forwarded-For` names the client.
    pub trust_proxy: Vec<String>,
    /// `--node-name`, when given (`node_name` is always set).
    pub node_name_given: Option<String>,
    /// `--image-repository`.
    pub image_repository: Option<String>,
}

/// How writes resolve MEDIA values, from `--media-fetch`,
/// `--media-max-bytes`, `--media-store` and `--media-timeout-s`.
fn media_resolver(o: &Options) -> Result<strata::media::Resolver, String> {
    let allow = strata::media::Allowlist::parse(&o.media_fetch).map_err(|e| e.to_string())?;
    if let Some(dir) = &o.media_store {
        std::fs::create_dir_all(dir).map_err(|e| format!("--media-store {}: {e}", dir.display()))?;
    }
    let timeout = if o.media_timeout_s > 0 { Duration::from_secs(o.media_timeout_s) } else { crate::media::DEFAULT_TIMEOUT };
    let max_bytes = if o.media_max_bytes > 0 { o.media_max_bytes } else { strata::media::DEFAULT_MAX_BYTES };
    let http = crate::media::HttpFetcher::new(allow.clone(), timeout)?;
    tracing::info!(fetch = %allow.entries().join(", "), max_bytes, store = ?o.media_store, timeout_s = timeout.as_secs(), "media fields: what writes may fetch");
    Ok(strata::media::Resolver { allow, max_bytes, store: o.media_store.clone(), http: Some(std::sync::Arc::new(http)) })
}

/// Segment builds (flushes, merges, imports): vector indexes on every core.
fn build_options() -> BuildOptions {
    BuildOptions::default().set("vector.threads", std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// How the server opens a database. Each write publishes its snapshot,
/// rebuilding the newest in-memory chunk (up to 1,024 rows): those chunks
/// keep their vectors flat, since a graph over so few rows costs more to
/// build at every write than an exact scan of them costs a query. Segments
/// build their vector indexes on every core.
pub fn db_options(directory: bool) -> DbOptions {
    DbOptions {
        publish_snapshots: true,
        build: build_options(),
        mem_build: Some(BuildOptions::default().set("vector.index", "flat")),
        layout: if directory { Layout::Directory } else { Layout::SingleFile },
        // The writer thread retries a busy write until each request's
        // deadline, and refuses one whose deadline passed: it does not
        // wait inside Strata past it.
        write_wait: Duration::ZERO,
        ..DbOptions::default()
    }
}

/// Opens or creates the database, or connects to the shards.
pub async fn service(o: &Options) -> Result<Arc<Service>, String> {
    let agent = AgentConfig { api_key: o.api_key.clone(), model: o.model.clone(), base_url: o.api_base.clone() };
    let timeout = Duration::from_millis(o.timeout_ms.max(1));
    // What a new database is filled with, once its models are open.
    enum Fill {
        Demo(usize),
        Copy(PathBuf),
    }
    let mut fill = None;
    let mut slots_file = None;
    if o.seq_no && !o.shards.is_empty() {
        return Err("--seq-no numbers a database's writes: start each shard server with it, not the coordinator".into());
    }
    let replicating = !o.replicas.is_empty() || o.replica || o.promote;
    if replicating && !o.shards.is_empty() {
        return Err("--replicas, --replica and --promote are for the servers holding a shard's copies, not the coordinator".into());
    }
    if o.promote && o.replica {
        return Err("a server is a primary (--replicas, --promote) or a replica (--replica), not both".into());
    }
    // A server that numbers its writes keeps a log of them to ship, so that
    // it can become a primary while it serves (`Configure`), and so that
    // rows moving to another shard catch up from it.
    let ship_log_bytes = match replicating || o.seq_no {
        true => (if o.ship_log_mb > 0 { o.ship_log_mb } else { 64 }) << 20,
        false => 0,
    };
    let mut replication_file = None;
    let (backend, schema) = if !o.shards.is_empty() {
        // A shard with no replica answering yet (the cluster is starting, or
        // one is down) is waited for; a wrong URL or schema is not.
        let c = loop {
            match Coordinator::connect_with(o.shards.clone(), o.coord.clone()).await {
                Ok(c) => break c,
                // A shard that refuses the request, or the slot map, will
                // not change its mind.
                Err(e) if e.status != 400 && e.status != 409 => {
                    tracing::warn!(error = %e.message, "waiting for the shards");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => return Err(e.message),
            }
        };
        let schema = c.schema.clone();
        let c = Arc::new(c);
        // Follows the cluster map as admin calls (here or at another
        // coordinator) change it.
        c.watch();
        (Backend::Cluster(c), schema)
    } else {
        let path = o.db.clone().ok_or("pass --db PATH (or --shards to coordinate shard servers)")?;
        slots_file = Some(crate::slots::file_of(&path));
        replication_file = Some(replication::file_of(&path));
        // A part in replication kept beside the database is taken up again
        // (`Configure` set it, or a promotion): such a database numbers
        // its writes, and keeps a log of them.
        let kept = if path.exists() && !replicating { replication::kept_role(&path)?.0 } else { None };
        let ship_log_bytes = if kept.is_some() && ship_log_bytes == 0 { (if o.ship_log_mb > 0 { o.ship_log_mb } else { 64 }) << 20 } else { ship_log_bytes };
        if o.copy_from.is_some() && (path.exists() || o.schema.is_none()) {
            return Err("--copy-from fills a new database: pass a --db that does not exist and its --schema".into());
        }
        let db = if path.exists() {
            let db = Db::open(&path, DbOptions { wal_drop_damaged: o.wal_drop_damaged, verify_checksums: o.verify_checksums, ship_log_bytes, ..db_options(o.directory) })
                .map_err(|e| format!("open {}: {e}", path.display()))?;
            if let Some(what) = db.storage().damaged() {
                tracing::warn!(db = %path.display(), damage = %what, "opened the database past damage");
            }
            db
        } else if let Some(n) = o.demo {
            fill = Some(Fill::Demo(n));
            Db::create_in(&path, demo::schema(n), DbOptions { ship_log_bytes, ..db_options(o.directory) }, &o.models).map_err(|e| e.to_string())?
        } else if let Some(s) = &o.schema {
            let text = std::fs::read_to_string(s).map_err(|e| format!("read {}: {e}", s.display()))?;
            let schema = Schema::from_json(&text).map_err(|e| format!("{}: {e}", s.display()))?;
            fill = o.copy_from.clone().map(Fill::Copy);
            Db::create_in(&path, schema, DbOptions { ship_log_bytes, ..db_options(o.directory) }, &o.models).map_err(|e| e.to_string())?
        } else {
            // No schema: a database with no tables yet, which CREATE TABLE
            // and the imports that create tables fill.
            tracing::info!(db = %path.display(), "creating the database with no tables: create one with CREATE TABLE or an import");
            Db::create_empty(&path, DbOptions { ship_log_bytes, ..db_options(o.directory) }).map_err(|e| e.to_string())?
        };
        for (field, why) in db.open_codebooks(&o.models) {
            tracing::warn!(field = %field, error = %why, "cannot give this field semantic IDs: writes must carry them");
        }
        if o.seq_no {
            db.storage().start_seq_no().map_err(|e| format!("--seq-no on {}: {e}", path.display()))?;
        }
        if replicating && db.storage().seq_no().is_none() {
            return Err(format!("{} does not number its writes, which replication ships by: pass --seq-no", path.display()));
        }
        let mut db = db;
        if let Some(mb) = o.filter_cache_mb {
            db.exec.filter_cache = Some(Arc::new(strata::query::exec::FilterCache::new(mb << 20)));
        }
        let db = Arc::new(db);
        // Writes go on while a full memtable becomes segments, and while
        // segments are merged.
        db.spawn_flusher();
        db.spawn_merger(o.merger.clone());
        if let Some(b) = db.storage().merge_status().memory_budget {
            tracing::info!(threads = o.merger.threads.max(1), memory_mib = b >> 20, "merging in the background");
        }
        let schema = (*db.schema()).clone();
        (Backend::Local(db), schema)
    };
    let mut svc = Service::new(backend, schema, agent, o.max_concurrent.max(1), timeout);
    let given = |v: &Option<String>| v.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(String::from);
    svc.identity = crate::service::Identity {
        name: given(&o.cluster_name).or_else(|| o.kube_cluster.clone().filter(|_| !o.shards.is_empty())),
        environment: given(&o.environment),
        public_url: given(&o.public_url).map(|u| u.trim_end_matches('/').to_string()),
    };
    let d = crate::service::Slo::default();
    svc.slo = crate::service::Slo { error_ratio: o.slo_error_ratio.filter(|r| *r > 0.0).unwrap_or(d.error_ratio), p99_ms: o.slo_p99_ms.filter(|r| *r > 0.0).unwrap_or(d.p99_ms) };
    svc.place = crate::service::Place::from_env(given(&o.node_name_given));
    if let Some(r) = given(&o.image_repository) {
        crate::operator::upgrade::Ref::parse(&r).map_err(|e| format!("--image-repository: {e}"))?;
        svc.image_repository = r;
    }
    svc.trusted_proxies = o.trust_proxy.iter().filter(|p| !p.trim().is_empty()).map(|p| crate::http::parse_net(p)).collect::<Result<_, _>>().map_err(|e| format!("--trust-proxy: {e}"))?;
    if let Some(path) = slots_file {
        svc.keep_slots_in(path)?;
        if let Some((m, shard)) = svc.slots() {
            tracing::info!(epoch = m.epoch, shard, of = m.shards(), "holds slots");
        }
    }
    if let Some(c) = o.kube_cluster.as_deref().filter(|_| !o.shards.is_empty()) {
        match crate::operator::kube::Link::new(c) {
            Ok(l) => {
                tracing::info!(cluster = %c, "run by the Kubernetes operator: the command center shows and scales the StrataCluster");
                svc.kube = Some(l);
            }
            Err(e) => tracing::warn!(cluster = %c, error = %e, "cannot reach the StrataCluster that runs this coordinator"),
        }
    }
    if o.max_timeout_ms > 0 {
        svc.max_timeout = Duration::from_millis(o.max_timeout_ms).max(timeout);
    }
    svc.slow = (o.slow_ms > 0).then(|| Duration::from_millis(o.slow_ms));
    svc.max_rows = (o.max_result_window > 0).then_some(o.max_result_window);
    svc.max_groups = (o.max_groups > 0).then_some(o.max_groups);
    svc.max_rerank = (o.max_rerank > 0).then_some(o.max_rerank);
    let query_memory = match o.query_memory_mb {
        Some(mb) => (mb > 0).then(|| mb.saturating_mul(1 << 20)),
        None => strata::storage::memory_limit().map(|l| l / 3),
    };
    if let Some(b) = query_memory {
        tracing::info!(memory_mib = b >> 20, "queries share a memory budget");
    }
    svc.query_memory = query_memory.map(strata::query::memory::MemoryBudget::new);
    svc.max_queued_writes = if o.max_queued_writes > 0 { o.max_queued_writes } else { usize::MAX };
    if o.max_body_mb > 0 {
        svc.max_body = o.max_body_mb.saturating_mul(1 << 20);
    }
    let aws = o.aws_file.clone().or_else(|| o.db.as_deref().filter(|_| o.shards.is_empty()).map(crate::aws::Aws::file_of));
    if let Some(file) = aws {
        svc.aws = Arc::new(crate::aws::Aws::open(file)?);
    }
    let digitalocean = o.digitalocean_file.clone().or_else(|| o.db.as_deref().filter(|_| o.shards.is_empty()).map(crate::digitalocean::DigitalOcean::file_of));
    if let Some(file) = digitalocean {
        svc.digitalocean = Arc::new(crate::digitalocean::DigitalOcean::open(file)?);
    }
    let cloud = o.cloud_file.clone().or_else(|| o.db.as_deref().filter(|_| o.shards.is_empty()).map(crate::cloud::clusters::Clusters::file_of));
    if let Some(file) = cloud {
        svc.clouds = Arc::new(crate::cloud::clusters::Clusters::open(file)?);
        // Clusters on their way when the server stopped go on.
        if svc.clouds.list().iter().any(|m| !matches!(m.phase, crate::cloud::clusters::Phase::Deleted | crate::cloud::clusters::Phase::Failed)) {
            svc.clouds.start(svc.aws.clone(), svc.digitalocean.clone());
        }
    }
    let hub = o.hub_file.clone().or_else(|| o.db.as_deref().filter(|_| o.shards.is_empty()).map(crate::hub::Hub::file_of));
    if let Some(file) = hub {
        let secrets = crate::hub::Secrets::detect(Some(crate::hub::Hub::keys_file_of(&file)));
        svc.hub = Arc::new(crate::hub::Hub::open(file, secrets, o.hub_device.clone())?);
    }
    let keys = o.keys.clone().or_else(|| o.db.as_deref().filter(|_| o.shards.is_empty()).map(Keys::file_of));
    if let Some(file) = keys {
        svc.keys = Arc::new(Keys::open(file)?);
    }
    svc.audit = Audit::new(o.audit_log.clone())?;
    svc.alerts = Alerts::new(o.alerts.clone(), o.alert_webhook.clone())?;
    svc.alerts.follow_slo(svc.slo.error_ratio, svc.slo.p99_ms);
    // fleet: Contain rules kept across a restart.
    if let Some(f) = &o.contain {
        svc.contain = crate::contain::Contain::open(f.clone())?;
    }
    // The cluster's name in alerts and route tests from the start.
    crate::alerts::identify(&svc);
    (svc.backfill, svc.fetch_models) = (o.backfill, o.fetch_models);
    svc.open_models(&o.models);
    for m in svc.models().iter() {
        match &m.model {
            Ok(e) => tracing::info!(table = %m.table, field = %m.field, model = %e.name(), from = %m.spec.from.join(" + "), "embedded field"),
            Err(why) => tracing::warn!(table = %m.table, field = %m.field, model = %m.spec.model, from = %m.spec.from.join(" + "), error = %why, "cannot run this embedded field's model: writes must supply its vectors"),
        }
    }
    if let (Some(fill), Backend::Local(db)) = (fill, &svc.backend) {
        let t0 = std::time::Instant::now();
        let n = match fill {
            Fill::Demo(n) => {
                let keep = |i: usize| match o.demo_shard {
                    Some((s, of)) => shard_of(&strata::Value::Str(format!("p{i:07}")), of) == s,
                    None => true,
                };
                db.load((0..n).filter(|&i| keep(i)).map(|i| demo::product(i, 7))).map_err(|e| e.to_string())?;
                db.snapshot().n_live_docs()
            }
            Fill::Copy(from) => {
                // A database from before field ids opens to be copied too.
                let old = Db::open(&from, DbOptions { open_old: true, ..DbOptions::default() }).map_err(|e| format!("open {}: {e}", from.display()))?;
                db.load_from(&old).map_err(|e| format!("copy from {}: {e}", from.display()))?
            }
        };
        tracing::info!(rows = n, secs = t0.elapsed().as_secs_f64(), "loaded the new database");
    }
    if let Some(path) = &o.synonyms {
        svc.open_synonyms(path).map_err(|e| e.message)?;
    }
    if let Some(dir) = &o.backup_dir {
        svc.set_backup_dir(dir).map_err(|e| e.message)?;
    }
    if !o.media_fetch.is_empty() || o.media_store.is_some() || o.media_max_bytes > 0 {
        svc.set_media(media_resolver(o)?).map_err(|e| e.message)?;
    }
    // Once loaded: bulk loads are not shipped, so every copy loads the same.
    if let (Backend::Local(_), Some(file)) = (&svc.backend, &replication_file) {
        svc.promote_options = Some(PrimaryOptions { token: o.coord.token.clone(), ca: o.coord.ca.clone(), verify_every: Duration::from_secs(o.verify_every_s), acks: o.write_acks, ..Default::default() });
        svc.failover_after = Duration::from_millis(o.failover_after_ms);
        svc.keep_replication_in(file.clone());
    }
    if let (Backend::Local(db), Some(file)) = (&svc.backend, &replication_file) {
        // How it ships as a primary, but for its replicas and acks.
        let ships = PrimaryOptions { token: o.coord.token.clone(), ca: o.coord.ca.clone(), verify_every: Duration::from_secs(o.verify_every_s), ..Default::default() };
        let ships = crate::service::failover_tuned(ships, Duration::from_millis(o.failover_after_ms));
        let kept = match replicating {
            true => (None, false),
            false => replication::kept_role(db.storage().path())?,
        };
        if kept.0.is_some() && db.storage().seq_no().is_none() {
            return Err(format!("{} keeps a part in replication, but does not number its writes", db.storage().path().display()));
        }
        // With automatic failover, a primary that starts again is a replica
        // until its shard's copies elect one: another may have been
        // elected while it was down.
        let elects = o.failover_after_ms > 0 && svc.slots().is_some_and(|(m, s)| m.replicas().get(s).is_some_and(|c| c.len() > 1));
        let was_primary = matches!(kept.0, Some(replication::KeptRole::Primary { .. })) && !kept.1;
        if let Some(Restart::Primary { replicas, acks }) = shard::restart(kept.0.as_ref(), kept.1, elects) {
            let opts = PrimaryOptions { replicas: replicas.clone(), acks, ..ships };
            let p = Primary::start(db.clone(), Some(file), opts)?;
            tracing::info!(replicas = %replicas.join(", "), write_acks = acks.name(), "primary, as kept beside the database: ships its writes to its replicas");
            svc.set_replication(Replication::Primary(p));
        } else if kept.0.is_some() {
            svc.set_replication(Replication::Replica(Arc::new(Follow::open(db.clone(), Some(file))?)));
            match (was_primary, kept.1) {
                (true, _) => tracing::info!("was its shard's primary: starts as a replica, until its shard's copies elect one (--failover-after-ms)"),
                (false, true) => tracing::info!("replica: a primary of a newer term replaced this one, which follows it"),
                (false, false) => tracing::info!("replica, as kept beside the database: applies the writes its primary ships"),
            }
        } else if (!o.replicas.is_empty() && !o.replica) || o.promote {
            let opts = PrimaryOptions { replicas: o.replicas.clone(), acks: o.write_acks, promote: o.promote, ..ships };
            let p = Primary::start(db.clone(), Some(file), opts)?;
            tracing::info!(replicas = %o.replicas.join(", "), write_acks = o.write_acks.name(), "primary: ships its writes to its replicas");
            svc.set_replication(Replication::Primary(p));
        } else if o.replica {
            svc.set_replication(Replication::Replica(Arc::new(Follow::open_first(db.clone(), Some(file))?)));
            // Its --replicas: the shard's other copies, the ones a
            // promotion ships to (Service::known_copies).
            svc.promote_options = Some(PrimaryOptions { replicas: o.replicas.clone(), acks: o.write_acks, ..ships });
            tracing::info!(copies = %o.replicas.join(", "), "replica: applies the writes its primary ships, and takes none from clients");
        }
    }
    open_log_store(&mut svc, o)?;
    let svc = Arc::new(svc);
    // Clusters made here that were to bring tables when ready, and have not.
    for m in svc.clouds.list() {
        if !m.bring.is_empty() && m.brought.is_none() {
            crate::copy::bring_when_ready(svc.clone(), m.id);
        }
    }
    spawn_maintenance(&svc);
    spawn_expirer(&svc, Duration::from_secs(5));
    spawn_sampler(&svc);
    spawn_watch(&svc);
    // workdata: every table's counts a minute apart (rates on a first look).
    crate::inspect::keep_looking(&svc);
    tokio::spawn(svc.clone().keep_models());
    svc.start_failover();
    // The cluster's keys: a coordinator takes them from its shards before
    // it serves, and a coordinator or primary passes changes on.
    crate::keysync::start(&svc).await;
    Ok(svc)
}

/// Opens the server's log store ([`crate::logstore`]), beside the database
/// unless `o.log_store` says where or that there is none, and hands it the
/// service's queries; a coordinator keeps one only where told. Without one
/// the lines kept for it are let go ([`obs::Sink::stop`]).
fn open_log_store(svc: &mut Service, o: &Options) -> Result<(), String> {
    // A coordinator in a shard server's process: the shard keeps the
    // process's lines.
    if o.coord.local.is_some() {
        return Ok(());
    }
    let path = match &o.log_store {
        Some(p) => p.clone(),
        None => o.db.as_deref().filter(|_| o.shards.is_empty()).map(crate::logstore::file_of),
    };
    let Some(path) = path else {
        crate::obs::sink().stop();
        return Ok(());
    };
    let node = match o.node_name.is_empty() {
        true => crate::logstore::hostname(),
        false => o.node_name.clone(),
    };
    let mut lo = crate::logstore::LogOptions::new(path, node);
    if !o.log_retain.is_zero() {
        lo.retain = o.log_retain;
    }
    lo.max_bytes = o.log_store_bytes;
    lo.queries_per_sec = o.log_queries_per_sec;
    let store = crate::logstore::LogStore::open(lo)?;
    tracing::info!(path = %store.options().path.display(), node = %store.options().node, retain_s = store.options().retain.as_secs(), max_mib = store.options().max_bytes >> 20, "keeps its logs in a log store");
    let keep = store.clone();
    let _ = svc.query_log.keep.set(Box::new(move |q, slow| keep.keep_query(q, slow)));
    let ev = store.clone();
    svc.events.sink(move |r| ev.keep_event(r));
    svc.log_store = Some(store);
    Ok(())
}

/// Asks the replica serving HTTP at `url` to become the primary of a new
/// term, shipping to `replicas` under `write_acks` (`POST
/// /v1/admin/promote`), and answers what it said.
pub async fn promote(url: &str, replicas: &[String], write_acks: &str, force: bool, token: Option<&str>) -> Result<serde_json::Value, String> {
    let mut req = crate::http_client()
        .build()
        .map_err(|e| e.to_string())?
        .post(format!("{}/v1/admin/promote", url.trim_end_matches('/')))
        .json(&serde_json::json!({"replicas": replicas, "write_acks": write_acks, "force": force}));
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        req = req.bearer_auth(t);
    }
    let r = req.send().await.map_err(|e| format!("{url}: {e}"))?;
    let status = r.status();
    let j: serde_json::Value = r.json().await.map_err(|e| format!("{url}: {status}: {e}"))?;
    match status.is_success() {
        true => Ok(j),
        false => Err(format!("{url}: {status}: {}", j["error"].as_str().map_or_else(|| j.to_string(), String::from))),
    }
}

/// Downloads model `name` ([`strata::embed::KNOWN_MODELS`],
/// [`strata::derive::KNOWN_MODELS`], a cross-encoder of
/// [`cross_encoder::KNOWN_MODELS`], or a Hugging Face repository
/// `owner/name` of an embedding model or a cross-encoder) into `dir`;
/// returns its directory.
pub async fn fetch_model(name: &str, dir: &std::path::Path) -> Result<PathBuf, String> {
    use strata::query::cross_encoder;
    let derives = strata::derive::KNOWN_MODELS.iter().any(|m| m.0.eq_ignore_ascii_case(name));
    let known = || strata::embed::KNOWN_MODELS.iter().chain(strata::derive::KNOWN_MODELS).chain(cross_encoder::KNOWN_MODELS);
    let (local, repo) = match known().find(|m| m.0.eq_ignore_ascii_case(name)) {
        Some(m) => (m.0.to_string(), m.1.to_string()),
        None if name.contains('/') => (name.rsplit('/').next().unwrap_or(name).to_string(), name.to_string()),
        None => {
            let names: Vec<&str> = known().map(|m| m.0).collect();
            return Err(format!("unknown model {name}: one of {}, or a Hugging Face repository owner/name", names.join(", ")));
        }
    };
    let out = dir.join(&local);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let client = crate::http_client().build().map_err(|e| e.to_string())?;
    // Each file's path in the repository, and its name here.
    let files: Vec<(&str, &str)> = match (derives, cross_encoder::fetch_files(&local)) {
        (true, _) => strata::derive::model_files(&local).iter().map(|&f| (f, f)).collect(),
        (false, Some(f)) => f,
        (false, None) => strata::embed::model_files(&local).iter().map(|&f| (f, f)).collect(),
    };
    for (from, file) in files {
        let url = format!("https://huggingface.co/{repo}/resolve/main/{from}");
        let r = client.get(&url).send().await.and_then(|r| r.error_for_status()).map_err(|e| format!("{url}: {e}"))?;
        let bytes = r.bytes().await.map_err(|e| format!("{url}: {e}"))?;
        // Written aside and renamed, so that an open model is never
        // overwritten in place.
        let tmp = out.join(format!("{file}.part"));
        std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, out.join(file))).map_err(|e| format!("{}: {e}", out.join(file).display()))?;
        tracing::info!(file, bytes = bytes.len(), "downloaded");
    }
    if derives {
        strata::derive::open(&local, dir).map_err(|e| e.to_string())?;
    } else if !cross_encoder::is_cross_encoder(&out) {
        strata::embed::open(&local, dir).map_err(|e| e.to_string())?;
    } else if let Err(e) = cross_encoder::CrossEncoder::open(&local, &out) {
        let runs = if out.join(cross_encoder::ONNX).exists() { cfg!(feature = "onnx") } else { cfg!(feature = "bert") };
        match runs {
            true => return Err(e.to_string()),
            false => tracing::warn!(error = %e, "downloaded a cross-encoder this build cannot run"),
        }
    }
    Ok(out)
}

/// What `strata-server import` does with SQLite and Parquet files.
#[derive(Default)]
pub struct FileImport {
    /// Every file is a SQLite database (`--format sqlite`); otherwise a
    /// file is one when its first bytes say so.
    pub sqlite: bool,
    /// Every file is Parquet, or a directory of Parquet files (`--format
    /// parquet`); otherwise one is when its name or first bytes say so
    /// ([`strata::parquet::is_parquet`]).
    pub parquet: bool,
    /// SQLite: only these tables.
    pub tables: Vec<String>,
    /// Parquet: the key of a table the import creates
    /// ([`strata::parquet::ImportOptions::key`]).
    pub key: Option<String>,
    /// Binary columns as base64 text ([`strata::sqlite::ImportOptions::blobs_base64`]).
    pub blobs_base64: bool,
    /// Print the plan as JSON, and write nothing.
    pub plan: bool,
}

/// Whether `s` is an `s3://` (or `s3a://`) URL.
pub fn is_url(s: &str) -> bool {
    let l = s.get(..6).unwrap_or(s).to_ascii_lowercase();
    l.starts_with("s3://") || l.starts_with("s3a://")
}

/// A file to read on this machine: the path given, or what an `s3://`
/// URL names, read into a temporary directory removed when this is
/// dropped.
struct Fetched {
    path: PathBuf,
    #[cfg(feature = "s3")]
    _held: Option<strata::objects::Input>,
}

fn fetch(f: &Path) -> Result<Fetched, String> {
    let s = f.to_string_lossy();
    if !is_url(&s) {
        return Ok(Fetched {
            path: f.to_path_buf(),
            #[cfg(feature = "s3")]
            _held: None,
        });
    }
    #[cfg(feature = "s3")]
    {
        let t0 = std::time::Instant::now();
        let i = strata::objects::Input::open(&s).map_err(|e| e.to_string())?;
        tracing::info!(url = %s, secs = t0.elapsed().as_secs_f64(), "fetched");
        Ok(Fetched { path: i.path().to_path_buf(), _held: Some(i) })
    }
    #[cfg(not(feature = "s3"))]
    Err(format!("{s}: this strata-server was built without s3:// URLs (feature s3)"))
}

/// A path to write on this machine, and the URL it then goes to.
struct Upload {
    path: PathBuf,
    #[cfg(feature = "s3")]
    out: Option<strata::objects::Output>,
}

fn upload_to(out: &str) -> Result<Upload, String> {
    if !is_url(out) {
        return Ok(Upload {
            path: PathBuf::from(out),
            #[cfg(feature = "s3")]
            out: None,
        });
    }
    #[cfg(feature = "s3")]
    {
        let o = strata::objects::Output::new(out).map_err(|e| e.to_string())?;
        Ok(Upload { path: o.path().to_path_buf(), out: Some(o) })
    }
    #[cfg(not(feature = "s3"))]
    Err(format!("{out}: this strata-server was built without s3:// URLs (feature s3)"))
}

impl Upload {
    /// Sends what was written to the URL, if there is one.
    fn finish(self) -> Result<(), String> {
        #[cfg(feature = "s3")]
        if let Some(o) = self.out {
            let url = o.url().unwrap_or_default().to_string();
            let t0 = std::time::Instant::now();
            let bytes = o.finish().map_err(|e| e.to_string())?;
            tracing::info!(url = %url, bytes, secs = t0.elapsed().as_secs_f64(), "uploaded");
        }
        Ok(())
    }
}

#[cfg(feature = "parquet")]
fn is_parquet(f: &Path) -> bool {
    strata::parquet::is_parquet(f)
}

#[cfg(not(feature = "parquet"))]
fn is_parquet(_: &Path) -> bool {
    false
}

/// `strata-server import`: loads `files` into the database at `path`, rows
/// from JSON Lines, JSON, CSV or TSV files (`format`, or each file's
/// extension) into its table `table` (the first if `None`), the tables of
/// SQLite files into the tables of their names, and Parquet files and
/// directories into `table` (by default, the table an export kept in them
/// or one named for them), and says what each wrote. A file may be an
/// `s3://` URL. A database that does not exist is created with `schema`,
/// or from the first SQLite or Parquet file; with SQLite files, `schema`
/// holds schemas for the tables they create
/// ([`strata::sqlite::schemas_from_json`]), with Parquet files the schema
/// of the table they create.
#[allow(clippy::too_many_arguments)]
pub fn import_files(path: &Path, schema: Option<&Path>, table: Option<&str>, models: &Path, format: Option<Format>, directory: bool, files: &[PathBuf], how: FileImport) -> Result<(), String> {
    let opts = || DbOptions { layout: if directory { Layout::Directory } else { Layout::SingleFile }, build: build_options(), ..DbOptions::default() };
    if how.parquet && !cfg!(feature = "parquet") {
        return Err("this strata-server was built without Parquet (feature parquet)".into());
    }
    let is_sqlite = |f: &Path| how.sqlite || (format.is_none() && !how.parquet && strata::sqlite::is_sqlite_file(f));
    let is_parquet = |f: &Path| how.parquet || (format.is_none() && !how.sqlite && is_parquet(f));
    let inputs: Vec<(String, Fetched)> = files.iter().map(|f| Ok((f.display().to_string(), fetch(f)?))).collect::<Result<_, String>>()?;
    let any_sqlite = inputs.iter().any(|(_, f)| is_sqlite(&f.path));
    let any_parquet = inputs.iter().any(|(_, f)| is_parquet(&f.path));
    if let Some((f, _)) = inputs.iter().find(|(_, f)| table.is_some() && is_sqlite(&f.path)) {
        return Err(format!("{f}: --table is for rows; a SQLite file's tables go to the tables of their names (--tables picks them)"));
    }
    if any_sqlite && any_parquet && schema.is_some() {
        return Err("--schema with SQLite and Parquet files: import them one kind at a time".into());
    }
    let text = schema.map(|s| std::fs::read_to_string(s).map_err(|e| format!("read {}: {e}", s.display()))).transpose()?;
    let given = match (&text, schema, any_sqlite) {
        (Some(t), Some(s), true) => strata::sqlite::schemas_from_json(t).map_err(|e| format!("{}: {e}", s.display()))?,
        _ => Vec::new(),
    };
    let given_table = match (&text, schema, any_parquet) {
        (Some(t), Some(s), true) => Some(Schema::from_json(t).map_err(|e| format!("{}: {e}", s.display()))?),
        _ => None,
    };
    let sq = strata::sqlite::ImportOptions { tables: how.tables.clone(), schemas: given, blobs_base64: how.blobs_base64 };
    let pq = ParquetImport { table: table.map(String::from), schema: given_table, key: how.key.clone(), blobs_base64: how.blobs_base64 };
    let created = |db: Db| {
        for (field, why) in db.open_embedders(models) {
            tracing::warn!(field = %field, error = %why, "cannot embed this field: its vectors must be in the files");
        }
        db
    };
    let mut db = if path.exists() {
        if schema.is_some() && !any_sqlite && !any_parquet {
            return Err(format!("{} exists: --schema is only for a new database", path.display()));
        }
        Some(created(Db::open(path, opts()).map_err(|e| format!("open {}: {e}", path.display()))?))
    } else {
        None
    };
    if how.plan {
        let mut plans = Vec::new();
        for (shown, file) in &inputs {
            if is_sqlite(&file.path) {
                plans.push(strata::sqlite::plan(&file.path, db.as_ref(), &sq).map_err(|e| format!("{shown}: {e}"))?.to_json());
            } else if is_parquet(&file.path) {
                plans.push(pq.plan(&file.path, db.as_ref()).map_err(|e| format!("{shown}: {e}"))?);
            } else {
                return Err(format!("{shown}: --plan is for SQLite and Parquet files"));
            }
        }
        let out = if plans.len() == 1 { plans.remove(0) } else { serde_json::Value::Array(plans) };
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return Ok(());
    }
    for (shown, fetched) in &inputs {
        let file = &fetched.path;
        let t0 = std::time::Instant::now();
        if is_sqlite(file) {
            let at = |e: strata::Error| format!("{shown}: {e}");
            let src = strata::sqlite::Source::open(file).map_err(at)?;
            let plan = src.plan(db.as_ref(), &sq).map_err(at)?;
            let d = match db.take() {
                Some(d) => d,
                None => {
                    let first = plan.tables.first().ok_or_else(|| format!("{shown}: no table to import"))?;
                    created(Db::create_in(path, first.schema.clone(), opts(), models).map_err(|e| format!("create {}: {e}", path.display()))?)
                }
            };
            let r = src.load(&d, &plan);
            db = Some(d);
            let r = r.map_err(at)?;
            for (name, why) in &r.skipped {
                tracing::info!(file = %shown, source = %name, "left out: {why}");
            }
            for t in &r.tables {
                let how = if t.bulk { "loaded" } else { "upserted" };
                tracing::info!(file = %shown, source = %t.source, table = %t.table, created = t.created, rows = t.rows, how, "imported");
                for n in &t.notes {
                    tracing::warn!(table = %t.table, "{n}");
                }
            }
            tracing::info!(file = %shown, tables = r.tables.len(), secs = t0.elapsed().as_secs_f64(), "imported the SQLite file");
            continue;
        }
        if is_parquet(file) {
            db = Some(pq.import(shown, file, db.take(), |schema| Ok(created(Db::create_in(path, schema, opts(), models).map_err(|e| format!("create {}: {e}", path.display()))?)))?);
            continue;
        }
        let d = match db.take() {
            Some(d) => d,
            None => {
                let (s, text) = schema.zip(text.as_deref()).ok_or_else(|| format!("{} does not exist: pass --schema FILE to create it", path.display()))?;
                let schema = Schema::from_json(text).map_err(|e| format!("{}: {e}", s.display()))?;
                created(Db::create_in(path, schema, opts(), models).map_err(|e| format!("create {}: {e}", path.display()))?)
            }
        };
        let into = table.map_or_else(|| d.schema().table.clone(), String::from);
        let r = strata::ingest::import_file_into(&d, &into, file, format).map_err(|e| format!("{shown}: {e}"));
        db = Some(d);
        let r = r?;
        let how = if r.bulk { "loaded" } else { "upserted" };
        tracing::info!(file = %shown, rows = r.rows, how, secs = t0.elapsed().as_secs_f64(), "imported");
        if !r.unknown.is_empty() {
            tracing::warn!(file = %shown, fields = %r.unknown.join(", "), "skipped fields the schema lacks");
        }
    }
    let db = db.ok_or("no files to import")?;
    db.storage().merge(&MergePolicy::default()).map_err(|e| format!("merge: {e}"))?;
    tracing::info!(db = %path.display(), rows = db.snapshot().n_live_docs(), "imported every file");
    Ok(())
}

/// How `strata-server import` reads Parquet files
/// ([`strata::parquet::ImportOptions`]).
#[cfg_attr(not(feature = "parquet"), allow(dead_code))]
struct ParquetImport {
    table: Option<String>,
    schema: Option<Schema>,
    key: Option<String>,
    blobs_base64: bool,
}

#[cfg(feature = "parquet")]
impl ParquetImport {
    fn options(&self) -> strata::parquet::ImportOptions {
        strata::parquet::ImportOptions { table: self.table.clone(), schema: self.schema.clone(), key: self.key.clone(), blobs_base64: self.blobs_base64 }
    }

    fn plan(&self, file: &Path, db: Option<&Db>) -> strata::Result<serde_json::Value> {
        Ok(strata::parquet::plan(file, db, &self.options())?.to_json())
    }

    /// Imports `file` into `db`, or into a database `create` makes with the
    /// table's schema, and gives the database back.
    fn import(&self, shown: &str, file: &Path, db: Option<Db>, create: impl FnOnce(Schema) -> Result<Db, String>) -> Result<Db, String> {
        let t0 = std::time::Instant::now();
        let at = |e: strata::Error| format!("{shown}: {e}");
        let src = strata::parquet::Source::open(file).map_err(at)?;
        let plan = src.plan(db.as_ref(), &self.options()).map_err(at)?;
        let d = match db {
            Some(d) => d,
            None => create(plan.schema.clone())?,
        };
        let r = src.load(&d, &plan).map_err(at)?;
        for (column, why) in &r.skipped {
            tracing::info!(file = %shown, column = %column, "left out: {why}");
        }
        for n in &r.notes {
            tracing::warn!(table = %r.table, "{n}");
        }
        let how = if r.bulk { "loaded" } else { "upserted" };
        tracing::info!(file = %shown, table = %r.table, created = r.created, files = r.files, rows = r.rows, how, secs = t0.elapsed().as_secs_f64(), "imported the Parquet rows");
        Ok(d)
    }
}

#[cfg(not(feature = "parquet"))]
impl ParquetImport {
    fn plan(&self, _: &Path, _: Option<&Db>) -> strata::Result<serde_json::Value> {
        Err(strata::Error::Schema("this strata-server was built without Parquet (feature parquet)".into()))
    }

    fn import(&self, _: &str, _: &Path, _: Option<Db>, _: impl FnOnce(Schema) -> Result<Db, String>) -> Result<Db, String> {
        Err("this strata-server was built without Parquet (feature parquet)".into())
    }
}

/// Writes the database at `path` to a new SQLite file `out`, a path or an
/// `s3://` URL ([`strata::sqlite::export`]).
pub fn export_sqlite(path: &Path, out: &str, opts: &strata::sqlite::ExportOptions) -> Result<(), String> {
    let db = Db::open(path, DbOptions::default()).map_err(|e| format!("open {}: {e}", path.display()))?;
    let t0 = std::time::Instant::now();
    let up = upload_to(out)?;
    let r = strata::sqlite::export(&db, &up.path, opts).map_err(|e| format!("{out}: {e}"))?;
    for t in &r.tables {
        tracing::info!(table = %t.table, rows = t.rows, indexes = t.indexes.len(), fts = t.fts.as_deref().unwrap_or("none"), "exported");
        for n in &t.notes {
            tracing::info!(table = %t.table, "{n}");
        }
    }
    up.finish()?;
    tracing::info!(out = %out, tables = r.tables.len(), secs = t0.elapsed().as_secs_f64(), "exported to SQLite");
    Ok(())
}

/// Writes the database at `path` (its tables `opts.tables`, or all) to
/// Parquet at `out`, a path or an `s3://` URL: a `.parquet` file for one table,
/// else a directory of them ([`strata::parquet::export`]).
#[cfg(feature = "parquet")]
pub fn export_parquet(path: &Path, out: &str, opts: &ParquetExport) -> Result<(), String> {
    let db = Db::open(path, DbOptions::default()).map_err(|e| format!("open {}: {e}", path.display()))?;
    let t0 = std::time::Instant::now();
    let up = upload_to(out.trim_end_matches('/'))?;
    let r = strata::parquet::export(&db, &up.path, opts).map_err(|e| format!("{out}: {e}"))?;
    for t in &r.tables {
        tracing::info!(table = %t.table, rows = t.rows, bytes = t.bytes, file = %t.path.file_name().unwrap_or_default().to_string_lossy(), "exported");
        for n in &t.notes {
            tracing::info!(table = %t.table, "{n}");
        }
    }
    up.finish()?;
    tracing::info!(out = %out, tables = r.tables.len(), secs = t0.elapsed().as_secs_f64(), "exported to Parquet");
    Ok(())
}

/// What a Parquet export writes, in a build without Parquet.
#[cfg(not(feature = "parquet"))]
#[derive(Default)]
pub struct ParquetExport {
    pub tables: Vec<String>,
    pub meta: bool,
}

#[cfg(feature = "parquet")]
pub use strata::parquet::ExportOptions as ParquetExport;

#[cfg(not(feature = "parquet"))]
pub fn export_parquet(_: &Path, _: &str, _: &ParquetExport) -> Result<(), String> {
    Err("this strata-server was built without Parquet (feature parquet)".into())
}

/// Releases expired snapshot leases, and reports merges that fail (the
/// database's merger threads merge as writes add segments; a database
/// that cannot write, [`Service::failing`], merges nothing).
pub fn spawn_maintenance(svc: &Arc<Service>) {
    let Some(db) = svc.local_db().cloned() else { return };
    let svc = Arc::downgrade(svc);
    tokio::spawn(async move {
        let mut reported = None;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let Some(svc) = svc.upgrade() else { return };
            svc.sweep_leases();
            let why = svc.failing().map(|w| format!("{w}; merges stop until then")).or_else(|| db.storage().merge_status().error.map(|e| format!("merge failed: {e}")));
            if why.is_some() && why != reported {
                tracing::error!(error = why.as_deref().unwrap_or_default(), "the database cannot merge");
            }
            reported = why;
        }
    });
}

/// Deletes rows whose `expire_by` time has passed, every `every`
/// ([`Service::expire_due`]), until the service is dropped.
pub fn spawn_expirer(svc: &Arc<Service>, every: Duration) {
    if svc.local_db().is_none() {
        return;
    }
    let svc = Arc::downgrade(svc);
    tokio::spawn(async move {
        let mut reported = None;
        loop {
            tokio::time::sleep(every).await;
            let Some(svc) = svc.upgrade() else { return };
            let r = tokio::task::spawn_blocking(move || svc.expire_due()).await;
            match r {
                Ok(Ok(0)) => reported = None,
                Ok(Ok(n)) => {
                    reported = None;
                    tracing::debug!(rows = n, "deleted expired rows");
                }
                Ok(Err(e)) => {
                    let why = e.to_string();
                    if reported.as_ref() != Some(&why) {
                        tracing::warn!(error = %why, "could not delete expired rows; reads leave them out meanwhile");
                    }
                    reported = Some(why);
                }
                Err(e) => tracing::error!(error = %e, "the expirer stopped"),
            }
        }
    });
}

/// Samples the service's counters and gauges every interval of its
/// series ([`crate::telemetry::Series`]), for the UI's charts, until the
/// service is dropped.
pub fn spawn_sampler(svc: &Arc<Service>) {
    let every = svc.series.interval;
    let svc = Arc::downgrade(svc);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The minute the log store last had a row of load for.
        let mut minute = crate::telemetry::now_ms() / 60_000;
        loop {
            tick.tick().await;
            let Some(svc) = svc.upgrade() else { return };
            let s = svc.sample();
            if let Some(point) = svc.series.add(s) {
                // alerts agent: this server's rules, and a coordinator's cluster.
                crate::alerts::tick(&svc, &point);
            }
            let now = crate::telemetry::now_ms();
            if now / 60_000 != minute {
                minute = now / 60_000;
                if let Some((store, load)) = svc.log_store.as_ref().zip(svc.minute_load(now)) {
                    store.keep_load(load);
                }
            }
        }
    });
}

/// How often a coordinator looks at its cluster without being asked, so
/// that events (a copy down, a new primary) and fleet health's "since"
/// are found when they happen rather than when someone looks.
pub const WATCH_EVERY: Duration = Duration::from_secs(5);

/// Judges fleet health every [`WATCH_EVERY`], which on a coordinator
/// surveys the cluster when its snapshot is older than
/// [`crate::coord::SNAPSHOT_EVERY`] ([`crate::events`]); notes a job of
/// moves that ended; and keeps each table's size in the log store an
/// hour apart.
pub fn spawn_watch(svc: &Arc<Service>) {
    let svc = Arc::downgrade(svc);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(WATCH_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut hour = 0;
        let mut job: Option<String> = None;
        loop {
            tick.tick().await;
            let Some(svc) = svc.upgrade() else { return };
            if matches!(svc.backend, Backend::Cluster(_)) {
                svc.fleet_health().await;
            }
            // A job of moves that ended is an event.
            if let crate::service::Backend::Cluster(c) = &svc.backend {
                let m = c.moves();
                let state = m["state"].as_str().map(String::from);
                if job.as_deref() == Some("running") && state.as_deref() != Some("running") {
                    let s = state.as_deref().unwrap_or("done");
                    let title = format!("A job of moves ({}) ended: {s}, {} of {} moves, {} rows", m["job"]["kind"].as_str().unwrap_or("moves"), m["done"], m["of"], m["rows_done"]);
                    let severity = if s == "failed" { "warning" } else { "info" };
                    svc.events.add(crate::events::Event { kind: "moves_finished".into(), severity: severity.into(), title, detail: serde_json::json!({"state": s, "error": m["job"]["error"], "rows": m["rows_done"]}), where_: serde_json::Value::Null, who: m["started_by"].as_str().map(String::from) });
                }
                job = state;
            }
            // Each table's size, an hour apart.
            let h = crate::telemetry::now_ms() / 3_600_000;
            if h != hour && svc.log_store.is_some() {
                hour = h;
                crate::inspect::keep_sizes(&svc).await;
            }
        }
    });
}

/// Serves the HTTP API and UI, over TLS with `access.tls`.
pub async fn serve_http(svc: Arc<Service>, listener: TcpListener, access: Access) -> std::io::Result<()> {
    let router = http::router(svc, &access);
    let Some(tls) = &access.tls else {
        // Each request knows its peer's address (the audit log's `addr`).
        return axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(shutdown()).await;
    };
    let config = auth::server_config(tls).map_err(std::io::Error::other)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    let stop = shutdown();
    tokio::pin!(stop);
    loop {
        let (tcp, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(c) => c,
                // Out of descriptors, say: wait rather than spin.
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = &mut stop => return Ok(()),
        };
        let _ = tcp.set_nodelay(true);
        let service = hyper_util::service::TowerToHyperService::new(router.clone().layer(axum::Extension(axum::extract::ConnectInfo(peer))));
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            // A client that never finishes its handshake holds a task, not
            // the accept loop.
            let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await else { return };
            let io = hyper_util::rt::TokioIo::new(stream);
            let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new()).serve_connection_with_upgrades(io, service).await;
        });
    }
}

/// Serves gRPC: every call needs `access.cluster`, `access.token` or a key
/// of the scope its method needs ([`crate::keys`]), over TLS with
/// `access.tls`.
pub async fn serve_grpc(svc: Arc<Service>, listener: TcpListener, access: Access) -> Result<(), tonic::transport::Error> {
    use tokio_stream::StreamExt;
    // A gRPC answer is several small writes (headers, data, trailers); with
    // Nagle's algorithm on, the last waits for the client's delayed ACK and
    // every call to a shard took 40 ms longer.
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener).map(|s| {
        if let Ok(s) = &s {
            let _ = s.set_nodelay(true);
        }
        s
    });
    let service = grpc_service(svc, &access);
    let mut builder = tonic::transport::Server::builder();
    if let Some(tls) = &access.tls {
        builder = builder.tls_config(tonic::transport::ServerTlsConfig::new().identity(tonic::transport::Identity::from_pem(&tls.cert, &tls.key)))?;
    }
    builder.add_service(service).serve_with_incoming_shutdown(incoming, shutdown()).await
}

/// A shard server's gRPC service, its calls checked for their token or key
/// ([`Check`]): what [`serve_grpc`] serves, and what a coordinator in the
/// same process calls in-process ([`crate::coord::LocalShard`]).
pub type GrpcService = tonic::service::interceptor::InterceptedService<pb::strata_server::StrataServer<grpc::Grpc>, Check>;

pub fn grpc_service(svc: Arc<Service>, access: &Access) -> GrpcService {
    // A Write message carries rows, as an HTTP write's body does.
    let max_body = svc.max_body;
    let check = Check { svc: svc.clone(), token: access.token.clone(), cluster: access.cluster.clone() };
    let service = pb::strata_server::StrataServer::new(grpc::Grpc(svc)).max_decoding_message_size(max_body).max_encoding_message_size(256 << 20);
    tonic::service::interceptor::InterceptedService::new(service, check)
}

/// Who a gRPC call is from, for each method to check its scope: the
/// caller its token or key names, or a refusal.
#[derive(Clone)]
pub struct Check {
    svc: Arc<Service>,
    token: Option<Secret>,
    cluster: Option<Secret>,
}

impl tonic::service::Interceptor for Check {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        match self.svc.keys.resolve(self.token.as_ref(), self.cluster.as_ref(), req.metadata().get("authorization").and_then(|v| v.to_str().ok())) {
            Some(who) => {
                req.extensions_mut().insert(who);
                Ok(req)
            }
            None => {
                self.svc.metrics.grpc("any", "Unauthenticated", 0.0);
                // The method is not known yet: the path is the service's.
                self.svc.audit.add(crate::keys::Entry {
                    ts_ms: crate::telemetry::now_ms(),
                    who: "unknown".into(),
                    key: None,
                    scope: None,
                    method: "gRPC".into(),
                    route: "any".into(),
                    status: 401,
                    ms: 0.0,
                    request_id: None,
                    detail: None,
                    kind: crate::keys::REFUSAL.into(),
                    ..Default::default()
                });
                Err(tonic::Status::unauthenticated("missing or wrong token"))
            }
        }
    }
}

/// Whether the server was told to stop and serves out its drain time
/// ([`set_drain`]): `/readyz` answers 503, so that a load balancer sends
/// it no more clients, and each HTTP answer closes its connection.
static DRAINING: AtomicBool = AtomicBool::new(false);
static DRAIN_MS: AtomicU64 = AtomicU64::new(0);

/// How long the server serves on once told to stop (`--drain-ms`), before
/// it stops taking connections and ends with the requests under way.
pub fn set_drain(d: Duration) {
    DRAIN_MS.store(d.as_millis() as u64, Ordering::Relaxed);
}

/// Whether the server was told to stop and serves out its drain time.
pub fn draining() -> bool {
    DRAINING.load(Ordering::Relaxed)
}

/// Ctrl-C, or SIGTERM: how Docker and Kubernetes stop a container, then
/// the drain time ([`set_drain`]). As a container's first process the
/// server would otherwise ignore SIGTERM, and be killed only when the
/// grace period ran out (30 s in Kubernetes). The HTTP and gRPC servers
/// each wait for it, and so drain together.
async fn shutdown() {
    stop_signal().await;
    let ms = DRAIN_MS.load(Ordering::Relaxed);
    if !DRAINING.swap(true, Ordering::Relaxed) {
        match ms {
            0 => tracing::info!("stopping: no new connections; the requests under way end first"),
            _ => tracing::info!(drain_ms = ms, "stopping: /readyz answers 503 while the server serves on for its drain time, then it takes no new connections"),
        }
    }
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
