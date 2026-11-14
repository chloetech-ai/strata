//! Opening the database, and serving HTTP and gRPC.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chloedb::db::Db;
use chloedb::ingest::Format;
use chloedb::schema::Schema;
use chloedb::segment::build::BuildOptions;
use chloedb::storage::{DbOptions, Layout, MergePolicy, MergerOptions};
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
    /// Open a database that opening would otherwise refuse because it
    /// loses acknowledged writes, accepting the loss
    /// ([`DbOptions::recover`]).
    pub recover: bool,
    /// Check every segment's checksum on opening
    /// ([`DbOptions::verify_checksums`]).
    pub verify_checksums: bool,
    /// Number the database's writes ([`chloedb::storage::Database::start_seq_no`]),
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
    /// On a database server: how often it reads every segment against its
    /// checksum, in seconds (0: only when asked, `POST /v1/admin/scrub`;
    /// [`Service::start_scrubs`]).
    pub scrub_every_s: u64,
    /// The MiB a second a scheduled scrub reads at most (0: 64).
    pub scrub_mib_s: u64,
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
    /// How long a client's query waits for a slot before it is refused
    /// with 429 ([`crate::service::Service::admission_wait`]; 0: until its
    /// deadline).
    pub admission_wait_ms: u64,
    /// The most rows a query may reach, OFFSET included (0: no limit).
    pub max_result_window: usize,
    /// The rows the open scrolls may hold between them
    /// ([`crate::service::Service::max_scroll_rows`]; `None`:
    /// [`crate::service::MAX_SCROLL_ROWS`]).
    pub max_scroll_rows: Option<usize>,
    /// The most groups a query may make (0: no limit).
    pub max_groups: usize,
    /// The most rows a cross-encoder's rerank phase may score (0: no
    /// limit).
    pub max_rerank: usize,
    /// Memory the running queries may hold together, in MiB (`None`: a
    /// third of [`chloedb::storage::memory_limit`]; 0: no limit).
    pub query_memory_mb: Option<u64>,
    /// Memory for filters kept between queries, in MiB (`None`:
    /// [`chloedb::query::exec::FILTER_CACHE_BYTES`]; 0: none kept).
    pub filter_cache_mb: Option<usize>,
    /// Memory for answers kept between queries, in MiB (`None`:
    /// [`crate::answers::DEFAULT_MB`]; 0: none kept).
    pub answer_cache_mb: Option<usize>,
    /// The longest statement a request may send, in bytes, and the most
    /// elements it may hold (`None`: [`chloedb::query::parse::STATEMENT_BYTES`]
    /// and [`chloedb::query::parse::STATEMENT_ELEMENTS`]; 0: no bound).
    pub max_statement_bytes: Option<usize>,
    pub max_statement_elements: Option<usize>,
    /// Write requests held at once before more are refused (0: no limit).
    pub max_queued_writes: usize,
    /// The largest write or import body, in MiB (0: 256).
    pub max_body_mb: usize,
    /// How long tables OTLP's logs make keep their rows, in seconds
    /// (`None`: for ever).
    pub otlp_retain: Option<i64>,
    pub model: String,
    pub api_key: Option<String>,
    pub api_base: String,
    /// Where embedded fields' models are ([`chloedb::embed::open`]).
    pub models: PathBuf,
    /// Fill in the vectors of rows stored while their embedded field had
    /// no model open, once it has one ([`Service::keep_models`]).
    pub backfill: bool,
    /// Fetch the models Strata knows ([`chloedb::embed::KNOWN_MODELS`]) that
    /// an embedded field names and `models` lacks.
    pub fetch_models: bool,
    /// The synonym list for `synonyms => 1` searches (Solr format), read at
    /// start if it exists and written by `PUT /v1/synonyms`.
    pub synonyms: Option<PathBuf>,
    /// Where `POST /v1/admin/checkpoint` writes checkpoints.
    pub backup_dir: Option<PathBuf>,
    /// Where `POST /v1/admin/slices` writes slices ([`chloedb::slice`]).
    pub slices: Option<PathBuf>,
    /// Every this many seconds, rebuild the slices whose table changed
    /// since they were built (0: only when asked).
    pub slices_rebuild_s: u64,
    /// Keep segments cold on object storage ([`chloedb::storage::cold`]).
    pub cold: Option<ColdFlags>,
    /// What writes may fetch for MEDIA fields: `file:` roots and hosts
    /// ([`chloedb::media::Allowlist::parse`]).
    pub media_fetch: Vec<String>,
    /// The largest media file a write fetches or gives, in bytes (0:
    /// [`chloedb::media::DEFAULT_MAX_BYTES`]).
    pub media_max_bytes: u64,
    /// Where bytes that writes give a MEDIA field without a URI are kept.
    pub media_store: Option<PathBuf>,
    /// How long a media fetch may take, in seconds (0:
    /// [`crate::media::DEFAULT_TIMEOUT`]).
    pub media_timeout_s: u64,
    /// The background merger ([`chloedb::storage::Database::spawn_merger`]).
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
    /// The connectors' secrets (default: DB.connectors.json beside --db).
    pub connector_secrets: Option<PathBuf>,
    /// The prefix of the variables connectors' `env:` secrets may name
    /// (default [`crate::connectors::ENV_PREFIX`]).
    pub connector_env_prefix: Option<String>,
    /// Pulls of the connectors run here on their schedules.
    pub connectors: bool,
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
    /// `--app name=url`: apps the command center shows ([`crate::apps`]).
    pub apps: Vec<String>,
    /// What the server sends apps as its token (`CHLOEDB_APP_TOKEN`).
    pub app_token: Option<String>,
    /// `--tenants URL`: the object store of the tenants this server opens
    /// (experimental, [`crate::tenants`]), and `--tenant-cache DIR`.
    pub tenants: Option<String>,
    pub tenant_cache: Option<PathBuf>,
    /// `--tenant-idle-s` and `--tenant-open-max`: when tenants are closed.
    pub tenant_idle_s: Option<f64>,
    pub tenant_open_max: Option<usize>,
    /// The apps the server starts as child processes (`--start-apps`,
    /// [`crate::app_procs`]); [`crate::apps::Apps::start`] starts them once
    /// the server listens.
    pub started_apps: Vec<crate::app_procs::Plan>,
    /// A folder of each app's own token, a file by the app's name
    /// (`CHLOEDB_APP_TOKEN_DIR`; [`crate::apps`]).
    pub app_token_dir: Option<std::path::PathBuf>,
}

/// How writes resolve MEDIA values, from `--media-fetch`,
/// `--media-max-bytes`, `--media-store` and `--media-timeout-s`.
fn media_resolver(o: &Options) -> Result<chloedb::media::Resolver, String> {
    let allow = chloedb::media::Allowlist::parse(&o.media_fetch).map_err(|e| e.to_string())?;
    if let Some(dir) = &o.media_store {
        std::fs::create_dir_all(dir).map_err(|e| format!("--media-store {}: {e}", dir.display()))?;
    }
    let timeout = if o.media_timeout_s > 0 { Duration::from_secs(o.media_timeout_s) } else { crate::media::DEFAULT_TIMEOUT };
    let max_bytes = if o.media_max_bytes > 0 { o.media_max_bytes } else { chloedb::media::DEFAULT_MAX_BYTES };
    let http = crate::media::HttpFetcher::new(allow.clone(), timeout)?;
    tracing::info!(fetch = %allow.entries().join(", "), max_bytes, store = ?o.media_store, timeout_s = timeout.as_secs(), "media fields: what writes may fetch");
    Ok(chloedb::media::Resolver { allow, max_bytes, store: o.media_store.clone(), http: Some(std::sync::Arc::new(http)) })
}

/// The cold tier's flags ([`chloedb::storage::ColdOptions`]).
#[derive(Clone, Debug)]
pub struct ColdFlags {
    /// An `s3://` URL or a directory ([`chloedb::storage::open_store`]).
    pub store: String,
    pub cache_bytes: u64,
    pub keep_local_bytes: Option<u64>,
    pub min_segment_bytes: u64,
    pub cache_dir: Option<PathBuf>,
}

impl ColdFlags {
    fn options(&self) -> Result<chloedb::storage::ColdOptions, String> {
        let store = chloedb::storage::open_store(&self.store).map_err(|e| format!("--cold-store {}: {e}", self.store))?;
        Ok(chloedb::storage::ColdOptions {
            cache_dir: self.cache_dir.clone(),
            cache_bytes: self.cache_bytes,
            keep_local_bytes: self.keep_local_bytes,
            min_segment_bytes: self.min_segment_bytes,
            ..chloedb::storage::ColdOptions::new(store)
        })
    }
}

/// How often the cold tier looks for segments to make cold.
const COLD_EVERY: Duration = Duration::from_secs(30);

/// Segment builds (flushes, merges, imports): vector indexes on every core.
fn build_options() -> BuildOptions {
    BuildOptions::default().set("vector.threads", std::thread::available_parallelism().map_or(1, |n| n.get()))
}

/// How the server opens a database. Each write publishes its snapshot
/// while queries come in, building the in-memory chunks it filled (1,024
/// rows each; a write nobody reads leaves them to the flush, or to the
/// first query after it), and the first query after it builds the last
/// piece of the chunk still filling (128 rows,
/// `DbOptions::memtable_tail_docs`): those chunks keep their vectors flat,
/// since a graph over so few rows costs more to build at every write than
/// an exact scan of them costs a query. Segments build their vector
/// indexes on every core.
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
        let cold = match &o.cold {
            Some(c) if path.is_file() || (!path.exists() && !o.directory) => {
                return Err(format!("--cold-store {}: cold segments need a database in the directory layout (--directory)", c.store));
            }
            Some(c) => Some(c.options()?),
            None => None,
        };
        let db_options = |directory: bool| DbOptions { cold: cold.clone(), ..db_options(directory) };
        let db = if path.exists() {
            let db = Db::open(&path, DbOptions { wal_drop_damaged: o.wal_drop_damaged, recover: o.recover, verify_checksums: o.verify_checksums, ship_log_bytes, ..db_options(o.directory) })
                .map_err(|e| format!("open {}: {e}", path.display()))?;
            if let Some(what) = db.storage().damaged() {
                // Also in /v1/health ("degraded") and /v1/admin/check (an
                // error) until the database is opened clean again.
                tracing::error!(db = %path.display(), damage = %what, "opened the database past damage");
            }
            db
        } else if let Some(n) = o.demo {
            fill = Some(Fill::Demo(n));
            Db::create_in(&path, demo::schema(n), DbOptions { ship_log_bytes, ..db_options(o.directory) }, &o.models).map_err(|e| e.to_string())?
        } else if let Some(s) = &o.schema {
            let (s, text) = schema_arg(s)?;
            let schema = Schema::from_json(&text).map_err(|e| format!("{s}: {e}"))?;
            fill = o.copy_from.clone().map(Fill::Copy);
            Db::create_in(&path, schema, DbOptions { ship_log_bytes, ..db_options(o.directory) }, &o.models).map_err(|e| e.to_string())?
        } else {
            // No schema: a database with no tables yet, which CREATE TABLE
            // and the imports that create tables fill.
            tracing::info!(db = %path.display(), "creating the database with no tables: create one with CREATE TABLE or an import");
            Db::create_empty(&path, DbOptions { ship_log_bytes, ..db_options(o.directory) }).map_err(|e| e.to_string())?
        };
        // Copies a transfer left when this server was stopped during it.
        if replicating || o.seq_no {
            crate::replication::sweep_copies(&path);
        }
        for (field, why) in db.open_codebooks(&o.models) {
            tracing::warn!(field = %field, error = %why, "cannot give this field semantic IDs: writes must carry them");
        }
        // A server stores rows whose field's model is not open yet without
        // their vectors, and fills them in once it opens (the backfill).
        db.set_embed_later(true);
        if o.seq_no {
            db.storage().start_seq_no().map_err(|e| format!("--seq-no on {}: {e}", path.display()))?;
        }
        if replicating && db.storage().seq_no().is_none() {
            return Err(format!("{} does not number its writes, which replication ships by: pass --seq-no", path.display()));
        }
        let mut db = db;
        // Each query is given the server's own budget (--query-memory-mb)
        // where it runs, not the one a program's database counts against.
        db.exec.memory = None;
        if let Some(mb) = o.filter_cache_mb {
            db.exec.filter_cache = Some(Arc::new(chloedb::query::exec::FilterCache::new(mb << 20)));
        }
        let db = Arc::new(db);
        // Writes go on while a full memtable becomes segments, and while
        // segments are merged.
        db.spawn_flusher();
        db.spawn_merger(o.merger.clone());
        if let Some(c) = &o.cold {
            db.spawn_cold(COLD_EVERY);
            tracing::info!(store = %c.store, cache_mib = c.cache_bytes >> 20, keep_local_mib = ?c.keep_local_bytes.map(|b| b >> 20), "cold segments: kept on object storage, read through a local cache");
        }
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
    svc.apps = crate::apps::Apps::with_started(crate::apps::parse(&o.apps)?, o.app_token.clone(), o.started_apps.clone()).with_token_dir(o.app_token_dir.clone());
    if let Some(url) = given(&o.tenants) {
        let cache = o.tenant_cache.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("chloedb-tenants-{}", std::process::id())));
        let d = crate::tenants::Limits::default();
        let limits = crate::tenants::Limits { idle: o.tenant_idle_s.filter(|s| *s > 0.0).map_or(d.idle, Duration::from_secs_f64), open_max: o.tenant_open_max.filter(|n| *n > 0).unwrap_or(d.open_max) };
        let tenants = Arc::new(crate::tenants::Tenants::open(&url, cache, limits)?);
        tenants.spawn_closer();
        svc.tenants = Some(tenants);
        tracing::warn!(store = %url, "tenants are experimental: their layout in the store may change without a migration");
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
                // One coordinator at a time changes the map (finding E-2),
                // known by its pod's name.
                if let crate::service::Backend::Cluster(coord) = &svc.backend {
                    let me = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| format!("coordinator-{}", std::process::id()));
                    coord.lease_map_changes(l.map_lease(&me));
                }
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
    svc.admission_wait = (o.admission_wait_ms > 0).then(|| Duration::from_millis(o.admission_wait_ms));
    svc.answers = crate::answers::AnswerCache::new(o.answer_cache_mb.unwrap_or(crate::answers::DEFAULT_MB)).map(Arc::new);
    svc.max_scroll_rows = o.max_scroll_rows.unwrap_or(crate::service::MAX_SCROLL_ROWS);
    svc.max_groups = (o.max_groups > 0).then_some(o.max_groups);
    svc.max_rerank = (o.max_rerank > 0).then_some(o.max_rerank);
    let query_memory = match o.query_memory_mb {
        Some(mb) => (mb > 0).then(|| mb.saturating_mul(1 << 20)),
        None => chloedb::storage::memory_limit().map(|l| l / 3),
    };
    if let Some(b) = query_memory {
        tracing::info!(memory_mib = b >> 20, "queries share a memory budget");
    }
    svc.query_memory = query_memory.map(crate::service::query_budget);
    if let Some(n) = o.max_statement_bytes {
        svc.max_statement_bytes = n;
    }
    if let Some(n) = o.max_statement_elements {
        svc.max_statement_elements = n;
    }
    svc.max_queued_writes = if o.max_queued_writes > 0 { o.max_queued_writes } else { usize::MAX };
    if o.max_body_mb > 0 {
        svc.max_body = o.max_body_mb.saturating_mul(1 << 20);
    }
    svc.otlp_retain = o.otlp_retain;
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
    let connector_secrets = o.connector_secrets.clone().or_else(|| o.db.as_deref().filter(|_| o.shards.is_empty()).map(crate::connectors::Connectors::file_of));
    if let Some(file) = connector_secrets {
        svc.connectors.keep_secrets(file)?;
    }
    if let Some(p) = &o.connector_env_prefix {
        svc.connectors.set_env_prefix(p)?;
    }
    if o.connectors {
        svc.connectors.enable();
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
                    Some((s, of)) => shard_of(&chloedb::Value::Str(format!("p{i:07}")), of) == s,
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
    svc.scrubs.every = Duration::from_secs(o.scrub_every_s);
    svc.scrubs.rate = if o.scrub_mib_s == 0 { crate::service::scrub::DEFAULT_RATE } else { o.scrub_mib_s << 20 };
    if let Some(dir) = &o.slices {
        svc.set_slices_dir(dir).map_err(|e| e.message)?;
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
    spawn_rollups(&svc, Duration::from_secs(5));
    spawn_key_forgetter(&svc, Duration::from_secs(60));
    spawn_sampler(&svc);
    spawn_watch(&svc);
    // workdata: every table's counts a minute apart (rates on a first look).
    crate::inspect::keep_looking(&svc);
    tokio::spawn(svc.clone().keep_models());
    if o.slices.is_some() && o.slices_rebuild_s > 0 {
        let (svc, every) = (svc.clone(), Duration::from_secs(o.slices_rebuild_s));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                match svc.rebuild_stale_slices().await {
                    Ok(t) if !t.is_empty() => tracing::info!(tables = ?t, "rebuilt stale slices"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e.message, "rebuilding stale slices"),
                }
            }
        });
    }
    svc.start_failover();
    svc.start_scrubs();
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

/// Downloads model `name` ([`chloedb::embed::KNOWN_MODELS`],
/// [`chloedb::derive::KNOWN_MODELS`], a cross-encoder of
/// [`cross_encoder::KNOWN_MODELS`], or a Hugging Face repository
/// `owner/name` of an embedding model or a cross-encoder) into `dir`;
/// returns its directory.
pub async fn fetch_model(name: &str, dir: &std::path::Path) -> Result<PathBuf, String> {
    use chloedb::query::cross_encoder;
    let derives = chloedb::derive::KNOWN_MODELS.iter().any(|m| m.0.eq_ignore_ascii_case(name));
    let known = || chloedb::embed::KNOWN_MODELS.iter().chain(chloedb::derive::KNOWN_MODELS).chain(cross_encoder::KNOWN_MODELS);
    let (local, repo) = match known().find(|m| m.0.eq_ignore_ascii_case(name)) {
        Some(m) => (m.0.to_string(), m.1.to_string()),
        None if name.contains('/') => (name.rsplit('/').next().unwrap_or(name).to_string(), name.to_string()),
        None => {
            let names: Vec<&str> = known().map(|m| m.0).collect();
            return Err(format!("unknown model {name}: one of {}, or a Hugging Face repository owner/name", names.join(", ")));
        }
    };
    // A model of a table in fewer bits (`potion-retrieval-32M-int8`) is
    // its original fetched aside, then written into its own directory, so
    // that a fetch cut short never leaves the original under its name.
    let quantized = chloedb::embed::quantized_of(&local);
    let model_dir = dir.join(&local);
    let out = match quantized {
        Some(_) => dir.join(format!(".{local}.fetch")),
        None => model_dir.clone(),
    };
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let client = crate::http_client().build().map_err(|e| e.to_string())?;
    // Each file's path in the repository, and its name here.
    let files: Vec<(&str, &str)> = match (derives, cross_encoder::fetch_files(&local)) {
        (true, _) => chloedb::derive::model_files(&local).iter().map(|&f| (f, f)).collect(),
        (false, Some(f)) => f,
        (false, None) => chloedb::embed::model_files(quantized.map_or(local.as_str(), |q| q.0)).iter().map(|&f| (f, f)).collect(),
    };
    // A model ChloeDB knows comes from the commit it was pinned at, each
    // file checked against its SHA-256 ([`chloedb::model_pins`]); another
    // repository from its main branch, as it is.
    let pin = chloedb::model_pins::pin(&repo);
    let revision = pin.map_or("main", |p| p.revision);
    for (from, file) in files {
        // A file already here that is the one pinned is kept, so a second
        // fetch (the guides' "once") downloads nothing.
        if let Some(want) = pin.and_then(|p| p.sha256(from)) {
            if std::fs::read(out.join(file)).is_ok_and(|have| chloedb::model_pins::sha256_hex(&have) == want) {
                tracing::info!(file, "already here, as pinned");
                continue;
            }
        }
        let url = format!("https://huggingface.co/{repo}/resolve/{revision}/{from}");
        let r = client.get(&url).send().await.and_then(|r| r.error_for_status()).map_err(|e| format!("{url}: {e}"))?;
        let bytes = r.bytes().await.map_err(|e| format!("{url}: {e}"))?;
        if let Some(p) = pin {
            let want = p.sha256(from).ok_or_else(|| format!("{url}: no checksum is pinned for this file of {repo}"))?;
            let got = chloedb::model_pins::sha256_hex(&bytes);
            if got != want {
                return Err(format!("{url}: SHA-256 {got}, where {want} is pinned for {repo} at {revision}: the file is not the one ChloeDB was built for, and is not kept"));
            }
        }
        // Written aside and renamed, so that an open model is never
        // overwritten in place.
        let tmp = out.join(format!("{file}.part"));
        std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, out.join(file))).map_err(|e| format!("{}: {e}", out.join(file).display()))?;
        tracing::info!(file, bytes = bytes.len(), "downloaded");
    }
    if let Some((_, elem)) = quantized {
        let q = chloedb::embed::quantize_static(&out, &model_dir, elem).map_err(|e| e.to_string())?;
        tracing::info!(model = %local, bytes = q.bytes_after, was = q.bytes_before, "wrote the table in {elem}");
        let _ = std::fs::remove_dir_all(&out);
        chloedb::embed::open(&local, dir).map_err(|e| e.to_string())?;
        return Ok(model_dir);
    }
    if derives {
        chloedb::derive::open(&local, dir).map_err(|e| e.to_string())?;
    } else if !cross_encoder::is_cross_encoder(&out) {
        chloedb::embed::open(&local, dir).map_err(|e| e.to_string())?;
    } else if let Err(e) = cross_encoder::CrossEncoder::open(&local, &out) {
        let runs = if out.join(cross_encoder::ONNX).exists() { cfg!(feature = "onnx") } else { cfg!(feature = "bert") };
        match runs {
            true => return Err(e.to_string()),
            false => tracing::warn!(error = %e, "downloaded a cross-encoder this build cannot run"),
        }
    }
    Ok(out)
}

/// What `chloedb-server import` does with SQLite and Parquet files.
#[derive(Default)]
pub struct FileImport {
    /// Every file is a SQLite database (`--format sqlite`); otherwise a
    /// file is one when its first bytes say so.
    pub sqlite: bool,
    /// Every file is Parquet, or a directory of Parquet files (`--format
    /// parquet`); otherwise one is when its name or first bytes say so
    /// ([`chloedb::parquet::is_parquet`]).
    pub parquet: bool,
    /// SQLite: only these tables.
    pub tables: Vec<String>,
    /// Parquet: the key of a table the import creates
    /// ([`chloedb::parquet::ImportOptions::key`]).
    pub key: Option<String>,
    /// Binary columns as base64 text ([`chloedb::sqlite::ImportOptions::blobs_base64`]).
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
    _held: Option<chloedb::objects::Input>,
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
        let i = chloedb::objects::Input::open(&s).map_err(|e| e.to_string())?;
        tracing::info!(url = %s, secs = t0.elapsed().as_secs_f64(), "fetched");
        Ok(Fetched { path: i.path().to_path_buf(), _held: Some(i) })
    }
    #[cfg(not(feature = "s3"))]
    Err(format!("{s}: this chloedb-server was built without s3:// URLs (feature s3)"))
}

/// A path to write on this machine, and the URL it then goes to.
struct Upload {
    path: PathBuf,
    #[cfg(feature = "s3")]
    out: Option<chloedb::objects::Output>,
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
        let o = chloedb::objects::Output::new(out).map_err(|e| e.to_string())?;
        Ok(Upload { path: o.path().to_path_buf(), out: Some(o) })
    }
    #[cfg(not(feature = "s3"))]
    Err(format!("{out}: this chloedb-server was built without s3:// URLs (feature s3)"))
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
    chloedb::parquet::is_parquet(f)
}

#[cfg(not(feature = "parquet"))]
fn is_parquet(_: &Path) -> bool {
    false
}

/// `--schema`'s value: a JSON file, or the JSON itself (a value that starts
/// with `{` or `[` and names no file). The name errors call it by, and its
/// text.
pub fn schema_arg(s: &Path) -> Result<(String, String), String> {
    let given = s.to_string_lossy();
    let inline = given.trim_start();
    if (inline.starts_with('{') || inline.starts_with('[')) && !s.exists() {
        return Ok(("--schema".to_string(), inline.to_string()));
    }
    let text = std::fs::read_to_string(s).map_err(|e| format!("--schema {}: {e}", s.display()))?;
    Ok((s.display().to_string(), text))
}

/// `chloedb-server import`: loads `files` into the database at `path`, rows
/// from JSON Lines, JSON, CSV or TSV files (`format`, or each file's
/// extension) into its table `table` (the first if `None`), the tables of
/// SQLite files into the tables of their names, and Parquet files and
/// directories into `table` (by default, the table an export kept in them
/// or one named for them), and says what each wrote. A file may be an
/// `s3://` URL. A database that does not exist is created with `schema`,
/// or from the first SQLite or Parquet file; with SQLite files, `schema`
/// holds schemas for the tables they create
/// ([`chloedb::sqlite::schemas_from_json`]), with Parquet files the schema
/// of the table they create.
#[allow(clippy::too_many_arguments)]
pub fn import_files(path: &Path, schema: Option<&Path>, table: Option<&str>, models: &Path, format: Option<Format>, directory: bool, files: &[PathBuf], how: FileImport) -> Result<(), String> {
    let opts = || DbOptions { layout: if directory { Layout::Directory } else { Layout::SingleFile }, build: build_options(), ..DbOptions::default() };
    if how.parquet && !cfg!(feature = "parquet") {
        return Err("this chloedb-server was built without Parquet (feature parquet)".into());
    }
    let is_sqlite = |f: &Path| how.sqlite || (format.is_none() && !how.parquet && chloedb::sqlite::is_sqlite_file(f));
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
    let schema = schema.map(schema_arg).transpose()?;
    let given = match (&schema, any_sqlite) {
        (Some((s, t)), true) => chloedb::sqlite::schemas_from_json(t).map_err(|e| format!("{s}: {e}"))?,
        _ => Vec::new(),
    };
    let given_table = match (&schema, any_parquet) {
        (Some((s, t)), true) => Some(Schema::from_json(t).map_err(|e| format!("{s}: {e}"))?),
        _ => None,
    };
    let sq = chloedb::sqlite::ImportOptions { tables: how.tables.clone(), schemas: given, blobs_base64: how.blobs_base64 };
    let pq = ParquetImport { table: table.map(String::from), schema: given_table, key: how.key.clone(), blobs_base64: how.blobs_base64 };
    let created = |db: Db| {
        for (field, why) in db.open_embedders(models) {
            tracing::warn!(field = %field, error = %why, "cannot embed this field: its vectors must be in the files");
        }
        // As before: rows without the vectors are imported without them,
        // which a server fills in once the model opens.
        db.set_embed_later(true);
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
                plans.push(chloedb::sqlite::plan(&file.path, db.as_ref(), &sq).map_err(|e| format!("{shown}: {e}"))?.to_json());
            } else if is_parquet(&file.path) {
                plans.push(pq.plan(&file.path, db.as_ref()).map_err(|e| format!("{shown}: {e}"))?);
            } else {
                return Err(format!("{shown}: --plan is for SQLite and Parquet files"));
            }
        }
        let out = if plans.len() == 1 { plans.remove(0) } else { serde_json::Value::Array(plans) };
        crate::print_out(&serde_json::to_string_pretty(&out).unwrap_or_default());
        return Ok(());
    }
    for (shown, fetched) in &inputs {
        let file = &fetched.path;
        let t0 = std::time::Instant::now();
        if is_sqlite(file) {
            let at = |e: chloedb::Error| format!("{shown}: {e}");
            let src = chloedb::sqlite::Source::open(file).map_err(at)?;
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
                let (s, text) = schema.as_ref().ok_or_else(|| format!("{} does not exist: pass --schema (a JSON file, or the JSON) to create it", path.display()))?;
                let schema = Schema::from_json(text).map_err(|e| format!("{s}: {e}"))?;
                created(Db::create_in(path, schema, opts(), models).map_err(|e| format!("create {}: {e}", path.display()))?)
            }
        };
        let into = table.map_or_else(|| d.schema().table.clone(), String::from);
        let r = chloedb::ingest::import_file_into(&d, &into, file, format).map_err(|e| format!("{shown}: {e}"));
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

/// How `chloedb-server import` reads Parquet files
/// ([`chloedb::parquet::ImportOptions`]).
#[cfg_attr(not(feature = "parquet"), allow(dead_code))]
struct ParquetImport {
    table: Option<String>,
    schema: Option<Schema>,
    key: Option<String>,
    blobs_base64: bool,
}

#[cfg(feature = "parquet")]
impl ParquetImport {
    fn options(&self) -> chloedb::parquet::ImportOptions {
        chloedb::parquet::ImportOptions { table: self.table.clone(), schema: self.schema.clone(), key: self.key.clone(), blobs_base64: self.blobs_base64 }
    }

    fn plan(&self, file: &Path, db: Option<&Db>) -> chloedb::Result<serde_json::Value> {
        Ok(chloedb::parquet::plan(file, db, &self.options())?.to_json())
    }

    /// Imports `file` into `db`, or into a database `create` makes with the
    /// table's schema, and gives the database back.
    fn import(&self, shown: &str, file: &Path, db: Option<Db>, create: impl FnOnce(Schema) -> Result<Db, String>) -> Result<Db, String> {
        let t0 = std::time::Instant::now();
        let at = |e: chloedb::Error| format!("{shown}: {e}");
        let src = chloedb::parquet::Source::open(file).map_err(at)?;
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
    fn plan(&self, _: &Path, _: Option<&Db>) -> chloedb::Result<serde_json::Value> {
        Err(chloedb::Error::Schema("this chloedb-server was built without Parquet (feature parquet)".into()))
    }

    fn import(&self, _: &str, _: &Path, _: Option<Db>, _: impl FnOnce(Schema) -> Result<Db, String>) -> Result<Db, String> {
        Err("this chloedb-server was built without Parquet (feature parquet)".into())
    }
}

/// Writes the database at `path` to a new SQLite file `out`, a path or an
/// `s3://` URL ([`chloedb::sqlite::export`]).
pub fn export_sqlite(path: &Path, out: &str, opts: &chloedb::sqlite::ExportOptions) -> Result<(), String> {
    let db = Db::open(path, DbOptions::default()).map_err(|e| format!("open {}: {e}", path.display()))?;
    let t0 = std::time::Instant::now();
    let up = upload_to(out)?;
    let r = chloedb::sqlite::export(&db, &up.path, opts).map_err(|e| format!("{out}: {e}"))?;
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
/// else a directory of them ([`chloedb::parquet::export`]).
#[cfg(feature = "parquet")]
pub fn export_parquet(path: &Path, out: &str, opts: &ParquetExport) -> Result<(), String> {
    let db = Db::open(path, DbOptions::default()).map_err(|e| format!("open {}: {e}", path.display()))?;
    let t0 = std::time::Instant::now();
    let up = upload_to(out.trim_end_matches('/'))?;
    let r = chloedb::parquet::export(&db, &up.path, opts).map_err(|e| format!("{out}: {e}"))?;
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
pub use chloedb::parquet::ExportOptions as ParquetExport;

#[cfg(not(feature = "parquet"))]
pub fn export_parquet(_: &Path, _: &str, _: &ParquetExport) -> Result<(), String> {
    Err("this chloedb-server was built without Parquet (feature parquet)".into())
}

/// Releases expired snapshot leases and scrolls, and reports merges that fail (the
/// database's merger threads merge as writes add segments; a database
/// that cannot write, [`Service::failing`], merges nothing).
pub fn spawn_maintenance(svc: &Arc<Service>) {
    let Some(db) = svc.local_db().cloned() else {
        // A coordinator ends the scrolls not read over their keep-alive,
        // and their shards' with them.
        let svc = Arc::downgrade(svc);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(svc) = svc.upgrade() else { return };
                svc.sweep_scrolls();
            }
        });
        return;
    };
    let svc = Arc::downgrade(svc);
    tokio::spawn(async move {
        let mut reported = None;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let Some(svc) = svc.upgrade() else { return };
            svc.sweep_scrolls();
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

/// Refreshes the rollups every `every`, and every bucket of each every
/// sixtieth time ([`Service::refresh_rollups_due`]), until the service is
/// dropped.
pub fn spawn_rollups(svc: &Arc<Service>, every: Duration) {
    if svc.local_db().is_none() {
        return;
    }
    let svc = Arc::downgrade(svc);
    tokio::spawn(async move {
        // The error each rollup last reported, which is said again only
        // once it changes.
        let mut reported: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut listing: Option<String> = None;
        let mut tick: u64 = 0;
        loop {
            tokio::time::sleep(every).await;
            let Some(svc) = svc.upgrade() else { return };
            let full = tick % 60 == 59;
            tick += 1;
            match tokio::task::spawn_blocking(move || svc.refresh_rollups_due(full)).await {
                Ok(Ok(done)) => {
                    listing = None;
                    let names: std::collections::HashSet<String> = done.iter().map(|(n, _)| n.clone()).collect();
                    reported.retain(|n, _| names.contains(n));
                    for (name, res) in done {
                        match res {
                            Ok(s) => {
                                if reported.remove(&name).is_some() {
                                    tracing::info!(rollup = %name, "a rollup's refresh works again");
                                }
                                if s.folded > 0 {
                                    tracing::debug!(rollup = %name, buckets = s.folded, "refreshed a rollup");
                                }
                            }
                            Err(e) => {
                                let why = e.to_string();
                                if reported.get(&name) != Some(&why) {
                                    tracing::warn!(rollup = %name, error = %why, "could not refresh a rollup; the others go on, and it is tried again in a few seconds");
                                }
                                reported.insert(name, why);
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    let why = e.to_string();
                    if listing.as_ref() != Some(&why) {
                        tracing::warn!(error = %why, "could not read the rollups; the next try is in a few seconds");
                    }
                    listing = Some(why);
                }
                Err(e) => tracing::error!(error = %e, "the rollups' refresher stopped"),
            }
        }
    });
}

/// Deletes the database's records of idempotency keys older than
/// [`crate::idem::TTL`], every `every` ([`Service::forget_keys_due`]),
/// until the service is dropped.
pub fn spawn_key_forgetter(svc: &Arc<Service>, every: Duration) {
    if svc.local_db().is_none() {
        return;
    }
    let svc = Arc::downgrade(svc);
    tokio::spawn(async move {
        let mut reported = None;
        loop {
            tokio::time::sleep(every).await;
            let Some(svc) = svc.upgrade() else { return };
            match tokio::task::spawn_blocking(move || svc.forget_keys_due()).await {
                Ok(Ok(n)) => {
                    reported = None;
                    if n > 0 {
                        tracing::debug!(keys = n, "forgot expired idempotency keys");
                    }
                }
                Ok(Err(e)) => {
                    if reported.as_ref() != Some(&e.message) {
                        tracing::warn!(error = %e.message, "could not delete expired idempotency keys; retries ignore them meanwhile");
                    }
                    reported = Some(e.message);
                }
                Err(e) => tracing::error!(error = %e, "the idempotency key forgetter stopped"),
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
            let mut s = svc.sample();
            crate::alerts::refresh_sql(&svc);
            s.named = svc.alerts.sql_values();
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
    serve_http_until(svc, listener, access, shutdown()).await
}

/// [`serve_http`] until `stop` ends (rather than until SIGTERM or Ctrl-C
/// and the drain time).
pub async fn serve_http_until(svc: Arc<Service>, listener: TcpListener, access: Access, stop: impl std::future::Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
    let router = http::router(svc, &access);
    serve_router_until(router, listener, &access, stop).await
}

/// Serves the OpenSearch-compatible API ([`crate::opensearch`]) on its own
/// listener (`--opensearch-listen`), with the HTTP API's credentials and
/// TLS. `version` is the version its root reports.
pub async fn serve_opensearch(svc: Arc<Service>, listener: TcpListener, access: Access, version: &str) -> std::io::Result<()> {
    let router = crate::opensearch::router(svc, &access, version);
    serve_router_until(router, listener, &access, shutdown()).await
}

/// Serves `router` on `listener`, over TLS with `access.tls`, until `stop`
/// ends; then waits for the requests under way, up to
/// [`auth::Limits::stop_grace`], and returns.
///
/// Each connection must send a request's headers within
/// [`auth::Limits::header_read`] (an idle keep-alive one is closed after
/// it, and a TLS handshake has as long, 10 s at most), and the listener
/// holds [`auth::Limits::max_connections`] at once, of which one client
/// address holds [`auth::Limits::per_client`] at most: a client that opens
/// thousands and sends a byte a minute holds neither the process's file
/// descriptors nor its memory, nor every connection the others need. Past
/// the cap a few connections are still taken, for the probes alone
/// ([`crate::listener::HEALTH_ROUTES`]; any other request on them is
/// answered 503). A connection with no request under way for
/// [`auth::Limits::idle`] is closed, over HTTP/2 too; a request body that
/// pauses or trickles ([`auth::Limits::body`]) fails, and so does a write
/// the client does not read for [`auth::Limits::write`]
/// ([`crate::listener`]). Every answer says to browsers not to sniff its
/// type nor be framed by another site ([`security_headers`]). Long answers
/// that would never end on their own (a follower's feed) watch
/// [`Stopping`] and end when the listener stops taking connections.
pub async fn serve_router_until(router: axum::Router, listener: TcpListener, access: &Access, stop: impl std::future::Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
    serve_conns(router, listener, access, stop, Kind::Http).await
}

/// What a listener serves: HTTP (the API, the UI, OpenSearch's API), or
/// gRPC (HTTP/2 alone, request bodies not timed, no browser headers, no
/// connections for the probes past the cap).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Http,
    Grpc,
}

/// The calls under way at once on one gRPC connection (a coordinator
/// sends all its calls to a shard on one): past it, a client waits for
/// one to end.
pub const GRPC_STREAMS: u32 = 1024;

/// How long a connection closed for being idle has to say goodbye (an
/// HTTP/2 GOAWAY, its ping answered) before it is dropped.
const IDLE_GOODBYE: Duration = Duration::from_secs(1);

async fn serve_conns(router: axum::Router, listener: TcpListener, access: &Access, stop: impl std::future::Future<Output = ()> + Send + 'static, kind: Kind) -> std::io::Result<()> {
    use crate::listener::{self, Slot};
    use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
    let (stopping, stopped) = tokio::sync::watch::channel(false);
    let router = router.layer(axum::middleware::from_fn(listener::track));
    let router = match kind {
        // x-chloedb-* headers are read as their x-strata-* names (crate::names).
        Kind::Http => crate::names::http_layer(router).layer(axum::middleware::map_response(security_headers)),
        Kind::Grpc => router.layer(axum::middleware::from_fn(listener::grpc_deadline)),
    };
    let router = router.layer(axum::Extension(Stopping(stopped)));
    let tls = match &access.tls {
        Some(t) => Some(tokio_rustls::TlsAcceptor::from(auth::server_config(t).map_err(std::io::Error::other)?)),
        None => None,
    };
    let limits = access.limits;
    let (permits, cap) = match limits.max_connections {
        0 => connections(),
        n => (Arc::new(tokio::sync::Semaphore::new(n)), n),
    };
    let clients = listener::Clients::new(match limits.per_client {
        0 => cap / 2,
        n => n,
    });
    // Past the cap: a few connections for the probes, two at once per
    // address.
    let health = Arc::new(tokio::sync::Semaphore::new(if kind == Kind::Http { listener::HEALTH_SLOTS } else { 0 }));
    let probers = listener::Clients::new(2);
    let builder = |header_read: Duration| {
        let mut b = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
        b.http1().timer(TokioTimer::new()).header_read_timeout(header_read);
        // HTTP/2 has no header timeout: pings find a peer that went
        // away, and the idle timeout one that answers them.
        b.http2().timer(TokioTimer::new()).keep_alive_interval(limits.header_read).keep_alive_timeout(limits.header_read.min(Duration::from_secs(20)));
        match kind {
            Kind::Http => b,
            Kind::Grpc => {
                b.http2().max_concurrent_streams(GRPC_STREAMS);
                b.http2_only()
            }
        }
    };
    let (full, probe) = (Arc::new(builder(limits.header_read)), Arc::new(builder(limits.header_read.min(Duration::from_secs(5)))));
    // Each connection's task holds a clone of `open`: once every clone is
    // dropped, `closed` ends. A connection is told to finish when
    // `stopping` says true, and is cut when `alive` is dropped: once the
    // grace time is over, or when the listener's future is dropped (its
    // task aborted, as tests stop a shard), as tonic's server did.
    let (open, mut closed) = tokio::sync::mpsc::channel::<()>(1);
    let (alive, _) = tokio::sync::watch::channel(());
    let handshake = limits.header_read.min(Duration::from_secs(10));
    let body = if kind == Kind::Http { limits.body } else { Duration::ZERO };
    tokio::pin!(stop);
    loop {
        // Past the cap, wait for a connection to close before taking one,
        // but for the few kept for the probes.
        let (permit, slot) = tokio::select! {
            biased;
            _ = &mut stop => break,
            p = permits.clone().acquire_owned() => (p.map_err(std::io::Error::other)?, Slot::Full),
            p = health.clone().acquire_owned() => (p.map_err(std::io::Error::other)?, Slot::Health),
        };
        let (tcp, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(c) => c,
                // Out of descriptors, say: wait rather than spin.
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = &mut stop => break,
        };
        // A place under the cap freed while this one waited for a
        // connection: it takes that instead.
        let (permit, slot) = match slot {
            Slot::Health => match permits.clone().try_acquire_owned() {
                Ok(p) => (p, Slot::Full),
                Err(_) => (permit, slot),
            },
            Slot::Full => (permit, slot),
        };
        // A client past its share is closed at once, before its
        // handshake: it holds nothing the others need.
        let held = match slot {
            Slot::Full => clients.take(peer.ip(), None),
            Slot::Health => probers.take(peer.ip(), None),
        };
        let Some(held) = held else {
            // Its client sees only a reset: say so here, a line a second
            // at most, and count it.
            listener::CLOSED_PAST_SHARE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            static LIMIT: crate::obs::RateLimit = crate::obs::RateLimit::new(1);
            if let Some(more) = LIMIT.admit() {
                tracing::info!(client = %peer.ip(), held = clients.of(peer.ip()), closed_since = more, "a client past its share of connections (--max-connections-per-client): its new connection closed before any request");
            }
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let activity = listener::Activity::new();
        // Each request knows its peer's address (the audit log's `addr`)
        // and its connection's, set on it as it comes in: layering the
        // router per connection would copy every route, about 300 KB a
        // connection.
        let conn = listener::Conn { activity: activity.clone(), slot, body };
        let service = hyper_util::service::TowerToHyperService::new(tower::ServiceExt::map_request(router.clone(), with_conn(peer, conn)));
        let builder = if slot == Slot::Full { full.clone() } else { probe.clone() };
        let (tls, open, mut told, mut gone) = (tls.clone(), open.clone(), stopping.subscribe(), alive.subscribe());
        let tcp = listener::Stall::new(tcp, limits.write);
        tokio::spawn(async move {
            let (_permit, _held, _open) = (permit, held, open);
            // Serves one connection until it ends, or finishes the
            // requests under way once the listener stops or the
            // connection has been idle too long.
            macro_rules! serve {
                ($io:expr) => {{
                    let conn = builder.serve_connection_with_upgrades(TokioIo::new($io), service);
                    tokio::pin!(conn);
                    let mut ending = false;
                    // An idle connection told to finish has this long to
                    // (HTTP/2's goodbye waits for a ping's answer).
                    let mut cut: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
                    loop {
                        tokio::select! {
                            _ = conn.as_mut() => break,
                            Ok(()) = async { told.wait_for(|s| *s).await.map(|_| ()) }, if !ending => {
                                conn.as_mut().graceful_shutdown();
                                ending = true;
                            }
                            _ = activity.idle_for(limits.idle), if !ending => {
                                conn.as_mut().graceful_shutdown();
                                ending = true;
                                cut = Some(Box::pin(tokio::time::sleep(IDLE_GOODBYE)));
                            }
                            _ = async { cut.as_mut().expect("set").await }, if cut.is_some() => break,
                            _ = gone.changed() => break,
                        }
                    }
                }};
            }
            match tls {
                None => serve!(tcp),
                Some(acceptor) => {
                    // A client that never finishes its handshake holds a
                    // task for a while, not the accept loop.
                    let Ok(Ok(stream)) = tokio::time::timeout(handshake, acceptor.accept(tcp)).await else { return };
                    serve!(stream)
                }
            }
        });
    }
    drop(listener);
    let _ = stopping.send(true);
    drop(open);
    if tokio::time::timeout(limits.stop_grace, closed.recv()).await.is_err() {
        tracing::warn!(grace_ms = limits.stop_grace.as_millis() as u64, "stopping with requests still under way: they had their grace time (--stop-grace-ms)");
    }
    drop(alive);
    Ok(())
}

/// Sets a request's peer address (axum's `ConnectInfo`) and its
/// connection ([`crate::listener::Conn`]) as it comes in.
fn with_conn<B>(peer: std::net::SocketAddr, conn: crate::listener::Conn) -> impl Fn(axum::http::Request<B>) -> axum::http::Request<B> + Clone {
    move |mut r| {
        r.extensions_mut().insert(axum::extract::ConnectInfo(peer));
        r.extensions_mut().insert(conn.clone());
        r
    }
}

/// That the listener a request came in on stopped taking connections, for
/// an answer that goes on until told to end (a feed): [`Self::wait`].
#[derive(Clone)]
pub struct Stopping(tokio::sync::watch::Receiver<bool>);

impl Stopping {
    /// Ends once the listener stops taking connections (never, when it
    /// was dropped without stopping).
    pub async fn wait(mut self) {
        if self.0.wait_for(|s| *s).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// [`Self::wait`] for `s`, or never without one.
    pub async fn of(s: Option<Stopping>) {
        match s {
            Some(s) => s.wait().await,
            None => std::future::pending().await,
        }
    }
}

/// The connections the listeners hold at once, all of them together, when
/// `--max-connections` is 0: three quarters of the process's open-file
/// limit (the rest for its databases, its shards and the listeners
/// themselves); and how many that is.
fn connections() -> (Arc<tokio::sync::Semaphore>, usize) {
    static ALL: std::sync::OnceLock<(Arc<tokio::sync::Semaphore>, usize)> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        let mut n = 65_536;
        #[cfg(unix)]
        {
            raise_open_files();
            let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            // SAFETY: getrlimit writes the struct it is given, and nothing else.
            if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } == 0 && r.rlim_cur != libc::RLIM_INFINITY {
                n = (r.rlim_cur as usize / 4 * 3).clamp(64, 1 << 20);
            }
        }
        (Arc::new(tokio::sync::Semaphore::new(n)), n)
    })
    .clone()
}

/// Raises the process's soft open-file limit to its hard limit (at most
/// 2^20), once: the soft limit is often 1,024 or 20,000 while the hard one
/// allows a million, and each connection is a descriptor. A limit that
/// cannot be raised (macOS caps it below an infinite hard limit) is left
/// as it was, or set to the most it takes.
#[cfg(unix)]
fn raise_open_files() {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit writes the struct it is given, and nothing else.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } != 0 {
        return;
    }
    let want = if r.rlim_max == libc::RLIM_INFINITY { 1 << 20 } else { r.rlim_max.min(1 << 20) };
    for cur in [want, 1 << 16, 10_240] {
        if cur <= r.rlim_cur {
            return;
        }
        let n = libc::rlimit { rlim_cur: cur, rlim_max: r.rlim_max };
        // SAFETY: setrlimit reads the struct it is given, and nothing else.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &n) } == 0 {
            tracing::debug!(from = r.rlim_cur as u64, to = cur as u64, "raised the open-file limit");
            return;
        }
    }
}

/// Headers every HTTP answer carries unless its handler set its own: no
/// guessing a type other than the one it says (`nosniff`), no framing by
/// a page of another site (clicks on a framed command center would run
/// its queries), and no address of a page sent to other sites.
async fn security_headers(mut res: axum::response::Response) -> axum::response::Response {
    use axum::http::header::{self, HeaderValue};
    let h = res.headers_mut();
    h.entry(header::X_CONTENT_TYPE_OPTIONS).or_insert(HeaderValue::from_static("nosniff"));
    h.entry(header::X_FRAME_OPTIONS).or_insert(HeaderValue::from_static("SAMEORIGIN"));
    h.entry(header::REFERRER_POLICY).or_insert(HeaderValue::from_static("same-origin"));
    h.entry(header::CONTENT_SECURITY_POLICY).or_insert(HeaderValue::from_static("frame-ancestors 'self'; base-uri 'self'; object-src 'none'"));
    res
}

/// Serves gRPC: every call needs `access.cluster`, `access.token` or a key
/// of the scope its method needs ([`crate::keys`]), over TLS with
/// `access.tls`, until SIGTERM or Ctrl-C and the drain time.
pub async fn serve_grpc(svc: Arc<Service>, listener: TcpListener, access: Access) -> std::io::Result<()> {
    serve_grpc_until(svc, listener, access, shutdown()).await
}

/// [`serve_grpc`] until `stop` ends, on the HTTP listeners' accept loop
/// ([`serve_router_until`]): the same connection cap and share per client,
/// handshake, idle and write timeouts, and grace for the calls under way
/// when it stops; a follower's `Feed` ends then.
pub async fn serve_grpc_until(svc: Arc<Service>, listener: TcpListener, access: Access, stop: impl std::future::Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
    // otlp: OpenTelemetry's LogsService, behind the same check.
    let otlp = crate::otlp::grpc_service(svc.clone(), svc.max_body);
    let otlp = tonic::service::interceptor::InterceptedService::new(otlp, Check { svc: svc.clone(), token: access.token.clone(), cluster: access.cluster.clone() });
    let service = grpc_service(svc, &access);
    let router = tonic::service::Routes::new(service).add_service(otlp).into_axum_router();
    serve_conns(router, listener, &access, stop, Kind::Grpc).await
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
        // x-chloedb-* metadata is read as its x-strata-* names (crate::names).
        crate::names::alias_metadata(req.metadata_mut());
        match self.svc.keys.resolve(self.token.as_ref(), self.cluster.as_ref(), req.metadata().get("authorization").and_then(|v| v.to_str().ok())) {
            // A key limited to tables is an app's, over HTTP: its routes
            // check the tables they touch ([`crate::scoped`]), gRPC's
            // methods do not.
            Some(who) if who.limited() => {
                self.svc.metrics.grpc("any", "PermissionDenied", 0.0);
                // Audited as HTTP's refusals are: a stolen app key tried
                // on the gRPC port leaves a trace.
                self.svc.audit.add(crate::keys::Entry {
                    ts_ms: crate::telemetry::now_ms(),
                    who: who.name.clone(),
                    key: who.key.clone(),
                    scope: Some(who.scope.name()),
                    method: "gRPC".into(),
                    route: "any".into(),
                    status: 403,
                    ms: 0.0,
                    request_id: None,
                    detail: Some("a key limited to tables calls the HTTP API, not gRPC".into()),
                    kind: crate::keys::REFUSAL.into(),
                    ..Default::default()
                });
                Err(tonic::Status::permission_denied(format!("the key {:?} is limited to tables: it calls the HTTP API, not gRPC", who.name)))
            }
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
